//! What a lease may make the worker do (PROTOCOL 7, rev 6; security review H2, H3, M2).
//!
//! A lease that names a signed definition: the definition is downloaded, verified against the
//! trusted **data** keys (never the release keys), checked against the lease (kind and id,
//! `expires`, a version no older than the highest seen, the worker build and runtime, every
//! checkpoint's sha256 and size), and the job line is built from its job template plus only the
//! per-lease fields (`gid_base`, `games`, `seed` / `first`, `count`). Nothing else the coordinator
//! sends reaches the worker.
//!
//! A legacy lease (no definition; coordinators before rev 6) gets a strict allowlist instead: the
//! kind is forced, only the kind's template keys and per-lease fields are kept, unknown keys
//! (`prefix`, `models`, ...) are dropped, and a value that looks like a path refuses the lease.
//! The agent always writes `models` / `model` / `reference` / `runtime_dir` / `out_dir` itself.
//!
//! A lease that fails is refused (`done` with `refused: "definition"`), reported once a day as
//! crash kind `definition`, and the agent waits before asking again.

use crate::engine::{Control, Engine};
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use zerofarm_common::definition::{self as def, CheckpointDef, Definition};
use zerofarm_common::http::{CallError, Client, Want};
use zerofarm_common::keys::TrustedKeys;
use zerofarm_common::layout::Layout;
use zerofarm_common::manifest::{verify_dir, verify_manifest_bytes, Expect, Kind, Manifest, MAX_MANIFEST_BYTES, MAX_SIG_BYTES};
use zerofarm_common::protocol::{canonical_json, ArtifactRef, CheckpointRef, DefinitionRef, Lease};

/// What the worker of a lease will be told, before the agent's local fields.
#[derive(Debug, Clone)]
pub struct Plan {
    /// The job template (signed, or the allowlisted legacy job) plus the per-lease fields.
    pub job: Map<String, Value>,
    /// The verified definition; None for a legacy lease.
    pub def: Option<Definition>,
    /// rev 11 (PROTOCOL 7, 9): the lease's runtime is not the definition's but the one its worker build requires
    /// (a run alternate); the agent's guard (9.5) applies.
    pub alternate: bool,
}

impl Plan {
    /// The declared sizes of a checkpoint the plan's definition names (None: legacy): its gen's, or (rev 20) the
    /// variant's model with the gen's reference set.
    pub fn declared(&self, c: &CheckpointRef) -> Option<CheckpointDef> {
        self.def.as_ref().and_then(|d| d.checkpoint(c.gen)).and_then(|e| e.for_variant(c.variant.as_deref()))
    }

    /// A checkpoint a beat offers mid-lease (self-play): with a definition, only a gen it names,
    /// with its hashes (rev 20: a variant it signs for that gen).
    pub fn check_new_checkpoint(&self, c: &CheckpointRef) -> Result<Option<CheckpointDef>, String> {
        match &self.def {
            Some(d) => match_checkpoint(d, c).map(Some),
            None if c.variant.is_some() => Err(format!("checkpoint {}: a model variant needs a definition", c.gen)),
            None => Ok(None),
        }
    }
}

#[derive(Debug)]
pub enum PlanError {
    /// The lease (or its definition) failed a check: refuse it.
    Refused(String),
    /// The definition couldn't be fetched right now (network): give the lease back and wait.
    Unavailable(String),
    Stopped,
}

fn refused<T>(m: impl Into<String>) -> Result<T, PlanError> {
    Err(PlanError::Refused(m.into()))
}

/// Highest definition version seen per `kind/id` (`state\definitions.json`): never goes back.
pub struct SeenVersions;

impl SeenVersions {
    fn path(layout: &Layout) -> PathBuf {
        layout.state().join("definitions.json")
    }

    pub fn load(layout: &Layout) -> BTreeMap<String, u64> {
        std::fs::read(Self::path(layout)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    /// Records `version` for `scope` if it is the highest; errs if an older one than seen.
    pub fn check_and_record(layout: &Layout, scope: &str, version: u64) -> Result<(), String> {
        let mut m = Self::load(layout);
        let seen = m.get(scope).copied().unwrap_or(0);
        if version < seen {
            return Err(format!("definition {scope} version {version} is older than version {seen}, seen before"));
        }
        if version > seen {
            m.insert(scope.to_string(), version);
            let _ = std::fs::create_dir_all(layout.state());
            zerofarm_common::fsutil::atomic_write(&Self::path(layout), &serde_json::to_vec(&m).expect("json"))
                .map_err(|e| format!("cannot record the definition version: {e}"))?;
        }
        Ok(())
    }
}

/// A lease's checkpoint against its definition: the gen is listed and the lease's hashes are the
/// definition's (the files are then fetched with the definition's sizes and hashes). Rev 20 (PROTOCOL 14.4): a checkpoint
/// naming a model `variant` is checked against that variant of its gen (the reference stays the gen's). Returns what the
/// files are checked against.
pub fn match_checkpoint(d: &Definition, c: &CheckpointRef) -> Result<CheckpointDef, String> {
    let gen = d.checkpoint(c.gen).ok_or_else(|| format!("checkpoint {} is not in definition {} version {}", c.gen, d.scope(), d.version))?;
    if let Some(v) = c.variant.as_deref().filter(|v| !def::is_variant_name(v)) {
        return Err(format!("checkpoint {}: {:?} is not a variant name", c.gen, v.chars().take(40).collect::<String>()));
    }
    let want = gen.for_variant(c.variant.as_deref()).ok_or_else(|| {
        format!("checkpoint {}: definition {} version {} doesn't sign variant {}", c.gen, d.scope(), d.version, c.variant.as_deref().unwrap_or(""))
    })?;
    if c.sha256 != want.model_sha256 {
        return Err(format!(
            "checkpoint {}: the lease's {} sha256 is not the definition's",
            c.gen,
            c.variant.as_deref().map_or("model".to_string(), |v| format!("variant {v}"))
        ));
    }
    if c.reference_sha256 != want.reference_sha256 || c.reference.is_some() != want.reference_sha256.is_some() {
        return Err(format!("checkpoint {}: the lease's reference is not the definition's", c.gen));
    }
    Ok(want)
}

/// The checkpoints a lease carries, against what its job needs: self-play at most one
/// `checkpoint`; eval exactly the networks its seats name, in `checkpoints`; fuzz and diag none.
fn check_lease_checkpoints(lease: &Lease, job: &Map<String, Value>) -> Result<(), String> {
    match lease.kind.as_str() {
        "selfplay" => {
            if lease.checkpoints.is_some() {
                return Err("a self-play lease carries `checkpoint`, not `checkpoints`".into());
            }
        }
        "eval" => {
            if lease.checkpoint.is_some() {
                return Err("an eval lease carries `checkpoints`, not `checkpoint`".into());
            }
            let mut listed: Vec<u64> = lease.checkpoints.iter().flatten().map(|c| c.gen).collect();
            let n = listed.len();
            listed.sort_unstable();
            listed.dedup();
            if listed.len() != n {
                return Err("a checkpoint is listed twice".into());
            }
            let mut seats = def::seat_gens(job.get("seats").unwrap_or(&Value::Null))?;
            seats.sort_unstable();
            if listed != seats {
                return Err(format!("the lease's checkpoints {listed:?} are not the networks its seats name {seats:?}"));
            }
        }
        _ => {
            if lease.checkpoint.is_some() || lease.checkpoints.is_some() {
                return Err(format!("a {} lease carries no checkpoint", lease.kind));
            }
        }
    }
    Ok(())
}

impl Engine {
    /// Works out what this lease's worker may be told, or why the lease is refused.
    pub(crate) fn plan(&mut self, lease: &Lease) -> Result<Plan, PlanError> {
        if !self.cfg.kinds.contains(&lease.kind) {
            return refused(format!("kind {:?} is not one this agent asked for", lease.kind));
        }
        // The lease's kind is the job's kind: a job that says otherwise is refused, never obeyed.
        if let Some(k) = lease.job.get("kind").filter(|k| k.as_str() != Some(lease.kind.as_str())) {
            return refused(format!("the job says kind {k}, the lease {}", lease.kind));
        }
        let plan = match &lease.definition {
            Some(_) => self.plan_signed(lease)?,
            None => plan_legacy(lease)?,
        };
        // PROTOCOL 5: a line is at most 1 MB (the agent's fields come on top).
        if serde_json::to_vec(&plan.job).map(|b| b.len()).unwrap_or(usize::MAX) > 900 << 10 {
            return refused("the job is too large");
        }
        Ok(plan)
    }

    fn plan_signed(&mut self, lease: &Lease) -> Result<Plan, PlanError> {
        let dref = lease.definition.as_ref().expect("signed lease");
        let id = &lease.lease_id;
        let d = self.fetch().definition(dref, id)?;
        // Kind and id against the lease's run or campaign.
        let (want_kind, want_id) = match (&lease.run, &lease.campaign) {
            (Some(r), None) => ("run", r),
            (None, Some(c)) => ("campaign", c),
            _ => return refused("a lease names exactly one run or campaign"),
        };
        if d.kind != want_kind || &d.id != want_id {
            return refused(format!("definition {} is not for the lease's {want_kind} {want_id}", d.scope()));
        }
        if !def::job_kinds(&d.kind).contains(&lease.kind.as_str()) {
            return refused(format!("a {} lease can't come from a {} definition", lease.kind, d.kind));
        }
        let now = zerofarm_common::now_unix();
        if let Some(e) = d.expires.filter(|e| now > *e) {
            return refused(format!("definition {} version {} expired at {}", d.scope(), d.version, zerofarm_common::rotlog::utc_timestamp(e)));
        }
        SeenVersions::check_and_record(&self.cfg.layout, &d.scope(), d.version).map_err(PlanError::Refused)?;
        // Builds and runtime.
        if !d.builds.contains(&lease.build.build_id) {
            return refused(format!("worker build {} is not in definition {} version {}", lease.build.build_id, d.scope(), d.version));
        }
        // rev 11 (PROTOCOL 7, 9): the definition's runtime, or the one the lease's worker build itself requires (its
        // signed manifest): a definition authorizes each build it lists with that build's pack (a run alternate).
        let mut alternate = false;
        if lease.runtime.as_ref().map(|r| &r.build_id) != d.runtime.as_ref() {
            let Some(rt) = lease.runtime.as_ref().map(|r| r.build_id.clone()) else {
                return refused(format!("the lease has no runtime, the definition names {:?}", d.runtime));
            };
            let m = self.signed_manifest(&lease.build, Kind::Worker)?;
            if m.requires_runtime.as_deref() != Some(rt.as_str()) {
                return refused(format!(
                    "runtime {rt} is neither the definition's {:?} nor the one worker build {} requires ({:?})",
                    d.runtime, lease.build.build_id, m.requires_runtime
                ));
            }
            alternate = true;
        }
        let template = d.template(&lease.kind).ok_or_else(|| PlanError::Refused(format!("definition {} has no {} job", d.scope(), lease.kind)))?;
        check_lease_checkpoints(lease, template).map_err(PlanError::Refused)?;
        for c in lease.checkpoint.iter().chain(lease.checkpoints.iter().flatten()) {
            match_checkpoint(&d, c).map_err(PlanError::Refused)?;
        }
        let paired = template.get("paired").and_then(Value::as_bool).unwrap_or(false);
        let mut job = template.clone();
        job.extend(def::lease_fields(&lease.kind, &lease.job, paired).map_err(PlanError::Refused)?);
        log::info!("lease {id}: definition {} version {} verified{}", d.scope(), d.version, if alternate { " (an alternate runtime pack)" } else { "" });
        Ok(Plan { job, def: Some(d), alternate })
    }

    /// A build's signed manifest alone (rev 11, PROTOCOL 9.5): see [`Fetch::signed_manifest`].
    pub(crate) fn signed_manifest(&self, r: &ArtifactRef, kind: Kind) -> Result<Manifest, PlanError> {
        self.fetch().signed_manifest(r, kind)
    }

    /// What fetches and verifies definitions and manifests, also off the engine's thread (a rolling lease's new
    /// definition, PROTOCOL 10.6).
    pub(crate) fn fetch(&self) -> Fetch {
        Fetch {
            client: self.client.clone(),
            layout: self.cfg.layout.clone(),
            keys: self.cfg.keys.clone(),
            data_keys: self.cfg.data_keys.clone(),
            control: self.control.clone(),
        }
    }
}

/// Fetches and verifies the small signed files a lease is checked with (definitions, manifests), on any thread.
#[derive(Clone)]
pub struct Fetch {
    pub client: Client,
    pub layout: Layout,
    /// Release keys (manifests) and data keys (definitions): each check accepts only its own list.
    pub keys: TrustedKeys,
    pub data_keys: TrustedKeys,
    pub control: Arc<Control>,
}

impl Fetch {
    /// A definition (PROTOCOL 7 steps 1-2): its reference well formed, its bytes with the referenced sha256, its
    /// signature by a trusted **data** key, and the key it says it is. `tag` names the temporary files (unique per
    /// lease and use).
    pub fn definition(&self, dref: &DefinitionRef, tag: &str) -> Result<Definition, PlanError> {
        if zerofarm_common::http::check_key(&dref.key).is_err()
            || !dref.key.starts_with("defs/")
            || !dref.key.ends_with(&format!("/{}", def::DEFINITION_FILE))
            || !zerofarm_common::is_sha256_hex(&dref.sha256)
        {
            return refused(format!("bad definition reference {:?}", dref.key));
        }
        let bytes = self.fetch_small(&dref.key, Some(&dref.sha256), def::MAX_DEFINITION_BYTES, &format!("def-{tag}.json"))?;
        let sig = self.fetch_small(&format!("{}{}", dref.key, def::SIG_SUFFIX), None, def::MAX_SIG_BYTES, &format!("def-{tag}.minisig"))?;
        let sig = String::from_utf8(sig).map_err(|_| PlanError::Refused("the definition's signature is not text".into()))?;
        let d = def::verify(&self.data_keys, &bytes, &sig).map_err(PlanError::Refused)?;
        if d.key() != dref.key {
            return refused(format!("the definition at {} says it is {}", dref.key, d.key()));
        }
        Ok(d)
    }

    /// A build's signed manifest alone (rev 11, PROTOCOL 9.5): from the cache when the build is there and verifies,
    /// else just `manifest.json` + its signature, verified with the release keys (no other file is fetched).
    pub fn signed_manifest(&self, r: &ArtifactRef, kind: Kind) -> Result<Manifest, PlanError> {
        if !zerofarm_common::is_build_id(&r.build_id) || r.manifest != format!("builds/{}/manifest.json", r.build_id) {
            return refused(format!("bad build reference {:?}", r.build_id));
        }
        let expect = Expect { build_id: Some(&r.build_id), kind: Some(kind), version: None };
        if let Ok(v) = verify_dir(&self.layout.cache_builds().join(&r.build_id), &self.keys, &expect) {
            return Ok(v.manifest);
        }
        let bytes = self.fetch_small(&r.manifest, None, MAX_MANIFEST_BYTES, &format!("m-{}.json", r.build_id))?;
        let sig = self.fetch_small(&format!("{}.minisig", r.manifest), None, MAX_SIG_BYTES, &format!("m-{}.minisig", r.build_id))?;
        let sig = String::from_utf8(sig).map_err(|_| PlanError::Refused("a manifest signature is not text".into()))?;
        let (m, id) = verify_manifest_bytes(&self.keys, &bytes, &sig).map_err(|e| PlanError::Refused(format!("build {}: {e}", r.build_id)))?;
        if id != r.build_id || m.kind != kind {
            return refused(format!("build {}: the manifest is {} {id}", r.build_id, m.kind));
        }
        Ok(m)
    }

    /// Downloads a small file (definition, signature) into `cache\tmp` and returns its bytes.
    fn fetch_small(&self, key: &str, sha: Option<&str>, max: u64, tmp_name: &str) -> Result<Vec<u8>, PlanError> {
        let dir = self.layout.cache_tmp();
        let _ = std::fs::create_dir_all(&dir);
        let dest = dir.join(tmp_name);
        let _ = std::fs::remove_file(&dest);
        let _ = std::fs::remove_file(zerofarm_common::http::part_path(&dest));
        let control = self.control.clone();
        let stop = move || control.quitting();
        let want = Want { size: None, sha256: sha.map(String::from), max };
        let r = zerofarm_common::http::with_retry(&mut zerofarm_common::http::Backoff::default(), Some(3), &stop, || {
            self.client.download(key, &dest, &want, &stop)
        });
        let out = match r {
            Ok(()) => std::fs::read(&dest).map_err(|e| PlanError::Unavailable(format!("{key}: {e}"))),
            Err(CallError::Stopped) => Err(PlanError::Stopped),
            // A hash or size that doesn't match, or a definition the coordinator doesn't have.
            Err(e @ CallError::Integrity(_)) => Err(PlanError::Refused(format!("{key}: {e}"))),
            Err(e) if e.is_retryable() => Err(PlanError::Unavailable(format!("{key}: {e}"))),
            Err(e) => Err(PlanError::Refused(format!("{key}: {e}"))),
        };
        let _ = std::fs::remove_file(&dest);
        out
    }

    /// rev 15 (PROTOCOL 10.6): a rolling lease's new generation comes with the run's current definition. It is
    /// verified as at grant (PROTOCOL 7 steps 1-4: the signature by a data key, kind and id, expiry, a version never
    /// older than any seen - and recorded -, the lease's worker build in `builds` and its runtime authorized as at
    /// grant, the new checkpoint's hashes as signed), and its self-play template must be the lease's own (canonical
    /// JSON): anything else ends the lease gracefully.
    pub fn recheck(&self, lease: &Lease, current: &Definition, dref: &DefinitionRef, c: &CheckpointRef) -> Result<Definition, Recheck> {
        let tag = format!("{}-g{}", lease.lease_id, c.gen);
        let d = self.definition(dref, &tag).map_err(Recheck::from)?;
        let no = |m: String| Err(Recheck::Refused(m));
        let Some(run) = &lease.run else { return no("a rolling lease names a run".into()) };
        if d.kind != "run" || &d.id != run {
            return no(format!("definition {} is not for the lease's run {run}", d.scope()));
        }
        if let Some(e) = d.expires.filter(|e| zerofarm_common::now_unix() > *e) {
            return no(format!("definition {} version {} expired at {}", d.scope(), d.version, zerofarm_common::rotlog::utc_timestamp(e)));
        }
        SeenVersions::check_and_record(&self.layout, &d.scope(), d.version).map_err(Recheck::Refused)?;
        if !d.builds.contains(&lease.build.build_id) {
            return no(format!("worker build {} is not in definition {} version {}", lease.build.build_id, d.scope(), d.version));
        }
        // the runtime as at grant: the definition's, or the one the lease's worker build itself requires (an alternate)
        if lease.runtime.as_ref().map(|r| &r.build_id) != d.runtime.as_ref() {
            let Some(rt) = lease.runtime.as_ref().map(|r| r.build_id.clone()) else {
                return no(format!("the lease has no runtime, definition {} version {} names {:?}", d.scope(), d.version, d.runtime));
            };
            let m = self.signed_manifest(&lease.build, Kind::Worker).map_err(Recheck::from)?;
            if m.requires_runtime.as_deref() != Some(rt.as_str()) {
                return no(format!(
                    "runtime {rt} is neither definition {} version {}'s {:?} nor the one worker build {} requires",
                    d.scope(),
                    d.version,
                    d.runtime,
                    lease.build.build_id
                ));
            }
        }
        match_checkpoint(&d, c).map_err(Recheck::Refused)?;
        let template = |x: &Definition| x.template(&lease.kind).map(|t| canonical_json(&Value::Object(t.clone())));
        if template(&d).is_none() || template(&d) != template(current) {
            return Err(Recheck::TemplateChanged(format!(
                "definition {} version {} has another {} template than version {}",
                d.scope(),
                d.version,
                lease.kind,
                current.version
            )));
        }
        Ok(d)
    }
}

/// Why a rolling lease's new definition isn't taken (PROTOCOL 10.6).
#[derive(Debug)]
pub enum Recheck {
    /// It failed a check: the lease finishes, reported once (crash kind `definition`).
    Refused(String),
    /// Its job template is not the lease's: the lease finishes (the next lease brings the new template).
    TemplateChanged(String),
    /// It couldn't be fetched right now: asked again at a later beat.
    Unavailable(String),
    Stopped,
}

impl From<PlanError> for Recheck {
    fn from(e: PlanError) -> Recheck {
        match e {
            PlanError::Refused(m) => Recheck::Refused(m),
            PlanError::Unavailable(m) => Recheck::Unavailable(m),
            PlanError::Stopped => Recheck::Stopped,
        }
    }
}

/// A legacy lease (no definition): the strict allowlist.
fn plan_legacy(lease: &Lease) -> Result<Plan, PlanError> {
    let kind = lease.kind.as_str();
    let keys = def::template_keys(kind).ok_or_else(|| PlanError::Refused(format!("unknown job kind {kind:?}")))?;
    let src = lease.job.as_object().ok_or_else(|| PlanError::Refused("the lease's job is not an object".into()))?;
    let mut template = Map::new();
    let mut dropped = Vec::new();
    for (k, v) in src {
        if keys.contains(&k.as_str()) {
            template.insert(k.clone(), v.clone());
        } else if !def::lease_keys(kind).contains(&k.as_str()) && k != "kind" {
            dropped.push(k.chars().take(40).collect::<String>());
        }
    }
    if !dropped.is_empty() {
        log::warn!("lease {}: dropping job keys this agent doesn't pass on: {}", lease.lease_id, dropped.join(", "));
    }
    def::check_template(kind, &template).map_err(PlanError::Refused)?;
    check_lease_checkpoints(lease, &template).map_err(PlanError::Refused)?;
    // rev 20 (PROTOCOL 14.4): a model variant is only ever one a definition signs
    if lease.checkpoint.iter().chain(lease.checkpoints.iter().flatten()).any(|c| c.variant.is_some()) {
        return refused("a model variant without a definition");
    }
    let paired = template.get("paired").and_then(Value::as_bool).unwrap_or(false);
    let mut job = template;
    job.extend(def::lease_fields(kind, &lease.job, paired).map_err(PlanError::Refused)?);
    Ok(Plan { job, def: None, alternate: false })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn lease(kind: &str, job: Value) -> Lease {
        serde_json::from_value(json!({"lease_id": "l_1", "kind": kind, "issued_at": 0, "expires_at": 0, "beat_s": 60, "slack_s": 600,
            "build": {"build_id": "0123456789abcdef01234567", "manifest": "m"}, "job": job}))
        .unwrap()
    }

    #[test]
    fn legacy_allowlist_keeps_only_the_kinds_keys() {
        let l = lease(
            "eval",
            json!({"kind": "eval", "gid_base": 0, "games": 2, "seed": 1, "paired": true, "settings": {"sims": 8},
                   "seats": [{"bot": "a"}, {"bot": "b"}], "prefix": "..\\..\\Desktop\\x", "models": {"1": {"model": "\\\\h\\s\\m"}},
                   "ort_dylib": "x", "uniform": true}),
        );
        let p = plan_legacy(&l).unwrap();
        assert_eq!(
            Value::Object(p.job),
            json!({"gid_base": 0, "games": 2, "seed": 1, "paired": true, "settings": {"sims": 8}, "seats": [{"bot": "a"}, {"bot": "b"}]})
        );
    }

    #[test]
    fn legacy_refuses_paths_and_kind_mismatches() {
        let refused = |l: Lease| matches!(plan_legacy(&l), Err(PlanError::Refused(_)));
        assert!(refused(lease("selfplay", json!({"gid_base": 0, "games": 2, "seed": 1, "settings": {"book": "C:\\Users\\x\\a.txt"}}))));
        assert!(refused(lease("selfplay", json!({"kind": "eval", "gid_base": 0, "games": 2, "seed": 1, "settings": {}}))));
        assert!(refused(lease("fuzz", json!({"mode": "m", "env": {"X": "\\\\host\\share"}, "first": 0, "count": 5}))));
        assert!(refused(lease("selfplay", json!({"games": 2, "seed": 1, "settings": {}}))), "a per-lease field missing");
        assert!(refused(lease("evil", json!({}))));
        // Eval checkpoints must be exactly the seats' networks.
        let mut l = lease("eval", json!({"gid_base": 0, "games": 2, "seed": 1, "paired": false, "settings": {}, "seats": [{"net": 1}, {"net": 2}]}));
        assert!(refused(l.clone()), "no checkpoints");
        l.checkpoints = Some(vec![CheckpointRef { gen: 1, model: "ckpt/r/1/model.onnx".into(), sha256: "a".repeat(64), reference: None, reference_sha256: None, variant: None }]);
        assert!(refused(l.clone()), "one missing");
        let mut c2 = l.checkpoints.as_ref().unwrap()[0].clone();
        c2.gen = 2;
        l.checkpoints.as_mut().unwrap().push(c2);
        assert!(plan_legacy(&l).is_ok());
        // rev 20: a model variant needs a definition
        l.checkpoints.as_mut().unwrap()[1].variant = Some("gpu".into());
        assert!(refused(l.clone()), "a variant on a legacy lease");
    }

    #[test]
    fn checkpoints_match_the_definitions_model_or_variant() {
        let d = Definition::parse(
            &serde_json::to_vec(&json!({
                "definition": 1, "kind": "run", "id": "r", "version": 1, "created": 5, "game": "mtg", "builds": ["0123456789abcdef01234567"],
                "checkpoints": {"1": {"model_sha256": "a".repeat(64), "model_size": 10, "reference_sha256": "b".repeat(64), "reference_size": 4,
                                      "variants": {"gpu": {"model_sha256": "c".repeat(64), "model_size": 12}}},
                                "2": {"model_sha256": "e".repeat(64), "model_size": 11}},
                "jobs": {"selfplay": {"settings": {}}}
            }))
            .unwrap(),
        )
        .unwrap();
        let ck = |gen: u64, sha: char, variant: Option<&str>, reference: bool| CheckpointRef {
            gen,
            model: format!("ckpt/r/{gen}/model.onnx"),
            sha256: sha.to_string().repeat(64),
            reference: reference.then(|| format!("ckpt/r/{gen}/reference.npz")),
            reference_sha256: reference.then(|| "b".repeat(64)),
            variant: variant.map(String::from),
        };
        let m = match_checkpoint(&d, &ck(1, 'a', None, true)).unwrap();
        assert_eq!((m.model_size, m.reference_size), (10, Some(4)));
        let v = match_checkpoint(&d, &ck(1, 'c', Some("gpu"), true)).unwrap();
        assert_eq!((v.model_sha256, v.model_size, v.reference_size), ("c".repeat(64), 12, Some(4)), "the variant's file, the gen's reference");
        let e = |c: CheckpointRef| match_checkpoint(&d, &c).unwrap_err();
        assert!(e(ck(1, 'a', Some("gpu"), true)).contains("variant gpu sha256"), "the default's hash under the variant's name");
        assert!(e(ck(1, 'c', None, true)).contains("model sha256"), "the variant's hash as the default");
        assert!(e(ck(1, 'c', Some("fp16"), true)).contains("doesn't sign variant fp16"));
        assert!(e(ck(2, 'e', Some("gpu"), false)).contains("doesn't sign variant gpu"));
        assert!(e(ck(1, 'c', Some("..\\x"), true)).contains("not a variant name"));
        assert!(e(ck(1, 'c', Some("gpu"), false)).contains("reference"), "the reference is still checked");
    }

    #[test]
    fn versions_never_go_back() {
        let d = tempfile::tempdir().unwrap();
        let layout = Layout::new(d.path());
        SeenVersions::check_and_record(&layout, "run/a", 3).unwrap();
        SeenVersions::check_and_record(&layout, "run/a", 3).unwrap();
        assert!(SeenVersions::check_and_record(&layout, "run/a", 2).unwrap_err().contains("older"));
        SeenVersions::check_and_record(&layout, "run/b", 1).unwrap();
        SeenVersions::check_and_record(&layout, "run/a", 9).unwrap();
        assert_eq!(SeenVersions::load(&layout).get("run/a"), Some(&9));
    }
}

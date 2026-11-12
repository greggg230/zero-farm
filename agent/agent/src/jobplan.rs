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

use crate::engine::Engine;
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::path::PathBuf;
use zerofarm_common::definition::{self as def, CheckpointDef, Definition};
use zerofarm_common::http::{CallError, Want};
use zerofarm_common::layout::Layout;
use zerofarm_common::protocol::{CheckpointRef, Lease};

/// What the worker of a lease will be told, before the agent's local fields.
#[derive(Debug, Clone)]
pub struct Plan {
    /// The job template (signed, or the allowlisted legacy job) plus the per-lease fields.
    pub job: Map<String, Value>,
    /// The verified definition; None for a legacy lease.
    pub def: Option<Definition>,
}

impl Plan {
    /// The declared sizes of a checkpoint the plan's definition names (None: legacy).
    pub fn declared(&self, gen: u64) -> Option<&CheckpointDef> {
        self.def.as_ref().and_then(|d| d.checkpoint(gen))
    }

    /// A checkpoint a beat offers mid-lease (self-play): with a definition, only a gen it names,
    /// with its hashes.
    pub fn check_new_checkpoint(&self, c: &CheckpointRef) -> Result<Option<CheckpointDef>, String> {
        match &self.def {
            Some(d) => match_checkpoint(d, c).map(|x| Some(x.clone())),
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
/// definition's (the files are then fetched with the definition's sizes and hashes).
pub fn match_checkpoint<'a>(d: &'a Definition, c: &CheckpointRef) -> Result<&'a CheckpointDef, String> {
    let want = d.checkpoint(c.gen).ok_or_else(|| format!("checkpoint {} is not in definition {} version {}", c.gen, d.scope(), d.version))?;
    if c.sha256 != want.model_sha256 {
        return Err(format!("checkpoint {}: the lease's model sha256 is not the definition's", c.gen));
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
        if zerofarm_common::http::check_key(&dref.key).is_err()
            || !dref.key.starts_with("defs/")
            || !dref.key.ends_with(&format!("/{}", def::DEFINITION_FILE))
            || !zerofarm_common::is_sha256_hex(&dref.sha256)
        {
            return refused(format!("bad definition reference {:?}", dref.key));
        }
        let bytes = self.fetch_small(&dref.key, Some(&dref.sha256), def::MAX_DEFINITION_BYTES, &format!("def-{id}.json"))?;
        let sig = self.fetch_small(&format!("{}{}", dref.key, def::SIG_SUFFIX), None, def::MAX_SIG_BYTES, &format!("def-{id}.minisig"))?;
        let sig = String::from_utf8(sig).map_err(|_| PlanError::Refused("the definition's signature is not text".into()))?;
        let d = def::verify(&self.cfg.data_keys, &bytes, &sig).map_err(PlanError::Refused)?;
        if d.key() != dref.key {
            return refused(format!("the definition at {} says it is {}", dref.key, d.key()));
        }
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
        if lease.runtime.as_ref().map(|r| &r.build_id) != d.runtime.as_ref() {
            return refused(format!("runtime {:?} is not the definition's {:?}", lease.runtime.as_ref().map(|r| &r.build_id), d.runtime));
        }
        let template = d.template(&lease.kind).ok_or_else(|| PlanError::Refused(format!("definition {} has no {} job", d.scope(), lease.kind)))?;
        check_lease_checkpoints(lease, template).map_err(PlanError::Refused)?;
        for c in lease.checkpoint.iter().chain(lease.checkpoints.iter().flatten()) {
            match_checkpoint(&d, c).map_err(PlanError::Refused)?;
        }
        let paired = template.get("paired").and_then(Value::as_bool).unwrap_or(false);
        let mut job = template.clone();
        job.extend(def::lease_fields(&lease.kind, &lease.job, paired).map_err(PlanError::Refused)?);
        log::info!("lease {id}: definition {} version {} verified", d.scope(), d.version);
        Ok(Plan { job, def: Some(d) })
    }

    /// Downloads a small file (definition, signature) into `cache\tmp` and returns its bytes.
    fn fetch_small(&self, key: &str, sha: Option<&str>, max: u64, tmp_name: &str) -> Result<Vec<u8>, PlanError> {
        let dir = self.cfg.layout.cache_tmp();
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
    let paired = template.get("paired").and_then(Value::as_bool).unwrap_or(false);
    let mut job = template;
    job.extend(def::lease_fields(kind, &lease.job, paired).map_err(PlanError::Refused)?);
    Ok(Plan { job, def: None })
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
        l.checkpoints = Some(vec![CheckpointRef { gen: 1, model: "ckpt/r/1/model.onnx".into(), sha256: "a".repeat(64), reference: None, reference_sha256: None }]);
        assert!(refused(l.clone()), "one missing");
        let mut c2 = l.checkpoints.as_ref().unwrap()[0].clone();
        c2.gen = 2;
        l.checkpoints.as_mut().unwrap().push(c2);
        assert!(plan_legacy(&l).is_ok());
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

//! `definition/1` (PROTOCOL 7, rev 6): what a run or campaign may make a friend's PC download and
//! run, signed on the desktop with a **data key** (a list separate from the release keys).
//!
//! The agent builds a worker's job line only from a verified definition's job template, plus the
//! few per-lease fields the coordinator may set (`gid_base`, `games`, `seed` / `first`, `count`)
//! and its own local paths. The template and per-lease checks here are shared by
//! `farm-release define` (which refuses to sign a bad definition), the agent's definition path
//! and its allowlist for legacy leases (coordinators before rev 6), so all three agree.

use crate::keys::TrustedKeys;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::BTreeMap;

/// `definition/1`.
pub const DEFINITION_VERSION: u32 = 1;
pub const DEFINITION_FILE: &str = "definition.json";
/// The signature sits next to the definition: `<key>.minisig`.
pub const SIG_SUFFIX: &str = ".minisig";
/// Upper bound for a definition file (they are a few KB).
pub const MAX_DEFINITION_BYTES: u64 = 1 << 20;
/// Upper bound for its signature file.
pub const MAX_SIG_BYTES: u64 = 4096;
/// Largest checkpoint file (model or reference) a definition may declare.
pub const MAX_CHECKPOINT_FILE_BYTES: u64 = 1 << 30;
/// Most games one lease may carry.
pub const MAX_GAMES: u64 = 100_000;
/// Most fuzz seeds one lease may carry.
pub const MAX_SEEDS: u64 = 100_000_000;
/// Game ids and seeds stay below 2^53 (exact in JSON numbers everywhere).
pub const MAX_ID: u64 = 1 << 53;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CheckpointDef {
    pub model_sha256: String,
    pub model_size: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference_size: Option<u64>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[serde(deny_unknown_fields)]
pub struct Jobs {
    #[serde(default)]
    pub selfplay: Option<Map<String, Value>>,
    #[serde(default)]
    pub eval: Option<Map<String, Value>>,
    #[serde(default)]
    pub fuzz: Option<Map<String, Value>>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Definition {
    pub definition: u32,
    /// `run` | `campaign`
    pub kind: String,
    /// The run or campaign slug.
    pub id: String,
    /// Strictly increasing per (kind, id); agents never accept an older one than they have seen.
    pub version: u64,
    pub created: i64,
    /// Agents refuse it after this time.
    #[serde(default)]
    pub expires: Option<i64>,
    pub game: String,
    /// Worker builds allowed to run it.
    pub builds: Vec<String>,
    /// The runtime pack those builds load (a lease's runtime must equal it).
    #[serde(default)]
    pub runtime: Option<String>,
    /// Networks by gen: every checkpoint file's sha256 and size.
    #[serde(default)]
    pub checkpoints: BTreeMap<String, CheckpointDef>,
    /// Exact job templates; only per-lease fields are added.
    pub jobs: Jobs,
}

/// `defs/<kind>/<id>/<version>/definition.json`.
pub fn key(kind: &str, id: &str, version: u64) -> String {
    format!("defs/{kind}/{id}/{version}/{DEFINITION_FILE}")
}

/// The job kinds a definition kind carries: a run self-play and eval, a campaign fuzz.
pub fn job_kinds(def_kind: &str) -> &'static [&'static str] {
    match def_kind {
        "run" => &["selfplay", "eval"],
        "campaign" => &["fuzz"],
        _ => &[],
    }
}

impl Definition {
    /// Parses and validates `definition.json` bytes (unknown fields are refused).
    pub fn parse(bytes: &[u8]) -> Result<Definition, String> {
        if bytes.len() as u64 > MAX_DEFINITION_BYTES {
            return Err(format!("definition is {} bytes (at most {MAX_DEFINITION_BYTES})", bytes.len()));
        }
        let d: Definition = serde_json::from_slice(bytes).map_err(|e| format!("definition json: {e}"))?;
        d.validate()?;
        Ok(d)
    }

    pub fn key(&self) -> String {
        key(&self.kind, &self.id, self.version)
    }

    /// `run/eoe2`: what versions are counted per.
    pub fn scope(&self) -> String {
        format!("{}/{}", self.kind, self.id)
    }

    /// The trusted comment `farm-release define` signs with (informational: the signed bytes
    /// already say all of it).
    pub fn trusted_comment(&self) -> String {
        format!("definition={} kind={} id={} version={}", self.definition, self.kind, self.id, self.version)
    }

    /// The job template for a lease of `job_kind`, if this definition has one.
    pub fn template(&self, job_kind: &str) -> Option<&Map<String, Value>> {
        match job_kind {
            "selfplay" => self.jobs.selfplay.as_ref(),
            "eval" => self.jobs.eval.as_ref(),
            "fuzz" => self.jobs.fuzz.as_ref(),
            _ => None,
        }
    }

    pub fn checkpoint(&self, gen: u64) -> Option<&CheckpointDef> {
        self.checkpoints.get(&gen.to_string())
    }

    /// Everything a `definition/1` must be, short of its signature.
    pub fn validate(&self) -> Result<(), String> {
        if self.definition != DEFINITION_VERSION {
            return Err(format!("definition version {} (this tool reads {DEFINITION_VERSION})", self.definition));
        }
        if job_kinds(&self.kind).is_empty() {
            return Err(format!("kind {:?} must be run or campaign", self.kind));
        }
        if !crate::is_slug(&self.id) {
            return Err(format!("id {:?} is not a run/campaign slug", self.id));
        }
        if self.version == 0 || self.version > MAX_ID {
            return Err("version must be a positive integer".into());
        }
        if self.created <= 0 {
            return Err("created must be a Unix time".into());
        }
        if self.expires.is_some_and(|e| e <= self.created) {
            return Err("expires must be after created".into());
        }
        if !crate::is_slug(&self.game) {
            return Err(format!("game {:?} is not a slug", self.game));
        }
        if self.builds.is_empty() || self.builds.len() > 64 {
            return Err("builds must list 1 to 64 worker build ids".into());
        }
        for (i, b) in self.builds.iter().enumerate() {
            if !crate::is_build_id(b) {
                return Err(format!("builds: {b:?} is not a build id"));
            }
            if self.builds[..i].contains(b) {
                return Err(format!("builds: {b} is listed twice"));
            }
        }
        if let Some(r) = &self.runtime {
            if !crate::is_build_id(r) {
                return Err(format!("runtime {r:?} is not a build id"));
            }
        }
        if self.checkpoints.len() > 1024 {
            return Err("at most 1024 checkpoints".into());
        }
        for (gen, c) in &self.checkpoints {
            if gen.parse::<u64>().ok().map(|g| g.to_string()).as_deref() != Some(gen.as_str()) {
                return Err(format!("checkpoints: {gen:?} is not a generation number"));
            }
            check_file(&format!("checkpoint {gen} model"), &c.model_sha256, c.model_size)?;
            match (&c.reference_sha256, c.reference_size) {
                (Some(h), Some(n)) => check_file(&format!("checkpoint {gen} reference"), h, n)?,
                (None, None) => {}
                _ => return Err(format!("checkpoint {gen}: reference_sha256 and reference_size go together")),
            }
        }
        let allowed = job_kinds(&self.kind);
        let mut any = false;
        for (k, t) in [("selfplay", &self.jobs.selfplay), ("eval", &self.jobs.eval), ("fuzz", &self.jobs.fuzz)] {
            let Some(t) = t else { continue };
            if !allowed.contains(&k) {
                return Err(format!("a {} defines {} jobs only, not {k}", self.kind, allowed.join(" and ")));
            }
            check_template(k, t).map_err(|e| format!("jobs.{k}: {e}"))?;
            if k == "eval" {
                for g in seat_gens(t.get("seats").unwrap_or(&Value::Null))? {
                    if self.checkpoint(g).is_none() {
                        return Err(format!("jobs.eval: seat network {g} is not in checkpoints"));
                    }
                }
            }
            any = true;
        }
        if !any {
            return Err("jobs defines no job".into());
        }
        Ok(())
    }
}

fn check_file(what: &str, sha: &str, size: u64) -> Result<(), String> {
    if !crate::is_sha256_hex(sha) {
        return Err(format!("{what}: sha256 must be 64 lowercase hex chars"));
    }
    if size == 0 || size > MAX_CHECKPOINT_FILE_BYTES {
        return Err(format!("{what}: size must be 1 to {MAX_CHECKPOINT_FILE_BYTES} bytes"));
    }
    Ok(())
}

/// Checks a definition's signature against the trusted **data** keys, then parses and validates it.
pub fn verify(data_keys: &TrustedKeys, bytes: &[u8], minisig: &str) -> Result<Definition, String> {
    if data_keys.is_empty() {
        return Err("this build trusts no data key".into());
    }
    if bytes.len() as u64 > MAX_DEFINITION_BYTES || minisig.len() as u64 > MAX_SIG_BYTES {
        return Err("definition or signature too large".into());
    }
    data_keys.verify(bytes, minisig).map_err(|e| format!("definition signature: {e}"))?;
    Definition::parse(bytes)
}

// ---------- job templates and per-lease fields ----------

/// The keys a job template (or a legacy lease's job, after the allowlist) may have, per kind.
pub fn template_keys(kind: &str) -> Option<&'static [&'static str]> {
    match kind {
        "selfplay" => Some(&["settings"]),
        "eval" => Some(&["settings", "seats", "paired"]),
        "fuzz" => Some(&["mode", "env", "announce_result"]),
        // Diagnostics leases have no run or campaign, so no definition (legacy only).
        "diag" => Some(&["what", "params"]),
        _ => None,
    }
}

/// The per-lease fields the coordinator sets, per kind (PROTOCOL 7).
pub fn lease_keys(kind: &str) -> &'static [&'static str] {
    match kind {
        "selfplay" | "eval" => &["gid_base", "games", "seed"],
        "fuzz" => &["first", "count"],
        _ => &[],
    }
}

/// A string a worker could take for a file path: separators, `..`, a drive (`C:`). Job
/// templates carry none; paths on a job line are only ever the agent's own. The Oracle name
/// separator ` // ` (a split, adventure or double-faced card's name: "Rough // Tumble") is not a
/// separator: a game spec names such cards; any other `/` still is.
pub fn looks_like_path(s: &str) -> bool {
    let b = s.as_bytes();
    s.replace(" // ", "").contains(['\\', '/']) || s.contains("..") || (b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':')
}

fn shorten(s: &str) -> String {
    let t: String = s.chars().take(60).collect();
    if t.len() < s.len() {
        format!("{t}…")
    } else {
        t
    }
}

/// Every string and object key in `v` (at most 32 levels deep) is free of path-like text.
pub fn check_no_paths(v: &Value, at: &str) -> Result<(), String> {
    fn walk(v: &Value, at: &str, depth: u32) -> Result<(), String> {
        if depth > 32 {
            return Err(format!("{at}: nested too deeply"));
        }
        match v {
            Value::String(s) if looks_like_path(s) => Err(format!("{at}: {:?} looks like a file path", shorten(s))),
            Value::Array(a) => a.iter().enumerate().try_for_each(|(i, x)| walk(x, &format!("{at}[{i}]"), depth + 1)),
            Value::Object(m) => m.iter().try_for_each(|(k, x)| {
                if looks_like_path(k) {
                    return Err(format!("{at}: key {:?} looks like a file path", shorten(k)));
                }
                walk(x, &format!("{at}.{k}"), depth + 1)
            }),
            _ => Ok(()),
        }
    }
    walk(v, at, 0)
}

fn is_name(s: &str, max: usize, extra: &[u8]) -> bool {
    !s.is_empty() && s.len() <= max && s.bytes().all(|c| c.is_ascii_alphanumeric() || extra.contains(&c))
}

/// An eval job's seats (PROTOCOL 5.2): 2 to 16, each `{"net": gen}` or `{"bot": name}` (exactly
/// one; other keys pass through). Returns the networks named, in first-seen order.
pub fn seat_gens(seats: &Value) -> Result<Vec<u64>, String> {
    let a = seats.as_array().ok_or("seats must be a list")?;
    if !(2..=16).contains(&a.len()) {
        return Err(format!("seats: {} seats (2 to 16)", a.len()));
    }
    let mut gens = Vec::new();
    for (i, s) in a.iter().enumerate() {
        let o = s.as_object().ok_or_else(|| format!("seat {i} is not an object"))?;
        match (o.get("net"), o.get("bot")) {
            (Some(n), None) => {
                let g = n.as_u64().ok_or_else(|| format!("seat {i}: net must be a generation number"))?;
                if !gens.contains(&g) {
                    gens.push(g);
                }
            }
            (None, Some(b)) => {
                let b = b.as_str().ok_or_else(|| format!("seat {i}: bot must be a name"))?;
                if !is_name(b, 64, b"_.:-") || b.starts_with("ext:") {
                    return Err(format!("seat {i}: bot {:?} is not a built-in bot name", shorten(b)));
                }
            }
            _ => return Err(format!("seat {i} needs exactly one of net and bot")),
        }
    }
    Ok(gens)
}

/// A job template for `kind`: only that kind's keys, the right types, no path-like strings.
pub fn check_template(kind: &str, t: &Map<String, Value>) -> Result<(), String> {
    let allowed = template_keys(kind).ok_or_else(|| format!("unknown job kind {kind:?}"))?;
    if let Some(k) = t.keys().find(|k| !allowed.contains(&k.as_str())) {
        return Err(format!("key {:?} is not allowed in a {kind} job (allowed: {})", shorten(k), allowed.join(", ")));
    }
    let obj = |k: &str, required: bool| -> Result<(), String> {
        match t.get(k) {
            Some(Value::Object(_)) => Ok(()),
            None if !required => Ok(()),
            _ => Err(format!("{k} must be an object")),
        }
    };
    let boolean = |k: &str, required: bool| -> Result<(), String> {
        match t.get(k) {
            Some(Value::Bool(_)) => Ok(()),
            None if !required => Ok(()),
            _ => Err(format!("{k} must be true or false")),
        }
    };
    match kind {
        "selfplay" => obj("settings", true)?,
        "eval" => {
            obj("settings", true)?;
            seat_gens(t.get("seats").unwrap_or(&Value::Null))?;
            boolean("paired", true)?;
        }
        "fuzz" => {
            let mode = t.get("mode").and_then(Value::as_str).ok_or("mode must be a string")?;
            if !is_name(mode, 128, b"_.-") {
                return Err(format!("mode {:?} is not a mode name", shorten(mode)));
            }
            obj("env", false)?;
            if let Some(Value::Object(env)) = t.get("env") {
                for (k, v) in env {
                    if !is_name(k, 64, b"_") {
                        return Err(format!("env: {:?} is not a variable name", shorten(k)));
                    }
                    if v.as_str().is_none_or(|s| s.len() > 4096) {
                        return Err(format!("env.{k} must be a string of at most 4096 bytes"));
                    }
                }
            }
            boolean("announce_result", false)?;
        }
        "diag" => {
            let what = t.get("what").and_then(Value::as_str).unwrap_or("");
            if !matches!(what, "sweep" | "profile" | "stability") {
                return Err("what must be sweep, profile or stability".into());
            }
            obj("params", false)?;
        }
        _ => unreachable!("template_keys knows the kinds"),
    }
    check_no_paths(&Value::Object(t.clone()), kind)
}

/// The per-lease fields of a lease's `job` for `kind`, checked: integers in range, and for a
/// paired eval whole pairs (`gid_base` and `games` even). A `kind` in the job must equal the
/// lease's. Nothing else of the coordinator's job is taken.
pub fn lease_fields(kind: &str, job: &Value, paired: bool) -> Result<Map<String, Value>, String> {
    let j = job.as_object().ok_or("the lease's job is not an object")?;
    if let Some(k) = j.get("kind") {
        if k.as_str() != Some(kind) {
            return Err(format!("the job says kind {k}, the lease {kind}"));
        }
    }
    let int = |k: &str| -> Result<u64, String> { j.get(k).and_then(Value::as_u64).ok_or_else(|| format!("{k} must be a non-negative integer")) };
    let mut out = Map::new();
    match kind {
        "selfplay" | "eval" => {
            let (base, games, seed) = (int("gid_base")?, int("games")?, int("seed")?);
            if games == 0 || games > MAX_GAMES {
                return Err(format!("games {games} (1 to {MAX_GAMES})"));
            }
            if base > MAX_ID - games {
                return Err(format!("gid_base {base} is too large"));
            }
            if paired && (base % 2 != 0 || games % 2 != 0) {
                return Err(format!("a paired lease carries whole pairs (gid_base {base}, games {games})"));
            }
            out.insert("gid_base".into(), base.into());
            out.insert("games".into(), games.into());
            out.insert("seed".into(), seed.into());
        }
        "fuzz" => {
            let (first, count) = (int("first")?, int("count")?);
            if count == 0 || count > MAX_SEEDS {
                return Err(format!("count {count} (1 to {MAX_SEEDS})"));
            }
            if first > MAX_ID - count {
                return Err(format!("first {first} is too large"));
            }
            out.insert("first".into(), first.into());
            out.insert("count".into(), count.into());
        }
        _ => {}
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const B: &str = "0123456789abcdef01234567";

    fn run_def() -> Value {
        json!({
            "definition": 1, "kind": "run", "id": "eoe2", "version": 7, "created": 1794500000, "expires": null,
            "game": "mtg", "builds": [B], "runtime": null,
            "checkpoints": {"80": {"model_sha256": "a".repeat(64), "model_size": 1000,
                                   "reference_sha256": "b".repeat(64), "reference_size": 10}},
            "jobs": {"selfplay": {"settings": {"sims": 64}},
                     "eval": {"settings": {"sims": 32}, "seats": [{"net": 80}, {"bot": "random"}], "paired": true},
                     "fuzz": null}
        })
    }

    fn parse(v: &Value) -> Result<Definition, String> {
        Definition::parse(&serde_json::to_vec(v).unwrap())
    }

    #[test]
    fn a_good_definition_parses() {
        let d = parse(&run_def()).unwrap();
        assert_eq!(d.key(), "defs/run/eoe2/7/definition.json");
        assert_eq!(d.scope(), "run/eoe2");
        assert_eq!(d.checkpoint(80).unwrap().model_size, 1000);
        assert!(d.template("eval").is_some() && d.template("fuzz").is_none());
        assert_eq!(d.trusted_comment(), "definition=1 kind=run id=eoe2 version=7");
        let c = json!({"definition": 1, "kind": "campaign", "id": "fuzz-delver", "version": 1, "created": 5, "game": "mtg",
                       "builds": [B], "jobs": {"fuzz": {"mode": "delver-games", "env": {"FUZZ_POOL": "delver"}}}});
        parse(&c).unwrap();
    }

    #[test]
    fn bad_definitions_are_refused() {
        let bad = |f: &dyn Fn(&mut Value), why: &str| {
            let mut v = run_def();
            f(&mut v);
            let e = parse(&v).unwrap_err();
            assert!(e.contains(why), "{why:?} not in {e:?}");
        };
        bad(&|v| v["definition"] = json!(2), "definition version");
        bad(&|v| v["surprise"] = json!(1), "unknown field");
        bad(&|v| v["kind"] = json!("build"), "run or campaign");
        bad(&|v| v["id"] = json!("Eoe/2"), "slug");
        bad(&|v| v["version"] = json!(0), "positive");
        bad(&|v| v["expires"] = json!(1), "after created");
        bad(&|v| v["builds"] = json!([]), "1 to 64");
        bad(&|v| v["builds"] = json!([B, B]), "twice");
        bad(&|v| v["runtime"] = json!("x"), "runtime");
        bad(&|v| v["checkpoints"]["080"] = v["checkpoints"]["80"].clone(), "generation number");
        bad(&|v| v["checkpoints"]["80"]["model_size"] = json!(0), "size");
        bad(&|v| v["checkpoints"]["80"]["reference_size"] = Value::Null, "go together");
        bad(&|v| v["jobs"]["fuzz"] = json!({"mode": "x"}), "not fuzz");
        bad(&|v| v["jobs"] = json!({}), "no job");
        bad(&|v| v["jobs"]["selfplay"]["prefix"] = json!("x"), "not allowed");
        bad(&|v| v["jobs"]["selfplay"]["models"] = json!({}), "not allowed");
        bad(&|v| v["jobs"]["selfplay"]["games"] = json!(4), "not allowed");
        bad(&|v| v["jobs"]["selfplay"]["settings"]["book"] = json!("\\\\host\\share\\x"), "file path");
        bad(&|v| v["jobs"]["eval"]["seats"] = json!([{"net": 80}, {"net": 81}]), "not in checkpoints");
        bad(&|v| v["jobs"]["eval"]["seats"] = json!([{"net": 80}, {"bot": "ext:evil"}]), "built-in");
        bad(&|v| v["jobs"]["eval"]["seats"] = json!([{"net": 80}]), "2 to 16");
        bad(&|v| v["jobs"]["eval"]["paired"] = json!("yes"), "paired");
    }

    #[test]
    fn path_like_strings() {
        for p in [r"\\attacker\s\m.onnx", r"C:\Users\x", "c:x", "../x", "a/b", r"x\y", "/etc", "a //b", "a// b", "x // ../y", "/ // /", "a //// b"] {
            assert!(looks_like_path(p), "{p}");
        }
        // (cards named with the Oracle separator, in a game spec: not paths)
        for s in ["delver-games", "mcts:400", "random", "0.25", "{\"game_ms\":50}", "Rough // Tumble", "Tamiyo, Inquisitive Student // Tamiyo, Seasoned Scholar"] {
            assert!(!looks_like_path(s), "{s}");
        }
        assert!(check_no_paths(&json!({"a": [{"b": "ok"}, {"c": "..\\x"}]}), "job").unwrap_err().contains("job.a[1].c"));
    }

    #[test]
    fn templates_per_kind() {
        let t = |v: Value| v.as_object().unwrap().clone();
        check_template("fuzz", &t(json!({"mode": "delver-games", "env": {"FUZZ_POOL": "delver"}, "announce_result": false}))).unwrap();
        assert!(check_template("fuzz", &t(json!({"mode": "x", "env": {"A": 1}}))).is_err());
        assert!(check_template("fuzz", &t(json!({"mode": "x", "env": {"PATH": "C:\\evil"}}))).is_err());
        assert!(check_template("diag", &t(json!({"what": "rm"}))).is_err());
        check_template("diag", &t(json!({"what": "sweep", "params": {"threads": [1, 2]}}))).unwrap();
        assert!(check_template("selfplay", &t(json!({}))).unwrap_err().contains("settings"));
        assert!(check_template("nope", &t(json!({}))).is_err());
    }

    #[test]
    fn per_lease_fields() {
        let f = lease_fields("eval", &json!({"kind": "eval", "gid_base": 10, "games": 4, "seed": 3, "settings": {"x": 1}}), true).unwrap();
        assert_eq!(Value::Object(f), json!({"gid_base": 10, "games": 4, "seed": 3}), "only the per-lease fields");
        assert!(lease_fields("eval", &json!({"gid_base": 11, "games": 4, "seed": 3}), true).unwrap_err().contains("pairs"));
        assert!(lease_fields("eval", &json!({"gid_base": 11, "games": 4, "seed": 3}), false).is_ok());
        assert!(lease_fields("selfplay", &json!({"kind": "eval", "gid_base": 0, "games": 4, "seed": 3}), false).unwrap_err().contains("kind"));
        assert!(lease_fields("selfplay", &json!({"gid_base": 0, "games": 0, "seed": 3}), false).is_err());
        assert!(lease_fields("selfplay", &json!({"gid_base": u64::MAX, "games": 4, "seed": 3}), false).is_err());
        assert!(lease_fields("selfplay", &json!({"gid_base": -1, "games": 4, "seed": 3}), false).is_err());
        let f = lease_fields("fuzz", &json!({"first": 5, "count": 10, "mode": "x"}), false).unwrap();
        assert_eq!(Value::Object(f), json!({"first": 5, "count": 10}));
        assert!(lease_fields("fuzz", &json!({"first": 5}), false).is_err());
        assert!(lease_fields("diag", &json!({"what": "sweep"}), false).unwrap().is_empty());
    }
}

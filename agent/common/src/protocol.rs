//! HTTP API v1 types (PROTOCOL 3 and 4). Unknown fields are ignored everywhere, so the
//! coordinator can add fields without breaking deployed agents.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

pub type Stats = Map<String, Value>;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub struct Gpu {
    pub vendor: String,
    pub name: String,
    pub vram_mb: u64,
    pub driver: String,
    /// rev 7 (PROTOCOL 8): the adapter LUID, 16 lowercase hex digits of `(HighPart << 32) |
    /// LowPart`; `gpus[]` has one entry per LUID. Absent from older agents.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub luid: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub struct Profile {
    pub os: String,
    pub cpu: String,
    pub cores: u32,
    pub threads: u32,
    pub perf_cores: u32,
    pub eff_cores: u32,
    pub ram_mb: u64,
    pub disk_free_mb: u64,
    pub gpus: Vec<Gpu>,
    /// rev 7 (PROTOCOL 8): the execution providers that passed this PC's probe with any runtime pack,
    /// preference order, `cpu` last (`["cpu"]` before any probe).
    pub providers: Vec<String>,
    /// rev 7: per runtime pack build_id, the providers that passed with it (absent before any probe).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_providers: Option<std::collections::BTreeMap<String, Vec<String>>>,
    /// Availability: hours per day the idle policy let this PC work / the agent was running,
    /// 7-day means of complete days (absent before the first complete day).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_h_day: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_h_day: Option<f64>,
}

impl Profile {
    /// sha256 of the canonical JSON: keys sorted, no whitespace (PROTOCOL 3, `profile_digest`).
    pub fn digest(&self) -> String {
        let v = serde_json::to_value(self).expect("profile serialises");
        crate::hash::sha256_hex(canonical_json(&v).as_bytes())
    }
}

/// Canonical JSON: object keys sorted (serde_json's default map is a BTreeMap), compact.
pub fn canonical_json(v: &Value) -> String {
    fn sort(v: &Value) -> Value {
        match v {
            Value::Object(m) => {
                let mut keys: Vec<&String> = m.keys().collect();
                keys.sort();
                let mut out = Map::new();
                for k in keys {
                    out.insert(k.clone(), sort(&m[k]));
                }
                Value::Object(out)
            }
            Value::Array(a) => Value::Array(a.iter().map(sort).collect()),
            other => other.clone(),
        }
    }
    serde_json::to_string(&sort(v)).expect("json")
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct EnrollReq {
    pub invite: String,
    pub name: String,
    pub profile: Profile,
    /// rev 2: lets the first `/v1/lease` skip resending the profile.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_digest: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct EnrollResp {
    pub machine_id: String,
    pub token: String,
    #[serde(default = "default_channel")]
    pub channel: String,
}

pub fn default_channel() -> String {
    "stable".into()
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub struct Caps {
    pub threads: u32,
    pub ram_mb: u64,
    pub gpu: bool,
    pub disk_mb: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub struct Cached {
    pub builds: Vec<String>,
    pub ckpts: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct LeaseReq {
    pub profile_digest: String,
    pub profile: Option<Profile>,
    pub caps: Caps,
    pub cached: Cached,
    pub kinds: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ArtifactRef {
    pub build_id: String,
    pub manifest: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct CheckpointRef {
    pub gen: u64,
    pub model: String,
    pub sha256: String,
    #[serde(default)]
    pub reference: Option<String>,
    #[serde(default)]
    pub reference_sha256: Option<String>,
}

/// rev 6 (PROTOCOL 7): where a lease's signed definition is (`defs/<kind>/<id>/<version>/
/// definition.json`, signature next to it as `.minisig`) and the sha256 of its bytes.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct DefinitionRef {
    pub key: String,
    pub sha256: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Lease {
    pub lease_id: String,
    pub kind: String,
    pub issued_at: i64,
    pub expires_at: i64,
    pub beat_s: u64,
    pub slack_s: u64,
    #[serde(default)]
    pub run: Option<String>,
    #[serde(default)]
    pub campaign: Option<String>,
    pub build: ArtifactRef,
    #[serde(default)]
    pub runtime: Option<ArtifactRef>,
    #[serde(default)]
    pub checkpoint: Option<CheckpointRef>,
    /// rev 5 (eval, PROTOCOL 5.2): every network the job's seats name; `checkpoint` is null.
    #[serde(default)]
    pub checkpoints: Option<Vec<CheckpointRef>>,
    /// rev 6 (PROTOCOL 7): the signed definition the job line is built from. Absent on legacy
    /// leases (coordinators before rev 6).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub definition: Option<DefinitionRef>,
    /// rev 7 (PROTOCOL 8): the job's execution-provider allowlist (null: any). The agent only narrows
    /// its verified providers with it; it is a routing hint, not part of the signed job.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub providers: Option<Vec<String>>,
    /// The per-lease fields (`gid_base`, `games`, `seed` / `first`, `count`); before rev 6 the
    /// whole job. The agent never forwards anything else from it (PROTOCOL 7).
    pub job: Value,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct UpdateReq {
    pub min_agent: String,
}

/// `/v1/lease` answer: exactly one of `lease`, `wait_s`, `update`.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct LeaseResp {
    #[serde(default)]
    pub lease: Option<Lease>,
    #[serde(default)]
    pub wait_s: Option<u64>,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub update: Option<UpdateReq>,
    /// rev 2: any lease answer may ask for diagnostics (`["logs"]`).
    #[serde(default)]
    pub upload_diag: Option<Vec<String>>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub struct Progress {
    pub games_done: u64,
    pub games_total: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct BeatReq {
    pub state: String,
    pub progress: Progress,
    pub stats: Stats,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct BeatResp {
    pub action: String,
    #[serde(default)]
    pub expires_at: Option<i64>,
    #[serde(default)]
    pub checkpoint: Option<CheckpointRef>,
    #[serde(default)]
    pub upload_diag: Option<Vec<String>>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct DoneReq {
    pub results: Vec<u64>,
    pub stats: Stats,
    pub given_back: bool,
    pub unfinished: Option<Value>,
    /// rev 6: the agent refused the lease (`"definition"`: its definition or its job failed the
    /// checks of PROTOCOL 7); nothing was run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refused: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct AgentLatest {
    pub version: String,
    pub build_id: String,
    pub manifest: String,
    #[serde(default)]
    pub min_agent: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub struct CrashReq {
    pub build_id: Option<String>,
    pub kind: String,
    pub message: String,
    pub backtrace: Option<String>,
    pub lease_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// rev 5, kind `blocked`: the Windows error that refused to start the worker (4551, 1260, 225).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub os_error: Option<i64>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct ApiError {
    #[serde(default)]
    pub error: String,
    #[serde(default)]
    pub message: String,
}

/// `PUT .../result/{seq}` answer (rev 2): first copy of an id wins.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct ResultResp {
    #[serde(default)]
    pub duplicate: bool,
    #[serde(default)]
    pub credited: u64,
    #[serde(default)]
    pub already_credited: u64,
    /// rev 5 (paired eval): games of this chunk waiting for their pair-mate (not credited yet).
    #[serde(default)]
    pub held: u64,
}

/// Header names (PROTOCOL 1 and 3).
pub mod headers {
    pub const AGENT: &str = "x-farm-agent";
    pub const AW: &str = "x-farm-aw";
    pub const SHA256: &str = "x-sha256";
    /// rev 2: JSON half-open ranges of the game ids / seeds a result chunk completes.
    pub const IDS: &str = "x-ids";
    /// rev 2 (self-play): the oldest checkpoint generation any game in the chunk used.
    pub const GEN: &str = "x-gen";
    /// `user` on `POST /v1/diag/{name}`: the friend pressed "Send diagnostics" (no request
    /// needed; once an hour, 20 MB).
    pub const DIAG: &str = "x-farm-diag";
    /// `1` on every answer the coordinator itself makes (not a WAF or proxy in front of it). The
    /// agent forgets its enrollment only on a coordinator-marked `revoked` (security review M1).
    pub const COORDINATOR: &str = "x-zerofarm-coordinator";
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digest_is_order_independent_and_stable() {
        let p = Profile { os: "Windows 11 26100".into(), cpu: "x".into(), cores: 8, threads: 16, ..Default::default() };
        let d1 = p.digest();
        let v: Value = serde_json::from_str(&serde_json::to_string(&p).unwrap()).unwrap();
        let mut m = v.as_object().unwrap().clone();
        // Re-insert in reverse order: canonical form must not care.
        let mut rev = Map::new();
        let keys: Vec<String> = m.keys().cloned().collect();
        for k in keys.iter().rev() {
            rev.insert(k.clone(), m.remove(k).unwrap());
        }
        let p2: Profile = serde_json::from_value(Value::Object(rev)).unwrap();
        assert_eq!(d1, p2.digest());
        assert_eq!(
            canonical_json(&serde_json::json!({"b": 1, "a": {"d": [2, {"z": 1, "y": 2}], "c": null}})),
            r#"{"a":{"c":null,"d":[2,{"y":2,"z":1}]},"b":1}"#
        );
    }

    #[test]
    fn gpu_luid_is_optional() {
        let old: Gpu = serde_json::from_str(r#"{"vendor": "amd", "name": "RX 7900", "vram_mb": 20464, "driver": "1.2"}"#).unwrap();
        assert_eq!(old.luid, None);
        let v = serde_json::to_value(&old).unwrap();
        assert!(v.get("luid").is_none(), "no luid key when unknown (old digests unchanged)");
        let g = Gpu { luid: Some("000000000000d1f3".into()), ..old };
        let back: Gpu = serde_json::from_str(&serde_json::to_string(&g).unwrap()).unwrap();
        assert_eq!(back.luid.as_deref(), Some("000000000000d1f3"));
    }

    #[test]
    fn lease_resp_variants() {
        let w: LeaseResp = serde_json::from_str(r#"{"wait_s": 300, "reason": "no_work"}"#).unwrap();
        assert_eq!(w.wait_s, Some(300));
        let u: LeaseResp = serde_json::from_str(r#"{"update": {"min_agent": "0.4.0"}}"#).unwrap();
        assert_eq!(u.update.unwrap().min_agent, "0.4.0");
        let l: LeaseResp = serde_json::from_str(
            r#"{"lease": {"lease_id": "l_1", "kind": "fuzz", "issued_at": 1, "expires_at": 2, "beat_s": 60,
                "slack_s": 600, "run": null, "campaign": "fuzz-delver",
                "build": {"build_id": "0123456789abcdef01234567", "manifest": "builds/x/manifest.json"},
                "runtime": null, "checkpoint": null, "job": {"kind": "fuzz", "first": 5, "count": 10},
                "future_field": true}}"#,
        )
        .unwrap();
        assert_eq!(l.lease.unwrap().campaign.as_deref(), Some("fuzz-delver"));
        // rev 5: an eval lease names its networks in `checkpoints`.
        let e: Lease = serde_json::from_str(
            r#"{"lease_id": "l_2", "kind": "eval", "issued_at": 1, "expires_at": 2, "beat_s": 60, "slack_s": 600,
                "run": "h2h-a", "build": {"build_id": "0123456789abcdef01234567", "manifest": "m"}, "checkpoint": null,
                "checkpoints": [{"gen": 80, "model": "ckpt/h2h-a/80/model.onnx", "sha256": "ab", "reference": null,
                                 "reference_sha256": null}],
                "job": {"kind": "eval", "games": 2, "gid_base": 0}}"#,
        )
        .unwrap();
        assert_eq!(e.checkpoints.unwrap()[0].gen, 80);
        let c = CrashReq { kind: "blocked".into(), os_error: Some(4551), ..Default::default() };
        assert_eq!(serde_json::to_value(&c).unwrap()["os_error"], 4551);
        assert!(serde_json::to_value(CrashReq::default()).unwrap().get("os_error").is_none());
    }
}

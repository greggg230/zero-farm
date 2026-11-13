//! Execution-provider probes (PROTOCOL 8): which ONNX Runtime providers pass on this PC.
//!
//! Between leases, when the GPU may be used, the agent runs a cached worker build's
//! `probe-providers --runtime-dir <pack> --ensure-ready` (a small built-in graph and the self-test
//! per provider and adapter; on Windows 11 24H2+ it also lets Windows ML install the GPU vendor's
//! provider) for every cached runtime pack that a cached worker build loads and that has no result
//! yet for this PC's adapters and drivers. Results are kept in `state\probes.json`, keyed by
//! (runtime build, adapters with their driver versions): a new driver or runtime pack is probed
//! again, a reboot (new LUIDs) is not. A failed probe is tried again after a day.
//!
//! What the results become: the profile's `providers` (every provider that passed with any
//! runtime pack, preference order, `cpu` last) and `runtime_providers` (per runtime pack); `caps.gpu`
//! only when a GPU provider passed; and a job line's `providers` and `gpu` (`plan_providers`).
//! Only provider names, adapter names and timings are kept; the readable report goes to the worker
//! log (what "Send diagnostics" uploads, redacted).

use crate::worker::{SpawnOpts, WorkerMsg, WorkerProc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use zerofarm_common::layout::Layout;
use zerofarm_common::protocol::Gpu;
use zerofarm_common::rotlog::RotatingFile;

/// Provider names in the default order of preference (the worker's `ProviderKind::ALL`).
pub const ORDER: [&str; 7] = ["tensorrt_rtx", "cuda", "amdgpu", "migraphx", "openvino", "dml", "cpu"];

/// How long a probe may take (the first one on a Windows 11 24H2+ PC downloads the GPU provider).
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(900);
/// A failed probe is tried again after this long.
pub const RETRY_S: i64 = 24 * 3600;

/// One (provider, adapter) result.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Pass {
    pub provider: String,
    /// The adapter's name (the profile's `gpus[].name`); None for the CPU.
    #[serde(default)]
    pub adapter: Option<String>,
    pub ok: bool,
    #[serde(default)]
    pub max_abs: Option<f64>,
    #[serde(default)]
    pub cpu_nodes: Option<u64>,
    #[serde(default)]
    pub rows_per_s: Option<f64>,
    #[serde(default)]
    pub why: Option<String>,
}

/// A probe of one runtime pack on this PC's adapters.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Probe {
    pub runtime: String,
    pub adapters: String,
    pub worker: String,
    pub t: i64,
    /// Passed, in preference order, `cpu` last.
    pub providers: Vec<String>,
    #[serde(default)]
    pub results: Vec<Pass>,
    #[serde(default)]
    pub error: Option<String>,
}

/// The adapters part of a probe's key: each adapter's vendor, name and driver version, sorted.
pub fn adapters_key(gpus: &[Gpu]) -> String {
    let mut v: Vec<String> = gpus.iter().map(|g| format!("{}|{}|{}", g.vendor, g.name, g.driver)).collect();
    v.sort();
    v.join(";")
}

fn path(layout: &Layout) -> PathBuf {
    layout.state().join("probes.json")
}

pub fn load(layout: &Layout) -> Vec<Probe> {
    std::fs::read(path(layout)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

fn save(layout: &Layout, v: &[Probe]) {
    let _ = std::fs::create_dir_all(layout.state());
    let _ = zerofarm_common::fsutil::atomic_write(&path(layout), &serde_json::to_vec_pretty(v).expect("json"));
}

/// Records `p`, replacing the probe of the same key; at most 32 kept (newest).
pub fn record(layout: &Layout, p: Probe) {
    let mut v = load(layout);
    v.retain(|x| !(x.runtime == p.runtime && x.adapters == p.adapters));
    v.push(p);
    v.sort_by_key(|x| std::cmp::Reverse(x.t));
    v.truncate(32);
    save(layout, &v);
}

/// The probe of `runtime` for these adapters, if any.
pub fn find<'a>(probes: &'a [Probe], runtime: &str, adapters: &str) -> Option<&'a Probe> {
    probes.iter().find(|p| p.runtime == runtime && p.adapters == adapters)
}

fn ordered(set: impl IntoIterator<Item = String>) -> Vec<String> {
    let set: Vec<String> = set.into_iter().collect();
    let mut out: Vec<String> = ORDER.iter().filter(|p| set.iter().any(|s| s == *p)).map(|s| s.to_string()).collect();
    if !out.iter().any(|p| p == "cpu") {
        out.push("cpu".into());
    }
    out
}

/// The profile's `providers` (any runtime pack, these adapters) and `runtime_providers`.
pub fn profile_providers(layout: &Layout, gpus: &[Gpu]) -> (Vec<String>, Option<BTreeMap<String, Vec<String>>>) {
    let key = adapters_key(gpus);
    let probes: Vec<Probe> = load(layout).into_iter().filter(|p| p.adapters == key && p.error.is_none()).collect();
    let all = ordered(probes.iter().flat_map(|p| p.providers.clone()));
    if probes.is_empty() {
        return (all, None);
    }
    let mut per: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for p in probes.iter().take(16) {
        per.insert(p.runtime.clone(), ordered(p.providers.clone()));
    }
    (all, Some(per))
}

/// Whether the PC verified a GPU provider (any runtime pack).
pub fn has_gpu_provider(providers: &[String]) -> bool {
    providers.iter().any(|p| p != "cpu")
}

/// A job line's `providers` and `gpu` (PROTOCOL 8): the adapter with the most video memory among those
/// where an allowed GPU provider passed with this runtime pack (a discrete GPU before an integrated one),
/// then every allowed provider that passed on it in preference order, then the CPU. `["cpu"]` and null when
/// the GPU may not be used now, the pack was never probed here, or nothing else passed.
pub fn plan_providers(probe: Option<&Probe>, gpus: &[Gpu], allow: Option<&[String]>, gpu_ok: bool) -> (Vec<String>, Value) {
    let cpu_only = (vec!["cpu".to_string()], Value::Null);
    let Some(p) = probe.filter(|p| p.error.is_none()) else { return cpu_only };
    if !gpu_ok {
        return cpu_only;
    }
    let allowed = |prov: &str| prov != "cpu" && allow.is_none_or(|a| a.iter().any(|x| x == prov));
    let passed_on = |name: &str| -> Vec<String> {
        ORDER.iter().filter(|q| allowed(q) && p.results.iter().any(|r| r.ok && r.provider == **q && r.adapter.as_deref() == Some(name))).map(|s| s.to_string()).collect()
    };
    let best = gpus.iter().filter(|g| !passed_on(&g.name).is_empty()).max_by_key(|g| g.vram_mb);
    let Some(g) = best else { return cpu_only };
    let mut list = passed_on(&g.name);
    list.push("cpu".into());
    (list, json!({"luid": g.luid.clone().unwrap_or_default(), "vendor": g.vendor, "name": g.name}))
}

/// What to probe now: (worker build dir + entry, runtime build id, runtime dir) of a cached pair without
/// a fresh result for these adapters, newest worker first.
pub fn next_probe(layout: &Layout, keys: &zerofarm_common::keys::TrustedKeys, gpus: &[Gpu], now: i64) -> Option<(PathBuf, String, String, PathBuf)> {
    use zerofarm_common::manifest::{verify_dir, Expect, Kind};
    let key = adapters_key(gpus);
    let probes = load(layout);
    let mut workers = Vec::new();
    for e in std::fs::read_dir(layout.cache_builds()).ok()?.flatten() {
        let id = e.file_name().to_string_lossy().to_string();
        if !zerofarm_common::is_build_id(&id) {
            continue;
        }
        let Ok(v) = verify_dir(&e.path(), keys, &Expect { build_id: Some(&id), kind: Some(Kind::Worker), version: None }) else { continue };
        if v.manifest.zero_onnx.is_none() || v.manifest.aw != Some(zerofarm_common::AW_VERSION) {
            continue;
        }
        let Some(rt) = v.manifest.requires_runtime.clone() else { continue };
        workers.push((v.manifest.created, v, rt));
    }
    workers.sort_by_key(|w| std::cmp::Reverse(w.0));
    for (_, w, rt) in workers {
        if let Some(p) = find(&probes, &rt, &key) {
            // a fresh result, or a failure tried less than a day ago
            if p.error.is_none() || now - p.t < RETRY_S {
                continue;
            }
        }
        let rdir = layout.cache_builds().join(&rt);
        if verify_dir(&rdir, keys, &Expect { build_id: Some(&rt), kind: Some(Kind::Runtime), version: None }).is_err() {
            continue;
        }
        let entry = w.entry_path()?;
        return Some((entry, w.build_id.clone(), rt, rdir));
    }
    None
}

/// The worker's JSON report -> a `Probe` (the adapters' names stand for their LUIDs).
pub fn from_report(v: &Value, runtime: &str, adapters: &str, worker: &str, t: i64) -> Probe {
    let mut results = Vec::new();
    for r in v["results"].as_array().into_iter().flatten() {
        let st = &r["selftest"];
        results.push(Pass {
            provider: r["provider"].as_str().unwrap_or("").to_string(),
            adapter: r["gpu"]["name"].as_str().map(String::from),
            ok: r["ok"].as_bool().unwrap_or(false),
            max_abs: st["max_abs"].as_f64(),
            cpu_nodes: st["cpu_fallback_nodes"].as_array().map(|a| a.len() as u64),
            rows_per_s: r["rows_per_s"].as_f64(),
            why: r["why"].as_str().map(|s| s.chars().take(300).collect()),
        });
    }
    let providers: Vec<String> = v["providers"].as_array().into_iter().flatten().filter_map(Value::as_str).map(String::from).collect();
    Probe {
        runtime: runtime.into(),
        adapters: adapters.into(),
        worker: worker.into(),
        t,
        providers: ordered(providers),
        results,
        error: v["error"].as_str().map(String::from),
    }
}

/// Runs one probe (in a job object, below-normal priority, the runtime pack first on the DLL path),
/// at most `timeout`; the readable report goes to the worker log.
pub fn run(
    layout: &Layout,
    entry: &Path,
    worker: &str,
    runtime: &str,
    rdir: &Path,
    gpus: &[Gpu],
    log: Arc<RotatingFile>,
    timeout: Duration,
    stop: &dyn Fn() -> bool,
) -> Probe {
    let now = zerofarm_common::now_unix();
    let akey = adapters_key(gpus);
    let fail = |e: String| Probe { runtime: runtime.into(), adapters: akey.clone(), worker: worker.into(), t: now, providers: vec!["cpu".into()], results: vec![], error: Some(e) };
    let work = layout.work().join("probe");
    let _ = zerofarm_common::fsutil::remove_dir_all_retry(&work, Duration::from_secs(5));
    if let Err(e) = std::fs::create_dir_all(&work) {
        return fail(format!("work dir: {e}"));
    }
    let out = work.join("probe.json");
    let out_s = out.to_string_lossy().to_string();
    let rdir_s = rdir.to_string_lossy().to_string();
    let args = ["probe-providers", "--runtime-dir", rdir_s.as_str(), "--ensure-ready", "--threads", "2", "--reps", "2", "--json", out_s.as_str()];
    let t0 = Instant::now();
    let spawn = WorkerProc::spawn(
        SpawnOpts { exe: entry, args: &args, cwd: &work, mem_limit_mb: 4096, cpu_job: false, log, tag: "probe".into(), runtime_dir: Some(rdir) },
        |_m: WorkerMsg| {},
    );
    let mut p = match spawn {
        Ok(p) => p,
        Err(e) => return fail(format!("could not start the probe: {e}")),
    };
    let code = loop {
        if let Some(c) = p.try_wait() {
            break Some(c);
        }
        if t0.elapsed() > timeout || stop() {
            p.kill();
            break None;
        }
        std::thread::sleep(Duration::from_millis(200));
    };
    let r = match code {
        None if stop() => fail("stopped".into()),
        None => fail(format!("no answer within {} s", timeout.as_secs())),
        Some(c) => match std::fs::read(&out).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok()) {
            Some(v) => from_report(&v, runtime, &akey, worker, now),
            // (a worker build without the command answers usage and 2: never tried again with it)
            None => fail(format!("the probe exited {c} without a report")),
        },
    };
    let _ = zerofarm_common::fsutil::remove_dir_all_retry(&work, Duration::from_secs(5));
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gpu(name: &str, vendor: &str, luid: &str) -> Gpu {
        Gpu { vendor: vendor.into(), name: name.into(), vram_mb: 1, driver: "1.0".into(), luid: Some(luid.into()) }
    }

    fn pass(prov: &str, adapter: Option<&str>, ok: bool) -> Pass {
        Pass { provider: prov.into(), adapter: adapter.map(String::from), ok, max_abs: None, cpu_nodes: None, rows_per_s: None, why: None }
    }

    #[test]
    fn plans_follow_the_probe_the_allowlist_and_the_policy() {
        let mut gpus = [gpu("UHD 770", "intel", "00000000000000b2"), gpu("RX 9070 XT", "amd", "00000000000000a1")];
        gpus[1].vram_mb = 16304; // (the discrete card is chosen though DXGI lists the integrated one first)
        let p = Probe {
            runtime: "r".into(),
            adapters: adapters_key(&gpus),
            worker: "w".into(),
            t: 0,
            providers: vec!["amdgpu".into(), "dml".into(), "cpu".into()],
            results: vec![pass("cpu", None, true), pass("amdgpu", Some("RX 9070 XT"), true), pass("dml", Some("RX 9070 XT"), true), pass("dml", Some("UHD 770"), true), pass("openvino", Some("UHD 770"), false)],
            error: None,
        };
        let (list, gpu) = plan_providers(Some(&p), &gpus, None, true);
        assert_eq!(list, vec!["amdgpu", "dml", "cpu"]);
        assert_eq!(gpu["luid"], "00000000000000a1");
        let allow = vec!["dml".to_string(), "cpu".to_string()];
        let (list, gpu) = plan_providers(Some(&p), &gpus, Some(&allow), true);
        assert_eq!((list, gpu["name"].as_str()), (vec!["dml".to_string(), "cpu".to_string()], Some("RX 9070 XT")));
        assert_eq!(plan_providers(Some(&p), &gpus, None, false).0, vec!["cpu"]);
        assert_eq!(plan_providers(None, &gpus, None, true).1, Value::Null);
        let only_cpu = vec!["cpu".to_string()];
        assert_eq!(plan_providers(Some(&p), &gpus, Some(&only_cpu), true).0, vec!["cpu"]);
    }

    /// A real worker and runtime pack (`ZEROFARM_TEST_PROBE_EXE`, `ZEROFARM_TEST_RUNTIME_DIR`): the probe runs, its
    /// report is read, the CPU passes. `cargo test -p farm-agent probe -- --ignored`.
    #[test]
    #[ignore]
    fn a_real_probe_runs() {
        let (Ok(exe), Ok(rt)) = (std::env::var("ZEROFARM_TEST_PROBE_EXE"), std::env::var("ZEROFARM_TEST_RUNTIME_DIR")) else { return };
        let d = tempfile::tempdir().unwrap();
        let layout = Layout::new(d.path());
        let gpus = crate::profile::gpus(None);
        let log = Arc::new(RotatingFile::new(d.path().join("worker.log"), 1 << 20, 1));
        let p = run(&layout, Path::new(&exe), "w", "rt", Path::new(&rt), &gpus, log, Duration::from_secs(600), &|| false);
        let report = std::fs::read_to_string(d.path().join("worker.log")).unwrap_or_default();
        assert!(p.error.is_none(), "{p:?}
{report}");
        assert!(p.providers.contains(&"cpu".to_string()) && p.results.iter().any(|r| r.provider == "cpu" && r.ok), "{p:?}");
        assert!(report.contains("probe-providers:"), "{report}");
        eprintln!("{p:#?}");
        record(&layout, p.clone());
        let (all, per) = profile_providers(&layout, &gpus);
        eprintln!("profile providers {all:?} runtime_providers {per:?}; plan {:?}", plan_providers(Some(&p), &gpus, None, true));
    }

    #[test]
    fn reports_become_probes_and_profiles() {
        let d = tempfile::tempdir().unwrap();
        let layout = Layout::new(d.path());
        let gpus = [gpu("RTX 4070 Ti", "nvidia", "000000000000d1f3")];
        let report = json!({"providers": ["dml", "cpu"], "error": null, "results": [
            {"provider": "cpu", "gpu": null, "ok": true, "selftest": {"max_abs": 3e-7, "cpu_fallback_nodes": []}, "rows_per_s": 1000.0},
            {"provider": "tensorrt_rtx", "gpu": {"name": "RTX 4070 Ti", "luid": "000000000000d1f3"}, "ok": false, "why": "runs no node"},
            {"provider": "dml", "gpu": {"name": "RTX 4070 Ti", "luid": "000000000000d1f3"}, "ok": true, "selftest": {"max_abs": 8e-6, "cpu_fallback_nodes": ["a", "b"]}}]});
        let p = from_report(&report, "rt1", &adapters_key(&gpus), "w1", 100);
        assert_eq!(p.providers, vec!["dml", "cpu"]);
        assert_eq!(p.results[2].cpu_nodes, Some(2));
        record(&layout, p);
        let (all, per) = profile_providers(&layout, &gpus);
        assert_eq!(all, vec!["dml", "cpu"]);
        assert_eq!(per.unwrap()["rt1"], vec!["dml", "cpu"]);
        assert!(has_gpu_provider(&all));
        // another driver: not this PC's probe any more
        let mut g2 = gpus.clone();
        g2[0].driver = "2.0".into();
        assert_eq!(profile_providers(&layout, &g2), (vec!["cpu".to_string()], None));
    }
}

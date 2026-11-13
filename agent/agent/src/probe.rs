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
    /// rev 11 (PROTOCOL 9.6): the GPU providers the pack's manifest says it brings (its `gpu` requirement); None: none
    /// declared.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub need: Option<Vec<String>>,
    /// rev 11: the job's own network was benched on this pack here, with the providers that passed the built-in probe
    /// and the pack's own (the built-in graph is not the last word: CUDA fails its priors, 2.4e-3, where real networks
    /// pass at 1e-5) ...
    #[serde(default)]
    pub benched: bool,
    /// ... and these GPU providers passed there.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub real: Vec<String>,
}

impl Probe {
    /// Providers that passed here: the built-in probe's and those that passed on a job's network.
    pub fn passed(&self) -> Vec<String> {
        ordered(self.providers.iter().chain(self.real.iter()).cloned())
    }
}

// ---------- 0.2.11: what a probe may load, where it runs, and probes the PC never finished ----------

/// The providers the built-in probe and the bench may load (`--providers`): all the worker knows but AMD's (`amdgpu`,
/// `migraphx`; profile::GPU_VENDORS_OFF). Every worker with `probe-providers` knows these names. (The AMD session's
/// diagnostic variants ignore `--providers` by design: the in-progress marker below is what protects there.)
pub const PROBE_PROVIDERS: [&str; 5] = ["tensorrt_rtx", "cuda", "openvino", "dml", "cpu"];

/// `--gpu <LUID>` for a probe or bench on a PC with adapters this agent never uses (`off`): one usable adapter
/// (NVIDIA first, then Intel, then any), so the worker doesn't enumerate the others. None: no adapter is off (the
/// worker probes every adapter, as before), or none is usable (nothing runs then).
pub fn pin_gpu(gpus: &[Gpu], off: &[Gpu]) -> Option<String> {
    if off.is_empty() {
        return None;
    }
    let rank = |g: &&Gpu| match g.vendor.as_str() {
        "nvidia" => 0,
        "intel" => 1,
        _ => 2,
    };
    gpus.iter().filter(|g| g.luid.is_some()).min_by_key(rank).and_then(|g| g.luid.clone())
}

/// A probe or bench running now (`state\probe-inflight.json`, written through to disk before its worker starts and
/// removed when it ends, whatever the outcome). Found at the next start, it means the agent or the PC stopped during
/// it - a GPU driver fault that took the PC down, a hang the user ended with a reset, an agent crash - and counts as
/// an unfinished probe of that pack on these adapters.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Inflight {
    /// "probe" (the built-in graph) or "bench" (a job's network)
    pub kind: String,
    pub runtime: String,
    pub adapters: String,
    pub worker: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<String>,
    pub t: i64,
}

/// The unfinished probes of one pack on these adapters (`state\unfinished.json`).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Unfinished {
    pub runtime: String,
    pub adapters: String,
    pub count: u32,
    /// when the last one was found (the next start)
    pub t: i64,
    pub kind: String,
    pub worker: String,
}

/// Unfinished this many times: never probed or benched again on these adapters and drivers (a driver update is a new
/// key: one more try). Once: not again for `RETRY_S`.
pub const UNFINISHED_NEVER: u32 = 2;

/// Whether Windows is logging off or shutting down (`SM_SHUTTINGDOWN`): a probe or bench running then is stopped and
/// its marker removed, so a shutdown is never taken for a fault.
#[cfg(windows)]
fn session_ending() -> bool {
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_SHUTTINGDOWN};
    // SAFETY: GetSystemMetrics has no preconditions.
    unsafe { GetSystemMetrics(SM_SHUTTINGDOWN) != 0 }
}

#[cfg(not(windows))]
fn session_ending() -> bool {
    false
}

fn inflight_path(layout: &Layout) -> PathBuf {
    layout.state().join("probe-inflight.json")
}

fn unfinished_path(layout: &Layout) -> PathBuf {
    layout.state().join("unfinished.json")
}

/// Writes the marker before a probe or bench starts. Err: it couldn't be written - the probe doesn't run then.
fn mark(layout: &Layout, m: &Inflight) -> Result<(), String> {
    zerofarm_common::fsutil::durable_write(&inflight_path(layout), &serde_json::to_vec(m).expect("json"))
        .map_err(|e| format!("could not write the probe marker ({e}); not probing"))
}

fn unmark(layout: &Layout) {
    let p = inflight_path(layout);
    for _ in 0..20 {
        match std::fs::remove_file(&p) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => std::thread::sleep(Duration::from_millis(50)),
            _ => return,
        }
    }
}

pub fn load_unfinished(layout: &Layout) -> Vec<Unfinished> {
    std::fs::read(unfinished_path(layout)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

/// At start: a marker the last run left means one more unfinished probe of its pack on its adapters (kept in
/// `unfinished.json`, at most 64 entries, newest first); returns the marker and that pack's count. The marker is gone
/// afterwards.
pub fn take_inflight(layout: &Layout, now: i64) -> Option<(Inflight, u32)> {
    let path = inflight_path(layout);
    let bytes = std::fs::read(&path).ok()?;
    let m: Option<Inflight> = serde_json::from_slice(&bytes).ok();
    let Some(m) = m else {
        unmark(layout);
        return None;
    };
    let mut v = load_unfinished(layout);
    let count = match v.iter_mut().find(|u| u.runtime == m.runtime && u.adapters == m.adapters) {
        Some(u) => {
            u.count += 1;
            u.t = now;
            u.kind = m.kind.clone();
            u.worker = m.worker.clone();
            u.count
        }
        None => {
            v.push(Unfinished { runtime: m.runtime.clone(), adapters: m.adapters.clone(), count: 1, t: now, kind: m.kind.clone(), worker: m.worker.clone() });
            1
        }
    };
    v.sort_by_key(|u| std::cmp::Reverse(u.t));
    v.truncate(64);
    if let Err(e) = zerofarm_common::fsutil::durable_write(&unfinished_path(layout), &serde_json::to_vec_pretty(&v).expect("json")) {
        // keep the marker: the next start counts it then
        log::warn!("could not record the unfinished probe ({e})");
        return Some((m, count));
    }
    unmark(layout);
    Some((m, count))
}

/// Whether a pack is held on these adapters: unfinished `UNFINISHED_NEVER` times, or once less than `RETRY_S` ago.
/// A held pack is neither probed nor benched, gets no GPU job (the CPU), the guard refuses it as an alternate, and
/// the profile lists it with no provider.
pub fn held(unfinished: &[Unfinished], runtime: &str, adapters: &str, now: i64) -> bool {
    unfinished
        .iter()
        .find(|u| u.runtime == runtime && u.adapters == adapters)
        .is_some_and(|u| u.count >= UNFINISHED_NEVER || now - u.t < RETRY_S)
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
    let mut p = p;
    // a real network's verdict for the same pack and adapters outlives a re-probe of the built-in graph (9.6)
    if let Some(old) = v.iter().find(|x| x.runtime == p.runtime && x.adapters == p.adapters) {
        p.benched |= old.benched;
        for r in &old.real {
            if !p.real.contains(r) {
                p.real.push(r.clone());
            }
        }
        if p.need.is_none() {
            p.need = old.need.clone();
        }
    }
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

/// Provider names in the order of preference (`ORDER`), the CPU last (and always there).
pub fn ordered(set: impl IntoIterator<Item = String>) -> Vec<String> {
    let set: Vec<String> = set.into_iter().collect();
    let mut out: Vec<String> = ORDER.iter().filter(|p| set.iter().any(|s| s == *p)).map(|s| s.to_string()).collect();
    if !out.iter().any(|p| p == "cpu") {
        out.push("cpu".into());
    }
    out
}

/// The profile's `providers` (any runtime pack, these adapters) and `runtime_providers`. Rev 11 (PROTOCOL 3, 9.4):
/// a pack whose probe failed less than a day ago is listed with no provider (`[]`), so the coordinator doesn't
/// offer it again (it would be refused here, 9.5); after a day it is left out, and may be tried once more.
pub fn profile_providers(layout: &Layout, gpus: &[Gpu]) -> (Vec<String>, Option<BTreeMap<String, Vec<String>>>) {
    let key = adapters_key(gpus);
    let now = zerofarm_common::now_unix();
    let mine: Vec<Probe> = load(layout).into_iter().filter(|p| p.adapters == key).collect();
    let probes: Vec<&Probe> = mine.iter().filter(|p| p.error.is_none()).collect();
    let all = ordered(probes.iter().flat_map(|p| p.passed()));
    let failed: Vec<&Probe> = mine.iter().filter(|p| p.error.is_some() && now - p.t < RETRY_S).collect();
    // 0.2.11: packs the PC stopped during (agent or PC down mid-probe) count as failed, and their providers don't
    let unfinished = load_unfinished(layout);
    let held_here: Vec<&Unfinished> = unfinished.iter().filter(|u| u.adapters == key && held(&unfinished, &u.runtime, &key, now)).collect();
    let probes: Vec<&Probe> = probes.into_iter().filter(|p| !held_here.iter().any(|u| u.runtime == p.runtime)).collect();
    let all = if held_here.is_empty() { all } else { ordered(probes.iter().flat_map(|p| p.passed())) };
    if probes.is_empty() && failed.is_empty() && held_here.is_empty() {
        return (all, None);
    }
    let mut per: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for p in probes.iter().take(16) {
        // a GPU pack whose own providers failed the built-in probe but were never tried on a real network here is
        // unknown, not failed (left out, so the coordinator may still offer it: the first lease benches it, 9.6)
        if p.need.is_some() && !p.benched && pack_bad_probe(p, 0) == Some(true) {
            continue;
        }
        per.insert(p.runtime.clone(), p.passed());
    }
    for p in failed {
        if per.len() < 16 && !per.contains_key(&p.runtime) {
            per.insert(p.runtime.clone(), vec![]);
        }
    }
    for u in held_here {
        if per.len() < 16 && !per.contains_key(&u.runtime) {
            per.insert(u.runtime.clone(), vec![]);
        }
    }
    (all, Some(per))
}

/// Rev 11 (PROTOCOL 9.5): the pack failed here for these adapters and drivers - a probe in which none of the
/// GPU providers it brings passed (`need`: its manifest's; None: any GPU provider), or a probe that failed less
/// than a day ago. Such a pack is not downloaded again.
pub fn pack_bad(probes: &[Probe], runtime: &str, adapters: &str, need: Option<&[String]>, now: i64) -> bool {
    let Some(p) = find(probes, runtime, adapters) else { return false };
    if p.error.is_some() {
        return now - p.t < RETRY_S;
    }
    let pays = p.passed().iter().any(|x| x != "cpu" && need.is_none_or(|n| n.iter().any(|y| y == x)));
    // without a real network's verdict a failed built-in probe doesn't condemn a pack (PROTOCOL 9.6)
    !pays && p.benched
}

/// Whether a probe alone (no bench yet) failed every GPU provider of its pack: Some(true) failed, Some(false) one
/// passed, None it crashed. `now` 0: ignore the retry window.
fn pack_bad_probe(p: &Probe, _now: i64) -> Option<bool> {
    if p.error.is_some() {
        return None;
    }
    let need = p.need.as_deref();
    Some(!p.passed().iter().any(|x| x != "cpu" && need.is_none_or(|n| n.iter().any(|y| y == x))))
}

/// Records a bench's verdict in the pack's probe (PROTOCOL 9.6): benched, and the GPU providers that passed on the
/// job's network. No probe for these adapters: nothing (the bench only follows a probe).
pub fn record_real(layout: &Layout, runtime: &str, adapters: &str, need: Option<&[String]>, passed: &[String]) {
    let mut v = load(layout);
    if let Some(p) = v.iter_mut().find(|p| p.runtime == runtime && p.adapters == adapters) {
        p.benched = true;
        if need.is_some() {
            p.need = need.map(|n| n.to_vec());
        }
        for x in passed.iter().filter(|x| *x != "cpu") {
            if !p.real.contains(x) {
                p.real.push(x.clone());
            }
        }
        save(layout, &v);
    }
}

// ---------- rev 11 (PROTOCOL 9.6): benches of the job's own network, and the measured choice ----------

/// Batch sizes a bench measures.
pub const BENCH_ROWS: [u64; 6] = [1, 8, 32, 128, 512, 1024];
/// A bench is measured again after this long.
pub const BENCH_MAX_AGE_S: i64 = 7 * 24 * 3600;
/// A GPU provider runs only if it is at least this many times the CPU at the job's batch size.
pub const GPU_MARGIN: f64 = 1.1;
/// A GPU job's `conc` is raised to at least this (never above its games).
pub const GPU_CONC: u64 = 512;

/// One (provider, adapter) of a bench: passed its self-test, rows per second by batch rows.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct BenchRow {
    pub provider: String,
    #[serde(default)]
    pub adapter: Option<String>,
    pub ok: bool,
    /// {"<rows>": rows per second}
    #[serde(default)]
    pub rows: BTreeMap<String, f64>,
}

/// A bench of one run's network on one runtime pack, on these adapters (PROTOCOL 9.6).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Bench {
    pub runtime: String,
    pub adapters: String,
    pub run: String,
    pub worker: String,
    pub t: i64,
    pub threads: u32,
    #[serde(default)]
    pub results: Vec<BenchRow>,
    #[serde(default)]
    pub error: Option<String>,
    /// The fewest games per lease at which this PC's best GPU provider pays for `run` (PROTOCOL 9.4: `caps.gpu_games`),
    /// worked out by [`choose`] with the job's `groups`; None: no GPU provider pays at any size.
    #[serde(default)]
    pub gpu_games: Option<u64>,
}

fn bench_path(layout: &Layout) -> PathBuf {
    layout.state().join("bench.json")
}

pub fn load_benches(layout: &Layout) -> Vec<Bench> {
    std::fs::read(bench_path(layout)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

/// Records `b`, replacing the bench of the same (runtime, adapters, run); at most 32 kept (newest).
pub fn record_bench(layout: &Layout, b: Bench) {
    let mut v = load_benches(layout);
    v.retain(|x| !(x.runtime == b.runtime && x.adapters == b.adapters && x.run == b.run));
    v.push(b);
    v.sort_by_key(|x| std::cmp::Reverse(x.t));
    v.truncate(32);
    let _ = std::fs::create_dir_all(layout.state());
    let _ = zerofarm_common::fsutil::atomic_write(&bench_path(layout), &serde_json::to_vec_pretty(&v).expect("json"));
}

/// A fresh bench of `run`'s network on `runtime` for these adapters: one that ran, for 7 days; one that failed to
/// run, for a day (it isn't retried every lease; `choose` takes it as no bench: the CPU).
pub fn find_bench<'a>(benches: &'a [Bench], runtime: &str, adapters: &str, run: &str, now: i64) -> Option<&'a Bench> {
    benches.iter().find(|b| {
        b.runtime == runtime && b.adapters == adapters && b.run == run && now - b.t < if b.error.is_none() { BENCH_MAX_AGE_S } else { RETRY_S }
    })
}

/// The worker's JSON report of `probe-providers --model --bench-rows` -> a `Bench`.
pub fn bench_from_report(v: &Value, runtime: &str, adapters: &str, run: &str, worker: &str, threads: u32, t: i64) -> Bench {
    let mut results = Vec::new();
    for r in v["results"].as_array().into_iter().flatten() {
        let rows = r["rows_per_s_by_batch"].as_object().map(|m| m.iter().filter_map(|(k, x)| Some((k.clone(), x.as_f64()?))).collect()).unwrap_or_default();
        results.push(BenchRow {
            provider: r["provider"].as_str().unwrap_or("").to_string(),
            adapter: r["gpu"]["name"].as_str().map(String::from),
            ok: r["ok"].as_bool().unwrap_or(false),
            rows,
        });
    }
    Bench { runtime: runtime.into(), adapters: adapters.into(), run: run.into(), worker: worker.into(), t, threads, results, error: v["error"].as_str().map(String::from), gpu_games: None }
}

/// Rows per second at `b` rows from a bench's sizes: log-log interpolation between the two nearest sizes, the
/// end values beyond them.
pub fn rate_at(rows: &BTreeMap<String, f64>, b: u64) -> Option<f64> {
    let mut pts: Vec<(f64, f64)> = rows.iter().filter_map(|(k, v)| Some((k.parse::<f64>().ok()?, *v))).filter(|(n, v)| *n >= 1.0 && *v > 0.0).collect();
    pts.sort_by(|a, b| a.0.total_cmp(&b.0));
    let (first, last) = (*pts.first()?, *pts.last()?);
    let b = (b.max(1)) as f64;
    if let Some(p) = pts.iter().find(|p| p.0 == b) {
        return Some(p.1);
    }
    if b <= first.0 {
        return Some(first.1);
    }
    if b >= last.0 {
        return Some(last.1);
    }
    let i = pts.iter().position(|p| p.0 >= b)?;
    let (lo, hi) = (pts[i - 1], pts[i]);
    let f = (b.ln() - lo.0.ln()) / (hi.0.ln() - lo.0.ln());
    Some((lo.1.ln() + f * (hi.1.ln() - lo.1.ln())).exp())
}

/// Rows per batch of a job: one leaf per game in flight per batch, `groups` batches in turn.
pub fn batch_rows(conc: u64, games: u64, groups: u64) -> u64 {
    conc.min(games).max(1).div_ceil(groups.max(1))
}

/// What a job line gets (PROTOCOL 9.6).
#[derive(Debug, Clone, PartialEq)]
pub struct Choice {
    pub providers: Vec<String>,
    pub gpu: Value,
    /// The raised `conc` of a GPU job (None: the template's).
    pub conc: Option<u64>,
    /// The `groups` of a GPU job: at most 2, so batches are big (None: the template's).
    pub groups: Option<u64>,
    /// The fewest games per lease at which the best GPU pays here (Bench `gpu_games`, `caps.gpu_games`).
    pub gpu_games: Option<u64>,
    /// Rows per batch as chosen (Stats `batch_rows`).
    pub batch_rows: u64,
    /// Rows per second at the job's batch size per provider (Stats `rows_per_s`): the CPU and each GPU candidate.
    pub rows_per_s: BTreeMap<String, f64>,
    pub why: String,
}

/// The measured choice (PROTOCOL 9.6), the same for every provider and vendor: at the job's batch size the
/// fastest provider whose self-test passed, a GPU one only at `GPU_MARGIN` times the CPU or better; the CPU when
/// the GPU may not be used, there is no bench, or the bench has no CPU baseline.
pub fn choose(bench: Option<&Bench>, gpus: &[Gpu], allow: Option<&[String]>, gpu_ok: bool, games: u64, conc: u64, groups: u64) -> Choice {
    let cpu_b = batch_rows(conc, games, groups);
    let gpu_conc = games.min(conc.max(GPU_CONC)).max(1);
    let gpu_groups = groups.clamp(1, 2);
    let gpu_b = batch_rows(gpu_conc, games, gpu_groups);
    let mut c = Choice {
        providers: vec!["cpu".into()],
        gpu: Value::Null,
        conc: None,
        groups: None,
        gpu_games: None,
        batch_rows: cpu_b,
        rows_per_s: BTreeMap::new(),
        why: String::new(),
    };
    let Some(b) = bench.filter(|b| b.error.is_none()) else {
        c.why = "no bench of this network on this pack".into();
        return c;
    };
    let Some(cpu) = b.results.iter().find(|r| r.ok && r.provider == "cpu").and_then(|r| rate_at(&r.rows, cpu_b)) else {
        c.why = "the bench has no CPU baseline".into();
        return c;
    };
    c.rows_per_s.insert("cpu".into(), cpu.round());
    // (0.2.11: never a provider a probe may not load - AMD's - even from a bench an older agent kept)
    let allowed = |p: &str| p != "cpu" && PROBE_PROVIDERS.contains(&p) && allow.is_none_or(|a| a.iter().any(|x| x == p));
    let mut best: Option<(&BenchRow, &Gpu, f64)> = None;
    for r in b.results.iter().filter(|r| r.ok && allowed(&r.provider)) {
        let Some(g) = gpus.iter().find(|g| Some(g.name.as_str()) == r.adapter.as_deref()) else { continue };
        let Some(rate) = rate_at(&r.rows, gpu_b) else { continue };
        let e = c.rows_per_s.entry(r.provider.clone()).or_insert(0.0);
        *e = e.max(rate.round());
        if best.is_none_or(|(_, _, x)| rate > x) {
            best = Some((r, g, rate));
        }
    }
    // The fewest games per lease at which a GPU pays (PROTOCOL 9.4): the smallest batch where one reaches 2x the CPU,
    // else GPU_MARGIN times, with `gpu_groups` batches in turn.
    let cpu_rows = &b.results.iter().find(|r| r.ok && r.provider == "cpu").expect("checked above").rows;
    let gpu_rows: Vec<&BenchRow> =
        b.results.iter().filter(|r| r.ok && allowed(&r.provider) && gpus.iter().any(|g| Some(g.name.as_str()) == r.adapter.as_deref())).collect();
    let pays_at = |ratio: f64| {
        let mut n = 1u64;
        while n <= 1024 {
            let cpu_n = rate_at(cpu_rows, n).unwrap_or(cpu);
            if gpu_rows.iter().any(|r| rate_at(&r.rows, n).is_some_and(|x| x >= ratio * cpu_n)) {
                return Some(n);
            }
            n = (n * 5).div_ceil(4);
        }
        None
    };
    c.gpu_games = pays_at(2.0).or_else(|| pays_at(GPU_MARGIN)).map(|n| n * gpu_groups);
    if !gpu_ok {
        c.why = "the GPU may not be used now".into();
        return c;
    }
    match best {
        Some((r, g, rate)) if rate >= GPU_MARGIN * cpu => {
            c.providers = vec![r.provider.clone(), "cpu".into()];
            c.gpu = json!({"luid": g.luid.clone().unwrap_or_default(), "vendor": g.vendor, "name": g.name});
            c.conc = Some(gpu_conc);
            c.groups = (gpu_groups != groups).then_some(gpu_groups);
            c.batch_rows = gpu_b;
            c.why = format!("{} on {}: {:.0} rows/s at {gpu_b} rows, the CPU {:.0} at {cpu_b}", r.provider, g.name, rate, cpu);
        }
        Some((r, g, rate)) => {
            c.why = format!("the CPU: {:.0} rows/s at {cpu_b} rows; the best GPU, {} on {}, {:.0} at {gpu_b} (needs {GPU_MARGIN}x)", cpu, r.provider, g.name, rate);
        }
        None => c.why = "no GPU provider passed on this pack".into(),
    }
    c
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
    let unfinished = load_unfinished(layout);
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
        // 0.2.11: the PC stopped during this pack's probe or bench before (twice: never again for these drivers)
        if held(&unfinished, &rt, &key, now) {
            continue;
        }
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
        need: None,
        benched: false,
        real: vec![],
    }
}

/// Runs one probe (in a job object, below-normal priority, the runtime pack first on the DLL path),
/// at most `timeout`; the readable report goes to the worker log.
#[allow(clippy::too_many_arguments)]
pub fn run(
    layout: &Layout,
    entry: &Path,
    worker: &str,
    runtime: &str,
    rdir: &Path,
    gpus: &[Gpu],
    off: &[Gpu],
    log: Arc<RotatingFile>,
    timeout: Duration,
    stop: &dyn Fn() -> bool,
) -> Probe {
    let now = zerofarm_common::now_unix();
    let akey = adapters_key(gpus);
    let rdir_s = rdir.to_string_lossy().to_string();
    let provs = PROBE_PROVIDERS.join(",");
    let pin = pin_gpu(gpus, off);
    let mut args = vec!["probe-providers", "--runtime-dir", rdir_s.as_str(), "--ensure-ready", "--threads", "2", "--reps", "2"];
    args.extend(["--providers", provs.as_str()]);
    if let Some(l) = &pin {
        args.extend(["--gpu", l.as_str()]);
    }
    let failed = |e: String| Probe { runtime: runtime.into(), adapters: akey.clone(), worker: worker.into(), t: now, providers: vec!["cpu".into()], results: vec![], error: Some(e), need: None, benched: false, real: vec![] };
    let m = Inflight { kind: "probe".into(), runtime: runtime.into(), adapters: akey.clone(), worker: worker.into(), run: None, t: now };
    if let Err(e) = mark(layout, &m) {
        return failed(e);
    }
    let r = run_worker(layout, "probe", entry, &args, rdir, log, timeout, stop);
    unmark(layout);
    match r {
        Ok(v) => from_report(&v, runtime, &akey, worker, now),
        Err(e) => failed(e),
    }
}

/// Runs `<entry> <args> --json <file>` in `work\<dir>` (a job object, the runtime pack first on the DLL path), at
/// most `timeout` -> the JSON report; the readable report goes to the worker log. Err("stopped") when `stop`.
#[allow(clippy::too_many_arguments)]
fn run_worker(layout: &Layout, dir: &str, entry: &Path, args: &[&str], rdir: &Path, log: Arc<RotatingFile>, timeout: Duration, stop: &dyn Fn() -> bool) -> Result<Value, String> {
    let work = layout.work().join(dir);
    let _ = zerofarm_common::fsutil::remove_dir_all_retry(&work, Duration::from_secs(5));
    std::fs::create_dir_all(&work).map_err(|e| format!("work dir: {e}"))?;
    let out = work.join("probe.json");
    let out_s = out.to_string_lossy().to_string();
    let mut all: Vec<&str> = args.to_vec();
    all.extend(["--json", out_s.as_str()]);
    let t0 = Instant::now();
    let spawn = WorkerProc::spawn(
        SpawnOpts { exe: entry, args: &all, cwd: &work, mem_limit_mb: 4096, cpu_job: false, log, tag: dir.into(), runtime_dir: Some(rdir) },
        |_m: WorkerMsg| {},
    );
    let mut p = spawn.map_err(|e| format!("could not start the probe: {e}"))?;
    let code = loop {
        if let Some(c) = p.try_wait() {
            break Some(c);
        }
        if t0.elapsed() > timeout || stop() || session_ending() {
            p.kill();
            break None;
        }
        std::thread::sleep(Duration::from_millis(200));
    };
    let r = match code {
        // (0.2.11: Windows logging off or shutting down ends a probe like the agent quitting: not an unfinished one)
        None if stop() || session_ending() => Err("stopped".to_string()),
        None => Err(format!("no answer within {} s", timeout.as_secs())),
        Some(c) => match std::fs::read(&out).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok()) {
            Some(v) => Ok(v),
            // (a worker build without the command answers usage and 2: never tried again with it)
            None => Err(format!("the probe exited {c} without a report")),
        },
    };
    let _ = zerofarm_common::fsutil::remove_dir_all_retry(&work, Duration::from_secs(5));
    r
}

/// What a bench measures (PROTOCOL 9.6): the job's own network, with the job's threads.
pub struct BenchSpec<'a> {
    pub model: &'a Path,
    pub reference: &'a Path,
    pub run: &'a str,
    pub threads: u32,
    /// The providers to bench: those that passed the pack's (built-in) probe here.
    pub providers: Vec<String>,
}

/// Benches the job's network on a pack (PROTOCOL 9.6): `probe-providers --model --reference --bench-rows ...
/// --providers <those that passed the pack's built-in probe>`, a record of its own next to that probe (which stays
/// the built-in one). `Err("stopped")` when the agent is quitting (nothing to record).
#[allow(clippy::too_many_arguments)]
pub fn run_bench(
    layout: &Layout,
    entry: &Path,
    worker: &str,
    runtime: &str,
    rdir: &Path,
    gpus: &[Gpu],
    off: &[Gpu],
    log: Arc<RotatingFile>,
    timeout: Duration,
    stop: &dyn Fn() -> bool,
    spec: &BenchSpec,
) -> Result<Bench, String> {
    let now = zerofarm_common::now_unix();
    let akey = adapters_key(gpus);
    let (rdir_s, model, reference, threads) =
        (rdir.to_string_lossy().to_string(), spec.model.to_string_lossy().to_string(), spec.reference.to_string_lossy().to_string(), spec.threads.max(1).to_string());
    let rows = BENCH_ROWS.iter().map(|n| n.to_string()).collect::<Vec<_>>().join(",");
    // 0.2.11: never AMD's providers (PROBE_PROVIDERS), and the CPU always (the baseline)
    let provs = ordered(spec.providers.iter().filter(|p| PROBE_PROVIDERS.contains(&p.as_str())).cloned()).join(",");
    let pin = pin_gpu(gpus, off);
    let mut args = vec!["probe-providers", "--runtime-dir", rdir_s.as_str(), "--model", model.as_str(), "--reference", reference.as_str()];
    args.extend(["--threads", threads.as_str(), "--reps", "3", "--bench-rows", rows.as_str(), "--providers", provs.as_str()]);
    if let Some(l) = &pin {
        args.extend(["--gpu", l.as_str()]);
    }
    let failed = |e: String| Bench { runtime: runtime.into(), adapters: akey.clone(), run: spec.run.into(), worker: worker.into(), t: now, threads: spec.threads, results: vec![], error: Some(e), gpu_games: None };
    let m = Inflight { kind: "bench".into(), runtime: runtime.into(), adapters: akey.clone(), worker: worker.into(), run: Some(spec.run.into()), t: now };
    if let Err(e) = mark(layout, &m) {
        return Ok(failed(e));
    }
    let r = run_worker(layout, "bench", entry, &args, rdir, log, timeout, stop);
    unmark(layout);
    match r {
        Ok(v) => Ok(bench_from_report(&v, runtime, &akey, spec.run, worker, spec.threads, now)),
        Err(e) if e == "stopped" => Err(e),
        Err(e) => Ok(failed(e)),
    }
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
            need: None,
            benched: false,
            real: vec![],
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
        let p = run(&layout, Path::new(&exe), "w", "rt", Path::new(&rt), &gpus, &[], log, Duration::from_secs(600), &|| false);
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

    fn bench(rows: &[(&str, Option<&str>, &[(u64, f64)])]) -> Bench {
        Bench {
            runtime: "rt".into(),
            adapters: "a".into(),
            run: "r".into(),
            worker: "w".into(),
            t: 0,
            threads: 4,
            results: rows
                .iter()
                .map(|(p, a, pts)| BenchRow { provider: p.to_string(), adapter: a.map(String::from), ok: true, rows: pts.iter().map(|(n, v)| (n.to_string(), *v)).collect() })
                .collect(),
            error: None,
            gpu_games: None,
        }
    }

    // The canary's measured numbers (phase 4: RTX 4070 Ti, 4 cores): CPU flat ~2.2-2.6k rows/s; CUDA 12k at 64 rows,
    // 24k at 256+; DirectML 0.9k / 2.5k / 4.4k at 64 / 256 / 1,024.
    const CPU: &[(u64, f64)] = &[(1, 2400.0), (8, 2500.0), (32, 2600.0), (128, 2400.0), (512, 2200.0), (1024, 2200.0)];
    const CUDA: &[(u64, f64)] = &[(1, 300.0), (8, 2400.0), (32, 7000.0), (128, 16000.0), (512, 24000.0), (1024, 24000.0)];
    const DML: &[(u64, f64)] = &[(1, 30.0), (8, 150.0), (32, 450.0), (128, 1500.0), (512, 3400.0), (1024, 4400.0)];

    #[test]
    fn interpolation_and_batch_rows() {
        let m: BTreeMap<String, f64> = CUDA.iter().map(|(n, v)| (n.to_string(), *v)).collect();
        assert_eq!(rate_at(&m, 128), Some(16000.0));
        assert_eq!(rate_at(&m, 4096), Some(24000.0), "clamped above");
        assert_eq!(rate_at(&m, 0), Some(300.0), "clamped below");
        let mid = rate_at(&m, 64).unwrap();
        assert!(mid > 7000.0 && mid < 16000.0, "{mid}");
        assert_eq!(rate_at(&BTreeMap::new(), 8), None);
        assert_eq!(batch_rows(512, 128, 2), 64);
        assert_eq!(batch_rows(1536, 6, 1), 6, "never more rows than games in flight");
        assert_eq!(batch_rows(8, 100, 4), 2);
        assert_eq!(batch_rows(0, 0, 0), 1);
    }

    #[test]
    fn the_choice_is_measured_and_vendor_neutral() {
        let gpus = [gpu("RTX 4070 Ti", "nvidia", "00000000000000d1"), gpu("RX 9070 XT", "amd", "00000000000000a1")];
        let b = bench(&[("cpu", None, CPU), ("cuda", Some("RTX 4070 Ti"), CUDA), ("dml", Some("RTX 4070 Ti"), DML)]);
        // A 6-game lease (the Delver run's): batches of at most 6 rows - the CPU, whatever the GPU.
        let c = choose(Some(&b), &gpus, None, true, 6, 1536, 1);
        assert_eq!((c.providers.clone(), c.conc, c.batch_rows), (vec!["cpu".to_string()], None, 6), "{}", c.why);
        assert!(c.rows_per_s["cuda"] < c.rows_per_s["cpu"], "{:?}", c.rows_per_s);
        // A 128-game eval lease, conc 16, 2 groups: the GPU candidate runs at conc 128 -> 64-row batches.
        let c = choose(Some(&b), &gpus, None, true, 128, 16, 2);
        assert_eq!(c.providers, vec!["cuda", "cpu"], "{}", c.why);
        assert_eq!((c.conc, c.batch_rows, c.groups), (Some(128), 64, None));
        // CUDA reaches 2x the CPU (5,000 rows/s) between 8 and 32 rows: with 2 groups, ~2x that many games pay
        let g = c.gpu_games.unwrap();
        assert!((16..=64).contains(&g), "{g}");
        // the Delver run's 4 groups: a GPU job runs 2 (bigger batches); 24 games -> 12-row batches
        let c = choose(Some(&b), &gpus, None, true, 24, 1536, 4);
        assert_eq!((c.providers.clone(), c.groups, c.batch_rows), (vec!["cuda".to_string(), "cpu".to_string()], Some(2), 12), "{}", c.why);
        assert_eq!(c.gpu["luid"], "00000000000000d1");
        assert!(c.rows_per_s.contains_key("dml") && c.rows_per_s.contains_key("cpu"));
        // DirectML alone (the AMD card's provider here) at the same lease: below the CPU at 64 rows -> the CPU.
        let amd = bench(&[("cpu", None, CPU), ("dml", Some("RX 9070 XT"), DML)]);
        assert_eq!(choose(Some(&amd), &gpus, None, true, 128, 16, 2).providers, vec!["cpu"]);
        // ... but a big lease: 1,024-row batches, DirectML at 2x the CPU -> DirectML (no vendor or provider is special).
        let c = choose(Some(&amd), &gpus, None, true, 4096, 2048, 2);
        assert_eq!((c.providers.clone(), c.gpu["name"].as_str()), (vec!["dml".to_string(), "cpu".to_string()], Some("RX 9070 XT")), "{}", c.why);
        // An allowlist without cuda; the GPU not allowed now; no bench; a bench without a CPU baseline.
        let only_dml = vec!["dml".to_string(), "cpu".to_string()];
        assert_eq!(choose(Some(&b), &gpus, Some(&only_dml), true, 128, 16, 2).providers, vec!["cpu"]);
        let c = choose(Some(&b), &gpus, None, false, 128, 16, 2);
        assert_eq!((c.providers.clone(), c.gpu.clone()), (vec!["cpu".to_string()], Value::Null));
        assert!(c.rows_per_s.contains_key("cuda"), "measured rates are still reported");
        assert_eq!(choose(None, &gpus, None, true, 128, 16, 2).providers, vec!["cpu"]);
        let no_cpu = bench(&[("cuda", Some("RTX 4070 Ti"), CUDA)]);
        assert_eq!(choose(Some(&no_cpu), &gpus, None, true, 128, 16, 2).providers, vec!["cpu"]);
        // a provider that only just beats the CPU (< 1.1x) doesn't run
        let close = bench(&[("cpu", None, &[(64, 1000.0)]), ("cuda", Some("RTX 4070 Ti"), &[(64, 1050.0)])]);
        assert_eq!(choose(Some(&close), &gpus, None, true, 128, 16, 2).providers, vec!["cpu"]);
        // an adapter this PC doesn't have (any more) is never chosen
        let gone = bench(&[("cpu", None, CPU), ("cuda", Some("RTX 3090"), CUDA)]);
        assert_eq!(choose(Some(&gone), &gpus, None, true, 128, 16, 2).providers, vec!["cpu"]);
    }

    fn probe_rec(runtime: &str, key: &str, t: i64, providers: &[&str], error: Option<&str>, need: Option<&[&str]>) -> Probe {
        Probe {
            runtime: runtime.into(),
            adapters: key.into(),
            worker: "w".into(),
            t,
            providers: providers.iter().map(|x| x.to_string()).collect(),
            results: vec![],
            error: error.map(String::from),
            need: need.map(|n| n.iter().map(|x| x.to_string()).collect()),
            benched: false,
            real: vec![],
        }
    }

    #[test]
    fn failed_packs_are_marked_bad_only_with_a_real_networks_verdict() {
        let d = tempfile::tempdir().unwrap();
        let layout = Layout::new(d.path());
        let gpus = [gpu("RTX 4070 Ti", "nvidia", "000000000000d1f3")];
        let key = adapters_key(&gpus);
        let now = zerofarm_common::now_unix();
        let cuda = vec!["cuda".to_string()];
        // never probed: not bad
        assert!(!pack_bad(&load(&layout), "cuda-pack", &key, Some(&cuda), now));
        // the built-in graph failed cuda (the CUDA false negative): not bad yet, and unknown to the coordinator
        record(&layout, probe_rec("cuda-pack", &key, now, &["cpu"], None, Some(&["cuda"])));
        assert!(!pack_bad(&load(&layout), "cuda-pack", &key, Some(&cuda), now));
        assert!(profile_providers(&layout, &gpus).1.is_none_or(|m| !m.contains_key("cuda-pack")), "left out: unknown");
        // the job's network passed cuda: it counts, the pack is listed with it, a re-probe keeps that verdict
        record_real(&layout, "cuda-pack", &key, Some(&cuda), &cuda);
        assert!(!pack_bad(&load(&layout), "cuda-pack", &key, Some(&cuda), now));
        assert_eq!(profile_providers(&layout, &gpus).1.unwrap()["cuda-pack"], vec!["cuda", "cpu"]);
        record(&layout, probe_rec("cuda-pack", &key, now + 1, &["cpu"], None, None));
        let p = find(&load(&layout), "cuda-pack", &key).cloned().unwrap();
        assert!(p.benched && p.real == cuda && p.need.as_deref() == Some(&cuda[..]), "{p:?}");
        // a pack that failed both: bad for this driver, listed without a GPU provider
        record(&layout, probe_rec("cuda-2", &key, now, &["cpu"], None, Some(&["cuda"])));
        record_real(&layout, "cuda-2", &key, Some(&cuda), &[]);
        assert!(pack_bad(&load(&layout), "cuda-2", &key, Some(&cuda), now));
        assert_eq!(profile_providers(&layout, &gpus).1.unwrap()["cuda-2"], vec!["cpu"]);
        // another driver: a fresh start
        let mut g2 = gpus.clone();
        g2[0].driver = "2.0".into();
        assert!(!pack_bad(&load(&layout), "cuda-2", &adapters_key(&g2), Some(&cuda), now));
        // a probe that crashed: bad for a day, listed as [] meanwhile, then left out
        record(&layout, probe_rec("p2", &key, now - 10, &["cpu"], Some("crashed"), None));
        assert!(pack_bad(&load(&layout), "p2", &key, None, now));
        assert_eq!(profile_providers(&layout, &gpus).1.unwrap()["p2"], Vec::<String>::new());
        assert!(!pack_bad(&load(&layout), "p2", &key, None, now + RETRY_S));
        // a pack without a declared requirement (the Windows ML pack): its built-in probe is listed as it is
        record(&layout, probe_rec("winml", &key, now, &["dml", "cpu"], None, None));
        assert_eq!(profile_providers(&layout, &gpus).1.unwrap()["winml"], vec!["dml", "cpu"]);
    }

    #[test]
    fn only_a_passed_self_test_counts_never_rows_per_second() {
        // a bench row whose self-test failed (ok false) is never chosen, however fast, nor recorded as a pass
        let gpus = [gpu("RTX 4070 Ti", "nvidia", "00000000000000d1")];
        let mut b = bench(&[("cpu", None, CPU), ("cuda", Some("RTX 4070 Ti"), CUDA)]);
        b.results[1].ok = false;
        b.results[1].rows = [("1".to_string(), 1e9), ("1024".to_string(), 1e9)].into_iter().collect();
        let c = choose(Some(&b), &gpus, None, true, 1024, 1024, 1);
        assert_eq!(c.providers, vec!["cpu"]);
        assert!(!c.rows_per_s.contains_key("cuda"));
        assert_eq!(c.gpu_games, None);
        // the report's own `ok` (the factory's self-test) is what a bench row carries
        let report = json!({"results": [{"provider": "cuda", "gpu": {"name": "RTX 4070 Ti"}, "ok": false,
            "selftest": {"max_abs": 2.4e-3, "ok": false}, "rows_per_s_by_batch": {"1024": 1e9}}]});
        assert!(!bench_from_report(&report, "rt", "a", "r", "w", 4, 0).results[0].ok);
    }

    #[test]
    fn bench_reports_and_storage() {
        let d = tempfile::tempdir().unwrap();
        let layout = Layout::new(d.path());
        let report = json!({"providers": ["cuda", "cpu"], "error": null, "results": [
            {"provider": "cpu", "gpu": null, "ok": true, "rows_per_s_by_batch": {"1": 2000.0, "1024": 2200.0}},
            {"provider": "cuda", "gpu": {"name": "RTX 4070 Ti", "luid": "d1"}, "ok": true, "rows_per_s_by_batch": {"1": 300.0, "1024": 24000.0}},
            {"provider": "dml", "gpu": {"name": "RTX 4070 Ti", "luid": "d1"}, "ok": false, "why": "x"}]});
        let b = bench_from_report(&report, "rt", "a", "run1", "w", 4, 100);
        assert_eq!(b.results.len(), 3);
        assert_eq!(b.results[1].rows["1024"], 24000.0);
        record_bench(&layout, b.clone());
        let now = 100 + 60;
        assert!(find_bench(&load_benches(&layout), "rt", "a", "run1", now).is_some());
        assert!(find_bench(&load_benches(&layout), "rt", "a", "run2", now).is_none(), "per run");
        assert!(find_bench(&load_benches(&layout), "rt", "a", "run1", 100 + BENCH_MAX_AGE_S).is_none(), "stale after a week");
        // a bench that failed to run is kept a day (no re-run every lease) and chooses the CPU; a provider that failed
        // its self-test there stays failed for that (pack, adapters, run) until the bench is a week old
        let mut bad = b.clone();
        bad.run = "run3".into();
        bad.error = Some("crashed".into());
        record_bench(&layout, bad);
        let found = find_bench(&load_benches(&layout), "rt", "a", "run3", now).cloned();
        assert!(found.as_ref().is_some_and(|x| x.error.is_some()));
        assert_eq!(choose(found.as_ref(), &[], None, true, 128, 16, 2).providers, vec!["cpu"]);
        assert!(find_bench(&load_benches(&layout), "rt", "a", "run3", 100 + RETRY_S).is_none(), "tried again after a day");
        let all = load_benches(&layout);
        let dml_failed = find_bench(&all, "rt", "a", "run1", now).unwrap();
        assert!(!dml_failed.results.iter().find(|r| r.provider == "dml").unwrap().ok);
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

    // ---- 0.2.11 ----

    #[test]
    fn an_unfinished_probe_holds_its_pack_a_day_then_forever() {
        let d = tempfile::tempdir().unwrap();
        let layout = Layout::new(d.path());
        let gpus = [gpu("RTX 4070 Ti", "nvidia", "000000000000d1f3")];
        let key = adapters_key(&gpus);
        // a probe that passed earlier, then a later one the PC never finished
        let report = json!({"providers": ["cuda", "cpu"], "error": null, "results": [
            {"provider": "cpu", "gpu": null, "ok": true},
            {"provider": "cuda", "gpu": {"name": "RTX 4070 Ti", "luid": "000000000000d1f3"}, "ok": true}]});
        // (profile_providers reads the clock: the unfinished probe is found "now")
        let now = zerofarm_common::now_unix();
        record(&layout, from_report(&report, "rt1", &key, "w1", now - 200));
        assert!(take_inflight(&layout, now).is_none(), "no marker: nothing");
        let m = Inflight { kind: "probe".into(), runtime: "rt1".into(), adapters: key.clone(), worker: "w1".into(), run: None, t: now - 100 };
        mark(&layout, &m).unwrap();
        let (got, n) = take_inflight(&layout, now).unwrap();
        assert_eq!((got, n), (m.clone(), 1));
        assert!(!inflight_path(&layout).exists(), "the marker is consumed");
        assert!(take_inflight(&layout, now + 1).is_none());
        let u = load_unfinished(&layout);
        assert!(held(&u, "rt1", &key, now + RETRY_S - 1), "held for a day");
        assert!(!held(&u, "rt1", &key, now + RETRY_S), "then tried once more");
        assert!(!held(&u, "rt2", &key, now), "another pack is not");
        // held: listed with no provider, its providers gone from the profile's union
        let mut g = gpus.clone();
        let (all, per) = profile_providers(&layout, &g);
        assert_eq!(all, vec!["cpu"], "{all:?}");
        assert_eq!(per.unwrap()["rt1"], Vec::<String>::new());
        // a second unfinished one: never again for these adapters and drivers ...
        mark(&layout, &Inflight { kind: "bench".into(), run: Some("h2h-x".into()), t: now + RETRY_S, ..m.clone() }).unwrap();
        assert_eq!(take_inflight(&layout, now + RETRY_S + 5).unwrap().1, 2);
        let u = load_unfinished(&layout);
        assert!(held(&u, "rt1", &key, i64::MAX / 2));
        assert_eq!(u[0].kind, "bench");
        // ... a driver update is a new key: one more try
        g[0].driver = "999.1".into();
        assert!(!held(&u, "rt1", &adapters_key(&g), now));
        // a broken marker is dropped, nothing counted
        std::fs::write(inflight_path(&layout), b"{not json").unwrap();
        assert!(take_inflight(&layout, now).is_none());
        assert!(!inflight_path(&layout).exists());
        assert_eq!(load_unfinished(&layout)[0].count, 2);
    }

    #[test]
    fn next_probe_skips_held_packs() {
        let d = tempfile::tempdir().unwrap();
        let layout = Layout::new(d.path());
        let gpus = [gpu("RTX 4070 Ti", "nvidia", "000000000000d1f3")];
        let key = adapters_key(&gpus);
        let u = vec![Unfinished { runtime: "rt1".into(), adapters: key.clone(), count: 1, t: 10, kind: "probe".into(), worker: "w".into() }];
        std::fs::create_dir_all(layout.state()).unwrap();
        std::fs::write(unfinished_path(&layout), serde_json::to_vec(&u).unwrap()).unwrap();
        // (no cached builds here: next_probe finds nothing either way; the rule itself)
        assert!(held(&load_unfinished(&layout), "rt1", &key, 11));
        let keys = zerofarm_common::keys::TrustedKeys::default();
        assert!(next_probe(&layout, &keys, &gpus, 11).is_none());
    }

    #[test]
    fn amd_providers_are_never_probed_benched_or_chosen() {
        assert!(!PROBE_PROVIDERS.contains(&"amdgpu") && !PROBE_PROVIDERS.contains(&"migraphx"));
        assert!(PROBE_PROVIDERS.iter().all(|p| ORDER.contains(p)));
        // a bench an older agent kept with an AMD provider "passing": never chosen
        let gpus = [gpu("RX 9070 XT", "amd", "00000000000000a1")];
        let mut rows = BTreeMap::new();
        rows.insert("64".to_string(), 1e6);
        let mut cpu_rows = BTreeMap::new();
        cpu_rows.insert("64".to_string(), 10.0);
        let b = Bench {
            runtime: "rt".into(),
            adapters: adapters_key(&gpus),
            run: "r".into(),
            worker: "w".into(),
            t: 1,
            threads: 2,
            results: vec![
                BenchRow { provider: "cpu".into(), adapter: None, ok: true, rows: cpu_rows },
                BenchRow { provider: "amdgpu".into(), adapter: Some("RX 9070 XT".into()), ok: true, rows: rows.clone() },
                BenchRow { provider: "migraphx".into(), adapter: Some("RX 9070 XT".into()), ok: true, rows },
            ],
            error: None,
            gpu_games: None,
        };
        let c = choose(Some(&b), &gpus, None, true, 128, 128, 2);
        assert_eq!(c.providers, vec!["cpu"], "{c:?}");
    }

    #[test]
    fn probes_are_pinned_to_a_usable_adapter_when_one_is_off() {
        let nv = gpu("RTX 4080", "nvidia", "00000000000000b1");
        let intel = gpu("UHD 770", "intel", "00000000000000b2");
        let amd = gpu("RX 9070 XT", "amd", "00000000000000a1");
        let (both, off) = (vec![intel.clone(), nv], vec![amd]);
        assert_eq!(pin_gpu(&both, &[]), None, "nothing off: every adapter, as before");
        assert_eq!(pin_gpu(&both, &off).as_deref(), Some("00000000000000b1"), "NVIDIA first");
        assert_eq!(pin_gpu(std::slice::from_ref(&intel), &off).as_deref(), Some("00000000000000b2"));
        assert_eq!(pin_gpu(&[], &off), None, "an AMD-only PC: nothing runs at all");
    }
}

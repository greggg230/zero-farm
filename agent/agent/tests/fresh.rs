//! Agent 0.3.17 / 0.3.18 (PROTOCOL 18.3): the freshness cap - a rolling lease whose coordinator sends the run's `fresh_s`
//! caps the job's games in flight at `fresh_s` x its leaves/s / the p75 game's leaves (a worker's first wave), handing a
//! running layout over the cap down to the capped one by the usual handoff, and a later lease of the same run, build,
//! runtime and provider starts at the remembered cap (agent 0.3.18: also when every lease ends before a steady state, as
//! a run's ~9-minute leases with ~7-minute drains do); a coordinator without the field: exactly as before. With the real
//! engine, the fakeworker (a CUDA crew, which the farm's measurements showed losing 12-14 % of its rows to staleness) and
//! the mock coordinator.
// GPU work: Windows only (no GPU jobs on macOS)
#![cfg(windows)]

mod harness;

use farm_agent::policy::GpuSignal;
use harness::{wait, H};
use serde_json::{json, Value};
use std::path::Path;
use std::time::{Duration, Instant};
use zerofarm_common::manifest::{GpuNeed, Kind};
use zerofarm_common::protocol::Gpu;
use zerofarm_common::sign::{template, SigningKey};

const RUN: &str = "gpu-run";

/// The canary PC's GPU: an RTX 4070 Ti, 12,282 MB.
fn nvidia(vram_mb: u64) -> Gpu {
    Gpu { vendor: "nvidia".into(), name: "RTX 4070 Ti".into(), vram_mb, driver: "560.94".into(), luid: Some("000000000000d1f3".into()) }
}

/// The probe and bench of a PC where CUDA passes: the canary's 4070 Ti rates (its full batch: 512 rows, the job's target
/// 1,024 games in flight), the CPU flat at 1,000 rows/s.
fn cuda_passes() -> Value {
    let rows = |v: [f64; 6]| json!({"1": v[0], "8": v[1], "32": v[2], "128": v[3], "512": v[4], "1024": v[5]});
    json!({"probe": 1, "providers": ["cuda", "cpu"], "error": null, "results": [
        {"provider": "cpu", "gpu": null, "ok": true, "rows_per_s": 1000.0, "rows_per_s_by_batch": rows([1000.0; 6])},
        {"provider": "cuda", "gpu": {"name": "RTX 4070 Ti", "luid": "000000000000d1f3"}, "ok": true, "rows_per_s": 20000.0,
         "rows_per_s_by_batch": rows([300.0, 2400.0, 7000.0, 16000.0, 24000.0, 24000.0])}]})
}

/// A CUDA-like runtime pack signed with `gpu` (nvidia, cuda).
fn cuda_pack(dir: &Path, key: &SigningKey) -> String {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join("onnxruntime.dll"), b"a small dll").unwrap();
    let mut t = template(Kind::Runtime, "cuda-test");
    t.min_agent = Some("0.2.10".into());
    t.gpu = Some(GpuNeed { vendor: "nvidia".into(), min_driver: Some("527.41".into()), providers: vec!["cuda".into()] });
    zerofarm_common::sign::sign_dir(dir, t, key).unwrap().0
}

/// The fakeworker signed with `requires_runtime` = `pack`, its probe and bench reports CUDA passing.
fn cuda_worker(dir: &Path, key: &SigningKey, pack: &str) -> String {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::copy(mockcoord::testkit::bin("fakeworker"), dir.join("fakeworker.exe")).unwrap();
    std::fs::write(dir.join("probe.json"), cuda_passes().to_string()).unwrap();
    std::fs::write(dir.join("probe_model.json"), cuda_passes().to_string()).unwrap();
    let mut t = template(Kind::Worker, "fake-cuda");
    t.game = Some("fake".into());
    t.aw = Some(1);
    t.zero_onnx = Some(vec![1]);
    t.profile = Some("release".into());
    t.entry = Some("fakeworker.exe".into());
    t.requires_runtime = Some(pack.into());
    t.min_agent = Some("0.2.10".into());
    zerofarm_common::sign::sign_dir(dir, t, key).unwrap().0
}

struct Gpus {
    pack: String,
    alt: String,
}

/// The harness of an NVIDIA PC (a GPU of `vram_mb`, GPU work allowed): `workers` per job, `autotune`; the fake worker
/// never counted as starved by the test machine's load.
fn harness(vram_mb: u64, workers: Option<u32>, autotune: bool) -> (H, Gpus) {
    let mut h = H::new();
    h.version = "0.3.4".into();
    h.signals.real_starvation = false;
    h.gpus = vec![nvidia(vram_mb)];
    gpu_signal(&h, true);
    {
        let mut m = h.machine.lock().unwrap();
        m.settings.workers = workers;
        m.settings.autotune = autotune;
    }
    let pack = cuda_pack(&h.dir.path().join("pack"), &h.key.key);
    h.mock.add_build_dir(&h.dir.path().join("pack"));
    let alt = cuda_worker(&h.dir.path().join("wcuda"), &h.key.key, &pack);
    h.mock.add_build_dir(&h.dir.path().join("wcuda"));
    (h, Gpus { pack, alt })
}

fn gpu_signal(h: &H, on: bool) {
    h.signals.set(|s| s.gpu = on.then_some(GpuSignal { other_util_pct: 0, temp_c: None }));
}

/// A rolling lease of the CUDA build: ids `[0, block)` (8 released), `gpu` games in flight for a GPU job, the definition's
/// settings carrying `fake`.
fn gpu_lease(h: &H, g: &Gpus, id: &str, fake: &Value, gpu: Value, block: u64) -> Value {
    let (m, r) = (format!("onnx graph {RUN}").into_bytes(), format!("reference rows {RUN}").into_bytes());
    let (ml, rl) = (m.len(), r.len());
    let model = format!("ckpt/{RUN}/1/model.onnx");
    let reference = format!("ckpt/{RUN}/1/reference.npz");
    let sha = h.mock.add_artifact(&model, m);
    let rsha = h.mock.add_artifact(&reference, r);
    let def = json!({"definition": 1, "kind": "run", "id": RUN, "version": 1, "created": zerofarm_common::now_unix() - 60, "expires": null,
        "game": "fake", "builds": [h.build_id, g.alt], "runtime": null,
        "checkpoints": {"1": {"model_sha256": sha, "model_size": ml, "reference_sha256": rsha, "reference_size": rl}},
        "jobs": {"selfplay": {"settings": {"sims": 8, "fake": fake}}}});
    let dref = h.define(&def);
    let mut l = h.lease(id, json!({"kind": "selfplay", "gid_base": 0, "games": 8, "seed": 7}));
    l["run"] = json!(RUN);
    l["definition"] = dref;
    l["checkpoint"] = json!({"gen": 1, "model": model, "sha256": sha, "reference": reference, "reference_sha256": rsha});
    l["build"] = json!({"build_id": g.alt, "manifest": format!("builds/{}/manifest.json", g.alt)});
    l["runtime"] = json!({"build_id": g.pack, "manifest": format!("builds/{}/manifest.json", g.pack)});
    l["rolling"] = json!({"block": [0, block], "inflight": {"cpu": 8, "gpu": gpu}, "until": zerofarm_common::now_unix() + 7200, "max_s": 7200, "flush_s": 10});
    l
}

/// The fake: a CUDA worker of `game_ms` games, small chunks, statuses every 100 ms, a rate model (counters; `crew`: the
/// crew's total by its size), `vram_base_mb` + 1 MB a batch row of video memory.
fn fake(game_ms: u64, crew: Value) -> Value {
    json!({"game_ms": game_ms, "shard_every": 4, "flush_ms": 300, "status_ms": 100, "provider": "cuda",
           "vram_base_mb": 600, "vram_per_row_mb": 1, "rate": {"leaves_s": 1000, "best": {}, "crew": crew}})
}

/// This PC measured the run's memory per game with the CUDA build (the canary's 2-3 MB, remembered as 4).
fn seed_memory(h: &H, g: &Gpus, per_game: u64) {
    std::fs::create_dir_all(h.layout.state()).unwrap();
    std::fs::write(h.layout.state().join("rolling_memory.json"), json!({format!("{RUN}/{}/gpu", g.alt): per_game}).to_string()).unwrap();
}

fn job_lines(h: &H) -> Vec<Value> {
    (0..).map_while(|n| h.job_line_n(n)).collect()
}


fn fresh_log(h: &H) -> String {
    h.agent_log().lines().filter(|l| l.contains("freshness") || l.contains("handing the job over") || l.contains("joins the crew")).collect::<Vec<_>>().join("
")
}

/// The cap of the last "freshness: ... at most N games in flight" line.
fn last_cap(h: &H) -> Option<u64> {
    let log = h.agent_log();
    let l = log.lines().filter(|l| l.contains("tuning [") && l.contains("freshness:") && l.contains("at most ")).next_back()?;
    let rest = &l[l.find("at most ")? + 8..];
    rest.split_whitespace().next()?.replace(',', "").parse().ok()
}

/// A CUDA crew of the run's 600 games in flight (three workers of 200, each finishing a game every 40 ms: 75 games a
/// second in all) on a run whose games may last 2 s: once a turnover of its start finished and the rate was measured, the
/// cap - rate x 2 s, ~150 games in flight - binds, and the crew is handed over to a layout within it (two workers of ~75:
/// fewer workers rather than workers under `fresh::MIN_PER_WORKER_GPU`; the old crew's games in flight finished or given
/// back, every id accounted for). When the coordinator then says the run has no budget any more, the cap goes and the
/// fixed crew is handed back to its start.
#[test]
fn a_cuda_crew_is_capped_by_the_runs_freshness_budget() {
    let (h, g) = harness(12_282, None, true);
    seed_memory(&h, &g, 3);
    let mut h = h;
    let mut l = gpu_lease(&h, &g, "l_FR", &fake(40, json!([1.0, 1.0, 1.0])), json!(600), 1_000_000);
    l["rolling"]["fresh_s"] = json!(2);
    h.push(l);
    h.start();
    assert!(wait(|| job_lines(&h).len() >= 2), "{}", h.agent_log());
    let start_total: u64 = {
        let n = h.agent_log().lines().filter(|l| l.contains("joins the crew")).count() as u64 + 1;
        n * job_lines(&h)[0]["conc"].as_u64().unwrap()
    };
    assert!((384..=640).contains(&start_total), "the run's 600 shared by the crew: {}", fresh_log(&h));
    // the cap: once measured, binding, and the job handed over to a layout within it
    assert!(wait(|| h.agent_log().contains("capping the job at")), "{}", fresh_log(&h));
    let cap = last_cap(&h).expect("a cap");
    assert!((crate_floor()..start_total).contains(&cap), "cap {cap} under the start's {start_total}: {}", fresh_log(&h));
    let lines0 = job_lines(&h).len();
    assert!(wait(|| h.agent_log().lines().any(|l| l.contains("tuning [l_FR]: handing the job over to") && l.contains("not measured"))), "{}", fresh_log(&h));
    assert!(wait(|| job_lines(&h).len() > lines0), "{}", fresh_log(&h));
    // every job line from the handoff on plays within the cap in all (its workers' share each)
    let after: Vec<Value> = job_lines(&h)[lines0..].to_vec();
    let capped = last_cap(&h).unwrap();
    for j in &after {
        assert!(j["conc"].as_u64().unwrap() <= capped, "{j} over the cap of {capped}: {}", fresh_log(&h));
    }
    // no budget any more (an answer's `fresh_s: null`): the cap goes, and the fixed crew goes back to its start
    h.mock.set_fresh("l_FR", Value::Null);
    assert!(wait(|| h.agent_log().contains("no game-latency budget any more")), "{}", fresh_log(&h));
    let n = job_lines(&h).len();
    assert!(wait(|| job_lines(&h)[n..].iter().any(|j| j["conc"].as_u64().unwrap() > capped)), "{}", fresh_log(&h));
    h.stop();
    let done = h.wait_done("l_FR");
    let (credited, again) = credits(&h, "l_FR");
    assert_eq!(again, 0, "an id played twice: {done}");
    assert_eq!(credited + unfinished_n(&done), done["stats"]["released"].as_u64().unwrap(), "{done}");
    assert!(h.crashes().is_empty(), "{:?}
{}", h.crashes(), h.agent_log());
}

fn crate_floor() -> u64 {
    farm_agent::fresh::FLOOR_GPU
}

/// The short-lease pattern (agent 0.3.18): the canary's leases of 2026-11-22 ran ~9 minutes, then the coordinator
/// answered `finish` (a renewal or the priority lane) and the crew drained its games for ~7 minutes - 0.3.17's estimator
/// (completed games over 300 s after a whole turnover) never measured once. Here three leases in a row are each told
/// `finish` before their workers' first waves reach their p75 game (~6 s): the drain finishes the first waves, so lease 1
/// learns the run's leaves per game and its leaves/s (no cap binds in it), and leases 2 and 3 start AT the capped layout -
/// their very first job lines within the cap, no handoff down.
#[test]
fn short_leases_learn_the_cap_and_the_next_leases_start_at_it() {
    let (h, g) = harness(12_282, None, true);
    seed_memory(&h, &g, 3);
    let mut h = h;
    let mut template = gpu_lease(&h, &g, "l_S1", &fake(40, json!([1.0, 1.0, 1.0])), json!(600), 3_000_000);
    template["rolling"]["fresh_s"] = json!(5);
    h.start();
    let mut caps = Vec::new();
    for n in 1..=3u64 {
        let id = format!("l_S{n}");
        let mut l = template.clone();
        l["lease_id"] = json!(id);
        l["job"]["gid_base"] = json!((n - 1) * 1_000_000);
        l["rolling"]["block"] = json!([(n - 1) * 1_000_000, n * 1_000_000]);
        let n0 = job_lines(&h).len();
        h.push(l);
        let crew = |h: &H| h.agent_log().lines().filter(|x| x.contains(&format!("crew [{id}]")) && x.contains("joins the crew")).count() + 1;
        assert!(wait(|| job_lines(&h).len() > n0 && crew(&h) >= 2), "{}", fresh_log(&h));
        if n >= 2 {
            // it starts at the remembered cap: every job line of the lease within it
            let lines: Vec<String> = h.agent_log().lines().filter(|x| x.contains(&format!("tuning [{id}]: freshness:")) && x.contains("starting at most")).map(String::from).collect();
            assert_eq!(lines.len(), 1, "lease {n} starts capped: {}", fresh_log(&h));
            let cap = last_cap(&h).unwrap();
            assert!((crate_floor()..600).contains(&cap), "cap {cap}: {}", fresh_log(&h));
            caps.push(cap);
        }
        // ~3 s of play (the first waves' p75 comes at ~6 s), then `finish` and the drain
        std::thread::sleep(Duration::from_secs(3));
        h.mock.script_extend(&id, json!({"finish": true, "reason": "max_s"}));
        let done = h.wait_done(&id);
        let (credited, again) = credits(&h, &id);
        assert_eq!(again, 0, "{done}");
        assert_eq!(credited + unfinished_n(&done), done["stats"]["released"].as_u64().unwrap(), "{done}");
        if let Some(&cap) = caps.last().filter(|_| n >= 2) {
            // (every worker of the lease within its share of the cap, the lease's Stats saying so, and no handoff down)
            let workers = crew(&h) as u64;
            for j in &job_lines(&h)[n0..] {
                assert!(j["conc"].as_u64().unwrap() * workers <= cap, "lease {n}: {j} over the cap of {cap}: {}", fresh_log(&h));
            }
            assert_eq!(done["stats"]["fresh_cap"].as_u64(), Some(cap), "{done}");
            assert!(!h.agent_log().contains(&format!("tuning [{id}]: handing the job over to")), "no handoff: {}", fresh_log(&h));
        } else {
            assert!(!h.agent_log().contains("tuning [l_S1]: freshness: games may last"), "no cap in lease 1: {}", fresh_log(&h));
            assert!(done["stats"].get("fresh_cap").is_none(), "{done}");
        }
    }
    // lease 1 learned from its drain (first-wave samples, the leaves/s), and the caps agree lease to lease
    assert!(h.agent_log().contains("tuning [l_S1]: freshness: worker pid") && h.agent_log().contains("its p75 game took"), "{}", fresh_log(&h));
    let learned: Value = serde_json::from_slice(&std::fs::read(h.layout.state().join("rolling_fresh.json")).unwrap()).unwrap();
    let entry = learned.as_object().unwrap().values().next().cloned().unwrap();
    assert!(entry["lpg"].as_array().is_some_and(|a| a.len() >= 3) && entry["leaves_s"].as_f64().is_some(), "{learned}");
    assert!(caps[1] as f64 >= caps[0] as f64 * 0.7 && caps[1] as f64 <= caps[0] as f64 * 1.3, "{caps:?}");
    h.stop();
    assert!(h.crashes().is_empty(), "{:?}
{}", h.crashes(), h.agent_log());
}

/// A coordinator that sends no budget (before rev 26, or a machine its owner's setting leaves out): no cap however long
/// the games last - the crew plays its start on, nothing about freshness in the log.
#[test]
fn without_a_budget_nothing_is_capped() {
    let (h, g) = harness(12_282, None, true);
    seed_memory(&h, &g, 3);
    let mut h = h;
    h.push(gpu_lease(&h, &g, "l_NF", &fake(40, json!([1.0, 1.0, 1.0])), json!(600), 1_000_000));
    h.start();
    assert!(wait(|| job_lines(&h).len() >= 2), "{}", h.agent_log());
    // past a turnover of the start and the rate window (the capped test's cap comes within this)
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(20) {
        std::thread::sleep(Duration::from_millis(500));
    }
    assert!(!h.agent_log().contains("freshness"), "{}", fresh_log(&h));
    assert!(!h.layout.state().join("rolling_fresh.json").exists(), "nothing learned without a budget");
    // (the memory plan's handoffs are 0.3.16's; nothing is cut below the run's 600 shared by the crew)
    for j in &job_lines(&h) {
        assert!(j["conc"].as_u64().unwrap() >= 192, "{j}");
    }
    h.stop();
    let done = h.wait_done("l_NF");
    assert!(done["stats"].get("fresh_cap").is_none(), "{done}");
}

fn credits(h: &H, id: &str) -> (u64, u64) {
    h.mock.with(|s| s.credits.iter().filter(|(l, ..)| l == id).fold((0, 0), |(c, a), (_, _, x, y)| (c + x, a + y)))
}

fn unfinished_n(done: &Value) -> u64 {
    done["unfinished"]["gids"].as_array().map_or(0, |u| u.iter().map(|r| r[1].as_u64().unwrap() - r[0].as_u64().unwrap()).sum())
}

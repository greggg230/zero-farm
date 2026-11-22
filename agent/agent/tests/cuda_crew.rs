//! Agent 0.3.16: an NVIDIA GPU's CUDA job is a CUDA crew (PROTOCOL 13.1 **CUDA crews**) - DirectML's target and start
//! (up to 3 workers of 1,152 games in flight in two groups, fewer games each where memory holds less, then fewer workers,
//! at least two where two fit) and no in-lease trials - with the real engine, the fakeworker (a CUDA job) and the mock
//! coordinator. The bench-shaped start other GPUs keep is tests/gpu.rs's.
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

#[allow(dead_code)]
fn range_of(j: &Value) -> (u64, u64) {
    let b = j["gid_base"].as_u64().unwrap();
    (b, b + j["games"].as_u64().unwrap())
}

fn credits(h: &H, id: &str) -> (u64, u64) {
    h.mock.with(|s| s.credits.iter().filter(|(l, ..)| l == id).fold((0, 0), |(c, a), (_, _, x, y)| (c + x, a + y)))
}

fn unfinished_n(done: &Value) -> u64 {
    done["unfinished"]["gids"].as_array().map_or(0, |u| u.iter().map(|r| r[1].as_u64().unwrap() - r[0].as_u64().unwrap()).sum())
}

/// No id played twice, and every released id credited or reported unfinished.
fn ids_accounted(h: &H, id: &str, done: &Value) {
    let (credited, again) = credits(h, id);
    assert_eq!(again, 0, "an id played twice: {done}");
    let released = done["stats"]["released"].as_u64().unwrap();
    assert_eq!(credited + unfinished_n(done), released, "{done}");
}

fn crew_log(h: &H) -> String {
    h.agent_log().lines().filter(|l| l.contains("crew [") || l.contains("tuning [") || l.contains("yielded") || l.contains("resumed")).collect::<Vec<_>>().join("\n")
}

/// The 4070 Ti PC's lease under 0.3.16: the crew rule's start (two groups of at most 576 rows, each worker 768 to 1,152
/// games in flight, 2 or 3 workers by this test machine's memory limit) whatever the bench's batch, and it plays that
/// layout on: no measured trial is handed over to, even with the owner away and trials allowed, and nothing is
/// remembered as a search (a fixed layout).
#[test]
fn an_nvidia_cuda_job_starts_the_crew_rule_and_runs_no_trials() {
    let (h, g) = harness(12_282, None, true);
    seed_memory(&h, &g, 3);
    let mut h = h;
    // (two workers 40 % over one in all, three 20 %: a tuner would hand over)
    h.push(gpu_lease(&h, &g, "l_CC", &fake(40, json!([1.0, 1.4, 1.2])), json!("auto"), 1_000_000));
    h.start();
    assert!(wait(|| job_lines(&h).len() >= 2), "{}", h.agent_log());
    let first: Vec<Value> = job_lines(&h);
    let n = h.agent_log().lines().filter(|l| l.contains("joins the crew")).count() + 1;
    assert!((2..=3).contains(&n), "a crew of two or three: {}", crew_log(&h));
    for j in &first[..2] {
        assert_eq!(j["providers"], json!(["cuda", "cpu"]), "{j}");
        let conc = j["conc"].as_u64().unwrap();
        assert!((768..=1152).contains(&conc), "the crew rule's 768-1,152 a worker: {j}");
        assert_eq!(j["groups"].as_u64(), Some(2), "two groups: {j}");
    }
    // well past the time the tuner's first trial takes here (tests/gpu.rs: tuned within minutes)
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(45) {
        std::thread::sleep(Duration::from_millis(500));
    }
    let measured = h.agent_log().lines().filter(|l| l.contains("tuning [l_CC]: handing the job over to") && l.contains("(a measured trial)")).count();
    assert_eq!(measured, 0, "no in-lease trial: {}", crew_log(&h));
    let tuned = std::fs::read(h.layout.state().join("tuning.json")).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok()).unwrap_or(Value::Null);
    assert!(tuned["entries"].as_array().is_none_or(|e| e.iter().all(|x| x["key"]["tuner"] != 5)), "a fixed layout remembers no search: {tuned}");
    h.stop();
    let done = h.wait_done("l_CC");
    ids_accounted(&h, "l_CC", &done);
    assert!(h.crashes().is_empty(), "{:?}", h.crashes());
}

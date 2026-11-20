//! Agent 0.3.4: rolling GPU self-play leases that use the GPU properly (PROTOCOL 10.6, 13, 13.5), with the real engine,
//! the fakeworker (a CUDA job: `provider` cuda, its `rate` model, the video memory it reports) and the mock
//! coordinator: the job's games in flight from the bench's batch target and the calibrated memory plan - raised within
//! the lease once a measurement says they fit -, a crew of workers each with its own GPU session and part of the search
//! threads and memory, their video memory planned within the GPU's (and given back on a pause: every member yields),
//! a member's crash, a worker out of video memory started again with smaller batches, and the tuner choosing the GPU
//! layout (its worker count reported).
// GPU work: Windows only (no GPU jobs on macOS)
#![cfg(windows)]

mod harness;

use farm_agent::policy::GpuSignal;
use harness::{wait, H};
use serde_json::{json, Value};
use std::collections::BTreeSet;
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

fn learned(h: &H) -> Value {
    std::fs::read(h.layout.state().join("rolling_memory.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or(json!({}))
}

fn job_lines(h: &H) -> Vec<Value> {
    (0..).map_while(|n| h.job_line_n(n)).collect()
}

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

fn chunk_ids(h: &H, id: &str) -> Vec<(u64, u64)> {
    h.mock
        .results_for(id)
        .iter()
        .filter_map(|(_, r)| r.headers.get("x-ids").and_then(|v| serde_json::from_str::<Value>(v).ok()))
        .flat_map(|v| v.as_array().cloned().unwrap_or_default())
        .map(|r| (r[0].as_u64().unwrap(), r[1].as_u64().unwrap()))
        .collect()
}

/// The workers (their first job lines' ranges) whose ids appear in `id`'s chunks.
fn workers_uploading(h: &H, id: &str) -> usize {
    let ranges: Vec<(u64, u64)> = job_lines(h).iter().map(range_of).collect();
    let ids = chunk_ids(h, id);
    ranges.iter().filter(|(a, _)| ids.iter().any(|(x, _)| x >= a && ranges.iter().filter(|(b, _)| b > a).all(|(b, _)| x < b))).count()
}

fn crew_log(h: &H) -> String {
    h.agent_log().lines().filter(|l| l.contains("crew [") || l.contains("tuning [") || l.contains("yielded") || l.contains("resumed")).collect::<Vec<_>>().join("\n")
}

/// The pids whose fakeworker logged `what` ("yielded", "resumed").
fn pids_logging(h: &H, what: &str) -> BTreeSet<u64> {
    h.agent_log()
        .lines()
        .filter_map(|l| {
            let i = l.find("fakeworker: pid ")?;
            let rest: Vec<&str> = l[i + 16..].split_whitespace().collect();
            (rest.get(1) == Some(&what)).then(|| rest[0].parse().ok()).flatten()
        })
        .collect()
}

/// The canary of 2026-11-19 with agent 0.3.4: its bench's batch is 512 rows, so the job targets 1,024 games in flight;
/// with the memory per game measured there (remembered as 4 MB) and the GPU's 12 GB, an NVIDIA PC starts a crew of
/// three workers of 384 in one group each, its search threads shared (12 on this test PC's 8 cores: 4 each), each worker
/// a third of the job's memory limit, every worker's job line a CUDA job without inference fields (the worker's own).
/// A coordinator hint (the hardware class's, a CPU layout) is not used for a GPU job.
#[test]
fn a_gpu_lease_starts_a_crew_at_the_bench_target() {
    let (mut h, g) = harness(12_282, None, true);
    // (the owner's computer in use: work goes on, trials don't - the start's layout stays)
    h.machine.lock().unwrap().settings.mode = farm_agent::settings::Mode::Always;
    h.user_active(true);
    seed_memory(&h, &g, 4);
    let mut l = gpu_lease(&h, &g, "l_GA", &fake(20, json!([1.0, 1.3, 1.5])), json!("auto"), 1_000_000);
    l["tuning_hint"] = json!({"layout": {"threads": 8, "infer_workers": 1, "infer_threads": 8, "groups": 4, "conc": 32}, "leaves_s": 900, "machines": 3, "net": "0123456789abcdef"});
    h.push(l);
    h.start();
    assert!(wait(|| job_lines(&h).len() >= 3), "{}", h.agent_log());
    let lines = job_lines(&h);
    for j in &lines[..3] {
        assert_eq!(j["providers"], json!(["cuda", "cpu"]), "{j}");
        assert_eq!(j["gpu"]["luid"], "000000000000d1f3", "{j}");
        assert_eq!((j["conc"].as_u64(), j["groups"].as_u64(), j["threads"].as_u64()), (Some(384), Some(1), Some(4)), "3 x 384 in one group, 4 of 12 search threads: {j}");
        assert!(j.get("infer_workers").is_none() && j.get("infer_threads").is_none(), "a GPU job's inference threads are the worker's own: {j}");
        assert_eq!(j["rolling"], true);
    }
    let log = h.agent_log();
    // each worker a third of the job's limit (half the RAM, at most the free memory less a floor: 15,808 MB here)
    let limits: BTreeSet<u64> = log
        .lines()
        .filter(|l| l.contains("joins the crew"))
        .filter_map(|l| l.split("memory limit ").nth(1)?.split(' ').next()?.parse().ok())
        .collect();
    assert!(limits.len() == 1 && limits.iter().all(|&m| (15_000..=16_384).contains(&(3 * m))), "{limits:?}: {}", crew_log(&h));
    assert!(!log.contains("starting the tuning from the coordinator's hint"), "no hint for a GPU job");
    // beats: three workers, 1,152 games in flight, their video memory as measured (600 + 384 MB each)
    let beat = |h: &H| h.mock.with(|s| s.beats.iter().filter(|(l, _)| l == "l_GA").map(|(_, b)| b["stats"].clone()).next_back());
    assert!(wait(|| beat(&h).is_some_and(|b| b["workers"] == 3 && b["inflight"] == 1152 && b["vram_mb"] == 3 * 984)), "{:?}\n{}", beat(&h), crew_log(&h));
    // the GPU busy rule sees every worker of the crew as the lease's own
    assert!(wait(|| h.signals.ours.lock().unwrap().len() >= 3), "{:?}", h.signals.ours.lock().unwrap());
    let status = h.status.lock().unwrap().tuning.clone().unwrap_or_default();
    assert!(status.contains("3 workers x (4 search threads, 384 games in flight in 1 group (384 rows)) on the GPU"), "{status}");
    h.stop();
    let done = h.wait_done("l_GA");
    ids_accounted(&h, "l_GA", &done);
    assert!(h.crashes().is_empty(), "{:?}", h.crashes());
}

/// The canary of 2026-11-19 with agent 0.3.3: the lease was planned at the GPU's default 1,800 MB + 45 MB a game (128 in
/// flight), the worker measured ~3 MB a game, and the games in flight stayed. Now the steady reading lowers the plan
/// within the lease and the job is handed over to a worker of the target, 1,024 games in flight in two groups of 512
/// (`autotune` off here: the fixed rules - one worker - and no trials, so the handover is the plan's alone).
#[test]
fn a_gpu_jobs_games_in_flight_follow_the_calibrated_memory_within_the_lease() {
    let (mut h, g) = harness(12_282, None, false);
    h.machine.lock().unwrap().settings.max_ram_mb = Some(9_500);
    // the worker commits the CUDA runtime's 1,800 MB and 3 MB a game in flight
    let mut fk = fake(4, json!([1.0]));
    (fk["mem_base_mb"], fk["mem_per_game_mb"]) = (json!(1800), json!(3));
    h.push(gpu_lease(&h, &g, "l_GC", &fk, json!("auto"), 1_000_000));
    h.start();
    assert!(wait(|| !job_lines(&h).is_empty()), "{}", h.agent_log());
    let first = h.job_line();
    // (0.85 x 9,500 - 1,800) / 45 = 139 games in flight (0.3.3's plan rounded it to 128), one batch (0.3.4: 2 of 70)
    assert_eq!((first["conc"].as_u64(), first["groups"].as_u64()), (Some(139), Some(1)), "{first}");
    let t0 = Instant::now();
    assert!(
        mockcoord::testkit::wait_until(Duration::from_secs(90), || job_lines(&h).iter().any(|j| j["conc"] == 1024)),
        "not raised after {:?}: {:?}\n{}",
        t0.elapsed(),
        learned(&h),
        h.agent_log().lines().filter(|l| l.contains("tuning [") || l.contains("planning")).collect::<Vec<_>>().join("\n")
    );
    let raised = job_lines(&h).into_iter().find(|j| j["conc"] == 1024).unwrap();
    assert_eq!((raised["groups"].as_u64(), raised["threads"].as_u64()), (Some(2), Some(12)), "two full batches of 512, every search thread: {raised}");
    let per = learned(&h)[format!("{RUN}/{}/gpu", g.alt)].as_u64().unwrap();
    assert!(per <= 6, "the memory per game remembered for the run and the CUDA build: {per}");
    h.stop();
    let done = h.wait_done("l_GC");
    ids_accounted(&h, "l_GC", &done);
    assert!(h.crashes().is_empty(), "{:?}", h.crashes());
}

/// GPU work stops being allowed (another program uses the GPU): every worker of the crew gives its video memory back
/// (`yield`, then stopped) - not only the main one -, nothing is played meanwhile, and all of them resume.
#[test]
fn a_gpu_crew_yields_its_video_memory_and_resumes_together() {
    let (mut h, g) = harness(12_282, Some(3), false);
    seed_memory(&h, &g, 4);
    h.push(gpu_lease(&h, &g, "l_GY", &fake(30, json!([1.0, 1.3, 1.5])), json!(96), 1_000_000));
    h.start();
    assert!(wait(|| job_lines(&h).len() >= 3 && workers_uploading(&h, "l_GY") >= 3), "{}", crew_log(&h));
    for j in &job_lines(&h)[..3] {
        assert_eq!(j["conc"], 32, "the run's 96 shared by three: {j}");
    }
    gpu_signal(&h, false);
    assert!(wait(|| h.phase() == farm_agent::status::Phase::Paused), "{:?}", h.status.lock().unwrap());
    assert!(wait(|| pids_logging(&h, "yielded").len() >= 3), "every worker yields: {}", crew_log(&h));
    std::thread::sleep(Duration::from_millis(700));
    let n = h.mock.results_for("l_GY").len();
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(h.mock.results_for("l_GY").len(), n, "nothing is played while paused: {}", crew_log(&h));
    let beat = |h: &H| h.mock.with(|s| s.beats.iter().filter(|(l, _)| l == "l_GY").map(|(_, b)| b["stats"].clone()).next_back());
    assert!(wait(|| beat(&h).is_some_and(|b| b["vram_mb"] == 0)), "their video memory given back: {:?}", beat(&h));
    gpu_signal(&h, true);
    assert!(wait(|| pids_logging(&h, "resumed").len() >= 3), "every worker resumes: {}", crew_log(&h));
    let before = chunk_ids(&h, "l_GY").len();
    assert!(wait(|| chunk_ids(&h, "l_GY").len() > before + 6), "work goes on");
    h.stop();
    let done = h.wait_done("l_GY");
    ids_accounted(&h, "l_GY", &done);
    assert!(done["stats"]["yields"].as_u64().unwrap() >= 1, "{done}");
    assert!(h.crashes().is_empty(), "{:?}", h.crashes());
}

/// A crew member crashes in its range: its games in flight go back (never credited, never played by another), the crash
/// is reported once, a worker takes its place, and the lease ends as any.
#[test]
fn a_gpu_crew_member_crashing_is_replaced_and_the_others_go_on() {
    let (mut h, g) = harness(12_282, Some(2), false);
    seed_memory(&h, &g, 4);
    let mut fk = fake(50, json!([1.0, 1.3]));
    fk["crash_gid"] = json!(12);
    h.push(gpu_lease(&h, &g, "l_GX", &fk, json!(8), 200));
    h.start();
    let done = h.wait_done("l_GX");
    assert_eq!(done["given_back"], false, "{done}\n{}", crew_log(&h));
    ids_accounted(&h, "l_GX", &done);
    assert_eq!(done["stats"]["released"], 200, "{done}");
    assert!(!chunk_ids(&h, "l_GX").iter().any(|&(a, b)| a <= 12 && 12 < b), "id 12 was never credited");
    let crashes = h.crashes();
    assert_eq!(crashes.len(), 1, "reported once: {crashes:?}");
    assert!(job_lines(&h).len() >= 3, "a worker took its place: {}", crew_log(&h));
    assert!(job_lines(&h).iter().all(|j| j["providers"] == json!(["cuda", "cpu"])));
    h.stop();
}

/// The video memory plan: a GPU with little memory gets one worker whose batches fit it, and when other programs take
/// the GPU's memory mid-lease (95 % of it in use) a worker of the crew gives way and none joins until it fits again.
#[test]
fn the_video_memory_plan_holds_the_crew() {
    // 3,500 MB, other programs 500 MB: 2,475 MB for the job - one worker's batches at most 725 rows, a second worker
    // doesn't fit (1,237 MB each, less than its base and a batch): one worker of 1,024 in two groups of 512
    let (mut h, g) = harness(3_500, None, false);
    h.signals.set_gpu_others(Some(500));
    seed_memory(&h, &g, 4);
    h.push(gpu_lease(&h, &g, "l_GV1", &fake(20, json!([1.0])), json!("auto"), 1_000_000));
    h.start();
    assert!(wait(|| !job_lines(&h).is_empty()), "{}", h.agent_log());
    std::thread::sleep(Duration::from_secs(2));
    let lines = job_lines(&h);
    assert_eq!(lines.len(), 1, "one worker: {}", crew_log(&h));
    assert_eq!((lines[0]["conc"].as_u64(), lines[0]["groups"].as_u64()), (Some(1024), Some(2)), "{}", lines[0]);
    assert!(h.agent_log().contains("video memory: 3500 MB on the GPU, 500 MB used by other programs, 2475 MB for the job's workers"), "{}", h.agent_log());
    h.stop();

    // a 12 GB GPU and a crew of three of 32 (a drain of 32 games takes ~5 s); another program takes 11.5 GB: one worker
    // gives way (finishes its games, one drain at a time) and none joins while the memory is taken
    let (mut h, g) = harness(12_282, Some(3), false);
    h.signals.set_gpu_others(Some(400));
    seed_memory(&h, &g, 4);
    h.push(gpu_lease(&h, &g, "l_GV2", &fake(150, json!([1.0, 1.3, 1.5])), json!(96), 1_000_000));
    h.start();
    assert!(wait(|| job_lines(&h).len() >= 3 && workers_uploading(&h, "l_GV2") >= 3), "{}", crew_log(&h));
    let joins = |h: &H| h.agent_log().matches("joins the crew").count();
    h.signals.set_gpu_others(Some(11_500));
    assert!(wait(|| h.agent_log().contains("the GPU's dedicated memory is")), "{}", crew_log(&h));
    assert!(wait(|| h.agent_log().contains("starts no new game until")), "a worker gives way: {}", crew_log(&h));
    let n = joins(&h);
    std::thread::sleep(Duration::from_millis(2500));
    assert_eq!(joins(&h), n, "no worker joins while the GPU's memory is taken: {}", crew_log(&h));
    assert_eq!(h.agent_log().matches("starts no new game until").count(), 1, "one drain at a time: {}", crew_log(&h));
    // the memory is free again: the crew refills once the drain is over
    h.signals.set_gpu_others(Some(400));
    assert!(wait(|| joins(&h) > n), "{}", crew_log(&h));
    h.stop();
    let done = h.wait_done("l_GV2");
    ids_accounted(&h, "l_GV2", &done);
    assert!(h.crashes().is_empty(), "{:?}", h.crashes());
}

/// A worker whose batches don't fit the GPU's memory fails like ONNX Runtime's CUDA allocator: the video memory per row
/// is planned higher (remembered per run and worker build - the memory per game is not touched) and the worker starts
/// again with fewer games in flight, reported once, until its batches fit; the lease goes on.
#[test]
fn a_worker_out_of_video_memory_is_started_again_with_smaller_batches() {
    let (mut h, g) = harness(12_282, None, false);
    seed_memory(&h, &g, 4);
    let mut fk = fake(20, json!([1.0]));
    fk["vram_oom_rows"] = json!(300);
    h.push(gpu_lease(&h, &g, "l_GO", &fk, json!("auto"), 1_000_000));
    h.start();
    assert!(wait(|| job_lines(&h).iter().any(|j| j["conc"].as_u64().unwrap().div_ceil(j["groups"].as_u64().unwrap()) <= 300)), "{}", h.agent_log());
    let lines = job_lines(&h);
    assert_eq!(lines[0]["conc"], 1024, "{:?}", lines);
    let concs: Vec<u64> = lines.iter().map(|j| j["conc"].as_u64().unwrap()).collect();
    assert!(concs.windows(2).all(|w| w[1] < w[0]), "fewer each time: {concs:?}");
    assert!(lines.len() <= 1 + farm_agent::rolling::MAX_RESTARTS as usize, "{concs:?}");
    // it plays on
    assert!(wait(|| !chunk_ids(&h, "l_GO").is_empty()), "{}", h.agent_log());
    let m = learned(&h);
    assert!(m[format!("{RUN}/{}/vram", g.alt)].as_u64().unwrap() > farm_agent::rolling::GPU_VRAM.per_row_mb, "{m}");
    assert_eq!(m[format!("{RUN}/{}/gpu", g.alt)], 4, "the memory per game untouched: {m}");
    let crashes = h.crashes();
    assert_eq!(crashes.len(), 1, "{crashes:?}");
    assert!(crashes[0]["message"].as_str().unwrap().contains("ran out of video memory"), "{crashes:?}");
    h.stop();
    let done = h.wait_done("l_GO");
    ids_accounted(&h, "l_GO", &done);
}

/// The tuner on a GPU job: from the NVIDIA start (three workers) it measures the worker count first (two play more
/// here) and keeps searching (groups, search threads), then remembers the result per GPU and driver and reports it with
/// its worker count.
#[test]
fn the_tuner_chooses_the_gpu_layout_and_reports_its_worker_count() {
    let (mut h, g) = harness(12_282, None, true);
    seed_memory(&h, &g, 4);
    // two workers 40 % over one in all, three only 20 %
    h.push(gpu_lease(&h, &g, "l_GT", &fake(40, json!([1.0, 1.4, 1.2])), json!(96), 1_000_000));
    h.start();
    let entry = |h: &H| -> Value {
        std::fs::read(h.layout.state().join("tuning.json")).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok()).map_or(Value::Null, |v| v["entries"][0].clone())
    };
    let t0 = Instant::now();
    assert!(
        h.mock.wait(Duration::from_secs(180), |_| entry(&h)["state"] == "tuned"),
        "not tuned after {:?}: {}\n{}",
        t0.elapsed(),
        entry(&h),
        crew_log(&h)
    );
    let e = entry(&h);
    assert_eq!(e["start"]["workers"], 3, "an NVIDIA GPU starts with the most workers that hold the target: {e}");
    assert_eq!(e["incumbent"]["workers"], 2, "two pay most: {e}\n{}", crew_log(&h));
    assert_eq!(e["incumbent"]["conc"], 48, "the run's 96 games in flight, 48 a worker: {e}");
    let hw = e["key"]["hw"].as_str().unwrap();
    assert!(hw.contains("gpu nvidia RTX 4070 Ti 12282 MB driver 560.94 | cuda"), "remembered per GPU and driver: {hw}");
    assert_eq!(e["key"]["runtime"], g.pack.as_str());
    assert!(e["space"]["gpu"] == true, "{e}");
    h.stop();
    let done = h.wait_done("l_GT");
    ids_accounted(&h, "l_GT", &done);
    // the next lease request reports it, with its worker count
    h.push(gpu_lease(&h, &g, "l_GT2", &fake(40, json!([1.0, 1.4, 1.2])), json!(96), 1_000_000));
    let before = job_lines(&h).len();
    h.start();
    assert!(wait(|| job_lines(&h).len() >= before + 2), "the next lease starts with its two workers: {}", crew_log(&h));
    let req = h.mock.with(|s| s.lease_requests.last().cloned().unwrap());
    let rep = req["tuning"].as_array().unwrap().iter().find(|r| r["runtime"] == g.pack.as_str()).cloned().unwrap_or(Value::Null);
    assert_eq!(rep["layout"]["workers"], 2, "{req}");
    assert_eq!(rep["layout"]["infer_workers"], 1, "{rep}");
    h.stop();
}

/// What the tuner logged: (every handoff line, every handoff to a measured trial).
fn handoffs(h: &H, id: &str) -> (Vec<String>, Vec<String>) {
    let all: Vec<String> = h.agent_log().lines().filter(|l| l.contains(&format!("tuning [{id}]: handing the job over to"))).map(String::from).collect();
    let measured = all.iter().filter(|l| l.contains("(a measured trial)")).cloned().collect();
    (all, measured)
}

/// Agent 0.3.5, the 4070 Ti PC's first 0.3.4 lease (round 4, 2026-11-20): no memory measured for the (run, build), so the
/// plan used the GPU default (1,800 MB + 45 MB a game) under its 9.5 GB cap - 139 games in flight, 5,179 leaves/s -
/// while the memory-fixed worker commits ~2 MB a game; and the tuner handed the job over to a trial whose worker didn't
/// fit beside the old one, which then drained alone for minutes, again and again. Now an early reading (half the games in
/// flight finished) plans ~4 MB a game within the first minutes, the start is picked again for the plan - a crew of two
/// of 512 there -, the crew starts beside the old worker as memory allows, and no trial loops. Agent 0.3.7: trials wait
/// for the early reading, then run after the old crew's drain when they don't fit beside it (0.3.5-0.3.6 postponed them,
/// so a memory-tight PC measured nothing) - each layout measured once.
#[test]
fn an_uncalibrated_gpu_start_reaches_the_target_within_minutes_and_never_loops() {
    let (mut h, g) = harness(12_282, None, true);
    h.machine.lock().unwrap().settings.max_ram_mb = Some(9_500);
    // the memory-fixed worker: ~1.7 GB of runtime, ~2 MB a game in flight
    let mut fk = fake(40, json!([1.0, 1.25, 1.4]));
    (fk["mem_base_mb"], fk["mem_per_game_mb"]) = (json!(1700), json!(2));
    h.push(gpu_lease(&h, &g, "l_GE", &fk, json!("auto"), 10_000_000));
    let t0 = Instant::now();
    h.start();
    assert!(wait(|| !job_lines(&h).is_empty()), "{}", h.agent_log());
    let first = h.job_line();
    assert_eq!((first["conc"].as_u64(), first["groups"].as_u64()), (Some(139), Some(1)), "the default plan's start, one batch of 139: {first}");
    // the crew of two of 512 (1,024 games in flight in all) within the first minutes - here seconds
    let beat = |h: &H| h.mock.with(|s| s.beats.iter().filter(|(l, _)| l == "l_GE").map(|(_, b)| b["stats"].clone()).next_back());
    assert!(
        mockcoord::testkit::wait_until(Duration::from_secs(60), || beat(&h).is_some_and(|b| b["workers"] == 2 && b["inflight"] == 1024)),
        "not at the target after {:?}: {:?}\n{}\n{:?}",
        t0.elapsed(),
        beat(&h),
        crew_log(&h),
        learned(&h)
    );
    eprintln!("at the target after {:?}", t0.elapsed());
    let log = h.agent_log();
    assert!(log.contains("early in their games"), "an early reading planned the job: {}", crew_log(&h));
    let per = learned(&h)[format!("{RUN}/{}/gpu", g.alt)].as_u64().unwrap();
    assert!((2..=6).contains(&per), "~2 MB a game measured, planned x 2: {per}");
    // the tuner runs on, without a handoff loop: each trial (after the drain when it doesn't fit beside) measured once
    std::thread::sleep(Duration::from_secs(12));
    let (all, measured) = handoffs(&h, "l_GE");
    assert!(all.len() <= 6, "a handoff loop: {all:#?}\n{}", crew_log(&h));
    let distinct: BTreeSet<&String> = measured.iter().collect();
    assert_eq!(distinct.len(), measured.len(), "a trial handed over to twice: {measured:#?}");
    assert!(!log.contains("drained alone"), "no full-drain trial");
    assert!(!h.agent_log().contains("is postponed"), "agent 0.3.7: no trial is postponed");
    // (the start, then at most a crew of three per handoff)
    let lines = job_lines(&h);
    assert!(lines.len() <= 1 + 3 * all.len(), "workers started: {} for {} handoffs\n{}", lines.len(), all.len(), crew_log(&h));
    h.stop();
    let done = h.wait_done("l_GE");
    ids_accounted(&h, "l_GE", &done);
    assert!(h.crashes().is_empty(), "{:?}", h.crashes());
}

/// Agent 0.3.5, the same lease's loop: a trial was handed over to, and by the time its worker started a trial couldn't run
/// any more (the PC's background load hovered at the 10 % bar); 0.3.4 swapped the candidate for the incumbent silently,
/// counting nothing, and planned the same candidate 20 s later. Now that is decided when the worker starts, and the
/// candidate counts as discarded (no trial again for a while, skipped after 3).
#[test]
fn a_trial_that_cant_run_when_its_worker_starts_counts_as_discarded() {
    let (mut h, g) = harness(12_282, Some(1), true);
    // (the owner's computer: work goes on while it is used, trials don't)
    h.machine.lock().unwrap().settings.mode = farm_agent::settings::Mode::Always;
    seed_memory(&h, &g, 4);
    let mut fk = fake(40, json!([1.0]));
    fk["rate"]["best"] = json!({"batch": 256});
    h.push(gpu_lease(&h, &g, "l_GD", &fk, json!(512), 10_000_000));
    h.start();
    // the start is measured by the lease's first worker; then a trial is handed over to - and the owner comes back
    assert!(wait(|| !handoffs(&h, "l_GD").1.is_empty()), "{}", crew_log(&h));
    h.user_active(true);
    assert!(wait(|| h.agent_log().contains("can't run now (the computer is busy or not idle): counted as discarded")), "{}", crew_log(&h));
    let n = handoffs(&h, "l_GD").1.len();
    std::thread::sleep(Duration::from_secs(4));
    assert_eq!(handoffs(&h, "l_GD").1.len(), n, "no trial while the computer is used: {}", crew_log(&h));
    let tuning: Value = serde_json::from_slice(&std::fs::read(h.layout.state().join("tuning.json")).unwrap()).unwrap();
    let e = &tuning["entries"][0];
    assert!(e["discards"].as_array().is_some_and(|d| d.iter().any(|x| x[1] == 1)), "the candidate counted as discarded: {e}");
    h.stop();
    let done = h.wait_done("l_GD");
    ids_accounted(&h, "l_GD", &done);
}

/// Agent 0.3.6, the 4070 Ti PC's 0.3.5 lease (round 5, 2026-11-20): the early reading planned 4 MB a game, but the plan's
/// fixed 1,800 MB base per worker and its 9.3 GB cap kept a crew from ever fitting beside the running worker, and the
/// lease stayed one worker of 1,024 (~9.7k leaves/s; a crew of two measured 12.4k there); and in the hand run of the same
/// day the start's drain (its last games at a few rows a batch, 5 minutes) kept the crew's second member out. Now the
/// base is measured too (a worker's commit before its games grow), the start is picked again when the new plan holds
/// more workers, and a worker finishing its games that the next one waits for ends its last games. Here: a 9.3 GB cap,
/// workers of 1.5 GB and 2 MB a game: a crew forms within seconds (minutes on a real PC), every id credited or returned.
#[test]
fn a_gpu_crew_forms_under_a_tight_cap_with_the_measured_base_and_no_drain_tail() {
    let (mut h, g) = harness(12_282, None, true);
    h.machine.lock().unwrap().settings.max_ram_mb = Some(9_300);
    // the memory-fixed worker: 1.5 GB of runtime at once, ~2 MB a game in flight as its games grow (over 2 s here)
    let mut fk = fake(40, json!([1.0, 1.25, 1.4]));
    (fk["mem_base_mb"], fk["mem_per_game_mb"], fk["mem_ramp_ms"]) = (json!(1500), json!(2), json!(2000));
    h.push(gpu_lease(&h, &g, "l_GB", &fk, json!("auto"), 10_000_000));
    let t0 = Instant::now();
    h.start();
    let beat = |h: &H| h.mock.with(|s| s.beats.iter().filter(|(l, _)| l == "l_GB").map(|(_, b)| b["stats"].clone()).next_back());
    let crew = |b: &Value| b["workers"].as_u64().unwrap_or(1) >= 2 && b["inflight"].as_u64().unwrap_or(0) as f64 >= 0.8 * 1024.0;
    assert!(
        mockcoord::testkit::wait_until(Duration::from_secs(60), || beat(&h).is_some_and(|b| crew(&b))),
        "no crew after {:?}: {:?}\n{}\n{:?}",
        t0.elapsed(),
        beat(&h),
        crew_log(&h),
        learned(&h)
    );
    eprintln!("crew after {:?}: {:?}", t0.elapsed(), beat(&h).map(|b| (b["workers"].clone(), b["inflight"].clone())));
    let log = h.agent_log();
    assert!(log.contains("before its games grow: planning a base of"), "the base measured: {}", crew_log(&h));
    // (the crew's last member waits for the draining start's memory: the start's last games end - their ids returned)
    assert!(log.contains("ends its last") && log.contains("the next worker waits for its memory"), "{}", crew_log(&h));
    let m = learned(&h);
    let base = m[format!("{RUN}/{}/gpu_base", g.alt)].as_u64().unwrap();
    assert!((1500..1650).contains(&base), "~1.5 GB measured: {m}");
    // the crew stays (no loop), and ends cleanly: every id credited or returned, also those of ended drain tails
    std::thread::sleep(Duration::from_secs(8));
    let (all, measured) = handoffs(&h, "l_GB");
    assert!(all.len() <= 4, "{all:#?}\n{}", crew_log(&h));
    let distinct: BTreeSet<&String> = measured.iter().collect();
    assert_eq!(distinct.len(), measured.len(), "a trial handed over to twice: {measured:#?}");
    h.stop();
    let done = h.wait_done("l_GB");
    ids_accounted(&h, "l_GB", &done);
    assert!(h.crashes().is_empty(), "{:?}", h.crashes());
}


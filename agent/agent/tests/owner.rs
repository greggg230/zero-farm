//! PROTOCOL 13.6: an owner machine - the farm owner's own PC, which plays its runs' self-play through the farm beside the
//! owner's trainer on the same GPU - with the real engine, the fakeworker (a CUDA job) and the mock coordinator: the
//! owner's fixed layout of more than 3 workers runs as set, and neither the owner at the keyboard, a fullscreen app nor
//! another program keeping the GPU busy (the trainer) holds it back.
// GPU work: Windows only (no GPU jobs on macOS)
#![cfg(windows)]

mod harness;

use farm_agent::policy::{GpuSignal, Notification};
use harness::{wait, H};
use serde_json::{json, Value};
use std::path::Path;
use std::time::{Duration, Instant};
use zerofarm_common::manifest::{GpuNeed, Kind};
use zerofarm_common::protocol::Gpu;
use zerofarm_common::sign::{template, SigningKey};

const RUN: &str = "owner-run";

fn nvidia(vram_mb: u64) -> Gpu {
    Gpu { vendor: "nvidia".into(), name: "RTX 4070 Ti".into(), vram_mb, driver: "560.94".into(), luid: Some("000000000000d1f3".into()) }
}

/// The probe and bench of a PC where CUDA passes (tests/cuda_crew.rs's).
fn cuda_passes() -> Value {
    let rows = |v: [f64; 6]| json!({"1": v[0], "8": v[1], "32": v[2], "128": v[3], "512": v[4], "1024": v[5]});
    json!({"probe": 1, "providers": ["cuda", "cpu"], "error": null, "results": [
        {"provider": "cpu", "gpu": null, "ok": true, "rows_per_s": 1000.0, "rows_per_s_by_batch": rows([1000.0; 6])},
        {"provider": "cuda", "gpu": {"name": "RTX 4070 Ti", "luid": "000000000000d1f3"}, "ok": true, "rows_per_s": 20000.0,
         "rows_per_s_by_batch": rows([300.0, 2400.0, 7000.0, 16000.0, 24000.0, 24000.0])}]})
}

fn cuda_pack(dir: &Path, key: &SigningKey) -> String {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join("onnxruntime.dll"), b"a small dll").unwrap();
    let mut t = template(Kind::Runtime, "cuda-test");
    t.min_agent = Some("0.2.10".into());
    t.gpu = Some(GpuNeed { vendor: "nvidia".into(), min_driver: Some("527.41".into()), providers: vec!["cuda".into()] });
    zerofarm_common::sign::sign_dir(dir, t, key).unwrap().0
}

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

fn gpu_lease(h: &H, g: &Gpus, id: &str, fake: &Value) -> Value {
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
    l["rolling"] = json!({"block": [0, 1_000_000], "inflight": {"cpu": 8, "gpu": "auto"}, "until": zerofarm_common::now_unix() + 7200, "max_s": 7200, "flush_s": 10});
    l
}

fn credits(h: &H, id: &str) -> (u64, u64) {
    h.mock.with(|s| s.credits.iter().filter(|(l, ..)| l == id).fold((0, 0), |(c, a), (_, _, x, y)| (c + x, a + y)))
}

fn unfinished_n(done: &Value) -> u64 {
    done["unfinished"]["gids"].as_array().map_or(0, |u| u.iter().map(|r| r[1].as_u64().unwrap() - r[0].as_u64().unwrap()).sum())
}

fn job_lines(h: &H) -> Vec<Value> {
    (0..).map_while(|n| h.job_line_n(n)).collect()
}

/// An owner machine as set with `farm-agent set owner on autotune off workers 6 worker_inflight 128 worker_groups 2
/// worker_threads 2 gpu_reserve_mb 2048` (the test machine's sizes: a 12 GB GPU, 32 GB of RAM): the six workers start
/// as set - more than friends' 3 -, and play on while the owner types, a fullscreen app runs and another program (the
/// trainer) keeps the GPU 90 % busy past the busy gate's 30 s: nothing is yielded.
#[test]
fn an_owner_machine_runs_its_fixed_crew_beside_a_busy_gpu_and_the_owner() {
    let mut h = H::new();
    h.version = "0.3.4".into();
    h.signals.real_starvation = false;
    h.gpus = vec![nvidia(12_282)];
    {
        let mut m = h.machine.lock().unwrap();
        let said: Vec<String> = [("owner", "on"), ("autotune", "off"), ("workers", "6"), ("worker_inflight", "128"), ("worker_groups", "2"), ("worker_threads", "2"), ("gpu_reserve_mb", "2048")]
            .iter()
            .map(|(k, v)| m.settings.set_by_name(k, v).unwrap())
            .collect();
        assert_eq!(said.len(), 7);
    }
    // the owner at the keyboard, a fullscreen app, the trainer on the GPU - from before the lease
    h.user_active(true);
    h.signals.set(|s| {
        s.notification = Notification::D3dFullscreen;
        s.gpu = Some(GpuSignal { other_util_pct: 90, temp_c: Some(65) });
    });
    let pack = cuda_pack(&h.dir.path().join("pack"), &h.key.key);
    h.mock.add_build_dir(&h.dir.path().join("pack"));
    let alt = cuda_worker(&h.dir.path().join("wcuda"), &h.key.key, &pack);
    h.mock.add_build_dir(&h.dir.path().join("wcuda"));
    let g = Gpus { pack, alt };
    // this PC measured the run's memory per game with the CUDA build
    std::fs::create_dir_all(h.layout.state()).unwrap();
    std::fs::write(h.layout.state().join("rolling_memory.json"), json!({format!("{RUN}/{}/gpu", g.alt): 3}).to_string()).unwrap();
    let fake = json!({"game_ms": 40, "shard_every": 4, "flush_ms": 300, "status_ms": 100, "provider": "cuda",
                      "vram_base_mb": 600, "vram_per_row_mb": 1, "rate": {"leaves_s": 1000, "best": {}}});
    h.push(gpu_lease(&h, &g, "l_OW", &fake));
    h.start();
    assert!(wait(|| job_lines(&h).len() >= 6), "six workers: {}", h.agent_log());
    for j in &job_lines(&h)[..6] {
        assert_eq!(j["providers"], json!(["cuda", "cpu"]), "a GPU job: {j}");
        assert_eq!((j["conc"].as_u64(), j["groups"].as_u64(), j["threads"].as_u64()), (Some(128), Some(2), Some(2)), "as set: {j}");
    }
    assert!(h.agent_log().contains("(2048 MB kept for them: gpu_reserve_mb)"), "{}", h.agent_log());
    // past the busy gate's 30 s: no worker yielded, the crew still plays
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(34) {
        std::thread::sleep(Duration::from_millis(500));
    }
    let (before, _) = credits(&h, "l_OW");
    assert!(wait(|| credits(&h, "l_OW").0 > before), "games still credited after 34 s");
    let log = h.agent_log();
    assert!(!log.lines().any(|l| l.contains("yielded")), "nothing yielded: {}", log.lines().filter(|l| l.contains("yield")).collect::<Vec<_>>().join("\n"));
    assert_eq!(h.phase(), farm_agent::status::Phase::Working);
    h.stop();
    let done = h.wait_done("l_OW");
    let (credited, again) = credits(&h, "l_OW");
    assert_eq!(again, 0, "an id played twice: {done}");
    assert_eq!(credited + unfinished_n(&done), done["stats"]["released"].as_u64().unwrap(), "{done}");
    assert!(h.crashes().is_empty(), "{:?}", h.crashes());
}

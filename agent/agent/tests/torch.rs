//! PROTOCOL 19: the torch runtime pack (an opt-in pack: `gpu` NVIDIA with `torch-cuda`, `opt_in: "torch"`) and a torch
//! worker build (`zf-pyshim.exe` + `worker_main.py`), with the real engine, the real shim, the fakeworker standing in for
//! the pack's `python\python.exe` (its python mode: the torch probe's report from the pack's `torch_probe.json`, and an
//! aw/1 worker behind the shim) and the mock coordinator. The pack's probe runs before the first job line (the pack's
//! python, not `probe-providers`), the job runs `torch-cuda` on the adapter it passed on as ONE worker process - an owner
//! machine's fixed crew of 6 notwithstanding - with `ZEROFARM_RUNTIME_DIR` and `CUDA_VISIBLE_DEVICES` set; a pack whose
//! probe fails refuses the lease (no CPU fallback) and is removed.
// GPU work: Windows only (no GPU jobs on macOS)
#![cfg(windows)]

mod harness;

use farm_agent::policy::GpuSignal;
use harness::{unredact, wait, H};
use serde_json::{json, Value};
use std::path::Path;
use std::time::Duration;
use zerofarm_common::manifest::{GpuNeed, Kind, OPT_IN_MIN_AGENT, TORCH_PROVIDER};
use zerofarm_common::protocol::Gpu;
use zerofarm_common::sign::{template, SigningKey};

const RUN: &str = "torch-run";
const UUID: &str = "0a1b2c3d-4e5f-6071-8293-a4b5c6d7e8f9";

fn nvidia() -> Gpu {
    Gpu { vendor: "nvidia".into(), name: "NVIDIA Test GPU".into(), vram_mb: 12_282, driver: "581.57".into(), luid: Some("000000000000d1f3".into()) }
}

fn probe_passes() -> Value {
    json!({"probe": "torch/1", "python": "3.13.14", "torch": "2.11.0+cu128", "cuda": "12.8", "available": true,
           "devices": [{"index": 0, "name": "NVIDIA Test GPU", "uuid": UUID, "total_mb": 12282, "cc": "8.9", "ok": true, "max_abs": 1e-7}]})
}

/// The torch pack: `python\python.exe` (the fakeworker) and its probe report, signed with `gpu` (NVIDIA, torch-cuda),
/// `opt_in` torch and `min_agent` 0.3.19.
fn torch_pack(dir: &Path, key: &SigningKey, probe: &Value) -> String {
    let py = dir.join("python");
    std::fs::create_dir_all(&py).unwrap();
    std::fs::copy(mockcoord::testkit::bin("fakeworker"), py.join("python.exe")).unwrap();
    std::fs::write(py.join("torch_probe.json"), probe.to_string()).unwrap();
    std::fs::write(py.join("python313._pth"), "python313.zip\n.\nLib\\site-packages\n").unwrap();
    // empty files, as a real Python has many (`__init__.py`): agents before 0.3.21 could not download one
    for pkg in ["pkg", "pkg/sub"] {
        let d = py.join("Lib").join("site-packages").join(pkg);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("__init__.py"), b"").unwrap();
    }
    let mut t = template(Kind::Runtime, "torch-2.11.0-cu128-py313");
    t.min_agent = Some(OPT_IN_MIN_AGENT.into());
    t.gpu = Some(GpuNeed { vendor: "nvidia".into(), min_driver: Some("570.65".into()), providers: vec![TORCH_PROVIDER.into()] });
    t.opt_in = Some("torch".into());
    zerofarm_common::sign::sign_dir(dir, t, key).unwrap().0
}

/// The torch worker build: the shim as `entry`, `worker_main.py` next to it (lane A's python; the fakeworker ignores it).
fn torch_worker(dir: &Path, key: &SigningKey, pack: &str) -> String {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::copy(mockcoord::testkit::bin("zf-pyshim"), dir.join("zf-pyshim.exe")).unwrap();
    std::fs::write(dir.join("worker_main.py"), "# the torch worker: zero-core's self-play behind aw/1\n").unwrap();
    let mut t = template(Kind::Worker, "torch-fake");
    t.game = Some("fake".into());
    t.aw = Some(1);
    t.profile = Some("release".into());
    t.entry = Some("zf-pyshim.exe".into());
    t.requires_runtime = Some(pack.into());
    t.min_agent = Some(OPT_IN_MIN_AGENT.into());
    zerofarm_common::sign::sign_dir(dir, t, key).unwrap().0
}

struct Torch {
    pack: String,
    worker: String,
}

fn setup(h: &H, probe: &Value) -> Torch {
    let pack = torch_pack(&h.dir.path().join("torchpack"), &h.key.key, probe);
    h.mock.add_build_dir(&h.dir.path().join("torchpack"));
    let worker = torch_worker(&h.dir.path().join("torchworker"), &h.key.key, &pack);
    h.mock.add_build_dir(&h.dir.path().join("torchworker"));
    Torch { pack, worker }
}

/// A rolling self-play lease of the torch alternate (its variant's file is the gen's model here: the fakeworker only
/// checks the hash).
fn torch_lease(h: &H, t: &Torch, id: &str, fake: &Value) -> Value {
    let (m, r) = (format!("torch-weights/1 {RUN}").into_bytes(), format!("reference rows {RUN}").into_bytes());
    let (ml, rl) = (m.len(), r.len());
    let model = format!("ckpt/{RUN}/1/model.onnx");
    let reference = format!("ckpt/{RUN}/1/reference.npz");
    let sha = h.mock.add_artifact(&model, m);
    let rsha = h.mock.add_artifact(&reference, r);
    let def = json!({"definition": 1, "kind": "run", "id": RUN, "version": 1, "created": zerofarm_common::now_unix() - 60, "expires": null,
        "game": "fake", "builds": [h.build_id, t.worker], "runtime": null,
        "checkpoints": {"1": {"model_sha256": sha, "model_size": ml, "reference_sha256": rsha, "reference_size": rl}},
        "jobs": {"selfplay": {"settings": {"sims": 8, "fake": fake}}}});
    let dref = h.define(&def);
    let mut l = h.lease(id, json!({"kind": "selfplay", "gid_base": 0, "games": 8, "seed": 7}));
    l["run"] = json!(RUN);
    l["definition"] = dref;
    l["checkpoint"] = json!({"gen": 1, "model": model, "sha256": sha, "reference": reference, "reference_sha256": rsha});
    l["build"] = json!({"build_id": t.worker, "manifest": format!("builds/{}/manifest.json", t.worker)});
    l["runtime"] = json!({"build_id": t.pack, "manifest": format!("builds/{}/manifest.json", t.pack)});
    l["providers"] = json!([TORCH_PROVIDER, "cuda", "cpu"]);
    l["rolling"] = json!({"block": [0, 1_000_000], "inflight": {"cpu": 8, "gpu": 512}, "until": zerofarm_common::now_unix() + 7200, "max_s": 7200, "flush_s": 10});
    l
}

fn job_lines(h: &H) -> Vec<Value> {
    (0..).map_while(|n| h.job_line_n(n)).collect()
}

fn credits(h: &H, id: &str) -> (u64, u64) {
    h.mock.with(|s| s.credits.iter().filter(|(l, ..)| l == id).fold((0, 0), |(c, a), (_, _, x, y)| (c + x, a + y)))
}

fn unfinished_n(done: &Value) -> u64 {
    done["unfinished"]["gids"].as_array().map_or(0, |u| u.iter().map(|r| r[1].as_u64().unwrap() - r[0].as_u64().unwrap()).sum())
}

/// The fakeworker's python-mode lines (`fakeworker: python ...`), unredacted.
fn python_lines(h: &H) -> Vec<String> {
    h.agent_log().lines().filter(|l| l.contains("fakeworker: python ")).map(unredact).collect()
}

fn owner_machine(h: &H) {
    let mut m = h.machine.lock().unwrap();
    for (k, v) in [("owner", "on"), ("autotune", "off"), ("workers", "6"), ("worker_threads", "2"), ("gpu_reserve_mb", "2048")] {
        m.settings.set_by_name(k, v).unwrap();
    }
}

/// An owner machine set for a crew of 6 GPU workers gets a torch lease: the pack's python probes PyTorch's CUDA before the
/// first job line, then ONE worker - the shim, running the pack's python on the build's worker_main.py - plays the job
/// with torch-cuda on the adapter the probe passed on, `ZEROFARM_RUNTIME_DIR` the pack, `CUDA_VISIBLE_DEVICES` its device;
/// every id credited or unfinished once, no crash.
#[test]
fn a_torch_job_is_one_worker_process_behind_the_shim_on_the_packs_python() {
    let mut h = H::new();
    h.version = OPT_IN_MIN_AGENT.into();
    h.signals.real_starvation = false;
    h.gpus = vec![nvidia()];
    owner_machine(&h);
    h.signals.set(|s| s.gpu = Some(GpuSignal { other_util_pct: 0, temp_c: Some(60) }));
    let t = setup(&h, &probe_passes());
    // this PC measured the run's memory per game with the torch build (so the memory plan holds the run's 512)
    std::fs::create_dir_all(h.layout.state()).unwrap();
    std::fs::write(h.layout.state().join("rolling_memory.json"), json!({format!("{RUN}/{}/gpu", t.worker): 3}).to_string()).unwrap();
    let fake = json!({"game_ms": 40, "shard_every": 4, "flush_ms": 300, "status_ms": 100, "provider": TORCH_PROVIDER,
                      "vram_base_mb": 1500, "vram_per_row_mb": 1});
    h.push(torch_lease(&h, &t, "l_T1", &fake));
    h.start();
    assert!(wait(|| credits(&h, "l_T1").0 >= 8), "games credited: {}", h.agent_log());
    // the pack's empty files were installed (0.3.21): present under their names, zero bytes
    for pkg in ["pkg", "pkg\\sub"] {
        let init = h.layout.cache_builds().join(&t.pack).join("python\\Lib\\site-packages").join(pkg).join("__init__.py");
        assert_eq!(std::fs::metadata(&init).map(|m| m.len()).ok(), Some(0), "{}", init.display());
    }
    // the probe: the pack's python with the probe's flags, before the first job line; recorded as torch-cuda
    let log = h.agent_log();
    let probe_at = log.find("fakeworker: python -I -B -X utf8 script=<code>").unwrap_or_else(|| panic!("the torch probe ran: {log}"));
    assert!(log.find("fakeworker: job ").unwrap() > probe_at, "probed before the first job line");
    assert!(log.contains(&format!("torch probe of runtime pack {}: torch-cuda", t.pack)), "{log}");
    assert!(!log.contains("probe-providers"), "no ONNX Runtime probe or bench for a torch pack");
    let akey = farm_agent::probe::adapters_key(&h.gpus);
    let p = farm_agent::probe::find(&farm_agent::probe::load(&h.layout), &t.pack, &akey).cloned().expect("the probe is recorded");
    assert_eq!(p.passed(), vec![TORCH_PROVIDER.to_string()]);
    // the worker: the shim ran the pack's python on worker_main.py with the interpreter flags, the pack's directory and
    // the device by UUID in its environment
    let lines = python_lines(&h);
    let worker = lines.iter().find(|l| l.contains("-u script=")).unwrap_or_else(|| panic!("the worker ran behind the shim: {lines:?}"));
    // (paths as the log has them - redacted, short or long names - so compared by their cache part)
    assert!(worker.contains("fakeworker: python -I -B -X utf8 -u script="), "{worker}");
    assert!(worker.contains(&format!("\\cache\\builds\\{}\\worker_main.py runtime_dir=", t.worker)), "the script next to the shim: {worker}");
    assert!(worker.contains(&format!("\\cache\\builds\\{} cuda_visible_devices=", t.pack)), "the pack's directory: {worker}");
    assert!(worker.ends_with(&format!("cuda_visible_devices=GPU-{UUID}")), "{worker}");
    // one worker process, whatever the owner's `workers 6`; torch-cuda on the probe's adapter; the run's 512 in the
    // template's one group, the owner's 2 search threads
    std::thread::sleep(Duration::from_secs(3));
    let jobs = job_lines(&h);
    assert_eq!(jobs.len(), 1, "one worker: {jobs:?}");
    let j = &jobs[0];
    assert_eq!(j["providers"], json!([TORCH_PROVIDER]), "{j}");
    assert_eq!(j["gpu"]["luid"], "000000000000d1f3", "{j}");
    assert_eq!((j["conc"].as_u64(), j["groups"].as_u64(), j["threads"].as_u64()), (Some(512), Some(1), Some(2)), "{j}");
    assert_eq!(lines.iter().filter(|l| l.contains("-u script=")).count(), 1, "one python: {lines:?}");
    h.stop();
    let done = h.wait_done("l_T1");
    assert!(done.get("refused").is_none_or(Value::is_null), "{done}");
    let (credited, again) = credits(&h, "l_T1");
    assert_eq!(again, 0, "an id played twice: {done}");
    assert_eq!(credited + unfinished_n(&done), done["stats"]["released"].as_u64().unwrap(), "{done}");
    assert!(h.crashes().is_empty(), "{:?}", h.crashes());
    // both builds still verify: nothing was written into them (python's -B)
    let keys = zerofarm_common::keys::TrustedKeys::parse(&[h.key.pubkey.clone()]).unwrap();
    for (id, kind) in [(&t.worker, Kind::Worker), (&t.pack, Kind::Runtime)] {
        let e = zerofarm_common::manifest::Expect { build_id: Some(id), kind: Some(kind), version: None };
        zerofarm_common::manifest::verify_dir(&h.layout.cache_builds().join(id), &keys, &e).unwrap();
    }
}

/// Agent 0.3.22 (PROTOCOL 19.5): a torch worker's base is measured - its commit at its job's first self-test, before any
/// game - and remembered for the run and worker build although it is far from the plan's (0.3.6-0.3.21 kept the
/// constant 1,800 MB unless a reading was lower, and charged a torch worker's GBs to its games); the cost per game is
/// counted over it from then on.
#[test]
fn a_torch_workers_base_is_measured_at_its_first_selftest() {
    let mut h = H::new();
    h.version = OPT_IN_MIN_AGENT.into();
    h.signals.real_starvation = false;
    h.gpus = vec![nvidia()];
    owner_machine(&h);
    h.signals.set(|s| s.gpu = Some(GpuSignal { other_util_pct: 0, temp_c: Some(60) }));
    let t = setup(&h, &probe_passes());
    let (gpu_key, base_key) = (format!("{RUN}/{}/gpu", t.worker), format!("{RUN}/{}/gpu_base", t.worker));
    // the run's last build measured 3 MB a game here (over the base of then); this build is new
    std::fs::create_dir_all(h.layout.state()).unwrap();
    std::fs::write(h.layout.state().join("rolling_memory.json"), json!({format!("{RUN}/0123456789abcdef01234567/gpu"): 3}).to_string()).unwrap();
    // 700 MB before its games (committed before its self-test, as a real worker's runtime and network), 1 MB a game
    let fake = json!({"game_ms": 40, "shard_every": 4, "flush_ms": 300, "status_ms": 100, "provider": TORCH_PROVIDER,
                      "mem_base_mb": 700, "mem_base_first": true, "mem_per_game_mb": 1});
    h.push(torch_lease(&h, &t, "l_TB", &fake));
    h.start();
    let learned = || -> Value { std::fs::read(h.layout.state().join("rolling_memory.json")).map(|b| serde_json::from_slice(&b).unwrap()).unwrap_or(json!(null)) };
    assert!(wait(|| learned()[&base_key].is_u64()), "a base remembered: {} {}", learned(), h.agent_log());
    let m = learned();
    let base = m[&base_key].as_u64().unwrap();
    assert!((700..=800).contains(&base), "the worker's 700 MB and its own few: {m} {}", h.agent_log());
    let log = h.agent_log();
    let line = log.lines().find(|l| l.contains("games in flight in") && l.contains("MB per game")).unwrap().to_string();
    let torch_base = farm_agent::rolling::TORCH_BASE_MB;
    assert!(line.contains(&format!("{torch_base} MB + 3 MB per game, measured here with the run's worker build 0123456789abcdef01234567")), "{line}");
    assert!(log.contains("before its games: planning a base of"), "{log}");
    // counted over the measured base: the same commit for the 512 games in flight, split again - (4,096 + 3 x 512 -
    // (base + 1,024)) / 512, rounded up
    let planned = base + farm_agent::rolling::TORCH_BASE_DRIFT_MB;
    let per = (torch_base + 3 * 512 - planned).div_ceil(512);
    assert!(wait(|| learned()[&gpu_key].is_u64()), "{}", learned());
    assert!(learned()[&gpu_key].as_u64().unwrap() <= per, "at most {per} (a steady reading lowers it): {}", learned());
    assert!(learned()[format!("{RUN}/{}/t", t.worker)].is_u64(), "stamped: {}", learned());
    assert_eq!(job_lines(&h)[0]["conc"].as_u64(), Some(512));
    h.stop();
    let done = h.wait_done("l_TB");
    assert!(done.get("refused").is_none_or(Value::is_null), "{done}");
    assert!(h.crashes().is_empty(), "{:?}", h.crashes());
}

/// A torch pack whose probe finds no usable CUDA refuses the lease (`refused: "runtime"`: a torch worker never runs on
/// the CPU), is removed after it, and the next lease is refused before anything is downloaded.
#[test]
fn a_torch_pack_whose_probe_fails_refuses_the_lease_and_is_removed() {
    let mut h = H::new();
    h.version = OPT_IN_MIN_AGENT.into();
    h.timings.start_backoff = Duration::from_millis(150);
    h.timings.start_backoff_max = Duration::from_millis(300);
    h.gpus = vec![nvidia()];
    h.signals.set(|s| s.gpu = Some(GpuSignal { other_util_pct: 0, temp_c: Some(60) }));
    let t = setup(&h, &json!({"probe": "torch/1", "python": "3.13.14", "torch": "2.11.0+cu128", "cuda": "12.8", "available": false, "devices": []}));
    let fake = json!({"game_ms": 40, "provider": TORCH_PROVIDER});
    h.push(torch_lease(&h, &t, "l_T2", &fake));
    h.start();
    let done = h.wait_done("l_T2");
    assert_eq!(done["refused"], "runtime", "{done}");
    let why = done["stats"]["error"].as_str().unwrap_or("");
    assert!(why.contains("PyTorch's CUDA check failed"), "{why}");
    assert!(job_lines(&h).is_empty(), "no worker started");
    assert!(wait(|| !h.layout.cache_builds().join(&t.pack).exists()), "the failed pack is removed");
    h.push(torch_lease(&h, &t, "l_T3", &fake));
    let done = h.wait_done("l_T3");
    assert_eq!(done["refused"], "runtime", "{done}");
    assert!(done["stats"]["error"].as_str().unwrap_or("").contains("failed on this PC"), "{done}");
    assert!(!h.layout.cache_builds().join(&t.pack).exists(), "not downloaded again");
    h.stop();
}

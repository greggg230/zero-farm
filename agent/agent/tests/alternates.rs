//! Rev 11 (PROTOCOL 9): runtime packs per GPU vendor, with the real engine, the fakeworker and the mock
//! coordinator. A lease whose runtime is an alternate (the pack its worker build requires, not the definition's)
//! or whose pack declares a GPU requirement passes the agent's guard before anything but the two manifests is
//! downloaded: a PC without the vendor's adapter, a driver too old, GPU work not allowed, a fuzz job or a pack
//! that failed here before refuses it (`done` with `refused: "runtime"`). An accepted pack is fetched through its
//! zstd copies and unpacked byte for byte.
// GPU runtime packs and alternates: Windows only (no GPU work on macOS in rev 17)
#![cfg(windows)]

mod harness;

use farm_agent::policy::GpuSignal;
use harness::{wait, H};
use serde_json::{json, Value};
use std::path::Path;
use std::time::Duration;
use zerofarm_common::manifest::{GpuNeed, Kind};
use zerofarm_common::protocol::Gpu;
use zerofarm_common::sign::{list_files, sign_dir_zst, template, SigningKey};

const RUN: &str = "cuda-run";

fn nvidia(driver: &str) -> Gpu {
    Gpu { vendor: "nvidia".into(), name: "RTX 4070 Ti".into(), vram_mb: 12282, driver: driver.into(), luid: Some("000000000000d1f3".into()) }
}

fn amd() -> Gpu {
    Gpu { vendor: "amd".into(), name: "RX 9070 XT".into(), vram_mb: 16304, driver: "32.0.21001.9024".into(), luid: Some("00000000000000a1".into()) }
}

/// A CUDA-like runtime pack (two "DLLs", one big enough to get a zstd copy), signed with `gpu` and `min_agent`
/// 0.2.10, its compressed copies next to it. Returns (build id, the big file's bytes).
fn cuda_pack(dir: &Path, key: &SigningKey) -> (String, Vec<u8>) {
    std::fs::create_dir_all(dir).unwrap();
    let big: Vec<u8> = (0..3_000_000u32).map(|i| ((i % 253) as u8) ^ ((i / 9000) as u8)).collect();
    std::fs::write(dir.join("cublasLt64_12.dll"), &big).unwrap();
    std::fs::write(dir.join("onnxruntime.dll"), b"a small dll").unwrap();
    let mut zst = std::collections::BTreeMap::new();
    for f in list_files(dir).unwrap() {
        if f.size > 64 << 10 {
            let mut zd = dir.as_os_str().to_owned();
            zd.push(".zst");
            let z = Path::new(&zd).join(format!("{}.zst", f.path));
            let _ = z.parent().map(std::fs::create_dir_all);
            zst.insert(f.path.clone(), zerofarm_common::zst::pack(&dir.join(&f.path), &z, 3, 2).unwrap());
        }
    }
    let mut t = template(Kind::Runtime, "cuda-test");
    t.min_agent = Some("0.2.10".into());
    t.gpu = Some(GpuNeed { vendor: "nvidia".into(), min_driver: Some("527.41".into()), providers: vec!["cuda".into()] });
    (sign_dir_zst(dir, t, key, &zst).unwrap().0, big)
}

/// The fakeworker signed again with `requires_runtime` = `pack` (one signed worker manifest per pack, 9.1).
fn worker_for(dir: &Path, key: &SigningKey, pack: &str, probe: Option<&Value>) -> String {
    worker_for2(dir, key, pack, probe, None)
}

/// `probe`: the fakeworker's report of the built-in graph; `on_model`: its report on a job's network (the bench).
fn worker_for2(dir: &Path, key: &SigningKey, pack: &str, probe: Option<&Value>, on_model: Option<&Value>) -> String {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::copy(mockcoord::testkit::bin("fakeworker"), dir.join("fakeworker.exe")).unwrap();
    if let Some(p) = probe {
        std::fs::write(dir.join("probe.json"), p.to_string()).unwrap();
    }
    if let Some(p) = on_model {
        std::fs::write(dir.join("probe_model.json"), p.to_string()).unwrap();
    }
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

struct Setup {
    pack: String,
    alt: String,
    big: Vec<u8>,
    dref: Value,
    ckpt: Value,
}

/// A run whose definition lists the default build (h.build_id, no runtime) and the CUDA build; the lease below
/// carries the CUDA build and its pack (an alternate).
fn setup(h: &H) -> Setup {
    setup_with(h, None, 4)
}

/// The probe report of a PC where CUDA passes (the canary's measured rates, phase 4) and the CPU's flat 1,000 rows/s.
fn cuda_passes() -> Value {
    let rows = |v: [f64; 6]| json!({"1": v[0], "8": v[1], "32": v[2], "128": v[3], "512": v[4], "1024": v[5]});
    json!({"probe": 1, "providers": ["cuda", "cpu"], "error": null, "results": [
        {"provider": "cpu", "gpu": null, "ok": true, "rows_per_s": 1000.0, "rows_per_s_by_batch": rows([1000.0; 6])},
        {"provider": "cuda", "gpu": {"name": "RTX 4070 Ti", "luid": "000000000000d1f3"}, "ok": true, "rows_per_s": 20000.0,
         "rows_per_s_by_batch": rows([300.0, 2400.0, 7000.0, 16000.0, 24000.0, 24000.0])}]})
}

fn setup_with(h: &H, probe: Option<&Value>, conc: u64) -> Setup {
    setup_with2(h, probe, None, conc)
}

fn setup_with2(h: &H, probe: Option<&Value>, on_model: Option<&Value>, conc: u64) -> Setup {
    setup_with3(h, probe, on_model, conc, json!({"game_ms": 5, "shard_every": 8}))
}

/// `fake`: the jobs' fakeworker script.
fn setup_with3(h: &H, probe: Option<&Value>, on_model: Option<&Value>, conc: u64, fake: Value) -> Setup {
    let (pack, big) = cuda_pack(&h.dir.path().join("pack"), &h.key.key);
    setup_pack(h, pack, big, probe, on_model, conc, fake)
}

/// The run of `setup_with3` around a pack already signed into `h.dir/pack`.
fn setup_pack(h: &H, pack: String, big: Vec<u8>, probe: Option<&Value>, on_model: Option<&Value>, conc: u64, fake: Value) -> Setup {
    h.mock.add_build_dir(&h.dir.path().join("pack"));
    let alt = worker_for2(&h.dir.path().join("wcuda"), &h.key.key, &pack, probe, on_model);
    h.mock.add_build_dir(&h.dir.path().join("wcuda"));
    let (m, r) = (b"onnx graph cuda 1".to_vec(), b"reference rows cuda 1".to_vec());
    let (ml, rl) = (m.len(), r.len());
    let sha = h.mock.add_artifact("ckpt/cuda-run/1/model.onnx", m);
    let rsha = h.mock.add_artifact("ckpt/cuda-run/1/reference.npz", r);
    let def = json!({"definition": 1, "kind": "run", "id": RUN, "version": 1, "created": zerofarm_common::now_unix() - 60, "expires": null,
        "game": "fake", "builds": [h.build_id, alt], "runtime": null,
        "checkpoints": {"1": {"model_sha256": sha, "model_size": ml, "reference_sha256": rsha, "reference_size": rl}},
        "jobs": {"selfplay": {"settings": {"sims": 8, "conc": conc, "fake": fake}}}});
    let dref = h.define(&def);
    let ckpt = json!({"gen": 1, "model": "ckpt/cuda-run/1/model.onnx", "sha256": sha, "reference": "ckpt/cuda-run/1/reference.npz", "reference_sha256": rsha});
    Setup { pack, alt, big, dref, ckpt }
}

fn alt_lease(h: &H, s: &Setup, id: &str, kind: &str) -> Value {
    alt_lease_n(h, s, id, kind, 4)
}

fn alt_lease_n(h: &H, s: &Setup, id: &str, kind: &str, games: u64) -> Value {
    let job = if kind == "fuzz" { json!({"kind": "fuzz", "first": 1, "count": 10}) } else { json!({"kind": kind, "gid_base": 0, "games": games, "seed": 3}) };
    let mut l = h.lease(id, job);
    l["run"] = json!(RUN);
    l["definition"] = s.dref.clone();
    l["build"] = json!({"build_id": s.alt, "manifest": format!("builds/{}/manifest.json", s.alt)});
    l["runtime"] = json!({"build_id": s.pack, "manifest": format!("builds/{}/manifest.json", s.pack)});
    if kind == "selfplay" {
        l["checkpoint"] = s.ckpt.clone();
    }
    l
}

/// Paths of the pack's files (not its manifest) the agent asked the coordinator for.
fn pack_file_gets(h: &H, pack: &str) -> Vec<String> {
    let prefix = format!("/v1/artifact/builds/{pack}/");
    h.mock.with(|st| st.log.iter().filter(|l| l.method == "GET" && l.path.starts_with(&prefix) && !l.path.contains("manifest.json")).map(|l| l.path.clone()).collect())
}

fn gpu_signal(h: &H, on: bool) {
    h.signals.set(|s| s.gpu = on.then_some(GpuSignal { other_util_pct: 0, temp_c: None }));
}

fn quick(h: &mut H) {
    h.timings.start_backoff = Duration::from_millis(150);
    h.timings.start_backoff_max = Duration::from_millis(300);
}

fn refused_runtime(h: &H, id: &str) -> String {
    let done = h.wait_done(id);
    assert_eq!(done["refused"], "runtime", "{done}");
    assert_eq!(done["given_back"], true, "{done}");
    done["stats"]["error"].as_str().unwrap_or("").to_string()
}

#[test]
fn a_pc_without_the_vendors_gpu_never_downloads_the_pack() {
    for (name, gpus) in [("no gpu", vec![]), ("amd", vec![amd()]), ("old driver", vec![nvidia("516.94")])] {
        let mut h = H::new();
        quick(&mut h);
        h.version = "0.2.10".into();
        h.gpus = gpus;
        gpu_signal(&h, true);
        let s = setup(&h);
        h.push(alt_lease(&h, &s, "l_A1", "selfplay"));
        h.start();
        let why = refused_runtime(&h, "l_A1");
        assert!(why.contains("needs a nvidia GPU"), "{name}: {why}");
        assert!(pack_file_gets(&h, &s.pack).is_empty(), "{name}: {:?}", pack_file_gets(&h, &s.pack));
        assert!(!h.layout.cache_builds().join(&s.pack).exists(), "{name}");
        assert!(!h.layout.cache_builds().join(&s.alt).exists(), "{name}: not even the worker build");
        h.stop();
    }
}

#[test]
fn gpu_work_not_allowed_a_fuzz_job_or_a_failed_pack_refuse_it_too() {
    // GPU work not allowed right now (no GPU signal = the policy blocks GPU work)
    let mut h = H::new();
    quick(&mut h);
    h.version = "0.2.10".into();
    h.gpus = vec![nvidia("560.94")];
    gpu_signal(&h, false);
    let s = setup(&h);
    h.push(alt_lease(&h, &s, "l_B1", "selfplay"));
    h.start();
    assert!(refused_runtime(&h, "l_B1").contains("isn't allowed"));
    h.stop();
    assert!(pack_file_gets(&h, &s.pack).is_empty());

    // a fuzz lease with a GPU pack
    let mut h = H::new();
    quick(&mut h);
    h.version = "0.2.10".into();
    h.gpus = vec![nvidia("560.94")];
    gpu_signal(&h, true);
    let s = setup(&h);
    let mut l = alt_lease(&h, &s, "l_B2", "fuzz");
    l["run"] = Value::Null;
    l["campaign"] = json!("fz");
    let cdef = json!({"definition": 1, "kind": "campaign", "id": "fz", "version": 1, "created": zerofarm_common::now_unix() - 60, "expires": null,
        "game": "fake", "builds": [s.alt], "runtime": s.pack, "checkpoints": {}, "jobs": {"fuzz": {"mode": "games"}}});
    l["definition"] = h.define(&cdef);
    h.push(l);
    h.start();
    let done = h.wait_done("l_B2");
    assert!(done["refused"] == "runtime" || done["refused"] == "definition", "{done}");
    h.stop();
    assert!(pack_file_gets(&h, &s.pack).is_empty());

    // a pack that failed here before (its probe for these adapters and drivers passed no cuda)
    let mut h = H::new();
    quick(&mut h);
    h.version = "0.2.10".into();
    h.gpus = vec![nvidia("560.94")];
    gpu_signal(&h, true);
    let s = setup(&h);
    let akey = farm_agent::probe::adapters_key(&h.gpus);
    farm_agent::probe::record(
        &h.layout,
        farm_agent::probe::Probe { runtime: s.pack.clone(), adapters: akey, worker: "w".into(), t: zerofarm_common::now_unix(), providers: vec!["cpu".into()], results: vec![], error: None, need: Some(vec!["cuda".into()]), benched: true, real: vec![], rules: farm_agent::probe::RULES },
    );
    h.push(alt_lease(&h, &s, "l_B3", "selfplay"));
    h.start();
    assert!(refused_runtime(&h, "l_B3").contains("failed on this PC"));
    h.stop();
    assert!(pack_file_gets(&h, &s.pack).is_empty());
}

#[test]
fn a_pack_whose_probe_fails_runs_the_job_on_the_cpu_then_is_removed_and_never_fetched_again() {
    let mut h = H::new();
    quick(&mut h);
    h.version = "0.2.10".into();
    h.gpus = vec![nvidia("560.94")];
    gpu_signal(&h, true);
    let s = setup(&h); // the fakeworker's own probe report: only the CPU passes
    h.push(alt_lease(&h, &s, "l_C1", "selfplay"));
    h.start();
    let done = h.wait_done("l_C1");
    assert!(done.get("refused").is_none_or(Value::is_null), "{done}\n{}", h.agent_log());
    // the big file came as its zstd copy, the small one plain
    let gets = pack_file_gets(&h, &s.pack);
    assert!(gets.iter().any(|p| p.ends_with("cublasLt64_12.dll.zst")), "{gets:?}");
    assert!(!gets.iter().any(|p| p.ends_with("cublasLt64_12.dll")), "{gets:?}");
    // the built-in probe ran before the first job line (the same one as between leases), then the job ran on the CPU
    let log = h.agent_log();
    let probe_at = log.find("fakeworker: probe-providers --runtime-dir").expect("the built-in probe ran");
    assert!(!log[probe_at..].lines().next().unwrap().contains("--model"), "the built-in graph, not the job's");
    // 0.2.11: never AMD's providers, and (no adapter is off here) not pinned to one adapter
    let probe_line = log[probe_at..].lines().next().unwrap();
    assert!(probe_line.contains("--providers tensorrt_rtx,cuda,openvino,dml,cpu") && !probe_line.contains("--gpu"), "{probe_line}");
    assert!(log.find("fakeworker: job ").unwrap() > probe_at, "probed before the first job line");
    // the built-in graph is not the last word: the pack's own provider is tried on the job's network too, and fails there
    let bench_at = log.find("--bench-rows").expect("the job's network was benched");
    assert!(log[bench_at..].lines().next().unwrap().contains("--providers cuda,cpu"), "the pack's own provider despite the built-in FAIL");
    assert_eq!(h.job_line()["providers"], json!(["cpu"]));
    let p = farm_agent::probe::find(&farm_agent::probe::load(&h.layout), &s.pack, &farm_agent::probe::adapters_key(&h.gpus)).cloned().unwrap();
    assert!(p.benched && p.real.is_empty() && p.need == Some(vec!["cuda".to_string()]), "both verdicts recorded: {p:?}");
    // marked bad for this driver, removed from the cache after the lease, refused next time without a download
    assert!(wait(|| !h.layout.cache_builds().join(&s.pack).exists()), "the failed pack is evicted");
    let n = pack_file_gets(&h, &s.pack).len();
    h.push(alt_lease(&h, &s, "l_C2", "selfplay"));
    assert!(refused_runtime(&h, "l_C2").contains("failed on this PC"));
    assert_eq!(pack_file_gets(&h, &s.pack).len(), n, "not downloaded again");
    h.stop();
}

#[test]
fn an_nvidia_pc_fetches_the_pack_compressed_benches_and_runs_cuda_when_it_pays() {
    let mut h = H::new();
    quick(&mut h);
    h.version = "0.2.10".into();
    h.gpus = vec![nvidia("560.94")];
    gpu_signal(&h, true);
    let probe = cuda_passes();
    let s = setup_with(&h, Some(&probe), 8);
    // 128 games: a GPU job runs them all at once (conc 8 raised to 128) -> 128-row batches, CUDA 16x the CPU
    h.push(alt_lease_n(&h, &s, "l_E1", "selfplay", 128));
    h.start();
    let done = h.wait_done("l_E1");
    assert!(done.get("refused").is_none_or(Value::is_null), "{done}\n{}", h.agent_log());
    let dir = h.layout.cache_builds().join(&s.pack);
    assert_eq!(std::fs::read(dir.join("cublasLt64_12.dll")).unwrap(), s.big, "unpacked byte for byte");
    assert!(!dir.join("cublasLt64_12.dll.zst").exists(), "only the manifest's files stay in the build directory");
    let log = h.agent_log();
    assert!(log.contains("--bench-rows 1,8,32,128,512,1024") && log.contains("--providers cuda,cpu"), "the job's network was benched");
    let line = h.job_line();
    assert_eq!(line["providers"], json!(["cuda", "cpu"]), "{line}");
    assert_eq!(line["gpu"]["luid"], "000000000000d1f3");
    assert_eq!(line["conc"], 128, "{line}");
    assert!(line.get("groups").is_none(), "the template's groups (1) are kept: {line}");
    assert_eq!(done["stats"]["batch_rows"], 128, "{done}");
    assert!(done["stats"]["rows_per_s"]["cuda"].as_f64().unwrap() > done["stats"]["rows_per_s"]["cpu"].as_f64().unwrap() * 10.0, "{done}");
    // the pack stays (it passed), and the next lease request tells the coordinator from how many games CUDA pays here
    assert!(dir.exists());
    let caps = h.mock.wait(harness::T, |st| st.lease_requests.iter().any(|r| r["caps"]["gpu_games"][RUN].is_u64()));
    assert!(caps, "caps.gpu_games: {:?}", h.mock.with(|st| st.lease_requests.last().cloned()));
    let req = h.mock.with(|st| st.lease_requests.iter().rev().find(|r| r["caps"]["gpu_games"][RUN].is_u64()).cloned().unwrap());
    let g = req["caps"]["gpu_games"][RUN].as_u64().unwrap();
    assert!((1..=32).contains(&g), "CUDA reaches 2x the CPU at a few rows: {g}");
    assert_eq!(req["caps"]["gpu_allowed"], true);
    h.stop();
}

/// Agent 0.3.2: a GPU job yields when GPU work stops being allowed - from its start, before its worker has sessions and
/// names its provider (`provider: ""` until then: the agent's choice, CUDA, counts), and once it reports CUDA; a GPU job
/// whose worker reports the CPU (it fell back) plays on.
#[test]
fn a_gpu_job_yields_when_gpu_work_stops_being_allowed_before_and_after_its_sessions_exist() {
    let mut h = H::new();
    quick(&mut h);
    h.version = "0.2.10".into();
    h.gpus = vec![nvidia("560.94")];
    gpu_signal(&h, true);
    let s = setup_with3(&h, Some(&cuda_passes()), None, 8, json!({"game_ms": 60, "shard_every": 8, "build_ms": 3000, "provider": "cuda"}));
    h.push(alt_lease_n(&h, &s, "l_G1", "selfplay", 128));
    h.start();
    assert!(wait(|| h.job_line_n(0).is_some()), "{}", h.agent_log());
    assert_eq!(h.job_line()["providers"], json!(["cuda", "cpu"]), "a CUDA job");
    // the GPU is taken while the worker builds its sessions (it says provider "")
    gpu_signal(&h, false);
    let paused = |h: &H| h.phase() == farm_agent::status::Phase::Paused && h.status.lock().unwrap().detail == "GPU is busy";
    assert!(wait(|| paused(&h)), "{:?}
{}", h.status.lock().unwrap(), h.agent_log());
    assert!(!h.agent_log().contains("sessions built"), "yielded before the worker named its provider: {}", h.agent_log());
    gpu_signal(&h, true);
    assert!(wait(|| h.agent_log().contains("sessions built (cuda)")), "{}", h.agent_log());
    assert!(wait(|| h.phase() == farm_agent::status::Phase::Working));
    // and again once it plays on CUDA
    assert!(wait(|| h.status.lock().unwrap().games_today >= 1), "{}", h.agent_log());
    gpu_signal(&h, false);
    assert!(wait(|| paused(&h)), "{}", h.agent_log());
    gpu_signal(&h, true);
    let done = h.wait_done("l_G1");
    assert!(done.get("refused").is_none_or(Value::is_null), "{done}");
    assert_eq!(done["given_back"], false, "{done}");
    assert!(done["stats"]["yields"].as_u64().unwrap() >= 2, "{done}");
    assert_eq!(done["stats"]["provider"], "cuda", "{done}");
    h.stop();

    // the same CUDA job whose worker reports the CPU: GPU work not allowed doesn't pause it
    let mut h = H::new();
    quick(&mut h);
    h.version = "0.2.10".into();
    h.gpus = vec![nvidia("560.94")];
    gpu_signal(&h, true);
    let s = setup_with3(&h, Some(&cuda_passes()), None, 8, json!({"game_ms": 60, "shard_every": 8, "build_ms": 500, "provider": "cpu"}));
    h.push(alt_lease_n(&h, &s, "l_G2", "selfplay", 128));
    h.start();
    assert!(wait(|| h.agent_log().contains("sessions built (cpu)")), "{}", h.agent_log());
    assert!(wait(|| h.status.lock().unwrap().games_today >= 1));
    gpu_signal(&h, false);
    let done = h.wait_done("l_G2");
    assert_eq!(h.job_line()["providers"], json!(["cuda", "cpu"]));
    assert_eq!(done["given_back"], false, "{done}");
    assert_eq!(done["stats"]["yields"], 0, "a CPU worker plays on: {done}");
    h.stop();
}

#[test]
fn cuda_failing_the_built_in_graph_but_passing_the_jobs_network_is_used_and_kept() {
    // the built-in probe graph's CUDA false negative (duplicate scatter slots): the job's own network decides
    let mut h = H::new();
    quick(&mut h);
    h.version = "0.2.10".into();
    h.gpus = vec![nvidia("560.94")];
    gpu_signal(&h, true);
    let builtin_fails = json!({"probe": 1, "providers": ["cpu"], "error": null, "results": [
        {"provider": "cpu", "gpu": null, "ok": true, "rows_per_s_by_batch": {"1": 1000.0}},
        {"provider": "cuda", "gpu": {"name": "RTX 4070 Ti", "luid": "000000000000d1f3"}, "ok": false,
         "selftest": {"max_abs": 0.0024, "ok": false}}]});
    let s = setup_with2(&h, Some(&builtin_fails), Some(&cuda_passes()), 8);
    h.push(alt_lease_n(&h, &s, "l_F1", "selfplay", 128));
    h.start();
    let done = h.wait_done("l_F1");
    assert!(done.get("refused").is_none_or(Value::is_null), "{done}
{}", h.agent_log());
    let log = h.agent_log();
    assert!(log.contains("built-in probe FAIL, cuda-run's network PASS"), "both verdicts in the worker log:
{log}");
    assert_eq!(h.job_line()["providers"], json!(["cuda", "cpu"]));
    let p = farm_agent::probe::find(&farm_agent::probe::load(&h.layout), &s.pack, &farm_agent::probe::adapters_key(&h.gpus)).cloned().unwrap();
    assert!(p.providers == vec!["cpu"] && p.real == vec!["cuda"] && p.benched, "both verdicts recorded: {p:?}");
    assert!(h.layout.cache_builds().join(&s.pack).exists(), "kept: it passed on a real network");
    // (what the profile then tells the coordinator - runtime_providers [cuda, cpu] - is probe.rs's unit test: the harness
    // profile is a fixed one)
    h.stop();
}

#[test]
fn an_older_runtime_rule_still_refuses_a_runtime_no_listed_build_requires() {
    // the lease's runtime is neither the definition's nor the one its build requires: the definition refuses it
    let mut h = H::new();
    quick(&mut h);
    h.version = "0.2.10".into();
    h.gpus = vec![nvidia("560.94")];
    gpu_signal(&h, true);
    let s = setup(&h);
    let mut l = alt_lease(&h, &s, "l_D1", "selfplay");
    l["build"] = json!({"build_id": h.build_id, "manifest": format!("builds/{}/manifest.json", h.build_id)}); // requires no runtime
    h.push(l);
    h.start();
    let done = h.wait_done("l_D1");
    assert_eq!(done["refused"], "definition", "{done}");
    h.stop();
    assert!(pack_file_gets(&h, &s.pack).is_empty());
    let _ = wait(|| true);
}

// ---------- agent 0.3.11 (PROTOCOL 9.8): AMD GPUs with DirectML only ----------

/// A DirectML-only runtime pack (onnxruntime.dll and DirectML.dll; `extra` more files, e.g. Windows ML's catalog DLL),
/// signed with `gpu` (none: a pack that may run anywhere) and `min_agent` 0.2.10.
fn dml_pack(dir: &Path, key: &SigningKey, gpu: Option<GpuNeed>, extra: &[&str]) -> String {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join("onnxruntime.dll"), b"a small dll").unwrap();
    std::fs::write(dir.join("DirectML.dll"), b"directml").unwrap();
    for f in extra {
        std::fs::write(dir.join(f), b"another dll").unwrap();
    }
    let mut t = template(Kind::Runtime, "dml-test");
    t.min_agent = Some("0.2.10".into());
    t.gpu = gpu;
    zerofarm_common::sign::sign_dir(dir, t, key).unwrap().0
}

fn amd_dml() -> Option<GpuNeed> {
    Some(GpuNeed { vendor: "amd".into(), min_driver: None, providers: vec!["dml".into()] })
}

/// The probe report of an AMD GPU where DirectML passes (fast at big batches) beside the CPU's flat 1,000 rows/s.
fn dml_passes() -> Value {
    let rows = |v: [f64; 6]| json!({"1": v[0], "8": v[1], "32": v[2], "128": v[3], "512": v[4], "1024": v[5]});
    json!({"probe": 1, "providers": ["dml", "cpu"], "error": null, "results": [
        {"provider": "cpu", "gpu": null, "ok": true, "rows_per_s": 1000.0, "rows_per_s_by_batch": rows([1000.0; 6])},
        {"provider": "dml", "gpu": {"name": "RX 9070 XT", "luid": "00000000000000a1"}, "ok": true, "rows_per_s": 9000.0,
         "rows_per_s_by_batch": rows([100.0, 800.0, 3000.0, 8000.0, 12000.0, 12000.0])}]})
}

fn line_of<'a>(log: &'a str, what: &str) -> &'a str {
    let at = log.find(what).unwrap_or_else(|| panic!("no {what:?} in\n{log}"));
    log[at..].lines().next().unwrap()
}

#[test]
fn an_amd_pc_probes_benches_and_plays_directml_alone_from_a_directml_pack() {
    let mut h = H::new();
    quick(&mut h);
    h.version = "0.3.11".into();
    h.gpus = vec![amd()];
    gpu_signal(&h, true);
    let pack = dml_pack(&h.dir.path().join("pack"), &h.key.key, amd_dml(), &[]);
    let s = setup_pack(&h, pack, vec![], Some(&dml_passes()), None, 8, json!({"game_ms": 5, "shard_every": 8}));
    h.push(alt_lease_n(&h, &s, "l_M1", "selfplay", 128));
    h.start();
    let done = h.wait_done("l_M1");
    assert!(done.get("refused").is_none_or(Value::is_null), "{done}\n{}", h.agent_log());
    let log = h.agent_log();
    // the built-in probe: the AMD GPU alone, DirectML and the CPU only, nothing downloaded by Windows ML
    let probe = line_of(&log, "fakeworker: probe-providers --runtime-dir");
    assert!(probe.contains("--providers dml,cpu") && probe.contains("--gpu 00000000000000a1"), "{probe}");
    assert!(!probe.contains("--ensure-ready") && !probe.contains("--model"), "{probe}");
    // the bench of the job's network: the same
    let bench = line_of(&log, "--bench-rows");
    assert!(bench.contains("--providers dml,cpu") && bench.contains("--gpu 00000000000000a1"), "{bench}");
    // DirectML pays at 128-row batches: the job plays on the AMD GPU with DirectML
    let line = h.job_line();
    assert_eq!(line["providers"], json!(["dml", "cpu"]), "{line}");
    assert_eq!((line["gpu"]["luid"].as_str(), line["gpu"]["vendor"].as_str()), (Some("00000000000000a1"), Some("amd")), "{line}");
    // recorded under this agent's rules, and the profile's providers say DirectML (the coordinator may offer the pack again)
    let p = farm_agent::probe::find(&farm_agent::probe::load(&h.layout), &s.pack, &farm_agent::probe::adapters_key(&h.gpus)).cloned().unwrap();
    assert_eq!((p.rules, p.passed()), (farm_agent::probe::RULES, vec!["dml".to_string(), "cpu".to_string()]));
    let req = h.mock.with(|st| st.lease_requests.first().cloned().unwrap());
    assert_eq!(req["caps"]["gpu_allowed"], true, "{req}");
    h.stop();
}

#[test]
fn an_amd_pack_with_another_provider_or_windows_mls_catalog_is_refused_and_so_is_one_whose_vendor_is_off() {
    let cases: [(&str, Option<GpuNeed>, &[&str], &str); 3] = [
        ("migraphx", Some(GpuNeed { vendor: "amd".into(), min_driver: None, providers: vec!["dml".into(), "migraphx".into()] }), &[], "brings migraphx"),
        ("catalog", amd_dml(), &["Microsoft.Windows.AI.MachineLearning.dll"], "Microsoft.Windows.AI.MachineLearning.dll"),
        ("off", amd_dml(), &[], "turned GPU work off"),
    ];
    for (name, gpu, extra, why) in cases {
        let mut h = H::new();
        quick(&mut h);
        h.version = "0.3.11".into();
        h.gpus = vec![amd()];
        gpu_signal(&h, true);
        if name == "off" {
            // the owner's kill switch (rev 22), received with the update check at start
            let l = json!({"version": "0.3.11", "build_id": "a".repeat(24), "manifest": format!("builds/{}/manifest.json", "a".repeat(24)), "min_agent": "0.1.0", "gpu_vendors_off": ["amd"]});
            h.mock.with(|st| st.latest = Some(l));
        }
        let pack = dml_pack(&h.dir.path().join("pack"), &h.key.key, gpu, extra);
        let s = setup_pack(&h, pack, vec![], Some(&dml_passes()), None, 8, json!({"game_ms": 5, "shard_every": 8}));
        h.push(alt_lease(&h, &s, "l_M2", "selfplay"));
        h.start();
        let err = refused_runtime(&h, "l_M2");
        assert!(err.contains(why), "{name}: {err}");
        assert!(pack_file_gets(&h, &s.pack).is_empty(), "{name}: nothing of the pack downloaded");
        assert!(!h.agent_log().contains("probe-providers"), "{name}: nothing probed");
        h.stop();
    }
}

#[test]
fn a_pack_that_ships_windows_mls_catalog_never_touches_an_amd_gpu() {
    // a run whose pack (no `gpu`: the Windows ML pack's kind) ships the catalog, on a PC with an AMD GPU and an Intel iGPU:
    // its probe runs on the Intel GPU alone (as 0.2.11 did); on an AMD-only PC not at all, and its job runs on the CPU
    for (name, gpus) in [("amd + intel", vec![amd(), intel()]), ("amd only", vec![amd()])] {
        let mut h = H::new();
        quick(&mut h);
        h.version = "0.3.11".into();
        h.gpus = gpus;
        gpu_signal(&h, true);
        let pack = dml_pack(&h.dir.path().join("pack"), &h.key.key, None, &["Microsoft.Windows.AI.MachineLearning.dll"]);
        let s = setup_pack(&h, pack, vec![], Some(&dml_passes()), None, 8, json!({"game_ms": 5, "shard_every": 8}));
        h.push(alt_lease_n(&h, &s, "l_M3", "selfplay", 128));
        h.start();
        let done = h.wait_done("l_M3");
        assert!(done.get("refused").is_none_or(Value::is_null), "{name}: {done}\n{}", h.agent_log());
        let log = h.agent_log();
        if name == "amd only" {
            assert!(!log.contains("probe-providers"), "{name}: nothing probed\n{log}");
        } else {
            let probe = line_of(&log, "fakeworker: probe-providers --runtime-dir");
            assert!(probe.contains("--gpu 00000000000000b2") && probe.contains("--providers tensorrt_rtx,cuda,openvino,dml,cpu"), "{name}: {probe}");
        }
        // (the fake report's DirectML pass names the AMD GPU: never chosen from this pack)
        assert_eq!(h.job_line()["providers"], json!(["cpu"]), "{name}");
        h.stop();
    }
}

#[test]
fn the_directml_pack_signed_for_nvidia_runs_directml_on_an_nvidia_pc_as_any_pack_does() {
    // the same DirectML-only pack signed with `gpu` {nvidia, [dml]} (an end-to-end test on an NVIDIA PC): no AMD rule
    // applies - every adapter, every provider a probe may load, the measured choice picks DirectML when it pays
    let mut h = H::new();
    quick(&mut h);
    h.version = "0.3.11".into();
    h.gpus = vec![nvidia("560.94")];
    gpu_signal(&h, true);
    let pack = dml_pack(&h.dir.path().join("pack"), &h.key.key, Some(GpuNeed { vendor: "nvidia".into(), min_driver: None, providers: vec!["dml".into()] }), &[]);
    let report = dml_passes().to_string().replace("RX 9070 XT", "RTX 4070 Ti").replace("00000000000000a1", "000000000000d1f3");
    let s = setup_pack(&h, pack, vec![], Some(&serde_json::from_str(&report).unwrap()), None, 8, json!({"game_ms": 5, "shard_every": 8}));
    h.push(alt_lease_n(&h, &s, "l_M4", "selfplay", 128));
    h.start();
    let done = h.wait_done("l_M4");
    assert!(done.get("refused").is_none_or(Value::is_null), "{done}
{}", h.agent_log());
    let log = h.agent_log();
    let probe = line_of(&log, "fakeworker: probe-providers --runtime-dir");
    assert!(probe.contains("--ensure-ready") && probe.contains("--providers tensorrt_rtx,cuda,openvino,dml,cpu") && !probe.contains("--gpu"), "{probe}");
    assert!(line_of(&log, "--bench-rows").contains("--providers dml,cpu"));
    let line = h.job_line();
    assert_eq!((line["providers"].clone(), line["gpu"]["luid"].as_str()), (json!(["dml", "cpu"]), Some("000000000000d1f3")), "{line}");
    h.stop();
}

fn intel() -> Gpu {
    Gpu { vendor: "intel".into(), name: "UHD 770".into(), vram_mb: 128, driver: "32.0.101.6078".into(), luid: Some("00000000000000b2".into()) }
}

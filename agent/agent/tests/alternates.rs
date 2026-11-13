//! Rev 11 (PROTOCOL 9): runtime packs per GPU vendor, with the real engine, the fakeworker and the mock
//! coordinator. A lease whose runtime is an alternate (the pack its worker build requires, not the definition's)
//! or whose pack declares a GPU requirement passes the agent's guard before anything but the two manifests is
//! downloaded: a PC without the vendor's adapter, a driver too old, GPU work not allowed, a fuzz job or a pack
//! that failed here before refuses it (`done` with `refused: "runtime"`). An accepted pack is fetched through its
//! zstd copies and unpacked byte for byte.

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
    std::fs::create_dir_all(dir).unwrap();
    std::fs::copy(mockcoord::testkit::bin("fakeworker"), dir.join("fakeworker.exe")).unwrap();
    if let Some(p) = probe {
        std::fs::write(dir.join("probe.json"), p.to_string()).unwrap();
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
    let (pack, big) = cuda_pack(&h.dir.path().join("pack"), &h.key.key);
    h.mock.add_build_dir(&h.dir.path().join("pack"));
    let alt = worker_for(&h.dir.path().join("wcuda"), &h.key.key, &pack, probe);
    h.mock.add_build_dir(&h.dir.path().join("wcuda"));
    let (m, r) = (b"onnx graph cuda 1".to_vec(), b"reference rows cuda 1".to_vec());
    let (ml, rl) = (m.len(), r.len());
    let sha = h.mock.add_artifact("ckpt/cuda-run/1/model.onnx", m);
    let rsha = h.mock.add_artifact("ckpt/cuda-run/1/reference.npz", r);
    let def = json!({"definition": 1, "kind": "run", "id": RUN, "version": 1, "created": zerofarm_common::now_unix() - 60, "expires": null,
        "game": "fake", "builds": [h.build_id, alt], "runtime": null,
        "checkpoints": {"1": {"model_sha256": sha, "model_size": ml, "reference_sha256": rsha, "reference_size": rl}},
        "jobs": {"selfplay": {"settings": {"sims": 8, "conc": conc, "fake": {"game_ms": 5, "shard_every": 8}}}}});
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
        farm_agent::probe::Probe { runtime: s.pack.clone(), adapters: akey, worker: "w".into(), t: zerofarm_common::now_unix(), providers: vec!["cpu".into()], results: vec![], error: None },
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
    assert!(log.find("fakeworker: job ").unwrap() > probe_at, "probed before the first job line");
    assert!(!log.contains("--bench-rows"), "nothing to bench: no GPU provider passed");
    assert_eq!(h.job_line()["providers"], json!(["cpu"]));
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

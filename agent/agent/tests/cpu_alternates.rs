//! Agent 0.3.2 (PROTOCOL 9.5, 11.4), with the real engine, the fakeworker and the mock coordinator, on both platforms:
//! a CPU alternate - a run's alternate whose runtime pack has no `gpu` - is CPU work. The agent's guard takes it on a
//! machine where GPU work isn't allowed (every Mac), where 0.3.1 refused it as GPU work ("is for GPU work, which isn't
//! allowed here right now") and reported a crash; a probe here that passed only the CPU doesn't condemn it either. A GPU
//! pack (`gpu`) is still refused there before it is downloaded.

mod harness;

use harness::H;
use mockcoord::testkit::{bin, platform_fields, FAKEWORKER};
use serde_json::{json, Value};
use std::path::Path;
use zerofarm_common::manifest::{GpuNeed, Kind};
use zerofarm_common::sign::{sign_dir, template, SigningKey};

const RUN: &str = "cpu-alt-run";

/// A runtime pack of this platform (one small "library"), with `gpu` or without.
fn pack(dir: &Path, key: &SigningKey, gpu: Option<GpuNeed>) -> String {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join(if cfg!(windows) { "onnxruntime.dll" } else { "libonnxruntime.dylib" }), b"a small library").unwrap();
    let mut t = template(Kind::Runtime, "alt-pack-test");
    // (a pack with `gpu` needs a min_agent that knows it, 9.2)
    t.min_agent = gpu.as_ref().map(|_| "0.2.10".into());
    t.gpu = gpu;
    platform_fields(&mut t);
    sign_dir(dir, t, key).unwrap().0
}

/// The fakeworker signed again with `requires_runtime` = `pack` (one signed worker manifest per pack, 9.1).
fn worker_for(dir: &Path, key: &SigningKey, pack: &str) -> String {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::copy(bin("fakeworker"), dir.join(FAKEWORKER)).unwrap();
    let mut t = template(Kind::Worker, "fake-alt");
    t.game = Some("fake".into());
    t.aw = Some(1);
    t.zero_onnx = Some(vec![1]);
    t.profile = Some("release".into());
    t.entry = Some(FAKEWORKER.into());
    t.requires_runtime = Some(pack.into());
    platform_fields(&mut t);
    sign_dir(dir, t, key).unwrap().0
}

/// The pack (with `gpu` or without) and its worker build served, and a self-play lease of a run whose definition lists
/// the default build (no runtime) and that worker build: the lease carries the alternate. Returns (lease, pack).
fn alt_lease(h: &H, id: &str, gpu: Option<GpuNeed>) -> (Value, String) {
    let pack = pack(&h.dir.path().join("pack"), &h.key.key, gpu);
    h.mock.add_build_dir(&h.dir.path().join("pack"));
    let alt = worker_for(&h.dir.path().join("walt"), &h.key.key, &pack);
    h.mock.add_build_dir(&h.dir.path().join("walt"));
    let (m, r) = (format!("onnx graph {RUN} 1").into_bytes(), format!("reference rows {RUN} 1").into_bytes());
    let (ml, rl) = (m.len(), r.len());
    let sha = h.mock.add_artifact(&format!("ckpt/{RUN}/1/model.onnx"), m);
    let rsha = h.mock.add_artifact(&format!("ckpt/{RUN}/1/reference.npz"), r);
    let def = json!({"definition": 1, "kind": "run", "id": RUN, "version": 1, "created": zerofarm_common::now_unix() - 60, "expires": null,
        "game": "fake", "builds": [h.build_id, alt], "runtime": null,
        "checkpoints": {"1": {"model_sha256": sha, "model_size": ml, "reference_sha256": rsha, "reference_size": rl}},
        "jobs": {"selfplay": {"settings": {"sims": 8, "conc": 4, "fake": {"game_ms": 5, "shard_every": 4}}}}});
    let mut l = h.lease(id, json!({"kind": "selfplay", "gid_base": 0, "games": 8, "seed": 3}));
    l["run"] = json!(RUN);
    l["definition"] = h.define(&def);
    l["build"] = json!({"build_id": alt, "manifest": format!("builds/{alt}/manifest.json")});
    l["runtime"] = json!({"build_id": pack, "manifest": format!("builds/{pack}/manifest.json")});
    l["checkpoint"] = json!({"gen": 1, "model": format!("ckpt/{RUN}/1/model.onnx"), "sha256": sha,
                             "reference": format!("ckpt/{RUN}/1/reference.npz"), "reference_sha256": rsha});
    (l, pack)
}

/// Paths of the pack's files (not its manifest) the agent asked the coordinator for.
fn pack_file_gets(h: &H, pack: &str) -> Vec<String> {
    let prefix = format!("/v1/artifact/builds/{pack}/");
    h.mock.with(|st| st.log.iter().filter(|l| l.method == "GET" && l.path.starts_with(&prefix) && !l.path.contains("manifest.json")).map(|l| l.path.clone()).collect())
}

/// A machine where GPU work isn't allowed: no GPU signal (a Mac's profile has no GPU at all).
fn no_gpu_work() -> H {
    let h = H::new();
    h.signals.set(|s| s.gpu = None);
    h
}

#[test]
fn a_cpu_alternate_is_cpu_work_where_gpu_work_isnt_allowed() {
    let mut h = no_gpu_work();
    let (l, pack) = alt_lease(&h, "l_CA", None);
    // a probe of the pack here in which only the CPU passed (a GPU pack's verdict "failed") doesn't condemn a CPU pack
    let akey = farm_agent::probe::adapters_key(&h.gpus);
    farm_agent::probe::record(
        &h.layout,
        farm_agent::probe::Probe { runtime: pack.clone(), adapters: akey, worker: "w".into(), t: zerofarm_common::now_unix(), providers: vec!["cpu".into()], results: vec![], error: None, need: None, benched: true, real: vec![], rules: farm_agent::probe::RULES },
    );
    h.push(l);
    h.start();
    let done = h.wait_done("l_CA");
    assert!(done.get("refused").is_none_or(Value::is_null), "{done}\n{}", h.agent_log());
    assert_eq!(done["given_back"], false, "{done}");
    assert!(!pack_file_gets(&h, &pack).is_empty(), "the pack was downloaded");
    let line = h.job_line();
    assert_eq!(line["providers"], json!(["cpu"]), "{line}");
    let rt = line["runtime_dir"].as_str().unwrap_or_default().replace('\\', "/");
    assert!(rt.ends_with(&format!("builds/{pack}")), "the worker loads the pack: {line}");
    assert!(!h.mock.results_for("l_CA").is_empty(), "games played");
    assert!(h.crashes().iter().all(|c| c["kind"] != "definition"), "no refusal reported: {:?}", h.crashes());
    h.stop();
}

#[test]
fn a_gpu_pack_is_still_refused_where_gpu_work_isnt_allowed() {
    let mut h = no_gpu_work();
    let need = GpuNeed { vendor: "nvidia".into(), min_driver: Some("527.41".into()), providers: vec!["cuda".into()] };
    let (l, pack) = alt_lease(&h, "l_GA", Some(need));
    h.push(l);
    h.start();
    let done = h.wait_done("l_GA");
    assert_eq!(done["refused"], "runtime", "{done}");
    assert_eq!(done["given_back"], true, "{done}");
    assert!(done["stats"]["error"].as_str().unwrap_or("").contains("is for GPU work, which isn't allowed here right now"), "{done}");
    assert!(pack_file_gets(&h, &pack).is_empty(), "refused before the download");
    assert!(harness::wait(|| h.crashes().iter().any(|c| c["kind"] == "definition")), "{:?}", h.crashes());
    h.stop();
}

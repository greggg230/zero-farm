//! Model variants per generation (rev 20, PROTOCOL 14) with the real engine, the real fakeworker and the mock
//! coordinator: a checkpoint naming a `variant` is verified against the variant its definition signs for that gen (its
//! sha256 and size, the gen's own reference set), cached per (run, gen, variant) and handed to the worker as the model -
//! in a self-play job line, an eval job line's `models`, and a rolling lease's in-place switch; a variant the definition
//! doesn't sign, with another hash, or with a name that isn't one refuses the lease (a rolling switch: finishes it).

mod harness;

use harness::{wait, H};
use serde_json::{json, Value};
use std::path::Path;
use std::time::Duration;

const RUN: &str = "var-run";

/// Serves gen `gen`'s default model, reference set and the named variants' models: (the default checkpoint, the
/// definition entry with `variants`, per variant its checkpoint).
fn network(h: &H, gen: u64, variants: &[&str]) -> (Value, Value, Vec<Value>) {
    let (m, r) = (format!("onnx graph {RUN} {gen}").into_bytes(), format!("reference rows {RUN} {gen}").into_bytes());
    let (ml, rl) = (m.len(), r.len());
    let model = format!("ckpt/{RUN}/{gen}/model.onnx");
    let reference = format!("ckpt/{RUN}/{gen}/reference.npz");
    let sha = h.mock.add_artifact(&model, m);
    let rsha = h.mock.add_artifact(&reference, r);
    let base = json!({"gen": gen, "model": model, "sha256": sha, "reference": reference, "reference_sha256": rsha});
    let mut entry = json!({"model_sha256": sha, "model_size": ml, "reference_sha256": rsha, "reference_size": rl});
    let mut refs = Vec::new();
    for v in variants {
        let bytes = format!("fixed-shape graph {RUN} {gen} {v}").into_bytes();
        let n = bytes.len();
        let key = format!("ckpt/{RUN}/{gen}/model-{v}.onnx");
        let vsha = h.mock.add_artifact(&key, bytes);
        entry["variants"][*v] = json!({"model_sha256": vsha, "model_size": n});
        let mut c = base.clone();
        c["model"] = json!(key);
        c["sha256"] = json!(vsha);
        c["variant"] = json!(v);
        refs.push(c);
    }
    (base, entry, refs)
}

fn definition(h: &H, version: u64, checkpoints: Value, jobs: Value) -> Value {
    json!({"definition": 1, "kind": "run", "id": RUN, "version": version, "created": zerofarm_common::now_unix() - 60,
           "expires": null, "game": "fake", "builds": [h.build_id], "runtime": null, "checkpoints": checkpoints, "jobs": jobs})
}

fn selfplay(game_ms: u64) -> Value {
    json!({"selfplay": {"settings": {"sims": 8, "fake": {"game_ms": game_ms, "shard_every": 2, "flush_ms": 500}}}})
}

fn signed_lease(h: &H, id: &str, kind: &str, dref: &Value, job: Value) -> Value {
    let mut job = job;
    job["kind"] = json!(kind);
    let mut l = h.lease(id, job);
    l["run"] = json!(RUN);
    l["definition"] = dref.clone();
    l
}

fn quick_refusals(h: &mut H) {
    h.timings.start_backoff = Duration::from_millis(150);
    h.timings.start_backoff_max = Duration::from_millis(300);
}

fn file_name(p: &Value) -> String {
    Path::new(p.as_str().unwrap()).file_name().unwrap().to_string_lossy().to_string()
}

fn ckpt_dir(h: &H, gen: u64) -> std::path::PathBuf {
    h.layout.cache_ckpt().join(RUN).join(gen.to_string())
}

#[test]
fn a_self_play_lease_plays_the_variant_its_definition_signs_and_caches_it_per_variant() {
    let mut h = H::new();
    let (_, e1, v1) = network(&h, 1, &["gpu"]);
    let dref = h.define(&definition(&h, 1, json!({"1": e1}), selfplay(50)));
    let mut l = signed_lease(&h, "l_V1", "selfplay", &dref, json!({"gid_base": 0, "games": 4, "seed": 3}));
    l["checkpoint"] = v1[0].clone();
    h.push(l);
    h.start();
    let done = h.wait_done("l_V1");
    assert_eq!(done["given_back"], false, "{done}");
    assert!(done.get("refused").is_none(), "{done}");
    let j = h.job_line();
    assert_eq!(file_name(&j["model"]), "model-gpu.onnx", "{j}");
    assert_eq!(file_name(&j["reference"]), "reference.npz", "the gen's reference set: {j}");
    assert!(Path::new(j["model"].as_str().unwrap()).ends_with(Path::new("ckpt").join(RUN).join("1").join("model-gpu.onnx")), "{j}");
    // only the variant's model was fetched (this PC never needs the default graph), next to the gen's reference
    assert_eq!(h.mock.count("GET", &format!("/v1/artifact/ckpt/{RUN}/1/model-gpu.onnx")), 1);
    assert_eq!(h.mock.count("GET", &format!("/v1/artifact/ckpt/{RUN}/1/model.onnx")), 0);
    assert!(ckpt_dir(&h, 1).join("model-gpu.onnx").exists() && ckpt_dir(&h, 1).join("reference.npz").exists());
    assert!(!ckpt_dir(&h, 1).join("model.onnx").exists());
    // the next lease request lists the cached variant (PROTOCOL 14.3); no default model is listed for the gen
    assert!(
        wait(|| h.mock.with(|s| s.lease_requests.iter().any(|r| r["cached"]["ckpts"] == json!([format!("{RUN}/1/gpu")])))),
        "{:?}",
        h.mock.with(|s| s.lease_requests.iter().map(|r| r["cached"].clone()).collect::<Vec<_>>())
    );
    assert!(h.crashes().is_empty(), "{:?}", h.crashes());
    h.stop();
}

#[test]
fn a_variant_the_definition_doesnt_sign_as_given_refuses_the_lease() {
    let mut h = H::new();
    quick_refusals(&mut h);
    let (c1, e1, v1) = network(&h, 1, &["gpu"]);
    let dref = h.define(&definition(&h, 1, json!({"1": e1}), selfplay(50)));
    let job = || json!({"gid_base": 0, "games": 2, "seed": 1});
    let mut cases: Vec<(&str, Value, &str)> = Vec::new();
    // a variant the definition doesn't sign for the gen
    let mut l = signed_lease(&h, "l_NS", "selfplay", &dref, job());
    l["checkpoint"] = v1[0].clone();
    l["checkpoint"]["variant"] = json!("fp16");
    cases.push(("l_NS", l, "doesn't sign variant fp16"));
    // the variant's name on the default model's hash
    let mut l = signed_lease(&h, "l_DH", "selfplay", &dref, job());
    l["checkpoint"] = c1.clone();
    l["checkpoint"]["variant"] = json!("gpu");
    cases.push(("l_DH", l, "variant gpu sha256 is not the definition's"));
    // the variant's file as the default model (no `variant`)
    let mut l = signed_lease(&h, "l_VH", "selfplay", &dref, job());
    l["checkpoint"] = v1[0].clone();
    l["checkpoint"].as_object_mut().unwrap().remove("variant");
    cases.push(("l_VH", l, "model sha256 is not the definition's"));
    // a name that isn't one (it would name a file)
    let mut l = signed_lease(&h, "l_BN", "selfplay", &dref, job());
    l["checkpoint"] = v1[0].clone();
    l["checkpoint"]["variant"] = json!("..\\..\\x");
    cases.push(("l_BN", l, "not a variant name"));
    // a variant on a legacy lease (no definition)
    let mut l = h.lease("l_LG", json!({"kind": "selfplay", "gid_base": 0, "games": 2, "seed": 1, "settings": {"fake": {"game_ms": 50}}}));
    l["run"] = json!(RUN);
    l["checkpoint"] = v1[0].clone();
    cases.push(("l_LG", l, "a model variant without a definition"));
    for (_, l, _) in &cases {
        h.push(l.clone());
    }
    h.start();
    for (id, _, why) in &cases {
        let done = h.wait_done(id);
        assert_eq!(done["refused"], "definition", "{id}: {done}");
        let err = done["stats"]["error"].as_str().unwrap();
        assert!(err.contains(why), "{id}: {why:?} not in {err:?}");
    }
    assert_eq!(h.job_line_n(0), None, "no worker got a job");
    assert_eq!(h.mock.count("GET", &format!("/v1/artifact/ckpt/{RUN}/")), 0, "nothing was downloaded");
    h.stop();
}

#[test]
fn a_variant_file_that_isnt_the_signed_size_is_never_used() {
    let mut h = H::new();
    quick_refusals(&mut h);
    let (_, mut e1, v1) = network(&h, 1, &["gpu"]);
    // the definition signs the variant's hash with another size: the download can't verify
    e1["variants"]["gpu"]["model_size"] = json!(e1["variants"]["gpu"]["model_size"].as_u64().unwrap() + 1);
    let dref = h.define(&definition(&h, 1, json!({"1": e1}), selfplay(50)));
    let mut l = signed_lease(&h, "l_SZ", "selfplay", &dref, json!({"gid_base": 0, "games": 2, "seed": 1}));
    l["checkpoint"] = v1[0].clone();
    h.push(l);
    h.start();
    let done = h.wait_done("l_SZ");
    assert_eq!(done["given_back"], true, "{done}");
    assert_eq!(h.job_line_n(0), None, "no worker got a job");
    assert!(!ckpt_dir(&h, 1).join("model-gpu.onnx").exists());
    h.stop();
}

#[test]
fn an_eval_lease_hands_each_gen_its_own_file() {
    let mut h = H::new();
    let (c1, e1, _) = network(&h, 1, &[]);
    let (_, e2, v2) = network(&h, 2, &["gpu"]);
    let jobs = json!({"eval": {"settings": {"sims": 32, "fake": {"game_ms": 50, "shard_every": 2}}, "seats": [{"net": 2}, {"net": 1}], "paired": true}});
    let dref = h.define(&definition(&h, 1, json!({"1": e1, "2": e2}), jobs));
    let mut l = signed_lease(&h, "l_VE", "eval", &dref, json!({"gid_base": 0, "games": 4, "seed": 31}));
    l["checkpoints"] = json!([v2[0], c1]);
    h.push(l);
    h.start();
    let done = h.wait_done("l_VE");
    assert_eq!(done["given_back"], false, "{done}");
    let j = h.job_line();
    let models = j["models"].as_object().unwrap();
    assert_eq!((file_name(&models["2"]["model"]), models["2"]["sha256"].clone()), ("model-gpu.onnx".to_string(), v2[0]["sha256"].clone()), "{j}");
    assert_eq!((file_name(&models["1"]["model"]), models["1"]["sha256"].clone()), ("model.onnx".to_string(), c1["sha256"].clone()), "{j}");
    assert_eq!(file_name(&models["2"]["reference"]), "reference.npz");
    assert!(h.crashes().is_empty(), "{:?}", h.crashes());
    h.stop();
}

/// A rolling lease of ids `[base, hi)`, `games` released, 2 games in flight on the CPU.
fn rolling_lease(h: &H, id: &str, ckpt: &Value, dref: &Value, base: u64, games: u64, hi: u64) -> Value {
    let mut l = signed_lease(h, id, "selfplay", dref, json!({"gid_base": base, "games": games, "seed": 7}));
    l["checkpoint"] = ckpt.clone();
    l["rolling"] = json!({"block": [base, hi], "inflight": {"cpu": 2, "gpu": "auto"},
                          "until": zerofarm_common::now_unix() + 7200, "max_s": 7200, "flush_s": 10});
    l
}

fn beats(h: &H, id: &str) -> Vec<Value> {
    h.mock.with(|s| s.beats.iter().filter(|(l, _)| l == id).map(|(_, b)| b.clone()).collect())
}

#[test]
fn a_rolling_lease_switches_in_place_to_the_next_gens_variant_then_to_a_default() {
    let mut h = H::new();
    let (_, e1, v1) = network(&h, 1, &["gpu"]);
    let dref1 = h.define(&definition(&h, 1, json!({"1": e1.clone()}), selfplay(100)));
    h.push(rolling_lease(&h, "l_VR", &v1[0], &dref1, 0, 4, 60));
    h.start();
    assert!(wait(|| !h.mock.results_for("l_VR").is_empty()));
    // gen 2 with its variant: definition version 2 signs it; the beat answer names the variant
    let (_, e2, v2) = network(&h, 2, &["gpu"]);
    let dref2 = h.define(&definition(&h, 2, json!({"1": e1.clone(), "2": e2.clone()}), selfplay(100)));
    h.mock.set_run_current(RUN, v2[0].clone(), dref2);
    assert!(wait(|| beats(&h, "l_VR").iter().any(|b| b["gen"] == 2)), "{}", h.agent_log());
    let log = h.agent_log();
    assert!(log.contains("checkpoint Some(2) model file model-gpu.onnx"), "{log}");
    assert!(log.contains("checkpoint Some(2) self-tested (sha256 given: true)"), "the variant's sha256 with it: {log}");
    // gen 3 without a variant (the coordinator gives the default): switched to the default graph
    let (c3, e3, _) = network(&h, 3, &[]);
    let dref3 = h.define(&definition(&h, 3, json!({"1": e1, "2": e2, "3": e3}), selfplay(100)));
    h.mock.set_run_current(RUN, c3, dref3);
    assert!(wait(|| beats(&h, "l_VR").iter().any(|b| b["gen"] == 3)), "{}", h.agent_log());
    assert!(h.agent_log().contains("checkpoint Some(3) model file model.onnx"), "{}", h.agent_log());
    let done = h.wait_done("l_VR");
    assert_eq!(done["given_back"], false, "{done}");
    assert_eq!(done["stats"]["selftest"]["2"]["ok"], true, "{done}");
    assert!(ckpt_dir(&h, 2).join("model-gpu.onnx").exists() && ckpt_dir(&h, 3).join("model.onnx").exists());
    assert!(h.crashes().is_empty(), "{:?}", h.crashes());
    h.stop();
}

#[test]
fn a_rolling_switch_to_a_variant_the_new_definition_doesnt_sign_finishes_the_lease() {
    let mut h = H::new();
    let (_, e1, v1) = network(&h, 1, &["gpu"]);
    let dref1 = h.define(&definition(&h, 1, json!({"1": e1.clone()}), selfplay(100)));
    h.push(rolling_lease(&h, "l_VX", &v1[0], &dref1, 0, 4, 100_000));
    h.start();
    assert!(wait(|| !h.mock.results_for("l_VX").is_empty()));
    // gen 2's variant is served, but definition version 2 signs gen 2 without it
    let (_, mut e2, v2) = network(&h, 2, &["gpu"]);
    e2.as_object_mut().unwrap().remove("variants");
    let dref2 = h.define(&definition(&h, 2, json!({"1": e1, "2": e2}), selfplay(100)));
    h.mock.set_run_current(RUN, v2[0].clone(), dref2);
    let done = h.wait_done("l_VX");
    assert_eq!(done["given_back"], false, "{done}");
    assert!(done.get("refused").is_none(), "the lease ran: it is finished, not refused: {done}");
    assert!(h.agent_log().contains("rolling job ended (finish)"), "{}", h.agent_log());
    assert_eq!(h.mock.count("GET", &format!("/v1/artifact/ckpt/{RUN}/2/")), 0, "the unsigned variant is never fetched");
    std::thread::sleep(Duration::from_millis(1500));
    let crashes: Vec<Value> = h.crashes().into_iter().filter(|c| c["kind"] == "definition").collect();
    assert_eq!(crashes.len(), 1, "{crashes:?}");
    assert!(crashes[0]["message"].as_str().unwrap().contains("doesn't sign variant gpu"), "{crashes:?}");
    h.stop();
}

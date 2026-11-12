//! Signed definitions (PROTOCOL 7, rev 6) with the real engine, the real fakeworker and the mock
//! coordinator: the job line is built only from the signed template plus the per-lease fields and
//! the agent's own paths; every check that fails refuses the lease (`done` with `refused`), is
//! reported once as crash kind `definition`, and makes the agent wait; legacy leases (no
//! definition) get the strict allowlist (security review H2, H3, M2, L1).

mod harness;

use harness::{wait, H};
use serde_json::{json, Value};
use std::time::Duration;

const RUN: &str = "def-run";

/// Serves a network (model + reference) for `gen`: (lease checkpoint, definition entry).
fn network(h: &H, run: &str, gen: u64) -> (Value, Value) {
    let (m, r) = (format!("onnx graph {run} {gen}").into_bytes(), format!("reference rows {run} {gen}").into_bytes());
    let (ml, rl) = (m.len(), r.len());
    let model = format!("ckpt/{run}/{gen}/model.onnx");
    let reference = format!("ckpt/{run}/{gen}/reference.npz");
    let sha = h.mock.add_artifact(&model, m);
    let rsha = h.mock.add_artifact(&reference, r);
    (
        json!({"gen": gen, "model": model, "sha256": sha, "reference": reference, "reference_sha256": rsha}),
        json!({"model_sha256": sha, "model_size": ml, "reference_sha256": rsha, "reference_size": rl}),
    )
}

fn definition(h: &H, kind: &str, id: &str, version: u64, checkpoints: Value, jobs: Value) -> Value {
    json!({"definition": 1, "kind": kind, "id": id, "version": version, "created": zerofarm_common::now_unix() - 60,
           "expires": null, "game": "fake", "builds": [h.build_id], "runtime": null, "checkpoints": checkpoints, "jobs": jobs})
}

fn signed_lease(h: &H, id: &str, kind: &str, def_ref: Value, job: Value) -> Value {
    let mut job = job;
    job["kind"] = json!(kind);
    let mut l = h.lease(id, job);
    l["run"] = json!(RUN);
    l["definition"] = def_ref;
    l
}

/// Short waits after a refusal, so a test can go through several.
fn quick_refusals(h: &mut H) {
    h.timings.start_backoff = Duration::from_millis(150);
    h.timings.start_backoff_max = Duration::from_millis(300);
}

fn no_worker_ran(h: &H) -> bool {
    !h.agent_log().contains("fakeworker: job")
}

#[test]
fn the_job_line_comes_from_the_signed_template_only() {
    let mut h = H::new();
    let (c1, d1) = network(&h, RUN, 1);
    let def = definition(
        &h,
        "run",
        RUN,
        3,
        json!({"1": d1}),
        json!({"selfplay": {"settings": {"sims": 64, "fake": {"game_ms": 100, "shard_every": 2}}}}),
    );
    let dref = h.define(&def);
    // The coordinator's job carries the per-lease fields plus everything an attacker would add.
    let job = json!({"gid_base": 40, "games": 6, "seed": 9, "settings": {"sims": 1, "book": "\\\\attacker\\s\\b"},
                     "prefix": "..\\..\\Desktop\\x", "models": {"1": {"model": "\\\\attacker\\s\\m.onnx"}},
                     "out_dir": "C:\\Windows", "runtime_dir": "\\\\attacker\\rt", "threads": 64, "uniform": true});
    let mut l = signed_lease(&h, "l_D1", "selfplay", dref.clone(), job);
    l["checkpoint"] = c1;
    h.push(l);
    h.start();
    // A newer network that the definition doesn't name is never fetched.
    assert!(wait(|| !h.mock.results_for("l_D1").is_empty()));
    let (c2, _) = network(&h, RUN, 2);
    h.mock.script_beat("l_D1", json!({"action": "continue", "checkpoint": c2}));
    let done = h.wait_done("l_D1");
    assert_eq!(done["given_back"], false, "{done}");
    assert!(done.get("refused").is_none(), "{done}");

    let j = h.job_line();
    let mut keys: Vec<&str> = j.as_object().unwrap().keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        ["games", "gid_base", "kind", "lease_id", "model", "op", "out_dir", "reference", "runtime_dir", "seed", "settings", "threads"],
        "{j}"
    );
    assert_eq!(j["settings"], json!({"sims": 64, "fake": {"game_ms": 100, "shard_every": 2}}), "the signed settings, not the coordinator's");
    assert_eq!((j["gid_base"].as_u64(), j["games"].as_u64(), j["seed"].as_u64()), (Some(40), Some(6), Some(9)));
    assert_eq!(j["kind"], "selfplay");
    assert_eq!(j["out_dir"], "out");
    assert!(j["runtime_dir"].is_null());
    assert_eq!(j["threads"], 6, "the agent's own thread count");
    let model = std::path::PathBuf::from(j["model"].as_str().unwrap());
    assert!(model.is_absolute() && model.ends_with(format!("{RUN}\\1\\model.onnx")), "{j}");

    assert_eq!(h.mock.count("GET", &format!("/v1/artifact/ckpt/{RUN}/2/")), 0, "network 2 is not in the definition");
    assert_eq!(h.mock.count("GET", &format!("/v1/artifact/{}", dref["key"].as_str().unwrap())), 2, "the definition and its signature, once each");
    let seen: Value = serde_json::from_slice(&std::fs::read(h.layout.state().join("definitions.json")).unwrap()).unwrap();
    assert_eq!(seen, json!({"run/def-run": 3}));
    assert!(h.crashes().is_empty(), "{:?}", h.crashes());
    h.stop();
}

#[test]
fn an_eval_definition_names_the_networks_and_the_seats() {
    let mut h = H::new();
    let (c80, d80) = network(&h, RUN, 80);
    let (c70, d70) = network(&h, RUN, 70);
    let def = definition(
        &h,
        "run",
        RUN,
        1,
        json!({"80": d80, "70": d70}),
        json!({"eval": {"settings": {"sims": 32, "fake": {"game_ms": 50, "shard_every": 2}}, "seats": [{"net": 80}, {"net": 70}], "paired": true}}),
    );
    let dref = h.define(&def);
    // The coordinator's job says other seats; only the signed ones count.
    let mut l = signed_lease(&h, "l_DE", "eval", dref, json!({"gid_base": 10, "games": 4, "seed": 31, "seats": [{"bot": "a"}, {"bot": "b"}], "paired": false}));
    l["checkpoints"] = json!([c80, c70]);
    h.push(l);
    h.start();
    let done = h.wait_done("l_DE");
    assert_eq!(done["given_back"], false, "{done}");
    let j = h.job_line();
    assert_eq!(j["seats"], json!([{"net": 80}, {"net": 70}]));
    assert_eq!(j["paired"], true);
    let models = j["models"].as_object().unwrap();
    assert_eq!(models.len(), 2);
    for (g, c) in [("80", &c80), ("70", &c70)] {
        assert_eq!(models[g]["sha256"], c["sha256"]);
        assert!(std::path::Path::new(models[g]["model"].as_str().unwrap()).is_absolute());
    }
    assert_eq!(h.mock.results_for("l_DE").len(), 2);
    h.stop();
}

#[test]
fn every_failed_check_refuses_the_lease_reports_once_and_waits() {
    let mut h = H::new();
    quick_refusals(&mut h);
    let (c1, d1) = network(&h, RUN, 1);
    let sp = json!({"selfplay": {"settings": {"fake": {"game_ms": 50}}}});
    let job = || json!({"gid_base": 0, "games": 2, "seed": 1});
    let ok_def = |version: u64| definition(&h, "run", RUN, version, json!({"1": d1.clone()}), sp.clone());
    let lease_for = |id: &str, def: &Value| {
        let mut l = signed_lease(&h, id, "selfplay", h.define(def), job());
        l["checkpoint"] = c1.clone();
        l
    };
    let mut cases: Vec<(&str, Value, &str)> = Vec::new();

    // A good version 5 first: it runs and is remembered.
    cases.push(("l_OK", lease_for("l_OK", &ok_def(5)), ""));
    // Signed with the release key (not a data key).
    let (bytes, sig) = mockcoord::testkit::sign_definition(&ok_def(6), &h.key.key);
    let mut l = signed_lease(&h, "l_RK", "selfplay", h.mock.add_definition(bytes, &sig), job());
    l["checkpoint"] = c1.clone();
    cases.push(("l_RK", l, "signed by unknown key"));
    // The bytes served are not the ones the lease names.
    let l = lease_for("l_TA", &ok_def(7));
    let key = l["definition"]["key"].as_str().unwrap().to_string();
    h.mock.with(|s| s.artifacts.get_mut(&key).unwrap().push(b' '));
    cases.push(("l_TA", l, "sha256 mismatch"));
    // An older version than seen.
    cases.push(("l_OLD", lease_for("l_OLD", &ok_def(4)), "older than version 5"));
    // Another run's definition.
    let mut other = ok_def(8);
    other["id"] = json!("other-run");
    cases.push(("l_ID", lease_for("l_ID", &other), "not for the lease's run def-run"));
    // Expired.
    let mut exp = ok_def(9);
    exp["created"] = json!(1000);
    exp["expires"] = json!(2000);
    cases.push(("l_EX", lease_for("l_EX", &exp), "expired"));
    // A worker build the definition doesn't name.
    let mut nb = ok_def(10);
    nb["builds"] = json!(["0123456789abcdef01234567"]);
    cases.push(("l_NB", lease_for("l_NB", &nb), "is not in definition"));
    // A runtime the definition doesn't name.
    let mut l = lease_for("l_RT", &ok_def(11));
    l["runtime"] = json!({"build_id": "aaaaaaaaaaaaaaaaaaaaaaaa", "manifest": "builds/aaaaaaaaaaaaaaaaaaaaaaaa/manifest.json"});
    cases.push(("l_RT", l, "runtime"));
    // A checkpoint whose hash isn't the definition's; then the same again: refused, but not
    // reported twice.
    let def12 = ok_def(12);
    for id in ["l_CK", "l_CK2"] {
        let mut l = lease_for(id, &def12);
        l["checkpoint"]["sha256"] = json!("0".repeat(64));
        cases.push((id, l, "not the definition's"));
    }
    // A job kind the definition has no template for.
    let mut l = lease_for("l_FZ", &ok_def(13));
    l["kind"] = json!("fuzz");
    l["checkpoint"] = Value::Null;
    l["job"] = json!({"first": 0, "count": 10});
    cases.push(("l_FZ", l, "can't come from a run definition"));
    // Per-lease fields out of range.
    let mut l = lease_for("l_PL", &ok_def(14));
    l["job"]["games"] = json!(0);
    cases.push(("l_PL", l, "games 0"));
    for (_, l, _) in &cases {
        h.push(l.clone());
    }
    h.start();
    for (id, _, why) in &cases {
        let done = h.wait_done(id);
        if why.is_empty() {
            assert_eq!(done["given_back"], false, "{id}: {done}");
            continue;
        }
        assert_eq!(done["refused"], "definition", "{id}: {done}");
        assert_eq!(done["given_back"], true);
        assert!(done["results"].as_array().unwrap().is_empty());
        let err = done["stats"]["error"].as_str().unwrap();
        assert!(err.contains(why), "{id}: {why:?} not in {err:?}");
        assert!(h.mock.results_for(id).is_empty());
    }
    assert_eq!(h.job_line_n(1), None, "only the good lease's worker got a job");
    let c = h.crashes();
    assert!(c.iter().all(|c| c["kind"] == "definition"), "{c:?}");
    assert_eq!(c.len(), cases.len() - 2, "one report per refusal, the repeated one not again: {c:?}");
    // The engine waited after each refusal: one lease request per lease, no storm.
    let reqs = h.mock.with(|s| s.lease_requests.len());
    assert!(reqs <= cases.len() + 3, "{reqs} lease requests for {} leases", cases.len());
    h.stop();
}

#[test]
fn a_legacy_lease_gets_the_allowlist() {
    let mut h = H::new();
    quick_refusals(&mut h);
    // The review's H3 lease: eval, no checkpoints, a `models` naming a UNC path, a `prefix` out
    // of the work folder. The worker gets neither.
    let mut l = h.lease(
        "l_LG",
        json!({"kind": "eval", "gid_base": 0, "games": 2, "seed": 1, "paired": true, "settings": {"fake": {"game_ms": 50}},
               "seats": [{"bot": "first"}, {"bot": "second"}],
               "models": {"1": {"model": "\\\\attacker.example\\s\\m.onnx", "sha256": "00"}},
               "prefix": "..\\..\\..\\Desktop\\x", "ort_dylib": "x", "_raw": 1}),
    );
    l["run"] = json!(RUN);
    l["checkpoint"] = Value::Null;
    l["checkpoints"] = Value::Null;
    h.push(l);
    // A path anywhere in a legacy job refuses it; so does a job kind that isn't the lease's.
    let mut p = h.lease("l_LP", json!({"kind": "selfplay", "gid_base": 0, "games": 2, "seed": 1, "settings": {"net": "C:\\Users\\x\\a.onnx"}}));
    p["run"] = json!(RUN);
    h.push(p);
    let mut k = h.lease("l_LK", json!({"kind": "selfplay", "gid_base": 0, "games": 2, "seed": 1, "settings": {}}));
    k["kind"] = json!("eval");
    h.push(k);
    h.start();
    let done = h.wait_done("l_LG");
    assert_eq!(done["given_back"], false, "{done}");
    let j = h.job_line();
    assert_eq!(j["models"], json!({}), "the agent's own (empty) models, never the coordinator's: {j}");
    for k in ["prefix", "ort_dylib", "_raw"] {
        assert!(j.get(k).is_none(), "{k} dropped: {j}");
    }
    assert_eq!(j["kind"], "eval");
    let p = h.wait_done("l_LP");
    assert_eq!(p["refused"], "definition");
    assert!(p["stats"]["error"].as_str().unwrap().contains("looks like a file path"), "{p}");
    let k = h.wait_done("l_LK");
    assert_eq!(k["refused"], "definition");
    assert!(k["stats"]["error"].as_str().unwrap().contains("kind"), "{k}");
    assert_eq!(h.job_line_n(1), None, "the refused leases never reached a worker");
    h.stop();
}

#[test]
fn a_declared_checkpoint_bigger_than_the_disk_cap_is_never_downloaded() {
    // L1: room for the declared size is made before a byte is fetched.
    let mut h = H::new();
    h.machine.lock().unwrap().settings.disk_cap_mb = 500;
    let (c1, mut d1) = network(&h, RUN, 1);
    d1["model_size"] = json!(900u64 << 20);
    let def = definition(&h, "run", RUN, 1, json!({"1": d1}), json!({"selfplay": {"settings": {}}}));
    let mut l = signed_lease(&h, "l_BIG", "selfplay", h.define(&def), json!({"gid_base": 0, "games": 2, "seed": 1}));
    l["checkpoint"] = c1;
    h.push(l);
    h.start();
    let done = h.wait_done("l_BIG");
    assert_eq!(done["given_back"], true);
    assert!(done["stats"]["error"].as_str().unwrap().contains("disk space"), "{done}");
    assert_eq!(h.mock.count("GET", &format!("/v1/artifact/ckpt/{RUN}/1/")), 0, "nothing fetched over the cap");
    assert!(no_worker_ran(&h));
    h.stop();
}

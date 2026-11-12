//! Eval leases with two or more networks (PROTOCOL 5.2, rev 5), with the real engine, the real
//! fakeworker and the mock coordinator: every checkpoint downloaded and verified, `models` in the
//! job line, `evals` result files uploaded with `x-ids` and no `x-gen`, per-gen self-tests in
//! Stats and in the crash report when one fails.

mod harness;

use farm_agent::status::Phase;
use harness::{wait, H};
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::time::Duration;

const RUN: &str = "h2h-test";

/// Serves model + reference for `gen` and returns its Checkpoint object.
fn ckpt(h: &H, gen: u64) -> Value {
    let model = format!("ckpt/{RUN}/{gen}/model.onnx");
    let reference = format!("ckpt/{RUN}/{gen}/reference.npz");
    let sha = h.mock.add_artifact(&model, format!("onnx graph of network {gen}").into_bytes());
    let rsha = h.mock.add_artifact(&reference, format!("reference rows of network {gen}").into_bytes());
    json!({"gen": gen, "model": model, "sha256": sha, "reference": reference, "reference_sha256": rsha})
}

fn eval_lease(h: &H, id: &str, games: u64, base: u64, ckpts: Vec<Value>, fake: Value) -> Value {
    let seats: Vec<Value> = ckpts.iter().map(|c| json!({"net": c["gen"]})).collect();
    let job = json!({"kind": "eval", "games": games, "gid_base": base, "seed": 31, "paired": true,
                     "settings": {"sims": 32, "fake": fake}, "seats": seats});
    let mut l = h.lease(id, job);
    l["run"] = json!(RUN);
    l["checkpoint"] = Value::Null;
    l["checkpoints"] = Value::Array(ckpts);
    l
}

fn ranges_of(v: &str) -> Vec<(u64, u64)> {
    let a: Value = serde_json::from_str(v).unwrap();
    a.as_array().unwrap().iter().map(|r| (r[0].as_u64().unwrap(), r[1].as_u64().unwrap())).collect()
}

#[test]
fn eval_lease_with_two_networks_runs_and_uploads_every_game() {
    let mut h = H::new();
    let (c80, c70) = (ckpt(&h, 80), ckpt(&h, 70));
    // shard_every 3 with pairs: files end in the middle of a pair.
    h.push(eval_lease(&h, "l_EV", 8, 100, vec![c80.clone(), c70.clone()], json!({"game_ms": 50, "shard_every": 3})));
    h.start();
    let done = h.wait_done("l_EV");
    assert_eq!(done["given_back"], false, "{done}");
    assert!(done.get("unfinished").map(Value::is_null).unwrap_or(true), "{done}");

    // Both networks (and their references) were downloaded, verified and cached.
    for g in [80, 70] {
        let d = h.layout.cache_ckpt().join(RUN).join(g.to_string());
        assert!(d.join("model.onnx").exists() && d.join("reference.npz").exists(), "gen {g} cached");
        assert_eq!(h.mock.count("GET", &format!("/v1/artifact/ckpt/{RUN}/{g}/")), 2, "model + reference of {g}, once each");
    }

    // The job line: `models` by gen with absolute paths and hashes; no single model.
    let job = h.job_line();
    assert!(job["model"].is_null() && job["reference"].is_null(), "{job}");
    let models = job["models"].as_object().expect("models");
    assert_eq!(models.keys().cloned().collect::<BTreeSet<_>>(), BTreeSet::from(["70".to_string(), "80".to_string()]));
    for (c, g) in [(&c80, "80"), (&c70, "70")] {
        let m = &models[g];
        assert_eq!(m["sha256"], c["sha256"]);
        let model = std::path::Path::new(m["model"].as_str().unwrap());
        assert!(model.is_absolute() && model.ends_with(format!("{RUN}\\{g}\\model.onnx")), "{m}");
        assert!(m["reference"].as_str().unwrap().ends_with("reference.npz"), "{m}");
    }
    assert_eq!(job["seats"], json!([{"net": 80}, {"net": 70}]));

    // Every game uploaded once, with x-ids and never x-gen; rows in the 5.2 format.
    let results = h.mock.results_for("l_EV");
    assert_eq!(results.len(), 3, "8 games in files of 3");
    let mut gids = Vec::new();
    for (_, r) in &results {
        assert!(!r.headers.contains_key("x-gen"), "no x-gen on eval chunks");
        let ids = ranges_of(&r.headers["x-ids"]);
        let mut in_file: Vec<u64> = Vec::new();
        for line in String::from_utf8_lossy(&r.bytes).lines() {
            let row: Value = serde_json::from_str(line).unwrap();
            let g = row["gid"].as_u64().unwrap();
            in_file.push(g);
            assert_eq!(row["pair"], g / 2);
            let seats = if g % 2 == 0 { json!([{"net": 80}, {"net": 70}]) } else { json!([{"net": 70}, {"net": 80}]) };
            assert_eq!(row["seats"], seats, "seat order swaps within a pair");
            assert_eq!(row["result"].as_array().unwrap().len(), 2);
        }
        let declared: Vec<u64> = ids.iter().flat_map(|&(a, b)| a..b).collect();
        assert_eq!(in_file, declared, "x-ids = the file's gids");
        gids.extend(in_file);
    }
    assert_eq!(gids, (100..108).collect::<Vec<_>>());
    assert_eq!(ranges_of(&results[0].1.headers["x-ids"]), [(100, 103)], "a file split pair 101/102");

    // Stats: one self-test per network.
    let st = &done["stats"]["selftest"];
    for g in ["70", "80"] {
        assert_eq!(st[g]["ok"], true, "{st}");
        assert_eq!(st[g]["provider"], "cpu");
    }
    assert!(done["stats"]["selftest_max_abs"].as_f64().unwrap() > 0.0);
    assert!(h.crashes().is_empty(), "{:?}", h.crashes());

    // The next lease request lists both checkpoints as cached.
    assert!(wait(|| h.mock.with(|s| {
        s.lease_requests.last().is_some_and(|r| {
            let c = &r["cached"]["ckpts"];
            c.as_array().is_some_and(|a| a.contains(&json!(format!("{RUN}/70"))) && a.contains(&json!(format!("{RUN}/80"))))
        })
    })));
    h.stop();
}

#[test]
fn a_tampered_checkpoint_is_refused_before_any_worker_runs() {
    let mut h = H::new();
    let good = ckpt(&h, 80);
    let bad = ckpt(&h, 70);
    // The lease's hash is right; the bytes served are not.
    h.mock.with(|s| s.artifacts.insert(format!("ckpt/{RUN}/70/model.onnx"), b"not the network the lease names".to_vec()));
    h.push(eval_lease(&h, "l_ET", 4, 0, vec![good.clone(), bad.clone()], json!({})));
    // The same bad network again: it must not be fetched and refused in a loop.
    h.push(eval_lease(&h, "l_ET2", 4, 4, vec![good, bad], json!({})));
    h.start();
    let done = h.wait_done("l_ET");
    assert_eq!(done["given_back"], true);
    assert!(done["stats"]["error"].as_str().unwrap().contains("sha256"), "{done}");
    assert_eq!(done["unfinished"], json!({"gids": [[0, 4]]}));
    assert!(!h.layout.cache_ckpt().join(RUN).join("70").join("model.onnx").exists(), "nothing unverified in the cache");
    assert!(h.mock.results_for("l_ET").is_empty());
    assert!(!h.agent_log().contains("fakeworker: job"), "no worker got the job");
    // No lease storm after a verification failure: the agent waits (1 min) before asking again.
    std::thread::sleep(Duration::from_secs(3));
    assert_eq!(h.mock.with(|s| s.lease_requests.len()), 1, "one lease request, then waiting");
    assert_eq!(h.phase(), Phase::Waiting);
    h.stop();
}

#[test]
fn a_failing_self_test_is_reported_with_every_gen() {
    let mut h = H::new();
    let (c70, c80) = (ckpt(&h, 70), ckpt(&h, 80));
    h.push(eval_lease(&h, "l_ES", 4, 20, vec![c70, c80], json!({"selftest_fail_gen": 80})));
    h.start();
    let done = h.wait_done("l_ES");
    assert_eq!(done["given_back"], true);
    assert_eq!(done["unfinished"], json!({"gids": [[20, 24]]}));
    assert_eq!(done["stats"]["selftest"]["70"]["ok"], true, "{done}");
    assert_eq!(done["stats"]["selftest"]["80"]["ok"], false, "{done}");
    let c = h.crashes();
    assert_eq!(c.len(), 1, "one report: {c:?}");
    assert_eq!(c[0]["kind"], "crash");
    assert_eq!(c[0]["lease_id"], "l_ES");
    let msg = c[0]["message"].as_str().unwrap();
    assert!(msg.contains("network 80 failed the self-test"), "{msg}");
    assert!(msg.contains("selftest per gen") && msg.contains("\"70\"") && msg.contains("\"80\""), "{msg}");
    assert!(h.mock.results_for("l_ES").is_empty());
    h.stop();
}

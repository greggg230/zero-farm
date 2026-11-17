//! Rolling self-play leases (rev 15, PROTOCOL 10) with the real engine, the real fakeworker and the mock
//! coordinator: more ids on `low` (`POST .../extend`), `wait_s`, the coordinator's `finish`, a new generation with
//! its definition verified and switched to in place, a changed template or a bad definition ending the lease
//! gracefully, an old worker running the lease as a classic one, an agent update finishing it, a lost lease, and
//! classic leases unchanged.

mod harness;

use farm_agent::engine::EngineExit;
use harness::{wait, H};
use mockcoord::testkit::{bin, make_agent_package, sign_definition};
use serde_json::{json, Value};
use std::time::{Duration, Instant};

const RUN: &str = "roll-run";

/// Serves a network (model + reference) for `gen`: (lease checkpoint, definition entry).
fn network(h: &H, gen: u64) -> (Value, Value) {
    let (m, r) = (format!("onnx graph {RUN} {gen}").into_bytes(), format!("reference rows {RUN} {gen}").into_bytes());
    let (ml, rl) = (m.len(), r.len());
    let model = format!("ckpt/{RUN}/{gen}/model.onnx");
    let reference = format!("ckpt/{RUN}/{gen}/reference.npz");
    let sha = h.mock.add_artifact(&model, m);
    let rsha = h.mock.add_artifact(&reference, r);
    (
        json!({"gen": gen, "model": model, "sha256": sha, "reference": reference, "reference_sha256": rsha}),
        json!({"model_sha256": sha, "model_size": ml, "reference_sha256": rsha, "reference_size": rl}),
    )
}

fn definition(h: &H, version: u64, checkpoints: Value, settings: Value) -> Value {
    json!({"definition": 1, "kind": "run", "id": RUN, "version": version, "created": zerofarm_common::now_unix() - 60,
           "expires": null, "game": "fake", "builds": [h.build_id], "runtime": null, "checkpoints": checkpoints,
           "jobs": {"selfplay": {"settings": settings}}})
}

fn settings(game_ms: u64) -> Value {
    json!({"sims": 8, "fake": {"game_ms": game_ms, "shard_every": 2, "flush_ms": 500}})
}

/// Network 1 and definition version 1 (signing it, with `settings(game_ms)`): (checkpoint, definition reference).
fn generation_1(h: &H, game_ms: u64) -> (Value, Value) {
    let (c1, d1) = network(h, 1);
    let dref = h.define(&definition(h, 1, json!({"1": d1}), settings(game_ms)));
    (c1, dref)
}

/// A rolling lease of ids `[base, hi)`, `games` released, `cpu` games in flight on the CPU.
fn rolling_lease(h: &H, id: &str, (ckpt, dref): &(Value, Value), base: u64, games: u64, hi: u64, cpu: Value) -> Value {
    let mut l = h.lease(id, json!({"kind": "selfplay", "gid_base": base, "games": games, "seed": 7}));
    l["run"] = json!(RUN);
    l["definition"] = dref.clone();
    l["checkpoint"] = ckpt.clone();
    l["rolling"] = json!({"block": [base, hi], "inflight": {"cpu": cpu, "gpu": "auto"},
                          "until": zerofarm_common::now_unix() + 7200, "max_s": 7200, "flush_s": 10});
    l
}

fn extends(h: &H, id: &str) -> Vec<Value> {
    h.mock.with(|s| s.extends.iter().filter(|(l, _)| l == id).map(|(_, b)| b.clone()).collect())
}

/// Every id the coordinator credited to `id`'s chunks.
fn credited(h: &H, id: &str) -> u64 {
    h.mock.with(|s| s.credits.iter().filter(|(l, ..)| l == id).map(|(_, _, c, _)| *c).sum())
}

/// How many ids `done` reports unfinished.
fn unfinished_n(done: &Value) -> u64 {
    done["unfinished"]["gids"].as_array().map_or(0, |u| u.iter().map(|r| r[1].as_u64().unwrap() - r[0].as_u64().unwrap()).sum())
}

/// Every released id is either credited or reported unfinished: `finish` (the block used up) starts no new game, so
/// a released id not started yet (fewer than the games in flight) goes back.
fn all_accounted(h: &H, id: &str, done: &Value, released: u64, games_in_flight: u64) {
    let (c, u) = (credited(h, id), unfinished_n(done));
    assert_eq!(c + u, released, "{done}");
    assert!(u < games_in_flight, "at most the unstarted ids go back: {done}");
}

fn beats(h: &H, id: &str) -> Vec<Value> {
    h.mock.with(|s| s.beats.iter().filter(|(l, _)| l == id).map(|(_, b)| b.clone()).collect())
}

fn x_gens(h: &H, id: &str) -> Vec<u64> {
    h.mock.results_for(id).iter().filter_map(|(_, r)| r.headers.get("x-gen").and_then(|g| g.parse().ok())).collect()
}

fn definition_crashes(h: &H) -> usize {
    h.crashes().iter().filter(|c| c["kind"] == "definition").count()
}

#[test]
fn extends_on_low_until_the_block_is_used_up() {
    let mut h = H::new();
    let g1 = generation_1(&h, 100);
    h.push(rolling_lease(&h, "l_R1", &g1, 1000, 4, 1016, json!(2)));
    h.start();
    let done = h.wait_done("l_R1");
    assert_eq!(done["given_back"], false, "{done}");
    assert!(done.get("refused").is_none(), "{done}");
    // 4 released at grant, then 4 more three times, then the block is used up: `finish`
    assert_eq!(extends(&h, "l_R1"), vec![json!({"games": 4}); 4], "about 2 x the games in flight each time");
    all_accounted(&h, "l_R1", &done, 16, 2);
    assert!(h.agent_log().contains("rolling job ended (finish)"), "{}", h.agent_log());
    let j = h.job_line();
    assert_eq!((j["rolling"].clone(), j["flush_s"].clone(), j["games"].clone(), j["gid_base"].clone()), (json!(true), json!(10), json!(4), json!(1000)), "{j}");
    assert_eq!((j["conc"].clone(), j["groups"].clone()), (json!(2), json!(1)), "the run's CPU games in flight, the template's groups");
    assert_eq!((done["stats"]["inflight"].clone(), done["stats"]["released"].clone()), (json!(2), json!(16)), "{done}");
    let b = beats(&h, "l_R1");
    assert!(!b.is_empty() && b.iter().all(|b| b["gen"] == 1), "rolling beats carry the generation: {b:?}");
    assert_eq!(h.mock.with(|s| s.lease_requests[0]["features"].clone()), json!(["rolling"]));
    assert!(h.crashes().is_empty(), "{:?}", h.crashes());
    h.stop();
}

#[test]
fn wait_s_is_honoured_before_asking_again() {
    let mut h = H::new();
    let g1 = generation_1(&h, 100);
    h.push(rolling_lease(&h, "l_RW", &g1, 0, 4, 8, json!(2)));
    h.mock.script_extend("l_RW", json!({"wait_s": 1}));
    h.start();
    assert!(wait(|| !extends(&h, "l_RW").is_empty()));
    let t0 = Instant::now();
    assert!(wait(|| extends(&h, "l_RW").len() >= 2));
    assert!(t0.elapsed() >= Duration::from_millis(800), "asked again after {:?}, before wait_s", t0.elapsed());
    let done = h.wait_done("l_RW");
    assert_eq!(done["given_back"], false, "{done}");
    all_accounted(&h, "l_RW", &done, 8, 2);
    h.stop();
}

#[test]
fn the_coordinators_finish_ends_the_lease_gracefully() {
    let mut h = H::new();
    let g1 = generation_1(&h, 150);
    h.push(rolling_lease(&h, "l_RF", &g1, 0, 4, 100, json!(2)));
    h.mock.script_extend("l_RF", json!({"finish": true, "reason": "max_s"}));
    h.start();
    let done = h.wait_done("l_RF");
    assert_eq!(done["given_back"], false, "{done}");
    assert_eq!(extends(&h, "l_RF").len(), 1);
    assert!(h.agent_log().contains("rolling job ended (finish)"), "{}", h.agent_log());
    // the released ids not finished, inside the released ones
    if let Some(u) = done["unfinished"]["gids"].as_array() {
        assert!(u.iter().all(|r| r[0].as_u64().unwrap() >= 2 && r[1].as_u64().unwrap() <= 4), "{done}");
    }
    all_accounted(&h, "l_RF", &done, 4, 2);
    h.stop();
}

#[test]
fn a_new_generation_with_a_verified_definition_is_switched_to_in_place() {
    let mut h = H::new();
    let g1 = generation_1(&h, 100);
    h.push(rolling_lease(&h, "l_RG", &g1, 0, 4, 40, json!(2)));
    h.start();
    assert!(wait(|| !h.mock.results_for("l_RG").is_empty()));
    // the trainer publishes gen 2: definition version 2 signs gens 1 and 2 with the same template
    let (_, d1) = network(&h, 1);
    let (c2, d2) = network(&h, 2);
    let dref2 = h.define(&definition(&h, 2, json!({"1": d1, "2": d2}), settings(100)));
    h.mock.set_run_current(RUN, c2, dref2);
    assert!(wait(|| beats(&h, "l_RG").iter().any(|b| b["gen"] == 2)), "{}", h.agent_log());
    assert!(h.agent_log().contains("checkpoint Some(2) self-tested (sha256 given: true)"), "{}", h.agent_log());
    let done = h.wait_done("l_RG");
    assert_eq!(done["given_back"], false, "{done}");
    all_accounted(&h, "l_RG", &done, 40, 2);
    assert!(x_gens(&h, "l_RG").contains(&2), "chunks of the new generation: {:?}", x_gens(&h, "l_RG"));
    assert_eq!(h.mock.count("GET", &format!("/v1/artifact/ckpt/{RUN}/2/model.onnx")), 1, "fetched once, whatever the repeats");
    let seen: Value = serde_json::from_slice(&std::fs::read(h.layout.state().join("definitions.json")).unwrap()).unwrap();
    assert_eq!(seen, json!({"run/roll-run": 2}), "the new version is remembered");
    assert!(h.crashes().is_empty(), "{:?}", h.crashes());
    assert_eq!(done["stats"]["selftest"]["2"]["ok"], true, "{done}");
    h.stop();
}

#[test]
fn a_changed_template_finishes_the_lease() {
    let mut h = H::new();
    let g1 = generation_1(&h, 100);
    h.push(rolling_lease(&h, "l_RT", &g1, 0, 4, 100_000, json!("auto")));
    h.start();
    assert!(wait(|| !h.mock.results_for("l_RT").is_empty()));
    let (_, d1) = network(&h, 1);
    let (c2, d2) = network(&h, 2);
    let mut other = settings(100);
    other["sims"] = json!(16);
    let dref2 = h.define(&definition(&h, 2, json!({"1": d1, "2": d2}), other));
    h.mock.set_run_current(RUN, c2, dref2);
    let done = h.wait_done("l_RT");
    assert_eq!(done["given_back"], false, "{done}");
    assert!(h.agent_log().contains("rolling job ended (finish)"), "{}", h.agent_log());
    assert_eq!(h.mock.count("GET", &format!("/v1/artifact/ckpt/{RUN}/2/")), 0, "the new network is never fetched");
    assert_eq!(definition_crashes(&h), 0, "a new template is no fault");
    // "auto" on the CPU: 4 per thread (8 here) in 4 groups, never more than fits the memory cap
    let j = h.job_line();
    let (conc, groups) = (j["conc"].as_u64().unwrap(), j["groups"].as_u64().unwrap());
    assert!((1..=32).contains(&conc) && groups == conc.min(4), "{j}");
    h.stop();
}

#[test]
fn a_definition_that_fails_a_check_finishes_the_lease_reported_once() {
    let mut h = H::new();
    let g1 = generation_1(&h, 100);
    h.push(rolling_lease(&h, "l_RB", &g1, 0, 4, 100_000, json!(2)));
    h.start();
    assert!(wait(|| !h.mock.results_for("l_RB").is_empty()));
    let (_, d1) = network(&h, 1);
    let (c2, d2) = network(&h, 2);
    // signed with the release key, which never signs a definition
    let (bytes, sig) = sign_definition(&definition(&h, 2, json!({"1": d1, "2": d2}), settings(100)), &h.key.key);
    let dref2 = h.mock.add_definition(bytes, &sig);
    h.mock.set_run_current(RUN, c2, dref2);
    let done = h.wait_done("l_RB");
    assert_eq!(done["given_back"], false, "{done}");
    assert!(done.get("refused").is_none(), "the lease ran: it is finished, not refused: {done}");
    assert_eq!(h.mock.count("GET", &format!("/v1/artifact/ckpt/{RUN}/2/")), 0);
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(definition_crashes(&h), 1, "{:?}", h.crashes());
    let seen: Value = serde_json::from_slice(&std::fs::read(h.layout.state().join("definitions.json")).unwrap()).unwrap();
    assert_eq!(seen, json!({"run/roll-run": 1}));
    h.stop();
}

#[test]
fn a_worker_without_the_feature_runs_the_lease_as_a_classic_one() {
    let mut h = H::with_build(&[("startup.json", br#"{"features": []}"#)]);
    let g1 = generation_1(&h, 100);
    h.push(rolling_lease(&h, "l_RO", &g1, 50, 6, 1000, json!(2)));
    h.start();
    let done = h.wait_done("l_RO");
    assert_eq!(done["given_back"], false, "{done}");
    assert!(done["unfinished"].is_null(), "{done}");
    assert_eq!(credited(&h, "l_RO"), 6, "the released ids, then done");
    assert!(extends(&h, "l_RO").is_empty(), "no extension for an old worker");
    let j = h.job_line();
    assert!(j.get("rolling").is_none() && j.get("flush_s").is_none(), "{j}");
    assert_eq!(j["games"], 6);
    assert!(!h.agent_log().contains("unknown op"), "no `extend` sent: {}", h.agent_log());
    h.stop();
}

/// Builds and serves an agent package for `v`.
fn publish(h: &H, v: &str) {
    let dir = h.dir.path().join(format!("pkg-{v}"));
    let launcher = std::fs::read(bin("ZeroFarm")).unwrap();
    let id = make_agent_package(&dir, v, &bin("fakeagent"), &[("ZeroFarm.exe", &launcher)], &h.key.key);
    h.mock.add_build_dir(&dir);
    h.mock.with(|s| s.latest = Some(json!({"version": v, "build_id": id, "manifest": format!("builds/{id}/manifest.json"), "min_agent": "0.1.0"})));
}

#[test]
fn a_ready_agent_update_finishes_the_lease_then_switches() {
    let mut h = H::new();
    h.timings.update_check = Duration::from_secs(1);
    let g1 = generation_1(&h, 100);
    h.push(rolling_lease(&h, "l_RU", &g1, 0, 4, 100_000, json!(2)));
    h.start();
    assert!(wait(|| !h.mock.results_for("l_RU").is_empty()));
    publish(&h, "0.2.0");
    assert!(wait(|| h.finished()), "{}", h.agent_log());
    assert_eq!(h.join(), EngineExit::Update("0.2.0".into()));
    let done = h.mock.done_for("l_RU").expect("done before switching");
    assert_eq!(done["given_back"], false, "{done}");
    assert!(h.agent_log().contains("rolling job ended (finish)"), "{}", h.agent_log());
    assert_eq!(h.layout.pending().as_deref(), Some("0.2.0"));
}

#[test]
fn a_lost_lease_stops_like_an_expired_one() {
    let mut h = H::new();
    let g1 = generation_1(&h, 100);
    h.push(rolling_lease(&h, "l_RL", &g1, 0, 4, 100, json!(2)));
    h.mock.script_extend("l_RL", json!({"status": 410}));
    h.start();
    let done = h.wait_done("l_RL");
    assert_eq!(done["given_back"], true, "{done}");
    assert_eq!(extends(&h, "l_RL").len(), 1);
    h.stop();
}

#[test]
fn classic_leases_are_unchanged() {
    let mut h = H::new();
    let g1 = generation_1(&h, 100);
    let mut l = rolling_lease(&h, "l_RC", &g1, 0, 6, 100, json!(2));
    l.as_object_mut().unwrap().remove("rolling");
    h.push(l);
    h.start();
    let done = h.wait_done("l_RC");
    assert_eq!(done["given_back"], false, "{done}");
    assert_eq!(credited(&h, "l_RC"), 6);
    assert!(extends(&h, "l_RC").is_empty());
    let j = h.job_line();
    for k in ["rolling", "flush_s", "conc", "groups"] {
        assert!(j.get(k).is_none(), "{k} in a classic CPU job line: {j}");
    }
    assert!(beats(&h, "l_RC").iter().all(|b| b.get("gen").is_none()), "classic beats carry no gen");
    assert!(done["stats"].get("released").is_none() && done["stats"].get("inflight").is_none(), "{done}");
    h.stop();
}

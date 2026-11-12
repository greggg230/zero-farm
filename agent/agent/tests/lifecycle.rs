//! Whole lease lifecycles with the real engine, the real fakeworker process and the mock
//! coordinator: download + verify, run, yield/suspend/resume, give-back after slack, upload
//! retry, restart with pending results, cancel, crashes, hangs, fuzz results, checkpoints.

mod harness;

use farm_agent::engine::EngineExit;
use farm_agent::results::{ResultMeta, ResultStore, Source};
use farm_agent::status::Phase;
use harness::{wait, H};
use serde_json::{json, Value};
use std::time::{Duration, Instant};

/// A beat for `lease` has reported at least `n` finished games (the worker is really playing).
fn progressed(h: &H, lease: &str, n: u64) -> bool {
    h.mock.with(|s| s.beats.iter().any(|(l, b)| l == lease && b["progress"]["games_done"].as_u64().unwrap_or(0) >= n))
}

/// A legacy self-play job; the fake worker's script travels in `settings.fake` (agents pass on
/// only a job's template keys since 0.2.5).
fn selfplay(games: u64, base: u64, fake: Value) -> Value {
    json!({"kind": "selfplay", "games": games, "gid_base": base, "seed": 7, "settings": {"fake": fake}})
}

#[test]
fn selfplay_lease_downloads_runs_yields_resumes_and_completes() {
    let mut h = H::new();
    h.push(h.lease("l_A", selfplay(8, 1000, json!({"game_ms": 150, "shard_every": 2, "status_ms": 200}))));
    h.start();
    assert!(wait(|| !h.mock.results_for("l_A").is_empty()), "first result");
    // The build came from the coordinator, verified: manifest, signature and the exe.
    assert!(h.mock.count("GET", &format!("/v1/artifact/builds/{}/", h.build_id)) >= 3);
    assert!(h.layout.cache_builds().join(&h.build_id).join("fakeworker.exe").exists());
    assert!(h.layout.is_healthy("0.1.0"), "healthy marker after the first successful call");

    // The user comes back: yield → suspended, lease kept alive with paused beats.
    h.user_active(true);
    assert!(wait(|| h.phase() == Phase::Paused));
    std::thread::sleep(Duration::from_millis(300));
    let n_paused = h.mock.results_for("l_A").len();
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(h.mock.results_for("l_A").len(), n_paused, "no progress while suspended");
    assert!(wait(|| h.mock.with(|s| s.beats.iter().any(|(l, b)| l == "l_A" && b["state"] == "paused"))));

    // Idle again: resume and finish.
    h.user_active(false);
    let done = h.wait_done("l_A");
    assert_eq!(done["given_back"], false, "{done}");
    assert_eq!(done["results"], json!([0, 1, 2, 3]));
    assert!(done.get("unfinished").map(Value::is_null).unwrap_or(true));
    assert!(done["stats"]["yields"].as_u64().unwrap() >= 1);
    assert!(done["stats"]["yield_ms_max"].as_u64().unwrap() < 2000);
    assert!(done["stats"]["paused_ms"].as_u64().unwrap() >= 1000);
    let results = h.mock.results_for("l_A");
    assert_eq!(results.len(), 4);
    let r0 = &results[0].1;
    assert_eq!(r0.headers["x-ids"], "[[1000,1002]]");
    assert!(!r0.headers.contains_key("x-gen"), "no checkpoint, no x-gen");
    assert!(String::from_utf8_lossy(&r0.bytes).contains("fake shard"));
    // The worker's job: no clipboard, atoms, desktops, display/system settings or logoff.
    assert!(String::from_utf8_lossy(&r0.bytes).contains("ui_limits=254"), "{}", String::from_utf8_lossy(&r0.bytes));
    // The worker got the job with our thread cap and paths.
    let gids: Vec<String> = results.iter().map(|(_, r)| r.headers["x-ids"].clone()).collect();
    assert_eq!(gids, ["[[1000,1002]]", "[[1002,1004]]", "[[1004,1006]]", "[[1006,1008]]"]);
    assert!(h.machine.lock().unwrap().games_today() >= 8);
    assert_eq!(h.stop(), EngineExit::Quit);
    assert!(!h.layout.results_pending().join("l_A").exists(), "closed leases leave nothing behind");
}

#[test]
fn paused_past_slack_gives_the_lease_back() {
    let mut h = H::new();
    let mut l = h.lease("l_B", selfplay(40, 500, json!({"game_ms": 100, "shard_every": 2})));
    l["slack_s"] = json!(1);
    h.push(l);
    h.start();
    assert!(wait(|| !h.mock.results_for("l_B").is_empty()));
    h.user_active(true);
    let done = h.wait_done("l_B");
    assert_eq!(done["given_back"], true);
    let delivered = h.mock.results_for("l_B").len() as u64;
    assert_eq!(done["results"].as_array().unwrap().len() as u64, delivered);
    // Unfinished = everything not delivered (finished games were flushed on quit).
    let unfinished = done["unfinished"]["gids"].as_array().unwrap().clone();
    let left: u64 = unfinished.iter().map(|r| r[1].as_u64().unwrap() - r[0].as_u64().unwrap()).sum();
    let games: u64 = h
        .mock
        .results_for("l_B")
        .iter()
        .map(|(_, r)| {
            let v: Value = serde_json::from_str(&r.headers["x-ids"]).unwrap();
            v.as_array().unwrap().iter().map(|p| p[1].as_u64().unwrap() - p[0].as_u64().unwrap()).sum::<u64>()
        })
        .sum();
    assert_eq!(left + games, 40, "{done}");
    h.stop();
}

#[test]
fn uploads_are_retried_after_injected_http_failures() {
    let mut h = H::new();
    h.mock.fail_next("PUT", "/v1/lease/l_C/result", 3, 503);
    h.mock.fail_next("POST", "/v1/lease/l_C/done", 1, 500);
    h.push(h.lease("l_C", selfplay(4, 0, json!({"game_ms": 50, "shard_every": 2}))));
    h.start();
    let done = h.wait_done("l_C");
    assert_eq!(done["given_back"], false);
    assert_eq!(h.mock.results_for("l_C").len(), 2);
    let failed = h.mock.with(|s| s.log.iter().filter(|l| l.path.starts_with("/v1/lease/l_C/result") && l.status == 503).count());
    assert_eq!(failed, 3);
    assert_eq!(h.mock.count("POST", "/v1/lease/l_C/done"), 2);
    h.stop();
}

#[test]
fn agent_restart_uploads_pending_results() {
    let mut h = H::new();
    h.mock.fail_next("PUT", "/v1/lease/l_D/result", 10_000, 503);
    h.push(h.lease("l_D", selfplay(4, 0, json!({"game_ms": 50, "shard_every": 2}))));
    h.start();
    let dir = h.layout.results_pending().join("l_D");
    assert!(wait(|| dir.join("done.json").exists()), "worker finished; done waits for uploads");
    assert_eq!(h.stop(), EngineExit::Quit);
    assert!(dir.join("0").exists() && dir.join("1").exists(), "results survive the agent stopping");
    assert!(h.mock.done_for("l_D").is_none());

    // A new agent process on the same root, coordinator healthy again.
    h.mock.clear_failures();
    h.start();
    let done = h.wait_done("l_D");
    assert_eq!(done["given_back"], false);
    assert_eq!(done["results"], json!([0, 1]));
    assert_eq!(h.mock.results_for("l_D").len(), 2);
    assert!(wait(|| !dir.exists()));
    h.stop();
}

#[test]
fn lease_orphaned_by_a_dead_agent_is_closed_with_unfinished_games() {
    let mut h = H::new();
    // What a killed agent leaves: lease.json + one result, no done.json. The coordinator knows the lease.
    let l = h.lease("l_E", selfplay(10, 300, json!({})));
    h.mock.with(|s| s.leases.insert("l_E".into(), mockcoord::LeaseState { lease: l.clone(), ..Default::default() }));
    let store = ResultStore::new(h.layout.clone());
    store.begin(&serde_json::from_value(l).unwrap()).unwrap();
    store
        .add("l_E", ResultMeta { kind: "shard".into(), games: 3, ids: Some(json!([[300, 303]])), ..Default::default() }, Source::Bytes(b"x"))
        .unwrap();
    h.start();
    let done = h.wait_done("l_E");
    assert_eq!(done["given_back"], true);
    assert_eq!(done["results"], json!([0]));
    assert_eq!(done["unfinished"], json!({"gids": [[303, 310]]}));
    assert_eq!(h.mock.results_for("l_E")[0].1.headers["x-ids"], "[[300,303]]");
    h.stop();
}

#[test]
fn lease_dirs_created_after_start_are_never_closed_as_orphans() {
    let mut h = H::new();
    h.start();
    assert!(wait(|| h.mock.count("POST", "/v1/lease") >= 1));
    // What a lease looks like in the instant between "directory created" and "worker started".
    let l = h.lease("l_Z", selfplay(10, 0, json!({})));
    h.mock.with(|s| s.leases.insert("l_Z".into(), mockcoord::LeaseState { lease: l.clone(), ..Default::default() }));
    ResultStore::new(h.layout.clone()).begin(&serde_json::from_value(l).unwrap()).unwrap();
    std::thread::sleep(Duration::from_millis(1500));
    assert!(h.mock.done_for("l_Z").is_none(), "a fresh lease directory must not be given back as an orphan");
    h.stop();
}

#[test]
fn cancel_stops_and_discards() {
    let mut h = H::new();
    h.mock.script_beat("l_F", json!({"action": "continue"}));
    h.mock.script_beat("l_F", json!({"action": "cancel"}));
    h.push(h.lease("l_F", selfplay(1000, 0, json!({"game_ms": 100, "shard_every": 1000}))));
    h.start();
    assert!(wait(|| h.mock.with(|s| s.leases.get("l_F").is_some_and(|l| l.cancelled))));
    assert!(wait(|| !h.layout.results_pending().join("l_F").exists()), "discarded");
    std::thread::sleep(Duration::from_millis(500));
    assert!(h.mock.done_for("l_F").is_none(), "no done for a cancelled lease");
    assert!(h.crashes().is_empty());
    assert!(wait(|| h.phase() == Phase::Waiting), "back to asking for work");
    h.stop();
}

#[test]
fn worker_crash_is_reported_and_the_lease_closed() {
    let mut h = H::new();
    h.push(h.lease("l_G", selfplay(10, 0, json!({"game_ms": 50, "shard_every": 2, "crash_after": 3}))));
    h.start();
    let done = h.wait_done("l_G");
    assert_eq!(done["given_back"], true);
    // 2 games delivered (one shard), games 2.. unfinished (game 3 finished but was not flushed).
    assert_eq!(done["unfinished"], json!({"gids": [[2, 10]]}));
    let c = h.crashes();
    assert_eq!(c.len(), 1, "{c:?}");
    assert_eq!(c[0]["kind"], "panic");
    assert_eq!(c[0]["exit_code"], 101);
    assert_eq!(c[0]["lease_id"], "l_G");
    assert_eq!(c[0]["build_id"], h.build_id.as_str());
    assert!(c[0]["backtrace"].as_str().unwrap().contains("fake crash after 3 games"));
    h.stop();
}

#[test]
fn worker_that_does_not_yield_in_2s_is_killed() {
    let mut h = H::new();
    h.timings.starve_cap = Duration::ZERO; // this test is about the timeout itself
    h.push(h.lease("l_H", selfplay(100, 0, json!({"game_ms": 100, "shard_every": 1, "yield_delay_ms": 5000}))));
    h.start();
    assert!(wait(|| !h.mock.results_for("l_H").is_empty()));
    let t = Instant::now();
    h.user_active(true);
    let done = h.wait_done("l_H");
    assert!(t.elapsed() < Duration::from_secs(10));
    assert_eq!(done["given_back"], true);
    assert!(h.crashes().iter().any(|c| c["message"].as_str().unwrap().contains("yield within 2 s")));
    h.stop();
}

#[test]
fn hung_worker_is_killed() {
    let mut h = H::new();
    h.timings.ping_after = Duration::from_secs(1);
    h.timings.hang_after = Duration::from_secs(3);
    h.timings.starve_cap = Duration::ZERO; // this test is about the timeout itself
    h.push(h.lease("l_I", selfplay(100, 0, json!({"game_ms": 50, "shard_every": 1, "hang_after": 2, "status_ms": 100000}))));
    h.start();
    let done = h.wait_done("l_I");
    assert_eq!(done["given_back"], true);
    // Game 0 was flushed; game 1 finished but the worker hung before writing it.
    assert_eq!(done["unfinished"], json!({"gids": [[1, 100]]}));
    assert!(h.crashes().iter().any(|c| c["message"].as_str().unwrap().contains("silent")));
    h.stop();
}

#[test]
fn worker_fatal_error_event_is_reported() {
    let mut h = H::new();
    h.push(h.lease("l_J", selfplay(10, 0, json!({"game_ms": 30, "fatal_after": 1}))));
    h.start();
    let done = h.wait_done("l_J");
    assert_eq!(done["given_back"], true);
    assert!(h.crashes().iter().any(|c| c["message"].as_str().unwrap().contains("fake fatal error")));
    h.stop();
}

#[test]
fn fuzz_lease_uploads_each_failure_then_a_summary() {
    let mut h = H::new();
    let mut l = h.lease(
        "l_K",
        json!({"kind": "fuzz", "mode": "delver", "first": 10_000, "count": 4000,
               "env": {"FUZZ_POOL": "delver", "FAKE": json!({"game_ms": 50, "chunk": 1000, "fail_seeds": [10_500, 13_999]}).to_string()}}),
    );
    l["campaign"] = json!("fuzz-delver");
    h.push(l);
    h.start();
    let done = h.wait_done("l_K");
    assert_eq!(done["given_back"], false);
    let results = h.mock.results_for("l_K");
    assert_eq!(results.len(), 3);
    let bodies: Vec<Value> = results.iter().map(|(_, r)| serde_json::from_slice(&r.bytes).unwrap()).collect();
    assert_eq!(bodies[0]["final"], false);
    assert_eq!(bodies[0]["failures"][0]["seed"], 10_500);
    assert_eq!(bodies[0]["failures"][0]["site"], "engine/src/fake.rs:42:9");
    assert_eq!(bodies[0]["campaign"], "fuzz-delver");
    let summary = &bodies[2];
    assert_eq!(summary["final"], true);
    assert_eq!(summary["done"], 4000);
    assert_eq!(summary["count"], 4000);
    assert_eq!(summary["failures"].as_array().unwrap().len(), 2);
    assert_eq!(summary["build_id"], h.build_id.as_str());
    // rev 2: per-failure uploads complete no seeds; the summary completes the whole range.
    assert!(!results[0].1.headers.contains_key("x-ids"));
    assert_eq!(results[2].1.headers["x-ids"], "[[10000,14000]]");
    assert_eq!(h.mock.with(|s| s.credits.iter().map(|c| c.2).sum::<u64>()), 4000);
    h.stop();
}

#[test]
fn new_checkpoint_from_a_beat_is_fetched_verified_and_switched_to() {
    let mut h = H::new();
    let m1 = b"onnx model gen 1".to_vec();
    let m2 = b"onnx model gen 2".to_vec();
    let s1 = h.mock.add_artifact("ckpt/eoe2/1/model.onnx", m1);
    let s2 = h.mock.add_artifact("ckpt/eoe2/2/model.onnx", m2);
    let r1 = h.mock.add_artifact("ckpt/eoe2/1/reference.npz", b"ref1".to_vec());
    let mut l = h.lease("l_L", selfplay(30, 0, json!({"game_ms": 100, "shard_every": 1})));
    l["run"] = json!("eoe2");
    l["checkpoint"] = json!({"gen": 1, "model": "ckpt/eoe2/1/model.onnx", "sha256": s1,
                             "reference": "ckpt/eoe2/1/reference.npz", "reference_sha256": r1});
    h.push(l);
    h.start();
    assert!(wait(|| !h.mock.results_for("l_L").is_empty()));
    // A wrong hash for gen 3 must never be used; gen 2 is fine.
    h.mock.script_beat("l_L", json!({"action": "continue", "checkpoint": {"gen": 3, "model": "ckpt/eoe2/2/model.onnx", "sha256": "0".repeat(64)}}));
    h.mock.script_beat("l_L", json!({"action": "continue", "checkpoint": {"gen": 2, "model": "ckpt/eoe2/2/model.onnx", "sha256": s2}}));
    let done = h.wait_done("l_L");
    assert_eq!(done["given_back"], false);
    let gens: Vec<String> = h.mock.results_for("l_L").iter().map(|(_, r)| r.headers.get("x-gen").cloned().unwrap_or_default()).collect();
    assert_eq!(gens.first().map(String::as_str), Some("1"));
    assert_eq!(gens.last().map(String::as_str), Some("2"), "{gens:?}");
    assert!(!gens.contains(&"3".to_string()));
    assert!(h.layout.cache_ckpt().join("eoe2").join("2").join("model.onnx").exists());
    h.stop();
}

#[test]
fn expired_lease_still_uploads_finished_results() {
    let mut h = H::new();
    h.push(h.lease("l_M", selfplay(1000, 0, json!({"game_ms": 100, "shard_every": 1}))));
    h.start();
    assert!(wait(|| progressed(&h, "l_M", 2)));
    h.mock.script_beat("l_M", json!({"action": "expire"}));
    assert!(wait(|| h.mock.with(|s| s.leases.get("l_M").is_some_and(|l| l.expired))));
    assert!(wait(|| !h.layout.results_pending().join("l_M").exists()));
    assert!(!h.mock.results_for("l_M").is_empty(), "finished games were uploaded after expiry");
    h.stop();
}

#[test]
fn give_back_before_the_worker_got_its_job_returns_everything() {
    let mut h = H::with_build(&[("startup.json", br#"{"hello_delay_ms": 2500}"#)]);
    h.mock.script_beat("l_U", json!({"action": "give_back"}));
    h.push(h.lease("l_U", selfplay(20, 40, json!({}))));
    h.start();
    let done = h.wait_done("l_U");
    assert_eq!(done["given_back"], true);
    assert_eq!(done["results"], json!([]));
    assert_eq!(done["unfinished"], json!({"gids": [[40, 60]]}));
    h.stop();
}

#[test]
fn coordinator_give_back_flushes_and_closes() {
    let mut h = H::new();
    h.push(h.lease("l_N", selfplay(1000, 0, json!({"game_ms": 100, "shard_every": 1000}))));
    h.start();
    assert!(wait(|| progressed(&h, "l_N", 2)));
    h.mock.script_beat("l_N", json!({"action": "give_back"}));
    let done = h.wait_done("l_N");
    assert_eq!(done["given_back"], true);
    // The quit flushed the finished-but-unwritten games into one last shard.
    let log: Vec<String> = h.mock.with(|s| s.log.iter().map(|l| format!("{} {} {}", l.method, l.path, l.status)).collect());
    assert_eq!(h.mock.results_for("l_N").len(), 1, "{done}
{log:#?}");
    h.stop();
}

fn tampered(h: &H) {
    let key = format!("builds/{}/fakeworker.exe", h.build_id);
    h.mock.with(|s| {
        let b = s.artifacts.get_mut(&key).unwrap();
        let n = b.len();
        b[n - 100] ^= 0x55;
    });
}

#[test]
fn tampered_build_is_never_run() {
    let mut h = H::new();
    tampered(&h);
    h.push(h.lease("l_O", selfplay(4, 0, json!({}))));
    h.start();
    let done = h.wait_done("l_O");
    assert_eq!(done["given_back"], true);
    assert!(done["stats"]["error"].as_str().unwrap().contains("sha256"), "{done}");
    assert_eq!(done["unfinished"], json!({"gids": [[0, 4]]}));
    assert!(!h.layout.cache_builds().join(&h.build_id).exists(), "nothing unverified lands in the cache");
    assert!(h.mock.results_for("l_O").is_empty());
    h.stop();
}

#[test]
fn build_signed_by_an_unknown_key_is_never_run() {
    let mut h = H::new();
    // Re-sign the same files with a key the agent doesn't trust, serve that instead.
    let other = mockcoord::testkit::test_key();
    let bdir = h.dir.path().join("evil");
    let evil_id = mockcoord::testkit::make_worker_build(&bdir, &other.key, &mockcoord::testkit::bin("fakeworker"), &[]);
    h.mock.add_build_dir(&bdir);
    let mut l = h.lease("l_P", selfplay(4, 0, json!({})));
    l["build"] = json!({"build_id": evil_id, "manifest": format!("builds/{evil_id}/manifest.json")});
    h.push(l);
    h.start();
    let done = h.wait_done("l_P");
    assert_eq!(done["given_back"], true);
    assert!(done["stats"]["error"].as_str().unwrap().contains("unknown key"), "{done}");
    h.stop();
}

#[test]
fn bad_signature_is_never_run() {
    let mut h = H::new();
    // Swap in a signature over different bytes (valid key, wrong data).
    let sig = h.key.key.sign(b"something else", &format!("build_id={} kind=worker", h.build_id)).unwrap();
    h.mock.with(|s| s.artifacts.insert(format!("builds/{}/manifest.json.minisig", h.build_id), sig.into_bytes()));
    h.push(h.lease("l_Q", selfplay(4, 0, json!({}))));
    h.start();
    let done = h.wait_done("l_Q");
    assert!(done["stats"]["error"].as_str().unwrap().contains("signature does not verify"), "{done}");
    h.stop();
}

#[test]
fn worker_must_say_hello_within_10s() {
    let mut h = H::with_build(&[("startup.json", br#"{"hello_delay_ms": 60000}"#)]);
    // (The first start of a build gets 60 s for antivirus scans; the plain deadline is tested here.)
    h.timings.hello_first = Duration::from_secs(10);
    h.timings.starve_cap = Duration::ZERO; // this test is about the timeout itself
    h.push(h.lease("l_R", selfplay(4, 0, json!({}))));
    h.start();
    let done = h.wait_done("l_R");
    assert_eq!(done["given_back"], true);
    assert!(h.crashes().iter().any(|c| c["message"].as_str().unwrap().contains("no hello")));
    h.stop();
}

#[test]
fn disk_cap_too_small_gives_the_lease_back() {
    let mut h = H::new();
    h.machine.lock().unwrap().settings.disk_cap_mb = 0;
    h.push(h.lease("l_S", selfplay(4, 0, json!({}))));
    h.start();
    let done = h.wait_done("l_S");
    assert!(done["stats"]["error"].as_str().unwrap().contains("disk space"), "{done}");
    h.stop();
}

#[test]
fn profile_is_resent_when_the_coordinator_asks() {
    let mut h = H::new();
    // The agent believes the coordinator knows its digest; the coordinator doesn't.
    std::fs::create_dir_all(h.layout.state()).unwrap();
    std::fs::write(h.layout.state().join("profile-digest"), harness::fake_profile().digest()).unwrap();
    h.mock.with(|s| s.need_profile = true);
    h.start();
    assert!(wait(|| h.mock.with(|s| s.lease_requests.len() >= 2)));
    let reqs = h.mock.with(|s| s.lease_requests.clone());
    assert!(reqs[0]["profile"].is_null());
    assert_eq!(reqs[1]["profile"]["cpu"], "Test CPU");
    assert_eq!(reqs[1]["caps"]["threads"], 6, "physical cores - 2");
    assert_eq!(reqs[1]["kinds"], json!(["fuzz", "eval", "selfplay", "diag"]));
    // Afterwards the profile is not sent again.
    assert!(wait(|| h.mock.with(|s| s.lease_requests.len() >= 3)));
    assert!(h.mock.with(|s| s.lease_requests[2]["profile"].is_null()));
    h.stop();
}

#[test]
fn no_lease_is_requested_while_the_user_is_active() {
    let mut h = H::new();
    h.user_active(true);
    h.push(h.lease("l_T", selfplay(4, 0, json!({}))));
    h.start();
    assert!(wait(|| h.phase() == Phase::Idle));
    std::thread::sleep(Duration::from_millis(800));
    assert_eq!(h.mock.with(|s| s.lease_requests.len()), 0);
    h.user_active(false);
    h.wait_done("l_T");
    h.stop();
}

#[test]
fn revoked_token_stops_the_engine() {
    // Only the coordinator's own `revoked` (marked x-zerofarm-coordinator: 1) ends it.
    let mut h = H::new();
    let t = h.token.clone();
    h.mock.with(|s| {
        s.machines.remove(&t);
        s.revoked.insert(t.clone());
    });
    h.start();
    assert_eq!(h.join(), EngineExit::Revoked);
}

#[test]
fn an_unknown_token_is_kept_and_retried_later() {
    // The coordinator says `unauthorized`: keep the token, tell the friend, back off (M1).
    let mut h = H::new();
    h.token = "not-a-known-token".into();
    h.start();
    assert!(wait(|| h.phase() == Phase::SignInFailed), "phase {:?}", h.phase());
    assert_eq!(h.status.lock().unwrap().headline(), "This PC can't sign in to the farm; ask Gregg.");
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(h.mock.count("POST", "/v1/lease"), 1, "one try, then a 1 min wait");
    assert!(!h.finished(), "the engine keeps running");
    assert_eq!(h.stop(), EngineExit::Quit);
}

#[test]
fn a_403_from_something_in_front_of_the_coordinator_is_a_network_error() {
    // A WAF or proxy answers 403 with an HTML page, without the coordinator's header: the agent
    // must not forget its enrollment (security review M1); it retries and works on.
    let mut h = H::new();
    let waf = "<html><body>Sorry, you have been blocked (Cloudflare error 1020)</body></html>";
    h.mock.fail_next_raw("POST", "/v1/lease", 2, 403, waf);
    h.mock.fail_next_raw("PUT", "/v1/lease/", 1, 401, waf);
    h.mock.fail_next_raw("POST", "/v1/lease/l_W/beat", 1, 403, waf);
    h.push(h.lease("l_W", selfplay(4, 0, json!({"game_ms": 100, "shard_every": 2}))));
    h.start();
    let done = h.wait_done("l_W");
    assert_eq!(done["given_back"], false, "{done}");
    assert_eq!(h.mock.results_for("l_W").len(), 2, "both chunks up after the refused upload was retried");
    assert!(h.mock.count("POST", "/v1/lease") >= 3, "the two refused lease requests were retried");
    assert!(!h.finished());
    assert_eq!(h.stop(), EngineExit::Quit);
}

#[test]
fn quit_event_stops_the_engine_and_launcher_events_are_reported() {
    let mut h = H::new();
    h.layout
        .push_launcher_event(&zerofarm_common::layout::LauncherEvent {
            kind: "rollback".into(),
            from: "0.2.0".into(),
            to: Some("0.1.0".into()),
            reason: "did not write healthy-0.2.0 within 120 s".into(),
            t: 1,
        })
        .unwrap();
    h.start();
    assert!(wait(|| h.crashes().iter().any(|c| c["kind"] == "agent")));
    let c = h.crashes();
    assert!(c[0]["message"].as_str().unwrap().contains("rollback"));
    assert_eq!(c[0]["agent"], "0.1.0");
    zerofarm_common::names::QuitEvent::open(&h.layout.root).unwrap().set();
    assert!(wait(|| h.finished()));
    assert_eq!(h.join(), EngineExit::Quit);
}

// ---- PROTOCOL v1 rev 2 behaviours ----

#[test]
fn stale_chunks_are_dropped_and_left_out_of_done() {
    let mut h = H::new();
    let m1 = h.mock.add_artifact("ckpt/eoe2/1/model.onnx", b"model".to_vec());
    let mut l = h.lease("l_V", selfplay(4, 0, json!({"game_ms": 50, "shard_every": 1})));
    l["run"] = json!("eoe2");
    l["checkpoint"] = json!({"gen": 1, "model": "ckpt/eoe2/1/model.onnx", "sha256": m1});
    // The run moved on: everything played with gen 1 is stale (410).
    h.mock.with(|s| s.stale_below_gen = Some(3));
    h.push(l);
    h.start();
    let done = h.wait_done("l_V");
    assert_eq!(done["results"], json!([]), "refused chunks are not claimed in done");
    assert!(h.mock.results_for("l_V").is_empty());
    let refused = h.mock.with(|s| s.log.iter().filter(|x| x.path.starts_with("/v1/lease/l_V/result") && x.status == 410).count());
    assert_eq!(refused, 4, "each chunk tried once, then dropped (not retried)");
    h.stop();
}

#[test]
fn done_missing_results_are_reconciled() {
    let mut h = H::new();
    // The coordinator acknowledges the PUTs but loses them: done answers 409 {"missing"}.
    h.mock.with(|s| s.forget_results = true);
    h.push(h.lease("l_W", selfplay(4, 0, json!({"game_ms": 30, "shard_every": 2}))));
    h.start();
    let done = h.wait_done("l_W");
    assert_eq!(done["results"], json!([]));
    assert!(h.mock.with(|s| s.log.iter().any(|x| x.path == "/v1/lease/l_W/done" && x.status == 409)));
    assert!(wait(|| !h.layout.results_pending().join("l_W").exists()));
    h.stop();
}

#[test]
fn upload_diag_in_a_lease_answer_sends_logs() {
    let mut h = H::new();
    h.mock.with(|s| s.upload_diag_next = true);
    h.start();
    assert!(wait(|| h.mock.with(|s| s.diags.iter().any(|(n, _)| n == "logs"))));
    h.stop();
}

#[test]
fn repeated_newer_checkpoint_is_fetched_once_and_a_bad_one_is_not_retried() {
    let mut h = H::new();
    let m1 = h.mock.add_artifact("ckpt/eoe2/1/model.onnx", b"m1".to_vec());
    let m2 = h.mock.add_artifact("ckpt/eoe2/2/model.onnx", b"m2".to_vec());
    h.mock.add_artifact("ckpt/eoe2/3/model.onnx", b"m3".to_vec());
    let mut l = h.lease("l_X", selfplay(60, 0, json!({"game_ms": 100, "shard_every": 1})));
    l["run"] = json!("eoe2");
    l["checkpoint"] = json!({"gen": 1, "model": "ckpt/eoe2/1/model.onnx", "sha256": m1});
    h.push(l);
    h.start();
    assert!(wait(|| !h.mock.results_for("l_X").is_empty()));
    // A bad gen 3 repeated in three beats, then a good gen 2 repeated in three beats.
    let bad = json!({"gen": 3, "model": "ckpt/eoe2/3/model.onnx", "sha256": "0".repeat(64)});
    let good = json!({"gen": 2, "model": "ckpt/eoe2/2/model.onnx", "sha256": m2});
    for c in [&bad, &bad, &bad, &good, &good, &good] {
        h.mock.script_beat("l_X", json!({"action": "continue", "checkpoint": c}));
    }
    assert!(wait(|| h.mock.with(|s| s.beat_script.get("l_X").is_none_or(|q| q.is_empty()))));
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(h.mock.count("GET", "/v1/artifact/ckpt/eoe2/3/"), 1, "bad checkpoint downloaded once");
    assert_eq!(h.mock.count("GET", "/v1/artifact/ckpt/eoe2/2/"), 1, "good checkpoint downloaded once");
    h.wait_done("l_X");
    h.stop();
}

#[test]
fn ids_credited_elsewhere_are_reported_but_harmless() {
    let mut h = H::new();
    // Two leases over the same game ids (a reissue after a dropout): the second copy is a duplicate.
    let mut a = h.lease("l_Y1", selfplay(4, 100, json!({"game_ms": 30, "shard_every": 2})));
    let mut b = h.lease("l_Y2", selfplay(4, 100, json!({"game_ms": 30, "shard_every": 2})));
    a["run"] = json!("eoe2");
    b["run"] = json!("eoe2");
    h.push(a);
    h.push(b);
    h.start();
    h.wait_done("l_Y1");
    h.wait_done("l_Y2");
    let credits = h.mock.with(|s| s.credits.clone());
    let first: u64 = credits.iter().filter(|c| c.0 == "l_Y1").map(|c| c.2).sum();
    let dup: u64 = credits.iter().filter(|c| c.0 == "l_Y2").map(|c| c.3).sum();
    assert_eq!((first, dup), (4, 4), "{credits:?} {:?} {:?} crashes {:?}", h.mock.done_for("l_Y1"), h.mock.done_for("l_Y2"), h.crashes());
    h.stop();
}

#[test]
fn lease_requests_carry_the_agent_version() {
    let mut h = H::new();
    h.start();
    assert!(wait(|| h.mock.count("POST", "/v1/lease") >= 1));
    let ok = h.mock.with(|s| s.log.iter().filter(|x| x.path == "/v1/lease").all(|x| x.agent.as_deref() == Some("0.1.0") && x.status == 200));
    assert!(ok);
    h.stop();
}

#[test]
fn starved_past_slack_gives_the_lease_back() {
    // The PC is saturated by other programs: the idle-priority worker gets no CPU. That counts
    // toward the slack like a pause (Stats starved_ms), so the games go to a PC that has time.
    let mut h = H::new();
    let mut l = h.lease("l_ST", selfplay(400, 9000, json!({"game_ms": 100, "shard_every": 2})));
    l["slack_s"] = json!(2);
    h.push(l);
    h.start();
    assert!(wait(|| !h.mock.results_for("l_ST").is_empty()));
    h.signals.set_starved(true);
    let done = h.wait_done("l_ST");
    assert_eq!(done["given_back"], true, "{done}");
    // Whole sampling windows (250 ms here) up to the give-back, 2 s after the stall began.
    assert!(done["stats"]["starved_ms"].as_u64().unwrap() >= 1500, "{done}");
    assert_eq!(done["stats"]["paused_ms"], 0, "starved, never paused");
    assert!(h.mock.with(|s| s.beats.iter().any(|(l, b)| l == "l_ST" && b["stats"]["starved_ms"].as_u64().unwrap_or(0) > 0)));
    h.signals.set_starved(false);
    assert_eq!(h.stop(), EngineExit::Quit);
}

#[test]
fn brief_starvation_is_counted_but_the_lease_is_kept() {
    let mut h = H::new();
    h.push(h.lease("l_SB", selfplay(40, 9500, json!({"game_ms": 100, "shard_every": 5}))));
    h.start();
    assert!(wait(|| !h.mock.results_for("l_SB").is_empty()));
    h.signals.set_starved(true);
    std::thread::sleep(Duration::from_millis(1200));
    h.signals.set_starved(false);
    let done = h.wait_done("l_SB");
    assert_eq!(done["given_back"], false, "{done}");
    let starved = done["stats"]["starved_ms"].as_u64().unwrap();
    assert!((500..5000).contains(&starved), "{done}");
    assert_eq!(h.stop(), EngineExit::Quit);
}

#[test]
fn a_pause_then_starvation_add_up_toward_the_slack() {
    let mut h = H::new();
    let mut l = h.lease("l_PS", selfplay(400, 9900, json!({"game_ms": 100, "shard_every": 2})));
    l["slack_s"] = json!(3);
    h.push(l);
    h.start();
    assert!(wait(|| !h.mock.results_for("l_PS").is_empty()));
    let t0 = Instant::now();
    h.user_active(true);
    assert!(wait(|| h.phase() == Phase::Paused));
    std::thread::sleep(Duration::from_millis(1500));
    // Back, but now the PC is busy: no CPU for the worker. The stall started with the pause.
    h.signals.set_starved(true);
    h.user_active(false);
    let done = h.wait_done("l_PS");
    assert_eq!(done["given_back"], true, "{done}");
    assert!(t0.elapsed() < Duration::from_secs(10), "given back after ~3 s of pause + starvation, not 3 s of each");
    assert!(done["stats"]["paused_ms"].as_u64().unwrap() >= 1000, "{done}");
    assert!(done["stats"]["starved_ms"].as_u64().unwrap() > 0, "{done}");
    h.signals.set_starved(false);
    assert_eq!(h.stop(), EngineExit::Quit);
}

#[test]
fn availability_is_recorded_per_day() {
    let mut h = H::new();
    h.start();
    std::thread::sleep(Duration::from_millis(1500)); // idle: allowed to work (no work offered)
    h.user_active(true);
    std::thread::sleep(Duration::from_millis(1000));
    assert_eq!(h.stop(), EngineExit::Quit);
    let hist = farm_agent::availability::load(&h.layout);
    let today = hist.first_day.clone().expect("a day recorded");
    let d = hist.days[&today];
    assert!(d.on_s >= 2.0 && d.on_s < 10.0, "{d:?}");
    assert!(d.idle_s >= 1.0 && d.idle_s < d.on_s - 0.5, "{d:?}");
    // Not a complete day yet: nothing in the profile.
    assert_eq!(hist.means(&today), None);
}

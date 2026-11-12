//! A worker Windows refuses to start (PROTOCOL 3, rev 5). Found on a friend's PC with Smart App
//! Control on 2026-11-12: every lease failed with os error 4551 and agent 0.2.1 leased again at
//! once, 521 times in 45 minutes. Now: one `blocked` report per build per day, the lease given
//! back, a growing wait before the next lease request, and a tray message; any other start
//! failure is a `crash` report and a wait too. The start failure is scripted through
//! `AgentConfig::test_spawn_error` (debug builds only).

mod harness;

use farm_agent::status::{Phase, BLOCKED_MESSAGE};
use harness::{wait, H};
use serde_json::json;
use std::time::{Duration, Instant};

fn selfplay(games: u64, base: u64) -> serde_json::Value {
    json!({"kind": "selfplay", "games": games, "gid_base": base, "seed": 7, "settings": {}, "fake": {"game_ms": 50}})
}

fn lease_requests(h: &H) -> usize {
    h.mock.with(|s| s.lease_requests.len())
}

#[test]
fn a_blocked_worker_is_reported_once_and_the_agent_waits_instead_of_leasing_again() {
    let mut h = H::new();
    h.spawn_error = Some(4551);
    for i in 0..3 {
        h.push(h.lease(&format!("l_BL{i}"), selfplay(10, 100 * i)));
    }
    h.start();
    let done = h.wait_done("l_BL0");
    assert_eq!(done["given_back"], true);
    assert_eq!(done["results"], json!([]));
    assert_eq!(done["unfinished"], json!({"gids": [[0, 10]]}));
    assert!(done["stats"]["error"].as_str().unwrap().contains("could not start worker"), "{done}");

    let c = h.crashes();
    assert_eq!(c.len(), 1, "{c:?}");
    assert_eq!(c[0]["kind"], "blocked");
    assert_eq!(c[0]["os_error"], 4551);
    assert_eq!(c[0]["build_id"], h.build_id.as_str());
    assert_eq!(c[0]["lease_id"], "l_BL0");
    assert!(c[0]["message"].as_str().unwrap().contains("Smart App Control"), "{c:?}");

    // No lease storm: the first wait is 15 minutes.
    assert!(wait(|| h.phase() == Phase::Blocked));
    std::thread::sleep(Duration::from_secs(3));
    assert_eq!(lease_requests(&h), 1, "one lease request, then waiting");
    let st = h.status.lock().unwrap().clone();
    assert_eq!(st.detail, BLOCKED_MESSAGE);
    assert_eq!(st.toast.as_deref(), Some(BLOCKED_MESSAGE), "a balloon tells the friend once");
    assert!(st.headline().contains("Smart App Control"), "{}", st.headline());

    // The wait survives a restart of the agent.
    h.stop();
    assert!(h.layout.state().join("start-backoff.json").exists());
    h.start();
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(lease_requests(&h), 1, "still waiting after a restart");
    assert_eq!(h.phase(), Phase::Blocked);
    h.stop();
}

#[test]
fn the_wait_grows_and_each_build_is_reported_once_a_day() {
    let mut h = H::new();
    h.spawn_error = Some(4551);
    // Steps of 1 s, 4 s, 16 s... (the real ones are 15 min, 1 h, 4 h, 16 h, 24 h).
    h.timings.blocked_backoff = Duration::from_secs(1);
    for i in 0..5 {
        h.push(h.lease(&format!("l_BG{i}"), selfplay(4, 10 * i)));
    }
    let t0 = Instant::now();
    h.start();
    h.wait_done("l_BG0");
    h.wait_done("l_BG1");
    let second = t0.elapsed();
    h.wait_done("l_BG2");
    let third = t0.elapsed();
    assert!(second >= Duration::from_millis(900), "1 s before the second lease: {second:?}");
    assert!(third - second >= Duration::from_millis(3500), "4 s before the third: {:?}", third - second);
    // The next wait is 16 s: nothing more for a while.
    std::thread::sleep(Duration::from_secs(3));
    assert_eq!(lease_requests(&h), 3, "three leases in ~8 s, not a storm");
    let c = h.crashes();
    assert_eq!(c.len(), 1, "the same build is reported once a day: {c:?}");
    assert_eq!(c[0]["kind"], "blocked");
    h.stop();
}

#[test]
fn an_older_coordinator_gets_the_blocked_report_as_a_crash() {
    let mut h = H::new();
    h.spawn_error = Some(1260);
    h.mock.with(|s| s.refuse_blocked = true);
    h.push(h.lease("l_BO", selfplay(4, 0)));
    h.start();
    h.wait_done("l_BO");
    assert!(wait(|| !h.crashes().is_empty()));
    let c = h.crashes();
    assert_eq!(c.len(), 1, "{c:?}");
    assert_eq!(c[0]["kind"], "crash");
    assert!(c[0].get("os_error").is_none());
    assert!(c[0]["message"].as_str().unwrap().contains("blocked by policy (os error 1260)"), "{c:?}");
    assert!(c[0]["message"].as_str().unwrap().contains("AppLocker"), "{c:?}");
    h.stop();
}

#[test]
fn any_other_start_failure_is_a_crash_report_and_a_wait() {
    let mut h = H::new();
    h.spawn_error = Some(5); // ERROR_ACCESS_DENIED: not a policy block
    h.push(h.lease("l_BX", selfplay(4, 0)));
    h.push(h.lease("l_BY", selfplay(4, 10)));
    h.start();
    let done = h.wait_done("l_BX");
    assert_eq!(done["given_back"], true);
    let c = h.crashes();
    assert_eq!(c.len(), 1, "{c:?}");
    assert_eq!(c[0]["kind"], "crash");
    assert!(c[0].get("os_error").is_none());
    assert!(c[0]["message"].as_str().unwrap().contains("could not start worker"), "{c:?}");
    std::thread::sleep(Duration::from_secs(3));
    assert_eq!(lease_requests(&h), 1, "a minute before the next try");
    let st = h.status.lock().unwrap().clone();
    assert_eq!(st.phase, Phase::Waiting);
    assert!(st.detail.contains("could not start"), "{}", st.detail);
    h.stop();
}

#[test]
fn a_worker_that_starts_ends_the_wait() {
    let mut h = H::new();
    h.spawn_error = Some(4551);
    h.timings.blocked_backoff = Duration::from_secs(1);
    h.push(h.lease("l_BR0", selfplay(4, 0)));
    h.start();
    h.wait_done("l_BR0");
    assert!(wait(|| h.phase() == Phase::Blocked));
    h.stop();
    assert!(h.layout.state().join("start-backoff.json").exists());
    // Smart App Control turned off: the worker starts, and the wait is over.
    h.spawn_error = None;
    h.push(h.lease("l_BR1", selfplay(4, 10)));
    h.start();
    let done = h.wait_done("l_BR1");
    assert_eq!(done["given_back"], false, "{done}");
    assert!(!h.layout.state().join("start-backoff.json").exists(), "reset at hello");
    assert_eq!(h.crashes().len(), 1);
    h.stop();
}

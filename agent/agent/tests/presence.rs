//! rev 16 (PROTOCOL 3, `POST /v1/presence`): while the idle policy blocks work the agent says why
//! - at once when the reason changes, then every `presence` - and stops while it works.

mod harness;

use harness::{wait, H};
use serde_json::{json, Value};
use std::time::Duration;

fn reasons(h: &H) -> Vec<String> {
    h.mock.with(|s| s.presences.iter().map(|p| p["reason"].as_str().unwrap_or("?").to_string()).collect())
}

#[test]
fn blocked_agent_sends_its_reason_and_stops_while_working() {
    let mut h = H::new();
    h.user_active(true);
    h.start();
    // In use: sent at once, then repeated (fast timings: every 300 ms).
    assert!(wait(|| reasons(&h).len() >= 3), "presences: {:?}", reasons(&h));
    assert!(reasons(&h).iter().all(|r| r == "user_active"), "{:?}", reasons(&h));
    assert_eq!(h.mock.count("POST", "/v1/lease"), 0, "no lease asked for while in use");

    // A different reason is sent without waiting for the interval.
    h.signals.set(|s| {
        s.idle_s = 3600;
        s.on_ac = Some(false);
    });
    assert!(wait(|| reasons(&h).last().map(String::as_str) == Some("on_battery")), "{:?}", reasons(&h));

    // Allowed to work: it asks for leases and the presences stop.
    h.signals.set(|s| s.on_ac = Some(true));
    assert!(wait(|| h.mock.count("POST", "/v1/lease") >= 1));
    std::thread::sleep(Duration::from_millis(200));
    let n = reasons(&h).len();
    std::thread::sleep(Duration::from_millis(1000));
    assert_eq!(reasons(&h).len(), n, "no presence while allowed to work");
    h.stop();
}

#[test]
fn a_coordinator_without_presence_changes_nothing() {
    let mut h = H::new();
    h.mock.fail_next("POST", "/v1/presence", 100, 404);
    h.user_active(true);
    h.start();
    assert!(wait(|| h.mock.count("POST", "/v1/presence") >= 3));
    h.user_active(false);
    h.push(h.lease("l_P", json!({"kind": "selfplay", "games": 2, "gid_base": 0, "seed": 7, "settings": {"fake": {"game_ms": 50}}})));
    let done: Value = h.wait_done("l_P");
    assert_eq!(done["given_back"], false, "{done}");
    h.stop();
}

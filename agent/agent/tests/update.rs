//! Self-update, in process (PROTOCOL 6 agent side): find a newer version, download and verify
//! it into versions\<v>\, write state\pending and exit 75 — or refuse.

mod harness;

use farm_agent::engine::EngineExit;
use harness::{wait, H};
use mockcoord::testkit::{bin, make_agent_package};
use serde_json::json;
use std::time::Duration;
use zerofarm_common::manifest::{verify_dir, Expect, Kind};

/// Builds and serves an agent package for `v`; returns its build id.
fn publish(h: &H, v: &str, tamper: bool) -> String {
    let dir = h.dir.path().join(format!("pkg-{v}"));
    let launcher = std::fs::read(bin("ZeroFarm")).unwrap();
    let id = make_agent_package(&dir, v, &bin("fakeagent"), &[("ZeroFarm.exe", &launcher)], &h.key.key);
    h.mock.add_build_dir(&dir);
    if tamper {
        h.mock.with(|s| {
            let b = s.artifacts.get_mut(&format!("builds/{id}/farm-agent.exe")).unwrap();
            b[1000] ^= 1;
        });
    }
    h.mock.with(|s| s.latest = Some(json!({"version": v, "build_id": id, "manifest": format!("builds/{id}/manifest.json"), "min_agent": "0.1.0"})));
    id
}

#[test]
fn newer_latest_is_downloaded_verified_and_switched_to() {
    let mut h = H::new();
    let id = publish(&h, "0.2.0", false);
    h.start();
    assert_eq!(h.join(), EngineExit::Update("0.2.0".into()));
    assert_eq!(h.layout.pending().as_deref(), Some("0.2.0"));
    let v = verify_dir(&h.layout.version_dir("0.2.0"), &h.config().keys, &Expect { build_id: Some(&id), kind: Some(Kind::Agent), version: Some("0.2.0") });
    assert!(v.is_ok(), "{v:?}");
}

#[test]
fn update_signal_in_a_lease_answer_triggers_the_update() {
    let mut h = H::new();
    h.start();
    // First check found nothing (404); now the coordinator demands 0.2.0.
    assert!(wait(|| h.mock.count("POST", "/v1/lease") >= 1));
    publish(&h, "0.2.0", false);
    h.mock.with(|s| s.update_min_agent = Some("0.2.0".into()));
    assert!(wait(|| h.finished()));
    assert_eq!(h.join(), EngineExit::Update("0.2.0".into()));
}

#[test]
fn tampered_agent_update_is_refused() {
    let mut h = H::new();
    publish(&h, "0.2.0", true);
    h.start();
    assert!(wait(|| h.mock.count("GET", "/v1/artifact/builds/") >= 3));
    std::thread::sleep(Duration::from_millis(800));
    assert!(!h.finished(), "must not exit for an unverifiable version");
    assert_eq!(h.layout.pending(), None);
    assert!(!h.layout.version_dir("0.2.0").exists());
    assert_eq!(h.stop(), EngineExit::Quit);
}

#[test]
fn version_rolled_back_recently_is_not_retried() {
    let mut h = H::new();
    publish(&h, "0.2.0", false);
    h.layout.mark_bad("0.2.0").unwrap();
    h.start();
    assert!(wait(|| h.mock.count("GET", "/v1/agent/latest") >= 1));
    std::thread::sleep(Duration::from_millis(800));
    assert!(!h.finished());
    assert_eq!(h.mock.count("GET", "/v1/artifact/"), 0, "no download of a bad version");
    assert_eq!(h.stop(), EngineExit::Quit);
}

#[test]
fn required_update_that_cannot_happen_does_not_spin() {
    // The canary test PC, 2026-11-11: the launcher rolled back 0.1.2; the coordinator kept answering
    // {"update": min 0.1.2}; 0.1.1 skipped the bad version and re-leased ~8 times a second.
    // Now it waits (max_wait, 500 ms in the harness; 5 min for real) between attempts.
    let mut h = H::new();
    publish(&h, "0.2.0", false);
    h.layout.mark_bad("0.2.0").unwrap();
    h.mock.with(|s| s.update_min_agent = Some("0.2.0".into()));
    h.start();
    assert!(wait(|| h.mock.count("POST", "/v1/lease") >= 1));
    std::thread::sleep(Duration::from_millis(1500));
    let n = h.mock.count("POST", "/v1/lease");
    assert!(n <= 6, "agent re-leased {n} times in 1.5 s while its required update can't happen");
    assert!(!h.finished());
    assert_eq!(h.mock.count("GET", "/v1/artifact/"), 0, "no download of a bad version");
    assert_eq!(h.stop(), EngineExit::Quit);
}

#[test]
fn older_or_equal_latest_is_ignored() {
    let mut h = H::new();
    h.version = "0.2.0".into();
    publish(&h, "0.2.0", false);
    h.start();
    assert!(wait(|| h.mock.count("POST", "/v1/lease") >= 2));
    assert_eq!(h.mock.count("GET", "/v1/artifact/"), 0);
    assert_eq!(h.stop(), EngineExit::Quit);
}

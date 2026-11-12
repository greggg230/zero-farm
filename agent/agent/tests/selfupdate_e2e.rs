//! Self-update end to end with real processes: ZeroFarm.exe (launcher) runs the real
//! farm-agent.exe from a temp root; the mock coordinator publishes a newer version; the agent
//! downloads it, exits 75, the launcher switches. A version that never writes healthy is rolled
//! back, reported by the old agent, and not downloaded again.

use farm_agent::settings::{Machine, Pause};
use mockcoord::testkit::{bin, make_agent_package, make_agent_version, test_key, wait_until, KillOnDrop, TestKey, TEST_KEYS_ENV};
use mockcoord::MockCoord;
use serde_json::json;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;
use zerofarm_common::layout::Layout;

const T: Duration = Duration::from_secs(60);

struct E {
    dir: tempfile::TempDir,
    root: PathBuf,
    layout: Layout,
    mock: MockCoord,
    key: TestKey,
}

fn env() -> E {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("ZeroFarm");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::copy(bin("ZeroFarm"), root.join("ZeroFarm.exe")).unwrap();
    let layout = Layout::new(&root);
    let key = test_key();
    let mock = MockCoord::start();
    mock.trust(&key.pubkey);
    let launcher = std::fs::read(bin("ZeroFarm")).unwrap();
    make_agent_version(&layout, "0.1.0", &bin("farm-agent"), &[("ZeroFarm.exe", &launcher)], &key.key);
    layout.set_current("0.1.0").unwrap();
    layout.mark_healthy("0.1.0").unwrap();
    // Enrolled, pointed at the mock, paused (no leases needed: updates don't depend on idleness).
    let token = mock.add_machine("m_E2E00000000000000000000001");
    let mut m = Machine { machine_id: Some("m_E2E".into()), coordinator: Some(mock.url.clone()), ..Machine::default() };
    m.set_token(&token).unwrap();
    m.settings.pause = Pause::Indefinitely;
    m.save(&layout).unwrap();
    E { dir, root, layout, mock, key }
}

impl E {
    fn publish(&self, v: &str, exe: PathBuf, extra: &[(&str, &[u8])]) -> String {
        let pkg = self.dir.path().join(format!("pkg-{v}"));
        let launcher = std::fs::read(bin("ZeroFarm")).unwrap();
        let mut files: Vec<(&str, &[u8])> = vec![("ZeroFarm.exe", &launcher)];
        files.extend_from_slice(extra);
        let id = make_agent_package(&pkg, v, &exe, &files, &self.key.key);
        self.mock.add_build_dir(&pkg);
        self.mock.with(|s| s.latest = Some(json!({"version": v, "build_id": id, "manifest": format!("builds/{id}/manifest.json"), "min_agent": null})));
        id
    }
    fn launch(&self) -> KillOnDrop {
        KillOnDrop(
            Command::new(self.root.join("ZeroFarm.exe"))
                .env(TEST_KEYS_ENV, &self.key.pubkey)
                .env("ZEROFARM_TEST_HEADLESS", "1")
                .env("ZEROFARM_TEST_HEALTHY_TIMEOUT_S", "8")
                .spawn()
                .unwrap(),
        )
    }
    fn logs(&self) -> String {
        let r = |n: &str| std::fs::read_to_string(self.layout.logs().join(n)).unwrap_or_default();
        format!("--- launcher.log\n{}\n--- agent.log\n{}", r("launcher.log"), r("agent.log"))
    }
    fn agents_seen(&self) -> Vec<String> {
        let mut v: Vec<String> = self.mock.with(|s| s.log.iter().filter_map(|l| l.agent.clone()).collect());
        v.dedup();
        v
    }
}

#[test]
fn agent_updates_itself_through_the_launcher() {
    let e = env();
    let id2 = e.publish("0.2.0", bin("farm-agent"), &[]);
    let _l = e.launch();
    assert!(
        wait_until(T, || e.layout.current().as_deref() == Some("0.2.0") && e.layout.is_healthy("0.2.0")),
        "{}",
        e.logs()
    );
    assert_eq!(e.layout.previous().as_deref(), Some("0.1.0"));
    assert_eq!(e.layout.pending(), None);
    assert!(wait_until(T, || e.agents_seen().contains(&"0.2.0".to_string())), "{:?}", e.agents_seen());
    assert_eq!(e.agents_seen()[0], "0.1.0");
    // Downloaded exactly once, verified, nothing left in staging.
    assert_eq!(e.mock.count("GET", &format!("/v1/artifact/builds/{id2}/farm-agent.exe")), 1);
    assert!(e.logs().contains("switched 0.1.0 -> 0.2.0"));
}

#[test]
fn version_that_never_becomes_healthy_is_rolled_back_and_reported() {
    let e = env();
    // 0.3.0 is properly signed but never reaches the coordinator (a scripted fake agent).
    let id3 = e.publish("0.3.0", bin("fakeagent"), &[("behaviour", b"forever\n")]);
    let _l = e.launch();
    assert!(
        wait_until(T, || e.mock.with(|s| s.crashes.iter().any(|c| c["kind"] == "agent"))),
        "{}",
        e.logs()
    );
    let c = e.mock.with(|s| s.crashes.iter().find(|c| c["kind"] == "agent").cloned().unwrap());
    let msg = c["message"].as_str().unwrap();
    assert!(msg.contains("rollback") && msg.contains("0.3.0") && msg.contains("healthy-0.3.0"), "{msg}");
    assert_eq!(c["agent"], "0.1.0", "reported by the old agent");
    assert_eq!(e.layout.current().as_deref(), Some("0.1.0"));
    assert!(e.layout.bad_since("0.3.0").is_some());
    // The old agent keeps running and does not fetch 0.3.0 again.
    std::thread::sleep(Duration::from_secs(3));
    assert_eq!(e.mock.count("GET", &format!("/v1/artifact/builds/{id3}/farm-agent.exe")), 1, "{}", e.logs());
    assert!(wait_until(T, || !e.layout.version_dir("0.3.0").exists()), "old versions are cleaned up");
    assert!(std::fs::read_to_string(e.root.join("fakeagent.log")).unwrap().contains("0.3.0 start"));
}

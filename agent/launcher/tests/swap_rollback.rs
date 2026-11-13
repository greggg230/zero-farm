//! Launcher swap and rollback (PROTOCOL 6) in a temp root with fake agent versions.
//!
//! Each test copies ZeroFarm.exe into a fresh root, creates signed `versions\<v>\` directories
//! whose `farm-agent.exe` is the scripted `fakeagent`, and runs the real launcher.

use mockcoord::testkit::{bin, make_agent_version, test_key, wait_until, KillOnDrop, TestKey, TEST_KEYS_ENV};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;
use zerofarm_common::layout::Layout;

struct Env {
    _dir: tempfile::TempDir,
    root: PathBuf,
    layout: Layout,
    key: TestKey,
}

fn env() -> Env {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("ZeroFarm");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::copy(env!("CARGO_BIN_EXE_ZeroFarm"), root.join("ZeroFarm.exe")).unwrap();
    Env { layout: Layout::new(&root), root, _dir: dir, key: test_key() }
}

impl Env {
    fn version(&self, v: &str, behaviour: &str) {
        make_agent_version(&self.layout, v, &bin("fakeagent"), &[("behaviour", behaviour.as_bytes())], &self.key.key);
    }
    fn launch(&self) -> KillOnDrop {
        KillOnDrop(
            Command::new(self.root.join("ZeroFarm.exe"))
                .env(TEST_KEYS_ENV, &self.key.pubkey)
                .env("ZEROFARM_TEST_HEALTHY_TIMEOUT_S", "3")
                .spawn()
                .unwrap(),
        )
    }
    fn starts(&self) -> Vec<String> {
        std::fs::read_to_string(self.root.join("fakeagent.log"))
            .unwrap_or_default()
            .lines()
            .map(|l| l.split_whitespace().next().unwrap().to_string())
            .collect()
    }
    fn stop(&self) {
        std::fs::write(self.root.join("stop"), "1").unwrap();
    }
    fn launcher_log(&self) -> String {
        std::fs::read_to_string(self.layout.logs().join("launcher.log")).unwrap_or_default()
    }
    fn events(&self) -> Vec<serde_json::Value> {
        std::fs::read_to_string(self.layout.launcher_events_path())
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }
}

fn exits(l: &mut KillOnDrop, timeout: Duration) -> Option<i32> {
    let mut code = None;
    wait_until(timeout, || match l.0.try_wait().unwrap() {
        Some(s) => {
            code = s.code();
            true
        }
        None => false,
    });
    code
}

fn tamper(p: &Path) {
    let mut b = std::fs::read(p).unwrap();
    let n = b.len();
    b[n - 1] ^= 0xff;
    std::fs::write(p, b).unwrap();
}

#[test]
fn starts_current_and_stops_when_agent_quits() {
    let e = env();
    e.version("1.0.0", "healthy\nwait-stop\n");
    e.layout.set_current("1.0.0").unwrap();
    let mut l = e.launch();
    assert!(wait_until(Duration::from_secs(10), || e.starts() == ["1.0.0"]));
    e.stop();
    assert_eq!(exits(&mut l, Duration::from_secs(10)), Some(0), "launcher exits when the agent exits 0");
    assert!(e.launcher_log().contains("exited 0 (quit)"));
}

#[test]
fn swaps_to_verified_pending_on_exit_75() {
    let e = env();
    e.version("1.0.0", "healthy\nonce: pending 2.0.0\nonce: exit 75\nwait-stop\n");
    e.version("2.0.0", "healthy\nwait-stop\n");
    e.layout.set_current("1.0.0").unwrap();
    std::fs::write(e.layout.state().join("healthy-2.0.0"), "stale").unwrap();
    let mut l = e.launch();
    assert!(wait_until(Duration::from_secs(10), || e.starts() == ["1.0.0", "2.0.0"]), "{:?}", e.starts());
    assert!(wait_until(Duration::from_secs(5), || e.layout.is_healthy("2.0.0")));
    assert_eq!(e.layout.current().as_deref(), Some("2.0.0"));
    assert_eq!(e.layout.previous().as_deref(), Some("1.0.0"));
    assert_eq!(e.layout.pending(), None);
    assert!(e.launcher_log().contains("switched 1.0.0 -> 2.0.0"));
    assert!(e.launcher_log().contains("on probation"), "a stale healthy marker must not skip probation");
    e.stop();
    assert_eq!(exits(&mut l, Duration::from_secs(10)), Some(0));
}

#[test]
fn refuses_a_tampered_pending_version() {
    let e = env();
    e.version("1.0.0", "healthy\nonce: pending 2.0.0\nonce: exit 75\nwait-stop\n");
    e.version("2.0.0", "healthy\nwait-stop\n");
    tamper(&e.layout.version_dir("2.0.0").join("farm-agent.exe"));
    e.layout.set_current("1.0.0").unwrap();
    let mut l = e.launch();
    assert!(wait_until(Duration::from_secs(10), || e.starts().len() == 2), "{:?}", e.starts());
    assert_eq!(e.starts(), ["1.0.0", "1.0.0"], "the tampered version must never start");
    assert_eq!(e.layout.current().as_deref(), Some("1.0.0"));
    assert!(e.layout.bad_since("2.0.0").is_some());
    let evs = e.events();
    assert_eq!(evs.len(), 1, "{evs:?}");
    assert_eq!(evs[0]["kind"], "verify_failed");
    assert_eq!(evs[0]["from"], "2.0.0");
    assert!(evs[0]["reason"].as_str().unwrap().contains("sha256"));
    e.stop();
    assert_eq!(exits(&mut l, Duration::from_secs(10)), Some(0));
}

#[test]
fn rolls_back_when_new_version_never_writes_healthy() {
    let e = env();
    e.version("1.0.0", "healthy\nonce: pending 2.0.0\nonce: exit 75\nwait-stop\n");
    e.version("2.0.0", "forever\n");
    e.layout.set_current("1.0.0").unwrap();
    let mut l = e.launch();
    assert!(
        wait_until(Duration::from_secs(20), || e.starts() == ["1.0.0", "2.0.0", "1.0.0"]),
        "{:?}\n{}",
        e.starts(),
        e.launcher_log()
    );
    assert_eq!(e.layout.current().as_deref(), Some("1.0.0"));
    assert_eq!(e.layout.previous(), None);
    assert!(e.layout.bad_since("2.0.0").is_some());
    let evs = e.events();
    assert_eq!(evs.len(), 1);
    assert_eq!(evs[0]["kind"], "rollback");
    assert_eq!(evs[0]["from"], "2.0.0");
    assert_eq!(evs[0]["to"], "1.0.0");
    assert!(evs[0]["reason"].as_str().unwrap().contains("healthy-2.0.0"));
    e.stop();
    assert_eq!(exits(&mut l, Duration::from_secs(10)), Some(0));
}

#[test]
fn rolls_back_after_three_crashes_in_ten_minutes() {
    let e = env();
    e.version("1.0.0", "healthy\nonce: pending 2.0.0\nonce: exit 75\nwait-stop\n");
    e.version("2.0.0", "healthy\ncrash\n");
    e.layout.set_current("1.0.0").unwrap();
    let mut l = e.launch();
    assert!(
        wait_until(Duration::from_secs(20), || e.starts() == ["1.0.0", "2.0.0", "2.0.0", "2.0.0", "1.0.0"]),
        "{:?}\n{}",
        e.starts(),
        e.launcher_log()
    );
    assert_eq!(e.layout.current().as_deref(), Some("1.0.0"));
    let evs = e.events();
    assert_eq!(evs[0]["kind"], "rollback");
    assert!(evs[0]["reason"].as_str().unwrap().contains("3 times"), "{evs:?}");
    e.stop();
    assert_eq!(exits(&mut l, Duration::from_secs(10)), Some(0));
}

#[test]
fn keeps_restarting_a_crashing_agent_when_there_is_nothing_to_roll_back_to() {
    let e = env();
    e.version("1.0.0", "crash\n");
    e.layout.set_current("1.0.0").unwrap();
    let _l = e.launch();
    assert!(wait_until(Duration::from_secs(15), || e.starts().len() >= 3), "{:?}", e.starts());
    assert!(wait_until(Duration::from_secs(10), || e.launcher_log().contains("no previous version")), "{}", e.launcher_log());
    assert_eq!(e.layout.current().as_deref(), Some("1.0.0"));
    assert!(e.events().is_empty());
}

#[test]
fn second_launcher_exits_immediately() {
    let e = env();
    e.version("1.0.0", "healthy\nwait-stop\n");
    e.layout.set_current("1.0.0").unwrap();
    let mut l1 = e.launch();
    assert!(wait_until(Duration::from_secs(10), || e.starts().len() == 1));
    let mut l2 = e.launch();
    assert_eq!(exits(&mut l2, Duration::from_secs(10)), Some(0));
    assert_eq!(e.starts().len(), 1);
    e.stop();
    assert_eq!(exits(&mut l1, Duration::from_secs(10)), Some(0));
}

#[test]
fn refuses_to_run_an_unsigned_or_untrusted_current_version() {
    let e = env();
    e.version("1.0.0", "healthy\nwait-stop\n");
    e.layout.set_current("1.0.0").unwrap();
    // Run with a different trusted key: the signature is by an unknown key.
    let other = test_key();
    let mut l = KillOnDrop(Command::new(e.root.join("ZeroFarm.exe")).env(TEST_KEYS_ENV, &other.pubkey).spawn().unwrap());
    assert_eq!(exits(&mut l, Duration::from_secs(10)), Some(2));
    assert!(e.starts().is_empty());
    assert!(e.launcher_log().contains("unknown key"), "{}", e.launcher_log());
}

#[test]
fn deletes_leftover_launcher_files_at_start() {
    let e = env();
    e.version("1.0.0", "healthy
wait-stop
");
    e.layout.set_current("1.0.0").unwrap();
    let leftovers = ["ZeroFarm.old-1794300000.exe", "ZeroFarm.bad-7.exe", "ZeroFarm.new.exe"];
    for n in leftovers.iter().chain(&["ZeroFarmSetup.exe"]) {
        std::fs::write(e.root.join(n), "x").unwrap();
    }
    let mut l = e.launch();
    assert!(wait_until(Duration::from_secs(10), || e.starts().len() == 1));
    for n in leftovers {
        assert!(!e.root.join(n).exists(), "{n} should be gone");
    }
    assert!(e.root.join("ZeroFarmSetup.exe").exists(), "only leftovers are deleted");
    e.stop();
    assert_eq!(exits(&mut l, Duration::from_secs(10)), Some(0));
}

#[test]
fn selftest_checks_keys_and_the_current_version() {
    let e = env();
    e.version("1.0.0", "healthy
wait-stop
");
    e.layout.set_current("1.0.0").unwrap();
    let run = |pubkey: &str| Command::new(e.root.join("ZeroFarm.exe")).arg("selftest").env(TEST_KEYS_ENV, pubkey).output().unwrap();
    let ok = run(&e.key.pubkey);
    assert_eq!(ok.status.code(), Some(0));
    assert_eq!(String::from_utf8_lossy(&ok.stdout).trim(), format!("ok {}", env!("CARGO_PKG_VERSION")));
    let bad = run(&test_key().pubkey);
    assert_eq!(bad.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&bad.stdout).contains("unknown key"), "{}", String::from_utf8_lossy(&bad.stdout));
    assert!(e.starts().is_empty(), "selftest never starts the agent");
    assert!(!e.layout.logs().join("launcher.log").exists(), "nor touches the launcher log");
}

// ---- 0.2.11: the Windows session ending is not a crash ----

#[test]
fn session_end_exits_never_roll_back() {
    // STATUS_DLL_INIT_FAILED_LOGOFF three times within 10 minutes rolled a friend's 0.2.8 back to 0.2.5 (2026-11-13)
    let e = env();
    e.version("1.0.0", "healthy\nonce: pending 2.0.0\nonce: exit 75\nwait-stop\n");
    e.version("2.0.0", "healthy\nexit -1073741205\n");
    e.layout.set_current("1.0.0").unwrap();
    let _l = e.launch();
    assert!(
        wait_until(Duration::from_secs(30), || e.starts().iter().filter(|v| *v == "2.0.0").count() >= 4),
        "{:?}\n{}",
        e.starts(),
        e.launcher_log()
    );
    assert_eq!(e.layout.current().as_deref(), Some("2.0.0"), "no rollback");
    assert!(e.events().is_empty(), "{:?}", e.events());
    assert!(e.launcher_log().contains("as the Windows session ends: not a crash"), "{}", e.launcher_log());
    assert!(!e.starts().iter().skip(2).any(|v| v == "1.0.0"), "{:?}", e.starts());
}

#[test]
fn no_new_start_while_the_session_is_ending() {
    let e = env();
    e.version("1.0.0", "healthy\ncrash\n");
    e.layout.set_current("1.0.0").unwrap();
    let _l = KillOnDrop(
        Command::new(e.root.join("ZeroFarm.exe"))
            .env(TEST_KEYS_ENV, &e.key.pubkey)
            .env("ZEROFARM_TEST_SESSION_ENDING", "1")
            .spawn()
            .unwrap(),
    );
    assert!(wait_until(Duration::from_secs(10), || e.launcher_log().contains("the session is ending")), "{}", e.launcher_log());
    std::thread::sleep(Duration::from_secs(4));
    assert_eq!(e.starts(), ["1.0.0"], "{}", e.launcher_log());
    assert!(e.events().is_empty());
}

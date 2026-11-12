//! The launcher updating itself (PROTOCOL 6): the current, healthy agent finds a newer
//! `ZeroFarm.exe` in its package, selftests it, renames the running launcher to
//! `ZeroFarm.old-<n>.exe`, puts the new one in place and exits 76; the old launcher starts the new
//! one and exits once it confirms, or puts itself back.
//!
//! In process: the engine's side (plan, selftest, swap, exit code). With real processes: the real
//! launcher handing over to a newer real launcher, and to a fake launcher that crashes or hangs
//! (rolled back). The installed launcher is a copy of the real one with its VERSIONINFO patched
//! to 0.0.1, so the package's launcher is "newer".

mod harness;

use farm_agent::engine::EngineExit;
use farm_agent::launcher_update::{plan, STAGED_EXE};
use farm_agent::settings::{Machine, Pause};
use harness::{wait, H};
use mockcoord::testkit::{bin, launcher_copy, make_agent_version, sha256_of, test_key, wait_until, KillOnDrop, TestKey, TEST_KEYS_ENV};
use mockcoord::MockCoord;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;
use zerofarm_common::keys::TrustedKeys;
use zerofarm_common::layout::Layout;
use zerofarm_common::names::{instance_running, QuitEvent};

const T: Duration = Duration::from_secs(60);

/// The launcher crate's version: what the built ZeroFarm.exe carries in its VERSIONINFO. Read from
/// its Cargo.toml so a version bump for a release doesn't break these tests.
fn launcher_version() -> (u16, u16, u16) {
    let v = include_str!("../../launcher/Cargo.toml")
        .lines()
        .find_map(|l| l.trim().strip_prefix("version = \"").and_then(|r| r.strip_suffix('"')))
        .expect("version line in launcher/Cargo.toml");
    let p: Vec<u16> = v.split('.').map(|x| x.parse().expect("numeric launcher version")).collect();
    (p[0], p[1], p[2])
}

fn launcher_version_str() -> String {
    let (a, b, c) = launcher_version();
    format!("{a}.{b}.{c}")
}

fn files_named(root: &Path, prefix: &str) -> Vec<String> {
    std::fs::read_dir(root)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| n.starts_with(prefix))
        .collect()
}

fn fake_log(root: &Path) -> Vec<String> {
    std::fs::read_to_string(root.join("fakelauncher.log")).unwrap_or_default().lines().map(String::from).collect()
}

/// Installed launcher = the real one patched to 0.0.1; `versions\0.1.0` = an agent package (its
/// farm-agent.exe is `agent_exe`) carrying `package_launcher` as ZeroFarm.exe; current + healthy.
fn install(layout: &Layout, key: &TestKey, agent_exe: &Path, package_launcher: &Path) -> String {
    std::fs::create_dir_all(&layout.root).unwrap();
    let old_sha = launcher_copy(&bin("ZeroFarm"), &layout.launcher_exe(), (0, 0, 1));
    let launcher = std::fs::read(package_launcher).unwrap();
    make_agent_version(layout, "0.1.0", agent_exe, &[("ZeroFarm.exe", &launcher)], &key.key);
    layout.set_current("0.1.0").unwrap();
    layout.mark_healthy("0.1.0").unwrap();
    old_sha
}

fn behaviour(root: &Path, text: &str) {
    std::fs::write(root.join("fakelauncher.behaviour"), text).unwrap();
}

// ---------------------------------------------------------------------------------------------
// In process: the engine's side.

#[test]
fn engine_swaps_in_a_newer_launcher_and_asks_for_a_handover() {
    let mut h = H::new();
    install(&h.layout, &h.key, &bin("fakeagent"), &bin("fakelauncher"));
    behaviour(&h.layout.root, "selftest ok\n");
    h.start();
    assert_eq!(h.join(), EngineExit::RestartLauncher);
    let root = h.layout.root.clone();
    assert_eq!(sha256_of(&h.layout.launcher_exe()), sha256_of(&bin("fakelauncher")), "the new launcher is ZeroFarm.exe");
    let old = files_named(&root, "ZeroFarm.old-");
    assert_eq!(old.len(), 1, "{old:?}");
    assert_eq!(zerofarm_common::winres::fixed_file_version(&std::fs::read(root.join(&old[0])).unwrap()), Some((0, 0, 1, 0)));
    assert!(!root.join(STAGED_EXE).exists());
    let swap = h.layout.launcher_swap().expect("state\\launcher-swap");
    assert_eq!((swap.old.as_str(), swap.from.as_str(), swap.to.as_str()), (old[0].as_str(), "0.0.1", "0.1.0"));
    assert_eq!(swap.sha256, sha256_of(&bin("fakelauncher")));
    assert_eq!(fake_log(&root), ["selftest"], "selftested once, from the staging name");
}

#[test]
fn with_a_launcher_that_cannot_hand_over_the_new_one_waits_for_the_next_logon() {
    let mut h = H::new();
    h.launcher_level = 0; // a launcher before 0.2.0 set no ZEROFARM_LAUNCHER
    install(&h.layout, &h.key, &bin("fakeagent"), &bin("fakelauncher"));
    behaviour(&h.layout.root, "selftest ok\n");
    h.start();
    assert!(wait(|| files_named(&h.layout.root, "ZeroFarm.old-").len() == 1));
    std::thread::sleep(Duration::from_millis(500));
    assert!(!h.finished(), "keeps running on the old launcher");
    assert_eq!(sha256_of(&h.layout.launcher_exe()), sha256_of(&bin("fakelauncher")));
    assert!(h.layout.launcher_swap().is_none());
    assert_eq!(h.stop(), EngineExit::Quit);
}

#[test]
fn a_launcher_failing_its_selftest_is_never_installed_or_retried() {
    let mut h = H::new();
    let old_sha = install(&h.layout, &h.key, &bin("fakeagent"), &bin("fakelauncher"));
    behaviour(&h.layout.root, "selftest fail\n");
    h.start();
    assert!(wait(|| h.crashes().iter().any(|c| c["kind"] == "agent")), "reported");
    let c = h.crashes().into_iter().find(|c| c["kind"] == "agent").unwrap();
    assert!(c["message"].as_str().unwrap().contains("failed its selftest"), "{c}");
    assert_eq!(sha256_of(&h.layout.launcher_exe()), old_sha, "the installed launcher is untouched");
    assert!(files_named(&h.layout.root, "ZeroFarm.old-").is_empty());
    assert!(!h.layout.root.join(STAGED_EXE).exists());
    assert!(h.layout.launcher_bad_since(&sha256_of(&bin("fakelauncher"))).is_some());
    std::thread::sleep(Duration::from_millis(300));
    assert!(!h.finished());
    assert_eq!(h.stop(), EngineExit::Quit);
    // A new agent run doesn't try it again (bad for 24 h).
    h.start();
    std::thread::sleep(Duration::from_millis(800));
    assert_eq!(h.stop(), EngineExit::Quit);
    assert_eq!(fake_log(&h.layout.root), ["selftest"]);
}

#[test]
fn plan_only_for_a_newer_launcher_from_the_current_healthy_version() {
    let d = tempfile::tempdir().unwrap();
    let layout = Layout::new(d.path().join("ZeroFarm"));
    let key = test_key();
    let keys = TrustedKeys::parse(&[key.pubkey.clone()]).unwrap();
    let new = d.path().join("ZeroFarm-new.exe");
    std::fs::copy(bin("ZeroFarm"), &new).unwrap();
    install(&layout, &key, &bin("fakeagent"), &new);
    let p = plan(&layout, "0.1.0", &keys).expect("the built launcher replaces 0.0.1");
    let (a, b, c) = launcher_version();
    assert_eq!((p.from, p.to), ((0, 0, 1, 0), (a, b, c, 0)));
    assert!(plan(&layout, "0.0.9", &keys).is_none(), "not the current version");
    assert!(plan(&layout, "0.1.0", &TrustedKeys::parse(&[test_key().pubkey]).unwrap()).is_none(), "package must verify");
    std::fs::remove_file(layout.healthy_marker("0.1.0")).unwrap();
    assert!(plan(&layout, "0.1.0", &keys).is_none(), "not healthy yet");
    layout.mark_healthy("0.1.0").unwrap();
    // Installed is newer or equal: never replaced by an older one (a rolled-back agent version).
    launcher_copy(&bin("ZeroFarm"), &layout.launcher_exe(), (a, b + 1, 0));
    assert!(plan(&layout, "0.1.0", &keys).is_none());
    launcher_copy(&bin("ZeroFarm"), &layout.launcher_exe(), (a, b, c));
    assert!(plan(&layout, "0.1.0", &keys).is_none(), "same version, other bytes");
    // A launcher without VERSIONINFO (before 0.2.0) counts as 0.0.0.
    std::fs::copy(bin("fakeagent"), layout.launcher_exe()).unwrap();
    assert!(plan(&layout, "0.1.0", &keys).is_some());
    // Marked bad recently: not again for 24 h.
    layout.mark_launcher_bad(&p.sha256).unwrap();
    assert!(plan(&layout, "0.1.0", &keys).is_none());
}

// ---------------------------------------------------------------------------------------------
// Real processes: the launcher hands over (or takes itself back).

struct E {
    _dir: tempfile::TempDir,
    root: PathBuf,
    layout: Layout,
    mock: MockCoord,
    key: TestKey,
    old_sha: String,
}

/// Stops whatever runs for the root when the test ends (the new launcher is not our child).
impl Drop for E {
    fn drop(&mut self) {
        if let Some(q) = QuitEvent::open(&self.root) {
            q.set();
        }
        let gone = wait_until(Duration::from_secs(20), || !instance_running("launcher", &self.root) && !instance_running("agent", &self.root));
        if !gone {
            for pid in self.launcher_pids() {
                let _ = Command::new("taskkill").args(["/F", "/T", "/PID", &pid.to_string()]).output();
            }
        }
    }
}

impl E {
    fn new(package_launcher: &Path) -> E {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("ZeroFarm");
        let layout = Layout::new(&root);
        let key = test_key();
        let old_sha = install(&layout, &key, &bin("farm-agent"), package_launcher);
        let mock = MockCoord::start();
        mock.trust(&key.pubkey);
        let token = mock.add_machine("m_LAUNCHER0000000000000000001");
        let mut m = Machine { machine_id: Some("m_LAUNCHER".into()), coordinator: Some(mock.url.clone()), ..Machine::default() };
        m.set_token(&token).unwrap();
        m.settings.pause = Pause::Indefinitely; // no leases: this is about the launcher
        m.save(&layout).unwrap();
        E { _dir: dir, root, layout, mock, key, old_sha }
    }

    fn launch(&self, handover_s: u64) -> KillOnDrop {
        KillOnDrop(
            Command::new(self.root.join("ZeroFarm.exe"))
                .env(TEST_KEYS_ENV, &self.key.pubkey)
                .env("ZEROFARM_TEST_HEADLESS", "1")
                .env("ZEROFARM_TEST_TAKEOVER_CONFIRM_S", "1")
                .env("ZEROFARM_TEST_HANDOVER_S", handover_s.to_string())
                .spawn()
                .unwrap(),
        )
    }

    fn log(&self) -> String {
        let r = |n: &str| std::fs::read_to_string(self.layout.logs().join(n)).unwrap_or_default();
        format!("--- launcher.log\n{}\n--- agent.log\n{}", r("launcher.log"), r("agent.log"))
    }

    fn launcher_pids(&self) -> Vec<u32> {
        let text = std::fs::read_to_string(self.layout.logs().join("launcher.log")).unwrap_or_default();
        let mut pids: Vec<u32> = text
            .lines()
            .filter_map(|l| l.split("[launcher ").nth(1)?.split(']').next()?.parse().ok())
            .collect();
        pids.sort();
        pids.dedup();
        pids
    }

    /// Agent starts seen by the mock (each start checks for updates first).
    fn agent_starts(&self) -> usize {
        self.mock.count("GET", "/v1/agent/latest")
    }
}

#[test]
fn launcher_replaces_itself_and_hands_over() {
    let e = E::new(&bin("ZeroFarm"));
    let mut old = e.launch(60);
    let code = {
        let mut code = None;
        wait_until(T, || match old.0.try_wait().unwrap() {
            Some(s) => {
                code = s.code();
                true
            }
            None => false,
        });
        code
    };
    assert_eq!(code, Some(0), "the old launcher exits once the new one took over\n{}", e.log());
    assert_eq!(sha256_of(&e.layout.launcher_exe()), sha256_of(&bin("ZeroFarm")));
    let log = e.log();
    let v = launcher_version_str();
    assert!(log.contains(&format!("handing over to launcher {v} (this one is 0.0.1")), "{log}");
    assert!(log.contains("took over from launcher pid"), "{log}");
    assert!(log.contains(&format!("launcher {v} (pid")), "{log}");
    // The new launcher runs the agent again (a second agent start), and cleans up.
    assert!(instance_running("launcher", &e.root), "{log}");
    assert!(wait_until(T, || e.agent_starts() >= 2), "{}", e.log());
    assert!(wait_until(T, || files_named(&e.root, "ZeroFarm.old-").is_empty()), "the old launcher's file is deleted\n{}", e.log());
    assert!(e.layout.launcher_swap().is_none());
    assert!(files_named(&e.layout.state(), "launcher-ok-").is_empty());
    // The new agent run sees the same launcher in its package: no second swap.
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(e.log().matches("handing over").count(), 1);
}

fn rolled_back(behaviour_text: &str, handover_s: u64, why: &str) {
    let e = E::new(&bin("fakelauncher"));
    behaviour(&e.root, behaviour_text);
    let mut old = e.launch(handover_s);
    assert!(
        wait_until(T, || e.mock.with(|s| s.crashes.iter().any(|c| c["kind"] == "agent"))),
        "the rollback is reported by the restarted agent\n{}",
        e.log()
    );
    let c = e.mock.with(|s| s.crashes.iter().find(|c| c["kind"] == "agent").cloned().unwrap());
    let msg = c["message"].as_str().unwrap();
    assert!(msg.contains("launcher_rollback") && msg.contains(why), "{msg}");
    assert_eq!(sha256_of(&e.layout.launcher_exe()), e.old_sha, "the old launcher is ZeroFarm.exe again");
    assert!(old.0.try_wait().unwrap().is_none(), "and keeps running\n{}", e.log());
    assert!(e.layout.launcher_bad_since(&sha256_of(&bin("fakelauncher"))).is_some());
    assert!(e.layout.launcher_swap().is_none());
    assert!(wait_until(T, || files_named(&e.root, "ZeroFarm.bad-").is_empty()), "the bad launcher's file is deleted\n{}", e.log());
    // It isn't tried again.
    std::thread::sleep(Duration::from_secs(2));
    let calls = fake_log(&e.root);
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert_eq!(calls[0], "selftest");
    assert!(calls[1].starts_with("--takeover "), "{calls:?}");
    assert!(e.agent_starts() >= 2, "the agent was restarted\n{}", e.log());
}

#[test]
fn a_new_launcher_that_exits_at_once_is_rolled_back() {
    rolled_back("selftest ok\ntakeover crash\n", 60, "exited with code 3 before taking over");
}

#[test]
fn a_new_launcher_that_never_takes_over_is_killed_and_rolled_back() {
    rolled_back("selftest ok\ntakeover hang\n", 3, "did not take over within 3 s");
}

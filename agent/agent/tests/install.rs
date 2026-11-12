//! install / uninstall in a temp root with a throwaway registry name (`ZeroFarmTest-<pid>-<n>`)
//! and a temp shortcut folder. The real `ZeroFarm` entries are never touched; a guard removes
//! the test entries even when an assertion fails.

use farm_agent::install::{remove_entries, reg_get_sz, run_value, shortcut_path, uninstall_key};
use farm_agent::settings::{Machine, Pause};
use mockcoord::testkit::{bin, make_agent_package, test_key, test_reg_name, wait_until, KillOnDrop, TestKey, TEST_KEYS_ENV};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;
use zerofarm_common::layout::Layout;

struct RegGuard(String);
impl Drop for RegGuard {
    fn drop(&mut self) {
        remove_entries(&self.0);
    }
}

struct S {
    dir: tempfile::TempDir,
    stage: PathBuf,
    root: PathBuf,
    shortcuts: PathBuf,
    reg: RegGuard,
    key: TestKey,
}

fn setup() -> S {
    let dir = tempfile::tempdir().unwrap();
    let stage = dir.path().join("stage");
    let key = test_key();
    let launcher = std::fs::read(bin("ZeroFarm")).unwrap();
    make_agent_package(&stage, "0.1.0", &bin("farm-agent"), &[("ZeroFarm.exe", &launcher)], &key.key);
    let reg = RegGuard(test_reg_name());
    assert_ne!(reg.0, "ZeroFarm");
    S { root: dir.path().join("Local").join("ZeroFarm"), shortcuts: dir.path().join("StartMenu"), stage, dir, reg, key }
}

impl S {
    /// Runs a command with a hard 60 s limit (a hang fails the test instead of the suite).
    fn agent(&self, exe: &Path, args: &[&str]) -> std::process::Output {
        let out_path = self.dir.path().join(format!("out-{}.txt", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        let f = std::fs::File::create(&out_path).unwrap();
        let mut child = KillOnDrop(
            Command::new(exe)
                .args(args)
                .env(TEST_KEYS_ENV, &self.key.pubkey)
                .env("ZEROFARM_TEST_HEADLESS", "1")
                .stdin(std::process::Stdio::null())
                .stdout(f.try_clone().unwrap())
                .stderr(f)
                .spawn()
                .unwrap(),
        );
        let mut status = None;
        let finished = wait_until(Duration::from_secs(60), || {
            status = child.0.try_wait().unwrap();
            status.is_some()
        });
        assert!(finished, "{} {args:?} did not finish within 60 s", exe.display());
        std::process::Output { status: status.unwrap(), stdout: std::fs::read(&out_path).unwrap_or_default(), stderr: vec![] }
    }
    fn install(&self, extra: &[&str]) -> std::process::Output {
        let root = self.root.display().to_string();
        let sc = self.shortcuts.display().to_string();
        let mut args = vec!["install", "--root", &root, "--reg-name", &self.reg.0, "--shortcut-dir", &sc, "--quiet"];
        args.extend_from_slice(extra);
        self.agent(&self.stage.join("farm-agent.exe"), &args)
    }
    fn uninstall_via_launcher(&self) -> std::process::Output {
        let sc = self.shortcuts.display().to_string();
        self.agent(&self.root.join("ZeroFarm.exe"), &["uninstall", "--reg-name", &self.reg.0, "--shortcut-dir", &sc, "--quiet"])
    }
}

#[test]
fn install_then_uninstall_with_a_test_registry_name() {
    let s = setup();
    let out = s.install(&["--no-start"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stdout));
    let layout = Layout::new(&s.root);
    assert!(s.root.join("ZeroFarm.exe").exists());
    assert!(layout.version_dir("0.1.0").join("farm-agent.exe").exists());
    assert_eq!(layout.current().as_deref(), Some("0.1.0"));
    assert!(layout.is_healthy("0.1.0"));
    // HKCU Run value points at the launcher.
    let run = run_value(&s.reg.0).expect("Run value");
    assert_eq!(run, format!("\"{}\"", s.root.join("ZeroFarm.exe").display()));
    // HKCU Uninstall entry.
    let uk = uninstall_key(&s.reg.0);
    assert_eq!(reg_get_sz(&uk, "DisplayName").as_deref(), Some(format!("Zero Farm ({})", s.reg.0).as_str()));
    assert_eq!(reg_get_sz(&uk, "DisplayVersion").as_deref(), Some("0.1.0"));
    let un = reg_get_sz(&uk, "UninstallString").unwrap();
    assert!(un.contains("ZeroFarm.exe\" uninstall") && un.contains(&s.reg.0), "{un}");
    // Shortcut.
    let lnk = shortcut_path(&s.reg.0, Some(&s.shortcuts));
    assert!(lnk.exists(), "{}", lnk.display());
    // Nothing went to the real Run value name.
    assert_ne!(run_value("ZeroFarm").as_deref(), Some(run.as_str()));

    // Uninstall the way Apps & Features does: the launcher forwards to the agent, which hands
    // off to a copy in %TEMP% so its own folder can be deleted.
    let out = s.uninstall_via_launcher();
    assert!(out.status.success());
    assert!(wait_until(Duration::from_secs(40), || !s.root.exists()), "root still exists");
    assert!(run_value(&s.reg.0).is_none());
    assert!(reg_get_sz(&uk, "DisplayName").is_none());
    assert!(!lnk.exists());
    // The stage-2 copy in %TEMP% deletes itself shortly after.
    let leftover = || {
        std::fs::read_dir(std::env::temp_dir())
            .unwrap()
            .flatten()
            .any(|e| e.file_name().to_string_lossy().starts_with("ZeroFarm-uninstall-") && e.metadata().map(|m| m.modified().unwrap().elapsed().unwrap().as_secs() < 60).unwrap_or(false))
    };
    assert!(wait_until(Duration::from_secs(20), || !leftover()), "stage-2 uninstaller left in %TEMP%");
    let _ = &s.dir;
}

#[test]
fn install_starts_the_launcher_and_uninstall_stops_it() {
    let s = setup();
    // Pre-enrolled (paused, pointed nowhere reachable) so the headless agent stays up.
    let layout = Layout::new(&s.root);
    let mut m = Machine { machine_id: Some("m_I".into()), coordinator: Some("http://127.0.0.1:9".into()), ..Machine::default() };
    m.set_token("tok").unwrap();
    m.settings.pause = Pause::Indefinitely;
    m.save(&layout).unwrap();
    let out = s.install(&[]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stdout));
    assert!(
        wait_until(Duration::from_secs(20), || zerofarm_common::names::instance_running("agent", &s.root)),
        "launcher started the agent"
    );
    assert!(zerofarm_common::names::instance_running("launcher", &s.root));
    let out = s.uninstall_via_launcher();
    assert!(out.status.success());
    assert!(wait_until(Duration::from_secs(40), || !s.root.exists()));
    assert!(!zerofarm_common::names::instance_running("agent", &s.root));
    assert!(!zerofarm_common::names::instance_running("launcher", &s.root));
    assert!(run_value(&s.reg.0).is_none());
}

#[test]
fn install_refuses_a_tampered_package() {
    let s = setup();
    let p = s.stage.join("ZeroFarm.exe");
    let mut b = std::fs::read(&p).unwrap();
    b[500] ^= 1;
    std::fs::write(&p, b).unwrap();
    let out = s.install(&["--no-start"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("failed verification"));
    assert!(run_value(&s.reg.0).is_none());
    assert!(!s.root.join("ZeroFarm.exe").exists());
}

#[test]
fn reinstalling_a_newer_version_keeps_the_old_one_as_previous() {
    let s = setup();
    assert!(s.install(&["--no-start"]).status.success());
    let stage2 = s.dir.path().join("stage2");
    let launcher = std::fs::read(bin("ZeroFarm")).unwrap();
    make_agent_package(&stage2, "0.2.0", &bin("farm-agent"), &[("ZeroFarm.exe", &launcher)], &s.key.key);
    let root = s.root.display().to_string();
    let sc = s.shortcuts.display().to_string();
    let out = s.agent(&stage2.join("farm-agent.exe"), &["install", "--root", &root, "--reg-name", &s.reg.0, "--shortcut-dir", &sc, "--quiet", "--no-start"]);
    assert!(out.status.success());
    let layout = Layout::new(&s.root);
    assert_eq!(layout.current().as_deref(), Some("0.2.0"));
    assert_eq!(layout.previous().as_deref(), Some("0.1.0"));
    // An older installer doesn't downgrade.
    let out = s.install(&["--no-start"]);
    assert!(out.status.success());
    assert_eq!(layout.current().as_deref(), Some("0.2.0"));
}

#[test]
fn forward_slash_root_is_normalized_and_uninstall_removes_the_run_value() {
    let mut s = setup();
    s.root = PathBuf::from(s.root.display().to_string().replace('\\', "/"));
    assert!(s.install(&["--no-start"]).status.success());
    let run = run_value(&s.reg.0).unwrap();
    assert!(!run.contains('/'), "registry paths use backslashes: {run}");
    assert!(s.uninstall_via_launcher().status.success());
    assert!(wait_until(Duration::from_secs(40), || !s.root.exists()));
    assert!(run_value(&s.reg.0).is_none(), "uninstall must remove its own Run value");
}

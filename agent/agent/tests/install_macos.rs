//! macOS install end to end (PROTOCOL 11.2, 11.5) with the real launcher and agent: `ZeroFarm setup --invite` from an
//! unpacked package (temp root, temp LaunchAgents folder and label, no `launchctl`: the launcher is started
//! directly), then the headless CLI against the running agent - `status`, `set`, `pause` / `resume` through the
//! agent's request files, `quit` - and `uninstall`. The real LaunchAgent and `~/Library/Application Support/ZeroFarm`
//! are never touched.
#![cfg(target_os = "macos")]

use mockcoord::testkit::{bin, make_agent_package, test_key, wait_until, TestKey, TEST_KEYS_ENV};
use mockcoord::MockCoord;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Duration;
use zerofarm_common::layout::{Layout, LAUNCHER_EXE};
use zerofarm_common::names::instance_running;

const T: Duration = Duration::from_secs(30);

struct Setup {
    _dir: tempfile::TempDir,
    stage: PathBuf,
    root: PathBuf,
    agents: PathBuf,
    label: String,
    key: TestKey,
    mock: MockCoord,
}

impl Setup {
    fn new() -> Setup {
        let dir = tempfile::tempdir().unwrap();
        let key = test_key();
        let stage = dir.path().join("download");
        let launcher = std::fs::read(bin("ZeroFarm")).unwrap();
        make_agent_package(&stage, "0.1.0", &bin("farm-agent"), &[(LAUNCHER_EXE, &launcher)], &key.key);
        zerofarm_common::fsutil::set_mode(&stage.join(LAUNCHER_EXE), 0o755);
        let mock = MockCoord::start();
        mock.trust(&key.pubkey);
        Setup {
            root: dir.path().join("Application Support").join("ZeroFarm"),
            agents: dir.path().join("LaunchAgents"),
            label: format!("com.zerofarm.test-{}", std::process::id()),
            _dir: dir,
            stage,
            key,
            mock,
        }
    }

    fn run(&self, exe: &Path, args: &[&str]) -> Output {
        Command::new(exe).args(args).env(TEST_KEYS_ENV, &self.key.pubkey).output().unwrap()
    }

    /// `<root>/ZeroFarm <args>` (the installed launcher's CLI).
    fn cli(&self, args: &[&str]) -> (bool, String) {
        let o = self.run(&self.root.join(LAUNCHER_EXE), args);
        (o.status.success(), format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr)))
    }

    fn running(&self) -> bool {
        instance_running("launcher", &self.root) || instance_running("agent", &self.root)
    }
}

impl Drop for Setup {
    fn drop(&mut self) {
        // never leave a detached launcher behind, whatever failed
        if let Some(q) = zerofarm_common::names::QuitEvent::open(&self.root) {
            q.set();
        }
        wait_until(T, || !self.running());
    }
}

#[test]
fn setup_runs_headless_and_the_cli_reaches_the_running_agent() {
    let s = Setup::new();
    s.mock.add_invite("ABCD-EFGH-JKM2");
    let (root, agents) = (s.root.display().to_string(), s.agents.display().to_string());
    let o = s.run(
        &s.stage.join(LAUNCHER_EXE),
        &["setup", "--invite", "abcd efgh jkm2", "--name", "Test Mac", "--root", &root, "--launch-agents-dir", &agents, "--label", &s.label, "--no-launchctl", "--coordinator", &s.mock.url],
    );
    let text = format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
    assert!(o.status.success(), "{text}");
    assert!(text.contains("Joined the farm as Test Mac") && text.contains("installed and running"), "{text}");
    // the layout, the token file and the LaunchAgent
    let layout = Layout::new(&s.root);
    assert_eq!(layout.current().as_deref(), Some("0.1.0"));
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&layout.machine_json()), 0o600);
        assert_eq!(mode(&layout.state()), 0o700);
        assert_eq!(mode(&layout.launcher_exe()), 0o755);
        assert_eq!(mode(&layout.version_dir("0.1.0").join("farm-agent")), 0o755);
    }
    let plist = std::fs::read_to_string(s.agents.join(format!("{}.plist", s.label))).unwrap();
    assert!(plist.contains(&format!("<string>{}</string>", layout.launcher_exe().display())) && plist.contains("<key>RunAtLoad</key>"), "{plist}");
    assert!(s.mock.with(|m| m.enrolls.len()) == 1);
    // the launcher started the agent, which works for the mock (asks for leases with its platform)
    assert!(wait_until(T, || instance_running("agent", &s.root) && instance_running("launcher", &s.root)), "{}", s.cli(&["status"]).1);
    assert!(s.mock.wait(T, |m| m.log.iter().any(|x| x.path == "/v1/lease")));
    // status: what the running agent wrote
    assert!(wait_until(T, || s.cli(&["status"]).1.contains("Zero Farm 0.1.0:")), "{}", s.cli(&["status"]).1);
    let (ok, st) = s.cli(&["status"]);
    assert!(ok && st.contains("this Mac: Test Mac") && st.contains("CPU threads:") && st.contains("(automatic: one per physical core"), "{st}");
    // set, through the running agent's request files
    let (ok, out) = s.cli(&["set", "cpu_threads", "3"]);
    assert!(ok && out.trim() == "cpu_threads 3", "{out}");
    assert_eq!(zerofarm_farm_threads(&layout), Some(3));
    assert!(wait_until(T, || s.cli(&["status"]).1.contains("CPU threads: 3 (set; automatic would be")), "{}", s.cli(&["status"]).1);
    let (ok, out) = s.cli(&["set", "work_when", "sometimes"]);
    assert!(!ok && out.contains("work_when: idle"), "{out}");
    let (ok, out) = s.cli(&["pause", "30"]);
    assert!(ok && out.contains("paused for 30 more minutes"), "{out}");
    assert!(s.cli(&["status"]).1.contains("paused for"), "{}", s.cli(&["status"]).1);
    let (ok, out) = s.cli(&["resume"]);
    assert!(ok && out.contains("resumed"), "{out}");
    // quit: both gone; the CLI then changes machine.json itself
    let (ok, out) = s.cli(&["quit"]);
    assert!(ok && out.contains("quit"), "{out}");
    assert!(!s.running());
    let (ok, st) = s.cli(&["status"]);
    assert!(ok && st.contains("not running"), "{st}");
    let (ok, out) = s.cli(&["set", "keep_awake", "false"]);
    assert!(ok && out.trim() == "keep_awake false", "{out}");
    assert!(!farm_agent::settings::Machine::load(&layout).settings.keep_awake);
    // uninstall: the root and our LaunchAgent go
    let (ok, out) = s.cli(&["uninstall", "--launch-agents-dir", &agents, "--label", &s.label, "--no-launchctl"]);
    assert!(ok, "{out}");
    assert!(!s.root.exists() && !s.agents.join(format!("{}.plist", s.label)).exists());
}

fn zerofarm_farm_threads(layout: &Layout) -> Option<u32> {
    farm_agent::settings::Machine::load(layout).settings.cpu_threads
}

#[test]
fn setup_refuses_a_tampered_package_and_a_windows_one() {
    let s = Setup::new();
    let agents = s.agents.display().to_string();
    let root = s.root.display().to_string();
    let setup = |st: &Setup| st.run(&st.stage.join(LAUNCHER_EXE), &["setup", "--invite", "ABCD-EFGH-JKM2", "--root", &root, "--launch-agents-dir", &agents, "--label", &st.label, "--no-launchctl"]);
    let a = s.stage.join("farm-agent");
    let mut b = std::fs::read(&a).unwrap();
    let n = b.len();
    b[n - 64] ^= 1;
    std::fs::write(&a, &b).unwrap();
    let o = setup(&s);
    assert!(!o.status.success() && String::from_utf8_lossy(&o.stderr).contains("failed verification"), "{o:?}");
    assert!(!s.root.exists(), "nothing installed");
    // a package without `platform` is windows-x64's: the agent's install refuses it
    let w = Setup::new();
    let dir = w.stage.clone();
    let mut t = zerofarm_common::sign::template(zerofarm_common::manifest::Kind::Agent, "0.1.0");
    t.entry = Some("farm-agent".into());
    zerofarm_common::sign::sign_dir(&dir, t, &w.key.key).unwrap();
    let root2 = w.root.display().to_string();
    let o = w.run(&w.stage.join(LAUNCHER_EXE), &["setup", "--invite", "ABCD-EFGH-JKM2", "--root", &root2, "--launch-agents-dir", &agents, "--label", &w.label, "--no-launchctl"]);
    assert!(!o.status.success() && String::from_utf8_lossy(&o.stderr).contains("is for windows-x64"), "{o:?}");
}

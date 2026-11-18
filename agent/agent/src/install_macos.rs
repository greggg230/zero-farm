//! Per-user install and uninstall on macOS (PROTOCOL 11.5): everything under one root (default
//! `~/Library/Application Support/ZeroFarm`) and a LaunchAgent, `~/Library/LaunchAgents/com.zerofarm.agent.plist`,
//! that runs `<root>/ZeroFarm` at login (`RunAtLoad`, no `KeepAlive`: the launcher restarts the agent itself). No
//! admin rights. `launchctl bootstrap gui/<uid>` loads it at install (which starts the launcher) and
//! `launchctl bootout` unloads it at uninstall.
//!
//! The LaunchAgents folder, the label and whether `launchctl` runs are parameters, so tests use a temporary folder and
//! label and never touch the real LaunchAgent (without `launchctl` the launcher is started directly, detached).

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use zerofarm_common::keys::TrustedKeys;
use zerofarm_common::layout::{Layout, AGENT_EXE, LAUNCHER_EXE, LAUNCH_AGENT_LABEL};
use zerofarm_common::manifest::{verify_dir, Expect, Kind};
use zerofarm_common::names::{instance_running, QuitEvent};

pub const DEFAULT_LABEL: &str = LAUNCH_AGENT_LABEL;

#[derive(Debug, Clone)]
pub struct InstallOpts {
    pub root: PathBuf,
    /// Where the plist goes; `None` = `~/Library/LaunchAgents`.
    pub launch_agents_dir: Option<PathBuf>,
    /// The LaunchAgent's label (and plist name): `com.zerofarm.agent` for real installs.
    pub label: String,
    /// Load / unload the LaunchAgent with `launchctl` (false in tests: the launcher is then started directly).
    pub launchctl: bool,
    /// Start the launcher when done.
    pub start: bool,
    /// The signed agent package to install from (manifest + files: `farm-agent` and `ZeroFarm`).
    pub source: PathBuf,
}

#[derive(Debug, Clone)]
pub struct UninstallOpts {
    pub root: PathBuf,
    pub launch_agents_dir: Option<PathBuf>,
    pub label: String,
    pub launchctl: bool,
}

pub fn launch_agents_dir(dir: Option<&Path>) -> PathBuf {
    dir.map(PathBuf::from)
        .unwrap_or_else(|| std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default().join("Library").join("LaunchAgents"))
}

pub fn plist_path(label: &str, dir: Option<&Path>) -> PathBuf {
    launch_agents_dir(dir).join(format!("{label}.plist"))
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// The LaunchAgent: runs the launcher at login. `ProcessType` `Interactive`: launchd otherwise applies its default
/// light resource limits to an agent (CPU and I/O throttling); the worker's own nice 19 keeps it out of the user's way.
pub fn plist(label: &str, launcher: &Path) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>{}</string>
	<key>ProgramArguments</key>
	<array>
		<string>{}</string>
	</array>
	<key>RunAtLoad</key>
	<true/>
	<key>KeepAlive</key>
	<false/>
	<key>ProcessType</key>
	<string>Interactive</string>
	<key>StandardOutPath</key>
	<string>/dev/null</string>
	<key>StandardErrorPath</key>
	<string>/dev/null</string>
</dict>
</plist>
"#,
        xml_escape(label),
        xml_escape(&launcher.display().to_string())
    )
}

fn uid() -> u32 {
    // SAFETY: no preconditions.
    unsafe { libc::getuid() }
}

fn launchctl(args: &[&str]) -> Result<(), String> {
    let out = Command::new("/bin/launchctl").args(args).stdin(Stdio::null()).output().map_err(|e| format!("launchctl: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!("launchctl {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim()))
    }
}

/// Asks a running launcher/agent for this root to quit (SIGTERM to the lock holders) and waits for both to be gone.
pub fn stop_running(root: &Path, timeout: Duration) -> bool {
    let running = || instance_running("launcher", root) || instance_running("agent", root);
    if !running() {
        return true;
    }
    if let Some(q) = QuitEvent::open(root) {
        q.set();
    }
    let end = Instant::now() + timeout;
    while Instant::now() < end {
        if !running() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

/// Starts `exe` detached from us: a session of its own, no stdio (a closed terminal never takes it down).
pub fn spawn_detached(exe: &Path, args: &[&str], cwd: &Path) -> std::io::Result<()> {
    use std::os::unix::process::CommandExt;
    let mut cmd = Command::new(exe);
    cmd.args(args).current_dir(cwd).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    // SAFETY: setsid(2) is async-signal-safe.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    cmd.spawn().map(|_| ())
}

/// An absolute root path (it ends up in the plist).
pub fn clean_root(root: &Path) -> PathBuf {
    let abs = std::path::absolute(root).unwrap_or_else(|_| root.to_path_buf());
    PathBuf::from(zerofarm_common::names::normalize_root(&abs))
}

/// Installs the package (verified with `keys`) and writes the LaunchAgent; with `start`, starts it. Returns the
/// version.
pub fn install(opts: &InstallOpts, keys: &TrustedKeys) -> Result<String, String> {
    let opts = &InstallOpts { root: clean_root(&opts.root), ..opts.clone() };
    let pkg = verify_dir(&opts.source, keys, &Expect { kind: Some(Kind::Agent), ..Default::default() })
        .map_err(|e| format!("the downloaded files failed verification: {e}"))?;
    let v = pkg.manifest.version.clone();
    if !zerofarm_common::layout::is_version_label(&v) {
        return Err(format!("bad version {v:?}"));
    }
    if pkg.manifest.platform_name() != zerofarm_common::PLATFORM {
        return Err(format!("this package is for {}, not {}", pkg.manifest.platform_name(), zerofarm_common::PLATFORM));
    }
    let has = |name: &str| pkg.manifest.files.iter().any(|f| f.path == name);
    if !has(AGENT_EXE) || !has(LAUNCHER_EXE) {
        return Err(format!("the package must contain {AGENT_EXE} and {LAUNCHER_EXE}"));
    }
    let layout = Layout::new(&opts.root);
    if !stop_running(&opts.root, Duration::from_secs(30)) {
        return Err("Zero Farm is still running and did not stop; run `ZeroFarm quit` and try again".into());
    }
    if opts.launchctl {
        // (an older install's job: unloaded before it is loaded again; not loaded is fine)
        let _ = launchctl(&["bootout", &format!("gui/{}/{}", uid(), opts.label)]);
    }
    for d in [layout.versions(), layout.state(), layout.cache(), layout.logs()] {
        std::fs::create_dir_all(&d).map_err(|e| format!("{}: {e}", d.display()))?;
    }
    zerofarm_common::fsutil::set_mode(&layout.state(), 0o700);
    let dest = layout.version_dir(&v);
    let same = dest.exists() && verify_dir(&dest, keys, &Expect { build_id: Some(&pkg.build_id), ..Default::default() }).is_ok();
    if !same {
        if dest.exists() {
            zerofarm_common::fsutil::remove_dir_all_retry(&dest, Duration::from_secs(10)).map_err(|e| e.to_string())?;
        }
        let tmp = layout.versions().join(format!(".{v}.installing"));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).map_err(|e| e.to_string())?;
        for f in pkg.manifest.files.iter().map(|f| f.path.as_str()).chain(["manifest.json", "manifest.json.minisig"]) {
            let to = tmp.join(f);
            if let Some(p) = to.parent() {
                std::fs::create_dir_all(p).map_err(|e| e.to_string())?;
            }
            std::fs::copy(opts.source.join(f), &to).map_err(|e| format!("copy {f}: {e}"))?;
        }
        let vb = verify_dir(&tmp, keys, &Expect { build_id: Some(&pkg.build_id), ..Default::default() }).map_err(|e| e.to_string())?;
        crate::cache::mark_executable(&tmp, &vb.manifest);
        zerofarm_common::fsutil::rename_retry(&tmp, &dest).map_err(|e| e.to_string())?;
    }
    crate::cache::mark_executable(&dest, &pkg.manifest);
    let launcher = layout.launcher_exe();
    let staged = opts.root.join(format!("{LAUNCHER_EXE}.installing"));
    std::fs::copy(dest.join(LAUNCHER_EXE), &staged).map_err(|e| format!("copy launcher: {e}"))?;
    zerofarm_common::fsutil::set_mode(&staged, 0o755);
    zerofarm_common::fsutil::rename_retry(&staged, &launcher).map_err(|e| format!("put the launcher in place: {e}"))?;
    // Point `current` at this version unless a newer, verifiable one is already installed.
    let switch = match layout.current() {
        None => true,
        Some(cur) if cur == v => false,
        Some(cur) => {
            let cur_ok = verify_dir(&layout.version_dir(&cur), keys, &Expect { kind: Some(Kind::Agent), version: Some(&cur), ..Default::default() }).is_ok();
            !cur_ok || !crate::update::is_newer(&cur, &v)
        }
    };
    if switch {
        if let Some(cur) = layout.current().filter(|c| *c != v && layout.version_dir(c).exists()) {
            layout.set_previous(Some(&cur)).map_err(|e| e.to_string())?;
        }
        layout.set_current(&v).map_err(|e| e.to_string())?;
        // Installed by the user: no probation for this version.
        layout.mark_healthy(&v).map_err(|e| e.to_string())?;
    }
    layout.set_pending(None).map_err(|e| e.to_string())?;
    let plist_file = plist_path(&opts.label, opts.launch_agents_dir.as_deref());
    if let Some(d) = plist_file.parent() {
        std::fs::create_dir_all(d).map_err(|e| format!("{}: {e}", d.display()))?;
    }
    zerofarm_common::fsutil::atomic_write(&plist_file, plist(&opts.label, &launcher).as_bytes()).map_err(|e| format!("{}: {e}", plist_file.display()))?;
    if opts.start {
        start(opts)?;
    }
    Ok(v)
}

/// Starts the installed launcher: loads the LaunchAgent (its `RunAtLoad` starts it), or without `launchctl` starts
/// it directly.
pub fn start(opts: &InstallOpts) -> Result<(), String> {
    let root = clean_root(&opts.root);
    if opts.launchctl {
        let plist_file = plist_path(&opts.label, opts.launch_agents_dir.as_deref());
        let _ = launchctl(&["bootout", &format!("gui/{}/{}", uid(), opts.label)]);
        launchctl(&["bootstrap", &format!("gui/{}", uid()), &plist_file.display().to_string()])
    } else {
        spawn_detached(&Layout::new(&root).launcher_exe(), &[], &root).map_err(|e| format!("start: {e}"))
    }
}

/// True if `root` looks like a Zero Farm install (so uninstall never deletes a wrong folder).
pub fn looks_installed(root: &Path) -> bool {
    root.join("versions").is_dir() || root.join("state").is_dir() || root.join(LAUNCHER_EXE).is_file()
}

/// Removes the install: stops it, unloads and removes the LaunchAgent (only one that runs this root's launcher) and
/// deletes the root.
pub fn uninstall(opts: &UninstallOpts) -> Result<(), String> {
    let root = clean_root(&opts.root);
    if !looks_installed(&root) {
        return Err(format!("{} is not a Zero Farm folder", root.display()));
    }
    let plist_file = plist_path(&opts.label, opts.launch_agents_dir.as_deref());
    let ours = std::fs::read_to_string(&plist_file)
        .is_ok_and(|t| t.contains(&format!("<string>{}</string>", xml_escape(&Layout::new(&root).launcher_exe().display().to_string()))));
    if !stop_running(&root, Duration::from_secs(30)) {
        return Err("Zero Farm did not stop; run `ZeroFarm quit` and try again".into());
    }
    if ours {
        if opts.launchctl {
            let _ = launchctl(&["bootout", &format!("gui/{}/{}", uid(), opts.label)]);
        }
        let _ = std::fs::remove_file(&plist_file);
    } else if plist_file.exists() {
        log::warn!("{} runs another folder's launcher; leaving it", plist_file.display());
    }
    zerofarm_common::fsutil::remove_dir_all_retry(&root, Duration::from_secs(20)).map_err(|e| format!("remove {}: {e}", root.display()))?;
    Ok(())
}

/// Windows copies the uninstaller out of the install first (a running exe can't be deleted); a Unix file can be
/// deleted while it runs, so never.
pub fn hand_off_if_inside(_opts: &UninstallOpts, _extra_args: &[String]) -> Result<bool, String> {
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plist_runs_the_launcher_at_login() {
        let p = plist("com.zerofarm.agent", Path::new("/Users/x/Library/Application Support/ZeroFarm/ZeroFarm"));
        assert!(p.contains("<string>com.zerofarm.agent</string>"));
        assert!(p.contains("<string>/Users/x/Library/Application Support/ZeroFarm/ZeroFarm</string>"));
        assert!(p.contains("<key>RunAtLoad</key>\n\t<true/>"));
        assert!(p.contains("<key>KeepAlive</key>\n\t<false/>"));
        assert!(!p.contains("Background"), "never the background process type (efficiency cores only)");
        assert_eq!(xml_escape("a&b<c>"), "a&amp;b&lt;c&gt;");
    }
}

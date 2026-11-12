//! Replacing the launcher (PROTOCOL 6). Every agent package carries a `ZeroFarm.exe`. When the
//! running agent is the current, healthy version and its package's launcher is newer (by
//! VERSIONINFO) than the installed `<root>\ZeroFarm.exe`, the agent:
//!
//! 1. copies the verified package launcher to `<root>\ZeroFarm.new.exe` and runs
//!    `ZeroFarm.new.exe selftest` (it must start, trust a key and verify the current version);
//! 2. renames the running launcher to `ZeroFarm.old-<n>.exe` (Windows allows renaming a running
//!    exe) and the new one to `ZeroFarm.exe`;
//! 3. with a launcher that can hand over (`ZEROFARM_LAUNCHER=2`): writes `state\launcher-swap` and
//!    exits [`EXIT_RESTART_LAUNCHER`](zerofarm_common::EXIT_RESTART_LAUNCHER). The old launcher
//!    starts the new one and puts itself back if the new one doesn't take over. With an older
//!    launcher, the new one simply starts at the next logon.
//!
//! A launcher that fails its selftest or is rolled back is marked bad (`state\launcher-bad-<sha>`)
//! and not tried again for 24 h. A launcher is never replaced by an older or equal version, so a
//! rolled-back agent version doesn't undo a newer launcher.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use zerofarm_common::keys::TrustedKeys;
use zerofarm_common::layout::{Layout, LauncherSwap, LAUNCHER_EXE};
use zerofarm_common::manifest::{verify_dir, Expect, Kind};
use zerofarm_common::winres::fixed_file_version;

/// A launcher marked bad is not installed again for this long.
pub const BAD_LAUNCHER_RETRY_S: i64 = 24 * 3600;
pub const STAGED_EXE: &str = "ZeroFarm.new.exe";

type Version = (u16, u16, u16, u16);

fn show(v: Version) -> String {
    format!("{}.{}.{}", v.0, v.1, v.2)
}

/// What a replacement would install.
#[derive(Debug, Clone)]
pub struct Plan {
    /// `versions\<v>\ZeroFarm.exe` (verified as part of the package).
    pub source: PathBuf,
    pub sha256: String,
    pub from: Version,
    pub to: Version,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Swapped; the launcher should hand over now (exit 76).
    Restart,
    /// Swapped; the running launcher can't hand over, the new one starts at the next logon.
    NextLogon,
}

/// Should this agent replace the installed launcher? `None` unless the agent is the current,
/// healthy version, its package verifies, and its launcher is newer and not marked bad.
pub fn plan(layout: &Layout, version: &str, keys: &TrustedKeys) -> Option<Plan> {
    if layout.current().as_deref() != Some(version) || !layout.is_healthy(version) {
        return None;
    }
    let installed = std::fs::read(layout.launcher_exe()).ok()?;
    let dir = layout.version_dir(version);
    let vb = verify_dir(&dir, keys, &Expect { build_id: None, kind: Some(Kind::Agent), version: Some(version) }).ok()?;
    let entry = vb.manifest.files.iter().find(|f| f.path.eq_ignore_ascii_case(LAUNCHER_EXE))?;
    if zerofarm_common::hash::sha256_hex(&installed) == entry.sha256 {
        return None;
    }
    let source = dir.join(&entry.path);
    let to = fixed_file_version(&std::fs::read(&source).ok()?)?;
    let from = fixed_file_version(&installed).unwrap_or((0, 0, 0, 0));
    if to <= from {
        return None;
    }
    if let Some(t) = layout.launcher_bad_since(&entry.sha256) {
        if zerofarm_common::now_unix() - t < BAD_LAUNCHER_RETRY_S {
            return None;
        }
    }
    Some(Plan { source, sha256: entry.sha256.clone(), from, to })
}

/// Runs `<exe> selftest` in the root; Ok(its output) if it exits 0 within `timeout`.
fn selftest(exe: &Path, root: &Path, timeout: Duration) -> Result<String, String> {
    use std::io::Read;
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let mut child = Command::new(exe)
        .arg("selftest")
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map_err(|e| format!("could not start it: {e}"))?;
    let end = Instant::now() + timeout;
    let status = loop {
        if let Some(st) = child.try_wait().map_err(|e| e.to_string())? {
            break st;
        }
        if Instant::now() >= end {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("no answer within {} s", timeout.as_secs()));
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let mut out = String::new();
    if let Some(mut s) = child.stdout.take() {
        let _ = s.read_to_string(&mut out);
    }
    let out = out.trim().chars().take(500).collect::<String>();
    if status.success() {
        Ok(out)
    } else {
        Err(format!("exit code {}: {out}", status.code().unwrap_or(-1)))
    }
}

/// Puts `plan`'s launcher in place (see the module docs). `can_hand_over`: the running launcher
/// set `ZEROFARM_LAUNCHER` >= 2.
pub fn apply(layout: &Layout, plan: &Plan, can_hand_over: bool, selftest_timeout: Duration) -> Result<Outcome, String> {
    let root = &layout.root;
    let staged = root.join(STAGED_EXE);
    let installed = layout.launcher_exe();
    let mark_bad = |why: String| -> String {
        let _ = layout.mark_launcher_bad(&plan.sha256);
        let _ = std::fs::remove_file(&staged);
        why
    };
    std::fs::copy(&plan.source, &staged).map_err(|e| format!("copy to {STAGED_EXE}: {e}"))?;
    let (sha, _) = zerofarm_common::hash::sha256_file(&staged).map_err(|e| e.to_string())?;
    if sha != plan.sha256 {
        let _ = std::fs::remove_file(&staged);
        return Err(format!("{STAGED_EXE} does not match the package's sha256"));
    }
    selftest(&staged, root, selftest_timeout).map_err(|e| mark_bad(format!("launcher {} failed its selftest: {e}", show(plan.to))))?;
    let mut n = zerofarm_common::now_unix();
    let old = loop {
        let name = format!("ZeroFarm.old-{n}.exe");
        if !root.join(&name).exists() {
            break name;
        }
        n += 1;
    };
    zerofarm_common::fsutil::rename_retry(&installed, &root.join(&old)).map_err(|e| format!("rename the running launcher: {e}"))?;
    if let Err(e) = zerofarm_common::fsutil::rename_retry(&staged, &installed) {
        let _ = zerofarm_common::fsutil::rename_retry(&root.join(&old), &installed);
        return Err(format!("put the new launcher in place: {e}"));
    }
    if !can_hand_over {
        return Ok(Outcome::NextLogon);
    }
    let swap = LauncherSwap { old, sha256: plan.sha256.clone(), from: show(plan.from), to: show(plan.to), t: zerofarm_common::now_unix() };
    layout.set_launcher_swap(Some(&swap)).map_err(|e| format!("write state\\launcher-swap: {e}"))?;
    Ok(Outcome::Restart)
}

impl crate::engine::Engine {
    /// Once per run, after the first successful coordinator call: replace the launcher if this
    /// agent's package carries a newer one. Some(Restart) means: exit 76 now.
    pub fn maybe_update_launcher(&mut self) -> Option<Outcome> {
        if self.launcher_checked || !self.cfg.layout.is_healthy(&self.cfg.version) {
            return None;
        }
        self.launcher_checked = true;
        let p = plan(&self.cfg.layout, &self.cfg.version, &self.cfg.keys)?;
        let hand_over = self.cfg.launcher_level >= zerofarm_common::LAUNCHER_LEVEL;
        log::info!("replacing launcher {} with {} from agent {}", show(p.from), show(p.to), self.cfg.version);
        match apply(&self.cfg.layout, &p, hand_over, self.cfg.timings.launcher_selftest) {
            Ok(o) => {
                match o {
                    Outcome::Restart => log::info!("launcher {} is in place; asking the launcher to hand over", show(p.to)),
                    Outcome::NextLogon => log::info!("launcher {} is in place; it starts at the next logon", show(p.to)),
                }
                Some(o)
            }
            Err(e) => {
                log::warn!("launcher not replaced: {e}");
                self.report_crash(zerofarm_common::protocol::CrashReq {
                    build_id: self.cfg.build_id.clone(),
                    kind: "agent".into(),
                    message: format!("launcher update {} -> {}: {e}", show(p.from), show(p.to)),
                    ..Default::default()
                });
                None
            }
        }
    }
}

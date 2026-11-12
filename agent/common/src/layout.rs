//! The install layout (PROTOCOL 6) and the small state files the launcher and the agent share.
//!
//! ```text
//! <root>\ZeroFarm.exe
//!        versions\<version>\farm-agent.exe (+ manifest.json, manifest.json.minisig, files)
//!        state\current  state\previous  state\pending  state\healthy-<v>  state\bad-<v>
//!        state\machine.json  state\launcher-events.jsonl  state\crashes\
//!        cache\builds\<build_id>\  cache\ckpt\<run>\<gen>\  cache\tmp\
//!        results\pending\<lease_id>\
//!        work\<lease_id>\
//!        logs\
//! ```

use crate::fsutil::{atomic_write, read_trimmed, remove_file_quiet};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const AGENT_EXE: &str = "farm-agent.exe";
pub const LAUNCHER_EXE: &str = "ZeroFarm.exe";

/// Version labels are used as directory and file names: `[0-9A-Za-z.+-]{1,64}`, no `..`.
pub fn is_version_label(v: &str) -> bool {
    !v.is_empty()
        && v.len() <= 64
        && !v.contains("..")
        && !v.starts_with('.')
        && !v.ends_with('.')
        && v.bytes().all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'+' | b'-'))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    pub root: PathBuf,
}

/// `state\launcher-swap`: written by an agent that put a newer `ZeroFarm.exe` in place, just
/// before it exits with `EXIT_RESTART_LAUNCHER`; read by the launcher it replaced, which starts
/// the new one and puts the old one back if the new one doesn't take over (PROTOCOL 6).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct LauncherSwap {
    /// The old launcher's file name now, in the root: `ZeroFarm.old-<n>.exe`.
    pub old: String,
    /// sha256 of the new `ZeroFarm.exe`.
    pub sha256: String,
    /// Old and new launcher versions (from their VERSIONINFO; "0.0.0" when absent).
    pub from: String,
    pub to: String,
    pub t: i64,
}

/// Leftovers of a launcher replacement, deleted at the next launcher start:
/// `ZeroFarm.old-<n>.exe` (the replaced launcher), `ZeroFarm.bad-<n>.exe` (a new launcher that
/// was rolled back) and, with `staged`, `ZeroFarm.new.exe` (an agent's staging copy).
pub fn is_launcher_leftover(name: &str, staged: bool) -> bool {
    let n = name.to_ascii_lowercase();
    let numbered = |prefix: &str| {
        n.strip_prefix(prefix).and_then(|r| r.strip_suffix(".exe")).is_some_and(|k| !k.is_empty() && k.bytes().all(|c| c.is_ascii_digit()))
    };
    numbered("zerofarm.old-") || numbered("zerofarm.bad-") || (staged && n == "zerofarm.new.exe")
}

/// Something the launcher wants the next agent to report (`/v1/crash` kind `agent`).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct LauncherEvent {
    /// `rollback` | `verify_failed` | `launcher_rollback`
    pub kind: String,
    pub from: String,
    pub to: Option<String>,
    pub reason: String,
    pub t: i64,
}

impl Layout {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Layout { root: root.into() }
    }

    pub fn versions(&self) -> PathBuf {
        self.root.join("versions")
    }
    pub fn version_dir(&self, v: &str) -> PathBuf {
        self.versions().join(v)
    }
    pub fn state(&self) -> PathBuf {
        self.root.join("state")
    }
    pub fn cache(&self) -> PathBuf {
        self.root.join("cache")
    }
    pub fn cache_builds(&self) -> PathBuf {
        self.cache().join("builds")
    }
    pub fn cache_ckpt(&self) -> PathBuf {
        self.cache().join("ckpt")
    }
    pub fn cache_tmp(&self) -> PathBuf {
        self.cache().join("tmp")
    }
    pub fn results_pending(&self) -> PathBuf {
        self.root.join("results").join("pending")
    }
    pub fn work(&self) -> PathBuf {
        self.root.join("work")
    }
    pub fn logs(&self) -> PathBuf {
        self.root.join("logs")
    }
    pub fn machine_json(&self) -> PathBuf {
        self.state().join("machine.json")
    }
    pub fn crashes(&self) -> PathBuf {
        self.state().join("crashes")
    }
    pub fn launcher_exe(&self) -> PathBuf {
        self.root.join(LAUNCHER_EXE)
    }

    fn read_label(&self, name: &str) -> Option<String> {
        read_trimmed(&self.state().join(name)).filter(|v| is_version_label(v))
    }
    fn write_label(&self, name: &str, v: Option<&str>) -> std::io::Result<()> {
        let p = self.state().join(name);
        match v {
            Some(v) => {
                assert!(is_version_label(v), "bad version label {v:?}");
                atomic_write(&p, v.as_bytes())
            }
            None => {
                remove_file_quiet(&p);
                Ok(())
            }
        }
    }

    pub fn current(&self) -> Option<String> {
        self.read_label("current")
    }
    pub fn set_current(&self, v: &str) -> std::io::Result<()> {
        self.write_label("current", Some(v))
    }
    pub fn previous(&self) -> Option<String> {
        self.read_label("previous")
    }
    pub fn set_previous(&self, v: Option<&str>) -> std::io::Result<()> {
        self.write_label("previous", v)
    }
    pub fn pending(&self) -> Option<String> {
        self.read_label("pending")
    }
    pub fn set_pending(&self, v: Option<&str>) -> std::io::Result<()> {
        self.write_label("pending", v)
    }

    pub fn healthy_marker(&self, v: &str) -> PathBuf {
        self.state().join(format!("healthy-{v}"))
    }
    pub fn is_healthy(&self, v: &str) -> bool {
        self.healthy_marker(v).exists()
    }
    pub fn mark_healthy(&self, v: &str) -> std::io::Result<()> {
        atomic_write(&self.healthy_marker(v), crate::now_unix().to_string().as_bytes())
    }

    /// `bad-<v>` holds the time a version was rolled back; agents skip it for a while.
    pub fn bad_since(&self, v: &str) -> Option<i64> {
        read_trimmed(&self.state().join(format!("bad-{v}"))).and_then(|s| s.parse().ok())
    }
    pub fn mark_bad(&self, v: &str) -> std::io::Result<()> {
        atomic_write(&self.state().join(format!("bad-{v}")), crate::now_unix().to_string().as_bytes())
    }

    pub fn launcher_swap_path(&self) -> PathBuf {
        self.state().join("launcher-swap")
    }
    pub fn launcher_swap(&self) -> Option<LauncherSwap> {
        std::fs::read(self.launcher_swap_path()).ok().and_then(|b| serde_json::from_slice(&b).ok())
    }
    pub fn set_launcher_swap(&self, s: Option<&LauncherSwap>) -> std::io::Result<()> {
        match s {
            Some(s) => atomic_write(&self.launcher_swap_path(), &serde_json::to_vec(s).expect("json")),
            None => {
                remove_file_quiet(&self.launcher_swap_path());
                Ok(())
            }
        }
    }
    /// `state\launcher-ok-<pid>`: a launcher started with `--takeover` writes it once its first
    /// agent has run for a while; the launcher handing over waits for it.
    pub fn launcher_ok_marker(&self, pid: u32) -> PathBuf {
        self.state().join(format!("launcher-ok-{pid}"))
    }
    /// `state\launcher-bad-<first 16 hex of its sha256>`: a launcher that failed its selftest or
    /// didn't take over; agents don't install it again for a while.
    pub fn launcher_bad_marker(&self, sha256: &str) -> PathBuf {
        self.state().join(format!("launcher-bad-{}", &sha256[..sha256.len().min(16)]))
    }
    pub fn launcher_bad_since(&self, sha256: &str) -> Option<i64> {
        read_trimmed(&self.launcher_bad_marker(sha256)).and_then(|s| s.parse().ok())
    }
    pub fn mark_launcher_bad(&self, sha256: &str) -> std::io::Result<()> {
        atomic_write(&self.launcher_bad_marker(sha256), crate::now_unix().to_string().as_bytes())
    }

    pub fn launcher_events_path(&self) -> PathBuf {
        self.state().join("launcher-events.jsonl")
    }
    pub fn push_launcher_event(&self, ev: &LauncherEvent) -> std::io::Result<()> {
        use std::io::Write;
        std::fs::create_dir_all(self.state())?;
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(self.launcher_events_path())?;
        writeln!(f, "{}", serde_json::to_string(ev).expect("json"))
    }
    /// Reads and removes the launcher's queued events.
    pub fn take_launcher_events(&self) -> Vec<LauncherEvent> {
        let p = self.launcher_events_path();
        let Ok(text) = std::fs::read_to_string(&p) else { return vec![] };
        remove_file_quiet(&p);
        text.lines().filter_map(|l| serde_json::from_str(l).ok()).collect()
    }
}

/// The default install root: `%LOCALAPPDATA%\ZeroFarm`.
pub fn default_root() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(|| {
        std::env::var_os("USERPROFILE").map(|p| Path::new(&p).join("AppData").join("Local")).unwrap_or_default()
    });
    base.join("ZeroFarm")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels() {
        assert!(is_version_label("0.3.1"));
        assert!(is_version_label("1.0.0-rc.1+abc"));
        assert!(!is_version_label("../x"));
        assert!(!is_version_label("a\\b"));
        assert!(!is_version_label(".."));
        assert!(!is_version_label(""));
    }

    #[test]
    fn launcher_leftovers() {
        assert!(is_launcher_leftover("ZeroFarm.old-1794300000.exe", false));
        assert!(is_launcher_leftover("zerofarm.BAD-7.exe", false));
        assert!(!is_launcher_leftover("ZeroFarm.new.exe", false));
        assert!(is_launcher_leftover("ZeroFarm.new.exe", true));
        assert!(!is_launcher_leftover("ZeroFarm.exe", true));
        assert!(!is_launcher_leftover("ZeroFarm.old-.exe", true));
        assert!(!is_launcher_leftover("ZeroFarm.old-1x.exe", true));
        assert!(!is_launcher_leftover("ZeroFarmSetup.exe", true));
    }

    #[test]
    fn state_roundtrip() {
        let d = tempfile::tempdir().unwrap();
        let l = Layout::new(d.path());
        assert_eq!(l.current(), None);
        l.set_current("0.1.0").unwrap();
        l.set_previous(Some("0.0.9")).unwrap();
        assert_eq!(l.current().as_deref(), Some("0.1.0"));
        assert_eq!(l.previous().as_deref(), Some("0.0.9"));
        l.set_previous(None).unwrap();
        assert_eq!(l.previous(), None);
        assert!(!l.is_healthy("0.1.0"));
        l.mark_healthy("0.1.0").unwrap();
        assert!(l.is_healthy("0.1.0"));
        l.push_launcher_event(&LauncherEvent { kind: "rollback".into(), from: "b".into(), to: Some("a".into()), reason: "r".into(), t: 1 })
            .unwrap();
        let evs = l.take_launcher_events();
        assert_eq!(evs.len(), 1);
        assert!(l.take_launcher_events().is_empty());
    }
}

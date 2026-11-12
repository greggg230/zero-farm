//! Agent self-update (PROTOCOL 6, DESIGN 2.2): learn of a newer version from
//! `GET /v1/agent/latest` (every 6 h) or from `{"update": {"min_agent"}}`, download and verify it
//! into `versions\<new>\`, and once no lease is running write `state\pending` and exit 75. The
//! launcher does the switch (and the rollback).

use crate::engine::Engine;
use std::time::{Duration, Instant};
use zerofarm_common::layout::{is_version_label, Layout, AGENT_EXE};
use zerofarm_common::manifest::{verify_dir, Expect, Kind};
use zerofarm_common::protocol::AgentLatest;

/// A version the launcher rolled back from is not retried for this long.
pub const BAD_VERSION_RETRY_S: i64 = 24 * 3600;

pub fn parse_version(v: &str) -> Option<semver::Version> {
    semver::Version::parse(v.trim().trim_start_matches('v')).ok()
}

/// True when `candidate` is a newer semver than `running`.
pub fn is_newer(candidate: &str, running: &str) -> bool {
    match (parse_version(candidate), parse_version(running)) {
        (Some(c), Some(r)) => c > r,
        _ => false,
    }
}

/// Removes `versions\*` other than current, previous, pending and the running one, and the
/// healthy/bad markers of versions that are gone (PROTOCOL 6: the last two versions are kept).
pub fn cleanup_versions(layout: &Layout, running: &str) {
    let keep: Vec<String> = [layout.current(), layout.previous(), layout.pending(), Some(running.to_string())]
        .into_iter()
        .flatten()
        .collect();
    if let Ok(rd) = std::fs::read_dir(layout.versions()) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if keep.contains(&name) {
                continue;
            }
            log::info!("removing old agent version {name}");
            let _ = zerofarm_common::fsutil::remove_dir_all_retry(&e.path(), Duration::from_secs(5));
        }
    }
    if let Ok(rd) = std::fs::read_dir(layout.state()) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if let Some(v) = name.strip_prefix("healthy-") {
                if !keep.iter().any(|k| k == v) {
                    let _ = std::fs::remove_file(e.path());
                }
            }
            if let Some(v) = name.strip_prefix("bad-") {
                let old = layout.bad_since(v).map(|t| zerofarm_common::now_unix() - t > 7 * 24 * 3600).unwrap_or(true);
                if old {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
    }
}

impl Engine {
    /// `GET /v1/agent/latest` at start and every 6 h.
    pub fn maybe_check_latest(&mut self) {
        let due = self.last_update_check.map(|t| t.elapsed() >= self.cfg.timings.update_check).unwrap_or(true);
        if !due {
            return;
        }
        self.last_update_check = Some(Instant::now());
        if let Err(e) = self.update_to_latest(None) {
            log::warn!("update check: {e}");
        }
    }

    /// Fetches `latest`; if it is newer than us (and at least `min_agent` when given), downloads
    /// and verifies it and sets `pending_update`.
    pub fn update_to_latest(&mut self, min_agent: Option<&str>) -> Result<(), String> {
        let path = format!("/v1/agent/latest?channel={}", self.cfg.channel);
        let latest: AgentLatest = match self.client.get_json(&path) {
            Ok(l) => l,
            Err(e) if e.status() == Some(404) => return Ok(()),
            Err(e) => return Err(e.to_string()),
        };
        if let Some(min) = min_agent {
            if !is_newer(&latest.version, min) && parse_version(&latest.version) != parse_version(min) {
                return Err(format!("latest agent {} is older than the required {min}", latest.version));
            }
        }
        if !is_newer(&latest.version, &self.cfg.version) {
            return Ok(());
        }
        if !is_version_label(&latest.version) || !zerofarm_common::is_build_id(&latest.build_id) {
            return Err(format!("bad latest answer {latest:?}"));
        }
        if let Some(t) = self.cfg.layout.bad_since(&latest.version) {
            if zerofarm_common::now_unix() - t < BAD_VERSION_RETRY_S {
                log::info!("agent {} was rolled back recently; not retrying it yet", latest.version);
                return Ok(());
            }
        }
        log::info!("agent {} available (running {}); downloading", latest.version, self.cfg.version);
        self.set_status(crate::status::Phase::Updating, &latest.version, None);
        self.prepare_version(&latest)?;
        self.pending_update = Some(latest.version.clone());
        Ok(())
    }

    fn prepare_version(&mut self, latest: &AgentLatest) -> Result<(), String> {
        let layout = &self.cfg.layout;
        let dest = layout.version_dir(&latest.version);
        let expect = Expect { build_id: Some(&latest.build_id), kind: Some(Kind::Agent), version: Some(&latest.version) };
        let check_entry = |m: &zerofarm_common::manifest::Manifest| -> Result<(), String> {
            if m.entry.as_deref().map(|e| e.eq_ignore_ascii_case(AGENT_EXE)) == Some(true) {
                Ok(())
            } else {
                Err(format!("agent manifest entry is {:?}", m.entry))
            }
        };
        if dest.exists() {
            if let Ok(v) = verify_dir(&dest, &self.cfg.keys, &expect) {
                return check_entry(&v.manifest);
            }
        }
        let tmp = layout.cache_tmp().join(format!("agent-{}", latest.version));
        let cap = self.settings().disk_cap_mb << 20;
        let cache = &self.cache;
        let mut room = |need: u64| cache.ensure_space(need, cap, &[]);
        let control = self.control.clone();
        let stop = move || control.quitting();
        let v = crate::cache::download_build(&self.client, &self.cfg.keys, &latest.manifest, &expect, &dest, &tmp, &mut room, &stop)
            .map_err(|e| e.to_string())?;
        if let Err(e) = check_entry(&v.manifest) {
            let _ = std::fs::remove_dir_all(&dest);
            return Err(e);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semver_compare() {
        assert!(is_newer("0.2.0", "0.1.9"));
        assert!(is_newer("1.0.0", "1.0.0-rc.1"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.0.9", "0.1.0"));
        assert!(!is_newer("garbage", "0.1.0"));
    }

    #[test]
    fn cleanup_keeps_current_previous_pending_running() {
        let d = tempfile::tempdir().unwrap();
        let l = Layout::new(d.path());
        for v in ["0.1.0", "0.2.0", "0.3.0", "0.4.0", "0.5.0"] {
            std::fs::create_dir_all(l.version_dir(v)).unwrap();
            l.mark_healthy(v).unwrap();
        }
        l.set_current("0.3.0").unwrap();
        l.set_previous(Some("0.2.0")).unwrap();
        l.set_pending(Some("0.5.0")).unwrap();
        cleanup_versions(&l, "0.3.0");
        let left: Vec<String> = std::fs::read_dir(l.versions()).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into()).collect();
        assert_eq!(left.len(), 3, "{left:?}");
        assert!(!l.version_dir("0.1.0").exists() && !l.version_dir("0.4.0").exists());
        assert!(!l.is_healthy("0.1.0") && l.is_healthy("0.3.0"));
    }
}

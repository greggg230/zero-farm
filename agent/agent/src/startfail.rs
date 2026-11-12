//! A worker that can't be started (PROTOCOL 3, rev 5). Windows application control (Smart App
//! Control / WDAC, AppLocker / SRP) or Defender may refuse to run the unsigned job program: the
//! agent then reports it once per build per day as `/v1/crash` kind `blocked`, gives the lease
//! back and waits before asking for work again (15 min, 1 h, 4 h, 16 h, 24 h), instead of leasing
//! again at once. Found on a friend's PC on 2026-11-12: agent 0.2.1 leased 521 times in 45 min,
//! every lease failing with os error 4551. Any other start failure waits too (1 min doubling to
//! 1 h). The wait survives agent restarts (`state\start-backoff.json`) and ends when a worker
//! starts.

use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};
use zerofarm_common::layout::Layout;

/// ERROR_SYSTEM_INTEGRITY_POLICY_VIOLATION: Smart App Control / WDAC blocked the file.
pub const ERROR_SYSTEM_INTEGRITY_POLICY_VIOLATION: i32 = 4551;
/// ERROR_ACCESS_DISABLED_BY_POLICY: AppLocker or a software restriction policy.
pub const ERROR_ACCESS_DISABLED_BY_POLICY: i32 = 1260;
/// ERROR_VIRUS_INFECTED: Defender (or another antivirus) refused the file.
pub const ERROR_VIRUS_INFECTED: i32 = 225;

/// The raw Windows error if `e` means "blocked by policy" (nothing the agent can retry its way out of).
pub fn blocked_code(e: &std::io::Error) -> Option<i32> {
    e.raw_os_error()
        .filter(|c| [ERROR_SYSTEM_INTEGRITY_POLICY_VIOLATION, ERROR_ACCESS_DISABLED_BY_POLICY, ERROR_VIRUS_INFECTED].contains(c))
}

/// What blocked it, in words, for the crash report and the log.
pub fn blocked_by(code: i32) -> &'static str {
    match code {
        ERROR_SYSTEM_INTEGRITY_POLICY_VIOLATION => "Smart App Control / Windows Defender Application Control",
        ERROR_ACCESS_DISABLED_BY_POLICY => "AppLocker or a software restriction policy",
        ERROR_VIRUS_INFECTED => "Windows Defender (or another antivirus)",
        _ => "a Windows policy",
    }
}

/// The wait after start failures, persisted in `state\start-backoff.json`.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct StartBackoff {
    /// Unix time before which no lease is asked for (0: none).
    pub until: i64,
    /// Failures in a row of the current kind (the next wait is `first × factor^level`).
    pub level: u32,
    /// The last failure was "blocked by policy".
    pub blocked: bool,
    /// The same moment as `until`, to the millisecond, for this process (`until` is whole seconds).
    #[serde(skip)]
    pub deadline: Option<Instant>,
}

/// `first × factor^level`, capped at `max`.
pub fn step(first: Duration, factor: u32, level: u32, max: Duration) -> Duration {
    let mut d = first;
    for _ in 0..level {
        d = d.saturating_mul(factor);
        if d >= max {
            return max;
        }
    }
    d.min(max)
}

impl StartBackoff {
    fn path(layout: &Layout) -> std::path::PathBuf {
        layout.state().join("start-backoff.json")
    }

    pub fn load(layout: &Layout) -> StartBackoff {
        std::fs::read(Self::path(layout)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    fn save(&self, layout: &Layout) {
        let p = Self::path(layout);
        if *self == StartBackoff::default() {
            let _ = std::fs::remove_file(p);
        } else {
            let _ = std::fs::create_dir_all(layout.state());
            let _ = zerofarm_common::fsutil::atomic_write(&p, &serde_json::to_vec(self).expect("json"));
        }
    }

    /// A start failure at `now`: the wait before the next lease request.
    pub fn fail(&mut self, layout: &Layout, blocked: bool, t: &crate::Timings, now: i64) -> Duration {
        if blocked != self.blocked {
            self.level = 0;
        }
        let d = if blocked {
            step(t.blocked_backoff, 4, self.level, t.blocked_backoff_max)
        } else {
            step(t.start_backoff, 2, self.level, t.start_backoff_max)
        };
        self.level = self.level.saturating_add(1);
        self.blocked = blocked;
        self.deadline = Some(Instant::now() + d);
        // Whole seconds, at least 1, so a persisted wait is never shorter than the one in memory.
        self.until = now + d.as_secs().max(1) as i64 + i64::from(d.subsec_nanos() > 0);
        self.save(layout);
        d
    }

    /// A worker started: no more waiting.
    pub fn reset(&mut self, layout: &Layout) {
        if *self != StartBackoff::default() {
            *self = StartBackoff::default();
            self.save(layout);
        }
    }

    /// Time left to wait at `now` (never more than the longest step, whatever the file says).
    pub fn remaining(&self, now: i64, t: &crate::Timings) -> Option<Duration> {
        if let Some(d) = self.deadline {
            return d.checked_duration_since(Instant::now()).filter(|left| !left.is_zero());
        }
        let left = self.until - now;
        (left > 0).then(|| Duration::from_secs(left as u64).min(t.blocked_backoff_max.max(t.start_backoff_max)))
    }
}

/// `state\blocked-<build_id>`: when this build was last reported `blocked` (once a day per build).
fn marker(layout: &Layout, build_id: &str) -> std::path::PathBuf {
    layout.state().join(format!("blocked-{build_id}"))
}

/// True (and the marker set) if a `blocked` report for `build_id` is due: none in the last 24 h.
pub fn take_blocked_report(layout: &Layout, build_id: &str, now: i64) -> bool {
    if !zerofarm_common::is_build_id(build_id) {
        return false;
    }
    let p = marker(layout, build_id);
    let last = zerofarm_common::fsutil::read_trimmed(&p).and_then(|s| s.parse::<i64>().ok());
    if last.is_some_and(|t| now - t < 86_400 && now >= t) {
        return false;
    }
    let _ = std::fs::create_dir_all(layout.state());
    let _ = zerofarm_common::fsutil::atomic_write(&p, now.to_string().as_bytes());
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_policy_blocks() {
        for c in [4551, 1260, 225] {
            assert_eq!(blocked_code(&std::io::Error::from_raw_os_error(c)), Some(c));
        }
        for c in [2, 5, 193] {
            assert_eq!(blocked_code(&std::io::Error::from_raw_os_error(c)), None);
        }
        assert_eq!(blocked_code(&std::io::Error::other("x")), None);
    }

    #[test]
    fn backoff_steps_and_persistence() {
        let d = tempfile::tempdir().unwrap();
        let layout = Layout::new(d.path());
        let t = crate::Timings::default();
        let mut b = StartBackoff::load(&layout);
        let mins = |b: &mut StartBackoff, blocked: bool| b.fail(&layout, blocked, &t, 1000).as_secs() / 60;
        // Blocked: 15 min, 1 h, 4 h, 16 h, then 24 h for good.
        let got: Vec<u64> = (0..7).map(|_| mins(&mut b, true)).collect();
        assert_eq!(got, [15, 60, 240, 960, 1440, 1440, 1440]);
        let loaded = StartBackoff::load(&layout);
        assert_eq!((loaded.until, loaded.level, loaded.blocked), (b.until, b.level, b.blocked), "persisted");
        assert!(b.remaining(1000, &t).is_some_and(|d| d > Duration::from_secs(86_000)), "in memory: to the millisecond");
        let b = loaded;
        assert_eq!(b.remaining(1000, &t), Some(Duration::from_secs(86_400)));
        assert_eq!(b.remaining(1000 + 86_400, &t), None);
        // Another kind of failure starts its own sequence: 1 min doubling to 1 h.
        let mut b = b;
        let got: Vec<u64> = (0..8).map(|_| mins(&mut b, false)).collect();
        assert_eq!(got, [1, 2, 4, 8, 16, 32, 60, 60]);
        b.reset(&layout);
        assert_eq!(StartBackoff::load(&layout), StartBackoff::default());
        assert!(!layout.state().join("start-backoff.json").exists());
        // A file from the future can't make the agent wait longer than the longest step.
        let far = StartBackoff { until: i64::MAX / 2, level: 1, blocked: true, deadline: None };
        assert_eq!(far.remaining(0, &t), Some(Duration::from_secs(86_400)));
    }

    #[test]
    fn blocked_reports_once_a_day_per_build() {
        let d = tempfile::tempdir().unwrap();
        let layout = Layout::new(d.path());
        let (a, b) = ("aaaaaaaaaaaaaaaaaaaaaaaa", "bbbbbbbbbbbbbbbbbbbbbbbb");
        assert!(take_blocked_report(&layout, a, 1000));
        assert!(!take_blocked_report(&layout, a, 1000 + 3600));
        assert!(take_blocked_report(&layout, b, 1000 + 3600), "another build is reported");
        assert!(take_blocked_report(&layout, a, 1000 + 86_400));
        assert!(!take_blocked_report(&layout, "../x", 0), "only build ids name files");
    }
}

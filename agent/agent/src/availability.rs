//! Availability (DESIGN 5.4): how many hours a day this PC is on with the agent running, and how
//! many of those the idle policy lets it work. Kept per local day in `state\availability.json`
//! (the last 14 days); the profile reports 7-day means of complete days (`on_h_day`,
//! `idle_h_day`, PROTOCOL 3), so they change at most once a day.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use zerofarm_common::layout::Layout;

/// Gaps between policy samples longer than this count nothing (the PC slept, or the agent was
/// stopped or stuck): availability is only what was seen.
pub const MAX_GAP_MS: i64 = 10 * 60 * 1000;
const KEEP_DAYS: i64 = 14;
const SAVE_EVERY: Duration = Duration::from_secs(300);

#[derive(Serialize, Deserialize, Default, Clone, Copy, Debug, PartialEq)]
pub struct Day {
    /// Seconds the agent ran.
    pub on_s: f64,
    /// Seconds the idle policy said "work".
    pub idle_s: f64,
}

#[derive(Serialize, Deserialize, Default, Clone, Debug, PartialEq)]
pub struct History {
    /// The first local day anything was recorded (YYYY-MM-DD).
    pub first_day: Option<String>,
    pub days: BTreeMap<String, Day>,
}

pub fn path(layout: &Layout) -> PathBuf {
    layout.state().join("availability.json")
}

pub fn load(layout: &Layout) -> History {
    std::fs::read(path(layout)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

/// Days since 1970-01-01 of a civil date (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { yoe + era * 400 + 1 } else { yoe + era * 400 }, m, d)
}

/// "YYYY-MM-DD" → days since the epoch.
pub fn day_number(day: &str) -> Option<i64> {
    let mut it = day.split('-').map(|p| p.parse::<i64>().ok());
    let (y, m, d) = (it.next()??, it.next()??, it.next()??);
    ((1..=12).contains(&m) && (1..=31).contains(&d)).then(|| days_from_civil(y, m, d))
}

pub fn day_name(n: i64) -> String {
    let (y, m, d) = civil_from_days(n);
    format!("{y:04}-{m:02}-{d:02}")
}

impl History {
    /// (idle hours per day, on hours per day): means over the last 7 complete days before
    /// `today`, counting days the agent didn't run as 0. The install day (partial) only counts
    /// while it is the only complete day. `None` before one complete day.
    pub fn means(&self, today: &str) -> Option<(f64, f64)> {
        let t = day_number(today)?;
        let first = day_number(self.first_day.as_deref()?)?;
        if first >= t {
            return None;
        }
        let start = if first + 1 < t { (first + 1).max(t - 7) } else { first };
        let n = (t - start) as f64;
        let (mut idle, mut on) = (0.0, 0.0);
        for k in start..t {
            if let Some(d) = self.days.get(&day_name(k)) {
                idle += d.idle_s;
                on += d.on_s;
            }
        }
        let h = |s: f64| (s / 3600.0 / n * 10.0).round() / 10.0;
        Some((h(idle).min(24.0), h(on).min(24.0)))
    }

    fn add(&mut self, day: &str, ms: i64, work: bool) {
        if self.first_day.is_none() {
            self.first_day = Some(day.to_string());
        }
        let e = self.days.entry(day.to_string()).or_default();
        e.on_s += ms as f64 / 1000.0;
        if work {
            e.idle_s += ms as f64 / 1000.0;
        }
        if let Some(t) = day_number(day) {
            self.days.retain(|k, _| day_number(k).is_some_and(|n| n > t - KEEP_DAYS));
        }
    }
}

/// Records policy samples into a [`History`] and saves it now and then.
pub struct Tracker {
    path: PathBuf,
    pub history: History,
    last: Option<(i64, String, bool)>,
    saved: Instant,
    dirty: bool,
}

impl Tracker {
    pub fn load(layout: &Layout) -> Tracker {
        Tracker { path: path(layout), history: load(layout), last: None, saved: Instant::now(), dirty: false }
    }

    /// One policy decision at `now_ms` (Unix ms) on local day `day`. The time since the previous
    /// sample is counted with the previous sample's answer, on the previous sample's day.
    pub fn record(&mut self, now_ms: i64, day: &str, work: bool) {
        if let Some((t, d, w)) = self.last.take() {
            let gap = now_ms - t;
            if gap > 0 && gap <= MAX_GAP_MS {
                self.history.add(&d, gap, w);
                self.dirty = true;
            }
        }
        self.last = Some((now_ms, day.to_string(), work));
        if self.dirty && self.saved.elapsed() >= SAVE_EVERY {
            self.save();
        }
    }

    pub fn save(&mut self) {
        if !self.dirty {
            return;
        }
        if let Err(e) = zerofarm_common::fsutil::atomic_write(&self.path, &serde_json::to_vec(&self.history).expect("json")) {
            log::warn!("could not save availability: {e}");
        }
        self.saved = Instant::now();
        self.dirty = false;
    }
}

/// Now, in Unix milliseconds.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates() {
        assert_eq!(day_number("1970-01-01"), Some(0));
        assert_eq!(day_name(day_number("2026-11-12").unwrap()), "2026-11-12");
        assert_eq!(day_name(day_number("2024-03-01").unwrap() - 1), "2024-02-29");
        assert_eq!(day_name(day_number("2027-01-01").unwrap() - 1), "2026-12-31");
        assert_eq!(day_number("2026-13-01"), None);
        assert_eq!(day_number("junk"), None);
    }

    #[test]
    fn samples_hold_until_the_next_one_and_gaps_count_nothing() {
        let d = tempfile::tempdir().unwrap();
        let mut t = Tracker::load(&Layout::new(d.path()));
        let m = 60_000;
        t.record(0, "2026-11-10", true);
        t.record(9 * m, "2026-11-10", false); // 9 min allowed to work
        t.record(10 * m, "2026-11-10", true); // 1 min on, the user was there
        t.record(10 * m + 30_000, "2026-11-11", true); // 30 s idle, counted on the 10th
        t.record(21 * m + 30_000, "2026-11-11", true); // an 11 min gap (asleep): nothing
        t.record(26 * m + 30_000, "2026-11-11", false); // 5 min idle on the 11th
        assert_eq!(t.history.days["2026-11-10"], Day { on_s: 630.0, idle_s: 570.0 });
        assert_eq!(t.history.days["2026-11-11"], Day { on_s: 300.0, idle_s: 300.0 });
        assert_eq!(t.history.first_day.as_deref(), Some("2026-11-10"));
        t.save();
        assert_eq!(load(&Layout::new(d.path())), t.history);
    }

    #[test]
    fn means_over_complete_days() {
        let mut h = History::default();
        assert_eq!(h.means("2026-11-12"), None);
        h.add("2026-11-10", 3 * 3_600_000, true); // install day: 3 h idle
        h.add("2026-11-10", 3_600_000, false);
        assert_eq!(h.means("2026-11-10"), None, "no complete day yet");
        assert_eq!(h.means("2026-11-11"), Some((3.0, 4.0)), "only the install day is complete");
        h.add("2026-11-11", 10 * 3_600_000, true);
        assert_eq!(h.means("2026-11-12"), Some((10.0, 10.0)), "the partial install day is left out");
        // Days the agent didn't run count as 0; at most 7 days.
        assert_eq!(h.means("2026-11-15"), Some((2.5, 2.5)));
        assert_eq!(h.means("2026-11-30"), Some((0.0, 0.0)));
        // Old days are dropped after 14 days.
        h.add("2026-11-30", 1000, true);
        assert!(!h.days.contains_key("2026-11-11"));
    }
}

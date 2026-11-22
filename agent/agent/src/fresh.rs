//! rev 26 (PROTOCOL 18.3), agent 0.3.17: the freshness cap. A rolling lease's coordinator may send the run's freshness
//! budget `fresh_s` - how long a game may last for its first rows to reach the trainer within the run's staleness window
//! (zero-core's pool drops rows searched by a network more than `max_stale` generations behind). The tuner maximises
//! leaves/s by raising the games in flight, and a game lasts about (games in flight) / (games finished per second)
//! (Little's law): past rate x `fresh_s` games in flight the extra games only make rows the trainer drops. Measured on
//! legacy-meta3-r1, 2026-11-22 (pool `by_machine`, 100 iterations): a laptop GPU lost 72 % of its rows, a Mac 25 %, the
//! CUDA desktops 12-14 %.
//!
//! So the job's games in flight (every worker's, 13.5) are capped at `cap = max(floor, rate x fresh_s)`:
//! - `rate`: the lease's own completed games per running second (every worker's `fin`s, paused and starved time left
//!   out), over the last `RATE_WINDOW_S` of running time (longer, up to `RATE_WINDOW_MAX` times, until `RATE_MIN_GAMES`
//!   finished) - and only from once a whole turnover of the layout finished (`Gauge::settle`): a fresh worker starts
//!   every game at once, so its first game length finishes few games and would read as a slow rate;
//! - `floor`: `FLOOR_GPU` / `FLOOR_CPU` games in flight in all, and each worker keeps at least `MIN_PER_WORKER_GPU` /
//!   `MIN_PER_WORKER_CPU` (fewer workers where the cap is lower, `tune::Space::fit`) - enough to keep a GPU's batches and
//!   a CPU's search threads usefully busy; low enough to bind on a fast CUDA crew (its 3 x 1,152 would come down to
//!   ~900-1,600 at ~5.6 games/s and a 157-292 s budget);
//! - `next_cap`: a new cap replaces the running one only when it differs by more than `HYSTERESIS`, and after a change
//!   that moves the layout, the next decision waits for a turnover of the new layout (`Gauge::settle`).
//!
//! No budget (an older coordinator, a machine the owner's setting leaves out, a run with too little history): no cap,
//! exactly as before.

use std::collections::VecDeque;
use std::time::Instant;

/// The rate is measured over this much running time (seconds) at least (`Timings::fresh_window`)...
pub const RATE_WINDOW_S: f64 = 300.0;
/// ...and over longer windows, up to this many times as long, until it holds `RATE_MIN_GAMES` finished games (a CPU
/// laptop's 32 games in flight at ~20 minutes a game finish ~8 in 5 minutes).
pub const RATE_WINDOW_MAX: f64 = 6.0;
pub const RATE_MIN_GAMES: u64 = 16;
/// A sample of the finished games at most once per this share of the window (10 s of 300).
pub const SAMPLE_SHARE: f64 = 1.0 / 30.0;
/// A tick gap longer than this (the PC slept, the loop stalled) adds no running time.
pub const MAX_TICK_S: f64 = 30.0;
/// A new cap replaces the running one only when it differs from it by more than this share.
pub const HYSTERESIS: f64 = 0.15;
/// The cap is never below this many games in flight in all (a GPU job: two batches of 64 rows; a CPU job: two groups of
/// the Windows tuner's smallest batch, `tune::MIN_BATCH`)...
pub const FLOOR_GPU: u64 = 128;
pub const FLOOR_CPU: u64 = 16;
/// ...and a capped crew runs fewer workers rather than workers of fewer games than this each.
pub const MIN_PER_WORKER_GPU: u64 = 64;
pub const MIN_PER_WORKER_CPU: u64 = 8;

/// The cap's floor for a GPU or a CPU job.
pub fn floor(gpu: bool) -> u64 {
    if gpu {
        FLOOR_GPU
    } else {
        FLOOR_CPU
    }
}

/// The games in flight a job of `rate` completed games per second may run so that its games last about `fresh_s`
/// (Little's law), at least `floor` (and at most 8 x `rolling::MAX_INFLIGHT`: no cap in practice).
pub fn raw_cap(rate: f64, fresh_s: u64, floor: u64) -> u64 {
    let n = if rate.is_finite() && rate > 0.0 { (rate * fresh_s as f64).floor() } else { 0.0 };
    (n.min((crate::rolling::MAX_INFLIGHT * 8) as f64) as u64).max(floor)
}

/// The cap after a new reading `raw`: the first reading is taken; then only one that differs from the running cap by more
/// than `HYSTERESIS` replaces it.
pub fn next_cap(cur: Option<u64>, raw: u64) -> u64 {
    match cur {
        None => raw,
        Some(c) => {
            let (lo, hi) = (c as f64 * (1.0 - HYSTERESIS), c as f64 * (1.0 + HYSTERESIS));
            if (raw as f64) < lo || (raw as f64) > hi {
                raw
            } else {
                c
            }
        }
    }
}

/// The lease's completed-games rate over running time.
#[derive(Debug, Clone)]
pub struct Gauge {
    /// The shortest window (seconds of running time).
    window: f64,
    /// Running seconds (paused, starved and slept time left out).
    clock: f64,
    last: Option<Instant>,
    /// (running seconds, games finished) - only from once `settle_games` finished.
    samples: VecDeque<(f64, u64)>,
    /// No sample before the lease's finished games reach this (a turnover of the layout since it changed).
    settle_games: u64,
}

impl Default for Gauge {
    fn default() -> Gauge {
        Gauge::new(0, RATE_WINDOW_S)
    }
}

impl Gauge {
    /// A gauge of windows from `window` seconds of running time that measures once `turnover` games finished (the
    /// lease's first layout's games in flight).
    pub fn new(turnover: u64, window: f64) -> Gauge {
        Gauge { window: window.max(0.001), clock: 0.0, last: None, samples: VecDeque::new(), settle_games: turnover }
    }

    fn every(&self) -> f64 {
        self.window * SAMPLE_SHARE
    }

    fn longest(&self) -> f64 {
        self.window * RATE_WINDOW_MAX
    }

    /// One tick of the lease's loop: `running` - the workers play (not paused, not starved); `games` - the lease's
    /// finished games so far (every worker's).
    pub fn tick(&mut self, now: Instant, running: bool, games: u64) {
        let dt = self.last.map_or(0.0, |l| now.saturating_duration_since(l).as_secs_f64());
        self.last = Some(now);
        if !running {
            return;
        }
        if dt <= MAX_TICK_S {
            self.clock += dt;
        }
        self.sample(games);
    }

    /// The same with the running clock given (tests).
    pub fn tick_at(&mut self, clock: f64, games: u64) {
        self.clock = clock;
        self.sample(games);
    }

    fn sample(&mut self, games: u64) {
        if games < self.settle_games {
            self.samples.clear();
            return;
        }
        let (every, longest) = (self.every(), self.longest());
        if self.samples.back().is_none_or(|&(t, _)| self.clock - t >= every) {
            self.samples.push_back((self.clock, games));
        }
        while self.samples.front().is_some_and(|&(t, _)| self.clock - t > longest + every) {
            self.samples.pop_front();
        }
    }

    /// The next rate is measured from once `turnover` more games than `games` finished (the layout changed).
    pub fn settle(&mut self, games: u64, turnover: u64) {
        self.settle_games = games.saturating_add(turnover);
        self.samples.clear();
    }

    /// Completed games per running second (None: not measured yet). From the newest sample back: the first window of at
    /// least the gauge's that holds `RATE_MIN_GAMES` finished games, within `RATE_WINDOW_MAX` times it.
    pub fn rate(&self) -> Option<f64> {
        let &(t1, g1) = self.samples.back()?;
        for &(t0, g0) in self.samples.iter().rev() {
            let span = t1 - t0;
            if span > self.longest() + self.every() {
                break;
            }
            if span >= self.window && g1.saturating_sub(g0) >= RATE_MIN_GAMES {
                return Some((g1 - g0) as f64 / span);
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_cap_is_rate_times_the_budget_above_the_floor() {
        assert_eq!(raw_cap(5.6, 157, FLOOR_GPU), 879); // a CUDA crew at ~336 games a minute, legacy-meta3-r1's budget
        assert_eq!(raw_cap(0.3, 270, FLOOR_GPU), FLOOR_GPU); // a DirectML PC: 81 -> the floor
        assert_eq!(raw_cap(0.02, 270, FLOOR_CPU), FLOOR_CPU); // a CPU laptop
        assert_eq!(raw_cap(0.0, 270, FLOOR_CPU), FLOOR_CPU);
        assert_eq!(raw_cap(f64::NAN, 270, FLOOR_CPU), FLOOR_CPU);
        assert_eq!(raw_cap(1e9, 86_400, FLOOR_GPU), crate::rolling::MAX_INFLIGHT * 8);
    }

    #[test]
    fn hysteresis_keeps_the_cap_within_fifteen_percent() {
        assert_eq!(next_cap(None, 900), 900);
        assert_eq!(next_cap(Some(900), 800), 900);
        assert_eq!(next_cap(Some(900), 1000), 900);
        assert_eq!(next_cap(Some(900), 760), 760);
        assert_eq!(next_cap(Some(900), 1040), 1040);
    }

    #[test]
    fn the_rate_waits_for_a_turnover_then_a_window() {
        let mut g = Gauge::new(1000, RATE_WINDOW_S);
        // the first game length: few games finish, no rate
        for s in 0..=60 {
            g.tick_at(s as f64 * 10.0, s * 10);
        }
        assert_eq!(g.rate(), None, "before a turnover of the layout finished");
        // from 1,000 finished games on: 5 games a second
        let mut games = 1000;
        let mut t = 610.0;
        while t <= 900.0 {
            g.tick_at(t, games);
            t += 10.0;
            games += 50;
        }
        assert_eq!(g.rate(), None, "under the window");
        g.tick_at(t, games);
        assert!((g.rate().unwrap() - 5.0).abs() < 1e-9, "{:?}", g.rate());
    }

    #[test]
    fn a_slow_job_takes_a_longer_window_for_its_games() {
        let mut g = Gauge::new(0, RATE_WINDOW_S);
        // one game a minute: 5 in 300 s, 16 take 960 s
        for s in 0..=200u64 {
            g.tick_at(s as f64 * 10.0, s / 6);
        }
        let r = g.rate().expect("measured within 30 minutes");
        assert!((r - 1.0 / 60.0).abs() < 0.002, "{r}");
        // nothing finishing at all: no rate (not a rate of 0, which would cap at the floor)
        let mut idle = Gauge::new(0, RATE_WINDOW_S);
        for s in 0..=300u64 {
            idle.tick_at(s as f64 * 10.0, 7);
        }
        assert_eq!(idle.rate(), None);
    }

    #[test]
    fn paused_and_slept_time_is_not_running_time() {
        let t0 = Instant::now();
        let mut g = Gauge::new(0, RATE_WINDOW_S);
        g.tick(t0, true, 0);
        g.tick(t0 + std::time::Duration::from_secs(10), true, 10);
        g.tick(t0 + std::time::Duration::from_secs(500), false, 10); // paused 490 s
        g.tick(t0 + std::time::Duration::from_secs(510), true, 10); // (from the paused tick on: running again)
        assert!((g.clock - 20.0).abs() < 1e-9, "{}", g.clock);
        g.tick(t0 + std::time::Duration::from_secs(4000), true, 20); // a sleep: no running time
        assert!((g.clock - 20.0).abs() < 1e-9, "{}", g.clock);
    }

    #[test]
    fn settling_starts_the_measurement_again() {
        let mut g = Gauge::new(0, RATE_WINDOW_S);
        for s in 0..=40u64 {
            g.tick_at(s as f64 * 10.0, s * 20);
        }
        assert!(g.rate().is_some());
        g.settle(800, 300);
        assert_eq!(g.rate(), None);
        g.tick_at(410.0, 900);
        assert!(g.samples.is_empty(), "no sample before 1,100 finished");
    }
}

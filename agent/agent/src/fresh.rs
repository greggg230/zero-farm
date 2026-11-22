//! rev 26 (PROTOCOL 18.3): the freshness cap. A rolling lease's coordinator may send the run's freshness budget
//! `fresh_s` - how long a game may last for its first rows to reach the trainer within the run's staleness window
//! (zero-core's pool drops rows searched by a network more than `max_stale` generations behind). The tuner maximises
//! leaves/s by raising the games in flight, and every game in flight shares the worker's leaves: past some number of games
//! in flight the extra games only make every game longer, and their first rows reach the trainer too late. Measured on
//! legacy-meta3-r1, 2026-11-22 (pool `by_machine`, 100 iterations): a laptop GPU lost 72 % of its rows, a Mac 25 %, the
//! CUDA desktops 12-14 %.
//!
//! **The estimator** (agent 0.3.18). A worker searches one leaf per game in flight per batch, so its games share its
//! leaves equally, and a game ends once it has had its own number of leaves (its moves x the run's simulations) - a
//! property of the run, not of the machine's speed or layout. So a game's latency at a layout is
//!
//!   `latency = leaves per game x games in flight / leaves per second`
//!
//! and the cap that keeps it within the budget is `cap = fresh_s x leaves/s / leaves per game` (Little's law with the
//! completion rate written as leaves/s / leaves per game). Both are measured without a steady state:
//! - **leaves per game** at the quantile `QUANTILE` (p75), from each worker's first wave (`Wave`): a fresh worker starts
//!   its `conc` games at once, and each of them has had `x` = the sum over its statuses of (new leaves / games in flight)
//!   leaves - exactly, as the leaves are shared equally, also while it drains with fewer games after `finish`. When
//!   `QUANTILE` of its first wave has finished, `x` is the p75 game's leaves. A 9-minute lease's ~7-minute drain finishes
//!   its first wave, so every lease gives a sample, even one that never reaches a steady state; the samples are remembered
//!   per run, worker build, runtime and provider (`state\rolling_fresh.json`, the median of the last `LPG_SAMPLES`);
//! - **leaves per second**: the lease's workers' leaves over the last `Timings::fresh_window` (300 s) of running time
//!   while it plays new games (`Gauge`), remembered too with the games in flight it was measured at.
//!
//! A lease of a key with both remembered starts AT the capped layout (no handoff down, which ends games), and corrects the
//! cap with its own leaves/s once measured. The 0.3.17 estimator (completed games over 300 s, only after a whole turnover
//! of the layout) never measured on runs whose leases last ~9 minutes plus a drain.
//!
//! `cap = max(floor, fresh_s x leaves/s / leaves per game)`:
//! - `floor`: `FLOOR_GPU` / `FLOOR_CPU` games in flight in all, and each worker keeps at least `MIN_PER_WORKER_GPU` /
//!   `MIN_PER_WORKER_CPU` (fewer workers where the cap is lower, `tune::Space::fit`);
//! - `next_cap`: a new cap replaces the running one only when it differs by more than `HYSTERESIS`; after a change that
//!   moves the layout, the next decision waits for a new leaves/s window (`Gauge::reset`).
//!
//! No budget (an older coordinator, a machine the owner's setting leaves out, a run with too little history): no cap and
//! nothing learned, exactly as before.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::PathBuf;
use std::time::Instant;
use zerofarm_common::layout::Layout;

/// The leaves/s window (seconds of running time; `Timings::fresh_window`).
pub const RATE_WINDOW_S: f64 = 300.0;
/// A sample of the counter at most once per this share of the window (10 s of 300).
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
/// The game the cap is sized for: the p75 of the run's games by leaves (the budget's own margin, 0.75 of the window, is
/// for the spread of the window and the flush; this one for the spread of game lengths).
pub const QUANTILE: f64 = 0.75;
/// A worker's first wave counts once it has at least this many games in flight.
pub const MIN_WAVE: u64 = 16;
/// The leaves per game remembered per key: the median of the last this many samples (each a worker's first wave).
pub const LPG_SAMPLES: usize = 8;
/// The remembered leaves/s is written at most this often (seconds).
pub const SAVE_EVERY_S: u64 = 60;

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

/// The cap for a job playing `leaves_s` leaves per second whose games take `lpg` leaves (`QUANTILE`): `raw_cap` of the
/// completion rate `leaves_s / lpg`.
pub fn latency_cap(leaves_s: f64, lpg: f64, fresh_s: u64, floor: u64) -> u64 {
    let rate = if lpg.is_finite() && lpg > 0.0 { leaves_s / lpg } else { 0.0 };
    raw_cap(rate, fresh_s, floor)
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

/// A counter's rate over running time (the lease's leaves).
#[derive(Debug, Clone)]
pub struct Gauge {
    /// The window (seconds of running time).
    window: f64,
    /// Running seconds (paused, starved and slept time left out).
    clock: f64,
    last: Option<Instant>,
    /// (running seconds, counter).
    samples: VecDeque<(f64, u64)>,
}

impl Default for Gauge {
    fn default() -> Gauge {
        Gauge::new(RATE_WINDOW_S)
    }
}

impl Gauge {
    pub fn new(window: f64) -> Gauge {
        Gauge { window: window.max(0.001), clock: 0.0, last: None, samples: VecDeque::new() }
    }

    fn every(&self) -> f64 {
        self.window * SAMPLE_SHARE
    }

    /// One tick of the lease's loop: `running` - the workers play new games (not paused, not starved, not finishing);
    /// `count` - the counter so far.
    pub fn tick(&mut self, now: Instant, running: bool, count: u64) {
        let dt = self.last.map_or(0.0, |l| now.saturating_duration_since(l).as_secs_f64());
        self.last = Some(now);
        if !running {
            return;
        }
        if dt <= MAX_TICK_S {
            self.clock += dt;
        }
        self.sample(count);
    }

    /// The same with the running clock given (tests).
    pub fn tick_at(&mut self, clock: f64, count: u64) {
        self.clock = clock;
        self.sample(count);
    }

    fn sample(&mut self, count: u64) {
        let every = self.every();
        if self.samples.back().is_none_or(|&(t, _)| self.clock - t >= every) {
            self.samples.push_back((self.clock, count));
        }
        while self.samples.len() > 1 && self.samples.get(1).is_some_and(|&(t, _)| self.clock - t >= self.window) {
            self.samples.pop_front();
        }
    }

    /// The next rate is measured afresh (the layout changed).
    pub fn reset(&mut self) {
        self.samples.clear();
    }

    /// The counter's rate per running second over the last window (None: not a whole window measured yet).
    pub fn rate(&self) -> Option<f64> {
        let (&(t0, c0), &(t1, c1)) = (self.samples.front()?, self.samples.back()?);
        let span = t1 - t0;
        (span >= self.window && c1 > c0).then(|| (c1 - c0) as f64 / span)
    }
}

/// A worker's first wave: the leaves each of its first `conc` games has had (`x`), until `QUANTILE` of them finished.
#[derive(Debug, Clone)]
pub struct Wave {
    conc: u64,
    /// Its last status's (leaves, games finished, games started).
    last: Option<(u64, u64, u64)>,
    x: f64,
    done: bool,
}

impl Wave {
    pub fn new(conc: u64) -> Wave {
        Wave { conc, last: None, x: 0.0, done: conc < MIN_WAVE }
    }

    /// A status of the worker (its own counters from its start). -> (new leaves since its last status, a leaves-per-game
    /// sample once `QUANTILE` of its first wave finished).
    pub fn status(&mut self, leaves: u64, games: u64, started: u64) -> (u64, Option<f64>) {
        let new = match self.last {
            Some((l0, g0, s0)) => {
                let d = leaves.saturating_sub(l0);
                // (the games in flight over the interval: as at its start; each had an equal share of the new leaves)
                let alive = s0.saturating_sub(g0).max(1);
                if !self.done {
                    self.x += d as f64 / alive as f64;
                }
                d
            }
            None => leaves,
        };
        if self.last.is_none() && !self.done {
            // (its first status: the leaves so far shared by the games it has in flight)
            self.x = leaves as f64 / started.saturating_sub(games).max(1) as f64;
        }
        self.last = Some((leaves, games, started));
        if !self.done && games as f64 >= QUANTILE * self.conc as f64 {
            self.done = true;
            return (new, (self.x > 0.0).then_some(self.x));
        }
        (new, None)
    }
}

/// A lease's freshness measurements: its workers' first waves (by pid), its leaves, their rate.
#[derive(Debug, Clone, Default)]
pub struct Meter {
    waves: HashMap<u32, Wave>,
    /// Every worker's leaves since the lease started (their statuses' increments).
    pub leaves: u64,
    pub gauge: Gauge,
    saved: Option<Instant>,
}

impl Meter {
    pub fn new(window: f64) -> Meter {
        Meter { gauge: Gauge::new(window), ..Meter::default() }
    }

    /// A status of worker `pid` of `conc` games in flight. -> a leaves-per-game sample (its first wave's `QUANTILE`).
    pub fn status(&mut self, pid: u32, conc: u64, leaves: u64, games: u64, started: u64) -> Option<f64> {
        if self.waves.len() > 64 {
            self.waves.retain(|_, w| !w.done);
        }
        let w = self.waves.entry(pid).or_insert_with(|| Wave::new(conc));
        let (new, sample) = w.status(leaves, games, started);
        self.leaves += new;
        sample
    }

    /// The remembered leaves/s is due to be written (at most every `SAVE_EVERY_S`).
    pub fn save_due(&mut self) -> bool {
        if self.saved.is_some_and(|t| t.elapsed().as_secs() < SAVE_EVERY_S) {
            return false;
        }
        self.saved = Some(Instant::now());
        true
    }
}

/// What a key remembers: leaves per game samples (`QUANTILE`, each a worker's first wave), and the leaves/s last measured
/// with the games in flight it was measured at.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Entry {
    #[serde(default)]
    pub lpg: Vec<f64>,
    #[serde(default)]
    pub leaves_s: Option<f64>,
    #[serde(default)]
    pub inflight: Option<u64>,
    #[serde(default)]
    pub t: i64,
}

impl Entry {
    /// The leaves per game planned with: the median of the samples.
    pub fn lpg(&self) -> Option<f64> {
        let mut v: Vec<f64> = self.lpg.iter().copied().filter(|x| x.is_finite() && *x > 0.0).collect();
        if v.is_empty() {
            return None;
        }
        v.sort_by(|a, b| a.total_cmp(b));
        let m = v.len() / 2;
        Some(if v.len() % 2 == 1 { v[m] } else { (v[m - 1] + v[m]) / 2.0 })
    }

    /// The cap a lease of this key starts with under `fresh_s` (None: not both measured yet).
    pub fn start_cap(&self, fresh_s: u64, floor: u64) -> Option<u64> {
        Some(latency_cap(self.leaves_s.filter(|r| *r > 0.0)?, self.lpg()?, fresh_s, floor))
    }
}

/// `state\rolling_fresh.json`: per key `<run>/<build_id>/<runtime build_id or ->/<provider>`.
pub struct Learned;

impl Learned {
    fn path(layout: &Layout) -> PathBuf {
        layout.state().join("rolling_fresh.json")
    }

    pub fn key(run: &str, build: &str, runtime: Option<&str>, provider: &str) -> String {
        format!("{run}/{build}/{}/{provider}", runtime.unwrap_or("-"))
    }

    pub fn load(layout: &Layout) -> BTreeMap<String, Entry> {
        std::fs::read(Self::path(layout)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    pub fn get(layout: &Layout, key: &str) -> Option<Entry> {
        Self::load(layout).remove(key)
    }

    fn update(layout: &Layout, key: &str, f: impl FnOnce(&mut Entry)) -> Entry {
        let mut m = Self::load(layout);
        let e = m.entry(key.to_string()).or_default();
        f(e);
        e.t = zerofarm_common::now_unix();
        let out = e.clone();
        // (at most 64 keys: the least recently changed go)
        while m.len() > 64 {
            let Some(k) = m.iter().min_by_key(|(_, e)| e.t).map(|(k, _)| k.clone()) else { break };
            m.remove(&k);
        }
        let _ = std::fs::create_dir_all(layout.state());
        if let Err(e) = zerofarm_common::fsutil::atomic_write(&Self::path(layout), &serde_json::to_vec(&m).expect("json")) {
            log::warn!("could not save {}: {e}", Self::path(layout).display());
        }
        out
    }

    /// A worker's leaves-per-game sample. -> the key's entry now.
    pub fn sample(layout: &Layout, key: &str, lpg: f64) -> Entry {
        Self::update(layout, key, |e| {
            e.lpg.push(lpg);
            let n = e.lpg.len();
            if n > LPG_SAMPLES {
                e.lpg.drain(..n - LPG_SAMPLES);
            }
        })
    }

    /// The leaves/s measured at `inflight` games in flight.
    pub fn rate(layout: &Layout, key: &str, leaves_s: f64, inflight: u64) -> Entry {
        Self::update(layout, key, |e| {
            e.leaves_s = Some(leaves_s);
            e.inflight = Some(inflight);
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_cap_is_rate_times_the_budget_above_the_floor() {
        assert_eq!(raw_cap(5.6, 157, FLOOR_GPU), 879);
        assert_eq!(raw_cap(0.3, 270, FLOOR_GPU), FLOOR_GPU);
        assert_eq!(raw_cap(0.0, 270, FLOOR_CPU), FLOOR_CPU);
        assert_eq!(raw_cap(f64::NAN, 270, FLOOR_CPU), FLOOR_CPU);
        assert_eq!(raw_cap(1e9, 86_400, FLOOR_GPU), crate::rolling::MAX_INFLIGHT * 8);
        // leaves: 25,000 leaves/s, 20,000 leaves a game (p75) - 1.25 games/s, x 303 s = 378
        assert_eq!(latency_cap(25_000.0, 20_000.0, 303, FLOOR_GPU), 378);
        assert_eq!(latency_cap(25_000.0, 0.0, 303, FLOOR_GPU), FLOOR_GPU);
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
    fn the_gauge_measures_a_whole_window_of_running_time() {
        let mut g = Gauge::new(300.0);
        for s in 0..30u64 {
            g.tick_at(s as f64 * 10.0, s * 1000);
        }
        assert_eq!(g.rate(), None, "290 s");
        g.tick_at(300.0, 30_000);
        assert!((g.rate().unwrap() - 100.0).abs() < 1e-9);
        for s in 31..90u64 {
            g.tick_at(s as f64 * 10.0, 30_000 + (s - 30) * 500); // slower: 50 a second
        }
        assert!((g.rate().unwrap() - 50.0).abs() < 1e-9, "{:?}", g.rate());
        g.reset();
        assert_eq!(g.rate(), None);
    }

    #[test]
    fn paused_and_slept_time_is_not_running_time() {
        let t0 = Instant::now();
        let mut g = Gauge::new(300.0);
        g.tick(t0, true, 0);
        g.tick(t0 + std::time::Duration::from_secs(10), true, 10);
        g.tick(t0 + std::time::Duration::from_secs(500), false, 10);
        g.tick(t0 + std::time::Duration::from_secs(510), true, 10);
        assert!((g.clock - 20.0).abs() < 1e-9, "{}", g.clock);
        g.tick(t0 + std::time::Duration::from_secs(4000), true, 20);
        assert!((g.clock - 20.0).abs() < 1e-9, "{}", g.clock);
    }

    /// A simulated worker: `conc` games started at once, each needing its own leaves (`need`), sharing `rate` leaves a
    /// second equally; a finished game is replaced by a new one while `rolling`, then (after `finish_at` s) not - the
    /// drain. Statuses every `dt` s. -> the wave's sample and when it came.
    fn simulate(conc: usize, need: &dyn Fn(usize) -> f64, rate: f64, finish_at: f64, dt: f64) -> Option<(f64, f64)> {
        let mut games: Vec<(usize, f64)> = (0..conc).map(|i| (i, 0.0)).collect(); // (id, leaves had)
        let mut next = conc;
        let (mut leaves, mut finished, mut started) = (0u64, 0u64, conc as u64);
        let mut acc = 0.0;
        let mut w = Wave::new(conc as u64);
        let mut t = 0.0;
        // (fine steps: games finish between statuses)
        let step = dt / 50.0;
        while t < 100_000.0 && !games.is_empty() {
            for _ in 0..50 {
                if games.is_empty() {
                    break;
                }
                let share = rate * step / games.len() as f64;
                acc += rate * step;
                let mut i = 0;
                while i < games.len() {
                    games[i].1 += share;
                    if games[i].1 >= need(games[i].0) {
                        games.swap_remove(i);
                        finished += 1;
                        if t < finish_at {
                            games.push((next, 0.0));
                            next += 1;
                            started += 1;
                        }
                    } else {
                        i += 1;
                    }
                }
                t += step;
            }
            leaves = acc as u64;
            if let (_, Some(x)) = w.status(leaves, finished, started) {
                return Some((x, t));
            }
        }
        let _ = leaves;
        None
    }

    #[test]
    fn a_first_wave_gives_the_p75_games_leaves_also_through_a_drain() {
        // games of 10,000 to 30,000 leaves (uniform by id): p75 = 25,000
        let need = |i: usize| 10_000.0 + 20_000.0 * ((i * 7919) % 1000) as f64 / 1000.0;
        // a long lease: the wave's p75 while it runs
        let (x, t) = simulate(1000, &need, 50_000.0, f64::INFINITY, 10.0).expect("a sample");
        assert!((x - 25_000.0).abs() < 600.0, "{x}");
        assert!((t - 25_000.0 * 1000.0 / 50_000.0).abs() < 30.0, "at {t} s");
        // a short lease: finished after 200 s (about half the wave's p75 time), the rest drained: the same p75
        let (x, t) = simulate(1000, &need, 50_000.0, 200.0, 10.0).expect("a sample in the drain");
        assert!((x - 25_000.0).abs() < 600.0, "{x}");
        assert!(t < 500.0 && t > 200.0, "in the drain: {t}");
        // a tiny worker: no sample
        assert!(simulate(8, &need, 50_000.0, f64::INFINITY, 10.0).is_none());
    }

    #[test]
    fn entries_remember_the_median_and_a_start_cap() {
        let dir = tempfile::tempdir().unwrap();
        let layout = Layout::new(dir.path());
        let k = Learned::key("r", "b", None, "cuda");
        assert_eq!(k, "r/b/-/cuda");
        assert_eq!(Learned::get(&layout, &k), None);
        for x in [20_000.0, 22_000.0, 90_000.0] {
            Learned::sample(&layout, &k, x);
        }
        let e = Learned::get(&layout, &k).unwrap();
        assert_eq!(e.lpg(), Some(22_000.0), "the median, an outlier aside");
        assert_eq!(e.start_cap(303, FLOOR_GPU), None, "no leaves/s yet");
        let e = Learned::rate(&layout, &k, 25_000.0, 2304);
        assert_eq!(e.start_cap(303, FLOOR_GPU), Some(344)); // 25,000 / 22,000 x 303
        for i in 0..10 {
            Learned::sample(&layout, &k, 1000.0 + i as f64);
        }
        assert_eq!(Learned::get(&layout, &k).unwrap().lpg.len(), LPG_SAMPLES);
        // another key is its own
        assert_eq!(Learned::get(&layout, &Learned::key("r", "b", Some("rt"), "cuda")), None);
    }
}

//! On-device layout autotuning (rev 19, PROTOCOL 13): which CPU layout a rolling self-play job runs with - search
//! threads, inference sessions x threads per session, groups (the batch size) and games in flight - measured on this
//! machine instead of fixed rules. Everything here is shared by both platforms; the only per-platform part is the
//! starting point (`defaults`). lease.rs runs the trials (layout handoffs inside real rolling leases) and feeds the
//! measurements in; this module decides what to try next, what wins and what is remembered (`state/tuning.json`).
//!
//! **What is measured.** Leaves per second (the worker's `leaves` counter) of a worker that starts fresh, over a
//! window placed at the same *game progress* for every trial: from `p0` to `p1` leaves per game in flight. A fresh
//! rolling worker starts all its games at once and its rate falls as they grow (rolling_measure_2026-11-16: a CPU
//! worker at 32 in flight 855 leaves/s in its first 20 s, 520 at 7 min, 420 at 18 min - still falling after more than a
//! game length), so a window at a fixed wall time would favour more games in flight (their games are younger at the
//! same time). Compared at the same leaves per game the ratios between layouts match the steady state's: 32 vs 16 in
//! flight 1.32 (steady 1.30), 64 vs 32 1.20 (steady 1.23); at 30-120 s of wall time they read 1.42 and 1.26. The
//! first measurement of a search fixes the window: its worker's games in flight refilled, `warm` more (the first
//! status interval is the only startup transient in that data), then `window` seconds; later trials use the same
//! `[p0, p1]`.
//!
//! **The search.** From the starting point (a coordinator hint for this hardware class, else the platform's default)
//! a coordinate search: for each coordinate in turn (batch size, the split of the thread budget between search and
//! inference, sessions, games in flight) the incumbent's two neighbours, moving to one that is at least `GAIN` faster
//! (and so on from there), at most `MAX_TRIALS` trials; then the incumbent is measured once more and kept only if its
//! mean is still `GAIN` over the start's (a lucky single measurement doesn't stick). Search threads plus inference
//! threads never exceed the thread budget (the M4 Max, 2026-11-18: 24 + 16 threads ran at 0.7x of 16 + 15, 36 + 36 at
//! 0.3x: oversubscribing collapses the native worker), so the budget is a constraint, never a dimension.
//!
//! **Remembered** per (worker build, runtime pack, network fingerprint, hardware, OS, tuner version); a key not seen
//! is searched again; a tuned key is re-checked weekly (the runner-up against the incumbent, both measured afresh).

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::path::PathBuf;
use zerofarm_common::layout::Layout as Paths;

/// The `features` this agent asks `/v1/lease` with for tuning (PROTOCOL 13: it takes `tuning_hint`).
pub const FEATURES: [&str; 1] = ["tuning"];
/// A worker whose `hello` lists this honours the job line's `infer_workers` (macOS builds always do: `Defaults`).
pub const WORKER_FEATURE: &str = "infer_workers";
/// Bumped when the search or the measurement changes: every key is searched again.
pub const TUNER_VERSION: u32 = 1;
/// A layout replaces the incumbent only when it is at least this much faster (noise guard).
pub const GAIN: f64 = 1.03;
/// Candidate trials per search at most (the final re-measure of the incumbent comes on top).
pub const MAX_TRIALS: u32 = 9;
/// A candidate discarded this often (the PC was used, went on battery, got busy, ... during its trial) is skipped.
pub const MAX_DISCARDS: u32 = 3;
/// A tuned key is re-checked (runner-up vs incumbent) this long after it was tuned.
pub const RECHECK_S: i64 = 7 * 86_400;
/// Keys remembered at most (the least recently used go first).
pub const MAX_ENTRIES: usize = 16;
/// Layouts reported to the coordinator per lease request at most.
pub const MAX_REPORTS: usize = 4;
/// Other programs may use at most this share of the CPUs while a trial starts and runs.
pub const OTHER_LOAD_MAX: f64 = 0.10;
/// A batch has at least this many rows (fewer groups than that would make).
pub const MIN_BATCH: u64 = 8;
/// A trial's games in flight count as refilled at this share of its `conc`.
pub const REFILL_SHARE: f64 = 0.98;
/// Bounds of a layout's numbers (a hint is refused outside them).
pub const MAX_THREADS: u32 = 512;
pub const MAX_SESSIONS: u32 = 64;

/// A CPU job's layout: the job line's `threads` (search), `infer_workers` (ONNX Runtime sessions, each on an evaluator
/// thread of its own), `infer_threads` (threads per session), `groups` (batches in turn: rows per batch = conc /
/// groups) and `conc` (games in flight).
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Layout {
    pub threads: u32,
    pub infer_workers: u32,
    pub infer_threads: u32,
    pub groups: u64,
    pub conc: u64,
}

impl Layout {
    pub fn infer_total(&self) -> u32 {
        self.infer_workers * self.infer_threads
    }
    pub fn total_threads(&self) -> u32 {
        self.threads + self.infer_total()
    }
    /// Rows per batch (one leaf per game in flight per batch, `groups` batches in turn).
    pub fn batch(&self) -> u64 {
        self.conc.div_ceil(self.groups.max(1))
    }
    /// "8 search + 4 x 2 inference threads, 2,304 games in flight in 6 groups (384 rows)".
    pub fn describe(&self) -> String {
        format!(
            "{} search + {} x {} inference threads, {} games in flight in {} groups ({} rows)",
            self.threads,
            self.infer_workers,
            self.infer_threads,
            thousands(self.conc),
            self.groups,
            self.batch()
        )
    }
    pub fn to_json(&self) -> Value {
        serde_json::to_value(self).expect("json")
    }
    /// A layout from JSON (a coordinator hint): every field present and within bounds, else None.
    pub fn from_json(v: &Value) -> Option<Layout> {
        let n = |k: &str, max: u64| v.get(k).and_then(Value::as_u64).filter(|x| (1..=max).contains(x));
        let l = Layout {
            threads: n("threads", MAX_THREADS as u64)? as u32,
            infer_workers: n("infer_workers", MAX_SESSIONS as u64)? as u32,
            infer_threads: n("infer_threads", MAX_THREADS as u64)? as u32,
            groups: n("groups", crate::rolling::MAX_INFLIGHT)?,
            conc: n("conc", crate::rolling::MAX_INFLIGHT)?,
        };
        (l.groups <= l.conc && l.total_threads() <= 2 * MAX_THREADS).then_some(l)
    }
}

pub fn thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// The machine's CPU as the starting point sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hw {
    /// Physical cores (performance + efficiency).
    pub cores: u32,
    /// Logical processors (SMT threads; on Apple silicon the cores).
    pub logical: u32,
    pub perf: u32,
    pub eff: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Platform {
    Windows,
    MacOs,
}

impl Platform {
    pub fn current() -> Platform {
        if cfg!(target_os = "macos") {
            Platform::MacOs
        } else {
            Platform::Windows
        }
    }
}

/// The starting point per platform (the only per-platform part of tuning).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Defaults {
    /// Search threads + inference threads at most (the owner's `cpu_threads` replaces it).
    pub budget: u32,
    /// The start's search threads, sessions and threads per session.
    pub threads: u32,
    pub infer_workers: u32,
    pub infer_threads: u32,
    /// The start's rows per batch: `groups` = conc / batch (None: the groups of rolling.rs's "auto" as they are).
    pub batch: Option<u64>,
    /// Every worker build of the platform honours `infer_workers` (else only one whose `hello` lists the feature).
    pub sessions_always: bool,
}

/// The starting point for `hw` on `p`. Windows: today's layout (one search thread per physical core, one session with
/// the rest of the logical threads - the worker's own default, `infer_threads` = `threads`, on an SMT CPU), budget the
/// logical threads. macOS (Apple silicon, no SMT): budget the cores, split between search and inference (the shipped
/// 16 + 5 x 3 threads ran at 61-69 % machine load and was inference-bound on the M4 Max, 2026-11-18).
pub fn defaults(p: Platform, hw: &Hw) -> Defaults {
    let cores = hw.cores.max(1);
    let logical = hw.logical.max(cores);
    match p {
        Platform::Windows => {
            let (threads, infer) = if logical > cores { (cores, logical - cores) } else { (cores - cores / 2, (cores / 2).max(1)) };
            Defaults { budget: logical.max(2), threads: threads.max(1), infer_workers: 1, infer_threads: infer.max(1), batch: None, sessions_always: false }
        }
        Platform::MacOs => {
            // half the cores search, the other half answers batches in sessions of 2 threads
            let budget = cores.max(2);
            let infer = (budget / 2).max(1);
            let workers = (infer / 2).max(1);
            Defaults {
                budget,
                threads: (budget - workers * (infer / workers)).max(1),
                infer_workers: workers,
                infer_threads: infer / workers,
                batch: Some(crate::rolling::MACOS_CPU_BATCH * 2),
                sessions_always: true,
            }
        }
    }
}

/// What a search may vary for one job.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Space {
    /// Search + inference threads at most.
    pub budget: u32,
    /// The worker honours `infer_workers` (else one session).
    pub sessions: bool,
    /// Games in flight within these bounds (equal: the run's number, fixed).
    pub conc_min: u64,
    pub conc_max: u64,
}

impl Space {
    /// A layout made valid: at least 1 of everything, one session without the feature, the threads within the budget
    /// (a thread at a time from the larger of search and inference; inference gives threads per session before
    /// sessions), the games in flight within bounds, at least `MIN_BATCH` rows per batch where the games in flight
    /// allow.
    pub fn fit(&self, l: Layout) -> Layout {
        let budget = self.budget.max(2);
        let mut l = Layout {
            threads: l.threads.clamp(1, MAX_THREADS),
            infer_workers: if self.sessions { l.infer_workers.clamp(1, MAX_SESSIONS) } else { 1 },
            infer_threads: l.infer_threads.clamp(1, MAX_THREADS),
            groups: l.groups.max(1),
            conc: l.conc.clamp(self.conc_min.max(1), self.conc_max.max(self.conc_min).max(1)),
        };
        while l.total_threads() > budget {
            if l.threads > 1 && l.threads >= l.infer_total() {
                l.threads -= 1;
            } else if l.infer_threads > 1 {
                l.infer_threads -= 1;
            } else if l.infer_workers > 1 {
                l.infer_workers -= 1;
            } else {
                l.threads = l.threads.saturating_sub(1).max(1);
                break;
            }
        }
        l.groups = l.groups.min((l.conc / MIN_BATCH).max(1)).min(l.conc);
        l
    }
}

/// The coordinates of the search, in the order they are tried.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Coord {
    /// Rows per batch (groups halved or doubled).
    Batch,
    /// Threads moved between search and inference (the total kept).
    Split,
    /// Sessions doubled or halved (their threads kept).
    Sessions,
    /// Games in flight doubled or halved (rows per batch kept).
    Conc,
}

pub const COORDS: [Coord; 4] = [Coord::Batch, Coord::Split, Coord::Sessions, Coord::Conc];

/// `l`'s neighbour along `c` (`up`: bigger batches, more inference threads, more sessions, more games in flight), made
/// valid; None when there is none (a bound, or the same layout).
pub fn neighbour(l: &Layout, c: Coord, up: bool, s: &Space) -> Option<Layout> {
    let mut n = *l;
    match c {
        Coord::Batch => n.groups = if up { (l.groups / 2).max(1) } else { l.groups * 2 },
        Coord::Split => {
            // about an eighth of the budget, in whole threads per session
            let total = l.total_threads().min(s.budget.max(2));
            let per = ((s.budget / 8).max(1) / l.infer_workers.max(1)).max(1);
            let t = if up { l.infer_threads + per } else { l.infer_threads.checked_sub(per).filter(|&t| t >= 1)? };
            let infer = t * l.infer_workers;
            if infer >= total {
                return None;
            }
            n.infer_threads = t;
            n.threads = total - infer;
        }
        Coord::Sessions => {
            if !s.sessions {
                return None;
            }
            if up {
                if l.infer_threads < 2 {
                    return None;
                }
                n.infer_workers = l.infer_workers * 2;
                n.infer_threads = l.infer_threads / 2;
            } else {
                if l.infer_workers < 2 {
                    return None;
                }
                n.infer_workers = l.infer_workers / 2;
                n.infer_threads = l.infer_threads * 2;
            }
        }
        Coord::Conc => {
            if s.conc_min >= s.conc_max {
                return None;
            }
            let batch = l.batch();
            n.conc = if up { l.conc * 2 } else { l.conc / 2 };
            n.conc = n.conc.clamp(s.conc_min.max(1), s.conc_max);
            n.groups = n.conc.div_ceil(batch.max(1)).max(1);
        }
    }
    let n = s.fit(n);
    (n != s.fit(*l)).then_some(n)
}

/// The window trials are compared over: leaves per game in flight, from `p0` to `p1` (the search's first measurement).
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
pub struct Phase {
    pub p0: f64,
    pub p1: f64,
    /// The games in flight of the measurement that set it (a trial with more takes longer to cover it).
    pub conc: u64,
}

/// What a tuning result is remembered by.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Key {
    pub build: String,
    pub runtime: Option<String>,
    /// The network and its settings (`net_fingerprint`).
    pub net: String,
    /// The CPU (`hw_fingerprint`).
    pub hw: String,
    pub os: String,
    pub tuner: u32,
}

/// The network's fingerprint: the job template the worker plays (its settings: search, network and game options; the
/// per-lease fields left out) and the model file's size in MiB (the network's shape; its generations keep it). A new
/// generation of the same run keeps it; another architecture or other settings change it.
pub fn net_fingerprint(template: &Map<String, Value>, model_size: Option<u64>) -> String {
    let mut t = template.clone();
    for k in ["gid_base", "games", "seed", "first", "count"] {
        t.remove(k);
    }
    let text = format!("{}|{}", zerofarm_common::protocol::canonical_json(&Value::Object(t)), model_size.map_or(-1, |s| (s >> 20) as i64));
    zerofarm_common::hash::sha256_hex(text.as_bytes())[..16].to_string()
}

/// The CPU's fingerprint: brand, cores, threads, performance and efficiency cores, RAM in GiB.
pub fn hw_fingerprint(p: &zerofarm_common::protocol::Profile) -> String {
    format!("{} | {}c {}t {}p {}e | {} GB", p.cpu, p.cores, p.threads, p.perf_cores, p.eff_cores, p.ram_mb >> 10)
}

/// One valid measurement: a fresh worker of `layout`, `rate` leaves/s over the search's phase window.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Measured {
    pub layout: Layout,
    pub rate: f64,
    pub t: i64,
    /// The weekly check it belongs to (0: the search).
    #[serde(default)]
    pub epoch: u32,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum State {
    /// Trying candidates around the incumbent.
    Searching,
    /// Re-measuring the incumbent before it is kept.
    Final,
    Tuned,
    /// The weekly check: runner-up and incumbent measured afresh.
    Checking,
}

/// One key's search and result.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Entry {
    pub key: Key,
    /// The run it was measured with (reported; not part of the key).
    pub run: String,
    pub start: Layout,
    pub incumbent: Layout,
    pub measured: Vec<Measured>,
    /// Discarded trials per layout.
    #[serde(default)]
    pub discards: Vec<(Layout, u32)>,
    pub phase: Option<Phase>,
    /// Candidate trials measured so far in the search.
    pub trials: u32,
    pub state: State,
    #[serde(default)]
    pub epoch: u32,
    pub tuned_t: Option<i64>,
    pub check_after: Option<i64>,
    /// The start came from the coordinator (`tuning_hint`).
    #[serde(default)]
    pub hint: bool,
    /// The move that made the incumbent (coordinate index, up): tried again first.
    #[serde(default)]
    pub momentum: Option<(usize, bool)>,
    /// Last change (least recently used keys are forgotten first).
    pub t: i64,
}

impl Entry {
    pub fn new(key: Key, run: &str, start: Layout, hint: bool, now: i64) -> Entry {
        Entry {
            key,
            run: run.to_string(),
            start,
            incumbent: start,
            measured: vec![],
            discards: vec![],
            phase: None,
            trials: 0,
            state: State::Searching,
            epoch: 0,
            tuned_t: None,
            check_after: None,
            hint,
            momentum: None,
            t: now,
        }
    }

    fn rates(&self, l: &Layout, epoch: Option<u32>) -> Vec<f64> {
        self.measured.iter().filter(|m| m.layout == *l && epoch.is_none_or(|e| m.epoch == e)).map(|m| m.rate).collect()
    }

    /// The mean rate of `l` (this check's measurements only during a check).
    pub fn mean(&self, l: &Layout) -> Option<f64> {
        let r = self.rates(l, (self.state == State::Checking).then_some(self.epoch));
        (!r.is_empty()).then(|| r.iter().sum::<f64>() / r.len() as f64)
    }

    fn mean_all(&self, l: &Layout) -> Option<f64> {
        let r = self.rates(l, None);
        (!r.is_empty()).then(|| r.iter().sum::<f64>() / r.len() as f64)
    }

    fn discarded(&self, l: &Layout) -> u32 {
        self.discards.iter().find(|(x, _)| x == l).map_or(0, |(_, n)| *n)
    }

    /// The best layout measured other than the incumbent (all measurements).
    pub fn runner_up(&self) -> Option<Layout> {
        let mut best: Option<(Layout, f64)> = None;
        for m in &self.measured {
            if m.layout == self.incumbent || best.is_some_and(|(l, _)| l == m.layout) {
                continue;
            }
            let r = self.mean_all(&m.layout).unwrap_or(0.0);
            if best.is_none_or(|(_, b)| r > b) {
                best = Some((m.layout, r));
            }
        }
        best.map(|(l, _)| l)
    }

    /// The next candidate of the search: the first unmeasured neighbour of the incumbent - the move that made it
    /// first (momentum), then the coordinates in order.
    fn next_candidate(&self, s: &Space) -> Option<Layout> {
        if self.trials >= MAX_TRIALS {
            return None;
        }
        let inc = s.fit(self.incumbent);
        let order = self.momentum.into_iter().chain(COORDS.iter().enumerate().flat_map(|(i, _)| [(i, true), (i, false)]));
        for (ci, up) in order {
            let Some(&c) = COORDS.get(ci) else { continue };
            let Some(n) = neighbour(&inc, c, up, s) else { continue };
            if self.rates(&n, Some(0)).is_empty() && self.discarded(&n) < MAX_DISCARDS {
                return Some(n);
            }
        }
        None
    }

    /// The layout to measure next (None: nothing to measure now - tuned, or a check not due).
    pub fn plan(&self, s: &Space) -> Option<Layout> {
        match self.state {
            State::Searching => {
                if self.rates(&self.incumbent, Some(0)).is_empty() && self.discarded(&self.incumbent) < MAX_DISCARDS * 2 {
                    return Some(s.fit(self.incumbent));
                }
                self.next_candidate(s)
            }
            State::Final => Some(s.fit(self.incumbent)),
            State::Tuned => None,
            State::Checking => {
                let fresh = |l: &Layout| !self.rates(l, Some(self.epoch)).is_empty();
                match self.runner_up() {
                    Some(r) if !fresh(&r) && self.discarded(&r) < MAX_DISCARDS => Some(s.fit(r)),
                    _ if !fresh(&self.incumbent) => Some(s.fit(self.incumbent)),
                    _ => None,
                }
            }
        }
    }

    /// The layout the main worker runs between trials (the incumbent).
    pub fn wanted(&self, s: &Space) -> Layout {
        s.fit(self.incumbent)
    }

    /// A valid measurement of `l`.
    pub fn record(&mut self, l: Layout, rate: f64, phase: Option<Phase>, s: &Space, now: i64) {
        if self.phase.is_none() {
            self.phase = phase;
        }
        let epoch = if self.state == State::Checking { self.epoch } else { 0 };
        self.measured.push(Measured { layout: l, rate, t: now, epoch });
        self.t = now;
        if self.state == State::Searching && l != self.incumbent {
            self.trials += 1;
            if let (Some(c), Some(i)) = (self.mean(&l), self.mean(&self.incumbent)) {
                if c >= GAIN * i {
                    log::info!("tuning: {} is {:.1} % faster than {}: the new incumbent", l.describe(), (c / i - 1.0) * 100.0, self.incumbent.describe());
                    let old = s.fit(self.incumbent);
                    self.momentum = COORDS.iter().enumerate().flat_map(|(ci, &cd)| [(ci, cd, true), (ci, cd, false)]).find(|&(_, cd, up)| neighbour(&old, cd, up, s) == Some(l)).map(|(ci, _, up)| (ci, up));
                    self.incumbent = l;
                }
            }
        }
        self.advance(s, now);
    }

    /// A trial that didn't count (it is tried again later, at most `MAX_DISCARDS` times).
    pub fn discard(&mut self, l: Layout, s: &Space, now: i64) {
        match self.discards.iter_mut().find(|(x, _)| *x == l) {
            Some((_, n)) => *n += 1,
            None => self.discards.push((l, 1)),
        }
        self.t = now;
        self.advance(s, now);
    }

    /// State changes once the current step has nothing left to measure.
    pub fn advance(&mut self, s: &Space, now: i64) {
        loop {
            match self.state {
                State::Searching if self.plan(s).is_none() => {
                    self.state = if self.incumbent != self.start && self.rates(&self.incumbent, Some(0)).len() < 2 { State::Final } else { State::Tuned };
                    if self.state == State::Tuned {
                        self.conclude(now);
                    }
                }
                State::Final if self.rates(&self.incumbent, Some(0)).len() >= 2 || self.discarded(&self.incumbent) >= MAX_DISCARDS => {
                    self.state = State::Tuned;
                    self.conclude(now);
                }
                State::Checking if self.plan(s).is_none() => {
                    if let Some(r) = self.runner_up() {
                        let (a, b) = (self.rates(&r, Some(self.epoch)), self.rates(&self.incumbent, Some(self.epoch)));
                        if let (Some(&a), Some(&b)) = (a.last(), b.last()) {
                            if a >= GAIN * b {
                                log::info!("tuning: the weekly check found {} {:.1} % faster than {}", r.describe(), (a / b - 1.0) * 100.0, self.incumbent.describe());
                                self.incumbent = r;
                            }
                        }
                    }
                    self.state = State::Tuned;
                    self.tuned_t = Some(now);
                    self.check_after = Some(now + RECHECK_S);
                }
                _ => return,
            }
        }
    }

    /// The search's end: the incumbent stays only if its mean is still `GAIN` over the start's.
    fn conclude(&mut self, now: i64) {
        if self.incumbent != self.start {
            if let (Some(i), Some(st)) = (self.mean_all(&self.incumbent), self.mean_all(&self.start)) {
                if i < GAIN * st {
                    log::info!("tuning: {} re-measured at only {:.1} % over the start: keeping the start", self.incumbent.describe(), (i / st - 1.0) * 100.0);
                    self.incumbent = self.start;
                }
            }
        }
        self.tuned_t = Some(now);
        self.check_after = Some(now + RECHECK_S);
    }

    /// Starts the weekly check when it is due (and there is a runner-up to check). True when it started.
    pub fn maybe_check(&mut self, now: i64) -> bool {
        if self.state != State::Tuned || self.check_after.is_none_or(|t| now < t) {
            return false;
        }
        if self.runner_up().is_none() {
            self.check_after = Some(now + RECHECK_S);
            return false;
        }
        self.state = State::Checking;
        self.epoch += 1;
        self.t = now;
        true
    }

    /// The incumbent's mean rate (all measurements).
    pub fn rate(&self) -> Option<f64> {
        self.mean_all(&self.incumbent)
    }

    /// What `ZeroFarm status` / the tray say about it.
    pub fn describe(&self) -> String {
        let rate = self.rate().map(|r| format!(", {} leaves/s", thousands(r.round() as u64))).unwrap_or_default();
        match self.state {
            State::Tuned => format!("tuned: {}{rate}", self.incumbent.describe()),
            State::Checking => format!("weekly check of the tuned layout: {}{rate}", self.incumbent.describe()),
            _ => format!("tuning ({} of at most {} trials): {}{rate}", self.trials, MAX_TRIALS, self.incumbent.describe()),
        }
    }

    /// The coordinator's report of it (PROTOCOL 13): None before the incumbent was measured.
    pub fn report(&self) -> Option<Value> {
        let rate = self.rate()?;
        Some(json!({
            "run": self.run, "build": self.key.build, "runtime": self.key.runtime, "net": self.key.net,
            "layout": self.incumbent.to_json(), "leaves_s": rate.round(), "trials": self.measured.len(),
            "state": if self.state == State::Tuned { "tuned" } else { "searching" }, "t": self.t,
        }))
    }
}

/// `state/tuning.json`: every key's entry.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Cache {
    pub v: u32,
    pub entries: Vec<Entry>,
}

impl Cache {
    fn path(paths: &Paths) -> PathBuf {
        paths.state().join("tuning.json")
    }

    pub fn load(paths: &Paths) -> Cache {
        std::fs::read(Self::path(paths)).ok().and_then(|b| serde_json::from_slice::<Cache>(&b).ok()).filter(|c| c.v == 1).unwrap_or(Cache { v: 1, entries: vec![] })
    }

    pub fn save(&mut self, paths: &Paths) {
        self.v = 1;
        self.entries.sort_by_key(|e| std::cmp::Reverse(e.t));
        self.entries.truncate(MAX_ENTRIES);
        let _ = std::fs::create_dir_all(paths.state());
        if let Err(e) = zerofarm_common::fsutil::atomic_write(&Self::path(paths), &serde_json::to_vec_pretty(self).expect("json")) {
            log::warn!("could not save the tuning results: {e}");
        }
    }

    pub fn get(&self, key: &Key) -> Option<&Entry> {
        self.entries.iter().find(|e| &e.key == key)
    }

    /// Replaces (or adds) `e`'s key.
    pub fn put(&mut self, e: Entry) {
        self.entries.retain(|x| x.key != e.key);
        self.entries.push(e);
    }

    /// The coordinator's `tuning` (PROTOCOL 13): the most recently changed measured entries, at most `MAX_REPORTS`.
    pub fn reports(&self) -> Vec<Value> {
        let mut es: Vec<&Entry> = self.entries.iter().filter(|e| e.key.tuner == TUNER_VERSION).collect();
        es.sort_by_key(|e| std::cmp::Reverse(e.t));
        es.iter().filter_map(|e| e.report()).take(MAX_REPORTS).collect()
    }
}

/// CPU time at a point: the system's (idle, total over all CPUs) and our workers', ms. Two of them give the share of
/// the CPUs other programs used in between (a trial needs a quiet machine).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LoadMark {
    pub idle_ms: u64,
    pub total_ms: u64,
    pub ours_ms: u64,
}

impl LoadMark {
    /// Other programs' share of the CPUs from `self` to `now` (0 when no time passed or the system says nothing).
    pub fn other_since(&self, now: &LoadMark) -> f64 {
        let total = now.total_ms.saturating_sub(self.total_ms);
        if total == 0 {
            return 0.0;
        }
        let busy = total.saturating_sub(now.idle_ms.saturating_sub(self.idle_ms));
        let ours = now.ours_ms.saturating_sub(self.ours_ms);
        (busy.saturating_sub(ours) as f64 / total as f64).clamp(0.0, 1.0)
    }
}

/// The coordinator's `tuning_hint` (PROTOCOL 13), checked: its layout within bounds, made valid for `s`. Unsigned and
/// only ever a starting point: it decides how fast the worker plays, never what.
pub fn hint_layout(hint: Option<&Value>, s: &Space) -> Option<Layout> {
    let l = Layout::from_json(hint?.get("layout")?)?;
    Some(s.fit(l))
}

/// One trial's measurement (see the module's notes): fed the fresh worker's `status` counters, answers once it has a
/// rate over the phase window, or knows it can't.
#[derive(Clone, Debug)]
pub struct Probe {
    pub conc: u64,
    phase: Option<Phase>,
    warm: f64,
    window: f64,
    max_s: f64,
    /// (seconds since the worker started, leaves)
    last: Option<(f64, f64)>,
    refill: Option<(f64, f64)>,
    /// Where the window starts: (s, leaves).
    from: Option<(f64, f64)>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Verdict {
    Pending,
    /// Leaves/s over the window (and the window, which the first measurement sets).
    Rate(f64, Phase),
    Invalid(String),
}

impl Probe {
    /// `warm`, `window`: seconds (`window` only for the first measurement of a search, `phase` None).
    pub fn new(conc: u64, phase: Option<Phase>, warm: f64, window: f64) -> Probe {
        let scale = phase.map_or(1.0, |p| (conc as f64 / p.conc.max(1) as f64).max(1.0));
        Probe { conc: conc.max(1), phase, warm, window, max_s: 4.0 * (warm + window) * scale + 30.0 * warm.min(1.0), last: None, refill: None, from: None }
    }

    /// The worker's counters at `t` seconds after it started: `leaves` (cumulative), `games` (finished), `started`.
    pub fn sample(&mut self, t: f64, leaves: u64, games: u64, started: u64) -> Verdict {
        let leaves = leaves as f64;
        let conc = self.conc as f64;
        let prev = self.last.replace((t, leaves));
        if let Some((pt, pl)) = prev {
            if t <= pt || leaves < pl {
                // a counter that went back: not the same worker's
                return Verdict::Invalid("the worker's counters went back".into());
            }
        }
        if self.refill.is_none() && started.saturating_sub(games) as f64 >= (REFILL_SHARE * conc).floor().max(1.0) {
            self.refill = Some((t, leaves));
        }
        match self.phase {
            None => {
                let Some((rt, _)) = self.refill else { return self.timeout(t, leaves) };
                match self.from {
                    None if t >= rt + self.warm => {
                        self.from = Some((t, leaves));
                        Verdict::Pending
                    }
                    Some((ft, fl)) if t >= ft + self.window => {
                        if leaves <= fl {
                            return Verdict::Invalid("no leaves in the window".into());
                        }
                        Verdict::Rate((leaves - fl) / (t - ft), Phase { p0: fl / conc, p1: leaves / conc, conc: self.conc })
                    }
                    _ => self.timeout(t, leaves),
                }
            }
            Some(ph) => {
                let (a, b) = (ph.p0 * conc, ph.p1 * conc);
                if self.from.is_none() {
                    if leaves < a {
                        return self.timeout(t, leaves);
                    }
                    let Some((rt, rl)) = self.refill else {
                        return Verdict::Invalid("the games in flight didn't refill before the window".into());
                    };
                    // where the counter passed p0 (between the last two samples), or the refill if that came later
                    let ta = match prev {
                        Some((pt, pl)) if pl < a => pt + (t - pt) * (a - pl) / (leaves - pl).max(1e-9),
                        _ => t,
                    };
                    self.from = if rt > ta {
                        if rl > a + 0.25 * (b - a) {
                            return Verdict::Invalid("the games in flight refilled late in the window".into());
                        }
                        Some((rt, rl))
                    } else {
                        Some((ta, a))
                    };
                }
                let (ft, fl) = self.from.expect("set");
                if leaves >= b {
                    let tb = match prev {
                        Some((pt, pl)) if pl < b && pt >= ft => pt + (t - pt) * (b - pl) / (leaves - pl).max(1e-9),
                        _ => t,
                    };
                    let (end_t, end_l) = if tb > ft { (tb, b) } else { (t, leaves) };
                    if end_t <= ft || end_l <= fl {
                        return Verdict::Invalid("the window was empty".into());
                    }
                    return Verdict::Rate((end_l - fl) / (end_t - ft), ph);
                }
                self.timeout(t, leaves)
            }
        }
    }

    /// Past the longest a trial may take: a rate over what the window covered so far (a slow layout), else invalid.
    fn timeout(&self, t: f64, leaves: f64) -> Verdict {
        if t < self.max_s {
            return Verdict::Pending;
        }
        match (self.from, self.phase) {
            (Some((ft, fl)), Some(ph)) if t > ft && leaves > fl => Verdict::Rate((leaves - fl) / (t - ft), ph),
            _ => Verdict::Invalid(format!("no measurement after {t:.0} s")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn space(budget: u32) -> Space {
        Space { budget, sessions: true, conc_min: 64, conc_max: 4096 }
    }

    fn lay(threads: u32, w: u32, t: u32, groups: u64, conc: u64) -> Layout {
        Layout { threads, infer_workers: w, infer_threads: t, groups, conc }
    }

    #[test]
    fn layouts_fit_the_thread_budget() {
        let s = space(16);
        // the shipped macOS layout (16 + 5 x 3 = 31 threads) is cut to 16: threads per session first, then sessions
        let f = s.fit(lay(16, 5, 3, 12, 2304));
        assert!(f.total_threads() <= 16, "{f:?}");
        assert_eq!(f.groups, 12);
        // without the sessions feature: one session
        let one = Space { sessions: false, ..s };
        assert_eq!(one.fit(lay(8, 4, 2, 4, 32)).infer_workers, 1);
        // games in flight within bounds, at least MIN_BATCH rows per batch
        assert_eq!(s.fit(lay(8, 2, 4, 64, 8192)).conc, 4096);
        assert_eq!(s.fit(lay(8, 2, 4, 64, 64)).groups, 8, "64 games: at most 8 groups of 8 rows");
        // never 0 of anything, budget at least 2
        let tiny = Space { budget: 1, ..s };
        assert_eq!(tiny.fit(lay(0, 0, 0, 0, 0)), lay(1, 1, 1, 1, 64));
    }

    #[test]
    fn neighbours_move_one_coordinate() {
        let s = space(16);
        let l = lay(8, 4, 2, 6, 2304);
        assert_eq!(neighbour(&l, Coord::Batch, true, &s), Some(lay(8, 4, 2, 3, 2304)), "bigger batches: half the groups");
        assert_eq!(neighbour(&l, Coord::Batch, false, &s), Some(lay(8, 4, 2, 12, 2304)));
        // split: about 2 threads (16 / 8) between search and inference, in whole threads per session, the total kept
        assert_eq!(neighbour(&l, Coord::Split, true, &s), Some(lay(4, 4, 3, 6, 2304)), "4 sessions: one more thread each");
        assert_eq!(neighbour(&l, Coord::Split, false, &s), Some(lay(12, 4, 1, 6, 2304)));
        assert_eq!(neighbour(&lay(8, 8, 1, 6, 2304), Coord::Split, false, &s), None, "no thread per session to give");
        let l2 = lay(8, 2, 4, 6, 2304);
        assert_eq!(neighbour(&l2, Coord::Split, true, &s), Some(lay(6, 2, 5, 6, 2304)));
        assert_eq!(neighbour(&l2, Coord::Split, false, &s), Some(lay(10, 2, 3, 6, 2304)));
        assert_eq!(neighbour(&l, Coord::Sessions, true, &s), Some(lay(8, 8, 1, 6, 2304)));
        assert_eq!(neighbour(&l, Coord::Sessions, false, &s), Some(lay(8, 2, 4, 6, 2304)));
        // games in flight: rows per batch kept
        assert_eq!(neighbour(&l, Coord::Conc, false, &s), Some(lay(8, 4, 2, 3, 1152)));
        assert_eq!(neighbour(&l, Coord::Conc, true, &s), Some(lay(8, 4, 2, 11, 4096)), "capped at 4,096");
        // the run's number: fixed
        let fixed = Space { conc_min: 2304, conc_max: 2304, ..s };
        assert_eq!(neighbour(&l, Coord::Conc, true, &fixed), None);
        let no_sessions = Space { sessions: false, ..s };
        assert_eq!(neighbour(&lay(8, 1, 8, 4, 32), Coord::Sessions, true, &no_sessions), None);
        assert_eq!(neighbour(&lay(8, 1, 8, 1, 2304), Coord::Batch, true, &s), None, "one group already");
    }

    /// A rate source: a peak at `best`, 15 % less per doubling away on each coordinate, oversubscription free (the
    /// budget is a constraint), and `noise` (a fixed pseudo-random sequence of +- that share).
    struct Fake {
        best: Layout,
        noise: f64,
        n: u64,
    }

    impl Fake {
        fn rate(&mut self, l: &Layout) -> f64 {
            let f = |v: f64, b: f64| (1.0 - 0.15 * (v / b).log2().abs()).max(0.05);
            let r = 10_000.0
                * f(l.threads as f64, self.best.threads as f64)
                * f(l.infer_total() as f64, self.best.infer_total() as f64)
                * f(l.infer_workers as f64, self.best.infer_workers as f64)
                * f(l.batch() as f64, self.best.batch() as f64)
                * f(l.conc as f64, self.best.conc as f64);
            self.n = self.n.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let u = ((self.n >> 33) as f64 / (1u64 << 31) as f64) * 2.0 - 1.0;
            r * (1.0 + self.noise * u)
        }
    }

    fn key() -> Key {
        Key { build: "b".into(), runtime: None, net: "n".into(), hw: "h".into(), os: "o".into(), tuner: TUNER_VERSION }
    }

    /// Runs a search to its end with `fake`: (entry, layouts measured in order).
    fn search(start: Layout, s: &Space, fake: &mut Fake) -> (Entry, Vec<Layout>) {
        let mut e = Entry::new(key(), "r", s.fit(start), false, 0);
        let mut seen = vec![];
        let mut guard = 0;
        while let Some(l) = e.plan(s) {
            seen.push(l);
            let r = fake.rate(&l);
            e.record(l, r, Some(Phase { p0: 1.0, p1: 2.0, conc: 64 }), s, 100);
            guard += 1;
            assert!(guard < 40, "the search doesn't end: {seen:?}");
        }
        (e, seen)
    }

    #[test]
    fn the_search_climbs_to_a_better_layout_within_its_budget() {
        let s = space(16);
        let best = lay(4, 4, 3, 4, 2304);
        let mut fake = Fake { best, noise: 0.0, n: 1 };
        let start = lay(8, 4, 2, 12, 2304);
        let start_rate = fake.rate(&start);
        let (e, seen) = search(start, &s, &mut fake);
        assert_eq!(e.state, State::Tuned);
        assert!(e.trials <= MAX_TRIALS, "{}", e.trials);
        assert!(seen.len() as u32 <= MAX_TRIALS + 2, "start + trials + the final re-measure: {seen:?}");
        let got = fake.rate(&e.incumbent);
        assert!(got > start_rate * 1.2, "{:?} {got} vs start {start_rate}", e.incumbent);
        assert!(seen.iter().all(|l| l.total_threads() <= 16), "never over the budget: {seen:?}");
        // the winner was measured twice (the final re-measure)
        assert_eq!(e.measured.iter().filter(|m| m.layout == e.incumbent).count(), 2);
        assert!(e.check_after == Some(100 + RECHECK_S));
        assert!(e.runner_up().is_some());
    }

    #[test]
    fn a_move_that_paid_is_tried_again_first() {
        let s = space(16);
        let start = lay(12, 2, 2, 6, 2304);
        let mut e = Entry::new(key(), "r", start, false, 0);
        e.record(start, 10_000.0, None, &s, 0);
        // more inference threads pays: the next candidate goes on that way, before the other coordinates
        let more_infer = neighbour(&start, Coord::Split, true, &s).unwrap();
        e.record(more_infer, 11_000.0, None, &s, 0);
        assert_eq!(e.incumbent, more_infer);
        assert_eq!(e.momentum, Some((1, true)));
        assert_eq!(e.plan(&s), neighbour(&more_infer, Coord::Split, true, &s));
    }

    #[test]
    fn the_start_stays_when_nothing_is_clearly_faster() {
        let s = space(16);
        let start = lay(8, 4, 2, 6, 2304);
        // the start is the peak: every neighbour is slower
        let mut fake = Fake { best: start, noise: 0.0, n: 1 };
        let (e, seen) = search(start, &s, &mut fake);
        assert_eq!(e.incumbent, start);
        assert_eq!(seen[0], start, "the start is measured first");
        assert!(!seen[1..].contains(&start), "no final re-measure of an unchanged start: {seen:?}");
    }

    #[test]
    fn noise_below_the_gain_never_moves_the_incumbent() {
        let s = space(16);
        let start = lay(8, 4, 2, 6, 2304);
        // a flat landscape (every layout the same) with +-1 % noise
        for seed in 1..50u64 {
            let mut e = Entry::new(key(), "r", start, false, 0);
            let mut n = seed;
            while let Some(l) = e.plan(&s) {
                n = n.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                let u = ((n >> 33) as f64 / (1u64 << 31) as f64) * 2.0 - 1.0;
                e.record(l, 10_000.0 * (1.0 + 0.01 * u), None, &s, 0);
            }
            assert_eq!(e.incumbent, start, "seed {seed}");
        }
    }

    #[test]
    fn a_lucky_measurement_is_undone_by_the_final_re_measure() {
        let s = space(16);
        let start = lay(8, 4, 2, 6, 2304);
        let mut e = Entry::new(key(), "r", start, false, 0);
        e.record(start, 10_000.0, None, &s, 0);
        // the first candidate measures 5 % faster once (luck), every other one 10 % slower
        let first = e.plan(&s).unwrap();
        e.record(first, 10_500.0, None, &s, 0);
        assert_eq!(e.incumbent, first);
        while let Some(l) = e.plan(&s) {
            let r = if l == first { 10_050.0 } else { 9_000.0 };
            e.record(l, r, None, &s, 0);
        }
        assert_eq!(e.state, State::Tuned);
        assert_eq!(e.incumbent, start, "mean 10,275 is under 3 % over the start");
    }

    #[test]
    fn discarded_trials_are_retried_then_skipped() {
        let s = space(16);
        let start = lay(8, 4, 2, 6, 2304);
        let mut e = Entry::new(key(), "r", start, false, 0);
        e.record(start, 10_000.0, None, &s, 0);
        let c = e.plan(&s).unwrap();
        e.discard(c, &s, 0);
        assert_eq!(e.plan(&s), Some(c), "a discarded trial is tried again");
        e.discard(c, &s, 0);
        e.discard(c, &s, 0);
        assert_ne!(e.plan(&s), Some(c), "after {MAX_DISCARDS} discards it is skipped");
        assert_eq!(e.trials, 0, "discards are not trials");
    }

    #[test]
    fn the_weekly_check_compares_runner_up_and_incumbent_afresh() {
        let s = space(16);
        let start = lay(8, 4, 2, 6, 2304);
        let mut fake = Fake { best: start, noise: 0.0, n: 1 };
        let (mut e, _) = search(start, &s, &mut fake);
        assert!(!e.maybe_check(100 + RECHECK_S - 1), "not due yet");
        assert!(e.maybe_check(100 + RECHECK_S));
        assert_eq!(e.state, State::Checking);
        let r = e.runner_up().unwrap();
        assert_eq!(e.plan(&s), Some(r), "the runner-up first");
        e.record(r, 12_000.0, None, &s, 200 + RECHECK_S);
        assert_eq!(e.plan(&s), Some(start), "then the incumbent");
        e.record(start, 10_000.0, None, &s, 200 + RECHECK_S);
        assert_eq!(e.state, State::Tuned);
        assert_eq!(e.incumbent, r, "the runner-up is now 20 % faster");
        assert_eq!(e.check_after, Some(200 + 2 * RECHECK_S));
    }

    #[test]
    fn the_cache_keeps_the_most_recent_keys_and_reports_measured_ones() {
        let d = tempfile::tempdir().unwrap();
        let paths = Paths::new(d.path());
        let mut c = Cache::load(&paths);
        assert!(c.entries.is_empty());
        let s = space(16);
        for i in 0..(MAX_ENTRIES as i64 + 3) {
            let mut k = key();
            k.net = format!("n{i}");
            let mut e = Entry::new(k, "r", lay(8, 4, 2, 6, 2304), false, i);
            if i % 2 == 0 {
                e.record(e.start, 1000.0 + i as f64, None, &s, i);
            }
            c.put(e);
        }
        c.save(&paths);
        let c = Cache::load(&paths);
        assert_eq!(c.entries.len(), MAX_ENTRIES);
        assert!(c.entries.iter().all(|e| e.t >= 3), "the oldest went");
        let r = c.reports();
        assert_eq!(r.len(), MAX_REPORTS);
        assert_eq!(r[0]["net"], "n18", "newest first: {r:?}");
        assert_eq!(r[0]["layout"]["infer_workers"], 4);
        assert!(r.iter().all(|x| x["leaves_s"].as_f64().unwrap() > 0.0));
    }

    #[test]
    fn hints_are_checked_and_fitted() {
        let s = space(16);
        let ok = json!({"layout": {"threads": 6, "infer_workers": 5, "infer_threads": 2, "groups": 6, "conc": 2304}, "leaves_s": 15000});
        assert_eq!(hint_layout(Some(&ok), &s), Some(lay(6, 5, 2, 6, 2304)));
        let over = json!({"layout": {"threads": 30, "infer_workers": 5, "infer_threads": 3, "groups": 6, "conc": 2304}});
        assert!(hint_layout(Some(&over), &s).unwrap().total_threads() <= 16, "fitted to this machine's budget");
        for bad in [
            json!({"layout": {"threads": 0, "infer_workers": 1, "infer_threads": 1, "groups": 1, "conc": 32}}),
            json!({"layout": {"threads": 4, "infer_workers": 1, "infer_threads": 1, "groups": 64, "conc": 32}}),
            json!({"layout": {"threads": 4, "infer_workers": 99, "infer_threads": 1, "groups": 1, "conc": 32}}),
            json!({"layout": {"threads": 4, "infer_workers": 1, "infer_threads": 1, "groups": 1}}),
            json!({"layout": "fast"}),
            json!(null),
        ] {
            assert_eq!(hint_layout(Some(&bad), &s), None, "{bad}");
        }
        assert_eq!(hint_layout(None, &s), None);
    }

    #[test]
    fn defaults_per_platform() {
        let m4max = Hw { cores: 16, logical: 16, perf: 12, eff: 4 };
        let d = defaults(Platform::MacOs, &m4max);
        assert_eq!(d.budget, 16);
        assert!(d.threads + d.infer_workers * d.infer_threads <= 16, "{d:?}");
        assert!(d.sessions_always && d.batch.is_some());
        let air = Hw { cores: 8, logical: 8, perf: 4, eff: 4 };
        let d = defaults(Platform::MacOs, &air);
        assert!(d.threads + d.infer_workers * d.infer_threads <= 8 && d.threads >= 1, "{d:?}");
        // Windows: today's layout on an SMT CPU (one search thread per core, one session of the other threads)
        let i9 = Hw { cores: 8, logical: 16, perf: 8, eff: 0 };
        let d = defaults(Platform::Windows, &i9);
        assert_eq!((d.budget, d.threads, d.infer_workers, d.infer_threads, d.sessions_always), (16, 8, 1, 8, false));
        let no_smt = Hw { cores: 6, logical: 6, perf: 6, eff: 0 };
        let d = defaults(Platform::Windows, &no_smt);
        assert_eq!((d.budget, d.threads, d.infer_threads), (6, 3, 3), "no SMT: the cores split");
    }

    #[test]
    fn fingerprints() {
        let t = |v: Value| v.as_object().unwrap().clone();
        let a = net_fingerprint(&t(json!({"settings": {"sims": 8}, "gid_base": 1, "games": 4, "seed": 9})), Some(40 << 20));
        assert_eq!(a, net_fingerprint(&t(json!({"settings": {"sims": 8}, "gid_base": 77, "games": 1, "seed": 3})), Some((40 << 20) + 5)), "per-lease fields and a few bytes don't count");
        assert_ne!(a, net_fingerprint(&t(json!({"settings": {"sims": 16}})), Some(40 << 20)), "settings do");
        assert_ne!(a, net_fingerprint(&t(json!({"settings": {"sims": 8}})), Some(80 << 20)), "the network's size does");
        assert_eq!(a.len(), 16);
    }

    /// Statuses every `dt` s of a worker at `rate(p)` leaves/s (p: leaves per game in flight), its games refilled from
    /// `refill_s`, fed to `p` until it answers.
    fn run_probe(p: &mut Probe, conc: u64, refill_s: f64, rate: &dyn Fn(f64) -> f64, dt: f64) -> Verdict {
        let (mut t, mut leaves) = (0.0f64, 0.0f64);
        loop {
            t += dt;
            leaves += rate(leaves / conc as f64) * dt * if t < refill_s { 0.5 } else { 1.0 };
            let started = if t < refill_s { conc / 2 } else { conc };
            match p.sample(t, leaves as u64, 0, started) {
                Verdict::Pending => {}
                v => return v,
            }
            assert!(t < 10_000.0);
        }
    }

    #[test]
    fn the_probe_compares_at_the_same_game_progress() {
        // a fresh worker's rate falls as its games grow: 1,000 / (1 + p / 2,000) leaves/s per 32 games in flight,
        // more games in flight 1.2x faster at the same progress
        let decay = |base: f64| move |p: f64| base / (1.0 + p / 2000.0);
        let mut first = Probe::new(32, None, 20.0, 75.0);
        let Verdict::Rate(r32, ph) = run_probe(&mut first, 32, 5.0, &decay(1000.0), 1.0) else { panic!() };
        assert!(ph.p0 > 0.0 && ph.p1 > ph.p0 && ph.conc == 32, "{ph:?}");
        let mut second = Probe::new(64, Some(ph), 20.0, 75.0);
        let Verdict::Rate(r64, _) = run_probe(&mut second, 64, 5.0, &decay(1200.0), 1.0) else { panic!() };
        let ratio = r64 / r32;
        assert!((ratio - 1.2).abs() < 0.01, "matched progress gives the true 1.2: {ratio}");
        // the same layout measured again gives the same rate (statuses at other times)
        let mut again = Probe::new(32, Some(ph), 20.0, 75.0);
        let Verdict::Rate(r32b, _) = run_probe(&mut again, 32, 3.0, &decay(1000.0), 1.7) else { panic!() };
        assert!((r32b / r32 - 1.0).abs() < 0.01, "{r32b} vs {r32}");
    }

    #[test]
    fn the_probe_refuses_what_it_cant_measure() {
        // never refilled (no ids): no measurement, at the timeout
        let mut p = Probe::new(32, None, 1.0, 2.0);
        let mut v = Verdict::Pending;
        for i in 1..100 {
            v = p.sample(i as f64, i * 100, 0, 4);
            if v != Verdict::Pending {
                break;
            }
        }
        assert!(matches!(v, Verdict::Invalid(_)), "{v:?}");
        // counters that go back (another worker)
        let mut p = Probe::new(32, None, 1.0, 2.0);
        assert_eq!(p.sample(1.0, 500, 0, 32), Verdict::Pending);
        assert!(matches!(p.sample(2.0, 100, 0, 32), Verdict::Invalid(_)));
        // a slow trial: at the timeout, the rate over what the window covered
        let ph = Phase { p0: 10.0, p1: 1000.0, conc: 32 };
        let mut p = Probe::new(32, Some(ph), 1.0, 2.0);
        let mut v = Verdict::Pending;
        for i in 1..1000 {
            v = p.sample(i as f64, i * 50, 0, 32);
            if v != Verdict::Pending {
                break;
            }
        }
        match v {
            Verdict::Rate(r, _) => assert!((r - 50.0).abs() < 1.0, "{r}"),
            v => panic!("{v:?}"),
        }
    }

    #[test]
    fn other_programs_load() {
        let a = LoadMark { idle_ms: 1000, total_ms: 10_000, ours_ms: 500 };
        // 16,000 CPU-ms passed, 4,000 idle: 12,000 busy, 10,000 of them ours
        let b = LoadMark { idle_ms: 5000, total_ms: 26_000, ours_ms: 10_500 };
        assert!((a.other_since(&b) - 0.125).abs() < 1e-9);
        assert_eq!(a.other_since(&a), 0.0);
        let ours_more = LoadMark { ours_ms: 99_999, ..b };
        assert_eq!(a.other_since(&ours_more), 0.0, "never below 0");
    }

    #[test]
    fn thousands_separators() {
        assert_eq!(thousands(2304), "2,304");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1_234_567), "1,234,567");
        assert!(lay(8, 4, 2, 6, 2304).describe().contains("2,304 games in flight in 6 groups (384 rows)"));
    }
}

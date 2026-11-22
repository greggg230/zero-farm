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
//! a coordinate search: for each coordinate in turn (batch size, search threads, threads per inference session,
//! sessions, games in flight) the incumbent's two neighbours, moving to one that is at least `GAIN` faster (and on from
//! there), at most `MAX_TRIALS` trials; then the incumbent is measured once more and kept only if its mean is still
//! `GAIN` over the start's (a lucky single measurement doesn't stick). Search threads and inference threads each stay
//! within a cap, never searched upward (the M4 Max, 2026-11-18: every layout with more threads than its 16 + 5 x 3 ran
//! slower, 36 + 12 x 3 at 0.3x: oversubscribing collapses the native worker), and the batches within the platform's
//! rows (`defaults`).
//!
//! **Remembered** per (worker build, runtime pack, network fingerprint, hardware, OS, tuner version); a key not seen
//! is searched again; a tuned key is re-checked weekly (the runner-up against the incumbent, both measured afresh).
//!
//! **Several workers** (PROTOCOL 13.5). The layout's first coordinate is the number of worker processes serving the
//! lease (`workers`, 1 to `MAX_WORKERS`), each with the rest of the layout (search threads, sessions, groups and games
//! in flight are per worker; the games in flight of the job are `workers` x `conc`). The M4 Max, 2026-11-18, clean
//! windows: one native worker at best ~12.6k leaves/s (old export) - every extra thread inside one process hurt -,
//! two processes of (12 search + 4 x 3 inference threads, 1,152 games in flight in 6 groups) 16.4k (+30 %; the fast
//! export ~18.0k, the owner's SSH self-play's parity), three 16.1k, four 15.2k. Contention inside one process (its
//! search threads and evaluator share one tree arena, one cache, one runtime) is what costs, not the thread total, so
//! the thread caps are per worker: one worker keeps the caps of before (search and inference threads each at most the
//! cores), and N > 1 workers share `THREAD_BUDGET_MULTI` x the cores per kind (2 x 12 on 16 cores, the measured
//! best). Worker counts stay within the cores and the RAM (`CORES_PER_WORKER`, `RAM_MB_PER_WORKER`) and within the
//! memory plan (each worker's base plus its games in flight, PROTOCOL 10.6, at most `PLAN_SHARE` of the job's limit).

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::path::PathBuf;
use zerofarm_common::layout::Layout as Paths;

/// The `features` this agent asks `/v1/lease` with for tuning (PROTOCOL 13: it takes `tuning_hint`).
pub const FEATURES: [&str; 1] = ["tuning"];
/// A worker whose `hello` lists this honours the job line's `infer_workers` (macOS builds always do: `Defaults`).
pub const WORKER_FEATURE: &str = "infer_workers";
/// Bumped when the search or the measurement changes: every key is searched again. (2: the worker count, 13.5.)
pub const TUNER_VERSION: u32 = 2;
/// agent 0.3.7: a GPU job's keys are searched again from this version (3: two groups a worker, trials after the drain).
/// Entries of 0.3.5-0.3.6 counted every postponed trial as a discard: the 4070 Ti PC's crew of two had 3 (given up on)
/// before 0.3.7 could hand over to it, and its start was a single worker.
pub const GPU_TUNER_VERSION: u32 = 3;
/// agent 0.3.12: a DirectML GPU job's keys (provider `dml`) are searched again from this version (4: DirectML's own
/// start, `Space::dml_start`). 0.3.11's entries started from one worker of ~270 games in flight and crept up within 9
/// trials (discarded whenever the PC wasn't idle); their incumbents would keep a lease from ever starting the crew. Other
/// GPU jobs' keys keep `GPU_TUNER_VERSION`.
pub const DML_TUNER_VERSION: u32 = 4;
/// agent 0.3.16: an NVIDIA GPU's CUDA job starts from the crew rule of DirectML's start (`Space::dml_start`, the target
/// `rolling::dml_target`: up to 3 workers of `rolling::DML_CONC` games in flight in two groups, each down to
/// `rolling::DML_MIN_CONC` where the memory plans hold less before the crew has a worker fewer) instead of the most workers
/// sharing two of the bench's full batches. The farm's CUDA worker is bound by ONNX Runtime's host side (~1,200 kernels
/// launched one by one a call, ~20 ms of a CPU thread whatever the batch), so each worker wants big batches, and more
/// games in flight in all pay until a worker's evaluator thread is busy all the time. Measured on the 4070 Ti PC
/// (4 cores / 8 threads, 2026-11-22, the released CUDA worker, the fp16 variant, leaves/s; the trainer's torch path
/// 32.9k): 3 x 384 in one group 20.0k, 3 x 704 in two (0.3.15's start there) 23.5k, 3 x 912 26.4k, 3 x 1,150 28.1k,
/// 2 x 1,244 (3 search threads each) 25.2k, 2 x 1,866 24.9k, 1 x 2,048 17.0k: with that PC's memory plan (1.99 GB a
/// worker + 2-3 MB a game, 11.4 GB) the rule starts 3 x 912 at 2 MB a game and 2 x 1,152 at 3 MB, where 0.3.15 started
/// 3 x 704 / 3 x 608 / 3 x 384 (the bench's batch 1,024 or 512).
pub const CUDA_CREW: bool = true;
/// agent 0.3.16: a CUDA crew (`CUDA_CREW`) runs at least this many workers where the memory plans hold them (each with
/// `rolling::DML_MIN_CONC` games in flight): one worker's evaluator thread caps it - on the 4070 Ti PC one worker of
/// 1,796-2,048 games in flight played 14.5-17.0k leaves/s, every crew of two or three 20-28k -, and the tuner's trial of
/// one worker cost a handoff that quit ~1,200 games in flight (the crew's memory) and ~9 minutes at its rate.
pub const CUDA_CREW_MIN_WORKERS: u32 = 2;
/// agent 0.3.16: a CUDA crew (`CUDA_CREW`) plays its start for the whole lease, with no in-lease trials (the memory plan
/// still re-plans it, as a fixed layout's). Each GPU trial is a handoff: the old crew's games in flight (1,000-1,600 on
/// the 4070 Ti PC) are quit after `drain_tail` because the next crew needs their memory, the new crew refills under the
/// coordinator's in-flight limit (rev 21), and a trial short of ids is put off and handed over to again - on a test run
/// with no quota (2026-11-22, 0.3.15's tuner) six trials in 47 minutes quit 6,783 games against ~12,500 finished and the
/// lease averaged ~16k leaves/s where its layouts measured 21-22k.
pub const CUDA_CREW_NO_TRIALS: bool = true;
/// agent 0.3.16: CUDA crews' keys (`CUDA_CREW`) searched again from their new start.
pub const CUDA_CREW_TUNER_VERSION: u32 = 5;
/// Worker processes a lease's job may run (searched and set) at most; a hint or a report may say up to `MAX_WORKERS_JSON`.
pub const MAX_WORKERS: u32 = 3;
pub const MAX_WORKERS_JSON: u32 = 8;
/// N > 1 workers share this many times the cores per kind of thread (search, inference): per worker
/// `cap x THREAD_BUDGET_MULTI / N` (the M4 Max's best: 2 x 12 on 16 cores). One worker keeps `cap`.
pub const THREAD_BUDGET_MULTI: f64 = 1.5;
/// A worker per this many physical cores at most ...
pub const CORES_PER_WORKER: u32 = 4;
/// ... and per this much RAM (MB): a 64 GB M4 Max 3 at most, a 32 GB machine 2, 16 GB 1.
pub const RAM_MB_PER_WORKER: u64 = 16 * 1024;
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
/// Other programs may use at most this share of the CPUs, on average, before a trial (`LOAD_BASELINE_TRIALS`) and over
/// it (agent 0.3.3: a mean over the window, not every sample - a Mac's background daemons burst, a sync daemon to ~55 %
/// of a core, syncthing to ~90 %, and 0.3.2 discarded every trial within seconds of such a burst).
pub const OTHER_LOAD_MAX: f64 = 0.10;
/// ... and a trial's mean may exceed the mean before it by at most this much (interference that came with the trial
/// would bias its comparison).
pub const OTHER_LOAD_RISE: f64 = 0.05;
/// A trial is discarded at once when other programs used more than this share over the last `LOAD_SUSTAINED_SHARE` of
/// a trial's window (sustained, not a burst): it couldn't end valid.
pub const OTHER_LOAD_SUSTAINED: f64 = 0.25;
/// The load before a trial is averaged over this many trial lengths (`tune_warm` + `tune_window`: 95 s).
pub const LOAD_BASELINE_TRIALS: f64 = 1.0;
/// "Sustained" is this share of a trial's window (`tune_window`: 30 s of the real 75).
pub const LOAD_SUSTAINED_SHARE: f64 = 0.4;
/// A candidate measured with this much less load from other programs than the incumbent (the CPU it didn't have to
/// share) doesn't replace it: the comparison is between similar conditions.
pub const LOAD_SIMILAR: f64 = 0.05;
/// agent 0.3.3: at most this many layout handoffs start per `Timings::tune_handoff_window` (an hour): a safety net under
/// the one-drain-at-a-time rule (lease.rs) - 0.3.2 handed a job over to the same layout 3 times in 7 minutes.
pub const MAX_HANDOFFS: usize = 6;
/// A batch has at least this many rows (fewer groups than that would make).
pub const MIN_BATCH: u64 = 8;
/// A trial's games in flight count as refilled at this share of its `conc`.
pub const REFILL_SHARE: f64 = 0.98;
/// Bounds of a layout's numbers (a hint is refused outside them).
pub const MAX_THREADS: u32 = 512;
pub const MAX_SESSIONS: u32 = 64;

/// A CPU job's layout: `workers` (worker processes serving the lease, PROTOCOL 13.5) and each one's job line:
/// `threads` (search), `infer_workers` (ONNX Runtime sessions, each on an evaluator thread of its own),
/// `infer_threads` (threads per session), `groups` (batches in turn: rows per batch = conc / groups) and `conc` (its
/// games in flight; the job's are `workers` x `conc`).
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Layout {
    #[serde(default = "one_worker")]
    pub workers: u32,
    pub threads: u32,
    pub infer_workers: u32,
    pub infer_threads: u32,
    pub groups: u64,
    pub conc: u64,
}

fn one_worker() -> u32 {
    1
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
    /// The job's games in flight: every worker's.
    pub fn total_conc(&self) -> u64 {
        self.workers.max(1) as u64 * self.conc
    }
    /// One worker's part of it (its job line).
    pub fn per_worker(&self) -> Layout {
        Layout { workers: 1, ..*self }
    }
    /// "8 search + 4 x 2 inference threads, 2,304 games in flight in 6 groups (384 rows)"; several workers: "2 workers x
    /// (12 search + 4 x 3 inference threads, 1,152 games in flight in 6 groups (192 rows))".
    pub fn describe(&self) -> String {
        self.describe_on(false)
    }

    /// `describe`, or (agent 0.3.4) a GPU job's: "3 workers x (2 search threads, 384 games in flight in 1 group (384
    /// rows)) on the GPU" - its inference threads are the worker's own.
    pub fn describe_on(&self, gpu: bool) -> String {
        if gpu {
            let one = format!(
                "{} search threads, {} games in flight in {} group{} ({} rows)",
                self.threads,
                thousands(self.conc),
                self.groups,
                if self.groups == 1 { "" } else { "s" },
                self.batch()
            );
            return if self.workers > 1 { format!("{} workers x ({one}) on the GPU", self.workers) } else { format!("{one} on the GPU") };
        }
        let one = format!(
            "{} search + {} x {} inference threads, {} games in flight in {} groups ({} rows)",
            self.threads,
            self.infer_workers,
            self.infer_threads,
            thousands(self.conc),
            self.groups,
            self.batch()
        );
        if self.workers > 1 {
            format!("{} workers x ({one})", self.workers)
        } else {
            one
        }
    }
    pub fn to_json(&self) -> Value {
        serde_json::to_value(self).expect("json")
    }
    /// A layout from JSON (a coordinator hint): every field present and within bounds (`workers` may be left out: 1),
    /// else None.
    pub fn from_json(v: &Value) -> Option<Layout> {
        let n = |k: &str, max: u64| v.get(k).and_then(Value::as_u64).filter(|x| (1..=max).contains(x));
        let workers = match v.get("workers") {
            None | Some(Value::Null) => 1,
            Some(_) => n("workers", MAX_WORKERS_JSON as u64)? as u32,
        };
        let l = Layout {
            workers,
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
    /// Installed RAM (MB).
    pub ram_mb: u64,
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

/// The starting point and the bounds per platform (the only per-platform part of tuning).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Defaults {
    /// rev 19 (PROTOCOL 13.5): the start's worker processes, and the most the cores and RAM allow (`max_workers`).
    pub workers: u32,
    pub max_workers: u32,
    /// The start's search threads, sessions and threads per session (per worker, for `workers`).
    pub threads: u32,
    pub infer_workers: u32,
    pub infer_threads: u32,
    /// One worker's search threads at most, and inference threads (sessions x threads per session) at most: never
    /// searched upward (the owner's `cpu_threads` replaces both); N > 1 workers share `THREAD_BUDGET_MULTI` x them.
    pub max_search: u32,
    pub max_infer: u32,
    /// Rows per batch within these bounds (`groups` = games in flight / rows; the start's groups are 10.6's "auto").
    pub batch_min: u64,
    pub batch_max: u64,
    /// Every worker build of the platform honours `infer_workers` (else only one whose `hello` lists the feature).
    pub sessions_always: bool,
    /// The start's inference is in sessions of this many threads (0: one session of all of them).
    pub session_threads: u32,
}

/// Per-worker threads of one kind for `n` workers within a cap: the cap for one worker, else `THREAD_BUDGET_MULTI` x the
/// cap shared by `n` (at least 1).
pub fn budget(cap: u32, n: u32) -> u32 {
    if n <= 1 {
        cap.max(1)
    } else {
        ((cap.max(1) as f64 * THREAD_BUDGET_MULTI / n as f64).floor() as u32).max(1)
    }
}

impl Defaults {
    /// The platform's per-worker (search threads, sessions, threads per session) for `n` workers: search threads at the
    /// budget, inference at the budget in sessions of `session_threads` (one session under two of them).
    pub fn threads_for(&self, n: u32) -> (u32, u32, u32) {
        let (s, i) = (budget(self.max_search, n), budget(self.max_infer, n));
        let st = self.session_threads;
        if st > 0 && i >= 2 * st {
            (s, i / st, st)
        } else {
            (s, 1, i)
        }
    }

    /// The start's layout for `n` workers serving `conc` games in flight in `groups` groups in all (10.6's choice): each
    /// worker the platform's threads for `n`, `conc` / `n` games in flight (a multiple of 32 from 32) in its share of
    /// the groups (agent 0.3.3: the group count is kept, not the rows per batch - 12 groups on 16 cores are 6 a worker for
    /// two, whatever memory leaves of the games in flight; `Space::fit` keeps the batches within bounds).
    pub fn layout_for(&self, n: u32, conc: u64, groups: u64) -> Layout {
        let n = n.max(1);
        let (threads, infer_workers, infer_threads) = self.threads_for(n);
        let per = if n > 1 { crate::rolling::round_games(conc / n as u64) } else { conc.max(1) };
        Layout { workers: n, threads, infer_workers, infer_threads, groups: groups.max(1).div_ceil(n as u64).min(per).max(1), conc: per }
    }
}

/// The starting point for `hw` on `p`.
///
/// Windows: today's layout - one worker, one search thread per physical core, one inference session with as many
/// threads (the worker's own default) -, which is also each one's cap (never more threads than before); batches of at
/// least `MIN_BATCH` rows.
///
/// macOS (Apple silicon), from the M4 Max's clean windows of 2026-11-18 (legacy-meta3-r1's gen-398 network, leaves/s;
/// the owner's 3-process setup 19,035-20,034, 19,504 in a later control): in one worker the layout agent 0.3.0 ships -
/// one search thread per core, inference in sessions of 3 threads (16 cores: 16 + 5 x 3), 2,304 games in flight in 12
/// groups of 192 rows - is the best measured region: 10,977-11,086 (fused-attention export 12,292); 6 groups (384-450
/// rows) 11,155 (12,560); 4 groups (~675 rows) 8,970 (load 46 %: too few groups starve the search); 4 x 4 inference
/// threads 6,596; 24 + 8 x 2 7,707; 36 + 12 x 3 3,475 - more threads in any form hurt, and inference cost per row is
/// about flat in the batch size. So the search stays narrow around it: search and inference threads each at most the
/// cores per worker, batches of 192-384 rows. Two workers of (12 + 4 x 3, 1,152 games in flight in 6 groups) played
/// 16.4k (fast export ~18.0k, three 16.1k, four 15.2k): a Mac with at least 12 cores and 32 GB starts with two
/// (PROTOCOL 13.5), each with the games in flight the memory plan holds (`Space::fit`: 921 each on a 64 GB Mac at the
/// default cap, `rolling::MACOS_CPU_COST`).
pub fn defaults(p: Platform, hw: &Hw) -> Defaults {
    let cores = hw.cores.max(1);
    let max_workers = MAX_WORKERS.min(cores / CORES_PER_WORKER).min((hw.ram_mb / RAM_MB_PER_WORKER) as u32).max(1);
    let mut d = match p {
        Platform::Windows => Defaults {
            workers: 1,
            max_workers,
            threads: cores,
            infer_workers: 1,
            infer_threads: cores,
            max_search: cores,
            max_infer: cores,
            batch_min: MIN_BATCH,
            batch_max: crate::rolling::MAX_INFLIGHT,
            sessions_always: false,
            session_threads: 0,
        },
        Platform::MacOs => Defaults {
            workers: if cores >= 12 && hw.ram_mb >= 32 * 1024 { 2 } else { 1 },
            max_workers,
            threads: cores,
            infer_workers: 1,
            infer_threads: cores,
            max_search: cores,
            max_infer: cores,
            batch_min: crate::rolling::MACOS_CPU_MIN_BATCH,
            batch_max: 2 * crate::rolling::MACOS_CPU_BATCH,
            sessions_always: true,
            // (the worker's own split, docs/az.md 62.1: sessions of 3 threads; under 6 threads one session)
            session_threads: 3,
        },
    };
    d.workers = d.workers.min(d.max_workers);
    (d.threads, d.infer_workers, d.infer_threads) = d.threads_for(d.workers);
    d
}

/// What a search may vary for one job.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct Space {
    /// One worker's search threads at most, inference threads at most (N > 1 workers: `budget` of them).
    pub max_search: u32,
    pub max_infer: u32,
    /// The worker honours `infer_workers` (else one session).
    pub sessions: bool,
    /// The job's games in flight (all workers') within these bounds (equal: the run's number, fixed).
    pub conc_min: u64,
    pub conc_max: u64,
    /// Rows per batch within these bounds where the games in flight allow.
    pub batch_min: u64,
    pub batch_max: u64,
    /// Worker processes within these bounds (equal: fixed).
    pub workers_min: u32,
    pub workers_max: u32,
    /// The job's memory plan (PROTOCOL 10.6): its limit (MB) and cost; each worker's base and games in flight together
    /// at most `PLAN_SHARE` of the limit. None: no limit.
    pub mem: Option<(u64, crate::rolling::MemCost)>,
    /// agent 0.3.4: a GPU job (PROTOCOL 13.1): `max_search` is the search threads the workers share (`max_search` / N
    /// each, `search_cap`), inference threads follow them (one session each; the job line leaves them to the worker),
    /// one or two groups a worker (`rolling::GPU_MAX_GROUPS`), batches up to `batch_max`.
    #[serde(default)]
    pub gpu: bool,
    /// agent 0.3.4: a GPU job's video memory plan: the budget of its workers (MB, `rolling::vram_budget`) and a worker's
    /// cost; each worker's base and its biggest batch together within its share. None: not planned (a CPU job).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vram: Option<(u64, crate::rolling::VramCost)>,
    /// agent 0.3.4: a GPU job's target games in flight in all (`rolling::gpu_target`): a crew of N starts with it shared
    /// (`target_conc`, rounded up to a multiple of `GPU_CONC_STEP`), and a layout the memory plan held back is lifted to
    /// it, not past it (`top_conc`). 0: none (a CPU job: its bounds are the target).
    #[serde(default)]
    pub target: u64,
    /// agent 0.3.12: a DirectML job (PROTOCOL 13.1 **DirectML**): its start is DirectML's (`dml_start`, any vendor) and
    /// each worker's share of an "auto" target is at most `rolling::DML_CONC` (`target_conc`).
    #[serde(default)]
    pub dml: bool,
    /// agent 0.3.17 (PROTOCOL 18.3): the freshness cap - the job's games in flight in all at most this (`fresh.rs`: the
    /// run's `fresh_s` x the lease's leaves/s / the p75 game's leaves, agent 0.3.18; at least `fresh::floor`); a capped crew runs fewer workers
    /// rather than workers of fewer than `fresh::MIN_PER_WORKER_*` games each. None: no cap. Never remembered: the
    /// entry's layouts are compared and stored without it (`uncapped`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fresh: Option<u64>,
}

/// agent 0.3.4: a GPU crew's share of the target games in flight is rounded up to a multiple of this (1,024 for three
/// workers: 384 each, as measured).
pub const GPU_CONC_STEP: u64 = 64;
/// agent 0.3.4: a GPU job's games in flight in all may go this far over its target (the tuner's games-in-flight moves; a
/// crew's rounded-up shares): agent 0.3.7: 2,048 for a target of 1,024 (0.3.4-0.3.6: 1.5 times) - the 4070 Ti PC's
/// fastest crew of 2026-11-20 ran 3 x 1,150 in two groups, 1.7 times its target of 2,048 (`rolling::GPU_SPLIT_MIN_ROWS`).
pub const GPU_CONC_OVER: f64 = 2.0;
/// agent 0.3.5: a GPU crew starts when it holds at least this share of the target in all (two workers of 506 on the 4070
/// Ti PC at a 9.0 GB cap: 1,012 of 1,024; 0.3.4 asked for all of it and fell back to one worker).
pub const GPU_START_SHARE: f64 = 0.8;
/// agent 0.3.4: a GPU worker's batch has at most this many rows (the worker answers bigger batches in pieces of 1,024
/// rows, PROTOCOL 8).
pub const GPU_BATCH_MAX: u64 = 2048;

impl Space {
    /// The space of a job on a platform: its bounds (the owner's `cpu_threads` caps search and inference threads each,
    /// as the worker's default `infer_threads` = `threads` always did), whether sessions vary, the games in flight in
    /// all, the worker processes (the owner's `workers`, else 1 to what the cores, the RAM and the memory plan allow).
    pub fn new(d: &Defaults, cpu_threads: Option<u32>, workers: Option<u32>, conc_min: u64, conc_max: u64, mem: Option<(u64, crate::rolling::MemCost)>) -> Space {
        let (max_search, max_infer) = match cpu_threads {
            Some(n) => (n.max(1), n.max(1)),
            None => (d.max_search.max(1), d.max_infer.max(1)),
        };
        let mut s = Space {
            max_search,
            max_infer,
            sessions: d.sessions_always,
            conc_min,
            conc_max,
            batch_min: d.batch_min.max(1),
            batch_max: d.batch_max.max(d.batch_min).max(1),
            workers_min: 1,
            workers_max: 1,
            mem,
            gpu: false,
            vram: None,
            target: 0,
            dml: false,
            fresh: None,
        };
        s.bound_workers(workers, d.max_workers);
        s
    }

    /// agent 0.3.4: the space of a GPU job (PROTOCOL 13.1): `threads` search threads shared by its workers (the owner's
    /// `cpu_threads`, else `rolling::GPU_THREADS_PER_CORE` per physical core), `target` games in flight in all (the
    /// bench's, `rolling::gpu_target`; a run's number: fixed), from half of it to `GPU_CONC_OVER` times it ("auto"),
    /// batches up to `GPU_BATCH_MAX` rows in one or two groups, the workers (the owner's `workers`, else 1 to `max_workers`)
    /// as many as the memory plan `mem` and the video memory plan `vram` hold with the fewest games in flight allowed.
    pub fn new_gpu(threads: u32, workers: Option<u32>, max_workers: u32, target: u64, fixed: bool, mem: Option<(u64, crate::rolling::MemCost)>, vram: Option<(u64, crate::rolling::VramCost)>) -> Space {
        let target = target.clamp(1, crate::rolling::MAX_INFLIGHT);
        let (conc_min, conc_max) = if fixed { (target, target) } else { ((target / 2).max(1), ((target as f64 * GPU_CONC_OVER) as u64).min(crate::rolling::MAX_INFLIGHT).max(target)) };
        let mut s = Space {
            max_search: threads.max(1),
            max_infer: threads.max(1),
            sessions: false,
            conc_min,
            conc_max,
            batch_min: 1,
            batch_max: GPU_BATCH_MAX,
            workers_min: 1,
            workers_max: 1,
            mem,
            gpu: true,
            vram,
            target,
            dml: false,
            fresh: None,
        };
        s.bound_workers(workers, max_workers);
        s
    }

    /// agent 0.3.12: the space of a DirectML GPU job (PROTOCOL 13.1 **DirectML**), any vendor: `new_gpu`'s, with
    /// `target` from `rolling::dml_target`, each worker's share of an "auto" target at most `rolling::DML_CONC`
    /// (`target_conc`), its games in flight in all ("auto") from `rolling::DML_MIN_CONC` (so one worker's start is
    /// DirectML's 1,152, not half the crew's target) to `GPU_CONC_OVER` times the target, and DirectML's start
    /// (`dml_start`).
    pub fn new_dml(threads: u32, workers: Option<u32>, max_workers: u32, target: u64, fixed: bool, mem: Option<(u64, crate::rolling::MemCost)>, vram: Option<(u64, crate::rolling::VramCost)>) -> Space {
        let mut s = Space::new_gpu(threads, workers, max_workers, target, fixed, mem, vram);
        s.dml = true;
        if !fixed {
            s.conc_min = crate::rolling::DML_MIN_CONC.min(s.target).max(1);
        }
        s.bound_workers(workers, max_workers);
        s
    }

    /// agent 0.3.16: a CUDA crew's floor (`CUDA_CREW_MIN_WORKERS`) where `n` workers each keep `rolling::DML_MIN_CONC`
    /// games in flight under the memory plans and the bounds allow it; else the bounds as they are.
    pub fn cuda_crew_floor(&mut self) {
        let n = CUDA_CREW_MIN_WORKERS;
        let holds = self.mem_room(n).is_none_or(|r| r >= crate::rolling::DML_MIN_CONC) && self.fits(n);
        if holds && self.workers_max >= n {
            self.workers_min = self.workers_min.max(n);
        }
    }

    /// The worker bounds: the owner's `workers` (`set`), else 1 to `max` (the cores' and RAM's), never more than the
    /// memory plans hold (`fits`).
    pub fn bound_workers(&mut self, set: Option<u32>, max: u32) {
        let fits = (1..=MAX_WORKERS_JSON).take_while(|&n| self.fits(n)).last().unwrap_or(1);
        let (lo, hi) = match set {
            Some(n) => (n.clamp(1, MAX_WORKERS).min(fits), n.clamp(1, MAX_WORKERS).min(fits)),
            None => (1, max.clamp(1, MAX_WORKERS).min(fits)),
        };
        (self.workers_min, self.workers_max) = (lo.max(1), hi.max(1));
    }

    /// `n` workers fit: the memory plan holds each one's base and its share of the fewest games in flight allowed; a GPU
    /// job's video memory plan holds each one's base and that share in its batches (two groups at most), and each worker
    /// gets a search thread.
    fn fits(&self, n: u32) -> bool {
        let need = self.conc_min.max(n as u64) as u128;
        let mem = self.mem_room(n).is_none_or(|room| room as u128 * n as u128 >= need);
        let vram = self.vram_rows(n).is_none_or(|rows| rows > 0 && (rows * crate::rolling::GPU_MAX_GROUPS) as u128 * n as u128 >= need);
        mem && vram && (!self.gpu || n <= self.max_search)
    }

    /// agent 0.3.4: the rows one of `n` workers' batches may have under the video memory plan (None: not planned).
    pub fn vram_rows(&self, n: u32) -> Option<u64> {
        let (budget, cost) = self.vram?;
        Some(crate::rolling::vram_rows(budget, n, cost))
    }

    /// One of `n` workers' search threads at most: a GPU job's share of its total, else `budget` of the cap.
    pub fn search_cap(&self, n: u32) -> u32 {
        if self.gpu {
            (self.max_search / n.max(1)).max(1)
        } else {
            budget(self.max_search.clamp(1, MAX_THREADS), n)
        }
    }

    fn infer_cap(&self, n: u32) -> u32 {
        if self.gpu {
            self.search_cap(n)
        } else {
            budget(self.max_infer.max(1), n)
        }
    }

    /// agent 0.3.4: a GPU job's games in flight per worker for a crew of `n`: its share of the target (rounded up to a
    /// multiple of `GPU_CONC_STEP` for N > 1: 1,024 is 1 x 1,024, 2 x 512, 3 x 384).
    pub fn target_conc(&self, n: u32) -> u64 {
        let n = n.max(1) as u64;
        // (agent 0.3.12: a DirectML job's "auto" share is DirectML's per worker - 1,152 for one, two or three of 3,456 -,
        // a run's number is shared as any GPU job's)
        if self.dml && self.conc_min < self.conc_max {
            return (self.target / n).clamp(1, crate::rolling::DML_CONC);
        }
        if n > 1 {
            self.target.div_ceil(n).div_ceil(GPU_CONC_STEP) * GPU_CONC_STEP
        } else {
            self.target.max(1)
        }
    }

    /// agent 0.3.4: a GPU job's start for `n` workers whose full batch is `batch` rows: each its share of the target
    /// (`target_conc`) in batches of at most `batch` rows (`rolling::gpu_groups`) and its share of the search threads,
    /// made valid (the memory plans may cut its games in flight).
    pub fn gpu_layout(&self, n: u32, batch: u64) -> Layout {
        let n = n.max(1);
        let conc = self.target_conc(n);
        let threads = self.search_cap(n);
        let l = self.fit(Layout { workers: n, threads, infer_workers: 1, infer_threads: threads, groups: crate::rolling::gpu_groups(conc, batch), conc });
        // (agent 0.3.5: the groups of the games in flight the memory plans leave - 0.3.4 kept the target's: 139 games in
        // flight in 2 groups of 70 rows on the 4070 Ti PC, where one batch of 139 is the rule's)
        self.fit(Layout { groups: crate::rolling::gpu_groups(l.conc, batch), ..l })
    }

    /// agent 0.3.4: a GPU job's starting crew (PROTOCOL 13.1): on an NVIDIA GPU the most workers (within the bounds) whose
    /// start holds the target games in flight in all (agent 0.3.5: `GPU_START_SHARE` of it) - three of 384 or two of 512 for a target of 1,024 where the video
    /// memory and the memory plan allow, else one; on other GPUs one (the tuner tries more). Agent 0.3.12: a DirectML job,
    /// whatever the vendor, DirectML's start (`dml_start`).
    pub fn gpu_start(&self, nvidia: bool, batch: u64) -> Layout {
        if self.dml {
            return self.dml_start();
        }
        let lo = self.workers_min.max(1);
        let hi = self.workers_max.max(lo);
        if lo == hi || !nvidia {
            return self.gpu_layout(lo, batch);
        }
        (lo..=hi).rev().map(|n| self.gpu_layout(n, batch)).find(|l| l.total_conc() as f64 >= self.target as f64 * GPU_START_SHARE).unwrap_or_else(|| self.gpu_layout(lo, batch))
    }

    /// agent 0.3.12 (PROTOCOL 13.1 **DirectML**): a DirectML job's starting crew, on any vendor's GPU: `rolling::dml_crew`
    /// within the bounds - the most workers (at most `workers_max`) each of whose start (`gpu_layout`: its share of the
    /// target, at most `rolling::DML_CONC`, in batches of at most `rolling::DML_ROWS` rows, cut by the memory plan and the
    /// video memory plan) keeps `rolling::DML_MIN_CONC` games in flight - where memory holds less, fewer games each first,
    /// then fewer workers -, else the fewest workers allowed with what they hold (the owner's fixed count: that many). The
    /// tuner searches from it as from any start.
    pub fn dml_start(&self) -> Layout {
        let lo = self.workers_min.max(1);
        let hi = self.workers_max.max(lo);
        let rows = crate::rolling::DML_ROWS;
        if lo == hi {
            return self.gpu_layout(lo, rows);
        }
        let room = |n: u32| Some(self.gpu_layout(n.clamp(lo, hi), rows).conc);
        let (n, _) = crate::rolling::dml_crew(self.target, self.conc_min >= self.conc_max, hi, &room);
        self.gpu_layout(n.clamp(lo, hi), rows)
    }

    /// The games in flight one of `n` workers may run under the memory plan (None: no limit): (`PLAN_SHARE` x the
    /// limit / `n` - base) / per game, at least 1.
    pub fn mem_room(&self, n: u32) -> Option<u64> {
        let (limit, cost) = self.mem?;
        let share = (limit as f64 * crate::rolling::PLAN_SHARE / n.max(1) as f64) as u64;
        Some((share.saturating_sub(cost.base_mb) / cost.per_game_mb.max(1)).max(1))
    }

    /// The most games in flight one of `n` workers may run: its part of the job's bounds and the memory plan (a GPU job:
    /// at most its share of the target, `target_conc`, and what its video memory holds in two groups).
    pub fn top_conc(&self, n: u32) -> u64 {
        let mut cmax = (self.conc_max.max(self.conc_min).max(1) / n.max(1) as u64).max(1);
        if self.target > 0 {
            cmax = cmax.min(self.target_conc(n)).max(1);
        }
        if let Some(rows) = self.vram_rows(n) {
            cmax = cmax.min(rows * crate::rolling::GPU_MAX_GROUPS).max(1);
        }
        self.mem_room(n).map_or(cmax, |r| r.min(cmax))
    }

    /// The groups the games in flight allow: batches within the bounds where possible, at least one, at most one per
    /// game.
    fn groups_range(&self, conc: u64) -> (u64, u64) {
        let lo = conc.div_ceil(self.batch_max).max(1);
        let mut hi = (conc / self.batch_min).max(1).max(lo).min(conc.max(1));
        if self.gpu {
            hi = hi.clamp(1, crate::rolling::GPU_MAX_GROUPS);
        }
        (lo.min(hi), hi)
    }

    /// A layout made valid: the workers within bounds; at least 1 of everything, one session without the feature, search
    /// threads and inference threads within their caps per worker (`budget`; inference gives threads per session
    /// before sessions), each worker's games in flight within its part of the job's bounds and the memory plan, the
    /// batches within bounds where the games in flight allow.
    pub fn fit(&self, l: Layout) -> Layout {
        let u = self.fit_within(l, None);
        let Some(cap) = self.fresh.map(|c| c.max(1)).filter(|&c| u.total_conc() > c) else { return u };
        // agent 0.3.17 (PROTOCOL 18.3): over the freshness cap - fewer workers where each would keep fewer than its
        // minimum (never below the bounds' least), each worker's share of the cap; a GPU worker's groups are the rule's for
        // its games (two once each keeps `rolling::GPU_SPLIT_MIN_ROWS` rows), a CPU worker keeps its groups where the
        // batches allow (as where the memory plan cuts its games, agent 0.3.3)
        let per_min = if self.gpu { crate::fresh::MIN_PER_WORKER_GPU } else { crate::fresh::MIN_PER_WORKER_CPU };
        let w = ((cap / per_min).clamp(1, MAX_WORKERS_JSON as u64) as u32).clamp(self.workers_min.max(1), u.workers.max(self.workers_min).max(1));
        let conc = (cap / w as u64).max(1);
        let mut n = if w == u.workers { u } else { self.rescale(&u, w) };
        n.groups = if self.gpu { crate::rolling::gpu_groups(conc, GPU_BATCH_MAX) } else { u.groups };
        n.conc = conc;
        self.fit_within(n, Some(conc))
    }

    /// This space without the freshness cap: where the entry's layouts are compared and what it remembers.
    pub fn uncapped(&self) -> Space {
        Space { fresh: None, ..*self }
    }

    /// agent 0.3.17: the freshness cap holds `l` back (it runs fewer games in flight than without the cap).
    pub fn capped(&self, l: &Layout) -> bool {
        self.fresh.is_some() && self.fit(*l).total_conc() < self.uncapped().fit(*l).total_conc()
    }

    /// `l` with `w` workers: each kind of thread keeps its share of its budget for the new count (as the search's worker
    /// move, `neighbour`).
    fn rescale(&self, l: &Layout, w: u32) -> Layout {
        let scale = |v: u32, infer: bool| {
            let (a, b) = if infer { (self.infer_cap(w), self.infer_cap(l.workers)) } else { (self.search_cap(w), self.search_cap(l.workers)) };
            ((v as f64 * a as f64 / b.max(1) as f64).round() as u32).max(1)
        };
        let mut n = Layout { workers: w, threads: scale(l.threads, false), ..*l };
        let infer = scale(l.infer_total(), true);
        if self.sessions {
            n.infer_workers = ((infer as f64 / l.infer_threads.max(1) as f64).round() as u32).max(1);
        } else {
            n.infer_threads = infer;
        }
        if self.gpu {
            n.infer_threads = n.threads;
        }
        n
    }

    /// `fit` within the bounds, each worker's games in flight at most `ceiling` (the freshness cap's share).
    fn fit_within(&self, l: Layout, ceiling: Option<u64>) -> Layout {
        let workers = l.workers.clamp(self.workers_min.max(1), self.workers_max.max(self.workers_min).max(1));
        let (max_search, max_infer) = (self.search_cap(workers), self.infer_cap(workers));
        let n = workers as u64;
        let (cmin, cmax) = (self.conc_min.max(1).div_ceil(n), (self.conc_max.max(self.conc_min).max(1) / n).max(1));
        let mut conc = l.conc.clamp(cmin.min(cmax), cmax);
        if let Some(room) = self.mem_room(workers) {
            conc = conc.min(room);
        }
        if let Some(c) = ceiling {
            conc = conc.min(c.max(1));
        }
        let threads = l.threads.clamp(1, max_search);
        let mut l = Layout {
            workers,
            threads,
            infer_workers: if self.sessions { l.infer_workers.clamp(1, MAX_SESSIONS) } else { 1 },
            // (a GPU job: the worker's own default, its search threads)
            infer_threads: if self.gpu { threads } else { l.infer_threads.clamp(1, MAX_THREADS) },
            groups: l.groups.max(1),
            conc: conc.max(1),
        };
        while l.infer_total() > max_infer {
            if l.infer_threads > 1 {
                l.infer_threads -= 1;
            } else {
                l.infer_workers -= 1;
            }
        }
        let (lo, hi) = self.groups_range(l.conc);
        l.groups = l.groups.clamp(lo, hi);
        // agent 0.3.4: a GPU job's batches within its video memory: more groups first (up to two), then fewer games
        if let Some(rows) = self.vram_rows(workers) {
            let rows = rows.max(1);
            if l.batch() > rows {
                let (_, hi) = self.groups_range(l.conc);
                l.groups = l.conc.div_ceil(rows).clamp(l.groups, hi);
                l.conc = l.conc.min(rows * l.groups).max(1);
                let (lo, hi) = self.groups_range(l.conc);
                l.groups = l.groups.clamp(lo, hi);
            }
        }
        l
    }
}

/// The coordinates of the search, in the order they are tried.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Coord {
    /// rev 19 (PROTOCOL 13.5): worker processes (one; the job's games in flight and rows per batch kept, each kind of
    /// thread scaled with its budget for the new count).
    Workers,
    /// Rows per batch (`up`: bigger: two thirds of the groups; else half again as many).
    Batch,
    /// Search threads (an eighth of their cap, at least 1).
    Search,
    /// Threads per inference session (one).
    InferThreads,
    /// Inference sessions (one; their threads kept). Only for a worker that honours `infer_workers`.
    Sessions,
    /// Games in flight doubled / halved (rows per batch kept); "auto" only.
    Conc,
}

pub const COORDS: [Coord; 6] = [Coord::Workers, Coord::Batch, Coord::Search, Coord::InferThreads, Coord::Sessions, Coord::Conc];

/// `l`'s neighbour along `c`, valid in `s`; None when there is none (a bound, or the same layout). Thread moves past a
/// cap are no neighbours (never cut back into another shape); batch, games-in-flight and worker moves are kept within
/// bounds (a worker move rescales each worker's threads to the new count's budget).
pub fn neighbour(l: &Layout, c: Coord, up: bool, s: &Space) -> Option<Layout> {
    let l = s.fit(*l);
    let mut n = l;
    match c {
        Coord::Workers => {
            let w = if up { l.workers + 1 } else { l.workers.checked_sub(1).filter(|&w| w >= 1)? };
            if w < s.workers_min || w > s.workers_max {
                return None;
            }
            // the job's games in flight and rows per batch kept; each kind of thread keeps its share of its budget
            let scale = |v: u32, infer: bool| {
                let (a, b) = if infer { (s.infer_cap(w), s.infer_cap(l.workers)) } else { (s.search_cap(w), s.search_cap(l.workers)) };
                ((v as f64 * a as f64 / b.max(1) as f64).round() as u32).max(1)
            };
            n.workers = w;
            n.conc = (l.total_conc() / w as u64).max(1);
            n.groups = n.conc.div_ceil(l.batch().max(1)).max(1);
            n.threads = scale(l.threads, false);
            let infer = scale(l.infer_total(), true);
            if s.sessions {
                n.infer_workers = ((infer as f64 / l.infer_threads.max(1) as f64).round() as u32).max(1);
            } else {
                n.infer_threads = infer;
            }
        }
        // (a GPU job's inference threads follow its search threads, one session: no coordinates of their own)
        Coord::InferThreads | Coord::Sessions if s.gpu => return None,
        Coord::Batch => n.groups = if up { (l.groups * 2 / 3).max(1) } else { (l.groups * 3).div_ceil(2) },
        Coord::Search => {
            let step = (s.max_search / 8).max(1);
            n.threads = if up { l.threads + step } else { l.threads.checked_sub(step).filter(|&t| t >= 1)? };
        }
        Coord::InferThreads => n.infer_threads = if up { l.infer_threads + 1 } else { l.infer_threads.checked_sub(1).filter(|&t| t >= 1)? },
        Coord::Sessions => {
            if !s.sessions {
                return None;
            }
            n.infer_workers = if up { l.infer_workers + 1 } else { l.infer_workers.checked_sub(1).filter(|&w| w >= 1)? };
        }
        Coord::Conc => {
            if s.conc_min >= s.conc_max {
                return None;
            }
            let batch = l.batch();
            // (each worker's games in flight within its part of the job's bounds - agent 0.3.4: 0.3.1-0.3.3 clamped them to
            // the job's, so a crew's move went nowhere)
            let w = l.workers.max(1) as u64;
            let (cmin, cmax) = (s.conc_min.max(1).div_ceil(w), (s.conc_max / w).max(1));
            n.conc = (if up { l.conc * 2 } else { l.conc / 2 }).clamp(cmin.min(cmax), cmax);
            n.groups = n.conc.div_ceil(batch.max(1)).max(1);
        }
    }
    if s.gpu {
        // (a GPU job's inference threads follow its search threads)
        n.infer_threads = n.threads;
    }
    let f = s.fit(n);
    let thread_move = matches!(c, Coord::Search | Coord::InferThreads | Coord::Sessions);
    if (thread_move && f != n) || f == l {
        return None;
    }
    Some(f)
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
/// generation of the same run keeps it; another architecture or other settings change it. Rev 20 (PROTOCOL 14.4): a model
/// variant's name too (a variant graph is laid out apart from the default graph; without one the fingerprint is as before).
/// Agent 0.3.14: without the settings' `setting_classes` - the trainer's list of every setting it knows and its class,
/// which changes with the trainer's version and not with how the worker plays: a run forked by a newer trainer, its play
/// settings the same, got a new fingerprint and searched its layout again from the start (2026-11-21: legacy-meta3-r1's
/// list lacks 7 trainer settings its forks' have). Keys remembered with the fingerprint of before are found by
/// `net_fingerprint_legacy` (`Cache::find`).
pub fn net_fingerprint(template: &Map<String, Value>, model_size: Option<u64>, variant: Option<&str>) -> String {
    fingerprint(template, model_size, variant, true)
}

/// agent 0.3.14: the fingerprint of agents 0.3.1-0.3.13 (with the settings' `setting_classes`), which their keys in
/// `state/tuning.json` were remembered by (`Cache::find`).
pub fn net_fingerprint_legacy(template: &Map<String, Value>, model_size: Option<u64>, variant: Option<&str>) -> String {
    fingerprint(template, model_size, variant, false)
}

fn fingerprint(template: &Map<String, Value>, model_size: Option<u64>, variant: Option<&str>, drop_classes: bool) -> String {
    let mut t = template.clone();
    for k in ["gid_base", "games", "seed", "first", "count"] {
        t.remove(k);
    }
    if drop_classes {
        if let Some(Value::Object(s)) = t.get_mut("settings") {
            s.remove("setting_classes");
        }
    }
    let mut text = format!("{}|{}", zerofarm_common::protocol::canonical_json(&Value::Object(t)), model_size.map_or(-1, |s| (s >> 20) as i64));
    if let Some(v) = variant {
        text.push_str(&format!("|variant:{v}"));
    }
    zerofarm_common::hash::sha256_hex(text.as_bytes())[..16].to_string()
}

/// The CPU's fingerprint: brand, cores, threads, performance and efficiency cores, RAM in GiB.
pub fn hw_fingerprint(p: &zerofarm_common::protocol::Profile) -> String {
    format!("{} | {}c {}t {}p {}e | {} GB", p.cpu, p.cores, p.threads, p.perf_cores, p.eff_cores, p.ram_mb >> 10)
}

/// agent 0.3.4: a GPU job's hardware: the CPU's fingerprint, the GPU (vendor, name, dedicated memory, driver) and the
/// execution provider - a GPU job's result is remembered per GPU and driver (PROTOCOL 13.3).
pub fn gpu_fingerprint(p: &zerofarm_common::protocol::Profile, g: &zerofarm_common::protocol::Gpu, provider: &str) -> String {
    format!("{} | gpu {} {} {} MB driver {} | {provider}", hw_fingerprint(p), g.vendor, g.name, g.vram_mb, g.driver)
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
    /// agent 0.3.3: other programs' mean share of the CPUs over its trial (None: not recorded, agents 0.3.1-0.3.2).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub load: Option<f64>,
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
    /// agent 0.3.3: the bounds of the lease that used it last (its memory plan, the owner's settings, the worker's
    /// features). Its layouts are compared as `Space::fit` makes them there - what the workers run: a remembered layout
    /// the memory plan no longer holds is its fitted one (0.3.2 kept an incumbent of 2 x 958 games in flight from a
    /// lease planned at 13 MB a game while the next lease's plan, at 16 MB, held 2 x 778; the fitted incumbent's
    /// measurements and discards were never the incumbent's, so it was "unmeasured" for ever and handed over to again
    /// and again).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub space: Option<Space>,
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
            space: None,
        }
    }

    /// The bounds the entry's layouts are compared in from now on (the lease's space).
    pub fn bind(&mut self, s: &Space) {
        // (agent 0.3.17: never the freshness cap - a lease's, from its run's pace and its own rate)
        self.space = Some(s.uncapped());
    }

    /// `l` as it runs in `s` (else in the bound space; as it is without one). Agent 0.3.17: without the freshness cap -
    /// measurements belong to the layouts as they ran, the cap is the lease's (a capped layout's numbers are its own).
    fn fitted(&self, l: &Layout, s: Option<&Space>) -> Layout {
        match s.or(self.space.as_ref()) {
            Some(s) => s.uncapped().fit(*l),
            None => *l,
        }
    }

    /// `a` and `b` are the same layout where they run.
    fn same(&self, a: &Layout, b: &Layout, s: Option<&Space>) -> bool {
        self.fitted(a, s) == self.fitted(b, s)
    }

    /// The measurements of `l` (of one epoch, or all), compared where they run.
    fn of(&self, l: &Layout, epoch: Option<u32>, s: Option<&Space>) -> Vec<&Measured> {
        let k = self.fitted(l, s);
        self.measured.iter().filter(|m| self.fitted(&m.layout, s) == k && epoch.is_none_or(|e| m.epoch == e)).collect()
    }

    fn rates(&self, l: &Layout, epoch: Option<u32>, s: Option<&Space>) -> Vec<f64> {
        self.of(l, epoch, s).iter().map(|m| m.rate).collect()
    }

    /// Other programs' mean load over `l`'s measurements that recorded it (this check's during a check).
    fn load_of(&self, l: &Layout, s: Option<&Space>) -> Option<f64> {
        let epoch = (self.state == State::Checking).then_some(self.epoch);
        let v: Vec<f64> = self.of(l, epoch, s).iter().filter_map(|m| m.load).collect();
        (!v.is_empty()).then(|| v.iter().sum::<f64>() / v.len() as f64)
    }

    /// `a` was measured with clearly less load from other programs than `b` (`LOAD_SIMILAR`): faster numbers of `a` may
    /// be the CPU it didn't share, not its layout.
    fn quieter(&self, a: &Layout, b: &Layout, s: Option<&Space>) -> bool {
        matches!((self.load_of(a, s), self.load_of(b, s)), (Some(x), Some(y)) if x + LOAD_SIMILAR < y)
    }

    fn mean_in(&self, l: &Layout, s: Option<&Space>) -> Option<f64> {
        let r = self.rates(l, (self.state == State::Checking).then_some(self.epoch), s);
        (!r.is_empty()).then(|| r.iter().sum::<f64>() / r.len() as f64)
    }

    /// The mean rate of `l` (this check's measurements only during a check), where the entry's layouts run.
    pub fn mean(&self, l: &Layout) -> Option<f64> {
        self.mean_in(l, None)
    }

    fn mean_all(&self, l: &Layout, s: Option<&Space>) -> Option<f64> {
        let r = self.rates(l, None, s);
        (!r.is_empty()).then(|| r.iter().sum::<f64>() / r.len() as f64)
    }

    fn discarded(&self, l: &Layout, s: Option<&Space>) -> u32 {
        let k = self.fitted(l, s);
        self.discards.iter().filter(|(x, _)| self.fitted(x, s) == k).map(|(_, n)| *n).sum()
    }

    fn runner_up_in(&self, s: Option<&Space>) -> Option<Layout> {
        let inc = self.fitted(&self.incumbent, s);
        let mut best: Option<(Layout, f64)> = None;
        for m in &self.measured {
            let l = self.fitted(&m.layout, s);
            if l == inc || best.is_some_and(|(b, _)| b == l) {
                continue;
            }
            let r = self.mean_all(&l, s).unwrap_or(0.0);
            if best.is_none_or(|(_, b)| r > b) {
                best = Some((l, r));
            }
        }
        best.map(|(l, _)| l)
    }

    /// The best layout measured other than the incumbent (all measurements).
    pub fn runner_up(&self) -> Option<Layout> {
        self.runner_up_in(None)
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
            if self.rates(&n, Some(0), Some(s)).is_empty() && self.discarded(&n, Some(s)) < MAX_DISCARDS {
                return Some(n);
            }
        }
        None
    }

    /// The layout to measure next (None: nothing to measure now - tuned, or a check not due).
    pub fn plan(&self, s: &Space) -> Option<Layout> {
        let sp = Some(s);
        match self.state {
            State::Searching => {
                if self.rates(&self.incumbent, Some(0), sp).is_empty() && self.discarded(&self.incumbent, sp) < MAX_DISCARDS * 2 {
                    return Some(s.fit(self.incumbent));
                }
                self.next_candidate(s)
            }
            State::Final => Some(s.fit(self.incumbent)),
            State::Tuned => None,
            State::Checking => {
                let fresh = |l: &Layout| !self.rates(l, Some(self.epoch), sp).is_empty();
                match self.runner_up_in(sp) {
                    Some(r) if !fresh(&r) && self.discarded(&r, sp) < MAX_DISCARDS => Some(s.fit(r)),
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

    /// agent 0.3.3: the memory plan grew from `old` to `new` (a lower cost per game measured, rolling.rs
    /// `Learned::steady`): the incumbent and the start, where the old bounds held their games in flight back (at the
    /// most a worker could run there), get the most the new bounds hold; a tuner's choice of fewer games stays.
    pub fn lift_conc(&mut self, old: &Space, new: &Space) {
        // (agent 0.3.17: without the freshness cap - it isn't the memory plan's, and the entry never remembers it)
        let (old, new) = (&old.uncapped(), &new.uncapped());
        for l in [&mut self.incumbent, &mut self.start] {
            let f = old.fit(*l);
            if f.conc >= old.top_conc(f.workers) {
                l.conc = l.conc.max(new.top_conc(f.workers));
            }
        }
    }

    /// A valid measurement of `l` (`load`: other programs' mean share of the CPUs over its trial).
    pub fn record(&mut self, l: Layout, rate: f64, load: Option<f64>, phase: Option<Phase>, s: &Space, now: i64) {
        self.bind(s);
        let sp = Some(s);
        if self.phase.is_none() {
            self.phase = phase;
        }
        let epoch = if self.state == State::Checking { self.epoch } else { 0 };
        self.measured.push(Measured { layout: l, rate, t: now, epoch, load });
        self.t = now;
        if self.state == State::Searching && !self.same(&l, &self.incumbent, sp) {
            self.trials += 1;
            if let (Some(c), Some(i)) = (self.mean_in(&l, sp), self.mean_in(&self.incumbent, sp)) {
                if c >= GAIN * i && self.quieter(&l, &self.incumbent, sp) {
                    log::info!(
                        "tuning: {} measured {:.1} % faster than {}, but with less load from other programs ({:.0} % vs {:.0} %): not comparable, the incumbent stays",
                        l.describe(),
                        (c / i - 1.0) * 100.0,
                        self.incumbent.describe(),
                        self.load_of(&l, sp).unwrap_or(0.0) * 100.0,
                        self.load_of(&self.incumbent, sp).unwrap_or(0.0) * 100.0
                    );
                } else if c >= GAIN * i {
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
        self.bind(s);
        match self.discards.iter_mut().find(|(x, _)| *x == l) {
            Some((_, n)) => *n += 1,
            None => self.discards.push((l, 1)),
        }
        self.t = now;
        self.advance(s, now);
    }

    /// State changes once the current step has nothing left to measure.
    pub fn advance(&mut self, s: &Space, now: i64) {
        let sp = Some(s);
        loop {
            match self.state {
                State::Searching if self.plan(s).is_none() => {
                    self.state = if !self.same(&self.incumbent, &self.start, sp) && self.rates(&self.incumbent, Some(0), sp).len() < 2 { State::Final } else { State::Tuned };
                    if self.state == State::Tuned {
                        self.conclude(s, now);
                    }
                }
                State::Final if self.rates(&self.incumbent, Some(0), sp).len() >= 2 || self.discarded(&self.incumbent, sp) >= MAX_DISCARDS => {
                    self.state = State::Tuned;
                    self.conclude(s, now);
                }
                State::Checking if self.plan(s).is_none() => {
                    if let Some(r) = self.runner_up_in(sp) {
                        let (a, b) = (self.rates(&r, Some(self.epoch), sp), self.rates(&self.incumbent, Some(self.epoch), sp));
                        if let (Some(&a), Some(&b)) = (a.last(), b.last()) {
                            if a >= GAIN * b && !self.quieter(&r, &self.incumbent, sp) {
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

    /// The search's end: the incumbent stays only if its mean is still `GAIN` over the start's (with load no lighter
    /// than the start's, `LOAD_SIMILAR`).
    fn conclude(&mut self, s: &Space, now: i64) {
        let sp = Some(s);
        if !self.same(&self.incumbent, &self.start, sp) {
            if let (Some(i), Some(st)) = (self.mean_all(&self.incumbent, sp), self.mean_all(&self.start, sp)) {
                if i < GAIN * st || self.quieter(&self.incumbent, &self.start, sp) {
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

    /// The incumbent as the workers run it (in the bound space).
    pub fn incumbent_run(&self) -> Layout {
        self.fitted(&self.incumbent, None)
    }

    /// The incumbent's mean rate (all measurements).
    pub fn rate(&self) -> Option<f64> {
        self.mean_all(&self.incumbent, None)
    }

    /// What `ZeroFarm status` / the tray say about it.
    pub fn describe(&self) -> String {
        let rate = self.rate().map(|r| format!(", {} leaves/s", thousands(r.round() as u64))).unwrap_or_default();
        let inc = self.incumbent_run().describe_on(self.space.is_some_and(|s| s.gpu));
        match self.state {
            State::Tuned => format!("tuned: {inc}{rate}"),
            State::Checking => format!("weekly check of the tuned layout: {inc}{rate}"),
            _ => format!("tuning ({} of at most {} trials): {inc}{rate}", self.trials, MAX_TRIALS),
        }
    }

    /// The coordinator's report of it (PROTOCOL 13): None before the incumbent was measured. The layout is the
    /// incumbent as it ran (fitted where it was measured).
    pub fn report(&self) -> Option<Value> {
        let rate = self.rate()?;
        Some(json!({
            "run": self.run, "build": self.key.build, "runtime": self.key.runtime, "net": self.key.net,
            "layout": self.incumbent_run().to_json(), "leaves_s": rate.round(), "trials": self.measured.len(),
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

    /// agent 0.3.14: `key`'s entry, else the one agents 0.3.1-0.3.13 remembered for it under the network fingerprint
    /// `legacy_net` (`net_fingerprint_legacy`), re-keyed to `key` (saved under it from then on), so an update keeps every
    /// tuned layout.
    pub fn find(&self, key: &Key, legacy_net: &str) -> Option<Entry> {
        if let Some(e) = self.get(key) {
            return Some(e.clone());
        }
        let old = Key { net: legacy_net.to_string(), ..key.clone() };
        self.get(&old).map(|e| Entry { key: key.clone(), ..e.clone() })
    }

    /// Replaces (or adds) `e`'s key.
    pub fn put(&mut self, e: Entry) {
        self.entries.retain(|x| x.key != e.key);
        self.entries.push(e);
    }

    /// The coordinator's `tuning` (PROTOCOL 13): the most recently changed measured entries, at most `MAX_REPORTS`.
    pub fn reports(&self) -> Vec<Value> {
        let mut es: Vec<&Entry> = self.entries.iter().filter(|e| [TUNER_VERSION, GPU_TUNER_VERSION, DML_TUNER_VERSION, CUDA_CREW_TUNER_VERSION].contains(&e.key.tuner)).collect();
        es.sort_by_key(|e| std::cmp::Reverse(e.t));
        es.iter().filter_map(|e| e.report()).take(MAX_REPORTS).collect()
    }
}

/// CPU time at a point: the system's (idle, total over all CPUs) and our workers', ms. Two of them give the share of
/// the CPUs other programs used in between (a trial needs a quiet machine). agent 0.3.10: and the system's interrupt and
/// DPC time (`dpc_ms`, Windows; 0 elsewhere) - drivers' work, no program's: a GPU crew's own batches drive the GPU
/// driver's, which read as 6-11 % "other programs" on the idle 4070 Ti PC, two groups of small batches the most.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LoadMark {
    pub idle_ms: u64,
    pub total_ms: u64,
    pub ours_ms: u64,
    pub dpc_ms: u64,
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
        let drivers = now.dpc_ms.saturating_sub(self.dpc_ms);
        (busy.saturating_sub(ours).saturating_sub(drivers) as f64 / total as f64).clamp(0.0, 1.0)
    }
}

/// agent 0.3.3: other programs' load over time - each sample their share of the CPUs since the one before
/// (`LoadMark::other_since`) - judged over windows, not sample by sample: a trial may start when the mean over the last
/// trial length is at most `OTHER_LOAD_MAX`; it is discarded at once when the mean over the last `LOAD_SUSTAINED_SHARE`
/// of a window passes `OTHER_LOAD_SUSTAINED` (it couldn't end valid); and it counts when its own mean is at most
/// `OTHER_LOAD_MAX` and at most `OTHER_LOAD_RISE` over the mean before it (`judge_trial`). A burst of a background
/// daemon passes; interference that lasts, or that came with the trial, doesn't. The user's own rule - trials only while
/// the computer isn't used (input, battery, fullscreen, blocks) - is the policy's and stays strict (`policy.rs`).
#[derive(Clone, Debug, Default)]
pub struct LoadTrack {
    /// (end of the sample in seconds on the track's clock, other programs' share over it, its length in seconds)
    samples: std::collections::VecDeque<(f64, f64, f64)>,
}

impl LoadTrack {
    /// Samples older than this are dropped (seconds).
    pub const KEEP_S: f64 = 3600.0;

    /// A sample: other programs used `share` of the CPUs over the `dt` seconds up to `t`.
    pub fn push(&mut self, t: f64, dt: f64, share: f64) {
        if dt <= 0.0 || !share.is_finite() {
            return;
        }
        self.samples.push_back((t, share.clamp(0.0, 1.0), dt));
        while self.samples.front().is_some_and(|s| s.0 < t - Self::KEEP_S) {
            self.samples.pop_front();
        }
    }

    /// The time-weighted mean share over `(a, b]` and the seconds of it the samples cover (None: none).
    pub fn mean(&self, a: f64, b: f64) -> Option<(f64, f64)> {
        let (mut w, mut sum) = (0.0, 0.0);
        for &(t, share, dt) in &self.samples {
            let ov = (t.min(b) - (t - dt).max(a)).max(0.0);
            if ov > 0.0 {
                w += ov;
                sum += share * ov;
            }
        }
        (w > 0.0).then(|| (sum / w, w))
    }

    /// The last sample's share.
    pub fn last(&self) -> Option<f64> {
        self.samples.back().map(|s| s.1)
    }

    /// A trial may start at `now`: the mean over the last `baseline_s` is at most `OTHER_LOAD_MAX` (nothing measured
    /// yet: yes - the trial's own window is judged).
    pub fn quiet(&self, now: f64, baseline_s: f64) -> bool {
        self.mean(now - baseline_s, now).is_none_or(|(m, _)| m <= OTHER_LOAD_MAX)
    }

    /// The load is sustained at `now`: the mean over the last `window_s` (covered at least 90 %) passes
    /// `OTHER_LOAD_SUSTAINED` -> that mean.
    pub fn sustained(&self, now: f64, window_s: f64) -> Option<f64> {
        self.mean(now - window_s, now).filter(|&(m, w)| w >= 0.9 * window_s && m > OTHER_LOAD_SUSTAINED).map(|(m, _)| m)
    }
}

/// agent 0.3.3: a trial's load verdict: its mean `trial` over its window and the mean `before` it -> the mean to record
/// with the measurement, or why it doesn't count.
pub fn judge_trial(trial: Option<f64>, before: Option<f64>) -> Result<Option<f64>, String> {
    let Some(m) = trial else { return Ok(None) };
    if m > OTHER_LOAD_MAX {
        return Err(format!("other programs used {:.0} % of the CPUs during the trial", m * 100.0));
    }
    if let Some(b) = before.filter(|&b| m > b + OTHER_LOAD_RISE) {
        return Err(format!("other programs used {:.0} % of the CPUs during the trial, {:.0} % before it", m * 100.0, b * 100.0));
    }
    Ok(Some(m))
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
    /// agent 0.3.13: seconds the trial waited for game ids before its games in flight refilled (`hold`).
    held: f64,
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
        Probe { conc: conc.max(1), phase, warm, window, max_s: 4.0 * (warm + window) * scale + 30.0 * warm.min(1.0), last: None, refill: None, from: None, held: 0.0 }
    }

    /// agent 0.3.13: the games in flight refilled (the window may start).
    pub fn refilled(&self) -> bool {
        self.refill.is_some()
    }

    /// agent 0.3.13: the lease waited `secs` for game ids the coordinator didn't release (its in-flight limit, the run's
    /// quota) before the games in flight refilled: the trial may take that much longer (the time is the coordinator's,
    /// not the layout's), and if it measures nothing it is put off, not discarded (`held`).
    pub fn hold(&mut self, secs: f64) {
        if self.refill.is_none() && secs > 0.0 {
            self.max_s += secs;
            self.held += secs;
        }
    }

    /// agent 0.3.13: the trial waited for game ids before its games in flight refilled.
    pub fn held(&self) -> bool {
        self.held > 0.0
    }

    /// The worker's counters at `t` seconds after it started: `leaves` (cumulative), `games` (finished), `started`.
    pub fn sample(&mut self, t: f64, leaves: u64, games: u64, started: u64) -> Verdict {
        let leaves = leaves as f64;
        let conc = self.conc as f64;
        let prev = self.last.replace((t, leaves));
        if let Some((pt, pl)) = prev {
            if leaves < pl {
                // a counter that went back: not the same worker's
                return Verdict::Invalid("the worker's counters went back".into());
            }
            if t <= pt {
                // (a crew's samples at the same instant: the first counts)
                self.last = prev;
                return Verdict::Pending;
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

    /// The M4 Max's space: search and inference threads at most 16, sessions, 1,152-2,304 games in flight, 192-384 rows.
    fn space() -> Space {
        Space { max_search: 16, max_infer: 16, sessions: true, conc_min: 1152, conc_max: 2304, batch_min: 192, batch_max: 384, workers_min: 1, workers_max: 1, mem: None, gpu: false, vram: None, target: 0, dml: false, fresh: None }
    }

    fn lay(threads: u32, w: u32, t: u32, groups: u64, conc: u64) -> Layout {
        Layout { workers: 1, threads, infer_workers: w, infer_threads: t, groups, conc }
    }

    /// The shipped macOS layout on the M4 Max: the measured best region.
    fn shipped() -> Layout {
        lay(16, 5, 3, 12, 2304)
    }

    #[test]
    fn layouts_fit_their_caps() {
        let s = space();
        assert_eq!(s.fit(shipped()), shipped(), "the start is valid as it is");
        // over a cap: search threads cut, inference threads per session before sessions
        assert_eq!(s.fit(lay(36, 12, 3, 12, 2304)), lay(16, 12, 1, 12, 2304));
        assert_eq!(s.fit(lay(16, 4, 5, 12, 2304)), lay(16, 4, 4, 12, 2304));
        // without the sessions feature: one session
        let one = Space { sessions: false, ..s };
        assert_eq!(one.fit(lay(8, 4, 2, 12, 2304)).infer_workers, 1);
        // games in flight and batches within bounds
        assert_eq!(s.fit(lay(8, 2, 4, 12, 8192)).conc, 2304);
        assert_eq!(s.fit(lay(8, 2, 4, 2, 2304)).groups, 6, "at most 384 rows");
        assert_eq!(s.fit(lay(8, 2, 4, 40, 2304)).groups, 12, "at least 192 rows");
        let small = Space { conc_min: 64, conc_max: 64, ..s };
        assert_eq!(small.fit(lay(8, 2, 4, 12, 64)).groups, 1, "fewer games than a batch: one group");
        // never 0 of anything
        assert_eq!(Space { max_search: 0, max_infer: 0, ..s }.fit(lay(0, 0, 0, 0, 0)), lay(1, 1, 1, 3, 1152), "1,152 games: 3 groups of 384 rows at least");
        // Windows: batches from MIN_BATCH rows to all games in flight
        let win = Space::new(&defaults(Platform::Windows, &Hw { cores: 8, logical: 16, perf: 8, eff: 0, ram_mb: 16384 }), None, None, 16, 32, None);
        assert_eq!(win.fit(lay(8, 1, 8, 4, 32)), lay(8, 1, 8, 4, 32));
        assert_eq!(win.fit(lay(8, 1, 8, 9, 32)).groups, 4, "at least 8 rows");
        assert_eq!(win.fit(lay(9, 1, 17, 4, 32)), lay(8, 1, 8, 4, 32), "search and inference threads each at most the cores");
        // the owner's cpu_threads caps search and inference threads each
        let owner = Space::new(&defaults(Platform::MacOs, &Hw { cores: 16, logical: 16, perf: 12, eff: 4, ram_mb: 16384 }), Some(8), None, 2304, 2304, None);
        assert_eq!(owner.fit(shipped()), lay(8, 5, 1, 12, 2304));
    }

    #[test]
    fn neighbours_move_one_coordinate_and_never_past_a_cap() {
        let s = space();
        let l = shipped();
        assert_eq!(neighbour(&l, Coord::Batch, true, &s), Some(lay(16, 5, 3, 8, 2304)), "bigger batches: two thirds of the groups");
        assert_eq!(neighbour(&lay(16, 5, 3, 8, 2304), Coord::Batch, true, &s), Some(lay(16, 5, 3, 6, 2304)), "384 rows at most");
        assert_eq!(neighbour(&lay(16, 5, 3, 6, 2304), Coord::Batch, true, &s), None);
        assert_eq!(neighbour(&l, Coord::Batch, false, &s), None, "192 rows at least");
        assert_eq!(neighbour(&lay(16, 5, 3, 6, 2304), Coord::Batch, false, &s), Some(lay(16, 5, 3, 9, 2304)));
        // threads: never up past a cap, never cut back into another shape
        assert_eq!(neighbour(&l, Coord::Search, true, &s), None);
        assert_eq!(neighbour(&l, Coord::Search, false, &s), Some(lay(14, 5, 3, 12, 2304)), "an eighth of the cap");
        assert_eq!(neighbour(&l, Coord::InferThreads, true, &s), None, "5 x 4 = 20 > 16");
        assert_eq!(neighbour(&l, Coord::InferThreads, false, &s), Some(lay(16, 5, 2, 12, 2304)));
        assert_eq!(neighbour(&l, Coord::Sessions, true, &s), None, "6 x 3 = 18 > 16");
        assert_eq!(neighbour(&l, Coord::Sessions, false, &s), Some(lay(16, 4, 3, 12, 2304)));
        assert_eq!(neighbour(&lay(16, 1, 3, 12, 2304), Coord::Sessions, false, &s), None);
        let no_sessions = Space { sessions: false, ..s };
        assert_eq!(neighbour(&lay(16, 1, 3, 12, 2304), Coord::Sessions, true, &no_sessions), None);
        // games in flight: rows per batch kept, within bounds; the run's number fixed
        assert_eq!(neighbour(&l, Coord::Conc, false, &s), Some(lay(16, 5, 3, 6, 1152)));
        assert_eq!(neighbour(&l, Coord::Conc, true, &s), None, "not above 10.6's auto");
        assert_eq!(neighbour(&l, Coord::Conc, false, &Space { conc_min: 2304, ..s }), None);
    }

    /// A rate source: a peak at `best`, 15 % less per doubling away on each coordinate, and `noise` (a fixed
    /// pseudo-random sequence of +- that share).
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
            e.record(l, r, None, Some(Phase { p0: 1.0, p1: 2.0, conc: 64 }), s, 100);
            guard += 1;
            assert!(guard < 40, "the search doesn't end: {seen:?}");
        }
        (e, seen)
    }

    #[test]
    fn the_search_climbs_to_a_better_layout_within_its_budget() {
        let s = space();
        let best = lay(12, 4, 3, 8, 2304);
        let mut fake = Fake { best, noise: 0.0, n: 1 };
        let start = shipped();
        let start_rate = fake.rate(&start);
        let (e, seen) = search(start, &s, &mut fake);
        assert_eq!(e.state, State::Tuned);
        assert!(e.trials <= MAX_TRIALS, "{}", e.trials);
        assert!(seen.len() as u32 <= MAX_TRIALS + 2, "start + trials + the final re-measure: {seen:?}");
        assert_eq!(e.incumbent, best, "{seen:?}");
        assert!(fake.rate(&e.incumbent) > start_rate * 1.2);
        assert!(seen.iter().all(|l| l.threads <= 16 && l.infer_total() <= 16 && (192..=384).contains(&l.batch())), "never past a cap: {seen:?}");
        // the winner was measured twice (the final re-measure)
        assert_eq!(e.measured.iter().filter(|m| m.layout == e.incumbent).count(), 2);
        assert!(e.check_after == Some(100 + RECHECK_S));
        assert!(e.runner_up().is_some());
    }

    #[test]
    fn a_move_that_paid_is_tried_again_first() {
        let s = space();
        let start = shipped();
        let mut e = Entry::new(key(), "r", start, false, 0);
        e.record(start, 10_000.0, None, None, &s, 0);
        // fewer search threads pays: the next candidate goes on that way, before the other coordinates
        let fewer = neighbour(&start, Coord::Search, false, &s).unwrap();
        e.record(fewer, 11_000.0, None, None, &s, 0);
        assert_eq!(e.incumbent, fewer);
        assert_eq!(e.momentum, Some((2, false)), "Search is the third coordinate");
        assert_eq!(e.plan(&s), neighbour(&fewer, Coord::Search, false, &s));
    }

    #[test]
    fn the_start_stays_when_nothing_is_clearly_faster() {
        let s = space();
        let start = shipped();
        // the start is the peak: every neighbour is slower
        let mut fake = Fake { best: start, noise: 0.0, n: 1 };
        let (e, seen) = search(start, &s, &mut fake);
        assert_eq!(e.incumbent, start);
        assert_eq!(seen[0], start, "the start is measured first");
        assert!(!seen[1..].contains(&start), "no final re-measure of an unchanged start: {seen:?}");
    }

    #[test]
    fn noise_below_the_gain_never_moves_the_incumbent() {
        let s = space();
        let start = shipped();
        // a flat landscape (every layout the same) with +-1 % noise
        for seed in 1..50u64 {
            let mut e = Entry::new(key(), "r", start, false, 0);
            let mut n = seed;
            while let Some(l) = e.plan(&s) {
                n = n.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                let u = ((n >> 33) as f64 / (1u64 << 31) as f64) * 2.0 - 1.0;
                e.record(l, 10_000.0 * (1.0 + 0.01 * u), None, None, &s, 0);
            }
            assert_eq!(e.incumbent, start, "seed {seed}");
        }
    }

    #[test]
    fn a_lucky_measurement_is_undone_by_the_final_re_measure() {
        let s = space();
        let start = shipped();
        let mut e = Entry::new(key(), "r", start, false, 0);
        e.record(start, 10_000.0, None, None, &s, 0);
        // the first candidate measures 5 % faster once (luck), every other one 10 % slower
        let first = e.plan(&s).unwrap();
        e.record(first, 10_500.0, None, None, &s, 0);
        assert_eq!(e.incumbent, first);
        while let Some(l) = e.plan(&s) {
            let r = if l == first { 10_050.0 } else { 9_000.0 };
            e.record(l, r, None, None, &s, 0);
        }
        assert_eq!(e.state, State::Tuned);
        assert_eq!(e.incumbent, start, "mean 10,275 is under 3 % over the start");
    }

    /// agent 0.3.13: a trial that waits for game ids before its games in flight refill gets that time on top of its
    /// longest wait, and says it waited; one that refilled is not held any more.
    #[test]
    fn a_probe_waiting_for_game_ids_is_held() {
        let mut p = Probe::new(32, None, 1.0, 2.0);
        // (16 of 32 in flight: no refill; the longest wait 4 x 3 + 30 = 42 s)
        assert_eq!(p.sample(41.0, 1000, 0, 16), Verdict::Pending);
        p.hold(20.0);
        assert!(p.held() && !p.refilled());
        assert_eq!(p.sample(50.0, 1200, 0, 16), Verdict::Pending, "held 20 s: no timeout at 50 s");
        assert!(matches!(p.sample(63.0, 1300, 0, 16), Verdict::Invalid(_)), "62 s at most");
        // a refilled probe isn't held
        let mut q = Probe::new(32, None, 1.0, 2.0);
        assert_eq!(q.sample(1.0, 100, 0, 32), Verdict::Pending);
        q.hold(30.0);
        assert!(q.refilled() && !q.held());
    }

    #[test]
    fn discarded_trials_are_retried_then_skipped() {
        let s = space();
        let start = shipped();
        let mut e = Entry::new(key(), "r", start, false, 0);
        e.record(start, 10_000.0, None, None, &s, 0);
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
        let s = space();
        let start = shipped();
        let mut fake = Fake { best: start, noise: 0.0, n: 1 };
        let (mut e, _) = search(start, &s, &mut fake);
        assert!(!e.maybe_check(100 + RECHECK_S - 1), "not due yet");
        assert!(e.maybe_check(100 + RECHECK_S));
        assert_eq!(e.state, State::Checking);
        let r = e.runner_up().unwrap();
        assert_eq!(e.plan(&s), Some(r), "the runner-up first");
        e.record(r, 12_000.0, None, None, &s, 200 + RECHECK_S);
        assert_eq!(e.plan(&s), Some(start), "then the incumbent");
        e.record(start, 10_000.0, None, None, &s, 200 + RECHECK_S);
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
        let s = space();
        for i in 0..(MAX_ENTRIES as i64 + 3) {
            let mut k = key();
            k.net = format!("n{i}");
            let mut e = Entry::new(k, "r", shipped(), false, i);
            if i % 2 == 0 {
                e.record(e.start, 1000.0 + i as f64, None, None, &s, i);
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
        assert_eq!(r[0]["layout"]["infer_workers"], 5);
        assert!(r.iter().all(|x| x["leaves_s"].as_f64().unwrap() > 0.0));
    }

    #[test]
    fn hints_are_checked_and_fitted() {
        let s = space();
        let ok = json!({"layout": {"threads": 6, "infer_workers": 5, "infer_threads": 2, "groups": 6, "conc": 2304}, "leaves_s": 15000});
        assert_eq!(hint_layout(Some(&ok), &s), Some(lay(6, 5, 2, 6, 2304)));
        let over = json!({"layout": {"threads": 30, "infer_workers": 5, "infer_threads": 4, "groups": 6, "conc": 2304}});
        assert_eq!(hint_layout(Some(&over), &s), Some(lay(16, 5, 3, 6, 2304)), "fitted to this machine's caps");
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
        // macOS: the shipped layout (the M4 Max's measured best region), search and inference threads each at most
        // the cores, 192-384 rows per batch, sessions always
        let m4max = Hw { cores: 16, logical: 16, perf: 12, eff: 4, ram_mb: 16384 };
        let d = defaults(Platform::MacOs, &m4max);
        assert_eq!((d.threads, d.infer_workers, d.infer_threads, d.max_search, d.max_infer, d.batch_min, d.batch_max), (16, 5, 3, 16, 16, 96, 384));
        assert!(d.sessions_always);
        let air = defaults(Platform::MacOs, &Hw { cores: 8, logical: 8, perf: 4, eff: 4, ram_mb: 16384 });
        assert_eq!((air.threads, air.infer_workers, air.infer_threads), (8, 2, 3));
        let tiny = defaults(Platform::MacOs, &Hw { cores: 4, logical: 4, perf: 4, eff: 0, ram_mb: 16384 });
        assert_eq!((tiny.infer_workers, tiny.infer_threads), (1, 4), "under 6 cores one session");
        // Windows: today's layout (one search thread per core, one session of as many threads)
        let i9 = defaults(Platform::Windows, &Hw { cores: 8, logical: 16, perf: 8, eff: 0, ram_mb: 16384 });
        assert_eq!((i9.threads, i9.infer_workers, i9.infer_threads, i9.max_search, i9.max_infer, i9.sessions_always), (8, 1, 8, 8, 8, false));
        assert_eq!((i9.batch_min, i9.batch_max), (MIN_BATCH, crate::rolling::MAX_INFLIGHT));
        // either way the start is valid in its own space
        for (p, hw) in [(Platform::MacOs, m4max), (Platform::Windows, Hw { cores: 6, logical: 6, perf: 6, eff: 0, ram_mb: 16384 })] {
            let d = defaults(p, &hw);
            let s = Space::new(&d, None, Some(1), 32, 2304, None);
            let start = Layout { workers: 1, threads: d.threads, infer_workers: d.infer_workers, infer_threads: d.infer_threads, groups: 12, conc: 2304 };
            assert_eq!(s.fit(start), start, "{p:?}");
        }
    }

    /// The M4 Max as the profile sees it (64 GB).
    fn m4max() -> Hw {
        Hw { cores: 16, logical: 16, perf: 12, eff: 4, ram_mb: 65_536 }
    }

    #[test]
    fn a_big_mac_starts_with_two_workers_of_the_measured_best_layout() {
        let d = defaults(Platform::MacOs, &m4max());
        assert_eq!((d.workers, d.max_workers), (2, 3), "16 cores / 4, 64 GB / 16 GB, at most 3");
        assert_eq!((d.threads, d.infer_workers, d.infer_threads), (12, 4, 3), "2 x 12 on 16 cores");
        // 10.6 / 11.5's choice for the job: 2,304 games in flight in 12 groups of 192
        assert_eq!(d.layout_for(d.workers, 2304, 12), Layout { workers: 2, threads: 12, infer_workers: 4, infer_threads: 3, groups: 6, conc: 1152 });
        assert_eq!(d.layout_for(1, 2304, 12), lay(16, 5, 3, 12, 2304), "one worker: the layout of before");
        // a 12-core 32 GB Mac starts with two; less RAM or fewer cores: one
        assert_eq!(defaults(Platform::MacOs, &Hw { cores: 12, logical: 12, perf: 8, eff: 4, ram_mb: 32_768 }).workers, 2);
        assert_eq!(defaults(Platform::MacOs, &Hw { cores: 12, logical: 12, perf: 8, eff: 4, ram_mb: 24_576 }).workers, 1);
        assert_eq!(defaults(Platform::MacOs, &Hw { cores: 10, logical: 10, perf: 4, eff: 6, ram_mb: 65_536 }).workers, 1);
        let air = defaults(Platform::MacOs, &Hw { cores: 8, logical: 8, perf: 4, eff: 4, ram_mb: 16_384 });
        assert_eq!((air.workers, air.max_workers), (1, 1), "a small machine stays at one process");
        // Windows starts with one whatever the machine; the search may try more where the cores and RAM allow
        let ws = defaults(Platform::Windows, &Hw { cores: 16, logical: 32, perf: 16, eff: 0, ram_mb: 65_536 });
        assert_eq!((ws.workers, ws.max_workers, ws.threads, ws.infer_workers, ws.infer_threads), (1, 3, 16, 1, 16));
    }

    #[test]
    fn a_macs_crew_starts_within_the_default_memory_cap() {
        // agent 0.3.3: a Mac's games in flight cost ~16 MB each and more (rolling::MACOS_CPU_COST plans 18), so the start's
        // two workers hold 716 each; the plan bounds each worker's games in flight (each its own base), the tuner keeps
        // its games-in-flight range, and the groups stay where memory cuts the games (6 a worker, batches shrink)
        let cost = crate::rolling::MACOS_CPU_COST;
        let start_of = |hw: &Hw, cap: u64| {
            let d = defaults(Platform::MacOs, hw);
            let (auto, groups) = crate::rolling::mac_cpu_auto(hw.cores as u64);
            let one = crate::rolling::ram_cap(cap, cost).min(auto);
            let s = Space::new(&d, None, None, (one / 2).max(1), one, Some((cap, cost)));
            (s, s.fit(d.layout_for(d.workers, one, crate::rolling::mac_capped_groups(one, groups))), one)
        };
        // 64 GB, cap 32,768 MB: one worker would get 1,472 in 12 groups; two get (0.85 x 16,384 - 1,024) / 18 = 716 each,
        // in 6 groups (of 120 rows)
        let (s, l, one) = start_of(&m4max(), 32_768);
        assert_eq!(one, 1472);
        assert_eq!(s.mem_room(2), Some(716));
        assert_eq!(l, Layout { workers: 2, threads: 12, infer_workers: 4, infer_threads: 3, groups: 6, conc: 716 }, "the groups kept, batches of 120 rows");
        assert_eq!((s.conc_min, s.conc_max, s.workers_max), (736, 1472, 3), "the tuner's games in flight and workers kept");
        // every worker's base and games in flight within 85 % of its part, and at the crew's measured 16 MB a game under
        // the 92 % where a worker gives way
        for (cap, l) in [(32_768u64, l), (31_671, start_of(&m4max(), 31_671).1), (31_717, start_of(&m4max(), 31_717).1)] {
            let part = cap as f64 / l.workers as f64;
            assert!((cost.base_mb + l.conc * cost.per_game_mb) as f64 <= part * crate::rolling::PLAN_SHARE, "{cap}: {l:?}");
            assert!(1024.0 + l.conc as f64 * 16.0 < part * crate::rolling::HIGH_SHARE * 0.9, "{cap}: {l:?}");
            assert_eq!((l.workers, l.groups), (2, 6), "{cap}: {l:?}");
        }
        // the second validation lease's limit (31,717 MB): 2 x 691 in 6 groups (0.3.2: 2 x 778 in 4 groups of 195)
        assert_eq!(start_of(&m4max(), 31_717).1.conc, 691);
        // the owner's fixed 2 workers (autotune off): the same start
        let d = defaults(Platform::MacOs, &m4max());
        let fixed = Space::new(&d, None, Some(2), 736, 1472, Some((32_768, cost)));
        assert_eq!(fixed.fit(d.layout_for(2, 1472, crate::rolling::mac_capped_groups(1472, 12))), l);
        // 0.3.1's plan (4 MB a game) let both run 1,152; 0.3.2's (14 MB) 921 in 4 groups
        let old = Space { conc_min: 1152, conc_max: 2304, mem: Some((32_768, crate::rolling::MemCost { base_mb: 1024, per_game_mb: 4 })), ..fixed };
        assert_eq!(old.fit(d.layout_for(2, 2304, 12)).conc, 1152);
        // a 32 GB Mac with 12 cores (cap 16,384 MB): two workers of 329 in 3 groups (110 rows)
        let (s, l, one) = start_of(&Hw { cores: 12, logical: 12, perf: 8, eff: 4, ram_mb: 32_768 }, 16_384);
        assert_eq!(one, 704);
        assert_eq!((s.workers_max, l.workers, l.conc, l.groups), (2, 2, 329, 3));
        // a 16 GB Mac with 8 cores (cap 8,192 MB): one worker of 320 in 3 groups (107 rows; 0.3.2: 416 in 2)
        let (s, l, one) = start_of(&Hw { cores: 8, logical: 8, perf: 4, eff: 4, ram_mb: 16_384 }, 8192);
        assert_eq!(one, 320);
        assert_eq!((s.workers_max, l.workers, l.conc, l.groups), (1, 1, 320, 3));
    }

    #[test]
    fn a_remembered_incumbent_the_memory_no_longer_holds_is_its_fitted_layout() {
        // agent 0.3.2 on the M4 Max (2026-11-19): the entry kept 2 x 958 games in flight from a lease planned at 13 MB a
        // game (its trial discarded); the next lease, planned at 16 MB, held 2 x 778. The first worker's trial of 778 was
        // discarded (load) and its discards and measurements were never the incumbent's: `plan` returned the fitted
        // incumbent for ever and the agent handed the job over to the same layout 3 times in 7 minutes
        let d = defaults(Platform::MacOs, &m4max());
        let cost = |per| Some((31_742, crate::rolling::MemCost { base_mb: 1024, per_game_mb: per }));
        let old = Space::new(&d, None, None, 992, 1984, cost(13));
        let new = Space::new(&d, None, None, 800, 1600, Some((31_717, crate::rolling::MemCost { base_mb: 1024, per_game_mb: 16 })));
        let inc = Layout { workers: 2, threads: 12, infer_workers: 4, infer_threads: 3, groups: 4, conc: 958 };
        assert_eq!(old.fit(inc), inc);
        let l = new.fit(inc);
        assert_eq!(l, Layout { conc: 778, ..inc });
        let mut e = Entry::new(key(), "r", inc, false, 0);
        e.discard(inc, &old, 0);
        // the next lease binds its bounds (lease.rs `tuning_for`): the incumbent is what runs there
        e.bind(&new);
        assert_eq!((e.wanted(&new), e.plan(&new), e.incumbent_run()), (l, Some(l), l));
        e.discard(l, &new, 1);
        assert_eq!(e.discarded(&inc, Some(&new)), 2, "the discards of 958 and 778 are the incumbent's");
        assert_eq!(e.plan(&new), Some(l), "measured afresh (a fresh worker)");
        // measured: the incumbent's measurement, not a trial - the search goes on to a neighbour
        e.record(l, 15_000.0, Some(0.03), Some(Phase { p0: 1.0, p1: 2.0, conc: 1556 }), &new, 2);
        assert_eq!((e.trials, e.rate()), (0, Some(15_000.0)));
        let next = e.plan(&new).unwrap();
        assert_ne!(next, l, "no second trial of the same layout");
        assert_eq!(e.report().unwrap()["layout"], l.to_json(), "the incumbent as it ran");
        assert!(e.describe().contains("778 games in flight"), "{}", e.describe());
        // discarded at most 6 times (twice MAX_DISCARDS) where it runs, then the search goes on without it
        let mut e = Entry::new(key(), "r", inc, false, 0);
        e.bind(&new);
        for k in 0..2 * MAX_DISCARDS {
            assert_eq!(e.plan(&new), Some(l), "{k}");
            e.discard(if k % 2 == 0 { inc } else { l }, &new, k as i64);
        }
        assert_ne!(e.plan(&new), Some(l));
        // the stored layouts are kept as they were (a lease with more memory runs 958 again)
        assert_eq!(e.incumbent, inc);
        assert_eq!(old.fit(e.incumbent), inc);
        // an entry of 0.3.1-0.3.2 (no bounds, no loads) still loads
        let v = serde_json::to_value(Entry::new(key(), "r", inc, false, 0)).unwrap();
        assert!(v.get("space").is_none());
        let mut legacy = v.clone();
        legacy["measured"] = json!([{"layout": inc.to_json(), "rate": 9000.0, "t": 1}]);
        let back: Entry = serde_json::from_value(legacy).unwrap();
        assert_eq!((back.space, back.measured[0].load), (None, None));
        let mut bound = Entry::new(key(), "r", inc, false, 0);
        bound.record(l, 1.0, Some(0.02), None, &new, 0);
        let back: Entry = serde_json::from_value(serde_json::to_value(&bound).unwrap()).unwrap();
        assert_eq!(back, bound, "bounds and loads are remembered");
    }

    #[test]
    fn bursts_of_other_programs_pass_and_sustained_load_doesnt() {
        // the owner's Mac (2026-11-19): a sync daemon ~55 % of a core in bursts, syncthing to ~90 %, kernel_task ~19 % -
        // on 16 cores bursts of 10-18 % of the CPUs; 0.3.2 discarded a trial on any 2-s sample over 10 % (the first
        // trial of the crew within 9 s). Here: samples every 2 s, a 4-s burst of 18 % every 30 s over 3 % otherwise
        let mut tr = LoadTrack::default();
        let mut t = 0.0;
        for i in 0..200 {
            t += 2.0;
            tr.push(t, 2.0, if i % 15 < 2 { 0.18 } else { 0.03 });
        }
        assert!(tr.quiet(t, 95.0), "a trial may start: {:?}", tr.mean(t - 95.0, t));
        assert_eq!(tr.sustained(t, 30.0), None, "a burst isn't sustained");
        let (before, w) = tr.mean(t - 95.0, t).unwrap();
        assert!(before < 0.07 && (w - 95.0).abs() < 1e-9, "{before} {w}");
        // a trial in the same conditions counts, and records its load
        let t0 = t;
        for i in 0..60 {
            t += 2.0;
            tr.push(t, 2.0, if i % 15 < 2 { 0.18 } else { 0.03 });
            assert_eq!(tr.sustained(t, 30.0), None);
        }
        let during = tr.mean(t0, t).map(|(m, _)| m);
        assert_eq!(judge_trial(during, Some(before)), Ok(during));
        // sustained: another program takes 30 % of the CPUs from now on - the trial ends at once, none starts
        for _ in 0..15 {
            t += 2.0;
            tr.push(t, 2.0, 0.30);
        }
        assert!(tr.sustained(t, 30.0).is_some_and(|m| (m - 0.30).abs() < 1e-9));
        assert!(!tr.quiet(t, 95.0));
        // a trial's mean over 10 %, or 5 points over the load before it, doesn't count
        assert!(judge_trial(Some(0.12), Some(0.11)).unwrap_err().starts_with("other programs used 12 % of the CPUs"));
        assert!(judge_trial(Some(0.09), Some(0.02)).is_err(), "rose 7 points");
        assert_eq!(judge_trial(Some(0.06), Some(0.03)), Ok(Some(0.06)));
        assert_eq!(judge_trial(Some(0.06), None), Ok(Some(0.06)), "nothing measured before it (a lease's first worker)");
        assert_eq!(judge_trial(None, None), Ok(None));
        // means weigh partial samples by time; nothing measured yet is quiet; sustained needs the window covered
        let mut tr = LoadTrack::default();
        assert!(tr.quiet(0.0, 1.0));
        tr.push(10.0, 10.0, 0.5);
        tr.push(20.0, 10.0, 0.0);
        assert_eq!(tr.mean(5.0, 15.0), Some((0.25, 10.0)));
        assert_eq!(tr.sustained(10.0, 30.0), None, "only 10 of 30 s covered");
        assert_eq!(tr.last(), Some(0.0));
    }

    #[test]
    fn a_candidate_measured_on_a_quieter_machine_doesnt_replace_the_incumbent() {
        let s = space();
        let start = shipped();
        let run = |inc_load: f64, cand_load: f64| {
            let mut e = Entry::new(key(), "r", start, false, 0);
            e.record(start, 10_000.0, Some(inc_load), None, &s, 0);
            let c = e.plan(&s).unwrap();
            e.record(c, 10_500.0, Some(cand_load), None, &s, 0);
            (e.incumbent == c, e)
        };
        assert!(!run(0.08, 0.01).0, "5 % faster with 7 points less load: not comparable");
        assert!(run(0.04, 0.02).0, "similar load: 5 % faster wins");
        assert!(run(0.01, 0.09).0, "faster under more load wins");
        let (_, e) = run(0.08, 0.01);
        assert_eq!(e.measured[1].load, Some(0.01), "the load is recorded with the measurement");
    }

    /// agent 0.3.4: the 4070 Ti PC of 2026-11-19 (4 cores / 8 threads: 6 search threads shared; up to 3 workers) with a
    /// target of 1,024 games in flight (its bench's 512-row batches), a memory limit of `limit` MB at 1,800 MB + `per_game`
    /// MB a game, and `budget` MB of video memory at `GPU_VRAM`.
    fn gpu_space(limit: u64, per_game: u64, budget: u64, set: Option<u32>) -> Space {
        let mem = crate::rolling::MemCost { base_mb: 1800, per_game_mb: per_game };
        Space::new_gpu(6, set, 3, 1024, false, Some((limit, mem)), Some((budget, crate::rolling::GPU_VRAM)))
    }

    fn glay(workers: u32, threads: u32, groups: u64, conc: u64) -> Layout {
        Layout { workers, threads, infer_workers: 1, infer_threads: threads, groups, conc }
    }

    #[test]
    fn a_gpu_crew_starts_at_the_bench_target_where_memory_allows() {
        // half of 32 GB, 4 MB a game measured, the 4070 Ti's 9,939 MB: three workers of 384 in one group, 2 search threads
        // each - the bench's best (14,154 leaves/s)
        let s = gpu_space(16_384, 4, 9_939, None);
        assert_eq!((s.workers_min, s.workers_max, s.conc_min, s.conc_max), (1, 3, 512, 2048), "agent 0.3.7: up to twice the target (0.3.4-0.3.6: 1.5 times)");
        assert_eq!(s.gpu_start(true, 512), glay(3, 2, 1, 384));
        assert_eq!((s.target_conc(1), s.target_conc(2), s.target_conc(3)), (1024, 512, 384));
        // a ~9.5 GB limit: three workers hold 3 x 222 in all, 65 % of 1,024 - two of 512, 3 search threads each (agent 0.3.7:
        // in two groups of 256 rows)
        let s = gpu_space(9_500, 4, 9_939, None);
        assert_eq!(s.gpu_start(true, 512), glay(2, 3, 2, 512));
        // a 9.0 GB limit: two hold 2 x 506 = 1,012, 99 % of the target (0.3.4: one worker of 1,024)
        assert_eq!(gpu_space(9_000, 4, 9_939, None).gpu_start(true, 512), glay(2, 3, 1, 506));
        // at the default 45 MB a game: one worker, (8,075 - 1,800) / 45 = 139 games in flight (0.3.3's plan: 128)
        let s = gpu_space(9_500, 45, 9_939, None);
        assert_eq!(s.workers_max, 1);
        assert_eq!(s.gpu_start(true, 512), glay(1, 6, 1, 139), "one batch of 139 (0.3.4: 2 groups of 70 rows)");
        // not NVIDIA: one worker of 1,024 in two groups of 512 (the tuner tries more)
        assert_eq!(gpu_space(16_384, 4, 9_939, None).gpu_start(false, 512), glay(1, 6, 2, 1024));
        // the owner's two workers; `autotune` off without them: one
        assert_eq!(gpu_space(16_384, 4, 9_939, Some(2)).gpu_start(true, 512), glay(2, 3, 2, 512));
        assert_eq!(gpu_space(16_384, 4, 9_939, Some(1)).gpu_start(true, 512), glay(1, 6, 2, 1024));
        // a run's number: fixed games in flight in all
        let fixed = Space::new_gpu(6, None, 3, 600, true, None, Some((9_939, crate::rolling::GPU_VRAM)));
        assert_eq!((fixed.conc_min, fixed.conc_max), (600, 600));
        assert_eq!(fixed.gpu_start(true, 512).total_conc(), 3 * 200);
    }

    /// agent 0.3.7: the 4070 Ti PC's 0.3.6 canary lease of 2026-11-20 (a 13,973 MB limit, a measured base of 1,659 MB, 4 MB
    /// a game planned, 9,831 MB of video memory for the job, the bench's 1,024-row batch: a target of 2,048): the crew of 3 x
    /// 575 starts in two groups of 288 rows each (0.3.6: one group of 575, its search threads ~60 % idle); the tuner's
    /// batch move tries the one group, its worker move two workers of 862.
    #[test]
    fn a_gpu_crew_starts_in_two_groups() {
        let mem = crate::rolling::MemCost { base_mb: 1659, per_game_mb: 4 };
        let s = Space::new_gpu(6, None, 3, 2048, false, Some((13_973, mem)), Some((9_831, crate::rolling::GPU_VRAM)));
        let start = s.gpu_start(true, 1024);
        assert_eq!(start, glay(3, 2, 2, 575));
        assert_eq!(start.batch(), 288);
        assert_eq!(neighbour(&start, Coord::Batch, true, &s), Some(glay(3, 2, 1, 575)), "the 0.3.6 layout is a trial away");
        assert_eq!(neighbour(&start, Coord::Workers, false, &s), Some(glay(2, 3, 2, 862)));
    }

    #[test]
    fn a_gpu_crew_fits_the_video_memory() {
        // a 4 GB GPU (3,000 MB for the job): one worker's batch 988 rows at most, two workers' 238, three none
        let s = gpu_space(16_384, 4, 3_000, None);
        assert_eq!((s.vram_rows(1), s.vram_rows(2), s.vram_rows(3)), (Some(988), Some(238), Some(0)));
        assert_eq!(s.workers_max, 2, "a third worker's base doesn't fit");
        // two of 512 don't fit in one group each: two groups of 238 rows, 476 games in flight - 952 in all, 93 % of the
        // target: the start (0.3.4 asked for all of it: one worker of 1,024)
        assert_eq!(s.fit(glay(2, 3, 1, 512)), glay(2, 3, 2, 476));
        assert_eq!(s.gpu_start(true, 512), glay(2, 3, 2, 476));
        // every layout of the space within the budget at its plan
        for l in [glay(1, 6, 1, 1536), glay(2, 3, 1, 768), glay(2, 3, 2, 768), glay(1, 6, 2, 4096)] {
            let f = s.fit(l);
            assert!(f.workers as u64 * crate::rolling::GPU_VRAM.plan_mb(f.batch()) <= 3_000, "{l:?} -> {f:?}");
        }
        // too little for one worker's base and batch: one worker, its batch as small as it goes
        let tiny = gpu_space(16_384, 4, 1_000, None);
        assert_eq!(tiny.workers_max, 1);
        assert_eq!(tiny.fit(glay(1, 6, 2, 1024)).conc, 2, "one row a batch, two groups");
    }

    #[test]
    fn gpu_neighbours_split_threads_and_keep_one_session() {
        let s = gpu_space(16_384, 4, 9_939, None);
        let three = glay(3, 2, 1, 384);
        // two workers: the job's 1,152 games in flight and its 384-row batches kept, 3 search threads each
        assert_eq!(neighbour(&three, Coord::Workers, false, &s), Some(glay(2, 3, 2, 576)));
        assert_eq!(neighbour(&three, Coord::Workers, true, &s), None, "at most 3");
        // no inference coordinates; search threads within their share
        assert_eq!(neighbour(&three, Coord::InferThreads, false, &s), None);
        assert_eq!(neighbour(&three, Coord::Sessions, true, &s), None);
        assert_eq!(neighbour(&three, Coord::Search, true, &s), None, "2 of 6 for three workers");
        assert_eq!(neighbour(&three, Coord::Search, false, &s), Some(glay(3, 1, 1, 384)));
        // one or two groups
        assert_eq!(neighbour(&three, Coord::Batch, false, &s), Some(glay(3, 2, 2, 384)));
        assert_eq!(neighbour(&three, Coord::Batch, true, &s), None);
        // games in flight per worker: up to 2,048 / 3 (agent 0.3.7; 0.3.4-0.3.6: 1,536 / 3), down to 512 / 3 (0.3.3 clamped
        // them to the job's bounds)
        assert_eq!(neighbour(&three, Coord::Conc, true, &s), Some(glay(3, 2, 2, 682)));
        assert_eq!(neighbour(&three, Coord::Conc, false, &s), Some(glay(3, 2, 1, 192)));
        let one = glay(1, 6, 2, 1024);
        assert_eq!(neighbour(&one, Coord::Workers, true, &s), Some(glay(2, 3, 1, 512)), "one worker -> two of 512 (2 groups of 512 rows: 1 group each)");
        assert!(one.describe_on(true).starts_with("6 search threads, 1,024 games in flight in 2 groups (512 rows) on the GPU"), "{}", one.describe_on(true));
        assert!(three.describe_on(true).starts_with("3 workers x (2 search threads, 384 games in flight in 1 group (384 rows)) on the GPU"), "{}", three.describe_on(true));
    }

    #[test]
    fn a_gpu_layout_the_memory_plan_held_back_is_lifted_to_the_target_not_past_it() {
        // the canary's first 0.3.4 lease without a measurement: 1 x 139 at 45 MB a game; a steady reading of 3 MB (planned
        // at 4): 1 x 1,024 (the target, not the 1,536 the bounds allow), and up to three workers
        let old = gpu_space(9_500, 45, 9_939, None);
        let start = old.gpu_start(true, 512);
        let mut e = Entry::new(key(), "r", start, false, 0);
        e.bind(&old);
        let mut new = gpu_space(9_500, 4, 9_939, None);
        new.bound_workers(None, 3);
        assert_eq!(new.workers_max, 3);
        e.lift_conc(&old, &new);
        assert_eq!(e.wanted(&new), glay(1, 6, 1, 1024));
        assert_eq!(new.top_conc(3), 222, "three workers: the memory plan's 222 each");
        // (agent 0.3.5: lease.rs `calibrate` picks a start nothing else beat again for the new plan: two of 512)
        assert_eq!(new.gpu_start(true, 512), glay(2, 3, 2, 512));
        // a tuner's choice of fewer games than the old plan held stays
        let roomy = gpu_space(9_500, 10, 9_939, None);
        assert_eq!(roomy.top_conc(1), 627);
        let mut e = Entry::new(key(), "r", glay(1, 6, 2, 600), false, 0);
        e.lift_conc(&roomy, &new);
        assert_eq!(e.incumbent.conc, 600);
    }

    /// agent 0.3.12: a DirectML job's space - "auto" (3,456 games in flight: `rolling::dml_target` of a 65,536-id block),
    /// `threads` search threads shared, at most 3 workers, the job's memory limit with a base and cost per game, and
    /// `vram_mb` of video memory for its workers.
    fn dml_space(limit: u64, base: u64, per_game: u64, vram_mb: u64, threads: u32, set: Option<u32>) -> Space {
        let mem = crate::rolling::MemCost { base_mb: base, per_game_mb: per_game };
        let target = crate::rolling::dml_target(crate::rolling::Inflight::Auto, 65_536);
        Space::new_dml(threads, set, 3, target, false, Some((limit, mem)), Some((vram_mb, crate::rolling::GPU_VRAM)))
    }

    /// The video memory a 12 GB and a 16 GB GPU leave the job's workers (85 % of 12,282 / 16,304 MB, less 1,024 MB for
    /// other programs).
    const VRAM_12G: u64 = 9_415;
    const VRAM_16G: u64 = 12_834;

    /// agent 0.3.12 (PROTOCOL 13.1 **DirectML**): a DirectML job starts from DirectML's own layout on any vendor's GPU -
    /// three workers of 1,152 games in flight in two groups of 576 rows where the memory plans hold it (the 4070 Ti PC's
    /// measured best, 3 x 1,150: 12.0k leaves/s) -, never the one small worker 0.3.11 started a GPU other than NVIDIA's with.
    /// agent 0.3.16 (`CUDA_CREW`): an NVIDIA GPU's CUDA job starts from DirectML's crew rule - on the 4070 Ti PC's memory
    /// plan (a 13,470 MB limit, 1,992 MB a worker; 9,831 MB of video memory, 6 search threads) 3 x 912 at 2 MB a game
    /// (measured 26.4k leaves/s; 0.3.15's start there 3 x 704, 23.5k) and 2 x 1,152 at 3 MB (2 x 1,244 measured 25.2k;
    /// 0.3.15: 3 x 608), where 0.3.15's bench-shaped start shared two of the bench's 1,024-row batches by three workers.
    #[test]
    fn an_nvidia_cuda_job_starts_from_the_crew_rule() {
        assert!(CUDA_CREW);
        let at2 = dml_space(13_470, 1_992, 2, 9_831, 6, None);
        assert_eq!(at2.gpu_start(true, 1024), glay(3, 2, 2, 912));
        let at3 = dml_space(13_470, 1_992, 3, 9_831, 6, None);
        assert_eq!(at3.gpu_start(true, 1024), glay(2, 3, 2, 1152));
        // the floor of two workers (`CUDA_CREW_MIN_WORKERS`): both plans hold two
        let (mut f2, mut f3) = (at2.clone(), at3.clone());
        f2.cuda_crew_floor();
        f3.cuda_crew_floor();
        assert_eq!((f2.workers_min, f2.workers_max, f3.workers_min, f3.workers_max), (2, 3, 2, 3));
        // a memory plan that holds one worker only: no floor
        let mut tight = dml_space(4_000, 1_992, 3, 9_831, 6, None);
        tight.cuda_crew_floor();
        assert_eq!(tight.workers_min, 1);
        // 0.3.15's start on the same plans, for the record
        let old = |c| Space::new_gpu(6, None, 3, crate::rolling::gpu_target(crate::rolling::Inflight::Auto, 1024), false, Some((13_470, crate::rolling::MemCost { base_mb: 1_992, per_game_mb: c })), Some((9_831, crate::rolling::GPU_VRAM))).gpu_start(true, 1024);
        assert_eq!((old(2), old(3)), (glay(3, 2, 2, 704), glay(3, 2, 2, 608)));
    }

    /// agent 0.3.17 (PROTOCOL 18.3): the freshness cap bounds every layout the space makes valid - the job's games in
    /// flight in all at most the cap, each worker its share, fewer workers rather than workers below their minimum (never
    /// below the bounds' least: a CUDA crew's floor, the owner's fixed count); a GPU worker's groups are the rule's for its
    /// games. No cap, or a cap above the layout: the layout as before.
    #[test]
    fn the_freshness_cap_bounds_a_gpu_crew() {
        let mut s = dml_space(24_000, 2_059, 3, VRAM_12G, 6, None);
        let crew = s.gpu_start(true, 1024);
        assert_eq!(crew, glay(3, 2, 2, 1152));
        assert!(!s.capped(&crew));
        // a CUDA crew at ~5.6 games/s and legacy-meta3-r1's 157 s: 879 games in flight - three workers of 293 in one batch
        s.fresh = Some(879);
        let c = s.fit(crew);
        assert_eq!(c, glay(3, 2, 1, 293));
        assert!(c.total_conc() <= 879 && s.capped(&crew) && !s.capped(&c));
        assert_eq!(s.fit(c), c, "a capped layout is valid as it is");
        assert_eq!(s.uncapped().fit(crew), crew);
        // 1,600 in all: 533 each, two groups (each keeps 256 rows or more)
        s.fresh = Some(1600);
        assert_eq!(s.fit(crew), glay(3, 2, 2, 533));
        // the floor: 128 in all - two workers of 64 (their threads rescaled to the share of two), not three of 42
        s.fresh = Some(crate::fresh::FLOOR_GPU);
        assert_eq!(s.fit(crew), glay(2, 3, 1, 64));
        // a cap above the layout: nothing changes
        s.fresh = Some(4000);
        assert_eq!(s.fit(crew), crew);
        // the bounds' least worker count stays: a CUDA crew's floor of two, the owner's fixed three
        let mut floor2 = dml_space(24_000, 2_059, 3, VRAM_12G, 6, None);
        floor2.cuda_crew_floor();
        floor2.fresh = Some(64);
        assert_eq!(floor2.fit(crew), glay(2, 3, 1, 32));
        let mut owner3 = dml_space(24_000, 2_059, 3, VRAM_12G, 6, Some(3));
        owner3.fresh = Some(crate::fresh::FLOOR_GPU);
        assert_eq!(owner3.fit(crew), glay(3, 2, 1, 42));
        // the tuner's moves stay under it: no neighbour plays more games in flight than the cap
        s.fresh = Some(900);
        let c = s.fit(crew);
        for cd in COORDS {
            for up in [true, false] {
                if let Some(n) = neighbour(&c, cd, up, &s) {
                    assert!(n.total_conc() <= 900, "{cd:?} {up}: {n:?}");
                }
            }
        }
    }

    /// agent 0.3.17: a CPU job's cap keeps its groups where the batches allow; the entry never remembers the cap (its
    /// bound space, its measurements compared without it), and a remembered incumbent is capped where it runs.
    #[test]
    fn the_freshness_cap_bounds_a_cpu_job_and_is_never_remembered() {
        let d = defaults(Platform::Windows, &Hw { cores: 8, logical: 16, perf: 8, eff: 0, ram_mb: 32_768 });
        let mut s = Space::new(&d, None, None, 16, 32, None);
        let l = s.fit(d.layout_for(1, 32, 4));
        assert_eq!((l.workers, l.conc, l.groups), (1, 32, 4));
        s.fresh = Some(20);
        let c = s.fit(l);
        assert_eq!((c.workers, c.conc, c.groups), (1, 20, 2), "two groups of 10 rows (at least 8)");
        s.fresh = Some(4);
        assert_eq!(s.fit(l).conc, 4, "the space takes the cap as given; the agent never sets one under fresh::FLOOR_CPU");
        // the entry: bound without the cap, measurements of the uncapped layout stay its own, wanted is capped
        s.fresh = Some(20);
        let mut e = Entry::new(key(), "r", l, false, 0);
        e.bind(&s);
        assert_eq!(e.space.unwrap().fresh, None);
        e.record(l, 1000.0, None, None, &s, 1);
        assert_eq!(e.mean(&l), Some(1000.0));
        assert_eq!(e.mean(&c), None, "the capped layout has no measurement of its own");
        assert_eq!(e.wanted(&s), c);
        assert!(serde_json::to_string(&e).unwrap().find("fresh").is_none());
    }

    #[test]
    fn a_directml_job_starts_from_directmls_crew_on_any_vendor() {
        // a 12 GB GPU, 4 cores (6 search threads), a 24,000 MB limit, 3 MB a game measured: 3 x 1,152, 2 search threads each
        let s = dml_space(24_000, 1800, 3, VRAM_12G, 6, None);
        assert!(s.dml && s.gpu);
        assert_eq!((s.workers_min, s.workers_max, s.conc_min, s.conc_max, s.target), (1, 3, 768, 4096, 3456));
        assert_eq!((s.target_conc(1), s.target_conc(2), s.target_conc(3)), (1152, 1152, 1152), "DirectML's 1,152 a worker, however many");
        let start = s.gpu_start(true, 1024);
        assert_eq!(start, glay(3, 2, 2, 1152));
        assert_eq!((start.batch(), start.total_conc()), (576, 3456));
        // the same on an AMD GPU (vendor no matter): a 16 GB GPU, 8 cores (12 search threads), 4 each
        assert_eq!(dml_space(24_000, 1800, 3, VRAM_16G, 12, None).gpu_start(false, 1024), glay(3, 4, 2, 1152));
        // a 16 GB AMD GPU on a 32 GB PC (the job's limit half of it), 2 MB a game measured: 3 x 1,152 too
        assert_eq!(dml_space(16_384, 1800, 2, VRAM_16G, 12, None).gpu_start(false, 1024), glay(3, 4, 2, 1152));
        // ... at 3 MB a game: (4,642 - 1,800) / 3 = 947 each (fewer games each, not fewer workers)
        assert_eq!(dml_space(16_384, 1800, 3, VRAM_16G, 12, None).gpu_start(false, 1024), glay(3, 4, 2, 947));
        // a 32 GB PC's first lease, nothing measured (the default 1,800 MB + 45 MB a game): one worker of 269 in one batch,
        // as before - until the lease's early reading (lease.rs `calibrate` picks the start again): at 4 MB a game over a
        // measured base of 2,059 MB two of 1,152 (three would hold 645 each), at a steady 3 MB three of 861
        let first = dml_space(16_384, 1800, 45, VRAM_12G, 6, None);
        assert_eq!(first.workers_max, 1);
        assert_eq!(first.gpu_start(true, 1024), glay(1, 6, 1, 269));
        assert_eq!(dml_space(16_384, 2059, 4, VRAM_12G, 6, None).gpu_start(true, 1024), glay(2, 3, 2, 1152));
        assert_eq!(dml_space(16_384, 2059, 3, VRAM_12G, 6, None).gpu_start(true, 1024), glay(3, 2, 2, 861));
        // a layout the default plan held back is lifted to DirectML's 1,152 a worker, not the crew's 3,456 (its groups kept,
        // as any GPU job's; a search that measured nothing but the start picks the start again instead)
        let calibrated = dml_space(16_384, 2059, 3, VRAM_12G, 6, None);
        assert_eq!((calibrated.top_conc(1), calibrated.top_conc(3)), (1152, 861));
        let mut e = Entry::new(key(), "r", first.gpu_start(true, 1024), false, 0);
        e.lift_conc(&first, &calibrated);
        assert_eq!(e.wanted(&calibrated), glay(1, 6, 1, 1152));
    }

    /// agent 0.3.12: where the memory plans hold less than 3 x 1,152, a DirectML crew's workers run fewer games each - down to
    /// 768 (batches of 384 rows) -, then the crew has a worker fewer; one worker runs what memory holds.
    #[test]
    fn a_directml_crew_shrinks_its_games_then_its_workers_where_memory_is_short() {
        // 4 MB a game: an 18,000 MB limit holds 3 x 825; 16,000 MB 3 x 683 - under 768: two of 1,152; 9,500 MB two of 559:
        // one of 1,152
        assert_eq!(dml_space(18_000, 1800, 4, VRAM_12G, 6, None).gpu_start(true, 1024), glay(3, 2, 2, 825));
        assert_eq!(dml_space(16_000, 1800, 4, VRAM_12G, 6, None).gpu_start(true, 1024), glay(2, 3, 2, 1152));
        let small = dml_space(9_500, 1800, 4, VRAM_12G, 6, None);
        assert_eq!(small.workers_max, 2, "three don't hold 768 in all");
        assert_eq!(small.gpu_start(true, 1024), glay(1, 6, 2, 1152));
        // the default 45 MB a game under 9,500 MB: one worker of 139 in one batch
        assert_eq!(dml_space(9_500, 1800, 45, VRAM_12G, 6, None).gpu_start(true, 1024), glay(1, 6, 1, 139));
        // a 4 GB GPU (3,000 MB for the job) and plenty of RAM: two workers' batches hold 238 rows (2 x 476 < 768), one
        // worker's 988: one of 1,152 in two batches of 576
        let gpu4 = dml_space(24_000, 1800, 3, 3_000, 6, None);
        assert_eq!(gpu4.workers_max, 2);
        assert_eq!(gpu4.gpu_start(true, 1024), glay(1, 6, 2, 1152));
        // every start within the video memory plan
        for s in [dml_space(18_000, 1800, 4, VRAM_12G, 6, None), gpu4, dml_space(24_000, 1800, 3, 4_500, 6, None)] {
            let l = s.gpu_start(true, 1024);
            assert!(l.workers as u64 * crate::rolling::GPU_VRAM.plan_mb(l.batch()) <= s.vram.unwrap().0, "{l:?}");
            assert!(l.workers == 1 || l.conc >= crate::rolling::DML_MIN_CONC, "{l:?}");
        }
    }

    /// agent 0.3.12: the owner's worker count, a run's number and a small block bound DirectML's start as any GPU job's;
    /// the tuner searches from it (its first trial: one worker fewer, the job's games in flight and batches kept).
    #[test]
    fn directmls_start_keeps_the_owners_settings_and_the_tuner_searches_from_it() {
        assert_eq!(dml_space(24_000, 1800, 3, VRAM_12G, 6, Some(1)).gpu_start(true, 1024), glay(1, 6, 2, 1152));
        assert_eq!(dml_space(24_000, 1800, 3, VRAM_12G, 6, Some(2)).gpu_start(true, 1024), glay(2, 3, 2, 1152));
        // a run's 2,048: shared - three of 682 keep less than 768, two of 1,024
        let mem = crate::rolling::MemCost { base_mb: 1800, per_game_mb: 3 };
        let fixed = Space::new_dml(6, None, 3, 2048, true, Some((24_000, mem)), Some((VRAM_12G, crate::rolling::GPU_VRAM)));
        assert_eq!((fixed.conc_min, fixed.conc_max), (2048, 2048));
        assert_eq!(fixed.gpu_start(true, 1024), glay(2, 3, 2, 1024));
        // a lease's block of 2,000 ids: two of 1,000
        let target = crate::rolling::dml_target(crate::rolling::Inflight::Auto, 2_000);
        let block = Space::new_dml(6, None, 3, target, false, Some((24_000, mem)), Some((VRAM_12G, crate::rolling::GPU_VRAM)));
        assert_eq!(block.gpu_start(true, 1024), glay(2, 3, 2, 1000));
        // the search from 3 x 1,152: one worker fewer (the job's 3,456 and its 576-row batches kept as far as two groups
        // go), games in flight halved / doubled within each worker's part, search threads within their share
        let s = dml_space(24_000, 1800, 3, VRAM_12G, 6, None);
        let start = s.gpu_start(true, 1024);
        assert_eq!(neighbour(&start, Coord::Workers, false, &s), Some(glay(2, 3, 2, 1728)));
        assert_eq!(neighbour(&start, Coord::Workers, true, &s), None, "at most 3");
        assert_eq!(neighbour(&start, Coord::Conc, true, &s), Some(glay(3, 2, 2, 1365)));
        assert_eq!(neighbour(&start, Coord::Conc, false, &s), Some(glay(3, 2, 1, 576)));
        assert_eq!(neighbour(&start, Coord::Search, false, &s), Some(glay(3, 1, 2, 1152)));
        let mut e = Entry::new(key(), "r", start, false, 0);
        assert_eq!(e.plan(&s), Some(start), "the start is measured first");
        e.record(start, 12_000.0, Some(0.0), None, &s, 0);
        assert_eq!(e.plan(&s), Some(glay(2, 3, 2, 1728)), "then the first candidate");
    }

    /// agent 0.3.12: nothing changes for a GPU job on another provider - CUDA's crew from the bench's target on an NVIDIA
    /// GPU, one worker on another vendor's GPU -, and a space stored before 0.3.12 reads as not DirectML.
    #[test]
    fn a_cuda_jobs_start_is_unchanged_by_directml() {
        let cuda = gpu_space(16_384, 4, 9_939, None);
        assert!(!cuda.dml);
        assert_eq!(cuda.gpu_start(true, 512), glay(3, 2, 1, 384));
        assert_eq!(cuda.gpu_start(false, 512), glay(1, 6, 2, 1024), "another provider on another vendor's GPU: one worker");
        assert_eq!((cuda.conc_min, cuda.conc_max, cuda.target_conc(1), cuda.target_conc(3)), (512, 2048, 1024, 384));
        // the same PC on DirectML: its own crew (two of 1,152 under this plan: three would hold 710 each)
        assert_eq!(dml_space(16_384, 1800, 4, 9_939, 6, None).gpu_start(true, 512), glay(2, 3, 2, 1152));
        let mut v = serde_json::to_value(cuda).unwrap();
        v.as_object_mut().unwrap().remove("dml");
        assert_eq!(serde_json::from_value::<Space>(v).unwrap(), cuda);
    }

    #[test]
    fn thread_budgets_per_worker_count() {
        assert_eq!((budget(16, 1), budget(16, 2), budget(16, 3)), (16, 12, 8));
        assert_eq!((budget(8, 1), budget(8, 2), budget(8, 3)), (8, 6, 4));
        assert_eq!(budget(1, 3), 1, "never 0");
    }

    /// The M4 Max's space for 1 to 3 workers, 1,152-2,304 games in flight in all, no memory limit.
    fn crew_space() -> Space {
        Space { workers_min: 1, workers_max: 3, ..space() }
    }

    #[test]
    fn worker_bounds_come_from_the_setting_cores_ram_and_memory() {
        let d = defaults(Platform::MacOs, &m4max());
        let cost = crate::rolling::MemCost { base_mb: 1024, per_game_mb: 9 };
        let s = Space::new(&d, None, None, 1152, 2304, Some((32_768, cost)));
        assert_eq!((s.workers_min, s.workers_max), (1, 3), "auto: 1 to what the cores and RAM allow");
        let s = Space::new(&d, None, Some(2), 1152, 2304, Some((32_768, cost)));
        assert_eq!((s.workers_min, s.workers_max), (2, 2), "the owner's number");
        assert_eq!(Space::new(&d, None, Some(9), 1152, 2304, None).workers_max, MAX_WORKERS);
        // a small memory limit: (0.85 x 6,000 - 1,024) / 9 = 452 games for one worker, 169 each for two - fewer than the
        // 1,152 the job needs at least: one worker only
        let small = Space::new(&d, None, None, 1152, 2304, Some((6000, cost)));
        assert_eq!(small.workers_max, 1);
        assert_eq!(Space::new(&d, None, Some(3), 1152, 2304, Some((6000, cost))).workers_max, 1, "never more than the memory holds");
    }

    #[test]
    fn crews_fit_their_budgets_and_memory() {
        let s = crew_space();
        // two workers: threads within 12 each, each worker 576-1,152 games in flight
        assert_eq!(s.fit(Layout { workers: 2, ..shipped() }), Layout { workers: 2, threads: 12, infer_workers: 5, infer_threads: 2, groups: 6, conc: 1152 });
        assert_eq!(s.fit(Layout { workers: 3, threads: 12, infer_workers: 4, infer_threads: 3, groups: 4, conc: 768 }).infer_total(), 8);
        assert_eq!(s.fit(Layout { workers: 5, ..shipped() }).workers, 3);
        // memory: (0.85 x 20,000 / 2 - 1,024) / 9 = 830 a worker
        let m = Space { mem: Some((20_000, crate::rolling::MemCost { base_mb: 1024, per_game_mb: 9 })), ..s };
        assert_eq!(m.fit(Layout { workers: 2, threads: 12, infer_workers: 4, infer_threads: 3, groups: 6, conc: 1152 }).conc, 830);
    }

    #[test]
    fn worker_moves_keep_the_jobs_games_and_batches_and_rescale_threads() {
        let s = crew_space();
        let one = shipped();
        let two = neighbour(&one, Coord::Workers, true, &s).unwrap();
        assert_eq!(two, Layout { workers: 2, threads: 12, infer_workers: 4, infer_threads: 3, groups: 6, conc: 1152 }, "one worker -> the measured best");
        assert_eq!(neighbour(&two, Coord::Workers, false, &s), Some(one), "and back");
        let three = neighbour(&two, Coord::Workers, true, &s).unwrap();
        assert_eq!((three.workers, three.threads, three.conc, three.groups), (3, 8, 768, 4));
        assert!(three.infer_total() <= 8);
        assert_eq!(neighbour(&three, Coord::Workers, true, &s), None, "at most 3");
        assert_eq!(neighbour(&one, Coord::Workers, false, &s), None);
        assert_eq!(neighbour(&one, Coord::Workers, true, &space()), None, "a fixed worker count");
        // Windows (no sessions): 8 + 1 x 8 with 32 games in flight in 4 groups -> 2 x (6 + 1 x 6), 16 in 2 groups
        let w = Space::new(&defaults(Platform::Windows, &Hw { cores: 8, logical: 16, perf: 8, eff: 0, ram_mb: 32_768 }), None, None, 16, 32, None);
        assert_eq!(w.workers_max, 2);
        let n = neighbour(&lay(8, 1, 8, 4, 32), Coord::Workers, true, &w).unwrap();
        assert_eq!(n, Layout { workers: 2, threads: 6, infer_workers: 1, infer_threads: 6, groups: 2, conc: 16 });
    }

    #[test]
    fn the_search_finds_more_workers_when_they_pay() {
        // a peak at the M4 Max's measured best: two workers 30 % over one, three slightly less
        let s = crew_space();
        let best = Layout { workers: 2, threads: 12, infer_workers: 4, infer_threads: 3, groups: 6, conc: 1152 };
        let mut fake = Fake { best: best.per_worker(), noise: 0.0, n: 1 };
        let crew = [1.0, 1.3, 1.25];
        let mut rate = |l: &Layout| fake.rate(&l.per_worker()) * crew[l.workers as usize - 1];
        let mut e = Entry::new(key(), "r", shipped(), false, 0);
        let mut seen = vec![];
        while let Some(l) = e.plan(&s) {
            seen.push(l);
            e.record(l, rate(&l), None, None, &s, 0);
            assert!(seen.len() < 30);
        }
        assert_eq!(seen[1].workers, 2, "the worker count is the first coordinate: {seen:?}");
        assert_eq!(e.incumbent, best, "{seen:?}");
        assert!(seen.iter().all(|l| l.workers <= 3 && l.threads <= budget(16, l.workers) && l.infer_total() <= budget(16, l.workers)), "{seen:?}");
    }

    #[test]
    fn layouts_without_a_worker_count_are_one_worker() {
        let old: Layout = serde_json::from_value(json!({"threads": 8, "infer_workers": 1, "infer_threads": 8, "groups": 4, "conc": 32})).unwrap();
        assert_eq!(old.workers, 1);
        assert_eq!(Layout::from_json(&json!({"threads": 8, "infer_workers": 1, "infer_threads": 8, "groups": 4, "conc": 32})).unwrap().workers, 1);
        assert_eq!(Layout::from_json(&json!({"workers": 2, "threads": 8, "infer_workers": 1, "infer_threads": 8, "groups": 4, "conc": 32})).unwrap().workers, 2);
        assert_eq!(Layout::from_json(&json!({"workers": 0, "threads": 8, "infer_workers": 1, "infer_threads": 8, "groups": 4, "conc": 32})), None);
        assert_eq!(Layout::from_json(&json!({"workers": 99, "threads": 8, "infer_workers": 1, "infer_threads": 8, "groups": 4, "conc": 32})), None);
        let two = Layout { workers: 2, threads: 12, infer_workers: 4, infer_threads: 3, groups: 6, conc: 1152 };
        assert_eq!(two.total_conc(), 2304);
        assert_eq!(two.to_json()["workers"], 2);
        assert!(two.describe().starts_with("2 workers x (12 search + 4 x 3 inference threads, 1,152 games in flight in 6 groups (192 rows))"), "{}", two.describe());
        // a hint for two workers fits this machine's bounds
        let hint = json!({"layout": two.to_json()});
        assert_eq!(hint_layout(Some(&hint), &crew_space()), Some(two));
        assert_eq!(hint_layout(Some(&hint), &space()).map(|l| l.workers), Some(1), "a machine held to one worker");
    }

    #[test]
    fn fingerprints() {
        let t = |v: Value| v.as_object().unwrap().clone();
        let a = net_fingerprint(&t(json!({"settings": {"sims": 8}, "gid_base": 1, "games": 4, "seed": 9})), Some(40 << 20), None);
        assert_eq!(a, net_fingerprint(&t(json!({"settings": {"sims": 8}, "gid_base": 77, "games": 1, "seed": 3})), Some((40 << 20) + 5), None), "per-lease fields and a few bytes don't count");
        assert_ne!(a, net_fingerprint(&t(json!({"settings": {"sims": 16}})), Some(40 << 20), None), "settings do");
        assert_ne!(a, net_fingerprint(&t(json!({"settings": {"sims": 8}})), Some(80 << 20), None), "the network's size does");
        // rev 20: a model variant of the same size is a network of its own; none keeps the fingerprint of before
        assert_ne!(a, net_fingerprint(&t(json!({"settings": {"sims": 8}})), Some(40 << 20), Some("gpu")), "a variant does");
        let before = format!("{}|{}", zerofarm_common::protocol::canonical_json(&json!({"settings": {"sims": 8}})), 40);
        assert_eq!(a, zerofarm_common::hash::sha256_hex(before.as_bytes())[..16], "the fingerprint of before rev 20");
        assert_eq!(a.len(), 16);
    }

    /// agent 0.3.14: a fork whose trainer knows more settings (a longer `setting_classes`), its play settings the same, keeps
    /// the network's fingerprint - and with it the tuned layout; a key remembered with the fingerprint of before is found.
    #[test]
    fn a_fork_with_the_same_play_settings_keeps_its_tuned_layout() {
        let t = |v: Value| v.as_object().unwrap().clone();
        let parent = t(json!({"settings": {"sims": 64, "setting_classes": {"sims": "play", "lr": "trainer"}}, "gid_base": 1, "games": 4, "seed": 9}));
        let fork = t(json!({"settings": {"sims": 64, "setting_classes": {"sims": "play", "lr": "trainer", "pool_variants": "trainer"}}, "gid_base": 5}));
        let other = t(json!({"settings": {"sims": 16, "setting_classes": {"sims": "play", "lr": "trainer"}}}));
        let size = Some(40 << 20);
        assert_eq!(net_fingerprint(&parent, size, None), net_fingerprint(&fork, size, None), "the trainer's list of settings doesn't count");
        assert_ne!(net_fingerprint(&parent, size, None), net_fingerprint(&other, size, None), "play settings do");
        assert_eq!(net_fingerprint(&parent, size, None), net_fingerprint(&t(json!({"settings": {"sims": 64}})), size, None));
        assert_ne!(net_fingerprint_legacy(&parent, size, None), net_fingerprint_legacy(&fork, size, None), "agents 0.3.1-0.3.13 counted it");
        // the legacy fingerprint is the one of before 0.3.14, byte for byte
        let before = format!("{}|{}", zerofarm_common::protocol::canonical_json(&json!({"settings": {"sims": 64, "setting_classes": {"sims": "play", "lr": "trainer"}}})), 40);
        assert_eq!(net_fingerprint_legacy(&parent, size, None), zerofarm_common::hash::sha256_hex(before.as_bytes())[..16]);
        // a tuned entry an older agent saved under the parent's legacy fingerprint: found for the parent (re-keyed), and
        // for the fork once the parent's lease saved it under the new key
        let new_key = Key { net: net_fingerprint(&parent, size, None), ..key() };
        let mut tuned = Entry::new(Key { net: net_fingerprint_legacy(&parent, size, None), ..key() }, "parent", Layout { workers: 2, threads: 12, infer_workers: 4, infer_threads: 3, groups: 6, conc: 1152 }, false, 7);
        tuned.state = State::Tuned;
        let mut c = Cache { v: 1, entries: vec![tuned.clone()] };
        assert!(c.get(&new_key).is_none(), "not under the new fingerprint");
        let found = c.find(&new_key, &net_fingerprint_legacy(&parent, size, None)).expect("the legacy key");
        assert_eq!((found.key.clone(), found.state, found.incumbent, found.run.as_str()), (new_key.clone(), State::Tuned, tuned.incumbent, "parent"));
        assert!(c.find(&new_key, &net_fingerprint_legacy(&fork, size, None)).is_none(), "the fork's legacy fingerprint is another");
        c.put(found);
        let for_fork = c.find(&Key { net: net_fingerprint(&fork, size, None), ..key() }, &net_fingerprint_legacy(&fork, size, None)).expect("the new key");
        assert_eq!((for_fork.state, for_fork.incumbent), (State::Tuned, tuned.incumbent));
        assert!(c.find(&Key { net: net_fingerprint(&other, size, None), ..key() }, &net_fingerprint_legacy(&other, size, None)).is_none(), "other play settings search afresh");
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
        let a = LoadMark { idle_ms: 1000, total_ms: 10_000, ours_ms: 500, dpc_ms: 100 };
        // 16,000 CPU-ms passed, 4,000 idle: 12,000 busy, 10,000 of them ours
        let b = LoadMark { idle_ms: 5000, total_ms: 26_000, ours_ms: 10_500, dpc_ms: 100 };
        assert!((a.other_since(&b) - 0.125).abs() < 1e-9);
        assert_eq!(a.other_since(&a), 0.0);
        let ours_more = LoadMark { ours_ms: 99_999, ..b };
        assert_eq!(a.other_since(&ours_more), 0.0, "never below 0");
        // agent 0.3.10: 800 of the 2,000 busy CPU-ms not ours were interrupts and DPCs (drivers): 1,200 other
        let drivers = LoadMark { dpc_ms: 900, ..b };
        assert!((a.other_since(&drivers) - 0.075).abs() < 1e-9);
    }

    #[test]
    fn thousands_separators() {
        assert_eq!(thousands(2304), "2,304");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1_234_567), "1,234,567");
        assert!(lay(8, 4, 2, 6, 2304).describe().contains("2,304 games in flight in 6 groups (384 rows)"));
    }
}

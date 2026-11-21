//! Running one lease (PROTOCOL 3 + 5): fetch and verify what it needs, start the worker, relay
//! its results, beat, yield/suspend/resume with the user's activity, give the lease back after
//! `slack_s` paused, honour cancel/give_back/expiry and new checkpoints, and close it with
//! `done`. Every network call runs off this loop's thread, so a yield is never delayed by I/O.
//! Eval leases (rev 5, PROTOCOL 5.2) bring every network their seats name in `checkpoints`;
//! all are downloaded and verified, then handed to the worker as `models`. Before anything is
//! downloaded the lease's plan is worked out (jobplan.rs, PROTOCOL 7): its signed definition, or
//! for a legacy lease the allowlist; the job line is built from that plan alone.

use crate::cache::{Cache, CacheError, CkptFiles};
use crate::engine::Engine;
use crate::jobplan::{Plan, PlanError, Recheck};
use crate::results::{compute_unfinished, ResultMeta, Source};
use crate::rolling::{Learned, MemPlan, Setup, VramCost};
use crate::status::Phase;
use crate::tune::{self, Layout, LoadMark, LoadTrack, Probe, Verdict};
use crate::worker::{CpuMark, SpawnOpts, WorkerMsg, WorkerProc};
use serde_json::{json, Map, Value};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};
use zerofarm_common::definition::Definition;
use zerofarm_common::http::{Backoff, CallError};
use zerofarm_common::manifest::{check_rel_path, Kind, VerifiedBuild};
use zerofarm_common::protocol::{BeatReq, BeatResp, CheckpointRef, CrashReq, DefinitionRef, DoneReq, ExtendReq, ExtendResp, Lease, Progress, Stats};
use zerofarm_common::manifest::GpuNeed;
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LeaseEnd {
    /// The worker finished its job.
    Done,
    /// Given back (slack exceeded, coordinator asked, memory pressure, setup failed, ...).
    GivenBack(String),
    Cancelled,
    Expired,
    /// The worker crashed, hung or broke the protocol.
    Failed(String),
    Revoked,
    /// The agent is shutting down.
    Quit,
    /// The worker could not be started (or died before `hello`): the lease went back and the
    /// engine waits before asking again (startfail.rs). `blocked`: Windows refused to run it.
    NotStarted { blocked: bool },
    /// The lease failed its definition or job checks (PROTOCOL 7): nothing ran, `done` said
    /// `refused`, and the engine waits before asking again.
    Refused(String),
}

enum Ev {
    /// A line of the worker of this generation (a rolling lease's restart starts generation 1, 2, ...: lines of an
    /// earlier worker are dropped).
    Worker(u32, WorkerMsg),
    Beat(Result<BeatResp, CallError>),
    Ckpt(Result<(CheckpointRef, CkptFiles), (CheckpointRef, String)>),
    /// rev 15 (PROTOCOL 10.4): the answer to a rolling lease's extension.
    Extend(Result<ExtendResp, CallError>),
    /// rev 15 (PROTOCOL 10.6): a rolling lease's new definition, verified (or why not), with the checkpoint it came with.
    Def(Box<Result<(DefinitionRef, Definition, CheckpointRef), Recheck>>),
    /// rev 15 (PROTOCOL 10.6): a rolling lease's update check: the agent version now ready, the evict list (rev 22: and
    /// the GPU vendors off).
    Update(Result<crate::update::News, String>),
}

/// rev 15 (PROTOCOL 10): a rolling lease while it runs.
struct Roll {
    setup: Setup,
    /// The released ids: `[lo, lo + released)`.
    released: u64,
    /// The worker's `hello` lists `rolling`: it gets the rolling job and `extend`s. Else the lease runs as a classic
    /// one (done after its released ids).
    active: bool,
    /// `finish` was sent: no more ids, generations or update checks.
    finishing: bool,
    /// The worker said `low`: ask for more ids when allowed.
    want_more: bool,
    extend_inflight: bool,
    /// No extension before this (at most one per `extend_every`; a `wait_s`; a backoff after an error).
    next_extend: Instant,
    backoff: Backoff,
    /// agent 0.3.13: the last extension released nothing (a `wait_s` - the coordinator's in-flight limit or the run's
    /// quota - or a failed call); cleared when ids are released and when a new main worker starts.
    refused: bool,
    /// The definition the lease runs under (at grant, then each accepted new one).
    def: DefinitionRef,
    def_inflight: bool,
    /// A `definition` crash report went out for this lease (at most one).
    def_reported: bool,
    update_inflight: bool,
    /// The memory the job is planned within (its provider's cost, PROTOCOL 10.6).
    mem: MemPlan,
    /// The job runs on a GPU (the measured cost is the run's GPU one).
    gpu: bool,
    /// The most the current worker committed (MB).
    peak_mb: u64,
    /// rev 19 (PROTOCOL 13.5): the most any worker of the lease committed per game in flight over the cost's base (MB),
    /// remembered at the end when more than planned.
    per_game_max: u64,
    /// agent 0.3.3: the most a worker committed per game at its steady state (`rolling::steady_reading`; 0: none reached
    /// it yet; agent 0.3.9: KB, `rolling::measured_per_game_kb`): remembered when lower than planned (`Learned::steady`,
    /// with a margin), and planned with when well lower (`calibrate`).
    steady_kb: u64,
    /// agent 0.3.3: the steady reading `calibrate` last acted on, and the most a reading over the plan was remembered at
    /// (`Learned::raise`, at once).
    calibrated: u64,
    raised: u64,
    /// agent 0.3.5: a GPU job's early reading (`rolling::early_reading`; 0: none yet; agent 0.3.9: KB), and whether
    /// `calibrate` acted on it.
    early_kb: u64,
    early_done: bool,
    /// agent 0.3.6: the lowest base reading of the lease's workers (`rolling::base_reading`; 0: none yet), and the one
    /// `calibrate` acted on.
    base_min: u64,
    base_applied: u64,
    /// The current worker's ids start at `lo + offset` (0 until a restart; a restart's worker gets the ids after
    /// the ones its predecessor started).
    offset: u64,
    /// Restarts for memory so far.
    restarts: u32,
    /// A restart for memory is due once the current worker is gone.
    restart: Option<Restart>,
    /// agent 0.3.4: a GPU job's full batch (rows, `rolling::gpu_batch` of its bench; 0 on the CPU).
    gpu_batch: u64,
    /// agent 0.3.4 (PROTOCOL 13.5): a GPU job's video memory plan and readings (None on the CPU, or when the adapter's
    /// size isn't known: then one worker).
    vram: Option<Vram>,
}

/// agent 0.3.4 (PROTOCOL 10.6, 13.5): a GPU job's video memory: the adapter, what its workers are planned within, the
/// cost of a worker (`rolling::Learned::vram_plan`) and what they were measured to hold.
#[derive(Debug, Clone)]
struct Vram {
    luid: u64,
    /// The adapter's dedicated memory (MB).
    total_mb: u64,
    /// What the lease's workers are planned within (MB): `rolling::vram_budget` at the lease's start.
    budget_mb: u64,
    /// Other programs' dedicated memory on the adapter at the start (MB; `rolling::VRAM_OTHERS_MB` when not measured).
    others_mb: u64,
    cost: VramCost,
    /// The most a worker held per row of its batch over the base, and at its steady state (MB), and the most remembered
    /// at once (`Learned::vram_raise`).
    per_row_max: u64,
    steady_per_row: u64,
    raised: u64,
    /// The last reading (at most one a second) and the adapter's memory in use then (every program's, MB).
    read_at: Option<Instant>,
    adapter_used_mb: u64,
}

/// rev 15 (PROTOCOL 10.6): a rolling lease's worker restarted with fewer games in flight, for memory; rev 19
/// (PROTOCOL 13): or with another layout, once its predecessor finished its games (a handoff memory didn't allow to
/// overlap).
#[derive(Debug, Clone)]
struct Restart {
    /// The games in flight of the next worker.
    conc: u64,
    /// The worker died (it ran into the job's memory limit): its games in flight are lost and the next worker's ids
    /// start after every id released so far. Else it finishes its games first (`finish`) and the next worker takes
    /// the released ids it never started.
    burn: bool,
    /// rev 19: the next worker's whole layout (a handoff); None: the current one with `conc` games in flight.
    layout: Option<Layout>,
    /// It counts toward `MAX_RESTARTS` (memory); a handoff doesn't.
    count: bool,
    /// rev 19: the next worker is a measured trial.
    measure: bool,
}

/// rev 19 (PROTOCOL 13, 13.5): a worker beside the main one. Either a member of the lease's crew (several workers serving
/// one lease): it plays a closed range of the lease's released ids `[lo + offset, lo + end)` (never extended: only the
/// main worker, the tail, gets `extend`s), switches networks with the main worker, pauses with it, and is finished once
/// it started all of its ids (a new worker takes its place); or `finishing` - after a layout handoff, a retirement or
/// the lease's end - it finishes its games in flight, takes no new games and no ids. Either way its results are
/// uploaded as any and the lease closes only after it ended.
struct Mate {
    worker: WorkerProc,
    wgen: u32,
    out_dir: String,
    end_ev: Option<Value>,
    eof: bool,
    last_msg: Instant,
    /// Its games in flight (its last `status`).
    in_flight: Option<u64>,
    /// Where the next worker's ids were taken to start (its unstarted tail, estimated): checked against its `end`.
    tail: Option<u64>,
    /// It starts no game: it finishes the ones in flight (a drainer).
    finishing: bool,
    /// Its ids, relative to the lease's first: `[offset, end)` (fixed).
    offset: u64,
    end: u64,
    /// Its own job line's layout (one worker).
    layout: Layout,
    /// It said `hello` and got its job line.
    job_sent: bool,
    spawned: Instant,
    pinged: bool,
    /// When it was last pinged for fresh counters (near the end of its range).
    pinged_at: Option<Instant>,
    /// Its last `status` counters: (leaves, games, games_started from its first id).
    counters: Option<(u64, u64, u64)>,
    /// After `finish`: its frozen `games_started` read twice - the ids it never started go to the spare ranges.
    reclaim: Option<Reclaim>,
    /// The oldest generation the next result chunk of it may have used (`x-gen`).
    gen_floor: Option<u64>,
    /// The most it committed (MB).
    peak_mb: u64,
    /// Its fatal error's message (a memory failure is told by it).
    fatal: Option<String>,
    /// agent 0.3.4: a GPU job's worker: `yield` was sent (when), it answered `yielded` (its session released), it is to be
    /// stopped now (`mates_tick`); its video memory (MB: Windows' counters, else its `vram_mb`) and its last `vram_mb`.
    yield_at: Option<Instant>,
    yielded: bool,
    want_stop: bool,
    vram_mb: u64,
    stats_vram: u64,
    /// agent 0.3.6: since when it finishes its games, and whether its last ones were ended (`tail_tick`).
    finishing_since: Option<Instant>,
    tail_quit: bool,
    /// agent 0.3.15: its progress watched (`Watch`), and since when a `resume` went out without its `resumed`.
    watch: Watch,
    resuming: Option<Instant>,
}

/// agent 0.3.15 (PROTOCOL 10.6 **A worker that stops playing**): a worker's progress, watched. zero-core's worker answers
/// a `resume` whose sessions can't be built again (2026-11-21: CUDA out of memory after a pause, another program holding
/// the GPU) with a non-fatal `error` and no `resumed`, and then plays nothing - but answers every `ping` with its frozen
/// counters, so the silence check (`Timings::hang_after`) never fires: a lease sat dead for 86 minutes until the
/// coordinator cancelled it. A worker is taken as failed (the paths a fatal error takes, `Lease::worker_failed`) when:
/// - it sent a non-fatal `error` while resuming and its counters didn't move for `Timings::resume_fail_after` after it;
/// - or, running or resuming, with games in flight (`games_started` over `games`; a worker that doesn't count them has
///   none here) its counters (leaves, games, games started) didn't move for `Timings::stall_after` while it got CPU.
#[derive(Debug, Clone)]
pub(crate) struct Watch {
    /// The worker watched (the main worker changes at restarts and handoffs: a new one starts a new watch).
    pid: u32,
    /// Its last counters (leaves, games, games_started) and when they last moved.
    seen: Option<(u64, u64, u64)>,
    moved: Instant,
    /// A non-fatal error it sent while resuming, and when.
    resume_error: Option<(String, Instant)>,
    /// Its last non-fatal error (told in the failure's message).
    last_error: Option<String>,
}

/// agent 0.3.15: the lease's event loop not ticking for this long (the PC slept, the agent was stopped) starts every
/// progress watch's clock again.
const WATCH_TICK_GAP: Duration = Duration::from_secs(30);

impl Watch {
    pub(crate) fn new(pid: u32) -> Watch {
        Watch { pid, seen: None, moved: Instant::now(), resume_error: None, last_error: None }
    }

    /// A `status`'s counters: when they moved, the clock starts again (and a resume error is forgotten: it plays).
    pub(crate) fn status(&mut self, leaves: Option<u64>, games: Option<u64>, started: Option<u64>) {
        let games = games.unwrap_or(0);
        let c = (leaves.unwrap_or(0), games, started.unwrap_or(games));
        if self.seen != Some(c) {
            if self.seen.is_some() {
                self.resume_error = None;
            }
            self.seen = Some(c);
            self.moved = Instant::now();
        }
    }

    /// The clock starts again (a resume sent, CPU starvation, a gap in the loop).
    pub(crate) fn restart_clock(&mut self) {
        self.moved = Instant::now();
    }

    /// A non-fatal `error`, `resuming` or not.
    pub(crate) fn error(&mut self, msg: &str, resuming: bool) {
        self.last_error = Some(msg.to_string());
        if resuming {
            self.resume_error = Some((msg.to_string(), Instant::now()));
        }
    }

    fn in_flight(&self) -> u64 {
        self.seen.map_or(0, |(_, g, st)| st.saturating_sub(g))
    }

    /// Whether the worker failed at `now` (`resuming`: a `resume` went out and no `resumed` came; `starved`: the PC gives
    /// it no CPU, which isn't a stall) -> why, for the failure's message.
    pub(crate) fn verdict(&self, now: Instant, resume_fail_after: Duration, stall_after: Duration, resuming: bool, starved: bool) -> Option<String> {
        if let Some((msg, at)) = &self.resume_error {
            if now.saturating_duration_since(*at) >= resume_fail_after && self.moved <= *at {
                return Some(format!("its resume failed ({msg}) and it played nothing for {} s after", now.saturating_duration_since(*at).as_secs()));
            }
        }
        let quiet = now.saturating_duration_since(self.moved);
        if starved || quiet < stall_after || !(resuming || self.in_flight() > 0) {
            return None;
        }
        let state = if resuming { "after a resume (no `resumed`)".to_string() } else { format!("with {} games in flight", self.in_flight()) };
        let err = self.last_error.as_deref().map(|e| format!("; its last error: {e}")).unwrap_or_default();
        Some(format!("it played nothing for {} s {state}{err}", quiet.as_secs()))
    }
}

/// rev 19 (PROTOCOL 13.5): where a finishing worker's ids stop: its `games_started` after `finish`, read until two agree.
#[derive(Debug, Clone)]
struct Reclaim {
    since: Instant,
    reads: Vec<u64>,
    last_ping: Option<Instant>,
}

impl Mate {
    fn active(&self) -> bool {
        !self.finishing
    }
}

/// rev 19 (PROTOCOL 13.5): a crew's tail (the main worker) is handed a new member only with at least this many of its
/// games in flight in unstarted ids (the member that takes the tail's place leaves it a closed range that long).
const RUNWAY_INFLIGHTS: u64 = 2;

/// agent 0.3.7: a GPU job's trials wait for its memory plan to be measured at most this long from the lease's start:
/// then they run on the plan as it is.
const GPU_TRIAL_WAIT: Duration = Duration::from_secs(15 * 60);

/// agent 0.3.13: a beat brought forward because a worker started or ended (`beat_soon`) goes out at most this soon after
/// the last one (the coordinator's beat bucket: one per 10 s after a burst, extensions included; PROTOCOL 1).
const BEAT_SOON_MIN: Duration = Duration::from_secs(10);

/// rev 19 (PROTOCOL 13): the tuning of a rolling lease's CPU job.
struct Tuning {
    space: tune::Space,
    entry: tune::Entry,
    /// The main worker's layout.
    current: Layout,
    /// The main worker started fresh with `current` and is being measured.
    probe: Option<(Probe, Instant)>,
    handoff: Option<Handoff>,
    /// The other programs' load: the last CPU mark, and their share of the CPUs over time since the lease started
    /// (agent 0.3.3: judged over windows, `tune::LoadTrack`; the track's clock starts at `origin`).
    load: Option<(Instant, LoadMark)>,
    loads: LoadTrack,
    origin: Instant,
    /// The running measurement's load window: where it started on the track's clock, and the mean load before it.
    probe_load: Option<(f64, Option<f64>)>,
    /// agent 0.3.3: when the last layout handoffs started (at most `tune::MAX_HANDOFFS` per `tune_handoff_window`).
    handoffs: Vec<Instant>,
    last_ping: Instant,
    /// A trial was discarded: back to the incumbent first, and no trial before this.
    revert_due: bool,
    no_trial_before: Instant,
    /// The platform's starting point (whether every worker honours `infer_workers`).
    defaults: tune::Defaults,
    /// rev 19 (PROTOCOL 13.5): the owner's worker count with `autotune` off: the layout is fixed, nothing is measured
    /// or remembered.
    fixed: bool,
    /// The most workers this lease may still run (lowered when a member fails with no restart left).
    crew_cap: u32,
    /// agent 0.3.4: the worker bounds' inputs (the owner's `workers`, the most the cores and RAM allow): the bounds are
    /// worked out again when the memory plan changes (`calibrate`).
    bounds: (Option<u32>, u32),
    /// agent 0.3.4: a GPU job's layout (PROTOCOL 13.1).
    gpu: bool,
    /// agent 0.3.5: a GPU job's start rule's inputs (an NVIDIA GPU, the bench's full batch): the start is picked again
    /// when the memory plan calibrates and nothing better was measured (`calibrate`).
    gpu_start: (bool, u64),
    /// agent 0.3.5: a trial may run now (the last tick's verdict: idle, quiet, not paused) - checked again when a trial's
    /// worker starts, not while it waits for room.
    strict: bool,
    /// rev 19 (PROTOCOL 13.5): the workers a measurement counts (generation, last counters): every member of the crew
    /// since it started - one that finished its range stays counted until it ends, then with its last counters (a
    /// rotation is the crew's normal work); one that fails discards it.
    probe_members: Vec<(u32, (u64, u64, u64))>,
    /// agent 0.3.13: since when the running measurement waits for game ids before its games in flight refilled (its
    /// probe is held that long, `Probe::hold`).
    hold_mark: Option<Instant>,
    /// agent 0.3.13: a trial put off for want of game ids (`put_off_probe`): its layout runs on unmeasured and is the next
    /// one measured - no handoff back to the incumbent meanwhile.
    postponed: Option<Layout>,
    /// agent 0.3.13: the lease is short of game ids and the tuner said it waits (once per shortage).
    ids_wait: bool,
}

/// rev 19: a layout handoff in progress, before the next worker starts: the main worker was told `finish`.
#[derive(Debug, Clone)]
struct Handoff {
    next: Layout,
    measure: bool,
    since: Instant,
    /// The main worker's `games_started` read after `finish` (twice the same: where its ids stop).
    reads: Vec<u64>,
    last_ping: Option<Instant>,
}

impl Tuning {
    /// Now on the load track's clock (seconds).
    fn now_s(&self) -> f64 {
        self.origin.elapsed().as_secs_f64()
    }

    /// A measurement of the main worker (and its crew, summed) starts now - `conc` games in flight in all: the window's
    /// load is judged against the mean load before it (agent 0.3.3, `tune::judge_trial`).
    fn start_probe(&mut self, conc: u64, t: &crate::Timings) {
        let now = self.now_s();
        let before = self.loads.mean(now - baseline_s(t), now).map(|(m, _)| m);
        self.probe = Some((Probe::new(conc, self.entry.phase, t.tune_warm.as_secs_f64(), t.tune_window.as_secs_f64()), Instant::now()));
        self.probe_members.clear();
        self.probe_load = Some((now, before));
    }
}

/// agent 0.3.3: the load before a trial is the mean over this long (one trial length).
fn baseline_s(t: &crate::Timings) -> f64 {
    (t.tune_warm + t.tune_window).as_secs_f64() * tune::LOAD_BASELINE_TRIALS
}

/// agent 0.3.3: load that lasts this long during a trial is sustained (30 s of the real 75-s window).
fn sustained_s(t: &crate::Timings) -> f64 {
    t.tune_window.as_secs_f64() * tune::LOAD_SUSTAINED_SHARE
}

/// What a worker is started with (kept: a rolling lease's restart starts another).
struct SpawnSpec {
    exe: PathBuf,
    cwd: PathBuf,
    cpu_job: bool,
    log: Arc<zerofarm_common::rotlog::RotatingFile>,
    tag: String,
    runtime_dir: Option<PathBuf>,
}

impl SpawnSpec {
    /// Starts the worker with its own memory limit (`mem_limit_mb`: the job's, or one worker's part of it); its lines
    /// reach `tx` as generation `gen`'s.
    fn spawn(&self, gen: u32, tx: Sender<Ev>, mem_limit_mb: u64) -> std::io::Result<WorkerProc> {
        WorkerProc::spawn(
            SpawnOpts {
                exe: &self.exe,
                args: &["serve"],
                cwd: &self.cwd,
                mem_limit_mb,
                cpu_job: self.cpu_job,
                log: self.log.clone(),
                tag: self.tag.clone(),
                runtime_dir: self.runtime_dir.as_deref(),
            },
            move |m| {
                let _ = tx.send(Ev::Worker(gen, m));
            },
        )
    }
}

/// A worker's fatal error that says it could not allocate memory (ONNX Runtime's "bad allocation", Rust's "memory
/// allocation of N bytes failed", ...).
fn alloc_failure(msg: &str) -> bool {
    let m = msg.to_ascii_lowercase();
    ["bad allocation", "bad_alloc", "out of memory", "memory allocation of", "failed to allocate"].iter().any(|k| m.contains(k))
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum WState {
    Starting,
    Running,
    Yielding(Instant),
    Paused(Instant),
    Resuming(Instant),
    Stopping(Instant),
}

#[derive(Debug, Clone, PartialEq)]
enum After {
    GiveBack(String),
    Cancel,
    Expire,
    Quit,
    Revoked,
    Kill(String),
}

pub fn describe_job(lease: &Lease) -> String {
    let run = lease.run.clone().or(lease.campaign.clone()).unwrap_or_default();
    match lease.kind.as_str() {
        "selfplay" => format!("self-play for {run}"),
        "eval" => format!("evaluation games for {run}"),
        "fuzz" => format!("fuzzing ({run})"),
        "diag" => "diagnostics".to_string(),
        k => format!("{k} ({run})"),
    }
}

fn games_total(lease: &Lease) -> u64 {
    let j = &lease.job;
    j.get("games").or_else(|| j.get("count")).and_then(Value::as_u64).unwrap_or(0)
}

/// What the worker should be told to load (paths under the cache).
struct Prepared {
    build: VerifiedBuild,
    runtime: Option<VerifiedBuild>,
    ckpt: Option<(CheckpointRef, CkptFiles)>,
    /// Eval (rev 5): every network of the lease's `checkpoints`, in its order.
    models: Vec<(CheckpointRef, CkptFiles)>,
}

/// A verified checkpoint still matches its hashes and declared sizes (model, and reference when
/// it has one).
fn ckpt_intact(c: &CheckpointRef, f: &CkptFiles) -> bool {
    Cache::reverify_file(&f.model, &c.sha256, f.model_size)
        && match (&f.reference, &c.reference_sha256) {
            (Some(p), Some(h)) => Cache::reverify_file(p, h, f.reference_size),
            _ => true,
        }
}

/// Stats as they leave the PC: no user name or profile path in any string (security review M5).
pub fn redacted(mut s: Stats) -> Stats {
    for v in s.values_mut() {
        zerofarm_common::redact::redact_json(v);
    }
    s
}

impl Engine {
    pub fn run_lease(&mut self, lease: Lease) -> LeaseEnd {
        if !zerofarm_common::is_safe_id(&lease.lease_id) {
            log::error!("refusing lease with unsafe id {:?}", lease.lease_id);
            return LeaseEnd::Failed("bad lease id".into());
        }
        let id = lease.lease_id.clone();
        log::info!("lease {id}: {} ({} games/seeds), build {}", lease.kind, games_total(&lease), lease.build.build_id);
        // Active before its directory exists, so the uploader never mistakes it for an orphan.
        self.uploader.set_active(Some(&id));
        if let Err(e) = self.results.begin(&lease) {
            log::error!("lease {id}: cannot create its results directory: {e}");
            self.uploader.set_active(None);
            return LeaseEnd::Failed(e.to_string());
        }
        // (macOS: a working Mac on AC power doesn't idle-sleep, `keep_awake`; nothing elsewhere)
        self.signals.lease_active(true);
        let end = self.lease_inner(&lease);
        self.signals.lease_active(false);
        // (rev 19: the tuning line describes the job running)
        crate::status::set(&self.status, |s| s.tuning = None);
        if end == LeaseEnd::Cancelled {
            self.results.discard(&id);
        }
        // rev 11 (PROTOCOL 9.6): a GPU pack that failed here goes (it is not downloaded again for this driver)
        for key in std::mem::take(&mut self.evict_after) {
            log::info!("removing {key} from the cache (it failed on this PC)");
            self.cache.evict(&key);
        }
        self.uploader.set_active(None);
        let _ = zerofarm_common::fsutil::remove_dir_all_retry(&self.cfg.layout.work().join(&id), Duration::from_secs(5));
        log::info!("lease {id} ended: {end:?}");
        end
    }

    /// Closes a lease that never got a worker: everything goes back.
    fn give_back_unstarted(&mut self, lease: &Lease, why: &str) -> LeaseEnd {
        let mut stats = Stats::new();
        stats.insert("error".into(), json!(why));
        let done = DoneReq {
            results: vec![],
            stats: redacted(stats),
            given_back: true,
            unfinished: compute_unfinished(lease, &[]),
            refused: None,
        };
        let _ = self.results.write_done(&lease.lease_id, &done);
        LeaseEnd::GivenBack(why.to_string())
    }

    /// A lease that failed its definition or job checks (PROTOCOL 7): nothing is downloaded or
    /// run; `done` says `refused: "definition"` (the coordinator works out the unfinished ids);
    /// a crash report of kind `definition` goes out once a day per definition and reason; the
    /// engine waits before asking again (1 min doubling to 1 h), so a bad definition never makes
    /// a lease storm.
    fn refuse(&mut self, lease: &Lease, why: &str) -> LeaseEnd {
        self.refuse_as(lease, why, "definition")
    }

    /// `refuse`, with the `refused` reason `done` sends: `definition` (PROTOCOL 7), rev 11 `runtime` (the agent's
    /// guard refused a runtime pack this PC shouldn't download, PROTOCOL 9.5) or rev 17 `platform` (a build of another
    /// platform, PROTOCOL 11.3).
    fn refuse_as(&mut self, lease: &Lease, why: &str, reason: &str) -> LeaseEnd {
        let id = &lease.lease_id;
        log::error!("lease {id}: refused ({reason}): {why}");
        let mut stats = Stats::new();
        stats.insert("error".into(), json!(format!("refused: {why}")));
        let done = DoneReq { results: vec![], stats: redacted(stats), given_back: true, unfinished: None, refused: Some(reason.into()) };
        if let Err(e) = self.results.write_done(id, &done) {
            log::error!("lease {id}: could not write done: {e}");
        }
        self.uploader.wake();
        let scope = match (&lease.definition, reason) {
            (_, "runtime") => format!("runtime {}", lease.runtime.as_ref().map(|r| r.build_id.as_str()).unwrap_or("none")),
            // rev 17 (PROTOCOL 11.3): once a day per build
            (_, "platform") => format!("platform {}", lease.build.build_id),
            (Some(d), _) => d.key.clone(),
            (None, _) => format!("legacy {}", lease.run.as_deref().or(lease.campaign.as_deref()).unwrap_or(&lease.kind)),
        };
        if crate::startfail::take_report(&self.cfg.layout, &format!("definition {scope}: {why}"), zerofarm_common::now_unix()) {
            self.report_crash(CrashReq {
                build_id: Some(lease.build.build_id.clone()),
                kind: "definition".into(),
                message: format!("lease refused ({scope}): {why}"),
                lease_id: Some(id.clone()),
                ..Default::default()
            });
        } else {
            log::info!("this refusal was already reported today");
        }
        self.start_failed(false);
        LeaseEnd::Refused(why.to_string())
    }

    /// A lease whose files couldn't be fetched, verified or set up: give it back and wait before
    /// asking again (the start-failure backoff, 1 min doubling to 1 h), never a lease storm. A
    /// checkpoint or build that keeps failing verification would otherwise be leased again at
    /// once, every time, at the coordinator's rate limit.
    fn setup_failed(&mut self, lease: &Lease, why: &str) -> LeaseEnd {
        let end = self.give_back_unstarted(lease, why);
        self.start_failed(false);
        end
    }

    /// Cache entries a running lease must keep: its build, runtime and every network it uses.
    fn keep_list(lease: &Lease) -> Vec<String> {
        let mut keep = vec![format!("builds/{}", lease.build.build_id)];
        if let Some(r) = &lease.runtime {
            keep.push(format!("builds/{}", r.build_id));
        }
        if let (Some(run), Some(c)) = (&lease.run, &lease.checkpoint) {
            keep.push(Cache::ckpt_key(run, c.gen));
        }
        // Eval: all of its networks stay in the cache while it runs (none evicts another).
        if let (Some(run), Some(cks)) = (&lease.run, &lease.checkpoints) {
            keep.extend(cks.iter().map(|c| Cache::ckpt_key(run, c.gen)));
        }
        keep
    }

    /// Beats `running` (0 games) from another thread until the returned pump is stopped: while a lease downloads its
    /// files, or benches its network before the first job line (PROTOCOL 9.6), it must not expire.
    fn beat_pump(&self, lease: &Lease) -> (Arc<AtomicBool>, std::thread::JoinHandle<()>) {
        let pump_stop = Arc::new(AtomicBool::new(false));
        let pump = {
            let (client, stop, id, beat_s) = (self.client.clone(), pump_stop.clone(), lease.lease_id.clone(), lease.beat_s.max(5));
            let total = games_total(lease);
            // rev 15: a rolling lease's beats carry its generation
            let gen = lease.rolling.as_ref().and(lease.checkpoint.as_ref()).map(|c| c.gen);
            std::thread::spawn(move || {
                let mut next = Instant::now() + Duration::from_secs(beat_s);
                while !stop.load(Ordering::SeqCst) {
                    if Instant::now() >= next {
                        let body = BeatReq { state: "running".into(), progress: Progress { games_done: 0, games_total: total }, stats: Stats::new(), gen };
                        let _ = client.post_json::<_, Value>(&format!("/v1/lease/{id}/beat"), &body);
                        next = Instant::now() + Duration::from_secs(beat_s);
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
            })
        };
        (pump_stop, pump)
    }

    /// rev 17 (PROTOCOL 11.3), before anything but manifests is downloaded: the lease's worker build and runtime pack
    /// must be of this agent's platform (a manifest without `platform` is `windows-x64`'s).
    fn platform_guard(&mut self, lease: &Lease) -> Result<(), PlanError> {
        let mut refs = vec![(&lease.build, Kind::Worker)];
        if let Some(rt) = &lease.runtime {
            refs.push((rt, Kind::Runtime));
        }
        for (r, kind) in refs {
            // A cached build's manifest: its signature checked, not its files (they are, before the worker starts), so
            // the check costs a lease nothing; else the manifest alone is fetched and verified.
            use zerofarm_common::manifest::{verify_manifest_bytes, MANIFEST_FILE, SIG_FILE};
            let dir = self.cfg.layout.cache_builds().join(&r.build_id);
            let cached = match (std::fs::read(dir.join(MANIFEST_FILE)), std::fs::read_to_string(dir.join(SIG_FILE))) {
                (Ok(b), Ok(sig)) if zerofarm_common::is_build_id(&r.build_id) => {
                    verify_manifest_bytes(&self.cfg.keys, &b, &sig).ok().filter(|(m, id)| *id == r.build_id && m.kind == kind).map(|(m, _)| m)
                }
                _ => None,
            };
            let m = match cached {
                Some(m) => m,
                None => self.signed_manifest(r, kind)?,
            };
            if m.platform_name() != zerofarm_common::PLATFORM {
                return Err(PlanError::Refused(format!(
                    "{} build {} is for {}; this agent runs on {}",
                    kind,
                    r.build_id,
                    m.platform_name(),
                    zerofarm_common::PLATFORM
                )));
            }
        }
        Ok(())
    }

    /// The agent's guard (rev 11, PROTOCOL 9.5), before anything but manifests is downloaded: a lease whose pack
    /// declares a GPU requirement (`gpu`) needs a self-play or eval job, GPU work allowed now, an adapter that meets the
    /// requirement, and the pack neither failed here nor held (9.8); an alternate pack, GPU or not, needs room under the
    /// disk cap. A pack without `gpu` - the CPU pack, a CPU alternate's (11.4), Windows ML's - is not GPU work, default
    /// or alternate: none of the GPU conditions apply to it (its job runs on the CPU unless 9.6's probe and bench, which
    /// run only while GPU work is allowed and the pack isn't held, pick a GPU provider for it). Agent 0.3.11 (PROTOCOL
    /// 9.8): a pack for a vendor with a provider restriction (an AMD GPU: DirectML only) must bring only those providers
    /// and must not ship Windows ML's catalog, and a pack for a vendor the owner turned off is refused. Returns the
    /// pack's requirement.
    fn runtime_guard(&mut self, lease: &Lease, plan: &Plan) -> Result<Option<GpuNeed>, PlanError> {
        use zerofarm_common::manifest::Kind;
        let Some(rt) = &lease.runtime else { return Ok(None) };
        let pack = self.signed_manifest(rt, Kind::Runtime)?;
        let need = pack.gpu.clone();
        if !plan.alternate && need.is_none() {
            return Ok(None);
        }
        let no = |m: String| Err(PlanError::Refused(m));
        if let Some(n) = &need {
            if !matches!(lease.kind.as_str(), "selfplay" | "eval") {
                return no(format!("a {} lease can't use runtime pack {} (only self-play and evaluation get a GPU pack)", lease.kind, rt.build_id));
            }
            if !self.decide(&[]).gpu {
                return no(format!("runtime pack {} is for GPU work, which isn't allowed here right now", rt.build_id));
            }
            // agent 0.3.11 (PROTOCOL 9.8): an AMD GPU runs DirectML only, never from a pack that ships Windows ML's catalog
            if let Some(only) = crate::profile::vendor_providers(&n.vendor) {
                if let Some(x) = n.providers.iter().find(|x| !only.contains(&x.as_str())) {
                    return no(format!("runtime pack {} brings {x} for {} GPUs, which run {} only here", rt.build_id, n.vendor, only.join(", ")));
                }
                if crate::probe::PackGpu::of(&pack).catalog {
                    return no(format!("runtime pack {} for {} GPUs ships {}, which could register other providers", rt.build_id, n.vendor, crate::probe::WINML_CATALOG_DLL));
                }
            }
            if !self.profile.gpus.iter().any(|g| n.met_by(&g.vendor, &g.driver)) && self.profile.gpus_off.iter().any(|g| n.met_by(&g.vendor, &g.driver)) {
                return no(format!("runtime pack {} is for {} GPUs, on which the owner turned GPU work off", rt.build_id, n.vendor));
            }
            if !self.profile.gpus.iter().any(|g| n.met_by(&g.vendor, &g.driver)) {
                let have: Vec<String> = self.profile.gpus.iter().map(|g| format!("{} {} (driver {})", g.vendor, g.name, g.driver)).collect();
                return no(format!(
                    "runtime pack {} needs a {} GPU with driver {} or newer; this PC has {}",
                    rt.build_id,
                    n.vendor,
                    n.min_driver.as_deref().unwrap_or("any"),
                    if have.is_empty() { "none".to_string() } else { have.join(", ") }
                ));
            }
            let probes = crate::probe::load(&self.cfg.layout);
            let akey = crate::probe::adapters_key(&self.profile.gpus);
            if crate::probe::pack_bad(&probes, &rt.build_id, &akey, Some(n.providers.as_slice()), zerofarm_common::now_unix()) {
                return no(format!("runtime pack {} failed on this PC's GPU and driver before", rt.build_id));
            }
            if crate::probe::held(&crate::probe::load_unfinished(&self.cfg.layout), &rt.build_id, &akey, zerofarm_common::now_unix()) {
                return no(format!("runtime pack {} stopped this PC during a GPU check before (the agent or the PC went down)", rt.build_id));
            }
        }
        let mut bytes = 0u64;
        for (r, kind) in [(&lease.build, Kind::Worker), (rt, Kind::Runtime)] {
            if !self.cfg.layout.cache_builds().join(&r.build_id).exists() {
                bytes += if kind == Kind::Runtime { pack.total_size() } else { self.signed_manifest(r, kind)?.total_size() };
            }
        }
        let budget = self.disk_budget_mb(&self.settings()) << 20;
        if bytes > budget {
            return no(format!("runtime pack {} and its worker need {} MB, more than the disk cap leaves ({} MB)", rt.build_id, bytes >> 20, budget >> 20));
        }
        Ok(need)
    }

    /// The job's shape for the batch size (PROTOCOL 9.6): (games, conc, groups) from the plan's per-lease `games` and
    /// its template's settings (`groups` default 1).
    fn job_shape(plan: &Plan) -> (u64, u64, u64) {
        let games = plan.job.get("games").and_then(Value::as_u64).unwrap_or(1);
        let st = plan.job.get("settings");
        let conc = st.and_then(|s| s.get("conc")).and_then(Value::as_u64).unwrap_or(64);
        let groups = st.and_then(|s| s.get("groups")).and_then(Value::as_u64).unwrap_or(1);
        (games, conc, groups)
    }

    /// The measured choice for the job (PROTOCOL 9.6), or for a rolling lease (rev 15, PROTOCOL 10.6) with its games in
    /// flight: the run's setting for the CPU and the GPU, "auto" worked out by `rolling.rs` (a fixed number on the
    /// CPU, a GPU's from its bench row, both within the memory plans `mem` (CPU, GPU), PROTOCOL 10.6; agent 0.3.12: a
    /// DirectML candidate's from DirectML's start, `rolling::dml_shape`).
    #[allow(clippy::too_many_arguments)]
    fn pick(bench: Option<&crate::probe::Bench>, gpus: &[zerofarm_common::protocol::Gpu], lease: &Lease, plan: &Plan, gpu_ok: bool, mem: (MemPlan, MemPlan), roll: Option<&Setup>) -> crate::probe::Choice {
        let (games, conc, groups) = Self::job_shape(plan);
        let allow = lease.providers.as_deref();
        match roll {
            None => crate::probe::choose(bench, gpus, allow, gpu_ok, games, conc, groups),
            Some(r) => {
                let cpu = crate::rolling::cpu_shape(r.cpu, groups, mem.0);
                // (agent 0.3.12: a DirectML candidate rated at the batch of DirectML's start - 576 rows for "auto" -, not at
                // the bench's CUDA-shaped target under a default memory plan)
                let gpu = |row: &crate::probe::BenchRow| {
                    if row.provider == "dml" {
                        crate::rolling::dml_shape(r.gpu, r.hi - r.lo, mem.1)
                    } else {
                        crate::rolling::gpu_shape(r.gpu, &row.rows, groups, mem.1)
                    }
                };
                crate::probe::choose_rolling(bench, gpus, allow, gpu_ok, cpu, &gpu, groups)
            }
        }
    }

    /// Before the first job line (rev 11, PROTOCOL 9.6): a pack with no probe here yet gets the built-in probe first
    /// (exactly the between-lease one, recorded as the pack's probe); then, if a GPU provider passed and there is no
    /// fresh bench of this run's network on the pack, the bench (a record of its own); then CPU or GPU by measured speed.
    #[allow(clippy::too_many_arguments)]
    fn choose_providers(&mut self, lease: &Lease, plan: &Plan, prep: &Prepared, gpu_ok: bool, threads: u32, mem: (MemPlan, MemPlan), need: Option<&GpuNeed>, roll: Option<&Setup>) -> crate::probe::Choice {
        let net = match lease.kind.as_str() {
            "selfplay" => prep.ckpt.as_ref().map(|(_, f)| f),
            "eval" => prep.models.first().map(|(_, f)| f),
            _ => None,
        };
        let (Some(rt), Some(net), Some(run)) = (&prep.runtime, net, lease.run.as_deref()) else {
            return Self::pick(None, &self.profile.gpus, lease, plan, gpu_ok, mem, roll);
        };
        let id = lease.lease_id.clone();
        let control = self.control.clone();
        let stop = move || control.quitting();
        let entry = prep.build.entry_path().expect("worker manifests have an entry");
        let akey = crate::probe::adapters_key(&self.profile.gpus);
        // 0.2.11: a pack the PC stopped during (a probe or bench never finished) is neither probed nor benched here, and
        // its job runs on the CPU
        let held = crate::probe::held(&crate::probe::load_unfinished(&self.cfg.layout), &rt.build_id, &akey, zerofarm_common::now_unix());
        if held {
            log::warn!("lease {id}: runtime pack {} stopped this PC during a GPU check before: no probe or bench, the CPU only", rt.build_id);
        }
        // agent 0.3.11 (PROTOCOL 9.8): where the pack's probe and bench run (one adapter on a PC with an AMD GPU, its own
        // providers), and the adapters its GPU work may use (an AMD GPU never with a pack that ships Windows ML's catalog)
        let pack = crate::probe::PackGpu::of(&rt.manifest);
        let sc = crate::probe::scope(&self.profile.gpus, &self.profile.gpus_off, &pack);
        if sc.is_none() && gpu_ok && !self.profile.gpus.is_empty() {
            log::info!("lease {id}: runtime pack {} may use none of this PC's GPUs (an AMD GPU runs DirectML only, never with Windows ML's catalog): the CPU only", rt.build_id);
        }
        let gpu_ok = gpu_ok && !held && sc.is_some();
        if let (true, Some(sc)) = (gpu_ok && !self.profile.gpus.is_empty() && crate::probe::find(&crate::probe::load(&self.cfg.layout), &rt.build_id, &akey).is_none(), sc.as_ref()) {
            log::info!("lease {id}: probing the execution providers of runtime pack {} (new here) with worker build {}", rt.build_id, prep.build.build_id);
            self.set_status(Phase::Working, "checking the GPU", Some(describe_job(lease)));
            let (stop_pump, pump) = self.beat_pump(lease);
            let p = crate::probe::run(&self.cfg.layout, &entry, &prep.build.build_id, &rt.build_id, &rt.dir, &self.profile.gpus, sc, self.worker_log.clone(), crate::probe::PROBE_TIMEOUT, &stop);
            stop_pump.store(true, Ordering::SeqCst);
            let _ = pump.join();
            if p.error.as_deref() != Some("stopped") {
                log::info!("lease {id}: provider probe of runtime pack {}: {} {}", rt.build_id, p.providers.join(", "), p.error.as_deref().unwrap_or(""));
                let mut p = p;
                p.need = need.map(|n| n.providers.clone());
                crate::probe::record(&self.cfg.layout, p);
                self.profile = (self.profile_fn)();
                self.profile_refreshed = Instant::now();
            }
        }
        let gpus = self.profile.gpus.clone();
        let akey = crate::probe::adapters_key(&gpus);
        // (agent 0.3.11: what the measured choice may pick from: the adapters this pack may use)
        let pack_gpus = crate::probe::pack_gpus(&gpus, &pack);
        let now = zerofarm_common::now_unix();
        let probes = crate::probe::load(&self.cfg.layout);
        let probe = crate::probe::find(&probes, &rt.build_id, &akey).filter(|p| p.error.is_none());
        let builtin: Vec<String> = probe.map(|p| p.providers.clone()).unwrap_or_default();
        let passed: Vec<String> = probe.map(|p| p.passed()).unwrap_or_default();
        // what the bench tries: the providers that passed here, and a GPU pack's own providers even when the built-in
        // probe failed them (its graph is not the job's; the bench is the factory's full self-test on the job's own
        // network and reference set, 2e-5, twice bit-identical, a node on the GPU - rows/s alone never count)
        let mut candidates = passed.clone();
        for p in need.map(|n| n.providers.as_slice()).unwrap_or(&[]) {
            if !candidates.contains(p) {
                candidates.push(p.clone());
            }
        }
        let candidates = crate::probe::ordered(candidates);
        let mut bench = crate::probe::find_bench(&crate::probe::load_benches(&self.cfg.layout), &rt.build_id, &akey, run, now).cloned();
        // (agent 0.3.11: only the scope's providers - an AMD GPU: DirectML)
        let candidates: Vec<String> = candidates.into_iter().filter(|p| p == "cpu" || sc.as_ref().is_some_and(|s| s.providers.contains(p))).collect();
        if let (true, None, Some(reference), Some(sc)) = (gpu_ok && probe.is_some() && crate::probe::has_gpu_provider(&candidates), &bench, net.reference.as_ref(), sc.as_ref()) {
            log::info!("lease {id}: measuring {run}'s network on runtime pack {} ({})", rt.build_id, candidates.join(", "));
            self.set_status(Phase::Working, "checking the GPU", Some(describe_job(lease)));
            let (stop_pump, pump) = self.beat_pump(lease);
            let spec = crate::probe::BenchSpec { model: &net.model, reference, run, threads, providers: candidates.clone() };
            let r = crate::probe::run_bench(&self.cfg.layout, &entry, &prep.build.build_id, &rt.build_id, &rt.dir, &gpus, sc, self.worker_log.clone(), crate::probe::PROBE_TIMEOUT, &stop, &spec);
            stop_pump.store(true, Ordering::SeqCst);
            let _ = pump.join();
            match r {
                Ok(b) if b.error.is_none() => {
                    // the verdict of the job's network goes into the pack's probe record next to the built-in one
                    let real: Vec<String> = b.results.iter().filter(|x| x.ok && x.provider != "cpu").map(|x| x.provider.clone()).collect();
                    crate::probe::record_real(&self.cfg.layout, &rt.build_id, &akey, need.map(|n| n.providers.as_slice()), &real);
                    for x in b.results.iter().filter(|x| x.provider != "cpu") {
                        let builtin_ok = builtin.contains(&x.provider);
                        let line = format!(
                            "runtime pack {} provider {} on {}: built-in probe {}, {run}'s network {}",
                            rt.build_id,
                            x.provider,
                            x.adapter.as_deref().unwrap_or("?"),
                            if builtin_ok { "PASS" } else { "FAIL" },
                            if x.ok { "PASS" } else { "FAIL" }
                        );
                        log::info!("lease {id}: {line}");
                        self.worker_log.write_line(&format!("bench: {line}"));
                    }
                    self.profile = (self.profile_fn)();
                    self.profile_refreshed = Instant::now();
                    bench = Some(b);
                }
                Ok(b) => {
                    log::warn!("lease {id}: the bench failed: {}", b.error.as_deref().unwrap_or(""));
                    crate::probe::record_real(&self.cfg.layout, &rt.build_id, &akey, need.map(|n| n.providers.as_slice()), &[]);
                    // kept a day, so it isn't run again every lease (the CPU meanwhile)
                    crate::probe::record_bench(&self.cfg.layout, b);
                }
                Err(_) => {}
            }
        }
        // A GPU pack none of whose providers passed here, on the built-in graph or the job's own network: marked bad for
        // this driver (its probe), run on the CPU, and evicted when the lease ends (PROTOCOL 9.6).
        if let Some(n) = need {
            let probes = crate::probe::load(&self.cfg.layout);
            if crate::probe::pack_bad(&probes, &rt.build_id, &akey, Some(&n.providers), now) {
                log::warn!("lease {id}: runtime pack {} failed on this GPU: running on the CPU, removing the pack afterwards", rt.build_id);
                self.evict_after.push(format!("builds/{}", rt.build_id));
            }
        }
        let c = Self::pick(bench.as_ref(), &pack_gpus, lease, plan, gpu_ok, mem, roll);
        if let Some(mut b) = bench.filter(|b| b.error.is_none()) {
            // what the coordinator sizes this run's GPU leases by (PROTOCOL 9.4: caps.gpu_games)
            b.gpu_games = c.gpu_games;
            crate::probe::record_bench(&self.cfg.layout, b);
        }
        log::info!("lease {id}: providers {:?} ({}); rows/s at the batch size {:?}; a GPU pays from {:?} games", c.providers, c.why, c.rows_per_s, c.gpu_games);
        c
    }

    fn prepare(&mut self, lease: &Lease, plan: &Plan) -> Result<Prepared, CacheError> {
        let s = self.settings();
        let cap = s.disk_cap_mb << 20;
        let keep = Self::keep_list(lease);
        // Beat while downloading so a big first download doesn't let the lease expire.
        let (pump_stop, pump) = self.beat_pump(lease);
        let control = self.control.clone();
        let stop = move || control.quitting();
        let result = (|| {
            let build = self.cache.ensure_build(&self.client, &lease.build, Kind::Worker, cap, &keep, &stop)?;
            let runtime = match &lease.runtime {
                Some(r) => Some(self.cache.ensure_build(&self.client, r, Kind::Runtime, cap, &keep, &stop)?),
                None => None,
            };
            let ckpt = match &lease.checkpoint {
                Some(c) => {
                    let run = lease.run.as_deref().ok_or_else(|| CacheError::Bad("checkpoint without a run".into()))?;
                    Some((c.clone(), self.cache.ensure_checkpoint(&self.client, run, c, plan.declared(c).as_ref(), cap, &keep, &stop)?))
                }
                None => None,
            };
            let mut models = Vec::new();
            for c in lease.checkpoints.iter().flatten() {
                let run = lease.run.as_deref().ok_or_else(|| CacheError::Bad("checkpoints without a run".into()))?;
                if models.iter().any(|(m, _): &(CheckpointRef, CkptFiles)| m.gen == c.gen) {
                    return Err(CacheError::Bad(format!("checkpoint {} is listed twice", c.gen)));
                }
                models.push((c.clone(), self.cache.ensure_checkpoint(&self.client, run, c, plan.declared(c).as_ref(), cap, &keep, &stop)?));
            }
            Ok(Prepared { build, runtime, ckpt, models })
        })();
        pump_stop.store(true, Ordering::SeqCst);
        let _ = pump.join();
        result
    }

    fn lease_inner(&mut self, lease: &Lease) -> LeaseEnd {
        let id = lease.lease_id.clone();
        self.set_status(Phase::Downloading, "work files", Some(describe_job(lease)));
        // What the worker may be told, before anything is downloaded (PROTOCOL 7).
        let plan = match self.plan(lease) {
            Ok(p) => p,
            Err(PlanError::Stopped) => {
                self.give_back_unstarted(lease, "agent quitting");
                return LeaseEnd::Quit;
            }
            Err(PlanError::Unavailable(why)) => {
                log::warn!("lease {id}: its definition can't be fetched right now: {why}");
                return self.setup_failed(lease, &format!("definition unavailable: {why}"));
            }
            Err(PlanError::Refused(why)) => return self.refuse(lease, &why),
        };
        // rev 17 (PROTOCOL 11.3): a build of another platform is refused before anything else is downloaded
        match self.platform_guard(lease) {
            Ok(()) => {}
            Err(PlanError::Stopped) => {
                self.give_back_unstarted(lease, "agent quitting");
                return LeaseEnd::Quit;
            }
            Err(PlanError::Unavailable(why)) => return self.setup_failed(lease, &format!("build manifest unavailable: {why}")),
            Err(PlanError::Refused(why)) => return self.refuse_as(lease, &why, "platform"),
        }
        // rev 11 (PROTOCOL 9.5): a GPU vendor's runtime pack only where it can run, before it is downloaded
        let need = match self.runtime_guard(lease, &plan) {
            Ok(n) => n,
            Err(PlanError::Stopped) => {
                self.give_back_unstarted(lease, "agent quitting");
                return LeaseEnd::Quit;
            }
            Err(PlanError::Unavailable(why)) => return self.setup_failed(lease, &format!("runtime pack manifest unavailable: {why}")),
            Err(PlanError::Refused(why)) => return self.refuse_as(lease, &why, "runtime"),
        };
        let prep = match self.prepare(lease, &plan) {
            Ok(p) => p,
            Err(CacheError::Stopped) => {
                self.give_back_unstarted(lease, "agent quitting");
                return LeaseEnd::Quit;
            }
            Err(e) => {
                log::error!("lease {id}: setup failed: {e}");
                return self.setup_failed(lease, &format!("setup failed: {e}"));
            }
        };
        // Compatibility (DESIGN 2.2): a build that needs a newer agent makes us update first.
        let m = &prep.build.manifest;
        let too_new = m.aw.is_some_and(|aw| aw != zerofarm_common::AW_VERSION)
            || m.min_agent.as_deref().is_some_and(|min| crate::update::is_newer(min, &self.cfg.version));
        if too_new {
            log::warn!("lease {id}: build needs aw {:?} / agent {:?}; updating first", m.aw, m.min_agent);
            self.last_update_check = None;
            return self.give_back_unstarted(lease, "build needs a newer agent");
        }
        // Re-verify right before starting (PROTOCOL 2).
        let build = match self.cache.reverify(&prep.build, Kind::Worker) {
            Ok(b) => b,
            Err(e) => {
                log::error!("lease {id}: build failed re-verification: {e}");
                return self.setup_failed(lease, &format!("build verification failed: {e}"));
            }
        };
        if let Some((c, f)) = &prep.ckpt {
            if !ckpt_intact(c, f) {
                return self.setup_failed(lease, "checkpoint verification failed");
            }
        }
        if let Some((c, _)) = prep.models.iter().find(|(c, f)| !ckpt_intact(c, f)) {
            log::error!("lease {id}: checkpoint {} failed re-verification", c.gen);
            return self.setup_failed(lease, &format!("checkpoint {} verification failed", c.gen));
        }
        let work = self.cfg.layout.work().join(&id);
        let _ = zerofarm_common::fsutil::remove_dir_all_retry(&work, Duration::from_secs(5));
        if let Err(e) = std::fs::create_dir_all(work.join("out")) {
            return self.setup_failed(lease, &format!("work dir: {e}"));
        }
        let d = self.decide(&[]);
        let caps = self.caps(&d);
        // rev 15 (PROTOCOL 10): a rolling lease (its block checked against its job; with a definition only, which is
        // what its new generations are verified against); else, or anything amiss, a classic lease
        let roll = match (crate::rolling::setup(lease), &lease.definition) {
            (Ok(Some(s)), Some(dref)) => {
                log::info!("lease {id}: rolling, ids [{}, {}) reserved, {} released, flush {} s", s.lo, s.hi, s.released, s.flush_s);
                Some((s, dref.clone()))
            }
            (Ok(Some(_)), None) => {
                log::warn!("lease {id}: a rolling lease without a definition: running it as a classic lease");
                None
            }
            (Ok(None), _) => None,
            (Err(e), _) => {
                log::warn!("lease {id}: its rolling part is not usable ({e}): running it as a classic lease");
                None
            }
        };
        // rev 11 (PROTOCOL 9.6): CPU or GPU by measured speed at the job's batch size (the providers this PC verified
        // with the lease's pack, narrowed by the lease's allowlist; the CPU alone when the GPU may not be used now)
        // rev 15 (PROTOCOL 10.6): the games in flight fit the job's memory limit at the run's cost per game (this PC's
        // measurement when it has one)
        let limit_mb = caps.ram_mb.max(256);
        let run_name = lease.run.clone().unwrap_or_default();
        // (agent 0.3.3: per run and worker build - a build not measured here starts from the platform's default; agent
        // 0.3.14: a run not measured here plans with another run's measurement of the same build, `Learned::find`)
        let build_id = lease.build.build_id.clone();
        let mem = (Learned::plan(&self.cfg.layout, &run_name, &build_id, false, limit_mb), Learned::plan(&self.cfg.layout, &run_name, &build_id, true, limit_mb));
        let choice = self.choose_providers(lease, &plan, &prep, d.gpu && !self.profile.gpus.is_empty(), caps.threads, mem, need.as_ref(), roll.as_ref().map(|(s, _)| s));
        let plan_providers = (choice.providers.clone(), choice.gpu.clone());
        let gpu_choice = plan_providers.0 != ["cpu"];
        if roll.is_some() {
            let m = if gpu_choice { mem.1 } else { mem.0 };
            let source = match (m.measured, Learned::borrowed(&self.cfg.layout, &run_name, &build_id, gpu_choice)) {
                (true, Some(from)) => format!(", measured here with run {from} (the same worker build)"),
                (true, None) => ", measured here".to_string(),
                (false, _) => String::new(),
            };
            let line = format!(
                "{} games in flight in {} groups (memory: {} MB limit, {} MB + {} MB per game{source})",
                choice.conc.unwrap_or(0),
                choice.groups.unwrap_or(0),
                limit_mb,
                m.cost.base_mb,
                m.cost.per_game_mb,
            );
            log::info!("lease {id}: {line}");
            self.worker_log.write_line(&format!("memory [{id}]: {line}"));
        }
        if plan_providers.0 != ["cpu"] {
            log::info!("lease {id}: providers {:?} on {}", plan_providers.0, plan_providers.1["name"].as_str().unwrap_or("?"));
        }
        // agent 0.3.4: a rolling GPU job's full batch (its bench's) and video memory plan (PROTOCOL 10.6, 13.5)
        let gpu_batch = if gpu_choice { crate::rolling::gpu_batch(&choice.gpu_rows) } else { 0 };
        let vram = if gpu_choice && roll.is_some() { self.vram_plan(lease, &choice) } else { None };
        if let Some(v) = &vram {
            let line = format!(
                "video memory: {} MB on the GPU, {} MB used by other programs, {} MB for the job's workers at {} MB + {} MB per batch row{}",
                v.total_mb,
                v.others_mb,
                v.budget_mb,
                v.cost.base_mb,
                v.cost.per_row_mb,
                if Learned::vram_plan(&self.cfg.layout, lease.run.as_deref().unwrap_or(""), &lease.build.build_id).1 { " (measured here)" } else { "" }
            );
            log::info!("lease {id}: {line}");
            self.worker_log.write_line(&format!("crew [{id}]: {line}"));
        }
        // rev 19 (PROTOCOL 13): a rolling CPU job's layout is tuned on this machine (`autotune` on); with `autotune` off
        // an owner's worker count above 1 (13.5) runs as a fixed layout; agent 0.3.4: a rolling GPU job's too - always a
        // layout (fixed with `autotune` off: the owner's worker count, else one), so its games in flight follow the
        // memory plan within the lease (`calibrate`)
        let mut tuning = None;
        let mut measure0 = false;
        let settings = self.settings();
        let crew_fixed = settings.workers.is_some_and(|n| n > 1);
        if let (Some((setup, _)), true, "selfplay") = (roll.as_ref(), gpu_choice, lease.kind.as_str()) {
            let (t, m) = self.gpu_tuning_for(lease, &plan, &prep, setup, &choice, gpu_batch, (limit_mb, mem.1.cost), vram.as_ref());
            log::info!(
                "lease {id}: GPU layout {}{} ({})",
                t.current.describe_on(true),
                if m { ", measured" } else { "" },
                if t.fixed { "fixed: autotune off".to_string() } else { t.entry.describe() }
            );
            measure0 = m;
            tuning = Some(t);
        } else if let (Some((setup, _)), false, true, "selfplay") = (roll.as_ref(), gpu_choice, settings.autotune || crew_fixed, lease.kind.as_str()) {
            // (agent 0.3.4: a CPU job on a GPU runtime pack takes no hint - the hint for that build and pack is most likely a
            // GPU layout now)
            let (t, m) = self.tuning_for(lease, &plan, &prep, setup, &choice, (limit_mb, mem.0.cost), need.is_none());
            log::info!(
                "lease {id}: CPU layout {}{} ({})",
                t.current.describe(),
                if m { ", measured" } else { "" },
                if t.fixed { "fixed: autotune off".to_string() } else { t.entry.describe() }
            );
            measure0 = m;
            // (an owner's worker count the memory doesn't hold with autotune off: one worker, the fixed rules of before)
            if t.fixed && t.current.workers <= 1 {
                log::info!("lease {id}: the memory limit holds one worker only: the fixed rules");
            } else {
                tuning = Some(t);
            }
        }
        let layout0 = tuning.as_ref().map(|t| t.current);
        // (a crew's workers each get their part of the job's memory limit, PROTOCOL 13.5)
        let limit0 = layout0.map_or(limit_mb, |l| (limit_mb / l.workers.max(1) as u64).max(256));
        let (tx, rx) = mpsc::channel::<Ev>();
        let entry = build.entry_path().expect("worker manifests have an entry");
        let spawn_spec = SpawnSpec {
            exe: entry,
            cwd: work.clone(),
            cpu_job: plan_providers.0 == ["cpu"],
            log: self.worker_log.clone(),
            tag: id.clone(),
            runtime_dir: prep.runtime.as_ref().map(|r| r.dir.clone()),
        };
        let spawn = match self.cfg.test_spawn_error.filter(|_| cfg!(debug_assertions)) {
            Some(code) => Err(std::io::Error::from_raw_os_error(code)),
            None => spawn_spec.spawn(0, tx.clone(), limit0),
        };
        let worker = match spawn {
            Ok(w) => w,
            Err(e) => return self.worker_not_started(lease, &e),
        };
        log::info!("lease {id}: worker pid {} started (memory limit {limit0} MB)", worker.pid);
        let mark = CpuMark::take(&worker);
        let mut run = Run {
            lease: lease.clone(),
            plan,
            work,
            prep: Prepared { build, ..prep },
            worker,
            tx,
            rx,
            state: WState::Starting,
            started: Instant::now(),
            last_msg: Instant::now(),
            pinged: false,
            beat_due: Instant::now() + Duration::from_secs(lease.beat_s.max(1)),
            beat_inflight: false,
            ckpt_inflight: false,
            games_done: 0,
            stats: Map::new(),
            provider: None,
            yields: 0,
            yield_ms_max: 0,
            resume_ms_max: 0,
            paused_ms: 0,
            failures: vec![],
            end_ev: None,
            after: None,
            threads: layout0.map_or(caps.threads, |l| l.threads),
            providers: plan_providers,
            conc: layout0.map(|l| l.conc).or(choice.conc),
            groups: layout0.map(|l| l.groups).or(choice.groups),
            batch_rows: layout0.map_or(choice.batch_rows, |l| l.batch()),
            rows_per_s: choice.rows_per_s.clone(),
            killed: false,
            eof: false,
            job_sent: false,
            first_start: !self.cfg.layout.cache().join("started").join(&lease.build.build_id).exists(),
            gen_floor: lease.checkpoint.as_ref().map(|c| c.gen),
            ckpt_failed: Vec::new(),
            mark,
            starved_for: Duration::ZERO,
            sample: (mark, Instant::now()),
            starved_ms: 0,
            stall_since: None,
            selftests: Map::new(),
            selftest_failed: false,
            crash_reported: false,
            hello_seen: false,
            spawn: spawn_spec,
            wgen: 0,
            out_dir: "out".into(),
            roll: roll.map(|(setup, def)| Roll {
                released: setup.released,
                setup,
                active: false,
                finishing: false,
                want_more: false,
                extend_inflight: false,
                next_extend: Instant::now(),
                backoff: Backoff::default(),
                refused: false,
                def,
                def_inflight: false,
                def_reported: false,
                update_inflight: false,
                mem: if gpu_choice { mem.1 } else { mem.0 },
                gpu: gpu_choice,
                peak_mb: 0,
                per_game_max: 0,
                steady_kb: 0,
                calibrated: 0,
                raised: 0,
                early_kb: 0,
                early_done: false,
                base_min: 0,
                base_applied: 0,
                offset: 0,
                restarts: 0,
                restart: None,
                gpu_batch,
                vram,
            }),
            // (a GPU job's inference threads are the worker's own: not in its job line)
            infer: layout0.filter(|_| !gpu_choice).map(|l| (l.infer_workers, l.infer_threads)),
            mates: Vec::new(),
            tuning: tuning.map(|mut t| {
                if measure0 {
                    t.start_probe(t.current.total_conc(), &self.cfg.timings);
                }
                t
            }),
            spawns: 0,
            main_exit: None,
            gens: 0,
            lease_limit_mb: limit_mb,
            spare: Vec::new(),
            counters: None,
            progress: None,
            counters_at: None,
            watch: Watch::new(0),
            watch_tick: Instant::now(),
            crew_changed: Instant::now(),
            last_counter_ping: None,
            vram_mb: 0,
            stats_vram: 0,
            finish_sent: None,
            tail_quit: false,
        };
        run.show_tuning(self);
        run.go(self)
    }

    /// rev 19 (PROTOCOL 13): a rolling CPU job's tuning: its key (worker build, runtime pack, network, hardware, OS,
    /// tuner), what may vary (search and inference threads within the platform's caps or the owner's `cpu_threads`,
    /// sessions, batches within the platform's rows, games in flight from half 10.6's choice to it, or the run's
    /// number), and the remembered search or a new one (from the coordinator's hint for this hardware class, else the
    /// platform's starting point with 10.6's games in flight and groups). The lease's first worker runs the next layout to
    /// measure when a trial may run now (a fresh start is a free trial), else the incumbent. -> (tuning, measured).
    #[allow(clippy::too_many_arguments)]
    fn tuning_for(&self, lease: &Lease, plan: &Plan, prep: &Prepared, setup: &Setup, choice: &crate::probe::Choice, mem: (u64, crate::rolling::MemCost), hint_ok: bool) -> (Tuning, bool) {
        let p = &self.profile;
        let hw = tune::Hw { cores: p.cores, logical: p.threads, perf: p.perf_cores, eff: p.eff_cores, ram_mb: p.ram_mb };
        let d = tune::defaults(tune::Platform::current(), &hw);
        let settings = self.settings();
        // (10.6's choice already fits the memory plan; the search only lowers it)
        let conc = choice.conc.unwrap_or(crate::rolling::CPU_INFLIGHT).max(1);
        let groups = choice.groups.unwrap_or(1);
        let (conc_min, conc_max) = match setup.cpu {
            crate::rolling::Inflight::N(_) => (conc, conc),
            crate::rolling::Inflight::Auto => ((conc / 2).max(1), conc),
        };
        // rev 19 (PROTOCOL 13.5): the worker count is searched from 1 to what the cores, the RAM and the memory plan allow,
        // or fixed by the owner's `workers`
        let space = tune::Space::new(&d, settings.cpu_threads, settings.workers, conc_min, conc_max, Some(mem));
        let run = lease.run.clone().unwrap_or_default();
        // (agent 0.3.14: the network's fingerprint without the trainer's `setting_classes`; a key remembered with the one of
        // before is found by it, `tune::Cache::find`)
        let (model_size, variant) = (prep.ckpt.as_ref().and_then(|(_, f)| f.model_size), prep.ckpt.as_ref().and_then(|(c, _)| c.variant.as_deref()));
        let legacy_net = tune::net_fingerprint_legacy(&plan.job, model_size, variant);
        let key = tune::Key {
            build: lease.build.build_id.clone(),
            runtime: lease.runtime.as_ref().map(|r| r.build_id.clone()),
            net: tune::net_fingerprint(&plan.job, model_size, variant),
            // (an owner's fixed worker count is searched apart from "auto")
            hw: match settings.workers {
                Some(n) => format!("{} | workers {n}", tune::hw_fingerprint(p)),
                None => tune::hw_fingerprint(p),
            },
            os: p.os.clone(),
            tuner: tune::TUNER_VERSION,
        };
        let now = zerofarm_common::now_unix();
        let start = space.fit(d.layout_for(d.workers.clamp(space.workers_min, space.workers_max), conc, groups));
        let fixed = !settings.autotune;
        let mut entry = match tune::Cache::load(&self.cfg.layout).find(&key, &legacy_net).filter(|_| !fixed) {
            Some(e) => e,
            None if fixed => tune::Entry::new(key, &run, start, false, now),
            None => match tune::hint_layout(lease.tuning_hint.as_ref().filter(|_| hint_ok), &space) {
                Some(h) => {
                    log::info!("lease {}: starting the tuning from the coordinator's hint: {}", lease.lease_id, h.describe());
                    tune::Entry::new(key, &run, h, true, now)
                }
                None => tune::Entry::new(key, &run, start, false, now),
            },
        };
        entry.run = run;
        // (agent 0.3.3: its layouts are compared as they run in this lease's bounds - a remembered incumbent the memory
        // plan no longer holds is its fitted layout)
        entry.bind(&space);
        if !fixed && entry.maybe_check(now) {
            log::info!("lease {}: the weekly check of the tuned layout is due", lease.lease_id);
        }
        let next = if self.tune_ok && !fixed { entry.plan(&space) } else { None };
        let current = next.unwrap_or_else(|| entry.wanted(&space));
        let t = Tuning {
            space,
            entry,
            current,
            probe: None,
            handoff: None,
            load: None,
            loads: LoadTrack::default(),
            origin: Instant::now(),
            probe_load: None,
            handoffs: Vec::new(),
            last_ping: Instant::now(),
            revert_due: false,
            no_trial_before: Instant::now(),
            defaults: d,
            fixed,
            crew_cap: tune::MAX_WORKERS_JSON,
            probe_members: Vec::new(),
            hold_mark: None,
            postponed: None,
            ids_wait: false,
            bounds: (settings.workers, d.max_workers),
            gpu: false,
            gpu_start: (false, 0),
            strict: false,
        };
        (t, next.is_some())
    }

    /// agent 0.3.4 (PROTOCOL 10.6, 13.1, 13.5): a rolling GPU job's video memory: the adapter its job line names, its
    /// dedicated memory, other programs' use of it now (Windows' counters, else `rolling::VRAM_OTHERS_MB`), the budget of
    /// the job's workers and a worker's cost (this PC's measurement for the run and worker build, else
    /// `rolling::GPU_VRAM`). None when the adapter's size isn't known (the job then runs one worker).
    fn vram_plan(&self, lease: &Lease, choice: &crate::probe::Choice) -> Option<Vram> {
        let luid_hex = choice.gpu.get("luid").and_then(Value::as_str)?;
        let luid = u64::from_str_radix(luid_hex.trim_start_matches("0x"), 16).ok()?;
        let g = self.profile.gpus.iter().find(|g| g.luid.as_deref().is_some_and(|l| l.eq_ignore_ascii_case(luid_hex)))?;
        if g.vram_mb == 0 {
            return None;
        }
        let others = self.signals.gpu_memory(luid, &[]).map_or(crate::rolling::VRAM_OTHERS_MB, |m| m.adapter_used_mb);
        let (cost, _) = Learned::vram_plan(&self.cfg.layout, lease.run.as_deref().unwrap_or(""), &lease.build.build_id);
        Some(Vram {
            luid,
            total_mb: g.vram_mb,
            budget_mb: crate::rolling::vram_budget(g.vram_mb, others),
            others_mb: others,
            cost,
            per_row_max: 0,
            steady_per_row: 0,
            raised: 0,
            read_at: None,
            adapter_used_mb: others,
        })
    }

    /// agent 0.3.4 (PROTOCOL 13.1): a rolling GPU job's layout - its key (worker build, runtime pack, network, the CPU,
    /// the GPU and its driver, the provider, OS, tuner), what may vary (the workers: 1 to 3, at most one per physical core,
    /// as many as the memory plan and the video memory plan hold; the search threads they share; one or two groups each;
    /// the games in flight from half the target to 1.5 times it, or a run's number), and the remembered search or a new
    /// one from the GPU's start (an NVIDIA GPU: the most workers that hold the target in all, `tune::Space::gpu_start`;
    /// agent 0.3.12: a DirectML job on any vendor's GPU DirectML's own crew, `tune::Space::dml_start`, keyed by
    /// `tune::DML_TUNER_VERSION`). No coordinator hint: a report doesn't say whether its layout ran on the GPU or on the
    /// CPU of the same build and pack (no wire change), and the GPU's start from its own bench is near. `autotune` off: the
    /// start for the owner's worker count (else one), fixed. -> (tuning, measured).
    #[allow(clippy::too_many_arguments)]
    fn gpu_tuning_for(&self, lease: &Lease, plan: &Plan, prep: &Prepared, setup: &Setup, choice: &crate::probe::Choice, batch: u64, mem: (u64, crate::rolling::MemCost), vram: Option<&Vram>) -> (Tuning, bool) {
        let p = &self.profile;
        let settings = self.settings();
        let fixed = !settings.autotune;
        // the search threads the workers share: the owner's `cpu_threads`, else 1.5 per physical core (at most the
        // logical processors)
        let threads = settings.cpu_threads.unwrap_or(((p.cores.max(1) as f64) * crate::rolling::GPU_THREADS_PER_CORE).round() as u32).clamp(1, p.threads.max(1));
        let provider = choice.providers.first().cloned().unwrap_or_default();
        // (agent 0.3.12: a DirectML job, any vendor, targets DirectML's crew - up to 3 workers of 1,152 games in flight in
        // two groups, within the lease's block -, not two of the bench's full batches)
        let dml = provider == "dml";
        let target = if dml { crate::rolling::dml_target(setup.gpu, setup.hi - setup.lo) } else { crate::rolling::gpu_target(setup.gpu, batch) };
        // (no video memory plan: one worker; `autotune` off without the owner's count: one)
        let set = match (vram.is_some(), fixed) {
            (false, _) => Some(1),
            (true, true) => Some(settings.workers.unwrap_or(1)),
            (true, false) => settings.workers,
        };
        let max_workers = tune::MAX_WORKERS.min(p.cores.max(1));
        let run_n = matches!(setup.gpu, crate::rolling::Inflight::N(_));
        let vram_plan = vram.map(|v| (v.budget_mb, v.cost));
        let space = if dml {
            tune::Space::new_dml(threads, set, max_workers, target, run_n, Some(mem), vram_plan)
        } else {
            tune::Space::new_gpu(threads, set, max_workers, target, run_n, Some(mem), vram_plan)
        };
        let run = lease.run.clone().unwrap_or_default();
        let luid = choice.gpu.get("luid").and_then(Value::as_str).unwrap_or("");
        let g = p.gpus.iter().find(|g| g.luid.as_deref().is_some_and(|l| l.eq_ignore_ascii_case(luid))).cloned().unwrap_or_default();
        let hw = tune::gpu_fingerprint(p, &g, &provider);
        // (agent 0.3.14: the network's fingerprint without the trainer's `setting_classes`; a key remembered with the one of
        // before is found by it, `tune::Cache::find`)
        let (model_size, variant) = (prep.ckpt.as_ref().and_then(|(_, f)| f.model_size), prep.ckpt.as_ref().and_then(|(c, _)| c.variant.as_deref()));
        let legacy_net = tune::net_fingerprint_legacy(&plan.job, model_size, variant);
        let key = tune::Key {
            build: lease.build.build_id.clone(),
            runtime: lease.runtime.as_ref().map(|r| r.build_id.clone()),
            net: tune::net_fingerprint(&plan.job, model_size, variant),
            hw: match settings.workers {
                Some(n) => format!("{hw} | workers {n}"),
                None => hw,
            },
            os: p.os.clone(),
            // (agent 0.3.12: DirectML's keys searched again from its own start)
            tuner: if dml { tune::DML_TUNER_VERSION } else { tune::GPU_TUNER_VERSION },
        };
        let now = zerofarm_common::now_unix();
        let start = space.gpu_start(g.vendor.eq_ignore_ascii_case("nvidia"), batch);
        let mut entry = match tune::Cache::load(&self.cfg.layout).find(&key, &legacy_net).filter(|_| !fixed) {
            Some(e) => e,
            None => tune::Entry::new(key, &run, start, false, now),
        };
        entry.run = run;
        entry.bind(&space);
        if !fixed && entry.maybe_check(now) {
            log::info!("lease {}: the weekly check of the tuned GPU layout is due", lease.lease_id);
        }
        let next = if self.tune_ok && !fixed { entry.plan(&space) } else { None };
        let current = next.unwrap_or_else(|| entry.wanted(&space));
        let hw0 = tune::Hw { cores: p.cores, logical: p.threads, perf: p.perf_cores, eff: p.eff_cores, ram_mb: p.ram_mb };
        let t = Tuning {
            space,
            entry,
            current,
            probe: None,
            handoff: None,
            load: None,
            loads: LoadTrack::default(),
            origin: Instant::now(),
            probe_load: None,
            handoffs: Vec::new(),
            last_ping: Instant::now(),
            revert_due: false,
            no_trial_before: Instant::now(),
            defaults: tune::defaults(tune::Platform::current(), &hw0),
            fixed,
            crew_cap: tune::MAX_WORKERS_JSON,
            probe_members: Vec::new(),
            hold_mark: None,
            postponed: None,
            ids_wait: false,
            bounds: (set, max_workers),
            gpu: true,
            gpu_start: (g.vendor.eq_ignore_ascii_case("nvidia"), batch),
            strict: false,
        };
        (t, next.is_some())
    }

    /// The worker process could not be created. Windows refusing it by policy (Smart App
    /// Control, AppLocker, Defender) is reported as `blocked` once a day per build; anything else
    /// as a `crash`. Either way the lease goes back and the engine waits before asking again.
    fn worker_not_started(&mut self, lease: &Lease, e: &std::io::Error) -> LeaseEnd {
        let id = &lease.lease_id;
        let build = lease.build.build_id.clone();
        let blocked = crate::startfail::blocked_code(e);
        match blocked {
            Some(code) => {
                let by = crate::startfail::blocked_by(code);
                log::error!("lease {id}: Windows refused to start the worker ({by}, os error {code}): {e}");
                if crate::startfail::take_blocked_report(&self.cfg.layout, &build, zerofarm_common::now_unix()) {
                    self.report_crash(CrashReq {
                        build_id: Some(build),
                        kind: "blocked".into(),
                        message: format!("Windows refused to start the worker: {by} ({e})"),
                        lease_id: Some(id.clone()),
                        os_error: Some(code as i64),
                        ..Default::default()
                    });
                } else {
                    log::info!("build {build} was already reported blocked today");
                }
            }
            None => {
                log::error!("lease {id}: could not start worker: {e}");
                self.report_crash(CrashReq {
                    build_id: Some(build),
                    kind: "crash".into(),
                    message: format!("could not start worker: {e}"),
                    lease_id: Some(id.clone()),
                    ..Default::default()
                });
            }
        }
        self.give_back_unstarted(lease, &format!("could not start worker: {e}"));
        self.start_failed(blocked.is_some());
        LeaseEnd::NotStarted { blocked: blocked.is_some() }
    }
}

struct Run {
    lease: Lease,
    /// The job template and per-lease fields (jobplan.rs): the only source of the job line.
    plan: Plan,
    work: PathBuf,
    prep: Prepared,
    worker: WorkerProc,
    tx: Sender<Ev>,
    rx: Receiver<Ev>,
    state: WState,
    started: Instant,
    last_msg: Instant,
    pinged: bool,
    beat_due: Instant,
    beat_inflight: bool,
    ckpt_inflight: bool,
    games_done: u64,
    stats: Stats,
    /// The execution provider the worker reports (`selftest`, `status`) once it has built its sessions; None until then
    /// (a worker says `""` before, zero-core's default: never stored).
    provider: Option<String>,
    yields: u64,
    yield_ms_max: u64,
    resume_ms_max: u64,
    paused_ms: u64,
    failures: Vec<Value>,
    end_ev: Option<Value>,
    after: Option<After>,
    threads: u32,
    /// The job line's `providers` and `gpu` (PROTOCOL 8).
    providers: (Vec<String>, Value),
    /// rev 11 (PROTOCOL 9.6): a GPU job's raised `conc`, the batch size as chosen and the measured rows/s.
    conc: Option<u64>,
    groups: Option<u64>,
    batch_rows: u64,
    rows_per_s: BTreeMap<String, f64>,
    killed: bool,
    eof: bool,
    /// The worker has its job (before that, nothing of the lease has been played).
    job_sent: bool,
    /// First run of this build on this PC (longer hello deadline).
    first_start: bool,
    /// Oldest checkpoint generation games of the next result chunk may have used (`x-gen`).
    gen_floor: Option<u64>,
    /// Checkpoints (gen, sha256) that failed to download or verify: not retried every beat.
    ckpt_failed: Vec<(u64, String)>,
    /// CPU accounting at the start of the current deadline window (starvation checks).
    mark: CpuMark,
    /// Time deadlines were extended because the worker was starved.
    starved_for: Duration,
    /// Starvation sampling window: CPU accounting and when it was taken.
    sample: (CpuMark, Instant),
    /// Stats `starved_ms`: time the (unpaused) worker got no CPU because the PC was busy.
    starved_ms: u64,
    /// Since when the worker has made no progress, paused or starved; past `slack_s` the lease
    /// is given back (PROTOCOL 3: starvation counts toward the slack like a pause).
    stall_since: Option<Instant>,
    /// The worker's `selftest` results per network generation (Stats `selftest`, rev 5).
    selftests: Map<String, Value>,
    /// A network failed its self-test (the worker refuses to play with it).
    selftest_failed: bool,
    /// A crash report was sent for this lease.
    crash_reported: bool,
    /// The worker said `hello` (it started): start failures are over.
    hello_seen: bool,
    /// What the worker was started with (a rolling lease's restart starts another).
    spawn: SpawnSpec,
    /// The current worker's generation (`Ev::Worker`).
    wgen: u32,
    /// The current worker's results directory under the work directory (`out`; a restart's worker gets its own).
    out_dir: String,
    /// rev 15 (PROTOCOL 10): a rolling lease.
    roll: Option<Roll>,
    /// rev 19 (PROTOCOL 13): the job line's `infer_workers` and `infer_threads` (a tuned layout; None: not sent).
    infer: Option<(u32, u32)>,
    /// rev 19: workers beside the main one - a crew's members (PROTOCOL 13.5) and workers finishing their games after a
    /// layout handoff.
    mates: Vec<Mate>,
    /// rev 19: the CPU layout's tuning (a rolling CPU job with `autotune` on).
    tuning: Option<Tuning>,
    /// Workers started after the first (memory restarts, handoffs): `done`'s unfinished ids are then worked out.
    spawns: u32,
    /// The main worker exited (its exit code) while drainers still finish their games.
    main_exit: Option<i32>,
    /// The last worker generation handed out (`Ev::Worker`): every spawn takes the next.
    gens: u32,
    /// The job's memory limit (MB): a crew's workers share it (PROTOCOL 13.5).
    lease_limit_mb: u64,
    /// rev 19 (PROTOCOL 13.5): released ids no worker owns and none started (relative `[a, b)`, from workers that
    /// finished early): a new member plays one before it would take the tail's place.
    spare: Vec<(u64, u64)>,
    /// The main worker's last `status` counters (leaves, games, games_started from its first id), and when.
    counters: Option<(u64, u64, u64)>,
    /// agent 0.3.3: the main worker's own `games` and `games_started` from its last `status` (None until it sent one;
    /// whether its memory reading is a steady one, `rolling::steady_reading`).
    progress: Option<(u64, u64)>,
    counters_at: Option<Instant>,
    /// agent 0.3.15: the main worker's progress watched (`Watch`), and the event loop's last tick (`WATCH_TICK_GAP`).
    watch: Watch,
    watch_tick: Instant,
    /// The crew changed (a worker joined or left): no trial for a while.
    crew_changed: Instant,
    last_counter_ping: Option<Instant>,
    /// agent 0.3.4: a GPU job's main worker's video memory (MB: Windows' counters, else its `vram_mb`) and its last
    /// `vram_mb`.
    vram_mb: u64,
    stats_vram: u64,
    /// agent 0.3.6: when the main worker was told `finish` (a handoff, a restart, giving way), and whether its last games
    /// were ended (`tail_tick`).
    finish_sent: Option<Instant>,
    tail_quit: bool,
}

impl Run {
    fn id(&self) -> &str {
        &self.lease.lease_id
    }

    fn merged_stats(&self) -> Stats {
        let mut s = self.stats.clone();
        let mut paused = self.paused_ms;
        if let WState::Paused(t) = self.state {
            paused += t.elapsed().as_millis() as u64;
        }
        s.insert("yields".into(), json!(self.yields));
        s.insert("yield_ms_max".into(), json!(self.yield_ms_max));
        s.insert("resume_ms_max".into(), json!(self.resume_ms_max));
        s.insert("paused_ms".into(), json!(paused));
        s.insert("starved_ms".into(), json!(self.starved_ms));
        // rev 11 (PROTOCOL 9.6)
        s.insert("batch_rows".into(), json!(self.batch_rows));
        if !self.rows_per_s.is_empty() {
            s.insert("rows_per_s".into(), json!(self.rows_per_s));
        }
        // rev 15 (PROTOCOL 10.6)
        if let Some(r) = &self.roll {
            // (rev 19, PROTOCOL 13.5: the crew's games in flight in all, and its workers; agent 0.3.13: with the games of the
            // workers finishing theirs - a handoff's old worker, a member that played its range -, which play them beside
            // the new ones: the coordinator's in-flight limit, rev 21, must leave the new worker its games beside them)
            let crew: Vec<&Mate> = self.mates.iter().filter(|m| m.active()).collect();
            let draining: u64 = self.mates.iter().filter(|m| m.finishing && m.end_ev.is_none()).filter_map(|m| m.in_flight).sum();
            let inflight = self.conc.map(|c| c + crew.iter().map(|m| m.layout.conc).sum::<u64>() + draining);
            s.insert("inflight".into(), json!(inflight));
            s.insert("released".into(), json!(r.released));
            if !crew.is_empty() || self.tuning.as_ref().is_some_and(|t| t.current.workers > 1) {
                s.insert("workers".into(), json!(1 + crew.len()));
            }
            // (agent 0.3.4: a GPU job's workers' video memory, as measured)
            if r.vram.is_some() {
                s.insert("vram_mb".into(), json!(self.vram_mb + self.mates.iter().map(|m| m.vram_mb).sum::<u64>()));
            }
        }
        if !self.selftests.is_empty() {
            let max = self.selftests.values().filter_map(|t| t.get("max_abs").and_then(Value::as_f64)).fold(None, |m: Option<f64>, x| {
                Some(m.map_or(x, |m| m.max(x)))
            });
            if let Some(m) = max {
                s.insert("selftest_max_abs".into(), json!(m));
            }
            s.insert("selftest".into(), Value::Object(self.selftests.clone()));
        }
        redacted(s)
    }

    /// The per-gen self-test results, for a crash report.
    fn selftest_note(&self) -> String {
        if self.selftests.is_empty() {
            String::new()
        } else {
            format!("; selftest per gen: {}", Value::Object(self.selftests.clone()))
        }
    }

    /// Every `starve_window` while the worker should be running: was it starved of CPU? Starved
    /// windows add to `starved_ms` and to the stall; a window with CPU ends the stall.
    fn sample_starvation(&mut self, eng: &Engine, t: &crate::Timings) {
        let running = matches!(self.state, WState::Starting | WState::Running | WState::Resuming(_));
        if !running || self.sample.1.elapsed() < t.starve_window {
            return;
        }
        let window = self.sample.1.elapsed();
        if eng.signals.worker_starved(&self.sample.0, &self.worker) {
            if self.stall_since.is_none() {
                log::info!("lease {}: the worker gets no CPU (the PC is busy)", self.id());
            }
            self.starved_ms += window.as_millis() as u64;
            self.stall_since.get_or_insert(self.sample.1);
        } else {
            self.stall_since = None;
        }
        self.sample = (CpuMark::take(&self.worker), Instant::now());
    }

    fn send(&mut self, v: Value) {
        // (agent 0.3.5: nothing to a worker that ended - its pipe closes: "The pipe is being closed", os error 232, when
        // a handoff's ping came after a drainer's `end`)
        if self.end_ev.is_some() || self.eof {
            return;
        }
        if v.get("op").and_then(Value::as_str) == Some("finish") {
            self.finish_sent.get_or_insert_with(Instant::now);
        }
        if let Err(e) = self.worker.send(&v) {
            log::warn!("lease {}: write to worker failed: {e}", self.id());
        }
    }

    fn kill(&mut self, why: &str) {
        log::warn!("lease {}: killing worker: {why}", self.id());
        for d in &mut self.mates {
            d.worker.kill();
        }
        self.worker.kill();
        self.killed = true;
        if self.after.is_none() {
            self.after = Some(After::Kill(why.to_string()));
        }
    }

    /// Ask the worker to stop now (finished games are flushed first; PROTOCOL 5 `quit`).
    fn stop(&mut self, after: After) {
        if self.after.is_none() {
            self.after = Some(after);
        }
        match self.state {
            WState::Stopping(_) => return,
            WState::Paused(t) => {
                self.paused_ms += t.elapsed().as_millis() as u64;
                self.worker.resume();
            }
            _ => {}
        }
        self.send(json!({"op": "quit"}));
        self.state = WState::Stopping(Instant::now());
        self.quit_mates();
    }

    /// A deadline window (started at `since`) has passed. True if the worker was starved by other
    /// programs (not hung) and the leniency budget allows waiting longer; the window restarts.
    fn starved_extend(&mut self, eng: &Engine, since: Instant, t: &crate::Timings) -> bool {
        if t.starve_cap.is_zero() || self.starved_for >= t.starve_cap || !eng.signals.worker_starved(&self.mark, &self.worker) {
            return false;
        }
        if self.starved_for.is_zero() {
            log::info!("lease {}: worker is getting no CPU (the PC is busy); waiting instead of timing out", self.id());
        }
        self.starved_for += since.elapsed();
        self.mark = CpuMark::take(&self.worker);
        true
    }

    /// The job uses a GPU (it yields when GPU work stops being allowed): the provider the worker reports once it has
    /// built its sessions, until then what the agent chose for the job (its job line's `providers`). A worker that has
    /// no sessions yet says `provider: ""` - unknown, never a GPU: a CPU job taken for a GPU one would yield on every
    /// machine without a usable GPU (every Mac) and stay suspended, its sessions never built.
    fn gpu_job(&self) -> bool {
        match self.provider.as_deref() {
            Some(p) => p != "cpu",
            None => self.providers.0.iter().any(|p| p != "cpu"),
        }
    }

    /// rev 22 (PROTOCOL 9.8): the vendor of this GPU job's adapter (its job line's `gpu`) when the owner turned it off
    /// (the profile lists the adapter under `gpus_off`); None for a CPU job or a vendor that is on.
    fn gpu_vendor_off(&self, eng: &Engine) -> Option<String> {
        if !self.gpu_job() {
            return None;
        }
        let luid = self.providers.1.get("luid").and_then(Value::as_str)?;
        eng.profile.gpus_off.iter().find(|g| g.luid.as_deref().is_some_and(|l| l.eq_ignore_ascii_case(luid))).map(|g| g.vendor.clone())
    }

    /// A provider the worker reported (`selftest`, `status`): stored once it names one (`""`: no sessions yet).
    fn note_provider(&mut self, v: Option<&Value>) {
        if let Some(p) = v.and_then(Value::as_str).filter(|p| !p.is_empty()) {
            self.provider = Some(p.to_string());
        }
    }

    fn go(&mut self, eng: &mut Engine) -> LeaseEnd {
        eng.set_status(Phase::Working, "", Some(describe_job(&self.lease)));
        let exit_code = loop {
            match self.rx.recv_timeout(eng.cfg.timings.tick) {
                Ok(ev) => self.on_event(eng, ev),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {}
            }
            self.reap_mates(eng);
            // rev 19 (PROTOCOL 13): the main worker is gone; workers handed off from (and a crew's members, 13.5) finish
            // their games first
            if let Some(code) = self.main_exit {
                // (agent 0.3.3: a restart that waited for memory starts once the drain made room)
                if self.restart_pending() && self.restart_room() && self.restart_worker(eng) {
                    self.main_exit = None;
                    continue;
                }
                if self.mates.is_empty() {
                    break code;
                }
                self.wind_down(eng);
                continue;
            }
            if let Some(code) = self.worker.try_wait() {
                // Drain what the worker wrote before exiting (its `end`, last results).
                let until = Instant::now() + Duration::from_secs(2);
                while !self.eof && Instant::now() < until {
                    if let Ok(ev) = self.rx.recv_timeout(Duration::from_millis(50)) {
                        self.on_event(eng, ev);
                    }
                }
                while let Ok(ev) = self.rx.try_recv() {
                    self.on_event(eng, ev);
                }
                // rev 15 (PROTOCOL 10.6): a rolling worker that died at the job's memory limit without a word (an
                // abort on a failed allocation) is restarted smaller, as one that said so
                if self.end_ev.is_none() && !self.killed && !self.memory_failure(eng, &format!("worker exited with code {code}")) {
                    // rev 19 (PROTOCOL 13.5): a crew's main worker that crashed is replaced; the others go on
                    self.crew_crash(eng, &format!("worker exited with code {code} without `end`"), Some(code as i64), false);
                }
                // rev 19: a handoff whose old worker finished its games before the next one could start beside it
                self.handoff_after_exit(eng);
                // agent 0.3.3: the next worker starts only when its plan fits beside the workers still finishing their
                // games; until then the lease winds down with them
                if self.restart_pending() && !self.restart_room() {
                    log::info!("lease {}: the next worker waits for memory: the workers finishing their games beside it hold too much", self.id());
                    self.main_exit = Some(code);
                    continue;
                }
                if self.restart_worker(eng) {
                    continue;
                }
                if !self.mates.is_empty() {
                    self.main_exit = Some(code);
                    continue;
                }
                break code;
            }
            self.tick(eng);
        };
        self.finish(eng, exit_code)
    }

    fn tick(&mut self, eng: &mut Engine) {
        let t = eng.cfg.timings.clone();
        eng.maybe_send_saved_crashes();
        if eng.quitting() && !matches!(self.state, WState::Stopping(_)) {
            self.stop(After::Quit);
        }
        let d = eng.decide(&self.worker_pids());
        let must_pause = !d.work || (self.gpu_job() && !d.gpu);
        // (agent 0.3.15: a gap in the loop - the PC slept - is no worker's stall)
        if self.watch_tick.elapsed() > WATCH_TICK_GAP {
            self.watch.restart_clock();
            for m in &mut self.mates {
                m.watch.restart_clock();
            }
        }
        self.watch_tick = Instant::now();
        // rev 22 (PROTOCOL 9.8): the owner turned GPU work on this job's GPU's vendor off (a rolling lease's update check):
        // the lease goes back at once, its GPU released
        if let Some(v) = self.gpu_vendor_off(eng) {
            if !matches!(self.state, WState::Stopping(_)) && self.after.is_none() {
                log::warn!("lease {}: GPU work on {v} GPUs was turned off; giving the lease back", self.id());
                self.stop(After::GiveBack(format!("GPU work on {v} GPUs was turned off")));
            }
        }
        self.sample_starvation(eng, &t);
        if let Some(since) = self.stall_since {
            if since.elapsed() > Duration::from_secs(self.lease.slack_s) && !matches!(self.state, WState::Stopping(_)) {
                let what = if matches!(self.state, WState::Paused(_) | WState::Yielding(_)) { "paused" } else { "starved of CPU" };
                log::info!("lease {}: {what} longer than slack ({} s); giving it back", self.id(), self.lease.slack_s);
                self.stop(After::GiveBack(format!("{what} longer than slack")));
            }
        }
        match self.state {
            WState::Starting => {
                let limit = if self.first_start { t.hello_first } else { t.hello };
                if self.started.elapsed() > limit {
                    if self.starved_extend(eng, self.started, &t) {
                        self.started = Instant::now();
                    } else {
                        self.report(eng, "crash", &format!("worker sent no hello within {} s", limit.as_secs()), None);
                        self.kill("no hello");
                    }
                }
            }
            WState::Running => {
                if must_pause {
                    let why = d.block.map(|b| b.describe()).unwrap_or("GPU is busy");
                    log::info!("lease {}: yielding ({why})", self.id());
                    self.send(json!({"op": "yield"}));
                    self.yields += 1;
                    self.mark = CpuMark::take(&self.worker);
                    self.state = WState::Yielding(Instant::now());
                    self.stall_since.get_or_insert(Instant::now());
                    eng.set_status(Phase::Paused, why, Some(describe_job(&self.lease)));
                } else {
                    self.check_alive(eng, &t);
                    self.check_progress(eng, &t);
                }
            }
            WState::Yielding(since) => {
                if since.elapsed() > t.yield_answer {
                    let starved = !t.starve_cap.is_zero() && eng.signals.worker_starved(&self.mark, &self.worker);
                    if starved && !self.gpu_job() {
                        // It got no CPU to answer with; a CPU worker holds no VRAM: just suspend it.
                        // (It reads the pending `yield` after `resume` and answers both in order.)
                        log::info!("lease {}: worker starved of CPU before answering yield; suspending it", self.id());
                        self.worker.suspend();
                        self.state = WState::Paused(Instant::now());
                    } else {
                        self.report(eng, "crash", "worker did not answer yield within 2 s", None);
                        self.kill("yield timeout");
                    }
                }
            }
            WState::Paused(since) => {
                if d.mem_pressure {
                    log::info!("lease {}: memory pressure while paused; giving it back", self.id());
                    self.stop(After::GiveBack("memory pressure".into()));
                } else if !must_pause {
                    self.paused_ms += since.elapsed().as_millis() as u64;
                    self.worker.resume();
                    self.send(json!({"op": "resume"}));
                    self.state = WState::Resuming(Instant::now());
                    self.last_msg = Instant::now();
                    // (agent 0.3.15: its progress is watched from the resume on)
                    self.main_watch().restart_clock();
                    // The stall lasts until a starvation window shows the worker getting CPU.
                    self.sample = (CpuMark::take(&self.worker), Instant::now());
                    eng.set_status(Phase::Working, "", Some(describe_job(&self.lease)));
                }
            }
            WState::Resuming(_) => {
                self.check_alive(eng, &t);
                self.check_progress(eng, &t);
            }
            WState::Stopping(since) => {
                if since.elapsed() > t.quit_grace && !self.killed {
                    self.kill("no end after quit");
                }
            }
        }
        // Beats run on their own thread; results come back as events.
        let paused = matches!(self.state, WState::Paused(_));
        self.maybe_beat(eng, paused);
        self.roll_tick(eng);
        self.mem_check(eng);
        self.vram_check(eng);
        self.calibrate(eng);
        self.mates_tick(eng, &t, must_pause);
        self.tune_tick(eng, &t, must_pause);
        self.crew_tick(eng, &t, must_pause);
        self.tail_tick(eng, &t, must_pause);
    }

    /// agent 0.3.13 (PROTOCOL 10.4, 10.6): the released ids a rolling lease's workers still hold - the ones they play or
    /// will play: the main worker's range less its finished games (also while it finishes them for a handoff or a
    /// restart: its unstarted ids are the next worker's), a crew member's likewise, a worker finishing its games only
    /// those in flight (its unstarted ids are another worker's, spare or given up; its whole range while it may still give
    /// them to the spare ranges), and the spare ranges a crew member can still play. The rest of the released ids are
    /// finished or given up - the games in flight of a worker that ended them or failed, ids no worker will play - and go
    /// back to the pool when the lease closes (`done`'s unfinished ids are worked out from the credited ones). None: not a
    /// rolling job.
    fn ids_held(&self) -> Option<u64> {
        let r = self.roll.as_ref().filter(|r| r.active)?;
        let mut held = 0u64;
        // (the main worker, unless it ended and no worker follows it)
        if self.main_exit.is_none() || r.restart.is_some() {
            let fin = self.progress.map_or(0, |(g, _)| g);
            held += r.released.saturating_sub(r.offset).saturating_sub(fin);
        }
        for m in self.mates.iter().filter(|m| m.end_ev.is_none()) {
            let (fin, started) = m.counters.map_or((0, None), |(_, g, st)| (g, Some(st)));
            let range = m.end.saturating_sub(m.offset).saturating_sub(fin);
            held += match (m.finishing && m.reclaim.is_none(), started, m.in_flight) {
                (true, Some(st), _) => range.min(st.saturating_sub(fin)),
                (true, None, Some(n)) => range.min(n),
                _ => range,
            };
        }
        if self.crew_target() > 1 {
            let per = self.conc.unwrap_or(1).max(1);
            held += self.spare.iter().filter(|&&(a, b)| b - a >= RUNWAY_INFLIGHTS * per).map(|&(a, b)| b - a).sum::<u64>();
        }
        Some(held.min(r.released))
    }

    /// The beats' `progress.games_done`: the games the lease's workers finished; agent 0.3.13, a rolling lease: the
    /// released ids it is done with - finished or given up (`ids_held`) - when more. The coordinator counts a rolling
    /// lease's unfinished ids as its released ids less the more of this and its credited ones (rev 21, PROTOCOL 10.4):
    /// given-up ids counted as unfinished until the lease closed made its in-flight limit refuse every id (agents
    /// 0.3.6-0.3.12: a DirectML PC's handoffs each ended a worker's last games, ~8 games an hour).
    fn games_done_report(&self) -> u64 {
        match (self.roll.as_ref(), self.ids_held()) {
            (Some(r), Some(held)) => self.games_done.max(r.released.saturating_sub(held)),
            _ => self.games_done,
        }
    }

    /// agent 0.3.13: a worker started or ended: the next beat goes out soon (`BEAT_SOON_MIN` after the last at the
    /// earliest), so the coordinator's in-flight limit sees the lease's games in flight and the ids it is done with
    /// before the new worker's first ask - not up to a beat interval later.
    fn beat_soon(&mut self) {
        if self.roll.is_none() {
            return;
        }
        let now = Instant::now();
        let last = self.beat_due.checked_sub(Duration::from_secs(self.lease.beat_s.max(1))).unwrap_or(now);
        self.beat_due = self.beat_due.min((last + BEAT_SOON_MIN).max(now));
    }

    /// agent 0.3.13: the lease waits for game ids its worker asked for that the coordinator didn't release (a `wait_s`:
    /// its in-flight limit or the run's quota; or a failed call).
    fn ids_refused(&self) -> bool {
        self.roll.as_ref().is_some_and(|r| r.active && !r.finishing && r.refused && r.want_more)
    }

    /// agent 0.3.13: the lease is short of game ids: refused (`ids_refused`) while its main worker's games in flight
    /// aren't full - fewer than `tune::REFILL_SHARE` of them, what a trial's fresh worker must reach to be measured.
    fn ids_short(&self) -> bool {
        let need = ((tune::REFILL_SHARE * self.conc.unwrap_or(1).max(1) as f64).floor() as u64).max(1);
        self.ids_refused() && self.progress.is_none_or(|(g, st)| st.saturating_sub(g) < need)
    }

    /// A beat when one is due (on its own thread; the answer comes back as an event).
    fn maybe_beat(&mut self, eng: &Engine, paused: bool) {
        if !self.beat_inflight && Instant::now() >= self.beat_due && !matches!(self.after, Some(After::Cancel | After::Revoked)) {
            self.beat_inflight = true;
            self.beat_due = Instant::now() + Duration::from_secs(self.lease.beat_s.max(1));
            let body = BeatReq {
                state: if paused { "paused" } else { "running" }.into(),
                progress: Progress { games_done: self.games_done_report(), games_total: games_total(&self.lease) },
                stats: self.merged_stats(),
                // rev 15 (PROTOCOL 10.6): the newest generation the worker plays
                gen: self.roll.as_ref().and(self.prep.ckpt.as_ref()).map(|(c, _)| c.gen),
            };
            let (client, tx, id) = (eng.client.clone(), self.tx.clone(), self.lease.lease_id.clone());
            std::thread::spawn(move || {
                let r = client.post_json::<_, BeatResp>(&format!("/v1/lease/{id}/beat"), &body);
                let _ = tx.send(Ev::Beat(r));
            });
        }
    }

    /// rev 15 (PROTOCOL 10.6), each tick of a rolling lease: the worker's committed memory. At `HIGH_SHARE` of the
    /// job's limit the worker finishes its games and is started again with fewer in flight (`restart_worker`), and the
    /// run's cost per game measured here is remembered (`Learned`); one that can't have fewer finishes the lease.
    fn mem_check(&mut self, eng: &Engine) {
        let limit = self.worker.mem_limit_mb;
        let live = matches!(self.state, WState::Running | WState::Resuming(_)) && self.after.is_none();
        // (rev 19: a worker handing off to the next layout is finishing its games anyway)
        if self.tuning.as_ref().is_some_and(|t| t.handoff.is_some()) {
            return;
        }
        let conc = self.conc.unwrap_or(1);
        let steady = self.progress.is_some_and(|(g, s)| crate::rolling::steady_reading(g, s, conc));
        // (agent 0.3.5: a GPU job's early reading - half its games in flight finished, some time after its start)
        let early = !steady && self.started.elapsed() >= eng.cfg.timings.early_calibration && self.progress.is_some_and(|(g, s)| crate::rolling::early_reading(g, s, conc));
        let (run, build) = (self.lease.run.clone().unwrap_or_default(), self.lease.build.build_id.clone());
        let Some(r) = self.roll.as_mut().filter(|r| r.active) else { return };
        let Some(now) = self.worker.commit_mb() else { return };
        r.peak_mb = r.peak_mb.max(now);
        let per = crate::rolling::measured_per_game(now, conc, r.mem.cost);
        let per_kb = crate::rolling::measured_per_game_kb(now, conc, r.mem.cost);
        r.per_game_max = r.per_game_max.max(per);
        if steady {
            r.steady_kb = r.steady_kb.max(per_kb);
        }
        // (an early reading at or under the base counts 1 MB a game: planned at 2, as 0.3.5-0.3.8)
        if early && r.gpu {
            r.early_kb = r.early_kb.max(per_kb.max(1024));
        }
        // (agent 0.3.3: more per game than planned is remembered at once; the plan of this lease changes at 92 %)
        if per > r.mem.cost.per_game_mb && per > r.raised {
            r.raised = per;
            if Learned::raise(&eng.cfg.layout, &run, &build, r.gpu, per) {
                log::info!("lease {}: the worker commits {per} MB per game in flight, more than planned: remembering it for this run and worker build", self.lease.lease_id);
            }
        }
        if !live || r.finishing || r.restart.is_some() || limit == 0 || (now as f64) < limit as f64 * crate::rolling::HIGH_SHARE {
            return;
        }
        self.primary_memory_high(eng, now);
    }

    /// rev 15 (PROTOCOL 10.6): the main worker committed `now` MB, near its limit (or, rev 19, the crew's sum is near the
    /// job's, 13.5): it finishes its games and is started again with fewer in flight; one that can't have fewer
    /// finishes the lease.
    fn primary_memory_high(&mut self, eng: &Engine, now: u64) {
        let limit = self.worker.mem_limit_mb;
        let Some(base) = self.roll.as_ref().map(|r| r.mem.cost) else { return };
        let id = self.lease.lease_id.clone();
        let conc = self.conc.unwrap_or(1);
        let per = crate::rolling::measured_per_game(now, conc, base);
        let cost = self.learn_cost(eng, per);
        let Some(r) = self.roll.as_mut() else { return };
        match crate::rolling::restart_games(conc, limit, cost).filter(|_| r.restarts < crate::rolling::MAX_RESTARTS) {
            Some(n) => {
                log::warn!("lease {id}: the worker commits {now} MB of its {limit} MB limit with {conc} games in flight; finishing them, then going on with {n}");
                r.restart = Some(Restart { conc: n, burn: false, layout: None, count: true, measure: false });
                self.send(json!({"op": "finish"}));
            }
            None => self.roll_finish(&format!("the worker commits {now} MB of its {limit} MB limit and can't run fewer games")),
        }
    }

    /// rev 15 (PROTOCOL 10.6): the worker failed (`why`: its fatal error or its exit) - if a rolling worker ran into
    /// the job's memory limit (Windows' peak at `HIT_SHARE` of it, or an allocation failure), a restart with fewer games
    /// in flight is due (its games in flight are lost), the cost per game remembered, and the failure reported once per
    /// lease. -> true when a restart is due.
    fn memory_failure(&mut self, eng: &Engine, why: &str) -> bool {
        let limit = self.worker.mem_limit_mb;
        let peak = self.worker.job_peak_mb().unwrap_or(0);
        // (agent 0.3.4: a GPU job's worker that ran out of video memory, not of the job's memory)
        if self.vram_oom_restart(eng, why, peak, limit) {
            return true;
        }
        let hit = (limit > 0 && peak as f64 >= limit as f64 * crate::rolling::HIT_SHARE) || alloc_failure(why) || alloc_failure(&self.worker.stderr_tail());
        let stopping = matches!(self.state, WState::Stopping(_)) || self.after.is_some() || eng.quitting();
        let conc = self.conc.unwrap_or(1);
        let Some((base, peak_mb)) = self.roll.as_ref().filter(|r| r.active && !r.finishing && !stopping && hit).map(|r| (r.mem.cost, r.peak_mb)) else { return false };
        let id = self.lease.lease_id.clone();
        let per = crate::rolling::measured_per_game(peak.max(peak_mb).max(limit), conc, base);
        let cost = self.learn_cost(eng, per);
        let Some(r) = self.roll.as_mut() else { return false };
        let Some(n) = crate::rolling::restart_games(conc, limit, cost).filter(|_| r.restarts < crate::rolling::MAX_RESTARTS) else {
            log::error!("lease {id}: the worker ran into its {limit} MB memory limit with {conc} games in flight ({why}); no restart left");
            return false;
        };
        log::warn!("lease {id}: the worker ran into its {limit} MB memory limit with {conc} games in flight ({why}); starting it again with {n}");
        r.restart = Some(Restart { conc: n, burn: true, layout: None, count: true, measure: false });
        let first = r.restarts == 0;
        if first {
            self.report(eng, "crash", &format!("rolling worker ran into its {limit} MB memory limit (peak {peak} MB) with {conc} games in flight: {why}; restarted with {n}"), None);
        }
        if self.worker.try_wait().is_none() {
            self.worker.kill();
        }
        self.killed = true;
        true
    }

    /// agent 0.3.4 (PROTOCOL 10.6): a GPU job's worker that ran out of video memory (`vram_failed`): the video memory per
    /// row planned higher (`learn_vram_oom`) and the worker started again with fewer games in flight - its games in
    /// flight lost, a restart (at most `MAX_RESTARTS`), reported once per lease; the job's memory per game is not touched.
    /// -> true when a restart is due.
    fn vram_oom_restart(&mut self, eng: &Engine, why: &str, peak: u64, limit: u64) -> bool {
        let stopping = matches!(self.state, WState::Stopping(_)) || self.after.is_some() || eng.quitting();
        let tail = self.worker.stderr_tail();
        if stopping || !self.roll.as_ref().is_some_and(|r| r.active && !r.finishing) || !self.vram_failed(why, &tail, peak, limit) {
            return false;
        }
        let id = self.lease.lease_id.clone();
        let conc = self.conc.unwrap_or(1);
        let rows = self.main_layout().batch();
        self.learn_vram_oom(eng, rows, self.vram_mb);
        let Some(r) = self.roll.as_mut() else { return false };
        let Some(n) = crate::rolling::restart_games(conc, u64::MAX >> 24, r.mem.cost).filter(|_| r.restarts < crate::rolling::MAX_RESTARTS) else {
            log::error!("lease {id}: the worker ran out of video memory with {conc} games in flight ({why}); no restart left");
            return false;
        };
        log::warn!("lease {id}: the worker ran out of video memory with {conc} games in flight, batches of {rows} rows ({why}); starting it again with {n}");
        r.restart = Some(Restart { conc: n, burn: true, layout: None, count: true, measure: false });
        if r.restarts == 0 {
            self.report(eng, "crash", &format!("rolling worker ran out of video memory with {conc} games in flight (batches of {rows} rows): {why}; restarted with {n}"), None);
        }
        if self.worker.try_wait().is_none() {
            self.worker.kill();
        }
        self.killed = true;
        true
    }

    /// rev 15 (PROTOCOL 10.6): the restart due (`Roll::restart`) once the worker is gone: a new worker with fewer games
    /// in flight, the lease's latest network, its own results directory and the ids after the ones its predecessor
    /// started; extensions go on as before. -> false when there is none (or it can't start): the lease ends.
    fn restart_worker(&mut self, eng: &mut Engine) -> bool {
        let lo_released = self.roll.as_ref().map(|r| (r.setup.lo, r.released));
        let stopping = self.after.is_some() || eng.quitting();
        let Some(r) = self.roll.as_mut() else { return false };
        let Some(rs) = r.restart.take() else { return false };
        // (a worker that was finishing its games for a restart and died without its `end`, of anything but memory - that
        // would have made the restart a `burn` one -, fails as any worker)
        if r.finishing || stopping || (!rs.burn && self.end_ev.is_none()) {
            return false;
        }
        let (lo, released) = lo_released.expect("rolling");
        // the ids the new worker plays: after every released id when its predecessor died; else the released ids it
        // never started - its `end`'s unfinished tail, the range ending at the released ids' end (agent 0.3.13: also when
        // other ranges come before it - a worker that ended its games in flight, `quit`, leaves their ids scattered: never
        // credited, they are given up, its unstarted tail isn't; 0.3.6-0.3.12 took fresh ids after the released ones then,
        // giving the tail up too)
        let tail = self
            .end_ev
            .as_ref()
            .and_then(|e| e.get("unfinished"))
            .and_then(|u| u.get("gids"))
            .map(crate::util::parse_ranges)
            .and_then(|g| g.iter().copied().max_by_key(|&(_, b)| b))
            .filter(|&(a, b)| b == lo + released && a >= lo + r.offset)
            .map(|(a, _)| a - lo);
        r.offset = if rs.burn { released } else { tail.unwrap_or(released) };
        if rs.count {
            r.restarts += 1;
        }
        r.want_more = false;
        r.refused = false;
        r.next_extend = Instant::now();
        r.peak_mb = 0;
        let gpu = r.gpu;
        let batch = r.gpu_batch;
        let restarts = r.restarts;
        let offset = r.offset;
        match rs.layout {
            // rev 19 (PROTOCOL 13): a handoff's next layout
            Some(l) => {
                self.conc = Some(l.conc);
                self.groups = Some(l.groups);
                self.threads = l.threads;
                self.infer = self.infer_for(&l);
            }
            None => {
                self.conc = Some(rs.conc);
                self.groups = Some(if gpu { crate::rolling::gpu_groups(rs.conc, batch) } else { self.groups.unwrap_or(1).clamp(1, rs.conc) });
            }
        }
        self.spawns += 1;
        // (a memory restart's results directory is named by its restart, a handoff's by its worker)
        self.out_dir = if rs.count { format!("out-r{restarts}") } else { format!("out-w{}", self.spawns) };
        let timings = eng.cfg.timings.clone();
        let (conc, groups, threads, infer) = (self.conc.unwrap_or(1), self.groups.unwrap_or(1), self.threads, self.infer);
        let mut refit = None;
        if let Some(t) = self.tuning.as_mut() {
            // (a GPU job's inference threads: the worker's own, its search threads)
            let (w, it) = infer.unwrap_or((1, threads));
            let workers = rs.layout.map_or(t.current.workers, |l| l.workers);
            t.current = Layout { workers, threads, infer_workers: w, infer_threads: it, groups, conc };
            if rs.layout.is_none() {
                // (a memory restart: the layouts of this lease keep within the games in flight that fit - else the
                // incumbent would be handed back to at once, PROTOCOL 13.5 - and its batches within bounds, the groups
                // kept where the fewer games allow: what the crew's next members run, and the incumbent at this size)
                t.space.conc_max = t.space.conc_max.min(t.current.total_conc());
                t.space.conc_min = t.space.conc_min.min(t.space.conc_max);
                t.current = t.space.fit(t.current);
                refit = Some(t.current);
            }
            t.handoff = None;
            t.probe = None;
            t.probe_load = None;
            t.probe_members.clear();
            t.postponed = None;
            if rs.measure {
                t.start_probe(t.current.total_conc(), &timings);
            }
            if rs.layout.is_some() {
                t.revert_due = false;
            }
        }
        if let Some(l) = refit {
            self.conc = Some(l.conc);
            self.groups = Some(l.groups);
            self.threads = l.threads;
            self.infer = self.infer_for(&l);
        }
        let limit = self.worker_limit();
        self.wgen = self.next_gen();
        self.end_ev = None;
        self.killed = false;
        self.eof = false;
        self.job_sent = false;
        self.pinged = false;
        self.state = WState::Starting;
        self.started = Instant::now();
        self.last_msg = Instant::now();
        self.stall_since = None;
        let id = self.lease.lease_id.clone();
        if let Err(e) = std::fs::create_dir_all(self.work.join(&self.out_dir)) {
            log::error!("lease {id}: cannot start the worker again: {e}");
            self.after = Some(After::Kill(format!("restart: {e}")));
            return false;
        }
        match self.spawn.spawn(self.wgen, self.tx.clone(), limit) {
            Ok(w) => {
                self.counters = None;
                self.progress = None;
                (self.finish_sent, self.tail_quit) = (None, false);
        (self.finish_sent, self.tail_quit) = (None, false);
                self.crew_changed = Instant::now();
                match rs.layout {
                    Some(l) => log::info!("lease {id}: worker pid {} started with layout {} (ids from {}, memory limit {limit} MB)", w.pid, l.per_worker().describe_on(gpu), lo + offset),
                    None => log::info!("lease {id}: worker pid {} started again ({} games in flight, ids from {}, restart {restarts})", w.pid, self.conc.unwrap_or(rs.conc), lo + offset),
                }
                self.worker = w;
                self.mark = CpuMark::take(&self.worker);
                self.sample = (self.mark, Instant::now());
                self.show_tuning(eng);
                self.beat_soon();
                true
            }
            Err(e) => {
                log::error!("lease {id}: cannot start the worker again: {e}");
                self.after = Some(After::Kill(format!("restart: {e}")));
                false
            }
        }
    }

    /// rev 15 (PROTOCOL 10.4, 10.6), each tick of a rolling lease: more ids when the worker is low (at most once per
    /// `extend_every`, not before a `wait_s` or a backoff), the update check every `update_check` (a rolling lease
    /// would otherwise never let the agent update), and `finish` once a newer agent is ready.
    fn roll_tick(&mut self, eng: &mut Engine) {
        let stopping = matches!(self.state, WState::Stopping(_)) || self.after.is_some();
        if stopping || !self.roll.as_ref().is_some_and(|r| r.active && !r.finishing) {
            return;
        }
        if let Some(v) = eng.pending_update.clone() {
            self.roll_finish(&format!("agent {v} is ready to switch to"));
            return;
        }
        // about 2 x the games in flight (PROTOCOL 10.4; rev 19, 13.5: the whole crew's)
        let ask = 2 * self.crew_conc().max(1);
        let Some(r) = self.roll.as_mut() else { return };
        let now = Instant::now();
        if r.want_more && !r.extend_inflight && now >= r.next_extend {
            r.extend_inflight = true;
            r.next_extend = now + eng.cfg.timings.extend_every;
            let body = ExtendReq { games: ask };
            log::info!("lease {}: the worker is low on game ids ({} released); asking for {} more", self.lease.lease_id, r.released, body.games);
            let (client, tx, id) = (eng.client.clone(), self.tx.clone(), self.lease.lease_id.clone());
            std::thread::spawn(move || {
                let r = client.post_json::<_, ExtendResp>(&format!("/v1/lease/{id}/extend"), &body);
                let _ = tx.send(Ev::Extend(r));
            });
        }
        let due = eng.last_update_check.is_none_or(|t| t.elapsed() >= eng.cfg.timings.update_check);
        if due && !r.update_inflight {
            r.update_inflight = true;
            eng.last_update_check = Some(now);
            let mut keep = Engine::keep_list(&self.lease);
            if let (Some(run), Some((c, _))) = (&self.lease.run, &self.prep.ckpt) {
                keep.push(Cache::ckpt_key(run, c.gen));
            }
            let check = crate::update::InLease {
                client: eng.client.clone(),
                keys: eng.cfg.keys.clone(),
                layout: eng.cfg.layout.clone(),
                cache: Cache::new(eng.cfg.layout.clone(), eng.cfg.keys.clone()),
                cap: eng.settings().disk_cap_mb << 20,
                keep,
                control: eng.control.clone(),
                channel: eng.cfg.channel.clone(),
                running: eng.cfg.version.clone(),
            };
            let tx = self.tx.clone();
            std::thread::spawn(move || {
                let _ = tx.send(Ev::Update(check.run()));
            });
        }
    }

    /// rev 15 (PROTOCOL 10.6): ends a rolling lease gracefully: the worker starts no new game, finishes the ones in
    /// flight, flushes and ends with `finish`; `done` then closes the lease (not given back). Once.
    fn roll_finish(&mut self, why: &str) {
        let Some(r) = self.roll.as_mut() else { return };
        if r.finishing {
            return;
        }
        r.finishing = true;
        r.want_more = false;
        // (a restart for memory gives way: the lease ends with this worker; so does a layout handoff)
        r.restart = None;
        if let Some(t) = self.tuning.as_mut() {
            t.handoff = None;
            t.probe = None;
        }
        log::info!("lease {}: finishing the games in flight ({why})", self.lease.lease_id);
        if !matches!(self.state, WState::Stopping(_)) {
            self.send(json!({"op": "finish"}));
        }
        // rev 19 (PROTOCOL 13.5): every member of the crew too (the ids they never started go back)
        for m in self.mates.iter_mut().filter(|m| m.active()) {
            m.finishing = true;
            if m.job_sent {
                let _ = m.worker.send(&json!({"op": "finish"}));
            } else {
                // (no job yet: nothing to finish)
                let _ = m.worker.send(&json!({"op": "quit"}));
            }
        }
    }

    /// rev 15 (PROTOCOL 10.4): the answer to an extension.
    fn on_extend(&mut self, eng: &mut Engine, res: Result<ExtendResp, CallError>) {
        let id = self.lease.lease_id.clone();
        let Some(r) = self.roll.as_mut() else { return };
        r.extend_inflight = false;
        let a = match res {
            Ok(a) => a,
            Err(e) if matches!(e.status(), Some(404 | 410)) => {
                log::info!("lease {id}: the coordinator no longer has it ({e})");
                self.stop(After::Expire);
                return;
            }
            Err(e) if e.is_revoked() => {
                self.after = Some(After::Revoked);
                self.kill("machine revoked");
                return;
            }
            Err(e) if e.is_retryable() => {
                let d = r.backoff.delay_for(&e);
                log::warn!("lease {id}: asking for more game ids failed ({e}); again in {d:?}");
                r.next_extend = Instant::now() + d;
                r.refused = true;
                return;
            }
            Err(e) => {
                self.roll_finish(&format!("the coordinator refused more game ids: {e}"));
                return;
            }
        };
        r.backoff.reset();
        if r.finishing {
            return;
        }
        let block = r.setup.hi - r.setup.lo;
        if a.games > block {
            log::error!("lease {id}: the coordinator released {} ids, more than the lease's block of {block}", a.games);
            self.roll_finish("an extension past the lease's block");
            return;
        }
        if a.games > r.released {
            log::info!("lease {id}: {} more game ids released ({} in all)", a.games - r.released, a.games);
            r.released = a.games;
            r.want_more = false;
            r.refused = false;
            let total = a.games;
            self.lease.job["games"] = json!(total);
            self.plan.job.insert("games".into(), json!(total));
            // what an agent that dies now closes the lease with (its released ids)
            if let Err(e) = eng.results.begin(&self.lease) {
                log::warn!("lease {id}: could not save the lease: {e}");
            }
            // (the worker's games count from its own first id; a worker without its job yet gets them in the job line)
            let offset = self.roll.as_ref().map_or(0, |r| r.offset);
            if self.job_sent {
                self.send(json!({"op": "extend", "games": total - offset}));
            }
        } else if a.games < r.released {
            log::warn!("lease {id}: the coordinator's extension says {} ids, fewer than the {} released; ignored", a.games, r.released);
        }
        let r = self.roll.as_mut().expect("rolling");
        if a.finish {
            let why = a.reason.as_deref().unwrap_or("finish").to_string();
            self.roll_finish(&format!("the coordinator releases no more ids: {why}"));
        } else if let Some(w) = a.wait_s {
            log::info!("lease {id}: no game ids now ({}); asking again in {w} s", a.reason.as_deref().unwrap_or("wait"));
            r.next_extend = r.next_extend.max(Instant::now() + Duration::from_secs(w.min(3600)));
            r.refused = true;
        }
    }

    /// agent 0.3.15: the main worker's progress watch (a new main worker - a restart, a handoff - starts a new one).
    fn main_watch(&mut self) -> &mut Watch {
        if self.watch.pid != self.worker.pid {
            self.watch = Watch::new(self.worker.pid);
        }
        &mut self.watch
    }

    /// agent 0.3.15 (PROTOCOL 10.6 **A worker that stops playing**): a running or resuming main worker that failed its
    /// resume or plays nothing (`Watch::verdict`) failed - as with a fatal error (`worker_failed`): a rolling worker out of
    /// video memory (a session it couldn't build again) starts again with smaller batches, a crew's main worker - and a
    /// lone rolling worker - is replaced, else the failure is reported and the lease fails.
    fn check_progress(&mut self, eng: &mut Engine, t: &crate::Timings) {
        if self.killed || self.after.is_some() || eng.quitting() || matches!(self.state, WState::Stopping(_)) {
            return;
        }
        let resuming = matches!(self.state, WState::Resuming(_));
        let starved = self.stall_since.is_some();
        let w = self.main_watch();
        if starved {
            // (a worker the PC gives no CPU plays nothing; the slack rule bounds that)
            w.restart_clock();
        }
        let Some(why) = w.verdict(Instant::now(), t.resume_fail_after, t.stall_after, resuming, starved) else { return };
        log::warn!("lease {}: the worker failed: {why}", self.id());
        eng.worker_log.write_line(&format!("watch [{}]: the worker failed: {why}", self.lease.lease_id));
        self.worker_failed(eng, &why, &format!("worker failed: {why}"), true, "it plays nothing");
    }

    /// The main worker failed - its fatal error (`msg`), or (agent 0.3.15) a failed resume or no progress (`check_progress`)
    /// -, one way for all: a rolling worker out of memory or video memory starts again with fewer games in flight
    /// (`memory_failure`, rev 15 / agent 0.3.4); a crew's main worker is replaced, the others going on (`crew_crash`, rev
    /// 19; `lone`: a lone rolling worker too - its build works, the worker lost its sessions); else the failure (`why`) is
    /// reported and the worker killed: the lease fails (`kill_why`).
    fn worker_failed(&mut self, eng: &mut Engine, msg: &str, why: &str, lone: bool, kill_why: &str) {
        if self.memory_failure(eng, msg) {
            return;
        }
        if self.crew_crash(eng, why, None, lone) {
            self.worker.kill();
            self.killed = true;
            return;
        }
        self.report(eng, "crash", why, None);
        self.kill(kill_why);
    }

    fn check_alive(&mut self, eng: &Engine, t: &crate::Timings) {
        let quiet = self.last_msg.elapsed();
        if quiet > t.hang_after {
            if self.starved_extend(eng, self.last_msg, t) {
                self.last_msg = Instant::now();
                return;
            }
            self.report(eng, "crash", &format!("worker silent for {} s", quiet.as_secs()), None);
            self.kill("hung");
        } else if quiet > t.ping_after && !self.pinged {
            self.pinged = true;
            self.send(json!({"op": "ping"}));
        }
    }

    fn report(&mut self, eng: &Engine, kind: &str, message: &str, exit_code: Option<i64>) {
        self.crash_reported = true;
        eng.report_crash(CrashReq {
            build_id: Some(self.lease.build.build_id.clone()),
            kind: kind.into(),
            message: format!("{message}{}", self.selftest_note()),
            backtrace: Some(self.worker.stderr_tail()).filter(|s| !s.is_empty()),
            lease_id: Some(self.lease.lease_id.clone()),
            exit_code,
            agent: None,
            os_error: None,
        });
    }

    fn on_event(&mut self, eng: &mut Engine, ev: Ev) {
        match ev {
            // rev 19 (PROTOCOL 13): a worker finishing its games after a layout handoff
            Ev::Worker(g, m) if g != self.wgen && self.mates.iter().any(|d| d.wgen == g) => self.on_mate(eng, g, m),
            // (an earlier worker's lines, after a restart: dropped)
            Ev::Worker(g, _) if g != self.wgen => {}
            Ev::Worker(_, WorkerMsg::Line(v)) => self.on_worker(eng, v),
            Ev::Worker(_, WorkerMsg::BadLine(s)) => log::warn!("lease {}: worker wrote a non-JSON line: {s}", self.id()),
            Ev::Worker(_, WorkerMsg::Eof) => self.eof = true,
            Ev::Beat(r) => self.on_beat(eng, r),
            Ev::Ckpt(r) => {
                self.ckpt_inflight = false;
                let rolling = self.roll.as_ref().is_some_and(|r| r.active);
                if self.roll.as_ref().is_some_and(|r| r.finishing) {
                    return;
                }
                match r {
                    Ok((c, f)) => {
                        log::info!("lease {}: switching to checkpoint {}", self.id(), c.gen);
                        let mut op = json!({"op": "checkpoint", "gen": c.gen, "model": f.model, "reference": f.reference});
                        if rolling {
                            // rev 15 (PROTOCOL 10.6): with the model's sha256 (the worker self-tests it before switching)
                            op["sha256"] = json!(c.sha256);
                        }
                        // (a restarted worker without its job yet gets the network in its job line)
                        if self.job_sent || !rolling {
                            self.send(op.clone());
                        }
                        // rev 19 (PROTOCOL 13.5): a crew switches together (a member without its job yet gets it in its
                        // job line; one finishing its games keeps the network it has)
                        for m in self.mates.iter_mut().filter(|m| m.active() && m.job_sent) {
                            if let Err(e) = m.worker.send(&op) {
                                log::warn!("lease {}: write to worker pid {} failed: {e}", self.lease.lease_id, m.worker.pid);
                            }
                        }
                        self.prep.ckpt = Some((c, f));
                    }
                    Err((c, e)) => {
                        log::warn!("lease {}: checkpoint {} not usable: {e}", self.id(), c.gen);
                        self.ckpt_failed.push((c.gen, c.sha256));
                        // a rolling lease would otherwise play an old network for good: the next lease starts afresh
                        if rolling && !eng.quitting() {
                            self.roll_finish(&format!("checkpoint {} could not be fetched", c.gen));
                        }
                    }
                }
            }
            Ev::Extend(r) => self.on_extend(eng, r),
            Ev::Def(r) => self.on_definition(eng, *r),
            Ev::Update(r) => {
                if let Some(x) = self.roll.as_mut() {
                    x.update_inflight = false;
                }
                match r {
                    Ok(crate::update::News { ready, evict, vendors_off }) => {
                        // the coordinator's evict list, applied once the lease ends (always between leases)
                        for b in evict.iter().take(64).filter(|b| zerofarm_common::is_build_id(b)) {
                            let key = format!("builds/{b}");
                            if eng.cfg.layout.cache_builds().join(b).exists() && !eng.evict_after.contains(&key) {
                                eng.evict_after.push(key);
                            }
                        }
                        // rev 22 (PROTOCOL 9.8): the owner's GPU vendors off, at once (a GPU job on such an adapter is
                        // given back at the next tick)
                        if let Some(off) = vendors_off {
                            eng.apply_vendors_off(&off);
                        }
                        if let Some(v) = ready {
                            log::info!("lease {}: agent {v} is ready; finishing this rolling lease to switch to it", self.id());
                            eng.pending_update = Some(v);
                        }
                    }
                    Err(e) => log::warn!("update check: {e}"),
                }
            }
        }
    }

    /// rev 15 (PROTOCOL 10.6): a rolling lease's new definition, checked off this thread.
    fn on_definition(&mut self, eng: &mut Engine, r: Result<(DefinitionRef, Definition, CheckpointRef), Recheck>) {
        let id = self.lease.lease_id.clone();
        let Some(roll) = self.roll.as_mut() else { return };
        roll.def_inflight = false;
        if roll.finishing {
            return;
        }
        match r {
            Ok((dref, d, c)) => {
                log::info!("lease {id}: definition {} version {} verified (the same template); fetching checkpoint {}", d.scope(), d.version, c.gen);
                roll.def = dref;
                self.plan.def = Some(d);
                self.fetch_checkpoint(eng, c);
            }
            Err(Recheck::TemplateChanged(why)) => {
                log::info!("lease {id}: {why}");
                self.roll_finish("the run's self-play template changed; the next lease brings it");
            }
            Err(Recheck::Refused(why)) => {
                log::error!("lease {id}: the run's new definition failed a check: {why}");
                if !roll.def_reported {
                    roll.def_reported = true;
                    eng.report_crash(CrashReq {
                        build_id: Some(self.lease.build.build_id.clone()),
                        kind: "definition".into(),
                        message: format!("rolling lease's new definition refused: {why}"),
                        lease_id: Some(id.clone()),
                        ..Default::default()
                    });
                }
                self.roll_finish("the run's new definition failed a check");
            }
            Err(Recheck::Unavailable(why)) => log::warn!("lease {id}: the run's new definition can't be fetched right now ({why}); trying at a later beat"),
            Err(Recheck::Stopped) => {}
        }
    }

    /// The job line (PROTOCOL 5 and 7): the plan (template + per-lease fields) and the agent's
    /// own fields, which always overwrite: `op`, `lease_id`, `kind` (the lease's), the local
    /// network paths (`model`, `reference`, eval `models`), `threads`, `out_dir`, `runtime_dir`.
    /// No other coordinator-supplied key gets here.
    fn job_line(&self) -> Value {
        let ids = self.roll.as_ref().filter(|r| r.active).map(|r| (r.offset, r.released - r.offset));
        self.job_line_with(self.threads, self.conc, self.groups, self.infer, &self.out_dir, ids)
    }

    /// The job line of a worker with these tuning fields, results directory and (rolling) ids: `(offset, games)` -
    /// `gid_base` = the lease's first id + `offset` (rev 19, PROTOCOL 13.5: a crew member's range).
    fn job_line_with(&self, threads: u32, conc: Option<u64>, groups: Option<u64>, infer: Option<(u32, u32)>, out_dir: &str, ids: Option<(u64, u64)>) -> Value {
        let mut j = self.plan.job.clone();
        j.insert("op".into(), json!("job"));
        j.insert("lease_id".into(), json!(self.lease.lease_id));
        j.insert("kind".into(), json!(self.lease.kind));
        let (model, reference) = match &self.prep.ckpt {
            Some((_, f)) => (json!(f.model), json!(f.reference)),
            None => (Value::Null, Value::Null),
        };
        j.insert("model".into(), model);
        j.insert("reference".into(), reference);
        // Eval (rev 5, PROTOCOL 5.2): every network by gen; `model`/`reference` stay null.
        if self.lease.kind == "eval" {
            let mut models = Map::new();
            for (c, f) in &self.prep.models {
                models.insert(c.gen.to_string(), json!({"model": f.model, "sha256": c.sha256, "reference": f.reference}));
            }
            j.insert("models".into(), Value::Object(models));
        } else {
            j.remove("models");
        }
        j.insert("threads".into(), json!(threads));
        // rev 7 (PROTOCOL 8): the agent's own, never the coordinator's
        j.insert("providers".into(), json!(self.providers.0));
        j.insert("gpu".into(), self.providers.1.clone());
        // rev 11 (PROTOCOL 9.6): a GPU job runs at a high conc (the agent's tuning field, PROTOCOL 5)
        for (k, v) in [("conc", conc), ("groups", groups)] {
            match v {
                Some(x) => {
                    j.insert(k.into(), json!(x));
                }
                None => {
                    j.remove(k);
                }
            }
        }
        // rev 19 (PROTOCOL 13): a tuned layout's inference sessions and threads per session
        match infer {
            Some((w, t)) => {
                j.insert("infer_workers".into(), json!(w));
                j.insert("infer_threads".into(), json!(t));
            }
            None => {
                j.remove("infer_workers");
                j.remove("infer_threads");
            }
        }
        j.insert("out_dir".into(), json!(out_dir));
        j.insert("runtime_dir".into(), self.prep.runtime.as_ref().map(|r| json!(r.dir)).unwrap_or(Value::Null));
        // rev 15 (PROTOCOL 10.5): a worker with the feature keeps `conc` games in flight past `games` (the released
        // ids), asks for more with `low`, and flushes finished games every `flush_s`
        match (self.roll.as_ref().filter(|r| r.active), ids) {
            (Some(r), Some((offset, games))) => {
                j.insert("rolling".into(), json!(true));
                j.insert("flush_s".into(), json!(r.setup.flush_s));
                // (a restarted worker: the ids after the ones its predecessors started, PROTOCOL 10.6; a crew member: its
                // range, 13.5)
                j.insert("gid_base".into(), json!(r.setup.lo + offset));
                j.insert("games".into(), json!(games));
            }
            _ => {
                j.remove("rolling");
                j.remove("flush_s");
            }
        }
        Value::Object(j)
    }

    fn on_worker(&mut self, eng: &mut Engine, v: Value) {
        self.last_msg = Instant::now();
        self.pinged = false;
        if !matches!(self.state, WState::Yielding(_)) {
            self.mark = CpuMark::take(&self.worker);
        }
        let ev = v.get("ev").and_then(Value::as_str).unwrap_or("").to_string();
        match ev.as_str() {
            "hello" => {
                if self.state != WState::Starting {
                    return;
                }
                let aw = v.get("aw").and_then(Value::as_u64);
                let bid = v.get("build_id").and_then(Value::as_str).unwrap_or("");
                if aw != Some(zerofarm_common::AW_VERSION as u64) {
                    self.report(eng, "crash", &format!("worker speaks aw {aw:?}"), None);
                    self.kill("wrong aw");
                    return;
                }
                if bid != self.prep.build.build_id {
                    self.report(eng, "crash", &format!("worker says build {bid}, lease says {}", self.prep.build.build_id), None);
                    self.kill("wrong build id");
                    return;
                }
                // rev 15 (PROTOCOL 10.5): only a worker that lists `rolling` gets a rolling job and `extend`s
                if let Some(r) = self.roll.as_mut() {
                    let features = v.get("features").and_then(Value::as_array);
                    r.active = features.is_some_and(|f| f.iter().any(|x| x.as_str() == Some(crate::rolling::WORKER_FEATURE)));
                    if !r.active {
                        log::info!("lease {}: worker build {bid} can't play rolling jobs; running the lease as a classic one ({} games)", self.lease.lease_id, r.released);
                    }
                }
                // rev 19 (PROTOCOL 13): a worker that lists `infer_workers` honours the job line's sessions (macOS builds
                // always do)
                // (agent 0.3.4: a GPU job's inference threads are the worker's own: no sessions)
                if let Some(t) = self.tuning.as_mut().filter(|t| !t.gpu) {
                    let listed = v.get("features").and_then(Value::as_array).is_some_and(|f| f.iter().any(|x| x.as_str() == Some(tune::WORKER_FEATURE)));
                    t.space.sessions = t.defaults.sessions_always || listed;
                    let fitted = t.space.fit(t.current);
                    if fitted != t.current {
                        // (one session: the layout's inference threads in it)
                        t.current = fitted;
                        self.threads = fitted.threads;
                        self.infer = Some((fitted.infer_workers, fitted.infer_threads));
                    }
                }
                let line = self.job_line();
                self.send(line);
                self.job_sent = true;
                self.hello_seen = true;
                eng.start_ok();
                self.state = WState::Running;
                let started = eng.cfg.layout.cache().join("started");
                let _ = std::fs::create_dir_all(&started);
                let _ = std::fs::write(started.join(&self.lease.build.build_id), b"");
            }
            "selftest" => {
                log::info!("lease {}: selftest {v}", self.id());
                // rev 5: one per network (`gen`); a single-model worker may leave `gen` out.
                let gen = v.get("gen").and_then(Value::as_u64).or_else(|| self.prep.ckpt.as_ref().map(|(c, _)| c.gen));
                let ok = v.get("ok").and_then(Value::as_bool).unwrap_or(false);
                let mut t = Map::new();
                for k in ["provider", "max_abs", "ok"] {
                    t.insert(k.into(), v.get(k).cloned().unwrap_or(Value::Null));
                }
                self.selftests.insert(gen.map(|g| g.to_string()).unwrap_or_else(|| "model".into()), Value::Object(t));
                if !ok {
                    log::error!("lease {}: network {gen:?} failed its self-test: {v}", self.id());
                    self.selftest_failed = true;
                }
                self.note_provider(v.get("provider"));
            }
            "status" => {
                let s = match v.get("stats") {
                    Some(Value::Object(m)) => m.clone(),
                    _ => {
                        let mut m = v.as_object().cloned().unwrap_or_default();
                        m.remove("ev");
                        m
                    }
                };
                self.note_provider(s.get("provider"));
                if let (Some(g), Some(st)) = (s.get("games").and_then(Value::as_u64), s.get("games_started").and_then(Value::as_u64)) {
                    self.progress = Some((g, st));
                }
                // (agent 0.3.15: its progress watched)
                let n = |k: &str| s.get(k).and_then(Value::as_u64);
                let (leaves, games, started) = (n("leaves"), n("games"), n("games_started"));
                self.main_watch().status(leaves, games, started);
                // (agent 0.3.4: a worker that reports its video memory)
                self.stats_vram = s.get("vram_mb").and_then(Value::as_u64).unwrap_or(0);
                // (agent 0.3.6: a GPU job's worker's base reading - sessions built, games started, none finished)
                let named = s.get("provider").and_then(Value::as_str).is_some_and(|p| !p.is_empty());
                if self.gpu_lease() && self.progress.is_some_and(|(g, st)| crate::rolling::base_reading(named, g, st)) {
                    if let (Some(c), Some(r)) = (self.worker.commit_mb(), self.roll.as_mut()) {
                        r.base_min = if r.base_min == 0 { c } else { r.base_min.min(c) };
                    }
                }
                self.tune_status(eng, &s);
                self.stats = s;
            }
            "fin" => {
                let n = v.get("n").and_then(Value::as_u64).unwrap_or(0);
                self.games_done += n;
                eng.add_games(n);
                let mut m = eng.machine.lock().unwrap();
                if !m.first_work_toast_shown {
                    m.first_work_toast_shown = true;
                    let _ = m.save(&eng.cfg.layout);
                    crate::status::set(&eng.status, |s| {
                        s.toast = Some("Zero Farm is now using your idle PC. It stops as soon as you need it.".into())
                    });
                }
            }
            "shard" | "records" | "evals" => {
                let dir = self.out_dir.clone();
                let floor = self.gen_floor;
                self.gen_floor = self.on_result_file(eng, &ev, &v, &dir, floor);
            }
            // rev 15 (PROTOCOL 10.5): fewer than `conc` of the released ids are unstarted
            "low" => {
                let handing_off = self.tuning.as_ref().is_some_and(|t| t.handoff.is_some());
                if let Some(r) = self.roll.as_mut().filter(|r| r.active && !r.finishing && !handing_off) {
                    r.want_more = true;
                }
            }
            "failure" => {
                let mut f = Map::new();
                for k in ["seed", "panic", "site", "repro", "output_tail"] {
                    f.insert(k.into(), v.get(k).cloned().unwrap_or(Value::Null));
                }
                // rev 2: the coordinator needs an integer seed and a string site.
                if !f["site"].is_string() {
                    f.insert("site".into(), json!(panic_site(v.get("panic").and_then(Value::as_str).unwrap_or(""))));
                }
                if !f["seed"].is_u64() {
                    log::error!("lease {}: worker failure without an integer seed: {v}", self.id());
                    return;
                }
                let f = Value::Object(f);
                self.failures.push(f.clone());
                let body = self.fuzz_json(vec![f], None, false);
                self.store_bytes(eng, "fuzz", 0, &body, false, None);
            }
            "yielded" => {
                if let WState::Yielding(since) = self.state {
                    let ms = since.elapsed().as_millis() as u64;
                    self.yield_ms_max = self.yield_ms_max.max(ms);
                    if self.worker.suspend() {
                        log::info!("lease {}: worker yielded in {ms} ms; suspended", self.id());
                    } else {
                        log::warn!("lease {}: could not suspend the worker", self.id());
                    }
                    self.state = WState::Paused(Instant::now());
                }
            }
            "resumed" => {
                if let WState::Resuming(since) = self.state {
                    self.resume_ms_max = self.resume_ms_max.max(since.elapsed().as_millis() as u64);
                    self.state = WState::Running;
                }
                self.main_watch().resume_error = None;
            }
            "end" => {
                log::info!("lease {}: worker ended ({})", self.id(), v.get("reason").and_then(Value::as_str).unwrap_or("?"));
                if let Some(Value::Object(s)) = v.get("stats") {
                    self.stats = s.clone();
                }
                self.end_ev = Some(v);
            }
            "error" => {
                let msg = v.get("message").and_then(Value::as_str).unwrap_or("").to_string();
                if v.get("fatal").and_then(Value::as_bool).unwrap_or(false) {
                    self.worker_failed(eng, &msg, &format!("worker fatal error: {msg}"), false, "fatal error");
                } else {
                    // (agent 0.3.15: one while resuming is taken as a failed resume once nothing is played after it,
                    // `Watch`)
                    let resuming = matches!(self.state, WState::Resuming(_));
                    log::warn!("lease {}: worker error{}: {msg}", self.id(), if resuming { " while resuming" } else { "" });
                    self.main_watch().error(&msg, resuming);
                }
            }
            other => log::debug!("lease {}: ignoring worker event {other:?}", self.id()),
        }
    }

    /// A result file a worker announced (from `out_dir`, `floor`: that worker's `x-gen` floor) -> its floor from now on.
    fn on_result_file(&mut self, eng: &mut Engine, kind: &str, v: &Value, out_dir: &str, floor: Option<u64>) -> Option<u64> {
        let Some(path) = v.get("path").and_then(Value::as_str) else {
            log::warn!("lease {}: {kind} without a path", self.id());
            return floor;
        };
        let rel = match check_rel_path(path) {
            Ok(r) => r,
            Err(_) => {
                log::error!("lease {}: worker announced an unsafe path {path:?}", self.id());
                return floor;
            }
        };
        let src = self.work.join(out_dir).join(rel);
        if !src.is_file() {
            log::error!("lease {}: announced result {path} does not exist", self.id());
            return floor;
        }
        // x-gen (self-play only): the oldest generation in the chunk. Games that started before a
        // checkpoint switch finish on the old net, so until the next chunk the floor is the old gen.
        let selfplay = self.lease.kind == "selfplay";
        let gen = if selfplay { v.get("gen").and_then(Value::as_u64).or(floor) } else { None };
        let floor = self.prep.ckpt.as_ref().map(|(c, _)| c.gen).or(floor);
        let meta = ResultMeta {
            kind: kind.to_string(),
            games: v.get("games").and_then(Value::as_u64).unwrap_or(0),
            ids: v.get("gids").cloned().filter(|g| g.is_array()),
            gen,
            ..Default::default()
        };
        self.store(eng, meta, Source::File(&src));
        floor
    }

    fn store(&mut self, eng: &Engine, meta: ResultMeta, src: Source) {
        match eng.results.add(self.id(), meta, src) {
            Ok(m) => {
                log::debug!("lease {}: result {} queued ({} bytes)", self.id(), m.seq, m.size);
                eng.uploader.wake();
            }
            Err(e) => log::error!("lease {}: could not store a result: {e}", self.id()),
        }
    }

    fn store_bytes(&mut self, eng: &Engine, kind: &str, games: u64, body: &Value, is_final: bool, ids: Option<Value>) {
        let bytes = serde_json::to_vec(body).expect("json");
        let meta = ResultMeta { kind: kind.into(), games, is_final, ids, ..Default::default() };
        self.store(eng, meta, Source::Bytes(&bytes));
    }

    /// PROTOCOL 5.1 fuzz result.
    fn fuzz_json(&self, failures: Vec<Value>, done: Option<u64>, is_final: bool) -> Value {
        let j = &self.plan.job;
        json!({
            "campaign": self.lease.campaign, "mode": j.get("mode"), "first": j.get("first"), "count": j.get("count"),
            "done": done, "failures": failures, "build_id": self.prep.build.build_id,
            "engine_commit": self.prep.build.manifest.engine_commit, "final": is_final,
        })
    }

    fn on_beat(&mut self, eng: &mut Engine, r: Result<BeatResp, CallError>) {
        self.beat_inflight = false;
        match r {
            Ok(resp) => match resp.action.as_str() {
                "continue" => {
                    // rev 2: a newer checkpoint is repeated in every beat; act on it once
                    // (self-play only: eval networks never change during a lease).
                    if let Some(c) = resp.checkpoint.filter(|_| self.lease.kind == "selfplay") {
                        let current = self.prep.ckpt.as_ref().map(|(c, _)| c.gen);
                        let failed = self.ckpt_failed.iter().any(|(g, h)| *g == c.gen && *h == c.sha256);
                        if current.is_none_or(|g| c.gen > g) && !self.ckpt_inflight && !failed {
                            match self.roll.as_ref().map(|r| (r.active, r.finishing || r.def_inflight, &r.def)) {
                                // rev 15 (PROTOCOL 10.6): a rolling lease's new generation comes with the run's current
                                // definition: verified (off this thread) before the checkpoint is fetched
                                Some((true, false, cur)) if resp.definition.as_ref().is_some_and(|d| d != cur) => {
                                    self.recheck_definition(eng, resp.definition.clone().expect("checked"), c)
                                }
                                // finishing, or a definition being checked: repeats are ignored
                                Some((true, true, _)) => {}
                                // a rolling lease before its worker said hello: nothing is switched yet
                                Some((false, _, _)) if !self.hello_seen => {}
                                // classic, an old worker, or a gen the lease's own definition signs
                                _ => self.fetch_checkpoint(eng, c),
                            }
                        }
                    }
                    if let Some(what) = resp.upload_diag {
                        crate::logging::upload_diag(&eng.client, &eng.cfg.layout, &what);
                    }
                }
                "cancel" => {
                    log::info!("lease {}: cancelled by the coordinator", self.id());
                    self.after = Some(After::Cancel);
                    self.kill("cancelled");
                }
                "give_back" => {
                    log::info!("lease {}: coordinator asks to give it back", self.id());
                    self.stop(After::GiveBack("coordinator asked".into()));
                }
                other => log::warn!("lease {}: unknown beat action {other:?}", self.id()),
            },
            Err(e) if e.status() == Some(410) => {
                log::info!("lease {}: expired at the coordinator", self.id());
                self.stop(After::Expire);
            }
            Err(e) if e.is_revoked() => {
                self.after = Some(After::Revoked);
                self.kill("machine revoked");
            }
            Err(e) => log::warn!("lease {}: beat failed: {e}", self.id()),
        }
    }

    /// rev 15 (PROTOCOL 10.6): verifies a rolling lease's new definition on another thread (`Ev::Def`).
    fn recheck_definition(&mut self, eng: &Engine, dref: DefinitionRef, c: CheckpointRef) {
        let Some(current) = self.plan.def.clone() else { return };
        let Some(r) = self.roll.as_mut() else { return };
        r.def_inflight = true;
        log::info!("lease {}: generation {} comes with definition {}; checking it", self.lease.lease_id, c.gen, dref.key);
        let (fetch, lease, tx) = (eng.fetch(), self.lease.clone(), self.tx.clone());
        std::thread::spawn(move || {
            let r = fetch.recheck(&lease, &current, &dref, &c).map(|d| (dref, d, c));
            let _ = tx.send(Ev::Def(Box::new(r)));
        });
    }

    fn fetch_checkpoint(&mut self, eng: &Engine, c: CheckpointRef) {
        let Some(run) = self.lease.run.clone() else { return };
        // With a definition, only a network it names, with its hashes and sizes (PROTOCOL 7).
        let declared = match self.plan.check_new_checkpoint(&c) {
            Ok(d) => d,
            Err(e) => {
                log::warn!("lease {}: checkpoint {} not usable: {e}", self.id(), c.gen);
                self.ckpt_failed.push((c.gen, c.sha256));
                return;
            }
        };
        self.ckpt_inflight = true;
        let (client, tx) = (eng.client.clone(), self.tx.clone());
        let cache = Cache::new(eng.cfg.layout.clone(), eng.cfg.keys.clone());
        let cap = eng.settings().disk_cap_mb << 20;
        // Everything the running worker uses stays (security review L2), plus the new network.
        let mut keep = Engine::keep_list(&self.lease);
        keep.push(Cache::ckpt_key(&run, c.gen));
        // the network the worker plays now (a rolling lease's may be newer than the lease's first)
        if let Some((cur, _)) = &self.prep.ckpt {
            keep.push(Cache::ckpt_key(&run, cur.gen));
        }
        let control = eng.control.clone();
        std::thread::spawn(move || {
            let stop = move || control.quitting();
            let r = match cache.ensure_checkpoint(&client, &run, &c, declared.as_ref(), cap, &keep, &stop) {
                Ok(f) => Ok((c, f)),
                Err(e) => Err((c, e.to_string())),
            };
            let _ = tx.send(Ev::Ckpt(r));
        });
    }

    fn finish(&mut self, eng: &mut Engine, code: i32) -> LeaseEnd {
        let id = self.lease.lease_id.clone();
        if let WState::Paused(t) = self.state {
            self.paused_ms += t.elapsed().as_millis() as u64;
        }
        match &self.after {
            Some(After::Cancel) => return LeaseEnd::Cancelled,
            Some(After::Revoked) => return LeaseEnd::Revoked,
            _ => {}
        }
        let end = self.end_ev.clone();
        let reason = end.as_ref().and_then(|e| e.get("reason")).and_then(Value::as_str).unwrap_or("").to_string();
        if end.is_none() && !self.killed {
            let tail = self.worker.stderr_tail();
            let kind = if tail.contains("panicked at") { "panic" } else { "crash" };
            self.report(eng, kind, &format!("worker exited with code {code} without `end`"), Some(code as i64));
            self.after.get_or_insert(After::Kill(format!("worker exited with code {code}")));
        }
        // A network that failed its self-test is always reported, with every gen's result.
        if self.selftest_failed && !self.crash_reported {
            self.report(eng, "crash", "a network failed the worker's self-test", None);
        }
        let fuzz = self.lease.kind == "fuzz";
        let mut unfinished = end.as_ref().map(|e| e.get("unfinished").cloned().unwrap_or(Value::Null));
        if fuzz && end.is_some() && self.job_sent {
            let first = self.plan.job.get("first").and_then(Value::as_u64).unwrap_or(0);
            let count = self.plan.job.get("count").and_then(Value::as_u64).unwrap_or(0);
            let left = unfinished.as_ref().and_then(|u| u.get("seeds")).map(crate::util::parse_ranges).unwrap_or_default();
            // x-ids: the seeds this lease covered = its range minus what the worker left.
            let covered = crate::util::subtract((first, first + count), &left);
            let done_n = crate::util::total(&covered);
            let body = self.fuzz_json(self.failures.clone(), Some(done_n), true);
            let ids = (!covered.is_empty()).then(|| crate::util::ranges_json(&covered));
            self.store_bytes(eng, "fuzz", done_n, &body, true, ids);
        }
        let metas = eng.results.metas(&id);
        // (after a restart the last worker's `unfinished` covers only its own ids: the lease's are worked out)
        let restarted = self.spawns > 0;
        if end.is_none() || !self.job_sent || restarted {
            unfinished = Some(compute_unfinished(&self.lease, &metas).unwrap_or(Value::Null));
        }
        // rev 15 (PROTOCOL 10.6): a rolling job's memory per game measured over the lease, remembered when more than
        // it was planned with
        // (rev 19, PROTOCOL 13.5: the most any worker of the crew committed per game)
        if let (Some(r), Some(conc)) = (self.roll.as_ref().filter(|r| r.active && (r.peak_mb > 0 || r.per_game_max > 0)), self.conc) {
            let per = crate::rolling::measured_per_game(r.peak_mb, conc, r.mem.cost).max(r.per_game_max);
            let (run, build) = (self.lease.run.as_deref().unwrap_or(""), self.lease.build.build_id.as_str());
            if per > r.mem.cost.per_game_mb && Learned::raise(&eng.cfg.layout, run, build, r.gpu, per) {
                log::info!("lease {id}: the worker committed up to {} MB with {conc} games in flight: remembering {per} MB per game for this run", r.peak_mb);
            }
            // agent 0.3.3: workers that ran to their steady state under the plan lower it (with a margin, never below
            // what any of them committed per game at it)
            if r.steady_kb > 0 {
                if let Some(v) = Learned::steady(&eng.cfg.layout, run, build, r.gpu, r.steady_kb) {
                    log::info!("lease {id}: its workers committed at most {:.1} MB per game at their steady state: remembering {v} MB for this run and worker build", r.steady_kb as f64 / 1024.0);
                }
            }
            // agent 0.3.4: a GPU job's video memory per batch row at the workers' steady state likewise
            if let Some(x) = r.vram.as_ref().filter(|x| x.steady_per_row > 0) {
                if let Some(v) = Learned::vram_steady(&eng.cfg.layout, run, build, x.steady_per_row) {
                    log::info!("lease {id}: its workers held at most {} MB of video memory per batch row at their steady state: remembering {v} MB for this run and worker build", x.steady_per_row);
                }
            }
        }
        let given_back = !(end.is_some() && matches!(reason.as_str(), "done" | "finish") && self.after.is_none());
        let done = DoneReq {
            results: eng.results.delivered_seqs(&id),
            stats: self.merged_stats(),
            given_back,
            unfinished: unfinished.filter(|u| !u.is_null()),
            refused: None,
        };
        if let Err(e) = eng.results.write_done(&id, &done) {
            log::error!("lease {id}: could not write done: {e}");
        }
        eng.uploader.wake();
        // Dead or killed before `hello`: a start failure, so no new lease at once.
        if !self.hello_seen && matches!(self.after, Some(After::Kill(_))) {
            eng.start_failed(false);
            return LeaseEnd::NotStarted { blocked: false };
        }
        match self.after.clone() {
            None => LeaseEnd::Done,
            Some(After::GiveBack(w)) => LeaseEnd::GivenBack(w),
            Some(After::Expire) => LeaseEnd::Expired,
            Some(After::Quit) => LeaseEnd::Quit,
            Some(After::Kill(w)) => LeaseEnd::Failed(w),
            Some(After::Cancel) => LeaseEnd::Cancelled,
            Some(After::Revoked) => LeaseEnd::Revoked,
        }
    }
}


/// rev 19 (PROTOCOL 13): tuning a rolling CPU job's layout inside the lease - measurements of fresh workers, layout
/// handoffs (the main worker finishes its games beside the next one, as memory allows), and the workers that finish
/// their games meanwhile. 13.5: the crew - several workers serving one lease, each on a closed range of its released
/// ids but the main one (the tail, which gets the `extend`s).
impl Run {
    /// The next worker generation (`Ev::Worker`).
    fn next_gen(&mut self) -> u32 {
        self.gens += 1;
        self.gens
    }

    /// agent 0.3.4: the lease's job runs on a GPU (a rolling GPU job's workers yield for pauses, video memory is planned).
    fn gpu_lease(&self) -> bool {
        self.roll.as_ref().is_some_and(|r| r.gpu)
    }

    /// A layout's job-line `infer_workers` / `infer_threads`: a CPU job's; a GPU job's are the worker's own (None).
    fn infer_for(&self, l: &Layout) -> Option<(u32, u32)> {
        (!self.gpu_lease()).then_some((l.infer_workers, l.infer_threads))
    }

    /// A layout as the logs and the tray say it (a GPU job's: `Layout::describe_on`).
    fn desc(&self, l: &Layout) -> String {
        l.describe_on(self.gpu_lease())
    }

    /// agent 0.3.4: the lease's workers' pids (the main one while it runs, every one beside it): their GPU use is the
    /// lease's, never other programs' (the GPU busy rule, PROTOCOL 8).
    fn worker_pids(&self) -> Vec<u32> {
        let main = (self.main_exit.is_none()).then_some(self.worker.pid);
        main.into_iter().chain(self.mates.iter().map(|m| m.worker.pid)).collect()
    }

    /// The crew's size wanted now: the layout's workers, at most what this lease may still run; 1 without tuning.
    fn crew_target(&self) -> u32 {
        self.tuning.as_ref().map_or(1, |t| t.current.workers.min(t.crew_cap).max(1))
    }

    /// The workers playing new games now: the main one (unless it is finishing for a handoff, a restart or the lease's
    /// end) and the crew's members playing their ranges.
    fn crew_active(&self) -> usize {
        let main_on = self.main_exit.is_none()
            && self.tuning.as_ref().is_none_or(|t| t.handoff.is_none())
            && self.roll.as_ref().is_none_or(|r| !r.finishing && r.restart.is_none());
        usize::from(main_on) + self.mates.iter().filter(|m| m.active()).count()
    }

    fn crew_complete(&self) -> bool {
        self.crew_active() >= self.crew_target() as usize
    }

    /// One worker's memory limit: its part of the job's (the layout's workers share it, PROTOCOL 13.5).
    fn worker_limit(&self) -> u64 {
        let n = self.tuning.as_ref().map_or(1, |t| t.current.workers.max(1)) as u64;
        if self.lease_limit_mb == 0 {
            0
        } else {
            (self.lease_limit_mb / n).max(256)
        }
    }

    /// The crew's games in flight (an extension asks for about twice them): the main worker's for every worker wanted.
    fn crew_conc(&self) -> u64 {
        self.conc.unwrap_or(1) * self.crew_target() as u64
    }

    /// The main worker's own layout (its job line's tuning fields).
    fn main_layout(&self) -> Layout {
        let (iw, it) = self.infer.unwrap_or((1, self.threads));
        Layout { workers: 1, threads: self.threads, infer_workers: iw, infer_threads: it, groups: self.groups.unwrap_or(1), conc: self.conc.unwrap_or(1) }
    }

    /// The tray's and `ZeroFarm status`'s tuning line.
    fn show_tuning(&self, eng: &Engine) {
        let finishing: u64 = self.mates.iter().filter(|m| m.finishing).filter_map(|m| m.in_flight).sum();
        let beside = if finishing > 0 { format!(" ({finishing} games of the previous layout finishing)") } else { String::new() };
        let (active, target) = (self.crew_active(), self.crew_target() as usize);
        let building = if target > 1 && active < target { format!(" ({active} of {target} workers running)") } else { String::new() };
        let line = self.tuning.as_ref().map(|t| {
            let cur = t.current.describe_on(t.gpu);
            if t.fixed && t.gpu && t.bounds.0.is_none_or(|n| n <= 1) {
                return format!("{cur} (autotune off){building}{beside}");
            }
            if t.fixed {
                return format!("{cur} (workers {} set, autotune off){building}{beside}", t.current.workers);
            }
            match &t.probe {
                Some(_) => format!("measuring {cur} - {}{building}{beside}", t.entry.describe()),
                None if t.current != t.entry.wanted(&t.space) => format!("running {cur} - {}{building}{beside}", t.entry.describe()),
                None => format!("{}{building}{beside}", t.entry.describe()),
            }
        });
        crate::status::set(&eng.status, |s| s.tuning = line);
    }

    fn save_tuning(&self, eng: &Engine) {
        if let Some(t) = self.tuning.as_ref().filter(|t| !t.fixed) {
            let mut c = tune::Cache::load(&eng.cfg.layout);
            c.put(t.entry.clone());
            c.save(&eng.cfg.layout);
        }
    }

    /// The main worker's `status`: its counters (a measurement's sample, where its ids stand for the crew), or a
    /// handoff's read of where the old worker's ids stop.
    fn tune_status(&mut self, eng: &mut Engine, s: &Stats) {
        let id = self.lease.lease_id.clone();
        let Some(t) = self.tuning.as_mut() else { return };
        let started = s.get("games_started").and_then(Value::as_u64);
        if let Some(h) = t.handoff.as_mut() {
            if let (true, Some(st)) = (h.since.elapsed() >= eng.cfg.timings.tune_settle, started) {
                h.reads.push(st);
            }
            return;
        }
        let (Some(leaves), Some(games), Some(started)) = (s.get("leaves").and_then(Value::as_u64), s.get("games").and_then(Value::as_u64), started) else {
            // a worker that doesn't count its leaves and games can't be measured (nor run as a crew): not tuned this lease
            log::info!("lease {id}: the worker reports no leaves / games started: its layout isn't tuned");
            self.tuning = None;
            crate::status::set(&eng.status, |s| s.tuning = None);
            return;
        };
        self.counters = Some((leaves, games, started));
        self.counters_at = Some(Instant::now());
        self.probe_sample(eng);
    }

    /// A measurement's sample: the crew's counters summed - the main worker's, every member's playing its range, and those
    /// of members that finished their ranges since the measurement started (until they end, then their last counters):
    /// the sum only grows, through rotations.
    fn probe_sample(&mut self, eng: &mut Engine) {
        if self.tuning.as_ref().is_none_or(|t| t.probe.is_none()) {
            return;
        }
        let mut now = vec![(self.wgen, self.counters.unwrap_or((0, 0, 0)))];
        let t = self.tuning.as_ref().expect("checked");
        for m in &self.mates {
            if m.active() || t.probe_members.iter().any(|(w, _)| *w == m.wgen) {
                now.push((m.wgen, m.counters.unwrap_or((0, 0, 0))));
            }
        }
        let t = self.tuning.as_mut().expect("checked");
        for (w, c) in now {
            match t.probe_members.iter_mut().find(|(x, _)| *x == w) {
                Some((_, last)) => *last = c,
                None => t.probe_members.push((w, c)),
            }
        }
        let (leaves, games, started) = t.probe_members.iter().fold((0, 0, 0), |(l, g, s), (_, (a, b, c))| (l + a, g + b, s + c));
        let Some((probe, since)) = t.probe.as_mut() else { return };
        let verdict = probe.sample(since.elapsed().as_secs_f64(), leaves, games, started);
        let held = probe.held();
        match verdict {
            Verdict::Pending => {}
            Verdict::Rate(r, ph) => self.end_probe(eng, Ok((r, ph))),
            // (agent 0.3.13: one that waited for game ids measured nothing for want of them: put off, not discarded)
            Verdict::Invalid(why) if held => self.put_off_probe(eng, &why),
            Verdict::Invalid(why) => self.end_probe(eng, Err(why)),
        }
    }

    /// agent 0.3.13: a trial that waited for game ids (`Probe::hold`) and measured nothing - its games in flight didn't
    /// refill in time - is put off, not discarded: the coordinator held the ids back (its in-flight limit, the run's
    /// quota), which says nothing about the layout, and a handoff back to the incumbent would end games for nothing. Its
    /// worker plays on, nothing is counted against the layout, and it is the next one measured (afresh: a new worker)
    /// once the lease's games in flight are full again (`ids_short`), not before `retry` from now. Agents 0.3.4-0.3.12
    /// discarded it ("no measurement") and handed the job over to the incumbent, again and again on a DirectML PC.
    fn put_off_probe(&mut self, eng: &mut Engine, why: &str) {
        let id = self.lease.lease_id.clone();
        let retry = 2 * (eng.cfg.timings.tune_warm + eng.cfg.timings.tune_window);
        let Some(t) = self.tuning.as_mut() else { return };
        if t.probe.take().is_none() {
            return;
        }
        t.probe_members.clear();
        t.probe_load = None;
        t.hold_mark = None;
        t.postponed = Some(t.current);
        t.revert_due = false;
        t.no_trial_before = Instant::now() + retry;
        let line = format!(
            "the trial of {} is put off: the coordinator released too few game ids to fill its games in flight ({why}); it plays on and is measured again once ids come",
            t.current.describe_on(t.gpu)
        );
        log::info!("lease {id}: {line}");
        eng.worker_log.write_line(&format!("tuning [{id}]: {line}"));
        self.show_tuning(eng);
    }

    /// A measurement's end: recorded or discarded, remembered, the drainers going on. A rate counts only when the
    /// other programs' load over the trial passes `tune::judge_trial` (agent 0.3.3); it is recorded with that load.
    fn end_probe(&mut self, eng: &mut Engine, r: Result<(f64, tune::Phase), String>) {
        let id = self.lease.lease_id.clone();
        let retry = 2 * (eng.cfg.timings.tune_warm + eng.cfg.timings.tune_window);
        let Some(t) = self.tuning.as_mut() else { return };
        if t.probe.take().is_none() {
            return;
        }
        t.probe_members.clear();
        let now = zerofarm_common::now_unix();
        let l = t.current;
        let load = t.probe_load.take().map(|(t0, before)| (t.loads.mean(t0, t.now_s()).map(|(m, _)| m), before));
        let r = match (r, load) {
            (Ok(x), Some((during, before))) => tune::judge_trial(during, before).map(|m| (x, m)),
            (Ok(x), None) => Ok((x, None)),
            (Err(e), _) => Err(e),
        };
        let ld = l.describe_on(t.gpu);
        match r {
            Ok(((rate, ph), load)) => {
                let other = load.map(|m| format!(" (other programs {:.0} % of the CPUs)", m * 100.0)).unwrap_or_default();
                log::info!("lease {id}: {ld} measured {} leaves/s{other}", rate.round());
                eng.worker_log.write_line(&format!("tuning [{id}]: {ld} measured {} leaves/s{other}", rate.round()));
                t.entry.record(l, rate, load, Some(ph), &t.space, now);
            }
            Err(why) => {
                // (PROTOCOL 13: a trial is never paused and resumed; it is tried again later, from the incumbent)
                log::info!("lease {id}: the trial of {ld} is discarded: {why}");
                eng.worker_log.write_line(&format!("tuning [{id}]: the trial of {ld} is discarded: {why}"));
                t.entry.discard(l, &t.space, now);
                t.revert_due = l != t.entry.wanted(&t.space);
                t.no_trial_before = Instant::now() + retry;
            }
        }
        if t.entry.state == tune::State::Tuned && t.entry.tuned_t == Some(now) {
            log::info!("lease {id}: tuned: {}", t.entry.describe());
        }
        self.save_tuning(eng);
        self.show_tuning(eng);
    }

    /// The other programs' share of the CPUs, sampled every `tune_ping` (at least every 0.1 s) into the load track.
    fn sample_load(&mut self, eng: &Engine, t: &crate::Timings) {
        if self.tuning.as_ref().is_none_or(|x| x.load.is_some_and(|(at, _)| at.elapsed() < t.tune_ping.max(Duration::from_millis(100)))) {
            return;
        }
        let (idle_ms, total_ms) = crate::worker::sys_cpu_ms();
        // (agent 0.3.9: the agent's own CPU time is ours too - it compresses and uploads the workers' results, ~11-20k
        // games an hour from a GPU crew; 0.3.8 counted it as another program's, and the idle 4070 Ti PC read 7-11 % "other
        // programs" during its GPU trials, at the 10 % bar)
        let ours_ms = crate::worker::self_cpu_ms() + self.worker.cpu_ms() + self.mates.iter().map(|d| d.worker.cpu_ms()).sum::<u64>();
        let now = LoadMark { idle_ms, total_ms, ours_ms, dpc_ms: crate::worker::sys_dpc_ms() };
        let tu = self.tuning.as_mut().expect("checked");
        if let Some((at, prev)) = tu.load {
            let share = eng.signals.other_load(prev.other_since(&now));
            let ts = tu.now_s();
            tu.loads.push(ts, at.elapsed().as_secs_f64(), share);
        }
        tu.load = Some((Instant::now(), now));
    }

    /// agent 0.3.3: a worker finishes its games beside the ones playing new games (a handoff's old crew, a member that
    /// played its range or gave way to memory), or the main worker is finishing for a handoff or a restart: a drain is
    /// in progress, and no handoff starts until it is over (one drain at a time per lease, PROTOCOL 13.2).
    fn draining(&self) -> bool {
        self.mates.iter().any(|m| m.finishing)
            || self.tuning.as_ref().is_some_and(|t| t.handoff.is_some())
            || self.roll.as_ref().is_some_and(|r| r.restart.is_some())
    }

    /// What a worker of `conc` games in flight is planned to commit (PROTOCOL 10.6: the base + the cost per game).
    fn plan_mb(&self, conc: u64) -> u64 {
        self.roll.as_ref().map_or(0, |r| r.mem.cost.base_mb + conc * r.mem.cost.per_game_mb)
    }

    /// agent 0.3.3: the memory the lease's workers hold or will hold (MB) - every worker playing new games at the more of
    /// its commit and its plan (a fresh worker commits little at first and grows to its plan), every worker finishing
    /// its games (a drainer) at its commit - which a new worker's plan must fit beside within `PLAN_SHARE` of the job's
    /// limit. 0.3.2 counted commits only: a crew that had just started looked small, and two handoffs later 4 drainers
    /// and the crew committed 38.9 GB of a 31.7 GB cap.
    /// (`with_main`: the main worker counts too - not once it exited.)
    fn reserved_mb(&mut self, with_main: bool) -> u64 {
        let with_main = with_main && self.main_exit.is_none();
        let main_on = with_main
            && self.tuning.as_ref().is_none_or(|t| t.handoff.is_none())
            && self.roll.as_ref().is_none_or(|r| !r.finishing && r.restart.is_none());
        let main_plan = if main_on { self.plan_mb(self.conc.unwrap_or(1)) } else { 0 };
        let mut held = if with_main { self.worker.commit_mb().unwrap_or(0).max(main_plan) } else { 0 };
        let plans: Vec<u64> = self.mates.iter().map(|m| if m.active() { self.plan_mb(m.layout.conc) } else { 0 }).collect();
        for (m, p) in self.mates.iter_mut().zip(plans) {
            held += m.worker.commit_mb().unwrap_or(0).max(p);
        }
        held
    }

    /// A new worker of layout `l` (`n` of them) fits beside the lease's workers: their reservation (`reserved_mb`) plus
    /// its plan within `PLAN_SHARE` of the job's limit (no limit: always), and (agent 0.3.4) a GPU job's video memory
    /// likewise (`vram_room`).
    fn room_for(&mut self, n: u64, l: &Layout) -> bool {
        let limit = self.lease_limit_mb;
        let need = n * self.plan_mb(l.conc);
        (limit == 0 || (self.reserved_mb(true) + need) as f64 <= limit as f64 * crate::rolling::PLAN_SHARE) && self.vram_room(n, l, true)
    }

    /// agent 0.3.4: what a GPU worker batching `rows` rows is planned to hold in video memory (MB; 0 without a plan).
    fn vram_plan_mb(&self, rows: u64) -> u64 {
        self.roll.as_ref().and_then(|r| r.vram.as_ref()).map_or(0, |v| v.cost.plan_mb(rows))
    }

    /// agent 0.3.4: the video memory the lease's workers hold or will hold (MB), as `reserved_mb`: every worker playing
    /// new games at the more of its reading and its plan, every one finishing its games at its reading (`with_main`: the
    /// main worker counts too, unless it exited).
    fn vram_reserved(&self, with_main: bool) -> u64 {
        let with_main = with_main && self.main_exit.is_none();
        let main_on = with_main
            && self.tuning.as_ref().is_none_or(|t| t.handoff.is_none())
            && self.roll.as_ref().is_none_or(|r| !r.finishing && r.restart.is_none());
        let main = if !with_main {
            0
        } else if main_on {
            self.vram_mb.max(self.vram_plan_mb(self.main_layout().batch()))
        } else {
            self.vram_mb
        };
        main + self.mates.iter().map(|m| if m.active() { m.vram_mb.max(self.vram_plan_mb(m.layout.batch())) } else { m.vram_mb }).sum::<u64>()
    }

    /// agent 0.3.4: `n` new GPU workers of layout `l` fit the video memory plan beside the lease's workers
    /// (`vram_reserved`): within the budget (no plan, a CPU job: always).
    fn vram_room(&self, n: u64, l: &Layout, with_main: bool) -> bool {
        let Some(v) = self.roll.as_ref().and_then(|r| r.vram.as_ref()) else { return true };
        // (the budget of the lease's start, less what other programs took since: their use at the last reading)
        let ours = self.vram_mb + self.mates.iter().map(|m| m.vram_mb).sum::<u64>();
        let budget = v.budget_mb.min(crate::rolling::vram_budget(v.total_mb, v.adapter_used_mb.saturating_sub(ours)));
        self.vram_reserved(with_main) + n * v.cost.plan_mb(l.batch()) <= budget
    }

    /// A restart of the main worker is due (memory, a handoff that had to wait, a crashed tail).
    fn restart_pending(&self) -> bool {
        self.roll.as_ref().is_some_and(|r| r.restart.is_some())
    }

    /// agent 0.3.3: the restart due may start now that the main worker is gone: no worker beside it is finishing its
    /// games (nothing to wait for: members playing their ranges are watched by the crew's memory check), or the next
    /// worker's plan fits beside them (drainers at their commit, members at least at their plan) within `PLAN_SHARE` of
    /// the job's limit. Else it waits for the drain (`go`).
    fn restart_room(&mut self) -> bool {
        let Some(rs) = self.roll.as_ref().and_then(|r| r.restart.clone()) else { return true };
        // (agent 0.3.4: a GPU job's next worker fits the video memory beside the workers still there)
        let batch = self.roll.as_ref().map_or(0, |r| r.gpu_batch);
        let next = rs.layout.map(|l| l.per_worker()).unwrap_or(Layout { conc: rs.conc, groups: crate::rolling::gpu_groups(rs.conc, batch), ..self.main_layout() });
        if !self.mates.is_empty() && !self.vram_room(1, &next, false) {
            return false;
        }
        let limit = self.lease_limit_mb;
        if limit == 0 || !self.mates.iter().any(|m| m.finishing) {
            return true;
        }
        let need = self.plan_mb(next.conc);
        (self.reserved_mb(false) + need) as f64 <= limit as f64 * crate::rolling::PLAN_SHARE
    }

    /// Each tick: a measurement holds only while a trial may run (else it is discarded); a handoff goes on; else the
    /// next layout to measure (when a trial may run) or the incumbent (when the main worker runs another) is handed
    /// over to. A fixed layout (`autotune` off) runs no trial. agent 0.3.3: other programs' load is judged over windows
    /// (`tune::LoadTrack`: a burst passes, sustained load discards), a handoff starts only when no drain is in progress
    /// and at most `tune::MAX_HANDOFFS` per `tune_handoff_window`, and never to the layout the main worker runs unless to
    /// measure it.
    fn tune_tick(&mut self, eng: &mut Engine, t: &crate::Timings, must_pause: bool) {
        if self.tuning.is_none() {
            return;
        }
        self.sample_load(eng, t);
        // (the lease's first worker is rolling once its `hello` says so)
        let rolling_ok = self.roll.as_ref().is_some_and(|r| (r.active || !self.job_sent) && !r.finishing && r.restart.is_none());
        let (quiet_load, sustained) = self.tuning.as_ref().map_or((true, None), |x| {
            let now = x.now_s();
            (x.loads.quiet(now, baseline_s(t)), x.loads.sustained(now, sustained_s(t)))
        });
        let quiet = quiet_load && self.stall_since.is_none();
        let strict = eng.tune_ok && quiet && !must_pause;
        if let Some(x) = self.tuning.as_mut() {
            x.strict = strict;
        }
        if self.tuning.as_ref().is_some_and(|x| x.probe.is_some()) {
            let why = if !eng.tune_ok {
                Some("the computer is in use, on battery, running a fullscreen app or otherwise not idle".to_string())
            } else if must_pause || !matches!(self.state, WState::Starting | WState::Running) {
                Some("work was paused".to_string())
            } else if self.stall_since.is_some() {
                Some("the worker got no CPU".to_string())
            } else if let Some(m) = sustained {
                Some(format!("other programs used {:.0} % of the CPUs for {:.0} s", m * 100.0, sustained_s(t)))
            } else if !rolling_ok || self.after.is_some() || eng.quitting() {
                Some("the lease is ending".to_string())
            } else {
                None
            };
            match why {
                Some(w) => self.end_probe(eng, Err(w)),
                None => {
                    // agent 0.3.13: time spent waiting for game ids before the games in flight refilled is the
                    // coordinator's, not the layout's: the trial gets it on top (and is put off, not discarded, if it
                    // measures nothing)
                    let waiting = self.ids_refused();
                    let tu = self.tuning.as_mut().expect("checked");
                    let now = Instant::now();
                    match (waiting, tu.probe.as_mut()) {
                        (true, Some((p, _))) if !p.refilled() => {
                            if let Some(at) = tu.hold_mark {
                                p.hold(now.duration_since(at).as_secs_f64());
                            }
                            tu.hold_mark = Some(now);
                        }
                        _ => tu.hold_mark = None,
                    }
                    if tu.last_ping.elapsed() >= t.tune_ping {
                        tu.last_ping = Instant::now();
                        // (every worker the measurement counts: their counters are summed)
                        if self.job_sent {
                            self.send(json!({"op": "ping"}));
                        }
                        for m in self.mates.iter_mut().filter(|m| m.active() && m.job_sent && !m.worker.is_suspended()) {
                            let _ = m.worker.send(&json!({"op": "ping"}));
                        }
                    }
                }
            }
            return;
        }
        if self.tuning.as_ref().is_some_and(|x| x.handoff.is_some()) {
            // (work paused meanwhile: the handoff waits, its reads start again afterwards)
            if must_pause || !matches!(self.state, WState::Running) {
                if let Some(h) = self.tuning.as_mut().and_then(|x| x.handoff.as_mut()) {
                    h.since = Instant::now();
                    h.reads.clear();
                    h.last_ping = None;
                }
                return;
            }
            self.handoff_tick(eng, t, strict);
            return;
        }
        // (a fixed layout, `autotune` off, runs no trial; it is handed over only when the memory plan holds more games in
        // flight, `calibrate`)
        let fixed = self.tuning.as_ref().is_some_and(|x| x.fixed);
        let running = matches!(self.state, WState::Running) && self.job_sent && self.after.is_none() && !eng.quitting();
        // (a worker runs a while before it is replaced; one drain at a time - no handoff while any worker finishes its
        // games; a crew is handed over only once it is complete and settled)
        let settled = self.crew_complete() && self.crew_changed.elapsed() >= t.tune_warm;
        if !running || !rolling_ok || self.draining() || self.started.elapsed() < t.tune_warm || !settled {
            return;
        }
        // agent 0.3.13: no handoff while the lease is short of game ids (refused, its games in flight not full): a trial's
        // new worker couldn't fill its games in flight either - measured nothing, it was discarded and the job handed back,
        // the old workers' last games ended for nothing each time (agents 0.3.4-0.3.12). The job plays on as it is.
        if self.ids_short() {
            let id = self.lease.lease_id.clone();
            if let Some(tu) = self.tuning.as_mut().filter(|tu| !tu.ids_wait) {
                tu.ids_wait = true;
                let line = "trials wait: the coordinator releases too few game ids to keep the games in flight full";
                log::info!("lease {id}: {line}");
                eng.worker_log.write_line(&format!("tuning [{id}]: {line}"));
            }
            return;
        }
        if let Some(tu) = self.tuning.as_mut() {
            tu.ids_wait = false;
        }
        // agent 0.3.7: a GPU job's trials wait for its memory plan's early reading (`calibrate`; a cost remembered from an
        // earlier lease counts): the start is picked again for the measured plan (the crew it holds), and trials under the
        // platform's default cost would measure single workers of ~140 games that the plan then drops - each trial's fresh
        // worker restarting the reading. A worker whose commit can't be read never gives one; and at most `GPU_TRIAL_WAIT`.
        // (0.3.5-0.3.6 postponed every trial that didn't fit beside the running worker, which kept it for the reading.)
        let lease_age = self.tuning.as_ref().map_or(Duration::ZERO, |x| x.origin.elapsed());
        let readable = self.worker.commit_mb().is_some();
        let uncalibrated = self.gpu_lease() && readable && self.roll.as_ref().is_some_and(|r| !r.mem.measured && !r.early_done) && lease_age < GPU_TRIAL_WAIT;
        let tu = self.tuning.as_mut().expect("checked");
        tu.handoffs.retain(|at| at.elapsed() < t.tune_handoff_window);
        if tu.handoffs.len() >= tune::MAX_HANDOFFS {
            // (the safety net: the next handoff waits until the oldest of these leaves the window)
            return;
        }
        let wanted = tu.entry.wanted(&tu.space);
        let next = if !fixed && strict && !uncalibrated && !tu.revert_due && Instant::now() >= tu.no_trial_before { tu.entry.plan(&tu.space) } else { None };
        // (agent 0.3.13: a trial put off for want of game ids runs on until it is measured afresh - no handoff back to the
        // incumbent meanwhile, which would end games for nothing)
        if next.is_none() && tu.postponed == Some(tu.current) {
            return;
        }
        let (target, measure) = match next {
            Some(l) => (l, true),
            None if tu.current != wanted && (!fixed || wanted.conc > tu.current.conc) => (wanted, strict && !fixed),
            None => return,
        };
        // (never to the layout the main worker runs, unless to measure it: a measurement needs a fresh worker)
        if target == tu.current && !measure {
            return;
        }
        // agent 0.3.7: every measured trial is handed over to - a GPU job's too, its workers after the old ones' drain when
        // they don't fit beside them (as a CPU job's, PROTOCOL 13.2). 0.3.5-0.3.6 postponed a GPU trial that didn't fit
        // beside, because a GPU drain then took 6-8 minutes; 0.3.6 ends a drain's last games after `Timings::drain_tail`
        // (their ids go back with the lease). Postponed, a memory-tight PC's GPU job could measure nothing: the 4070 Ti PC's
        // crew of 3 x 575 used its whole limit, its own re-measurement (the incumbent, handed over to unmeasured) never fit
        // beside it, and its tuning stayed at 0 trials for the lease.
        self.start_handoff(eng, target, measure);
    }

    /// A layout handoff starts: the main worker - and every member of the crew - starts no new game (`finish`); once
    /// the main worker says where its ids stop, the next worker starts beside it with the rest (as memory allows; else
    /// after it ended); the members' unstarted ids go to the spare ranges as they say where theirs stop.
    fn start_handoff(&mut self, eng: &Engine, next: Layout, measure: bool) {
        let line = format!("handing the job over to {} ({})", self.desc(&next), if measure { "a measured trial" } else { "the incumbent, not measured" });
        log::info!("lease {}: {line}", self.lease.lease_id);
        eng.worker_log.write_line(&format!("tuning [{}]: {line}", self.lease.lease_id));
        self.send(json!({"op": "finish"}));
        if let Some(r) = self.roll.as_mut() {
            r.want_more = false;
        }
        let mut spare = Vec::new();
        for m in self.mates.iter_mut().filter(|m| m.active()) {
            m.finishing = true;
            if m.job_sent {
                let _ = m.worker.send(&json!({"op": "finish"}));
                m.reclaim = Some(Reclaim { since: Instant::now(), reads: Vec::new(), last_ping: None });
            } else {
                // (no job yet: none of its ids started)
                let _ = m.worker.send(&json!({"op": "quit"}));
                spare.push((m.offset, m.end));
            }
        }
        self.spare.extend(spare);
        self.crew_changed = Instant::now();
        let tu = self.tuning.as_mut().expect("tuning");
        tu.handoffs.push(Instant::now());
        tu.postponed = None;
        tu.handoff = Some(Handoff { next, measure, since: Instant::now(), reads: Vec::new(), last_ping: None });
    }

    /// A handoff waits for the old worker's id count (two equal reads after `finish`, else after `10 x tune_settle` the
    /// next worker takes fresh ids), then for room: the lease's workers' reservation (`reserved_mb`: the old crew, now
    /// finishing, at its commit) plus the next worker's plan (a measured crew's: every one of its workers') within
    /// `PLAN_SHARE` of the job's limit. A trial that can't be measured when its worker starts: the incumbent starts instead,
    /// unmeasured, and the trial counts as discarded (`settle_trial`; agent 0.3.3: 0.3.2 started it unmeasured and handed
    /// over to it again at once; 0.3.4 counted only the incumbent's re-measure, not a candidate's).
    fn handoff_tick(&mut self, eng: &mut Engine, t: &crate::Timings, strict: bool) {
        let retry = 2 * (t.tune_warm + t.tune_window);
        let tu = self.tuning.as_mut().expect("tuning");
        let h = tu.handoff.as_mut().expect("handoff");
        if h.since.elapsed() < t.tune_settle {
            return;
        }
        if h.last_ping.is_none_or(|p| p.elapsed() >= t.tune_settle) {
            h.last_ping = Some(Instant::now());
            self.send(json!({"op": "ping"}));
        }
        let h = self.tuning.as_ref().and_then(|x| x.handoff.clone()).expect("handoff");
        let n = h.reads.len();
        let tail = if n >= 2 && h.reads[n - 1] == h.reads[n - 2] {
            Some(h.reads[n - 1])
        } else if h.since.elapsed() > t.tune_settle * 10 {
            None
        } else {
            return;
        };
        // (agent 0.3.5: whether the trial can be measured is decided now that its worker starts - 0.3.4 decided it at any
        // tick while the handoff waited, and swapped a candidate for the incumbent without counting it: the next trial
        // was the same candidate 20 s later, again and again)
        self.settle_trial(eng, strict, retry);
        let h = self.tuning.as_ref().and_then(|x| x.handoff.clone()).expect("handoff");
        // (a measured crew starts together: room for all of it; otherwise its workers join as memory allows)
        let workers = if h.measure { h.next.workers.max(1) as u64 } else { 1 };
        if !self.room_for(workers, &h.next.per_worker()) {
            // (no room yet: the old workers finish their games, and the next one starts once they freed enough - at the
            // latest after the main worker ended, `handoff_after_exit`)
            return;
        }
        self.spawn_beside(eng, h.next, h.measure, tail);
    }

    /// The next worker of a handoff starts beside the old one, which becomes a drainer (stopped while the next crew is
    /// measured). Its ids: the old worker's unstarted tail and on (`started`: the old worker's games started, from
    /// its own first id), else fresh ids after the released ones. `next` is the whole layout (a crew's other workers
    /// join as `add_member` allows).
    fn spawn_beside(&mut self, eng: &mut Engine, next: Layout, measure: bool, started: Option<u64>) {
        let id = self.lease.lease_id.clone();
        let Some((lo, released, offset)) = self.roll.as_ref().map(|r| (r.setup.lo, r.released, r.offset)) else { return };
        let first = started.map_or(lo + released, |s| lo + offset + s).clamp(lo + offset, lo + released);
        let dir = format!("out-w{}", self.spawns + 1);
        if let Err(e) = std::fs::create_dir_all(self.work.join(&dir)) {
            log::error!("lease {id}: cannot start the next layout's worker: {e}");
            if let Some(t) = self.tuning.as_mut() {
                t.handoff = None;
            }
            return;
        }
        let gen = self.next_gen();
        let limit = if self.lease_limit_mb == 0 { 0 } else { (self.lease_limit_mb / next.workers.max(1) as u64).max(256) };
        let w = match self.spawn.spawn(gen, self.tx.clone(), limit) {
            Ok(w) => w,
            Err(e) => {
                // (the old worker finishes its games; the lease ends with it)
                log::error!("lease {id}: cannot start the next layout's worker: {e}");
                if let Some(t) = self.tuning.as_mut() {
                    t.handoff = None;
                }
                return;
            }
        };
        log::info!(
            "lease {id}: worker pid {} started with layout {} (ids from {}{}, memory limit {limit} MB); pid {} finishes its games beside it",
            w.pid,
            self.desc(&next),
            first,
            if started.is_none() { ", fresh: the old worker's id count is unknown" } else { "" },
            self.worker.pid
        );
        let old_layout = self.main_layout();
        let old_gen = std::mem::replace(&mut self.wgen, gen);
        self.spawns += 1;
        let old = std::mem::replace(&mut self.worker, w);
        let old_dir = std::mem::replace(&mut self.out_dir, dir);
        let (old_offset, old_peak) = self.roll.as_ref().map_or((0, 0), |r| (r.offset, r.peak_mb));
        self.mates.push(Mate {
            worker: old,
            wgen: old_gen,
            out_dir: old_dir,
            end_ev: self.end_ev.take(),
            eof: std::mem::take(&mut self.eof),
            last_msg: Instant::now(),
            // (agent 0.3.13: its games in flight as its last status said - the beats count them, `merged_stats`)
            in_flight: self.progress.map(|(g, st)| st.saturating_sub(g)),
            tail: started.map(|_| first),
            finishing: true,
            offset: old_offset,
            end: first - lo,
            layout: old_layout,
            job_sent: true,
            spawned: self.started,
            pinged: false,
            pinged_at: None,
            counters: self.counters.take(),
            reclaim: None,
            gen_floor: self.gen_floor,
            peak_mb: old_peak,
            fatal: None,
            yield_at: None,
            yielded: false,
            want_stop: false,
            vram_mb: std::mem::take(&mut self.vram_mb),
            stats_vram: std::mem::take(&mut self.stats_vram),
            finishing_since: None,
            tail_quit: std::mem::take(&mut self.tail_quit),
            watch: Watch::new(0),
            resuming: None,
        });
        if let Some(r) = self.roll.as_mut() {
            r.offset = first - lo;
            r.want_more = false;
            r.refused = false;
            r.next_extend = Instant::now();
            r.peak_mb = 0;
        }
        let per = next.per_worker();
        self.conc = Some(per.conc);
        self.groups = Some(per.groups);
        self.threads = per.threads;
        self.infer = self.infer_for(&per);
        self.batch_rows = per.batch();
        self.state = WState::Starting;
        self.started = Instant::now();
        self.last_msg = Instant::now();
        self.pinged = false;
        self.job_sent = false;
        self.killed = false;
        self.stall_since = None;
        self.counters_at = None;
        self.progress = None;
        (self.finish_sent, self.tail_quit) = (None, false);
        self.crew_changed = Instant::now();
        self.mark = CpuMark::take(&self.worker);
        self.sample = (self.mark, Instant::now());
        if let Some(t) = self.tuning.as_mut() {
            t.current = next;
            t.handoff = None;
            t.load = None;
            if next == t.entry.wanted(&t.space) {
                t.revert_due = false;
            }
            t.probe = None;
            t.probe_load = None;
            t.probe_members.clear();
            if measure {
                t.start_probe(next.total_conc(), &eng.cfg.timings);
            }
        }
        self.show_tuning(eng);
        self.beat_soon();
    }

    /// agent 0.3.5: a handoff's trial whose worker starts now while no trial may run (the computer got busy or used): the
    /// incumbent starts instead, unmeasured, and the trial counts as discarded (retried later, skipped after
    /// `tune::MAX_DISCARDS`; no trial before `retry` from now).
    fn settle_trial(&mut self, eng: &Engine, strict: bool, retry: Duration) {
        let id = self.lease.lease_id.clone();
        let Some(tu) = self.tuning.as_mut() else { return };
        let wanted = tu.entry.wanted(&tu.space);
        let Some(h) = tu.handoff.as_mut().filter(|h| h.measure && !strict) else { return };
        h.measure = false;
        let l = std::mem::replace(&mut h.next, wanted);
        let line = format!("the trial of {} can't run now (the computer is busy or not idle): counted as discarded, {} instead", l.describe_on(tu.gpu), wanted.describe_on(tu.gpu));
        log::info!("lease {id}: {line}");
        eng.worker_log.write_line(&format!("tuning [{id}]: {line}"));
        tu.entry.discard(l, &tu.space, zerofarm_common::now_unix());
        tu.no_trial_before = Instant::now() + retry;
        tu.revert_due = false;
        self.save_tuning(eng);
    }

    /// The main worker ended while a handoff waited for room: the next worker starts after it (`restart_worker`, with
    /// the released ids it never started).
    fn handoff_after_exit(&mut self, eng: &Engine) {
        // (agent 0.3.5: a trial is measured only if one may run now)
        let strict = self.tuning.as_ref().is_some_and(|t| t.strict);
        let retry = 2 * (eng.cfg.timings.tune_warm + eng.cfg.timings.tune_window);
        self.settle_trial(eng, strict, retry);
        let Some(h) = self.tuning.as_mut().and_then(|t| t.handoff.take()) else { return };
        if let Some(r) = self.roll.as_mut().filter(|r| !r.finishing && r.restart.is_none()) {
            r.restart = Some(Restart { conc: h.next.conc, burn: false, layout: Some(h.next), count: false, measure: h.measure });
        }
    }

    /// A line of a worker beside the main one: a crew member gets its job line at `hello`, its counters feed a
    /// measurement; a finishing one's games and results count as any, and after `finish` its counters say where its ids
    /// stop.
    fn on_mate(&mut self, eng: &mut Engine, g: u32, m: WorkerMsg) {
        let Some(i) = self.mates.iter().position(|d| d.wgen == g) else { return };
        let v = match m {
            WorkerMsg::Eof => {
                self.mates[i].eof = true;
                return;
            }
            WorkerMsg::BadLine(s) => {
                log::warn!("lease {}: worker pid {} wrote a non-JSON line: {s}", self.id(), self.mates[i].worker.pid);
                return;
            }
            WorkerMsg::Line(v) => v,
        };
        self.mates[i].last_msg = Instant::now();
        self.mates[i].pinged = false;
        let settle = eng.cfg.timings.tune_settle;
        match v.get("ev").and_then(Value::as_str).unwrap_or("") {
            "hello" if !self.mates[i].job_sent => {
                let aw = v.get("aw").and_then(Value::as_u64);
                let bid = v.get("build_id").and_then(Value::as_str).unwrap_or("");
                let rolling = v.get("features").and_then(Value::as_array).is_some_and(|f| f.iter().any(|x| x.as_str() == Some(crate::rolling::WORKER_FEATURE)));
                if aw != Some(zerofarm_common::AW_VERSION as u64) || bid != self.prep.build.build_id || !rolling {
                    let m = &mut self.mates[i];
                    log::error!("lease {}: worker pid {} says aw {aw:?}, build {bid}, rolling {rolling}: not a member of this crew", self.lease.lease_id, m.worker.pid);
                    m.fatal = Some(format!("worker says aw {aw:?}, build {bid}"));
                    m.worker.kill();
                    return;
                }
                if self.mates[i].finishing {
                    // (handed off before its job: none of its ids started)
                    let _ = self.mates[i].worker.send(&json!({"op": "quit"}));
                    return;
                }
                let m = &self.mates[i];
                let l = m.layout;
                let line = self.job_line_with(l.threads, Some(l.conc), Some(l.groups), self.infer_for(&l), &m.out_dir, Some((m.offset, m.end - m.offset)));
                let m = &mut self.mates[i];
                if let Err(e) = m.worker.send(&line) {
                    log::warn!("lease {}: write to worker pid {} failed: {e}", self.lease.lease_id, m.worker.pid);
                }
                m.job_sent = true;
            }
            "fin" => {
                let n = v.get("n").and_then(Value::as_u64).unwrap_or(0);
                self.games_done += n;
                eng.add_games(n);
            }
            k @ ("shard" | "records" | "evals") => {
                let (dir, floor) = (self.mates[i].out_dir.clone(), self.mates[i].gen_floor);
                let floor = self.on_result_file(eng, k, &v, &dir, floor);
                self.mates[i].gen_floor = floor;
            }
            "status" => {
                let s = v.get("stats");
                let n = |k: &str| s.and_then(|s| s.get(k)).and_then(Value::as_u64);
                let m = &mut self.mates[i];
                if let (Some(st), Some(g)) = (n("games_started"), n("games")) {
                    m.in_flight = Some(st.saturating_sub(g));
                }
                if let (Some(rc), Some(st)) = (m.reclaim.as_mut(), n("games_started")) {
                    if rc.since.elapsed() >= settle {
                        rc.reads.push(st);
                    }
                }
                if let (Some(l), Some(g), Some(st)) = (n("leaves"), n("games"), n("games_started")) {
                    m.counters = Some((l, g, st));
                }
                // (agent 0.3.15: its progress watched)
                m.watch.status(n("leaves"), n("games"), n("games_started"));
                m.stats_vram = n("vram_mb").unwrap_or(0);
                if m.active() {
                    self.probe_sample(eng);
                }
            }
            "selftest" => {
                let gen = v.get("gen").and_then(Value::as_u64).or_else(|| self.prep.ckpt.as_ref().map(|(c, _)| c.gen));
                let ok = v.get("ok").and_then(Value::as_bool).unwrap_or(false);
                let mut t = Map::new();
                for k in ["provider", "max_abs", "ok"] {
                    t.insert(k.into(), v.get(k).cloned().unwrap_or(Value::Null));
                }
                self.selftests.insert(gen.map(|g| g.to_string()).unwrap_or_else(|| "model".into()), Value::Object(t));
                if !ok {
                    log::error!("lease {}: network {gen:?} failed its self-test in worker pid {}: {v}", self.id(), self.mates[i].worker.pid);
                    self.selftest_failed = true;
                }
            }
            "end" => self.mates[i].end_ev = Some(v),
            // agent 0.3.4: a GPU job's member released its session for a pause: stopped now (unless the pause ended)
            "yielded" => {
                let m = &mut self.mates[i];
                m.yield_at = None;
                // (a pause that ended before it answered: `resume` went out already, `mates_tick`)
                m.yielded = m.want_stop;
                if m.want_stop && !m.worker.is_suspended() {
                    m.worker.suspend();
                }
            }
            "error" if v.get("fatal").and_then(Value::as_bool) == Some(true) => {
                let msg = v.get("message").and_then(Value::as_str).unwrap_or("").to_string();
                log::warn!("lease {}: worker pid {} failed: {msg}", self.id(), self.mates[i].worker.pid);
                self.mates[i].fatal = Some(msg);
            }
            // agent 0.3.15: a non-fatal one - while resuming, a failed resume once nothing is played after it (`Watch`)
            "error" => {
                let msg = v.get("message").and_then(Value::as_str).unwrap_or("").to_string();
                let m = &mut self.mates[i];
                let resuming = m.resuming.is_some();
                log::warn!("lease {}: worker pid {} error{}: {msg}", self.lease.lease_id, m.worker.pid, if resuming { " while resuming" } else { "" });
                m.watch.error(&msg, resuming);
            }
            "resumed" => {
                let m = &mut self.mates[i];
                m.resuming = None;
                m.watch.resume_error = None;
            }
            _ => {}
        }
    }

    /// Workers beside the main one that exited: their last lines, then gone. A finishing one ended as expected (the next
    /// worker's ids were taken from its unstarted tail: checked against its `end`; one that ended before it said where
    /// its ids stop gives its unstarted ids to the spare ranges); a crew member that exited while playing its range
    /// failed (`mate_failed`).
    fn reap_mates(&mut self, eng: &mut Engine) {
        let mut i = 0;
        while i < self.mates.len() {
            let Some(code) = self.mates[i].worker.try_wait() else {
                i += 1;
                continue;
            };
            let until = Instant::now() + Duration::from_secs(2);
            while !self.mates[i].eof && Instant::now() < until {
                if let Ok(ev) = self.rx.recv_timeout(Duration::from_millis(50)) {
                    self.on_event(eng, ev);
                }
            }
            let d = self.mates.remove(i);
            let lo = self.roll.as_ref().map_or(0, |r| r.setup.lo);
            let reason = d.end_ev.as_ref().and_then(|e| e.get("reason")).and_then(Value::as_str).map(String::from);
            if d.finishing || reason.is_some() {
                log::info!(
                    "lease {}: worker pid {} {} ended ({})",
                    self.id(),
                    d.worker.pid,
                    if d.finishing { "finishing its games" } else { "of the crew" },
                    reason.clone().unwrap_or_else(|| format!("exit code {code}, its games in flight lost"))
                );
                // (one that died finishing its games - not told to quit - failed: reported once per lease)
                let stopping = matches!(self.state, WState::Stopping(_)) || self.after.is_some() || eng.quitting();
                if reason.is_none() && !stopping && !self.crash_reported {
                    self.crash_reported = true;
                    let tail = d.worker.stderr_tail();
                    let kind = if tail.contains("panicked at") { "panic" } else { "crash" };
                    let why = d.fatal.clone().unwrap_or_else(|| format!("exit code {code} without `end`"));
                    self.report_with(eng, kind, &format!("worker finishing its games failed: {why}; its games in flight are lost"), &tail, Some(code as i64));
                }
                // where its unstarted ids really began (its `end`: the released ids it never finished, its tail last)
                let gids = d.end_ev.as_ref().and_then(|e| e.get("unfinished")).and_then(|u| u.get("gids")).map(crate::util::parse_ranges);
                let real = gids.as_ref().and_then(|g| g.last().map(|r| r.0));
                if let (Some(est), Some(real)) = (d.tail, real) {
                    if est != real {
                        log::warn!("lease {}: the next worker's ids started at {est}, the old worker's unstarted ones at {real}", self.id());
                    }
                }
                // (its ids not read yet: a `finish` end's unfinished ids were never started - they are spare)
                if let (Some(_), Some("finish"), Some(g)) = (&d.reclaim, reason.as_deref(), gids) {
                    for (a, b) in g {
                        let (a, b) = (a.saturating_sub(lo).max(d.offset), b.saturating_sub(lo).min(d.end));
                        if a < b {
                            self.spare.push((a, b));
                        }
                    }
                }
            } else {
                self.mate_failed(eng, d, code);
            }
            self.crew_changed = Instant::now();
            if let Some(t) = self.tuning.as_mut() {
                t.load = None;
            }
            self.show_tuning(eng);
            self.beat_soon();
        }
    }

    /// rev 19 (PROTOCOL 13.5): a crew member exited while playing its range: its games in flight are lost (its ids go
    /// back with `done`'s worked-out unfinished ids). One that ran into its memory limit shrinks the next members (the
    /// cost per game remembered); any other failure is reported once per lease. A replacement joins (as `add_member`
    /// allows) while restarts are left; then the crew runs smaller.
    fn mate_failed(&mut self, eng: &mut Engine, d: Mate, code: i32) {
        let id = self.lease.lease_id.clone();
        let stopping = matches!(self.state, WState::Stopping(_)) || self.after.is_some() || eng.quitting();
        let limit = d.worker.mem_limit_mb;
        let peak = d.worker.job_peak_mb().unwrap_or(0).max(d.peak_mb);
        let tail = d.worker.stderr_tail();
        let why = d.fatal.clone().unwrap_or_else(|| format!("exit code {code} without `end`"));
        // (agent 0.3.4: a GPU job's member that ran out of video memory, not of its memory)
        let vram_hit = self.vram_failed(&why, &tail, peak, limit);
        let hit = !vram_hit && ((limit > 0 && peak as f64 >= limit as f64 * crate::rolling::HIT_SHARE) || alloc_failure(&why) || alloc_failure(&tail));
        if stopping && self.killed {
            // (agent 0.3.7: killed with the lease - cancelled, stopped, the agent quitting -: no failure; 0.3.6 warned
            // "failed (exit code 57005 without `end`)" for every member of a cancelled crew)
            log::info!("lease {id}: worker pid {} of the crew stopped with the lease ({why}); its games in flight end here", d.worker.pid);
        } else {
            log::warn!("lease {id}: worker pid {} of the crew failed ({why}); its games in flight are lost", d.worker.pid);
        }
        if self.tuning.as_ref().is_some_and(|t| t.probe.is_some()) {
            self.end_probe(eng, Err("a worker of the crew failed during the trial".into()));
        }
        let Some(base) = self.roll.as_ref().filter(|r| !r.finishing && !stopping).map(|r| r.mem.cost) else { return };
        let others = self.mates.iter().filter(|m| m.active()).count() + 1;
        if hit {
            let per = crate::rolling::measured_per_game(peak.max(limit), d.layout.conc, base);
            self.learn_cost(eng, per);
        }
        if vram_hit {
            self.learn_vram_oom(eng, d.layout.batch(), d.vram_mb);
        }
        let Some(r) = self.roll.as_mut() else { return };
        let restart_left = r.restarts < crate::rolling::MAX_RESTARTS && !self.selftest_failed;
        if hit || vram_hit {
            let cost = r.mem.cost;
            let lim = if vram_hit || limit == 0 { u64::MAX >> 24 } else { limit };
            match crate::rolling::restart_games(d.layout.conc, lim, cost).filter(|_| restart_left) {
                Some(n) => {
                    r.restarts += 1;
                    self.shrink_members(n);
                }
                None => self.cap_crew(others as u32),
            }
            if !self.crash_reported {
                self.crash_reported = true;
                let msg = if vram_hit {
                    format!("crew worker ran out of video memory with {} games in flight (batches of {} rows): {why}", d.layout.conc, d.layout.batch())
                } else {
                    format!("crew worker ran into its {limit} MB memory limit (peak {peak} MB) with {} games in flight: {why}", d.layout.conc)
                };
                self.report_with(eng, "crash", &msg, &tail, Some(code as i64));
            }
        } else {
            if restart_left {
                r.restarts += 1;
            } else {
                self.cap_crew(others as u32);
            }
            if !self.crash_reported {
                self.crash_reported = true;
                let kind = if tail.contains("panicked at") { "panic" } else { "crash" };
                self.report_with(eng, kind, &format!("crew worker failed: {why}; the other workers go on"), &tail, Some(code as i64));
            }
        }
    }

    /// A crash report with another worker's stderr (a crew member's).
    fn report_with(&self, eng: &Engine, kind: &str, message: &str, tail: &str, exit_code: Option<i64>) {
        eng.report_crash(CrashReq {
            build_id: Some(self.lease.build.build_id.clone()),
            kind: kind.into(),
            message: format!("{message}{}", self.selftest_note()),
            backtrace: Some(tail.to_string()).filter(|s| !s.is_empty()),
            lease_id: Some(self.lease.lease_id.clone()),
            exit_code,
            agent: None,
            os_error: None,
        });
    }

    /// The crew's next members run `n` games in flight each (a memory failure): the layouts of this lease keep within it.
    fn shrink_members(&mut self, n: u64) {
        if let Some(t) = self.tuning.as_mut() {
            t.current.conc = t.current.conc.min(n).max(1);
            t.space.conc_max = t.space.conc_max.min(t.current.total_conc());
            t.space.conc_min = t.space.conc_min.min(t.space.conc_max);
            // (agent 0.3.3: the batches within bounds, the groups kept where the fewer games allow)
            t.current = t.space.fit(t.current);
            log::info!("lease {}: the crew's next workers run {} games in flight each in {} groups", self.lease.lease_id, t.current.conc, t.current.groups);
        }
    }

    /// agent 0.3.3 (PROTOCOL 10.6): the memory plan calibrates itself. The lease's steady readings (the most a worker
    /// committed per game at its steady state, `rolling::steady_reading`) are remembered when lower than planned
    /// (`Learned::steady`: with a margin, per run and worker build); when well lower (`RAISE_INFLIGHT_SHARE`) the lease
    /// plans with them from now on - the tuner's memory bounds widen (an "auto" job's games in flight are worked out
    /// again), a layout the old plan held back gets what the new one holds (`tune::Entry::lift_conc`), and the layout
    /// wanted changes: handed over to as any (one drain at a time, within the memory cap). Without a tuned layout the
    /// next lease plans with it.
    fn calibrate(&mut self, eng: &Engine) {
        let id = self.lease.lease_id.clone();
        let (run, build) = (self.lease.run.clone().unwrap_or_default(), self.lease.build.build_id.clone());
        let groups = Engine::job_shape(&self.plan).2;
        let limit = self.lease_limit_mb;
        let Some(r) = self.roll.as_mut().filter(|r| r.active && !r.finishing) else { return };
        let mut changed = Vec::new();
        // (agent 0.3.6: the base, from the workers' base readings - lower than planned: planned with at once)
        if r.base_min > 0 && r.base_min != r.base_applied {
            r.base_applied = r.base_min;
            if let Some(raw) = Learned::set_base(&eng.cfg.layout, &run, &build, r.gpu, r.base_min) {
                // (agent 0.3.9: a GPU worker's reading plus the base's drift after its first status, as `Learned::plan`;
                // within a lease only ever lower, a higher base planned from the next lease)
                let b = crate::rolling::planned_base(raw, r.gpu);
                let was = r.mem.cost.base_mb;
                if (b as f64) < was as f64 * (1.0 - crate::rolling::BASE_TOLERANCE) {
                    let line = format!("a worker commits {} MB before its games grow: planning a base of {b} MB (was {was})", r.base_min);
                    log::info!("lease {id}: {line}");
                    eng.worker_log.write_line(&format!("tuning [{id}]: {line}"));
                    r.mem.cost.base_mb = b;
                    changed.push(format!("a base of {b} MB from now on (was {was})"));
                }
            }
        }
        // (agent 0.3.5: a GPU job's early reading first - once -, then the steady ones)
        let reading = if r.steady_kb > 0 && r.steady_kb != r.calibrated {
            r.calibrated = r.steady_kb;
            Some((r.steady_kb, false))
        } else if r.early_kb > 0 && !r.early_done && r.steady_kb == 0 {
            r.early_done = true;
            Some((r.early_kb, true))
        } else {
            None
        };
        if let Some((reading, early)) = reading {
            let learned = if early { Learned::early(&eng.cfg.layout, &run, &build, r.gpu, reading) } else { Learned::steady(&eng.cfg.layout, &run, &build, r.gpu, reading) };
            if let Some(v) = learned {
                let was = r.mem.cost.per_game_mb;
                let when = if early { "early in their games (planned x 2)" } else { "at their steady state" };
                let mb = reading as f64 / 1024.0;
                log::info!("lease {id}: its workers commit {mb:.1} MB per game in flight {when} (planned {was}): remembering {v} MB for this run and worker build");
                eng.worker_log.write_line(&format!("tuning [{id}]: the workers commit {mb:.1} MB per game in flight {when}: planning {v} MB (was {was})"));
                if v as f64 <= was as f64 * crate::rolling::RAISE_INFLIGHT_SHARE {
                    r.mem.cost.per_game_mb = v;
                    r.mem.measured = true;
                    changed.push(format!("{v} MB per game in flight from now on (was {was})"));
                }
            }
        }
        if changed.is_empty() {
            return;
        }
        let (cost, setting) = (r.mem.cost, r.setup.cpu);
        let Some(t) = self.tuning.as_mut().filter(|t| t.space.mem.is_some()) else { return };
        let old = t.space;
        t.space.mem = Some((limit, cost));
        // (agent 0.3.4: a GPU job's games in flight are its target's, the memory plan cuts each worker's - `Space::fit`)
        match (t.gpu, setting) {
            (true, _) => {}
            (false, crate::rolling::Inflight::Auto) => {
                let (c, _) = crate::rolling::cpu_shape(setting, groups, MemPlan { ram_mb: limit, cost, measured: true });
                (t.space.conc_min, t.space.conc_max) = ((c / 2).max(1), c.max(1));
            }
            (false, crate::rolling::Inflight::N(n)) => (t.space.conc_min, t.space.conc_max) = (n, n),
        }
        // (agent 0.3.4: as many workers as the memory plan holds now - a crew the old plan held back is possible again)
        let (set, max) = t.bounds;
        t.space.bound_workers(set, max);
        t.entry.lift_conc(&old, &t.space);
        // (agent 0.3.5: a GPU job's start was picked under the old plan - an uncalibrated 45 MB a game held the 4070 Ti PC
        // to one worker of 139 -: picked again while nothing else won, so the job goes to the crew the plan now holds;
        // agent 0.3.6: also when the old plan held the job to fewer workers than the new start has - the PC's second 0.3.5
        // lease measured a one-worker candidate first and stayed with one worker of 1,024, its crew never measured)
        if t.gpu {
            let (nvidia, batch) = t.gpu_start;
            let s = t.space.gpu_start(nvidia, batch);
            let fresh = t.entry.incumbent == t.entry.start && t.entry.state == tune::State::Searching && t.entry.trials == 0;
            let held_back = old.workers_max < t.space.workers_max && t.entry.wanted(&t.space).workers < s.workers;
            if (fresh || held_back) && s != t.entry.wanted(&t.space) {
                t.entry.start = s;
                t.entry.incumbent = s;
                if held_back {
                    // (a search under the old plan: searched again from the new start, its measurements kept)
                    (t.entry.state, t.entry.trials, t.entry.momentum) = (tune::State::Searching, 0, None);
                }
            }
        }
        let wanted = t.entry.wanted(&t.space);
        let line = format!("planning {}: the layout wanted is {}", changed.join(" and "), wanted.describe_on(t.gpu));
        log::info!("lease {id}: {line}");
        eng.worker_log.write_line(&format!("tuning [{id}]: {line}"));
        self.save_tuning(eng);
        self.show_tuning(eng);
    }

    /// agent 0.3.3: a measured cost per game (MB over the base) - remembered for the run when higher (`Learned`) and
    /// planned with for the rest of the lease: the crew's next members, handoffs and restarts, and the tuner's memory
    /// bounds (0.3.2 remembered it for the next lease only). -> the cost planned with from now on.
    fn learn_cost(&mut self, eng: &Engine, per: u64) -> crate::rolling::MemCost {
        let id = self.lease.lease_id.clone();
        let (run, build) = (self.lease.run.clone().unwrap_or_default(), self.lease.build.build_id.clone());
        let Some(r) = self.roll.as_mut() else { return crate::rolling::cpu_cost() };
        r.raised = r.raised.max(per);
        if Learned::raise(&eng.cfg.layout, &run, &build, r.gpu, per) {
            log::info!("lease {id}: remembering {per} MB per game in flight for this run");
        }
        r.mem.cost.per_game_mb = r.mem.cost.per_game_mb.max(per);
        r.mem.measured = true;
        let cost = r.mem.cost;
        if let Some(t) = self.tuning.as_mut() {
            if let Some((limit, _)) = t.space.mem {
                t.space.mem = Some((limit, cost));
            }
        }
        cost
    }

    /// The crew runs at most `n` workers for the rest of the lease (a member failed with no restart left).
    fn cap_crew(&mut self, n: u32) {
        if let Some(t) = self.tuning.as_mut() {
            t.crew_cap = t.crew_cap.min(n.max(1));
            log::warn!("lease {}: no restart left: the crew runs at most {} workers", self.lease.lease_id, t.crew_cap);
        }
    }

    /// rev 19 (PROTOCOL 13.5): the main worker failed (not of memory: a fatal error or an exit without `end`) while
    /// crew members play their ranges: it is replaced (a new tail after every released id; its games in flight lost),
    /// reported once per lease, and the others go on. Agent 0.3.15: with `lone`, a lone rolling worker too (one that
    /// failed its resume or plays nothing, `check_progress`: a fresh worker of the same build builds its sessions anew).
    /// -> true when a restart is due (else the lease fails as before).
    fn crew_crash(&mut self, eng: &mut Engine, why: &str, code: Option<i64>, lone: bool) -> bool {
        let stopping = matches!(self.state, WState::Stopping(_)) || self.after.is_some() || eng.quitting();
        let crew = self.mates.iter().any(|m| m.active());
        let layout = self.tuning.as_ref().map_or(self.main_layout(), |t| t.current);
        let selftest_failed = self.selftest_failed;
        let Some(r) = self.roll.as_mut().filter(|r| r.active && !r.finishing && r.restart.is_none()) else { return false };
        if !(crew || lone) || stopping || selftest_failed || r.restarts >= crate::rolling::MAX_RESTARTS {
            return false;
        }
        r.restart = Some(Restart { conc: layout.conc, burn: true, layout: Some(layout), count: true, measure: false });
        if crew {
            log::warn!("lease {}: the crew's main worker failed ({why}); starting another, the others go on", self.lease.lease_id);
        } else {
            log::warn!("lease {}: the worker failed ({why}); starting another", self.lease.lease_id);
        }
        if self.tuning.as_ref().is_some_and(|t| t.probe.is_some()) {
            self.end_probe(eng, Err("the crew's main worker failed during the trial".into()));
        }
        if !self.crash_reported {
            let kind = if self.worker.stderr_tail().contains("panicked at") { "panic" } else { "crash" };
            let msg = if crew { format!("crew main worker failed: {why}; replaced (the other workers go on)") } else { format!("{why}; started again") };
            self.report(eng, kind, &msg, code);
        }
        true
    }

    /// Workers beside the main one are stopped while work is paused (all of them, with the main worker) and the finishing
    /// ones while the next crew is measured; any is killed when silent too long (or with no `hello` in time); a crew
    /// member silent past `ping_after` is pinged.
    fn mates_tick(&mut self, eng: &Engine, t: &crate::Timings, must_pause: bool) {
        let measuring = self.tuning.as_ref().is_some_and(|x| x.probe.is_some()) && self.crew_complete();
        let id = self.lease.lease_id.clone();
        let gpu = self.gpu_lease();
        // (agent 0.3.15: the PC gives the workers no CPU - the main worker's sample says so: no member's stall)
        let starved = self.stall_since.is_some();
        for d in &mut self.mates {
            let stop = must_pause || (d.finishing && measuring);
            d.want_stop = stop;
            if d.finishing && d.finishing_since.is_none() {
                d.finishing_since = Some(Instant::now());
            }
            // agent 0.3.4: a GPU job's worker gives its video memory back for a pause first, as the main worker does
            // (`yield`; stopped once it said `yielded`, killed when it doesn't within `yield_answer`) - a stopped worker
            // would hold it while another program needs the GPU
            if stop && gpu && must_pause && d.job_sent && !d.yielded && d.end_ev.is_none() {
                match d.yield_at {
                    None => {
                        let _ = d.worker.send(&json!({"op": "yield"}));
                        d.yield_at = Some(Instant::now());
                    }
                    Some(at) if at.elapsed() > t.yield_answer => {
                        log::warn!("lease {id}: worker pid {} did not answer yield within {} s: killing it", d.worker.pid, t.yield_answer.as_secs());
                        d.fatal.get_or_insert_with(|| format!("did not answer yield within {} s", t.yield_answer.as_secs()));
                        d.yield_at = None;
                        d.worker.kill();
                    }
                    Some(_) => {}
                }
                continue;
            }
            if stop && !d.worker.is_suspended() {
                d.worker.suspend();
            } else if !stop && d.worker.is_suspended() {
                d.worker.resume();
                d.last_msg = Instant::now();
                d.watch.restart_clock();
            }
            if !stop && (d.yielded || d.yield_at.is_some()) {
                // (its session is built again: `resume`; a `yield` the pause outlived is answered by it too)
                let _ = d.worker.send(&json!({"op": "resume"}));
                (d.yielded, d.yield_at) = (false, None);
                d.last_msg = Instant::now();
                // (agent 0.3.15: watched from the resume on, its `resumed` awaited)
                d.resuming = Some(Instant::now());
                d.watch.restart_clock();
            }
            if d.worker.is_suspended() {
                continue;
            }
            if starved {
                d.watch.restart_clock();
            }
            let quiet = d.last_msg.elapsed();
            let failed = if d.job_sent && d.fatal.is_none() { d.watch.verdict(Instant::now(), t.resume_fail_after, t.stall_after, d.resuming.is_some(), starved) } else { None };
            if !d.job_sent && !d.finishing && d.spawned.elapsed() > t.hello {
                log::warn!("lease {id}: worker pid {} sent no hello within {} s: killing it", d.worker.pid, t.hello.as_secs());
                d.fatal = Some(format!("no hello within {} s", t.hello.as_secs()));
                d.worker.kill();
            } else if quiet > t.hang_after {
                log::warn!("lease {id}: worker pid {} was silent for {} s: killing it", d.worker.pid, quiet.as_secs());
                d.fatal.get_or_insert_with(|| format!("silent for {} s", quiet.as_secs()));
                d.worker.kill();
                d.last_msg = Instant::now();
            } else if let Some(why) = failed {
                // agent 0.3.15 (`Watch`): it failed - its exit takes the paths of any member's (`mate_failed`: out of video
                // memory, the plan per row raised; a replacement joins while restarts are left)
                log::warn!("lease {id}: worker pid {} failed: {why}: killing it", d.worker.pid);
                eng.worker_log.write_line(&format!("watch [{id}]: worker pid {} failed: {why}", d.worker.pid));
                d.fatal = Some(why);
                d.worker.kill();
            } else if d.active() && d.job_sent && quiet > t.ping_after && !d.pinged {
                d.pinged = true;
                let _ = d.worker.send(&json!({"op": "ping"}));
            }
        }
    }

    /// Every worker beside the main one is told to quit (finished games flushed, then `end`), once.
    fn quit_mates(&mut self) {
        for d in &mut self.mates {
            if d.worker.is_suspended() {
                d.worker.resume();
            }
            if d.end_ev.is_none() {
                let _ = d.worker.send(&json!({"op": "quit"}));
            }
            d.finishing = true;
            d.last_msg = Instant::now();
        }
    }

    /// The main worker is gone and workers beside it still finish their games (or play the rest of their ranges):
    /// beats, pauses, quits and the slack as for any lease.
    fn wind_down(&mut self, eng: &mut Engine) {
        let t = eng.cfg.timings.clone();
        eng.maybe_send_saved_crashes();
        if eng.quitting() && !matches!(self.after, Some(After::Quit)) {
            self.after = Some(After::Quit);
            self.quit_mates();
        }
        let d = eng.decide(&self.worker_pids());
        // (agent 0.3.4: a GPU job's workers pause too when GPU work stops being allowed)
        let pause = !d.work || (self.gpu_lease() && !d.gpu);
        if pause {
            let since = *self.stall_since.get_or_insert(Instant::now());
            if since.elapsed() > Duration::from_secs(self.lease.slack_s) && self.after.is_none() {
                log::info!("lease {}: paused longer than slack ({} s); giving it back", self.id(), self.lease.slack_s);
                self.after = Some(After::GiveBack("paused longer than slack".into()));
                self.quit_mates();
            }
        } else {
            self.stall_since = None;
        }
        self.mates_tick(eng, &t, pause && self.after.is_none());
        self.retire_mates(eng, &t);
        self.tail_tick(eng, &t, pause);
        self.maybe_beat(eng, pause);
    }

    /// rev 19 (PROTOCOL 13.5), each tick: the finishing workers' reads, the members that started all of their ids
    /// retired, the crew's memory, and a new member when the crew is short of the layout's workers.
    fn crew_tick(&mut self, eng: &mut Engine, t: &crate::Timings, must_pause: bool) {
        self.reclaim_tick(t);
        self.retire_mates(eng, t);
        if self.tuning.is_none() || self.mates.is_empty() && self.crew_target() <= 1 {
            return;
        }
        self.crew_mem_check(eng);
        self.add_member(eng, t, must_pause);
    }

    /// A worker told `finish` early (a handoff, its memory) is pinged until two of its `games_started` agree: the ids
    /// of its range from there were never started and go to the spare ranges (none read within `10 x tune_settle`: they
    /// go back with the lease's unfinished ids).
    fn reclaim_tick(&mut self, t: &crate::Timings) {
        let id = self.lease.lease_id.clone();
        let mut spare = Vec::new();
        for m in &mut self.mates {
            let Some(rc) = m.reclaim.as_mut() else { continue };
            if m.worker.is_suspended() {
                // (a stopped worker answers nothing: the reads start again once it runs)
                rc.since = Instant::now();
                rc.reads.clear();
                continue;
            }
            if rc.since.elapsed() < t.tune_settle {
                continue;
            }
            if rc.last_ping.is_none_or(|p| p.elapsed() >= t.tune_settle) {
                rc.last_ping = Some(Instant::now());
                let _ = m.worker.send(&json!({"op": "ping"}));
            }
            let n = rc.reads.len();
            if n >= 2 && rc.reads[n - 1] == rc.reads[n - 2] {
                let first = (m.offset + rc.reads[n - 1]).min(m.end);
                if first < m.end {
                    log::info!("lease {id}: worker pid {} never started ids [{first}, {}) of its range (relative): spare for the next workers", m.worker.pid, m.end);
                    spare.push((first, m.end));
                }
                m.reclaim = None;
            } else if rc.since.elapsed() > t.tune_settle * 10 {
                log::info!("lease {id}: worker pid {} didn't say where its ids stop: its unstarted ids go back with the lease", m.worker.pid);
                m.reclaim = None;
            }
        }
        self.spare.extend(spare);
    }

    /// Crew members that started every id of their range finish their games (`finish`); the crew is refilled by
    /// `add_member`. Near the end of its range a member is pinged for fresh counters.
    fn retire_mates(&mut self, eng: &Engine, t: &crate::Timings) {
        let id = self.lease.lease_id.clone();
        let mut changed = false;
        for m in self.mates.iter_mut().filter(|m| m.active() && m.job_sent) {
            let Some((_, _, started)) = m.counters else { continue };
            let len = m.end - m.offset;
            if started >= len {
                let _ = m.worker.send(&json!({"op": "finish"}));
                m.finishing = true;
                changed = true;
                let line = format!("worker pid {} started every id of its range ({len} games from its first): it finishes its games, a new worker takes its place", m.worker.pid);
                log::info!("lease {id}: {line}");
                eng.worker_log.write_line(&format!("crew [{id}]: {line}"));
            } else if len - started < m.layout.conc.max(1) && !m.worker.is_suspended() && m.pinged_at.is_none_or(|p| p.elapsed() >= t.tune_ping) {
                m.pinged_at = Some(Instant::now());
                let _ = m.worker.send(&json!({"op": "ping"}));
            }
        }
        if changed {
            self.crew_changed = Instant::now();
            self.show_tuning(eng);
        }
    }

    /// agent 0.3.4 (PROTOCOL 10.6, 13.5): a GPU job's video memory, read at most once a second - each worker's (Windows'
    /// counters, else its `status` `vram_mb`; none while it is paused) and the adapter's in use (every program's). A
    /// worker holding more per batch row than planned raises the plan at once (remembered per run and worker build: the
    /// crew's next workers, handoffs and the tuner's bounds plan with it); steady readings lower it when the lease ends.
    /// When the adapter's memory in use reaches `VRAM_HIGH_SHARE`, one worker gives way while no drain is in progress: a
    /// crew's biggest finishes its games (the next one joins only when the video memory allows), a lone worker is started
    /// again with fewer games in flight (a memory restart).
    fn vram_check(&mut self, eng: &Engine) {
        let Some(v) = self.roll.as_ref().filter(|r| r.active).and_then(|r| r.vram.clone()) else { return };
        if v.read_at.is_some_and(|t| t.elapsed() < Duration::from_secs(1)) {
            return;
        }
        let pids = self.worker_pids();
        let m = eng.signals.gpu_memory(v.luid, &pids);
        let of = |pid: u32| m.as_ref().map_or(0, |m| m.of(pid));
        let paused = matches!(self.state, WState::Paused(_) | WState::Yielding(_));
        let main_live = self.main_exit.is_none() && self.job_sent && !paused;
        self.vram_mb = if self.main_exit.is_none() { of(self.worker.pid).max(if paused { 0 } else { self.stats_vram }) } else { 0 };
        let mut pdh_ours = if self.main_exit.is_none() { of(self.worker.pid) } else { 0 };
        for d in &mut self.mates {
            let held = d.yielded || d.yield_at.is_some();
            d.vram_mb = of(d.worker.pid).max(if held { 0 } else { d.stats_vram });
            pdh_ours += of(d.worker.pid);
        }
        let ours = self.vram_mb + self.mates.iter().map(|d| d.vram_mb).sum::<u64>();
        // (Windows' view of our workers replaced by the more of it and theirs)
        let adapter = m.as_ref().map_or(v.others_mb, |m| m.adapter_used_mb.saturating_sub(pdh_ours)) + ours;
        // what each worker playing new games holds per row of its batch over the base, and whether it is at its steady state
        let conc = self.conc.unwrap_or(1);
        let main_steady = self.progress.is_some_and(|(g, st)| crate::rolling::steady_reading(g, st, conc));
        let mut readings: Vec<(u64, u64, bool)> = Vec::new();
        if main_live && self.vram_mb > 0 {
            readings.push((self.vram_mb, self.main_layout().batch(), main_steady));
        }
        for d in self.mates.iter().filter(|d| d.active() && d.job_sent && d.vram_mb > 0) {
            readings.push((d.vram_mb, d.layout.batch(), d.counters.is_some_and(|(_, g, st)| crate::rolling::steady_reading(g, st, d.layout.conc))));
        }
        let (run, build) = (self.lease.run.clone().unwrap_or_default(), self.lease.build.build_id.clone());
        let id = self.lease.lease_id.clone();
        let mut raised = None;
        if let Some(x) = self.roll.as_mut().and_then(|r| r.vram.as_mut()) {
            x.read_at = Some(Instant::now());
            x.adapter_used_mb = adapter;
            for &(mb, rows, steady) in &readings {
                let per = mb.saturating_sub(x.cost.base_mb).div_ceil(rows.max(1));
                x.per_row_max = x.per_row_max.max(per);
                if steady {
                    x.steady_per_row = x.steady_per_row.max(per);
                }
                if per > x.cost.per_row_mb && per > x.raised {
                    x.raised = per;
                    x.cost.per_row_mb = per;
                    raised = Some((per, mb, rows, x.budget_mb, x.cost));
                }
            }
        }
        if let Some((per, mb, rows, budget, cost)) = raised {
            if Learned::vram_raise(&eng.cfg.layout, &run, &build, per) {
                log::info!("lease {id}: a worker holds {mb} MB of video memory with batches of {rows} rows, {per} MB per row - more than planned: remembering it for this run and worker build");
            }
            if let Some(t) = self.tuning.as_mut() {
                t.space.vram = Some((budget, cost));
            }
        }
        // the adapter nearly full: one worker gives way (one drain at a time)
        let busy = !main_live || self.after.is_some() || self.draining() || self.roll.as_ref().is_none_or(|r| r.finishing || r.restart.is_some());
        if busy || (adapter as f64) < v.total_mb as f64 * crate::rolling::VRAM_HIGH_SHARE {
            return;
        }
        let line = format!("the GPU's dedicated memory is {adapter} MB of {} MB in use (the lease's workers {ours} MB)", v.total_mb);
        log::warn!("lease {id}: {line}");
        eng.worker_log.write_line(&format!("crew [{id}]: {line}"));
        let biggest = self.mates.iter().enumerate().filter(|(_, d)| d.active() && d.job_sent).max_by_key(|(_, d)| d.vram_mb).map(|(i, d)| (i, d.vram_mb));
        match biggest {
            Some((i, mb)) if mb >= self.vram_mb => self.mate_give_way(eng, i, mb),
            Some(_) => self.primary_give_way(eng, self.vram_mb),
            None => {
                let limit = self.worker.mem_limit_mb;
                let Some(r) = self.roll.as_mut() else { return };
                let cost = r.mem.cost;
                match crate::rolling::restart_games(conc, if limit == 0 { u64::MAX >> 24 } else { limit }, cost).filter(|_| r.restarts < crate::rolling::MAX_RESTARTS) {
                    Some(n) => {
                        log::warn!("lease {id}: finishing the {conc} games in flight, then going on with {n} (the GPU's memory)");
                        r.restart = Some(Restart { conc: n, burn: false, layout: None, count: true, measure: false });
                        self.send(json!({"op": "finish"}));
                    }
                    None => self.roll_finish("the GPU's memory is full and the worker can't run fewer games"),
                }
            }
        }
    }

    /// agent 0.3.4: a GPU job's worker failed with `why` (its fatal error or its exit; `tail` its stderr, `peak` /
    /// `limit` its job's memory): did it run out of video memory? Its message says so (`rolling::gpu_oom`), or it failed
    /// to allocate far from its memory limit (ONNX Runtime's CUDA arena says only "Failed to allocate memory").
    fn vram_failed(&self, why: &str, tail: &str, peak: u64, limit: u64) -> bool {
        if self.roll.as_ref().is_none_or(|r| r.vram.is_none()) {
            return false;
        }
        let text = format!("{why}\n{tail}");
        crate::rolling::gpu_oom(&text) || (alloc_failure(&text) && (limit == 0 || (peak as f64) < limit as f64 * 0.9))
    }

    /// agent 0.3.4: a worker batching `rows` rows (holding `held` MB when last read) ran out of video memory: the plan per
    /// row is raised - to what it held, at least a quarter more than planned - and remembered for the run and worker build.
    fn learn_vram_oom(&mut self, eng: &Engine, rows: u64, held: u64) {
        let id = self.lease.lease_id.clone();
        let (run, build) = (self.lease.run.clone().unwrap_or_default(), self.lease.build.build_id.clone());
        let Some(v) = self.roll.as_mut().and_then(|r| r.vram.as_mut()) else { return };
        let per = held.saturating_sub(v.cost.base_mb).div_ceil(rows.max(1)).max((v.cost.per_row_mb * 5).div_ceil(4)).max(v.cost.per_row_mb + 1);
        v.cost.per_row_mb = per;
        v.raised = v.raised.max(per);
        let (budget, cost) = (v.budget_mb, v.cost);
        if Learned::vram_raise(&eng.cfg.layout, &run, &build, per) {
            log::info!("lease {id}: a worker with batches of {rows} rows ran out of video memory: planning {per} MB per row for this run and worker build");
        }
        if let Some(t) = self.tuning.as_mut() {
            t.space.vram = Some((budget, cost));
        }
    }

    /// agent 0.3.6 (PROTOCOL 13.2, 13.5): a worker finishing its games whose memory the next worker waits for - a crew's
    /// next member, a restart, a handoff's next worker - ends its last games (`quit`) once it has finished for
    /// `Timings::drain_tail` or only an eighth of its games in flight (at most 4) are left: their ids go back with the lease
    /// (`done`'s unfinished ids are worked out from the credited ones), none is played twice. A drain ends in its longest
    /// games at a few rows a batch: the 4070 Ti PC's start drained for 303 s beside the first member of its crew, the second
    /// waiting for its memory all along.
    fn tail_tick(&mut self, eng: &mut Engine, t: &crate::Timings, must_pause: bool) {
        if must_pause || self.after.is_some() || eng.quitting() || !self.roll.as_ref().is_some_and(|r| r.active && !r.finishing) {
            return;
        }
        let id = self.lease.lease_id.clone();
        let small = |n: Option<u64>, conc: u64| n.is_some_and(|n| n <= (conc / 8).max(4));
        // the main worker finishing for a handoff whose next worker hasn't started yet, or for a restart
        let handoff_waits = self.tuning.as_ref().and_then(|tu| tu.handoff.as_ref()).is_some_and(|h| h.since.elapsed() >= t.tune_settle * 10);
        let restart_runs = self.main_exit.is_none() && self.restart_pending();
        if self.main_exit.is_none() && !self.tail_quit && (handoff_waits || restart_runs) {
            let in_flight = self.progress.map(|(g, st)| st.saturating_sub(g));
            let long = self.finish_sent.is_some_and(|at| at.elapsed() >= t.drain_tail);
            if long || small(in_flight, self.conc.unwrap_or(1)) {
                let line = format!("worker pid {} ends its last {} games now: the next worker waits for its memory (their ids go back with the lease)", self.worker.pid, in_flight.map_or("?".into(), |n| n.to_string()));
                log::info!("lease {id}: {line}");
                eng.worker_log.write_line(&format!("crew [{id}]: {line}"));
                self.tail_quit = true;
                self.send(json!({"op": "quit"}));
            }
        }
        // workers beside it finishing their games while a crew's member, a restart or a handoff waits for room - room their
        // memory is what is missing (not other programs' video memory, say)
        let next = match (self.restart_pending(), self.tuning.as_ref()) {
            (true, _) => self.roll.as_ref().and_then(|r| r.restart.as_ref()).map(|rs| rs.layout.map(|l| l.per_worker()).unwrap_or(Layout { conc: rs.conc, ..self.main_layout() })),
            (false, Some(tu)) => Some(tu.handoff.as_ref().map_or(tu.current, |h| h.next).per_worker()),
            (false, None) => None,
        };
        let Some(next) = next else { return };
        let crew_waits = self.main_exit.is_none() && matches!(self.state, WState::Running) && self.job_sent && !self.crew_complete() && !self.room_for(1, &next);
        let restart_waits = self.main_exit.is_some() && self.restart_pending() && !self.restart_room();
        if !(crew_waits || restart_waits || handoff_waits) || !self.room_without_drainers(&next) {
            return;
        }
        for m in self.mates.iter_mut().filter(|m| m.finishing && m.job_sent && !m.tail_quit && m.end_ev.is_none() && !m.worker.is_suspended()) {
            let long = m.finishing_since.is_some_and(|at| at.elapsed() >= t.drain_tail);
            if long || small(m.in_flight, m.layout.conc) {
                let line = format!("worker pid {} ends its last {} games now: the next worker waits for its memory (their ids go back with the lease)", m.worker.pid, m.in_flight.map_or("?".into(), |n| n.to_string()));
                log::info!("lease {id}: {line}");
                eng.worker_log.write_line(&format!("crew [{id}]: {line}"));
                m.tail_quit = true;
                let _ = m.worker.send(&json!({"op": "quit"}));
            }
        }
    }

    /// agent 0.3.6: one worker of layout `l` would fit if the workers finishing their games were gone: what the others hold
    /// or will hold (`reserved_mb`, `vram_reserved`) less the drainers', within `PLAN_SHARE` of the job's limit and the video
    /// memory budget.
    fn room_without_drainers(&mut self, l: &Layout) -> bool {
        let limit = self.lease_limit_mb;
        let drain_mb: u64 = self.mates.iter_mut().filter(|m| m.finishing).map(|m| m.worker.commit_mb().unwrap_or(0)).sum();
        let host = limit == 0 || (self.reserved_mb(true).saturating_sub(drain_mb) + self.plan_mb(l.conc)) as f64 <= limit as f64 * crate::rolling::PLAN_SHARE;
        let vram = match self.roll.as_ref().and_then(|r| r.vram.as_ref()) {
            None => true,
            Some(v) => {
                let ours = self.vram_mb + self.mates.iter().map(|m| m.vram_mb).sum::<u64>();
                let drain_vram: u64 = self.mates.iter().filter(|m| m.finishing).map(|m| m.vram_mb).sum();
                let budget = v.budget_mb.min(crate::rolling::vram_budget(v.total_mb, v.adapter_used_mb.saturating_sub(ours)));
                self.vram_reserved(true).saturating_sub(drain_vram) + v.cost.plan_mb(l.batch()) <= budget
            }
        };
        host && vram
    }

    /// The crew's memory (PROTOCOL 13.5): each member within its part of the job's limit (`HIGH_SHARE` of it: it
    /// finishes its games, the next members run fewer), and every worker of the lease - those finishing their games too
    /// (agent 0.3.3; 0.3.2 looked away while any finished) - within the job's limit: at `HIGH_SHARE` of it in all, the
    /// biggest worker playing new games gives way - a memory restart's way when it commits more per game than planned
    /// (the cost remembered, the next members smaller), else it just starts no new game and the crew refills as the
    /// drain makes room (`add_member`, `restart_room`; no restart counted, nothing shrunk). Once nothing plays new games,
    /// new games have stopped until memory frees.
    fn crew_mem_check(&mut self, eng: &Engine) {
        if self.tuning.as_ref().is_some_and(|t| t.handoff.is_some()) || self.after.is_some() {
            return;
        }
        let Some(cost) = self.roll.as_ref().filter(|r| r.active && !r.finishing && r.restart.is_none()).map(|r| r.mem.cost) else { return };
        let main_on = matches!(self.state, WState::Running | WState::Resuming(_)) && self.main_exit.is_none();
        let main_mb = if self.main_exit.is_none() { self.worker.commit_mb().unwrap_or(0) } else { 0 };
        let mut sum = main_mb;
        let mut high = None;
        let mut biggest: Option<(usize, u64)> = None;
        let (mut per_max, mut steady) = (0, 0);
        for (i, m) in self.mates.iter_mut().enumerate() {
            let Some(c) = m.worker.commit_mb() else { continue };
            m.peak_mb = m.peak_mb.max(c);
            sum += c;
            if !m.active() || !m.job_sent {
                continue;
            }
            let per = crate::rolling::measured_per_game(c, m.layout.conc, cost);
            per_max = per_max.max(per);
            if m.counters.is_some_and(|(_, g, s)| crate::rolling::steady_reading(g, s, m.layout.conc)) {
                steady = steady.max(crate::rolling::measured_per_game_kb(c, m.layout.conc, cost));
            }
            let lim = m.worker.mem_limit_mb;
            if high.is_none() && lim > 0 && c as f64 >= lim as f64 * crate::rolling::HIGH_SHARE {
                high = Some((i, c));
            }
            if biggest.is_none_or(|(_, b)| c > b) {
                biggest = Some((i, c));
            }
        }
        if let Some(r) = self.roll.as_mut() {
            r.per_game_max = r.per_game_max.max(per_max);
            r.steady_kb = r.steady_kb.max(steady);
        }
        if let Some((i, c)) = high {
            return self.mate_memory_high(eng, i, c);
        }
        let limit = self.lease_limit_mb;
        if limit == 0 || (sum as f64) < limit as f64 * crate::rolling::HIGH_SHARE {
            return;
        }
        let finishing = self.mates.iter().filter(|m| m.finishing).count();
        let over = |c: u64, conc: u64| crate::rolling::measured_per_game(c, conc, cost) > cost.per_game_mb;
        let main_conc = self.conc.unwrap_or(1);
        match biggest {
            Some((i, c)) if !main_on || c >= main_mb => {
                log::warn!("lease {}: its workers commit {sum} MB of the job's {limit} MB limit ({finishing} finishing their games)", self.lease.lease_id);
                if over(c, self.mates[i].layout.conc) {
                    self.mate_memory_high(eng, i, c);
                } else {
                    self.mate_give_way(eng, i, c);
                }
            }
            _ if main_on => {
                log::warn!("lease {}: its workers commit {sum} MB of the job's {limit} MB limit ({finishing} finishing their games)", self.lease.lease_id);
                if over(main_mb, main_conc) {
                    self.primary_memory_high(eng, main_mb);
                } else {
                    self.primary_give_way(eng, main_mb);
                }
            }
            _ => {}
        }
    }

    /// agent 0.3.3: a crew member within its plan gives way to the memory the workers finishing their games still hold:
    /// it starts no new game (its unstarted ids become spare) and a new member joins once the drain made room.
    fn mate_give_way(&mut self, eng: &Engine, i: usize, now: u64) {
        let id = self.lease.lease_id.clone();
        let m = &mut self.mates[i];
        let line = format!("worker pid {} ({now} MB) starts no new game until the workers finishing theirs made room", m.worker.pid);
        log::warn!("lease {id}: {line}");
        eng.worker_log.write_line(&format!("crew [{id}]: {line}"));
        let _ = m.worker.send(&json!({"op": "finish"}));
        m.finishing = true;
        m.reclaim = Some(Reclaim { since: Instant::now(), reads: Vec::new(), last_ping: None });
        self.crew_changed = Instant::now();
    }

    /// agent 0.3.3: the main worker within its plan gives way the same way: it finishes its games and the next one, with
    /// the same games in flight and the released ids it never started, starts once the drain made room (`restart_room`;
    /// no restart counted).
    fn primary_give_way(&mut self, eng: &Engine, now: u64) {
        let id = self.lease.lease_id.clone();
        let conc = self.conc.unwrap_or(1);
        let Some(r) = self.roll.as_mut().filter(|r| r.restart.is_none() && !r.finishing) else { return };
        r.restart = Some(Restart { conc, burn: false, layout: None, count: false, measure: false });
        r.want_more = false;
        let line = format!("the main worker ({now} MB) starts no new game until the workers finishing theirs made room");
        log::warn!("lease {id}: {line}");
        eng.worker_log.write_line(&format!("crew [{id}]: {line}"));
        self.send(json!({"op": "finish"}));
        self.crew_changed = Instant::now();
    }

    /// A crew member near its memory limit (`now` MB): it finishes its games (the ids it never started become spare),
    /// the cost per game is remembered, and the next members run fewer games (while restarts are left; then the crew
    /// runs without it).
    fn mate_memory_high(&mut self, eng: &Engine, i: usize, now: u64) {
        let id = self.lease.lease_id.clone();
        let Some(base) = self.roll.as_ref().map(|r| r.mem.cost) else { return };
        let (conc, limit) = (self.mates[i].layout.conc, self.mates[i].worker.mem_limit_mb);
        let per = crate::rolling::measured_per_game(now, conc, base);
        let cost = self.learn_cost(eng, per);
        let Some(r) = self.roll.as_mut() else { return };
        let m = &mut self.mates[i];
        log::warn!("lease {id}: worker pid {} commits {now} MB of its {limit} MB limit with {conc} games in flight; it finishes them", m.worker.pid);
        let _ = m.worker.send(&json!({"op": "finish"}));
        m.finishing = true;
        m.reclaim = Some(Reclaim { since: Instant::now(), reads: Vec::new(), last_ping: None });
        let others = self.mates.iter().filter(|m| m.active()).count() + 1;
        match crate::rolling::restart_games(conc, limit, cost).filter(|_| r.restarts < crate::rolling::MAX_RESTARTS) {
            Some(n) => {
                r.restarts += 1;
                self.shrink_members(n);
            }
            None => self.cap_crew(others as u32),
        }
        self.crew_changed = Instant::now();
    }

    /// A new member when the crew is short of the layout's workers (at the lease's start, after a retirement or a
    /// failure), one at a time, while work runs and memory allows (`room_for`: what every worker holds or will hold -
    /// workers playing new games at least their plan, drainers their commit - plus its plan within `PLAN_SHARE` of the
    /// job's limit; agent 0.3.3): it plays a spare range that fills its games in flight, else it takes the tail's
    /// place - the main worker keeps its range (at least `RUNWAY_INFLIGHTS` x its games in flight unstarted; else more
    /// ids are asked for first) and the new worker gets the ids released from then on.
    fn add_member(&mut self, eng: &mut Engine, t: &crate::Timings, must_pause: bool) {
        let Some(tu) = self.tuning.as_ref() else { return };
        if tu.handoff.is_some() || must_pause || self.after.is_some() || eng.quitting() || self.main_exit.is_some() {
            return;
        }
        if !matches!(self.state, WState::Running) || !self.job_sent {
            return;
        }
        let Some(r) = self.roll.as_ref().filter(|r| r.active && !r.finishing && r.restart.is_none()) else { return };
        if self.crew_complete() || self.mates.iter().any(|m| m.active() && !m.job_sent) {
            return;
        }
        let per = tu.current.per_worker();
        let (released, offset) = (r.released, r.offset);
        let limit = self.worker_limit();
        // (agent 0.3.3: beside what every worker holds or will hold - a fresh one at its plan, a drainer at its commit;
        // agent 0.3.4: a GPU job's video memory too)
        if !self.room_for(1, &per) {
            return;
        }
        // a spare range that holds RUNWAY_INFLIGHTS x its games in flight (a shorter one goes back with the lease)
        if let Some(k) = self.spare.iter().position(|&(a, b)| b - a >= RUNWAY_INFLIGHTS * per.conc.max(1)) {
            let range = self.spare.remove(k);
            return self.spawn_mate(eng, per, range, limit);
        }
        // else the tail's place: fresh counters of the main worker first
        let fresh = t.tune_ping * 3;
        let Some((_, _, started)) = self.counters.filter(|_| self.counters_at.is_some_and(|at| at.elapsed() <= fresh)) else {
            if self.last_counter_ping.is_none_or(|p| p.elapsed() >= t.tune_ping) {
                self.last_counter_ping = Some(Instant::now());
                self.send(json!({"op": "ping"}));
            }
            return;
        };
        let unstarted = (released - offset).saturating_sub(started);
        if unstarted < RUNWAY_INFLIGHTS * self.conc.unwrap_or(1).max(1) {
            // (more ids first: the main worker would be left with a short range)
            if let Some(r) = self.roll.as_mut().filter(|r| !r.extend_inflight) {
                r.want_more = true;
            }
            return;
        }
        self.spawn_tail(eng, per, limit);
    }

    /// A new member of the crew on a spare range `[a, b)` (relative), closed: its job line goes out at its `hello`.
    fn spawn_mate(&mut self, eng: &mut Engine, per: Layout, (a, b): (u64, u64), limit: u64) {
        let id = self.lease.lease_id.clone();
        let lo = self.roll.as_ref().map_or(0, |r| r.setup.lo);
        let dir = format!("out-w{}", self.spawns + 1);
        let gen = self.next_gen();
        let w = match std::fs::create_dir_all(self.work.join(&dir)).and_then(|_| self.spawn.spawn(gen, self.tx.clone(), limit)) {
            Ok(w) => w,
            Err(e) => {
                log::error!("lease {id}: cannot start a worker of the crew: {e}");
                self.spare.push((a, b));
                let n = self.crew_active() as u32;
                self.cap_crew(n);
                return;
            }
        };
        let line = format!("worker pid {} joins the crew with ids [{}, {}) ({}, memory limit {limit} MB)", w.pid, lo + a, lo + b, self.desc(&per));
        log::info!("lease {id}: {line}");
        eng.worker_log.write_line(&format!("crew [{id}]: {line}"));
        self.spawns += 1;
        self.mates.push(Mate {
            worker: w,
            wgen: gen,
            out_dir: dir,
            end_ev: None,
            eof: false,
            last_msg: Instant::now(),
            in_flight: None,
            tail: None,
            finishing: false,
            offset: a,
            end: b,
            layout: per,
            job_sent: false,
            spawned: Instant::now(),
            pinged: false,
            pinged_at: None,
            counters: None,
            reclaim: None,
            gen_floor: self.prep.ckpt.as_ref().map(|(c, _)| c.gen).or(self.gen_floor),
            peak_mb: 0,
            fatal: None,
            yield_at: None,
            yielded: false,
            want_stop: false,
            vram_mb: 0,
            stats_vram: 0,
            finishing_since: None,
            tail_quit: false,
            watch: Watch::new(0),
            resuming: None,
        });
        self.crew_changed = Instant::now();
        self.show_tuning(eng);
        self.beat_soon();
    }

    /// A new member takes the tail's place: the main worker becomes a member keeping its range (every id released so
    /// far from its first), and the new worker - the main one from now on - starts at the end of the released ids with
    /// none (its `low` asks for them) and gets the `extend`s.
    fn spawn_tail(&mut self, eng: &mut Engine, per: Layout, limit: u64) {
        let id = self.lease.lease_id.clone();
        let Some((lo, released, offset, peak)) = self.roll.as_ref().map(|r| (r.setup.lo, r.released, r.offset, r.peak_mb)) else { return };
        let dir = format!("out-w{}", self.spawns + 1);
        let gen = self.next_gen();
        let w = match std::fs::create_dir_all(self.work.join(&dir)).and_then(|_| self.spawn.spawn(gen, self.tx.clone(), limit)) {
            Ok(w) => w,
            Err(e) => {
                log::error!("lease {id}: cannot start a worker of the crew: {e}");
                let n = self.crew_active() as u32;
                self.cap_crew(n);
                return;
            }
        };
        let line = format!(
            "worker pid {} joins the crew as its tail, ids from {} ({}, memory limit {limit} MB); pid {} keeps ids [{}, {})",
            w.pid,
            lo + released,
            self.desc(&per),
            self.worker.pid,
            lo + offset,
            lo + released
        );
        log::info!("lease {id}: {line}");
        eng.worker_log.write_line(&format!("crew [{id}]: {line}"));
        let old_layout = self.main_layout();
        let old_gen = std::mem::replace(&mut self.wgen, gen);
        self.spawns += 1;
        let old = std::mem::replace(&mut self.worker, w);
        let old_dir = std::mem::replace(&mut self.out_dir, dir);
        let counters = self.counters.take();
        self.mates.push(Mate {
            worker: old,
            wgen: old_gen,
            out_dir: old_dir,
            end_ev: self.end_ev.take(),
            eof: std::mem::take(&mut self.eof),
            last_msg: self.last_msg,
            in_flight: counters.map(|(_, g, s)| s.saturating_sub(g)),
            tail: None,
            finishing: false,
            offset,
            end: released,
            layout: old_layout,
            job_sent: true,
            spawned: self.started,
            pinged: self.pinged,
            pinged_at: None,
            counters,
            reclaim: None,
            gen_floor: self.gen_floor,
            peak_mb: peak,
            fatal: None,
            yield_at: None,
            yielded: false,
            want_stop: false,
            vram_mb: std::mem::take(&mut self.vram_mb),
            stats_vram: std::mem::take(&mut self.stats_vram),
            finishing_since: None,
            tail_quit: std::mem::take(&mut self.tail_quit),
            watch: Watch::new(0),
            resuming: None,
        });
        if let Some(r) = self.roll.as_mut() {
            r.offset = released;
            r.want_more = false;
            r.refused = false;
            r.next_extend = Instant::now();
            r.peak_mb = 0;
        }
        self.conc = Some(per.conc);
        self.groups = Some(per.groups);
        self.threads = per.threads;
        self.infer = self.infer_for(&per);
        self.batch_rows = per.batch();
        self.state = WState::Starting;
        self.started = Instant::now();
        self.last_msg = Instant::now();
        self.pinged = false;
        self.job_sent = false;
        self.killed = false;
        self.stall_since = None;
        self.counters_at = None;
        self.progress = None;
        (self.finish_sent, self.tail_quit) = (None, false);
        self.gen_floor = self.prep.ckpt.as_ref().map(|(c, _)| c.gen).or(self.gen_floor);
        self.crew_changed = Instant::now();
        self.mark = CpuMark::take(&self.worker);
        self.sample = (self.mark, Instant::now());
        self.show_tuning(eng);
        self.beat_soon();
    }
}

/// A fuzz failure's dedupe site when the worker gave none: the `file:line:col` after "panicked at"
/// in the panic message, else "unknown".
pub fn panic_site(panic: &str) -> String {
    if let Some(i) = panic.find("panicked at ") {
        let rest = &panic[i + 12..];
        let end = rest.find([' ', '\n', ',']).unwrap_or(rest.len());
        let site = rest[..end].trim_end_matches(':');
        if !site.is_empty() {
            return site.to_string();
        }
    }
    "unknown".into()
}

#[cfg(test)]
mod tests {
    #[test]
    fn panic_sites() {
        assert_eq!(super::panic_site("thread 'main' panicked at engine/src/x.rs:42:9:\nboom"), "engine/src/x.rs:42:9");
        assert_eq!(super::panic_site("index out of bounds"), "unknown");
    }

    /// agent 0.3.15: when a watched worker has failed (`Watch::verdict`).
    #[test]
    fn a_worker_that_plays_nothing_is_told_from_a_slow_one() {
        use super::Watch;
        use std::time::{Duration, Instant};
        let (fail, stall) = (Duration::from_secs(30), Duration::from_secs(300));
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        let mut w = Watch::new(7);
        w.moved = t0;
        assert_eq!(w.verdict(at(1000), fail, stall, false, false), None, "no counters yet: no games in flight known");
        w.seen = Some((5000, 10, 18));
        assert_eq!(w.verdict(at(299), fail, stall, false, false), None);
        let why = w.verdict(at(300), fail, stall, false, false).expect("5 minutes, 8 games in flight");
        assert!(why.contains("played nothing for 300 s with 8 games in flight"), "{why}");
        assert_eq!(w.verdict(at(900), fail, stall, false, true), None, "starved of CPU: no stall");
        w.seen = Some((5000, 18, 18));
        assert_eq!(w.verdict(at(900), fail, stall, false, false), None, "no games in flight: waiting for ids");
        assert!(w.verdict(at(900), fail, stall, true, false).is_some_and(|y| y.contains("after a resume")), "resuming: no `resumed` in 5 minutes");
        // counters that move start the clock again
        w.status(Some(5001), Some(18), Some(26));
        assert_eq!(w.verdict(Instant::now() + Duration::from_secs(299), fail, stall, false, false), None);
        // a failed resume: failed once nothing moved for `resume_fail_after` after it, starved or not
        let mut r = Watch::new(7);
        r.seen = Some((100, 1, 9));
        r.moved = t0;
        r.error("resume: CUBLAS_STATUS_ALLOC_FAILED", true);
        let e = r.resume_error.as_ref().unwrap().1;
        assert_eq!(r.verdict(e + Duration::from_secs(29), fail, stall, true, false), None);
        let why = r.verdict(e + Duration::from_secs(30), fail, stall, true, true).expect("failed resume");
        assert!(why.contains("its resume failed (resume: CUBLAS_STATUS_ALLOC_FAILED)"), "{why}");
        // ... unless it plays after all
        r.status(Some(150), Some(1), Some(9));
        assert!(r.resume_error.is_none());
        assert_eq!(r.verdict(Instant::now() + Duration::from_secs(60), fail, stall, true, false), None);
        // an error when not resuming is only remembered (told with a stall)
        let mut n = Watch::new(7);
        n.seen = Some((1, 0, 4));
        n.moved = t0;
        n.error("extend: games is required", false);
        assert!(n.resume_error.is_none());
        assert!(n.verdict(at(300), fail, stall, false, false).is_some_and(|y| y.ends_with("its last error: extend: games is required")));
    }
}

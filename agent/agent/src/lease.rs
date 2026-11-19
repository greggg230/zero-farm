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
use crate::rolling::{Learned, MemPlan, Setup};
use crate::status::Phase;
use crate::tune::{self, Layout, LoadMark, Probe, Verdict};
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
    /// rev 15 (PROTOCOL 10.6): a rolling lease's update check: the agent version now ready, the evict list.
    Update(Result<(Option<String>, Vec<String>), String>),
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
    /// The current worker's ids start at `lo + offset` (0 until a restart; a restart's worker gets the ids after
    /// the ones its predecessor started).
    offset: u64,
    /// Restarts for memory so far.
    restarts: u32,
    /// A restart for memory is due once the current worker is gone.
    restart: Option<Restart>,
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

/// rev 19 (PROTOCOL 13): a worker that finishes its games beside the current one after a layout handoff. It takes no
/// new games and no ids; its results are uploaded as any; the lease closes only after it ended.
struct Drainer {
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
}

/// rev 19 (PROTOCOL 13): the tuning of a rolling lease's CPU job.
struct Tuning {
    space: tune::Space,
    entry: tune::Entry,
    /// The main worker's layout.
    current: Layout,
    /// The main worker started fresh with `current` and is being measured.
    probe: Option<(Probe, Instant)>,
    handoff: Option<Handoff>,
    /// The other programs' load: the last CPU mark and their share of the CPUs since the one before.
    load: Option<(Instant, LoadMark)>,
    other_load: f64,
    last_ping: Instant,
    /// A trial was discarded: back to the incumbent first, and no trial before this.
    revert_due: bool,
    no_trial_before: Instant,
    /// The platform's starting point (whether every worker honours `infer_workers`).
    defaults: tune::Defaults,
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

/// At most this many workers finish their games beside the current one.
const MAX_DRAINERS: usize = 2;

/// What a worker is started with (kept: a rolling lease's restart starts another).
struct SpawnSpec {
    exe: PathBuf,
    cwd: PathBuf,
    mem_limit_mb: u64,
    cpu_job: bool,
    log: Arc<zerofarm_common::rotlog::RotatingFile>,
    tag: String,
    runtime_dir: Option<PathBuf>,
}

impl SpawnSpec {
    /// Starts the worker; its lines reach `tx` as generation `gen`'s.
    fn spawn(&self, gen: u32, tx: Sender<Ev>) -> std::io::Result<WorkerProc> {
        WorkerProc::spawn(
            SpawnOpts {
                exe: &self.exe,
                args: &["serve"],
                cwd: &self.cwd,
                mem_limit_mb: self.mem_limit_mb,
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

    /// The agent's guard (rev 11, PROTOCOL 9.5), before anything but manifests is downloaded: a lease whose runtime is
    /// an alternate, or whose pack declares a GPU requirement, needs a self-play or eval job, GPU work allowed now, an
    /// adapter that meets the pack's requirement, the pack not failed here, and room under the disk cap. Returns the
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
        if !matches!(lease.kind.as_str(), "selfplay" | "eval") {
            return no(format!("a {} lease can't use runtime pack {} (only self-play and evaluation get a GPU pack)", lease.kind, rt.build_id));
        }
        if !self.decide(None).gpu {
            return no(format!("runtime pack {} is for GPU work, which isn't allowed here right now", rt.build_id));
        }
        if let Some(n) = &need {
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
        }
        let probes = crate::probe::load(&self.cfg.layout);
        let akey = crate::probe::adapters_key(&self.profile.gpus);
        if crate::probe::pack_bad(&probes, &rt.build_id, &akey, need.as_ref().map(|n| n.providers.as_slice()), zerofarm_common::now_unix()) {
            return no(format!("runtime pack {} failed on this PC's GPU and driver before", rt.build_id));
        }
        if crate::probe::held(&crate::probe::load_unfinished(&self.cfg.layout), &rt.build_id, &akey, zerofarm_common::now_unix()) {
            return no(format!("runtime pack {} stopped this PC during a GPU check before (the agent or the PC went down)", rt.build_id));
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
    /// CPU, a GPU's from its bench row, both within the memory plans `mem` (CPU, GPU), PROTOCOL 10.6).
    #[allow(clippy::too_many_arguments)]
    fn pick(bench: Option<&crate::probe::Bench>, gpus: &[zerofarm_common::protocol::Gpu], lease: &Lease, plan: &Plan, gpu_ok: bool, mem: (MemPlan, MemPlan), roll: Option<&Setup>) -> crate::probe::Choice {
        let (games, conc, groups) = Self::job_shape(plan);
        let allow = lease.providers.as_deref();
        match roll {
            None => crate::probe::choose(bench, gpus, allow, gpu_ok, games, conc, groups),
            Some(r) => {
                let cpu = crate::rolling::cpu_shape(r.cpu, groups, mem.0);
                let gpu = |row: &crate::probe::BenchRow| crate::rolling::gpu_shape(r.gpu, &row.rows, groups, mem.1);
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
        let gpu_ok = gpu_ok && !held;
        if gpu_ok && !self.profile.gpus.is_empty() && crate::probe::find(&crate::probe::load(&self.cfg.layout), &rt.build_id, &akey).is_none() {
            log::info!("lease {id}: probing the execution providers of runtime pack {} (new here) with worker build {}", rt.build_id, prep.build.build_id);
            self.set_status(Phase::Working, "checking the GPU", Some(describe_job(lease)));
            let (stop_pump, pump) = self.beat_pump(lease);
            let p = crate::probe::run(&self.cfg.layout, &entry, &prep.build.build_id, &rt.build_id, &rt.dir, &self.profile.gpus, &self.profile.gpus_off, self.worker_log.clone(), crate::probe::PROBE_TIMEOUT, &stop);
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
        if let (true, None, Some(reference)) = (gpu_ok && probe.is_some() && crate::probe::has_gpu_provider(&candidates), &bench, net.reference.as_ref()) {
            log::info!("lease {id}: measuring {run}'s network on runtime pack {} ({})", rt.build_id, candidates.join(", "));
            self.set_status(Phase::Working, "checking the GPU", Some(describe_job(lease)));
            let (stop_pump, pump) = self.beat_pump(lease);
            let spec = crate::probe::BenchSpec { model: &net.model, reference, run, threads, providers: candidates.clone() };
            let r = crate::probe::run_bench(&self.cfg.layout, &entry, &prep.build.build_id, &rt.build_id, &rt.dir, &gpus, &self.profile.gpus_off, self.worker_log.clone(), crate::probe::PROBE_TIMEOUT, &stop, &spec);
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
        let c = Self::pick(bench.as_ref(), &gpus, lease, plan, gpu_ok, mem, roll);
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
                    Some((c.clone(), self.cache.ensure_checkpoint(&self.client, run, c, plan.declared(c.gen), cap, &keep, &stop)?))
                }
                None => None,
            };
            let mut models = Vec::new();
            for c in lease.checkpoints.iter().flatten() {
                let run = lease.run.as_deref().ok_or_else(|| CacheError::Bad("checkpoints without a run".into()))?;
                if models.iter().any(|(m, _): &(CheckpointRef, CkptFiles)| m.gen == c.gen) {
                    return Err(CacheError::Bad(format!("checkpoint {} is listed twice", c.gen)));
                }
                models.push((c.clone(), self.cache.ensure_checkpoint(&self.client, run, c, plan.declared(c.gen), cap, &keep, &stop)?));
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
        let d = self.decide(None);
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
        let mem = (Learned::plan(&self.cfg.layout, &run_name, false, limit_mb), Learned::plan(&self.cfg.layout, &run_name, true, limit_mb));
        let choice = self.choose_providers(lease, &plan, &prep, d.gpu && !self.profile.gpus.is_empty(), caps.threads, mem, need.as_ref(), roll.as_ref().map(|(s, _)| s));
        let plan_providers = (choice.providers.clone(), choice.gpu.clone());
        let gpu_choice = plan_providers.0 != ["cpu"];
        if roll.is_some() {
            let m = if gpu_choice { mem.1 } else { mem.0 };
            log::info!(
                "lease {id}: {} games in flight in {} groups (memory: {} MB limit, {} MB + {} MB per game{})",
                choice.conc.unwrap_or(0),
                choice.groups.unwrap_or(0),
                limit_mb,
                m.cost.base_mb,
                m.cost.per_game_mb,
                if m.measured { ", measured here" } else { "" }
            );
        }
        if plan_providers.0 != ["cpu"] {
            log::info!("lease {id}: providers {:?} on {}", plan_providers.0, plan_providers.1["name"].as_str().unwrap_or("?"));
        }
        // rev 19 (PROTOCOL 13): a rolling CPU job's layout is tuned on this machine (`autotune` on)
        let mut tuning = None;
        let mut measure0 = false;
        if let (Some((setup, _)), false, true, "selfplay") = (roll.as_ref(), gpu_choice, self.settings().autotune, lease.kind.as_str()) {
            let (t, m) = self.tuning_for(lease, &plan, &prep, setup, &choice, mem.0);
            log::info!(
                "lease {id}: CPU layout {}{} ({})",
                t.current.describe(),
                if m { ", measured" } else { "" },
                t.entry.describe()
            );
            measure0 = m;
            tuning = Some(t);
        }
        let layout0 = tuning.as_ref().map(|t| t.current);
        let (tx, rx) = mpsc::channel::<Ev>();
        let entry = build.entry_path().expect("worker manifests have an entry");
        let spawn_spec = SpawnSpec {
            exe: entry,
            cwd: work.clone(),
            mem_limit_mb: limit_mb,
            cpu_job: plan_providers.0 == ["cpu"],
            log: self.worker_log.clone(),
            tag: id.clone(),
            runtime_dir: prep.runtime.as_ref().map(|r| r.dir.clone()),
        };
        let spawn = match self.cfg.test_spawn_error.filter(|_| cfg!(debug_assertions)) {
            Some(code) => Err(std::io::Error::from_raw_os_error(code)),
            None => spawn_spec.spawn(0, tx.clone()),
        };
        let worker = match spawn {
            Ok(w) => w,
            Err(e) => return self.worker_not_started(lease, &e),
        };
        log::info!("lease {id}: worker pid {} started", worker.pid);
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
                def,
                def_inflight: false,
                def_reported: false,
                update_inflight: false,
                mem: if gpu_choice { mem.1 } else { mem.0 },
                gpu: gpu_choice,
                peak_mb: 0,
                offset: 0,
                restarts: 0,
                restart: None,
            }),
            infer: layout0.map(|l| (l.infer_workers, l.infer_threads)),
            drainers: Vec::new(),
            tuning: tuning.map(|mut t| {
                if measure0 {
                    t.probe = Some((Probe::new(t.current.conc, t.entry.phase, self.cfg.timings.tune_warm.as_secs_f64(), self.cfg.timings.tune_window.as_secs_f64()), Instant::now()));
                }
                t
            }),
            spawns: 0,
            main_exit: None,
        };
        run.show_tuning(self);
        run.go(self)
    }

    /// rev 19 (PROTOCOL 13): a rolling CPU job's tuning: its key (worker build, runtime pack, network, hardware, OS,
    /// tuner), what may vary (the thread budget - the platform's, or the owner's `cpu_threads` -, sessions, games in
    /// flight within memory, or the run's number), and the remembered search or a new one (from the coordinator's hint
    /// for this hardware class, else the platform's starting point). The lease's first worker runs the next layout to
    /// measure when a trial may run now (a fresh start is a free trial), else the incumbent. -> (tuning, measured).
    fn tuning_for(&self, lease: &Lease, plan: &Plan, prep: &Prepared, setup: &Setup, choice: &crate::probe::Choice, mem: MemPlan) -> (Tuning, bool) {
        let p = &self.profile;
        let hw = tune::Hw { cores: p.cores, logical: p.threads, perf: p.perf_cores, eff: p.eff_cores };
        let d = tune::defaults(tune::Platform::current(), &hw);
        let settings = self.settings();
        let budget = settings.cpu_threads.map_or(d.budget, |n| n.max(2));
        let conc = choice.conc.unwrap_or(crate::rolling::CPU_INFLIGHT).max(1);
        let (conc_min, conc_max) = match setup.cpu {
            crate::rolling::Inflight::N(_) => (conc, conc),
            crate::rolling::Inflight::Auto => ((conc / 4).max(tune::MIN_BATCH).min(conc), mem.cap(crate::rolling::MAX_INFLIGHT, true).min(conc * 2).max(conc)),
        };
        let space = tune::Space { budget, sessions: d.sessions_always, conc_min, conc_max };
        let run = lease.run.clone().unwrap_or_default();
        let key = tune::Key {
            build: lease.build.build_id.clone(),
            runtime: lease.runtime.as_ref().map(|r| r.build_id.clone()),
            net: tune::net_fingerprint(&plan.job, prep.ckpt.as_ref().and_then(|(_, f)| f.model_size)),
            hw: tune::hw_fingerprint(p),
            os: p.os.clone(),
            tuner: tune::TUNER_VERSION,
        };
        let now = zerofarm_common::now_unix();
        let mut entry = match tune::Cache::load(&self.cfg.layout).get(&key) {
            Some(e) => e.clone(),
            None => {
                let groups = d.batch.map_or(choice.groups.unwrap_or(1), |b| (conc / b.max(1)).max(1));
                let start = space.fit(Layout { threads: d.threads, infer_workers: d.infer_workers, infer_threads: d.infer_threads, groups, conc });
                match tune::hint_layout(lease.tuning_hint.as_ref(), &space) {
                    Some(h) => {
                        log::info!("lease {}: starting the tuning from the coordinator's hint: {}", lease.lease_id, h.describe());
                        tune::Entry::new(key, &run, h, true, now)
                    }
                    None => tune::Entry::new(key, &run, start, false, now),
                }
            }
        };
        entry.run = run;
        if entry.maybe_check(now) {
            log::info!("lease {}: the weekly check of the tuned layout is due", lease.lease_id);
        }
        let next = if self.tune_ok { entry.plan(&space) } else { None };
        let current = next.unwrap_or_else(|| entry.wanted(&space));
        let t = Tuning {
            space,
            entry,
            current,
            probe: None,
            handoff: None,
            load: None,
            other_load: 0.0,
            last_ping: Instant::now(),
            revert_due: false,
            no_trial_before: Instant::now(),
            defaults: d,
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
    /// rev 19: workers finishing their games after a layout handoff.
    drainers: Vec<Drainer>,
    /// rev 19: the CPU layout's tuning (a rolling CPU job with `autotune` on).
    tuning: Option<Tuning>,
    /// Workers started after the first (memory restarts, handoffs): `done`'s unfinished ids are then worked out.
    spawns: u32,
    /// The main worker exited (its exit code) while drainers still finish their games.
    main_exit: Option<i32>,
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
            s.insert("inflight".into(), json!(self.conc));
            s.insert("released".into(), json!(r.released));
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
        if let Err(e) = self.worker.send(&v) {
            log::warn!("lease {}: write to worker failed: {e}", self.id());
        }
    }

    fn kill(&mut self, why: &str) {
        log::warn!("lease {}: killing worker: {why}", self.id());
        for d in &mut self.drainers {
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
        self.quit_drainers();
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

    fn gpu_job(&self) -> bool {
        self.provider.as_deref().is_some_and(|p| p != "cpu")
    }

    fn go(&mut self, eng: &mut Engine) -> LeaseEnd {
        eng.set_status(Phase::Working, "", Some(describe_job(&self.lease)));
        let exit_code = loop {
            match self.rx.recv_timeout(eng.cfg.timings.tick) {
                Ok(ev) => self.on_event(eng, ev),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {}
            }
            self.reap_drainers(eng);
            // rev 19 (PROTOCOL 13): the main worker is gone; workers handed off from finish their games first
            if let Some(code) = self.main_exit {
                if self.drainers.is_empty() {
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
                if self.end_ev.is_none() && !self.killed {
                    self.memory_failure(eng, &format!("worker exited with code {code}"));
                }
                // rev 19: a handoff whose old worker finished its games before the next one could start beside it
                self.handoff_after_exit();
                if self.restart_worker(eng) {
                    continue;
                }
                if !self.drainers.is_empty() {
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
        let d = eng.decide(Some(self.worker.pid));
        let must_pause = !d.work || (self.gpu_job() && !d.gpu);
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
                    // The stall lasts until a starvation window shows the worker getting CPU.
                    self.sample = (CpuMark::take(&self.worker), Instant::now());
                    eng.set_status(Phase::Working, "", Some(describe_job(&self.lease)));
                }
            }
            WState::Resuming(_) => self.check_alive(eng, &t),
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
        self.drainers_tick(&t, must_pause);
        self.tune_tick(eng, &t, must_pause);
    }

    /// A beat when one is due (on its own thread; the answer comes back as an event).
    fn maybe_beat(&mut self, eng: &Engine, paused: bool) {
        if !self.beat_inflight && Instant::now() >= self.beat_due && !matches!(self.after, Some(After::Cancel | After::Revoked)) {
            self.beat_inflight = true;
            self.beat_due = Instant::now() + Duration::from_secs(self.lease.beat_s.max(1));
            let body = BeatReq {
                state: if paused { "paused" } else { "running" }.into(),
                progress: Progress { games_done: self.games_done, games_total: games_total(&self.lease) },
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
        let Some(r) = self.roll.as_mut().filter(|r| r.active) else { return };
        let Some(now) = self.worker.commit_mb() else { return };
        r.peak_mb = r.peak_mb.max(now);
        if !live || r.finishing || r.restart.is_some() || limit == 0 || (now as f64) < limit as f64 * crate::rolling::HIGH_SHARE {
            return;
        }
        let id = self.lease.lease_id.clone();
        let conc = self.conc.unwrap_or(1);
        let per = crate::rolling::measured_per_game(now, conc, r.mem.cost);
        if Learned::raise(&eng.cfg.layout, self.lease.run.as_deref().unwrap_or(""), r.gpu, per) {
            log::info!("lease {id}: remembering {per} MB per game in flight for this run");
        }
        let cost = crate::rolling::MemCost { per_game_mb: r.mem.cost.per_game_mb.max(per), ..r.mem.cost };
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
        let hit = (limit > 0 && peak as f64 >= limit as f64 * crate::rolling::HIT_SHARE) || alloc_failure(why) || alloc_failure(&self.worker.stderr_tail());
        let stopping = matches!(self.state, WState::Stopping(_)) || self.after.is_some() || eng.quitting();
        let conc = self.conc.unwrap_or(1);
        let Some(r) = self.roll.as_mut().filter(|r| r.active && !r.finishing && !stopping && hit) else { return false };
        let id = self.lease.lease_id.clone();
        let per = crate::rolling::measured_per_game(peak.max(r.peak_mb).max(limit), conc, r.mem.cost);
        if Learned::raise(&eng.cfg.layout, self.lease.run.as_deref().unwrap_or(""), r.gpu, per) {
            log::info!("lease {id}: remembering {per} MB per game in flight for this run");
        }
        let cost = crate::rolling::MemCost { per_game_mb: r.mem.cost.per_game_mb.max(per), ..r.mem.cost };
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
        // never started (its `end`'s unfinished tail)
        let tail = self
            .end_ev
            .as_ref()
            .and_then(|e| e.get("unfinished"))
            .and_then(|u| u.get("gids"))
            .map(crate::util::parse_ranges)
            .filter(|g| g.len() == 1 && g[0].1 == lo + released && g[0].0 >= lo + r.offset)
            .map(|g| g[0].0 - lo);
        r.offset = if rs.burn { released } else { tail.unwrap_or(released) };
        if rs.count {
            r.restarts += 1;
        }
        r.want_more = false;
        r.next_extend = Instant::now();
        r.peak_mb = 0;
        let gpu = r.gpu;
        let restarts = r.restarts;
        let offset = r.offset;
        match rs.layout {
            // rev 19 (PROTOCOL 13): a handoff's next layout
            Some(l) => {
                self.conc = Some(l.conc);
                self.groups = Some(l.groups);
                self.threads = l.threads;
                self.infer = Some((l.infer_workers, l.infer_threads));
            }
            None => {
                self.conc = Some(rs.conc);
                self.groups = Some(if gpu { crate::rolling::gpu_groups(rs.conc) } else { self.groups.unwrap_or(1).clamp(1, rs.conc) });
            }
        }
        self.spawns += 1;
        // (a memory restart's results directory is named by its restart, a handoff's by its worker)
        self.out_dir = if rs.count { format!("out-r{restarts}") } else { format!("out-w{}", self.spawns) };
        let (warm, window) = (eng.cfg.timings.tune_warm.as_secs_f64(), eng.cfg.timings.tune_window.as_secs_f64());
        let (conc, groups, threads, infer) = (self.conc.unwrap_or(1), self.groups.unwrap_or(1), self.threads, self.infer);
        if let (Some(t), Some((w, it))) = (self.tuning.as_mut(), infer) {
            t.current = Layout { threads, infer_workers: w, infer_threads: it, groups, conc };
            t.handoff = None;
            t.probe = rs.measure.then(|| (Probe::new(conc, t.entry.phase, warm, window), Instant::now()));
            if rs.layout.is_some() {
                t.revert_due = false;
            }
        }
        self.wgen += 1;
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
        match self.spawn.spawn(self.wgen, self.tx.clone()) {
            Ok(w) => {
                match rs.layout {
                    Some(l) => log::info!("lease {id}: worker pid {} started with layout {} (ids from {})", w.pid, l.describe(), lo + offset),
                    None => log::info!("lease {id}: worker pid {} started again ({} games in flight, ids from {}, restart {restarts})", w.pid, rs.conc, lo + offset),
                }
                self.worker = w;
                self.mark = CpuMark::take(&self.worker);
                self.sample = (self.mark, Instant::now());
                self.show_tuning(eng);
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
        let Some(r) = self.roll.as_mut() else { return };
        let now = Instant::now();
        if r.want_more && !r.extend_inflight && now >= r.next_extend {
            r.extend_inflight = true;
            r.next_extend = now + eng.cfg.timings.extend_every;
            // about 2 x the games in flight (PROTOCOL 10.4)
            let body = ExtendReq { games: 2 * self.conc.unwrap_or(1).max(1) };
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
        }
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
            Ev::Worker(g, m) if g != self.wgen && self.drainers.iter().any(|d| d.wgen == g) => self.on_drainer(eng, g, m),
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
                            self.send(op);
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
                    Ok((ready, evict)) => {
                        // the coordinator's evict list, applied once the lease ends (always between leases)
                        for b in evict.iter().take(64).filter(|b| zerofarm_common::is_build_id(b)) {
                            let key = format!("builds/{b}");
                            if eng.cfg.layout.cache_builds().join(b).exists() && !eng.evict_after.contains(&key) {
                                eng.evict_after.push(key);
                            }
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
        j.insert("threads".into(), json!(self.threads));
        // rev 7 (PROTOCOL 8): the agent's own, never the coordinator's
        j.insert("providers".into(), json!(self.providers.0));
        j.insert("gpu".into(), self.providers.1.clone());
        // rev 11 (PROTOCOL 9.6): a GPU job runs at a high conc (the agent's tuning field, PROTOCOL 5)
        for (k, v) in [("conc", self.conc), ("groups", self.groups)] {
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
        match self.infer {
            Some((w, t)) => {
                j.insert("infer_workers".into(), json!(w));
                j.insert("infer_threads".into(), json!(t));
            }
            None => {
                j.remove("infer_workers");
                j.remove("infer_threads");
            }
        }
        j.insert("out_dir".into(), json!(self.out_dir));
        j.insert("runtime_dir".into(), self.prep.runtime.as_ref().map(|r| json!(r.dir)).unwrap_or(Value::Null));
        // rev 15 (PROTOCOL 10.5): a worker with the feature keeps `conc` games in flight past `games` (the released
        // ids), asks for more with `low`, and flushes finished games every `flush_s`
        match self.roll.as_ref().filter(|r| r.active) {
            Some(r) => {
                j.insert("rolling".into(), json!(true));
                j.insert("flush_s".into(), json!(r.setup.flush_s));
                // (a restarted worker: the ids after the ones its predecessors started, PROTOCOL 10.6)
                j.insert("gid_base".into(), json!(r.setup.lo + r.offset));
                j.insert("games".into(), json!(r.released - r.offset));
            }
            None => {
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
                if let Some(t) = self.tuning.as_mut() {
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
                if let Some(p) = v.get("provider").and_then(Value::as_str) {
                    self.provider = Some(p.to_string());
                }
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
                if let Some(p) = s.get("provider").and_then(Value::as_str) {
                    self.provider = Some(p.to_string());
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
                self.on_result_file(eng, &ev, &v, &dir)
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
                    // rev 15 (PROTOCOL 10.6): a rolling worker out of memory is started again with fewer games
                    if self.memory_failure(eng, &msg) {
                        return;
                    }
                    self.report(eng, "crash", &format!("worker fatal error: {msg}"), None);
                    self.kill("fatal error");
                } else {
                    log::warn!("lease {}: worker error: {msg}", self.id());
                }
            }
            other => log::debug!("lease {}: ignoring worker event {other:?}", self.id()),
        }
    }

    fn on_result_file(&mut self, eng: &mut Engine, kind: &str, v: &Value, out_dir: &str) {
        let Some(path) = v.get("path").and_then(Value::as_str) else {
            log::warn!("lease {}: {kind} without a path", self.id());
            return;
        };
        let rel = match check_rel_path(path) {
            Ok(r) => r,
            Err(_) => {
                log::error!("lease {}: worker announced an unsafe path {path:?}", self.id());
                return;
            }
        };
        let src = self.work.join(out_dir).join(rel);
        if !src.is_file() {
            log::error!("lease {}: announced result {path} does not exist", self.id());
            return;
        }
        // x-gen (self-play only): the oldest generation in the chunk. Games that started before a
        // checkpoint switch finish on the old net, so until the next chunk the floor is the old gen.
        let selfplay = self.lease.kind == "selfplay";
        let gen = if selfplay { v.get("gen").and_then(Value::as_u64).or(self.gen_floor) } else { None };
        self.gen_floor = self.prep.ckpt.as_ref().map(|(c, _)| c.gen).or(self.gen_floor);
        let meta = ResultMeta {
            kind: kind.to_string(),
            games: v.get("games").and_then(Value::as_u64).unwrap_or(0),
            ids: v.get("gids").cloned().filter(|g| g.is_array()),
            gen,
            ..Default::default()
        };
        self.store(eng, meta, Source::File(&src));
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
        if let (Some(r), Some(conc)) = (self.roll.as_ref().filter(|r| r.active && r.peak_mb > 0), self.conc) {
            let per = crate::rolling::measured_per_game(r.peak_mb, conc, r.mem.cost);
            if per > r.mem.cost.per_game_mb && Learned::raise(&eng.cfg.layout, self.lease.run.as_deref().unwrap_or(""), r.gpu, per) {
                log::info!("lease {id}: the worker committed up to {} MB with {conc} games in flight: remembering {per} MB per game for this run", r.peak_mb);
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
/// their games meanwhile.
impl Run {
    /// The tray's and `ZeroFarm status`'s tuning line.
    fn show_tuning(&self, eng: &Engine) {
        let finishing: u64 = self.drainers.iter().filter_map(|d| d.in_flight).sum();
        let beside = if finishing > 0 { format!(" ({finishing} games of the previous layout finishing)") } else { String::new() };
        let line = self.tuning.as_ref().map(|t| match &t.probe {
            Some(_) => format!("measuring {} - {}{beside}", t.current.describe(), t.entry.describe()),
            None if t.current != t.entry.wanted(&t.space) => format!("running {} - {}{beside}", t.current.describe(), t.entry.describe()),
            None => format!("{}{beside}", t.entry.describe()),
        });
        crate::status::set(&eng.status, |s| s.tuning = line);
    }

    fn save_tuning(&self, eng: &Engine) {
        if let Some(t) = &self.tuning {
            let mut c = tune::Cache::load(&eng.cfg.layout);
            c.put(t.entry.clone());
            c.save(&eng.cfg.layout);
        }
    }

    /// The main worker's `status`: a measurement's sample, or a handoff's read of where the old worker's ids stop.
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
            // a worker that doesn't count its leaves and games can't be measured: not tuned this lease
            log::info!("lease {id}: the worker reports no leaves / games started: its layout isn't tuned");
            self.tuning = None;
            crate::status::set(&eng.status, |s| s.tuning = None);
            return;
        };
        let Some((probe, since)) = t.probe.as_mut() else { return };
        match probe.sample(since.elapsed().as_secs_f64(), leaves, games, started) {
            Verdict::Pending => {}
            Verdict::Rate(r, ph) => self.end_probe(eng, Ok((r, ph))),
            Verdict::Invalid(why) => self.end_probe(eng, Err(why)),
        }
    }

    /// A measurement's end: recorded or discarded, remembered, the drainers going on.
    fn end_probe(&mut self, eng: &mut Engine, r: Result<(f64, tune::Phase), String>) {
        let id = self.lease.lease_id.clone();
        let retry = 2 * (eng.cfg.timings.tune_warm + eng.cfg.timings.tune_window);
        let Some(t) = self.tuning.as_mut() else { return };
        if t.probe.take().is_none() {
            return;
        }
        let now = zerofarm_common::now_unix();
        let l = t.current;
        match r {
            Ok((rate, ph)) => {
                log::info!("lease {id}: {} measured {} leaves/s", l.describe(), rate.round());
                eng.worker_log.write_line(&format!("tuning [{id}]: {} measured {} leaves/s", l.describe(), rate.round()));
                t.entry.record(l, rate, Some(ph), &t.space, now);
            }
            Err(why) => {
                // (PROTOCOL 13: a trial is never paused and resumed; it is tried again later, from the incumbent)
                log::info!("lease {id}: the trial of {} is discarded: {why}", l.describe());
                eng.worker_log.write_line(&format!("tuning [{id}]: the trial of {} is discarded: {why}", l.describe()));
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

    /// The other programs' share of the CPUs, sampled every `tune_ping` (at least every 0.5 s).
    fn sample_load(&mut self, eng: &Engine, t: &crate::Timings) {
        if self.tuning.as_ref().is_none_or(|x| x.load.is_some_and(|(at, _)| at.elapsed() < t.tune_ping.max(Duration::from_millis(500)))) {
            return;
        }
        let (idle_ms, total_ms) = crate::worker::sys_cpu_ms();
        let ours_ms = self.worker.cpu_ms() + self.drainers.iter().map(|d| d.worker.cpu_ms()).sum::<u64>();
        let now = LoadMark { idle_ms, total_ms, ours_ms };
        let tu = self.tuning.as_mut().expect("checked");
        if let Some((_, prev)) = tu.load {
            tu.other_load = eng.signals.other_load(prev.other_since(&now));
        }
        tu.load = Some((Instant::now(), now));
    }

    /// Each tick: a measurement holds only while a trial may run (else it is discarded); a handoff goes on; else the
    /// next layout to measure (when a trial may run) or the incumbent (when the main worker runs another) is handed
    /// over to.
    fn tune_tick(&mut self, eng: &mut Engine, t: &crate::Timings, must_pause: bool) {
        if self.tuning.is_none() {
            return;
        }
        self.sample_load(eng, t);
        let rolling_ok = self.roll.as_ref().is_some_and(|r| r.active && !r.finishing && r.restart.is_none());
        let other = self.tuning.as_ref().map_or(0.0, |x| x.other_load);
        let quiet = other <= tune::OTHER_LOAD_MAX && self.stall_since.is_none();
        let strict = eng.tune_ok && quiet && !must_pause;
        if self.tuning.as_ref().is_some_and(|x| x.probe.is_some()) {
            let why = if !eng.tune_ok {
                Some("the computer is in use, on battery, running a fullscreen app or otherwise not idle".to_string())
            } else if must_pause || !matches!(self.state, WState::Starting | WState::Running) {
                Some("work was paused".to_string())
            } else if self.stall_since.is_some() {
                Some("the worker got no CPU".to_string())
            } else if other > tune::OTHER_LOAD_MAX {
                Some(format!("other programs used {:.0} % of the CPUs", other * 100.0))
            } else if !rolling_ok || self.after.is_some() || eng.quitting() {
                Some("the lease is ending".to_string())
            } else {
                None
            };
            match why {
                Some(w) => self.end_probe(eng, Err(w)),
                None => {
                    let tu = self.tuning.as_mut().expect("checked");
                    if self.job_sent && tu.last_ping.elapsed() >= t.tune_ping {
                        tu.last_ping = Instant::now();
                        self.send(json!({"op": "ping"}));
                    }
                }
            }
            return;
        }
        if self.tuning.as_ref().is_some_and(|x| x.handoff.is_some()) {
            self.handoff_tick(eng, t, strict);
            return;
        }
        let running = matches!(self.state, WState::Running) && self.job_sent && self.after.is_none() && !eng.quitting();
        // (a worker runs a while before it is replaced; at most MAX_DRAINERS finish their games beside it)
        if !running || !rolling_ok || self.drainers.len() >= MAX_DRAINERS || self.started.elapsed() < t.tune_warm {
            return;
        }
        let tu = self.tuning.as_ref().expect("checked");
        let wanted = tu.entry.wanted(&tu.space);
        let next = if strict && !tu.revert_due && Instant::now() >= tu.no_trial_before { tu.entry.plan(&tu.space) } else { None };
        let (target, measure) = match next {
            Some(l) => (l, true),
            None if tu.current != wanted => (wanted, strict),
            None => return,
        };
        self.start_handoff(eng, target, measure);
    }

    /// A layout handoff starts: the main worker starts no new game (`finish`); once it says where its ids stop, the
    /// next worker starts beside it with the rest (as memory allows; else after it ended).
    fn start_handoff(&mut self, eng: &Engine, next: Layout, measure: bool) {
        let line = format!("handing the job over to {} ({})", next.describe(), if measure { "a measured trial" } else { "the incumbent, not measured" });
        log::info!("lease {}: {line}", self.lease.lease_id);
        eng.worker_log.write_line(&format!("tuning [{}]: {line}", self.lease.lease_id));
        self.send(json!({"op": "finish"}));
        if let Some(r) = self.roll.as_mut() {
            r.want_more = false;
        }
        let tu = self.tuning.as_mut().expect("tuning");
        tu.handoff = Some(Handoff { next, measure, since: Instant::now(), reads: Vec::new(), last_ping: None });
    }

    /// A handoff waits for the old worker's id count (two equal reads after `finish`, else after `10 x tune_settle` the
    /// next worker takes fresh ids), then for room: the old worker and the drainers' memory plus the next worker's plan
    /// within `PLAN_SHARE` of the job's limit.
    fn handoff_tick(&mut self, eng: &mut Engine, t: &crate::Timings, strict: bool) {
        let tu = self.tuning.as_mut().expect("tuning");
        let wanted = tu.entry.wanted(&tu.space);
        let h = tu.handoff.as_mut().expect("handoff");
        // (a trial that can't be measured now: the incumbent instead)
        if h.measure && !strict {
            h.measure = false;
            h.next = wanted;
        }
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
        let limit = self.worker.mem_limit_mb;
        let Some(cost) = self.roll.as_ref().map(|r| r.mem.cost) else { return };
        let mut held = self.worker.commit_mb().unwrap_or(0);
        for d in &mut self.drainers {
            held += d.worker.commit_mb().unwrap_or(0);
        }
        let plan_mb = cost.base_mb + h.next.conc * cost.per_game_mb;
        if limit > 0 && (held + plan_mb) as f64 > limit as f64 * crate::rolling::PLAN_SHARE {
            // (no room yet: the old worker finishes its games, and the next one starts after it)
            return;
        }
        self.spawn_beside(eng, h.next, h.measure, tail);
    }

    /// The next worker of a handoff starts beside the old one, which becomes a drainer (stopped while the next one is
    /// measured). Its ids: the old worker's unstarted tail and on (`started`: the old worker's games started, from
    /// its own first id), else fresh ids after the released ones.
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
        let gen = self.wgen + 1;
        let w = match self.spawn.spawn(gen, self.tx.clone()) {
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
            "lease {id}: worker pid {} started with layout {} (ids from {}{}); pid {} finishes its games beside it",
            w.pid,
            next.describe(),
            first,
            if started.is_none() { ", fresh: the old worker's id count is unknown" } else { "" },
            self.worker.pid
        );
        self.wgen = gen;
        self.spawns += 1;
        let old = std::mem::replace(&mut self.worker, w);
        let old_dir = std::mem::replace(&mut self.out_dir, dir);
        let mut d = Drainer {
            worker: old,
            wgen: gen - 1,
            out_dir: old_dir,
            end_ev: self.end_ev.take(),
            eof: std::mem::take(&mut self.eof),
            last_msg: Instant::now(),
            in_flight: None,
            tail: started.map(|_| first),
        };
        if measure {
            d.worker.suspend();
        }
        self.drainers.push(d);
        if let Some(r) = self.roll.as_mut() {
            r.offset = first - lo;
            r.want_more = false;
            r.next_extend = Instant::now();
            r.peak_mb = 0;
        }
        self.conc = Some(next.conc);
        self.groups = Some(next.groups);
        self.threads = next.threads;
        self.infer = Some((next.infer_workers, next.infer_threads));
        self.batch_rows = next.batch();
        self.state = WState::Starting;
        self.started = Instant::now();
        self.last_msg = Instant::now();
        self.pinged = false;
        self.job_sent = false;
        self.killed = false;
        self.stall_since = None;
        self.mark = CpuMark::take(&self.worker);
        self.sample = (self.mark, Instant::now());
        let (warm, window) = (eng.cfg.timings.tune_warm.as_secs_f64(), eng.cfg.timings.tune_window.as_secs_f64());
        if let Some(t) = self.tuning.as_mut() {
            t.current = next;
            t.handoff = None;
            t.load = None;
            if next == t.entry.wanted(&t.space) {
                t.revert_due = false;
            }
            t.probe = measure.then(|| (Probe::new(next.conc, t.entry.phase, warm, window), Instant::now()));
        }
        self.show_tuning(eng);
    }

    /// The main worker ended while a handoff waited for room: the next worker starts after it (`restart_worker`, with
    /// the released ids it never started).
    fn handoff_after_exit(&mut self) {
        let Some(h) = self.tuning.as_mut().and_then(|t| t.handoff.take()) else { return };
        if let Some(r) = self.roll.as_mut().filter(|r| !r.finishing && r.restart.is_none()) {
            r.restart = Some(Restart { conc: h.next.conc, burn: false, layout: Some(h.next), count: false, measure: h.measure });
        }
    }

    /// A line of a worker finishing its games after a handoff: its games and results count as any; nothing else is
    /// asked of it.
    fn on_drainer(&mut self, eng: &mut Engine, g: u32, m: WorkerMsg) {
        let Some(i) = self.drainers.iter().position(|d| d.wgen == g) else { return };
        let v = match m {
            WorkerMsg::Eof => {
                self.drainers[i].eof = true;
                return;
            }
            WorkerMsg::BadLine(s) => {
                log::warn!("lease {}: a worker finishing its games wrote a non-JSON line: {s}", self.id());
                return;
            }
            WorkerMsg::Line(v) => v,
        };
        self.drainers[i].last_msg = Instant::now();
        match v.get("ev").and_then(Value::as_str).unwrap_or("") {
            "fin" => {
                let n = v.get("n").and_then(Value::as_u64).unwrap_or(0);
                self.games_done += n;
                eng.add_games(n);
            }
            k @ ("shard" | "records" | "evals") => {
                let dir = self.drainers[i].out_dir.clone();
                self.on_result_file(eng, k, &v, &dir);
            }
            "status" => {
                let s = v.get("stats");
                let n = |k: &str| s.and_then(|s| s.get(k)).and_then(Value::as_u64);
                if let (Some(st), Some(g)) = (n("games_started"), n("games")) {
                    self.drainers[i].in_flight = Some(st.saturating_sub(g));
                }
            }
            "end" => self.drainers[i].end_ev = Some(v),
            "error" if v.get("fatal").and_then(Value::as_bool) == Some(true) => {
                log::warn!("lease {}: a worker finishing its games failed: {}", self.id(), v.get("message").and_then(Value::as_str).unwrap_or(""));
            }
            _ => {}
        }
    }

    /// Drainers that exited: their last lines, then gone (the next worker's ids were taken from their unstarted tail:
    /// checked against their `end`).
    fn reap_drainers(&mut self, eng: &mut Engine) {
        let mut i = 0;
        while i < self.drainers.len() {
            let Some(code) = self.drainers[i].worker.try_wait() else {
                i += 1;
                continue;
            };
            let until = Instant::now() + Duration::from_secs(2);
            while !self.drainers[i].eof && Instant::now() < until {
                if let Ok(ev) = self.rx.recv_timeout(Duration::from_millis(50)) {
                    self.on_event(eng, ev);
                }
            }
            let d = self.drainers.remove(i);
            let reason = d.end_ev.as_ref().and_then(|e| e.get("reason")).and_then(Value::as_str).map(String::from);
            log::info!(
                "lease {}: worker pid {} of the previous layout ended ({})",
                self.id(),
                d.worker.pid,
                reason.unwrap_or_else(|| format!("exit code {code}, its games in flight lost"))
            );
            // where its unstarted ids really began (its `end`: the released ids it never finished, its tail last)
            let real = d.end_ev.as_ref().and_then(|e| e.get("unfinished")).and_then(|u| u.get("gids")).map(crate::util::parse_ranges).and_then(|g| g.last().map(|r| r.0));
            if let (Some(est), Some(real)) = (d.tail, real) {
                if est != real {
                    log::warn!("lease {}: the next worker's ids started at {est}, the old worker's unstarted ones at {real}", self.id());
                }
            }
            if let Some(t) = self.tuning.as_mut() {
                t.load = None;
            }
        }
    }

    /// Drainers are stopped while work is paused and while the main worker is measured, and killed when silent too long.
    fn drainers_tick(&mut self, t: &crate::Timings, must_pause: bool) {
        let stop = must_pause || self.tuning.as_ref().is_some_and(|x| x.probe.is_some());
        let id = self.lease.lease_id.clone();
        for d in &mut self.drainers {
            if stop && !d.worker.is_suspended() {
                d.worker.suspend();
            } else if !stop && d.worker.is_suspended() {
                d.worker.resume();
                d.last_msg = Instant::now();
            }
            if !d.worker.is_suspended() && d.last_msg.elapsed() > t.hang_after {
                log::warn!("lease {id}: a worker finishing its games was silent for {} s: killing it", d.last_msg.elapsed().as_secs());
                d.worker.kill();
                d.last_msg = Instant::now();
            }
        }
    }

    /// Every drainer is told to quit (finished games flushed, then `end`), once.
    fn quit_drainers(&mut self) {
        for d in &mut self.drainers {
            if d.worker.is_suspended() {
                d.worker.resume();
            }
            if d.end_ev.is_none() {
                let _ = d.worker.send(&json!({"op": "quit"}));
            }
            d.last_msg = Instant::now();
        }
    }

    /// The main worker is gone and drainers still finish their games: beats, pauses, quits and the slack as for any
    /// lease.
    fn wind_down(&mut self, eng: &mut Engine) {
        let t = eng.cfg.timings.clone();
        eng.maybe_send_saved_crashes();
        if eng.quitting() && !matches!(self.after, Some(After::Quit)) {
            self.after = Some(After::Quit);
            self.quit_drainers();
        }
        let d = eng.decide(None);
        let pause = !d.work;
        if pause {
            let since = *self.stall_since.get_or_insert(Instant::now());
            if since.elapsed() > Duration::from_secs(self.lease.slack_s) && self.after.is_none() {
                log::info!("lease {}: paused longer than slack ({} s); giving it back", self.id(), self.lease.slack_s);
                self.after = Some(After::GiveBack("paused longer than slack".into()));
                self.quit_drainers();
            }
        } else {
            self.stall_since = None;
        }
        self.drainers_tick(&t, pause && self.after.is_none());
        self.maybe_beat(eng, pause);
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
}

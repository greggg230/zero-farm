//! Running one lease (PROTOCOL 3 + 5): fetch and verify what it needs, start the worker, relay
//! its results, beat, yield/suspend/resume with the user's activity, give the lease back after
//! `slack_s` paused, honour cancel/give_back/expiry and new checkpoints, and close it with
//! `done`. Every network call runs off this loop's thread, so a yield is never delayed by I/O.

use crate::cache::{Cache, CacheError, CkptFiles};
use crate::engine::Engine;
use crate::results::{compute_unfinished, ResultMeta, Source};
use crate::status::Phase;
use crate::worker::{CpuMark, SpawnOpts, WorkerMsg, WorkerProc};
use serde_json::{json, Map, Value};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};
use zerofarm_common::http::CallError;
use zerofarm_common::manifest::{check_rel_path, Kind, VerifiedBuild};
use zerofarm_common::protocol::{BeatReq, BeatResp, CheckpointRef, CrashReq, DoneReq, Lease, Progress, Stats};

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
}

enum Ev {
    Worker(WorkerMsg),
    Beat(Result<BeatResp, CallError>),
    Ckpt(Result<(CheckpointRef, CkptFiles), (CheckpointRef, String)>),
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
        let end = self.lease_inner(&lease);
        if end == LeaseEnd::Cancelled {
            self.results.discard(&id);
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
            stats,
            given_back: true,
            unfinished: compute_unfinished(lease, &[]),
        };
        let _ = self.results.write_done(&lease.lease_id, &done);
        LeaseEnd::GivenBack(why.to_string())
    }

    fn prepare(&mut self, lease: &Lease) -> Result<Prepared, CacheError> {
        let s = self.settings();
        let cap = s.disk_cap_mb << 20;
        let mut keep = vec![format!("builds/{}", lease.build.build_id)];
        if let Some(r) = &lease.runtime {
            keep.push(format!("builds/{}", r.build_id));
        }
        if let (Some(run), Some(c)) = (&lease.run, &lease.checkpoint) {
            keep.push(Cache::ckpt_key(run, c.gen));
        }
        // Beat while downloading so a big first download doesn't let the lease expire.
        let pump_stop = Arc::new(AtomicBool::new(false));
        let pump = {
            let (client, stop, id, beat_s) = (self.client.clone(), pump_stop.clone(), lease.lease_id.clone(), lease.beat_s.max(5));
            let total = games_total(lease);
            std::thread::spawn(move || {
                let mut next = Instant::now() + Duration::from_secs(beat_s);
                while !stop.load(Ordering::SeqCst) {
                    if Instant::now() >= next {
                        let body = BeatReq { state: "running".into(), progress: Progress { games_done: 0, games_total: total }, stats: Stats::new() };
                        let _ = client.post_json::<_, Value>(&format!("/v1/lease/{id}/beat"), &body);
                        next = Instant::now() + Duration::from_secs(beat_s);
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
            })
        };
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
                    Some((c.clone(), self.cache.ensure_checkpoint(&self.client, run, c, cap, &keep, &stop)?))
                }
                None => None,
            };
            Ok(Prepared { build, runtime, ckpt })
        })();
        pump_stop.store(true, Ordering::SeqCst);
        let _ = pump.join();
        result
    }

    fn lease_inner(&mut self, lease: &Lease) -> LeaseEnd {
        let id = lease.lease_id.clone();
        self.set_status(Phase::Downloading, "work files", Some(describe_job(lease)));
        let prep = match self.prepare(lease) {
            Ok(p) => p,
            Err(CacheError::Stopped) => {
                self.give_back_unstarted(lease, "agent quitting");
                return LeaseEnd::Quit;
            }
            Err(e) => {
                log::error!("lease {id}: setup failed: {e}");
                return self.give_back_unstarted(lease, &format!("setup failed: {e}"));
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
                return self.give_back_unstarted(lease, &format!("build verification failed: {e}"));
            }
        };
        if let Some((c, f)) = &prep.ckpt {
            if !Cache::reverify_file(&f.model, &c.sha256) {
                return self.give_back_unstarted(lease, "checkpoint verification failed");
            }
        }
        let work = self.cfg.layout.work().join(&id);
        let _ = zerofarm_common::fsutil::remove_dir_all_retry(&work, Duration::from_secs(5));
        if let Err(e) = std::fs::create_dir_all(work.join("out")) {
            return self.give_back_unstarted(lease, &format!("work dir: {e}"));
        }
        let d = self.decide(None);
        let caps = self.caps(&d);
        let (tx, rx) = mpsc::channel::<Ev>();
        let txw = tx.clone();
        let entry = build.entry_path().expect("worker manifests have an entry");
        let spawn = WorkerProc::spawn(
            SpawnOpts {
                exe: &entry,
                cwd: &work,
                mem_limit_mb: caps.ram_mb.max(256),
                cpu_job: !caps.gpu,
                log: self.worker_log.clone(),
                tag: id.clone(),
                runtime_dir: prep.runtime.as_ref().map(|r| r.dir.as_path()),
            },
            move |m| {
                let _ = txw.send(Ev::Worker(m));
            },
        );
        let worker = match spawn {
            Ok(w) => w,
            Err(e) => {
                log::error!("lease {id}: could not start worker: {e}");
                return self.give_back_unstarted(lease, &format!("could not start worker: {e}"));
            }
        };
        log::info!("lease {id}: worker pid {} started", worker.pid);
        let mark = CpuMark::take(&worker);
        let mut run = Run {
            lease: lease.clone(),
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
            threads: caps.threads,
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
        };
        run.go(self)
    }
}

struct Run {
    lease: Lease,
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
        s
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
                break code;
            }
            self.tick(eng);
        };
        self.finish(eng, exit_code)
    }

    fn tick(&mut self, eng: &mut Engine) {
        let t = eng.cfg.timings.clone();
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
        if !self.beat_inflight && Instant::now() >= self.beat_due && !matches!(self.after, Some(After::Cancel | After::Revoked)) {
            self.beat_inflight = true;
            self.beat_due = Instant::now() + Duration::from_secs(self.lease.beat_s.max(1));
            let body = BeatReq {
                state: if paused { "paused" } else { "running" }.into(),
                progress: Progress { games_done: self.games_done, games_total: games_total(&self.lease) },
                stats: self.merged_stats(),
            };
            let (client, tx, id) = (eng.client.clone(), self.tx.clone(), self.lease.lease_id.clone());
            std::thread::spawn(move || {
                let r = client.post_json::<_, BeatResp>(&format!("/v1/lease/{id}/beat"), &body);
                let _ = tx.send(Ev::Beat(r));
            });
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

    fn report(&self, eng: &Engine, kind: &str, message: &str, exit_code: Option<i64>) {
        eng.report_crash(CrashReq {
            build_id: Some(self.lease.build.build_id.clone()),
            kind: kind.into(),
            message: message.into(),
            backtrace: Some(self.worker.stderr_tail()).filter(|s| !s.is_empty()),
            lease_id: Some(self.lease.lease_id.clone()),
            exit_code,
            agent: None,
            os_error: None,
        });
    }

    fn on_event(&mut self, eng: &mut Engine, ev: Ev) {
        match ev {
            Ev::Worker(WorkerMsg::Line(v)) => self.on_worker(eng, v),
            Ev::Worker(WorkerMsg::BadLine(s)) => log::warn!("lease {}: worker wrote a non-JSON line: {s}", self.id()),
            Ev::Worker(WorkerMsg::Eof) => self.eof = true,
            Ev::Beat(r) => self.on_beat(eng, r),
            Ev::Ckpt(r) => {
                self.ckpt_inflight = false;
                match r {
                    Ok((c, f)) => {
                        log::info!("lease {}: switching to checkpoint {}", self.id(), c.gen);
                        self.send(json!({"op": "checkpoint", "gen": c.gen, "model": f.model, "reference": f.reference}));
                        self.prep.ckpt = Some((c, f));
                    }
                    Err((c, e)) => {
                        log::warn!("lease {}: checkpoint {} not usable: {e}", self.id(), c.gen);
                        self.ckpt_failed.push((c.gen, c.sha256));
                    }
                }
            }
        }
    }

    fn job_line(&self) -> Value {
        let mut j = match &self.lease.job {
            Value::Object(m) => m.clone(),
            _ => Map::new(),
        };
        j.insert("op".into(), json!("job"));
        j.insert("lease_id".into(), json!(self.lease.lease_id));
        j.entry("kind").or_insert(json!(self.lease.kind));
        let (model, reference) = match &self.prep.ckpt {
            Some((_, f)) => (json!(f.model), json!(f.reference)),
            None => (Value::Null, Value::Null),
        };
        j.insert("model".into(), model);
        j.insert("reference".into(), reference);
        j.insert("threads".into(), json!(self.threads));
        j.insert("out_dir".into(), json!("out"));
        j.insert("runtime_dir".into(), self.prep.runtime.as_ref().map(|r| json!(r.dir)).unwrap_or(Value::Null));
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
                let line = self.job_line();
                self.send(line);
                self.job_sent = true;
                self.state = WState::Running;
                let started = eng.cfg.layout.cache().join("started");
                let _ = std::fs::create_dir_all(&started);
                let _ = std::fs::write(started.join(&self.lease.build.build_id), b"");
            }
            "selftest" => {
                log::info!("lease {}: selftest {v}", self.id());
                if let Some(x) = v.get("max_abs") {
                    self.stats.insert("selftest_max_abs".into(), x.clone());
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
            "shard" | "records" => self.on_result_file(eng, &ev, &v),
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
                    self.report(eng, "crash", &format!("worker fatal error: {msg}"), None);
                    self.kill("fatal error");
                } else {
                    log::warn!("lease {}: worker error: {msg}", self.id());
                }
            }
            other => log::debug!("lease {}: ignoring worker event {other:?}", self.id()),
        }
    }

    fn on_result_file(&mut self, eng: &mut Engine, kind: &str, v: &Value) {
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
        let src = self.work.join("out").join(rel);
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
        let j = &self.lease.job;
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
                    // rev 2: a newer checkpoint is repeated in every beat; act on it once.
                    if let Some(c) = resp.checkpoint {
                        let current = self.prep.ckpt.as_ref().map(|(c, _)| c.gen);
                        let failed = self.ckpt_failed.iter().any(|(g, h)| *g == c.gen && *h == c.sha256);
                        if current.is_none_or(|g| c.gen > g) && !self.ckpt_inflight && !failed {
                            self.fetch_checkpoint(eng, c);
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
            Err(e) if e.is_auth() => {
                self.after = Some(After::Revoked);
                self.kill("token refused");
            }
            Err(e) => log::warn!("lease {}: beat failed: {e}", self.id()),
        }
    }

    fn fetch_checkpoint(&mut self, eng: &Engine, c: CheckpointRef) {
        let Some(run) = self.lease.run.clone() else { return };
        self.ckpt_inflight = true;
        let (client, tx) = (eng.client.clone(), self.tx.clone());
        let cache = Cache::new(eng.cfg.layout.clone(), eng.cfg.keys.clone());
        let cap = eng.settings().disk_cap_mb << 20;
        let keep = vec![format!("builds/{}", self.lease.build.build_id), Cache::ckpt_key(&run, c.gen)];
        let control = eng.control.clone();
        std::thread::spawn(move || {
            let stop = move || control.quitting();
            let r = match cache.ensure_checkpoint(&client, &run, &c, cap, &keep, &stop) {
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
        let fuzz = self.lease.kind == "fuzz" || self.lease.job.get("kind").and_then(Value::as_str) == Some("fuzz");
        let mut unfinished = end.as_ref().map(|e| e.get("unfinished").cloned().unwrap_or(Value::Null));
        if fuzz && end.is_some() && self.job_sent {
            let first = self.lease.job.get("first").and_then(Value::as_u64).unwrap_or(0);
            let count = self.lease.job.get("count").and_then(Value::as_u64).unwrap_or(0);
            let left = unfinished.as_ref().and_then(|u| u.get("seeds")).map(crate::util::parse_ranges).unwrap_or_default();
            // x-ids: the seeds this lease covered = its range minus what the worker left.
            let covered = crate::util::subtract((first, first + count), &left);
            let done_n = crate::util::total(&covered);
            let body = self.fuzz_json(self.failures.clone(), Some(done_n), true);
            let ids = (!covered.is_empty()).then(|| crate::util::ranges_json(&covered));
            self.store_bytes(eng, "fuzz", done_n, &body, true, ids);
        }
        let metas = eng.results.metas(&id);
        if end.is_none() || !self.job_sent {
            unfinished = Some(compute_unfinished(&self.lease, &metas).unwrap_or(Value::Null));
        }
        let given_back = !(end.is_some() && matches!(reason.as_str(), "done" | "finish") && self.after.is_none());
        let done = DoneReq {
            results: eng.results.delivered_seqs(&id),
            stats: self.merged_stats(),
            given_back,
            unfinished: unfinished.filter(|u| !u.is_null()),
        };
        if let Err(e) = eng.results.write_done(&id, &done) {
            log::error!("lease {id}: could not write done: {e}");
        }
        eng.uploader.wake();
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

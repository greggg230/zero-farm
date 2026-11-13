//! The agent's main loop (PROTOCOL 3): check the idle policy, ask for a lease, run it, repeat;
//! meanwhile report crashes and launcher events, check for agent updates, keep the tray status.

use crate::cache::Cache;
use crate::policy::{decide, mem_floor_mb, Decision, PolicyState};
use crate::results::{ResultStore, Uploader};
use crate::settings::{Settings, SharedMachine};
use crate::signals::SignalSource;
use crate::startfail::StartBackoff;
use crate::status::{self, Phase, SharedStatus};
use crate::AgentConfig;
use serde_json::Value;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};
use zerofarm_common::http::{Backoff, CallError, Client};
use zerofarm_common::protocol::{Caps, Cached, CrashReq, LeaseReq, LeaseResp, Profile};
use zerofarm_common::rotlog::RotatingFile;

/// Past this many bytes of unsent results, ask only for fuzz/diag work (DESIGN 3.5).
pub const MAX_PENDING_RESULTS: u64 = 200 << 20;

/// Quit and wake-up signals between the tray/main thread and the engine thread.
#[derive(Default)]
pub struct Control {
    quit: AtomicBool,
    lock: Mutex<bool>,
    cv: Condvar,
}

impl Control {
    pub fn request_quit(&self) {
        self.quit.store(true, Ordering::SeqCst);
        self.wake();
    }
    pub fn quitting(&self) -> bool {
        self.quit.load(Ordering::SeqCst)
    }
    /// Wakes a sleeping engine (settings changed).
    pub fn wake(&self) {
        *self.lock.lock().unwrap() = true;
        self.cv.notify_all();
    }
    /// Sleeps up to `d` or until woken. False if quitting.
    pub fn sleep(&self, d: Duration) -> bool {
        let mut g = self.lock.lock().unwrap();
        if !*g && !self.quitting() {
            g = self.cv.wait_timeout(g, d).unwrap().0;
        }
        *g = false;
        !self.quitting()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineExit {
    Quit,
    /// A verified new version is in `versions\<v>\` and `state\pending` names it: exit 75.
    Update(String),
    /// The coordinator itself said this machine was revoked (`revoked`, marked with
    /// `x-zerofarm-coordinator: 1`): the enrollment is forgotten.
    Revoked,
    /// A newer `ZeroFarm.exe` is in place (`state\launcher-swap`): exit 76 so the launcher hands
    /// over to it.
    RestartLauncher,
}

pub enum Answer {
    Lease(Box<zerofarm_common::protocol::Lease>),
    Wait(u64, String),
    Update(String),
}

pub struct Engine {
    pub cfg: AgentConfig,
    pub client: Client,
    pub machine: SharedMachine,
    pub status: SharedStatus,
    pub signals: Box<dyn SignalSource>,
    pub policy: PolicyState,
    pub control: Arc<Control>,
    pub results: ResultStore,
    pub uploader: Uploader,
    pub cache: Cache,
    pub worker_log: Arc<RotatingFile>,
    pub profile_fn: Box<dyn Fn() -> Profile + Send>,
    pub profile: Profile,
    pub(crate) profile_refreshed: Instant,
    /// rev 11 (PROTOCOL 9.6): cache entries to remove when the current lease ends (a GPU pack that failed here).
    pub(crate) evict_after: Vec<String>,
    pub(crate) last_update_check: Option<Instant>,
    pub pending_update: Option<String>,
    /// The launcher replacement check ran this run (launcher_update.rs).
    pub(crate) launcher_checked: bool,
    /// Hours on / allowed to work per day (availability.rs).
    pub avail: crate::availability::Tracker,
    quit_event: Option<zerofarm_common::names::QuitEvent>,
    backoff: Backoff,
    /// The coordinator refuses the token (`unauthorized`): 1 min doubling to 1 h between tries,
    /// the token kept (security review M1).
    auth_backoff: Backoff,
    games_unsaved: u64,
    games_saved_at: Instant,
    /// The wait after a worker could not start (startfail.rs; persisted).
    pub(crate) start_backoff: StartBackoff,
    /// The "Windows is blocking..." balloon was shown this run.
    blocked_toast_shown: bool,
    /// Saved crash reports (`state\crashes\`) are sent in the background every `crash_resend`.
    crash_sent_at: Instant,
    crash_sending: Arc<AtomicBool>,
}

impl Engine {
    pub fn new(
        cfg: AgentConfig,
        token: String,
        machine: SharedMachine,
        status: SharedStatus,
        signals: Box<dyn SignalSource>,
        control: Arc<Control>,
        profile_fn: Box<dyn Fn() -> Profile + Send>,
    ) -> Result<Engine, String> {
        let healthy = Arc::new(AtomicBool::new(false));
        let (layout, version) = (cfg.layout.clone(), cfg.version.clone());
        let client = Client::new(&cfg.coordinator, Some(token), &cfg.version)?.on_success(Arc::new(move || {
            if !healthy.swap(true, Ordering::SeqCst) {
                if let Err(e) = layout.mark_healthy(&version) {
                    log::warn!("could not write healthy-{version}: {e}");
                }
            }
        }));
        let results = ResultStore::new(cfg.layout.clone());
        let uploader = Uploader::start(results.clone(), client.clone(), cfg.timings.upload_rescan);
        let cache = Cache::new(cfg.layout.clone(), cfg.keys.clone());
        let worker_log = Arc::new(RotatingFile::new(cfg.layout.logs().join("worker.log"), 1536 * 1024, 1));
        let profile = profile_fn();
        let avail = crate::availability::Tracker::load(&cfg.layout);
        status::set(&status, |s| s.version = cfg.version.clone());
        Ok(Engine {
            quit_event: zerofarm_common::names::QuitEvent::open(&cfg.layout.root),
            client,
            machine,
            status,
            signals,
            policy: PolicyState::default(),
            control,
            results,
            uploader,
            cache,
            worker_log,
            profile_fn,
            profile,
            profile_refreshed: Instant::now(),
            evict_after: Vec::new(),
            last_update_check: None,
            pending_update: None,
            launcher_checked: false,
            avail,
            backoff: Backoff::default(),
            auth_backoff: Backoff::new(Duration::from_secs(60), Duration::from_secs(3600)),
            games_unsaved: 0,
            games_saved_at: Instant::now(),
            start_backoff: StartBackoff::load(&cfg.layout),
            blocked_toast_shown: false,
            crash_sent_at: Instant::now(),
            crash_sending: Arc::new(AtomicBool::new(false)),
            cfg,
        })
    }

    pub fn settings(&self) -> Settings {
        self.machine.lock().unwrap().settings.clone()
    }

    pub fn quitting(&self) -> bool {
        if self.quit_event.as_ref().is_some_and(|q| q.is_set()) {
            self.control.request_quit();
        }
        self.control.quitting()
    }

    /// Samples the signals and applies the policy (and records availability).
    pub fn decide(&mut self, worker_pid: Option<u32>) -> Decision {
        let s = self.signals.sample(worker_pid);
        let cfg = self.settings();
        let d = decide(&mut self.policy, &s, &cfg);
        self.avail.record(crate::availability::now_ms(), &crate::util::local_now().0, d.work);
        d
    }

    pub fn set_status(&self, phase: Phase, detail: &str, job: Option<String>) {
        let games = self.machine.lock().unwrap().games_today();
        status::set(&self.status, |s| {
            s.phase = phase;
            s.detail = detail.to_string();
            s.job = job;
            s.games_today = games;
        });
    }

    pub fn add_games(&mut self, n: u64) {
        self.games_unsaved += n;
        let mut m = self.machine.lock().unwrap();
        m.add_games(n);
        if self.games_saved_at.elapsed() > Duration::from_secs(30) {
            let _ = m.save(&self.cfg.layout);
            self.games_unsaved = 0;
            self.games_saved_at = Instant::now();
        }
        let g = m.games_today();
        drop(m);
        status::set(&self.status, |s| s.games_today = g);
    }

    fn flush_games(&mut self) {
        if self.games_unsaved > 0 {
            let _ = self.machine.lock().unwrap().save(&self.cfg.layout);
            self.games_unsaved = 0;
        }
    }

    /// `/v1/crash`, or kept in `state\crashes\` until it can be sent.
    pub fn report_crash(&self, mut c: CrashReq) {
        c.agent = Some(self.cfg.version.clone());
        log::warn!("crash report: {} {}", c.kind, c.message);
        if let Err(e) = send_crash(&self.client, &c) {
            log::info!("crash report not sent ({e}); keeping it for later");
            save_crash_file(&self.cfg.layout, &c);
        }
    }

    /// Sends the crash reports saved under `state\crashes\` (a caught tray panic, reports that
    /// could not be sent) in the background, at most every `crash_resend`.
    pub(crate) fn maybe_send_saved_crashes(&mut self) {
        if self.crash_sent_at.elapsed() < self.cfg.timings.crash_resend {
            return;
        }
        self.crash_sent_at = Instant::now();
        let any = std::fs::read_dir(self.cfg.layout.crashes()).map(|mut rd| rd.next().is_some()).unwrap_or(false);
        if !any || self.crash_sending.swap(true, Ordering::SeqCst) {
            return;
        }
        let (client, layout, busy) = (self.client.clone(), self.cfg.layout.clone(), self.crash_sending.clone());
        std::thread::spawn(move || {
            send_saved_crashes(&client, &layout);
            busy.store(false, Ordering::SeqCst);
        });
    }

    /// A worker could not be started (startfail.rs): wait before asking for work again.
    pub(crate) fn start_failed(&mut self, blocked: bool) {
        let d = self.start_backoff.fail(&self.cfg.layout, blocked, &self.cfg.timings, zerofarm_common::now_unix());
        log::warn!(
            "{}; asking for work again in {} s",
            if blocked { "Windows is blocking the job program" } else { "the job program could not start" },
            d.as_secs()
        );
        self.show_start_wait();
        if blocked && !self.blocked_toast_shown {
            self.blocked_toast_shown = true;
            status::set(&self.status, |s| s.toast = Some(status::BLOCKED_MESSAGE.into()));
        }
    }

    /// A worker started (said hello): start failures are over.
    pub(crate) fn start_ok(&mut self) {
        self.start_backoff.reset(&self.cfg.layout);
    }

    fn show_start_wait(&self) {
        if self.start_backoff.blocked {
            self.set_status(Phase::Blocked, status::BLOCKED_MESSAGE, None);
        } else {
            self.set_status(Phase::Waiting, "a job program could not start; trying again later", None);
        }
    }

    fn send_saved_reports(&self) {
        for ev in self.cfg.layout.take_launcher_events() {
            let c = CrashReq {
                build_id: None,
                kind: "agent".into(),
                message: format!("launcher {}: {} -> {:?}: {}", ev.kind, ev.from, ev.to, ev.reason),
                backtrace: None,
                lease_id: None,
                exit_code: None,
                agent: Some(self.cfg.version.clone()),
                os_error: None,
            };
            self.report_crash(c);
        }
        send_saved_crashes(&self.client, &self.cfg.layout);
    }

    fn startup(&mut self) {
        log::info!("engine start: agent {} coordinator {}", self.cfg.version, self.cfg.coordinator);
        crate::update::cleanup_versions(&self.cfg.layout, &self.cfg.version);
        let _ = zerofarm_common::fsutil::remove_dir_all_retry(&self.cfg.layout.work(), Duration::from_secs(5));
        let _ = std::fs::create_dir_all(self.cfg.layout.work());
        self.note_unfinished_probe();
        if !self.profile.gpus_off.is_empty() {
            let off: Vec<String> = self.profile.gpus_off.iter().map(|g| format!("{} {}", g.vendor, g.name)).collect();
            log::info!("GPU work is off on {} (this agent never uses these vendors' GPUs): CPU jobs only there", off.join(", "));
        }
        self.send_saved_reports();
    }

    /// 0.2.11: a probe or bench the last run never finished (its marker is still there: the agent or the PC stopped
    /// during it) - counted against its pack on these adapters (`probe::take_inflight`: once, not again for a day;
    /// twice, never for these drivers), reported to the coordinator (`probe_unfinished`), and the profile refreshed so
    /// the coordinator stops offering that pack here.
    fn note_unfinished_probe(&mut self) {
        let now = zerofarm_common::now_unix();
        let Some((m, n)) = crate::probe::take_inflight(&self.cfg.layout, now) else { return };
        let what = if m.kind == "bench" { format!("bench of {}'s network", m.run.as_deref().unwrap_or("a run")) } else { "provider probe".to_string() };
        let after = if n >= crate::probe::UNFINISHED_NEVER { "never tried again with these GPUs and drivers".to_string() } else { "not tried again for a day".to_string() };
        let msg = format!(
            "the agent or the PC stopped during a {what} of runtime pack {} with worker build {} ({} s before this start; adapters {}): unfinished {n} time(s), {after}",
            m.runtime,
            m.worker,
            (now - m.t).max(0),
            m.adapters
        );
        log::warn!("{msg}");
        save_crash_file(
            &self.cfg.layout,
            &CrashReq { build_id: Some(m.worker.clone()), kind: "probe_unfinished".into(), message: msg, backtrace: None, lease_id: None, exit_code: None, agent: Some(self.cfg.version.clone()), os_error: None },
        );
        self.profile = (self.profile_fn)();
        self.profile_refreshed = Instant::now();
    }

    /// Bytes we may still download: the cap minus everything that can't be evicted, bounded by
    /// the volume's free space.
    pub fn disk_budget_mb(&self, s: &Settings) -> u64 {
        let cap = s.disk_cap_mb << 20;
        let fixed = self.cache.usage().saturating_sub(self.cache.evictable(&[]));
        let mut b = cap.saturating_sub(fixed);
        if let Some(free) = crate::util::disk_free_bytes(&self.cfg.layout.root) {
            b = b.min(free.saturating_sub(crate::cache::VOLUME_RESERVE));
        }
        b >> 20
    }

    pub fn caps(&mut self, d: &Decision) -> Caps {
        let s = self.settings();
        let (avail, total) = crate::signals::memory_mb();
        let ram_free = avail.saturating_sub(mem_floor_mb(total));
        Caps {
            threads: s.threads(self.profile.cores, self.profile.threads),
            ram_mb: s.ram_cap_mb(self.profile.ram_mb).min(if total > 0 { ram_free } else { u64::MAX }),
            // rev 7 (PROTOCOL 8): only when a GPU provider passed this PC's probe
            gpu: d.gpu && !self.profile.gpus.is_empty() && crate::probe::has_gpu_provider(&self.profile.providers),
            disk_mb: self.disk_budget_mb(&s),
            // rev 11 (PROTOCOL 9.4): the coordinator may then offer a GPU vendor's runtime pack not probed here yet
            gpu_allowed: d.gpu && !self.profile.gpus.is_empty(),
            gpu_games: self.gpu_games(),
        }
    }

    /// rev 11 (PROTOCOL 9.4): `caps.gpu_games` - per run, from the fresh benches of these adapters, the fewest games per
    /// lease at which a GPU pays here (at most 32 runs).
    fn gpu_games(&self) -> std::collections::BTreeMap<String, u64> {
        let akey = crate::probe::adapters_key(&self.profile.gpus);
        let now = zerofarm_common::now_unix();
        let mut m = std::collections::BTreeMap::new();
        for b in crate::probe::load_benches(&self.cfg.layout) {
            if b.adapters != akey || b.error.is_some() || now - b.t >= crate::probe::BENCH_MAX_AGE_S || !zerofarm_common::is_slug(&b.run) {
                continue;
            }
            // (0: benched, and no GPU provider pays at any size here)
            let g = b.gpu_games.unwrap_or(0);
            let e = m.entry(b.run.clone()).or_insert(g);
            *e = if *e == 0 || g == 0 { (*e).max(g) } else { (*e).min(g) };
        }
        while m.len() > 32 {
            let k = m.keys().next_back().cloned().unwrap();
            m.remove(&k);
        }
        m
    }

    /// Between leases, when the GPU may be used: probe one cached runtime pack this PC has no fresh
    /// result for (probe.rs; PROTOCOL 8), then refresh the profile. True if a probe ran.
    pub(crate) fn maybe_probe(&mut self, d: &Decision) -> bool {
        if !d.gpu || self.profile.gpus.is_empty() {
            return false;
        }
        let now = zerofarm_common::now_unix();
        let Some((entry, worker, runtime, rdir)) = crate::probe::next_probe(&self.cfg.layout, &self.cfg.keys, &self.profile.gpus, now) else {
            return false;
        };
        log::info!("probing the execution providers of runtime pack {runtime} with worker build {worker}");
        self.set_status(Phase::Working, "checking the GPU", None);
        let control = self.control.clone();
        let stop = move || control.quitting();
        let p = crate::probe::run(&self.cfg.layout, &entry, &worker, &runtime, &rdir, &self.profile.gpus, &self.profile.gpus_off, self.worker_log.clone(), crate::probe::PROBE_TIMEOUT, &stop);
        if p.error.as_deref() == Some("stopped") {
            return true;
        }
        match &p.error {
            Some(e) => log::warn!("provider probe of runtime pack {runtime}: {e}"),
            None => log::info!(
                "provider probe of runtime pack {runtime}: {} ({})",
                p.providers.join(", "),
                p.results.iter().map(|r| format!("{}{}: {}", r.provider, r.adapter.as_ref().map(|a| format!(" on {a}")).unwrap_or_default(), if r.ok { "ok" } else { "no" })).collect::<Vec<_>>().join("; ")
            ),
        }
        // rev 11 (PROTOCOL 9.6): a GPU pack's own providers, so its verdict waits for a real network's when the
        // built-in graph fails them
        let mut p = p;
        p.need = zerofarm_common::manifest::verify_dir(
            &rdir,
            &self.cfg.keys,
            &zerofarm_common::manifest::Expect { build_id: Some(&runtime), kind: Some(zerofarm_common::manifest::Kind::Runtime), version: None },
        )
        .ok()
        .and_then(|v| v.manifest.gpu.map(|g| g.providers));
        crate::probe::record(&self.cfg.layout, p);
        self.profile = (self.profile_fn)();
        self.profile_refreshed = Instant::now();
        true
    }

    fn profile_digest_path(&self) -> std::path::PathBuf {
        self.cfg.layout.state().join("profile-digest")
    }

    pub fn request_lease(&mut self, d: &Decision) -> Result<Answer, CallError> {
        if self.profile_refreshed.elapsed() > Duration::from_secs(3600) {
            self.profile = (self.profile_fn)();
            self.profile_refreshed = Instant::now();
        }
        let digest = self.profile.digest();
        let known = zerofarm_common::fsutil::read_trimmed(&self.profile_digest_path()).as_deref() == Some(digest.as_str());
        let caps = self.caps(d);
        let (builds, ckpts) = self.cache.cached();
        let mut kinds = self.cfg.kinds.clone();
        if self.results.pending_bytes() > MAX_PENDING_RESULTS {
            kinds.retain(|k| k == "fuzz" || k == "diag");
        }
        let mut req = LeaseReq {
            profile_digest: digest.clone(),
            profile: (!known).then(|| self.profile.clone()),
            caps,
            cached: Cached { builds, ckpts },
            kinds,
        };
        let resp: LeaseResp = match self.client.post_json("/v1/lease", &req) {
            Err(CallError::Http { status: 409, body: Some(b), .. }) if b.get("need_profile").and_then(Value::as_bool) == Some(true) => {
                req.profile = Some(self.profile.clone());
                self.client.post_json("/v1/lease", &req)?
            }
            r => r?,
        };
        if req.profile.is_some() {
            let _ = zerofarm_common::fsutil::atomic_write(&self.profile_digest_path(), digest.as_bytes());
        }
        // rev 2: any lease answer may ask for diagnostics.
        if let Some(what) = &resp.upload_diag {
            crate::logging::upload_diag(&self.client, &self.cfg.layout, what);
        }
        if let Some(l) = resp.lease {
            Ok(Answer::Lease(Box::new(l)))
        } else if let Some(u) = resp.update {
            Ok(Answer::Update(u.min_agent))
        } else {
            Ok(Answer::Wait(resp.wait_s.unwrap_or(60), resp.reason.unwrap_or_default()))
        }
    }

    pub fn run(&mut self) -> EngineExit {
        self.startup();
        let exit = self.main_loop();
        self.flush_games();
        self.avail.save();
        self.uploader.stop(Duration::from_secs(3));
        log::info!("engine stop: {exit:?}");
        exit
    }

    fn main_loop(&mut self) -> EngineExit {
        loop {
            if self.quitting() {
                return EngineExit::Quit;
            }
            if self.uploader.revoked() {
                return EngineExit::Revoked;
            }
            if let Some(v) = self.pending_update.clone() {
                match self.cfg.layout.set_pending(Some(&v)) {
                    Ok(()) => {
                        self.set_status(Phase::Updating, &v, None);
                        return EngineExit::Update(v);
                    }
                    Err(e) => {
                        log::error!("could not write state\\pending: {e}");
                        self.pending_update = None;
                    }
                }
            }
            self.maybe_check_latest();
            if self.pending_update.is_some() {
                continue;
            }
            if let Some(crate::launcher_update::Outcome::Restart) = self.maybe_update_launcher() {
                self.set_status(Phase::Updating, "launcher", None);
                return EngineExit::RestartLauncher;
            }
            self.maybe_send_saved_crashes();
            let d = self.decide(None);
            // A worker could not start: no lease until the wait is over (never a lease storm).
            if let Some(left) = self.start_backoff.remaining(zerofarm_common::now_unix(), &self.cfg.timings) {
                self.show_start_wait();
                self.control.sleep(left.min(self.cfg.timings.max_wait));
                continue;
            }
            if !d.work {
                let why = d.block.map(|b| b.describe()).unwrap_or("");
                self.set_status(Phase::Idle, why, None);
                self.control.sleep(self.cfg.timings.idle_poll);
                continue;
            }
            if self.maybe_probe(&d) {
                continue;
            }
            let answer = self.request_lease(&d);
            if answer.is_ok() {
                self.auth_backoff.reset();
            }
            match answer {
                Ok(Answer::Lease(l)) => {
                    self.backoff.reset();
                    match self.run_lease(*l) {
                        crate::lease::LeaseEnd::Revoked => return EngineExit::Revoked,
                        crate::lease::LeaseEnd::Quit => return EngineExit::Quit,
                        _ => {}
                    }
                }
                Ok(Answer::Wait(s, reason)) => {
                    self.backoff.reset();
                    self.set_status(Phase::Waiting, &reason, None);
                    let d = Duration::from_secs(s.max(1)).min(self.cfg.timings.max_wait);
                    self.control.sleep(d);
                }
                Ok(Answer::Update(min)) => {
                    self.backoff.reset();
                    match self.update_to_latest(Some(&min)) {
                        Err(e) => {
                            log::error!("coordinator requires agent {min}, but updating failed: {e}");
                            self.control.sleep(Duration::from_secs(300).min(self.cfg.timings.max_wait));
                        }
                        // No update is going to happen now (the required version was rolled back
                        // recently, or nothing newer is published): wait, never ask again at once.
                        // Found on the canary test PC 2026-11-11: 0.1.1 re-leased several times a
                        // second after the launcher rolled back 0.1.2.
                        Ok(()) if self.pending_update.is_none() => {
                            log::warn!("coordinator requires agent {min}; no usable update right now; asking again in 5 min");
                            self.set_status(Phase::Waiting, "update required", None);
                            self.control.sleep(Duration::from_secs(300).min(self.cfg.timings.max_wait));
                        }
                        Ok(()) => {}
                    }
                }
                Err(e) if e.is_revoked() => {
                    log::error!("the coordinator says this machine was revoked: {e}");
                    return EngineExit::Revoked;
                }
                // The coordinator itself refuses the token: keep it (a coordinator bug or a
                // restore can fix that), say so in the tray and try again later.
                Err(e) if e.is_unauthorized() => {
                    let d = self.auth_backoff.next_delay();
                    log::error!("the coordinator doesn't accept this PC's token ({e}); keeping it, trying again in {d:?}");
                    self.set_status(Phase::SignInFailed, "", None);
                    status::set(&self.status, |s| s.last_error = Some(e.to_string()));
                    self.control.sleep(d);
                }
                Err(e) => {
                    let d = self.backoff.delay_for(&e);
                    log::warn!("lease request failed: {e}; retrying in {d:?}");
                    self.set_status(Phase::Offline, &e.to_string(), None);
                    status::set(&self.status, |s| s.last_error = Some(e.to_string()));
                    self.control.sleep(d);
                }
            }
        }
    }
}

/// `POST /v1/crash`, with the user name and profile path redacted from the message and the
/// backtrace (worker stderr carries paths; security review M5). A coordinator that doesn't know a
/// newer kind (`blocked` before rev 5, `definition` before rev 6) refuses it with 400: it then
/// goes as a plain `crash`, the kind in its message.
pub fn send_crash(client: &Client, c: &CrashReq) -> Result<(), CallError> {
    let mut c = c.clone();
    c.message = zerofarm_common::redact::redact(&c.message).into_owned();
    c.backtrace = c.backtrace.map(|b| zerofarm_common::redact::redact(&b).into_owned());
    match client.post_json::<_, Value>("/v1/crash", &c) {
        Ok(_) => Ok(()),
        Err(CallError::Http { status: 400, .. }) if !matches!(c.kind.as_str(), "panic" | "crash" | "agent") => {
            let mut plain = c.clone();
            plain.kind = "crash".into();
            plain.os_error = None;
            plain.message = match c.kind.as_str() {
                "blocked" => format!("blocked by policy (os error {}): {}", c.os_error.unwrap_or(0), c.message),
                k => format!("{k}: {}", c.message),
            };
            client.post_json::<_, Value>("/v1/crash", &plain).map(|_| ())
        }
        Err(e) => Err(e),
    }
}

/// Sends every report in `state\crashes\`, oldest first; stops at the first that has to wait
/// (network, 5xx, 429, or the token refused). One the coordinator refuses otherwise is dropped.
pub fn send_saved_crashes(client: &Client, layout: &zerofarm_common::layout::Layout) {
    let Ok(rd) = std::fs::read_dir(layout.crashes()) else { return };
    let mut files: Vec<std::path::PathBuf> = rd.flatten().map(|e| e.path()).collect();
    files.sort();
    for p in files {
        let Some(c) = std::fs::read(&p).ok().and_then(|b| serde_json::from_slice::<CrashReq>(&b).ok()) else {
            let _ = std::fs::remove_file(&p);
            continue;
        };
        match send_crash(client, &c) {
            Ok(()) => {
                let _ = std::fs::remove_file(&p);
            }
            Err(e) if e.is_retryable() || e.is_revoked() || e.is_unauthorized() => break,
            Err(e) => {
                log::warn!("saved crash report refused ({e}); dropping it");
                let _ = std::fs::remove_file(&p);
            }
        }
    }
}

pub fn save_crash_file(layout: &zerofarm_common::layout::Layout, c: &CrashReq) {
    let dir = layout.crashes();
    let _ = std::fs::create_dir_all(&dir);
    // Keep at most 20 reports.
    if let Ok(rd) = std::fs::read_dir(&dir) {
        if rd.count() >= 20 {
            return;
        }
    }
    let name = format!("{}-{}.json", zerofarm_common::now_unix(), std::process::id());
    let _ = std::fs::write(dir.join(name), serde_json::to_vec(c).unwrap_or_default());
}

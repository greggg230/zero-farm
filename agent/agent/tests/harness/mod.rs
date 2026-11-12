//! Shared harness: a temp install root, the mock coordinator with a signed fake worker build, and
//! the real engine on a thread with scripted idle signals and short timings.
#![allow(dead_code)]

use farm_agent::engine::{Control, Engine, EngineExit};
use farm_agent::policy::Signals;
use farm_agent::settings::{Machine, SharedMachine};
use farm_agent::signals::SharedSignals;
use farm_agent::status::{Phase, SharedStatus, Status};
use farm_agent::{AgentConfig, Timings};
use mockcoord::testkit::{bin, make_worker_build, test_key, wait_until, TestKey};
use mockcoord::MockCoord;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;
use zerofarm_common::keys::TrustedKeys;
use zerofarm_common::layout::Layout;
use zerofarm_common::protocol::Profile;

pub const T: Duration = Duration::from_secs(60);

pub struct H {
    pub dir: tempfile::TempDir,
    pub layout: Layout,
    pub mock: MockCoord,
    pub key: TestKey,
    pub build_id: String,
    pub token: String,
    pub signals: SharedSignals,
    pub control: Arc<Control>,
    pub machine: SharedMachine,
    pub status: SharedStatus,
    pub timings: Timings,
    pub version: String,
    /// What the "launcher" told the engine (`ZEROFARM_LAUNCHER`).
    pub launcher_level: u32,
    /// Starting a worker fails with this raw Windows error (`AgentConfig::test_spawn_error`).
    pub spawn_error: Option<i32>,
    thread: Option<JoinHandle<EngineExit>>,
}

pub fn fake_profile() -> Profile {
    Profile {
        os: "Windows 11 26100".into(),
        cpu: "Test CPU".into(),
        cores: 8,
        threads: 16,
        perf_cores: 8,
        eff_cores: 0,
        ram_mb: 32768,
        disk_free_mb: 100_000,
        gpus: vec![],
        providers: vec!["cpu".into()],
        idle_h_day: None,
        on_h_day: None,
    }
}

pub fn fast_timings() -> Timings {
    Timings {
        tick: Duration::from_millis(50),
        hello: Duration::from_secs(10),
        hello_first: Duration::from_secs(60),
        yield_answer: Duration::from_secs(2),
        ping_after: Duration::from_secs(5),
        hang_after: Duration::from_secs(15),
        quit_grace: Duration::from_secs(8),
        starve_cap: Duration::from_secs(600),
        starve_window: Duration::from_millis(250),
        update_check: Duration::from_secs(6 * 3600),
        idle_poll: Duration::from_millis(100),
        upload_rescan: Duration::from_millis(300),
        max_wait: Duration::from_millis(500),
        launcher_selftest: Duration::from_secs(30),
        // The real start-failure backoffs: tests that need several attempts shorten them.
        blocked_backoff: Duration::from_secs(15 * 60),
        blocked_backoff_max: Duration::from_secs(24 * 3600),
        start_backoff: Duration::from_secs(60),
        start_backoff_max: Duration::from_secs(3600),
        crash_resend: Duration::from_millis(500),
    }
}

impl H {
    pub fn new() -> H {
        Self::with_build(&[])
    }

    /// `extra` files go into the signed worker build (e.g. `startup.json`).
    pub fn with_build(extra: &[(&str, &[u8])]) -> H {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("ZeroFarm");
        std::fs::create_dir_all(&root).unwrap();
        let layout = Layout::new(&root);
        let mock = MockCoord::start();
        let key = test_key();
        mock.trust(&key.pubkey);
        let bdir = dir.path().join("build");
        let build_id = make_worker_build(&bdir, &key.key, &bin("fakeworker"), extra);
        mock.add_build_dir(&bdir);
        let token = mock.add_machine("m_TEST0000000000000000000001");
        let machine = Machine { machine_id: Some("m_TEST".into()), ..Machine::default() };
        H {
            dir,
            layout,
            mock,
            key,
            build_id,
            token,
            signals: SharedSignals::new(Signals::all_clear(0)),
            control: Arc::new(Control::default()),
            machine: Arc::new(Mutex::new(machine)),
            status: Arc::new(Mutex::new(Status::default())),
            timings: fast_timings(),
            version: "0.1.0".into(),
            launcher_level: 2,
            spawn_error: None,
            thread: None,
        }
    }

    pub fn config(&self) -> AgentConfig {
        AgentConfig {
            layout: self.layout.clone(),
            coordinator: self.mock.url.clone(),
            version: self.version.clone(),
            build_id: None,
            keys: TrustedKeys::parse(&[self.key.pubkey.clone()]).unwrap(),
            timings: self.timings.clone(),
            kinds: vec!["fuzz".into(), "eval".into(), "selfplay".into(), "diag".into()],
            channel: "stable".into(),
            launcher_level: self.launcher_level,
            test_spawn_error: self.spawn_error,
        }
    }

    pub fn start(&mut self) {
        assert!(self.thread.is_none());
        if std::env::var("ZEROFARM_TEST_LOG").is_ok() {
            farm_agent::logging::init(&self.layout, true);
        }
        self.control = Arc::new(Control::default());
        let mut eng = Engine::new(
            self.config(),
            self.token.clone(),
            self.machine.clone(),
            self.status.clone(),
            Box::new(self.signals.clone()),
            self.control.clone(),
            Box::new(fake_profile),
        )
        .unwrap();
        self.thread = Some(std::thread::spawn(move || eng.run()));
    }

    /// Asks the engine to quit and returns how it ended.
    pub fn stop(&mut self) -> EngineExit {
        self.control.request_quit();
        self.join()
    }

    pub fn join(&mut self) -> EngineExit {
        self.thread.take().expect("running").join().expect("engine thread panicked")
    }

    pub fn finished(&self) -> bool {
        self.thread.as_ref().map(|t| t.is_finished()).unwrap_or(true)
    }

    pub fn lease(&self, id: &str, job: Value) -> Value {
        let kind = job["kind"].as_str().unwrap().to_string();
        let mut l = MockCoord::lease(id, &kind, &self.build_id, job);
        l["beat_s"] = json!(1);
        l
    }

    pub fn push(&self, l: Value) {
        self.mock.push_lease(l);
    }

    pub fn phase(&self) -> Phase {
        self.status.lock().unwrap().phase
    }

    pub fn user_active(&self, active: bool) {
        self.signals.set(|s| s.idle_s = if active { 0 } else { 3600 });
    }

    pub fn wait_done(&self, id: &str) -> Value {
        assert!(wait_until(T, || self.mock.done_for(id).is_some()), "no done for {id}; log: {}", self.agent_log());
        self.mock.done_for(id).unwrap()
    }

    pub fn agent_log(&self) -> String {
        // In-process tests don't install the global logger; the worker log is still written.
        std::fs::read_to_string(self.layout.logs().join("worker.log")).unwrap_or_default()
    }

    pub fn crashes(&self) -> Vec<Value> {
        self.mock.with(|s| s.crashes.clone())
    }
}

impl Drop for H {
    fn drop(&mut self) {
        if self.thread.is_some() {
            self.control.request_quit();
            let _ = self.thread.take().unwrap().join();
        }
    }
}

pub fn wait(f: impl FnMut() -> bool) -> bool {
    wait_until(T, f)
}

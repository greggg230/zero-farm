//! farm-agent: the zero-farm app on a friend's PC (DESIGN 3, PROTOCOL 3/5/6).
//!
//! The binary (`main.rs`) wires these modules together; tests drive the [`engine::Engine`]
//! in-process against the mock coordinator and the scripted fake worker.

pub mod availability;
pub mod cache;
pub mod engine;
pub mod enroll;
pub mod install;
pub mod jobplan;
pub mod launcher_update;
pub mod lease;
pub mod logging;
pub mod policy;
pub mod probe;
pub mod profile;
pub mod results;
pub mod settings;
pub mod signals;
pub mod startfail;
pub mod status;
pub mod tray;
pub mod update;
pub mod util;
pub mod worker;

use std::path::Path;
use std::time::Duration;
use zerofarm_common::keys::TrustedKeys;
use zerofarm_common::layout::Layout;

/// Production coordinator (DESIGN 1).
pub const DEFAULT_COORDINATOR: &str = "https://farm.greggg230.com";

/// Timeouts and intervals. PROTOCOL 5 fixes `hello` (10 s) and `yield` (2 s); the rest is agent
/// policy, shortened in tests.
#[derive(Debug, Clone)]
pub struct Timings {
    /// Lease event-loop tick (signals are sampled this often while a worker runs).
    pub tick: Duration,
    pub hello: Duration,
    /// `hello` deadline the first time a build runs on this PC: antivirus scans a newly
    /// downloaded executable before it can start, which can take longer than 10 s.
    pub hello_first: Duration,
    pub yield_answer: Duration,
    /// No message from a running worker for this long → `ping`.
    pub ping_after: Duration,
    /// No message from a running worker for this long → it is hung: kill it.
    pub hang_after: Duration,
    /// After `quit`, how long to wait for `end` before killing.
    pub quit_grace: Duration,
    /// The worker runs at idle priority: on a CPU saturated by other programs it may get no CPU
    /// at all. A deadline that passes while the worker was starved (it got no CPU time and the
    /// system had no idle time) is extended instead of enforced, for at most this long in total.
    /// Zero disables the leniency.
    pub starve_cap: Duration,
    /// How often a running worker is checked for CPU starvation (Stats `starved_ms`; a worker
    /// starved, or paused, longer than the lease's slack gives the lease back).
    pub starve_window: Duration,
    /// `GET /v1/agent/latest` interval (PROTOCOL 6: every 6 h).
    pub update_check: Duration,
    /// Policy check interval while not working.
    pub idle_poll: Duration,
    /// Uploader rescan interval when nothing wakes it.
    pub upload_rescan: Duration,
    /// Longest `wait_s` we honour in one go (so settings changes are noticed).
    pub max_wait: Duration,
    /// How long a new launcher's `selftest` may take (antivirus scans a new exe first).
    pub launcher_selftest: Duration,
    /// Windows refused to start the worker (Smart App Control, AppLocker, Defender): wait this
    /// long before asking for work again, ×4 after each further refusal, up to
    /// `blocked_backoff_max` (PROTOCOL 3, rev 5: 15 min, 1 h, 4 h, 16 h, 24 h).
    pub blocked_backoff: Duration,
    pub blocked_backoff_max: Duration,
    /// Any other failure to start a worker: wait this long, doubling up to `start_backoff_max`.
    pub start_backoff: Duration,
    pub start_backoff_max: Duration,
    /// How often crash reports saved under `state\crashes\` (e.g. a caught tray panic) are sent.
    pub crash_resend: Duration,
}

impl Default for Timings {
    fn default() -> Self {
        Timings {
            tick: Duration::from_millis(200),
            hello: Duration::from_secs(10),
            hello_first: Duration::from_secs(60),
            yield_answer: Duration::from_secs(2),
            ping_after: Duration::from_secs(30),
            hang_after: Duration::from_secs(90),
            quit_grace: Duration::from_secs(10),
            starve_cap: Duration::from_secs(600),
            starve_window: Duration::from_secs(5),
            update_check: Duration::from_secs(6 * 3600),
            idle_poll: Duration::from_secs(1),
            upload_rescan: Duration::from_secs(30),
            max_wait: Duration::from_secs(300),
            launcher_selftest: Duration::from_secs(60),
            blocked_backoff: Duration::from_secs(15 * 60),
            blocked_backoff_max: Duration::from_secs(24 * 3600),
            start_backoff: Duration::from_secs(60),
            start_backoff_max: Duration::from_secs(3600),
            crash_resend: Duration::from_secs(60),
        }
    }
}

/// Everything the engine needs to know that is not user-adjustable.
#[derive(Clone)]
pub struct AgentConfig {
    pub layout: Layout,
    pub coordinator: String,
    /// This agent's version (from its signed manifest when it runs from `versions\<v>\`).
    pub version: String,
    /// This agent's build id, when it runs from a signed version directory.
    pub build_id: Option<String>,
    /// Release keys: manifests (worker builds, runtime packs, agent updates) only.
    pub keys: TrustedKeys,
    /// Data keys: definitions (PROTOCOL 7) only. Each check accepts only its own list.
    pub data_keys: TrustedKeys,
    pub timings: Timings,
    /// Job kinds this agent asks for.
    pub kinds: Vec<String>,
    pub channel: String,
    /// `ZEROFARM_LAUNCHER` from the launcher that started us (0: none, or a launcher before
    /// 0.2.0 that can't hand over to a new launcher).
    pub launcher_level: u32,
    /// Tests only (honoured by debug builds; `farm-agent.exe` always sets None): starting a
    /// worker fails with this raw Windows error instead of creating the process, e.g. 4551 as
    /// Smart App Control answers for an unsigned program.
    pub test_spawn_error: Option<i32>,
}

/// The agent's identity: version and build id from `manifest.json` next to the executable
/// (the launcher verified it before starting us), else the compiled-in package version.
pub fn agent_identity(exe_dir: &Path) -> (String, Option<String>) {
    use zerofarm_common::manifest::{build_id, Kind, Manifest, MANIFEST_FILE};
    if let Ok(bytes) = std::fs::read(exe_dir.join(MANIFEST_FILE)) {
        if let Ok(m) = Manifest::parse(&bytes) {
            if m.kind == Kind::Agent {
                return (m.version, Some(build_id(&bytes)));
            }
        }
    }
    (env!("CARGO_PKG_VERSION").to_string(), None)
}

/// Tests only (debug builds; compiled out of release builds): workers run at normal priority
/// instead of IDLE_PRIORITY_CLASS, so a PC saturated by other builds doesn't starve the fake
/// worker of the timing-sensitive suites (lifecycle, blocked, eval) into timeouts.
pub fn test_worker_normal_priority() -> bool {
    #[cfg(debug_assertions)]
    {
        std::env::var("ZEROFARM_TEST_WORKER_PRIORITY").is_ok_and(|v| v == "normal")
    }
    #[cfg(not(debug_assertions))]
    {
        false
    }
}

/// Debug builds only: an environment override used by tests that run the real binaries.
pub fn test_env(name: &str) -> Option<String> {
    #[cfg(debug_assertions)]
    {
        std::env::var(name).ok().filter(|v| !v.is_empty())
    }
    #[cfg(not(debug_assertions))]
    {
        let _ = name;
        None
    }
}

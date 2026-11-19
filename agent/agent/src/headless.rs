//! The macOS agent has no tray in 0.3.0 (PROTOCOL 11.5): it runs headless under the launcher, and the launcher's
//! CLI (`ZeroFarm status | pause [minutes] | resume | set <key> <value> | quit`, forwarded to this agent binary) talks
//! to the running agent through `state/`:
//!
//! * the agent writes `state/status.json` (what the tray would show: phase, job, games today, CPU threads and the
//!   settings) when it changes and every 30 s;
//! * a change is a request file `state/requests/<id>.json` (`{"op": "pause", "minutes"}`, `{"op": "resume"}`,
//!   `{"op": "set", "key", "value"}`) plus SIGUSR1 to the agent (the holder of `state/agent.lock`); the agent applies
//!   it as the tray's menu does (settings saved, the engine woken) and answers in `<id>.done`;
//! * with no agent running, the CLI changes `state/machine.json` itself.
//!
//! The loop also asks the engine to quit at once on a quit signal (launchd's SIGTERM, `ZeroFarm quit`), and when the
//! launcher that started it is gone (the agent was reparented: the job object's kill-on-close on Windows), and waits
//! for an enrollment (`ZeroFarm setup --invite`) instead of exiting when there is none, so a launcher never sees a
//! crash loop.

use crate::engine::{Control, EngineExit};
use crate::settings::{pause_for, Machine, Pause, Settings, SharedMachine};
use crate::status::{Phase, SharedStatus};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use zerofarm_common::layout::Layout;

pub type StartEngine = Box<dyn FnMut() -> Result<JoinHandle<EngineExit>, String>>;

/// How long the CLI waits for the running agent to answer a request.
pub const ANSWER_WAIT: Duration = Duration::from_secs(5);
/// `status.json` is rewritten at least this often while nothing changes.
const STATUS_EVERY: Duration = Duration::from_secs(30);

/// The cores the CPU-threads setting is worked out from.
#[derive(Clone, Copy, Debug)]
pub struct Cores {
    pub physical: u32,
    pub logical: u32,
    pub perf: u32,
    pub eff: u32,
}

impl Cores {
    pub fn detect() -> Cores {
        let (physical, logical, perf, eff) = crate::profile::cores();
        Cores { physical, logical, perf, eff }
    }

    /// The CPU threads a job gets with `s` (automatic: one per physical core).
    pub fn threads(&self, s: &Settings) -> u32 {
        s.threads(self.physical, self.logical)
    }

    pub fn describe(&self, s: &Settings) -> String {
        let auto = Settings { cpu_threads: None, ..s.clone() }.threads(self.physical, self.logical);
        let split = if self.eff > 0 { format!(", {} performance + {} efficiency", self.perf, self.eff) } else { String::new() };
        match s.cpu_threads {
            None => format!("{auto} (automatic: one per physical core{split})"),
            Some(_) => format!("{} (set; automatic would be {auto}{split})", self.threads(s)),
        }
    }
}

pub fn status_path(layout: &Layout) -> PathBuf {
    layout.state().join("status.json")
}

pub fn requests_dir(layout: &Layout) -> PathBuf {
    layout.state().join("requests")
}

/// A change asked for from the CLI.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Request {
    pub op: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub minutes: Option<u64>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Reply {
    pub ok: bool,
    pub message: String,
}

/// How a pause reads.
pub fn pause_text(p: &Pause, now: i64) -> String {
    match p {
        Pause::None => "not paused".into(),
        Pause::Indefinitely => "paused until you resume".into(),
        Pause::Until { t } if *t <= now => "not paused".into(),
        Pause::Until { t } => format!("paused for {} more minutes", (t - now + 59) / 60),
    }
}

/// Applies a request to the settings (as the tray's menu does).
pub fn apply(m: &mut Machine, r: &Request, now: i64) -> Result<String, String> {
    match r.op.as_str() {
        "pause" => {
            m.settings.pause = match r.minutes {
                Some(min) if min > 0 => pause_for(now, (min.min(60 * 24 * 365) * 60) as i64),
                _ => Pause::Indefinitely,
            };
            Ok(pause_text(&m.settings.pause, now))
        }
        "resume" => {
            m.settings.pause = Pause::None;
            Ok("resumed".into())
        }
        "set" => {
            let (Some(k), Some(v)) = (&r.key, &r.value) else { return Err("set <key> <value>".into()) };
            m.settings.set_by_name(k, v)
        }
        op => Err(format!("unknown request {op:?}")),
    }
}

/// What `status.json` holds.
fn status_json(status: &SharedStatus, machine: &SharedMachine, cores: Cores) -> Value {
    let st = status.lock().map(|s| s.clone()).unwrap_or_default();
    let m = machine.lock().map(|m| m.clone()).unwrap_or_default();
    json!({
        "t": zerofarm_common::now_unix(),
        "pid": std::process::id(),
        "version": st.version,
        "phase": format!("{:?}", st.phase),
        "headline": st.headline(),
        "job": st.job,
        "detail": st.detail,
        "last_error": st.last_error,
        "games_today": m.games_today(),
        "name": m.name,
        "machine_id": m.machine_id,
        "threads": cores.threads(&m.settings),
        "threads_text": cores.describe(&m.settings),
        "tuning": st.tuning,
        "settings": m.settings,
    })
}

/// Answers the requests waiting in `state/requests/`.
fn serve_requests(layout: &Layout, machine: &SharedMachine, control: &Control) {
    let dir = requests_dir(layout);
    let Ok(rd) = std::fs::read_dir(&dir) else { return };
    let mut reqs: Vec<PathBuf> = rd.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "json")).collect();
    reqs.sort();
    for p in reqs {
        let reply = match std::fs::read(&p).ok().and_then(|b| serde_json::from_slice::<Request>(&b).ok()) {
            None => Reply { ok: false, message: "unreadable request".into() },
            Some(r) => {
                let mut m = machine.lock().unwrap();
                match apply(&mut m, &r, zerofarm_common::now_unix()) {
                    Ok(msg) => match m.save(layout) {
                        Ok(()) => {
                            log::info!("settings changed from the command line: {msg}");
                            Reply { ok: true, message: msg }
                        }
                        Err(e) => Reply { ok: false, message: format!("could not save the settings: {e}") },
                    },
                    Err(e) => Reply { ok: false, message: e },
                }
            }
        };
        let _ = zerofarm_common::fsutil::atomic_write(&p.with_extension("done"), &serde_json::to_vec(&reply).expect("json"));
        let _ = std::fs::remove_file(&p);
        control.wake();
    }
}

fn quit_asked(layout: &Layout) -> bool {
    zerofarm_common::names::QuitEvent::open(&layout.root).is_some_and(|q| q.is_set())
}

/// Not enrolled: says so in `status.json` and waits for `ZeroFarm setup --invite CODE` to enroll this Mac (or a quit).
/// True once enrolled.
fn wait_for_enrollment(layout: &Layout, machine: &SharedMachine, status: &SharedStatus, cores: Cores) -> bool {
    crate::status::set(status, |s| s.phase = Phase::NotEnrolled);
    log::warn!("not enrolled: waiting for `ZeroFarm setup --invite CODE`");
    let mut written = None::<Instant>;
    loop {
        if quit_asked(layout) {
            return false;
        }
        let m = Machine::load(layout);
        if m.enrolled() {
            *machine.lock().unwrap() = m;
            log::info!("enrolled as {:?}", machine.lock().unwrap().machine_id);
            return true;
        }
        if written.is_none_or(|t| t.elapsed() >= STATUS_EVERY) {
            let _ = zerofarm_common::fsutil::atomic_write(&status_path(layout), &serde_json::to_vec_pretty(&status_json(status, machine, cores)).expect("json"));
            written = Some(Instant::now());
        }
        std::thread::sleep(Duration::from_secs(2));
    }
}

/// Runs the engine until it ends; returns the process exit code (0 quit, 75 update, 76 new launcher, 4 revoked).
pub fn run(machine: SharedMachine, status: SharedStatus, control: Arc<Control>, layout: Layout, cores: Cores, mut start_engine: StartEngine) -> i32 {
    zerofarm_common::names::install_quit_signals();
    let _ = std::fs::create_dir_all(requests_dir(&layout));
    // the launcher that started us (PROTOCOL 6: `ZEROFARM_LAUNCHER`); when it is gone, so are we
    let parent = std::env::var_os(zerofarm_common::LAUNCHER_ENV).map(|_| unsafe { libc::getppid() });
    if !machine.lock().unwrap().enrolled() && !wait_for_enrollment(&layout, &machine, &status, cores) {
        return 0;
    }
    let engine = match start_engine() {
        Ok(h) => h,
        Err(e) => {
            log::error!("engine did not start: {e}");
            return 70;
        }
    };
    let mut last: Option<(Value, Instant)> = None;
    let mut asked_quit = false;
    loop {
        if engine.is_finished() {
            break;
        }
        if !asked_quit && quit_asked(&layout) {
            log::info!("quit requested");
            control.request_quit();
            asked_quit = true;
        }
        if !asked_quit && parent.is_some_and(|p| unsafe { libc::getppid() } != p) {
            log::warn!("the launcher that started this agent is gone; quitting");
            control.request_quit();
            asked_quit = true;
        }
        zerofarm_common::names::take_wake();
        serve_requests(&layout, &machine, &control);
        let now = status_json(&status, &machine, cores);
        let changed = last.as_ref().is_none_or(|(v, t)| {
            let strip = |v: &Value| {
                let mut v = v.clone();
                v.as_object_mut().map(|o| o.remove("t"));
                v
            };
            strip(v) != strip(&now) || t.elapsed() >= STATUS_EVERY
        });
        if changed {
            if let Err(e) = zerofarm_common::fsutil::atomic_write(&status_path(&layout), &serde_json::to_vec_pretty(&now).expect("json")) {
                log::debug!("status.json: {e}");
            }
            last = Some((now, Instant::now()));
        }
        // (a SIGUSR1 interrupts the sleep only between ticks: requests are answered within 250 ms)
        std::thread::sleep(Duration::from_millis(250));
    }
    let code = match engine.join() {
        Ok(EngineExit::Quit) => 0,
        Ok(EngineExit::Update(_)) => zerofarm_common::EXIT_UPDATE,
        Ok(EngineExit::RestartLauncher) => zerofarm_common::EXIT_RESTART_LAUNCHER,
        Ok(EngineExit::Revoked) => {
            let mut m = machine.lock().unwrap();
            m.forget_enrollment();
            let _ = m.save(&layout);
            drop(m);
            crate::status::set(&status, |s| s.phase = Phase::Revoked);
            log::error!("this Mac was removed from the farm; `ZeroFarm setup --invite CODE` joins it again");
            4
        }
        Err(_) => 70,
    };
    let _ = zerofarm_common::fsutil::atomic_write(&status_path(&layout), &serde_json::to_vec_pretty(&status_json(&status, &machine, cores)).expect("json"));
    code
}

// ---------- the CLI side ----------

/// `status`: what the running agent wrote, or the settings when it isn't running.
pub fn cli_status(layout: &Layout) -> i32 {
    let running = zerofarm_common::names::holder_pid("agent", &layout.root);
    let st: Option<Value> = std::fs::read(status_path(layout)).ok().and_then(|b| serde_json::from_slice(&b).ok());
    let st = st.filter(|v| running.is_some_and(|p| v["pid"].as_i64() == Some(p as i64)));
    let m = Machine::load(layout);
    let now = zerofarm_common::now_unix();
    let cores = Cores::detect();
    match &st {
        Some(v) => println!("Zero Farm {}: {}", v["version"].as_str().unwrap_or("?"), v["headline"].as_str().unwrap_or("?")),
        None if running.is_some() => println!("Zero Farm is starting"),
        None => println!("Zero Farm is not running (it starts at login; `launchctl kickstart gui/$(id -u)/{}` starts it now)", zerofarm_common::layout::LAUNCH_AGENT_LABEL),
    }
    if !m.enrolled() {
        println!("  not set up: `ZeroFarm setup --invite CODE` joins the farm");
    } else {
        println!("  this Mac: {} ({})", m.name.clone().unwrap_or_default(), m.machine_id.clone().unwrap_or_default());
        println!("  games today: {}", st.as_ref().and_then(|v| v["games_today"].as_u64()).unwrap_or(m.games_today()));
    }
    let s = &m.settings;
    println!("  CPU threads: {}", cores.describe(s));
    // rev 19 (PROTOCOL 13.5): worker processes per self-play job
    println!(
        "  worker processes: {}",
        match (s.workers, s.autotune) {
            (Some(n), true) => format!("{n} (set; `ZeroFarm set workers auto` lets the tuner choose)"),
            (Some(n), false) => format!("{n} (set, a fixed layout: autotune off)"),
            (None, true) => "auto (the tuner measures 1 to what the cores and memory allow)".to_string(),
            (None, false) => "auto: 1 (autotune off)".to_string(),
        }
    );
    // rev 19 (PROTOCOL 13): the layout of the current self-play job, else the last tuned one
    match st.as_ref().and_then(|v| v["tuning"].as_str()) {
        Some(t) => println!("  layout: {t}"),
        None if !s.autotune => println!("  layout: autotune off (`ZeroFarm set autotune on` measures the best one)"),
        None => match crate::tune::Cache::load(layout).entries.iter().filter(|e| e.rate().is_some()).max_by_key(|e| e.t) {
            Some(e) => println!("  layout (last tuned, {}): {}", e.run, e.describe()),
            None => println!("  layout: not tuned yet (measured during the first self-play while the Mac is idle)"),
        },
    }
    let when = if s.mode == crate::settings::Mode::Always {
        "also while the Mac is in use".to_string()
    } else {
        format!("only while the Mac is idle ({} min without input)", s.idle_minutes)
    };
    let power = if s.on_battery { "on battery too" } else { "not on battery or in Low Power Mode" };
    println!("  works: {when}, {power}{}", s.hours.map(|_| format!(", {}", s.describe_key("hours"))).unwrap_or_default());
    println!("  keep awake while working: {}", if s.keep_awake { "on" } else { "off" });
    println!("  {}", pause_text(&s.pause, now));
    println!(
        "  memory cap for jobs: {}",
        s.max_ram_mb.map_or_else(|| format!("auto (half the RAM, {} MB)", s.ram_cap_mb(crate::signals::memory_mb().1)), |n| format!("{n} MB"))
    );
    if let Some(e) = st.as_ref().and_then(|v| v["last_error"].as_str()) {
        println!("  last error: {e}");
    }
    0
}

/// `pause [minutes]`, `resume`, `set <key> <value>`: asks the running agent, or changes `machine.json` when none runs.
pub fn cli_change(layout: &Layout, r: &Request) -> i32 {
    let say = |rep: Reply| {
        if rep.ok {
            println!("{}", rep.message);
            0
        } else {
            eprintln!("{}", rep.message);
            1
        }
    };
    if let Some(pid) = zerofarm_common::names::holder_pid("agent", &layout.root) {
        let dir = requests_dir(layout);
        let _ = std::fs::create_dir_all(&dir);
        let id = format!("{}-{}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0), std::process::id());
        let req = dir.join(format!("{id}.json"));
        let done = dir.join(format!("{id}.done"));
        if zerofarm_common::fsutil::atomic_write(&req, &serde_json::to_vec(r).expect("json")).is_ok() {
            // SAFETY: kill(2) of the agent lock's holder.
            unsafe { libc::kill(pid, libc::SIGUSR1) };
            let end = Instant::now() + ANSWER_WAIT;
            while Instant::now() < end {
                if let Some(rep) = std::fs::read(&done).ok().and_then(|b| serde_json::from_slice::<Reply>(&b).ok()) {
                    let _ = std::fs::remove_file(&done);
                    return say(rep);
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            let _ = std::fs::remove_file(&req);
            if zerofarm_common::names::instance_running("agent", &layout.root) {
                eprintln!("the running agent did not answer within {} s", ANSWER_WAIT.as_secs());
                return 1;
            }
        }
    }
    let mut m = Machine::load(layout);
    let rep = match apply(&mut m, r, zerofarm_common::now_unix()) {
        Ok(msg) => match m.save(layout) {
            Ok(()) => Reply { ok: true, message: msg },
            Err(e) => Reply { ok: false, message: format!("could not save the settings: {e}") },
        },
        Err(e) => Reply { ok: false, message: e },
    };
    say(rep)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_change_the_settings_like_the_tray() {
        let mut m = Machine::default();
        let r = |op: &str| Request { op: op.into(), key: None, value: None, minutes: None };
        assert_eq!(apply(&mut m, &Request { minutes: Some(30), ..r("pause") }, 1000).unwrap(), "paused for 30 more minutes");
        assert_eq!(m.settings.pause, Pause::Until { t: 1000 + 1800 });
        assert_eq!(apply(&mut m, &r("pause"), 1000).unwrap(), "paused until you resume");
        assert_eq!(apply(&mut m, &r("resume"), 1000).unwrap(), "resumed");
        assert_eq!(m.settings.pause, Pause::None);
        let set = Request { key: Some("cpu_threads".into()), value: Some("8".into()), ..r("set") };
        assert_eq!(apply(&mut m, &set, 0).unwrap(), "cpu_threads 8");
        assert!(apply(&mut m, &r("set"), 0).is_err());
        assert!(apply(&mut m, &r("dance"), 0).is_err());
        let c = Cores { physical: 16, logical: 16, perf: 12, eff: 4 };
        assert_eq!(c.describe(&m.settings), "8 (set; automatic would be 16, 12 performance + 4 efficiency)");
        assert_eq!(c.describe(&Settings::default()), "16 (automatic: one per physical core, 12 performance + 4 efficiency)");
    }
}

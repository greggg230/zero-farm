//! ZeroFarm.exe — the launcher (PROTOCOL 6). Started at logon from the HKCU Run key.
//!
//! * starts `versions\<current>\farm-agent.exe run --root <root>` (verified against the
//!   compiled-in release keys before every start), telling it through `ZEROFARM_LAUNCHER=2` that
//!   this launcher can hand over to a newer one;
//! * on exit code 75 with `state\pending`: verifies the pending version's manifest and files,
//!   sets previous := current, current := pending, and starts it;
//! * rolls back (current := previous) if a freshly switched version doesn't write
//!   `state\healthy-<v>` within 120 s, or after 3 non-zero exits within 10 minutes, and leaves a
//!   note in `state\launcher-events.jsonl` for the older agent to report (`/v1/crash`, kind
//!   `agent`);
//! * on exit code 76 (the agent put a newer `ZeroFarm.exe` in place and renamed this one to
//!   `ZeroFarm.old-<n>.exe`, see `state\launcher-swap`): starts the new launcher with
//!   `--takeover <pid>` and exits once it confirms (`state\launcher-ok-<its pid>`, written when
//!   its first agent has run for 10 s). If it can't start, exits early or doesn't confirm within
//!   60 s, this launcher kills it, puts itself back as `ZeroFarm.exe`, marks the new one bad and
//!   carries on;
//! * exit code 0 from the agent (the user chose Quit) stops the launcher too.
//!
//! It never downloads anything and has no console window. Leftover `ZeroFarm.old-*.exe` /
//! `ZeroFarm.bad-*.exe` / `ZeroFarm.new.exe` files are deleted when it starts.
//!
//! `ZeroFarm.exe uninstall` forwards to `versions\<current>\farm-agent.exe uninstall` (the
//! Apps & Features uninstall entry points here because the agent's path changes on update).
//! `ZeroFarm.exe selftest` checks that this binary runs, trusts a key and verifies the current
//! version (an agent runs it on a new launcher before putting it in place); exit 0 = good.

#![windows_subsystem = "windows"]

use std::collections::VecDeque;
use std::os::windows::io::AsRawHandle;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use zerofarm_common::fsutil::{atomic_write, remove_file_quiet, rename_retry};
use zerofarm_common::keys::TrustedKeys;
use zerofarm_common::layout::{is_launcher_leftover, is_version_label, LauncherEvent, Layout, AGENT_EXE};
use zerofarm_common::manifest::{verify_dir, Expect, Kind};
use zerofarm_common::names::{single_instance, Handle, QuitEvent};
use zerofarm_common::rotlog::{utc_timestamp, RotatingFile};
use zerofarm_common::{now_unix, EXIT_RESTART_LAUNCHER, EXIT_UPDATE, LAUNCHER_ENV, LAUNCHER_LEVEL};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const DETACHED_PROCESS: u32 = 0x0000_0008;
const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
const HEALTHY_TIMEOUT: Duration = Duration::from_secs(120);
const CRASH_WINDOW: Duration = Duration::from_secs(600);
const CRASHES_FOR_ROLLBACK: usize = 3;
/// A launcher started with `--takeover` confirms once its first agent has run this long.
const TAKEOVER_CONFIRM: Duration = Duration::from_secs(10);
/// How long the launcher handing over waits for that confirmation.
const HANDOVER_TIMEOUT: Duration = Duration::from_secs(60);
/// How long a launcher started with `--takeover` waits for the old one to release the mutex.
const INSTANCE_WAIT: Duration = Duration::from_secs(30);
/// After a takeover, keep trying to delete the old launcher's file (it runs until it sees the
/// confirmation) for this long.
const LEFTOVER_RETRY: Duration = Duration::from_secs(120);

mod job {
    use std::ffi::c_void;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation, SetInformationJobObject,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };

    /// A job object that kills its processes when the launcher goes away.
    pub struct Job(HANDLE);
    impl Job {
        pub fn new() -> Option<Job> {
            unsafe {
                let h = CreateJobObjectW(std::ptr::null(), std::ptr::null());
                if h.is_null() {
                    return None;
                }
                let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
                info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                SetInformationJobObject(
                    h,
                    JobObjectExtendedLimitInformation,
                    &info as *const _ as *const c_void,
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                );
                Some(Job(h))
            }
        }
        pub fn assign(&self, process: HANDLE) -> bool {
            unsafe { AssignProcessToJobObject(self.0, process) != 0 }
        }
    }
    impl Drop for Job {
        fn drop(&mut self) {
            unsafe { CloseHandle(self.0) };
        }
    }
}

enum Outcome {
    Exited(i32),
    NotHealthy,
    Quit,
}

/// Verifies `versions\<v>\` as a signed agent package whose entry is farm-agent.exe.
fn verify_version(layout: &Layout, keys: &TrustedKeys, v: &str) -> Result<(), String> {
    if !is_version_label(v) {
        return Err(format!("bad version label {v:?}"));
    }
    let dir = layout.version_dir(v);
    let vb = verify_dir(&dir, keys, &Expect { build_id: None, kind: Some(Kind::Agent), version: Some(v) }).map_err(|e| e.to_string())?;
    if vb.manifest.entry.as_deref().map(|e| e.eq_ignore_ascii_case(AGENT_EXE)) != Some(true) {
        return Err(format!("agent manifest entry is {:?}, expected {AGENT_EXE}", vb.manifest.entry));
    }
    Ok(())
}

/// Starts a process that must outlive this launcher: detached, no stdio, out of our job when the
/// job we run in allows it.
fn spawn_outliving(exe: &Path, args: &[&str], cwd: &Path) -> std::io::Result<Child> {
    let base = DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW;
    let go = |flags: u32| {
        Command::new(exe)
            .args(args)
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(flags)
            .spawn()
    };
    go(base | CREATE_BREAKAWAY_FROM_JOB).or_else(|_| go(base))
}

/// The single-instance mutex, waiting up to `wait` for another holder to let go.
fn take_instance(root: &Path, wait: Duration) -> Option<Handle> {
    let end = Instant::now() + wait;
    loop {
        if let Some(h) = single_instance("launcher", root) {
            return Some(h);
        }
        if Instant::now() >= end {
            return None;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

struct Launcher {
    layout: Layout,
    keys: TrustedKeys,
    log: RotatingFile,
    quit: Option<QuitEvent>,
    job: Option<job::Job>,
    healthy_timeout: Duration,
    crashes: VecDeque<Instant>,
    instance: Option<Handle>,
    /// Started by a launcher handing over to us (`--takeover <its pid>`), not yet confirmed.
    takeover: Option<u32>,
    confirm_after: Duration,
    handover_timeout: Duration,
    /// Until then, retry deleting leftover launcher files now and then.
    leftovers_until: Option<Instant>,
    leftovers_next: Instant,
}

impl Launcher {
    fn log(&self, msg: &str) {
        self.log.write_line(&format!("{} [launcher {}] {msg}", utc_timestamp(now_unix()), std::process::id()));
    }

    fn quit_requested(&self) -> bool {
        self.quit.as_ref().is_some_and(|q| q.is_set())
    }

    /// Sleeps up to `d`; false if a quit was requested meanwhile.
    fn sleep(&self, d: Duration) -> bool {
        match &self.quit {
            Some(q) => !q.wait(d.as_millis().min(u32::MAX as u128) as u32),
            None => {
                std::thread::sleep(d);
                true
            }
        }
    }

    fn verify(&self, v: &str) -> Result<(), String> {
        verify_version(&self.layout, &self.keys, v)
    }

    fn event(&self, kind: &str, from: &str, to: Option<&str>, reason: &str) {
        let ev = LauncherEvent { kind: kind.into(), from: from.into(), to: to.map(String::from), reason: reason.into(), t: now_unix() };
        if let Err(e) = self.layout.push_launcher_event(&ev) {
            self.log(&format!("could not record event: {e}"));
        }
    }

    /// Deletes leftover launcher files in the root (`staged`: also an agent's `ZeroFarm.new.exe`,
    /// only safe before any agent runs). True when none is left.
    fn cleanup_leftovers(&self, staged: bool) -> bool {
        let Ok(rd) = std::fs::read_dir(&self.layout.root) else { return true };
        let mut clean = true;
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if !is_launcher_leftover(&name, staged) {
                continue;
            }
            match std::fs::remove_file(e.path()) {
                Ok(()) => self.log(&format!("removed leftover {name}")),
                Err(_) => clean = false, // still running (an old launcher finishing its handover)
            }
        }
        clean
    }

    fn tidy(&mut self) {
        if let Some(until) = self.leftovers_until {
            if Instant::now() >= self.leftovers_next {
                self.leftovers_next = Instant::now() + Duration::from_secs(2);
                if self.cleanup_leftovers(false) || Instant::now() > until {
                    self.leftovers_until = None;
                }
            }
        }
    }

    /// We were started by a launcher handing over: tell it we're in charge.
    fn confirm_takeover(&mut self) {
        let Some(old) = self.takeover.take() else { return };
        match atomic_write(&self.layout.launcher_ok_marker(std::process::id()), now_unix().to_string().as_bytes()) {
            Ok(()) => self.log(&format!("took over from launcher pid {old}")),
            Err(e) => self.log(&format!("could not confirm the takeover: {e}")),
        }
        self.leftovers_until = Some(Instant::now() + LEFTOVER_RETRY);
    }

    /// The version to start: `state\current` if it verifies, else a verified `previous`.
    fn resolve_current(&mut self) -> Option<String> {
        let cur = match self.layout.current() {
            Some(c) => c,
            None => {
                self.log("state\\current is missing");
                return self.layout.previous().filter(|p| self.verify(p).is_ok()).inspect(|p| {
                    let _ = self.layout.set_current(p);
                });
            }
        };
        match self.verify(&cur) {
            Ok(()) => Some(cur),
            Err(e) => {
                self.log(&format!("version {cur} fails verification: {e}"));
                match self.layout.previous().filter(|p| *p != cur && self.verify(p).is_ok()) {
                    Some(prev) => {
                        self.event("verify_failed", &cur, Some(&prev), &e);
                        self.switch_back(&cur, &prev);
                        Some(prev)
                    }
                    None => {
                        self.event("verify_failed", &cur, None, &e);
                        None
                    }
                }
            }
        }
    }

    fn switch_back(&mut self, from: &str, to: &str) {
        let _ = self.layout.set_current(to);
        let _ = self.layout.set_previous(None);
        let _ = self.layout.mark_bad(from);
        let _ = std::fs::remove_file(self.layout.healthy_marker(from));
        self.crashes.clear();
    }

    /// A version that has never written its healthy marker, right after a switch.
    fn on_probation(&self, cur: &str) -> bool {
        !self.layout.is_healthy(cur) && self.layout.previous().is_some_and(|p| p != cur && self.layout.version_dir(&p).exists())
    }

    fn rollback(&mut self, from: &str, reason: &str) -> bool {
        let Some(to) = self.layout.previous().filter(|p| p != from) else {
            self.log(&format!("{from}: {reason}; no previous version to roll back to"));
            return false;
        };
        if let Err(e) = self.verify(&to) {
            self.log(&format!("{from}: {reason}; previous {to} fails verification ({e}), not rolling back"));
            return false;
        }
        self.log(&format!("rolling back {from} -> {to}: {reason}"));
        self.event("rollback", from, Some(&to), reason);
        self.switch_back(from, &to);
        true
    }

    fn spawn(&self, v: &str) -> std::io::Result<Child> {
        let dir = self.layout.version_dir(v);
        let child = Command::new(dir.join(AGENT_EXE))
            .arg("run")
            .arg("--root")
            .arg(&self.layout.root)
            .current_dir(&self.layout.root)
            .env(LAUNCHER_ENV, LAUNCHER_LEVEL.to_string())
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()?;
        if let Some(j) = &self.job {
            j.assign(child.as_raw_handle() as _);
        }
        Ok(child)
    }

    fn supervise(&mut self, child: &mut Child, v: &str, mut probation: bool, started: Instant) -> Outcome {
        loop {
            match child.try_wait() {
                Ok(Some(st)) => return Outcome::Exited(st.code().unwrap_or(-1)),
                Ok(None) => {}
                Err(e) => {
                    self.log(&format!("wait failed: {e}"));
                    return Outcome::Exited(-1);
                }
            }
            if self.takeover.is_some() && started.elapsed() >= self.confirm_after {
                self.confirm_takeover();
            }
            self.tidy();
            if probation {
                if self.layout.is_healthy(v) {
                    self.log(&format!("{v} is healthy"));
                    probation = false;
                } else if started.elapsed() > self.healthy_timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Outcome::NotHealthy;
                }
            }
            if self.quit_requested() {
                // The agent watches the same event; give it time to finish cleanly.
                let deadline = Instant::now() + Duration::from_secs(15);
                while Instant::now() < deadline {
                    if let Ok(Some(_)) = child.try_wait() {
                        return Outcome::Quit;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                let _ = child.kill();
                let _ = child.wait();
                return Outcome::Quit;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    fn handle_update(&mut self, cur: &str) {
        let pending = self.layout.pending();
        let _ = self.layout.set_pending(None);
        let Some(p) = pending else {
            self.log("agent exited 75 without state\\pending; restarting it");
            return;
        };
        if p == cur {
            return;
        }
        match self.verify(&p) {
            Ok(()) => {
                // A fresh switch is on probation until the new version proves itself.
                let _ = std::fs::remove_file(self.layout.healthy_marker(&p));
                let _ = self.layout.set_previous(Some(cur));
                let _ = self.layout.set_current(&p);
                self.crashes.clear();
                self.log(&format!("switched {cur} -> {p}"));
            }
            Err(e) => {
                self.log(&format!("pending version {p} fails verification: {e}; staying on {cur}"));
                self.event("verify_failed", &p, Some(cur), &e);
                let _ = self.layout.mark_bad(&p);
            }
        }
    }

    /// The agent exited 76: it put a newer launcher in place as `ZeroFarm.exe`. Start it and wait
    /// for it to take over (true: we're done, exit). Otherwise put ourselves back (false: carry on).
    fn hand_over(&mut self) -> bool {
        let Some(swap) = self.layout.launcher_swap() else {
            self.log("agent exited 76 without state\\launcher-swap; restarting it");
            return false;
        };
        let root = self.layout.root.clone();
        let new_exe = self.layout.launcher_exe();
        let old_exe = root.join(&swap.old);
        if !is_launcher_leftover(&swap.old, false) || !old_exe.is_file() {
            self.log(&format!("state\\launcher-swap names {:?}, which is not there; restarting the agent", swap.old));
            let _ = self.layout.set_launcher_swap(None);
            return false;
        }
        let reason = if !new_exe.is_file() {
            "ZeroFarm.exe is missing".to_string()
        } else {
            self.log(&format!("handing over to launcher {} (this one is {}, now {})", swap.to, swap.from, swap.old));
            self.instance = None; // the new launcher takes the single-instance mutex
            let me = std::process::id().to_string();
            match spawn_outliving(&new_exe, &["--takeover", &me], &root) {
                Err(e) => format!("could not start: {e}"),
                Ok(mut child) => {
                    let ok = self.layout.launcher_ok_marker(child.id());
                    let deadline = Instant::now() + self.handover_timeout;
                    let outcome = loop {
                        if ok.exists() {
                            break None;
                        }
                        if let Ok(Some(st)) = child.try_wait() {
                            if ok.exists() {
                                break None;
                            }
                            break Some(format!("exited with code {} before taking over", st.code().unwrap_or(-1)));
                        }
                        if self.quit_requested() {
                            self.log("quit requested during the handover");
                            return true;
                        }
                        if Instant::now() >= deadline {
                            let _ = child.kill();
                            let _ = child.wait();
                            break Some(format!("did not take over within {} s", self.handover_timeout.as_secs()));
                        }
                        std::thread::sleep(Duration::from_millis(100));
                    };
                    match outcome {
                        None => {
                            remove_file_quiet(&ok);
                            let _ = self.layout.set_launcher_swap(None);
                            self.log(&format!("launcher {} (pid {}) took over; exiting", swap.to, child.id()));
                            return true;
                        }
                        Some(r) => r,
                    }
                }
            }
        };
        // Roll back: this launcher is ZeroFarm.exe again; the new one is set aside and marked bad.
        self.log(&format!("new launcher {}: {reason}; putting {} back", swap.to, swap.from));
        if new_exe.exists() {
            let bad = root.join(format!("ZeroFarm.bad-{}.exe", now_unix()));
            if let Err(e) = rename_retry(&new_exe, &bad) {
                self.log(&format!("could not set the new launcher aside: {e}"));
            }
        }
        if let Err(e) = rename_retry(&old_exe, &new_exe) {
            self.log(&format!("could not restore {}: {e}", swap.old));
        }
        let _ = self.layout.mark_launcher_bad(&swap.sha256);
        self.event("launcher_rollback", &swap.to, Some(&swap.from), &reason);
        let _ = self.layout.set_launcher_swap(None);
        self.leftovers_until = Some(Instant::now() + LEFTOVER_RETRY); // ZeroFarm.bad-<n>.exe
        self.instance = take_instance(&root, INSTANCE_WAIT);
        if self.instance.is_none() {
            self.log("another launcher holds this root; exiting");
            return true;
        }
        false
    }

    /// Returns false if a quit came in while backing off.
    fn handle_crash(&mut self, cur: &str, code: i32) -> bool {
        let now = Instant::now();
        while self.crashes.front().is_some_and(|t| now.duration_since(*t) > CRASH_WINDOW) {
            self.crashes.pop_front();
        }
        self.crashes.push_back(now);
        let n = self.crashes.len();
        self.log(&format!("{cur} exited with code {code} ({n} in the last 10 min)"));
        if n >= CRASHES_FOR_ROLLBACK
            && self.rollback(cur, &format!("exited non-zero {n} times within 10 minutes (last exit code {code})"))
        {
            return true;
        }
        let delay = Duration::from_secs((1u64 << (n.saturating_sub(1)).min(10)).min(600));
        self.sleep(delay)
    }

    fn run(&mut self) -> i32 {
        self.log(&format!(
            "start {}, root {}{}",
            env!("CARGO_PKG_VERSION"),
            self.layout.root.display(),
            self.takeover.map(|p| format!(", taking over from pid {p}")).unwrap_or_default()
        ));
        if self.keys.is_empty() {
            self.log("this launcher build trusts no release key; refusing to run anything");
            return 2;
        }
        if !self.cleanup_leftovers(self.takeover.is_none()) && self.takeover.is_none() {
            self.leftovers_until = Some(Instant::now() + LEFTOVER_RETRY);
        }
        loop {
            if self.quit_requested() {
                self.log("quit requested");
                return 0;
            }
            let Some(cur) = self.resolve_current() else {
                self.log("no verifiable agent version to run; giving up");
                return 2;
            };
            let probation = self.on_probation(&cur);
            let started = Instant::now();
            let mut child = match self.spawn(&cur) {
                Ok(c) => c,
                Err(e) => {
                    self.log(&format!("could not start {cur}: {e}"));
                    if !self.handle_crash(&cur, -1) {
                        return 0;
                    }
                    continue;
                }
            };
            self.log(&format!("started {cur} (pid {}){}", child.id(), if probation { " on probation" } else { "" }));
            match self.supervise(&mut child, &cur, probation, started) {
                Outcome::Exited(0) => {
                    self.confirm_takeover(); // the user quit: the new launcher did its job
                    self.log(&format!("{cur} exited 0 (quit); stopping"));
                    return 0;
                }
                Outcome::Exited(EXIT_UPDATE) => self.handle_update(&cur),
                Outcome::Exited(EXIT_RESTART_LAUNCHER) => {
                    if self.hand_over() {
                        return 0;
                    }
                }
                Outcome::Exited(code) => {
                    if started.elapsed() > CRASH_WINDOW {
                        self.crashes.clear();
                    }
                    if !self.handle_crash(&cur, code) {
                        return 0;
                    }
                }
                Outcome::NotHealthy => {
                    let secs = self.healthy_timeout.as_secs();
                    if !self.rollback(&cur, &format!("did not write healthy-{cur} within {secs} s")) {
                        // Nothing to go back to: keep it running, without probation.
                        let _ = self.layout.mark_healthy(&cur);
                    }
                }
                Outcome::Quit => {
                    self.log("quit requested; agent stopped");
                    return 0;
                }
            }
        }
    }
}

fn forward_uninstall(layout: &Layout, args: &[String]) -> i32 {
    let Some(cur) = layout.current() else { return 2 };
    let exe = layout.version_dir(&cur).join(AGENT_EXE);
    match Command::new(exe).arg("uninstall").arg("--root").arg(&layout.root).args(args).creation_flags(CREATE_NO_WINDOW).status() {
        Ok(st) => st.code().unwrap_or(1),
        Err(_) => 1,
    }
}

/// `ZeroFarm.exe selftest`: this binary starts, trusts a release key and verifies the current
/// agent version. Prints `ok <launcher version>` (exit 0) or the problem (exit 2).
fn selftest(layout: &Layout) -> i32 {
    let keys = TrustedKeys::compiled();
    let result = if keys.is_empty() {
        Err("this launcher build trusts no release key".to_string())
    } else {
        match layout.current() {
            None => Err("state\\current is missing".to_string()),
            Some(cur) => verify_version(layout, &keys, &cur).map_err(|e| format!("version {cur}: {e}")),
        }
    };
    match result {
        Ok(()) => {
            println!("ok {}", env!("CARGO_PKG_VERSION"));
            0
        }
        Err(e) => {
            println!("fail: {e}");
            2
        }
    }
}

/// Debug builds only: test overrides of the timeouts.
fn test_secs(name: &str, default: Duration) -> Duration {
    #[cfg(debug_assertions)]
    if let Some(s) = std::env::var(name).ok().and_then(|v| v.parse::<u64>().ok()) {
        return Duration::from_secs(s);
    }
    let _ = name;
    default
}

fn main() {
    let exe = std::env::current_exe().unwrap_or_default();
    let root: PathBuf = exe.parent().map(PathBuf::from).unwrap_or_default();
    let layout = Layout::new(root.clone());
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("uninstall") => std::process::exit(forward_uninstall(&layout, &args[1..])),
        Some("selftest") => std::process::exit(selftest(&layout)),
        _ => {}
    }
    let takeover = args.iter().position(|a| a == "--takeover").and_then(|i| args.get(i + 1)).and_then(|p| p.parse::<u32>().ok());
    // A launcher handing over to us released the mutex just before starting us.
    let Some(instance) = take_instance(&root, if takeover.is_some() { INSTANCE_WAIT } else { Duration::ZERO }) else {
        // Already running for this root.
        return;
    };
    let quit = QuitEvent::open(&root);
    if let Some(q) = &quit {
        if takeover.is_none() {
            q.reset();
        }
    }
    let mut l = Launcher {
        log: RotatingFile::new(layout.logs().join("launcher.log"), 512 * 1024, 1),
        layout,
        keys: TrustedKeys::compiled(),
        quit,
        job: job::Job::new(),
        healthy_timeout: test_secs("ZEROFARM_TEST_HEALTHY_TIMEOUT_S", HEALTHY_TIMEOUT),
        crashes: VecDeque::new(),
        instance: Some(instance),
        takeover,
        confirm_after: test_secs("ZEROFARM_TEST_TAKEOVER_CONFIRM_S", TAKEOVER_CONFIRM),
        handover_timeout: test_secs("ZEROFARM_TEST_HANDOVER_S", HANDOVER_TIMEOUT),
        leftovers_until: None,
        leftovers_next: Instant::now(),
    };
    std::process::exit(l.run());
}

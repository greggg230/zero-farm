//! The worker on macOS (PROTOCOL 11.5). No job objects: the worker gets its own process group, CPU work runs at
//! nice 19 (never the background QoS / `PRIO_DARWIN_BG`, which confines a process to the efficiency cores),
//! yield/resume are SIGSTOP/SIGCONT to the group, and the memory cap is the agent's: the group's physical footprint
//! is read at every `try_wait` (each tick of the lease loop, at most every 100 ms) and a group past its cap is killed
//! (its peak then reads at the cap, which the rolling memory plan takes as "ran into the limit", PROTOCOL 10.6). The
//! group is killed when the `WorkerProc` goes, and a group whose agent died is killed at the agent's next start
//! ([`kill_orphans`]): a worker stopped by SIGSTOP can't see its stdin close.

use super::{start_readers, SpawnOpts, WorkerMsg};
use serde_json::Value;
use std::collections::VecDeque;
use std::io::Write;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The files in the worker's working directory naming its process group (pid and start time), for [`kill_orphans`]:
/// `.zerofarm-worker-<pid>`, one per worker (rev 19: a crew's workers share the lease's directory, PROTOCOL 13.5).
const RECORD: &str = ".zerofarm-worker";
/// The memory cap is checked at most this often.
const MEM_EVERY: Duration = Duration::from_millis(100);
/// Nice values: CPU work, GPU work (none on macOS yet), tests (`ZEROFARM_TEST_WORKER_PRIORITY=normal`).
const NICE_CPU: i32 = 19;
const NICE_GPU: i32 = 10;

pub struct WorkerProc {
    child: Child,
    stdin: Option<ChildStdin>,
    pub pid: u32,
    /// The memory cap (MB; 0: none), enforced on the group's physical footprint.
    pub mem_limit_mb: u64,
    tail: Arc<Mutex<VecDeque<String>>>,
    suspended: bool,
    exited: bool,
    record: Option<PathBuf>,
    peak_mb: u64,
    mem_checked: Option<Instant>,
}

fn killpg(pgid: u32, sig: i32) -> bool {
    // SAFETY: kill(2) of our worker's own process group (its pgid is its pid; the group can't be reused while any of
    // its processes lives, and we only signal it until we reaped the leader).
    unsafe { libc::killpg(pgid as i32, sig) == 0 }
}

impl WorkerProc {
    /// Starts `<exe> <args>` (`serve`) in `cwd`, in a process group of its own, at its nice value. Every stdout line is
    /// delivered to `on_msg` from a reader thread (then `Eof` when the pipe closes).
    pub fn spawn(opts: SpawnOpts, on_msg: impl Fn(WorkerMsg) + Send + 'static) -> std::io::Result<WorkerProc> {
        let nice = if crate::test_worker_normal_priority() || opts.normal_priority {
            0
        } else if opts.cpu_job {
            NICE_CPU
        } else {
            NICE_GPU
        };
        let mut cmd = Command::new(opts.exe);
        if let Some(rt) = opts.runtime_dir {
            let mut joined = rt.as_os_str().to_owned();
            if let Some(old) = std::env::var_os("DYLD_LIBRARY_PATH").filter(|p| !p.is_empty()) {
                joined.push(":");
                joined.push(old);
            }
            cmd.env("DYLD_LIBRARY_PATH", joined).env("ZEROFARM_RUNTIME_DIR", rt);
        }
        cmd.args(opts.args).current_dir(opts.cwd).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).process_group(0);
        // SAFETY: setpriority(2) is async-signal-safe; it only lowers this child's own priority before exec.
        unsafe {
            cmd.pre_exec(move || {
                if nice > 0 {
                    libc::setpriority(libc::PRIO_PROCESS, 0, nice);
                }
                Ok(())
            });
        }
        let mut child = cmd.spawn()?;
        let pid = child.id();
        let record = opts.cwd.join(format!("{RECORD}-{pid}"));
        match crate::macos::proc_start(pid as i32) {
            Some((s, us)) => {
                if let Err(e) = std::fs::write(&record, format!("{pid} {s} {us}")) {
                    log::debug!("could not record worker {pid}: {e}");
                }
            }
            None => log::debug!("no start time for worker {pid}"),
        }
        let stdout = child.stdout.take().expect("piped");
        let stderr = child.stderr.take().expect("piped");
        let tail = start_readers(stdout, stderr, on_msg, opts.log.clone(), opts.tag.clone());
        let stdin = child.stdin.take();
        Ok(WorkerProc { child, stdin, pid, mem_limit_mb: opts.mem_limit_mb, tail, suspended: false, exited: false, record: Some(record), peak_mb: 0, mem_checked: None })
    }

    /// Sends one JSON line.
    pub fn send(&mut self, v: &Value) -> std::io::Result<()> {
        let stdin = self.stdin.as_mut().ok_or_else(|| std::io::Error::other("stdin closed"))?;
        let mut line = serde_json::to_vec(v).expect("json");
        line.push(b'\n');
        stdin.write_all(&line)?;
        stdin.flush()
    }

    pub fn suspend(&mut self) -> bool {
        let ok = !self.exited && killpg(self.pid, libc::SIGSTOP);
        if ok {
            self.suspended = true;
        }
        ok
    }

    pub fn resume(&mut self) -> bool {
        let ok = !self.exited && killpg(self.pid, libc::SIGCONT);
        if ok {
            self.suspended = false;
        }
        ok
    }

    pub fn is_suspended(&self) -> bool {
        self.suspended
    }

    fn reaped(&mut self) {
        if !self.exited {
            self.exited = true;
            // (anything the worker started dies with it, as in a job object)
            killpg(self.pid, libc::SIGKILL);
            if let Some(r) = self.record.take() {
                let _ = std::fs::remove_file(r);
            }
        }
    }

    /// The memory cap: a group whose physical footprint is past it is killed (logged; its peak is the footprint it
    /// was killed at).
    fn watch_memory(&mut self) {
        if self.exited || self.mem_limit_mb == 0 || self.mem_checked.is_some_and(|t| t.elapsed() < MEM_EVERY) {
            return;
        }
        self.mem_checked = Some(Instant::now());
        let Some(now) = self.footprint_mb() else { return };
        if now > self.mem_limit_mb {
            log::warn!("worker pid {}: {now} MB physical footprint, over its {} MB memory cap: killing it", self.pid, self.mem_limit_mb);
            killpg(self.pid, libc::SIGKILL);
        }
    }

    /// The group's physical footprint now (MB), and the peak updated.
    fn footprint_mb(&mut self) -> Option<u64> {
        if self.exited {
            return None;
        }
        let mut pids = crate::macos::group_pids(self.pid as i32);
        if !pids.contains(&(self.pid as i32)) {
            pids.push(self.pid as i32);
        }
        let mut total = 0u64;
        let mut any = false;
        for p in pids {
            if let Some(b) = crate::macos::proc_footprint(p) {
                total += b;
                any = true;
            }
        }
        let mb = total >> 20;
        any.then(|| {
            self.peak_mb = self.peak_mb.max(mb);
            mb
        })
    }

    /// Exit code if it has exited (-1 for a signal); otherwise checks the memory cap.
    pub fn try_wait(&mut self) -> Option<i32> {
        match self.child.try_wait() {
            Ok(Some(s)) => {
                self.reaped();
                Some(s.code().unwrap_or(-1))
            }
            _ => {
                self.watch_memory();
                None
            }
        }
    }

    /// Kills the worker (and anything it started) now.
    pub fn kill(&mut self) {
        if !self.exited {
            killpg(self.pid, libc::SIGKILL);
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        self.reaped();
    }

    /// Waits up to `d` for exit.
    pub fn wait_timeout(&mut self, d: Duration) -> Option<i32> {
        let end = Instant::now() + d;
        loop {
            if let Some(c) = self.try_wait() {
                return Some(c);
            }
            if Instant::now() >= end {
                return None;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// CPU time (user + system) the worker process has used, in ms.
    pub fn cpu_ms(&self) -> u64 {
        if self.exited {
            return 0;
        }
        crate::macos::proc_cpu_ms(self.pid as i32).unwrap_or(0)
    }

    /// The worker group's physical footprint now (MB): what the memory cap counts. None once it has exited.
    pub fn commit_mb(&mut self) -> Option<u64> {
        self.footprint_mb()
    }

    /// The most physical footprint the group was seen with (MB): a worker killed at its memory cap shows a peak past
    /// the cap even after it died.
    pub fn job_peak_mb(&self) -> Option<u64> {
        Some(self.peak_mb)
    }

    pub fn stderr_tail(&self) -> String {
        self.tail.lock().unwrap().iter().cloned().collect::<Vec<_>>().join("\n")
    }
}

impl Drop for WorkerProc {
    fn drop(&mut self) {
        if self.try_wait().is_none() {
            self.kill();
        }
    }
}

/// At the agent's start, before `work/` is cleared: kills every worker group a previous agent left behind (its record
/// in a working directory names a process that is still the one it started: same pid, same start time).
pub fn kill_orphans(work: &Path) -> usize {
    let Ok(rd) = std::fs::read_dir(work) else { return 0 };
    let mut n = 0;
    for e in rd.flatten() {
        // (every worker of the lease: `.zerofarm-worker-<pid>`; agents before 0.3.1 wrote `.zerofarm-worker`)
        let Ok(files) = std::fs::read_dir(e.path()) else { continue };
        for f in files.flatten().filter(|f| f.file_name().to_string_lossy().starts_with(RECORD)) {
            let Ok(text) = std::fs::read_to_string(f.path()) else { continue };
            let v: Vec<u64> = text.split_whitespace().filter_map(|x| x.parse().ok()).collect();
            let [pid, s, us] = v[..] else { continue };
            if pid > 1 && crate::macos::proc_start(pid as i32) == Some((s, us)) {
                log::warn!("killing worker pid {pid} left by an earlier agent");
                killpg(pid as u32, libc::SIGKILL);
                n += 1;
            }
        }
    }
    n
}

/// agent 0.3.9: CPU time (user + system) this agent's own process has used, ms (its uploads, pings and logs are the
/// job's work, `tune::LoadMark`).
pub fn self_cpu_ms() -> u64 {
    crate::macos::proc_cpu_ms(std::process::id() as i32).unwrap_or(0)
}

/// agent 0.3.10: the system's interrupt time so far, ms - not told apart on macOS: 0 (`tune::LoadMark`).
pub fn sys_dpc_ms() -> u64 {
    0
}

/// rev 19 (PROTOCOL 13): the system's CPU time so far over all cores, ms: (idle, total).
pub fn sys_cpu_ms() -> (u64, u64) {
    let (idle, total) = crate::macos::cpu_ticks();
    // SAFETY: sysconf with a valid name.
    let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) }.max(1) as u64;
    (idle * 1000 / hz, total * 1000 / hz)
}

/// A point in time for starvation checks: the worker's CPU time and the system's idle/total CPU ticks.
#[derive(Debug, Clone, Copy)]
pub struct CpuMark {
    worker_ms: u64,
    sys_idle: u64,
    sys_total: u64,
}

impl CpuMark {
    pub fn take(w: &WorkerProc) -> CpuMark {
        let (sys_idle, sys_total) = crate::macos::cpu_ticks();
        CpuMark { worker_ms: w.cpu_ms(), sys_idle, sys_total }
    }

    /// Since `self`: the worker got (almost) no CPU while the system had (almost) no idle time - it is starved by
    /// other programs, not hung.
    pub fn starved_since(&self, w: &WorkerProc) -> bool {
        let now = CpuMark::take(w);
        let worker = now.worker_ms.saturating_sub(self.worker_ms);
        let total = now.sys_total.saturating_sub(self.sys_total).max(1);
        let idle = now.sys_idle.saturating_sub(self.sys_idle);
        worker < 100 && (idle as f64 / total as f64) < 0.10
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// rev 19 (PROTOCOL 13.5): a crew's workers share the lease's directory; every one a dead agent left is killed.
    #[test]
    fn every_worker_of_a_lease_left_behind_is_killed() {
        let work = tempfile::tempdir().unwrap();
        let lease = work.path().join("l_ORPHANS");
        std::fs::create_dir_all(&lease).unwrap();
        let mut kids = Vec::new();
        for _ in 0..2 {
            let c = Command::new("sleep").arg("30").process_group(0).spawn().unwrap();
            let pid = c.id();
            let (s, us) = crate::macos::proc_start(pid as i32).expect("start time");
            std::fs::write(lease.join(format!("{RECORD}-{pid}")), format!("{pid} {s} {us}")).unwrap();
            kids.push(c);
        }
        assert_eq!(kill_orphans(work.path()), 2);
        for mut c in kids {
            let status = c.wait().unwrap();
            assert!(!status.success(), "killed: {status:?}");
        }
    }
}

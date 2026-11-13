//! Running one worker process (PROTOCOL 5, aw/1).
//!
//! The worker is created suspended, put in a job object (killed when the agent goes away,
//! memory-capped, no crash dialogs), given IDLE_PRIORITY_CLASS and EcoQoS for CPU work, and only
//! then allowed to run. Yield/resume use NtSuspendProcess/NtResumeProcess (the whole process,
//! all threads). stdout carries JSON lines (bounded length); stderr goes to the worker log.

use serde_json::Value;
use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::windows::io::AsRawHandle;
use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, Mutex};
use windows_sys::Win32::Foundation::HANDLE;
use zerofarm_common::names::Handle;
use zerofarm_common::rotlog::RotatingFile;

/// Longest stdout line accepted from a worker.
pub const MAX_LINE: usize = 1 << 20;
const STDERR_TAIL: usize = 60;

#[derive(Debug, Clone)]
pub enum WorkerMsg {
    Line(Value),
    BadLine(String),
    Eof,
}

pub struct SpawnOpts<'a> {
    pub exe: &'a Path,
    /// The command line (`["serve"]` for a lease; the provider probe's own).
    pub args: &'a [&'a str],
    pub cwd: &'a Path,
    /// Job memory limit (whole process tree).
    pub mem_limit_mb: u64,
    /// CPU work: IDLE_PRIORITY_CLASS + EcoQoS. GPU work: BELOW_NORMAL, no EcoQoS.
    pub cpu_job: bool,
    pub log: Arc<RotatingFile>,
    pub tag: String,
    /// A verified runtime pack directory: put first on the worker's DLL search path and passed
    /// as `ZEROFARM_RUNTIME_DIR`.
    pub runtime_dir: Option<&'a Path>,
}

pub struct WorkerProc {
    child: Child,
    job: Handle,
    stdin: Option<ChildStdin>,
    pub pid: u32,
    tail: Arc<Mutex<VecDeque<String>>>,
    suspended: bool,
}

type NtProcFn = unsafe extern "system" fn(HANDLE) -> i32;

fn ntdll_fn(name: &[u8]) -> Option<NtProcFn> {
    use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
    unsafe {
        let m = GetModuleHandleW(crate::util::wide("ntdll.dll").as_ptr());
        if m.is_null() {
            return None;
        }
        let p = GetProcAddress(m, name.as_ptr())?;
        Some(std::mem::transmute::<unsafe extern "system" fn() -> isize, NtProcFn>(p))
    }
}

fn nt_suspend(h: HANDLE) -> bool {
    ntdll_fn(b"NtSuspendProcess\0").map(|f| unsafe { f(h) } >= 0).unwrap_or(false)
}

fn nt_resume(h: HANDLE) -> bool {
    ntdll_fn(b"NtResumeProcess\0").map(|f| unsafe { f(h) } >= 0).unwrap_or(false)
}

fn make_job(mem_limit_mb: u64) -> std::io::Result<Handle> {
    use windows_sys::Win32::System::JobObjects::*;
    unsafe {
        let h = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if h.is_null() {
            return Err(std::io::Error::last_os_error());
        }
        let job = Handle(h);
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION;
        if mem_limit_mb > 0 {
            info.BasicLimitInformation.LimitFlags |= JOB_OBJECT_LIMIT_JOB_MEMORY;
            info.JobMemoryLimit = (mem_limit_mb as usize) << 20;
        }
        if SetInformationJobObject(
            h,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const core::ffi::c_void,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        ) == 0
        {
            return Err(std::io::Error::last_os_error());
        }
        // A worker has no UI: no clipboard, global atoms, desktop switching, system settings or
        // logoff (DESIGN 4; the AppContainer sandbox is the next step, README "Not done yet").
        // USER handles stay allowed for GPU drivers.
        let ui = JOBOBJECT_BASIC_UI_RESTRICTIONS {
            UIRestrictionsClass: JOB_OBJECT_UILIMIT_READCLIPBOARD
                | JOB_OBJECT_UILIMIT_WRITECLIPBOARD
                | JOB_OBJECT_UILIMIT_GLOBALATOMS
                | JOB_OBJECT_UILIMIT_DESKTOP
                | JOB_OBJECT_UILIMIT_DISPLAYSETTINGS
                | JOB_OBJECT_UILIMIT_SYSTEMPARAMETERS
                | JOB_OBJECT_UILIMIT_EXITWINDOWS,
        };
        if SetInformationJobObject(
            h,
            JobObjectBasicUIRestrictions,
            &ui as *const _ as *const core::ffi::c_void,
            std::mem::size_of::<JOBOBJECT_BASIC_UI_RESTRICTIONS>() as u32,
        ) == 0
        {
            return Err(std::io::Error::last_os_error());
        }
        Ok(job)
    }
}

/// EcoQoS: ask the scheduler to prefer efficiency cores and low clocks for this process.
fn set_eco_qos(h: HANDLE) -> bool {
    use windows_sys::Win32::System::Threading::{
        ProcessPowerThrottling, SetProcessInformation, PROCESS_POWER_THROTTLING_CURRENT_VERSION,
        PROCESS_POWER_THROTTLING_EXECUTION_SPEED, PROCESS_POWER_THROTTLING_STATE,
    };
    let state = PROCESS_POWER_THROTTLING_STATE {
        Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
        ControlMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
        StateMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
    };
    unsafe {
        SetProcessInformation(
            h,
            ProcessPowerThrottling,
            &state as *const _ as *const core::ffi::c_void,
            std::mem::size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
        ) != 0
    }
}

impl WorkerProc {
    /// Starts `<exe> <args>` (`serve`) in `cwd`. Every stdout line is delivered to `on_msg` from a reader
    /// thread (then `Eof` when the pipe closes).
    pub fn spawn(opts: SpawnOpts, on_msg: impl Fn(WorkerMsg) + Send + 'static) -> std::io::Result<WorkerProc> {
        use windows_sys::Win32::System::Threading::{
            BELOW_NORMAL_PRIORITY_CLASS, CREATE_NO_WINDOW, CREATE_SUSPENDED, IDLE_PRIORITY_CLASS,
        };
        let job = make_job(opts.mem_limit_mb)?;
        let normal_for_tests = crate::test_worker_normal_priority();
        let prio = if normal_for_tests {
            windows_sys::Win32::System::Threading::NORMAL_PRIORITY_CLASS
        } else if opts.cpu_job {
            IDLE_PRIORITY_CLASS
        } else {
            BELOW_NORMAL_PRIORITY_CLASS
        };
        let mut cmd = Command::new(opts.exe);
        if let Some(rt) = opts.runtime_dir {
            let path = std::env::var_os("PATH").unwrap_or_default();
            let mut joined = rt.as_os_str().to_owned();
            joined.push(";");
            joined.push(path);
            cmd.env("PATH", joined).env("ZEROFARM_RUNTIME_DIR", rt);
        }
        let mut child = cmd
            .args(opts.args)
            .current_dir(opts.cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .creation_flags(CREATE_NO_WINDOW | CREATE_SUSPENDED | prio)
            .spawn()?;
        let h = child.as_raw_handle() as HANDLE;
        let assigned = unsafe { windows_sys::Win32::System::JobObjects::AssignProcessToJobObject(job.0, h) } != 0;
        if !assigned {
            let e = std::io::Error::last_os_error();
            let _ = child.kill();
            return Err(e);
        }
        if opts.cpu_job && !normal_for_tests && !set_eco_qos(h) {
            log::debug!("EcoQoS not available on this Windows version");
        }
        let pid = child.id();
        let stdout = child.stdout.take().expect("piped");
        let stderr = child.stderr.take().expect("piped");
        let tail = Arc::new(Mutex::new(VecDeque::new()));
        std::thread::Builder::new()
            .name("worker-stdout".into())
            .spawn(move || read_lines(stdout, on_msg))
            .expect("thread");
        let (tail2, log, tag) = (tail.clone(), opts.log.clone(), opts.tag.clone());
        std::thread::Builder::new()
            .name("worker-stderr".into())
            .spawn(move || {
                let r = BufReader::new(stderr);
                for line in r.split(b'\n') {
                    let Ok(line) = line else { break };
                    let line = String::from_utf8_lossy(&line).trim_end().to_string();
                    log.write_line(&format!("{} [{tag}] {line}", zerofarm_common::rotlog::utc_timestamp(zerofarm_common::now_unix())));
                    let mut t = tail2.lock().unwrap();
                    if t.len() >= STDERR_TAIL {
                        t.pop_front();
                    }
                    t.push_back(line);
                }
            })
            .expect("thread");
        if !nt_resume(h) {
            let _ = child.kill();
            return Err(std::io::Error::other("could not start the worker's main thread"));
        }
        let stdin = child.stdin.take();
        Ok(WorkerProc { child, job, stdin, pid, tail, suspended: false })
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
        let ok = nt_suspend(self.child.as_raw_handle() as HANDLE);
        if ok {
            self.suspended = true;
        }
        ok
    }

    pub fn resume(&mut self) -> bool {
        let ok = nt_resume(self.child.as_raw_handle() as HANDLE);
        if ok {
            self.suspended = false;
        }
        ok
    }

    pub fn is_suspended(&self) -> bool {
        self.suspended
    }

    /// Exit code if it has exited.
    pub fn try_wait(&mut self) -> Option<i32> {
        match self.child.try_wait() {
            Ok(Some(s)) => Some(s.code().unwrap_or(-1)),
            _ => None,
        }
    }

    /// Kills the worker (and anything it started) now.
    pub fn kill(&mut self) {
        unsafe { windows_sys::Win32::System::JobObjects::TerminateJobObject(self.job.0, 0xDEAD) };
        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    /// Waits up to `d` for exit.
    pub fn wait_timeout(&mut self, d: std::time::Duration) -> Option<i32> {
        let end = std::time::Instant::now() + d;
        loop {
            if let Some(c) = self.try_wait() {
                return Some(c);
            }
            if std::time::Instant::now() >= end {
                return None;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    /// CPU time (user + kernel) the worker process has used, in ms.
    pub fn cpu_ms(&self) -> u64 {
        use windows_sys::Win32::Foundation::FILETIME;
        use windows_sys::Win32::System::Threading::GetProcessTimes;
        let z = || FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
        let (mut c, mut e, mut k, mut u) = (z(), z(), z(), z());
        let ok = unsafe { GetProcessTimes(self.child.as_raw_handle() as HANDLE, &mut c, &mut e, &mut k, &mut u) };
        if ok == 0 {
            return 0;
        }
        (ft(&k) + ft(&u)) / 10_000
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

fn ft(t: &windows_sys::Win32::Foundation::FILETIME) -> u64 {
    ((t.dwHighDateTime as u64) << 32) | t.dwLowDateTime as u64
}

/// A point in time for starvation checks: the worker's CPU time and the system's idle/total time.
#[derive(Debug, Clone, Copy)]
pub struct CpuMark {
    worker_ms: u64,
    sys_idle: u64,
    sys_total: u64,
}

impl CpuMark {
    pub fn take(w: &WorkerProc) -> CpuMark {
        use windows_sys::Win32::Foundation::FILETIME;
        use windows_sys::Win32::System::Threading::GetSystemTimes;
        let z = || FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
        let (mut i, mut k, mut u) = (z(), z(), z());
        unsafe { GetSystemTimes(&mut i, &mut k, &mut u) };
        // Kernel time includes idle time.
        CpuMark { worker_ms: w.cpu_ms(), sys_idle: ft(&i), sys_total: ft(&k) + ft(&u) }
    }

    /// Since `self`: the worker got (almost) no CPU while the system had (almost) no idle time —
    /// it is starved by other programs, not hung.
    pub fn starved_since(&self, w: &WorkerProc) -> bool {
        let now = CpuMark::take(w);
        let worker = now.worker_ms.saturating_sub(self.worker_ms);
        let total = now.sys_total.saturating_sub(self.sys_total).max(1);
        let idle = now.sys_idle.saturating_sub(self.sys_idle);
        worker < 100 && (idle as f64 / total as f64) < 0.10
    }
}

fn read_lines(stdout: impl Read, on_msg: impl Fn(WorkerMsg)) {
    let mut r = BufReader::new(stdout);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        match (&mut r).take(MAX_LINE as u64 + 1).read_until(b'\n', &mut buf) {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                if buf.last() != Some(&b'\n') && buf.len() > MAX_LINE {
                    on_msg(WorkerMsg::BadLine(format!("line longer than {MAX_LINE} bytes")));
                    // Skip the rest of the line.
                    let mut sink = Vec::new();
                    if r.read_until(b'\n', &mut sink).unwrap_or(0) == 0 {
                        break;
                    }
                    continue;
                }
                let text = String::from_utf8_lossy(&buf);
                let t = text.trim();
                if t.is_empty() {
                    continue;
                }
                match serde_json::from_str::<Value>(t) {
                    Ok(v) if v.is_object() => on_msg(WorkerMsg::Line(v)),
                    _ => on_msg(WorkerMsg::BadLine(t.chars().take(200).collect())),
                }
            }
        }
    }
    on_msg(WorkerMsg::Eof);
}

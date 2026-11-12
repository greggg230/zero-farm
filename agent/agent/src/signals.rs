//! Sampling the Windows signals the policy needs (DESIGN 3.2). Only numbers and yes/no answers
//! are produced: no process, window or file names are ever read.

use crate::policy::{GpuSignal, Notification, Signals};
use crate::worker::{CpuMark, WorkerProc};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

pub trait SignalSource: Send {
    /// `worker_pid`: our own worker, excluded from "other GPU users".
    fn sample(&mut self, worker_pid: Option<u32>) -> Signals;

    /// Since `mark`, was the worker starved of CPU: it got almost none while the PC had almost no
    /// idle time (other programs), so it is waiting for a turn, not hung.
    fn worker_starved(&self, mark: &CpuMark, worker: &WorkerProc) -> bool {
        mark.starved_since(worker)
    }
}

/// The real thing.
pub struct Win32Signals {
    nvml: Option<nvml_wrapper::Nvml>,
    last_util_ts: u64,
}

impl Win32Signals {
    /// NVML is loaded only if the NVIDIA driver provides it (`nvml.dll`); otherwise GPU signals
    /// are simply absent.
    pub fn new(use_nvml: bool) -> Self {
        let nvml = if use_nvml {
            match nvml_wrapper::Nvml::init() {
                Ok(n) => Some(n),
                Err(e) => {
                    log::info!("NVML not available ({e}); no NVIDIA GPU signals");
                    None
                }
            }
        } else {
            None
        };
        Win32Signals { nvml, last_util_ts: 0 }
    }

    pub fn has_nvml(&self) -> bool {
        self.nvml.is_some()
    }

    fn gpu(&mut self, worker_pid: Option<u32>) -> Option<GpuSignal> {
        use nvml_wrapper::enum_wrappers::device::TemperatureSensor;
        let nvml = self.nvml.as_ref()?;
        let count = nvml.device_count().ok()?;
        if count == 0 {
            return None;
        }
        let mut out = GpuSignal::default();
        let mut newest_ts = self.last_util_ts;
        for i in 0..count {
            let Ok(dev) = nvml.device_by_index(i) else { continue };
            if let Ok(t) = dev.temperature(TemperatureSensor::Gpu) {
                out.temp_c = Some(out.temp_c.map_or(t, |c: u32| c.max(t)));
            }
            let other = match dev.process_utilization_stats(self.last_util_ts) {
                Ok(samples) => {
                    // Latest sample per pid, excluding our worker.
                    let mut per_pid: std::collections::HashMap<u32, (u64, u32)> = Default::default();
                    for s in samples {
                        newest_ts = newest_ts.max(s.timestamp);
                        if Some(s.pid) == worker_pid {
                            continue;
                        }
                        let e = per_pid.entry(s.pid).or_insert((0, 0));
                        if s.timestamp >= e.0 {
                            *e = (s.timestamp, s.sm_util);
                        }
                    }
                    per_pid.values().map(|(_, u)| *u).sum::<u32>()
                }
                // Per-process stats unsupported (or no samples yet): whole-GPU utilisation, which
                // is "other" whenever our worker is not a GPU job.
                Err(_) => dev.utilization_rates().map(|u| u.gpu).unwrap_or(0),
            };
            out.other_util_pct = out.other_util_pct.max(other.min(100));
        }
        self.last_util_ts = newest_ts;
        Some(out)
    }
}

impl SignalSource for Win32Signals {
    fn sample(&mut self, worker_pid: Option<u32>) -> Signals {
        let (_, sod) = crate::util::local_now();
        let (mem_avail_mb, mem_total_mb) = memory_mb();
        let (on_ac, battery_saver) = power();
        Signals {
            now: zerofarm_common::now_unix(),
            local_min: sod / 60,
            idle_s: idle_seconds(),
            notification: notification_state(),
            on_ac,
            battery_saver,
            mem_avail_mb,
            mem_total_mb,
            gpu: self.gpu(worker_pid),
        }
    }
}

pub fn idle_seconds() -> u64 {
    use windows_sys::Win32::System::SystemInformation::GetTickCount;
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};
    let mut lii = LASTINPUTINFO { cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32, dwTime: 0 };
    if unsafe { GetLastInputInfo(&mut lii) } == 0 {
        return 0;
    }
    let now = unsafe { GetTickCount() };
    (now.wrapping_sub(lii.dwTime) / 1000) as u64
}

pub fn notification_state() -> Notification {
    use windows_sys::Win32::UI::Shell::SHQueryUserNotificationState;
    let mut st = 0i32;
    if unsafe { SHQueryUserNotificationState(&mut st) } != 0 {
        return Notification::Unknown;
    }
    match st {
        1 => Notification::NotPresent,
        2 => Notification::Busy,
        3 => Notification::D3dFullscreen,
        4 => Notification::Presentation,
        5 => Notification::AcceptsNotifications,
        6 => Notification::QuietTime,
        7 => Notification::App,
        _ => Notification::Unknown,
    }
}

/// (on AC: Some/None when unknown, battery saver on)
pub fn power() -> (Option<bool>, bool) {
    use windows_sys::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};
    let mut s: SYSTEM_POWER_STATUS = unsafe { std::mem::zeroed() };
    if unsafe { GetSystemPowerStatus(&mut s) } == 0 {
        return (None, false);
    }
    let on_ac = match s.ACLineStatus {
        0 => Some(false),
        1 => Some(true),
        _ => None,
    };
    (on_ac, s.SystemStatusFlag == 1)
}

/// (available, total) physical memory in MB.
pub fn memory_mb() -> (u64, u64) {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    let mut m: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
    m.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
    if unsafe { GlobalMemoryStatusEx(&mut m) } == 0 {
        return (0, 0);
    }
    (m.ullAvailPhys / (1 << 20), m.ullTotalPhys / (1 << 20))
}

/// A signal source the tests (and `--simulate`) set by hand. `now` is filled in on each sample
/// unless `frozen_now` is set.
#[derive(Clone)]
pub struct SharedSignals {
    pub inner: Arc<Mutex<Signals>>,
    pub frozen_now: bool,
    /// Tests: the worker counts as starved of CPU (as well as when it really is).
    pub starved: Arc<AtomicBool>,
}

impl SharedSignals {
    pub fn new(s: Signals) -> Self {
        SharedSignals { inner: Arc::new(Mutex::new(s)), frozen_now: false, starved: Arc::new(AtomicBool::new(false)) }
    }
    pub fn set_starved(&self, starved: bool) {
        self.starved.store(starved, Ordering::SeqCst);
    }
    pub fn set(&self, f: impl FnOnce(&mut Signals)) {
        f(&mut self.inner.lock().unwrap());
    }
}

impl SignalSource for SharedSignals {
    fn sample(&mut self, _worker_pid: Option<u32>) -> Signals {
        let mut s = self.inner.lock().unwrap().clone();
        if !self.frozen_now {
            s.now = zerofarm_common::now_unix();
        }
        s
    }
    fn worker_starved(&self, mark: &CpuMark, worker: &WorkerProc) -> bool {
        self.starved.load(Ordering::SeqCst) || mark.starved_since(worker)
    }
}

//! Sampling the Windows signals the policy needs (DESIGN 3.2). Only numbers and yes/no answers
//! are produced: no process, window or file names are ever read.
//!
//! The GPU signals are vendor-neutral (PROTOCOL 8, DESIGN 8.5):
//! - **Other use**: the PDH counters `\GPU Engine(*)\Utilization Percentage`, one instance per
//!   process and engine (what Task Manager shows). Per engine the processes other than ours (the
//!   worker and the agent) are summed, per adapter the busiest engine counts, and the signal is the
//!   busiest adapter. Instance names carry only pids and adapter/engine numbers.
//! - **Temperature**: `D3DKMTQueryAdapterInfo(KMTQAITYPE_ADAPTERPERFDATA)` per adapter (Task
//!   Manager's "GPU temperature"). An adapter whose driver reports 0 or fails has no reading, and
//!   no reading means no temperature rule.
//!
//! NVML is not part of the gate; profile.rs asks it only for the NVIDIA driver version.

use crate::policy::{GpuSignal, Signals};
#[cfg(windows)]
use crate::policy::Notification;
#[cfg(windows)]
use crate::profile::GpuAdapter;
use crate::worker::{CpuMark, WorkerProc};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
#[cfg(windows)]
use std::time::Instant;

/// macOS (PROTOCOL 11.5): IOKit input idle time and power source, Low Power Mode, memory pressure; no GPU, no
/// fullscreen detection; the power assertion that keeps a working Mac awake.
#[cfg(target_os = "macos")]
#[path = "signals_macos.rs"]
mod mac;
#[cfg(target_os = "macos")]
pub use mac::{memory_mb, power, MacSignals};
/// The real signal source of this platform.
#[cfg(windows)]
pub type OsSignals = Win32Signals;
#[cfg(target_os = "macos")]
pub type OsSignals = MacSignals;

pub trait SignalSource: Send {
    /// `worker_pids`: our own workers (agent 0.3.4: every worker of a lease's crew), excluded from "other GPU users".
    fn sample(&mut self, worker_pids: &[u32]) -> Signals;

    /// agent 0.3.4: the dedicated video memory on adapter `luid` (MB): in use by every program, and by each of `pids`
    /// that holds some (Task Manager's "Dedicated GPU memory"). None: not measured here (no such counters, no GPU, not
    /// Windows).
    fn gpu_memory(&self, _luid: u64, _pids: &[u32]) -> Option<GpuMemory> {
        None
    }

    /// (available, total) physical memory in MB: what a worker's memory cap is worked out from (`Engine::caps`).
    fn memory_mb(&self) -> (u64, u64) {
        memory_mb()
    }

    /// Since `mark`, was the worker starved of CPU: it got almost none while the PC had almost no
    /// idle time (other programs), so it is waiting for a turn, not hung.
    fn worker_starved(&self, mark: &CpuMark, worker: &WorkerProc) -> bool {
        mark.starved_since(worker)
    }

    /// The engine holds a lease (true) or not (false): macOS keeps a working Mac on AC power from idle-sleeping
    /// meanwhile (`keep_awake`, PROTOCOL 11.5). Nothing elsewhere.
    fn lease_active(&mut self, _active: bool) {}

    /// rev 19 (PROTOCOL 13): the share of the CPUs other programs used (`measured`: from the system's CPU times minus
    /// our workers', `worker::LoadMark`). Tests set their own.
    fn other_load(&self, measured: f64) -> f64 {
        measured
    }
}

/// agent 0.3.4: an adapter's dedicated video memory in use (MB): by every program, and by some processes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GpuMemory {
    pub adapter_used_mb: u64,
    pub per_pid: Vec<(u32, u64)>,
}

impl GpuMemory {
    /// What `pid` holds (0: nothing measured).
    pub fn of(&self, pid: u32) -> u64 {
        self.per_pid.iter().filter(|(p, _)| *p == pid).map(|(_, mb)| *mb).sum()
    }
}

/// The GPUs are measured at most this often; `sample` reuses the last measurement in between
/// (the lease loop samples every 200 ms, and a PDH collection over every process's GPU engines
/// is not free). A PDH rate is the average since the previous collection.
pub const GPU_MEASURE_EVERY: Duration = Duration::from_secs(1);
/// The adapter list is re-read this often: a driver update or restart gives an adapter a new LUID,
/// and an external GPU can come and go.
pub const ADAPTER_REFRESH: Duration = Duration::from_secs(60);

#[cfg(windows)]
/// The real thing.
pub struct Win32Signals {
    gpu: Option<GpuSampler>,
    /// agent 0.3.4: the GPU memory counters (opened at the first `gpu_memory`; None inside: Windows has none), and the
    /// last collection (at most one per `GPU_MEASURE_EVERY`).
    mem: Mutex<Option<GpuMemSampler>>,
}

#[cfg(windows)]
struct GpuMemSampler {
    pdh: Option<PdhMemory>,
    at: Option<std::time::Instant>,
    last: Vec<MemUse>,
}

#[cfg(windows)]
impl Win32Signals {
    /// `gpu`: measure the GPUs (false: report none, as on a PC without one).
    pub fn new(gpu: bool) -> Self {
        Win32Signals { gpu: gpu.then(GpuSampler::new), mem: Mutex::new(None) }
    }

    /// Whether Windows has the GPU Engine counters (without them other programs' GPU use reads 0).
    pub fn has_busy_counters(&self) -> bool {
        self.gpu.as_ref().is_some_and(|g| g.pdh.is_some())
    }

    /// The per-adapter readings behind the last [`SignalSource::sample`] (diagnostics); empty
    /// without a hardware GPU.
    pub fn gpu_readings(&self) -> &[AdapterReading] {
        self.gpu.as_ref().map(|g| g.readings.as_slice()).unwrap_or(&[])
    }
}

#[cfg(windows)]
impl SignalSource for Win32Signals {
    fn gpu_memory(&self, luid: u64, pids: &[u32]) -> Option<GpuMemory> {
        self.gpu.as_ref()?;
        let mut g = self.mem.lock().unwrap();
        let m = g.get_or_insert_with(|| {
            let pdh = PdhMemory::open().map_err(|st| log::info!("no GPU memory counters (PDH 0x{st:08x}): the video memory in use is not measured")).ok();
            GpuMemSampler { pdh, at: None, last: Vec::new() }
        });
        let pdh = m.pdh.as_mut()?;
        if m.at.is_none_or(|t| t.elapsed() >= GPU_MEASURE_EVERY) {
            m.at = Some(std::time::Instant::now());
            m.last = pdh.collect();
        }
        gpu_memory_of(&m.last, luid, pids)
    }

    fn sample(&mut self, worker_pids: &[u32]) -> Signals {
        let (_, sod) = crate::util::local_now();
        let (mem_avail_mb, mem_total_mb) = memory_mb();
        let (on_ac, battery_saver) = power();
        let ours: Vec<u32> = worker_pids.iter().copied().chain([std::process::id()]).collect();
        Signals {
            now: zerofarm_common::now_unix(),
            local_min: sod / 60,
            idle_s: idle_seconds(),
            notification: notification_state(),
            on_ac,
            battery_saver,
            mem_avail_mb,
            mem_total_mb,
            gpu: self.gpu.as_mut().and_then(|g| g.sample(&ours)),
        }
    }
}

/// One adapter's reading.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdapterReading {
    pub luid: u64,
    pub vendor: String,
    pub name: String,
    /// Other programs' use, percent 0..=100 (its busiest engine).
    pub other_util_pct: u32,
    /// °C; `None` = the driver gives no temperature.
    pub temp_c: Option<u32>,
}

/// The policy's view of the adapters: the busiest one, and the hottest one with a reading.
pub fn gpu_signal(readings: &[AdapterReading]) -> GpuSignal {
    GpuSignal {
        other_util_pct: readings.iter().map(|r| r.other_util_pct).max().unwrap_or(0).min(100),
        temp_c: readings.iter().filter_map(|r| r.temp_c).max(),
    }
}

#[cfg(windows)]
struct GpuSampler {
    adapters: Vec<GpuAdapter>,
    adapters_at: Instant,
    /// `None`: Windows has no GPU Engine counters (logged once), or there is no GPU.
    pdh: Option<PdhEngines>,
    /// The last PDH collection.
    engines: Vec<EngineUse>,
    /// Per adapter LUID, the last temperature.
    temps: BTreeMap<u64, Option<u32>>,
    measured_at: Option<Instant>,
    readings: Vec<AdapterReading>,
}

#[cfg(windows)]
impl GpuSampler {
    fn new() -> Self {
        let adapters = crate::profile::adapters();
        log::info!("GPU adapters: {}", describe_adapters(&adapters));
        let pdh = if adapters.is_empty() { None } else { open_pdh() };
        GpuSampler {
            adapters,
            adapters_at: Instant::now(),
            pdh,
            engines: Vec::new(),
            temps: BTreeMap::new(),
            measured_at: None,
            readings: Vec::new(),
        }
    }

    /// `None` = no hardware GPU adapter. `ours`: the pids whose GPU use isn't "other".
    fn sample(&mut self, ours: &[u32]) -> Option<GpuSignal> {
        let now = Instant::now();
        if now.duration_since(self.adapters_at) >= ADAPTER_REFRESH {
            let a = crate::profile::adapters();
            if a != self.adapters {
                log::info!("GPU adapters changed: {}", describe_adapters(&a));
                if self.adapters.is_empty() && self.pdh.is_none() {
                    self.pdh = open_pdh();
                }
                self.adapters = a;
                self.measured_at = None;
            }
            self.adapters_at = now;
        }
        if self.adapters.is_empty() {
            self.readings.clear();
            return None;
        }
        if self.measured_at.is_none_or(|t| now.duration_since(t) >= GPU_MEASURE_EVERY) {
            self.measured_at = Some(now);
            if let Some(p) = &mut self.pdh {
                self.engines = p.collect();
            }
            self.temps = self.adapters.iter().map(|a| (a.luid, adapter_temp_c(a.luid))).collect();
        }
        let other = other_use_by_adapter(&self.engines, ours);
        self.readings = self
            .adapters
            .iter()
            .map(|a| AdapterReading {
                luid: a.luid,
                vendor: a.vendor.clone(),
                name: a.name.clone(),
                other_util_pct: other.get(&a.luid).copied().unwrap_or(0),
                temp_c: self.temps.get(&a.luid).copied().flatten(),
            })
            .collect();
        Some(gpu_signal(&self.readings))
    }
}

#[cfg(windows)]
fn open_pdh() -> Option<PdhEngines> {
    match PdhEngines::open() {
        Ok(p) => Some(p),
        Err(st) => {
            log::info!("no GPU Engine counters (PDH 0x{st:08x}): other programs' GPU use is not measured");
            None
        }
    }
}

#[cfg(windows)]
fn describe_adapters(a: &[GpuAdapter]) -> String {
    if a.is_empty() {
        return "none".into();
    }
    a.iter().map(|a| format!("{} {} ({})", a.luid_hex(), a.vendor, a.name)).collect::<Vec<_>>().join(", ")
}

/// One `GPU Engine` counter instance: one process's use of one engine of one adapter.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EngineUse {
    pub pid: u32,
    pub luid: u64,
    pub phys: u32,
    pub eng: u32,
    pub util_pct: f64,
}

/// `pid_1234_luid_0x00000000_0x0000D1F3_phys_0_eng_0_engtype_3D` → (pid, luid, phys, eng). The
/// engine type (which may itself contain `_` or spaces) is not needed.
pub fn parse_engine_instance(name: &str) -> Option<(u32, u64, u32, u32)> {
    fn dec(s: &str) -> Option<u32> {
        if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        s.parse().ok()
    }
    fn hex32(s: &str) -> Option<u32> {
        let h = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X"))?;
        if h.is_empty() || h.len() > 8 || !h.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        u32::from_str_radix(h, 16).ok()
    }
    let mut t = name.split('_');
    (t.next()? == "pid").then_some(())?;
    let pid = dec(t.next()?)?;
    (t.next()? == "luid").then_some(())?;
    let high = hex32(t.next()?)?;
    let low = hex32(t.next()?)?;
    (t.next()? == "phys").then_some(())?;
    let phys = dec(t.next()?)?;
    (t.next()? == "eng").then_some(())?;
    let eng = dec(t.next()?)?;
    match t.next() {
        None | Some("engtype") => Some((pid, ((high as u64) << 32) | low as u64, phys, eng)),
        Some(_) => None,
    }
}

/// agent 0.3.4: one `GPU Adapter Memory` instance (`pid` None) or `GPU Process Memory` instance: bytes of dedicated
/// video memory on adapter `luid`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MemUse {
    pub pid: Option<u32>,
    pub luid: u64,
    pub bytes: u64,
}

/// `luid_0x00000000_0x0000D1F3_phys_0` (an adapter) or `pid_1234_luid_0x00000000_0x0000D1F3_phys_0` (a process on it)
/// -> (pid, luid).
pub fn parse_mem_instance(name: &str) -> Option<(Option<u32>, u64)> {
    let (pid, rest) = match name.strip_prefix("pid_") {
        Some(r) => {
            let (p, rest) = r.split_once('_')?;
            (Some(p.parse::<u32>().ok().filter(|_| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))?), rest)
        }
        None => (None, name),
    };
    // (the engine parser reads the same `luid_<hi>_<lo>_phys_<n>` tail; nothing may follow it here)
    if rest.split('_').count() != 5 {
        return None;
    }
    let (_, luid, _, _) = parse_engine_instance(&format!("pid_0_{rest}_eng_0"))?;
    Some((pid, luid))
}

/// The video memory on `luid` (MB) from a collection: the adapter's use (every program; without an adapter instance
/// the sum of its processes'), and each of `pids`' that holds some. None: nothing on that adapter.
pub fn gpu_memory_of(uses: &[MemUse], luid: u64, pids: &[u32]) -> Option<GpuMemory> {
    let on: Vec<&MemUse> = uses.iter().filter(|u| u.luid == luid).collect();
    if on.is_empty() {
        return None;
    }
    let procs: u64 = on.iter().filter(|u| u.pid.is_some()).map(|u| u.bytes).sum();
    let adapter = on.iter().filter(|u| u.pid.is_none()).map(|u| u.bytes).max().unwrap_or(procs);
    let mut per_pid: Vec<(u32, u64)> = Vec::new();
    for u in on.iter().filter(|u| u.pid.is_some_and(|p| pids.contains(&p))) {
        let p = u.pid.expect("filtered");
        match per_pid.iter_mut().find(|(x, _)| *x == p) {
            Some((_, b)) => *b += u.bytes,
            None => per_pid.push((p, u.bytes)),
        }
    }
    Some(GpuMemory { adapter_used_mb: adapter >> 20, per_pid: per_pid.into_iter().map(|(p, b)| (p, b >> 20)).collect() })
}

/// Other programs' GPU use per adapter LUID, percent 0..=100: per engine (luid, phys, eng) the
/// sum over every process not in `ours`, per adapter its busiest engine.
pub fn other_use_by_adapter(engines: &[EngineUse], ours: &[u32]) -> BTreeMap<u64, u32> {
    let mut per_engine: BTreeMap<(u64, u32, u32), f64> = BTreeMap::new();
    for e in engines {
        if ours.contains(&e.pid) || !e.util_pct.is_finite() || e.util_pct <= 0.0 {
            continue;
        }
        *per_engine.entry((e.luid, e.phys, e.eng)).or_default() += e.util_pct;
    }
    let mut out = BTreeMap::new();
    for ((luid, _, _), u) in per_engine {
        let pct = u.round().clamp(0.0, 100.0) as u32;
        let m = out.entry(luid).or_insert(0u32);
        *m = (*m).max(pct);
    }
    out
}

/// D3DKMT's deci-Celsius → °C (rounded). 0 (the driver doesn't report it) or above 125 °C (not a
/// real GPU reading; it would stop GPU work for good) = no reading.
pub fn temp_from_deci_c(deci: u32) -> Option<u32> {
    let c = deci.saturating_add(5) / 10;
    (1..=125).contains(&c).then_some(c)
}

#[cfg(windows)]
const GPU_ENGINE_COUNTER: &str = r"\GPU Engine(*)\Utilization Percentage";
#[cfg(windows)]
/// Not in windows-sys: don't cap the formatted value at 100.
const PDH_FMT_NOCAP100: u32 = 0x8000;

#[cfg(windows)]
/// One open PDH query on the GPU Engine counters (every instance).
struct PdhEngines {
    query: windows_sys::Win32::System::Performance::PDH_HQUERY,
    counter: windows_sys::Win32::System::Performance::PDH_HCOUNTER,
}

#[cfg(windows)]
// SAFETY: PDH query and counter handles may be used from any thread; `&mut self` keeps it to one
// thread at a time.
#[cfg(windows)]
unsafe impl Send for PdhEngines {}

#[cfg(windows)]
impl PdhEngines {
    fn open() -> Result<Self, u32> {
        use windows_sys::Win32::System::Performance::{PdhAddEnglishCounterW, PdhCloseQuery, PdhOpenQueryW};
        let mut query = std::ptr::null_mut();
        let st = unsafe { PdhOpenQueryW(std::ptr::null(), 0, &mut query) };
        if st != 0 {
            return Err(st);
        }
        let path = crate::util::wide(GPU_ENGINE_COUNTER);
        let mut counter = std::ptr::null_mut();
        let st = unsafe { PdhAddEnglishCounterW(query, path.as_ptr(), 0, &mut counter) };
        if st != 0 {
            unsafe { PdhCloseQuery(query) };
            return Err(st);
        }
        Ok(PdhEngines { query, counter })
    }

    /// Collects, then reads every engine instance that has a value. The first collection has no
    /// rate yet, nor has an instance seen for the first time: both read as no use.
    fn collect(&mut self) -> Vec<EngineUse> {
        use windows_sys::Win32::System::Performance::{
            PdhCollectQueryData, PdhGetFormattedCounterArrayW, PDH_CSTATUS_NEW_DATA, PDH_CSTATUS_VALID_DATA, PDH_FMT_COUNTERVALUE_ITEM_W,
            PDH_FMT_DOUBLE, PDH_MORE_DATA,
        };
        let t0 = Instant::now();
        let st = unsafe { PdhCollectQueryData(self.query) };
        if st != 0 {
            log::debug!("GPU Engine counters: collect failed (PDH 0x{st:08x})");
            return Vec::new();
        }
        let mut out = Vec::new();
        let mut instances = 0usize;
        // u64 elements: the items hold doubles and pointers.
        let mut buf: Vec<u64> = Vec::new();
        for _ in 0..4 {
            let mut bytes = (buf.len() * 8) as u32;
            let mut count = 0u32;
            let items_ptr = if buf.is_empty() { std::ptr::null_mut() } else { buf.as_mut_ptr() as *mut PDH_FMT_COUNTERVALUE_ITEM_W };
            let st = unsafe { PdhGetFormattedCounterArrayW(self.counter, PDH_FMT_DOUBLE | PDH_FMT_NOCAP100, &mut bytes, &mut count, items_ptr) };
            if st == PDH_MORE_DATA {
                buf = vec![0u64; (bytes as usize).div_ceil(8).max(1)];
                continue;
            }
            // Anything else (PDH_INVALID_DATA after the first collection, no instances): no use.
            if st != 0 || count == 0 || buf.is_empty() {
                break;
            }
            instances = count as usize;
            let items = unsafe { std::slice::from_raw_parts(buf.as_ptr() as *const PDH_FMT_COUNTERVALUE_ITEM_W, instances) };
            for it in items {
                if it.FmtValue.CStatus != PDH_CSTATUS_VALID_DATA && it.FmtValue.CStatus != PDH_CSTATUS_NEW_DATA {
                    continue;
                }
                let name = unsafe { wide_ptr_to_string(it.szName) };
                let Some((pid, luid, phys, eng)) = parse_engine_instance(&name) else { continue };
                let util_pct = unsafe { it.FmtValue.Anonymous.doubleValue };
                out.push(EngineUse { pid, luid, phys, eng, util_pct });
            }
            break;
        }
        log::debug!("GPU Engine counters: {instances} instances, {} with values, {} ms", out.len(), t0.elapsed().as_millis());
        out
    }
}

#[cfg(windows)]
impl Drop for PdhEngines {
    fn drop(&mut self) {
        unsafe { windows_sys::Win32::System::Performance::PdhCloseQuery(self.query) };
    }
}

#[cfg(windows)]
const GPU_ADAPTER_MEMORY_COUNTER: &str = r"\GPU Adapter Memory(*)\Dedicated Usage";
#[cfg(windows)]
const GPU_PROCESS_MEMORY_COUNTER: &str = r"\GPU Process Memory(*)\Dedicated Usage";

#[cfg(windows)]
/// agent 0.3.4: one open PDH query on the dedicated GPU memory counters (every adapter's, every process's; what Task
/// Manager shows). Instance names carry only pids and adapter numbers.
struct PdhMemory {
    query: windows_sys::Win32::System::Performance::PDH_HQUERY,
    adapter: windows_sys::Win32::System::Performance::PDH_HCOUNTER,
    process: windows_sys::Win32::System::Performance::PDH_HCOUNTER,
}

#[cfg(windows)]
// SAFETY: as `PdhEngines`; it is used behind a mutex.
unsafe impl Send for PdhMemory {}

#[cfg(windows)]
impl PdhMemory {
    fn open() -> Result<Self, u32> {
        use windows_sys::Win32::System::Performance::{PdhAddEnglishCounterW, PdhCloseQuery, PdhOpenQueryW};
        let mut query = std::ptr::null_mut();
        let st = unsafe { PdhOpenQueryW(std::ptr::null(), 0, &mut query) };
        if st != 0 {
            return Err(st);
        }
        let add = |path: &str| {
            let w = crate::util::wide(path);
            let mut c = std::ptr::null_mut();
            let st = unsafe { PdhAddEnglishCounterW(query, w.as_ptr(), 0, &mut c) };
            if st != 0 {
                Err(st)
            } else {
                Ok(c)
            }
        };
        match (add(GPU_ADAPTER_MEMORY_COUNTER), add(GPU_PROCESS_MEMORY_COUNTER)) {
            (Ok(adapter), Ok(process)) => Ok(PdhMemory { query, adapter, process }),
            (Err(st), _) | (_, Err(st)) => {
                unsafe { PdhCloseQuery(query) };
                Err(st)
            }
        }
    }

    /// Collects, then reads every instance of both counters (raw byte counts: one collection is enough).
    fn collect(&mut self) -> Vec<MemUse> {
        use windows_sys::Win32::System::Performance::PdhCollectQueryData;
        let st = unsafe { PdhCollectQueryData(self.query) };
        if st != 0 {
            log::debug!("GPU memory counters: collect failed (PDH 0x{st:08x})");
            return Vec::new();
        }
        let mut out = Vec::new();
        for c in [self.adapter, self.process] {
            for (name, bytes) in large_array(c) {
                if let Some((pid, luid)) = parse_mem_instance(&name) {
                    out.push(MemUse { pid, luid, bytes });
                }
            }
        }
        out
    }
}

#[cfg(windows)]
/// Every instance of counter `c` with a valid value, as a 64-bit integer: (instance name, value).
fn large_array(c: windows_sys::Win32::System::Performance::PDH_HCOUNTER) -> Vec<(String, u64)> {
    use windows_sys::Win32::System::Performance::{PdhGetFormattedCounterArrayW, PDH_CSTATUS_NEW_DATA, PDH_CSTATUS_VALID_DATA, PDH_FMT_COUNTERVALUE_ITEM_W, PDH_FMT_LARGE, PDH_MORE_DATA};
    let mut out = Vec::new();
    let mut buf: Vec<u64> = Vec::new();
    for _ in 0..4 {
        let mut bytes = (buf.len() * 8) as u32;
        let mut count = 0u32;
        let items_ptr = if buf.is_empty() { std::ptr::null_mut() } else { buf.as_mut_ptr() as *mut PDH_FMT_COUNTERVALUE_ITEM_W };
        let st = unsafe { PdhGetFormattedCounterArrayW(c, PDH_FMT_LARGE, &mut bytes, &mut count, items_ptr) };
        if st == PDH_MORE_DATA {
            buf = vec![0u64; (bytes as usize).div_ceil(8).max(1)];
            continue;
        }
        if st != 0 || count == 0 || buf.is_empty() {
            break;
        }
        let items = unsafe { std::slice::from_raw_parts(buf.as_ptr() as *const PDH_FMT_COUNTERVALUE_ITEM_W, count as usize) };
        for it in items {
            if it.FmtValue.CStatus != PDH_CSTATUS_VALID_DATA && it.FmtValue.CStatus != PDH_CSTATUS_NEW_DATA {
                continue;
            }
            let v = unsafe { it.FmtValue.Anonymous.largeValue };
            out.push((unsafe { wide_ptr_to_string(it.szName) }, v.max(0) as u64));
        }
        break;
    }
    out
}

#[cfg(windows)]
impl Drop for PdhMemory {
    fn drop(&mut self) {
        unsafe { windows_sys::Win32::System::Performance::PdhCloseQuery(self.query) };
    }
}

#[cfg(windows)]
/// A NUL-terminated UTF-16 string (at most 1024 units read).
///
/// # Safety
/// `p` is null or points to a NUL-terminated (or at least 1024-unit) UTF-16 buffer.
unsafe fn wide_ptr_to_string(p: *const u16) -> String {
    if p.is_null() {
        return String::new();
    }
    let mut n = 0;
    while n < 1024 && unsafe { *p.add(n) } != 0 {
        n += 1;
    }
    String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(p, n) })
}

#[cfg(windows)]
/// The adapter's temperature from the D3DKMT adapter performance data, °C: the hottest physical
/// adapter of a linked adapter. `None` when the adapter can't be opened, the driver doesn't
/// support the query (it needs WDDM 2.4+) or reports no temperature.
pub fn adapter_temp_c(luid: u64) -> Option<u32> {
    use windows_sys::Wdk::Graphics::Direct3D::{
        D3DKMTCloseAdapter, D3DKMTOpenAdapterFromLuid, D3DKMTQueryAdapterInfo, D3DKMT_ADAPTER_PERFDATA, D3DKMT_CLOSEADAPTER,
        D3DKMT_OPENADAPTERFROMLUID, D3DKMT_QUERYADAPTERINFO, KMTQAITYPE_ADAPTERPERFDATA, KMTQAITYPE_PHYSICALADAPTERCOUNT,
    };
    use windows_sys::Win32::Foundation::LUID;
    let luid = LUID { LowPart: luid as u32, HighPart: (luid >> 32) as u32 as i32 };
    let mut open = D3DKMT_OPENADAPTERFROMLUID { AdapterLuid: luid, hAdapter: 0 };
    if unsafe { D3DKMTOpenAdapterFromLuid(&mut open) } < 0 {
        return None;
    }
    let h = open.hAdapter;
    let mut count = 1u32;
    let mut q = D3DKMT_QUERYADAPTERINFO {
        hAdapter: h,
        Type: KMTQAITYPE_PHYSICALADAPTERCOUNT,
        pPrivateDriverData: &mut count as *mut u32 as *mut std::ffi::c_void,
        PrivateDriverDataSize: std::mem::size_of::<u32>() as u32,
    };
    if unsafe { D3DKMTQueryAdapterInfo(&mut q) } < 0 {
        count = 1;
    }
    let mut hottest: Option<u32> = None;
    for i in 0..count.clamp(1, 8) {
        let mut pd = D3DKMT_ADAPTER_PERFDATA { PhysicalAdapterIndex: i, ..Default::default() };
        let mut q = D3DKMT_QUERYADAPTERINFO {
            hAdapter: h,
            Type: KMTQAITYPE_ADAPTERPERFDATA,
            pPrivateDriverData: &mut pd as *mut D3DKMT_ADAPTER_PERFDATA as *mut std::ffi::c_void,
            PrivateDriverDataSize: std::mem::size_of::<D3DKMT_ADAPTER_PERFDATA>() as u32,
        };
        if unsafe { D3DKMTQueryAdapterInfo(&mut q) } >= 0 {
            if let Some(t) = temp_from_deci_c(pd.Temperature) {
                hottest = Some(hottest.map_or(t, |h| h.max(t)));
            }
        }
    }
    unsafe { D3DKMTCloseAdapter(&D3DKMT_CLOSEADAPTER { hAdapter: h }) };
    hottest
}

#[cfg(windows)]
/// NVML from `%SystemRoot%\System32\nvml.dll` only, where the NVIDIA driver installs it
/// (security review L7): a bare `nvml.dll` would be searched for in the current directory and
/// every `PATH` directory, where another program could plant one. The DLL is loaded with
/// `LOAD_LIBRARY_SEARCH_SYSTEM32` and then handed to NVML by the full path it was loaded from.
pub fn load_nvml() -> Result<nvml_wrapper::Nvml, String> {
    let path = system32_module("nvml.dll").ok_or("no nvml.dll in System32")?;
    nvml_wrapper::Nvml::builder().lib_path(path.as_os_str()).init().map_err(|e| e.to_string())
}

#[cfg(windows)]
/// Loads `name` from System32 only and returns the full path it was loaded from. The module stays
/// loaded (NVML loads it again by that path, which finds the same module).
fn system32_module(name: &str) -> Option<std::path::PathBuf> {
    use windows_sys::Win32::System::LibraryLoader::{GetModuleFileNameW, LoadLibraryExW, LOAD_LIBRARY_SEARCH_SYSTEM32};
    let w = crate::util::wide(name);
    let h = unsafe { LoadLibraryExW(w.as_ptr(), std::ptr::null_mut(), LOAD_LIBRARY_SEARCH_SYSTEM32) };
    if h.is_null() {
        return None;
    }
    let mut buf = vec![0u16; 1024];
    let n = unsafe { GetModuleFileNameW(h, buf.as_mut_ptr(), buf.len() as u32) } as usize;
    (n > 0 && n < buf.len()).then(|| std::path::PathBuf::from(crate::util::from_wide(&buf[..n])))
}

#[cfg(windows)]
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

#[cfg(windows)]
pub fn notification_state() -> Notification {
    use windows_sys::Win32::UI::Shell::SHQueryUserNotificationState;
    let mut st = 0i32;
    if unsafe { SHQueryUserNotificationState(&mut st) } != 0 {
        return Notification::Unknown;
    }
    let n = match st {
        1 => Notification::NotPresent,
        2 => Notification::Busy,
        3 => Notification::D3dFullscreen,
        4 => Notification::Presentation,
        5 => Notification::AcceptsNotifications,
        6 => Notification::QuietTime,
        7 => Notification::App,
        _ => Notification::Unknown,
    };
    // agent 0.3.7: "busy" (a fullscreen app) and "app" (a fullscreen Store app) count only while a foreground window
    // covers its whole monitor. The 4070 Ti canary PC reported a fullscreen app on 2026-11-20 with its last input ~4.5
    // days old and no foreground window at all; a Direct3D fullscreen game and presentation mode stay as Windows says.
    if matches!(n, Notification::Busy | Notification::App) && !foreground_fullscreen() {
        return Notification::AcceptsNotifications;
    }
    n
}

/// agent 0.3.7: a foreground window covers its whole monitor (not the desktop's own window). When Windows can't say
/// where the window or its monitor is, yes (Windows' own "fullscreen" answer stands).
#[cfg(windows)]
fn foreground_fullscreen() -> bool {
    use windows_sys::Win32::Foundation::RECT;
    use windows_sys::Win32::Graphics::Gdi::{GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONULL};
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetShellWindow, GetWindowRect};
    let w = unsafe { GetForegroundWindow() };
    if w.is_null() || w == unsafe { GetShellWindow() } {
        return false;
    }
    let mut r: RECT = unsafe { std::mem::zeroed() };
    if unsafe { GetWindowRect(w, &mut r) } == 0 {
        return true;
    }
    let m = unsafe { MonitorFromWindow(w, MONITOR_DEFAULTTONULL) };
    if m.is_null() {
        return false;
    }
    let mut mi: MONITORINFO = unsafe { std::mem::zeroed() };
    mi.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
    if unsafe { GetMonitorInfoW(m, &mut mi) } == 0 {
        return true;
    }
    let rect = |x: &RECT| (x.left, x.top, x.right, x.bottom);
    covers(rect(&r), rect(&mi.rcMonitor))
}

/// agent 0.3.7: a window's rectangle `w` covers the monitor's `m` (left, top, right, bottom): a fullscreen window does;
/// a maximized one stops at the taskbar.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn covers(w: (i32, i32, i32, i32), m: (i32, i32, i32, i32)) -> bool {
    w.0 <= m.0 && w.1 <= m.1 && w.2 >= m.2 && w.3 >= m.3
}

#[cfg(windows)]
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

#[cfg(windows)]
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
    /// Tests: other programs' share of the CPUs (None: as measured).
    pub other_load: Arc<Mutex<Option<f64>>>,
    /// Tests: false - the worker counts as starved only when `starved` says so, not when the test machine itself is
    /// busy (a fake worker sleeps, so on a saturated machine it always looks starved).
    pub real_starvation: bool,
    /// Tests (agent 0.3.4): the dedicated video memory other programs hold on every adapter (MB; None: not measured,
    /// as without the counters). The workers' own come from their `vram_mb`.
    pub gpu_others_mb: Arc<Mutex<Option<u64>>>,
    /// Tests: the pids the last `sample` was told are ours.
    pub ours: Arc<Mutex<Vec<u32>>>,
}

impl SharedSignals {
    pub fn new(s: Signals) -> Self {
        SharedSignals {
            inner: Arc::new(Mutex::new(s)),
            frozen_now: false,
            starved: Arc::new(AtomicBool::new(false)),
            other_load: Arc::new(Mutex::new(Some(0.0))),
            real_starvation: true,
            gpu_others_mb: Arc::new(Mutex::new(None)),
            ours: Arc::new(Mutex::new(Vec::new())),
        }
    }
    pub fn set_gpu_others(&self, mb: Option<u64>) {
        *self.gpu_others_mb.lock().unwrap() = mb;
    }
    pub fn set_other_load(&self, share: Option<f64>) {
        *self.other_load.lock().unwrap() = share;
    }
    pub fn set_starved(&self, starved: bool) {
        self.starved.store(starved, Ordering::SeqCst);
    }
    pub fn set(&self, f: impl FnOnce(&mut Signals)) {
        f(&mut self.inner.lock().unwrap());
    }
}

impl SignalSource for SharedSignals {
    fn sample(&mut self, worker_pids: &[u32]) -> Signals {
        *self.ours.lock().unwrap() = worker_pids.to_vec();
        let mut s = self.inner.lock().unwrap().clone();
        if !self.frozen_now {
            s.now = zerofarm_common::now_unix();
        }
        s
    }
    fn memory_mb(&self) -> (u64, u64) {
        let s = self.inner.lock().unwrap();
        (s.mem_avail_mb, s.mem_total_mb)
    }
    fn worker_starved(&self, mark: &CpuMark, worker: &WorkerProc) -> bool {
        self.starved.load(Ordering::SeqCst) || (self.real_starvation && mark.starved_since(worker))
    }
    fn other_load(&self, measured: f64) -> f64 {
        self.other_load.lock().unwrap().unwrap_or(measured)
    }
    fn gpu_memory(&self, _luid: u64, _pids: &[u32]) -> Option<GpuMemory> {
        self.gpu_others_mb.lock().unwrap().map(|mb| GpuMemory { adapter_used_mb: mb, per_pid: vec![] })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gpu_memory_instances() {
        assert_eq!(parse_mem_instance("luid_0x00000000_0x0000D1F3_phys_0"), Some((None, 0xD1F3)));
        assert_eq!(parse_mem_instance("pid_1234_luid_0x00000001_0x00008A2C_phys_0"), Some((Some(1234), 0x1_0000_8A2C)));
        assert_eq!(parse_mem_instance("pid_x_luid_0x00000000_0x0000D1F3_phys_0"), None);
        assert_eq!(parse_mem_instance("luid_0x00000000_phys_0"), None);
        assert_eq!(parse_mem_instance("pid_12_luid_0x00000000_0x0000D1F3_phys_0_eng_0_engtype_3D"), None, "an engine instance is not a memory one");
        let mb = |n: u64| n << 20;
        let uses = [
            MemUse { pid: None, luid: 0xD1F3, bytes: mb(2557) },
            MemUse { pid: Some(10), luid: 0xD1F3, bytes: mb(700) },
            MemUse { pid: Some(11), luid: 0xD1F3, bytes: mb(650) },
            MemUse { pid: Some(12), luid: 0xD1F3, bytes: mb(300) },
            MemUse { pid: Some(10), luid: 0xBEEF, bytes: mb(5) },
        ];
        let m = gpu_memory_of(&uses, 0xD1F3, &[10, 11, 99]).unwrap();
        assert_eq!(m, GpuMemory { adapter_used_mb: 2557, per_pid: vec![(10, 700), (11, 650)] });
        assert_eq!((m.of(10), m.of(99)), (700, 0));
        assert_eq!(gpu_memory_of(&uses[1..4], 0xD1F3, &[]).unwrap().adapter_used_mb, 1650, "no adapter instance: its processes' sum");
        assert_eq!(gpu_memory_of(&uses, 0x1234, &[10]), None);
    }

    /// The real counters on this PC (a GPU and Windows' counters needed): every program's dedicated memory in use on
    /// each adapter, and this process's (none: it uses no GPU).
    #[test]
    #[cfg(windows)]
    #[ignore]
    fn gpu_memory_counters_on_this_pc() {
        let s = Win32Signals::new(true);
        // (`ZF_GPU_PIDS=1,2`: processes whose dedicated memory is shown too)
        let pids: Vec<u32> = std::env::var("ZF_GPU_PIDS").unwrap_or_default().split(',').filter_map(|p| p.trim().parse().ok()).collect();
        let mut any = false;
        for a in crate::profile::adapters() {
            let m = s.gpu_memory(a.luid, &pids);
            eprintln!("{} {} ({} MB dedicated): {m:?}", a.vendor, a.name, a.vram_mb);
            any |= m.is_some_and(|m| m.adapter_used_mb > 0);
        }
        assert!(any, "no adapter with dedicated memory in use");
    }

    const LUID_A: u64 = 0xD1F3;
    const LUID_B: u64 = 0x1_0000_8A2C;

    fn e(pid: u32, luid: u64, eng: u32, util_pct: f64) -> EngineUse {
        EngineUse { pid, luid, phys: 0, eng, util_pct }
    }

    #[test]
    fn parses_engine_instance_names() {
        assert_eq!(parse_engine_instance("pid_1234_luid_0x00000000_0x0000D1F3_phys_0_eng_0_engtype_3D"), Some((1234, 0xD1F3, 0, 0)));
        assert_eq!(
            parse_engine_instance("pid_8_luid_0x00000001_0x00008A2C_phys_1_eng_12_engtype_VideoDecode"),
            Some((8, 0x1_0000_8A2C, 1, 12))
        );
        // Engine types with spaces or underscores, an empty type, lowercase hex.
        assert_eq!(parse_engine_instance("pid_4_luid_0x00000000_0x0000d1f3_phys_0_eng_3_engtype_Legacy Overlay"), Some((4, 0xD1F3, 0, 3)));
        assert_eq!(parse_engine_instance("pid_4_luid_0x00000000_0x0000D1F3_phys_0_eng_3_engtype_Copy_2"), Some((4, 0xD1F3, 0, 3)));
        assert_eq!(parse_engine_instance("pid_4_luid_0x00000000_0x0000D1F3_phys_0_eng_3_engtype_"), Some((4, 0xD1F3, 0, 3)));
        assert_eq!(parse_engine_instance("pid_4_luid_0x00000000_0x0000D1F3_phys_0_eng_3"), Some((4, 0xD1F3, 0, 3)));
        assert_eq!(
            parse_engine_instance("pid_4294967295_luid_0xFFFFFFFF_0xFFFFFFFF_phys_0_eng_0_engtype_3D"),
            Some((u32::MAX, u64::MAX, 0, 0))
        );
        for bad in [
            "",
            "_Total",
            "pid_",
            "pid_1234",
            "pid_x_luid_0x00000000_0x0000D1F3_phys_0_eng_0_engtype_3D",
            "pid_+12_luid_0x00000000_0x0000D1F3_phys_0_eng_0_engtype_3D",
            "pid_12_luid_00000000_0x0000D1F3_phys_0_eng_0_engtype_3D",
            "pid_12_luid_0x_0x0000D1F3_phys_0_eng_0_engtype_3D",
            "pid_12_luid_0x000000000_0x0000D1F3_phys_0_eng_0_engtype_3D",
            "pid_12_luid_0x0000000G_0x0000D1F3_phys_0_eng_0_engtype_3D",
            "pid_12_luid_0x00000000_phys_0_eng_0_engtype_3D",
            "pid_12_luid_0x00000000_0x0000D1F3_eng_0_engtype_3D",
            "pid_12_luid_0x00000000_0x0000D1F3_phys_0_eng_engtype_3D",
            "pid_12_luid_0x00000000_0x0000D1F3_phys_0_eng_0_extra_3D",
            "pid_99999999999_luid_0x00000000_0x0000D1F3_phys_0_eng_0_engtype_3D",
            "luid_0x00000000_0x0000D1F3_phys_0_eng_0_engtype_3D",
        ] {
            assert_eq!(parse_engine_instance(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn other_use_sums_processes_per_engine_and_takes_the_busiest_engine() {
        let engines = [
            // Adapter A, engine 0 (3D): two other programs, 30 % + 25 %.
            e(100, LUID_A, 0, 30.0),
            e(200, LUID_A, 0, 25.0),
            // Adapter A, engine 1 (copy): 40 %, less than engine 0's 55 %.
            e(100, LUID_A, 1, 40.0),
            // Adapter B: 7 % on one engine, 9 % on another.
            e(300, LUID_B, 0, 7.0),
            e(300, LUID_B, 2, 9.0),
        ];
        let m = other_use_by_adapter(&engines, &[]);
        assert_eq!(m.get(&LUID_A), Some(&55));
        assert_eq!(m.get(&LUID_B), Some(&9));
        assert_eq!(m.len(), 2);
        // The same engine number on another physical adapter of a linked adapter is another engine.
        let linked = [EngineUse { pid: 1, luid: LUID_A, phys: 0, eng: 0, util_pct: 30.0 }, EngineUse {
            pid: 2,
            luid: LUID_A,
            phys: 1,
            eng: 0,
            util_pct: 40.0,
        }];
        assert_eq!(other_use_by_adapter(&linked, &[]).get(&LUID_A), Some(&40));
    }

    #[test]
    fn our_pids_are_not_other_use() {
        let worker = 4242;
        let agent = 77;
        let engines = [e(worker, LUID_A, 0, 95.0), e(agent, LUID_A, 0, 3.0), e(500, LUID_A, 0, 4.0), e(worker, LUID_A, 1, 60.0)];
        let m = other_use_by_adapter(&engines, &[worker, agent]);
        assert_eq!(m.get(&LUID_A), Some(&4));
        // Only our worker on the GPU: no other use at all.
        let only_ours = [e(worker, LUID_A, 0, 99.0)];
        assert_eq!(other_use_by_adapter(&only_ours, &[worker]).get(&LUID_A), None);
        // Without the exclusion the worker counts.
        assert_eq!(other_use_by_adapter(&only_ours, &[]).get(&LUID_A), Some(&99));
    }

    #[test]
    fn other_use_is_clamped_and_ignores_junk_values() {
        let engines = [
            e(1, LUID_A, 0, 80.0),
            e(2, LUID_A, 0, 70.0), // 150 % on one engine (sampling skew): 100
            e(3, LUID_B, 0, f64::NAN),
            e(4, LUID_B, 0, -5.0),
            e(5, LUID_B, 1, f64::INFINITY),
            e(6, LUID_B, 2, 0.4),
        ];
        let m = other_use_by_adapter(&engines, &[]);
        assert_eq!(m.get(&LUID_A), Some(&100));
        assert_eq!(m.get(&LUID_B), Some(&0));
        assert!(other_use_by_adapter(&[], &[1]).is_empty());
    }

    #[test]
    fn signal_is_busiest_adapter_and_hottest_reading() {
        let r = |luid, other_util_pct, temp_c| AdapterReading { luid, vendor: "amd".into(), name: "x".into(), other_util_pct, temp_c };
        assert_eq!(gpu_signal(&[]), GpuSignal { other_util_pct: 0, temp_c: None });
        assert_eq!(gpu_signal(&[r(LUID_A, 5, Some(61)), r(LUID_B, 30, None)]), GpuSignal { other_util_pct: 30, temp_c: Some(61) });
        assert_eq!(gpu_signal(&[r(LUID_A, 0, None), r(LUID_B, 0, None)]), GpuSignal { other_util_pct: 0, temp_c: None });
        assert_eq!(gpu_signal(&[r(LUID_A, 0, Some(70)), r(LUID_B, 0, Some(84))]).temp_c, Some(84));
    }

    #[test]
    fn a_fullscreen_window_covers_its_monitor_a_maximized_one_does_not() {
        let monitor = (0, 0, 2560, 1440);
        assert!(covers((0, 0, 2560, 1440), monitor), "fullscreen");
        assert!(covers((-8, -8, 2568, 1448), monitor), "past the edges (a frame)");
        assert!(!covers((-8, -8, 2568, 1400), monitor), "maximized: stops at the taskbar");
        assert!(!covers((100, 100, 900, 700), monitor), "a window");
        // a second monitor to the right
        assert!(covers((2560, 0, 4480, 1080), (2560, 0, 4480, 1080)));
        assert!(!covers((0, 0, 2560, 1440), (2560, 0, 4480, 1080)));
    }

    #[test]
    fn deci_celsius() {
        assert_eq!(temp_from_deci_c(0), None, "0 = the driver doesn't report it");
        assert_eq!(temp_from_deci_c(4), None);
        assert_eq!(temp_from_deci_c(5), Some(1));
        assert_eq!(temp_from_deci_c(612), Some(61));
        assert_eq!(temp_from_deci_c(615), Some(62));
        assert_eq!(temp_from_deci_c(1250), Some(125));
        assert_eq!(temp_from_deci_c(1255), None);
        assert_eq!(temp_from_deci_c(u32::MAX), None);
    }

    #[test]
    #[cfg(windows)]
    fn perfdata_layout_matches_the_sdk() {
        // d3dkmthk.h (SDK 10.0.26100): u32, five 8-aligned u64s, FanRPM, Power, Temperature, u8.
        use windows_sys::Wdk::Graphics::Direct3D::D3DKMT_ADAPTER_PERFDATA;
        assert_eq!(std::mem::size_of::<D3DKMT_ADAPTER_PERFDATA>(), 64);
        assert_eq!(std::mem::offset_of!(D3DKMT_ADAPTER_PERFDATA, MemoryFrequency), 8);
        assert_eq!(std::mem::offset_of!(D3DKMT_ADAPTER_PERFDATA, FanRPM), 48);
        assert_eq!(std::mem::offset_of!(D3DKMT_ADAPTER_PERFDATA, Temperature), 56);
        assert_eq!(std::mem::offset_of!(D3DKMT_ADAPTER_PERFDATA, PowerStateOverride), 60);
    }
}

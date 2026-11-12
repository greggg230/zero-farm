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

use crate::policy::{GpuSignal, Notification, Signals};
use crate::profile::GpuAdapter;
use crate::worker::{CpuMark, WorkerProc};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub trait SignalSource: Send {
    /// `worker_pid`: our own worker, excluded from "other GPU users".
    fn sample(&mut self, worker_pid: Option<u32>) -> Signals;

    /// Since `mark`, was the worker starved of CPU: it got almost none while the PC had almost no
    /// idle time (other programs), so it is waiting for a turn, not hung.
    fn worker_starved(&self, mark: &CpuMark, worker: &WorkerProc) -> bool {
        mark.starved_since(worker)
    }
}

/// The GPUs are measured at most this often; `sample` reuses the last measurement in between
/// (the lease loop samples every 200 ms, and a PDH collection over every process's GPU engines
/// is not free). A PDH rate is the average since the previous collection.
pub const GPU_MEASURE_EVERY: Duration = Duration::from_secs(1);
/// The adapter list is re-read this often: a driver update or restart gives an adapter a new LUID,
/// and an external GPU can come and go.
pub const ADAPTER_REFRESH: Duration = Duration::from_secs(60);

/// The real thing.
pub struct Win32Signals {
    gpu: Option<GpuSampler>,
}

impl Win32Signals {
    /// `gpu`: measure the GPUs (false: report none, as on a PC without one).
    pub fn new(gpu: bool) -> Self {
        Win32Signals { gpu: gpu.then(GpuSampler::new) }
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

impl SignalSource for Win32Signals {
    fn sample(&mut self, worker_pid: Option<u32>) -> Signals {
        let (_, sod) = crate::util::local_now();
        let (mem_avail_mb, mem_total_mb) = memory_mb();
        let (on_ac, battery_saver) = power();
        let ours: Vec<u32> = worker_pid.into_iter().chain([std::process::id()]).collect();
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

fn open_pdh() -> Option<PdhEngines> {
    match PdhEngines::open() {
        Ok(p) => Some(p),
        Err(st) => {
            log::info!("no GPU Engine counters (PDH 0x{st:08x}): other programs' GPU use is not measured");
            None
        }
    }
}

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

const GPU_ENGINE_COUNTER: &str = r"\GPU Engine(*)\Utilization Percentage";
/// Not in windows-sys: don't cap the formatted value at 100.
const PDH_FMT_NOCAP100: u32 = 0x8000;

/// One open PDH query on the GPU Engine counters (every instance).
struct PdhEngines {
    query: windows_sys::Win32::System::Performance::PDH_HQUERY,
    counter: windows_sys::Win32::System::Performance::PDH_HCOUNTER,
}

// SAFETY: PDH query and counter handles may be used from any thread; `&mut self` keeps it to one
// thread at a time.
unsafe impl Send for PdhEngines {}

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

impl Drop for PdhEngines {
    fn drop(&mut self) {
        unsafe { windows_sys::Win32::System::Performance::PdhCloseQuery(self.query) };
    }
}

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

/// NVML from `%SystemRoot%\System32\nvml.dll` only, where the NVIDIA driver installs it
/// (security review L7): a bare `nvml.dll` would be searched for in the current directory and
/// every `PATH` directory, where another program could plant one. The DLL is loaded with
/// `LOAD_LIBRARY_SEARCH_SYSTEM32` and then handed to NVML by the full path it was loaded from.
pub fn load_nvml() -> Result<nvml_wrapper::Nvml, String> {
    let path = system32_module("nvml.dll").ok_or("no nvml.dll in System32")?;
    nvml_wrapper::Nvml::builder().lib_path(path.as_os_str()).init().map_err(|e| e.to_string())
}

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

#[cfg(test)]
mod tests {
    use super::*;

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

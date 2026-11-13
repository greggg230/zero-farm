//! The machine profile (PROTOCOL 3, DESIGN 5.4): CPU model, performance/efficiency cores, RAM,
//! free disk, GPUs (DXGI, one entry per adapter LUID, plus NVML for NVIDIA driver versions), Windows build, and the ONNX Runtime
//! providers (filled in by the runtime pack later; `cpu` for now), and availability (idle and on
//! hours per day, availability.rs). Hardware facts and hours only.

use zerofarm_common::protocol::{Gpu, Profile};

/// CPU brand string via CPUID leaves 0x80000002..4.
pub fn cpu_brand() -> String {
    #[cfg(target_arch = "x86_64")]
    {
        use std::arch::x86_64::__cpuid;
        if __cpuid(0x8000_0000).eax >= 0x8000_0004 {
            let mut b = Vec::with_capacity(48);
            for leaf in 0x8000_0002u32..=0x8000_0004 {
                let r = __cpuid(leaf);
                for x in [r.eax, r.ebx, r.ecx, r.edx] {
                    b.extend_from_slice(&x.to_le_bytes());
                }
            }
            return String::from_utf8_lossy(&b).trim_matches(char::from(0)).trim().to_string();
        }
    }
    "unknown CPU".into()
}

/// (physical cores, logical processors, performance cores, efficiency cores).
pub fn cores() -> (u32, u32, u32, u32) {
    use windows_sys::Win32::System::SystemInformation::{
        GetLogicalProcessorInformationEx, RelationProcessorCore, SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX,
    };
    let mut len = 0u32;
    unsafe { GetLogicalProcessorInformationEx(RelationProcessorCore, std::ptr::null_mut(), &mut len) };
    if len == 0 {
        let n = std::thread::available_parallelism().map(|n| n.get() as u32).unwrap_or(1);
        return (n, n, n, 0);
    }
    let mut buf = vec![0u8; len as usize];
    if unsafe { GetLogicalProcessorInformationEx(RelationProcessorCore, buf.as_mut_ptr() as *mut _, &mut len) } == 0 {
        let n = std::thread::available_parallelism().map(|n| n.get() as u32).unwrap_or(1);
        return (n, n, n, 0);
    }
    let mut classes: Vec<(u8, u32)> = Vec::new(); // (efficiency class, logical count) per core
    let mut off = 0usize;
    while off < len as usize {
        let rec = unsafe { &*(buf.as_ptr().add(off) as *const SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX) };
        if rec.Relationship == RelationProcessorCore {
            let p = unsafe { &rec.Anonymous.Processor };
            let mut logical = 0u32;
            for i in 0..p.GroupCount as usize {
                let g = unsafe { *p.GroupMask.as_ptr().add(i) };
                logical += g.Mask.count_ones();
            }
            classes.push((p.EfficiencyClass, logical));
        }
        if rec.Size == 0 {
            break;
        }
        off += rec.Size as usize;
    }
    let physical = classes.len() as u32;
    let logical: u32 = classes.iter().map(|c| c.1).sum();
    let max_class = classes.iter().map(|c| c.0).max().unwrap_or(0);
    let min_class = classes.iter().map(|c| c.0).min().unwrap_or(0);
    let (perf, eff) = if max_class == min_class {
        (physical, 0)
    } else {
        // Higher efficiency class = faster (performance) core.
        let p = classes.iter().filter(|c| c.0 == max_class).count() as u32;
        (p, physical - p)
    };
    (physical.max(1), logical.max(1), perf, eff)
}

/// "Windows 11 26100" from RtlGetVersion (GetVersionEx lies without a manifest).
pub fn os_name() -> String {
    #[repr(C)]
    struct OsVersionInfoW {
        size: u32,
        major: u32,
        minor: u32,
        build: u32,
        platform: u32,
        csd: [u16; 128],
    }
    type RtlGetVersion = unsafe extern "system" fn(*mut OsVersionInfoW) -> i32;
    use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
    unsafe {
        let m = GetModuleHandleW(crate::util::wide("ntdll.dll").as_ptr());
        if let Some(f) = GetProcAddress(m, b"RtlGetVersion\0".as_ptr()) {
            let f: RtlGetVersion = std::mem::transmute(f);
            let mut v: OsVersionInfoW = std::mem::zeroed();
            v.size = std::mem::size_of::<OsVersionInfoW>() as u32;
            if f(&mut v) == 0 {
                let name = if v.major == 10 && v.build >= 22000 { "Windows 11".to_string() } else { format!("Windows {}", v.major) };
                return format!("{name} {}", v.build);
            }
        }
    }
    "Windows".into()
}

/// A hardware GPU adapter as DXGI lists it: software adapters and the Microsoft Basic Render
/// Driver are left out, and each LUID appears once (DXGI can list one card twice).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GpuAdapter {
    /// `(HighPart << 32) | LowPart` of the adapter LUID (PROTOCOL 8 `GpuDevice.luid`). Windows
    /// assigns it at boot, so it changes across reboots and driver restarts.
    pub luid: u64,
    /// `nvidia`, `amd`, `intel` or `other`.
    pub vendor: String,
    pub name: String,
    pub vram_mb: u64,
    /// The user-mode driver version DXGI reports (`a.b.c.d`), empty if unknown.
    pub umd_driver: String,
}

impl GpuAdapter {
    pub fn luid_hex(&self) -> String {
        luid_hex(self.luid)
    }
}

/// A LUID as the profile and the job line carry it: 16 lowercase hex digits.
pub fn luid_hex(luid: u64) -> String {
    format!("{luid:016x}")
}

/// The inverse of [`luid_hex`] (any case, optional `0x`).
pub fn parse_luid_hex(s: &str) -> Option<u64> {
    let s = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")).unwrap_or(s);
    if s.is_empty() || s.len() > 16 {
        return None;
    }
    u64::from_str_radix(s, 16).ok()
}

/// `(HighPart, LowPart)` of a Windows LUID as one number.
pub fn luid_u64(high: i32, low: u32) -> u64 {
    ((high as u32 as u64) << 32) | low as u64
}

/// PCI vendor id → profile vendor name; `None` for adapters that are not GPUs (Microsoft's Basic
/// Render Driver and remote-display adapters).
pub fn vendor_name(vendor_id: u32) -> Option<&'static str> {
    match vendor_id {
        0x10DE => Some("nvidia"),
        0x1002 | 0x1022 => Some("amd"),
        0x8086 => Some("intel"),
        0x1414 => None,
        _ => Some("other"),
    }
}

/// Keeps the first adapter of each LUID, in order.
pub fn dedup_by_luid(adapters: Vec<GpuAdapter>) -> Vec<GpuAdapter> {
    let mut seen = std::collections::HashSet::new();
    adapters.into_iter().filter(|a| seen.insert(a.luid)).collect()
}

/// The hardware GPU adapters (DXGI, all vendors), de-duplicated by LUID.
pub fn adapters() -> Vec<GpuAdapter> {
    use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIDevice, IDXGIFactory1, DXGI_ADAPTER_FLAG_SOFTWARE};
    let mut out = Vec::new();
    let Ok(factory) = (unsafe { CreateDXGIFactory1::<IDXGIFactory1>() }) else { return out };
    let mut i = 0;
    while let Ok(a) = unsafe { factory.EnumAdapters1(i) } {
        i += 1;
        let Ok(d) = (unsafe { a.GetDesc1() }) else { continue };
        if d.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0 {
            continue;
        }
        let Some(vendor) = vendor_name(d.VendorId) else { continue };
        let umd_driver = unsafe { a.CheckInterfaceSupport(&<IDXGIDevice as windows::core::Interface>::IID) }
            .map(|v| {
                let v = v as u64;
                format!("{}.{}.{}.{}", v >> 48, (v >> 32) & 0xffff, (v >> 16) & 0xffff, v & 0xffff)
            })
            .unwrap_or_default();
        out.push(GpuAdapter {
            luid: luid_u64(d.AdapterLuid.HighPart, d.AdapterLuid.LowPart),
            vendor: vendor.into(),
            name: crate::util::from_wide(&d.Description),
            vram_mb: (d.DedicatedVideoMemory as u64) >> 20,
            umd_driver,
        });
    }
    dedup_by_luid(out)
}

/// GPUs for the profile: [`adapters`] with their LUIDs. NVIDIA driver versions via NVML when it
/// is loadable (and its device count checked against the NVIDIA adapters); others report the
/// user-mode driver version DXGI gives.
pub fn gpus(nvml: Option<&nvml_wrapper::Nvml>) -> Vec<Gpu> {
    let adapters = adapters();
    if let Some(n) = nvml {
        let nv = adapters.iter().filter(|a| a.vendor == "nvidia").count();
        match n.device_count() {
            Ok(c) if c as usize != nv => {
                log::warn!("NVML sees {c} NVIDIA GPU(s) but DXGI lists {nv} NVIDIA adapter(s) (after de-duplication by LUID)")
            }
            Ok(_) => {}
            Err(e) => log::warn!("NVML device count failed: {e}"),
        }
    }
    let nv_driver = nvml.and_then(|n| n.sys_driver_version().ok());
    adapters
        .into_iter()
        .map(|a| {
            let luid = Some(a.luid_hex());
            let driver = if a.vendor == "nvidia" { nv_driver.clone().unwrap_or(a.umd_driver) } else { a.umd_driver };
            Gpu { vendor: a.vendor, name: a.name, vram_mb: a.vram_mb, driver, luid }
        })
        .collect()
}

/// The ONNX Runtime execution providers that passed this PC's probes (probe.rs, PROTOCOL 8) with
/// these adapters and drivers: (any runtime pack, per runtime pack). `["cpu"]` before any probe.
pub fn providers(root: &std::path::Path, gpus: &[Gpu]) -> (Vec<String>, Option<std::collections::BTreeMap<String, Vec<String>>>) {
    crate::probe::profile_providers(&zerofarm_common::layout::Layout::new(root), gpus)
}

pub fn detect(root: &std::path::Path) -> Profile {
    let (cores, threads, perf, eff) = cores();
    let (_, total) = crate::signals::memory_mb();
    let nvml = crate::signals::load_nvml().ok();
    // Rounded to 10 GB so the profile digest doesn't change with every download.
    let free_mb = crate::util::disk_free_bytes(root).map(|b| (b >> 20) / 10240 * 10240).unwrap_or(0);
    let avail = crate::availability::load(&zerofarm_common::layout::Layout::new(root)).means(&crate::util::local_now().0);
    let gpus = gpus(nvml.as_ref());
    let (providers, runtime_providers) = providers(root, &gpus);
    Profile {
        os: os_name(),
        cpu: cpu_brand(),
        cores,
        threads,
        perf_cores: perf,
        eff_cores: eff,
        ram_mb: total,
        disk_free_mb: free_mb,
        gpus,
        providers,
        runtime_providers,
        idle_h_day: avail.map(|a| a.0),
        on_h_day: avail.map(|a| a.1),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn adapter(luid: u64, vendor: &str, name: &str) -> GpuAdapter {
        GpuAdapter { luid, vendor: vendor.into(), name: name.into(), vram_mb: 16376, umd_driver: String::new() }
    }

    #[test]
    fn luid_hex_round_trip() {
        assert_eq!(luid_u64(0, 0xD1F3), 0xD1F3);
        assert_eq!(luid_u64(1, 0x8000_0000), 0x1_8000_0000);
        assert_eq!(luid_u64(-1, 0), 0xFFFF_FFFF_0000_0000, "HighPart is not sign-extended into the low half");
        assert_eq!(luid_hex(0xD1F3), "000000000000d1f3");
        assert_eq!(luid_hex(luid_u64(0x12, 0xABCDEF01)), "00000012abcdef01");
        for l in [0, 0xD1F3, 0x1_8000_0000, u64::MAX] {
            assert_eq!(parse_luid_hex(&luid_hex(l)), Some(l));
        }
        assert_eq!(parse_luid_hex("0x0000D1F3"), Some(0xD1F3));
        assert_eq!(parse_luid_hex(""), None);
        assert_eq!(parse_luid_hex("xyz"), None);
        assert_eq!(parse_luid_hex("00000000000000001"), None, "17 digits");
    }

    #[test]
    fn vendors() {
        assert_eq!(vendor_name(0x10DE), Some("nvidia"));
        assert_eq!(vendor_name(0x1002), Some("amd"));
        assert_eq!(vendor_name(0x8086), Some("intel"));
        assert_eq!(vendor_name(0x5143), Some("other"));
        assert_eq!(vendor_name(0x1414), None, "Microsoft Basic Render Driver");
    }

    #[test]
    fn one_card_listed_twice_is_one_gpu() {
        let v = vec![
            adapter(0xD1F3, "nvidia", "NVIDIA GeForce RTX 4080"),
            adapter(0xD1F3, "nvidia", "NVIDIA GeForce RTX 4080"),
            adapter(0x1_0000, "intel", "Intel(R) UHD Graphics 770"),
        ];
        let d = dedup_by_luid(v);
        assert_eq!(d.len(), 2);
        assert_eq!(d[0].luid, 0xD1F3);
        assert_eq!(d[1].vendor, "intel");
        // Two identical cards are two adapters (different LUIDs).
        let twins = dedup_by_luid(vec![adapter(1, "nvidia", "RTX 4080"), adapter(2, "nvidia", "RTX 4080")]);
        assert_eq!(twins.len(), 2);
    }
}

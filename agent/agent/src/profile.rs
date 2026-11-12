//! The machine profile (PROTOCOL 3, DESIGN 5.4): CPU model, performance/efficiency cores, RAM,
//! free disk, GPUs (DXGI, plus NVML for NVIDIA drivers), Windows build, and the ONNX Runtime
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

/// GPUs via DXGI (all vendors), skipping software adapters. NVIDIA driver versions via NVML when
/// it is loadable; others report the user-mode driver version DXGI gives.
pub fn gpus(nvml: Option<&nvml_wrapper::Nvml>) -> Vec<Gpu> {
    use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIDevice, IDXGIFactory1, DXGI_ADAPTER_FLAG_SOFTWARE};
    let mut out = Vec::new();
    let Ok(factory) = (unsafe { CreateDXGIFactory1::<IDXGIFactory1>() }) else { return out };
    let nv_driver = nvml.and_then(|n| n.sys_driver_version().ok());
    let mut i = 0;
    while let Ok(a) = unsafe { factory.EnumAdapters1(i) } {
        i += 1;
        let Ok(d) = (unsafe { a.GetDesc1() }) else { continue };
        if d.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0 {
            continue;
        }
        let vendor = match d.VendorId {
            0x10DE => "nvidia",
            0x1002 | 0x1022 => "amd",
            0x8086 => "intel",
            0x1414 => continue, // Microsoft Basic Render Driver
            _ => "other",
        };
        let umd = unsafe { a.CheckInterfaceSupport(&<IDXGIDevice as windows::core::Interface>::IID) }
            .map(|v| {
                let v = v as u64;
                format!("{}.{}.{}.{}", v >> 48, (v >> 32) & 0xffff, (v >> 16) & 0xffff, v & 0xffff)
            })
            .unwrap_or_default();
        let driver = if vendor == "nvidia" { nv_driver.clone().unwrap_or(umd) } else { umd };
        out.push(Gpu {
            vendor: vendor.into(),
            name: crate::util::from_wide(&d.Description),
            vram_mb: (d.DedicatedVideoMemory as u64) >> 20,
            driver,
        });
    }
    out
}

/// ONNX Runtime execution providers available on this PC. The runtime pack will report the real
/// list (Windows ML catalog); until then only the CPU provider is claimed.
pub fn providers() -> Vec<String> {
    vec!["cpu".into()]
}

pub fn detect(root: &std::path::Path) -> Profile {
    let (cores, threads, perf, eff) = cores();
    let (_, total) = crate::signals::memory_mb();
    let nvml = crate::signals::load_nvml().ok();
    // Rounded to 10 GB so the profile digest doesn't change with every download.
    let free_mb = crate::util::disk_free_bytes(root).map(|b| (b >> 20) / 10240 * 10240).unwrap_or(0);
    let avail = crate::availability::load(&zerofarm_common::layout::Layout::new(root)).means(&crate::util::local_now().0);
    Profile {
        os: os_name(),
        cpu: cpu_brand(),
        cores,
        threads,
        perf_cores: perf,
        eff_cores: eff,
        ram_mb: total,
        disk_free_mb: free_mb,
        gpus: gpus(nvml.as_ref()),
        providers: providers(),
        idle_h_day: avail.map(|a| a.0),
        on_h_day: avail.map(|a| a.1),
    }
}

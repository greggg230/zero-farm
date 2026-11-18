//! The macOS calls the agent needs (PROTOCOL 11.5), as small FFI declarations: `sysctl`, CoreFoundation and IOKit
//! (input idle time, the power source, the power assertion that keeps a working Mac awake), Low Power Mode
//! (`NSProcessInfo` through the Objective-C runtime), memory and CPU statistics, and per-process accounting
//! (`proc_pid_rusage`: CPU time and physical footprint; the processes of a group). Only numbers and yes/no answers
//! come out: no process, window or file names are read.

#![allow(non_upper_case_globals, non_camel_case_types, non_snake_case)]
// libc marks its Mach declarations deprecated in favour of the `mach2` crate; these three calls don't need a crate.
#![allow(deprecated)]

use std::ffi::{c_char, c_int, c_uint, c_void, CStr, CString};
use std::sync::OnceLock;

// ---------- sysctl ----------

fn sysctl_raw(name: &str) -> Option<Vec<u8>> {
    let n = CString::new(name).ok()?;
    let mut len: libc::size_t = 0;
    // SAFETY: a NUL-terminated name; first call asks for the size, the second fills a buffer of that size.
    unsafe {
        if libc::sysctlbyname(n.as_ptr(), std::ptr::null_mut(), &mut len, std::ptr::null_mut(), 0) != 0 || len == 0 {
            return None;
        }
        let mut buf = vec![0u8; len];
        if libc::sysctlbyname(n.as_ptr(), buf.as_mut_ptr() as *mut c_void, &mut len, std::ptr::null_mut(), 0) != 0 {
            return None;
        }
        buf.truncate(len);
        Some(buf)
    }
}

/// A string sysctl (`machdep.cpu.brand_string`, `kern.osproductversion`).
pub fn sysctl_string(name: &str) -> Option<String> {
    let b = sysctl_raw(name)?;
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    let s = String::from_utf8_lossy(&b[..end]).trim().to_string();
    (!s.is_empty()).then_some(s)
}

/// An integer sysctl of 4 or 8 bytes (`hw.memsize`, `hw.physicalcpu`).
pub fn sysctl_u64(name: &str) -> Option<u64> {
    let b = sysctl_raw(name)?;
    match b.len() {
        4 => Some(u32::from_ne_bytes(b[..4].try_into().ok()?) as u64),
        8 => Some(u64::from_ne_bytes(b[..8].try_into().ok()?)),
        _ => None,
    }
}

// ---------- CoreFoundation ----------

type CFTypeRef = *const c_void;
type CFStringRef = *const c_void;
type CFIndex = isize;
type CFTypeID = usize;
const kCFStringEncodingUTF8: u32 = 0x0800_0100;
const kCFNumberSInt64Type: CFIndex = 4;

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFStringCreateWithCString(alloc: *const c_void, cstr: *const c_char, encoding: u32) -> CFStringRef;
    fn CFStringGetCString(s: CFStringRef, buf: *mut c_char, size: CFIndex, encoding: u32) -> u8;
    fn CFStringGetTypeID() -> CFTypeID;
    fn CFNumberGetTypeID() -> CFTypeID;
    fn CFNumberGetValue(n: CFTypeRef, kind: CFIndex, out: *mut c_void) -> u8;
    fn CFGetTypeID(cf: CFTypeRef) -> CFTypeID;
    fn CFRelease(cf: CFTypeRef);
}

/// A CoreFoundation object this code owns (released on drop).
struct Cf(CFTypeRef);
impl Drop for Cf {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: an object obtained from a Create/Copy function, released once.
            unsafe { CFRelease(self.0) };
        }
    }
}

fn cfstr(s: &str) -> Cf {
    let c = CString::new(s).expect("no NUL");
    // SAFETY: a NUL-terminated UTF-8 string; the default allocator.
    Cf(unsafe { CFStringCreateWithCString(std::ptr::null(), c.as_ptr(), kCFStringEncodingUTF8) })
}

/// The text of a CFString (not consumed).
fn cf_string(s: CFStringRef) -> Option<String> {
    // SAFETY: `s` is null or a live CF object; the buffer's size is passed.
    unsafe {
        if s.is_null() || CFGetTypeID(s) != CFStringGetTypeID() {
            return None;
        }
        let mut buf = [0 as c_char; 256];
        (CFStringGetCString(s, buf.as_mut_ptr(), buf.len() as CFIndex, kCFStringEncodingUTF8) != 0).then(|| CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned())
    }
}

// ---------- IOKit ----------

type io_object_t = c_uint;
/// `kIOMainPortDefault` (formerly `kIOMasterPortDefault`).
const kIOMainPortDefault: c_uint = 0;
const kIOPMAssertionLevelOn: u32 = 255;

#[link(name = "IOKit", kind = "framework")]
extern "C" {
    fn IOServiceMatching(name: *const c_char) -> *mut c_void;
    fn IOServiceGetMatchingService(main_port: c_uint, matching: *mut c_void) -> io_object_t;
    fn IORegistryEntryCreateCFProperty(entry: io_object_t, key: CFStringRef, alloc: *const c_void, options: u32) -> CFTypeRef;
    fn IOObjectRelease(obj: io_object_t) -> c_int;
    fn IOPSCopyPowerSourcesInfo() -> CFTypeRef;
    fn IOPSGetProvidingPowerSourceType(snapshot: CFTypeRef) -> CFStringRef;
    fn IOPMAssertionCreateWithName(kind: CFStringRef, level: u32, name: CFStringRef, id: *mut u32) -> c_int;
    fn IOPMAssertionRelease(id: u32) -> c_int;
}

/// Seconds since the last keyboard, mouse or trackpad input (`IOHIDSystem`'s `HIDIdleTime`, nanoseconds). None if
/// IOKit doesn't say.
pub fn hid_idle_seconds() -> Option<u64> {
    let name = CString::new("IOHIDSystem").expect("no NUL");
    // SAFETY: IOServiceMatching's dictionary is consumed by IOServiceGetMatchingService; the service and the property
    // are released here.
    unsafe {
        let svc = IOServiceGetMatchingService(kIOMainPortDefault, IOServiceMatching(name.as_ptr()));
        if svc == 0 {
            return None;
        }
        let key = cfstr("HIDIdleTime");
        let v = Cf(IORegistryEntryCreateCFProperty(svc, key.0, std::ptr::null(), 0));
        IOObjectRelease(svc);
        if v.0.is_null() || CFGetTypeID(v.0) != CFNumberGetTypeID() {
            return None;
        }
        let mut ns: i64 = 0;
        (CFNumberGetValue(v.0, kCFNumberSInt64Type, &mut ns as *mut i64 as *mut c_void) != 0).then(|| (ns.max(0) as u64) / 1_000_000_000)
    }
}

/// Whether the Mac runs on AC power (the providing power source): Some(true) on AC (a desktop Mac always),
/// Some(false) on its battery or a UPS, None when IOKit doesn't say.
pub fn on_ac() -> Option<bool> {
    // SAFETY: the snapshot is released here; the type string belongs to it (Get rule) and is read before that.
    unsafe {
        let snap = Cf(IOPSCopyPowerSourcesInfo());
        if snap.0.is_null() {
            return None;
        }
        match cf_string(IOPSGetProvidingPowerSourceType(snap.0)).as_deref() {
            Some("AC Power") => Some(true),
            Some("Battery Power") | Some("UPS Power") => Some(false),
            _ => None,
        }
    }
}

/// A power assertion that keeps the Mac from idle-sleeping (`PreventUserIdleSystemSleep`, as `caffeinate -i`; the
/// display still sleeps), released on drop.
pub struct AwakeAssertion(u32);

impl AwakeAssertion {
    pub fn take(reason: &str) -> Option<AwakeAssertion> {
        let (kind, name) = (cfstr("PreventUserIdleSystemSleep"), cfstr(reason));
        let mut id = 0u32;
        // SAFETY: two live CFStrings and an out pointer.
        (unsafe { IOPMAssertionCreateWithName(kind.0, kIOPMAssertionLevelOn, name.0, &mut id) } == 0).then_some(AwakeAssertion(id))
    }
}

impl Drop for AwakeAssertion {
    fn drop(&mut self) {
        // SAFETY: an assertion id this process created, released once.
        unsafe { IOPMAssertionRelease(self.0) };
    }
}

// ---------- Low Power Mode (NSProcessInfo) ----------

#[link(name = "objc")]
extern "C" {
    fn objc_getClass(name: *const c_char) -> *mut c_void;
    fn sel_registerName(name: *const c_char) -> *mut c_void;
    fn objc_msgSend();
}
#[link(name = "Foundation", kind = "framework")]
extern "C" {}

/// `NSProcessInfo.processInfo.isLowPowerModeEnabled` (macOS 12+; false before, or when it can't be asked).
pub fn low_power_mode() -> bool {
    let sel = |s: &str| {
        let c = CString::new(s).expect("no NUL");
        // SAFETY: a NUL-terminated selector name; selectors are interned and never freed.
        unsafe { sel_registerName(c.as_ptr()) }
    };
    let cls_name = CString::new("NSProcessInfo").expect("no NUL");
    // SAFETY: objc_msgSend called through the exact non-variadic signatures of these three methods:
    // +processInfo (an id), -respondsToSelector: (a BOOL) and -isLowPowerModeEnabled (a BOOL). processInfo is the
    // shared instance: nothing is allocated or autoreleased.
    unsafe {
        let cls = objc_getClass(cls_name.as_ptr());
        if cls.is_null() {
            return false;
        }
        let send_id: unsafe extern "C" fn(*mut c_void, *mut c_void) -> *mut c_void = std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
        let send_bool: unsafe extern "C" fn(*mut c_void, *mut c_void) -> u8 = std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
        let send_responds: unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void) -> u8 = std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
        let pi = send_id(cls, sel("processInfo"));
        if pi.is_null() {
            return false;
        }
        let lpm = sel("isLowPowerModeEnabled");
        if send_responds(pi, sel("respondsToSelector:"), lpm) == 0 {
            return false;
        }
        send_bool(pi, lpm) != 0
    }
}

// ---------- memory and CPU ----------

fn host() -> libc::mach_port_t {
    // (once: every mach_host_self() adds a reference to the host port, and references overflow)
    static H: OnceLock<libc::mach_port_t> = OnceLock::new();
    // SAFETY: no preconditions.
    *H.get_or_init(|| unsafe { libc::mach_host_self() })
}

fn page_size() -> u64 {
    // SAFETY: no preconditions.
    let p = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if p > 0 {
        p as u64
    } else {
        16384
    }
}

/// `kern.memorystatus_vm_pressure_level`: 1 normal, 2 warning, 4 critical (0 if unknown).
pub fn memory_pressure_level() -> u64 {
    sysctl_u64("kern.memorystatus_vm_pressure_level").unwrap_or(0)
}

/// (available, total) physical memory in MB. Available = free + inactive pages (what macOS hands out without
/// compressing or swapping); 0 while the system reports memory pressure (warning or critical), so the policy's
/// `low_memory` follows macOS's own judgement (PROTOCOL 11.5).
pub fn memory_mb() -> (u64, u64) {
    let total = sysctl_u64("hw.memsize").unwrap_or(0) >> 20;
    let mut vm: libc::vm_statistics64 = unsafe { std::mem::zeroed() };
    let mut count = libc::HOST_VM_INFO64_COUNT;
    // SAFETY: a vm_statistics64 buffer and its size in integers.
    let ok = unsafe { libc::host_statistics64(host(), libc::HOST_VM_INFO64, &mut vm as *mut _ as *mut i32, &mut count) } == 0;
    if !ok {
        return (0, total);
    }
    let avail = ((vm.free_count as u64 + vm.inactive_count as u64) * page_size()) >> 20;
    if memory_pressure_level() >= 2 {
        return (0, total);
    }
    (avail.min(total), total)
}

/// The system's CPU time so far (ticks over all cores): (idle, total).
pub fn cpu_ticks() -> (u64, u64) {
    let mut info: libc::host_cpu_load_info = unsafe { std::mem::zeroed() };
    let mut count = libc::HOST_CPU_LOAD_INFO_COUNT;
    // SAFETY: a host_cpu_load_info buffer and its size in integers.
    if unsafe { libc::host_statistics(host(), libc::HOST_CPU_LOAD_INFO, &mut info as *mut _ as *mut i32, &mut count) } != 0 {
        return (0, 0);
    }
    let t = info.cpu_ticks;
    let total: u64 = t.iter().map(|&x| x as u64).sum();
    (t[libc::CPU_STATE_IDLE as usize] as u64, total)
}

/// Mach absolute time units -> nanoseconds (`rusage_info` CPU times are in them on Apple silicon).
fn mach_to_ns(t: u64) -> u64 {
    static TB: OnceLock<(u32, u32)> = OnceLock::new();
    let (n, d) = *TB.get_or_init(|| {
        let mut tb = libc::mach_timebase_info { numer: 0, denom: 0 };
        // SAFETY: an out struct.
        unsafe { libc::mach_timebase_info(&mut tb) };
        if tb.denom == 0 {
            (1, 1)
        } else {
            (tb.numer, tb.denom)
        }
    });
    ((t as u128 * n as u128) / d as u128) as u64
}

fn rusage(pid: i32) -> Option<libc::rusage_info_v2> {
    let mut ri: libc::rusage_info_v2 = unsafe { std::mem::zeroed() };
    // SAFETY: a rusage_info_v2 buffer for the V2 flavor.
    (unsafe { libc::proc_pid_rusage(pid, libc::RUSAGE_INFO_V2, &mut ri as *mut _ as *mut libc::rusage_info_t) } == 0).then_some(ri)
}

/// CPU time (user + system) a process has used, ms.
pub fn proc_cpu_ms(pid: i32) -> Option<u64> {
    rusage(pid).map(|ri| mach_to_ns(ri.ri_user_time + ri.ri_system_time) / 1_000_000)
}

/// A process's physical footprint (what Activity Monitor calls its memory: dirty, compressed and swapped pages),
/// bytes.
pub fn proc_footprint(pid: i32) -> Option<u64> {
    rusage(pid).map(|ri| ri.ri_phys_footprint)
}

/// The processes of process group `pgid`.
pub fn group_pids(pgid: i32) -> Vec<i32> {
    let mut buf = vec![0i32; 1024];
    // SAFETY: the buffer and its size in bytes.
    let n = unsafe { libc::proc_listpgrppids(pgid, buf.as_mut_ptr() as *mut c_void, (buf.len() * 4) as c_int) };
    if n <= 0 {
        return vec![];
    }
    buf.truncate((n as usize / 4).min(buf.len()));
    buf.retain(|&p| p > 0);
    buf
}

/// When a process started (seconds, microseconds since the epoch): with its pid, what identifies it (a pid alone
/// can be reused).
pub fn proc_start(pid: i32) -> Option<(u64, u64)> {
    let mut bi: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as c_int;
    // SAFETY: a proc_bsdinfo buffer and its size.
    let n = unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, &mut bi as *mut _ as *mut c_void, size) };
    (n == size).then_some((bi.pbi_start_tvsec, bi.pbi_start_tvusec))
}

// ---------- the rest ----------

/// "macOS 26.6" (major.minor of `kern.osproductversion`).
pub fn os_name() -> String {
    match sysctl_string("kern.osproductversion") {
        Some(v) => format!("macOS {}", v.split('.').take(2).collect::<Vec<_>>().join(".")),
        None => "macOS".into(),
    }
}

/// The host name without `.local` (the friend can change it at setup).
pub fn host_name() -> Option<String> {
    let mut buf = [0 as c_char; 256];
    // SAFETY: the buffer and its size; gethostname NUL-terminates within it.
    if unsafe { libc::gethostname(buf.as_mut_ptr(), buf.len() - 1) } != 0 {
        return None;
    }
    let s = unsafe { CStr::from_ptr(buf.as_ptr()) }.to_string_lossy().into_owned();
    let s = s.strip_suffix(".local").unwrap_or(&s).trim().to_string();
    (!s.is_empty()).then_some(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readings_are_sane() {
        let (avail, total) = memory_mb();
        assert!(total >= 1024 && avail <= total, "{avail} / {total}");
        assert!(sysctl_u64("hw.physicalcpu").unwrap() >= 1);
        assert!(sysctl_string("machdep.cpu.brand_string").is_some());
        assert!(os_name().starts_with("macOS "));
        let me = std::process::id() as i32;
        assert!(proc_footprint(me).unwrap() > 0);
        assert!(proc_cpu_ms(me).is_some());
        assert!(proc_start(me).is_some());
        let (idle, total) = cpu_ticks();
        assert!(total > 0 && idle <= total);
        // (on a headless test Mac these may be None; they must not crash)
        let _ = (hid_idle_seconds(), on_ac(), low_power_mode());
        let pg = unsafe { libc::getpgrp() };
        assert!(group_pids(pg).contains(&me) || group_pids(pg).is_empty());
    }
}

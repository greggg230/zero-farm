//! Per-user install and uninstall (DESIGN 3.1): everything under one root (default
//! `%LOCALAPPDATA%\ZeroFarm`), an HKCU `Run` value for the launcher, a Start-menu shortcut and an
//! HKCU Uninstall entry. No admin rights and no UAC: only HKCU and the user's own folders.
//!
//! The registry value / key name is a parameter (`ZeroFarm` for real installs) so tests use a
//! throwaway name and never touch the real entries.

use crate::util::{wide, wide_path};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::ERROR_SUCCESS;
use windows_sys::Win32::System::Registry::*;
use zerofarm_common::keys::TrustedKeys;
use zerofarm_common::layout::{Layout, AGENT_EXE, LAUNCHER_EXE};
use zerofarm_common::manifest::{verify_dir, Expect, Kind};
use zerofarm_common::names::{instance_running, QuitEvent};

pub const DEFAULT_REG_NAME: &str = "ZeroFarm";
const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
const UNINSTALL_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall";
const DETACHED_PROCESS: u32 = 0x0000_0008;
const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[derive(Debug, Clone)]
pub struct InstallOpts {
    pub root: PathBuf,
    pub reg_name: String,
    /// Where the shortcut goes; `None` = the Start menu's Programs folder.
    pub shortcut_dir: Option<PathBuf>,
    pub no_shortcut: bool,
    /// Start the launcher when done.
    pub start: bool,
    /// The signed agent package to install from (manifest + files, including ZeroFarm.exe).
    pub source: PathBuf,
}

#[derive(Debug, Clone)]
pub struct UninstallOpts {
    pub root: PathBuf,
    pub reg_name: String,
    pub shortcut_dir: Option<PathBuf>,
}

fn display_name(reg_name: &str) -> String {
    if reg_name == DEFAULT_REG_NAME {
        "Zero Farm".into()
    } else {
        format!("Zero Farm ({reg_name})")
    }
}

pub fn shortcut_path(reg_name: &str, dir: Option<&Path>) -> PathBuf {
    let dir = dir.map(PathBuf::from).unwrap_or_else(|| {
        std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_default()
            .join("Microsoft\\Windows\\Start Menu\\Programs")
    });
    dir.join(format!("{}.lnk", display_name(reg_name)))
}

// ---- registry (HKCU only) ----

fn reg_set_sz(subkey: &str, name: &str, value: &str) -> Result<(), String> {
    reg_set(subkey, name, REG_SZ, &wide(value).iter().flat_map(|c| c.to_le_bytes()).collect::<Vec<u8>>())
}

fn reg_set_dword(subkey: &str, name: &str, value: u32) -> Result<(), String> {
    reg_set(subkey, name, REG_DWORD, &value.to_le_bytes())
}

fn reg_set(subkey: &str, name: &str, kind: REG_VALUE_TYPE, data: &[u8]) -> Result<(), String> {
    unsafe {
        let mut hk: HKEY = std::ptr::null_mut();
        let r = RegCreateKeyExW(
            HKEY_CURRENT_USER,
            wide(subkey).as_ptr(),
            0,
            std::ptr::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE,
            std::ptr::null(),
            &mut hk,
            std::ptr::null_mut(),
        );
        if r != ERROR_SUCCESS {
            return Err(format!("open HKCU\\{subkey}: error {r}"));
        }
        let r = RegSetValueExW(hk, wide(name).as_ptr(), 0, kind, data.as_ptr(), data.len() as u32);
        RegCloseKey(hk);
        if r != ERROR_SUCCESS {
            return Err(format!("set HKCU\\{subkey}\\{name}: error {r}"));
        }
    }
    Ok(())
}

/// Reads a REG_SZ value under HKCU.
pub fn reg_get_sz(subkey: &str, name: &str) -> Option<String> {
    unsafe {
        let mut buf = vec![0u16; 2048];
        let mut len = (buf.len() * 2) as u32;
        let r = RegGetValueW(
            HKEY_CURRENT_USER,
            wide(subkey).as_ptr(),
            wide(name).as_ptr(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            buf.as_mut_ptr() as *mut _,
            &mut len,
        );
        (r == ERROR_SUCCESS).then(|| crate::util::from_wide(&buf))
    }
}

fn reg_delete_value(subkey: &str, name: &str) {
    unsafe {
        let mut hk: HKEY = std::ptr::null_mut();
        if RegOpenKeyExW(HKEY_CURRENT_USER, wide(subkey).as_ptr(), 0, KEY_SET_VALUE, &mut hk) == ERROR_SUCCESS {
            RegDeleteValueW(hk, wide(name).as_ptr());
            RegCloseKey(hk);
        }
    }
}

fn reg_delete_tree(subkey: &str) {
    unsafe {
        RegDeleteTreeW(HKEY_CURRENT_USER, wide(subkey).as_ptr());
        RegDeleteKeyW(HKEY_CURRENT_USER, wide(subkey).as_ptr());
    }
}

pub fn run_value(reg_name: &str) -> Option<String> {
    reg_get_sz(RUN_KEY, reg_name)
}

pub fn uninstall_key(reg_name: &str) -> String {
    format!("{UNINSTALL_KEY}\\{reg_name}")
}

// ---- shortcut (COM) ----

fn create_shortcut(lnk: &Path, target: &Path, workdir: &Path, args: &str, desc: &str) -> Result<(), String> {
    use windows::core::{Interface, PCWSTR};
    use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, IPersistFile, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED};
    use windows::Win32::UI::Shell::{IShellLinkW, ShellLink};
    if let Some(d) = lnk.parent() {
        std::fs::create_dir_all(d).map_err(|e| e.to_string())?;
    }
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER).map_err(|e| e.to_string())?;
        let (t, w, a, d) = (wide_path(target), wide_path(workdir), wide(args), wide(desc));
        link.SetPath(PCWSTR(t.as_ptr())).map_err(|e| e.to_string())?;
        link.SetWorkingDirectory(PCWSTR(w.as_ptr())).map_err(|e| e.to_string())?;
        link.SetArguments(PCWSTR(a.as_ptr())).map_err(|e| e.to_string())?;
        link.SetDescription(PCWSTR(d.as_ptr())).map_err(|e| e.to_string())?;
        let pf: IPersistFile = link.cast().map_err(|e| e.to_string())?;
        let l = wide_path(lnk);
        pf.Save(PCWSTR(l.as_ptr()), true).map_err(|e| e.to_string())?;
    }
    Ok(())
}

// ---- processes ----

/// Asks a running launcher/agent for this root to quit and waits for both to be gone.
pub fn stop_running(root: &Path, timeout: Duration) -> bool {
    let running = || instance_running("launcher", root) || instance_running("agent", root);
    if !running() {
        return true;
    }
    let q = QuitEvent::open(root);
    if let Some(q) = &q {
        q.set();
    }
    let end = Instant::now() + timeout;
    while Instant::now() < end {
        if !running() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

/// A detached process must not keep our caller's pipes open: Windows copies every
/// *inheritable* handle into a child, including the std handles we inherited ourselves, so
/// clear their inherit flag (callers exit right after spawning anyway).
fn clear_std_inherit() {
    unsafe {
        use windows_sys::Win32::Foundation::{SetHandleInformation, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE};
        use windows_sys::Win32::System::Console::{GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE};
        for which in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
            let h = GetStdHandle(which);
            if !h.is_null() && h != INVALID_HANDLE_VALUE {
                SetHandleInformation(h, HANDLE_FLAG_INHERIT, 0);
            }
        }
    }
}

/// Starts `exe` fully detached from us (and from our job, when allowed), with no stdio.
pub fn spawn_detached(exe: &Path, args: &[&str], cwd: &Path) -> std::io::Result<()> {
    use std::os::windows::process::CommandExt;
    let flags = DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW;
    clear_std_inherit();
    let try_spawn = |f: u32| {
        std::process::Command::new(exe)
            .args(args)
            .current_dir(cwd)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .creation_flags(f)
            .spawn()
    };
    match try_spawn(flags | CREATE_BREAKAWAY_FROM_JOB) {
        Ok(_) => Ok(()),
        Err(_) => try_spawn(flags).map(|_| ()),
    }
}

// ---- install ----

/// An absolute root path with backslashes (it ends up in the registry and in shortcuts).
pub fn clean_root(root: &Path) -> PathBuf {
    let abs = std::path::absolute(root).unwrap_or_else(|_| root.to_path_buf());
    PathBuf::from(abs.to_string_lossy().replace('/', "\\").trim_end_matches('\\'))
}

pub fn install(opts: &InstallOpts, keys: &TrustedKeys) -> Result<String, String> {
    let opts = &InstallOpts { root: clean_root(&opts.root), ..opts.clone() };
    let pkg = verify_dir(&opts.source, keys, &Expect { kind: Some(Kind::Agent), ..Default::default() })
        .map_err(|e| format!("the installer's files failed verification: {e}"))?;
    let v = pkg.manifest.version.clone();
    if !zerofarm_common::layout::is_version_label(&v) {
        return Err(format!("bad version {v:?}"));
    }
    let has = |name: &str| pkg.manifest.files.iter().any(|f| f.path.eq_ignore_ascii_case(name));
    if !has(AGENT_EXE) || !has(LAUNCHER_EXE) {
        return Err(format!("the package must contain {AGENT_EXE} and {LAUNCHER_EXE}"));
    }
    let layout = Layout::new(&opts.root);
    if !stop_running(&opts.root, Duration::from_secs(30)) {
        return Err("Zero Farm is still running and did not stop; quit it from the tray and try again".into());
    }
    for d in [layout.versions(), layout.state(), layout.cache(), layout.logs()] {
        std::fs::create_dir_all(&d).map_err(|e| format!("{}: {e}", d.display()))?;
    }
    let dest = layout.version_dir(&v);
    let same = dest.exists() && verify_dir(&dest, keys, &Expect { build_id: Some(&pkg.build_id), ..Default::default() }).is_ok();
    if !same {
        if dest.exists() {
            zerofarm_common::fsutil::remove_dir_all_retry(&dest, Duration::from_secs(10)).map_err(|e| e.to_string())?;
        }
        let tmp = layout.versions().join(format!(".{v}.installing"));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).map_err(|e| e.to_string())?;
        for f in pkg.manifest.files.iter().map(|f| f.path.as_str()).chain(["manifest.json", "manifest.json.minisig"]) {
            let rel = f.replace('/', "\\");
            let to = tmp.join(&rel);
            if let Some(p) = to.parent() {
                std::fs::create_dir_all(p).map_err(|e| e.to_string())?;
            }
            std::fs::copy(opts.source.join(&rel), &to).map_err(|e| format!("copy {f}: {e}"))?;
        }
        verify_dir(&tmp, keys, &Expect { build_id: Some(&pkg.build_id), ..Default::default() }).map_err(|e| e.to_string())?;
        zerofarm_common::fsutil::rename_retry(&tmp, &dest).map_err(|e| e.to_string())?;
    }
    std::fs::copy(dest.join(LAUNCHER_EXE), layout.launcher_exe()).map_err(|e| format!("copy launcher: {e}"))?;
    // Point `current` at this version unless a newer, verifiable one is already installed.
    let switch = match layout.current() {
        None => true,
        Some(cur) if cur == v => false,
        Some(cur) => {
            let cur_ok = verify_dir(&layout.version_dir(&cur), keys, &Expect { kind: Some(Kind::Agent), version: Some(&cur), ..Default::default() }).is_ok();
            !cur_ok || !crate::update::is_newer(&cur, &v)
        }
    };
    if switch {
        if let Some(cur) = layout.current().filter(|c| *c != v && layout.version_dir(c).exists()) {
            layout.set_previous(Some(&cur)).map_err(|e| e.to_string())?;
        }
        layout.set_current(&v).map_err(|e| e.to_string())?;
        // Installed by the user: no probation for this version.
        layout.mark_healthy(&v).map_err(|e| e.to_string())?;
    }
    layout.set_pending(None).map_err(|e| e.to_string())?;
    let launcher = layout.launcher_exe();
    reg_set_sz(RUN_KEY, &opts.reg_name, &format!("\"{}\"", launcher.display()))?;
    let uk = uninstall_key(&opts.reg_name);
    let mut uninstall_cmd = format!("\"{}\" uninstall --reg-name {}", launcher.display(), opts.reg_name);
    if let Some(d) = &opts.shortcut_dir {
        uninstall_cmd.push_str(&format!(" --shortcut-dir \"{}\"", d.display()));
    }
    reg_set_sz(&uk, "DisplayName", &display_name(&opts.reg_name))?;
    reg_set_sz(&uk, "DisplayVersion", &v)?;
    reg_set_sz(&uk, "Publisher", "zero-farm")?;
    reg_set_sz(&uk, "InstallLocation", &opts.root.display().to_string())?;
    reg_set_sz(&uk, "DisplayIcon", &launcher.display().to_string())?;
    reg_set_sz(&uk, "UninstallString", &uninstall_cmd)?;
    reg_set_sz(&uk, "QuietUninstallString", &format!("{uninstall_cmd} --quiet"))?;
    reg_set_dword(&uk, "NoModify", 1)?;
    reg_set_dword(&uk, "NoRepair", 1)?;
    reg_set_dword(&uk, "EstimatedSize", (zerofarm_common::fsutil::dir_size(&opts.root) / 1024) as u32)?;
    if !opts.no_shortcut {
        let lnk = shortcut_path(&opts.reg_name, opts.shortcut_dir.as_deref());
        create_shortcut(&lnk, &launcher, &opts.root, "", "Zero Farm: lend your idle PC to AI training runs")
            .map_err(|e| format!("shortcut: {e}"))?;
    }
    if opts.start {
        spawn_detached(&launcher, &[], &opts.root).map_err(|e| format!("start: {e}"))?;
    }
    Ok(v)
}

// ---- uninstall ----

/// True if `root` looks like a Zero Farm install (so uninstall never deletes a wrong folder).
pub fn looks_installed(root: &Path) -> bool {
    root.join("versions").is_dir() || root.join("state").is_dir() || root.join(LAUNCHER_EXE).is_file()
}

/// Removes the install. The caller must not be running from inside `root` (see
/// [`uninstall_from_anywhere`]).
pub fn uninstall(opts: &UninstallOpts) -> Result<(), String> {
    if !looks_installed(&opts.root) {
        return Err(format!("{} is not a Zero Farm folder", opts.root.display()));
    }
    if !stop_running(&opts.root, Duration::from_secs(30)) {
        return Err("Zero Farm did not stop; quit it from the tray and try again".into());
    }
    // Only remove the Run value if it is ours (points into this root), comparing normalised paths.
    if let Some(v) = run_value(&opts.reg_name) {
        let target = zerofarm_common::names::normalize_root(Path::new(v.trim().trim_matches('"')));
        let root = zerofarm_common::names::normalize_root(&opts.root);
        if target.starts_with(&format!("{root}\\")) {
            reg_delete_value(RUN_KEY, &opts.reg_name);
        } else {
            log::warn!("Run value {} points elsewhere ({v}); leaving it", opts.reg_name);
        }
    }
    reg_delete_tree(&uninstall_key(&opts.reg_name));
    let _ = std::fs::remove_file(shortcut_path(&opts.reg_name, opts.shortcut_dir.as_deref()));
    zerofarm_common::fsutil::remove_dir_all_retry(&opts.root, Duration::from_secs(20)).map_err(|e| format!("remove {}: {e}", opts.root.display()))?;
    Ok(())
}

/// Uninstall entry point: if we run from inside the install, copy ourselves to %TEMP% and let
/// that copy (`--stage2`) do the work, so our own exe can be deleted. Returns true when the
/// work was handed off.
pub fn hand_off_if_inside(opts: &UninstallOpts, extra_args: &[String]) -> Result<bool, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let root_n = zerofarm_common::names::normalize_root(&opts.root);
    let exe_n = zerofarm_common::names::normalize_root(&exe);
    if !exe_n.starts_with(&format!("{root_n}\\")) {
        return Ok(false);
    }
    let tmp = std::env::temp_dir().join(format!("ZeroFarm-uninstall-{}.exe", std::process::id()));
    std::fs::copy(&exe, &tmp).map_err(|e| e.to_string())?;
    let mut args: Vec<String> = vec!["uninstall".into(), "--stage2".into(), "--root".into(), opts.root.display().to_string(), "--reg-name".into(), opts.reg_name.clone()];
    if let Some(d) = &opts.shortcut_dir {
        args.push("--shortcut-dir".into());
        args.push(d.display().to_string());
    }
    args.extend(extra_args.iter().cloned());
    let a: Vec<&str> = args.iter().map(String::as_str).collect();
    spawn_detached(&tmp, &a, &std::env::temp_dir()).map_err(|e| e.to_string())?;
    Ok(true)
}

/// After a stage-2 uninstall from %TEMP%: delete our own copy once we have exited. The command
/// line goes to cmd.exe verbatim (`raw_arg`): cmd does not understand MSVCRT-style `\"` quoting.
pub fn schedule_self_delete() {
    use std::os::windows::process::CommandExt;
    let Ok(exe) = std::env::current_exe() else { return };
    clear_std_inherit();
    let _ = std::process::Command::new("cmd.exe")
        .raw_arg(format!("/d /c ping 127.0.0.1 -n 3 > nul & del /f /q \"{}\"", exe.display()))
        .current_dir(std::env::temp_dir())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW)
        .spawn();
}

/// Removes the Run value and Uninstall key for `reg_name` unconditionally (test cleanup; never
/// called with the real name by the agent itself).
pub fn remove_entries(reg_name: &str) {
    reg_delete_value(RUN_KEY, reg_name);
    reg_delete_tree(&uninstall_key(reg_name));
}

//! The tray icon, its menu and the invite-code dialog (DESIGN 3.1). Plain Win32: a hidden window
//! owns the notification-area icon; the engine runs on its own thread and this thread only
//! reads its status and writes settings.
//!
//! Menu: status, games today; pause 1 h / until tomorrow / indefinitely, resume; mode (idle
//! only / always, low priority); GPU on/off; CPU threads; open logs folder; send diagnostics;
//! about; quit.

use crate::engine::{Control, EngineExit};
use crate::settings::{pause_for, pause_until_tomorrow, Mode, Pause, SharedMachine};
use crate::status::{Phase, SharedStatus};
use crate::util::wide;
use std::cell::RefCell;
use std::sync::Arc;
use std::thread::JoinHandle;
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{EnableWindow, SetFocus};
use windows_sys::Win32::UI::Shell::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

const WM_TRAY: u32 = WM_APP + 1;
const NIN_KEYSELECT: u32 = NIN_SELECT | 0x1;
const TIMER_ID: usize = 1;
const ID_PAUSE_1H: usize = 10;
const ID_PAUSE_TOMORROW: usize = 11;
const ID_PAUSE_FOREVER: usize = 12;
const ID_RESUME: usize = 13;
const ID_MODE_IDLE: usize = 20;
const ID_MODE_ALWAYS: usize = 21;
const ID_GPU: usize = 30;
const ID_THREADS_AUTO: usize = 40;
const ID_THREADS_BASE: usize = 41; // + n
const ID_LOGS: usize = 150;
const ID_ABOUT: usize = 151;
const ID_QUIT: usize = 152;
const ID_DIAG: usize = 153;

/// Starts the engine (given a token) and returns its join handle.
pub type StartEngine = Box<dyn FnMut() -> Result<JoinHandle<EngineExit>, String>>;

/// "Send diagnostics": uploads the logs (blocking; the tray runs it on its own thread) and
/// returns what to tell the friend.
pub type SendDiag = Arc<dyn Fn() -> String + Send + Sync>;

struct Tray {
    hwnd: HWND,
    machine: SharedMachine,
    status: SharedStatus,
    control: Arc<Control>,
    layout: zerofarm_common::layout::Layout,
    version: String,
    logical: u32,
    physical: u32,
    engine: Option<JoinHandle<EngineExit>>,
    exit_code: Option<i32>,
    icon: HICON,
    icon_phase: Option<Phase>,
    taskbar_created: u32,
    quitting: bool,
    send_diag: SendDiag,
    diag_running: Arc<std::sync::atomic::AtomicBool>,
}

thread_local! {
    static TRAY: RefCell<Option<Tray>> = const { RefCell::new(None) };
    static DLG: RefCell<Option<Dlg>> = const { RefCell::new(None) };
}

fn hinstance() -> HINSTANCE {
    unsafe { GetModuleHandleW(std::ptr::null()) }
}

fn copy_wide(dst: &mut [u16], s: &str) {
    let w: Vec<u16> = s.encode_utf16().take(dst.len() - 1).collect();
    dst[..w.len()].copy_from_slice(&w);
    dst[w.len()] = 0;
}

/// A round status icon drawn at runtime: colour by phase, white "Z" (the exe icon is the green one).
fn make_icon(rgb: u32) -> HICON {
    unsafe {
        let size = GetSystemMetrics(SM_CXSMICON).max(16);
        let mut bi: BITMAPINFO = std::mem::zeroed();
        bi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
        bi.bmiHeader.biWidth = size;
        bi.bmiHeader.biHeight = -size;
        bi.bmiHeader.biPlanes = 1;
        bi.bmiHeader.biBitCount = 32;
        bi.bmiHeader.biCompression = BI_RGB;
        let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
        let hdc = GetDC(std::ptr::null_mut());
        let color = CreateDIBSection(hdc, &bi, DIB_RGB_COLORS, &mut bits, std::ptr::null_mut(), 0);
        ReleaseDC(std::ptr::null_mut(), hdc);
        if color.is_null() || bits.is_null() {
            return LoadIconW(std::ptr::null_mut(), IDI_APPLICATION);
        }
        let px = std::slice::from_raw_parts_mut(bits as *mut u32, (size * size) as usize);
        // The same round "Z" as the exe icon (zerofarm_common::icon), premultiplied for GDI.
        for (dst, [r, g, b, a]) in px.iter_mut().zip(zerofarm_common::icon::round_z(size as u32, rgb)) {
            let pre = |c: u8| c as u32 * a as u32 / 255;
            *dst = ((a as u32) << 24) | (pre(r) << 16) | (pre(g) << 8) | pre(b);
        }
        let mask = CreateBitmap(size, size, 1, 1, std::ptr::null());
        let ii = ICONINFO { fIcon: 1, xHotspot: 0, yHotspot: 0, hbmMask: mask, hbmColor: color };
        let icon = CreateIconIndirect(&ii);
        DeleteObject(mask as _);
        DeleteObject(color as _);
        icon
    }
}

fn phase_color(p: Phase) -> u32 {
    match p {
        Phase::Working => 0x2E7D32,
        Phase::Downloading | Phase::Updating => 0x1565C0,
        Phase::Waiting => 0x5C6BC0,
        Phase::Idle | Phase::Paused | Phase::Starting => 0x757575,
        Phase::Offline | Phase::NotEnrolled => 0xEF6C00,
        Phase::Revoked => 0xC62828,
    }
}

impl Tray {
    fn nid(&self) -> NOTIFYICONDATAW {
        let mut n: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
        n.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        n.hWnd = self.hwnd;
        n.uID = 1;
        n
    }

    fn add_icon(&mut self) {
        let mut n = self.nid();
        n.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
        n.uCallbackMessage = WM_TRAY;
        n.hIcon = self.icon;
        copy_wide(&mut n.szTip, "Zero Farm");
        unsafe {
            Shell_NotifyIconW(NIM_ADD, &n);
            n.Anonymous.uVersion = NOTIFYICON_VERSION_4;
            Shell_NotifyIconW(NIM_SETVERSION, &n);
        }
    }

    fn refresh(&mut self) {
        let st = self.status.lock().unwrap().clone();
        if self.icon_phase != Some(st.phase) {
            let old = self.icon;
            self.icon = make_icon(phase_color(st.phase));
            self.icon_phase = Some(st.phase);
            if !old.is_null() {
                unsafe { DestroyIcon(old) };
            }
        }
        let mut n = self.nid();
        n.uFlags = NIF_ICON | NIF_TIP;
        n.hIcon = self.icon;
        copy_wide(&mut n.szTip, &format!("Zero Farm — {}", st.headline()));
        if let Some(t) = st.toast.clone() {
            n.uFlags |= NIF_INFO;
            copy_wide(&mut n.szInfoTitle, "Zero Farm");
            copy_wide(&mut n.szInfo, &t);
            n.dwInfoFlags = NIIF_INFO;
            crate::status::set(&self.status, |s| s.toast = None);
        }
        unsafe { Shell_NotifyIconW(NIM_MODIFY, &n) };
    }

    fn remove_icon(&self) {
        let n = self.nid();
        unsafe { Shell_NotifyIconW(NIM_DELETE, &n) };
    }

    fn save_settings(&self, f: impl FnOnce(&mut crate::settings::Settings)) {
        let mut m = self.machine.lock().unwrap();
        f(&mut m.settings);
        if let Err(e) = m.save(&self.layout) {
            log::error!("could not save settings: {e}");
        }
        drop(m);
        self.control.wake();
    }

    fn show_menu(&mut self) {
        let st = self.status.lock().unwrap().clone();
        let s = self.machine.lock().unwrap().settings.clone();
        let now = zerofarm_common::now_unix();
        unsafe {
            let m = CreatePopupMenu();
            let add = |flags: u32, id: usize, text: &str| {
                let w = wide(text);
                AppendMenuW(m, flags, id, w.as_ptr());
            };
            add(MF_STRING | MF_GRAYED, 1, &format!("Zero Farm {}", self.version));
            add(MF_STRING | MF_GRAYED, 2, &st.headline());
            add(MF_STRING | MF_GRAYED, 3, &format!("Games today: {}", st.games_today));
            if let Some(g) = &st.gpu_note {
                add(MF_STRING | MF_GRAYED, 4, g);
            }
            if let Pause::Until { t } = s.pause {
                if t > now {
                    add(MF_STRING | MF_GRAYED, 5, &format!("Paused for {} more minutes", (t - now + 59) / 60));
                }
            }
            add(MF_SEPARATOR, 0, "");
            let paused = s.paused(now);
            add(MF_STRING, ID_PAUSE_1H, "Pause for 1 hour");
            add(MF_STRING, ID_PAUSE_TOMORROW, "Pause until tomorrow");
            add(MF_STRING, ID_PAUSE_FOREVER, "Pause until I resume");
            add(MF_STRING | if paused { 0 } else { MF_GRAYED }, ID_RESUME, "Resume");
            add(MF_SEPARATOR, 0, "");
            let mode = CreatePopupMenu();
            let idle = wide("Only when I'm away (idle)");
            let always = wide("Always, at low priority");
            AppendMenuW(mode, MF_STRING | if s.mode == Mode::IdleOnly { MF_CHECKED } else { 0 }, ID_MODE_IDLE, idle.as_ptr());
            AppendMenuW(mode, MF_STRING | if s.mode == Mode::Always { MF_CHECKED } else { 0 }, ID_MODE_ALWAYS, always.as_ptr());
            let mode_label = wide("When to work");
            AppendMenuW(m, MF_POPUP, mode as usize, mode_label.as_ptr());
            add(MF_STRING | if s.gpu { MF_CHECKED } else { 0 }, ID_GPU, "Use the graphics card (GPU)");
            let threads = CreatePopupMenu();
            let auto = s.threads(self.physical, self.logical);
            let auto_label = wide(&format!("Automatic ({auto})"));
            AppendMenuW(threads, MF_STRING | if s.cpu_threads.is_none() { MF_CHECKED } else { 0 }, ID_THREADS_AUTO, auto_label.as_ptr());
            for n in thread_choices(self.logical) {
                let label = wide(&n.to_string());
                let checked = s.cpu_threads == Some(n);
                AppendMenuW(threads, MF_STRING | if checked { MF_CHECKED } else { 0 }, ID_THREADS_BASE + n as usize, label.as_ptr());
            }
            let threads_label = wide("CPU threads");
            AppendMenuW(m, MF_POPUP, threads as usize, threads_label.as_ptr());
            add(MF_SEPARATOR, 0, "");
            add(MF_STRING, ID_LOGS, "Open logs folder");
            let busy = self.diag_running.load(std::sync::atomic::Ordering::SeqCst);
            add(MF_STRING | if busy { MF_GRAYED } else { 0 }, ID_DIAG, if busy { "Sending diagnostics…" } else { "Send diagnostics" });
            add(MF_STRING, ID_ABOUT, "About Zero Farm");
            add(MF_STRING, ID_QUIT, "Quit");
            let mut pt = POINT { x: 0, y: 0 };
            GetCursorPos(&mut pt);
            SetForegroundWindow(self.hwnd);
            let cmd = TrackPopupMenu(m, TPM_RETURNCMD | TPM_NONOTIFY | TPM_RIGHTBUTTON, pt.x, pt.y, 0, self.hwnd, std::ptr::null());
            PostMessageW(self.hwnd, WM_NULL, 0, 0);
            DestroyMenu(m);
            if cmd != 0 {
                self.command(cmd as usize);
            }
        }
    }

    fn command(&mut self, id: usize) {
        let now = zerofarm_common::now_unix();
        match id {
            ID_PAUSE_1H => self.save_settings(|s| s.pause = pause_for(now, 3600)),
            ID_PAUSE_TOMORROW => self.save_settings(|s| s.pause = pause_until_tomorrow(now)),
            ID_PAUSE_FOREVER => self.save_settings(|s| s.pause = Pause::Indefinitely),
            ID_RESUME => self.save_settings(|s| s.pause = Pause::None),
            ID_MODE_IDLE => self.save_settings(|s| s.mode = Mode::IdleOnly),
            ID_MODE_ALWAYS => self.save_settings(|s| s.mode = Mode::Always),
            ID_GPU => self.save_settings(|s| s.gpu = !s.gpu),
            ID_THREADS_AUTO => self.save_settings(|s| s.cpu_threads = None),
            ID_LOGS => unsafe {
                let p = crate::util::wide_path(&self.layout.logs());
                let op = wide("open");
                ShellExecuteW(std::ptr::null_mut(), op.as_ptr(), p.as_ptr(), std::ptr::null(), std::ptr::null(), SW_SHOWNORMAL);
            },
            ID_DIAG => {
                use std::sync::atomic::Ordering;
                if self.diag_running.swap(true, Ordering::SeqCst) {
                    return;
                }
                let (send, running, status) = (self.send_diag.clone(), self.diag_running.clone(), self.status.clone());
                // The answer comes back as a balloon (refresh shows `toast`).
                std::thread::spawn(move || {
                    let msg = send();
                    crate::status::set(&status, |s| s.toast = Some(msg));
                    running.store(false, Ordering::SeqCst);
                });
            }
            ID_ABOUT => {
                let m = self.machine.lock().unwrap().clone();
                let text = format!(
                    "Zero Farm {}\n\nThis PC: {} ({})\nGames played today: {}\n\nZero Farm lends your PC to AI training runs while you're away. \
                     It stops within seconds when you come back, never reads your files, and only reports hardware facts \
                     (CPU, RAM, GPU) and how fast games run.",
                    self.version,
                    m.name.clone().unwrap_or_default(),
                    m.machine_id.clone().unwrap_or_default(),
                    m.games_today()
                );
                message_box(self.hwnd, &text, "About Zero Farm", MB_OK | MB_ICONINFORMATION);
            }
            ID_QUIT => {
                self.quitting = true;
                self.remove_icon();
                self.control.request_quit();
            }
            n if (ID_THREADS_BASE..ID_LOGS).contains(&n) => {
                let t = (n - ID_THREADS_BASE) as u32;
                self.save_settings(|s| s.cpu_threads = Some(t));
            }
            _ => {}
        }
    }

    /// Called every second: refresh the icon and notice the engine ending.
    fn tick(&mut self) {
        if self.engine.as_ref().is_some_and(|e| e.is_finished()) {
            let exit = self.engine.take().unwrap().join();
            self.exit_code = Some(match exit {
                Ok(EngineExit::Quit) => 0,
                Ok(EngineExit::Update(_)) => zerofarm_common::EXIT_UPDATE,
                Ok(EngineExit::RestartLauncher) => zerofarm_common::EXIT_RESTART_LAUNCHER,
                Ok(EngineExit::Revoked) => {
                    let mut m = self.machine.lock().unwrap();
                    m.forget_enrollment();
                    let _ = m.save(&self.layout);
                    drop(m);
                    message_box(
                        self.hwnd,
                        "This PC was removed from the farm. Ask for a new invite code to join again.",
                        "Zero Farm",
                        MB_OK | MB_ICONWARNING,
                    );
                    4
                }
                Err(_) => 70,
            });
            self.remove_icon();
            unsafe { PostQuitMessage(0) };
            return;
        }
        if !self.quitting {
            self.refresh();
        }
    }
}

fn thread_choices(logical: u32) -> Vec<u32> {
    let mut v = vec![1u32, 2, 4, 6, 8, 12, 16, 24, 32, 48, 64];
    v.retain(|n| *n <= logical);
    if !v.contains(&logical) {
        v.push(logical);
    }
    v
}

pub fn message_box(hwnd: HWND, text: &str, title: &str, flags: u32) -> i32 {
    let (t, c) = (wide(text), wide(title));
    unsafe { MessageBoxW(hwnd, t.as_ptr(), c.as_ptr(), flags) }
}

unsafe extern "system" fn tray_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let handled = TRAY.with(|t| {
        let mut g = t.borrow_mut();
        let Some(tray) = g.as_mut() else { return false };
        if msg == tray.taskbar_created && msg != 0 {
            tray.add_icon();
            return true;
        }
        match msg {
            WM_TIMER => {
                tray.tick();
                true
            }
            WM_TRAY => {
                let ev = (lp as u32) & 0xffff;
                if ev == WM_CONTEXTMENU || ev == WM_LBUTTONUP || ev == NIN_SELECT || ev == NIN_KEYSELECT {
                    tray.show_menu();
                }
                true
            }
            _ => false,
        }
    });
    if handled {
        return 0;
    }
    DefWindowProcW(hwnd, msg, wp, lp)
}

/// Runs the tray until the engine ends; returns the process exit code (0 quit, 75 update).
#[allow(clippy::too_many_arguments)]
pub fn run(
    machine: SharedMachine,
    status: SharedStatus,
    control: Arc<Control>,
    layout: zerofarm_common::layout::Layout,
    version: String,
    physical: u32,
    logical: u32,
    mut start_engine: StartEngine,
    send_diag: SendDiag,
) -> i32 {
    unsafe {
        let class = wide("ZeroFarmTray");
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: 0,
            lpfnWndProc: Some(tray_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinstance(),
            hIcon: std::ptr::null_mut(),
            hCursor: std::ptr::null_mut(),
            hbrBackground: std::ptr::null_mut(),
            lpszMenuName: std::ptr::null(),
            lpszClassName: class.as_ptr(),
            hIconSm: std::ptr::null_mut(),
        };
        RegisterClassExW(&wc);
        let title = wide("Zero Farm");
        let hwnd = CreateWindowExW(0, class.as_ptr(), title.as_ptr(), WS_OVERLAPPED, 0, 0, 0, 0, std::ptr::null_mut(), std::ptr::null_mut(), hinstance(), std::ptr::null());
        if hwnd.is_null() {
            log::error!("could not create the tray window");
            return 70;
        }
        let engine = match start_engine() {
            Ok(h) => h,
            Err(e) => {
                log::error!("engine did not start: {e}");
                return 70;
            }
        };
        let tc = wide("TaskbarCreated");
        let mut tray = Tray {
            hwnd,
            machine,
            status,
            control,
            layout,
            version,
            logical,
            physical,
            engine: Some(engine),
            exit_code: None,
            icon: std::ptr::null_mut(),
            icon_phase: None,
            taskbar_created: RegisterWindowMessageW(tc.as_ptr()),
            quitting: false,
            send_diag,
            diag_running: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        };
        tray.icon = make_icon(phase_color(Phase::Starting));
        tray.add_icon();
        TRAY.with(|t| *t.borrow_mut() = Some(tray));
        SetTimer(hwnd, TIMER_ID, 1000, None);
        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        let code = TRAY.with(|t| t.borrow().as_ref().and_then(|t| t.exit_code)).unwrap_or(0);
        DestroyWindow(hwnd);
        code
    }
}

// ---------------------------------------------------------------------------------------------
// Invite-code dialog: a small window with two edit boxes. Enter = Join, Esc = Cancel.

pub type TryEnroll<'a> = dyn FnMut(&str, &str) -> Result<(), String> + 'a;

struct Dlg {
    code: HWND,
    name: HWND,
    err: HWND,
    join: HWND,
    try_enroll: *mut TryEnroll<'static>,
    result: Option<bool>,
}

const IDC_CODE: usize = 101;
const IDC_NAME: usize = 102;
const IDOK_: usize = 1;
const IDCANCEL_: usize = 2;

unsafe extern "system" fn dlg_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_COMMAND => {
            let id = wp & 0xffff;
            if id == IDOK_ {
                DLG.with(|d| {
                    let mut g = d.borrow_mut();
                    let Some(dlg) = g.as_mut() else { return };
                    let code = window_text(dlg.code);
                    let name = window_text(dlg.name);
                    if crate::enroll::normalize_invite(&code).is_none() {
                        set_text(dlg.err, "That doesn't look like an invite code. It has 12 letters and digits: XXXX-XXXX-XXXX.");
                        return;
                    }
                    set_text(dlg.err, "Connecting to the farm…");
                    EnableWindow(dlg.join, 0);
                    UpdateWindow(hwnd);
                    let f = &mut *dlg.try_enroll;
                    let r = f(&code, if name.trim().is_empty() { "My PC" } else { name.trim() });
                    EnableWindow(dlg.join, 1);
                    match r {
                        Ok(()) => {
                            dlg.result = Some(true);
                            DestroyWindow(hwnd);
                        }
                        Err(e) => set_text(dlg.err, &e),
                    }
                });
                return 0;
            }
            if id == IDCANCEL_ {
                DLG.with(|d| {
                    if let Some(dlg) = d.borrow_mut().as_mut() {
                        dlg.result = Some(false);
                    }
                });
                DestroyWindow(hwnd);
                return 0;
            }
        }
        WM_CLOSE => {
            DLG.with(|d| {
                if let Some(dlg) = d.borrow_mut().as_mut() {
                    dlg.result = Some(false);
                }
            });
            DestroyWindow(hwnd);
            return 0;
        }
        WM_CTLCOLORSTATIC => {
            SetBkMode(wp as HDC, TRANSPARENT as i32);
            return GetSysColorBrush(COLOR_WINDOW) as LRESULT;
        }
        _ => {}
    }
    DefWindowProcW(hwnd, msg, wp, lp)
}

fn window_text(h: HWND) -> String {
    unsafe {
        let n = GetWindowTextLengthW(h);
        let mut buf = vec![0u16; n as usize + 1];
        GetWindowTextW(h, buf.as_mut_ptr(), buf.len() as i32);
        crate::util::from_wide(&buf)
    }
}

fn set_text(h: HWND, s: &str) {
    let w = wide(s);
    unsafe { SetWindowTextW(h, w.as_ptr()) };
}

/// Shows the dialog until enrollment succeeds (true) or the friend cancels (false).
pub fn invite_dialog(default_name: &str, try_enroll: &mut TryEnroll) -> bool {
    unsafe {
        let _ = windows_sys::Win32::UI::HiDpi::SetProcessDpiAwarenessContext(windows_sys::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_SYSTEM_AWARE);
        let dpi = windows_sys::Win32::UI::HiDpi::GetDpiForSystem().max(96) as i32;
        let sc = |v: i32| v * dpi / 96;
        let class = wide("ZeroFarmInvite");
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: 0,
            lpfnWndProc: Some(dlg_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinstance(),
            hIcon: make_icon(0x2E7D32),
            hCursor: LoadCursorW(std::ptr::null_mut(), IDC_ARROW),
            hbrBackground: (COLOR_WINDOW + 1) as usize as HBRUSH,
            lpszMenuName: std::ptr::null(),
            lpszClassName: class.as_ptr(),
            hIconSm: std::ptr::null_mut(),
        };
        RegisterClassExW(&wc);
        let (w, h) = (sc(460), sc(250));
        let (sw, sh) = (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN));
        let title = wide("Join Zero Farm");
        let hwnd = CreateWindowExW(
            WS_EX_DLGMODALFRAME | WS_EX_TOPMOST,
            class.as_ptr(),
            title.as_ptr(),
            WS_POPUP | WS_CAPTION | WS_SYSMENU,
            (sw - w) / 2,
            (sh - h) / 2,
            w,
            h,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            hinstance(),
            std::ptr::null(),
        );
        if hwnd.is_null() {
            return false;
        }
        let mut ncm: NONCLIENTMETRICSW = std::mem::zeroed();
        ncm.cbSize = std::mem::size_of::<NONCLIENTMETRICSW>() as u32;
        SystemParametersInfoW(SPI_GETNONCLIENTMETRICS, ncm.cbSize, &mut ncm as *mut _ as *mut _, 0);
        let font = CreateFontIndirectW(&ncm.lfMessageFont);
        let mk = |cls: &str, text: &str, style: u32, x: i32, y: i32, cw: i32, ch: i32, id: usize, ex: u32| -> HWND {
            let (c, t) = (wide(cls), wide(text));
            let hw = CreateWindowExW(ex, c.as_ptr(), t.as_ptr(), WS_CHILD | WS_VISIBLE | style, sc(x), sc(y), sc(cw), sc(ch), hwnd, id as HMENU, hinstance(), std::ptr::null());
            SendMessageW(hw, WM_SETFONT, font as WPARAM, 1);
            hw
        };
        mk("STATIC", "Welcome! Enter the invite code you were given:", 0, 16, 14, 420, 20, 0, 0);
        let code = mk("EDIT", "", WS_TABSTOP | WS_BORDER | ES_UPPERCASE as u32 | ES_AUTOHSCROLL as u32, 16, 38, 420, 26, IDC_CODE, WS_EX_CLIENTEDGE);
        mk("STATIC", "A name for this PC (shown on the contributors page):", 0, 16, 74, 420, 20, 0, 0);
        let name = mk("EDIT", default_name, WS_TABSTOP | WS_BORDER | ES_AUTOHSCROLL as u32, 16, 98, 420, 26, IDC_NAME, WS_EX_CLIENTEDGE);
        let err = mk("STATIC", "", 0, 16, 132, 420, 40, 0, 0);
        let join = mk("BUTTON", "Join", WS_TABSTOP | BS_DEFPUSHBUTTON as u32, 236, 176, 96, 30, IDOK_, 0);
        mk("BUTTON", "Cancel", WS_TABSTOP | BS_PUSHBUTTON as u32, 340, 176, 96, 30, IDCANCEL_, 0);
        let ptr: *mut TryEnroll<'static> = std::mem::transmute::<*mut TryEnroll<'_>, *mut TryEnroll<'static>>(try_enroll as *mut _);
        DLG.with(|d| *d.borrow_mut() = Some(Dlg { code, name, err, join, try_enroll: ptr, result: None }));
        ShowWindow(hwnd, SW_SHOW);
        SetForegroundWindow(hwnd);
        SetFocus(code);
        let mut msg: MSG = std::mem::zeroed();
        loop {
            if DLG.with(|d| d.borrow().as_ref().map(|x| x.result.is_some()).unwrap_or(true)) {
                break;
            }
            if GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) <= 0 {
                break;
            }
            if IsDialogMessageW(hwnd, &msg) == 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        let r = DLG.with(|d| d.borrow_mut().take().and_then(|x| x.result)).unwrap_or(false);
        if IsWindow(hwnd) != 0 {
            DestroyWindow(hwnd);
        }
        DeleteObject(font as _);
        r
    }
}

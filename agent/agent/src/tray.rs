//! The tray icon, its menu and the invite-code dialog (DESIGN 3.1). Plain Win32: a hidden window
//! owns the notification-area icon; the engine runs on its own thread and this thread only
//! reads its status and writes settings.
//!
//! Menu: status, games today; pause 1 h / until tomorrow / indefinitely, resume; mode (idle
//! only / always, low priority); GPU on/off; CPU threads; open logs folder; send diagnostics;
//! about; quit.
//!
//! Re-entrancy (0.2.3): SetForegroundWindow, TrackPopupMenu, MessageBox, Shell_NotifyIcon and
//! ShellExecute can pump or dispatch messages, which calls the window procedure again on this
//! thread. 0.2.1 held a `RefCell` borrow across them; a nested message then panicked with
//! "already borrowed" inside the `extern "system"` window procedure, which can't unwind, and the
//! process aborted (0xC0000409) on tray clicks. Now the tray state is only borrowed for short
//! stretches that make no such call: the window procedure collects what to do ([`Ui`]), drops
//! the borrow, then does it; a message that arrives while the state is borrowed is skipped
//! (`try_borrow_mut`); and every window procedure runs inside [`crate::logging::guard_ui`], so a
//! panic is reported (`/v1/crash`, kind `agent`) instead of killing the app.

use crate::engine::{Control, EngineExit};
use crate::settings::{pause_for, pause_until_tomorrow, Mode, Pause, SharedMachine};
use crate::status::{Phase, SharedStatus};
use crate::util::wide;
use std::cell::{Cell, RefCell};
use std::sync::Arc;
use std::thread::JoinHandle;
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Controls::{InitCommonControlsEx, ICC_LINK_CLASS, INITCOMMONCONTROLSEX, LWS_TRANSPARENT, NMHDR, NM_CLICK, NM_RETURN};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{EnableWindow, SetFocus};
use windows_sys::Win32::UI::Shell::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

const WM_TRAY: u32 = WM_APP + 1;
/// Tests only: makes the tray window procedure panic (it must be caught).
#[cfg(test)]
const WM_TEST_PANIC: u32 = WM_APP + 99;
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
    /// The tray menu is open (TrackPopupMenu runs its own loop): further clicks are ignored.
    static MENU_OPEN: Cell<bool> = const { Cell::new(false) };
    /// Explorer restarted while the tray state was busy: add the icon at the next tick.
    static READD_ICON: Cell<bool> = const { Cell::new(false) };
}

/// Runs `f` on the tray state, unless it is already borrowed further up this thread's stack (a
/// message dispatched while a modal call pumps): then nothing happens and the answer is None.
/// `f` must not call anything that can pump or dispatch messages: return [`Ui`] steps instead.
fn with_tray<R>(f: impl FnOnce(&mut Tray) -> R) -> Option<R> {
    TRAY.with(|t| match t.try_borrow_mut() {
        Ok(mut g) => g.as_mut().map(f),
        Err(_) => None,
    })
}

/// A Win32 call that can re-enter the window procedure, made after the tray state is released.
enum Ui {
    Notify(u32, Box<NOTIFYICONDATAW>),
    Message { text: String, title: &'static str, flags: u32 },
    Open(Vec<u16>),
    Quit,
}

fn run_ui(hwnd: HWND, steps: Vec<Ui>) {
    for step in steps {
        match step {
            Ui::Notify(op, n) => unsafe {
                Shell_NotifyIconW(op, &*n);
            },
            Ui::Message { text, title, flags } => {
                message_box(hwnd, &text, title, flags);
            }
            Ui::Open(path) => unsafe {
                let op = wide("open");
                ShellExecuteW(std::ptr::null_mut(), op.as_ptr(), path.as_ptr(), std::ptr::null(), std::ptr::null(), SW_SHOWNORMAL);
            },
            Ui::Quit => unsafe { PostQuitMessage(0) },
        }
    }
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
        Phase::Offline | Phase::NotEnrolled | Phase::Blocked | Phase::SignInFailed => 0xEF6C00,
        Phase::Revoked => 0xC62828,
    }
}

impl Tray {
    fn nid(&self) -> Box<NOTIFYICONDATAW> {
        let mut n: Box<NOTIFYICONDATAW> = Box::new(unsafe { std::mem::zeroed() });
        n.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        n.hWnd = self.hwnd;
        n.uID = 1;
        n
    }

    fn add_icon(&self) -> Vec<Ui> {
        let mut n = self.nid();
        n.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
        n.uCallbackMessage = WM_TRAY;
        n.hIcon = self.icon;
        copy_wide(&mut n.szTip, "Zero Farm");
        let mut v = self.nid();
        v.Anonymous.uVersion = NOTIFYICON_VERSION_4;
        vec![Ui::Notify(NIM_ADD, n), Ui::Notify(NIM_SETVERSION, v)]
    }

    fn refresh(&mut self) -> Vec<Ui> {
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
        vec![Ui::Notify(NIM_MODIFY, n)]
    }

    fn remove_icon(&self) -> Ui {
        Ui::Notify(NIM_DELETE, self.nid())
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

    /// The tray menu (built only: showing it pumps messages, so that happens after the borrow).
    fn build_menu(&self) -> HMENU {
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
            if st.phase == Phase::Blocked {
                // The rest of the explanation (a menu item doesn't wrap).
                for line in crate::status::BLOCKED_MESSAGE.split(". ").skip(1) {
                    let line = if line.ends_with('.') { line.to_string() } else { format!("{line}.") };
                    add(MF_STRING | MF_GRAYED, 6, &line);
                }
            }
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
            m
        }
    }

    fn command(&mut self, id: usize) -> Vec<Ui> {
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
            ID_LOGS => return vec![Ui::Open(crate::util::wide_path(&self.layout.logs()))],
            ID_DIAG => {
                use std::sync::atomic::Ordering;
                if self.diag_running.swap(true, Ordering::SeqCst) {
                    return vec![];
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
                return vec![Ui::Message { text, title: "About Zero Farm", flags: MB_OK | MB_ICONINFORMATION }];
            }
            ID_QUIT => {
                self.quitting = true;
                self.control.request_quit();
                return vec![self.remove_icon()];
            }
            n if (ID_THREADS_BASE..ID_LOGS).contains(&n) => {
                let t = (n - ID_THREADS_BASE) as u32;
                self.save_settings(|s| s.cpu_threads = Some(t));
            }
            _ => {}
        }
        vec![]
    }

    /// Called every second: refresh the icon and notice the engine ending.
    fn tick(&mut self) -> Vec<Ui> {
        if self.engine.as_ref().is_some_and(|e| e.is_finished()) {
            let exit = self.engine.take().unwrap().join();
            self.quitting = true;
            let mut steps = Vec::new();
            self.exit_code = Some(match exit {
                Ok(EngineExit::Quit) => 0,
                Ok(EngineExit::Update(_)) => zerofarm_common::EXIT_UPDATE,
                Ok(EngineExit::RestartLauncher) => zerofarm_common::EXIT_RESTART_LAUNCHER,
                Ok(EngineExit::Revoked) => {
                    let mut m = self.machine.lock().unwrap();
                    m.forget_enrollment();
                    let _ = m.save(&self.layout);
                    drop(m);
                    steps.push(Ui::Message {
                        text: "This PC was removed from the farm. Ask for a new invite code to join again.".into(),
                        title: "Zero Farm",
                        flags: MB_OK | MB_ICONWARNING,
                    });
                    4
                }
                Err(_) => 70,
            });
            steps.push(self.remove_icon());
            steps.push(Ui::Quit);
            return steps;
        }
        if self.quitting {
            return vec![];
        }
        let mut steps = if READD_ICON.with(|r| r.replace(false)) { self.add_icon() } else { vec![] };
        steps.extend(self.refresh());
        steps
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

/// Shows the tray menu at the cursor and runs the chosen command. No borrow is held while the
/// menu is up (TrackPopupMenu runs a message loop).
fn tray_menu(hwnd: HWND) {
    if MENU_OPEN.with(|m| m.get()) {
        return;
    }
    let Some(menu) = with_tray(|t| t.build_menu()) else { return };
    MENU_OPEN.with(|m| m.set(true));
    let cmd = unsafe {
        let mut pt = POINT { x: 0, y: 0 };
        GetCursorPos(&mut pt);
        SetForegroundWindow(hwnd);
        let cmd = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_NONOTIFY | TPM_RIGHTBUTTON, pt.x, pt.y, 0, hwnd, std::ptr::null());
        PostMessageW(hwnd, WM_NULL, 0, 0);
        DestroyMenu(menu);
        cmd
    };
    MENU_OPEN.with(|m| m.set(false));
    if cmd != 0 {
        if let Some(steps) = with_tray(|t| t.command(cmd as usize)) {
            run_ui(hwnd, steps);
        }
    }
}

/// The tray window's procedure. Panics are caught (`guard_ui`): logged and reported, never an abort.
unsafe extern "system" fn tray_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    crate::logging::guard_ui("tray window", || tray_message(hwnd, msg, wp, lp)).unwrap_or(0)
}

fn tray_message(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    #[cfg(test)]
    if msg == WM_TEST_PANIC {
        panic!("test panic in the tray window procedure");
    }
    // TaskbarCreated (Explorer restarted): add the icon again.
    let taskbar_created = TRAY.with(|t| t.try_borrow().ok().and_then(|g| g.as_ref().map(|t| t.taskbar_created)));
    if msg != 0 && taskbar_created == Some(msg) {
        match with_tray(|t| t.add_icon()) {
            Some(steps) => run_ui(hwnd, steps),
            None => READD_ICON.with(|r| r.set(true)),
        }
        return 0;
    }
    match msg {
        WM_TIMER => {
            // Busy (a nested message): skip this tick; the next one comes in a second.
            if let Some(steps) = with_tray(|t| t.tick()) {
                run_ui(hwnd, steps);
            }
            0
        }
        WM_TRAY => {
            let ev = (lp as u32) & 0xffff;
            if ev == WM_CONTEXTMENU || ev == WM_LBUTTONUP || ev == NIN_SELECT || ev == NIN_KEYSELECT {
                tray_menu(hwnd);
            }
            0
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
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
        let add = tray.add_icon();
        TRAY.with(|t| *t.borrow_mut() = Some(tray));
        run_ui(hwnd, add);
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
const IDC_LICENSES: usize = 103;

/// The README's list of the third-party components Zero Farm ships (ONNX Runtime, the Windows ML
/// runtime and DirectML, the MSVC runtime, and for NVIDIA PCs the CUDA runtime, cuBLAS, cuFFT and cuDNN)
/// with their license terms.
pub const THIRD_PARTY_URL: &str = "https://github.com/greggg230/zero-farm#third-party-components";
/// The invite dialog's one line about them (a SysLink: the link part opens [`THIRD_PARTY_URL`]).
pub const THIRD_PARTY_LINE: &str =
    "By installing, you accept the <a href=\"https://github.com/greggg230/zero-farm#third-party-components\">license terms of the included third-party components</a>.";
const IDOK_: usize = 1;
const IDCANCEL_: usize = 2;

/// Like [`with_tray`], for the invite dialog's state.
fn with_dlg<R>(f: impl FnOnce(&mut Dlg) -> R) -> Option<R> {
    DLG.with(|d| match d.try_borrow_mut() {
        Ok(mut g) => g.as_mut().map(f),
        Err(_) => None,
    })
}

unsafe extern "system" fn dlg_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    crate::logging::guard_ui("invite dialog", || unsafe { dlg_message(hwnd, msg, wp, lp) }).unwrap_or(0)
}

/// The dialog's state is borrowed only to copy handles out or set the result; the calls that
/// send messages back to this window (EnableWindow, UpdateWindow, DestroyWindow, ...) and the
/// blocking enrollment run without a borrow.
unsafe fn dlg_message(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_COMMAND => {
            let id = wp & 0xffff;
            if id == IDOK_ {
                let Some((code_h, name_h, err_h, join_h, enroll)) = with_dlg(|d| (d.code, d.name, d.err, d.join, d.try_enroll)) else {
                    return 0;
                };
                let code = window_text(code_h);
                let name = window_text(name_h);
                if crate::enroll::normalize_invite(&code).is_none() {
                    set_text(err_h, "That doesn't look like an invite code. It has 12 letters and digits: XXXX-XXXX-XXXX.");
                    return 0;
                }
                set_text(err_h, "Connecting to the farm…");
                EnableWindow(join_h, 0);
                UpdateWindow(hwnd);
                let f = &mut *enroll;
                let r = f(&code, if name.trim().is_empty() { "My PC" } else { name.trim() });
                EnableWindow(join_h, 1);
                match r {
                    Ok(()) => {
                        with_dlg(|d| d.result = Some(true));
                        DestroyWindow(hwnd);
                    }
                    Err(e) => set_text(err_h, &e),
                }
                return 0;
            }
            if id == IDCANCEL_ {
                with_dlg(|d| d.result = Some(false));
                DestroyWindow(hwnd);
                return 0;
            }
        }
        WM_CLOSE => {
            with_dlg(|d| d.result = Some(false));
            DestroyWindow(hwnd);
            return 0;
        }
        WM_NOTIFY => {
            // the third-party line's link (clicked, or Enter on it): the README's section in the browser
            let hdr = &*(lp as *const NMHDR);
            if hdr.idFrom == IDC_LICENSES && (hdr.code == NM_CLICK || hdr.code == NM_RETURN) {
                let (op, url) = (wide("open"), wide(THIRD_PARTY_URL));
                ShellExecuteW(hwnd, op.as_ptr(), url.as_ptr(), std::ptr::null(), std::ptr::null(), SW_SHOWNORMAL);
                return 0;
            }
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
        // (the SysLink class of the third-party line)
        let icc = INITCOMMONCONTROLSEX { dwSize: std::mem::size_of::<INITCOMMONCONTROLSEX>() as u32, dwICC: ICC_LINK_CLASS };
        InitCommonControlsEx(&icc);
        let (w, h) = (sc(460), sc(290));
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
        mk("SysLink", THIRD_PARTY_LINE, WS_TABSTOP | LWS_TRANSPARENT, 16, 176, 420, 34, IDC_LICENSES, 0);
        let join = mk("BUTTON", "Join", WS_TABSTOP | BS_DEFPUSHBUTTON as u32, 236, 216, 96, 30, IDOK_, 0);
        mk("BUTTON", "Cancel", WS_TABSTOP | BS_PUSHBUTTON as u32, 340, 216, 96, 30, IDCANCEL_, 0);
        let ptr: *mut TryEnroll<'static> = std::mem::transmute::<*mut TryEnroll<'_>, *mut TryEnroll<'static>>(try_enroll as *mut _);
        DLG.with(|d| *d.borrow_mut() = Some(Dlg { code, name, err, join, try_enroll: ptr, result: None }));
        ShowWindow(hwnd, SW_SHOW);
        SetForegroundWindow(hwnd);
        SetFocus(code);
        let mut msg: MSG = std::mem::zeroed();
        loop {
            // (Never busy here: this loop runs outside the window procedure.)
            if DLG.with(|d| d.try_borrow().map(|g| g.as_ref().map(|x| x.result.is_some()).unwrap_or(true)).unwrap_or(false)) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::Machine;
    use crate::status::Status;
    use std::sync::Mutex;
    use zerofarm_common::layout::Layout;

    /// A message-only window with the real tray window procedure and a real tray state.
    fn test_tray(layout: &Layout) -> HWND {
        unsafe {
            let class = wide("ZeroFarmTrayTest");
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
            let title = wide("Zero Farm test");
            let hwnd = CreateWindowExW(0, class.as_ptr(), title.as_ptr(), 0, 0, 0, 0, 0, HWND_MESSAGE, std::ptr::null_mut(), hinstance(), std::ptr::null());
            assert!(!hwnd.is_null(), "test window");
            let tray = Tray {
                hwnd,
                machine: Arc::new(Mutex::new(Machine::default())),
                status: Arc::new(Mutex::new(Status::default())),
                control: Arc::new(Control::default()),
                layout: layout.clone(),
                version: "test".into(),
                logical: 8,
                physical: 4,
                engine: None,
                exit_code: None,
                icon: std::ptr::null_mut(),
                icon_phase: None,
                taskbar_created: 0,
                quitting: false,
                send_diag: Arc::new(String::new),
                diag_running: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            };
            TRAY.with(|t| *t.borrow_mut() = Some(tray));
            hwnd
        }
    }

    fn drop_tray(hwnd: HWND) {
        if let Some(t) = TRAY.with(|t| t.borrow_mut().take()) {
            if !t.icon.is_null() {
                unsafe { DestroyIcon(t.icon) };
            }
        }
        unsafe { DestroyWindow(hwnd) };
    }

    #[test]
    fn the_invite_dialog_names_the_third_party_terms_and_links_the_readme_section() {
        // The owner's wording (2026-11-12), one line; the link is the README's "Third-party components".
        let text: String = {
            let mut out = String::new();
            let mut rest = THIRD_PARTY_LINE;
            while let Some(i) = rest.find('<') {
                out.push_str(&rest[..i]);
                rest = &rest[rest[i..].find('>').map(|j| i + j + 1).unwrap()..];
            }
            out + rest
        };
        assert_eq!(text, "By installing, you accept the license terms of the included third-party components.");
        assert!(THIRD_PARTY_LINE.contains(&format!("href=\"{THIRD_PARTY_URL}\"")));
        let readme = include_str!("../../../README.md");
        assert!(readme.lines().any(|l| l.trim() == "## Third-party components"), "README.md has the section the link names");
    }

    #[test]
    fn a_message_that_arrives_while_the_tray_is_busy_is_skipped_not_fatal() {
        let d = tempfile::tempdir().unwrap();
        let hwnd = test_tray(&Layout::new(d.path()));
        let before = crate::logging::caught_panics();
        // What aborted 0.2.1: SetForegroundWindow, TrackPopupMenu and MessageBox dispatch messages,
        // so the window procedure runs again while the tray state is borrowed.
        let inside = with_tray(|_| unsafe {
            SendMessageW(hwnd, WM_TIMER, TIMER_ID, 0);
            SendMessageW(hwnd, WM_TRAY, 0, WM_LBUTTONUP as LPARAM); // would open the menu
            "inside"
        });
        assert_eq!(inside, Some("inside"));
        assert_eq!(crate::logging::caught_panics(), before, "no panic, not even a caught one");
        assert_eq!(with_tray(|t| t.icon_phase), Some(None), "the nested tick was skipped");
        // Not busy: a tick runs (it refreshes the icon).
        unsafe { SendMessageW(hwnd, WM_TIMER, TIMER_ID, 0) };
        assert_eq!(with_tray(|t| t.icon_phase), Some(Some(Phase::Starting)));
        drop_tray(hwnd);
    }

    #[test]
    fn a_panic_in_the_window_procedure_is_reported_and_the_tray_keeps_working() {
        let _lock = crate::logging::CRASH_CTX_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let d = tempfile::tempdir().unwrap();
        let layout = Layout::new(d.path());
        crate::logging::set_crash_context(layout.clone(), "0.0.0-test".into(), None);
        let hwnd = test_tray(&layout);
        let before = crate::logging::caught_panics();
        let r = unsafe { SendMessageW(hwnd, WM_TEST_PANIC, 0, 0) };
        assert_eq!(r, 0);
        assert_eq!(crate::logging::caught_panics(), before + 1);
        let reports: Vec<zerofarm_common::protocol::CrashReq> = std::fs::read_dir(layout.crashes())
            .unwrap()
            .flatten()
            .filter_map(|f| serde_json::from_slice(&std::fs::read(f.path()).unwrap()).ok())
            .collect();
        let c = reports.iter().find(|c| c.message.contains("test panic in the tray")).expect("an agent crash report");
        assert_eq!(c.kind, "agent");
        assert!(c.message.contains("tray window"), "{}", c.message);
        // The state was released by the unwinding: the next message works.
        unsafe { SendMessageW(hwnd, WM_TIMER, TIMER_ID, 0) };
        assert_eq!(with_tray(|t| t.icon_phase), Some(Some(Phase::Starting)));
        drop_tray(hwnd);
    }

    #[test]
    fn the_blocked_message_fits_the_tray() {
        let st = Status { phase: Phase::Blocked, ..Status::default() };
        // The tooltip holds 128 characters with "Zero Farm — " in front; the balloon 256.
        assert!(st.headline().chars().count() + 12 < 128, "{}", st.headline());
        assert!(crate::status::BLOCKED_MESSAGE.chars().count() < 256);
        assert_eq!(crate::status::BLOCKED_MESSAGE.split(". ").count(), 3);
    }
}

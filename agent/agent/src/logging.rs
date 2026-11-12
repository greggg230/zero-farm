//! Logs (`logs\agent.log`, rotating, part of the 20 MB cap together with worker.log and
//! launcher.log), the panic hook that turns an agent panic into a `/v1/crash` report on the next
//! start, the guard that keeps a panic in a window procedure from aborting the process, and log
//! uploads: on request (`upload_diag`) and when the friend presses "Send diagnostics" in the tray
//! (`x-farm-diag: user`, once an hour). Log lines are redacted as they are written (the rotating
//! log does it) and the uploaded body again (logs from before 0.2.5): no Windows user name or
//! user-profile path leaves the PC (security review M5).

use std::cell::{Cell, RefCell};
use std::sync::{Arc, Mutex};
use zerofarm_common::http::{CallError, Client};
use zerofarm_common::layout::Layout;
use zerofarm_common::protocol::CrashReq;
use zerofarm_common::rotlog::{utc_timestamp, RotatingFile};

/// agent.log: 4 files × 4 MB; worker.log 2 × 1.5 MB; launcher.log 2 × 0.5 MB → under 20 MB.
pub const AGENT_LOG_BYTES: u64 = 4 << 20;
pub const AGENT_LOG_KEEP: u32 = 3;

struct Logger {
    file: Arc<RotatingFile>,
    level: log::LevelFilter,
    echo: bool,
}

impl log::Log for Logger {
    fn enabled(&self, m: &log::Metadata) -> bool {
        m.level() <= self.level
    }
    fn log(&self, r: &log::Record) {
        if !self.enabled(r.metadata()) {
            return;
        }
        let line = format!("{} {:5} [{}] {}", utc_timestamp(zerofarm_common::now_unix()), r.level(), r.target(), r.args());
        self.file.write_line(&line);
        if self.echo {
            eprintln!("{line}");
        }
    }
    fn flush(&self) {}
}

/// Installs the file logger (once per process; later calls are ignored).
pub fn init(layout: &Layout, echo: bool) -> Arc<RotatingFile> {
    let file = Arc::new(RotatingFile::new(layout.logs().join("agent.log"), AGENT_LOG_BYTES, AGENT_LOG_KEEP));
    let level = if std::env::var("ZEROFARM_DEBUG").is_ok() { log::LevelFilter::Debug } else { log::LevelFilter::Info };
    if log::set_boxed_logger(Box::new(Logger { file: file.clone(), level, echo })).is_ok() {
        log::set_max_level(level);
    }
    file
}

/// Where a caught UI panic is reported: the install layout, agent version and build id.
struct CrashCtx {
    layout: Layout,
    version: String,
    build_id: Option<String>,
}

static CRASH_CTX: Mutex<Option<CrashCtx>> = Mutex::new(None);

/// Tests that set the (process-wide) crash context hold this, so they don't swap it under each other.
#[cfg(test)]
pub(crate) static CRASH_CTX_TEST_LOCK: Mutex<()> = Mutex::new(());

thread_local! {
    /// Depth of [`guard_ui`] calls on this thread: a panic inside one is caught, not fatal.
    static GUARDED: Cell<u32> = const { Cell::new(0) };
    /// What the panic hook saw of the last panic inside a guard (message at location, backtrace).
    static CAUGHT: RefCell<Option<(String, String)>> = const { RefCell::new(None) };
    /// Panics [`guard_ui`] caught on this thread (tests).
    static CAUGHT_COUNT: Cell<u64> = const { Cell::new(0) };
}

/// Where [`guard_ui`] writes its crash reports (set by [`install_panic_hook`]; tests set it too).
pub fn set_crash_context(layout: Layout, version: String, build_id: Option<String>) {
    *CRASH_CTX.lock().unwrap_or_else(|e| e.into_inner()) = Some(CrashCtx { layout, version, build_id });
}

fn payload_text(p: &(dyn std::any::Any + Send)) -> String {
    match p.downcast_ref::<&str>() {
        Some(s) => s.to_string(),
        None => p.downcast_ref::<String>().cloned().unwrap_or_else(|| "panic".into()),
    }
}

/// Runs a window procedure or another callback Windows calls into (`extern "system"`), where a
/// panic can't unwind and would abort the process (0xC0000409; tray clicks on 0.2.1 did that). A
/// panic is caught instead: logged, and saved under `state\crashes\` as a `/v1/crash` report of
/// kind `agent`, which the engine sends within a minute. Returns None after a panic.
pub fn guard_ui<R>(what: &str, f: impl FnOnce() -> R) -> Option<R> {
    GUARDED.with(|g| g.set(g.get() + 1));
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
    GUARDED.with(|g| g.set(g.get() - 1));
    match r {
        Ok(v) => Some(v),
        Err(payload) => {
            CAUGHT_COUNT.with(|c| c.set(c.get() + 1));
            let (msg, bt) = CAUGHT.with(|c| c.borrow_mut().take()).unwrap_or_else(|| (payload_text(&*payload), String::new()));
            log::error!("caught a panic in the {what}: {msg}");
            if let Some(ctx) = CRASH_CTX.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
                let c = CrashReq {
                    build_id: ctx.build_id.clone(),
                    kind: "agent".into(),
                    message: format!("agent {}: panic in the {what} (caught, Zero Farm kept running): {msg}", ctx.version),
                    backtrace: Some(bt).filter(|b| !b.is_empty()),
                    lease_id: None,
                    exit_code: None,
                    agent: Some(ctx.version.clone()),
                    os_error: None,
                };
                crate::engine::save_crash_file(&ctx.layout, &c);
            }
            None
        }
    }
}

/// Panics [`guard_ui`] caught on this thread.
pub fn caught_panics() -> u64 {
    CAUGHT_COUNT.with(|c| c.get())
}

/// On panic: write a crash report to `state\crashes\` (sent at the next start) and log it. A
/// panic inside [`guard_ui`] is left to the guard (one `agent` report, the process keeps running).
pub fn install_panic_hook(layout: Layout, version: String, build_id: Option<String>) {
    set_crash_context(layout.clone(), version.clone(), build_id.clone());
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let msg = payload_text(info.payload());
        let loc = info.location().map(|l| format!("{}:{}", l.file(), l.line())).unwrap_or_default();
        let bt = std::backtrace::Backtrace::force_capture().to_string();
        if GUARDED.with(|g| g.get()) > 0 {
            CAUGHT.with(|c| *c.borrow_mut() = Some((format!("{msg} at {loc}"), bt.chars().take(16_000).collect())));
            return;
        }
        log::error!("agent panic at {loc}: {msg}");
        let c = CrashReq {
            build_id: build_id.clone(),
            kind: "panic".into(),
            message: format!("agent {version} panicked at {loc}: {msg}"),
            backtrace: Some(bt.chars().take(16_000).collect()),
            lease_id: None,
            exit_code: None,
            agent: Some(version.clone()),
            os_error: None,
        };
        crate::engine::save_crash_file(&layout, &c);
        default(info);
    }));
}

/// The `logs` diagnostics body (PROTOCOL 3, `text/plain`): `header` (if any), then the agent,
/// worker and launcher logs, each tail-capped at 4 MB (under 13 MB in all), redacted.
pub fn logs_body(layout: &Layout, header: &str) -> Vec<u8> {
    let mut body = String::new();
    if !header.is_empty() {
        body.push_str(&format!("===== about =====\n{header}\n"));
    }
    for name in ["agent.log", "worker.log", "launcher.log"] {
        if let Ok(b) = std::fs::read(layout.logs().join(name)) {
            let start = b.len().saturating_sub(4 << 20);
            body.push_str(&format!("===== {name} =====\n"));
            body.push_str(&String::from_utf8_lossy(&b[start..]));
        }
    }
    zerofarm_common::redact::redact(&body).into_owned().into_bytes()
}

/// `upload_diag` from a beat (PROTOCOL 3): only `logs` is supported — the current agent and
/// worker logs, tail-truncated to 4 MB, as `POST /v1/diag/logs`. Runs in the background.
pub fn upload_diag(client: &Client, layout: &Layout, what: &[String]) {
    if !what.iter().any(|w| w == "logs") {
        return;
    }
    let (client, layout) = (client.clone(), layout.clone());
    std::thread::spawn(move || {
        let body = logs_body(&layout, "");
        if let Err(e) = client.post_bytes_json::<serde_json::Value>("/v1/diag/logs", &body, "text/plain") {
            log::warn!("diag upload failed: {e}");
        }
    });
}

/// The friend's "Send diagnostics": `POST /v1/diag/logs` with `x-farm-diag: user`, no request
/// needed (the coordinator takes one an hour per machine). Returns the stored key.
pub fn send_user_diag(client: &Client, layout: &Layout, header: &str) -> Result<String, CallError> {
    let body = logs_body(layout, header);
    let v: serde_json::Value = client.post_bytes_json_with(
        "/v1/diag/logs",
        &body,
        "text/plain",
        &[(zerofarm_common::protocol::headers::DIAG, "user")],
    )?;
    log::info!("diagnostics sent by the user ({} bytes)", body.len());
    Ok(v.get("key").and_then(|k| k.as_str()).unwrap_or("").to_string())
}

/// What the tray says after "Send diagnostics".
pub fn user_diag_message(r: &Result<String, CallError>) -> String {
    match r {
        Ok(_) => "Diagnostics sent. Thank you!".into(),
        Err(CallError::Http { status: 429, retry_after, .. }) => {
            let min = retry_after.map(|s| s.div_ceil(60).max(1)).unwrap_or(60);
            format!("Diagnostics were already sent in the last hour. You can send them again in {min} min.")
        }
        Err(CallError::Http { status: 413, .. }) => "The logs are too big to send.".into(),
        Err(e) if e.is_revoked() => "This PC is no longer part of the farm, so diagnostics can't be sent.".into(),
        Err(e) if e.is_unauthorized() => crate::status::SIGN_IN_MESSAGE.into(),
        Err(e) => format!("Couldn't send diagnostics: {e}"),
    }
}

#[cfg(test)]
mod guard_tests {
    use super::*;

    #[test]
    fn a_guarded_panic_is_caught_and_saved_as_an_agent_report() {
        let _lock = CRASH_CTX_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let d = tempfile::tempdir().unwrap();
        let layout = Layout::new(d.path());
        set_crash_context(layout.clone(), "9.9.9".into(), None);
        let before = caught_panics();
        assert_eq!(guard_ui("test window", || 7), Some(7));
        assert_eq!(guard_ui("test window", || -> i32 { panic!("boom in a window procedure") }), None);
        assert_eq!(caught_panics(), before + 1);
        let files: Vec<_> = std::fs::read_dir(layout.crashes()).unwrap().flatten().collect();
        let c: CrashReq = files
            .iter()
            .filter_map(|f| serde_json::from_slice::<CrashReq>(&std::fs::read(f.path()).unwrap()).ok())
            .find(|c| c.message.contains("boom in a window procedure"))
            .expect("crash report saved");
        assert_eq!(c.kind, "agent");
        assert!(c.message.contains("test window") && c.message.contains("caught"), "{}", c.message);
    }
}

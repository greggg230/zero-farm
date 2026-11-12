//! Logs (`logs\agent.log`, rotating, part of the 20 MB cap together with worker.log and
//! launcher.log), the panic hook that turns an agent panic into a `/v1/crash` report on the next
//! start, and log uploads: on request (`upload_diag`) and when the friend presses "Send
//! diagnostics" in the tray (`x-farm-diag: user`, once an hour).

use std::sync::Arc;
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

/// On panic: write a crash report to `state\crashes\` (sent at the next start) and log it.
pub fn install_panic_hook(layout: Layout, version: String, build_id: Option<String>) {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let msg = match info.payload().downcast_ref::<&str>() {
            Some(s) => s.to_string(),
            None => info.payload().downcast_ref::<String>().cloned().unwrap_or_else(|| "panic".into()),
        };
        let loc = info.location().map(|l| format!("{}:{}", l.file(), l.line())).unwrap_or_default();
        let bt = std::backtrace::Backtrace::force_capture().to_string();
        log::error!("agent panic at {loc}: {msg}");
        let c = CrashReq {
            build_id: build_id.clone(),
            kind: "panic".into(),
            message: format!("agent {version} panicked at {loc}: {msg}"),
            backtrace: Some(bt.chars().take(16_000).collect()),
            lease_id: None,
            exit_code: None,
            agent: Some(version.clone()),
        };
        crate::engine::save_crash_file(&layout, &c);
        default(info);
    }));
}

/// The `logs` diagnostics body (PROTOCOL 3, `text/plain`): `header` (if any), then the agent,
/// worker and launcher logs, each tail-capped at 4 MB (under 13 MB in all).
pub fn logs_body(layout: &Layout, header: &str) -> Vec<u8> {
    let mut body = Vec::new();
    if !header.is_empty() {
        body.extend_from_slice(format!("===== about =====\n{header}\n").as_bytes());
    }
    for name in ["agent.log", "worker.log", "launcher.log"] {
        if let Ok(b) = std::fs::read(layout.logs().join(name)) {
            let start = b.len().saturating_sub(4 << 20);
            body.extend_from_slice(format!("===== {name} =====\n").as_bytes());
            body.extend_from_slice(&b[start..]);
        }
    }
    body
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
        Err(e) if e.is_auth() => "This PC is no longer part of the farm, so diagnostics can't be sent.".into(),
        Err(e) => format!("Couldn't send diagnostics: {e}"),
    }
}

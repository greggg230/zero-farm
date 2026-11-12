//! The coordinator HTTP client (ureq + rustls/ring, no OpenSSL) with PROTOCOL 1's rules:
//! JSON bodies, bearer auth, `x-farm-agent` / `x-farm-aw` headers, error bodies
//! `{"error", "message"}`, and retries of 5xx / network errors / 429 with exponential backoff
//! (1 s → 5 min, jitter) while 4xx are never retried.

use crate::protocol::{headers, ApiError};
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// Largest JSON answer we accept.
const MAX_JSON_BYTES: u64 = 8 << 20;

#[derive(Debug, Clone)]
pub enum CallError {
    /// Could not talk to the server (DNS, connect, TLS, timeout, reset).
    Network(String),
    /// The server answered with a 4xx/5xx.
    Http { status: u16, code: String, message: String, retry_after: Option<u64>, body: Option<Value> },
    /// The answer could not be parsed.
    Decode(String),
    /// Downloaded bytes did not match the expected size or sha256.
    Integrity(String),
    /// The caller asked us to stop (quit, cancel).
    Stopped,
}

impl CallError {
    pub fn status(&self) -> Option<u16> {
        match self {
            CallError::Http { status, .. } => Some(*status),
            _ => None,
        }
    }
    pub fn code(&self) -> Option<&str> {
        match self {
            CallError::Http { code, .. } => Some(code),
            _ => None,
        }
    }
    /// PROTOCOL 1: retry 5xx and network errors; never retry 4xx except 429.
    pub fn is_retryable(&self) -> bool {
        match self {
            CallError::Network(_) | CallError::Integrity(_) => true,
            CallError::Http { status, .. } => *status >= 500 || *status == 429,
            CallError::Decode(_) | CallError::Stopped => false,
        }
    }
    /// The token is no longer accepted (revoked machine or unknown token).
    pub fn is_auth(&self) -> bool {
        matches!(self, CallError::Http { status: 401 | 403, .. })
    }
}

impl fmt::Display for CallError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CallError::Network(m) => write!(f, "network: {m}"),
            CallError::Http { status, code, message, .. } => write!(f, "HTTP {status} {code}: {message}"),
            CallError::Decode(m) => write!(f, "bad answer: {m}"),
            CallError::Integrity(m) => write!(f, "integrity: {m}"),
            CallError::Stopped => write!(f, "stopped"),
        }
    }
}
impl std::error::Error for CallError {}

/// Refuses anything but `https://`, except plain `http://` to the loopback interface (tests and
/// local development).
pub fn check_base_url(base: &str) -> Result<(), String> {
    let (scheme, rest) = base.split_once("://").ok_or_else(|| format!("{base:?} is not a URL"))?;
    let host_port = rest.split('/').next().unwrap_or("");
    let host = if let Some(h) = host_port.strip_prefix('[') {
        h.split(']').next().unwrap_or("")
    } else {
        host_port.split(':').next().unwrap_or("")
    };
    if host.is_empty() {
        return Err(format!("{base:?} has no host"));
    }
    match scheme {
        "https" => Ok(()),
        "http" if matches!(host, "127.0.0.1" | "localhost" | "::1") => Ok(()),
        _ => Err(format!("{base:?}: only https (or http to localhost) is allowed")),
    }
}

/// R2 keys handed out by the coordinator: `[A-Za-z0-9._/-]`, no `..`, no leading `/`.
pub fn check_key(key: &str) -> Result<(), CallError> {
    let ok = !key.is_empty()
        && key.len() <= 512
        && !key.starts_with('/')
        && !key.contains("..")
        && !key.contains("//")
        && key.bytes().all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'/' | b'-'));
    if ok {
        Ok(())
    } else {
        Err(CallError::Decode(format!("unsafe artifact key {key:?}")))
    }
}

type Hook = Arc<dyn Fn() + Send + Sync>;

#[derive(Clone)]
pub struct Client {
    agent: ureq::Agent,
    base: String,
    token: Option<String>,
    version: String,
    on_success: Option<Hook>,
    aw_header: bool,
}

/// What a download must turn out to be.
#[derive(Debug, Clone)]
pub struct Want {
    pub size: Option<u64>,
    pub sha256: Option<String>,
    /// Hard cap on bytes accepted.
    pub max: u64,
}

impl Client {
    /// `version` goes into `x-farm-agent` (and the user agent).
    pub fn new(base: &str, token: Option<String>, version: &str) -> Result<Client, String> {
        check_base_url(base)?;
        let config = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_connect(Some(Duration::from_secs(15)))
            .timeout_global(Some(Duration::from_secs(90)))
            .max_redirects(0)
            .user_agent(format!("zero-farm/{version}"))
            .build();
        Ok(Client {
            agent: config.new_agent(),
            base: base.trim_end_matches('/').to_string(),
            token,
            version: version.to_string(),
            on_success: None,
            aw_header: true,
        })
    }

    /// For the desktop tool: no `x-farm-aw` header.
    pub fn without_agent_headers(mut self) -> Self {
        self.aw_header = false;
        self
    }

    pub fn with_token(mut self, token: Option<String>) -> Self {
        self.token = token;
        self
    }

    /// Called after every 2xx answer (the agent writes `healthy-<version>` on the first).
    pub fn on_success(mut self, hook: Hook) -> Self {
        self.on_success = Some(hook);
        self
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base, path)
    }

    fn decorate<B>(&self, mut rb: ureq::RequestBuilder<B>) -> ureq::RequestBuilder<B> {
        if self.aw_header {
            rb = rb.header(headers::AGENT, &self.version).header(headers::AW, crate::AW_VERSION.to_string());
        }
        if let Some(t) = &self.token {
            rb = rb.header("authorization", format!("Bearer {t}"));
        }
        rb
    }

    fn finish(&self, res: Result<ureq::http::Response<ureq::Body>, ureq::Error>) -> Result<Vec<u8>, CallError> {
        let mut resp = res.map_err(net_err)?;
        let status = resp.status().as_u16();
        let retry_after = resp
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.trim().parse::<u64>().ok());
        let body = resp.body_mut().with_config().limit(MAX_JSON_BYTES).read_to_vec().map_err(net_err)?;
        if (200..300).contains(&status) {
            if let Some(h) = &self.on_success {
                h();
            }
            return Ok(body);
        }
        Err(http_err(status, &body, retry_after))
    }

    pub fn get_json<T: DeserializeOwned>(&self, path: &str) -> Result<T, CallError> {
        let body = self.finish(self.decorate(self.agent.get(&self.url(path))).call())?;
        serde_json::from_slice(&body).map_err(|e| CallError::Decode(e.to_string()))
    }

    pub fn post_json<B: Serialize, T: DeserializeOwned>(&self, path: &str, body: &B) -> Result<T, CallError> {
        let bytes = serde_json::to_vec(body).map_err(|e| CallError::Decode(e.to_string()))?;
        self.post_bytes_json(path, &bytes, "application/json")
    }

    pub fn post_bytes_json<T: DeserializeOwned>(&self, path: &str, bytes: &[u8], ctype: &str) -> Result<T, CallError> {
        self.post_bytes_json_with(path, bytes, ctype, &[])
    }

    /// POST raw bytes with `extra` headers; JSON answer.
    pub fn post_bytes_json_with<T: DeserializeOwned>(&self, path: &str, bytes: &[u8], ctype: &str, extra: &[(&str, &str)]) -> Result<T, CallError> {
        let mut rb = self.decorate(self.agent.post(&self.url(path))).header("content-type", ctype);
        for (k, v) in extra {
            rb = rb.header(*k, *v);
        }
        let body = self.finish(rb.send(bytes))?;
        serde_json::from_slice(&body).map_err(|e| CallError::Decode(e.to_string()))
    }

    /// PUT raw bytes (results, admin artifact uploads). `extra` are additional headers.
    pub fn put_bytes(&self, path: &str, bytes: &[u8], extra: &[(&str, String)]) -> Result<Value, CallError> {
        let mut rb = self
            .decorate(self.agent.put(&self.url(path)))
            .header("content-type", "application/octet-stream")
            .config()
            .timeout_global(Some(Duration::from_secs(60 + bytes.len() as u64 / 16_384)))
            .build();
        for (k, v) in extra {
            rb = rb.header(*k, v);
        }
        let body = self.finish(rb.send(bytes))?;
        if body.is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_slice(&body).map_err(|e| CallError::Decode(e.to_string()))
    }

    /// Downloads `GET /v1/artifact/{key}` into `dest`, resuming a previous partial download
    /// (`dest` + `.part`) with a range request, refusing more than `want.max` bytes, and checking
    /// size and sha256 before the file appears under its final name.
    pub fn download(&self, key: &str, dest: &Path, want: &Want, stop: &dyn Fn() -> bool) -> Result<(), CallError> {
        check_key(key)?;
        let part = part_path(dest);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).map_err(|e| CallError::Network(format!("mkdir: {e}")))?;
        }
        let cap = want.size.map(|s| s.min(want.max)).unwrap_or(want.max);
        let mut have = fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
        if have > cap {
            let _ = fs::remove_file(&part);
            have = 0;
        }
        let expected = want.size.unwrap_or(cap);
        let body_timeout = Duration::from_secs(120 + expected / 32_768);
        let mut rb = self
            .decorate(self.agent.get(&self.url(&format!("/v1/artifact/{key}"))))
            .config()
            .timeout_global(None)
            .timeout_recv_response(Some(Duration::from_secs(60)))
            .timeout_recv_body(Some(body_timeout))
            .build();
        let complete_already = want.size.is_some() && have == want.size.unwrap();
        if have > 0 && !complete_already {
            rb = rb.header("range", format!("bytes={have}-"));
        }
        if !complete_already {
            let mut resp = rb.call().map_err(net_err)?;
            let status = resp.status().as_u16();
            let append = match status {
                200 => false,
                206 => {
                    let start = resp
                        .headers()
                        .get("content-range")
                        .and_then(|v| v.to_str().ok())
                        .and_then(|s| s.strip_prefix("bytes "))
                        .and_then(|s| s.split('-').next())
                        .and_then(|s| s.parse::<u64>().ok());
                    if start != Some(have) {
                        let _ = fs::remove_file(&part);
                        return Err(CallError::Integrity(format!("range answer starts at {start:?}, wanted {have}")));
                    }
                    true
                }
                416 => {
                    // We asked past the end: start over.
                    let _ = fs::remove_file(&part);
                    return Err(CallError::Integrity("range not satisfiable; restarting".into()));
                }
                _ => {
                    let body = resp.body_mut().with_config().limit(64 * 1024).read_to_vec().unwrap_or_default();
                    return Err(http_err(status, &body, None));
                }
            };
            if let Some(h) = &self.on_success {
                h();
            }
            let mut f = OpenOptions::new()
                .create(true)
                .write(true)
                .append(append)
                .truncate(!append)
                .open(&part)
                .map_err(|e| CallError::Network(format!("open {}: {e}", part.display())))?;
            let mut written = if append { have } else { 0 };
            let mut reader = resp.body_mut().with_config().limit(cap.saturating_add(1)).reader();
            let mut buf = vec![0u8; 128 * 1024];
            loop {
                if stop() {
                    return Err(CallError::Stopped);
                }
                let n = match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => n,
                    Err(e) => return Err(CallError::Network(format!("download: {e}"))),
                };
                written += n as u64;
                if written > cap {
                    drop(f);
                    let _ = fs::remove_file(&part);
                    return Err(CallError::Integrity(format!("{key}: more than {cap} bytes")));
                }
                f.write_all(&buf[..n]).map_err(|e| CallError::Network(format!("write: {e}")))?;
            }
            f.flush().map_err(|e| CallError::Network(format!("flush: {e}")))?;
        }
        let (sha, size) = crate::hash::sha256_file(&part).map_err(|e| CallError::Network(e.to_string()))?;
        if let Some(s) = want.size {
            if size != s {
                if size > s {
                    let _ = fs::remove_file(&part);
                }
                return Err(CallError::Integrity(format!("{key}: {size} bytes, expected {s}")));
            }
        }
        if let Some(want_sha) = &want.sha256 {
            if &sha != want_sha {
                let _ = fs::remove_file(&part);
                return Err(CallError::Integrity(format!("{key}: sha256 mismatch")));
            }
        }
        crate::fsutil::rename_retry(&part, dest).map_err(|e| CallError::Network(format!("rename: {e}")))?;
        Ok(())
    }
}

pub fn part_path(dest: &Path) -> PathBuf {
    let mut s = dest.as_os_str().to_owned();
    s.push(".part");
    PathBuf::from(s)
}

fn net_err(e: ureq::Error) -> CallError {
    CallError::Network(e.to_string())
}

fn http_err(status: u16, body: &[u8], retry_after: Option<u64>) -> CallError {
    let parsed: Option<Value> = serde_json::from_slice(body).ok();
    let api: ApiError = parsed.clone().and_then(|v| serde_json::from_value(v).ok()).unwrap_or_default();
    CallError::Http {
        status,
        code: if api.error.is_empty() { format!("http_{status}") } else { api.error },
        message: if api.message.is_empty() { String::from_utf8_lossy(&body[..body.len().min(200)]).into_owned() } else { api.message },
        retry_after,
        body: parsed,
    }
}

/// Exponential backoff with jitter: delays are drawn from [d/2, d] where d doubles from `min`
/// up to `max` (PROTOCOL 1: 1 s → 5 min).
#[derive(Debug, Clone)]
pub struct Backoff {
    cur: Duration,
    min: Duration,
    max: Duration,
    rng: u64,
}

impl Default for Backoff {
    fn default() -> Self {
        Backoff::new(Duration::from_secs(1), Duration::from_secs(300))
    }
}

impl Backoff {
    pub fn new(min: Duration, max: Duration) -> Self {
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(1)
            ^ (&min as *const _ as u64);
        Backoff { cur: min, min, max, rng: seed | 1 }
    }

    fn rand(&mut self) -> u64 {
        // xorshift64*
        let mut x = self.rng;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.rng = x;
        x.wrapping_mul(0x2545F4914F6CDD1D)
    }

    /// The next delay; doubles the base for the time after.
    pub fn next_delay(&mut self) -> Duration {
        let d = self.cur;
        self.cur = (self.cur * 2).min(self.max);
        let ms = d.as_millis() as u64;
        let half = ms / 2;
        Duration::from_millis(half + if half > 0 { self.rand() % (half + 1) } else { 0 })
    }

    /// The delay after an error, honouring `retry-after` when the server sent one.
    pub fn delay_for(&mut self, e: &CallError) -> Duration {
        let d = self.next_delay();
        match e {
            CallError::Http { retry_after: Some(s), .. } => Duration::from_secs(*s).max(d.min(Duration::from_secs(1))),
            _ => d,
        }
    }

    pub fn reset(&mut self) {
        self.cur = self.min;
    }

    pub fn current_base(&self) -> Duration {
        self.cur
    }
}

/// Sleeps up to `d`, waking every 100 ms to check `stop`. Returns false if stopped.
pub fn sleep_unless(d: Duration, stop: &dyn Fn() -> bool) -> bool {
    let end = std::time::Instant::now() + d;
    loop {
        if stop() {
            return false;
        }
        let now = std::time::Instant::now();
        if now >= end {
            return true;
        }
        std::thread::sleep((end - now).min(Duration::from_millis(100)));
    }
}

/// Runs `op` until it succeeds, fails with a non-retryable error, `stop()` turns true, or
/// `max_attempts` (if any) is reached.
pub fn with_retry<T>(
    backoff: &mut Backoff,
    max_attempts: Option<u32>,
    stop: &dyn Fn() -> bool,
    mut op: impl FnMut() -> Result<T, CallError>,
) -> Result<T, CallError> {
    let mut attempts = 0u32;
    loop {
        match op() {
            Ok(v) => {
                backoff.reset();
                return Ok(v);
            }
            Err(e) => {
                attempts += 1;
                if !e.is_retryable() || max_attempts.is_some_and(|m| attempts >= m) {
                    return Err(e);
                }
                let d = backoff.delay_for(&e);
                if !sleep_unless(d, stop) {
                    return Err(CallError::Stopped);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_urls() {
        assert!(check_base_url("https://farm.greggg230.com").is_ok());
        assert!(check_base_url("http://127.0.0.1:5000").is_ok());
        assert!(check_base_url("http://localhost:1/x").is_ok());
        assert!(check_base_url("http://[::1]:8/").is_ok());
        assert!(check_base_url("http://farm.greggg230.com").is_err());
        assert!(check_base_url("ftp://x").is_err());
        assert!(check_base_url("farm").is_err());
    }

    #[test]
    fn keys() {
        assert!(check_key("builds/0123/manifest.json").is_ok());
        assert!(check_key("ckpt/eoe2/81/model.onnx").is_ok());
        assert!(check_key("../etc").is_err());
        assert!(check_key("/abs").is_err());
        assert!(check_key("a b").is_err());
    }

    #[test]
    fn backoff_grows_with_jitter_and_caps() {
        let mut b = Backoff::default();
        let mut prev_base = Duration::ZERO;
        for _ in 0..20 {
            let base = b.current_base();
            assert!(base >= prev_base);
            let d = b.next_delay();
            assert!(d <= base && d >= base / 2, "{d:?} vs {base:?}");
            prev_base = base;
        }
        assert_eq!(b.current_base(), Duration::from_secs(300));
        b.reset();
        assert_eq!(b.current_base(), Duration::from_secs(1));
    }

    #[test]
    fn retryability() {
        let h = |s| CallError::Http { status: s, code: String::new(), message: String::new(), retry_after: None, body: None };
        assert!(h(500).is_retryable());
        assert!(h(503).is_retryable());
        assert!(h(429).is_retryable());
        assert!(!h(400).is_retryable());
        assert!(!h(409).is_retryable());
        assert!(!h(410).is_retryable());
        assert!(CallError::Network("x".into()).is_retryable());
        assert!(h(401).is_auth());
    }
}

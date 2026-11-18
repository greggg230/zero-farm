//! POSIX stand-ins for the Windows named objects (PROTOCOL 11.5): one launcher and one agent per
//! root, and "quit".
//!
//! * **Single instance**: an exclusive `flock` on `<root>/state/<what>.lock`, held for the life of
//!   the process (the kernel drops it when the process ends, however it ends). The holder writes
//!   its pid into the file; that pid is only ever trusted while the lock is held.
//! * **Quit**: `QuitEvent::set` sends SIGTERM to the processes holding this root's `launcher` and
//!   `agent` locks (never to itself) and marks the root as quitting in this process (in-process
//!   engines, tests). The launcher and the agent call [`install_quit_signals`] at start: SIGTERM,
//!   SIGINT and SIGHUP then set a flag every `QuitEvent` of the process reports (launchd sends
//!   SIGTERM at logout and on `launchctl bootout`). A launcher resets the in-process mark when it
//!   starts, as the Windows launcher resets its event.
//! * SIGUSR1 means "look at `state/` for a request" (the headless CLI, `headless.rs`): [`take_wake`].

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Absolute root path as a string, no trailing `/`.
pub fn normalize_root(root: &Path) -> String {
    let abs = std::path::absolute(root).unwrap_or_else(|_| root.to_path_buf());
    let mut s = abs.to_string_lossy().to_string();
    while s.ends_with('/') && s.len() > 1 {
        s.pop();
    }
    s
}

/// `<root>/state/<what>.lock`.
pub fn lock_path(what: &str, root: &Path) -> PathBuf {
    root.join("state").join(format!("{what}.lock"))
}

/// A held single-instance lock (released on drop or when the process ends).
pub struct Handle(#[allow(dead_code)] File);

fn open_lock(p: &Path, create: bool) -> Option<File> {
    if create {
        if let Some(d) = p.parent() {
            if !d.exists() {
                std::fs::create_dir_all(d).ok()?;
                crate::fsutil::set_mode(d, 0o700);
            }
        }
    }
    OpenOptions::new().read(true).write(create).create(create).truncate(false).mode(0o600).open(p).ok()
}

fn try_flock(f: &File, how: i32) -> bool {
    // SAFETY: a valid descriptor owned by `f`.
    unsafe { libc::flock(f.as_raw_fd(), how | libc::LOCK_NB) == 0 }
}

/// Takes the single-instance lock `what` for `root`. `None` if another process holds it. A probe
/// ([`instance_running`]) holds a shared lock for an instant, so a refusal is retried for 100 ms
/// before it counts.
pub fn single_instance(what: &str, root: &Path) -> Option<Handle> {
    let mut f = open_lock(&lock_path(what, root), true)?;
    let end = Instant::now() + Duration::from_millis(100);
    while !try_flock(&f, libc::LOCK_EX) {
        if Instant::now() >= end {
            return None;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let _ = f.set_len(0);
    let _ = f.seek(SeekFrom::Start(0));
    let _ = write!(f, "{}", std::process::id());
    let _ = f.flush();
    Some(Handle(f))
}

/// True if some process currently holds the single-instance lock `what` for `root`. Never
/// creates the lock file.
pub fn instance_running(what: &str, root: &Path) -> bool {
    let Some(f) = open_lock(&lock_path(what, root), false) else { return false };
    if try_flock(&f, libc::LOCK_SH) {
        // SAFETY: as above.
        unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_UN) };
        false
    } else {
        true
    }
}

/// The pid of the process holding the lock `what` for `root`, while it holds it.
pub fn holder_pid(what: &str, root: &Path) -> Option<i32> {
    if !instance_running(what, root) {
        return None;
    }
    let mut s = String::new();
    File::open(lock_path(what, root)).ok()?.read_to_string(&mut s).ok()?;
    s.trim().parse::<i32>().ok().filter(|p| *p > 1)
}

/// Sends `sig` to the holder of the lock `what` for `root` (never to this process). True if sent.
pub fn signal_holder(what: &str, root: &Path, sig: i32) -> bool {
    match holder_pid(what, root) {
        // SAFETY: kill(2) with a pid read from a lock file whose holder is alive (its lock is held).
        Some(pid) if pid as u32 != std::process::id() => unsafe { libc::kill(pid, sig) == 0 },
        _ => false,
    }
}

static SIGNALLED: AtomicBool = AtomicBool::new(false);
static WOKEN: AtomicBool = AtomicBool::new(false);
static QUITTING_ROOTS: Mutex<Vec<String>> = Mutex::new(Vec::new());

extern "C" fn on_quit_signal(_sig: libc::c_int) {
    SIGNALLED.store(true, Ordering::SeqCst);
}

extern "C" fn on_wake_signal(_sig: libc::c_int) {
    WOKEN.store(true, Ordering::SeqCst);
}

fn handle(sig: i32, f: extern "C" fn(libc::c_int)) {
    // SAFETY: the handlers only store to atomics (async-signal-safe); SA_RESTART keeps blocking
    // reads of other threads going.
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = f as usize;
        sa.sa_flags = libc::SA_RESTART;
        libc::sigemptyset(&mut sa.sa_mask);
        libc::sigaction(sig, &sa, std::ptr::null_mut());
    }
}

/// The launcher and the agent (never a library user such as a test): SIGTERM, SIGINT and SIGHUP
/// ask this process to quit ([`QuitEvent::is_set`]); SIGUSR1 wakes it ([`take_wake`]).
pub fn install_quit_signals() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        for s in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
            handle(s, on_quit_signal);
        }
        handle(libc::SIGUSR1, on_wake_signal);
    });
}

/// A quit signal reached this process.
pub fn quit_signalled() -> bool {
    SIGNALLED.load(Ordering::SeqCst)
}

/// A SIGUSR1 arrived since the last call.
pub fn take_wake() -> bool {
    WOKEN.swap(false, Ordering::SeqCst)
}

/// "Quit" for `root` (see the module docs).
pub struct QuitEvent {
    root: PathBuf,
    key: String,
}

impl QuitEvent {
    pub fn open(root: &Path) -> Option<QuitEvent> {
        Some(QuitEvent { root: root.to_path_buf(), key: normalize_root(root) })
    }
    pub fn is_set(&self) -> bool {
        quit_signalled() || QUITTING_ROOTS.lock().map(|r| r.contains(&self.key)).unwrap_or(false)
    }
    /// Marks the root as quitting here and asks its launcher and agent (other processes) to quit.
    pub fn set(&self) {
        if let Ok(mut r) = QUITTING_ROOTS.lock() {
            if !r.contains(&self.key) {
                r.push(self.key.clone());
            }
        }
        for what in ["launcher", "agent"] {
            signal_holder(what, &self.root, libc::SIGTERM);
        }
    }
    pub fn reset(&self) {
        if let Ok(mut r) = QUITTING_ROOTS.lock() {
            r.retain(|k| k != &self.key);
        }
    }
    /// Waits up to `ms` for the event; true if set.
    pub fn wait(&self, ms: u32) -> bool {
        let end = Instant::now() + Duration::from_millis(ms as u64);
        loop {
            if self.is_set() {
                return true;
            }
            let now = Instant::now();
            if now >= end {
                return false;
            }
            std::thread::sleep((end - now).min(Duration::from_millis(20)));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_records_the_holder() {
        let d = tempfile::tempdir().unwrap();
        assert_eq!(holder_pid("agent", d.path()), None);
        let h = single_instance("agent", d.path()).unwrap();
        assert_eq!(holder_pid("agent", d.path()), Some(std::process::id() as i32));
        assert!(!signal_holder("agent", d.path(), libc::SIGTERM), "never signals itself");
        drop(h);
        assert_eq!(holder_pid("agent", d.path()), None, "a stale pid in the file is not trusted");
        assert_eq!(normalize_root(Path::new("/a/b/")), "/a/b");
    }
}

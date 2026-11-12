//! Named kernel objects shared by the launcher, the agent and the installer for one install
//! root: a single-instance mutex per program and a manual-reset "quit" event that asks the
//! running agent (and launcher) to exit cleanly (used by uninstall and re-install).

use std::path::Path;
use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE, WAIT_OBJECT_0};
use windows_sys::Win32::System::Threading::{CreateEventW, CreateMutexW, ResetEvent, SetEvent, WaitForSingleObject};

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Normalised root path (lowercase, backslashes, no `\\?\` prefix, no trailing separator).
pub fn normalize_root(root: &Path) -> String {
    let abs = std::path::absolute(root).unwrap_or_else(|_| root.to_path_buf());
    let mut s = abs.to_string_lossy().replace('/', "\\").to_lowercase();
    if let Some(rest) = s.strip_prefix("\\\\?\\") {
        s = rest.to_string();
    }
    while s.ends_with('\\') && s.len() > 3 {
        s.pop();
    }
    s
}

/// `Local\ZeroFarm-<what>-<16 hex of sha256(normalised root)>`.
pub fn object_name(what: &str, root: &Path) -> String {
    let h = crate::hash::sha256_hex(normalize_root(root).as_bytes());
    format!("Local\\ZeroFarm-{what}-{}", &h[..16])
}

/// An owned kernel handle, closed on drop.
pub struct Handle(pub HANDLE);
unsafe impl Send for Handle {}
unsafe impl Sync for Handle {}
impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { CloseHandle(self.0) };
        }
    }
}

/// Takes the single-instance mutex `what` for `root`. `None` if another process holds it.
pub fn single_instance(what: &str, root: &Path) -> Option<Handle> {
    let name = wide(&object_name(what, root));
    let h = unsafe { CreateMutexW(std::ptr::null(), 1, name.as_ptr()) };
    if h.is_null() {
        return None;
    }
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        unsafe { CloseHandle(h) };
        return None;
    }
    Some(Handle(h))
}

/// True if some process currently holds the single-instance mutex `what` for `root`. Only
/// opens the mutex (never creates it), so probing can't make a starting instance think another
/// copy is already running.
pub fn instance_running(what: &str, root: &Path) -> bool {
    use windows_sys::Win32::System::Threading::{OpenMutexW, SYNCHRONIZATION_SYNCHRONIZE};
    let name = wide(&object_name(what, root));
    let h = unsafe { OpenMutexW(SYNCHRONIZATION_SYNCHRONIZE, 0, name.as_ptr()) };
    if h.is_null() {
        false
    } else {
        unsafe { CloseHandle(h) };
        true
    }
}

/// The manual-reset quit event for `root` (created if needed).
pub struct QuitEvent(Handle);

impl QuitEvent {
    pub fn open(root: &Path) -> Option<QuitEvent> {
        let name = wide(&object_name("quit", root));
        let h = unsafe { CreateEventW(std::ptr::null(), 1, 0, name.as_ptr()) };
        if h.is_null() {
            None
        } else {
            Some(QuitEvent(Handle(h)))
        }
    }
    pub fn is_set(&self) -> bool {
        unsafe { WaitForSingleObject(self.0 .0, 0) == WAIT_OBJECT_0 }
    }
    pub fn set(&self) {
        unsafe { SetEvent(self.0 .0) };
    }
    pub fn reset(&self) {
        unsafe { ResetEvent(self.0 .0) };
    }
    /// Waits up to `ms` for the event; true if set.
    pub fn wait(&self, ms: u32) -> bool {
        unsafe { WaitForSingleObject(self.0 .0, ms) == WAIT_OBJECT_0 }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_stable_across_spellings() {
        let a = object_name("quit", Path::new("C:\\Users\\X\\AppData\\Local\\ZeroFarm"));
        let b = object_name("quit", Path::new("c:/users/x/appdata/local/zerofarm/"));
        assert_eq!(a, b);
        assert!(a.starts_with("Local\\ZeroFarm-quit-"));
    }

    #[test]
    fn mutex_and_event() {
        let d = tempfile::tempdir().unwrap();
        let m = single_instance("test", d.path()).expect("first");
        assert!(single_instance("test", d.path()).is_none());
        assert!(instance_running("test", d.path()));
        drop(m);
        assert!(!instance_running("test", d.path()));
        let e = QuitEvent::open(d.path()).unwrap();
        let e2 = QuitEvent::open(d.path()).unwrap();
        assert!(!e.is_set());
        e2.set();
        assert!(e.is_set());
        e.reset();
        assert!(!e2.is_set());
    }
}

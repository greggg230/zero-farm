//! Small filesystem helpers that tolerate Windows' transient sharing violations (antivirus
//! scanners, indexers) by retrying briefly.

use std::fs;
use std::io;
use std::path::Path;
use std::time::{Duration, Instant};

fn retry<T>(mut f: impl FnMut() -> io::Result<T>) -> io::Result<T> {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        match f() {
            Ok(v) => return Ok(v),
            Err(e) if e.kind() != io::ErrorKind::NotFound && Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(50));
                let _ = e;
            }
            Err(e) => return Err(e),
        }
    }
}

/// Writes `bytes` to a temp file next to `path`, then renames it over `path`.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_file_name(format!(
        ".{}.{}.tmp",
        path.file_name().and_then(|s| s.to_str()).unwrap_or("f"),
        std::process::id()
    ));
    fs::write(&tmp, bytes)?;
    retry(|| fs::rename(&tmp, path)).inspect_err(|_| {
        let _ = fs::remove_file(&tmp);
    })
}

/// Reads a small text file and trims it; `None` if missing or empty.
pub fn read_trimmed(path: &Path) -> Option<String> {
    let s = fs::read_to_string(path).ok()?;
    let t = s.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

pub fn remove_file_quiet(path: &Path) {
    let _ = retry(|| fs::remove_file(path));
}

/// Removes a directory tree, retrying for up to `timeout` while files are still locked.
pub fn remove_dir_all_retry(path: &Path, timeout: Duration) -> io::Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        match fs::remove_dir_all(path) {
            Ok(()) => return Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) => {
                if Instant::now() >= deadline {
                    return Err(e);
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }
}

pub fn rename_retry(from: &Path, to: &Path) -> io::Result<()> {
    retry(|| fs::rename(from, to))
}

/// Total size of all files under `path` (0 if missing).
pub fn dir_size(path: &Path) -> u64 {
    let mut total = 0;
    let mut stack = vec![path.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            match e.file_type() {
                Ok(t) if t.is_dir() => stack.push(e.path()),
                Ok(t) if t.is_file() => total += e.metadata().map(|m| m.len()).unwrap_or(0),
                _ => {}
            }
        }
    }
    total
}

/// Copies a directory tree (files and subdirectories only).
pub fn copy_dir(from: &Path, to: &Path) -> io::Result<()> {
    fs::create_dir_all(to)?;
    for e in fs::read_dir(from)? {
        let e = e?;
        let t = e.file_type()?;
        let dest = to.join(e.file_name());
        if t.is_dir() {
            copy_dir(&e.path(), &dest)?;
        } else if t.is_file() {
            retry(|| fs::copy(e.path(), &dest))?;
        }
    }
    Ok(())
}

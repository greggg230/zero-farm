//! A size-capped rotating log file: `<name>.log`, `<name>.log.1` … `<name>.log.<keep>`.
//! Total size on disk stays under `max_bytes * (keep + 1)`. Every line is redacted
//! (`redact.rs`): no user name or user-profile path reaches a log file.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

pub struct RotatingFile {
    path: PathBuf,
    max_bytes: u64,
    keep: u32,
    inner: Mutex<Option<(File, u64)>>,
}

impl RotatingFile {
    pub fn new(path: impl Into<PathBuf>, max_bytes: u64, keep: u32) -> Self {
        RotatingFile { path: path.into(), max_bytes, keep, inner: Mutex::new(None) }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn rotate(&self) {
        for i in (1..self.keep).rev() {
            let from = self.rotated(i);
            if from.exists() {
                let _ = fs::rename(&from, self.rotated(i + 1));
            }
        }
        if self.keep >= 1 {
            let _ = fs::rename(&self.path, self.rotated(1));
        } else {
            let _ = fs::remove_file(&self.path);
        }
    }

    fn rotated(&self, i: u32) -> PathBuf {
        let mut s = self.path.as_os_str().to_owned();
        s.push(format!(".{i}"));
        PathBuf::from(s)
    }

    /// Appends one line (a trailing newline is added), with the user name and profile path
    /// redacted (security review M5: logs can be uploaded). Errors are swallowed: logging must
    /// never take the program down.
    pub fn write_line(&self, line: &str) {
        let line = crate::redact::redact(line);
        let line: &str = &line;
        let mut g = match self.inner.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        if g.is_none() {
            if let Some(parent) = self.path.parent() {
                let _ = fs::create_dir_all(parent);
            }
            if let Ok(f) = OpenOptions::new().create(true).append(true).open(&self.path) {
                let len = f.metadata().map(|m| m.len()).unwrap_or(0);
                *g = Some((f, len));
            }
        }
        let need_rotate = matches!(&*g, Some((_, len)) if *len + line.len() as u64 + 1 > self.max_bytes && *len > 0);
        if need_rotate {
            *g = None;
            self.rotate();
            if let Ok(f) = OpenOptions::new().create(true).append(true).open(&self.path) {
                *g = Some((f, 0));
            }
        }
        if let Some((f, len)) = g.as_mut() {
            if f.write_all(line.as_bytes()).and_then(|_| f.write_all(b"\n")).is_ok() {
                *len += line.len() as u64 + 1;
            }
        }
    }
}

/// `YYYY-MM-DD HH:MM:SS` in UTC, without a date crate.
pub fn utc_timestamp(t: i64) -> String {
    let days = t.div_euclid(86400);
    let secs = t.rem_euclid(86400);
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}", secs / 3600, (secs / 60) % 60, secs % 60)
}

/// Howard Hinnant's days-from-civil inverse.
pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotates_and_caps() {
        let d = tempfile::tempdir().unwrap();
        let r = RotatingFile::new(d.path().join("a.log"), 100, 2);
        for i in 0..100 {
            r.write_line(&format!("line {i:04} xxxxxxxxxxxxxxxx"));
        }
        assert!(d.path().join("a.log").exists());
        assert!(d.path().join("a.log.1").exists());
        assert!(d.path().join("a.log.2").exists());
        assert!(!d.path().join("a.log.3").exists());
        let total: u64 = fs::read_dir(d.path()).unwrap().map(|e| e.unwrap().metadata().unwrap().len()).sum();
        assert!(total <= 300, "{total}");
    }

    #[test]
    fn timestamps() {
        assert_eq!(utc_timestamp(0), "1970-01-01 00:00:00");
        assert_eq!(utc_timestamp(1794300000), "2026-11-10 08:40:00");
    }
}

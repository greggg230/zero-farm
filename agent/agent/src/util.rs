//! Small shared helpers: wide strings, local time, disk space, id ranges.

use serde_json::{json, Value};
use std::path::Path;

pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

pub fn wide_path(p: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    p.as_os_str().encode_wide().chain(std::iter::once(0)).collect()
}

pub fn from_wide(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

/// Local wall-clock time: (YYYY-MM-DD, seconds since local midnight).
pub fn local_now() -> (String, u32) {
    use windows_sys::Win32::System::SystemInformation::GetLocalTime;
    let mut st = unsafe { std::mem::zeroed() };
    unsafe { GetLocalTime(&mut st) };
    let st: windows_sys::Win32::Foundation::SYSTEMTIME = st;
    (
        format!("{:04}-{:02}-{:02}", st.wYear, st.wMonth, st.wDay),
        st.wHour as u32 * 3600 + st.wMinute as u32 * 60 + st.wSecond as u32,
    )
}

/// Free bytes available to this user on the volume holding `path`.
pub fn disk_free_bytes(path: &Path) -> Option<u64> {
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    let mut p = path.to_path_buf();
    while !p.exists() {
        p = p.parent()?.to_path_buf();
    }
    let w = wide_path(&p);
    let mut avail = 0u64;
    let ok = unsafe { GetDiskFreeSpaceExW(w.as_ptr(), &mut avail, std::ptr::null_mut(), std::ptr::null_mut()) };
    (ok != 0).then_some(avail)
}

/// Half-open id ranges `[a, b)` as JSON pairs, the form used in `gids` and `unfinished`.
pub type Ranges = Vec<(u64, u64)>;

pub fn parse_ranges(v: &Value) -> Ranges {
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|r| {
                    let r = r.as_array()?;
                    Some((r.first()?.as_u64()?, r.get(1)?.as_u64()?))
                })
                .filter(|(a, b)| a < b)
                .collect()
        })
        .unwrap_or_default()
}

pub fn ranges_json(r: &Ranges) -> Value {
    Value::Array(r.iter().map(|(a, b)| json!([a, b])).collect())
}

/// Sorts and merges overlapping/adjacent ranges.
pub fn normalize(mut r: Ranges) -> Ranges {
    r.retain(|(a, b)| a < b);
    r.sort();
    let mut out: Ranges = Vec::new();
    for (a, b) in r {
        match out.last_mut() {
            Some(last) if a <= last.1 => last.1 = last.1.max(b),
            _ => out.push((a, b)),
        }
    }
    out
}

/// `whole` minus every range in `minus`.
pub fn subtract(whole: (u64, u64), minus: &Ranges) -> Ranges {
    let mut out = Vec::new();
    let mut at = whole.0;
    for &(a, b) in &normalize(minus.clone()) {
        if b <= at || a >= whole.1 {
            continue;
        }
        if a > at {
            out.push((at, a.min(whole.1)));
        }
        at = at.max(b);
        if at >= whole.1 {
            break;
        }
    }
    if at < whole.1 {
        out.push((at, whole.1));
    }
    out
}

pub fn total(r: &Ranges) -> u64 {
    r.iter().map(|(a, b)| b - a).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn range_math() {
        assert_eq!(normalize(vec![(5, 7), (1, 3), (2, 4), (7, 8)]), vec![(1, 4), (5, 8)]);
        assert_eq!(subtract((100, 110), &vec![(100, 102), (104, 106)]), vec![(102, 104), (106, 110)]);
        assert_eq!(subtract((100, 110), &vec![]), vec![(100, 110)]);
        assert_eq!(subtract((100, 110), &vec![(90, 120)]), Vec::<(u64, u64)>::new());
        assert_eq!(subtract((100, 110), &vec![(105, 200)]), vec![(100, 105)]);
        assert_eq!(parse_ranges(&json!([[1, 3], [5, 5], "x"])), vec![(1, 3)]);
        assert_eq!(total(&vec![(1, 3), (10, 20)]), 12);
    }
}

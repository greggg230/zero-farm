//! An executable's own version, read from its bytes without running it (PROTOCOL 6: an agent compares its package's
//! launcher with the installed one before replacing it; equal or older is never installed).
//!
//! * Windows: the VERSIONINFO resource every exe carries ([`crate::winres`]).
//! * macOS (no resources): a fixed 32-byte marker each binary embeds with [`mark`] in a `#[used]` static -
//!   `\0ZFVERSION:` and the crate version, padded with NULs. Read by [`exe_version`], patched by [`patch_version`]
//!   (tests make "older" launchers).

pub type Version = (u16, u16, u16, u16);

/// The marker's length.
pub const MARK_LEN: usize = 32;
/// The marker's prefix, reversed: the scanner's needle is put together at run time, so the bytes of a binary that
/// contains the scanner never hold a second copy of the prefix that could be taken for a marker.
const PREFIX_REV: &[u8; 11] = b":NOISREVFZ\0";

/// The marker for `version` (at most 20 bytes); for a `#[used] static` (compile time).
pub const fn mark(version: &str) -> [u8; MARK_LEN] {
    let v = version.as_bytes();
    assert!(v.len() <= MARK_LEN - PREFIX_REV.len() - 1, "version too long for the marker");
    let mut out = [0u8; MARK_LEN];
    let mut i = 0;
    while i < PREFIX_REV.len() {
        out[i] = PREFIX_REV[PREFIX_REV.len() - 1 - i];
        i += 1;
    }
    let mut j = 0;
    while j < v.len() {
        out[PREFIX_REV.len() + j] = v[j];
        j += 1;
    }
    out
}

fn prefix() -> Vec<u8> {
    PREFIX_REV.iter().rev().copied().collect()
}

/// `a.b.c` (a pre-release or build suffix ignored) -> (a, b, c, 0).
fn parse(v: &str) -> Option<Version> {
    let core = v.split(['-', '+']).next()?;
    let p: Vec<u16> = core.split('.').map(|x| x.parse().ok()).collect::<Option<_>>()?;
    (p.len() == 3).then(|| (p[0], p[1], p[2], 0))
}

/// The first well-formed marker in `bytes`: its offset and version.
fn find_mark(bytes: &[u8]) -> Option<(usize, Version)> {
    let pre = prefix();
    let mut from = 0;
    while let Some(k) = bytes[from..].windows(pre.len()).position(|w| w == pre.as_slice()).map(|k| k + from) {
        from = k + 1;
        let Some(m) = bytes.get(k..k + MARK_LEN) else { continue };
        let field = &m[pre.len()..];
        let end = field.iter().position(|&b| b == 0).unwrap_or(field.len());
        if end == 0 || end == field.len() || field[end..].iter().any(|&b| b != 0) {
            continue;
        }
        if let Some(v) = std::str::from_utf8(&field[..end]).ok().and_then(parse) {
            return Some((k, v));
        }
    }
    None
}

/// The version marker's version, whatever platform reads it (`farm-release` on Windows reads a Mac binary's).
pub fn exe_version_marker(bytes: &[u8]) -> Option<Version> {
    find_mark(bytes).map(|(_, v)| v)
}

/// The executable's version: Windows VERSIONINFO, elsewhere the marker. None when it has neither.
pub fn exe_version(bytes: &[u8]) -> Option<Version> {
    if cfg!(windows) {
        crate::winres::fixed_file_version(bytes)
    } else {
        find_mark(bytes).map(|(_, v)| v)
    }
}

/// Rewrites the executable's version (tests: an "older" launcher). False when it has none.
pub fn patch_version(bytes: &mut [u8], v: (u16, u16, u16)) -> bool {
    if cfg!(windows) {
        return crate::winres::patch_fixed_file_version(bytes, v);
    }
    let Some((k, _)) = find_mark(bytes) else { return false };
    let m = mark(&format!("{}.{}.{}", v.0, v.1, v.2));
    bytes[k..k + MARK_LEN].copy_from_slice(&m);
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marker_round_trip() {
        static M: [u8; MARK_LEN] = mark("0.3.0");
        let mut bin = b"junk ZFVERSION: not one\0\0".to_vec();
        bin.extend_from_slice(&prefix());
        bin.extend_from_slice(b"9.9"); // a truncated one: no NUL padding to the end
        bin.extend_from_slice(&M);
        bin.extend_from_slice(b"tail");
        assert_eq!(find_mark(&bin).map(|x| x.1), Some((0, 3, 0, 0)));
        assert!(patch_version(&mut bin, (0, 2, 11)) || cfg!(windows));
        if !cfg!(windows) {
            assert_eq!(exe_version(&bin), Some((0, 2, 11, 0)));
        }
        assert_eq!(find_mark(b"no marker here"), None);
        assert_eq!(parse("1.2.3-rc.1"), Some((1, 2, 3, 0)));
        assert_eq!(parse("1.2"), None);
    }
}

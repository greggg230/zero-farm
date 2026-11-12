//! zero-farm shared pieces used by the launcher, the agent and the desktop release tool.
//!
//! The wire contract lives in `PROTOCOL.md` at the repository root; the types here follow it.

pub mod fsutil;
pub mod hash;
pub mod icon;
pub mod keys;
pub mod layout;
pub mod manifest;
pub mod names;
pub mod payload;
pub mod protocol;
pub mod rotlog;
pub mod winres;

#[cfg(feature = "dpapi")]
pub mod dpapi;
#[cfg(feature = "http")]
pub mod http;
#[cfg(feature = "sign")]
pub mod sign;

/// Agent↔worker protocol version this code speaks (PROTOCOL 5).
pub const AW_VERSION: u32 = 1;
/// `manifest/1` (PROTOCOL 2).
pub const MANIFEST_VERSION: u32 = 1;
/// Exit code an agent uses to ask the launcher to switch to `state\pending` (PROTOCOL 6).
pub const EXIT_UPDATE: i32 = 75;
/// Exit code an agent uses after it put a newer `ZeroFarm.exe` in place: the launcher starts the
/// new launcher and hands over to it (PROTOCOL 6).
pub const EXIT_RESTART_LAUNCHER: i32 = 76;
/// Set by a launcher for the agent it starts: the launcher protocol level it speaks. 2 = it knows
/// [`EXIT_RESTART_LAUNCHER`]. Launchers before 0.2.0 set nothing.
pub const LAUNCHER_ENV: &str = "ZEROFARM_LAUNCHER";
pub const LAUNCHER_LEVEL: u32 = 2;

/// Seconds since the Unix epoch.
pub fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// True when `s` is a run/campaign slug: `[a-z0-9][a-z0-9-]{0,62}` (PROTOCOL 1).
pub fn is_slug(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && b.len() <= 63
        && (b[0].is_ascii_lowercase() || b[0].is_ascii_digit())
        && b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
}

/// True when `s` is safe to use as one path component for an id the coordinator chose
/// (lease ids, machine ids): `[A-Za-z0-9_-]{1,64}`.
pub fn is_safe_id(s: &str) -> bool {
    !s.is_empty() && s.len() <= 64 && s.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
}

/// True when `s` is a build id: 24 lowercase hex chars (PROTOCOL 2).
pub fn is_build_id(s: &str) -> bool {
    s.len() == 24 && s.bytes().all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}

/// True when `s` is a lowercase hex sha256.
pub fn is_sha256_hex(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs_and_ids() {
        assert!(is_slug("eoe2"));
        assert!(is_slug("fuzz-delver"));
        assert!(!is_slug("-x"));
        assert!(!is_slug("Eoe"));
        assert!(!is_slug("a/b"));
        assert!(!is_slug(""));
        assert!(is_safe_id("l_ABCDEFGHJKMNPQRSTVWXYZ0123"));
        assert!(!is_safe_id("..\\x"));
        assert!(is_build_id("0123456789abcdef01234567"));
        assert!(!is_build_id("0123456789ABCDEF01234567"));
    }
}

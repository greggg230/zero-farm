//! zstd copies of build files (PROTOCOL 9.3, rev 11).
//!
//! A manifest file entry may name a compressed copy (`zst`: its size and sha256) stored at
//! `builds/<build_id>/<path>.zst`. The agent downloads that, checks it, then [`unpack`]s it: one zstd frame,
//! a window of at most 2^27 bytes, the output capped at the file's declared size and checked against its
//! sha256 before it gets its name. `farm-release sign --zst` makes the copies with [`pack`].

use crate::hash::hex;
use crate::manifest::{Packed, ZST_WINDOW_LOG_MAX};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::Path;

/// Decompresses `src` into `dest` (via `dest.part`): at most `size` bytes, which must hash to `sha256`.
pub fn unpack(src: &Path, dest: &Path, size: u64, sha256: &str, stop: &dyn Fn() -> bool) -> Result<(), String> {
    let mut part = dest.as_os_str().to_owned();
    part.push(".part");
    let part = std::path::PathBuf::from(part);
    let r = (|| {
        let f = std::fs::File::open(src).map_err(|e| format!("{}: {e}", src.display()))?;
        let mut dec = zstd::stream::read::Decoder::new(f).map_err(|e| format!("zstd: {e}"))?;
        dec.window_log_max(ZST_WINDOW_LOG_MAX).map_err(|e| format!("zstd: {e}"))?;
        let mut dec = dec.single_frame().take(size + 1);
        let mut out = std::io::BufWriter::with_capacity(1 << 20, std::fs::File::create(&part).map_err(|e| format!("{}: {e}", part.display()))?);
        let mut h = Sha256::new();
        let mut buf = vec![0u8; 1 << 20];
        let mut n = 0u64;
        loop {
            if stop() {
                return Err("stopped".to_string());
            }
            let k = dec.read(&mut buf).map_err(|e| format!("zstd: {e}"))?;
            if k == 0 {
                break;
            }
            n += k as u64;
            if n > size {
                return Err(format!("{}: unpacks to more than its {size} bytes", src.display()));
            }
            h.update(&buf[..k]);
            out.write_all(&buf[..k]).map_err(|e| format!("{}: {e}", part.display()))?;
        }
        out.flush().map_err(|e| format!("{}: {e}", part.display()))?;
        drop(out);
        if n != size {
            return Err(format!("{}: unpacks to {n} bytes, the manifest says {size}", src.display()));
        }
        if hex(&h.finalize()) != sha256 {
            return Err(format!("{}: the unpacked file's sha256 is not the manifest's", src.display()));
        }
        let _ = std::fs::remove_file(dest);
        std::fs::rename(&part, dest).map_err(|e| format!("{}: {e}", dest.display()))
    })();
    if r.is_err() {
        let _ = std::fs::remove_file(&part);
    }
    r
}

/// Compresses `src` into `dest` (zstd `level`, long-distance matching, window 2^27, `threads` workers) -> the
/// compressed copy's size and sha256.
#[cfg(feature = "zst-pack")]
pub fn pack(src: &Path, dest: &Path, level: i32, threads: u32) -> Result<Packed, String> {
    if let Some(p) = dest.parent() {
        std::fs::create_dir_all(p).map_err(|e| format!("{}: {e}", p.display()))?;
    }
    let mut input = std::fs::File::open(src).map_err(|e| format!("{}: {e}", src.display()))?;
    let out = std::fs::File::create(dest).map_err(|e| format!("{}: {e}", dest.display()))?;
    let mut enc = zstd::stream::write::Encoder::new(std::io::BufWriter::with_capacity(1 << 20, out), level).map_err(|e| format!("zstd: {e}"))?;
    let z = |r: std::io::Result<()>| r.map_err(|e| format!("zstd: {e}"));
    z(enc.window_log(ZST_WINDOW_LOG_MAX))?;
    z(enc.long_distance_matching(true))?;
    z(enc.include_checksum(true))?;
    z(enc.include_contentsize(true))?;
    let len = input.metadata().map_err(|e| e.to_string())?.len();
    z(enc.set_pledged_src_size(Some(len)))?;
    if threads > 1 {
        z(enc.multithread(threads))?;
    }
    std::io::copy(&mut input, &mut enc).map_err(|e| format!("zstd: {e}"))?;
    let mut w = enc.finish().map_err(|e| format!("zstd: {e}"))?;
    w.flush().map_err(|e| e.to_string())?;
    drop(w);
    let (sha256, size) = crate::hash::sha256_file(dest).map_err(|e| format!("{}: {e}", dest.display()))?;
    Ok(Packed { sha256, size })
}

#[cfg(all(test, feature = "zst-pack"))]
mod tests {
    use super::*;

    #[test]
    fn pack_then_unpack_checks_size_and_hash() {
        let d = tempfile::tempdir().unwrap();
        let src = d.path().join("a.dll");
        let data: Vec<u8> = (0..3_000_000u32).map(|i| (i % 251) as u8 ^ (i / 7000) as u8).collect();
        std::fs::write(&src, &data).unwrap();
        let z = d.path().join("z").join("a.dll.zst");
        let p = pack(&src, &z, 3, 2).unwrap();
        assert!(p.size < data.len() as u64 / 2, "{p:?}");
        let (sha, n) = crate::hash::sha256_file(&src).unwrap();
        let out = d.path().join("out").join("a.dll");
        std::fs::create_dir_all(out.parent().unwrap()).unwrap();
        unpack(&z, &out, n, &sha, &|| false).unwrap();
        assert_eq!(std::fs::read(&out).unwrap(), data);
        // a wrong size or hash never gets the file's name
        let bad = d.path().join("out").join("b.dll");
        assert!(unpack(&z, &bad, n - 1, &sha, &|| false).unwrap_err().contains("more than"));
        assert!(unpack(&z, &bad, n + 1, &sha, &|| false).unwrap_err().contains("unpacks to"));
        assert!(unpack(&z, &bad, n, &"0".repeat(64), &|| false).unwrap_err().contains("sha256"));
        assert!(!bad.exists());
        assert!(std::fs::read_dir(d.path().join("out")).unwrap().flatten().all(|e| !e.file_name().to_string_lossy().ends_with("part")));
    }
}

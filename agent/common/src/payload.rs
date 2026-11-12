//! The payload ZeroFarmSetup.exe embeds: a flat agent package (manifest, signature, files).
//!
//! Format: `ZFSETUP1`, u32 file count, then per file: u16 name length, UTF-8 name, u64 size,
//! bytes (all little-endian). Names are plain file names (no directories).

use std::path::Path;

const MAGIC: &[u8; 8] = b"ZFSETUP1";

pub fn pack(files: &[(String, Vec<u8>)]) -> Vec<u8> {
    let mut out = MAGIC.to_vec();
    out.extend_from_slice(&(files.len() as u32).to_le_bytes());
    for (name, bytes) in files {
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
        out.extend_from_slice(bytes);
    }
    out
}

/// Packs every file of a flat directory.
pub fn pack_dir(dir: &Path) -> Result<Vec<u8>, String> {
    let mut files = Vec::new();
    for e in std::fs::read_dir(dir).map_err(|e| e.to_string())?.flatten() {
        if !e.file_type().map(|t| t.is_file()).unwrap_or(false) {
            return Err(format!("{}: payloads are flat (no subdirectories)", e.path().display()));
        }
        files.push((e.file_name().to_string_lossy().to_string(), std::fs::read(e.path()).map_err(|e| e.to_string())?));
    }
    files.sort();
    Ok(pack(&files))
}

pub fn unpack(data: &[u8]) -> Result<Vec<(String, &[u8])>, String> {
    let bad = || "corrupt setup payload".to_string();
    if data.len() < 12 || &data[..8] != MAGIC {
        return Err(bad());
    }
    let n = u32::from_le_bytes(data[8..12].try_into().unwrap()) as usize;
    let mut at = 12usize;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        let nl = u16::from_le_bytes(data.get(at..at + 2).ok_or_else(bad)?.try_into().unwrap()) as usize;
        at += 2;
        let name = std::str::from_utf8(data.get(at..at + nl).ok_or_else(bad)?).map_err(|_| bad())?.to_string();
        at += nl;
        let sz = u64::from_le_bytes(data.get(at..at + 8).ok_or_else(bad)?.try_into().unwrap()) as usize;
        at += 8;
        let bytes = data.get(at..at.checked_add(sz).ok_or_else(bad)?).ok_or_else(bad)?;
        at += sz;
        if name.is_empty() || name.contains(['/', '\\', ':']) || name == "." || name == ".." {
            return Err(format!("bad file name {name:?} in setup payload"));
        }
        out.push((name, bytes));
    }
    if at != data.len() {
        return Err(bad());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn roundtrip_and_rejects() {
        let files = vec![("a.exe".to_string(), vec![1u8, 2, 3]), ("manifest.json".to_string(), b"{}".to_vec())];
        let p = pack(&files);
        let u = unpack(&p).unwrap();
        assert_eq!(u.len(), 2);
        assert_eq!(u[0].0, "a.exe");
        assert_eq!(u[0].1, &[1, 2, 3]);
        assert!(unpack(&p[..p.len() - 1]).is_err());
        assert!(unpack(b"nope").is_err());
        let evil = pack(&[("..\\x".to_string(), vec![])]);
        assert!(unpack(&evil).is_err());
    }
}

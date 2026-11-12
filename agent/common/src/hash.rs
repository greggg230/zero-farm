//! sha256 helpers (lowercase hex, PROTOCOL 1).

use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::{self, Read, Write};
use std::path::Path;

pub fn sha256_hex(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

pub fn hex(bytes: &[u8]) -> String {
    const H: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(H[(b >> 4) as usize] as char);
        s.push(H[(b & 15) as usize] as char);
    }
    s
}

/// Streams a file through sha256; returns (hex digest, size in bytes).
pub fn sha256_file(path: &Path) -> io::Result<(String, u64)> {
    let mut f = File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 256 * 1024];
    let mut n_total = 0u64;
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
        n_total += n as u64;
    }
    Ok((hex(&h.finalize()), n_total))
}

/// A writer that hashes and counts everything written through it.
pub struct HashingWriter<W: Write> {
    inner: W,
    hasher: Sha256,
    written: u64,
}

impl<W: Write> HashingWriter<W> {
    pub fn new(inner: W) -> Self {
        HashingWriter { inner, hasher: Sha256::new(), written: 0 }
    }
    /// Feeds bytes already on disk (a resumed download) into the hash without writing them.
    pub fn absorb(&mut self, bytes: &[u8]) {
        self.hasher.update(bytes);
        self.written += bytes.len() as u64;
    }
    pub fn written(&self) -> u64 {
        self.written
    }
    pub fn finish(self) -> (W, String, u64) {
        (self.inner, hex(&self.hasher.finalize()), self.written)
    }
}

impl<W: Write> Write for HashingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.hasher.update(&buf[..n]);
        self.written += n as u64;
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn known_vector() {
        assert_eq!(sha256_hex(b"abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
    }
}

//! Compiles the trusted release public keys into the binary.
//!
//! Sources (all concatenated, duplicates removed):
//!   * `trusted_keys.txt` next to this file: one minisign public key (the base64 line of a
//!     `minisign.pub`) per line; `#` starts a comment.
//!   * the build-time environment variable `ZEROFARM_TRUSTED_KEYS`: keys separated by `,` `;` or
//!     whitespace.
//!
//! Key rotation = ship an agent that trusts old + new, then sign with new (PROTOCOL 2).
use std::{env, fs, path::Path};

fn main() {
    println!("cargo:rerun-if-changed=trusted_keys.txt");
    println!("cargo:rerun-if-env-changed=ZEROFARM_TRUSTED_KEYS");
    let mut keys: Vec<String> = Vec::new();
    if let Ok(text) = fs::read_to_string("trusted_keys.txt") {
        for line in text.lines() {
            let line = line.split('#').next().unwrap_or("").trim();
            if !line.is_empty() {
                keys.push(line.to_string());
            }
        }
    }
    if let Ok(v) = env::var("ZEROFARM_TRUSTED_KEYS") {
        for k in v.split(|c: char| c == ',' || c == ';' || c.is_whitespace()) {
            let k = k.trim();
            if !k.is_empty() {
                keys.push(k.to_string());
            }
        }
    }
    let mut seen = std::collections::BTreeSet::new();
    keys.retain(|k| seen.insert(k.clone()));
    for k in &keys {
        // minisign public key = base64 of "Ed" + 8-byte key id + 32-byte key = 56 chars.
        if k.len() != 56 || !k.starts_with("RW") {
            panic!("trusted key {k:?} is not a minisign public key (expected 56 base64 chars starting with RW)");
        }
    }
    let out = Path::new(&env::var("OUT_DIR").unwrap()).join("trusted_keys.rs");
    let body = keys.iter().map(|k| format!("    {k:?},\n")).collect::<String>();
    fs::write(out, format!("pub const COMPILED_KEYS: &[&str] = &[\n{body}];\n")).unwrap();
}

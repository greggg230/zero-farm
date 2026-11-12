//! Compiles the trusted public keys into the binary: two separate lists (PROTOCOL 2 and 7).
//!
//! * Release keys (sign manifests: agents, workers, runtime packs): `trusted_keys.txt` next to this
//!   file plus the build-time environment variable `ZEROFARM_TRUSTED_KEYS`.
//! * Data keys (sign definitions: what a run or campaign may make a PC download and run):
//!   `trusted_data_keys.txt` plus the build-time variable `ZEROFARM_TRUSTED_DATA_KEYS`.
//!
//! Files: one minisign public key (the base64 line of a `minisign.pub`) per line; `#` starts a
//! comment. Variables: keys separated by `,` `;` or whitespace. Duplicates are removed. A key in
//! both lists fails the build: each check accepts only its own list.
//!
//! Key rotation = ship an agent that trusts old + new, then sign with new (PROTOCOL 2).
use std::{env, fs, path::Path};

fn read_keys(file: &str, var: &str) -> Vec<String> {
    println!("cargo:rerun-if-changed={file}");
    println!("cargo:rerun-if-env-changed={var}");
    let mut keys: Vec<String> = Vec::new();
    if let Ok(text) = fs::read_to_string(file) {
        for line in text.lines() {
            let line = line.split('#').next().unwrap_or("").trim();
            if !line.is_empty() {
                keys.push(line.to_string());
            }
        }
    }
    if let Ok(v) = env::var(var) {
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
            panic!("trusted key {k:?} ({file} / {var}) is not a minisign public key (expected 56 base64 chars starting with RW)");
        }
    }
    keys
}

fn list(name: &str, keys: &[String]) -> String {
    let body = keys.iter().map(|k| format!("    {k:?},\n")).collect::<String>();
    format!("pub const {name}: &[&str] = &[\n{body}];\n")
}

fn main() {
    let release = read_keys("trusted_keys.txt", "ZEROFARM_TRUSTED_KEYS");
    let data = read_keys("trusted_data_keys.txt", "ZEROFARM_TRUSTED_DATA_KEYS");
    if let Some(k) = release.iter().find(|k| data.contains(k)) {
        panic!("key {k} is both a release key and a data key; a key may sign manifests or definitions, never both");
    }
    let out = Path::new(&env::var("OUT_DIR").unwrap()).join("trusted_keys.rs");
    fs::write(out, format!("{}{}", list("COMPILED_KEYS", &release), list("COMPILED_DATA_KEYS", &data))).unwrap();
}

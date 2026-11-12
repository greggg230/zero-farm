//! The public keys an agent or launcher trusts, and minisign signature checks.
//!
//! Two separate lists (PROTOCOL 2 and 7): **release keys** sign manifests (agents, workers,
//! runtime packs) and **data keys** sign definitions (what a run or campaign may make a PC
//! download and run). Each check accepts only its own list ([`TrustedKeys::compiled`] and
//! [`TrustedKeys::compiled_data`]), and `build.rs` refuses a key on both.
//!
//! Production binaries trust exactly the keys compiled in by `build.rs`. Debug builds (never
//! shipped: `farm-release release-agent` builds with `--release` and refuses a binary that contains
//! the overrides' names) also accept extra keys from the environment variables
//! [`TEST_KEYS_ENV`] and [`TEST_DATA_KEYS_ENV`], so tests can sign with temporary keys.

use base64::Engine as _;
use std::fmt;

include!(concat!(env!("OUT_DIR"), "/trusted_keys.rs"));

/// Debug-build-only env var adding trusted keys (comma separated). Compiled out of release builds.
#[cfg(debug_assertions)]
pub const TEST_KEYS_ENV: &str = "ZEROFARM_TEST_TRUSTED_KEYS";
/// Debug-build-only env var adding trusted data keys (comma separated). Compiled out of release
/// builds.
#[cfg(debug_assertions)]
pub const TEST_DATA_KEYS_ENV: &str = "ZEROFARM_TEST_TRUSTED_DATA_KEYS";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SigError {
    /// The `.minisig` text could not be parsed.
    Encoding(String),
    /// Signed by a key that is not in the trusted list.
    UnknownKey(String),
    /// The signature does not verify with the matching trusted key.
    BadSignature,
    /// No trusted keys at all (a release build without a configured key).
    NoTrustedKeys,
}

impl fmt::Display for SigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SigError::Encoding(m) => write!(f, "malformed signature: {m}"),
            SigError::UnknownKey(id) => write!(f, "signed by unknown key {id}"),
            SigError::BadSignature => write!(f, "signature does not verify"),
            SigError::NoTrustedKeys => write!(f, "this build trusts no release key"),
        }
    }
}
impl std::error::Error for SigError {}

#[derive(Clone)]
pub struct TrustedKey {
    pub b64: String,
    pub key_id: [u8; 8],
    pk: minisign_verify::PublicKey,
}

impl TrustedKey {
    pub fn parse(b64: &str) -> Result<Self, String> {
        let b64 = b64.trim();
        let pk = minisign_verify::PublicKey::from_base64(b64).map_err(|e| format!("bad public key {b64:?}: {e}"))?;
        let raw = base64::engine::general_purpose::STANDARD.decode(b64).map_err(|e| e.to_string())?;
        let mut key_id = [0u8; 8];
        key_id.copy_from_slice(&raw[2..10]);
        Ok(TrustedKey { b64: b64.to_string(), key_id, pk })
    }
    /// The key id as minisign prints it (16 uppercase hex digits of the little-endian u64).
    pub fn key_id_hex(&self) -> String {
        key_id_hex(&self.key_id)
    }
}

pub fn key_id_hex(id: &[u8; 8]) -> String {
    format!("{:016X}", u64::from_le_bytes(*id))
}

#[derive(Clone, Default)]
pub struct TrustedKeys(Vec<TrustedKey>);

/// What a verified signature says.
#[derive(Debug, Clone)]
pub struct SigInfo {
    pub key_id: String,
    pub trusted_comment: String,
}

impl TrustedKeys {
    pub fn parse<S: AsRef<str>>(keys: &[S]) -> Result<Self, String> {
        let mut out: Vec<TrustedKey> = Vec::new();
        for k in keys {
            let k = k.as_ref().trim();
            if k.is_empty() {
                continue;
            }
            let tk = TrustedKey::parse(k)?;
            if !out.iter().any(|o| o.b64 == tk.b64) {
                out.push(tk);
            }
        }
        Ok(TrustedKeys(out))
    }

    /// The release keys this binary trusts (manifests): the compiled-in list, plus (debug builds
    /// only) the test override from the environment.
    pub fn compiled() -> Self {
        #[cfg(debug_assertions)]
        let test = Some(TEST_KEYS_ENV);
        #[cfg(not(debug_assertions))]
        let test = None;
        Self::from_compiled(COMPILED_KEYS, test)
    }

    /// The data keys this binary trusts (definitions, PROTOCOL 7): the compiled-in list, plus
    /// (debug builds only) the test override from the environment.
    pub fn compiled_data() -> Self {
        #[cfg(debug_assertions)]
        let test = Some(TEST_DATA_KEYS_ENV);
        #[cfg(not(debug_assertions))]
        let test = None;
        Self::from_compiled(COMPILED_DATA_KEYS, test)
    }

    fn from_compiled(compiled: &[&str], test_env: Option<&str>) -> Self {
        let mut list: Vec<String> = compiled.iter().map(|s| s.to_string()).collect();
        if let Some(v) = test_env.and_then(|name| std::env::var(name).ok()) {
            list.extend(v.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()));
        }
        // A malformed compiled key fails the build (build.rs); a malformed test key is a test bug.
        TrustedKeys::parse(&list).expect("trusted key list")
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
    pub fn keys(&self) -> &[TrustedKey] {
        &self.0
    }
    pub fn contains_b64(&self, b64: &str) -> bool {
        self.0.iter().any(|k| k.b64 == b64.trim())
    }

    /// Verifies a minisign signature (`.minisig` text) over `data` with whichever trusted key
    /// matches its key id. Prehashed (`ED`) signatures only; legacy `Ed` is refused.
    pub fn verify(&self, data: &[u8], minisig: &str) -> Result<SigInfo, SigError> {
        if self.0.is_empty() {
            return Err(SigError::NoTrustedKeys);
        }
        let sig = minisign_verify::Signature::decode(minisig).map_err(|e| SigError::Encoding(e.to_string()))?;
        let sig_key_id = signature_key_id(minisig).map_err(SigError::Encoding)?;
        for k in &self.0 {
            if k.key_id != sig_key_id {
                continue;
            }
            return match k.pk.verify(data, &sig, false) {
                Ok(()) => Ok(SigInfo { key_id: k.key_id_hex(), trusted_comment: sig.trusted_comment().to_string() }),
                Err(_) => Err(SigError::BadSignature),
            };
        }
        Err(SigError::UnknownKey(key_id_hex(&sig_key_id)))
    }
}

/// The key id a `.minisig` claims (second line: base64 of alg(2) + key id(8) + sig(64)).
pub fn signature_key_id(minisig: &str) -> Result<[u8; 8], String> {
    let line = minisig.lines().nth(1).ok_or("missing signature line")?;
    let raw = base64::engine::general_purpose::STANDARD.decode(line.trim()).map_err(|e| e.to_string())?;
    if raw.len() != 74 {
        return Err(format!("signature line is {} bytes, expected 74", raw.len()));
    }
    let mut id = [0u8; 8];
    id.copy_from_slice(&raw[2..10]);
    Ok(id)
}

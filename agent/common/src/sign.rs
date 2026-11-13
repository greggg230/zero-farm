//! minisign signing (desktop release tool and tests). Signatures are produced by the `minisign`
//! crate (the reference Rust implementation by minisign's author), prehashed `ED` format, so
//! `minisign -Vm manifest.json -p release.pub` verifies them.

use crate::hash::sha256_file;
use crate::manifest::{self, build_id, check_rel_path, FileEntry, Kind, Manifest, MANIFEST_FILE, SIG_FILE};
use std::io::Cursor;
use std::path::Path;

pub struct SigningKey {
    pk: minisign::PublicKey,
    sk: minisign::SecretKey,
}

impl SigningKey {
    /// A fresh Ed25519 key pair with a random key id.
    pub fn generate() -> Result<Self, String> {
        let kp = minisign::KeyPair::generate_unencrypted_keypair().map_err(|e| e.to_string())?;
        Ok(SigningKey { pk: kp.pk, sk: kp.sk })
    }

    /// The base64 public key (second line of `minisign.pub`) that agents compile in.
    pub fn public_b64(&self) -> String {
        self.pk.to_base64()
    }

    pub fn key_id_hex(&self) -> String {
        let mut id = [0u8; 8];
        id.copy_from_slice(&self.pk.keynum()[..8]);
        crate::keys::key_id_hex(&id)
    }

    /// `minisign.pub` file contents.
    pub fn public_file(&self) -> String {
        format!("untrusted comment: minisign public key {}\n{}\n", self.key_id_hex(), self.public_b64())
    }

    /// The unencrypted minisign secret key box. Sensitive: only ever store it DPAPI-wrapped.
    pub fn secret_box_string(&self) -> Result<String, String> {
        Ok(self.sk.to_box(Some("zero-farm release key")).map_err(|e| e.to_string())?.into_string())
    }

    pub fn from_secret_box_string(s: &str) -> Result<Self, String> {
        let sk_box = minisign::SecretKeyBox::from_string(s).map_err(|e| e.to_string())?;
        let sk = sk_box.into_unencrypted_secret_key().map_err(|e| e.to_string())?;
        let pk = minisign::PublicKey::from_secret_key(&sk).map_err(|e| e.to_string())?;
        Ok(SigningKey { pk, sk })
    }

    /// A `.minisig` over `data` with the given trusted comment.
    pub fn sign(&self, data: &[u8], trusted_comment: &str) -> Result<String, String> {
        let untrusted = format!("signature from zero-farm release key {}", self.key_id_hex());
        let sb = minisign::sign(Some(&self.pk), &self.sk, Cursor::new(data), Some(trusted_comment), Some(&untrusted))
            .map_err(|e| e.to_string())?;
        Ok(sb.into_string())
    }

    /// The secret key as a standard minisign secret key file encrypted with `passphrase`:
    /// minisign's own format (scrypt + XOR, BLAKE2b checksum) and default scrypt limits, so
    /// `minisign -S -s <file>` can sign with it. The result is checked by decrypting it with the
    /// `minisign` crate before it is returned.
    pub fn encrypted_secret_file(&self, passphrase: &str) -> Result<String, String> {
        use base64::Engine as _;
        check_passphrase(passphrase)?;
        let mut b = self.sk.to_bytes();
        if b.len() != SK_LEN || b[2..4] != [0, 0] {
            return Err("unexpected secret key layout".into());
        }
        let mut salt = [0u8; 32];
        getrandom::fill(&mut salt).map_err(|e| e.to_string())?;
        b[2..4].copy_from_slice(b"Sc");
        b[6..38].copy_from_slice(&salt);
        b[38..46].copy_from_slice(&KDF_OPSLIMIT.to_le_bytes());
        b[46..54].copy_from_slice(&KDF_MEMLIMIT.to_le_bytes());
        let mut stream = [0u8; SK_LEN - 54];
        scrypt::scrypt(passphrase.as_bytes(), &salt, &kdf_params()?, &mut stream).map_err(|e| e.to_string())?;
        for (x, s) in b[54..].iter_mut().zip(stream) {
            *x ^= s;
        }
        let text = format!(
            "untrusted comment: minisign encrypted secret key (zero-farm release key {})\n{}\n",
            self.key_id_hex(),
            base64::engine::general_purpose::STANDARD.encode(&b)
        );
        let back = Self::from_encrypted_secret_file(&text, passphrase).map_err(|e| format!("self-check failed: {e}"))?;
        if back.sk != self.sk || back.public_b64() != self.public_b64() {
            return Err("self-check failed: the encrypted key does not decrypt to this key".into());
        }
        Ok(text)
    }

    /// Reads a minisign secret key file encrypted with a passphrase (one written by
    /// `encrypted_secret_file` or by `minisign -G`). A wrong passphrase is an error.
    pub fn from_encrypted_secret_file(text: &str, passphrase: &str) -> Result<Self, String> {
        let sk_box = minisign::SecretKeyBox::from_string(text).map_err(|e| e.to_string())?;
        let sk = minisign::SecretKey::from_box(sk_box, Some(passphrase.to_string())).map_err(|e| {
            let m = e.to_string();
            if m.to_ascii_lowercase().contains("wrong password") {
                "wrong passphrase for this key file".to_string()
            } else {
                format!("not a passphrase-encrypted minisign secret key: {m}")
            }
        })?;
        // Kept in memory (and DPAPI-wrapped on disk) as an unencrypted box: clear the KDF fields.
        // The checksum covers only the algorithm, key id and key, so it stays valid.
        let mut b = sk.to_bytes();
        if b.len() != SK_LEN {
            return Err("unexpected secret key layout".into());
        }
        b[2..4].copy_from_slice(&[0, 0]);
        b[6..54].fill(0);
        let sk = minisign::SecretKey::from_bytes(&b).map_err(|e| e.to_string())?;
        let pk = minisign::PublicKey::from_secret_key(&sk).map_err(|e| e.to_string())?;
        let key = SigningKey { pk, sk };
        // Round-trip through the unencrypted box the key file stores, so a key that can't be
        // stored fails here and not at the next signing.
        Self::from_secret_box_string(&key.secret_box_string()?)
    }
}

/// minisign's secret key: sig alg (2) + kdf alg (2) + chk alg (2) + salt (32) + opslimit (8) +
/// memlimit (8) + key id (8) + secret key (64) + checksum (32). The last three are encrypted.
const SK_LEN: usize = 158;
/// minisign's defaults (`minisign -G`, libsodium's scryptsalsa208sha256 "sensitive" limits).
const KDF_OPSLIMIT: u64 = 1_048_576;
const KDF_MEMLIMIT: u64 = 33_554_432;

/// The scrypt parameters minisign derives from those limits (libsodium's `pickparams`):
/// memlimit / (r * 128) = 32768 -> N = 2^15; opslimit / 4 / N / r -> p = 1; r = 8.
fn kdf_params() -> Result<scrypt::Params, String> {
    scrypt::Params::new(15, 8, 1, 32).map_err(|e| e.to_string())
}

/// minisign's own limit is 1024 bytes; an empty passphrase would leave the key unprotected.
pub fn check_passphrase(p: &str) -> Result<(), String> {
    if p.is_empty() {
        return Err("an empty passphrase would leave the key unprotected".into());
    }
    if p.len() > 1024 {
        return Err("a passphrase can't be longer than 1024 bytes (minisign's limit)".into());
    }
    Ok(())
}

/// Lists every file under `dir` (except the manifest and its signature) with size and sha256,
/// sorted by path, using `/` separators.
pub fn list_files(dir: &Path) -> Result<Vec<FileEntry>, String> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).map_err(|e| format!("{}: {e}", d.display()))? {
            let e = e.map_err(|e| e.to_string())?;
            let p = e.path();
            let ft = e.file_type().map_err(|e| e.to_string())?;
            if ft.is_dir() {
                stack.push(p);
                continue;
            }
            let rel = p.strip_prefix(dir).unwrap().to_string_lossy().replace('\\', "/");
            let lower = rel.to_ascii_lowercase();
            if lower == MANIFEST_FILE || lower == SIG_FILE {
                continue;
            }
            check_rel_path(&rel).map_err(|e| e.to_string())?;
            let (sha256, size) = sha256_file(&p).map_err(|e| format!("{rel}: {e}"))?;
            out.push(FileEntry { path: rel, sha256, size, zst: None });
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

/// Fills `template.files` from `dir`, writes `manifest.json` (pretty JSON, stored bytes are what
/// is signed) and `manifest.json.minisig`. Returns the build id and the manifest.
pub fn sign_dir(dir: &Path, template: Manifest, key: &SigningKey) -> Result<(String, Manifest), String> {
    sign_dir_zst(dir, template, key, &std::collections::BTreeMap::new())
}

/// [`sign_dir`], with compressed copies (PROTOCOL 9.3) for the files `zst` names (by manifest path).
pub fn sign_dir_zst(
    dir: &Path,
    mut template: Manifest,
    key: &SigningKey,
    zst: &std::collections::BTreeMap<String, crate::manifest::Packed>,
) -> Result<(String, Manifest), String> {
    template.files = list_files(dir)?;
    for f in &mut template.files {
        f.zst = zst.get(&f.path).cloned();
    }
    if let Some(p) = zst.keys().find(|p| !template.files.iter().any(|f| &f.path == *p)) {
        return Err(format!("a compressed copy of {p}, which is not in the build"));
    }
    template.validate().map_err(|e| e.to_string())?;
    let mut bytes = serde_json::to_vec_pretty(&template).map_err(|e| e.to_string())?;
    bytes.push(b'\n');
    let id = build_id(&bytes);
    let sig = key.sign(&bytes, &manifest::trusted_comment(&id, template.kind))?;
    std::fs::write(dir.join(MANIFEST_FILE), &bytes).map_err(|e| e.to_string())?;
    std::fs::write(dir.join(SIG_FILE), sig).map_err(|e| e.to_string())?;
    Ok((id, template))
}

/// A manifest template with only the required fields set.
pub fn template(kind: Kind, version: &str) -> Manifest {
    Manifest {
        manifest: crate::MANIFEST_VERSION,
        kind,
        version: version.to_string(),
        created: crate::now_unix(),
        game: None,
        engine_commit: None,
        core_commit: None,
        profile: None,
        aw: None,
        zero_onnx: None,
        min_agent: None,
        requires_runtime: None,
        entry: None,
        gpu: None,
        files: vec![],
    }
}

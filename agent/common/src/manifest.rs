//! `manifest/1` (PROTOCOL 2): parse, check the signature, check every file.
//!
//! Nothing executable is run or loaded unless [`verify_dir`] accepted its directory: the
//! signature verifies with a trusted key, the trusted comment names this exact build id and
//! kind, and every listed file has the listed size and sha256. Files that the manifest does not
//! list are refused too (a planted DLL next to an executable would otherwise be loaded).

use crate::hash::{sha256_file, sha256_hex};
use crate::keys::{SigError, TrustedKeys};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fmt;
use std::path::{Component, Path, PathBuf};

pub const MANIFEST_FILE: &str = "manifest.json";
pub const SIG_FILE: &str = "manifest.json.minisig";
/// Upper bound for a manifest file (they are a few KB).
pub const MAX_MANIFEST_BYTES: u64 = 1 << 20;
/// Upper bound for a signature file.
pub const MAX_SIG_BYTES: u64 = 4096;

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Agent,
    Worker,
    Runtime,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Agent => "agent",
            Kind::Worker => "worker",
            Kind::Runtime => "runtime",
        }
    }
    pub fn parse(s: &str) -> Option<Kind> {
        match s {
            "agent" => Some(Kind::Agent),
            "worker" => Some(Kind::Worker),
            "runtime" => Some(Kind::Runtime),
            _ => None,
        }
    }
}

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct FileEntry {
    pub path: String,
    pub sha256: String,
    pub size: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Manifest {
    pub manifest: u32,
    pub kind: Kind,
    pub version: String,
    pub created: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub game: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub engine_commit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub core_commit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aw: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub zero_onnx: Option<Vec<u32>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requires_runtime: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry: Option<String>,
    pub files: Vec<FileEntry>,
}

/// Why a manifest or build directory was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManifestError {
    Io(String),
    Json(String),
    Signature(SigError),
    /// The trusted comment does not say `build_id=<id> kind=<kind>` for this manifest.
    TrustedComment(String),
    BuildIdMismatch { expected: String, actual: String },
    KindMismatch { expected: Kind, actual: Kind },
    Invalid(String),
    BadPath(String),
    MissingFile(String),
    SizeMismatch { path: String, expected: u64, actual: u64 },
    HashMismatch { path: String },
    ExtraFile(String),
}

impl fmt::Display for ManifestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use ManifestError::*;
        match self {
            Io(m) => write!(f, "i/o: {m}"),
            Json(m) => write!(f, "manifest json: {m}"),
            Signature(e) => write!(f, "signature: {e}"),
            TrustedComment(m) => write!(f, "trusted comment: {m}"),
            BuildIdMismatch { expected, actual } => write!(f, "build id {actual}, expected {expected}"),
            KindMismatch { expected, actual } => write!(f, "kind {actual}, expected {expected}"),
            Invalid(m) => write!(f, "invalid manifest: {m}"),
            BadPath(p) => write!(f, "unsafe file path {p:?}"),
            MissingFile(p) => write!(f, "missing file {p}"),
            SizeMismatch { path, expected, actual } => write!(f, "{path}: {actual} bytes, expected {expected}"),
            HashMismatch { path } => write!(f, "{path}: sha256 mismatch"),
            ExtraFile(p) => write!(f, "file not in manifest: {p}"),
        }
    }
}
impl std::error::Error for ManifestError {}

/// build_id = first 24 hex chars of sha256(manifest.json bytes).
pub fn build_id(manifest_bytes: &[u8]) -> String {
    sha256_hex(manifest_bytes)[..24].to_string()
}

/// The trusted comment a release signature carries (PROTOCOL 2).
pub fn trusted_comment(build_id: &str, kind: Kind) -> String {
    format!("build_id={build_id} kind={kind}")
}

/// Checks that a manifest file path is a plain relative path inside the build directory.
/// Accepts `/` or `\` separators; refuses `..`, roots, drive letters, ADS colons, reserved
/// device names, empty components and the manifest's own file names.
pub fn check_rel_path(p: &str) -> Result<PathBuf, ManifestError> {
    let bad = || ManifestError::BadPath(p.to_string());
    if p.is_empty() || p.len() > 260 || p.contains(':') || p.contains('\0') {
        return Err(bad());
    }
    let norm = p.replace('\\', "/");
    if norm.starts_with('/') {
        return Err(bad());
    }
    let mut out = PathBuf::new();
    for part in norm.split('/') {
        if part.is_empty() || part == "." || part == ".." || part.ends_with('.') || part.ends_with(' ') {
            return Err(bad());
        }
        if part.chars().any(|c| c.is_control() || matches!(c, '<' | '>' | '"' | '|' | '?' | '*')) {
            return Err(bad());
        }
        let stem = part.split('.').next().unwrap_or("").to_ascii_uppercase();
        const RESERVED: &[&str] = &["CON", "PRN", "AUX", "NUL", "CONIN$", "CONOUT$"];
        let is_com_lpt = (stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.len() == 4
            && stem.as_bytes()[3].is_ascii_digit();
        if RESERVED.contains(&stem.as_str()) || is_com_lpt {
            return Err(bad());
        }
        out.push(part);
    }
    if out.components().any(|c| !matches!(c, Component::Normal(_))) {
        return Err(bad());
    }
    let lower = norm.to_ascii_lowercase();
    if lower == MANIFEST_FILE || lower == SIG_FILE {
        return Err(bad());
    }
    Ok(out)
}

impl Manifest {
    pub fn parse(bytes: &[u8]) -> Result<Manifest, ManifestError> {
        let m: Manifest = serde_json::from_slice(bytes).map_err(|e| ManifestError::Json(e.to_string()))?;
        m.validate()?;
        Ok(m)
    }

    /// Structural checks that do not need the files.
    pub fn validate(&self) -> Result<(), ManifestError> {
        let inv = |m: &str| Err(ManifestError::Invalid(m.to_string()));
        if self.manifest != crate::MANIFEST_VERSION {
            return inv(&format!("manifest version {} (this agent reads {})", self.manifest, crate::MANIFEST_VERSION));
        }
        if self.version.is_empty() || self.version.len() > 64 {
            return inv("version must be 1-64 chars");
        }
        if self.files.is_empty() {
            return inv("no files");
        }
        let mut seen = BTreeSet::new();
        for f in &self.files {
            check_rel_path(&f.path)?;
            if !crate::is_sha256_hex(&f.sha256) {
                return inv(&format!("{}: sha256 must be 64 lowercase hex chars", f.path));
            }
            if !seen.insert(f.path.replace('\\', "/").to_ascii_lowercase()) {
                return inv(&format!("duplicate file {}", f.path));
            }
        }
        match self.kind {
            Kind::Agent | Kind::Worker => {
                let Some(entry) = &self.entry else { return inv("entry is required for agent and worker manifests") };
                let entry_n = entry.replace('\\', "/").to_ascii_lowercase();
                if !seen.contains(&entry_n) {
                    return inv(&format!("entry {entry} is not one of the files"));
                }
            }
            Kind::Runtime => {}
        }
        if self.kind == Kind::Worker && self.aw.is_none() {
            return inv("worker manifests must declare aw");
        }
        Ok(())
    }

    pub fn total_size(&self) -> u64 {
        self.files.iter().map(|f| f.size).sum()
    }
}

/// Checks a manifest's bytes against its signature: trusted key, valid signature, trusted
/// comment naming this build id and kind, structurally valid. Returns the manifest and build id.
pub fn verify_manifest_bytes(
    keys: &TrustedKeys,
    manifest_bytes: &[u8],
    minisig: &str,
) -> Result<(Manifest, String), ManifestError> {
    let info = keys.verify(manifest_bytes, minisig).map_err(ManifestError::Signature)?;
    let id = build_id(manifest_bytes);
    let m = Manifest::parse(manifest_bytes)?;
    let mut tc_id = None;
    let mut tc_kind = None;
    for pair in info.trusted_comment.split_whitespace() {
        if let Some(v) = pair.strip_prefix("build_id=") {
            tc_id = Some(v);
        } else if let Some(v) = pair.strip_prefix("kind=") {
            tc_kind = Some(v);
        }
    }
    if tc_id != Some(id.as_str()) || tc_kind != Some(m.kind.as_str()) {
        return Err(ManifestError::TrustedComment(format!(
            "{:?} does not match build_id={} kind={}",
            info.trusted_comment, id, m.kind
        )));
    }
    Ok((m, id))
}

/// A build directory that passed [`verify_dir`].
#[derive(Debug, Clone)]
pub struct VerifiedBuild {
    pub dir: PathBuf,
    pub build_id: String,
    pub manifest: Manifest,
}

impl VerifiedBuild {
    pub fn entry_path(&self) -> Option<PathBuf> {
        self.manifest.entry.as_ref().map(|e| self.dir.join(check_rel_path(e).expect("validated")))
    }
}

/// What the caller expects of a build directory.
#[derive(Debug, Clone, Default)]
pub struct Expect<'a> {
    pub build_id: Option<&'a str>,
    pub kind: Option<Kind>,
    pub version: Option<&'a str>,
}

/// Verifies a build directory: `manifest.json` + `manifest.json.minisig` + exactly the listed
/// files, each with the listed size and sha256.
pub fn verify_dir(dir: &Path, keys: &TrustedKeys, expect: &Expect) -> Result<VerifiedBuild, ManifestError> {
    let io = |e: std::io::Error, what: &str| ManifestError::Io(format!("{what}: {e}"));
    let mpath = dir.join(MANIFEST_FILE);
    let spath = dir.join(SIG_FILE);
    let msize = std::fs::metadata(&mpath).map_err(|_| ManifestError::MissingFile(MANIFEST_FILE.into()))?.len();
    if msize > MAX_MANIFEST_BYTES {
        return Err(ManifestError::Invalid("manifest too large".into()));
    }
    let ssize = std::fs::metadata(&spath).map_err(|_| ManifestError::MissingFile(SIG_FILE.into()))?.len();
    if ssize > MAX_SIG_BYTES {
        return Err(ManifestError::Invalid("signature too large".into()));
    }
    let mbytes = std::fs::read(&mpath).map_err(|e| io(e, MANIFEST_FILE))?;
    let sig = std::fs::read_to_string(&spath).map_err(|e| io(e, SIG_FILE))?;
    let (m, id) = verify_manifest_bytes(keys, &mbytes, &sig)?;
    if let Some(want) = expect.build_id {
        if want != id {
            return Err(ManifestError::BuildIdMismatch { expected: want.into(), actual: id });
        }
    }
    if let Some(want) = expect.kind {
        if want != m.kind {
            return Err(ManifestError::KindMismatch { expected: want, actual: m.kind });
        }
    }
    if let Some(want) = expect.version {
        if want != m.version {
            return Err(ManifestError::Invalid(format!("version {} in a directory for {want}", m.version)));
        }
    }
    let mut listed = BTreeSet::new();
    for f in &m.files {
        let rel = check_rel_path(&f.path)?;
        listed.insert(f.path.replace('\\', "/").to_ascii_lowercase());
        let p = dir.join(&rel);
        let md = std::fs::symlink_metadata(&p).map_err(|_| ManifestError::MissingFile(f.path.clone()))?;
        if !md.is_file() {
            return Err(ManifestError::MissingFile(f.path.clone()));
        }
        if md.len() != f.size {
            return Err(ManifestError::SizeMismatch { path: f.path.clone(), expected: f.size, actual: md.len() });
        }
        let (h, n) = sha256_file(&p).map_err(|e| io(e, &f.path))?;
        if n != f.size {
            return Err(ManifestError::SizeMismatch { path: f.path.clone(), expected: f.size, actual: n });
        }
        if h != f.sha256 {
            return Err(ManifestError::HashMismatch { path: f.path.clone() });
        }
    }
    // Refuse anything the manifest does not list.
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).map_err(|e| io(e, "read_dir"))? {
            let e = e.map_err(|e| io(e, "read_dir"))?;
            let p = e.path();
            let ft = e.file_type().map_err(|e| io(e, "file_type"))?;
            let rel = p.strip_prefix(dir).unwrap().to_string_lossy().replace('\\', "/");
            if ft.is_dir() {
                stack.push(p);
                continue;
            }
            let lower = rel.to_ascii_lowercase();
            if lower == MANIFEST_FILE || lower == SIG_FILE || listed.contains(&lower) {
                continue;
            }
            return Err(ManifestError::ExtraFile(rel));
        }
    }
    Ok(VerifiedBuild { dir: dir.to_path_buf(), build_id: id, manifest: m })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rel_paths() {
        assert!(check_rel_path("mtgz-worker.exe").is_ok());
        assert!(check_rel_path("lib/onnxruntime.dll").is_ok());
        assert!(check_rel_path("lib\\x.dll").is_ok());
        for bad in [
            "",
            "../x",
            "a/../../x",
            "/abs",
            "\\abs",
            "C:\\x",
            "c:x",
            "x:stream",
            "a//b",
            "./a",
            "nul",
            "NUL.txt",
            "com1",
            "lpt9.dll",
            "a.",
            "a ",
            "manifest.json",
            "MANIFEST.JSON.minisig",
            "a*b",
        ] {
            assert!(check_rel_path(bad).is_err(), "{bad:?} should be refused");
        }
    }

    #[test]
    fn validate_rules() {
        let f = FileEntry { path: "w.exe".into(), sha256: "0".repeat(64), size: 1 };
        let mut m = Manifest {
            manifest: 1,
            kind: Kind::Worker,
            version: "x".into(),
            created: 0,
            game: Some("mtg".into()),
            engine_commit: None,
            core_commit: None,
            profile: None,
            aw: Some(1),
            zero_onnx: None,
            min_agent: None,
            requires_runtime: None,
            entry: Some("w.exe".into()),
            files: vec![f.clone()],
        };
        assert!(m.validate().is_ok());
        m.entry = Some("other.exe".into());
        assert!(m.validate().is_err());
        m.entry = Some("w.exe".into());
        m.aw = None;
        assert!(m.validate().is_err());
        m.aw = Some(1);
        m.files.push(FileEntry { path: "W.EXE".into(), ..f.clone() });
        assert!(m.validate().is_err(), "case-insensitive duplicate");
        m.files.pop();
        m.manifest = 2;
        assert!(m.validate().is_err());
    }

    #[test]
    fn build_id_is_24_hex() {
        let id = build_id(b"{}");
        assert_eq!(id.len(), 24);
        assert!(crate::is_build_id(&id));
    }
}

//! Worker builds, runtime packs and checkpoints on disk (DESIGN 3.5): downloaded by hash,
//! verified (signature + every file for builds, sha256 and, from a signed definition, size for
//! checkpoints) before first use and again before every worker start, least-recently-used evicted
//! under the user's disk cap. Room for a checkpoint file is made for its declared size before it
//! is downloaded (a legacy lease declares none: then for the most a legacy file may be).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use zerofarm_common::fsutil::{atomic_write, dir_size, remove_dir_all_retry, rename_retry};
use zerofarm_common::http::{CallError, Client, Want};
use zerofarm_common::keys::TrustedKeys;
use zerofarm_common::layout::Layout;
use zerofarm_common::manifest::{
    check_rel_path, verify_dir, verify_manifest_bytes, Expect, Kind, ManifestError, VerifiedBuild, MANIFEST_FILE,
    MAX_MANIFEST_BYTES, MAX_SIG_BYTES, SIG_FILE,
};
use zerofarm_common::definition::CheckpointDef;
use zerofarm_common::protocol::{ArtifactRef, CheckpointRef};

/// Keep at least this much free on the volume regardless of the cap.
pub const VOLUME_RESERVE: u64 = 1 << 30;
/// Largest checkpoint/reference file accepted from a legacy lease (no definition, so no declared
/// size): room for this much is made before it downloads, so the disk cap holds (security review
/// L1). Networks are about 13 MB.
pub const MAX_LEGACY_CKPT_BYTES: u64 = 256 << 20;

#[derive(Debug)]
pub enum CacheError {
    Call(CallError),
    Verify(ManifestError),
    NoSpace { need: u64 },
    Bad(String),
    Stopped,
}

impl std::fmt::Display for CacheError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CacheError::Call(e) => write!(f, "{e}"),
            CacheError::Verify(e) => write!(f, "verification failed: {e}"),
            CacheError::NoSpace { need } => write!(f, "not enough disk space under the cap for {} MB", need >> 20),
            CacheError::Bad(m) => write!(f, "{m}"),
            CacheError::Stopped => write!(f, "stopped"),
        }
    }
}

impl From<CallError> for CacheError {
    fn from(e: CallError) -> Self {
        match e {
            CallError::Stopped => CacheError::Stopped,
            e => CacheError::Call(e),
        }
    }
}

/// Downloads a signed build (manifest + signature + files) into `dest`, staging in `tmp`
/// (kept across attempts so interrupted files resume). Nothing appears under `dest` unless the
/// whole directory verified.
#[allow(clippy::too_many_arguments)]
pub fn download_build(
    client: &Client,
    keys: &TrustedKeys,
    manifest_key: &str,
    expect: &Expect,
    dest: &Path,
    tmp: &Path,
    before_files: &mut dyn FnMut(u64) -> Result<(), CacheError>,
    stop: &dyn Fn() -> bool,
) -> Result<VerifiedBuild, CacheError> {
    let prefix = manifest_key
        .strip_suffix(MANIFEST_FILE)
        .ok_or_else(|| CacheError::Bad(format!("manifest key {manifest_key:?} does not end in {MANIFEST_FILE}")))?;
    std::fs::create_dir_all(tmp).map_err(|e| CacheError::Bad(e.to_string()))?;
    let mpath = tmp.join(MANIFEST_FILE);
    let spath = tmp.join(SIG_FILE);
    // The manifest may be stale from an earlier attempt of a different build: always refetch it.
    let _ = std::fs::remove_file(&mpath);
    let _ = std::fs::remove_file(&spath);
    client.download(manifest_key, &mpath, &Want { size: None, sha256: None, max: MAX_MANIFEST_BYTES }, stop)?;
    client.download(&format!("{manifest_key}.minisig"), &spath, &Want { size: None, sha256: None, max: MAX_SIG_BYTES }, stop)?;
    let mbytes = std::fs::read(&mpath).map_err(|e| CacheError::Bad(e.to_string()))?;
    let sig = std::fs::read_to_string(&spath).map_err(|e| CacheError::Bad(e.to_string()))?;
    let (m, id) = verify_manifest_bytes(keys, &mbytes, &sig).map_err(CacheError::Verify)?;
    if let Some(want) = expect.build_id {
        if want != id {
            return Err(CacheError::Verify(ManifestError::BuildIdMismatch { expected: want.into(), actual: id }));
        }
    }
    if let Some(want) = expect.kind {
        if want != m.kind {
            return Err(CacheError::Verify(ManifestError::KindMismatch { expected: want, actual: m.kind }));
        }
    }
    // Remove anything in tmp the manifest doesn't list (leftovers of another attempt).
    let listed: std::collections::BTreeSet<String> =
        m.files.iter().map(|f| f.path.replace('\\', "/").to_ascii_lowercase()).collect();
    prune_unlisted(tmp, tmp, &listed);
    let missing: u64 = m
        .files
        .iter()
        .filter(|f| {
            let p = tmp.join(check_rel_path(&f.path).expect("validated"));
            std::fs::metadata(&p).map(|md| md.len() != f.size).unwrap_or(true)
        })
        .map(|f| f.size)
        .sum();
    before_files(missing)?;
    for f in &m.files {
        if stop() {
            return Err(CacheError::Stopped);
        }
        let rel = check_rel_path(&f.path).map_err(CacheError::Verify)?;
        let p = tmp.join(&rel);
        if let Ok(md) = std::fs::metadata(&p) {
            if md.len() == f.size && zerofarm_common::hash::sha256_file(&p).map(|(h, _)| h == f.sha256).unwrap_or(false) {
                continue;
            }
            let _ = std::fs::remove_file(&p);
        }
        let key = format!("{prefix}{}", f.path.replace('\\', "/"));
        client.download(&key, &p, &Want { size: Some(f.size), sha256: Some(f.sha256.clone()), max: f.size }, stop)?;
    }
    let v = verify_dir(tmp, keys, expect).map_err(CacheError::Verify)?;
    if dest.exists() {
        remove_dir_all_retry(dest, std::time::Duration::from_secs(10)).map_err(|e| CacheError::Bad(e.to_string()))?;
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| CacheError::Bad(e.to_string()))?;
    }
    rename_retry(tmp, dest).map_err(|e| CacheError::Bad(format!("rename into place: {e}")))?;
    Ok(VerifiedBuild { dir: dest.to_path_buf(), ..v })
}

fn prune_unlisted(base: &Path, dir: &Path, listed: &std::collections::BTreeSet<String>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            prune_unlisted(base, &p, listed);
            continue;
        }
        let rel = p.strip_prefix(base).unwrap().to_string_lossy().replace('\\', "/").to_ascii_lowercase();
        let rel_nopart = rel.strip_suffix(".part").unwrap_or(&rel).to_string();
        if rel == MANIFEST_FILE || rel == SIG_FILE || listed.contains(&rel) || listed.contains(&rel_nopart) {
            continue;
        }
        let _ = std::fs::remove_file(&p);
    }
}

/// Paths of a verified checkpoint, and the sizes its definition declares (None: legacy).
#[derive(Debug, Clone)]
pub struct CkptFiles {
    pub model: PathBuf,
    pub reference: Option<PathBuf>,
    pub model_size: Option<u64>,
    pub reference_size: Option<u64>,
}

pub struct Cache {
    pub layout: Layout,
    pub keys: TrustedKeys,
}

impl Cache {
    pub fn new(layout: Layout, keys: TrustedKeys) -> Self {
        Cache { layout, keys }
    }

    fn lru_path(&self) -> PathBuf {
        self.layout.cache().join("lru.json")
    }

    fn lru(&self) -> BTreeMap<String, i64> {
        std::fs::read(self.lru_path()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    pub fn touch(&self, key: &str) {
        let mut m = self.lru();
        m.insert(key.to_string(), zerofarm_common::now_unix());
        let _ = atomic_write(&self.lru_path(), &serde_json::to_vec(&m).expect("json"));
    }

    fn entry_dir(&self, key: &str) -> PathBuf {
        self.layout.cache().join(key.replace('/', "\\"))
    }

    /// Cached build ids and checkpoints (`<run>/<gen>`) for the lease request.
    pub fn cached(&self) -> (Vec<String>, Vec<String>) {
        let mut builds: Vec<String> = std::fs::read_dir(self.layout.cache_builds())
            .map(|rd| {
                rd.flatten()
                    .filter_map(|e| e.file_name().to_str().map(String::from))
                    .filter(|n| zerofarm_common::is_build_id(n))
                    .collect()
            })
            .unwrap_or_default();
        builds.sort();
        let mut ckpts = Vec::new();
        if let Ok(runs) = std::fs::read_dir(self.layout.cache_ckpt()) {
            for r in runs.flatten() {
                let run = r.file_name().to_string_lossy().to_string();
                if let Ok(gens) = std::fs::read_dir(r.path()) {
                    for g in gens.flatten() {
                        let gen = g.file_name().to_string_lossy().to_string();
                        if g.path().join("model.onnx").exists() {
                            ckpts.push(format!("{run}/{gen}"));
                        }
                    }
                }
            }
        }
        ckpts.sort();
        (builds, ckpts)
    }

    /// Bytes used under the install root (everything counts toward the cap).
    pub fn usage(&self) -> u64 {
        dir_size(&self.layout.root)
    }

    /// Bytes that eviction could free (cache entries not in `keep`).
    pub fn evictable(&self, keep: &[String]) -> u64 {
        self.lru().keys().filter(|k| !keep.contains(k)).map(|k| dir_size(&self.entry_dir(k))).sum()
    }

    /// Makes room for `need` more bytes under `cap`, evicting least-recently-used cache entries
    /// (never those in `keep`). Also keeps [`VOLUME_RESERVE`] free on the volume.
    pub fn ensure_space(&self, need: u64, cap: u64, keep: &[String]) -> Result<(), CacheError> {
        loop {
            let used = self.usage();
            let vol_ok = crate::util::disk_free_bytes(&self.layout.root).map(|f| f >= need + VOLUME_RESERVE).unwrap_or(true);
            if used + need <= cap && vol_ok {
                return Ok(());
            }
            let lru = self.lru();
            let victim = lru.iter().filter(|(k, _)| !keep.contains(k)).min_by_key(|(_, t)| **t).map(|(k, _)| k.clone());
            let Some(victim) = victim else {
                return Err(CacheError::NoSpace { need });
            };
            log::info!("evicting {victim} (least recently used) to make room");
            let _ = remove_dir_all_retry(&self.entry_dir(&victim), std::time::Duration::from_secs(5));
            let mut m = lru;
            m.remove(&victim);
            let _ = atomic_write(&self.lru_path(), &serde_json::to_vec(&m).expect("json"));
        }
    }

    /// A verified build directory for `r` (downloading it if needed).
    pub fn ensure_build(
        &self,
        client: &Client,
        r: &ArtifactRef,
        kind: Kind,
        cap: u64,
        keep: &[String],
        stop: &dyn Fn() -> bool,
    ) -> Result<VerifiedBuild, CacheError> {
        if !zerofarm_common::is_build_id(&r.build_id) {
            return Err(CacheError::Bad(format!("bad build id {:?}", r.build_id)));
        }
        let key = format!("builds/{}", r.build_id);
        let dir = self.entry_dir(&key);
        let expect = Expect { build_id: Some(&r.build_id), kind: Some(kind), version: None };
        if dir.exists() {
            match verify_dir(&dir, &self.keys, &expect) {
                Ok(v) => {
                    self.touch(&key);
                    return Ok(v);
                }
                Err(e) => {
                    log::warn!("cached build {} fails verification ({e}); fetching it again", r.build_id);
                    let _ = remove_dir_all_retry(&dir, std::time::Duration::from_secs(5));
                }
            }
        }
        let tmp = self.layout.cache_tmp().join(&r.build_id);
        let mut room = |need: u64| self.ensure_space(need, cap, keep);
        let v = download_build(client, &self.keys, &r.manifest, &expect, &dir, &tmp, &mut room, stop)?;
        self.touch(&key);
        Ok(v)
    }

    /// Re-verifies a cached build right before a worker starts (PROTOCOL 2).
    pub fn reverify(&self, v: &VerifiedBuild, kind: Kind) -> Result<VerifiedBuild, ManifestError> {
        verify_dir(&v.dir, &self.keys, &Expect { build_id: Some(&v.build_id), kind: Some(kind), version: None })
    }

    pub fn ckpt_key(run: &str, gen: u64) -> String {
        format!("ckpt/{run}/{gen}")
    }

    /// Verified checkpoint files (downloading if needed). The sha256 comes from the lease, which
    /// the caller checked against the definition; `declared` (the definition's entry) adds the
    /// sizes.
    #[allow(clippy::too_many_arguments)]
    pub fn ensure_checkpoint(
        &self,
        client: &Client,
        run: &str,
        ck: &CheckpointRef,
        declared: Option<&CheckpointDef>,
        cap: u64,
        keep: &[String],
        stop: &dyn Fn() -> bool,
    ) -> Result<CkptFiles, CacheError> {
        if !zerofarm_common::is_slug(run) {
            return Err(CacheError::Bad(format!("bad run slug {run:?}")));
        }
        if !zerofarm_common::is_sha256_hex(&ck.sha256) {
            return Err(CacheError::Bad("checkpoint sha256 missing or malformed".into()));
        }
        let key = Self::ckpt_key(run, ck.gen);
        let dir = self.entry_dir(&key);
        let model = dir.join("model.onnx");
        let model_size = declared.map(|d| d.model_size);
        let reference_size = declared.and_then(|d| d.reference_size);
        self.ensure_file(client, &ck.model, &model, &ck.sha256, model_size, cap, keep, stop)?;
        let reference = match (&ck.reference, &ck.reference_sha256) {
            (Some(rkey), Some(rsha)) if zerofarm_common::is_sha256_hex(rsha) => {
                let p = dir.join("reference.npz");
                self.ensure_file(client, rkey, &p, rsha, reference_size, cap, keep, stop)?;
                Some(p)
            }
            (Some(_), _) => return Err(CacheError::Bad("reference without a valid sha256".into())),
            _ => None,
        };
        self.touch(&key);
        Ok(CkptFiles { model, reference, model_size, reference_size })
    }

    #[allow(clippy::too_many_arguments)]
    fn ensure_file(
        &self,
        client: &Client,
        key: &str,
        dest: &Path,
        sha: &str,
        size: Option<u64>,
        cap: u64,
        keep: &[String],
        stop: &dyn Fn() -> bool,
    ) -> Result<(), CacheError> {
        if dest.exists() {
            if Self::reverify_file(dest, sha, size) {
                return Ok(());
            }
            let _ = std::fs::remove_file(dest);
        }
        // Room for the whole file under the cap before a byte is fetched (L1): its declared size,
        // else the most a legacy checkpoint file may be. A partial download already counts.
        let max = size.unwrap_or(MAX_LEGACY_CKPT_BYTES);
        let have = std::fs::metadata(zerofarm_common::http::part_path(dest)).map(|m| m.len()).unwrap_or(0);
        self.ensure_space(max.saturating_sub(have), cap, keep)?;
        client.download(key, dest, &Want { size, sha256: Some(sha.to_string()), max }, stop)?;
        Ok(())
    }

    /// Re-checks a checkpoint file's size (when declared) and sha256 right before handing it to a
    /// worker.
    pub fn reverify_file(path: &Path, sha: &str, size: Option<u64>) -> bool {
        if size.is_some_and(|n| std::fs::metadata(path).map(|m| m.len() != n).unwrap_or(true)) {
            return false;
        }
        zerofarm_common::hash::sha256_file(path).map(|(h, n)| h == sha && size.is_none_or(|s| s == n)).unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lru_eviction_respects_keep_and_order() {
        let d = tempfile::tempdir().unwrap();
        let c = Cache::new(Layout::new(d.path()), TrustedKeys::default());
        for (i, key) in ["builds/aaaaaaaaaaaaaaaaaaaaaaaa", "builds/bbbbbbbbbbbbbbbbbbbbbbbb", "ckpt/eoe2/81"].iter().enumerate() {
            let dir = c.entry_dir(key);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("f"), vec![0u8; 1000]).unwrap();
            let mut m = c.lru();
            m.insert(key.to_string(), 100 + i as i64);
            atomic_write(&c.lru_path(), &serde_json::to_vec(&m).unwrap()).unwrap();
        }
        let used = c.usage();
        // Need room for 1700 more bytes under a cap of used + 600: two evictions, oldest first,
        // but "a" is kept, so "b" and then the checkpoint go.
        let keep = vec!["builds/aaaaaaaaaaaaaaaaaaaaaaaa".to_string()];
        c.ensure_space(1700, used + 600, &keep).unwrap();
        assert!(c.entry_dir("builds/aaaaaaaaaaaaaaaaaaaaaaaa").exists());
        assert!(!c.entry_dir("builds/bbbbbbbbbbbbbbbbbbbbbbbb").exists());
        assert!(!c.entry_dir("ckpt/eoe2/81").exists());
        // Nothing evictable left: refuse.
        assert!(matches!(c.ensure_space(10_000, c.usage() + 1, &keep), Err(CacheError::NoSpace { .. })));
    }
}

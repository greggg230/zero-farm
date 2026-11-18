//! farm-release: the desktop's release tool (DESIGN 4, PROTOCOL 2 + 4).
//!
//! * the release secret key never leaves the desktop: it is a minisign secret key stored
//!   DPAPI-encrypted (CurrentUser) in a key file, usable without a password so per-run builds can
//!   be signed automatically (DESIGN 2.1);
//! * signatures are minisign-compatible (`minisign -Vm manifest.json -p release.pub` verifies);
//! * publishing PUTs the files, then the manifest and signature, then `POST /v1/admin/build`;
//! * an agent release can also be a GitHub Release (through the `gh` CLI): the installer, the
//!   package, `SHA256SUMS` and its signature; the coordinator's `GET /download` redirects to the
//!   installer there (`github_url`);
//! * the key can be exported as a passphrase-encrypted minisign secret key file (a backup that
//!   outlives this PC's DPAPI) and imported again on another PC;
//! * definitions (PROTOCOL 7) are signed with a separate **data key** (`keygen --data`, the same
//!   key file format), never with the release key: `define` validates a `definition/1`, signs it,
//!   and with `--publish` uploads it and registers it with the coordinator;
//! * an agent is signed only when its trusted keys are exactly `common/trusted_keys.txt` and
//!   `common/trusted_data_keys.txt` (no stray build-time key ships to friends).

use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use zerofarm_common::http::{with_retry, Backoff, Client};
use zerofarm_common::keys::TrustedKeys;
use zerofarm_common::manifest::{
    verify_dir, Expect, GpuNeed, Kind, Manifest, Packed, VerifiedBuild, MANIFEST_FILE, PLATFORMS, PLATFORM_MIN_AGENT, PLATFORM_WINDOWS_X64, SIG_FILE,
    ZST_MIN_AGENT,
};
use zerofarm_common::sign::{list_files, sign_dir_zst, template, SigningKey};

pub const KEY_FORMAT: &str = "zero-farm-release-key/1";
pub const KEY_PURPOSE: &str = "zero-farm release key";

/// Debug-only names that must never appear in a shipped agent or launcher.
pub const TEST_MARKERS: &[&str] = &[
    "ZEROFARM_TEST_TRUSTED_KEYS",
    "ZEROFARM_TEST_HEADLESS",
    "ZEROFARM_TEST_HEALTHY_TIMEOUT_S",
    "ZEROFARM_TEST_TAKEOVER_CONFIRM_S",
    "ZEROFARM_TEST_HANDOVER_S",
    "ZEROFARM_TEST_TRUSTED_DATA_KEYS",
    "ZEROFARM_TEST_WORKER_PRIORITY",
];

#[derive(Serialize, Deserialize)]
pub struct KeyFile {
    pub format: String,
    pub key_id: String,
    /// The public key (base64 line of minisign.pub) — not secret.
    pub public: String,
    /// base64(DPAPI(CurrentUser, minisign secret key box)).
    pub secret_dpapi: String,
}

pub fn default_key_path() -> PathBuf {
    if let Some(p) = std::env::var_os("ZEROFARM_RELEASE_KEY") {
        return PathBuf::from(p);
    }
    std::env::var_os("USERPROFILE").map(PathBuf::from).unwrap_or_default().join(".zero-farm").join("release.key")
}

/// Generates a key, writes the DPAPI-protected key file and `<file>.pub`; returns the key.
/// Refuses to overwrite an existing key file.
pub fn keygen(out: &Path) -> Result<SigningKey, String> {
    refuse_existing(out)?;
    let key = SigningKey::generate()?;
    write_key_file(out, &key)?;
    Ok(key)
}

fn refuse_existing(out: &Path) -> Result<(), String> {
    if out.exists() {
        return Err(format!("{} already exists; refusing to overwrite a release key", out.display()));
    }
    Ok(())
}

/// Asks for a passphrase: the console without echo for the CLI, a scripted answer in tests.
/// The argument is the prompt.
pub type PassphraseReader<'a> = &'a mut dyn FnMut(&str) -> Result<String, String>;

/// Writes the release key as a standard minisign secret key file encrypted with a passphrase
/// asked for twice, for a backup (a password manager) that outlives this PC and its DPAPI.
/// Refuses to overwrite `out`; never writes anything when the two entries differ.
pub fn export_key(key_file: &Path, out: &Path, ask: PassphraseReader) -> Result<SigningKey, String> {
    refuse_existing(out)?;
    let key = load_key(key_file)?;
    let p1 = ask("Passphrase for the exported key: ")?;
    zerofarm_common::sign::check_passphrase(&p1)?;
    let p2 = ask("The same passphrase again: ")?;
    if p1 != p2 {
        return Err("the two passphrases differ; nothing written".into());
    }
    let text = key.encrypted_secret_file(&p1)?;
    if let Some(d) = out.parent() {
        std::fs::create_dir_all(d).map_err(|e| e.to_string())?;
    }
    write_new(out, text.as_bytes())?;
    Ok(key)
}

/// Reads a passphrase-encrypted minisign secret key file (from `export_key`, or `minisign -G`)
/// and stores it as this tool's DPAPI key file for the current Windows user, so a new PC can sign.
pub fn import_key(from: &Path, out: &Path, ask: PassphraseReader) -> Result<SigningKey, String> {
    refuse_existing(out)?;
    let text = std::fs::read_to_string(from).map_err(|e| format!("{}: {e}", from.display()))?;
    let p = ask(&format!("Passphrase of {}: ", from.display()))?;
    let key = SigningKey::from_encrypted_secret_file(&text, &p)?;
    write_key_file(out, &key)?;
    Ok(key)
}

/// Creates `path`, failing if it already exists (no window between a check and the write).
fn write_new(path: &Path, bytes: &[u8]) -> Result<(), String> {
    use std::io::Write as _;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    f.write_all(bytes).map_err(|e| format!("{}: {e}", path.display()))
}

/// The DPAPI-protected key file and `<file>.pub`.
fn write_key_file(out: &Path, key: &SigningKey) -> Result<(), String> {
    use base64::Engine as _;
    let secret = key.secret_box_string()?;
    let blob = zerofarm_common::dpapi::protect(secret.as_bytes(), KEY_PURPOSE).map_err(|e| e.to_string())?;
    let kf = KeyFile {
        format: KEY_FORMAT.into(),
        key_id: key.key_id_hex(),
        public: key.public_b64(),
        secret_dpapi: base64::engine::general_purpose::STANDARD.encode(blob),
    };
    if let Some(d) = out.parent() {
        std::fs::create_dir_all(d).map_err(|e| e.to_string())?;
    }
    write_new(out, &serde_json::to_vec_pretty(&kf).unwrap())?;
    std::fs::write(pub_path(out), key.public_file()).map_err(|e| e.to_string())?;
    Ok(())
}

pub fn pub_path(key: &Path) -> PathBuf {
    let mut s = key.as_os_str().to_owned();
    s.push(".pub");
    PathBuf::from(s)
}

pub fn read_key_file(path: &Path) -> Result<KeyFile, String> {
    let b = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let kf: KeyFile = serde_json::from_slice(&b).map_err(|e| format!("{}: {e}", path.display()))?;
    if kf.format != KEY_FORMAT {
        return Err(format!("{}: unknown key format {:?}", path.display(), kf.format));
    }
    Ok(kf)
}

/// Decrypts the secret key (only works for the Windows user that created it).
pub fn load_key(path: &Path) -> Result<SigningKey, String> {
    use base64::Engine as _;
    let kf = read_key_file(path)?;
    let blob = base64::engine::general_purpose::STANDARD.decode(&kf.secret_dpapi).map_err(|e| e.to_string())?;
    let secret = zerofarm_common::dpapi::unprotect(&blob, KEY_PURPOSE)
        .map_err(|e| format!("cannot decrypt {} (wrong Windows user?): {e}", path.display()))?;
    let key = SigningKey::from_secret_box_string(&String::from_utf8(secret).map_err(|e| e.to_string())?)?;
    if key.public_b64() != kf.public {
        return Err("key file is inconsistent: secret and public keys differ".into());
    }
    Ok(key)
}

/// Everything `sign` can put in a manifest besides the file list.
#[derive(Debug, Clone, Default)]
pub struct SignOpts {
    pub kind: Option<Kind>,
    pub version: Option<String>,
    pub game: Option<String>,
    pub engine_commit: Option<String>,
    pub core_commit: Option<String>,
    pub profile: Option<String>,
    pub aw: Option<u32>,
    pub zero_onnx: Option<Vec<u32>>,
    pub min_agent: Option<String>,
    pub requires_runtime: Option<String>,
    pub entry: Option<String>,
    /// rev 11 (PROTOCOL 9.2), runtime packs: the hardware the pack needs.
    pub gpu: Option<GpuNeed>,
    /// rev 11 (PROTOCOL 9.3): make zstd copies of the files at this level (in `<dir>.zst`, see [`zst_dir`]).
    pub zst_level: Option<i32>,
    /// rev 17 (PROTOCOL 11.3): the platform the build is for (one of [`PLATFORMS`]); written into the manifest, which
    /// then needs `min_agent` 0.3.0 or newer. None: no `platform` field (= windows-x64, as before).
    pub platform: Option<String>,
}

/// Where `sign --zst` puts a build directory's compressed copies (next to it, so the build directory itself
/// holds exactly the listed files): `<dir>.zst/<path>.zst`.
pub fn zst_dir(dir: &Path) -> PathBuf {
    let mut s = dir.as_os_str().to_owned();
    s.push(".zst");
    PathBuf::from(s)
}

/// Files of at most this size get no compressed copy (PROTOCOL 9.3) ...
pub const ZST_MIN_FILE: u64 = 64 << 10;
/// ... nor files that shrink by less than 5 %.
pub const ZST_MIN_GAIN_PCT: u64 = 5;

fn local(rel: &str) -> String {
    rel.replace('/', std::path::MAIN_SEPARATOR_STR)
}

/// Compressed copies of `dir`'s files into [`zst_dir`] -> by manifest path.
fn pack_dir(dir: &Path, level: i32) -> Result<std::collections::BTreeMap<String, Packed>, String> {
    let zdir = zst_dir(dir);
    if zdir.exists() {
        std::fs::remove_dir_all(&zdir).map_err(|e| format!("{}: {e}", zdir.display()))?;
    }
    // (at most 2 workers: level 19 with a 2^27 window takes several hundred MB of memory per worker)
    let threads = std::thread::available_parallelism().map(|n| n.get() as u32).unwrap_or(2).clamp(1, 2);
    let mut out = std::collections::BTreeMap::new();
    for f in list_files(dir)? {
        if f.size <= ZST_MIN_FILE {
            continue;
        }
        let src = dir.join(local(&f.path));
        let dest = zdir.join(format!("{}.zst", local(&f.path)));
        let t0 = std::time::Instant::now();
        let p = zerofarm_common::zst::pack(&src, &dest, level, threads)?;
        if p.size * 100 >= f.size * (100 - ZST_MIN_GAIN_PCT) {
            let _ = std::fs::remove_file(&dest);
            continue;
        }
        eprintln!("zst {}: {} -> {} bytes ({:.1}%) in {:.0} s", f.path, f.size, p.size, p.size as f64 * 100.0 / f.size as f64, t0.elapsed().as_secs_f64());
        out.insert(f.path.clone(), p);
    }
    Ok(out)
}

fn is_full_sha(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|c| c.is_ascii_hexdigit())
}

/// Builds the manifest for `dir` from `opts` (with the defaults and checks per kind), signs it.
pub fn sign(dir: &Path, opts: &SignOpts, key: &SigningKey) -> Result<(String, Manifest), String> {
    let kind = opts.kind.ok_or("--kind worker|agent|runtime is required")?;
    if TrustedKeys::compiled_data().contains_b64(&key.public_b64()) {
        return Err(format!("key {} is a data key: it signs definitions only; builds are signed with the release key", key.key_id_hex()));
    }
    // rev 17 (PROTOCOL 11.3): a platform this tool knows, and only with agents that know platforms
    if let Some(p) = &opts.platform {
        if !PLATFORMS.contains(&p.as_str()) {
            return Err(format!("--platform {p:?}: one of {}", PLATFORMS.join(", ")));
        }
        if !opts.min_agent.as_deref().is_some_and(|m| zerofarm_common::manifest::semver_at_least(m, PLATFORM_MIN_AGENT)) {
            return Err(format!("--platform needs --min-agent {PLATFORM_MIN_AGENT} or newer (older agents don't know platforms)"));
        }
    }
    let windows = opts.platform.as_deref().is_none_or(|p| p == PLATFORM_WINDOWS_X64);
    // Re-signing: the old manifest/signature are not part of the file list anyway.
    let mut t = template(kind, "");
    t.platform = opts.platform.clone();
    match kind {
        Kind::Worker => {
            let game = opts.game.clone().ok_or("worker builds need --game")?;
            let ec = opts.engine_commit.clone().ok_or("worker builds need --engine-commit (full sha)")?;
            let cc = opts.core_commit.clone().ok_or("worker builds need --core-commit (full sha)")?;
            if !is_full_sha(&ec) || !is_full_sha(&cc) {
                return Err("--engine-commit and --core-commit must be full 40-hex shas".into());
            }
            t.version = opts.version.clone().unwrap_or_else(|| format!("{game}-{}", &ec[..7]));
            t.game = Some(game);
            t.engine_commit = Some(ec.to_lowercase());
            t.core_commit = Some(cc.to_lowercase());
            t.profile = Some(opts.profile.clone().unwrap_or_else(|| "release".into()));
            t.aw = Some(opts.aw.unwrap_or(zerofarm_common::AW_VERSION));
            t.zero_onnx = Some(opts.zero_onnx.clone().unwrap_or_else(|| vec![1]));
            t.min_agent = opts.min_agent.clone();
            t.requires_runtime = opts.requires_runtime.clone();
            t.entry = Some(match &opts.entry {
                Some(e) => e.clone(),
                None => single_exe(dir, windows)?,
            });
        }
        Kind::Agent => {
            let v = opts.version.clone().ok_or("agent builds need --version (semver)")?;
            if semver_ok(&v).is_none() {
                return Err(format!("agent version {v:?} is not semver"));
            }
            t.version = v;
            t.entry = Some(opts.entry.clone().unwrap_or_else(|| if windows { "farm-agent.exe" } else { "farm-agent" }.into()));
            t.min_agent = opts.min_agent.clone();
        }
        Kind::Runtime => {
            t.version = opts.version.clone().ok_or("runtime packs need --version")?;
            t.min_agent = opts.min_agent.clone();
            t.entry = opts.entry.clone();
            t.gpu = opts.gpu.clone();
        }
    }
    if opts.gpu.is_some() && kind != Kind::Runtime {
        return Err("--gpu-vendor/--gpu-providers: only a runtime pack declares the GPU it needs".into());
    }
    let new_enough = t.min_agent.as_deref().is_some_and(|m| zerofarm_common::manifest::semver_at_least(m, ZST_MIN_AGENT));
    if (opts.zst_level.is_some() || t.gpu.is_some()) && !new_enough {
        return Err(format!("--zst and --gpu-* need --min-agent {ZST_MIN_AGENT} or newer (older agents don't read them)"));
    }
    if let Some(r) = &t.requires_runtime {
        if !zerofarm_common::is_build_id(r) {
            return Err(format!("--requires-runtime {r:?} is not a build id"));
        }
    }
    let zst = match opts.zst_level {
        Some(level) => pack_dir(dir, level)?,
        None => {
            let zdir = zst_dir(dir);
            if zdir.exists() {
                std::fs::remove_dir_all(&zdir).map_err(|e| format!("{}: {e}", zdir.display()))?;
            }
            Default::default()
        }
    };
    let (id, m) = sign_dir_zst(dir, t, key, &zst)?;
    // Self-check exactly as an agent will.
    let keys = TrustedKeys::parse(&[key.public_b64()])?;
    verify_dir(dir, &keys, &Expect { build_id: Some(&id), kind: Some(kind), version: None }).map_err(|e| format!("self-check failed: {e}"))?;
    Ok((id, m))
}

fn semver_ok(v: &str) -> Option<()> {
    let core = v.split(['-', '+']).next()?;
    let parts: Vec<&str> = core.split('.').collect();
    (parts.len() == 3 && parts.iter().all(|p| !p.is_empty() && p.bytes().all(|c| c.is_ascii_digit()))).then_some(())
}

/// The worker's executable when `--entry` isn't given: the one `.exe` in `dir`; for another platform (PROTOCOL 11.3:
/// no extension) the one top-level file without an extension.
fn single_exe(dir: &Path, windows: bool) -> Result<String, String> {
    let exes: Vec<String> = std::fs::read_dir(dir)
        .map_err(|e| e.to_string())?
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .filter_map(|e| e.file_name().to_str().map(String::from))
        .filter(|n| if windows { n.to_ascii_lowercase().ends_with(".exe") } else { !n.contains('.') })
        .collect();
    match exes.as_slice() {
        [one] => Ok(one.clone()),
        _ if windows => Err(format!("give --entry: {} .exe files in {}", exes.len(), dir.display())),
        _ => Err(format!("give --entry: {} files without an extension in {}", exes.len(), dir.display())),
    }
}

/// Trusted keys for `verify`/`publish`: an explicit public key, the key file's public key, or
/// the keys compiled into this tool.
pub fn keys_for(pubkey: Option<&str>, key_file: Option<&Path>) -> Result<TrustedKeys, String> {
    if let Some(p) = pubkey {
        return TrustedKeys::parse(&[p]);
    }
    if let Some(k) = key_file {
        if k.exists() {
            return TrustedKeys::parse(&[read_key_file(k)?.public]);
        }
    }
    let c = TrustedKeys::compiled();
    if c.is_empty() {
        return Err("no public key: pass --pubkey or --key".into());
    }
    Ok(c)
}

pub fn verify(dir: &Path, keys: &TrustedKeys) -> Result<VerifiedBuild, String> {
    verify_dir(dir, keys, &Expect::default()).map_err(|e| e.to_string())
}

pub fn admin_client(url: &str, token: &str) -> Result<Client, String> {
    Ok(Client::new(url, Some(token.to_string()), concat!("farm-release/", env!("CARGO_PKG_VERSION")))?.without_agent_headers())
}

/// Uploads a verified build: files, then manifest + signature, then `POST /v1/admin/build`.
pub fn publish(dir: &Path, keys: &TrustedKeys, client: &Client) -> Result<String, String> {
    let v = verify(dir, keys)?;
    let id = v.build_id.clone();
    let zdir = zst_dir(dir);
    for f in &v.manifest.files {
        let rel = f.path.replace('\\', "/");
        put_object(client, &format!("builds/{id}/{rel}"), &dir.join(local(&rel)), Some(&f.sha256))?;
        // rev 11 (PROTOCOL 9.3): the compressed copy too (the plain file stays, for older agents and tools)
        if let Some(z) = &f.zst {
            let zp = zdir.join(format!("{}.zst", local(&rel)));
            let (h, n) = zerofarm_common::hash::sha256_file(&zp).map_err(|e| format!("{}: {e} (sign it again with --zst)", zp.display()))?;
            if h != z.sha256 || n != z.size {
                return Err(format!("{}: not the compressed copy the manifest names (sign it again with --zst)", zp.display()));
            }
            put_object(client, &format!("builds/{id}/{rel}.zst"), &zp, Some(&z.sha256))?;
        }
    }
    put_object(client, &format!("builds/{id}/{MANIFEST_FILE}"), &dir.join(MANIFEST_FILE), None)?;
    put_object(client, &format!("builds/{id}/{SIG_FILE}"), &dir.join(SIG_FILE), None)?;
    let never = || false;
    with_retry(&mut Backoff::default(), Some(6), &never, || {
        client.post_json::<_, serde_json::Value>("/v1/admin/build", &serde_json::json!({"build_id": id}))
    })
    .map_err(|e| format!("POST /v1/admin/build: {e}"))?;
    Ok(id)
}

/// Objects up to this size go up in one PUT; bigger ones in parts (a Worker request's body is limited).
pub const SIMPLE_PUT_MAX: u64 = 64 << 20;
/// The part size of a multipart upload (R2: every part but the last the same size, at least 5 MiB).
pub const PART_SIZE: u64 = 32 << 20;

fn pct(s: &str) -> String {
    s.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-._~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect()
}

/// Uploads one file to the R2 key `key` (PROTOCOL 4): one PUT (with `x-sha256` when given), or a multipart upload
/// for a file over [`SIMPLE_PUT_MAX`].
pub fn put_object(client: &Client, key: &str, file: &Path, sha256: Option<&str>) -> Result<(), String> {
    use std::io::{Read, Seek, SeekFrom};
    let never = || false;
    let len = std::fs::metadata(file).map_err(|e| format!("{}: {e}", file.display()))?.len();
    let path = format!("/v1/admin/artifact/{key}");
    if len <= SIMPLE_PUT_MAX {
        let bytes = std::fs::read(file).map_err(|e| format!("{}: {e}", file.display()))?;
        let extra: Vec<(&str, String)> = sha256.map(|h| vec![("x-sha256", h.to_string())]).unwrap_or_default();
        with_retry(&mut Backoff::default(), Some(6), &never, || client.put_bytes(&path, &bytes, &extra)).map_err(|e| format!("PUT {key}: {e}"))?;
        return Ok(());
    }
    let start: serde_json::Value = with_retry(&mut Backoff::default(), Some(6), &never, || client.post_json(&format!("{path}?uploads"), &serde_json::json!({})))
        .map_err(|e| format!("POST {key}?uploads: {e}"))?;
    let uid = start["upload_id"].as_str().ok_or_else(|| format!("POST {key}?uploads: no upload_id in {start}"))?.to_string();
    let mut f = std::fs::File::open(file).map_err(|e| format!("{}: {e}", file.display()))?;
    let mut parts = Vec::new();
    let n_parts = len.div_ceil(PART_SIZE);
    for i in 0..n_parts {
        let off = i * PART_SIZE;
        let mut buf = vec![0u8; PART_SIZE.min(len - off) as usize];
        f.seek(SeekFrom::Start(off)).map_err(|e| e.to_string())?;
        f.read_exact(&mut buf).map_err(|e| format!("{}: {e}", file.display()))?;
        let url = format!("{path}?upload_id={}&part={}", pct(&uid), i + 1);
        let r = with_retry(&mut Backoff::default(), Some(8), &never, || client.put_bytes(&url, &buf, &[])).map_err(|e| format!("PUT {key} part {}: {e}", i + 1))?;
        let etag = r["etag"].as_str().ok_or_else(|| format!("PUT {key} part {}: no etag in {r}", i + 1))?.to_string();
        parts.push(serde_json::json!({"part": i + 1, "etag": etag}));
        eprintln!("uploaded {key} part {}/{n_parts}", i + 1);
    }
    let done: serde_json::Value = with_retry(&mut Backoff::default(), Some(6), &never, || {
        client.post_json(&format!("{path}?upload_id={}", pct(&uid)), &serde_json::json!({"parts": parts}))
    })
    .map_err(|e| format!("completing {key}: {e}"))?;
    if done["size"].as_u64() != Some(len) {
        return Err(format!("completing {key}: the coordinator says {done}, the file has {len} bytes"));
    }
    Ok(())
}

/// The installer for an agent release.
#[derive(Debug, Clone, PartialEq)]
pub enum Setup {
    None,
    /// Build `ZeroFarmSetup.exe` with the signed package embedded.
    Build,
    /// Use this already-built installer (tests; re-releasing the same build).
    Prebuilt(PathBuf),
}

#[derive(Debug, Clone)]
pub struct ReleaseAgentOpts {
    pub workspace: PathBuf,
    /// Use already-built binaries from this directory instead of `cargo build --release`.
    pub bin_dir: Option<PathBuf>,
    pub out: Option<PathBuf>,
    pub channel: String,
    pub min_agent: Option<String>,
    pub publish: Option<(String, String)>,
    /// Tests only: accept debug binaries (they honour test-only environment overrides).
    pub allow_test_binaries: bool,
    /// The installer: built after signing; a GitHub release needs one.
    pub setup: Setup,
    /// Also make a GitHub Release with the installer and the package (before the coordinator's
    /// release, which then names the installer's GitHub URL for `GET /download`).
    pub github: Option<GithubOpts>,
}

pub struct Released {
    pub version: String,
    pub build_id: String,
    pub dir: PathBuf,
    /// `dist/ZeroFarmSetup.exe` (next to `dir`) when an installer was made.
    pub setup: Option<PathBuf>,
    pub github: Option<GithubReleased>,
}

fn run_capture(exe: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new(exe).args(args).output().map_err(|e| format!("{}: {e}", exe.display()))?;
    if !out.status.success() {
        return Err(format!("{} {args:?} failed: {}", exe.display(), String::from_utf8_lossy(&out.stderr)));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn target_dir(workspace: &Path) -> Result<PathBuf, String> {
    let out = Command::new("cargo")
        .args(["metadata", "--format-version", "1", "--no-deps"])
        .current_dir(workspace)
        .output()
        .map_err(|e| e.to_string())?;
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).map_err(|e| format!("cargo metadata: {e}"))?;
    v["target_directory"].as_str().map(PathBuf::from).ok_or_else(|| "cargo metadata: no target_directory".into())
}

/// Builds (or takes) farm-agent.exe + ZeroFarm.exe, checks them, signs them as an agent
/// package, and optionally publishes it and points `latest` for the channel at it.
pub fn release_agent(opts: &ReleaseAgentOpts, key: &SigningKey) -> Result<Released, String> {
    check_channel(&opts.channel)?;
    if let Some(g) = &opts.github {
        g.check()?;
        if opts.setup == Setup::None {
            return Err("a GitHub release needs the installer (--setup or --setup-exe)".into());
        }
    }
    let bins = match &opts.bin_dir {
        Some(d) => d.clone(),
        None => {
            let st = Command::new("cargo")
                .args(["build", "--release", "-p", "farm-agent", "-p", "zerofarm-launcher"])
                .current_dir(&opts.workspace)
                .status()
                .map_err(|e| format!("cargo: {e}"))?;
            if !st.success() {
                return Err("cargo build --release failed".into());
            }
            target_dir(&opts.workspace)?.join("release")
        }
    };
    let agent = bins.join("farm-agent.exe");
    let launcher = bins.join("ZeroFarm.exe");
    for b in [&agent, &launcher] {
        if !b.exists() {
            return Err(format!("{} not found", b.display()));
        }
        let bytes = std::fs::read(b).map_err(|e| e.to_string())?;
        if !opts.allow_test_binaries {
            for m in TEST_MARKERS {
                if bytes.windows(m.len()).any(|w| w == m.as_bytes()) {
                    return Err(format!("{} is a debug build (contains {m}); release builds only", b.display()));
                }
            }
        }
    }
    let version = run_capture(&agent, &["version"])?;
    if semver_ok(&version).is_none() {
        return Err(format!("farm-agent reports version {version:?}"));
    }
    let agent_keys = parse_keys_output(&run_capture(&agent, &["keys"])?)?;
    if !agent_keys.release.contains(&key.public_b64()) {
        return Err(format!(
            "this farm-agent.exe does not trust the signing key {} — it could never verify its own updates. \
             Add the public key to common/trusted_keys.txt and rebuild.",
            key.key_id_hex()
        ));
    }
    // Exactly the keys of the source tree (security review L3): a stray ZEROFARM_TRUSTED_KEYS or
    // ZEROFARM_TRUSTED_DATA_KEYS at build time would otherwise ship an extra root of trust.
    let want = expected_keys(&opts.workspace, opts.allow_test_binaries)?;
    check_key_set("farm-agent.exe", &agent_keys, &want)?;
    let launcher_keys = parse_keys_output(&run_capture(&launcher, &["keys"])?)?;
    check_key_set("ZeroFarm.exe", &launcher_keys, &KeySet { release: want.release.clone(), data: BTreeSet::new() })?;
    let out = opts.out.clone().unwrap_or_else(|| opts.workspace.join("dist").join(format!("agent-{version}")));
    if out.exists() {
        std::fs::remove_dir_all(&out).map_err(|e| e.to_string())?;
    }
    std::fs::create_dir_all(&out).map_err(|e| e.to_string())?;
    std::fs::copy(&agent, out.join("farm-agent.exe")).map_err(|e| e.to_string())?;
    std::fs::copy(&launcher, out.join("ZeroFarm.exe")).map_err(|e| e.to_string())?;
    let (build_id, _) = sign(
        &out,
        &SignOpts { kind: Some(Kind::Agent), version: Some(version.clone()), min_agent: opts.min_agent.clone(), ..Default::default() },
        key,
    )?;
    let dist = out.parent().unwrap_or(&out).to_path_buf();
    let setup = match &opts.setup {
        Setup::None => None,
        Setup::Build => Some(build_setup(&opts.workspace, &out, &dist)?),
        Setup::Prebuilt(p) => {
            let dest = dist.join("ZeroFarmSetup.exe");
            if p != &dest {
                std::fs::copy(p, &dest).map_err(|e| format!("{}: {e}", p.display()))?;
            }
            Some(dest)
        }
    };
    // The build goes up first (agents update from the coordinator), then the GitHub release, and
    // only then does the channel point at the release: GET /download never names a missing asset.
    let client = match &opts.publish {
        Some((url, token)) => {
            let client = admin_client(url, token)?;
            publish(&out, &TrustedKeys::parse(&[key.public_b64()])?, &client)?;
            Some(client)
        }
        None => None,
    };
    let github = match (&opts.github, &setup) {
        (Some(g), Some(s)) => Some(github_release(&out, s, key, &opts.channel, g, false)?),
        _ => None,
    };
    if let Some(client) = &client {
        post_agent_release(client, &version, &build_id, &opts.channel, opts.min_agent.as_deref(), github.as_ref().map(|g| g.installer_url.as_str()))?;
    }
    Ok(Released { version, build_id, dir: out, setup, github })
}

/// The trusted keys a binary reports with its `keys` command.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KeySet {
    pub release: BTreeSet<String>,
    pub data: BTreeSet<String>,
}

/// Parses `farm-agent keys` / `ZeroFarm.exe keys`: `release <id> <key>` and `data <id> <key>`
/// lines (agents before 0.2.5 print `<id> <key>`, release keys only).
pub fn parse_keys_output(out: &str) -> Result<KeySet, String> {
    let mut k = KeySet::default();
    for line in out.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let f: Vec<&str> = line.split_whitespace().collect();
        match f.as_slice() {
            ["release", _, b64] | [_, b64] => k.release.insert(b64.to_string()),
            ["data", _, b64] => k.data.insert(b64.to_string()),
            _ => return Err(format!("unexpected line in the keys output: {line:?}")),
        };
    }
    Ok(k)
}

/// The keys in a trusted-keys file (`#` comments, one key per line), as `build.rs` reads it.
pub fn key_file_keys(path: &Path) -> Result<BTreeSet<String>, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(text.lines().map(|l| l.split('#').next().unwrap_or("").trim()).filter(|l| !l.is_empty()).map(String::from).collect())
}

/// What a released binary must trust: `common/trusted_keys.txt` and `common/trusted_data_keys.txt`
/// of `workspace`; with test binaries, plus the test keys of this process's environment.
pub fn expected_keys(workspace: &Path, test_binaries: bool) -> Result<KeySet, String> {
    let common = workspace.join("common");
    let mut k = KeySet { release: key_file_keys(&common.join("trusted_keys.txt"))?, data: key_file_keys(&common.join("trusted_data_keys.txt"))? };
    if test_binaries {
        let env = |name: &str| std::env::var(name).unwrap_or_default().split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect::<Vec<_>>();
        k.release.extend(env("ZEROFARM_TEST_TRUSTED_KEYS"));
        k.data.extend(env("ZEROFARM_TEST_TRUSTED_DATA_KEYS"));
    }
    Ok(k)
}

/// A binary's trusted keys must be exactly `want`, both lists.
pub fn check_key_set(what: &str, got: &KeySet, want: &KeySet) -> Result<(), String> {
    let diff = |kind: &str, got: &BTreeSet<String>, want: &BTreeSet<String>| -> Option<String> {
        let extra: Vec<&String> = got.difference(want).collect();
        let missing: Vec<&String> = want.difference(got).collect();
        (!extra.is_empty() || !missing.is_empty()).then(|| format!("{kind} keys: extra {extra:?}, missing {missing:?}"))
    };
    let problems: Vec<String> = [diff("release", &got.release, &want.release), diff("data", &got.data, &want.data)].into_iter().flatten().collect();
    if problems.is_empty() {
        return Ok(());
    }
    Err(format!(
        "{what} does not trust exactly the keys in common/trusted_keys.txt and common/trusted_data_keys.txt ({}); \
         rebuild it without ZEROFARM_TRUSTED_KEYS / ZEROFARM_TRUSTED_DATA_KEYS set",
        problems.join("; ")
    ))
}

fn check_channel(channel: &str) -> Result<(), String> {
    if !matches!(channel, "canary" | "stable") {
        return Err("--channel must be canary or stable".into());
    }
    Ok(())
}

/// `POST /v1/admin/agent/release`: points the channel's `latest` at the build (agent
/// self-updates) and, with `github_url`, `GET /download` at the installer on GitHub.
pub fn post_agent_release(
    client: &Client,
    version: &str,
    build_id: &str,
    channel: &str,
    min_agent: Option<&str>,
    github_url: Option<&str>,
) -> Result<(), String> {
    post_agent_release_for(client, version, build_id, channel, min_agent, github_url, None)
}

/// [`post_agent_release`] with rev 17's `platform` (PROTOCOL 11.2; sent only when given: absent = windows-x64).
pub fn post_agent_release_for(
    client: &Client,
    version: &str,
    build_id: &str,
    channel: &str,
    min_agent: Option<&str>,
    github_url: Option<&str>,
    platform: Option<&str>,
) -> Result<(), String> {
    let mut body = serde_json::json!({"version": version, "build_id": build_id, "channel": channel,
                                      "min_agent": min_agent, "github_url": github_url});
    if let Some(p) = platform {
        body["platform"] = serde_json::json!(p);
    }
    with_retry(&mut Backoff::default(), Some(6), &|| false, || client.post_json::<_, serde_json::Value>("/v1/admin/agent/release", &body))
        .map_err(|e| format!("POST /v1/admin/agent/release: {e}"))?;
    Ok(())
}

// ---------- Definitions (PROTOCOL 7) ----------

/// What `define` did (printed as one JSON line).
#[derive(Debug, Clone, Serialize)]
pub struct Defined {
    pub kind: String,
    pub id: String,
    pub version: u64,
    /// `defs/<kind>/<id>/<version>/definition.json`
    pub key: String,
    pub sha256: String,
    /// The `.minisig` written next to the definition file.
    pub signature: PathBuf,
    pub published: bool,
}

/// Validates `file` against `definition/1`, signs its bytes exactly as they are (minisign,
/// prehashed) with a data key, writes `<file>.minisig`, and with `publish` uploads both to
/// `defs/<kind>/<id>/<version>/` (`PUT /v1/admin/artifact/...`) and registers the definition
/// (`PUT /v1/admin/definition`), which makes it current for that run or campaign.
pub fn define(file: &Path, key: &SigningKey, publish: Option<&Client>) -> Result<Defined, String> {
    use zerofarm_common::definition::{self as def, Definition};
    let bytes = std::fs::read(file).map_err(|e| format!("{}: {e}", file.display()))?;
    let d = Definition::parse(&bytes).map_err(|e| format!("{}: {e}", file.display()))?;
    // A data key, never a release key: each check accepts only its own list.
    if TrustedKeys::compiled().contains_b64(&key.public_b64()) {
        return Err(format!("key {} is the release key; definitions are signed with a data key (farm-release keygen --data)", key.key_id_hex()));
    }
    let data_keys = TrustedKeys::compiled_data();
    if !data_keys.contains_b64(&key.public_b64()) {
        return Err(format!(
            "key {} is not a trusted data key, so agents would refuse this definition: add its public key to \
             agent/common/trusted_data_keys.txt and release an agent first",
            key.key_id_hex()
        ));
    }
    let sig = key.sign(&bytes, &d.trusted_comment())?;
    // Self-check exactly as an agent will.
    def::verify(&data_keys, &bytes, &sig).map_err(|e| format!("self-check failed: {e}"))?;
    let mut sig_path = file.as_os_str().to_owned();
    sig_path.push(def::SIG_SUFFIX);
    let sig_path = PathBuf::from(sig_path);
    std::fs::write(&sig_path, &sig).map_err(|e| format!("{}: {e}", sig_path.display()))?;
    let sha = zerofarm_common::hash::sha256_hex(&bytes);
    let k = d.key();
    if let Some(client) = publish {
        let mut backoff = Backoff::default();
        let never = || false;
        let up = |path: &str, body: &[u8], backoff: &mut Backoff| -> Result<(), String> {
            let sha = zerofarm_common::hash::sha256_hex(body);
            with_retry(backoff, Some(6), &never, || client.put_bytes(&format!("/v1/admin/artifact/{path}"), body, &[("x-sha256", sha.clone())]))
                .map(|_| ())
                .map_err(|e| format!("PUT {path}: {e}"))
        };
        up(&k, &bytes, &mut backoff)?;
        up(&format!("{k}{}", def::SIG_SUFFIX), sig.as_bytes(), &mut backoff)?;
        let body = serde_json::json!({"kind": d.kind, "id": d.id, "version": d.version, "key": k, "sha256": sha});
        with_retry(&mut backoff, Some(6), &never, || client.put_json::<_, serde_json::Value>("/v1/admin/definition", &body))
            .map_err(|e| format!("PUT /v1/admin/definition: {e}"))?;
    }
    Ok(Defined { kind: d.kind, id: d.id, version: d.version, key: k, sha256: sha, signature: sig_path, published: publish.is_some() })
}

// ---------- GitHub Releases ----------

pub const SUMS_FILE: &str = "SHA256SUMS";
pub const SUMS_SIG_FILE: &str = "SHA256SUMS.minisig";
pub const SETUP_FILE: &str = "ZeroFarmSetup.exe";

#[derive(Debug, Clone)]
pub struct GithubOpts {
    /// `owner/name`.
    pub repo: String,
    /// The `gh` CLI to run (tests point this at a fake).
    pub gh: PathBuf,
    /// Release notes (Markdown); default: generated (version, channel, how to verify).
    pub notes_file: Option<PathBuf>,
}

impl GithubOpts {
    pub fn new(repo: &str) -> GithubOpts {
        GithubOpts { repo: repo.to_string(), gh: PathBuf::from("gh"), notes_file: None }
    }

    fn check(&self) -> Result<(), String> {
        let ok_part = |p: &str| !p.is_empty() && p.bytes().all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c));
        match self.repo.split_once('/') {
            Some((o, n)) if ok_part(o) && ok_part(n) => {}
            _ => return Err(format!("--github {:?} must be OWNER/REPO", self.repo)),
        }
        if let Some(f) = &self.notes_file {
            if !f.is_file() {
                return Err(format!("--notes-file {} not found", f.display()));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct GithubReleased {
    pub tag: String,
    /// `https://github.com/<repo>/releases/download/<tag>/ZeroFarmSetup.exe`
    pub installer_url: String,
    /// True when the release already existed (assets replaced with --clobber).
    pub updated: bool,
    /// The directory with `SHA256SUMS`, its signature, the installer copy and the notes.
    pub staging: PathBuf,
}

/// `dist/agent-<v>-github/`: `SHA256SUMS`, its signature, the notes and this version's installer
/// (kept, since `dist/ZeroFarmSetup.exe` is overwritten by the next release).
pub fn github_staging_dir(package: &Path) -> PathBuf {
    package.with_file_name(format!("{}-github", package.file_name().and_then(|n| n.to_str()).unwrap_or("agent")))
}

pub fn github_tag(version: &str) -> String {
    format!("agent-v{version}")
}

pub fn github_installer_url(repo: &str, version: &str) -> String {
    format!("https://github.com/{repo}/releases/download/{}/{SETUP_FILE}", github_tag(version))
}

/// `SHA256SUMS` in the `sha256sum` format (`<hex>  <name>`, LF), sorted by name.
pub fn sha256sums(files: &[(String, PathBuf)]) -> Result<String, String> {
    let mut lines = Vec::new();
    for (name, path) in files {
        let (sha, _) = zerofarm_common::hash::sha256_file(path).map_err(|e| format!("{}: {e}", path.display()))?;
        lines.push(format!("{sha}  {name}\n"));
    }
    lines.sort_by(|a, b| a[66..].cmp(&b[66..]));
    Ok(lines.concat())
}

/// Makes (or updates) the GitHub Release `agent-v<version>` for a signed agent package through
/// the `gh` CLI: assets = the installer, the package files (`farm-agent.exe`, `ZeroFarm.exe`,
/// `manifest.json`, `manifest.json.minisig`), `SHA256SUMS` and `SHA256SUMS.minisig` (signed with
/// the release key, minisign format). A prerelease for canary, a normal release for stable. A
/// re-run uploads with `--clobber` and refreshes the title, notes and prerelease flag.
/// `check_installer`: the installer must embed exactly this package (`github-release` on an
/// existing `dist/`, where `dist/ZeroFarmSetup.exe` may belong to another version).
pub fn github_release(
    package: &Path,
    setup: &Path,
    key: &SigningKey,
    channel: &str,
    o: &GithubOpts,
    check_installer: bool,
) -> Result<GithubReleased, String> {
    check_channel(channel)?;
    o.check()?;
    let v = verify(package, &TrustedKeys::parse(&[key.public_b64()])?)?;
    if v.manifest.kind != Kind::Agent {
        return Err(format!("{} is a {} build, not an agent package", package.display(), v.manifest.kind));
    }
    let version = v.manifest.version.clone();
    let mut names: Vec<String> = v.manifest.files.iter().map(|f| f.path.clone()).collect();
    for need in ["farm-agent.exe", "ZeroFarm.exe"] {
        if !names.iter().any(|n| n == need) {
            return Err(format!("{}: the agent package has no {need}", package.display()));
        }
    }
    if names.iter().any(|n| n.contains('/') || n.contains('#')) {
        return Err("agent package files must be flat names (GitHub assets)".into());
    }
    names.push(MANIFEST_FILE.into());
    names.push(SIG_FILE.into());
    let setup_bytes = std::fs::read(setup).map_err(|e| format!("{}: {e}", setup.display()))?;
    if check_installer {
        let manifest = std::fs::read(package.join(MANIFEST_FILE)).map_err(|e| e.to_string())?;
        let sig = std::fs::read(package.join(SIG_FILE)).map_err(|e| e.to_string())?;
        let has = |needle: &[u8]| setup_bytes.windows(needle.len()).any(|w| w == needle);
        if !has(&manifest) || !has(&sig) {
            return Err(format!(
                "{} does not contain the package in {} (an installer of another version?); rebuild it or pass --setup-exe",
                setup.display(),
                package.display()
            ));
        }
    }

    let tag = github_tag(&version);
    let staging = github_staging_dir(package);
    if staging.exists() {
        std::fs::remove_dir_all(&staging).map_err(|e| format!("{}: {e}", staging.display()))?;
    }
    std::fs::create_dir_all(&staging).map_err(|e| e.to_string())?;
    let setup_copy = staging.join(SETUP_FILE);
    std::fs::write(&setup_copy, &setup_bytes).map_err(|e| e.to_string())?;

    let mut assets: Vec<(String, PathBuf)> = vec![(SETUP_FILE.into(), setup_copy)];
    assets.extend(names.iter().map(|n| (n.clone(), package.join(n))));
    let sums = sha256sums(&assets)?;
    let sums_path = staging.join(SUMS_FILE);
    std::fs::write(&sums_path, &sums).map_err(|e| e.to_string())?;
    let sig = key.sign(sums.as_bytes(), &format!("file={SUMS_FILE} version={version} build_id={}", v.build_id))?;
    TrustedKeys::parse(&[key.public_b64()])?
        .verify(sums.as_bytes(), &sig)
        .map_err(|e| format!("self-check of {SUMS_SIG_FILE} failed: {e}"))?;
    let sums_sig_path = staging.join(SUMS_SIG_FILE);
    std::fs::write(&sums_sig_path, sig).map_err(|e| e.to_string())?;
    assets.push((SUMS_FILE.into(), sums_path));
    assets.push((SUMS_SIG_FILE.into(), sums_sig_path));

    let notes = match &o.notes_file {
        Some(f) => f.clone(),
        None => {
            let p = staging.join("notes.md");
            std::fs::write(&p, default_notes(&o.repo, &version, channel, &v.build_id, &key.public_b64())).map_err(|e| e.to_string())?;
            p
        }
    };
    let title = format!("Zero Farm agent {version}");
    let prerelease = channel == "canary";
    let files: Vec<String> = assets.iter().map(|(_, p)| p.to_string_lossy().into_owned()).collect();
    let notes_s = notes.to_string_lossy().into_owned();

    let view = gh(&o.gh, &["release", "view", &tag, "--repo", &o.repo, "--json", "tagName"])?;
    let updated = if view.status.success() {
        let mut a = vec!["release", "upload", tag.as_str()];
        a.extend(files.iter().map(String::as_str));
        a.extend(["--repo", &o.repo, "--clobber"]);
        gh_ok(&o.gh, &a)?;
        let pre = format!("--prerelease={prerelease}");
        let mut a = vec!["release", "edit", tag.as_str(), "--repo", &o.repo, "--title", &title, "--notes-file", &notes_s, &pre];
        // Promoting a canary release to stable must also move GitHub's "Latest" marker to it
        // (an edit doesn't, and the marker otherwise stays on the previous stable release).
        if !prerelease {
            a.push("--latest");
        }
        gh_ok(&o.gh, &a)?;
        true
    } else if String::from_utf8_lossy(&view.stderr).to_ascii_lowercase().contains("not found") {
        let mut a = vec!["release", "create", tag.as_str()];
        a.extend(files.iter().map(String::as_str));
        a.extend(["--repo", &o.repo, "--title", &title, "--notes-file", &notes_s]);
        if prerelease {
            a.push("--prerelease");
        } else {
            a.push("--latest");
        }
        gh_ok(&o.gh, &a)?;
        false
    } else {
        return Err(format!("gh release view {tag} failed: {}", String::from_utf8_lossy(&view.stderr).trim()));
    };
    Ok(GithubReleased { tag, installer_url: github_installer_url(&o.repo, &version), updated, staging })
}

fn gh(exe: &Path, args: &[&str]) -> Result<std::process::Output, String> {
    Command::new(exe).args(args).output().map_err(|e| format!("{} (the GitHub CLI): {e}", exe.display()))
}

fn gh_ok(exe: &Path, args: &[&str]) -> Result<(), String> {
    let out = gh(exe, args)?;
    if !out.status.success() {
        return Err(format!(
            "gh {} {} failed: {}",
            args.first().unwrap_or(&""),
            args.get(1).unwrap_or(&""),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(())
}

fn default_notes(repo: &str, version: &str, channel: &str, build_id: &str, pubkey: &str) -> String {
    let which = if channel == "canary" {
        "A **canary** (test) build: it goes to the owner's own test PCs first, and to everyone once it is promoted to stable."
    } else {
        "A **stable** build: every installed Zero Farm updates to it by itself."
    };
    format!(
        "Zero Farm agent {version}. {which}\n\n\
         **Install:** download `ZeroFarmSetup.exe` (Windows 10/11, no admin rights; you need an invite code). \
         Installed copies update themselves; you don't need to download this.\n\n\
         **Verify** (with [minisign](https://jedisct1.github.io/minisign/)):\n\n\
         ```\n\
         minisign -Vm SHA256SUMS -P {pubkey}\n\
         sha256sum -c SHA256SUMS\n\
         minisign -Vm manifest.json -P {pubkey}\n\
         ```\n\n\
         On Windows without `sha256sum`: `certutil -hashfile ZeroFarmSetup.exe SHA256` and compare with the line in `SHA256SUMS`.\n\n\
         Agent package build id: `{build_id}`. See [CHANGELOG.md](https://github.com/{repo}/blob/main/CHANGELOG.md) for what changed.\n"
    )
}

/// The payload ZeroFarmSetup.exe embeds (format in `zerofarm_common::payload`).
pub fn pack_payload(dir: &Path) -> Result<Vec<u8>, String> {
    zerofarm_common::payload::pack_dir(dir)
}

/// Builds `ZeroFarmSetup.exe` (the launcher crate's setup binary with the package embedded).
pub fn build_setup(workspace: &Path, package: &Path, out: &Path) -> Result<PathBuf, String> {
    let payload = pack_payload(package)?;
    let ppath = out.join("setup-payload.bin");
    std::fs::write(&ppath, payload).map_err(|e| e.to_string())?;
    let st = Command::new("cargo")
        .args(["build", "--release", "-p", "zerofarm-launcher", "--bin", "ZeroFarmSetup"])
        .env("ZEROFARM_SETUP_PAYLOAD", &ppath)
        .current_dir(workspace)
        .status()
        .map_err(|e| format!("cargo: {e}"))?;
    if !st.success() {
        return Err("cargo build of ZeroFarmSetup failed".into());
    }
    let built = target_dir(workspace)?.join("release").join("ZeroFarmSetup.exe");
    let dest = out.join("ZeroFarmSetup.exe");
    std::fs::copy(&built, &dest).map_err(|e| e.to_string())?;
    let _ = std::fs::remove_file(&ppath);
    Ok(dest)
}

// ---------- macOS agent releases (rev 17, PROTOCOL 11.2) ----------

/// The macOS release's one download (PROTOCOL 11.2): the launcher and the signed agent package, flat.
pub const MAC_TARBALL: &str = "ZeroFarm-macos-arm64.tar.gz";
/// The macOS agent package's files.
pub const MAC_AGENT: &str = "farm-agent";
pub const MAC_LAUNCHER: &str = "ZeroFarm";

/// `agent-macos-v<version>`: a macOS release's GitHub tag (`agent-v<version>` stays Windows').
pub fn github_mac_tag(version: &str) -> String {
    format!("agent-macos-v{version}")
}

/// What the coordinator's `github_url` of a macOS release must be exactly (PROTOCOL 11.2).
pub fn github_mac_tarball_url(repo: &str, version: &str) -> String {
    format!("https://github.com/{repo}/releases/download/{}/{MAC_TARBALL}", github_mac_tag(version))
}

#[derive(Debug, Clone)]
pub struct MacReleaseOpts {
    pub workspace: PathBuf,
    /// The Mac-built release binaries (`farm-agent`, `ZeroFarm`; `tools/mac-agent-build.sh` copies them here).
    pub prebuilt: PathBuf,
    pub out: Option<PathBuf>,
    pub channel: String,
    /// The coordinator release's and the manifest's `min_agent` (0.3.0 or newer; default 0.3.0).
    pub min_agent: Option<String>,
    pub publish: Option<(String, String)>,
    /// Tests only: accept debug binaries.
    pub allow_test_binaries: bool,
    pub github: Option<GithubOpts>,
}

pub struct MacReleased {
    pub version: String,
    pub build_id: String,
    /// The signed package: `dist/agent-macos-<v>/`.
    pub dir: PathBuf,
    /// `dist/agent-macos-<v>-github/ZeroFarm-macos-arm64.tar.gz`.
    pub tarball: PathBuf,
    pub github: Option<GithubReleased>,
}

/// A 64-bit arm64 Mach-O executable (`MH_MAGIC_64`, `CPU_TYPE_ARM64`, `MH_EXECUTE`).
pub fn is_macho_arm64_exe(b: &[u8]) -> bool {
    b.len() >= 16 && b[0..4] == [0xcf, 0xfa, 0xed, 0xfe] && b[4..8] == [0x0c, 0x00, 0x00, 0x01] && b[12..16] == [0x02, 0x00, 0x00, 0x00]
}

/// The minisign public keys compiled into a binary: every `RW` + 54 base64 characters that decodes to `Ed` + key id
/// + key (a release binary can't be run on this PC to print them, as `farm-agent keys` does on Windows).
pub fn scan_keys(b: &[u8]) -> BTreeSet<String> {
    use base64::Engine as _;
    let b64 = |c: u8| c.is_ascii_alphanumeric() || c == b'+' || c == b'/';
    let mut out = BTreeSet::new();
    let mut i = 0;
    while i + 56 <= b.len() {
        if b[i] == b'R' && b[i + 1] == b'W' && b[i..i + 56].iter().all(|&c| b64(c)) {
            if let Ok(raw) = base64::engine::general_purpose::STANDARD.decode(&b[i..i + 56]) {
                if raw.len() == 42 && raw[0..2] == *b"Ed" {
                    out.insert(String::from_utf8_lossy(&b[i..i + 56]).into_owned());
                    i += 56;
                    continue;
                }
            }
        }
        i += 1;
    }
    out
}

fn crc32(data: &[u8]) -> u32 {
    let mut table = [0u32; 256];
    for (n, t) in table.iter_mut().enumerate() {
        let mut c = n as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
        }
        *t = c;
    }
    let mut c = !0u32;
    for &x in data {
        c = table[((c ^ x as u32) & 0xff) as usize] ^ (c >> 8);
    }
    !c
}

/// A gzip-compressed ustar archive of flat `(name, bytes, mode)` entries, all with modification time `mtime`.
pub fn tar_gz(entries: &[(String, Vec<u8>, u32)], mtime: i64) -> Result<Vec<u8>, String> {
    let mut tar = Vec::new();
    for (name, bytes, mode) in entries {
        if name.is_empty() || name.len() > 99 || name.contains('/') || !name.is_ascii() {
            return Err(format!("tar entry name {name:?}"));
        }
        let mut h = [0u8; 512];
        h[..name.len()].copy_from_slice(name.as_bytes());
        let mut put = |off: usize, len: usize, v: u64| {
            let s = format!("{:0w$o}", v, w = len - 1);
            h[off..off + len - 1].copy_from_slice(s.as_bytes());
        };
        put(100, 8, *mode as u64);
        put(108, 8, 0);
        put(116, 8, 0);
        put(124, 12, bytes.len() as u64);
        put(136, 12, mtime.max(0) as u64);
        h[148..156].copy_from_slice(b"        ");
        h[156] = b'0';
        h[257..263].copy_from_slice(b"ustar\0");
        h[263..265].copy_from_slice(b"00");
        let sum: u32 = h.iter().map(|&c| c as u32).sum();
        let s = format!("{sum:06o}\0 ");
        h[148..156].copy_from_slice(s.as_bytes());
        tar.extend_from_slice(&h);
        tar.extend_from_slice(bytes);
        tar.resize(tar.len().div_ceil(512) * 512, 0);
    }
    tar.resize(tar.len() + 1024, 0);
    let mut gz = vec![0x1f, 0x8b, 8, 0];
    gz.extend_from_slice(&(mtime.max(0) as u32).to_le_bytes());
    gz.extend_from_slice(&[2, 3]);
    gz.extend_from_slice(&miniz_oxide::deflate::compress_to_vec(&tar, 9));
    gz.extend_from_slice(&crc32(&tar).to_le_bytes());
    gz.extend_from_slice(&(tar.len() as u32).to_le_bytes());
    Ok(gz)
}

/// The entries of a [`tar_gz`] archive: (name, bytes, mode).
pub fn untar_gz(gz: &[u8]) -> Result<Vec<(String, Vec<u8>, u32)>, String> {
    if gz.len() < 18 || gz[0..3] != [0x1f, 0x8b, 8] {
        return Err("not gzip".into());
    }
    let tar = miniz_oxide::inflate::decompress_to_vec(&gz[10..gz.len() - 8]).map_err(|e| format!("inflate: {e:?}"))?;
    if crc32(&tar).to_le_bytes() != gz[gz.len() - 8..gz.len() - 4] {
        return Err("gzip crc".into());
    }
    let mut out = Vec::new();
    let mut at = 0;
    while at + 512 <= tar.len() && tar[at] != 0 {
        let h = &tar[at..at + 512];
        let name = String::from_utf8_lossy(&h[..100]).trim_end_matches('\0').to_string();
        let oct = |r: std::ops::Range<usize>| u64::from_str_radix(String::from_utf8_lossy(&h[r]).trim_matches(|c: char| c == '\0' || c == ' '), 8).map_err(|e| e.to_string());
        let (mode, size) = (oct(100..108)? as u32, oct(124..136)? as usize);
        let data = tar.get(at + 512..at + 512 + size).ok_or("truncated tar")?.to_vec();
        out.push((name, data, mode));
        at += 512 + size.div_ceil(512) * 512;
    }
    Ok(out)
}

/// Signs prebuilt macOS binaries as an agent package for `macos-arm64` (PROTOCOL 11.2, 11.3), makes the tarball
/// `ZeroFarm-macos-arm64.tar.gz` (the launcher and the package: `ZeroFarm`, `farm-agent`, `manifest.json` and its
/// signature, nothing else), and optionally publishes the build, makes the GitHub Release `agent-macos-v<version>` and
/// points the channel's macOS `latest` at it. The binaries can't run here, so what `release_agent` asks them is read
/// from their bytes: arm64 Mach-O executables, no test-only names, the version markers (`zerofarm_common::binver`),
/// and exactly the source tree's trusted keys.
pub fn release_agent_macos(opts: &MacReleaseOpts, key: &SigningKey) -> Result<MacReleased, String> {
    use zerofarm_common::manifest::{semver_at_least, PLATFORM_MACOS_ARM64};
    check_channel(&opts.channel)?;
    if let Some(g) = &opts.github {
        g.check()?;
    }
    let min_agent = opts.min_agent.clone().unwrap_or_else(|| PLATFORM_MIN_AGENT.into());
    if !semver_at_least(&min_agent, PLATFORM_MIN_AGENT) {
        return Err(format!("--min-agent {min_agent}: a macOS release needs {PLATFORM_MIN_AGENT} or newer (the first Mac agent)"));
    }
    let read = |name: &str| -> Result<Vec<u8>, String> {
        let p = opts.prebuilt.join(name);
        let b = std::fs::read(&p).map_err(|e| format!("{}: {e}", p.display()))?;
        if !is_macho_arm64_exe(&b) {
            return Err(format!("{} is not an arm64 Mach-O executable (build it on the Mac: tools/mac-agent-build.sh)", p.display()));
        }
        if !opts.allow_test_binaries {
            if let Some(m) = TEST_MARKERS.iter().find(|m| b.windows(m.len()).any(|w| w == m.as_bytes())) {
                return Err(format!("{} is a debug build (contains {m}); release builds only", p.display()));
            }
        }
        Ok(b)
    };
    let (agent, launcher) = (read(MAC_AGENT)?, read(MAC_LAUNCHER)?);
    let ver = |b: &[u8], what: &str| -> Result<String, String> {
        let v = zerofarm_common::binver::exe_version_marker(b).ok_or(format!("{what} carries no version marker"))?;
        Ok(format!("{}.{}.{}", v.0, v.1, v.2))
    };
    let version = ver(&agent, MAC_AGENT)?;
    let launcher_version = ver(&launcher, MAC_LAUNCHER)?;
    let want = expected_keys(&opts.workspace, opts.allow_test_binaries)?;
    if !want.release.contains(&key.public_b64()) {
        return Err(format!("the signing key {} is not in common/trusted_keys.txt: these agents could never verify their own updates", key.key_id_hex()));
    }
    let union: BTreeSet<String> = want.release.union(&want.data).cloned().collect();
    let found = scan_keys(&agent);
    if found != union {
        return Err(format!(
            "{MAC_AGENT} does not trust exactly the keys in common/trusted_keys.txt and common/trusted_data_keys.txt (extra {:?}, missing {:?}); rebuild it without ZEROFARM_TRUSTED_KEYS / ZEROFARM_TRUSTED_DATA_KEYS set",
            found.difference(&union).collect::<Vec<_>>(),
            union.difference(&found).collect::<Vec<_>>()
        ));
    }
    let lfound = scan_keys(&launcher);
    if !want.release.is_subset(&lfound) || !lfound.is_subset(&union) {
        return Err(format!("{MAC_LAUNCHER} does not trust exactly the release keys (found {lfound:?})"));
    }
    let out = opts.out.clone().unwrap_or_else(|| opts.workspace.join("dist").join(format!("agent-macos-{version}")));
    if out.exists() {
        std::fs::remove_dir_all(&out).map_err(|e| e.to_string())?;
    }
    std::fs::create_dir_all(&out).map_err(|e| e.to_string())?;
    std::fs::write(out.join(MAC_AGENT), &agent).map_err(|e| e.to_string())?;
    std::fs::write(out.join(MAC_LAUNCHER), &launcher).map_err(|e| e.to_string())?;
    let (build_id, m) = sign(
        &out,
        &SignOpts { kind: Some(Kind::Agent), version: Some(version.clone()), min_agent: Some(min_agent.clone()), platform: Some(PLATFORM_MACOS_ARM64.into()), ..Default::default() },
        key,
    )?;
    let staging = github_staging_dir(&out);
    if staging.exists() {
        std::fs::remove_dir_all(&staging).map_err(|e| format!("{}: {e}", staging.display()))?;
    }
    std::fs::create_dir_all(&staging).map_err(|e| e.to_string())?;
    let file = |n: &str| std::fs::read(out.join(n)).map_err(|e| format!("{n}: {e}"));
    let entries = vec![
        (MAC_LAUNCHER.to_string(), file(MAC_LAUNCHER)?, 0o755),
        (MAC_AGENT.to_string(), file(MAC_AGENT)?, 0o755),
        (MANIFEST_FILE.to_string(), file(MANIFEST_FILE)?, 0o644),
        (SIG_FILE.to_string(), file(SIG_FILE)?, 0o644),
    ];
    let tarball = staging.join(MAC_TARBALL);
    std::fs::write(&tarball, tar_gz(&entries, m.created)?).map_err(|e| e.to_string())?;
    eprintln!("macOS agent {version} (launcher {launcher_version}) build_id {build_id}: {}", tarball.display());
    let client = match &opts.publish {
        Some((url, token)) => {
            let client = admin_client(url, token)?;
            publish(&out, &TrustedKeys::parse(&[key.public_b64()])?, &client)?;
            Some(client)
        }
        None => None,
    };
    let github = match &opts.github {
        Some(g) => Some(github_release_macos(&out, &tarball, key, &opts.channel, g)?),
        None => None,
    };
    if let Some(client) = &client {
        post_agent_release_for(client, &version, &build_id, &opts.channel, Some(&min_agent), github.as_ref().map(|g| g.installer_url.as_str()), Some(PLATFORM_MACOS_ARM64))?;
    }
    Ok(MacReleased { version, build_id, dir: out, tarball, github })
}

/// The GitHub Release `agent-macos-v<version>` (PROTOCOL 11.2): the tarball, `SHA256SUMS` (its line) and
/// `SHA256SUMS.minisig`; a prerelease for canary; never marked Latest (that stays the Windows release, where the
/// farm page's installer link points). A re-run uploads with `--clobber` and refreshes the title, notes and flags.
pub fn github_release_macos(package: &Path, tarball: &Path, key: &SigningKey, channel: &str, o: &GithubOpts) -> Result<GithubReleased, String> {
    check_channel(channel)?;
    o.check()?;
    let v = verify(package, &TrustedKeys::parse(&[key.public_b64()])?)?;
    if v.manifest.kind != Kind::Agent || v.manifest.platform.as_deref() != Some(zerofarm_common::manifest::PLATFORM_MACOS_ARM64) {
        return Err(format!("{} is not a macOS agent package", package.display()));
    }
    let version = v.manifest.version.clone();
    let tag = github_mac_tag(&version);
    let staging = tarball.parent().unwrap_or(package).to_path_buf();
    let sums = sha256sums(&[(MAC_TARBALL.to_string(), tarball.to_path_buf())])?;
    let sums_path = staging.join(SUMS_FILE);
    std::fs::write(&sums_path, &sums).map_err(|e| e.to_string())?;
    let sig = key.sign(sums.as_bytes(), &format!("file={SUMS_FILE} version={version} build_id={}", v.build_id))?;
    TrustedKeys::parse(&[key.public_b64()])?.verify(sums.as_bytes(), &sig).map_err(|e| format!("self-check of {SUMS_SIG_FILE} failed: {e}"))?;
    let sums_sig_path = staging.join(SUMS_SIG_FILE);
    std::fs::write(&sums_sig_path, sig).map_err(|e| e.to_string())?;
    let notes = match &o.notes_file {
        Some(f) => f.clone(),
        None => {
            let p = staging.join("notes.md");
            std::fs::write(&p, default_mac_notes(&o.repo, &version, channel, &v.build_id, &key.public_b64())).map_err(|e| e.to_string())?;
            p
        }
    };
    let title = format!("Zero Farm agent {version} for macOS (Apple silicon)");
    let prerelease = channel == "canary";
    let files: Vec<String> = [tarball.to_path_buf(), sums_path, sums_sig_path].iter().map(|p| p.to_string_lossy().into_owned()).collect();
    let notes_s = notes.to_string_lossy().into_owned();
    let view = gh(&o.gh, &["release", "view", &tag, "--repo", &o.repo, "--json", "tagName"])?;
    let updated = if view.status.success() {
        let mut a = vec!["release", "upload", tag.as_str()];
        a.extend(files.iter().map(String::as_str));
        a.extend(["--repo", &o.repo, "--clobber"]);
        gh_ok(&o.gh, &a)?;
        let pre = format!("--prerelease={prerelease}");
        gh_ok(&o.gh, &["release", "edit", tag.as_str(), "--repo", &o.repo, "--title", &title, "--notes-file", &notes_s, &pre, "--latest=false"])?;
        true
    } else if String::from_utf8_lossy(&view.stderr).to_ascii_lowercase().contains("not found") {
        let mut a = vec!["release", "create", tag.as_str()];
        a.extend(files.iter().map(String::as_str));
        a.extend(["--repo", &o.repo, "--title", &title, "--notes-file", &notes_s, "--latest=false"]);
        if prerelease {
            a.push("--prerelease");
        }
        gh_ok(&o.gh, &a)?;
        false
    } else {
        return Err(format!("gh release view {tag} failed: {}", String::from_utf8_lossy(&view.stderr).trim()));
    };
    Ok(GithubReleased { tag, installer_url: github_mac_tarball_url(&o.repo, &version), updated, staging })
}

fn default_mac_notes(repo: &str, version: &str, channel: &str, build_id: &str, pubkey: &str) -> String {
    let which = if channel == "canary" {
        "A **canary** (test) build: it goes to the owner's own test Macs first, and to everyone once it is promoted to stable."
    } else {
        "A **stable** build: every installed Zero Farm on a Mac updates to it by itself."
    };
    format!(
        "Zero Farm agent {version} for macOS on Apple silicon (macOS 13 or newer). {which}\n\n\
         **Install** (you need an invite code): `curl -fsSL https://farm.greggg230.com/install-macos.sh | sh -s -- --invite CODE`. \
         Installed copies update themselves; you don't need to download this.\n\n\
         **Verify** (with [minisign](https://jedisct1.github.io/minisign/)):\n\n\
         ```\n\
         minisign -Vm SHA256SUMS -P {pubkey}\n\
         shasum -a 256 -c SHA256SUMS\n\
         ```\n\n\
         The tarball holds `ZeroFarm` (the launcher) and the signed agent package (`farm-agent`, `manifest.json`, \
         `manifest.json.minisig`: `minisign -Vm manifest.json -P {pubkey}`). Agent package build id: `{build_id}`. \
         See [CHANGELOG.md](https://github.com/{repo}/blob/main/CHANGELOG.md) for what changed.\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tar_gz_round_trips_and_the_system_tar_reads_it() {
        let entries = vec![
            (MAC_LAUNCHER.to_string(), b"launcher bytes".to_vec(), 0o755),
            (MAC_AGENT.to_string(), vec![42u8; 70_000], 0o755),
            (MANIFEST_FILE.to_string(), b"{}\n".to_vec(), 0o644),
        ];
        let gz = tar_gz(&entries, 1_794_300_000).unwrap();
        assert_eq!(untar_gz(&gz).unwrap(), entries);
        assert!(tar_gz(&[("a/b".into(), vec![], 0o644)], 0).is_err(), "flat names only");
        // an independent reader: bsdtar (macOS /usr/bin/tar, Windows System32\tar.exe), with the modes as written
        let tar = if cfg!(windows) { std::path::PathBuf::from(r"C:\Windows\System32\tar.exe") } else { std::path::PathBuf::from("/usr/bin/tar") };
        if !tar.exists() {
            return;
        }
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join(MAC_TARBALL);
        std::fs::write(&f, &gz).unwrap();
        let x = d.path().join("x");
        std::fs::create_dir_all(&x).unwrap();
        let st = Command::new(&tar).arg("-xzf").arg(&f).arg("-C").arg(&x).status().unwrap();
        assert!(st.success());
        for (n, b, _mode) in &entries {
            assert_eq!(&std::fs::read(x.join(n)).unwrap(), b, "{n}");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                assert_eq!(std::fs::metadata(x.join(n)).unwrap().permissions().mode() & 0o777, *_mode, "{n}");
            }
        }
    }

    #[test]
    fn keys_and_mach_o_are_recognised_in_binary_bytes() {
        let k = zerofarm_common::sign::SigningKey::generate().unwrap().public_b64();
        let mut b = vec![0xcf, 0xfa, 0xed, 0xfe, 0x0c, 0, 0, 1, 0, 0, 0, 0, 2, 0, 0, 0];
        b.extend_from_slice(b"RWnotakeyRWxyz");
        b.extend_from_slice(k.as_bytes());
        b.extend_from_slice(b"RWRW");
        assert!(is_macho_arm64_exe(&b));
        assert_eq!(scan_keys(&b), BTreeSet::from([k]));
        assert!(!is_macho_arm64_exe(b"MZ\x90\x00"));
    }
}

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
//!   outlives this PC's DPAPI) and imported again on another PC.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Command;
use zerofarm_common::http::{with_retry, Backoff, Client};
use zerofarm_common::keys::TrustedKeys;
use zerofarm_common::manifest::{verify_dir, Expect, Kind, Manifest, VerifiedBuild, MANIFEST_FILE, SIG_FILE};
use zerofarm_common::sign::{sign_dir, template, SigningKey};

pub const KEY_FORMAT: &str = "zero-farm-release-key/1";
pub const KEY_PURPOSE: &str = "zero-farm release key";

/// Debug-only names that must never appear in a shipped agent or launcher.
pub const TEST_MARKERS: &[&str] = &[
    "ZEROFARM_TEST_TRUSTED_KEYS",
    "ZEROFARM_TEST_HEADLESS",
    "ZEROFARM_TEST_HEALTHY_TIMEOUT_S",
    "ZEROFARM_TEST_TAKEOVER_CONFIRM_S",
    "ZEROFARM_TEST_HANDOVER_S",
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
}

fn is_full_sha(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|c| c.is_ascii_hexdigit())
}

/// Builds the manifest for `dir` from `opts` (with the defaults and checks per kind), signs it.
pub fn sign(dir: &Path, opts: &SignOpts, key: &SigningKey) -> Result<(String, Manifest), String> {
    let kind = opts.kind.ok_or("--kind worker|agent|runtime is required")?;
    // Re-signing: the old manifest/signature are not part of the file list anyway.
    let mut t = template(kind, "");
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
                None => single_exe(dir)?,
            });
        }
        Kind::Agent => {
            let v = opts.version.clone().ok_or("agent builds need --version (semver)")?;
            if semver_ok(&v).is_none() {
                return Err(format!("agent version {v:?} is not semver"));
            }
            t.version = v;
            t.entry = Some(opts.entry.clone().unwrap_or_else(|| "farm-agent.exe".into()));
            t.min_agent = opts.min_agent.clone();
        }
        Kind::Runtime => {
            t.version = opts.version.clone().ok_or("runtime packs need --version")?;
            t.min_agent = opts.min_agent.clone();
            t.entry = opts.entry.clone();
        }
    }
    if let Some(r) = &t.requires_runtime {
        if !zerofarm_common::is_build_id(r) {
            return Err(format!("--requires-runtime {r:?} is not a build id"));
        }
    }
    let (id, m) = sign_dir(dir, t, key)?;
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

fn single_exe(dir: &Path) -> Result<String, String> {
    let exes: Vec<String> = std::fs::read_dir(dir)
        .map_err(|e| e.to_string())?
        .flatten()
        .filter_map(|e| e.file_name().to_str().map(String::from))
        .filter(|n| n.to_ascii_lowercase().ends_with(".exe"))
        .collect();
    match exes.as_slice() {
        [one] => Ok(one.clone()),
        _ => Err(format!("give --entry: {} .exe files in {}", exes.len(), dir.display())),
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
    let mut backoff = Backoff::default();
    let never = || false;
    let mut put = |rel: &str| -> Result<(), String> {
        let bytes = std::fs::read(dir.join(rel.replace('/', "\\"))).map_err(|e| format!("{rel}: {e}"))?;
        let path = format!("/v1/admin/artifact/builds/{id}/{rel}");
        with_retry(&mut backoff, Some(6), &never, || client.put_bytes(&path, &bytes, &[])).map_err(|e| format!("PUT {rel}: {e}"))?;
        Ok(())
    };
    for f in &v.manifest.files {
        put(&f.path.replace('\\', "/"))?;
    }
    put(MANIFEST_FILE)?;
    put(SIG_FILE)?;
    with_retry(&mut Backoff::default(), Some(6), &never, || {
        client.post_json::<_, serde_json::Value>("/v1/admin/build", &serde_json::json!({"build_id": id}))
    })
    .map_err(|e| format!("POST /v1/admin/build: {e}"))?;
    Ok(id)
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
    let trusted = run_capture(&agent, &["keys"])?;
    if !trusted.lines().any(|l| l.split_whitespace().nth(1) == Some(key.public_b64().as_str())) {
        return Err(format!(
            "this farm-agent.exe does not trust the signing key {} — it could never verify its own updates. \
             Add the public key to common/trusted_keys.txt (or ZEROFARM_TRUSTED_KEYS) and rebuild.",
            key.key_id_hex()
        ));
    }
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
    let body = serde_json::json!({"version": version, "build_id": build_id, "channel": channel,
                                  "min_agent": min_agent, "github_url": github_url});
    with_retry(&mut Backoff::default(), Some(6), &|| false, || client.post_json::<_, serde_json::Value>("/v1/admin/agent/release", &body))
        .map_err(|e| format!("POST /v1/admin/agent/release: {e}"))?;
    Ok(())
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

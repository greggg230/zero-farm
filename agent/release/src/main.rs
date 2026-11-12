//! farm-release.exe (desktop only)
//!
//! ```text
//! farm-release keygen --out FILE [--data]
//! farm-release export-key --out FILE [--key FILE]        passphrase asked twice on the console
//! farm-release import-key --from FILE --out FILE         passphrase asked on the console
//! farm-release sign DIR --kind worker|agent|runtime [--key FILE] [--version V] [--game G]
//!              [--engine-commit SHA] [--core-commit SHA] [--profile release|fuzz] [--aw 1]
//!              [--zero-onnx 1] [--min-agent V] [--requires-runtime BUILD_ID] [--entry FILE]
//! farm-release verify DIR [--pubkey B64 | --key FILE]
//! farm-release publish DIR --url URL [--token T] [--pubkey B64 | --key FILE]
//! farm-release release-agent [--url URL] [--token T] [--key FILE] [--channel canary|stable]
//!              [--min-agent V] [--workspace DIR] [--bin-dir DIR] [--out DIR] [--no-publish]
//!              [--setup | --setup-exe FILE] [--github OWNER/REPO [--notes-file F] [--gh PATH]]
//! farm-release github-release DIR --github OWNER/REPO [--channel canary|stable] [--key FILE]
//!              [--setup-exe FILE] [--notes-file F] [--gh PATH]
//!              [--url URL [--token T] [--min-agent V]]
//! farm-release define DEFINITION.json --key DATA_KEY_FILE [--publish --url URL [--token T]]
//! ```
//!
//! `define` (PROTOCOL 7) validates a `definition/1` file, signs its bytes as they are with a data
//! key (never the release key; the key must be in `common/trusted_data_keys.txt`), writes
//! `DEFINITION.json.minisig` next to it, and with `--publish` uploads both to
//! `defs/<kind>/<id>/<version>/` and registers the definition (`PUT /v1/admin/definition`). It
//! prints one JSON line: `{"kind", "id", "version", "key", "sha256", "signature", "published"}`.
//! Exit code 0 on success, 1 with `error: ...` on stderr otherwise. Other repositories' tools
//! call it; keep this interface stable. The data key comes from `--key` or `ZEROFARM_DATA_KEY`
//! (never the release key's default path). `keygen --data` makes one (same file format).
//!
//! The admin token comes from `--token` or `ZEROFARM_ADMIN_TOKEN`; the key file from `--key`,
//! `ZEROFARM_RELEASE_KEY`, or `%USERPROFILE%\.zero-farm\release.key`.
//!
//! `--setup` builds `ZeroFarmSetup.exe` with the signed package inside (`--setup-exe` takes an
//! already-built one). `--github` also makes the GitHub Release `agent-v<version>` (installer,
//! package, `SHA256SUMS` + signature; a prerelease for canary) through the `gh` CLI, and the
//! coordinator release then names the installer's GitHub URL, where `GET /download` redirects.
//! `--github` implies `--setup`.
//!
//! `github-release` does the GitHub part for an already-built package directory (`dist/agent-<v>`;
//! the installer defaults to the copy kept in `dist/agent-<v>-github/` by an earlier run, else
//! `dist/ZeroFarmSetup.exe`, and must embed exactly that package), and
//! with `--url` also publishes the build (idempotent) and points the channel at the release.
//!
//! `export-key` writes the release key as a passphrase-encrypted minisign secret key file (the
//! `minisign` CLI can sign with it): keep that file and its passphrase in a password manager.
//! `import-key` turns such a file back into a DPAPI key file for the current Windows user.
//! Passphrases are only ever read from the console, never from arguments.

use farm_release::*;
use std::path::PathBuf;
use zerofarm_common::manifest::Kind;

struct Args {
    pos: Vec<String>,
    flags: Vec<(String, Option<String>)>,
}

impl Args {
    fn parse() -> Args {
        let mut pos = Vec::new();
        let mut flags = Vec::new();
        let raw: Vec<String> = std::env::args().skip(1).collect();
        let mut it = raw.into_iter().peekable();
        while let Some(a) = it.next() {
            if let Some(n) = a.strip_prefix("--") {
                let v = match it.peek() {
                    Some(v) if !v.starts_with("--") => it.next(),
                    _ => None,
                };
                flags.push((n.to_string(), v));
            } else {
                pos.push(a);
            }
        }
        Args { pos, flags }
    }
    fn get(&self, n: &str) -> Option<String> {
        self.flags.iter().find(|(k, _)| k == n).and_then(|(_, v)| v.clone())
    }
    fn has(&self, n: &str) -> bool {
        self.flags.iter().any(|(k, _)| k == n)
    }
    fn key_path(&self) -> PathBuf {
        self.get("key").map(PathBuf::from).unwrap_or_else(default_key_path)
    }
    fn token(&self) -> Result<String, String> {
        self.get("token").or_else(|| std::env::var("ZEROFARM_ADMIN_TOKEN").ok()).ok_or_else(|| "--token or ZEROFARM_ADMIN_TOKEN is required".into())
    }
    fn dir(&self) -> Result<PathBuf, String> {
        self.pos.get(1).map(PathBuf::from).ok_or_else(|| "a directory argument is required".into())
    }
    fn github(&self) -> Result<Option<GithubOpts>, String> {
        Ok(match self.get("github") {
            Some(repo) => {
                let mut g = GithubOpts::new(&repo);
                g.notes_file = self.get("notes-file").map(PathBuf::from);
                if let Some(gh) = self.get("gh") {
                    g.gh = PathBuf::from(gh);
                }
                Some(g)
            }
            None if self.has("github") => return Err("--github OWNER/REPO".into()),
            None => None,
        })
    }
}

/// Reads a passphrase from the console without echo. Never from arguments or the environment.
fn console_passphrase(prompt: &str) -> Result<String, String> {
    rpassword::prompt_password(prompt).map_err(|e| format!("cannot read a passphrase from the console: {e}"))
}

fn refuse_passphrase_args(a: &Args) -> Result<(), String> {
    for f in ["passphrase", "password", "pass"] {
        if a.has(f) {
            return Err(format!("--{f}: the passphrase is never taken from the command line; it is asked for on the console"));
        }
    }
    Ok(())
}

fn run(a: &Args) -> Result<(), String> {
    match a.pos.first().map(String::as_str) {
        Some("keygen") => {
            let out = a.get("out").map(PathBuf::from).ok_or("--out FILE is required")?;
            let k = keygen(&out)?;
            println!("wrote {} (DPAPI-encrypted for this Windows user) and {}", out.display(), pub_path(&out).display());
            println!("key id {}", k.key_id_hex());
            if a.has("data") {
                println!(
                    "a DATA key: it signs definitions only (farm-release define). Add the public key to \
                     agent/common/trusted_data_keys.txt, NOT trusted_keys.txt (a key on both lists fails the build):\n{}",
                    k.public_b64()
                );
            } else {
                println!("a RELEASE key: it signs builds and agent releases. Add the public key to agent/common/trusted_keys.txt:\n{}", k.public_b64());
            }
            Ok(())
        }
        Some("export-key") => {
            refuse_passphrase_args(a)?;
            let out = a.get("out").map(PathBuf::from).ok_or("--out FILE is required")?;
            let k = export_key(&a.key_path(), &out, &mut console_passphrase)?;
            println!("wrote {} (minisign secret key, encrypted with your passphrase)", out.display());
            println!("key id {}", k.key_id_hex());
            println!("public key {}", k.public_b64());
            println!("Keep the file and its passphrase in a password manager; `farm-release import-key` restores it.");
            Ok(())
        }
        Some("import-key") => {
            refuse_passphrase_args(a)?;
            let from = a.get("from").map(PathBuf::from).ok_or("--from FILE is required")?;
            let out = a.get("out").map(PathBuf::from).ok_or("--out FILE is required")?;
            let k = import_key(&from, &out, &mut console_passphrase)?;
            println!("wrote {} (DPAPI-encrypted for this Windows user) and {}", out.display(), pub_path(&out).display());
            println!("key id {}", k.key_id_hex());
            println!("public key {}", k.public_b64());
            Ok(())
        }
        Some("sign") => {
            let dir = a.dir()?;
            let key = load_key(&a.key_path())?;
            let kind = a.get("kind").and_then(|k| Kind::parse(&k)).ok_or("--kind worker|agent|runtime is required")?;
            let opts = SignOpts {
                kind: Some(kind),
                version: a.get("version"),
                game: a.get("game"),
                engine_commit: a.get("engine-commit"),
                core_commit: a.get("core-commit"),
                profile: a.get("profile"),
                aw: a.get("aw").map(|v| v.parse().map_err(|_| "--aw must be a number")).transpose()?,
                zero_onnx: a.get("zero-onnx").map(|v| v.split(',').filter_map(|x| x.trim().parse().ok()).collect()),
                min_agent: a.get("min-agent"),
                requires_runtime: a.get("requires-runtime"),
                entry: a.get("entry"),
            };
            let (id, m) = sign(&dir, &opts, &key)?;
            println!("signed {} {} ({} files) build_id {id}", m.kind, m.version, m.files.len());
            Ok(())
        }
        Some("verify") => {
            let dir = a.dir()?;
            let keys = keys_for(a.get("pubkey").as_deref(), Some(&a.key_path()))?;
            let v = verify(&dir, &keys)?;
            println!("OK {} {} build_id {} ({} files)", v.manifest.kind, v.manifest.version, v.build_id, v.manifest.files.len());
            Ok(())
        }
        Some("publish") => {
            let dir = a.dir()?;
            let url = a.get("url").ok_or("--url is required")?;
            let keys = keys_for(a.get("pubkey").as_deref(), Some(&a.key_path()))?;
            let id = publish(&dir, &keys, &admin_client(&url, &a.token()?)?)?;
            println!("published build {id}");
            Ok(())
        }
        Some("release-agent") => {
            let key = load_key(&a.key_path())?;
            let workspace = a.get("workspace").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".."));
            let publish = if a.has("no-publish") {
                None
            } else {
                Some((a.get("url").ok_or("--url is required (or --no-publish)")?, a.token()?))
            };
            // The coordinator refuses an agent release without min_agent (PROTOCOL 4), so a
            // publishing release must say it; fail here, before building and signing anything.
            if publish.is_some() && a.get("min-agent").is_none() {
                return Err("--min-agent is required when publishing an agent release (the oldest agent that may keep taking work)".into());
            }
            let github = a.github()?;
            let opts = ReleaseAgentOpts {
                workspace: workspace.clone(),
                bin_dir: a.get("bin-dir").map(PathBuf::from),
                out: a.get("out").map(PathBuf::from),
                channel: a.get("channel").unwrap_or_else(|| "canary".into()),
                min_agent: a.get("min-agent"),
                publish,
                allow_test_binaries: a.has("allow-test-binaries"),
                setup: match (a.has("setup") || github.is_some(), a.get("setup-exe")) {
                    (_, Some(p)) => Setup::Prebuilt(PathBuf::from(p)),
                    (true, None) => Setup::Build,
                    (false, None) => Setup::None,
                },
                github,
            };
            let r = release_agent(&opts, &key)?;
            println!("agent {} build_id {} in {}", r.version, r.build_id, r.dir.display());
            if let Some(s) = &r.setup {
                println!("installer: {}", s.display());
            }
            if let Some(g) = &r.github {
                println!("GitHub release {} {}; installer {}", g.tag, if g.updated { "updated" } else { "created" }, g.installer_url);
            }
            if opts.publish.is_some() {
                println!(
                    "published and released on channel {}{}",
                    opts.channel,
                    if r.github.is_some() { ", GET /download -> GitHub" } else { "" }
                );
            }
            Ok(())
        }
        Some("github-release") => {
            let dir = a.dir()?;
            let g = a.github()?.ok_or("--github OWNER/REPO is required")?;
            let key = load_key(&a.key_path())?;
            let channel = a.get("channel").unwrap_or_else(|| "canary".into());
            // Default: the copy an earlier github-release kept for this version, else the newest
            // installer in dist/ (it must embed this package; checked below).
            let setup = a.get("setup-exe").map(PathBuf::from).unwrap_or_else(|| {
                let kept = github_staging_dir(&dir).join(SETUP_FILE);
                if kept.is_file() {
                    kept
                } else {
                    dir.with_file_name(SETUP_FILE)
                }
            });
            // Fail on a missing token before anything goes to GitHub.
            let client = match a.get("url") {
                Some(url) => Some(admin_client(&url, &a.token()?)?),
                None => None,
            };
            let r = github_release(&dir, &setup, &key, &channel, &g, true)?;
            println!("GitHub release {} {}; installer {}", r.tag, if r.updated { "updated" } else { "created" }, r.installer_url);
            if let Some(client) = &client {
                let keys = zerofarm_common::keys::TrustedKeys::parse(&[key.public_b64()])?;
                let v = verify(&dir, &keys)?;
                let min_agent = a.get("min-agent").or(v.manifest.min_agent.clone()).ok_or("--min-agent is required (the manifest has none)")?;
                publish(&dir, &keys, client)?;
                post_agent_release(client, &v.manifest.version, &v.build_id, &channel, Some(&min_agent), Some(&r.installer_url))?;
                println!("released {} on channel {channel}; GET /download -> {}", v.manifest.version, r.installer_url);
            } else {
                println!("the coordinator was not told (no --url): GET /download does not point here yet");
            }
            Ok(())
        }
        Some("define") => {
            let file = a.pos.get(1).map(PathBuf::from).ok_or("usage: farm-release define DEFINITION.json --key DATA_KEY_FILE [--publish --url URL --token T]")?;
            let key_path = a
                .get("key")
                .or_else(|| std::env::var("ZEROFARM_DATA_KEY").ok())
                .map(PathBuf::from)
                .ok_or("--key DATA_KEY_FILE (or ZEROFARM_DATA_KEY) is required: definitions are signed with a data key")?;
            let client = if a.has("publish") {
                let url = a.get("url").ok_or("--publish needs --url")?;
                Some(admin_client(&url, &a.token()?)?)
            } else if a.has("url") {
                return Err("--url without --publish: add --publish to upload and register the definition".into());
            } else {
                None
            };
            let key = load_key(&key_path)?;
            let d = define(&file, &key, client.as_ref())?;
            println!("{}", serde_json::to_string(&d).map_err(|e| e.to_string())?);
            Ok(())
        }
        _ => Err("usage: farm-release keygen|export-key|import-key|sign|verify|publish|release-agent|github-release|define (see the source header)".into()),
    }
}

fn main() {
    let a = Args::parse();
    if let Err(e) = run(&a) {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

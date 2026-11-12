//! farm-release: keygen (DPAPI), sign (minisign-compatible), verify, publish to the mock
//! coordinator, release-agent with prebuilt binaries (exact trusted-key sets), define (signed
//! definitions, PROTOCOL 7). Temporary keys only.

use mockcoord::testkit::{bin, TEST_DATA_KEYS_ENV, TEST_KEYS_ENV};
use mockcoord::MockCoord;
use std::path::Path;
use std::process::{Command, Output};
use zerofarm_common::keys::TrustedKeys;
use zerofarm_common::manifest::{Kind, Manifest, MANIFEST_FILE, SIG_FILE};

const EC: &str = "60397f3aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const CC: &str = "f9ca5c3bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

fn cli(args: &[&str], env: &[(&str, &str)]) -> Output {
    let mut c = Command::new(env!("CARGO_BIN_EXE_farm-release"));
    c.args(args);
    for (k, v) in env {
        c.env(k, v);
    }
    c.output().unwrap()
}

fn text(o: &Output) -> String {
    format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr))
}

fn worker_dir(root: &Path) -> std::path::PathBuf {
    let d = root.join("worker");
    std::fs::create_dir_all(&d).unwrap();
    std::fs::write(d.join("mtgz-worker.exe"), b"MZ worker bytes").unwrap();
    std::fs::write(d.join("mimalloc.dll"), vec![9u8; 5000]).unwrap();
    d
}

#[test]
fn keygen_writes_a_dpapi_key_and_minisign_pub_and_never_overwrites() {
    let t = tempfile::tempdir().unwrap();
    let k = t.path().join("keys").join("release.key");
    let o = cli(&["keygen", "--out", k.to_str().unwrap()], &[]);
    assert!(o.status.success(), "{}", text(&o));
    let raw = std::fs::read_to_string(&k).unwrap();
    assert!(!raw.contains("untrusted comment"), "secret box must be DPAPI-encrypted");
    let kf: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(kf["format"], "zero-farm-release-key/1");
    let pubfile = std::fs::read_to_string(farm_release::pub_path(&k)).unwrap();
    minisign_verify::PublicKey::decode(&pubfile).unwrap();
    assert!(text(&o).contains(kf["public"].as_str().unwrap()), "prints the public key");
    let key = farm_release::load_key(&k).unwrap();
    assert_eq!(key.public_b64(), kf["public"].as_str().unwrap());
    let again = cli(&["keygen", "--out", k.to_str().unwrap()], &[]);
    assert!(!again.status.success());
    assert!(text(&again).contains("refusing to overwrite"));
}

#[test]
fn sign_worker_verify_and_detect_tampering() {
    let t = tempfile::tempdir().unwrap();
    let k = t.path().join("release.key");
    farm_release::keygen(&k).unwrap();
    let d = worker_dir(t.path());
    let o = cli(
        &["sign", d.to_str().unwrap(), "--kind", "worker", "--key", k.to_str().unwrap(), "--game", "mtg", "--engine-commit", EC, "--core-commit", CC, "--profile", "fuzz", "--min-agent", "0.1.0"],
        &[],
    );
    assert!(o.status.success(), "{}", text(&o));
    let m = Manifest::parse(&std::fs::read(d.join(MANIFEST_FILE)).unwrap()).unwrap();
    assert_eq!(m.kind, Kind::Worker);
    assert_eq!(m.version, "mtg-60397f3");
    assert_eq!(m.game.as_deref(), Some("mtg"));
    assert_eq!(m.engine_commit.as_deref(), Some(EC));
    assert_eq!(m.profile.as_deref(), Some("fuzz"));
    assert_eq!(m.aw, Some(1));
    assert_eq!(m.zero_onnx, Some(vec![1]));
    assert_eq!(m.entry.as_deref(), Some("mtgz-worker.exe"));
    assert_eq!(m.files.len(), 2);
    // What `minisign -Vm manifest.json -p release.key.pub` checks: the prehashed signature and the
    // global signature over the trusted comment, with the minisign.pub file.
    let pubfile = std::fs::read_to_string(farm_release::pub_path(&k)).unwrap();
    let pk = minisign_verify::PublicKey::decode(&pubfile).unwrap();
    let sig = minisign_verify::Signature::decode(&std::fs::read_to_string(d.join(SIG_FILE)).unwrap()).unwrap();
    pk.verify(&std::fs::read(d.join(MANIFEST_FILE)).unwrap(), &sig, false).unwrap();
    assert!(sig.trusted_comment().starts_with("build_id="));
    let v = cli(&["verify", d.to_str().unwrap(), "--key", k.to_str().unwrap()], &[]);
    assert!(v.status.success(), "{}", text(&v));
    assert!(text(&v).contains("OK worker mtg-60397f3"));
    std::fs::write(d.join("mtgz-worker.exe"), b"MZ evil!! bytes").unwrap(); // same length: a content change
    let v = cli(&["verify", d.to_str().unwrap(), "--key", k.to_str().unwrap()], &[]);
    assert!(!v.status.success());
    assert!(text(&v).contains("sha256 mismatch"), "{}", text(&v));
}

#[test]
fn sign_refuses_incomplete_manifests() {
    let t = tempfile::tempdir().unwrap();
    let k = t.path().join("release.key");
    farm_release::keygen(&k).unwrap();
    let d = worker_dir(t.path());
    let o = cli(&["sign", d.to_str().unwrap(), "--kind", "worker", "--key", k.to_str().unwrap()], &[]);
    assert!(text(&o).contains("--game"), "{}", text(&o));
    let o = cli(&["sign", d.to_str().unwrap(), "--kind", "worker", "--key", k.to_str().unwrap(), "--game", "mtg", "--engine-commit", "60397f3", "--core-commit", CC], &[]);
    assert!(text(&o).contains("full 40-hex"));
    let o = cli(&["sign", d.to_str().unwrap(), "--kind", "agent", "--key", k.to_str().unwrap(), "--version", "v1"], &[]);
    assert!(text(&o).contains("not semver"));
    assert!(!d.join(MANIFEST_FILE).exists());
}

#[test]
fn publish_uploads_files_then_registers_the_build() {
    let t = tempfile::tempdir().unwrap();
    let k = t.path().join("release.key");
    let key = farm_release::keygen(&k).unwrap();
    let d = worker_dir(t.path());
    let (id, _) = farm_release::sign(
        &d,
        &farm_release::SignOpts { kind: Some(Kind::Worker), game: Some("mtg".into()), engine_commit: Some(EC.into()), core_commit: Some(CC.into()), ..Default::default() },
        &key,
    )
    .unwrap();
    let mock = MockCoord::start();
    mock.trust(&key.public_b64());
    let o = cli(&["publish", d.to_str().unwrap(), "--url", &mock.url, "--key", k.to_str().unwrap()], &[("ZEROFARM_ADMIN_TOKEN", "admin-secret")]);
    assert!(o.status.success(), "{}", text(&o));
    assert_eq!(mock.with(|s| s.builds_registered.clone()), vec![id.clone()]);
    for f in ["mtgz-worker.exe", "mimalloc.dll", MANIFEST_FILE, SIG_FILE] {
        assert!(mock.with(|s| s.artifacts.contains_key(&format!("builds/{id}/{f}"))), "{f}");
    }
    // The manifest and signature go up last (after every file).
    let order: Vec<String> = mock.with(|s| s.log.iter().filter(|l| l.method == "PUT").map(|l| l.path.clone()).collect());
    assert!(order[order.len() - 2].ends_with(MANIFEST_FILE) && order[order.len() - 1].ends_with(SIG_FILE), "{order:?}");
    // Wrong admin token: refused, loudly.
    let o = cli(&["publish", d.to_str().unwrap(), "--url", &mock.url, "--key", k.to_str().unwrap(), "--token", "nope"], &[]);
    assert!(!o.status.success());
    assert!(text(&o).contains("401"));
}

#[test]
fn publish_refuses_a_build_this_key_did_not_sign() {
    let t = tempfile::tempdir().unwrap();
    let k = t.path().join("release.key");
    farm_release::keygen(&k).unwrap();
    let other = zerofarm_common::sign::SigningKey::generate().unwrap();
    let d = worker_dir(t.path());
    farm_release::sign(
        &d,
        &farm_release::SignOpts { kind: Some(Kind::Worker), game: Some("mtg".into()), engine_commit: Some(EC.into()), core_commit: Some(CC.into()), ..Default::default() },
        &other,
    )
    .unwrap();
    let mock = MockCoord::start();
    let o = cli(&["publish", d.to_str().unwrap(), "--url", &mock.url, "--key", k.to_str().unwrap(), "--token", "admin-secret"], &[]);
    assert!(!o.status.success());
    assert!(text(&o).contains("unknown key"));
    assert_eq!(mock.count("PUT", "/"), 0, "nothing uploaded");
}

/// The farm-agent crate's version (what release-agent reads from the binary), so a version bump
/// for a release doesn't break this test.
fn agent_crate_version() -> String {
    let toml = include_str!("../../agent/Cargo.toml");
    toml.lines()
        .find_map(|l| l.trim().strip_prefix("version = \"").and_then(|r| r.strip_suffix('"')))
        .expect("version line in agent/Cargo.toml")
        .to_string()
}

fn release_args<'a>(k: &'a str, out: &'a str, url: &'a str, bins: &'a str) -> Vec<&'a str> {
    vec!["release-agent", "--key", k, "--bin-dir", bins, "--out", out, "--url", url, "--token", "admin-secret", "--channel", "canary", "--min-agent", "0.1.0"]
}

#[test]
fn release_agent_signs_publishes_and_sets_latest() {
    let t = tempfile::tempdir().unwrap();
    let k = t.path().join("release.key");
    let key = farm_release::keygen(&k).unwrap();
    let mock = MockCoord::start();
    mock.trust(&key.public_b64());
    // Prebuilt (debug) binaries from this workspace; the debug agent trusts the key through the
    // test-only variable, which release builds don't have.
    let bins = bin("farm-agent").parent().unwrap().to_path_buf();
    let out = t.path().join("dist");
    let mut args = release_args(k.to_str().unwrap(), out.to_str().unwrap(), &mock.url, bins.to_str().unwrap());
    args.push("--allow-test-binaries");
    let o = cli(&args, &[(TEST_KEYS_ENV, &key.public_b64())]);
    assert!(o.status.success(), "{}", text(&o));
    let rel = mock.with(|s| s.agent_releases.clone());
    assert_eq!(rel.len(), 1);
    assert_eq!(rel[0]["version"], agent_crate_version().as_str());
    assert_eq!(rel[0]["channel"], "canary");
    let id = rel[0]["build_id"].as_str().unwrap().to_string();
    assert_eq!(mock.with(|s| s.builds_registered.clone()), vec![id.clone()]);
    let v = farm_release::verify(&out, &TrustedKeys::parse(&[key.public_b64()]).unwrap()).unwrap();
    assert_eq!(v.build_id, id);
    assert_eq!(v.manifest.kind, Kind::Agent);
    assert_eq!(v.manifest.entry.as_deref(), Some("farm-agent.exe"));
    assert!(v.manifest.files.iter().any(|f| f.path == "ZeroFarm.exe"));
    assert_eq!(mock.with(|s| s.latest.clone()).unwrap()["build_id"], id.as_str());
    assert!(rel[0]["github_url"].is_null(), "no GitHub release asked for");
}

// ---------- GitHub Releases (through fakegh, a stand-in for the gh CLI) ----------

const REPO: &str = "greggg230/zero-farm";
const ASSETS: [&str; 7] =
    ["SHA256SUMS", "SHA256SUMS.minisig", "ZeroFarm.exe", "ZeroFarmSetup.exe", "farm-agent.exe", "manifest.json", "manifest.json.minisig"];

fn gh_calls(gh_dir: &Path) -> Vec<Vec<String>> {
    match std::fs::read_to_string(gh_dir.join("calls.jsonl")) {
        Ok(s) => s.lines().map(|l| serde_json::from_str(l).unwrap()).collect(),
        Err(_) => vec![],
    }
}

fn gh_release(gh_dir: &Path, tag: &str) -> serde_json::Value {
    serde_json::from_str(&std::fs::read_to_string(gh_dir.join("releases").join(tag).join("release.json")).unwrap()).unwrap()
}

fn gh_assets(gh_dir: &Path, tag: &str) -> std::collections::BTreeMap<String, Vec<u8>> {
    std::fs::read_dir(gh_dir.join("releases").join(tag).join("assets"))
        .unwrap()
        .flatten()
        .map(|e| (e.file_name().to_string_lossy().into_owned(), std::fs::read(e.path()).unwrap()))
        .collect()
}

/// The release's SHA256SUMS lists every other asset with its hash, and its signature verifies
/// with the minisign public key file (what `minisign -Vm SHA256SUMS -p release.key.pub` checks).
fn check_sums(assets: &std::collections::BTreeMap<String, Vec<u8>>, pubfile: &str, version: &str) {
    let names: Vec<&str> = assets.keys().map(String::as_str).collect();
    assert_eq!(names, ASSETS.to_vec());
    let sums = String::from_utf8(assets["SHA256SUMS"].clone()).unwrap();
    assert!(!sums.contains('\r'));
    let mut listed = vec![];
    for line in sums.lines() {
        let (sha, name) = line.split_once("  ").unwrap();
        assert_eq!(sha, zerofarm_common::hash::sha256_hex(&assets[name]), "{name}");
        listed.push(name);
    }
    assert_eq!(listed, vec!["ZeroFarm.exe", "ZeroFarmSetup.exe", "farm-agent.exe", "manifest.json", "manifest.json.minisig"]);
    let pk = minisign_verify::PublicKey::decode(pubfile).unwrap();
    let sig = minisign_verify::Signature::decode(std::str::from_utf8(&assets["SHA256SUMS.minisig"]).unwrap()).unwrap();
    pk.verify(sums.as_bytes(), &sig, false).unwrap();
    assert!(sig.trusted_comment().starts_with(&format!("file=SHA256SUMS version={version} build_id=")), "{}", sig.trusted_comment());
    // And the package itself verifies as an agent would (`minisign -Vm manifest.json`).
    let msig = minisign_verify::Signature::decode(std::str::from_utf8(&assets["manifest.json.minisig"]).unwrap()).unwrap();
    pk.verify(&assets["manifest.json"], &msig, false).unwrap();
}

#[test]
fn release_agent_with_github_makes_the_release_then_points_download_at_it() {
    let t = tempfile::tempdir().unwrap();
    let k = t.path().join("release.key");
    let key = farm_release::keygen(&k).unwrap();
    let mock = MockCoord::start();
    mock.trust(&key.public_b64());
    let bins = bin("farm-agent").parent().unwrap().to_path_buf();
    let out = t.path().join("dist").join("agent");
    // An already-built installer (building one runs cargo; real releases do that with --setup).
    let setup = t.path().join("prebuilt-setup.exe");
    std::fs::write(&setup, b"MZ fake installer bytes").unwrap();
    let gh_dir = t.path().join("gh");
    let fakegh = bin("fakegh");
    let mut args = release_args(k.to_str().unwrap(), out.to_str().unwrap(), &mock.url, bins.to_str().unwrap());
    args.extend(["--allow-test-binaries", "--setup-exe", setup.to_str().unwrap(), "--github", REPO, "--gh", fakegh.to_str().unwrap()]);
    let o = cli(&args, &[(TEST_KEYS_ENV, &key.public_b64()), ("FAKE_GH_DIR", gh_dir.to_str().unwrap())]);
    assert!(o.status.success(), "{}", text(&o));
    let v = agent_crate_version();
    let tag = format!("agent-v{v}");
    let url = format!("https://github.com/{REPO}/releases/download/{tag}/ZeroFarmSetup.exe");
    assert!(text(&o).contains(&url), "{}", text(&o));

    // GitHub: a prerelease (canary) with the installer, the package, SHA256SUMS and its signature.
    let calls = gh_calls(&gh_dir);
    assert_eq!(calls.iter().map(|c| c[1].as_str()).collect::<Vec<_>>(), vec!["view", "create"], "{calls:?}");
    let rel = gh_release(&gh_dir, &tag);
    assert_eq!(rel["repo"], REPO);
    assert_eq!(rel["title"], format!("Zero Farm agent {v}"));
    assert_eq!(rel["prerelease"], true);
    assert!(rel["notes"].as_str().unwrap().contains(&format!("minisign -Vm SHA256SUMS -P {}", key.public_b64())));
    let assets = gh_assets(&gh_dir, &tag);
    assert_eq!(assets["ZeroFarmSetup.exe"], b"MZ fake installer bytes");
    assert_eq!(assets["farm-agent.exe"], std::fs::read(out.join("farm-agent.exe")).unwrap());
    check_sums(&assets, &std::fs::read_to_string(farm_release::pub_path(&k)).unwrap(), &v);

    // The coordinator: the build, then the release naming the installer's GitHub URL. Nothing
    // goes to R2's old setup/ prefix any more.
    let rel = mock.with(|s| s.agent_releases.clone());
    assert_eq!(rel.len(), 1);
    assert_eq!(rel[0]["github_url"], url.as_str());
    assert!(rel[0].get("setup").is_none());
    assert!(mock.with(|s| s.artifacts.keys().all(|k| k.starts_with("builds/"))));
}

#[test]
fn a_failed_github_release_never_points_the_channel_at_it() {
    let t = tempfile::tempdir().unwrap();
    let k = t.path().join("release.key");
    let key = farm_release::keygen(&k).unwrap();
    let mock = MockCoord::start();
    mock.trust(&key.public_b64());
    let bins = bin("farm-agent").parent().unwrap().to_path_buf();
    let out = t.path().join("dist").join("agent");
    let setup = t.path().join("prebuilt-setup.exe");
    std::fs::write(&setup, b"MZ fake installer bytes").unwrap();
    let gh_dir = t.path().join("gh");
    let fakegh = bin("fakegh");
    let mut args = release_args(k.to_str().unwrap(), out.to_str().unwrap(), &mock.url, bins.to_str().unwrap());
    args.extend(["--allow-test-binaries", "--setup-exe", setup.to_str().unwrap(), "--github", REPO, "--gh", fakegh.to_str().unwrap()]);
    let env = [(TEST_KEYS_ENV, key.public_b64()), ("FAKE_GH_DIR", gh_dir.to_string_lossy().into_owned()), ("FAKE_GH_FAIL", "create".into())];
    let o = cli(&args, &env.iter().map(|(a, b)| (*a, b.as_str())).collect::<Vec<_>>());
    assert!(!o.status.success());
    assert!(text(&o).contains("gh release create failed"), "{}", text(&o));
    assert!(mock.with(|s| s.agent_releases.is_empty()), "the channel still points at the previous release");
    // A malformed repo is refused before anything is built or signed.
    let mut args = release_args(k.to_str().unwrap(), out.to_str().unwrap(), &mock.url, bins.to_str().unwrap());
    args.extend(["--allow-test-binaries", "--setup-exe", setup.to_str().unwrap(), "--github", "not-a-repo"]);
    let o = cli(&args, &[(TEST_KEYS_ENV, &key.public_b64())]);
    assert!(text(&o).contains("OWNER/REPO"), "{}", text(&o));
}

/// A signed agent package in `<root>/dist/agent-<version>` and, unless `stale`, an installer next
/// to it that embeds exactly that package (as `release-agent --setup` leaves them).
fn built_package(root: &Path, key: &zerofarm_common::sign::SigningKey, version: &str, stale: bool) -> std::path::PathBuf {
    let dir = root.join("dist").join(format!("agent-{version}"));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("farm-agent.exe"), format!("MZ agent {version}")).unwrap();
    std::fs::write(dir.join("ZeroFarm.exe"), b"MZ launcher").unwrap();
    let opts = farm_release::SignOpts { kind: Some(Kind::Agent), version: Some(version.into()), min_agent: Some("0.1.0".into()), ..Default::default() };
    farm_release::sign(&dir, &opts, key).unwrap();
    let mut setup = b"MZ setup stub ".to_vec();
    setup.extend(farm_release::pack_payload(&dir).unwrap());
    std::fs::write(root.join("dist").join("ZeroFarmSetup.exe"), setup).unwrap();
    if stale {
        // A later build of the same version: the installer no longer holds this package.
        std::fs::write(dir.join("farm-agent.exe"), format!("MZ agent {version}, rebuilt")).unwrap();
        farm_release::sign(&dir, &opts, key).unwrap();
    }
    dir
}

#[test]
fn github_release_publishes_an_existing_package_and_reruns_with_clobber() {
    let t = tempfile::tempdir().unwrap();
    let k = t.path().join("release.key");
    let key = farm_release::keygen(&k).unwrap();
    let pubfile = std::fs::read_to_string(farm_release::pub_path(&k)).unwrap();
    let dir = built_package(t.path(), &key, "0.2.0", false);
    let mock = MockCoord::start();
    mock.trust(&key.public_b64());
    let gh_dir = t.path().join("gh");
    let fakegh = bin("fakegh");
    let args = |extra: &[&str]| -> Vec<String> {
        let mut a: Vec<String> = ["github-release", dir.to_str().unwrap(), "--github", REPO, "--channel", "stable", "--key", k.to_str().unwrap()]
            .iter()
            .map(|s| s.to_string())
            .collect();
        a.extend(["--gh".into(), fakegh.to_string_lossy().into_owned(), "--url".into(), mock.url.clone(), "--token".into(), "admin-secret".into()]);
        a.extend(extra.iter().map(|s| s.to_string()));
        a
    };
    let env = [("FAKE_GH_DIR", gh_dir.to_str().unwrap())];
    let run = |a: Vec<String>| cli(&a.iter().map(String::as_str).collect::<Vec<_>>(), &env);
    let o = run(args(&[]));
    assert!(o.status.success(), "{}", text(&o));
    let rel = gh_release(&gh_dir, "agent-v0.2.0");
    assert_eq!(rel["prerelease"], false, "stable is a normal release");
    let create = gh_calls(&gh_dir).into_iter().find(|c| c[1] == "create").unwrap();
    assert!(!create.contains(&"--prerelease".to_string()), "{create:?}");
    let assets = gh_assets(&gh_dir, "agent-v0.2.0");
    assert_eq!(assets["ZeroFarmSetup.exe"], std::fs::read(t.path().join("dist").join("ZeroFarmSetup.exe")).unwrap());
    check_sums(&assets, &pubfile, "0.2.0");
    // The coordinator: the build (published again, idempotently) and the release with its URL;
    // min_agent from the manifest.
    let url = format!("https://github.com/{REPO}/releases/download/agent-v0.2.0/ZeroFarmSetup.exe");
    let rel = mock.with(|s| s.agent_releases.clone());
    assert_eq!(rel.len(), 1);
    assert_eq!(rel[0]["github_url"], url.as_str());
    assert_eq!(rel[0]["channel"], "stable");
    assert_eq!(rel[0]["min_agent"], "0.1.0");
    assert_eq!(rel[0]["version"], "0.2.0");
    assert_eq!(mock.with(|s| s.builds_registered.len()), 1);

    // Again, with notes: the release exists, so its assets are replaced (--clobber) and its
    // notes refreshed; nothing is created twice.
    let notes = t.path().join("notes.md");
    std::fs::write(&notes, "Hand-written notes for 0.2.0.").unwrap();
    let o = run(args(&["--notes-file", notes.to_str().unwrap()]));
    assert!(o.status.success(), "{}", text(&o));
    assert!(text(&o).contains("updated"), "{}", text(&o));
    let calls = gh_calls(&gh_dir);
    assert_eq!(calls.iter().map(|c| c[1].as_str()).collect::<Vec<_>>(), vec!["view", "create", "view", "upload", "edit"], "{calls:?}");
    assert!(calls[3].contains(&"--clobber".to_string()), "{:?}", calls[3]);
    assert!(calls[4].contains(&"--prerelease=false".to_string()), "{:?}", calls[4]);
    assert_eq!(gh_release(&gh_dir, "agent-v0.2.0")["notes"], "Hand-written notes for 0.2.0.");
    check_sums(&gh_assets(&gh_dir, "agent-v0.2.0"), &pubfile, "0.2.0");
}

#[test]
fn github_release_refuses_an_installer_of_another_package() {
    let t = tempfile::tempdir().unwrap();
    let k = t.path().join("release.key");
    let key = farm_release::keygen(&k).unwrap();
    let dir = built_package(t.path(), &key, "0.2.0", true);
    let gh_dir = t.path().join("gh");
    let fakegh = bin("fakegh");
    let o = cli(
        &["github-release", dir.to_str().unwrap(), "--github", REPO, "--key", k.to_str().unwrap(), "--gh", fakegh.to_str().unwrap()],
        &[("FAKE_GH_DIR", gh_dir.to_str().unwrap())],
    );
    assert!(!o.status.success());
    assert!(text(&o).contains("does not contain the package"), "{}", text(&o));
    assert!(gh_calls(&gh_dir).is_empty(), "nothing went to GitHub");
}

// ---------- release key backup: export-key / import-key ----------

const PASS: &str = "correct horse battery staple";

/// A scripted passphrase reader: hands out `answers` in order and counts the prompts.
fn answers(list: &[&str]) -> (impl FnMut(&str) -> Result<String, String>, std::rc::Rc<std::cell::Cell<usize>>) {
    let asked = std::rc::Rc::new(std::cell::Cell::new(0));
    let a = asked.clone();
    let mut it = list.iter().map(|s| s.to_string()).collect::<Vec<_>>().into_iter();
    (
        move |_prompt: &str| {
            a.set(a.get() + 1);
            it.next().ok_or_else(|| "no more answers".to_string())
        },
        asked,
    )
}

#[test]
fn export_then_import_gives_the_same_key() {
    use base64::Engine as _;
    let t = tempfile::tempdir().unwrap();
    let k = t.path().join("release.key");
    let key = farm_release::keygen(&k).unwrap();
    let out = t.path().join("backup").join("zero-farm-release.minisign.key");
    let (mut ask, asked) = answers(&[PASS, PASS]);
    let exported = farm_release::export_key(&k, &out, &mut ask).unwrap();
    assert_eq!(asked.get(), 2, "the passphrase is asked for twice");
    assert_eq!(exported.public_b64(), key.public_b64());

    // A standard minisign secret key file, encrypted: scrypt KDF ("Sc"), not the plain key.
    let text = std::fs::read_to_string(&out).unwrap();
    let mut lines = text.lines();
    assert!(lines.next().unwrap().starts_with("untrusted comment: "));
    let raw = base64::engine::general_purpose::STANDARD.decode(lines.next().unwrap()).unwrap();
    assert_eq!(raw.len(), 158);
    assert_eq!(&raw[..6], b"EdScB2");
    let plain = key.secret_box_string().unwrap();
    assert_ne!(text.lines().nth(1), plain.lines().nth(1));
    // The reference implementation (the `minisign` crate, as the minisign CLI) decrypts it with
    // the passphrase and signs with it.
    let sk = minisign::SecretKey::from_file(&out, Some(PASS.to_string())).unwrap();
    let pk = minisign::PublicKey::from_base64(&key.public_b64()).unwrap();
    let sb = minisign::sign(Some(&pk), &sk, std::io::Cursor::new(b"made by minisign"), Some("t"), None).unwrap();
    let pubfile = std::fs::read_to_string(farm_release::pub_path(&k)).unwrap();
    let vpk = minisign_verify::PublicKey::decode(&pubfile).unwrap();
    vpk.verify(b"made by minisign", &minisign_verify::Signature::decode(&sb.into_string()).unwrap(), false).unwrap();

    // Import on "another PC": the same key id and public key, a working DPAPI key file whose
    // signatures verify with the original public key.
    let k2 = t.path().join("new-pc").join("release.key");
    let (mut ask, asked) = answers(&[PASS]);
    let imported = farm_release::import_key(&out, &k2, &mut ask).unwrap();
    assert_eq!(asked.get(), 1);
    assert_eq!(imported.key_id_hex(), key.key_id_hex());
    assert_eq!(imported.public_b64(), key.public_b64());
    assert_eq!(std::fs::read_to_string(farm_release::pub_path(&k2)).unwrap(), pubfile);
    let loaded = farm_release::load_key(&k2).unwrap();
    let sig = loaded.sign(b"signed after import", "build_id=x kind=agent").unwrap();
    vpk.verify(b"signed after import", &minisign_verify::Signature::decode(&sig).unwrap(), false).unwrap();
    TrustedKeys::parse(&[key.public_b64()]).unwrap().verify(b"signed after import", &sig).unwrap();
    // And a package signed with it verifies against the original key.
    let d = worker_dir(t.path());
    farm_release::sign(
        &d,
        &farm_release::SignOpts { kind: Some(Kind::Worker), game: Some("mtg".into()), engine_commit: Some(EC.into()), core_commit: Some(CC.into()), ..Default::default() },
        &loaded,
    )
    .unwrap();
    farm_release::verify(&d, &TrustedKeys::parse(&[key.public_b64()]).unwrap()).unwrap();
}

#[test]
fn key_backup_refuses_wrong_or_mismatched_passphrases() {
    let t = tempfile::tempdir().unwrap();
    let k = t.path().join("release.key");
    farm_release::keygen(&k).unwrap();
    let out = t.path().join("backup.key");
    // Two different entries, or an empty one: nothing written.
    let (mut ask, _) = answers(&[PASS, "correct horse battery stapler"]);
    let e = farm_release::export_key(&k, &out, &mut ask).err().unwrap();
    assert!(e.contains("differ"), "{e}");
    assert!(!out.exists());
    let (mut ask, _) = answers(&["", ""]);
    let e = farm_release::export_key(&k, &out, &mut ask).err().unwrap();
    assert!(e.contains("empty"), "{e}");
    assert!(!out.exists());
    // A wrong passphrase at import: refused, no key file.
    let (mut ask, _) = answers(&[PASS, PASS]);
    farm_release::export_key(&k, &out, &mut ask).unwrap();
    let k2 = t.path().join("k2.key");
    let (mut ask, _) = answers(&["Correct horse battery staple"]);
    let e = farm_release::import_key(&out, &k2, &mut ask).err().unwrap();
    assert!(e.contains("wrong passphrase"), "{e}");
    assert!(!k2.exists());
    // The CLI never takes a passphrase as an argument.
    let o = cli(&["export-key", "--key", k.to_str().unwrap(), "--out", t.path().join("x.key").to_str().unwrap(), "--passphrase", PASS], &[]);
    assert!(!o.status.success());
    assert!(text(&o).contains("never taken from the command line"), "{}", text(&o));
    assert!(!t.path().join("x.key").exists());
}

#[test]
fn key_backup_never_overwrites() {
    let t = tempfile::tempdir().unwrap();
    let k = t.path().join("release.key");
    farm_release::keygen(&k).unwrap();
    let out = t.path().join("backup.key");
    std::fs::write(&out, "keep me").unwrap();
    let (mut ask, asked) = answers(&[PASS, PASS]);
    let e = farm_release::export_key(&k, &out, &mut ask).err().unwrap();
    assert!(e.contains("refusing to overwrite"), "{e}");
    assert_eq!(asked.get(), 0, "refused before asking");
    assert_eq!(std::fs::read_to_string(&out).unwrap(), "keep me");
    // Import onto an existing DPAPI key file: refused, the key file unchanged.
    std::fs::remove_file(&out).unwrap();
    let (mut ask, _) = answers(&[PASS, PASS]);
    farm_release::export_key(&k, &out, &mut ask).unwrap();
    let before = std::fs::read(&k).unwrap();
    let (mut ask, asked) = answers(&[PASS]);
    let e = farm_release::import_key(&out, &k, &mut ask).err().unwrap();
    assert!(e.contains("refusing to overwrite"), "{e}");
    assert_eq!(asked.get(), 0);
    assert_eq!(std::fs::read(&k).unwrap(), before);
}

#[test]
fn release_agent_refuses_an_agent_that_would_not_trust_its_own_updates() {
    let t = tempfile::tempdir().unwrap();
    let k = t.path().join("release.key");
    farm_release::keygen(&k).unwrap();
    let mock = MockCoord::start();
    let bins = bin("farm-agent").parent().unwrap().to_path_buf();
    let out = t.path().join("dist");
    let mut args = release_args(k.to_str().unwrap(), out.to_str().unwrap(), &mock.url, bins.to_str().unwrap());
    args.push("--allow-test-binaries");
    let o = cli(&args, &[]);
    assert!(!o.status.success());
    assert!(text(&o).contains("does not trust the signing key"), "{}", text(&o));
    assert!(mock.with(|s| s.agent_releases.is_empty()));
}

#[test]
fn release_agent_refuses_debug_binaries() {
    let t = tempfile::tempdir().unwrap();
    let k = t.path().join("release.key");
    let key = farm_release::keygen(&k).unwrap();
    let mock = MockCoord::start();
    let bins = bin("farm-agent").parent().unwrap().to_path_buf();
    let out = t.path().join("dist");
    let o = cli(&release_args(k.to_str().unwrap(), out.to_str().unwrap(), &mock.url, bins.to_str().unwrap()), &[(TEST_KEYS_ENV, &key.public_b64())]);
    assert!(!o.status.success());
    assert!(text(&o).contains("debug build"), "{}", text(&o));
}

// ---------- keys and definitions (PROTOCOL 7) ----------

#[test]
fn keygen_data_says_where_the_key_belongs() {
    let t = tempfile::tempdir().unwrap();
    let k = t.path().join("data.key");
    let o = cli(&["keygen", "--data", "--out", k.to_str().unwrap()], &[]);
    assert!(o.status.success(), "{}", text(&o));
    let out = text(&o);
    assert!(out.contains("trusted_data_keys.txt, NOT trusted_keys.txt"), "{out}");
    assert!(!out.contains("Add the public key to agent/common/trusted_keys.txt"), "{out}");
    // Same key file format as a release key: `define` loads it.
    farm_release::load_key(&k).unwrap();
    let r = cli(&["keygen", "--out", t.path().join("release.key").to_str().unwrap()], &[]);
    assert!(text(&r).contains("RELEASE key") && text(&r).contains("trusted_keys.txt"), "{}", text(&r));
}

#[test]
fn the_exact_key_set_is_checked() {
    use farm_release::{check_key_set, parse_keys_output, KeySet};
    let (a, b, c) = ("RWQAZVT770KTG1hmXk6yJ5i9pF1v5Q4lNLQ9C3fOUo8ynWWfamzJ2LpS", "RWTImfGffZDbH0xWVxO/H563BYqds0G+D+hJLn79XEo9xU3sbl6lLp9D", "RWQxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx");
    let got = parse_keys_output(&format!("release 1B9342EFFB546500 {a}\ndata 1FDB907D9FF199C8 {b}\n")).unwrap();
    let want = KeySet { release: [a.to_string()].into(), data: [b.to_string()].into() };
    check_key_set("farm-agent.exe", &got, &want).unwrap();
    // A stray build-time key on either list is refused, and so is a missing one.
    let extra = parse_keys_output(&format!("release X {a}\nrelease Y {c}\ndata Z {b}\n")).unwrap();
    let e = check_key_set("farm-agent.exe", &extra, &want).unwrap_err();
    assert!(e.contains("release keys: extra") && e.contains(c), "{e}");
    let no_data = parse_keys_output(&format!("release X {a}\n")).unwrap();
    assert!(check_key_set("farm-agent.exe", &no_data, &want).unwrap_err().contains("data keys"));
    let swapped = parse_keys_output(&format!("release X {b}\ndata Z {a}\n")).unwrap();
    assert!(check_key_set("farm-agent.exe", &swapped, &want).is_err(), "a release key is not a data key");
    // Agents before 0.2.5 print `<id> <key>`: release keys.
    assert_eq!(parse_keys_output(&format!("1B9342EFFB546500 {a}\n")).unwrap().release, [a.to_string()].into());
    assert!(parse_keys_output("what is this line").is_err());
    // This tree's files are what the shipped binaries must trust.
    let ws = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let files = farm_release::expected_keys(&ws, false).unwrap();
    assert!(files.release.contains(a) && files.data.contains(b), "{files:?}");
    assert!(files.release.is_disjoint(&files.data));
}

#[test]
fn the_launcher_and_agent_print_their_trusted_keys() {
    let ws = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let want = farm_release::expected_keys(&ws, false).unwrap();
    let run = |exe: &str| {
        let o = Command::new(bin(exe)).arg("keys").env_remove(TEST_KEYS_ENV).env_remove(TEST_DATA_KEYS_ENV).output().unwrap();
        farm_release::parse_keys_output(&String::from_utf8_lossy(&o.stdout)).unwrap()
    };
    let agent = run("farm-agent");
    farm_release::check_key_set("farm-agent.exe", &agent, &want).unwrap();
    let launcher = run("ZeroFarm");
    assert_eq!(launcher.release, want.release);
    assert!(launcher.data.is_empty());
}

fn definition_json(id: &str, version: u64) -> serde_json::Value {
    serde_json::json!({
        "definition": 1, "kind": "run", "id": id, "version": version, "created": 1794500000, "expires": null,
        "game": "mtg", "builds": ["0123456789abcdef01234567"], "runtime": null,
        "checkpoints": {"81": {"model_sha256": "a".repeat(64), "model_size": 13421772, "reference_sha256": "b".repeat(64), "reference_size": 412000}},
        "jobs": {"selfplay": {"settings": {"sims": 64}}, "eval": null, "fuzz": null}
    })
}

#[test]
fn define_signs_publishes_and_registers_a_definition() {
    let t = tempfile::tempdir().unwrap();
    let dk = t.path().join("data.key");
    let data = farm_release::keygen(&dk).unwrap();
    let file = t.path().join("eoe2-v7.json");
    // Signed as the bytes are, never re-serialised (compact here, with a trailing newline).
    let mut bytes = serde_json::to_vec(&definition_json("eoe2", 7)).unwrap();
    bytes.push(b'\n');
    std::fs::write(&file, &bytes).unwrap();
    let mock = MockCoord::start();
    mock.trust_data(&data.public_b64());
    let env = [(TEST_DATA_KEYS_ENV, data.public_b64()), ("ZEROFARM_ADMIN_TOKEN", "admin-secret".to_string())];
    let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, v.as_str())).collect();

    let o = cli(&["define", file.to_str().unwrap(), "--key", dk.to_str().unwrap(), "--publish", "--url", &mock.url], &env);
    assert!(o.status.success(), "{}", text(&o));
    let out: serde_json::Value = serde_json::from_slice(&o.stdout).expect("one JSON line");
    let key = "defs/run/eoe2/7/definition.json";
    let sha = zerofarm_common::hash::sha256_hex(&bytes);
    assert_eq!(out["key"], key);
    assert_eq!(out["sha256"], sha.as_str());
    assert_eq!((out["kind"].as_str(), out["id"].as_str(), out["version"].as_u64()), (Some("run"), Some("eoe2"), Some(7)));
    assert_eq!(out["published"], true);
    // What the agent checks: the stored bytes, a minisign signature by the data key (prehashed),
    // verifiable with the minisign tooling.
    assert_eq!(mock.with(|s| s.artifacts.get(key).cloned()).unwrap(), bytes);
    let sig = String::from_utf8(mock.with(|s| s.artifacts.get(&format!("{key}.minisig")).cloned()).unwrap()).unwrap();
    assert_eq!(std::fs::read_to_string(t.path().join("eoe2-v7.json.minisig")).unwrap(), sig);
    let pk = minisign_verify::PublicKey::from_base64(&data.public_b64()).unwrap();
    let s = minisign_verify::Signature::decode(&sig).unwrap();
    pk.verify(&bytes, &s, false).unwrap();
    assert_eq!(s.trusted_comment(), "definition=1 kind=run id=eoe2 version=7");
    let d = zerofarm_common::definition::verify(&TrustedKeys::parse(&[data.public_b64()]).unwrap(), &bytes, &sig).unwrap();
    assert_eq!(d.version, 7);
    let reg = mock.with(|s| s.definitions_registered.clone());
    assert_eq!(reg, vec![serde_json::json!({"kind": "run", "id": "eoe2", "version": 7, "key": key, "sha256": sha})]);
    // Uploads first, registration last.
    let order: Vec<String> = mock.with(|s| s.log.iter().filter(|l| l.method == "PUT").map(|l| l.path.clone()).collect());
    assert_eq!(order, [format!("/v1/admin/artifact/{key}"), format!("/v1/admin/artifact/{key}.minisig"), "/v1/admin/definition".to_string()]);

    // Without --publish: signed locally only.
    let file2 = t.path().join("v8.json");
    std::fs::write(&file2, serde_json::to_vec_pretty(&definition_json("eoe2", 8)).unwrap()).unwrap();
    let o = cli(&["define", file2.to_str().unwrap(), "--key", dk.to_str().unwrap()], &env);
    assert!(o.status.success(), "{}", text(&o));
    assert!(t.path().join("v8.json.minisig").exists());
    assert_eq!(mock.with(|s| s.definitions_registered.len()), 1);
    let o = cli(&["define", file2.to_str().unwrap(), "--key", dk.to_str().unwrap(), "--url", &mock.url], &env);
    assert!(!o.status.success() && text(&o).contains("--publish"), "{}", text(&o));
}

#[test]
fn define_refuses_bad_definitions_and_the_wrong_keys() {
    let t = tempfile::tempdir().unwrap();
    let dk = t.path().join("data.key");
    let data = farm_release::keygen(&dk).unwrap();
    let rk = t.path().join("release.key");
    let release = farm_release::keygen(&rk).unwrap();
    let env_both = [(TEST_DATA_KEYS_ENV, data.public_b64()), (TEST_KEYS_ENV, release.public_b64())];
    let env_both: Vec<(&str, &str)> = env_both.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let file = t.path().join("d.json");
    let write = |v: &serde_json::Value| std::fs::write(&file, serde_json::to_vec_pretty(v).unwrap()).unwrap();
    let define = |key: &Path, env: &[(&str, &str)]| cli(&["define", file.to_str().unwrap(), "--key", key.to_str().unwrap()], env);

    write(&definition_json("eoe2", 1));
    // The release key never signs a definition.
    let o = define(&rk, &env_both);
    assert!(!o.status.success() && text(&o).contains("is the release key"), "{}", text(&o));
    // A key the agents don't trust as a data key: they would refuse it.
    let o = define(&dk, &[]);
    assert!(!o.status.success() && text(&o).contains("not a trusted data key"), "{}", text(&o));
    // Invalid definitions.
    for (change, why) in [
        (serde_json::json!({"jobs": {"selfplay": {"settings": {}, "prefix": "x"}}}), "not allowed"),
        (serde_json::json!({"jobs": {"selfplay": {"settings": {"book": "\\\\host\\share\\b"}}}}), "file path"),
        (serde_json::json!({"builds": ["nope"]}), "not a build id"),
        (serde_json::json!({"surprise": true}), "unknown field"),
        (serde_json::json!({"kind": "campaign"}), "campaign defines fuzz jobs only"),
    ] {
        let mut v = definition_json("eoe2", 1);
        for (k, x) in change.as_object().unwrap() {
            v[k] = x.clone();
        }
        write(&v);
        let o = define(&dk, &env_both);
        assert!(!o.status.success() && text(&o).contains(why), "{why}: {}", text(&o));
    }
    assert!(!t.path().join("d.json.minisig").exists(), "nothing signed");
    // And a data key never signs a build.
    let d = worker_dir(t.path());
    let o = cli(&["sign", d.to_str().unwrap(), "--kind", "worker", "--key", dk.to_str().unwrap(), "--game", "mtg", "--engine-commit", EC, "--core-commit", CC], &env_both);
    assert!(!o.status.success() && text(&o).contains("is a data key"), "{}", text(&o));
}

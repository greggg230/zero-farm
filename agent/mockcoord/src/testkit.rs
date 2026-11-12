//! Helpers shared by the integration tests: temporary release keys, signed fake builds and
//! agent versions, locating sibling binaries in the target dir, and process guards.
//!
//! Binaries under test only trust a temporary key through the debug-only
//! `ZEROFARM_TEST_TRUSTED_KEYS` variable, passed per spawned process (never set globally).

use std::path::{Path, PathBuf};
use std::process::Child;
use std::time::{Duration, Instant};
use zerofarm_common::layout::Layout;
use zerofarm_common::manifest::Kind;
use zerofarm_common::sign::{sign_dir, template, SigningKey};

pub const TEST_KEYS_ENV: &str = "ZEROFARM_TEST_TRUSTED_KEYS";

/// `target/debug` (or whichever profile the test binary was built in).
pub fn target_dir() -> PathBuf {
    let exe = std::env::current_exe().unwrap();
    let deps = exe.parent().unwrap();
    if deps.file_name().map(|n| n == "deps").unwrap_or(false) {
        deps.parent().unwrap().to_path_buf()
    } else {
        deps.to_path_buf()
    }
}

/// A binary built by this workspace (e.g. `fakeworker`). `cargo test --workspace` builds them all.
pub fn bin(name: &str) -> PathBuf {
    let p = target_dir().join(format!("{name}.exe"));
    assert!(
        p.exists(),
        "{} not found: run `cargo test --workspace` (or `cargo build --workspace`) so the test binaries exist",
        p.display()
    );
    p
}

pub struct TestKey {
    pub key: SigningKey,
    pub pubkey: String,
}

pub fn test_key() -> TestKey {
    let key = SigningKey::generate().unwrap();
    let pubkey = key.public_b64();
    TestKey { key, pubkey }
}

/// Copies `files` (destination name, source path) into `dir` and signs it as a worker build.
pub fn make_worker_build(dir: &Path, key: &SigningKey, worker_exe: &Path, extra: &[(&str, &[u8])]) -> String {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::copy(worker_exe, dir.join("fakeworker.exe")).unwrap();
    for (name, bytes) in extra {
        std::fs::write(dir.join(name), bytes).unwrap();
    }
    let mut t = template(Kind::Worker, "fake-1");
    t.game = Some("fake".into());
    t.aw = Some(1);
    t.zero_onnx = Some(vec![1]);
    t.profile = Some("release".into());
    t.entry = Some("fakeworker.exe".into());
    sign_dir(dir, t, key).unwrap().0
}

/// Creates `versions\<v>\` in `layout` holding `farm-agent.exe` (a copy of `exe`) plus `extra`
/// files, signed as an agent manifest for version `v`. Returns the build id.
pub fn make_agent_version(layout: &Layout, v: &str, exe: &Path, extra: &[(&str, &[u8])], key: &SigningKey) -> String {
    let dir = layout.version_dir(v);
    make_agent_package(&dir, v, exe, extra, key)
}

pub fn make_agent_package(dir: &Path, v: &str, exe: &Path, extra: &[(&str, &[u8])], key: &SigningKey) -> String {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::copy(exe, dir.join("farm-agent.exe")).unwrap();
    for (name, bytes) in extra {
        std::fs::write(dir.join(name), bytes).unwrap();
    }
    let mut t = template(Kind::Agent, v);
    t.entry = Some("farm-agent.exe".into());
    sign_dir(dir, t, key).unwrap().0
}

/// A copy of a built launcher whose VERSIONINFO says `version` (an "older" launcher for
/// launcher-update tests). Returns its sha256.
pub fn launcher_copy(src: &Path, dest: &Path, version: (u16, u16, u16)) -> String {
    let mut b = std::fs::read(src).unwrap();
    assert!(zerofarm_common::winres::patch_fixed_file_version(&mut b, version), "{} has no VERSIONINFO", src.display());
    std::fs::write(dest, &b).unwrap();
    zerofarm_common::hash::sha256_hex(&b)
}

pub fn sha256_of(p: &Path) -> String {
    zerofarm_common::hash::sha256_hex(&std::fs::read(p).unwrap())
}

/// Kills the child (and, through its job object, its children) when dropped.
pub struct KillOnDrop(pub Child);
impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Polls `f` every 50 ms until it is true or `timeout` passes.
pub fn wait_until(timeout: Duration, mut f: impl FnMut() -> bool) -> bool {
    let end = Instant::now() + timeout;
    loop {
        if f() {
            return true;
        }
        if Instant::now() >= end {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// A unique registry value name for tests (`ZeroFarmTest-<pid>-<n>`), never the real `ZeroFarm`.
pub fn test_reg_name() -> String {
    use std::sync::atomic::{AtomicU32, Ordering};
    static N: AtomicU32 = AtomicU32::new(0);
    format!("ZeroFarmTest-{}-{}", std::process::id(), N.fetch_add(1, Ordering::SeqCst))
}

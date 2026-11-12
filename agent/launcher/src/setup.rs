//! ZeroFarmSetup.exe — the single-file installer friends double-click.
//!
//! The signed agent package (farm-agent.exe, ZeroFarm.exe, manifest.json + .minisig) is embedded
//! at build time. Setup unpacks it to a temp folder, verifies it against the compiled-in release
//! keys, and runs `farm-agent.exe install` from there, which installs per user (no admin, no
//! UAC), registers autostart and the Start-menu/uninstall entries, and starts Zero Farm (which
//! then asks for the invite code). Arguments are passed through to `install` (e.g. `--quiet`).

#![windows_subsystem = "windows"]

use std::os::windows::process::CommandExt;
use zerofarm_common::keys::TrustedKeys;
use zerofarm_common::manifest::{verify_dir, Expect, Kind};

static PAYLOAD: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/setup-payload.bin"));
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

fn message(text: &str) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONERROR, MB_OK};
    let w = |s: &str| s.encode_utf16().chain(std::iter::once(0)).collect::<Vec<u16>>();
    let (t, c) = (w(text), w("Zero Farm Setup"));
    unsafe { MessageBoxW(std::ptr::null_mut(), t.as_ptr(), c.as_ptr(), MB_OK | MB_ICONERROR) };
}

fn run() -> Result<i32, String> {
    if PAYLOAD.is_empty() {
        return Err("This ZeroFarmSetup.exe was built without the Zero Farm files inside.".into());
    }
    let files = zerofarm_common::payload::unpack(PAYLOAD)?;
    let dir = std::env::temp_dir().join(format!("ZeroFarmSetup-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let result = (|| {
        for (name, bytes) in &files {
            std::fs::write(dir.join(name), bytes).map_err(|e| format!("{name}: {e}"))?;
        }
        verify_dir(&dir, &TrustedKeys::compiled(), &Expect { kind: Some(Kind::Agent), ..Default::default() })
            .map_err(|e| format!("The installer's files failed verification ({e}). Please download it again."))?;
        let args: Vec<String> = std::env::args().skip(1).collect();
        let st = std::process::Command::new(dir.join("farm-agent.exe"))
            .arg("install")
            .arg("--source")
            .arg(&dir)
            .args(&args)
            .creation_flags(CREATE_NO_WINDOW)
            .status()
            .map_err(|e| e.to_string())?;
        Ok(st.code().unwrap_or(1))
    })();
    let _ = zerofarm_common::fsutil::remove_dir_all_retry(&dir, std::time::Duration::from_secs(10));
    result
}

fn main() {
    match run() {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            if !std::env::args().any(|a| a == "--quiet") {
                message(&e);
            }
            eprintln!("{e}");
            std::process::exit(1);
        }
    }
}

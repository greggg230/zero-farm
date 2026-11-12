//! * Embeds an application manifest in the binaries: `asInvoker` (never elevate — this matters
//!   for an exe named "...Setup", which Windows would otherwise treat as an installer) and the
//!   Common Controls 6 dependency (themed message boxes).
//! * ZeroFarmSetup.exe: embeds the signed agent package named by ZEROFARM_SETUP_PAYLOAD (built
//!   by `farm-release release-agent --setup`); empty otherwise.
//! * Both: the round "Z" icon and a VERSIONINFO with the crate version (agents compare an
//!   installed launcher's version with their package's before replacing it, PROTOCOL 6).
use std::{env, fs, path::Path};

#[path = "../common/src/buildres.rs"]
mod buildres;
#[path = "../common/src/icon.rs"]
mod icon;
#[path = "../common/src/winres.rs"]
mod winres;

fn main() {
    buildres::rerun_if_changed();
    println!("cargo:rerun-if-env-changed=ZEROFARM_SETUP_PAYLOAD");
    let out = Path::new(&env::var("OUT_DIR").unwrap()).join("setup-payload.bin");
    match env::var("ZEROFARM_SETUP_PAYLOAD") {
        Ok(p) if !p.is_empty() => {
            println!("cargo:rerun-if-changed={p}");
            fs::copy(&p, &out).unwrap_or_else(|e| panic!("ZEROFARM_SETUP_PAYLOAD {p}: {e}"));
        }
        _ => fs::write(&out, b"").unwrap(),
    }
    if env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        println!("cargo:rustc-link-arg-bins=/MANIFEST:EMBED");
        println!("cargo:rustc-link-arg-bins=/MANIFESTUAC:level='asInvoker'");
        println!(
            "cargo:rustc-link-arg-bins=/MANIFESTDEPENDENCY:type='win32' name='Microsoft.Windows.Common-Controls' version='6.0.0.0' processorArchitecture='*' publicKeyToken='6595b64144ccf1df' language='*'"
        );
    }
    buildres::embed("ZeroFarm", "Zero Farm");
    buildres::embed("ZeroFarmSetup", "Zero Farm Setup");
}

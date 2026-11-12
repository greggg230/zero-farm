//! Embeds in farm-agent.exe: an application manifest (`asInvoker`: never elevate; Common
//! Controls 6: themed tray menu, dialog and message boxes) and the round "Z" icon + VERSIONINFO.
use std::env;

#[path = "../common/src/buildres.rs"]
mod buildres;
#[path = "../common/src/icon.rs"]
mod icon;
#[path = "../common/src/winres.rs"]
mod winres;

fn main() {
    buildres::rerun_if_changed();
    if env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        println!("cargo:rustc-link-arg-bins=/MANIFEST:EMBED");
        println!("cargo:rustc-link-arg-bins=/MANIFESTUAC:level='asInvoker'");
        println!(
            "cargo:rustc-link-arg-bins=/MANIFESTDEPENDENCY:type='win32' name='Microsoft.Windows.Common-Controls' version='6.0.0.0' processorArchitecture='*' publicKeyToken='6595b64144ccf1df' language='*'"
        );
    }
    buildres::embed("farm-agent", "Zero Farm");
}

//! fakelauncher.exe gets a VERSIONINFO (launcher self-update tests compare versions).
#[path = "../common/src/buildres.rs"]
mod buildres;
#[path = "../common/src/icon.rs"]
mod icon;
#[path = "../common/src/winres.rs"]
mod winres;

fn main() {
    buildres::rerun_if_changed();
    buildres::embed("fakelauncher", "Zero Farm test launcher");
}

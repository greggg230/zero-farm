//! fakelauncher: stands in for a new ZeroFarm.exe in launcher self-update tests. It carries a
//! VERSIONINFO (build.rs), so an agent sees it as newer than a launcher patched to 0.0.1.
//!
//! It runs from the install root (as `ZeroFarm.new.exe` for the agent's selftest, then as
//! `ZeroFarm.exe`) and follows `<root>\fakelauncher.behaviour`, one `<when> <what>` per line:
//!
//! ```text
//! selftest ok | fail       `ZeroFarm.exe selftest` exits 0 / 2 (default ok)
//! takeover crash | hang    started with --takeover: exit 3 at once / never confirm (default crash)
//! ```
//!
//! Every start appends its arguments to `<root>\fakelauncher.log`.

use std::io::Write;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let exe = std::env::current_exe().unwrap();
    let root = exe.parent().unwrap().to_path_buf();
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(root.join("fakelauncher.log")) {
        let _ = writeln!(f, "{}", args.join(" "));
    }
    let script = std::fs::read_to_string(root.join("fakelauncher.behaviour")).unwrap_or_default();
    let rule = |when: &str, default: &str| -> String {
        script
            .lines()
            .filter_map(|l| l.split_once(' '))
            .find(|(w, _)| *w == when)
            .map(|(_, what)| what.trim().to_string())
            .unwrap_or_else(|| default.to_string())
    };
    if args.first().map(String::as_str) == Some("selftest") {
        if rule("selftest", "ok") == "ok" {
            println!("ok fake");
            std::process::exit(0);
        }
        println!("fail: scripted");
        std::process::exit(2);
    }
    if args.iter().any(|a| a == "--takeover") {
        match rule("takeover", "crash").as_str() {
            "hang" => loop {
                std::thread::sleep(std::time::Duration::from_secs(3600));
            },
            _ => std::process::exit(3),
        }
    }
    std::process::exit(0);
}

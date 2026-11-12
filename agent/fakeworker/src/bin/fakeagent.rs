//! fakeagent: stands in for farm-agent.exe in launcher tests.
//!
//! `fakeagent run --root <root>`; its version is the name of the directory it runs from
//! (`versions\<v>\farm-agent.exe`). It executes the steps in a `behaviour` file next to itself
//! (default: `healthy` then `wait-stop`), one per line:
//!
//! ```text
//! healthy          write state\healthy-<v>
//! sleep <ms>
//! pending <v>      write state\pending
//! exit <code>
//! crash            exit 3
//! wait-stop        wait until <root>\stop exists, then exit 0
//! forever          never return
//! once: <step>     run <step> only the first time this version reaches this line
//! ```
//!
//! Every start appends `<v> start <pid>` to `<root>\fakeagent.log`.

use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let root = args.iter().position(|a| a == "--root").and_then(|i| args.get(i + 1)).map(PathBuf::from).expect("--root");
    let exe = std::env::current_exe().unwrap();
    let dir = exe.parent().unwrap();
    let version = dir.file_name().unwrap().to_string_lossy().to_string();
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(root.join("fakeagent.log")) {
        let _ = writeln!(f, "{version} start {}", std::process::id());
    }
    let script = std::fs::read_to_string(dir.join("behaviour")).unwrap_or_else(|_| "healthy\nwait-stop\n".into());
    let state = root.join("state");
    for (i, line) in script.lines().enumerate() {
        let line = match line.strip_prefix("once:") {
            Some(rest) => {
                let marker = root.join("fakeagent-once").join(format!("{version}-{i}"));
                if marker.exists() {
                    continue;
                }
                std::fs::create_dir_all(marker.parent().unwrap()).unwrap();
                std::fs::write(&marker, "1").unwrap();
                rest.trim()
            }
            None => line,
        };
        let mut it = line.split_whitespace();
        match (it.next(), it.next()) {
            (Some("healthy"), _) => {
                std::fs::create_dir_all(&state).unwrap();
                std::fs::write(state.join(format!("healthy-{version}")), "1").unwrap();
            }
            (Some("sleep"), Some(ms)) => std::thread::sleep(Duration::from_millis(ms.parse().unwrap())),
            (Some("pending"), Some(v)) => std::fs::write(state.join("pending"), v).unwrap(),
            (Some("exit"), Some(c)) => std::process::exit(c.parse().unwrap()),
            (Some("crash"), _) => std::process::exit(3),
            (Some("wait-stop"), _) => {
                while !root.join("stop").exists() {
                    std::thread::sleep(Duration::from_millis(50));
                }
                std::process::exit(0);
            }
            (Some("forever"), _) => loop {
                std::thread::sleep(Duration::from_secs(3600));
            },
            (None, _) => {}
            (Some(other), _) => panic!("unknown step {other}"),
        }
    }
}

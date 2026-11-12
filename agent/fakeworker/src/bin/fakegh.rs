//! fakegh.exe: a stand-in for the GitHub CLI in farm-release's tests. It understands the four
//! calls farm-release makes and keeps the "GitHub" side in a directory:
//!
//! ```text
//! gh release view TAG --repo R --json tagName
//! gh release create TAG FILE... --repo R --title T --notes-file F [--prerelease]
//! gh release upload TAG FILE... --repo R [--clobber]
//! gh release edit TAG --repo R --title T --notes-file F --prerelease=BOOL
//! ```
//!
//! `FAKE_GH_DIR` (required): every call is appended to `calls.jsonl` (its argv), and release TAG
//! lives in `releases/TAG/` (`release.json`: repo, title, notes, prerelease; `assets/<name>`,
//! copied at upload time). `FAKE_GH_FAIL=<subcommand>` makes that subcommand fail.
//! Like the real CLI, `view` of a missing release says "release not found" and exits 1, `create`
//! of an existing one fails, and `upload` of an existing asset without `--clobber` fails.

use serde_json::{json, Value};
use std::io::Write;
use std::path::{Path, PathBuf};

fn fail(msg: &str) -> ! {
    eprintln!("{msg}");
    std::process::exit(1);
}

struct Parsed {
    pos: Vec<String>,
    flags: Vec<(String, Option<String>)>,
}

const VALUE_FLAGS: &[&str] = &["--repo", "--title", "--notes-file", "--json"];

fn parse(args: &[String]) -> Parsed {
    let mut pos = Vec::new();
    let mut flags = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if let Some((k, v)) = a.split_once('=').filter(|_| a.starts_with("--")) {
            flags.push((k.to_string(), Some(v.to_string())));
        } else if VALUE_FLAGS.contains(&a.as_str()) {
            flags.push((a.clone(), it.next().cloned()));
        } else if a.starts_with("--") {
            flags.push((a.clone(), None));
        } else {
            pos.push(a.clone());
        }
    }
    Parsed { pos, flags }
}

impl Parsed {
    fn get(&self, k: &str) -> Option<&(String, Option<String>)> {
        self.flags.iter().find(|(n, _)| n == k)
    }
    fn value(&self, k: &str) -> Option<String> {
        self.get(k).and_then(|(_, v)| v.clone())
    }
}

fn copy_assets(dir: &Path, files: &[String], clobber: bool) {
    let assets = dir.join("assets");
    std::fs::create_dir_all(&assets).unwrap();
    for f in files {
        let src = PathBuf::from(f);
        let name = src.file_name().unwrap_or_else(|| fail(&format!("bad asset path {f}"))).to_owned();
        let dest = assets.join(&name);
        if dest.exists() && !clobber {
            fail(&format!("asset under the same name already exists: {}", name.to_string_lossy()));
        }
        std::fs::copy(&src, &dest).unwrap_or_else(|e| fail(&format!("{f}: {e}")));
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let root = PathBuf::from(std::env::var("FAKE_GH_DIR").unwrap_or_else(|_| fail("FAKE_GH_DIR is not set")));
    std::fs::create_dir_all(&root).unwrap();
    let mut log = std::fs::OpenOptions::new().create(true).append(true).open(root.join("calls.jsonl")).unwrap();
    writeln!(log, "{}", json!(args)).unwrap();

    let p = parse(&args);
    if p.pos.first().map(String::as_str) != Some("release") || p.pos.len() < 3 {
        fail("fakegh: only `release view|create|upload|edit TAG ...`");
    }
    let sub = p.pos[1].as_str();
    if std::env::var("FAKE_GH_FAIL").ok().as_deref() == Some(sub) {
        fail(&format!("HTTP 502: {sub} failed (FAKE_GH_FAIL)"));
    }
    let tag = &p.pos[2];
    let files = &p.pos[3..];
    let dir = root.join("releases").join(tag);
    let meta_path = dir.join("release.json");
    let repo = p.value("--repo").unwrap_or_else(|| fail("--repo is required in tests"));
    let notes = |p: &Parsed| p.value("--notes-file").map(|f| std::fs::read_to_string(&f).unwrap_or_else(|e| fail(&format!("{f}: {e}"))));
    match sub {
        "view" => {
            if !meta_path.exists() {
                fail("release not found");
            }
            println!("{}", json!({"tagName": tag}));
        }
        "create" => {
            if meta_path.exists() {
                fail("a release with the same tag name already exists");
            }
            copy_assets(&dir, files, false);
            let meta = json!({"repo": repo, "title": p.value("--title"), "notes": notes(&p),
                              "prerelease": p.get("--prerelease").is_some()});
            std::fs::write(&meta_path, meta.to_string()).unwrap();
            println!("https://github.com/{repo}/releases/tag/{tag}");
        }
        "upload" => {
            if !meta_path.exists() {
                fail("release not found");
            }
            copy_assets(&dir, files, p.get("--clobber").is_some());
        }
        "edit" => {
            if !meta_path.exists() {
                fail("release not found");
            }
            let mut meta: Value = serde_json::from_str(&std::fs::read_to_string(&meta_path).unwrap()).unwrap();
            if let Some(t) = p.value("--title") {
                meta["title"] = json!(t);
            }
            if let Some(n) = notes(&p) {
                meta["notes"] = json!(n);
            }
            if let Some((_, v)) = p.get("--prerelease") {
                meta["prerelease"] = json!(v.as_deref() != Some("false"));
            }
            std::fs::write(&meta_path, meta.to_string()).unwrap();
        }
        _ => fail(&format!("fakegh: unknown subcommand {sub}")),
    }
}

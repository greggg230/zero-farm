//! zf-pyshim.exe as the agent starts a torch worker build's entry (PROTOCOL 19.4), with the fakeworker standing in for
//! the pack's `python\python.exe` (its python mode): the interpreter flags and the script next to the shim, the
//! arguments passed on, stdin and stdout inherited, the exit code forwarded, loud errors, and (Windows) python killed
//! with the shim. Needs the workspace's binaries (`cargo test --workspace` builds them).

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn shim_exe() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_zf-pyshim"))
}

fn fake_python() -> PathBuf {
    let p = shim_exe().with_file_name(format!("fakeworker{}", std::env::consts::EXE_SUFFIX));
    assert!(p.exists(), "{} not found: run `cargo test --workspace` so the fakeworker exists", p.display());
    p
}

/// A torch worker build (the shim + worker_main.py) and a runtime pack (python = the fakeworker). -> (tmp, build's shim,
/// pack dir)
fn layout() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let t = tempfile::tempdir().unwrap();
    let build = t.path().join("builds").join("torchworker");
    std::fs::create_dir_all(&build).unwrap();
    let shim = build.join(format!("zf-pyshim{}", std::env::consts::EXE_SUFFIX));
    std::fs::copy(shim_exe(), &shim).unwrap();
    std::fs::write(build.join("worker_main.py"), "# the torch worker (lane A)\n").unwrap();
    let pack = t.path().join("builds").join("torchpack");
    let py = pack.join(zf_pyshim::python_rel());
    std::fs::create_dir_all(py.parent().unwrap()).unwrap();
    std::fs::copy(fake_python(), &py).unwrap();
    (t, shim, pack)
}

fn run(shim: &Path, rt: Option<&Path>, args: &[&str], cwd: &Path) -> std::process::Output {
    let mut c = Command::new(shim);
    c.args(args).current_dir(cwd).env_remove(zf_pyshim::RUNTIME_ENV);
    if let Some(rt) = rt {
        c.env(zf_pyshim::RUNTIME_ENV, rt);
    }
    c.output().unwrap()
}

#[test]
fn it_runs_the_packs_python_on_worker_main_with_its_args_and_forwards_the_exit_code() {
    let (t, shim, pack) = layout();
    let work = t.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    let out = run(&shim, Some(&pack), &["shimtest", "exit", "7"], &work);
    assert_eq!(out.status.code(), Some(7), "{}", String::from_utf8_lossy(&out.stderr));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let argv: Vec<String> = v["argv"].as_array().unwrap().iter().map(|x| x.as_str().unwrap().to_string()).collect();
    let script = shim.with_file_name("worker_main.py");
    assert_eq!(&argv[1..], &["-I", "-B", "-X", "utf8", "-u", &script.to_string_lossy(), "shimtest", "exit", "7"], "{argv:?}");
    assert_eq!(v["runtime_dir"].as_str().unwrap(), pack.to_string_lossy());
    assert_eq!(Path::new(v["cwd"].as_str().unwrap()).canonicalize().unwrap(), work.canonicalize().unwrap(), "the shim's current directory");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("fakeworker: python -I -B -X utf8 -u script="), "{err}");
    // exit 0 too
    assert_eq!(run(&shim, Some(&pack), &["shimtest", "exit", "0"], &work).status.code(), Some(0));
}

#[test]
fn stdin_and_stdout_reach_python() {
    let (_t, shim, pack) = layout();
    let mut c = Command::new(&shim)
        .args(["shimtest", "echo"])
        .env(zf_pyshim::RUNTIME_ENV, &pack)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::Write;
    c.stdin.take().unwrap().write_all(b"{\"op\":\"job\"}\n").unwrap();
    let out = c.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "{\"op\":\"job\"}");
}

#[test]
fn a_missing_runtime_python_or_script_fails_loudly() {
    let (t, shim, pack) = layout();
    let out = run(&shim, None, &["serve"], t.path());
    assert_eq!(out.status.code(), Some(zf_pyshim::EXIT_RUNTIME));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.starts_with("zf-pyshim: ZEROFARM_RUNTIME_DIR is not set"), "{err}");
    let out = run(&shim, Some(&t.path().join("nope")), &["serve"], t.path());
    assert_eq!(out.status.code(), Some(zf_pyshim::EXIT_RUNTIME));
    assert!(String::from_utf8_lossy(&out.stderr).contains("is not a directory"));
    std::fs::remove_file(pack.join(zf_pyshim::python_rel())).unwrap();
    let out = run(&shim, Some(&pack), &["serve"], t.path());
    assert_eq!(out.status.code(), Some(zf_pyshim::EXIT_RUNTIME));
    assert!(String::from_utf8_lossy(&out.stderr).contains("has no python"));
    std::fs::copy(fake_python(), pack.join(zf_pyshim::python_rel())).unwrap();
    std::fs::remove_file(shim.with_file_name("worker_main.py")).unwrap();
    let out = run(&shim, Some(&pack), &["serve"], t.path());
    assert_eq!(out.status.code(), Some(zf_pyshim::EXIT_SCRIPT));
    assert!(String::from_utf8_lossy(&out.stderr).contains("no worker_main.py next to the shim"));
}

/// Killing the shim alone (TerminateProcess, as `Child::kill` does) ends python: the shim's own job object.
#[cfg(windows)]
#[test]
fn killing_the_shim_kills_python() {
    let (t, shim, pack) = layout();
    let pidfile = t.path().join("python.pid");
    let mut c = Command::new(&shim).args(["shimtest", "sleep", &pidfile.to_string_lossy()]).env(zf_pyshim::RUNTIME_ENV, &pack).stderr(Stdio::null()).spawn().unwrap();
    let t0 = Instant::now();
    while !pidfile.exists() && t0.elapsed() < Duration::from_secs(20) {
        std::thread::sleep(Duration::from_millis(50));
    }
    let pid: u32 = std::fs::read_to_string(&pidfile).expect("python started").trim().parse().unwrap();
    assert!(alive(pid), "python runs");
    c.kill().unwrap();
    let _ = c.wait();
    let t0 = Instant::now();
    while alive(pid) && t0.elapsed() < Duration::from_secs(10) {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(!alive(pid), "python {pid} outlived the shim");
}

#[cfg(windows)]
fn alive(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, WAIT_TIMEOUT};
    use windows_sys::Win32::System::Threading::{OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE};
    // SAFETY: a handle opened here and closed below.
    unsafe {
        let h = OpenProcess(PROCESS_SYNCHRONIZE, 0, pid);
        if h.is_null() {
            return false;
        }
        let r = WaitForSingleObject(h, 0);
        CloseHandle(h);
        r == WAIT_TIMEOUT
    }
}

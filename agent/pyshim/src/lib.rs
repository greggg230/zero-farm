//! `zf-pyshim.exe` (PROTOCOL 19.4): the entry of a torch worker build.
//!
//! The agent starts a worker build's `entry` as `<entry> serve` with the work directory as its current directory and
//! aw/1 JSON lines on stdin/stdout. A torch worker is python code (`worker_main.py` and its packages, in the worker
//! build) run by the python of the torch runtime pack. This shim is that build's `entry`: it finds the pack through
//! `ZEROFARM_RUNTIME_DIR` (set by the agent, an absolute path, for every worker it starts) and runs
//!
//! ```text
//! %ZEROFARM_RUNTIME_DIR%\python\python.exe -I -B -X utf8 -u <the shim's directory>\worker_main.py <its own arguments>
//! ```
//!
//! with stdin, stdout and stderr inherited (the agent's pipes reach python directly), in the shim's current directory,
//! and exits with python's exit code. On Windows the shim first puts itself in a job object of its own that kills every
//! process in it when its last handle closes, so python (and anything it starts) ends when the shim is killed, also by
//! a plain `TerminateProcess` of the shim alone (the agent's own job ends both anyway).
//!
//! Interpreter flags: `-I` isolated mode (no `PYTHON*` variables, no user site-packages, the script's directory not
//! put on `sys.path`: `worker_main.py` puts its own directory there itself); `-B` no `.pyc` files written - the
//! pack's and the build's directories must hold exactly their manifest's files, or the agent's re-verification before
//! every worker start would refuse them and download them again; `-X utf8` UTF-8 for every text stream and file;
//! `-u` unbuffered stdout and stderr (aw/1 lines reach the agent at once).
//!
//! Failures before python runs are loud: one line on stderr starting `zf-pyshim:` (the agent keeps a worker's stderr
//! in the worker log and in its crash report) and a non-zero exit code ([`EXIT_RUNTIME`], [`EXIT_SCRIPT`],
//! [`EXIT_SPAWN`]).

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// The variable the agent sets to the runtime pack's directory (absolute).
pub const RUNTIME_ENV: &str = "ZEROFARM_RUNTIME_DIR";
/// The worker's script, next to the shim in the worker build.
pub const SCRIPT: &str = "worker_main.py";
/// The interpreter flags, in this order, before the script.
pub const PY_FLAGS: [&str; 5] = ["-I", "-B", "-X", "utf8", "-u"];

/// `ZEROFARM_RUNTIME_DIR` unset, empty, relative or not a directory, or the pack has no python there.
pub const EXIT_RUNTIME: i32 = 3;
/// No `worker_main.py` next to the shim.
pub const EXIT_SCRIPT: i32 = 4;
/// Python could not be started.
pub const EXIT_SPAWN: i32 = 5;

/// The pack's python, relative to the runtime directory.
pub fn python_rel() -> PathBuf {
    if cfg!(windows) {
        Path::new("python").join("python.exe")
    } else {
        Path::new("python").join("bin").join("python3")
    }
}

/// Why the shim can't start python: its exit code and the message for stderr.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShimError {
    pub code: i32,
    pub message: String,
}

impl std::fmt::Display for ShimError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "zf-pyshim: {}", self.message)
    }
}

/// What the shim runs: the interpreter and its arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub python: PathBuf,
    pub args: Vec<OsString>,
}

/// The plan for `runtime_dir` (the variable's value, None when unset), the shim at `exe` and its own arguments `args`
/// (without the program name).
pub fn plan(runtime_dir: Option<OsString>, exe: &Path, args: &[OsString]) -> Result<Plan, ShimError> {
    let err = |code: i32, message: String| Err(ShimError { code, message });
    let rt = match runtime_dir {
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => return err(EXIT_RUNTIME, format!("{RUNTIME_ENV} is not set: the agent sets it to the torch runtime pack's directory for every worker it starts (PROTOCOL 19.4); without it there is no python to run")),
    };
    if !rt.is_absolute() {
        return err(EXIT_RUNTIME, format!("{RUNTIME_ENV} must be an absolute path, not {}", rt.display()));
    }
    if !rt.is_dir() {
        return err(EXIT_RUNTIME, format!("{RUNTIME_ENV} {} is not a directory", rt.display()));
    }
    let python = rt.join(python_rel());
    if !python.is_file() {
        return err(EXIT_RUNTIME, format!("the runtime pack at {} has no {} (is it the torch pack?)", rt.display(), python_rel().display()));
    }
    let Some(dir) = exe.parent() else {
        return err(EXIT_SCRIPT, format!("the shim's own directory is unknown ({})", exe.display()));
    };
    let script = dir.join(SCRIPT);
    if !script.is_file() {
        return err(EXIT_SCRIPT, format!("no {SCRIPT} next to the shim in {}", dir.display()));
    }
    let mut all: Vec<OsString> = PY_FLAGS.iter().map(OsString::from).collect();
    all.push(script.into_os_string());
    all.extend(args.iter().cloned());
    Ok(Plan { python, args: all })
}

/// Puts this process in a job object of its own that kills every process in it when its last handle closes (the
/// handle is kept open, leaked on purpose, until the process ends). Nested in the agent's job, whose limits still hold.
#[cfg(windows)]
pub fn kill_children_with_me() -> Result<(), String> {
    use windows_sys::Win32::Foundation::GetLastError;
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation, SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    // SAFETY: plain Win32 calls on handles created here; the info struct is zeroed and sized as the API expects.
    unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
            return Err(format!("CreateJobObject failed ({})", GetLastError()));
        }
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION;
        if SetInformationJobObject(job, JobObjectExtendedLimitInformation, &info as *const _ as *const core::ffi::c_void, std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32) == 0 {
            return Err(format!("SetInformationJobObject failed ({})", GetLastError()));
        }
        if AssignProcessToJobObject(job, GetCurrentProcess()) == 0 {
            return Err(format!("AssignProcessToJobObject failed ({})", GetLastError()));
        }
        // (the handle stays open for the shim's life: closing it would kill the shim itself)
        Ok(())
    }
}

/// Runs the plan: python with inherited stdio; its exit code (Windows: also when it was killed, e.g. 0xDEAD from the
/// agent's job; elsewhere this process becomes python, and only returns on failure).
pub fn run(p: &Plan) -> Result<i32, ShimError> {
    #[cfg(windows)]
    {
        if let Err(e) = kill_children_with_me() {
            // (python still ends with the agent's job; only a kill of the shim alone would leave it running)
            eprintln!("zf-pyshim: warning: no job object of its own ({e}): python outlives a kill of the shim alone");
        }
        match std::process::Command::new(&p.python).args(&p.args).status() {
            Ok(s) => Ok(s.code().unwrap_or(1)),
            Err(e) => Err(ShimError { code: EXIT_SPAWN, message: format!("could not start {}: {e}", p.python.display()) }),
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let e = std::process::Command::new(&p.python).args(&p.args).exec();
        Err(ShimError { code: EXIT_SPAWN, message: format!("could not start {}: {e}", p.python.display()) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pack() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let t = tempfile::tempdir().unwrap();
        let rt = t.path().join("rt");
        let py = rt.join(python_rel());
        std::fs::create_dir_all(py.parent().unwrap()).unwrap();
        std::fs::write(&py, b"").unwrap();
        let build = t.path().join("build");
        std::fs::create_dir_all(&build).unwrap();
        std::fs::write(build.join(SCRIPT), b"").unwrap();
        (t, rt, build.join("zf-pyshim.exe"))
    }

    #[test]
    fn the_plan_is_the_packs_python_on_the_builds_script_with_the_args() {
        let (_t, rt, exe) = pack();
        let p = plan(Some(rt.clone().into_os_string()), &exe, &["serve".into(), "--x".into()]).unwrap();
        assert_eq!(p.python, rt.join(python_rel()));
        let script = exe.parent().unwrap().join(SCRIPT).into_os_string();
        let want: Vec<OsString> = ["-I", "-B", "-X", "utf8", "-u"].iter().map(OsString::from).chain([script, "serve".into(), "--x".into()]).collect();
        assert_eq!(p.args, want);
    }

    #[test]
    fn every_missing_piece_is_a_loud_error_with_its_code() {
        let (t, rt, exe) = pack();
        let e = plan(None, &exe, &[]).unwrap_err();
        assert_eq!(e.code, EXIT_RUNTIME);
        assert!(e.to_string().starts_with("zf-pyshim: ZEROFARM_RUNTIME_DIR is not set"), "{e}");
        assert_eq!(plan(Some("".into()), &exe, &[]).unwrap_err().code, EXIT_RUNTIME);
        let e = plan(Some("relative\\dir".into()), &exe, &[]).unwrap_err();
        assert!(e.message.contains("absolute"), "{e}");
        let e = plan(Some(t.path().join("nowhere").into_os_string()), &exe, &[]).unwrap_err();
        assert!(e.message.contains("is not a directory"), "{e}");
        // a pack without python (the CUDA pack, say)
        let other = t.path().join("cuda");
        std::fs::create_dir_all(&other).unwrap();
        let e = plan(Some(other.into_os_string()), &exe, &[]).unwrap_err();
        assert_eq!(e.code, EXIT_RUNTIME);
        assert!(e.message.contains("has no python"), "{e}");
        // a build without the script
        std::fs::remove_file(exe.parent().unwrap().join(SCRIPT)).unwrap();
        let e = plan(Some(rt.into_os_string()), &exe, &[]).unwrap_err();
        assert_eq!(e.code, EXIT_SCRIPT);
        assert!(e.message.contains("no worker_main.py"), "{e}");
    }
}

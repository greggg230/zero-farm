//! zf-pyshim.exe: see the library (`lib.rs`) and PROTOCOL 19.4.

fn main() {
    let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("zf-pyshim: the shim's own path is unknown: {e}");
            std::process::exit(zf_pyshim::EXIT_SCRIPT);
        }
    };
    let code = match zf_pyshim::plan(std::env::var_os(zf_pyshim::RUNTIME_ENV), &exe, &args).and_then(|p| zf_pyshim::run(&p)) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            e.code
        }
    };
    std::process::exit(code);
}

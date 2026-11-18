//! farm-agent.exe
//!
//! ```text
//! farm-agent run       [--root DIR] [--headless] [--coordinator URL]   (what the launcher starts)
//! farm-agent dry-run   [--root DIR] [--seconds N] [--every N]   log idle-policy decisions (and per-GPU readings) only; no work
//! farm-agent install   [--root DIR] [--reg-name NAME] [--shortcut-dir DIR] [--no-shortcut] [--no-start] [--quiet]
//! farm-agent uninstall [--root DIR] [--reg-name NAME] [--shortcut-dir DIR] [--quiet]
//! farm-agent enroll    --invite CODE [--name NAME] [--root DIR] [--coordinator URL]
//! farm-agent profile | keys | version
//! ```
//!
//! `keys` prints every trusted key, one per line: `release <key id> <public key>` (manifests) and
//! `data <key id> <public key>` (definitions); `farm-release release-agent` compares them with
//! `common/trusted_keys.txt` and `common/trusted_data_keys.txt` before it signs this binary.
//!
//! Exit codes: 0 quit, 75 switch to `state\pending` (launcher), 76 hand over to the new
//! `ZeroFarm.exe` (launcher), 2 usage, 3 not enrolled (headless), 4 revoked, 70 internal error.
//!
//! macOS (PROTOCOL 11.5; no tray: `run` is always headless and waits for an enrollment instead of exiting 3):
//!
//! ```text
//! farm-agent setup     [--source DIR] [--invite CODE] [--name NAME] [--root DIR] [--coordinator URL]
//!                      [--launch-agents-dir DIR] [--label L] [--no-launchctl] [--no-start]
//! farm-agent install   [--source DIR] [--root DIR] [--launch-agents-dir DIR] [--label L] [--no-launchctl] [--no-start]
//! farm-agent uninstall [--root DIR] [--launch-agents-dir DIR] [--label L] [--no-launchctl]
//! farm-agent status | pause [MINUTES] | resume | set KEY VALUE   [--root DIR]   (the launcher's CLI forwards these)
//! ```

#![windows_subsystem = "windows"]

/// macOS: this binary's version, which `farm-release release-agent --platform macos-arm64` reads from the file (it
/// can't run a Mac binary on the Windows desktop; `zerofarm_common::binver`).
#[cfg(not(windows))]
#[used]
static VERSION_MARK: [u8; zerofarm_common::binver::MARK_LEN] = zerofarm_common::binver::mark(env!("CARGO_PKG_VERSION"));

#[cfg(windows)]
use farm_agent::engine::EngineExit;
use farm_agent::engine::{Control, Engine};
use farm_agent::settings::Machine;
use farm_agent::signals::SignalSource;
#[cfg(windows)]
use farm_agent::signals::Win32Signals;
#[cfg(windows)]
use farm_agent::status::Phase;
use farm_agent::status::Status;
use farm_agent::{agent_identity, install, test_env, AgentConfig, Timings, DEFAULT_COORDINATOR};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use zerofarm_common::keys::TrustedKeys;
use zerofarm_common::layout::{default_root, Layout};

struct Args {
    cmd: String,
    flags: Vec<(String, Option<String>)>,
    /// Arguments after the command that are not flags (macOS CLI: `pause 30`, `set cpu_threads 12`).
    #[cfg_attr(windows, allow(dead_code))]
    pos: Vec<String>,
}

/// macOS: flags that never take a value (so `--no-launchctl pause` can't swallow what follows). None on Windows,
/// whose parsing stays as it was.
const SWITCHES: &[&str] = if cfg!(windows) { &[] } else { &["no-start", "no-launchctl", "quiet", "headless"] };

impl Args {
    fn parse() -> Args {
        let raw: Vec<String> = std::env::args().skip(1).collect();
        let mut it = raw.into_iter().peekable();
        let cmd = match it.peek() {
            Some(a) if !a.starts_with("--") => it.next().unwrap(),
            Some(a) if a == "--version" => {
                it.next();
                "version".into()
            }
            _ => "run".into(),
        };
        let mut flags = Vec::new();
        let mut pos = Vec::new();
        while let Some(a) = it.next() {
            if let Some(name) = a.strip_prefix("--") {
                let value = match it.peek() {
                    Some(v) if !v.starts_with("--") && !SWITCHES.contains(&name) => it.next(),
                    _ => None,
                };
                flags.push((name.to_string(), value));
            } else {
                pos.push(a);
            }
        }
        Args { cmd, flags, pos }
    }
    fn get(&self, name: &str) -> Option<String> {
        self.flags.iter().find(|(n, _)| n == name).and_then(|(_, v)| v.clone())
    }
    fn has(&self, name: &str) -> bool {
        self.flags.iter().any(|(n, _)| n == name)
    }
    /// Everything except the given flags, to pass along.
    #[cfg_attr(not(windows), allow(dead_code))]
    fn passthrough(&self, except: &[&str]) -> Vec<String> {
        let mut v = Vec::new();
        for (n, val) in &self.flags {
            if except.contains(&n.as_str()) {
                continue;
            }
            v.push(format!("--{n}"));
            if let Some(x) = val {
                v.push(x.clone());
            }
        }
        v
    }
}

fn exe_dir() -> PathBuf {
    std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf)).unwrap_or_default()
}

/// `--root`, else `<root>` when we run from `<root>\versions\<v>\`, else the default.
fn root(args: &Args) -> PathBuf {
    if let Some(r) = args.get("root") {
        return PathBuf::from(r);
    }
    let d = exe_dir();
    if let Some(versions) = d.parent() {
        if versions.file_name().map(|n| n.eq_ignore_ascii_case("versions")).unwrap_or(false) {
            if let Some(r) = versions.parent() {
                return r.to_path_buf();
            }
        }
    }
    default_root()
}

#[cfg(windows)]
fn attach_console() {
    use windows_sys::Win32::System::Console::{AttachConsole, ATTACH_PARENT_PROCESS};
    unsafe { AttachConsole(ATTACH_PARENT_PROCESS) };
}

#[cfg(windows)]
fn say(quiet: bool, gui: bool, text: &str, error: bool) {
    println!("{text}");
    if gui && !quiet {
        use windows_sys::Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MB_ICONINFORMATION, MB_OK};
        farm_agent::tray::message_box(std::ptr::null_mut(), text, "Zero Farm", MB_OK | if error { MB_ICONERROR } else { MB_ICONINFORMATION });
    }
}


fn coordinator(args: &Args, machine: &Machine) -> String {
    args.get("coordinator")
        .or_else(|| test_env("ZEROFARM_TEST_COORDINATOR"))
        .or_else(|| machine.coordinator.clone())
        .unwrap_or_else(|| DEFAULT_COORDINATOR.to_string())
}

fn config(args: &Args, layout: &Layout, machine: &Machine) -> AgentConfig {
    let (version, build_id) = agent_identity(&exe_dir());
    AgentConfig {
        layout: layout.clone(),
        coordinator: coordinator(args, machine),
        version,
        build_id,
        keys: TrustedKeys::compiled(),
        data_keys: TrustedKeys::compiled_data(),
        timings: Timings::default(),
        kinds: vec!["fuzz".into(), "eval".into(), "selfplay".into(), "diag".into()],
        channel: machine.channel.clone(),
        launcher_level: std::env::var(zerofarm_common::LAUNCHER_ENV).ok().and_then(|v| v.parse().ok()).unwrap_or(0),
        test_spawn_error: None,
    }
}

fn cmd_run(args: &Args) -> i32 {
    let layout = Layout::new(root(args));
    let headless = args.has("headless") || test_env("ZEROFARM_TEST_HEADLESS").is_some();
    farm_agent::logging::init(&layout, false);
    let (version, build_id) = agent_identity(&exe_dir());
    farm_agent::logging::install_panic_hook(layout.clone(), version.clone(), build_id.clone());
    let Some(_instance) = zerofarm_common::names::single_instance("agent", &layout.root) else {
        log::info!("another agent is running for {}; exiting", layout.root.display());
        return 0;
    };
    let machine = Machine::load(&layout);
    let cfg = config(args, &layout, &machine);
    log::info!("farm-agent {} (build {:?}) starting, root {}, headless {headless}", cfg.version, cfg.build_id, layout.root.display());
    if cfg.keys.is_empty() {
        log::error!("this build trusts no release key: no work or update can be verified");
    }
    if cfg.data_keys.is_empty() {
        log::error!("this build trusts no data key: no definition can be verified, so only legacy leases can run");
    }
    let profile_root = layout.root.clone();
    let profile = farm_agent::profile::detect(&profile_root);
    let machine = Arc::new(Mutex::new(machine));
    let status = Arc::new(Mutex::new(Status { version: cfg.version.clone(), ..Status::default() }));
    let control = Arc::new(Control::default());

    let make_engine = {
        let (cfg, machine, status, control) = (cfg.clone(), machine.clone(), status.clone(), control.clone());
        move || -> Result<Engine, String> {
            let token = machine.lock().unwrap().token().ok_or("not enrolled")?;
            let root = cfg.layout.root.clone();
            Engine::new(
                cfg.clone(),
                token,
                machine.clone(),
                status.clone(),
                os_signals(&machine),
                control.clone(),
                Box::new(move || farm_agent::profile::detect(&root)),
            )
        }
    };

    // macOS (PROTOCOL 11.5): no tray; status, settings and quit go through `state/` and signals (headless.rs)
    #[cfg(target_os = "macos")]
    {
        let _ = (headless, profile);
        let start: farm_agent::headless::StartEngine = Box::new(move || {
            let mut eng = make_engine()?;
            Ok(std::thread::Builder::new().name("engine".into()).spawn(move || eng.run()).map_err(|e| e.to_string())?)
        });
        farm_agent::headless::run(machine, status, control, layout, farm_agent::headless::Cores::detect(), start)
    }
    #[cfg(windows)]
    run_windows(headless, layout, cfg, profile, machine, status, control, make_engine)
}

#[cfg(windows)]
fn os_signals(_machine: &farm_agent::settings::SharedMachine) -> Box<dyn SignalSource> {
    Box::new(Win32Signals::new(true))
}

#[cfg(target_os = "macos")]
fn os_signals(machine: &farm_agent::settings::SharedMachine) -> Box<dyn SignalSource> {
    Box::new(farm_agent::signals::MacSignals::new(Some(machine.clone())))
}

/// The Windows agent: headless (`--headless`), or the invite dialog when needed and then the tray.
#[cfg(windows)]
#[allow(clippy::too_many_arguments)]
fn run_windows(
    headless: bool,
    layout: Layout,
    cfg: AgentConfig,
    profile: zerofarm_common::protocol::Profile,
    machine: farm_agent::settings::SharedMachine,
    status: farm_agent::status::SharedStatus,
    control: Arc<Control>,
    make_engine: impl Fn() -> Result<Engine, String> + 'static,
) -> i32 {
    if headless {
        if !machine.lock().unwrap().enrolled() {
            log::error!("not enrolled; run `farm-agent enroll --invite CODE`");
            return 3;
        }
        let mut eng = match make_engine() {
            Ok(e) => e,
            Err(e) => {
                log::error!("{e}");
                return 70;
            }
        };
        return match eng.run() {
            EngineExit::Quit => 0,
            EngineExit::Update(_) => zerofarm_common::EXIT_UPDATE,
            EngineExit::RestartLauncher => zerofarm_common::EXIT_RESTART_LAUNCHER,
            EngineExit::Revoked => {
                let mut m = machine.lock().unwrap();
                m.forget_enrollment();
                let _ = m.save(&layout);
                4
            }
        };
    }

    // Interactive: invite dialog first if needed, then the tray.
    if !machine.lock().unwrap().enrolled() {
        farm_agent::status::set(&status, |s| s.phase = Phase::NotEnrolled);
        let name = farm_agent::enroll::default_machine_name();
        let (lay, coord, ver, m2, prof) = (layout.clone(), cfg.coordinator.clone(), cfg.version.clone(), machine.clone(), profile.clone());
        let mut try_enroll = move |code: &str, name: &str| -> Result<(), String> {
            let mut m = m2.lock().unwrap();
            farm_agent::enroll::enroll(&lay, &coord, &ver, &mut m, code, name, &prof).map_err(|e| e.to_string())
        };
        if !farm_agent::tray::invite_dialog(&name, &mut try_enroll) {
            log::info!("invite dialog cancelled");
            return 0;
        }
        log::info!("enrolled as {:?}", machine.lock().unwrap().machine_id);
    }
    let start: farm_agent::tray::StartEngine = Box::new(move || {
        let mut eng = make_engine()?;
        Ok(std::thread::Builder::new().name("engine".into()).spawn(move || eng.run()).map_err(|e| e.to_string())?)
    });
    let send_diag: farm_agent::tray::SendDiag = {
        let (machine, status, layout, coord, version) = (machine.clone(), status.clone(), layout.clone(), cfg.coordinator.clone(), cfg.version.clone());
        std::sync::Arc::new(move || {
            let (token, settings) = {
                let m = machine.lock().unwrap();
                (m.token(), serde_json::to_string(&m.settings).unwrap_or_default())
            };
            let Some(token) = token else { return "This PC isn't part of the farm yet.".to_string() };
            let client = match zerofarm_common::http::Client::new(&coord, Some(token), &version) {
                Ok(c) => c,
                Err(e) => return format!("Couldn't send diagnostics: {e}"),
            };
            let headline = status.lock().map(|s| s.headline()).unwrap_or_default();
            let header = format!(
                "Zero Farm {version}, sent from the tray at {}\nstatus: {headline}\nsettings: {settings}",
                zerofarm_common::rotlog::utc_timestamp(zerofarm_common::now_unix())
            );
            farm_agent::logging::user_diag_message(&farm_agent::logging::send_user_diag(&client, &layout, &header))
        })
    };
    farm_agent::tray::run(machine, status, control, layout, cfg.version, profile.cores, profile.threads, start, send_diag)
}

fn cmd_dry_run(args: &Args) -> i32 {
    let layout = Layout::new(root(args));
    farm_agent::logging::init(&layout, true);
    let machine = Machine::load(&layout);
    let secs: u64 = args.get("seconds").and_then(|s| s.parse().ok()).unwrap_or(u64::MAX);
    // Log at least this often even when the decision doesn't change (with the per-GPU readings).
    let every: u64 = args.get("every").and_then(|s| s.parse().ok()).unwrap_or(60).max(1);
    #[cfg(windows)]
    let mut signals = Win32Signals::new(true);
    #[cfg(target_os = "macos")]
    let mut signals = farm_agent::signals::MacSignals::new(None);
    let mut st = farm_agent::policy::PolicyState::default();
    let start = std::time::Instant::now();
    let mut last: Option<String> = None;
    let mut last_log = std::time::Instant::now();
    log::info!(
        "dry run: logging decisions only (no leases, no work); GPU Engine counters {}",
        if signals.has_busy_counters() { "available" } else { "missing" }
    );
    while start.elapsed().as_secs() < secs {
        let t0 = std::time::Instant::now();
        let s = signals.sample(None);
        let took_ms = t0.elapsed().as_millis();
        let d = farm_agent::policy::decide(&mut st, &s, &machine.settings);
        let line = format!(
            "work={} block={:?} gpu={} gpu_block={:?} mem_pressure={} | idle_s={} fullscreen={} on_ac={:?} saver={} mem={}/{} MB gpu={:?} (sampled in {took_ms} ms)",
            d.work, d.block, d.gpu, d.gpu_block, d.mem_pressure, s.idle_s, s.notification.is_fullscreen(), s.on_ac, s.battery_saver,
            s.mem_avail_mb, s.mem_total_mb, s.gpu
        );
        let key = format!("{} {:?} {} {:?}", d.work, d.block, d.gpu, d.gpu_block);
        if last.as_deref() != Some(&key) || last_log.elapsed().as_secs() >= every {
            log::info!("decision: {line}");
            for r in signals.gpu_readings() {
                let temp = r.temp_c.map_or_else(|| "none".to_string(), |t| format!("{t} C"));
                log::info!(
                    "  gpu luid {} {} ({}): other use {} %, temperature {temp}",
                    farm_agent::profile::luid_hex(r.luid),
                    r.vendor,
                    r.name,
                    r.other_util_pct
                );
            }
            last = Some(key);
            last_log = std::time::Instant::now();
        }
        std::thread::sleep(std::time::Duration::from_secs(1).min(std::time::Duration::from_secs(secs.saturating_sub(start.elapsed().as_secs()).max(1))));
    }
    0
}

#[cfg(windows)]
fn reg_name(args: &Args) -> String {
    args.get("reg-name").unwrap_or_else(|| install::DEFAULT_REG_NAME.to_string())
}

#[cfg(windows)]
fn cmd_install(args: &Args) -> i32 {
    let quiet = args.has("quiet");
    let opts = install::InstallOpts {
        root: args.get("root").map(PathBuf::from).unwrap_or_else(default_root),
        reg_name: reg_name(args),
        shortcut_dir: args.get("shortcut-dir").map(PathBuf::from),
        no_shortcut: args.has("no-shortcut"),
        start: !args.has("no-start"),
        source: args.get("source").map(PathBuf::from).unwrap_or_else(exe_dir),
    };
    match install::install(&opts, &TrustedKeys::compiled()) {
        Ok(v) => {
            say(quiet, true, &format!("Zero Farm {v} is installed. It lives in the notification area (bottom right)."), false);
            0
        }
        Err(e) => {
            say(quiet, true, &format!("Zero Farm could not be installed: {e}"), true);
            1
        }
    }
}

#[cfg(windows)]
fn cmd_uninstall(args: &Args) -> i32 {
    let quiet = args.has("quiet");
    let opts = install::UninstallOpts {
        root: args.get("root").map(PathBuf::from).unwrap_or_else(|| root(args)),
        reg_name: reg_name(args),
        shortcut_dir: args.get("shortcut-dir").map(PathBuf::from),
    };
    if !args.has("stage2") {
        match install::hand_off_if_inside(&opts, &args.passthrough(&["root", "reg-name", "shortcut-dir", "stage2"])) {
            Ok(true) => return 0,
            Ok(false) => {}
            Err(e) => {
                say(quiet, true, &format!("Zero Farm could not be removed: {e}"), true);
                return 1;
            }
        }
    }
    let r = install::uninstall(&opts);
    if args.has("stage2") {
        install::schedule_self_delete();
    }
    match r {
        Ok(()) => {
            say(quiet, true, "Zero Farm was removed from this PC.", false);
            0
        }
        Err(e) => {
            say(quiet, true, &format!("Zero Farm could not be removed: {e}"), true);
            1
        }
    }
}

#[cfg(target_os = "macos")]
fn mac_install_opts(args: &Args, start: bool) -> install::InstallOpts {
    install::InstallOpts {
        root: args.get("root").map(PathBuf::from).unwrap_or_else(default_root),
        launch_agents_dir: args.get("launch-agents-dir").map(PathBuf::from),
        label: args.get("label").unwrap_or_else(|| install::DEFAULT_LABEL.to_string()),
        launchctl: !args.has("no-launchctl"),
        start,
        source: args.get("source").map(PathBuf::from).unwrap_or_else(exe_dir),
    }
}

#[cfg(target_os = "macos")]
fn cmd_install(args: &Args) -> i32 {
    let opts = mac_install_opts(args, !args.has("no-start"));
    match install::install(&opts, &TrustedKeys::compiled()) {
        Ok(v) => {
            println!("Zero Farm {v} is installed in {}.", opts.root.display());
            0
        }
        Err(e) => {
            eprintln!("Zero Farm could not be installed: {e}");
            1
        }
    }
}

#[cfg(target_os = "macos")]
fn cmd_uninstall(args: &Args) -> i32 {
    let opts = install::UninstallOpts {
        root: args.get("root").map(PathBuf::from).unwrap_or_else(|| root(args)),
        launch_agents_dir: args.get("launch-agents-dir").map(PathBuf::from),
        label: args.get("label").unwrap_or_else(|| install::DEFAULT_LABEL.to_string()),
        launchctl: !args.has("no-launchctl"),
    };
    match install::uninstall(&opts) {
        Ok(()) => {
            println!("Zero Farm was removed from this Mac.");
            0
        }
        Err(e) => {
            eprintln!("Zero Farm could not be removed: {e}");
            1
        }
    }
}

/// Reads one line from the terminal (`/dev/tty`: `curl ... | sh` leaves stdin to the script); None without one.
#[cfg(target_os = "macos")]
fn ask(prompt: &str) -> Option<String> {
    use std::io::{BufRead, Write};
    let tty = std::fs::OpenOptions::new().read(true).write(true).open("/dev/tty").ok()?;
    let mut w = tty.try_clone().ok()?;
    write!(w, "{prompt}").ok()?;
    w.flush().ok()?;
    let mut line = String::new();
    std::io::BufReader::new(tty).read_line(&mut line).ok()?;
    Some(line.trim().to_string())
}

/// `ZeroFarm setup` (PROTOCOL 11.2, 11.5): installs the package it runs from (the launcher verified it), enrolls this
/// Mac (the invite code and name from the arguments, else asked on the terminal) and starts it.
#[cfg(target_os = "macos")]
fn cmd_setup(args: &Args) -> i32 {
    let opts = mac_install_opts(args, false);
    let v = match install::install(&opts, &TrustedKeys::compiled()) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("Zero Farm could not be installed: {e}");
            return 1;
        }
    };
    let root = install::clean_root(&opts.root);
    let layout = Layout::new(&root);
    let launcher = layout.launcher_exe();
    let mut machine = Machine::load(&layout);
    if let Some(c) = args.get("coordinator") {
        machine.coordinator = Some(c);
    }
    if !machine.enrolled() {
        let cfg = config(args, &layout, &machine);
        let profile = farm_agent::profile::detect(&layout.root);
        let default_name = farm_agent::enroll::default_machine_name();
        let mut code = args.get("invite");
        let mut name = args.get("name");
        let mut tries = 0;
        loop {
            let c = match code.take().or_else(|| ask("Invite code (XXXX-XXXX-XXXX): ")) {
                Some(c) if !c.is_empty() => c,
                _ => {
                    eprintln!("No invite code: run `{} setup --invite CODE` (Zero Farm is installed but not started).", launcher.display());
                    return 2;
                }
            };
            let n = match name.clone().or_else(|| ask(&format!("A name for this Mac on the farm's pages [{default_name}]: "))) {
                Some(n) if !n.trim().is_empty() => n.trim().to_string(),
                _ => default_name.clone(),
            };
            name = Some(n.clone());
            match farm_agent::enroll::enroll(&layout, &cfg.coordinator, &cfg.version, &mut machine, &c, &n, &profile) {
                Ok(()) => {
                    println!("Joined the farm as {n} ({}).", machine.machine_id.clone().unwrap_or_default());
                    break;
                }
                Err(e @ farm_agent::enroll::EnrollError::UpdateRequired { .. }) => {
                    eprintln!("{e}");
                    return 1;
                }
                Err(e) => {
                    eprintln!("{e}");
                    tries += 1;
                    if tries >= 3 || args.get("invite").is_some() {
                        return 1;
                    }
                }
            }
        }
    }
    if args.has("no-start") {
        println!("Zero Farm {v} is installed in {} (not started: it starts at the next login).", root.display());
        return 0;
    }
    if let Err(e) = install::start(&opts) {
        eprintln!("Zero Farm {v} is installed but could not be started: {e}");
        return 1;
    }
    println!("Zero Farm {v} is installed and running. It works while this Mac is idle and on power, and starts at login.");
    println!("  status:    \"{}\" status", launcher.display());
    println!("  settings:  \"{}\" set <key> <value>   (keys: {})", launcher.display(), farm_agent::settings::Settings::KEYS.join(", "));
    println!("  pause:     \"{}\" pause [minutes]   /   resume   /   quit   /   uninstall", launcher.display());
    0
}

fn cmd_enroll(args: &Args) -> i32 {
    let layout = Layout::new(root(args));
    let Some(code) = args.get("invite") else {
        println!("usage: farm-agent enroll --invite XXXX-XXXX-XXXX [--name NAME]");
        return 2;
    };
    let mut machine = Machine::load(&layout);
    let cfg = config(args, &layout, &machine);
    let name = args.get("name").unwrap_or_else(farm_agent::enroll::default_machine_name);
    let profile = farm_agent::profile::detect(&layout.root);
    if let Some(c) = args.get("coordinator") {
        machine.coordinator = Some(c);
    }
    match farm_agent::enroll::enroll(&layout, &cfg.coordinator, &cfg.version, &mut machine, &code, &name, &profile) {
        Ok(()) => {
            println!("enrolled as {}", machine.machine_id.clone().unwrap_or_default());
            0
        }
        Err(e) => {
            println!("{e}");
            1
        }
    }
}

fn main() {
    #[cfg(windows)]
    attach_console();
    // Inherited by the worker: no "program has stopped working" / missing-DLL dialogs.
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::System::Diagnostics::Debug::{SetErrorMode, SEM_FAILCRITICALERRORS, SEM_NOGPFAULTERRORBOX};
        SetErrorMode(SEM_FAILCRITICALERRORS | SEM_NOGPFAULTERRORBOX);
    }
    let args = Args::parse();
    let code = match args.cmd.as_str() {
        "run" => cmd_run(&args),
        "dry-run" => cmd_dry_run(&args),
        "install" => cmd_install(&args),
        "uninstall" => cmd_uninstall(&args),
        "enroll" => cmd_enroll(&args),
        // macOS: the headless CLI (the launcher forwards `ZeroFarm setup|status|pause|resume|set` here)
        #[cfg(target_os = "macos")]
        "setup" => cmd_setup(&args),
        #[cfg(target_os = "macos")]
        "status" => farm_agent::headless::cli_status(&Layout::new(root(&args))),
        #[cfg(target_os = "macos")]
        "pause" | "resume" | "set" => {
            use farm_agent::headless::Request;
            let req = match (args.cmd.as_str(), args.pos.as_slice()) {
                ("pause", []) => Some(Request { op: "pause".into(), key: None, value: None, minutes: None }),
                ("pause", [m]) => m.parse().ok().map(|m| Request { op: "pause".into(), key: None, value: None, minutes: Some(m) }),
                ("resume", []) => Some(Request { op: "resume".into(), key: None, value: None, minutes: None }),
                ("set", [k, v]) => Some(Request { op: "set".into(), key: Some(k.clone()), value: Some(v.clone()), minutes: None }),
                _ => None,
            };
            match req {
                Some(r) => farm_agent::headless::cli_change(&Layout::new(root(&args)), &r),
                None => {
                    eprintln!(
                        "usage: ZeroFarm pause [minutes] | resume | set <key> <value> (keys: {})",
                        farm_agent::settings::Settings::KEYS.join(", ")
                    );
                    2
                }
            }
        }
        "version" => {
            println!("{}", agent_identity(&exe_dir()).0);
            0
        }
        "keys" => {
            for k in TrustedKeys::compiled().keys() {
                println!("release {} {}", k.key_id_hex(), k.b64);
            }
            for k in TrustedKeys::compiled_data().keys() {
                println!("data {} {}", k.key_id_hex(), k.b64);
            }
            0
        }
        "profile" => {
            let layout = Layout::new(root(&args));
            println!("{}", serde_json::to_string_pretty(&farm_agent::profile::detect(&layout.root)).unwrap());
            0
        }
        other => {
            println!("unknown command {other:?}; see the farm-agent.exe header for usage");
            2
        }
    };
    std::process::exit(code);
}

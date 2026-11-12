//! farm-agent.exe
//!
//! ```text
//! farm-agent run       [--root DIR] [--headless] [--coordinator URL]   (what the launcher starts)
//! farm-agent dry-run   [--root DIR] [--seconds N]    log idle-policy decisions only; no work
//! farm-agent install   [--root DIR] [--reg-name NAME] [--shortcut-dir DIR] [--no-shortcut] [--no-start] [--quiet]
//! farm-agent uninstall [--root DIR] [--reg-name NAME] [--shortcut-dir DIR] [--quiet]
//! farm-agent enroll    --invite CODE [--name NAME] [--root DIR] [--coordinator URL]
//! farm-agent profile | keys | version
//! ```
//!
//! Exit codes: 0 quit, 75 switch to `state\pending` (launcher), 76 hand over to the new
//! `ZeroFarm.exe` (launcher), 2 usage, 3 not enrolled (headless), 4 revoked, 70 internal error.

#![windows_subsystem = "windows"]

use farm_agent::engine::{Control, Engine, EngineExit};
use farm_agent::settings::Machine;
use farm_agent::signals::{SignalSource, Win32Signals};
use farm_agent::status::{Phase, Status};
use farm_agent::{agent_identity, install, test_env, AgentConfig, Timings, DEFAULT_COORDINATOR};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use zerofarm_common::keys::TrustedKeys;
use zerofarm_common::layout::{default_root, Layout};

struct Args {
    cmd: String,
    flags: Vec<(String, Option<String>)>,
}

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
        while let Some(a) = it.next() {
            if let Some(name) = a.strip_prefix("--") {
                let value = match it.peek() {
                    Some(v) if !v.starts_with("--") => it.next(),
                    _ => None,
                };
                flags.push((name.to_string(), value));
            }
        }
        Args { cmd, flags }
    }
    fn get(&self, name: &str) -> Option<String> {
        self.flags.iter().find(|(n, _)| n == name).and_then(|(_, v)| v.clone())
    }
    fn has(&self, name: &str) -> bool {
        self.flags.iter().any(|(n, _)| n == name)
    }
    /// Everything except the given flags, to pass along.
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

fn attach_console() {
    use windows_sys::Win32::System::Console::{AttachConsole, ATTACH_PARENT_PROCESS};
    unsafe { AttachConsole(ATTACH_PARENT_PROCESS) };
}

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
        timings: Timings::default(),
        kinds: vec!["fuzz".into(), "eval".into(), "selfplay".into(), "diag".into()],
        channel: machine.channel.clone(),
        launcher_level: std::env::var(zerofarm_common::LAUNCHER_ENV).ok().and_then(|v| v.parse().ok()).unwrap_or(0),
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
                Box::new(Win32Signals::new(true)) as Box<dyn SignalSource>,
                control.clone(),
                Box::new(move || farm_agent::profile::detect(&root)),
            )
        }
    };

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
    let mut signals = Win32Signals::new(true);
    let mut st = farm_agent::policy::PolicyState::default();
    let start = std::time::Instant::now();
    let mut last: Option<String> = None;
    let mut last_log = std::time::Instant::now();
    log::info!("dry run: logging decisions only (no leases, no work); nvml {}", signals.has_nvml());
    while start.elapsed().as_secs() < secs {
        let s = signals.sample(None);
        let d = farm_agent::policy::decide(&mut st, &s, &machine.settings);
        let line = format!(
            "work={} block={:?} gpu={} gpu_block={:?} mem_pressure={} | idle_s={} fullscreen={} on_ac={:?} saver={} mem={}/{} MB gpu={:?}",
            d.work, d.block, d.gpu, d.gpu_block, d.mem_pressure, s.idle_s, s.notification.is_fullscreen(), s.on_ac, s.battery_saver,
            s.mem_avail_mb, s.mem_total_mb, s.gpu
        );
        let key = format!("{} {:?} {} {:?}", d.work, d.block, d.gpu, d.gpu_block);
        if last.as_deref() != Some(&key) || last_log.elapsed().as_secs() >= 60 {
            log::info!("decision: {line}");
            last = Some(key);
            last_log = std::time::Instant::now();
        }
        std::thread::sleep(std::time::Duration::from_secs(1).min(std::time::Duration::from_secs(secs.saturating_sub(start.elapsed().as_secs()).max(1))));
    }
    0
}

fn reg_name(args: &Args) -> String {
    args.get("reg-name").unwrap_or_else(|| install::DEFAULT_REG_NAME.to_string())
}

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
    attach_console();
    // Inherited by the worker: no "program has stopped working" / missing-DLL dialogs.
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
        "version" => {
            println!("{}", agent_identity(&exe_dir()).0);
            0
        }
        "keys" => {
            for k in TrustedKeys::compiled().keys() {
                println!("{} {}", k.key_id_hex(), k.b64);
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

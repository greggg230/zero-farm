//! `state\machine.json`: machine id, the DPAPI-protected token, the channel, and the friend's
//! settings (DESIGN 3.2: all user-adjustable; defaults below).

use crate::util::local_now;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};
#[cfg(windows)]
use zerofarm_common::fsutil::atomic_write;
use zerofarm_common::layout::Layout;

pub const TOKEN_PURPOSE: &str = "zero-farm machine token";

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// Work only after the PC has been idle (default).
    IdleOnly,
    /// Work even while the user is active, at idle priority.
    Always,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum Pause {
    None,
    Until { t: i64 },
    Indefinitely,
}

/// Optional working hours, local time, minutes since midnight; may wrap midnight.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hours {
    pub start_min: u32,
    pub end_min: u32,
}

impl Hours {
    pub fn contains(&self, min: u32) -> bool {
        if self.start_min <= self.end_min {
            min >= self.start_min && min < self.end_min
        } else {
            min >= self.start_min || min < self.end_min
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Settings {
    pub mode: Mode,
    pub pause: Pause,
    pub gpu: bool,
    /// CPU threads for work; `None` = physical cores (at least 1, at most the logical threads).
    pub cpu_threads: Option<u32>,
    pub idle_minutes: u32,
    pub hours: Option<Hours>,
    /// RAM cap for the worker; `None` = half of physical RAM.
    pub max_ram_mb: Option<u64>,
    /// Hard disk cap for everything under the install root (DESIGN 3.5).
    pub disk_cap_mb: u64,
    /// Never work while a fullscreen app / game / presentation runs (also in Always mode).
    pub respect_fullscreen: bool,
    /// rev 17: work on battery power and with battery saver / Low Power Mode on too (default off: never). For an
    /// owner's own laptop (macOS: `ZeroFarm set on_battery true`); written to `machine.json` only when on.
    #[serde(skip_serializing_if = "is_false")]
    pub on_battery: bool,
    /// rev 17 (PROTOCOL 11.5, macOS): while working on AC power, keep the Mac from idle-sleeping (default on);
    /// written to `machine.json` only when off.
    #[serde(skip_serializing_if = "is_true")]
    pub keep_awake: bool,
    /// rev 19 (PROTOCOL 13): measure and tune the CPU layout of self-play jobs on this machine (default on); written
    /// to `machine.json` only when off. Off: the fixed rules of before (threads = `cpu_threads`, the worker's own
    /// inference default, rolling "auto"'s games in flight).
    #[serde(skip_serializing_if = "is_true")]
    pub autotune: bool,
    /// rev 19 (PROTOCOL 13.5): worker processes per self-play job: `None` = auto (the tuner's choice, from what the
    /// cores and memory allow; one with `autotune` off), else 1 to 3 (with `autotune` off a fixed layout of that many);
    /// written to `machine.json` only when set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workers: Option<u32>,
}

fn is_false(b: &bool) -> bool {
    !*b
}

fn is_true(b: &bool) -> bool {
    *b
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            mode: Mode::IdleOnly,
            pause: Pause::None,
            gpu: true,
            cpu_threads: None,
            idle_minutes: 5,
            hours: None,
            max_ram_mb: None,
            disk_cap_mb: 3072,
            respect_fullscreen: true,
            on_battery: false,
            keep_awake: true,
            autotune: true,
            workers: None,
        }
    }
}

impl Settings {
    pub fn paused(&self, now: i64) -> bool {
        match self.pause {
            Pause::None => false,
            Pause::Until { t } => now < t,
            Pause::Indefinitely => true,
        }
    }

    pub fn threads(&self, physical_cores: u32, logical: u32) -> u32 {
        let auto = physical_cores.max(1);
        self.cpu_threads.unwrap_or(auto).clamp(1, logical.max(1))
    }

    pub fn ram_cap_mb(&self, total_mb: u64) -> u64 {
        self.max_ram_mb.unwrap_or(total_mb / 2)
    }

    /// The keys `ZeroFarm set <key> <value>` takes (macOS, PROTOCOL 11.5; the tray's settings on Windows).
    pub const KEYS: &'static [&'static str] =
        &["cpu_threads", "work_when", "mode", "idle_minutes", "on_battery", "keep_awake", "hours", "max_ram_mb", "disk_cap_mb", "gpu", "autotune", "workers"];

    /// `set <key> <value>`: changes one setting, or says why not. Returns how it reads now.
    pub fn set_by_name(&mut self, key: &str, value: &str) -> Result<String, String> {
        let v = value.trim().to_ascii_lowercase();
        let flag = |v: &str| match v {
            "true" | "on" | "yes" | "1" => Ok(true),
            "false" | "off" | "no" | "0" => Ok(false),
            _ => Err(format!("{key}: true or false")),
        };
        let number = |v: &str, lo: u64, hi: u64| -> Result<u64, String> {
            v.parse::<u64>().ok().filter(|n| (lo..=hi).contains(n)).ok_or_else(|| format!("{key}: a number from {lo} to {hi}"))
        };
        let auto = |v: &str| matches!(v, "auto" | "automatic" | "default");
        match key {
            "cpu_threads" | "threads" => self.cpu_threads = if auto(&v) { None } else { Some(number(&v, 1, 1024)? as u32) },
            "work_when" | "mode" => {
                self.mode = match v.as_str() {
                    "idle" | "idle_only" | "away" => Mode::IdleOnly,
                    "active" | "always" => Mode::Always,
                    _ => return Err(format!("{key}: idle (only while the computer is not in use) or active (also while it is)")),
                }
            }
            "idle_minutes" => self.idle_minutes = number(&v, 1, 240)? as u32,
            "on_battery" => self.on_battery = flag(&v)?,
            "keep_awake" => self.keep_awake = flag(&v)?,
            "autotune" => self.autotune = flag(&v)?,
            "workers" => self.workers = if auto(&v) { None } else { Some(number(&v, 1, crate::tune::MAX_WORKERS as u64)? as u32) },
            "gpu" => self.gpu = flag(&v)?,
            "max_ram_mb" => self.max_ram_mb = if auto(&v) { None } else { Some(number(&v, 1024, 1 << 22)?) },
            "disk_cap_mb" => self.disk_cap_mb = number(&v, 1024, 1 << 24)?,
            "hours" => {
                self.hours = if matches!(v.as_str(), "off" | "any" | "always" | "none") {
                    None
                } else {
                    let hm = |s: &str| -> Option<u32> {
                        let (h, m) = s.split_once(':').unwrap_or((s, "0"));
                        let (h, m) = (h.parse::<u32>().ok()?, m.parse::<u32>().ok()?);
                        (h <= 24 && m < 60 && h * 60 + m <= 1440).then_some((h * 60 + m) % 1440)
                    };
                    let parsed = v.split_once('-').and_then(|(a, b)| Some(Hours { start_min: hm(a.trim())?, end_min: hm(b.trim())? }));
                    Some(parsed.filter(|h| h.start_min != h.end_min).ok_or("hours: HH:MM-HH:MM (local time, may wrap midnight) or off")?)
                }
            }
            _ => return Err(format!("unknown setting {key:?}; one of {}", Self::KEYS.join(", "))),
        }
        Ok(self.describe_key(key))
    }

    /// One setting as `set` takes it back: `cpu_threads auto`, `work_when idle`, ...
    pub fn describe_key(&self, key: &str) -> String {
        let yes = |b: bool| if b { "true" } else { "false" };
        let val = match key {
            "cpu_threads" | "threads" => self.cpu_threads.map_or("auto".to_string(), |n| n.to_string()),
            "work_when" | "mode" => (if self.mode == Mode::Always { "active" } else { "idle" }).to_string(),
            "idle_minutes" => self.idle_minutes.to_string(),
            "on_battery" => yes(self.on_battery).to_string(),
            "keep_awake" => yes(self.keep_awake).to_string(),
            "autotune" => (if self.autotune { "on" } else { "off" }).to_string(),
            "workers" => self.workers.map_or("auto".to_string(), |n| n.to_string()),
            "gpu" => yes(self.gpu).to_string(),
            "max_ram_mb" => self.max_ram_mb.map_or("auto".to_string(), |n| n.to_string()),
            "disk_cap_mb" => self.disk_cap_mb.to_string(),
            "hours" => self.hours.map_or("off".to_string(), |h| format!("{:02}:{:02}-{:02}:{:02}", h.start_min / 60, h.start_min % 60, h.end_min / 60, h.end_min % 60)),
            _ => "?".to_string(),
        };
        let key = match key {
            "threads" => "cpu_threads",
            "mode" => "work_when",
            k => k,
        };
        format!("{key} {val}")
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct GamesToday {
    pub day: String,
    pub n: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Machine {
    pub machine_id: Option<String>,
    /// base64 of the DPAPI (CurrentUser) blob holding the machine token.
    pub token_dpapi: Option<String>,
    pub channel: String,
    pub name: Option<String>,
    /// Coordinator override (staging, tests). `None` = production.
    pub coordinator: Option<String>,
    pub settings: Settings,
    pub first_work_toast_shown: bool,
    pub games_today: GamesToday,
    /// rev 17 (PROTOCOL 11.5, macOS): the machine token in the clear; the file is mode 0600 in a 0700 `state/` (the
    /// Keychain would prompt after every self-update, whose code identity changes). Never set on Windows.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
}

impl Default for Machine {
    fn default() -> Self {
        Machine {
            machine_id: None,
            token_dpapi: None,
            channel: "stable".into(),
            name: None,
            coordinator: None,
            settings: Settings::default(),
            first_work_toast_shown: false,
            games_today: GamesToday::default(),
            token: None,
        }
    }
}

impl Machine {
    /// Loads `state\machine.json`; a missing file gives defaults, a corrupt one is set aside as
    /// `machine.json.bad` (and defaults are used) so a bad write can't brick the agent.
    pub fn load(layout: &Layout) -> Machine {
        let p = layout.machine_json();
        match std::fs::read(&p) {
            Ok(b) => match serde_json::from_slice(&b) {
                Ok(m) => m,
                Err(e) => {
                    log::error!("machine.json is corrupt ({e}); starting from defaults");
                    let _ = std::fs::rename(&p, p.with_extension("json.bad"));
                    Machine::default()
                }
            },
            Err(_) => Machine::default(),
        }
    }

    #[cfg(windows)]
    pub fn save(&self, layout: &Layout) -> std::io::Result<()> {
        atomic_write(&layout.machine_json(), &serde_json::to_vec_pretty(self).expect("json"))
    }

    /// macOS: written with mode 0600 (it holds the token), `state/` made 0700.
    #[cfg(not(windows))]
    pub fn save(&self, layout: &Layout) -> std::io::Result<()> {
        std::fs::create_dir_all(layout.state())?;
        zerofarm_common::fsutil::set_mode(&layout.state(), 0o700);
        zerofarm_common::fsutil::atomic_write_private(&layout.machine_json(), &serde_json::to_vec_pretty(self).expect("json"))
    }

    #[cfg(windows)]
    pub fn enrolled(&self) -> bool {
        self.machine_id.is_some() && self.token_dpapi.is_some()
    }

    #[cfg(not(windows))]
    pub fn enrolled(&self) -> bool {
        self.machine_id.is_some() && self.token.is_some()
    }

    #[cfg(not(windows))]
    pub fn token(&self) -> Option<String> {
        self.token.clone().filter(|t| !t.is_empty())
    }

    #[cfg(not(windows))]
    pub fn set_token(&mut self, token: &str) -> std::io::Result<()> {
        self.token = Some(token.to_string());
        Ok(())
    }

    #[cfg(windows)]
    pub fn token(&self) -> Option<String> {
        use base64::Engine as _;
        let blob = base64::engine::general_purpose::STANDARD.decode(self.token_dpapi.as_ref()?).ok()?;
        let raw = zerofarm_common::dpapi::unprotect(&blob, TOKEN_PURPOSE).ok()?;
        String::from_utf8(raw).ok()
    }

    #[cfg(windows)]
    pub fn set_token(&mut self, token: &str) -> std::io::Result<()> {
        use base64::Engine as _;
        let blob = zerofarm_common::dpapi::protect(token.as_bytes(), TOKEN_PURPOSE)?;
        self.token_dpapi = Some(base64::engine::general_purpose::STANDARD.encode(blob));
        Ok(())
    }

    pub fn forget_enrollment(&mut self) {
        self.machine_id = None;
        self.token_dpapi = None;
        self.token = None;
    }

    pub fn add_games(&mut self, n: u64) {
        let (day, _) = local_now();
        if self.games_today.day != day {
            self.games_today = GamesToday { day, n: 0 };
        }
        self.games_today.n += n;
    }

    pub fn games_today(&self) -> u64 {
        let (day, _) = local_now();
        if self.games_today.day == day {
            self.games_today.n
        } else {
            0
        }
    }
}

pub type SharedMachine = Arc<Mutex<Machine>>;

/// Pause helpers for the tray menu.
pub fn pause_for(now: i64, secs: i64) -> Pause {
    Pause::Until { t: now + secs }
}

/// Paused until the next local midnight.
pub fn pause_until_tomorrow(now: i64) -> Pause {
    let (_, sod) = local_now();
    Pause::Until { t: now + (86_400 - sod as i64).max(60) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hours_wrap() {
        let night = Hours { start_min: 22 * 60, end_min: 6 * 60 };
        assert!(night.contains(23 * 60));
        assert!(night.contains(60));
        assert!(!night.contains(12 * 60));
        let day = Hours { start_min: 9 * 60, end_min: 17 * 60 };
        assert!(day.contains(9 * 60));
        assert!(!day.contains(17 * 60));
    }

    #[test]
    fn defaults_and_threads() {
        let s = Settings::default();
        assert_eq!(s.threads(8, 16), 8, "auto = physical cores");
        assert_eq!(s.threads(2, 4), 2);
        assert_eq!(s.threads(0, 4), 1, "at least 1");
        assert_eq!(s.threads(8, 4), 4, "at most the logical threads");
        let s = Settings { cpu_threads: Some(64), ..Settings::default() };
        assert_eq!(s.threads(8, 16), 16);
        assert_eq!(s.ram_cap_mb(32768), 16384);
        assert!(!s.paused(0));
        assert!(Settings { pause: Pause::Until { t: 10 }, ..s.clone() }.paused(5));
        assert!(!Settings { pause: Pause::Until { t: 10 }, ..s.clone() }.paused(10));
    }

    #[test]
    fn set_by_name_for_the_headless_cli() {
        let mut s = Settings::default();
        assert_eq!(s.set_by_name("cpu_threads", "12").unwrap(), "cpu_threads 12");
        assert_eq!(s.threads(16, 16), 12);
        assert_eq!(s.set_by_name("cpu_threads", "auto").unwrap(), "cpu_threads auto");
        assert_eq!(s.threads(16, 16), 16, "auto: every physical core (12 performance + 4 efficiency on an M4 Max)");
        assert_eq!(s.set_by_name("work_when", "active").unwrap(), "work_when active");
        assert_eq!(s.mode, Mode::Always);
        assert_eq!(s.set_by_name("mode", "idle_only").unwrap(), "work_when idle");
        assert_eq!(s.set_by_name("on_battery", "true").unwrap(), "on_battery true");
        assert_eq!(s.set_by_name("keep_awake", "off").unwrap(), "keep_awake false");
        assert_eq!(s.set_by_name("autotune", "off").unwrap(), "autotune off");
        assert!(!s.autotune);
        assert!(serde_json::to_string(&s).unwrap().contains("\"autotune\":false"));
        assert_eq!(s.set_by_name("autotune", "on").unwrap(), "autotune on");
        // rev 19 (PROTOCOL 13.5): worker processes per job
        assert_eq!(s.set_by_name("workers", "2").unwrap(), "workers 2");
        assert_eq!(s.workers, Some(2));
        assert!(serde_json::to_string(&s).unwrap().contains("\"workers\":2"));
        assert_eq!(s.set_by_name("workers", "auto").unwrap(), "workers auto");
        assert_eq!(s.workers, None);
        assert!(!serde_json::to_string(&s).unwrap().contains("workers"));
        assert!(s.set_by_name("workers", "0").is_err() && s.set_by_name("workers", "4").is_err());
        assert_eq!(s.set_by_name("hours", "22:00-07:30").unwrap(), "hours 22:00-07:30");
        assert_eq!(s.hours, Some(Hours { start_min: 1320, end_min: 450 }));
        assert_eq!(s.set_by_name("hours", "off").unwrap(), "hours off");
        assert_eq!(s.set_by_name("max_ram_mb", "48000").unwrap(), "max_ram_mb 48000");
        for (k, v) in [("cpu_threads", "0"), ("work_when", "sometimes"), ("on_battery", "maybe"), ("hours", "9-9"), ("nope", "1"), ("max_ram_mb", "12")] {
            assert!(s.set_by_name(k, v).is_err(), "{k} {v}");
        }
        // The new settings stay out of machine.json at their defaults (Windows files unchanged); a missing key reads as
        // its default.
        let d = serde_json::to_string(&Settings::default()).unwrap();
        assert!(!d.contains("on_battery") && !d.contains("keep_awake") && !d.contains("autotune"), "{d}");
        let back: Settings = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert!(back.on_battery && !back.keep_awake && back == s);
        let old: Settings = serde_json::from_str(r#"{"mode":"idle_only"}"#).unwrap();
        assert!(!old.on_battery && old.keep_awake && old.autotune);
    }

    #[test]
    fn machine_json_roundtrip_with_token_and_corruption() {
        let d = tempfile::tempdir().unwrap();
        let l = Layout::new(d.path());
        let mut m = Machine::load(&l);
        assert!(!m.enrolled());
        m.machine_id = Some("m_X".into());
        m.set_token("secret-token").unwrap();
        m.settings.mode = Mode::Always;
        m.save(&l).unwrap();
        let text = std::fs::read_to_string(l.machine_json()).unwrap();
        #[cfg(windows)]
        assert!(!text.contains("secret-token"), "token must be DPAPI-protected on disk");
        // macOS (PROTOCOL 11.5): in the clear, in a file only this user can read, in a folder only this user can open
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert!(text.contains("\"token\": \"secret-token\"") && !text.contains("token_dpapi\": \""), "{text}");
            assert_eq!(std::fs::metadata(l.machine_json()).unwrap().permissions().mode() & 0o777, 0o600);
            assert_eq!(std::fs::metadata(l.state()).unwrap().permissions().mode() & 0o777, 0o700);
        }
        let m2 = Machine::load(&l);
        assert_eq!(m2.token().as_deref(), Some("secret-token"));
        assert_eq!(m2.settings.mode, Mode::Always);
        std::fs::write(l.machine_json(), b"{not json").unwrap();
        let m3 = Machine::load(&l);
        assert!(!m3.enrolled());
        assert!(l.state().join("machine.json.bad").exists());
    }
}

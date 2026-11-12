//! `state\machine.json`: machine id, the DPAPI-protected token, the channel, and the friend's
//! settings (DESIGN 3.2: all user-adjustable; defaults below).

use crate::util::local_now;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};
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
    /// CPU threads for work; `None` = physical cores − 2 (at least 1).
    pub cpu_threads: Option<u32>,
    pub idle_minutes: u32,
    pub hours: Option<Hours>,
    /// RAM cap for the worker; `None` = half of physical RAM.
    pub max_ram_mb: Option<u64>,
    /// Hard disk cap for everything under the install root (DESIGN 3.5).
    pub disk_cap_mb: u64,
    /// Never work while a fullscreen app / game / presentation runs (also in Always mode).
    pub respect_fullscreen: bool,
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
        let auto = physical_cores.saturating_sub(2).max(1);
        self.cpu_threads.unwrap_or(auto).clamp(1, logical.max(1))
    }

    pub fn ram_cap_mb(&self, total_mb: u64) -> u64 {
        self.max_ram_mb.unwrap_or(total_mb / 2)
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

    pub fn save(&self, layout: &Layout) -> std::io::Result<()> {
        atomic_write(&layout.machine_json(), &serde_json::to_vec_pretty(self).expect("json"))
    }

    pub fn enrolled(&self) -> bool {
        self.machine_id.is_some() && self.token_dpapi.is_some()
    }

    pub fn token(&self) -> Option<String> {
        use base64::Engine as _;
        let blob = base64::engine::general_purpose::STANDARD.decode(self.token_dpapi.as_ref()?).ok()?;
        let raw = zerofarm_common::dpapi::unprotect(&blob, TOKEN_PURPOSE).ok()?;
        String::from_utf8(raw).ok()
    }

    pub fn set_token(&mut self, token: &str) -> std::io::Result<()> {
        use base64::Engine as _;
        let blob = zerofarm_common::dpapi::protect(token.as_bytes(), TOKEN_PURPOSE)?;
        self.token_dpapi = Some(base64::engine::general_purpose::STANDARD.encode(blob));
        Ok(())
    }

    pub fn forget_enrollment(&mut self) {
        self.machine_id = None;
        self.token_dpapi = None;
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
        assert_eq!(s.threads(8, 16), 6);
        assert_eq!(s.threads(2, 4), 1);
        let s = Settings { cpu_threads: Some(64), ..Settings::default() };
        assert_eq!(s.threads(8, 16), 16);
        assert_eq!(s.ram_cap_mb(32768), 16384);
        assert!(!s.paused(0));
        assert!(Settings { pause: Pause::Until { t: 10 }, ..s.clone() }.paused(5));
        assert!(!Settings { pause: Pause::Until { t: 10 }, ..s.clone() }.paused(10));
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
        assert!(!text.contains("secret-token"), "token must be DPAPI-protected on disk");
        let m2 = Machine::load(&l);
        assert_eq!(m2.token().as_deref(), Some("secret-token"));
        assert_eq!(m2.settings.mode, Mode::Always);
        std::fs::write(l.machine_json(), b"{not json").unwrap();
        let m3 = Machine::load(&l);
        assert!(!m3.enrolled());
        assert!(l.state().join("machine.json.bad").exists());
    }
}

//! When the agent may work (DESIGN 3.2) — a pure decision function over sampled signals.
//!
//! Only yes/no answers leave this module (and the agent): which app is fullscreen or which
//! process uses the GPU is never looked up, recorded or uploaded.
//!
//! | Signal | Rule |
//! |---|---|
//! | pause (tray) | no work while paused |
//! | optional hours | no work outside them |
//! | power | never on battery or with battery saver on |
//! | fullscreen / D3D / presentation / busy | never work (also in "always" mode, unless disabled) |
//! | memory | keep ≥ max(4 GB, 25 %) free; below that: no work, and pressure ends a paused lease |
//! | input idle | "idle only" mode: work after `idle_minutes` without input; any input → yield |
//! | other GPU process | no GPU work while others use > 10 % for 30 s (cleared after 30 s quiet) |
//! | GPU temperature | no GPU work above 83 °C, resumes at ≤ 78 °C |

use crate::settings::{Mode, Settings};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Notification {
    /// Screen saver, locked, or another user's session is active (a fine time to work).
    NotPresent,
    /// A fullscreen app or presentation (QUNS_BUSY).
    Busy,
    /// A Direct3D fullscreen game (QUNS_RUNNING_D3D_FULL_SCREEN).
    D3dFullscreen,
    /// Presentation mode (QUNS_PRESENTATION_MODE).
    Presentation,
    AcceptsNotifications,
    QuietTime,
    /// A Windows Store app is fullscreen (QUNS_APP).
    App,
    Unknown,
}

impl Notification {
    pub fn is_fullscreen(self) -> bool {
        matches!(self, Notification::Busy | Notification::D3dFullscreen | Notification::Presentation | Notification::App)
    }
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct GpuSignal {
    /// Utilisation by processes other than our worker, percent (max over GPUs).
    pub other_util_pct: u32,
    /// Hottest GPU, °C.
    pub temp_c: Option<u32>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Signals {
    pub now: i64,
    /// Local minutes since midnight (for the optional hours).
    pub local_min: u32,
    /// Seconds since the last keyboard/mouse input.
    pub idle_s: u64,
    pub notification: Notification,
    /// `None` = unknown (treated as on AC).
    pub on_ac: Option<bool>,
    pub battery_saver: bool,
    pub mem_avail_mb: u64,
    pub mem_total_mb: u64,
    /// `None` = no NVIDIA GPU / NVML not available.
    pub gpu: Option<GpuSignal>,
}

impl Signals {
    /// All clear: idle for an hour, on AC, plenty of memory, no GPU.
    pub fn all_clear(now: i64) -> Signals {
        Signals {
            now,
            local_min: 12 * 60,
            idle_s: 3600,
            notification: Notification::AcceptsNotifications,
            on_ac: Some(true),
            battery_saver: false,
            mem_avail_mb: 24_000,
            mem_total_mb: 32_768,
            gpu: None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Block {
    Paused,
    OutsideHours,
    OnBattery,
    BatterySaver,
    Fullscreen,
    LowMemory,
    UserActive,
}

impl Block {
    pub fn describe(self) -> &'static str {
        match self {
            Block::Paused => "paused",
            Block::OutsideHours => "outside working hours",
            Block::OnBattery => "on battery",
            Block::BatterySaver => "battery saver is on",
            Block::Fullscreen => "a fullscreen app is running",
            Block::LowMemory => "memory is low",
            Block::UserActive => "you're using the PC",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GpuBlock {
    Disabled,
    NoGpu,
    OtherProcess,
    TooHot,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Decision {
    /// May run (or keep running) a worker.
    pub work: bool,
    pub block: Option<Block>,
    /// May use the GPU.
    pub gpu: bool,
    pub gpu_block: Option<GpuBlock>,
    /// Free memory is below the floor (a suspended worker still holds its RAM: end it).
    pub mem_pressure: bool,
}

/// State carried between decisions (for the 30 s GPU rule and temperature hysteresis).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PolicyState {
    busy_since: Option<i64>,
    quiet_since: Option<i64>,
    other_busy: bool,
    hot: bool,
}

pub const GPU_BUSY_PCT: u32 = 10;
pub const GPU_BUSY_SECS: i64 = 30;
pub const GPU_HOT_C: u32 = 83;
pub const GPU_COOL_C: u32 = 78;
pub const MEM_FLOOR_MB: u64 = 4096;

/// Free memory to keep: 4 GB or 25 % of RAM, whichever is larger.
pub fn mem_floor_mb(total_mb: u64) -> u64 {
    MEM_FLOOR_MB.max(total_mb / 4)
}

pub fn decide(st: &mut PolicyState, s: &Signals, cfg: &Settings) -> Decision {
    let gpu_state = update_gpu_state(st, s);
    let mem_pressure = s.mem_avail_mb < mem_floor_mb(s.mem_total_mb);
    let block = if cfg.paused(s.now) {
        Some(Block::Paused)
    } else if cfg.hours.is_some_and(|h| !h.contains(s.local_min)) {
        Some(Block::OutsideHours)
    } else if s.on_ac == Some(false) {
        Some(Block::OnBattery)
    } else if s.battery_saver {
        Some(Block::BatterySaver)
    } else if cfg.respect_fullscreen && s.notification.is_fullscreen() {
        Some(Block::Fullscreen)
    } else if mem_pressure {
        Some(Block::LowMemory)
    } else if cfg.mode == Mode::IdleOnly && s.idle_s < cfg.idle_minutes as u64 * 60 {
        Some(Block::UserActive)
    } else {
        None
    };
    let work = block.is_none();
    let gpu_block = if !cfg.gpu {
        Some(GpuBlock::Disabled)
    } else {
        gpu_state
    };
    Decision { work, block, gpu: work && gpu_block.is_none(), gpu_block, mem_pressure }
}

fn update_gpu_state(st: &mut PolicyState, s: &Signals) -> Option<GpuBlock> {
    let Some(g) = &s.gpu else {
        *st = PolicyState::default();
        return Some(GpuBlock::NoGpu);
    };
    if g.other_util_pct > GPU_BUSY_PCT {
        st.quiet_since = None;
        let since = *st.busy_since.get_or_insert(s.now);
        if s.now - since >= GPU_BUSY_SECS {
            st.other_busy = true;
        }
    } else {
        st.busy_since = None;
        let since = *st.quiet_since.get_or_insert(s.now);
        if s.now - since >= GPU_BUSY_SECS {
            st.other_busy = false;
        }
    }
    if let Some(t) = g.temp_c {
        if t > GPU_HOT_C {
            st.hot = true;
        } else if t <= GPU_COOL_C {
            st.hot = false;
        }
    }
    if st.other_busy {
        Some(GpuBlock::OtherProcess)
    } else if st.hot {
        Some(GpuBlock::TooHot)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::{Hours, Pause};

    fn d(s: &Signals, cfg: &Settings) -> Decision {
        decide(&mut PolicyState::default(), s, cfg)
    }

    #[test]
    fn all_clear_works() {
        let r = d(&Signals::all_clear(1000), &Settings::default());
        assert!(r.work);
        assert_eq!(r.block, None);
        assert!(!r.gpu, "no GPU present");
        assert_eq!(r.gpu_block, Some(GpuBlock::NoGpu));
    }

    #[test]
    fn idle_threshold_and_input_yield() {
        let cfg = Settings::default();
        let mut s = Signals::all_clear(0);
        s.idle_s = 299;
        assert_eq!(d(&s, &cfg).block, Some(Block::UserActive));
        s.idle_s = 300;
        assert!(d(&s, &cfg).work);
        // Input while working: idle time resets, the decision flips at once (agent yields).
        s.idle_s = 0;
        assert_eq!(d(&s, &cfg).block, Some(Block::UserActive));
        // Always mode ignores input.
        let always = Settings { mode: Mode::Always, ..Settings::default() };
        assert!(d(&s, &always).work);
    }

    #[test]
    fn fullscreen_blocks_even_in_always_mode() {
        let always = Settings { mode: Mode::Always, ..Settings::default() };
        for n in [Notification::Busy, Notification::D3dFullscreen, Notification::Presentation, Notification::App] {
            let mut s = Signals::all_clear(0);
            s.notification = n;
            assert_eq!(d(&s, &always).block, Some(Block::Fullscreen), "{n:?}");
        }
        let mut s = Signals::all_clear(0);
        s.notification = Notification::NotPresent; // locked: fine
        assert!(d(&s, &Settings::default()).work);
        s.notification = Notification::D3dFullscreen;
        let lax = Settings { respect_fullscreen: false, ..always };
        assert!(d(&s, &lax).work);
    }

    #[test]
    fn power_rules() {
        let mut s = Signals::all_clear(0);
        s.on_ac = Some(false);
        assert_eq!(d(&s, &Settings::default()).block, Some(Block::OnBattery));
        s.on_ac = None;
        assert!(d(&s, &Settings::default()).work, "unknown power = desktop");
        s.battery_saver = true;
        assert_eq!(d(&s, &Settings::default()).block, Some(Block::BatterySaver));
    }

    #[test]
    fn memory_floor_is_4gb_or_quarter() {
        assert_eq!(mem_floor_mb(8192), 4096);
        assert_eq!(mem_floor_mb(65536), 16384);
        let mut s = Signals::all_clear(0);
        s.mem_total_mb = 65536;
        s.mem_avail_mb = 16000;
        let r = d(&s, &Settings::default());
        assert_eq!(r.block, Some(Block::LowMemory));
        assert!(r.mem_pressure);
        s.mem_avail_mb = 16384;
        assert!(d(&s, &Settings::default()).work);
    }

    #[test]
    fn pause_and_hours() {
        let s = Signals::all_clear(1000);
        let p = Settings { pause: Pause::Until { t: 2000 }, ..Settings::default() };
        assert_eq!(d(&s, &p).block, Some(Block::Paused));
        let p = Settings { pause: Pause::Until { t: 1000 }, ..Settings::default() };
        assert!(d(&s, &p).work, "pause expired");
        let p = Settings { pause: Pause::Indefinitely, ..Settings::default() };
        assert_eq!(d(&s, &p).block, Some(Block::Paused));
        let night = Settings { hours: Some(Hours { start_min: 0, end_min: 8 * 60 }), ..Settings::default() };
        assert_eq!(d(&s, &night).block, Some(Block::OutsideHours));
        let mut s2 = s.clone();
        s2.local_min = 3 * 60;
        assert!(d(&s2, &night).work);
    }

    #[test]
    fn block_precedence() {
        let mut s = Signals::all_clear(0);
        s.idle_s = 0;
        s.on_ac = Some(false);
        s.notification = Notification::D3dFullscreen;
        let p = Settings { pause: Pause::Indefinitely, ..Settings::default() };
        assert_eq!(d(&s, &p).block, Some(Block::Paused));
        assert_eq!(d(&s, &Settings::default()).block, Some(Block::OnBattery));
    }

    #[test]
    fn gpu_other_process_needs_30s_both_ways() {
        let cfg = Settings::default();
        let mut st = PolicyState::default();
        let mut s = Signals::all_clear(0);
        s.gpu = Some(GpuSignal { other_util_pct: 0, temp_c: Some(60) });
        assert!(decide(&mut st, &s, &cfg).gpu);
        // Another process starts using 50 %.
        for t in [1, 10, 30] {
            s.now = t;
            s.gpu = Some(GpuSignal { other_util_pct: 50, temp_c: Some(60) });
            assert!(decide(&mut st, &s, &cfg).gpu, "t={t}: not yet 30 s");
        }
        s.now = 31;
        let r = decide(&mut st, &s, &cfg);
        assert!(!r.gpu);
        assert_eq!(r.gpu_block, Some(GpuBlock::OtherProcess));
        assert!(r.work, "CPU work is still fine");
        // Quiet again: needs 30 s quiet.
        s.gpu = Some(GpuSignal { other_util_pct: 5, temp_c: Some(60) });
        s.now = 40;
        assert!(!decide(&mut st, &s, &cfg).gpu);
        s.now = 69;
        assert!(!decide(&mut st, &s, &cfg).gpu);
        s.now = 70;
        assert!(decide(&mut st, &s, &cfg).gpu);
        // A short spike doesn't count.
        s.now = 71;
        s.gpu = Some(GpuSignal { other_util_pct: 90, temp_c: Some(60) });
        decide(&mut st, &s, &cfg);
        s.now = 80;
        s.gpu = Some(GpuSignal { other_util_pct: 0, temp_c: Some(60) });
        s.now = 120;
        assert!(decide(&mut st, &s, &cfg).gpu);
    }

    #[test]
    fn gpu_temperature_hysteresis_and_toggle() {
        let cfg = Settings::default();
        let mut st = PolicyState::default();
        let mut s = Signals::all_clear(0);
        s.gpu = Some(GpuSignal { other_util_pct: 0, temp_c: Some(84) });
        assert_eq!(decide(&mut st, &s, &cfg).gpu_block, Some(GpuBlock::TooHot));
        s.gpu = Some(GpuSignal { other_util_pct: 0, temp_c: Some(80) });
        assert_eq!(decide(&mut st, &s, &cfg).gpu_block, Some(GpuBlock::TooHot), "still cooling");
        s.gpu = Some(GpuSignal { other_util_pct: 0, temp_c: Some(78) });
        assert!(decide(&mut st, &s, &cfg).gpu);
        let off = Settings { gpu: false, ..Settings::default() };
        assert_eq!(decide(&mut st, &s, &off).gpu_block, Some(GpuBlock::Disabled));
        // No GPU work when no work at all.
        s.idle_s = 0;
        let r = decide(&mut st, &s, &cfg);
        assert!(!r.work && !r.gpu);
    }
}

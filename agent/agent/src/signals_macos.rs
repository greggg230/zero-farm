//! The macOS signal source (PROTOCOL 11.5). Input idle time from IOKit (`HIDIdleTime`); `on_battery` from the
//! providing power source and `battery_saver` from Low Power Mode; free memory and memory pressure (`low_memory`);
//! no GPU and no fullscreen detection in 0.3.0. While the engine holds a lease on AC power and the friend's
//! `keep_awake` setting is on (the default), a power assertion keeps the Mac from idle-sleeping (like `caffeinate -i`;
//! the display still sleeps): a Mac otherwise sleeps whenever the user leaves, which is exactly when it may work.

use crate::macos;
use crate::policy::{Notification, Signals};
use crate::settings::SharedMachine;
use std::time::{Duration, Instant};

/// The power source and Low Power Mode are read at most this often (an IPC each; the lease loop samples every
/// 200 ms). Input idle time and memory are read every sample.
const POWER_EVERY: Duration = Duration::from_secs(2);

pub use macos::memory_mb;

/// (on AC: Some/None when unknown, Low Power Mode on).
pub fn power() -> (Option<bool>, bool) {
    (macos::on_ac(), macos::low_power_mode())
}

pub struct MacSignals {
    machine: Option<SharedMachine>,
    power: Option<(Instant, (Option<bool>, bool))>,
    lease: bool,
    awake: Option<macos::AwakeAssertion>,
}

impl MacSignals {
    /// `machine`: where the `keep_awake` setting is read (None: never keep the Mac awake, e.g. `dry-run`).
    pub fn new(machine: Option<SharedMachine>) -> Self {
        MacSignals { machine, power: None, lease: false, awake: None }
    }

    fn power(&mut self) -> (Option<bool>, bool) {
        match self.power {
            Some((t, p)) if t.elapsed() < POWER_EVERY => p,
            _ => {
                let p = power();
                self.power = Some((Instant::now(), p));
                p
            }
        }
    }

    /// Holds the power assertion exactly while a lease runs, the Mac is on AC power and `keep_awake` is on.
    fn keep_awake(&mut self, on_ac: Option<bool>) {
        let setting = self.machine.as_ref().is_some_and(|m| m.lock().map(|m| m.settings.keep_awake).unwrap_or(true));
        let want = self.lease && on_ac == Some(true) && setting;
        match (want, self.awake.is_some()) {
            (true, false) => {
                self.awake = macos::AwakeAssertion::take("Zero Farm is working");
                if self.awake.is_some() {
                    log::info!("keeping the Mac from idle-sleeping while it works (keep_awake)");
                } else {
                    log::warn!("could not take the power assertion that keeps the Mac awake");
                }
            }
            (false, true) => {
                self.awake = None;
                log::info!("the Mac may idle-sleep again");
            }
            _ => {}
        }
    }

    /// Whether the power assertion is held (tests, status).
    pub fn awake_held(&self) -> bool {
        self.awake.is_some()
    }

    /// (`dry-run`, as on Windows) No GPU counters on macOS.
    pub fn has_busy_counters(&self) -> bool {
        false
    }

    /// (`dry-run`) No GPU readings on macOS.
    pub fn gpu_readings(&self) -> &[crate::signals::AdapterReading] {
        &[]
    }
}

impl crate::signals::SignalSource for MacSignals {
    fn sample(&mut self, _worker_pid: Option<u32>) -> Signals {
        let (_, sod) = crate::util::local_now();
        let (mem_avail_mb, mem_total_mb) = memory_mb();
        let (on_ac, battery_saver) = self.power();
        self.keep_awake(on_ac);
        Signals {
            now: zerofarm_common::now_unix(),
            local_min: sod / 60,
            idle_s: macos::hid_idle_seconds().unwrap_or(0),
            // (no fullscreen detection on macOS in 0.3.0)
            notification: Notification::AcceptsNotifications,
            on_ac,
            battery_saver,
            mem_avail_mb,
            mem_total_mb,
            gpu: None,
        }
    }

    fn lease_active(&mut self, active: bool) {
        self.lease = active;
        self.power = None;
        let (on_ac, _) = self.power();
        self.keep_awake(on_ac);
    }
}

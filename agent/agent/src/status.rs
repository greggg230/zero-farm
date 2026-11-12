//! What the tray shows. The engine writes it; the tray reads it.

use std::sync::{Arc, Mutex};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Phase {
    #[default]
    Starting,
    NotEnrolled,
    /// Not allowed to work right now (detail says why).
    Idle,
    /// Allowed to work; waiting for the coordinator to have some.
    Waiting,
    Downloading,
    Working,
    /// A lease is suspended (yielded) because the PC is in use.
    Paused,
    Updating,
    Offline,
    Revoked,
    /// Windows refuses to start the job program (Smart App Control and the like): waiting.
    Blocked,
}

/// What the friend reads when Windows blocks the job program (tray menu and balloon).
pub const BLOCKED_MESSAGE: &str = "Windows is blocking Zero Farm's job program (Smart App Control). Zero Farm can't work on this PC until its programs are code-signed. Nothing is wrong with your PC.";

#[derive(Clone, Debug, Default)]
pub struct Status {
    pub phase: Phase,
    pub detail: String,
    /// e.g. "self-play for eoe2", "fuzzing (fuzz-delver)"
    pub job: Option<String>,
    pub games_today: u64,
    pub last_error: Option<String>,
    pub gpu_note: Option<String>,
    pub version: String,
    /// A one-time balloon the tray shows and clears (first work only).
    pub toast: Option<String>,
}

impl Status {
    pub fn headline(&self) -> String {
        let s = match self.phase {
            Phase::Starting => "Starting…".to_string(),
            Phase::NotEnrolled => "Not set up yet: enter your invite code".to_string(),
            Phase::Idle => format!("Idle — {}", self.detail),
            Phase::Waiting => "Ready — waiting for work".to_string(),
            Phase::Downloading => format!("Downloading {}", self.detail),
            Phase::Working => format!("Working — {}", self.job.clone().unwrap_or_default()),
            Phase::Paused => format!("Paused — {}", self.detail),
            Phase::Updating => "Updating…".to_string(),
            Phase::Offline => "Can't reach the farm right now; will retry".to_string(),
            Phase::Revoked => "This PC was removed from the farm".to_string(),
            Phase::Blocked => "Windows is blocking Zero Farm's job program (Smart App Control)".to_string(),
        };
        s.chars().take(120).collect()
    }
}

pub type SharedStatus = Arc<Mutex<Status>>;

pub fn set(st: &SharedStatus, f: impl FnOnce(&mut Status)) {
    if let Ok(mut g) = st.lock() {
        f(&mut g);
    }
}

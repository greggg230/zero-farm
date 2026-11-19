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
    /// The coordinator refuses this PC's token (`unauthorized`); the token is kept and retried.
    SignInFailed,
    /// Windows refuses to start the job program (Smart App Control and the like): waiting.
    Blocked,
}

/// What the friend reads when Windows blocks the job program (tray menu and balloon).
pub const BLOCKED_MESSAGE: &str = "Windows is blocking Zero Farm's job program (Smart App Control). Zero Farm can't work on this PC until its programs are code-signed. Nothing is wrong with your PC.";

/// What the friend reads when the farm refuses this PC's token (not a revocation).
pub const SIGN_IN_MESSAGE: &str = "This PC can't sign in to the farm; ask Gregg.";

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
    /// rev 19 (PROTOCOL 13): the CPU layout of the current self-play job, tuned or being tuned (None: not tuning).
    pub tuning: Option<String>,
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
            Phase::SignInFailed => SIGN_IN_MESSAGE.to_string(),
            Phase::Blocked => "Windows is blocking Zero Farm's job program (Smart App Control)".to_string(),
        };
        s.chars().take(120).collect()
    }
}

/// `text` in lines of at most `width` characters (broken at spaces), at most `lines` of them (the last cut short with
/// "…"): the tray's menu items don't wrap (rev 19: a crew's layout line, PROTOCOL 13.5).
pub fn wrap_menu(text: &str, width: usize, lines: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    for word in text.split(' ') {
        let len = cur.chars().count();
        if len > 0 && len + 1 + word.chars().count() > width {
            out.push(std::mem::take(&mut cur));
        }
        if !cur.is_empty() {
            cur.push(' ');
        }
        cur.push_str(word);
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    if out.len() > lines {
        out.truncate(lines);
        if let Some(last) = out.last_mut() {
            let keep: String = last.chars().take(width.saturating_sub(1)).collect();
            *last = format!("{keep}…");
        }
    }
    for l in &mut out {
        if l.chars().count() > width {
            *l = l.chars().take(width).collect();
        }
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn menu_lines_wrap_at_spaces() {
        let t = "tuned: 2 workers x (12 search + 4 x 3 inference threads, 1,152 games in flight in 6 groups (192 rows)), 18,012 leaves/s";
        let lines = super::wrap_menu(t, 100, 3);
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(lines.iter().all(|l| l.chars().count() <= 100));
        assert_eq!(lines.join(" "), t);
        assert_eq!(super::wrap_menu("short", 100, 3), vec!["short".to_string()]);
        let long = "word ".repeat(100);
        let cut = super::wrap_menu(long.trim(), 20, 2);
        assert_eq!(cut.len(), 2);
        assert!(cut[1].ends_with('…') && cut[1].chars().count() <= 20, "{cut:?}");
    }
}

pub type SharedStatus = Arc<Mutex<Status>>;

pub fn set(st: &SharedStatus, f: impl FnOnce(&mut Status)) {
    if let Ok(mut g) = st.lock() {
        f(&mut g);
    }
}

//! Rolling self-play leases (rev 15, PROTOCOL 10): what the agent works out for one - the lease's block and
//! setting checked, and the games in flight (`conc`, `groups`) for "auto" - and the numbers those choices use.
//! The lease's life (extensions, generation switches, finishing) is in lease.rs.
//!
//! **The numbers for "auto" live here, in one place** (PROTOCOL 10.6): they are first guesses, to be tuned from
//! measurements of the worker's memory per game in flight and of its rows/s by games in flight.

use std::collections::BTreeMap;
use zerofarm_common::protocol::Lease;

/// The `features` this agent asks `/v1/lease` with (PROTOCOL 10.2).
pub const FEATURES: [&str; 1] = ["rolling"];
/// The worker feature a rolling job needs (its `hello.features`, PROTOCOL 10.5).
pub const WORKER_FEATURE: &str = "rolling";

/// "auto" on the CPU: this many games in flight per thread of the job ...
pub const CPU_GAMES_PER_THREAD: u64 = 4;
/// ... in this many groups (batches in turn).
pub const CPU_GROUPS: u64 = 4;
/// "auto" on a GPU: the games in flight tried, in order; the next is taken while doubling gains enough.
pub const GPU_STEPS: [u64; 4] = [64, 128, 256, 512];
/// "auto" on a GPU: groups (each batch is half of the games in flight).
pub const GPU_GROUPS: u64 = 2;
/// "auto" on a GPU: doubling the games in flight must raise the bench's rows/s at least this much (15 %).
pub const GPU_DOUBLING_GAIN: f64 = 1.15;
/// Memory of one game in flight (its search tree, positions, rows waiting), MB. "auto" never plans more games in
/// flight than fit `caps.ram_mb` with this and `RAM_BASE_MB`.
pub const RAM_PER_GAME_MB: u64 = 32;
/// The worker's memory before any game (the network's session, the runtime, the process), MB.
pub const RAM_BASE_MB: u64 = 600;

/// The run's setting for a CPU or GPU job (`rolling.inflight.cpu` / `.gpu`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Inflight {
    N(u64),
    Auto,
}

/// Most games in flight a run may set (PROTOCOL 10.1).
pub const MAX_INFLIGHT: u64 = 4096;
/// `flush_s` is kept within these bounds (PROTOCOL 10.1).
pub const FLUSH_S: (u64, u64) = (10, 3600);

impl Inflight {
    /// `n` (1 to 4,096) or `"auto"`; anything else is taken as `"auto"` (the agent's own choice, never more than
    /// fits its memory).
    pub fn parse(v: &serde_json::Value) -> Inflight {
        match v.as_u64() {
            Some(n) if (1..=MAX_INFLIGHT).contains(&n) => Inflight::N(n),
            _ => {
                if !v.is_null() && v.as_str() != Some("auto") {
                    log::warn!("rolling: games in flight {v} is not a number from 1 to {MAX_INFLIGHT} or \"auto\": taking \"auto\"");
                }
                Inflight::Auto
            }
        }
    }
}

/// A rolling lease as the agent runs it: the checked block and setting.
#[derive(Debug, Clone, PartialEq)]
pub struct Setup {
    /// Ids `[lo, hi)` reserved for the lease.
    pub lo: u64,
    pub hi: u64,
    /// The released ids at grant (`job.games`): `[lo, lo + released)`.
    pub released: u64,
    pub flush_s: u64,
    pub cpu: Inflight,
    pub gpu: Inflight,
}

/// The rolling part of a lease, checked against its job: a self-play lease whose `gid_base` is the block's start and
/// whose released ids lie inside the block. Anything else runs as a classic lease (None, with the reason).
pub fn setup(lease: &Lease) -> Result<Option<Setup>, String> {
    let Some(r) = &lease.rolling else { return Ok(None) };
    if lease.kind != "selfplay" {
        return Err(format!("a {} lease can't be rolling", lease.kind));
    }
    let (lo, hi) = r.block;
    let base = lease.job.get("gid_base").and_then(|v| v.as_u64());
    let games = lease.job.get("games").and_then(|v| v.as_u64()).unwrap_or(0);
    if base != Some(lo) || hi <= lo || hi > zerofarm_common::definition::MAX_ID || games == 0 || games > hi - lo {
        return Err(format!("block [{lo}, {hi}) doesn't hold the job's ids (gid_base {base:?}, games {games})"));
    }
    Ok(Some(Setup {
        lo,
        hi,
        released: games,
        flush_s: if r.flush_s == 0 { 120 } else { r.flush_s.clamp(FLUSH_S.0, FLUSH_S.1) },
        cpu: Inflight::parse(&r.inflight.cpu),
        gpu: Inflight::parse(&r.inflight.gpu),
    }))
}

/// The most games in flight `ram_mb` holds (at least 1).
pub fn ram_cap(ram_mb: u64) -> u64 {
    (ram_mb.saturating_sub(RAM_BASE_MB) / RAM_PER_GAME_MB).max(1)
}

/// A CPU job's (conc, groups): the run's number (in the template's groups, at most one per game), or "auto":
/// `CPU_GAMES_PER_THREAD` x the job's threads in `CPU_GROUPS` groups, capped by memory.
pub fn cpu_shape(setting: Inflight, threads: u32, template_groups: u64, ram_mb: u64) -> (u64, u64) {
    match setting {
        Inflight::N(n) => (n, template_groups.clamp(1, n)),
        Inflight::Auto => {
            let conc = (CPU_GAMES_PER_THREAD * threads.max(1) as u64).min(ram_cap(ram_mb)).max(1);
            (conc, CPU_GROUPS.min(conc))
        }
    }
}

/// A GPU job's (conc, groups) from the bench of the job's network on the provider (rows/s by batch rows): the run's
/// number (in at most 2 groups), or "auto": among `GPU_STEPS`, in `GPU_GROUPS` groups, the games in flight after which
/// doubling gains less than `GPU_DOUBLING_GAIN` rows/s, capped by memory.
pub fn gpu_shape(setting: Inflight, rows: &BTreeMap<String, f64>, template_groups: u64, ram_mb: u64) -> (u64, u64) {
    match setting {
        Inflight::N(n) => (n, template_groups.clamp(1, 2).min(n)),
        Inflight::Auto => {
            let rate = |n: u64| crate::probe::rate_at(rows, n.div_ceil(GPU_GROUPS));
            let mut n = GPU_STEPS[0];
            for &next in &GPU_STEPS[1..] {
                match (rate(n), rate(next)) {
                    (Some(a), Some(b)) if b >= a * GPU_DOUBLING_GAIN => n = next,
                    _ => break,
                }
            }
            let conc = n.min(ram_cap(ram_mb)).max(1);
            (conc, GPU_GROUPS.min(conc))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn rows(pts: &[(u64, f64)]) -> BTreeMap<String, f64> {
        pts.iter().map(|(n, r)| (n.to_string(), *r)).collect()
    }

    #[test]
    fn settings_parse() {
        assert_eq!(Inflight::parse(&json!(16)), Inflight::N(16));
        assert_eq!(Inflight::parse(&json!("auto")), Inflight::Auto);
        assert_eq!(Inflight::parse(&json!(0)), Inflight::Auto);
        assert_eq!(Inflight::parse(&json!(5000)), Inflight::Auto);
        assert_eq!(Inflight::parse(&json!(null)), Inflight::Auto);
    }

    #[test]
    fn cpu_auto_is_four_per_thread_in_four_groups_within_memory() {
        assert_eq!(cpu_shape(Inflight::Auto, 8, 1, 16_000), (32, 4));
        assert_eq!(cpu_shape(Inflight::Auto, 32, 1, 16_000), (128, 4));
        // 600 MB + 32 MB per game: 1,880 MB hold 40 games
        assert_eq!(cpu_shape(Inflight::Auto, 32, 1, 1880), (40, 4));
        assert_eq!(cpu_shape(Inflight::Auto, 8, 1, 100), (1, 1), "never 0");
        assert_eq!(cpu_shape(Inflight::N(24), 8, 3, 100), (24, 3), "the run's number as it is");
        assert_eq!(cpu_shape(Inflight::N(2), 8, 4, 16_000), (2, 2));
    }

    #[test]
    fn gpu_auto_stops_doubling_below_15_percent() {
        // batch rows 32, 64, 128, 256 for 64, 128, 256, 512 in flight
        let r = rows(&[(1, 100.0), (32, 3000.0), (128, 9000.0), (512, 12000.0), (1024, 12500.0)]);
        // 64 -> 128: rate at 32 = 3000, at 64 ~ 5196 (+73 %); 128 -> 256: at 128 = 9000 (+73 %); 256 -> 512: at 256 ~
        // 10392 (+15.5 %): 512
        assert_eq!(gpu_shape(Inflight::Auto, &r, 1, 64_000), (512, 2));
        let flat = rows(&[(1, 100.0), (32, 3000.0), (64, 3300.0), (1024, 3400.0)]);
        assert_eq!(gpu_shape(Inflight::Auto, &flat, 1, 64_000), (64, 2), "64 -> 128 gains 10 %");
        let r2 = rows(&[(32, 3000.0), (64, 6000.0), (128, 6600.0)]);
        assert_eq!(gpu_shape(Inflight::Auto, &r2, 1, 64_000), (128, 2));
        // memory: 600 + 32 x n <= 5,000 MB -> 137
        assert_eq!(gpu_shape(Inflight::Auto, &r, 1, 5000), (137, 2));
        assert_eq!(gpu_shape(Inflight::N(300), &r, 4, 100), (300, 2), "the run's number, at most 2 groups");
        assert_eq!(gpu_shape(Inflight::Auto, &BTreeMap::new(), 1, 64_000), (64, 2), "no numbers: the smallest step");
    }

    #[test]
    fn setup_checks_the_block() {
        let lease = |job: serde_json::Value, rolling: serde_json::Value| -> Lease {
            serde_json::from_value(json!({"lease_id": "l_1", "kind": "selfplay", "issued_at": 0, "expires_at": 0, "beat_s": 60,
                "slack_s": 600, "build": {"build_id": "0123456789abcdef01234567", "manifest": "m"}, "job": job, "rolling": rolling}))
            .unwrap()
        };
        let s = setup(&lease(json!({"gid_base": 100, "games": 8, "seed": 1}), json!({"block": [100, 200], "inflight": {"cpu": 4, "gpu": "auto"}, "flush_s": 1})))
            .unwrap()
            .unwrap();
        assert_eq!(s, Setup { lo: 100, hi: 200, released: 8, flush_s: 10, cpu: Inflight::N(4), gpu: Inflight::Auto });
        assert!(setup(&lease(json!({"gid_base": 101, "games": 8, "seed": 1}), json!({"block": [100, 200]}))).is_err());
        assert!(setup(&lease(json!({"gid_base": 100, "games": 101, "seed": 1}), json!({"block": [100, 200]}))).is_err());
        assert!(setup(&lease(json!({"gid_base": 100, "games": 8, "seed": 1}), json!({"block": [100, 100]}))).is_err());
        let mut classic = lease(json!({"gid_base": 100, "games": 8, "seed": 1}), json!(null));
        assert_eq!(setup(&classic).unwrap(), None);
        classic.rolling = Some(Default::default());
        classic.kind = "eval".into();
        assert!(setup(&classic).is_err());
    }
}

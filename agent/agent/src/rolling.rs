//! Rolling self-play leases (rev 15, PROTOCOL 10): what the agent works out for one - the lease's block and
//! setting checked, and the games in flight (`conc`, `groups`) for "auto" - and the numbers those choices use.
//! The lease's life (extensions, generation switches, finishing) is in lease.rs.
//!
//! **The numbers for "auto" live here, in one place** (PROTOCOL 10.6). Speed, from the canary measurement of 2026-11-16
//! (a run's network on a PC with an RTX 4070 Ti, rolling leases): on the GPU 64 / 128 / 256 games in flight gave 831 /
//! 1,320 / 2,130 games an hour (2,191 at 256 in one group), still climbing at 256, so memory mostly decides; on the CPU
//! 64 in flight gave only 23 % more rows/s than 32 while a game took about 19 minutes (stale rows), so 32.
//!
//! Memory, from the soaks of 2026-11-17 (pb3's gen-143 network, a rolling job of 224 games in flight with no memory
//! limit; the same 4070 Ti PC): the worker commits about 1.75 GB before its games (the CUDA runtime; the CPU runtime
//! well under 1 GB), a game in flight grows as it is played, and the total levels off once the first wave of games has
//! been replaced - 9.4 GB at 12 minutes (191 games finished), 11.2 GB at 21 (497), 11.4 GB at 29 (776), ... - about 43
//! MB per game in flight at steady state. 0.2.13 planned 40 MB per game in flight plus 1 GB with no headroom, so
//! every lease ran into its 10,735 MB limit after 21-25 minutes; the plan now has a base, 45 MB per game and 15 %
//! headroom, and the agent watches the worker's memory as it runs (lease.rs, PROTOCOL 10.6 **Memory**).

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;
use zerofarm_common::layout::Layout;
use zerofarm_common::protocol::Lease;

/// The `features` this agent asks `/v1/lease` with (PROTOCOL 10.2).
pub const FEATURES: [&str; 1] = ["rolling"];
/// The worker feature a rolling job needs (its `hello.features`, PROTOCOL 10.5).
pub const WORKER_FEATURE: &str = "rolling";

/// "auto" on the CPU: this many games in flight ...
pub const CPU_INFLIGHT: u64 = 32;
/// ... in this many groups (batches in turn).
pub const CPU_GROUPS: u64 = 4;
/// "auto" on a GPU: the games in flight tried, in order; the next is taken while doubling gains enough.
pub const GPU_STEPS: [u64; 4] = [64, 128, 256, 512];
/// "auto" on a GPU: doubling the games in flight must raise the bench's rows/s at least this much (15 %).
pub const GPU_DOUBLING_GAIN: f64 = 1.15;
/// "auto" on a GPU: from this many games in flight one group (one batch of all of them), below it 2 (each batch half).
pub const GPU_ONE_GROUP_FROM: u64 = 256;
/// What a rolling job's worker commits (the memory its job's limit counts, MB): `base_mb` before any game (the
/// network's session, the runtime, the process), and `per_game_mb` for each game in flight (its search trees, its
/// positions and records so far, its share of the evaluation cache and of the runtime's buffers).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemCost {
    pub base_mb: u64,
    pub per_game_mb: u64,
}

/// A CPU job's cost until this PC measured the run's own (`Learned`).
pub const CPU_COST: MemCost = MemCost { base_mb: 600, per_game_mb: 45 };

/// macOS (rev 17, PROTOCOL 11.5): "auto" on the CPU is this many games in flight per physical core (all of them,
/// performance and efficiency: the threads are one per physical core, `settings.rs`) ...
pub const MACOS_CPU_INFLIGHT_PER_CORE: u64 = 144;
/// ... in groups (batches in turn) of this many games. From the Mac's own self-play over SSH on 2026-11-18 (the live
/// run, the whole M4 Max: 3 processes of 768 games in flight in 4 groups of 192, ~3.9k games an hour): 16 cores give
/// 2,304 games in flight in 12 groups of 192, the same games in flight and batch. Windows' 32 would leave most of a
/// Mac's cores waiting for batches.
pub const MACOS_CPU_BATCH: u64 = 192;
/// macOS (agent 0.3.3): when memory caps the games in flight, the groups stay (the cores' 12 on 16 cores, `cpu_shape`;
/// 6 a worker in a crew of two, `tune::Defaults::layout_for`) and the batches shrink - down to this many rows. The
/// group count is what keeps the search threads fed: on the M4 Max 4 groups played 8,970 leaves/s against 11,155 with 6
/// (one worker, PROTOCOL 13), and a crew of two held to 2 x 882 in 4 groups of 221 rows by 0.3.2's 192-row floor played
/// ~14.8k (each worker using ~4 of its cores) against ~18k with 6 groups a worker; inference costs about the same per row
/// whatever the batch (192-675 rows measured), so smaller batches cost little. 96 rows keep 6 groups down to 576 games
/// in flight a worker (2 x 691 on a 64 GB Mac: 6 groups of 116 rows).
pub const MACOS_CPU_MIN_BATCH: u64 = 96;
/// macOS: a CPU job's cost until this Mac measured the run's own with the lease's worker build (`Learned`), in physical
/// footprint (what the memory cap counts there, `worker_unix.rs`); the numbers below are the native worker builds of
/// before (zero-core's fix of 2026-11-19 needs ~2.5 MB a game, and is planned with what is measured). 0.3.0 planned 4 MB per game from the SSH self-play above (2.03-2.08 GB a process with 768
/// games in flight, 2.7 MB a game with the process, read early). A real rolling lease on the M4 Max (2026-11-19:
/// legacy-meta3-r1's network, fast export, one worker of 2,304 games in flight, about 235 games finished a minute, so a
/// game lasts about 10 minutes) grew all the way: 12.8 GB at 3 minutes, 19.0 at 5, 22.2 at 7, 25.2 at 9, 27.0 at 11,
/// 28.1 at 13, 29.1 at 15 - about 12.2 MB a game over the base, still climbing about 0.5 GB a minute and slowing - where
/// the agent's memory check restarted it at 1,728 (PROTOCOL 10.6) and remembered 13 MB a game for the run; 0.3.2 planned
/// 14 MB. The crew of the same day (two workers of 882 games in flight, 0.3.2) went further: the main worker reached
/// 14,487 MB of its 15,746 MB part after ~27 minutes - 16 MB a game over the base, still rising slowly - and hit the 92 %
/// trigger. Planned with a 1 GB base (what this Mac's measurements, `Learned`, are counted over) and 18 MB a game (agent
/// 0.3.3): with the default memory cap of half the RAM a 64 GB Mac (32,768 MB) holds 1,472 games in flight in one worker,
/// two workers 716 each (each its own base, `tune::Space`), a 16 GB Mac (8,192 MB) 320 - each about 76 % of its cap at
/// 16 MB a game and 85 % at 18, under the 92 % where a worker gives way.
pub const MACOS_CPU_COST: MemCost = MemCost { base_mb: 1024, per_game_mb: 18 };

/// "auto" on the CPU, (games in flight, groups): `CPU_INFLIGHT` in `CPU_GROUPS`; on macOS `MACOS_CPU_INFLIGHT_PER_CORE`
/// per physical core (rounded as `round_games`) in groups of `MACOS_CPU_BATCH`.
pub fn cpu_auto() -> (u64, u64) {
    #[cfg(target_os = "macos")]
    {
        mac_cpu_auto(crate::profile::cores().0 as u64)
    }
    #[cfg(not(target_os = "macos"))]
    {
        (CPU_INFLIGHT, CPU_GROUPS)
    }
}

/// macOS "auto" on the CPU for `cores` physical cores.
pub fn mac_cpu_auto(cores: u64) -> (u64, u64) {
    let conc = round_games(MACOS_CPU_INFLIGHT_PER_CORE * cores.max(1));
    (conc, mac_groups(conc))
}

/// macOS: the groups of `conc` games in flight - batches of at least `MACOS_CPU_BATCH` rows (one group under it).
pub fn mac_groups(conc: u64) -> u64 {
    (conc / MACOS_CPU_BATCH).max(1)
}

/// macOS (0.3.3): the groups of `conc` games in flight when memory capped them below the cores' choice: `groups` kept,
/// the batches shrinking to at least `MACOS_CPU_MIN_BATCH` rows (fewer groups only below that; at least one).
pub fn mac_capped_groups(conc: u64, groups: u64) -> u64 {
    groups.min(conc / MACOS_CPU_MIN_BATCH).max(1)
}

/// A CPU job's cost until this PC measured the run's own: `CPU_COST`, on macOS `MACOS_CPU_COST`.
pub fn cpu_cost() -> MemCost {
    if cfg!(target_os = "macos") {
        MACOS_CPU_COST
    } else {
        CPU_COST
    }
}
/// A GPU job's cost until this PC measured the run's own: the GPU runtime commits more before any game.
pub const GPU_COST: MemCost = MemCost { base_mb: 1800, per_game_mb: 45 };
/// The games in flight are planned to commit at most this share of the memory cap (the job's limit): the rest is
/// headroom for games that run long and for the runtime's buffers.
pub const PLAN_SHARE: f64 = 0.85;
/// A rolling worker that commits this share of its job's memory limit is restarted with fewer games in flight (it
/// finishes its games first, PROTOCOL 10.6) before it can run into the limit.
pub const HIGH_SHARE: f64 = 0.92;
/// A worker that died with the job's peak memory at this share of the limit or more ran into the limit.
pub const HIT_SHARE: f64 = 0.97;
/// A restart takes at most this share of the games in flight before it (fewer when the measured cost says so).
pub const RESTART_SHARE: f64 = 0.75;
/// Restarts of one lease's worker for memory at most; the next memory failure ends the lease as a failure.
pub const MAX_RESTARTS: u32 = 4;
/// A memory cap of at least this many games in flight is rounded down to a multiple of it.
pub const RAM_CAP_STEP: u64 = 32;
/// agent 0.3.3: a worker's commit counts as its steady state once it finished this many times its games in flight with
/// them full (`steady_reading`): every game in flight then started after the worker did, at least twice over (a game
/// lasts ~10 minutes on the M4 Max at the start's layout).
pub const STEADY_TURNOVERS: u64 = 2;
/// ... and a steady reading is remembered with this margin (the cost per game it shows x 1.2, rounded up): the old native
/// worker's footprint still crept up after half an hour (11.5); the fixed one (zero-core fix/worker-memory, 2026-11-19:
/// 2.5 MB a game at 882 in flight after 12 minutes, growth tailing off; read as 3, whole MB rounded up) is planned at 4.
pub const LEARN_MARGIN: f64 = 1.2;
/// A steady cost at most this share of the cost a lease is planned with raises its games in flight (a layout handoff,
/// lease.rs `calibrate`); a smaller gain only waits for the next lease.
pub const RAISE_INFLIGHT_SHARE: f64 = 0.8;

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

/// The most games in flight `ram_mb` holds at `cost`: (`ram_mb` x `PLAN_SHARE` - base) / per game, rounded down to a
/// multiple of `RAM_CAP_STEP` (when at least that), at least 1.
pub fn ram_cap(ram_mb: u64, cost: MemCost) -> u64 {
    let room = (ram_mb as f64 * PLAN_SHARE) as u64;
    round_games(room.saturating_sub(cost.base_mb) / cost.per_game_mb.max(1))
}

/// `n` games in flight rounded down to a multiple of `RAM_CAP_STEP` when at least that, at least 1.
pub fn round_games(n: u64) -> u64 {
    if n >= RAM_CAP_STEP {
        n / RAM_CAP_STEP * RAM_CAP_STEP
    } else {
        n.max(1)
    }
}

/// The games in flight a restart for memory runs with (PROTOCOL 10.6): what fits the job's limit at the measured
/// cost, and never more than `RESTART_SHARE` of the games in flight before it (rounded as `round_games`, always
/// fewer than before); None when one game in flight already didn't fit.
pub fn restart_games(conc: u64, limit_mb: u64, cost: MemCost) -> Option<u64> {
    if conc <= 1 {
        return None;
    }
    let shrunk = ((conc as f64 * RESTART_SHARE) as u64).max(1);
    Some(round_games(ram_cap(limit_mb, cost).min(shrunk)).min(conc - 1))
}

/// The per-game cost a worker that committed `commit_mb` with `conc` games in flight shows over `cost`'s base (MB,
/// rounded up; 0 when it committed no more than the base).
pub fn measured_per_game(commit_mb: u64, conc: u64, cost: MemCost) -> u64 {
    commit_mb.saturating_sub(cost.base_mb).div_ceil(conc.max(1))
}

/// The memory a game in flight was measured to take on this PC, per run, worker build and provider class
/// (`state\rolling_memory.json`, keys `<run>/<build_id>/cpu` and `.../gpu`, MB per game over the base; agent 0.3.3 -
/// 0.2.14-0.3.2's `<run>/cpu` keys, measured with builds of before, are not read). The memory plan calibrates itself
/// (PROTOCOL 10.6): a build not measured here starts from the platform's conservative default (`cpu_cost`, `GPU_COST`);
/// a measured one is planned with what was measured - lower than the default too: a worker build fixed to need less
/// memory per game (zero-core fix/worker-memory: 13.1 -> 2.5 MB a game on the Mac) gets the games in flight it holds.
/// It goes up at once (`raise`) and down from steady readings with a margin (`steady`); the lease that measured it plans
/// with it from then on (lease.rs).
pub struct Learned;

impl Learned {
    fn path(layout: &Layout) -> PathBuf {
        layout.state().join("rolling_memory.json")
    }

    fn key(run: &str, build: &str, gpu: bool) -> String {
        format!("{run}/{build}/{}", if gpu { "gpu" } else { "cpu" })
    }

    pub fn load(layout: &Layout) -> BTreeMap<String, u64> {
        std::fs::read(Self::path(layout)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    /// The measured per-game cost of `run` with worker build `build` on the CPU or a GPU (None: never measured here).
    pub fn get(layout: &Layout, run: &str, build: &str, gpu: bool) -> Option<u64> {
        Self::load(layout).get(&Self::key(run, build, gpu)).copied()
    }

    /// The platform's default cost of a job on the CPU or a GPU (a build not measured here).
    pub fn default_cost(gpu: bool) -> MemCost {
        if gpu {
            GPU_COST
        } else {
            cpu_cost()
        }
    }

    /// What a job of `run` with worker build `build` is planned with: the default's base and the measured per-game cost
    /// (the default's when there is none), and whether there was a measurement.
    pub fn plan(layout: &Layout, run: &str, build: &str, gpu: bool, ram_mb: u64) -> MemPlan {
        let d = Self::default_cost(gpu);
        let m = Self::get(layout, run, build, gpu);
        MemPlan { ram_mb, cost: MemCost { per_game_mb: m.unwrap_or(d.per_game_mb).max(1), ..d }, measured: m.is_some() }
    }

    /// Records `per_game_mb` for `run` / `build` when it is more than what is planned with now (the stored value, else
    /// the default) -> true when it was.
    pub fn raise(layout: &Layout, run: &str, build: &str, gpu: bool, per_game_mb: u64) -> bool {
        let mut m = Self::load(layout);
        let k = Self::key(run, build, gpu);
        let now = m.get(&k).copied().unwrap_or(Self::default_cost(gpu).per_game_mb);
        if per_game_mb <= now {
            return false;
        }
        m.insert(k, per_game_mb);
        Self::save(layout, &m);
        true
    }

    /// agent 0.3.3: a steady reading (`steady_reading`) of `reading_mb` per game: remembered as `reading_mb` x
    /// `LEARN_MARGIN` (rounded up, at least 1) when that is less than what is planned with now (the stored value, else the
    /// default) -> the new value when it was lowered. Rises go through `raise`, at once.
    pub fn steady(layout: &Layout, run: &str, build: &str, gpu: bool, reading_mb: u64) -> Option<u64> {
        let mut m = Self::load(layout);
        let k = Self::key(run, build, gpu);
        let now = m.get(&k).copied().unwrap_or(Self::default_cost(gpu).per_game_mb);
        let v = ((reading_mb as f64 * LEARN_MARGIN).ceil() as u64).max(1);
        if v >= now {
            return None;
        }
        m.insert(k, v);
        Self::save(layout, &m);
        Some(v)
    }

    fn save(layout: &Layout, m: &BTreeMap<String, u64>) {
        let _ = std::fs::create_dir_all(layout.state());
        if let Err(e) = zerofarm_common::fsutil::atomic_write(&Self::path(layout), &serde_json::to_vec(m).expect("json")) {
            log::warn!("could not record a rolling job's memory per game: {e}");
        }
    }
}

/// agent 0.3.3: a worker's reading counts as its steady state: it finished `STEADY_TURNOVERS` times its `conc` games in
/// flight since it started, and its games in flight are full (at least 98 % of `conc`: a worker short of ids holds fewer
/// games than its per-game cost would be read over).
pub fn steady_reading(games: u64, started: u64, conc: u64) -> bool {
    let conc = conc.max(1);
    games >= STEADY_TURNOVERS * conc && started.saturating_sub(games) as f64 >= (0.98 * conc as f64).floor().max(1.0)
}

/// The memory a rolling job is planned within: the cap (`caps.ram_mb`, the job's limit), the cost per game, and
/// whether that cost was measured on this PC for the run (`Learned`): only then is a run's number capped too.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MemPlan {
    pub ram_mb: u64,
    pub cost: MemCost,
    pub measured: bool,
}

impl MemPlan {
    /// The games in flight a job may run: "auto"'s choice capped by memory; a run's number as it is, unless this PC
    /// measured that it doesn't fit.
    pub fn cap(&self, n: u64, auto: bool) -> u64 {
        if auto || self.measured {
            n.min(ram_cap(self.ram_mb, self.cost)).max(1)
        } else {
            n
        }
    }
}

/// A GPU job's groups for "auto": one at `GPU_ONE_GROUP_FROM` games in flight or more, else 2 (at most one per game).
pub fn gpu_groups(conc: u64) -> u64 {
    if conc >= GPU_ONE_GROUP_FROM {
        1
    } else {
        2.min(conc.max(1))
    }
}

/// A CPU job's (conc, groups): the run's number (in the template's groups, at most one per game), or "auto":
/// `cpu_auto()` (`CPU_INFLIGHT` games in `CPU_GROUPS` groups; more on macOS); capped by memory (`MemPlan::cap`; on
/// macOS the cores' groups kept, the batches shrinking to `MACOS_CPU_MIN_BATCH` rows, `mac_capped_groups`, 0.3.3).
pub fn cpu_shape(setting: Inflight, template_groups: u64, mem: MemPlan) -> (u64, u64) {
    match setting {
        Inflight::N(n) => {
            let n = mem.cap(n, false);
            (n, template_groups.clamp(1, n))
        }
        Inflight::Auto => {
            let (inflight, groups) = cpu_auto();
            let conc = mem.cap(inflight, true);
            let groups = if cfg!(target_os = "macos") { mac_capped_groups(conc, groups) } else { groups };
            (conc, groups.min(conc))
        }
    }
}

/// A GPU job's (conc, groups) from the bench of the job's network on the provider (rows/s by batch rows): the run's
/// number (in at most 2 groups), or "auto": among `GPU_STEPS` the games in flight after which doubling gains less than
/// `GPU_DOUBLING_GAIN` rows/s (each rated at its batch, `conc / gpu_groups(conc)` rows), in `gpu_groups` groups; capped
/// by memory (`MemPlan::cap`).
pub fn gpu_shape(setting: Inflight, rows: &BTreeMap<String, f64>, template_groups: u64, mem: MemPlan) -> (u64, u64) {
    match setting {
        Inflight::N(n) => {
            let n = mem.cap(n, false);
            (n, template_groups.clamp(1, 2).min(n))
        }
        Inflight::Auto => {
            let rate = |n: u64| crate::probe::rate_at(rows, n.div_ceil(gpu_groups(n)));
            let mut n = GPU_STEPS[0];
            for &next in &GPU_STEPS[1..] {
                match (rate(n), rate(next)) {
                    (Some(a), Some(b)) if b >= a * GPU_DOUBLING_GAIN => n = next,
                    _ => break,
                }
            }
            let conc = mem.cap(n, true);
            (conc, gpu_groups(conc))
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

    /// A plan with `ram_mb` at `per_game` MB per game over a base of `base` MB (`measured`: this PC measured it).
    fn plan(ram_mb: u64, base: u64, per_game: u64, measured: bool) -> MemPlan {
        MemPlan { ram_mb, cost: MemCost { base_mb: base, per_game_mb: per_game }, measured }
    }

    fn auto_cpu(ram_mb: u64) -> MemPlan {
        MemPlan { ram_mb, cost: cpu_cost(), measured: false }
    }

    fn auto_gpu(ram_mb: u64) -> MemPlan {
        MemPlan { ram_mb, cost: GPU_COST, measured: false }
    }

    #[test]
    fn memory_caps_the_games_in_flight() {
        // (ram x 0.85 - base) / per game, a multiple of 32 from 32
        assert_eq!(ram_cap(10_000, MemCost { base_mb: 1500, per_game_mb: 50 }), 128, "(8,500 - 1,500) / 50 = 140 -> 128");
        assert_eq!(ram_cap(1000, MemCost { base_mb: 0, per_game_mb: 10 }), 64, "85 -> 64");
        assert_eq!(ram_cap(1000, MemCost { base_mb: 0, per_game_mb: 30 }), 28, "below 32 as it is");
        assert_eq!(ram_cap(500, MemCost { base_mb: 1000, per_game_mb: 10 }), 1, "never 0");
        assert_eq!(ram_cap(500, MemCost { base_mb: 0, per_game_mb: 0 }), 416, "a per-game cost of 0 counts 1 MB");
        // the canary PC of 2026-11-17 (a 10,735 MB limit), GPU defaults: (9,124 - 1,800) / 45 = 162 -> 160; at the
        // soak's steady state (1.75 GB + 43 MB a game) that commits 8.6 GB, 80 % of the limit, under HIGH_SHARE
        let canary = ram_cap(10_735, GPU_COST);
        assert_eq!(canary, 160);
        assert!(((1750 + canary * 43) as f64) < 10_735.0 * HIGH_SHARE);
    }

    #[test]
    fn a_restart_runs_fewer_games_than_before() {
        let c = MemCost { base_mb: 1000, per_game_mb: 40 };
        // (10,000 x 0.75 - 1,000) / 40 = 162 -> 160, but at most 3/4 of 224 = 168 -> 160
        assert_eq!(restart_games(224, 10_000, c), Some(160));
        // fits more than 3/4: 3/4 (rounded)
        assert_eq!(restart_games(64, 100_000, c), Some(32), "48 -> a multiple of 32");
        assert_eq!(restart_games(10, 100_000, c), Some(7));
        assert_eq!(restart_games(2, 100_000, c), Some(1));
        assert_eq!(restart_games(1, 100_000, c), None, "one game didn't fit: nothing to shrink");
        assert_eq!(restart_games(40, 100, c), Some(1), "at least 1");
        // never as many as before, whatever fits
        for conc in [2u64, 3, 33, 64, 65, 100, 512] {
            assert!(restart_games(conc, 1_000_000, c).unwrap() < conc, "{conc}");
        }
    }

    #[test]
    fn the_cost_per_game_is_measured_over_the_base() {
        let c = MemCost { base_mb: 2000, per_game_mb: 60 };
        // the canary's crash: 10,735 MB at 224 in flight
        assert_eq!(measured_per_game(10_735, 224, c), 39);
        assert_eq!(measured_per_game(1999, 10, c), 0);
        assert_eq!(measured_per_game(2001, 0, c), 1, "no games counts 1");
    }

    #[test]
    fn the_measured_cost_is_remembered_per_run_build_and_provider() {
        let dir = std::env::temp_dir().join(format!("zf-rolling-mem-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let layout = Layout::new(&dir);
        assert_eq!(Learned::plan(&layout, "r", "b1", true, 10_000), MemPlan { ram_mb: 10_000, cost: GPU_COST, measured: false });
        // a rise is remembered at once, and only a rise over what is planned with
        assert!(Learned::raise(&layout, "r", "b1", true, 75));
        assert!(!Learned::raise(&layout, "r", "b1", true, 70), "only up");
        assert!(!Learned::raise(&layout, "r", "b1", false, cpu_cost().per_game_mb), "not over the default: nothing to remember");
        assert!(Learned::raise(&layout, "r", "b1", false, cpu_cost().per_game_mb + 2));
        assert_eq!(Learned::get(&layout, "r", "b1", true), Some(75));
        assert_eq!(Learned::get(&layout, "other", "b1", true), None);
        let p = Learned::plan(&layout, "r", "b1", true, 10_000);
        assert_eq!((p.cost.per_game_mb, p.cost.base_mb, p.measured), (75, GPU_COST.base_mb, true));
        // a new worker build starts fresh: the platform's default
        assert_eq!(Learned::plan(&layout, "r", "b2", true, 10_000), MemPlan { ram_mb: 10_000, cost: GPU_COST, measured: false });
        assert_eq!(Learned::get(&layout, "r", "b2", false), None);
        // 0.3.2's keys (no build: measured with builds of before) are not read
        std::fs::write(layout.state().join("rolling_memory.json"), r#"{"r/cpu": 99}"#).unwrap();
        assert_eq!(Learned::plan(&layout, "r", "b1", false, 10_000).cost, cpu_cost());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_steady_reading_lowers_the_plan_with_a_margin_and_a_rise_raises_it_at_once() {
        let dir = std::env::temp_dir().join(format!("zf-rolling-steady-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let layout = Layout::new(&dir);
        let d = cpu_cost().per_game_mb;
        // the fixed native worker (2026-11-19): 2.5 MB a game at its steady state, read as 3 (whole MB, rounded up) ->
        // planned at 4 (x 1.2, rounded up), below the platform's default: no max with it any more
        assert_eq!(Learned::steady(&layout, "r", "fixed", false, 3), Some(4));
        assert_eq!(Learned::plan(&layout, "r", "fixed", false, 10_000).cost.per_game_mb, 4);
        assert_eq!(Learned::steady(&layout, "r", "fixed", false, 2), Some(3), "lower again");
        assert_eq!(Learned::steady(&layout, "r", "fixed", false, 3), None, "3 x 1.2 = 4 is no lower than 3");
        // a rise is remembered at once
        assert!(Learned::raise(&layout, "r", "fixed", false, 7));
        assert_eq!(Learned::plan(&layout, "r", "fixed", false, 10_000).cost.per_game_mb, 7);
        // an old build at the default (no measurement): a steady reading near it changes nothing; one well under lowers it
        assert_eq!(Learned::steady(&layout, "r", "old", false, d), None);
        assert_eq!(Learned::steady(&layout, "r", "old", false, d - d / 4), Some(((d - d / 4) as f64 * 1.2).ceil() as u64).filter(|&v| v < d));
        // steady: twice the games in flight finished, the games in flight full
        assert!(steady_reading(1764, 2646, 882));
        assert!(!steady_reading(1763, 2645, 882), "one turnover and a bit");
        assert!(!steady_reading(5000, 5100, 882), "short of ids: few games in flight");
        assert!(steady_reading(2, 3, 1));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn mac_cpu_auto_fills_the_cores_and_fits_memory() {
        assert_eq!(mac_cpu_auto(16), (2304, 12), "M4 Max: the SSH self-play's 3 x 768 in batches of 192");
        assert_eq!(mac_cpu_auto(8), (1152, 6));
        assert_eq!(mac_cpu_auto(10), (1440, 7));
        assert_eq!(mac_cpu_auto(0), (128, 1));
        assert_eq!((mac_groups(1888), mac_groups(416), mac_groups(100)), (9, 2, 1), "batches of 192 rows at least");
        // 0.3.3: capped by memory, the groups stay and the batches shrink to 96 rows
        assert_eq!((mac_capped_groups(1472, 12), mac_capped_groups(691, 6), mac_capped_groups(320, 6), mac_capped_groups(50, 6)), (12, 6, 3, 1));
        let mac = |ram_mb| MemPlan { ram_mb, cost: MACOS_CPU_COST, measured: false };
        // 64 GB Mac, default cap (half the RAM): (27,852 - 1,024) / 18 = 1,490 -> 1,472 of the 16 cores' 2,304
        assert_eq!(ram_cap(32_768, MACOS_CPU_COST), 1472);
        assert_eq!(mac(32_768).cap(2304, true), 1472);
        assert_eq!(mac(32_768).cap(1024, false), 1024, "a run's number as it is until this Mac measured it");
        // the first validation lease's limit (half the RAM, less with little memory free): 1,408
        assert_eq!(mac(31_671).cap(2304, true), 1408);
        // 16 GB Mac (8 cores: 1,152 in 6 groups), 8 GB cap: (6,963 - 1,024) / 18 = 329 -> 320
        assert_eq!(mac(8192).cap(mac_cpu_auto(8).0, true), 320);
        // the plan fits what the crew measured on 2026-11-19 (16 MB a game over the base after 27 minutes, still rising
        // slowly): about 76 % of the cap there, 85 % at the planned 18 MB, under the 92 % where a worker gives way
        for ram in [8192u64, 16_384, 31_671, 32_768] {
            let n = mac(ram).cap(4096, true) as f64;
            assert!(1024.0 + n * 16.0 < ram as f64 * 0.8, "{ram}: {n}");
            assert!(1024.0 + n * 18.0 < ram as f64 * HIGH_SHARE, "{ram}: {n}");
            assert!(1024.0 + n * 16.0 > ram as f64 * 0.7, "{ram}: {n} (not far below)");
        }
        // ... while 0.3.1's 4 MB let 2,304 in, which committed 29,140 MB of its 31,671 MB limit at 15 minutes (92 %: the
        // memory check's restart), and 0.3.2's 14 MB let a crew of two run 882 each, whose main worker committed 14,487
        // MB of its 15,746 MB part (92 %) after 27 minutes
        assert_eq!(ram_cap(31_671, MemCost { base_mb: 1024, per_game_mb: 4 }).min(2304), 2304);
        const { assert!(29_140.0 > 31_671.0 * HIGH_SHARE) };
        assert_eq!(measured_per_game(29_140, 2304, MACOS_CPU_COST), 13, "what the agent remembered for the run");
        const { assert!(14_487.0 > 15_746.0 * HIGH_SHARE) };
        assert_eq!(measured_per_game(14_487, 882, MACOS_CPU_COST), 16, "what the crew remembered");
        if cfg!(target_os = "macos") {
            let (n, g) = cpu_auto();
            assert_eq!(cpu_shape(Inflight::Auto, 1, auto_cpu(1 << 20)), (n, g), "room for all: the cores' choice");
            // memory-capped: the cores' groups kept (a 16-core Mac's 2,304 in 12 groups -> 1,472 in 12 groups of 123)
            let c = n.min(1472);
            assert_eq!(cpu_shape(Inflight::Auto, 1, auto_cpu(32_768)), (c, mac_capped_groups(c, g)));
            assert_eq!(cpu_shape(Inflight::Auto, 1, auto_cpu(100)), (1, 1), "never 0");
        }
    }

    #[test]
    #[cfg(not(target_os = "macos"))]
    fn cpu_auto_is_32_in_four_groups_within_memory() {
        assert_eq!(cpu_shape(Inflight::Auto, 1, auto_cpu(16_000)), (32, 4));
        assert_eq!(cpu_shape(Inflight::Auto, 1, plan(2000, 0, 75, false)), (22, 4), "the memory cap: 1,700 / 75");
        assert_eq!(cpu_shape(Inflight::Auto, 1, auto_cpu(100)), (1, 1), "never 0");
        assert_eq!(cpu_shape(Inflight::N(24), 3, auto_cpu(100)), (24, 3), "the run's number as it is");
        assert_eq!(cpu_shape(Inflight::N(24), 3, plan(2000, 0, 75, true)), (22, 3), "unless this PC measured it doesn't fit");
        assert_eq!(cpu_shape(Inflight::N(2), 4, auto_cpu(16_000)), (2, 2));
    }

    #[test]
    fn gpu_auto_stops_doubling_below_15_percent_and_fits_memory() {
        // batch rows 32, 64 (2 groups) for 64, 128 in flight; 256, 512 (1 group) for 256, 512
        let climbing = rows(&[(1, 100.0), (32, 3000.0), (64, 4800.0), (256, 9000.0), (512, 11000.0), (1024, 11500.0)]);
        // 64 -> 128: 3000 -> 4800 (+60 %); 128 -> 256: 4800 -> 9000; 256 -> 512: 9000 -> 11000 (+22 %): 512, one group
        assert_eq!(gpu_shape(Inflight::Auto, &climbing, 1, auto_gpu(100_000)), (512, 1));
        // (10,000 x 0.85 - 1,500) / 50 = 140 -> 128, in 2 groups
        assert_eq!(gpu_shape(Inflight::Auto, &climbing, 1, plan(10_000, 1500, 50, false)), (128, 2));
        assert_eq!(gpu_shape(Inflight::Auto, &climbing, 1, plan(40_000, 0, 100, false)), (320, 1), "340 -> a multiple of 32, one group from 256");
        let flat = rows(&[(1, 100.0), (32, 3000.0), (64, 3300.0), (1024, 3400.0)]);
        assert_eq!(gpu_shape(Inflight::Auto, &flat, 1, auto_gpu(100_000)), (64, 2), "64 -> 128 gains 10 %");
        let r2 = rows(&[(32, 3000.0), (64, 6000.0), (256, 6600.0)]);
        assert_eq!(gpu_shape(Inflight::Auto, &r2, 1, auto_gpu(100_000)), (128, 2), "128 -> 256 gains 10 %");
        assert_eq!(gpu_shape(Inflight::N(300), &climbing, 4, auto_gpu(100)), (300, 2), "the run's number, at most 2 groups");
        assert_eq!(gpu_shape(Inflight::N(300), &climbing, 4, plan(40_000, 0, 200, true)), (160, 2), "measured: 170 -> 160");
        assert_eq!(gpu_shape(Inflight::Auto, &BTreeMap::new(), 1, auto_gpu(100_000)), (64, 2), "no numbers: the smallest step");
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

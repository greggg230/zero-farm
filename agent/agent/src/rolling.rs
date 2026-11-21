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
/// "auto" on a GPU (agent 0.3.4): the batch sizes (rows) tried, in order; the next is taken while doubling it gains
/// enough on the bench of the job's own network (`gpu_batch`). 0.2.13-0.3.3 tried 64-512 games in flight, rated at
/// half of them below 256: a 4070 Ti PC never got past 512, and its memory plan held it to 128.
pub const GPU_BATCH_STEPS: [u64; 6] = [32, 64, 128, 256, 512, 1024];
/// "auto" on a GPU: doubling the batch must raise the bench's rows/s at least this much (15 %).
pub const GPU_DOUBLING_GAIN: f64 = 1.15;
/// agent 0.3.4: a GPU job keeps this many full batches in flight (all its workers' games: `GPU_BATCHES` x the bench's
/// batch): while one batch is on the GPU the next one fills. The 4070 Ti PC of 2026-11-19 (legacy-meta3-r1 gen 769, the
/// CUDA build, 5-minute windows, leaves/s): one worker of 128 in flight in 2 groups 5,030 (GPU 37 % busy), 256 in one
/// group 8,379, 512 9,803, 1,024 10,825 (2 groups of 512: 10,697), two workers of 512 12,529, three of 384 14,154 (GPU 82
/// % busy); the bench's batch there is 512 rows, so 1,024 games in flight - where one worker levels off.
pub const GPU_BATCHES: u64 = 2;
/// A GPU job's worker runs at most this many groups (batches in turn).
pub const GPU_MAX_GROUPS: u64 = 2;
/// agent 0.3.4: a GPU job's workers share this many search threads per physical core (at most the logical processors;
/// the owner's `cpu_threads` instead when set). On the 4-core / 8-thread PC above: 6 - one worker 6, two 3 each, three
/// 2 each, as measured; the trainer's own torch path there found 6 search threads faster than 8.
pub const GPU_THREADS_PER_CORE: f64 = 1.5;
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

/// agent 0.3.4: the video memory a GPU job's worker holds (dedicated GPU memory, MB): `base_mb` before any batch (the
/// CUDA context, the network, the runtime's workspaces) and `per_row_mb` for each row of its biggest batch (the
/// activations). 0.2.13-0.3.3 didn't plan video memory: one worker of at most 512 games in flight never came near a
/// GPU's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct VramCost {
    pub base_mb: u64,
    pub per_row_mb: u64,
}

impl VramCost {
    /// What a worker batching `rows` rows is planned to hold.
    pub fn plan_mb(&self, rows: u64) -> u64 {
        self.base_mb + rows.max(1) * self.per_row_mb
    }
}

/// A GPU job's video memory until this PC measured the run's own with the lease's worker build (`Learned`): 1 GB + 2 MB
/// per batch row, about twice what the 4070 Ti PC above used - the whole GPU (its desktop included) held 709 MB with one
/// worker of 128 in flight, 1,605 MB with 512 in one group, 2,557 MB with three workers of 384 (nvidia-smi, 5-s samples).
pub const GPU_VRAM: VramCost = VramCost { base_mb: 1024, per_row_mb: 2 };
/// A GPU job's workers are planned within this share of the adapter's dedicated memory, less what other programs use.
pub const VRAM_SHARE: f64 = 0.85;
/// When the adapter's dedicated memory in use (every program's) reaches this share, a worker of the lease gives way.
pub const VRAM_HIGH_SHARE: f64 = 0.95;
/// Other programs' dedicated GPU memory when Windows doesn't say (no `GPU Adapter Memory` counters).
pub const VRAM_OTHERS_MB: u64 = 1024;

/// The video memory a GPU job's workers are planned within (MB): `VRAM_SHARE` of the adapter's `total_mb`, less what
/// other programs hold (`others_mb`).
pub fn vram_budget(total_mb: u64, others_mb: u64) -> u64 {
    ((total_mb as f64 * VRAM_SHARE) as u64).saturating_sub(others_mb)
}

/// The rows a batch of one of `n` workers sharing `budget_mb` may have at `cost` (0: not even its base fits).
pub fn vram_rows(budget_mb: u64, n: u32, cost: VramCost) -> u64 {
    (budget_mb / n.max(1) as u64).saturating_sub(cost.base_mb) / cost.per_row_mb.max(1)
}

/// A worker's fatal error or stderr says the GPU ran out of memory (ONNX Runtime's CUDA allocator: "CUDA failure 2: out
/// of memory ... cudaMalloc", cuBLAS / cuDNN "ALLOC_FAILED", DirectML's "E_OUTOFMEMORY" / "device removed" on a full
/// adapter).
pub fn gpu_oom(text: &str) -> bool {
    let m = text.to_ascii_lowercase();
    let gpu = ["cuda", "cudamalloc", "cublas", "cudnn", "gpu", "device", "directml", "dml", "vram"].iter().any(|k| m.contains(k));
    ["cuda failure 2", "cuda_error_out_of_memory", "cudaerrormemoryallocation", "status_alloc_failed", "e_outofmemory"].iter().any(|k| m.contains(k))
        || (gpu && (m.contains("out of memory") || m.contains("failed to allocate")))
}
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
/// agent 0.3.5: a GPU job's worker gives an **early reading** once it finished this share of its games in flight with
/// them full (and `Timings::early_calibration` after its start): the job's memory plan calibrates within minutes instead
/// of after `STEADY_TURNOVERS` (the 4070 Ti PC's first 0.3.4 lease, 2026-11-20: planned at the default 45 MB a game for a
/// build not measured there - 139 games in flight under its 9.5 GB cap, 5,179 leaves/s - while the memory-fixed worker
/// commits 2-3 MB a game; 2 x 139 games take ~4 minutes there, and the layout trials kept replacing the worker before).
pub const EARLY_TURNOVER: f64 = 0.5;
/// ... and an early reading is planned with this margin (the cost per game it shows x 2, rounded up): a young game holds
/// less than a grown one (the fixed worker: ~1.6 MB a game over the base at 5 minutes, 2-3 MB later). A later steady
/// reading lowers it further (x `LEARN_MARGIN`); more is remembered at once, as always.
pub const EARLY_MARGIN: f64 = 2.0;

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

/// agent 0.3.9: the same in KB (rounded up) - the steady and early readings the plan calibrates from: a whole MB rounded
/// up before the margin made 2.6 MB a game 3, and x 1.2 then 4 (0.3.3-0.3.8).
pub fn measured_per_game_kb(commit_mb: u64, conc: u64, cost: MemCost) -> u64 {
    (commit_mb.saturating_sub(cost.base_mb) * 1024).div_ceil(conc.max(1))
}

/// agent 0.3.9: a GPU worker's commit at its first status (its base reading) is short of its base by about this much -
/// CUDA loads kernels lazily as batch shapes come, and the runtime's arenas grow after the first status; counted per
/// game, it made the plan's cost per game too high. The 4070 Ti PC (2026-11-20, one worker, fp16 network, 10-minute
/// runs): first status 1,659 MB; 3.54 GB at 575 games in flight and ~2.6 MB a game more from 575 to 1,150 (commit and
/// working set alike) - a steady base of ~2.0 GB.
pub const GPU_BASE_DRIFT_MB: u64 = 400;

/// agent 0.3.9: the base a base reading plans with (a GPU worker's: plus `GPU_BASE_DRIFT_MB`).
pub fn planned_base(reading_mb: u64, gpu: bool) -> u64 {
    reading_mb + if gpu { GPU_BASE_DRIFT_MB } else { 0 }
}

/// The memory a game in flight was measured to take on this PC, per run, worker build and provider class
/// (`state\rolling_memory.json`, keys `<run>/<build_id>/cpu` and `.../gpu`, MB per game over the base; agent 0.3.3 -
/// 0.2.14-0.3.2's `<run>/cpu` keys, measured with builds of before, are not read). The memory plan calibrates itself
/// (PROTOCOL 10.6): a build not measured here starts from the platform's conservative default (`cpu_cost`, `GPU_COST`);
/// a measured one is planned with what was measured - lower than the default too: a worker build fixed to need less
/// memory per game (zero-core fix/worker-memory: 13.1 -> 2.5 MB a game on the Mac) gets the games in flight it holds.
/// It goes up at once (`raise`) and down from steady readings with a margin (`steady`); the lease that measured it plans
/// with it from then on (lease.rs).
///
/// **agent 0.3.14: a run not measured here borrows its worker build's measurement** (`find`). 0.3.3-0.3.13 looked up the
/// run's own key only, so every new run name - an experiment's fork, an A/B arm - started each PC at the platform's
/// default although the PC had measured the same worker build on another run (2026-11-21: both forks of the day began at
/// 45 MB a game, 268 games in flight on a 16 GB PC, where the build needs ~3). Each figure (the cost per game, the base,
/// the video memory per row) is now the run's own entry when it has one, else the highest entry another run has for the
/// same worker build and figure (provider class included), else the default. Not one key per build: the memory a game
/// takes also follows the run's play settings (simulations per move, the decks and so the game's length), which the build
/// doesn't fix - one shared figure would go up and down between two runs of a build, each switch to the hungrier run a
/// worker over its plan. The highest is the cautious pick (the file keeps no time, so "the newest" isn't known), and the
/// run's own readings take over from it as before: a rise is remembered for the run at once, a steady reading below what
/// it borrowed lowers it. Writes stay per run; existing entries are read as they are.
pub struct Learned;

/// The video memory per batch row figure (`Learned::vram_plan`).
const VRAM_FIGURE: &str = "vram";

/// agent 0.3.14: a figure `Learned` plans with and, when it is another run's (`Learned::find`), that run's name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub value: u64,
    pub from: Option<String>,
}

impl Learned {
    fn path(layout: &Layout) -> PathBuf {
        layout.state().join("rolling_memory.json")
    }

    /// The provider class's figure name: the cost per game `cpu` / `gpu`.
    fn class(gpu: bool) -> &'static str {
        if gpu {
            "gpu"
        } else {
            "cpu"
        }
    }

    fn entry_key(run: &str, build: &str, figure: &str) -> String {
        format!("{run}/{build}/{figure}")
    }

    pub fn load(layout: &Layout) -> BTreeMap<String, u64> {
        std::fs::read(Self::path(layout)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    /// agent 0.3.14: `figure` of `run` with worker build `build`: the run's own entry, else the highest entry another run
    /// has for that build and figure (ties: the first run name), else None.
    pub fn find(m: &BTreeMap<String, u64>, run: &str, build: &str, figure: &str) -> Option<Found> {
        if let Some(&v) = m.get(&Self::entry_key(run, build, figure)) {
            return Some(Found { value: v, from: None });
        }
        let tail = format!("/{build}/{figure}");
        let mut best: Option<Found> = None;
        for (k, &v) in m {
            if let Some(other) = k.strip_suffix(tail.as_str()) {
                if best.as_ref().is_none_or(|b| v > b.value) {
                    best = Some(Found { value: v, from: Some(other.to_string()) });
                }
            }
        }
        best
    }

    /// The per-game cost `run` with worker build `build` is planned with on the CPU or a GPU - its own measurement, else
    /// (agent 0.3.14) another run's of the same build (`find`); None: the build was never measured here.
    pub fn get(layout: &Layout, run: &str, build: &str, gpu: bool) -> Option<u64> {
        Self::find(&Self::load(layout), run, build, Self::class(gpu)).map(|f| f.value)
    }

    /// agent 0.3.14: the run whose measurement of `build` a job of `run` plans its cost per game with, when `run` has
    /// none of its own here (None: its own, or none at all).
    pub fn borrowed(layout: &Layout, run: &str, build: &str, gpu: bool) -> Option<String> {
        Self::find(&Self::load(layout), run, build, Self::class(gpu)).and_then(|f| f.from)
    }

    /// The platform's default cost of a job on the CPU or a GPU (a build not measured here).
    pub fn default_cost(gpu: bool) -> MemCost {
        if gpu {
            GPU_COST
        } else {
            cpu_cost()
        }
    }

    /// What a job of `run` with worker build `build` is planned with: the measured base (agent 0.3.6; the default's when
    /// there is none) and the measured per-game cost (the default's when there is none), and whether the per-game cost was
    /// measured (agent 0.3.14: either may be another run's of the same build, `find`).
    pub fn plan(layout: &Layout, run: &str, build: &str, gpu: bool, ram_mb: u64) -> MemPlan {
        let d = Self::default_cost(gpu);
        let m = Self::get(layout, run, build, gpu);
        let base = Self::base(layout, run, build, gpu).map(|b| planned_base(b, gpu)).unwrap_or(d.base_mb);
        MemPlan { ram_mb, cost: MemCost { base_mb: base, per_game_mb: m.unwrap_or(d.per_game_mb).max(1) }, measured: m.is_some() }
    }

    fn base_figure(gpu: bool) -> String {
        format!("{}_base", Self::class(gpu))
    }

    /// agent 0.3.6: the worker's measured base (MB) for `run` / `build` on the CPU or a GPU (None: not measured here;
    /// agent 0.3.14: another run's of the same build when `run` has none, `find`).
    pub fn base(layout: &Layout, run: &str, build: &str, gpu: bool) -> Option<u64> {
        Self::find(&Self::load(layout), run, build, &Self::base_figure(gpu)).map(|f| f.value)
    }

    /// agent 0.3.6: a base reading (`base_reading`): remembered when it is lower than what is planned with (the stored
    /// value - agent 0.3.14: or the one borrowed, `find` -, else the default) by more than `BASE_TOLERANCE` -> the value
    /// planned with from now on. Only lower: a reading holds the games' first memory too (more with more games in flight),
    /// and a base above the default is in the cost per game measured over it.
    pub fn set_base(layout: &Layout, run: &str, build: &str, gpu: bool, reading_mb: u64) -> Option<u64> {
        let mut m = Self::load(layout);
        let figure = Self::base_figure(gpu);
        let now = Self::find(&m, run, build, &figure).map_or(Self::default_cost(gpu).base_mb, |f| f.value);
        let v = reading_mb.max(MIN_BASE_MB);
        if v as f64 >= now as f64 * (1.0 - BASE_TOLERANCE) {
            return None;
        }
        m.insert(Self::entry_key(run, build, &figure), v);
        Self::save(layout, &m);
        Some(v)
    }

    /// Records `per_game_mb` for `run` / `build` when it is more than what is planned with now (the stored value - agent
    /// 0.3.14: or the one borrowed -, else the default) -> true when it was.
    pub fn raise(layout: &Layout, run: &str, build: &str, gpu: bool, per_game_mb: u64) -> bool {
        Self::raise_figure(layout, run, build, Self::class(gpu), Self::default_cost(gpu).per_game_mb, per_game_mb)
    }

    /// agent 0.3.3: a steady reading (`steady_reading`) of `reading_kb` KB per game: remembered as the reading x
    /// `LEARN_MARGIN` in MB (agent 0.3.9: rounded to the nearest MB from the KB reading - 0.3.3-0.3.8 rounded the reading
    /// up to a whole MB and the product up again; at least 1) when that is less than what is planned with now (the stored
    /// value - agent 0.3.14: or the one borrowed -, else the default) -> the new value when it was lowered. Rises go
    /// through `raise`, at once.
    pub fn steady(layout: &Layout, run: &str, build: &str, gpu: bool, reading_kb: u64) -> Option<u64> {
        // (never under the reading rounded up - what `raise` compares with: 1.1 MB x 1.2 rounds to 1, and the next
        // reading's 2 would raise it again at once)
        let v = ((reading_kb as f64 * LEARN_MARGIN / 1024.0).round() as u64).max(reading_kb.div_ceil(1024)).max(1);
        Self::lower_figure(layout, run, build, Self::class(gpu), Self::default_cost(gpu).per_game_mb, v)
    }

    /// agent 0.3.5: an early reading (`early_reading`) of `reading_kb` KB per game: remembered as the reading x
    /// `EARLY_MARGIN` in MB (rounded up, at least 1) when that is less than what is planned with now -> the new value when
    /// it was lowered.
    pub fn early(layout: &Layout, run: &str, build: &str, gpu: bool, reading_kb: u64) -> Option<u64> {
        let v = ((reading_kb.max(1) as f64 * EARLY_MARGIN / 1024.0).ceil() as u64).max(1);
        Self::lower_figure(layout, run, build, Self::class(gpu), Self::default_cost(gpu).per_game_mb, v)
    }

    /// `v` stored as `run`'s `figure` when lower than what is planned with now (`find`, else `default`) -> `v` when it was.
    fn lower_figure(layout: &Layout, run: &str, build: &str, figure: &str, default: u64, v: u64) -> Option<u64> {
        let mut m = Self::load(layout);
        let now = Self::find(&m, run, build, figure).map_or(default, |f| f.value);
        if v >= now {
            return None;
        }
        m.insert(Self::entry_key(run, build, figure), v);
        Self::save(layout, &m);
        Some(v)
    }

    /// agent 0.3.4: a GPU job's video memory per batch row over `GPU_VRAM`'s base, per run and worker build (key
    /// `<run>/<build_id>/vram`; agent 0.3.14: another run's of the same build when `run` has none, `find`), calibrated as
    /// the memory per game: a build not measured here is planned with `GPU_VRAM`; more is remembered at once
    /// (`vram_raise`), less from steady readings with the margin (`vram_steady`). -> (the cost, whether it was measured
    /// here).
    pub fn vram_plan(layout: &Layout, run: &str, build: &str) -> (VramCost, bool) {
        let m = Self::find(&Self::load(layout), run, build, VRAM_FIGURE).map(|f| f.value);
        (VramCost { per_row_mb: m.unwrap_or(GPU_VRAM.per_row_mb).max(1), ..GPU_VRAM }, m.is_some())
    }

    /// Records `per_row_mb` of video memory when it is more than what is planned with now -> true when it was.
    pub fn vram_raise(layout: &Layout, run: &str, build: &str, per_row_mb: u64) -> bool {
        Self::raise_figure(layout, run, build, VRAM_FIGURE, GPU_VRAM.per_row_mb, per_row_mb)
    }

    /// A steady reading of `reading_mb` of video memory per row, remembered with the margin when lower -> the new value.
    pub fn vram_steady(layout: &Layout, run: &str, build: &str, reading_mb: u64) -> Option<u64> {
        let v = ((reading_mb as f64 * LEARN_MARGIN).ceil() as u64).max(1);
        Self::lower_figure(layout, run, build, VRAM_FIGURE, GPU_VRAM.per_row_mb, v)
    }

    /// `v` stored as `run`'s `figure` when higher than what is planned with now (`find`, else `default`) -> true when it was.
    fn raise_figure(layout: &Layout, run: &str, build: &str, figure: &str, default: u64, v: u64) -> bool {
        let mut m = Self::load(layout);
        let now = Self::find(&m, run, build, figure).map_or(default, |f| f.value);
        if v <= now {
            return false;
        }
        m.insert(Self::entry_key(run, build, figure), v);
        Self::save(layout, &m);
        true
    }

    fn save(layout: &Layout, m: &BTreeMap<String, u64>) {
        let _ = std::fs::create_dir_all(layout.state());
        if let Err(e) = zerofarm_common::fsutil::atomic_write(&Self::path(layout), &serde_json::to_vec(m).expect("json")) {
            log::warn!("could not record a rolling job's memory per game: {e}");
        }
    }
}

/// agent 0.3.6: a worker's **base reading** is its commit at its first `status` after its sessions exist (it names a
/// provider) with games started and none finished: the runtime and the network, its games barely begun - the base the
/// plan counts per worker (each worker of a crew its own). 0.2.14-0.3.5 planned a fixed base (1,800 MB with a GPU, 600
/// on Windows' CPU, 1,024 on a Mac) and measured the cost per game over it.
pub fn base_reading(provider_named: bool, games: u64, started: u64) -> bool {
    provider_named && games == 0 && started > 0
}
/// A base reading is remembered when it differs from the stored one by more than this share.
pub const BASE_TOLERANCE: f64 = 0.05;
/// A base reading counts at least this much (MB).
pub const MIN_BASE_MB: u64 = 256;

/// agent 0.3.5: a GPU job's worker's reading counts as an early one: it finished `EARLY_TURNOVER` of its `conc` games in
/// flight, its games in flight full (as `steady_reading`).
pub fn early_reading(games: u64, started: u64, conc: u64) -> bool {
    let conc = conc.max(1);
    games as f64 >= (EARLY_TURNOVER * conc as f64).ceil() && started.saturating_sub(games) as f64 >= (0.98 * conc as f64).floor().max(1.0)
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

/// agent 0.3.7: a GPU worker of at least twice this many games in flight runs them in two groups, even when one batch
/// would hold them all: its search threads play one group while the other is on the GPU. With one group they wait for
/// every batch - the 4070 Ti PC of 2026-11-20 (fp16 network, 5-minute windows, leaves/s): a crew of 3 x 575 in one group
/// 22.3k (search threads ~60 % idle, GPU 56 % busy), 3 x 768 in two groups 24.6k, 3 x 1,150 in two groups 28.0k (GPU 75
/// %), 2 x 1,536 in two groups 27.0k; the trainer's own path there 32.8k.
pub const GPU_SPLIT_MIN_ROWS: u64 = 256;

/// agent 0.3.4: the groups of a GPU worker of `conc` games in flight whose full batch is `batch` rows: as many as make
/// batches of at most `batch` rows, at most `GPU_MAX_GROUPS`, at most one per game; agent 0.3.7: two once each keeps
/// `GPU_SPLIT_MIN_ROWS` rows.
pub fn gpu_groups(conc: u64, batch: u64) -> u64 {
    let split = if conc >= 2 * GPU_SPLIT_MIN_ROWS { 2 } else { 1 };
    conc.div_ceil(batch.max(1)).max(split).clamp(1, GPU_MAX_GROUPS).min(conc.max(1))
}

/// agent 0.3.4: the bench's full batch on a GPU (rows): among `GPU_BATCH_STEPS` the batch after which doubling gains
/// less than `GPU_DOUBLING_GAIN` rows/s (the smallest step without numbers).
pub fn gpu_batch(rows: &BTreeMap<String, f64>) -> u64 {
    let rate = |b: u64| crate::probe::rate_at(rows, b);
    let mut b = GPU_BATCH_STEPS[0];
    for &next in &GPU_BATCH_STEPS[1..] {
        match (rate(b), rate(next)) {
            (Some(a), Some(c)) if c >= a * GPU_DOUBLING_GAIN => b = next,
            _ => break,
        }
    }
    b
}

/// agent 0.3.4: the games in flight a GPU job targets in all (its workers'): `GPU_BATCHES` full batches, or the run's
/// number.
pub fn gpu_target(setting: Inflight, batch: u64) -> u64 {
    match setting {
        Inflight::N(n) => n,
        Inflight::Auto => (GPU_BATCHES * batch).min(MAX_INFLIGHT),
    }
}

/// agent 0.3.12 (PROTOCOL 13.1 **DirectML**): a DirectML job (`dml`, any vendor) starts from DirectML's own layout, not
/// from the bench's full batch: up to this many workers ...
pub const DML_WORKERS: u32 = 3;
/// ... each with this many games in flight ("auto") in `DML_GROUPS` groups - batches of `DML_ROWS` rows. DirectML pays a
/// large fixed cost per inference call (~130-140 ms on an RTX 4070 Ti whether the batch has 448 or 671 rows, the GPU 33-46
/// % busy), so it wants big batches and several processes calling at once. That PC (the worker's self-play on a real
/// network, 5-minute windows, leaves/s, 2026-11-20): 1 worker x 2,048 games in flight in two groups 3.3k, 2 x 2,048 7.5k,
/// 3 x 768 8.6k, 4 x 768 10.7k, 3 x 1,150 12.0k (the best; CUDA on that layout 25.3k). Agents 0.3.11 started a DirectML
/// job as any GPU but NVIDIA's: one worker of the bench's target, which the memory plan held to ~270 games in flight in
/// one group until its early reading, then one worker of 2,048 (~740 games an hour on that PC in a lease's first 15
/// minutes). 1,152: the measured best rounded to `tune::GPU_CONC_STEP`.
pub const DML_CONC: u64 = 1152;
/// A DirectML worker plays its games in this many groups (one plays on the CPU while the other is on the GPU).
pub const DML_GROUPS: u64 = 2;
/// A DirectML worker's rows per inference call at `DML_CONC`.
pub const DML_ROWS: u64 = DML_CONC / DML_GROUPS;
/// agent 0.3.12: where the memory plans hold less, a DirectML crew's workers each run fewer games in flight, down to this
/// many (batches of 384 rows), before the crew has a worker fewer: 3 x 768 played 8.6k on the 4070 Ti PC above, more than
/// 2 x 2,048 (7.5k) - DirectML's time per call barely depends on the batch, so more processes calling at once pay more
/// than bigger batches. One worker runs what the memory holds.
pub const DML_MIN_CONC: u64 = 768;

/// agent 0.3.12: the games in flight a DirectML job targets in all: "auto" `DML_WORKERS` x `DML_CONC` (3,456), at most
/// the lease's block of ids (`block`) and `MAX_INFLIGHT`; a run's number as it is. The coordinator's in-flight limit
/// (rev 21, 10.4) follows the agent's asks (about twice the crew's games in flight, a crew allowed twice its games in
/// flight), so it doesn't bound the start.
pub fn dml_target(setting: Inflight, block: u64) -> u64 {
    match setting {
        Inflight::N(n) => n,
        Inflight::Auto => (DML_WORKERS as u64 * DML_CONC).min(block.max(1)).min(MAX_INFLIGHT),
    }
}

/// agent 0.3.12: DirectML's start rule (PROTOCOL 13.1), as (workers, games in flight each) for `total` games in flight in
/// all (`dml_target`; `fixed`: a run's number, shared by the workers, else each worker at most `DML_CONC`), at most
/// `max_workers`, each one of n within `room(n)` (the games in flight the memory plans leave one of n workers; None: no
/// limit): the most workers that each keep `DML_MIN_CONC` - fewer games each first, then fewer workers -, else one worker
/// with what it holds. `tune::Space::dml_start` applies it within a lease's whole space (its video memory plan, the
/// owner's settings).
pub fn dml_crew(total: u64, fixed: bool, max_workers: u32, room: &dyn Fn(u32) -> Option<u64>) -> (u32, u64) {
    let per = |n: u32| {
        let share = total.max(1) / n.max(1) as u64;
        let p = if fixed { share } else { share.min(DML_CONC) };
        room(n).map_or(p, |r| p.min(r)).max(1)
    };
    (2..=max_workers.max(1)).rev().map(|n| (n, per(n))).find(|&(_, p)| p >= DML_MIN_CONC).unwrap_or((1, per(1)))
}

/// agent 0.3.12 (PROTOCOL 9.6): one DirectML worker's (conc, groups) as the measured choice rates a `dml` candidate - the
/// batch DirectML's start will use (`dml_crew` of `dml_target`: 1,152 games in flight in two groups of 576 rows for
/// "auto"), not the bench's CUDA-shaped target under the memory plan (0.3.11 rated DirectML at the ~192-270 rows a
/// default memory plan left one worker, where it is slow). The memory plan bounds it only when this PC measured the run's
/// cost per game (`mem.measured`): the default's conservative cost is replaced by the lease's early reading within
/// minutes, and the start picked again then (lease.rs `calibrate`).
pub fn dml_shape(setting: Inflight, block: u64, mem: MemPlan) -> (u64, u64) {
    let total = dml_target(setting, block);
    let room = |n: u32| mem.measured.then(|| ((mem.ram_mb as f64 * PLAN_SHARE / n.max(1) as f64) as u64).saturating_sub(mem.cost.base_mb) / mem.cost.per_game_mb.max(1));
    let (_, conc) = dml_crew(total, matches!(setting, Inflight::N(_)), DML_WORKERS, &room);
    (conc, gpu_groups(conc, DML_ROWS))
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

/// One GPU worker's (conc, groups) from the bench of the job's network on the provider (rows/s by batch rows): the run's
/// number (in at most 2 groups), or "auto" (agent 0.3.4): `GPU_BATCHES` full batches of the bench's batch (`gpu_batch`)
/// in groups of at most that batch (`gpu_groups`); capped by memory (`MemPlan::cap`). A GPU job's layout (its workers,
/// their games in flight and groups) is then the tuner's (lease.rs, PROTOCOL 13), starting from this target.
pub fn gpu_shape(setting: Inflight, rows: &BTreeMap<String, f64>, template_groups: u64, mem: MemPlan) -> (u64, u64) {
    match setting {
        Inflight::N(n) => {
            let n = mem.cap(n, false);
            (n, template_groups.clamp(1, GPU_MAX_GROUPS).min(n))
        }
        Inflight::Auto => {
            let b = gpu_batch(rows);
            let conc = mem.cap(gpu_target(setting, b), true);
            (conc, gpu_groups(conc, b))
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
        assert_eq!(Learned::borrowed(&layout, "r", "b1", true), None, "its own");
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

    /// agent 0.3.14: a run this PC never measured (a fork, an A/B arm) plans with what another run measured with the same
    /// worker build and provider class - the highest such figure -, not with the platform's default; its own readings take
    /// over from there and never change the other run's.
    #[test]
    fn a_new_run_plans_with_its_worker_builds_measurement() {
        let dir = std::env::temp_dir().join(format!("zf-rolling-borrow-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let layout = Layout::new(&dir);
        // what agents 0.3.3-0.3.13 left: the parent's figures (a 16 GB PC's CPU job), another run's of the build, a GPU's
        std::fs::create_dir_all(layout.state()).unwrap();
        std::fs::write(
            layout.state().join("rolling_memory.json"),
            r#"{"parent/b1/cpu":3,"parent/b1/cpu_base":500,"games2x/b1/cpu":4,"parent/b1/gpu":2,"parent/b1/vram":3,"old/b0/cpu":9,"mac-test/cpu":16}"#,
        )
        .unwrap();
        // the forks of 2026-11-21: 0.3.13 planned them at the platform's default (45 MB a game on Windows)
        let fork = Learned::plan(&layout, "fork", "b1", false, 16_384);
        assert_eq!(fork.cost, MemCost { base_mb: 500, per_game_mb: 4 }, "the highest of the build's CPU figures, and its base");
        assert!(fork.measured);
        assert_eq!(Learned::borrowed(&layout, "fork", "b1", false), Some("games2x".to_string()));
        assert_eq!(cpu_shape(Inflight::N(5000), 4, fork).0, ram_cap(16_384, fork.cost), "a measured plan caps a run's number too");
        assert!(ram_cap(16_384, fork.cost) > ram_cap(16_384, cpu_cost()));
        // the provider class and the build count: the GPU's own figure; another build, or none, the default
        let g = Learned::plan(&layout, "fork", "b1", true, 16_384);
        assert_eq!((g.cost.per_game_mb, g.cost.base_mb, g.measured), (2, GPU_COST.base_mb, true));
        assert_eq!(Learned::plan(&layout, "fork", "b2", false, 16_384), MemPlan { ram_mb: 16_384, cost: cpu_cost(), measured: false });
        assert_eq!(Learned::borrowed(&layout, "fork", "b2", false), None);
        assert_eq!(Learned::vram_plan(&layout, "fork", "b1"), (VramCost { per_row_mb: 3, ..GPU_VRAM }, true));
        assert_eq!(Learned::vram_plan(&layout, "fork", "b2"), (GPU_VRAM, false));
        // the fork's own readings compare with what it borrowed and are its own from then on
        assert!(!Learned::raise(&layout, "fork", "b1", false, 4), "no more than the 4 it plans with");
        assert!(Learned::raise(&layout, "fork", "b1", false, 6), "above the 4 borrowed (below the default 45: 0.3.13 kept nothing)");
        assert_eq!((Learned::get(&layout, "fork", "b1", false), Learned::borrowed(&layout, "fork", "b1", false)), (Some(6), None));
        assert_eq!(Learned::get(&layout, "parent", "b1", false), Some(3), "the parent's own unchanged");
        assert_eq!(Learned::get(&layout, "arm", "b1", false), Some(6), "the next new run borrows the highest, now the fork's");
        assert_eq!(Learned::steady(&layout, "arm", "b1", false, 2048), Some(2), "a steady reading below the 6 borrowed: 2.4 -> 2");
        assert_eq!(Learned::get(&layout, "fork", "b1", false), Some(6), "the fork's own unchanged");
        assert_eq!(Learned::early(&layout, "arm2", "b1", true, 3072), None, "6 is no lower than the 2 borrowed");
        assert_eq!(Learned::early(&layout, "arm2", "b1", true, 512), Some(1));
        assert_eq!(Learned::set_base(&layout, "arm3", "b1", false, 490), None, "within 5 % of the 500 borrowed, not of the default");
        assert_eq!(Learned::set_base(&layout, "arm3", "b1", false, 400), Some(400));
        assert_eq!(Learned::base(&layout, "parent", "b1", false), Some(500));
        assert!(!Learned::vram_raise(&layout, "arm4", "b1", 3) && Learned::vram_raise(&layout, "arm4", "b1", 4));
        assert_eq!(Learned::vram_steady(&layout, "arm5", "b1", 1), Some(2), "1 x 1.2 rounded up, below the 4 borrowed");
        // nothing was rewritten: every key of before still there
        let m = Learned::load(&layout);
        for k in ["parent/b1/cpu", "parent/b1/cpu_base", "games2x/b1/cpu", "parent/b1/gpu", "parent/b1/vram", "old/b0/cpu", "mac-test/cpu"] {
            assert!(m.contains_key(k), "{k}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn find_takes_the_runs_own_figure_else_the_builds_highest() {
        let m: BTreeMap<String, u64> =
            [("a/b/cpu", 5), ("c/b/cpu", 7), ("d/b/cpu", 7), ("e/b/cpu_base", 900), ("f/bb/cpu", 99), ("/b/cpu", 1), ("x/y/b/cpu", 2)].iter().map(|(k, v)| (k.to_string(), *v)).collect();
        assert_eq!(Learned::find(&m, "a", "b", "cpu"), Some(Found { value: 5, from: None }), "its own, though lower");
        assert_eq!(Learned::find(&m, "new", "b", "cpu"), Some(Found { value: 7, from: Some("c".into()) }), "the highest; a tie: the first name");
        assert_eq!(Learned::find(&m, "new", "b", "cpu_base"), Some(Found { value: 900, from: Some("e".into()) }), "per figure");
        assert_eq!(Learned::find(&m, "new", "b", "gpu"), None);
        assert_eq!(Learned::find(&m, "new", "bb", "cpu"), Some(Found { value: 99, from: Some("f".into()) }), "another build is no match: whole names");
        assert_eq!(Learned::find(&m, "", "b", "cpu"), Some(Found { value: 1, from: None }), "a lease without a run");
    }

    #[test]
    fn a_steady_reading_lowers_the_plan_with_a_margin_and_a_rise_raises_it_at_once() {
        let dir = std::env::temp_dir().join(format!("zf-rolling-steady-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let layout = Layout::new(&dir);
        let d = cpu_cost().per_game_mb;
        // a steady reading (KB, agent 0.3.9) of 3 MB a game -> x 1.2 = 3.6: planned at 4, below the platform's default (no
        // max with it any more)
        assert_eq!(Learned::steady(&layout, "r", "fixed", false, 3072), Some(4));
        assert_eq!(Learned::plan(&layout, "r", "fixed", false, 10_000).cost.per_game_mb, 4);
        assert_eq!(Learned::steady(&layout, "r", "fixed", false, 2048), Some(2), "lower again: 2.4, to the nearest MB");
        assert_eq!(Learned::steady(&layout, "r", "fixed", false, 2560), None, "the fixed native worker's 2.5 MB x 1.2 = 3 is no lower than 2");
        // a rise is remembered at once
        assert!(Learned::raise(&layout, "r", "fixed", false, 7));
        assert_eq!(Learned::plan(&layout, "r", "fixed", false, 10_000).cost.per_game_mb, 7);
        // an old build at the default (no measurement): a steady reading near it changes nothing; one well under lowers it
        assert_eq!(Learned::steady(&layout, "r", "old", false, d * 1024), None);
        assert_eq!(Learned::steady(&layout, "r", "old", false, (d - d / 4) * 1024), Some(((d - d / 4) as f64 * 1.2).round() as u64).filter(|&v| v < d));
        // steady: twice the games in flight finished, the games in flight full
        assert!(steady_reading(1764, 2646, 882));
        assert!(!steady_reading(1763, 2645, 882), "one turnover and a bit");
        assert!(!steady_reading(5000, 5100, 882), "short of ids: few games in flight");
        assert!(steady_reading(2, 3, 1));
        // agent 0.3.5: an early reading once half the games in flight finished, full; x 2 when lower than planned
        assert!(early_reading(70, 209, 139) && !early_reading(69, 208, 139) && !early_reading(70, 100, 139));
        assert_eq!(Learned::early(&layout, "r", "gpu-build", true, 2048), Some(4), "2 MB a game early: planned at 4 (default 45)");
        assert_eq!(Learned::early(&layout, "r", "gpu-build", true, 3072), None, "6 is no lower than 4");
        assert_eq!(Learned::early(&layout, "r", "gpu-build2", true, 1024), Some(2), "a reading under the base counts 1 MB (lease.rs): 2");
        // agent 0.3.9: a steady reading of 2.6 MB a game (KB) x 1.2 = 3.1: planned at 3 (0.3.3-0.3.8: 3 x 1.2 rounded up, 4)
        assert_eq!(Learned::steady(&layout, "r", "gpu-build", true, 2662), Some(3));
        // never under the reading rounded up: 1.1 MB x 1.2 = 1.3 -> 2, not 1 (a reading of 2 would raise 1 at once)
        assert_eq!(Learned::steady(&layout, "r", "small", true, 1126), Some(2));
        assert_eq!(measured_per_game_kb(3540, 575, MemCost { base_mb: 2059, per_game_mb: 4 }), 2638, "1,481 MB over 575 games, rounded up");
        assert_eq!(measured_per_game(3540, 575, MemCost { base_mb: 2059, per_game_mb: 4 }), 3);
        // agent 0.3.6: the base, measured - only lower, by more than 5 %; planned with
        assert!(base_reading(true, 0, 139) && !base_reading(false, 0, 139) && !base_reading(true, 1, 139) && !base_reading(true, 0, 0));
        assert_eq!(Learned::set_base(&layout, "r", "g", true, 1750), None, "within 5 % of the default 1,800");
        assert_eq!(Learned::set_base(&layout, "r", "g", true, 1500), Some(1500));
        assert_eq!(Learned::set_base(&layout, "r", "g", true, 1900), None, "never raised: a reading holds the games' first memory too");
        assert_eq!(Learned::set_base(&layout, "r", "g", true, 100), Some(MIN_BASE_MB));
        let p = Learned::plan(&layout, "r", "g", true, 9_300);
        assert_eq!((p.cost.base_mb, p.cost.per_game_mb), (MIN_BASE_MB + GPU_BASE_DRIFT_MB, GPU_COST.per_game_mb), "agent 0.3.9: a GPU base reading plans with its drift");
        assert_eq!((planned_base(1659, true), planned_base(1659, false)), (2059, 1659));
        assert_eq!(Learned::base(&layout, "r", "g", false), None, "per provider class");
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
    fn gpu_auto_targets_two_full_batches_and_fits_memory() {
        let climbing = rows(&[(1, 100.0), (32, 3000.0), (64, 4800.0), (256, 9000.0), (512, 11000.0), (1024, 11500.0)]);
        // 32 -> 64 rows: +60 %; 64 -> 128 (log-log between 64 and 256): +37 %; 128 -> 256 +37 %; 256 -> 512 +22 %; 512 ->
        // 1,024 +4.5 %: a full batch is 512 rows, the target 1,024 games in flight in 2 groups of 512
        assert_eq!(gpu_batch(&climbing), 512);
        assert_eq!(gpu_shape(Inflight::Auto, &climbing, 1, auto_gpu(100_000)), (1024, 2));
        // (10,000 x 0.85 - 1,500) / 50 = 140 -> 128, in 1 group (a batch of 128 <= 512 rows)
        assert_eq!(gpu_shape(Inflight::Auto, &climbing, 1, plan(10_000, 1500, 50, false)), (128, 1));
        assert_eq!(gpu_shape(Inflight::Auto, &climbing, 1, plan(40_000, 0, 50, false)), (672, 2), "680 -> a multiple of 32, 2 batches of 336");
        let flat = rows(&[(1, 100.0), (32, 3000.0), (64, 3300.0), (1024, 3400.0)]);
        assert_eq!(gpu_batch(&flat), 32, "32 -> 64 rows gains 10 %");
        assert_eq!(gpu_shape(Inflight::Auto, &flat, 1, auto_gpu(100_000)), (64, 2));
        let r2 = rows(&[(32, 3000.0), (64, 6000.0), (128, 9000.0), (256, 9900.0)]);
        assert_eq!(gpu_shape(Inflight::Auto, &r2, 1, auto_gpu(100_000)), (256, 2), "128 -> 256 rows gains 10 %");
        assert_eq!(gpu_shape(Inflight::N(300), &climbing, 4, auto_gpu(100)), (300, 2), "the run's number, at most 2 groups");
        assert_eq!(gpu_shape(Inflight::N(300), &climbing, 4, plan(40_000, 0, 200, true)), (160, 2), "measured: 170 -> 160");
        assert_eq!(gpu_shape(Inflight::Auto, &BTreeMap::new(), 1, auto_gpu(100_000)), (64, 2), "no numbers: the smallest step");
        // the canary PC's 4070 Ti (the agent's bench of legacy-meta3-r1 there, rows/s): 512 rows, 1,024 games in flight
        let canary = rows(&[(1, 300.0), (8, 2400.0), (32, 7000.0), (128, 16000.0), (512, 24000.0), (1024, 24000.0)]);
        assert_eq!((gpu_batch(&canary), gpu_target(Inflight::Auto, gpu_batch(&canary))), (512, 1024));
        // 0.3.3 planned that PC's lease at 1,800 MB + 45 MB a game: 128 in flight with a ~9.5 GB limit; at the 4 MB a game
        // measured there (2-3 MB, x 1.2, rounded up) the target fits
        assert_eq!(gpu_shape(Inflight::Auto, &canary, 1, plan(9_500, 1800, 45, false)), (128, 1));
        assert_eq!(gpu_shape(Inflight::Auto, &canary, 1, plan(9_500, 1800, 4, true)), (1024, 2));
        assert_eq!((gpu_groups(384, 512), gpu_groups(1024, 512), gpu_groups(4096, 512), gpu_groups(1, 512)), (1, 2, 2, 1));
        // agent 0.3.7: two groups once each keeps 256 rows, even under a bigger batch (the 4070 Ti PC's crew of 3 x 575
        // under its 1,024-row batch: two groups of 288 rows, not one of 575)
        assert_eq!((gpu_groups(575, 1024), gpu_groups(511, 1024), gpu_groups(512, 1024)), (2, 1, 2));
    }

    /// agent 0.3.12: DirectML's start - up to 3 workers of 1,152 games in flight in two groups of 576 rows (3,456 in all),
    /// within the lease's block; where memory holds less, fewer games each down to 768, then fewer workers - and the batch
    /// the measured choice rates a DirectML candidate at.
    #[test]
    fn directml_targets_its_own_crew_and_is_rated_at_its_batch() {
        assert_eq!((DML_CONC, DML_ROWS, DML_GROUPS, DML_MIN_CONC), (1152, 576, 2, 768));
        assert_eq!(dml_target(Inflight::Auto, 65_536), 3456);
        assert_eq!(dml_target(Inflight::Auto, 2_000), 2000, "at most the lease's block");
        assert_eq!(dml_target(Inflight::N(500), 65_536), 500, "a run's number as it is");
        // the crew: as many workers as each keep 768 games in flight, 1,152 at most each
        let none = |_: u32| None;
        assert_eq!(dml_crew(3456, false, 3, &none), (3, 1152));
        assert_eq!(dml_crew(3456, false, 2, &none), (2, 1152), "two allowed: 1,152 each, not 1,728");
        assert_eq!(dml_crew(3456, false, 1, &none), (1, 1152));
        // memory for 3 x 900: fewer games each first; 3 x 700 doesn't keep 768: two workers; 2 x 600 neither: one
        assert_eq!(dml_crew(3456, false, 3, &|n| Some([0, 2000, 1500, 900][n as usize])), (3, 900));
        assert_eq!(dml_crew(3456, false, 3, &|n| Some([0, 2000, 1500, 700][n as usize])), (2, 1152));
        assert_eq!(dml_crew(3456, false, 3, &|n| Some([0, 300, 150, 100][n as usize])), (1, 300), "one worker runs what it holds");
        // a run's number is shared: 2,048 -> 2 x 1,024 (3 x 682 < 768); 4,096 -> 3 x 1,365 (no per-worker cap)
        assert_eq!(dml_crew(2048, true, 3, &none), (2, 1024));
        assert_eq!(dml_crew(4096, true, 3, &none), (3, 1365));
        assert_eq!(dml_crew(2000, false, 3, &none), (2, 1000), "a block of 2,000");
        // the choice's batch: 1,152 in two groups (576 rows) unless this PC measured the run's cost and it holds less
        assert_eq!(dml_shape(Inflight::Auto, 65_536, auto_gpu(8_000)), (1152, 2), "the default 45 MB a game: not the start's bound");
        assert_eq!(dml_shape(Inflight::Auto, 65_536, plan(16_384, 1800, 3, true)), (947, 2), "measured: 3 x (4,642 - 1,800) / 3");
        assert_eq!(dml_shape(Inflight::Auto, 65_536, plan(16_000, 1800, 4, true)), (1152, 2), "3 x 683 doesn't keep 768: two of 1,152");
        assert_eq!(dml_shape(Inflight::Auto, 65_536, plan(4_000, 1800, 4, true)), (400, 1), "one worker of 400 in one batch");
        assert_eq!(dml_shape(Inflight::N(2048), 65_536, auto_gpu(8_000)), (1024, 2));
    }

    #[test]
    fn video_memory_is_planned_per_batch_row_and_learned_per_run_and_build() {
        // the 4070 Ti (12,282 MB) with 500 MB used by the desktop: 9,939 MB for the job; three workers of 384 rows plan
        // 3 x 1,792 MB, more than the whole GPU held then (2,557 MB) and well within it
        let b = vram_budget(12_282, 500);
        assert_eq!(b, 9_939);
        assert_eq!(GPU_VRAM.plan_mb(384), 1_792);
        assert!(3 * GPU_VRAM.plan_mb(384) <= b);
        assert_eq!(vram_rows(b, 3, GPU_VRAM), 1_144, "(3,313 - 1,024) / 2");
        assert_eq!(vram_rows(4_000, 2, GPU_VRAM), 488);
        assert_eq!(vram_rows(1_500, 2, GPU_VRAM), 0, "no room for a second worker's base");
        let dir = std::env::temp_dir().join(format!("zf-rolling-vram-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let layout = Layout::new(&dir);
        assert_eq!(Learned::vram_plan(&layout, "r", "b"), (GPU_VRAM, false));
        assert!(Learned::vram_raise(&layout, "r", "b", 5));
        assert!(!Learned::vram_raise(&layout, "r", "b", 4), "only up");
        assert_eq!(Learned::vram_plan(&layout, "r", "b"), (VramCost { base_mb: 1024, per_row_mb: 5 }, true));
        assert_eq!(Learned::vram_steady(&layout, "r", "b", 1), Some(2), "1 x 1.2, rounded up");
        assert_eq!(Learned::vram_plan(&layout, "r", "b2"), (GPU_VRAM, false), "per worker build");
        // the memory per game keys are apart
        assert_eq!(Learned::get(&layout, "r", "b", true), None);
        let _ = std::fs::remove_dir_all(&dir);
        // a GPU out of memory, told from a host one
        assert!(gpu_oom("onnxruntime::CudaCall CUDA failure 2: out of memory ; GPU=0 ; expr=cudaMalloc((void**)&p, size);"));
        assert!(gpu_oom("CUBLAS failure 3: CUBLAS_STATUS_ALLOC_FAILED"));
        assert!(gpu_oom("DirectML: E_OUTOFMEMORY"));
        assert!(!gpu_oom("ONNX Runtime: Non-zero status code returned while running Concat node. Status Message: bad allocation"));
        assert!(!gpu_oom("memory allocation of 1048576 bytes failed"));
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

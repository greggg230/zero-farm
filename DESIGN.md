# zero-farm: friends' idle PCs doing zero-core work

Status: phase 0 DONE 2026-11-11 = GO on the native worker (0a and 0b: internal reports; gates 1-2
passed, 1.5x the Python worker on CPU); phase 1: coordinator LIVE, agent 0.1.3 on canary (the canary
test PC), fuzz worker next.

## 1. Goal and decisions

A Windows app that invited friends install. It runs in the background and uses their otherwise idle
CPU and NVIDIA GPU to do work for zero-core training runs, MTG Zero first.

Decisions (user, 2026-11-11):

| Question | Decision |
|---|---|
| Who installs it | **Invited friends** (single-use invite codes). Not public. |
| Platforms, v1 | **Windows 10/11 x64**, CPU work on any PC (Intel or AMD CPUs), GPU work on **NVIDIA** RTX cards first; **AMD (and Intel) GPUs** through the same Windows ML path once a friend's PC passes the self-test (8.4). Mac/Linux later. |
| Network inference | **Native Rust worker + ONNX Runtime now** (no Python, no torch on friends' PCs). |
| Which runs | **Any run, including runs not conceived yet** — no farm change per run (2.1). |
| Updates | **Self-update, transparent to the user** (2.2). |
| Domain | `farm.greggg230.com`. |
| Code signing | Alpha unsigned; buy Authenticode signing before inviting more friends. |
| Engine binary | OK to give the compiled engine (private repo) to invited friends. |

Decisions I made (cheap to revisit, flagged here so the user can veto):

- **Game-agnostic in zero-core.** The worker library, the record format and the trainer's `pool`
  remote live in zero-core; MTG Zero is the first game that builds a worker. Dominion, Kingdom
  Builder etc. get it by building their own worker binary.
- **Coordinator on Cloudflare** (Worker + D1 + R2 + one Durable Object).
  Free R2 egress for checkpoints and worker builds; the desktop is never exposed to the internet.
- **Training never leaves the desktop (or a rented box).** Gradients are ~4 GB per generation; home
  uplinks can't carry that (measured on a rented cloud GPU, 2026-11-05). Friends play games; they don't train.

Non-goals for v1: public volunteers, anti-cheat beyond replay validation, Mac/Linux,
distributed training (1.1), credits/leaderboards beyond a simple contributors page.

### 1.1 Why one run's training stays on one machine

1. **It is the critical path, and friends' PCs come and go.** Every generation waits for training.
   A friend's PC drops out the moment someone touches the mouse; a synchronous training step that
   waits on it stalls the whole run. Self-play, evaluation and fuzzing tolerate loss: a lost game
   is simply played elsewhere.
2. **Bandwidth.** Data-parallel training exchanges a full gradient every step: 6–13 MB for these
   1.5–3.3 M-parameter nets, ~600 steps per generation (~103 ms/step on the 5090) ≈ 4–8 GB per
   generation per worker. Home uplinks carry 1–5 MB/s.
3. **Speed.** Training takes ~60 s per generation on the 5090 (Delver line, 11-05). Most friends'
   GPUs are slower; splitting it among them would make generations slower, not faster.
4. **Correctness.** An unstable (e.g. overclocked) GPU can return a wrong or non-finite gradient that
   the trainer cannot verify; mtg4 collapsed from a single non-finite step.

**How friends' GPUs still speed up training:** take all self-play off the 5090 so it trains
continuously (asynchronous actor/learner: `selfplay_local 0` on `feat/multi-remote`). On the
Delver line the 5090 currently shares each ~95 s generation between self-play and ~60–68 s of
training; with the self-play elsewhere, a generation approaches the training time alone.

**Later, not v1 (each behind an A/B against normal training):**
- a friend with a big GPU opts in to host a whole A/B run on their PC (as rented GPU boxes do
  today), with an opt-in Python + torch "trainer pack";
- low-communication training (local steps + periodic weight merging, DiLoCo-style). Records make
  the replay window cheap to ship (6 generations × 600 games × ~10 KB ≈ 36 MB, rebuilt into rows
  locally by replay), but it changes the optimizer, so it is a research A/B, not a drop-in.

## 2. Architecture

```
 friend's PC (no inbound ports)                  Cloudflare                       desktop (trainer)
 ┌──────────────────────────────┐   HTTPS   ┌──────────────────────┐   HTTPS   ┌────────────────────────┐
 │ farm-agent.exe  (tray, idle, │──────────▶│ farm coordinator     │◀──────────│ zero run (pool remote) │
 │   updates, leases, uploads)  │           │  Worker + D1 + DO    │           │  publishes checkpoints │
 │   └─ spawns ─▶ worker.exe    │◀──────────│  R2: builds/ ckpt/   │──────────▶│  pulls records,        │
 │      (engine + search + ORT) │  R2 GETs  │      results/        │  R2 GETs  │  REPLAYS them → shards │
 └──────────────────────────────┘           └──────────────────────┘           └────────────────────────┘
```

Three programs:

1. **farm-agent** (Rust, one per friend's PC, game-agnostic): tray icon, settings, idle detection,
   leases work, downloads and verifies signed worker builds and checkpoints, starts/suspends/kills
   the worker, uploads results, updates itself.
2. **worker** (Rust, one build per game and engine commit): the game engine + zero-core search +
   ONNX Runtime. Runs one job (fuzz / eval / self-play) and writes results. Knows nothing about
   HTTP, idle state or the coordinator; talks to the agent over stdin/stdout JSON lines.
3. **coordinator** (TypeScript Cloudflare Worker): enrollment, leases, the job mix per run,
   signed-artifact index, result index, a contributors page. Holds no signing key.

And one zero-core piece on the trainer: the **`pool` remote** (section 7).

### 2.1 Any run, including runs not conceived yet

A run is (game repo, pinned engine commit, pinned core commit, settings, checkpoints). The farm
must serve a new run, a new network architecture or a new game with **no farm change**:

1. **Engine-specific things travel as a signed build; run-specific things travel as data.** When a
   run that uses the pool starts (or is re-pinned), the pool remote looks up a worker build for its
   exact (game repo, engine commit, core commit). If there is none, it builds one from the run's
   pinned worktree, gates it (point 5), signs it and uploads it, then registers the run with its
   settings. Minutes, no human step.
2. **The network travels as a self-contained ONNX graph** (8.2, protocol `zero-onnx/1`). The graph
   takes the driver's batch as the driver produces it and returns the generic answer every
   zero-core evaluator returns; anything network-specific (token packing, readers, heads, aux
   transforms) is inside the graph, exported by zero-core's own exporter from the same `net.py`
   the run trains. A future architecture needs only that the exporter can export it, and the
   per-checkpoint gate proves it did.
3. **One implementation of every play-affecting rule, in Rust, shared by Python runs and the
   worker.** Seat parsing moves from `zero_py` into `zero_search` (serde), and the Python run
   calls the same parser; Python-only play logic a worker needs (league seats, forked starts,
   co-training episodes, per-game splitting) moves to Rust rather than being copied. A new
   setting then reaches the run and the worker in one change, and the worker build of that commit
   has it.
4. **Fail loudly, never ignore a setting.** Every worker build answers `check-config <settings>`:
   an unknown key, an unsupported seat kind (e.g. an `ext:` Node bot) or a missing capability
   makes the pool refuse to start, with the reason, while the run itself carries on with its own
   and remote workers. (Silent no-ops are bugs.)
5. **Automatic gates, on the desktop, before anything reaches a friend:**
   - per build: native worker vs the Python remote worker on a few games of that run's exact
     settings (CPU EP; rows equal up to float rounding, as Mac vs desktop today);
   - per checkpoint: ORT vs torch on stored batches of that run (seconds) — a failure keeps the
     checkpoint off the pool and says so loudly;
   - **per machine:** every published checkpoint ships a small reference set (~256 real batch rows
     + torch's answers, a few hundred KB). Before a PC uses a (checkpoint, execution provider, GPU)
     combination it runs the set and must match within the same 2e-5; a failure disables that
     provider for that run on that PC (it falls back to the CPU provider or other jobs) and
     reports GPU model, driver, provider version and the max difference. Re-run hourly while
     working, which also catches unstable (overclocked or overheating) GPUs. This is what lets
     hardware we can't test ourselves (AMD, Intel) take part safely;
   - per record: replay (6.1).

### 2.2 Updates are silent

Friends never see an update prompt, a UAC dialog or a restart request.

- **Worker builds and runtime packs** are fetched per lease by hash and cached (default cap 3 GB,
  least-recently-used evicted). A new run or re-pin is just a new hash.
- **The agent itself:** the logon entry starts a tiny launcher (`ZeroFarm.exe`, which almost never
  changes) that runs `versions\<v>\farm-agent.exe`. The agent learns of a new version from
  `GET /v1/agent/latest` and from `min_agent` in every lease answer, downloads it into
  `versions\<new>\`, verifies the signature, and switches between leases: it exits, the launcher
  starts the new version, and the new version must reach the coordinator within 2 min or the
  launcher goes back to the previous one and reports the failure. The last two versions are kept.
- **Compatibility:** a worker build declares the agent↔worker protocol it speaks; a job that needs
  a newer agent makes the agent update first instead of failing.
- **Rollout:** `farm-release release-agent` (desktop) builds, signs, uploads and sets `latest` per
  channel: `canary` (the user's own PCs) first, then `stable`. Each agent release is also a GitHub
  Release of the public repository (installer, package, signed `SHA256SUMS`; PROTOCOL 2.1), where
  the onboarding page's download link redirects; agents themselves update only from the
  coordinator.

## 3. farm-agent (the app friends install)

### 3.1 Install and run

- Per-user install to `%LOCALAPPDATA%\ZeroFarm`, no admin rights. Starts at logon from the user's
  `Run` key. It must run in the user's session, not as a service: services live in session 0 and
  cannot see keyboard/mouse idle time or fullscreen apps.
- **A friend installs only our installer** (~30–60 MB: launcher, agent, CPU ONNX Runtime). No
  CUDA toolkit, cuDNN, Python or anything else. The one prerequisite is the GPU driver they
  already have; GPU work needs a recent one, and if theirs is too old GPU work stays off (CPU work
  continues) and the tray says "update your NVIDIA driver to enable GPU work". GPU libraries,
  worker builds and checkpoints arrive later in the background, by themselves (2.2, 3.5).
- First run asks for an invite code → `POST /v1/enroll` → a per-machine token (stored with DPAPI).
- Tray menu: status (idle/working/paused, what job, games today), Pause for 1 h / until tomorrow /
  indefinitely, Settings, Quit. A toast once on first work, never afterwards.

### 3.2 When it works (defaults; all user-adjustable)

| Signal | Windows API | Default rule |
|---|---|---|
| User input idle | `GetLastInputInfo` | start after 5 min idle; yield within 2 s of input |
| Fullscreen app / game / presentation | `SHQueryUserNotificationState` (`QUNS_RUNNING_D3D_FULL_SCREEN`, `QUNS_BUSY`, `QUNS_PRESENTATION_MODE`) | never work |
| Power | `GetSystemPowerStatus` | never on battery or battery saver |
| Another GPU user | NVML: running compute/graphics processes, utilization | no GPU work while another process uses >10% for 30 s |
| GPU temperature | NVML | back off above 83 °C |
| Memory | `GlobalMemoryStatusEx` | keep ≥ 4 GB or 25% free, whichever is larger |

Modes: **Idle only** (default), **Always, low priority**, **Paused**, plus optional hours ("only
00:00–08:00"). Caps: CPU threads (default physical cores − 2), GPU on/off, max RAM.

CPU work runs at `IDLE_PRIORITY_CLASS` with Windows EcoQoS (`PROCESS_POWER_THROTTLING_EXECUTION_SPEED`)
so the scheduler prefers efficiency cores and the fans stay quiet.

### 3.3 Yielding without losing work

On user activity the agent sends the worker `yield`. The worker stops starting network batches,
**drops its ORT session and CUDA context (frees all VRAM)**, keeps its game trees in RAM, answers
`yielded`, and the agent suspends the process. On idle again: resume, `resume`, the worker rebuilds
the session (TensorRT-RTX engines are cached on disk, so this is seconds) and carries on. A worker
that does not answer within 2 s is killed; its lease's unfinished games are simply lost (leases are
small, section 5). A worker suspended longer than 30 min, or under memory pressure, is ended
cleanly after flushing finished games.

### 3.4 Bandwidth

Checkpoints ~13 MB (eoe1), fetched only when a job starts or the run publishes a new one, at most
once per 5 min. Uploads: game records, ~5–10 KB per self-play game (section 6.1), vs ~660 KB per
game for today's sample shards (eoe1 iteration 80: 397 MB for 600 games). A busy PC uploads well
under 100 KB/s.

### 3.5 Disk footprint on a friend's PC

Everything lives in `%LOCALAPPDATA%\ZeroFarm`; nothing else on the disk is read or written, and
uninstall removes the folder.

| Item | Size | Kept |
|---|---|---|
| launcher + agent | ~20 MB per version | last 2 versions |
| ONNX Runtime, CPU | ~20–30 MB | 1 |
| ONNX Runtime, NVIDIA providers (GPU PCs only) | TensorRT-RTX: a few hundred MB (measured in phase 0; on Windows 11 24H2+ Windows ML may already provide it); CUDA EP: ~1–1.5 GB (CUDA runtime + cuBLAS + cuDNN) | 1 |
| worker builds (engine + fuzz build) | ~25–30 MB per run commit | least recently used evicted |
| checkpoints (ONNX) | ~13 MB each | current + the evaluation's opponents |
| TensorRT engine cache (GPU PCs) | about the net's size per GPU | newest checkpoint only |
| unsent results | ~5–10 KB per game | capped at 200 MB: past that (e.g. offline) no new self-play leases |
| logs | rotating | 20 MB cap |

Typical total: **~150–300 MB on a CPU-only PC, ~0.5–2 GB on an NVIDIA PC**, under a hard cap
(default 3 GB, user-settable) that includes everything; the agent evicts least-recently-used
files and then declines jobs that would need more. Write volume is a few MB per hour (no SSD-wear
concern). RAM is capped separately (3.2).

## 4. Security (friends' PCs run our native code)

- **Everything executable is signed with an ed25519 release key** (minisign format) that never
  leaves the desktop (DPAPI-encrypted; the build tool uses it automatically, which per-run builds
  need: 2.1). Never on Cloudflare, GitHub or a rented box. Signed: agent updates and worker builds, each as a manifest
  `{kind, game, engine_commit, core_commit, files:[{name, sha256, size}]}`. The agent embeds the
  public key and refuses anything else. The coordinator only hands out URLs and hashes, so a
  compromised Cloudflare account can waste friends' CPU but cannot run code on their PCs.
- Checkpoints and job specs are data, checked by sha256 from the signed-or-authenticated lease;
  the worker parses them with size limits (an ONNX file is a parser attack surface: only load ONNX
  files whose hash the trainer published).
- No inbound ports, no remote shell, no VPN. The agent only talks HTTPS to the coordinator.
- The worker gets no network access and only its job directory (it never needs more): an
  AppContainer / restricted-token sandbox, planned before the stable channel (not in the 0.1 alpha,
  where the worker runs as the user at idle priority inside a Job Object).
- **Windows Defender risk:** an unsigned exe that uses the CPU/GPU in the background looks like a
  coin miner. Mitigations: Authenticode-sign the agent and worker (Azure Artifact Signing, about
  $10/month; check it accepts individual developers before relying on it), submit builds to
  Microsoft's false-positive portal, and tell friends what SmartScreen will show for the first
  unsigned alpha.
- Per-machine tokens; revoking one stops its leases and lets the trainer drop every record it
  produced (records carry the machine id, section 6).

## 5. Coordinator

Cloudflare Worker `zero-farm` at `farm.greggg230.com`; D1 `zero-farm`; R2 `zero-farm`; one Durable
Object `LeaseBook` per run (serialises lease grants and quota counting).

Friend-facing API (bearer = machine token):

| Call | Purpose |
|---|---|
| `POST /v1/enroll {invite, machine}` | invite → machine id + token |
| `POST /v1/lease {caps}` | caps = threads, RAM, GPU name/VRAM/driver, cached builds → a job or `{wait: s}` |
| `POST /v1/lease/:id/beat {progress}` | every 60 s → `continue` / `cancel` / `new_checkpoint` |
| `PUT  /v1/lease/:id/result/:n` | upload a result chunk (streamed to R2) |
| `POST /v1/lease/:id/done {stats}` | close the lease |
| `GET  /v1/agent/latest` | signed agent-update manifest |

Trainer-facing API (bearer = admin token, from the desktop only):

| Call | Purpose |
|---|---|
| `PUT /v1/admin/run/:run` | register a run: game, worker build, job mix, staleness limit |
| `PUT /v1/admin/run/:run/checkpoint/:gen` | publish a checkpoint (ONNX + sha256) |
| `GET /v1/admin/run/:run/results?after=` | list uploaded results since a cursor |
| `POST /v1/admin/run/:run/ack` | mark results ingested (R2 objects deleted after 7 days) |
| `POST /v1/admin/machine/:id/revoke` | revoke a machine |

Leases are small (≈5–10 min of work for that machine, sized from its measured rate) so a yield, a
crash or a shutdown loses little. A lease not heard from for 3 min is reissued. Contributors page:
machines, hours, games per day, last seen.

### 5.1 How games are handed out

- **Pull, never push.** An idle agent asks for work and states its capabilities (threads, RAM,
  GPU and VRAM, builds and checkpoints already cached). The coordinator picks a job by: the user's
  run priorities (weights per run, set from the desktop); what each run needs right now (self-play
  short of this generation's quota, evaluation pending); and fit for that machine (its measured
  rate per job kind; preferring builds/checkpoints it already has).
- **A lease** = run, worker build, checkpoint, settings, N games (≈5–10 min at that machine's
  rate) and a **disjoint game-id/seed block**: no two machines play the same game, and every game
  is reproducible from its record.
- **The worker plays its N games concurrently** (concurrency from its cores or GPU), uploads
  finished games' records every ~30 s rather than at the end, and heartbeats every 60 s. A yield,
  crash or shutdown loses only games in progress, which the coordinator reissues.
- **Quota:** the coordinator stops issuing a generation's self-play leases once accepted +
  in-flight games reach the quota + 10%; extra games carry over to the next generation (as remote
  games do today). Machines that find no self-play get evaluation, then **fuzzing, the infinite
  backfill**: there is always fuzz work and it needs no checkpoint, so no idle machine is wasted.
- **Rates:** a machine's rate per job kind comes from its real leases' Stats; before it has any,
  the coordinator uses its hardware class's average (no separate benchmark lease, as built).
- **Other games** (Dominion, Kingdom Builder, ...): same agent; a lease names that game's worker
  build, downloaded once.

### 5.2 PCs with and without a suitable GPU

Both contribute; the coordinator routes by measured rate.

| Machine | Gets |
|---|---|
| NVIDIA GPU with enough VRAM (≥ 6 GB to start; measured) | self-play and evaluation with GPU inference at high concurrency (hundreds of games in flight, batches of 256–1024); its CPU cores run the search at the same time |
| CPU only (or an unsuitable GPU) | self-play and evaluation with the CPU EP at small batches (these nets are small: the M4 Max's CPU beat its own GPU on MTG Zero; phase 0a measures x86), and fuzzing, which uses no network at all |

GPU-heavy self-play goes preferentially to GPU PCs and fuzzing preferentially to CPU-only PCs.
A GPU PC whose GPU is busy (a game running) can still take CPU work, if the user's settings allow
work while they are active.

### 5.3 Dropouts are normal, not errors

The coordinator and the trainer assume any PC can vanish at any moment.

| What happens on the friend's PC | What the farm does |
|---|---|
| Starts a game / touches the mouse | Agent yields in ≤ 2 s: the GPU worker drops its ONNX session and CUDA context (VRAM freed), the process is suspended (0% CPU). The agent keeps heartbeating "paused", so the lease stays alive; paused past the lease's slack (~10 min), the agent gives the lease back early: finished games are already uploaded, unfinished ones are reissued elsewhere. Idle again: resume, or take a new lease. |
| Shuts down, sleeps, loses network, kills the app | Heartbeats stop; after 3 min the coordinator reissues the lease's unfinished games. Finished games uploaded before (every ~30 s) are kept. |
| Comes back hours later with unsent games | Accepted if still within the run's staleness limit and not already played elsewhere (a game id counts once: the first copy wins); otherwise dropped. |
| Upload interrupted midway | Chunks are idempotent `PUT`s by sequence number and retried; a partial chunk is ignored. |

The run never waits for a friend: the pool holds no share of a generation's quota (section 7, point 3), so the
desktop (and any other remote workers) simply play whatever the pool doesn't deliver. The worst case
of a dropout is a few minutes of that PC's games in progress, never a stalled run.

### 5.4 Diagnostics and tuning on friends' PCs

Yes: from the first alpha. The point is to find, per kind of machine, what limits it and which
settings make it fastest, the way profiling found the desktop's serial loop and the Mac's
CPU-beats-MPS result, but across every friend's hardware.

**Always collected** (small; sent with heartbeats and lease results):

| What | Examples |
|---|---|
| Machine profile (at enroll and when it changes) | CPU model, performance/efficiency cores, RAM, GPU model/VRAM/driver, Windows build, which ONNX Runtime providers Windows ML offers |
| Per-lease performance | games/h, decisions/s, network leaves/s, batch sizes and inference latency (p50/p95), time split (search vs inference vs waiting), worker CPU %, GPU % and VRAM (NVML), peak RAM, concurrency/threads used |
| Thermals and throttling | GPU temperature, clocks and throttle reasons (NVML); CPU "% processor performance" (PDH) to spot throttling laptops |
| Availability | idle hours/day, yields per hour, yield time (input → VRAM freed), resume time, leases given back |
| Correctness | per-machine self-test max differences per provider, records rejected on replay |
| Failures | worker panics (message, backtrace, build id) and crashes; upload/download failures |

**On request only:** full local logs and ONNX Runtime per-operator profiles, uploaded when the
user asks for a machine's diagnostics from the desktop, or when the friend presses "Send
diagnostics" in the tray (a user-initiated upload, rate-limited to one per hour and 20 MB; the
coordinator built on 11-11 still refuses unrequested uploads with 409 - follow-up).

**Diagnostic jobs.** `diag` is just another lease kind, run under the same idle rules and signed
builds: a settings sweep (concurrency, search threads, inference threads, batch size, CPU vs GPU
provider, efficiency cores on/off, EcoQoS on/off), an ORT profile of a few minutes of real play,
a stability run that repeats the self-test under load. The desktop schedules them per machine or
per hardware class (`farm diag <machine|class> <kind>`).

**Autotuning.** On enrollment and whenever a new worker build or network arrives, the agent runs a
short sweep within the user's caps and keeps the fastest settings for that (machine, build,
network); the coordinator aggregates results by hardware class so new machines start from good
defaults. Settings never change what is played (search settings come from the run), only how fast.

**Where it goes.** Time series in Workers Analytics Engine, summaries in D1, logs/profiles in R2.
An admin dashboard (behind Cloudflare Access, desktop/user only): throughput and bottleneck
breakdown per machine and per hardware class, availability, failures, self-test results. Friends
see their own machine's stats in the tray and the contributors page.

**Privacy (friends).** Machine and performance data only: no file names, no user names, and no
process or game names (the agent sees that "a fullscreen app / another GPU process is active" in
order to yield, and only that boolean is recorded). Machines are a random id plus a name the friend
picks. A "What we collect" page is linked from the installer; raw time series kept 90 days, then
aggregated.

## 6. Work types and the record format

Job kinds, in the order they ship:

1. **fuzz**: run seeds `[a, b)` of a fuzz mode with no network; upload only failures with their
   repro data. Tests the whole agent/coordinator pipeline with zero risk to training data, and
   serves the no-approximations goal. This is already mtg-zero's model: `engine/tests/fuzz.rs`
   (`fuzz_random_games`, `fuzz_random_matches`) and `draft.rs` (`fuzz_random_events`) take
   `FUZZ_FIRST` / `FUZZ_GAMES` / `FUZZ_POOL` and the mode variables, and `tools/fuzz_farm.py`
   keeps a never-rewinding seed cursor per mode (from 10,000,000), dedupes failures by panic site,
   bisects hung chunks and stores `failures/<id>.json` with a repro string `(sha, env, seed)`.
   For the farm the fuzz loops move from the test files into an engine library module that both
   the tests and the worker call (no shipping of test executables), built with the existing
   `fuzz` profile (release + debug assertions + overflow checks) as a second worker binary.
   The card data is compiled in (`include_str!`), so the binary is self-contained. Throughput is
   large (~140k cube games or ~80k EoE games per core-hour), and friends' PCs can cover the modes
   the Mac farm skips today (eoe, sealed, delver). The coordinator owns each campaign's seed cursor
   (one row per (build, mode) in D1); a desktop tool pulls failures into mtg-zero's `failures/`
   in the same JSON format.
2. **eval**: play fixed checkpoints against each other or yardsticks; upload one row per game
   (result, margin, rounds, over) and, for `mtg_eoe`, its game-log line. A job is (seats, spec or
   eval spec, games, seed, paired) → per-game rows, the same thing `Run.evaluate` computes; the
   trainer keeps the Bradley-Terry Elo fit. The natural first target is the per-iteration
   evaluation (100 heuristic + 10 random games at 32 sims + paired Elo games; 217 s on the Mac's
   CPU, longer than run 2's 132 s iteration), split into many small leases. `ext:` (Node) bots
   and the Python race tool (`mtgz/sealed/compare.py`, 4–12 h per pool) stay on the desktop/Mac
   until ported. The no-network `search_smoke` configurations port easily too (the
   `engine/tests/az.rs:2253` uniform-evaluator pattern).
3. **selfplay**: play with the run's latest checkpoint; upload game records; the trainer replays
   them to rebuild training rows.

### 6.1 Self-play records instead of shards

A shard row is `x` (the features), `player`, `n_players`, `opt_off`/`opt_action`, `pi` (f16),
`z`/`q`/`sc` (f16 [P]), `net`, and optionally `aux` (zero-core `data.py:149-195`). Most of it can
be **recomputed by replaying the game**: the live game is never determinized (only search clones
are), so `new_game(spec, n, seed)` followed by the real moves reproduces every state, and
`encode(decider)` on it reproduces `x`; the legal options give `opt_action`; the game's
`aux_target` gives `aux`; the terminal result gives `z` and `sc`. Only what the search produced
must travel:

```
record (one game; zstd-compressed CBOR, ~5–10 KB for an EoE game):
  header:  run, build_id, net (checkpoint id), machine id, spec, n_players, game seed, gid,
           per-seat deck where the game builds one (sealed)
  steps:   every real move in order, including forced single-option steps and external answers
           (u16 action ids; EoE ~530 decisions per game ≈ 1 KB)
  samples: for each training row (EoE ~129 per game): step index, decider, kind (leaf /
           value-only), pi as f16 over that step's options (bit-identical to what a shard would
           hold), q (f16 [P])
  result:  per-seat result and margin, rounds, aborted/resigned flags (cross-checked on replay)
```

The engine already promises this: `Game` "replays exactly from its seed and actions" (mtg-zero
`AGENTS.md:37-39`), tested by `engine/tests/game_log.rs:170-190` (rebuild from spec + seed +
actions, fingerprint and result bit for bit), and `mtg_eoe` already writes a v2 game log with the
actions (`engine/src/az/game_log.rs`). What the log lacks is exactly the search output above (pi,
q, which decisions were full searches) plus the checkpoint id and driver seed. One determinism
risk: the sealed deck builder calls the platform's libm `exp`/`powf`
(`az/sealed/builder.rs:322,395,435`), so a deck rebuilt from seeds on another OS could differ.
Friends and the trainer are all Windows in v1, the record carries the decks anyway, and replay
compares them; switching the builder to a portable libm is the real fix before Mac/Linux workers.

The trainer replays each record with the **same engine build** (records name their build; a
record from another build is refused, as an older checkout is today, az.md 18.5). Replay is the
validation: an illegal action, an options count that differs from `pi`'s length, or a terminal
result that differs from the header rejects the whole record and counts against that machine.
Search outputs (`pi`, `q`) cannot be verified without re-searching; for invited friends that is
acceptable, and records carry the machine id so a bad machine's data can be dropped later.

Needed in zero-core: a game-seed derivation the worker reports (today `Rng(split64(seed ^
split64(stream+1)))`, `worker.rs:425-429`), a `Record` → step-index link from `take_samples`, and
a replay-to-rows function in Rust (`new_game` → `step` → `encode`) the trainer calls through PyO3.
A per-game equivalence test pins it: for N self-play games, shard rows written directly ==
rows rebuilt from their records, bit for bit.

## 7. Trainer side (zero-core)

A new remote kind `pool:<run>` in zero-core's remote self-play, on top of `feat/multi-remote`
(selfplay remotes as a list, `selfplay_local 0` for a trainer that only trains; built 11-05, not
merged into zero-core main yet). Unlike the existing remote-worker kind it has no sessions; it is one thread of the
run process that:

1. **Publishes** checkpoint i-1 at each phase start: ONNX export (8.2) → upload to R2 →
   `PUT /v1/admin/run/:run/checkpoint/:gen`. Friends' workers switch to it at their next game
   boundary (rolling, like `selfplay_remote_linger`).
2. **Ingests** results: polls `GET …/results`, downloads records, replays each through the engine
   (a new PyO3 call `replay_rows(record) -> sample arrays`, Rust underneath), and writes ordinary
   shards into `data/iter_i/` (`meta.source = 'pool'`, `meta.machine`), through the same
   non-finite guard and (game, features, actions) check as remote shards. Rejected records are
   counted per machine and kept in `<run>/debug/pool/` for a look.
3. **Counts** accepted games toward the iteration's quota exactly as a remote worker's accepted
   games count today (workqueue.py: the iteration ends when `games` are accepted; extra games are
   held for the next iteration). The pool never holds a share of the quota, so friends going idle
   or offline only means the desktop plays more. Wire rule (PROTOCOL rev 2): `accept` is the
   trainer's running total for the generation, including held-over games, and is sent before
   `ack`; otherwise self-play can briefly overshoot the cap.
4. **Staleness:** a record played with checkpoint `net` is accepted into iteration i while
   `net ≥ i-1-pool_max_stale` (default 1, the same one-generation bound linger has; raise it if
   friends' rate makes that wasteful), dropped after that; every accepted row goes into the
   iteration's staleness histogram as remote rows do now.

Evaluation uses the same thread: `eval_remote pool:<run>` posts the evaluation's games as leases,
collects per-game rows, then runs `elo_update` locally. Fuzz campaigns are not tied to a run
(5, 6).

Branching note: the farm's zero-core work must start from mtg-zero's PINNED core (f9ca5c3, branch
remote-eval-files, not on GitHub yet), not from the standalone zero-core main (d499716), which
can't load eoe1 checkpoints. Normal flow after: merge to zero-core main, push, re-pin.

Worker builds come from the desktop: it is Windows/MSVC like the friends, so no CI is needed. A
mtg-zero tool builds `mtgz-worker.exe` (release, with mimalloc: −40–48% search time vs the Windows
heap) and `mtgz-worker-fuzz.exe` at the run's pinned commit, writes the manifest, signs it with
the offline key and uploads it; the run registers that build id. ONNX Runtime and its provider
DLLs ship as a separate signed "runtime pack" that rarely changes, so a new engine commit is a
download of ~25 MB, not hundreds.

## 8. Native worker and ONNX

### 8.1 What the worker links and what it re-implements

- **Links as-is:** zero-core's `zero_search` crate (its only dependency is `serde_json`; no PyO3)
  and the game's engine crate (`impl SearchGame`, depends only on `zero_search`). The driver
  (`Driver<G>`: rolling, `queue_only`, paired, sparse), playout caps, root noise, temperature,
  move rule, policy targets (visits / 'cq'), resignation and sample records are all Rust already.
- **Re-implements in Rust** (today Python or PyO3 glue):
  1. seat dict → `NnSeat` with defaults, `branch`/`starts`, `DriverCfg` assembly (`zero_py`
     lib.rs:156-302, 796-822; `games.nn_seat`). Best done as serde support in `zero_search` so
     Python and the worker share one parser.
  2. the network evaluator: ORT session on a `zero-onnx/1` graph + the generic post-processing
     every zero-core evaluator does (gather each row's legal options from `policy`, max-subtracted
     softmax per segment in f32, `value`/`margin` passed through, `aux` for belief rows). The
     TokenNet-specific packing (`presence_sparse`, `token_index`, padding; `infer.py` 230-253 and
     487-535 of the pinned copy) goes inside the graph (8.2), not into the worker.
  3. the drive loop: up to 2 batches in flight, results submitted in launch order (`infer.drive`).
  4. the per-game splitting and tallies `workqueue.py` / `shardthread.py` do; the record writer.
- **Stays on the trainer:** `value_q` mixing (training time), the Bradley-Terry Elo fit, league
  bookkeeping, node-based yardsticks (`ext:` bots can't run natively, so eval jobs that need them
  stay on the desktop).

### 8.2 ONNX export of TokenNet

Start from `TokenNet.forward_static` (the CUDA-graph entry point: fixed shapes, float32 only, no
data-dependent shapes or host syncs). Today it takes host-packed inputs (`infer.py` 358-368 of
the pinned core): `x` f32 [Bp, F], `n_players` i64 [Bp], `full` i64 [Bp, Tp] token positions,
`ntok` i64 [Bp] → `out` [Bp, 1040] (policy logits 1024 ‖ value 8 softmaxed ‖ margin 8) and `aux`
[Bp, 2048]. The host packing (`presence_sparse`, `token_index`, padding) is TokenNet-specific, so
for 2.1 it moves **inside** the exported graph. The exported interface is the same for every
architecture:

**`zero-onnx/1`**

| | name | type | shape | notes |
|---|---|---|---|---|
| in | `x_ptr` | i64 | [B+1] | the driver's sparse batch as produced (`DriverCfg::sparse`) |
| in | `x_idx` | i32 | [nnz] | |
| in | `x_val` | f32 | [nnz] | |
| in | `n_players` | i64 | [B] | |
| in | `belief` | bool | [B] | rows whose aux answer is wanted (`ROW_BELIEF`) |
| out | `policy` | f32 | [B, A] | raw logits; the worker gathers each row's legal options and softmaxes per segment (zero-core's generic step for every architecture) |
| out | `value` | f32 | [B, 8] | probabilities, zero beyond `n_players` |
| out | `margin` | f32 | [B, 8] | |
| out | `aux` | f32 | [B, W] | already transformed per target (softmax / exp / raw); W = 0 for nets without the head |

Model metadata carries `zero_onnx=1`, the game, features F, actions A, aux layout W, the
checkpoint id and its sha256. A future change that cannot fit this interface becomes
`zero-onnx/2`, which a worker build declares and the agent update (2.2) brings in.

Sparse inputs matter for speed: dense uploads were the desktop's measured bottleneck (az.md
10.6/10.7).

**Phase 0a result (2026-11-11, zero-core `farm` 508a193 + 4edb1e7, az.md 41; internal report):**
TokenNet, GridNet and the MLP Net all export with torch 2.11's dynamo exporter (opset 18, one file,
~the checkpoint's size: 13.4 MB eoe1, 9.7 MB Delver), including readers, a widened aux head and no
aux head. Packing = **`packed`**: in-graph compaction where the per-token layers run only on the
present tokens and only attention/pooling are padded; the worker splits batches over 64 rows into
chunks grouped by non-zero count (no network knowledge needed). Rows average only 15–21 present
tokens of 170, so masking all 170 is 5–10× slower and padded compaction / host packing 1.3–2.5×
slower. CPU EP vs torch at 1 thread: 1.09–1.69× torch eager on 16–64-row batches, 0.90–1.26× at
256; ORT keeps scaling with threads where torch eager collapsed on the loaded desktop. Session load
0.2–0.4 s. Still open: whether TensorRT-RTX accepts `packed`'s data-dependent shapes (`mask` has the
same interface as a fallback).

**Export rule learned:** ONNX Runtime's CPU ScatterND with `reduction='add'` races on duplicate
indices across threads (wrong, run-to-run different sums); the first `packed` pooling failed 4 of 36
eoe1 cases at up to 0.058 because of it. Exports must never use `index_add` or accumulate-style
scatters, and the gate runs every case twice and requires bit-identical answers.

Current nets are small (d=128, 4 heads; 4–16 layers; 1.5–3.3 M parameters; 12.75 MB for eoe1), so
a checkpoint ships as an ONNX file of about the same size. The trainer exports each published
checkpoint (`net.build(config)` + `load_state_dict`, against the run's PINNED zero-core: the
standalone zero-core main can't load eoe1 checkpoints).

Watch-outs: export with dynamic B/T, opset ≥ 18 (exact GELU, LayerNorm, SDPA with a boolean mask);
ScatterElements with duplicate indices into the dummy policy column (harmless, check ORT accepts
it); int64 identity decoding (fine on CUDA/TensorRT, weak on DirectML — not a v1 target); turn
**TF32 off** in ORT for fp32 parity. Nothing in either repo exports ONNX yet; torch 2.11's default
dynamo exporter needs `onnxscript`.

### 8.3 Parity gate (new code is right only if it matches)

1. **Network:** ORT vs torch `forward_static` on recorded real driver batches (leaf, value-only
   and belief rows; 1–1024 rows; junk in padding and NaN in absent slots, as
   `tests/test_token_graphs.py` does): priors, value, margin, aux within 2e-5 max abs (the
   existing graphs-vs-eager tolerance; measured baseline ~3e-6), every case run twice and
   bit-identical. **PASSED** in phase 0a: eoe1 iter 80, 198 cases on real driver batches + 2,048
   shard rows, worst margin 1.5e-5 (priors ≤ 1.4e-6, value ≤ 3.7e-6, aux ≤ 5.0e-6); Delver deep8
   iter 3293 (layout v5, 8 layers), 52 cases on shard rows, worst 4.6e-6. eoe1's margin has only
   ~25% headroom; against a float64 forward ORT is as accurate as torch (4.6e-6 vs 9.9e-6), so the
   gap is two float32 roundings, not an ORT error.
2. **Worker:** the native worker and the Python remote worker, same seeds and checkpoint, CPU EP:
   game results and sample rows equal up to the networks' float rounding; then a paired `h2h`
   (native vs Python) scoring ~0.5, as graphs vs eager did.
3. **Records:** rows rebuilt from records == rows the worker would have written as a shard,
   bit for bit (section 6.1).

### 8.4 Execution providers

| GPU | Provider | Where it comes from |
|---|---|---|
| NVIDIA RTX (Ampere and newer) | **TensorRT-RTX EP** (ORT's recommended provider for consumer RTX; compiles an engine per GPU on first use, cached) | Windows ML on Windows 11 24H2+ (via Windows Update), or shipped with our runtime pack |
| NVIDIA, any CUDA GPU | **CUDA EP** | our runtime pack: CUDA runtime + cuBLAS + cuDNN, ~1–1.5 GB |
| **AMD Radeon** | **MIGraphX EP** (AMD's GPU provider for Windows ML, on ROCm) | Windows ML on Windows 11 (via Windows Update; needs a recent AMD driver) |
| Intel Arc / iGPU | OpenVINO EP | Windows ML |
| any DirectX 12 GPU | DirectML EP ("legacy" in Microsoft's list) | in-box; the only GPU option on Windows 10 for AMD |
| every PC | **CPU EP** | our runtime pack |

Building the GPU path on **Windows ML's provider catalog** means one code path picks the right
provider for whatever GPU a friend has: AMD and Intel come with no extra worker code, only the
per-machine self-test (2.1 point 5) deciding whether that provider is good enough on that PC. We
have no AMD GPU to test on, so AMD's first real check is a friend's PC running the self-test.
Risks to measure: whether each provider runs every node of our graph (the int64 identity
decoding, GatherElements/ScatterElements and boolean-mask attention were flagged for DirectML;
nodes a provider can't run fall back to the CPU, which can be slow) and whether it keeps fp32 (a
provider that silently drops to fp16 fails the 2e-5 self-test and is not used). Windows 10 PCs
get the CPU provider, plus the CUDA EP for NVIDIA.

Rust binding: the `ort` crate (2.0 release candidates) with `load-dynamic`, so the agent can load
either our own ONNX Runtime build or Windows ML's and register the provider libraries the catalog
offers. **NVIDIA default: TensorRT-RTX**, because it keeps the friend's download small: nothing
from us when Windows 11 24H2+ already supplies it through Windows ML, otherwise a few hundred MB
fetched once in the background. The CUDA EP (~1–1.5 GB of CUDA libraries) is used only if phase
0/4 measurements show TensorRT-RTX can't do the job (accuracy or speed).

Testing AMD before a friend does: Vast had **no AMD GPUs** on 2026-11-11 (4,015 offers, all
NVIDIA), and Vast boxes are Linux containers, so they could only ever test MIGraphX on Linux, not
the Windows ML path. Linux AMD clouds (Instinct MI-series) likewise test only MIGraphX's handling
of our graph. Windows cloud machines with Radeon cards exist but test the wrong thing: AWS
g4ad.xlarge (Radeon Pro V520, RDNA1, 4 vCPU; ~$0.56/h Windows vs ~$0.38/h Linux, us-east-1, per
price aggregators) and Azure NG8ads_V620_v1 (Radeon PRO V620, RDNA2; ~$1.00/h Windows in East US 2,
~$1.15–1.19/h elsewhere). Both are older than the RDNA3+ AMD lists for its related Windows GPU
provider, and cloud images are mostly Windows Server, while Windows ML's provider components ship
for Windows 11 24H2+; so they would mostly exercise DirectML, the Windows 10 fallback. A friend's
RX 7000/9000 PC is the real test (cost $0), in phase 4b.

Expect many friends' PCs to contribute mainly through the **CPU EP**: these nets are small, and
CPU inference is competitive at small batches (the M4 Max beat its own GPU via MPS for MTG Zero;
the 5090 needed CUDA graphs plus sparse uploads to pull ahead). Phase 0 measures this.

### 8.5 Phases 3 and 4 in parallel (2026-11-12)

The user's go-ahead covers both phases at once: self-play (3) and GPU for all vendors (4 + 4b,
with no NVIDIA-first step). They share one contract, PROTOCOL section 8 (`create_session`).

**Who owns what.**

| Area | GPU work | Self-play work |
|---|---|---|
| worker (`zero_worker`) | `session` (providers, self-test, device release), `probe-providers` | the self-play job, the record writer; calls `create_session` only |
| agent | `signals`/`policy`/`profile` GPU code (PDH busy signal, temperature, LUIDs, probe cache, `providers`/`gpu` in the job line) | the `records` result event, if anything changes there |
| runtime | the runtime packs (Windows ML catalog, DirectML pack for Windows 10) | none |
| coordinator | capability routing (`gpu`, `providers`) | self-play leases, quota and staleness with records |
| trainer (zero-core, mtg-zero) | none | `feat/multi-remote` merge, `replay_rows`, the `pool` remote, gate 3 |

**GPU busy and temperature.** The busy gate is the Windows PDH "GPU Engine" counters (per process,
what Task Manager shows), excluding Zero Farm's own worker processes, for every vendor. NVML is
kept only for NVIDIA details. Options for temperature, in order:
1. `D3DKMTQueryAdapterInfo(KMTQAITYPE_ADAPTERPERFDATA)`: vendor-neutral, the source of Task
   Manager's "GPU temperature" on recent drivers. To be confirmed on the AMD card.
2. AMD's ADLX library (installed with the AMD driver): AMD only, and needs C++ interface bindings.
3. No temperature gate (the driver's own thermal limits only).
Decision pending (the user asked to be shown the choice).

**Providers.** On Windows 11 24H2+: the Windows ML provider catalog (TensorRT-RTX, MIGraphX,
OpenVINO). Its availability in an unpackaged per-user app is to be confirmed first. On Windows 10
(the canary PC): a DirectML runtime pack. Each provider is accepted only through the self-test,
which also exposes nodes left to the CPU. The graph features flagged in 8.2 are the risks: int64
identity decoding, GatherElements/ScatterElements, boolean-mask attention.

**Self-play run.** Phase 3 needs a run to feed. Options: (a) a short throwaway run forked from an
EoE checkpoint, with a desktop-only twin as an A/B; (b) joining EoE run 2 (`eoe2e`, live,
managed by another session) at an iteration boundary; (c) resuming the paused Delver line with the
pool added. The user decides.

## 9. Phases

Each phase ends with a stop for the user's go-ahead.

**Phase 0: spikes on the desktop (no friends, no cloud).** Go/no-go for the native worker.
- 0a: zero-core's ONNX exporter (`zero-onnx/1`) for TokenNet, proven generic on two different
  nets (eoe1: layout v6 + deck reader + belief/deck aux; a Delver belief net: layout v5, 8
  layers); parity gate 1 on the CPU EP; ORT vs torch leaves/s on the CPU. GPU EPs (CUDA,
  TensorRT-RTX) are measured once the 5090 is free (a live run and a race hold it as of 11-11).
- 0b: `mtgz-worker selfplay` skeleton: seats from settings (serde in `zero_search`, shared with
  `zero_py`), ORT evaluator, drive loop, `check-config`; parity gate 2 against the Python remote
  worker.
- Measure a friend-class PC (the desktop restricted to 8 threads, or a friend's PC):
  games/hour by job kind. This sets how much the whole idea is worth.

**Phase 1: fuzz alpha with 2–3 friends.**
- Coordinator (enroll, invites, leases, results, campaigns with seed cursors, signed-artifact
  index, contributors page) on `farm.greggg230.com`.
- farm-agent: tray, settings, idle rules, yield/suspend, leases, verified downloads.
- **Silent self-update** (launcher + version swap + rollback, 2.2) — before the first friend
  installs, since an agent without it can never be updated.
- Fuzz loops moved into an engine library module; `mtgz-worker-fuzz.exe`; failure pull tool.
- Signing tool + release key; installer (per-user, no admin).
- Diagnostics from day one (5.4): machine profile, per-lease performance, availability, panics and
  crashes, admin dashboard; logs on request.

Phase 1 status (2026-11-11): coordinator LIVE at farm.greggg230.com (77 tests); agent + launcher +
ZeroFarmSetup + farm-release merged (113 tests, verified against the real coordinator with the
fakeworker: self-play credit and a fuzz campaign); release key 1B9342EFFB546500 trusted by agent and
coordinator; **agent 0.1.0 published to canary** (build 00639f0da4eddccaef6c202f). Left: the fuzz
worker (after 0b) and these follow-ups from the agent build. **First real install 11-11 on
the canary test PC** (a 4-core Windows 10 PC with an NVIDIA GPU, a disconnected console session): install
without admin, headless enroll, HTTPS to the live coordinator; silent self-update 0.1.0 -> 0.1.1
verified (at the next check-in, healthy in 1 s); forced rollback verified (a deliberately broken
0.1.2: 3 crashes in 3 s -> back to 0.1.1, marked bad); it exposed a lease hot loop when a required
update can't happen (fixed in 0.1.3 + regression test; the PC moved 0.1.1 -> 0.1.3). Follow-ups:
**Done 11-12 (merged 3db9e93 + 226ebbc, agent/launcher 0.2.0, PROTOCOL v1 rev 3; not yet deployed or
released):** per-machine rate limits (lease 1/5 s burst 6, beats 1/10 s burst 12, crash 1/10 s
burst 30, enroll 1/60 s burst 10 per IP; 429 + retry-after), public onboarding page `/` and
`/download`, user-initiated diag uploads + tray "Send diagnostics", `starved_ms` (counts toward
slack), launcher self-replacement (exit 76, probation + rollback), idle-hours metric, exe icon,
worker job object forbids UI access; the AppContainer sandbox is a design note in agent/README.md
(still before the stable channel). Result uploads and `done` are deliberately not rate-limited
(idempotent, bursty on reconnect, bounded by the lease limit). The items below are now history:
- the coordinator should rate-limit each machine's lease requests (429 + retry-after), so a buggy
  agent can't hammer it;
- a worker starved of CPU (idle priority on a busy PC) is not dead: deadlines wait up to 10 min; add
  `starved_ms` to Stats and count starvation toward the lease's slack like a pause;
- the launcher can't update itself: the agent replaces `ZeroFarm.exe` by rename-in-place (a
  running exe can be renamed) and exits with a "restart launcher" code;
- a public `GET /download` (latest stable `ZeroFarmSetup.exe`) for onboarding friends and for the
  426-at-enroll case (self-update needs a machine token, so a too-old installer can't update itself);
- `farm-release release-agent` must require `--min-agent` (it sent null; the coordinator refuses);
- idle-hours-per-day metric, ONNX Runtime provider detection (phase 4), an exe icon.

**Phase 2: evaluation jobs.** `eval_remote pool:<run>`; per-iteration evaluation games; `diag`
jobs and autotuning (5.4) arrive with the first network jobs.

**Phase 3: self-play for any single-game run.** Merge `feat/multi-remote`; record writer +
`replay_rows` + parity gate 3; the `pool` remote with automatic per-run build/gate/publish (2.1);
the Python-only pieces a worker needs moved to Rust (league seats, forked/archive starts).

**Phase 3b: co-training runs** (team_draft: deferred results, two nets): episodes as lease items
via the `queue_only` driver's tags. Until then `check-config` refuses those runs loudly.

**Phase 4: GPU.** Windows ML provider selection; TensorRT-RTX (or CUDA) EP on NVIDIA behind the
yield protocol (drop the session to free VRAM), NVML gating, per-GPU engine cache; the per-machine
self-test. **Phase 4b: AMD and Intel GPUs** through the same path (MIGraphX, OpenVINO, DirectML on
Windows 10), first on a friend's AMD PC; GPU-busy detection for them (no NVML: DXGI/PDH GPU-engine
counters).

**Later:** Authenticode signing (needed before many more friends), Mac (portable libm in the deck
builder first), porting the race tool, other zero-core games.

## 10. Answered (user, 2026-11-11)

1. Domain `farm.greggg230.com`: yes.
2. Unsigned alpha, signing before more friends: yes.
3. Giving invited friends the compiled engine: yes.
4. Runs: **any run, including not-yet-conceived ones** (2.1); self-update needed, transparent to
   the user (2.2).

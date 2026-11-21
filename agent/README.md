# zero-farm agent

The app invited friends install so their idle PCs do jobs for zero-core runs (MTG Zero first) - on
Windows, and from 0.3.0 headless on Macs with Apple silicon ([macOS](#macos-apple-silicon-protocol-11-agent-030)) -
plus its launcher, its installer and the desktop's release tool. The wire contract is
[`../PROTOCOL.md`](../PROTOCOL.md) (v1 rev 17); the reasons are in
[`../DESIGN.md`](../DESIGN.md) sections 2.2, 3, 4 and 5.

| Crate | Output | What it is |
|---|---|---|
| `common` | lib | protocol types, `manifest/1` + `definition/1` + minisign verification, trusted release and data keys, log redaction, hashing, HTTP client (ureq + rustls/ring, no OpenSSL), DPAPI, install layout and state files, named mutex/quit event, rotating log, setup payload format, minisign signing (feature `sign`), the round "Z" icon and Windows resources (`icon.rs`, `winres.rs`, also used by the build scripts) |
| `launcher` | `ZeroFarm.exe`, `ZeroFarmSetup.exe` | PROTOCOL 6 launcher (no console, depends only on `common` without HTTP; hands over to a newer launcher); the single-file installer |
| `agent` | `farm-agent.exe` (+ lib) | tray app: enrollment, idle policy, leases, verified downloads, worker control, uploads, self-update, install/uninstall |
| `release` | `farm-release.exe` | desktop tool: keygen (release and data keys), key backup (export-key / import-key), sign, verify, publish, release-agent, installer build, GitHub releases (through `gh`), define (signed definitions) |
| `fakeworker` | `fakeworker.exe`, `fakeagent.exe`, `fakelauncher.exe`, `fakegh.exe` | test binaries: an aw/1 worker with scripted behaviours (self-play, fuzz and eval: per-network self-tests, eval rows), scripted stand-ins for the agent, for a new launcher and for the GitHub CLI |
| `mockcoord` | lib | in-process mock coordinator (PROTOCOL rev 2) and a test kit (dev-dependency only) |

## Build

```
cd agent
cargo build --workspace                                  # debug, everything
cargo build --release -p farm-agent -p zerofarm-launcher # what ships
```

Builds go to the default `target/` (set `CARGO_TARGET_DIR` to put them elsewhere). Rust 1.95
MSVC; ring needs the MSVC C compiler (Visual Studio Build Tools).

**Icon and version.** Every exe carries the round "Z" icon (the tray's, in green; 16-96 px DIBs
and a 256 px PNG) and a VERSIONINFO (file version = the crate version, shown in Explorer's
Properties). The build scripts write the `.res` file themselves (`common/src/winres.rs`, PNG
compressed with `miniz_oxide`) and pass it to the MSVC linker, so no `rc.exe` is needed.
Agents compare launcher versions by this VERSIONINFO: **bump `launcher/Cargo.toml`'s version
whenever the launcher changes** (an installed launcher is only ever replaced by a newer one).

**Trusted keys are compiled in**, in two separate lists; each check accepts only its own list, and
a key on both fails the build:

- release keys (manifests: worker builds, runtime packs, agent updates): `common/trusted_keys.txt`
  (one minisign public key per line) plus the build-time variable `ZEROFARM_TRUSTED_KEYS`. It holds
  the release key (key id 1B9342EFFB546500);
- data keys (definitions, PROTOCOL 7): `common/trusted_data_keys.txt` plus the build-time variable
  `ZEROFARM_TRUSTED_DATA_KEYS`. It holds the data key (key id 1FDB907D9FF199C8).

A build without a release key refuses to run any worker or update (the launcher exits, the agent
logs it); without a data key it can verify no definition. `farm-release release-agent` refuses to
sign an agent (or package a launcher) whose trusted keys are not exactly the two files' (a stray
`ZEROFARM_TRUSTED_KEYS` at build time would otherwise ship an extra root of trust), or that does not
trust the signing key. A build from this source trusts exactly the official keys, so it runs only
workers, jobs and updates the owner signed.

## Test

```
cargo test --workspace --all-features -- --test-threads=2
```

Run it on the whole workspace in debug: the integration tests start the real `ZeroFarm.exe`,
`farm-agent.exe`, `fakeworker.exe` and `fakeagent.exe` from the target dir, and those are built
because each package has integration tests. Debug builds also honour these test-only variables
(compiled out of release builds; `release-agent` refuses a binary that contains their names):
`ZEROFARM_TEST_TRUSTED_KEYS` and `ZEROFARM_TEST_TRUSTED_DATA_KEYS` (extra trusted release / data
keys, so tests sign with temporary keys), `ZEROFARM_TEST_HEADLESS` (no tray),
`ZEROFARM_TEST_HEALTHY_TIMEOUT_S`, `ZEROFARM_TEST_TAKEOVER_CONFIRM_S`, `ZEROFARM_TEST_HANDOVER_S`,
`ZEROFARM_TEST_COORDINATOR`, `ZEROFARM_TEST_WORKER_PRIORITY=normal` (workers at normal priority
instead of idle; the in-process test harness sets it).
The in-process tests script a worker that Windows refuses to start through
`AgentConfig::test_spawn_error` (honoured by debug builds only; `farm-agent.exe` sets None).

No test installs the real agent, touches the real `ZeroFarm` registry entries, generates the real
key, uses the CPU (the fake worker sleeps) or reaches the network beyond its own localhost mock.
Install tests use a temp root, a temp shortcut folder and the registry name
`ZeroFarmTest-<pid>-<n>`, removed by a guard even when an assertion fails.

On macOS no test touches the real LaunchAgent or `~/Library/Application Support/ZeroFarm`: the install test uses a
temp root, a temp LaunchAgents folder, the label `com.zerofarm.test-<pid>` and `--no-launchctl`, and quits whatever
it started even when an assertion fails. Tests that patch a launcher's version re-sign the copy ad hoc (`codesign`:
Apple silicon runs only signed code).

Windows: 261 tests (+2 ignored), about 3 minutes on an otherwise idle PC. macOS: 221 (+1 ignored); the Windows-only
suites (`install`, `alternates`, `unfinished`, `release`, the session-end codes) are left out there, and the macOS
ones (`install_macos`, the footprint / signals / token / CLI unit tests) come in:

| Suite | Tests | Covers |
|---|---|---|
| `common` unit + `manifest_verify` | 33 + 9 | manifest rules and verification: tampered file, size change, missing/extra file, edited manifest, bad signature, unknown key, wrong trusted comment, legacy signatures, key rotation; minisign format; paths; DPAPI; backoff; canonical digest; rotating log; payload format; the round Z; VERSIONINFO write/read/patch and `.res` layout; launcher leftovers; `definition/1` (a good one, every refusal: unknown fields, ids, builds, sizes, templates with foreign keys or paths, seats); path-like strings; per-lease fields (ranges, whole pairs, kind); redaction (profile paths in every spelling, short paths, names as words only, JSON); only a coordinator-marked `revoked` revokes, a WAF's 401/403 is retried |
| `agent` unit | 34 | idle policy (pure function over simulated signals), settings, cache LRU, results store, ranges, update cleanup, invite codes, panic sites, availability (dates, sample-and-hold with gaps, 7-day means); start failures (policy error codes, the 15 min → 24 h and 1 min → 1 h waits and their file, one `blocked` report per build per day, other reports once a day per reason); the tray window procedure re-entered while its state is busy (skipped, no panic) and a panic inside it (caught, reported as `agent`, the tray keeps working); the panic guard; the legacy allowlist (only the kind's keys, paths and kind mismatches refused, eval checkpoints = the seats' networks); definition versions never go back |
| `agent/tests/lifecycle` | 38 | whole leases with the real fakeworker and the mock: download + verify, run, yield/suspend/resume, give-back after slack, upload retries after injected 503s, restart with pending results, orphaned leases, cancel, crash, hang, fatal error, slow yield, no hello, fuzz failures + summary, checkpoint switch, expiry, coordinator give-back, tampered / unknown-key / bad-signature builds, disk cap, profile resend, revoked token (only the coordinator's own `revoked`), an unknown token kept (tray "can't sign in", one try then a wait), a WAF's 403/401 HTML on lease requests, uploads and beats retried as network errors, quit event, launcher events, rev 2 (stale chunks, `done` missing, `upload_diag`, repeated checkpoints, duplicate ids, `x-farm-agent`); rev 3: starvation past slack gives back, brief starvation counted and kept, pause + starvation add up, availability recorded, the worker job's UI restrictions; rev 17: a build of another platform refused before anything but its manifest is downloaded (`refused: "platform"`, one `definition` report, the start-failure wait) |
| `agent/tests/definitions` | 5 | signed definitions (PROTOCOL 7): the job line is the signed template + per-lease fields + the agent's own fields only (a coordinator's `prefix`, `models`, `out_dir`, `runtime_dir`, `threads`, settings dropped or overwritten), the definition remembered; an eval definition's seats and networks (the coordinator's seats ignored); a mid-lease network the definition doesn't name never fetched; refusals (`done` with `refused`, a `definition` report once per reason, a wait): signed with the release key, tampered bytes, an older version, another run's definition, expired, a build or runtime it doesn't name, a checkpoint hash, a job kind it has no template for, per-lease fields out of range; a legacy lease's allowlist (the review's UNC `models` and `prefix` never reach the worker; a path or a kind mismatch refuses it); a declared checkpoint size over the disk cap never downloaded |
| `agent/tests/rolling` | 13 | rolling self-play leases (rev 15, PROTOCOL 10) with the fakeworker's rolling mode: `features: ["rolling"]` in lease requests; the job line's `rolling`, `flush_s`, `conc` / `groups` (the run's number, "auto" on the CPU) and released `games`; an extension of 2 x `conc` per `low` until the block is used up (`finish`), every id credited once, Stats `inflight` / `released`, beats with `gen`; `wait_s` honoured; the coordinator's `finish` (unfinished = released ids not played); a new generation with a verified definition switched to in place (`checkpoint` with `sha256`, self-tested, fetched once, the version remembered, chunks with the new `x-gen`); a changed template and a definition that fails a check finishing the lease (the latter reported once); a worker without the feature running it as a classic lease (no `extend`); a ready agent update finishing it, then exit 75; a 410 on an extension stopping it like a lost lease; classic leases unchanged; memory (0.2.14, the fakeworker committing real memory against the job's limit): a worker that runs into the limit with a fatal "bad allocation" or a silent abort started again with fewer games (ids after the released ones, its own `out_dir`, the lost ids unfinished, every id credited once, one crash report), the measured cost remembered and capping the run's next lease; a worker past 92 % finishing its games and the lease going on with fewer from the first id it never started, nothing lost, no crash report; 0.3.14: another run's measurement of the build planning a new run; 0.3.15 (`lease::Watch`): a worker that plays nothing more while still sending `status` started again (reported once, the lease playing on), and no false alarm for a slow worker (games longer than `stall_after`) or a pause longer than it |
| `agent/tests/gpu` | 10 (Windows) | agent 0.3.4, rolling GPU leases (PROTOCOL 10.6, 13, 13.5) with a CUDA fakeworker: a crew at the bench target (3 x 384 on a 4070 Ti, search threads shared, no `infer_*`, each worker a third of the memory limit, no hint), the games in flight raised within the lease once the memory measured fits (128-ish -> 1,024), every member yielding its video memory and resuming, a member's crash, the video memory plan (a small GPU: one worker; another program filling the GPU: one gives way, none joins until it fits), a worker out of video memory restarted with smaller batches (learned per row, the memory per game untouched), the tuner choosing the worker count (reported); 0.3.5: an uncalibrated start (9.5 GB cap, ~2 MB a game) reaching a crew of 2 x 512 within seconds without a handoff loop, a trial that can't run when its worker starts counted as discarded; 0.3.6: a crew forming under a 9.3 GB cap with workers of 1.5 GB and 2 MB a game (the base measured, the start picked again, the draining start's last games ended, ids exact); 0.3.15: a worker whose resume fails (zero-core's non-fatal `error`, then frozen: `resume_error`) started again with smaller batches, and a crew whose resumes fail (main worker restarted, member replaced) |
| `agent/tests/variants` | 6 | agent 0.3.5, model variants per generation (PROTOCOL 14): a self-play lease whose checkpoint names a `variant` gets that file (`model-<name>.onnx`, cached next to the gen's reference set, only it fetched) as the job line's `model`, the next lease request lists `<run>/<gen>/<name>` in `cached.ckpts`; refusals (`refused: "definition"`, nothing downloaded): a variant the definition doesn't sign, the default's hash under a variant's name or the reverse, a name that isn't one, a variant on a legacy lease; a variant file of another size never used; an eval lease's `models` with one gen's variant and another's default; a rolling lease switching in place to the next gen's variant (the `checkpoint` op's `model` and `sha256`) and then to a default; a rolling switch to a variant the new definition doesn't sign finishing the lease, reported once |
| `agent/tests/eval` | 3 | eval leases (PROTOCOL 5.2) with the fakeworker's eval mode: two networks and their references downloaded, verified and cached; the job line's `models` (absolute paths, hashes, `model` null); every game uploaded with `x-ids` and no `x-gen`, files that split a pair, seat order per game; per-gen `selftest` in Stats; cached checkpoints in the next lease request; a tampered checkpoint refused before any worker starts, then a wait (no lease storm); a network failing its self-test reported once, with every gen's result |
| `agent/tests/blocked` | 5 | a worker Windows refuses to start (scripted os error 4551): one `blocked` report with `os_error` and the build, the lease given back, no lease storm (one lease request while the 15 min wait runs), the tray status and balloon, the wait kept across a restart; shortened waits growing ×4 with the same build reported once; an older coordinator gets it as a `crash`; any other start failure: a `crash` report and a 1 min wait; a worker that starts ends the wait |
| `agent/tests/update` | 6 | self-update in process: download + verify + `state\pending` + exit 75; `{"update"}` answer; tampered update refused; rolled-back version not retried; a required update that can't happen doesn't spin; older/equal ignored |
| `agent/tests/selfupdate_e2e` | 2 | real launcher + real agent processes: update through the launcher; a version that never becomes healthy is rolled back, reported by the old agent, never re-downloaded, cleaned up |
| `agent/tests/launcher_update` | 7 | the launcher updating itself. In process: swap + exit 76 + `launcher-swap`; with a pre-0.2 launcher, swap only (next logon); selftest failure never installs and isn't retried; `plan` (current + healthy only, verified package, never a downgrade or same version, no VERSIONINFO = 0.0.0, bad for 24 h). Real processes: the real launcher hands over to a newer real launcher (old exits 0, new runs the agent, `.old` deleted); fake new launchers that exit at once or never confirm are killed and rolled back, reported, not retried, `.bad` deleted |
| `agent/tests/install_macos` | 2 (macOS) | `ZeroFarm setup --invite` from an unpacked package with the real launcher and agent (token file 0600 in a 0700 `state/`, 0755 executables, the LaunchAgent plist, enrollment, the launcher starting the agent), the CLI against the running agent (`status` with the CPU threads, `set` and `pause` / `resume` through request files, a bad value refused, `quit`), `set` with no agent running, `uninstall`; a tampered package and a Windows package refused |
| `agent/tests/install` | 5 | install/uninstall with a test registry name (Run value, Uninstall entry, shortcut, files, self-deleting stage 2), install starting the launcher and uninstall stopping it, tampered package refused, upgrade keeps previous / no downgrade, path normalisation |
| `agent/tests/enroll` | 2 | invite → DPAPI token, single use, 426 keeps the invite and shows the download link |
| `agent/tests/diag` | 2 | "Send diagnostics": logs body (header, tail caps), `x-farm-diag: user`, the once-an-hour 429 message, unrequested uploads without the header still refused; no user name or profile path in a log line as written, in an uploaded logs body (old logs too, JSON-escaped and `/` spellings), a crash report or Stats |
| `agent/tests/real_coordinator` | 1 (ignored) | the agent against `wrangler dev` of `../coordinator`: run with checkpoint + fuzz campaign, every id credited once, the failure recorded (see the file header to run it) |
| `launcher/tests/swap_rollback` | 10 | swap on exit 75, tampered pending refused, rollback when healthy never appears, rollback after 3 crashes, crash loop without a previous version, single instance, untrusted current refused, quit on exit 0, leftover launcher files deleted at start, `selftest` |
| `release/tests/release` | 20 | keygen (DPAPI, never overwrites; `--data` says where the key goes), exact trusted-key sets (`keys` of farm-agent and ZeroFarm; a stray, missing or swapped key refused), define (validates, signs the bytes as they are, minisign-verifiable, trusted comment, uploads then registers, `.minisig` next to the file, `--url` needs `--publish`; refuses the release key, an untrusted data key and bad definitions; a data key never signs a build), sign + minisign-compatible verify + tamper detection, incomplete manifests refused, publish order and admin auth, foreign builds refused, release-agent publish + latest, refuses agents that don't trust the key, refuses debug binaries; GitHub releases through `fakegh`: release-agent `--github` (prerelease for canary, the seven assets, `SHA256SUMS` hashes and its minisign signature, then the coordinator release with `github_url`, nothing in R2 `setup/`), a failed `gh` leaves the channel alone, `github-release` of an existing package (stable, `--url`, re-run = view + upload `--clobber` + edit), an installer of another package refused; key backup: export then import gives the same key (the `minisign` crate decrypts the export; signatures after import verify with the original key), wrong / mismatched / empty passphrases refused, never overwrites, no passphrase on the command line |
| `fakeworker` | 1 + 3 | the fake worker speaks aw/1 |

Timing-based tests tolerate a loaded machine. The in-process suites (`lifecycle`, `blocked`,
`eval`, `definitions`) run the fake worker at normal priority (`ZEROFARM_TEST_WORKER_PRIORITY`,
set once by the harness), so other builds or training at normal priority can't starve it into
timeouts as they could the idle-priority worker; and their waits on the mock coordinator's state
wake on each request it answers (`MockCoord::wait`) instead of polling. Starvation itself is still
tested, with simulated signals.

## macOS (Apple silicon, PROTOCOL 11, agent 0.3.0)

The same workspace builds and tests on a Mac (Rust 1.95; the Xcode command-line tools provide the linker, which
ad-hoc signs the arm64 binaries). There the outputs are `farm-agent` and `ZeroFarm` (no `.exe`); `ZeroFarmSetup` only
says it is the Windows installer, and `farm-release` stays the Windows desktop's tool (it keeps the release key with
DPAPI; its tests run on Windows only). The macOS code sits next to the Windows code behind `cfg`: `agent/src/macos.rs`
(sysctl, IOKit, Low Power Mode, memory, per-process accounting), `signals_macos.rs`, `worker_unix.rs`,
`install_macos.rs`, `headless.rs`, `common/src/names_unix.rs`, `common/src/binver.rs`.

```
cd agent
cargo build --release -p farm-agent -p zerofarm-launcher            # what ships (on the Mac)
cargo test --workspace --all-features -- --test-threads=2           # on the Mac too
```

| | Windows | macOS |
|---|---|---|
| root | `%LOCALAPPDATA%\ZeroFarm` | `~/Library/Application Support/ZeroFarm` |
| autostart | HKCU `Run` value | LaunchAgent `~/Library/LaunchAgents/com.zerofarm.agent.plist` (`RunAtLoad`, no `KeepAlive`, `ProcessType` `Interactive`), `launchctl bootstrap gui/<uid>` / `bootout` |
| token | DPAPI in `state\machine.json` | in the clear in `state/machine.json`, mode 0600, `state/` 0700 |
| single instance, quit | named mutexes, a named quit event | `flock` on `state/<role>.lock` (holding the pid), SIGTERM; the launcher passes a quit on to its agent; an agent whose launcher is gone quits |
| worker | job object (kill on close, memory limit, UI restrictions), idle priority + EcoQoS, NtSuspendProcess | own process group, nice 19 (never background QoS), SIGSTOP/SIGCONT to the group, the memory cap enforced by the agent on the group's physical footprint (checked at least every 100 ms, killed past it), leftover groups killed at the next start |
| when it works | idle time, fullscreen, power, battery saver, memory, GPU | IOKit `HIDIdleTime`, the power source, Low Power Mode, memory pressure; no fullscreen detection, no GPU work |
| keep awake | - | an IOKit `PreventUserIdleSystemSleep` assertion while a lease runs on AC power (`keep_awake`, default on) |
| launcher version | VERSIONINFO | a 32-byte marker in the binary (`binver.rs`) |
| UI | tray, invite dialog | headless CLI: `ZeroFarm status \| pause [minutes] \| resume \| set <key> <value> \| quit \| uninstall \| setup` |
| rolling "auto" (CPU) | 32 in flight in 4 groups | 144 per physical core in groups of 192 (an M4 Max: 2,304 in 12), planned at 1 GB + 18 MB per game of physical footprint for a worker build not measured here (0.3.3: the plan calibrates itself per run and build, `rolling::Learned`; a 64 GB Mac's default cap holds 1,472 - memory cuts the games, the 12 groups stay, batches down to 96 rows) |
| layout tuning (rev 19) | start: one worker, cores search + 1 x cores inference (each its cap); batches >= 8 rows | start: cores search + sessions of 3 (5 x 3 on 16 cores); inference <= cores; batches 96-384 rows; sessions always searched; >= 12 cores and >= 32 GB: two workers of 12 + 4 x 3, each with the games in flight its half of the memory plan holds and half the cores' groups (a 64 GB Mac: 716 in 6 groups) |

**Install** (what `curl -fsSL https://farm.greggg230.com/install-macos.sh | sh -s -- --invite CODE` ends in): the
tarball `ZeroFarm-macos-arm64.tar.gz` holds `ZeroFarm`, `farm-agent`, `manifest.json` and its signature. `./ZeroFarm
setup [--invite CODE] [--name NAME]` verifies that package against the launcher's compiled-in release keys and runs
its `farm-agent setup`, which installs it (the package must name `macos-arm64`), writes the LaunchAgent, enrolls (the
code and name asked on `/dev/tty` when not given; the name defaults to the host name) and loads the LaunchAgent, which
starts the launcher. Options for tests and odd setups: `--root`, `--launch-agents-dir`, `--label`, `--no-launchctl`
(start the launcher directly), `--no-start`, `--coordinator`. Uninstall: `ZeroFarm uninstall` (quits, unloads and
removes the LaunchAgent, deletes the root). Without an enrollment the agent waits for one instead of exiting.

**CLI** (`<root>/ZeroFarm`; everything but `quit` is forwarded to the current `farm-agent`): `status` (what the
running agent wrote to `state/status.json`: its state, games today, CPU threads with their automatic number, the
settings), `pause [minutes]` (no minutes: until `resume`), `resume`, `set <key> <value>` with the keys `cpu_threads`
(N or `auto`: one per physical core, 12 performance + 4 efficiency on an M4 Max), `work_when` (`idle` | `active`),
`idle_minutes`, `on_battery` (also work on battery and in Low Power Mode; default false), `keep_awake` (default true),
`hours` (`HH:MM-HH:MM` | `off`), `max_ram_mb` (N or `auto`: half the RAM), `disk_cap_mb`, `autotune` (`on` | `off`,
default on: layout tuning, PROTOCOL 13; `status` shows the job's layout, else the last tuned one), `workers` (`auto` |
1-3: worker processes per self-play job, PROTOCOL 13.5; auto = the tuner's choice). A change goes to the running
agent as a request file in `state/requests/` plus SIGUSR1 (applied as the tray applies its menu, answered within a
second); with no agent running the CLI changes `machine.json` itself. An owner's own Mac that should work like the
SSH self-play does: `set work_when active` and `set on_battery true`.

**Release** (desktop): `tools/mac-agent-build.sh [DEST]` pushes HEAD to a bare repository on the Mac (git remote `mac`,
or `ZEROFARM_MAC_REMOTE`), builds `farm-agent` and `ZeroFarm` there in release (`ZEROFARM_MAC` = its ssh target; the
binaries checked: arm64, ad-hoc signature, versions, trusted keys) and copies them into `agent/dist/macos-arm64-prebuilt`.
Then
```
farm-release release-agent --platform macos-arm64 --prebuilt agent/dist/macos-arm64-prebuilt --url https://farm.greggg230.com --token %ZEROFARM_ADMIN_TOKEN% --channel canary --min-agent 0.3.0 --github greggg230/zero-farm
```
reads what it can't ask a Mac binary to print from its bytes (arm64 Mach-O, no test-only names, the version markers,
exactly the trusted keys of `common/trusted_keys.txt` + `trusted_data_keys.txt`), signs the package for `macos-arm64`
(`dist/agent-macos-<v>/`), makes the tarball (`dist/agent-macos-<v>-github/`), uploads and registers the build, makes
the GitHub Release `agent-macos-v<version>` (the tarball, `SHA256SUMS` with its line, `SHA256SUMS.minisig`; a prerelease
for canary; never marked Latest, which stays the Windows release) and then `POST /v1/admin/agent/release` with
`"platform": "macos-arm64"` and the tarball's GitHub URL. Worker builds and runtime packs for Macs:
`farm-release sign ... --platform macos-arm64 --min-agent 0.3.0`.

## Layout tuning (PROTOCOL 13, rev 19, agent 0.3.1)

`tune.rs` (shared by both platforms; the only per-platform part is `defaults`, the starting point and the caps) and
the tuning half of `lease.rs` measure the CPU layout of rolling self-play jobs on the machine: `threads`,
`infer_workers`, `infer_threads`, `groups`, `conc` in the job line.

- **Space.** Start: the coordinator's `tuning_hint` for a new key, else the layout of before (search threads = physical
  cores; Windows one session of as many threads, macOS sessions of 3; 10.6's games in flight and groups). Caps, never
  searched upward: search threads <= cores and inference threads <= cores (never more than before), or the owner's
  `cpu_threads` for each; macOS batches 96-384 rows (0.3.3; memory cuts games, not groups), Windows >= 8; games in
  flight from half of "auto" to "auto" (a run's number fixed). Remembered layouts are compared as they run in the
  lease's bounds (`Space::fit`; the entry keeps them as `space`, 0.3.3). Coordinates: batch size, search threads, threads per session, sessions (a worker listing
  `infer_workers`, or any macOS build), games in flight; momentum first; a move must gain 3 %; at most 9 trials; the
  winner re-measured and kept only at 3 % over the start's mean. Unit tests with a fake rate source: `tune::tests`.
- **Trials** run inside real rolling leases as **layout handoffs**: the main worker gets `finish`, two equal
  `games_started` reads give the first id it never started, the next worker takes the released ids from there and
  starts beside it when its plan fits beside what the others hold or will hold (`reserved_mb`: a fresh worker at its
  plan, a drainer at its commit; else after it, `restart_room`); the old worker finishes its games as a drainer
  (stopped while the main worker is measured or work is paused). One drain at a time per lease, at most
  `tune::MAX_HANDOFFS` (6) handoffs per hour, never to the layout running unless to measure it (0.3.3). No game is
  lost and no id played twice (`tests/tuning.rs` checks both; `tests/crew.rs` adds up what the fakeworkers hold at
  once). A trial starts only while `policy::tuning_allowed` (idle, AC power, no battery saver / Low Power Mode, no
  fullscreen app, nothing blocking - whatever `work_when`, `on_battery` and `respect_fullscreen` allow) and the
  machine is quiet (`tune::LoadTrack`: other programs <= 10 % of the CPUs on average over the last trial length, from
  the system's CPU times minus the workers'; the worker not starved); sustained load (25 % over 30 s), anything else
  the policy blocks, or a trial window over 10 % on average / 5 points over the load before it discards it (bursts
  pass; each measurement records its load), the incumbent comes back by a handoff, and the candidate is retried at a
  later idle period (3 discards: skipped).
- **Measurement**: leaves/s of the fresh worker between the same leaves-per-game-in-flight marks for every trial
  (set by the search's first measurement: refill, 20 s, 75 s), pinging it every 2 s. Why not wall time: a fresh rolling
  worker's rate falls as its games grow, and at a fixed time more games in flight look better than they are (the
  module doc has the numbers).
- **Cache** `state/tuning.json` per (build, runtime, network fingerprint, hardware, OS, tuner version), 16 keys (0.3.14:
  the fingerprint leaves the settings' `setting_classes` out, `tune::net_fingerprint`; a key saved with the one of
  before is found by `net_fingerprint_legacy`, `tune::Cache::find`, and saved under the new key); weekly
  check of runner-up vs incumbent. Lease requests report it (`tuning`, feature `tuning`); the tray (Windows) and
  `ZeroFarm status` (macOS) show the job's layout. `autotune` (default on; macOS `ZeroFarm set autotune off`) turns all
  of it off: the fixed rules of before.
- **Expect** a search to take a while on a real machine: each trial waits for its predecessor's drainers (about a game
  length of the old layout's games, overlapped with real work), so 6-10 trials take a few hours of idle time on a
  Windows CPU PC and longer on a Mac with 2,304 games in flight; discarded trials (the owner came back) add to it. The
  result is reused for every lease of the key.
- **GPU jobs** (agent 0.3.4, PROTOCOL 13.1): a rolling GPU job always has a layout (fixed with `autotune` off) -
  workers 1-3 (at most one per physical core, within the memory and video memory plans), search threads shared (the
  owner's `cpu_threads`, else 1.5 per physical core: `rolling::GPU_THREADS_PER_CORE`), one or two groups a worker, the
  job's games in flight from half the bench target (`rolling::gpu_target`: two full batches of `gpu_batch`) to 1.5 x it;
  inference threads are the worker's own (no `infer_*` in the job line). `tune::Space::new_gpu` / `gpu_start` (NVIDIA:
  the most workers that hold the target - 3 x 384 / 2 x 512 for 1,024 -, else 1), no coordinator hint, the key's
  hardware with the GPU, driver and provider (`tune::gpu_fingerprint`). Trials, handoffs and the cache as for the CPU.
  0.3.5: an early memory reading (`rolling::early_reading`, `Learned::early`: half the games in flight finished,
  `Timings::early_calibration` after the start, planned x 2) and the start picked again by `calibrate` while the search
  measured nothing else; a GPU job's measured trials only when their workers fit beside the running ones (`room_beside`), else
  postponed (a discard, against the cap); a trial's measurability decided at its worker's start (`settle_trial`).
  0.3.6: a GPU worker's base measured (`rolling::base_reading`, `Learned::set_base`, key `<run>/<build>/gpu_base`,
  lower only), the start picked again whenever the new plan holds more workers, and a drainer the next worker waits
  for ends its last games after `Timings::drain_tail` (2 min) or with an eighth of its games left (`tail_tick`).
- **fakeworker** `fake.rate` (`{"leaves_s", "best": {threads, infer_workers, infer_threads, batch, conc}, "penalty",
  "decay_p", "crew"}`) plays leaves/s by layout and keeps `conc` games in flight, for the integration tests; workers of
  one lease share the machine (`crew`: the crew's total by its size, beat files in the lease's `.crew/`, rewritten with
  changing content - 0.3.4: on Windows an empty rewrite kept the old modification time and every worker counted a crew of
  one). A GPU job's fake: `provider` cuda, `vram_base_mb` / `vram_per_row_mb` (its `status` `vram_mb`), `vram_oom_rows`
  (fails at the start like ONNX Runtime's CUDA allocator); it logs `fakeworker: pid P yielded` / `resumed`.
  `mem_base_mb` / `mem_per_game_mb` / `mem_ramp_ms` commit a worker's memory as a real one grows, each change logged
  (`fakeworker: pid P holds N MB`, `... exits`).

## Several workers per lease (PROTOCOL 13.5, rev 19, agent 0.3.1)

One native worker process tops out well below what a big CPU can do (the M4 Max: one process at best ~12.6k leaves/s,
every extra thread inside it slower; two processes of 12 + 4 x 3 threads with 1,152 games in flight each 16.4k, the fast
export ~18.0k = the SSH self-play's parity; three 16.1k, four 15.2k). So a rolling CPU self-play lease can be served by
a **crew** of worker processes, the worker count being the tuner's first coordinate (`tune.rs`) and `lease.rs`'s `Mate`
the members beside the main worker.

- **Ids.** aw/1 gives a worker one contiguous range that only grows at its end (`extend`), and the coordinator releases
  ids only at the lease's tail, so one worker - the main one, the **tail** - gets the extensions; every other member
  plays a **closed** range of released ids. A new member takes the tail's place (the main worker becomes a member and
  keeps every id released so far from its first - at least 2 x its games in flight unstarted, else more ids are asked
  for first - and the new worker starts at the end of the released ids with none: its `low` asks) or plays a **spare**
  range that holds 2 x its games in flight (ids a worker told `finish` early never started: its frozen `games_started`
  read twice, as in a handoff, or its `finish` end's `unfinished`). A member that started all of its ids is told `finish`
  and a new one joins (memory permitting: every worker's commit plus the new one's plan within 85 % of the job's
  limit). Extensions ask for 2 x the crew's games in flight (the same ids in hand as one worker with all of them). No
  coordinator change, no extra lease: the coordinator sees one lease, its released ids and its extensions.
- **Results** of every worker are queued under the lease (one results directory per worker, `out-w<n>`); `done`'s
  unfinished ids are worked out from the credited ones.
- **Generations, pauses, ending.** `checkpoint` goes to every member with its job (a finishing one keeps its network);
  a pause yields the main worker and suspends every other (a CPU job: no VRAM to free); a GPU job's (0.3.4) members are
  yielded first (`Mate::yield_at` / `yielded`; suspended at `yielded`, killed without it in 2 s, `resume` after), and every
  worker's pid is the lease's own for the GPU busy rule (`Engine::decide(&pids)`); `finish`, `quit` and kills reach all;
  the lease closes once every worker ended.
- **Memory.** Each worker gets its part of the job's limit (job object on Windows, the footprint cap on macOS:
  limit / workers); a member at 92 % of its part finishes its games (its unstarted ids become spare) and the next
  members run fewer games (`restart_games`, counted as a restart); the crew's sum at 92 % of the job's limit makes the
  biggest give way the same way; the most any worker committed per game is remembered (`Learned`). The tuner's worker
  count stays within what the memory plan holds with each worker's base. A GPU job's video memory (0.3.4, `Vram` in
  `lease.rs`, PROTOCOL 10.6): budget 85 % of the adapter less other programs' use (`signals::GpuMemory` from the PDH
  `GPU Adapter Memory` / `GPU Process Memory` counters, `Win32Signals::gpu_memory`; else 1 GB), 1 GB + 2 MB per batch row a
  worker until measured (`Learned::vram_plan`, key `<run>/<build>/vram`), readings once a second (the counters or the
  worker's `vram_mb`), `room_for` / `restart_room` check it, `vram_check` gives way at 95 % of the adapter, a worker out
  of video memory (`rolling::gpu_oom`, or an allocation failure far from its memory limit) is restarted with smaller
  batches (`vram_oom_restart`). `farm-agent dry-run --pids A,B` logs the counters.
- **Failures.** A member that exits without `end` or with a fatal error loses its games in flight (they go back with the
  lease) and is reported once per lease; a new one joins while restarts are left (4 per lease, shared with memory
  restarts), then the crew runs smaller. The main worker failing while members play is replaced the same way (a new
  tail after every released id). A self-test failure is fatal as before.
- **Tuner.** `workers` 1..3 within physical cores / 4, RAM / 16 GB and the memory plan, or the owner's `workers`; thread
  caps per worker: one worker the caps of before, N > 1 workers 1.5 x the cores per kind between them (2 x 12 on 16
  cores); a worker move keeps the job's games in flight and rows per batch and rescales each kind of thread to its
  budget. Trials measure the crew's summed counters through rotations (a member that finished its range counts until
  it ends; a member failing discards the trial) and start only with memory for the whole crew. A Mac with >= 12 cores and >= 32 GB starts at 2 x (12 + 4 x 3, 1,152 in 6 groups); everything else at 1.
  `autotune off` with `workers N` (N > 1) runs that crew fixed; `autotune off` with `workers auto` is one worker, the
  rules of before. The tray and `ZeroFarm status` show "2 workers x (...)" and how many are running.
- **Cost.** Rotations: a member's closed range lasts between one and a few game lengths of its own (about 1-3 h on the
  M4 Max), then it finishes its games beside its successor (a third process for up to a game length, and the successor
  waits for room). A worker `extend` that takes a new range would remove that (a worker change, aw/1 rev); not needed
  for the measured gain.

## Install flow (`farm-agent install`, run by ZeroFarmSetup.exe)

1. Verify the package next to the executable (manifest, signature, files) against the compiled-in
   keys; it must contain `farm-agent.exe` and `ZeroFarm.exe`.
2. Ask a running launcher/agent for this root to quit (named quit event) and wait.
3. Copy the package to `<root>\versions\<v>\` (staged, re-verified), copy `ZeroFarm.exe` to the
   root, set `state\current` (keeping an older version as `previous`; never downgrading), mark it
   healthy (user-installed: no probation).
4. `HKCU\Software\Microsoft\Windows\CurrentVersion\Run\ZeroFarm` = `"<root>\ZeroFarm.exe"`;
   `HKCU\...\Uninstall\ZeroFarm` (DisplayName "Zero Farm", version, `UninstallString` =
   `"<root>\ZeroFarm.exe" uninstall`); Start-menu shortcut `Zero Farm.lnk`. HKCU only: no admin, no
   UAC (every exe embeds an `asInvoker` manifest, which also stops Windows treating
   "ZeroFarmSetup.exe" as an installer that needs elevation).
5. Start the launcher (detached).

Root: `%LOCALAPPDATA%\ZeroFarm` unless `--root`. Options: `--reg-name`, `--shortcut-dir`,
`--no-shortcut`, `--no-start`, `--quiet`, `--source`.

Uninstall (`ZeroFarm.exe uninstall` → `versions\<current>\farm-agent.exe uninstall`): copies itself
to `%TEMP%`, and that copy stops the launcher and agent, removes the Run value (only if it points
into this root), the Uninstall entry, the shortcut and the whole root, then deletes itself.

## Release flow (desktop)

One time:
```
farm-release keygen --out %USERPROFILE%\.zero-farm\release.key
```
writes the secret key DPAPI-encrypted for this Windows user (usable without a password, so per-run
worker builds can be signed automatically) and `release.key.pub`, and prints the public key: add
it to `agent/common/trusted_keys.txt`, commit.

**Back the key up** (DPAPI ties the key file to this Windows account, so a copy of it alone is
useless on another PC):
```
farm-release export-key --key %USERPROFILE%\.zero-farm\release.key --out zero-farm-release.minisign.key
```
asks for a passphrase twice on the console (no echo; never as an argument), writes the key as a
standard minisign secret key file encrypted with it (minisign's scrypt format: `minisign -S -s
<file>` can sign with it), prints the key id and public key, and never overwrites a file. Keep that
file plus its passphrase in a password manager. With it, a lost desktop doesn't strand anyone's
install: on a new PC,
```
farm-release import-key --from zero-farm-release.minisign.key --out %USERPROFILE%\.zero-farm\release.key
```
asks for the passphrase and re-wraps the key with DPAPI for that Windows user, and releases carry on.
Without it, installed agents can never verify another update (they trust only this key), and
every friend would have to reinstall from the farm page.

An agent release:
```
farm-release release-agent --url https://farm.greggg230.com --token %ZEROFARM_ADMIN_TOKEN% --channel canary --min-agent 0.1.0 --github greggg230/zero-farm
```
builds `farm-agent.exe` + `ZeroFarm.exe` in release, refuses debug binaries and agents that would
not trust the key, signs them as an agent package (`dist/agent-<version>/`, version = the crate
version), builds `dist/ZeroFarmSetup.exe` (the package embedded; `--github` implies `--setup`),
uploads the package and registers the build (agents self-update from the coordinator), then makes
the GitHub Release `agent-v<version>` with the [`gh` CLI](https://cli.github.com/) (logged in;
`--gh PATH` to use another): a prerelease for canary, a normal release for stable, with the
installer, the four package files, `SHA256SUMS` and `SHA256SUMS.minisig` (PROTOCOL 2.1; staged in
`dist/agent-<version>-github/`), and generated notes (`--notes-file F` to write your own). Only
then does it point `latest` for the channel at the release, with the installer's GitHub URL: the
public [https://farm.greggg230.com/download](https://farm.greggg230.com/download) redirects to the
newest stable installer (else canary's). A re-run replaces the assets (`--clobber`) and refreshes
the notes. If `gh` fails, the channel is left as it was.

For a package that is already built (or released before GitHub Releases existed):
```
farm-release github-release dist\agent-0.2.0 --github greggg230/zero-farm --channel canary --url https://farm.greggg230.com --token %ZEROFARM_ADMIN_TOKEN%
```
checks that the installer (the copy an earlier run kept in `dist\agent-<v>-github\`, else
`dist\ZeroFarmSetup.exe`, which the next release overwrites, or `--setup-exe FILE`) embeds exactly that
package, makes or updates the GitHub Release, and with `--url` publishes the build again
(idempotent) and points the channel at it (`--min-agent`, default the manifest's). Then
`--channel stable` once canary PCs are fine (or the coordinator's
`npm run admin -- agent-release <v> <build_id> stable <min> --github-url URL`). Bump
`agent/agent/Cargo.toml`'s version for every release (agents only update to a newer semver), and
`launcher/Cargo.toml`'s whenever the launcher changed (see "Icon and version"). Without
`--github`, `release-agent` publishes for self-updates only and `/download` keeps pointing at the
previous GitHub installer.

Definitions (PROTOCOL 7; what a run or campaign may make PCs download and run). One time,
a data key, which signs definitions only (the release key never does, and a data key never signs a
build):
```
farm-release keygen --data --out %USERPROFILE%\.zero-farm\data.key
```
then add its public key to `agent/common/trusted_data_keys.txt` (not `trusted_keys.txt`) and
release an agent. Then, for every new version of a run's or campaign's definition:
```
farm-release define eoe2-v7.json --key %USERPROFILE%\.zero-farm\data.key --publish --url https://farm.greggg230.com --token %ZEROFARM_ADMIN_TOKEN%
```
validates the file against `definition/1` (the job templates take only their kind's keys and no
path-like strings), signs its bytes as they are (minisign, prehashed; trusted comment
`definition=1 kind=<kind> id=<id> version=<version>`), refuses a key that isn't a trusted data key
of this build, writes `eoe2-v7.json.minisig`, uploads both to `defs/<kind>/<id>/<version>/`
(`PUT /v1/admin/artifact/...`) and registers it (`PUT /v1/admin/definition`). It prints one JSON
line (`kind`, `id`, `version`, `key`, `sha256`, `signature`, `published`) and exits 0, else prints
`error: ...` and exits 1; the trainer's and campaign tools call it, so this interface stays as
it is. Without `--publish` it only signs.

Worker builds and runtime packs:
```
farm-release sign <dir> --kind worker --game mtg --engine-commit <sha> --core-commit <sha> [--profile fuzz] [--min-agent 0.2.0]
farm-release sign <dir> --kind runtime --version ort-1.23-cpu
farm-release sign <dir> --kind runtime --version cuda-... --min-agent 0.2.10 --gpu-vendor nvidia --gpu-min-driver 527.41 --gpu-providers cuda --zst
farm-release sign <dir> --kind worker --game mtg --engine-commit <sha> --core-commit <sha> --platform macos-arm64 --min-agent 0.3.0
farm-release verify <dir>
farm-release publish <dir> --url ... --token ...
```
Signatures are minisign-compatible: `minisign -Vm <dir>\manifest.json -p release.key.pub`.
`--platform windows-x64|macos-arm64` (PROTOCOL 11.3) writes the manifest's `platform` and needs `--min-agent 0.3.0` or
newer; without it the manifest has no `platform` (= `windows-x64`, as before). A macOS build's executables have no
extension: the entry defaults to the one file in `<dir>` without one (`--entry` when there are several).

## What the friend sees

1. Opens [https://farm.greggg230.com](https://farm.greggg230.com) (what Zero Farm is and does to
   the PC, "you need an invite code from Gregg") and downloads `ZeroFarmSetup.exe` (alpha:
   unsigned, so SmartScreen says "Windows protected your PC"; "More info" → "Run anyway").
   Double-click: no UAC prompt; "Zero Farm is installed. It lives in the notification area." (an
   older installer whose version the farm no longer accepts says so instead, with the download
   link, and the invite stays unused).
2. A small "Join Zero Farm" window asks for the invite code (`XXXX-XXXX-XXXX`) and a name for the
   PC, then disappears.
3. A round "Z" icon in the notification area: green = working, grey = idle/paused, blue =
   downloading/updating, orange = can't reach the farm, red = removed from the farm. Hover: status.
   Click: status, games today, Pause for 1 hour / until tomorrow / until I resume, Resume, When to
   work (only when I'm away / always at low priority), Use the GPU, CPU threads (automatic or a
   number), Open logs folder, Send diagnostics (uploads the logs; a balloon confirms, or says
   when it can be sent again: once an hour), About, Quit.
4. Once, on the first work: a balloon "Zero Farm is now using your idle PC. It stops as soon as you
   need it." Never again.
5. Nothing else: it starts at logon, works after 5 minutes without input (never in fullscreen
   games or presentations, on battery or with low memory), stops within 2 seconds of a mouse move,
   and updates itself silently. "Zero Farm" in Apps & features removes everything.

## How the agent behaves

- **Policy** (`policy.rs`, pure and unit-tested): input idle ≥ 5 min (idle-only mode), no
  fullscreen/D3D/presentation/busy state, on AC and no battery saver, free RAM ≥ max(4 GB, 25 %) - 12.5 % while the PC
  is idle (0.3.6, `mem_floor_now`; the job's memory limit `caps.ram_mb` is the free RAM above that floor, at most
  `max_ram_mb` / half the RAM),
  optional hours, not paused; GPU work also needs no other process > 10 % GPU for 30 s and GPU ≤
  83 °C (resumes at 78 °C). Only yes/no answers are kept; no process, window or file names are read.
  `farm-agent dry-run` logs the decisions on a real PC without doing any work.
- **Leases**: profile digest (resent on 409), caps (threads = physical cores by default, RAM,
  GPU, disk budget), cached builds/checkpoints; only fuzz/diag while > 200 MB of results are unsent.
- **Downloads**: manifest + signature first, then each file with resume and size/sha256 checks into
  a staging folder; nothing unverified enters the cache; builds are re-verified before every worker
  start; LRU eviction under the disk cap (default 3 GB, everything under the root counts).
- **Worker control**: created suspended, put in a job object (killed with the agent, memory limit,
  no crash dialogs), IDLE_PRIORITY_CLASS + EcoQoS for CPU work, then resumed. `yield` → `yielded`
  within 2 s → NtSuspendProcess; `resume` → NtResumeProcess. Paused past `slack_s` (or under memory
  pressure) → `quit` and give the lease back. Deadlines: `hello` 10 s (60 s on a build's first
  start, for antivirus scans), yield 2 s, ping after 30 s of silence, kill after 90 s; while the
  idle-priority worker gets no CPU because the PC is saturated by other programs, deadlines wait
  (up to 10 min) instead of killing it, and a starved CPU worker is suspended instead of killed.
  The worker's job also forbids clipboard, global atoms, desktops, display/system settings and
  logoff (a worker has no UI).
- **Starvation**: every 5 s while the worker should run, the agent checks whether it got any CPU
  (the PC had no idle time and the worker under 100 ms). Starved time is Stats `starved_ms`, and
  like a pause it starts a stall: paused and/or starved longer than `slack_s` in one stretch →
  the lease is given back, so a PC that stays busy doesn't sit on games.
- **Availability**: every policy decision is recorded per local day (`state\availability.json`:
  hours the agent ran, hours the policy said work); the profile reports 7-day means of complete
  days (`on_h_day`, `idle_h_day`), so the admin page shows how available each PC is.
- **Results**: `results\pending\<lease>\` survives restarts; idempotent PUTs with `x-ids` / `x-gen`,
  retried with backoff; 4xx-refused chunks are dropped and left out of `done`; `done` after all
  uploads (409 `missing` handled); a lease left by a dead agent is closed at the next start.
- **Self-update**: `latest` at start and every 6 h, or an `{"update"}` answer: download + verify
  into `versions\<new>\`, then between leases write `state\pending` and exit 75. `healthy-<v>` after
  the first 2xx. Versions the launcher rolled back are skipped for 24 h.
- **Launcher update** (`launcher_update.rs`, PROTOCOL 6): once per run, the current and healthy
  agent compares its package's `ZeroFarm.exe` with the installed one by VERSIONINFO. A newer one
  is staged as `ZeroFarm.new.exe`, must pass `ZeroFarm.new.exe selftest`, then the running
  launcher is renamed `ZeroFarm.old-<n>.exe`, the new one becomes `ZeroFarm.exe`, and the agent
  exits 76. The old launcher starts `ZeroFarm.exe --takeover <pid>` and exits when the new one
  confirms (its first agent ran 10 s); if it can't start, exits early or doesn't confirm in 60 s,
  the old launcher kills it, renames itself back, marks the new one bad for 24 h and carries on.
  Launchers older than 0.2.0 (they don't set `ZEROFARM_LAUNCHER=2` and don't know exit 76) get
  the files swapped after the selftest and the new launcher at the next logon, without the
  takeover probation (only the canary test PC ran such a launcher).
- **Definitions** (`jobplan.rs`, rev 6, PROTOCOL 7): before anything is downloaded, a lease's
  definition (`defs/<kind>/<id>/<version>/definition.json` + `.minisig`, through `/v1/artifact`) is
  verified against the trusted **data** keys, then checked against the lease: its run or
  campaign, `expires`, a version no older than the highest seen for it (`state\definitions.json`),
  the worker build (in `builds`) and runtime (equal to `runtime`), every checkpoint's sha256 (and
  then, when downloading and before every start, its size), the job kind. The job line is the
  definition's template plus only `gid_base` / `games` / `seed` (or `first` / `count`), range-checked
  (whole pairs for a paired eval), plus the agent's own fields, which always overwrite (`op`,
  `lease_id`, `kind`, `model`, `reference`, eval `models`, `threads`, `out_dir`, `runtime_dir`). A
  lease that fails is refused: `done` with `refused: "definition"`, a `/v1/crash` of kind
  `definition` once a day per definition and reason (a coordinator before rev 6 gets a `crash`),
  and the start-failure wait (1 min doubling to 1 h). A mid-lease self-play checkpoint is taken
  only if the definition names it. Model variants (0.3.5, PROTOCOL 14): a checkpoint with `variant` is checked against
  that variant of its gen (`CheckpointDef::for_variant`: its sha256 and size, the gen's reference set), cached as
  `cache\ckpt\<run>\<gen>\model-<name>.onnx` (`Cache::model_file`) and handed over as the model; a legacy lease with
  one is refused; the tuning key's network fingerprint adds the variant's name. **Legacy leases** (no `definition`, coordinators before rev 6)
  get a strict allowlist instead: the job's kind must be the lease's, only the kind's template keys
  (`settings`; `settings`/`seats`/`paired`; `mode`/`env`/`announce_result`; diag `what`/`params`)
  and per-lease fields are kept, everything else (`prefix`, `models`, ...) is dropped, and a string
  that looks like a path (separators, `..`, a drive) refuses the lease. Room for a checkpoint
  file is made before it downloads: its declared size, or 256 MB for a legacy one.
- **Rolling self-play leases** (`rolling.rs`, rev 15, PROTOCOL 10, agent 0.2.13): the agent asks with
  `features: ["rolling"]`; a lease with `rolling` (a reserved block of ids, the run's games in flight for a CPU and a
  GPU job) runs its worker with `rolling: true`, `flush_s`, the released ids as `games` and `conc` / `groups` from the
  run's setting for the provider the measured choice picked ("auto": 32 in 4 groups on the CPU; on a GPU, from the bench
  of the job's network, the games in flight among 64-512 after which doubling gains under 15 % rows/s, one group from
  256, else 2; both capped so the games in flight commit at most 85 % of the worker's memory limit at a base plus a cost
  per game - this PC's measured cost for the run when higher -, rounded down to a multiple of 32; the constants at the
  top of `rolling.rs`). **Memory** (0.2.14, PROTOCOL 10.6): the worker's committed memory is read every tick; at 92 %
  of its limit it finishes its games and a new worker goes on in the same lease with fewer in flight (the released ids
  its predecessor never started, its own results directory); a worker that runs into the limit (an allocation failure,
  or Windows' job peak at the limit) is started again the same way, its games in flight lost and reported once; at most
  4 restarts a lease; the cost per game measured is kept per run and CPU / GPU in `state\rolling_memory.json` and plans
  the run's next leases (0.3.14: a run with no entry of its own plans with the highest entry another run has for the
  same worker build and CPU / GPU - `rolling::Learned::find`, also for the base and the video memory -; the lease's
  plan line says whose: "measured here with run X (the same worker build)"). The
  worker's `low` asks `POST /v1/lease/{id}/extend` for 2 x `conc` (at most every 10 s; `wait_s`, `finish`, a lost
  lease and errors handled); beats carry `gen`, and a newer generation's definition is verified off the lease's
  thread as at grant (`jobplan.rs` `Fetch::recheck`, the same template required) before its checkpoint is fetched and
  sent with its `sha256`. A changed template, a failed check, the coordinator's `finish` or an agent update ready to
  switch to (checked every 6 h during the lease) end it with `finish`. A worker whose `hello` doesn't list `rolling`
  runs the lease as a classic one.
- **Eval leases** (rev 5, PROTOCOL 5.2): every network in the lease's `checkpoints` (model and
  reference) is downloaded, checked against its sha256 and kept in the cache, none evicted while
  the lease runs, and re-checked before the worker starts; the job line gets `models` by gen
  (`model`/`reference` null). `evals` files upload like shards (`x-ids` = gids, never `x-gen`).
  Each `selftest` is kept per gen in Stats `selftest` (`selftest_max_abs` = the largest); a
  network failing it ends the lease with one `crash` report that lists every gen's result.
- **Start failures** (`startfail.rs`, rev 5): a worker Windows refuses to run (os error 4551
  Smart App Control / WDAC, 1260 AppLocker / SRP, 225 Defender) is reported once per build per
  day as `/v1/crash` kind `blocked` with `os_error` (a coordinator that refuses the kind gets a
  plain `crash`), the lease goes back, and no lease is asked for during 15 min, 1 h, 4 h, 16 h,
  then 24 h; the tray says "Windows is blocking Zero Farm's job program (Smart App Control)..."
  with a balloon once. Any other start failure (or a worker that dies before `hello`) is a
  `crash` report and a wait of 1 min doubling to 1 h. A lease whose files can't be downloaded,
  verified or set up waits the same way (without a report), so a bad file is never fetched and
  refused in a loop. The wait is kept in `state\start-backoff.json` across restarts and ends when
  a worker says `hello`. (0.2.1 on a friend's PC with Smart App Control leased 521 times in 45
  min.)
- **Tray** (`tray.rs`): the tray state is never borrowed across a call that can pump or dispatch
  messages (SetForegroundWindow, TrackPopupMenu, MessageBox, Shell_NotifyIcon, ShellExecute,
  DestroyWindow, EnableWindow): the window procedure works out what to do under a short
  `try_borrow_mut`, releases it, then makes those calls; a message that arrives while the state is
  busy is skipped. Every window procedure runs in `logging::guard_ui` (`catch_unwind`): a panic
  is logged and saved as a `/v1/crash` report of kind `agent`, which the engine sends within a
  minute, and the app keeps running. (0.2.1 aborted with 0xC0000409 on tray clicks: "RefCell
  already borrowed" inside the window procedure.)
- **Reports**: `/v1/crash` for worker panics/crashes/hangs (stderr tail), agent panics (sent at the
  next start), caught tray panics, blocked workers, refused definitions, launcher rollbacks (agent
  and launcher) and failed launcher selftests; reports that couldn't be sent wait in
  `state\crashes\` and go out within a minute. Logs uploaded on `upload_diag` or "Send
  diagnostics". Logs: `logs\agent.log`, `worker.log`, `launcher.log`, rotating, under 20 MB
  together.
- **Privacy** (`common/src/redact.rs`): every log line is redacted as it is written (the Windows
  user name becomes `<user>`, the user-profile path `%USERPROFILE%`, in its `\`, `/`, JSON-escaped
  and 8.3 short spellings), and so is every upload that can carry a path: the logs body (also logs
  written before 0.2.5), crash reports (message and worker stderr tail) and Stats (beats and
  `done`).
- **Sign-in** (security review M1): only a coordinator answer marked `x-zerofarm-coordinator: 1`
  with `revoked` makes the agent forget its enrollment ("This PC was removed from the farm"). A
  marked `unauthorized` keeps the token, says "This PC can't sign in to the farm; ask Gregg." in the
  tray and tries again after 1 min doubling to 1 h. Any other 401/403 (a WAF or proxy in front of
  the coordinator) is a network error, retried with backoff.
- **NVML** is loaded only from System32 (`LoadLibraryExW` with `LOAD_LIBRARY_SEARCH_SYSTEM32`, then
  by that full path), never by a bare name searched through the current directory and `PATH`.

## CLI

```
farm-agent run [--root DIR] [--headless] [--coordinator URL]     what the launcher starts
farm-agent dry-run [--root DIR] [--seconds N]                    log policy decisions only
farm-agent install | uninstall [...]                             see "Install flow"
farm-agent enroll --invite CODE [--name NAME]                    headless enrollment
farm-agent profile | keys | version                                keys: "release|data <id> <key>" lines
ZeroFarm.exe [uninstall ... | selftest | keys | --takeover PID]
ZeroFarmSetup.exe [install options]
```

Agent exit codes: 0 quit, 75 switch to `state\pending`, 76 hand over to the new `ZeroFarm.exe`,
2 usage, 3 not enrolled (headless), 4 revoked, 70 internal error.

## Not done yet

- Worker sandbox (DESIGN 4: no network, only its work directory): see the design note below. Today
  the worker runs in a job object (kill on close, memory cap, no crash dialogs, no UI: clipboard,
  atoms, desktops, settings, logoff) at idle priority with EcoQoS, as the friend's user.
- Rev 11 (PROTOCOL 9, agent 0.2.10): run alternates and the agent's guard (`lease.rs` `runtime_guard`: a GPU
  vendor's runtime pack is refused before anything but its manifest is downloaded unless the PC qualifies; from 0.3.2
  a pack without `gpu`, such as a CPU alternate's, is not GPU work),
  zstd copies of build files (`cache.rs`, `zerofarm_common::zst`), probe-first and the bench of the job's own
  network (`state\bench.json`) and the measured CPU-or-GPU choice (`probe.rs` `choose`), `caps.gpu_allowed`,
  `caps.gpu_games`, a failed GPU pack marked bad per driver and evicted.
- Rev 12 (PROTOCOL 9.8, agent 0.2.11): the probe-in-progress marker (`probe.rs` `mark` / `take_inflight`,
  `state\probe-inflight.json` written with `fsutil::durable_write`; an unfinished probe holds its pack in
  `state\unfinished.json`: a day, then for good per adapters and drivers; reported as `probe_unfinished` at
  start by `engine.rs` `note_unfinished_probe`), `--providers` without AMD's providers for probes and benches
  (`PROBE_PROVIDERS`), AMD adapters left out of the profile (`profile.rs` `GPU_VENDORS_OFF`, `split_off`; a probe
  is then pinned to a usable adapter, `pin_gpu`), the coordinator's evict list applied at start
  (`update.rs` `apply_evict`), and the launcher's session-end handling (`SESSION_END_CODES`, `SM_SHUTTINGDOWN`).
- Rev 22 (PROTOCOL 9.8, agent 0.3.11): AMD adapters are reported again, DirectML only (`profile.rs` `VENDOR_PROVIDERS`,
  `probe.rs` `allowed_on`): a probe or bench on a PC with an AMD adapter runs on one adapter (`probe.rs` `scope`: the
  pack's vendor, NVIDIA, AMD, Intel; an AMD one with `--providers dml,cpu`, no `--ensure-ready`), a pack that ships
  Windows ML's catalog never touches an AMD adapter (`PackGpu`, `pack_gpus`), the measured choice never puts another
  provider there, older records of AMD adapters are ignored (`RULES`), and the guard refuses an `amd` pack that brings
  more than `dml` or ships the catalog. The owner's `gpu_vendors_off` from `GET /v1/agent/latest` is kept in
  `state\gpu_vendors_off.json` and applied at start and every update check (`update.rs` `apply_vendors_off`,
  `profile.rs` `vendors_off` / `split_off`); a rolling GPU job on such an adapter is given back (`lease.rs`
  `gpu_vendor_off`).
- GPU work (phase 4, PROTOCOL 8) is built but not released: provider probes between leases
  (`probe.rs`: a cached worker build's `probe-providers` per runtime pack, results in
  `state\probes.json`, the readable report in the worker log), the profile's `providers` /
  `runtime_providers` and adapter LUIDs, `caps.gpu` only with a verified GPU provider, the job line's
  `providers` / `gpu`; the vendor-neutral GPU signals (PDH busy counters, D3DKMT temperature). Not yet:
  gating on the job's own adapter only (today the busiest adapter gates every GPU job). An AppContainer
  worker would need the `packageQuery` capability for Windows ML's provider catalog.
- Authenticode signing (planned before more friends).
- macOS: a menu-bar icon, a notarized `.pkg` / `.app` (Developer ID), fullscreen detection, GPU work (Core ML).
- Layout tuning (rev 19) covers rolling self-play only (CPU; GPU from 0.3.4): eval, fuzz and classic jobs keep their
  fixed rules; `diag` jobs are passed to the worker as they are. Several workers per lease (13.5) likewise. A GPU
  worker's sessions per process (`infer_workers` > 1 on CUDA) are not tried.
- A crew member's closed range ends in a finish beside its successor (13.5): a worker op that hands a running worker a
  new range would let every member run for the whole lease (a worker and aw/1 change).

### Design note: the worker sandbox (AppContainer)

Goal: a compromised or buggy worker can't use the network, and can read only its build and
runtime directories and its checkpoint, and write only its `work\<lease>\`. AppContainer gives
exactly that without admin rights; a restricted token at low integrity would still let the worker
read the friend's files and use the network (blocking that needs the firewall, i.e. admin), so it
is not enough on its own.

1. **Profile.** `CreateAppContainerProfile("ZeroFarm.Worker.<16 hex of the root's hash>", ...)`
   with no capabilities, at install (or lazily before the first worker); `DeleteAppContainerProfile`
   at uninstall. No `internetClient` / `privateNetworkClientServer` capability = no network,
   loopback included. Tests use `ZeroFarmTest.Worker.<hash>` and delete it in a guard.
2. **Files.** An AppContainer process opens only objects whose DACL grants its SID or ALL
   APPLICATION PACKAGES (Windows, System32 and the GPU driver store do, the user profile doesn't).
   The agent adds inheritable ACEs for the container SID once (SetNamedSecurityInfo): read +
   execute on `cache\builds\` and `cache\ckpt\`, modify on `work\`. Never on `state\` (the token),
   `results\`, `logs\`, `versions\`. The worker gets `TEMP`/`TMP` = `work\<lease>\tmp`.
3. **Process creation.** `CreateProcessW` with a `STARTUPINFOEXW` attribute list:
   `PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES` (the container SID, no capabilities) and
   `PROC_THREAD_ATTRIBUTE_HANDLE_LIST` (only the three pipe ends inherit), plus the existing
   `CREATE_SUSPENDED` + job assignment + idle priority + EcoQoS + resume. `std::process::Command`
   can't pass attribute lists on stable Rust (`raw_attribute` is unstable), so `WorkerProc` gets
   its own small spawn (`CreatePipe`, `WaitForSingleObject`, `GetExitCodeProcess`) behind the same
   API; the rest of the agent doesn't change.
4. **Fallback, loudly.** If the profile can't be created (policy, an old Windows build), the worker
   runs as today and the profile says `"sandbox": false` (the admin page shows it); never a silent
   downgrade.
5. **Tests.** The lifecycle suite runs unchanged with the fakeworker inside the container (it
   reads its build dir and writes `out\`). New fakeworker probes, reported in its result files:
   connect to the mock's port (must fail), read `state\machine.json` and a file in the user's
   `%TEMP%` (must fail), write outside `work\` (must fail), write under `work\<lease>\` (works).
6. **Open questions** for phase 4 on a real PC: GPU providers inside an AppContainer (DirectML is
   used by UWP apps and should work; TensorRT-RTX / CUDA need their DLLs and registry reads
   allowed, to be measured), antivirus behaviour with container processes, and whether a fuzz
   worker that spawns child processes for crash bisection keeps working (children inherit the
   container).

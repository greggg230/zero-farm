# zero-farm agent

The Windows app invited friends install so their idle PCs do jobs for zero-core runs (MTG Zero
first), plus its launcher, its installer and the desktop's release tool. The wire contract is
[`../PROTOCOL.md`](../PROTOCOL.md) (v1 rev 6); the reasons are in
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

184 tests (+1 ignored), about 3 minutes on an otherwise idle PC:

| Suite | Tests | Covers |
|---|---|---|
| `common` unit + `manifest_verify` | 33 + 9 | manifest rules and verification: tampered file, size change, missing/extra file, edited manifest, bad signature, unknown key, wrong trusted comment, legacy signatures, key rotation; minisign format; paths; DPAPI; backoff; canonical digest; rotating log; payload format; the round Z; VERSIONINFO write/read/patch and `.res` layout; launcher leftovers; `definition/1` (a good one, every refusal: unknown fields, ids, builds, sizes, templates with foreign keys or paths, seats); path-like strings; per-lease fields (ranges, whole pairs, kind); redaction (profile paths in every spelling, short paths, names as words only, JSON); only a coordinator-marked `revoked` revokes, a WAF's 401/403 is retried |
| `agent` unit | 34 | idle policy (pure function over simulated signals), settings, cache LRU, results store, ranges, update cleanup, invite codes, panic sites, availability (dates, sample-and-hold with gaps, 7-day means); start failures (policy error codes, the 15 min → 24 h and 1 min → 1 h waits and their file, one `blocked` report per build per day, other reports once a day per reason); the tray window procedure re-entered while its state is busy (skipped, no panic) and a panic inside it (caught, reported as `agent`, the tray keeps working); the panic guard; the legacy allowlist (only the kind's keys, paths and kind mismatches refused, eval checkpoints = the seats' networks); definition versions never go back |
| `agent/tests/lifecycle` | 37 | whole leases with the real fakeworker and the mock: download + verify, run, yield/suspend/resume, give-back after slack, upload retries after injected 503s, restart with pending results, orphaned leases, cancel, crash, hang, fatal error, slow yield, no hello, fuzz failures + summary, checkpoint switch, expiry, coordinator give-back, tampered / unknown-key / bad-signature builds, disk cap, profile resend, revoked token (only the coordinator's own `revoked`), an unknown token kept (tray "can't sign in", one try then a wait), a WAF's 403/401 HTML on lease requests, uploads and beats retried as network errors, quit event, launcher events, rev 2 (stale chunks, `done` missing, `upload_diag`, repeated checkpoints, duplicate ids, `x-farm-agent`); rev 3: starvation past slack gives back, brief starvation counted and kept, pause + starvation add up, availability recorded, the worker job's UI restrictions |
| `agent/tests/definitions` | 5 | signed definitions (PROTOCOL 7): the job line is the signed template + per-lease fields + the agent's own fields only (a coordinator's `prefix`, `models`, `out_dir`, `runtime_dir`, `threads`, settings dropped or overwritten), the definition remembered; an eval definition's seats and networks (the coordinator's seats ignored); a mid-lease network the definition doesn't name never fetched; refusals (`done` with `refused`, a `definition` report once per reason, a wait): signed with the release key, tampered bytes, an older version, another run's definition, expired, a build or runtime it doesn't name, a checkpoint hash, a job kind it has no template for, per-lease fields out of range; a legacy lease's allowlist (the review's UNC `models` and `prefix` never reach the worker; a path or a kind mismatch refuses it); a declared checkpoint size over the disk cap never downloaded |
| `agent/tests/eval` | 3 | eval leases (PROTOCOL 5.2) with the fakeworker's eval mode: two networks and their references downloaded, verified and cached; the job line's `models` (absolute paths, hashes, `model` null); every game uploaded with `x-ids` and no `x-gen`, files that split a pair, seat order per game; per-gen `selftest` in Stats; cached checkpoints in the next lease request; a tampered checkpoint refused before any worker starts, then a wait (no lease storm); a network failing its self-test reported once, with every gen's result |
| `agent/tests/blocked` | 5 | a worker Windows refuses to start (scripted os error 4551): one `blocked` report with `os_error` and the build, the lease given back, no lease storm (one lease request while the 15 min wait runs), the tray status and balloon, the wait kept across a restart; shortened waits growing ×4 with the same build reported once; an older coordinator gets it as a `crash`; any other start failure: a `crash` report and a 1 min wait; a worker that starts ends the wait |
| `agent/tests/update` | 6 | self-update in process: download + verify + `state\pending` + exit 75; `{"update"}` answer; tampered update refused; rolled-back version not retried; a required update that can't happen doesn't spin; older/equal ignored |
| `agent/tests/selfupdate_e2e` | 2 | real launcher + real agent processes: update through the launcher; a version that never becomes healthy is rolled back, reported by the old agent, never re-downloaded, cleaned up |
| `agent/tests/launcher_update` | 7 | the launcher updating itself. In process: swap + exit 76 + `launcher-swap`; with a pre-0.2 launcher, swap only (next logon); selftest failure never installs and isn't retried; `plan` (current + healthy only, verified package, never a downgrade or same version, no VERSIONINFO = 0.0.0, bad for 24 h). Real processes: the real launcher hands over to a newer real launcher (old exits 0, new runs the agent, `.old` deleted); fake new launchers that exit at once or never confirm are killed and rolled back, reported, not retried, `.bad` deleted |
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
farm-release verify <dir>
farm-release publish <dir> --url ... --token ...
```
Signatures are minisign-compatible: `minisign -Vm <dir>\manifest.json -p release.key.pub`.

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
  fullscreen/D3D/presentation/busy state, on AC and no battery saver, free RAM ≥ max(4 GB, 25 %),
  optional hours, not paused; GPU work also needs no other process > 10 % GPU for 30 s and GPU ≤
  83 °C (resumes at 78 °C). Only yes/no answers are kept; no process, window or file names are read.
  `farm-agent dry-run` logs the decisions on a real PC without doing any work.
- **Leases**: profile digest (resent on 409), caps (threads = physical cores − 2 by default, RAM,
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
  only if the definition names it. **Legacy leases** (no `definition`, coordinators before rev 6)
  get a strict allowlist instead: the job's kind must be the lease's, only the kind's template keys
  (`settings`; `settings`/`seats`/`paired`; `mode`/`env`/`announce_result`; diag `what`/`params`)
  and per-lease fields are kept, everything else (`prefix`, `models`, ...) is dropped, and a string
  that looks like a path (separators, `..`, a drive) refuses the lease. Room for a checkpoint
  file is made before it downloads: its declared size, or 256 MB for a legacy one.
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
- ONNX Runtime provider detection (`providers` is `["cpu"]`; the runtime pack fills it in later),
  the per-machine self-test and GPU providers (phase 4). NVML signals and DXGI/NVML GPU profiles are
  in place.
- Authenticode signing (planned before more friends).
- Autotuning sweeps and `diag` jobs beyond passing them to the worker.

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

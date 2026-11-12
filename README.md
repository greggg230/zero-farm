# Zero Farm

Zero Farm is a small Windows app that lends a PC's idle time to Gregg's game-playing AIs. While
you're away from your computer it does jobs for AlphaZero-style training runs (MTG Zero first)
and sends the results back. It is invite-only: friends get a code from Gregg.

This repository is everything that runs on your PC from Zero Farm itself, plus the server it talks
to, so you can read exactly what it does before you install it. Start at
[farm.greggg230.com](https://farm.greggg230.com) to install; every release is also on this repo's
[Releases page](https://github.com/greggg230/zero-farm/releases).

Status: **alpha**. The first jobs are fuzz tests of a game engine (millions of random games looking
for crashes), on the CPU only. From version 0.2.3 there are also evaluation jobs, where trained
networks play each other (below). Self-play training games and GPU work come later.

## What it does on your PC

**When it works.** By default only when you're away:

- after **5 minutes** without keyboard or mouse input;
- **never** while a fullscreen game or app, a presentation or Windows' "busy" state is on;
- **never** on battery or battery saver;
- only while at least **4 GB or 25 % of your RAM** (whichever is larger) stays free;
- not while paused (tray menu: 1 hour, until tomorrow, until you resume), and only within your
  chosen hours if you set some.

You can switch to "always, at low priority" in the tray menu. For GPU work (not shipping yet) it
will also wait while another program uses the GPU (over 10 % for 30 s) or the GPU is above 83 °C.

**When you come back** it stops within **2 seconds** of a key press or mouse move: the job is told
to yield and is then suspended (0 % CPU) until you're away again. A job that was paused or starved
of CPU for more than about 10 minutes is handed back to the farm.

**How much it uses.**

| | Limit |
|---|---|
| Priority | Windows' lowest (`IDLE_PRIORITY_CLASS`) plus EcoQoS: anything you run comes first, fans stay quiet |
| CPU | physical cores − 2 threads by default (at least 1); adjustable in the tray |
| RAM | the job's memory is capped by Windows (a job object): half your RAM by default, never more than what is free above the 4 GB / 25 % floor |
| Disk | everything lives in `%LOCALAPPDATA%\ZeroFarm`, hard cap 3 GB including logs (20 MB) and caches; nothing else on the disk is written |
| Network | HTTPS only, to `farm.greggg230.com`; no open ports, no remote access |

**Network use.** About one request every few minutes while idle, a heartbeat every 60 s while
working, and an update check every 6 hours. Downloads: the job programs (tens of MB, once per
engine version), and network files for evaluation jobs (about 13 MB each, kept in the cache).
Uploads: results, a few KB per game or one small file per fuzz job. A busy PC uploads well under
100 KB/s.

**Evaluation jobs** (from 0.2.3). An evaluation job plays two or more trained networks against
each other, for example this week's network against an older one, to measure which is stronger.
Zero Farm downloads every network file the job names (about 13 MB each, plus a small reference
file the job uses to check that the network computes correctly on your PC), checks each file's
size and SHA-256 against the job's signed definition (from 0.2.5; before, against the farm's), and
keeps them in its cache under the 3 GB cap, so a later job with the
same networks downloads nothing. The job program first self-tests every network, then plays the
games on the CPU at the lowest priority, like any other job. What goes back is one small row per
game: which network played which side, who won, by how much and in how many rounds, a few hundred
bytes. Network files are never uploaded.

**If Windows blocks the job program.** Windows 11's Smart App Control (or a company's application
control, or an antivirus) can refuse to run the job program, because it isn't code-signed yet.
Zero Farm then tells the farm (once a day), waits before trying again (15 minutes, then 1 hour,
4 hours, 16 hours, then once a day), and the tray says: "Windows is blocking Zero Farm's job
program (Smart App Control). Zero Farm can't work on this PC until its programs are code-signed.
Nothing is wrong with your PC." There's nothing you need to do.

**What it doesn't do.** No admin rights (it installs into your user account, no UAC prompt), no
service, no drivers, no ads. It reads and writes only its own folder and doesn't look at your
files, windows or programs: to stay out of your way it only asks Windows yes/no questions ("is a
fullscreen app running?", "how long since the last input?") and reads hardware facts (CPU, RAM,
disk space, GPU).

## What it sends

Machine and performance data only ([DESIGN.md](DESIGN.md) 5.4; exact fields in
[PROTOCOL.md](PROTOCOL.md) section 3):

| What | When |
|---|---|
| The name you give the PC and a random machine id | at sign-up |
| Hardware profile: Windows version, CPU model and core counts, RAM, free disk, GPU models / VRAM / driver versions, the ONNX Runtime providers available, hours per day the PC was idle / Zero Farm was running | at sign-up and when it changes (at most daily) |
| Per-job performance: games or seeds done, timings, CPU %, memory, threads, time paused or starved of CPU, how fast it yielded | with each heartbeat and at the end of a job |
| Crash reports: the error message and stack trace of a crashed job or of Zero Farm itself, the job's last log lines; if Windows refuses to run the job program, which kind of policy refused it (a Windows error number) | when something crashes; a blocked job program once a day |
| Logs: Zero Farm's own log files (`%LOCALAPPDATA%\ZeroFarm\logs`), with your Windows user name and user-folder path removed | only when you press **Send diagnostics** in the tray, or when Gregg asks for that machine's logs from the farm |
| The job results | as jobs finish |

Privacy rules:

- No file names, window titles, process names or game names are read or sent. "A fullscreen app /
  another GPU program is active" is only ever a yes/no used to step aside.
- **Logs are redacted.** From 0.2.5, everything Zero Farm writes to its logs, and everything it
  uploads that could carry a file path (the logs, crash reports, performance numbers), has your
  Windows user name replaced with `<user>` and your user folder (`C:\Users\<you>`) with
  `%USERPROFILE%`. Before 0.2.5 the logs contained the install path, and so your Windows user name.
  The logs still contain paths inside Zero Farm's own folder, and the PC name you gave at sign-up
  (by default your PC's name) is sent once, at sign-up.
- **Your IP address** is seen by Cloudflare, like with any website, and the farm's server keeps it
  briefly to rate-limit sign-up attempts (pruned after a day).
- Per-heartbeat performance samples go to Cloudflare Workers Analytics Engine; summaries are kept in
  the farm's database for the admin dashboard.

## What code runs

- **This repo**: the launcher (`ZeroFarm.exe`), the app (`farm-agent.exe`), the installer
  (`ZeroFarmSetup.exe`), the release tool, and the coordinator (the Cloudflare Worker at
  farm.greggg230.com). See [`agent/README.md`](agent/README.md) and
  [`coordinator/README.md`](coordinator/README.md).
- **The jobs** run in *worker* programs built from a private game engine (MTG Zero), which is not
  in this repo. They are signed builds (below): Zero Farm downloads them, checks the signature and
  every file's hash, and refuses anything else. Their source isn't public, so this is what limits
  them **today**:
  - a Windows job object: killed when Zero Farm stops, memory-capped, no crash dialogs, and no UI
    access (no clipboard, global atoms, desktops, display or system settings, log-off);
  - the lowest CPU priority with EcoQoS, and suspended whenever you use the PC;
  - its own working folder under `%LOCALAPPDATA%\ZeroFarm\work`.

  It still runs as your Windows user, so Windows itself would let it read your files or use the
  network. **Planned before the stable channel**: an AppContainer sandbox, where a worker has no
  network access at all and can read only its own build and data and write only its work folder
  ([DESIGN.md](DESIGN.md) section 4, design note in [`agent/README.md`](agent/README.md)). Until then,
  run it only if you trust Gregg's builds.

## Security model

- **Everything executable is signed** with one Ed25519 release key in
  [minisign](https://jedisct1.github.io/minisign/) format: app updates and worker builds alike,
  each as a `manifest.json` listing every file with its size and SHA-256, plus
  `manifest.json.minisig`. The app has the public key compiled in, verifies the signature and every
  file before anything runs (and again before every start), and refuses unknown keys, extra files
  and edited manifests.
- **The secret key never leaves Gregg's PC** (stored DPAPI-encrypted); it is not on Cloudflare or
  GitHub. A passphrase-protected backup of the key is kept in a password manager
  (`farm-release export-key`): with it, a lost desktop doesn't strand anyone's install; without
  it, every friend would have to reinstall from the farm page, because installed copies trust only
  this key.
- **The job instructions come from signed definitions** (from 0.2.5). What a job may make your PC
  download and run (which job programs, which network files with their exact sizes and SHA-256,
  and the exact job settings) is fixed by a *definition* that Gregg signs on his PC with a second
  key, the **data key**, which can sign nothing else. Zero Farm checks the signature, that the
  definition hasn't expired and isn't older than one it has already seen, and builds the job
  program's instructions only from it, plus a few numbers the farm fills in per job (which games or
  seeds). The server (the coordinator) holds neither key, so a compromised server can withhold work
  or waste your idle time, but can't choose what your PC runs or which files the job program reads
  ([PROTOCOL.md](PROTOCOL.md) section 7). A job that fails any check is refused and reported, and
  Zero Farm waits before asking for work again.
- **A "removed from the farm" message** only follows the farm server's own answer. An error page
  from something between your PC and the farm (a firewall, a proxy, Cloudflare's own protection) is
  treated like a network problem: Zero Farm keeps its sign-in and tries again later.
- **The installer isn't Authenticode-signed yet** (alpha), so Windows SmartScreen says "Windows
  protected your PC" (More info → Run anyway). Code signing is planned before more friends join.

The release public key (also in [`agent/common/trusted_keys.txt`](agent/common/trusted_keys.txt)),
key id `1B9342EFFB546500`:

```
RWQAZVT770KTG1hmXk6yJ5i9pF1v5Q4lNLQ9C3fOUo8ynWWfamzJ2LpS
```

The data public key (definitions only; in
[`agent/common/trusted_data_keys.txt`](agent/common/trusted_data_keys.txt)), key id
`1FDB907D9FF199C8`:

```
RWTImfGffZDbH0xWVxO/H563BYqds0G+D+hJLn79XEo9xU3sbl6lLp9D
```

### Verify a download yourself

Each [release](https://github.com/greggg230/zero-farm/releases) has `ZeroFarmSetup.exe`, the app
package (`farm-agent.exe`, `ZeroFarm.exe`, `manifest.json`, `manifest.json.minisig`), and
`SHA256SUMS` with its signature `SHA256SUMS.minisig`. With
[minisign](https://jedisct1.github.io/minisign/) installed, in the download folder:

```
minisign -Vm SHA256SUMS -P RWQAZVT770KTG1hmXk6yJ5i9pF1v5Q4lNLQ9C3fOUo8ynWWfamzJ2LpS
sha256sum -c SHA256SUMS
minisign -Vm manifest.json -P RWQAZVT770KTG1hmXk6yJ5i9pF1v5Q4lNLQ9C3fOUo8ynWWfamzJ2LpS
```

The first line proves `SHA256SUMS` was signed with the release key, the second that every file
matches it, the third checks the app package the way Zero Farm itself does. Without `sha256sum`
(plain Windows), compare `certutil -hashfile ZeroFarmSetup.exe SHA256` (or PowerShell's
`Get-FileHash ZeroFarmSetup.exe`) with the installer's line in `SHA256SUMS`. The installer also
re-verifies the package it carries before installing it.

## Updates

- **Silent.** No prompts, no UAC, no restart. The app checks for a new version every 6 hours (and
  whenever the farm says one is required), downloads it from the farm, verifies it as above, and
  switches between jobs.
- **Rollback.** The launcher starts the new version and gives it 2 minutes to reach the farm; if it
  doesn't, or crashes 3 times in 10 minutes, the launcher goes back to the previous version (the
  last two are kept) and won't retry the bad one for a day. The launcher can update itself the same
  way, with a self-test and automatic rollback. This was tested on purpose with a broken build
  (0.1.2, see [CHANGELOG.md](CHANGELOG.md)).
- **Channels.** New versions go to the **canary** channel first (Gregg's own test PCs; GitHub
  *prereleases*), then to **stable** (everyone; normal GitHub releases). Your invite decides your
  channel.
- Updates come only from the farm and only signed with the release key; GitHub Releases are for
  first installs and for checking what you run.

## Uninstall

Settings → Apps → Installed apps (Apps & features on Windows 10) → **Zero Farm** → Uninstall. That
stops it and removes its Run entry, its Apps entry, the Start-menu shortcut and the whole
`%LOCALAPPDATA%\ZeroFarm` folder. The same from a command line:
`"%LOCALAPPDATA%\ZeroFarm\ZeroFarm.exe" uninstall`. Quit from the tray icon to stop it without
uninstalling.

## Build from source

**App** (Windows, [Rust](https://rustup.rs) 1.95 with the MSVC toolchain, and Visual Studio Build
Tools with the C++ workload and a Windows SDK):

```
cd agent
cargo build --release -p farm-agent -p zerofarm-launcher
cargo test --workspace --all-features -- --test-threads=2
```

A build from source trusts exactly the official release and data keys, so it runs only workers,
jobs and updates Gregg signed. Details, the test suites and the release flow: [`agent/README.md`](agent/README.md).

**Coordinator** (Node.js 22 and npm; tests run in a local Cloudflare runtime, no account needed):

```
cd coordinator
npm install
npm test
npm run check
npm run dev        # local server on port 5400
```

## Repository layout

| Path | What |
|---|---|
| [`agent/`](agent/) | Rust workspace: `agent` (farm-agent.exe, the tray app), `launcher` (ZeroFarm.exe and ZeroFarmSetup.exe), `common` (manifests, signatures, HTTP, DPAPI), `release` (farm-release.exe, the owner's release tool), `fakeworker` / `mockcoord` (test doubles) |
| [`coordinator/`](coordinator/) | the Cloudflare Worker at farm.greggg230.com: sign-up, job leases, signed-build index, results, the onboarding page, the admin dashboard |
| [`DESIGN.md`](DESIGN.md) | why it works this way: goals, architecture, security, diagnostics, the training side |
| [`PROTOCOL.md`](PROTOCOL.md) | exactly what goes over the wire, the signed manifest format, the install layout |
| [`CHANGELOG.md`](CHANGELOG.md) | what changed in each app version |

## License

[MIT](LICENSE). Copyright (c) 2026 greggg230.

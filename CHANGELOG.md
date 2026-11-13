# Changelog

Versions of the Zero Farm app (`farm-agent.exe` and the `ZeroFarm.exe` launcher). Installed copies
update themselves; see [README.md](README.md) for how updates, channels and rollback work. Each
version is a GitHub Release tagged `agent-v<version>`.

## Unreleased (GPU work)

- **Your graphics card can help, whatever its make.** With "Use the GPU" on, Zero Farm checks once
  which ways of running networks on your GPU work on your PC (NVIDIA, AMD or Intel; DirectML on any
  DirectX 12 card, and on Windows 11 24H2 or newer the GPU maker's own Windows ML provider, which
  Windows downloads the first time). A way is used only if it computes the same answers as the
  reference, to the last decimal places that matter, every time. The check runs between jobs, takes
  seconds, and is repeated after a driver update; its readable report is in the logs "Send
  diagnostics" sends.
- **Stopping for other GPU use works on every GPU.** Zero Farm now sees other programs' GPU use the way
  Task Manager does (no longer only on NVIDIA cards) and reads the GPU temperature where the driver
  reports it; a card listed twice by Windows is counted once.

## 0.2.6 (2026-11-12, canary)

- Release-path check: no user-visible changes. 0.2.5 must update itself to it before 0.2.5 goes
  to the stable channel.

## 0.2.5 (2026-11-12, canary)

A security update, from an independent review of the code.

- **Job instructions come only from signed definitions.** What a job may make your PC download and
  run (which job programs, which network files with their sizes and SHA-256, the exact job
  settings) is now fixed by a definition Gregg signs with a separate *data key*. Zero Farm checks
  its signature, that it hasn't expired and isn't older than one it has seen, that every network
  file has the size and hash it names, and builds the job program's instructions only from it plus
  the few numbers the farm fills in per job (which games or seeds) and its own file locations.
  Before, the job program got most of a job's instructions as the farm server sent them, so the
  server, not Gregg's signature, decided them. A job that fails a check is refused, reported once a
  day, and Zero Farm waits before
  asking again. Jobs from today's farm server (before it sends definitions) go through a strict
  allowlist instead: only the fields a job needs, and nothing that looks like a file path.
- **No more "This PC was removed from the farm" by mistake.** Only the farm server's own "revoked"
  answer makes Zero Farm forget its sign-up. An error page from something in between (a firewall, a
  proxy, Cloudflare's protection) is now treated like a network problem and retried. If the farm
  refuses this PC's sign-in, the tray says "This PC can't sign in to the farm; ask Gregg." and Zero
  Farm keeps trying, instead of throwing its sign-in away.
- **Logs are redacted.** Your Windows user name and your user folder (`C:\Users\<you>`) no longer
  appear in Zero Farm's logs, and are removed from everything it uploads: the logs (also older
  ones), crash reports and performance numbers.
- The NVIDIA GPU library (`nvml.dll`) is loaded only from `C:\Windows\System32`, never from another
  folder where another program could have put a file of that name.
- A network file is never downloaded unless the space it needs fits under the 3 GB cap first.

## 0.2.4 (2026-11-12, canary)

- Release-path check: no user-visible changes. 0.2.3 must update itself to it before 0.2.3 goes
  to the stable channel.

## 0.2.3 (2026-11-12, canary)

- **Evaluation jobs**: two or more trained networks play each other, for example to see whether a
  new network beats an older one. Zero Farm downloads each network file the job names (about
  13 MB each, kept in its cache), checks its SHA-256, lets the job program self-test every network
  before the first game, and sends back one small row per game.
- **When Windows blocks the job program** (Smart App Control, a company's application control, or
  an antivirus): the tray now says "Windows is blocking Zero Farm's job program (Smart App
  Control). Zero Farm can't work on this PC until its programs are code-signed. Nothing is wrong
  with your PC." Zero Farm tells the farm once a day and then waits before trying again (15 min,
  1 h, 4 h, 16 h, then once a day). Before, it asked for new work again at once, hundreds of times
  an hour.
- Any other job program that fails to start is reported and retried later (1 minute, doubling up
  to an hour), never in a tight loop; so is a job whose files can't be downloaded or don't match
  their checksums.
- Fix: clicking the tray icon could close Zero Farm ("RefCell already borrowed", seen three times
  in 10 s on one PC). The tray no longer re-enters itself while a menu or message box is open, and
  a problem in the tray is now reported to the farm instead of closing the app.

## 0.2.2 (2026-11-12, canary)

- Release-path check: no user-visible changes. The first version built from this public
  repository; 0.2.1 must update itself to it before 0.2.1 goes to the stable channel.

## 0.2.1 (2026-11-12, stable)

- The first stable release. No user-visible changes from 0.2.0; it proved on a canary PC that
  0.2.0 updates itself, launcher included, and then updated itself to 0.2.2 the same way.

## 0.2.0 (2026-11-12, canary)

- The launcher (`ZeroFarm.exe`) can now update itself too: a new launcher must pass a self-test
  and then take over; if it doesn't, the old one puts itself back and carries on.
- **Send diagnostics** in the tray menu uploads Zero Farm's logs when you want to report a problem
  (at most once an hour).
- A PC that is busy with other programs no longer holds on to jobs it can't get CPU time for:
  being starved of CPU counts like a pause, and the job is handed back after about 10 minutes.
- The farm now sees how available each PC is (idle hours and running hours per day, a 7-day
  average), to hand out the right amount of work.
- Jobs are not allowed to touch the clipboard, desktops, display or system settings, or log you
  off.
- An installer too old to join tells you where to get a new one, and your invite code stays
  valid.
- The round green "Z" icon and version information on every program (Explorer → Properties).

## 0.1.3 (2026-11-11, canary)

- Fix: when the farm required an update that couldn't be installed (the required version had been
  rolled back on that PC), the app asked for work several times a second. It now waits 5 minutes
  between attempts.

## 0.1.2 (2026-11-11, canary only, withdrawn)

- A deliberately broken build that crashes at start, published only to the canary channel to test
  automatic rollback on a real PC. The launcher went back to 0.1.1 after 3 crashes and marked
  0.1.2 as bad. Never offered to the stable channel; 0.1.3 replaced it.

## 0.1.1 (2026-11-11, canary)

- Silent self-update test from 0.1.0; no user-visible changes.

## 0.1.0 (2026-11-11, canary)

- First release: per-user installer (no admin rights), sign-up with an invite code, tray icon and
  menu, the idle rules (5 minutes without input; never in fullscreen apps, on battery or with low
  memory; stops within 2 seconds), signed and verified downloads, jobs at the lowest priority in a
  memory-capped job object, results that survive restarts, silent self-update with automatic
  rollback, uninstall from Apps & features.

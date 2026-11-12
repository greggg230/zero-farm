# Changelog

Versions of the Zero Farm app (`farm-agent.exe` and the `ZeroFarm.exe` launcher). Installed copies
update themselves; see [README.md](README.md) for how updates, channels and rollback work. Each
version is a GitHub Release tagged `agent-v<version>`.

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

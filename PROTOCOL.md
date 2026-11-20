# zero-farm protocol v1

The contract between the four pieces: **coordinator** (Cloudflare Worker), **agent** (the app on a
friend's PC), **worker** (per-game native binary the agent runs), and the **desktop tools**
(release signing, trainer). DESIGN.md says why; this file says exactly what goes over the wire.
A change to anything here is a protocol change: bump the version it belongs to and note it at the
bottom.

Versions: HTTP API `v1` (path prefix), agent↔worker protocol `aw/1`, network files `zero-onnx/1`,
manifests `manifest/1`. Current revision: v1 rev 20 (section 14, model variants per generation; rev 19: section 13, layout tuning and
several workers per lease; rev 18: section 12, hardware routing; rev 17: section 11, platforms: macOS on Apple silicon).

## 1. Conventions

- HTTPS JSON (`content-type: application/json`), UTF-8, except artifact/result bodies (bytes).
- Times: integer Unix seconds (`t`, `expires_at`). Sizes in bytes. Hashes: lowercase hex sha256.
- Id ranges (game ids, fuzz seeds) are lists of half-open ranges `[[lo, hi], ...]` meaning
  lo <= id < hi, everywhere (`unfinished`, `x-ids`, result listings).
- IDs: `machine_id` `m_<26 base32>`, `lease_id` `l_<26>`, `invite` codes `XXXX-XXXX-XXXX` (base32, no
  0/1/O/I), runs and campaigns are slugs `[a-z0-9][a-z0-9-]{0,62}`.
- Auth: `authorization: Bearer <token>`. Machine tokens are 32 random bytes base64url, stored in D1
  only as sha256. One admin token (Worker secret `ADMIN_TOKEN`), used only from the desktop. Rev 6:
  `/v1/admin/*` also accepts a Cloudflare Access login (section 4).
- **Coordinator mark** (rev 6): every `/v1/*` answer, errors included, carries the header
  `x-zerofarm-coordinator: 1`. A 401/403 without it came from something on the path (a proxy, a
  firewall or WAF rule, a misplaced Access policy), not from the coordinator.
- Errors: HTTP 4xx/5xx with `{"error": "<code>", "message": "<text>"}` (some add fields, named
  below). Codes and their statuses: `bad_request` 400 (405 for a method a path doesn't take),
  `unauthorized` 401 (403 for an unknown or used invite code), `revoked` 403, `not_found` 404,
  `conflict` 409, `gone` 410 (lease expired/cancelled, stale result), `too_large` 413 (a body, or rev
  6 a lease's result limits),
  `update_required` 426, `rate_limited` 429, `internal` 500. Clients retry 5xx and network errors
  with exponential backoff (1 s → 5 min, jitter); never retry 4xx except `429` (honour
  `retry-after`).
- Rate limits (rev 3): a token bucket per machine (GCRA: on average one request per interval, a
  burst at once). `POST /v1/lease` 1 per 5 s after a burst of 6; beats 1 per 10 s after 12 (all
  of a machine's leases together); `POST /v1/crash` 1 per 10 s after 30; a user-initiated
  `POST /v1/diag/{name}` 1 per hour; `POST /v1/enroll` 1 per 60 s after 10 per client IP (rev 6:
  the coordinator keys it by an HMAC of the address, IPv6 by its /64, and deletes the row once the
  limit has reset; it never stores an address). Over
  the limit → 429 `rate_limited` with header `retry-after: <s>` and `"retry_after": <s>`; a
  refused request changes nothing (no lease work, nothing stored). A well-behaved agent (one
  lease request per `wait_s` or per finished lease, one beat per `beat_s`) never hits them.
- Every agent request carries headers `x-farm-agent: <version>` and `x-farm-aw: 1`. `/v1/lease`
  refuses a request without a semver `x-farm-agent` (400); `x-farm-aw` defaults to 1.

## 2. Signed manifests (`manifest/1`)

Everything executable reaches a friend's PC only through a manifest signed with the release key.

```json
{
  "manifest": 1,
  "kind": "agent" | "worker" | "runtime",
  "version": "0.3.1",                       // agent: semver; worker/runtime: free-form label
  "created": 1794300000,
  "game": "mtg",                            // worker only: the game repo ("mtg" = mtg-zero)
  "engine_commit": "60397f3...", "core_commit": "f9ca5c3...",   // worker only (full shas)
  "profile": "release" | "fuzz",            // worker only
  "aw": 1,                                  // worker: agent↔worker protocol it speaks
  "zero_onnx": [1],                         // worker: network file versions it can load
  "min_agent": "0.3.0",                     // worker/runtime: oldest agent that can run it
  "requires_runtime": "<build_id>",         // worker: the ONNX Runtime pack it links (optional)
  "entry": "mtgz-worker.exe",               // worker/agent: the executable to run
  "gpu": {"vendor": "nvidia", "min_driver": "527.41", "providers": ["cuda"]},  // runtime only, rev 11 (9.2)
  "files": [ {"path": "mtgz-worker.exe", "sha256": "...", "size": 26214400,
              "zst": {"sha256": "...", "size": 9000000}} ]                    // `zst`: rev 11 (9.3), optional
}
```

Rev 11 (section 9): a runtime manifest's optional `gpu` is a hard hardware requirement (9.2); a file's optional
`zst` names its zstd-compressed copy at `builds/<build_id>/<path>.zst` (9.3). Both appear only in manifests with
`min_agent` 0.2.10 or newer. To run one engine with several packs, its executable is signed once per pack (9.1).

- **Bytes signed** = the manifest file exactly as stored (`manifest.json`); never re-serialised.
- **build_id** = first 24 hex chars of sha256(manifest.json bytes).
- **Signature**: minisign format (Ed25519, prehashed `ED`, `manifest.json.minisig`), the trusted
  comment holding `build_id=<id> kind=<kind>` (more `key=value` pairs may follow; the agent requires
  `build_id` and `kind` to match the manifest it verifies). Legacy (non-prehashed) signatures are
  refused. The agent embeds a list of trusted public keys (key rotation = ship an agent that trusts
  old + new, then sign with new).
- `files[].path`: relative, `/`-separated, no `..`, no drive or device names, never `manifest.json`
  or its signature. Files are fetched from the manifest key's directory: `builds/<build_id>/<path>`.
- The agent verifies the signature, then every file's size and sha256, before anything is executed
  or loaded. A build directory must hold exactly the listed files plus the manifest and signature:
  an unlisted file (e.g. a planted DLL) fails verification. Files live in `cache/builds/<build_id>/`
  and are re-verified on each start of a worker.
- An `agent` manifest has `entry: "farm-agent.exe"` and also contains `ZeroFarm.exe` (the launcher:
  used by the installer, and put in place by an agent when it is newer than the installed one,
  section 6). Since rev 3 every exe carries a VERSIONINFO resource (file version = its crate
  version) and the round "Z" icon.
- The coordinator may verify signatures too (it knows the public key) but never holds the secret key.

R2 layout (served through the Worker, never public):

```
builds/<build_id>/manifest.json | manifest.json.minisig | <files...>
agent/<version>/            -> also a build: builds/<build_id>/..., indexed by version+channel in D1
ckpt/<run>/<gen>/model.onnx | reference.npz       (reference set for the per-machine self-test)
results/<run|campaign>/<lease_id>/<seq>-<sha16>   (uploaded result chunks; sha16 = first 16 hex of their sha256)
diag/<machine_id>/<t>-<name>                      (logs/profiles uploaded on request or by the friend)
diag/<machine_id>/<t>-<lease_id>-<seq>-<sha16>    (results of diag leases)
```

Installers are not in R2 (rev 4; rev 3 kept them under `setup/`, now unused): each agent release
is also a GitHub Release (below), and `GET /download` redirects there.

### 2.1 GitHub Releases (rev 4)

`farm-release` makes the GitHub Release `agent-v<version>` of the public repository for every agent
release it is asked to (a prerelease for canary, a normal release for stable). Assets:

| Asset | What |
|---|---|
| `ZeroFarmSetup.exe` | the installer (the signed agent package embedded) |
| `farm-agent.exe`, `ZeroFarm.exe`, `manifest.json`, `manifest.json.minisig` | the agent package exactly as agents download it from the coordinator |
| `SHA256SUMS` | `<sha256 hex>  <name>` per line (LF, sorted by name, `sha256sum -c` format) for the five files above |
| `SHA256SUMS.minisig` | minisign signature (prehashed) of `SHA256SUMS` with the release key; trusted comment `file=SHA256SUMS version=<version> build_id=<build_id>` |

So `minisign -Vm SHA256SUMS -P <release public key>` then `sha256sum -c SHA256SUMS` checks every
asset, and `minisign -Vm manifest.json -P <key>` checks the package as an agent does. The agent
itself never talks to GitHub: self-updates still come from `/v1/artifact` (section 6).

Result keys carry the hash so two different bodies uploaded for one `seq` can never overwrite each
other; the trainer uses the `key` the result listing gives it.

## 3. Friend-facing API (bearer = machine token)

### POST /v1/enroll  (no auth)
Request `{"invite": "ABCD-EFGH-JKLM", "name": "Sam's desktop", "profile": Profile,
"profile_digest": "<optional, as in /v1/lease>"}` →
`{"machine_id": "m_...", "token": "<base64url>", "channel": "stable"}`. Invite codes are single-use
(case-insensitive); a used or unknown code → 403 `unauthorized`. The channel comes from the invite.
An `x-farm-agent` older than that channel's `min_agent` → 426 `update_required`
`{"min_agent": "...", "download": "https://farm.greggg230.com/download"}` (the invite is not used
up; `download` names the invite's channel, `?channel=canary`, when that isn't stable).

`Profile` (also sent when it changes, in `/v1/lease`):
```json
{"os": "Windows 11 26100", "cpu": "AMD Ryzen 7 7800X3D", "cores": 8, "threads": 16,
 "perf_cores": 8, "eff_cores": 0, "ram_mb": 32768, "disk_free_mb": 412000,
 "gpus": [{"vendor": "nvidia", "name": "RTX 4070", "vram_mb": 12282, "driver": "610.88", "luid": "000000000000d1f3"}],
 "providers": ["dml", "cpu"],
 "runtime_providers": {"<runtime build_id>": ["dml", "cpu"]},
 "idle_h_day": 9.5, "on_h_day": 14.2}
```
Rev 7 (section 8): `gpus[]` has one entry per adapter LUID (`luid`: 16 lowercase hex digits; DXGI may
list one card twice); `providers` lists only the execution providers that passed this PC's probe, with
any runtime pack, in the agent's order of preference, `cpu` last (`["cpu"]` before any probe);
`runtime_providers` (optional, at most 16 runtime builds x 16 names) the same per runtime pack (rev 11: an empty
list when that pack was probed here and nothing passed, or its probe failed). Provider
names: `cpu`, `tensorrt_rtx`, `cuda`, `migraphx`, `amdgpu`, `openvino`, `dml`; names a coordinator doesn't
know are kept and ignored for routing. LUIDs change when Windows restarts, so the profile is resent then.
`idle_h_day` / `on_h_day` (rev 3, optional, 0..24): availability, the mean hours per day over the
last 7 complete local days that the agent's idle policy allowed work / the agent was running
(days it didn't run count 0); absent before the first complete day. They change at most once a
day, so the profile is resent at most daily for them.

### POST /v1/lease
Request:
```json
{"profile_digest": "<sha256 of canonical Profile>", "profile": Profile | null,
 "caps": {"threads": 6, "ram_mb": 12000, "gpu": true, "gpu_allowed": true, "disk_mb": 2500, "gpu_games": {"eoe2": 24}},
 "cached": {"builds": ["<build_id>", ...], "ckpts": ["<run>/<gen>", ...]},
 "kinds": ["fuzz", "eval", "selfplay", "diag"]}
```
`caps.gpu_allowed` (rev 11, agents from 0.2.10, optional): the policy and the user's "Use the GPU" setting allow
GPU work right now, whether or not a GPU provider has passed a probe (`caps.gpu` keeps its meaning: allowed and a
GPU provider passed; section 9.4 uses `gpu_allowed`). `caps.gpu_games` (rev 11, optional): per run, the fewest games per lease
at which this PC's GPU pays, from its bench (section 9.4).
`profile` is sent only when the digest changed (the coordinator answers 409 `conflict` with
`{"need_profile": true}` if it doesn't know the digest). The digest is opaque to the coordinator:
it only compares it with the last one it stored, so the agent may compute it any stable way
(sha256 of the profile JSON with sorted keys is the suggestion). `kinds` the coordinator doesn't
know are ignored.

Response, exactly one of:
- `{"lease": Lease}`
- `{"wait_s": 300, "reason": "no_work" | "paused_by_admin" | "too_many_leases" | "outside_hours"}`
  (`outside_hours` is the agent's own; the coordinator never sends it; `too_many_leases`, rev 6:
  the machine already holds `MAX_ACTIVE_LEASES` live leases, default 2)
- `{"update": {"min_agent": "0.4.0"}}` → the agent updates itself (section 6) before asking again.
  Sent when the agent is older than the `min_agent` of its channel's latest release, or older than
  the `min_agent` of a build it would otherwise be given and the channel's latest release is new
  enough for it. A job whose build needs an agent the channel doesn't offer yet is skipped. A worker
  build whose `aw` differs from the request's `x-farm-aw` is skipped. Rev 5: an eval job needs
  agent 0.2.3 or newer (the first that downloads `checkpoints`) on top of its builds' `min_agent`,
  with the same rule (update if the channel offers it, else skipped). A build this machine
  reported `blocked` (`/v1/crash` below) is skipped for that machine. Rev 6 (section 7): a run or
  campaign with a definition needs an agent of at least `DEFINITION_AGENT` (0.2.5), with the same
  rule; an agent of at least `DEFINITION_AGENT` gets nothing from a run or campaign without one, and
  no diag leases. Rev 9: a run's or campaign's own `min_agent` (section 4) counts like a build's,
  with the same rule: an older agent is told to update when its channel's latest release is new
  enough, else that run or campaign is skipped for it and it gets other work (a canary-only agent
  release keeps the stable channel's agents on everything else).

Any of the three may also carry `"upload_diag": ["logs"]` (as a beat answer can): a machine with no
lease hears about a diagnostics request on its next `/v1/lease`.

How the coordinator picks (DESIGN 5.1): diagnostics asked for this machine first; then runs'
evaluation (unissued games) and self-play (short of quota), by run priority × fit ÷ (1 + that job's
active leases), evaluation before self-play at the same tier (rev 7; rev 18: on a PC that plays self-play on its GPU,
only a batch the CPU PCs couldn't finish within the horizon, section 12.2); then fuzz, the backfill (rev 14: a campaign with a `share` competes with the runs' games within it, section 5.4; also where a
job with `"gpu": "preferred"` goes on a PC that isn't GPU-eligible for it, section 8). Caps must cover the job's `needs` and the disk its uncached builds (and, rev 5, an
eval job's uncached checkpoints) take. A lease is
sized to about 450 s of this machine's measured rate for that job (ids per running second, from its
past leases; its hardware class's average before it has one; 16 games / 10 eval games / 20000
seeds before either), unless the run sets `games_per_lease` / the campaign sets `chunk` (rev 18: an `auto` eval
lease on the CPU gets at least 2 games per thread, section 12.3). Rev 18: after fuzz, tier 3, a PC's self-play that it
would play on its CPU while the run has a GPU version (section 12.1). Every
lease covers one contiguous id range.

```json
Lease = {
  "lease_id": "l_...", "kind": "fuzz" | "eval" | "selfplay" | "diag",
  "issued_at": 1794300000, "expires_at": 1794300180,   // 3 beats; every beat sets it to now + 3 beats
  "beat_s": 60, "slack_s": 600,                        // paused longer than slack -> give back
  "run": "eoe2" | null, "campaign": "fuzz-delver" | null,
  "build":   {"build_id": "...", "manifest": "builds/<id>/manifest.json"},
  "runtime": {"build_id": "...", "manifest": "builds/<id>/manifest.json"} | null,
  "checkpoint": {"gen": 81, "model": "ckpt/eoe2/81/model.onnx", "sha256": "...",
                 "reference": "ckpt/eoe2/81/reference.npz", "reference_sha256": "...",
                 "variant": "gpu"?} | null,                // `variant`: rev 20, section 14.3 (agents from 0.3.5)
  "checkpoints": [Checkpoint, ...] | null,              // eval only (rev 5, section 5.2): every
                                                       // network the job's seats name, by gen
  "definition": {"key": "defs/<kind>/<id>/<version>/definition.json", "sha256": "..."},
                                                       // rev 6, section 7; absent on legacy leases
  "providers": ["dml", "cpu"] | null,                  // rev 7, section 8: the job's provider allowlist
                                                       // (null: any); a routing hint, not signed
  "job": Job                                           // section 5; with a definition only
                                                       // {"kind", "gid_base", "games", "seed"} or
                                                       // {"kind", "first", "count"}
}
```

### POST /v1/lease/{lease_id}/beat
`{"state": "running" | "paused", "progress": {"games_done": 12, "games_total": 40},
  "stats": Stats}` → `{"action": "continue" | "cancel" | "give_back",
  "expires_at": 1794301500, "checkpoint": {...} | null}`. A newer `checkpoint` (self-play) means:
switch at the next game boundary; it is repeated in every beat while it is newer than the lease's
own checkpoint (ignore repeats). Rev 6: only a gen the lease's own definition signs is offered, so
a definition lease normally keeps its gen (a new gen comes with a new definition version and
reaches new leases); eval leases never switch. `cancel`: stop now, discard (sent when the run or campaign was
paused, or the run dropped that job kind; the lease is closed, so later calls get 410).
`give_back`: finish uploading finished games, then `done` with `given_back: true` (also sent, as a
backstop, once a lease has reported `paused` for longer than `slack_s`). A beat for an expired or
cancelled lease → 410 `gone`; a lease id that isn't this machine's → 404 `not_found`. A lease
that misses 3 beats expires and its unfinished ids go back to the pool.

### PUT /v1/lease/{lease_id}/result/{seq}
Body: result bytes (≤ 32 MB, else 413 `too_large`); headers:
- `x-sha256` (required; a body that doesn't match → 400);
- `x-ids`: JSON half-open ranges of the game ids (self-play/eval: from the worker's `gids`) or
  seeds (fuzz) this chunk completes, inside the lease's `[gid_base, gid_base + games)` /
  `[first, first + count)` (else 400). An id counts once: it is credited to the first chunk, from
  any lease, that reports it; later copies are duplicates. A chunk without `x-ids` completes
  nothing (fuzz per-failure results, diag results). Ids a lease never reports are reissued.
- `x-gen` (self-play): the oldest checkpoint generation any game in the chunk was played with
  (default: the lease's checkpoint). Ignored on eval chunks (rev 5).

Rev 6 limits (security review M3; Worker vars): at most `MAX_LEASE_CHUNKS` chunks (default 2,000)
and `MAX_LEASE_BYTES` bytes (default 1 GiB) per lease, else 413 `too_large` with `"limit": "chunks"`
or `"bytes"` (checked against `content-length` before the body is read); no new chunk more than
`LATE_RESULTS_S` (default 24 h) after the lease closed, else 410 `gone`. A retry of a stored chunk
is still answered. A fuzz failure's `seed` must be one of the lease's seeds (else 400). A
machine's measured rate counts only the ids credited to its chunks.

→ `{"ok": true, "duplicate": false, "credited": 4, "already_credited": 0}` (an eval chunk adds
`"held": n`, section 5.2). Idempotent: the same
`seq` with the same hash → 200 (`duplicate: true`); a different hash → 409. `seq` counts from 0 per
lease. Late chunks (the lease expired, was given back or is done) are accepted: the trainer reads
results by cursor, so it sees them whatever it acked before. 410 `gone`: the lease was cancelled,
or a self-play chunk is older than the run's `max_stale` generations behind its current checkpoint;
the agent drops that chunk. A fuzz chunk must be the JSON of section 5.1 (else 400).

### POST /v1/lease/{lease_id}/done
`{"results": [0, 1, 2], "stats": Stats, "given_back": false,
  "unfinished": {"gids": [[1200, 1215]]} | null, "refused": "definition"?}` → `{"ok": true,
  "returned": 15}`. `refused` (rev 6, section 7): the agent refused the lease; it is closed like a
give-back (state `given_back`, reason `refused:<why>`) and counted on the admin page. `results` lists
every seq the agent uploaded; any the coordinator doesn't have → 409 `conflict`
`{"missing": [seq, ...]}` (upload them, then call `done` again). Unfinished ids go back to the
pool; the coordinator works them out itself (the lease's ids minus every id credited to any chunk),
so `unfinished` (a fuzz lease reports `{"seeds": [[a, b]]}`) is only checked to lie inside the
lease (else 400). `returned` is how many ids went back. Repeating `done` → 200. On an expired lease
→ 200 (stats kept; its ids went back when it expired); on a cancelled lease → 410.

### Other
- `GET` (or `HEAD`) `/v1/artifact/{key}` → bytes (R2 key from a lease/manifest; only keys under
  `builds/`, `ckpt/` and, rev 6, `defs/<kind>/<id>/<version>/definition.json` and its `.minisig`;
  anything else → 404; supports `range` → 206 and `if-none-match` → 304).
- `GET /v1/agent/latest?channel=stable` → `{"version": "0.3.1", "build_id": "...",
  "manifest": "builds/<id>/manifest.json", "min_agent": "0.3.0"}`: the latest release on the
  machine's channel; nothing released on it → 404. Rev 10: `channel` is accepted and ignored. Agents
  send the channel they were enrolled on, which goes stale when `POST /v1/admin/machine/{id}` moves
  the machine; the machine's channel as the coordinator holds it is the one that counts. Rev 12:
  `"evict": ["<build_id>", ...]` (only when the owner's evict list is not empty, `POST /v1/admin/evict`):
  agents from 0.2.11 delete those builds from their cache at once (section 9.8); older agents ignore the key.
- `POST /v1/presence` `{"reason": "user_active"}` → `{"ok": true}` (rev 16; migration 0012). An agent that may
  not work right now sends why, at most every 60 s: when the reason changes and every 5 min while it lasts, in
  the background (a failure changes nothing; a coordinator without it answers 404, which the agent ignores).
  Reasons (agent policy `Block`): `user_active` (input within the idle threshold), `fullscreen` (agents from 0.3.7:
  Windows' "busy" / "app" states only while a foreground window covers its monitor), `outside_hours`,
  `on_battery`, `battery_saver`, `low_memory`, `paused` (paused from the tray). Any slug
  `[a-z][a-z0-9_]{0,31}` is stored, so a newer agent's reason needs no coordinator change. The coordinator
  keeps it on the machine (`presence`, `presence_t`) and moves `last_seen`, so an agent that is on but not
  allowed to work stays online. The admin stats show it (machine and contributor `presence`) while it is
  under 10 min old and no later contact replaced it (a contact more than 30 s after it); a contributor
  with an active lease has none. Rate limit 60 s, burst 5. Before rev 16 such an agent was silent, so a PC
  in use and a PC switched off looked the same.
- `POST /v1/crash` `{"build_id", "kind": "panic" | "crash" | "agent" | "blocked" | "definition" | "probe_unfinished",
  "message", "backtrace", "lease_id", "os_error"?}` → `{"ok": true}` (message kept to 4 KB,
  backtrace to 64 KB). Rev 6: at most `MAX_CRASHES_PER_DAY` (100) rows per machine per 24 h are
  kept, for `CRASH_RETENTION_DAYS` (30); past the cap the answer is `{"ok": true, "stored": false}`
  (a `blocked` block is still recorded). `definition`: a lease the agent refused (section 7). `panic`: a Rust panic (a worker's, seen on its stderr, with the worker `build_id`; or the
  agent's own, sent at its next start, with the agent's `build_id`). `crash`: a worker that exited
  without `end`, hung, missed a deadline or broke aw/1; `backtrace` = its stderr tail (agents may
  add `"exit_code"`). `agent`: a launcher rollback or refused update, reported by the next agent
  that runs. `blocked` (rev 5): Windows refused to start the worker `build_id` (Smart App Control /
  WDAC, AppLocker / SRP or Defender; `os_error` 4551, 1260 or 225). The coordinator then stops
  offering that build to that machine for 24 h, or until the machine asks for work with a newer
  agent than the one that reported it (a newer build is a different `build_id`, so it is offered at
  once); the admin page lists it under the machine, and `POST /v1/admin/machine/{id}` `{"unblock":
  true}` lifts it. A `blocked` report without `build_id` → 400. `probe_unfinished` (rev 12, agents from 0.2.11):
  the agent or the PC stopped during a provider probe or a bench (section 9.8), found at the next start;
  `build_id` is the worker build that ran it.
- `POST /v1/diag/{name}` body bytes (≤ 32 MB) → `{"ok": true, "key": "diag/..."}`, when the
  coordinator asked for it in a beat or lease answer (`"upload_diag": ["logs"]`); else, with header
  `x-farm-diag: user` (rev 3: the friend pressed "Send diagnostics"), ≤ 20 MB (else 413) and
  once an hour per machine (else 429); else 409 `conflict`. Any upload answers the open requests
  of that name. An unanswered request is asked again after 30 min. `logs`: `text/plain`, an
  optional `===== about =====` header (agent version, status, settings), then the agent, worker
  and launcher logs, each tail-capped at 4 MB. Rev 6: diagnostics uploads and diag-lease results (R2
  objects and rows) are deleted after `DIAG_RETENTION_DAYS` (14).

### Public pages (no auth, rev 3)
- `GET /`: the onboarding page for invited friends (what Zero Farm is and does to a PC, the
  download, "you need an invite code").
- `GET` (or `HEAD`) `/download[?channel=stable|canary]` (rev 4): `302` to the installer on GitHub
  Releases (`location` = the release's `github_url`; headers `x-farm-version`, `x-farm-channel`,
  `cache-control: no-store`). Without `channel`: the newest stable release that has a
  `github_url`, else the newest canary one; an installer older than its channel's `min_agent` is
  skipped (enroll would refuse it). None → 404 `not_found` ("no installer has been released
  yet"). The coordinator serves no installer bytes.
- The onboarding page links the source code and all releases:
  [https://github.com/greggg230/zero-farm](https://github.com/greggg230/zero-farm).

Agent-side notes: the agent numbers its uploads (`seq` counts every result it uploads for a lease,
including fuzz JSON; a worker's own `seq` is informational). A chunk refused with a 4xx (410 stale or
cancelled, 409, 400) is dropped and left out of `done`'s `results`; `done` is sent once every other
chunk is up. A lease left open by an agent that stopped is closed at that agent's next start with
`given_back: true` once its saved results are uploaded. The agent beats (`running`, 0 games) while
it downloads what a lease needs. A 401/403 (rev 6, agent 0.2.5): only `revoked` 403 with the
coordinator mark (section 1) makes the agent forget its enrollment and ask for a new invite; a
marked `unauthorized` keeps the token and backs off; an unmarked 401/403 is treated like a network
error (retried with backoff). Older agents stop at any 401/403.
A worker that can't be started (rev 5) is never re-leased at once: the lease is given back and the
agent waits before asking again. Blocked by policy (os error 4551
ERROR_SYSTEM_INTEGRITY_POLICY_VIOLATION, 1260 ERROR_ACCESS_DISABLED_BY_POLICY, 225
ERROR_VIRUS_INFECTED): a `blocked` crash report once per build per day, then 15 min, 1 h, 4 h,
16 h and 24 h between lease requests (capped at 24 h), and the tray says Windows is blocking the
job program; any other start failure (or a worker that dies before `hello`): a `crash` report and
1 min doubling to 1 h. A lease whose files can't be downloaded, verified or set up waits the same
way (no report). The backoff resets when a worker starts.

`Stats` (per beat/done; all optional, counters since lease start):
```json
{"games": 12, "decisions": 6100, "leaves": 410000, "infer_batches": 3300,
 "infer_ms_p50": 4.1, "infer_ms_p95": 9.8, "search_ms": 51000, "infer_ms": 40000, "wait_ms": 3000,
 "cpu_pct": 540, "gpu_pct": 0, "vram_mb": 0, "rss_mb": 900, "threads": 6, "concurrency": 32,
 "provider": "cpu", "gpu_temp_c": null, "throttle": [], "yields": 1, "yield_ms_max": 640,
 "selftest_max_abs": 3.1e-6, "paused_ms": 20000, "resume_ms_max": 80, "starved_ms": 0,
 "selftest": {"81": {"provider": "cpu", "max_abs": 3.1e-6, "ok": true}}}
```
`selftest` (rev 5): the worker's `selftest` result per network generation (an eval lease has one per
checkpoint); `selftest_max_abs` is the largest `max_abs` among them. The coordinator keeps at most
16 entries, keyed by gen (other keys are dropped).
`starved_ms` (rev 3): time the worker should have been running but got no CPU because other
programs saturated the PC (it runs at idle priority); the agent samples this every 5 s. It counts
toward the lease's slack like a pause: a lease that has been paused and/or starved for longer than
`slack_s` in one stretch is given back (`given_back: true`). The beat `state` stays `running`
while starved. The coordinator keeps the fields above (`paused_ms`, `resume_ms_max` in D1 only)
and leaves paused and starved time out of a machine's rate.

## 4. Desktop API (bearer = admin token, or a Cloudflare Access login)

Rev 6: when the Worker secrets `ACCESS_TEAM_DOMAIN` and `ACCESS_AUD` are set, a valid Access JWT
(RS256 from the team's JWKS, `aud` one of `ACCESS_AUD`, `iss` the team, not expired; with
`ACCESS_EMAILS` set, its `email` or service token `common_name` listed) also counts as the admin,
from the `Cf-Access-Jwt-Assertion` header or the `CF_Authorization` cookie. Access protects only
the `/admin` page; its API calls to `/v1/admin/*` carry the browser's Access cookie. A
cookie-authenticated request must come from that page: a change (any method but GET/HEAD) needs
the header `x-farm-admin: 1` and a same-origin `Origin` and/or `Sec-Fetch-Site: same-origin` (at
least one present, neither saying otherwise); a read is refused when `Sec-Fetch-Site` is present
and not `same-origin` or `none`; refusals are 403 `unauthorized`. The bearer token keeps working
and needs none of this (desktop tools use it). If `/v1/admin` is ever put behind Access, desktop
tools also send an Access service token (`CF-Access-Client-Id`, `CF-Access-Client-Secret`).

| Call | Body → answer |
|---|---|
| `POST /v1/admin/invites` | `{"count": 3, "note": "Sam", "channel": "stable" (default) \| "canary"}` → `{"codes": [...]}` |
| `PUT /v1/admin/artifact/{key}` | bytes (optional `x-sha256`, checked by R2) → `{"ok": true, "key", "size"}` (builds/, ckpt/, rev 6 `defs/<kind>/<id>/<version>/definition.json` and `.minisig`; up to ~100 MB, a Worker request's limit). A registered definition's files can't change: the same bytes again → `"unchanged": true`, other bytes → 409 |
| `POST /v1/admin/artifact/{key}?uploads` · `PUT …?upload_id=&part=N` · `POST …?upload_id=` | multipart upload for bigger artifacts: → `{"upload_id"}`; part bytes (≥ 5 MiB except the last) → `{"part", "etag"}`; `{"parts": [{"part", "etag"}, ...]}` → `{"ok": true, "size"}` |
| `GET /v1/admin/artifact/{key}` | any object (results, diag uploads, builds, checkpoints); `range` as for machines |
| `POST /v1/admin/build` | `{"build_id"}` after its manifest, signature and files are uploaded; the coordinator checks presence + sizes + hashes, and the signature and its trusted comment (`build_id=`, `kind=`) when the Worker var `RELEASE_PUBKEYS` is set → `{"ok": true, "build_id", "kind", "version", "size", "sig_verified"}`; failures → 400 with `"problems": [...]` |
| `POST /v1/admin/agent/release` | `{"version", "build_id", "channel": "canary" \| "stable", "min_agent", "github_url": "<url>" \| null}`: `build_id` must be a registered agent build of that version, `min_agent` ≤ `version`. `github_url` (rev 4): the installer asset of the release's GitHub Release, where `GET /download` redirects; create the GitHub Release first. Rev 6: exactly `https://github.com/<GITHUB_REPO>/releases/download/agent-v<version>/ZeroFarmSetup.exe` (the var defaults to `greggg230/zero-farm`), else 400. `setup` (rev 3) → 400 → `{"ok": true, "channel", "version", "build_id", "min_agent", "github_url"}` |
| `PUT /v1/admin/definition` | rev 6, section 7: `{"kind": "run" \| "campaign", "id", "version", "key", "sha256"}` after `definition.json` and `.minisig` are uploaded to `key` = `defs/<kind>/<id>/<version>/definition.json` → `{"ok": true, "kind", "id", "version", "key", "sha256", "unchanged", "current": true}`; failures → 400 with `"problems"`, a version not above the current one (or the same with other bytes) → 409 |
| `PUT /v1/admin/run/{run}` | `{"game", "build_id", "runtime", "priority": 1.0, "min_agent": "0.2.8" \| null, "alternates": [{"build_id", "runtime", "when": {"vendor", "min_driver"?}, "model"?}] \| null (rev 11, section 9.4; `model`: rev 20, section 14.3), "selfplay": {"quota": 600, "slack": 0.1, "max_stale": 1, "games_per_lease": "auto", "gpu_games_per_lease": 24 | "auto" (rev 11, optional, section 9.4), "gpu": "any" \| "preferred" \| "required", "providers": ["dml", "cpu"] \| null, "needs": {"threads", "ram_mb"}, "job": {...}, "share": 0.25 \| null (rev 13, section 5.3), "rolling": {...} \| null (rev 15, section 10.1)} \| null, "eval": {"batch": "it81", "games": 110, "checkpoints": [81, 70], "games_per_lease": "auto", "gpu", "providers", "needs", "job": {"settings", "seats", "paired"}, "share" (rev 13)} \| null, "state": "active" \| "paused"}` → `{"ok": true, "created"}`. `build_id` must be a registered worker build, `runtime` a runtime build. A new eval `batch` reserves `games` fresh game ids (the old batch's unissued ones are dropped); its rules and checks are in section 5.2 (rev 5: `checkpoints` replaces `gen`, which is now refused). Pausing a run, or dropping its `selfplay`/`eval`, cancels those leases at their next beat. Rev 9: `min_agent` (semver, optional; absent or null: none) - the run's leases (self-play and eval) go only to agents at or above it (section 3's update rule); a PUT is the whole configuration, so a PUT without it removes the limit; leases already issued run on (migration 0007) |
| `GET /v1/admin/run/{run}` | config (rev 9: with `min_agent`) + state: `current_gen`, `next_gid`, `pool`, `selfplay_state` (`cap`, `accepted`, `leased`, `pending`, `in_flight`, `headroom`), `checkpoints` (registered gens, rev 5), `eval_state` (the current batch's progress, section 5.2) and `eval_batches` (every batch of the run, newest first, same fields); rev 6 `definition` (as in stats; `GET /v1/admin/campaign/{id}` too) |
| `PUT /v1/admin/run/{run}/checkpoint/{gen}` | `{"model", "sha256", "reference", "reference_sha256"}` (files under `ckpt/<run>/`, uploaded first); makes `gen` current for self-play and resets that generation's quota counter → `{"ok": true, "unchanged", "current"}`. Rev 5: a `gen` older than the current one is registered without becoming current (`"current": false`: an eval opponent or a head-to-head label); the same `gen` with other files → 409. Rev 6: a gen the current definition already signs must be exactly those files (else 400); self-play with a definition plays the new current gen once a definition signs it. Rev 20: optional `"variants": {"<name>": {"model", "sha256"}}`, which a registered gen may add to (section 14.2; the answer adds `variants`) |
| `GET /v1/admin/run/{run}/results?after={cursor}&limit=500` | → `{"results": [{"id", "key", "lease_id", "seq", "machine_id", "kind", "gen", "games", "ids", "dup_ids", "sha256", "size", "t"}], "cursor"}`. `ids`/`games`: the game ids credited to this chunk; `dup_ids`: ids in it that an earlier chunk was credited with (skip those games). Eval chunks (rev 5) also carry `batch`, `held` and `also`, and their `games` counts `also` too (section 5.2). `limit` ≤ 1000 |
| `POST /v1/admin/run/{run}/accept` | `{"gen", "games": n}`: the trainer's total of games accepted for `gen` (a total, not an increment; include games held over for the next generation). Self-play stops when accepted + in flight (unclaimed ids in active self-play leases + credited self-play games not yet acked) reaches `quota × (1 + slack)`. Call it before `ack` |
| `POST /v1/admin/run/{run}/ack` | `{"cursor"}`: results up to it are ingested (R2 objects deleted after 7 days) → `{"ok": true, "acked"}` |
| `PUT /v1/admin/campaign/{id}` | `{"build_id", "mode", "env": {"FUZZ_POOL": "delver", ...}, "seed_base": 10000000, "chunk": "auto" \| n, "priority": 0.2, "state", "needs": {"threads", "ram_mb"} \| null, "gpu": "any" (default) \| "preferred" \| "required", "providers": [...] \| null, "min_agent": "0.2.8" \| null, "share": 0.5 \| null}` → `{"ok": true, "created", "cursor"}`. The seed cursor never rewinds: a lower `seed_base` is ignored, a higher one moves it up. Rev 7: `gpu` and `providers` (section 8; migration 0006). Rev 9: `min_agent` as for runs (migration 0007). Rev 14: `share` (0 < x <= 1, or null/absent: the backfill; section 5.4; migration 0010); a PUT is the whole configuration, so one without `share` removes it |
| `GET /v1/admin/campaign/{id}` · `GET …/results?after=` · `POST …/ack` | config (rev 14: with `share`) + `cursor`, `seeds_done`, `seeds_leased`, `active_leases`, `pool`, `failures`, `sites`; results and ack as for runs (`ids` are seeds) |
| `GET /v1/admin/campaign/{id}/failures?after=&limit=` | → `{"failures": [{"id", "seed", "mode", "site", "panic", "repro", "output_tail", "build_id", "engine_commit", "machine_id", "lease_id", "first_of_site", "t"}], "cursor"}` (section 5.1; one row per (seed, site)) |
| `GET /v1/admin/machines` · `POST /v1/admin/machine/{id}/revoke` | revoking cancels the machine's active leases at once (their ids go back to the pool) → `{"ok": true, "cancelled"}` |
| `POST /v1/admin/machine/{id}` | `{"name"?, "channel"?, "paused"?, "unblock"?}`; a paused machine gets `wait_s` / `paused_by_admin`; `"unblock": true` (rev 5) lifts the machine's build blocks (`/v1/crash` kind `blocked`) and answers `{"ok": true, "unblocked": n}` |
| `POST /v1/admin/pause` | `{"paused": true \| false}`: farm-wide, no new leases while paused |
| `POST /v1/admin/routing` | rev 18: `{"on": bool, "eval_horizon_s": 0..604800, "eval_games_per_thread": 0..16}`, any of them; the others keep their values → `{"ok": true, "on", "eval_horizon_s", "eval_games_per_thread"}`. `GET /v1/admin/routing` → the same fields and `"stored"` (false: never set, the defaults). Section 12 |
| `POST /v1/admin/evict` | rev 12: `{"builds": ["<build_id>", ...]}` (at most 64; `[]` clears it) replaces the evict list → `{"ok": true, "builds"}` (deduplicated). `GET /v1/admin/evict` → `{"builds"}`. It rides on every `GET /v1/agent/latest` (section 3) |
| `POST /v1/admin/diag` | `{"machine_id" \| "class", "kind": "sweep" \| "profile" \| "stability" \| "logs", "params"}` → `{"requests": [{"id", "machine_id"}]}`. `class` = a hardware class as the stats show it (`"<cpu> / <gpu or 'no gpu'>"`). `logs` goes out as `upload_diag`; the others become `diag` leases (`params.build_id` or `params.run` picks the build) |
| `GET /v1/admin/diag?machine_id=` | requests, uploads and diag-lease results with their R2 keys |
| `GET /v1/admin/stats` | aggregates for the dashboard (`/admin`); per machine also `idle_h_day`, `on_h_day`, `starved_s_24h`, `blocked` (rev 5: `[{"build_id", "until", "os_error", "message", "agent_version", "t"}]`); per run `eval` (the current batch's progress, rev 5); `eval_batches` (the 20 newest batches of all runs, with `run`); `releases` (latest per channel, `github_url`), `diag_uploads` (recent, `user`). Rev 6: `auth` (`{"via": "token" \| "access", "email"?, "common_name"?}`), `limits`, per run and campaign `definition` (current version, gens, jobs, builds and `problems`: why a job can't be leased), per machine `refused_24h`, `blocks` (every active block), `contributors` (farm PCs and external sources, below), `totals` (`farm`, `own`, `all`: self-play/eval games and fuzz seeds over 1 h and 24 h), `by_scope` |
| `POST /v1/admin/external` | rev 6: a reporter's numbers for the owner's machines that don't run the agent (the desktop playing locally, machines over SSH, rented boxes): `{"reporter": "<id>", "t": <unix>, "sources": [{"id", "name", "kind": "local" \| "ssh" \| "vast" \| "other", "hardware"?, "working_on": ["run:<run>", "campaign:<id>", ...], "selfplay_games": {"1h", "24h"}, "eval_games": {"1h", "24h"}, "fuzz_seeds": {"1h", "24h"}, "busy_h_24h": x \| null}]}` (at most 64 sources, 64 KB) → `{"ok": true, "reporter", "sources": n}`. Each POST replaces all sources of its reporter; a source whose last report is over 30 min old shows offline; rows are deleted 7 days after it. Admin page only (`contributors` in stats), never on a public page |

## 5. Jobs (`lease.job`) and the agent↔worker protocol (`aw/1`)

The agent starts `<entry> serve` from the verified build directory with the working directory set
to a fresh `work/<lease_id>/`, sends one `job` line, and reads events. stdin/stdout carry JSON
lines only (one object per line); stderr is a log the agent keeps (rotated) and never parses.

Agent → worker:

| Line | Meaning |
|---|---|
| `{"op": "job", "lease_id", "kind", ...Job, "model": "<local path>" \| null, "reference": "<local path>" \| null, "threads": 6, "out_dir": "out", "runtime_dir": "<local path>" \| null}` | start. Optional worker tuning (set by the agent's autotuning, never by the run): `conc`, `groups`, `infer_threads`, `spinning`, rev 19 `infer_workers` (ONNX Runtime sessions, each with `infer_threads` threads; a worker that honours it lists the feature `infer_workers` in `hello`, macOS builds always do; section 13); also `gen` and `sha256` of the model (else `gen` comes from the graph's metadata), `prefix` for shard names. Eval (rev 5): `models` instead of `model`/`reference` (both null), section 5.2 |
| `{"op": "checkpoint", "gen", "model", "reference", "sha256"?}` | self-play: a phase change, as a remote worker's today: games in flight continue, their searches restart on the new network, and each row records the network that searched it (`net`) |
| `{"op": "yield"}` | stop starting batches, drop the ONNX session (free VRAM), answer `yielded` |
| `{"op": "resume"}` | rebuild the session, carry on |
| `{"op": "finish"}` | start no new games, finish the ones in flight, flush, `end` |
| `{"op": "quit"}` | stop now: flush games already finished (announce their `shard`/`records`), then `end`; games in flight are lost. May arrive before `job` |
| `{"op": "ping"}` | answer `status` |

Worker → agent (`"ev"`):

| Event | Fields |
|---|---|
| `hello` | `aw: 1`, `build_id`, `game`, `kinds`, `zero_onnx: [1]`, `pid` |
| `selftest` | `provider`, `max_abs`, `ok` (before the first real batch with a model); `gen` (rev 5): the network it tested, one event per gen of an eval job's `models` |
| `status` | `stats`: Stats (section 3); its `provider` is `""` until the worker has built its sessions (section 8, **Which provider a job uses**) |
| `fin` | `n`: games finished since the last `fin` |
| `shard` / `records` | `seq`, `path` (relative, under out_dir), `games`, `rows`, `bytes`, `gids`, `gen`? — a result file ready to upload (`shard` until phase 3, then `records`: rev 8, below); the agent moves the file away and sends `gids` as `x-ids`, `gen` (the oldest checkpoint generation of its games) as `x-gen` |
| `evals` | eval (rev 5): `seq`, `path`, `games`, `gids` (as `shard`, no `gen`): a file of eval rows (section 5.2); the agent uploads it with `gids` as `x-ids` |
| `failure` | fuzz: `seed` (integer), `mode`, `panic`, `site` (string), `repro`, `output_tail` |
| `yielded` · `resumed` | |
| `end` | `reason`: `done` \| `finish` \| `quit`, `stats`: Stats, `unfinished`: `{"gids": ranges}` \| `{"seeds": ranges}` \| null |
| `error` | `message`, `fatal: true \| false` |

Timing rules: `hello` within 10 s of start (60 s the first time a build runs on a PC: antivirus
scans a newly downloaded executable before it starts); `yielded` within 2 s of `yield` (else the
agent kills the process); `status` at least every 30 s while running (the agent pings after 30 s of
silence and kills a worker silent for 90 s).

The worker reports the `build_id` of the `manifest.json` next to its executable (first 24 hex of its
sha256); a mismatch with the lease is fatal. Paths in `job`/`checkpoint` lines are absolute.
`runtime_dir`, when set, is a verified runtime pack; the agent also puts it first on the worker's
`PATH` and in `ZEROFARM_RUNTIME_DIR`. Lines are at most 1 MB. When the agent sends `x-gen` and the
worker gave no `gen`, it uses the generation in effect when the chunk's games started (after a
`checkpoint` switch, the old one until the next chunk). The worker runs in a job object (killed with
the agent, memory-capped), at IDLE_PRIORITY_CLASS with EcoQoS for CPU work, suspended
(NtSuspendProcess) while yielded.

Job kinds:

```json
{"kind": "fuzz", "mode": "delver-games", "env": {"FUZZ_POOL": "delver", ...}, "first": 10450000, "count": 20000}
{"kind": "selfplay", "settings": {...run settings the worker needs...}, "games": 40,
 "gid_base": 1200, "seed": 7919001}
{"kind": "eval", "settings": {...}, "seats": [...], "games": 40, "gid_base": 0, "seed": 31, "paired": true}
{"kind": "diag", "what": "sweep" | "profile" | "stability", "params": {...}}
```

For self-play and eval the coordinator copies the run's `selfplay.job` / `eval.job` object and sets
`kind`, `games`, `gid_base` and `seed`. `seed` is the run's base seed (`job.seed` if the run sets
one, else derived from the run name), the same for every lease of the run: workers derive each
game's seed from (`seed`, gid), so a game id replayed after a dropout is the same game. The game ids
are `[gid_base, gid_base + games)`; the agent passes the worker's `gids` for each result file on as
that upload's `x-ids`.

### 5.1 Fuzz results
Modes are tools/fuzz_farm.py's names or `<name>-games|-matches|-events`; check a mode against a
build with `mtgz-worker fuzz-check --mode M --env K=V` before creating a campaign. The **agent**
builds and uploads the end-of-lease summary below; the worker writes its own copy but announces
it only when the job sets `announce_result: true` (else each PC's fuzz rate would count twice).
For fuzz-profile builds `repro` says `--profile fuzz` instead of `--release` (debug assertions
don't reproduce under `--release`).
A fuzz lease uploads one JSON result at the end (and one per failure as it happens):
`{"campaign", "mode", "first", "count", "done", "failures": [{"seed", "panic", "site", "repro",
"output_tail"}], "build_id", "engine_commit"}`. `repro` is the string mtg-zero's
`tools/fuzz_farm.py` writes today, `site` its dedupe key (panic location). The final result's
upload carries `x-ids` with the seeds it covered (`[[first, first + done]]` when they run in order);
per-failure uploads carry none. Each failure needs an integer `seed` and a string `site`. The agent
builds these from the worker's `failure` events and its `end`, adding `"final": false` (per failure,
`done: null`, `failures: [that one]`) or `"final": true` (the end-of-lease summary); a worker failure
without a `site` gets the `file:line:col` after "panicked at" in its panic text, else `"unknown"`.

### 5.2 Evaluation and head-to-heads (rev 5)

An eval batch plays fixed networks against each other (or against a built-in bot) and reports one
row per game; the desktop computes the score, its error and an Elo difference. Checkpoints never go
stale, so eval leases ignore `max_stale` (and `x-gen`).

- **Batch** (desktop): `PUT /v1/admin/run/{run}` with `"eval": {"batch", "games", "checkpoints":
  [gen, ...], "games_per_lease", "gpu", "needs", "job": {"settings": {...}, "seats": [Seat, Seat],
  "paired": true}}`. Every gen in `checkpoints` must be registered on the run (`PUT .../checkpoint/
  {gen}`, which needs the run to exist: a new run is put with `"eval": null` first, then its
  checkpoints, then the batch). Gens may be registered in any order: one older than the run's
  current gen is registered without becoming current. An ad hoc head-to-head is a run of its own
  (e.g. `h2h-eoe1-80v70`, `"selfplay": null`) whose gens are just labels for the two networks.
- **Checks** (400 unless noted): `checkpoints` is a list of at most 16 distinct registered gens;
  `job.seats` is a list of 2 to 16 Seats; the set of gens the seats name equals `checkpoints`;
  `ext:` bots are refused; `paired` is a boolean and `true` needs an even `games`; `gen` (rev 2's
  single eval network) is refused. A batch can't change: putting the same `batch` again with other
  `games`, `checkpoints` (order included) or `job` → 409 (re-putting it unchanged, or changing
  `games_per_lease`, `gpu`, `needs`, the run's `priority` or `state`, is fine), and a batch name
  the run used before → 409.
- **Seat**: `{"net": <gen>}` or `{"bot": "<name>"}` (a built-in bot of the game; a name of up to 64
  characters from `A-Za-z0-9_.:-`), exactly one of the two; other keys are passed to the worker as
  they are.
- **Paired games**: with `"paired": true` game ids come in pairs `(2k, 2k+1)`: the same deal with
  the seats swapped. A paired batch starts on an even id (an odd id skipped to get there goes to the
  run's self-play pool). Leases always carry whole pairs (`gid_base` and `games` even). Seat order
  for game g: `seats` for even g, reversed for odd g.
- **Lease**: `checkpoints` lists every network the seats name (`gen`, `model`, `sha256`,
  `reference`, `reference_sha256`), in the batch's order; `checkpoint` is null. Only agents 0.2.3
  or newer get eval leases (section 3). The agent downloads and verifies every checkpoint (and its
  reference) into its cache before it starts the worker, and lists them in `cached.ckpts`.
- **Agent → worker** (`job` line): `models: {"<gen>": {"model": path, "sha256", "reference": path |
  null}}` for every listed checkpoint; `model` and `reference` are null. The worker runs the
  self-test (`selftest` event with `gen`, one per gen) before its first game and refuses to play
  with a network that fails it (a fatal `error`). The agent reports the per-gen results in Stats
  (`selftest`, section 3) and, when one fails, in its `crash` report.
- **Results**: the worker writes JSON lines, one per game: `{"gid", "pair", "seats": [Seat, ...],
  "result": [r_seat0, ...], "margin", "rounds", "over", "net_ms"?}` (`result`: 1 win, 0.5 draw,
  0 loss per seat, in seat order of that game). It announces each file with the event `evals`
  (same fields as `shard`: `seq`, `path`, `games`, `gids`). The agent uploads it like any result,
  `gids` as `x-ids`. A worker should keep both games of a pair in one file when it can (it saves
  the desktop a lookup), but doesn't have to.
- **Crediting** (paired batches): a pair is credited only when both of its games are in, from one
  lease. Each eval chunk's reported ids split into `ids` (credited; their rows are in this chunk),
  `dup_ids` (credited to another chunk before) and `held` (one game of a pair whose other game this
  lease hasn't reported yet: not credited). When a later chunk of the same lease brings the other
  game, that chunk credits the whole pair: its own game in `ids`, the held one in `also: [{"seq",
  "key", "ids"}]`, naming the earlier chunk whose row it is. A lease that ends with held games
  leaves those pairs unclaimed, so they go back to the pool and are played again in full (a late
  chunk of that lease can still complete them, if no other lease has). The upload answer adds
  `held` (games of this chunk now held). Unpaired batches credit game by game, as self-play does.
- **Listing**: `GET /v1/admin/run/{run}/results` lists eval chunks with `"kind": "eval"`, `batch`,
  `held` and `also`; `games` = credited ids in `ids` plus those in `also`. To read a batch: for
  every listed chunk of it, take the rows of `key` whose `gid` is in `ids`, plus, for each `also`
  entry, the rows of its `key` whose `gid` is in its `ids`. That is every credited game exactly
  once, with both games of every credited pair.
- **Progress**: `GET /v1/admin/run/{run}` → `eval_state` = `{"batch", "games", "paired",
  "checkpoints", "lo", "hi", "done", "leased", "unissued", "created_at", "replaced_at"}` for the
  current batch (`done`: credited games; `leased`: ids in active leases not credited yet;
  `unissued`: in the pool; game ids `[lo, hi)`), and `eval_batches` (newest first, up to 50). The
  batch is complete when `done` = `games`. The coordinator never reads rows, so it can't show a
  score.
- **Semantics** must equal zero-core's `zero.h2h` / `Run.evaluate` at the same settings (eval sims,
  seat order, the deal shared by a pair); the worker's gate checks native rows against the Python
  path running the same ONNX graphs, byte for byte.

### 5.3 Play jobs with traces: analysis campaigns, background shares (rev 13)

One job model for "play N games with these networks under these settings over game ids [0, N) and return every game"
(the owner's 2026-11-15 architecture review: self-play, evaluation, yardsticks, census and analyses are all instances
of it). A **play job** = a worker build, networks by sha256, the play settings, seats, a base seed, ids `[0, N)` ->
one traced game record and its digest per id, positive coverage (every id done or failed with a reason), a spot
replay on the desktop. Options: `trace` (the moves come back) and `samples` (the search's policy targets and values
too). On the wire it reuses the two existing job kinds, so **agents need no change** (any agent that takes eval or
self-play leases runs it; the agent forwards the template's `settings` as they are):

| play job | wire form | rows / files |
|---|---|---|
| without samples (evaluation seats: a head-to-head, a run's yardstick, analysis games) | an eval batch (5.2) of a run of its own, its settings with `eval_trace: 1` (zero-core docs/az.md 50) | eval rows + `digest` + `rec` (the game's record: seed, every real move, result, margins, rounds, setup) |
| with samples (self-play seats: noise, temperature, playout cap - what a training game is) | the self-play of a run of its own: `quota` = N, `slack` 0, its one checkpoint current; the desktop posts `accept` = the games it took | the records files of rev 8 |

- **The run.** `cmp-<name>`, `selfplay` null (or the samples job), its own signed definition (7), gens 1 (A) and
  2 (B, absent for self-play). The batch's job carries `seed`, the base seed of every game (`baseSeed`: the job's
  `seed` wins over the name), so a campaign plays exactly the games a local match of that seed plays: game g's seed is
  `game_seed(seed, g, paired)`.
- **Background share** (rev 13, coordinator only; migration none): a run's `selfplay` or `eval` may carry
  `"share": x` (0 < x <= 1; absent or null: none, and the stored config has no such key). Within a tier, such a job
  goes first only while its own active leases + 1 are at most `x` times (all runs' active self-play and eval leases
  + 1); otherwise it goes after every other run job. The lease loop takes the first candidate that grants, so a
  background job still gets every machine the other runs have nothing for (a self-play quota filled, a paused run,
  no run): it never idles a PC, and the default 0.25 never starves a live run's farm self-play (1 machine in 4 at
  most while both want work). Without `share` nothing changes: evaluation still goes before self-play (rev 7).
  `share` is not part of a batch's identity (it may change like `games_per_lease`). Lease counts measure machine time
  because leases are sized to ~7.5 minutes of each machine (5.1).
- **Trust.** Each traced row carries `digest` = SHA-256 of its record's sorted-key compact JSON (16 hex); the desktop
  checks every row's digest against its record, then replays a random ~5% of the games (seeded by the campaign's
  name) through the engine from seed + moves (every move legal, the result, margins, rounds and setup the record's);
  any mismatch refuses the whole campaign. A lease's (or campaign's) games have one digest: SHA-256 over `gid:digest`
  lines in gid order. This catches bad hardware and transport faults (bad RAM, a broken build) - the realistic threat
  with invited friends; it proves the moves are legal and the results theirs, not that the network chose them (a
  duplicate-lease audit would, later).
- **Coverage and fallback.** The coordinator's reissue rules are unchanged (a lease not heard from is reissued). The
  desktop collector gives every campaign a deadline: game ids the farm has not delivered by then are played on the
  owner's machine by the same signed worker executable on the same graphs (the same games), the campaign's run is
  paused (its leases cancelled at their next beat), and a piece that fails there marks its games failed with the
  worker's error. Without local fallback the campaign fails loudly, naming the missing ranges.
- **Desktop.** MTG Zero's `tools/farm_campaign.py submit|status|collect` and library `mtgz.az.farm_campaign`
  (its docs/ENGINE.md 165).

### 5.4 A fuzz campaign's share: fuzz ahead of a live run's self-play while it is behind (rev 14)

By default a fuzz campaign is the **backfill** (section 3: after every run's evaluation and self-play), so while a live
run's farm self-play has games to give - always, at the farm's size, since the pool never holds a share of a
generation's quota - fuzz gets no machine. The owner's 2026-11-15 decision: continuous fuzz of 50,000 seeds a day for
main's build and each live run's pinned build, on the farm first, and the farm's side-work cap (~25% of the farm
while a run is live; idle machines always) **may be exceeded while a build's fuzz is behind its daily pace**. So a
campaign takes an optional `"share": x` (0 < x <= 1; absent or null: the backfill, unchanged):

- with a share the campaign's candidate is in tier 1 beside the runs' games, and goes first while its own active
  leases + 1 are at most `x` times (all runs' active self-play and eval leases + its own active leases + 1) - the
  same rule as rev 13's background jobs, with the campaign's own leases added to the total because they are not run
  leases; beyond its share it goes after every other run job (still before the tier-2 backfill), so it never idles a
  PC and never takes more than its share from a live run that has games to give;
- `share` 0.25 is the side-work share; the steward uses more (e.g. 0.5: every other machine) only while that build's
  fuzz is behind its pace, and removes the share once it is back on pace (the campaign is the backfill again);
- several campaigns with a share each count only their own leases: the desktop keeps their number small (the MTG Zero
  steward boosts at most one campaign per build at a time).

The share is not signed (like `priority`), and changes no job: the same seeds, mode and env. Coordinator only
(migration 0010 adds the column); agents need no change. The desktop side: MTG Zero's `tools/fuzz_steward.py`
(its docs/ENGINE.md).

## 6. Agent install layout, launcher and self-update

```
%LOCALAPPDATA%\ZeroFarm\
  ZeroFarm.exe                 launcher (HKCU\...\Run "ZeroFarm" points here)
  versions\<version>\farm-agent.exe (+ files of the agent manifest)
  state\current  state\previous   one version string each
  state\pending                  version to switch to (written by the agent)
  state\healthy-<version>        written by an agent after its first successful coordinator call
  state\bad-<version>            written by the launcher on rollback; agents skip it for 24 h
  state\launcher-events.jsonl    the launcher's rollback/refusal notes, sent by the next agent
  state\launcher-swap            {"old", "sha256", "from", "to", "t"}: a launcher replacement in progress
  state\launcher-ok-<pid>        written by a launcher started with --takeover once it is in charge
  state\launcher-bad-<sha16>     a launcher that failed its selftest or was rolled back; not tried for 24 h
  state\availability.json        hours on / allowed to work per local day (last 14 days)
  ZeroFarm.old-<n>.exe ZeroFarm.bad-<n>.exe ZeroFarm.new.exe   launcher replacement leftovers
  state\machine.json             machine_id, token (DPAPI, CurrentUser), settings
  state\crashes\                 agent panic reports not yet sent
  cache\builds\<build_id>\ cache\ckpt\<run>\<gen>\   (LRU under the disk cap; cache\tmp\ staging)
  results\pending\<lease_id>\     lease.json, <seq> + <seq>.meta.json until uploaded, done.json
  work\<lease_id>\                the worker's working directory (out\ = out_dir)
  logs\  (rotating, 20 MB: agent.log, worker.log, launcher.log)
```

Launcher loop: start `versions\<current>\farm-agent.exe`; when it exits with code 75 and
`state\pending` exists → verify that version's manifest + files (same key list), set previous :=
current, current := pending, start it; if `healthy-<new>` doesn't appear within 120 s or the new
agent exits non-zero 3 times in 10 min → current := previous, report via the old agent's next
`/v1/crash` (`kind: "agent"`). The launcher never downloads; only agents do. Last two versions kept.
Details:
- The agent is started as `farm-agent.exe run --root <root>`. `current` is verified before every
  start (not only at a switch); if it fails and `previous` verifies, the launcher goes back to
  `previous` and notes it. Nothing verifiable → it exits.
- On a switch the new version's old `healthy-<v>` marker (if any) is deleted first, so probation
  always applies. A rollback writes `state\bad-<v>`, clears `previous` and appends a note to
  `state\launcher-events.jsonl`; a pending version that fails verification is refused the same way.
- Exit code 0 from the agent means the user chose Quit: the launcher exits too. Other non-zero
  exits restart the agent with backoff (1 s, 2 s, 4 s … 10 min); the 3-crash rollback needs a
  verifiable `previous`.
- One launcher and one agent per root (named mutexes); a named "quit" event per root asks both to
  exit (uninstall and re-install use it). The agent runs in a kill-on-close job of the launcher.
- `ZeroFarm.exe uninstall [args]` forwards to `versions\<current>\farm-agent.exe uninstall`.
- An agent that gets 426 at enroll cannot update itself (`/v1/agent/latest` and `/v1/artifact` need
  a machine token): it tells the friend to download a newer installer (from the answer's
  `download` link); the invite stays unused.
- The launcher starts the agent with `ZEROFARM_LAUNCHER=2` (its launcher protocol level; launchers
  before 0.2.0 set nothing). `ZeroFarm.exe selftest` exits 0 (printing `ok <version>`) if it
  trusts a release key and `versions\<current>` verifies, else 2. Leftover `ZeroFarm.old-*.exe`,
  `ZeroFarm.bad-*.exe` and `ZeroFarm.new.exe` are deleted when a launcher starts.

Launcher update (rev 3; the launcher's own version is its VERSIONINFO file version):
1. The agent that is `current` and `healthy-<v>` checks, once per run, whether its package's
   `ZeroFarm.exe` is newer than the installed `<root>\ZeroFarm.exe` (one without VERSIONINFO
   counts as 0.0.0; equal or older is never installed, so a rolled-back agent version can't undo a
   newer launcher) and not marked `launcher-bad-<sha16>` within 24 h.
2. It copies it to `ZeroFarm.new.exe`, checks its sha256 against the package manifest, and runs
   `ZeroFarm.new.exe selftest` (60 s). Failure → `launcher-bad-<sha16>`, a `/v1/crash`
   (`kind: "agent"`), nothing replaced.
3. It renames the running launcher to `ZeroFarm.old-<n>.exe` (a running exe can be renamed) and
   `ZeroFarm.new.exe` to `ZeroFarm.exe`.
4. With `ZEROFARM_LAUNCHER` ≥ 2: it writes `state\launcher-swap` and exits 76. Otherwise (an
   older launcher, which doesn't know 76) it keeps running; the new launcher starts at the next
   logon.
5. The launcher, on 76 with `state\launcher-swap`: releases its single-instance mutex, starts
   `ZeroFarm.exe --takeover <its pid>` (detached, out of its job when allowed) and waits up to 60 s
   for `state\launcher-ok-<new pid>`. The new launcher takes the mutex (waiting up to 30 s), runs
   as usual and writes `launcher-ok-<pid>` once its first agent has run for 10 s (or exited 0).
   Then the old launcher deletes `launcher-swap` and exits; the new one deletes
   `ZeroFarm.old-<n>.exe` once that file is free.
6. Rollback: if the new launcher can't be started, exits before confirming, or doesn't confirm
   within 60 s (it is killed, and its agent with it), the old launcher renames `ZeroFarm.exe` to
   `ZeroFarm.bad-<n>.exe` and itself back to `ZeroFarm.exe`, writes `launcher-bad-<sha16>`,
   appends a `launcher_rollback` note to `launcher-events.jsonl` (reported by the next agent),
   takes the mutex back and restarts the agent. The quit event during a handover ends both.

Agent update: on `{"update": ...}` from `/v1/lease`, or `GET /v1/agent/latest` every 6 h, newer than
running → download + verify into `versions\<new>\` → wait until no lease is running (or give the
current one back if `min_agent` forces it) → write `state\pending` → exit 75.

Agent exit codes: 0 the user quit, 75 switch to `state\pending`, 76 hand over to the new
`ZeroFarm.exe` (rev 3), anything else a crash (restart with backoff, 3 in 10 min → rollback).

## 7. Signed definitions (rev 6)

Everything a lease tells a friend's PC to download or run is fixed by a **definition** signed on the
desktop with a **data key**. The coordinator only adds a few per-lease fields. The coordinator never
holds a data key, so a compromised coordinator (or admin token) can withhold work but can no longer
choose what a PC's worker reads (DESIGN 4; security review 2026-11-12, findings H2, H3, M2).

**Keys.** Data keys are minisign keys like release keys, but in a separate trusted list
(`agent/common/trusted_data_keys.txt`, plus the build-time variable `ZEROFARM_TRUSTED_DATA_KEYS`).
A data key can only sign definitions; a release key can only sign manifests. Each check accepts only
its own list. The first data key is `RWTImfGffZDbH0xWVxO/H563BYqds0G+D+hJLn79XEo9xU3sbl6lLp9D` (id 1FDB907D9FF199C8).

**Definition** (`definition/1`, the bytes of `definition.json`, signed as
`definition.json.minisig`):

```json
{
  "definition": 1,
  "kind": "run" | "campaign",
  "id": "eoe2",                       // the run or campaign slug
  "version": 7,                       // strictly increasing per (kind, id)
  "created": 1794500000,
  "expires": 1795100000 | null,       // agents refuse it after this time
  "game": "mtg",
  "builds": ["<worker build_id>", ...],             // workers allowed to run it
  "runtime": "<runtime build_id>" | null,           // the runtime pack those builds load
  "checkpoints": {"<gen>": {"model_sha256": "...", "model_size": 13421772,
                            "reference_sha256": "...", "reference_size": 412000,
                            "variants": {"<name>": {"model_sha256", "model_size"}}?}},   // rev 20, section 14.1
  "jobs": {                           // exact job templates; only per-lease fields are added
    "selfplay": {"settings": {...}} | null,
    "eval": {"settings": {...}, "seats": [Seat, ...], "paired": true} | null,
    "fuzz": {"mode": "delver-games", "env": {"FUZZ_POOL": "delver"}} | null
  }
}
```

**Per-lease fields the coordinator may set** (everything else comes from the definition):
self-play/eval `gid_base`, `games`, `seed`, plus which signed checkpoint gens the lease uses;
fuzz `first`, `count`.

**Lease.** Every lease for a run or campaign carries `"definition": {"key":
"defs/<kind>/<id>/<version>/definition.json", "sha256": "..."}`, with the signature next to it
(`.minisig`). The **agent**:
1. downloads both through `/v1/artifact`;
2. verifies the signature against the trusted data keys;
3. checks `kind`/`id` against the lease's run/campaign, `expires`, and that `version` is at least
   the highest it has seen for that (kind, id) (stored in its state, so no rollback);
4. checks the lease's worker build is in `builds` and its runtime equals `runtime` (rev 11, agents from
   0.2.10: or equals the `requires_runtime` of the lease's worker build, from that build's signed manifest: a
   definition authorizes each build it lists with the pack that build requires; section 9), and that every
   checkpoint file's size and sha256 equal the definition's;
5. **builds the job line itself** from the definition's job template, plus only the per-lease fields
   above, plus its own local paths (`models`, `model`, `reference`, `runtime_dir`, `out_dir`) and
   tuning. It forwards **no other coordinator-supplied key**.
A lease that fails any check is refused (done with `refused: "definition"`, reported once with crash
kind `definition`), and the agent waits before asking again.

**Coordinator.** `PUT /v1/admin/definition` registers an uploaded definition (`{"kind", "id",
"version", "key", "sha256"}`), verifies its signature against the `DATA_PUBKEYS` var, and makes it
current for that run/campaign. Leases are issued only for a run or campaign with a current
definition, and only with builds, runtimes, checkpoints and job kinds it names. The run/campaign
`job` objects the coordinator keeps are no longer sent to agents.

Coordinator details (from building it, rev 6):
- **Registering.** `key` must be `defs/<kind>/<id>/<version>/definition.json` (the `.minisig` next to
  it, at most 4 KB; the definition at most 1 MiB) and its bytes must hash to `sha256`. The signature
  is checked against `DATA_PUBKEYS` only (a release key's signature is refused); no data key set →
  400. The contents get the agent's checks (`definition/1`: no unknown fields; slugs; `created` > 0
  and `expires` after it; 1 to 64 worker builds; at most 1024 checkpoints, files of 1 byte to
  1 GiB, `reference_sha256` and `reference_size` together; templates with only their kind's keys:
  self-play `settings`, eval `settings`, `seats`, `paired` (required), fuzz `mode`, `env`,
  `announce_result`; no per-lease fields such as `seed`; no path-like strings or keys (rev 8: a `\` or `/`
  - the Oracle name separator ` // ` of a split, adventure or double-faced card's name excepted, as a game spec
  names such cards -, `..`, or a drive prefix `X:`); eval seats
  name only listed gens; a run defines self-play and eval, a campaign fuzz) plus the
  coordinator's: the body names the requested kind, id and version, it hasn't expired, its builds
  are registered worker builds of its `game`, its runtime a registered runtime, and gens already
  registered on the run match it. The trusted comment (`definition=1 kind=… id=… version=…` from
  `farm-release define`) is not checked: the signed bytes are what count. A version must be
  higher than the current one; re-registering the current one with the same bytes answers
  `"unchanged": true`. Once registered, its files can't be replaced.
- **Leasing.** A lease uses the current (highest) version, and only if it hasn't expired, names the
  run's (or campaign's) build and runtime (a campaign's runtime is null; rev 11: or, for agents from 0.2.10, one
  of the run's alternates, whose runtime is its build's `requires_runtime`, section 9), has a template for the
  job kind, signs the checkpoint files exactly as registered (self-play: the current gen; eval:
  every network of the batch, which must be exactly the gens the signed seats name, and the
  batch's pairing must equal the template's `paired`). Otherwise that job gets no lease (the admin
  page says why). The lease's `job` is `{"kind", "gid_base", "games", "seed"}` (self-play, eval)
  or `{"kind", "first", "count"}` (fuzz), `kind` = the lease's kind; `seed` is the run's base
  seed (its job's `seed`, else from its name), the same as on legacy leases. Paired eval leases
  keep whole pairs; games stay within 1 to 100,000, seeds per lease within 1 to 10^8, ids below
  2^53.
- **Rollout.** `DEFINITION_AGENT` (var, default `0.2.5`) is the first definition-aware agent. An
  older agent gets legacy leases (no `definition`, the coordinator's job object) only from a run or
  campaign that has no definition yet; once it has one, the older agent gets `{"update"}` if its
  channel offers `DEFINITION_AGENT`, else nothing from there. A definition-aware agent gets nothing
  from a run or campaign without a definition, and no diag leases (they carry admin-chosen
  `params` and no definition); logs requests (`upload_diag`) are unaffected.

**Desktop.** `farm-release define <definition.json> --key <data key> [--publish --url --token]`
signs, uploads and registers a definition. `farm_h2h`, campaign creation and the trainer's pool
remote produce a new version whenever builds, checkpoints or job settings change; self-play adds
each new checkpoint gen.

**Rollout.** The coordinator keeps serving legacy leases (without `definition`) only to agents
older than the first definition-aware agent, until that agent is `min_agent` on both channels. After
that, a lease without a definition is never issued, and a definition-aware agent never runs one.

## 8. GPU inference contract (rev 7, phases 3-4)

One worker-side function creates every inference session; the GPU work and the self-play work
both build on it, and nothing else in a worker creates an ONNX Runtime session.

**Worker API** (`zero_worker::session`, Rust; owned by the GPU work; zero-core `gpu` 3727b3b):

```rust
pub enum ProviderKind { Cpu, TensorRtRtx, Cuda, MiGraphX, AmdGpu, OpenVino, DirectMl }
pub struct GpuDevice { pub luid: u64, pub vendor: String, pub name: String }
pub struct SessionSpec<'a> {
    pub model: &'a Path,               // a zero-onnx/1 graph
    pub reference: Option<&'a Path>,   // reference.npz; required to accept any GPU provider
    pub providers: &'a [ProviderKind], // preference order from the job line; Cpu always last
    pub device: Option<GpuDevice>,     // the adapter to use (job line `gpu`); None: the first it serves
    pub intra_threads: usize,
    pub runtime: &'a Path,             // the runtime pack's onnxruntime.dll
    pub spinning: bool,
    pub expect: Option<&'a Expect>,    // the job's game (None: the graph's own metadata; tools)
}
pub struct SelfTest { pub provider: ProviderKind, pub max_abs: f32, pub ok: bool,
                      pub cpu_fallback_nodes: Vec<String>, pub deterministic: bool, pub rows: usize,
                      pub items: [f64; 4], pub device: Option<GpuDevice>, pub build_s: f64, pub pass_s: [f64; 2], .. }
pub fn create_session(spec: &SessionSpec) -> Result<InferenceSession, SessionError>;
pub fn try_one(rt, spec, reference, p: ProviderKind, want: Option<&GpuDevice>, adapters) -> Tried; // one provider (the probe)
impl InferenceSession {
    pub fn provider(&self) -> ProviderKind;
    pub fn selftest(&self) -> &SelfTest;
    pub fn attempts(&self) -> &[Attempt];   // providers tried before it, and why each was not used
    pub fn eval(&mut self, batch: &LeafBatch) -> Result<Answer, SessionError>; // zero-onnx/1 in, the
                                            // generic post-processed answer out (section 5)
    pub fn release_device(&mut self);       // on `yield`: free all GPU memory; rebuilt on the next eval
}
// InferenceSession also implements the worker's Evaluator (release = release_device; `provider()` is the
// Stats `provider` the agent reads: "cpu", "dml", ...).
```

`create_session` tries `providers` in order. A provider is accepted only if its session builds and
the self-test on `reference` passes: max abs difference at most 2e-5 against the reference answers
on every output (priors after the generic step, value, margin, aux), the whole set answered twice and
bit-identical. A GPU provider is never accepted without a reference set, and one to which ONNX
Runtime gives no node of the graph is refused. Nodes the provider leaves to the CPU are listed in
`cpu_fallback_nodes` ("OpType (name)", from ONNX Runtime's verbose node placement while the session is
built). Otherwise it moves to the next provider, ending with `Cpu`; a CPU session that fails the
self-test ends the job, as before. GPU sessions run with deterministic compute on (and CUDA's TF32
off): a provider that drops below float32 fails the self-test. The worker emits one `selftest` event
per model with `gen`, `provider`, `max_abs`, `ok`, `cpu_fallback_nodes`, plus `deterministic`, `rows`,
`items`, `gpu`, `build_s`, `pass_s` and `tried` (the providers not used, with why). On the CPU a batch
over 64 rows is answered in 64-row pieces (section 5); a GPU provider takes whole batches (pieces of
at most 1,024 rows; TensorRT-RTX also at most 1,048,576 non-zeros, its optimisation profile's maximum).
A row's answer depends on its piece only by float rounding, and each provider passes its self-test
with its own pieces.

**How each provider is bound** (zero-core `session.rs`): `dml` - built into the runtime pack's ONNX
Runtime, on the adapter's DXGI index or ONNX Runtime's EP device with the adapter's LUID; `cuda` and a
standalone `tensorrt_rtx` - compiled into the pack's ONNX Runtime build, device ordinal = the adapter's
index among its vendor's; `tensorrt_rtx`, `amdgpu`, `migraphx`, `openvino` from Windows ML's catalog -
plugin libraries registered with ONNX Runtime and bound to the ONNX Runtime EP device whose `LUID`
metadata is the adapter's. TensorRT-RTX gets an explicit optimisation profile (min 1 row, opt 256 rows /
65,536 non-zeros, max 1,024 rows / 1,048,576 non-zeros) and `nv_op_types_to_exclude=NonZero` where the
provider takes that option.

**Job line (aw/1 additions):** `providers: ["dml", "cpu"]` (what the agent allows, in order, from its
verified provider list, the lease's allowlist and the user's "Use the GPU" setting; unknown names are
skipped, `cpu` is added last) and `gpu: {"luid": "<hex>", "vendor", "name"} | null`. A job line
without `providers` means `["cpu"]`. The worker's CLI has `--providers a,b` and `--gpu <luid>`.

**Which provider a job uses** (what the agent's pauses go by: a GPU job yields when GPU work stops being allowed, a
CPU job only when work does). Stats `provider` and `selftest`'s name the provider the worker's sessions use; before
its sessions exist (it builds them after the job line, which can take a while) a worker reports `""`. Agents from
0.3.2 take `""` as not known yet: until the worker names a provider, the job is what the agent chose for it - a GPU
job when its job line's `providers` names a GPU provider, else a CPU job -, and from then on the one it names (a GPU
job whose worker fell back to the CPU is a CPU job). 0.3.1 took `""` for a GPU provider: on a machine where GPU work
isn't allowed (every Mac) it yielded a CPU job's worker as soon as a `status` came before the sessions - at once with
a crew of workers (13.5), whose main worker is pinged right after its start - and the worker stayed suspended, its
sessions never built, until the coordinator took the lease back.

**`probe-providers`** (`<worker> probe-providers [--runtime-dir D] [--model M --reference R]
[--providers a,b] [--gpu LUID] [--threads T] [--reps N] [--bench-rows 64,256,...] [--ensure-ready]
[--json FILE]`): the CPU first, then every candidate GPU provider on every adapter it serves, each
through the factory's own steps (`try_one`: build, self-test, node placement) and a timing; the graph is
built in (a small token network with every operator of the real networks' graphs, and its reference
set) unless `--model`/`--reference` name a real one. `--ensure-ready` lets Windows ML download and
install the catalog providers this PC's adapters could use (a job never does). Output: one JSON object
(`probe` 1, `build_id`, `ort`, `available`, `modules`, `adapters` [{luid, vendor, vendor_id, device_id,
name, vram_mb, dxgi_index}], `catalog` {present, eps [{name, version, ready, certified, ensure}],
registered, errors}, `ep_devices`, `graph`, `rows`, `results` [{provider, gpu, ok, why | selftest,
cpu_fallback_ops, placed, chunk, rows_per_s, rows_per_s_by_batch, vs_cpu}], `providers` (passed, in
order, `cpu` last), `error`, `seconds`) on stdout or in FILE, and a readable report on stderr (the worker
log a friend's "Send diagnostics" uploads). Exit 0 even when only the CPU passes; 2 when not even the
CPU does.

**Runtime packs** (DESIGN 8.4-8.5): the CPU pack (ONNX Runtime 1.31.1); the **Windows ML pack**
(`farm_build.py --runtime --pack winml`: NuGet Microsoft.Windows.AI.MachineLearning 2.5.96 - ONNX Runtime
1.28.3 with the CPU and DirectML providers, DirectML.dll, and Microsoft.Windows.AI.MachineLearning.dll,
whose flat C API (`WinMLEpCatalog*`) is the execution-provider catalog: usable from an unpackaged,
per-user, medium-integrity process with no Windows App SDK runtime and no WinRT registration; the
catalog offers its providers on Windows 11 24H2+ and nothing on older Windows, where DirectML and the
CPU remain). A worker build names the pack it loads (`requires_runtime`), a definition names one
runtime for its builds. The worker loads `DirectML.dll` and the MSVC runtime from the pack by full path
before ONNX Runtime (System32 has an older DirectML). The worker's `ort` binding asks for the C API of
ONNX Runtime 1.23, so a pack may carry any ONNX Runtime from 1.23 on.

**Agent profile:** `gpus[]` gains `luid` (hex), and entries are de-duplicated by LUID. `providers` lists
only providers that passed a probe on this PC; `runtime_providers` the same per runtime pack. Probes
(agent `probe.rs`): between leases, while the policy allows GPU work, for each cached runtime pack that a
cached release worker build requires and that has no result for this PC's adapters (vendor, name and
driver version of each: a new driver or pack is probed again, a restart is not), the agent runs that
build's `probe-providers --runtime-dir <pack> --ensure-ready --json ...` in a job object at below-normal
priority (at most 15 min); results go to `state\probes.json` (provider names, adapter names, max abs,
nodes on the CPU, rows/s, why), a failed probe is tried again after a day, and the readable report goes
to the worker log. `caps.gpu` is true only when the policy allows GPU work and a GPU provider passed a
probe. The job line's `providers`/`gpu`: from the probe of the lease's runtime pack, narrowed by the
lease's `providers` allowlist; the adapter with the most video memory among those where an allowed GPU
provider passed, then every allowed provider that passed on it in order, then `cpu`; `["cpu"]` and
`gpu: null` when the GPU may not be used when the lease starts, the pack was never probed, or nothing
else passed. A worker whose Stats `provider` is not `cpu` runs a GPU job: it is yielded (its session
released) whenever the GPU rules block GPU work, and gets below-normal priority without EcoQoS.

**GPU busy signal:** vendor-neutral, from the Windows PDH "GPU Engine" counters (`\GPU
Engine(*)\Utilization Percentage`, per process: what Task Manager shows): per engine the sum over every
process except the agent and its workers (agents from 0.3.4: every worker of a lease's crew, 13.5 - before, only the
main one), per adapter its busiest engine; the gate is the busiest
adapter (more than 10 % for 30 s blocks GPU work; 30 s quiet clears it). It replaces NVML as the gate for
GPU work; NVML only gives the NVIDIA driver version and a cross-check of the adapter count. Temperature:
`D3DKMTQueryAdapterInfo(KMTQAITYPE_ADAPTERPERFDATA)` per adapter LUID (DESIGN 8.5, decided); a GPU whose
driver reports none (0, or the query fails) has no temperature rule.

**Coordinator routing:** a run's (`selfplay`, `eval`) or campaign's `gpu` setting (`any` |
`preferred` | `required`) and an optional `providers` allowlist (a non-empty list of known names, or
null for any) are matched against each machine's verified providers: `runtime_providers[<the job's
runtime>]` when the profile has it, else `providers`. A machine is **GPU-eligible** for a job when
`caps.gpu` and it verified a GPU provider the allowlist allows. `required`: only GPU-eligible machines;
`preferred`: GPU-eligible machines at the job's tier, others as backfill (fit 0.25); `any`: no filter.
`gpu: preferred | required` with an allowlist that holds no GPU provider is refused (400). Evaluation is
routed first, then self-play (within a tier); fuzz stays the backfill. Rev 18 (section 12) adds hardware routing on
top: CPU self-play of a run with a GPU version after fuzz, and eval after GPU self-play unless the batch is behind. Every lease carries the job's
allowlist as `providers` (null on diag leases).

## 9. Runtime packs per GPU vendor, compressed build files, the measured provider choice (rev 11)

A run names one **default** runtime pack, which every PC can load (the CPU pack, or the Windows ML pack), and
may name **alternates**: a worker build and the runtime pack it requires, offered only to PCs that meet a
condition (an NVIDIA GPU usable for this job, for the CUDA pack). A PC that doesn't meet the condition never
gets, and never downloads, the alternate's pack. Agents from **0.2.10** (`ALT_RUNTIME_AGENT`) take alternates;
older agents only ever see the default runtime, exactly as before.

### 9.1 One signed worker manifest per runtime pack

A worker build names exactly one pack (`requires_runtime`, unchanged). To run the same engine with another pack,
the same `mtgz-worker.exe` bytes are signed again with the other `requires_runtime`: a second worker build (a
different build_id, about 1 KB more in R2; a PC that takes both caches the executable twice, ~25 MB). Chosen over
a list of packs in one manifest because a build id then still pins exactly what runs (the executable and the
DLLs it loads), `requires_runtime` keeps its meaning for every agent and tool that reads it, the probe cache stays
keyed by (pack, adapters), and agents 0.2.5-0.2.9 can keep verifying the default build (a new manifest field
would be an unknown field there; they never see the alternate builds).

### 9.2 Runtime manifests: a hard hardware requirement (`gpu`)

```json
{"manifest": 1, "kind": "runtime", "version": "cuda-12.8-cudnn-9.28-ort-1.24.4", "min_agent": "0.2.10",
 "gpu": {"vendor": "nvidia", "min_driver": "527.41", "providers": ["cuda"]}, "files": [...]}
```

`gpu` (optional): the pack is useful only on a PC with an adapter of `vendor` (a profile `gpus[].vendor` name:
`nvidia`, `amd`, `intel`) whose driver version (the profile's `gpus[].driver`: NVML's for NVIDIA, e.g. `560.94`;
DXGI's user-mode driver version for other vendors) is at least `min_driver` (dot-separated numbers compared
numerically, missing parts 0; absent: any), and it adds `providers` (known provider names, at least one GPU
provider). The coordinator never leases such a pack to an agent older than 0.2.10 or to a PC without such an
adapter (default or alternate alike), and the agent refuses it (9.5). For the CUDA pack: NVIDIA, 527.41 (cuDNN
9.28's Windows minimum for CUDA 12.x, above CUDA 12's own minor-version minimum of 525; Blackwell cards need
570.65+, which every driver that supports them is), `["cuda"]`. A pack without `gpu` (CPU, Windows ML) loads
anywhere.

### 9.3 Compressed build files (`files[].zst`)

```json
{"path": "cublasLt64_12.dll", "sha256": "<the file's>", "size": 531812352,
 "zst": {"sha256": "<the compressed object's>", "size": 172000000}}
```

`zst` (optional): the file is also stored zstd-compressed (one frame, window at most 2^27 bytes) at
`builds/<build_id>/<path>.zst`. An agent from 0.2.10 downloads that object instead, checks its `zst.size` and
`zst.sha256` before decompressing it, decompresses it next to the build's staging directory and checks the file's
own `size` and `sha256` (whose meaning is unchanged: the bytes on the PC). `farm-release publish` uploads the
plain file too (agents before 0.2.10, which never get such a build, and tools read it); the coordinator checks
both objects at registration (`POST /v1/admin/build`). A manifest with any `zst` entry has `min_agent` 0.2.10 or
newer (`farm-release sign --zst` refuses less). Files of at most 64 KiB, or that compress by less than 5 %, get no
`zst`. Files over 64 MiB are uploaded in parts (section 4's multipart upload). Disk: the agent makes room for the
plain sizes plus the largest compressed object (one at a time is unpacked and then deleted).

### 9.4 Run alternates and how the coordinator picks one

`PUT /v1/admin/run/{run}` takes `"alternates": [{"build_id": "<worker build>", "runtime": "<runtime build>",
"when": {"vendor": "nvidia", "min_driver": "560.0"?}}, ...] | null` (at most 4, in order of preference;
migration 0009 `runs.alternates`). Checks (400): `build_id` a registered worker build whose manifest's
`requires_runtime` is `runtime`; `runtime` a registered runtime build; `when.vendor` a vendor name, and equal to
the pack's `gpu.vendor` when the pack has one (it defaults to it); `when.min_driver` optional (the stricter of it
and the pack's applies); no alternate equal to the default (`build_id`, `runtime`) or to another alternate; rev 12:
`when.vendor` not a vendor the farm never uses (`GPU_VENDORS_OFF`: `amd`, section 9.8), and an alternate of such a
vendor stored before rev 12 is never offered. `GET
/v1/admin/run/{run}` and the stats show them, with `problems` per alternate when the current definition doesn't
authorize it. Rev 20: an alternate may name a model variant (`"model": "<name>"`, section 14.3) that its machines play
instead of each gen's default model.

For a run's self-play or eval job and an agent of at least `ALT_RUNTIME_AGENT`, the coordinator puts the **first
alternate the machine qualifies for** in the lease (its `build` and `runtime`) instead of the run's default. A
machine qualifies when all hold:
1. `caps.gpu_allowed` (agents 0.2.10+: the policy and "Use the GPU" allow GPU work now, whether or not a GPU
   provider has passed a probe yet; else `caps.gpu`);
2. its profile lists an adapter of `when.vendor` with a driver at least the minimum (9.2);
3. the job's `providers` allowlist (section 8) allows one of the pack's `gpu.providers` (a pack without `gpu`: any
   GPU provider);
4. its profile's `runtime_providers` has no entry for the pack (never probed here: the first lease probes it), or
   one listing a provider of 3 (an entry without one means the pack failed here: never offered again for these
   adapters and drivers);
5. the alternate's uncached build and pack fit `caps.disk_mb` (their plain sizes);
6. neither build is blocked for this machine (`/v1/crash` `blocked`), both are registered, and their `min_agent` is
   at most the agent's;
7. the run's current definition authorizes it (section 7, rev 11: the build is in `builds` and the runtime is the
   definition's or the build's `requires_runtime`).
Otherwise the lease carries the default. Fuzz (campaigns) and diag leases never carry an alternate. For GPU
routing (section 8) a machine counts as GPU-eligible for an alternate it qualifies for while condition 4 is "no
entry" (it probes the pack before its first job line, 9.6).

**GPU lease sizes.** A batch has one row per game in flight per group, so it never has more rows than the lease
has games, and a GPU only pays from some batch size on (measured: CUDA on a 4070 Ti from ~10-16 rows, DirectML
from ~200-300). For a *GPU lease* - an agent from 0.2.10 that gets an alternate, or whose probe of the run's default
pack passed a GPU provider the job allows (`runtime_providers`) - a run's `selfplay` / `eval` may set `gpu_games_per_lease` (number | `"auto"`; absent: as
before):
- a number: exactly that many games (the owner picks it against the run's staleness window);
- `"auto"`, or absent while `games_per_lease` is `"auto"`: the larger of the normal size and the machine's
  `caps.gpu_games[<run>]` (agents from 0.2.10: per run, from its bench of that run's network, the fewest games
  per lease at which its best GPU reaches 2x the CPU, else 1.1x, with its GPU `groups`, 9.6), or before any
  bench `GPU_LEASE_MIN` (self-play 64, eval 128); at most 4,096. For self-play, once the machine has a measured
  rate, at most 0.8 x that rate x the run's iteration interval (the median gap of its last checkpoint
  registrations) x `max_stale`: a self-play lease keeps the gen it started with and its results are refused
  past `max_stale` gens. When the GPU's games don't fit under that cap the lease keeps its normal size, and the
  agent's measurement (9.6) runs it on the CPU;
- absent while `games_per_lease` is a number: that number (unchanged; GPU leases of such a run are no bigger, so
  the agent's measurement usually picks the CPU there).
`caps.gpu_games`: `{"<run>": games}`, at most 32 runs, games 0 to 100,000; 0: benched, and no GPU provider pays
there at any size (the lease keeps its normal size).

### 9.5 The agent's guard (defense in depth)

An agent from 0.2.10, for a lease whose runtime differs from its definition's `runtime` (an alternate), or whose
pack's manifest has `gpu`: fetches and verifies the build's and the pack's signed manifests alone (no other file),
and refuses the lease before anything else is downloaded unless all hold: the build's `requires_runtime` is the
lease's runtime (for an alternate); the build's and pack's files fit under the disk cap; and for a pack with `gpu`
(GPU work): the job is self-play or eval; the policy allows GPU work now; this PC has an adapter that meets the
pack's `gpu` (vendor and driver); the pack is not marked bad here (a probe of it for these adapters and drivers in
which none of its GPU providers passed) and not held (9.8). A pack without `gpu` - the CPU pack, a CPU alternate's
(11.4), Windows ML's - is not GPU work, default or alternate, and none of the GPU conditions apply to it (agents from
0.3.2; 0.2.10-0.3.1 applied them to every alternate, so a Mac refused a CPU alternate as GPU work): its job runs on
the CPU unless 9.6's probe and bench, which run only while GPU work is allowed and the pack isn't held, pick a GPU
provider of it (Windows ML's DirectML), and the platform check (11.3) applies as to every lease. A refusal is
`done` with `refused: "runtime"` (the lease is closed as given back), a crash report of kind `definition` at most
once a day per pack and reason, and the start-failure wait (1 min doubling to 1 h). Agents before 0.2.10 refuse such
a lease anyway (their exact runtime rule, section 7), before downloading anything.

### 9.6 Probe first, then choose CPU or GPU by measured speed

Before the first job line of a self-play or eval lease with a runtime pack, while the policy allows GPU work and
the PC has a GPU: a pack that has no probe here yet gets the **built-in probe** first, exactly the between-lease
one of section 8 (`probe-providers --runtime-dir <pack> --ensure-ready ...`, the built-in graph, recorded in
`state\probes.json`, the profile refreshed), so a newly downloaded pack is never judged unprobed (`["cpu"]`) on
its first lease; the between-lease probe itself is unchanged. Then, if a GPU provider passed that probe, the
agent makes sure it has a **bench** of the job's own network on this pack: `<worker> probe-providers
--runtime-dir <pack> --model <the lease's model> --reference <its reference set> --providers <the providers that
passed, cpu included> --bench-rows 1,8,32,128,512,1024 --reps 3 --threads <the job's threads> --json <file>`, at
most 15 minutes, beating meanwhile; a record of its own, per (runtime build, adapters with drivers, run), in
`state\bench.json`, kept 7 days. The profile's `runtime_providers` lists `[]` (rev 11) for a pack whose probe
failed less than a day ago.

The choice, the same for every vendor and provider: the job's batch size is `b = ceil(min(conc, games) /
groups)` rows (one leaf per game in flight per batch; `conc` and `groups` from the template's settings, `groups`
default 1). For each (provider, adapter) of the bench whose self-test passed, the rows per second at `b` are
interpolated log-linearly between the benched sizes (clamped at the ends); a GPU candidate is rated at its own `b`,
with `conc` raised to `min(games, max(conc, 512))` and `groups` lowered to at most 2. The fastest wins, and a GPU
provider only if it is at least 1.1 times the CPU. The job line then has `providers` = `[winner, "cpu"]`, `gpu` =
the winner's adapter and, for a GPU winner, those `conc` and `groups` (the agent's tuning fields, section 5);
else `["cpu"]`, `gpu` null and the template's `conc` and `groups`. No GPU provider slower than the CPU at the
job's batch size ever runs. From the same numbers the agent works out `caps.gpu_games[<run>]` (9.4). Stats (beats
and `done`) carry `batch_rows` (`b` as chosen) and `rows_per_s` (`{"<provider>": rows/s at b}` for the CPU and
each GPU candidate), which the coordinator keeps with the lease's stats.

The built-in graph is not the last word: the bench also tries a pack's own `gpu.providers` when the built-in probe
failed them (that graph fails CUDA's priors, max abs 2.4e-3 from duplicate scatter slots in its reference rows, where
real exports pass at ~2e-6). A pass there is the factory's full self-test on the job's own network and reference set
(the same `try_one` as `create_session`: 2e-5 on every output, twice bit-identical, a node on the GPU; rows/s never
count), and it counts for the pack: the probe record keeps both verdicts (`providers` of the built-in probe, `real`
and `benched` of the job's network; `need` the pack's providers), the worker log says `built-in probe FAIL, <run>'s
network PASS`, and `runtime_providers` lists the provider. A GPU pack whose own providers failed the built-in probe
but were never tried on a real network here is left out of `runtime_providers` (unknown, not failed). The job's own
`create_session` self-test stays the final gate for every network. A pack with `gpu` in which none of its providers
passed, on the built-in graph or a job's network, is marked bad for these adapters and drivers, the lease runs on the
CPU (its pack's CPU provider), and the pack is evicted from the cache when the lease ends; it is not downloaded again
until the driver or the adapters change (9.4 condition 4, 9.5). A bench that failed to run is kept a day (the CPU
meanwhile), a provider that failed its self-test in a bench stays failed for that (pack, adapters, run) for 7 days.

### 9.7 Packs

| GPU vendor | Pack | How it's chosen |
|---|---|---|
| any (default) | the CPU pack (ONNX Runtime 1.31.1 CPU), or the Windows ML pack (DirectML, Windows ML's catalog) | the run's `runtime` |
| NVIDIA | the **CUDA pack** (`farm_build.py --runtime --pack cuda`): ONNX Runtime 1.24.4 (the last CUDA 12 build; 1.27+ need CUDA 13, driver 580+) with the CUDA provider, the CUDA 12.8 runtime, cuBLAS and cuBLASLt, cuFFT (a load-time import of ONNX Runtime's CUDA provider), cuDNN 9.28 (`cudnn64_9`, `cudnn_graph64_9`, `cudnn_ops64_9`: the only cuDNN libraries our graphs load), the MSVC runtime; `gpu` NVIDIA 527.41 `["cuda"]` | a run alternate, `when.vendor` `nvidia` |
| AMD | none (rev 12: no GPU work on AMD GPUs, 9.8) | AMD PCs run CPU work on the run's default pack |

### 9.8 Probes the PC never finished, GPU vendors off, the evict list (rev 12, agents from 0.2.11)

**The in-progress marker.** Before a built-in probe or a bench (9.6) starts its worker, the agent writes
`state\probe-inflight.json` `{"kind": "probe" | "bench", "runtime", "adapters" (the probe key: vendor, name and
driver of each adapter), "worker", "run"?, "t"}` through to disk (flushed, renamed with write-through), and removes
it when the probe ends, whatever its outcome (a failed probe, a timeout or the agent quitting are recorded as
before; Windows logging off or shutting down - `SM_SHUTTINGDOWN` - stops a probe like the agent quitting, so a
shutdown is never taken for a fault). If it can't be written the probe doesn't run. A marker found at the next start means the agent or the PC
went down during it - a GPU driver fault, a hard hang ended with a reset, a crash - and counts as an **unfinished
probe** of that pack on those adapters (`state\unfinished.json`): once, the pack is held for `RETRY_S` (a day);
twice, it is held for good on those adapters and drivers (a driver update changes the key: one more try). A held
pack gets no probe (between leases or in a lease) and no bench, its jobs run on the CPU, the guard (9.5) refuses it
when it has `gpu` (`refused: "runtime"`; agents before 0.3.2: any alternate), and the profile's `runtime_providers`
lists it with no provider (`[]`), so the coordinator stops offering it as an alternate (9.4 condition 4). The agent
reports each one at that start (`POST /v1/crash` `probe_unfinished`: the pack, the worker, the count, how long before
the start).

**What a probe may load.** The built-in probe and the bench pass `--providers tensorrt_rtx,cuda,openvino,dml,cpu`:
never AMD's providers (`amdgpu`, `migraphx`), and the measured choice never picks them, even from a bench an older
agent kept. On a PC with an adapter of a vendor that is off (below) they also pass `--gpu <LUID>` of one usable
adapter (NVIDIA first, then Intel), so the worker doesn't enumerate the others.

**GPU vendors off.** The owner's decision of 2026-11-13: the farm runs no GPU work on AMD GPUs (the AMD providers
failed on a farm PC's AMD GPU and a driver fault mid-probe could take a PC down). Agents from 0.2.11 leave AMD
adapters (`vendor: "amd"`, PCI vendor 0x1002 / 0x1022) out of the profile's `gpus` altogether: no probe, bench or
GPU job touches them, `caps.gpu` and `caps.gpu_allowed` stay false when they are a PC's only GPUs, and no
alternate can match them (9.4). The coordinator refuses and skips AMD alternates (9.4). The choice itself (9.6)
stays vendor-neutral among the adapters that are left.

**The evict list.** `GET /v1/agent/latest` carries the owner's `evict` list (section 3, `POST /v1/admin/evict`).
Agents from 0.2.11 delete each listed build from `cache\builds\` (and its LRU entry) when they get it: at start,
before any probe (the update check is the main loop's first step), and every 6 hours after, always between
leases. Deletion only: a build that is still needed is downloaded again when a lease needs it. It is how a
withdrawn build (a diagnostic worker, a pack nobody should probe again) leaves PCs that cached it; a worker build
whose runtime pack is gone is never probed (the pack has to be in the cache and verify). The list is global (every
machine, every channel): before a run uses a listed build again, take it off the list (`POST /v1/admin/evict` with
the rest, or `[]`), or every agent deletes it at each start and each update check and downloads it again for every
lease.

## 10. Rolling self-play leases (rev 15; coordinator migration 0011, agent 0.2.13, worker feature `rolling`)

**Why.** A classic self-play lease starts all its games at once and replaces none: its batch drains to one row per
call for the last minutes, and it lasts as long as its slowest game whatever its size, so a large GPU lease keeps about
a third of its full-batch rate and whole leases outlive the staleness window (`max_stale`) and are refused. A *rolling*
lease keeps N games in flight: the worker starts a new game whenever one ends, asks for more game ids as it goes,
switches to each new generation in place (section 5, `checkpoint`) and uploads finished games in small chunks. The
batch stays full, a new generation costs no restart, and staleness is judged per row, not per lease. Classic leases
are unchanged; everything below is additive.

### 10.1 The run's setting

`PUT /v1/admin/run/{run}` `selfplay.rolling`: `{"inflight": n | "auto", "gpu_inflight": n | "auto", "block": 65536,
"max_s": 7200, "flush_s": 120}` or null / absent (classic leases only). `inflight` / `gpu_inflight` (1 to 4,096 or
`"auto"`, default `"auto"`): the games in flight of a CPU / GPU job (`"auto"`: the agent's choice, 10.6); `block` (1 to `MAX_LEASE_IDS`,
default 65,536): the ids a lease reserves; `max_s` (600 to 86,400, default 7,200): how long a lease runs before the
coordinator decides whether it goes on (10.4); `flush_s` (10 to 3,600, default 120): the longest a finished game waits
in the worker before it is uploaded. Missing keys take the defaults; unknown keys → 400. The desktop sets `rolling` only
for a run whose worker builds (the default and every alternate) announce the feature `rolling` (10.5); the coordinator
cannot see inside a build, so this is the desktop's declaration. GET and the stats show it.

### 10.2 Who gets one

A self-play lease of a run with `rolling` set is rolling when the agent is at least `ROLLING_AGENT` (Worker var,
default `0.2.13`) and its `POST /v1/lease` request lists `"rolling"` in the new optional field `"features": [...]`
(names the coordinator doesn't know are ignored). Any other agent gets a classic lease of that run, exactly as before.
Eval, fuzz and diag leases are never rolling.

### 10.3 The lease

A rolling lease is a self-play lease (section 3) plus

```json
"rolling": {"block": [lo, hi], "inflight": {"cpu": n | "auto", "gpu": n | "auto"}, "until": 1794307200,
            "max_s": 7200, "flush_s": 120}
```

- `block`: ids `[lo, hi)` reserved for this lease, a fresh range from the run's `next_gid` (never from the pool), of
  `block` ids (fewer when a game id would pass 2^32).
- **Released ids.** `job.gid_base` = `lo`; `job.games` = the ids released so far: the lease's game ids are `[lo, lo +
  games)`. At grant `games` = min(headroom, `block`, 2 x the larger `inflight`, `"auto"` counting 256). Only released
  ids count as leased for the quota (section 4 `selfplay_state.leased` = the unclaimed released ids of active leases);
  the rest of the block belongs to the lease but to no game yet.
- `until`: the lease runs to at least this time (`issued_at + max_s`, moved on by a renewal, 10.4).
- `inflight`: the run's setting for a CPU and a GPU job; the agent picks one by its own CPU-or-GPU choice (9.6).

### 10.4 `POST /v1/lease/{lease_id}/extend`

`{"games": k}` (k ≥ 1): release k more ids. Answers (200):
- `{"games": total}`: the lease's ids are now `[lo, lo + total)` (total ≥ the previous; it may be less than asked:
  the run's self-play headroom, or the block's end);
- `{"games": total, "wait_s": 60, "reason": "quota"}`: nothing released now (no headroom: the generation's quota is
  full); ask again after `wait_s`;
- `{"games": total, "finish": true, "reason": "max_s" | "block" | "state"}`: no id will ever be released: finish the
  games in flight and close the lease (`done`). `max_s`: `until` has passed and the coordinator does not renew it;
  `block`: the block is used up; `state`: the run's self-play is no longer rolling, or the run, the machine or the
  whole farm (`POST /v1/admin/pause`) is paused, or the lease's next beat cancels it or gives it back (a paused run's
  leases are also cancelled at their next beat, section 4), or (2026-11-19) the run's current definition no longer
  lists the lease's worker build (the run was re-pinned: its results would be refused and it can't switch
  generations; the agent asks for a fresh lease with the current builds).
- **Renewal.** Past `until`, the coordinator renews the lease (`until` += `max_s`, and the answer releases ids as
  usual) unless something else is waiting for this machine, in the order section 3 picks work: a diagnostics request
  for it; an eval batch of an active run with unissued ids; a fuzz campaign with a `share` that is below it (5.4);
  another active run's self-play with headroom and a higher `priority`. Then it answers `finish` with `"reason":
  "max_s"`, and the agent's next `/v1/lease` gets the waiting work. So a rolling lease is re-decided at every id
  release past `until`, at least every `max_s`.
- 400 for a classic lease or a bad `games`; 404 a lease that isn't this machine's; 410 a lease that isn't active
  (expired, closed, cancelled). Rate limit: the beat bucket (section 1). The agent asks when its worker says it is
  low on ids (10.5 `low`), at most once per 10 s, for about 2 x its games in flight.

### 10.5 The worker (aw/1, feature `rolling`)

- `hello` gains `"features": ["rolling"]` (absent: none); `<worker> info` prints the same list.
- The job line's optional `"rolling": true` (and `"flush_s"`): the job does **not** end when its `games` are done.
  It keeps up to `conc` games in flight while ids are left, then waits (still answering `ping` and sending `status`)
  for `{"op": "extend", "games": total}` (raises the job's games to `total`: ids `[gid_base, gid_base + total)`;
  never lower) or `finish` / `quit`. It sends `{"ev": "low", "started": s, "games": g}` when fewer than `conc` of its
  ids are unstarted (g - s < conc), once per `games` value.
- A records file is written when it holds `shard_rows` rows, or `flush_s` seconds after the first game in it
  finished, so finished games reach the coordinator within about `flush_s`.
- `checkpoint` (section 5) in a rolling job: the worker builds the new network's session and runs its self-test
  (`selftest` with `gen`) on a thread of its own while the games go on with the old network, then switches (games in
  flight continue, their searches restart on the new network, each row records `net`). A failed self-test is fatal,
  as at the start.
- `end` (`finish` or `quit`): `unfinished` = the released ids not finished.
- A worker without the feature ignores `rolling` and `extend` (an `error`, not fatal) and ends `done` after `games`:
  the agent then closes the lease as a classic one. The agent sends `extend` only to a worker whose `hello` lists
  `rolling`.

### 10.6 The agent (0.2.13; memory: 0.2.14)

- Sends `"features": ["rolling"]` in `/v1/lease`.
- **Games in flight.** `conc` of the job line = the lease's `inflight` for the job's provider (a GPU winner: `gpu`,
  else `cpu`). `"auto"`: CPU: 32 in flight in 4 groups; GPU (agents from 0.3.4): from the bench of the job's own network
  (9.6) its **full batch** - among 32, 64, 128, 256, 512 and 1,024 rows the batch after which doubling gains less than 15
  % rows/s - and **two full batches** in flight (every worker's games of the job: while one batch is on the GPU the next
  one fills), in groups of at most that batch (1 or 2; 0.2.13-0.3.3: the in-flight count among 64, 128, 256 and 512 after
  which doubling gained less than 15 %, each rated at half of it below 256 - a 4070 Ti PC never got past 512, and its
  memory plan held it to 128); a GPU job's layout - its workers, their games in flight and groups - is then section 13's
  from that target (13.1); both capped so that the games in flight
  fit the worker's memory limit (`caps.ram_mb`, the job object's limit): planned to commit at most 85% of it at a base
  plus a cost per game in flight - 1,800 MB + 45 MB on a GPU, 600 MB + 45 MB on the CPU (macOS: 11.5), or this PC's
  measured cost for the run when it is higher (**Memory**, below) - ((0.85 x `ram_mb` - base) / per game, rounded
  down to a multiple of 32 when at least 32; agent constants, `rolling.rs`, from the soak of 2026-11-17: pb3's network at 224 in flight
  with no limit levelled off at about 11.4 GB, 1.75 GB + 43 MB per game; a 4070 Ti PC with a 10,735 MB limit gets 160
  on the GPU, where 0.2.13's 1 GB + 40 MB per game gave 224 and every lease ran into the limit after 21-25 minutes; the
  4070 Ti PC of 2026-11-19, legacy-meta3-r1's network on the CUDA build, 5-minute windows: its bench's full batch is 512
  rows, so 1,024 games in flight - one worker of 128 in flight played 5,030 leaves/s with the GPU 37 % busy, 1,024 10,825,
  where one worker levels off; crews, 13.5). A number is taken as it is (a CPU job in the template's `groups`, a GPU job in at most 2) unless this
  PC measured the run's cost (then it is capped the same way); anything that is neither a number from 1 to 4,096 nor
  `"auto"` counts as `"auto"`. `games` of the job line = the released ids; the template's `conc` / `groups` are replaced as for a GPU job
  today (section 5 tuning fields). A lease whose `rolling` doesn't fit its job (`block[0]` = `gid_base`, the released
  ids inside the block), or that has no definition, runs as a classic lease; `flush_s` is kept within 10 to 3,600.
- **Ids.** On `low`, `POST .../extend` for about 2 x `conc`; on `{"games"}`, `{"op": "extend", "games": total}` to
  the worker; on `wait_s`, ask again then; on `finish`, `{"op": "finish"}` to the worker. A 404 or 410 stops the lease
  like a lost one (`quit`, given back); 429, 5xx and network errors are retried with backoff (honouring
  `retry-after`); any other 4xx, or a total past the block, ends it with `finish`.
- **Generations.** Beats of a rolling lease carry `"gen"` (the newest generation the worker plays). When the run's
  current generation is newer, the beat answer has `checkpoint` **and** `"definition": {"key", "sha256"}` (the current
  definition version). The agent verifies that definition as at grant (section 7, steps 1-4: signature, kind and id,
  expiry, a version never older than any seen, the lease's worker build in `builds` and its runtime authorized as at
  grant, the checkpoint's files as signed), and checks that its `jobs.selfplay` template is the lease's own (canonical
  JSON). Then it fetches the checkpoint (beating meanwhile) and sends `checkpoint` with the model's `sha256`. A
  template that changed, or a definition that fails a check, ends the lease gracefully: `finish` (its games in flight
  complete on the network they have; the agent's next lease brings the new template); a failed check is reported once
  per lease (`/v1/crash` kind `definition`), a changed template is not. A definition that can't be fetched right now
  is asked again at a later beat; a checkpoint whose files can't be fetched or verified ends the lease with `finish` too
  (it would otherwise play an old network for good). Repeats are ignored. Classic
  leases are unchanged (they switch only to gens their own definition signs, section 3).
- **Ending.** As any lease: `cancel` → `quit`; `give_back`, a pause or starvation past `slack_s` → give back; the user
  quits → `quit`. Rolling leases also end with `finish` when the coordinator answers `finish` to an extension, when the
  template changes (above), and when the agent has a newer version ready to switch to (section 6) - a rolling lease
  would otherwise never let it update: while one runs, the agent checks `GET /v1/agent/latest` every 6 hours as between
  leases (the download keeps the lease's files in the cache; the `evict` list is applied when the lease ends).
- **Memory** (0.2.14). The agent reads the worker's committed memory (what the job's limit counts) at every tick.
  - At 92% of the limit it sends `finish`; once the worker has ended (`end` `finish`) it starts a new worker in the
    same lease with fewer games in flight: what fits at the measured cost per game ((committed - base) / games in
    flight, if higher than the plan's), at most 3/4 of the games in flight before (rounded as above, always fewer).
    The new worker's job line has the lease's latest network, an `out_dir` of its own, `gid_base` = the first released
    id its predecessor never started (the single tail range of its `end`'s `unfinished`; anything else: the end of the
    released ids) and `games` = the released ids from there; `extend` totals count from its `gid_base`. Nothing is lost.
  - A worker that runs into the limit - its fatal `error` or its stderr says an allocation failed (ONNX Runtime's "bad
    allocation", Rust's "memory allocation of N bytes failed", ...), or it exits without `end` with the job's peak at
    97% of the limit or more - is started again the same way, its games in flight lost: the new worker's ids start at
    the end of the released ids (games 0: it asks with `low`). The lost ids go back to the pool when the lease closes:
    after a restart `done`'s `unfinished` is the released ids not credited (worked out from the results, as for a worker
    that died). Reported once per lease (`/v1/crash` kind `crash`); the lease is not given back.
  - At most 4 restarts per lease. After that, or when one game in flight already didn't fit, a worker at 92% finishes
    the lease (`finish`) and one that runs into the limit fails the lease as before.
  - **Self-calibrating** (agents from 0.3.3). The measured cost per game is remembered per run, **worker build** and
    CPU / GPU job in `state\rolling_memory.json` (keys `<run>/<build_id>/cpu|gpu`; 0.2.14-0.3.2 kept one value per run,
    `<run>/cpu`, only ever raised and planned as the more of it and the default - those keys, measured with the builds of
    before, are not read). A build not measured here is planned with the platform's default cost (above; macOS 11.5),
    which came from the builds of before and is conservative; a measured build with what was measured, below the default
    too (zero-core's native worker fixed on 2026-11-19 needs ~2.5 MB a game where the builds of before needed 13-16). It
    is raised at once: whenever a worker commits more per game than planned, at each restart and when a lease ends. It
    is lowered from **steady** readings - a worker that finished twice its games in flight since it started, its games
    in flight full -: the most such a worker committed per game x 1.2 (rounded up), when that is below the plan. "auto"
    and a run's number are planned with it from then on, and so is the rest of the lease that measured it: a raise at
    its memory checks (its next workers, a crew's next members, 13.5, handoffs, 13.2, and the tuner's memory bounds); a
    steady reading at most 80 % of the cost the lease is planned with at once - the tuner's bounds widen (an "auto"
    job's games in flight are worked out again), a layout the old plan held back gets the games in flight the new one
    holds, and the job is handed over to it (13.2, within its rules: one drain at a time, the memory cap; a fixed layout
    too, 13.1); agents from 0.3.4 also work the worker bounds out again (a crew the old plan held back is possible). A
    lease without a tuned layout leaves it to the next lease - a GPU job always has one (agents from 0.3.4; fixed with
    `autotune` off, 13.1), so its games in flight follow the measurement within the lease: the 4070 Ti PC's lease, planned
    at the default 1,800 MB + 45 MB a game (128 in flight with its limit), measured ~3 MB and kept 128 with agent 0.3.3;
    from 0.3.4 it is handed over to the target's 1,024 (or a crew, 13.1) once the steady reading says it fits.
  - **Early reading** (agents from 0.3.5; a GPU job). A GPU job's main worker gives one early reading once it finished
    half its games in flight with them full, and at least 90 s after it started: the cost per game it shows x 2 (rounded
    up) is remembered when lower than planned, and planned with as a steady reading is (above). The 4070 Ti PC's first
    0.3.4 lease, 2026-11-20: a CUDA build not measured there was planned at the default 45 MB a game - 139 in flight
    under its 9.5 GB cap, 5,179 leaves/s - while the memory-fixed worker commits ~2 MB a game; two turnovers of 139 take
    ~4 minutes there. An early reading at or under the base counts 1 MB a game.
  - **The base, measured** (agents from 0.3.6; a GPU job). A worker's commit at its first `status` after its sessions
    exist (it names a provider) with games started and none finished is a base reading: the runtime and the network,
    its games barely begun. The lease's lowest reading is remembered per run, worker build and provider class (key
    `<run>/<build_id>/gpu_base`) when more than 5 % below what is planned with (the default's 1,800 MB, or what is
    stored; never raised - a reading holds the games' first memory too, more with more games in flight, and a base above
    the default is in the cost per game measured over it), and planned with at once, as a lower cost per game is (the
    worker bounds, the layouts held back, the start). The 4070 Ti PC's 0.3.5 lease (round 5, 2026-11-20): planned at
    1,800 MB + 4 MB a game per worker under a 9.3 GB limit, a crew of two never fitted beside the running worker.
- **The job's memory limit** (`caps.ram_mb`; agents from 0.3.6). The owner's `max_ram_mb`, else half the RAM, and never
  more than the free memory less what is kept for the owner: an eighth of the RAM while the PC is idle (no input for
  `idle_minutes`), a quarter else, at least 4 GB either way (`policy::mem_floor_now`). Free memory below that floor blocks
  work (`low_memory`), and a paused lease is given back, its memory freed - so when the owner comes back, the quarter
  applies again. 0.2.0-0.3.5 kept a quarter whatever: the 4070 Ti PC (32 GB, its WSL servers holding ~15 GB, ~17 GB
  free) got 9.3 GB, idle; it gets ~12.9 GB now.
  - No wire change: the coordinator sees one lease, its released ids and its extensions as before.
- **Video memory** (agents from 0.3.4; a GPU job). 0.2.13-0.3.3 planned none: one worker of at most 512 games in flight
  never came near a GPU's. A GPU job's workers (13.5) are planned within it:
  - The adapter is the job line's `gpu`; its dedicated memory the profile's `vram_mb` (DXGI); other programs' use of it at
    the lease's start from Windows' `\GPU Adapter Memory(*)\Dedicated Usage` counters (1,024 MB when Windows has
    none). The budget: 85 % of the dedicated memory less that. Without the adapter's size: one worker.
  - A worker is planned at 1,024 MB + 2 MB per row of its biggest batch (games in flight / groups), until this PC
    measured the run's own with the lease's worker build (`state\rolling_memory.json` key `<run>/<build_id>/vram`, MB per
    row over the base), calibrated as the memory per game: higher at once, lower from steady readings x 1.2 when the lease
    ends. (The 4070 Ti PC: the whole GPU, its desktop included, held 709 MB with one worker of 128 in flight, 1,605 MB with
    512 in one group, 2,557 MB with three workers of 384 - about half the plan.)
  - Readings, at most once a second: each worker's dedicated memory from Windows' `\GPU Process Memory(*)\Dedicated
    Usage` (per pid, what Task Manager shows; NVML gives no per-process memory under WDDM), or the worker's `status`
    `vram_mb` when it reports one - the more of both; none while it is paused.
  - Every layout holds each worker's base and biggest batch within its share of the budget (13.1), and a new worker
    starts only when its plan fits beside what the lease's workers hold or will hold (13.2's rule: workers playing new
    games at the more of their reading and plan, drainers at their reading), within the budget less what other programs
    took since the start.
  - When the adapter's dedicated memory in use (every program's) reaches 95 % of it, one worker gives way while no drain
    is in progress: a crew's biggest finishes its games (no restart counted; none joins until its plan fits again); a lone
    worker is started again with fewer games in flight (10.6's restart).
  - A worker that runs out of video memory - its fatal error or stderr says so (ONNX Runtime's "CUDA failure 2: out of
    memory", `cudaMalloc`, `..._ALLOC_FAILED`, `E_OUTOFMEMORY`, "out of memory" / "failed to allocate" with a GPU word), or
    it failed to allocate below 90 % of its memory limit - raises the plan per row (to what it held, at least a quarter
    more, remembered) and is started again with fewer games in flight (its games in flight lost; a restart, at most 4; the
    memory per game untouched); a crew member's makes the next members smaller. Reported once per lease.
- Stats add `inflight` (the chosen `conc`; after a restart the current worker's) and `released` (the lease's released
  ids); a GPU job's (agents from 0.3.4) `vram_mb`: its workers' video memory as read.

### 10.7 Results, staleness and closing

- A rolling lease's self-play chunks are **never refused as stale** (no 410 for `max_stale`): the trainer judges
  each row by the network that searched it (zero-core's pool: rows older than its window are dropped, the rest of the
  game kept). `x-gen` is still checked (not older than the lease's first generation, not newer than the current one),
  and `x-ids` must lie in the released ids `[lo, lo + games)`.
- Closing (`done`, expiry, give back, cancel): the released ids not credited go back to the pool, as for a classic
  lease; the unreleased rest of the block `[lo + games, hi)` goes back to the run's `next_gid` when nothing was taken
  after it (`next_gid` = `hi`), else to the pool.
- The machine's rate counts credited ids per running second, as before; `games_per_lease` and `gpu_games_per_lease`
  don't apply to rolling leases.
- The D1 lease row gains `rel_hi` (released ids `[lo, rel_hi)`; NULL for classic leases), `until` and `cur_gen` (the
  newest generation the agent reported). Migration 0011.

Coordinator details (from building it, rev 15):
- **Order of the checks on extend.** 404 (not this machine's lease), 400 (`games` not an integer 1 to `MAX_LEASE_IDS`,
  or a classic lease, even a closed one), 410 (not active), then `finish` `state`, `finish` `block`, the renewal
  decision past `until`, and the release (or `wait_s` `quota`). A renewal is stored even when the answer is
  `wait_s`. `wait_s` is 60.
- **What waits for the machine** (renewal) is worked out like a lease request (section 3's candidates), from what
  the coordinator last stored for the machine: its caps of its last `/v1/lease` (`caps.gpu` standing in for
  `gpu_allowed`, no `gpu_games`), its stored profile and agent version, every job kind and nothing cached. A job
  waits when section 3 would rank it before the rolling lease's own self-play by tier and then by rank (evaluation,
  rev 13's background jobs and rev 14's fuzz shares within their share), or, at the same tier and rank, when it is
  another run's self-play with a higher `priority`; a self-play job waits only with headroom. A diagnostics request
  counts only when the agent would get it as a lease, which since rev 6 is never the case for a definition-aware
  agent (logs requests ride on beat answers and need no new lease), so for rolling leases it never ends one.
- **Generations.** A beat's `gen` (an integer, else 400) is stored as `cur_gen` only for a rolling lease and only when
  it is not older than the lease's first gen; a classic lease's beat ignores it and its answer keeps exactly the keys
  of before. The offer compares the run's current gen with `cur_gen` (before any report: the lease's first gen) and is
  made only when the current definition signs the current gen's files as registered; `definition` is that
  definition's `{key, sha256}`.
- **Closing.** `done`'s `returned` counts the released ids that went back to the pool; the unreleased tail (to
  `next_gid` or the pool) is not in it. `done`'s `unfinished` must lie in the released ids.
- **Admin.** `GET /v1/admin/run/{run}` shows the setting in `selfplay.rolling` and adds `rolling_leases` (each active
  rolling lease: `lease_id`, `machine_id`, `name`, `block`, `released`, `claimed`, `until`, `gen`, `cur_gen`,
  `issued_at`); the stats add per run `rolling` (the setting or null) and `rolling_leases`; the admin page shows a
  "rolling" column in Runs and a "Rolling leases" table.

## 11. Platforms: macOS on Apple silicon (rev 17; coordinator migration 0013, agents from 0.3.0)

**Why.** Until rev 17 every agent, worker build and runtime pack was Windows x64, and nothing on the wire said so.
A Mac (Apple silicon) can do the same CPU work, but it needs its own agent, worker builds and runtime packs, and a
Windows PC must never be handed a Mac build (or the reverse). Rev 17 names the platform everywhere a build is chosen.
Everything below is additive: an agent or manifest that says nothing is `windows-x64`, exactly as before.

Platform names: `windows-x64` and `macos-arm64` (Apple silicon, macOS 14 or newer: the ONNX Runtime 1.31 pack's minimum; the install script refuses older). Intel Macs and Linux: not yet.
`PLATFORM_AGENT` (coordinator var, default `0.3.0`) is the first agent that knows platforms.

### 11.1 The agent's platform

- Every request of an agent from 0.3.0 carries the header `x-farm-platform: <platform>`; absent = `windows-x64`
  (every older agent). The profile may carry the same value as `platform` (informational; the header counts).
- `POST /v1/enroll` with an unknown platform → 400. The coordinator stores the platform with the machine
  (`machines.platform`, migration 0013; existing machines `windows-x64`). It is fixed at enrollment: a later request
  whose header names another platform (no header: `windows-x64`) → 409 `conflict` with `"reason": "platform"` (a
  machine token copied to another computer: install there instead); an unknown platform → 400.
- The admin stats and `GET /v1/admin/machines` show each machine's `platform`; the stats' `limits` add
  `platform_agent`.

### 11.2 Agent releases per platform

- `agent_releases.platform` (migration 0013; existing rows `windows-x64`). `POST /v1/admin/agent/release` takes
  `"platform"` (default `windows-x64`), which must equal the agent build's manifest platform (11.3); the answer adds
  `platform`.
- Every lookup of "the channel's latest release" uses the newest release of (channel, platform), with the
  machine's platform (or the request's, before enrollment): enroll's 426, the update rule of `POST /v1/lease`
  (section 3), `GET /v1/agent/latest`, `GET /download`, the onboarding page and the stats' `releases` (one per
  channel and platform, with `platform`). So `latest` and `min_agent` are per platform: a Windows-only release never
  asks a Mac to update, and the reverse. Enroll's 426 `download` names the platform when it isn't `windows-x64`
  (`/download?platform=macos-arm64`, `&channel=canary` as before).
- GitHub (section 2.1): a macOS release is the GitHub Release `agent-macos-v<version>` (never marked Latest;
  `agent-v<version>` stays Windows), assets `ZeroFarm-macos-arm64.tar.gz`, `SHA256SUMS` (its one line: the tarball),
  `SHA256SUMS.minisig` (same rules as 2.1). Its `github_url` must be exactly
  `https://github.com/<GITHUB_REPO>/releases/download/agent-macos-v<version>/ZeroFarm-macos-arm64.tar.gz`.
- `GET /download?platform=macos-arm64` (optional `channel` as today) → 302 to the newest macOS release's tarball;
  every `/download` answer adds `x-farm-platform`; an unknown `platform` → 400.
  `GET /install-macos.sh` (optional `?channel=canary`) → a POSIX shell script (`text/plain`) that downloads that
  tarball, checks it against the release's `SHA256SUMS`, unpacks it into a temporary directory and runs
  `./ZeroFarm setup "$@"` (so `curl -fsSL https://farm.greggg230.com/install-macos.sh | sh -s -- --invite CODE`
  works, and without `--invite` the setup asks on the terminal). The tarball holds the launcher `ZeroFarm`, the
  agent package (its signed manifest and files) and nothing else, flat (`ZeroFarm`, `farm-agent`, `manifest.json`,
  `manifest.json.minisig`; executables 0755); `ZeroFarm setup` verifies the package's manifest against its embedded
  release keys before installing it. The script (`set -eu`, every failure a message and exit 1)
  stops unless `uname -s` is `Darwin` and `uname -m` is `arm64`; reads the tarball's URL from the coordinator's own
  `/download?platform=macos-arm64[&channel=]` (the request's origin) and goes on only for
  `https://github.com/<GITHUB_REPO>/releases/download/agent-macos-v<version>/ZeroFarm-macos-arm64.tar.gz`; fetches
  `SHA256SUMS` from that release (`shasum -a 256 -c` of the tarball's line); and, piped into `sh`, gives setup the
  terminal (`/dev/tty`) as its stdin. It is served before any macOS release too (it then says there is none).
- The onboarding page shows the macOS install command once a macOS release names a `github_url`.

### 11.3 Builds per platform

- Manifests (`agent`, `worker`, `runtime`) take an optional `"platform"` (`[a-z0-9-]{1,32}`); absent = `windows-x64`.
  A manifest that names a platform has `min_agent` 0.3.0 or newer (`farm-release sign --platform` refuses less; the
  agent's parser doesn't check it). Agents before 0.3.0 ignore the unknown field (manifest/1 parsing allows unknown
  fields) and would try to run the build: the coordinator never leases them one.
- On macOS executables have no extension (`entry: "mtgz-worker"`; an agent package's `farm-agent` and `ZeroFarm`)
  and a runtime pack carries `libonnxruntime.dylib` (the name the worker loads from aw/1 `runtime_dir`). The
  executable bit is not in the manifest: the agent sets mode 0755 on a build's `entry` (and an agent package's
  `ZeroFarm`) after verifying it. Binaries are ad-hoc code-signed (the arm64 linker does it) and not notarized;
  files the agent downloads itself carry no quarantine attribute, so Gatekeeper doesn't check them.
- `POST /v1/admin/build` stores the manifest's platform (`builds.platform`, migration 0013) and answers it; an
  unknown platform, or a manifest that names a platform with `min_agent` absent or below `PLATFORM_AGENT` → 400
  (`problems`).
- An agent from 0.3.0 checks, after the lease's definition (section 7) and before downloading anything but the
  manifests (fetched and verified alone, as in 9.5), that the lease's worker build (and runtime pack) manifest names
  its own platform (a manifest without one is `windows-x64`'s: a Mac refuses it). A mismatch → `done` with
  `refused: "platform"` (`given_back`, no `unfinished`), a crash report of kind `definition` ("lease refused (platform
  <build_id>): ...") at most once a day per build, and the start-failure wait (1 min doubling to 1 h, 9.5's pattern).

### 11.4 Which build a machine gets

- **Runs.** An alternate (9.4) takes `when.platform` (default `windows-x64`), and `when.vendor` becomes optional:
  an alternate without `vendor` whose `when` names `platform` (`windows-x64` too) is a **CPU alternate**, offered
  to every machine of its platform with no GPU condition (its runtime must have no `gpu`; no `min_driver`); the agent
  takes it as CPU work too (9.5, agents from 0.3.2: no GPU condition, the platform check of 11.3). One
  that names neither, with a pack that names no GPU vendor, is refused as before rev 17 (a GPU pack such as Windows
  ML needs its vendor spelled out). GPU alternates (a vendor) are `windows-x64` only in rev 17 (else 400).
  `PUT /v1/admin/run/{run}` checks that the alternate's build and runtime manifests name `when.platform`, and that
  the default build and runtime name one platform (else 400); still at most 4 alternates. A CPU alternate is stored
  (and shown) with its `platform`, a GPU alternate without one, as before rev 17.
- For a self-play or eval job and an agent of at least `ALT_RUNTIME_AGENT`, the lease carries: the first alternate
  **of the machine's platform** that it qualifies for (a GPU alternate: 9.4's conditions 1-7; a CPU alternate:
  conditions 5-7 and the build's `aw`), else the run's default when the default build's platform is the machine's,
  else nothing from that run for this machine (it gets other work). An agent older than `ALT_RUNTIME_AGENT` is
  `windows-x64` and is never offered a build of another platform. When the default is of another platform, the
  first alternate of the machine's platform that only its builds' `min_agent` keeps from this agent counts as the
  run's build for section 3's update rule (`{"update"}` when the channel offers that agent, else nothing from the
  run), as a default build's would.
- **Campaigns.** `PUT /v1/admin/campaign/{id}` takes `"alternates": [{"build_id", "when": {"platform"}}] | null`
  (at most 4 worker builds, one per platform at most, each of the campaign's game and profile, not the campaign's
  own build; `when.platform` required; migration 0013 `campaigns.alternates`; a PUT without them removes them). A
  machine gets the campaign's alternate of its platform (registered, not blocked for it, `min_agent` and `aw` met,
  listed in the campaign's current definition's `builds`), else the campaign's build when its platform is the
  machine's, else nothing from that campaign (an alternate that only its `min_agent` keeps from the agent counts
  for the update rule, as for runs). `GET /v1/admin/campaign/{id}` and the stats show them, each with `problems`
  (why the current definition doesn't authorize it).
- **Diag leases** (`POST /v1/admin/diag`): a request whose build (`params.build_id`, else `params.run`'s default
  build) is of another platform than a target machine's → 400. A diag lease never carries a build of another
  platform (diag leases go only to agents before `DEFINITION_AGENT`, all `windows-x64`).
- The coordinator checks the builds of every lease against the machine's platform again when it grants it (under
  the scope's lock): a build of another platform is never leased, whatever chose it.
- **The admin view.** Per run and campaign the stats (and `GET` run and campaign) add `platforms`: the platforms
  that can get its jobs (the default build's, and each alternate's that the current definition authorizes).
- **Results across platforms.** What the engine alone decides is identical on every platform (fuzz seeds, record
  replay). A network's outputs differ in rounding between ONNX Runtime's x64 and arm64 kernels, so a game that
  consults a network can play out differently on a Mac. Self-play records are unaffected (the trainer replays the
  moves, never the network: section 6.1 of DESIGN). A run or campaign whose games are compared across machines
  (eval traces and digests, section 5.3) gets no macOS alternate unless its checks allow it; the desktop tools add
  macOS alternates only where asked. A worker build refuses (`check-config`) settings whose replay would depend on
  the platform's libm (the engine's own rule; mtg-zero: the sealed deck builder until it uses a portable libm).

### 11.5 The macOS agent

- **Layout.** `~/Library/Application Support/ZeroFarm/`, the same tree as section 6 without `.exe`
  (`ZeroFarm`, `versions/<v>/farm-agent`; launcher leftovers `ZeroFarm.old-<n>`, `ZeroFarm.bad-<n>`, `ZeroFarm.new`).
  Autostart: the LaunchAgent `~/Library/LaunchAgents/com.zerofarm.agent.plist` runs `<root>/ZeroFarm` at login
  (`RunAtLoad`; no `KeepAlive`: the launcher restarts the agent itself, as on Windows; `ProcessType` `Interactive`, as
  launchd otherwise throttles an agent's CPU and I/O). Exit codes, `pending`/`healthy-<v>`/`bad-<v>`, the rollback and
  the launcher's own update (exit 76, `--takeover`, `launcher-swap`) behave as in section 6; a launcher's version
  (there is no VERSIONINFO) is a 32-byte marker in the binary: `\0ZFVERSION:` and its semver, NUL-padded. A
  launcher taking over runs in a session of its own (launchd ends the old launcher's process group with it).
- **Single instance and quit.** An exclusive `flock` on `state/<role>.lock` (`launcher`, `agent`), which holds the
  holder's pid (trusted only while the lock is held). "Quit" (`ZeroFarm quit`, uninstall, re-install) is SIGTERM to
  both holders; SIGTERM, SIGINT and SIGHUP quit the launcher and the agent (launchd sends SIGTERM at logout and on
  `bootout`; not a crash, there are no session-end codes); the launcher passes a quit on to its agent, and an agent
  whose launcher is gone (it was reparented) quits, as the job object's kill-on-close does on Windows.
- **Token.** `state/machine.json` (mode 0600, `state/` 0700) holds the token in the clear instead of DPAPI. The
  Keychain is not used: the agent's code identity changes with every self-update, so the Keychain would prompt.
- **Worker.** Its own process group; CPU work at nice 19 (never the background QoS / `PRIO_DARWIN_BG`, which
  confines a process to the efficiency cores); yield/resume = `SIGSTOP`/`SIGCONT` to the group; the memory cap and
  the rolling memory plan (10.6) read the group's physical footprint (`proc_pid_rusage` `ri_phys_footprint`, summed;
  there are no job objects): the agent reads it at least every 100 ms and kills a group past the cap, which then
  counts as running into the limit (its peak is past the cap). The group is killed when the agent exits; a worker
  whose stdin closes exits by itself; the agent records each worker (pid and start time) in its working directory
  and kills a group a dead agent left behind at its next start (a stopped worker can't see its stdin close). The
  runtime pack is passed as `ZEROFARM_RUNTIME_DIR` and first on `DYLD_LIBRARY_PATH`.
- **Rolling leases (10.6) on macOS.** "auto" on the CPU is 144 games in flight per physical core (performance and
  efficiency) in groups of 192 (16 cores: 2,304 in 12 groups); the memory plan's default cost (a worker build not
  measured here, 10.6) is 1,024 MB + 18 MB per game in flight, in physical footprint (agents from 0.3.3; 0.3.2 planned
  14 MB, 0.3.0-0.3.1 4 MB from the SSH self-play of 2026-11-18, about 2.7 MB per game with the process, read early).
  The numbers below are the builds of before; zero-core's native worker fixed on 2026-11-19 (fix/worker-memory:
  records kept sparse in flight, ONNX Runtime memory patterns off, a compact evaluation cache) measured 2.5 MB a game
  at 882 in flight after 12 minutes (13.1 before), and such a build is planned with what the Mac measures (10.6:
  about 4 MB a game with the margin). A real rolling lease on the M4 Max (2026-11-19,
  legacy-meta3-r1's network, fast export, one worker of 2,304 games in flight, a game lasting about 10 minutes) kept
  growing: 12.8 GB at 3 minutes, 19.0 at 5, 22.2 at 7, 25.2 at 9, 27.0 at 11, 28.1 at 13, 29.1 at 15 (12.2 MB a game
  over the base), still about 0.5 GB a minute and slowing, toward ~31 GB (~13 MB a game) at steady state; the memory
  check (10.6) restarted it at 1,728 and remembered 13 MB a game for the run. The first crew (13.5, agent 0.3.2, two
  workers of 882 games in flight planned at 14 MB) went further: the main worker committed 14,487 MB of its 15,746 MB
  part after ~27 minutes - 16 MB a game over the base, still rising slowly - and hit the 92 % trigger. With the default
  memory cap (half the RAM) the 0.3.3 plan holds 1,472 games in flight in one worker on a 64 GB Mac (32,768 MB),
  2 x 716 in two (13.5), 704 or 2 x 329 on a 32 GB Mac with 12 cores, 320 on a 16 GB Mac - about 76 % of the cap at
  16 MB a game, 85 % at 18. When memory caps "auto", the groups stay and the batches shrink (agents from 0.3.3:
  the cores' groups, batches of at least 96 rows - 1,472 in 12 groups of 123; 0.3.2 kept batches of 192 rows and so
  cut the groups, 1,888 in 9): the group count keeps the search threads fed (13), inference costs about the same per
  row whatever the batch.
- **When it works.** Input idle time from IOKit (`HIDIdleTime`); `on_battery` from the providing power source and
  `battery_saver` from Low Power Mode (`NSProcessInfo`); `low_memory` when memory pressure is at least "warning"
  (`kern.memorystatus_vm_pressure_level`) or free + inactive memory is below the floor; no fullscreen detection in
  0.3.0. While it holds a lease on AC power the agent keeps the Mac from idle-sleeping (an IOKit power assertion
  `PreventUserIdleSystemSleep`, as `caffeinate -i`; the display still sleeps), setting `keep_awake` (default on): a
  Mac otherwise sleeps whenever the user leaves. The setting `on_battery` (both platforms, default off) lets an
  owner's computer work on battery and in battery saver / Low Power Mode too; `work_when active` (the tray's
  "always") while it is in use.
- **Profile.** `os` (e.g. `macOS 26.1`), `cpu` (`machdep.cpu.brand_string`), `cores`/`threads`
  (`hw.physicalcpu`/`hw.logicalcpu`), `perf_cores`/`eff_cores` (`hw.perflevel0/1.physicalcpu`), `ram_mb`,
  `disk_free_mb`, `gpus: []`, `providers: ["cpu"]`, `platform`. No GPU work on Macs in rev 17 (Core ML later,
  behind the same probe and bench as section 9.6). The CPU threads a job gets: one per physical core (as on
  Windows; an M4 Max: 16, 12 performance + 4 efficiency), or the `cpu_threads` setting.
- **No tray in 0.3.0.** The agent runs headless; `ZeroFarm status | pause [minutes] | resume | set <key> <value> |
  quit | uninstall` work from a terminal (`<root>/ZeroFarm`; all but `quit` are forwarded to the current
  `farm-agent`). The running agent writes `state/status.json` (what the tray would show, the CPU threads, the
  settings); `pause`, `resume` and `set` are request files in `state/requests/` plus SIGUSR1 to the agent, which applies
  them as the tray does and answers in `<request>.done`; with no agent running the CLI changes `state/machine.json`
  itself. Keys of `set`: `cpu_threads` (N or `auto`), `work_when` (`idle` | `active`), `idle_minutes`, `on_battery`,
  `keep_awake`, `hours` (`HH:MM-HH:MM` | `off`), `max_ram_mb` (N or `auto`), `disk_cap_mb`. Without an enrollment the
  agent waits for one (`ZeroFarm setup --invite`) instead of exiting.
- **Install.** `curl -fsSL https://farm.greggg230.com/install-macos.sh | sh -s -- --invite CODE` (11.2). `ZeroFarm
  setup [--invite CODE] [--name NAME]` verifies the package it sits in, runs that package's `farm-agent setup`:
  install (the package must name `macos-arm64`), the LaunchAgent written, the enrollment (the invite code and name
  asked on `/dev/tty` when not given: `curl | sh` leaves stdin to the script), then `launchctl bootstrap gui/<uid>`
  (which starts the launcher). Uninstall: quit, `launchctl bootout gui/<uid>/com.zerofarm.agent`, the plist (only one
  that runs this root's launcher) and the root removed. A browser download (a `.pkg` or `.app`) would need Developer
  ID signing and notarization: later, the owner's decision.

## 12. Hardware routing (rev 18; coordinator migration 0014, no agent change)

Each kind of PC gets the work it is comparatively best at. Measured on the farm (2026-11-18, games per second):
self-play of a run with a CUDA version is ~9-14x faster on an NVIDIA PC than on an 8-core CPU-only PC, while an eval
batch (a lease lasts about as long as its slowest game) runs at ~90% of a recent NVIDIA GPU PC's pace on that CPU PC, and fuzz
uses no network at all. So: CPU PCs take eval and fuzz first and self-play last; GPU PCs take self-play first and eval
only when the CPU PCs can't keep up. Nobody idles: every rule changes the order, never what a PC may get.

The owner's switch (`POST /v1/admin/routing`, section 4; settings key `hw_routing`): `on` (default true; false: the
order of rev 17 exactly), `eval_horizon_s` (default 1800), `eval_games_per_thread` (default 2; independent of `on`).

### 12.1 CPU self-play is the last resort while the run has a GPU version

A run **has a GPU version** when it lists an alternate (section 9.4) for a GPU vendor the farm uses, or its default
runtime pack requires a GPU (`gpu` in its manifest, section 9.2). A default pack that only *may* use a GPU (DirectML)
counts by its alternates alone. A CPU alternate (rev 17, section 11.4: no `when.vendor`, e.g. a macOS build) is not a GPU version.

A PC **plays a run's self-play on its GPU** when its candidate is a GPU lease (an alternate it qualifies for, or a GPU
provider that passed with the default pack here; agents from 0.2.10) and its `caps.gpu_games[run]` isn't 0 (its bench
found no GPU that pays for this run's network).

On a PC that doesn't, the self-play of a run with a GPU version moves to **tier 3**: after fuzz (tier 2). With no fuzz
or eval for the PC, it still plays the self-play. A run without a GPU version is unchanged (CPU PCs play it in tier 1).
A rolling lease (section 10.4) of such self-play ends at renewal when fuzz waits, like any lower tier.

### 12.2 Eval on a PC that plays self-play on its GPU

Such a PC records it (`machines.gpu_play` = 1, written on each offer when it changes; every other PC 0). For it, an
eval batch in tier 1 that may run on the CPU (`gpu` not `required`) is **deferred** unless it is **behind**:

  unissued games > (sum of the CPU PCs' eval rates) x `eval_horizon_s`

where the CPU PCs are the machines with `gpu_play` 0, seen in the last 300 s, not paused or revoked, without a fresh
presence (section 3, rev 16), not the asking PC; each counts its measured eval rate for that batch (ids per running
second, PROTOCOL 3), else its average over every eval it played, else nothing (no CPU capacity: behind while anything
is unissued). A deferred batch ranks after the PC's self-play (rank 1.5: still tier 1, before fuzz; never earlier than
a background share puts it, section 5.3), so the PC gets it when its self-play has no headroom. On a rolling lease's
renewal a deferred batch counts only while the lease's own generation has no headroom (the lease would only wait).
`eval_horizon_s` 0: never deferred (eval before self-play everywhere, as before).

The horizon is the latency the owner accepts: GPU PCs help a batch until what is left would take the CPU PCs at most
that long, so a batch takes at most about `eval_horizon_s` longer than with every PC on it.

### 12.3 CPU eval lease floor

An eval lease takes about as long as its slowest game, so a CPU lease of a few games measures a low rate and its
`auto` size (450 s of that rate) stays small. An `auto` eval lease that is not a GPU lease gets at least
`eval_games_per_thread` x `caps.threads` games, no more than the agent's RAM holds (`(caps.ram_mb - 600) / 45`,
section 10.6's CPU plan) and within the eval limits (2..1024); a set `games_per_lease` is exact, as before. 0: no floor.

## 13. Layout tuning (rev 19; coordinator migration 0015, agents from 0.3.1)

**Why.** One CPU layout doesn't fit every machine, and a wrong one costs a lot. On the M4 Max (12 performance + 4
efficiency cores, 64 GB, 2026-11-18, legacy-meta3-r1's gen-398 network, clean 5-minute windows, leaves/s; the owner's
own 3-process setup of the same network 19,035-20,034, 19,504 in a later control) the native worker as agent 0.3.0
lays it out (16 search threads, 5 inference sessions x 3 threads, 2,304 games in flight in 12 groups of 192 rows)
played 10,977-11,086 (12,292 with a fused-attention export), and that was the best measured region: 6 groups (384-450
rows) 11,155 (12,560), 4 groups (~675 rows) 8,970 (too few groups starve the search), 4 x 4 inference threads 6,596,
24 + 8 x 2 7,707, 36 + 12 x 3 3,475. More threads in any form hurt; inference cost per row is about flat in the batch
size. An 8-core MacBook Air, a Windows desktop and the next network each have their own sweet spot, so the agent
measures it on the machine, inside real work, near the platform's known-good start, and remembers it. One process is
also a ceiling of its own: two worker processes of the same network on the M4 Max played 16.4k (+30 %; 13.5), so the
number of worker processes serving a lease is part of the layout. A GPU job too (agents from 0.3.4): on the 4070 Ti PC
of 2026-11-19 (4 cores / 8 threads; legacy-meta3-r1's gen-769 network, the CUDA build, 5-minute windows, leaves/s) the
job as agent 0.3.3 ran it - one worker of 128 games in flight in 2 groups, 6 search threads - played 5,030 with the GPU
37 % busy; one worker of 256 in one group 8,379, 512 9,803, 1,024 10,825 (2 groups: 10,697) - where one worker levels off
(one ONNX Runtime call at a time per process) -, two workers of 512 (3 search threads each) 12,529 with the GPU 73 %
busy, three of 384 (2 each) 14,154 at 82 %. Everything below is additive: an agent without the feature, a coordinator
before rev 19, eval, fuzz and classic jobs are unchanged; GPU jobs are tuned from agent 0.3.4 (13.1).

### 13.1 What is tuned

- The CPU job of a rolling self-play lease (section 10) whose worker counts `leaves`, `games` and `games_started` in
  its `status` (one that doesn't is run as before for the rest of the lease). Its **layout** is the number of worker
  processes serving the lease, `workers` (13.5), and five job-line tuning fields (section 5) each of them gets:
  `threads` (search threads), `infer_workers` (ONNX Runtime sessions, each on an evaluator thread of its own),
  `infer_threads` (threads per session), `groups` (batches in turn: rows per batch = `conc` / `groups`) and `conc`
  (its games in flight; the job's are `workers` x `conc`). The agent sends all five; a worker that doesn't know
  `infer_workers` ignores it (one session of `infer_threads`).
- **Bounds** (caps, never searched upward: oversubscribing collapses the native worker, above). Search threads and
  inference threads (`infer_workers` x `infer_threads`) each at most the physical cores - never more threads than the
  layout of before -, or the owner's `cpu_threads` when set (as the worker's default `infer_threads` = `threads`
  always did); with N > 1 workers each worker gets floor(1.5 x that cap / N) of each (13.5). Sessions vary only for a
  worker whose `hello` lists the feature `infer_workers`, and on macOS (every macOS worker build has it). Rows per
  batch: Windows at least 8, macOS 96 to 384 (where the games in flight allow; 192 to 384 in agents 0.3.1-0.3.2). Games in flight (all workers'): a
  run's number (`rolling.inflight.cpu`, 10.1) is kept; under `"auto"` from half 10.6's / 11.5's choice up to it (more
  would only make games longer). Workers: 13.5.
- **Start.** The coordinator's hint (13.4) for a key not tuned here yet, else the platform's default, which is the
  layout of before: one worker; one search thread per physical core; Windows one inference session with as many
  threads (the worker's own default), macOS sessions of 3 threads (16 cores: 5 x 3; under 6 cores one session); 10.6's /
  11.5's games in flight and groups (Windows 32 in 4 groups, an M4 Max 2,304 in 12 groups of 192). A Mac with at least
  12 physical cores and 32 GB starts with two workers of 12 search + 4 x 3 inference threads (16 cores; the budget of
  13.5), 1,152 games in flight in 6 groups of 192 - the M4 Max's measured best -, each within its part of the memory
  plan (13.5 **Memory**: 716 games in flight each on a 64 GB Mac at the default cap, agents from 0.3.3; 0.3.2 started
  2 x 921, 0.3.1 2 x 1,152, which the cap doesn't hold at the measured cost, 11.5). Several workers share the job's
  groups (agents from 0.3.3: the group count is kept where memory cuts the games, the batches shrink - 6 groups each
  for two on 16 cores, 116-120 rows at 691-716 games in flight; 0.3.2 kept the rows and ran 4 groups, the slow region).
- **GPU jobs** (agents from 0.3.4). The GPU job of a rolling self-play lease (a provider other than the CPU won 9.6's
  choice) has a layout too: `workers`, and each one's `threads` (search), `groups` (1 or 2) and `conc`; its inference
  threads are the worker's own - the job line carries no `infer_*` (one session, `infer_threads` = `threads`; layouts
  and reports say `infer_workers` 1, `infer_threads` = `threads`). **Bounds:** workers 1 to 3, at most one per physical
  core, the owner's `workers`, and as many as the memory plan (10.6, each worker its own base) and the video memory plan
  (10.6 **Video memory**: each worker's base and biggest batch within its share) hold with the fewest games in flight
  allowed; search threads: the owner's `cpu_threads`, else 1.5 per physical core (at most the logical processors), shared:
  each of N workers gets 1/N of them (the 4-core PC: 6; one worker 6, two 3, three 2, as measured; the trainer's own torch
  path there found 6 faster than 8); games in flight (all workers'): `"auto"` from half 10.6's target (two full batches)
  to 1.5 times it (agents from 0.3.7: twice it), a run's number kept; batches up to 2,048 rows. **Agents from 0.3.7:** a
  worker of at least 512 games in flight runs them in two groups even when one batch would hold them all (its search
  threads play one group while the other is on the GPU; with one group they waited for every batch): the 4070 Ti PC
  (fp16 network, 2026-11-20, leaves/s) - 3 x 575 in one group 22.3k (search threads ~60 % idle), 3 x 768 in two 24.6k,
  3 x 1,150 in two 28.0k, 2 x 1,536 in two 27.0k; the trainer's own path there 32.8k. **Start** (no coordinator hint: a report doesn't say
  whether its layout ran on the GPU or the CPU of the same build and pack): on an NVIDIA GPU the most workers within the bounds whose start holds the target in all
  (each its share rounded up to a multiple of 64 - 1,024: 1 x 1,024 in two groups of 512, 2 x 512, 3 x 384 - in batches
  of at most the full batch, and its share of the search threads), else one; on other GPUs one (the tuner tries more). A
  layout the memory plan held back is lifted to its share of the target, not past it (10.6). Agents from 0.3.5: a
  start's groups are the rule's for the games in flight the memory plans leave (139: one batch of 139; 0.3.4 kept the
  target's 2 groups, 70 rows), and while a search measured nothing but the start (no trial yet) a calibration that
  changes the plan (10.6) picks the start again - the 4070 Ti PC: one worker of 139 at 45 MB a game, two of 512 at 4 -,
  and a crew starts when it holds 80 % of the target in all (two of 506 at a 9.0 GB cap; 0.3.4 asked for all of it).
  Agents from 0.3.6 also pick the start again when the new plan holds more workers than the layout wanted has (the
  search then goes on from it, its measurements kept): the PC's 0.3.5 lease had measured a one-worker candidate first
  and stayed one worker of 1,024. `autotune` off: the start
  for the owner's `workers` (else one), fixed (handed over only when the memory plan holds more, 10.6). A CPU job on a
  GPU runtime pack takes no hint either (the hint for that build and pack is most likely a GPU layout).
- **The owner's settings** `autotune` (default on; Windows `machine.json`, macOS `ZeroFarm set autotune on|off`) and
  `workers` (`auto` (default) | 1 | 2 | 3, 13.5). `autotune` off: the fixed rules of before (one worker, `threads` = the
  CPU threads, no `infer_*` fields, 10.6's games in flight) - unless `workers` is 2 or 3: then that many workers with
  the platform's start for them, fixed (no trials, nothing remembered; handed over only when the memory plan comes to
  hold more games in flight, 10.6, agents from 0.3.3).

### 13.2 Trials

- **When** (CPU and, agents from 0.3.4, GPU jobs alike; GPU work blocked by section 8's rules is a pause). A trial
  starts only while the machine is **idle** (no input for `idle_minutes`, whatever `work_when` says),
  on **AC power** and not in battery saver / Low Power Mode (whatever `on_battery` says), with **no fullscreen app**
  (whatever `respect_fullscreen` says), nothing else blocking work (paused, outside the hours, low memory) - the
  owner's rule, strict -, and **quiet**: other programs used at most 10 % of the CPUs on average over the last trial
  length (`tune_warm` + `tune_window`, 95 s; the system's CPU time minus the agent's workers', sampled every 2 s; agents
  from 0.3.3 - 0.3.1-0.3.2 judged every sample, and a Mac's bursty background daemons, a sync daemon at ~55 % of a core
  and syncthing at ~90 %, discarded every trial within seconds) and the worker isn't starved. Real work may go on
  outside that (an owner's `work_when active`, `on_battery`); trials don't, and the job runs the incumbent layout.
- **Handoff** (how a layout changes inside a lease, no game lost). The main worker gets `finish`. After 1 s the agent
  pings it until two `status` answers give the same `games_started`: its ids stop at its `gid_base` + that. The next
  worker's job line takes the rest of the released ids from there (`gid_base`, `games`; `extend`s go to it from then
  on); with no such answer within 10 s it takes fresh ids after the released ones (the old worker's unstarted ids go
  back to the pool when the lease closes). It starts beside the old one as soon as its plan (10.6's base + `conc` x
  the cost per game; a measured crew's: every one of its workers') fits 85 % of the job's limit beside what the lease's
  workers hold or will hold - every worker playing new games at the more of its commit and its plan (one that just
  started commits little and grows to its plan), every worker finishing its games at its commit (agents from 0.3.3;
  0.3.1-0.3.2 counted commits only, and a Mac's crew ended up holding 38.9 GB against a 31.7 GB cap) -, else once the
  old one ended (as 10.6's restart, without counting toward its 4), and then too only when its plan fits beside the
  workers still finishing their games (else it waits for their drain). The old worker - a **drainer** - finishes its
  games in flight: its results upload as any, nothing else is asked of it, it is stopped (SIGSTOP / NtSuspendProcess)
  while the main worker is measured or work is paused, and killed when silent 90 s while running. **One drain at a
  time** (agents from 0.3.3; before: 2 drainers per worker of the crew): no handoff starts while any worker of the
  lease finishes its games (a handoff's, a crew member's at the end of its range or after giving way to memory, a
  restart's). A handoff never goes to the layout the main worker runs unless to measure it (a measurement needs a
  fresh worker), and at most 6 handoffs start per hour. **Agents from 0.3.5:** a GPU job's measured trial starts only
  when its workers fit beside the lease's workers as the handoff leaves them (every one finishing its games, at its commit and
  video memory, 10.6) - one that would wait for a whole drain doesn't start (the 4070 Ti PC's old worker drained alone,
  starting no game, for 6-8 minutes before each trial with 0.3.4): the layout wanted runs unmeasured instead (its first
  worker beside the old ones when it fits, else after their drain; its others join as memory allows), any other trial
  is postponed - counted as that layout's discard
  and against the hour's 6 for trials (postponed trials and handoffs together) -; whether a trial can be measured is
  decided when its worker starts, not while the handoff waits, and a trial that can't is counted as discarded whatever
  layout it was (0.3.4 swapped a candidate for the incumbent at any tick of the wait and counted nothing: the same
  candidate was handed over to again 20 s later, three times, measuring nothing - CPU and GPU jobs alike; a CPU job's
  trial still starts after the drain when it doesn't fit beside); nothing is written to a worker that
  ended (a handoff's ping after a drainer's `end` failed with "the pipe is being closed"). **Agents from 0.3.6:** a
  worker finishing its games whose memory the next worker waits for - a handoff's next worker, a restart, a crew's next
  member, when it would fit without the workers finishing theirs - ends its last games (`quit`) once it has finished
  for 2 minutes or only an eighth of its games in flight (at most 4) are left; their ids go back with the lease (`done`'s
  unfinished ids are worked out from the credited ones: each id credited or returned, none played twice). A drain ends
  in its longest games at a few rows a batch: the 4070 Ti PC's start drained for 303 s beside the first member of its
  crew, the second waiting for its memory all along (a safety net; 0.3.2 handed a job over to the same layout 3 times
  in 7 minutes). A trial that can't be measured any more when its worker would start (the machine got busy or used):
  a candidate gives way to the incumbent; the incumbent's own measurement starts unmeasured and counts as discarded.
  **Agents from 0.3.7:** a GPU job's measured trial is handed over to as a CPU job's - its workers after the old
  workers' drain when they don't fit beside them (0.3.6 bounds that drain) -, nothing is postponed: postponed, a
  memory-tight PC measured nothing (the 4070 Ti PC's crew of 3 x 575 held its whole limit; its own measurement - the
  incumbent, handed over to unmeasured - never fit beside it, 0 trials for the lease). Its trials wait for the memory
  plan's early reading (10.6; a cost remembered from an earlier lease counts), at most 15 minutes from the lease's start
  (a worker whose commit can't be read: no wait): trials under the platform's default cost measured single workers of
  ~140 games, each restarting the reading. A crew member killed with its lease (cancelled, stopped) is logged as
  stopped, not failed.
  The lease closes (`done`) once every worker ended, its `unfinished` worked out from the credited ids (as after a
  restart).
- **Measurement.** Only of workers that started fresh with the layout (a lease's first worker is one: when a trial
  may run then, it is the next layout to measure; with several workers their counters are summed - every member of the
  crew since the measurement started, one that finished its range until it ends (a rotation is the crew's normal work,
  13.5) -, and a member failing during the trial discards it). Leaves per second from its `status` (pinged every 2 s), over a window
  at the same **game progress** for every layout: from `p0` to `p1` leaves per game in flight, interpolated between
  samples. The search's first measurement sets the window: once the games in flight refilled (`games_started` - `games`
  >= 98 % of `conc`), 20 s more, then 75 s. Why not a fixed time: a fresh worker starts every game at once and slows down
  as they grow (rolling_measure 2026-11-16, 7700K, 32 in flight: 855 leaves/s in its first 20 s, 520 at 7 min, 420 at
  18 min); compared at the same leaves per game, 32 vs 16 in flight read 1.32 and 64 vs 32 1.20 (steady state 1.30 and
  1.23), at 30-120 s of wall time 1.42 and 1.26. A trial that doesn't reach `p1` in 4 x (20 + 75) s (more for more games
  in flight) counts at its rate so far; one that never refills or measures nothing doesn't count.
- **Discarded, never paused.** When during a trial the machine stops being idle, goes on battery or into Low Power Mode,
  shows a fullscreen app, work is paused or blocked, the worker is starved, other programs' load is sustained (more than
  25 % of the CPUs on average over the last 30 s, 0.4 x `tune_window`) or the lease ends, the trial is discarded at once;
  and a trial that measured counts only when other programs used at most 10 % of the CPUs on average over its window
  and at most 5 points more than on average over the trial length before it (load that came with the trial would bias
  it; short bursts pass, agents from 0.3.3). A discarded trial: the worker goes back to the incumbent layout (a
  handoff), and the trial is tried again at a later idle period (after the incumbent is back and at least 2 x (20 + 75)
  s later); a layout discarded 3 times is skipped. Every measurement records other programs' mean load over it
  (`state/tuning.json` `measured[].load`, 0.3.3).
- **The search.** Coordinates in order: workers (one; the job's games in flight and rows per batch kept, each kind of
  thread rescaled to its budget for the new count, 13.5), batch size (two thirds of the groups / half again as many),
  search threads (an eighth of their cap), threads per inference session (one), sessions (one, their threads kept),
  games in flight (each worker's halved / doubled within its part of the job's bounds, rows per batch kept; "auto" only;
  0.3.1-0.3.3 clamped a worker's to the job's bounds, so a crew's move went nowhere). A GPU job (agents from 0.3.4) has
  no inference coordinates; its batch move is one group <-> two. A thread move past a cap is no candidate (never
  cut back into another shape); batch, games-in-flight and worker moves stay within bounds. The incumbent's first unmeasured neighbour is
  tried, the move that made the incumbent first. A layout replaces the incumbent when it is at least 3 % faster - and
  (agents from 0.3.3) was not measured with more than 5 points less load from other programs than the incumbent (the CPU
  it didn't share is no layout's merit; likewise in the final re-measure and the weekly check). At most 9 candidate
  trials; then an incumbent that isn't the start is measured once more, and kept only if its mean is still 3 % over the
  start's (a lucky single window doesn't stick).

### 13.3 Remembered and re-checked

- `state/tuning.json`, per key (worker build, runtime pack, network, hardware - with the owner's `workers` when set; a
  GPU job's (agents from 0.3.4) also the GPU - vendor, name, dedicated memory, driver - and the provider -, OS version,
  tuner version 2): the start,
  the incumbent, every measurement, the window, the search's state. The network's fingerprint is the first 16 hex of the
  sha256 of the job template's canonical JSON without its per-lease fields, `|`, the model file's size in MiB (a run's
  generations keep it; another architecture or other settings change it); the hardware's is the CPU brand, cores,
  logical processors, performance and efficiency cores and RAM in GiB. At most 16 keys (the least recently changed go).
- A key not seen is searched; a tuned key's leases start with its incumbent and run no trial, except a weekly check:
  the runner-up (the best other layout measured) and the incumbent measured afresh, the runner-up taken at 3 % faster.
- **Compared as they run** (agents from 0.3.3). The remembered layouts - start, incumbent, measurements, discards - are
  compared as the lease's bounds (13.1: the memory plan, the owner's settings, the worker's features) make them: a
  layout the memory plan no longer holds is its fitted one, and that one's measurements and discards are its own. The
  entry keeps the bounds it was last used in (`space`) and its layouts as they were (a lease with more memory runs the
  bigger one again); reports (13.4) carry the incumbent as it ran. 0.3.1-0.3.2 compared the stored layouts: a Mac's
  entry kept an incumbent of 2 x 958 games in flight planned at 13 MB a game, the next lease's plan (16 MB) held 2 x
  778, and the fitted incumbent's measurements and discards were never the incumbent's - it stayed "unmeasured" and
  the job was handed over to the same layout again and again.
- `ZeroFarm status` (macOS) and the tray (Windows) show the job's layout (with several workers: "2 workers x (...)" and
  how many run) and the search's state; `ZeroFarm status` also the `workers` setting.

### 13.4 Reports and hints (wire)

- `POST /v1/lease`: `features` adds `"tuning"`; the optional `tuning` is at most 4 reports, the most recently changed
  keys with a measured incumbent: `{"run", "build", "runtime": build_id | null, "net": 16 hex, "layout": {"workers":
  1..8 (optional, absent: 1), "threads": 1..512, "infer_workers": 1..64, "infer_threads": 1..512, "groups": 1..4096,
  "conc": 1..4096}, "leaves_s": 0..1e9, "trials", "state": "tuned" | "searching", "t"}` (`groups` <= `conc`; the
  per-worker fields are each worker's; anything else → 400). The coordinator stores the
  latest non-empty one per machine (`machines.tuning`, `tuning_t`; migration 0015); absent or empty keeps it.
- A rolling self-play lease to an agent that lists `"tuning"` carries `tuning_hint` `{"layout", "leaves_s", "machines",
  "net"}` when a machine of its hardware class (`class`, section 3) and platform reported its worker build and runtime:
  the lease's run first, tuned before searching, then the highest rate; `machines`: those reporting that build,
  runtime and network (revoked machines left out, the asking one included). Unsigned and only a starting point: the
  agent checks every bound, fits it to its own budget and memory (its worker count too), and uses it only to start a
  key it has no result for. The layout keeps `workers` when the report had it. Agents from 0.3.4 report GPU jobs' layouts
  too (their runtime pack tells them apart; no wire change) and use no hint for a GPU job, nor for a CPU job on a GPU
  runtime pack.
- Admin: the stats add per machine `tuning` (its report) and `tuning_t`, and `tuning`: per class, platform, build,
  runtime and network the best layout, its rate and state, the machines and runs. The admin page shows "Tuned layouts
  by hardware class" and "Tuned layouts by machine" (a crew's layout as "2 workers x (...)").
- Lease stats (beats, `done`) of a rolling lease served by several workers add `workers` (how many play new games) and
  `inflight` is all of theirs.

### 13.5 Several workers per lease

**Why.** A single native worker process caps out: on the M4 Max (2026-11-18, clean windows, leaves/s) one process
played at best ~12.6k with the old export (~18.7k with the new fast export, in another session) and more threads inside
it always hurt, while two processes of 12 search + 4 x 3 inference threads with 1,152 games in flight in 6 groups each
played 16.4k with the old export (+30 %) / ~18.0k with the CUDA-safe `fast` export - the owner's SSH self-play's parity
in that session (3 Python processes, ~18k) -; three 16.1k, four 15.2k. The
processes don't share a tree arena, an evaluation cache or a runtime; what costs is contention inside one process, not
the thread total. Memory: ~21 GB for 2,304 games in flight across two processes (~9 MB per game with the process,
read during a short trial; a real lease grows to ~13 MB a game, 11.5).

A GPU job's single worker caps out too (agents from 0.3.4): one ONNX Runtime call at a time per process left the 4070 Ti
PC's GPU 45-56 % busy at 1,024 games in flight (10,825 leaves/s); two workers of 512, each its own CUDA session on the
GPU, played 12,529, three of 384 14,154 (GPU 82 % busy, +180 % over agent 0.3.3's 5,030).

**One lease, a crew of workers.** The agent serves one rolling self-play lease (section 10) - a CPU job, and from agent
0.3.4 a GPU job, each worker with its own session on the job's GPU - with `workers` worker
processes (1 to 3; 13.1, 13.2), all of the same worker build, network and per-worker layout. No coordinator change and no
extra lease: the coordinator sees one lease, its released ids, its extensions and its result chunks, as before.

- **Ids.** A worker's ids are one range `[gid_base, gid_base + games)` that only `extend` raises (10.5), and the
  coordinator releases ids only at the end of the lease's released ids (10.4). So one worker - the main one, the
  **tail** - owns the range ending at the released ids' end and gets every `extend`; every other member owns a
  **closed** range of released ids (its job line's `gid_base` and `games`, never extended). The ranges never overlap.
  - A member joins by taking the tail's place: the main worker keeps its range (every id released so far from its
    first) as a closed one - only once at least 2 x its games in flight of it are unstarted (its `status`
    `games_started`); else the agent asks for more ids first -, and the new worker starts at the end of the released ids
    with `games` 0 (its `low` asks for ids) and gets the extensions from then on.
  - Or it plays a **spare** range: released ids a worker told `finish` early never started (where its ids stop: two
    equal `games_started` reads after `finish`, as in a layout handoff, 13.2; or the `unfinished` of its `finish` end),
    when the range holds 2 x its games in flight (a shorter one goes back with the lease).
  - A member whose `games_started` reached its range's length is told `finish` (it starts no game; its games in flight
    complete) and a new member joins, when every worker's committed memory plus the new one's plan (10.6's base + its
    games in flight x the cost per game) fit 85 % of the job's limit. One member joins at a time (after the previous
    one's `hello`).
  - Extensions ask for about 2 x the crew's games in flight (10.4).
  - `done`'s `unfinished` is worked out from the credited ids (as after a restart, 10.6): every released id is credited
    or goes back, none is played twice.
- **Results.** Every worker has a results directory of its own; its `records` are queued as the lease's next chunks
  (`seq`), each with its own `x-ids` and `x-gen` (a worker's `gen`, else its own floor).
- **Generations.** `checkpoint` (10.6) goes to every member that has its job (one without gets the network in its job
  line; a finishing one keeps the network it has).
- **Pauses.** A pause yields the main worker and suspends every other (SIGSTOP / NtSuspendProcess: CPU jobs, no VRAM
  to free); resume resumes all. A GPU job's (agents from 0.3.4) every worker with its job - playing its range or finishing
  its games - is yielded first (`yield`: its session and its video memory released) and suspended once it answers
  `yielded`; one that doesn't within 2 s is killed (a failure, below); resume: `resume` after the process runs again. A
  worker finishing its games while the next crew is measured is only suspended (13.2). Every worker of the lease is the
  lease's own for section 8's GPU busy rule. `finish` (the lease ending, 10.6), `quit` and kills reach every worker; the
  lease closes (`done`) once all ended.
- **Memory** (10.6 for the crew). Each worker's own limit (job object on Windows, the physical footprint cap on macOS)
  is the job's limit / `workers`. A member at 92 % of its part finishes its games (its unstarted ids become spare) and the
  crew's next members run fewer games in flight (10.6's restart size; a restart, at most 4 per lease); the main
  worker's own memory is watched as in 10.6. Every worker of the lease counts toward the job's limit - those finishing
  their games too (agents from 0.3.3; before, only while none finished): at 92 % of it in all, the biggest worker
  playing new games gives way - the same way when it commits more per game than planned, else it just starts no new
  game (its unstarted ids spare; the main worker's successor takes the released ids it never started, the same games
  in flight; no restart counted, nothing shrunk) and the crew refills as the drain makes room; once nothing plays new
  games, none start until memory frees. A new member joins, and a restarted main worker starts, only when its plan
  fits 85 % of the job's limit beside what the others hold or will hold (13.2 **Handoff**). The most any worker
  committed per game is what is remembered for the run and worker build, and the lease plans with it from then on
  (10.6, self-calibrating). A GPU job's video memory likewise (agents from 0.3.4; 10.6 **Video memory**): each worker's
  base and biggest batch within its share of the budget, a new member only when its plan fits, at 95 % of the adapter in
  use the biggest worker gives way. Every layout - the
  start, a fixed `workers`, every trial - holds each worker's base and games in flight within 85 % of its part of the
  job's limit (each worker's games in flight bounded by it; the tuner's range of games in flight is kept), and the
  tuner's worker count is at most what that holds with the fewest games in flight allowed.
- **Failures.** A member that exits without `end` or with a fatal error (or stays silent 90 s, or sends no `hello` in
  10 s) loses its games in flight - they go back with the lease - and is reported once per lease (`/v1/crash`); a new
  member joins while restarts are left, then the crew runs smaller. One that ran into its memory limit counts as in
  10.6 (the crew's next members smaller, the cost per game remembered). The main worker failing (not of memory) while
  members play is replaced the same way: a new tail after every released id. A self-test failure stays fatal for the
  lease.
- **Tuning.** `workers` is the search's first coordinate: 1 to 3, at most one per 4 physical cores and per 16 GB of RAM
  and as many as the memory plan holds (each worker's base and the fewest games in flight allowed), or the owner's
  `workers` (a GPU job: 13.1 - at most one per physical core, within the video memory plan too). Per worker each kind of thread (search, inference) has floor(1.5 x the cap / `workers`) for N > 1, the cap
  for one (2 x 12 on 16 cores). A trial of a crew starts only with memory for all of it; its workers join one after
  the other; the measurement sums every member's counters through rotations (13.2). A layout handoff (13.2) tells
  every member `finish` with the main worker; the next crew's first worker takes the main worker's unstarted ids and
  the others' become spare.
- **Cost.** A member's closed range lasts one to a few game lengths of its own (about 1-3 h on the M4 Max at the start's
  layout), then it finishes its games beside its successor. A worker op handing a running worker a new range would let
  every member run as long as the lease (an aw/1 change); not needed for the measured gain.

## 14. Model variants per generation (rev 20; coordinator migration 0016, agents from 0.3.5)

**Why.** A run's GPU machines may play a generation with a graph made for them - a fixed-shape graph for CUDA graphs,
a reduced-precision one - that CPU PCs and Macs can't load or would play worse with. A **variant** is another model
file of the same generation and the same network (the same trained weights, exported another way), signed in the
definition beside the gen's default model and named by a run alternate (9.4, 11.4): a machine that gets that alternate
gets the variant, every other machine the default. Everything below is additive: a run without variants, a coordinator
before rev 20 and every agent before 0.3.5 are unchanged. `VARIANT_AGENT` (coordinator var, default `0.3.5`) is the
first agent that knows variants.

Variant names: `[a-z0-9][a-z0-9-]{0,31}` (a name ends up in a file name), at most 8 per gen. The first one: `gpu`, a
fixed-shape `zero-onnx/2` sibling of the gen's graph for the run's CUDA alternate (zero-core's export and worker; the
worker tells the graphs apart from their metadata, so neither the agent nor aw/1 changes).

### 14.1 In the definition (`definition/1`, an optional key)

```json
"checkpoints": {"81": {"model_sha256": "<hex>", "model_size": 13421772,
                       "reference_sha256": "<hex>", "reference_size": 412000,
                       "variants": {"gpu": {"model_sha256": "<hex>", "model_size": 14680064}}}}
```

- `variants`: optional; when present, an object of 1 to 8 variants (an empty object is refused: leave the key out for
  a gen without variants). Each name as above; each variant exactly `model_sha256` (64 lowercase hex) and `model_size`
  (1 byte to 1 GiB), no other key. The key order is free (the signed bytes are what count).
- A variant has no reference set of its own: the gen's `reference_sha256` / `reference_size` serve every variant (the
  reference rows and torch's answers don't depend on how the graph was exported). The per-machine self-test runs the
  variant graph against them (the worker's tolerance for a reduced-precision graph is the worker's: section 8).
- **Older agents.** `definition/1` refuses unknown fields in agents before 0.3.5, so they can't read a definition in
  which any gen has `variants`: they would refuse every lease of it (`refused: "definition"`). Therefore:
  - the coordinator counts a run's current definition with any `variants` as `min_agent` 0.3.5 (`VARIANT_AGENT`) for
    every lease of the run - self-play and eval, default build or alternate - with section 3's update rule (`{"update"}`
    when the machine's channel offers 0.3.5, else nothing from that run: the machine gets other work);
  - the desktop sets the run's own `min_agent` to at least 0.3.5 (`PUT /v1/admin/run`) before it registers the run's
    first definition with variants (zero-core's `zero.farm_def` and pool remote, 14.5), so the run's configuration says
    so too;
  - a lease granted to an older agent before keeps running: a classic one never reads a newer definition; a rolling one
    gets the new definition with its next generation (10.6), can't read it and finishes gracefully (its games in flight
    complete, one crash report `definition` for the lease), and its next `/v1/lease` gets `{"update"}` or other work.
  - Rollout: agent 0.3.5 on canary, validated, then stable (the owner's OK), before any live run registers variants.
- `farm-release define` checks the same (`common/src/definition.rs`).

### 14.2 Registering a variant (desktop)

`PUT /v1/admin/run/{run}/checkpoint/{gen}` takes an optional `"variants": {"<name>": {"model": "ckpt/<run>/<gen>/model-<name>.onnx",
"sha256": "<hex>"}}` beside `model`, `sha256`, `reference`, `reference_sha256`:

- Each `model` is a key under `ckpt/<run>/` (by convention `ckpt/<run>/<gen>/model-<name>.onnx`), uploaded first like the
  model (`PUT /v1/admin/artifact`), its sha256 checked the same way (R2's checksum when the upload carried one); its size
  is R2's. At most 8 names; a key used twice, or the gen's own model, → 400.
- A variant the current definition already signs for that gen must be exactly those files (sha256 and size), else 400 -
  as for the model (rev 6).
- **Adding to a registered gen.** The same gen again with the same default files may add variant names (a run that
  switches variants on can add one to its current gen); a name registered before with other files → 409 (files never
  change); a PUT that leaves out a registered name keeps it (variants are never removed). Adding variants doesn't make the
  gen current or reset its quota. `unchanged` is true when nothing was added.
- The answer adds `"variants": ["gpu", ...]` (every name the gen has now) when the gen has any. `GET
  /v1/admin/run/{run}`: `checkpoint` (the current gen) adds `variants` (`{"<name>": {"model", "sha256", "size"}}`, `{}`
  without), and `variants` lists every gen that has any (`{"<gen>": ["gpu"]}`).
- Migration 0016: `checkpoints.variants` (JSON `{"<name>": {"model", "sha256", "size"}}`, NULL: none) and `leases.variant`
  (the variant the lease was granted for, 14.3; NULL: the default).

**Registering a definition** (`PUT /v1/admin/definition`, rev 6) adds: every variant the definition signs for a gen the
run registered, under a name the run registered for that gen, has the registered sha256 and size (else 400, as for the
model). A variant signed but not registered (yet), or registered but not signed, is no error: until both agree for a
gen, machines get that gen's default model (14.3).

### 14.3 Which machines get a variant

- **The alternate.** `PUT /v1/admin/run/{run}` `alternates[]` takes an optional `"model": "<variant name>"` (9.4, 11.4; a
  GPU or a CPU alternate). Only the name's syntax is checked there (400): variants arrive per gen, so whether a gen has
  one is decided per lease. The run's default build never names one: machines that get the default get each gen's default
  model. `GET /v1/admin/run/{run}` and the stats show it in `alternates`.
- **The lease.** A self-play or eval lease that carries an alternate with `model` (section 9.4's choice, unchanged: the
  variant plays no part in which alternate a machine gets), to an agent of at least `VARIANT_AGENT`, carries for each
  gen it names the variant instead of the default model when all hold:
  1. the gen has the variant registered (14.2);
  2. the lease's definition signs that variant for the gen with the registered sha256 and size.

  The checkpoint is then `{"gen", "model": <the variant's key>, "sha256": <the variant's>, "variant": "<name>",
  "reference", "reference_sha256"}` (the reference is the gen's); otherwise the gen's default, exactly as before (no
  `variant` key). Where a checkpoint reaches an agent:
  - a self-play lease's `checkpoint`;
  - each of an eval lease's `checkpoints[]`, on its own (a batch's gens may differ);
  - a beat answer's `checkpoint` - a classic self-play lease's newer gen that its own definition signs (section 3), a
    rolling lease's in-place switch under the current definition (10.6) - with the lease's variant: `leases.variant`, the
    alternate's `model` when the lease was granted (to an agent from 0.3.5), as long as the run's alternate of the lease's
    build and runtime still names it (an owner who drops `model` turns it off for the next generation of running leases
    too); else the default. A lease granted without one (an older agent, an alternate without `model` then, the run's
    default build) never gets one later; a lease granted while its first gen had no variant gets the variant of a later
    gen that has it.
  Fuzz and diag leases carry no checkpoint variant.
- **A gen without the variant** (not registered, not signed, or registered other than signed): the machine plays that
  gen's default model - it is the same network, and a GPU alternate's pack runs the default graph as it did before rev
  20 -, and the admin stats say so: per alternate `variant_notes`, one per gen concerned (the run's current gen while it
  has self-play, and the current eval batch's gens), e.g. `"gen 82: no variant gpu registered: its default model is
  leased"`, `"gen 82: definition v12 doesn't sign variant gpu: ..."` or `"gen 82: variant gpu differs from definition
  v12: ..."`. A note, not one of the alternate's `problems` (which mean it can't be leased at all). The run's
  `definition` (GET run and the stats) adds `needs_agent` (the oldest agent that can read it: 0.2.5, or 0.3.5 when it
  signs a variant) and `variants` (the names it signs per gen, `{"<gen>": ["gpu"]}`); the admin page's "Signed
  definitions" shows both and the notes.
- **Cached checkpoints.** Agents from 0.3.5 list `"<run>/<gen>/<name>"` in `cached.ckpts` (section 3) for each variant
  model in their cache, beside `"<run>/<gen>"` for the default model. For a lease that would carry a variant, the
  coordinator's cache bonus and disk check use that entry and the variant's size.

### 14.4 The agent (0.3.5)

- Parses `variants` (14.1) and verifies the definition as before.
- A checkpoint with `variant` - in a lease's `checkpoint`, an eval lease's `checkpoints[]` or a beat answer - must name a
  variant the lease's definition signs for that gen (a rolling switch: the current definition, 10.6): `sha256` = its
  `model_sha256`, the file checked against its `model_size`; the reference is the gen's own, checked as before. The name
  must have the syntax above. Anything else refuses the lease (`refused: "definition"`, section 7), or, for a beat's
  checkpoint, the generation isn't taken (a rolling lease finishes, 10.6). A legacy lease (no definition) with a variant is
  refused.
- **Cache.** A variant's model is `cache\ckpt\<run>\<gen>\model-<name>.onnx`, next to the gen's `reference.npz` and
  default `model.onnx` (when this PC has them too); the gen's directory stays one LRU entry. Files are verified (size and
  sha256) when downloaded and again before every worker start, as the default model is.
- **The worker** gets the variant's file wherever it would get the model: the job line's `model` (and `sha256`), an eval
  job line's `models["<gen>"].model`, the aw/1 `checkpoint` op's `model` and `sha256` (a rolling lease may switch between
  a variant and a default graph from one generation to the next: the worker must take either). `reference` is unchanged.
  The bench of the job's network (9.6) benches the lease's graph, so a variant's (`probe-providers --model`).
- Tuning (13): a variant's layouts are remembered apart from the default graph's (the variant's name is part of the
  network's fingerprint).
- Stats: no change (the worker's `selftest` per gen as before).

### 14.5 What the trainer writes (zero-core `zero.farm_def`, the pool remote)

For each gen with variants: upload `ckpt/<run>/<gen>/model.onnx`, `reference.npz` and every
`ckpt/<run>/<gen>/model-<name>.onnx`; `PUT /v1/admin/run/{run}/checkpoint/{gen}` with `variants` (14.2); sign the next
definition version with `checkpoints["<gen>"].variants` (14.1, sha256 and size of each variant file exactly as uploaded);
set the run's alternate's `model` (the CUDA alternate: `"model": "gpu"`) and the run's `min_agent` (at least 0.3.5) with
`PUT /v1/admin/run/{run}` before the first definition with variants. A gen whose variant isn't ready is registered without
it: GPU machines play its default model.

## Changes
- v1 rev 20, agent 0.3.7 (2026-11-20; no wire change): **GPU layouts measured and split**, from a bench on the owner's
  4070 Ti PC: a GPU worker of 512+ games in flight runs two groups, games in flight up to twice the target (13.1); a GPU
  job's measured trials run after the drain when they don't fit beside, after the memory plan's early reading (13.2);
  Windows' fullscreen states "busy" and "app" hold work back only while a foreground window covers its monitor, and
  each change of what holds work back is logged (section 3's presence reasons; the PC reported a fullscreen app with no
  foreground window and its last input days old).
- v1 rev 20, agent 0.3.6 (2026-11-20; no wire change): **GPU crews form where memory allows**, from the owner's 4070 Ti
  PC's 0.3.5 lease (one worker of 1,024, ~9.7k leaves/s, its crew never fitting a 9.3 GB limit): a GPU worker's base is
  measured and planned with (10.6), the job's memory limit keeps an eighth of the RAM free while the PC is idle, a
  quarter else (10.6), the start is picked again when the plan holds more workers (13.1), and a drainer the next worker
  waits for ends its last games, their ids returned (13.2).
- v1 rev 20, agent 0.3.5, GPU fixes (2026-11-20; no wire change): from a real lease on the owner's 4070 Ti PC with
  0.3.4 (139 games in flight planned at the default 45 MB a game for a build not measured there, 5,179 leaves/s; the
  tuner handing over to the same trial again and again while the old worker drained for minutes): a GPU job's early
  memory reading (10.6), its start's groups after the memory plan and the start picked again after calibration (13.1),
  measured trials only when their workers fit beside the running ones, else postponed and counted; a trial's
  measurability decided when its worker starts and a lost trial always counted (13.2).
- v1 rev 20, agent 0.3.5 (2026-11-19; coordinator migration 0016): **model variants per generation** (section 14). A
  definition's `checkpoints[gen]` may sign `variants` (`{"<name>": {"model_sha256", "model_size"}}`, sharing the gen's
  reference set); `PUT /v1/admin/run/{run}/checkpoint/{gen}` registers them (`variants`: `{"<name>": {"model", "sha256"}}`,
  additive on a registered gen); a run alternate's `model` names the variant its machines get - in self-play leases, eval
  leases' `checkpoints[]` and beat answers' generation switches (a checkpoint's new `variant` key) -, a gen without it
  gives them its default model (an admin `variant_notes` entry); a run whose current definition has variants counts as
  `min_agent` 0.3.5 (`VARIANT_AGENT`: older agents can't read such a definition); agents from 0.3.5 verify a variant
  against the definition, cache it per (run, gen, variant) and hand it to the worker as the model, and list cached variants
  in `cached.ckpts` as `<run>/<gen>/<name>`.
- v1 rev 19, agent 0.3.4 (2026-11-19; no wire change): **rolling GPU jobs use the GPU properly**, from a measurement on
  the owner's 4070 Ti PC (agent 0.3.3's job: one worker of 128 games in flight, 5,030 leaves/s, the GPU 37 % busy; three
  workers of 384: 14,154, 82 %). GPU "auto" targets two full batches of the bench's batch (10.6: 1,024 games in flight
  there); a GPU job always has a layout, so its games in flight follow the calibrated memory within the lease (10.6); the
  video memory is planned per worker and batch row, measured (Windows' GPU memory counters, or the worker's `vram_mb`),
  calibrated per run and worker build, guarded at 95 % of the adapter, and a worker out of video memory is started again
  with smaller batches (10.6 **Video memory**); crews serve GPU leases - each worker its own session, the search threads
  shared (1.5 per core), every worker yielded for a pause and counted as the lease's own by the GPU busy rule (8, 13.5);
  the tuner covers GPU jobs (workers, games in flight, groups, search threads; an NVIDIA GPU starts with the most
  workers that hold the target; results per GPU and driver; reported with the worker count; no hints for GPU jobs) (13);
  a games-in-flight move of a crew moves each worker's within its part of the bounds (13.2); a CPU job's memory
  calibration works the worker bounds out again (10.6).
- v1 rev 19, agent 0.3.3 (2026-11-19; no wire change): fixes from a real-farm validation of 0.3.2 (the owner's Mac, a
  crew of two on a test run). Remembered layouts are compared as they run in the lease's bounds (13.3: 0.3.2 handed a
  job over to the same layout 3 times in 7 minutes, measuring nothing, when the memory plan held fewer games than the
  remembered incumbent); a handoff never goes to the layout the main worker runs except to measure it, a measurement
  lost before its worker starts counts as discarded, at most 6 handoffs an hour (13.2). The memory plan counts every
  worker - one that just started at its plan, drainers at their commit -, one drain at a time per lease, restarts and
  new members wait for room, and the crew's guard acts on the total with the drainers (13.2, 13.5: 0.3.2's crew held
  38.9 GB against a 31.7 GB cap). macOS keeps the group count where memory cuts the games (batches down to 96 rows;
  13.1, 11.5); the memory plan calibrates itself per run and worker build - a build not measured here starts from the
  platform's default (macOS now 18 MB per game in flight, 11.5), a rise is remembered at once, a steady reading lowers
  it (x 1.2), and one well below the lease's plan raises the job's games in flight by a handoff (10.6). Other programs' load is judged as a mean over a trial
  and before it, sustained load discards at once, each measurement records its load, and a candidate measured on a
  clearly quieter machine doesn't replace the incumbent (13.2).
- v1 rev 19, agent 0.3.2 (2026-11-19; no wire change): fixes from the first real crew on a Mac. A worker's `provider`
  `""` (no sessions yet) is not known yet, never a GPU provider: the job is what the agent chose until the worker
  names one (section 8, **Which provider a job uses**; 0.3.1 suspended every crew's main worker on a Mac for good). A
  runtime pack without `gpu` is not GPU work for the guard, default or alternate (9.5, 11.4: a Mac takes a run's CPU
  alternate). macOS plans 14 MB per game in flight instead of 4 (11.5), so a crew's start and every layout fit the
  memory cap (13.1, 13.5), and a memory-capped "auto" keeps its batches of 192 rows.
- v1 rev 19 (2026-11-18; coordinator migration 0015, agents from 0.3.1, feature `tuning`): **layout tuning** (section
  13). The agent measures the CPU layout of its rolling self-play jobs on the machine (search threads, inference
  sessions x threads, groups, games in flight; near the layout of before, thread counts capped, never searched upward)
  in trials inside
  real leases - layout handoffs in which the old worker finishes its games beside the next one -, only while the
  machine is idle, on AC power, without a fullscreen app and quiet (a trial is discarded, never paused, and the
  incumbent restored), compares fresh workers at the same game progress, keeps a layout only 3 % faster (and still so
  when measured again), remembers it per worker build, runtime, network, hardware and OS (`state/tuning.json`) and
  re-checks it weekly. Job line: `infer_workers` among the tuning fields; worker feature `infer_workers`. Lease
  requests report the tuned layouts (`tuning`); rolling self-play leases to agents with the feature carry the hardware
  class's best as `tuning_hint`; stats and the admin page show them per machine and class. Setting `autotune`.
  **Several workers per lease** (13.5): a rolling CPU self-play lease served by up to 3 worker processes - one tail
  extended, the others on closed ranges of the released ids, members replaced as they finish their ranges, memory split
  between them, paused, switched and ended together, a member's crash leaving the others going -; the worker count is
  the tuner's first coordinate (a Mac with >= 12 cores and 32 GB starts with two); `tuning` reports and hints carry
  `layout.workers` (optional); lease stats `workers`; setting `workers`. No coordinator change for the leases.
- v1 rev 18 (2026-11-18; coordinator migration 0014, no agent change): **hardware routing** (section 12). CPU
  self-play of a run with a GPU version goes after fuzz (tier 3); a PC that plays self-play on its GPU takes an eval
  batch after that self-play unless the CPU PCs online couldn't finish it within `eval_horizon_s` (1800 s); an `auto`
  eval lease on the CPU gets at least 2 games per thread. `GET/POST /v1/admin/routing` (`on` false: the order of
  before).
- v1 rev 17 (2026-11-18; coordinator migration 0013, agents from 0.3.0): platforms (section 11). Header
  `x-farm-platform` (`windows-x64` when absent; fixed per machine at enrollment, 409 `reason: platform`);
  `machines.platform`, `builds.platform`, `agent_releases.platform`, `campaigns.alternates`; manifests take
  `platform` (min_agent 0.3.0+); agent releases, `min_agent`, `/v1/agent/latest`, `/download` and the onboarding
  page are per (channel, platform); macOS GitHub Releases `agent-macos-v<version>`; `GET /install-macos.sh`;
  run alternates take `when.platform` and may omit `when.vendor` (CPU alternates, which name their platform; GPU
  alternates `windows-x64` only); campaign alternates per platform; an alternate held back only by `min_agent` drives
  the update rule when the default is of another platform; agents refuse a build of another platform
  (`refused: "platform"`); the macOS agent (layout, LaunchAgent, token file, process groups, IOKit idle/power,
  `keep_awake`, headless CLI).
- v1 rev 16 (2026-11-17; coordinator migration 0012, agent 0.2.14): `POST /v1/presence` (section
  3, Other): an agent that is on but may not work says why (owner using the PC, fullscreen app, battery, outside
  its hours, paused, low memory) every 5 min and when the reason changes; stats carry `presence`. Older agents
  stay silent while blocked, as before.
- v1 rev 15, agent 0.2.14 (2026-11-17; canary first; no coordinator or wire change for this part): **rolling leases and memory**
  (section 10.6, **Memory**). "auto" plans the games in flight to commit at most 85% of the worker's memory limit at a
  measured cost per game; a worker at 92% of its limit finishes its games and the lease goes on with a new worker with
  fewer in flight; one that runs into the limit is started again with fewer (its games in flight lost, nothing else);
  the run's cost per game measured on the PC is remembered and plans its next leases.
- v1 rev 15 (2026-11-16; coordinator migration 0011, agent 0.2.13 canary first, worker feature `rolling`): **rolling
  self-play leases** (section 10). A run's `selfplay.rolling` makes its self-play leases to agents from 0.2.13 (request
  `features: ["rolling"]`) long-lived: a reserved block of ids released as the worker asks (`POST /v1/lease/{id}/extend`),
  N games in flight with a new one started whenever one ends, finished games flushed every `flush_s`, each new
  generation switched to in place (beat answers carry the new `definition` beside `checkpoint`; the agent verifies it
  and requires an unchanged template, else `finish`), renewed every `max_s` unless other work waits for the machine,
  and never refused as stale (the trainer judges rows by `net`). aw/1 additions: `hello.features`, job `rolling` /
  `flush_s`, op `extend`, event `low`. Classic leases, older agents and workers without the feature are unchanged.
- v1 rev 14 (2026-11-16; coordinator only, migration 0010, no agent change): **a fuzz campaign's share** (section
  5.4). `PUT /v1/admin/campaign/{id}` takes an optional `share` (0 < x <= 1): the campaign then competes with the runs'
  games and goes first while its own active leases + 1 are at most `x` x (the runs' active self-play and eval leases +
  its own + 1), else after every other run job; absent or null, it stays the backfill. GET and the stats show it. For
  the desktop's fuzz steward: continuous fuzz of main's and each live run's build at a daily quota, allowed past the
  side-work share while behind its pace.
- v1 rev 13 (2026-11-15; coordinator only, no migration, no agent change): **play jobs with traces and background
  shares** (section 5.3). A run's `selfplay` and `eval` take an optional `share` (0 < x <= 1): such a job goes first
  only within that share of the runs' active leases, else after every other run job, and still gets any machine the
  others have nothing for; absent, nothing changes. Analysis campaigns are play jobs on runs of their own: an eval
  batch with the zero-core setting `eval_trace` (rows carry `digest` and `rec`) or a self-play with `quota` = the
  games; the desktop checks digests, replays ~5% and plays what is missing past a deadline itself.
- v1 rev 12 (2026-11-13; agent 0.2.11, canary first; coordinator: no migration): **probes the PC never finished,
  no GPU work on AMD, the evict list** (section 9.8). The agent writes a probe-in-progress marker through to disk
  before every built-in probe and bench; one left at the next start is an unfinished probe: reported (`/v1/crash`
  `probe_unfinished`), its pack held a day, for good after a second one on the same adapters and drivers, and listed
  with no provider in `runtime_providers`. Probes and benches never load `amdgpu` or `migraphx`. AMD adapters are
  left out of agents' profiles; AMD alternates are refused at `PUT /v1/admin/run` and never offered. `GET
  /v1/agent/latest` carries the owner's `evict` list (`POST`/`GET /v1/admin/evict`), applied by agents at start
  before any probe. The launcher no longer counts exits with the Windows session's end codes
  (STATUS_DLL_INIT_FAILED_LOGOFF, STATUS_CONTROL_C_EXIT) as crashes, and doesn't start the agent again while the
  session is ending (a logoff rolled a friend's 0.2.8 back to 0.2.5 on 2026-11-13).
- v1 rev 11 (2026-11-13; coordinator migration 0009, agent 0.2.10, canary first): **runtime packs per GPU vendor
  and the measured CPU-or-GPU choice** (section 9). Runs take `alternates` (a worker build + the pack it requires,
  with a vendor condition), leased only to agents from 0.2.10 on machines that qualify (9.4: an adapter of the
  vendor with a new enough driver, GPU work allowed now - `caps.gpu_allowed` -, a provider of the pack the job
  allows, the pack not failed here, disk room, not blocked, authorized by the definition); agents 0.2.5-0.2.9 only
  ever get the default runtime. Definitions keep their bytes (`definition/1`; agents before 0.2.10 refuse unknown
  fields, so a new field would have cut them off every run that names an alternate): from 0.2.10 a lease's runtime
  may also be its build's `requires_runtime` (section 7). One signed worker manifest per pack (9.1). Runtime
  manifests may carry a hard requirement `gpu` (9.2), files a zstd copy `zst` (9.3; the CUDA pack: 1.4 GB on disk,
  about 0.6 GB to download). The agent's guard refuses a misrouted alternate before downloading it (`refused:
  "runtime"`, 9.5), probes a new pack before its first job line, benches the job's own network per (pack, adapters,
  run) and runs a GPU provider only when it is at least 1.1x the CPU at the job's batch size, with `conc` raised for
  GPU jobs (9.6; Stats `batch_rows`, `rows_per_s`); a pack that fails is marked bad for that driver and evicted.
  `runtime_providers` may list `[]`. GPU leases get their own size (`gpu_games_per_lease`, or `auto` from the
  machine's `caps.gpu_games`, capped by the staleness window); GPU jobs run in at most 2 groups.
- v1 rev 10 (2026-11-12, coordinator): **`GET /v1/agent/latest` follows the machine's channel and ignores `?channel=`.**
  An agent asks for the channel it was enrolled on and never learns of a move, so a machine moved from stable to canary
  kept being offered stable's latest while its lease answers demanded canary's `min_agent`: it could neither update nor
  get work, retrying every 5 minutes (one canary test PC, found 2026-11-12). A machine moved back to stable likewise
  stops being offered canary builds. No agent change is needed; old agents' query is simply ignored.
- v1 rev 9 (2026-11-12, coordinator; migration 0007): **`min_agent` on runs and campaigns** - `PUT /v1/admin/run/{run}` and
  `PUT /v1/admin/campaign/{id}` take an optional `"min_agent"` (semver or null); a lease of that run or campaign goes only
  to an agent at or above it, with a build's `min_agent` rule (section 3: `{"update"}` when the agent's channel offers a
  new enough release, else that run or campaign is skipped and the agent gets other work). `GET` of a run or campaign and
  the stats' `runs` / `campaigns` show it. Why: a self-play run whose signed template names a split or double-faced card
  (rev 8) must reach only agents that accept it, while agents of an older stable release keep their fuzz leases. The
  trainer's pool remote sets it (`pool_min_agent`, zero-core). No agent change.
- v1 rev 8 (2026-11-12, phase 3 self-play; coordinator deployed 2026-11-12, agent 0.2.8, canary first):
  **records** - a self-play job's result files are game records (`records` events; the job line's optional
  `output`: `records`, the default of a worker build from phase 3, `shards` the shards of before, `both`), gzip'd
  JSON lines: a header `{"records": 1, "game", "spec", "aux", "features", "actions", "aux_width", "seed" (the lease's),
  "lease", "build_id", "games", "rows", "gids", "gen"}` then one game per line (zero-core docs/search.md 2.17: its
  seed, every real move - a best-of-three's game boundaries included -, each training row's step, decider, kind,
  policy and root value as float16, network id; per game of a match its outcome and decks); the agent uploads them
  as any result (`gids` as `x-ids`, `gen` as `x-gen`), the coordinator never reads them, the trainer's pool remote
  replays every credited game and refuses a chunk whose any game does not replay. **The path-like rule** of
  definitions (section 7) excepts the Oracle name separator ` // `: MTG Zero's Delver spec names "Rough // Tumble"
  and "Tamiyo, Inquisitive Student // Tamiyo, Seasoned Scholar", so its self-play template was refused by every
  check (agent, coordinator, farm-release); `a//b`, `/x`, `x // ../y` stay refused. An agent before this change
  refuses such a definition: the best-of-three run's farm self-play needs this agent released first.
- v1 rev 7 (2026-11-12, as built; zero-core `gpu` 3727b3b, agent and coordinator on zero-farm `gpu`): GPU inference
  (section 8). Worker: `create_session` the one session factory (`SessionSpec` gains `runtime`, `spinning`,
  `expect`; `ProviderKind` gains `AmdGpu` - Windows ML 2.x's AMD GPU provider; MIGraphX is deprecated there);
  a provider is accepted only through the self-test (twice, bit-identical, 2e-5; a GPU provider needs the
  reference set, and one that runs no node is refused); the `selftest` event gains `cpu_fallback_nodes`,
  `deterministic`, `gpu`, `tried`, `build_s`, `pass_s`; GPU providers take whole batches (pieces of up to
  1,024 rows; TensorRT-RTX also up to 1M non-zeros); `probe-providers` (command, JSON, report). Job line:
  `providers` (absent = `["cpu"]`; unknown names skipped) and `gpu`. Runtime packs: the Windows ML pack
  (ONNX Runtime 1.28.3 with DirectML, the catalog's C API; usable from an unpackaged per-user process, no
  Windows App SDK runtime). Exporter: graphs without a scatter-add, `portable()` (bit-identical on the
  CPU). Agent: probes between leases, cached per (runtime build, adapters with drivers) in
  `state\probes.json`; profile `gpus[].luid` (de-duplicated by LUID), `providers` from the probes,
  `runtime_providers`; `caps.gpu` only with a verified GPU provider; the job line's `providers`/`gpu` from
  the probe, the lease's allowlist and the policy (the adapter with the most video memory); PDH busy gate
  for every vendor, D3DKMT temperature (none = no rule). Coordinator: GPU eligibility from verified
  providers (per runtime when known), `required`/`preferred`/`any`, the run's or campaign's `providers`
  allowlist (also on the lease), campaign `gpu`/`providers` (migration 0006), evaluation before
  self-play.
- v1 rev 7 (2026-11-12, planned): GPU inference contract (section 8): `create_session` as the one session factory,
  `providers`/`gpu` in the job line, probed providers and adapter LUIDs in the profile, the PDH busy signal,
  provider routing.
- v1 rev 6 (2026-11-12): signed definitions (section 7): data keys, `definition/1`, the agent builds the job line
  from the signed template, coordinator-settable fields limited, versions never roll back.
- v1 rev 6, coordinator side (2026-11-12, from building it; agent 0.2.5): section 7's coordinator
  details (registration checks equal to the agent's, `defs/` keys in `/v1/artifact` and the admin
  upload, leasing rules, `seed` per lease and not in templates, beats switch only to gens the lease's
  definition signs, `DEFINITION_AGENT` rollout with `{"update"}`, no diag leases for
  definition-aware agents); `done` takes `refused`; crash kind `definition`; the coordinator mark
  `x-zerofarm-coordinator: 1` on every `/v1/*` answer and the agent's 401/403 rules; Cloudflare
  Access logins on `/v1/admin/*` (section 4: Access on `/admin` only; the `CF_Authorization`
  cookie with CSRF checks, `x-farm-admin: 1`, or `Cf-Access-Jwt-Assertion`); `github_url` pinned to `GITHUB_REPO`'s
  `agent-v<version>/ZeroFarmSetup.exe`; limits (security review M3/M4): result chunks and bytes
  per lease (413 with `limit`), no results 24 h after a lease closed (410), fuzz failure seeds
  inside the lease, rates from credited ids only, `MAX_ACTIVE_LEASES` per machine (`reason:
  "too_many_leases"`), crash rows capped per machine per day (`stored: false`) and kept 30 days,
  diagnostics kept 14 days; enroll rate limits keyed by an HMAC of the client IP; desktop API
  `PUT /v1/admin/definition` and `POST /v1/admin/external` (the owner's own machines, admin page
  only), stats fields (`auth`, `limits`, `definition`, `refused_24h`, `blocks`, `contributors`,
  `totals`, `by_scope`), checkpoints keep their reference size.
- v1 rev 5 (2026-11-12): evaluation and head-to-heads (section 5.2): `checkpoints` on eval leases,
  eval batches name `checkpoints`, seats (`net`/`bot`), whole paired leases, `models` in the job
  line, per-gen `selftest`, `evals` result files and their row format. From building it
  (coordinator, agent 0.2.3): the batch checks (registered gens, seats = checkpoints, `ext:`
  refused, even paired `games`, `gen` refused, batches immutable, names never reused);
  checkpoints may be registered older than the current gen (not made current; answer `current`); a
  paired batch starts on an even id; pair crediting within one lease, with `held` games and `also`
  entries in the result listing and `held` in the upload answer; `x-gen` ignored on eval chunks;
  `eval_state` / `eval_batches` / `checkpoints` in `GET /v1/admin/run`, run `eval` progress in
  stats; eval leases need agent 0.2.3+, and their uncached checkpoints count against `caps.disk_mb`;
  `selftest` events carry `gen`; Stats `selftest` per gen; aw/1 `evals` event in the table. Blocked
  workers (a real PC with Smart App Control, 2026-11-12): `/v1/crash` kind `blocked` with `os_error`
  (4551, 1260, 225); the coordinator skips that build for that machine for 24 h or until a newer
  agent asks, lists the block on the admin page, and `POST /v1/admin/machine/{id}` `{"unblock":
  true}` lifts it; agents back off after any worker start failure (blocked: 15 min, 1 h, 4 h, 16 h,
  24 h; others 1 min doubling to 1 h) instead of leasing again at once (0.2.1 leased 521 times in 45
  min). Section 5.1 now comes before 5.2.
- v1 (2026-11-11): initial.
- v1 rev 2 (2026-11-11, from building the coordinator): half-open id ranges spelled out; result
  uploads carry `x-ids` (and `x-gen`) and answer with credit counts, first copy wins; late uploads
  no longer refused once the trainer acked the lease's earlier chunks (only cancelled leases and
  stale self-play chunks get 410); `done` answers 409 `{"missing"}` and `{"returned"}`, works out
  unfinished ids itself, is 200 on expired leases; result keys carry a hash prefix; `profile_digest`
  is opaque; enroll may answer 426 and takes `profile_digest`; `upload_diag` may come with any
  `/v1/lease` answer; beats repeat a newer `checkpoint`; job `seed` is per run; status codes per
  error code; desktop API: invite channel, multipart uploads, `GET /v1/admin/artifact`, run and
  campaign GET, run `gpu`/`needs`, eval batch, campaign results/ack, machine settings, farm pause,
  diag listing, fuller answers.
- worker notes 2 (2026-11-12, phase 1 worker side): fuzz mode names (`delver-games`, not
  `delver`), `fuzz-check`, the agent owns the fuzz summary upload (worker's only with
  `announce_result`), fuzz-profile repros use `--profile fuzz`.
- v1 rev 2 + worker notes (2026-11-11, from phase 0b's mtgz-worker): `checkpoint` is a phase change
  (in-flight games continue on the new network; rows carry `net`), not a game-boundary switch; job
  line tuning fields `conc`/`groups`/`infer_threads`/`spinning`, model `gen`/`sha256`, shard
  `prefix`; `status` doesn't carry `cpu_pct`, GPU fields or `selftest_max_abs` yet; `selftest`,
  `fuzz`, `eval`, `diag` not implemented in the worker yet.
- v1 rev 2 + agent notes (2026-11-11, from building the agent; no wire change for the coordinator):
  manifest rules (prehashed signatures only, extra trusted-comment pairs, path rules, unlisted files
  refused, files under the manifest key's directory, agent packages include `ZeroFarm.exe`); crash
  kinds defined; `logs` diag format; agent-side upload/`done` rules (agent-numbered `seq`, refused
  chunks left out of `done`, orphaned leases closed on restart, beats during downloads); aw/1: job
  `runtime_dir`, `quit` flushes finished games and may precede `job`, `stats` field in `status` and
  `end`, `gen` in `shard`/`records`, `site` in `failure`, worker `build_id` from its manifest, 60 s
  first-start `hello`, ping/kill, 1 MB lines, absolute paths; fuzz results get `final`; layout files
  and launcher details; enroll 426 needs a new installer.
- v1 rev 3 (2026-11-12, follow-ups to the first real install): per-machine rate limits with 429
  `rate_limited` + `retry-after` (lease, beats, crash; enroll per client IP; user diagnostics);
  enroll's 426 carries a `download` link; public `GET /` (onboarding) and `GET /download` (the
  installer: stable, else canary); agent releases carry an installer (`setup`, R2 `setup/`);
  user-initiated `POST /v1/diag/{name}` with `x-farm-diag: user` (1/h, 20 MB); Stats
  `starved_ms` (counts toward the slack like a pause; with `paused_ms`/`resume_ms_max` kept by the
  coordinator, paused and starved time not in rates); Profile `idle_h_day`/`on_h_day`; stats
  endpoint fields; the launcher updates itself (agent exit 76, `ZEROFARM_LAUNCHER=2`,
  `selftest`, `--takeover`, `launcher-swap`/`launcher-ok-<pid>`/`launcher-bad-<sha16>`,
  leftovers, rollback); VERSIONINFO + icon in every exe; `availability.json` in the layout.
- v1 rev 4 (2026-11-12, the public GitHub repository): installers move to GitHub Releases.
  `POST /v1/admin/agent/release` takes `github_url` (a GitHub release asset URL) instead of
  `setup` (now 400); `GET /download` answers 302 to the newest release's `github_url` (stable,
  else canary; 404 without one) instead of streaming bytes from R2; R2 `setup/` and the
  `setup/` upload prefix are gone; stats `releases` carry `github_url` instead of `setup_size`;
  section 2.1 (GitHub Release assets, `SHA256SUMS` and its signature). No change for agents:
  self-updates still come from `/v1/artifact`, and enroll's 426 still names `/download`.

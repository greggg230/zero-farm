# zero-farm protocol v1

The contract between the four pieces: **coordinator** (Cloudflare Worker), **agent** (the app on a
friend's PC), **worker** (per-game native binary the agent runs), and the **desktop tools**
(release signing, trainer). DESIGN.md says why; this file says exactly what goes over the wire.
A change to anything here is a protocol change: bump the version it belongs to and note it at the
bottom.

Versions: HTTP API `v1` (path prefix), agent↔worker protocol `aw/1`, network files `zero-onnx/1`,
manifests `manifest/1`.

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
  "files": [ {"path": "mtgz-worker.exe", "sha256": "...", "size": 26214400} ]
}
```

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
`runtime_providers` (optional, at most 16 runtime builds x 16 names) the same per runtime pack. Provider
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
 "caps": {"threads": 6, "ram_mb": 12000, "gpu": true, "disk_mb": 2500},
 "cached": {"builds": ["<build_id>", ...], "ckpts": ["<run>/<gen>", ...]},
 "kinds": ["fuzz", "eval", "selfplay", "diag"]}
```
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
active leases), evaluation before self-play at the same tier (rev 7); then fuzz, the backfill (also where a
job with `"gpu": "preferred"` goes on a PC that isn't GPU-eligible for it, section 8). Caps must cover the job's `needs` and the disk its uncached builds (and, rev 5, an
eval job's uncached checkpoints) take. A lease is
sized to about 450 s of this machine's measured rate for that job (ids per running second, from its
past leases; its hardware class's average before it has one; 16 games / 10 eval games / 20000
seeds before either), unless the run sets `games_per_lease` / the campaign sets `chunk`. Every
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
                 "reference": "ckpt/eoe2/81/reference.npz", "reference_sha256": "..."} | null,
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
  the machine; the machine's channel as the coordinator holds it is the one that counts.
- `POST /v1/crash` `{"build_id", "kind": "panic" | "crash" | "agent" | "blocked" | "definition",
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
  true}` lifts it. A `blocked` report without `build_id` → 400.
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
| `PUT /v1/admin/run/{run}` | `{"game", "build_id", "runtime", "priority": 1.0, "min_agent": "0.2.8" \| null, "selfplay": {"quota": 600, "slack": 0.1, "max_stale": 1, "games_per_lease": "auto", "gpu": "any" \| "preferred" \| "required", "providers": ["dml", "cpu"] \| null, "needs": {"threads", "ram_mb"}, "job": {...}} \| null, "eval": {"batch": "it81", "games": 110, "checkpoints": [81, 70], "games_per_lease": "auto", "gpu", "providers", "needs", "job": {"settings", "seats", "paired"}} \| null, "state": "active" \| "paused"}` → `{"ok": true, "created"}`. `build_id` must be a registered worker build, `runtime` a runtime build. A new eval `batch` reserves `games` fresh game ids (the old batch's unissued ones are dropped); its rules and checks are in section 5.2 (rev 5: `checkpoints` replaces `gen`, which is now refused). Pausing a run, or dropping its `selfplay`/`eval`, cancels those leases at their next beat. Rev 9: `min_agent` (semver, optional; absent or null: none) - the run's leases (self-play and eval) go only to agents at or above it (section 3's update rule); a PUT is the whole configuration, so a PUT without it removes the limit; leases already issued run on (migration 0007) |
| `GET /v1/admin/run/{run}` | config (rev 9: with `min_agent`) + state: `current_gen`, `next_gid`, `pool`, `selfplay_state` (`cap`, `accepted`, `leased`, `pending`, `in_flight`, `headroom`), `checkpoints` (registered gens, rev 5), `eval_state` (the current batch's progress, section 5.2) and `eval_batches` (every batch of the run, newest first, same fields); rev 6 `definition` (as in stats; `GET /v1/admin/campaign/{id}` too) |
| `PUT /v1/admin/run/{run}/checkpoint/{gen}` | `{"model", "sha256", "reference", "reference_sha256"}` (files under `ckpt/<run>/`, uploaded first); makes `gen` current for self-play and resets that generation's quota counter → `{"ok": true, "unchanged", "current"}`. Rev 5: a `gen` older than the current one is registered without becoming current (`"current": false`: an eval opponent or a head-to-head label); the same `gen` with other files → 409. Rev 6: a gen the current definition already signs must be exactly those files (else 400); self-play with a definition plays the new current gen once a definition signs it |
| `GET /v1/admin/run/{run}/results?after={cursor}&limit=500` | → `{"results": [{"id", "key", "lease_id", "seq", "machine_id", "kind", "gen", "games", "ids", "dup_ids", "sha256", "size", "t"}], "cursor"}`. `ids`/`games`: the game ids credited to this chunk; `dup_ids`: ids in it that an earlier chunk was credited with (skip those games). Eval chunks (rev 5) also carry `batch`, `held` and `also`, and their `games` counts `also` too (section 5.2). `limit` ≤ 1000 |
| `POST /v1/admin/run/{run}/accept` | `{"gen", "games": n}`: the trainer's total of games accepted for `gen` (a total, not an increment; include games held over for the next generation). Self-play stops when accepted + in flight (unclaimed ids in active self-play leases + credited self-play games not yet acked) reaches `quota × (1 + slack)`. Call it before `ack` |
| `POST /v1/admin/run/{run}/ack` | `{"cursor"}`: results up to it are ingested (R2 objects deleted after 7 days) → `{"ok": true, "acked"}` |
| `PUT /v1/admin/campaign/{id}` | `{"build_id", "mode", "env": {"FUZZ_POOL": "delver", ...}, "seed_base": 10000000, "chunk": "auto" \| n, "priority": 0.2, "state", "needs": {"threads", "ram_mb"} \| null, "gpu": "any" (default) \| "preferred" \| "required", "providers": [...] \| null, "min_agent": "0.2.8" \| null}` → `{"ok": true, "created", "cursor"}`. The seed cursor never rewinds: a lower `seed_base` is ignored, a higher one moves it up. Rev 7: `gpu` and `providers` (section 8; migration 0006). Rev 9: `min_agent` as for runs (migration 0007) |
| `GET /v1/admin/campaign/{id}` · `GET …/results?after=` · `POST …/ack` | config + `cursor`, `seeds_done`, `pool`, `failures`, `sites`; results and ack as for runs (`ids` are seeds) |
| `GET /v1/admin/campaign/{id}/failures?after=&limit=` | → `{"failures": [{"id", "seed", "mode", "site", "panic", "repro", "output_tail", "build_id", "engine_commit", "machine_id", "lease_id", "first_of_site", "t"}], "cursor"}` (section 5.1; one row per (seed, site)) |
| `GET /v1/admin/machines` · `POST /v1/admin/machine/{id}/revoke` | revoking cancels the machine's active leases at once (their ids go back to the pool) → `{"ok": true, "cancelled"}` |
| `POST /v1/admin/machine/{id}` | `{"name"?, "channel"?, "paused"?, "unblock"?}`; a paused machine gets `wait_s` / `paused_by_admin`; `"unblock": true` (rev 5) lifts the machine's build blocks (`/v1/crash` kind `blocked`) and answers `{"ok": true, "unblocked": n}` |
| `POST /v1/admin/pause` | `{"paused": true \| false}`: farm-wide, no new leases while paused |
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
| `{"op": "job", "lease_id", "kind", ...Job, "model": "<local path>" \| null, "reference": "<local path>" \| null, "threads": 6, "out_dir": "out", "runtime_dir": "<local path>" \| null}` | start. Optional worker tuning (set by the agent's autotuning, never by the run): `conc`, `groups`, `infer_threads`, `spinning`; also `gen` and `sha256` of the model (else `gen` comes from the graph's metadata), `prefix` for shard names. Eval (rev 5): `models` instead of `model`/`reference` (both null), section 5.2 |
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
| `status` | `stats`: Stats (section 3) |
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
                            "reference_sha256": "...", "reference_size": 412000}},
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
4. checks the lease's worker build is in `builds` and its runtime equals `runtime`, and that every
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
  run's (or campaign's) build and runtime (a campaign's runtime is null), has a template for the
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
process except the agent and its worker, per adapter its busiest engine; the gate is the busiest
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
routed first, then self-play (within a tier); fuzz stays the backfill. Every lease carries the job's
allowlist as `providers` (null on diag leases).

## Changes
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

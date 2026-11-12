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
  only as sha256. One admin token (Worker secret `ADMIN_TOKEN`), used only from the desktop.
- Errors: HTTP 4xx/5xx with `{"error": "<code>", "message": "<text>"}` (some add fields, named
  below). Codes and their statuses: `bad_request` 400 (405 for a method a path doesn't take),
  `unauthorized` 401 (403 for an unknown or used invite code), `revoked` 403, `not_found` 404,
  `conflict` 409, `gone` 410 (lease expired/cancelled, stale result), `too_large` 413,
  `update_required` 426, `rate_limited` 429, `internal` 500. Clients retry 5xx and network errors
  with exponential backoff (1 s → 5 min, jitter); never retry 4xx except `429` (honour
  `retry-after`).
- Rate limits (rev 3): a token bucket per machine (GCRA: on average one request per interval, a
  burst at once). `POST /v1/lease` 1 per 5 s after a burst of 6; beats 1 per 10 s after 12 (all
  of a machine's leases together); `POST /v1/crash` 1 per 10 s after 30; a user-initiated
  `POST /v1/diag/{name}` 1 per hour; `POST /v1/enroll` 1 per 60 s after 10 per client IP. Over
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
 "gpus": [{"vendor": "nvidia", "name": "RTX 4070", "vram_mb": 12282, "driver": "610.88"}],
 "providers": ["cpu", "tensorrt_rtx", "cuda", "migraphx", "dml", "openvino"],
 "idle_h_day": 9.5, "on_h_day": 14.2}
```
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
- `{"wait_s": 300, "reason": "no_work" | "paused_by_admin" | "outside_hours"}` (`outside_hours` is
  the agent's own; the coordinator never sends it)
- `{"update": {"min_agent": "0.4.0"}}` → the agent updates itself (section 6) before asking again.
  Sent when the agent is older than the `min_agent` of its channel's latest release, or older than
  the `min_agent` of a build it would otherwise be given and the channel's latest release is new
  enough for it. A job whose build needs an agent the channel doesn't offer yet is skipped. A worker
  build whose `aw` differs from the request's `x-farm-aw` is skipped.

Any of the three may also carry `"upload_diag": ["logs"]` (as a beat answer can): a machine with no
lease hears about a diagnostics request on its next `/v1/lease`.

How the coordinator picks (DESIGN 5.1): diagnostics asked for this machine first; then runs'
self-play (short of quota) and evaluation (unissued games), by run priority × fit ÷ (1 + that job's
active leases); then fuzz, the backfill (also where a run with `"gpu": "preferred"` goes on a
CPU-only PC). Caps must cover the job's `needs` and the disk its uncached builds take. A lease is
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
  "job": Job                                           // section 5, passed to the worker as is
}
```

### POST /v1/lease/{lease_id}/beat
`{"state": "running" | "paused", "progress": {"games_done": 12, "games_total": 40},
  "stats": Stats}` → `{"action": "continue" | "cancel" | "give_back",
  "expires_at": 1794301500, "checkpoint": {...} | null}`. A newer `checkpoint` (self-play) means:
switch at the next game boundary; it is repeated in every beat while it is newer than the lease's
own checkpoint (ignore repeats). `cancel`: stop now, discard (sent when the run or campaign was
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
  (default: the lease's checkpoint).

→ `{"ok": true, "duplicate": false, "credited": 4, "already_credited": 0}`. Idempotent: the same
`seq` with the same hash → 200 (`duplicate: true`); a different hash → 409. `seq` counts from 0 per
lease. Late chunks (the lease expired, was given back or is done) are accepted: the trainer reads
results by cursor, so it sees them whatever it acked before. 410 `gone`: the lease was cancelled,
or a self-play chunk is older than the run's `max_stale` generations behind its current checkpoint;
the agent drops that chunk. A fuzz chunk must be the JSON of section 5.1 (else 400).

### POST /v1/lease/{lease_id}/done
`{"results": [0, 1, 2], "stats": Stats, "given_back": false,
  "unfinished": {"gids": [[1200, 1215]]} | null}` → `{"ok": true, "returned": 15}`. `results` lists
every seq the agent uploaded; any the coordinator doesn't have → 409 `conflict`
`{"missing": [seq, ...]}` (upload them, then call `done` again). Unfinished ids go back to the
pool; the coordinator works them out itself (the lease's ids minus every id credited to any chunk),
so `unfinished` (a fuzz lease reports `{"seeds": [[a, b]]}`) is only checked to lie inside the
lease (else 400). `returned` is how many ids went back. Repeating `done` → 200. On an expired lease
→ 200 (stats kept; its ids went back when it expired); on a cancelled lease → 410.

### Other
- `GET` (or `HEAD`) `/v1/artifact/{key}` → bytes (R2 key from a lease/manifest; only keys under
  `builds/`, `ckpt/`, anything else → 404; supports `range` → 206 and `if-none-match` → 304).
- `GET /v1/agent/latest?channel=stable` → `{"version": "0.3.1", "build_id": "...",
  "manifest": "builds/<id>/manifest.json", "min_agent": "0.3.0"}`. `channel` defaults to the
  machine's; nothing released on it → 404.
- `POST /v1/crash` `{"build_id", "kind": "panic" | "crash" | "agent", "message", "backtrace",
  "lease_id"}` → `{"ok": true}` (message kept to 4 KB, backtrace to 64 KB). `panic`: a Rust panic (a
  worker's, seen on its stderr, with the worker `build_id`; or the agent's own, sent at its next
  start, with the agent's `build_id`). `crash`: a worker that exited without `end`, hung, missed a
  deadline or broke aw/1; `backtrace` = its stderr tail (agents may add `"exit_code"`). `agent`: a
  launcher rollback or refused update, reported by the next agent that runs.
- `POST /v1/diag/{name}` body bytes (≤ 32 MB) → `{"ok": true, "key": "diag/..."}`, when the
  coordinator asked for it in a beat or lease answer (`"upload_diag": ["logs"]`); else, with header
  `x-farm-diag: user` (rev 3: the friend pressed "Send diagnostics"), ≤ 20 MB (else 413) and
  once an hour per machine (else 429); else 409 `conflict`. Any upload answers the open requests
  of that name. An unanswered request is asked again after 30 min. `logs`: `text/plain`, an
  optional `===== about =====` header (agent version, status, settings), then the agent, worker
  and launcher logs, each tail-capped at 4 MB.

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
it downloads what a lease needs. 401/403 on any call: the agent stops and asks for a new invite.

`Stats` (per beat/done; all optional, counters since lease start):
```json
{"games": 12, "decisions": 6100, "leaves": 410000, "infer_batches": 3300,
 "infer_ms_p50": 4.1, "infer_ms_p95": 9.8, "search_ms": 51000, "infer_ms": 40000, "wait_ms": 3000,
 "cpu_pct": 540, "gpu_pct": 0, "vram_mb": 0, "rss_mb": 900, "threads": 6, "concurrency": 32,
 "provider": "cpu", "gpu_temp_c": null, "throttle": [], "yields": 1, "yield_ms_max": 640,
 "selftest_max_abs": 3.1e-6, "paused_ms": 20000, "resume_ms_max": 80, "starved_ms": 0}
```
`starved_ms` (rev 3): time the worker should have been running but got no CPU because other
programs saturated the PC (it runs at idle priority); the agent samples this every 5 s. It counts
toward the lease's slack like a pause: a lease that has been paused and/or starved for longer than
`slack_s` in one stretch is given back (`given_back: true`). The beat `state` stays `running`
while starved. The coordinator keeps the fields above (`paused_ms`, `resume_ms_max` in D1 only)
and leaves paused and starved time out of a machine's rate.

## 4. Desktop API (bearer = admin token)

| Call | Body → answer |
|---|---|
| `POST /v1/admin/invites` | `{"count": 3, "note": "Sam", "channel": "stable" (default) \| "canary"}` → `{"codes": [...]}` |
| `PUT /v1/admin/artifact/{key}` | bytes (optional `x-sha256`, checked by R2) → `{"ok": true, "key", "size"}` (builds/, ckpt/; up to ~100 MB, a Worker request's limit) |
| `POST /v1/admin/artifact/{key}?uploads` · `PUT …?upload_id=&part=N` · `POST …?upload_id=` | multipart upload for bigger artifacts: → `{"upload_id"}`; part bytes (≥ 5 MiB except the last) → `{"part", "etag"}`; `{"parts": [{"part", "etag"}, ...]}` → `{"ok": true, "size"}` |
| `GET /v1/admin/artifact/{key}` | any object (results, diag uploads, builds, checkpoints); `range` as for machines |
| `POST /v1/admin/build` | `{"build_id"}` after its manifest, signature and files are uploaded; the coordinator checks presence + sizes + hashes, and the signature and its trusted comment (`build_id=`, `kind=`) when the Worker var `RELEASE_PUBKEYS` is set → `{"ok": true, "build_id", "kind", "version", "size", "sig_verified"}`; failures → 400 with `"problems": [...]` |
| `POST /v1/admin/agent/release` | `{"version", "build_id", "channel": "canary" \| "stable", "min_agent", "github_url": "<url>" \| null}`: `build_id` must be a registered agent build of that version, `min_agent` ≤ `version`. `github_url` (rev 4): the installer asset of the release's GitHub Release (`https://github.com/<owner>/<repo>/releases/download/<tag>/<file>.exe`, else 400), where `GET /download` redirects; create the GitHub Release first. `setup` (rev 3) → 400 → `{"ok": true, "channel", "version", "build_id", "min_agent", "github_url"}` |
| `PUT /v1/admin/run/{run}` | `{"game", "build_id", "runtime", "priority": 1.0, "selfplay": {"quota": 600, "slack": 0.1, "max_stale": 1, "games_per_lease": "auto", "gpu": "any" \| "preferred" \| "required", "needs": {"threads", "ram_mb"}, "job": {...}} \| null, "eval": {"batch": "it81", "games": 110, "games_per_lease": "auto", "gen": null, "gpu", "needs", "job": {...}} \| null, "state": "active" \| "paused"}` → `{"ok": true, "created"}`. `build_id` must be a registered worker build, `runtime` a runtime build. A new eval `batch` reserves `games` fresh game ids (the old batch's unissued ones are dropped). Pausing a run, or dropping its `selfplay`/`eval`, cancels those leases at their next beat |
| `GET /v1/admin/run/{run}` | config + state: `current_gen`, `next_gid`, `pool`, `selfplay_state` (`cap`, `accepted`, `leased`, `pending`, `in_flight`, `headroom`) |
| `PUT /v1/admin/run/{run}/checkpoint/{gen}` | `{"model", "sha256", "reference", "reference_sha256"}` (files under `ckpt/<run>/`, uploaded first); makes `gen` current for self-play and resets that generation's quota counter → `{"ok": true, "unchanged"}`. A `gen` older than the current one, or the same `gen` with other files → 409 |
| `GET /v1/admin/run/{run}/results?after={cursor}&limit=500` | → `{"results": [{"id", "key", "lease_id", "seq", "machine_id", "kind", "gen", "games", "ids", "dup_ids", "sha256", "size", "t"}], "cursor"}`. `ids`/`games`: the game ids credited to this chunk; `dup_ids`: ids in it that an earlier chunk was credited with (skip those games). `limit` ≤ 1000 |
| `POST /v1/admin/run/{run}/accept` | `{"gen", "games": n}`: the trainer's total of games accepted for `gen` (a total, not an increment; include games held over for the next generation). Self-play stops when accepted + in flight (unclaimed ids in active self-play leases + credited self-play games not yet acked) reaches `quota × (1 + slack)`. Call it before `ack` |
| `POST /v1/admin/run/{run}/ack` | `{"cursor"}`: results up to it are ingested (R2 objects deleted after 7 days) → `{"ok": true, "acked"}` |
| `PUT /v1/admin/campaign/{id}` | `{"build_id", "mode", "env": {"FUZZ_POOL": "delver", ...}, "seed_base": 10000000, "chunk": "auto" \| n, "priority": 0.2, "state", "needs": {"threads", "ram_mb"} \| null}` → `{"ok": true, "created", "cursor"}`. The seed cursor never rewinds: a lower `seed_base` is ignored, a higher one moves it up |
| `GET /v1/admin/campaign/{id}` · `GET …/results?after=` · `POST …/ack` | config + `cursor`, `seeds_done`, `pool`, `failures`, `sites`; results and ack as for runs (`ids` are seeds) |
| `GET /v1/admin/campaign/{id}/failures?after=&limit=` | → `{"failures": [{"id", "seed", "mode", "site", "panic", "repro", "output_tail", "build_id", "engine_commit", "machine_id", "lease_id", "first_of_site", "t"}], "cursor"}` (section 5.1; one row per (seed, site)) |
| `GET /v1/admin/machines` · `POST /v1/admin/machine/{id}/revoke` | revoking cancels the machine's active leases at once (their ids go back to the pool) → `{"ok": true, "cancelled"}` |
| `POST /v1/admin/machine/{id}` | `{"name"?, "channel"?, "paused"?}`; a paused machine gets `wait_s` / `paused_by_admin` |
| `POST /v1/admin/pause` | `{"paused": true \| false}`: farm-wide, no new leases while paused |
| `POST /v1/admin/diag` | `{"machine_id" \| "class", "kind": "sweep" \| "profile" \| "stability" \| "logs", "params"}` → `{"requests": [{"id", "machine_id"}]}`. `class` = a hardware class as the stats show it (`"<cpu> / <gpu or 'no gpu'>"`). `logs` goes out as `upload_diag`; the others become `diag` leases (`params.build_id` or `params.run` picks the build) |
| `GET /v1/admin/diag?machine_id=` | requests, uploads and diag-lease results with their R2 keys |
| `GET /v1/admin/stats` | aggregates for the dashboard (`/admin`); per machine also `idle_h_day`, `on_h_day`, `starved_s_24h`; `releases` (latest per channel, `github_url`), `diag_uploads` (recent, `user`) |

## 5. Jobs (`lease.job`) and the agent↔worker protocol (`aw/1`)

The agent starts `<entry> serve` from the verified build directory with the working directory set
to a fresh `work/<lease_id>/`, sends one `job` line, and reads events. stdin/stdout carry JSON
lines only (one object per line); stderr is a log the agent keeps (rotated) and never parses.

Agent → worker:

| Line | Meaning |
|---|---|
| `{"op": "job", "lease_id", "kind", ...Job, "model": "<local path>" \| null, "reference": "<local path>" \| null, "threads": 6, "out_dir": "out", "runtime_dir": "<local path>" \| null}` | start. Optional worker tuning (set by the agent's autotuning, never by the run): `conc`, `groups`, `infer_threads`, `spinning`; also `gen` and `sha256` of the model (else `gen` comes from the graph's metadata), `prefix` for shard names |
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
| `selftest` | `provider`, `max_abs`, `ok` (before the first real batch with a model) |
| `status` | `stats`: Stats (section 3) |
| `fin` | `n`: games finished since the last `fin` |
| `shard` / `records` | `seq`, `path` (relative, under out_dir), `games`, `rows`, `bytes`, `gids`, `gen`? — a result file ready to upload (`shard` until phase 3, then `records`); the agent moves the file away and sends `gids` as `x-ids`, `gen` (the oldest checkpoint generation of its games) as `x-gen` |
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

### 5.2 Evaluation and head-to-heads (rev 5)

An eval batch plays fixed networks against each other (or against a built-in bot) and reports one
row per game; the desktop computes the score, its error and an Elo difference. Checkpoints never go
stale, so eval leases ignore `max_stale`.

- **Batch** (desktop): `PUT /v1/admin/run/{run}` with `"eval": {"batch", "games", "checkpoints":
  [gen, ...], "games_per_lease", "gpu", "needs", "job": {"settings": {...}, "seats": [Seat, Seat],
  "paired": true}}`. Every gen in `checkpoints` must be registered on the run (`PUT .../checkpoint/
  {gen}`). An ad hoc head-to-head is a run of its own (e.g. `h2h-eoe1-80v70`, `"selfplay": null`)
  whose gens are just labels for the two networks.
- **Seat**: `{"net": <gen>}` or `{"bot": "<name>"}` (a built-in bot of the game; `ext:` bots are
  refused by `check-config`).
- **Paired games**: with `"paired": true` game ids come in pairs `(2k, 2k+1)`: the same deal with
  the seats swapped. Leases always carry whole pairs (`gid_base` and `games` even), and a pair is
  credited only when both games are in. Seat order for game g: `seats` for even g, reversed for odd g.
- **Lease**: `checkpoints` lists every network the seats name (`gen`, `model`, `sha256`,
  `reference`, `reference_sha256`); `checkpoint` is null.
- **Agent → worker** (`job` line): `models: {"<gen>": {"model": path, "sha256", "reference": path}}`
  for every listed checkpoint. The worker runs the self-test (`selftest` event, one per gen) before
  its first game and refuses to play with a network that fails it.
- **Results**: the worker writes JSON lines, one per game: `{"gid", "pair", "seats": [Seat, ...],
  "result": [r_seat0, ...], "margin", "rounds", "over", "net_ms"?}` (`result`: 1 win, 0.5 draw,
  0 loss per seat, in seat order of that game). It announces each file with the event `evals`
  (same fields as `shard`: `seq`, `path`, `games`, `gids`). The agent uploads it like any result;
  `GET /v1/admin/run/{run}/results` lists it with `"kind": "eval"` and the batch.
- **Semantics** must equal zero-core's `zero.h2h` / `Run.evaluate` at the same settings (eval sims,
  seat order, the deal shared by a pair); the worker's gate checks native rows against the Python
  path running the same ONNX graphs, byte for byte.

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

## Changes
- v1 rev 5 (2026-11-12): evaluation and head-to-heads (section 5.2): `checkpoints` on eval leases,
  eval batches name `checkpoints`, seats (`net`/`bot`), whole paired leases, `models` in the job
  line, per-gen `selftest`, `evals` result files and their row format.
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

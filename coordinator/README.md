# zero-farm coordinator

The Cloudflare Worker that hands out work to invited friends' idle Windows PCs for zero-core
training runs (MTG Zero first): enrollment, leases, the job mix per run, a signed-build index,
result indexing for the trainer, eval batches and head-to-heads (several networks per lease,
paired games), fuzz campaigns, diagnostics, builds a PC's Windows refuses to run, signed
definitions (what a lease may make a PC download and run, signed on the desktop with a data key),
an admin dashboard (Cloudflare Access login or the admin token; the owner's own machines merged in
as contributors), per-machine rate limits, upload and retention limits, and the public onboarding
page (its download link redirects to GitHub Releases).

- Wire contract: [`../PROTOCOL.md`](../PROTOCOL.md) sections 1-4 (and the coordinator side of 5-6).
- Why it works this way: [`../DESIGN.md`](../DESIGN.md) sections 2, 3.4-3.5, 4, 5, 6, 7.

Stack: Worker + D1 `zero-farm` + R2 `zero-farm` + Durable Object `LeaseBook` + Analytics Engine
dataset `zero_farm`. Production is `farm.greggg230.com` (see "Deployed").

```
src/index.ts       router + cron entry; marks every /v1/* answer (x-zerofarm-coordinator: 1)
src/config.ts      vars and secrets, with their defaults
src/friend.ts      friend API (enroll, lease + job choice and sizing, beat, result, done, artifact, ...)
src/admin.ts       desktop API (invites, artifacts, builds, releases, runs, checkpoints, results,
                   definitions, external contributions, stats, ...)
src/access.ts      Cloudflare Access JWTs as admin (RS256, the team's JWKS, cached)
src/definitions.ts signed definitions (PROTOCOL 7): parsing, the agent's checks, what they allow
src/external.ts    the owner's own machines' numbers (POST /v1/admin/external), contributors
src/book.ts        a scope's leases, id pool, claims and quota: runs inside LeaseBook
src/leasebook.ts   the Durable Object: serialises every change to one run / campaign
src/manifest.ts    build checks: manifest, file sizes/hashes, minisign (Ed25519, BLAKE2b prehash)
src/telemetry.ts   Analytics Engine writer (no-op without the binding) + Stats sanitising
src/admin_page.ts  the /admin dashboard (static HTML + JS)
src/onboard.ts     the public pages: / (onboarding) and /download (302 to the installer on GitHub)
src/ratelimit.ts   per-machine (and per-IP-HMAC for enroll) rate limits: GCRA in D1, 429 + retry-after
src/blocks.ts      worker builds Windows refused to start on a machine (crash kind "blocked")
src/cron.ts        every minute: expire silent leases, delete acked results after 7 days, crash
                   rows after 30 days, diagnostics after 14, prune limits and external sources
migrations/        D1 schema (numbered; `wrangler d1 migrations apply`)
scripts/admin.ts   admin CLI
test/              vitest in local workerd, including the simulated fleet
```

## Run locally

```
npm install                      # .npmrc sets legacy-peer-deps (npm 10 crashes on vitest 4's peers)
npm run db:local                 # apply migrations to the local D1 in .wrangler/
npm run dev                      # wrangler dev on 0.0.0.0:5400
```

`.dev.vars` (gitignored) holds `ADMIN_TOKEN=dev-admin-token` for local runs (keep it to that one
line: `wrangler types --check` reads it). The dashboard is
[http://localhost:5400/admin](http://localhost:5400/admin) while `npm run dev` runs.

Admin CLI (any base URL; behind Cloudflare Access also `CF_ACCESS_CLIENT_ID` and
`CF_ACCESS_CLIENT_SECRET`, an Access service token, sent as `CF-Access-Client-Id` /
`CF-Access-Client-Secret` next to the bearer token):

```
export FARM_URL=http://localhost:5400 FARM_ADMIN_TOKEN=dev-admin-token
npm run admin -- help
npm run admin -- invites --count 3 --note Sam
npm run admin -- upload-build path/to/build-dir     # manifest.json + .minisig + files, then register
npm run admin -- run eoe2 run.json
npm run admin -- checkpoint eoe2 81 model.onnx reference.npz
npm run admin -- machine m_... --unblock true      # lift the builds it reported blocked
npm run admin -- campaign fuzz-delver campaign.json
npm run admin -- definition definition.json         # + definition.json.minisig (a data key's), PROTOCOL 7
npm run admin -- agent-release 0.2.0 <build_id> canary 0.1.0 --github-url <installer asset URL>
npm run admin -- external report.json               # the owner's own machines (admin page only)
npm run admin -- stats
```

Since rev 6 a run or campaign gets work from definition-aware agents (0.2.5+) only once a signed
definition names its build, runtime, checkpoints and job templates: sign one on the desktop with
the data key (`farm-release define`, which also uploads and registers it), and a new version
whenever builds, checkpoints or job settings change (the trainer adds each new gen). Until a run
or campaign has one, only older agents work on it.

(`farm-release release-agent --github greggg230/zero-farm` in `../agent` does the agent release in
one go: build, sign, publish the build, make the GitHub Release, then this release call with its
`github_url`. `agent-release --github-url` here points a channel at an already-registered build
and its GitHub installer, e.g. to promote a canary build to stable.)

Commands: `invites`, `machines`, `machine`, `revoke`, `pause`/`resume`, `put`, `get`, `build`,
`upload-build`, `definition`, `agent-release`, `run`, `run-get`, `checkpoint`, `results`, `accept`,
`ack`, `campaign`, `campaign-get`, `campaign-results`, `campaign-ack`, `failures`, `diag`,
`diag-list`, `external`, `stats`. Files over 64 MB go up as R2 multipart uploads.

Example run config (`PUT /v1/admin/run/<run>`):

```json
{"game": "mtg", "build_id": "<24 hex>", "runtime": null, "priority": 1.0, "state": "active",
 "selfplay": {"quota": 600, "slack": 0.1, "max_stale": 1, "games_per_lease": "auto",
              "gpu": "any", "needs": {"threads": 2, "ram_mb": 2000}, "job": {"settings": {}}},
 "eval": null}
```

A head-to-head (PROTOCOL 5.2) is a run of its own: put it with `"selfplay": null, "eval": null`,
register its two networks (`checkpoint h2h-eoe1-80v70 80 ...`, then `... 70 ...`: the gens are
labels, any order), then put it again with the batch:

```json
"eval": {"batch": "b1", "games": 200, "checkpoints": [80, 70], "games_per_lease": "auto",
         "gpu": "any", "needs": {"threads": 2, "ram_mb": 2000},
         "job": {"settings": {"sims": 32}, "seats": [{"net": 80}, {"net": 70}], "paired": true}}
```

`run-get` shows its progress (`eval_state`); `results` lists the rows to read (`ids` and
`also`); a new batch needs a new name.

## Test

```
npm test          # vitest + @cloudflare/vitest-pool-workers: local workerd, no Cloudflare account
npm run check     # wrangler types --check, tsc (src + test), tsc (scripts)
```

140 tests in 17 files (about 2 minutes; the fleet simulation is most of it):

| File | Covers |
|---|---|
| `api.test.ts` | every endpoint's happy path and validation; auth failures on every route (no token, unknown token, admin token on friend routes, machine token on admin routes, revoked → 403); tokens stored only as sha256; builds (missing/mismatched files, missing signature, wrong id); releases; checkpoints (an older gen registered, not current); diag (logs via beats and lease answers, diag leases); crashes; stats (incl. availability, starvation, releases, diagnostics uploads, the admin page's columns) |
| `ratelimit.test.ts` | 429 + `retry-after` after the burst; the 2026-11-11 hot loop (8 lease requests a second) gets exactly one answer per 5 s and is never locked out; a simulated day of a well-behaved agent (leases, beats, need_profile retries, wait_s, back-to-back leases at the limit's rate) is never limited; per machine; beats and crash reports; enroll per client IP (wrong codes count, the invite survives); the cron prunes |
| `onboard.test.ts` | `/` without an installer (and its GitHub source link), with only canary (says "test build"), stable over canary, `?channel=`, an installer older than its channel's min_agent skipped; `/download` = 302 to the release's `github_url` (GET and HEAD, no bytes); `github_url` validation (GitHub release assets only), `setup` and `setup/` uploads refused; 426 `download` link per channel |
| `diag.test.ts` | user-initiated uploads (`x-farm-diag: user`): no request needed, once an hour per machine, 20 MB (an oversize one doesn't use up the hour), requested uploads unchanged, a user upload answers open requests |
| `lease.test.ts` | lifecycle: issue → beat → upload → done; expiry after 3 silent beats → reissue of exactly the unfinished ids; paused past slack → `give_back`; agent give-back; pause → `cancel`; new checkpoint through beats; `done` with missing results; job choice by priority, self-play → eval → fuzz backfill, GPU routing, needs/disk; lease sizing from measured and class rates; paused and starved (`starved_ms`) time not counted |
| `quota.test.ts` | cap = quota × (1 + slack), trimmed last lease, in-flight = leased + unacked, rejected games freeing room, given-back ids reissued, checkpoint resets the count, expired leases stop counting |
| `fuzz.test.ts` | cursor never rewinds (lower seed_base ignored, higher jumps), pool before cursor, failures parsed and deduped, campaign results/ack |
| `eval.test.ts` | rev 5 eval batches: a head-to-head run without self-play (gens registered in any order), leases with every network (`checkpoints`, `checkpoint: null`) in whole pairs, odd `games_per_lease` rounded down; the batch checks (registered gens, seats = checkpoints, `ext:` bots, one of net/bot, seat count, even paired games, `gen` refused, a new run must register its gens first, bots-only batches); batches immutable and names never reused; pair crediting across chunks of a lease (`held`, `also`, retries) read back the documented way; a held game's pair given back whole and played again, a late chunk completing its own pair first, an expired lease's pair reissued; the even start and the skipped id in the self-play pool; `max_stale` and `x-gen` ignored; agent 0.2.3 gating; uncached checkpoints against the disk cap; progress in `GET run`, stats and the dashboard; per-gen `selftest` stats kept |
| `blocked.test.ts` | rev 5 crash kind `blocked`: needs a `build_id`; that build no longer goes to that machine (others still get it); the crash row keeps `os_error`; stats and the dashboard list the block; lifted after 24 h, by a newer agent, by the admin (`unblock`), and a new build is offered at once; other crash kinds block nothing |
| `results.test.ts` | idempotent uploads, conflicting hash → 409 (and the loser's bytes removed), hash/size/seq/ids checks, 32 MB limit, first copy wins across leases, late copies (pool and reissued ids), staleness, cursor paging, R2 deletion 7 days after ack |
| `definitions.test.ts` | rev 6 signed definitions (really signed, with a throwaway test data key): registering (answer, `GET run`, stats; same version again, other bytes, lower versions; registered files can't change, the same bytes can be re-uploaded); signatures (none, another key, a release key, other bytes, no `DATA_PUBKEYS`, a key that isn't where kind/id/version say); contents (the agent's rules: unknown fields, templates per kind, per-lease fields, path-like strings, sizes, times; unregistered or wrong-kind builds, game, runtime, expired, gens that don't match registered checkpoints, a body naming another run); legacy leases only for older agents and only until a definition exists, then `{"update"}`; leases only with the definition's build, runtime, current gen and job kinds; eval batches only with the signed seats' networks and pairing; expiry; agents download `defs/`; `done` with `refused` and crash kind `definition`; one new version per gen |
| `access.test.ts` | Cloudflare Access (a locally generated RSA key and JWKS behind a stubbed fetch): a valid JWT is the admin, JWKS cached; service tokens; the bearer token still works; refused: wrong `aud`/`iss`, expired, not yet valid, no identity, other `alg`, another signer, tampered payload, unknown kid (refetched at most once a minute); several AUDs; `ACCESS_EMAILS`; no Access configured → JWTs mean nothing; the page's Access detection and token fallback |
| `limits.test.ts` | security review hardening: chunks and bytes per lease (413, nothing left in R2), results until 24 h after a lease closed, rates from credited ids only, fuzz failures inside the lease, `MAX_ACTIVE_LEASES` (expired leases don't count), crash rows per machine per day (blocks still recorded), crash and diagnostics retention (R2 objects and rows), enroll limits keyed by an HMAC of the IP (IPv6 by /64) and deleted within minutes, the coordinator mark on every `/v1/*` answer (errors, 405, artifacts, 206) and not elsewhere, malformed percent-encoding → 400, installers pinned to `GITHUB_REPO`'s `agent-v<version>/ZeroFarmSetup.exe` |
| `external.test.ts` | the owner's own machines: merged contributors (farm and own), totals by origin, per run; each POST replaces the reporter's sources; offline after 30 min (1-hour numbers drop out, 24-hour ones after a day), deleted after 7 days; validation and size limit; admin only; never on the public page |
| `update.test.ts` | `{"update"}` for agents older than the channel's or a build's `min_agent`, no update loop for builds no release satisfies, `aw` mismatch, enroll → 426 |
| `units.test.ts` | ranges, semver, sizing, BLAKE2b vectors, minisign (prehashed and legacy, tampering, wrong key, wrong trusted comment), telemetry adapter |
| `fleet.test.ts` | 10 fake machines × 6 generations against the real Worker, with the real lease cap (2); rev 6: 4 run an agent older than `DEFINITION_AGENT`, the campaign is signed from the start and the run once gen 2 is out (then a new version per gen and batch), every definition lease is checked as an agent would (signature, run, build, runtime, checkpoints, per-lease fields only), legacy agents get legacy self-play leases before the cutover and nothing after; random vanishing (some crash and lose games in progress, one dies for good holding a lease), pauses past the slack, late results, double and conflicting uploads, checkpoint switches, fuzz backfill, and two paired eval batches (gen 2 v 1, then the current gen v 1) whose games finish in random order, so pairs are split across chunks and left half-played by dropouts; a fake trainer fills each generation by cursor and reads the eval batches by `ids` + `also`. Asserts every game id and seed is claimed exactly once or back in the pool, every generation filled by exactly `quota` accepted games, accepted + in flight never over the cap, ≥ 3 machines per generation, the dead machine's games played by others, every eval pair credited exactly once with both games from one lease, eval chunks never refused as stale |

Test notes:
- `TEST_CLOCK=1` (set only in `vitest.config.ts`) lets tests move time with an `x-test-now` header.
  It is not in `wrangler.jsonc`, so a deployed Worker ignores the header.
- `vitest.config.ts` sets `RELEASE_PUBKEYS` to empty: `wrangler.jsonc` carries the real release
  key, and the test builds carry fake signatures (signature checks have their own unit tests).
  It sets `DATA_PUBKEYS` to a throwaway test data key (its secret half is in `test/helpers.ts`;
  nothing else trusts it), so test definitions are really signed and verified; `IP_SALT`; and
  `MAX_ACTIVE_LEASES` to 100 (tests hold several leases per machine on purpose; the cap's own test
  and the fleet use 2). `call(..., {env})` overrides vars for one request (the Worker's checks;
  LeaseBook keeps the bindings).
- Rate limits are on in every test. Tests that fire many requests at one frozen clock time space
  them out with `advance()`; enroll is limited per `cf-connecting-ip`, which only the rate-limit
  test sends.
- The helpers call the Worker's `fetch` handler directly (with the real local bindings) instead of
  `SELF.fetch`: in vitest-pool-workers 0.23 every `SELF` request wraps the entrypoint prototype in
  one more `Proxy`, so the thousands of requests in the fleet test become quadratic and overflow the
  stack. Durable Object calls still go through real stubs.
- `@cloudflare/vitest-pool-workers` was renamed `@cloudflare/vitest-plugin` (1.x) upstream; 0.23
  works with vitest 4.1 but gets no more updates. Moving is `npx @cloudflare/codemods
  vitest:pool-workers-to-vitest-plugin`.
- Analytics Engine is simulated by Miniflare (writes go nowhere); `src/telemetry.ts` is a no-op
  without the binding, and `units.test.ts` checks the data points with a fake dataset.
- `npm audit` flags sharp and undici under wrangler/miniflare: dev-only, nothing deployed.

## Deployed (2026-11-11)

Live at [https://farm.greggg230.com](https://farm.greggg230.com) (Worker `zero-farm`, custom domain).
D1 `zero-farm` = 095e93bc-c26c-4b69-ada1-d6293ed52041 (WNAM), migration 0001 applied; R2 bucket
`zero-farm` (private); Analytics Engine dataset `zero_farm`; cron every minute. `ADMIN_TOKEN` is a
Worker secret; the owner keeps the only other copy in a local file outside the repo (never print
it; `FARM_ADMIN_TOKEN="$(cat <that file>)"` for the CLI). Smoke-tested: admin auth and stats, invite ->
enroll -> invite reuse refused -> lease (`no_work`) -> revoke -> lease refused (test rows deleted).
`RELEASE_PUBKEYS` holds release key 1B9342EFFB546500 (step 6 done). Not done: step 9 (Cloudflare
Access; rev 6 adds the support, see "Cloudflare Access" below).
Redeploy: `npm run deploy` from this directory (migrations first, then code). The 11-12
follow-ups add migration `0002_followups.sql` (rate limits, installers on releases, user
diagnostics flag); `npm run deploy` applies it before the code that needs it. Rev 4 (installers
on GitHub Releases) adds `0003_github_releases.sql` (`agent_releases.github_url`). After deploying
rev 4, `/download` is 404 until a release names a `github_url` (`farm-release github-release`).
Rev 5 (eval batches, blocked builds) adds `0004_eval_blocks.sql` (`eval_batches`, `blocks`,
`results.batch/held/also`, `crashes.os_error`); deploy it before agents 0.2.3 go out, so their
`blocked` reports are understood (an older coordinator answers 400 to that kind; the agent then
sends it as `crash`).

### Deploy notes: rev 6 (signed definitions, Access, limits, contributors)

Migration `0005_definitions_limits.sql` adds `definitions`, `external_sources`,
`leases.def_version`, `checkpoints.reference_size` and indexes, and deletes the old
plaintext-IP rate-limit rows. New vars (in `wrangler.jsonc`, so a deploy sets them):

| Var | Default | What |
|---|---|---|
| `DATA_PUBKEYS` | the data key `RWTImfGf...` (id 1FDB907D9FF199C8) | the only keys that may sign definitions (comma separated) |
| `DEFINITION_AGENT` | `0.2.5` | the first agent that verifies definitions |
| `GITHUB_REPO` | `greggg230/zero-farm` | where installers (`github_url`) must come from |
| `MAX_ACTIVE_LEASES` | `2` | live leases per machine |
| `MAX_LEASE_CHUNKS` / `MAX_LEASE_BYTES` | `2000` / `1073741824` | result chunks / bytes per lease |
| `LATE_RESULTS_S` | `86400` | results are taken until this long after a lease closed |
| `MAX_CRASHES_PER_DAY` | `100` | crash rows kept per machine per 24 h |
| `CRASH_RETENTION_DAYS` / `DIAG_RETENTION_DAYS` | `30` / `14` | the cron deletes older crash rows / diagnostics (objects and rows) |

Secrets (`npx wrangler secret put NAME`; they stay out of this public repo, and a deploy doesn't
reset them):
- `IP_SALT`: the HMAC key for client IPs in enroll rate-limit rows, e.g.
  `node -e "console.log(require('crypto').randomBytes(32).toString('base64url'))"`. Without it
  each Worker instance draws a random key when it starts (so a new one with every deploy), and
  instances then don't share enroll limits.
- `ACCESS_TEAM_DOMAIN` (`<team>` or `<team>.cloudflareaccess.com`), `ACCESS_AUD` (the Access
  application's Application Audience (AUD) tag; comma separated for several) and optionally
  `ACCESS_EMAILS` (comma separated emails and service-token client ids allowed as admin): only
  once the Access application below exists.

Order:
1. `npx wrangler secret put IP_SALT`, then `npm run deploy` (migration 0005, then the code).
   Agents up to 0.2.4 keep working: nothing has a definition yet, so they get legacy leases.
2. Sign and register a definition for every active run and campaign (`farm-release define ...
   --publish`, or `npm run admin -- definition <file>`), check `stats` / the dashboard's "Signed
   definitions" (no problems listed), then release agent 0.2.5. From the first definition on, older
   agents get nothing from that run or campaign (and `{"update"}` once 0.2.5 is on their channel).
3. Cloudflare Access (below), then its secrets.

### Cloudflare Access (security review H1)

With Access, the dashboard needs no token box (the page asks once without one and hides the box
when the coordinator answers), and a leaked bearer token alone no longer opens `/v1/admin` from
outside. Steps for the owner, in the Cloudflare dashboard (Zero Trust):
1. **Access → Applications → Add an application → Self-hosted.** One application with two
   destinations: `farm.greggg230.com` path `admin`, and `farm.greggg230.com` path `v1/admin`
   (one application, so the browser's one login covers the page and the API calls it makes).
   Never a path that covers `/v1/` as a whole: friends' agents must reach `/v1/lease` etc. without
   Access.
2. **Policies:** an Allow policy for the owner's email; a **Service Auth** policy with a new service
   token (Access → Service credentials → Service Tokens) for the CLI, the trainer's pool remote and
   `farm-release`. Note the token's client id and secret.
3. Copy the application's **Application Audience (AUD) tag** (its overview page) and the team
   domain (Settings → Custom pages / your `<team>.cloudflareaccess.com`), then
   `npx wrangler secret put ACCESS_TEAM_DOMAIN`, `... ACCESS_AUD`, and optionally `...
   ACCESS_EMAILS` (the owner's email and the service token's client id).
4. The desktop tools keep their bearer token and add the service token: `CF_ACCESS_CLIENT_ID` and
   `CF_ACCESS_CLIENT_SECRET` for `scripts/admin.ts`; the trainer's pool remote and `farm-release`
   need the same two headers (`CF-Access-Client-Id`, `CF-Access-Client-Secret`) before step 1's
   `v1/admin` destination goes live, or they get Access's login page instead of the API.
5. Check: [https://farm.greggg230.com/admin](https://farm.greggg230.com/admin) shows "signed in
   through Cloudflare Access", and `npm run admin -- stats` still works.

## Deploy checklist (as originally written)

1. `npx wrangler login` (or `CLOUDFLARE_API_TOKEN` with Workers, D1, R2 and Analytics Engine edit).
2. D1: `npx wrangler d1 create zero-farm` and put the printed `database_id` in `wrangler.jsonc`
   (it holds a placeholder of zeros).
3. R2: `npx wrangler r2 bucket create zero-farm`. Keep it private (no public bucket, no r2.dev
   URL): everything is served through the Worker.
4. Analytics Engine: nothing to create; the `zero_farm` dataset appears on first write (Workers
   Paid includes AE). Query it later with the SQL API.
5. Secret: `npx wrangler secret put ADMIN_TOKEN` with a fresh random value, e.g.
   `node -e "console.log(require('crypto').randomBytes(32).toString('base64url'))"`. Keep it in
   the desktop's secret store for the trainer and the CLI.
6. Release key: once the minisign key exists, put its public key (the base64 line of
   `minisign.pub`; several comma-separated during a rotation) in `vars.RELEASE_PUBKEYS`. Until then
   the coordinator only checks that each build's signature file is present (`sig_verified: false`);
   agents verify signatures regardless.
7. Custom domain: the `routes` entry for `farm.greggg230.com` in `wrangler.jsonc` (the zone must
   be on the same Cloudflare account).
8. `npm run deploy`: type-checks, applies D1 migrations remotely, deploys. Later schema changes are
   new files in `migrations/`; apply them before deploying code that needs them.
9. Cloudflare Access: see "Cloudflare Access" above (rev 6: `/admin` and `/v1/admin`, a service
   token for the desktop tools, the `ACCESS_*` secrets).
10. Smoke test: `FARM_URL=https://farm.greggg230.com FARM_ADMIN_TOKEN=... npm run admin -- stats`,
    then open [https://farm.greggg230.com/admin](https://farm.greggg230.com/admin).
11. For local development, run `npm install` and `npm run db:local` once, then `npm run dev`.

## Design notes

**Scopes and LeaseBook.** Each run (`run:<slug>`), campaign (`campaign:<id>`) and the diag queue
(`diag`) is one `LeaseBook` Durable Object instance. All state lives in D1; the object only
serialises. While a Durable Object awaits D1 or R2, other requests to it can start, so each method
runs through a promise-chain lock, and each logical change (grant, credit, close) is one atomic
`db.batch`. Expected errors cross the RPC boundary as values (`Outcome`), because RPC drops custom
error fields.

**Id accounting.** Game ids come from `runs.next_gid` (self-play and eval share it); seeds from
`campaigns.cursor`, which only moves forward. Every id below the counter is exactly one of:
claimed (`claims`: credited to the first result chunk that reported it), unclaimed inside an active
lease's `[lo, hi)`, or pooled (`pool`). A lease is one contiguous range, from the lowest pooled
range first, else fresh. Closing a lease (done, given back, expired, cancelled) puts
`[lo, hi) − claims` back in the pool, so ids an agent never reported are reissued whatever its
`unfinished` says. Results declare their ids in `x-ids` because the coordinator can't read records.
A late chunk can claim ids that were reissued (the other lease's copy then counts as a duplicate
and its `claimed` counter rises) or are back in the pool (they leave it). The fleet test checks
that claims and pool tile every id handed out.

**Quota.** For the current generation: accepted (the trainer's total, `quotas`) + in flight
(unclaimed ids of active self-play leases + credited self-play games not yet acked) must stay
within `floor(quota × (1 + slack))`; the last lease is trimmed to fit. A new checkpoint starts a new
count, and leases already out carry over as in flight (they switch checkpoint at their next game
boundary, told through beats). Late credits can't push the total past the cap: freed ids are
reissued before any fresh id is.

**Choosing and sizing.** Pull only. For a lease request: diag requests for that machine first;
then each active run's self-play (needs a checkpoint and quota headroom) and eval (unissued batch
ids), scored priority × fit ÷ (1 + active leases of that job), which shares machines by priority;
then fuzz as the backfill. Fit: `gpu: required` excludes CPU-only PCs, `gpu: preferred` moves the
run to the backfill tier at ×0.25 on them, fuzz is ×0.5 on GPU PCs, cached builds ×1.2, a cached
checkpoint ×1.1; `needs` and disk must fit the caps. Size = ~450 s × the machine's rate for that
(kind, scope), an EWMA of ids per running second (paused time excluded), else its hardware class's
average, else defaults (16 games, 10 eval games, 20000 seeds), clamped. `games_per_lease` / `chunk`
override.

**Expiry.** A lease expires when it misses 3 beats (`expires_at` = last beat + 180 s). Expired leases
are swept before every grant, beat, result and done of their scope, and by the minute cron.
Paused leases keep beating; past `slack_s` the agent gives back, and the coordinator answers
`give_back` too if it doesn't.

**Results.** The Worker reads the body (≤ 32 MB), checks its sha256 and writes it to R2 *before*
asking LeaseBook to commit: 32 MB doesn't fit through Durable Object RPC. The key carries the hash,
so conflicting bodies for one seq never overwrite each other; whichever commits first wins and the
Worker deletes the other's object. Retries of a stored chunk are answered from D1 without touching
R2. Self-play chunks older than `max_stale` generations are refused (410) and their ids stay
unclaimed, so they get reissued rather than lost. The trainer lists results by cursor per run or
campaign. Each entry says which ids are first copies (`ids`) and which are duplicates (`dup_ids`).
The trainer posts `accept` (its total for the generation) then `ack`. R2 objects are deleted 7 days
after ack.

**Rate limits.** One D1 row per (machine, bucket) holding a GCRA "theoretical arrival time":
a request at `now` is allowed while `tat - now <= (burst - 1) * interval` and moves `tat` to
`max(tat, now) + interval`, in one conditional upsert (`RETURNING` tells allowed from refused, so
two concurrent requests can't both take the last slot); a refused request writes nothing.
Buckets: lease 5 s × 6, beat 10 s × 12, crash 10 s × 30, user diagnostics 1 h × 1, enroll
60 s × 10 per `cf-connecting-ip` (no header, as in local tools: not limited). A limited request
costs one D1 batch (upsert + select) after auth, instead of the lease's dozen queries and Durable
Object calls. The cron deletes rows whose `tat` is a day old.

**Onboarding and installers.** `GET /` is a static page with the newest installer's version
filled in and a link to the source code and releases on GitHub; `GET /download` answers 302 to
the newest stable release's `github_url`, else canary's (`x-farm-channel` says which), skipping
one older than its channel's `min_agent`. Installers live only on GitHub Releases (PROTOCOL
2.1, rev 4): `POST /v1/admin/agent/release` names one with `github_url`, which must be a GitHub
release asset URL. The coordinator stores and serves no installer bytes (rev 3's R2 `setup/`
objects and `setup_*` columns are unused).

**Diagnostics.** Requested uploads (`upload_diag`) are unchanged. A friend's "Send diagnostics"
(`x-farm-diag: user`) needs no request: body ≤ 20 MB is read first (so an oversize upload doesn't
use up the hour), then the hourly bucket is taken. `diag_uploads.user` marks them; the dashboard
lists recent uploads with a download button.

**Updates.** `/v1/lease` answers `{"update": {"min_agent"}}` when the agent is older than its
channel's latest release's `min_agent`, or older than the `min_agent` of a build it could otherwise
get, provided the channel's latest release satisfies it. Builds no release satisfies yet are
skipped, so an agent is never sent into an update loop.

**Eval batches (rev 5).** A batch is a run's `eval` config plus a row in `eval_batches` (game ids
`[lo, hi)`, paired or not, its checkpoints, and `spec`: the canonical JSON of `games`,
`checkpoints` and `job`, so a re-put with another spec is refused and a name is never reused).
Putting a batch checks every gen is registered; a run's gens may be registered in any order (an
older one doesn't become current). A paired batch reserves from an even id (an odd `next_gid`
gives its id to the self-play pool), and its pool, leases and claims stay pair-aligned: a lease
takes an even count from the lowest pooled range, and only whole pairs are ever claimed. Crediting
(`splitPaired` in `book.ts`): a chunk's ids minus those already claimed are split into whole pairs
(credited here), lone games whose mate an earlier chunk of the same lease holds (credited here,
the mate through an `also` entry naming that chunk), and lone games whose mate hasn't come (held:
stored on the result row, not claimed). Claims for a completed pair point at the completing
chunk. A held game is never claimed, so a lease that ends with one puts the whole pair back in the
pool; a late chunk of that lease can still complete it if nobody else has. Eval leases list every
network (`checkpoints`), go only to agents 0.2.3+, and their uncached checkpoints count against
the disk cap; their chunks skip the staleness check. Progress (`evalProgress`): credited ids in
the batch's range, unclaimed ids in active leases, pooled ids. The coordinator never reads rows,
so the dashboard shows progress, not a score.

**Blocked builds (rev 5).** `/v1/crash` kind `blocked` (Windows refused to start a worker: Smart
App Control / WDAC 4551, AppLocker / SRP 1260, Defender 225) adds a `blocks` row (machine,
build, the reporting agent version, `until` = now + 24 h). On `/v1/lease` the machine's blocks are
read first; expired ones, and ones a newer agent has outgrown, are deleted; candidates whose build
(or runtime) is blocked are skipped, so the machine gets other work or `no_work`. A new build has
another id, so it isn't blocked. `POST /v1/admin/machine/{id}` `{"unblock": true}` deletes them;
the dashboard lists them under the machine. Found on 2026-11-12: a friend's PC with Smart App
Control got 521 leases in 45 minutes, each given back, held back only by the lease rate limit.

**Telemetry and the dashboard.** Beats and `done` write a data point per Stats sample to Analytics
Engine (index = machine; 20 doubles, the most AE takes, the last being `starved_ms`;
`paused_ms` and `resume_ms_max` stay in D1). The dashboard reads D1 summaries via
`/v1/admin/stats`: machines (last seen, work, busy/paused hours, availability as idle h/day of
on h/day from the profile, yields, hours starved of CPU, give-backs, expiries, crashes),
throughput, runs (quota, cap, accepted, in flight, the current eval batch's games done), eval
batches (networks, paired, a progress bar, games done, in leases, unissued, state), campaigns,
rates by hardware class, agent releases per channel (with the installer's GitHub link), recent
diagnostics uploads, recent crashes, new fuzz failure sites, and per machine the builds Windows
blocked there. A Worker can write to AE but can only read it through the account's SQL API.

**Signed definitions (rev 6, PROTOCOL 7).** `definitions` holds every registered version per
(kind, id); the current one is the highest. Registering (`PUT /v1/admin/definition`, in the Worker,
then `putDefinition` in the scope's LeaseBook, so it is serialised with grants) checks the bytes'
sha256, the signature against `DATA_PUBKEYS` only (the same minisign code as builds), the agent's
own `definition/1` rules (`src/definitions.ts` mirrors `agent/common/src/definition.rs`, so a
definition every agent would refuse is never registered) and the coordinator's (registered builds
of the right kind and game, gens matching registered checkpoints, versions only going up). A grant
reads the scope's current definition: an agent of at least `DEFINITION_AGENT` (the Worker passes
`def_aware`) gets a lease only under one, an older agent only when there is none; the lease names
it (`definition: {key, sha256}`, `leases.def_version`) and its `job` holds only the per-lease
fields. A job the definition doesn't cover (build, runtime, unsigned current gen, eval networks or
pairing, expiry, no template) gets no lease, and `definition.problems` in `GET run` / stats says
why. Beats offer a newer gen only if the lease's own definition signs it. Diag leases never go to
definition-aware agents. The run's base seed stays the coordinator's (templates can't carry one).

**Limits and retention (rev 6, security review M3/M4).** Result chunks and bytes per lease are
checked twice: in the Worker before the body is read (declared length) and in LeaseBook with the
real size (concurrent uploads), so a refused chunk's R2 object is deleted. A machine's live leases
(`state = 'active'` and not past `expires_at`) are counted before choosing; concurrent requests
of one machine can overshoot by its burst, at most. The cron deletes crash rows, diagnostics
uploads and diag-lease results (objects and rows) and old diagnostics requests after their
retention, 500 per kind per minute.

**Cloudflare Access.** `authAdmin` accepts the bearer token first, then (with `ACCESS_TEAM_DOMAIN`
and `ACCESS_AUD` set) a `Cf-Access-Jwt-Assertion`: RS256 only, key by `kid` from the team's JWKS
(cached an hour; an unknown kid refetches at most once a minute), `iss`, `aud`, `exp`/`nbf`/`iat`
with a minute's leeway, and the optional `ACCESS_EMAILS` allowlist (email, or a service token's
`common_name`). Stats answer `auth`, which the page uses to hide the token box. The token, when
used, lives in `sessionStorage` (this tab only).

**Contributors.** `POST /v1/admin/external` stores a reporter's sources in `external_sources` (one
D1 batch: delete the reporter's rows, insert the new ones). Stats merge them with the farm's PCs
(credited ids per machine and kind from `results` over 1 h / 24 h): `contributors`, `totals`
(farm, own, all; an offline source's last hour doesn't count, its day only while its report is
under a day old) and `by_scope` (farm numbers per run/campaign plus the own sources working on
it; reporters give no per-run numbers).

**Not done / open:**
- The eval score (and its error, Elo) is the desktop's: the coordinator never reads rows, so the
  dashboard shows a batch's progress only.
- `outside_hours` is agent-side only. There is no contributors page yet (DESIGN 5).
- Result uploads and `done` are not rate-limited (a reconnecting agent may upload many chunks at
  once; rev 6 caps them per lease instead); invite codes carry 60 bits and tokens 256.
- The first-lease 60 s benchmark from DESIGN 5.1 isn't a separate job: rates come from real leases
  (with hardware-class averages before that).

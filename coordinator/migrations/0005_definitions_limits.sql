-- zero-farm coordinator, schema v5 (PROTOCOL v1 rev 6): signed definitions, upload and retention
-- limits (security review 2026-11-12, M3/M4), keyed client-IP rate limits, and contributions
-- reported for the owner's own machines (POST /v1/admin/external).

-- Signed definitions (PROTOCOL 7), one row per registered version. The current definition of a
-- run or campaign is its highest version; versions only ever go up. The definition's own bytes
-- (and .minisig) stay in R2 at `key` = defs/<kind>/<id>/<version>/definition.json; the columns
-- below are its parsed fields (JSON where they are lists or objects).
CREATE TABLE definitions (
  kind TEXT NOT NULL,                -- run | campaign
  id TEXT NOT NULL,                  -- the run or campaign slug
  version INTEGER NOT NULL,
  key TEXT NOT NULL,
  sha256 TEXT NOT NULL,              -- of definition.json
  size INTEGER NOT NULL,
  created INTEGER NOT NULL,
  expires INTEGER,                   -- agents refuse it after this time; no leases then either
  game TEXT NOT NULL,
  builds TEXT NOT NULL,              -- JSON [worker build_id, ...]
  runtime TEXT,                      -- runtime build_id or NULL
  checkpoints TEXT NOT NULL,         -- JSON {"<gen>": {model_sha256, model_size, reference_sha256, reference_size}}
  jobs TEXT NOT NULL,                -- JSON {selfplay, eval, fuzz}: templates or null
  key_id TEXT NOT NULL,              -- hex id of the data key that signed it
  registered_at INTEGER NOT NULL,
  PRIMARY KEY (kind, id, version)
);

-- The definition version a lease was issued with (NULL: a legacy lease, for agents older than
-- DEFINITION_AGENT while its run or campaign had no definition).
ALTER TABLE leases ADD COLUMN def_version INTEGER;

-- A checkpoint's reference size, compared with a definition's reference_size.
ALTER TABLE checkpoints ADD COLUMN reference_size INTEGER;

-- The daily crash cap per machine and the retention sweeps.
CREATE INDEX crashes_machine_t ON crashes(machine_id, t);
CREATE INDEX diag_uploads_t ON diag_uploads(t);
CREATE INDEX diag_requests_created ON diag_requests(created_at);

-- Enroll rate limits were keyed by the client IP in clear ('ip:<address>'); they are now keyed by
-- an HMAC of it ('ip:<hmac>', ratelimit.ts). The old rows go.
DELETE FROM rate_limits WHERE key LIKE 'ip:%';

-- Contributions of the owner's own machines that don't run the agent (the desktop playing
-- locally, machines reached over SSH, rented boxes), pushed by a desktop reporter. Each POST
-- replaces all rows of its reporter. Admin page only. seen_at = when the coordinator got the POST
-- (offline after 30 min, deleted after 7 days); t = the reporter's own clock.
CREATE TABLE external_sources (
  reporter TEXT NOT NULL,
  id TEXT NOT NULL,
  name TEXT NOT NULL,
  kind TEXT NOT NULL,                -- local | ssh | vast | other
  hardware TEXT,
  working_on TEXT NOT NULL,          -- JSON ["run:<run>", "campaign:<id>", ...]
  selfplay_1h INTEGER NOT NULL,
  selfplay_24h INTEGER NOT NULL,
  eval_1h INTEGER NOT NULL,
  eval_24h INTEGER NOT NULL,
  fuzz_1h INTEGER NOT NULL,
  fuzz_24h INTEGER NOT NULL,
  busy_h_24h REAL,
  t INTEGER NOT NULL,
  seen_at INTEGER NOT NULL,
  PRIMARY KEY (reporter, id)
);

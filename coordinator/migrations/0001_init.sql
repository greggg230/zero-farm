-- zero-farm coordinator, schema v1. Times are Unix seconds; ranges are half-open [lo, hi).

-- Farm-wide switches (key 'paused' = '1' stops all new leases).
CREATE TABLE settings (
  key TEXT PRIMARY KEY,
  value TEXT NOT NULL
);

-- Single-use invite codes (XXXX-XXXX-XXXX).
CREATE TABLE invites (
  code TEXT PRIMARY KEY,
  note TEXT,
  channel TEXT NOT NULL DEFAULT 'stable',  -- the enrolled machine's update channel
  created_at INTEGER NOT NULL,
  used_at INTEGER,
  machine_id TEXT
);

-- A friend's PC. The token itself is never stored, only sha256(token).
CREATE TABLE machines (
  machine_id TEXT PRIMARY KEY,
  token_hash TEXT NOT NULL UNIQUE,
  name TEXT NOT NULL,
  channel TEXT NOT NULL DEFAULT 'stable',
  invite TEXT,
  created_at INTEGER NOT NULL,
  last_seen INTEGER,
  agent_version TEXT,
  profile TEXT,            -- JSON Profile (PROTOCOL 3)
  profile_digest TEXT,     -- the agent's digest of it, compared as an opaque string
  class TEXT,              -- hardware class "<cpu> / <gpu or 'no gpu'>"
  caps TEXT,               -- JSON caps from the last /v1/lease
  paused INTEGER NOT NULL DEFAULT 0,
  revoked_at INTEGER
);
CREATE INDEX machines_class ON machines(class);

-- A verified, signed build (worker, runtime pack or agent), registered by POST /v1/admin/build.
CREATE TABLE builds (
  build_id TEXT PRIMARY KEY,
  kind TEXT NOT NULL,
  version TEXT NOT NULL,
  game TEXT,
  profile TEXT,
  min_agent TEXT,
  aw INTEGER,
  size INTEGER NOT NULL,      -- sum of file sizes
  manifest TEXT NOT NULL,     -- manifest.json exactly as stored
  sig_verified INTEGER NOT NULL,
  created_at INTEGER NOT NULL
);

-- Agent releases per channel; the newest row per channel is "latest".
CREATE TABLE agent_releases (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  channel TEXT NOT NULL,
  version TEXT NOT NULL,
  build_id TEXT NOT NULL,
  min_agent TEXT NOT NULL,
  t INTEGER NOT NULL
);
CREATE INDEX agent_releases_channel ON agent_releases(channel, id);

-- A training run that uses the pool. Game ids (self-play and eval) come from next_gid.
CREATE TABLE runs (
  run TEXT PRIMARY KEY,
  game TEXT NOT NULL,
  build_id TEXT NOT NULL,
  runtime TEXT,
  priority REAL NOT NULL,
  state TEXT NOT NULL,          -- active | paused
  selfplay TEXT,                -- JSON config or NULL
  eval TEXT,                    -- JSON config or NULL
  eval_batch TEXT,              -- the eval batch whose game ids are reserved
  next_gid INTEGER NOT NULL DEFAULT 0,
  current_gen INTEGER,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL
);

CREATE TABLE checkpoints (
  run TEXT NOT NULL,
  gen INTEGER NOT NULL,
  model TEXT NOT NULL,
  sha256 TEXT NOT NULL,
  reference TEXT,
  reference_sha256 TEXT,
  size INTEGER,
  t INTEGER NOT NULL,
  PRIMARY KEY (run, gen)
);

-- The trainer's count of accepted games per generation (POST .../accept).
CREATE TABLE quotas (
  run TEXT NOT NULL,
  gen INTEGER NOT NULL,
  accepted INTEGER NOT NULL DEFAULT 0,
  t INTEGER NOT NULL,
  PRIMARY KEY (run, gen)
);

-- A fuzz campaign. cursor only moves forward: seeds below it were handed out or are in the pool.
CREATE TABLE campaigns (
  campaign TEXT PRIMARY KEY,
  build_id TEXT NOT NULL,
  mode TEXT NOT NULL,
  env TEXT NOT NULL,            -- JSON
  seed_base INTEGER NOT NULL,
  cursor INTEGER NOT NULL,
  chunk TEXT NOT NULL,          -- "auto" or a seed count
  priority REAL NOT NULL,
  state TEXT NOT NULL,
  needs TEXT,                   -- JSON {threads, ram_mb} or NULL
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL
);

-- Leases. scope = 'run:<run>' | 'campaign:<id>' | 'diag'; every change to a scope's leases,
-- pool and claims goes through that scope's LeaseBook Durable Object.
CREATE TABLE leases (
  lease_id TEXT PRIMARY KEY,
  machine_id TEXT NOT NULL,
  scope TEXT NOT NULL,
  run TEXT,
  campaign TEXT,
  kind TEXT NOT NULL,           -- fuzz | eval | selfplay | diag
  pool TEXT,                    -- 'selfplay' | 'eval:<batch>' | 'fuzz'; NULL for diag
  lo INTEGER,                   -- ids [lo, hi): game ids or seeds
  hi INTEGER,
  claimed INTEGER NOT NULL DEFAULT 0,     -- ids in [lo, hi) credited by any result (active leases)
  units_done INTEGER NOT NULL DEFAULT 0,  -- ids this lease's results reported (incl. duplicates)
  gen INTEGER,                  -- checkpoint generation at issue
  build_id TEXT NOT NULL,
  diag_id INTEGER,
  lease TEXT NOT NULL,          -- the Lease JSON as sent
  state TEXT NOT NULL,          -- active | done | given_back | expired | cancelled
  pending TEXT,                 -- 'cancel' | 'give_back': the answer to the next beat
  beat_s INTEGER NOT NULL,
  slack_s INTEGER NOT NULL,
  issued_at INTEGER NOT NULL,
  expires_at INTEGER NOT NULL,
  last_beat INTEGER,
  beat_state TEXT,              -- running | paused (last beat)
  paused_since INTEGER,
  paused_s INTEGER NOT NULL DEFAULT 0,
  progress TEXT,
  stats TEXT,
  closed_at INTEGER,
  close_reason TEXT
);
CREATE INDEX leases_expiry ON leases(state, expires_at);
CREATE INDEX leases_scope ON leases(scope, state, lo);
CREATE INDEX leases_machine ON leases(machine_id, issued_at);
CREATE INDEX leases_issued ON leases(issued_at);

-- Ids handed back (expired, given back, cancelled leases), waiting to be reissued.
CREATE TABLE pool (
  scope TEXT NOT NULL,
  pool TEXT NOT NULL,
  lo INTEGER NOT NULL,
  hi INTEGER NOT NULL,
  PRIMARY KEY (scope, pool, lo)
);

-- Ids credited to a result: the first copy of a game id / seed wins. Disjoint per scope.
CREATE TABLE claims (
  scope TEXT NOT NULL,
  lo INTEGER NOT NULL,
  hi INTEGER NOT NULL,
  lease_id TEXT NOT NULL,
  result_id INTEGER NOT NULL
);
CREATE INDEX claims_scope_lo ON claims(scope, lo);

-- Uploaded result chunks. id is the trainer's cursor.
CREATE TABLE results (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  scope TEXT NOT NULL,
  lease_id TEXT NOT NULL,
  seq INTEGER NOT NULL,
  machine_id TEXT NOT NULL,
  kind TEXT NOT NULL,
  gen INTEGER,
  key TEXT NOT NULL,
  sha256 TEXT NOT NULL,
  size INTEGER NOT NULL,
  games INTEGER NOT NULL,       -- ids credited to this chunk (first copies)
  ids TEXT NOT NULL,            -- JSON ranges credited
  dup_ids TEXT NOT NULL,        -- JSON ranges another chunk had already credited
  t INTEGER NOT NULL,
  acked_at INTEGER,
  deleted_at INTEGER,
  UNIQUE (lease_id, seq)
);
CREATE INDEX results_scope ON results(scope, id);
CREATE INDEX results_pending ON results(scope) WHERE acked_at IS NULL;
CREATE INDEX results_acked ON results(acked_at) WHERE acked_at IS NOT NULL AND deleted_at IS NULL;
CREATE INDEX results_t ON results(t);

-- Fuzz failures parsed from fuzz results (PROTOCOL 5.1).
CREATE TABLE failures (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  campaign TEXT NOT NULL,
  seed INTEGER NOT NULL,
  mode TEXT,
  site TEXT NOT NULL,
  panic TEXT,
  repro TEXT,
  output_tail TEXT,
  build_id TEXT,
  engine_commit TEXT,
  machine_id TEXT,
  lease_id TEXT,
  result_id INTEGER,
  first_of_site INTEGER NOT NULL,
  t INTEGER NOT NULL,
  UNIQUE (campaign, seed, site)
);
CREATE INDEX failures_campaign ON failures(campaign, id);

-- Measured rate per machine, job kind and scope: ids (games or seeds) per running second.
CREATE TABLE rates (
  machine_id TEXT NOT NULL,
  kind TEXT NOT NULL,
  scope TEXT NOT NULL,
  rate REAL NOT NULL,
  samples INTEGER NOT NULL,
  t INTEGER NOT NULL,
  PRIMARY KEY (machine_id, kind, scope)
);

-- POST /v1/crash.
CREATE TABLE crashes (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  machine_id TEXT NOT NULL,
  build_id TEXT,
  kind TEXT NOT NULL,
  message TEXT,
  backtrace TEXT,
  lease_id TEXT,
  agent_version TEXT,
  t INTEGER NOT NULL
);
CREATE INDEX crashes_t ON crashes(t);

-- Diagnostics asked for from the desktop, one row per machine. kind 'logs' goes out as
-- upload_diag in beats; the others are diag leases.
CREATE TABLE diag_requests (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  machine_id TEXT NOT NULL,
  kind TEXT NOT NULL,           -- sweep | profile | stability | logs
  params TEXT,
  created_at INTEGER NOT NULL,
  delivered_at INTEGER,
  lease_id TEXT,
  done_at INTEGER
);
CREATE INDEX diag_requests_machine ON diag_requests(machine_id, done_at);

CREATE TABLE diag_uploads (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  machine_id TEXT NOT NULL,
  name TEXT NOT NULL,
  key TEXT NOT NULL,
  size INTEGER NOT NULL,
  request_id INTEGER,
  t INTEGER NOT NULL
);

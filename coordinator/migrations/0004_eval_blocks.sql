-- zero-farm coordinator, schema v4 (PROTOCOL v1 rev 5): evaluation batches with several networks
-- and paired games, and worker builds a machine reported as blocked by Windows.

-- Every eval batch a run has had. The current one is runs.eval_batch; game ids [lo, hi) were
-- reserved from runs.next_gid when it was put (a paired batch starts on an even id). spec is the
-- canonical JSON of {games, checkpoints, job}: a batch can't change once it exists, and a batch
-- name is never used twice in a run. Batches put before this migration (none in production) have
-- no row and are not offered.
CREATE TABLE eval_batches (
  run TEXT NOT NULL,
  batch TEXT NOT NULL,
  lo INTEGER NOT NULL,
  hi INTEGER NOT NULL,
  games INTEGER NOT NULL,
  paired INTEGER NOT NULL,
  checkpoints TEXT NOT NULL,      -- JSON [gen, ...], the order leases list them in
  spec TEXT NOT NULL,
  created_at INTEGER NOT NULL,
  replaced_at INTEGER,            -- set when another batch (or none) took its place
  PRIMARY KEY (run, batch)
);

-- Eval result chunks: the batch, the ids reported here that wait for their pair-mate (held, JSON
-- ranges), and the held ids of earlier chunks of the same lease that this chunk completed and
-- credited (also, JSON [{"seq", "key", "ids"}]). results.games counts ids and also together.
ALTER TABLE results ADD COLUMN batch TEXT;
ALTER TABLE results ADD COLUMN held TEXT;
ALTER TABLE results ADD COLUMN also TEXT;

-- POST /v1/crash kind 'blocked': Windows refused to start this worker build on this machine
-- (Smart App Control / WDAC, AppLocker / SRP, Defender). The build isn't offered to the machine
-- until `until`, or once it asks for work with an agent newer than agent_version.
CREATE TABLE blocks (
  machine_id TEXT NOT NULL,
  build_id TEXT NOT NULL,
  agent_version TEXT,
  os_error INTEGER,
  message TEXT,
  t INTEGER NOT NULL,
  until INTEGER NOT NULL,
  PRIMARY KEY (machine_id, build_id)
);

-- The os error of a crash report (kind 'blocked': 4551, 1260 or 225).
ALTER TABLE crashes ADD COLUMN os_error INTEGER;

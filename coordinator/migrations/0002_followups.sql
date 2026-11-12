-- zero-farm coordinator, schema v2 (2026-11-11 follow-ups): rate limits, installers attached to
-- agent releases, user-initiated diagnostics uploads.

-- Request rate limits (GCRA): one row per (key, bucket). key = machine id, or 'ip:<address>' for
-- /v1/enroll. tat = the "theoretical arrival time" of the next request (Unix seconds): a request
-- at `now` is allowed while tat - now <= (burst - 1) * interval, and moves tat to
-- max(tat, now) + interval. Rows untouched for a day are pruned by the cron.
CREATE TABLE rate_limits (
  key TEXT NOT NULL,
  bucket TEXT NOT NULL,          -- lease | beat | crash | enroll | diag_user
  tat REAL NOT NULL,
  PRIMARY KEY (key, bucket)
);
CREATE INDEX rate_limits_tat ON rate_limits(tat);

-- ZeroFarmSetup.exe uploaded with an agent release (R2 key under setup/), served by GET /download.
ALTER TABLE agent_releases ADD COLUMN setup_key TEXT;
ALTER TABLE agent_releases ADD COLUMN setup_sha256 TEXT;
ALTER TABLE agent_releases ADD COLUMN setup_size INTEGER;

-- 1 = the friend pressed "Send diagnostics" (x-farm-diag: user), 0 = asked for from the desktop.
ALTER TABLE diag_uploads ADD COLUMN user INTEGER NOT NULL DEFAULT 0;

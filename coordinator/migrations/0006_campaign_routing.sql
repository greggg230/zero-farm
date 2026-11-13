-- zero-farm coordinator, schema v6 (PROTOCOL v1 rev 7): capability routing for campaigns.
-- A run's `gpu` and `providers` live in its `selfplay` / `eval` JSON; a campaign gets columns.
-- gpu = any | preferred | required (campaigns put before rev 7: any); providers = JSON list of
-- provider names the campaign's jobs may use, or NULL for any provider.
ALTER TABLE campaigns ADD COLUMN gpu TEXT NOT NULL DEFAULT 'any';
ALTER TABLE campaigns ADD COLUMN providers TEXT;

-- zero-farm coordinator, schema v3 (PROTOCOL v1 rev 4): installers live on GitHub Releases.
-- GET /download redirects (302) to the newest release's github_url. The setup_key, setup_sha256
-- and setup_size columns from 0002 are no longer written or read (installers are not stored in
-- R2 any more); they stay only because dropping columns gains nothing.
ALTER TABLE agent_releases ADD COLUMN github_url TEXT;

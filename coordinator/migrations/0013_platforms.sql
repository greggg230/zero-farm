-- Rev 17 (PROTOCOL 11): platforms (macOS on Apple silicon joins the farm). Everything before rev 17 was Windows x64,
-- so existing machines, builds and agent releases are 'windows-x64'.
ALTER TABLE machines ADD COLUMN platform TEXT NOT NULL DEFAULT 'windows-x64';        -- fixed at enrollment (x-farm-platform)
ALTER TABLE builds ADD COLUMN platform TEXT NOT NULL DEFAULT 'windows-x64';          -- the manifest's `platform`
ALTER TABLE agent_releases ADD COLUMN platform TEXT NOT NULL DEFAULT 'windows-x64';  -- latest = newest per (channel, platform)
CREATE INDEX agent_releases_channel_platform ON agent_releases(channel, platform, id);
-- A fuzz campaign's worker build per platform (JSON list [{"build_id", "when": {"platform"}}], NULL: none; PROTOCOL 11.4).
ALTER TABLE campaigns ADD COLUMN alternates TEXT;

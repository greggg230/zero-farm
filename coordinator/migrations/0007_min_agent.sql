-- zero-farm coordinator, schema v7 (PROTOCOL v1 rev 9): the oldest agent a run's or campaign's
-- leases may go to. NULL: any agent the other rules allow (the builds' min_agent, rev 5's eval
-- minimum, DEFINITION_AGENT). A newer requirement makes an older agent update first when its
-- channel offers a new enough release, else that run or campaign is skipped for it.
ALTER TABLE runs ADD COLUMN min_agent TEXT;
ALTER TABLE campaigns ADD COLUMN min_agent TEXT;

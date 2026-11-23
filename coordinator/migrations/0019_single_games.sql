-- Rev 30 (PROTOCOL 22): single games. `results.games` counts the ids a result credited, and an id is whatever the job
-- plays under it: one game, or one whole best-of-three match. The trainer, which reads every result, says with its ack
-- how many single games the credited ids of a self-play or eval result held (POST .../ack `single_games`); NULL: not
-- said (every result before rev 30, a trainer that does not count, a result whose count is not known exactly).
ALTER TABLE results ADD COLUMN single_games INTEGER;

-- The same for the owner's own machines (POST /v1/admin/external): of a source's self-play + eval ids in each window,
-- `single_*` = the single games of the ids whose count its runs recorded, `uncounted_*` = the ids without a recorded
-- count. NULL: the reporter did not say (a reporter before rev 30): all of the source's ids are uncounted.
ALTER TABLE external_sources ADD COLUMN single_1h INTEGER;
ALTER TABLE external_sources ADD COLUMN single_24h INTEGER;
ALTER TABLE external_sources ADD COLUMN uncounted_1h INTEGER;
ALTER TABLE external_sources ADD COLUMN uncounted_24h INTEGER;

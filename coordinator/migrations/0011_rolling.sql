-- zero-farm coordinator, schema v11 (PROTOCOL v1 rev 15): rolling self-play leases (section 10). A rolling lease
-- reserves a block of game ids [lo, hi) from the run's next_gid and releases them as its worker asks
-- (POST /v1/lease/{id}/extend): its game ids are [lo, rel_hi). NULL rel_hi: a classic lease, whose ids are all of
-- [lo, hi) (every lease issued before rev 15). `until`: the lease runs to at least this time (issued_at + max_s,
-- moved on by a renewal). `cur_gen`: the newest generation the agent reported in a beat (`gen` stays the first).
ALTER TABLE leases ADD COLUMN rel_hi INTEGER;
ALTER TABLE leases ADD COLUMN until INTEGER;
ALTER TABLE leases ADD COLUMN cur_gen INTEGER;

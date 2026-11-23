-- Rev 28 (PROTOCOL 20): the owner's freshness settings per machine, as a JSON object; NULL: none (as before).
--   {"poll_s": n}  - the machine's rolling leases poll for a new generation every n seconds (POST /v1/lease/{id}/poll);
--   {"share": x}   - the machine's freshness budget is at most x times the run's generation period (PROTOCOL 18).
-- Set with POST /v1/admin/machine/{id} `fresh`; read by the lease, beat, extension and poll answers.
ALTER TABLE machines ADD COLUMN fresh TEXT;

-- Rev 28 (PROTOCOL 20.7): the owner's disk cap for the machine, in MB; NULL: none (the friend's own `disk_cap_mb` applies).
-- Set with POST /v1/admin/machine/{id} `disk_cap_mb` (only with the friend's consent); sent in every /v1/lease answer to
-- agents from DISK_CAP_AGENT, which use it in place of their own setting while it is set.
ALTER TABLE machines ADD COLUMN disk_cap_mb INTEGER;

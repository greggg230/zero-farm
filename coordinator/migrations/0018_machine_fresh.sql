-- Rev 28 (PROTOCOL 20): the owner's freshness settings per machine, as a JSON object; NULL: none (as before).
--   {"poll_s": n}  - the machine's rolling leases poll for a new generation every n seconds (POST /v1/lease/{id}/poll);
--   {"share": x}   - the machine's freshness budget is at most x times the run's generation period (PROTOCOL 18).
-- Set with POST /v1/admin/machine/{id} `fresh`; read by the lease, beat, extension and poll answers.
ALTER TABLE machines ADD COLUMN fresh TEXT;

-- Rev 29 (PROTOCOL 15.8): the priority lane decides with the machine's real ask and learns from its misses.
-- ask: what the machine's last POST /v1/lease offered besides its caps, as JSON {"kinds": [...], "builds": [...]} (the job
--   kinds and the cached builds it listed). The lane (15.3, 15.4) and the renewal (10.4) replay it, so they see the same
--   candidates the next lease request will. NULL (rows from before rev 29): every kind, nothing cached, as before.
-- lane: JSON {"yield": {"scope", "batch", "t"}, "miss": {"<scope>": {"batch", "t"}}}: the eval batch the machine's last
--   lease gave way to (until its next lease request settles it), and the batches it gave way to and then was not given
--   (no lane yield to such a batch for LANE_MISS_S). NULL: none.
ALTER TABLE machines ADD COLUMN ask TEXT;
ALTER TABLE machines ADD COLUMN lane TEXT;

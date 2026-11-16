-- zero-farm coordinator, schema v10 (PROTOCOL v1 rev 14): a fuzz campaign's share. NULL (every campaign put before
-- rev 14, and any put without it): the backfill, as before - fuzz goes after every run's games. A number in (0, 1]:
-- the campaign competes with the runs' games and goes first while its own active leases + 1 are at most share x (all
-- runs' active self-play and eval leases + its own + 1), else after every other run job (it never idles a PC).
-- Set by the desktop's fuzz steward while a build's daily fuzz is behind its pace.
ALTER TABLE campaigns ADD COLUMN share REAL;

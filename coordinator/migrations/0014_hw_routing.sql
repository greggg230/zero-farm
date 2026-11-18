-- Rev 18 (PROTOCOL 12): hardware routing. 1 while the machine's last offer had a run's self-play it plays on its GPU
-- (the CPU PCs' eval capacity leaves such machines out); NULL or 0 otherwise. The owner's switch and knobs live in
-- settings (key 'hw_routing', JSON {on, eval_horizon_s, eval_games_per_thread}; absent: the defaults).
ALTER TABLE machines ADD COLUMN gpu_play INTEGER;

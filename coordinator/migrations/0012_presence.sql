-- Rev 16 (PROTOCOL 3, POST /v1/presence): why an online agent isn't working right now.
ALTER TABLE machines ADD COLUMN presence TEXT;       -- the agent's reason slug (user_active, fullscreen, ...)
ALTER TABLE machines ADD COLUMN presence_t INTEGER;  -- when it was sent

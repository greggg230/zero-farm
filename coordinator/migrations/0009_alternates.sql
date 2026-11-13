-- Rev 11 (PROTOCOL 9.4): a run's alternate worker builds + runtime packs, each offered only to machines that
-- meet its vendor condition (JSON list [{"build_id", "runtime", "when": {"vendor", "min_driver"?}}], NULL: none).
ALTER TABLE runs ADD COLUMN alternates TEXT;

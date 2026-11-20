-- Rev 20 (PROTOCOL 14): model variants per generation. A checkpoint's variants (JSON {"<name>": {"model", "sha256", "size"}}:
-- another model file of the gen, sharing its reference set; NULL: none), and the variant a lease was granted for (its
-- alternate's `model`; NULL: the default model), which its generation switches keep while the alternate still names it.
ALTER TABLE checkpoints ADD COLUMN variants TEXT;
ALTER TABLE leases ADD COLUMN variant TEXT;

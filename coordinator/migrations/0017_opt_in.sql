-- Rev 27 (PROTOCOL 19): opt-in runtimes per machine. The names of the opt-in runtime packs (a runtime manifest's `opt_in`,
-- e.g. "torch") the owner allowed on this machine, as a JSON list of names [a-z0-9-]{1,32}; NULL: none. Set with
-- POST /v1/admin/machine/{id} (opt_in, opt_in_add, opt_in_remove); read by the lease rules for alternates with when.opt_in
-- and for packs with opt_in.
ALTER TABLE machines ADD COLUMN opt_in TEXT;

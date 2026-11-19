-- Rev 19 (PROTOCOL 13): layout tuning. The CPU layouts the machine's agent last reported with a lease request (its
-- `tuning`: a JSON list of at most 4 validated reports - run, build, runtime, net, layout, leaves_s, trials, state, t)
-- and when; NULL before the first report. Hints for a hardware class come from these rows.
ALTER TABLE machines ADD COLUMN tuning TEXT;
ALTER TABLE machines ADD COLUMN tuning_t INTEGER;

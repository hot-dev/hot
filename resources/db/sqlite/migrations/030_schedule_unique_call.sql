-- A schedule is one call: a function, a time and its arguments. Keeping one
-- schedule per function and time made a second call due at the same moment
-- fail with a duplicate key, which lost it. Only the same call again is a
-- repeat. The rows are already unique under this index, so it can't fail on
-- existing data.
CREATE UNIQUE INDEX idx_schedule_unique_call ON schedule(build_id, ns, var, cron, COALESCE(value, ''));

DROP INDEX idx_schedule_unique_function;

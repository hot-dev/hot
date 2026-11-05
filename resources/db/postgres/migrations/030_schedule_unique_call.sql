-- A schedule is one call: a function, a time and its arguments. Keeping one
-- schedule per function and time made a second call due at the same moment
-- fail with a duplicate key, which lost it. Only the same call again is a
-- repeat. md5 because a btree entry can't hold arguments of any size.
--
-- The new index goes first: the table stays readable while it builds, and the
-- rows are already unique under it, so it can't fail on existing data.
CREATE UNIQUE INDEX idx_schedule_unique_call ON hot.schedule USING btree (build_id, ns, var, cron, md5(COALESCE(value::text, '')));

DROP INDEX hot.idx_schedule_unique_function;

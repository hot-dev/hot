-- Migration 013 enforces these relationships on new writes, but its original
-- preflight did not examine retained origin-run or event references. Refuse an
-- upgrade that would leave existing rows unable to receive normal status/data
-- updates. Preserve the rows for an explicit, reviewed repair and retry.
DO $$
DECLARE
    invalid_rows bigint;
    prior_mode text := COALESCE(current_setting('hot.scope_mode', true), '');
    prior_operation text := COALESCE(current_setting('hot.scope_operation', true), '');
BEGIN
    -- The owner is still subject to FORCE RLS. Queue maintenance reads all
    -- retained runs, tasks, and events without disabling policies or BYPASSRLS.
    PERFORM set_config('hot.scope_mode', 'maintenance', true);
    PERFORM set_config('hot.scope_operation', 'queue', true);

    SELECT count(*) INTO invalid_rows FROM hot.run r
        LEFT JOIN hot.run origin ON origin.run_id = r.origin_run_id
        WHERE r.origin_run_id IS NOT NULL
          AND (origin.run_id IS NULL OR r.env_id <> origin.env_id);
    IF invalid_rows > 0 THEN
        RAISE EXCEPTION 'Scope preflight: run has % origin-run/env ownership mismatches', invalid_rows;
    END IF;

    SELECT count(*) INTO invalid_rows FROM hot.task t
        LEFT JOIN hot.run origin ON origin.run_id = t.origin_run_id
        WHERE t.origin_run_id IS NOT NULL
          AND (origin.run_id IS NULL OR t.env_id <> origin.env_id);
    IF invalid_rows > 0 THEN
        RAISE EXCEPTION 'Scope preflight: task has % origin-run/env ownership mismatches', invalid_rows;
    END IF;

    SELECT count(*) INTO invalid_rows FROM hot.run r
        LEFT JOIN hot.event e ON e.event_id = r.event_id
        WHERE r.event_id IS NOT NULL
          AND (e.event_id IS NULL OR r.env_id <> e.env_id);
    IF invalid_rows > 0 THEN
        RAISE EXCEPTION 'Scope preflight: run has % event/env ownership mismatches', invalid_rows;
    END IF;

    PERFORM set_config('hot.scope_mode', prior_mode, true);
    PERFORM set_config('hot.scope_operation', prior_operation, true);
END $$;

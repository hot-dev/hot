-- Match the retained runtime relationship preflight on PostgreSQL. SQLite
-- continues to enforce request scope in application code; do not silently
-- rewrite historical provenance while adopting the shared isolation contract.
CREATE TEMP TABLE retained_runtime_scope_preflight (
    run_origin integer NOT NULL DEFAULT 1
        CONSTRAINT run_origin_environment CHECK (run_origin = 1),
    task_origin integer NOT NULL DEFAULT 1
        CONSTRAINT task_origin_environment CHECK (task_origin = 1),
    run_event integer NOT NULL DEFAULT 1
        CONSTRAINT run_event_environment CHECK (run_event = 1)
);
INSERT INTO retained_runtime_scope_preflight(run_origin)
SELECT 0 FROM run r
LEFT JOIN run origin ON origin.run_id = r.origin_run_id
WHERE r.origin_run_id IS NOT NULL
  AND (origin.run_id IS NULL OR r.env_id <> origin.env_id)
LIMIT 1;
INSERT INTO retained_runtime_scope_preflight(task_origin)
SELECT 0 FROM task t
LEFT JOIN run origin ON origin.run_id = t.origin_run_id
WHERE t.origin_run_id IS NOT NULL
  AND (origin.run_id IS NULL OR t.env_id <> origin.env_id)
LIMIT 1;
INSERT INTO retained_runtime_scope_preflight(run_event)
SELECT 0 FROM run r
LEFT JOIN event e ON e.event_id = r.event_id
WHERE r.event_id IS NOT NULL
  AND (e.event_id IS NULL OR r.env_id <> e.env_id)
LIMIT 1;
DROP TABLE retained_runtime_scope_preflight;

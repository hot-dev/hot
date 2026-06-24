-- Track DB-visible build runtime readiness and deployment ordering.
--
-- Cache contents remain process-local. `env.runtime_revision` is a small
-- shared epoch that lets processes tell when their local runtime-surface cache
-- may be stale.

ALTER TABLE build
    ADD COLUMN runtime_status text NOT NULL DEFAULT 'pending'
        CHECK (runtime_status IN ('pending', 'loading', 'ready', 'failed', 'superseded'));

ALTER TABLE build
    ADD COLUMN runtime_ready_at datetime;

ALTER TABLE build
    ADD COLUMN runtime_error text;

ALTER TABLE build
    ADD COLUMN deployment_sequence integer NOT NULL DEFAULT 0;

ALTER TABLE project
    ADD COLUMN deployment_sequence integer NOT NULL DEFAULT 0;

ALTER TABLE env
    ADD COLUMN runtime_revision integer NOT NULL DEFAULT 0;

UPDATE project
SET deployment_sequence = 1
WHERE EXISTS (
    SELECT 1
    FROM build
    WHERE build.project_id = project.project_id
      AND build.deployed = 1
);

UPDATE env
SET runtime_revision = 1
WHERE EXISTS (
    SELECT 1
    FROM project
    JOIN build ON build.project_id = project.project_id
    WHERE project.env_id = env.env_id
      AND build.deployed = 1
);

UPDATE build
SET runtime_status = 'ready',
    runtime_ready_at = COALESCE(updated_at, created_at),
    deployment_sequence = CASE
        WHEN deployed = 1 THEN (
            SELECT project.deployment_sequence
            FROM project
            WHERE project.project_id = build.project_id
        )
        ELSE 0
    END
WHERE 1 = 1;

CREATE INDEX idx_build_project_deployment_sequence
    ON build(project_id, deployment_sequence);

CREATE INDEX idx_build_project_deployed_ready
    ON build(project_id, runtime_status)
    WHERE deployed = 1 AND runtime_status = 'ready';

CREATE INDEX idx_build_runtime_status
    ON build(runtime_status);

CREATE INDEX idx_env_runtime_revision
    ON env(env_id, runtime_revision);

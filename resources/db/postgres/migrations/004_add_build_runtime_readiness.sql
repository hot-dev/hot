-- Track DB-visible build runtime readiness and deployment ordering.
--
-- Cache contents remain process-local. `env.runtime_revision` is a small
-- shared epoch that lets processes tell when their local runtime-surface cache
-- may be stale.

ALTER TABLE hot.build
    ADD COLUMN runtime_status text NOT NULL DEFAULT 'pending'
        CHECK (runtime_status IN ('pending', 'loading', 'ready', 'failed', 'superseded')),
    ADD COLUMN runtime_ready_at timestamp with time zone,
    ADD COLUMN runtime_error text,
    ADD COLUMN deployment_sequence bigint NOT NULL DEFAULT 0;

ALTER TABLE hot.project
    ADD COLUMN deployment_sequence bigint NOT NULL DEFAULT 0;

ALTER TABLE hot.env
    ADD COLUMN runtime_revision bigint NOT NULL DEFAULT 0;

UPDATE hot.project
SET deployment_sequence = 1
WHERE EXISTS (
    SELECT 1
    FROM hot.build
    WHERE build.project_id = project.project_id
      AND build.deployed = true
);

UPDATE hot.env
SET runtime_revision = 1
WHERE EXISTS (
    SELECT 1
    FROM hot.project
    JOIN hot.build ON build.project_id = project.project_id
    WHERE project.env_id = env.env_id
      AND build.deployed = true
);

UPDATE hot.build
SET runtime_status = 'ready',
    runtime_ready_at = COALESCE(updated_at, created_at),
    deployment_sequence = CASE
        WHEN deployed = true THEN (
            SELECT project.deployment_sequence
            FROM hot.project
            WHERE project.project_id = build.project_id
        )
        ELSE 0
    END;

CREATE INDEX idx_build_project_deployment_sequence
    ON hot.build USING btree (project_id, deployment_sequence);

CREATE INDEX idx_build_project_deployed_ready
    ON hot.build USING btree (project_id, runtime_status)
    WHERE deployed = true AND runtime_status = 'ready';

CREATE INDEX idx_build_runtime_status
    ON hot.build USING btree (runtime_status);

CREATE INDEX idx_env_runtime_revision
    ON hot.env USING btree (env_id, runtime_revision);

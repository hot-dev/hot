-- Store bundle manifest versions so the app/API can surface runtime drift
-- without fetching build artifacts on read paths.

ALTER TABLE build
    ADD COLUMN engine_version text;

ALTER TABLE build
    ADD COLUMN hot_std_version text;

CREATE INDEX idx_build_deployed_bundle_missing_versions
    ON build(build_type_id, runtime_status)
    WHERE deployed = 1
      AND active = 1
      AND (engine_version IS NULL OR hot_std_version IS NULL);

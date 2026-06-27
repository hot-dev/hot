-- Store bundle manifest versions so the app/API can surface runtime drift
-- without fetching build artifacts on read paths.

ALTER TABLE hot.build
    ADD COLUMN engine_version text,
    ADD COLUMN hot_std_version text;

CREATE INDEX idx_build_deployed_bundle_missing_versions
    ON hot.build USING btree (build_type_id, runtime_status)
    WHERE deployed = true
      AND active = true
      AND (engine_version IS NULL OR hot_std_version IS NULL);

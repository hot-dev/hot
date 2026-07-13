-- The partial unique index on file(org_id, env_id, path) treats NULL env_id
-- values as distinct, so environment-less contexts could insert duplicate
-- active records for the same path — defeating the insert race that
-- FileStorage::write_file_if relies on for its create-if-absent arm.
-- Rebuild the index over a NULL-collapsing expression so NULL env_id rows
-- dedupe like any other environment (same pattern as idx_blob_object in 006).
DROP INDEX IF EXISTS hot.idx_file_org_env_path_active_unique;
CREATE UNIQUE INDEX idx_file_org_env_path_active_unique
    ON hot.file USING btree (org_id, COALESCE(env_id, '00000000-0000-0000-0000-000000000000'::uuid), path)
    WHERE (active = true);

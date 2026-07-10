-- The partial unique index on file(org_id, env_id, path) treats NULL env_id
-- values as distinct, so environment-less contexts could insert duplicate
-- active records for the same path — defeating the insert race that
-- FileStorage::write_file_if relies on for its create-if-absent arm.
-- Rebuild the index over a NULL-collapsing expression so NULL env_id rows
-- dedupe like any other environment.
DROP INDEX IF EXISTS idx_file_org_env_path_unique;
CREATE UNIQUE INDEX idx_file_org_env_path_unique
    ON file(org_id, COALESCE(env_id, x'00000000000000000000000000000000'), path)
    WHERE active = 1;

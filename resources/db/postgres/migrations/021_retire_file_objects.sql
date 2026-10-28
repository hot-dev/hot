-- Stored objects that no active file version references. A version retires
-- its object when an overwrite replaces it or a delete deactivates it, in the
-- same transaction as that change, and writers register the object they
-- stage just before committing it. Collection deletes an object once it has
-- been retired for a grace period, so reads that already resolved it can
-- finish, and a crash or failed delete only postpones it.
CREATE TABLE hot.retired_file_object (
    storage_backend text NOT NULL,
    storage_path text NOT NULL,
    org_id uuid NOT NULL,
    env_id uuid,
    retired_at timestamptz NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (storage_backend, storage_path)
);
CREATE INDEX retired_file_object_due_idx ON hot.retired_file_object(storage_backend, retired_at);
-- Collection deletes only objects no active version references.
CREATE INDEX file_active_storage_path_idx ON hot.file(storage_backend, storage_path) WHERE active = true;
ALTER TABLE hot.retired_file_object ENABLE ROW LEVEL SECURITY;
ALTER TABLE hot.retired_file_object FORCE ROW LEVEL SECURITY;
CREATE POLICY retired_object_access ON hot.retired_file_object USING (
    hot.scope_maintenance('retention')
    OR (env_id IS NULL AND hot.scope_org_writes(org_id))
    OR (env_id IS NOT NULL AND hot.scope_env_writes(env_id) AND hot.scope_org_allows(org_id))
) WITH CHECK (
    hot.scope_maintenance('retention')
    OR (env_id IS NULL AND hot.scope_org_writes(org_id))
    OR (env_id IS NOT NULL AND hot.scope_env_writes(env_id) AND hot.scope_org_allows(org_id))
);
CREATE POLICY retention_object_read ON hot.file FOR SELECT USING (hot.scope_maintenance('retention'));

CREATE FUNCTION hot.retire_file_object() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    INSERT INTO hot.retired_file_object(storage_backend, storage_path, org_id, env_id)
    VALUES (OLD.storage_backend, OLD.storage_path, OLD.org_id, OLD.env_id)
    ON CONFLICT (storage_backend, storage_path) DO UPDATE SET retired_at = EXCLUDED.retired_at;
    RETURN NULL;
END $$;
-- A committed object is referenced again, so it is no longer collectable.
CREATE FUNCTION hot.adopt_file_object() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    DELETE FROM hot.retired_file_object
    WHERE storage_backend = NEW.storage_backend AND storage_path = NEW.storage_path;
    RETURN NULL;
END $$;
CREATE TRIGGER retire_file_object AFTER UPDATE OF storage_path, active ON hot.file
    FOR EACH ROW WHEN (OLD.active AND COALESCE(OLD.storage_path, '') <> ''
        AND (NOT COALESCE(NEW.active, false) OR NEW.storage_path IS DISTINCT FROM OLD.storage_path))
    EXECUTE FUNCTION hot.retire_file_object();
CREATE TRIGGER adopt_file_object_insert AFTER INSERT ON hot.file
    FOR EACH ROW WHEN (NEW.active AND COALESCE(NEW.storage_path, '') <> '')
    EXECUTE FUNCTION hot.adopt_file_object();
CREATE TRIGGER adopt_file_object_update AFTER UPDATE OF storage_path ON hot.file
    FOR EACH ROW WHEN (NEW.active AND COALESCE(NEW.storage_path, '') <> ''
        AND NEW.storage_path IS DISTINCT FROM OLD.storage_path)
    EXECUTE FUNCTION hot.adopt_file_object();

-- Deleted files' objects were removed best-effort, so some remain. Retire
-- every inactive version's object no active version shares, under the same
-- FORCE RLS contract used at runtime.
DO $$
DECLARE m text:=COALESCE(current_setting('hot.scope_mode',true),''); op text:=COALESCE(current_setting('hot.scope_operation',true),'');
    org text:=COALESCE(current_setting('hot.scope_org',true),''); env text:=COALESCE(current_setting('hot.scope_env',true),'');
BEGIN
    PERFORM hot.apply_scope('maintenance','','','[]','[]','retention','');
    INSERT INTO hot.retired_file_object(storage_backend, storage_path, org_id, env_id)
    SELECT DISTINCT ON (f.storage_backend, f.storage_path) f.storage_backend, f.storage_path, f.org_id, f.env_id
    FROM hot.file f
    WHERE NOT COALESCE(f.active, false) AND COALESCE(f.storage_path, '') <> ''
      AND NOT EXISTS (SELECT 1 FROM hot.file a WHERE a.active
          AND a.storage_backend = f.storage_backend AND a.storage_path = f.storage_path)
    ON CONFLICT DO NOTHING;
    PERFORM hot.apply_scope(m,org,env,'[]','[]',op,'');
END $$;

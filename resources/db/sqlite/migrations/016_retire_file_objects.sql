-- Stored objects that no active file version references. A version retires
-- its object when an overwrite replaces it or a delete deactivates it, in the
-- same transaction as that change, and writers register the object they
-- stage just before committing it. Collection deletes an object once it has
-- been retired for a grace period, so reads that already resolved it can
-- finish, and a crash or failed delete only postpones it.
CREATE TABLE retired_file_object (
    storage_backend TEXT NOT NULL,
    storage_path TEXT NOT NULL,
    org_id BLOB NOT NULL,
    env_id BLOB,
    retired_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    PRIMARY KEY (storage_backend, storage_path)
);
CREATE INDEX retired_file_object_due_idx ON retired_file_object(storage_backend, retired_at);
-- Collection deletes only objects no active version references.
CREATE INDEX file_active_storage_path_idx ON file(storage_backend, storage_path) WHERE active = 1;

CREATE TRIGGER retire_file_object AFTER UPDATE OF storage_path, active ON file
WHEN OLD.active = 1 AND COALESCE(OLD.storage_path, '') <> ''
    AND (NEW.active IS NOT 1 OR NEW.storage_path IS NOT OLD.storage_path)
BEGIN
    INSERT INTO retired_file_object(storage_backend, storage_path, org_id, env_id)
    VALUES (OLD.storage_backend, OLD.storage_path, OLD.org_id, OLD.env_id)
    ON CONFLICT (storage_backend, storage_path) DO UPDATE SET retired_at = excluded.retired_at;
END;
-- A committed object is referenced again, so it is no longer collectable.
CREATE TRIGGER adopt_file_object_insert AFTER INSERT ON file
WHEN NEW.active = 1 AND COALESCE(NEW.storage_path, '') <> ''
BEGIN
    DELETE FROM retired_file_object
    WHERE storage_backend = NEW.storage_backend AND storage_path = NEW.storage_path;
END;
CREATE TRIGGER adopt_file_object_update AFTER UPDATE OF storage_path ON file
WHEN NEW.active = 1 AND COALESCE(NEW.storage_path, '') <> ''
    AND NEW.storage_path IS NOT OLD.storage_path
BEGIN
    DELETE FROM retired_file_object
    WHERE storage_backend = NEW.storage_backend AND storage_path = NEW.storage_path;
END;

-- Deleted files' objects were removed best-effort, so some remain. Retire
-- every inactive version's object no active version shares.
INSERT INTO retired_file_object(storage_backend, storage_path, org_id, env_id)
SELECT f.storage_backend, f.storage_path, f.org_id, f.env_id FROM file f
WHERE f.active IS NOT 1 AND COALESCE(f.storage_path, '') <> ''
  AND NOT EXISTS (SELECT 1 FROM file a WHERE a.active = 1
      AND a.storage_backend = f.storage_backend AND a.storage_path = f.storage_path)
ON CONFLICT DO NOTHING;

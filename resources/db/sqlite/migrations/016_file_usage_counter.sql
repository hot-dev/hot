-- Active file bytes per environment (or organization, for a file without an
-- environment) keep the storage quota check from summing every active file of
-- an account on each write. SQLite has a single writer, so every count lands
-- in slot 0. A file's organization and environment never change, and files are
-- deactivated rather than deleted, but a delete still uncounts its bytes.
CREATE TABLE usage_file (
    org_id blob NOT NULL, env_id blob, slot smallint NOT NULL CHECK(slot BETWEEN 0 AND 15),
    active_bytes bigint NOT NULL DEFAULT 0
);
-- A file without an environment counts under the zero sentinel, matching
-- PostgreSQL's UNIQUE NULLS NOT DISTINCT.
CREATE UNIQUE INDEX usage_file_org_id_env_id_slot_key
    ON usage_file(org_id, COALESCE(env_id, X'00000000000000000000000000000000'), slot);
CREATE TRIGGER count_file_bytes_insert AFTER INSERT ON file
WHEN (CASE WHEN COALESCE(NEW.active,0) THEN MAX(NEW.size,0) ELSE 0 END)<>0
BEGIN
    INSERT INTO usage_file(org_id,env_id,slot,active_bytes) VALUES(NEW.org_id,NEW.env_id,0,MAX(NEW.size,0))
    ON CONFLICT(org_id,COALESCE(env_id,X'00000000000000000000000000000000'),slot) DO UPDATE SET active_bytes=active_bytes+excluded.active_bytes;
END;
CREATE TRIGGER count_file_bytes_update AFTER UPDATE OF size, active ON file
WHEN (CASE WHEN COALESCE(NEW.active,0) THEN MAX(NEW.size,0) ELSE 0 END)
    -(CASE WHEN COALESCE(OLD.active,0) THEN MAX(OLD.size,0) ELSE 0 END)<>0
BEGIN
    INSERT INTO usage_file(org_id,env_id,slot,active_bytes) VALUES(NEW.org_id,NEW.env_id,0,
        (CASE WHEN COALESCE(NEW.active,0) THEN MAX(NEW.size,0) ELSE 0 END)
        -(CASE WHEN COALESCE(OLD.active,0) THEN MAX(OLD.size,0) ELSE 0 END))
    ON CONFLICT(org_id,COALESCE(env_id,X'00000000000000000000000000000000'),slot) DO UPDATE SET active_bytes=active_bytes+excluded.active_bytes;
END;
CREATE TRIGGER count_file_bytes_delete AFTER DELETE ON file
WHEN (CASE WHEN COALESCE(OLD.active,0) THEN MAX(OLD.size,0) ELSE 0 END)<>0
BEGIN
    INSERT INTO usage_file(org_id,env_id,slot,active_bytes) VALUES(OLD.org_id,OLD.env_id,0,-MAX(OLD.size,0))
    ON CONFLICT(org_id,COALESCE(env_id,X'00000000000000000000000000000000'),slot) DO UPDATE SET active_bytes=active_bytes+excluded.active_bytes;
END;
INSERT INTO usage_file(org_id,env_id,slot,active_bytes)
SELECT org_id,env_id,0,SUM(MAX(size,0)) FROM file WHERE COALESCE(active,0) GROUP BY org_id,env_id;
CREATE TRIGGER immutable_usage_file BEFORE UPDATE OF org_id,env_id,slot ON usage_file
WHEN NEW.org_id IS NOT OLD.org_id OR NEW.env_id IS NOT OLD.env_id OR NEW.slot IS NOT OLD.slot
BEGIN SELECT RAISE(ABORT,'Usage ownership is immutable'); END;

ALTER TABLE call ADD COLUMN env_id blob NOT NULL DEFAULT X'00000000000000000000000000000000';
-- TEXT length counts Unicode characters; storage admission counts UTF-8 bytes.
-- Keep the established fixed per-call overhead and include persisted errors.
UPDATE call SET env_id = (SELECT env_id FROM run WHERE run.run_id = call.run_id), size =
    COALESCE(length(CAST(args AS BLOB)), 0) +
    COALESCE(length(CAST(return_value AS BLOB)), 0) +
    COALESCE(length(CAST(flow AS BLOB)), 0) +
    COALESCE(length(CAST(error AS BLOB)), 0) + 50;

CREATE INDEX call_env_start_time_idx ON call(env_id, start_time);
CREATE TRIGGER call_environment_insert BEFORE INSERT ON call
WHEN NOT EXISTS (SELECT 1 FROM run r WHERE r.run_id=NEW.run_id AND r.env_id=NEW.env_id)
BEGIN SELECT RAISE(ABORT, 'Call environment does not own its run'); END;
CREATE TRIGGER call_environment_update BEFORE UPDATE OF env_id, run_id ON call
WHEN NEW.env_id IS NOT OLD.env_id OR NEW.run_id IS NOT OLD.run_id
BEGIN SELECT RAISE(ABORT, 'Call ownership is immutable'); END;

CREATE TABLE usage_storage (
    env_id blob NOT NULL, org_id blob NOT NULL, slot smallint NOT NULL CHECK(slot BETWEEN 0 AND 15),
    store_bytes bigint NOT NULL DEFAULT 0, call_bytes bigint NOT NULL DEFAULT 0,
    PRIMARY KEY(env_id,slot)
);
CREATE INDEX usage_storage_org_idx ON usage_storage(org_id);
CREATE TRIGGER count_call_bytes_insert AFTER INSERT ON call
WHEN (MAX(NEW.size,0))<>0
BEGIN
    INSERT INTO usage_storage(env_id,org_id,slot,call_bytes) VALUES(NEW.env_id,(SELECT org_id FROM env WHERE env_id=NEW.env_id),0,MAX(NEW.size,0))
    ON CONFLICT(env_id,slot) DO UPDATE SET call_bytes=call_bytes+excluded.call_bytes;
END;
CREATE TRIGGER count_call_bytes_update AFTER UPDATE OF size ON call
WHEN (MAX(NEW.size,0)-MAX(OLD.size,0))<>0
BEGIN
    INSERT INTO usage_storage(env_id,org_id,slot,call_bytes) VALUES(NEW.env_id,(SELECT org_id FROM env WHERE env_id=NEW.env_id),0,MAX(NEW.size,0)-MAX(OLD.size,0))
    ON CONFLICT(env_id,slot) DO UPDATE SET call_bytes=call_bytes+excluded.call_bytes;
END;
CREATE TRIGGER count_call_bytes_delete AFTER DELETE ON call
WHEN (-MAX(OLD.size,0))<>0
BEGIN
    INSERT INTO usage_storage(env_id,org_id,slot,call_bytes) VALUES(OLD.env_id,(SELECT org_id FROM env WHERE env_id=OLD.env_id),0,-MAX(OLD.size,0))
    ON CONFLICT(env_id,slot) DO UPDATE SET call_bytes=call_bytes+excluded.call_bytes;
END;
CREATE TRIGGER count_store_bytes_insert AFTER INSERT ON store_map_entry
WHEN (MAX(NEW.size,0))<>0
BEGIN
    INSERT INTO usage_storage(env_id,org_id,slot,store_bytes) VALUES(NEW.env_id,NEW.org_id,0,MAX(NEW.size,0))
    ON CONFLICT(env_id,slot) DO UPDATE SET store_bytes=store_bytes+excluded.store_bytes;
END;
CREATE TRIGGER count_store_bytes_update AFTER UPDATE OF size ON store_map_entry
WHEN (MAX(NEW.size,0)-MAX(OLD.size,0))<>0
BEGIN
    INSERT INTO usage_storage(env_id,org_id,slot,store_bytes) VALUES(NEW.env_id,NEW.org_id,0,MAX(NEW.size,0)-MAX(OLD.size,0))
    ON CONFLICT(env_id,slot) DO UPDATE SET store_bytes=store_bytes+excluded.store_bytes;
END;
CREATE TRIGGER count_store_bytes_delete AFTER DELETE ON store_map_entry
WHEN (-MAX(OLD.size,0))<>0
BEGIN
    INSERT INTO usage_storage(env_id,org_id,slot,store_bytes) VALUES(OLD.env_id,OLD.org_id,0,-MAX(OLD.size,0))
    ON CONFLICT(env_id,slot) DO UPDATE SET store_bytes=store_bytes+excluded.store_bytes;
END;
INSERT INTO usage_storage(env_id,org_id,slot,store_bytes,call_bytes)
SELECT x.env_id,x.org_id,0,SUM(x.store_bytes),SUM(x.call_bytes) FROM (
    SELECT env_id,org_id,CASE WHEN size<0 THEN 0 ELSE size END AS store_bytes,0 AS call_bytes FROM store_map_entry
    UNION ALL SELECT c.env_id,e.org_id,0,CASE WHEN c.size<0 THEN 0 ELSE c.size END FROM call c JOIN env e ON e.env_id=c.env_id
) x GROUP BY x.env_id,x.org_id;
CREATE TRIGGER immutable_usage_storage BEFORE UPDATE OF env_id,org_id,slot ON usage_storage
WHEN NEW.env_id IS NOT OLD.env_id OR NEW.org_id IS NOT OLD.org_id OR NEW.slot IS NOT OLD.slot
BEGIN SELECT RAISE(ABORT,'Usage ownership is immutable'); END;
CREATE TRIGGER immutable_run_usage_owner BEFORE UPDATE OF env_id ON run
WHEN NEW.env_id IS NOT OLD.env_id
BEGIN SELECT RAISE(ABORT,'Run ownership is immutable'); END;
CREATE TRIGGER immutable_store_usage_owner BEFORE UPDATE OF env_id,org_id ON store_map_entry
WHEN NEW.env_id IS NOT OLD.env_id OR NEW.org_id IS NOT OLD.org_id
BEGIN SELECT RAISE(ABORT,'Store ownership is immutable'); END;
CREATE TRIGGER immutable_environment_usage_owner BEFORE UPDATE OF org_id ON env
WHEN NEW.org_id IS NOT OLD.org_id
BEGIN SELECT RAISE(ABORT,'Environment ownership is immutable'); END;

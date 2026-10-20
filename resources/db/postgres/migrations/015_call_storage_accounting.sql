ALTER TABLE hot.call ADD COLUMN env_id uuid;
-- Reconcile historical trace bytes before enforcing account call-storage caps.
-- The migration owner may be NOSUPERUSER NOBYPASSRLS under FORCE RLS.
DO $$
DECLARE
    owners jsonb;
    owner jsonb;
    prior_mode text := COALESCE(current_setting('hot.scope_mode', true), '');
    prior_operation text := COALESCE(current_setting('hot.scope_operation', true), '');
    prior_org text := COALESCE(current_setting('hot.scope_org', true), '');
    prior_env text := COALESCE(current_setting('hot.scope_env', true), '');
BEGIN
    PERFORM set_config('hot.scope_mode', 'bootstrap', true);
    PERFORM set_config('hot.scope_operation', 'membership', true);
    SELECT COALESCE(jsonb_agg(jsonb_build_object('org', org_id, 'env', env_id)), '[]'::jsonb)
      INTO owners FROM hot.env;
    FOR owner IN SELECT value FROM jsonb_array_elements(owners) LOOP
        PERFORM hot.apply_scope('environment', owner->>'org', owner->>'env', '[]', '[]', '', '');
        UPDATE hot.call c SET env_id = r.env_id, size =
            COALESCE(octet_length(c.args::text), 0)::bigint +
            COALESCE(octet_length(c.return_value::text), 0) +
            COALESCE(octet_length(c.flow::text), 0) +
            COALESCE(octet_length(c.error), 0) + 50
        FROM hot.run r
        WHERE r.run_id = c.run_id AND r.env_id = (owner->>'env')::uuid;
    END LOOP;
    PERFORM hot.apply_scope(prior_mode, prior_org, prior_env, '[]', '[]', prior_operation, '');
END $$;

-- Keep trace ownership local to each row, including cascaded retention deletes.
ALTER TABLE hot.call ALTER COLUMN env_id SET NOT NULL;
CREATE UNIQUE INDEX run_env_ownership_idx ON hot.run(run_id, env_id);
ALTER TABLE hot.call ADD CONSTRAINT call_run_environment_fk
    FOREIGN KEY (run_id, env_id) REFERENCES hot.run(run_id, env_id) ON DELETE CASCADE;
CREATE INDEX call_env_start_time_idx ON hot.call(env_id, start_time);
DROP TRIGGER immutable_resource_scope ON hot.call;
CREATE TRIGGER immutable_resource_scope BEFORE UPDATE ON hot.call
    FOR EACH ROW EXECUTE FUNCTION hot.prevent_scope_reassignment('run_id', 'env_id');
DROP POLICY hot_scope_read ON hot.call;
DROP POLICY hot_scope_insert ON hot.call;
DROP POLICY hot_scope_update ON hot.call;
DROP POLICY hot_scope_delete ON hot.call;
CREATE POLICY hot_scope_read ON hot.call FOR SELECT USING ((hot.scope_env_allows(env_id) OR (hot.scope_maintenance('queue') OR hot.scope_maintenance('scheduler') OR hot.scope_maintenance('retention') OR hot.scope_maintenance('alerts')) OR hot.scope_billing_env(env_id)));
CREATE POLICY hot_scope_insert ON hot.call FOR INSERT WITH CHECK ((hot.scope_env_writes(env_id) OR (hot.scope_maintenance('queue') OR hot.scope_maintenance('scheduler') OR hot.scope_maintenance('retention') OR hot.scope_maintenance('alerts'))));
CREATE POLICY hot_scope_update ON hot.call FOR UPDATE USING ((hot.scope_env_writes(env_id) OR (hot.scope_maintenance('queue') OR hot.scope_maintenance('scheduler') OR hot.scope_maintenance('retention') OR hot.scope_maintenance('alerts')))) WITH CHECK ((hot.scope_env_writes(env_id) OR (hot.scope_maintenance('queue') OR hot.scope_maintenance('scheduler') OR hot.scope_maintenance('retention') OR hot.scope_maintenance('alerts'))));
CREATE POLICY hot_scope_delete ON hot.call FOR DELETE USING ((hot.scope_env_writes(env_id) OR (hot.scope_maintenance('queue') OR hot.scope_maintenance('scheduler') OR hot.scope_maintenance('retention') OR hot.scope_maintenance('alerts'))));

CREATE TABLE hot.usage_storage (
    env_id uuid NOT NULL, org_id uuid NOT NULL, slot smallint NOT NULL CHECK(slot BETWEEN 0 AND 15),
    store_bytes bigint NOT NULL DEFAULT 0, call_bytes bigint NOT NULL DEFAULT 0,
    PRIMARY KEY(env_id,slot)
);
CREATE INDEX usage_storage_org_idx ON hot.usage_storage(org_id);
CREATE FUNCTION hot.count_call_bytes() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE delta bigint; owner_org uuid;
BEGIN
    delta:=GREATEST(NEW.size,0)-CASE WHEN TG_OP='UPDATE' THEN GREATEST(OLD.size,0) ELSE 0 END;
    IF delta=0 THEN RETURN NULL; END IF;
    SELECT org_id INTO STRICT owner_org FROM hot.env WHERE env_id=NEW.env_id;
    INSERT INTO hot.usage_storage AS u(env_id,org_id,slot,call_bytes) VALUES(NEW.env_id,owner_org,pg_backend_pid()%16,delta)
    ON CONFLICT(env_id,slot) DO UPDATE SET call_bytes=u.call_bytes+EXCLUDED.call_bytes;
    RETURN NULL;
END $$;
CREATE TRIGGER count_call_bytes_insert AFTER INSERT ON hot.call FOR EACH ROW EXECUTE FUNCTION hot.count_call_bytes();
CREATE TRIGGER count_call_bytes_update AFTER UPDATE OF size ON hot.call
    FOR EACH ROW WHEN(OLD.size IS DISTINCT FROM NEW.size) EXECUTE FUNCTION hot.count_call_bytes();
CREATE FUNCTION hot.uncount_call_bytes() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    INSERT INTO hot.usage_storage AS u(env_id,org_id,slot,call_bytes)
    SELECT d.env_id,e.org_id,pg_backend_pid()%16,-SUM(GREATEST(d.size,0))
    FROM deleted d JOIN hot.env e ON e.env_id=d.env_id GROUP BY d.env_id,e.org_id HAVING SUM(GREATEST(d.size,0))<>0 ORDER BY d.env_id
    ON CONFLICT(env_id,slot) DO UPDATE SET call_bytes=u.call_bytes+EXCLUDED.call_bytes;
    RETURN NULL;
END $$;
CREATE TRIGGER uncount_call_bytes AFTER DELETE ON hot.call
    REFERENCING OLD TABLE AS deleted FOR EACH STATEMENT EXECUTE FUNCTION hot.uncount_call_bytes();
CREATE FUNCTION hot.count_store_bytes() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE delta bigint; owner_org uuid;
BEGIN
    delta:=GREATEST(NEW.size,0)-CASE WHEN TG_OP='UPDATE' THEN GREATEST(OLD.size,0) ELSE 0 END;
    IF delta=0 THEN RETURN NULL; END IF;
    owner_org:=NEW.org_id;
    INSERT INTO hot.usage_storage AS u(env_id,org_id,slot,store_bytes) VALUES(NEW.env_id,owner_org,pg_backend_pid()%16,delta)
    ON CONFLICT(env_id,slot) DO UPDATE SET store_bytes=u.store_bytes+EXCLUDED.store_bytes;
    RETURN NULL;
END $$;
CREATE TRIGGER count_store_bytes_insert AFTER INSERT ON hot.store_map_entry FOR EACH ROW EXECUTE FUNCTION hot.count_store_bytes();
CREATE TRIGGER count_store_bytes_update AFTER UPDATE OF size ON hot.store_map_entry
    FOR EACH ROW WHEN(OLD.size IS DISTINCT FROM NEW.size) EXECUTE FUNCTION hot.count_store_bytes();
CREATE FUNCTION hot.uncount_store_bytes() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    INSERT INTO hot.usage_storage AS u(env_id,org_id,slot,store_bytes)
    SELECT d.env_id,d.org_id,pg_backend_pid()%16,-SUM(GREATEST(d.size,0))
    FROM deleted d GROUP BY d.env_id,d.org_id HAVING SUM(GREATEST(d.size,0))<>0 ORDER BY d.env_id
    ON CONFLICT(env_id,slot) DO UPDATE SET store_bytes=u.store_bytes+EXCLUDED.store_bytes;
    RETURN NULL;
END $$;
CREATE TRIGGER uncount_store_bytes AFTER DELETE ON hot.store_map_entry
    REFERENCING OLD TABLE AS deleted FOR EACH STATEMENT EXECUTE FUNCTION hot.uncount_store_bytes();
DO $$
DECLARE m text:=COALESCE(current_setting('hot.scope_mode',true),''); op text:=COALESCE(current_setting('hot.scope_operation',true),'');
    org text:=COALESCE(current_setting('hot.scope_org',true),''); env text:=COALESCE(current_setting('hot.scope_env',true),''); roots uuid[]; root_id uuid;
BEGIN
    PERFORM hot.apply_scope('maintenance','','','[]','[]','scheduler','');
    -- Store rows require account billing visibility, including retained orphans.
    SELECT COALESCE(array_agg(org_id),ARRAY[]::uuid[]) INTO roots FROM hot.org WHERE parent_org_id IS NULL;
    FOREACH root_id IN ARRAY roots LOOP
        PERFORM hot.apply_scope('maintenance',root_id::text,'','[]','[]','billing','');
INSERT INTO hot.usage_storage(env_id,org_id,slot,store_bytes,call_bytes)
SELECT x.env_id,x.org_id,0,SUM(x.store_bytes),SUM(x.call_bytes) FROM (
    SELECT env_id,org_id,CASE WHEN size<0 THEN 0 ELSE size END AS store_bytes,0 AS call_bytes FROM hot.store_map_entry
    UNION ALL SELECT c.env_id,e.org_id,0,CASE WHEN c.size<0 THEN 0 ELSE c.size END FROM hot.call c JOIN hot.env e ON e.env_id=c.env_id
) x GROUP BY x.env_id,x.org_id;
    END LOOP;
    PERFORM hot.apply_scope(m,org,env,'[]','[]',op,'');
END $$;
ALTER TABLE hot.usage_storage ENABLE ROW LEVEL SECURITY;
ALTER TABLE hot.usage_storage FORCE ROW LEVEL SECURITY;
CREATE POLICY hot_scope_read ON hot.usage_storage FOR SELECT USING ((hot.scope_env_allows(env_id) AND hot.scope_org_allows(org_id)) OR (hot.scope_maintenance('queue') OR hot.scope_maintenance('scheduler') OR hot.scope_maintenance('retention') OR hot.scope_maintenance('alerts')) OR hot.scope_billing_org(org_id));
CREATE POLICY hot_scope_insert ON hot.usage_storage FOR INSERT WITH CHECK ((hot.scope_env_writes(env_id) AND hot.scope_org_allows(org_id)) OR (hot.scope_maintenance('queue') OR hot.scope_maintenance('scheduler') OR hot.scope_maintenance('retention') OR hot.scope_maintenance('alerts')) OR hot.scope_billing_org(org_id));
CREATE POLICY hot_scope_update ON hot.usage_storage FOR UPDATE USING ((hot.scope_env_writes(env_id) AND hot.scope_org_allows(org_id)) OR (hot.scope_maintenance('queue') OR hot.scope_maintenance('scheduler') OR hot.scope_maintenance('retention') OR hot.scope_maintenance('alerts')) OR hot.scope_billing_org(org_id)) WITH CHECK ((hot.scope_env_writes(env_id) AND hot.scope_org_allows(org_id)) OR (hot.scope_maintenance('queue') OR hot.scope_maintenance('scheduler') OR hot.scope_maintenance('retention') OR hot.scope_maintenance('alerts')) OR hot.scope_billing_org(org_id));
CREATE TRIGGER immutable_resource_scope BEFORE UPDATE ON hot.usage_storage
    FOR EACH ROW EXECUTE FUNCTION hot.prevent_scope_reassignment('env_id', 'org_id', 'slot');

-- Active file bytes per environment (or organization, for a file without an
-- environment) keep the storage quota check from summing every active file of
-- an account on each write. Writers spread over 16 slots, as call and store
-- bytes do. A file's organization and environment never change, and files are
-- deactivated rather than deleted, but a delete still uncounts its bytes.
CREATE TABLE hot.usage_file (
    org_id uuid NOT NULL, env_id uuid, slot smallint NOT NULL CHECK(slot BETWEEN 0 AND 15),
    active_bytes bigint NOT NULL DEFAULT 0,
    UNIQUE NULLS NOT DISTINCT (org_id, env_id, slot)
);
CREATE FUNCTION hot.count_file_bytes() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE delta bigint; file_org uuid; file_env uuid;
BEGIN
    delta:=CASE WHEN TG_OP<>'DELETE' AND COALESCE(NEW.active,false) THEN GREATEST(NEW.size,0) ELSE 0 END
        -CASE WHEN TG_OP<>'INSERT' AND COALESCE(OLD.active,false) THEN GREATEST(OLD.size,0) ELSE 0 END;
    IF delta=0 THEN RETURN NULL; END IF;
    IF TG_OP='DELETE' THEN file_org:=OLD.org_id; file_env:=OLD.env_id;
    ELSE file_org:=NEW.org_id; file_env:=NEW.env_id; END IF;
    UPDATE hot.usage_file SET active_bytes=active_bytes+delta
    WHERE org_id=file_org AND env_id IS NOT DISTINCT FROM file_env AND slot=pg_backend_pid()%16;
    IF FOUND THEN RETURN NULL; END IF;
    INSERT INTO hot.usage_file AS u(org_id,env_id,slot,active_bytes)
    VALUES(file_org,file_env,pg_backend_pid()%16,delta)
    ON CONFLICT(org_id,env_id,slot) DO UPDATE SET active_bytes=u.active_bytes+EXCLUDED.active_bytes;
    RETURN NULL;
END $$;
CREATE TRIGGER count_file_bytes_insert AFTER INSERT ON hot.file
    FOR EACH ROW EXECUTE FUNCTION hot.count_file_bytes();
CREATE TRIGGER count_file_bytes_update AFTER UPDATE OF size, active ON hot.file
    FOR EACH ROW WHEN (ROW(OLD.size, OLD.active) IS DISTINCT FROM ROW(NEW.size, NEW.active))
    EXECUTE FUNCTION hot.count_file_bytes();
CREATE TRIGGER count_file_bytes_delete AFTER DELETE ON hot.file
    FOR EACH ROW EXECUTE FUNCTION hot.count_file_bytes();
-- Count existing files under the same FORCE RLS contract used at runtime,
-- including when migrations run as a non-BYPASS schema owner.
DO $$
DECLARE m text:=COALESCE(current_setting('hot.scope_mode',true),''); op text:=COALESCE(current_setting('hot.scope_operation',true),'');
    org text:=COALESCE(current_setting('hot.scope_org',true),''); env text:=COALESCE(current_setting('hot.scope_env',true),''); roots uuid[]; root_id uuid;
BEGIN
    PERFORM hot.apply_scope('maintenance','','','[]','[]','scheduler','');
    SELECT COALESCE(array_agg(org_id),ARRAY[]::uuid[]) INTO roots FROM hot.org WHERE parent_org_id IS NULL;
    FOREACH root_id IN ARRAY roots LOOP
        PERFORM hot.apply_scope('maintenance',root_id::text,'','[]','[]','billing','');
        -- Filter by account explicitly: a superuser or BYPASSRLS migration
        -- role sees every account in every pass.
        INSERT INTO hot.usage_file(org_id,env_id,slot,active_bytes)
        SELECT f.org_id,f.env_id,0,SUM(GREATEST(f.size,0)) FROM hot.file f
        WHERE COALESCE(f.active,false)
          AND f.org_id IN (SELECT o.org_id FROM hot.org o WHERE o.org_id=root_id OR o.parent_org_id=root_id)
        GROUP BY f.org_id,f.env_id;
    END LOOP;
    PERFORM hot.apply_scope(m,org,env,'[]','[]',op,'');
END $$;
ALTER TABLE hot.usage_file ENABLE ROW LEVEL SECURITY;
ALTER TABLE hot.usage_file FORCE ROW LEVEL SECURITY;
CREATE POLICY hot_scope_read ON hot.usage_file FOR SELECT USING (((env_id IS NULL AND hot.scope_org_allows(org_id))) OR ((env_id IS NOT NULL AND hot.scope_env_allows(env_id) AND hot.scope_org_allows(org_id))) OR (hot.scope_billing_org(org_id)));
CREATE POLICY hot_scope_insert ON hot.usage_file FOR INSERT WITH CHECK (((env_id IS NULL AND hot.scope_org_writes(org_id))) OR ((env_id IS NOT NULL AND hot.scope_env_writes(env_id) AND hot.scope_org_allows(org_id))) OR (hot.scope_billing_org(org_id)));
CREATE POLICY hot_scope_update ON hot.usage_file FOR UPDATE USING (((env_id IS NULL AND hot.scope_org_writes(org_id))) OR ((env_id IS NOT NULL AND hot.scope_env_writes(env_id) AND hot.scope_org_allows(org_id))) OR (hot.scope_billing_org(org_id))) WITH CHECK (((env_id IS NULL AND hot.scope_org_writes(org_id))) OR ((env_id IS NOT NULL AND hot.scope_env_writes(env_id) AND hot.scope_org_allows(org_id))) OR (hot.scope_billing_org(org_id)));
CREATE TRIGGER immutable_resource_scope BEFORE UPDATE ON hot.usage_file
    FOR EACH ROW WHEN ((OLD.org_id, OLD.env_id, OLD.slot) IS DISTINCT FROM (NEW.org_id, NEW.env_id, NEW.slot))
    EXECUTE FUNCTION hot.prevent_scope_reassignment('org_id', 'env_id', 'slot');

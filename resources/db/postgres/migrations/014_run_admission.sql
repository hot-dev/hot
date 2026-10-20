-- Admission is durable independently of run retention and emitter delivery.
CREATE TABLE hot.run_admission (
    run_id uuid PRIMARY KEY,
    root_org_id uuid NOT NULL REFERENCES hot.org(org_id),
    org_id uuid NOT NULL REFERENCES hot.org(org_id),
    env_id uuid NOT NULL REFERENCES hot.env(env_id),
    admitted_at timestamptz NOT NULL DEFAULT CURRENT_TIMESTAMP
);
CREATE INDEX run_admission_account_period_idx ON hot.run_admission(root_org_id, admitted_at);
-- Preserve existing usage under the same FORCE RLS contract used at runtime,
-- including when migrations run as a non-BYPASS schema owner.
DO $$
DECLARE
    roots uuid[];
    root_id uuid;
    prior_mode text := COALESCE(current_setting('hot.scope_mode', true), '');
    prior_operation text := COALESCE(current_setting('hot.scope_operation', true), '');
    prior_org text := COALESCE(current_setting('hot.scope_org', true), '');
BEGIN
    PERFORM set_config('hot.scope_mode', 'bootstrap', true);
    PERFORM set_config('hot.scope_operation', 'membership', true);
    SELECT COALESCE(array_agg(org_id), ARRAY[]::uuid[]) INTO roots FROM hot.org WHERE parent_org_id IS NULL;
    PERFORM set_config('hot.scope_mode', 'maintenance', true);
    PERFORM set_config('hot.scope_operation', 'billing', true);
    FOREACH root_id IN ARRAY roots LOOP
        PERFORM set_config('hot.scope_org', root_id::text, true);
        INSERT INTO hot.run_admission(run_id,root_org_id,org_id,env_id,admitted_at)
        SELECT r.run_id,root_id,o.org_id,e.env_id,COALESCE(r.start_time,CURRENT_TIMESTAMP)
        FROM hot.run r JOIN hot.env e ON e.env_id=r.env_id JOIN hot.org o ON o.org_id=e.org_id
        WHERE COALESCE(o.parent_org_id,o.org_id)=root_id;
    END LOOP;
    PERFORM set_config('hot.scope_mode', prior_mode, true);
    PERFORM set_config('hot.scope_operation', prior_operation, true);
    PERFORM set_config('hot.scope_org', prior_org, true);
END $$;
ALTER TABLE hot.run_admission ENABLE ROW LEVEL SECURITY;
ALTER TABLE hot.run_admission FORCE ROW LEVEL SECURITY;
CREATE POLICY run_admission_read ON hot.run_admission FOR SELECT USING (
    hot.scope_env_allows(env_id) OR
    (hot.scope_maintenance('billing') AND root_org_id=hot.scope_org_id())
);
CREATE POLICY run_admission_insert ON hot.run_admission FOR INSERT WITH CHECK (
    hot.scope_maintenance('billing') AND root_org_id=hot.scope_org_id() AND EXISTS (
        SELECT 1 FROM hot.env e JOIN hot.org o ON o.org_id=e.org_id
        WHERE e.env_id=run_admission.env_id AND o.org_id=run_admission.org_id
          AND COALESCE(o.parent_org_id,o.org_id)=run_admission.root_org_id
    )
);
-- No runtime UPDATE/DELETE policy: deleting history cannot reset run capacity.

CREATE TABLE hot.usage_run (
    root_org_id uuid NOT NULL, period_start timestamptz NOT NULL,
    admitted bigint NOT NULL, PRIMARY KEY(root_org_id,period_start)
);
CREATE FUNCTION hot.count_run_admission() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    UPDATE hot.usage_run SET admitted=admitted+1
    WHERE root_org_id=NEW.root_org_id AND period_start<=NEW.admitted_at;
    RETURN NULL;
END $$;
CREATE TRIGGER count_run_admission AFTER INSERT ON hot.run_admission
    FOR EACH ROW EXECUTE FUNCTION hot.count_run_admission();
ALTER TABLE hot.usage_run ENABLE ROW LEVEL SECURITY;
ALTER TABLE hot.usage_run FORCE ROW LEVEL SECURITY;
CREATE POLICY hot_scope_read ON hot.usage_run FOR SELECT USING (hot.scope_maintenance('billing') AND root_org_id=hot.scope_org_id());
CREATE POLICY hot_scope_insert ON hot.usage_run FOR INSERT WITH CHECK (hot.scope_maintenance('billing') AND root_org_id=hot.scope_org_id());
CREATE POLICY hot_scope_update ON hot.usage_run FOR UPDATE USING (hot.scope_maintenance('billing') AND root_org_id=hot.scope_org_id()) WITH CHECK (hot.scope_maintenance('billing') AND root_org_id=hot.scope_org_id());
CREATE POLICY hot_scope_delete ON hot.usage_run FOR DELETE USING (hot.scope_maintenance('billing') AND root_org_id=hot.scope_org_id());
CREATE TRIGGER immutable_resource_scope BEFORE UPDATE ON hot.usage_run
    FOR EACH ROW EXECUTE FUNCTION hot.prevent_scope_reassignment('root_org_id','period_start');

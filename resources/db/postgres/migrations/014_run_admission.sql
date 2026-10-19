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

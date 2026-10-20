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
        UPDATE hot.call c SET size =
            COALESCE(octet_length(c.args::text), 0)::bigint +
            COALESCE(octet_length(c.return_value::text), 0) +
            COALESCE(octet_length(c.flow::text), 0) +
            COALESCE(octet_length(c.error), 0) + 50
        FROM hot.run r
        WHERE r.run_id = c.run_id AND r.env_id = (owner->>'env')::uuid;
    END LOOP;
    PERFORM hot.apply_scope(prior_mode, prior_org, prior_env, '[]', '[]', prior_operation, '');
END $$;

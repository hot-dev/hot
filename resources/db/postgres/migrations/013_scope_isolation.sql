-- Refuse enforcement over inconsistent existing ownership, including retained
-- runtime references and alert deliveries. This runs before any change and
-- reports the table requiring operator repair. Preserve the rows for an
-- explicit, reviewed repair and retry.
DO $$
DECLARE table_name text; invalid_rows bigint;
BEGIN
    FOREACH table_name IN ARRAY ARRAY['store_map','store_map_entry','file','file_upload','blob_object','blob_ref','alert','alert_channel','alert_subscription'] LOOP
        EXECUTE format('SELECT count(*) FROM hot.%I r JOIN hot.env e ON e.env_id = r.env_id WHERE r.org_id <> e.org_id', table_name) INTO invalid_rows;
        IF invalid_rows > 0 THEN RAISE EXCEPTION 'Scope preflight: % has % org/env ownership mismatches', table_name, invalid_rows; END IF;
    END LOOP;
    FOREACH table_name IN ARRAY ARRAY['run','task','agent','workflow'] LOOP
        EXECUTE format('SELECT count(*) FROM hot.%I r JOIN hot.build b ON b.build_id = r.build_id JOIN hot.project p ON p.project_id = b.project_id WHERE r.env_id <> p.env_id', table_name) INTO invalid_rows;
        IF invalid_rows > 0 THEN RAISE EXCEPTION 'Scope preflight: % has % build/env ownership mismatches', table_name, invalid_rows; END IF;
    END LOOP;
    FOREACH table_name IN ARRAY ARRAY['run','task','event'] LOOP
        EXECUTE format('SELECT count(*) FROM hot.%I r JOIN hot.stream s ON s.stream_id = r.stream_id WHERE r.env_id <> s.env_id', table_name) INTO invalid_rows;
        IF invalid_rows > 0 THEN RAISE EXCEPTION 'Scope preflight: % has % stream/env ownership mismatches', table_name, invalid_rows; END IF;
    END LOOP;
    FOREACH table_name IN ARRAY ARRAY['service_key','session'] LOOP
        EXECUTE format('SELECT count(*) FROM hot.%I r JOIN hot.api_key k ON k.api_key_id = r.api_key_id WHERE r.env_id <> k.env_id', table_name) INTO invalid_rows;
        IF invalid_rows > 0 THEN RAISE EXCEPTION 'Scope preflight: % has % credential/env ownership mismatches', table_name, invalid_rows; END IF;
    END LOOP;
    SELECT count(*) INTO invalid_rows FROM hot.context c JOIN hot.project p ON p.project_id = c.project_id
        WHERE c.env_id IS NOT NULL AND c.env_id <> p.env_id;
    IF invalid_rows > 0 THEN RAISE EXCEPTION 'Scope preflight: context has % project/env ownership mismatches', invalid_rows; END IF;
    SELECT count(*) INTO invalid_rows FROM hot.task t JOIN hot.run r ON r.run_id = t.run_id WHERE t.env_id <> r.env_id;
    IF invalid_rows > 0 THEN RAISE EXCEPTION 'Scope preflight: task has % run/env ownership mismatches', invalid_rows; END IF;
    SELECT count(*) INTO invalid_rows FROM hot.blob_ref r JOIN hot.blob_object b ON b.blob_object_id = r.blob_object_id
        WHERE r.org_id <> b.org_id OR r.env_id IS DISTINCT FROM b.env_id;
    IF invalid_rows > 0 THEN RAISE EXCEPTION 'Scope preflight: blob_ref has % object ownership mismatches', invalid_rows; END IF;
    SELECT count(*) INTO invalid_rows FROM hot.alert_subscription_channel l
        JOIN hot.alert_subscription s ON s.alert_subscription_id=l.subscription_id
        JOIN hot.alert_channel c ON c.alert_channel_id=l.channel_id
        WHERE (c.org_id IS NOT NULL AND c.org_id<>s.org_id)
           OR (c.env_id IS NOT NULL AND c.env_id IS DISTINCT FROM s.env_id);
    IF invalid_rows > 0 THEN RAISE EXCEPTION 'Scope preflight: alert subscription has % incompatible channel links', invalid_rows; END IF;
    SELECT count(*) INTO invalid_rows FROM hot.alert_subscription_destination l
        JOIN hot.alert_subscription s ON s.alert_subscription_id=l.subscription_id
        JOIN hot.alert_destination d ON d.alert_destination_id=l.destination_id
        WHERE d.org_id<>s.org_id;
    IF invalid_rows > 0 THEN RAISE EXCEPTION 'Scope preflight: alert subscription has % cross-org destination links', invalid_rows; END IF;
    SELECT count(*) INTO invalid_rows FROM hot.run r
        LEFT JOIN hot.run origin ON origin.run_id = r.origin_run_id
        WHERE r.origin_run_id IS NOT NULL
          AND (origin.run_id IS NULL OR r.env_id <> origin.env_id);
    IF invalid_rows > 0 THEN RAISE EXCEPTION 'Scope preflight: run has % origin-run/env ownership mismatches', invalid_rows; END IF;
    SELECT count(*) INTO invalid_rows FROM hot.task t
        LEFT JOIN hot.run origin ON origin.run_id = t.origin_run_id
        WHERE t.origin_run_id IS NOT NULL
          AND (origin.run_id IS NULL OR t.env_id <> origin.env_id);
    IF invalid_rows > 0 THEN RAISE EXCEPTION 'Scope preflight: task has % origin-run/env ownership mismatches', invalid_rows; END IF;
    SELECT count(*) INTO invalid_rows FROM hot.run r
        LEFT JOIN hot.event e ON e.event_id = r.event_id
        WHERE r.event_id IS NOT NULL
          AND (e.event_id IS NULL OR r.env_id <> e.env_id);
    IF invalid_rows > 0 THEN RAISE EXCEPTION 'Scope preflight: run has % event/env ownership mismatches', invalid_rows; END IF;
    -- Foreign-key checks bypass RLS, so a delivery could reference another
    -- environment's alert, subscription or destination.
    IF EXISTS (
        SELECT 1 FROM hot.alert_delivery d
        JOIN hot.alert a ON a.alert_id = d.alert_id
        JOIN hot.alert_subscription s ON s.alert_subscription_id = d.subscription_id
        JOIN hot.alert_destination t ON t.alert_destination_id = d.destination_id
        WHERE s.org_id <> a.org_id OR t.org_id <> a.org_id
           OR (s.env_id IS NOT NULL AND s.env_id <> a.env_id)
    ) THEN
        RAISE EXCEPTION 'Existing alert delivery ownership mismatch';
    END IF;
END $$;

-- Store each call's environment so trace isolation and retention do not join
-- through run. The same pass reconciles historical trace sizes, which 014's
-- storage accounting sums.
ALTER TABLE hot.call ADD COLUMN env_id uuid;
UPDATE hot.call c SET env_id = r.env_id, size =
    COALESCE(octet_length(c.args::text), 0)::bigint +
    COALESCE(octet_length(c.return_value::text), 0) +
    COALESCE(octet_length(c.flow::text), 0) +
    COALESCE(octet_length(c.error), 0) + 50
FROM hot.run r
WHERE r.run_id = c.run_id;
-- Keep trace ownership local to each row, including cascaded retention deletes.
ALTER TABLE hot.call ALTER COLUMN env_id SET NOT NULL;
CREATE UNIQUE INDEX run_env_ownership_idx ON hot.run(run_id, env_id);
ALTER TABLE hot.call ADD CONSTRAINT call_run_environment_fk
    FOREIGN KEY (run_id, env_id) REFERENCES hot.run(run_id, env_id) ON DELETE CASCADE;
-- The composite key also enforces run existence. Keep one parent check and
-- one cascade path rather than retaining the redundant original foreign key.
ALTER TABLE hot.call DROP CONSTRAINT call_run_id_fkey;
CREATE INDEX call_env_start_time_idx ON hot.call(env_id, start_time);

-- Scope is set only by trusted backend executors, transaction-locally.
-- This migration deliberately creates no login roles and grants no BYPASSRLS.
-- Use resources/db/postgres/provision_runtime.sql to provision restricted roles.
-- Runtime scope is not a defense against arbitrary SQL with backend credentials.
SET row_security = on;

-- Policies call these helpers once per row. Each top-level helper is a single
-- SQL expression without subqueries, so PostgreSQL inlines it into the policy;
-- table lookups live in separate *_check functions that run only when the
-- inlined mode test selects them. Ownership of the scoped environment is
-- verified once per scope by hot.apply_scope instead of once per row.
CREATE FUNCTION hot.scope_mode() RETURNS text LANGUAGE sql STABLE PARALLEL SAFE AS $$
    SELECT COALESCE(current_setting('hot.scope_mode', true), '')
$$;
CREATE FUNCTION hot.scope_org_id() RETURNS uuid LANGUAGE sql STABLE PARALLEL SAFE AS $$
    SELECT NULLIF(current_setting('hot.scope_org', true), '')::uuid
$$;
CREATE FUNCTION hot.scope_bootstrap(operation text) RETURNS boolean LANGUAGE sql STABLE PARALLEL SAFE AS $$
    SELECT hot.scope_mode() = 'bootstrap'
       AND current_setting('hot.scope_operation', true) = operation
$$;
CREATE FUNCTION hot.scope_maintenance(operation text) RETURNS boolean LANGUAGE sql STABLE PARALLEL SAFE AS $$
    SELECT hot.scope_mode() = 'maintenance'
       AND current_setting('hot.scope_operation', true) = operation
$$;
CREATE FUNCTION hot.scope_org_allows(target uuid) RETURNS boolean LANGUAGE sql STABLE PARALLEL SAFE AS $$
    SELECT COALESCE((hot.scope_mode() IN ('environment', 'organization') AND target = hot.scope_org_id())
        OR (hot.scope_mode() = 'read_set' AND
            COALESCE(NULLIF(current_setting('hot.scope_orgs', true), ''), '[]')::jsonb ? target::text), false)
$$;
CREATE FUNCTION hot.scope_org_writes(target uuid) RETURNS boolean LANGUAGE sql STABLE PARALLEL SAFE AS $$
    SELECT hot.scope_mode() = 'organization' AND target = hot.scope_org_id()
$$;
-- This helper does not query env, so the env policy can use it without recursion.
CREATE FUNCTION hot.scope_env_id_allows(target uuid) RETURNS boolean LANGUAGE sql STABLE PARALLEL SAFE AS $$
    SELECT COALESCE((hot.scope_mode() = 'environment' AND target = NULLIF(current_setting('hot.scope_env', true), '')::uuid)
        OR (hot.scope_mode() = 'read_set' AND
            COALESCE(NULLIF(current_setting('hot.scope_envs', true), ''), '[]')::jsonb ? target::text), false)
$$;
-- Only environments that hot.apply_scope verified against the scoped orgs.
-- Settings written without apply_scope verify nothing and grant nothing.
CREATE FUNCTION hot.scope_env_allows(target uuid) RETURNS boolean LANGUAGE sql STABLE PARALLEL SAFE AS $$
    SELECT COALESCE(
        (hot.scope_mode() = 'environment'
            AND target = NULLIF(current_setting('hot.scope_env_verified', true), '')::uuid)
        OR (hot.scope_mode() = 'read_set'
            AND target = ANY (NULLIF(current_setting('hot.scope_envs_verified', true), '')::uuid[])),
        false)
$$;
CREATE FUNCTION hot.scope_env_writes(target uuid) RETURNS boolean LANGUAGE sql STABLE PARALLEL SAFE AS $$
    SELECT hot.scope_mode() = 'environment' AND hot.scope_env_allows(target)
$$;
CREATE FUNCTION hot.scope_billing_org_check(target uuid) RETURNS boolean LANGUAGE sql STABLE PARALLEL SAFE AS $$
    SELECT hot.scope_org_id() IS NOT NULL AND EXISTS
        (SELECT 1 FROM hot.org o WHERE o.org_id = target
         AND (o.org_id = hot.scope_org_id() OR o.parent_org_id = hot.scope_org_id()))
$$;
CREATE FUNCTION hot.scope_billing_org(target uuid) RETURNS boolean LANGUAGE sql STABLE PARALLEL SAFE AS $$
    SELECT hot.scope_maintenance('billing') AND hot.scope_billing_org_check(target)
$$;
CREATE FUNCTION hot.scope_billing_env_check(target uuid) RETURNS boolean LANGUAGE sql STABLE PARALLEL SAFE AS $$
    SELECT EXISTS (SELECT 1 FROM hot.env e WHERE e.env_id = target AND hot.scope_billing_org(e.org_id))
$$;
-- hot.apply_scope lists a billing account's environments when there are few
-- enough to compare per row; larger accounts check ownership per row.
CREATE FUNCTION hot.scope_billing_env(target uuid) RETURNS boolean LANGUAGE sql STABLE PARALLEL SAFE AS $$
    SELECT hot.scope_maintenance('billing') AND CASE
        WHEN COALESCE(current_setting('hot.scope_billing_envs', true), '') <> ''
            THEN target = ANY (current_setting('hot.scope_billing_envs', true)::uuid[])
        ELSE hot.scope_billing_env_check(target)
    END
$$;
CREATE FUNCTION hot.scope_provisions_org_check(target uuid) RETURNS boolean LANGUAGE sql STABLE PARALLEL SAFE AS $$
    SELECT hot.scope_org_id() IS NOT NULL AND EXISTS
        (SELECT 1 FROM hot.org o WHERE o.org_id = target
         AND (o.org_id = hot.scope_org_id() OR o.parent_org_id = hot.scope_org_id()))
$$;
CREATE FUNCTION hot.scope_provisions_org(target uuid) RETURNS boolean LANGUAGE sql STABLE PARALLEL SAFE AS $$
    SELECT hot.scope_maintenance('organization_provisioning') AND hot.scope_provisions_org_check(target)
$$;
CREATE FUNCTION hot.scope_member_org_check(target uuid) RETURNS boolean LANGUAGE sql STABLE PARALLEL SAFE AS $$
    SELECT EXISTS (SELECT 1 FROM hot.org o WHERE o.org_id = target AND o.parent_org_id = hot.scope_org_id())
$$;
CREATE FUNCTION hot.scope_member_org(target uuid) RETURNS boolean LANGUAGE sql STABLE PARALLEL SAFE AS $$
    SELECT hot.scope_org_writes(target) OR (hot.scope_mode() = 'organization' AND hot.scope_member_org_check(target))
$$;

-- The only supported way to establish scope: trusted backends call this once
-- per transaction. Settings are transaction-local, so connection poolers and
-- proxies can multiplex sessions between transactions. Environment ownership
-- is verified here, under the env policy, rather than per protected row. A
-- read set is never a write grant, so it always makes the transaction read-only.
CREATE FUNCTION hot.apply_scope(
    mode text, org text, env text, orgs text, envs text, operation text, scope_user text
) RETURNS void LANGUAGE plpgsql AS $$
BEGIN
    PERFORM set_config('hot.scope_mode', mode, true),
            set_config('hot.scope_org', org, true),
            set_config('hot.scope_env', env, true),
            set_config('hot.scope_orgs', orgs, true),
            set_config('hot.scope_envs', envs, true),
            set_config('hot.scope_operation', operation, true),
            set_config('hot.scope_user', scope_user, true),
            set_config('hot.scope_env_verified', '', true),
            set_config('hot.scope_envs_verified', '', true),
            set_config('hot.scope_billing_envs', '', true);
    -- Key the lookup on the requested ids so verification is an index probe;
    -- the helper predicates keep the same authority rules.
    IF mode = 'environment' THEN
        PERFORM set_config('hot.scope_env_verified', COALESCE(
            (SELECT e.env_id::text FROM hot.env e
             WHERE e.env_id = NULLIF(env, '')::uuid AND e.org_id = NULLIF(org, '')::uuid
               AND hot.scope_env_id_allows(e.env_id) AND hot.scope_org_allows(e.org_id)), ''), true);
    ELSIF mode = 'read_set' THEN
        PERFORM set_config('hot.scope_envs_verified', COALESCE(
            (SELECT '{' || string_agg(e.env_id::text, ',') || '}' FROM hot.env e
             WHERE e.env_id = ANY (ARRAY(SELECT jsonb_array_elements_text(
                       COALESCE(NULLIF(envs, ''), '[]')::jsonb)::uuid))
               AND hot.scope_env_id_allows(e.env_id) AND hot.scope_org_allows(e.org_id)), ''), true);
        PERFORM set_config('transaction_read_only', 'on', true);
    ELSIF mode = 'maintenance' AND operation = 'billing' AND org <> '' THEN
        PERFORM set_config('hot.scope_billing_envs', COALESCE(
            (SELECT CASE WHEN count(*) <= 256
                         THEN '{' || COALESCE(string_agg(e.env_id::text, ','), '') || '}' END
             FROM hot.env e JOIN hot.org o ON o.org_id = e.org_id
             WHERE o.org_id = org::uuid OR o.parent_org_id = org::uuid), ''), true);
    END IF;
END
$$;

-- org: separate SELECT and mutation grants; a read set is never a write grant.
ALTER TABLE hot."org" ENABLE ROW LEVEL SECURITY;
ALTER TABLE hot."org" FORCE ROW LEVEL SECURITY;
CREATE POLICY hot_scope_read ON hot."org" FOR SELECT USING ((hot.scope_org_allows(org_id)) OR (hot.scope_mode() = 'organization' AND parent_org_id = hot.scope_org_id()) OR (hot.scope_bootstrap('authentication') OR hot.scope_bootstrap('routing') OR hot.scope_bootstrap('membership') OR hot.scope_bootstrap('onboarding')) OR (hot.scope_maintenance('queue') OR hot.scope_maintenance('scheduler') OR hot.scope_maintenance('retention') OR hot.scope_maintenance('alerts')) OR ((hot.scope_maintenance('billing') OR hot.scope_maintenance('organization_provisioning')) AND (org_id = hot.scope_org_id() OR parent_org_id = hot.scope_org_id())));
CREATE POLICY hot_scope_insert ON hot."org" FOR INSERT WITH CHECK ((hot.scope_org_writes(org_id)) OR (hot.scope_bootstrap('onboarding')) OR (hot.scope_maintenance('organization_provisioning') AND (org_id = hot.scope_org_id() OR parent_org_id = hot.scope_org_id())) OR (hot.scope_maintenance('billing') AND org_id = hot.scope_org_id()));
CREATE POLICY hot_scope_update ON hot."org" FOR UPDATE USING ((hot.scope_org_writes(org_id)) OR (hot.scope_bootstrap('onboarding')) OR (hot.scope_maintenance('organization_provisioning') AND (org_id = hot.scope_org_id() OR parent_org_id = hot.scope_org_id())) OR (hot.scope_maintenance('billing') AND org_id = hot.scope_org_id())) WITH CHECK ((hot.scope_org_writes(org_id)) OR (hot.scope_bootstrap('onboarding')) OR (hot.scope_maintenance('organization_provisioning') AND (org_id = hot.scope_org_id() OR parent_org_id = hot.scope_org_id())) OR (hot.scope_maintenance('billing') AND org_id = hot.scope_org_id()));
CREATE POLICY hot_scope_delete ON hot."org" FOR DELETE USING ((hot.scope_org_writes(org_id)) OR (hot.scope_bootstrap('onboarding')) OR (hot.scope_maintenance('organization_provisioning') AND (org_id = hot.scope_org_id() OR parent_org_id = hot.scope_org_id())) OR (hot.scope_maintenance('billing') AND org_id = hot.scope_org_id()));

-- env: separate SELECT and mutation grants; a read set is never a write grant.
ALTER TABLE hot."env" ENABLE ROW LEVEL SECURITY;
ALTER TABLE hot."env" FORCE ROW LEVEL SECURITY;
CREATE POLICY hot_scope_read ON hot."env" FOR SELECT USING (hot.scope_maintenance('blob_gc') OR ((hot.scope_env_id_allows(env_id) AND hot.scope_org_allows(org_id))) OR (hot.scope_mode() = 'organization' AND hot.scope_org_allows(org_id)) OR (hot.scope_bootstrap('authentication') OR hot.scope_bootstrap('routing') OR hot.scope_bootstrap('membership') OR hot.scope_bootstrap('onboarding')) OR (hot.scope_maintenance('queue') OR hot.scope_maintenance('scheduler') OR hot.scope_maintenance('retention') OR hot.scope_maintenance('alerts')) OR (hot.scope_billing_org(org_id)) OR (hot.scope_provisions_org(org_id)));
CREATE POLICY hot_scope_insert ON hot."env" FOR INSERT WITH CHECK ((hot.scope_org_writes(org_id)) OR (hot.scope_env_writes(env_id)) OR (hot.scope_bootstrap('onboarding')) OR (hot.scope_provisions_org(org_id)));
CREATE POLICY hot_scope_update ON hot."env" FOR UPDATE USING ((hot.scope_org_writes(org_id)) OR (hot.scope_env_writes(env_id)) OR (hot.scope_bootstrap('onboarding')) OR (hot.scope_provisions_org(org_id))) WITH CHECK ((hot.scope_org_writes(org_id)) OR (hot.scope_env_writes(env_id)) OR (hot.scope_bootstrap('onboarding')) OR (hot.scope_provisions_org(org_id)));
CREATE POLICY hot_scope_delete ON hot."env" FOR DELETE USING ((hot.scope_org_writes(org_id)) OR (hot.scope_env_writes(env_id)) OR (hot.scope_bootstrap('onboarding')) OR (hot.scope_provisions_org(org_id)));

-- org_note: separate SELECT and mutation grants; a read set is never a write grant.
ALTER TABLE hot."org_note" ENABLE ROW LEVEL SECURITY;
ALTER TABLE hot."org_note" FORCE ROW LEVEL SECURITY;
CREATE POLICY hot_scope_read ON hot."org_note" FOR SELECT USING ((hot.scope_org_allows(org_id)) OR (hot.scope_bootstrap('membership')) OR (hot.scope_maintenance('alerts')) OR (hot.scope_provisions_org(org_id)));
CREATE POLICY hot_scope_insert ON hot."org_note" FOR INSERT WITH CHECK ((hot.scope_org_writes(org_id)) OR (hot.scope_bootstrap('onboarding')) OR (hot.scope_provisions_org(org_id)));
CREATE POLICY hot_scope_update ON hot."org_note" FOR UPDATE USING ((hot.scope_org_writes(org_id)) OR (hot.scope_bootstrap('onboarding')) OR (hot.scope_provisions_org(org_id))) WITH CHECK ((hot.scope_org_writes(org_id)) OR (hot.scope_bootstrap('onboarding')) OR (hot.scope_provisions_org(org_id)));
CREATE POLICY hot_scope_delete ON hot."org_note" FOR DELETE USING ((hot.scope_org_writes(org_id)) OR (hot.scope_bootstrap('onboarding')) OR (hot.scope_provisions_org(org_id)));

-- team: separate SELECT and mutation grants; a read set is never a write grant.
ALTER TABLE hot."team" ENABLE ROW LEVEL SECURITY;
ALTER TABLE hot."team" FORCE ROW LEVEL SECURITY;
CREATE POLICY hot_scope_read ON hot."team" FOR SELECT USING ((hot.scope_org_allows(org_id)) OR (hot.scope_bootstrap('membership')) OR (hot.scope_maintenance('alerts')) OR (hot.scope_provisions_org(org_id)));
CREATE POLICY hot_scope_insert ON hot."team" FOR INSERT WITH CHECK ((hot.scope_org_writes(org_id)) OR (hot.scope_bootstrap('onboarding')) OR (hot.scope_provisions_org(org_id)));
CREATE POLICY hot_scope_update ON hot."team" FOR UPDATE USING ((hot.scope_org_writes(org_id)) OR (hot.scope_bootstrap('onboarding')) OR (hot.scope_provisions_org(org_id))) WITH CHECK ((hot.scope_org_writes(org_id)) OR (hot.scope_bootstrap('onboarding')) OR (hot.scope_provisions_org(org_id)));
CREATE POLICY hot_scope_delete ON hot."team" FOR DELETE USING ((hot.scope_org_writes(org_id)) OR (hot.scope_bootstrap('onboarding')) OR (hot.scope_provisions_org(org_id)));

-- invite: separate SELECT and mutation grants; a read set is never a write grant.
ALTER TABLE hot."invite" ENABLE ROW LEVEL SECURITY;
ALTER TABLE hot."invite" FORCE ROW LEVEL SECURITY;
CREATE POLICY hot_scope_read ON hot."invite" FOR SELECT USING ((hot.scope_org_allows(org_id)) OR (hot.scope_bootstrap('membership')) OR (hot.scope_maintenance('alerts')) OR (hot.scope_provisions_org(org_id)));
CREATE POLICY hot_scope_insert ON hot."invite" FOR INSERT WITH CHECK ((hot.scope_org_writes(org_id)) OR (hot.scope_bootstrap('onboarding')) OR (hot.scope_provisions_org(org_id)));
CREATE POLICY hot_scope_update ON hot."invite" FOR UPDATE USING ((hot.scope_org_writes(org_id)) OR (hot.scope_bootstrap('onboarding')) OR (hot.scope_provisions_org(org_id))) WITH CHECK ((hot.scope_org_writes(org_id)) OR (hot.scope_bootstrap('onboarding')) OR (hot.scope_provisions_org(org_id)));
CREATE POLICY hot_scope_delete ON hot."invite" FOR DELETE USING ((hot.scope_org_writes(org_id)) OR (hot.scope_bootstrap('onboarding')) OR (hot.scope_provisions_org(org_id)));

-- org_user: separate SELECT and mutation grants; a read set is never a write grant.
ALTER TABLE hot."org_user" ENABLE ROW LEVEL SECURITY;
ALTER TABLE hot."org_user" FORCE ROW LEVEL SECURITY;
CREATE POLICY hot_scope_read ON hot."org_user" FOR SELECT USING ((hot.scope_org_allows(org_id)) OR (hot.scope_bootstrap('authentication') OR hot.scope_bootstrap('membership') OR hot.scope_bootstrap('onboarding')) OR ((hot.scope_member_org(org_id) OR hot.scope_provisions_org(org_id))) OR (hot.scope_billing_org(org_id)));
CREATE POLICY hot_scope_insert ON hot."org_user" FOR INSERT WITH CHECK (((hot.scope_member_org(org_id) OR hot.scope_provisions_org(org_id))) OR (hot.scope_bootstrap('onboarding')));
CREATE POLICY hot_scope_update ON hot."org_user" FOR UPDATE USING (((hot.scope_member_org(org_id) OR hot.scope_provisions_org(org_id))) OR (hot.scope_bootstrap('onboarding'))) WITH CHECK (((hot.scope_member_org(org_id) OR hot.scope_provisions_org(org_id))) OR (hot.scope_bootstrap('onboarding')));
CREATE POLICY hot_scope_delete ON hot."org_user" FOR DELETE USING (((hot.scope_member_org(org_id) OR hot.scope_provisions_org(org_id))) OR (hot.scope_bootstrap('onboarding')));

-- org_usage: separate SELECT and mutation grants; a read set is never a write grant.
ALTER TABLE hot."org_usage" ENABLE ROW LEVEL SECURITY;
ALTER TABLE hot."org_usage" FORCE ROW LEVEL SECURITY;
CREATE POLICY hot_scope_read ON hot."org_usage" FOR SELECT USING ((hot.scope_org_allows(org_id)) OR (hot.scope_billing_org(org_id)));
CREATE POLICY hot_scope_insert ON hot."org_usage" FOR INSERT WITH CHECK ((hot.scope_org_writes(org_id)) OR (hot.scope_billing_org(org_id)));
CREATE POLICY hot_scope_update ON hot."org_usage" FOR UPDATE USING ((hot.scope_org_writes(org_id)) OR (hot.scope_billing_org(org_id))) WITH CHECK ((hot.scope_org_writes(org_id)) OR (hot.scope_billing_org(org_id)));
CREATE POLICY hot_scope_delete ON hot."org_usage" FOR DELETE USING ((hot.scope_org_writes(org_id)) OR (hot.scope_billing_org(org_id)));

-- org_plan: separate SELECT and mutation grants; a read set is never a write grant.
ALTER TABLE hot."org_plan" ENABLE ROW LEVEL SECURITY;
ALTER TABLE hot."org_plan" FORCE ROW LEVEL SECURITY;
CREATE POLICY hot_scope_read ON hot."org_plan" FOR SELECT USING ((hot.scope_maintenance('organization_provisioning') AND org_id = hot.scope_org_id()) OR hot.scope_maintenance('retention') OR (hot.scope_org_allows(org_id)) OR (hot.scope_billing_org(org_id)) OR hot.scope_bootstrap('billing_webhook'));
CREATE POLICY hot_scope_insert ON hot."org_plan" FOR INSERT WITH CHECK ((hot.scope_org_writes(org_id)) OR (hot.scope_billing_org(org_id)) OR (hot.scope_bootstrap('onboarding')));
CREATE POLICY hot_scope_update ON hot."org_plan" FOR UPDATE USING ((hot.scope_org_writes(org_id)) OR (hot.scope_billing_org(org_id)) OR (hot.scope_bootstrap('onboarding'))) WITH CHECK ((hot.scope_org_writes(org_id)) OR (hot.scope_billing_org(org_id)) OR (hot.scope_bootstrap('onboarding')));
CREATE POLICY hot_scope_delete ON hot."org_plan" FOR DELETE USING ((hot.scope_org_writes(org_id)) OR (hot.scope_billing_org(org_id)) OR (hot.scope_bootstrap('onboarding')));

-- org_child_provisioning: separate SELECT and mutation grants; a read set is never a write grant.
ALTER TABLE hot."org_child_provisioning" ENABLE ROW LEVEL SECURITY;
ALTER TABLE hot."org_child_provisioning" FORCE ROW LEVEL SECURITY;
CREATE POLICY hot_scope_read ON hot."org_child_provisioning" FOR SELECT USING ((hot.scope_org_allows(root_org_id)) OR (hot.scope_maintenance('organization_provisioning') AND root_org_id = hot.scope_org_id()));
CREATE POLICY hot_scope_insert ON hot."org_child_provisioning" FOR INSERT WITH CHECK (hot.scope_maintenance('organization_provisioning') AND root_org_id = hot.scope_org_id());
CREATE POLICY hot_scope_update ON hot."org_child_provisioning" FOR UPDATE USING (hot.scope_maintenance('organization_provisioning') AND root_org_id = hot.scope_org_id()) WITH CHECK (hot.scope_maintenance('organization_provisioning') AND root_org_id = hot.scope_org_id());
CREATE POLICY hot_scope_delete ON hot."org_child_provisioning" FOR DELETE USING (hot.scope_maintenance('organization_provisioning') AND root_org_id = hot.scope_org_id());

-- api_key: separate SELECT and mutation grants; a read set is never a write grant.
ALTER TABLE hot."api_key" ENABLE ROW LEVEL SECURITY;
ALTER TABLE hot."api_key" FORCE ROW LEVEL SECURITY;
CREATE POLICY hot_scope_read ON hot."api_key" FOR SELECT USING (hot.scope_bootstrap('onboarding') OR (hot.scope_env_allows(env_id)) OR ((hot.scope_bootstrap('authentication')) OR (hot.scope_provisions_org((SELECT e.org_id FROM hot.env e WHERE e.env_id = api_key.env_id)))));
CREATE POLICY hot_scope_insert ON hot."api_key" FOR INSERT WITH CHECK ((hot.scope_env_writes(env_id)) OR ((hot.scope_bootstrap('onboarding'))));
CREATE POLICY hot_scope_update ON hot."api_key" FOR UPDATE USING ((hot.scope_env_writes(env_id)) OR ((hot.scope_bootstrap('onboarding')))) WITH CHECK ((hot.scope_env_writes(env_id)) OR ((hot.scope_bootstrap('onboarding'))));
CREATE POLICY hot_scope_delete ON hot."api_key" FOR DELETE USING ((hot.scope_env_writes(env_id)) OR ((hot.scope_bootstrap('onboarding'))));

-- service_key: separate SELECT and mutation grants; a read set is never a write grant.
ALTER TABLE hot."service_key" ENABLE ROW LEVEL SECURITY;
ALTER TABLE hot."service_key" FORCE ROW LEVEL SECURITY;
CREATE POLICY hot_scope_read ON hot."service_key" FOR SELECT USING ((hot.scope_env_allows(env_id)) OR ((hot.scope_bootstrap('authentication')) OR (hot.scope_provisions_org((SELECT e.org_id FROM hot.env e WHERE e.env_id = service_key.env_id)))));
CREATE POLICY hot_scope_insert ON hot."service_key" FOR INSERT WITH CHECK ((hot.scope_env_writes(env_id)) OR ((hot.scope_bootstrap('authentication'))));
CREATE POLICY hot_scope_update ON hot."service_key" FOR UPDATE USING ((hot.scope_env_writes(env_id)) OR ((hot.scope_bootstrap('authentication')))) WITH CHECK ((hot.scope_env_writes(env_id)) OR ((hot.scope_bootstrap('authentication'))));
CREATE POLICY hot_scope_delete ON hot."service_key" FOR DELETE USING ((hot.scope_env_writes(env_id)) OR ((hot.scope_bootstrap('authentication'))));

-- session: separate SELECT and mutation grants; a read set is never a write grant.
ALTER TABLE hot."session" ENABLE ROW LEVEL SECURITY;
ALTER TABLE hot."session" FORCE ROW LEVEL SECURITY;
CREATE POLICY hot_scope_read ON hot."session" FOR SELECT USING (hot.scope_maintenance('retention') OR (hot.scope_env_allows(env_id)) OR ((hot.scope_bootstrap('authentication')) OR (hot.scope_provisions_org((SELECT e.org_id FROM hot.env e WHERE e.env_id = session.env_id)))));
CREATE POLICY hot_scope_insert ON hot."session" FOR INSERT WITH CHECK ((hot.scope_env_writes(env_id)) OR ((hot.scope_bootstrap('authentication')) OR (hot.scope_maintenance('retention'))));
CREATE POLICY hot_scope_update ON hot."session" FOR UPDATE USING ((hot.scope_env_writes(env_id)) OR ((hot.scope_bootstrap('authentication')) OR (hot.scope_maintenance('retention')))) WITH CHECK ((hot.scope_env_writes(env_id)) OR ((hot.scope_bootstrap('authentication')) OR (hot.scope_maintenance('retention'))));
CREATE POLICY hot_scope_delete ON hot."session" FOR DELETE USING ((hot.scope_env_writes(env_id)) OR ((hot.scope_bootstrap('authentication')) OR (hot.scope_maintenance('retention'))));

-- project: separate SELECT and mutation grants; a read set is never a write grant.
ALTER TABLE hot."project" ENABLE ROW LEVEL SECURITY;
ALTER TABLE hot."project" FORCE ROW LEVEL SECURITY;
CREATE POLICY hot_scope_read ON hot."project" FOR SELECT USING ((hot.scope_env_allows(env_id)) OR ((hot.scope_bootstrap('routing')) OR (hot.scope_maintenance('queue') OR hot.scope_maintenance('scheduler') OR hot.scope_maintenance('retention') OR hot.scope_maintenance('alerts')) OR (hot.scope_billing_env(env_id)) OR (hot.scope_mode() = 'organization' AND EXISTS (SELECT 1 FROM hot.env e WHERE e.env_id = project.env_id AND hot.scope_org_allows(e.org_id)))));
CREATE POLICY hot_scope_insert ON hot."project" FOR INSERT WITH CHECK ((hot.scope_env_writes(env_id)) OR ((hot.scope_bootstrap('onboarding')) OR (hot.scope_maintenance('queue'))));
CREATE POLICY hot_scope_update ON hot."project" FOR UPDATE USING ((hot.scope_env_writes(env_id)) OR ((hot.scope_bootstrap('onboarding')) OR (hot.scope_maintenance('queue')))) WITH CHECK ((hot.scope_env_writes(env_id)) OR ((hot.scope_bootstrap('onboarding')) OR (hot.scope_maintenance('queue'))));
CREATE POLICY hot_scope_delete ON hot."project" FOR DELETE USING ((hot.scope_env_writes(env_id)) OR ((hot.scope_bootstrap('onboarding')) OR (hot.scope_maintenance('queue'))));

-- domain: separate SELECT and mutation grants; a read set is never a write grant.
ALTER TABLE hot."domain" ENABLE ROW LEVEL SECURITY;
ALTER TABLE hot."domain" FORCE ROW LEVEL SECURITY;
CREATE POLICY hot_scope_read ON hot."domain" FOR SELECT USING ((hot.scope_env_allows(env_id)) OR ((hot.scope_bootstrap('routing')) OR (hot.scope_maintenance('queue')) OR (hot.scope_billing_env(env_id))));
CREATE POLICY hot_scope_insert ON hot."domain" FOR INSERT WITH CHECK ((hot.scope_env_writes(env_id)) OR (hot.scope_maintenance('queue')));
CREATE POLICY hot_scope_update ON hot."domain" FOR UPDATE USING ((hot.scope_env_writes(env_id)) OR (hot.scope_maintenance('queue'))) WITH CHECK ((hot.scope_env_writes(env_id)) OR (hot.scope_maintenance('queue')));
CREATE POLICY hot_scope_delete ON hot."domain" FOR DELETE USING ((hot.scope_env_writes(env_id)) OR (hot.scope_maintenance('queue')));

-- run: separate SELECT and mutation grants; a read set is never a write grant.
ALTER TABLE hot."run" ENABLE ROW LEVEL SECURITY;
ALTER TABLE hot."run" FORCE ROW LEVEL SECURITY;
CREATE POLICY hot_scope_read ON hot."run" FOR SELECT USING ((hot.scope_env_allows(env_id)) OR ((hot.scope_maintenance('queue') OR hot.scope_maintenance('scheduler') OR hot.scope_maintenance('retention') OR hot.scope_maintenance('alerts')) OR (hot.scope_billing_env(env_id))));
CREATE POLICY hot_scope_insert ON hot."run" FOR INSERT WITH CHECK ((hot.scope_env_writes(env_id)) OR (hot.scope_maintenance('queue') OR hot.scope_maintenance('scheduler') OR hot.scope_maintenance('retention') OR hot.scope_maintenance('alerts')));
CREATE POLICY hot_scope_update ON hot."run" FOR UPDATE USING ((hot.scope_env_writes(env_id)) OR (hot.scope_maintenance('queue') OR hot.scope_maintenance('scheduler') OR hot.scope_maintenance('retention') OR hot.scope_maintenance('alerts'))) WITH CHECK ((hot.scope_env_writes(env_id)) OR (hot.scope_maintenance('queue') OR hot.scope_maintenance('scheduler') OR hot.scope_maintenance('retention') OR hot.scope_maintenance('alerts')));
CREATE POLICY hot_scope_delete ON hot."run" FOR DELETE USING ((hot.scope_env_writes(env_id)) OR (hot.scope_maintenance('queue') OR hot.scope_maintenance('scheduler') OR hot.scope_maintenance('retention') OR hot.scope_maintenance('alerts')));

-- task: separate SELECT and mutation grants; a read set is never a write grant.
ALTER TABLE hot."task" ENABLE ROW LEVEL SECURITY;
ALTER TABLE hot."task" FORCE ROW LEVEL SECURITY;
CREATE POLICY hot_scope_read ON hot."task" FOR SELECT USING ((hot.scope_env_allows(env_id)) OR ((hot.scope_maintenance('queue') OR hot.scope_maintenance('scheduler') OR hot.scope_maintenance('retention') OR hot.scope_maintenance('alerts')) OR (hot.scope_billing_env(env_id))));
CREATE POLICY hot_scope_insert ON hot."task" FOR INSERT WITH CHECK ((hot.scope_env_writes(env_id)) OR (hot.scope_maintenance('queue') OR hot.scope_maintenance('scheduler') OR hot.scope_maintenance('retention') OR hot.scope_maintenance('alerts')));
CREATE POLICY hot_scope_update ON hot."task" FOR UPDATE USING ((hot.scope_env_writes(env_id)) OR (hot.scope_maintenance('queue') OR hot.scope_maintenance('scheduler') OR hot.scope_maintenance('retention') OR hot.scope_maintenance('alerts'))) WITH CHECK ((hot.scope_env_writes(env_id)) OR (hot.scope_maintenance('queue') OR hot.scope_maintenance('scheduler') OR hot.scope_maintenance('retention') OR hot.scope_maintenance('alerts')));
CREATE POLICY hot_scope_delete ON hot."task" FOR DELETE USING ((hot.scope_env_writes(env_id)) OR (hot.scope_maintenance('queue') OR hot.scope_maintenance('scheduler') OR hot.scope_maintenance('retention') OR hot.scope_maintenance('alerts')));

-- event: separate SELECT and mutation grants; a read set is never a write grant.
ALTER TABLE hot."event" ENABLE ROW LEVEL SECURITY;
ALTER TABLE hot."event" FORCE ROW LEVEL SECURITY;
CREATE POLICY hot_scope_read ON hot."event" FOR SELECT USING ((hot.scope_env_allows(env_id)) OR ((hot.scope_maintenance('queue') OR hot.scope_maintenance('scheduler') OR hot.scope_maintenance('retention') OR hot.scope_maintenance('alerts')) OR (hot.scope_billing_env(env_id))));
CREATE POLICY hot_scope_insert ON hot."event" FOR INSERT WITH CHECK ((hot.scope_env_writes(env_id)) OR (hot.scope_maintenance('queue') OR hot.scope_maintenance('scheduler') OR hot.scope_maintenance('retention') OR hot.scope_maintenance('alerts')));
CREATE POLICY hot_scope_update ON hot."event" FOR UPDATE USING ((hot.scope_env_writes(env_id)) OR (hot.scope_maintenance('queue') OR hot.scope_maintenance('scheduler') OR hot.scope_maintenance('retention') OR hot.scope_maintenance('alerts'))) WITH CHECK ((hot.scope_env_writes(env_id)) OR (hot.scope_maintenance('queue') OR hot.scope_maintenance('scheduler') OR hot.scope_maintenance('retention') OR hot.scope_maintenance('alerts')));
CREATE POLICY hot_scope_delete ON hot."event" FOR DELETE USING ((hot.scope_env_writes(env_id)) OR (hot.scope_maintenance('queue') OR hot.scope_maintenance('scheduler') OR hot.scope_maintenance('retention') OR hot.scope_maintenance('alerts')));

-- stream: separate SELECT and mutation grants; a read set is never a write grant.
ALTER TABLE hot."stream" ENABLE ROW LEVEL SECURITY;
ALTER TABLE hot."stream" FORCE ROW LEVEL SECURITY;
CREATE POLICY hot_scope_read ON hot."stream" FOR SELECT USING ((hot.scope_env_allows(env_id)) OR ((hot.scope_maintenance('queue') OR hot.scope_maintenance('scheduler') OR hot.scope_maintenance('retention') OR hot.scope_maintenance('alerts')) OR (hot.scope_billing_env(env_id))));
CREATE POLICY hot_scope_insert ON hot."stream" FOR INSERT WITH CHECK ((hot.scope_env_writes(env_id)) OR (hot.scope_maintenance('queue') OR hot.scope_maintenance('scheduler') OR hot.scope_maintenance('retention') OR hot.scope_maintenance('alerts')));
CREATE POLICY hot_scope_update ON hot."stream" FOR UPDATE USING ((hot.scope_env_writes(env_id)) OR (hot.scope_maintenance('queue') OR hot.scope_maintenance('scheduler') OR hot.scope_maintenance('retention') OR hot.scope_maintenance('alerts'))) WITH CHECK ((hot.scope_env_writes(env_id)) OR (hot.scope_maintenance('queue') OR hot.scope_maintenance('scheduler') OR hot.scope_maintenance('retention') OR hot.scope_maintenance('alerts')));
CREATE POLICY hot_scope_delete ON hot."stream" FOR DELETE USING ((hot.scope_env_writes(env_id)) OR (hot.scope_maintenance('queue') OR hot.scope_maintenance('scheduler') OR hot.scope_maintenance('retention') OR hot.scope_maintenance('alerts')));

-- access: separate SELECT and mutation grants; a read set is never a write grant.
ALTER TABLE hot."access" ENABLE ROW LEVEL SECURITY;
ALTER TABLE hot."access" FORCE ROW LEVEL SECURITY;
CREATE POLICY hot_scope_read ON hot."access" FOR SELECT USING ((hot.scope_env_allows(env_id)) OR (hot.scope_maintenance('retention')));
CREATE POLICY hot_scope_insert ON hot."access" FOR INSERT WITH CHECK ((hot.scope_env_writes(env_id)) OR (hot.scope_maintenance('retention')));
CREATE POLICY hot_scope_update ON hot."access" FOR UPDATE USING ((hot.scope_env_writes(env_id)) OR (hot.scope_maintenance('retention'))) WITH CHECK ((hot.scope_env_writes(env_id)) OR (hot.scope_maintenance('retention')));
CREATE POLICY hot_scope_delete ON hot."access" FOR DELETE USING ((hot.scope_env_writes(env_id)) OR (hot.scope_maintenance('retention')));

-- agent: separate SELECT and mutation grants; a read set is never a write grant.
ALTER TABLE hot."agent" ENABLE ROW LEVEL SECURITY;
ALTER TABLE hot."agent" FORCE ROW LEVEL SECURITY;
CREATE POLICY hot_scope_read ON hot."agent" FOR SELECT USING ((hot.scope_env_allows(env_id)) OR ((hot.scope_bootstrap('routing')) OR (hot.scope_maintenance('queue') OR hot.scope_maintenance('scheduler'))));
CREATE POLICY hot_scope_insert ON hot."agent" FOR INSERT WITH CHECK ((hot.scope_env_writes(env_id)) OR (hot.scope_maintenance('queue')));
CREATE POLICY hot_scope_update ON hot."agent" FOR UPDATE USING ((hot.scope_env_writes(env_id)) OR (hot.scope_maintenance('queue'))) WITH CHECK ((hot.scope_env_writes(env_id)) OR (hot.scope_maintenance('queue')));
CREATE POLICY hot_scope_delete ON hot."agent" FOR DELETE USING ((hot.scope_env_writes(env_id)) OR (hot.scope_maintenance('queue')));

-- workflow: separate SELECT and mutation grants; a read set is never a write grant.
ALTER TABLE hot."workflow" ENABLE ROW LEVEL SECURITY;
ALTER TABLE hot."workflow" FORCE ROW LEVEL SECURITY;
CREATE POLICY hot_scope_read ON hot."workflow" FOR SELECT USING ((hot.scope_env_allows(env_id)) OR ((hot.scope_bootstrap('routing')) OR (hot.scope_maintenance('queue') OR hot.scope_maintenance('scheduler'))));
CREATE POLICY hot_scope_insert ON hot."workflow" FOR INSERT WITH CHECK ((hot.scope_env_writes(env_id)) OR (hot.scope_maintenance('queue')));
CREATE POLICY hot_scope_update ON hot."workflow" FOR UPDATE USING ((hot.scope_env_writes(env_id)) OR (hot.scope_maintenance('queue'))) WITH CHECK ((hot.scope_env_writes(env_id)) OR (hot.scope_maintenance('queue')));
CREATE POLICY hot_scope_delete ON hot."workflow" FOR DELETE USING ((hot.scope_env_writes(env_id)) OR (hot.scope_maintenance('queue')));

-- build: separate SELECT and mutation grants; a read set is never a write grant.
ALTER TABLE hot."build" ENABLE ROW LEVEL SECURITY;
ALTER TABLE hot."build" FORCE ROW LEVEL SECURITY;
CREATE POLICY hot_scope_read ON hot."build" FOR SELECT USING (EXISTS (SELECT 1 FROM hot.project p WHERE p.project_id = build.project_id AND (hot.scope_env_allows(p.env_id) OR hot.scope_bootstrap('routing') OR hot.scope_maintenance('queue') OR hot.scope_maintenance('scheduler') OR hot.scope_maintenance('retention') OR hot.scope_maintenance('alerts') OR hot.scope_billing_env(p.env_id))));
CREATE POLICY hot_scope_insert ON hot."build" FOR INSERT WITH CHECK ((EXISTS (SELECT 1 FROM hot.project p WHERE p.project_id = build.project_id)) AND (hot.scope_mode() = 'environment' OR hot.scope_maintenance('queue') OR hot.scope_maintenance('scheduler') OR hot.scope_maintenance('retention') OR hot.scope_maintenance('alerts')));
CREATE POLICY hot_scope_update ON hot."build" FOR UPDATE USING ((EXISTS (SELECT 1 FROM hot.project p WHERE p.project_id = build.project_id)) AND (hot.scope_mode() = 'environment' OR hot.scope_maintenance('queue') OR hot.scope_maintenance('scheduler') OR hot.scope_maintenance('retention') OR hot.scope_maintenance('alerts'))) WITH CHECK ((EXISTS (SELECT 1 FROM hot.project p WHERE p.project_id = build.project_id)) AND (hot.scope_mode() = 'environment' OR hot.scope_maintenance('queue') OR hot.scope_maintenance('scheduler') OR hot.scope_maintenance('retention') OR hot.scope_maintenance('alerts')));
CREATE POLICY hot_scope_delete ON hot."build" FOR DELETE USING ((EXISTS (SELECT 1 FROM hot.project p WHERE p.project_id = build.project_id)) AND (hot.scope_mode() = 'environment' OR hot.scope_maintenance('queue') OR hot.scope_maintenance('scheduler') OR hot.scope_maintenance('retention') OR hot.scope_maintenance('alerts')));

-- call: separate SELECT and mutation grants; a read set is never a write grant.
ALTER TABLE hot."call" ENABLE ROW LEVEL SECURITY;
ALTER TABLE hot."call" FORCE ROW LEVEL SECURITY;
CREATE POLICY hot_scope_read ON hot."call" FOR SELECT USING ((hot.scope_env_allows(env_id) OR (hot.scope_maintenance('queue') OR hot.scope_maintenance('scheduler') OR hot.scope_maintenance('retention') OR hot.scope_maintenance('alerts')) OR hot.scope_billing_env(env_id)));
CREATE POLICY hot_scope_insert ON hot."call" FOR INSERT WITH CHECK ((hot.scope_env_writes(env_id) OR (hot.scope_maintenance('queue') OR hot.scope_maintenance('scheduler') OR hot.scope_maintenance('retention') OR hot.scope_maintenance('alerts'))));
CREATE POLICY hot_scope_update ON hot."call" FOR UPDATE USING ((hot.scope_env_writes(env_id) OR (hot.scope_maintenance('queue') OR hot.scope_maintenance('scheduler') OR hot.scope_maintenance('retention') OR hot.scope_maintenance('alerts')))) WITH CHECK ((hot.scope_env_writes(env_id) OR (hot.scope_maintenance('queue') OR hot.scope_maintenance('scheduler') OR hot.scope_maintenance('retention') OR hot.scope_maintenance('alerts'))));
CREATE POLICY hot_scope_delete ON hot."call" FOR DELETE USING ((hot.scope_env_writes(env_id) OR (hot.scope_maintenance('queue') OR hot.scope_maintenance('scheduler') OR hot.scope_maintenance('retention') OR hot.scope_maintenance('alerts'))));

-- event_handler: separate SELECT and mutation grants; a read set is never a write grant.
ALTER TABLE hot."event_handler" ENABLE ROW LEVEL SECURITY;
ALTER TABLE hot."event_handler" FORCE ROW LEVEL SECURITY;
CREATE POLICY hot_scope_read ON hot."event_handler" FOR SELECT USING (EXISTS (SELECT 1 FROM hot.build p WHERE p.build_id = event_handler.build_id));
CREATE POLICY hot_scope_insert ON hot."event_handler" FOR INSERT WITH CHECK ((EXISTS (SELECT 1 FROM hot.build p WHERE p.build_id = event_handler.build_id)) AND (hot.scope_mode() = 'environment' OR hot.scope_maintenance('queue') OR hot.scope_maintenance('scheduler') OR hot.scope_maintenance('retention') OR hot.scope_maintenance('alerts')));
CREATE POLICY hot_scope_update ON hot."event_handler" FOR UPDATE USING ((EXISTS (SELECT 1 FROM hot.build p WHERE p.build_id = event_handler.build_id)) AND (hot.scope_mode() = 'environment' OR hot.scope_maintenance('queue') OR hot.scope_maintenance('scheduler') OR hot.scope_maintenance('retention') OR hot.scope_maintenance('alerts'))) WITH CHECK ((EXISTS (SELECT 1 FROM hot.build p WHERE p.build_id = event_handler.build_id)) AND (hot.scope_mode() = 'environment' OR hot.scope_maintenance('queue') OR hot.scope_maintenance('scheduler') OR hot.scope_maintenance('retention') OR hot.scope_maintenance('alerts')));
CREATE POLICY hot_scope_delete ON hot."event_handler" FOR DELETE USING ((EXISTS (SELECT 1 FROM hot.build p WHERE p.build_id = event_handler.build_id)) AND (hot.scope_mode() = 'environment' OR hot.scope_maintenance('queue') OR hot.scope_maintenance('scheduler') OR hot.scope_maintenance('retention') OR hot.scope_maintenance('alerts')));

-- mcp_tool: separate SELECT and mutation grants; a read set is never a write grant.
ALTER TABLE hot."mcp_tool" ENABLE ROW LEVEL SECURITY;
ALTER TABLE hot."mcp_tool" FORCE ROW LEVEL SECURITY;
CREATE POLICY hot_scope_read ON hot."mcp_tool" FOR SELECT USING (EXISTS (SELECT 1 FROM hot.build p WHERE p.build_id = mcp_tool.build_id));
CREATE POLICY hot_scope_insert ON hot."mcp_tool" FOR INSERT WITH CHECK ((EXISTS (SELECT 1 FROM hot.build p WHERE p.build_id = mcp_tool.build_id)) AND (hot.scope_mode() = 'environment' OR hot.scope_maintenance('queue') OR hot.scope_maintenance('scheduler') OR hot.scope_maintenance('retention') OR hot.scope_maintenance('alerts')));
CREATE POLICY hot_scope_update ON hot."mcp_tool" FOR UPDATE USING ((EXISTS (SELECT 1 FROM hot.build p WHERE p.build_id = mcp_tool.build_id)) AND (hot.scope_mode() = 'environment' OR hot.scope_maintenance('queue') OR hot.scope_maintenance('scheduler') OR hot.scope_maintenance('retention') OR hot.scope_maintenance('alerts'))) WITH CHECK ((EXISTS (SELECT 1 FROM hot.build p WHERE p.build_id = mcp_tool.build_id)) AND (hot.scope_mode() = 'environment' OR hot.scope_maintenance('queue') OR hot.scope_maintenance('scheduler') OR hot.scope_maintenance('retention') OR hot.scope_maintenance('alerts')));
CREATE POLICY hot_scope_delete ON hot."mcp_tool" FOR DELETE USING ((EXISTS (SELECT 1 FROM hot.build p WHERE p.build_id = mcp_tool.build_id)) AND (hot.scope_mode() = 'environment' OR hot.scope_maintenance('queue') OR hot.scope_maintenance('scheduler') OR hot.scope_maintenance('retention') OR hot.scope_maintenance('alerts')));

-- webhook: separate SELECT and mutation grants; a read set is never a write grant.
ALTER TABLE hot."webhook" ENABLE ROW LEVEL SECURITY;
ALTER TABLE hot."webhook" FORCE ROW LEVEL SECURITY;
CREATE POLICY hot_scope_read ON hot."webhook" FOR SELECT USING (EXISTS (SELECT 1 FROM hot.build p WHERE p.build_id = webhook.build_id));
CREATE POLICY hot_scope_insert ON hot."webhook" FOR INSERT WITH CHECK ((EXISTS (SELECT 1 FROM hot.build p WHERE p.build_id = webhook.build_id)) AND (hot.scope_mode() = 'environment' OR hot.scope_maintenance('queue') OR hot.scope_maintenance('scheduler') OR hot.scope_maintenance('retention') OR hot.scope_maintenance('alerts')));
CREATE POLICY hot_scope_update ON hot."webhook" FOR UPDATE USING ((EXISTS (SELECT 1 FROM hot.build p WHERE p.build_id = webhook.build_id)) AND (hot.scope_mode() = 'environment' OR hot.scope_maintenance('queue') OR hot.scope_maintenance('scheduler') OR hot.scope_maintenance('retention') OR hot.scope_maintenance('alerts'))) WITH CHECK ((EXISTS (SELECT 1 FROM hot.build p WHERE p.build_id = webhook.build_id)) AND (hot.scope_mode() = 'environment' OR hot.scope_maintenance('queue') OR hot.scope_maintenance('scheduler') OR hot.scope_maintenance('retention') OR hot.scope_maintenance('alerts')));
CREATE POLICY hot_scope_delete ON hot."webhook" FOR DELETE USING ((EXISTS (SELECT 1 FROM hot.build p WHERE p.build_id = webhook.build_id)) AND (hot.scope_mode() = 'environment' OR hot.scope_maintenance('queue') OR hot.scope_maintenance('scheduler') OR hot.scope_maintenance('retention') OR hot.scope_maintenance('alerts')));

-- schedule: separate SELECT and mutation grants; a read set is never a write grant.
ALTER TABLE hot."schedule" ENABLE ROW LEVEL SECURITY;
ALTER TABLE hot."schedule" FORCE ROW LEVEL SECURITY;
CREATE POLICY hot_scope_read ON hot."schedule" FOR SELECT USING (EXISTS (SELECT 1 FROM hot.build p WHERE p.build_id = schedule.build_id));
CREATE POLICY hot_scope_insert ON hot."schedule" FOR INSERT WITH CHECK ((EXISTS (SELECT 1 FROM hot.build p WHERE p.build_id = schedule.build_id)) AND (hot.scope_mode() = 'environment' OR hot.scope_maintenance('queue') OR hot.scope_maintenance('scheduler') OR hot.scope_maintenance('retention') OR hot.scope_maintenance('alerts')));
CREATE POLICY hot_scope_update ON hot."schedule" FOR UPDATE USING ((EXISTS (SELECT 1 FROM hot.build p WHERE p.build_id = schedule.build_id)) AND (hot.scope_mode() = 'environment' OR hot.scope_maintenance('queue') OR hot.scope_maintenance('scheduler') OR hot.scope_maintenance('retention') OR hot.scope_maintenance('alerts'))) WITH CHECK ((EXISTS (SELECT 1 FROM hot.build p WHERE p.build_id = schedule.build_id)) AND (hot.scope_mode() = 'environment' OR hot.scope_maintenance('queue') OR hot.scope_maintenance('scheduler') OR hot.scope_maintenance('retention') OR hot.scope_maintenance('alerts')));
CREATE POLICY hot_scope_delete ON hot."schedule" FOR DELETE USING ((EXISTS (SELECT 1 FROM hot.build p WHERE p.build_id = schedule.build_id)) AND (hot.scope_mode() = 'environment' OR hot.scope_maintenance('queue') OR hot.scope_maintenance('scheduler') OR hot.scope_maintenance('retention') OR hot.scope_maintenance('alerts')));

-- schedule_log: separate SELECT and mutation grants; a read set is never a write grant.
ALTER TABLE hot."schedule_log" ENABLE ROW LEVEL SECURITY;
ALTER TABLE hot."schedule_log" FORCE ROW LEVEL SECURITY;
CREATE POLICY hot_scope_read ON hot."schedule_log" FOR SELECT USING (EXISTS (SELECT 1 FROM hot.schedule p WHERE p.schedule_id = schedule_log.schedule_id));
CREATE POLICY hot_scope_insert ON hot."schedule_log" FOR INSERT WITH CHECK ((EXISTS (SELECT 1 FROM hot.schedule p WHERE p.schedule_id = schedule_log.schedule_id)) AND (hot.scope_mode() = 'environment' OR hot.scope_maintenance('queue') OR hot.scope_maintenance('scheduler') OR hot.scope_maintenance('retention') OR hot.scope_maintenance('alerts')));
CREATE POLICY hot_scope_update ON hot."schedule_log" FOR UPDATE USING ((EXISTS (SELECT 1 FROM hot.schedule p WHERE p.schedule_id = schedule_log.schedule_id)) AND (hot.scope_mode() = 'environment' OR hot.scope_maintenance('queue') OR hot.scope_maintenance('scheduler') OR hot.scope_maintenance('retention') OR hot.scope_maintenance('alerts'))) WITH CHECK ((EXISTS (SELECT 1 FROM hot.schedule p WHERE p.schedule_id = schedule_log.schedule_id)) AND (hot.scope_mode() = 'environment' OR hot.scope_maintenance('queue') OR hot.scope_maintenance('scheduler') OR hot.scope_maintenance('retention') OR hot.scope_maintenance('alerts')));
CREATE POLICY hot_scope_delete ON hot."schedule_log" FOR DELETE USING ((EXISTS (SELECT 1 FROM hot.schedule p WHERE p.schedule_id = schedule_log.schedule_id)) AND (hot.scope_mode() = 'environment' OR hot.scope_maintenance('queue') OR hot.scope_maintenance('scheduler') OR hot.scope_maintenance('retention') OR hot.scope_maintenance('alerts')));

-- team_user: separate SELECT and mutation grants; a read set is never a write grant.
ALTER TABLE hot."team_user" ENABLE ROW LEVEL SECURITY;
ALTER TABLE hot."team_user" FORCE ROW LEVEL SECURITY;
CREATE POLICY hot_scope_read ON hot."team_user" FOR SELECT USING (EXISTS (SELECT 1 FROM hot.team p WHERE p.team_id = team_user.team_id));
CREATE POLICY hot_scope_insert ON hot."team_user" FOR INSERT WITH CHECK ((EXISTS (SELECT 1 FROM hot.team p WHERE p.team_id = team_user.team_id)) AND hot.scope_mode() IN ('organization','maintenance'));
CREATE POLICY hot_scope_update ON hot."team_user" FOR UPDATE USING ((EXISTS (SELECT 1 FROM hot.team p WHERE p.team_id = team_user.team_id)) AND hot.scope_mode() IN ('organization','maintenance')) WITH CHECK ((EXISTS (SELECT 1 FROM hot.team p WHERE p.team_id = team_user.team_id)) AND hot.scope_mode() IN ('organization','maintenance'));
CREATE POLICY hot_scope_delete ON hot."team_user" FOR DELETE USING ((EXISTS (SELECT 1 FROM hot.team p WHERE p.team_id = team_user.team_id)) AND hot.scope_mode() IN ('organization','maintenance'));

-- alert_delivery: separate SELECT and mutation grants; a read set is never a write grant.
ALTER TABLE hot."alert_delivery" ENABLE ROW LEVEL SECURITY;
ALTER TABLE hot."alert_delivery" FORCE ROW LEVEL SECURITY;
CREATE POLICY hot_scope_read ON hot."alert_delivery" FOR SELECT USING (EXISTS (SELECT 1 FROM hot.alert p WHERE p.alert_id = alert_delivery.alert_id));
CREATE POLICY hot_scope_insert ON hot."alert_delivery" FOR INSERT WITH CHECK ((EXISTS (SELECT 1 FROM hot.alert p WHERE p.alert_id = alert_delivery.alert_id)) AND (hot.scope_mode() = 'environment' OR hot.scope_maintenance('queue') OR hot.scope_maintenance('scheduler') OR hot.scope_maintenance('retention') OR hot.scope_maintenance('alerts')));
CREATE POLICY hot_scope_update ON hot."alert_delivery" FOR UPDATE USING ((EXISTS (SELECT 1 FROM hot.alert p WHERE p.alert_id = alert_delivery.alert_id)) AND (hot.scope_mode() = 'environment' OR hot.scope_maintenance('queue') OR hot.scope_maintenance('scheduler') OR hot.scope_maintenance('retention') OR hot.scope_maintenance('alerts'))) WITH CHECK ((EXISTS (SELECT 1 FROM hot.alert p WHERE p.alert_id = alert_delivery.alert_id)) AND (hot.scope_mode() = 'environment' OR hot.scope_maintenance('queue') OR hot.scope_maintenance('scheduler') OR hot.scope_maintenance('retention') OR hot.scope_maintenance('alerts')));
CREATE POLICY hot_scope_delete ON hot."alert_delivery" FOR DELETE USING ((EXISTS (SELECT 1 FROM hot.alert p WHERE p.alert_id = alert_delivery.alert_id)) AND (hot.scope_mode() = 'environment' OR hot.scope_maintenance('queue') OR hot.scope_maintenance('scheduler') OR hot.scope_maintenance('retention') OR hot.scope_maintenance('alerts')));

-- alert_subscription_channel: separate SELECT and mutation grants; a read set is never a write grant.
CREATE FUNCTION hot.scope_subscription_writes(target uuid) RETURNS boolean LANGUAGE sql STABLE AS $$
    SELECT EXISTS (SELECT 1 FROM hot.alert_subscription p WHERE p.alert_subscription_id = target
        AND ((p.env_id IS NULL AND hot.scope_org_writes(p.org_id))
          OR (p.env_id IS NOT NULL AND hot.scope_env_writes(p.env_id) AND hot.scope_org_allows(p.org_id))
          OR hot.scope_maintenance('alerts')))
$$;
CREATE FUNCTION hot.scope_subscription_channel_matches(subscription uuid, channel uuid) RETURNS boolean LANGUAGE sql STABLE AS $$
    SELECT EXISTS (SELECT 1 FROM hot.alert_subscription p JOIN hot.alert_channel c ON c.alert_channel_id = channel
        WHERE p.alert_subscription_id = subscription
          AND (c.org_id IS NULL OR c.org_id = p.org_id)
          AND (c.env_id IS NULL OR c.env_id = p.env_id))
$$;
CREATE FUNCTION hot.scope_subscription_destination_matches(subscription uuid, destination uuid) RETURNS boolean LANGUAGE sql STABLE AS $$
    SELECT EXISTS (SELECT 1 FROM hot.alert_subscription p JOIN hot.alert_destination d ON d.alert_destination_id = destination
        WHERE p.alert_subscription_id = subscription AND d.org_id = p.org_id)
$$;
ALTER TABLE hot."alert_subscription_channel" ENABLE ROW LEVEL SECURITY;
ALTER TABLE hot."alert_subscription_channel" FORCE ROW LEVEL SECURITY;
CREATE POLICY hot_scope_read ON hot."alert_subscription_channel" FOR SELECT USING (EXISTS (SELECT 1 FROM hot.alert_subscription p WHERE p.alert_subscription_id = alert_subscription_channel.subscription_id));
CREATE POLICY hot_scope_insert ON hot."alert_subscription_channel" FOR INSERT WITH CHECK (hot.scope_subscription_writes(subscription_id) AND hot.scope_subscription_channel_matches(subscription_id, channel_id));
CREATE POLICY hot_scope_update ON hot."alert_subscription_channel" FOR UPDATE USING (hot.scope_subscription_writes(subscription_id)) WITH CHECK (hot.scope_subscription_writes(subscription_id) AND hot.scope_subscription_channel_matches(subscription_id, channel_id));
CREATE POLICY hot_scope_delete ON hot."alert_subscription_channel" FOR DELETE USING (hot.scope_subscription_writes(subscription_id));

-- alert_subscription_destination: separate SELECT and mutation grants; a read set is never a write grant.
ALTER TABLE hot."alert_subscription_destination" ENABLE ROW LEVEL SECURITY;
ALTER TABLE hot."alert_subscription_destination" FORCE ROW LEVEL SECURITY;
CREATE POLICY hot_scope_read ON hot."alert_subscription_destination" FOR SELECT USING (EXISTS (SELECT 1 FROM hot.alert_subscription p WHERE p.alert_subscription_id = alert_subscription_destination.subscription_id));
CREATE POLICY hot_scope_insert ON hot."alert_subscription_destination" FOR INSERT WITH CHECK (hot.scope_subscription_writes(subscription_id) AND hot.scope_subscription_destination_matches(subscription_id, destination_id));
CREATE POLICY hot_scope_update ON hot."alert_subscription_destination" FOR UPDATE USING (hot.scope_subscription_writes(subscription_id)) WITH CHECK (hot.scope_subscription_writes(subscription_id) AND hot.scope_subscription_destination_matches(subscription_id, destination_id));
CREATE POLICY hot_scope_delete ON hot."alert_subscription_destination" FOR DELETE USING (hot.scope_subscription_writes(subscription_id));

-- file: separate SELECT and mutation grants; a read set is never a write grant.
ALTER TABLE hot."file" ENABLE ROW LEVEL SECURITY;
ALTER TABLE hot."file" FORCE ROW LEVEL SECURITY;
CREATE POLICY hot_scope_read ON hot."file" FOR SELECT USING (((env_id IS NULL AND hot.scope_org_allows(org_id))) OR ((env_id IS NOT NULL AND hot.scope_env_allows(env_id) AND hot.scope_org_allows(org_id))) OR (hot.scope_billing_org(org_id)));
CREATE POLICY hot_scope_insert ON hot."file" FOR INSERT WITH CHECK (((env_id IS NULL AND hot.scope_org_writes(org_id))) OR ((env_id IS NOT NULL AND hot.scope_env_writes(env_id) AND hot.scope_org_allows(org_id))));
CREATE POLICY hot_scope_update ON hot."file" FOR UPDATE USING (((env_id IS NULL AND hot.scope_org_writes(org_id))) OR ((env_id IS NOT NULL AND hot.scope_env_writes(env_id) AND hot.scope_org_allows(org_id)))) WITH CHECK (((env_id IS NULL AND hot.scope_org_writes(org_id))) OR ((env_id IS NOT NULL AND hot.scope_env_writes(env_id) AND hot.scope_org_allows(org_id))));
CREATE POLICY hot_scope_delete ON hot."file" FOR DELETE USING (((env_id IS NULL AND hot.scope_org_writes(org_id))) OR ((env_id IS NOT NULL AND hot.scope_env_writes(env_id) AND hot.scope_org_allows(org_id))));

-- file_upload: separate SELECT and mutation grants; a read set is never a write grant.
ALTER TABLE hot."file_upload" ENABLE ROW LEVEL SECURITY;
ALTER TABLE hot."file_upload" FORCE ROW LEVEL SECURITY;
CREATE POLICY hot_scope_read ON hot."file_upload" FOR SELECT USING (hot.scope_maintenance('retention') OR ((env_id IS NULL AND hot.scope_org_allows(org_id))) OR ((env_id IS NOT NULL AND hot.scope_env_allows(env_id) AND hot.scope_org_allows(org_id))) OR (hot.scope_billing_org(org_id)));
CREATE POLICY hot_scope_insert ON hot."file_upload" FOR INSERT WITH CHECK (((env_id IS NULL AND hot.scope_org_writes(org_id))) OR ((env_id IS NOT NULL AND hot.scope_env_writes(env_id) AND hot.scope_org_allows(org_id))));
CREATE POLICY hot_scope_update ON hot."file_upload" FOR UPDATE USING (hot.scope_maintenance('retention') OR ((env_id IS NULL AND hot.scope_org_writes(org_id))) OR ((env_id IS NOT NULL AND hot.scope_env_writes(env_id) AND hot.scope_org_allows(org_id)))) WITH CHECK (hot.scope_maintenance('retention') OR ((env_id IS NULL AND hot.scope_org_writes(org_id))) OR ((env_id IS NOT NULL AND hot.scope_env_writes(env_id) AND hot.scope_org_allows(org_id))));
CREATE POLICY hot_scope_delete ON hot."file_upload" FOR DELETE USING (((env_id IS NULL AND hot.scope_org_writes(org_id))) OR ((env_id IS NOT NULL AND hot.scope_env_writes(env_id) AND hot.scope_org_allows(org_id))));

-- file_upload_part_claim: a part reservation follows its upload. Reads need
-- the upload to be visible; writes need the upload's own write scope.
ALTER TABLE hot."file_upload_part_claim" ENABLE ROW LEVEL SECURITY;
ALTER TABLE hot."file_upload_part_claim" FORCE ROW LEVEL SECURITY;
CREATE POLICY hot_scope_read ON hot."file_upload_part_claim" FOR SELECT USING (EXISTS (SELECT 1 FROM hot.file_upload u WHERE u.upload_id = file_upload_part_claim.upload_id));
CREATE POLICY hot_scope_insert ON hot."file_upload_part_claim" FOR INSERT WITH CHECK (EXISTS (SELECT 1 FROM hot.file_upload u WHERE u.upload_id = file_upload_part_claim.upload_id AND ((u.env_id IS NULL AND hot.scope_org_writes(u.org_id)) OR (u.env_id IS NOT NULL AND hot.scope_env_writes(u.env_id) AND hot.scope_org_allows(u.org_id)))));
CREATE POLICY hot_scope_update ON hot."file_upload_part_claim" FOR UPDATE USING (EXISTS (SELECT 1 FROM hot.file_upload u WHERE u.upload_id = file_upload_part_claim.upload_id AND ((u.env_id IS NULL AND hot.scope_org_writes(u.org_id)) OR (u.env_id IS NOT NULL AND hot.scope_env_writes(u.env_id) AND hot.scope_org_allows(u.org_id))))) WITH CHECK (EXISTS (SELECT 1 FROM hot.file_upload u WHERE u.upload_id = file_upload_part_claim.upload_id AND ((u.env_id IS NULL AND hot.scope_org_writes(u.org_id)) OR (u.env_id IS NOT NULL AND hot.scope_env_writes(u.env_id) AND hot.scope_org_allows(u.org_id)))));
CREATE POLICY hot_scope_delete ON hot."file_upload_part_claim" FOR DELETE USING (EXISTS (SELECT 1 FROM hot.file_upload u WHERE u.upload_id = file_upload_part_claim.upload_id AND ((u.env_id IS NULL AND hot.scope_org_writes(u.org_id)) OR (u.env_id IS NOT NULL AND hot.scope_env_writes(u.env_id) AND hot.scope_org_allows(u.org_id)))));

-- blob_object: separate SELECT and mutation grants; a read set is never a write grant.
ALTER TABLE hot."blob_object" ENABLE ROW LEVEL SECURITY;
ALTER TABLE hot."blob_object" FORCE ROW LEVEL SECURITY;
CREATE POLICY hot_scope_read ON hot."blob_object" FOR SELECT USING (((env_id IS NULL AND hot.scope_org_allows(org_id))) OR ((env_id IS NOT NULL AND hot.scope_env_allows(env_id) AND hot.scope_org_allows(org_id))) OR (hot.scope_billing_org(org_id)) OR (hot.scope_maintenance('blob_gc')));
CREATE POLICY hot_scope_insert ON hot."blob_object" FOR INSERT WITH CHECK (((env_id IS NULL AND hot.scope_org_writes(org_id))) OR ((env_id IS NOT NULL AND hot.scope_env_writes(env_id) AND hot.scope_org_allows(org_id))) OR (hot.scope_maintenance('blob_gc')));
CREATE POLICY hot_scope_update ON hot."blob_object" FOR UPDATE USING (((env_id IS NULL AND hot.scope_org_writes(org_id))) OR ((env_id IS NOT NULL AND hot.scope_env_writes(env_id) AND hot.scope_org_allows(org_id))) OR (hot.scope_maintenance('blob_gc'))) WITH CHECK (((env_id IS NULL AND hot.scope_org_writes(org_id))) OR ((env_id IS NOT NULL AND hot.scope_env_writes(env_id) AND hot.scope_org_allows(org_id))) OR (hot.scope_maintenance('blob_gc')));
CREATE POLICY hot_scope_delete ON hot."blob_object" FOR DELETE USING (((env_id IS NULL AND hot.scope_org_writes(org_id))) OR ((env_id IS NOT NULL AND hot.scope_env_writes(env_id) AND hot.scope_org_allows(org_id))) OR (hot.scope_maintenance('blob_gc')));

-- blob_ref: separate SELECT and mutation grants; a read set is never a write grant.
ALTER TABLE hot."blob_ref" ENABLE ROW LEVEL SECURITY;
ALTER TABLE hot."blob_ref" FORCE ROW LEVEL SECURITY;
CREATE POLICY hot_scope_read ON hot."blob_ref" FOR SELECT USING (((env_id IS NULL AND hot.scope_org_allows(org_id))) OR ((env_id IS NOT NULL AND hot.scope_env_allows(env_id) AND hot.scope_org_allows(org_id))) OR (hot.scope_billing_org(org_id)) OR (hot.scope_maintenance('blob_gc')));
CREATE POLICY hot_scope_insert ON hot."blob_ref" FOR INSERT WITH CHECK (((env_id IS NULL AND hot.scope_org_writes(org_id))) OR ((env_id IS NOT NULL AND hot.scope_env_writes(env_id) AND hot.scope_org_allows(org_id))) OR (hot.scope_maintenance('blob_gc')));
CREATE POLICY hot_scope_update ON hot."blob_ref" FOR UPDATE USING (((env_id IS NULL AND hot.scope_org_writes(org_id))) OR ((env_id IS NOT NULL AND hot.scope_env_writes(env_id) AND hot.scope_org_allows(org_id))) OR (hot.scope_maintenance('blob_gc'))) WITH CHECK (((env_id IS NULL AND hot.scope_org_writes(org_id))) OR ((env_id IS NOT NULL AND hot.scope_env_writes(env_id) AND hot.scope_org_allows(org_id))) OR (hot.scope_maintenance('blob_gc')));
CREATE POLICY hot_scope_delete ON hot."blob_ref" FOR DELETE USING (((env_id IS NULL AND hot.scope_org_writes(org_id))) OR ((env_id IS NOT NULL AND hot.scope_env_writes(env_id) AND hot.scope_org_allows(org_id))) OR (hot.scope_maintenance('blob_gc')));

-- alert_subscription: separate SELECT and mutation grants; a read set is never a write grant.
ALTER TABLE hot."alert_subscription" ENABLE ROW LEVEL SECURITY;
ALTER TABLE hot."alert_subscription" FORCE ROW LEVEL SECURITY;
CREATE POLICY hot_scope_read ON hot."alert_subscription" FOR SELECT USING (((env_id IS NULL AND hot.scope_org_allows(org_id))) OR ((env_id IS NOT NULL AND hot.scope_env_allows(env_id) AND hot.scope_org_allows(org_id))) OR (hot.scope_maintenance('alerts')));
CREATE POLICY hot_scope_insert ON hot."alert_subscription" FOR INSERT WITH CHECK (((env_id IS NULL AND hot.scope_org_writes(org_id))) OR ((env_id IS NOT NULL AND hot.scope_env_writes(env_id) AND hot.scope_org_allows(org_id))) OR (hot.scope_maintenance('alerts')));
CREATE POLICY hot_scope_update ON hot."alert_subscription" FOR UPDATE USING (((env_id IS NULL AND hot.scope_org_writes(org_id))) OR ((env_id IS NOT NULL AND hot.scope_env_writes(env_id) AND hot.scope_org_allows(org_id))) OR (hot.scope_maintenance('alerts'))) WITH CHECK (((env_id IS NULL AND hot.scope_org_writes(org_id))) OR ((env_id IS NOT NULL AND hot.scope_env_writes(env_id) AND hot.scope_org_allows(org_id))) OR (hot.scope_maintenance('alerts')));
CREATE POLICY hot_scope_delete ON hot."alert_subscription" FOR DELETE USING (((env_id IS NULL AND hot.scope_org_writes(org_id))) OR ((env_id IS NOT NULL AND hot.scope_env_writes(env_id) AND hot.scope_org_allows(org_id))) OR (hot.scope_maintenance('alerts')));

-- store_map: separate SELECT and mutation grants; a read set is never a write grant.
ALTER TABLE hot."store_map" ENABLE ROW LEVEL SECURITY;
ALTER TABLE hot."store_map" FORCE ROW LEVEL SECURITY;
CREATE POLICY hot_scope_read ON hot."store_map" FOR SELECT USING (hot.scope_maintenance('store_provisioning') OR (hot.scope_env_allows(env_id) AND hot.scope_org_allows(org_id)) OR (hot.scope_billing_org(org_id)));
CREATE POLICY hot_scope_insert ON hot."store_map" FOR INSERT WITH CHECK (hot.scope_env_writes(env_id) AND hot.scope_org_allows(org_id));
CREATE POLICY hot_scope_update ON hot."store_map" FOR UPDATE USING (hot.scope_env_writes(env_id) AND hot.scope_org_allows(org_id)) WITH CHECK (hot.scope_env_writes(env_id) AND hot.scope_org_allows(org_id));
CREATE POLICY hot_scope_delete ON hot."store_map" FOR DELETE USING (hot.scope_env_writes(env_id) AND hot.scope_org_allows(org_id));

-- store_map_entry: separate SELECT and mutation grants; a read set is never a write grant.
ALTER TABLE hot."store_map_entry" ENABLE ROW LEVEL SECURITY;
ALTER TABLE hot."store_map_entry" FORCE ROW LEVEL SECURITY;
CREATE POLICY hot_scope_read ON hot."store_map_entry" FOR SELECT USING ((hot.scope_env_allows(env_id) AND hot.scope_org_allows(org_id)) OR (hot.scope_billing_org(org_id)));
CREATE POLICY hot_scope_insert ON hot."store_map_entry" FOR INSERT WITH CHECK (hot.scope_env_writes(env_id) AND hot.scope_org_allows(org_id));
CREATE POLICY hot_scope_update ON hot."store_map_entry" FOR UPDATE USING (hot.scope_env_writes(env_id) AND hot.scope_org_allows(org_id)) WITH CHECK (hot.scope_env_writes(env_id) AND hot.scope_org_allows(org_id));
CREATE POLICY hot_scope_delete ON hot."store_map_entry" FOR DELETE USING (hot.scope_env_writes(env_id) AND hot.scope_org_allows(org_id));

-- context: separate SELECT and mutation grants; a read set is never a write grant.
ALTER TABLE hot."context" ENABLE ROW LEVEL SECURITY;
ALTER TABLE hot."context" FORCE ROW LEVEL SECURITY;
CREATE POLICY hot_scope_read ON hot."context" FOR SELECT USING (((env_id IS NOT NULL AND hot.scope_env_allows(env_id))) OR ((project_id IS NOT NULL AND EXISTS (SELECT 1 FROM hot.project p WHERE p.project_id = context.project_id AND hot.scope_env_allows(p.env_id)))));
CREATE POLICY hot_scope_insert ON hot."context" FOR INSERT WITH CHECK (((env_id IS NOT NULL AND hot.scope_env_writes(env_id))) OR ((project_id IS NOT NULL AND EXISTS (SELECT 1 FROM hot.project p WHERE p.project_id = context.project_id AND hot.scope_env_writes(p.env_id)))));
CREATE POLICY hot_scope_update ON hot."context" FOR UPDATE USING (((env_id IS NOT NULL AND hot.scope_env_writes(env_id))) OR ((project_id IS NOT NULL AND EXISTS (SELECT 1 FROM hot.project p WHERE p.project_id = context.project_id AND hot.scope_env_writes(p.env_id))))) WITH CHECK (((env_id IS NOT NULL AND hot.scope_env_writes(env_id))) OR ((project_id IS NOT NULL AND EXISTS (SELECT 1 FROM hot.project p WHERE p.project_id = context.project_id AND hot.scope_env_writes(p.env_id)))));
CREATE POLICY hot_scope_delete ON hot."context" FOR DELETE USING (((env_id IS NOT NULL AND hot.scope_env_writes(env_id))) OR ((project_id IS NOT NULL AND EXISTS (SELECT 1 FROM hot.project p WHERE p.project_id = context.project_id AND hot.scope_env_writes(p.env_id)))));

-- alert: separate SELECT and mutation grants; a read set is never a write grant.
ALTER TABLE hot."alert" ENABLE ROW LEVEL SECURITY;
ALTER TABLE hot."alert" FORCE ROW LEVEL SECURITY;
CREATE POLICY hot_scope_read ON hot."alert" FOR SELECT USING ((hot.scope_env_allows(env_id) AND hot.scope_org_allows(org_id)) OR (hot.scope_maintenance('alerts') OR hot.scope_maintenance('retention')));
CREATE POLICY hot_scope_insert ON hot."alert" FOR INSERT WITH CHECK ((hot.scope_env_writes(env_id) AND hot.scope_org_allows(org_id)) OR (hot.scope_maintenance('alerts') OR hot.scope_maintenance('retention')));
CREATE POLICY hot_scope_update ON hot."alert" FOR UPDATE USING ((hot.scope_env_writes(env_id) AND hot.scope_org_allows(org_id)) OR (hot.scope_maintenance('alerts') OR hot.scope_maintenance('retention'))) WITH CHECK ((hot.scope_env_writes(env_id) AND hot.scope_org_allows(org_id)) OR (hot.scope_maintenance('alerts') OR hot.scope_maintenance('retention')));
CREATE POLICY hot_scope_delete ON hot."alert" FOR DELETE USING ((hot.scope_env_writes(env_id) AND hot.scope_org_allows(org_id)) OR (hot.scope_maintenance('alerts') OR hot.scope_maintenance('retention')));

-- alert_destination: separate SELECT and mutation grants; a read set is never a write grant.
ALTER TABLE hot."alert_destination" ENABLE ROW LEVEL SECURITY;
ALTER TABLE hot."alert_destination" FORCE ROW LEVEL SECURITY;
CREATE POLICY hot_scope_read ON hot."alert_destination" FOR SELECT USING ((hot.scope_org_allows(org_id)) OR (hot.scope_maintenance('alerts')));
CREATE POLICY hot_scope_insert ON hot."alert_destination" FOR INSERT WITH CHECK ((hot.scope_org_writes(org_id)) OR (hot.scope_maintenance('alerts')));
CREATE POLICY hot_scope_update ON hot."alert_destination" FOR UPDATE USING ((hot.scope_org_writes(org_id)) OR (hot.scope_maintenance('alerts'))) WITH CHECK ((hot.scope_org_writes(org_id)) OR (hot.scope_maintenance('alerts')));
CREATE POLICY hot_scope_delete ON hot."alert_destination" FOR DELETE USING ((hot.scope_org_writes(org_id)) OR (hot.scope_maintenance('alerts')));

-- alert_channel: separate SELECT and mutation grants; a read set is never a write grant.
ALTER TABLE hot."alert_channel" ENABLE ROW LEVEL SECURITY;
ALTER TABLE hot."alert_channel" FORCE ROW LEVEL SECURITY;
CREATE POLICY hot_scope_read ON hot."alert_channel" FOR SELECT USING ((org_id IS NULL AND hot.scope_mode() <> '') OR (org_id IS NOT NULL AND hot.scope_org_allows(org_id) AND (env_id IS NULL OR hot.scope_env_allows(env_id))) OR (hot.scope_maintenance('alerts')));
CREATE POLICY hot_scope_insert ON hot."alert_channel" FOR INSERT WITH CHECK ((org_id IS NOT NULL AND env_id IS NULL AND hot.scope_org_writes(org_id)) OR (env_id IS NOT NULL AND hot.scope_org_allows(org_id) AND hot.scope_env_writes(env_id)) OR hot.scope_maintenance('alerts'));
CREATE POLICY hot_scope_update ON hot."alert_channel" FOR UPDATE USING ((org_id IS NOT NULL AND env_id IS NULL AND hot.scope_org_writes(org_id)) OR (env_id IS NOT NULL AND hot.scope_org_allows(org_id) AND hot.scope_env_writes(env_id)) OR hot.scope_maintenance('alerts')) WITH CHECK ((org_id IS NOT NULL AND env_id IS NULL AND hot.scope_org_writes(org_id)) OR (env_id IS NOT NULL AND hot.scope_org_allows(org_id) AND hot.scope_env_writes(env_id)) OR hot.scope_maintenance('alerts'));
CREATE POLICY hot_scope_delete ON hot."alert_channel" FOR DELETE USING ((org_id IS NOT NULL AND env_id IS NULL AND hot.scope_org_writes(org_id)) OR (env_id IS NOT NULL AND hot.scope_org_allows(org_id) AND hot.scope_env_writes(env_id)) OR hot.scope_maintenance('alerts'));

-- user: separate SELECT and mutation grants; a read set is never a write grant.
ALTER TABLE hot."user" ENABLE ROW LEVEL SECURITY;
ALTER TABLE hot."user" FORCE ROW LEVEL SECURITY;
CREATE POLICY hot_scope_read ON hot."user" FOR SELECT USING ((hot.scope_mode() = 'user' AND user_id = NULLIF(current_setting('hot.scope_user', true), '')::uuid) OR EXISTS (SELECT 1 FROM hot.org_user ou WHERE ou.user_id = "user".user_id AND ou.active AND hot.scope_org_allows(ou.org_id)) OR hot.scope_bootstrap('authentication') OR hot.scope_bootstrap('membership') OR hot.scope_bootstrap('onboarding'));
CREATE POLICY hot_scope_insert ON hot."user" FOR INSERT WITH CHECK (hot.scope_bootstrap('authentication') OR hot.scope_bootstrap('onboarding'));
CREATE POLICY hot_scope_update ON hot."user" FOR UPDATE USING ((hot.scope_mode() = 'user' AND user_id = NULLIF(current_setting('hot.scope_user', true), '')::uuid) OR hot.scope_bootstrap('authentication') OR hot.scope_bootstrap('onboarding')) WITH CHECK ((hot.scope_mode() = 'user' AND user_id = NULLIF(current_setting('hot.scope_user', true), '')::uuid) OR hot.scope_bootstrap('authentication') OR hot.scope_bootstrap('onboarding'));
CREATE POLICY hot_scope_delete ON hot."user" FOR DELETE USING (hot.scope_bootstrap('authentication') OR hot.scope_bootstrap('onboarding'));

-- user_auth: separate SELECT and mutation grants; a read set is never a write grant.
ALTER TABLE hot."user_auth" ENABLE ROW LEVEL SECURITY;
ALTER TABLE hot."user_auth" FORCE ROW LEVEL SECURITY;
CREATE POLICY hot_scope_read ON hot."user_auth" FOR SELECT USING (hot.scope_bootstrap('authentication') OR hot.scope_bootstrap('onboarding'));
CREATE POLICY hot_scope_insert ON hot."user_auth" FOR INSERT WITH CHECK (hot.scope_bootstrap('authentication') OR hot.scope_bootstrap('onboarding'));
CREATE POLICY hot_scope_update ON hot."user_auth" FOR UPDATE USING (hot.scope_bootstrap('authentication') OR hot.scope_bootstrap('onboarding')) WITH CHECK (hot.scope_bootstrap('authentication') OR hot.scope_bootstrap('onboarding'));
CREATE POLICY hot_scope_delete ON hot."user_auth" FOR DELETE USING (hot.scope_bootstrap('authentication') OR hot.scope_bootstrap('onboarding'));

-- email_verification: separate SELECT and mutation grants; a read set is never a write grant.
ALTER TABLE hot."email_verification" ENABLE ROW LEVEL SECURITY;
ALTER TABLE hot."email_verification" FORCE ROW LEVEL SECURITY;
CREATE POLICY hot_scope_read ON hot."email_verification" FOR SELECT USING (hot.scope_bootstrap('authentication') OR hot.scope_bootstrap('onboarding'));
CREATE POLICY hot_scope_insert ON hot."email_verification" FOR INSERT WITH CHECK (hot.scope_bootstrap('authentication') OR hot.scope_bootstrap('onboarding'));
CREATE POLICY hot_scope_update ON hot."email_verification" FOR UPDATE USING (hot.scope_bootstrap('authentication') OR hot.scope_bootstrap('onboarding')) WITH CHECK (hot.scope_bootstrap('authentication') OR hot.scope_bootstrap('onboarding'));
CREATE POLICY hot_scope_delete ON hot."email_verification" FOR DELETE USING (hot.scope_bootstrap('authentication') OR hot.scope_bootstrap('onboarding'));

-- email_queue: separate SELECT and mutation grants; a read set is never a write grant.
ALTER TABLE hot."email_queue" ENABLE ROW LEVEL SECURITY;
ALTER TABLE hot."email_queue" FORCE ROW LEVEL SECURITY;
CREATE POLICY hot_scope_read ON hot."email_queue" FOR SELECT USING ((hot.scope_bootstrap('onboarding')) OR (hot.scope_maintenance('email')));
CREATE POLICY hot_scope_insert ON hot."email_queue" FOR INSERT WITH CHECK ((hot.scope_bootstrap('onboarding')) OR (hot.scope_maintenance('email')));
CREATE POLICY hot_scope_update ON hot."email_queue" FOR UPDATE USING ((hot.scope_bootstrap('onboarding')) OR (hot.scope_maintenance('email'))) WITH CHECK ((hot.scope_bootstrap('onboarding')) OR (hot.scope_maintenance('email')));
CREATE POLICY hot_scope_delete ON hot."email_queue" FOR DELETE USING ((hot.scope_bootstrap('onboarding')) OR (hot.scope_maintenance('email')));

-- scheduler_state: separate SELECT and mutation grants; a read set is never a write grant.
ALTER TABLE hot."scheduler_state" ENABLE ROW LEVEL SECURITY;
ALTER TABLE hot."scheduler_state" FORCE ROW LEVEL SECURITY;
CREATE POLICY hot_scope_read ON hot."scheduler_state" FOR SELECT USING (hot.scope_maintenance('scheduler'));
CREATE POLICY hot_scope_insert ON hot."scheduler_state" FOR INSERT WITH CHECK (hot.scope_maintenance('scheduler'));
CREATE POLICY hot_scope_update ON hot."scheduler_state" FOR UPDATE USING (hot.scope_maintenance('scheduler')) WITH CHECK (hot.scope_maintenance('scheduler'));
CREATE POLICY hot_scope_delete ON hot."scheduler_state" FOR DELETE USING (hot.scope_maintenance('scheduler'));


-- Account-rate admission is a root billing operation, never a child pool.
ALTER TABLE hot.account_request_window ENABLE ROW LEVEL SECURITY;
ALTER TABLE hot.account_request_window FORCE ROW LEVEL SECURITY;
CREATE POLICY account_request_window_scope ON hot.account_request_window
    USING (hot.scope_maintenance('billing') AND root_org_id = hot.scope_org_id())
    WITH CHECK (hot.scope_maintenance('billing') AND root_org_id = hot.scope_org_id());

-- Immutability prevents a permitted update from moving an existing resource
-- between projects/scopes. RLS alone checks rows, not historical ownership.
-- Each trigger's WHEN clause skips the check unless an ownership column
-- changes, so frequent updates (traces, heartbeats, counters) do not pay for it.
CREATE FUNCTION hot.prevent_scope_reassignment() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE column_name text;
BEGIN
    FOREACH column_name IN ARRAY TG_ARGV LOOP
        IF to_jsonb(NEW) -> column_name IS DISTINCT FROM to_jsonb(OLD) -> column_name THEN
            RAISE EXCEPTION 'Resource ownership is immutable' USING ERRCODE = '42501';
        END IF;
    END LOOP;
    RETURN NEW;
END $$;
CREATE TRIGGER immutable_resource_scope BEFORE UPDATE ON hot.env
    FOR EACH ROW WHEN (OLD.org_id IS DISTINCT FROM NEW.org_id)
    EXECUTE FUNCTION hot.prevent_scope_reassignment('org_id');
CREATE TRIGGER immutable_resource_scope BEFORE UPDATE ON hot.project
    FOR EACH ROW WHEN (OLD.env_id IS DISTINCT FROM NEW.env_id)
    EXECUTE FUNCTION hot.prevent_scope_reassignment('env_id');
CREATE TRIGGER immutable_resource_scope BEFORE UPDATE ON hot.build
    FOR EACH ROW WHEN (OLD.project_id IS DISTINCT FROM NEW.project_id)
    EXECUTE FUNCTION hot.prevent_scope_reassignment('project_id');
CREATE TRIGGER immutable_resource_scope BEFORE UPDATE ON hot.run
    FOR EACH ROW WHEN ((OLD.env_id, OLD.build_id, OLD.stream_id) IS DISTINCT FROM (NEW.env_id, NEW.build_id, NEW.stream_id))
    EXECUTE FUNCTION hot.prevent_scope_reassignment('env_id', 'build_id', 'stream_id');
CREATE TRIGGER immutable_resource_scope BEFORE UPDATE ON hot.task
    FOR EACH ROW WHEN ((OLD.env_id, OLD.build_id, OLD.stream_id) IS DISTINCT FROM (NEW.env_id, NEW.build_id, NEW.stream_id))
    EXECUTE FUNCTION hot.prevent_scope_reassignment('env_id', 'build_id', 'stream_id');
CREATE TRIGGER immutable_resource_scope BEFORE UPDATE ON hot.event
    FOR EACH ROW WHEN ((OLD.env_id, OLD.stream_id) IS DISTINCT FROM (NEW.env_id, NEW.stream_id))
    EXECUTE FUNCTION hot.prevent_scope_reassignment('env_id', 'stream_id');
CREATE TRIGGER immutable_resource_scope BEFORE UPDATE ON hot.stream
    FOR EACH ROW WHEN (OLD.env_id IS DISTINCT FROM NEW.env_id)
    EXECUTE FUNCTION hot.prevent_scope_reassignment('env_id');
CREATE TRIGGER immutable_resource_scope BEFORE UPDATE ON hot.api_key
    FOR EACH ROW WHEN (OLD.env_id IS DISTINCT FROM NEW.env_id)
    EXECUTE FUNCTION hot.prevent_scope_reassignment('env_id');
CREATE TRIGGER immutable_resource_scope BEFORE UPDATE ON hot.service_key
    FOR EACH ROW WHEN ((OLD.env_id, OLD.api_key_id) IS DISTINCT FROM (NEW.env_id, NEW.api_key_id))
    EXECUTE FUNCTION hot.prevent_scope_reassignment('env_id', 'api_key_id');
CREATE TRIGGER immutable_resource_scope BEFORE UPDATE ON hot.session
    FOR EACH ROW WHEN ((OLD.env_id, OLD.api_key_id) IS DISTINCT FROM (NEW.env_id, NEW.api_key_id))
    EXECUTE FUNCTION hot.prevent_scope_reassignment('env_id', 'api_key_id');
CREATE TRIGGER immutable_resource_scope BEFORE UPDATE ON hot.agent
    FOR EACH ROW WHEN ((OLD.env_id, OLD.build_id) IS DISTINCT FROM (NEW.env_id, NEW.build_id))
    EXECUTE FUNCTION hot.prevent_scope_reassignment('env_id', 'build_id');
CREATE TRIGGER immutable_resource_scope BEFORE UPDATE ON hot.workflow
    FOR EACH ROW WHEN ((OLD.env_id, OLD.build_id) IS DISTINCT FROM (NEW.env_id, NEW.build_id))
    EXECUTE FUNCTION hot.prevent_scope_reassignment('env_id', 'build_id');
CREATE TRIGGER immutable_resource_scope BEFORE UPDATE ON hot.file
    FOR EACH ROW WHEN ((OLD.org_id, OLD.env_id) IS DISTINCT FROM (NEW.org_id, NEW.env_id))
    EXECUTE FUNCTION hot.prevent_scope_reassignment('org_id', 'env_id');
CREATE TRIGGER immutable_resource_scope BEFORE UPDATE ON hot.file_upload
    FOR EACH ROW WHEN ((OLD.org_id, OLD.env_id) IS DISTINCT FROM (NEW.org_id, NEW.env_id))
    EXECUTE FUNCTION hot.prevent_scope_reassignment('org_id', 'env_id');
CREATE TRIGGER immutable_resource_scope BEFORE UPDATE ON hot.file_upload_part_claim
    FOR EACH ROW WHEN (OLD.upload_id IS DISTINCT FROM NEW.upload_id)
    EXECUTE FUNCTION hot.prevent_scope_reassignment('upload_id');
CREATE TRIGGER immutable_resource_scope BEFORE UPDATE ON hot.blob_object
    FOR EACH ROW WHEN ((OLD.org_id, OLD.env_id) IS DISTINCT FROM (NEW.org_id, NEW.env_id))
    EXECUTE FUNCTION hot.prevent_scope_reassignment('org_id', 'env_id');
CREATE TRIGGER immutable_resource_scope BEFORE UPDATE ON hot.blob_ref
    FOR EACH ROW WHEN ((OLD.org_id, OLD.env_id, OLD.blob_object_id) IS DISTINCT FROM (NEW.org_id, NEW.env_id, NEW.blob_object_id))
    EXECUTE FUNCTION hot.prevent_scope_reassignment('org_id', 'env_id', 'blob_object_id');
CREATE TRIGGER immutable_resource_scope BEFORE UPDATE ON hot.store_map
    FOR EACH ROW WHEN ((OLD.org_id, OLD.env_id) IS DISTINCT FROM (NEW.org_id, NEW.env_id))
    EXECUTE FUNCTION hot.prevent_scope_reassignment('org_id', 'env_id');
CREATE TRIGGER immutable_resource_scope BEFORE UPDATE ON hot.store_map_entry
    FOR EACH ROW WHEN ((OLD.org_id, OLD.env_id) IS DISTINCT FROM (NEW.org_id, NEW.env_id))
    EXECUTE FUNCTION hot.prevent_scope_reassignment('org_id', 'env_id');
CREATE TRIGGER immutable_resource_scope BEFORE UPDATE ON hot.domain
    FOR EACH ROW WHEN (OLD.env_id IS DISTINCT FROM NEW.env_id)
    EXECUTE FUNCTION hot.prevent_scope_reassignment('env_id');
CREATE TRIGGER immutable_resource_scope BEFORE UPDATE ON hot.schedule
    FOR EACH ROW WHEN (OLD.build_id IS DISTINCT FROM NEW.build_id)
    EXECUTE FUNCTION hot.prevent_scope_reassignment('build_id');
CREATE TRIGGER immutable_resource_scope BEFORE UPDATE ON hot.mcp_tool
    FOR EACH ROW WHEN (OLD.build_id IS DISTINCT FROM NEW.build_id)
    EXECUTE FUNCTION hot.prevent_scope_reassignment('build_id');
CREATE TRIGGER immutable_resource_scope BEFORE UPDATE ON hot.event_handler
    FOR EACH ROW WHEN (OLD.build_id IS DISTINCT FROM NEW.build_id)
    EXECUTE FUNCTION hot.prevent_scope_reassignment('build_id');
CREATE TRIGGER immutable_resource_scope BEFORE UPDATE ON hot.context
    FOR EACH ROW WHEN ((OLD.env_id, OLD.project_id) IS DISTINCT FROM (NEW.env_id, NEW.project_id))
    EXECUTE FUNCTION hot.prevent_scope_reassignment('env_id', 'project_id');
CREATE TRIGGER immutable_resource_scope BEFORE UPDATE ON hot.alert
    FOR EACH ROW WHEN ((OLD.org_id, OLD.env_id) IS DISTINCT FROM (NEW.org_id, NEW.env_id))
    EXECUTE FUNCTION hot.prevent_scope_reassignment('org_id', 'env_id');

-- Validate cross-resource ownership on writes. PostgreSQL foreign-key checks
-- intentionally bypass RLS, so FK existence alone is not an isolation check.
CREATE FUNCTION hot.enforce_resource_scope() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE r jsonb := to_jsonb(NEW);
        resource_env uuid := NULLIF(r ->> 'env_id','')::uuid;
        resource_org uuid := NULLIF(r ->> 'org_id','')::uuid;
        linked uuid;
BEGIN
    IF resource_env IS NOT NULL AND resource_org IS NOT NULL AND NOT EXISTS
        (SELECT 1 FROM hot.env e WHERE e.env_id = resource_env AND e.org_id = resource_org) THEN
        RAISE EXCEPTION 'Environment ownership mismatch' USING ERRCODE = '42501';
    END IF;
    IF TG_TABLE_NAME IN ('run','task','agent','workflow') THEN
        linked := (r ->> 'build_id')::uuid;
        IF NOT EXISTS (SELECT 1 FROM hot.build b JOIN hot.project p ON p.project_id = b.project_id
                       WHERE b.build_id = linked AND p.env_id = resource_env) THEN
            RAISE EXCEPTION 'Build ownership mismatch' USING ERRCODE = '42501';
        END IF;
    END IF;
    IF TG_TABLE_NAME IN ('run','task','event') THEN
        linked := (r ->> 'stream_id')::uuid;
        IF NOT EXISTS (SELECT 1 FROM hot.stream s WHERE s.stream_id = linked AND s.env_id = resource_env) THEN
            RAISE EXCEPTION 'Stream ownership mismatch' USING ERRCODE = '42501';
        END IF;
        linked := NULLIF(r ->> 'origin_run_id','')::uuid;
        IF linked IS NOT NULL AND NOT EXISTS
            (SELECT 1 FROM hot.run p WHERE p.run_id = linked AND p.env_id = resource_env) THEN
            RAISE EXCEPTION 'Origin run ownership mismatch' USING ERRCODE = '42501';
        END IF;
    END IF;
    IF TG_TABLE_NAME = 'task' THEN
        linked := NULLIF(r ->> 'run_id','')::uuid;
        IF linked IS NOT NULL AND NOT EXISTS
            (SELECT 1 FROM hot.run p WHERE p.run_id = linked AND p.env_id = resource_env) THEN
            RAISE EXCEPTION 'Task run ownership mismatch' USING ERRCODE = '42501';
        END IF;
    END IF;
    IF TG_TABLE_NAME = 'context' AND resource_env IS NOT NULL THEN
        linked := NULLIF(r ->> 'project_id','')::uuid;
        IF linked IS NOT NULL AND NOT EXISTS
            (SELECT 1 FROM hot.project p WHERE p.project_id = linked AND p.env_id = resource_env) THEN
            RAISE EXCEPTION 'Context project ownership mismatch' USING ERRCODE = '42501';
        END IF;
    END IF;
    IF TG_TABLE_NAME = 'run' THEN
        linked := NULLIF(r ->> 'event_id','')::uuid;
        IF linked IS NOT NULL AND NOT EXISTS
            (SELECT 1 FROM hot.event e WHERE e.event_id = linked AND e.env_id = resource_env) THEN
            RAISE EXCEPTION 'Event ownership mismatch' USING ERRCODE = '42501';
        END IF;
    END IF;
    IF TG_TABLE_NAME IN ('service_key','session') AND NOT EXISTS
        (SELECT 1 FROM hot.api_key k WHERE k.api_key_id = (r ->> 'api_key_id')::uuid AND k.env_id = resource_env) THEN
        RAISE EXCEPTION 'Credential ownership mismatch' USING ERRCODE = '42501';
    END IF;
    IF TG_TABLE_NAME = 'blob_ref' AND NOT EXISTS
        (SELECT 1 FROM hot.blob_object b WHERE b.blob_object_id = (r ->> 'blob_object_id')::uuid
            AND b.org_id = resource_org AND b.env_id IS NOT DISTINCT FROM resource_env) THEN
        RAISE EXCEPTION 'Blob ownership mismatch' USING ERRCODE = '42501';
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER resource_scope_integrity BEFORE INSERT OR UPDATE ON hot.run
    FOR EACH ROW EXECUTE FUNCTION hot.enforce_resource_scope();
CREATE TRIGGER resource_scope_integrity BEFORE INSERT OR UPDATE ON hot.task
    FOR EACH ROW EXECUTE FUNCTION hot.enforce_resource_scope();
CREATE TRIGGER resource_scope_integrity BEFORE INSERT OR UPDATE ON hot.event
    FOR EACH ROW EXECUTE FUNCTION hot.enforce_resource_scope();
CREATE TRIGGER resource_scope_integrity BEFORE INSERT OR UPDATE ON hot.agent
    FOR EACH ROW EXECUTE FUNCTION hot.enforce_resource_scope();
CREATE TRIGGER resource_scope_integrity BEFORE INSERT OR UPDATE ON hot.workflow
    FOR EACH ROW EXECUTE FUNCTION hot.enforce_resource_scope();
CREATE TRIGGER resource_scope_integrity BEFORE INSERT OR UPDATE ON hot.service_key
    FOR EACH ROW EXECUTE FUNCTION hot.enforce_resource_scope();
CREATE TRIGGER resource_scope_integrity BEFORE INSERT OR UPDATE ON hot.session
    FOR EACH ROW EXECUTE FUNCTION hot.enforce_resource_scope();
CREATE TRIGGER resource_scope_integrity BEFORE INSERT OR UPDATE ON hot.file
    FOR EACH ROW EXECUTE FUNCTION hot.enforce_resource_scope();
CREATE TRIGGER resource_scope_integrity BEFORE INSERT OR UPDATE ON hot.alert_channel
    FOR EACH ROW EXECUTE FUNCTION hot.enforce_resource_scope();
CREATE TRIGGER resource_scope_integrity BEFORE INSERT OR UPDATE ON hot.alert_subscription
    FOR EACH ROW EXECUTE FUNCTION hot.enforce_resource_scope();
CREATE TRIGGER resource_scope_integrity BEFORE INSERT OR UPDATE ON hot.file_upload
    FOR EACH ROW EXECUTE FUNCTION hot.enforce_resource_scope();
CREATE TRIGGER resource_scope_integrity BEFORE INSERT OR UPDATE ON hot.blob_object
    FOR EACH ROW EXECUTE FUNCTION hot.enforce_resource_scope();
CREATE TRIGGER resource_scope_integrity BEFORE INSERT OR UPDATE ON hot.blob_ref
    FOR EACH ROW EXECUTE FUNCTION hot.enforce_resource_scope();
CREATE TRIGGER resource_scope_integrity BEFORE INSERT OR UPDATE ON hot.store_map
    FOR EACH ROW EXECUTE FUNCTION hot.enforce_resource_scope();
CREATE TRIGGER resource_scope_integrity BEFORE INSERT OR UPDATE ON hot.store_map_entry
    FOR EACH ROW EXECUTE FUNCTION hot.enforce_resource_scope();
CREATE TRIGGER resource_scope_integrity BEFORE INSERT OR UPDATE ON hot.alert
    FOR EACH ROW EXECUTE FUNCTION hot.enforce_resource_scope();

CREATE TRIGGER resource_scope_integrity BEFORE INSERT OR UPDATE ON hot.context
    FOR EACH ROW EXECUTE FUNCTION hot.enforce_resource_scope();
CREATE TRIGGER immutable_resource_scope BEFORE UPDATE ON hot.call
    FOR EACH ROW WHEN ((OLD.run_id, OLD.env_id) IS DISTINCT FROM (NEW.run_id, NEW.env_id))
    EXECUTE FUNCTION hot.prevent_scope_reassignment('run_id', 'env_id');

-- Management credentials are account control data. An environment of the root
-- organization must not gain account authority merely by sharing its org_id.
CREATE TABLE hot.management_key (
    management_key_id uuid PRIMARY KEY,
    root_org_id uuid NOT NULL REFERENCES hot.org(org_id),
    created_by_user_id uuid NOT NULL REFERENCES hot."user"(user_id),
    name text NOT NULL,
    secret_hash bytea NOT NULL,
    capabilities jsonb NOT NULL,
    target_org_ids jsonb NOT NULL DEFAULT '[]'::jsonb,
    all_children boolean NOT NULL DEFAULT false,
    delegation_permissions jsonb,
    expires_at timestamptz NOT NULL,
    revoked_at timestamptz,
    created_at timestamptz NOT NULL DEFAULT CURRENT_TIMESTAMP
);
CREATE INDEX management_key_root_idx ON hot.management_key(root_org_id);
-- A cascade would let an environment delete and recreate a delegated API key
-- without its issuer, laundering the management ceiling and revocation chain.
-- Delegated keys are revoked using active=false; physical deletion requires
-- administrative removal of the protected issuer record first.
CREATE TABLE hot.api_key_management_issuer (
    api_key_id uuid PRIMARY KEY REFERENCES hot.api_key(api_key_id) ON DELETE RESTRICT,
    management_key_id uuid NOT NULL REFERENCES hot.management_key(management_key_id),
    created_at timestamptz NOT NULL DEFAULT CURRENT_TIMESTAMP
);
CREATE INDEX api_key_management_issuer_key_idx ON hot.api_key_management_issuer(management_key_id);

ALTER TABLE hot.management_key ENABLE ROW LEVEL SECURITY;
ALTER TABLE hot.management_key FORCE ROW LEVEL SECURITY;
CREATE POLICY management_key_read ON hot.management_key FOR SELECT USING (
    hot.scope_bootstrap('authentication') OR hot.scope_org_writes(root_org_id)
);
CREATE POLICY management_key_insert ON hot.management_key FOR INSERT WITH CHECK (hot.scope_org_writes(root_org_id));
CREATE POLICY management_key_update ON hot.management_key FOR UPDATE USING (hot.scope_org_writes(root_org_id)) WITH CHECK (hot.scope_org_writes(root_org_id));
CREATE TRIGGER immutable_resource_scope BEFORE UPDATE ON hot.management_key
    FOR EACH ROW WHEN (OLD.root_org_id IS DISTINCT FROM NEW.root_org_id)
    EXECUTE FUNCTION hot.prevent_scope_reassignment('root_org_id');
ALTER TABLE hot.api_key_management_issuer ENABLE ROW LEVEL SECURITY;
ALTER TABLE hot.api_key_management_issuer FORCE ROW LEVEL SECURITY;
CREATE POLICY api_key_management_issuer_read ON hot.api_key_management_issuer FOR SELECT USING (
    hot.scope_bootstrap('authentication') OR EXISTS (
        SELECT 1 FROM hot.api_key k WHERE k.api_key_id = api_key_management_issuer.api_key_id AND hot.scope_env_allows(k.env_id)
    )
);
CREATE POLICY api_key_management_issuer_insert ON hot.api_key_management_issuer FOR INSERT WITH CHECK (
    EXISTS (SELECT 1 FROM hot.api_key k WHERE k.api_key_id = api_key_management_issuer.api_key_id AND hot.scope_env_writes(k.env_id))
);
-- No UPDATE or DELETE policy: runtime-key issuer lineage cannot be detached or
-- reassigned.

-- Foreign-key checks bypass RLS. A delivery must not attach an alert to a
-- foreign subscription or destination, including another environment of the
-- same org.
CREATE FUNCTION hot.enforce_alert_delivery_scope() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM hot.alert a
        JOIN hot.alert_subscription s ON s.alert_subscription_id = NEW.subscription_id
        JOIN hot.alert_destination d ON d.alert_destination_id = NEW.destination_id
        WHERE a.alert_id = NEW.alert_id AND s.org_id = a.org_id AND d.org_id = a.org_id
          AND (s.env_id IS NULL OR s.env_id = a.env_id)
    ) THEN
        RAISE EXCEPTION 'Alert delivery ownership mismatch' USING ERRCODE = '42501';
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER resource_scope_integrity BEFORE INSERT OR UPDATE ON hot.alert_delivery
    FOR EACH ROW EXECUTE FUNCTION hot.enforce_alert_delivery_scope();
CREATE TRIGGER immutable_resource_scope BEFORE UPDATE ON hot.alert_delivery
    FOR EACH ROW WHEN ((OLD.alert_id, OLD.subscription_id, OLD.destination_id) IS DISTINCT FROM (NEW.alert_id, NEW.subscription_id, NEW.destination_id))
    EXECUTE FUNCTION hot.prevent_scope_reassignment('alert_id', 'subscription_id', 'destination_id');

-- Webhook IDs are stable capabilities reused across builds of one project.
-- Redeployment therefore changes build_id without changing resource ownership.
-- Keep the project boundary immutable; build.project_id, project.env_id and
-- env.org_id already have their own immutable ownership guards.
CREATE FUNCTION hot.prevent_webhook_project_reassignment() RETURNS trigger
LANGUAGE plpgsql SECURITY INVOKER SET search_path = pg_catalog, hot AS $$
BEGIN
    IF NEW.build_id IS DISTINCT FROM OLD.build_id AND NOT EXISTS (
        SELECT 1 FROM hot.build previous
        JOIN hot.build replacement ON replacement.project_id = previous.project_id
        WHERE previous.build_id = OLD.build_id AND replacement.build_id = NEW.build_id
    ) THEN
        -- This invoker query also fails closed when either build is hidden by
        -- RLS or missing. A maintenance scope cannot move a webhook to another
        -- project merely because it can see and update both rows.
        RAISE EXCEPTION 'Webhook project ownership is immutable' USING ERRCODE = '42501';
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER immutable_resource_scope BEFORE UPDATE ON hot.webhook
    FOR EACH ROW WHEN (OLD.build_id IS DISTINCT FROM NEW.build_id)
    EXECUTE FUNCTION hot.prevent_webhook_project_reassignment();

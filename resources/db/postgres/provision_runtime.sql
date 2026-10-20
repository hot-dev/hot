-- Apply as the schema owner after all migrations; the runtime login must already
-- exist and have independently managed credentials. Example:
-- psql "$ADMIN_URI" -v runtime_role=hot_runtime -v schema=hot -f provision_runtime.sql
\set ON_ERROR_STOP on
BEGIN;
SELECT format('REVOKE ALL ON ALL TABLES IN SCHEMA %I FROM %I', :'schema', :'runtime_role') \gexec
-- A table-level REVOKE does not remove older column ACLs. Strip the target
-- role's direct column grants as well, including on reference/ledger tables.
SELECT format('REVOKE ALL PRIVILEGES (%s) ON TABLE %I.%I FROM %I',
              string_agg(format('%I', a.attname), ', ' ORDER BY a.attnum),
              n.nspname, c.relname, :'runtime_role')
FROM pg_attribute a JOIN pg_class c ON c.oid = a.attrelid
JOIN pg_namespace n ON n.oid = c.relnamespace
WHERE n.nspname = :'schema' AND c.relkind IN ('r','p')
  AND a.attnum > 0 AND NOT a.attisdropped
  AND EXISTS (SELECT 1 FROM aclexplode(a.attacl) acl
              WHERE acl.grantee = (SELECT oid FROM pg_roles WHERE rolname = :'runtime_role'))
GROUP BY n.nspname, c.relname \gexec
SELECT format('REVOKE CREATE ON SCHEMA %I FROM %I', :'schema', :'runtime_role') \gexec
SELECT format('REVOKE ALL ON ALL SEQUENCES IN SCHEMA %I FROM %I', :'schema', :'runtime_role') \gexec
SELECT format('GRANT USAGE ON SCHEMA %I TO %I', :'schema', :'runtime_role') \gexec
-- Only explicitly protected customer tables receive mutation grants. In
-- particular, do not grant TRUNCATE, REFERENCES, TRIGGER, schema CREATE or role
-- membership. RLS does not protect a table from TRUNCATE.
SELECT format('GRANT SELECT, INSERT, UPDATE, DELETE ON TABLE %I.%I TO %I',
              n.nspname, c.relname, :'runtime_role')
FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
WHERE n.nspname = :'schema' AND c.relkind IN ('r','p')
  AND c.relrowsecurity AND c.relforcerowsecurity \gexec
SELECT format('GRANT SELECT ON TABLE %I.%I TO %I', n.nspname, c.relname, :'runtime_role')
FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
WHERE n.nspname = :'schema' AND c.relkind IN ('r','p')
  AND c.relname IN ('build_type','email_verification_status','invite_status',
      'org_user_role','run_status','run_type','task_status','team_user_role',
      'org_plan_status','alert_delivery_status','alert_destination_type','plan') \gexec
SELECT format('GRANT USAGE, SELECT ON ALL SEQUENCES IN SCHEMA %I TO %I', :'schema', :'runtime_role') \gexec

-- The script owns only the target login's direct grants. PUBLIC and shared
-- group grants need an explicit operator decision; silently changing them can
-- break other services. Abort this whole transaction if those grants or role
-- memberships still make the target unsafe. Check every membership, including
-- NOINHERIT roles usable through SET ROLE, just like runtime qualification.
SELECT set_config('hot.provision_runtime_role', :'runtime_role', true),
       set_config('hot.provision_schema', :'schema', true);
DO $$
DECLARE
    runtime_oid oid := (SELECT oid FROM pg_roles WHERE rolname = current_setting('hot.provision_runtime_role'));
    schema_oid oid := (SELECT oid FROM pg_namespace WHERE nspname = current_setting('hot.provision_schema'));
    reference_tables text[] := ARRAY['build_type','email_verification_status','invite_status',
        'org_user_role','run_status','run_type','task_status','team_user_role',
        'org_plan_status','alert_delivery_status','alert_destination_type','plan'];
BEGIN
    IF runtime_oid IS NULL OR schema_oid IS NULL THEN
        RAISE EXCEPTION 'Runtime role and application schema must exist before provisioning';
    END IF;
    IF EXISTS (
        SELECT 1 FROM pg_roles r WHERE pg_has_role(runtime_oid, r.oid, 'MEMBER')
          AND (r.rolsuper OR r.rolbypassrls OR r.rolcreaterole OR r.rolcreatedb OR r.rolreplication
               OR r.rolname IN ('pg_read_server_files','pg_write_server_files','pg_execute_server_program'))
    ) THEN
        RAISE EXCEPTION 'Runtime role has privileged attributes or role memberships; remove them before provisioning';
    END IF;
    IF EXISTS (
        SELECT 1 FROM pg_roles r WHERE pg_has_role(runtime_oid, r.oid, 'MEMBER')
          AND has_schema_privilege(r.oid, schema_oid, 'CREATE')
    ) THEN
        RAISE EXCEPTION 'Runtime role can CREATE in the application schema through ownership, PUBLIC or a role membership; remove that authority before provisioning';
    END IF;
    IF EXISTS (
        SELECT 1 FROM pg_class c WHERE c.relnamespace = schema_oid AND c.relkind IN ('r','p')
          AND pg_has_role(runtime_oid, c.relowner, 'MEMBER')
    ) THEN
        RAISE EXCEPTION 'Runtime role owns or can assume ownership of application tables; use a separate runtime login';
    END IF;
    IF EXISTS (
        SELECT 1 FROM pg_class c JOIN pg_roles r ON pg_has_role(runtime_oid, r.oid, 'MEMBER')
        WHERE c.relnamespace = schema_oid AND c.relkind IN ('r','p')
          AND (has_table_privilege(r.oid, c.oid, 'TRUNCATE,REFERENCES,TRIGGER')
               OR has_any_column_privilege(r.oid, c.oid, 'REFERENCES'))
    ) THEN
        RAISE EXCEPTION 'Runtime role retains TRUNCATE, REFERENCES or TRIGGER through PUBLIC, role membership or unrevokeable grants; remove those grants before provisioning';
    END IF;
    IF EXISTS (
        SELECT 1 FROM pg_class c JOIN pg_roles r ON pg_has_role(runtime_oid, r.oid, 'MEMBER')
        WHERE c.relnamespace = schema_oid AND c.relkind IN ('r','p') AND c.relname = ANY(reference_tables)
          AND (has_table_privilege(r.oid, c.oid, 'INSERT,UPDATE,DELETE,REFERENCES,TRIGGER')
               OR has_any_column_privilege(r.oid, c.oid, 'INSERT,UPDATE,REFERENCES'))
    ) THEN
        RAISE EXCEPTION 'Runtime role retains reference-table write privileges through PUBLIC, role membership or unrevokeable grants; remove those grants before provisioning';
    END IF;
    IF EXISTS (
        SELECT 1 FROM pg_class c JOIN pg_roles r ON pg_has_role(runtime_oid, r.oid, 'MEMBER')
        WHERE c.relnamespace = schema_oid AND c.relname = '_sqlx_migrations'
          AND (has_table_privilege(r.oid, c.oid, 'SELECT,INSERT,UPDATE,DELETE,TRUNCATE,REFERENCES,TRIGGER')
               OR has_any_column_privilege(r.oid, c.oid, 'SELECT,INSERT,UPDATE,REFERENCES'))
    ) THEN
        RAISE EXCEPTION 'Runtime role retains migration-ledger access through PUBLIC, role membership or unrevokeable grants; remove those grants before provisioning';
    END IF;
    IF EXISTS (
        SELECT 1 FROM pg_class c WHERE c.relnamespace = schema_oid AND c.relkind IN ('r','p')
          AND c.relname <> '_sqlx_migrations' AND NOT (c.relname = ANY(reference_tables))
          AND (NOT c.relrowsecurity OR NOT c.relforcerowsecurity)
    ) OR NOT EXISTS (
        SELECT 1 FROM pg_proc WHERE pronamespace = schema_oid AND proname = 'scope_env_allows'
    ) THEN
        RAISE EXCEPTION 'Apply all RLS migrations before provisioning runtime grants';
    END IF;
END $$;
COMMIT;

-- Apply as the schema owner after all migrations; the runtime login must already
-- exist and have independently managed credentials. Example:
-- psql "$ADMIN_URI" -v runtime_role=hot_runtime -v schema=hot -f provision_runtime.sql
\set ON_ERROR_STOP on
BEGIN;
SELECT format('REVOKE ALL ON ALL TABLES IN SCHEMA %I FROM %I', :'schema', :'runtime_role') \gexec
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
COMMIT;

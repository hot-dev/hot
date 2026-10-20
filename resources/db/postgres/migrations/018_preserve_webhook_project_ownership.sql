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

DROP TRIGGER immutable_resource_scope ON hot.webhook;
CREATE TRIGGER immutable_resource_scope BEFORE UPDATE ON hot.webhook
    FOR EACH ROW EXECUTE FUNCTION hot.prevent_webhook_project_reassignment();

-- A deployment carries its project's dynamic schedules to the new build, so a
-- schedule may move between builds of one project. Any other change of
-- build_id is still refused: a project belongs to one environment and org, so
-- a schedule never leaves its scope. A build the caller can't see reads as
-- NULL here, and is refused.
CREATE FUNCTION hot.prevent_schedule_scope_reassignment() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE new_project uuid;
BEGIN
    SELECT project_id INTO new_project FROM hot.build WHERE build_id = NEW.build_id;
    IF new_project IS NULL
        OR new_project IS DISTINCT FROM (SELECT project_id FROM hot.build WHERE build_id = OLD.build_id) THEN
        RAISE EXCEPTION 'Resource ownership is immutable' USING ERRCODE = '42501';
    END IF;
    RETURN NEW;
END $$;
DROP TRIGGER immutable_resource_scope ON hot.schedule;
CREATE TRIGGER immutable_resource_scope BEFORE UPDATE ON hot.schedule
    FOR EACH ROW WHEN (OLD.build_id IS DISTINCT FROM NEW.build_id)
    EXECUTE FUNCTION hot.prevent_schedule_scope_reassignment();

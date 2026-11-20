-- A deployment carries its project's dynamic schedules to the new build, so a
-- schedule may move between builds of one project. Any other change of
-- build_id is still refused, as PostgreSQL's guard refuses it.
DROP TRIGGER schedule_immutable_resource_scope;
CREATE TRIGGER schedule_immutable_resource_scope BEFORE UPDATE OF build_id ON schedule
WHEN NEW.build_id IS NOT OLD.build_id
    AND ((SELECT project_id FROM build WHERE build_id = NEW.build_id) IS NULL
        OR (SELECT project_id FROM build WHERE build_id = NEW.build_id)
            IS NOT (SELECT project_id FROM build WHERE build_id = OLD.build_id))
BEGIN SELECT RAISE(ABORT, 'Resource ownership is immutable'); END;

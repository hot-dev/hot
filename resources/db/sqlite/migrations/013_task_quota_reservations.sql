ALTER TABLE task ADD COLUMN reserved_compute_units integer NOT NULL DEFAULT 0 CHECK (reserved_compute_units >= 0);
ALTER TABLE task ADD COLUMN reserved_duration_ms integer NOT NULL DEFAULT 0 CHECK (reserved_duration_ms >= 0);

CREATE TABLE usage_task (
    env_id blob PRIMARY KEY NOT NULL, org_id blob NOT NULL,
    active_containers integer NOT NULL DEFAULT 0,
    reserved_compute_units integer NOT NULL DEFAULT 0,
    reserved_duration_ms integer NOT NULL DEFAULT 0,
    period_start text,
    compute_units integer NOT NULL DEFAULT 0,
    duration_ms integer NOT NULL DEFAULT 0
);
CREATE INDEX usage_task_org_idx ON usage_task(org_id);
CREATE VIEW task_usage AS
SELECT task_id,env_id,
    CASE WHEN active AND task_type='container' THEN 1 ELSE 0 END AS active_containers,
    CASE WHEN active THEN CASE WHEN reserved_compute_units>0 THEN reserved_compute_units
        WHEN task_type='container' THEN (MAX(timeout_ms,0)*32+999)/1000 ELSE 0 END ELSE 0 END AS reserved_compute_units,
    CASE WHEN active THEN CASE WHEN reserved_duration_ms>0 THEN reserved_duration_ms ELSE MAX(timeout_ms,0) END ELSE 0 END AS reserved_duration_ms,
    CASE WHEN NOT active AND task_type='container' THEN MAX(COALESCE(CASE WHEN length(ltrim(CAST(nested AS TEXT),'-')) BETWEEN 1 AND 19 AND ((CAST(nested AS TEXT) NOT GLOB '*[^0-9]*') OR (substr(CAST(nested AS TEXT),1,1)='-' AND substr(CAST(nested AS TEXT),2) NOT GLOB '*[^0-9]*')) THEN CASE WHEN length(ltrim(CAST(nested AS TEXT),'-'))<19 OR substr(ltrim(CAST(nested AS TEXT),'-'),1,1)='0' OR CAST(CAST(nested AS INTEGER) AS TEXT)=CAST(nested AS TEXT) THEN CAST(nested AS INTEGER) END END,CASE WHEN length(ltrim(CAST(direct AS TEXT),'-')) BETWEEN 1 AND 19 AND ((CAST(direct AS TEXT) NOT GLOB '*[^0-9]*') OR (substr(CAST(direct AS TEXT),1,1)='-' AND substr(CAST(direct AS TEXT),2) NOT GLOB '*[^0-9]*')) THEN CASE WHEN length(ltrim(CAST(direct AS TEXT),'-'))<19 OR substr(ltrim(CAST(direct AS TEXT),'-'),1,1)='0' OR CAST(CAST(direct AS INTEGER) AS TEXT)=CAST(direct AS TEXT) THEN CAST(direct AS INTEGER) END END,
        CASE WHEN reserved_duration_ms>0 THEN (reserved_compute_units*MIN(MAX(COALESCE(duration_ms,0),0),reserved_duration_ms)+reserved_duration_ms-1)/reserved_duration_ms
        ELSE reserved_compute_units END,0),0) ELSE 0 END AS compute_units,
    CASE WHEN NOT active THEN MAX(COALESCE(duration_ms,0),0) ELSE 0 END AS duration_ms,
    COALESCE(stop_time,start_time,created_at) AS usage_at
FROM (SELECT t.*, t.task_status_id=2 OR (t.task_status_id=5 AND t.task_type='container' AND t.reserved_duration_ms>0) AS active,
    CASE WHEN json_type(CASE WHEN json_valid(t.result) THEN t.result END,'$."$val".err."compute-units"') IN ('integer','text') THEN json_extract(CASE WHEN json_valid(t.result) THEN t.result END,'$."$val".err."compute-units"') END AS nested,
    CASE WHEN json_type(CASE WHEN json_valid(t.result) THEN t.result END,'$."compute-units"') IN ('integer','text') THEN json_extract(CASE WHEN json_valid(t.result) THEN t.result END,'$."compute-units"') END AS direct FROM task t);
CREATE TRIGGER count_task_usage_insert AFTER INSERT ON task
BEGIN
    INSERT INTO usage_task(env_id,org_id,active_containers,reserved_compute_units,reserved_duration_ms,compute_units,duration_ms)
    SELECT u.env_id,e.org_id,1*u.active_containers,1*u.reserved_compute_units,1*u.reserved_duration_ms,CASE WHEN julianday(u.usage_at)>=julianday(p.period_start) THEN 1*u.compute_units ELSE 0 END,CASE WHEN julianday(u.usage_at)>=julianday(p.period_start) THEN 1*u.duration_ms ELSE 0 END
    FROM task_usage u JOIN env e ON e.env_id=u.env_id LEFT JOIN usage_task p ON p.env_id=u.env_id WHERE u.task_id=NEW.task_id
    ON CONFLICT(env_id) DO UPDATE SET active_containers=active_containers+excluded.active_containers,reserved_compute_units=reserved_compute_units+excluded.reserved_compute_units,reserved_duration_ms=reserved_duration_ms+excluded.reserved_duration_ms,compute_units=compute_units+excluded.compute_units,duration_ms=duration_ms+excluded.duration_ms;
END;
CREATE TRIGGER count_task_usage_delete BEFORE DELETE ON task
BEGIN
    INSERT INTO usage_task(env_id,org_id,active_containers,reserved_compute_units,reserved_duration_ms,compute_units,duration_ms)
    SELECT u.env_id,e.org_id,-1*u.active_containers,-1*u.reserved_compute_units,-1*u.reserved_duration_ms,CASE WHEN julianday(u.usage_at)>=julianday(p.period_start) THEN -1*u.compute_units ELSE 0 END,CASE WHEN julianday(u.usage_at)>=julianday(p.period_start) THEN -1*u.duration_ms ELSE 0 END
    FROM task_usage u JOIN env e ON e.env_id=u.env_id LEFT JOIN usage_task p ON p.env_id=u.env_id WHERE u.task_id=OLD.task_id
    ON CONFLICT(env_id) DO UPDATE SET active_containers=active_containers+excluded.active_containers,reserved_compute_units=reserved_compute_units+excluded.reserved_compute_units,reserved_duration_ms=reserved_duration_ms+excluded.reserved_duration_ms,compute_units=compute_units+excluded.compute_units,duration_ms=duration_ms+excluded.duration_ms;
END;
CREATE TRIGGER count_task_usage_update_old BEFORE UPDATE OF task_status_id, task_type, timeout_ms, reserved_compute_units, reserved_duration_ms, result, duration_ms, stop_time, start_time, created_at ON task WHEN NEW.task_status_id IS NOT OLD.task_status_id OR NEW.task_type IS NOT OLD.task_type OR NEW.timeout_ms IS NOT OLD.timeout_ms OR NEW.reserved_compute_units IS NOT OLD.reserved_compute_units OR NEW.reserved_duration_ms IS NOT OLD.reserved_duration_ms OR NEW.result IS NOT OLD.result OR NEW.duration_ms IS NOT OLD.duration_ms OR NEW.stop_time IS NOT OLD.stop_time OR NEW.start_time IS NOT OLD.start_time OR NEW.created_at IS NOT OLD.created_at
BEGIN
    INSERT INTO usage_task(env_id,org_id,active_containers,reserved_compute_units,reserved_duration_ms,compute_units,duration_ms)
    SELECT u.env_id,e.org_id,-1*u.active_containers,-1*u.reserved_compute_units,-1*u.reserved_duration_ms,CASE WHEN julianday(u.usage_at)>=julianday(p.period_start) THEN -1*u.compute_units ELSE 0 END,CASE WHEN julianday(u.usage_at)>=julianday(p.period_start) THEN -1*u.duration_ms ELSE 0 END
    FROM task_usage u JOIN env e ON e.env_id=u.env_id LEFT JOIN usage_task p ON p.env_id=u.env_id WHERE u.task_id=OLD.task_id
    ON CONFLICT(env_id) DO UPDATE SET active_containers=active_containers+excluded.active_containers,reserved_compute_units=reserved_compute_units+excluded.reserved_compute_units,reserved_duration_ms=reserved_duration_ms+excluded.reserved_duration_ms,compute_units=compute_units+excluded.compute_units,duration_ms=duration_ms+excluded.duration_ms;
END;
CREATE TRIGGER count_task_usage_update_new AFTER UPDATE OF task_status_id, task_type, timeout_ms, reserved_compute_units, reserved_duration_ms, result, duration_ms, stop_time, start_time, created_at ON task WHEN NEW.task_status_id IS NOT OLD.task_status_id OR NEW.task_type IS NOT OLD.task_type OR NEW.timeout_ms IS NOT OLD.timeout_ms OR NEW.reserved_compute_units IS NOT OLD.reserved_compute_units OR NEW.reserved_duration_ms IS NOT OLD.reserved_duration_ms OR NEW.result IS NOT OLD.result OR NEW.duration_ms IS NOT OLD.duration_ms OR NEW.stop_time IS NOT OLD.stop_time OR NEW.start_time IS NOT OLD.start_time OR NEW.created_at IS NOT OLD.created_at
BEGIN
    INSERT INTO usage_task(env_id,org_id,active_containers,reserved_compute_units,reserved_duration_ms,compute_units,duration_ms)
    SELECT u.env_id,e.org_id,1*u.active_containers,1*u.reserved_compute_units,1*u.reserved_duration_ms,CASE WHEN julianday(u.usage_at)>=julianday(p.period_start) THEN 1*u.compute_units ELSE 0 END,CASE WHEN julianday(u.usage_at)>=julianday(p.period_start) THEN 1*u.duration_ms ELSE 0 END
    FROM task_usage u JOIN env e ON e.env_id=u.env_id LEFT JOIN usage_task p ON p.env_id=u.env_id WHERE u.task_id=NEW.task_id
    ON CONFLICT(env_id) DO UPDATE SET active_containers=active_containers+excluded.active_containers,reserved_compute_units=reserved_compute_units+excluded.reserved_compute_units,reserved_duration_ms=reserved_duration_ms+excluded.reserved_duration_ms,compute_units=compute_units+excluded.compute_units,duration_ms=duration_ms+excluded.duration_ms;
END;
INSERT INTO usage_task(env_id,org_id,active_containers,reserved_compute_units,reserved_duration_ms)
SELECT e.env_id,e.org_id,COALESCE(SUM(u.active_containers),0),COALESCE(SUM(u.reserved_compute_units),0),COALESCE(SUM(u.reserved_duration_ms),0)
FROM env e LEFT JOIN task_usage u ON u.env_id=e.env_id GROUP BY e.env_id,e.org_id;
CREATE TRIGGER immutable_usage_task BEFORE UPDATE OF env_id,org_id ON usage_task
WHEN NEW.env_id IS NOT OLD.env_id OR NEW.org_id IS NOT OLD.org_id
BEGIN SELECT RAISE(ABORT,'Usage ownership is immutable'); END;
CREATE TRIGGER immutable_task_usage_owner BEFORE UPDATE OF env_id ON task
WHEN NEW.env_id IS NOT OLD.env_id
BEGIN SELECT RAISE(ABORT,'Task ownership is immutable'); END;

-- A running task is its durable account quota reservation. Completion changes
-- status and writes actual metering in one UPDATE, releasing unused capacity.
ALTER TABLE task ADD COLUMN reserved_compute_units bigint NOT NULL DEFAULT 0 CHECK (reserved_compute_units >= 0);
ALTER TABLE task ADD COLUMN reserved_duration_ms bigint NOT NULL DEFAULT 0 CHECK (reserved_duration_ms >= 0);

-- Always-on per-environment counters keep quota admission independent of history.
CREATE TABLE hot.usage_task (
    env_id uuid PRIMARY KEY, org_id uuid NOT NULL,
    active_containers bigint NOT NULL DEFAULT 0,
    reserved_compute_units bigint NOT NULL DEFAULT 0,
    reserved_duration_ms bigint NOT NULL DEFAULT 0,
    period_start timestamptz,
    compute_units bigint NOT NULL DEFAULT 0,
    duration_ms bigint NOT NULL DEFAULT 0
);
CREATE INDEX usage_task_org_idx ON hot.usage_task(org_id);
CREATE FUNCTION hot.task_usage(t hot.task) RETURNS TABLE (
    active_containers bigint, reserved_compute_units bigint,
    reserved_duration_ms bigint, compute_units bigint, duration_ms bigint,
    usage_at timestamptz
) LANGUAGE sql STABLE PARALLEL SAFE AS $$
    SELECT
        CASE WHEN active AND t.task_type='container' THEN 1 ELSE 0 END::bigint,
        CASE WHEN active THEN CASE WHEN t.reserved_compute_units>0 THEN t.reserved_compute_units
            WHEN t.task_type='container' THEN (GREATEST(t.timeout_ms,0)*32+999)/1000 ELSE 0 END ELSE 0 END,
        CASE WHEN active THEN CASE WHEN t.reserved_duration_ms>0 THEN t.reserved_duration_ms ELSE GREATEST(t.timeout_ms,0) END ELSE 0 END,
        CASE WHEN NOT active AND t.task_type='container' THEN GREATEST(COALESCE(
            CASE WHEN nested ~ '^-?[0-9]{1,18}$' THEN nested::bigint END,
            CASE WHEN direct ~ '^-?[0-9]{1,18}$' THEN direct::bigint END,
            CASE WHEN t.reserved_duration_ms>0 THEN
                floor((t.reserved_compute_units::numeric*LEAST(GREATEST(COALESCE(t.duration_ms,0),0),t.reserved_duration_ms)+t.reserved_duration_ms-1)/t.reserved_duration_ms)::bigint
                ELSE t.reserved_compute_units END,0),0) ELSE 0 END,
        CASE WHEN NOT active THEN GREATEST(COALESCE(t.duration_ms,0),0) ELSE 0 END,
        COALESCE(t.stop_time,t.start_time,t.created_at)
    FROM (SELECT t.task_status_id=2 OR (t.task_status_id=5 AND t.task_type='container' AND t.reserved_duration_ms>0) AS active,
        t.result->'$val'->'err'->>'compute-units' AS nested, t.result->>'compute-units' AS direct) p
$$;
CREATE FUNCTION hot.count_task_usage() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE n record; o record; owner_env uuid; owner_org uuid;
BEGIN
    SELECT * INTO n FROM hot.task_usage(CASE WHEN TG_OP='DELETE' THEN OLD ELSE NEW END);
    SELECT * INTO o FROM hot.task_usage(CASE WHEN TG_OP='INSERT' THEN NEW ELSE OLD END);
    owner_env:=CASE WHEN TG_OP='DELETE' THEN OLD.env_id ELSE NEW.env_id END;
    SELECT org_id INTO STRICT owner_org FROM hot.env WHERE env_id=owner_env;
    INSERT INTO hot.usage_task AS u (env_id,org_id,active_containers,reserved_compute_units,reserved_duration_ms)
    VALUES (owner_env,owner_org,
        CASE WHEN TG_OP='DELETE' THEN -o.active_containers WHEN TG_OP='INSERT' THEN n.active_containers ELSE n.active_containers-o.active_containers END,
        CASE WHEN TG_OP='DELETE' THEN -o.reserved_compute_units WHEN TG_OP='INSERT' THEN n.reserved_compute_units ELSE n.reserved_compute_units-o.reserved_compute_units END,
        CASE WHEN TG_OP='DELETE' THEN -o.reserved_duration_ms WHEN TG_OP='INSERT' THEN n.reserved_duration_ms ELSE n.reserved_duration_ms-o.reserved_duration_ms END)
    ON CONFLICT (env_id) DO UPDATE SET
        active_containers=u.active_containers+EXCLUDED.active_containers,
        reserved_compute_units=u.reserved_compute_units+EXCLUDED.reserved_compute_units,
        reserved_duration_ms=u.reserved_duration_ms+EXCLUDED.reserved_duration_ms,
        compute_units=u.compute_units
            + CASE WHEN TG_OP<>'DELETE' AND n.usage_at>=u.period_start THEN n.compute_units ELSE 0 END
            - CASE WHEN TG_OP<>'INSERT' AND o.usage_at>=u.period_start THEN o.compute_units ELSE 0 END,
        duration_ms=u.duration_ms
            + CASE WHEN TG_OP<>'DELETE' AND n.usage_at>=u.period_start THEN n.duration_ms ELSE 0 END
            - CASE WHEN TG_OP<>'INSERT' AND o.usage_at>=u.period_start THEN o.duration_ms ELSE 0 END;
    RETURN NULL;
END $$;
CREATE TRIGGER count_task_usage_insert AFTER INSERT ON hot.task FOR EACH ROW EXECUTE FUNCTION hot.count_task_usage();
CREATE TRIGGER count_task_usage_delete AFTER DELETE ON hot.task FOR EACH ROW EXECUTE FUNCTION hot.count_task_usage();
CREATE TRIGGER count_task_usage_update AFTER UPDATE OF task_status_id, task_type, timeout_ms, reserved_compute_units, reserved_duration_ms, result, duration_ms, stop_time, start_time, created_at ON hot.task
    FOR EACH ROW WHEN (ROW(OLD.task_status_id, OLD.task_type, OLD.timeout_ms, OLD.reserved_compute_units, OLD.reserved_duration_ms, OLD.result, OLD.duration_ms, OLD.stop_time, OLD.start_time, OLD.created_at) IS DISTINCT FROM ROW(NEW.task_status_id, NEW.task_type, NEW.timeout_ms, NEW.reserved_compute_units, NEW.reserved_duration_ms, NEW.result, NEW.duration_ms, NEW.stop_time, NEW.start_time, NEW.created_at))
    EXECUTE FUNCTION hot.count_task_usage();
DO $$
DECLARE m text:=COALESCE(current_setting('hot.scope_mode',true),'');
    op text:=COALESCE(current_setting('hot.scope_operation',true),'');
    org text:=COALESCE(current_setting('hot.scope_org',true),'');
    env text:=COALESCE(current_setting('hot.scope_env',true),'');
BEGIN
    PERFORM hot.apply_scope('maintenance','','','[]','[]','scheduler','');
    INSERT INTO hot.usage_task(env_id,org_id,active_containers,reserved_compute_units,reserved_duration_ms)
    SELECT e.env_id,e.org_id,COALESCE(SUM(u.active_containers),0),COALESCE(SUM(u.reserved_compute_units),0),COALESCE(SUM(u.reserved_duration_ms),0)
    FROM hot.env e LEFT JOIN hot.task t ON t.env_id=e.env_id LEFT JOIN LATERAL hot.task_usage(t) u ON t.task_id IS NOT NULL
    GROUP BY e.env_id,e.org_id;
    PERFORM hot.apply_scope(m,org,env,'[]','[]',op,'');
END $$;
ALTER TABLE hot.usage_task ENABLE ROW LEVEL SECURITY;
ALTER TABLE hot.usage_task FORCE ROW LEVEL SECURITY;
CREATE POLICY hot_scope_read ON hot.usage_task FOR SELECT USING ((hot.scope_env_allows(env_id) AND hot.scope_org_allows(org_id)) OR (hot.scope_maintenance('queue') OR hot.scope_maintenance('scheduler') OR hot.scope_maintenance('retention') OR hot.scope_maintenance('alerts')) OR hot.scope_billing_org(org_id));
CREATE POLICY hot_scope_insert ON hot.usage_task FOR INSERT WITH CHECK ((hot.scope_env_writes(env_id) AND hot.scope_org_allows(org_id)) OR (hot.scope_maintenance('queue') OR hot.scope_maintenance('scheduler') OR hot.scope_maintenance('retention') OR hot.scope_maintenance('alerts')) OR hot.scope_billing_org(org_id));
CREATE POLICY hot_scope_update ON hot.usage_task FOR UPDATE USING ((hot.scope_env_writes(env_id) AND hot.scope_org_allows(org_id)) OR (hot.scope_maintenance('queue') OR hot.scope_maintenance('scheduler') OR hot.scope_maintenance('retention') OR hot.scope_maintenance('alerts')) OR hot.scope_billing_org(org_id)) WITH CHECK ((hot.scope_env_writes(env_id) AND hot.scope_org_allows(org_id)) OR (hot.scope_maintenance('queue') OR hot.scope_maintenance('scheduler') OR hot.scope_maintenance('retention') OR hot.scope_maintenance('alerts')) OR hot.scope_billing_org(org_id));
CREATE TRIGGER immutable_resource_scope BEFORE UPDATE ON hot.usage_task
    FOR EACH ROW EXECUTE FUNCTION hot.prevent_scope_reassignment('env_id', 'org_id');

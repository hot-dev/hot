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
-- Period initialization recounts only tasks whose usage time is in the period.
CREATE INDEX task_env_usage_at_idx ON hot.task(env_id,(COALESCE(stop_time,start_time,created_at)));
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
            CASE WHEN nested ~ '^-?[0-9]{1,19}$' THEN CASE WHEN nested::numeric BETWEEN -9223372036854775808::numeric AND 9223372036854775807::numeric THEN nested::bigint END END,
            CASE WHEN direct ~ '^-?[0-9]{1,19}$' THEN CASE WHEN direct::numeric BETWEEN -9223372036854775808::numeric AND 9223372036854775807::numeric THEN direct::bigint END END,
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
    -- Queued inserts and other writes that change no usage leave the counter
    -- row alone, so concurrent enqueues in one environment do not serialize.
    IF (CASE TG_OP
        WHEN 'INSERT' THEN (n.active_containers,n.reserved_compute_units,n.reserved_duration_ms,n.compute_units,n.duration_ms)=(0,0,0,0,0)
        WHEN 'DELETE' THEN (o.active_containers,o.reserved_compute_units,o.reserved_duration_ms,o.compute_units,o.duration_ms)=(0,0,0,0,0)
        ELSE (n.active_containers,n.reserved_compute_units,n.reserved_duration_ms)=(o.active_containers,o.reserved_compute_units,o.reserved_duration_ms)
            AND ((n.compute_units,n.duration_ms,o.compute_units,o.duration_ms)=(0,0,0,0)
                OR (n.compute_units,n.duration_ms,n.usage_at) IS NOT DISTINCT FROM (o.compute_units,o.duration_ms,o.usage_at))
    END) THEN
        RETURN NULL;
    END IF;
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
            - CASE WHEN TG_OP<>'INSERT' AND o.usage_at>=u.period_start THEN o.compute_units ELSE 0 END
            + CASE WHEN TG_OP<>'DELETE' AND n.usage_at>=u.period_start THEN n.compute_units ELSE 0 END,
        duration_ms=u.duration_ms
            - CASE WHEN TG_OP<>'INSERT' AND o.usage_at>=u.period_start THEN o.duration_ms ELSE 0 END
            + CASE WHEN TG_OP<>'DELETE' AND n.usage_at>=u.period_start THEN n.duration_ms ELSE 0 END;
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
    FOR EACH ROW WHEN ((OLD.env_id, OLD.org_id) IS DISTINCT FROM (NEW.env_id, NEW.org_id))
    EXECUTE FUNCTION hot.prevent_scope_reassignment('env_id', 'org_id');

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

CREATE TABLE hot.usage_run (
    root_org_id uuid NOT NULL, period_start timestamptz NOT NULL,
    admitted bigint NOT NULL, PRIMARY KEY(root_org_id,period_start)
);
CREATE FUNCTION hot.count_run_admission() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    UPDATE hot.usage_run SET admitted=admitted+1
    WHERE root_org_id=NEW.root_org_id AND period_start<=NEW.admitted_at;
    RETURN NULL;
END $$;
CREATE TRIGGER count_run_admission AFTER INSERT ON hot.run_admission
    FOR EACH ROW EXECUTE FUNCTION hot.count_run_admission();
ALTER TABLE hot.usage_run ENABLE ROW LEVEL SECURITY;
ALTER TABLE hot.usage_run FORCE ROW LEVEL SECURITY;
CREATE POLICY hot_scope_read ON hot.usage_run FOR SELECT USING (hot.scope_maintenance('billing') AND root_org_id=hot.scope_org_id());
CREATE POLICY hot_scope_insert ON hot.usage_run FOR INSERT WITH CHECK (hot.scope_maintenance('billing') AND root_org_id=hot.scope_org_id());
CREATE POLICY hot_scope_update ON hot.usage_run FOR UPDATE USING (hot.scope_maintenance('billing') AND root_org_id=hot.scope_org_id()) WITH CHECK (hot.scope_maintenance('billing') AND root_org_id=hot.scope_org_id());
CREATE POLICY hot_scope_delete ON hot.usage_run FOR DELETE USING (hot.scope_maintenance('billing') AND root_org_id=hot.scope_org_id());
CREATE TRIGGER immutable_resource_scope BEFORE UPDATE ON hot.usage_run
    FOR EACH ROW WHEN ((OLD.root_org_id, OLD.period_start) IS DISTINCT FROM (NEW.root_org_id, NEW.period_start))
    EXECUTE FUNCTION hot.prevent_scope_reassignment('root_org_id','period_start');

-- Storage counters start from store entries and the trace sizes 013
-- reconciled. The migration owner may be NOSUPERUSER NOBYPASSRLS under FORCE RLS.
CREATE TABLE hot.usage_storage (
    env_id uuid NOT NULL, org_id uuid NOT NULL, slot smallint NOT NULL CHECK(slot BETWEEN 0 AND 15),
    store_bytes bigint NOT NULL DEFAULT 0, call_bytes bigint NOT NULL DEFAULT 0,
    PRIMARY KEY(env_id,slot)
);
CREATE INDEX usage_storage_org_idx ON hot.usage_storage(org_id);
CREATE FUNCTION hot.count_call_bytes() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE delta bigint; owner_org uuid;
BEGIN
    delta:=GREATEST(NEW.size,0)-CASE WHEN TG_OP='UPDATE' THEN GREATEST(OLD.size,0) ELSE 0 END;
    IF delta=0 THEN RETURN NULL; END IF;
    -- Trace batches update the same slot once per write: a plain UPDATE avoids
    -- speculative insertion, and ownership is only needed to create the slot.
    UPDATE hot.usage_storage SET call_bytes=call_bytes+delta WHERE env_id=NEW.env_id AND slot=pg_backend_pid()%16;
    IF FOUND THEN RETURN NULL; END IF;
    SELECT org_id INTO STRICT owner_org FROM hot.env WHERE env_id=NEW.env_id;
    INSERT INTO hot.usage_storage AS u(env_id,org_id,slot,call_bytes) VALUES(NEW.env_id,owner_org,pg_backend_pid()%16,delta)
    ON CONFLICT(env_id,slot) DO UPDATE SET call_bytes=u.call_bytes+EXCLUDED.call_bytes;
    RETURN NULL;
END $$;
CREATE TRIGGER count_call_bytes_insert AFTER INSERT ON hot.call FOR EACH ROW EXECUTE FUNCTION hot.count_call_bytes();
CREATE TRIGGER count_call_bytes_update AFTER UPDATE OF size ON hot.call
    FOR EACH ROW WHEN(OLD.size IS DISTINCT FROM NEW.size) EXECUTE FUNCTION hot.count_call_bytes();
CREATE FUNCTION hot.uncount_call_bytes() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    INSERT INTO hot.usage_storage AS u(env_id,org_id,slot,call_bytes)
    SELECT d.env_id,e.org_id,pg_backend_pid()%16,-SUM(GREATEST(d.size,0))
    FROM deleted d JOIN hot.env e ON e.env_id=d.env_id GROUP BY d.env_id,e.org_id HAVING SUM(GREATEST(d.size,0))<>0 ORDER BY d.env_id
    ON CONFLICT(env_id,slot) DO UPDATE SET call_bytes=u.call_bytes+EXCLUDED.call_bytes;
    RETURN NULL;
END $$;
CREATE TRIGGER uncount_call_bytes AFTER DELETE ON hot.call
    REFERENCING OLD TABLE AS deleted FOR EACH STATEMENT EXECUTE FUNCTION hot.uncount_call_bytes();
CREATE FUNCTION hot.count_store_bytes() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE delta bigint; owner_org uuid;
BEGIN
    delta:=GREATEST(NEW.size,0)-CASE WHEN TG_OP='UPDATE' THEN GREATEST(OLD.size,0) ELSE 0 END;
    IF delta=0 THEN RETURN NULL; END IF;
    UPDATE hot.usage_storage SET store_bytes=store_bytes+delta WHERE env_id=NEW.env_id AND slot=pg_backend_pid()%16;
    IF FOUND THEN RETURN NULL; END IF;
    owner_org:=NEW.org_id;
    INSERT INTO hot.usage_storage AS u(env_id,org_id,slot,store_bytes) VALUES(NEW.env_id,owner_org,pg_backend_pid()%16,delta)
    ON CONFLICT(env_id,slot) DO UPDATE SET store_bytes=u.store_bytes+EXCLUDED.store_bytes;
    RETURN NULL;
END $$;
CREATE TRIGGER count_store_bytes_insert AFTER INSERT ON hot.store_map_entry FOR EACH ROW EXECUTE FUNCTION hot.count_store_bytes();
CREATE TRIGGER count_store_bytes_update AFTER UPDATE OF size ON hot.store_map_entry
    FOR EACH ROW WHEN(OLD.size IS DISTINCT FROM NEW.size) EXECUTE FUNCTION hot.count_store_bytes();
CREATE FUNCTION hot.uncount_store_bytes() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    INSERT INTO hot.usage_storage AS u(env_id,org_id,slot,store_bytes)
    SELECT d.env_id,d.org_id,pg_backend_pid()%16,-SUM(GREATEST(d.size,0))
    FROM deleted d GROUP BY d.env_id,d.org_id HAVING SUM(GREATEST(d.size,0))<>0 ORDER BY d.env_id
    ON CONFLICT(env_id,slot) DO UPDATE SET store_bytes=u.store_bytes+EXCLUDED.store_bytes;
    RETURN NULL;
END $$;
CREATE TRIGGER uncount_store_bytes AFTER DELETE ON hot.store_map_entry
    REFERENCING OLD TABLE AS deleted FOR EACH STATEMENT EXECUTE FUNCTION hot.uncount_store_bytes();
DO $$
DECLARE m text:=COALESCE(current_setting('hot.scope_mode',true),''); op text:=COALESCE(current_setting('hot.scope_operation',true),'');
    org text:=COALESCE(current_setting('hot.scope_org',true),''); env text:=COALESCE(current_setting('hot.scope_env',true),''); roots uuid[]; root_id uuid;
BEGIN
    PERFORM hot.apply_scope('maintenance','','','[]','[]','scheduler','');
    -- Store rows require account billing visibility, including retained orphans.
    SELECT COALESCE(array_agg(org_id),ARRAY[]::uuid[]) INTO roots FROM hot.org WHERE parent_org_id IS NULL;
    FOREACH root_id IN ARRAY roots LOOP
        PERFORM hot.apply_scope('maintenance',root_id::text,'','[]','[]','billing','');
-- Filter by account explicitly: a superuser or BYPASSRLS migration role sees
-- every account in every pass, and the filter lets each pass use indexes.
INSERT INTO hot.usage_storage(env_id,org_id,slot,store_bytes,call_bytes)
SELECT x.env_id,x.org_id,0,SUM(x.store_bytes),SUM(x.call_bytes) FROM (
    SELECT s.env_id,s.org_id,CASE WHEN s.size<0 THEN 0 ELSE s.size END AS store_bytes,0 AS call_bytes FROM hot.store_map_entry s
    WHERE s.org_id IN (SELECT o.org_id FROM hot.org o WHERE o.org_id=root_id OR o.parent_org_id=root_id)
    UNION ALL SELECT c.env_id,e.org_id,0,CASE WHEN c.size<0 THEN 0 ELSE c.size END FROM hot.call c JOIN hot.env e ON e.env_id=c.env_id
    WHERE e.org_id IN (SELECT o.org_id FROM hot.org o WHERE o.org_id=root_id OR o.parent_org_id=root_id)
) x GROUP BY x.env_id,x.org_id;
    END LOOP;
    PERFORM hot.apply_scope(m,org,env,'[]','[]',op,'');
END $$;
ALTER TABLE hot.usage_storage ENABLE ROW LEVEL SECURITY;
ALTER TABLE hot.usage_storage FORCE ROW LEVEL SECURITY;
CREATE POLICY hot_scope_read ON hot.usage_storage FOR SELECT USING ((hot.scope_env_allows(env_id) AND hot.scope_org_allows(org_id)) OR (hot.scope_maintenance('queue') OR hot.scope_maintenance('scheduler') OR hot.scope_maintenance('retention') OR hot.scope_maintenance('alerts')) OR hot.scope_billing_org(org_id));
CREATE POLICY hot_scope_insert ON hot.usage_storage FOR INSERT WITH CHECK ((hot.scope_env_writes(env_id) AND hot.scope_org_allows(org_id)) OR (hot.scope_maintenance('queue') OR hot.scope_maintenance('scheduler') OR hot.scope_maintenance('retention') OR hot.scope_maintenance('alerts')) OR hot.scope_billing_org(org_id));
CREATE POLICY hot_scope_update ON hot.usage_storage FOR UPDATE USING ((hot.scope_env_writes(env_id) AND hot.scope_org_allows(org_id)) OR (hot.scope_maintenance('queue') OR hot.scope_maintenance('scheduler') OR hot.scope_maintenance('retention') OR hot.scope_maintenance('alerts')) OR hot.scope_billing_org(org_id)) WITH CHECK ((hot.scope_env_writes(env_id) AND hot.scope_org_allows(org_id)) OR (hot.scope_maintenance('queue') OR hot.scope_maintenance('scheduler') OR hot.scope_maintenance('retention') OR hot.scope_maintenance('alerts')) OR hot.scope_billing_org(org_id));
CREATE TRIGGER immutable_resource_scope BEFORE UPDATE ON hot.usage_storage
    FOR EACH ROW WHEN ((OLD.env_id, OLD.org_id, OLD.slot) IS DISTINCT FROM (NEW.env_id, NEW.org_id, NEW.slot))
    EXECUTE FUNCTION hot.prevent_scope_reassignment('env_id', 'org_id', 'slot');

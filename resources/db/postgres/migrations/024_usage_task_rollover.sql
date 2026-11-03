-- Recounting a new billing period reads every task in the period, which on a
-- large account takes seconds. Task writers update the counter row, so the
-- recount must not hold that row while it reads. While one runs, these columns
-- count the task changes its snapshot cannot see, against the period it is
-- moving to. The row stays on its old period until the recount applies its
-- total, so usage readers never see a partial count. Both statements here are
-- instant: the columns need no table rewrite and `usage_task` is small.
ALTER TABLE hot.usage_task
    ADD COLUMN rollover_period timestamptz,
    ADD COLUMN rollover_compute_units bigint NOT NULL DEFAULT 0,
    ADD COLUMN rollover_duration_ms bigint NOT NULL DEFAULT 0;

CREATE OR REPLACE FUNCTION hot.count_task_usage() RETURNS trigger LANGUAGE plpgsql AS $$
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
            + CASE WHEN TG_OP<>'DELETE' AND n.usage_at>=u.period_start THEN n.duration_ms ELSE 0 END,
        rollover_compute_units=u.rollover_compute_units
            - CASE WHEN TG_OP<>'INSERT' AND o.usage_at>=u.rollover_period THEN o.compute_units ELSE 0 END
            + CASE WHEN TG_OP<>'DELETE' AND n.usage_at>=u.rollover_period THEN n.compute_units ELSE 0 END,
        rollover_duration_ms=u.rollover_duration_ms
            - CASE WHEN TG_OP<>'INSERT' AND o.usage_at>=u.rollover_period THEN o.duration_ms ELSE 0 END
            + CASE WHEN TG_OP<>'DELETE' AND n.usage_at>=u.rollover_period THEN n.duration_ms ELSE 0 END;
    RETURN NULL;
END $$;

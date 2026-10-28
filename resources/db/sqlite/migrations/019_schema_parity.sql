-- Reconcile the SQLite schema with PostgreSQL. Both backends now share their
-- migration numbers, and from 019 on a fresh database of either kind has the
-- same tables, columns, keys, indexes and ownership guards, except where the
-- engines genuinely differ (RLS, column types, generated defaults). The schema
-- parity test holds the two chains together from here.

-- Refuse an upgrade over rows the new keys would reject. Repair the named rows
-- and retry instead of letting the copy fail on an anonymous constraint.
CREATE TEMP TABLE schema_parity_preflight (
    verification_token integer NOT NULL DEFAULT 1
        CONSTRAINT unique_alert_destination_verification_token CHECK (verification_token = 1),
    task_run integer NOT NULL DEFAULT 1
        CONSTRAINT task_run_exists CHECK (task_run = 1),
    call_run integer NOT NULL DEFAULT 1
        CONSTRAINT call_run_environment_exists CHECK (call_run = 1),
    alert_channel_name integer NOT NULL DEFAULT 1
        CONSTRAINT unique_alert_channel_name CHECK (alert_channel_name = 1)
);
INSERT INTO schema_parity_preflight(verification_token)
SELECT 0 FROM alert_destination WHERE verification_token IS NOT NULL
GROUP BY verification_token HAVING count(*) > 1 LIMIT 1;
INSERT INTO schema_parity_preflight(task_run)
SELECT 0 FROM task t LEFT JOIN run r ON r.run_id = t.run_id
WHERE t.run_id IS NOT NULL AND r.run_id IS NULL LIMIT 1;
INSERT INTO schema_parity_preflight(call_run)
SELECT 0 FROM call c LEFT JOIN run r ON r.run_id = c.run_id AND r.env_id = c.env_id
WHERE r.run_id IS NULL LIMIT 1;
INSERT INTO schema_parity_preflight(alert_channel_name)
SELECT 0 FROM alert_channel WHERE org_id IS NULL
GROUP BY name HAVING count(*) > 1 LIMIT 1;
INSERT INTO schema_parity_preflight(alert_channel_name)
SELECT 0 FROM alert_channel WHERE org_id IS NOT NULL AND env_id IS NOT NULL
GROUP BY org_id, env_id, name HAVING count(*) > 1 LIMIT 1;
DROP TABLE schema_parity_preflight;

-- is_personal predates organization types and nothing reads it.
DROP INDEX idx_org_is_personal;
ALTER TABLE org DROP COLUMN is_personal;

CREATE UNIQUE INDEX alert_destination_verification_token_key ON alert_destination(verification_token);

-- Partial-index predicates and the env-less sentinel follow PostgreSQL.
DROP INDEX idx_alert_channel_name_system;
CREATE UNIQUE INDEX idx_alert_channel_name_system ON alert_channel(name) WHERE org_id IS NULL;
DROP INDEX idx_alert_channel_name_env;
CREATE UNIQUE INDEX idx_alert_channel_name_env ON alert_channel(org_id, env_id, name)
    WHERE org_id IS NOT NULL AND env_id IS NOT NULL;
DROP INDEX idx_email_queue_status;
CREATE INDEX idx_email_queue_status ON email_queue(status_id, created_at) WHERE status_id = 1;
DROP INDEX idx_file_upload_expires;
CREATE INDEX idx_file_upload_expires ON file_upload(expires_at)
    WHERE status IN ('pending', 'completing', 'aborting');
DROP INDEX idx_file_upload_status;
CREATE INDEX idx_file_upload_status ON file_upload(status) WHERE status IN ('pending', 'completing');
DROP INDEX idx_blob_object_content_unique;
CREATE UNIQUE INDEX idx_blob_object_content_unique
    ON blob_object(org_id, COALESCE(env_id, X'00000000000000000000000000000000'), hash_alg, hash);
DROP INDEX idx_blob_ref_source_object_unique;
CREATE UNIQUE INDEX idx_blob_ref_source_object_unique
    ON blob_ref(org_id, COALESCE(env_id, X'00000000000000000000000000000000'), source_kind, COALESCE(source_id, ''), blob_object_id);

CREATE INDEX idx_agent_build_env ON agent(build_id, env_id);
CREATE INDEX idx_workflow_build_env ON workflow(build_id, env_id);
CREATE INDEX idx_run_status_id ON run(status_id);
-- The parent key of call's (run_id, env_id) reference.
CREATE UNIQUE INDEX run_env_ownership_idx ON run(run_id, env_id);

-- SQLite cannot alter a foreign key, so the three tables whose references
-- differ are rebuilt. Each copy keeps every row and column; objects that name
-- a rebuilt table from outside it are dropped first and restored after.

-- A cascade would let an environment delete and recreate a delegated API key
-- without its issuer, laundering the management ceiling and revocation chain.
DROP TRIGGER delegated_api_key_no_delete;
CREATE TABLE api_key_management_issuer_new (
    api_key_id blob PRIMARY KEY NOT NULL REFERENCES api_key(api_key_id) ON DELETE RESTRICT,
    management_key_id blob NOT NULL REFERENCES management_key(management_key_id),
    created_at text NOT NULL DEFAULT CURRENT_TIMESTAMP
);
INSERT INTO api_key_management_issuer_new(api_key_id, management_key_id, created_at)
SELECT api_key_id, management_key_id, created_at FROM api_key_management_issuer;
DROP TABLE api_key_management_issuer;
ALTER TABLE api_key_management_issuer_new RENAME TO api_key_management_issuer;
CREATE INDEX api_key_management_issuer_key_idx ON api_key_management_issuer(management_key_id);
CREATE TRIGGER immutable_api_key_management_issuer BEFORE UPDATE ON api_key_management_issuer
BEGIN SELECT RAISE(ABORT, 'API key management issuer is immutable'); END;
CREATE TRIGGER api_key_management_issuer_no_delete BEFORE DELETE ON api_key_management_issuer
BEGIN SELECT RAISE(ABORT, 'API key management issuer is immutable'); END;
CREATE TRIGGER delegated_api_key_no_delete BEFORE DELETE ON api_key
WHEN EXISTS (SELECT 1 FROM api_key_management_issuer WHERE api_key_id = OLD.api_key_id)
BEGIN SELECT RAISE(ABORT, 'Delegated API keys must be deactivated'); END;

-- A task's run reference is a foreign key, as in PostgreSQL.
DROP VIEW task_usage;
CREATE TABLE task_new (
    task_id blob primary key,
    env_id blob not null references env(env_id) on update restrict on delete restrict,
    stream_id blob not null references stream(stream_id) on update restrict on delete restrict,
    build_id blob not null references build(build_id) on update restrict on delete restrict,
    origin_run_id blob references run(run_id) on update restrict on delete restrict,
    task_status_id integer not null default 1 references task_status(task_status_id) on update restrict on delete restrict,
    function_name text not null,
    args text,
    options text,
    task_type text not null default 'code',
    start_time datetime,
    stop_time datetime,
    duration_ms integer,
    result text,
    info text,
    timeout_ms integer not null default 1800000,
    retry_attempt integer not null default 0,
    next_retry_at datetime,
    created_at datetime default current_timestamp,
    by_user_id blob references "user"(user_id) on update restrict on delete restrict,
    run_id blob references run(run_id) on update restrict on delete restrict,
    worker_id text,
    last_heartbeat_at datetime,
    container_id text,
    timing text,
    parent_task_id blob,
    infra_retry_count integer NOT NULL DEFAULT 0,
    reserved_compute_units integer NOT NULL DEFAULT 0 CHECK (reserved_compute_units >= 0),
    reserved_duration_ms integer NOT NULL DEFAULT 0 CHECK (reserved_duration_ms >= 0)
);
INSERT INTO task_new(task_id, env_id, stream_id, build_id, origin_run_id, task_status_id,
    function_name, args, options, task_type, start_time, stop_time, duration_ms, result, info,
    timeout_ms, retry_attempt, next_retry_at, created_at, by_user_id, run_id, worker_id,
    last_heartbeat_at, container_id, timing, parent_task_id, infra_retry_count,
    reserved_compute_units, reserved_duration_ms)
SELECT task_id, env_id, stream_id, build_id, origin_run_id, task_status_id,
    function_name, args, options, task_type, start_time, stop_time, duration_ms, result, info,
    timeout_ms, retry_attempt, next_retry_at, created_at, by_user_id, run_id, worker_id,
    last_heartbeat_at, container_id, timing, parent_task_id, infra_retry_count,
    reserved_compute_units, reserved_duration_ms FROM task;
DROP TABLE task;
ALTER TABLE task_new RENAME TO task;
CREATE INDEX idx_task_created_at on task(created_at);
CREATE INDEX idx_task_env_created_at on task(env_id, created_at desc);
CREATE INDEX idx_task_env_id on task(env_id);
CREATE INDEX idx_task_env_status
    ON task(env_id, task_status_id);
CREATE INDEX idx_task_heartbeat_stale
    ON task (last_heartbeat_at)
    WHERE task_status_id = 2 AND last_heartbeat_at IS NOT NULL;
CREATE INDEX idx_task_origin_run on task(origin_run_id);
CREATE INDEX idx_task_pending_retry on task(next_retry_at) where task_status_id = 4 and next_retry_at is not null;
CREATE INDEX idx_task_status on task(task_status_id);
CREATE INDEX idx_task_stream_id on task(stream_id);
CREATE UNIQUE INDEX idx_task_parent_retry_unique
    ON task(parent_task_id, retry_attempt)
    WHERE parent_task_id IS NOT NULL;
-- PostgreSQL covers function_name with INCLUDE; SQLite keys it.
CREATE INDEX idx_task_run_id_fn ON task(run_id, function_name);
CREATE INDEX task_env_usage_at_idx ON task(env_id, COALESCE(stop_time, start_time, created_at));
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

-- A call's run and environment are one composite reference, as in
-- PostgreSQL, which replaces the insert-time ownership trigger. The copy
-- also drops the placeholder default that 013 needed to add env_id.
CREATE TABLE call_new (
    call_id blob not null primary key,
    run_id blob not null,
    parent_call_id blob,  -- No FK: avoids self-reference overhead and partition boundary issues (see DATABASE_PARTITIONING_ARCHIVING.md)
    function_name text not null,
    static_scope text not null,  -- For AST metadata lookup
    runtime_path text not null,  -- Unique execution instance
    call_depth integer not null,  -- 0 = top-level, 1 = first nested, etc.
    args text,  -- JSON array of function arguments
    return_value text,  -- JSON return value
    error text,  -- Error message if call failed
    flow text,  -- JSON: {type, modifier, flow_id, parent_flow_id, branch_index, pipe_position}
    start_time datetime not null,
    stop_time datetime,
    duration_us integer,  -- Duration in microseconds
    file text,
    line integer,
    "column" integer,
    position integer,
    size INTEGER NOT NULL DEFAULT 0,
    env_id blob NOT NULL,
    FOREIGN KEY (run_id, env_id) REFERENCES run(run_id, env_id) ON DELETE CASCADE
);
INSERT INTO call_new(call_id, run_id, parent_call_id, function_name, static_scope, runtime_path,
    call_depth, args, return_value, error, flow, start_time, stop_time, duration_us, file, line,
    "column", position, size, env_id)
SELECT call_id, run_id, parent_call_id, function_name, static_scope, runtime_path,
    call_depth, args, return_value, error, flow, start_time, stop_time, duration_us, file, line,
    "column", position, size, env_id FROM call;
DROP TABLE call;
ALTER TABLE call_new RENAME TO call;
CREATE INDEX idx_call_duration_us on call(duration_us) where duration_us is not null;
CREATE INDEX idx_call_function_name on call(function_name);
CREATE INDEX idx_call_parent_call_id on call(parent_call_id);
-- PostgreSQL covers the root call's name with INCLUDE; SQLite keys it.
CREATE INDEX idx_call_root_by_run
    ON call(run_id, function_name) WHERE parent_call_id IS NULL;
CREATE INDEX idx_call_run_id on call(run_id);
CREATE INDEX idx_call_runtime_path on call(runtime_path);
CREATE INDEX idx_call_start_time on call(start_time);
CREATE INDEX idx_call_static_scope on call(static_scope);
CREATE INDEX call_env_start_time_idx ON call(env_id, start_time);
CREATE TRIGGER count_call_bytes_insert AFTER INSERT ON call
WHEN (MAX(NEW.size,0))<>0
BEGIN
    INSERT INTO usage_storage(env_id,org_id,slot,call_bytes) VALUES(NEW.env_id,(SELECT org_id FROM env WHERE env_id=NEW.env_id),0,MAX(NEW.size,0))
    ON CONFLICT(env_id,slot) DO UPDATE SET call_bytes=call_bytes+excluded.call_bytes;
END;
CREATE TRIGGER count_call_bytes_update AFTER UPDATE OF size ON call
WHEN (MAX(NEW.size,0)-MAX(OLD.size,0))<>0
BEGIN
    INSERT INTO usage_storage(env_id,org_id,slot,call_bytes) VALUES(NEW.env_id,(SELECT org_id FROM env WHERE env_id=NEW.env_id),0,MAX(NEW.size,0)-MAX(OLD.size,0))
    ON CONFLICT(env_id,slot) DO UPDATE SET call_bytes=call_bytes+excluded.call_bytes;
END;
CREATE TRIGGER count_call_bytes_delete AFTER DELETE ON call
WHEN (-MAX(OLD.size,0))<>0
BEGIN
    INSERT INTO usage_storage(env_id,org_id,slot,call_bytes) VALUES(OLD.env_id,(SELECT org_id FROM env WHERE env_id=OLD.env_id),0,-MAX(OLD.size,0))
    ON CONFLICT(env_id,slot) DO UPDATE SET call_bytes=call_bytes+excluded.call_bytes;
END;

-- Ownership guards, matching PostgreSQL's immutable_resource_scope and
-- resource_scope_integrity triggers table for table. SQLite trigger names are
-- schema-wide, so each carries its table's name. The narrower guards these
-- replace go first.
DROP TRIGGER immutable_environment_usage_owner;
DROP TRIGGER immutable_run_usage_owner;
DROP TRIGGER immutable_store_usage_owner;
CREATE TRIGGER env_immutable_resource_scope BEFORE UPDATE OF org_id ON env
WHEN NEW.org_id IS NOT OLD.org_id
BEGIN SELECT RAISE(ABORT, 'Resource ownership is immutable'); END;
CREATE TRIGGER project_immutable_resource_scope BEFORE UPDATE OF env_id ON project
WHEN NEW.env_id IS NOT OLD.env_id
BEGIN SELECT RAISE(ABORT, 'Resource ownership is immutable'); END;
CREATE TRIGGER build_immutable_resource_scope BEFORE UPDATE OF project_id ON build
WHEN NEW.project_id IS NOT OLD.project_id
BEGIN SELECT RAISE(ABORT, 'Resource ownership is immutable'); END;
CREATE TRIGGER run_immutable_resource_scope BEFORE UPDATE OF env_id, build_id, stream_id ON run
WHEN NEW.env_id IS NOT OLD.env_id OR NEW.build_id IS NOT OLD.build_id OR NEW.stream_id IS NOT OLD.stream_id
BEGIN SELECT RAISE(ABORT, 'Resource ownership is immutable'); END;
CREATE TRIGGER task_immutable_resource_scope BEFORE UPDATE OF env_id, build_id, stream_id ON task
WHEN NEW.env_id IS NOT OLD.env_id OR NEW.build_id IS NOT OLD.build_id OR NEW.stream_id IS NOT OLD.stream_id
BEGIN SELECT RAISE(ABORT, 'Resource ownership is immutable'); END;
CREATE TRIGGER event_immutable_resource_scope BEFORE UPDATE OF env_id, stream_id ON event
WHEN NEW.env_id IS NOT OLD.env_id OR NEW.stream_id IS NOT OLD.stream_id
BEGIN SELECT RAISE(ABORT, 'Resource ownership is immutable'); END;
CREATE TRIGGER stream_immutable_resource_scope BEFORE UPDATE OF env_id ON stream
WHEN NEW.env_id IS NOT OLD.env_id
BEGIN SELECT RAISE(ABORT, 'Resource ownership is immutable'); END;
CREATE TRIGGER api_key_immutable_resource_scope BEFORE UPDATE OF env_id ON api_key
WHEN NEW.env_id IS NOT OLD.env_id
BEGIN SELECT RAISE(ABORT, 'Resource ownership is immutable'); END;
CREATE TRIGGER service_key_immutable_resource_scope BEFORE UPDATE OF env_id, api_key_id ON service_key
WHEN NEW.env_id IS NOT OLD.env_id OR NEW.api_key_id IS NOT OLD.api_key_id
BEGIN SELECT RAISE(ABORT, 'Resource ownership is immutable'); END;
CREATE TRIGGER session_immutable_resource_scope BEFORE UPDATE OF env_id, api_key_id ON session
WHEN NEW.env_id IS NOT OLD.env_id OR NEW.api_key_id IS NOT OLD.api_key_id
BEGIN SELECT RAISE(ABORT, 'Resource ownership is immutable'); END;
CREATE TRIGGER agent_immutable_resource_scope BEFORE UPDATE OF env_id, build_id ON agent
WHEN NEW.env_id IS NOT OLD.env_id OR NEW.build_id IS NOT OLD.build_id
BEGIN SELECT RAISE(ABORT, 'Resource ownership is immutable'); END;
CREATE TRIGGER workflow_immutable_resource_scope BEFORE UPDATE OF env_id, build_id ON workflow
WHEN NEW.env_id IS NOT OLD.env_id OR NEW.build_id IS NOT OLD.build_id
BEGIN SELECT RAISE(ABORT, 'Resource ownership is immutable'); END;
CREATE TRIGGER file_immutable_resource_scope BEFORE UPDATE OF org_id, env_id ON file
WHEN NEW.org_id IS NOT OLD.org_id OR NEW.env_id IS NOT OLD.env_id
BEGIN SELECT RAISE(ABORT, 'Resource ownership is immutable'); END;
CREATE TRIGGER file_upload_immutable_resource_scope BEFORE UPDATE OF org_id, env_id ON file_upload
WHEN NEW.org_id IS NOT OLD.org_id OR NEW.env_id IS NOT OLD.env_id
BEGIN SELECT RAISE(ABORT, 'Resource ownership is immutable'); END;
CREATE TRIGGER file_upload_part_claim_immutable_resource_scope BEFORE UPDATE OF upload_id ON file_upload_part_claim
WHEN NEW.upload_id IS NOT OLD.upload_id
BEGIN SELECT RAISE(ABORT, 'Resource ownership is immutable'); END;
CREATE TRIGGER blob_object_immutable_resource_scope BEFORE UPDATE OF org_id, env_id ON blob_object
WHEN NEW.org_id IS NOT OLD.org_id OR NEW.env_id IS NOT OLD.env_id
BEGIN SELECT RAISE(ABORT, 'Resource ownership is immutable'); END;
CREATE TRIGGER blob_ref_immutable_resource_scope BEFORE UPDATE OF org_id, env_id, blob_object_id ON blob_ref
WHEN NEW.org_id IS NOT OLD.org_id OR NEW.env_id IS NOT OLD.env_id OR NEW.blob_object_id IS NOT OLD.blob_object_id
BEGIN SELECT RAISE(ABORT, 'Resource ownership is immutable'); END;
CREATE TRIGGER store_map_immutable_resource_scope BEFORE UPDATE OF org_id, env_id ON store_map
WHEN NEW.org_id IS NOT OLD.org_id OR NEW.env_id IS NOT OLD.env_id
BEGIN SELECT RAISE(ABORT, 'Resource ownership is immutable'); END;
CREATE TRIGGER store_map_entry_immutable_resource_scope BEFORE UPDATE OF org_id, env_id ON store_map_entry
WHEN NEW.org_id IS NOT OLD.org_id OR NEW.env_id IS NOT OLD.env_id
BEGIN SELECT RAISE(ABORT, 'Resource ownership is immutable'); END;
CREATE TRIGGER domain_immutable_resource_scope BEFORE UPDATE OF env_id ON domain
WHEN NEW.env_id IS NOT OLD.env_id
BEGIN SELECT RAISE(ABORT, 'Resource ownership is immutable'); END;
CREATE TRIGGER schedule_immutable_resource_scope BEFORE UPDATE OF build_id ON schedule
WHEN NEW.build_id IS NOT OLD.build_id
BEGIN SELECT RAISE(ABORT, 'Resource ownership is immutable'); END;
CREATE TRIGGER mcp_tool_immutable_resource_scope BEFORE UPDATE OF build_id ON mcp_tool
WHEN NEW.build_id IS NOT OLD.build_id
BEGIN SELECT RAISE(ABORT, 'Resource ownership is immutable'); END;
CREATE TRIGGER event_handler_immutable_resource_scope BEFORE UPDATE OF build_id ON event_handler
WHEN NEW.build_id IS NOT OLD.build_id
BEGIN SELECT RAISE(ABORT, 'Resource ownership is immutable'); END;
CREATE TRIGGER context_immutable_resource_scope BEFORE UPDATE OF env_id, project_id ON context
WHEN NEW.env_id IS NOT OLD.env_id OR NEW.project_id IS NOT OLD.project_id
BEGIN SELECT RAISE(ABORT, 'Resource ownership is immutable'); END;
CREATE TRIGGER alert_immutable_resource_scope BEFORE UPDATE OF org_id, env_id ON alert
WHEN NEW.org_id IS NOT OLD.org_id OR NEW.env_id IS NOT OLD.env_id
BEGIN SELECT RAISE(ABORT, 'Resource ownership is immutable'); END;
CREATE TRIGGER call_immutable_resource_scope BEFORE UPDATE OF run_id, env_id ON call
WHEN NEW.run_id IS NOT OLD.run_id OR NEW.env_id IS NOT OLD.env_id
BEGIN SELECT RAISE(ABORT, 'Resource ownership is immutable'); END;
-- Webhook IDs are stable capabilities reused across builds of one project.
-- Redeployment therefore changes build_id without changing resource ownership.
CREATE TRIGGER webhook_immutable_resource_scope BEFORE UPDATE OF build_id ON webhook
WHEN NEW.build_id IS NOT OLD.build_id AND NOT EXISTS (
    SELECT 1 FROM build previous
    JOIN build replacement ON replacement.project_id = previous.project_id
    WHERE previous.build_id = OLD.build_id AND replacement.build_id = NEW.build_id)
BEGIN SELECT RAISE(ABORT, 'Webhook project ownership is immutable'); END;

CREATE TRIGGER run_resource_scope_integrity_insert BEFORE INSERT ON run
BEGIN
    SELECT CASE
        WHEN NOT EXISTS (SELECT 1 FROM build b JOIN project p ON p.project_id = b.project_id WHERE b.build_id = NEW.build_id AND p.env_id = NEW.env_id)
            THEN RAISE(ABORT, 'Build ownership mismatch')
        WHEN NOT EXISTS (SELECT 1 FROM stream s WHERE s.stream_id = NEW.stream_id AND s.env_id = NEW.env_id)
            THEN RAISE(ABORT, 'Stream ownership mismatch')
        WHEN NEW.origin_run_id IS NOT NULL AND NOT EXISTS (SELECT 1 FROM run p WHERE p.run_id = NEW.origin_run_id AND p.env_id = NEW.env_id)
            THEN RAISE(ABORT, 'Origin run ownership mismatch')
        WHEN NEW.event_id IS NOT NULL AND NOT EXISTS (SELECT 1 FROM event e WHERE e.event_id = NEW.event_id AND e.env_id = NEW.env_id)
            THEN RAISE(ABORT, 'Event ownership mismatch') END;
END;
CREATE TRIGGER run_resource_scope_integrity_update BEFORE UPDATE OF env_id, build_id, stream_id, origin_run_id, event_id ON run
BEGIN
    SELECT CASE
        WHEN NOT EXISTS (SELECT 1 FROM build b JOIN project p ON p.project_id = b.project_id WHERE b.build_id = NEW.build_id AND p.env_id = NEW.env_id)
            THEN RAISE(ABORT, 'Build ownership mismatch')
        WHEN NOT EXISTS (SELECT 1 FROM stream s WHERE s.stream_id = NEW.stream_id AND s.env_id = NEW.env_id)
            THEN RAISE(ABORT, 'Stream ownership mismatch')
        WHEN NEW.origin_run_id IS NOT NULL AND NOT EXISTS (SELECT 1 FROM run p WHERE p.run_id = NEW.origin_run_id AND p.env_id = NEW.env_id)
            THEN RAISE(ABORT, 'Origin run ownership mismatch')
        WHEN NEW.event_id IS NOT NULL AND NOT EXISTS (SELECT 1 FROM event e WHERE e.event_id = NEW.event_id AND e.env_id = NEW.env_id)
            THEN RAISE(ABORT, 'Event ownership mismatch') END;
END;
CREATE TRIGGER task_resource_scope_integrity_insert BEFORE INSERT ON task
BEGIN
    SELECT CASE
        WHEN NOT EXISTS (SELECT 1 FROM build b JOIN project p ON p.project_id = b.project_id WHERE b.build_id = NEW.build_id AND p.env_id = NEW.env_id)
            THEN RAISE(ABORT, 'Build ownership mismatch')
        WHEN NOT EXISTS (SELECT 1 FROM stream s WHERE s.stream_id = NEW.stream_id AND s.env_id = NEW.env_id)
            THEN RAISE(ABORT, 'Stream ownership mismatch')
        WHEN NEW.origin_run_id IS NOT NULL AND NOT EXISTS (SELECT 1 FROM run p WHERE p.run_id = NEW.origin_run_id AND p.env_id = NEW.env_id)
            THEN RAISE(ABORT, 'Origin run ownership mismatch')
        WHEN NEW.run_id IS NOT NULL AND NOT EXISTS (SELECT 1 FROM run p WHERE p.run_id = NEW.run_id AND p.env_id = NEW.env_id)
            THEN RAISE(ABORT, 'Task run ownership mismatch') END;
END;
CREATE TRIGGER task_resource_scope_integrity_update BEFORE UPDATE OF env_id, build_id, stream_id, origin_run_id, run_id ON task
BEGIN
    SELECT CASE
        WHEN NOT EXISTS (SELECT 1 FROM build b JOIN project p ON p.project_id = b.project_id WHERE b.build_id = NEW.build_id AND p.env_id = NEW.env_id)
            THEN RAISE(ABORT, 'Build ownership mismatch')
        WHEN NOT EXISTS (SELECT 1 FROM stream s WHERE s.stream_id = NEW.stream_id AND s.env_id = NEW.env_id)
            THEN RAISE(ABORT, 'Stream ownership mismatch')
        WHEN NEW.origin_run_id IS NOT NULL AND NOT EXISTS (SELECT 1 FROM run p WHERE p.run_id = NEW.origin_run_id AND p.env_id = NEW.env_id)
            THEN RAISE(ABORT, 'Origin run ownership mismatch')
        WHEN NEW.run_id IS NOT NULL AND NOT EXISTS (SELECT 1 FROM run p WHERE p.run_id = NEW.run_id AND p.env_id = NEW.env_id)
            THEN RAISE(ABORT, 'Task run ownership mismatch') END;
END;
CREATE TRIGGER event_resource_scope_integrity_insert BEFORE INSERT ON event
BEGIN
    SELECT CASE
        WHEN NOT EXISTS (SELECT 1 FROM stream s WHERE s.stream_id = NEW.stream_id AND s.env_id = NEW.env_id)
            THEN RAISE(ABORT, 'Stream ownership mismatch') END;
END;
CREATE TRIGGER event_resource_scope_integrity_update BEFORE UPDATE OF env_id, stream_id ON event
BEGIN
    SELECT CASE
        WHEN NOT EXISTS (SELECT 1 FROM stream s WHERE s.stream_id = NEW.stream_id AND s.env_id = NEW.env_id)
            THEN RAISE(ABORT, 'Stream ownership mismatch') END;
END;
CREATE TRIGGER agent_resource_scope_integrity_insert BEFORE INSERT ON agent
BEGIN
    SELECT CASE
        WHEN NOT EXISTS (SELECT 1 FROM build b JOIN project p ON p.project_id = b.project_id WHERE b.build_id = NEW.build_id AND p.env_id = NEW.env_id)
            THEN RAISE(ABORT, 'Build ownership mismatch') END;
END;
CREATE TRIGGER agent_resource_scope_integrity_update BEFORE UPDATE OF env_id, build_id ON agent
BEGIN
    SELECT CASE
        WHEN NOT EXISTS (SELECT 1 FROM build b JOIN project p ON p.project_id = b.project_id WHERE b.build_id = NEW.build_id AND p.env_id = NEW.env_id)
            THEN RAISE(ABORT, 'Build ownership mismatch') END;
END;
CREATE TRIGGER workflow_resource_scope_integrity_insert BEFORE INSERT ON workflow
BEGIN
    SELECT CASE
        WHEN NOT EXISTS (SELECT 1 FROM build b JOIN project p ON p.project_id = b.project_id WHERE b.build_id = NEW.build_id AND p.env_id = NEW.env_id)
            THEN RAISE(ABORT, 'Build ownership mismatch') END;
END;
CREATE TRIGGER workflow_resource_scope_integrity_update BEFORE UPDATE OF env_id, build_id ON workflow
BEGIN
    SELECT CASE
        WHEN NOT EXISTS (SELECT 1 FROM build b JOIN project p ON p.project_id = b.project_id WHERE b.build_id = NEW.build_id AND p.env_id = NEW.env_id)
            THEN RAISE(ABORT, 'Build ownership mismatch') END;
END;
CREATE TRIGGER service_key_resource_scope_integrity_insert BEFORE INSERT ON service_key
BEGIN
    SELECT CASE
        WHEN NOT EXISTS (SELECT 1 FROM api_key k WHERE k.api_key_id = NEW.api_key_id AND k.env_id = NEW.env_id)
            THEN RAISE(ABORT, 'Credential ownership mismatch') END;
END;
CREATE TRIGGER service_key_resource_scope_integrity_update BEFORE UPDATE OF env_id, api_key_id ON service_key
BEGIN
    SELECT CASE
        WHEN NOT EXISTS (SELECT 1 FROM api_key k WHERE k.api_key_id = NEW.api_key_id AND k.env_id = NEW.env_id)
            THEN RAISE(ABORT, 'Credential ownership mismatch') END;
END;
CREATE TRIGGER session_resource_scope_integrity_insert BEFORE INSERT ON session
BEGIN
    SELECT CASE
        WHEN NOT EXISTS (SELECT 1 FROM api_key k WHERE k.api_key_id = NEW.api_key_id AND k.env_id = NEW.env_id)
            THEN RAISE(ABORT, 'Credential ownership mismatch') END;
END;
CREATE TRIGGER session_resource_scope_integrity_update BEFORE UPDATE OF env_id, api_key_id ON session
BEGIN
    SELECT CASE
        WHEN NOT EXISTS (SELECT 1 FROM api_key k WHERE k.api_key_id = NEW.api_key_id AND k.env_id = NEW.env_id)
            THEN RAISE(ABORT, 'Credential ownership mismatch') END;
END;
CREATE TRIGGER file_resource_scope_integrity_insert BEFORE INSERT ON file
BEGIN
    SELECT CASE
        WHEN NEW.env_id IS NOT NULL AND NEW.org_id IS NOT NULL AND NOT EXISTS (SELECT 1 FROM env e WHERE e.env_id = NEW.env_id AND e.org_id = NEW.org_id)
            THEN RAISE(ABORT, 'Environment ownership mismatch') END;
END;
CREATE TRIGGER file_resource_scope_integrity_update BEFORE UPDATE OF org_id, env_id ON file
BEGIN
    SELECT CASE
        WHEN NEW.env_id IS NOT NULL AND NEW.org_id IS NOT NULL AND NOT EXISTS (SELECT 1 FROM env e WHERE e.env_id = NEW.env_id AND e.org_id = NEW.org_id)
            THEN RAISE(ABORT, 'Environment ownership mismatch') END;
END;
CREATE TRIGGER alert_channel_resource_scope_integrity_insert BEFORE INSERT ON alert_channel
BEGIN
    SELECT CASE
        WHEN NEW.env_id IS NOT NULL AND NEW.org_id IS NOT NULL AND NOT EXISTS (SELECT 1 FROM env e WHERE e.env_id = NEW.env_id AND e.org_id = NEW.org_id)
            THEN RAISE(ABORT, 'Environment ownership mismatch') END;
END;
CREATE TRIGGER alert_channel_resource_scope_integrity_update BEFORE UPDATE OF org_id, env_id ON alert_channel
BEGIN
    SELECT CASE
        WHEN NEW.env_id IS NOT NULL AND NEW.org_id IS NOT NULL AND NOT EXISTS (SELECT 1 FROM env e WHERE e.env_id = NEW.env_id AND e.org_id = NEW.org_id)
            THEN RAISE(ABORT, 'Environment ownership mismatch') END;
END;
CREATE TRIGGER alert_subscription_resource_scope_integrity_insert BEFORE INSERT ON alert_subscription
BEGIN
    SELECT CASE
        WHEN NEW.env_id IS NOT NULL AND NEW.org_id IS NOT NULL AND NOT EXISTS (SELECT 1 FROM env e WHERE e.env_id = NEW.env_id AND e.org_id = NEW.org_id)
            THEN RAISE(ABORT, 'Environment ownership mismatch') END;
END;
CREATE TRIGGER alert_subscription_resource_scope_integrity_update BEFORE UPDATE OF org_id, env_id ON alert_subscription
BEGIN
    SELECT CASE
        WHEN NEW.env_id IS NOT NULL AND NEW.org_id IS NOT NULL AND NOT EXISTS (SELECT 1 FROM env e WHERE e.env_id = NEW.env_id AND e.org_id = NEW.org_id)
            THEN RAISE(ABORT, 'Environment ownership mismatch') END;
END;
CREATE TRIGGER file_upload_resource_scope_integrity_insert BEFORE INSERT ON file_upload
BEGIN
    SELECT CASE
        WHEN NEW.env_id IS NOT NULL AND NEW.org_id IS NOT NULL AND NOT EXISTS (SELECT 1 FROM env e WHERE e.env_id = NEW.env_id AND e.org_id = NEW.org_id)
            THEN RAISE(ABORT, 'Environment ownership mismatch') END;
END;
CREATE TRIGGER file_upload_resource_scope_integrity_update BEFORE UPDATE OF org_id, env_id ON file_upload
BEGIN
    SELECT CASE
        WHEN NEW.env_id IS NOT NULL AND NEW.org_id IS NOT NULL AND NOT EXISTS (SELECT 1 FROM env e WHERE e.env_id = NEW.env_id AND e.org_id = NEW.org_id)
            THEN RAISE(ABORT, 'Environment ownership mismatch') END;
END;
CREATE TRIGGER blob_object_resource_scope_integrity_insert BEFORE INSERT ON blob_object
BEGIN
    SELECT CASE
        WHEN NEW.env_id IS NOT NULL AND NEW.org_id IS NOT NULL AND NOT EXISTS (SELECT 1 FROM env e WHERE e.env_id = NEW.env_id AND e.org_id = NEW.org_id)
            THEN RAISE(ABORT, 'Environment ownership mismatch') END;
END;
CREATE TRIGGER blob_object_resource_scope_integrity_update BEFORE UPDATE OF org_id, env_id ON blob_object
BEGIN
    SELECT CASE
        WHEN NEW.env_id IS NOT NULL AND NEW.org_id IS NOT NULL AND NOT EXISTS (SELECT 1 FROM env e WHERE e.env_id = NEW.env_id AND e.org_id = NEW.org_id)
            THEN RAISE(ABORT, 'Environment ownership mismatch') END;
END;
CREATE TRIGGER blob_ref_resource_scope_integrity_insert BEFORE INSERT ON blob_ref
BEGIN
    SELECT CASE
        WHEN NEW.env_id IS NOT NULL AND NEW.org_id IS NOT NULL AND NOT EXISTS (SELECT 1 FROM env e WHERE e.env_id = NEW.env_id AND e.org_id = NEW.org_id)
            THEN RAISE(ABORT, 'Environment ownership mismatch')
        WHEN NOT EXISTS (SELECT 1 FROM blob_object b WHERE b.blob_object_id = NEW.blob_object_id AND b.org_id = NEW.org_id AND b.env_id IS NEW.env_id)
            THEN RAISE(ABORT, 'Blob ownership mismatch') END;
END;
CREATE TRIGGER blob_ref_resource_scope_integrity_update BEFORE UPDATE OF org_id, env_id, blob_object_id ON blob_ref
BEGIN
    SELECT CASE
        WHEN NEW.env_id IS NOT NULL AND NEW.org_id IS NOT NULL AND NOT EXISTS (SELECT 1 FROM env e WHERE e.env_id = NEW.env_id AND e.org_id = NEW.org_id)
            THEN RAISE(ABORT, 'Environment ownership mismatch')
        WHEN NOT EXISTS (SELECT 1 FROM blob_object b WHERE b.blob_object_id = NEW.blob_object_id AND b.org_id = NEW.org_id AND b.env_id IS NEW.env_id)
            THEN RAISE(ABORT, 'Blob ownership mismatch') END;
END;
CREATE TRIGGER store_map_resource_scope_integrity_insert BEFORE INSERT ON store_map
BEGIN
    SELECT CASE
        WHEN NEW.env_id IS NOT NULL AND NEW.org_id IS NOT NULL AND NOT EXISTS (SELECT 1 FROM env e WHERE e.env_id = NEW.env_id AND e.org_id = NEW.org_id)
            THEN RAISE(ABORT, 'Environment ownership mismatch') END;
END;
CREATE TRIGGER store_map_resource_scope_integrity_update BEFORE UPDATE OF org_id, env_id ON store_map
BEGIN
    SELECT CASE
        WHEN NEW.env_id IS NOT NULL AND NEW.org_id IS NOT NULL AND NOT EXISTS (SELECT 1 FROM env e WHERE e.env_id = NEW.env_id AND e.org_id = NEW.org_id)
            THEN RAISE(ABORT, 'Environment ownership mismatch') END;
END;
CREATE TRIGGER store_map_entry_resource_scope_integrity_insert BEFORE INSERT ON store_map_entry
BEGIN
    SELECT CASE
        WHEN NEW.env_id IS NOT NULL AND NEW.org_id IS NOT NULL AND NOT EXISTS (SELECT 1 FROM env e WHERE e.env_id = NEW.env_id AND e.org_id = NEW.org_id)
            THEN RAISE(ABORT, 'Environment ownership mismatch') END;
END;
CREATE TRIGGER store_map_entry_resource_scope_integrity_update BEFORE UPDATE OF org_id, env_id ON store_map_entry
BEGIN
    SELECT CASE
        WHEN NEW.env_id IS NOT NULL AND NEW.org_id IS NOT NULL AND NOT EXISTS (SELECT 1 FROM env e WHERE e.env_id = NEW.env_id AND e.org_id = NEW.org_id)
            THEN RAISE(ABORT, 'Environment ownership mismatch') END;
END;
CREATE TRIGGER alert_resource_scope_integrity_insert BEFORE INSERT ON alert
BEGIN
    SELECT CASE
        WHEN NEW.env_id IS NOT NULL AND NEW.org_id IS NOT NULL AND NOT EXISTS (SELECT 1 FROM env e WHERE e.env_id = NEW.env_id AND e.org_id = NEW.org_id)
            THEN RAISE(ABORT, 'Environment ownership mismatch') END;
END;
CREATE TRIGGER alert_resource_scope_integrity_update BEFORE UPDATE OF org_id, env_id ON alert
BEGIN
    SELECT CASE
        WHEN NEW.env_id IS NOT NULL AND NEW.org_id IS NOT NULL AND NOT EXISTS (SELECT 1 FROM env e WHERE e.env_id = NEW.env_id AND e.org_id = NEW.org_id)
            THEN RAISE(ABORT, 'Environment ownership mismatch') END;
END;
CREATE TRIGGER context_resource_scope_integrity_insert BEFORE INSERT ON context
BEGIN
    SELECT CASE
        WHEN NEW.env_id IS NOT NULL AND NEW.project_id IS NOT NULL AND NOT EXISTS (SELECT 1 FROM project p WHERE p.project_id = NEW.project_id AND p.env_id = NEW.env_id)
            THEN RAISE(ABORT, 'Context project ownership mismatch') END;
END;
CREATE TRIGGER context_resource_scope_integrity_update BEFORE UPDATE OF env_id, project_id ON context
BEGIN
    SELECT CASE
        WHEN NEW.env_id IS NOT NULL AND NEW.project_id IS NOT NULL AND NOT EXISTS (SELECT 1 FROM project p WHERE p.project_id = NEW.project_id AND p.env_id = NEW.env_id)
            THEN RAISE(ABORT, 'Context project ownership mismatch') END;
END;

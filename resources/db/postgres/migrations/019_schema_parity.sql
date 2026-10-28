-- Reconcile the PostgreSQL schema with SQLite. Both backends now share their
-- migration numbers, and from 019 on a fresh database of either kind has the
-- same tables, columns, keys, indexes and ownership guards, except where the
-- engines genuinely differ (RLS, column types, generated defaults). The schema
-- parity test holds the two chains together from here.

-- Refuse an upgrade over rows the new keys would reject. Repair the named rows
-- and retry instead of rewriting them here.
DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM hot.project GROUP BY env_id, name HAVING count(*) > 1) THEN
        RAISE EXCEPTION 'Migration 019: an environment has two projects with the same name; rename one and retry';
    END IF;
    IF EXISTS (SELECT 1 FROM hot.store_map s WHERE NOT EXISTS (SELECT 1 FROM hot.org o WHERE o.org_id = s.org_id)
               OR NOT EXISTS (SELECT 1 FROM hot.env e WHERE e.env_id = s.env_id))
       OR EXISTS (SELECT 1 FROM hot.store_map_entry s WHERE NOT EXISTS (SELECT 1 FROM hot.org o WHERE o.org_id = s.org_id)
               OR NOT EXISTS (SELECT 1 FROM hot.env e WHERE e.env_id = s.env_id)) THEN
        RAISE EXCEPTION 'Migration 019: a store references a missing organization or environment';
    END IF;
END $$;

-- Deploys look projects up by name within an environment.
CREATE UNIQUE INDEX idx_project_env_name ON hot.project(env_id, name);

-- Stores belong to their organization and environment, as every other
-- runtime resource does.
ALTER TABLE hot.store_map
    ADD CONSTRAINT store_map_org_id_fkey FOREIGN KEY (org_id) REFERENCES hot.org(org_id) ON UPDATE RESTRICT ON DELETE RESTRICT,
    ADD CONSTRAINT store_map_env_id_fkey FOREIGN KEY (env_id) REFERENCES hot.env(env_id) ON UPDATE RESTRICT ON DELETE RESTRICT;
ALTER TABLE hot.store_map_entry
    ADD CONSTRAINT store_map_entry_org_id_fkey FOREIGN KEY (org_id) REFERENCES hot.org(org_id) ON UPDATE RESTRICT ON DELETE RESTRICT,
    ADD CONSTRAINT store_map_entry_env_id_fkey FOREIGN KEY (env_id) REFERENCES hot.env(env_id) ON UPDATE RESTRICT ON DELETE RESTRICT;

-- Admissions and delegated-key lineage are never edited, by any role. RLS
-- already denies runtime scopes an update; the triggers also bind the owner.
CREATE TRIGGER immutable_resource_scope BEFORE UPDATE ON hot.run_admission
    FOR EACH ROW WHEN ((OLD.run_id, OLD.root_org_id, OLD.org_id, OLD.env_id, OLD.admitted_at) IS DISTINCT FROM (NEW.run_id, NEW.root_org_id, NEW.org_id, NEW.env_id, NEW.admitted_at))
    EXECUTE FUNCTION hot.prevent_scope_reassignment('run_id', 'root_org_id', 'org_id', 'env_id', 'admitted_at');
CREATE TRIGGER immutable_resource_scope BEFORE UPDATE ON hot.api_key_management_issuer
    FOR EACH ROW WHEN ((OLD.api_key_id, OLD.management_key_id, OLD.created_at) IS DISTINCT FROM (NEW.api_key_id, NEW.management_key_id, NEW.created_at))
    EXECUTE FUNCTION hot.prevent_scope_reassignment('api_key_id', 'management_key_id', 'created_at');

-- Index names follow SQLite, which cannot rename an index.
ALTER INDEX hot.idx_hot_api_key_active RENAME TO idx_api_key_active;
ALTER INDEX hot.idx_hot_api_key_env_id RENAME TO idx_api_key_env_id;
ALTER INDEX hot.idx_hot_build_active RENAME TO idx_build_active;
ALTER INDEX hot.idx_hot_build_type_id RENAME TO idx_build_type_id;
ALTER INDEX hot.idx_hot_build_project_id RENAME TO idx_build_project_id;
ALTER INDEX hot.idx_hot_build_project_live_unique RENAME TO idx_build_project_live_unique;
ALTER INDEX hot.idx_hot_build_project_deployed_unique RENAME TO idx_build_project_deployed_unique;
ALTER INDEX hot.idx_hot_call_duration_us RENAME TO idx_call_duration_us;
ALTER INDEX hot.idx_hot_call_function_name RENAME TO idx_call_function_name;
ALTER INDEX hot.idx_hot_call_parent_call_id RENAME TO idx_call_parent_call_id;
ALTER INDEX hot.idx_hot_call_run_id RENAME TO idx_call_run_id;
ALTER INDEX hot.idx_hot_call_runtime_path RENAME TO idx_call_runtime_path;
ALTER INDEX hot.idx_hot_call_start_time RENAME TO idx_call_start_time;
ALTER INDEX hot.idx_hot_call_static_scope RENAME TO idx_call_static_scope;
ALTER INDEX hot.idx_hot_context_active RENAME TO idx_context_active;
ALTER INDEX hot.idx_hot_context_env_id RENAME TO idx_context_env_id;
ALTER INDEX hot.idx_hot_context_project_id RENAME TO idx_context_project_id;
ALTER INDEX hot.idx_hot_context_env_key RENAME TO idx_context_env_key;
ALTER INDEX hot.idx_hot_context_project_key RENAME TO idx_context_project_key;
ALTER INDEX hot.idx_hot_env_active RENAME TO idx_env_active;
ALTER INDEX hot.idx_hot_env_org_id RENAME TO idx_env_org_id;
ALTER INDEX hot.idx_hot_event_created_at RENAME TO idx_event_created_at;
ALTER INDEX hot.idx_hot_event_env_id RENAME TO idx_event_env_id;
ALTER INDEX hot.idx_hot_event_env_created_at RENAME TO idx_event_env_created_at;
ALTER INDEX hot.idx_hot_event_env_handled_created_at RENAME TO idx_event_env_handled_created_at;
ALTER INDEX hot.idx_hot_event_event_type RENAME TO idx_event_event_type;
ALTER INDEX hot.idx_hot_event_handled RENAME TO idx_event_handled;
ALTER INDEX hot.idx_hot_event_stream_id RENAME TO idx_event_stream_id;
ALTER INDEX hot.idx_file_org_env_path_active_unique RENAME TO idx_file_org_env_path_unique;
ALTER INDEX hot.idx_hot_invite_active RENAME TO idx_invite_active;
ALTER INDEX hot.idx_hot_invite_created_at RENAME TO idx_invite_created_at;
ALTER INDEX hot.idx_hot_invite_email RENAME TO idx_invite_email;
ALTER INDEX hot.idx_hot_invite_expires_at RENAME TO idx_invite_expires_at;
ALTER INDEX hot.idx_hot_invite_code RENAME TO idx_invite_code;
ALTER INDEX hot.idx_hot_invite_status_id RENAME TO idx_invite_status_id;
ALTER INDEX hot.idx_hot_invite_org_id RENAME TO idx_invite_org_id;
ALTER INDEX hot.idx_hot_org_active RENAME TO idx_org_active;
ALTER INDEX hot.idx_hot_org_name RENAME TO idx_org_name;
ALTER INDEX hot.idx_hot_org_type RENAME TO idx_org_type;
ALTER INDEX hot.idx_hot_subscription_org_id RENAME TO idx_org_plan_org_id;
ALTER INDEX hot.idx_hot_plan_uuid RENAME TO idx_plan_uuid;
ALTER INDEX hot.idx_hot_org_plan_status_id RENAME TO idx_org_plan_status_id;
ALTER INDEX hot.idx_hot_org_usage_org_id RENAME TO idx_org_usage_org_id;
ALTER INDEX hot.idx_hot_org_usage_period_start RENAME TO idx_org_usage_period_start;
ALTER INDEX hot.idx_hot_org_user_active RENAME TO idx_org_user_active;
ALTER INDEX hot.idx_hot_org_user_org_id RENAME TO idx_org_user_org_id;
ALTER INDEX hot.idx_hot_org_user_user_id RENAME TO idx_org_user_user_id;
ALTER INDEX hot.idx_hot_project_active RENAME TO idx_project_active;
ALTER INDEX hot.idx_hot_project_env_id RENAME TO idx_project_env_id;
ALTER INDEX hot.idx_hot_run_build_id RENAME TO idx_run_build_id;
ALTER INDEX hot.idx_hot_run_by_user_id RENAME TO idx_run_by_user_id;
ALTER INDEX hot.idx_hot_run_env_id RENAME TO idx_run_env_id;
ALTER INDEX hot.idx_hot_run_env_run_type_start_time RENAME TO idx_run_env_run_type_start_time;
ALTER INDEX hot.idx_hot_run_env_start_time RENAME TO idx_run_env_start_time;
ALTER INDEX hot.idx_hot_run_info_warning RENAME TO idx_run_info_warning;
ALTER INDEX hot.idx_hot_run_env_status_start_time RENAME TO idx_run_env_status_start_time;
ALTER INDEX hot.idx_hot_run_event_id RENAME TO idx_run_event_id;
ALTER INDEX hot.idx_hot_run_pending_retry RENAME TO idx_run_pending_retry;
ALTER INDEX hot.idx_hot_run_origin_run_id RENAME TO idx_run_origin_run_id;
ALTER INDEX hot.idx_hot_run_run_type_id RENAME TO idx_run_run_type_id;
ALTER INDEX hot.idx_hot_run_start_time RENAME TO idx_run_start_time;
ALTER INDEX hot.idx_hot_run_status_id RENAME TO idx_run_status_id;
ALTER INDEX hot.idx_hot_run_stop_time RENAME TO idx_run_stop_time;
ALTER INDEX hot.idx_hot_run_stream_id RENAME TO idx_run_stream_id;
ALTER INDEX hot.idx_hot_schedule_build_id RENAME TO idx_schedule_build_id;
ALTER INDEX hot.idx_hot_stream_created_at RENAME TO idx_stream_created_at;
ALTER INDEX hot.idx_hot_stream_env_id RENAME TO idx_stream_env_id;
ALTER INDEX hot.idx_hot_stream_env_last_activity_at RENAME TO idx_stream_env_last_activity_at;
ALTER INDEX hot.idx_hot_stream_env_started_at RENAME TO idx_stream_env_started_at;
ALTER INDEX hot.idx_hot_stream_last_activity_at RENAME TO idx_stream_last_activity_at;
ALTER INDEX hot.idx_hot_stream_started_at RENAME TO idx_stream_started_at;
ALTER INDEX hot.idx_hot_team_active RENAME TO idx_team_active;
ALTER INDEX hot.idx_hot_team_name RENAME TO idx_team_name;
ALTER INDEX hot.idx_hot_team_org_id RENAME TO idx_team_org_id;
ALTER INDEX hot.idx_hot_team_user_active RENAME TO idx_team_user_active;
ALTER INDEX hot.idx_hot_team_user_team_id RENAME TO idx_team_user_team_id;
ALTER INDEX hot.idx_hot_team_user_user_id RENAME TO idx_team_user_user_id;
ALTER INDEX hot.idx_hot_user_active RENAME TO idx_user_active;
ALTER INDEX hot.idx_hot_user_email RENAME TO idx_user_email;
ALTER INDEX hot.idx_hot_user_auth_active RENAME TO idx_user_auth_active;
ALTER INDEX hot.idx_hot_user_auth_type_identifier RENAME TO idx_user_auth_type_identifier;
ALTER INDEX hot.idx_hot_user_auth_user_id RENAME TO idx_user_auth_user_id;

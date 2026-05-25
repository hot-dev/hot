use crate::auth::{AppState, guest_only_middleware, session_middleware};
use crate::handlers::data::{
    event_activity_timeline_handler, event_handling_status_handler, event_type_timeline_handler,
    stream_activity_timeline_handler, stream_composition_handler, task_activity_timeline_handler,
    task_cus_timeline_handler,
};
use crate::handlers::docs::{
    docs_index_handler, docs_search_handler, pkg_route_handler, project_docs_index_handler,
    project_namespace_handler,
};
use crate::handlers::projects::{
    projects_builds_handler, projects_deploy_build_handler, projects_detail_handler,
    projects_list_handler, projects_toggle_active_handler,
};
use crate::handlers::*;
use axum::{Router, middleware, routing::get};
use hot::db::DatabasePool;
use hot::stream::StreamPubSub;
use hot::val::Val;
use std::sync::Arc;
use tokio::sync::watch;

pub fn routes(
    db: Arc<DatabasePool>,
    conf: Val,
    stream_pubsub: Option<Arc<StreamPubSub>>,
    shutdown_rx: watch::Receiver<bool>,
) -> Router {
    let app_state =
        AppState::new(db.clone(), conf.clone(), shutdown_rx).with_stream_pubsub(stream_pubsub);
    // Protected routes - require authentication
    let protected_routes = Router::new()
        .route("/", get(dashboard_handler))
        .route("/dashboard", get(dashboard_handler))
        .route(
            "/account",
            get(account_handler).post(account_update_handler),
        )
        .route(
            "/account/notifications",
            get(notifications_handler).post(notifications_update_handler),
        )
        .route("/account/billing", get(account_billing_handler))
        .route(
            "/dashboard/widgets/failed-runs",
            get(failed_runs_widget_handler),
        )
        .route(
            "/dashboard/widgets/cancelled-runs",
            get(cancelled_runs_widget_handler),
        )
        .route(
            "/dashboard/widgets/unhandled-events",
            get(unhandled_events_widget_handler),
        )
        .route(
            "/dashboard/widgets/recent-runs",
            get(recent_runs_widget_handler),
        )
        .route(
            "/dashboard/widgets/recent-events",
            get(recent_events_widget_handler),
        )
        .route(
            "/dashboard/widgets/failed-tasks",
            get(failed_tasks_widget_handler),
        )
        .route(
            "/dashboard/widgets/recent-tasks",
            get(recent_tasks_widget_handler),
        )
        .route(
            "/dashboard/widgets/recent-streams",
            get(recent_streams_widget_handler),
        )
        .route(
            "/dashboard/widgets/getting-started",
            get(getting_started_widget_handler),
        )
        .route(
            "/dashboard/widgets/agent-health",
            get(agent_health_widget_handler),
        )
        .route("/data/run-type-data", get(run_type_data_handler))
        .route("/data/status-chart-data", get(status_chart_data_handler))
        .route(
            "/data/filtered-type-summary",
            get(filtered_type_summary_handler),
        )
        .route("/data/stream-flow/{stream_id}", get(stream_flow_handler))
        .route("/data/stream-timeline", get(stream_timeline_handler))
        .route("/data/stream-metrics", get(stream_metrics_handler))
        .route(
            "/data/stream-activity-timeline",
            get(stream_activity_timeline_handler),
        )
        .route("/data/stream-composition", get(stream_composition_handler))
        .route(
            "/data/event-activity-timeline",
            get(event_activity_timeline_handler),
        )
        .route(
            "/data/event-type-timeline",
            get(event_type_timeline_handler),
        )
        .route(
            "/data/event-handling-status",
            get(event_handling_status_handler),
        )
        .route("/data/event-timeline", get(event_timeline_handler))
        .route(
            "/data/event-run-relationships",
            get(event_run_relationships_handler),
        )
        .route(
            "/data/task-activity-timeline",
            get(task_activity_timeline_handler),
        )
        .route("/data/task-cus-timeline", get(task_cus_timeline_handler))
        .route("/projects", get(projects_list_handler))
        // Top-level context variables (with project selector)
        .route("/contexts", get(contexts_index_handler))
        .route(
            "/contexts/{project_name}/new",
            get(contexts_new_handler).post(contexts_create_handler),
        )
        .route(
            "/contexts/{project_name}/{context_id}/edit",
            get(contexts_edit_handler).post(contexts_update_handler),
        )
        .route(
            "/contexts/{project_name}/{context_id}/delete",
            axum::routing::post(contexts_delete_handler),
        )
        // Top-level docs (with project selector)
        .route("/docs", get(docs_index_handler))
        // Namespace detail pages
        .route(
            "/docs/{project_name}/project/{*ns_path}",
            get(project_namespace_handler),
        )
        // Package docs - URL scheme matches registry docs: /pkg/{org}/{pkg_name}/{module}
        // Examples:
        //   /docs/{project}/pkg/hot.dev/anthropic         → package index
        //   /docs/{project}/pkg/hot.dev/anthropic/readme  → README
        //   /docs/{project}/pkg/hot.dev/anthropic/::anthropic → namespace detail
        .route(
            "/docs/{project_name}/pkg/{*pkg_path}",
            get(pkg_route_handler),
        )
        // Docs search API
        .route("/api/docs/{project_name}/search", get(docs_search_handler))
        .route("/projects/{project_name}", get(projects_detail_handler))
        .route(
            "/projects/{project_name}/builds",
            get(projects_builds_handler),
        )
        .route(
            "/projects/{project_name}/builds/{build_id}/deploy",
            axum::routing::post(projects_deploy_build_handler),
        )
        .route(
            "/projects/{project_name}/toggle-active",
            axum::routing::post(projects_toggle_active_handler),
        )
        // Legacy project-scoped routes (kept for backwards compatibility)
        .route(
            "/projects/{project_name}/contexts",
            get(contexts_list_handler),
        )
        .route(
            "/projects/{project_name}/contexts/new",
            get(contexts_new_handler).post(contexts_create_handler),
        )
        .route(
            "/projects/{project_name}/contexts/{context_id}/edit",
            get(contexts_edit_handler).post(contexts_update_handler),
        )
        .route(
            "/projects/{project_name}/contexts/{context_id}/delete",
            axum::routing::post(contexts_delete_handler),
        )
        // Project documentation
        .route(
            "/projects/{project_name}/docs",
            get(project_docs_index_handler),
        )
        .route(
            "/projects/{project_name}/docs/",
            get(project_docs_index_handler),
        )
        .route("/workflows", get(agents_list_handler))
        // Agent detail under Workflows; catch-all so `namespace/type` is one tail.
        .route(
            "/workflows/agents/{*qualified_name}",
            get(agents_detail_handler),
        )
        .route(
            "/workflows/named/{*qualified_name}",
            get(workflow_detail_handler),
        )
        .route(
            "/workflows/unnamed/{build_id}",
            get(unnamed_workflow_detail_handler),
        )
        .route("/data/workflow-graph", get(agent_graph_data_handler))
        .route(
            "/data/workflow-graph/agents/{*qualified_name}",
            get(agent_graph_detail_data_handler),
        )
        .route(
            "/data/workflow-graph/workflows/{*qualified_name}",
            get(workflow_graph_detail_data_handler),
        )
        .route(
            "/data/workflow-graph/unnamed/{build_id}",
            get(unnamed_workflow_graph_detail_data_handler),
        )
        // Legacy Agents routes retained as compatibility aliases.
        .route("/agents", get(agents_list_handler))
        .route("/agents/{*qualified_name}", get(agents_detail_handler))
        .route("/data/agent-graph", get(agent_graph_data_handler))
        .route(
            "/data/agent-graph/{*qualified_name}",
            get(agent_graph_detail_data_handler),
        )
        .route("/source/{build_id}/tree", get(source_tree_handler))
        .route("/source/{build_id}/file", get(source_file_handler))
        .route("/source/{build_id}/search", get(source_search_handler))
        .route("/streams", get(streams_list_handler))
        .route("/streams/{stream_id}", get(stream_detail_handler))
        .route("/tasks", get(tasks_list_handler))
        .route("/tasks/{task_id}", get(task_detail_handler))
        .route("/runs", get(runs_list_handler))
        .route("/runs/{run_id}", get(run_detail_handler))
        .route("/runs/{run_id}/tasks-tab", get(run_tasks_tab_handler))
        .route(
            "/runs/{run_id}/retry",
            axum::routing::post(run_retry_handler),
        )
        .route(
            "/runs/{run_id}/rerun",
            axum::routing::post(run_rerun_handler),
        )
        .route("/data/runs/{run_id}/json", get(run_json_handler))
        .route("/data/runs/{run_id}/hierarchy", get(get_hierarchy_handler))
        .route("/data/runs/{run_id}/files", get(run_files_handler))
        .route(
            "/claim-handle",
            get(claim_handle_handler).post(claim_handle_post_handler),
        )
        .route("/orgs", get(orgs_list_handler))
        .route("/orgs/new", get(orgs_new_handler).post(orgs_create_handler))
        // 301 redirect legacy /orgs/{slug}/* URLs to /@{slug}/*
        .route("/orgs/{*rest}", get(legacy_org_redirect))
        .route("/@{org_slug}", get(orgs_detail_handler))
        .route(
            "/@{org_slug}/edit",
            get(orgs_edit_handler).post(orgs_update_handler),
        )
        .route("/@{org_slug}/users", get(org_users_list_handler))
        .route(
            "/@{org_slug}/users/invite",
            get(org_users_invite_handler).post(org_users_invite_post_handler),
        )
        .route(
            "/@{org_slug}/users/{user_id}/edit",
            get(org_users_edit_handler).post(org_users_edit_post_handler),
        )
        .route("/@{org_slug}/billing", get(view_billing_handler))
        .route(
            "/@{org_slug}/billing/checkout",
            get(org_checkout_form_handler).post(org_create_checkout_handler),
        )
        .route(
            "/@{org_slug}/billing/cancel",
            axum::routing::post(cancel_subscription_handler),
        )
        .route(
            "/@{org_slug}/billing/reactivate",
            axum::routing::post(reactivate_subscription_handler),
        )
        .route("/@{org_slug}/usage", get(view_usage_handler))
        .route("/@{org_slug}/usage/stats", get(usage_stats_handler))
        .route("/@{org_slug}/teams", get(teams_list_handler))
        .route(
            "/@{org_slug}/teams/new",
            get(teams_new_handler).post(teams_create_handler),
        )
        .route("/@{org_slug}/teams/{team_id}", get(teams_detail_handler))
        .route(
            "/@{org_slug}/teams/{team_id}/edit",
            get(teams_edit_handler).post(teams_update_handler),
        )
        .route(
            "/@{org_slug}/teams/{team_id}/users",
            get(team_users_list_handler),
        )
        .route(
            "/@{org_slug}/teams/{team_id}/users/add",
            get(team_users_add_handler).post(team_users_add_post_handler),
        )
        .route(
            "/@{org_slug}/teams/{team_id}/users/{user_id}/edit",
            get(team_users_edit_handler).post(team_users_edit_post_handler),
        )
        .route(
            "/@{org_slug}/teams/{team_id}/users/{user_id}/remove",
            axum::routing::post(team_users_remove_post_handler),
        )
        .route("/keys", get(keys_list_handler))
        .route("/keys/new", get(keys_new_handler).post(keys_create_handler))
        .route(
            "/keys/{key_id}/edit",
            get(keys_edit_handler).post(keys_update_handler),
        )
        .route(
            "/service-keys",
            get(service_keys::service_keys_list_handler),
        )
        .route(
            "/service-keys/new",
            get(service_keys::service_keys_new_handler)
                .post(service_keys::service_keys_create_handler),
        )
        .route(
            "/service-keys/{key_id}",
            get(service_keys::service_keys_detail_handler),
        )
        .route(
            "/service-keys/{key_id}/revoke",
            axum::routing::post(service_keys::service_keys_revoke_handler),
        )
        .route("/domains", get(domains::domains_list_handler))
        .route(
            "/domains/new",
            get(domains::domains_new_handler).post(domains::domains_create_handler),
        )
        .route("/domains/{domain_id}", get(domains::domains_detail_handler))
        .route(
            "/domains/{domain_id}/verify",
            axum::routing::post(domains::domains_verify_handler),
        )
        .route(
            "/domains/{domain_id}/delete",
            axum::routing::post(domains::domains_delete_handler),
        )
        .route("/envs", get(envs_list_handler))
        .route("/envs/new", get(envs_new_handler).post(envs_create_handler))
        .route(
            "/envs/{env_id}/edit",
            get(envs_edit_handler).post(envs_update_handler),
        )
        .route(
            "/envs/{env_id}/switch",
            axum::routing::post(switch_env_handler),
        )
        .route("/events", get(events_list_handler))
        .route("/events/{event_id}", get(events_detail_handler))
        .route("/events/{event_id}/table", get(event_detail_table_handler))
        .route("/data/events/{event_id}/json", get(event_json_handler))
        .route("/schedules", get(schedules_list_handler))
        .route("/schedules/{schedule_id}", get(schedule_detail_handler))
        .route("/event-handlers", get(event_handlers_list_handler))
        .route("/mcp", get(mcp_services_list_handler))
        .route("/mcp/{service}", get(mcp_service_detail_handler))
        .route("/webhooks", get(webhook_services_list_handler))
        .route("/webhooks/{service}", get(webhook_service_detail_handler))
        .route("/files", get(files_list_handler))
        .route("/files/{file_id}", get(file_detail_handler))
        .route("/files/{file_id}/download", get(file_download_handler))
        .route("/stores", get(stores_list_handler))
        .route("/stores/{store_name}", get(store_detail_handler))
        .route(
            "/stores/{store_name}/entries/{key_encoded}",
            get(entry_detail_handler),
        )
        .route(
            "/stores/{store_name}/entries/{key_encoded}/value",
            get(entry_value_handler),
        )
        .route(
            "/stores/{store_name}/entries/delete",
            axum::routing::post(entry_delete_handler),
        )
        .route(
            "/switch-org/{org_id}",
            axum::routing::post(switch_org_handler),
        )
        // Real-time SSE subscription for dashboard updates
        .route("/env/subscribe", get(env_subscribe_handler))
        .route("/billing/success", get(checkout_success_handler))
        .route("/billing/cancel", get(checkout_cancel_handler))
        .route("/billing/create-checkout-form", get(checkout_form_handler))
        .route(
            "/billing/create-checkout",
            axum::routing::post(create_checkout_handler),
        )
        // Alert settings routes
        .route(
            "/settings/alerts/destinations",
            get(destinations_list_handler),
        )
        .route(
            "/settings/alerts/destinations/new",
            get(destinations_new_handler),
        )
        .route(
            "/settings/alerts/destinations",
            axum::routing::post(destinations_create_handler),
        )
        .route(
            "/settings/alerts/destinations/{destination_id}/edit",
            get(destinations_edit_handler),
        )
        .route(
            "/settings/alerts/destinations/{destination_id}",
            axum::routing::post(destinations_update_handler),
        )
        .route(
            "/settings/alerts/destinations/{destination_id}/delete",
            axum::routing::post(destinations_delete_handler),
        )
        .route(
            "/settings/alerts/destinations/{destination_id}/resend-verification",
            axum::routing::post(resend_destination_verification_handler),
        )
        .route(
            "/settings/alerts/subscriptions",
            get(subscriptions_list_handler),
        )
        .route(
            "/settings/alerts/subscriptions/new",
            get(subscriptions_new_handler),
        )
        .route(
            "/settings/alerts/subscriptions",
            axum::routing::post(subscriptions_create_handler),
        )
        .route(
            "/settings/alerts/subscriptions/{alert_subscription_id}/edit",
            get(subscriptions_edit_handler),
        )
        .route(
            "/settings/alerts/subscriptions/{alert_subscription_id}",
            axum::routing::post(subscriptions_update_handler),
        )
        .route(
            "/settings/alerts/subscriptions/{alert_subscription_id}/delete",
            axum::routing::post(subscriptions_delete_handler),
        )
        .route(
            "/settings/alerts/channels",
            get(channels_list_handler).post(channels_create_handler),
        )
        .route("/settings/alerts/channels/new", get(channels_new_handler))
        .route(
            "/settings/alerts/channels/{channel_id}/edit",
            get(channels_edit_handler),
        )
        .route(
            "/settings/alerts/channels/{channel_id}",
            axum::routing::post(channels_update_handler),
        )
        .route(
            "/settings/alerts/channels/{channel_id}/delete",
            axum::routing::post(channels_delete_handler),
        )
        .route("/settings/alerts/history", get(history_list_handler))
        .route(
            "/settings/alerts/history/{alert_id}",
            get(history_detail_handler),
        )
        .layer(middleware::from_fn_with_state(
            app_state.clone(),
            session_middleware,
        ))
        .with_state(app_state.clone());

    // Guest-only routes - redirect to dashboard if already signed in
    let guest_routes = Router::new()
        .route("/signin", get(signin_handler).post(signin_post_handler))
        .route("/signup", get(signup_handler).post(signup_post_handler))
        .route("/signup/plans", get(signup_plans_handler))
        .layer(middleware::from_fn(guest_only_middleware))
        .with_state(app_state.clone());

    // Public routes - no authentication required
    let public_routes = Router::new()
        .route("/signout", get(signout_page_handler).post(signout_handler))
        .route(
            "/invite",
            get(invite_accept_handler).post(invite_accept_post_handler),
        )
        .route("/status", get(status_handler))
        .route("/verify-email", get(verify_email_handler))
        .route(
            "/verify-alert-destination",
            get(verify_alert_destination_handler),
        )
        .route(
            "/resend-verification",
            axum::routing::post(resend_verification_handler),
        )
        .route("/auth/google", get(google_auth_handler))
        .route("/auth/google/callback", get(google_callback_handler))
        .route("/auth/github", get(github_auth_handler))
        .route("/auth/github/callback", get(github_callback_handler))
        .with_state(db.clone());

    // Webhook routes (no authentication required)
    let webhook_routes = Router::new()
        .route(
            "/webhooks/billing",
            axum::routing::post(billing_webhook_handler),
        )
        .with_state(db.clone());

    // Combine all routes and add conf as extension
    Router::new()
        .merge(protected_routes)
        .merge(guest_routes)
        .merge(public_routes)
        .merge(webhook_routes)
        .layer(axum::Extension(conf))
}

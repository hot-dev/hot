use crate::auth::{AppState, Session};
use crate::handlers::stream_graph;
use crate::templates;
use ahash::{AHashMap, AHashSet};
use askama::Template;
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Redirect};
use hot::db::{DatabasePool, Run, Task};
use hot::queue::Queue;
use hot::time_range::parse_time_range_cutoff;
use serde_json::json;
use std::str::FromStr;
use std::sync::Arc;
use uuid::Uuid;

pub async fn runs_list_handler(
    State(db): State<Arc<DatabasePool>>,
    Query(params): Query<AHashMap<String, String>>,
    headers: HeaderMap,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> impl IntoResponse {
    // Check if this is an HTMX request (partial update)
    let is_htmx_request = headers.get("HX-Request").is_some();
    // Build breadcrumbs: <org> (<env>) / Runs
    let mut breadcrumbs = templates::build_base_breadcrumbs_with_env(&session);
    breadcrumbs.push(templates::BreadcrumbItem::current("Runs".to_string()));

    // Parse query parameters
    let current_page_num = params
        .get("p")
        .and_then(|p| p.parse::<i64>().ok())
        .unwrap_or(1);

    // Parse filter parameters
    let statuses_param = params.get("statuses").map(|s| s.as_str());
    let run_types_param = params.get("run_types").map(|s| s.as_str());
    let time_range_param = params.get("time_range").map(|s| s.as_str());
    let project_param = params.get("project").map(|s| s.as_str());
    let search_param = params.get("search");

    // Default filters (all options selected if no filters specified)
    let selected_statuses: Vec<String> = if let Some(statuses_str) = statuses_param {
        if statuses_str.is_empty() {
            vec![
                "running".to_string(),
                "succeeded".to_string(),
                "failed".to_string(),
                "cancelled".to_string(),
            ]
        } else {
            statuses_str
                .split(',')
                .map(|s| s.trim().to_string())
                .collect()
        }
    } else {
        vec![
            "running".to_string(),
            "succeeded".to_string(),
            "failed".to_string(),
            "cancelled".to_string(),
        ]
    };

    let selected_run_types: Vec<String> = if let Some(run_types_str) = run_types_param {
        if run_types_str.is_empty() {
            vec![
                "call".to_string(),
                "event".to_string(),
                "schedule".to_string(),
                "run".to_string(),
                "eval".to_string(),
                "repl".to_string(),
            ]
        } else {
            run_types_str
                .split(',')
                .map(|s| s.trim().to_string())
                .collect()
        }
    } else {
        vec![
            "call".to_string(),
            "event".to_string(),
            "schedule".to_string(),
            "run".to_string(),
            "eval".to_string(),
            "repl".to_string(),
        ]
    };

    let selected_time_range = if let Some(time_range_str) = time_range_param {
        if time_range_str == "all" {
            "all".to_string()
        } else {
            time_range_str.to_string()
        }
    } else {
        "all".to_string() // Default to "all time"
    };

    // Parse project filter
    let selected_project: Option<String> = project_param.map(|s| s.to_string());
    let selected_project_uuid: Option<Uuid> = selected_project.as_ref().and_then(|s| {
        if s.is_empty() {
            None
        } else {
            Uuid::parse_str(s).ok()
        }
    });

    const RUNS_PER_PAGE: i64 = 10;

    // Calculate offset
    let offset = (current_page_num - 1) * RUNS_PER_PAGE;

    // Get env ID for filtering runs (use current env)
    let env_id = session.current_env_id();
    tracing::info!(
        "🔍 runs_list_handler: env_id={:?}, user_id={}, current_org={:?}",
        env_id,
        session.current_user_id(),
        session.current_org_id()
    );

    // Prepare filter arrays for database queries
    let status_filter: Option<Vec<&str>> = if selected_statuses.len() == 4 {
        None // All statuses selected, no need to filter
    } else {
        Some(selected_statuses.iter().map(|s| s.as_str()).collect())
    };

    let run_type_filter: Option<Vec<&str>> = if selected_run_types.len() == 6 {
        None // All run types selected, no need to filter
    } else {
        Some(selected_run_types.iter().map(|s| s.as_str()).collect())
    };

    let time_range_cutoff = parse_time_range_cutoff(time_range_param, chrono::Utc::now());

    // Convert search parameter to Option<&str>
    let search_filter: Option<&str> =
        search_param.and_then(|s| if s.is_empty() { None } else { Some(s.as_str()) });

    // Get filtered runs and total count for current environment
    let (runs_data, total_runs) = if let Some(env_id) = env_id {
        let runs = Run::get_filtered_runs_by_env(
            &db,
            &env_id,
            status_filter.as_deref(),
            run_type_filter.as_deref(),
            time_range_cutoff,
            selected_project_uuid.as_ref(),
            search_filter,
            Some(RUNS_PER_PAGE),
            Some(offset),
        )
        .await
        .unwrap_or_else(|e| {
            tracing::error!("Failed to get filtered runs by env {}: {}", env_id, e);
            Vec::new()
        });

        let total = Run::get_filtered_count_by_env(
            &db,
            &env_id,
            status_filter.as_deref(),
            run_type_filter.as_deref(),
            time_range_cutoff,
            selected_project_uuid.as_ref(),
            search_filter,
        )
        .await
        .unwrap_or_else(|e| {
            tracing::error!("Failed to get filtered run count by env {}: {}", env_id, e);
            0
        });

        (runs, total)
    } else {
        // No environment selected - this shouldn't happen in normal operation
        // The session should always have an environment from the cookie fallback
        tracing::error!(
            "No environment selected for runs list - session.current_env_id() returned None"
        );
        (Vec::new(), 0)
    };

    // Convert runs to display format with timezone-aware formatting
    let runs: Vec<templates::RunDisplay> = runs_data
        .iter()
        .map(|run| {
            templates::RunDisplay::from_with_timezone(
                run,
                &session.display_timezone,
                &session.timezone_abbreviation,
            )
        })
        .collect();

    // Get active projects for the filter dropdown
    let projects: Vec<_> = if let Some(env_id) = env_id {
        hot::db::Project::get_projects_by_env(&db, &env_id, None, None)
            .await
            .unwrap_or_else(|e| {
                tracing::error!("Failed to get projects for env {}: {}", env_id, e);
                Vec::new()
            })
            .into_iter()
            .filter(|p| p.active)
            .collect()
    } else {
        Vec::new()
    };

    // Calculate pagination info
    let total_pages = if total_runs > 0 {
        (total_runs + RUNS_PER_PAGE - 1) / RUNS_PER_PAGE
    } else {
        1
    };
    let has_next_page = current_page_num < total_pages;
    let has_prev_page = current_page_num > 1;

    // Calculate pagination window
    let start_page = std::cmp::max(1, current_page_num - 2);
    let end_page = std::cmp::min(total_pages, current_page_num + 2);

    // Return partial template for HTMX requests, full template otherwise
    if is_htmx_request {
        let template = templates::RunsTableContent {
            runs,
            current_page_num,
            start_page,
            end_page,
            has_next_page,
            has_prev_page,
            total_runs,
        };
        Html(template.render().unwrap()).into_response()
    } else {
        let template = templates::RunsList {
            title: "Runs",
            page_context: templates::PrivatePageContext::with_breadcrumbs(
                "runs",
                &session,
                breadcrumbs,
            ),
            runs,
            current_page_num,
            total_pages,
            start_page,
            end_page,
            has_next_page,
            has_prev_page,
            total_runs,
            selected_statuses,
            selected_run_types,
            selected_time_range,
            selected_project: selected_project.unwrap_or_default(),
            search_query: search_param.map(|s| s.to_string()).unwrap_or_default(),
            projects,
        };
        Html(template.render().unwrap()).into_response()
    }
}

pub async fn run_detail_handler(
    Path(run_id): Path<Uuid>,
    Query(params): Query<AHashMap<String, String>>,
    State(db): State<Arc<DatabasePool>>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> impl IntoResponse {
    // Parse query parameters
    let raw_mode = params.get("raw").map(|v| v == "1").unwrap_or(false);
    let inspect_mode = params.get("inspect").map(|v| v == "1").unwrap_or(false);

    // Get current env_id for access check
    let env_id = match session.current_env_id() {
        Some(id) => id,
        None => {
            return Redirect::to("/runs").into_response();
        }
    };

    // Get run details
    match hot::db::Run::get_run(&db, &run_id).await {
        Ok(run) => {
            // SECURITY: Verify the run belongs to the current environment
            if run.env_id != env_id {
                // If the run belongs to a different env in the same org, prompt to switch
                if session.has_env_access(&run.env_id) {
                    let env_name = session
                        .current_org_envs
                        .iter()
                        .find(|e| e.env_id == run.env_id)
                        .map(|e| e.name.as_str())
                        .unwrap_or("another environment");
                    let switch_url =
                        format!("/envs/{}/switch?redirect=/runs/{}", run.env_id, run_id);
                    let template = templates::EnvSwitchPrompt {
                        title: "Switch Environment",
                        page_context: templates::PrivatePageContext::new("runs", &session),
                        message: format!(
                            "This run belongs to the \"{}\" environment. Switch to view it.",
                            env_name
                        ),
                        switch_url,
                        back_url: "/runs".to_string(),
                        back_label: "Back to Runs".to_string(),
                    };
                    return Html(template.render().unwrap()).into_response();
                }
                return Redirect::to("/runs").into_response();
            }

            // Build graph data using unified stream_graph function with run focus
            let graph_data = stream_graph::build_stream_graph(
                &db,
                &run.stream_id,
                stream_graph::FocusElement::Run(run.run_id),
            )
            .await;
            tracing::info!(
                "🔍 Graph data generated: nodes={}, edges={}",
                graph_data.nodes.len(),
                graph_data.edges.len()
            );
            let graph_data_json =
                serde_json::to_string(&graph_data).unwrap_or_else(|_| "{}".to_string());
            tracing::info!("Graph data JSON length: {}", graph_data_json.len());

            // Convert run to display format with timezone-aware formatting
            let run_display = Some(templates::RunDisplay::from_with_timezone(
                &run,
                &session.display_timezone,
                &session.timezone_abbreviation,
            ));

            // Fetch access attribution info if available
            let access_info = if let Some(access_id) = run.access_id {
                match hot::db::access::Access::get_access(&db, &access_id).await {
                    Ok(access) => {
                        // Look up credential name for display
                        let api_key_name = if let Some(ref ak_id) = access.api_key_id {
                            hot::db::api_key::ApiKey::get_api_key(&db, ak_id)
                                .await
                                .ok()
                                .map(|k| k.description)
                        } else {
                            None
                        };
                        let service_key_name = if let Some(ref sk_id) = access.service_key_id {
                            hot::db::service_key::ServiceKey::get_service_key(&db, sk_id)
                                .await
                                .ok()
                                .and_then(|k| k.name)
                        } else {
                            None
                        };
                        Some(templates::AccessInfo::from_access(
                            &access,
                            api_key_name.as_deref(),
                            service_key_name.as_deref(),
                            &session.display_timezone,
                            &session.timezone_abbreviation,
                        ))
                    }
                    Err(_) => None,
                }
            } else {
                None
            };

            // Build breadcrumbs: <org> (<env>) / Runs / <run_id>
            let mut breadcrumbs = templates::build_base_breadcrumbs_with_env(&session);
            breadcrumbs.push(templates::BreadcrumbItem::clickable(
                "Runs".to_string(),
                "/runs".to_string(),
            ));
            breadcrumbs.push(templates::BreadcrumbItem::current(
                templates::get_uuid_short(&run_id),
            ));

            // If this is a task-type run, look up the associated task for linking
            let associated_task_id = if run.run_type == "task" {
                Task::get_by_run_id(&db, &run_id)
                    .await
                    .ok()
                    .flatten()
                    .map(|t| t.task_id)
            } else {
                None
            };

            let template = templates::RunDetail {
                title: &templates::get_uuid_short(&run_id),
                page_context: templates::PrivatePageContext::with_breadcrumbs(
                    "runs",
                    &session,
                    breadcrumbs,
                ),
                run: run_display,
                run_id,
                raw_mode,
                inspect_mode,
                graph_data_json,
                access_info,
                associated_task_id,
            };

            Html(template.render().unwrap()).into_response()
        }
        Err(_) => {
            // Run not found, redirect to runs list
            Redirect::to("/runs").into_response()
        }
    }
}

/// Returns the Tasks tab HTML fragment for a given run (tasks spawned by this run).
pub async fn run_tasks_tab_handler(
    Path(run_id): Path<Uuid>,
    State(db): State<Arc<DatabasePool>>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> impl IntoResponse {
    let env_id = match session.current_env_id() {
        Some(id) => id,
        None => return StatusCode::UNAUTHORIZED.into_response(),
    };

    // Verify run belongs to this env
    match hot::db::Run::get_run(&db, &run_id).await {
        Ok(run) if run.env_id == env_id => {}
        _ => return StatusCode::NOT_FOUND.into_response(),
    }

    let tasks = Task::get_by_origin_run_id(&db, &run_id, Some(100))
        .await
        .unwrap_or_default()
        .iter()
        .map(|t| {
            templates::TaskDisplay::from_with_timezone(
                t,
                &session.display_timezone,
                &session.timezone_abbreviation,
            )
        })
        .collect::<Vec<_>>();

    let template = templates::RunDetailTasksTab { tasks };
    Html(template.render().unwrap()).into_response()
}

// JSON API endpoint for getting run details
pub async fn run_json_handler(
    Path(run_id): Path<Uuid>,
    State(db): State<Arc<DatabasePool>>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> impl IntoResponse {
    // Get current env_id for access check
    let env_id = match session.current_env_id() {
        Some(id) => id,
        None => {
            return Json(json!({
                "error": "No environment selected"
            }));
        }
    };

    match Run::get_run(&db, &run_id).await {
        Ok(run) => {
            // SECURITY: Verify the run belongs to the current environment
            if run.env_id != env_id {
                return Json(json!({
                    "error": "Run not found"
                }));
            }

            Json(json!({
                "run_id": run.run_id,
                "env_id": run.env_id,
                "stream_id": run.stream_id,
                "build_id": run.build_id,
                "run_type": run.run_type,
                "run_type_id": run.run_type_id,
                "origin_run_id": run.origin_run_id,
                "event_id": run.event_id,
                "start_time": run.start_time,
                "stop_time": run.stop_time,
                "status": run.status,
                "status_id": run.status_id,
                "by_user_id": run.by_user_id,
                "result": run.result,
                "project_id": run.project_id,
                "project_name": run.project_name,
                "event_fn": run.event_fn,
            }))
        }
        Err(e) => {
            tracing::error!("Failed to get run {}: {}", run_id, e);
            Json(json!({
                "error": "Run not found"
            }))
        }
    }
}

/// Retry a failed run - creates a new run in the same stream with retry context
pub async fn run_retry_handler(
    Path(run_id): Path<Uuid>,
    State(state): State<AppState>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> impl IntoResponse {
    run_action_handler(run_id, state, session, true).await
}

/// Re-run any completed run - creates a new run in a new stream (no retry context)
pub async fn run_rerun_handler(
    Path(run_id): Path<Uuid>,
    State(state): State<AppState>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> impl IntoResponse {
    run_action_handler(run_id, state, session, false).await
}

/// Common handler for retry and rerun actions
async fn run_action_handler(
    run_id: Uuid,
    state: AppState,
    session: Session,
    is_retry: bool,
) -> impl IntoResponse {
    let db = &state.db;
    let conf = &state.conf;

    // Get current env_id for access check
    let env_id = match session.current_env_id() {
        Some(id) => id,
        None => {
            return (
                StatusCode::FORBIDDEN,
                Json(json!({"error": "No environment selected"})),
            );
        }
    };

    // Get the original run
    let run = match Run::get_run(db, &run_id).await {
        Ok(r) => r,
        Err(e) => {
            tracing::error!("Failed to get run {}: {}", run_id, e);
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "Run not found"})),
            );
        }
    };

    // SECURITY: Verify the run belongs to the current environment
    if run.env_id != env_id {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "Run not found"})),
        );
    }

    // For retry, the run must be failed
    if is_retry && run.status != "failed" {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "Can only retry failed runs"})),
        );
    }

    // Get the original event to extract function and args
    let event_id = match run.event_id {
        Some(id) => id,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "Run has no associated event"})),
            );
        }
    };

    let original_event = match hot::db::Event::get_event(db, &event_id).await {
        Ok(e) => e,
        Err(e) => {
            tracing::error!("Failed to get event {} for run {}: {}", event_id, run_id, e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "Failed to get original event"})),
            );
        }
    };

    // Extract fn and args from original event data
    let fn_name = original_event
        .event_data
        .get("fn")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let args = original_event
        .event_data
        .get("args")
        .cloned()
        .unwrap_or(serde_json::Value::Array(vec![]));

    let fn_name = match fn_name {
        Some(f) => f,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "Original event has no function name"})),
            );
        }
    };

    // Create new event data
    let new_event_id = Uuid::now_v7();
    let new_run_id = Uuid::now_v7();
    let event_time = chrono::Utc::now();

    // For retry: use same stream_id, for rerun: new stream_id
    let stream_id = if is_retry {
        run.stream_id
    } else {
        Uuid::now_v7()
    };

    // Build event data with optional retry context
    let event_data: serde_json::Value = if is_retry {
        json!({
            "fn": fn_name,
            "args": args,
            "retry": {
                "origin-run-id": run_id.to_string(),
                "attempt": run.retry_attempt + 1
            }
        })
    } else {
        json!({
            "fn": fn_name,
            "args": args
        })
    };

    // Insert the event
    if let Err(e) = hot::db::Event::insert_event(
        db,
        &new_event_id,
        &run.env_id,
        &stream_id,
        "hot:call",
        &event_data,
        event_time,
        &session.user.user_id,
        None,
    )
    .await
    {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "Failed to create event", "details": e.to_string()})),
        );
    }

    // Get env for org_id
    let env = match hot::db::Env::get_env(db, &run.env_id).await {
        Ok(e) => e,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "Failed to get environment", "details": e.to_string()})),
            );
        }
    };

    // Create execution context
    let execution_context = hot::lang::event::ExecutionContext {
        run_id: new_run_id,
        stream_id,
        run_type_id: hot::db::run::RunType::Call.as_id(),
        env_id: Some(run.env_id),
        env_name: None,
        user_id: Some(session.user.user_id),
        org_id: Some(env.org_id),
        org_slug: None, // Will be populated later if needed
        build_id: run.build_id,
        build_hash: None,
        project_id: run.project_id,
        project_name: run.project_name.clone(),
        event_id: Some(new_event_id),
        origin_run_id: if is_retry { Some(run_id) } else { None },
        retry_attempt: if is_retry { run.retry_attempt + 1 } else { 0 },
        secret_keys: AHashSet::new(), // Will be populated from ctx metadata
        secret_value_hashes: AHashSet::new(),
        access_id: None, // Dashboard-initiated, no API access log
        agent_type: None,
    };

    // Create event message
    let event_data_val: hot::val::Val =
        serde_json::from_value(event_data.clone()).unwrap_or(hot::val::Val::Null);
    let hot_event = hot::lang::event::Event {
        event_id: new_event_id,
        env_id: run.env_id,
        stream_id,
        event_type: "hot:call".to_string(),
        event_data: event_data_val,
        event_time,
        // Propagate project context from original run for routing tie-breaker
        target_project_id: run.project_id,
        target_project_name: run.project_name.clone(),
    };

    let event_message = hot::lang::event::EventMessage {
        id: new_event_id,
        head: AHashMap::from([
            ("env_id".to_string(), run.env_id.to_string()),
            ("event_type".to_string(), "hot:call".to_string()),
        ]),
        body: hot::lang::event::EventMessageBody {
            event: hot_event,
            execution_context,
        },
    };

    // Enqueue event to worker queue
    let message: hot::data::msg::Message = event_message.into();

    let queue_type_str = conf.get_str_or_default("queue.type", "memory");
    let queue_type =
        hot::queue::QueueType::from_str(&queue_type_str).unwrap_or(hot::queue::QueueType::Memory);

    let redis_uri_str = conf.get_str_or_default("redis.uri", "");
    let redis_uri = if redis_uri_str.is_empty() || redis_uri_str == "null" {
        None
    } else {
        Some(redis_uri_str)
    };

    let redis_cluster = conf.get_bool_or_default("redis.cluster", false);

    let serialization_str = conf.get_str_or_default("serialization.type", "zstd-json");
    let serialization =
        hot::data::serialization::Serialization::from_str(&serialization_str).unwrap_or_default();

    let queue = match hot::queue::ProcessingQueue::<hot::data::msg::Message>::new_with_cluster(
        queue_type,
        "hot:event".to_string(),
        redis_uri,
        redis_cluster,
        serialization,
    ) {
        Ok(q) => q,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "Failed to create event queue", "details": e.to_string()})),
            );
        }
    };

    if let Err(e) = queue.enqueue(message).await {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "Failed to enqueue event", "details": e.to_string()})),
        );
    }

    let action = if is_retry { "retried" } else { "re-run" };
    tracing::info!(
        "Run {} {} - new event {} queued",
        run_id,
        action,
        new_event_id
    );

    (
        StatusCode::OK,
        Json(json!({
            "success": true,
            "action": action,
            "event_id": new_event_id,
            "run_id": new_run_id,
            "stream_id": stream_id
        })),
    )
}

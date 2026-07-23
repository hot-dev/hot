use crate::auth::Session;
use crate::templates;
use crate::templates::filters;
use ahash::AHashMap;
use askama::Template;
use axum::extract::{Query, State};
use axum::response::{Html, IntoResponse};
use hot::db::{AgentStats, DatabasePool, Event, Run, StreamSummary, Task};
use hot::time_range::parse_time_range_cutoff;
use std::sync::Arc;

/// GET / or /dashboard – render the dashboard shell quickly.
///
/// The shell is intentionally bare: the only DB query we issue here is for the
/// project list (needed to populate the filter dropdown synchronously). Every
/// other panel — hero metrics, issues banner, agent health, charts, quick
/// issues tables — is rendered as a pulsing skeleton placeholder and is then
/// hydrated by the JS sequential refresh pipeline (`refreshDashboardData()`)
/// in `templates/dashboard.html`. Doing it this way means the first byte
/// arrives almost immediately and the visible "loading" state only ever
/// involves one DB query on the server's critical path for first paint.
pub async fn dashboard_handler(
    State(db): State<Arc<DatabasePool>>,
    _params: Query<AHashMap<String, String>>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> impl IntoResponse {
    let env_id = session.current_env_id();
    tracing::debug!("Dashboard handler: env_id = {:?}", env_id);
    if let Some(ref env) = session.current_env {
        tracing::debug!("Dashboard handler: current_env.env_id = {}", env.env_id);
    }

    let Some(env_id) = env_id else {
        tracing::warn!("No environment selected for dashboard");
        return Html(empty_dashboard(&session).render().unwrap()).into_response();
    };

    // Single critical-path query: the filter dropdown needs the project list
    // synchronously so the Alpine.js component can render its options.
    let projects: Vec<_> = hot::db::Project::get_projects_by_env(&db, &env_id, None, None)
        .await
        .unwrap_or_else(|e| {
            tracing::error!("Failed to get projects for env {}: {}", env_id, e);
            Vec::new()
        })
        .into_iter()
        .filter(|p| p.active)
        .collect();

    let projects_json = templates::script_safe_json(
        &projects
            .iter()
            .map(|p| {
                serde_json::json!({
                    "project_id": p.project_id.to_string(),
                    "name": p.name.clone(),
                })
            })
            .collect::<Vec<_>>(),
        "[]",
    );

    let template = templates::Dashboard {
        title: "Dashboard",
        page_context: templates::PrivatePageContext::new("dashboard", &session),
        web_url: session.product_web_url.clone(),
        success_message: "",
        chart_data_json: "{}",
        status_chart_data_json: "{}",
        projects,
        projects_json,
    };

    Html(template.render().unwrap()).into_response()
}

fn empty_dashboard(session: &Session) -> templates::Dashboard<'static> {
    templates::Dashboard {
        title: "Dashboard",
        page_context: templates::PrivatePageContext::new("dashboard", session),
        web_url: session.product_web_url.clone(),
        success_message: "",
        chart_data_json: "{}",
        status_chart_data_json: "{}",
        projects: Vec::new(),
        projects_json: "[]".to_string(),
    }
}

#[derive(Template)]
#[template(path = "partials/dashboard_agent_health.html")]
struct AgentHealthPartial {
    total_agents: usize,
    healthy_count: usize,
    degraded_count: usize,
    failing_count: usize,
    idle_count: usize,
    /// Failing agents first, then degraded, capped at MAX_PROBLEM_ROWS.
    problem_agents: Vec<templates::AgentHealthCard>,
    /// Problem agents beyond the cap ("+N more" link).
    more_problem_count: usize,
    /// Short label for the selected time window, e.g. "24h" or "all time".
    window_label: String,
}

/// Short display label for the dashboard `time_range` dropdown values.
fn time_range_label(raw: Option<&str>) -> &'static str {
    match raw {
        None | Some("") | Some("PT24H") | Some("P1D") => "24h",
        Some("PT1H") => "1h",
        Some("P7D") => "7d",
        Some("P30D") => "30d",
        Some("P90D") => "90d",
        Some("all") => "all time",
        Some(_) => "period",
    }
}

/// Maximum problem-agent rows shown on the dashboard widget; the full list
/// lives on /workflows.
const MAX_PROBLEM_ROWS: usize = 5;

/// GET /dashboard/widgets/agent-health - Render the Agent Health rollup partial.
///
/// Renders a one-line rollup (healthy/degraded/failing/idle counts) plus rows
/// for problem agents only, capped at MAX_PROBLEM_ROWS. Follows the dashboard
/// `time_range` and `project` filters like the other widgets. Returns an empty
/// body when no agents are deployed so the section stays hidden.
pub async fn agent_health_widget_handler(
    State(db): State<Arc<DatabasePool>>,
    params: Query<AHashMap<String, String>>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> impl IntoResponse {
    let env_id = match session.current_env_id() {
        Some(id) => id,
        None => return Html("").into_response(),
    };

    let time_range_raw = params.get("time_range").map(String::as_str);
    let cutoff = parse_time_range_cutoff(time_range_raw, chrono::Utc::now());
    let project_id = params
        .get("project")
        .and_then(|p| uuid::Uuid::parse_str(p).ok());
    let window_label = time_range_label(time_range_raw).to_string();

    let summaries = AgentStats::get_per_agent_health(&db, &env_id, cutoff, project_id.as_ref())
        .await
        .unwrap_or_else(|e| {
            tracing::error!("Failed to get agent health for env {}: {}", env_id, e);
            Vec::new()
        });

    if summaries.is_empty() {
        return Html("").into_response();
    }

    let total_agents = summaries.len();
    let mut healthy_count = 0;
    let mut degraded_count = 0;
    let mut failing_count = 0;
    let mut idle_count = 0;
    let mut problems: Vec<_> = Vec::new();

    for a in summaries {
        match a.health_color() {
            "green" => healthy_count += 1,
            "yellow" => {
                degraded_count += 1;
                problems.push(a);
            }
            "red" => {
                failing_count += 1;
                problems.push(a);
            }
            _ => idle_count += 1,
        }
    }

    // Failing before degraded, then by run volume so the busiest problem
    // agents surface first.
    problems.sort_by(|a, b| {
        let sev = |s: &hot::db::AgentHealthSummary| if s.health_color() == "red" { 0 } else { 1 };
        sev(a).cmp(&sev(b)).then(b.total_runs.cmp(&a.total_runs))
    });

    let more_problem_count = problems.len().saturating_sub(MAX_PROBLEM_ROWS);
    let problem_agents: Vec<templates::AgentHealthCard> = problems
        .into_iter()
        .take(MAX_PROBLEM_ROWS)
        .map(|a| {
            let sr = a.success_rate();
            let hc = a.health_color().to_string();
            templates::AgentHealthCard {
                qualified_name: super::agents::qualified_name_to_url_path(&a.qualified_name),
                display_name: a.display_name,
                total_runs: a.total_runs,
                success_rate: sr,
                health_color: hc,
            }
        })
        .collect();

    let template = AgentHealthPartial {
        total_agents,
        healthy_count,
        degraded_count,
        failing_count,
        idle_count,
        problem_agents,
        more_problem_count,
        window_label,
    };
    Html(template.render().unwrap()).into_response()
}

// ============================================
// Dashboard Widget Handlers (for HTMX refresh)
// ============================================

#[derive(Template)]
#[template(path = "components/dashboard_failed_runs_rows.html")]
struct FailedRunsRows {
    runs: Vec<templates::RunDisplay>,
}

/// GET /dashboard/widgets/failed-runs - Get failed runs table rows for Quick Issues
pub async fn failed_runs_widget_handler(
    State(db): State<Arc<DatabasePool>>,
    State(blob_store): State<Option<Arc<hot::blob::BlobStore>>>,
    params: Query<AHashMap<String, String>>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> impl IntoResponse {
    let env_id = match session.current_env_id() {
        Some(id) => id,
        None => {
            return Html("<tr><td colspan=\"3\" class=\"px-3 py-6 text-center text-sm text-gray-500 dark:text-gray-400\">No environment selected</td></tr>").into_response();
        }
    };

    let time_range_cutoff = parse_time_range_cutoff(
        params.get("time_range").map(String::as_str),
        chrono::Utc::now(),
    );
    let project_id = params
        .get("project")
        .and_then(|p| uuid::Uuid::parse_str(p).ok());

    let mut runs_data = Run::get_filtered_runs_by_env(
        &db,
        &env_id,
        Some(&["failed"]),
        None,
        time_range_cutoff,
        project_id.as_ref(),
        None,
        Some(5),
        None,
    )
    .await
    .unwrap_or_else(|e| {
        tracing::error!(
            "Failed to get runs for failed runs widget for env {}: {}",
            env_id,
            e
        );
        Vec::new()
    });
    crate::handlers::rehydrate_runs_for_display(blob_store.as_ref(), &session, &mut runs_data)
        .await;

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

    let template = FailedRunsRows { runs };
    Html(template.render().unwrap()).into_response()
}

#[derive(Template)]
#[template(path = "components/dashboard_cancelled_runs_rows.html")]
struct CancelledRunsRows {
    runs: Vec<templates::RunDisplay>,
}

/// GET /dashboard/widgets/cancelled-runs - Get cancelled runs table rows for Quick Issues
pub async fn cancelled_runs_widget_handler(
    State(db): State<Arc<DatabasePool>>,
    State(blob_store): State<Option<Arc<hot::blob::BlobStore>>>,
    params: Query<AHashMap<String, String>>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> impl IntoResponse {
    let env_id = match session.current_env_id() {
        Some(id) => id,
        None => {
            return Html("<tr><td colspan=\"3\" class=\"px-3 py-6 text-center text-sm text-gray-500 dark:text-gray-400\">No environment selected</td></tr>").into_response();
        }
    };

    let time_range_cutoff = parse_time_range_cutoff(
        params.get("time_range").map(String::as_str),
        chrono::Utc::now(),
    );
    let project_id = params
        .get("project")
        .and_then(|p| uuid::Uuid::parse_str(p).ok());

    let mut runs_data = Run::get_filtered_runs_by_env(
        &db,
        &env_id,
        Some(&["cancelled"]),
        None,
        time_range_cutoff,
        project_id.as_ref(),
        None,
        Some(5),
        None,
    )
    .await
    .unwrap_or_else(|e| {
        tracing::error!(
            "Failed to get runs for cancelled runs widget for env {}: {}",
            env_id,
            e
        );
        Vec::new()
    });
    crate::handlers::rehydrate_runs_for_display(blob_store.as_ref(), &session, &mut runs_data)
        .await;

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

    let template = CancelledRunsRows { runs };
    Html(template.render().unwrap()).into_response()
}

#[derive(Template)]
#[template(path = "components/dashboard_unhandled_events_rows.html")]
struct UnhandledEventsRows {
    events: Vec<templates::EventDisplay>,
}

/// GET /dashboard/widgets/unhandled-events - Get unhandled events table rows for Quick Issues
pub async fn unhandled_events_widget_handler(
    State(db): State<Arc<DatabasePool>>,
    State(blob_store): State<Option<Arc<hot::blob::BlobStore>>>,
    params: Query<AHashMap<String, String>>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> impl IntoResponse {
    let env_id = match session.current_env_id() {
        Some(id) => id,
        None => {
            return Html("<tr><td colspan=\"3\" class=\"px-3 py-6 text-center text-sm text-gray-500 dark:text-gray-400\">No environment selected</td></tr>").into_response();
        }
    };

    let time_range_cutoff = parse_time_range_cutoff(
        params.get("time_range").map(String::as_str),
        chrono::Utc::now(),
    );

    let mut events_data = Event::get_events_by_env_filtered(
        &db,
        &env_id,
        Some(false),
        time_range_cutoff,
        None,
        Some(5),
        None,
    )
    .await
    .unwrap_or_else(|e| {
        tracing::error!(
            "Failed to get events for unhandled events widget for env {}: {}",
            env_id,
            e
        );
        Vec::new()
    });
    crate::handlers::rehydrate_events_for_display(blob_store.as_ref(), &session, &mut events_data)
        .await;

    let events: Vec<templates::EventDisplay> = events_data
        .into_iter()
        .map(|event| {
            templates::EventDisplay::from_with_timezone(
                &event,
                &session.display_timezone,
                &session.timezone_abbreviation,
            )
        })
        .collect();

    let template = UnhandledEventsRows { events };
    Html(template.render().unwrap()).into_response()
}

/// GET /dashboard/widgets/recent-runs - Get recent runs table rows for Recent Activity
pub async fn recent_runs_widget_handler(
    State(db): State<Arc<DatabasePool>>,
    State(blob_store): State<Option<Arc<hot::blob::BlobStore>>>,
    _params: Query<AHashMap<String, String>>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> impl IntoResponse {
    let env_id = match session.current_env_id() {
        Some(id) => id,
        None => {
            return Html("<tr><td colspan=\"7\" class=\"px-6 py-4 text-center text-sm text-gray-500 dark:text-gray-400\">No environment selected</td></tr>").into_response();
        }
    };

    // Get recent runs (basic query)
    let mut runs_data = Run::get_runs_by_env(&db, &env_id, Some(5), None)
        .await
        .unwrap_or_else(|e| {
            tracing::error!(
                "Failed to get runs for recent runs widget for env {}: {}",
                env_id,
                e
            );
            Vec::new()
        });
    crate::handlers::rehydrate_runs_for_display(blob_store.as_ref(), &session, &mut runs_data)
        .await;

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

    let template = templates::DashboardRecentRunsTable { recent_runs: runs };
    Html(template.render().unwrap()).into_response()
}

/// GET /dashboard/widgets/recent-events - Get recent events table rows for Recent Activity
pub async fn recent_events_widget_handler(
    State(db): State<Arc<DatabasePool>>,
    State(blob_store): State<Option<Arc<hot::blob::BlobStore>>>,
    _params: Query<AHashMap<String, String>>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> impl IntoResponse {
    let env_id = match session.current_env_id() {
        Some(id) => id,
        None => {
            return Html("<tr><td colspan=\"5\" class=\"px-6 py-4 text-center text-sm text-gray-500 dark:text-gray-400\">No environment selected</td></tr>").into_response();
        }
    };

    // Get recent events (basic query)
    let mut events_data = Event::get_events_by_env(&db, &env_id, Some(5), None)
        .await
        .unwrap_or_else(|e| {
            tracing::error!(
                "Failed to get events for recent events widget for env {}: {}",
                env_id,
                e
            );
            Vec::new()
        });
    crate::handlers::rehydrate_events_for_display(blob_store.as_ref(), &session, &mut events_data)
        .await;

    let events: Vec<templates::EventDisplay> = events_data
        .into_iter()
        .map(|event| {
            templates::EventDisplay::from_with_timezone(
                &event,
                &session.display_timezone,
                &session.timezone_abbreviation,
            )
        })
        .collect();

    let template = templates::DashboardRecentEventsTable {
        recent_events: events,
    };
    Html(template.render().unwrap()).into_response()
}

#[derive(Template)]
#[template(path = "components/dashboard_failed_tasks_rows.html")]
struct FailedTasksRows {
    tasks: Vec<templates::TaskDisplay>,
}

/// GET /dashboard/widgets/failed-tasks - Get failed tasks table rows for Quick Issues
pub async fn failed_tasks_widget_handler(
    State(db): State<Arc<DatabasePool>>,
    State(blob_store): State<Option<Arc<hot::blob::BlobStore>>>,
    params: Query<AHashMap<String, String>>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> impl IntoResponse {
    let env_id = match session.current_env_id() {
        Some(id) => id,
        None => {
            return Html("<tr><td colspan=\"5\" class=\"px-3 py-6 text-center text-sm text-gray-500 dark:text-gray-400\">No environment selected</td></tr>").into_response();
        }
    };

    let time_range_cutoff = parse_time_range_cutoff(
        params.get("time_range").map(String::as_str),
        chrono::Utc::now(),
    );

    let mut tasks_data = Task::get_filtered_by_env(
        &db,
        &env_id,
        Some(&["failed", "timed_out"]),
        None,
        time_range_cutoff,
        None,
        5,
        0,
    )
    .await
    .unwrap_or_else(|e| {
        tracing::error!(
            "Failed to get tasks for failed tasks widget for env {}: {}",
            env_id,
            e
        );
        Vec::new()
    });
    crate::handlers::rehydrate_tasks_for_display(blob_store.as_ref(), &session, &mut tasks_data)
        .await;
    let tasks: Vec<templates::TaskDisplay> = tasks_data
        .iter()
        .map(|task| {
            templates::TaskDisplay::from_with_timezone(
                task,
                &session.display_timezone,
                &session.timezone_abbreviation,
            )
        })
        .collect();

    let template = FailedTasksRows { tasks };
    Html(template.render().unwrap()).into_response()
}

/// GET /dashboard/widgets/recent-tasks - Get recent tasks table rows for Recent Activity
pub async fn recent_tasks_widget_handler(
    State(db): State<Arc<DatabasePool>>,
    State(blob_store): State<Option<Arc<hot::blob::BlobStore>>>,
    _params: Query<AHashMap<String, String>>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> impl IntoResponse {
    let env_id = match session.current_env_id() {
        Some(id) => id,
        None => {
            return Html("<tr><td colspan=\"8\" class=\"px-6 py-4 text-center text-sm text-gray-500 dark:text-gray-400\">No environment selected</td></tr>").into_response();
        }
    };

    let mut tasks_data = Task::get_by_env(&db, &env_id, Some(5), None)
        .await
        .unwrap_or_else(|e| {
            tracing::error!(
                "Failed to get tasks for recent tasks widget for env {}: {}",
                env_id,
                e
            );
            Vec::new()
        });
    crate::handlers::rehydrate_tasks_for_display(blob_store.as_ref(), &session, &mut tasks_data)
        .await;

    let recent_tasks: Vec<templates::TaskDisplay> = tasks_data
        .iter()
        .map(|task| {
            templates::TaskDisplay::from_with_timezone(
                task,
                &session.display_timezone,
                &session.timezone_abbreviation,
            )
        })
        .collect();

    let template = templates::DashboardRecentTasksTable { recent_tasks };
    Html(template.render().unwrap()).into_response()
}

/// GET /dashboard/widgets/recent-streams - Get recent streams table rows for Recent Activity
pub async fn recent_streams_widget_handler(
    State(db): State<Arc<DatabasePool>>,
    Query(params): Query<AHashMap<String, String>>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> impl IntoResponse {
    let env_id = match session.current_env_id() {
        Some(id) => id,
        None => {
            return Html("<tr><td colspan=\"6\" class=\"px-6 py-4 text-center text-sm text-gray-500 dark:text-gray-400\">No environment selected</td></tr>").into_response();
        }
    };

    // Parse filters
    let _project_id = params
        .get("project")
        .and_then(|p| uuid::Uuid::parse_str(p).ok());
    let _time_range = params.get("time_range").map(String::as_str);

    // Get recent streams (basic query)
    let streams = StreamSummary::get_streams_by_env(&db, &env_id, Some(5), None)
        .await
        .unwrap_or_else(|e| {
            tracing::error!(
                "Failed to get streams for recent streams widget for env {}: {}",
                env_id,
                e
            );
            Vec::new()
        });

    // Convert streams to display format with timezone-aware formatting
    let streams_display: Vec<templates::StreamListItem> = streams
        .iter()
        .map(|stream| {
            templates::StreamListItem::from_with_timezone(
                stream,
                &session.display_timezone,
                &session.timezone_abbreviation,
            )
        })
        .collect();

    let template = templates::DashboardRecentStreamsTable {
        recent_streams: streams_display,
    };
    Html(template.render().unwrap()).into_response()
}

#[derive(Template)]
#[template(path = "components/getting_started.html")]
struct GettingStarted {
    projects: Vec<hot::db::Project>,
    web_url: String,
}

/// GET /dashboard/widgets/getting-started - Get getting started empty state (hides when projects exist)
pub async fn getting_started_widget_handler(
    State(db): State<Arc<DatabasePool>>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> impl IntoResponse {
    let env_id = match session.current_env_id() {
        Some(id) => id,
        None => {
            // No environment - show empty (no getting started message)
            return Html("").into_response();
        }
    };

    // Get active projects for the current environment
    let projects: Vec<_> = hot::db::Project::get_projects_by_env(&db, &env_id, None, None)
        .await
        .unwrap_or_else(|e| {
            tracing::error!(
                "Failed to get projects for getting started widget for env {}: {}",
                env_id,
                e
            );
            Vec::new()
        })
        .into_iter()
        .filter(|p| p.active)
        .collect();

    let template = GettingStarted {
        projects,
        web_url: session.product_web_url.clone(),
    };
    Html(template.render().unwrap()).into_response()
}

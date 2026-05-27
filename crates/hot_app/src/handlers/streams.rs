use crate::auth::Session;
use crate::handlers::stream_graph;
use crate::templates;
use ahash::AHashMap;
use askama::Template;
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::response::{Html, IntoResponse};
use hot::db::{DatabasePool, Event, Run, StreamSummary, Task};
use hot::time_range::parse_time_range_cutoff;
use std::sync::Arc;
use uuid::Uuid;

const STREAMS_PER_PAGE: i64 = 10;

pub async fn streams_list_handler(
    Query(params): Query<AHashMap<String, String>>,
    headers: HeaderMap,
    State(db): State<Arc<DatabasePool>>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> impl IntoResponse {
    // Check if this is an HTMX request (partial update)
    let is_htmx_request = headers.get("HX-Request").is_some();
    // Get the current environment ID
    let env_id = match session.current_env_id() {
        Some(id) => id,
        None => {
            return Html("<div>No environment selected</div>".to_string()).into_response();
        }
    };

    // Pagination parameters
    let page = params
        .get("p")
        .and_then(|p| p.parse::<i64>().ok())
        .unwrap_or(1);
    let page_size = STREAMS_PER_PAGE;
    let offset = (page - 1) * page_size;

    // Parse filter parameters
    let project_param = params.get("project");
    let selected_project: String = project_param
        .map(|s| s.to_string())
        .unwrap_or_else(String::new);

    let time_range_param = params.get("time_range");
    let selected_time_range: String = time_range_param
        .map(|s| s.to_string())
        .unwrap_or_else(|| "all".to_string());

    let search_param = params.get("search");
    let search_query: String = search_param
        .map(|s| s.to_string())
        .unwrap_or_else(String::new);

    // Convert project ID to UUID if specified
    let _selected_project_uuid = if !selected_project.is_empty() {
        Uuid::parse_str(&selected_project).ok()
    } else {
        None
    };

    let time_range_cutoff =
        parse_time_range_cutoff(time_range_param.map(|s| s.as_str()), chrono::Utc::now());

    // Convert search query to Option<&str>
    let search_term = if !search_query.is_empty() {
        Some(search_query.as_str())
    } else {
        None
    };

    // Fetch streams with filters
    let streams = StreamSummary::get_streams_by_env_filtered(
        &db,
        &env_id,
        time_range_cutoff,
        search_term,
        Some(page_size),
        Some(offset),
    )
    .await
    .unwrap_or_else(|e| {
        tracing::error!("Failed to fetch streams for env_id {}: {}", env_id, e);
        Vec::new()
    });

    // Get total count for pagination with filters
    let total_streams =
        StreamSummary::get_count_by_env_filtered(&db, &env_id, time_range_cutoff, search_term)
            .await
            .unwrap_or_else(|e| {
                tracing::error!("Failed to get stream count for env_id {}: {}", env_id, e);
                0
            });

    // Pagination logic
    let total_pages = if total_streams > 0 {
        (total_streams + page_size - 1) / page_size
    } else {
        1
    };
    let current_page_num = page;
    let has_next_page = current_page_num < total_pages;
    let has_prev_page = current_page_num > 1;

    // Calculate pagination window
    let start_page = std::cmp::max(1, current_page_num - 2);
    let end_page = std::cmp::min(total_pages, current_page_num + 2);

    // Get active projects for the filter dropdown
    let projects: Vec<_> = hot::db::Project::get_projects_by_env(&db, &env_id, None, None)
        .await
        .unwrap_or_else(|e| {
            tracing::error!("Failed to get projects for env {}: {}", env_id, e);
            Vec::new()
        })
        .into_iter()
        .filter(|p| p.active)
        .collect();

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

    let mut breadcrumbs = templates::build_base_breadcrumbs_with_env(&session);
    breadcrumbs.push(templates::BreadcrumbItem::current("Streams".to_string()));

    // Return partial template for HTMX requests, full template otherwise
    if is_htmx_request {
        let template = templates::StreamsTableContent {
            streams: streams_display,
            current_page_num,
            start_page,
            end_page,
            has_next_page,
            has_prev_page,
            total_streams,
        };
        Html(template.render().unwrap()).into_response()
    } else {
        let template = templates::StreamsList {
            title: "Streams",
            page_context: templates::PrivatePageContext::with_breadcrumbs(
                "streams",
                &session,
                breadcrumbs,
            ),
            streams: streams_display,
            current_page_num,
            total_pages,
            start_page,
            end_page,
            has_next_page,
            has_prev_page,
            total_streams,
            selected_project,
            selected_time_range,
            search_query,
            projects,
        };
        Html(template.render().unwrap()).into_response()
    }
}

pub async fn stream_detail_handler(
    Path(stream_id): Path<Uuid>,
    Query(params): Query<AHashMap<String, String>>,
    State(db): State<Arc<DatabasePool>>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> impl IntoResponse {
    // Get current env_id for access check
    let env_id = match session.current_env_id() {
        Some(id) => id,
        None => {
            return Html("<div>No environment selected</div>".to_string()).into_response();
        }
    };

    // Parse query parameters
    let page = params
        .get("p")
        .and_then(|p| p.parse::<i64>().ok())
        .unwrap_or(1);
    let page_size = STREAMS_PER_PAGE;
    let offset = (page - 1) * page_size;

    // Get stream summary
    let stream_raw = match StreamSummary::get_stream(&db, &stream_id).await {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("Failed to fetch stream {}: {}", stream_id, e);
            return Html("<div>Stream not found</div>".to_string()).into_response();
        }
    };

    // SECURITY: Verify the stream belongs to the current environment
    if stream_raw.env_id != env_id {
        return Html("<div>Stream not found</div>".to_string()).into_response();
    }

    // Convert stream to display format with timezone-aware formatting
    let stream = templates::StreamListItem::from_with_timezone(
        &stream_raw,
        &session.display_timezone,
        &session.timezone_abbreviation,
    );

    // Get runs for this stream
    tracing::debug!(
        "Fetching runs for stream_id: {} (page: {}, page_size: {}, offset: {})",
        stream_id,
        page,
        page_size,
        offset
    );

    let runs = Run::get_runs_by_stream(&db, &stream_id, &env_id, Some(page_size), Some(offset))
        .await
        .unwrap_or_else(|e| {
            tracing::error!("Failed to fetch runs for stream {}: {}", stream_id, e);
            Vec::new()
        });

    tracing::debug!("Found {} runs for stream_id: {}", runs.len(), stream_id);

    // Convert runs to display format with timezone-aware formatting
    let run_displays: Vec<templates::RunDisplay> = runs
        .iter()
        .map(|run| {
            templates::RunDisplay::from_with_timezone(
                run,
                &session.display_timezone,
                &session.timezone_abbreviation,
            )
        })
        .collect();

    // Get events for this stream
    let events_raw =
        Event::get_events_by_stream(&db, &stream_id, &env_id, Some(page_size), Some(offset))
            .await
            .unwrap_or_else(|e| {
                tracing::error!("Failed to fetch events for stream {}: {}", stream_id, e);
                Vec::new()
            });

    tracing::debug!(
        "Found {} events for stream_id: {}",
        events_raw.len(),
        stream_id
    );

    // Convert events to display format with timezone-aware formatting
    let events: Vec<templates::EventListItem> = events_raw
        .iter()
        .map(|event| {
            templates::EventListItem::from_with_timezone(
                event,
                &session.display_timezone,
                &session.timezone_abbreviation,
            )
        })
        .collect();

    // Get tasks for this stream
    let tasks_raw = Task::get_by_stream(&db, &stream_id, &env_id, Some(page_size))
        .await
        .unwrap_or_else(|e| {
            tracing::error!("Failed to fetch tasks for stream {}: {}", stream_id, e);
            Vec::new()
        });

    let tasks: Vec<templates::TaskDisplay> = tasks_raw
        .iter()
        .map(|task| {
            templates::TaskDisplay::from_with_timezone(
                task,
                &session.display_timezone,
                &session.timezone_abbreviation,
            )
        })
        .collect();

    // Total count for pagination
    let total_runs = Run::get_count_by_stream(&db, &stream_id, &env_id)
        .await
        .unwrap_or_else(|e| {
            tracing::error!("Failed to get run count for stream {}: {}", stream_id, e);
            0
        });

    tracing::debug!(
        "Total run count for stream_id {}: {}",
        stream_id,
        total_runs
    );

    // Pagination logic
    let total_pages = (total_runs + page_size - 1) / page_size;
    let current_page_num = page;
    let has_next_page = current_page_num < total_pages;
    let has_prev_page = current_page_num > 1;

    let start_page = std::cmp::max(1, current_page_num - 2);
    let end_page = std::cmp::min(total_pages, current_page_num + 2);

    // Build stream graph using unified function (no focus)
    let graph_data = stream_graph::build_stream_graph(
        &db,
        &stream_id,
        &env_id,
        stream_graph::FocusElement::None,
    )
    .await;
    let graph_data_json = serde_json::to_string(&graph_data).unwrap_or_else(|e| {
        tracing::error!("Failed to serialize graph data: {}", e);
        "{}".to_string()
    });

    let mut breadcrumbs = templates::build_base_breadcrumbs_with_env(&session);
    breadcrumbs.push(templates::BreadcrumbItem::clickable(
        "Streams".to_string(),
        "/streams".to_string(),
    ));
    breadcrumbs.push(templates::BreadcrumbItem::current(
        stream_id.to_string()[..8].to_string(),
    ));

    let template = templates::StreamDetail {
        title: "Stream Detail",
        page_context: templates::PrivatePageContext::with_breadcrumbs(
            "streams",
            &session,
            breadcrumbs,
        ),
        stream,
        runs: run_displays,
        events,
        tasks,
        graph_data,
        graph_data_json,
        current_page_num,
        total_pages,
        start_page,
        end_page,
        has_next_page,
        has_prev_page,
    };

    Html(template.render().unwrap()).into_response()
}

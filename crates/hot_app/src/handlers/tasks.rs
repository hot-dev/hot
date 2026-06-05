use crate::auth::Session;
use crate::handlers::stream_graph;
use crate::templates::{self, TaskDisplay};
use ahash::AHashMap;
use askama::Template;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse};
use hot::db::{DatabasePool, Task};
use hot::time_range::parse_time_range_cutoff;
use std::sync::Arc;
use uuid::Uuid;

pub async fn tasks_list_handler(
    State(db): State<Arc<DatabasePool>>,
    Query(params): Query<AHashMap<String, String>>,
    headers: HeaderMap,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> impl IntoResponse {
    let is_htmx_request = crate::handlers::is_htmx_request(&headers);

    let mut breadcrumbs = templates::build_base_breadcrumbs_with_env(&session);
    breadcrumbs.push(templates::BreadcrumbItem::current("Tasks".to_string()));

    let current_page_num = params
        .get("p")
        .and_then(|p| p.parse::<i64>().ok())
        .unwrap_or(1);

    let statuses_param = params.get("statuses").map(|s| s.as_str());
    let task_types_param = params.get("task_types").map(|s| s.as_str());
    let time_range_param = params.get("time_range").map(|s| s.as_str());
    let search_param = params.get("search");

    let selected_statuses: Vec<String> = if let Some(statuses_str) = statuses_param {
        if statuses_str.is_empty() {
            vec![]
        } else {
            statuses_str
                .split(',')
                .map(|s| s.trim().to_string())
                .collect()
        }
    } else {
        vec![]
    };

    let selected_task_types: Vec<String> = if let Some(types_str) = task_types_param {
        if types_str.is_empty() {
            vec![]
        } else {
            types_str.split(',').map(|s| s.trim().to_string()).collect()
        }
    } else {
        vec![]
    };

    let selected_time_range = time_range_param
        .map(|s| s.to_string())
        .unwrap_or_else(|| "all".to_string());

    let search_query = search_param.map(|s| s.to_string()).unwrap_or_default();

    const TASKS_PER_PAGE: i64 = 10;
    let offset = (current_page_num - 1) * TASKS_PER_PAGE;

    let status_filter: Option<Vec<&str>> = if selected_statuses.is_empty() {
        None
    } else {
        Some(selected_statuses.iter().map(|s| s.as_str()).collect())
    };

    let task_type_filter: Option<Vec<&str>> = if selected_task_types.is_empty() {
        None
    } else {
        Some(selected_task_types.iter().map(|s| s.as_str()).collect())
    };

    let time_range_cutoff = parse_time_range_cutoff(time_range_param, chrono::Utc::now());

    let search_filter: Option<&str> = if search_query.is_empty() {
        None
    } else {
        Some(&search_query)
    };

    let env_id = session.current_env_id();

    let (tasks_data, total_tasks) = if let Some(env_id) = env_id {
        let tasks = Task::get_filtered_by_env(
            &db,
            &env_id,
            status_filter.as_deref(),
            task_type_filter.as_deref(),
            time_range_cutoff,
            search_filter,
            TASKS_PER_PAGE,
            offset,
        )
        .await
        .unwrap_or_else(|e| {
            tracing::error!("Failed to get filtered tasks by env {}: {}", env_id, e);
            Vec::new()
        });

        let total = Task::get_filtered_count_by_env(
            &db,
            &env_id,
            status_filter.as_deref(),
            task_type_filter.as_deref(),
            time_range_cutoff,
            search_filter,
        )
        .await
        .unwrap_or_else(|e| {
            tracing::error!("Failed to get filtered task count by env {}: {}", env_id, e);
            0
        });

        (tasks, total)
    } else {
        (Vec::new(), 0)
    };

    let tasks_display: Vec<TaskDisplay> = tasks_data
        .iter()
        .map(|t| {
            TaskDisplay::from_with_timezone(
                t,
                &session.display_timezone,
                &session.timezone_abbreviation,
            )
        })
        .collect();

    let total_pages = if total_tasks > 0 {
        (total_tasks + TASKS_PER_PAGE - 1) / TASKS_PER_PAGE
    } else {
        1
    };
    let has_next_page = current_page_num < total_pages;
    let has_prev_page = current_page_num > 1;
    let start_page = std::cmp::max(1, current_page_num - 2);
    let end_page = std::cmp::min(total_pages, current_page_num + 2);

    if is_htmx_request {
        let template = templates::TasksTableContent {
            tasks: tasks_display,
            current_page_num,
            start_page,
            end_page,
            has_next_page,
            has_prev_page,
            total_tasks,
        };
        Html(template.render().unwrap_or_default()).into_response()
    } else {
        let page_context =
            templates::PrivatePageContext::with_breadcrumbs("tasks", &session, breadcrumbs);

        let template = templates::TasksList {
            title: "Tasks",
            page_context,
            tasks: tasks_display,
            current_page_num,
            total_pages,
            start_page,
            end_page,
            has_next_page,
            has_prev_page,
            total_tasks,
            selected_statuses,
            selected_task_types,
            selected_time_range,
            search_query,
        };

        Html(
            template
                .render()
                .unwrap_or_else(|e| format!("Template error: {}", e)),
        )
        .into_response()
    }
}

pub async fn task_detail_handler(
    Path(task_id): Path<String>,
    State(db): State<Arc<DatabasePool>>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> Result<Html<String>, (StatusCode, String)> {
    let task_uuid: Uuid = task_id
        .parse()
        .map_err(|_| (StatusCode::BAD_REQUEST, "Invalid task ID".to_string()))?;
    let env_id = session
        .current_env_id()
        .ok_or((StatusCode::NOT_FOUND, "Task not found".to_string()))?;

    let task = Task::get(&db, &task_uuid)
        .await
        .map_err(|_| (StatusCode::NOT_FOUND, "Task not found".to_string()))?;

    if task.env_id != env_id {
        return Err((StatusCode::NOT_FOUND, "Task not found".to_string()));
    }

    let task_display = TaskDisplay::from_with_timezone(
        &task,
        &session.display_timezone,
        &session.timezone_abbreviation,
    );

    let graph_data = stream_graph::build_stream_graph(
        &db,
        &task.stream_id,
        &task.env_id,
        stream_graph::FocusElement::Task(task_uuid),
    )
    .await;
    let graph_data_json = serde_json::to_string(&graph_data).unwrap_or_else(|e| {
        tracing::error!("Failed to serialize graph data: {}", e);
        "{}".to_string()
    });

    let mut breadcrumbs = templates::build_base_breadcrumbs_with_env(&session);
    breadcrumbs.push(templates::BreadcrumbItem::clickable(
        "Tasks".to_string(),
        "/tasks".to_string(),
    ));
    breadcrumbs.push(templates::BreadcrumbItem::current(
        templates::get_uuid_short(&task_uuid),
    ));

    let page_context =
        templates::PrivatePageContext::with_breadcrumbs("tasks", &session, breadcrumbs);

    let template = templates::TaskDetail {
        title: &templates::get_uuid_short(&task_uuid),
        page_context,
        task: task_display,
        graph_data_json,
    };

    Ok(Html(template.render().map_err(|e| {
        (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
    })?))
}

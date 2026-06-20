use crate::auth::Session;
use crate::handlers::list_query;
use crate::handlers::stream_graph;
use crate::templates::{self, TaskDisplay};
use ahash::AHashMap;
use askama::Template;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse};
use hot::db::{DatabasePool, Task};
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

    const TASKS_PER_PAGE: i64 = 10;
    let page = list_query::PageParams::parse(&params, TASKS_PER_PAGE);
    let query_filters = list_query::SearchAndTimeRange::parse(&params, chrono::Utc::now());
    let selected_statuses = list_query::selected_csv_or_empty(&params, "statuses");
    let selected_task_types = list_query::selected_csv_or_empty(&params, "task_types");
    let status_filter = list_query::non_empty_filter(&selected_statuses);
    let task_type_filter = list_query::non_empty_filter(&selected_task_types);
    let search_filter = query_filters.search_term();

    let env_id = session.current_env_id();

    let (tasks_data, total_tasks) = if let Some(env_id) = env_id {
        let tasks = Task::get_filtered_by_env(
            &db,
            &env_id,
            status_filter.as_deref(),
            task_type_filter.as_deref(),
            query_filters.time_range_cutoff,
            search_filter,
            TASKS_PER_PAGE,
            page.offset,
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
            query_filters.time_range_cutoff,
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

    let pagination = list_query::PaginationWindow::new(total_tasks, &page);

    if is_htmx_request {
        let template = templates::TasksTableContent {
            tasks: tasks_display,
            current_page_num: page.current_page_num,
            start_page: pagination.start_page,
            end_page: pagination.end_page,
            has_next_page: pagination.has_next_page,
            has_prev_page: pagination.has_prev_page,
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
            current_page_num: page.current_page_num,
            total_pages: pagination.total_pages,
            start_page: pagination.start_page,
            end_page: pagination.end_page,
            has_next_page: pagination.has_next_page,
            has_prev_page: pagination.has_prev_page,
            total_tasks,
            selected_statuses,
            selected_task_types,
            selected_time_range: query_filters.selected_time_range,
            search_query: query_filters.search_query,
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

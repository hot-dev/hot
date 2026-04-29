use crate::auth::Session;
use crate::templates;
use ahash::AHashMap;
use askama::Template;
use axum::extract::{Path, Query, State};
use axum::response::{Html, IntoResponse, Redirect};
use hot::db::{DatabasePool, Event, EventHandler, Schedule, ScheduleLog};
use std::sync::Arc;
use uuid::Uuid;

/// Handler for listing scheduled runs (schedules) for deployed builds
pub async fn schedules_list_handler(
    State(db): State<Arc<DatabasePool>>,
    Query(params): Query<AHashMap<String, String>>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> impl IntoResponse {
    // Build breadcrumbs: <org> (<env>) / Scheduled Runs
    let mut breadcrumbs = templates::build_base_breadcrumbs_with_env(&session);
    breadcrumbs.push(templates::BreadcrumbItem::current(
        "Scheduled Runs".to_string(),
    ));

    // Parse query parameters
    let current_page_num = params
        .get("p")
        .and_then(|p| p.parse::<i64>().ok())
        .unwrap_or(1);

    let search_query = params
        .get("search")
        .map(|s| s.to_string())
        .unwrap_or_default();

    const ITEMS_PER_PAGE: i64 = 20;
    let offset = (current_page_num - 1) * ITEMS_PER_PAGE;

    // Get env ID for filtering
    let env_id = session.current_env_id();

    let (schedules, total_schedules) = if let Some(env_id) = env_id {
        // Get all schedules first (we'll need to filter for search)
        let all_schedules = Schedule::get_schedules_by_env_deployed(
            &db,
            &env_id,
            Some(1000), // Get more to allow filtering
            Some(0),
        )
        .await
        .unwrap_or_else(|e| {
            tracing::error!("Failed to get schedules for env {}: {}", env_id, e);
            Vec::new()
        });

        // Apply search filter if present
        let filtered_schedules: Vec<_> = if search_query.is_empty() {
            all_schedules
        } else {
            let search_lower = search_query.to_lowercase();
            all_schedules
                .into_iter()
                .filter(|s| {
                    s.project_name.to_lowercase().contains(&search_lower)
                        || s.ns.to_lowercase().contains(&search_lower)
                        || s.var.to_lowercase().contains(&search_lower)
                        || s.cron.to_lowercase().contains(&search_lower)
                })
                .collect()
        };

        let total = filtered_schedules.len() as i64;

        // Paginate the filtered results and convert to display type
        let schedules: Vec<_> = filtered_schedules
            .into_iter()
            .skip(offset as usize)
            .take(ITEMS_PER_PAGE as usize)
            .map(|s| {
                templates::ScheduleDisplay::from_with_timezone(
                    s,
                    &session.display_timezone,
                    &session.timezone_abbreviation,
                )
            })
            .collect();

        (schedules, total)
    } else {
        (Vec::new(), 0)
    };

    // Calculate pagination info
    let total_pages = if total_schedules > 0 {
        (total_schedules + ITEMS_PER_PAGE - 1) / ITEMS_PER_PAGE
    } else {
        1
    };
    let has_next_page = current_page_num < total_pages;
    let has_prev_page = current_page_num > 1;
    let start_page = std::cmp::max(1, current_page_num - 2);
    let end_page = std::cmp::min(total_pages, current_page_num + 2);

    let template = templates::SchedulesList {
        title: "Scheduled Runs",
        page_context: templates::PrivatePageContext::with_breadcrumbs(
            "schedules",
            &session,
            breadcrumbs,
        ),
        schedules,
        current_page_num,
        total_pages,
        start_page,
        end_page,
        has_next_page,
        has_prev_page,
        total_schedules,
        search_query,
    };

    Html(template.render().unwrap()).into_response()
}

/// Handler for schedule detail page showing schedule log
pub async fn schedule_detail_handler(
    Path(schedule_id): Path<Uuid>,
    Query(params): Query<AHashMap<String, String>>,
    State(db): State<Arc<DatabasePool>>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> impl IntoResponse {
    // Get current env_id for access check
    let env_id = match session.current_env_id() {
        Some(id) => id,
        None => {
            return Redirect::to("/schedules").into_response();
        }
    };

    // Parse query parameters
    let current_page_num = params
        .get("p")
        .and_then(|p| p.parse::<i64>().ok())
        .unwrap_or(1);

    const ITEMS_PER_PAGE: i64 = 50;
    let offset = (current_page_num - 1) * ITEMS_PER_PAGE;

    // Get schedule details
    match Schedule::get_schedule(&db, &schedule_id).await {
        Ok(schedule) => {
            // SECURITY: Verify the schedule belongs to the current environment
            // by checking schedule -> build -> project -> env chain
            let build = match hot::db::Build::get_build(&db, &schedule.build_id).await {
                Ok(b) => b,
                Err(_) => {
                    return Redirect::to("/schedules").into_response();
                }
            };
            let project = match hot::db::Project::get_project(&db, &build.project_id).await {
                Ok(p) => p,
                Err(_) => {
                    return Redirect::to("/schedules").into_response();
                }
            };
            if project.env_id != env_id {
                return Redirect::to("/schedules").into_response();
            }
            // Get schedule log history with pagination
            let schedule_logs =
                ScheduleLog::get_history(&db, &schedule_id, Some(ITEMS_PER_PAGE), Some(offset))
                    .await
                    .unwrap_or_else(|e| {
                        tracing::error!("Failed to get schedule log for {}: {}", schedule_id, e);
                        Vec::new()
                    });

            let total_logs = ScheduleLog::get_execution_count(&db, &schedule_id)
                .await
                .unwrap_or(0);

            // Calculate pagination info
            let total_pages = if total_logs > 0 {
                (total_logs + ITEMS_PER_PAGE - 1) / ITEMS_PER_PAGE
            } else {
                1
            };
            let has_next_page = current_page_num < total_pages;
            let has_prev_page = current_page_num > 1;
            let start_page = std::cmp::max(1, current_page_num - 2);
            let end_page = std::cmp::min(total_pages, current_page_num + 2);

            // Build breadcrumbs
            let mut breadcrumbs = templates::build_base_breadcrumbs_with_env(&session);
            breadcrumbs.push(templates::BreadcrumbItem::clickable(
                "Scheduled Runs".to_string(),
                "/schedules".to_string(),
            ));
            breadcrumbs.push(templates::BreadcrumbItem::current(format!(
                "{}/{}",
                schedule.ns, schedule.var
            )));

            // Collect event_ids and fetch their stream_ids
            let event_ids: Vec<Uuid> = schedule_logs
                .iter()
                .filter_map(|log| log.event_id)
                .collect();

            // Build a map of event_id -> stream_id
            let mut event_stream_map: AHashMap<Uuid, Uuid> = AHashMap::new();
            for event_id in &event_ids {
                if let Ok(event) = Event::get_event(&db, event_id).await {
                    event_stream_map.insert(*event_id, event.stream_id);
                }
            }

            // Convert logs to display format with timezone
            let schedule_logs_display: Vec<templates::ScheduleLogDisplay> = schedule_logs
                .iter()
                .map(|log| {
                    let stream_id = log
                        .event_id
                        .and_then(|eid| event_stream_map.get(&eid).copied());
                    templates::ScheduleLogDisplay::from_with_timezone(
                        log,
                        stream_id,
                        &session.display_timezone,
                        &session.timezone_abbreviation,
                    )
                })
                .collect();

            // Extract retry display info
            let (retry_attempts, retry_delay) = templates::extract_retry_display(&schedule.meta);

            let template = templates::ScheduleDetail {
                title: &format!("{}/{}", schedule.ns, schedule.var),
                page_context: templates::PrivatePageContext::with_breadcrumbs(
                    "schedules",
                    &session,
                    breadcrumbs,
                ),
                schedule,
                schedule_logs: schedule_logs_display,
                current_page_num,
                total_pages,
                start_page,
                end_page,
                has_next_page,
                has_prev_page,
                total_logs,
                retry_attempts,
                retry_delay,
            };

            Html(template.render().unwrap()).into_response()
        }
        Err(_) => {
            // Schedule not found, redirect to schedules list
            Redirect::to("/schedules").into_response()
        }
    }
}

/// Handler for listing event handlers for deployed builds
pub async fn event_handlers_list_handler(
    State(db): State<Arc<DatabasePool>>,
    Query(params): Query<AHashMap<String, String>>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> impl IntoResponse {
    // Build breadcrumbs: <org> (<env>) / Event Handlers
    let mut breadcrumbs = templates::build_base_breadcrumbs_with_env(&session);
    breadcrumbs.push(templates::BreadcrumbItem::current(
        "Event Handlers".to_string(),
    ));

    // Parse query parameters
    let current_page_num = params
        .get("p")
        .and_then(|p| p.parse::<i64>().ok())
        .unwrap_or(1);

    let search_query = params
        .get("search")
        .map(|s| s.to_string())
        .unwrap_or_default();

    const ITEMS_PER_PAGE: i64 = 20;
    let offset = (current_page_num - 1) * ITEMS_PER_PAGE;

    // Get env ID for filtering
    let env_id = session.current_env_id();

    let (event_handlers, total_handlers) = if let Some(env_id) = env_id {
        // Get all event handlers first (we'll need to filter for search)
        let all_handlers = EventHandler::get_event_handlers_by_env_deployed(
            &db,
            &env_id,
            Some(1000), // Get more to allow filtering
            Some(0),
        )
        .await
        .unwrap_or_else(|e| {
            tracing::error!("Failed to get event handlers for env {}: {}", env_id, e);
            Vec::new()
        });

        // Apply search filter if present
        let filtered_handlers: Vec<_> = if search_query.is_empty() {
            all_handlers
        } else {
            let search_lower = search_query.to_lowercase();
            all_handlers
                .into_iter()
                .filter(|h| {
                    h.project_name.to_lowercase().contains(&search_lower)
                        || h.event_type.to_lowercase().contains(&search_lower)
                        || h.ns.to_lowercase().contains(&search_lower)
                        || h.var.to_lowercase().contains(&search_lower)
                })
                .collect()
        };

        let total = filtered_handlers.len() as i64;

        // Paginate the filtered results and convert to display type
        let handlers: Vec<_> = filtered_handlers
            .into_iter()
            .skip(offset as usize)
            .take(ITEMS_PER_PAGE as usize)
            .map(templates::EventHandlerDisplay::from)
            .collect();

        (handlers, total)
    } else {
        (Vec::new(), 0)
    };

    // Calculate pagination info
    let total_pages = if total_handlers > 0 {
        (total_handlers + ITEMS_PER_PAGE - 1) / ITEMS_PER_PAGE
    } else {
        1
    };
    let has_next_page = current_page_num < total_pages;
    let has_prev_page = current_page_num > 1;
    let start_page = std::cmp::max(1, current_page_num - 2);
    let end_page = std::cmp::min(total_pages, current_page_num + 2);

    let template = templates::EventHandlersList {
        title: "Event Handlers",
        page_context: templates::PrivatePageContext::with_breadcrumbs(
            "event_handlers",
            &session,
            breadcrumbs,
        ),
        event_handlers,
        current_page_num,
        total_pages,
        start_page,
        end_page,
        has_next_page,
        has_prev_page,
        total_handlers,
        search_query,
    };

    Html(template.render().unwrap()).into_response()
}

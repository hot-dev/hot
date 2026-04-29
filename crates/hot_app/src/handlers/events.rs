use crate::auth::Session;
use crate::handlers::stream_graph;
use crate::templates;
use ahash::{AHashMap, AHashSet};
use askama::Template;
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::response::{Html, IntoResponse, Redirect};
use hot::db::DatabasePool;
use hot::time_range::parse_time_range_cutoff;
use serde_json::json;
use std::sync::Arc;
use uuid::Uuid;

const EVENTS_PER_PAGE: i64 = 10;

pub async fn events_list_handler(
    State(db): State<Arc<DatabasePool>>,
    Query(params): Query<AHashMap<String, String>>,
    headers: HeaderMap,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> impl IntoResponse {
    // Check if this is an HTMX request (partial update)
    let is_htmx_request = headers.get("HX-Request").is_some();
    // Build breadcrumbs: <org> (<env>) / Events
    let mut breadcrumbs = templates::build_base_breadcrumbs_with_env(&session);
    breadcrumbs.push(templates::BreadcrumbItem::current("Events".to_string()));

    // Parse query parameters
    let current_page_num = params
        .get("p")
        .and_then(|p| p.parse::<i64>().ok())
        .unwrap_or(1);
    let inspect_mode = params.get("inspect").map(|v| v == "1").unwrap_or(false);

    // Parse handled filter: "all" (default), "handled", "unhandled"
    let handled_param = params.get("handled");
    let selected_handled: String = handled_param
        .map(|s| s.to_string())
        .unwrap_or_else(|| "all".to_string());

    // Parse time range filter
    let time_range_param = params.get("time_range");
    let selected_time_range: String = time_range_param
        .map(|s| s.to_string())
        .unwrap_or_else(|| "all".to_string());

    // Parse search filter
    let search_param = params.get("search");
    let search_query: String = search_param
        .map(|s| s.to_string())
        .unwrap_or_else(String::new);

    // Calculate offset
    let offset = (current_page_num - 1) * EVENTS_PER_PAGE;

    let time_range_cutoff =
        parse_time_range_cutoff(time_range_param.map(|s| s.as_str()), chrono::Utc::now());

    // Convert search query to Option<&str>
    let search_term = if !search_query.is_empty() {
        Some(search_query.as_str())
    } else {
        None
    };

    // Get events for current environment
    let (events, total_events) = if let Some(env) = &session.current_env {
        // Filter events based on handled status
        let events = match selected_handled.as_str() {
            "handled" => hot::db::Event::get_events_by_env_filtered(
                &db,
                &env.env_id,
                Some(true),
                time_range_cutoff,
                search_term,
                Some(EVENTS_PER_PAGE),
                Some(offset),
            )
            .await
            .unwrap_or_else(|e| {
                tracing::error!("Failed to get handled events by env {}: {}", env.env_id, e);
                Vec::new()
            }),
            "unhandled" => hot::db::Event::get_events_by_env_filtered(
                &db,
                &env.env_id,
                Some(false),
                time_range_cutoff,
                search_term,
                Some(EVENTS_PER_PAGE),
                Some(offset),
            )
            .await
            .unwrap_or_else(|e| {
                tracing::error!(
                    "Failed to get unhandled events by env {}: {}",
                    env.env_id,
                    e
                );
                Vec::new()
            }),
            _ => {
                // "all" or any other value - use filtered method with no handled filter
                hot::db::Event::get_events_by_env_filtered(
                    &db,
                    &env.env_id,
                    None,
                    time_range_cutoff,
                    search_term,
                    Some(EVENTS_PER_PAGE),
                    Some(offset),
                )
                .await
                .unwrap_or_else(|e| {
                    tracing::error!("Failed to get events by env {}: {}", env.env_id, e);
                    Vec::new()
                })
            }
        };

        // Get total count based on filter
        let total = hot::db::Event::get_filtered_count_by_env(
            &db,
            &env.env_id,
            match selected_handled.as_str() {
                "handled" => Some(true),
                "unhandled" => Some(false),
                _ => None,
            },
            time_range_cutoff,
            search_term,
        )
        .await
        .unwrap_or_else(|e| {
            tracing::error!(
                "Failed to get filtered event count by env {}: {}",
                env.env_id,
                e
            );
            0
        });

        (events, total)
    } else {
        (Vec::new(), 0)
    };

    // Calculate pagination info
    let total_pages = (total_events + EVENTS_PER_PAGE - 1) / EVENTS_PER_PAGE;
    let has_next_page = current_page_num < total_pages;
    let has_prev_page = current_page_num > 1;

    // Calculate pagination window
    let start_page = std::cmp::max(1, current_page_num - 2);
    let end_page = std::cmp::min(total_pages, current_page_num + 2);

    // Convert events to display format with timezone-aware formatting
    let events_display: Vec<templates::EventListItem> = events
        .iter()
        .map(|event| {
            templates::EventListItem::from_with_timezone(
                event,
                &session.display_timezone,
                &session.timezone_abbreviation,
            )
        })
        .collect();

    // Return partial template for HTMX requests, full template otherwise
    if is_htmx_request {
        let template = templates::EventsTableContent {
            events: events_display,
            current_page_num,
            start_page,
            end_page,
            has_next_page,
            has_prev_page,
            total_events,
        };
        Html(template.render().unwrap()).into_response()
    } else {
        let template = templates::EventsList {
            title: "Events",
            page_context: templates::PrivatePageContext::with_breadcrumbs(
                "events",
                &session,
                breadcrumbs,
            ),
            events: events_display,
            current_page_num,
            total_pages,
            start_page,
            end_page,
            has_next_page,
            has_prev_page,
            total_events,
            inspect_mode,
            selected_handled,
            selected_time_range,
            search_query,
        };
        Html(template.render().unwrap()).into_response()
    }
}

pub async fn events_detail_handler(
    Path(event_id): Path<Uuid>,
    Query(params): Query<AHashMap<String, String>>,
    State(db): State<Arc<DatabasePool>>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> impl IntoResponse {
    // Get current env_id for access check
    let env_id = match session.current_env_id() {
        Some(id) => id,
        None => {
            return Redirect::to("/events").into_response();
        }
    };

    // Parse query parameters
    let current_page_num = params
        .get("p")
        .and_then(|p| p.parse::<i64>().ok())
        .unwrap_or(1);

    // Calculate offset
    let offset = (current_page_num - 1) * EVENTS_PER_PAGE;

    // Get event details
    match hot::db::Event::get_event(&db, &event_id).await {
        Ok(event) => {
            // SECURITY: Verify the event belongs to the current environment
            if event.env_id != env_id {
                return Redirect::to("/events").into_response();
            }
            // Get event runs
            let event_runs_raw = hot::db::Event::get_runs_by_event(
                &db,
                &event_id,
                Some(EVENTS_PER_PAGE),
                Some(offset),
            )
            .await
            .unwrap_or_else(|e| {
                tracing::error!("Failed to get runs by event {}: {}", event_id, e);
                Vec::new()
            });

            // Convert runs to RunDisplay format for consistent formatting
            let event_runs: Vec<templates::RunDisplay> = event_runs_raw
                .iter()
                .map(|run| {
                    templates::RunDisplay::from_with_timezone(
                        run,
                        &session.display_timezone,
                        &session.timezone_abbreviation,
                    )
                })
                .collect();

            // Get total count for pagination
            let total_count = hot::db::Event::get_run_count_by_event(&db, &event_id)
                .await
                .unwrap_or_else(|e| {
                    tracing::error!("Failed to get run count by event {}: {}", event_id, e);
                    0
                });

            // Calculate pagination info
            let total_pages = (total_count + EVENTS_PER_PAGE - 1) / EVENTS_PER_PAGE;
            let has_next_page = current_page_num < total_pages;
            let has_prev_page = current_page_num > 1;

            // Calculate pagination window
            let start_page = std::cmp::max(1, current_page_num - 2);
            let end_page = std::cmp::min(total_pages, current_page_num + 2);

            // Build breadcrumbs: <org> (<env>) / Events / <event_id>
            let mut breadcrumbs = templates::build_base_breadcrumbs_with_env(&session);
            breadcrumbs.push(templates::BreadcrumbItem::clickable(
                "Events".to_string(),
                "/events".to_string(),
            ));
            breadcrumbs.push(templates::BreadcrumbItem::current(
                templates::get_uuid_short(&event_id),
            ));

            // Get unique stream IDs from runs
            let stream_ids: AHashSet<Uuid> =
                event_runs_raw.iter().map(|run| run.stream_id).collect();

            // Build stream graph data for each unique stream
            let mut stream_graphs = Vec::new();
            for stream_id in stream_ids {
                let graph_data = stream_graph::build_stream_graph(
                    &db,
                    &stream_id,
                    stream_graph::FocusElement::Event(event_id),
                )
                .await;

                let graph_data_json = serde_json::to_string(&graph_data).unwrap_or_else(|e| {
                    tracing::error!(
                        "Failed to serialize graph data for stream {}: {}",
                        stream_id,
                        e
                    );
                    "{}".to_string()
                });

                stream_graphs.push(templates::StreamGraphData {
                    stream_id,
                    stream_id_short: templates::get_uuid_short(&stream_id),
                    graph_data,
                    graph_data_json,
                });
            }

            // Fetch access attribution info if available
            let access_info = if let Some(access_id) = event.access_id {
                match hot::db::access::Access::get_access(&db, &access_id).await {
                    Ok(access) => {
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

            let template = templates::EventDetail {
                title: &templates::get_uuid_short(&event_id),
                page_context: templates::PrivatePageContext::with_breadcrumbs(
                    "events",
                    &session,
                    breadcrumbs,
                ),
                event: templates::EventDisplay::from_with_timezone(
                    &event,
                    &session.display_timezone,
                    &session.timezone_abbreviation,
                ),
                event_runs,
                stream_graphs,
                current_page_num,
                total_pages,
                start_page,
                end_page,
                has_next_page,
                has_prev_page,
                access_info,
            };

            Html(template.render().unwrap()).into_response()
        }
        Err(_) => {
            // Event not found, redirect to events list
            Redirect::to("/events").into_response()
        }
    }
}

pub async fn event_detail_table_handler(
    Path(event_id): Path<Uuid>,
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

    // SECURITY: Verify the event belongs to the current environment
    match hot::db::Event::get_event(&db, &event_id).await {
        Ok(event) => {
            if event.env_id != env_id {
                return Html("<div>Event not found</div>".to_string()).into_response();
            }
        }
        Err(_) => {
            return Html("<div>Event not found</div>".to_string()).into_response();
        }
    }

    // Parse query parameters
    let current_page_num = params
        .get("p")
        .and_then(|p| p.parse::<i64>().ok())
        .unwrap_or(1);

    // Calculate offset
    let offset = (current_page_num - 1) * EVENTS_PER_PAGE;

    // Get event runs
    let event_runs_raw =
        hot::db::Event::get_runs_by_event(&db, &event_id, Some(EVENTS_PER_PAGE), Some(offset))
            .await
            .unwrap_or_else(|e| {
                tracing::error!("Failed to get runs by event {} for table: {}", event_id, e);
                Vec::new()
            });

    // Convert runs to RunDisplay format for consistent formatting
    let event_runs: Vec<templates::RunDisplay> = event_runs_raw
        .iter()
        .map(|run| {
            templates::RunDisplay::from_with_timezone(
                run,
                &session.display_timezone,
                &session.timezone_abbreviation,
            )
        })
        .collect();

    // Get total count for pagination
    let total_count = hot::db::Event::get_run_count_by_event(&db, &event_id)
        .await
        .unwrap_or_else(|e| {
            tracing::error!(
                "Failed to get run count for event {} in table handler: {}",
                event_id,
                e
            );
            0
        });

    // Calculate pagination info
    let total_pages = (total_count + EVENTS_PER_PAGE - 1) / EVENTS_PER_PAGE;
    let has_next_page = current_page_num < total_pages;
    let has_prev_page = current_page_num > 1;

    // Calculate pagination window
    let start_page = std::cmp::max(1, current_page_num - 2);
    let end_page = std::cmp::min(total_pages, current_page_num + 2);

    let table_template = templates::EventDetailTable {
        event_id,
        event_runs,
        current_page_num,
        total_pages,
        start_page,
        end_page,
        has_next_page,
        has_prev_page,
    };

    Html(table_template.render().unwrap()).into_response()
}

// JSON API endpoint for getting event details
pub async fn event_json_handler(
    Path(event_id): Path<Uuid>,
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

    match hot::db::Event::get_event(&db, &event_id).await {
        Ok(event) => {
            // SECURITY: Verify the event belongs to the current environment
            if event.env_id != env_id {
                return Json(json!({
                    "error": "Event not found"
                }));
            }

            Json(json!({
                "event_id": event.event_id,
                "event_type": event.event_type,
                "event_data": event.event_data,
                "event_time": event.event_time,
                "created_at": event.created_at,
            }))
        }
        Err(e) => {
            tracing::error!("Failed to get event {}: {}", event_id, e);
            Json(json!({
                "error": "Event not found"
            }))
        }
    }
}

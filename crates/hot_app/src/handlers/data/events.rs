use crate::auth::Session;
use ahash::AHashMap;
use axum::extract::{Query, State};
use axum::response::{IntoResponse, Json};
use chrono::{DateTime, Utc};
use hot::db::{DatabasePool, Event, Run};
use serde::{Deserialize, Serialize};
use sqlx;
use std::sync::Arc;

/// GET /data/event-timeline - Get event timeline data with clustering
#[derive(Deserialize)]
pub struct EventTimelineParams {
    _time_range: Option<String>,
    limit: Option<i64>,
}

#[derive(Serialize)]
pub struct EventTimelineData {
    event_types: Vec<String>,
    events: Vec<EventPoint>,
}

#[derive(Serialize)]
pub struct EventPoint {
    event_id: String,
    event_type: String,
    event_time: i64, // Unix timestamp in milliseconds
    stream_id: String,
}

pub async fn event_timeline_handler(
    Query(params): Query<EventTimelineParams>,
    State(db): State<Arc<DatabasePool>>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> impl IntoResponse {
    let env_id = match session.current_env_id() {
        Some(id) => id,
        None => {
            return Json(serde_json::json!({"error": "No environment selected"})).into_response();
        }
    };

    let limit = params.limit.unwrap_or(500);

    // Get recent events
    let events = match Event::get_events_by_env(&db, &env_id, Some(limit), None).await {
        Ok(e) => e,
        Err(e) => {
            tracing::error!("Failed to get events: {}", e);
            return Json(serde_json::json!({"error": "Failed to fetch events"})).into_response();
        }
    };

    // Get unique event types
    let mut event_types: Vec<String> = events
        .iter()
        .map(|e| e.event_type.clone())
        .collect::<ahash::AHashSet<_>>()
        .into_iter()
        .collect();
    event_types.sort();

    // Convert events to points
    let event_points: Vec<EventPoint> = events
        .iter()
        .map(|e| EventPoint {
            event_id: e.event_id.to_string(),
            event_type: e.event_type.clone(),
            event_time: e.event_time.timestamp_millis(),
            stream_id: e.stream_id.to_string(),
        })
        .collect();

    Json(EventTimelineData {
        event_types,
        events: event_points,
    })
    .into_response()
}

/// GET /data/event-run-relationships - Get chord diagram data for event-run relationships
#[derive(Serialize)]
pub struct EventRunChordData {
    nodes: Vec<ChordNode>,
    links: Vec<ChordLink>,
}

#[derive(Serialize)]
pub struct ChordNode {
    name: String,
    #[serde(rename = "itemStyle")]
    item_style: Option<NodeStyle>,
}

#[derive(Serialize)]
pub struct NodeStyle {
    color: String,
}

#[derive(Serialize)]
pub struct ChordLink {
    source: String,
    target: String,
    value: i64,
}

pub async fn event_run_relationships_handler(
    State(db): State<Arc<DatabasePool>>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> impl IntoResponse {
    let env_id = match session.current_env_id() {
        Some(id) => id,
        None => {
            return Json(serde_json::json!({"error": "No environment selected"})).into_response();
        }
    };

    // Get events
    let events = match Event::get_events_by_env(&db, &env_id, Some(1000), None).await {
        Ok(e) => e,
        Err(e) => {
            tracing::error!("Failed to get events: {}", e);
            return Json(serde_json::json!({"error": "Failed to fetch events"})).into_response();
        }
    };

    // Get runs
    let runs = match Run::get_runs_by_env(&db, &env_id, Some(1000), None).await {
        Ok(r) => r,
        Err(e) => {
            tracing::error!("Failed to get runs: {}", e);
            return Json(serde_json::json!({"error": "Failed to fetch runs"})).into_response();
        }
    };

    // Build relationship map: event_type -> run_type -> count
    let mut relationships: AHashMap<(String, String), i64> = AHashMap::new();

    for run in runs {
        if let Some(event_id) = run.event_id {
            // Find the event
            if let Some(event) = events.iter().find(|e| e.event_id == event_id) {
                let key = (event.event_type.clone(), run.run_type.clone());
                *relationships.entry(key).or_insert(0) += 1;
            }
        }
    }

    // Get unique event types and run types
    let mut event_types: Vec<String> = relationships
        .keys()
        .map(|(et, _)| et.clone())
        .collect::<ahash::AHashSet<_>>()
        .into_iter()
        .collect();
    event_types.sort();

    let mut run_types: Vec<String> = relationships
        .keys()
        .map(|(_, rt)| rt.clone())
        .collect::<ahash::AHashSet<_>>()
        .into_iter()
        .collect();
    run_types.sort();

    // Build nodes
    let mut nodes = Vec::new();

    for event_type in &event_types {
        nodes.push(ChordNode {
            name: format!("{} (event)", event_type),
            item_style: Some(NodeStyle {
                color: "#10B981".to_string(), // Green for events
            }),
        });
    }

    for run_type in &run_types {
        nodes.push(ChordNode {
            name: format!("{} (run)", run_type),
            item_style: Some(NodeStyle {
                color: "#3B82F6".to_string(), // Blue for runs
            }),
        });
    }

    // Build links
    let links: Vec<ChordLink> = relationships
        .into_iter()
        .map(|((event_type, run_type), count)| ChordLink {
            source: format!("{} (event)", event_type),
            target: format!("{} (run)", run_type),
            value: count,
        })
        .collect();

    Json(EventRunChordData { nodes, links }).into_response()
}

/// GET /data/event-activity-timeline - Get event creation timeline with handled/unhandled breakdown
#[derive(Serialize)]
pub struct EventActivityTimelineData {
    dates: Vec<String>,
    series: Vec<EventTimelineSeries>,
}

#[derive(Serialize)]
pub struct EventTimelineSeries {
    name: String,
    data: Vec<i64>,
}

pub async fn event_activity_timeline_handler(
    State(db): State<Arc<DatabasePool>>,
    Query(params): Query<AHashMap<String, String>>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> impl IntoResponse {
    let env_id = match session.current_env_id() {
        Some(id) => id,
        None => {
            return Json(EventActivityTimelineData {
                dates: vec![],
                series: vec![],
            })
            .into_response();
        }
    };

    // Parse time_range and time_unit from params
    let time_range = params
        .get("time_range")
        .map(|s| s.as_str())
        .unwrap_or("P1D");
    let time_unit = params
        .get("time_unit")
        .map(|s| s.as_str())
        .unwrap_or("hour");

    // Calculate days for interval
    let days = match time_range {
        "PT24H" | "P1D" => 1,
        "P7D" => 7,
        "P30D" => 30,
        "P90D" => 90,
        _ => 1,
    };

    let interval = if time_range == "all" {
        None
    } else {
        Some(time_range)
    };

    // Get display timezone from session
    let display_timezone = &session.display_timezone;

    // Use SQL-based bucketing with timezone awareness
    match db.as_ref() {
        DatabasePool::Postgres(pg_pool) => {
            // Use timezone-aware date truncation for Postgres
            let group_by_clause =
                crate::timezone::postgres_date_trunc(time_unit, "e.created_at", display_timezone);
            let date_format = crate::timezone::postgres_date_format(time_unit);

            let query = if let Some(_interval_str) = interval {
                format!(
                    r#"
                    SELECT
                        {} as period,
                        e.handled,
                        COUNT(*) as count
                    FROM event e
                    WHERE e.env_id = $1
                        AND e.created_at >= NOW() - ($2 || ' days')::INTERVAL
                    GROUP BY period, e.handled
                    ORDER BY period ASC
                    "#,
                    group_by_clause
                )
            } else {
                format!(
                    r#"
                    SELECT
                        {} as period,
                        e.handled,
                        COUNT(*) as count
                    FROM event e
                    WHERE e.env_id = $1
                    GROUP BY period, e.handled
                    ORDER BY period ASC
                    "#,
                    group_by_clause
                )
            };

            let mut query_builder =
                sqlx::query_as::<_, (DateTime<Utc>, bool, i64)>(&query).bind(env_id);

            if interval.is_some() {
                query_builder = query_builder.bind(days);
            }

            let results = match query_builder.fetch_all(pg_pool).await {
                Ok(r) => r,
                Err(e) => {
                    tracing::error!("Failed to fetch event timeline data: {}", e);
                    return Json(EventActivityTimelineData {
                        dates: vec![],
                        series: vec![],
                    })
                    .into_response();
                }
            };

            // Build timeline data from SQL results
            // Format dates in the user's display timezone
            let mut handled_map: AHashMap<String, i64> = AHashMap::new();
            let mut unhandled_map: AHashMap<String, i64> = AHashMap::new();

            for (period, handled, count) in results {
                // Format the date in the user's timezone for display labels
                let date_str =
                    crate::timezone::format_in_timezone(&period, display_timezone, date_format);
                if handled {
                    handled_map.insert(date_str, count);
                } else {
                    unhandled_map.insert(date_str, count);
                }
            }

            // Generate complete list of time buckets from start to now
            // This ensures we show zeros for periods with no activity
            let dates: Vec<String> = if interval.is_some() {
                crate::timezone::generate_time_buckets(time_unit, days, display_timezone)
            } else {
                // For "all" time range, just use dates from results
                let mut all_dates: std::collections::BTreeSet<String> =
                    std::collections::BTreeSet::new();
                all_dates.extend(handled_map.keys().cloned());
                all_dates.extend(unhandled_map.keys().cloned());
                all_dates.into_iter().collect()
            };

            let handled_data: Vec<i64> = dates
                .iter()
                .map(|d| *handled_map.get(d).unwrap_or(&0))
                .collect();
            let unhandled_data: Vec<i64> = dates
                .iter()
                .map(|d| *unhandled_map.get(d).unwrap_or(&0))
                .collect();

            let series = vec![
                EventTimelineSeries {
                    name: "Handled".to_string(),
                    data: handled_data,
                },
                EventTimelineSeries {
                    name: "Unhandled".to_string(),
                    data: unhandled_data,
                },
            ];

            Json(EventActivityTimelineData { dates, series }).into_response()
        }
        DatabasePool::Sqlite(sqlite_pool) => {
            // Use timezone-aware date bucketing for SQLite
            let group_by_clause =
                crate::timezone::sqlite_date_bucket(time_unit, "e.created_at", display_timezone);

            let query = if interval.is_some() {
                format!(
                    r#"
                    SELECT
                        {} as period,
                        e.handled,
                        COUNT(*) as count
                    FROM event e
                    WHERE e.env_id = ?
                        AND e.created_at >= datetime('now', '-' || ? || ' days')
                    GROUP BY period, e.handled
                    ORDER BY period ASC
                    "#,
                    group_by_clause
                )
            } else {
                format!(
                    r#"
                    SELECT
                        {} as period,
                        e.handled,
                        COUNT(*) as count
                    FROM event e
                    WHERE e.env_id = ?
                    GROUP BY period, e.handled
                    ORDER BY period ASC
                    "#,
                    group_by_clause
                )
            };

            let mut query_builder = sqlx::query_as::<_, (String, i64, i64)>(&query).bind(env_id);

            if interval.is_some() {
                query_builder = query_builder.bind(days);
            }

            let results = match query_builder.fetch_all(sqlite_pool).await {
                Ok(r) => r,
                Err(e) => {
                    tracing::error!("Failed to fetch event timeline data: {}", e);
                    return Json(EventActivityTimelineData {
                        dates: vec![],
                        series: vec![],
                    })
                    .into_response();
                }
            };

            // Build timeline data from SQL results
            let mut handled_map: AHashMap<String, i64> = AHashMap::new();
            let mut unhandled_map: AHashMap<String, i64> = AHashMap::new();

            for (period, handled, count) in results {
                if handled == 1 {
                    handled_map.insert(period, count);
                } else {
                    unhandled_map.insert(period, count);
                }
            }

            // Generate complete list of time buckets from start to now
            // This ensures we show zeros for periods with no activity
            let dates: Vec<String> = if interval.is_some() {
                crate::timezone::generate_time_buckets(time_unit, days, display_timezone)
            } else {
                // For "all" time range, just use dates from results
                let mut all_dates: std::collections::BTreeSet<String> =
                    std::collections::BTreeSet::new();
                all_dates.extend(handled_map.keys().cloned());
                all_dates.extend(unhandled_map.keys().cloned());
                all_dates.into_iter().collect()
            };

            let handled_data: Vec<i64> = dates
                .iter()
                .map(|d| *handled_map.get(d).unwrap_or(&0))
                .collect();
            let unhandled_data: Vec<i64> = dates
                .iter()
                .map(|d| *unhandled_map.get(d).unwrap_or(&0))
                .collect();

            let series = vec![
                EventTimelineSeries {
                    name: "Handled".to_string(),
                    data: handled_data,
                },
                EventTimelineSeries {
                    name: "Unhandled".to_string(),
                    data: unhandled_data,
                },
            ];

            Json(EventActivityTimelineData { dates, series }).into_response()
        }
    }
}

/// GET /data/event-type-timeline - Get event type distribution over time as line chart
#[derive(Serialize)]
pub struct EventTypeTimelineData {
    dates: Vec<String>,
    series: Vec<EventTypeSeries>,
}

#[derive(Serialize)]
pub struct EventTypeSeries {
    name: String,
    data: Vec<i64>,
}

pub async fn event_type_timeline_handler(
    State(db): State<Arc<DatabasePool>>,
    Query(params): Query<AHashMap<String, String>>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> impl IntoResponse {
    let env_id = match session.current_env_id() {
        Some(id) => id,
        None => {
            return Json(EventTypeTimelineData {
                dates: vec![],
                series: vec![],
            })
            .into_response();
        }
    };

    // Parse time_range and time_unit from params
    let time_range = params
        .get("time_range")
        .map(|s| s.as_str())
        .unwrap_or("P1D");
    let time_unit = params
        .get("time_unit")
        .map(|s| s.as_str())
        .unwrap_or("hour");

    // Calculate days for interval
    let days = match time_range {
        "PT24H" | "P1D" => 1,
        "P7D" => 7,
        "P30D" => 30,
        "P90D" => 90,
        _ => 1,
    };

    let interval = if time_range == "all" {
        None
    } else {
        Some(time_range)
    };

    // Get display timezone from session
    let display_timezone = &session.display_timezone;

    // Use SQL-based bucketing with timezone awareness
    match db.as_ref() {
        DatabasePool::Postgres(pg_pool) => {
            // Use timezone-aware date truncation for Postgres
            let group_by_clause =
                crate::timezone::postgres_date_trunc(time_unit, "e.created_at", display_timezone);
            let date_format = crate::timezone::postgres_date_format(time_unit);

            let query = if let Some(_interval_str) = interval {
                format!(
                    r#"
                    SELECT
                        {} as period,
                        e.event_type,
                        COUNT(*) as count
                    FROM event e
                    WHERE e.env_id = $1
                        AND e.created_at >= NOW() - ($2 || ' days')::INTERVAL
                    GROUP BY period, e.event_type
                    ORDER BY period ASC, e.event_type ASC
                    "#,
                    group_by_clause
                )
            } else {
                format!(
                    r#"
                    SELECT
                        {} as period,
                        e.event_type,
                        COUNT(*) as count
                    FROM event e
                    WHERE e.env_id = $1
                    GROUP BY period, e.event_type
                    ORDER BY period ASC, e.event_type ASC
                    "#,
                    group_by_clause
                )
            };

            let mut query_builder =
                sqlx::query_as::<_, (DateTime<Utc>, String, i64)>(&query).bind(env_id);

            if interval.is_some() {
                query_builder = query_builder.bind(days);
            }

            let results = match query_builder.fetch_all(pg_pool).await {
                Ok(r) => r,
                Err(e) => {
                    tracing::error!("Failed to fetch event type timeline data: {}", e);
                    return Json(EventTypeTimelineData {
                        dates: vec![],
                        series: vec![],
                    })
                    .into_response();
                }
            };

            // Pivot the data: collect event types and build data map
            let mut all_types: std::collections::BTreeSet<String> =
                std::collections::BTreeSet::new();
            let mut data_map: AHashMap<(String, String), i64> = AHashMap::new();

            for (period, event_type, count) in &results {
                // Format the date in the user's timezone for display labels
                let date_str =
                    crate::timezone::format_in_timezone(period, display_timezone, date_format);
                all_types.insert(event_type.clone());
                data_map.insert((date_str, event_type.clone()), *count);
            }

            // Generate complete list of time buckets from start to now
            // This ensures we show zeros for periods with no activity
            let dates: Vec<String> = if interval.is_some() {
                crate::timezone::generate_time_buckets(time_unit, days, display_timezone)
            } else {
                // For "all" time range, just use dates from results
                let mut all_dates: std::collections::BTreeSet<String> =
                    std::collections::BTreeSet::new();
                for (period, _, _) in results {
                    let date_str =
                        crate::timezone::format_in_timezone(&period, display_timezone, date_format);
                    all_dates.insert(date_str);
                }
                all_dates.into_iter().collect()
            };

            let event_types: Vec<String> = all_types.into_iter().collect();

            // Build series for each event type
            let series: Vec<EventTypeSeries> = event_types
                .iter()
                .map(|event_type| {
                    let data: Vec<i64> = dates
                        .iter()
                        .map(|date| {
                            *data_map
                                .get(&(date.clone(), event_type.clone()))
                                .unwrap_or(&0)
                        })
                        .collect();
                    EventTypeSeries {
                        name: event_type.clone(),
                        data,
                    }
                })
                .collect();

            Json(EventTypeTimelineData { dates, series }).into_response()
        }
        DatabasePool::Sqlite(sqlite_pool) => {
            // Use timezone-aware date bucketing for SQLite
            let group_by_clause =
                crate::timezone::sqlite_date_bucket(time_unit, "e.created_at", display_timezone);

            let query = if let Some(_interval_str) = interval {
                format!(
                    r#"
                    SELECT
                        {} as period,
                        e.event_type,
                        COUNT(*) as count
                    FROM event e
                    WHERE e.env_id = ?1
                        AND e.created_at >= datetime('now', '-' || ?2 || ' days')
                    GROUP BY period, e.event_type
                    ORDER BY period ASC, e.event_type ASC
                    "#,
                    group_by_clause
                )
            } else {
                format!(
                    r#"
                    SELECT
                        {} as period,
                        e.event_type,
                        COUNT(*) as count
                    FROM event e
                    WHERE e.env_id = ?1
                    GROUP BY period, e.event_type
                    ORDER BY period ASC, e.event_type ASC
                    "#,
                    group_by_clause
                )
            };

            let mut query_builder = sqlx::query_as::<_, (String, String, i64)>(&query).bind(env_id);

            if interval.is_some() {
                query_builder = query_builder.bind(days);
            }

            let results = match query_builder.fetch_all(sqlite_pool).await {
                Ok(r) => r,
                Err(e) => {
                    tracing::error!("Failed to fetch event type timeline data: {}", e);
                    return Json(EventTypeTimelineData {
                        dates: vec![],
                        series: vec![],
                    })
                    .into_response();
                }
            };

            // Pivot the data: collect event types and build data map
            let mut all_types: std::collections::BTreeSet<String> =
                std::collections::BTreeSet::new();
            let mut data_map: AHashMap<(String, String), i64> = AHashMap::new();

            for (period, event_type, count) in &results {
                all_types.insert(event_type.clone());
                data_map.insert((period.clone(), event_type.clone()), *count);
            }

            // Generate complete list of time buckets from start to now
            // This ensures we show zeros for periods with no activity
            let dates: Vec<String> = if interval.is_some() {
                crate::timezone::generate_time_buckets(time_unit, days, display_timezone)
            } else {
                // For "all" time range, just use dates from results
                let mut all_dates: std::collections::BTreeSet<String> =
                    std::collections::BTreeSet::new();
                for (period, _, _) in results {
                    all_dates.insert(period);
                }
                all_dates.into_iter().collect()
            };

            let event_types: Vec<String> = all_types.into_iter().collect();

            // Build series for each event type
            let series: Vec<EventTypeSeries> = event_types
                .iter()
                .map(|event_type| {
                    let data: Vec<i64> = dates
                        .iter()
                        .map(|date| {
                            *data_map
                                .get(&(date.clone(), event_type.clone()))
                                .unwrap_or(&0)
                        })
                        .collect();
                    EventTypeSeries {
                        name: event_type.clone(),
                        data,
                    }
                })
                .collect();

            Json(EventTypeTimelineData { dates, series }).into_response()
        }
    }
}

/// GET /data/event-handling-status - Get handled vs unhandled event counts
#[derive(Serialize)]
pub struct EventHandlingStatusData {
    handled: i64,
    unhandled: i64,
}

pub async fn event_handling_status_handler(
    State(db): State<Arc<DatabasePool>>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> impl IntoResponse {
    let env_id = match session.current_env_id() {
        Some(id) => id,
        None => {
            return Json(EventHandlingStatusData {
                handled: 0,
                unhandled: 0,
            })
            .into_response();
        }
    };

    // Get handled and unhandled counts
    let handled = match Event::get_handled_count_by_env(&db, &env_id).await {
        Ok(count) => count,
        Err(e) => {
            tracing::error!("Failed to get handled events count: {}", e);
            0
        }
    };

    let unhandled = match Event::get_unhandled_count_by_env(&db, &env_id).await {
        Ok(count) => count,
        Err(e) => {
            tracing::error!("Failed to get unhandled events count: {}", e);
            0
        }
    };

    Json(EventHandlingStatusData { handled, unhandled }).into_response()
}

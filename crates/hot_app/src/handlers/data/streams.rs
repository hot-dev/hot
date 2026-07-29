use crate::auth::Session;
use ahash::AHashMap;
use axum::extract::{Path, Query, State};
use axum::response::{IntoResponse, Json};
use chrono::{DateTime, Utc};
use hot::db::{Build, DatabasePool, Event, Run, Stream};
use serde::{Deserialize, Serialize};
use sqlx;
use std::sync::Arc;
use uuid::Uuid;

/// GET /data/stream-flow/{stream_id} - Get flow diagram data for a specific stream
#[derive(Serialize)]
pub struct StreamFlowNode {
    id: String,
    name: String,
    category: usize, // Index into categories array
    #[serde(rename = "symbolSize")]
    symbol_size: f64,
    value: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    label: Option<NodeLabel>,
}

#[derive(Serialize)]
pub struct NodeLabel {
    show: bool,
}

#[derive(Serialize)]
pub struct StreamFlowLink {
    source: String,
    target: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    value: Option<f64>,
}

#[derive(Serialize)]
pub struct StreamFlowData {
    nodes: Vec<StreamFlowNode>,
    links: Vec<StreamFlowLink>,
    categories: Vec<FlowCategory>,
}

#[derive(Serialize)]
pub struct FlowCategory {
    name: String,
}

pub async fn stream_flow_handler(
    Path(stream_id): Path<Uuid>,
    State(db): State<Arc<DatabasePool>>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> impl IntoResponse {
    let env_id = match session.current_env_id() {
        Some(id) => id,
        None => {
            return Json(serde_json::json!({"error": "No environment selected"})).into_response();
        }
    };

    match Stream::get_stream(&db, &stream_id).await {
        Ok(stream) if stream.env_id == env_id => {}
        Ok(_) => {
            return Json(serde_json::json!({"error": "Stream not found"})).into_response();
        }
        Err(e) => {
            tracing::error!("Failed to verify stream {} access: {}", stream_id, e);
            return Json(serde_json::json!({"error": "Stream not found"})).into_response();
        }
    }

    // Get all runs for this stream
    let runs = match Run::get_runs_by_stream(&db, &stream_id, &env_id, None, None).await {
        Ok(r) => r,
        Err(e) => {
            tracing::error!("Failed to get runs for stream {}: {}", stream_id, e);
            return Json(serde_json::json!({"error": "Failed to fetch stream data"}))
                .into_response();
        }
    };

    // Get all events for this stream
    let events = match Event::get_events_by_stream(&db, &stream_id, &env_id, None, None).await {
        Ok(e) => e,
        Err(e) => {
            tracing::error!("Failed to get events for stream {}: {}", stream_id, e);
            return Json(serde_json::json!({"error": "Failed to fetch stream data"}))
                .into_response();
        }
    };

    // Build nodes
    let mut nodes = Vec::new();
    let mut links = Vec::new();

    // Add run nodes
    for run in &runs {
        let duration_ms = if let Some(stop) = &run.stop_time {
            (stop.timestamp_millis() - run.start_time.timestamp_millis()) as f64
        } else {
            0.0
        };

        let category = match run.run_type.as_str() {
            "call" => 0,
            "event" => 1,
            "schedule" => 2,
            "run" => 3,
            "eval" => 4,
            "repl" => 5,
            _ => 6,
        };

        nodes.push(StreamFlowNode {
            id: run.run_id.to_string(),
            name: format!("{} ({})", run.run_type, &run.run_id.to_string()[..8]),
            category,
            symbol_size: 30.0 + (duration_ms / 100.0).min(70.0),
            value: duration_ms,
            label: Some(NodeLabel { show: true }),
        });

        // Create link from origin run
        if let Some(origin_run_id) = run.origin_run_id {
            links.push(StreamFlowLink {
                source: origin_run_id.to_string(),
                target: run.run_id.to_string(),
                value: Some(1.0),
            });
        }

        // Create link from event
        if let Some(event_id) = run.event_id {
            links.push(StreamFlowLink {
                source: format!("event_{}", event_id),
                target: run.run_id.to_string(),
                value: Some(1.0),
            });
        }
    }

    // Add event nodes
    for event in &events {
        nodes.push(StreamFlowNode {
            id: format!("event_{}", event.event_id),
            name: format!("{} event", event.event_type),
            category: 7, // Event category
            symbol_size: 25.0,
            value: 1.0,
            label: Some(NodeLabel { show: true }),
        });
    }

    let categories = vec![
        FlowCategory {
            name: "Call".to_string(),
        },
        FlowCategory {
            name: "Event".to_string(),
        },
        FlowCategory {
            name: "Schedule".to_string(),
        },
        FlowCategory {
            name: "Run".to_string(),
        },
        FlowCategory {
            name: "Eval".to_string(),
        },
        FlowCategory {
            name: "Repl".to_string(),
        },
        FlowCategory {
            name: "Other".to_string(),
        },
        FlowCategory {
            name: "Event".to_string(),
        },
    ];

    Json(StreamFlowData {
        nodes,
        links,
        categories,
    })
    .into_response()
}

/// GET /data/stream-timeline - Get timeline data for streams
#[derive(Deserialize)]
pub struct StreamTimelineParams {
    _time_range: Option<String>,
    limit: Option<i64>,
}

#[derive(Serialize)]
pub struct StreamTimelineData {
    series: Vec<EChartsSeries>,
}

#[derive(Serialize)]
pub struct EChartsSeries {
    name: String,
    #[serde(rename = "type")]
    series_type: String,
    data: Vec<[i64; 2]>, // [timestamp, value] pairs
}

pub async fn stream_timeline_handler(
    Query(params): Query<StreamTimelineParams>,
    State(db): State<Arc<DatabasePool>>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> impl IntoResponse {
    let env_id = match session.current_env_id() {
        Some(id) => id,
        None => {
            return Json(serde_json::json!({"error": "No environment selected"})).into_response();
        }
    };

    let limit = params.limit.unwrap_or(10);

    // Get recent streams
    let streams = match Stream::get_streams_by_env(&db, &env_id, Some(limit), None).await {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("Failed to get streams: {}", e);
            return Json(serde_json::json!({"error": "Failed to fetch streams"})).into_response();
        }
    };

    // Create series for stream creation over time
    let mut stream_series_data = Vec::new();

    for stream in streams {
        let timestamp = stream.created_at.timestamp_millis();
        stream_series_data.push([timestamp, 1]);
    }

    let series = vec![EChartsSeries {
        name: "Streams Created".to_string(),
        series_type: "scatter".to_string(),
        data: stream_series_data,
    }];

    Json(StreamTimelineData { series }).into_response()
}

/// GET /data/stream-metrics - Get aggregated stream metrics
#[derive(Serialize)]
pub struct StreamMetrics {
    total_streams: i64,
    total_events: i64,
    handled_events: i64,
    total_runs: i64,
    running_count: i64,
    succeeded_count: i64,
    failed_count: i64,
    cancelled_count: i64,
    total_tasks: i64,
    tasks_running: i64,
    tasks_completed: i64,
    tasks_failed: i64,
    total_cus: i64,
    build_version_warning_count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    build_version_warning_url: Option<String>,
}

pub async fn stream_metrics_handler(
    State(db): State<Arc<DatabasePool>>,
    Query(params): Query<AHashMap<String, String>>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> impl IntoResponse {
    let env_id = match session.current_env_id() {
        Some(id) => id,
        None => {
            return Json(serde_json::json!({"error": "No environment selected"})).into_response();
        }
    };

    let time_range = params
        .get("time_range")
        .map(|s| s.as_str())
        .unwrap_or("P1D");
    let project_id = params
        .get("project_id")
        .and_then(|p| uuid::Uuid::parse_str(p).ok());

    let days: Option<i64> = match time_range {
        "PT24H" | "P1D" => Some(1),
        "P7D" => Some(7),
        "P30D" => Some(30),
        "P90D" => Some(90),
        "all" => None,
        _ => Some(1),
    };

    let build_version_warning_count = match Build::get_deployed_ready_bundle_builds_by_env(
        &db,
        &env_id,
        project_id.as_ref(),
        1_000,
    )
    .await
    {
        Ok(builds) => builds
            .iter()
            .filter(|build| build.runtime_version_warning().is_some())
            .count(),
        Err(e) => {
            tracing::warn!(
                "Failed to get deployed bundle builds for version warning count: {}",
                e
            );
            0
        }
    };
    let build_version_warning_url = if build_version_warning_count > 0 {
        Some("/projects".to_string())
    } else {
        None
    };

    match db.as_ref() {
        DatabasePool::Postgres(pg_pool) => {
            let time_clause_run = if let Some(d) = days {
                format!("AND r.start_time >= NOW() - ('{} days')::INTERVAL", d)
            } else {
                String::new()
            };
            let time_clause_event = if let Some(d) = days {
                format!("AND e.created_at >= NOW() - ('{} days')::INTERVAL", d)
            } else {
                String::new()
            };
            let time_clause_stream = if let Some(d) = days {
                format!("AND s.created_at >= NOW() - ('{} days')::INTERVAL", d)
            } else {
                String::new()
            };
            let time_clause_task = if let Some(d) = days {
                format!("AND t.created_at >= NOW() - ('{} days')::INTERVAL", d)
            } else {
                String::new()
            };

            let (project_join_run, project_clause_run) = if project_id.is_some() {
                (
                    "JOIN build b ON r.build_id = b.build_id",
                    "AND b.project_id = $2",
                )
            } else {
                ("", "")
            };
            let (project_join_task, project_clause_task) = if project_id.is_some() {
                (
                    "JOIN build bt ON t.build_id = bt.build_id",
                    "AND bt.project_id = $2",
                )
            } else {
                ("", "")
            };

            // Runs: total + per-status counts in a single query
            let run_query = format!(
                r#"SELECT
                    COUNT(*) as total,
                    COUNT(*) FILTER (WHERE r.status_id = 1) as running,
                    COUNT(*) FILTER (WHERE r.status_id = 2) as succeeded,
                    COUNT(*) FILTER (WHERE r.status_id = 3) as failed,
                    COUNT(*) FILTER (WHERE r.status_id = 4) as cancelled
                FROM run r
                {project_join_run}
                WHERE r.env_id = $1 AND r.run_type_id != 7 {project_clause_run} {time_clause_run}"#,
            );
            let mut run_qb = sqlx::query_as(sqlx::AssertSqlSafe(run_query.as_str())).bind(env_id);
            if let Some(ref pid) = project_id {
                run_qb = run_qb.bind(pid);
            }
            let (total_runs, running_count, succeeded_count, failed_count, cancelled_count): (
                i64,
                i64,
                i64,
                i64,
                i64,
            ) = run_qb.fetch_one(pg_pool).await.unwrap_or_else(|e| {
                tracing::error!("Failed to get run metrics: {}", e);
                (0, 0, 0, 0, 0)
            });

            let event_project_clause = if project_id.is_some() {
                "AND EXISTS (
                    SELECT 1
                    FROM run project_run
                    JOIN build project_build ON project_build.build_id = project_run.build_id
                    WHERE project_run.event_id = e.event_id
                      AND project_run.env_id = e.env_id
                      AND project_build.project_id = $2
                )"
            } else {
                ""
            };

            // A project-scoped event means an event handled by that project.
            let event_query = format!(
                r#"SELECT
                    COUNT(*) as total,
                    COUNT(*) FILTER (WHERE EXISTS (
                        SELECT 1 FROM run r2
                        WHERE r2.event_id = e.event_id AND r2.env_id = e.env_id
                    )) as handled
                FROM event e
                WHERE e.env_id = $1 {} {}"#,
                time_clause_event, event_project_clause
            );
            let mut event_qb =
                sqlx::query_as(sqlx::AssertSqlSafe(event_query.as_str())).bind(env_id);
            if let Some(ref pid) = project_id {
                event_qb = event_qb.bind(pid);
            }
            let (total_events, handled_events): (i64, i64) =
                event_qb.fetch_one(pg_pool).await.unwrap_or_else(|e| {
                    tracing::error!("Failed to get event metrics: {}", e);
                    (0, 0)
                });

            let stream_project_clause = if project_id.is_some() {
                "AND (
                    EXISTS (
                        SELECT 1
                        FROM run project_run
                        JOIN build run_build ON run_build.build_id = project_run.build_id
                        WHERE project_run.stream_id = s.stream_id
                          AND project_run.env_id = s.env_id
                          AND run_build.project_id = $2
                    )
                    OR EXISTS (
                        SELECT 1
                        FROM task project_task
                        JOIN build task_build ON task_build.build_id = project_task.build_id
                        WHERE project_task.stream_id = s.stream_id
                          AND project_task.env_id = s.env_id
                          AND task_build.project_id = $2
                    )
                )"
            } else {
                ""
            };
            let stream_query = format!(
                "SELECT COUNT(*) FROM stream s WHERE s.env_id = $1 {} {}",
                time_clause_stream, stream_project_clause
            );
            let mut stream_qb =
                sqlx::query_scalar(sqlx::AssertSqlSafe(stream_query.as_str())).bind(env_id);
            if let Some(ref pid) = project_id {
                stream_qb = stream_qb.bind(pid);
            }
            let total_streams: i64 = stream_qb.fetch_one(pg_pool).await.unwrap_or_else(|e| {
                tracing::error!("Failed to get stream count: {}", e);
                0
            });

            // Tasks: total + per-status + CUS
            let task_query = format!(
                r#"SELECT
                    COUNT(*) as total,
                    COUNT(*) FILTER (WHERE t.task_status_id = 2) as running,
                    COUNT(*) FILTER (WHERE t.task_status_id = 3) as completed,
                    COUNT(*) FILTER (WHERE t.task_status_id = 4) as failed,
                    COALESCE(SUM(CASE
                        WHEN t.result->'$val'->'err'->>'compute-units' IS NOT NULL
                            THEN (t.result->'$val'->'err'->>'compute-units')::bigint
                        WHEN t.result->>'compute-units' IS NOT NULL
                            THEN (t.result->>'compute-units')::bigint
                        ELSE 0
                    END), 0)::bigint as cus
                FROM task t
                {project_join_task}
                WHERE t.env_id = $1 {project_clause_task} {time_clause_task}"#,
            );
            let mut task_qb = sqlx::query_as(sqlx::AssertSqlSafe(task_query.as_str())).bind(env_id);
            if let Some(ref pid) = project_id {
                task_qb = task_qb.bind(pid);
            }
            let (total_tasks, tasks_running, tasks_completed, tasks_failed, total_cus): (
                i64,
                i64,
                i64,
                i64,
                i64,
            ) = task_qb.fetch_one(pg_pool).await.unwrap_or_else(|e| {
                tracing::error!("Failed to get task metrics: {}", e);
                (0, 0, 0, 0, 0)
            });

            Json(StreamMetrics {
                total_streams,
                total_events,
                handled_events,
                total_runs,
                running_count,
                succeeded_count,
                failed_count,
                cancelled_count,
                total_tasks,
                tasks_running,
                tasks_completed,
                tasks_failed,
                total_cus,
                build_version_warning_count,
                build_version_warning_url: build_version_warning_url.clone(),
            })
            .into_response()
        }
        DatabasePool::Sqlite(sqlite_pool) => {
            let time_clause_run = if let Some(d) = days {
                format!("AND r.start_time >= datetime('now', '-{} days')", d)
            } else {
                String::new()
            };
            let time_clause_event = if let Some(d) = days {
                format!("AND e.created_at >= datetime('now', '-{} days')", d)
            } else {
                String::new()
            };
            let time_clause_stream = if let Some(d) = days {
                format!("AND s.created_at >= datetime('now', '-{} days')", d)
            } else {
                String::new()
            };
            let time_clause_task = if let Some(d) = days {
                format!("AND t.created_at >= datetime('now', '-{} days')", d)
            } else {
                String::new()
            };

            let (project_join_run, project_clause_run) = if project_id.is_some() {
                (
                    "JOIN build b ON r.build_id = b.build_id",
                    "AND b.project_id = ?",
                )
            } else {
                ("", "")
            };
            let (project_join_task, project_clause_task) = if project_id.is_some() {
                (
                    "JOIN build bt ON t.build_id = bt.build_id",
                    "AND bt.project_id = ?",
                )
            } else {
                ("", "")
            };

            let run_query = format!(
                r#"SELECT
                    COUNT(*) as total,
                    SUM(CASE WHEN r.status_id = 1 THEN 1 ELSE 0 END) as running,
                    SUM(CASE WHEN r.status_id = 2 THEN 1 ELSE 0 END) as succeeded,
                    SUM(CASE WHEN r.status_id = 3 THEN 1 ELSE 0 END) as failed,
                    SUM(CASE WHEN r.status_id = 4 THEN 1 ELSE 0 END) as cancelled
                FROM run r
                {project_join_run}
                WHERE r.env_id = ? AND r.run_type_id != 7 {project_clause_run} {time_clause_run}"#,
            );
            let mut run_qb = sqlx::query_as(sqlx::AssertSqlSafe(run_query.as_str())).bind(env_id);
            if let Some(ref pid) = project_id {
                run_qb = run_qb.bind(pid);
            }
            let (total_runs, running_count, succeeded_count, failed_count, cancelled_count): (
                i64,
                i64,
                i64,
                i64,
                i64,
            ) = run_qb.fetch_one(sqlite_pool).await.unwrap_or_else(|e| {
                tracing::error!("Failed to get run metrics (sqlite): {}", e);
                (0, 0, 0, 0, 0)
            });

            let event_project_clause = if project_id.is_some() {
                "AND EXISTS (
                    SELECT 1
                    FROM run project_run
                    JOIN build project_build ON project_build.build_id = project_run.build_id
                    WHERE project_run.event_id = e.event_id
                      AND project_run.env_id = e.env_id
                      AND project_build.project_id = ?
                )"
            } else {
                ""
            };
            let event_query = format!(
                r#"SELECT
                    COUNT(*) as total,
                    SUM(CASE WHEN EXISTS (
                        SELECT 1 FROM run r2
                        WHERE r2.event_id = e.event_id AND r2.env_id = e.env_id
                    ) THEN 1 ELSE 0 END) as handled
                FROM event e
                WHERE e.env_id = ? {} {}"#,
                time_clause_event, event_project_clause
            );
            let mut event_qb =
                sqlx::query_as(sqlx::AssertSqlSafe(event_query.as_str())).bind(env_id);
            if let Some(ref pid) = project_id {
                event_qb = event_qb.bind(pid);
            }
            let (total_events, handled_events): (i64, i64) =
                event_qb.fetch_one(sqlite_pool).await.unwrap_or_else(|e| {
                    tracing::error!("Failed to get event metrics (sqlite): {}", e);
                    (0, 0)
                });

            let stream_project_clause = if project_id.is_some() {
                "AND (
                    EXISTS (
                        SELECT 1
                        FROM run project_run
                        JOIN build run_build ON run_build.build_id = project_run.build_id
                        WHERE project_run.stream_id = s.stream_id
                          AND project_run.env_id = s.env_id
                          AND run_build.project_id = ?
                    )
                    OR EXISTS (
                        SELECT 1
                        FROM task project_task
                        JOIN build task_build ON task_build.build_id = project_task.build_id
                        WHERE project_task.stream_id = s.stream_id
                          AND project_task.env_id = s.env_id
                          AND task_build.project_id = ?
                    )
                )"
            } else {
                ""
            };
            let stream_query = format!(
                "SELECT COUNT(*) FROM stream s WHERE s.env_id = ? {} {}",
                time_clause_stream, stream_project_clause
            );
            let mut stream_qb =
                sqlx::query_scalar(sqlx::AssertSqlSafe(stream_query.as_str())).bind(env_id);
            if let Some(ref pid) = project_id {
                // SQLite uses positional anonymous parameters here; bind once
                // for each EXISTS branch.
                stream_qb = stream_qb.bind(pid).bind(pid);
            }
            let total_streams: i64 = stream_qb.fetch_one(sqlite_pool).await.unwrap_or_else(|e| {
                tracing::error!("Failed to get stream count (sqlite): {}", e);
                0
            });

            let task_query = format!(
                r#"SELECT
                    COUNT(*) as total,
                    SUM(CASE WHEN t.task_status_id = 2 THEN 1 ELSE 0 END) as running,
                    SUM(CASE WHEN t.task_status_id = 3 THEN 1 ELSE 0 END) as completed,
                    SUM(CASE WHEN t.task_status_id = 4 THEN 1 ELSE 0 END) as failed,
                    COALESCE(SUM(CASE
                        WHEN json_extract(t.result, '$."$val"."err"."compute-units"') IS NOT NULL
                            THEN CAST(json_extract(t.result, '$."$val"."err"."compute-units"') AS INTEGER)
                        WHEN json_extract(t.result, '$."compute-units"') IS NOT NULL
                            THEN CAST(json_extract(t.result, '$."compute-units"') AS INTEGER)
                        ELSE 0
                    END), 0) as cus
                FROM task t
                {project_join_task}
                WHERE t.env_id = ? {project_clause_task} {time_clause_task}"#,
            );
            let mut task_qb = sqlx::query_as(sqlx::AssertSqlSafe(task_query.as_str())).bind(env_id);
            if let Some(ref pid) = project_id {
                task_qb = task_qb.bind(pid);
            }
            let (total_tasks, tasks_running, tasks_completed, tasks_failed, total_cus): (
                i64,
                i64,
                i64,
                i64,
                i64,
            ) = task_qb.fetch_one(sqlite_pool).await.unwrap_or_else(|e| {
                tracing::error!("Failed to get task metrics (sqlite): {}", e);
                (0, 0, 0, 0, 0)
            });

            Json(StreamMetrics {
                total_streams,
                total_events,
                handled_events,
                total_runs,
                running_count,
                succeeded_count,
                failed_count,
                cancelled_count,
                total_tasks,
                tasks_running,
                tasks_completed,
                tasks_failed,
                total_cus,
                build_version_warning_count,
                build_version_warning_url,
            })
            .into_response()
        }
    }
}

/// GET /data/stream-activity-timeline - Get stream creation timeline
#[derive(Serialize)]
pub struct StreamActivityTimelineData {
    dates: Vec<String>,
    count: Vec<i64>,
}

pub async fn stream_activity_timeline_handler(
    State(db): State<Arc<DatabasePool>>,
    Query(params): Query<AHashMap<String, String>>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> impl IntoResponse {
    let env_id = match session.current_env_id() {
        Some(id) => id,
        None => {
            return Json(StreamActivityTimelineData {
                dates: vec![],
                count: vec![],
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
    let project_id = params
        .get("project_id")
        .and_then(|value| Uuid::parse_str(value).ok());

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
                crate::timezone::postgres_date_trunc(time_unit, "s.created_at", display_timezone);
            let date_format = crate::timezone::postgres_date_format(time_unit);
            let project_clause = if project_id.is_some() {
                let placeholder = if interval.is_some() { "$3" } else { "$2" };
                format!(
                    "AND (
                        EXISTS (
                            SELECT 1
                            FROM run project_run
                            JOIN build run_build ON run_build.build_id = project_run.build_id
                            WHERE project_run.stream_id = s.stream_id
                              AND project_run.env_id = s.env_id
                              AND run_build.project_id = {0}
                        )
                        OR EXISTS (
                            SELECT 1
                            FROM task project_task
                            JOIN build task_build ON task_build.build_id = project_task.build_id
                            WHERE project_task.stream_id = s.stream_id
                              AND project_task.env_id = s.env_id
                              AND task_build.project_id = {0}
                        )
                    )",
                    placeholder
                )
            } else {
                String::new()
            };

            let query = if let Some(_interval_str) = interval {
                format!(
                    r#"
                    SELECT
                        {} as period,
                        COUNT(*) as count
                    FROM stream s
                    WHERE s.env_id = $1
                        AND s.created_at >= NOW() - ($2 || ' days')::INTERVAL
                        {}
                    GROUP BY period
                    ORDER BY period ASC
                    "#,
                    group_by_clause, project_clause
                )
            } else {
                format!(
                    r#"
                    SELECT
                        {} as period,
                        COUNT(*) as count
                    FROM stream s
                    WHERE s.env_id = $1
                        {}
                    GROUP BY period
                    ORDER BY period ASC
                    "#,
                    group_by_clause, project_clause
                )
            };

            let mut query_builder =
                sqlx::query_as::<_, (DateTime<Utc>, i64)>(sqlx::AssertSqlSafe(query.as_str()))
                    .bind(env_id);

            if interval.is_some() {
                query_builder = query_builder.bind(days);
            }
            if let Some(project_id) = project_id {
                query_builder = query_builder.bind(project_id);
            }

            let results = match query_builder.fetch_all(pg_pool).await {
                Ok(r) => r,
                Err(e) => {
                    tracing::error!("Failed to fetch stream timeline data: {}", e);
                    return Json(StreamActivityTimelineData {
                        dates: vec![],
                        count: vec![],
                    })
                    .into_response();
                }
            };

            // Build data map from SQL results with timezone-aware formatting
            let mut count_map: AHashMap<String, i64> = AHashMap::new();

            for (dt, c) in results {
                let date_str =
                    crate::timezone::format_in_timezone(&dt, display_timezone, date_format);
                count_map.insert(date_str, c);
            }

            // Generate complete list of time buckets from start to now
            // This ensures we show zeros for periods with no activity
            let dates: Vec<String> = if interval.is_some() {
                crate::timezone::generate_time_buckets(time_unit, days, display_timezone)
            } else {
                // For "all" time range, just use dates from results
                let mut all_dates: std::collections::BTreeSet<String> =
                    std::collections::BTreeSet::new();
                all_dates.extend(count_map.keys().cloned());
                all_dates.into_iter().collect()
            };

            let count: Vec<i64> = dates
                .iter()
                .map(|d| *count_map.get(d).unwrap_or(&0))
                .collect();

            Json(StreamActivityTimelineData { dates, count }).into_response()
        }
        DatabasePool::Sqlite(sqlite_pool) => {
            // Use timezone-aware date bucketing for SQLite
            let group_by_clause =
                crate::timezone::sqlite_date_bucket(time_unit, "s.created_at", display_timezone);
            let project_placeholder = if interval.is_some() { "?3" } else { "?2" };
            let project_clause = if project_id.is_some() {
                format!(
                    "AND (
                        EXISTS (
                            SELECT 1
                            FROM run project_run
                            JOIN build run_build ON run_build.build_id = project_run.build_id
                            WHERE project_run.stream_id = s.stream_id
                              AND project_run.env_id = s.env_id
                              AND run_build.project_id = {0}
                        )
                        OR EXISTS (
                            SELECT 1
                            FROM task project_task
                            JOIN build task_build ON task_build.build_id = project_task.build_id
                            WHERE project_task.stream_id = s.stream_id
                              AND project_task.env_id = s.env_id
                              AND task_build.project_id = {0}
                        )
                    )",
                    project_placeholder
                )
            } else {
                String::new()
            };

            let query = if let Some(_interval_str) = interval {
                format!(
                    r#"
                    SELECT
                        {} as period,
                        COUNT(*) as count
                    FROM stream s
                    WHERE s.env_id = ?1
                        AND s.created_at >= datetime('now', '-' || ?2 || ' days')
                        {}
                    GROUP BY period
                    ORDER BY period ASC
                    "#,
                    group_by_clause, project_clause
                )
            } else {
                format!(
                    r#"
                    SELECT
                        {} as period,
                        COUNT(*) as count
                    FROM stream s
                    WHERE s.env_id = ?1
                        {}
                    GROUP BY period
                    ORDER BY period ASC
                    "#,
                    group_by_clause, project_clause
                )
            };

            let mut query_builder =
                sqlx::query_as::<_, (String, i64)>(sqlx::AssertSqlSafe(query.as_str()))
                    .bind(env_id);

            if interval.is_some() {
                query_builder = query_builder.bind(days);
            }
            if let Some(project_id) = project_id {
                query_builder = query_builder.bind(project_id);
            }

            let results = match query_builder.fetch_all(sqlite_pool).await {
                Ok(r) => r,
                Err(e) => {
                    tracing::error!("Failed to fetch stream timeline data: {}", e);
                    return Json(StreamActivityTimelineData {
                        dates: vec![],
                        count: vec![],
                    })
                    .into_response();
                }
            };

            // Build data map from SQL results
            let mut count_map: AHashMap<String, i64> = AHashMap::new();

            for (period, c) in results {
                count_map.insert(period, c);
            }

            // Generate complete list of time buckets from start to now
            // This ensures we show zeros for periods with no activity
            let dates: Vec<String> = if interval.is_some() {
                crate::timezone::generate_time_buckets(time_unit, days, display_timezone)
            } else {
                // For "all" time range, just use dates from results
                let mut all_dates: std::collections::BTreeSet<String> =
                    std::collections::BTreeSet::new();
                all_dates.extend(count_map.keys().cloned());
                all_dates.into_iter().collect()
            };

            let count: Vec<i64> = dates
                .iter()
                .map(|d| *count_map.get(d).unwrap_or(&0))
                .collect();

            Json(StreamActivityTimelineData { dates, count }).into_response()
        }
    }
}

/// GET /data/stream-composition - Get avg runs and events per stream over time
#[derive(Serialize)]
pub struct StreamCompositionData {
    dates: Vec<String>,
    avg_runs: Vec<f64>,
    avg_events: Vec<f64>,
}

pub async fn stream_composition_handler(
    State(db): State<Arc<DatabasePool>>,
    Query(params): Query<AHashMap<String, String>>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> impl IntoResponse {
    let env_id = match session.current_env_id() {
        Some(id) => id,
        None => {
            return Json(StreamCompositionData {
                dates: vec![],
                avg_runs: vec![],
                avg_events: vec![],
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
    let project_id = params
        .get("project_id")
        .and_then(|value| Uuid::parse_str(value).ok());

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

    // Use SQL-based bucketing and aggregation with timezone awareness
    match db.as_ref() {
        DatabasePool::Postgres(pg_pool) => {
            // Use timezone-aware date truncation for Postgres
            let group_by_clause =
                crate::timezone::postgres_date_trunc(time_unit, "s.created_at", display_timezone);
            let date_format = crate::timezone::postgres_date_format(time_unit);

            let query = if project_id.is_some() {
                let project_placeholder = if interval.is_some() { "$3" } else { "$2" };
                let time_clause = if interval.is_some() {
                    "AND s.created_at >= NOW() - ($2 || ' days')::INTERVAL"
                } else {
                    ""
                };
                format!(
                    r#"
                    WITH project_streams AS (
                        SELECT
                            s.created_at,
                            (
                                SELECT COUNT(*)
                                FROM run project_run
                                JOIN build run_build ON run_build.build_id = project_run.build_id
                                WHERE project_run.stream_id = s.stream_id
                                  AND project_run.env_id = s.env_id
                                  AND run_build.project_id = {1}
                            ) AS total_runs,
                            (
                                SELECT COUNT(DISTINCT project_run.event_id)
                                FROM run project_run
                                JOIN build run_build ON run_build.build_id = project_run.build_id
                                WHERE project_run.stream_id = s.stream_id
                                  AND project_run.env_id = s.env_id
                                  AND run_build.project_id = {1}
                            ) AS total_events
                        FROM stream s
                        WHERE s.env_id = $1
                            {2}
                            AND (
                                EXISTS (
                                    SELECT 1
                                    FROM run project_run
                                    JOIN build run_build ON run_build.build_id = project_run.build_id
                                    WHERE project_run.stream_id = s.stream_id
                                      AND project_run.env_id = s.env_id
                                      AND run_build.project_id = {1}
                                )
                                OR EXISTS (
                                    SELECT 1
                                    FROM task project_task
                                    JOIN build task_build ON task_build.build_id = project_task.build_id
                                    WHERE project_task.stream_id = s.stream_id
                                      AND project_task.env_id = s.env_id
                                      AND task_build.project_id = {1}
                                )
                            )
                    )
                    SELECT
                        {0} as period,
                        CAST(AVG(s.total_runs) AS FLOAT8) as avg_runs,
                        CAST(AVG(s.total_events) AS FLOAT8) as avg_events
                    FROM project_streams s
                    GROUP BY period
                    ORDER BY period ASC
                    "#,
                    group_by_clause, project_placeholder, time_clause
                )
            } else if let Some(_interval_str) = interval {
                format!(
                    r#"
                    SELECT
                        {} as period,
                        CAST(AVG(s.total_runs) AS FLOAT8) as avg_runs,
                        CAST(AVG(s.total_events) AS FLOAT8) as avg_events
                    FROM stream s
                    WHERE s.env_id = $1
                        AND s.created_at >= NOW() - ($2 || ' days')::INTERVAL
                    GROUP BY period
                    ORDER BY period ASC
                    "#,
                    group_by_clause
                )
            } else {
                format!(
                    r#"
                    SELECT
                        {} as period,
                        CAST(AVG(s.total_runs) AS FLOAT8) as avg_runs,
                        CAST(AVG(s.total_events) AS FLOAT8) as avg_events
                    FROM stream s
                    WHERE s.env_id = $1
                    GROUP BY period
                    ORDER BY period ASC
                    "#,
                    group_by_clause
                )
            };

            let mut query_builder = sqlx::query_as::<_, (DateTime<Utc>, Option<f64>, Option<f64>)>(
                sqlx::AssertSqlSafe(query.as_str()),
            )
            .bind(env_id);

            if interval.is_some() {
                query_builder = query_builder.bind(days);
            }
            if let Some(project_id) = project_id {
                query_builder = query_builder.bind(project_id);
            }

            let results = match query_builder.fetch_all(pg_pool).await {
                Ok(r) => r,
                Err(e) => {
                    tracing::error!("Failed to fetch stream composition data: {}", e);
                    return Json(StreamCompositionData {
                        dates: vec![],
                        avg_runs: vec![],
                        avg_events: vec![],
                    })
                    .into_response();
                }
            };

            // Build data maps from SQL results with timezone-aware formatting
            let mut runs_map: AHashMap<String, f64> = AHashMap::new();
            let mut events_map: AHashMap<String, f64> = AHashMap::new();

            for (dt, r, e) in results {
                let date_str =
                    crate::timezone::format_in_timezone(&dt, display_timezone, date_format);
                runs_map.insert(date_str.clone(), r.unwrap_or(0.0));
                events_map.insert(date_str, e.unwrap_or(0.0));
            }

            // Generate complete list of time buckets from start to now
            // This ensures we show zeros for periods with no activity
            let dates: Vec<String> = if interval.is_some() {
                crate::timezone::generate_time_buckets(time_unit, days, display_timezone)
            } else {
                // For "all" time range, just use dates from results
                let mut all_dates: std::collections::BTreeSet<String> =
                    std::collections::BTreeSet::new();
                all_dates.extend(runs_map.keys().cloned());
                all_dates.into_iter().collect()
            };

            let avg_runs: Vec<f64> = dates
                .iter()
                .map(|d| *runs_map.get(d).unwrap_or(&0.0))
                .collect();
            let avg_events: Vec<f64> = dates
                .iter()
                .map(|d| *events_map.get(d).unwrap_or(&0.0))
                .collect();

            Json(StreamCompositionData {
                dates,
                avg_runs,
                avg_events,
            })
            .into_response()
        }
        DatabasePool::Sqlite(sqlite_pool) => {
            // Use timezone-aware date bucketing for SQLite
            let group_by_clause =
                crate::timezone::sqlite_date_bucket(time_unit, "s.created_at", display_timezone);

            let query = if project_id.is_some() {
                let project_placeholder = if interval.is_some() { "?3" } else { "?2" };
                let time_clause = if interval.is_some() {
                    "AND s.created_at >= datetime('now', '-' || ?2 || ' days')"
                } else {
                    ""
                };
                format!(
                    r#"
                    WITH project_streams AS (
                        SELECT
                            s.created_at,
                            (
                                SELECT COUNT(*)
                                FROM run project_run
                                JOIN build run_build ON run_build.build_id = project_run.build_id
                                WHERE project_run.stream_id = s.stream_id
                                  AND project_run.env_id = s.env_id
                                  AND run_build.project_id = {1}
                            ) AS total_runs,
                            (
                                SELECT COUNT(DISTINCT project_run.event_id)
                                FROM run project_run
                                JOIN build run_build ON run_build.build_id = project_run.build_id
                                WHERE project_run.stream_id = s.stream_id
                                  AND project_run.env_id = s.env_id
                                  AND run_build.project_id = {1}
                            ) AS total_events
                        FROM stream s
                        WHERE s.env_id = ?1
                            {2}
                            AND (
                                EXISTS (
                                    SELECT 1
                                    FROM run project_run
                                    JOIN build run_build ON run_build.build_id = project_run.build_id
                                    WHERE project_run.stream_id = s.stream_id
                                      AND project_run.env_id = s.env_id
                                      AND run_build.project_id = {1}
                                )
                                OR EXISTS (
                                    SELECT 1
                                    FROM task project_task
                                    JOIN build task_build ON task_build.build_id = project_task.build_id
                                    WHERE project_task.stream_id = s.stream_id
                                      AND project_task.env_id = s.env_id
                                      AND task_build.project_id = {1}
                                )
                            )
                    )
                    SELECT
                        {0} as period,
                        AVG(s.total_runs) as avg_runs,
                        AVG(s.total_events) as avg_events
                    FROM project_streams s
                    GROUP BY period
                    ORDER BY period ASC
                    "#,
                    group_by_clause, project_placeholder, time_clause
                )
            } else if let Some(_interval_str) = interval {
                format!(
                    r#"
                    SELECT
                        {} as period,
                        AVG(s.total_runs) as avg_runs,
                        AVG(s.total_events) as avg_events
                    FROM stream s
                    WHERE s.env_id = ?1
                        AND s.created_at >= datetime('now', '-' || ?2 || ' days')
                    GROUP BY period
                    ORDER BY period ASC
                    "#,
                    group_by_clause
                )
            } else {
                format!(
                    r#"
                    SELECT
                        {} as period,
                        AVG(s.total_runs) as avg_runs,
                        AVG(s.total_events) as avg_events
                    FROM stream s
                    WHERE s.env_id = ?1
                    GROUP BY period
                    ORDER BY period ASC
                    "#,
                    group_by_clause
                )
            };

            let mut query_builder = sqlx::query_as::<_, (String, Option<f64>, Option<f64>)>(
                sqlx::AssertSqlSafe(query.as_str()),
            )
            .bind(env_id);

            if interval.is_some() {
                query_builder = query_builder.bind(days);
            }
            if let Some(project_id) = project_id {
                query_builder = query_builder.bind(project_id);
            }

            let results = match query_builder.fetch_all(sqlite_pool).await {
                Ok(r) => r,
                Err(e) => {
                    tracing::error!("Failed to fetch stream composition data: {}", e);
                    return Json(StreamCompositionData {
                        dates: vec![],
                        avg_runs: vec![],
                        avg_events: vec![],
                    })
                    .into_response();
                }
            };

            // Build data maps from SQL results
            let mut runs_map: AHashMap<String, f64> = AHashMap::new();
            let mut events_map: AHashMap<String, f64> = AHashMap::new();

            for (period, r, e) in results {
                runs_map.insert(period.clone(), r.unwrap_or(0.0));
                events_map.insert(period, e.unwrap_or(0.0));
            }

            // Generate complete list of time buckets from start to now
            // This ensures we show zeros for periods with no activity
            let dates: Vec<String> = if interval.is_some() {
                crate::timezone::generate_time_buckets(time_unit, days, display_timezone)
            } else {
                // For "all" time range, just use dates from results
                let mut all_dates: std::collections::BTreeSet<String> =
                    std::collections::BTreeSet::new();
                all_dates.extend(runs_map.keys().cloned());
                all_dates.into_iter().collect()
            };

            let avg_runs: Vec<f64> = dates
                .iter()
                .map(|d| *runs_map.get(d).unwrap_or(&0.0))
                .collect();
            let avg_events: Vec<f64> = dates
                .iter()
                .map(|d| *events_map.get(d).unwrap_or(&0.0))
                .collect();

            Json(StreamCompositionData {
                dates,
                avg_runs,
                avg_events,
            })
            .into_response()
        }
    }
}

#[cfg(test)]
mod dashboard_filter_tests {
    use super::*;
    use axum::extract::Extension;
    use axum_extra::extract::CookieJar;
    use http_body_util::BodyExt;

    async fn json_body(response: axum::response::Response) -> serde_json::Value {
        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("collect response body")
            .to_bytes();
        serde_json::from_slice(&bytes).expect("valid JSON response")
    }

    #[tokio::test]
    async fn stream_charts_only_include_streams_touched_by_selected_project() {
        let db = hot::db::test_db().await;
        let data = hot::db::insert_test_data(&db).await.unwrap();

        let other_project_id = Uuid::now_v7();
        hot::db::Project::insert_project(
            &db,
            &other_project_id,
            &data.env_id,
            "other-project",
            &data.user_id,
        )
        .await
        .unwrap();
        let other_build_id = Uuid::now_v7();
        hot::db::Build::insert_build(
            &db,
            &other_build_id,
            &other_project_id,
            "other-build",
            0,
            hot::db::Build::BUILD_TYPE_LIVE,
            &data.user_id,
        )
        .await
        .unwrap();
        hot::db::Run::insert_run(
            &db,
            &Uuid::now_v7(),
            &data.env_id,
            &Uuid::now_v7(),
            Some(&other_build_id),
            hot::db::run::RunType::Run.as_id(),
            None,
            &data.user_id,
            None,
            None,
        )
        .await
        .unwrap();

        let conf = hot::val!({"app": {"host": "localhost", "port": 4680}});
        let session = Session::from_user_id(&db, &conf, &data.user_id, &CookieJar::new())
            .await
            .unwrap();
        let db = Arc::new(db);
        let params = AHashMap::from_iter([
            ("project_id".to_string(), data.project_id.to_string()),
            ("time_range".to_string(), "all".to_string()),
            ("time_unit".to_string(), "day".to_string()),
        ]);

        let activity = json_body(
            stream_activity_timeline_handler(
                State(db.clone()),
                Query(params.clone()),
                Extension(session.clone()),
            )
            .await
            .into_response(),
        )
        .await;
        let composition = json_body(
            stream_composition_handler(State(db), Query(params), Extension(session))
                .await
                .into_response(),
        )
        .await;

        let stream_count: i64 = activity["count"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(serde_json::Value::as_i64)
            .sum();
        assert_eq!(stream_count, 1);
        assert_eq!(
            composition["avg_runs"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(serde_json::Value::as_f64)
                .sum::<f64>(),
            1.0
        );
    }
}

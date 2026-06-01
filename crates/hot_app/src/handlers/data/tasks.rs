use crate::auth::Session;
use ahash::AHashMap;
use axum::extract::{Query, State};
use axum::response::{IntoResponse, Json};
use chrono::{DateTime, Utc};
use hot::db::DatabasePool;
use serde::Serialize;
use std::sync::Arc;

#[derive(Serialize)]
pub struct TaskActivityTimelineData {
    dates: Vec<String>,
    series: Vec<TaskActivitySeries>,
}

#[derive(Serialize)]
pub struct TaskActivitySeries {
    name: String,
    data: Vec<i64>,
}

/// GET /data/task-activity-timeline - Task creation/completion by status over time
pub async fn task_activity_timeline_handler(
    State(db): State<Arc<DatabasePool>>,
    Query(params): Query<AHashMap<String, String>>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> impl IntoResponse {
    let env_id = match session.current_env_id() {
        Some(id) => id,
        None => {
            return Json(TaskActivityTimelineData {
                dates: vec![],
                series: vec![],
            })
            .into_response();
        }
    };

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
        .and_then(|p| uuid::Uuid::parse_str(p).ok());

    let days: i64 = match time_range {
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

    let display_timezone = &session.display_timezone;
    let statuses = ["completed", "failed", "running", "queued"];

    match db.as_ref() {
        DatabasePool::Postgres(pg_pool) => {
            let group_by_clause =
                crate::timezone::postgres_date_trunc(time_unit, "t.created_at", display_timezone);
            let date_format = crate::timezone::postgres_date_format(time_unit);
            let pg_project_clause = if project_id.is_some() {
                if interval.is_some() {
                    "JOIN build bprj ON t.build_id = bprj.build_id AND bprj.project_id = $3"
                } else {
                    "JOIN build bprj ON t.build_id = bprj.build_id AND bprj.project_id = $2"
                }
            } else {
                ""
            };

            let query = if interval.is_some() {
                format!(
                    r#"
                    SELECT
                        {group_by} as period,
                        ts.name as status,
                        COUNT(*) as count
                    FROM task t
                    JOIN task_status ts ON t.task_status_id = ts.task_status_id
                    {pg_project_clause}
                    WHERE t.env_id = $1
                        AND t.created_at >= NOW() - ($2 || ' days')::INTERVAL
                    GROUP BY period, ts.name
                    ORDER BY period ASC
                    "#,
                    group_by = group_by_clause
                )
            } else {
                format!(
                    r#"
                    SELECT
                        {group_by} as period,
                        ts.name as status,
                        COUNT(*) as count
                    FROM task t
                    JOIN task_status ts ON t.task_status_id = ts.task_status_id
                    {pg_project_clause}
                    WHERE t.env_id = $1
                    GROUP BY period, ts.name
                    ORDER BY period ASC
                    "#,
                    group_by = group_by_clause
                )
            };

            let mut query_builder = sqlx::query_as::<_, (DateTime<Utc>, String, i64)>(
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
                    tracing::error!("Failed to fetch task activity timeline: {}", e);
                    return Json(TaskActivityTimelineData {
                        dates: vec![],
                        series: vec![],
                    })
                    .into_response();
                }
            };

            let mut status_map: AHashMap<String, AHashMap<String, i64>> = AHashMap::new();
            for (dt, status, count) in results {
                let date_str =
                    crate::timezone::format_in_timezone(&dt, display_timezone, date_format);
                status_map
                    .entry(status)
                    .or_default()
                    .insert(date_str, count);
            }

            let dates: Vec<String> = if interval.is_some() {
                crate::timezone::generate_time_buckets(time_unit, days, display_timezone)
            } else {
                let mut all_dates: std::collections::BTreeSet<String> =
                    std::collections::BTreeSet::new();
                for map in status_map.values() {
                    all_dates.extend(map.keys().cloned());
                }
                all_dates.into_iter().collect()
            };

            let series: Vec<TaskActivitySeries> = statuses
                .iter()
                .map(|s| {
                    let data: Vec<i64> = dates
                        .iter()
                        .map(|d| {
                            status_map
                                .get(*s)
                                .and_then(|m| m.get(d))
                                .copied()
                                .unwrap_or(0)
                        })
                        .collect();
                    TaskActivitySeries {
                        name: s.to_string(),
                        data,
                    }
                })
                .collect();

            Json(TaskActivityTimelineData { dates, series }).into_response()
        }
        DatabasePool::Sqlite(sqlite_pool) => {
            let group_by_clause =
                crate::timezone::sqlite_date_bucket(time_unit, "t.created_at", display_timezone);
            let sqlite_project_clause = if project_id.is_some() {
                if interval.is_some() {
                    "JOIN build bprj ON t.build_id = bprj.build_id AND bprj.project_id = ?3"
                } else {
                    "JOIN build bprj ON t.build_id = bprj.build_id AND bprj.project_id = ?2"
                }
            } else {
                ""
            };

            let query = if interval.is_some() {
                format!(
                    r#"
                    SELECT
                        {group_by} as period,
                        ts.name as status,
                        COUNT(*) as count
                    FROM task t
                    JOIN task_status ts ON t.task_status_id = ts.task_status_id
                    {sqlite_project_clause}
                    WHERE t.env_id = ?1
                        AND t.created_at >= datetime('now', '-' || ?2 || ' days')
                    GROUP BY period, ts.name
                    ORDER BY period ASC
                    "#,
                    group_by = group_by_clause
                )
            } else {
                format!(
                    r#"
                    SELECT
                        {group_by} as period,
                        ts.name as status,
                        COUNT(*) as count
                    FROM task t
                    JOIN task_status ts ON t.task_status_id = ts.task_status_id
                    {sqlite_project_clause}
                    WHERE t.env_id = ?1
                    GROUP BY period, ts.name
                    ORDER BY period ASC
                    "#,
                    group_by = group_by_clause
                )
            };

            let mut query_builder =
                sqlx::query_as::<_, (String, String, i64)>(sqlx::AssertSqlSafe(query.as_str()))
                    .bind(env_id.to_string());

            if interval.is_some() {
                query_builder = query_builder.bind(days);
            }
            if let Some(project_id) = project_id {
                query_builder = query_builder.bind(project_id.to_string());
            }

            let results = match query_builder.fetch_all(sqlite_pool).await {
                Ok(r) => r,
                Err(e) => {
                    tracing::error!("Failed to fetch task activity timeline (sqlite): {}", e);
                    return Json(TaskActivityTimelineData {
                        dates: vec![],
                        series: vec![],
                    })
                    .into_response();
                }
            };

            let mut status_map: AHashMap<String, AHashMap<String, i64>> = AHashMap::new();
            for (date_str, status, count) in results {
                status_map
                    .entry(status)
                    .or_default()
                    .insert(date_str, count);
            }

            let dates: Vec<String> = if interval.is_some() {
                crate::timezone::generate_time_buckets(time_unit, days, display_timezone)
            } else {
                let mut all_dates: std::collections::BTreeSet<String> =
                    std::collections::BTreeSet::new();
                for map in status_map.values() {
                    all_dates.extend(map.keys().cloned());
                }
                all_dates.into_iter().collect()
            };

            let series: Vec<TaskActivitySeries> = statuses
                .iter()
                .map(|s| {
                    let data: Vec<i64> = dates
                        .iter()
                        .map(|d| {
                            status_map
                                .get(*s)
                                .and_then(|m| m.get(d))
                                .copied()
                                .unwrap_or(0)
                        })
                        .collect();
                    TaskActivitySeries {
                        name: s.to_string(),
                        data,
                    }
                })
                .collect();

            Json(TaskActivityTimelineData { dates, series }).into_response()
        }
    }
}

#[derive(Serialize)]
pub struct TaskCusTimelineData {
    dates: Vec<String>,
    cus: Vec<i64>,
}

/// GET /data/task-cus-timeline - Aggregated CUS (Compute Unit Seconds) over time
pub async fn task_cus_timeline_handler(
    State(db): State<Arc<DatabasePool>>,
    Query(params): Query<AHashMap<String, String>>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> impl IntoResponse {
    let env_id = match session.current_env_id() {
        Some(id) => id,
        None => {
            return Json(TaskCusTimelineData {
                dates: vec![],
                cus: vec![],
            })
            .into_response();
        }
    };

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
        .and_then(|p| uuid::Uuid::parse_str(p).ok());

    let days: i64 = match time_range {
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

    let display_timezone = &session.display_timezone;

    match db.as_ref() {
        DatabasePool::Postgres(pg_pool) => {
            let group_by_clause =
                crate::timezone::postgres_date_trunc(time_unit, "t.created_at", display_timezone);
            let date_format = crate::timezone::postgres_date_format(time_unit);
            let project_join = if project_id.is_some() {
                if interval.is_some() {
                    "JOIN build bprj ON t.build_id = bprj.build_id AND bprj.project_id = $3"
                } else {
                    "JOIN build bprj ON t.build_id = bprj.build_id AND bprj.project_id = $2"
                }
            } else {
                ""
            };

            let cus_expr = r#"COALESCE(SUM(CASE
                WHEN t.result->'$val'->'err'->>'compute-units' IS NOT NULL
                    THEN (t.result->'$val'->'err'->>'compute-units')::bigint
                WHEN t.result->>'compute-units' IS NOT NULL
                    THEN (t.result->>'compute-units')::bigint
                ELSE 0
            END), 0)::bigint"#;

            let query = if interval.is_some() {
                format!(
                    r#"
                    SELECT
                        {group_by} as period,
                        {cus} as cus
                    FROM task t
                    {project_join}
                    WHERE t.env_id = $1
                        AND t.created_at >= NOW() - ($2 || ' days')::INTERVAL
                    GROUP BY period
                    ORDER BY period ASC
                    "#,
                    group_by = group_by_clause,
                    cus = cus_expr,
                )
            } else {
                format!(
                    r#"
                    SELECT
                        {group_by} as period,
                        {cus} as cus
                    FROM task t
                    {project_join}
                    WHERE t.env_id = $1
                    GROUP BY period
                    ORDER BY period ASC
                    "#,
                    group_by = group_by_clause,
                    cus = cus_expr,
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
                    tracing::error!("Failed to fetch task CUS timeline: {}", e);
                    return Json(TaskCusTimelineData {
                        dates: vec![],
                        cus: vec![],
                    })
                    .into_response();
                }
            };

            let mut cus_map: AHashMap<String, i64> = AHashMap::new();
            for (dt, cus) in results {
                let date_str =
                    crate::timezone::format_in_timezone(&dt, display_timezone, date_format);
                cus_map.insert(date_str, cus);
            }

            let dates: Vec<String> = if interval.is_some() {
                crate::timezone::generate_time_buckets(time_unit, days, display_timezone)
            } else {
                let mut all_dates: std::collections::BTreeSet<String> =
                    std::collections::BTreeSet::new();
                all_dates.extend(cus_map.keys().cloned());
                all_dates.into_iter().collect()
            };

            let cus: Vec<i64> = dates
                .iter()
                .map(|d| *cus_map.get(d).unwrap_or(&0))
                .collect();

            Json(TaskCusTimelineData { dates, cus }).into_response()
        }
        DatabasePool::Sqlite(sqlite_pool) => {
            let group_by_clause =
                crate::timezone::sqlite_date_bucket(time_unit, "t.created_at", display_timezone);
            let project_join = if project_id.is_some() {
                if interval.is_some() {
                    "JOIN build bprj ON t.build_id = bprj.build_id AND bprj.project_id = ?3"
                } else {
                    "JOIN build bprj ON t.build_id = bprj.build_id AND bprj.project_id = ?2"
                }
            } else {
                ""
            };

            let cus_expr = r#"COALESCE(SUM(CASE
                WHEN json_extract(t.result, '$."$val"."err"."compute-units"') IS NOT NULL
                    THEN CAST(json_extract(t.result, '$."$val"."err"."compute-units"') AS INTEGER)
                WHEN json_extract(t.result, '$."compute-units"') IS NOT NULL
                    THEN CAST(json_extract(t.result, '$."compute-units"') AS INTEGER)
                ELSE 0
            END), 0)"#;

            let query = if interval.is_some() {
                format!(
                    r#"
                    SELECT
                        {group_by} as period,
                        {cus} as cus
                    FROM task t
                    {project_join}
                    WHERE t.env_id = ?1
                        AND t.created_at >= datetime('now', '-' || ?2 || ' days')
                    GROUP BY period
                    ORDER BY period ASC
                    "#,
                    group_by = group_by_clause,
                    cus = cus_expr,
                )
            } else {
                format!(
                    r#"
                    SELECT
                        {group_by} as period,
                        {cus} as cus
                    FROM task t
                    {project_join}
                    WHERE t.env_id = ?1
                    GROUP BY period
                    ORDER BY period ASC
                    "#,
                    group_by = group_by_clause,
                    cus = cus_expr,
                )
            };

            let mut query_builder =
                sqlx::query_as::<_, (String, i64)>(sqlx::AssertSqlSafe(query.as_str()))
                    .bind(env_id.to_string());

            if interval.is_some() {
                query_builder = query_builder.bind(days);
            }
            if let Some(project_id) = project_id {
                query_builder = query_builder.bind(project_id.to_string());
            }

            let results = match query_builder.fetch_all(sqlite_pool).await {
                Ok(r) => r,
                Err(e) => {
                    tracing::error!("Failed to fetch task CUS timeline (sqlite): {}", e);
                    return Json(TaskCusTimelineData {
                        dates: vec![],
                        cus: vec![],
                    })
                    .into_response();
                }
            };

            let mut cus_map: AHashMap<String, i64> = AHashMap::new();
            for (date_str, cus) in results {
                cus_map.insert(date_str, cus);
            }

            let dates: Vec<String> = if interval.is_some() {
                crate::timezone::generate_time_buckets(time_unit, days, display_timezone)
            } else {
                let mut all_dates: std::collections::BTreeSet<String> =
                    std::collections::BTreeSet::new();
                all_dates.extend(cus_map.keys().cloned());
                all_dates.into_iter().collect()
            };

            let cus: Vec<i64> = dates
                .iter()
                .map(|d| *cus_map.get(d).unwrap_or(&0))
                .collect();

            Json(TaskCusTimelineData { dates, cus }).into_response()
        }
    }
}

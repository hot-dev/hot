use chrono::{DateTime, Utc};
use serde_json::Value as JsonValue;
use sqlx::FromRow;
use thiserror::Error;
use uuid::Uuid;

use super::search::IdSearch;

#[derive(Error, Debug)]
pub enum StreamError {
    #[error("Database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("Stream not found")]
    NotFound,
}

/// Stream model - execution stream tracking
#[derive(Debug, FromRow)]
pub struct Stream {
    pub stream_id: Uuid,
    pub env_id: Uuid,
    pub name: Option<String>,
    pub description: Option<String>,
    pub tags: Option<JsonValue>,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub last_activity_at: DateTime<Utc>,
    pub total_runs: i32,
    pub total_events: i32,
    pub total_duration_ms: i64,
}

/// Stream summary information with aggregated project data
#[derive(Debug, FromRow)]
pub struct StreamSummary {
    pub stream_id: Uuid,
    pub env_id: Uuid,
    pub project_ids: Option<JsonValue>, // JSON array of project UUIDs from runs
    pub project_names: Option<JsonValue>, // JSON array of project names from runs
    pub start_time: DateTime<Utc>,
    pub total_runs: i64,
    pub total_events: i64,
    pub last_activity_at: DateTime<Utc>,
    pub latest_event_type: Option<String>,
    pub latest_run_fn: Option<String>,
}

impl StreamSummary {
    /// Get formatted project names as a comma-separated string
    pub fn project_names_display(&self) -> String {
        match &self.project_names {
            Some(json_val) => {
                if let Some(arr) = json_val.as_array() {
                    let names: Vec<String> = arr
                        .iter()
                        .filter_map(|v| v.as_str())
                        .map(|s| s.to_string())
                        .collect();
                    if names.is_empty() {
                        "-".to_string()
                    } else {
                        names.join(", ")
                    }
                } else {
                    "-".to_string()
                }
            }
            None => "-".to_string(),
        }
    }
}

impl Stream {
    /// Create or get an existing stream
    pub async fn create_or_get_stream(
        db: &crate::db::DatabasePool,
        stream_id: Uuid,
        env_id: Uuid,
    ) -> Result<(), StreamError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                sqlx::query(
                    r#"
                    INSERT INTO stream (stream_id, env_id, started_at)
                    VALUES ($1, $2, now())
                    ON CONFLICT (stream_id) DO NOTHING
                    "#,
                )
                .bind(stream_id)
                .bind(env_id)
                .execute(pg_pool)
                .await?;
                Ok(())
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                sqlx::query(
                    r#"
                    INSERT OR IGNORE INTO stream (stream_id, env_id, started_at)
                    VALUES (?, ?, datetime('now'))
                    "#,
                )
                .bind(stream_id)
                .bind(env_id)
                .execute(sqlite_pool)
                .await?;
                Ok(())
            }
        }
    }

    /// Update stream metrics (called after run/event changes)
    pub async fn update_metrics(
        db: &crate::db::DatabasePool,
        stream_id: &Uuid,
    ) -> Result<(), StreamError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                sqlx::query(
                    r#"
                    UPDATE stream SET
                        total_runs = (SELECT COUNT(*) FROM run WHERE stream_id = $1),
                        total_events = (SELECT COUNT(*) FROM event WHERE stream_id = $1),
                        total_duration_ms = (
                            SELECT COALESCE(SUM(EXTRACT(EPOCH FROM (stop_time - start_time)) * 1000), 0)
                            FROM run
                            WHERE stream_id = $1 AND stop_time IS NOT NULL
                        ),
                        last_activity_at = now()
                    WHERE stream_id = $1
                    "#,
                )
                .bind(stream_id)
                .execute(pg_pool)
                .await?;
                Ok(())
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                sqlx::query(
                    r#"
                    UPDATE stream SET
                        total_runs = (SELECT COUNT(*) FROM run WHERE stream_id = ?),
                        total_events = (SELECT COUNT(*) FROM event WHERE stream_id = ?),
                        total_duration_ms = (
                            SELECT CAST(ROUND(COALESCE(SUM((julianday(stop_time) - julianday(start_time)) * 86400000), 0)) AS INTEGER)
                            FROM run
                            WHERE stream_id = ? AND stop_time IS NOT NULL
                        ),
                        last_activity_at = datetime('now')
                    WHERE stream_id = ?
                    "#,
                )
                .bind(stream_id)
                .bind(stream_id)
                .bind(stream_id)
                .bind(stream_id)
                .execute(sqlite_pool)
                .await?;
                Ok(())
            }
        }
    }

    /// Update the stream counters after a successful event insert.
    pub async fn record_event_inserted(
        db: &crate::db::DatabasePool,
        stream_id: &Uuid,
    ) -> Result<(), StreamError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                sqlx::query(
                    r#"
                    UPDATE stream SET
                        total_events = total_events + 1,
                        last_activity_at = now()
                    WHERE stream_id = $1
                    "#,
                )
                .bind(stream_id)
                .execute(pg_pool)
                .await?;
                Ok(())
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                sqlx::query(
                    r#"
                    UPDATE stream SET
                        total_events = total_events + 1,
                        last_activity_at = datetime('now')
                    WHERE stream_id = ?
                    "#,
                )
                .bind(stream_id)
                .execute(sqlite_pool)
                .await?;
                Ok(())
            }
        }
    }

    /// Update the stream counters after a run row is newly inserted.
    pub async fn record_run_started(
        db: &crate::db::DatabasePool,
        stream_id: &Uuid,
    ) -> Result<(), StreamError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                sqlx::query(
                    r#"
                    UPDATE stream SET
                        total_runs = total_runs + 1,
                        last_activity_at = now()
                    WHERE stream_id = $1
                    "#,
                )
                .bind(stream_id)
                .execute(pg_pool)
                .await?;
                Ok(())
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                sqlx::query(
                    r#"
                    UPDATE stream SET
                        total_runs = total_runs + 1,
                        last_activity_at = datetime('now')
                    WHERE stream_id = ?
                    "#,
                )
                .bind(stream_id)
                .execute(sqlite_pool)
                .await?;
                Ok(())
            }
        }
    }

    /// Update the stream counters after a run is inserted already terminal.
    pub async fn record_run_started_and_finished(
        db: &crate::db::DatabasePool,
        stream_id: &Uuid,
        duration_ms: i64,
    ) -> Result<(), StreamError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                sqlx::query(
                    r#"
                    UPDATE stream SET
                        total_runs = total_runs + 1,
                        total_duration_ms = total_duration_ms + $2,
                        last_activity_at = now()
                    WHERE stream_id = $1
                    "#,
                )
                .bind(stream_id)
                .bind(duration_ms.max(0))
                .execute(pg_pool)
                .await?;
                Ok(())
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                sqlx::query(
                    r#"
                    UPDATE stream SET
                        total_runs = total_runs + 1,
                        total_duration_ms = total_duration_ms + ?,
                        last_activity_at = datetime('now')
                    WHERE stream_id = ?
                    "#,
                )
                .bind(duration_ms.max(0))
                .bind(stream_id)
                .execute(sqlite_pool)
                .await?;
                Ok(())
            }
        }
    }

    /// Update the stream duration after a running row transitions out of running.
    pub async fn record_run_finished(
        db: &crate::db::DatabasePool,
        run_id: &Uuid,
    ) -> Result<(), StreamError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                sqlx::query(
                    r#"
                    UPDATE stream s SET
                        total_duration_ms = total_duration_ms + COALESCE(
                            EXTRACT(EPOCH FROM (r.stop_time - r.start_time)) * 1000,
                            0
                        )::bigint,
                        last_activity_at = now()
                    FROM run r
                    WHERE r.run_id = $1
                      AND s.stream_id = r.stream_id
                    "#,
                )
                .bind(run_id)
                .execute(pg_pool)
                .await?;
                Ok(())
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                sqlx::query(
                    r#"
                    UPDATE stream SET
                        total_duration_ms = total_duration_ms + (
                            SELECT CAST(ROUND(COALESCE(
                                (julianday(stop_time) - julianday(start_time)) * 86400000,
                                0
                            )) AS INTEGER)
                            FROM run
                            WHERE run_id = ?
                        ),
                        last_activity_at = datetime('now')
                    WHERE stream_id = (
                        SELECT stream_id
                        FROM run
                        WHERE run_id = ?
                    )
                    "#,
                )
                .bind(run_id)
                .bind(run_id)
                .execute(sqlite_pool)
                .await?;
                Ok(())
            }
        }
    }

    /// Get stream by ID
    pub async fn get_stream(
        db: &crate::db::DatabasePool,
        stream_id: &Uuid,
    ) -> Result<Stream, StreamError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let stream =
                    sqlx::query_as::<_, Stream>("SELECT * FROM stream WHERE stream_id = $1")
                        .bind(stream_id)
                        .fetch_one(pg_pool)
                        .await
                        .map_err(|e| match e {
                            sqlx::Error::RowNotFound => StreamError::NotFound,
                            other => StreamError::Database(other),
                        })?;
                Ok(stream)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let stream =
                    sqlx::query_as::<_, Stream>("SELECT * FROM stream WHERE stream_id = ?")
                        .bind(stream_id)
                        .fetch_one(sqlite_pool)
                        .await
                        .map_err(|e| match e {
                            sqlx::Error::RowNotFound => StreamError::NotFound,
                            other => StreamError::Database(other),
                        })?;
                Ok(stream)
            }
        }
    }

    /// Get streams by environment with pagination
    pub async fn get_streams_by_env(
        db: &crate::db::DatabasePool,
        env_id: &Uuid,
        limit: Option<i64>,
        offset: Option<i64>,
    ) -> Result<Vec<Stream>, StreamError> {
        let limit = limit.unwrap_or(50);
        let offset = offset.unwrap_or(0);

        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let streams = sqlx::query_as::<_, Stream>(
                    r#"
                    SELECT * FROM stream
                    WHERE env_id = $1
                    ORDER BY created_at DESC
                    LIMIT $2 OFFSET $3
                    "#,
                )
                .bind(env_id)
                .bind(limit)
                .bind(offset)
                .fetch_all(pg_pool)
                .await?;
                Ok(streams)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let streams = sqlx::query_as::<_, Stream>(
                    r#"
                    SELECT * FROM stream
                    WHERE env_id = ?
                    ORDER BY created_at DESC
                    LIMIT ? OFFSET ?
                    "#,
                )
                .bind(env_id)
                .bind(limit)
                .bind(offset)
                .fetch_all(sqlite_pool)
                .await?;
                Ok(streams)
            }
        }
    }

    /// Get count of streams by environment
    pub async fn get_count_by_env(
        db: &crate::db::DatabasePool,
        env_id: &Uuid,
    ) -> Result<i64, StreamError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let count: i64 =
                    sqlx::query_scalar("SELECT COUNT(*) FROM stream WHERE env_id = $1")
                        .bind(env_id)
                        .fetch_one(pg_pool)
                        .await?;
                Ok(count)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM stream WHERE env_id = ?")
                    .bind(env_id)
                    .fetch_one(sqlite_pool)
                    .await?;
                Ok(count)
            }
        }
    }
}

impl StreamSummary {
    /// Get streams by environment with pagination (backward compatibility)
    pub async fn get_streams_by_env(
        db: &crate::db::DatabasePool,
        env_id: &Uuid,
        limit: Option<i64>,
        offset: Option<i64>,
    ) -> Result<Vec<StreamSummary>, StreamError> {
        let limit = limit.unwrap_or(50);
        let offset = offset.unwrap_or(0);

        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let streams = sqlx::query_as::<_, StreamSummary>(
                    r#"
                    WITH page_streams AS (
                        SELECT
                            s.stream_id,
                            s.env_id,
                            s.started_at,
                            s.total_runs,
                            s.total_events,
                            s.last_activity_at,
                            s.created_at
                        FROM stream s
                        WHERE s.env_id = $1
                        ORDER BY s.created_at DESC
                        LIMIT $2 OFFSET $3
                    )
                        SELECT
                        ps.stream_id,
                        ps.env_id,
                        COALESCE(
                            jsonb_agg(DISTINCT p.project_id) FILTER (WHERE p.project_id IS NOT NULL),
                            '[]'::jsonb
                        ) as project_ids,
                        COALESCE(
                            jsonb_agg(DISTINCT p.name) FILTER (WHERE p.name IS NOT NULL),
                            '[]'::jsonb
                        ) as project_names,
                        ps.started_at as start_time,
                        ps.total_runs::bigint,
                        ps.total_events::bigint,
                        ps.last_activity_at,
                        (
                            SELECT e.event_type
                            FROM event e
                            WHERE e.stream_id = ps.stream_id AND e.env_id = ps.env_id
                            ORDER BY e.created_at DESC
                            LIMIT 1
                        ) as latest_event_type,
                        (
                            SELECT COALESCE(
                                e.event_data->>'fn',
                                (SELECT c.function_name FROM call c WHERE c.run_id = latest_run.run_id AND c.parent_call_id IS NULL LIMIT 1),
                                (SELECT t.function_name FROM task t WHERE t.run_id = latest_run.run_id LIMIT 1)
                            )
                            FROM run latest_run
                            LEFT JOIN event e ON latest_run.event_id = e.event_id
                            WHERE latest_run.stream_id = ps.stream_id AND latest_run.env_id = ps.env_id
                            ORDER BY latest_run.start_time DESC
                            LIMIT 1
                        ) as latest_run_fn
                    FROM page_streams ps
                    LEFT JOIN run r ON ps.stream_id = r.stream_id
                    LEFT JOIN build b ON r.build_id = b.build_id
                    LEFT JOIN project p ON b.project_id = p.project_id
                    GROUP BY ps.stream_id, ps.env_id, ps.started_at, ps.last_activity_at, ps.total_runs, ps.total_events, ps.created_at
                    ORDER BY ps.created_at DESC
                    "#,
                )
                .bind(env_id)
                .bind(limit)
                .bind(offset)
                .fetch_all(pg_pool)
                .await?;
                Ok(streams)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let streams = sqlx::query_as::<_, StreamSummary>(
                    r#"
                    WITH page_streams AS (
                        SELECT
                            s.stream_id,
                            s.env_id,
                            s.started_at,
                            s.total_runs,
                            s.total_events,
                            s.last_activity_at,
                            s.created_at
                        FROM stream s
                        WHERE s.env_id = ?
                        ORDER BY s.created_at DESC
                        LIMIT ? OFFSET ?
                    )
                        SELECT
                        ps.stream_id,
                        ps.env_id,
                        COALESCE(
                            json_group_array(DISTINCT LOWER(HEX(p.project_id))),
                            '[]'
                        ) as project_ids,
                        COALESCE(
                            json_group_array(DISTINCT p.name),
                            '[]'
                        ) as project_names,
                        ps.started_at as start_time,
                        CAST(ps.total_runs AS INTEGER) as total_runs,
                        CAST(ps.total_events AS INTEGER) as total_events,
                        ps.last_activity_at,
                        (
                            SELECT e.event_type
                            FROM event e
                            WHERE e.stream_id = ps.stream_id AND e.env_id = ps.env_id
                            ORDER BY e.created_at DESC
                            LIMIT 1
                        ) as latest_event_type,
                        (
                            SELECT COALESCE(
                                json_extract(e.event_data, '$.fn'),
                                (SELECT c.function_name FROM call c WHERE c.run_id = latest_run.run_id AND c.parent_call_id IS NULL LIMIT 1),
                                (SELECT t.function_name FROM task t WHERE t.run_id = latest_run.run_id LIMIT 1)
                            )
                            FROM run latest_run
                            LEFT JOIN event e ON latest_run.event_id = e.event_id
                            WHERE latest_run.stream_id = ps.stream_id AND latest_run.env_id = ps.env_id
                            ORDER BY latest_run.start_time DESC
                            LIMIT 1
                        ) as latest_run_fn
                    FROM page_streams ps
                    LEFT JOIN run r ON ps.stream_id = r.stream_id
                    LEFT JOIN build b ON r.build_id = b.build_id
                    LEFT JOIN project p ON b.project_id = p.project_id
                    GROUP BY ps.stream_id, ps.env_id, ps.started_at, ps.last_activity_at, ps.total_runs, ps.total_events, ps.created_at
                    ORDER BY ps.created_at DESC
                    "#,
                )
                .bind(env_id)
                .bind(limit)
                .bind(offset)
                .fetch_all(sqlite_pool)
                .await?;
                Ok(streams)
            }
        }
    }

    /// Get stream by ID (backward compatibility)
    pub async fn get_stream(
        db: &crate::db::DatabasePool,
        stream_id: &Uuid,
    ) -> Result<StreamSummary, StreamError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let stream = sqlx::query_as::<_, StreamSummary>(
                    r#"
                    SELECT
                        s.stream_id,
                        s.env_id,
                        COALESCE(
                            jsonb_agg(DISTINCT p.project_id) FILTER (WHERE p.project_id IS NOT NULL),
                            '[]'::jsonb
                        ) as project_ids,
                        COALESCE(
                            jsonb_agg(DISTINCT p.name) FILTER (WHERE p.name IS NOT NULL),
                            '[]'::jsonb
                        ) as project_names,
                        s.started_at as start_time,
                        s.total_runs::bigint,
                        s.total_events::bigint,
                        s.last_activity_at,
                        (
                            SELECT e.event_type
                            FROM event e
                            WHERE e.stream_id = s.stream_id AND e.env_id = s.env_id
                            ORDER BY e.created_at DESC
                            LIMIT 1
                        ) as latest_event_type,
                        (
                            SELECT COALESCE(
                                e.event_data->>'fn',
                                (SELECT c.function_name FROM call c WHERE c.run_id = latest_run.run_id AND c.parent_call_id IS NULL LIMIT 1),
                                (SELECT t.function_name FROM task t WHERE t.run_id = latest_run.run_id LIMIT 1)
                            )
                            FROM run latest_run
                            LEFT JOIN event e ON latest_run.event_id = e.event_id
                            WHERE latest_run.stream_id = s.stream_id AND latest_run.env_id = s.env_id
                            ORDER BY latest_run.start_time DESC
                            LIMIT 1
                        ) as latest_run_fn
                    FROM stream s
                    LEFT JOIN run r ON s.stream_id = r.stream_id
                    LEFT JOIN build b ON r.build_id = b.build_id
                    LEFT JOIN project p ON b.project_id = p.project_id
                    WHERE s.stream_id = $1
                    GROUP BY s.stream_id, s.env_id, s.started_at, s.last_activity_at, s.total_runs, s.total_events
                    "#,
                )
                .bind(stream_id)
                .fetch_one(pg_pool)
                .await
                .map_err(|e| match e {
                    sqlx::Error::RowNotFound => StreamError::NotFound,
                    other => StreamError::Database(other),
                })?;
                Ok(stream)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let stream = sqlx::query_as::<_, StreamSummary>(
                    r#"
                    SELECT
                        s.stream_id,
                        s.env_id,
                        COALESCE(
                            json_group_array(DISTINCT LOWER(HEX(p.project_id))),
                            '[]'
                        ) as project_ids,
                        COALESCE(
                            json_group_array(DISTINCT p.name),
                            '[]'
                        ) as project_names,
                        s.started_at as start_time,
                        CAST(s.total_runs AS INTEGER) as total_runs,
                        CAST(s.total_events AS INTEGER) as total_events,
                        s.last_activity_at,
                        (
                            SELECT e.event_type
                            FROM event e
                            WHERE e.stream_id = s.stream_id AND e.env_id = s.env_id
                            ORDER BY e.created_at DESC
                            LIMIT 1
                        ) as latest_event_type,
                        (
                            SELECT COALESCE(
                                json_extract(e.event_data, '$.fn'),
                                (SELECT c.function_name FROM call c WHERE c.run_id = latest_run.run_id AND c.parent_call_id IS NULL LIMIT 1),
                                (SELECT t.function_name FROM task t WHERE t.run_id = latest_run.run_id LIMIT 1)
                            )
                            FROM run latest_run
                            LEFT JOIN event e ON latest_run.event_id = e.event_id
                            WHERE latest_run.stream_id = s.stream_id AND latest_run.env_id = s.env_id
                            ORDER BY latest_run.start_time DESC
                            LIMIT 1
                        ) as latest_run_fn
                    FROM stream s
                    LEFT JOIN run r ON s.stream_id = r.stream_id
                    LEFT JOIN build b ON r.build_id = b.build_id
                    LEFT JOIN project p ON b.project_id = p.project_id
                    WHERE s.stream_id = ?
                    GROUP BY s.stream_id, s.env_id, s.started_at, s.last_activity_at, s.total_runs, s.total_events
                    "#,
                )
                .bind(stream_id)
                .fetch_one(sqlite_pool)
                .await
                .map_err(|e| match e {
                    sqlx::Error::RowNotFound => StreamError::NotFound,
                    other => StreamError::Database(other),
                })?;
                Ok(stream)
            }
        }
    }

    /// Get count of streams by environment (backward compatibility)
    pub async fn get_count_by_env(
        db: &crate::db::DatabasePool,
        env_id: &Uuid,
    ) -> Result<i64, StreamError> {
        Stream::get_count_by_env(db, env_id).await
    }

    /// Get streams by environment with filters (time_range, search).
    ///
    /// `time_range_cutoff` is the earliest `created_at` to include; pass
    /// `None` for "all time". Compute via [`crate::time_range::parse_time_range_cutoff`].
    pub async fn get_streams_by_env_filtered(
        db: &crate::db::DatabasePool,
        env_id: &Uuid,
        project_id: Option<&Uuid>,
        time_range_cutoff: Option<chrono::DateTime<chrono::Utc>>,
        search_term: Option<&str>,
        limit: Option<i64>,
        offset: Option<i64>,
    ) -> Result<Vec<StreamSummary>, StreamError> {
        let limit = limit.unwrap_or(50);
        let offset = offset.unwrap_or(0);

        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let mut query = String::from(
                    r#"
                    WITH page_streams AS (
                        SELECT
                            s.stream_id,
                            s.env_id,
                            s.started_at,
                            s.total_runs,
                            s.total_events,
                            s.last_activity_at,
                            s.created_at
                        FROM stream s
                        WHERE s.env_id = $1
                    "#,
                );

                let mut param_count = 1;

                if project_id.is_some() {
                    param_count += 1;
                    query.push_str(&format!(
                        r#"
                        AND EXISTS (
                            SELECT 1
                            FROM run project_run
                            JOIN build project_build ON project_run.build_id = project_build.build_id
                            WHERE project_run.stream_id = s.stream_id
                              AND project_run.env_id = s.env_id
                              AND project_build.project_id = ${}
                        )
                        "#,
                        param_count
                    ));
                }

                if time_range_cutoff.is_some() {
                    param_count += 1;
                    query.push_str(&format!(" AND s.created_at >= ${}", param_count));
                }

                let id_search = IdSearch::parse(search_term);

                if let Some(search) = &id_search
                    && search.uuid().is_some()
                {
                    let base = param_count + 1;
                    query.push_str(&format!(" AND s.stream_id = ${}", base));
                    param_count += 1;
                } else if let Some(search) = &id_search
                    && search.is_short_id()
                {
                    // Short ID detected (12 hex chars): pattern match on UUID suffix
                    let base = param_count + 1;
                    query.push_str(&format!(" AND CAST(s.stream_id AS TEXT) ILIKE ${}", base));
                    param_count += 1;
                } else if id_search.is_some() {
                    // General text search on stream_id
                    let base = param_count + 1;
                    query.push_str(&format!(" AND CAST(s.stream_id AS TEXT) ILIKE ${}", base));
                    param_count += 1;
                }

                param_count += 1;
                let limit_param = param_count;
                param_count += 1;
                let offset_param = param_count;

                query.push_str(&format!(
                    " ORDER BY s.created_at DESC LIMIT ${} OFFSET ${}) ",
                    limit_param, offset_param
                ));

                query.push_str(
                    r#"
                    SELECT
                        ps.stream_id,
                        ps.env_id,
                        COALESCE(
                            jsonb_agg(DISTINCT p.project_id) FILTER (WHERE p.project_id IS NOT NULL),
                            '[]'::jsonb
                        ) as project_ids,
                        COALESCE(
                            jsonb_agg(DISTINCT p.name) FILTER (WHERE p.name IS NOT NULL),
                            '[]'::jsonb
                        ) as project_names,
                        ps.started_at as start_time,
                        ps.total_runs::bigint,
                        ps.total_events::bigint,
                        ps.last_activity_at,
                        (
                            SELECT e.event_type
                            FROM event e
                            WHERE e.stream_id = ps.stream_id AND e.env_id = ps.env_id
                            ORDER BY e.created_at DESC
                            LIMIT 1
                        ) as latest_event_type,
                        (
                            SELECT COALESCE(
                                e.event_data->>'fn',
                                (SELECT c.function_name FROM call c WHERE c.run_id = latest_run.run_id AND c.parent_call_id IS NULL LIMIT 1),
                                (SELECT t.function_name FROM task t WHERE t.run_id = latest_run.run_id LIMIT 1)
                            )
                            FROM run latest_run
                            LEFT JOIN event e ON latest_run.event_id = e.event_id
                            WHERE latest_run.stream_id = ps.stream_id AND latest_run.env_id = ps.env_id
                            ORDER BY latest_run.start_time DESC
                            LIMIT 1
                        ) as latest_run_fn
                    FROM page_streams ps
                    LEFT JOIN run r ON ps.stream_id = r.stream_id
                    LEFT JOIN build b ON r.build_id = b.build_id
                    LEFT JOIN project p ON b.project_id = p.project_id
                    GROUP BY ps.stream_id, ps.env_id, ps.started_at, ps.last_activity_at, ps.total_runs, ps.total_events, ps.created_at
                    ORDER BY ps.created_at DESC
                    "#,
                );

                let mut db_query =
                    sqlx::query_as::<_, StreamSummary>(sqlx::AssertSqlSafe(query.as_str()))
                        .bind(env_id);

                if let Some(project_id) = project_id {
                    db_query = db_query.bind(project_id);
                }

                if let Some(cutoff) = time_range_cutoff {
                    db_query = db_query.bind(cutoff);
                }

                if let Some(search) = &id_search
                    && let Some(uuid) = search.uuid()
                {
                    db_query = db_query.bind(uuid); // stream_id exact match
                } else if let Some(search) = &id_search
                    && let Some(suffix_pattern) = search.suffix_pattern()
                {
                    db_query = db_query.bind(suffix_pattern); // stream_id suffix match
                } else if let Some(search) = &id_search {
                    db_query = db_query.bind(search.text_pattern()); // stream_id text search
                }

                let streams = db_query.bind(limit).bind(offset).fetch_all(pg_pool).await?;

                Ok(streams)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let mut query = String::from(
                    r#"
                    WITH page_streams AS (
                        SELECT
                            s.stream_id,
                            s.env_id,
                            s.started_at,
                            s.total_runs,
                            s.total_events,
                            s.last_activity_at,
                            s.created_at
                        FROM stream s
                        WHERE s.env_id = ?
                    "#,
                );

                if project_id.is_some() {
                    query.push_str(
                        r#"
                        AND EXISTS (
                            SELECT 1
                            FROM run project_run
                            JOIN build project_build ON project_run.build_id = project_build.build_id
                            WHERE project_run.stream_id = s.stream_id
                              AND project_run.env_id = s.env_id
                              AND project_build.project_id = ?
                        )
                        "#,
                    );
                }

                if time_range_cutoff.is_some() {
                    query.push_str(" AND s.created_at >= ?");
                }

                let id_search = IdSearch::parse(search_term);

                if let Some(search) = &id_search
                    && search.uuid().is_some()
                {
                    query.push_str(" AND s.stream_id = ?");
                } else if let Some(search) = &id_search
                    && search.is_short_id()
                {
                    // Short ID detected (12 hex chars): pattern match on UUID suffix
                    query.push_str(" AND LOWER(HEX(s.stream_id)) LIKE ?");
                } else if id_search.is_some() {
                    // General text search on stream_id
                    query.push_str(" AND LOWER(HEX(s.stream_id)) LIKE ?");
                }

                query.push_str(" ORDER BY s.created_at DESC LIMIT ? OFFSET ?) ");

                query.push_str(
                    r#"
                    SELECT
                        ps.stream_id,
                        ps.env_id,
                        COALESCE(
                            json_group_array(DISTINCT LOWER(HEX(p.project_id))),
                            '[]'
                        ) as project_ids,
                        COALESCE(
                            json_group_array(DISTINCT p.name),
                            '[]'
                        ) as project_names,
                        ps.started_at as start_time,
                        CAST(ps.total_runs AS INTEGER) as total_runs,
                        CAST(ps.total_events AS INTEGER) as total_events,
                        ps.last_activity_at,
                        (
                            SELECT e.event_type
                            FROM event e
                            WHERE e.stream_id = ps.stream_id AND e.env_id = ps.env_id
                            ORDER BY e.created_at DESC
                            LIMIT 1
                        ) as latest_event_type,
                        (
                            SELECT COALESCE(
                                json_extract(e.event_data, '$.fn'),
                                (SELECT c.function_name FROM call c WHERE c.run_id = latest_run.run_id AND c.parent_call_id IS NULL LIMIT 1),
                                (SELECT t.function_name FROM task t WHERE t.run_id = latest_run.run_id LIMIT 1)
                            )
                            FROM run latest_run
                            LEFT JOIN event e ON latest_run.event_id = e.event_id
                            WHERE latest_run.stream_id = ps.stream_id AND latest_run.env_id = ps.env_id
                            ORDER BY latest_run.start_time DESC
                            LIMIT 1
                        ) as latest_run_fn
                    FROM page_streams ps
                    LEFT JOIN run r ON ps.stream_id = r.stream_id
                    LEFT JOIN build b ON r.build_id = b.build_id
                    LEFT JOIN project p ON b.project_id = p.project_id
                    GROUP BY ps.stream_id, ps.env_id, ps.started_at, ps.last_activity_at, ps.total_runs, ps.total_events, ps.created_at
                    ORDER BY ps.created_at DESC
                    "#,
                );

                let mut db_query =
                    sqlx::query_as::<_, StreamSummary>(sqlx::AssertSqlSafe(query.as_str()))
                        .bind(env_id);

                if let Some(project_id) = project_id {
                    db_query = db_query.bind(project_id);
                }

                if let Some(cutoff) = time_range_cutoff {
                    db_query = db_query.bind(cutoff.format("%Y-%m-%d %H:%M:%S").to_string());
                }

                if let Some(search) = &id_search
                    && let Some(uuid) = search.uuid()
                {
                    db_query = db_query.bind(uuid); // stream_id exact match
                } else if let Some(search) = &id_search
                    && let Some(suffix_pattern) = search.suffix_pattern()
                {
                    db_query = db_query.bind(suffix_pattern); // stream_id suffix match
                } else if let Some(search) = &id_search {
                    db_query = db_query.bind(search.text_pattern()); // stream_id text search
                }

                let streams = db_query
                    .bind(limit)
                    .bind(offset)
                    .fetch_all(sqlite_pool)
                    .await?;

                Ok(streams)
            }
        }
    }

    /// Get count of streams by environment with filters.
    ///
    /// See [`Self::get_streams_by_env_filtered`] for the meaning of `time_range_cutoff`.
    pub async fn get_count_by_env_filtered(
        db: &crate::db::DatabasePool,
        env_id: &Uuid,
        project_id: Option<&Uuid>,
        time_range_cutoff: Option<chrono::DateTime<chrono::Utc>>,
        search_term: Option<&str>,
    ) -> Result<i64, StreamError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let mut query = String::from(
                    r#"
                    SELECT COUNT(*)
                    FROM stream s
                    WHERE s.env_id = $1
                    "#,
                );

                let mut param_count = 1;

                if project_id.is_some() {
                    param_count += 1;
                    query.push_str(&format!(
                        r#"
                        AND EXISTS (
                            SELECT 1
                            FROM run project_run
                            JOIN build project_build ON project_run.build_id = project_build.build_id
                            WHERE project_run.stream_id = s.stream_id
                              AND project_run.env_id = s.env_id
                              AND project_build.project_id = ${}
                        )
                        "#,
                        param_count
                    ));
                }

                if time_range_cutoff.is_some() {
                    param_count += 1;
                    query.push_str(&format!(" AND s.created_at >= ${}", param_count));
                }

                let id_search = IdSearch::parse(search_term);

                if let Some(search) = &id_search
                    && search.uuid().is_some()
                {
                    param_count += 1;
                    query.push_str(&format!(" AND s.stream_id = ${}", param_count));
                } else if let Some(search) = &id_search
                    && search.is_short_id()
                {
                    param_count += 1;
                    query.push_str(&format!(
                        " AND CAST(s.stream_id AS TEXT) ILIKE ${}",
                        param_count
                    ));
                } else if id_search.is_some() {
                    param_count += 1;
                    query.push_str(&format!(
                        " AND CAST(s.stream_id AS TEXT) ILIKE ${}",
                        param_count
                    ));
                }

                let mut db_query =
                    sqlx::query_scalar::<_, i64>(sqlx::AssertSqlSafe(query.as_str())).bind(env_id);

                if let Some(project_id) = project_id {
                    db_query = db_query.bind(project_id);
                }

                if let Some(cutoff) = time_range_cutoff {
                    db_query = db_query.bind(cutoff);
                }

                if let Some(search) = &id_search
                    && let Some(uuid) = search.uuid()
                {
                    db_query = db_query.bind(uuid); // stream_id exact match
                } else if let Some(search) = &id_search
                    && let Some(suffix_pattern) = search.suffix_pattern()
                {
                    db_query = db_query.bind(suffix_pattern); // stream_id suffix match
                } else if let Some(search) = &id_search {
                    db_query = db_query.bind(search.text_pattern()); // stream_id text search
                }

                let count = db_query.fetch_one(pg_pool).await?;
                Ok(count)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let mut query = String::from(
                    r#"
                    SELECT COUNT(*)
                    FROM stream s
                    WHERE s.env_id = ?
                    "#,
                );

                if project_id.is_some() {
                    query.push_str(
                        r#"
                        AND EXISTS (
                            SELECT 1
                            FROM run project_run
                            JOIN build project_build ON project_run.build_id = project_build.build_id
                            WHERE project_run.stream_id = s.stream_id
                              AND project_run.env_id = s.env_id
                              AND project_build.project_id = ?
                        )
                        "#,
                    );
                }

                if time_range_cutoff.is_some() {
                    query.push_str(" AND s.created_at >= ?");
                }

                let id_search = IdSearch::parse(search_term);

                if let Some(search) = &id_search
                    && search.uuid().is_some()
                {
                    query.push_str(" AND s.stream_id = ?");
                } else if let Some(search) = &id_search
                    && search.is_short_id()
                {
                    query.push_str(" AND LOWER(HEX(s.stream_id)) LIKE ?");
                } else if id_search.is_some() {
                    query.push_str(" AND LOWER(HEX(s.stream_id)) LIKE ?");
                }

                let mut db_query =
                    sqlx::query_scalar::<_, i64>(sqlx::AssertSqlSafe(query.as_str())).bind(env_id);

                if let Some(project_id) = project_id {
                    db_query = db_query.bind(project_id);
                }

                if let Some(cutoff) = time_range_cutoff {
                    db_query = db_query.bind(cutoff.format("%Y-%m-%d %H:%M:%S").to_string());
                }

                if let Some(search) = &id_search
                    && let Some(uuid) = search.uuid()
                {
                    db_query = db_query.bind(uuid); // stream_id exact match
                } else if let Some(search) = &id_search
                    && let Some(suffix_pattern) = search.suffix_pattern()
                {
                    db_query = db_query.bind(suffix_pattern); // stream_id suffix match
                } else if let Some(search) = &id_search {
                    db_query = db_query.bind(search.text_pattern()); // stream_id text search
                }

                let count = db_query.fetch_one(sqlite_pool).await?;
                Ok(count)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // UUIDs are stored as blobs in SQLite, so the search path must compare
    // against LOWER(HEX(stream_id)) rather than CAST(stream_id AS TEXT).
    // These tests guard that regression for full, hyphenless, and short ids.
    const STREAM_UUID: &str = "550e8400-e29b-41d4-a716-446655440000";
    const SHORT_ID: &str = "446655440000"; // last 12 hyphenless hex chars

    async fn seed_stream() -> (crate::db::DatabasePool, Uuid, Uuid) {
        let db = crate::db::test_db().await;
        let env_id = Uuid::now_v7();
        let stream_id = Uuid::parse_str(STREAM_UUID).unwrap();
        Stream::create_or_get_stream(&db, stream_id, env_id)
            .await
            .unwrap();
        (db, env_id, stream_id)
    }

    #[tokio::test]
    async fn run_metric_helpers_increment_without_recounting() {
        let (db, env_id, stream_id) = seed_stream().await;
        let crate::db::DatabasePool::Sqlite(sqlite_pool) = &db else {
            unreachable!("test_db uses SQLite");
        };
        let run_id = Uuid::now_v7();
        let started_at = Utc::now();
        let stopped_at = started_at + chrono::Duration::milliseconds(42);

        Stream::record_run_started(&db, &stream_id).await.unwrap();
        sqlx::query(
            "INSERT INTO run (run_id, env_id, stream_id, build_id, run_type_id, start_time, status_id)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(run_id)
        .bind(env_id)
        .bind(stream_id)
        .bind(Uuid::now_v7())
        .bind(1i16)
        .bind(started_at)
        .bind(1i16)
        .execute(sqlite_pool)
        .await
        .unwrap();

        sqlx::query("UPDATE run SET stop_time = ?, status_id = ? WHERE run_id = ?")
            .bind(stopped_at)
            .bind(2i16)
            .bind(run_id)
            .execute(sqlite_pool)
            .await
            .unwrap();
        Stream::record_run_finished(&db, &run_id).await.unwrap();

        Stream::record_run_started_and_finished(&db, &stream_id, 10)
            .await
            .unwrap();

        let stream = Stream::get_stream(&db, &stream_id).await.unwrap();
        assert_eq!(stream.total_runs, 2);
        assert_eq!(stream.total_events, 0);
        assert_eq!(stream.total_duration_ms, 52);
    }

    #[tokio::test]
    async fn search_by_short_id_matches_blob_uuid() {
        let (db, env_id, stream_id) = seed_stream().await;

        let results = StreamSummary::get_streams_by_env_filtered(
            &db,
            &env_id,
            None,
            None,
            Some(SHORT_ID),
            None,
            None,
        )
        .await
        .unwrap();

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].stream_id, stream_id);
    }

    #[tokio::test]
    async fn count_by_short_id_matches_blob_uuid() {
        let (db, env_id, _) = seed_stream().await;

        let count =
            StreamSummary::get_count_by_env_filtered(&db, &env_id, None, None, Some(SHORT_ID))
                .await
                .unwrap();

        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn search_by_full_uuid_matches() {
        let (db, env_id, stream_id) = seed_stream().await;

        let results = StreamSummary::get_streams_by_env_filtered(
            &db,
            &env_id,
            None,
            None,
            Some(STREAM_UUID),
            None,
            None,
        )
        .await
        .unwrap();

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].stream_id, stream_id);
    }

    #[tokio::test]
    async fn search_by_hyphenless_uuid_matches() {
        let (db, env_id, stream_id) = seed_stream().await;

        let results = StreamSummary::get_streams_by_env_filtered(
            &db,
            &env_id,
            None,
            None,
            Some("550e8400e29b41d4a716446655440000"),
            None,
            None,
        )
        .await
        .unwrap();

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].stream_id, stream_id);
    }

    #[tokio::test]
    async fn search_by_nonmatching_short_id_returns_empty() {
        let (db, env_id, _) = seed_stream().await;

        let results = StreamSummary::get_streams_by_env_filtered(
            &db,
            &env_id,
            None,
            None,
            Some("ffffffffffff"),
            None,
            None,
        )
        .await
        .unwrap();

        assert!(results.is_empty());
    }
}

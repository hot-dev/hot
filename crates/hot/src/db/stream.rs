use chrono::{DateTime, Utc};
use serde_json::Value as JsonValue;
use sqlx::FromRow;
use thiserror::Error;
use uuid::Uuid;

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
                            SELECT CAST(COALESCE(SUM((julianday(stop_time) - julianday(start_time)) * 86400000), 0) AS INTEGER)
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
                        .await?;
                Ok(stream)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let stream =
                    sqlx::query_as::<_, Stream>("SELECT * FROM stream WHERE stream_id = ?")
                        .bind(stream_id)
                        .fetch_one(sqlite_pool)
                        .await?;
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
                        s.last_activity_at
                    FROM stream s
                    LEFT JOIN run r ON s.stream_id = r.stream_id
                    LEFT JOIN build b ON r.build_id = b.build_id
                    LEFT JOIN project p ON b.project_id = p.project_id
                    WHERE s.env_id = $1
                    GROUP BY s.stream_id, s.env_id, s.started_at, s.last_activity_at, s.total_runs, s.total_events, s.created_at
                    ORDER BY s.created_at DESC
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
                let streams = sqlx::query_as::<_, StreamSummary>(
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
                        s.last_activity_at
                    FROM stream s
                    LEFT JOIN run r ON s.stream_id = r.stream_id
                    LEFT JOIN build b ON r.build_id = b.build_id
                    LEFT JOIN project p ON b.project_id = p.project_id
                    WHERE s.env_id = ?
                    GROUP BY s.stream_id, s.env_id, s.started_at, s.last_activity_at, s.total_runs, s.total_events
                    ORDER BY s.created_at DESC
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
                        s.last_activity_at
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
                .await?;
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
                        s.last_activity_at
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
                .await?;
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
                        s.last_activity_at
                    FROM stream s
                    LEFT JOIN run r ON s.stream_id = r.stream_id
                    LEFT JOIN build b ON r.build_id = b.build_id
                    LEFT JOIN project p ON b.project_id = p.project_id
                    WHERE s.env_id = $1
                    "#,
                );

                let mut param_count = 1;

                // project_id filter removed

                if time_range_cutoff.is_some() {
                    param_count += 1;
                    query.push_str(&format!(" AND s.created_at >= ${}", param_count));
                }

                // Add search filter with UUID optimization
                let search_uuid = search_term.and_then(|term| {
                    Uuid::parse_str(term).ok().or_else(|| {
                        if term.len() == 32 && term.chars().all(|c| c.is_ascii_hexdigit()) {
                            let with_dashes = format!(
                                "{}-{}-{}-{}-{}",
                                &term[0..8],
                                &term[8..12],
                                &term[12..16],
                                &term[16..20],
                                &term[20..32]
                            );
                            Uuid::parse_str(&with_dashes).ok()
                        } else {
                            None
                        }
                    })
                });

                // Check if search term is a short ID (last 12 chars of UUID)
                let is_short_id = search_term
                    .map(|term| term.len() == 12 && term.chars().all(|c| c.is_ascii_hexdigit()))
                    .unwrap_or(false);

                if search_uuid.is_some() {
                    let base = param_count + 1;
                    query.push_str(&format!(" AND s.stream_id = ${}", base));
                    param_count += 1;
                } else if is_short_id {
                    // Short ID detected (12 hex chars): pattern match on UUID suffix
                    let base = param_count + 1;
                    query.push_str(&format!(" AND CAST(s.stream_id AS TEXT) ILIKE ${}", base));
                    param_count += 1;
                } else if search_term.is_some() {
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
                    " GROUP BY s.stream_id, s.env_id, s.started_at, s.last_activity_at, s.total_runs, s.total_events, s.created_at ORDER BY s.created_at DESC LIMIT ${} OFFSET ${}",
                    limit_param, offset_param
                ));

                // Prepare search - UUID exact match or pattern matching
                let search_uuid_pg = search_term.and_then(|term| {
                    Uuid::parse_str(term).ok().or_else(|| {
                        if term.len() == 32 && term.chars().all(|c| c.is_ascii_hexdigit()) {
                            let with_dashes = format!(
                                "{}-{}-{}-{}-{}",
                                &term[0..8],
                                &term[8..12],
                                &term[12..16],
                                &term[16..20],
                                &term[20..32]
                            );
                            Uuid::parse_str(&with_dashes).ok()
                        } else {
                            None
                        }
                    })
                });

                // Check if search term is a short ID (last 12 chars of UUID)
                let is_short_id_pg = search_term
                    .map(|term| term.len() == 12 && term.chars().all(|c| c.is_ascii_hexdigit()))
                    .unwrap_or(false);

                // Short ID suffix pattern for matching end of UUID
                let short_id_pattern = search_term.map(|term| format!("%{}", term));
                let search_pattern = search_term.map(|term| format!("%{}%", term));

                let mut db_query = sqlx::query_as::<_, StreamSummary>(&query).bind(env_id);

                // project_id filter removed

                if let Some(cutoff) = time_range_cutoff {
                    db_query = db_query.bind(cutoff);
                }

                if let Some(uuid) = search_uuid_pg {
                    db_query = db_query.bind(uuid); // stream_id exact match
                } else if is_short_id_pg {
                    if let Some(ref suffix_pattern) = short_id_pattern {
                        db_query = db_query.bind(suffix_pattern); // stream_id suffix match
                    }
                } else if let Some(ref pattern) = search_pattern {
                    db_query = db_query.bind(pattern); // stream_id text search
                }

                let streams = db_query.bind(limit).bind(offset).fetch_all(pg_pool).await?;

                Ok(streams)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let mut query = String::from(
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
                        s.last_activity_at
                    FROM stream s
                    LEFT JOIN run r ON s.stream_id = r.stream_id
                    LEFT JOIN build b ON r.build_id = b.build_id
                    LEFT JOIN project p ON b.project_id = p.project_id
                    WHERE s.env_id = ?
                    "#,
                );

                // project_id filter removed

                if time_range_cutoff.is_some() {
                    query.push_str(" AND s.created_at >= ?");
                }

                // Add search filter with UUID optimization
                let search_uuid = search_term.and_then(|term| {
                    Uuid::parse_str(term).ok().or_else(|| {
                        if term.len() == 32 && term.chars().all(|c| c.is_ascii_hexdigit()) {
                            let with_dashes = format!(
                                "{}-{}-{}-{}-{}",
                                &term[0..8],
                                &term[8..12],
                                &term[12..16],
                                &term[16..20],
                                &term[20..32]
                            );
                            Uuid::parse_str(&with_dashes).ok()
                        } else {
                            None
                        }
                    })
                });

                // Check if search term is a short ID (last 12 chars of UUID)
                let is_short_id = search_term
                    .map(|term| term.len() == 12 && term.chars().all(|c| c.is_ascii_hexdigit()))
                    .unwrap_or(false);

                if search_uuid.is_some() {
                    query.push_str(" AND s.stream_id = ?");
                } else if is_short_id {
                    // Short ID detected (12 hex chars): pattern match on UUID suffix
                    query.push_str(" AND CAST(s.stream_id AS TEXT) LIKE ?");
                } else if search_term.is_some() {
                    // General text search on stream_id
                    query.push_str(" AND CAST(s.stream_id AS TEXT) LIKE ?");
                }

                query.push_str(" GROUP BY s.stream_id, s.env_id, s.started_at, s.last_activity_at, s.total_runs, s.total_events ORDER BY s.created_at DESC LIMIT ? OFFSET ?");

                // Prepare search - UUID exact match or pattern matching
                let search_uuid_sqlite = search_term.and_then(|term| {
                    Uuid::parse_str(term).ok().or_else(|| {
                        if term.len() == 32 && term.chars().all(|c| c.is_ascii_hexdigit()) {
                            let with_dashes = format!(
                                "{}-{}-{}-{}-{}",
                                &term[0..8],
                                &term[8..12],
                                &term[12..16],
                                &term[16..20],
                                &term[20..32]
                            );
                            Uuid::parse_str(&with_dashes).ok()
                        } else {
                            None
                        }
                    })
                });

                // Check if search term is a short ID (last 12 chars of UUID)
                let is_short_id_sqlite = search_term
                    .map(|term| term.len() == 12 && term.chars().all(|c| c.is_ascii_hexdigit()))
                    .unwrap_or(false);

                // Short ID suffix pattern for matching end of UUID
                let short_id_pattern = search_term.map(|term| format!("%{}", term));
                let search_pattern = search_term.map(|term| format!("%{}%", term));

                let mut db_query = sqlx::query_as::<_, StreamSummary>(&query).bind(env_id);

                // project_id filter removed

                if let Some(cutoff) = time_range_cutoff {
                    db_query = db_query.bind(cutoff.format("%Y-%m-%d %H:%M:%S").to_string());
                }

                if let Some(uuid) = search_uuid_sqlite {
                    db_query = db_query.bind(uuid); // stream_id exact match
                } else if is_short_id_sqlite {
                    if let Some(ref suffix_pattern) = short_id_pattern {
                        db_query = db_query.bind(suffix_pattern); // stream_id suffix match
                    }
                } else if let Some(ref pattern) = search_pattern {
                    db_query = db_query.bind(pattern); // stream_id text search
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

                // project_id filter removed

                if time_range_cutoff.is_some() {
                    param_count += 1;
                    query.push_str(&format!(" AND s.created_at >= ${}", param_count));
                }

                if search_term.is_some() {
                    let base = param_count + 1;
                    query.push_str(&format!(" AND CAST(s.stream_id AS TEXT) ILIKE ${}", base));
                }

                // Check if search term is a short ID (last 12 chars of UUID)
                let is_short_id = search_term
                    .map(|term| term.len() == 12 && term.chars().all(|c| c.is_ascii_hexdigit()))
                    .unwrap_or(false);

                // Prepare search pattern - use suffix pattern for short IDs
                let search_pattern = if is_short_id {
                    search_term.map(|term| format!("%{}", term))
                } else {
                    search_term.map(|term| format!("%{}%", term))
                };

                let mut db_query = sqlx::query_scalar::<_, i64>(&query).bind(env_id);

                // project_id filter removed

                if let Some(cutoff) = time_range_cutoff {
                    db_query = db_query.bind(cutoff);
                }

                if let Some(pattern) = &search_pattern {
                    db_query = db_query.bind(pattern); // stream_id
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

                // project_id filter removed

                if time_range_cutoff.is_some() {
                    query.push_str(" AND s.created_at >= ?");
                }

                if search_term.is_some() {
                    query.push_str(" AND CAST(s.stream_id AS TEXT) LIKE ?");
                }

                // Check if search term is a short ID (last 12 chars of UUID)
                let is_short_id = search_term
                    .map(|term| term.len() == 12 && term.chars().all(|c| c.is_ascii_hexdigit()))
                    .unwrap_or(false);

                // Prepare search pattern - use suffix pattern for short IDs
                let search_pattern = if is_short_id {
                    search_term.map(|term| format!("%{}", term))
                } else {
                    search_term.map(|term| format!("%{}%", term))
                };

                let mut db_query = sqlx::query_scalar::<_, i64>(&query).bind(env_id);

                // project_id filter removed

                if let Some(cutoff) = time_range_cutoff {
                    db_query = db_query.bind(cutoff.format("%Y-%m-%d %H:%M:%S").to_string());
                }

                if let Some(pattern) = &search_pattern {
                    db_query = db_query.bind(pattern); // stream_id
                }

                let count = db_query.fetch_one(sqlite_pool).await?;
                Ok(count)
            }
        }
    }
}

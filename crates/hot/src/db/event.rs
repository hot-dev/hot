use chrono::{DateTime, Utc};
use serde_json::Value as JsonValue;
use sqlx::FromRow;
use thiserror::Error;
use uuid::Uuid;

#[derive(Error, Debug)]
pub enum EventError {
    #[error("Database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("Event not found")]
    NotFound,
}

#[derive(Debug, Clone, FromRow)]
pub struct Event {
    pub event_id: Uuid,
    pub env_id: Uuid,
    pub stream_id: Uuid,
    pub event_type: String,
    pub event_data: JsonValue,
    pub event_time: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
    pub created_by_user_id: Uuid,
    pub handled: bool,
    // Access attribution - links to the access audit record for API-initiated events
    #[sqlx(default)]
    pub access_id: Option<Uuid>,
}

impl Event {
    /// Get event by ID
    pub async fn get_event(
        db: &crate::db::DatabasePool,
        event_id: &Uuid,
    ) -> Result<Event, EventError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let event = sqlx::query_as::<_, Event>(
                    "SELECT event_id, env_id, stream_id, event_type, event_data, event_time, created_at, created_by_user_id, handled, access_id FROM event WHERE event_id = $1"
                )
                .bind(event_id)
                .fetch_optional(pg_pool)
                .await
                .inspect_err(|e| tracing::error!("db error in Event::get_event: {}", e))?
                .ok_or(EventError::NotFound)?;
                Ok(event)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let event = sqlx::query_as::<_, Event>(
                    "SELECT event_id, env_id, stream_id, event_type, event_data, event_time, created_at, created_by_user_id, handled, access_id FROM event WHERE event_id = ?"
                )
                .bind(event_id)
                .fetch_optional(sqlite_pool)
                .await
                .inspect_err(|e| tracing::error!("db error in Event::get_event: {}", e))?
                .ok_or(EventError::NotFound)?;
                Ok(event)
            }
        }
    }

    /// Get count of events
    pub async fn get_count(db: &crate::db::DatabasePool) -> Result<i64, EventError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM event")
                    .fetch_one(pg_pool)
                    .await?;
                Ok(count)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM event")
                    .fetch_one(sqlite_pool)
                    .await?;
                Ok(count)
            }
        }
    }

    /// Get count of events by environment
    pub async fn get_count_by_env(
        db: &crate::db::DatabasePool,
        env_id: &Uuid,
    ) -> Result<i64, EventError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM event WHERE env_id = $1")
                    .bind(env_id)
                    .fetch_one(pg_pool)
                    .await?;
                Ok(count)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM event WHERE env_id = ?")
                    .bind(env_id)
                    .fetch_one(sqlite_pool)
                    .await?;
                Ok(count)
            }
        }
    }

    /// Get count of handled events by environment (events that have at least one associated run)
    pub async fn get_handled_count_by_env(
        db: &crate::db::DatabasePool,
        env_id: &Uuid,
    ) -> Result<i64, EventError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let count: i64 = sqlx::query_scalar(
                    "SELECT COUNT(*) FROM event WHERE env_id = $1 AND handled = true",
                )
                .bind(env_id)
                .fetch_one(pg_pool)
                .await?;
                Ok(count)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let count: i64 = sqlx::query_scalar(
                    "SELECT COUNT(*) FROM event WHERE env_id = ? AND handled = 1",
                )
                .bind(env_id)
                .fetch_one(sqlite_pool)
                .await?;
                Ok(count)
            }
        }
    }

    /// Get count of unhandled events by environment (events with no associated runs)
    pub async fn get_unhandled_count_by_env(
        db: &crate::db::DatabasePool,
        env_id: &Uuid,
    ) -> Result<i64, EventError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let count: i64 = sqlx::query_scalar(
                    "SELECT COUNT(*) FROM event WHERE env_id = $1 AND handled = false",
                )
                .bind(env_id)
                .fetch_one(pg_pool)
                .await?;
                Ok(count)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let count: i64 = sqlx::query_scalar(
                    "SELECT COUNT(*) FROM event WHERE env_id = ? AND handled = 0",
                )
                .bind(env_id)
                .fetch_one(sqlite_pool)
                .await?;
                Ok(count)
            }
        }
    }

    /// Mark an event as handled (when a run is created for it)
    pub async fn mark_event_as_handled(
        db: &crate::db::DatabasePool,
        event_id: &Uuid,
    ) -> Result<(), EventError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                sqlx::query("UPDATE event SET handled = true WHERE event_id = $1")
                    .bind(event_id)
                    .execute(pg_pool)
                    .await?;
                Ok(())
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                sqlx::query("UPDATE event SET handled = 1 WHERE event_id = ?")
                    .bind(event_id)
                    .execute(sqlite_pool)
                    .await?;
                Ok(())
            }
        }
    }

    /// Get events by environment with optional handled filter and pagination.
    ///
    /// `time_range_cutoff` is the earliest `created_at` timestamp to include
    /// (inclusive); pass `None` for "all time". The handler should compute
    /// this via [`crate::time_range::parse_time_range_cutoff`].
    #[allow(clippy::too_many_arguments)]
    pub async fn get_events_by_env_filtered(
        db: &crate::db::DatabasePool,
        env_id: &Uuid,
        handled_filter: Option<bool>, // None = all, Some(true) = handled only, Some(false) = unhandled only
        time_range_cutoff: Option<chrono::DateTime<chrono::Utc>>,
        search_term: Option<&str>, // Optional search term for Event ID, Stream ID, Event Type
        limit: Option<i64>,
        offset: Option<i64>,
    ) -> Result<Vec<Event>, EventError> {
        let limit = limit.unwrap_or(10);
        let offset = offset.unwrap_or(0);

        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let mut query = String::from(
                    r#"
                    SELECT event_id, env_id, stream_id, event_type, event_data, event_time, created_at, created_by_user_id, handled
                    FROM event
                    WHERE env_id = $1
                    "#,
                );

                let mut param_count = 1;

                if let Some(_handled) = handled_filter {
                    param_count += 1;
                    query.push_str(&format!(" AND handled = ${}", param_count));
                }

                // Note: project_id filter removed - projects are now derived from runs, not streams

                if time_range_cutoff.is_some() {
                    param_count += 1;
                    query.push_str(&format!(" AND created_at >= ${}", param_count));
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
                    // UUID detected: exact match on UUID fields + pattern match on text fields
                    let base = param_count + 1;
                    query.push_str(&format!(
                        " AND (event_id = ${} OR stream_id = ${} OR event_type ILIKE ${} OR CAST(event_data AS TEXT) ILIKE ${})",
                        base, base+1, base+2, base+3
                    ));
                    param_count += 4;
                } else if is_short_id {
                    // Short ID detected (12 hex chars): pattern match on UUID suffix + text fields
                    let base = param_count + 1;
                    query.push_str(&format!(
                        " AND (CAST(event_id AS TEXT) ILIKE ${} OR CAST(stream_id AS TEXT) ILIKE ${} OR event_type ILIKE ${} OR CAST(event_data AS TEXT) ILIKE ${})",
                        base, base+1, base+2, base+3
                    ));
                    param_count += 4;
                } else if search_term.is_some() {
                    // Pattern matching for non-UUID searches
                    let base = param_count + 1;
                    query.push_str(&format!(
                        " AND (event_type ILIKE ${} OR CAST(event_data AS TEXT) ILIKE ${})",
                        base,
                        base + 1
                    ));
                    param_count += 2;
                }

                param_count += 1;
                let limit_param = param_count;
                param_count += 1;
                let offset_param = param_count;

                query.push_str(&format!(
                    " ORDER BY event_time DESC LIMIT ${} OFFSET ${}",
                    limit_param, offset_param
                ));

                // Prepare search - UUID exact match + pattern matching for user data
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

                let search_pattern = search_term.map(|t| format!("%{}%", t));
                // Short ID suffix pattern for matching end of UUID
                let short_id_pattern = search_term.map(|term| format!("%{}", term));
                let handled_value = handled_filter;

                let mut db_query = sqlx::query_as::<_, Event>(&query).bind(env_id);

                if let Some(handled) = handled_value {
                    db_query = db_query.bind(handled);
                }

                // project_id filter removed

                if let Some(cutoff) = time_range_cutoff {
                    db_query = db_query.bind(cutoff);
                }

                if let Some(uuid) = search_uuid {
                    // Bind UUID for exact matching on UUID fields
                    db_query = db_query
                        .bind(uuid) // event_id
                        .bind(uuid); // stream_id
                    // Also bind pattern for searching UUID string in text fields
                    if let Some(ref pattern) = search_pattern {
                        db_query = db_query
                            .bind(pattern) // event_type
                            .bind(pattern); // event_data
                    }
                } else if is_short_id {
                    // Bind short ID suffix pattern for UUID fields + regular pattern for text fields
                    if let Some(ref suffix_pattern) = short_id_pattern {
                        db_query = db_query
                            .bind(suffix_pattern) // event_id suffix
                            .bind(suffix_pattern); // stream_id suffix
                    }
                    if let Some(ref pattern) = search_pattern {
                        db_query = db_query
                            .bind(pattern) // event_type
                            .bind(pattern); // event_data
                    }
                } else if let Some(ref pattern) = search_pattern {
                    // Bind pattern for text search
                    db_query = db_query
                        .bind(pattern) // event_type
                        .bind(pattern); // event_data
                }

                let events = db_query.bind(limit).bind(offset).fetch_all(pg_pool).await?;

                Ok(events)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let mut query = String::from(
                    r#"
                    SELECT event_id, env_id, stream_id, event_type, event_data, event_time, created_at, created_by_user_id, handled
                    FROM event
                    WHERE env_id = ?
                    "#,
                );

                if handled_filter.is_some() {
                    query.push_str(" AND handled = ?");
                }

                // project_id filter removed

                if time_range_cutoff.is_some() {
                    query.push_str(" AND created_at >= ?");
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
                    // UUID detected: exact match on UUID fields + pattern match on text fields
                    query.push_str(" AND (event_id = ? OR stream_id = ? OR event_type LIKE ? OR CAST(event_data AS TEXT) LIKE ?)");
                } else if is_short_id {
                    // Short ID detected (12 hex chars): pattern match on UUID suffix + text fields
                    query.push_str(" AND (CAST(event_id AS TEXT) LIKE ? OR CAST(stream_id AS TEXT) LIKE ? OR event_type LIKE ? OR CAST(event_data AS TEXT) LIKE ?)");
                } else if search_term.is_some() {
                    // Pattern matching for non-UUID searches
                    query.push_str(" AND (event_type LIKE ? OR CAST(event_data AS TEXT) LIKE ?)");
                }

                query.push_str(" ORDER BY event_time DESC LIMIT ? OFFSET ?");

                // Prepare search - UUID exact match + pattern matching for user data
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

                let search_pattern = search_term.map(|t| format!("%{}%", t));
                // Short ID suffix pattern for matching end of UUID
                let short_id_pattern = search_term.map(|term| format!("%{}", term));
                let handled_value = handled_filter;

                let mut db_query = sqlx::query_as::<_, Event>(&query).bind(env_id);

                if let Some(handled) = handled_value {
                    let handled_int = if handled { 1 } else { 0 };
                    db_query = db_query.bind(handled_int);
                }

                // project_id filter removed

                if let Some(cutoff) = time_range_cutoff {
                    db_query = db_query.bind(cutoff.format("%Y-%m-%d %H:%M:%S").to_string());
                }

                if let Some(uuid) = search_uuid_sqlite {
                    // Bind UUID for exact matching on UUID fields
                    db_query = db_query
                        .bind(uuid) // event_id
                        .bind(uuid); // stream_id
                    // Also bind pattern for searching UUID string in text fields
                    if let Some(ref pattern) = search_pattern {
                        db_query = db_query
                            .bind(pattern) // event_type
                            .bind(pattern); // event_data
                    }
                } else if is_short_id_sqlite {
                    // Bind short ID suffix pattern for UUID fields + regular pattern for text fields
                    if let Some(ref suffix_pattern) = short_id_pattern {
                        db_query = db_query
                            .bind(suffix_pattern) // event_id suffix
                            .bind(suffix_pattern); // stream_id suffix
                    }
                    if let Some(ref pattern) = search_pattern {
                        db_query = db_query
                            .bind(pattern) // event_type
                            .bind(pattern); // event_data
                    }
                } else if let Some(ref pattern) = search_pattern {
                    // Bind pattern for text search
                    db_query = db_query
                        .bind(pattern) // event_type
                        .bind(pattern); // event_data
                }

                let events = db_query
                    .bind(limit)
                    .bind(offset)
                    .fetch_all(sqlite_pool)
                    .await?;

                Ok(events)
            }
        }
    }

    /// Get count of filtered events by environment.
    ///
    /// See [`Self::get_events_by_env_filtered`] for the meaning of `time_range_cutoff`.
    pub async fn get_filtered_count_by_env(
        db: &crate::db::DatabasePool,
        env_id: &Uuid,
        handled_filter: Option<bool>,
        time_range_cutoff: Option<chrono::DateTime<chrono::Utc>>,
        search_term: Option<&str>,
    ) -> Result<i64, EventError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let mut query = String::from(
                    r#"
                    SELECT COUNT(*)
                    FROM event
                    WHERE env_id = $1
                    "#,
                );

                let mut param_count = 1;

                if let Some(_handled) = handled_filter {
                    param_count += 1;
                    query.push_str(&format!(" AND handled = ${}", param_count));
                }

                // project_id filter removed

                if time_range_cutoff.is_some() {
                    param_count += 1;
                    query.push_str(&format!(" AND created_at >= ${}", param_count));
                }

                if search_term.is_some() {
                    let base = param_count + 1;
                    query.push_str(&format!(
                        " AND (CAST(event_id AS TEXT) ILIKE ${} OR CAST(stream_id AS TEXT) ILIKE ${} OR event_type ILIKE ${})",
                        base, base+1, base+2
                    ));
                }

                // Check if search term is a short ID (last 12 chars of UUID)
                let is_short_id = search_term
                    .map(|term| term.len() == 12 && term.chars().all(|c| c.is_ascii_hexdigit()))
                    .unwrap_or(false);

                // Prepare search pattern - use suffix pattern for UUID fields when short ID
                let uuid_pattern = if is_short_id {
                    search_term.map(|term| format!("%{}", term))
                } else {
                    search_term.map(|term| format!("%{}%", term))
                };
                let text_pattern = search_term.map(|term| format!("%{}%", term));
                let handled_value = handled_filter;

                let mut db_query = sqlx::query_scalar::<_, i64>(&query).bind(env_id);

                if let Some(handled) = handled_value {
                    db_query = db_query.bind(handled);
                }

                // project_id filter removed

                if let Some(cutoff) = time_range_cutoff {
                    db_query = db_query.bind(cutoff);
                }

                if search_term.is_some() {
                    if let Some(ref pattern) = uuid_pattern {
                        db_query = db_query
                            .bind(pattern) // event_id
                            .bind(pattern); // stream_id
                    }
                    if let Some(ref pattern) = text_pattern {
                        db_query = db_query.bind(pattern); // event_type
                    }
                }

                let count = db_query.fetch_one(pg_pool).await?;
                Ok(count)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let mut query = String::from(
                    r#"
                    SELECT COUNT(*)
                    FROM event
                    WHERE env_id = ?
                    "#,
                );

                if handled_filter.is_some() {
                    query.push_str(" AND handled = ?");
                }

                // project_id filter removed

                if time_range_cutoff.is_some() {
                    query.push_str(" AND created_at >= ?");
                }

                if search_term.is_some() {
                    query.push_str(" AND (CAST(event_id AS TEXT) LIKE ? OR CAST(stream_id AS TEXT) LIKE ? OR event_type LIKE ?)");
                }

                // Check if search term is a short ID (last 12 chars of UUID)
                let is_short_id = search_term
                    .map(|term| term.len() == 12 && term.chars().all(|c| c.is_ascii_hexdigit()))
                    .unwrap_or(false);

                // Prepare search pattern - use suffix pattern for UUID fields when short ID
                let uuid_pattern = if is_short_id {
                    search_term.map(|term| format!("%{}", term))
                } else {
                    search_term.map(|term| format!("%{}%", term))
                };
                let text_pattern = search_term.map(|term| format!("%{}%", term));
                let handled_value = handled_filter;

                let mut db_query = sqlx::query_scalar::<_, i64>(&query).bind(env_id);

                if let Some(handled) = handled_value {
                    let handled_int = if handled { 1 } else { 0 };
                    db_query = db_query.bind(handled_int);
                }

                // project_id filter removed

                if let Some(cutoff) = time_range_cutoff {
                    db_query = db_query.bind(cutoff.format("%Y-%m-%d %H:%M:%S").to_string());
                }

                if search_term.is_some() {
                    if let Some(ref pattern) = uuid_pattern {
                        db_query = db_query
                            .bind(pattern) // event_id
                            .bind(pattern); // stream_id
                    }
                    if let Some(ref pattern) = text_pattern {
                        db_query = db_query.bind(pattern); // event_type
                    }
                }

                let count = db_query.fetch_one(sqlite_pool).await?;
                Ok(count)
            }
        }
    }

    /// Get events by environment with pagination
    pub async fn get_events_by_env(
        db: &crate::db::DatabasePool,
        env_id: &Uuid,
        limit: Option<i64>,
        offset: Option<i64>,
    ) -> Result<Vec<Event>, EventError> {
        let limit = limit.unwrap_or(10);
        let offset = offset.unwrap_or(0);

        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let events = sqlx::query_as::<_, Event>(
                    "SELECT event_id, env_id, stream_id, event_type, event_data, event_time, created_at, created_by_user_id, handled FROM event WHERE env_id = $1 ORDER BY event_time DESC LIMIT $2 OFFSET $3"
                )
                .bind(env_id)
                .bind(limit)
                .bind(offset)
                .fetch_all(pg_pool)
                .await?;
                Ok(events)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let events = sqlx::query_as::<_, Event>(
                    "SELECT event_id, env_id, stream_id, event_type, event_data, event_time, created_at, created_by_user_id, handled FROM event WHERE env_id = ? ORDER BY event_time DESC LIMIT ? OFFSET ?"
                )
                .bind(env_id)
                .bind(limit)
                .bind(offset)
                .fetch_all(sqlite_pool)
                .await?;

                Ok(events)
            }
        }
    }

    /// Get all events with pagination
    pub async fn get_all_events(
        db: &crate::db::DatabasePool,
        limit: Option<i64>,
        offset: Option<i64>,
    ) -> Result<Vec<Event>, EventError> {
        let limit = limit.unwrap_or(10);
        let offset = offset.unwrap_or(0);

        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let events = sqlx::query_as::<_, Event>(
                    "SELECT event_id, env_id, stream_id, event_type, event_data, event_time, created_at, created_by_user_id, handled FROM event ORDER BY event_time DESC LIMIT $1 OFFSET $2"
                )
                .bind(limit)
                .bind(offset)
                .fetch_all(pg_pool)
                .await?;
                Ok(events)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let events = sqlx::query_as::<_, Event>(
                    "SELECT event_id, env_id, stream_id, event_type, event_data, event_time, created_at, created_by_user_id, handled FROM event ORDER BY event_time DESC LIMIT ? OFFSET ?"
                )
                .bind(limit)
                .bind(offset)
                .fetch_all(sqlite_pool)
                .await?;

                Ok(events)
            }
        }
    }

    /// Get events by type with pagination
    pub async fn get_events_by_type(
        db: &crate::db::DatabasePool,
        event_type: &str,
        limit: Option<i64>,
        offset: Option<i64>,
    ) -> Result<Vec<Event>, EventError> {
        let limit = limit.unwrap_or(10);
        let offset = offset.unwrap_or(0);

        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let events = sqlx::query_as::<_, Event>(
                    "SELECT event_id, env_id, stream_id, event_type, event_data, event_time, created_at, created_by_user_id, handled FROM event WHERE event_type = $1 ORDER BY event_time DESC LIMIT $2 OFFSET $3"
                )
                .bind(event_type)
                .bind(limit)
                .bind(offset)
                .fetch_all(pg_pool)
                .await?;
                Ok(events)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let events = sqlx::query_as::<_, Event>(
                    "SELECT event_id, env_id, stream_id, event_type, event_data, event_time, created_at, created_by_user_id, handled FROM event WHERE event_type = ? ORDER BY event_time DESC LIMIT ? OFFSET ?"
                )
                .bind(event_type)
                .bind(limit)
                .bind(offset)
                .fetch_all(sqlite_pool)
                .await?;

                Ok(events)
            }
        }
    }

    /// Insert a new event
    ///
    /// This function automatically ensures the stream exists before inserting the event.
    /// If the stream doesn't exist, it will be created with the provided parameters.
    #[allow(clippy::too_many_arguments)]
    pub async fn insert_event(
        db: &crate::db::DatabasePool,
        event_id: &Uuid,
        env_id: &Uuid,
        stream_id: &Uuid,
        event_type: &str,
        event_data: &JsonValue,
        event_time: DateTime<Utc>,
        created_by_user_id: &Uuid,
        access_id: Option<&Uuid>,
    ) -> Result<(), EventError> {
        // Ensure the stream exists (create if it doesn't)
        crate::db::stream::Stream::create_or_get_stream(db, *stream_id, *env_id)
            .await
            .map_err(|e| {
                EventError::Database(sqlx::Error::Protocol(format!(
                    "Failed to create stream: {}",
                    e
                )))
            })?;

        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                sqlx::query("INSERT INTO event (event_id, env_id, stream_id, event_type, event_data, event_time, created_by_user_id, access_id) VALUES ($1, $2, $3, $4, $5, $6, $7, $8)")
                    .bind(event_id)
                    .bind(env_id)
                    .bind(stream_id)
                    .bind(event_type)
                    .bind(event_data)
                    .bind(event_time)
                    .bind(created_by_user_id)
                    .bind(access_id)
                    .execute(pg_pool)
                    .await?;
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let event_data_str = serde_json::to_string(event_data).map_err(|e| {
                    EventError::Database(sqlx::Error::Protocol(format!(
                        "JSON serialization error: {}",
                        e
                    )))
                })?;

                sqlx::query(
                    "INSERT INTO event (event_id, env_id, stream_id, event_type, event_data, event_time, created_by_user_id, access_id) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
                )
                .bind(event_id)
                .bind(env_id)
                .bind(stream_id)
                .bind(event_type)
                .bind(event_data_str)
                .bind(event_time)
                .bind(created_by_user_id)
                .bind(access_id)
                .execute(sqlite_pool)
                .await?;
            }
        }

        // Update stream metrics after inserting the event
        crate::db::stream::Stream::update_metrics(db, stream_id)
            .await
            .map_err(|e| {
                EventError::Database(sqlx::Error::Protocol(format!(
                    "Failed to update stream metrics: {}",
                    e
                )))
            })?;

        Ok(())
    }

    /// Delete event
    pub async fn delete_event(
        db: &crate::db::DatabasePool,
        event_id: &Uuid,
    ) -> Result<(), EventError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                sqlx::query("DELETE FROM event WHERE event_id = $1")
                    .bind(event_id)
                    .execute(pg_pool)
                    .await?;
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                sqlx::query("DELETE FROM event WHERE event_id = ?")
                    .bind(event_id)
                    .execute(sqlite_pool)
                    .await?;
            }
        }
        Ok(())
    }

    /// Get runs triggered by event ID
    pub async fn get_runs_by_event(
        db: &crate::db::DatabasePool,
        event_id: &Uuid,
        limit: Option<i64>,
        offset: Option<i64>,
    ) -> Result<Vec<crate::db::run::Run>, EventError> {
        let limit = limit.unwrap_or(10);
        let offset = offset.unwrap_or(0);

        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let runs = sqlx::query_as::<_, crate::db::run::Run>(
                    "SELECT r.run_id, r.env_id, r.stream_id, r.build_id, r.run_type_id, rt.run_type, r.origin_run_id, r.event_id, r.start_time, r.stop_time, r.status_id, rs.status, r.by_user_id, r.result, r.info, b.project_id, p.name as project_name, e.event_data->>'fn' as event_fn, r.retry_attempt, r.next_retry_at, e.created_at as queued_at
                     FROM run r
                     JOIN run_type rt ON r.run_type_id = rt.run_type_id
                     JOIN run_status rs ON r.status_id = rs.status_id
                     LEFT JOIN build b ON r.build_id = b.build_id
                     LEFT JOIN project p ON b.project_id = p.project_id
                     LEFT JOIN event e ON r.event_id = e.event_id
                     WHERE r.event_id = $1
                     ORDER BY r.start_time DESC
                     LIMIT $2 OFFSET $3"
                )
                .bind(event_id)
                .bind(limit)
                .bind(offset)
                .fetch_all(pg_pool)
                .await?;
                Ok(runs)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let runs = sqlx::query_as::<_, crate::db::run::Run>(
                    "SELECT r.run_id, r.env_id, r.stream_id, r.build_id, r.run_type_id, rt.run_type, r.origin_run_id, r.event_id, r.start_time, r.stop_time, r.status_id, rs.status, r.by_user_id, r.result, r.info, b.project_id, p.name as project_name, json_extract(e.event_data, '$.fn') as event_fn, r.retry_attempt, r.next_retry_at, e.created_at as queued_at
                     FROM run r
                     JOIN run_type rt ON r.run_type_id = rt.run_type_id
                     JOIN run_status rs ON r.status_id = rs.status_id
                     LEFT JOIN build b ON r.build_id = b.build_id
                     LEFT JOIN project p ON b.project_id = p.project_id
                     LEFT JOIN event e ON r.event_id = e.event_id
                     WHERE r.event_id = ?
                     ORDER BY r.start_time DESC
                     LIMIT ? OFFSET ?"
                )
                .bind(event_id)
                .bind(limit)
                .bind(offset)
                .fetch_all(sqlite_pool)
                .await?;
                Ok(runs)
            }
        }
    }

    /// Get count of runs triggered by event
    pub async fn get_run_count_by_event(
        db: &crate::db::DatabasePool,
        event_id: &Uuid,
    ) -> Result<i64, EventError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM run WHERE event_id = $1")
                    .bind(event_id)
                    .fetch_one(pg_pool)
                    .await?;
                Ok(count)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM run WHERE event_id = ?")
                    .bind(event_id)
                    .fetch_one(sqlite_pool)
                    .await?;
                Ok(count)
            }
        }
    }

    /// Get events likely published by a specific run
    /// This finds events in the same environment that were created during or shortly after the run
    pub async fn get_events_likely_published_by_run(
        db: &crate::db::DatabasePool,
        run: &crate::db::run::Run,
        limit: Option<i64>,
    ) -> Result<Vec<Event>, EventError> {
        let limit = limit.unwrap_or(10);

        // Use run start time as the earliest possible creation time
        let run_start = run.start_time;

        // Use run stop time (if available) plus a buffer, or current time as the latest
        let run_end = run.stop_time.unwrap_or_else(chrono::Utc::now);
        let search_end = run_end + chrono::Duration::minutes(5); // 5-minute buffer after run completion

        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                // If run has a user, filter by the same user
                if let Some(by_user_id) = &run.by_user_id {
                    let query = sqlx::query_as::<_, Event>(
                        "SELECT event_id, env_id, stream_id, event_type, event_data, event_time, created_at, created_by_user_id
                         FROM event
                         WHERE env_id = $1
                           AND created_at >= $2
                           AND created_at <= $3
                           AND created_by_user_id = $4
                         ORDER BY created_at ASC
                         LIMIT $5"
                    ).bind(run.env_id)
                     .bind(run_start)
                     .bind(search_end)
                     .bind(by_user_id)
                     .bind(limit);

                    let events = query.fetch_all(pg_pool).await?;
                    Ok(events)
                } else {
                    let query = sqlx::query_as::<_, Event>(
                        "SELECT event_id, env_id, stream_id, event_type, event_data, event_time, created_at, created_by_user_id
                         FROM event
                         WHERE env_id = $1
                           AND created_at >= $2
                           AND created_at <= $3
                         ORDER BY created_at ASC
                         LIMIT $4"
                    ).bind(run.env_id)
                     .bind(run_start)
                     .bind(search_end)
                     .bind(limit);

                    let events = query.fetch_all(pg_pool).await?;
                    Ok(events)
                }
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                // If run has a user, filter by the same user
                if let Some(by_user_id) = &run.by_user_id {
                    let query = sqlx::query_as::<_, Event>(
                        "SELECT event_id, env_id, stream_id, event_type, event_data, event_time, created_at, created_by_user_id
                         FROM event
                         WHERE env_id = ?
                           AND created_at >= ?
                           AND created_at <= ?
                           AND created_by_user_id = ?
                         ORDER BY created_at ASC
                         LIMIT ?"
                    ).bind(run.env_id)
                     .bind(run_start)
                     .bind(search_end)
                     .bind(by_user_id)
                     .bind(limit);

                    let events = query.fetch_all(sqlite_pool).await?;
                    Ok(events)
                } else {
                    let query = sqlx::query_as::<_, Event>(
                        "SELECT event_id, env_id, stream_id, event_type, event_data, event_time, created_at, created_by_user_id
                         FROM event
                         WHERE env_id = ?
                           AND created_at >= ?
                           AND created_at <= ?
                         ORDER BY created_at ASC
                         LIMIT ?"
                    ).bind(run.env_id)
                     .bind(run_start)
                     .bind(search_end)
                     .bind(limit);

                    let events = query.fetch_all(sqlite_pool).await?;
                    Ok(events)
                }
            }
        }
    }

    /// Get events by stream_id with pagination
    pub async fn get_events_by_stream(
        db: &crate::db::DatabasePool,
        stream_id: &Uuid,
        limit: Option<i64>,
        offset: Option<i64>,
    ) -> Result<Vec<Event>, EventError> {
        let limit = limit.unwrap_or(50);
        let offset = offset.unwrap_or(0);

        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let events = sqlx::query_as::<_, Event>(
                    "SELECT event_id, env_id, stream_id, event_type, event_data, event_time, created_at, created_by_user_id, handled FROM event WHERE stream_id = $1 ORDER BY event_time DESC LIMIT $2 OFFSET $3"
                )
                .bind(stream_id)
                .bind(limit)
                .bind(offset)
                .fetch_all(pg_pool)
                .await?;
                Ok(events)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let events = sqlx::query_as::<_, Event>(
                    "SELECT event_id, env_id, stream_id, event_type, event_data, event_time, created_at, created_by_user_id, handled FROM event WHERE stream_id = ? ORDER BY event_time DESC LIMIT ? OFFSET ?"
                )
                .bind(stream_id)
                .bind(limit)
                .bind(offset)
                .fetch_all(sqlite_pool)
                .await?;
                Ok(events)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::db::create_db_pool;
    use crate::val;

    #[tokio::test]
    async fn test_event_operations() {
        // Use a temp directory for the test database
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir.path().join("test.db");
        let db_uri = format!("sqlite:{}", db_path.display());

        let conf = val!({
            "uri": db_uri
        });
        let _db = create_db_pool(&conf).await.unwrap();

        // Test basic operations would go here
    }
}

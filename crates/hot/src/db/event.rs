use chrono::{DateTime, Utc};
use serde_json::Value as JsonValue;
use sqlx::FromRow;
use thiserror::Error;
use uuid::Uuid;

use super::search::IdSearch;

#[derive(Error, Debug)]
pub enum EventError {
    #[error("Database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("Event not found")]
    NotFound,
    #[error("Event {0} already exists")]
    AlreadyExists(Uuid),
}

/// SQLite's `created_at` holds text in two shapes: `YYYY-MM-DD HH:MM:SS` from
/// the column default and RFC 3339 with a `T` from an explicit bind. Compared
/// as text, a `T` row sorts after every space-separated time on its day, so
/// the exact cutoff goes through `julianday`. The date-only bound comes first
/// because it is correct for both shapes as text and can use an index.
const SQLITE_CREATED_SINCE: &str = " AND created_at >= ? AND julianday(created_at) >= julianday(?)";

/// Where a resend of a publish stands with its event's enqueue (see
/// [`Event::claim_enqueue`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnqueueClaim {
    /// The event is enqueued, so nothing is owed.
    Settled,
    /// Another publish holds the lease, and may be enqueuing right now.
    Held,
    /// The lease had ended and is now the caller's, who owes the enqueue.
    Claimed,
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
                sqlx::query(
                    "UPDATE event SET handled = true WHERE event_id = $1 AND handled = false",
                )
                .bind(event_id)
                .execute(pg_pool)
                .await?;
                Ok(())
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                sqlx::query("UPDATE event SET handled = 1 WHERE event_id = ? AND handled = 0")
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

                let id_search = IdSearch::parse(search_term);

                if let Some(search) = &id_search
                    && search.uuid().is_some()
                {
                    // UUID detected: exact match on UUID fields + pattern match on text fields
                    let base = param_count + 1;
                    query.push_str(&format!(
                        " AND (event_id = ${} OR stream_id = ${} OR event_type ILIKE ${} OR CAST(event_data AS TEXT) ILIKE ${})",
                        base, base+1, base+2, base+3
                    ));
                    param_count += 4;
                } else if let Some(search) = &id_search
                    && search.is_short_id()
                {
                    // Short ID detected (12 hex chars): pattern match on UUID suffix + text fields
                    let base = param_count + 1;
                    query.push_str(&format!(
                        " AND (CAST(event_id AS TEXT) ILIKE ${} OR CAST(stream_id AS TEXT) ILIKE ${} OR event_type ILIKE ${} OR CAST(event_data AS TEXT) ILIKE ${})",
                        base, base+1, base+2, base+3
                    ));
                    param_count += 4;
                } else if id_search.is_some() {
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

                let handled_value = handled_filter;

                let mut db_query =
                    sqlx::query_as::<_, Event>(sqlx::AssertSqlSafe(query.as_str())).bind(env_id);

                if let Some(handled) = handled_value {
                    db_query = db_query.bind(handled);
                }

                // project_id filter removed

                if let Some(cutoff) = time_range_cutoff {
                    db_query = db_query.bind(cutoff);
                }

                if let Some(search) = &id_search
                    && let Some(uuid) = search.uuid()
                {
                    // Bind UUID for exact matching on UUID fields
                    db_query = db_query
                        .bind(uuid) // event_id
                        .bind(uuid); // stream_id
                    // Also bind pattern for searching UUID string in text fields
                    db_query = db_query
                        .bind(search.text_pattern()) // event_type
                        .bind(search.text_pattern()); // event_data
                } else if let Some(search) = &id_search
                    && let Some(suffix_pattern) = search.suffix_pattern()
                {
                    // Bind short ID suffix pattern for UUID fields + regular pattern for text fields
                    db_query = db_query
                        .bind(suffix_pattern) // event_id suffix
                        .bind(suffix_pattern) // stream_id suffix
                        .bind(search.text_pattern()) // event_type
                        .bind(search.text_pattern()); // event_data
                } else if let Some(search) = &id_search {
                    // Bind pattern for text search
                    db_query = db_query
                        .bind(search.text_pattern()) // event_type
                        .bind(search.text_pattern()); // event_data
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
                    query.push_str(SQLITE_CREATED_SINCE);
                }

                let id_search = IdSearch::parse(search_term);

                if let Some(search) = &id_search
                    && search.uuid().is_some()
                {
                    // UUID detected: exact match on UUID fields + pattern match on text fields
                    query.push_str(" AND (event_id = ? OR stream_id = ? OR event_type LIKE ? OR CAST(event_data AS TEXT) LIKE ?)");
                } else if let Some(search) = &id_search
                    && search.is_short_id()
                {
                    // Short ID detected (12 hex chars): pattern match on UUID suffix + text fields
                    query.push_str(" AND (LOWER(HEX(event_id)) LIKE ? OR LOWER(HEX(stream_id)) LIKE ? OR event_type LIKE ? OR CAST(event_data AS TEXT) LIKE ?)");
                } else if id_search.is_some() {
                    // Pattern matching for non-UUID searches
                    query.push_str(" AND (event_type LIKE ? OR CAST(event_data AS TEXT) LIKE ?)");
                }

                query.push_str(" ORDER BY event_time DESC LIMIT ? OFFSET ?");

                let handled_value = handled_filter;

                let mut db_query =
                    sqlx::query_as::<_, Event>(sqlx::AssertSqlSafe(query.as_str())).bind(env_id);

                if let Some(handled) = handled_value {
                    let handled_int = if handled { 1 } else { 0 };
                    db_query = db_query.bind(handled_int);
                }

                // project_id filter removed

                if let Some(cutoff) = time_range_cutoff {
                    db_query = db_query
                        .bind(cutoff.format("%Y-%m-%d").to_string())
                        .bind(cutoff.format("%Y-%m-%d %H:%M:%S%.3f").to_string());
                }

                if let Some(search) = &id_search
                    && let Some(uuid) = search.uuid()
                {
                    // Bind UUID for exact matching on UUID fields
                    db_query = db_query
                        .bind(uuid) // event_id
                        .bind(uuid); // stream_id
                    // Also bind pattern for searching UUID string in text fields
                    db_query = db_query
                        .bind(search.text_pattern()) // event_type
                        .bind(search.text_pattern()); // event_data
                } else if let Some(search) = &id_search
                    && let Some(suffix_pattern) = search.suffix_pattern()
                {
                    // Bind short ID suffix pattern for UUID fields + regular pattern for text fields
                    db_query = db_query
                        .bind(suffix_pattern) // event_id suffix
                        .bind(suffix_pattern) // stream_id suffix
                        .bind(search.text_pattern()) // event_type
                        .bind(search.text_pattern()); // event_data
                } else if let Some(search) = &id_search {
                    // Bind pattern for text search
                    db_query = db_query
                        .bind(search.text_pattern()) // event_type
                        .bind(search.text_pattern()); // event_data
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

                let id_search = IdSearch::parse(search_term);

                if let Some(search) = &id_search
                    && search.uuid().is_some()
                {
                    let base = param_count + 1;
                    query.push_str(&format!(
                        " AND (event_id = ${} OR stream_id = ${} OR event_type ILIKE ${})",
                        base,
                        base + 1,
                        base + 2
                    ));
                } else if let Some(search) = &id_search
                    && search.is_short_id()
                {
                    let base = param_count + 1;
                    query.push_str(&format!(
                        " AND (CAST(event_id AS TEXT) ILIKE ${} OR CAST(stream_id AS TEXT) ILIKE ${} OR event_type ILIKE ${})",
                        base,
                        base + 1,
                        base + 2
                    ));
                } else if id_search.is_some() {
                    let base = param_count + 1;
                    query.push_str(&format!(
                        " AND (CAST(event_id AS TEXT) ILIKE ${} OR CAST(stream_id AS TEXT) ILIKE ${} OR event_type ILIKE ${})",
                        base,
                        base + 1,
                        base + 2
                    ));
                }

                let handled_value = handled_filter;

                let mut db_query =
                    sqlx::query_scalar::<_, i64>(sqlx::AssertSqlSafe(query.as_str())).bind(env_id);

                if let Some(handled) = handled_value {
                    db_query = db_query.bind(handled);
                }

                // project_id filter removed

                if let Some(cutoff) = time_range_cutoff {
                    db_query = db_query.bind(cutoff);
                }

                if let Some(search) = &id_search
                    && let Some(uuid) = search.uuid()
                {
                    db_query = db_query
                        .bind(uuid) // event_id
                        .bind(uuid) // stream_id
                        .bind(search.text_pattern()); // event_type
                } else if let Some(search) = &id_search
                    && let Some(suffix_pattern) = search.suffix_pattern()
                {
                    db_query = db_query
                        .bind(suffix_pattern) // event_id suffix
                        .bind(suffix_pattern) // stream_id suffix
                        .bind(search.text_pattern()); // event_type
                } else if let Some(search) = &id_search {
                    db_query = db_query
                        .bind(search.text_pattern()) // event_id
                        .bind(search.text_pattern()) // stream_id
                        .bind(search.text_pattern()); // event_type
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
                    query.push_str(SQLITE_CREATED_SINCE);
                }

                let id_search = IdSearch::parse(search_term);

                if let Some(search) = &id_search
                    && search.uuid().is_some()
                {
                    query.push_str(" AND (event_id = ? OR stream_id = ? OR event_type LIKE ?)");
                } else if let Some(search) = &id_search
                    && search.is_short_id()
                {
                    query.push_str(" AND (LOWER(HEX(event_id)) LIKE ? OR LOWER(HEX(stream_id)) LIKE ? OR event_type LIKE ?)");
                } else if id_search.is_some() {
                    query.push_str(" AND (LOWER(HEX(event_id)) LIKE ? OR LOWER(HEX(stream_id)) LIKE ? OR event_type LIKE ?)");
                }

                let handled_value = handled_filter;

                let mut db_query =
                    sqlx::query_scalar::<_, i64>(sqlx::AssertSqlSafe(query.as_str())).bind(env_id);

                if let Some(handled) = handled_value {
                    let handled_int = if handled { 1 } else { 0 };
                    db_query = db_query.bind(handled_int);
                }

                // project_id filter removed

                if let Some(cutoff) = time_range_cutoff {
                    db_query = db_query
                        .bind(cutoff.format("%Y-%m-%d").to_string())
                        .bind(cutoff.format("%Y-%m-%d %H:%M:%S%.3f").to_string());
                }

                if let Some(search) = &id_search
                    && let Some(uuid) = search.uuid()
                {
                    db_query = db_query
                        .bind(uuid) // event_id
                        .bind(uuid) // stream_id
                        .bind(search.text_pattern()); // event_type
                } else if let Some(search) = &id_search
                    && let Some(suffix_pattern) = search.suffix_pattern()
                {
                    db_query = db_query
                        .bind(suffix_pattern) // event_id suffix
                        .bind(suffix_pattern) // stream_id suffix
                        .bind(search.text_pattern()); // event_type
                } else if let Some(search) = &id_search {
                    db_query = db_query
                        .bind(search.text_pattern()) // event_id
                        .bind(search.text_pattern()) // stream_id
                        .bind(search.text_pattern()); // event_type
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
        let inserted = Self::insert_row(
            db,
            event_id,
            env_id,
            stream_id,
            event_type,
            event_data,
            event_time,
            created_by_user_id,
            access_id,
            None,
            None,
        )
        .await?;
        if inserted {
            Ok(())
        } else {
            Err(EventError::AlreadyExists(*event_id))
        }
    }

    /// Insert an event its publisher named, unless the id is taken, and report
    /// whether it was inserted. A resend of the publish finds the row, so the
    /// row starts with its enqueue leased to this publisher, who then settles
    /// it with [`Event::finish_enqueue`] or [`Event::release_enqueue`].
    #[allow(clippy::too_many_arguments)]
    pub async fn insert_event_if_absent(
        db: &crate::db::DatabasePool,
        event_id: &Uuid,
        env_id: &Uuid,
        stream_id: &Uuid,
        event_type: &str,
        event_data: &JsonValue,
        event_time: DateTime<Utc>,
        created_by_user_id: &Uuid,
        access_id: Option<&Uuid>,
        enqueue_leased_until: DateTime<Utc>,
    ) -> Result<bool, EventError> {
        Self::insert_row(
            db,
            event_id,
            env_id,
            stream_id,
            event_type,
            event_data,
            event_time,
            created_by_user_id,
            access_id,
            Some(enqueue_leased_until),
            None,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn insert_event_with_outbox(
        db: &crate::db::DatabasePool,
        event_id: &Uuid,
        env_id: &Uuid,
        stream_id: &Uuid,
        event_type: &str,
        event_data: &JsonValue,
        event_time: DateTime<Utc>,
        created_by_user_id: &Uuid,
        access_id: Option<&Uuid>,
        payload: &JsonValue,
    ) -> Result<bool, EventError> {
        crate::val::check_json_shape(payload, "event outbox")
            .map_err(|error| EventError::Database(sqlx::Error::Protocol(error)))?;
        Self::insert_row(
            db,
            event_id,
            env_id,
            stream_id,
            event_type,
            event_data,
            event_time,
            created_by_user_id,
            access_id,
            Some(Utc::now() + chrono::Duration::minutes(2)),
            Some(payload),
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn insert_row(
        db: &crate::db::DatabasePool,
        event_id: &Uuid,
        env_id: &Uuid,
        stream_id: &Uuid,
        event_type: &str,
        event_data: &JsonValue,
        event_time: DateTime<Utc>,
        created_by_user_id: &Uuid,
        access_id: Option<&Uuid>,
        enqueue_leased_until: Option<DateTime<Utc>>,
        enqueue_payload: Option<&JsonValue>,
    ) -> Result<bool, EventError> {
        // Ensure the stream exists (create if it doesn't)
        crate::db::stream::Stream::create_or_get_stream(db, *stream_id, *env_id)
            .await
            .map_err(|e| {
                EventError::Database(sqlx::Error::Protocol(format!(
                    "Failed to create stream: {}",
                    e
                )))
            })?;

        // The row and its stream counters commit together. A caller told the
        // insert failed must be able to rely on the row, and with it the outbox
        // lease that would still deliver the event, not being there.
        let inserted = match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let mut tx = pg_pool.begin().await?;
                // Keep PostgreSQL's database-clock DEFAULT now() as the
                // authoritative durable-creation timestamp.
                let inserted = sqlx::query("INSERT INTO event (event_id, env_id, stream_id, event_type, event_data, event_time, created_by_user_id, access_id, enqueue_leased_until, enqueue_payload) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) ON CONFLICT (event_id) DO NOTHING")
                    .bind(event_id)
                    .bind(env_id)
                    .bind(stream_id)
                    .bind(event_type)
                    .bind(event_data)
                    .bind(event_time)
                    .bind(created_by_user_id)
                    .bind(access_id)
                    .bind(enqueue_leased_until)
                    .bind(enqueue_payload)
                    .execute(&mut *tx)
                    .await?
                    .rows_affected();
                if inserted > 0 {
                    // A successful insert only changes the event count and activity time.
                    sqlx::query("UPDATE stream SET total_events = total_events + 1, last_activity_at = now() WHERE stream_id = $1")
                        .bind(stream_id)
                        .execute(&mut *tx)
                        .await?;
                }
                tx.commit().await?;
                inserted
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                // Bind this explicitly so SQLite retains sub-second precision
                // instead of using its seconds-only CURRENT_TIMESTAMP default.
                let created_at = Utc::now();
                let event_data_str = serde_json::to_string(event_data).map_err(|e| {
                    EventError::Database(sqlx::Error::Protocol(format!(
                        "JSON serialization error: {}",
                        e
                    )))
                })?;
                let enqueue_payload = enqueue_payload
                    .map(serde_json::to_string)
                    .transpose()
                    .map_err(|error| {
                        EventError::Database(sqlx::Error::Protocol(error.to_string()))
                    })?;

                let mut tx = sqlite_pool.begin().await?;
                let inserted = sqlx::query(
                    "INSERT INTO event (event_id, env_id, stream_id, event_type, event_data, event_time, created_at, created_by_user_id, access_id, enqueue_leased_until, enqueue_payload) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) ON CONFLICT (event_id) DO NOTHING",
                )
                    .bind(event_id)
                    .bind(env_id)
                    .bind(stream_id)
                    .bind(event_type)
                    .bind(event_data_str)
                    .bind(event_time)
                    .bind(created_at)
                    .bind(created_by_user_id)
                    .bind(access_id)
                    .bind(enqueue_leased_until)
                    .bind(enqueue_payload)
                    .execute(&mut *tx)
                    .await?
                    .rows_affected();
                if inserted > 0 {
                    // A successful insert only changes the event count and activity time.
                    sqlx::query("UPDATE stream SET total_events = total_events + 1, last_activity_at = datetime('now') WHERE stream_id = ?")
                        .bind(stream_id)
                        .execute(&mut *tx)
                        .await?;
                }
                tx.commit().await?;
                inserted
            }
        };

        Ok(inserted > 0)
    }

    pub async fn enqueue_payload(
        db: &crate::db::DatabasePool,
        event_id: &Uuid,
    ) -> Result<Option<JsonValue>, EventError> {
        let payload: Option<sqlx::types::Json<JsonValue>> = match db {
            crate::db::DatabasePool::Postgres(pool) => {
                sqlx::query_scalar("SELECT enqueue_payload FROM event WHERE event_id=$1")
                    .bind(event_id)
                    .fetch_one(pool)
                    .await?
            }
            crate::db::DatabasePool::Sqlite(pool) => {
                sqlx::query_scalar("SELECT enqueue_payload FROM event WHERE event_id=?")
                    .bind(event_id)
                    .fetch_one(pool)
                    .await?
            }
        };
        Ok(payload.map(|payload| payload.0))
    }

    pub async fn due_enqueues(
        db: &crate::db::DatabasePool,
        now: DateTime<Utc>,
        limit: i64,
        env_id: Option<Uuid>,
    ) -> Result<Vec<(Uuid, Uuid)>, EventError> {
        let query = "SELECT ev.event_id, ev.env_id FROM event ev JOIN env e ON e.env_id=ev.env_id JOIN org o ON o.org_id=e.org_id JOIN org root ON root.org_id=CASE WHEN o.parent_org_id IS NULL THEN o.org_id ELSE o.parent_org_id END WHERE ev.enqueue_payload IS NOT NULL AND ev.enqueue_leased_until <= $1 AND e.active AND o.active AND root.active AND o.lifecycle_state='active' AND root.lifecycle_state='active'";
        let rows = match db {
            crate::db::DatabasePool::Postgres(pool) => {
                let sql = format!(
                    "{query} AND ($3::uuid IS NULL OR ev.env_id=$3) ORDER BY ev.enqueue_leased_until, ev.event_id LIMIT $2"
                );
                sqlx::query_as(sqlx::AssertSqlSafe(sql.as_str()))
                    .bind(now)
                    .bind(limit)
                    .bind(env_id)
                    .fetch_all(pool)
                    .await?
            }
            crate::db::DatabasePool::Sqlite(pool) => {
                let sql = format!(
                    "{query} AND ($3 IS NULL OR ev.env_id=$3) ORDER BY ev.enqueue_leased_until, ev.event_id LIMIT $2"
                );
                sqlx::query_as(sqlx::AssertSqlSafe(sql.as_str()))
                    .bind(now)
                    .bind(limit)
                    .bind(env_id)
                    .fetch_all(pool)
                    .await?
            }
        };
        Ok(rows)
    }

    /// Take over an event's enqueue if its lease has ended, until
    /// `leased_until`. One caller gets it: others find it held.
    pub async fn claim_enqueue(
        db: &crate::db::DatabasePool,
        event_id: &Uuid,
        now: DateTime<Utc>,
        leased_until: DateTime<Utc>,
    ) -> Result<EnqueueClaim, EventError> {
        let claim = "UPDATE event SET enqueue_leased_until = $1 WHERE event_id = $2 AND enqueue_leased_until <= $3";
        let read = "SELECT enqueue_leased_until FROM event WHERE event_id = $1";
        let (claimed, lease): (u64, Option<DateTime<Utc>>) = match db {
            crate::db::DatabasePool::Postgres(pool) => {
                let claimed = sqlx::query(claim)
                    .bind(leased_until)
                    .bind(event_id)
                    .bind(now)
                    .execute(pool)
                    .await?
                    .rows_affected();
                let lease = sqlx::query_scalar(read)
                    .bind(event_id)
                    .fetch_one(pool)
                    .await?;
                (claimed, lease)
            }
            crate::db::DatabasePool::Sqlite(pool) => {
                let claimed = sqlx::query(claim)
                    .bind(leased_until)
                    .bind(event_id)
                    .bind(now)
                    .execute(pool)
                    .await?
                    .rows_affected();
                let lease = sqlx::query_scalar(read)
                    .bind(event_id)
                    .fetch_one(pool)
                    .await?;
                (claimed, lease)
            }
        };
        Ok(match (claimed, lease) {
            (0, None) => EnqueueClaim::Settled,
            (0, Some(_)) => EnqueueClaim::Held,
            _ => EnqueueClaim::Claimed,
        })
    }

    /// The event is enqueued: its publishers owe nothing more.
    pub async fn finish_enqueue(
        db: &crate::db::DatabasePool,
        event_id: &Uuid,
    ) -> Result<(), EventError> {
        let sql = "UPDATE event SET enqueue_leased_until = NULL, enqueue_payload = NULL WHERE event_id = $1";
        match db {
            crate::db::DatabasePool::Postgres(pool) => {
                sqlx::query(sql).bind(event_id).execute(pool).await?;
            }
            crate::db::DatabasePool::Sqlite(pool) => {
                sqlx::query(sql).bind(event_id).execute(pool).await?;
            }
        }
        Ok(())
    }

    /// End a lease whose enqueue failed, so a resend need not wait it out.
    pub async fn release_enqueue(
        db: &crate::db::DatabasePool,
        event_id: &Uuid,
        now: DateTime<Utc>,
    ) -> Result<(), EventError> {
        let sql = "UPDATE event SET enqueue_leased_until = $1 WHERE event_id = $2 AND enqueue_leased_until IS NOT NULL";
        match db {
            crate::db::DatabasePool::Postgres(pool) => {
                sqlx::query(sql)
                    .bind(now)
                    .bind(event_id)
                    .execute(pool)
                    .await?;
            }
            crate::db::DatabasePool::Sqlite(pool) => {
                sqlx::query(sql)
                    .bind(now)
                    .bind(event_id)
                    .execute(pool)
                    .await?;
            }
        }
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
        env_id: &Uuid,
        limit: Option<i64>,
        offset: Option<i64>,
    ) -> Result<Vec<Event>, EventError> {
        let limit = limit.unwrap_or(50);
        let offset = offset.unwrap_or(0);

        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let events = sqlx::query_as::<_, Event>(
                    "SELECT event_id, env_id, stream_id, event_type, event_data, event_time, created_at, created_by_user_id, handled FROM event WHERE stream_id = $1 AND env_id = $2 ORDER BY event_time DESC LIMIT $3 OFFSET $4"
                )
                .bind(stream_id)
                .bind(env_id)
                .bind(limit)
                .bind(offset)
                .fetch_all(pg_pool)
                .await?;
                Ok(events)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let events = sqlx::query_as::<_, Event>(
                    "SELECT event_id, env_id, stream_id, event_type, event_data, event_time, created_at, created_by_user_id, handled FROM event WHERE stream_id = ? AND env_id = ? ORDER BY event_time DESC LIMIT ? OFFSET ?"
                )
                .bind(stream_id)
                .bind(env_id)
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
    use super::{EnqueueClaim, Event, EventError};
    use crate::db::create_db_pool;
    use crate::val;
    use chrono::{Duration, Utc};
    use uuid::Uuid;

    #[tokio::test]
    async fn an_enqueue_lease_goes_to_one_publisher_at_a_time() {
        let db = crate::db::test_db().await;
        let data = crate::db::insert_test_data(&db).await.unwrap();
        let event_id = Uuid::new_v4();
        let now = Utc::now();
        let lease = now + Duration::minutes(2);
        let insert = |leased_until| {
            let db = &db;
            let data = &data;
            async move {
                Event::insert_event_if_absent(
                    db,
                    &event_id,
                    &data.env_id,
                    &data.stream_id,
                    "order:created",
                    &serde_json::json!({}),
                    now,
                    &data.user_id,
                    None,
                    leased_until,
                )
                .await
                .unwrap()
            }
        };
        let claim = |at| Event::claim_enqueue(&db, &event_id, at, lease);

        assert!(insert(lease).await);
        assert!(!insert(lease).await, "the id is taken");

        assert_eq!(claim(now).await.unwrap(), EnqueueClaim::Held);
        let after_the_lease = lease + Duration::seconds(1);
        assert_eq!(claim(after_the_lease).await.unwrap(), EnqueueClaim::Claimed);
        assert_eq!(claim(now).await.unwrap(), EnqueueClaim::Held);

        Event::release_enqueue(&db, &event_id, now).await.unwrap();
        assert_eq!(claim(now).await.unwrap(), EnqueueClaim::Claimed);

        Event::finish_enqueue(&db, &event_id).await.unwrap();
        assert_eq!(claim(now).await.unwrap(), EnqueueClaim::Settled);

        // A settled event stays settled.
        Event::release_enqueue(&db, &event_id, now).await.unwrap();
        assert_eq!(claim(now).await.unwrap(), EnqueueClaim::Settled);
    }

    #[tokio::test]
    async fn an_event_inserted_without_a_lease_owes_nothing() {
        let db = crate::db::test_db().await;
        let data = crate::db::insert_test_data(&db).await.unwrap();
        let event_id = Uuid::now_v7();
        let insert = async || {
            Event::insert_event(
                &db,
                &event_id,
                &data.env_id,
                &data.stream_id,
                "order:created",
                &serde_json::json!({}),
                Utc::now(),
                &data.user_id,
                None,
            )
            .await
        };

        insert().await.unwrap();

        assert!(matches!(
            insert().await,
            Err(EventError::AlreadyExists(id)) if id == event_id
        ));
        let now = Utc::now();
        assert_eq!(
            Event::claim_enqueue(&db, &event_id, now, now)
                .await
                .unwrap(),
            EnqueueClaim::Settled
        );
    }

    #[tokio::test]
    async fn a_failed_stream_counter_update_leaves_no_event_row() {
        let db = crate::db::test_db().await;
        let data = crate::db::insert_test_data(&db).await.unwrap();
        let crate::db::DatabasePool::Sqlite(pool) = &db else {
            unreachable!()
        };
        let event_id = Uuid::now_v7();
        let insert = async || {
            Event::insert_event_with_outbox(
                &db,
                &event_id,
                &data.env_id,
                &data.stream_id,
                "order:created",
                &serde_json::json!({}),
                Utc::now(),
                &data.user_id,
                None,
                &serde_json::json!({"queue": "hot:event"}),
            )
            .await
        };
        let total_events = async || {
            sqlx::query_scalar::<_, i64>("SELECT total_events FROM stream WHERE stream_id=?")
                .bind(data.stream_id)
                .fetch_one(pool)
                .await
                .unwrap()
        };
        let before = total_events().await;

        sqlx::query(
            "CREATE TRIGGER stream_counter_fails BEFORE UPDATE OF total_events ON stream \
             BEGIN SELECT RAISE(ABORT, 'stream counter unavailable'); END",
        )
        .execute(pool)
        .await
        .unwrap();
        assert!(insert().await.is_err());
        // The send failed, so no row and no outbox lease may be left to
        // deliver the event behind the caller's back.
        assert!(matches!(
            Event::get_event(&db, &event_id).await,
            Err(EventError::NotFound)
        ));
        assert!(
            Event::due_enqueues(&db, Utc::now() + Duration::hours(1), 10, None)
                .await
                .unwrap()
                .is_empty()
        );

        sqlx::query("DROP TRIGGER stream_counter_fails")
            .execute(pool)
            .await
            .unwrap();
        assert!(insert().await.unwrap());
        assert_eq!(total_events().await, before + 1);
    }

    #[tokio::test]
    async fn sqlite_time_range_cutoff_splits_events_of_the_same_day() {
        let db = crate::db::test_db().await;
        let data = crate::db::insert_test_data(&db).await.unwrap();
        let crate::db::DatabasePool::Sqlite(pool) = &db else {
            unreachable!()
        };
        // One row as `insert_event` writes it, and one as the column default does.
        let bound = Uuid::now_v7();
        Event::insert_event(
            &db,
            &bound,
            &data.env_id,
            &data.stream_id,
            "order:created",
            &serde_json::json!({}),
            Utc::now(),
            &data.user_id,
            None,
        )
        .await
        .unwrap();
        let defaulted = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO event (event_id, env_id, stream_id, event_type, event_data, event_time, created_by_user_id) VALUES (?, ?, ?, 'order:created', '{}', ?, ?)",
        )
        .bind(defaulted)
        .bind(data.env_id)
        .bind(data.stream_id)
        .bind(Utc::now())
        .bind(data.user_id)
        .execute(pool)
        .await
        .unwrap();

        let since = async |cutoff| {
            let listed = Event::get_events_by_env_filtered(
                &db,
                &data.env_id,
                None,
                cutoff,
                None,
                None,
                None,
            )
            .await
            .unwrap()
            .len() as i64;
            let counted = Event::get_filtered_count_by_env(&db, &data.env_id, None, cutoff, None)
                .await
                .unwrap();
            assert_eq!(listed, counted);
            counted
        };
        let now = Utc::now();
        assert_eq!(since(Some(now - Duration::minutes(5))).await, 2);
        // Both rows are from today, and both are older than a cutoff still ahead.
        assert_eq!(since(Some(now + Duration::minutes(5))).await, 0);
    }

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

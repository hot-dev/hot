use ahash::AHashMap;
use serde_json::Value as JsonValue;
use sqlx::{FromRow, Pool, Postgres, Sqlite};
use std::sync::{Arc, LazyLock, Mutex};
use thiserror::Error;
use uuid::Uuid;

/// Type alias for the event handler cache
type EventHandlerCache = Arc<Mutex<AHashMap<(Uuid, String), CachedEventHandlers>>>;

/// Global cache for event handlers by (env_id, event_type).
/// Invalidated when builds are deployed or projects are toggled.
static EVENT_HANDLER_CACHE: LazyLock<EventHandlerCache> =
    LazyLock::new(|| Arc::new(Mutex::new(AHashMap::new())));

/// Cached event handlers with timestamp for optional TTL
#[derive(Clone)]
struct CachedEventHandlers {
    handlers: Vec<EventHandler>,
    runtime_revision: i64,
    cached_at: std::time::Instant,
}

/// Maximum cache entries (one per env_id + event_type combination)
const MAX_EVENT_HANDLER_CACHE_ENTRIES: usize = 100;

#[derive(Error, Debug)]
pub enum EventHandlerError {
    #[error("Database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("Event handler not found")]
    NotFound,
    #[error("Serialization error: {0}")]
    SerializationError(String),
    #[error("Runtime revision error: {0}")]
    RuntimeRevision(String),
}

#[derive(Debug, Clone, FromRow)]
pub struct EventHandler {
    pub event_handler_id: Uuid,
    pub build_id: Uuid,
    pub event_type: String,
    pub ns: String,
    pub var: String,
    pub meta: Option<JsonValue>,
    pub value: Option<JsonValue>,
    pub file: Option<String>,
    pub line: Option<i32>,
    pub column: Option<i32>,
    pub position: Option<i32>,
}

/// EventHandler with project information for display purposes
#[derive(Debug, FromRow)]
pub struct EventHandlerWithProject {
    pub event_handler_id: Uuid,
    pub build_id: Uuid,
    pub event_type: String,
    pub ns: String,
    pub var: String,
    pub meta: Option<JsonValue>,
    pub value: Option<JsonValue>,
    pub file: Option<String>,
    pub line: Option<i32>,
    pub column: Option<i32>,
    pub position: Option<i32>,
    pub project_id: Uuid,
    pub project_name: String,
}

impl EventHandler {
    /// Get event handler by ID
    pub async fn get_event_handler(
        db: &crate::db::DatabasePool,
        event_handler_id: &Uuid,
    ) -> Result<EventHandler, EventHandlerError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                Self::get_event_handler_postgres(pg_pool, event_handler_id).await
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                Self::get_event_handler_sqlite(sqlite_pool, event_handler_id).await
            }
        }
    }

    async fn get_event_handler_sqlite(
        db: &Pool<Sqlite>,
        event_handler_id: &Uuid,
    ) -> Result<EventHandler, EventHandlerError> {
        let event_handler = sqlx::query_as::<_, EventHandler>(
            "SELECT event_handler_id, build_id, event_type, ns, var, meta, value, file, line, \"column\", position FROM event_handler WHERE event_handler_id = ?"
        )
        .bind(event_handler_id)
        .fetch_one(db)
        .await?;
        Ok(event_handler)
    }

    async fn get_event_handler_postgres(
        db: &Pool<Postgres>,
        event_handler_id: &Uuid,
    ) -> Result<EventHandler, EventHandlerError> {
        let event_handler = sqlx::query_as::<_, EventHandler>(
            "SELECT event_handler_id, build_id, event_type, ns, var, meta, value, file, line, \"column\", position FROM event_handler WHERE event_handler_id = $1"
        )
        .bind(event_handler_id)
        .fetch_one(db)
        .await?;
        Ok(event_handler)
    }

    /// Get event handlers by build ID
    pub async fn get_event_handlers_by_build(
        db: &crate::db::DatabasePool,
        build_id: &Uuid,
        limit: Option<i64>,
        offset: Option<i64>,
    ) -> Result<Vec<EventHandler>, EventHandlerError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                Self::get_event_handlers_by_build_postgres(pg_pool, build_id, limit, offset).await
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                Self::get_event_handlers_by_build_sqlite(sqlite_pool, build_id, limit, offset).await
            }
        }
    }

    async fn get_event_handlers_by_build_sqlite(
        db: &Pool<Sqlite>,
        build_id: &Uuid,
        limit: Option<i64>,
        offset: Option<i64>,
    ) -> Result<Vec<EventHandler>, EventHandlerError> {
        let limit = limit.unwrap_or(10);
        let offset = offset.unwrap_or(0);

        let event_handlers = sqlx::query_as::<_, EventHandler>(
            "SELECT event_handler_id, build_id, event_type, ns, var, meta, value, file, line, \"column\", position FROM event_handler WHERE build_id = ? ORDER BY event_type, ns, var LIMIT ? OFFSET ?"
        )
        .bind(build_id)
        .bind(limit)
        .bind(offset)
        .fetch_all(db)
        .await?;
        Ok(event_handlers)
    }

    async fn get_event_handlers_by_build_postgres(
        db: &Pool<Postgres>,
        build_id: &Uuid,
        limit: Option<i64>,
        offset: Option<i64>,
    ) -> Result<Vec<EventHandler>, EventHandlerError> {
        let limit = limit.unwrap_or(10);
        let offset = offset.unwrap_or(0);

        let event_handlers = sqlx::query_as::<_, EventHandler>(
            "SELECT event_handler_id, build_id, event_type, ns, var, meta, value, file, line, \"column\", position FROM event_handler WHERE build_id = $1 ORDER BY event_type, ns, var LIMIT $2 OFFSET $3"
        )
        .bind(build_id)
        .bind(limit)
        .bind(offset)
        .fetch_all(db)
        .await?;
        Ok(event_handlers)
    }

    /// Get event handlers by event type
    pub async fn get_event_handlers_by_event_type(
        db: &crate::db::DatabasePool,
        event_type: &str,
        limit: Option<i64>,
        offset: Option<i64>,
    ) -> Result<Vec<EventHandler>, EventHandlerError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                Self::get_event_handlers_by_event_type_postgres(pg_pool, event_type, limit, offset)
                    .await
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                Self::get_event_handlers_by_event_type_sqlite(
                    sqlite_pool,
                    event_type,
                    limit,
                    offset,
                )
                .await
            }
        }
    }

    async fn get_event_handlers_by_event_type_sqlite(
        db: &Pool<Sqlite>,
        event_type: &str,
        limit: Option<i64>,
        offset: Option<i64>,
    ) -> Result<Vec<EventHandler>, EventHandlerError> {
        let limit = limit.unwrap_or(10);
        let offset = offset.unwrap_or(0);

        let event_handlers = sqlx::query_as::<_, EventHandler>(
            "SELECT event_handler_id, build_id, event_type, ns, var, meta, value, file, line, \"column\", position FROM event_handler WHERE event_type = ? ORDER BY ns, var LIMIT ? OFFSET ?"
        )
        .bind(event_type)
        .bind(limit)
        .bind(offset)
        .fetch_all(db)
        .await?;
        Ok(event_handlers)
    }

    async fn get_event_handlers_by_event_type_postgres(
        db: &Pool<Postgres>,
        event_type: &str,
        limit: Option<i64>,
        offset: Option<i64>,
    ) -> Result<Vec<EventHandler>, EventHandlerError> {
        let limit = limit.unwrap_or(10);
        let offset = offset.unwrap_or(0);

        let event_handlers = sqlx::query_as::<_, EventHandler>(
            "SELECT event_handler_id, build_id, event_type, ns, var, meta, value, file, line, \"column\", position FROM event_handler WHERE event_type = $1 ORDER BY ns, var LIMIT $2 OFFSET $3"
        )
        .bind(event_type)
        .bind(limit)
        .bind(offset)
        .fetch_all(db)
        .await?;
        Ok(event_handlers)
    }

    /// Get event handlers by build ID and event type
    pub async fn get_event_handlers_by_build_and_event_type(
        db: &crate::db::DatabasePool,
        build_id: &Uuid,
        event_type: &str,
        limit: Option<i64>,
        offset: Option<i64>,
    ) -> Result<Vec<EventHandler>, EventHandlerError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                Self::get_event_handlers_by_build_and_event_type_postgres(
                    pg_pool, build_id, event_type, limit, offset,
                )
                .await
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                Self::get_event_handlers_by_build_and_event_type_sqlite(
                    sqlite_pool,
                    build_id,
                    event_type,
                    limit,
                    offset,
                )
                .await
            }
        }
    }

    async fn get_event_handlers_by_build_and_event_type_sqlite(
        db: &Pool<Sqlite>,
        build_id: &Uuid,
        event_type: &str,
        limit: Option<i64>,
        offset: Option<i64>,
    ) -> Result<Vec<EventHandler>, EventHandlerError> {
        let limit = limit.unwrap_or(10);
        let offset = offset.unwrap_or(0);

        let event_handlers = sqlx::query_as::<_, EventHandler>(
            "SELECT event_handler_id, build_id, event_type, ns, var, meta, value, file, line, \"column\", position FROM event_handler WHERE build_id = ? AND event_type = ? ORDER BY ns, var LIMIT ? OFFSET ?"
        )
        .bind(build_id)
        .bind(event_type)
        .bind(limit)
        .bind(offset)
        .fetch_all(db)
        .await?;
        Ok(event_handlers)
    }

    async fn get_event_handlers_by_build_and_event_type_postgres(
        db: &Pool<Postgres>,
        build_id: &Uuid,
        event_type: &str,
        limit: Option<i64>,
        offset: Option<i64>,
    ) -> Result<Vec<EventHandler>, EventHandlerError> {
        let limit = limit.unwrap_or(10);
        let offset = offset.unwrap_or(0);

        let event_handlers = sqlx::query_as::<_, EventHandler>(
            "SELECT event_handler_id, build_id, event_type, ns, var, meta, value, file, line, \"column\", position FROM event_handler WHERE build_id = $1 AND event_type = $2 ORDER BY ns, var LIMIT $3 OFFSET $4"
        )
        .bind(build_id)
        .bind(event_type)
        .bind(limit)
        .bind(offset)
        .fetch_all(db)
        .await?;
        Ok(event_handlers)
    }

    /// Get event handlers by environment ID and event type
    /// This queries across ALL deployed builds in the environment
    /// Used for multi-project event routing
    ///
    /// Results are cached in memory - call `invalidate_event_handler_cache` when
    /// builds are deployed or projects are toggled.
    pub async fn get_event_handlers_by_env_and_event_type(
        db: &crate::db::DatabasePool,
        env_id: &Uuid,
        event_type: &str,
    ) -> Result<Vec<EventHandler>, EventHandlerError> {
        let cache_key = (*env_id, event_type.to_string());
        let runtime_revision = crate::db::Env::get_runtime_revision(db, env_id)
            .await
            .map_err(|e| EventHandlerError::RuntimeRevision(e.to_string()))?;

        // Check cache first
        if let Ok(mut cache) = EVENT_HANDLER_CACHE.lock()
            && let Some(cached) = cache.get(&cache_key)
        {
            if cached.runtime_revision == runtime_revision {
                tracing::debug!(
                    "✓ Event handlers cache HIT for env={}, type={} revision={} ({} handlers)",
                    env_id,
                    event_type,
                    runtime_revision,
                    cached.handlers.len()
                );
                return Ok(cached.handlers.clone());
            }

            tracing::debug!(
                "Event handlers cache STALE for env={}, type={} cached_revision={} current_revision={}",
                env_id,
                event_type,
                cached.runtime_revision,
                runtime_revision
            );
            cache.remove(&cache_key);
        }

        tracing::debug!(
            "Event handlers cache MISS for env={}, type={} - querying DB",
            env_id,
            event_type
        );

        // Cache miss - query database
        let handlers = match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                Self::get_event_handlers_by_env_and_event_type_postgres(pg_pool, env_id, event_type)
                    .await?
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                Self::get_event_handlers_by_env_and_event_type_sqlite(
                    sqlite_pool,
                    env_id,
                    event_type,
                )
                .await?
            }
        };

        // Store in cache
        if let Ok(mut cache) = EVENT_HANDLER_CACHE.lock() {
            // Simple LRU: evict oldest if at capacity
            if cache.len() >= MAX_EVENT_HANDLER_CACHE_ENTRIES
                && !cache.contains_key(&cache_key)
                && let Some((oldest_key, _)) = cache
                    .iter()
                    .min_by_key(|(_, v)| v.cached_at)
                    .map(|(k, v)| (k.clone(), v.cached_at))
            {
                cache.remove(&oldest_key);
            }

            cache.insert(
                cache_key,
                CachedEventHandlers {
                    handlers: handlers.clone(),
                    runtime_revision,
                    cached_at: std::time::Instant::now(),
                },
            );
            tracing::debug!(
                "✓ Cached {} event handlers for env={}, type={} revision={}",
                handlers.len(),
                env_id,
                event_type,
                runtime_revision
            );
        }

        Ok(handlers)
    }

    /// Invalidate all cached event handlers for an environment.
    /// Call this when builds are deployed or projects are toggled.
    pub fn invalidate_event_handler_cache_for_env(env_id: &Uuid) {
        if let Ok(mut cache) = EVENT_HANDLER_CACHE.lock() {
            let keys_to_remove: Vec<_> = cache
                .keys()
                .filter(|(eid, _)| eid == env_id)
                .cloned()
                .collect();

            let count = keys_to_remove.len();
            for key in keys_to_remove {
                cache.remove(&key);
            }

            if count > 0 {
                tracing::info!(
                    "Invalidated {} event handler cache entries for env={}",
                    count,
                    env_id
                );
            }
        }
    }

    /// Invalidate all cached event handlers (e.g., on hot reload).
    pub fn invalidate_all_event_handler_cache() {
        if let Ok(mut cache) = EVENT_HANDLER_CACHE.lock() {
            let count = cache.len();
            cache.clear();
            if count > 0 {
                tracing::info!("Invalidated all {} event handler cache entries", count);
            }
        }
    }

    async fn get_event_handlers_by_env_and_event_type_sqlite(
        db: &Pool<Sqlite>,
        env_id: &Uuid,
        event_type: &str,
    ) -> Result<Vec<EventHandler>, EventHandlerError> {
        // Join through project -> build to find all deployed builds in the environment
        // and get their event handlers for the given event type
        // Only include active projects - deactivated projects should not have their handlers run
        let event_handlers = sqlx::query_as::<_, EventHandler>(
            r#"
            SELECT eh.event_handler_id, eh.build_id, eh.event_type, eh.ns, eh.var,
                   eh.meta, eh.value, eh.file, eh.line, eh."column", eh.position
            FROM event_handler eh
            INNER JOIN build b ON eh.build_id = b.build_id
            INNER JOIN project p ON b.project_id = p.project_id
            WHERE p.env_id = ?
              AND p.active = 1
              AND b.deployed = 1
              AND b.runtime_status = 'ready'
              AND eh.event_type = ?
            ORDER BY eh.ns, eh.var
            "#,
        )
        .bind(env_id)
        .bind(event_type)
        .fetch_all(db)
        .await?;
        Ok(event_handlers)
    }

    async fn get_event_handlers_by_env_and_event_type_postgres(
        db: &Pool<Postgres>,
        env_id: &Uuid,
        event_type: &str,
    ) -> Result<Vec<EventHandler>, EventHandlerError> {
        // Join through project -> build to find all deployed builds in the environment
        // and get their event handlers for the given event type
        // Only include active projects - deactivated projects should not have their handlers run
        let event_handlers = sqlx::query_as::<_, EventHandler>(
            r#"
            SELECT eh.event_handler_id, eh.build_id, eh.event_type, eh.ns, eh.var,
                   eh.meta, eh.value, eh.file, eh.line, eh."column", eh.position
            FROM event_handler eh
            INNER JOIN build b ON eh.build_id = b.build_id
            INNER JOIN project p ON b.project_id = p.project_id
            WHERE p.env_id = $1
              AND p.active = true
              AND b.deployed = true
              AND b.runtime_status = 'ready'
              AND eh.event_type = $2
            ORDER BY eh.ns, eh.var
            "#,
        )
        .bind(env_id)
        .bind(event_type)
        .fetch_all(db)
        .await?;
        Ok(event_handlers)
    }

    /// Get count of event handlers
    pub async fn get_count(db: &crate::db::DatabasePool) -> Result<i64, EventHandlerError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let count = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM event_handler")
                    .fetch_one(pg_pool)
                    .await?;
                Ok(count)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let count = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM event_handler")
                    .fetch_one(sqlite_pool)
                    .await?;
                Ok(count)
            }
        }
    }

    /// Get count of event handlers by build ID
    pub async fn get_count_by_build(
        db: &crate::db::DatabasePool,
        build_id: &Uuid,
    ) -> Result<i64, EventHandlerError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let count = sqlx::query_scalar::<_, i64>(
                    "SELECT COUNT(*) FROM event_handler WHERE build_id = $1",
                )
                .bind(build_id)
                .fetch_one(pg_pool)
                .await?;
                Ok(count)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let count = sqlx::query_scalar::<_, i64>(
                    "SELECT COUNT(*) FROM event_handler WHERE build_id = ?",
                )
                .bind(build_id)
                .fetch_one(sqlite_pool)
                .await?;
                Ok(count)
            }
        }
    }

    /// Get count of event handlers by event type
    pub async fn get_count_by_event_type(
        db: &crate::db::DatabasePool,
        event_type: &str,
    ) -> Result<i64, EventHandlerError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let count = sqlx::query_scalar::<_, i64>(
                    "SELECT COUNT(*) FROM event_handler WHERE event_type = $1",
                )
                .bind(event_type)
                .fetch_one(pg_pool)
                .await?;
                Ok(count)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let count = sqlx::query_scalar::<_, i64>(
                    "SELECT COUNT(*) FROM event_handler WHERE event_type = ?",
                )
                .bind(event_type)
                .fetch_one(sqlite_pool)
                .await?;
                Ok(count)
            }
        }
    }

    /// Insert a new event handler
    #[allow(clippy::too_many_arguments)]
    pub async fn insert_event_handler(
        db: &crate::db::DatabasePool,
        event_handler_id: &Uuid,
        build_id: &Uuid,
        event_type: &str,
        ns: &str,
        var: &str,
        meta: Option<&JsonValue>,
        value: Option<&JsonValue>,
        file: Option<&str>,
        line: Option<i32>,
        column: Option<i32>,
        position: Option<i32>,
    ) -> Result<(), EventHandlerError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                Self::insert_event_handler_postgres(
                    pg_pool,
                    event_handler_id,
                    build_id,
                    event_type,
                    ns,
                    var,
                    meta,
                    value,
                    file,
                    line,
                    column,
                    position,
                )
                .await
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                Self::insert_event_handler_sqlite(
                    sqlite_pool,
                    event_handler_id,
                    build_id,
                    event_type,
                    ns,
                    var,
                    meta,
                    value,
                    file,
                    line,
                    column,
                    position,
                )
                .await
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn insert_event_handler_sqlite(
        db: &Pool<Sqlite>,
        event_handler_id: &Uuid,
        build_id: &Uuid,
        event_type: &str,
        ns: &str,
        var: &str,
        meta: Option<&JsonValue>,
        value: Option<&JsonValue>,
        file: Option<&str>,
        line: Option<i32>,
        column: Option<i32>,
        position: Option<i32>,
    ) -> Result<(), EventHandlerError> {
        let meta_json = meta
            .map(serde_json::to_string)
            .transpose()
            .map_err(|e| EventHandlerError::SerializationError(e.to_string()))?;
        let value_json = value
            .map(serde_json::to_string)
            .transpose()
            .map_err(|e| EventHandlerError::SerializationError(e.to_string()))?;

        // Use INSERT OR IGNORE to handle race conditions where multiple workers
        // might try to insert the same handler simultaneously
        sqlx::query(
            "INSERT INTO event_handler (event_handler_id, build_id, event_type, ns, var, meta, value, file, line, \"column\", position)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT (build_id, ns, var, event_type) DO UPDATE SET
                meta = excluded.meta,
                value = excluded.value,
                file = excluded.file,
                line = excluded.line,
                \"column\" = excluded.\"column\",
                position = excluded.position"
        )
        .bind(event_handler_id)
        .bind(build_id)
        .bind(event_type)
        .bind(ns)
        .bind(var)
        .bind(meta_json)
        .bind(value_json)
        .bind(file)
        .bind(line)
        .bind(column)
        .bind(position)
        .execute(db)
        .await?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn insert_event_handler_postgres(
        db: &Pool<Postgres>,
        event_handler_id: &Uuid,
        build_id: &Uuid,
        event_type: &str,
        ns: &str,
        var: &str,
        meta: Option<&JsonValue>,
        value: Option<&JsonValue>,
        file: Option<&str>,
        line: Option<i32>,
        column: Option<i32>,
        position: Option<i32>,
    ) -> Result<(), EventHandlerError> {
        // Use ON CONFLICT to handle race conditions where multiple workers
        // might try to insert the same handler simultaneously
        sqlx::query(
            "INSERT INTO event_handler (event_handler_id, build_id, event_type, ns, var, meta, value, file, line, \"column\", position)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
             ON CONFLICT (build_id, ns, var, event_type) DO UPDATE SET
                meta = EXCLUDED.meta,
                value = EXCLUDED.value,
                file = EXCLUDED.file,
                line = EXCLUDED.line,
                \"column\" = EXCLUDED.\"column\",
                position = EXCLUDED.position"
        )
        .bind(event_handler_id)
        .bind(build_id)
        .bind(event_type)
        .bind(ns)
        .bind(var)
        .bind(meta)
        .bind(value)
        .bind(file)
        .bind(line)
        .bind(column)
        .bind(position)
        .execute(db)
        .await?;
        Ok(())
    }

    /// Insert a single event handler from a Val map, resolving meta and merging
    /// send targets.  Used by both the local batch path and the remote manifest path.
    pub async fn insert_event_handler_from_val(
        db: &crate::db::DatabasePool,
        build_id: &Uuid,
        event_type: &str,
        handler_val: &crate::val::Val,
        send_targets: &crate::lang::compiler::SendTargets,
    ) -> Result<(), EventHandlerError> {
        use crate::val::Val;

        let handler_map = match handler_val {
            Val::Map(map) => map,
            _ => {
                return Err(EventHandlerError::SerializationError(
                    "Event handler is not a map".to_string(),
                ));
            }
        };

        let fn_name = handler_map
            .get(&Val::from("fn"))
            .and_then(|v| match v {
                Val::Str(s) => Some((**s).to_owned()),
                _ => None,
            })
            .unwrap_or_default();

        let (ns, var) = fn_name
            .rsplit_once('/')
            .map(|(ns, var)| (ns.to_string(), var.to_string()))
            .unwrap_or_default();

        let meta = handler_map
            .get(&Val::from("meta"))
            .map(crate::db::resolve_meta_val)
            .and_then(|v| serde_json::to_value(&v).ok());

        let file = handler_map.get(&Val::from("file")).and_then(|v| match v {
            Val::Str(s) => Some((**s).to_owned()),
            Val::Null => None,
            _ => None,
        });

        let line = handler_map.get(&Val::from("line")).and_then(|v| match v {
            Val::Int(i) => Some(*i as i32),
            Val::Null => None,
            _ => None,
        });

        let column = handler_map.get(&Val::from("column")).and_then(|v| match v {
            Val::Int(i) => Some(*i as i32),
            Val::Null => None,
            _ => None,
        });

        let position = handler_map
            .get(&Val::from("position"))
            .and_then(|v| match v {
                Val::Int(i) => Some(*i as i32),
                Val::Null => None,
                _ => None,
            });

        let fn_key = format!("{}/{}", ns, var);
        let static_sends: Vec<String> = send_targets
            .get(&fn_key)
            .map(|targets| targets.iter().map(|t| t.event_name.clone()).collect())
            .unwrap_or_default();
        let merged_meta = crate::db::merge_sends_into_meta(meta, &static_sends);

        let event_handler_id = Uuid::now_v7();
        Self::insert_event_handler(
            db,
            &event_handler_id,
            build_id,
            event_type,
            &ns,
            &var,
            merged_meta.as_ref(),
            None, // value field unused
            file.as_deref(),
            line,
            column,
            position,
        )
        .await
    }

    /// Insert multiple event handlers for a build, merging statically-detected
    /// send targets into each handler's meta.sends.
    pub async fn insert_event_handlers_for_build(
        db: &crate::db::DatabasePool,
        build_id: &Uuid,
        event_handlers: &crate::lang::compiler::EventHandlers,
        send_targets: &crate::lang::compiler::SendTargets,
    ) -> Result<(), EventHandlerError> {
        for (event_type, handlers) in event_handlers {
            for handler in handlers {
                Self::insert_event_handler_from_val(
                    db,
                    build_id,
                    event_type,
                    &handler.event_handler,
                    send_targets,
                )
                .await?;
            }
        }
        Ok(())
    }

    /// Delete event handlers by build ID
    pub async fn delete_event_handlers_by_build(
        db: &crate::db::DatabasePool,
        build_id: &Uuid,
    ) -> Result<u64, EventHandlerError> {
        // Invalidate all event handler caches since handlers are being modified
        // (We don't have env_id here, so invalidate all to be safe)
        Self::invalidate_all_event_handler_cache();

        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let result = sqlx::query("DELETE FROM event_handler WHERE build_id = $1")
                    .bind(build_id)
                    .execute(pg_pool)
                    .await?;
                if result.rows_affected() > 0 {
                    tracing::info!(
                        "Deleted {} event handler(s) for build {}",
                        result.rows_affected(),
                        build_id
                    );
                }
                Ok(result.rows_affected())
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let result = sqlx::query("DELETE FROM event_handler WHERE build_id = ?")
                    .bind(build_id)
                    .execute(sqlite_pool)
                    .await?;
                if result.rows_affected() > 0 {
                    tracing::info!(
                        "Deleted {} event handler(s) for build {}",
                        result.rows_affected(),
                        build_id
                    );
                }
                Ok(result.rows_affected())
            }
        }
    }

    /// Delete a specific event handler
    pub async fn delete_event_handler(
        db: &crate::db::DatabasePool,
        event_handler_id: &Uuid,
    ) -> Result<(), EventHandlerError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                sqlx::query("DELETE FROM event_handler WHERE event_handler_id = $1")
                    .bind(event_handler_id)
                    .execute(pg_pool)
                    .await?;
                Ok(())
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                sqlx::query("DELETE FROM event_handler WHERE event_handler_id = ?")
                    .bind(event_handler_id)
                    .execute(sqlite_pool)
                    .await?;
                Ok(())
            }
        }
    }

    /// Get event handlers for deployed builds in a specific environment
    pub async fn get_event_handlers_by_env_deployed(
        db: &crate::db::DatabasePool,
        env_id: &Uuid,
        limit: Option<i64>,
        offset: Option<i64>,
    ) -> Result<Vec<EventHandlerWithProject>, EventHandlerError> {
        let limit = limit.unwrap_or(100);
        let offset = offset.unwrap_or(0);

        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let handlers = sqlx::query_as::<_, EventHandlerWithProject>(
                    "SELECT e.event_handler_id, e.build_id, e.event_type, e.ns, e.var, e.meta, e.value, e.file, e.line, e.\"column\", e.position, p.project_id, p.name as project_name
                     FROM event_handler e
                     JOIN build b ON e.build_id = b.build_id
                     JOIN project p ON b.project_id = p.project_id
                     WHERE b.deployed = true AND b.runtime_status = 'ready' AND b.active = true AND p.env_id = $1
                     ORDER BY p.name, e.event_type, e.ns, e.var
                     LIMIT $2 OFFSET $3"
                )
                .bind(env_id)
                .bind(limit)
                .bind(offset)
                .fetch_all(pg_pool)
                .await?;
                Ok(handlers)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let handlers = sqlx::query_as::<_, EventHandlerWithProject>(
                    "SELECT e.event_handler_id, e.build_id, e.event_type, e.ns, e.var, e.meta, e.value, e.file, e.line, e.\"column\", e.position, p.project_id, p.name as project_name
                     FROM event_handler e
                     JOIN build b ON e.build_id = b.build_id
                     JOIN project p ON b.project_id = p.project_id
                     WHERE b.deployed = 1 AND b.runtime_status = 'ready' AND b.active = 1 AND p.env_id = ?
                     ORDER BY p.name, e.event_type, e.ns, e.var
                     LIMIT ? OFFSET ?"
                )
                .bind(env_id)
                .bind(limit)
                .bind(offset)
                .fetch_all(sqlite_pool)
                .await?;
                Ok(handlers)
            }
        }
    }

    /// Get count of event handlers for deployed builds in a specific environment
    pub async fn get_count_by_env_deployed(
        db: &crate::db::DatabasePool,
        env_id: &Uuid,
    ) -> Result<i64, EventHandlerError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let count = sqlx::query_scalar::<_, i64>(
                    "SELECT COUNT(*) FROM event_handler e
                     JOIN build b ON e.build_id = b.build_id
                     JOIN project p ON b.project_id = p.project_id
                     WHERE b.deployed = true AND b.runtime_status = 'ready' AND b.active = true AND p.env_id = $1",
                )
                .bind(env_id)
                .fetch_one(pg_pool)
                .await?;
                Ok(count)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let count = sqlx::query_scalar::<_, i64>(
                    "SELECT COUNT(*) FROM event_handler e
                     JOIN build b ON e.build_id = b.build_id
                     JOIN project p ON b.project_id = p.project_id
                     WHERE b.deployed = 1 AND b.runtime_status = 'ready' AND b.active = 1 AND p.env_id = ?",
                )
                .bind(env_id)
                .fetch_one(sqlite_pool)
                .await?;
                Ok(count)
            }
        }
    }
}

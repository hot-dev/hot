use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use tracing::{debug, info};
use uuid::Uuid;

use super::{DatabaseError, DatabasePool};

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct Call {
    pub call_id: Uuid,
    pub run_id: Uuid,
    pub function_name: String,
    pub static_scope: String,
    pub parent_call_id: Option<Uuid>,
    pub call_depth: i32,
    pub start_time: DateTime<Utc>,
    pub stop_time: Option<DateTime<Utc>>,
    pub duration_us: Option<i64>,
    pub runtime_path: Option<String>,
    pub args: Option<serde_json::Value>,
    pub return_value: Option<serde_json::Value>,
    pub flow: Option<serde_json::Value>,
    pub file: Option<String>,
    pub line: Option<i32>,
    pub column: Option<i32>,
    pub position: Option<i32>,
}

/// Call row without the potentially large `args`/`return_value` payloads.
/// Used to build call timelines/hierarchies cheaply; full payloads are
/// fetched per call on demand (see `Call::get_call`). `flow` is included
/// because tree rendering needs its small metadata (type/inline/fn/branch).
#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct CallHeader {
    pub call_id: Uuid,
    pub run_id: Uuid,
    pub function_name: String,
    pub static_scope: String,
    pub parent_call_id: Option<Uuid>,
    pub call_depth: i32,
    pub start_time: DateTime<Utc>,
    pub stop_time: Option<DateTime<Utc>>,
    pub duration_us: Option<i64>,
    pub runtime_path: Option<String>,
    pub has_args: bool,
    pub has_return_value: bool,
    pub flow: Option<serde_json::Value>,
    pub file: Option<String>,
    pub line: Option<i32>,
    pub column: Option<i32>,
    pub position: Option<i32>,
}

impl CallHeader {
    /// Get all call headers for a run (no args/return_value payloads),
    /// ordered by start_time.
    pub async fn get_by_run(
        pool: &DatabasePool,
        run_id: &Uuid,
    ) -> Result<Vec<CallHeader>, DatabaseError> {
        let calls = match pool {
            DatabasePool::Postgres(pool) => {
                sqlx::query_as::<_, CallHeader>(
                    r#"
                    SELECT call_id, run_id, function_name, static_scope,
                           parent_call_id, call_depth, start_time, stop_time,
                           duration_us, runtime_path,
                           (args IS NOT NULL) AS has_args,
                           (return_value IS NOT NULL) AS has_return_value,
                           flow, file, line, "column", position
                    FROM hot.call
                    WHERE run_id = $1
                    ORDER BY start_time
                    "#,
                )
                .bind(run_id)
                .fetch_all(pool)
                .await?
            }
            DatabasePool::Sqlite(pool) => {
                sqlx::query_as::<_, CallHeader>(
                    r#"
                    SELECT call_id, run_id, function_name, static_scope,
                           parent_call_id, call_depth, start_time, stop_time,
                           duration_us, runtime_path,
                           (args IS NOT NULL) AS has_args,
                           (return_value IS NOT NULL) AS has_return_value,
                           flow, file, line, "column", position
                    FROM call
                    WHERE run_id = ?
                    ORDER BY start_time
                    "#,
                )
                .bind(run_id)
                .fetch_all(pool)
                .await?
            }
        };

        Ok(calls)
    }
}

/// Escape LIKE wildcards so a user-supplied term matches literally.
/// Used with `ESCAPE '\'` in the query.
fn escape_like(term: &str) -> String {
    term.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

impl Call {
    /// Find calls in a run whose args or return value contain `term`
    /// (case-insensitive substring match on the stored JSON text). Powers the
    /// server-side result search for the run detail timeline/hierarchy, where
    /// payloads are no longer shipped to the browser. Note: for payloads
    /// spilled to blob storage this matches the inline BlobRef preview text,
    /// not the full blob content.
    pub async fn search_call_ids_by_payload(
        pool: &DatabasePool,
        run_id: &Uuid,
        term: &str,
        limit: i64,
    ) -> Result<Vec<Uuid>, DatabaseError> {
        let pattern = format!("%{}%", escape_like(term));
        let ids: Vec<(Uuid,)> = match pool {
            DatabasePool::Postgres(pool) => {
                sqlx::query_as(
                    r#"
                    SELECT call_id
                    FROM hot.call
                    WHERE run_id = $1
                      AND (args::text ILIKE $2 ESCAPE '\'
                           OR return_value::text ILIKE $2 ESCAPE '\')
                    LIMIT $3
                    "#,
                )
                .bind(run_id)
                .bind(&pattern)
                .bind(limit)
                .fetch_all(pool)
                .await?
            }
            DatabasePool::Sqlite(pool) => {
                // SQLite LIKE is case-insensitive for ASCII by default.
                sqlx::query_as(
                    r#"
                    SELECT call_id
                    FROM call
                    WHERE run_id = ?
                      AND (args LIKE ? ESCAPE '\'
                           OR return_value LIKE ? ESCAPE '\')
                    LIMIT ?
                    "#,
                )
                .bind(run_id)
                .bind(&pattern)
                .bind(&pattern)
                .bind(limit)
                .fetch_all(pool)
                .await?
            }
        };

        Ok(ids.into_iter().map(|(id,)| id).collect())
    }

    /// Get all calls for a run, ordered by start_time
    pub async fn get_calls_by_run(
        pool: &DatabasePool,
        run_id: &Uuid,
    ) -> Result<Vec<Call>, DatabaseError> {
        let calls = match pool {
            DatabasePool::Postgres(pool) => {
                sqlx::query_as::<_, Call>(
                    r#"
                    SELECT call_id, run_id, function_name, static_scope,
                           parent_call_id, call_depth, start_time, stop_time,
                           duration_us, runtime_path, args, return_value, flow,
                           file, line, "column", position
                    FROM hot.call
                    WHERE run_id = $1
                    ORDER BY start_time
                    "#,
                )
                .bind(run_id)
                .fetch_all(pool)
                .await?
            }
            DatabasePool::Sqlite(pool) => {
                sqlx::query_as::<_, Call>(
                    r#"
                    SELECT call_id, run_id, function_name, static_scope,
                           parent_call_id, call_depth, start_time, stop_time,
                           duration_us, runtime_path, args, return_value, flow,
                           file, line, "column", position
                    FROM call
                    WHERE run_id = ?
                    ORDER BY start_time
                    "#,
                )
                .bind(run_id)
                .fetch_all(pool)
                .await?
            }
        };

        Ok(calls)
    }

    /// Get a single call by ID
    pub async fn get_call(pool: &DatabasePool, call_id: &Uuid) -> Result<Call, DatabaseError> {
        let call = match pool {
            DatabasePool::Postgres(pool) => {
                sqlx::query_as::<_, Call>(
                    r#"
                    SELECT call_id, run_id, function_name, static_scope,
                           parent_call_id, call_depth, start_time, stop_time,
                           duration_us, runtime_path, args, return_value, flow,
                           file, line, "column", position
                    FROM hot.call
                    WHERE call_id = $1
                    "#,
                )
                .bind(call_id)
                .fetch_one(pool)
                .await?
            }
            DatabasePool::Sqlite(pool) => {
                sqlx::query_as::<_, Call>(
                    r#"
                    SELECT call_id, run_id, function_name, static_scope,
                           parent_call_id, call_depth, start_time, stop_time,
                           duration_us, runtime_path, args, return_value, flow,
                           file, line, "column", position
                    FROM call
                    WHERE call_id = ?
                    "#,
                )
                .bind(call_id)
                .fetch_one(pool)
                .await?
            }
        };

        Ok(call)
    }

    /// Get root calls (calls with no parent) for a run
    pub async fn get_root_calls(
        pool: &DatabasePool,
        run_id: &Uuid,
    ) -> Result<Vec<Call>, DatabaseError> {
        let calls = match pool {
            DatabasePool::Postgres(pool) => {
                sqlx::query_as::<_, Call>(
                    r#"
                    SELECT call_id, run_id, function_name, static_scope,
                           parent_call_id, call_depth, start_time, stop_time,
                           duration_us, runtime_path, args, return_value, flow,
                           file, line, "column", position
                    FROM hot.call
                    WHERE run_id = $1 AND parent_call_id IS NULL
                    ORDER BY start_time
                    "#,
                )
                .bind(run_id)
                .fetch_all(pool)
                .await?
            }
            DatabasePool::Sqlite(pool) => {
                sqlx::query_as::<_, Call>(
                    r#"
                    SELECT call_id, run_id, function_name, static_scope,
                           parent_call_id, call_depth, start_time, stop_time,
                           duration_us, runtime_path, args, return_value, flow,
                           file, line, "column", position
                    FROM call
                    WHERE run_id = ? AND parent_call_id IS NULL
                    ORDER BY start_time
                    "#,
                )
                .bind(run_id)
                .fetch_all(pool)
                .await?
            }
        };

        Ok(calls)
    }

    /// Get child calls for a parent call
    pub async fn get_child_calls(
        pool: &DatabasePool,
        parent_call_id: &Uuid,
    ) -> Result<Vec<Call>, DatabaseError> {
        let calls = match pool {
            DatabasePool::Postgres(pool) => {
                sqlx::query_as::<_, Call>(
                    r#"
                    SELECT call_id, run_id, function_name, static_scope,
                           parent_call_id, call_depth, start_time, stop_time,
                           duration_us, runtime_path, args, return_value, flow,
                           file, line, "column", position
                    FROM hot.call
                    WHERE parent_call_id = $1
                    ORDER BY start_time
                    "#,
                )
                .bind(parent_call_id)
                .fetch_all(pool)
                .await?
            }
            DatabasePool::Sqlite(pool) => {
                sqlx::query_as::<_, Call>(
                    r#"
                    SELECT call_id, run_id, function_name, static_scope,
                           parent_call_id, call_depth, start_time, stop_time,
                           duration_us, runtime_path, args, return_value, flow,
                           file, line, "column", position
                    FROM call
                    WHERE parent_call_id = ?
                    ORDER BY start_time
                    "#,
                )
                .bind(parent_call_id)
                .fetch_all(pool)
                .await?
            }
        };

        Ok(calls)
    }

    /// Delete calls older than `days` in batches (global — for local dev cleanup).
    /// Returns total rows deleted. Logs progress every 100k rows.
    pub async fn delete_older_than(
        pool: &DatabasePool,
        days: i32,
        batch_size: i64,
    ) -> Result<i64, DatabaseError> {
        let days_str = days.to_string();
        let mut total_deleted: i64 = 0;
        let mut last_logged: i64 = 0;

        debug!(
            "hot.dev: call retention cleanup starting (>{} days, batch_size={})",
            days, batch_size
        );

        loop {
            let deleted = match pool {
                DatabasePool::Postgres(pool) => {
                    let result = sqlx::query(
                        "WITH to_delete AS (
                            SELECT call_id FROM hot.call
                            WHERE start_time < NOW() - ($1 || ' days')::interval
                            LIMIT $2
                        )
                        DELETE FROM hot.call WHERE call_id IN (SELECT call_id FROM to_delete)",
                    )
                    .bind(&days_str)
                    .bind(batch_size)
                    .execute(pool)
                    .await?;
                    result.rows_affected() as i64
                }
                DatabasePool::Sqlite(pool) => {
                    let result = sqlx::query(
                        "DELETE FROM call WHERE call_id IN (
                            SELECT call_id FROM call
                            WHERE start_time < datetime('now', '-' || ? || ' days')
                            LIMIT ?
                        )",
                    )
                    .bind(&days_str)
                    .bind(batch_size)
                    .execute(pool)
                    .await?;
                    result.rows_affected() as i64
                }
            };

            if deleted == 0 {
                break;
            }

            total_deleted += deleted;

            if total_deleted - last_logged >= 100_000 {
                info!(
                    "hot.dev: call retention cleanup progress: {} rows deleted so far (>{} days)",
                    total_deleted, days
                );
                last_logged = total_deleted;
            }

            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }

        Ok(total_deleted)
    }

    /// Delete calls older than `days` for a specific org (scoped by env_ids).
    /// Returns total rows deleted. Logs progress at ~2% intervals for large deletions.
    pub async fn delete_older_than_for_org(
        pool: &DatabasePool,
        env_ids: &[Uuid],
        days: i32,
        batch_size: i64,
    ) -> Result<i64, DatabaseError> {
        if env_ids.is_empty() {
            return Ok(0);
        }

        let days_str = days.to_string();
        let mut total_deleted: i64 = 0;

        loop {
            let deleted = match pool {
                DatabasePool::Postgres(pool) => {
                    let result = sqlx::query(
                        "WITH to_delete AS (
                            SELECT c.call_id FROM hot.call c
                            JOIN hot.run r ON r.run_id = c.run_id
                            WHERE r.env_id = ANY($1)
                            AND c.start_time < NOW() - ($2 || ' days')::interval
                            LIMIT $3
                        )
                        DELETE FROM hot.call WHERE call_id IN (SELECT call_id FROM to_delete)",
                    )
                    .bind(env_ids)
                    .bind(&days_str)
                    .bind(batch_size)
                    .execute(pool)
                    .await?;
                    result.rows_affected() as i64
                }
                DatabasePool::Sqlite(pool) => {
                    let env_ids_str: Vec<String> =
                        env_ids.iter().map(|id| id.to_string()).collect();
                    let placeholders = env_ids_str
                        .iter()
                        .map(|_| "?")
                        .collect::<Vec<_>>()
                        .join(",");
                    let query = format!(
                        "DELETE FROM call WHERE call_id IN (
                            SELECT c.call_id FROM call c
                            JOIN run r ON r.run_id = c.run_id
                            WHERE r.env_id IN ({})
                            AND c.start_time < datetime('now', '-' || ? || ' days')
                            LIMIT ?
                        )",
                        placeholders
                    );
                    let mut q = sqlx::query(sqlx::AssertSqlSafe(query.as_str()));
                    for id_str in &env_ids_str {
                        q = q.bind(id_str);
                    }
                    q = q.bind(&days_str).bind(batch_size);
                    let result = q.execute(pool).await?;
                    result.rows_affected() as i64
                }
            };

            if deleted == 0 {
                break;
            }

            total_deleted += deleted;
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }

        Ok(total_deleted)
    }
}

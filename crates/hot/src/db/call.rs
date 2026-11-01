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
                            WHERE c.env_id = ANY($1)
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
                    let placeholders = env_ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
                    let query = format!(
                        "DELETE FROM call WHERE call_id IN (
                            SELECT c.call_id FROM call c
                            WHERE c.env_id IN ({})
                            AND c.start_time < datetime('now', '-' || ? || ' days')
                            LIMIT ?
                        )",
                        placeholders
                    );
                    let mut q = sqlx::query(sqlx::AssertSqlSafe(query.as_str()));
                    for id in env_ids {
                        q = q.bind(id);
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

    /// Delete the call traces in `scope` within `env_id`, `batch_size` at a
    /// time, and deactivate the blob refs that hold their spilled payloads so
    /// blob GC reclaims the objects. Each batch deletes its calls and
    /// deactivates their refs in one transaction, so a purge that stops
    /// partway leaves no call without its payload. Calls a still-running run
    /// writes after the purge remain.
    ///
    /// `db` needs write access to the environment's execution tables (an
    /// `Environment` scope on PostgreSQL).
    ///
    /// With a `deadline`, the purge stops starting batches once it passes and
    /// reports `complete: false`; purging the same scope again continues it.
    pub async fn purge(
        db: &DatabasePool,
        env_id: &Uuid,
        scope: CallPurgeScope,
        batch_size: i64,
        deadline: Option<std::time::Instant>,
    ) -> Result<CallPurge, DatabaseError> {
        let mut purged = CallPurge {
            calls: 0,
            blob_refs: 0,
            complete: false,
        };
        loop {
            if deadline.is_some_and(|deadline| std::time::Instant::now() >= deadline) {
                break;
            }
            let (calls, blob_refs) = Self::purge_batch(db, env_id, scope, batch_size).await?;
            purged.calls += calls;
            purged.blob_refs += blob_refs;
            if calls < batch_size {
                purged.complete = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        if purged.calls > 0 {
            info!(
                "hot.dev: call purge deleted {} calls and deactivated {} blob refs in env {} ({:?})",
                purged.calls, purged.blob_refs, env_id, scope
            );
        }
        Ok(purged)
    }

    /// Purge every call trace in `org_id`'s environments, and in its
    /// sub-orgs' environments when `include_sub_orgs` is set. A `deadline`
    /// works as in [`Self::purge`].
    pub async fn purge_org(
        db: &DatabasePool,
        org_id: &Uuid,
        include_sub_orgs: bool,
        batch_size: i64,
        deadline: Option<std::time::Instant>,
    ) -> Result<CallPurge, DatabaseError> {
        let envs = Self::org_purge_envs(db, org_id, include_sub_orgs).await?;
        Self::purge_envs(db, &envs, batch_size, deadline).await
    }

    /// Purge every call trace in each (org, environment) pair, in order. A
    /// `deadline` works as in [`Self::purge`]; an environment left incomplete
    /// stops the purge there.
    pub async fn purge_envs(
        db: &DatabasePool,
        envs: &[(Uuid, Uuid)],
        batch_size: i64,
        deadline: Option<std::time::Instant>,
    ) -> Result<CallPurge, DatabaseError> {
        let mut purged = CallPurge {
            calls: 0,
            blob_refs: 0,
            complete: true,
        };
        for &(org_id, env_id) in envs {
            let env_db = db.with_scope(super::DatabaseScope::Environment { org_id, env_id });
            let env_purged =
                Self::purge(&env_db, &env_id, CallPurgeScope::Env, batch_size, deadline).await?;
            purged.calls += env_purged.calls;
            purged.blob_refs += env_purged.blob_refs;
            if !env_purged.complete {
                purged.complete = false;
                break;
            }
        }
        Ok(purged)
    }

    /// The (org, environment) pairs [`Self::purge_org`] purges.
    pub async fn org_purge_envs(
        db: &DatabasePool,
        org_id: &Uuid,
        include_sub_orgs: bool,
    ) -> Result<Vec<(Uuid, Uuid)>, DatabaseError> {
        let mut org_ids = vec![*org_id];
        if include_sub_orgs {
            let children = super::Org::list_children(db, org_id).await?;
            org_ids.extend(children.into_iter().map(|child| child.org_id));
        }
        let mut envs = Vec::new();
        for org_id in org_ids {
            let listing = db.with_scope(super::DatabaseScope::Maintenance {
                operation: super::MaintenanceOperation::Retention,
                org_id: Some(org_id),
            });
            let env_ids = super::Env::get_ids_for_org(&listing, &org_id).await?;
            envs.extend(env_ids.into_iter().map(|env_id| (org_id, env_id)));
        }
        Ok(envs)
    }

    /// One batch of [`Self::purge`]: the calls deleted and refs deactivated.
    async fn purge_batch(
        db: &DatabasePool,
        env_id: &Uuid,
        scope: CallPurgeScope,
        batch_size: i64,
    ) -> Result<(i64, u64), DatabaseError> {
        let now = Utc::now();
        match db {
            DatabasePool::Postgres(pool) => {
                let (filter, target) = match scope {
                    CallPurgeScope::Env => ("", None),
                    CallPurgeScope::Stream(stream_id) => (
                        "AND run_id IN (SELECT run_id FROM hot.run WHERE env_id = $1 AND stream_id = $3)",
                        Some(stream_id),
                    ),
                    CallPurgeScope::Run(run_id) => ("AND run_id = $3", Some(run_id)),
                };
                let query = format!(
                    "DELETE FROM hot.call WHERE call_id IN (
                        SELECT call_id FROM hot.call WHERE env_id = $1 {filter} LIMIT $2
                    ) RETURNING call_id"
                );
                let mut tx = pool.begin().await?;
                let mut delete = sqlx::query_scalar::<_, Uuid>(sqlx::AssertSqlSafe(query.as_str()))
                    .bind(env_id)
                    .bind(batch_size);
                if let Some(target) = target {
                    delete = delete.bind(target);
                }
                let call_ids: Vec<String> = delete
                    .fetch_all(&mut *tx)
                    .await?
                    .iter()
                    .map(Uuid::to_string)
                    .collect();
                let blob_refs = if call_ids.is_empty() {
                    0
                } else {
                    sqlx::query(
                        "UPDATE hot.blob_ref SET active = false, deactivated_at = $1
                         WHERE active = true AND env_id = $2
                         AND source_kind = ANY($3) AND source_id = ANY($4)",
                    )
                    .bind(now)
                    .bind(env_id)
                    .bind(CALL_BLOB_REF_KINDS)
                    .bind(&call_ids)
                    .execute(&mut *tx)
                    .await?
                    .rows_affected()
                };
                tx.commit().await?;
                Ok((call_ids.len() as i64, blob_refs))
            }
            DatabasePool::Sqlite(pool) => {
                let (filter, target) = match scope {
                    CallPurgeScope::Env => ("", None),
                    CallPurgeScope::Stream(stream_id) => (
                        "AND run_id IN (SELECT run_id FROM run WHERE env_id = ?1 AND stream_id = ?3)",
                        Some(stream_id),
                    ),
                    CallPurgeScope::Run(run_id) => ("AND run_id = ?3", Some(run_id)),
                };
                let query = format!(
                    "DELETE FROM call WHERE call_id IN (
                        SELECT call_id FROM call WHERE env_id = ?1 {filter} LIMIT ?2
                    ) RETURNING call_id"
                );
                let mut tx = pool.begin().await?;
                let mut delete = sqlx::query_scalar::<_, Uuid>(sqlx::AssertSqlSafe(query.as_str()))
                    .bind(env_id)
                    .bind(batch_size);
                if let Some(target) = target {
                    delete = delete.bind(target);
                }
                let call_ids: Vec<String> = delete
                    .fetch_all(&mut *tx)
                    .await?
                    .iter()
                    .map(Uuid::to_string)
                    .collect();
                let blob_refs = if call_ids.is_empty() {
                    0
                } else {
                    let kinds = serde_json::to_string(CALL_BLOB_REF_KINDS).unwrap_or_default();
                    let ids = serde_json::to_string(&call_ids).unwrap_or_default();
                    sqlx::query(
                        "UPDATE blob_ref SET active = 0, deactivated_at = ?
                         WHERE active = 1 AND env_id = ?
                         AND source_kind IN (SELECT value FROM json_each(?))
                         AND source_id IN (SELECT value FROM json_each(?))",
                    )
                    .bind(now)
                    .bind(env_id)
                    .bind(kinds)
                    .bind(ids)
                    .execute(&mut *tx)
                    .await?
                    .rows_affected()
                };
                tx.commit().await?;
                Ok((call_ids.len() as i64, blob_refs))
            }
        }
    }
}

/// Blob ref source kinds that hold spilled call payloads.
pub const CALL_BLOB_REF_KINDS: &[&str] = &["call_args", "call_return", "call_flow"];

/// Which call traces [`Call::purge`] deletes. Every scope lies within one
/// environment; [`Call::purge_org`] purges each of an org's environments.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallPurgeScope {
    /// Every call in the environment.
    Env,
    /// Calls of the stream's runs.
    Stream(Uuid),
    /// Calls of one run.
    Run(Uuid),
}

/// What a purge removed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct CallPurge {
    /// Calls deleted.
    pub calls: i64,
    /// Blob refs to spilled call payloads deactivated.
    pub blob_refs: u64,
    /// False when a deadline stopped the purge with calls left; purging the
    /// same scope again continues it.
    pub complete: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn sqlite_per_org_retention_deletes_only_expired_calls() {
        let db = crate::db::test_db().await;
        let data = crate::db::insert_test_data(&db).await.unwrap();
        let DatabasePool::Sqlite(pool) = &db else {
            return;
        };
        for start in ["datetime('now','-30 days')", "CURRENT_TIMESTAMP"] {
            let sql = format!(
                "INSERT INTO call(call_id,run_id,env_id,function_name,static_scope,runtime_path,call_depth,start_time,size) VALUES(?,?,?,'::retention/test','root','root',0,{start},1)"
            );
            sqlx::query(sqlx::AssertSqlSafe(sql.as_str()))
                .bind(Uuid::now_v7())
                .bind(data.run_id)
                .bind(data.env_id)
                .execute(pool)
                .await
                .unwrap();
        }
        assert_eq!(
            Call::delete_older_than_for_org(&db, &[data.env_id], 7, 100)
                .await
                .unwrap(),
            1
        );
        let left: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM call WHERE env_id=?")
            .bind(data.env_id)
            .fetch_one(pool)
            .await
            .unwrap();
        assert_eq!(left, 1);
    }

    /// Insert a call in `run_id` with a spilled `call_args` payload; returns
    /// the call and its blob ref.
    async fn insert_traced_call(
        db: &DatabasePool,
        org_id: Uuid,
        env_id: Uuid,
        run_id: Uuid,
    ) -> (Uuid, Uuid) {
        let DatabasePool::Sqlite(pool) = db else {
            unreachable!("sqlite test");
        };
        let call_id = Uuid::now_v7();
        sqlx::query("INSERT INTO call(call_id,run_id,env_id,function_name,static_scope,runtime_path,call_depth,start_time,size) VALUES(?,?,?,'::purge/test','root','root',0,CURRENT_TIMESTAMP,1)")
            .bind(call_id)
            .bind(run_id)
            .bind(env_id)
            .execute(pool)
            .await
            .unwrap();
        let object = crate::db::blob::insert_pending_object(
            db,
            org_id,
            Some(env_id),
            "blake3",
            &call_id.to_string(),
            1,
            None,
            "local",
            &call_id.to_string(),
        )
        .await
        .unwrap();
        let blob_ref = crate::db::blob::insert_ref(
            db,
            object.blob_object_id,
            org_id,
            Some(env_id),
            "call_args",
            Some(&call_id.to_string()),
            None,
            Some(run_id),
        )
        .await
        .unwrap();
        (call_id, blob_ref.blob_ref_id)
    }

    /// A purge that finished, having removed `calls` and `blob_refs`.
    fn purged(calls: i64, blob_refs: u64) -> CallPurge {
        CallPurge {
            calls,
            blob_refs,
            complete: true,
        }
    }

    async fn ref_is_active(db: &DatabasePool, blob_ref_id: Uuid) -> bool {
        crate::db::blob::get_ref_by_id(db, blob_ref_id)
            .await
            .unwrap()
            .unwrap()
            .active
    }

    async fn calls_in_run(db: &DatabasePool, run_id: Uuid) -> i64 {
        let DatabasePool::Sqlite(pool) = db else {
            unreachable!("sqlite test");
        };
        sqlx::query_scalar("SELECT COUNT(*) FROM call WHERE run_id=?")
            .bind(run_id)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn sqlite_purge_deletes_only_its_scope_and_its_payload_refs() {
        let db = crate::db::test_db().await;
        let data = crate::db::insert_test_data(&db).await.unwrap();
        if !matches!(db, DatabasePool::Sqlite(_)) {
            return;
        }
        // data.run_id and `sibling` share data.stream_id; `other` has its own.
        let sibling = Uuid::now_v7();
        let other = Uuid::now_v7();
        let other_stream = Uuid::now_v7();
        for (run_id, stream_id) in [(sibling, data.stream_id), (other, other_stream)] {
            crate::db::run::Run::insert_run(
                &db,
                &run_id,
                &data.env_id,
                &stream_id,
                Some(&data.build_id),
                crate::db::run::RunType::Run.as_id(),
                None,
                &data.user_id,
                None,
                None,
            )
            .await
            .unwrap();
        }
        let mut refs = std::collections::HashMap::new();
        for run_id in [data.run_id, sibling, other] {
            for _ in 0..2 {
                let (_, blob_ref) = insert_traced_call(&db, data.org_id, data.env_id, run_id).await;
                refs.entry(run_id).or_insert_with(Vec::new).push(blob_ref);
            }
        }
        // A non-call ref that happens to share a call's source id is kept.
        let (shared_call, _) = insert_traced_call(&db, data.org_id, data.env_id, other).await;
        let object = crate::db::blob::insert_pending_object(
            &db,
            data.org_id,
            Some(data.env_id),
            "blake3",
            "store",
            1,
            None,
            "local",
            "store",
        )
        .await
        .unwrap();
        let store_ref = crate::db::blob::insert_ref(
            &db,
            object.blob_object_id,
            data.org_id,
            Some(data.env_id),
            "store_value",
            Some(&shared_call.to_string()),
            None,
            None,
        )
        .await
        .unwrap()
        .blob_ref_id;

        // A batch size of 1 takes several batches.
        let run = Call::purge(&db, &data.env_id, CallPurgeScope::Run(data.run_id), 1, None)
            .await
            .unwrap();
        assert_eq!(run, purged(2, 2));
        assert_eq!(calls_in_run(&db, data.run_id).await, 0);
        assert_eq!(calls_in_run(&db, sibling).await, 2);
        for blob_ref in &refs[&data.run_id] {
            assert!(!ref_is_active(&db, *blob_ref).await);
        }
        assert!(ref_is_active(&db, refs[&sibling][0]).await);

        let stream = Call::purge(
            &db,
            &data.env_id,
            CallPurgeScope::Stream(data.stream_id),
            100,
            None,
        )
        .await
        .unwrap();
        assert_eq!(stream, purged(2, 2));
        assert_eq!(calls_in_run(&db, sibling).await, 0);
        assert_eq!(calls_in_run(&db, other).await, 3);

        // A purge in another environment touches nothing here.
        let elsewhere = Call::purge(&db, &Uuid::now_v7(), CallPurgeScope::Env, 100, None)
            .await
            .unwrap();
        assert_eq!(elsewhere, purged(0, 0));

        let env = Call::purge(&db, &data.env_id, CallPurgeScope::Env, 100, None)
            .await
            .unwrap();
        assert_eq!(env, purged(3, 3));
        assert_eq!(calls_in_run(&db, other).await, 0);
        assert!(ref_is_active(&db, store_ref).await);
        // The runs themselves stay.
        assert!(
            crate::db::run::Run::get_run(&db, &data.run_id)
                .await
                .is_ok()
        );
    }

    /// A purge past its deadline stops with calls left and says so; the
    /// next purge of the same scope finishes it.
    #[tokio::test]
    async fn sqlite_purge_past_its_deadline_is_incomplete_until_repeated() {
        let db = crate::db::test_db().await;
        let data = crate::db::insert_test_data(&db).await.unwrap();
        if !matches!(db, DatabasePool::Sqlite(_)) {
            return;
        }
        for _ in 0..3 {
            insert_traced_call(&db, data.org_id, data.env_id, data.run_id).await;
        }
        let scope = CallPurgeScope::Run(data.run_id);
        let past = Some(std::time::Instant::now());
        let stopped = Call::purge(&db, &data.env_id, scope, 1, past)
            .await
            .unwrap();
        assert_eq!(
            stopped,
            CallPurge {
                calls: 0,
                blob_refs: 0,
                complete: false
            }
        );
        assert_eq!(calls_in_run(&db, data.run_id).await, 3);
        let org = Call::purge_org(&db, &data.org_id, false, 1, past)
            .await
            .unwrap();
        assert!(!org.complete);

        let finished = Call::purge(&db, &data.env_id, scope, 1, None)
            .await
            .unwrap();
        assert_eq!(finished, purged(3, 3));
        assert_eq!(calls_in_run(&db, data.run_id).await, 0);
    }

    #[tokio::test]
    async fn sqlite_org_purge_includes_sub_orgs_only_when_asked() {
        let db = crate::db::test_db().await;
        let data = crate::db::insert_test_data(&db).await.unwrap();
        let DatabasePool::Sqlite(pool) = &db else {
            return;
        };
        sqlx::query("UPDATE org SET org_type='organization' WHERE org_id=?")
            .bind(data.org_id)
            .execute(pool)
            .await
            .unwrap();
        let child = Uuid::now_v7();
        sqlx::query("INSERT INTO org (org_id,parent_org_id,name,slug,org_type,created_by_user_id) VALUES (?,?,'Child',?,'organization',?)")
            .bind(child)
            .bind(data.org_id)
            .bind(child.to_string())
            .bind(data.user_id)
            .execute(pool)
            .await
            .unwrap();
        let child_env = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO env (env_id,org_id,name,created_by_user_id) VALUES (?,?,'child',?)",
        )
        .bind(child_env)
        .bind(child)
        .bind(data.user_id)
        .execute(pool)
        .await
        .unwrap();
        let child_run = Uuid::now_v7();
        crate::db::run::Run::insert_run(
            &db,
            &child_run,
            &child_env,
            &Uuid::now_v7(),
            Some(&data.build_id),
            crate::db::run::RunType::Run.as_id(),
            None,
            &data.user_id,
            None,
            None,
        )
        .await
        .unwrap();
        insert_traced_call(&db, data.org_id, data.env_id, data.run_id).await;
        insert_traced_call(&db, child, child_env, child_run).await;

        let root_only = Call::purge_org(&db, &data.org_id, false, 100, None)
            .await
            .unwrap();
        assert_eq!(root_only, purged(1, 1));
        assert_eq!(calls_in_run(&db, child_run).await, 1);

        let with_children = Call::purge_org(&db, &data.org_id, true, 100, None)
            .await
            .unwrap();
        assert_eq!(with_children, purged(1, 1));
        assert_eq!(calls_in_run(&db, child_run).await, 0);
    }
}

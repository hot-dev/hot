use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use std::str::FromStr;
use thiserror::Error;
use uuid::Uuid;

use super::search::{IdSearch, pg_placeholders, sqlite_placeholders};

#[derive(Error, Debug)]
pub enum TaskError {
    #[error("Database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("Task not found")]
    NotFound,
}

/// Heartbeat freshness window used by quota / "remaining concurrency" callers.
///
/// A `running` task whose `last_heartbeat_at` is older than this is treated as
/// a zombie and is *not* counted against an org's box-concurrency quota. The
/// background reaper in the task worker will eventually flip such rows to
/// `failed`; until then we don't want a single dead worker to brick an org's
/// ability to launch boxes.
///
/// Sized at ~8× the worker heartbeat interval (15s). Anything fresher than
/// 120s is almost certainly a live task; anything staler is almost certainly
/// a zombie waiting to be reaped.
pub const QUOTA_HEARTBEAT_FRESH_SECS: i64 = 120;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TaskStatus {
    Queued,
    Running,
    Completed,
    Failed,
    Cancelled,
    TimedOut,
}

impl TaskStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            TaskStatus::Queued => "queued",
            TaskStatus::Running => "running",
            TaskStatus::Completed => "completed",
            TaskStatus::Failed => "failed",
            TaskStatus::Cancelled => "cancelled",
            TaskStatus::TimedOut => "timed_out",
        }
    }

    pub fn as_id(&self) -> i16 {
        match self {
            TaskStatus::Queued => 1,
            TaskStatus::Running => 2,
            TaskStatus::Completed => 3,
            TaskStatus::Failed => 4,
            TaskStatus::Cancelled => 5,
            TaskStatus::TimedOut => 6,
        }
    }

    pub fn from_id(id: i16) -> Option<Self> {
        match id {
            1 => Some(TaskStatus::Queued),
            2 => Some(TaskStatus::Running),
            3 => Some(TaskStatus::Completed),
            4 => Some(TaskStatus::Failed),
            5 => Some(TaskStatus::Cancelled),
            6 => Some(TaskStatus::TimedOut),
            _ => None,
        }
    }
}

impl std::fmt::Display for TaskStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

impl FromStr for TaskStatus {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "queued" => Ok(TaskStatus::Queued),
            "running" => Ok(TaskStatus::Running),
            "completed" => Ok(TaskStatus::Completed),
            "failed" => Ok(TaskStatus::Failed),
            "cancelled" => Ok(TaskStatus::Cancelled),
            "timed_out" => Ok(TaskStatus::TimedOut),
            _ => Err(format!("Invalid task status: {}", s)),
        }
    }
}

#[derive(Debug, FromRow)]
pub struct Task {
    pub task_id: Uuid,
    pub env_id: Uuid,
    pub stream_id: Uuid,
    pub build_id: Uuid,
    pub origin_run_id: Option<Uuid>,
    pub task_status_id: i16,
    pub status: String,
    pub function_name: String,
    pub args: Option<serde_json::Value>,
    pub options: Option<serde_json::Value>,
    pub task_type: String,
    pub start_time: Option<DateTime<Utc>>,
    pub stop_time: Option<DateTime<Utc>>,
    pub duration_ms: Option<i64>,
    pub result: Option<serde_json::Value>,
    pub info: Option<serde_json::Value>,
    pub timeout_ms: i64,
    pub retry_attempt: i16,
    pub next_retry_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub by_user_id: Option<Uuid>,
    pub run_id: Option<Uuid>,
    pub worker_id: Option<String>,
    pub last_heartbeat_at: Option<DateTime<Utc>>,
    pub container_id: Option<String>,
    #[sqlx(default)]
    pub origin_run_fn: Option<String>,
}

const TASK_ORIGIN_FN_PG: &str = "\
    COALESCE(\
        origin_e.event_data->>'fn', \
        (SELECT function_name FROM call WHERE run_id = origin_r.run_id AND parent_call_id IS NULL LIMIT 1), \
        (SELECT function_name FROM task t2 WHERE t2.run_id = origin_r.run_id LIMIT 1)\
    ) as origin_run_fn";

const TASK_ORIGIN_FN_SQLITE: &str = "\
    COALESCE(\
        json_extract(origin_e.event_data, '$.fn'), \
        (SELECT function_name FROM call WHERE run_id = origin_r.run_id AND parent_call_id IS NULL LIMIT 1), \
        (SELECT function_name FROM task t2 WHERE t2.run_id = origin_r.run_id LIMIT 1)\
    ) as origin_run_fn";

const TASK_ORIGIN_JOIN: &str = "\
    LEFT JOIN run origin_r ON origin_r.run_id = t.origin_run_id \
    LEFT JOIN event origin_e ON origin_r.event_id = origin_e.event_id";

const TASK_ORIGIN_FN_EXPR_PG: &str = "\
    COALESCE(\
        origin_e.event_data->>'fn', \
        (SELECT function_name FROM call WHERE run_id = origin_r.run_id AND parent_call_id IS NULL LIMIT 1), \
        (SELECT function_name FROM task t2 WHERE t2.run_id = origin_r.run_id LIMIT 1)\
    )";

const TASK_ORIGIN_FN_EXPR_SQLITE: &str = "\
    COALESCE(\
        json_extract(origin_e.event_data, '$.fn'), \
        (SELECT function_name FROM call WHERE run_id = origin_r.run_id AND parent_call_id IS NULL LIMIT 1), \
        (SELECT function_name FROM task t2 WHERE t2.run_id = origin_r.run_id LIMIT 1)\
    )";

impl Task {
    pub fn status(&self) -> Option<TaskStatus> {
        TaskStatus::from_id(self.task_status_id)
    }

    /// Insert a new task with status 'queued'.
    #[allow(clippy::too_many_arguments)]
    pub async fn insert(
        db: &crate::db::DatabasePool,
        task_id: &Uuid,
        env_id: &Uuid,
        stream_id: &Uuid,
        build_id: &Uuid,
        origin_run_id: Option<&Uuid>,
        function_name: &str,
        args: Option<&serde_json::Value>,
        options: Option<&serde_json::Value>,
        task_type: &str,
        timeout_ms: i64,
        by_user_id: Option<&Uuid>,
    ) -> Result<(), TaskError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                sqlx::query(
                    "INSERT INTO task (task_id, env_id, stream_id, build_id, origin_run_id, task_status_id, function_name, args, options, task_type, timeout_ms, by_user_id) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)",
                )
                .bind(task_id)
                .bind(env_id)
                .bind(stream_id)
                .bind(build_id)
                .bind(origin_run_id)
                .bind(TaskStatus::Queued.as_id())
                .bind(function_name)
                .bind(args)
                .bind(options)
                .bind(task_type)
                .bind(timeout_ms)
                .bind(by_user_id)
                .execute(pg_pool)
                .await?;
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let args_str = args.map(|a| serde_json::to_string(a).unwrap_or_default());
                let options_str = options.map(|o| serde_json::to_string(o).unwrap_or_default());
                sqlx::query(
                    "INSERT INTO task (task_id, env_id, stream_id, build_id, origin_run_id, task_status_id, function_name, args, options, task_type, timeout_ms, by_user_id) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                )
                .bind(task_id)
                .bind(env_id)
                .bind(stream_id)
                .bind(build_id)
                .bind(origin_run_id)
                .bind(TaskStatus::Queued.as_id())
                .bind(function_name)
                .bind(args_str)
                .bind(options_str)
                .bind(task_type)
                .bind(timeout_ms)
                .bind(by_user_id)
                .execute(sqlite_pool)
                .await?;
            }
        }
        Ok(())
    }

    /// Insert a retry task — copies fields from the original but with a new ID and incremented attempt.
    #[allow(clippy::too_many_arguments)]
    pub async fn insert_retry(
        db: &crate::db::DatabasePool,
        new_task_id: &Uuid,
        original: &Task,
        retry_attempt: i16,
        next_retry_at: DateTime<Utc>,
    ) -> Result<(), TaskError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                sqlx::query(
                    "INSERT INTO task (task_id, env_id, stream_id, build_id, origin_run_id, task_status_id, function_name, args, options, task_type, timeout_ms, retry_attempt, next_retry_at, by_user_id) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14)",
                )
                .bind(new_task_id)
                .bind(original.env_id)
                .bind(original.stream_id)
                .bind(original.build_id)
                .bind(original.origin_run_id)
                .bind(TaskStatus::Queued.as_id())
                .bind(&original.function_name)
                .bind(&original.args)
                .bind(&original.options)
                .bind(&original.task_type)
                .bind(original.timeout_ms)
                .bind(retry_attempt)
                .bind(next_retry_at)
                .bind(original.by_user_id)
                .execute(pg_pool)
                .await?;
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let args_str = original
                    .args
                    .as_ref()
                    .map(|a| serde_json::to_string(a).unwrap_or_default());
                let options_str = original
                    .options
                    .as_ref()
                    .map(|o| serde_json::to_string(o).unwrap_or_default());
                sqlx::query(
                    "INSERT INTO task (task_id, env_id, stream_id, build_id, origin_run_id, task_status_id, function_name, args, options, task_type, timeout_ms, retry_attempt, next_retry_at, by_user_id) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                )
                .bind(new_task_id)
                .bind(original.env_id)
                .bind(original.stream_id)
                .bind(original.build_id)
                .bind(original.origin_run_id)
                .bind(TaskStatus::Queued.as_id())
                .bind(&original.function_name)
                .bind(args_str)
                .bind(options_str)
                .bind(&original.task_type)
                .bind(original.timeout_ms)
                .bind(retry_attempt)
                .bind(next_retry_at)
                .bind(original.by_user_id)
                .execute(sqlite_pool)
                .await?;
            }
        }
        Ok(())
    }

    /// Mark a task as running and set start_time.
    pub async fn mark_running(
        db: &crate::db::DatabasePool,
        task_id: &Uuid,
    ) -> Result<(), TaskError> {
        let now = Utc::now();
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                sqlx::query(
                    "UPDATE task SET task_status_id = $1, start_time = $2 WHERE task_id = $3",
                )
                .bind(TaskStatus::Running.as_id())
                .bind(now)
                .bind(task_id)
                .execute(pg_pool)
                .await?;
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                sqlx::query("UPDATE task SET task_status_id = ?, start_time = ? WHERE task_id = ?")
                    .bind(TaskStatus::Running.as_id())
                    .bind(now)
                    .bind(task_id)
                    .execute(sqlite_pool)
                    .await?;
            }
        }
        Ok(())
    }

    /// Cancel a task. Only affects tasks in queued or running state.
    /// Returns true if the task was actually cancelled, false if already in a terminal state.
    pub async fn cancel(db: &crate::db::DatabasePool, task_id: &Uuid) -> Result<bool, TaskError> {
        let now = Utc::now();
        let cancelled_id = TaskStatus::Cancelled.as_id();
        let queued_id = TaskStatus::Queued.as_id();
        let running_id = TaskStatus::Running.as_id();

        let rows_affected = match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                sqlx::query(
                    "UPDATE task SET task_status_id = $1, stop_time = $2, duration_ms = CASE WHEN start_time IS NOT NULL THEN EXTRACT(EPOCH FROM ($2 - start_time)) * 1000 ELSE 0 END WHERE task_id = $3 AND task_status_id IN ($4, $5)",
                )
                .bind(cancelled_id)
                .bind(now)
                .bind(task_id)
                .bind(queued_id)
                .bind(running_id)
                .execute(pg_pool)
                .await?
                .rows_affected()
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                sqlx::query(
                    "UPDATE task SET task_status_id = ?, stop_time = ?, duration_ms = CASE WHEN start_time IS NOT NULL THEN CAST((julianday(?) - julianday(start_time)) * 86400000 AS INTEGER) ELSE 0 END WHERE task_id = ? AND task_status_id IN (?, ?)",
                )
                .bind(cancelled_id)
                .bind(now)
                .bind(now)
                .bind(task_id)
                .bind(queued_id)
                .bind(running_id)
                .execute(sqlite_pool)
                .await?
                .rows_affected()
            }
        };
        Ok(rows_affected > 0)
    }

    /// Complete a task with a final status, result, and computed duration.
    pub async fn complete(
        db: &crate::db::DatabasePool,
        task_id: &Uuid,
        status: &TaskStatus,
        result: Option<&serde_json::Value>,
    ) -> Result<(), TaskError> {
        let now = Utc::now();
        let queued_id = TaskStatus::Queued.as_id();
        let running_id = TaskStatus::Running.as_id();
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                sqlx::query(
                    r#"UPDATE task SET
                        task_status_id = $1,
                        stop_time = $2,
                        duration_ms = EXTRACT(EPOCH FROM ($2 - start_time)) * 1000,
                        result = $3
                    WHERE task_id = $4
                      AND task_status_id IN ($5, $6)"#,
                )
                .bind(status.as_id())
                .bind(now)
                .bind(result)
                .bind(task_id)
                .bind(queued_id)
                .bind(running_id)
                .execute(pg_pool)
                .await?;
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let result_str = result.map(|r| serde_json::to_string(r).unwrap_or_default());
                sqlx::query(
                    r#"UPDATE task SET
                        task_status_id = ?,
                        stop_time = ?,
                        duration_ms = CAST((julianday(?) - julianday(start_time)) * 86400000 AS INTEGER),
                        result = ?
                    WHERE task_id = ?
                      AND task_status_id IN (?, ?)"#,
                )
                .bind(status.as_id())
                .bind(now)
                .bind(now)
                .bind(result_str)
                .bind(task_id)
                .bind(queued_id)
                .bind(running_id)
                .execute(sqlite_pool)
                .await?;
            }
        }
        Ok(())
    }

    /// Link a task to its execution run (the task-type run created by the task worker).
    pub async fn set_run_id(
        db: &crate::db::DatabasePool,
        task_id: &Uuid,
        run_id: &Uuid,
    ) -> Result<(), TaskError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                sqlx::query("UPDATE task SET run_id = $1 WHERE task_id = $2")
                    .bind(run_id)
                    .bind(task_id)
                    .execute(pg_pool)
                    .await?;
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                sqlx::query("UPDATE task SET run_id = ? WHERE task_id = ?")
                    .bind(run_id)
                    .bind(task_id)
                    .execute(sqlite_pool)
                    .await?;
            }
        }
        Ok(())
    }

    /// Get a task by its execution run_id.
    pub async fn get_by_run_id(
        db: &crate::db::DatabasePool,
        run_id: &Uuid,
    ) -> Result<Option<Task>, TaskError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let q = format!(
                    "SELECT t.*, ts.name as status, {TASK_ORIGIN_FN_PG} \
                     FROM task t JOIN task_status ts ON t.task_status_id = ts.task_status_id \
                     {TASK_ORIGIN_JOIN} WHERE t.run_id = $1"
                );
                let task = sqlx::query_as::<_, Task>(sqlx::AssertSqlSafe(q.as_str()))
                    .bind(run_id)
                    .fetch_optional(pg_pool)
                    .await?;
                Ok(task)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let q = format!(
                    "SELECT t.*, ts.name as status, {TASK_ORIGIN_FN_SQLITE} \
                     FROM task t JOIN task_status ts ON t.task_status_id = ts.task_status_id \
                     {TASK_ORIGIN_JOIN} WHERE t.run_id = ?"
                );
                let task = sqlx::query_as::<_, Task>(sqlx::AssertSqlSafe(q.as_str()))
                    .bind(run_id)
                    .fetch_optional(sqlite_pool)
                    .await?;
                Ok(task)
            }
        }
    }

    /// Get tasks spawned by a given origin run, ordered by creation time.
    pub async fn get_by_origin_run_id(
        db: &crate::db::DatabasePool,
        origin_run_id: &Uuid,
        limit: Option<i64>,
    ) -> Result<Vec<Task>, TaskError> {
        let limit = limit.unwrap_or(50);
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let q = format!(
                    "SELECT t.*, ts.name as status, {TASK_ORIGIN_FN_PG} \
                     FROM task t JOIN task_status ts ON t.task_status_id = ts.task_status_id \
                     {TASK_ORIGIN_JOIN} WHERE t.origin_run_id = $1 ORDER BY t.created_at DESC LIMIT $2"
                );
                let tasks = sqlx::query_as::<_, Task>(sqlx::AssertSqlSafe(q.as_str()))
                    .bind(origin_run_id)
                    .bind(limit)
                    .fetch_all(pg_pool)
                    .await?;
                Ok(tasks)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let q = format!(
                    "SELECT t.*, ts.name as status, {TASK_ORIGIN_FN_SQLITE} \
                     FROM task t JOIN task_status ts ON t.task_status_id = ts.task_status_id \
                     {TASK_ORIGIN_JOIN} WHERE t.origin_run_id = ? ORDER BY t.created_at DESC LIMIT ?"
                );
                let tasks = sqlx::query_as::<_, Task>(sqlx::AssertSqlSafe(q.as_str()))
                    .bind(origin_run_id)
                    .bind(limit)
                    .fetch_all(sqlite_pool)
                    .await?;
                Ok(tasks)
            }
        }
    }

    /// Get a task by ID with status name.
    pub async fn get(db: &crate::db::DatabasePool, task_id: &Uuid) -> Result<Task, TaskError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let q = format!(
                    "SELECT t.*, ts.name as status, {TASK_ORIGIN_FN_PG} \
                     FROM task t JOIN task_status ts ON t.task_status_id = ts.task_status_id \
                     {TASK_ORIGIN_JOIN} WHERE t.task_id = $1"
                );
                let task = sqlx::query_as::<_, Task>(sqlx::AssertSqlSafe(q.as_str()))
                    .bind(task_id)
                    .fetch_one(pg_pool)
                    .await?;
                Ok(task)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let q = format!(
                    "SELECT t.*, ts.name as status, {TASK_ORIGIN_FN_SQLITE} \
                     FROM task t JOIN task_status ts ON t.task_status_id = ts.task_status_id \
                     {TASK_ORIGIN_JOIN} WHERE t.task_id = ?"
                );
                let task = sqlx::query_as::<_, Task>(sqlx::AssertSqlSafe(q.as_str()))
                    .bind(task_id)
                    .fetch_one(sqlite_pool)
                    .await?;
                Ok(task)
            }
        }
    }

    /// Get queued tasks old enough that a lost queue entry should be reconciled.
    /// `next_retry_at` is reused as a throttle so the reconciler does not
    /// enqueue duplicate copies on every tick while a stale queued task waits.
    pub async fn get_stale_queued(
        db: &crate::db::DatabasePool,
        cutoff: DateTime<Utc>,
        now: DateTime<Utc>,
        limit: i64,
    ) -> Result<Vec<Task>, TaskError> {
        let queued_id = TaskStatus::Queued.as_id();
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let q = format!(
                    "SELECT t.*, ts.name as status, {TASK_ORIGIN_FN_PG} \
                     FROM task t JOIN task_status ts ON t.task_status_id = ts.task_status_id \
                     {TASK_ORIGIN_JOIN} \
                     WHERE t.task_status_id = $1 \
                       AND t.created_at <= $2 \
                       AND (t.next_retry_at IS NULL OR t.next_retry_at <= $3) \
                     ORDER BY t.created_at ASC LIMIT $4"
                );
                let tasks = sqlx::query_as::<_, Task>(sqlx::AssertSqlSafe(q.as_str()))
                    .bind(queued_id)
                    .bind(cutoff)
                    .bind(now)
                    .bind(limit)
                    .fetch_all(pg_pool)
                    .await?;
                Ok(tasks)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let q = format!(
                    "SELECT t.*, ts.name as status, {TASK_ORIGIN_FN_SQLITE} \
                     FROM task t JOIN task_status ts ON t.task_status_id = ts.task_status_id \
                     {TASK_ORIGIN_JOIN} \
                     WHERE t.task_status_id = ? \
                       AND t.created_at <= ? \
                       AND (t.next_retry_at IS NULL OR t.next_retry_at <= ?) \
                     ORDER BY t.created_at ASC LIMIT ?"
                );
                let tasks = sqlx::query_as::<_, Task>(sqlx::AssertSqlSafe(q.as_str()))
                    .bind(queued_id)
                    .bind(cutoff)
                    .bind(now)
                    .bind(limit)
                    .fetch_all(sqlite_pool)
                    .await?;
                Ok(tasks)
            }
        }
    }

    /// Defer the next queued-task reconciliation attempt after successfully
    /// enqueueing a replacement queue entry.
    pub async fn defer_queued_reconcile(
        db: &crate::db::DatabasePool,
        task_id: &Uuid,
        next_check_at: DateTime<Utc>,
    ) -> Result<(), TaskError> {
        let queued_id = TaskStatus::Queued.as_id();
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                sqlx::query(
                    "UPDATE task SET next_retry_at = $1 \
                     WHERE task_id = $2 AND task_status_id = $3",
                )
                .bind(next_check_at)
                .bind(task_id)
                .bind(queued_id)
                .execute(pg_pool)
                .await?;
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                sqlx::query(
                    "UPDATE task SET next_retry_at = ? \
                     WHERE task_id = ? AND task_status_id = ?",
                )
                .bind(next_check_at)
                .bind(task_id)
                .bind(queued_id)
                .execute(sqlite_pool)
                .await?;
            }
        }
        Ok(())
    }

    /// Get tasks by stream ID, ordered by creation time.
    pub async fn get_by_stream(
        db: &crate::db::DatabasePool,
        stream_id: &Uuid,
        env_id: &Uuid,
        limit: Option<i64>,
    ) -> Result<Vec<Task>, TaskError> {
        let limit = limit.unwrap_or(50);
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let q = format!(
                    "SELECT t.*, ts.name as status, {TASK_ORIGIN_FN_PG} \
                     FROM task t JOIN task_status ts ON t.task_status_id = ts.task_status_id \
                     {TASK_ORIGIN_JOIN} WHERE t.stream_id = $1 AND t.env_id = $2 ORDER BY t.created_at DESC LIMIT $3"
                );
                let tasks = sqlx::query_as::<_, Task>(sqlx::AssertSqlSafe(q.as_str()))
                    .bind(stream_id)
                    .bind(env_id)
                    .bind(limit)
                    .fetch_all(pg_pool)
                    .await?;
                Ok(tasks)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let q = format!(
                    "SELECT t.*, ts.name as status, {TASK_ORIGIN_FN_SQLITE} \
                     FROM task t JOIN task_status ts ON t.task_status_id = ts.task_status_id \
                     {TASK_ORIGIN_JOIN} WHERE t.stream_id = ? AND t.env_id = ? ORDER BY t.created_at DESC LIMIT ?"
                );
                let tasks = sqlx::query_as::<_, Task>(sqlx::AssertSqlSafe(q.as_str()))
                    .bind(stream_id)
                    .bind(env_id)
                    .bind(limit)
                    .fetch_all(sqlite_pool)
                    .await?;
                Ok(tasks)
            }
        }
    }

    /// Count running container tasks for an organization (for concurrent limit enforcement).
    ///
    /// Tasks whose heartbeat is older than `stale_heartbeat_secs` are excluded
    /// from the count: those are zombies belonging to a dead worker that the
    /// periodic reaper hasn't gotten to yet, and they should not consume the
    /// org's quota. Pass `0` to disable the freshness filter (count every
    /// `running` row regardless of heartbeat age).
    ///
    /// Tasks created very recently (within the heartbeat interval, ~15s) may
    /// have a `last_heartbeat_at` that is older than `stale_heartbeat_secs`
    /// purely because the periodic heartbeat hasn't fired yet — those are
    /// covered by the `OR start_time > NOW() - <threshold>` clause so we
    /// don't undercount genuinely fresh work.
    pub async fn count_running_containers_for_org(
        db: &crate::db::DatabasePool,
        org_id: &Uuid,
        stale_heartbeat_secs: i64,
    ) -> Result<i64, TaskError> {
        let running_id = TaskStatus::Running.as_id();
        if stale_heartbeat_secs <= 0 {
            return Self::count_running_containers_for_org_unfiltered(db, org_id).await;
        }
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let row: (i64,) = sqlx::query_as(
                    "SELECT COUNT(*)::bigint FROM task t
                     JOIN env e ON e.env_id = t.env_id
                     WHERE e.org_id = $1
                       AND t.task_status_id = $2
                       AND t.task_type = 'container'
                       AND (
                           (t.last_heartbeat_at IS NOT NULL
                            AND t.last_heartbeat_at >= NOW() - ($3 || ' seconds')::interval)
                        OR (t.start_time IS NOT NULL
                            AND t.start_time >= NOW() - ($3 || ' seconds')::interval)
                        OR (t.start_time IS NULL
                            AND t.created_at >= NOW() - ($3 || ' seconds')::interval)
                       )",
                )
                .bind(org_id)
                .bind(running_id)
                .bind(stale_heartbeat_secs.to_string())
                .fetch_one(pg_pool)
                .await?;
                Ok(row.0)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let row: (i64,) = sqlx::query_as(
                    "SELECT COUNT(*) FROM task t
                     JOIN env e ON e.env_id = t.env_id
                     WHERE e.org_id = ?
                       AND t.task_status_id = ?
                       AND t.task_type = 'container'
                       AND (
                           (t.last_heartbeat_at IS NOT NULL
                            AND t.last_heartbeat_at >= datetime('now', '-' || ? || ' seconds'))
                        OR (t.start_time IS NOT NULL
                            AND t.start_time >= datetime('now', '-' || ? || ' seconds'))
                        OR (t.start_time IS NULL
                            AND t.created_at >= datetime('now', '-' || ? || ' seconds'))
                       )",
                )
                .bind(org_id)
                .bind(running_id)
                .bind(stale_heartbeat_secs.to_string())
                .bind(stale_heartbeat_secs.to_string())
                .bind(stale_heartbeat_secs.to_string())
                .fetch_one(sqlite_pool)
                .await?;
                Ok(row.0)
            }
        }
    }

    /// Unfiltered variant kept for callers that want a strict row count
    /// (e.g. UI displays of "what the DB thinks is running") without applying
    /// the heartbeat freshness filter.
    pub async fn count_running_containers_for_org_unfiltered(
        db: &crate::db::DatabasePool,
        org_id: &Uuid,
    ) -> Result<i64, TaskError> {
        let running_id = TaskStatus::Running.as_id();
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let row: (i64,) = sqlx::query_as(
                    "SELECT COUNT(*)::bigint FROM task t
                     JOIN env e ON e.env_id = t.env_id
                     WHERE e.org_id = $1 AND t.task_status_id = $2 AND t.task_type = 'container'",
                )
                .bind(org_id)
                .bind(running_id)
                .fetch_one(pg_pool)
                .await?;
                Ok(row.0)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let row: (i64,) = sqlx::query_as(
                    "SELECT COUNT(*) FROM task t
                     JOIN env e ON e.env_id = t.env_id
                     WHERE e.org_id = ? AND t.task_status_id = ? AND t.task_type = 'container'",
                )
                .bind(org_id)
                .bind(running_id)
                .fetch_one(sqlite_pool)
                .await?;
                Ok(row.0)
            }
        }
    }

    /// Count all tasks for an environment.
    pub async fn get_count_by_env(
        db: &crate::db::DatabasePool,
        env_id: &Uuid,
    ) -> Result<i64, TaskError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let row: (i64,) =
                    sqlx::query_as("SELECT COUNT(*)::bigint FROM task WHERE env_id = $1")
                        .bind(env_id)
                        .fetch_one(pg_pool)
                        .await?;
                Ok(row.0)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let row: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM task WHERE env_id = ?")
                    .bind(env_id)
                    .fetch_one(sqlite_pool)
                    .await?;
                Ok(row.0)
            }
        }
    }

    /// Count tasks by status for an environment.
    pub async fn get_count_by_status_and_env(
        db: &crate::db::DatabasePool,
        status: &TaskStatus,
        env_id: &Uuid,
    ) -> Result<i64, TaskError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let row: (i64,) = sqlx::query_as(
                    "SELECT COUNT(*)::bigint FROM task WHERE env_id = $1 AND task_status_id = $2",
                )
                .bind(env_id)
                .bind(status.as_id())
                .fetch_one(pg_pool)
                .await?;
                Ok(row.0)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let row: (i64,) = sqlx::query_as(
                    "SELECT COUNT(*) FROM task WHERE env_id = ? AND task_status_id = ?",
                )
                .bind(env_id)
                .bind(status.as_id())
                .fetch_one(sqlite_pool)
                .await?;
                Ok(row.0)
            }
        }
    }

    /// Get all status counts for an environment in a single query.
    /// Returns (total, queued, running, completed, failed, cancelled, timed_out).
    pub async fn get_all_status_counts_by_env(
        db: &crate::db::DatabasePool,
        env_id: &Uuid,
    ) -> Result<(i64, i64, i64, i64, i64, i64, i64), TaskError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let rows: Vec<(i16, i64)> = sqlx::query_as(
                    "SELECT task_status_id, COUNT(*)::bigint FROM task WHERE env_id = $1 GROUP BY task_status_id",
                )
                .bind(env_id)
                .fetch_all(pg_pool)
                .await?;

                let mut queued = 0i64;
                let mut running = 0i64;
                let mut completed = 0i64;
                let mut failed = 0i64;
                let mut cancelled = 0i64;
                let mut timed_out = 0i64;
                let mut total = 0i64;
                for (status_id, count) in &rows {
                    total += count;
                    match TaskStatus::from_id(*status_id) {
                        Some(TaskStatus::Queued) => queued = *count,
                        Some(TaskStatus::Running) => running = *count,
                        Some(TaskStatus::Completed) => completed = *count,
                        Some(TaskStatus::Failed) => failed = *count,
                        Some(TaskStatus::Cancelled) => cancelled = *count,
                        Some(TaskStatus::TimedOut) => timed_out = *count,
                        None => {}
                    }
                }
                Ok((
                    total, queued, running, completed, failed, cancelled, timed_out,
                ))
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let rows: Vec<(i16, i64)> = sqlx::query_as(
                    "SELECT task_status_id, COUNT(*) FROM task WHERE env_id = ? GROUP BY task_status_id",
                )
                .bind(env_id)
                .fetch_all(sqlite_pool)
                .await?;

                let mut queued = 0i64;
                let mut running = 0i64;
                let mut completed = 0i64;
                let mut failed = 0i64;
                let mut cancelled = 0i64;
                let mut timed_out = 0i64;
                let mut total = 0i64;
                for (status_id, count) in &rows {
                    total += count;
                    match TaskStatus::from_id(*status_id) {
                        Some(TaskStatus::Queued) => queued = *count,
                        Some(TaskStatus::Running) => running = *count,
                        Some(TaskStatus::Completed) => completed = *count,
                        Some(TaskStatus::Failed) => failed = *count,
                        Some(TaskStatus::Cancelled) => cancelled = *count,
                        Some(TaskStatus::TimedOut) => timed_out = *count,
                        None => {}
                    }
                }
                Ok((
                    total, queued, running, completed, failed, cancelled, timed_out,
                ))
            }
        }
    }

    /// Get total CUS (Compute Unit Seconds) and total duration_ms for an environment.
    /// Returns (total_cus, total_duration_ms).
    pub async fn get_compute_stats_by_env(
        db: &crate::db::DatabasePool,
        env_id: &Uuid,
    ) -> Result<(i64, i64), TaskError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let row: (Option<i64>, Option<i64>) = sqlx::query_as(
                    "SELECT COALESCE(SUM(CASE
                        WHEN result->'$val'->'err'->>'compute-units' IS NOT NULL
                            THEN (result->'$val'->'err'->>'compute-units')::bigint
                        WHEN result->>'compute-units' IS NOT NULL
                            THEN (result->>'compute-units')::bigint
                        ELSE 0
                    END), 0)::bigint,
                    COALESCE(SUM(duration_ms), 0)::bigint
                    FROM task WHERE env_id = $1",
                )
                .bind(env_id)
                .fetch_one(pg_pool)
                .await?;
                Ok((row.0.unwrap_or(0), row.1.unwrap_or(0)))
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let row: (Option<i64>, Option<i64>) = sqlx::query_as(
                    "SELECT COALESCE(SUM(CASE
                        WHEN json_extract(result, '$.\"$val\".\"err\".\"compute-units\"') IS NOT NULL
                            THEN CAST(json_extract(result, '$.\"$val\".\"err\".\"compute-units\"') AS INTEGER)
                        WHEN json_extract(result, '$.\"compute-units\"') IS NOT NULL
                            THEN CAST(json_extract(result, '$.\"compute-units\"') AS INTEGER)
                        ELSE 0
                    END), 0),
                    COALESCE(SUM(duration_ms), 0)
                    FROM task WHERE env_id = ?",
                )
                .bind(env_id)
                .fetch_one(sqlite_pool)
                .await?;
                Ok((row.0.unwrap_or(0), row.1.unwrap_or(0)))
            }
        }
    }

    /// Get tasks by environment, ordered by creation time.
    pub async fn get_by_env(
        db: &crate::db::DatabasePool,
        env_id: &Uuid,
        limit: Option<i64>,
        offset: Option<i64>,
    ) -> Result<Vec<Task>, TaskError> {
        let limit = limit.unwrap_or(50);
        let offset = offset.unwrap_or(0);
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let q = format!(
                    "SELECT t.*, ts.name as status, {TASK_ORIGIN_FN_PG} \
                     FROM task t JOIN task_status ts ON t.task_status_id = ts.task_status_id \
                     {TASK_ORIGIN_JOIN} WHERE t.env_id = $1 ORDER BY t.created_at DESC LIMIT $2 OFFSET $3"
                );
                let tasks = sqlx::query_as::<_, Task>(sqlx::AssertSqlSafe(q.as_str()))
                    .bind(env_id)
                    .bind(limit)
                    .bind(offset)
                    .fetch_all(pg_pool)
                    .await?;
                Ok(tasks)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let q = format!(
                    "SELECT t.*, ts.name as status, {TASK_ORIGIN_FN_SQLITE} \
                     FROM task t JOIN task_status ts ON t.task_status_id = ts.task_status_id \
                     {TASK_ORIGIN_JOIN} WHERE t.env_id = ? ORDER BY t.created_at DESC LIMIT ? OFFSET ?"
                );
                let tasks = sqlx::query_as::<_, Task>(sqlx::AssertSqlSafe(q.as_str()))
                    .bind(env_id)
                    .bind(limit)
                    .bind(offset)
                    .fetch_all(sqlite_pool)
                    .await?;
                Ok(tasks)
            }
        }
    }

    /// Get filtered tasks by environment with pagination.
    ///
    /// `time_range_cutoff` filters to rows whose `created_at` is at or after
    /// the supplied UTC timestamp. Pass `None` for "all time". The handler
    /// is responsible for translating the user's ISO 8601 `time_range`
    /// query parameter into a cutoff via [`crate::time_range::parse_time_range_cutoff`].
    #[allow(clippy::too_many_arguments)]
    pub async fn get_filtered_by_env(
        db: &crate::db::DatabasePool,
        env_id: &Uuid,
        statuses: Option<&[&str]>,
        task_types: Option<&[&str]>,
        time_range_cutoff: Option<chrono::DateTime<chrono::Utc>>,
        search_term: Option<&str>,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<Task>, TaskError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let mut where_clauses = vec!["t.env_id = $1".to_string()];
                let mut param_count = 2;

                if time_range_cutoff.is_some() {
                    where_clauses.push(format!("t.created_at >= ${}", param_count));
                    param_count += 1;
                }

                let id_search = IdSearch::parse(search_term);

                if let Some(search) = &id_search
                    && search.uuid().is_some()
                {
                    let b = param_count;
                    where_clauses.push(format!(
                        "(t.task_id = ${} OR t.stream_id = ${} OR t.function_name ILIKE ${} OR t.task_type ILIKE ${} \
                         OR {TASK_ORIGIN_FN_EXPR_PG} ILIKE ${} OR CAST(t.args AS TEXT) ILIKE ${})",
                        b, b + 1, b + 2, b + 3, b + 4, b + 5
                    ));
                    param_count += 6;
                } else if let Some(search) = &id_search
                    && search.is_short_id()
                {
                    let b = param_count;
                    where_clauses.push(format!(
                        "(CAST(t.task_id AS TEXT) ILIKE ${} OR CAST(t.stream_id AS TEXT) ILIKE ${} OR t.function_name ILIKE ${} OR t.task_type ILIKE ${} \
                         OR {TASK_ORIGIN_FN_EXPR_PG} ILIKE ${} OR CAST(t.args AS TEXT) ILIKE ${})",
                        b, b + 1, b + 2, b + 3, b + 4, b + 5
                    ));
                    param_count += 6;
                } else if id_search.is_some() {
                    let b = param_count;
                    where_clauses.push(format!(
                        "(t.function_name ILIKE ${} OR t.task_type ILIKE ${} \
                         OR {TASK_ORIGIN_FN_EXPR_PG} ILIKE ${} OR CAST(t.args AS TEXT) ILIKE ${})",
                        b,
                        b + 1,
                        b + 2,
                        b + 3
                    ));
                    param_count += 4;
                }

                if let Some(statuses) = statuses
                    && !statuses.is_empty()
                {
                    let placeholders = pg_placeholders(param_count, statuses.len());
                    param_count += statuses.len();
                    where_clauses.push(format!("ts.name IN ({})", placeholders));
                }

                if let Some(task_types) = task_types
                    && !task_types.is_empty()
                {
                    let placeholders = pg_placeholders(param_count, task_types.len());
                    param_count += task_types.len();
                    where_clauses.push(format!("t.task_type IN ({})", placeholders));
                }

                let where_clause = where_clauses.join(" AND ");
                let query = format!(
                    "SELECT t.*, ts.name as status, {TASK_ORIGIN_FN_PG} FROM task t \
                     JOIN task_status ts ON t.task_status_id = ts.task_status_id \
                     {TASK_ORIGIN_JOIN} \
                     WHERE {} ORDER BY t.created_at DESC LIMIT ${} OFFSET ${}",
                    where_clause,
                    param_count,
                    param_count + 1
                );

                let mut q =
                    sqlx::query_as::<_, Task>(sqlx::AssertSqlSafe(query.as_str())).bind(env_id);

                if let Some(cutoff) = time_range_cutoff {
                    q = q.bind(cutoff);
                }

                if let Some(search) = &id_search
                    && let Some(uuid) = search.uuid()
                {
                    q = q.bind(uuid);
                    q = q.bind(uuid);
                    q = q.bind(search.text_pattern());
                    q = q.bind(search.text_pattern());
                    q = q.bind(search.text_pattern());
                    q = q.bind(search.text_pattern());
                } else if let Some(search) = &id_search
                    && let Some(suffix_pattern) = search.suffix_pattern()
                {
                    q = q.bind(suffix_pattern);
                    q = q.bind(suffix_pattern);
                    q = q.bind(search.text_pattern());
                    q = q.bind(search.text_pattern());
                    q = q.bind(search.text_pattern());
                    q = q.bind(search.text_pattern());
                } else if let Some(search) = &id_search {
                    q = q.bind(search.text_pattern());
                    q = q.bind(search.text_pattern());
                    q = q.bind(search.text_pattern());
                    q = q.bind(search.text_pattern());
                }

                if let Some(statuses) = statuses {
                    for s in statuses {
                        q = q.bind(*s);
                    }
                }
                if let Some(task_types) = task_types {
                    for tt in task_types {
                        q = q.bind(*tt);
                    }
                }

                q = q.bind(limit).bind(offset);
                let tasks = q.fetch_all(pg_pool).await?;
                Ok(tasks)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let mut where_clauses = vec!["t.env_id = ?".to_string()];
                let mut binds: Vec<String> = Vec::new();

                if let Some(cutoff) = time_range_cutoff {
                    where_clauses.push("t.created_at >= ?".to_string());
                    binds.push(cutoff.format("%Y-%m-%d %H:%M:%S").to_string());
                }

                let id_search = IdSearch::parse(search_term);

                if let Some(search) = &id_search
                    && let Some(uuid) = search.uuid()
                {
                    where_clauses.push(format!(
                        "(LOWER(HEX(t.task_id)) = ? OR LOWER(HEX(t.stream_id)) = ? OR t.function_name LIKE ? OR t.task_type LIKE ? \
                         OR {TASK_ORIGIN_FN_EXPR_SQLITE} LIKE ? OR CAST(t.args AS TEXT) LIKE ?)"
                    ));
                    let uuid_str = uuid.simple().to_string();
                    binds.extend([
                        uuid_str.clone(),
                        uuid_str,
                        search.text_pattern().to_string(),
                        search.text_pattern().to_string(),
                        search.text_pattern().to_string(),
                        search.text_pattern().to_string(),
                    ]);
                } else if let Some(search) = &id_search
                    && let Some(suffix) = search.suffix_pattern()
                {
                    where_clauses.push(format!(
                        "(LOWER(HEX(t.task_id)) LIKE ? OR LOWER(HEX(t.stream_id)) LIKE ? OR t.function_name LIKE ? OR t.task_type LIKE ? \
                         OR {TASK_ORIGIN_FN_EXPR_SQLITE} LIKE ? OR CAST(t.args AS TEXT) LIKE ?)"
                    ));
                    binds.extend([
                        suffix.to_string(),
                        suffix.to_string(),
                        search.text_pattern().to_string(),
                        search.text_pattern().to_string(),
                        search.text_pattern().to_string(),
                        search.text_pattern().to_string(),
                    ]);
                } else if let Some(search) = &id_search {
                    where_clauses.push(format!(
                        "(t.function_name LIKE ? OR t.task_type LIKE ? \
                         OR {TASK_ORIGIN_FN_EXPR_SQLITE} LIKE ? OR CAST(t.args AS TEXT) LIKE ?)"
                    ));
                    binds.extend([
                        search.text_pattern().to_string(),
                        search.text_pattern().to_string(),
                        search.text_pattern().to_string(),
                        search.text_pattern().to_string(),
                    ]);
                }

                if let Some(statuses) = statuses
                    && !statuses.is_empty()
                {
                    let placeholders = sqlite_placeholders(statuses.len());
                    where_clauses.push(format!("ts.name IN ({})", placeholders));
                    for s in statuses {
                        binds.push(s.to_string());
                    }
                }

                if let Some(task_types) = task_types
                    && !task_types.is_empty()
                {
                    let placeholders = sqlite_placeholders(task_types.len());
                    where_clauses.push(format!("t.task_type IN ({})", placeholders));
                    for tt in task_types {
                        binds.push(tt.to_string());
                    }
                }

                let where_clause = where_clauses.join(" AND ");
                let query = format!(
                    "SELECT t.*, ts.name as status, {TASK_ORIGIN_FN_SQLITE} FROM task t \
                     JOIN task_status ts ON t.task_status_id = ts.task_status_id \
                     {TASK_ORIGIN_JOIN} \
                     WHERE {} ORDER BY t.created_at DESC LIMIT ? OFFSET ?",
                    where_clause
                );

                let mut q =
                    sqlx::query_as::<_, Task>(sqlx::AssertSqlSafe(query.as_str())).bind(env_id);
                for b in &binds {
                    q = q.bind(b);
                }
                q = q.bind(limit).bind(offset);
                let tasks = q.fetch_all(sqlite_pool).await?;
                Ok(tasks)
            }
        }
    }

    /// Count filtered tasks by environment (for pagination).
    ///
    /// See [`Self::get_filtered_by_env`] for the meaning of `time_range_cutoff`.
    #[allow(clippy::too_many_arguments)]
    pub async fn get_filtered_count_by_env(
        db: &crate::db::DatabasePool,
        env_id: &Uuid,
        statuses: Option<&[&str]>,
        task_types: Option<&[&str]>,
        time_range_cutoff: Option<chrono::DateTime<chrono::Utc>>,
        search_term: Option<&str>,
    ) -> Result<i64, TaskError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let mut where_clauses = vec!["t.env_id = $1".to_string()];
                let mut param_count = 2;

                if time_range_cutoff.is_some() {
                    where_clauses.push(format!("t.created_at >= ${}", param_count));
                    param_count += 1;
                }

                let id_search = IdSearch::parse(search_term);

                if let Some(search) = &id_search
                    && search.uuid().is_some()
                {
                    let b = param_count;
                    where_clauses.push(format!(
                        "(t.task_id = ${} OR t.stream_id = ${} OR t.function_name ILIKE ${} OR t.task_type ILIKE ${} \
                         OR {TASK_ORIGIN_FN_EXPR_PG} ILIKE ${} OR CAST(t.args AS TEXT) ILIKE ${})",
                        b, b + 1, b + 2, b + 3, b + 4, b + 5
                    ));
                    param_count += 6;
                } else if let Some(search) = &id_search
                    && search.is_short_id()
                {
                    let b = param_count;
                    where_clauses.push(format!(
                        "(CAST(t.task_id AS TEXT) ILIKE ${} OR CAST(t.stream_id AS TEXT) ILIKE ${} OR t.function_name ILIKE ${} OR t.task_type ILIKE ${} \
                         OR {TASK_ORIGIN_FN_EXPR_PG} ILIKE ${} OR CAST(t.args AS TEXT) ILIKE ${})",
                        b, b + 1, b + 2, b + 3, b + 4, b + 5
                    ));
                    param_count += 6;
                } else if id_search.is_some() {
                    let b = param_count;
                    where_clauses.push(format!(
                        "(t.function_name ILIKE ${} OR t.task_type ILIKE ${} \
                         OR {TASK_ORIGIN_FN_EXPR_PG} ILIKE ${} OR CAST(t.args AS TEXT) ILIKE ${})",
                        b,
                        b + 1,
                        b + 2,
                        b + 3
                    ));
                    param_count += 4;
                }

                if let Some(statuses) = statuses
                    && !statuses.is_empty()
                {
                    let placeholders = pg_placeholders(param_count, statuses.len());
                    param_count += statuses.len();
                    where_clauses.push(format!("ts.name IN ({})", placeholders));
                }

                if let Some(task_types) = task_types
                    && !task_types.is_empty()
                {
                    let placeholders = pg_placeholders(param_count, task_types.len());
                    where_clauses.push(format!("t.task_type IN ({})", placeholders));
                }

                let where_clause = where_clauses.join(" AND ");
                let origin_join = if id_search.is_some() {
                    TASK_ORIGIN_JOIN
                } else {
                    ""
                };
                let query = format!(
                    "SELECT COUNT(*)::bigint FROM task t \
                     JOIN task_status ts ON t.task_status_id = ts.task_status_id \
                     {origin_join} \
                     WHERE {}",
                    where_clause
                );

                let mut q =
                    sqlx::query_as::<_, (i64,)>(sqlx::AssertSqlSafe(query.as_str())).bind(env_id);

                if let Some(cutoff) = time_range_cutoff {
                    q = q.bind(cutoff);
                }

                if let Some(search) = &id_search
                    && let Some(uuid) = search.uuid()
                {
                    q = q.bind(uuid);
                    q = q.bind(uuid);
                    q = q.bind(search.text_pattern());
                    q = q.bind(search.text_pattern());
                    q = q.bind(search.text_pattern());
                    q = q.bind(search.text_pattern());
                } else if let Some(search) = &id_search
                    && let Some(suffix_pattern) = search.suffix_pattern()
                {
                    q = q.bind(suffix_pattern);
                    q = q.bind(suffix_pattern);
                    q = q.bind(search.text_pattern());
                    q = q.bind(search.text_pattern());
                    q = q.bind(search.text_pattern());
                    q = q.bind(search.text_pattern());
                } else if let Some(search) = &id_search {
                    q = q.bind(search.text_pattern());
                    q = q.bind(search.text_pattern());
                    q = q.bind(search.text_pattern());
                    q = q.bind(search.text_pattern());
                }

                if let Some(statuses) = statuses {
                    for s in statuses {
                        q = q.bind(*s);
                    }
                }
                if let Some(task_types) = task_types {
                    for tt in task_types {
                        q = q.bind(*tt);
                    }
                }

                let row = q.fetch_one(pg_pool).await?;
                Ok(row.0)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let mut where_clauses = vec!["t.env_id = ?".to_string()];
                let mut binds: Vec<String> = Vec::new();

                if let Some(cutoff) = time_range_cutoff {
                    where_clauses.push("t.created_at >= ?".to_string());
                    binds.push(cutoff.format("%Y-%m-%d %H:%M:%S").to_string());
                }

                let id_search = IdSearch::parse(search_term);

                if let Some(search) = &id_search
                    && let Some(uuid) = search.uuid()
                {
                    where_clauses.push(format!(
                        "(LOWER(HEX(t.task_id)) = ? OR LOWER(HEX(t.stream_id)) = ? OR t.function_name LIKE ? OR t.task_type LIKE ? \
                         OR {TASK_ORIGIN_FN_EXPR_SQLITE} LIKE ? OR CAST(t.args AS TEXT) LIKE ?)"
                    ));
                    let uuid_str = uuid.simple().to_string();
                    binds.extend([
                        uuid_str.clone(),
                        uuid_str,
                        search.text_pattern().to_string(),
                        search.text_pattern().to_string(),
                        search.text_pattern().to_string(),
                        search.text_pattern().to_string(),
                    ]);
                } else if let Some(search) = &id_search
                    && let Some(suffix) = search.suffix_pattern()
                {
                    where_clauses.push(format!(
                        "(LOWER(HEX(t.task_id)) LIKE ? OR LOWER(HEX(t.stream_id)) LIKE ? OR t.function_name LIKE ? OR t.task_type LIKE ? \
                         OR {TASK_ORIGIN_FN_EXPR_SQLITE} LIKE ? OR CAST(t.args AS TEXT) LIKE ?)"
                    ));
                    binds.extend([
                        suffix.to_string(),
                        suffix.to_string(),
                        search.text_pattern().to_string(),
                        search.text_pattern().to_string(),
                        search.text_pattern().to_string(),
                        search.text_pattern().to_string(),
                    ]);
                } else if let Some(search) = &id_search {
                    where_clauses.push(format!(
                        "(t.function_name LIKE ? OR t.task_type LIKE ? \
                         OR {TASK_ORIGIN_FN_EXPR_SQLITE} LIKE ? OR CAST(t.args AS TEXT) LIKE ?)"
                    ));
                    binds.extend([
                        search.text_pattern().to_string(),
                        search.text_pattern().to_string(),
                        search.text_pattern().to_string(),
                        search.text_pattern().to_string(),
                    ]);
                }

                if let Some(statuses) = statuses
                    && !statuses.is_empty()
                {
                    let placeholders = sqlite_placeholders(statuses.len());
                    where_clauses.push(format!("ts.name IN ({})", placeholders));
                    for s in statuses {
                        binds.push(s.to_string());
                    }
                }

                if let Some(task_types) = task_types
                    && !task_types.is_empty()
                {
                    let placeholders = sqlite_placeholders(task_types.len());
                    where_clauses.push(format!("t.task_type IN ({})", placeholders));
                    for tt in task_types {
                        binds.push(tt.to_string());
                    }
                }

                let where_clause = where_clauses.join(" AND ");
                let origin_join = if id_search.is_some() {
                    TASK_ORIGIN_JOIN
                } else {
                    ""
                };
                let query = format!(
                    "SELECT COUNT(*) FROM task t \
                     JOIN task_status ts ON t.task_status_id = ts.task_status_id \
                     {origin_join} \
                     WHERE {}",
                    where_clause
                );

                let mut q =
                    sqlx::query_as::<_, (i64,)>(sqlx::AssertSqlSafe(query.as_str())).bind(env_id);
                for b in &binds {
                    q = q.bind(b);
                }
                let row = q.fetch_one(sqlite_pool).await?;
                Ok(row.0)
            }
        }
    }

    /// Set worker_id and initial heartbeat when a task starts running.
    pub async fn set_worker(
        db: &crate::db::DatabasePool,
        task_id: &Uuid,
        worker_id: &str,
    ) -> Result<(), TaskError> {
        let now = Utc::now();
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                sqlx::query(
                    "UPDATE task SET worker_id = $1, last_heartbeat_at = $2 WHERE task_id = $3",
                )
                .bind(worker_id)
                .bind(now)
                .bind(task_id)
                .execute(pg_pool)
                .await?;
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                sqlx::query(
                    "UPDATE task SET worker_id = ?, last_heartbeat_at = ? WHERE task_id = ?",
                )
                .bind(worker_id)
                .bind(now)
                .bind(task_id)
                .execute(sqlite_pool)
                .await?;
            }
        }
        Ok(())
    }

    /// Batch-update heartbeats for all tasks owned by this worker that are still running.
    pub async fn heartbeat(
        db: &crate::db::DatabasePool,
        worker_id: &str,
    ) -> Result<u64, TaskError> {
        let now = Utc::now();
        let running_id = TaskStatus::Running.as_id();
        let rows = match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                sqlx::query(
                    "UPDATE task SET last_heartbeat_at = $1 WHERE worker_id = $2 AND task_status_id = $3",
                )
                .bind(now)
                .bind(worker_id)
                .bind(running_id)
                .execute(pg_pool)
                .await?
                .rows_affected()
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                sqlx::query(
                    "UPDATE task SET last_heartbeat_at = ? WHERE worker_id = ? AND task_status_id = ?",
                )
                .bind(now)
                .bind(worker_id)
                .bind(running_id)
                .execute(sqlite_pool)
                .await?
                .rows_affected()
            }
        };
        Ok(rows)
    }

    /// Set the container_id for a container task (Phase 2 adoption).
    pub async fn set_container_id(
        db: &crate::db::DatabasePool,
        task_id: &Uuid,
        container_id: &str,
    ) -> Result<(), TaskError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                sqlx::query("UPDATE task SET container_id = $1 WHERE task_id = $2")
                    .bind(container_id)
                    .bind(task_id)
                    .execute(pg_pool)
                    .await?;
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                sqlx::query("UPDATE task SET container_id = ? WHERE task_id = ?")
                    .bind(container_id)
                    .bind(task_id)
                    .execute(sqlite_pool)
                    .await?;
            }
        }
        Ok(())
    }

    /// Find zombie tasks: running tasks whose heartbeat is older than the threshold.
    /// These are tasks whose worker crashed or was killed without graceful shutdown.
    pub async fn find_zombie_tasks(
        db: &crate::db::DatabasePool,
        stale_threshold_secs: i64,
    ) -> Result<Vec<Task>, TaskError> {
        let running_id = TaskStatus::Running.as_id();
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let tasks = sqlx::query_as::<_, Task>(
                    r#"SELECT t.*, ts.name as status
                       FROM task t
                       JOIN task_status ts ON t.task_status_id = ts.task_status_id
                       WHERE t.task_status_id = $1
                         AND t.last_heartbeat_at IS NOT NULL
                         AND t.last_heartbeat_at < NOW() - ($2 || ' seconds')::interval"#,
                )
                .bind(running_id)
                .bind(stale_threshold_secs.to_string())
                .fetch_all(pg_pool)
                .await?;
                Ok(tasks)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let tasks = sqlx::query_as::<_, Task>(
                    r#"SELECT t.*, ts.name as status
                       FROM task t
                       JOIN task_status ts ON t.task_status_id = ts.task_status_id
                       WHERE t.task_status_id = ?
                         AND t.last_heartbeat_at IS NOT NULL
                         AND t.last_heartbeat_at < datetime('now', '-' || ? || ' seconds')"#,
                )
                .bind(running_id)
                .bind(stale_threshold_secs.to_string())
                .fetch_all(sqlite_pool)
                .await?;
                Ok(tasks)
            }
        }
    }

    /// Save checkpoint data for a task (application-level state persistence).
    /// Stores serialized data in the `info` JSONB column.
    pub async fn set_checkpoint(
        db: &crate::db::DatabasePool,
        task_id: &Uuid,
        checkpoint: &serde_json::Value,
    ) -> Result<(), TaskError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                sqlx::query("UPDATE task SET info = $1 WHERE task_id = $2")
                    .bind(checkpoint)
                    .bind(task_id)
                    .execute(pg_pool)
                    .await?;
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let json_str = serde_json::to_string(checkpoint).unwrap_or_default();
                sqlx::query("UPDATE task SET info = ? WHERE task_id = ?")
                    .bind(json_str)
                    .bind(task_id)
                    .execute(sqlite_pool)
                    .await?;
            }
        }
        Ok(())
    }

    /// Retrieve checkpoint data for a task.
    pub async fn get_checkpoint(
        db: &crate::db::DatabasePool,
        task_id: &Uuid,
    ) -> Result<Option<serde_json::Value>, TaskError> {
        let task = Self::get(db, task_id).await?;
        Ok(task.info)
    }

    /// Find running tasks that have no heartbeat at all (legacy rows or pre-heartbeat tasks).
    /// These are tasks created before the heartbeat system was added.
    pub async fn find_running_without_heartbeat(
        db: &crate::db::DatabasePool,
    ) -> Result<Vec<Task>, TaskError> {
        let running_id = TaskStatus::Running.as_id();
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let tasks = sqlx::query_as::<_, Task>(
                    r#"SELECT t.*, ts.name as status
                       FROM task t
                       JOIN task_status ts ON t.task_status_id = ts.task_status_id
                       WHERE t.task_status_id = $1
                         AND t.last_heartbeat_at IS NULL
                         AND t.start_time < NOW() - interval '5 minutes'"#,
                )
                .bind(running_id)
                .fetch_all(pg_pool)
                .await?;
                Ok(tasks)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let tasks = sqlx::query_as::<_, Task>(
                    r#"SELECT t.*, ts.name as status
                       FROM task t
                       JOIN task_status ts ON t.task_status_id = ts.task_status_id
                       WHERE t.task_status_id = ?
                         AND t.last_heartbeat_at IS NULL
                         AND t.start_time < datetime('now', '-5 minutes')"#,
                )
                .bind(running_id)
                .fetch_all(sqlite_pool)
                .await?;
                Ok(tasks)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    fn test_ids() -> (Uuid, Uuid, Uuid, Uuid, Uuid, Uuid) {
        (
            Uuid::now_v7(), // task_id
            Uuid::now_v7(), // env_id
            Uuid::now_v7(), // stream_id
            Uuid::now_v7(), // build_id
            Uuid::now_v7(), // run_id
            Uuid::now_v7(), // user_id
        )
    }

    // -----------------------------------------------------------------------
    // TaskStatus conversion tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_task_status_as_id_round_trip() {
        let statuses = vec![
            (TaskStatus::Queued, 1, "queued"),
            (TaskStatus::Running, 2, "running"),
            (TaskStatus::Completed, 3, "completed"),
            (TaskStatus::Failed, 4, "failed"),
            (TaskStatus::Cancelled, 5, "cancelled"),
            (TaskStatus::TimedOut, 6, "timed_out"),
        ];
        for (status, expected_id, expected_str) in statuses {
            assert_eq!(status.as_id(), expected_id);
            assert_eq!(status.as_str(), expected_str);
            assert_eq!(TaskStatus::from_id(expected_id), Some(status.clone()));
            assert_eq!(format!("{}", status), expected_str);
            assert_eq!(TaskStatus::from_str(expected_str).unwrap(), status);
        }
    }

    #[test]
    fn test_task_status_from_id_invalid() {
        assert!(TaskStatus::from_id(0).is_none());
        assert!(TaskStatus::from_id(7).is_none());
        assert!(TaskStatus::from_id(-1).is_none());
    }

    #[test]
    fn test_task_status_from_str_invalid() {
        assert!(TaskStatus::from_str("bogus").is_err());
        assert!(TaskStatus::from_str("").is_err());
    }

    // -----------------------------------------------------------------------
    // CRUD tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_insert_and_get() {
        let db = crate::db::test_db().await;
        let (task_id, env_id, stream_id, build_id, run_id, user_id) = test_ids();
        let args = serde_json::json!({"key": "value"});

        Task::insert(
            &db,
            &task_id,
            &env_id,
            &stream_id,
            &build_id,
            Some(&run_id),
            "::app/my-task",
            Some(&args),
            None,
            "code",
            60_000,
            Some(&user_id),
        )
        .await
        .unwrap();

        let task = Task::get(&db, &task_id).await.unwrap();
        assert_eq!(task.task_id, task_id);
        assert_eq!(task.env_id, env_id);
        assert_eq!(task.stream_id, stream_id);
        assert_eq!(task.build_id, build_id);
        assert_eq!(task.origin_run_id, Some(run_id));
        assert_eq!(task.task_status_id, TaskStatus::Queued.as_id());
        assert_eq!(task.status, "queued");
        assert_eq!(task.function_name, "::app/my-task");
        assert_eq!(task.task_type, "code");
        assert_eq!(task.timeout_ms, 60_000);
        assert_eq!(task.by_user_id, Some(user_id));
        assert!(task.start_time.is_none());
        assert!(task.stop_time.is_none());
        assert!(task.duration_ms.is_none());
        assert!(task.result.is_none());
    }

    #[tokio::test]
    async fn test_insert_minimal() {
        let db = crate::db::test_db().await;
        let (task_id, env_id, stream_id, build_id, _, _) = test_ids();

        Task::insert(
            &db,
            &task_id,
            &env_id,
            &stream_id,
            &build_id,
            None,
            "::app/simple",
            None,
            None,
            "code",
            1_800_000,
            None,
        )
        .await
        .unwrap();

        let task = Task::get(&db, &task_id).await.unwrap();
        assert_eq!(task.origin_run_id, None);
        assert!(task.args.is_none());
        assert_eq!(task.by_user_id, None);
    }

    #[tokio::test]
    async fn test_get_stale_queued_throttles_by_next_retry_at() {
        let db = crate::db::test_db().await;
        let (task_id, env_id, stream_id, build_id, _, _) = test_ids();
        Task::insert(
            &db, &task_id, &env_id, &stream_id, &build_id, None, "::app/fn", None, None, "code",
            60_000, None,
        )
        .await
        .unwrap();

        let now = Utc::now();
        let stale_created_at = now - chrono::Duration::minutes(5);
        if let crate::db::DatabasePool::Sqlite(pool) = &db {
            sqlx::query("UPDATE task SET created_at = ? WHERE task_id = ?")
                .bind(stale_created_at)
                .bind(task_id)
                .execute(pool)
                .await
                .unwrap();
        }

        let stale = Task::get_stale_queued(&db, now - chrono::Duration::minutes(1), now, 10)
            .await
            .unwrap();
        assert_eq!(stale.len(), 1);
        assert_eq!(stale[0].task_id, task_id);

        let next_check_at = now + chrono::Duration::minutes(1);
        Task::defer_queued_reconcile(&db, &task_id, next_check_at)
            .await
            .unwrap();
        let throttled = Task::get_stale_queued(&db, now - chrono::Duration::minutes(1), now, 10)
            .await
            .unwrap();
        assert!(throttled.is_empty());

        let due_again = Task::get_stale_queued(
            &db,
            now - chrono::Duration::minutes(1),
            now + chrono::Duration::minutes(2),
            10,
        )
        .await
        .unwrap();
        assert_eq!(due_again.len(), 1);
        assert_eq!(due_again[0].task_id, task_id);
    }

    #[tokio::test]
    async fn test_mark_running() {
        let db = crate::db::test_db().await;
        let (task_id, env_id, stream_id, build_id, _, _) = test_ids();

        Task::insert(
            &db, &task_id, &env_id, &stream_id, &build_id, None, "::app/fn", None, None, "code",
            60_000, None,
        )
        .await
        .unwrap();

        Task::mark_running(&db, &task_id).await.unwrap();

        let task = Task::get(&db, &task_id).await.unwrap();
        assert_eq!(task.status, "running");
        assert_eq!(task.task_status_id, TaskStatus::Running.as_id());
        assert!(task.start_time.is_some());
    }

    #[tokio::test]
    async fn test_complete_success() {
        let db = crate::db::test_db().await;
        let (task_id, env_id, stream_id, build_id, _, _) = test_ids();

        Task::insert(
            &db, &task_id, &env_id, &stream_id, &build_id, None, "::app/fn", None, None, "code",
            60_000, None,
        )
        .await
        .unwrap();

        Task::mark_running(&db, &task_id).await.unwrap();

        // Small delay to ensure measurable duration
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        let result = serde_json::json!({"output": "done"});
        Task::complete(&db, &task_id, &TaskStatus::Completed, Some(&result))
            .await
            .unwrap();

        let task = Task::get(&db, &task_id).await.unwrap();
        assert_eq!(task.status, "completed");
        assert_eq!(task.task_status_id, TaskStatus::Completed.as_id());
        assert!(task.stop_time.is_some());
        assert!(task.duration_ms.is_some());
        assert!(task.duration_ms.unwrap() >= 0);
    }

    #[tokio::test]
    async fn test_complete_does_not_overwrite_terminal_status() {
        let db = crate::db::test_db().await;
        let (task_id, env_id, stream_id, build_id, _, _) = test_ids();

        Task::insert(
            &db, &task_id, &env_id, &stream_id, &build_id, None, "::app/fn", None, None, "code",
            60_000, None,
        )
        .await
        .unwrap();
        Task::mark_running(&db, &task_id).await.unwrap();

        let first = serde_json::json!({"output": "done"});
        Task::complete(&db, &task_id, &TaskStatus::Completed, Some(&first))
            .await
            .unwrap();

        let stale = serde_json::json!({"error": "late stale write"});
        Task::complete(&db, &task_id, &TaskStatus::Failed, Some(&stale))
            .await
            .unwrap();

        let task = Task::get(&db, &task_id).await.unwrap();
        assert_eq!(task.status, "completed");
        assert_eq!(task.task_status_id, TaskStatus::Completed.as_id());
        assert_eq!(task.result.unwrap(), first);
    }

    #[tokio::test]
    async fn test_complete_failed() {
        let db = crate::db::test_db().await;
        let (task_id, env_id, stream_id, build_id, _, _) = test_ids();

        Task::insert(
            &db, &task_id, &env_id, &stream_id, &build_id, None, "::app/fn", None, None, "code",
            60_000, None,
        )
        .await
        .unwrap();
        Task::mark_running(&db, &task_id).await.unwrap();

        let error = serde_json::json!({"error": "something broke"});
        Task::complete(&db, &task_id, &TaskStatus::Failed, Some(&error))
            .await
            .unwrap();

        let task = Task::get(&db, &task_id).await.unwrap();
        assert_eq!(task.status, "failed");
        assert!(task.stop_time.is_some());
    }

    #[tokio::test]
    async fn test_complete_timed_out() {
        let db = crate::db::test_db().await;
        let (task_id, env_id, stream_id, build_id, _, _) = test_ids();

        Task::insert(
            &db, &task_id, &env_id, &stream_id, &build_id, None, "::app/fn", None, None, "code",
            100, None,
        )
        .await
        .unwrap();
        Task::mark_running(&db, &task_id).await.unwrap();

        Task::complete(&db, &task_id, &TaskStatus::TimedOut, None)
            .await
            .unwrap();

        let task = Task::get(&db, &task_id).await.unwrap();
        assert_eq!(task.status, "timed_out");
        assert!(task.result.is_none());
    }

    #[tokio::test]
    async fn test_get_not_found() {
        let db = crate::db::test_db().await;
        let random_id = Uuid::now_v7();
        let result = Task::get(&db, &random_id).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_get_by_stream() {
        let db = crate::db::test_db().await;
        let env_id = Uuid::now_v7();
        let stream_a = Uuid::now_v7();
        let stream_b = Uuid::now_v7();
        let build_id = Uuid::now_v7();

        for i in 0..3 {
            let tid = Uuid::now_v7();
            Task::insert(
                &db,
                &tid,
                &env_id,
                &stream_a,
                &build_id,
                None,
                &format!("::app/task-{}", i),
                None,
                None,
                "code",
                60_000,
                None,
            )
            .await
            .unwrap();
        }

        let tid_b = Uuid::now_v7();
        Task::insert(
            &db,
            &tid_b,
            &env_id,
            &stream_b,
            &build_id,
            None,
            "::app/other",
            None,
            None,
            "code",
            60_000,
            None,
        )
        .await
        .unwrap();

        let tasks_a = Task::get_by_stream(&db, &stream_a, &env_id, None)
            .await
            .unwrap();
        assert_eq!(tasks_a.len(), 3);
        for t in &tasks_a {
            assert_eq!(t.stream_id, stream_a);
        }

        let tasks_b = Task::get_by_stream(&db, &stream_b, &env_id, None)
            .await
            .unwrap();
        assert_eq!(tasks_b.len(), 1);
        assert_eq!(tasks_b[0].stream_id, stream_b);
    }

    #[tokio::test]
    async fn test_get_by_stream_respects_limit() {
        let db = crate::db::test_db().await;
        let env_id = Uuid::now_v7();
        let stream_id = Uuid::now_v7();
        let build_id = Uuid::now_v7();

        for _ in 0..5 {
            let tid = Uuid::now_v7();
            Task::insert(
                &db, &tid, &env_id, &stream_id, &build_id, None, "::app/fn", None, None, "code",
                60_000, None,
            )
            .await
            .unwrap();
        }

        let tasks = Task::get_by_stream(&db, &stream_id, &env_id, Some(2))
            .await
            .unwrap();
        assert_eq!(tasks.len(), 2);
    }

    #[tokio::test]
    async fn test_get_by_env() {
        let db = crate::db::test_db().await;
        let env_a = Uuid::now_v7();
        let env_b = Uuid::now_v7();
        let build_id = Uuid::now_v7();

        for _ in 0..3 {
            let tid = Uuid::now_v7();
            let sid = Uuid::now_v7();
            Task::insert(
                &db, &tid, &env_a, &sid, &build_id, None, "::app/fn", None, None, "code", 60_000,
                None,
            )
            .await
            .unwrap();
        }

        let tid = Uuid::now_v7();
        let sid = Uuid::now_v7();
        Task::insert(
            &db, &tid, &env_b, &sid, &build_id, None, "::app/fn", None, None, "code", 60_000, None,
        )
        .await
        .unwrap();

        let tasks = Task::get_by_env(&db, &env_a, None, None).await.unwrap();
        assert_eq!(tasks.len(), 3);

        let tasks = Task::get_by_env(&db, &env_a, Some(2), None).await.unwrap();
        assert_eq!(tasks.len(), 2);

        let tasks = Task::get_by_env(&db, &env_a, Some(10), Some(2))
            .await
            .unwrap();
        assert_eq!(tasks.len(), 1);
    }

    // -----------------------------------------------------------------------
    // Worker tracking tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_set_worker() {
        let db = crate::db::test_db().await;
        let (task_id, env_id, stream_id, build_id, _, _) = test_ids();

        Task::insert(
            &db, &task_id, &env_id, &stream_id, &build_id, None, "::app/fn", None, None, "code",
            60_000, None,
        )
        .await
        .unwrap();

        Task::set_worker(&db, &task_id, "worker-abc").await.unwrap();

        let task = Task::get(&db, &task_id).await.unwrap();
        assert_eq!(task.worker_id.as_deref(), Some("worker-abc"));
        assert!(task.last_heartbeat_at.is_some());
    }

    #[tokio::test]
    async fn test_heartbeat() {
        let db = crate::db::test_db().await;
        let (task_id, env_id, stream_id, build_id, _, _) = test_ids();

        Task::insert(
            &db, &task_id, &env_id, &stream_id, &build_id, None, "::app/fn", None, None, "code",
            60_000, None,
        )
        .await
        .unwrap();

        Task::mark_running(&db, &task_id).await.unwrap();
        Task::set_worker(&db, &task_id, "worker-hb").await.unwrap();

        let before = Task::get(&db, &task_id).await.unwrap();
        let before_hb = before.last_heartbeat_at.unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let rows = Task::heartbeat(&db, "worker-hb").await.unwrap();
        assert_eq!(rows, 1);

        let after = Task::get(&db, &task_id).await.unwrap();
        assert!(after.last_heartbeat_at.unwrap() > before_hb);
    }

    #[tokio::test]
    async fn test_heartbeat_only_updates_running_tasks() {
        let db = crate::db::test_db().await;
        let (task_id, env_id, stream_id, build_id, _, _) = test_ids();

        Task::insert(
            &db, &task_id, &env_id, &stream_id, &build_id, None, "::app/fn", None, None, "code",
            60_000, None,
        )
        .await
        .unwrap();

        // Task is still queued, not running
        Task::set_worker(&db, &task_id, "worker-hb2").await.unwrap();

        let rows = Task::heartbeat(&db, "worker-hb2").await.unwrap();
        assert_eq!(rows, 0);
    }

    #[tokio::test]
    async fn test_set_container_id() {
        let db = crate::db::test_db().await;
        let (task_id, env_id, stream_id, build_id, _, _) = test_ids();

        Task::insert(
            &db,
            &task_id,
            &env_id,
            &stream_id,
            &build_id,
            None,
            "::app/fn",
            None,
            None,
            "container",
            60_000,
            None,
        )
        .await
        .unwrap();

        Task::set_container_id(&db, &task_id, "docker-abc123")
            .await
            .unwrap();

        let task = Task::get(&db, &task_id).await.unwrap();
        assert_eq!(task.container_id.as_deref(), Some("docker-abc123"));
    }

    // -----------------------------------------------------------------------
    // Checkpoint tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_checkpoint_and_restore() {
        let db = crate::db::test_db().await;
        let (task_id, env_id, stream_id, build_id, _, _) = test_ids();

        Task::insert(
            &db, &task_id, &env_id, &stream_id, &build_id, None, "::app/fn", None, None, "code",
            60_000, None,
        )
        .await
        .unwrap();

        // No checkpoint initially
        let checkpoint = Task::get_checkpoint(&db, &task_id).await.unwrap();
        assert!(checkpoint.is_none());

        // Save checkpoint
        let data = serde_json::json!({"offset": 500, "processed": 500});
        Task::set_checkpoint(&db, &task_id, &data).await.unwrap();

        // Restore checkpoint
        let restored = Task::get_checkpoint(&db, &task_id).await.unwrap();
        assert_eq!(restored.unwrap(), data);

        // Overwrite checkpoint
        let data2 = serde_json::json!({"offset": 1000, "processed": 1000});
        Task::set_checkpoint(&db, &task_id, &data2).await.unwrap();

        let restored2 = Task::get_checkpoint(&db, &task_id).await.unwrap();
        assert_eq!(restored2.unwrap(), data2);
    }

    // -----------------------------------------------------------------------
    // Zombie detection tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_find_zombie_tasks() {
        let db = crate::db::test_db().await;
        let (task_id, env_id, stream_id, build_id, _, _) = test_ids();

        Task::insert(
            &db, &task_id, &env_id, &stream_id, &build_id, None, "::app/fn", None, None, "code",
            60_000, None,
        )
        .await
        .unwrap();

        Task::mark_running(&db, &task_id).await.unwrap();
        Task::set_worker(&db, &task_id, "worker-z").await.unwrap();

        // With a very short threshold (0 seconds), the task should be stale
        // since its heartbeat was just set (but the query uses < NOW() - interval)
        // Use a large threshold to confirm it's NOT found
        let zombies = Task::find_zombie_tasks(&db, 9999).await.unwrap();
        assert!(
            zombies.iter().all(|t| t.task_id != task_id),
            "task with fresh heartbeat should not be a zombie"
        );
    }

    #[tokio::test]
    async fn test_find_running_without_heartbeat() {
        let db = crate::db::test_db().await;
        let (task_id, env_id, stream_id, build_id, _, _) = test_ids();

        Task::insert(
            &db, &task_id, &env_id, &stream_id, &build_id, None, "::app/fn", None, None, "code",
            60_000, None,
        )
        .await
        .unwrap();

        Task::mark_running(&db, &task_id).await.unwrap();
        // Don't set worker/heartbeat - simulates pre-heartbeat task

        // The query requires start_time < NOW() - 5 minutes, so a just-started
        // task won't appear. This confirms the method runs without errors.
        let orphans = Task::find_running_without_heartbeat(&db).await.unwrap();
        assert!(
            orphans.iter().all(|t| t.task_id != task_id),
            "just-started task should not appear (5-minute grace period)"
        );
    }

    // -----------------------------------------------------------------------
    // Concurrent-container quota tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_count_running_containers_for_org_excludes_stale() {
        let db = crate::db::test_db().await;
        let org_id = Uuid::now_v7();
        let env_id = Uuid::now_v7();
        let build_id = Uuid::now_v7();

        // Seed an env row that joins to the org so the WHERE clause matches.
        // FK enforcement is off in test_db so we can skip the parent org row.
        let env_id_str = env_id.to_string();
        let org_id_str = org_id.to_string();
        let pool = match &db {
            crate::db::DatabasePool::Sqlite(p) => p,
            _ => panic!("expected sqlite test pool"),
        };
        let _ = (env_id_str, org_id_str);
        sqlx::query(
            "INSERT INTO env (env_id, org_id, name, created_by_user_id) VALUES (?, ?, ?, ?)",
        )
        .bind(env_id)
        .bind(org_id)
        .bind("test")
        .bind(org_id)
        .execute(pool)
        .await
        .unwrap();

        // Helper: insert a `running` container task with a heartbeat at
        // `seconds_ago` seconds in the past.
        async fn insert_running_container(
            db: &crate::db::DatabasePool,
            env_id: &Uuid,
            build_id: &Uuid,
            seconds_ago: i64,
        ) -> Uuid {
            let task_id = Uuid::now_v7();
            let stream_id = Uuid::now_v7();
            Task::insert(
                db,
                &task_id,
                env_id,
                &stream_id,
                build_id,
                None,
                "::hot::box/start",
                None,
                None,
                "container",
                60_000,
                None,
            )
            .await
            .unwrap();
            Task::mark_running(db, &task_id).await.unwrap();

            // Backdate the heartbeat AND start_time so the row counts as
            // "stale" by both the heartbeat clause and the start-time
            // grace-period clause in `count_running_containers_for_org`.
            let pool = match db {
                crate::db::DatabasePool::Sqlite(p) => p,
                _ => panic!("expected sqlite test pool"),
            };
            let stamp = format!("-{} seconds", seconds_ago);
            sqlx::query(
                "UPDATE task
                   SET last_heartbeat_at = datetime('now', ?),
                       start_time        = datetime('now', ?)
                 WHERE task_id = ?",
            )
            .bind(&stamp)
            .bind(&stamp)
            .bind(task_id)
            .execute(pool)
            .await
            .unwrap();
            task_id
        }

        // 2 fresh (heartbeat 5s ago) + 3 stale (heartbeat 10 minutes ago)
        for _ in 0..2 {
            insert_running_container(&db, &env_id, &build_id, 5).await;
        }
        for _ in 0..3 {
            insert_running_container(&db, &env_id, &build_id, 600).await;
        }

        // Unfiltered: all 5 are counted.
        let unfiltered = Task::count_running_containers_for_org_unfiltered(&db, &org_id)
            .await
            .unwrap();
        assert_eq!(unfiltered, 5, "raw row count should include zombies");

        // 120s threshold: only the 2 fresh-heartbeat rows survive.
        let fresh = Task::count_running_containers_for_org(&db, &org_id, 120)
            .await
            .unwrap();
        assert_eq!(fresh, 2, "stale rows must not consume the org's quota");

        // Threshold of 0 = disabled, so it must equal the unfiltered count.
        let disabled = Task::count_running_containers_for_org(&db, &org_id, 0)
            .await
            .unwrap();
        assert_eq!(disabled, unfiltered);
    }
}

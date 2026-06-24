use ahash::AHashMap;
use chrono::{DateTime, Utc};
use sqlx::{FromRow, Row};
use std::str::FromStr;
use thiserror::Error;
use uuid::Uuid;

use super::search::{IdSearch, pg_placeholders, sqlite_placeholders};

#[derive(Error, Debug)]
pub enum RunError {
    #[error("Database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("Run not found")]
    NotFound,
}

#[derive(Error, Debug)]
#[error("Invalid run status: {0}")]
pub struct RunStatusParseError(String);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunStatus {
    Running,
    Succeeded,
    Failed,
    Cancelled,
    PendingRetry,
}

impl RunStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            RunStatus::Running => "running",
            RunStatus::Succeeded => "succeeded",
            RunStatus::Failed => "failed",
            RunStatus::Cancelled => "cancelled",
            RunStatus::PendingRetry => "pending_retry",
        }
    }

    pub fn as_id(&self) -> i16 {
        match self {
            RunStatus::Running => 1,
            RunStatus::Succeeded => 2,
            RunStatus::Failed => 3,
            RunStatus::Cancelled => 4,
            RunStatus::PendingRetry => 5,
        }
    }

    pub fn from_id(id: i16) -> Option<Self> {
        match id {
            1 => Some(RunStatus::Running),
            2 => Some(RunStatus::Succeeded),
            3 => Some(RunStatus::Failed),
            4 => Some(RunStatus::Cancelled),
            5 => Some(RunStatus::PendingRetry),
            _ => None,
        }
    }
}

impl std::fmt::Display for RunStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

impl FromStr for RunStatus {
    type Err = RunStatusParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "running" => Ok(RunStatus::Running),
            "succeeded" => Ok(RunStatus::Succeeded),
            "failed" => Ok(RunStatus::Failed),
            "cancelled" => Ok(RunStatus::Cancelled),
            "pending_retry" => Ok(RunStatus::PendingRetry),
            _ => Err(RunStatusParseError(s.to_string())),
        }
    }
}

#[derive(Debug, FromRow)]
pub struct RunStatusRecord {
    pub status_id: i16,
    pub status: String,
    pub sort_order: i16,
}

#[derive(Debug)]
pub struct RunTypeParseError(String);

impl std::fmt::Display for RunTypeParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Invalid run type: {}", self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunType {
    Call,
    Event,
    Schedule,
    Run,
    Eval,
    Repl,
    Task,
}

impl RunType {
    pub fn as_str(&self) -> &'static str {
        match self {
            RunType::Call => "call",
            RunType::Event => "event",
            RunType::Schedule => "schedule",
            RunType::Run => "run",
            RunType::Eval => "eval",
            RunType::Repl => "repl",
            RunType::Task => "task",
        }
    }

    pub fn as_id(&self) -> i16 {
        match self {
            RunType::Call => 1,
            RunType::Event => 2,
            RunType::Schedule => 3,
            RunType::Run => 4,
            RunType::Eval => 5,
            RunType::Repl => 6,
            RunType::Task => 7,
        }
    }

    pub fn from_id(id: i16) -> Option<Self> {
        match id {
            1 => Some(RunType::Call),
            2 => Some(RunType::Event),
            3 => Some(RunType::Schedule),
            4 => Some(RunType::Run),
            5 => Some(RunType::Eval),
            6 => Some(RunType::Repl),
            7 => Some(RunType::Task),
            _ => None,
        }
    }
}

impl std::fmt::Display for RunType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

impl FromStr for RunType {
    type Err = RunTypeParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "call" => Ok(RunType::Call),
            "event" => Ok(RunType::Event),
            "schedule" => Ok(RunType::Schedule),
            "run" => Ok(RunType::Run),
            "eval" => Ok(RunType::Eval),
            "repl" => Ok(RunType::Repl),
            "task" => Ok(RunType::Task),
            _ => Err(RunTypeParseError(s.to_string())),
        }
    }
}

#[derive(Debug, FromRow)]
pub struct RunTypeRecord {
    pub run_type_id: i16,
    pub run_type: String,
    pub sort_order: i16,
}

#[derive(Debug, FromRow)]
pub struct Run {
    pub run_id: Uuid,
    pub env_id: Uuid,
    pub stream_id: Uuid,
    pub build_id: Option<Uuid>,
    pub run_type_id: i16,
    pub run_type: String,
    pub origin_run_id: Option<Uuid>,
    pub event_id: Option<Uuid>,
    pub start_time: DateTime<Utc>,
    pub stop_time: Option<DateTime<Utc>>,
    pub status_id: i16,
    pub status: String,
    pub by_user_id: Option<Uuid>,
    pub result: Option<serde_json::Value>, // JSON: Failure type for failed runs, return values for successful runs
    pub info: Option<serde_json::Value>, // JSON: Execution info - warnings, routing decisions, diagnostics (null when empty)
    pub project_id: Option<Uuid>,
    pub project_name: Option<String>,
    pub event_fn: Option<String>, // Function name from event data (hot:schedule/hot:call) or root call (event handlers)
    // Retry state fields (config is read from handler/schedule meta)
    pub retry_attempt: i16, // Current retry attempt (0 = first try)
    pub next_retry_at: Option<DateTime<Utc>>, // When to retry next (null = no pending retry)
    // Queue timing - when the event was enqueued (for calculating queue wait time)
    pub queued_at: Option<DateTime<Utc>>, // From event.created_at - null for non-event runs
    // Access attribution - links to the access audit record for API-initiated runs
    #[sqlx(default)]
    pub access_id: Option<Uuid>,
    // Agent identity — qualified type name when run was produced by an agent handler
    #[sqlx(default)]
    pub agent_type: Option<String>,
}

impl Run {
    /// Get run by ID with status information
    pub async fn get_run_with_status(
        db: &crate::db::DatabasePool,
        run_id: &Uuid,
    ) -> Result<Run, RunError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let run = sqlx::query_as::<_, Run>(
                    "SELECT r.run_id, r.env_id, r.stream_id, r.build_id, r.run_type_id, rt.run_type, r.origin_run_id, r.event_id, r.start_time, r.stop_time, r.status_id, rs.status, r.by_user_id, r.result, r.info, r.retry_attempt, r.next_retry_at, b.project_id, p.name as project_name, COALESCE(e.event_data->>'fn', (SELECT function_name FROM call WHERE run_id = r.run_id AND parent_call_id IS NULL LIMIT 1), (SELECT function_name FROM task WHERE run_id = r.run_id LIMIT 1)) as event_fn, e.created_at as queued_at, r.access_id FROM run r JOIN run_status rs ON r.status_id = rs.status_id JOIN run_type rt ON r.run_type_id = rt.run_type_id LEFT JOIN build b ON r.build_id = b.build_id LEFT JOIN project p ON b.project_id = p.project_id LEFT JOIN event e ON r.event_id = e.event_id WHERE r.run_id = $1"
                )
                .bind(run_id)
                .fetch_one(pg_pool)
                .await?;
                Ok(run)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let run = sqlx::query_as::<_, Run>(
                    "SELECT r.run_id, r.env_id, r.stream_id, r.build_id, r.run_type_id, rt.run_type, r.origin_run_id, r.event_id, r.start_time, r.stop_time, r.status_id, rs.status, r.by_user_id, r.result, r.info, r.retry_attempt, r.next_retry_at, b.project_id, p.name as project_name, COALESCE(json_extract(e.event_data, '$.fn'), (SELECT function_name FROM call WHERE run_id = r.run_id AND parent_call_id IS NULL LIMIT 1), (SELECT function_name FROM task WHERE run_id = r.run_id LIMIT 1)) as event_fn, e.created_at as queued_at, r.access_id FROM run r JOIN run_status rs ON r.status_id = rs.status_id JOIN run_type rt ON r.run_type_id = rt.run_type_id LEFT JOIN build b ON r.build_id = b.build_id LEFT JOIN project p ON b.project_id = p.project_id LEFT JOIN event e ON r.event_id = e.event_id WHERE r.run_id = ?"
                )
                .bind(run_id)
                .fetch_one(sqlite_pool)
                .await?;
                Ok(run)
            }
        }
    }

    /// Get run by ID
    pub async fn get_run(db: &crate::db::DatabasePool, run_id: &Uuid) -> Result<Run, RunError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let run = sqlx::query_as::<_, Run>(
                    "SELECT r.run_id, r.env_id, r.stream_id, r.build_id, r.run_type_id, rt.run_type, r.origin_run_id, r.event_id, r.start_time, r.stop_time, r.status_id, rs.status, r.by_user_id, r.result, r.info, r.retry_attempt, r.next_retry_at, b.project_id, p.name as project_name, COALESCE(e.event_data->>'fn', (SELECT function_name FROM call WHERE run_id = r.run_id AND parent_call_id IS NULL LIMIT 1), (SELECT function_name FROM task WHERE run_id = r.run_id LIMIT 1)) as event_fn, e.created_at as queued_at, r.access_id FROM run r JOIN run_status rs ON r.status_id = rs.status_id JOIN run_type rt ON r.run_type_id = rt.run_type_id LEFT JOIN build b ON r.build_id = b.build_id LEFT JOIN project p ON b.project_id = p.project_id LEFT JOIN event e ON r.event_id = e.event_id WHERE r.run_id = $1"
                )
                .bind(run_id)
                .fetch_optional(pg_pool)
                .await
                .inspect_err(|e| tracing::error!("db error in Run::get_run: {}", e))?
                .ok_or(RunError::NotFound)?;
                Ok(run)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let run = sqlx::query_as::<_, Run>(
                    "SELECT r.run_id, r.env_id, r.stream_id, r.build_id, r.run_type_id, rt.run_type, r.origin_run_id, r.event_id, r.start_time, r.stop_time, r.status_id, rs.status, r.by_user_id, r.result, r.info, r.retry_attempt, r.next_retry_at, b.project_id, p.name as project_name, COALESCE(json_extract(e.event_data, '$.fn'), (SELECT function_name FROM call WHERE run_id = r.run_id AND parent_call_id IS NULL LIMIT 1), (SELECT function_name FROM task WHERE run_id = r.run_id LIMIT 1)) as event_fn, e.created_at as queued_at, r.access_id FROM run r JOIN run_status rs ON r.status_id = rs.status_id JOIN run_type rt ON r.run_type_id = rt.run_type_id LEFT JOIN build b ON r.build_id = b.build_id LEFT JOIN project p ON b.project_id = p.project_id LEFT JOIN event e ON r.event_id = e.event_id WHERE r.run_id = ?"
                )
                .bind(run_id)
                .fetch_optional(sqlite_pool)
                .await
                .inspect_err(|e| tracing::error!("db error in Run::get_run: {}", e))?
                .ok_or(RunError::NotFound)?;
                Ok(run)
            }
        }
    }

    /// Get count of runs
    pub async fn get_count(db: &crate::db::DatabasePool) -> Result<i64, RunError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM run")
                    .fetch_one(pg_pool)
                    .await?;
                Ok(count)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM run")
                    .fetch_one(sqlite_pool)
                    .await?;
                Ok(count)
            }
        }
    }

    /// Get count of runs by environment
    pub async fn get_count_by_env(
        db: &crate::db::DatabasePool,
        env_id: &Uuid,
    ) -> Result<i64, RunError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let count: i64 = sqlx::query_scalar(
                    "SELECT COUNT(*) FROM run WHERE env_id = $1 AND run_type_id != 7",
                )
                .bind(env_id)
                .fetch_one(pg_pool)
                .await?;
                Ok(count)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let count: i64 = sqlx::query_scalar(
                    "SELECT COUNT(*) FROM run WHERE env_id = ? AND run_type_id != 7",
                )
                .bind(env_id)
                .fetch_one(sqlite_pool)
                .await?;
                Ok(count)
            }
        }
    }

    /// Get count of runs by status and environment
    pub async fn get_count_by_status_and_env(
        db: &crate::db::DatabasePool,
        status: &RunStatus,
        env_id: &Uuid,
    ) -> Result<i64, RunError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let count: i64 = sqlx::query_scalar(
                    "SELECT COUNT(*) FROM run WHERE status_id = $1 AND env_id = $2 AND run_type_id != 7",
                )
                .bind(status.as_id())
                .bind(env_id)
                .fetch_one(pg_pool)
                .await?;
                Ok(count)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let count: i64 = sqlx::query_scalar(
                    "SELECT COUNT(*) FROM run WHERE status_id = ? AND env_id = ? AND run_type_id != 7",
                )
                .bind(status.as_id())
                .bind(env_id)
                .fetch_one(sqlite_pool)
                .await?;
                Ok(count)
            }
        }
    }

    /// Get all status counts for an environment in a single query.
    /// Returns (total, running, succeeded, failed, cancelled).
    pub async fn get_all_status_counts_by_env(
        db: &crate::db::DatabasePool,
        env_id: &Uuid,
    ) -> Result<(i64, i64, i64, i64, i64), RunError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let rows: Vec<(i16, i64)> = sqlx::query_as(
                    "SELECT status_id, COUNT(*) FROM run WHERE env_id = $1 AND run_type_id != 7 GROUP BY status_id",
                )
                .bind(env_id)
                .fetch_all(pg_pool)
                .await?;

                let mut running = 0i64;
                let mut succeeded = 0i64;
                let mut failed = 0i64;
                let mut cancelled = 0i64;
                let mut total = 0i64;
                for (status_id, count) in &rows {
                    total += count;
                    match RunStatus::from_id(*status_id) {
                        Some(RunStatus::Running) => running = *count,
                        Some(RunStatus::Succeeded) => succeeded = *count,
                        Some(RunStatus::Failed) => failed = *count,
                        Some(RunStatus::Cancelled) => cancelled = *count,
                        _ => {}
                    }
                }
                Ok((total, running, succeeded, failed, cancelled))
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let rows: Vec<(i16, i64)> = sqlx::query_as(
                    "SELECT status_id, COUNT(*) FROM run WHERE env_id = ? AND run_type_id != 7 GROUP BY status_id",
                )
                .bind(env_id)
                .fetch_all(sqlite_pool)
                .await?;

                let mut running = 0i64;
                let mut succeeded = 0i64;
                let mut failed = 0i64;
                let mut cancelled = 0i64;
                let mut total = 0i64;
                for (status_id, count) in &rows {
                    total += count;
                    match RunStatus::from_id(*status_id) {
                        Some(RunStatus::Running) => running = *count,
                        Some(RunStatus::Succeeded) => succeeded = *count,
                        Some(RunStatus::Failed) => failed = *count,
                        Some(RunStatus::Cancelled) => cancelled = *count,
                        _ => {}
                    }
                }
                Ok((total, running, succeeded, failed, cancelled))
            }
        }
    }

    /// Get count of runs by status, environment, and time range
    pub async fn get_count_by_status_time_and_env(
        db: &crate::db::DatabasePool,
        status: &RunStatus,
        env_id: &Uuid,
        start_time: DateTime<Utc>,
    ) -> Result<i64, RunError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let count: i64 = sqlx::query_scalar(
                    "SELECT COUNT(*) FROM run WHERE status_id = $1 AND env_id = $2 AND start_time >= $3 AND run_type_id != 7",
                )
                .bind(status.as_id())
                .bind(env_id)
                .bind(start_time)
                .fetch_one(pg_pool)
                .await?;
                Ok(count)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let count: i64 = sqlx::query_scalar(
                    "SELECT COUNT(*) FROM run WHERE status_id = ? AND env_id = ? AND start_time >= ? AND run_type_id != 7",
                )
                .bind(status.as_id())
                .bind(env_id)
                .bind(start_time)
                .fetch_one(sqlite_pool)
                .await?;
                Ok(count)
            }
        }
    }

    /// Get count of runs by run type, environment, and time range
    pub async fn get_count_by_type_time_and_env(
        db: &crate::db::DatabasePool,
        run_type: &RunType,
        env_id: &Uuid,
        start_time: DateTime<Utc>,
    ) -> Result<i64, RunError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let count: i64 = sqlx::query_scalar(
                    "SELECT COUNT(*) FROM run WHERE run_type_id = $1 AND env_id = $2 AND start_time >= $3",
                )
                .bind(run_type.as_id())
                .bind(env_id)
                .bind(start_time)
                .fetch_one(pg_pool)
                .await?;
                Ok(count)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let count: i64 = sqlx::query_scalar(
                    "SELECT COUNT(*) FROM run WHERE run_type_id = ? AND env_id = ? AND start_time >= ?",
                )
                .bind(run_type.as_id())
                .bind(env_id)
                .bind(start_time)
                .fetch_one(sqlite_pool)
                .await?;
                Ok(count)
            }
        }
    }

    /// Get count of runs by status, environment, time range, AND filtered by run types
    pub async fn get_count_by_status_time_env_and_types(
        db: &crate::db::DatabasePool,
        status: &RunStatus,
        env_id: &Uuid,
        start_time: DateTime<Utc>,
        run_types: &[&str],
    ) -> Result<i64, RunError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let run_type_placeholders = (1..=run_types.len())
                    .map(|i| format!("${}", i + 3))
                    .collect::<Vec<String>>()
                    .join(", ");

                let query = format!(
                    "SELECT COUNT(*) FROM run r JOIN run_type rt ON r.run_type_id = rt.run_type_id WHERE r.status_id = $1 AND r.env_id = $2 AND r.start_time >= $3 AND rt.run_type IN ({})",
                    run_type_placeholders
                );

                let mut q = sqlx::query_scalar::<_, i64>(sqlx::AssertSqlSafe(query.as_str()))
                    .bind(status.as_id())
                    .bind(env_id)
                    .bind(start_time);

                for run_type in run_types {
                    q = q.bind(*run_type);
                }

                let count = q.fetch_one(pg_pool).await?;
                Ok(count)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let run_type_placeholders = (0..run_types.len())
                    .map(|_| "?")
                    .collect::<Vec<&str>>()
                    .join(", ");

                let query = format!(
                    "SELECT COUNT(*) FROM run r JOIN run_type rt ON r.run_type_id = rt.run_type_id WHERE r.status_id = ? AND r.env_id = ? AND r.start_time >= ? AND rt.run_type IN ({})",
                    run_type_placeholders
                );

                let mut q = sqlx::query_scalar::<_, i64>(sqlx::AssertSqlSafe(query.as_str()))
                    .bind(status.as_id())
                    .bind(env_id)
                    .bind(start_time);

                for run_type in run_types {
                    q = q.bind(*run_type);
                }

                let count = q.fetch_one(sqlite_pool).await?;
                Ok(count)
            }
        }
    }

    /// Get count of runs by run type, environment, time range, AND filtered by statuses
    pub async fn get_count_by_type_time_env_and_statuses(
        db: &crate::db::DatabasePool,
        run_type: &RunType,
        env_id: &Uuid,
        start_time: DateTime<Utc>,
        statuses: &[&str],
    ) -> Result<i64, RunError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let status_placeholders = (1..=statuses.len())
                    .map(|i| format!("${}", i + 3))
                    .collect::<Vec<_>>()
                    .join(", ");

                let query = format!(
                    "SELECT COUNT(*) FROM run r JOIN run_status rs ON r.status_id = rs.status_id WHERE r.run_type_id = $1 AND r.env_id = $2 AND r.start_time >= $3 AND rs.status IN ({})",
                    status_placeholders
                );

                let mut q = sqlx::query_scalar::<_, i64>(sqlx::AssertSqlSafe(query.as_str()))
                    .bind(run_type.as_id())
                    .bind(env_id)
                    .bind(start_time);

                for status in statuses {
                    q = q.bind(*status);
                }

                let count = q.fetch_one(pg_pool).await?;
                Ok(count)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let status_placeholders = (0..statuses.len())
                    .map(|_| "?")
                    .collect::<Vec<_>>()
                    .join(", ");

                let query = format!(
                    "SELECT COUNT(*) FROM run r JOIN run_status rs ON r.status_id = rs.status_id WHERE r.run_type_id = ? AND r.env_id = ? AND r.start_time >= ? AND rs.status IN ({})",
                    status_placeholders
                );

                let mut q = sqlx::query_scalar::<_, i64>(sqlx::AssertSqlSafe(query.as_str()))
                    .bind(run_type.as_id())
                    .bind(env_id)
                    .bind(start_time);

                for status in statuses {
                    q = q.bind(*status);
                }

                let count = q.fetch_one(sqlite_pool).await?;
                Ok(count)
            }
        }
    }

    /// Get runs by environment ID with status information
    pub async fn get_runs_by_env_with_status(
        db: &crate::db::DatabasePool,
        env_id: &Uuid,
        limit: Option<i64>,
        offset: Option<i64>,
    ) -> Result<Vec<Run>, RunError> {
        let limit = limit.unwrap_or(10);
        let offset = offset.unwrap_or(0);

        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let runs = sqlx::query_as::<_, Run>(
                    "SELECT r.run_id, r.env_id, r.stream_id, r.build_id, r.run_type_id, rt.run_type, r.origin_run_id, r.event_id, r.start_time, r.stop_time, r.status_id, rs.status, r.by_user_id, r.result, r.info, r.retry_attempt, r.next_retry_at FROM run r JOIN run_status rs ON r.status_id = rs.status_id JOIN run_type rt ON r.run_type_id = rt.run_type_id WHERE r.env_id = $1 ORDER BY r.start_time DESC LIMIT $2 OFFSET $3"
                )
                .bind(env_id)
                .bind(limit)
                .bind(offset)
                .fetch_all(pg_pool)
                .await?;
                Ok(runs)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let runs = sqlx::query_as::<_, Run>(
                    "SELECT r.run_id, r.env_id, r.stream_id, r.build_id, r.run_type_id, rt.run_type, r.origin_run_id, r.event_id, r.start_time, r.stop_time, r.status_id, rs.status, r.by_user_id, r.result, r.info, r.retry_attempt, r.next_retry_at FROM run r JOIN run_status rs ON r.status_id = rs.status_id JOIN run_type rt ON r.run_type_id = rt.run_type_id WHERE r.env_id = ? ORDER BY r.start_time DESC LIMIT ? OFFSET ?"
                )
                .bind(env_id)
                .bind(limit)
                .bind(offset)
                .fetch_all(sqlite_pool)
                .await?;
                Ok(runs)
            }
        }
    }

    /// Get runs by environment ID
    pub async fn get_runs_by_env(
        db: &crate::db::DatabasePool,
        env_id: &Uuid,
        limit: Option<i64>,
        offset: Option<i64>,
    ) -> Result<Vec<Run>, RunError> {
        let limit = limit.unwrap_or(10);
        let offset = offset.unwrap_or(0);

        tracing::info!(
            "🔍 get_runs_by_env: env_id={}, limit={}, offset={}",
            env_id,
            limit,
            offset
        );

        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let runs = sqlx::query_as::<_, Run>(
                    "SELECT r.run_id, r.env_id, r.stream_id, r.build_id, r.run_type_id, rt.run_type, r.origin_run_id, r.event_id, r.start_time, r.stop_time, r.status_id, rs.status, r.by_user_id, r.result, r.info, r.retry_attempt, r.next_retry_at, b.project_id, p.name as project_name, COALESCE(e.event_data->>'fn', (SELECT function_name FROM call WHERE run_id = r.run_id AND parent_call_id IS NULL LIMIT 1), (SELECT function_name FROM task WHERE run_id = r.run_id LIMIT 1)) as event_fn, e.created_at as queued_at FROM run r JOIN run_status rs ON r.status_id = rs.status_id JOIN run_type rt ON r.run_type_id = rt.run_type_id LEFT JOIN build b ON r.build_id = b.build_id LEFT JOIN project p ON b.project_id = p.project_id LEFT JOIN event e ON r.event_id = e.event_id WHERE r.env_id = $1 AND r.run_type_id != 7 ORDER BY r.start_time DESC LIMIT $2 OFFSET $3"
                )
                .bind(env_id)
                .bind(limit)
                .bind(offset)
                .fetch_all(pg_pool)
                .await?;
                Ok(runs)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let result = sqlx::query_as::<_, Run>(
                    "SELECT r.run_id, r.env_id, r.stream_id, r.build_id, r.run_type_id, rt.run_type, r.origin_run_id, r.event_id, r.start_time, r.stop_time, r.status_id, rs.status, r.by_user_id, r.result, r.info, r.retry_attempt, r.next_retry_at, b.project_id, p.name as project_name, COALESCE(json_extract(e.event_data, '$.fn'), (SELECT function_name FROM call WHERE run_id = r.run_id AND parent_call_id IS NULL LIMIT 1), (SELECT function_name FROM task WHERE run_id = r.run_id LIMIT 1)) as event_fn, e.created_at as queued_at FROM run r JOIN run_status rs ON r.status_id = rs.status_id JOIN run_type rt ON r.run_type_id = rt.run_type_id LEFT JOIN build b ON r.build_id = b.build_id LEFT JOIN project p ON b.project_id = p.project_id LEFT JOIN event e ON r.event_id = e.event_id WHERE r.env_id = ? AND r.run_type_id != 7 ORDER BY r.start_time DESC LIMIT ? OFFSET ?"
                )
                .bind(env_id)
                .bind(limit)
                .bind(offset)
                .fetch_all(sqlite_pool)
                .await;

                match result {
                    Ok(runs) => {
                        tracing::debug!(
                            "get_runs_by_env: returned {} runs from SQLite",
                            runs.len()
                        );
                        Ok(runs)
                    }
                    Err(e) => {
                        tracing::error!("get_runs_by_env: SQLite query error: {}", e);
                        Err(RunError::Database(e))
                    }
                }
            }
        }
    }

    /// Get runs by user ID
    pub async fn get_runs_by_user(
        db: &crate::db::DatabasePool,
        user_id: &Uuid,
        limit: Option<i64>,
        offset: Option<i64>,
    ) -> Result<Vec<Run>, RunError> {
        let limit = limit.unwrap_or(10);
        let offset = offset.unwrap_or(0);

        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let runs = sqlx::query_as::<_, Run>(
                    "SELECT r.run_id, r.env_id, r.stream_id, r.build_id, r.run_type_id, rt.run_type, r.origin_run_id, r.event_id, r.start_time, r.stop_time, r.status_id, rs.status, r.by_user_id, r.result, r.info, r.retry_attempt, r.next_retry_at, b.project_id, p.name as project_name FROM run r JOIN run_status rs ON r.status_id = rs.status_id JOIN run_type rt ON r.run_type_id = rt.run_type_id LEFT JOIN build b ON r.build_id = b.build_id LEFT JOIN project p ON b.project_id = p.project_id WHERE r.by_user_id = $1 ORDER BY r.start_time DESC LIMIT $2 OFFSET $3"
                )
                .bind(user_id)
                .bind(limit)
                .bind(offset)
                .fetch_all(pg_pool)
                .await?;
                Ok(runs)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let runs = sqlx::query_as::<_, Run>(
                    "SELECT r.run_id, r.env_id, r.stream_id, r.build_id, r.run_type_id, rt.run_type, r.origin_run_id, r.event_id, r.start_time, r.stop_time, r.status_id, rs.status, r.by_user_id, r.result, r.info, r.retry_attempt, r.next_retry_at, b.project_id, p.name as project_name FROM run r JOIN run_status rs ON r.status_id = rs.status_id JOIN run_type rt ON r.run_type_id = rt.run_type_id LEFT JOIN build b ON r.build_id = b.build_id LEFT JOIN project p ON b.project_id = p.project_id WHERE r.by_user_id = ? ORDER BY r.start_time DESC LIMIT ? OFFSET ?"
                )
                .bind(user_id)
                .bind(limit)
                .bind(offset)
                .fetch_all(sqlite_pool)
                .await?;
                Ok(runs)
            }
        }
    }

    /// Get filtered runs by environment with pagination.
    ///
    /// `time_range_cutoff` filters to runs whose `start_time` is at or after
    /// the supplied UTC timestamp. Pass `None` for "all time". The handler
    /// is responsible for translating the user's ISO 8601 `time_range`
    /// query parameter into a cutoff via [`crate::time_range::parse_time_range_cutoff`].
    #[allow(clippy::too_many_arguments)]
    pub async fn get_filtered_runs_by_env(
        db: &crate::db::DatabasePool,
        env_id: &Uuid,
        statuses: Option<&[&str]>,
        run_types: Option<&[&str]>,
        time_range_cutoff: Option<chrono::DateTime<chrono::Utc>>,
        project_id: Option<&Uuid>, // Optional project filter
        search_term: Option<&str>, // Optional search term for Run ID, Stream ID, Project Name, Run Type
        limit: Option<i64>,
        offset: Option<i64>,
    ) -> Result<Vec<Run>, RunError> {
        let limit = limit.unwrap_or(10);
        let offset = offset.unwrap_or(0);

        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                // Build dynamic WHERE clauses (exclude task-type runs from listings)
                let mut where_clauses = vec![
                    "r.env_id = $1".to_string(),
                    "r.run_type_id != 7".to_string(),
                ];
                let mut param_count = 2; // Start at 2 since $1 is env_id

                // Add project filter if specified
                if project_id.is_some() {
                    where_clauses.push(format!("b.project_id = ${}", param_count));
                    param_count += 1;
                }

                // Add time range filter
                if time_range_cutoff.is_some() {
                    where_clauses.push(format!("r.start_time >= ${}", param_count));
                    param_count += 1;
                }

                let id_search = IdSearch::parse(search_term);

                if let Some(search) = &id_search
                    && search.uuid().is_some()
                {
                    // UUID detected: exact match on UUID fields + pattern match on text fields
                    // This allows finding UUIDs in user data (results, event_data, etc.)
                    let base = param_count;
                    where_clauses.push(format!(
                        "(r.run_id = ${} OR r.stream_id = ${} OR p.name ILIKE ${} OR rt.run_type ILIKE ${} OR COALESCE(e.event_data->>'fn', (SELECT function_name FROM call WHERE run_id = r.run_id AND parent_call_id IS NULL LIMIT 1)) ILIKE ${} OR CAST(r.result AS TEXT) ILIKE ${})",
                        base, base+1, base+2, base+3, base+4, base+5
                    ));
                    param_count += 6;
                } else if let Some(search) = &id_search
                    && search.is_short_id()
                {
                    // Short ID detected (12 hex chars): pattern match on UUID suffix + text fields
                    let base = param_count;
                    where_clauses.push(format!(
                        "(CAST(r.run_id AS TEXT) ILIKE ${} OR CAST(r.stream_id AS TEXT) ILIKE ${} OR CAST(r.event_id AS TEXT) ILIKE ${} OR CAST(r.origin_run_id AS TEXT) ILIKE ${} OR p.name ILIKE ${} OR rt.run_type ILIKE ${} OR COALESCE(e.event_data->>'fn', (SELECT function_name FROM call WHERE run_id = r.run_id AND parent_call_id IS NULL LIMIT 1)) ILIKE ${} OR CAST(r.result AS TEXT) ILIKE ${})",
                        base, base+1, base+2, base+3, base+4, base+5, base+6, base+7
                    ));
                    param_count += 8;
                } else if id_search.is_some() {
                    // Pattern matching for non-UUID searches
                    let base = param_count;
                    where_clauses.push(format!(
                        "(p.name ILIKE ${} OR rt.run_type ILIKE ${} OR COALESCE(e.event_data->>'fn', (SELECT function_name FROM call WHERE run_id = r.run_id AND parent_call_id IS NULL LIMIT 1)) ILIKE ${} OR CAST(r.result AS TEXT) ILIKE ${})",
                        base, base+1, base+2, base+3
                    ));
                    param_count += 4;
                }

                // Add status filter
                let status_filter = if let Some(statuses) = statuses {
                    if !statuses.is_empty() {
                        let placeholders = pg_placeholders(param_count, statuses.len());
                        param_count += statuses.len();
                        Some(format!("rs.status IN ({})", placeholders))
                    } else {
                        None
                    }
                } else {
                    None
                };

                if let Some(filter) = status_filter {
                    where_clauses.push(filter);
                }

                // Add run type filter
                let run_type_filter = if let Some(run_types) = run_types {
                    if !run_types.is_empty() {
                        let placeholders = pg_placeholders(param_count, run_types.len());
                        param_count += run_types.len();
                        Some(format!("rt.run_type IN ({})", placeholders))
                    } else {
                        None
                    }
                } else {
                    None
                };

                if let Some(filter) = run_type_filter {
                    where_clauses.push(filter);
                }

                let where_clause = where_clauses.join(" AND ");

                let query = format!(
                    "SELECT r.run_id, r.env_id, r.stream_id, r.build_id, r.run_type_id, rt.run_type, r.origin_run_id, r.event_id, r.start_time, r.stop_time, r.status_id, rs.status, r.by_user_id, r.result, r.info, r.retry_attempt, r.next_retry_at, b.project_id, p.name as project_name, COALESCE(e.event_data->>'fn', (SELECT function_name FROM call WHERE run_id = r.run_id AND parent_call_id IS NULL LIMIT 1), (SELECT function_name FROM task WHERE run_id = r.run_id LIMIT 1)) as event_fn, e.created_at as queued_at FROM run r JOIN run_status rs ON r.status_id = rs.status_id JOIN run_type rt ON r.run_type_id = rt.run_type_id LEFT JOIN build b ON r.build_id = b.build_id LEFT JOIN project p ON b.project_id = p.project_id LEFT JOIN event e ON r.event_id = e.event_id WHERE {} ORDER BY r.start_time DESC LIMIT ${} OFFSET ${}",
                    where_clause,
                    param_count,
                    param_count + 1
                );

                let mut q =
                    sqlx::query_as::<_, Run>(sqlx::AssertSqlSafe(query.as_str())).bind(env_id);

                // Bind project_id if specified
                if let Some(proj_id) = project_id {
                    q = q.bind(proj_id);
                }

                // Bind time range parameter
                if let Some(cutoff) = time_range_cutoff {
                    q = q.bind(cutoff);
                }

                if let Some(search) = &id_search
                    && let Some(uuid) = search.uuid()
                {
                    // Bind UUID for exact matching on UUID fields
                    q = q.bind(uuid); // run_id
                    q = q.bind(uuid); // stream_id
                    // Also bind pattern for searching UUID string in text fields
                    q = q.bind(search.text_pattern()); // project name
                    q = q.bind(search.text_pattern()); // run_type
                    q = q.bind(search.text_pattern()); // function name
                    q = q.bind(search.text_pattern()); // result (may contain UUIDs)
                } else if let Some(search) = &id_search
                    && let Some(suffix_pattern) = search.suffix_pattern()
                {
                    // Bind short ID suffix pattern for UUID fields + regular pattern for text fields
                    q = q.bind(suffix_pattern); // run_id suffix
                    q = q.bind(suffix_pattern); // stream_id suffix
                    q = q.bind(suffix_pattern); // event_id suffix
                    q = q.bind(suffix_pattern); // origin_run_id suffix
                    q = q.bind(search.text_pattern()); // project name
                    q = q.bind(search.text_pattern()); // run_type
                    q = q.bind(search.text_pattern()); // function name
                    q = q.bind(search.text_pattern()); // result
                } else if let Some(search) = &id_search {
                    // Bind for ILIKE pattern matching
                    q = q.bind(search.text_pattern()); // project name
                    q = q.bind(search.text_pattern()); // run_type
                    q = q.bind(search.text_pattern()); // function name
                    q = q.bind(search.text_pattern()); // result
                }

                // Bind status parameters
                if let Some(statuses) = statuses {
                    for status in statuses {
                        q = q.bind(*status);
                    }
                }

                // Bind run type parameters
                if let Some(run_types) = run_types {
                    for run_type in run_types {
                        q = q.bind(*run_type);
                    }
                }

                // Bind limit and offset
                q = q.bind(limit).bind(offset);

                let runs = q.fetch_all(pg_pool).await?;
                Ok(runs)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                // Build dynamic WHERE clauses (exclude task-type runs from listings)
                let mut where_clauses =
                    vec!["r.env_id = ?".to_string(), "r.run_type_id != 7".to_string()];

                // Add project filter if specified
                if project_id.is_some() {
                    where_clauses.push("b.project_id = ?".to_string());
                }

                // Add time range filter (cutoff bound below, after env_id/project_id binds)
                let time_cutoff_str =
                    time_range_cutoff.map(|c| c.format("%Y-%m-%d %H:%M:%S").to_string());
                if time_cutoff_str.is_some() {
                    where_clauses.push("r.start_time >= ?".to_string());
                }

                let id_search = IdSearch::parse(search_term);

                if let Some(search) = &id_search
                    && search.uuid().is_some()
                {
                    // UUID detected: exact match on UUID fields + pattern match on text fields
                    where_clauses.push(
                        "(r.run_id = ? OR r.stream_id = ? OR p.name LIKE ? OR rt.run_type LIKE ? OR COALESCE(json_extract(e.event_data, '$.fn'), (SELECT function_name FROM call WHERE run_id = r.run_id AND parent_call_id IS NULL LIMIT 1)) LIKE ? OR CAST(r.result AS TEXT) LIKE ?)".to_string()
                    );
                } else if let Some(search) = &id_search
                    && search.is_short_id()
                {
                    // Short ID detected (12 hex chars): pattern match on UUID suffix + text fields
                    where_clauses.push(
                        "(LOWER(HEX(r.run_id)) LIKE ? OR LOWER(HEX(r.stream_id)) LIKE ? OR LOWER(HEX(r.event_id)) LIKE ? OR LOWER(HEX(r.origin_run_id)) LIKE ? OR p.name LIKE ? OR rt.run_type LIKE ? OR COALESCE(json_extract(e.event_data, '$.fn'), (SELECT function_name FROM call WHERE run_id = r.run_id AND parent_call_id IS NULL LIMIT 1)) LIKE ? OR CAST(r.result AS TEXT) LIKE ?)"
                            .to_string(),
                    );
                } else if id_search.is_some() {
                    // Pattern matching for non-UUID searches
                    where_clauses.push(
                        "(p.name LIKE ? OR rt.run_type LIKE ? OR COALESCE(json_extract(e.event_data, '$.fn'), (SELECT function_name FROM call WHERE run_id = r.run_id AND parent_call_id IS NULL LIMIT 1)) LIKE ? OR CAST(r.result AS TEXT) LIKE ?)"
                            .to_string(),
                    );
                }

                // Add status filter
                let status_filter = if let Some(statuses) = statuses {
                    if !statuses.is_empty() {
                        let placeholders = sqlite_placeholders(statuses.len());
                        Some(format!("rs.status IN ({})", placeholders))
                    } else {
                        None
                    }
                } else {
                    None
                };

                if let Some(filter) = status_filter {
                    where_clauses.push(filter);
                }

                // Add run type filter
                let run_type_filter = if let Some(run_types) = run_types {
                    if !run_types.is_empty() {
                        let placeholders = sqlite_placeholders(run_types.len());
                        Some(format!("rt.run_type IN ({})", placeholders))
                    } else {
                        None
                    }
                } else {
                    None
                };

                if let Some(filter) = run_type_filter {
                    where_clauses.push(filter);
                }

                let where_clause = where_clauses.join(" AND ");

                let query = format!(
                    "SELECT r.run_id, r.env_id, r.stream_id, r.build_id, r.run_type_id, rt.run_type, r.origin_run_id, r.event_id, r.start_time, r.stop_time, r.status_id, rs.status, r.by_user_id, r.result, r.info, r.retry_attempt, r.next_retry_at, b.project_id, p.name as project_name, COALESCE(json_extract(e.event_data, '$.fn'), (SELECT function_name FROM call WHERE run_id = r.run_id AND parent_call_id IS NULL LIMIT 1), (SELECT function_name FROM task WHERE run_id = r.run_id LIMIT 1)) as event_fn, e.created_at as queued_at FROM run r JOIN run_status rs ON r.status_id = rs.status_id JOIN run_type rt ON r.run_type_id = rt.run_type_id LEFT JOIN build b ON r.build_id = b.build_id LEFT JOIN project p ON b.project_id = p.project_id LEFT JOIN event e ON r.event_id = e.event_id WHERE {} ORDER BY r.start_time DESC LIMIT ? OFFSET ?",
                    where_clause
                );

                let mut q =
                    sqlx::query_as::<_, Run>(sqlx::AssertSqlSafe(query.as_str())).bind(env_id);

                // Bind project_id if specified
                if let Some(proj_id) = project_id {
                    q = q.bind(proj_id);
                }

                // Bind time range cutoff (matches `r.start_time >= ?` clause above)
                if let Some(ref c) = time_cutoff_str {
                    q = q.bind(c);
                }

                if let Some(search) = &id_search
                    && let Some(uuid) = search.uuid()
                {
                    // Bind UUID for exact matching on UUID fields
                    q = q.bind(uuid); // run_id
                    q = q.bind(uuid); // stream_id
                    // Also bind pattern for searching UUID string in text fields
                    q = q.bind(search.text_pattern()); // project name
                    q = q.bind(search.text_pattern()); // run_type
                    q = q.bind(search.text_pattern()); // function name
                    q = q.bind(search.text_pattern()); // result (may contain UUIDs)
                } else if let Some(search) = &id_search
                    && let Some(suffix_pattern) = search.suffix_pattern()
                {
                    // Bind short ID suffix pattern for UUID fields + regular pattern for text fields
                    q = q.bind(suffix_pattern); // run_id suffix
                    q = q.bind(suffix_pattern); // stream_id suffix
                    q = q.bind(suffix_pattern); // event_id suffix
                    q = q.bind(suffix_pattern); // origin_run_id suffix
                    q = q.bind(search.text_pattern()); // project name
                    q = q.bind(search.text_pattern()); // run_type
                    q = q.bind(search.text_pattern()); // function name
                    q = q.bind(search.text_pattern()); // result
                } else if let Some(search) = &id_search {
                    // Bind for LIKE pattern matching
                    q = q.bind(search.text_pattern()); // project name
                    q = q.bind(search.text_pattern()); // run_type
                    q = q.bind(search.text_pattern()); // function name
                    q = q.bind(search.text_pattern()); // result
                }

                // Bind status parameters
                if let Some(statuses) = statuses {
                    for status in statuses {
                        q = q.bind(*status);
                    }
                }

                // Bind run type parameters
                if let Some(run_types) = run_types {
                    for run_type in run_types {
                        q = q.bind(*run_type);
                    }
                }

                // Bind limit and offset
                q = q.bind(limit).bind(offset);

                let runs = q.fetch_all(sqlite_pool).await?;
                Ok(runs)
            }
        }
    }

    /// Get count of filtered runs by environment.
    ///
    /// See [`Self::get_filtered_runs_by_env`] for the meaning of `time_range_cutoff`.
    pub async fn get_filtered_count_by_env(
        db: &crate::db::DatabasePool,
        env_id: &Uuid,
        statuses: Option<&[&str]>,
        run_types: Option<&[&str]>,
        time_range_cutoff: Option<chrono::DateTime<chrono::Utc>>,
        project_id: Option<&Uuid>, // Optional project filter
        search_term: Option<&str>, // Optional search term
    ) -> Result<i64, RunError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                // Build dynamic WHERE clauses (exclude task-type runs from counts)
                let mut where_clauses = vec![
                    "r.env_id = $1".to_string(),
                    "r.run_type_id != 7".to_string(),
                ];
                let mut param_count = 2; // Start at 2 since $1 is env_id

                // Add project filter if specified
                if project_id.is_some() {
                    where_clauses.push(format!("b.project_id = ${}", param_count));
                    param_count += 1;
                }

                // Add time range filter
                if time_range_cutoff.is_some() {
                    where_clauses.push(format!("r.start_time >= ${}", param_count));
                    param_count += 1;
                }

                let id_search = IdSearch::parse(search_term);

                if let Some(search) = &id_search
                    && search.uuid().is_some()
                {
                    // UUID detected: exact match on UUID fields + pattern match on text fields
                    where_clauses.push(format!(
                        "(r.run_id = ${} OR r.stream_id = ${} OR p.name ILIKE ${} OR rt.run_type ILIKE ${})",
                        param_count, param_count + 1, param_count + 2, param_count + 3
                    ));
                    param_count += 4;
                } else if let Some(search) = &id_search
                    && search.is_short_id()
                {
                    // Short ID detected (12 hex chars): pattern match on UUID suffix + text fields
                    let base = param_count;
                    where_clauses.push(format!(
                        "(CAST(r.run_id AS TEXT) ILIKE ${} OR CAST(r.stream_id AS TEXT) ILIKE ${} OR p.name ILIKE ${} OR rt.run_type ILIKE ${})",
                        base, base + 1, base + 2, base + 3
                    ));
                    param_count += 4;
                } else if id_search.is_some() {
                    // Pattern matching for non-UUID searches
                    let base = param_count;
                    where_clauses.push(format!(
                        "(p.name ILIKE ${} OR rt.run_type ILIKE ${})",
                        base,
                        base + 1
                    ));
                    param_count += 2;
                }

                // Add status filter
                let status_filter = if let Some(statuses) = statuses {
                    if !statuses.is_empty() {
                        let placeholders = pg_placeholders(param_count, statuses.len());
                        param_count += statuses.len();
                        Some(format!("rs.status IN ({})", placeholders))
                    } else {
                        None
                    }
                } else {
                    None
                };

                if let Some(filter) = status_filter {
                    where_clauses.push(filter);
                }

                // Add run type filter
                let run_type_filter = if let Some(run_types) = run_types {
                    if !run_types.is_empty() {
                        let placeholders = pg_placeholders(param_count, run_types.len());
                        Some(format!("rt.run_type IN ({})", placeholders))
                    } else {
                        None
                    }
                } else {
                    None
                };

                if let Some(filter) = run_type_filter {
                    where_clauses.push(filter);
                }

                let where_clause = where_clauses.join(" AND ");

                let query = format!(
                    "SELECT COUNT(*) FROM run r JOIN run_status rs ON r.status_id = rs.status_id JOIN run_type rt ON r.run_type_id = rt.run_type_id LEFT JOIN build b ON r.build_id = b.build_id LEFT JOIN project p ON b.project_id = p.project_id WHERE {}",
                    where_clause
                );

                let mut q =
                    sqlx::query_scalar::<_, i64>(sqlx::AssertSqlSafe(query.as_str())).bind(env_id);

                // Bind project_id if specified
                if let Some(proj_id) = project_id {
                    q = q.bind(proj_id);
                }

                // Bind time range cutoff
                if let Some(cutoff) = time_range_cutoff {
                    q = q.bind(cutoff);
                }

                if let Some(search) = &id_search
                    && let Some(uuid) = search.uuid()
                {
                    // Bind UUID for exact matching on UUID fields
                    q = q.bind(uuid); // run_id
                    q = q.bind(uuid); // stream_id
                    // Also bind pattern for searching UUID string in text fields
                    q = q.bind(search.text_pattern()); // project name
                    q = q.bind(search.text_pattern()); // run_type
                } else if let Some(search) = &id_search
                    && let Some(suffix_pattern) = search.suffix_pattern()
                {
                    // Bind short ID suffix pattern for UUID fields + regular pattern for text fields
                    q = q.bind(suffix_pattern); // run_id suffix
                    q = q.bind(suffix_pattern); // stream_id suffix
                    q = q.bind(search.text_pattern()); // project name
                    q = q.bind(search.text_pattern()); // run_type
                } else if let Some(search) = &id_search {
                    // Bind for ILIKE pattern matching
                    q = q.bind(search.text_pattern()); // project name
                    q = q.bind(search.text_pattern()); // run_type
                }

                // Bind status parameters
                if let Some(statuses) = statuses {
                    for status in statuses {
                        q = q.bind(*status);
                    }
                }

                // Bind run type parameters
                if let Some(run_types) = run_types {
                    for run_type in run_types {
                        q = q.bind(*run_type);
                    }
                }

                let count = q.fetch_one(pg_pool).await?;
                Ok(count)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                // Build dynamic WHERE clauses (exclude task-type runs from counts)
                let mut where_clauses =
                    vec!["r.env_id = ?".to_string(), "r.run_type_id != 7".to_string()];

                // Add project filter if specified
                if project_id.is_some() {
                    where_clauses.push("b.project_id = ?".to_string());
                }
                let time_cutoff_str =
                    time_range_cutoff.map(|c| c.format("%Y-%m-%d %H:%M:%S").to_string());
                if time_cutoff_str.is_some() {
                    where_clauses.push("r.start_time >= ?".to_string());
                }

                let id_search = IdSearch::parse(search_term);

                if let Some(search) = &id_search
                    && search.uuid().is_some()
                {
                    // UUID detected: exact match on UUID fields + pattern match on text fields
                    where_clauses.push(
                        "(r.run_id = ? OR r.stream_id = ? OR p.name LIKE ? OR rt.run_type LIKE ?)"
                            .to_string(),
                    );
                } else if let Some(search) = &id_search
                    && search.is_short_id()
                {
                    // Short ID detected (12 hex chars): pattern match on UUID suffix + text fields
                    where_clauses.push(
                        "(LOWER(HEX(r.run_id)) LIKE ? OR LOWER(HEX(r.stream_id)) LIKE ? OR p.name LIKE ? OR rt.run_type LIKE ?)"
                            .to_string(),
                    );
                } else if id_search.is_some() {
                    // Pattern matching for non-UUID searches
                    where_clauses.push("(p.name LIKE ? OR rt.run_type LIKE ?)".to_string());
                }

                // Add status filter
                let status_filter = if let Some(statuses) = statuses {
                    if !statuses.is_empty() {
                        let placeholders = sqlite_placeholders(statuses.len());
                        Some(format!("rs.status IN ({})", placeholders))
                    } else {
                        None
                    }
                } else {
                    None
                };

                if let Some(filter) = status_filter {
                    where_clauses.push(filter);
                }

                // Add run type filter
                let run_type_filter = if let Some(run_types) = run_types {
                    if !run_types.is_empty() {
                        let placeholders = sqlite_placeholders(run_types.len());
                        Some(format!("rt.run_type IN ({})", placeholders))
                    } else {
                        None
                    }
                } else {
                    None
                };

                if let Some(filter) = run_type_filter {
                    where_clauses.push(filter);
                }

                let where_clause = where_clauses.join(" AND ");

                let query = format!(
                    "SELECT COUNT(*) FROM run r JOIN run_status rs ON r.status_id = rs.status_id JOIN run_type rt ON r.run_type_id = rt.run_type_id LEFT JOIN build b ON r.build_id = b.build_id LEFT JOIN project p ON b.project_id = p.project_id WHERE {}",
                    where_clause
                );

                let mut q =
                    sqlx::query_scalar::<_, i64>(sqlx::AssertSqlSafe(query.as_str())).bind(env_id);

                // Bind project_id if specified
                if let Some(proj_id) = project_id {
                    q = q.bind(proj_id);
                }

                // Bind time range cutoff (matches `r.start_time >= ?` clause above)
                if let Some(ref c) = time_cutoff_str {
                    q = q.bind(c);
                }

                if let Some(search) = &id_search
                    && let Some(uuid) = search.uuid()
                {
                    // Bind UUID for exact matching on UUID fields
                    q = q.bind(uuid); // run_id
                    q = q.bind(uuid); // stream_id
                    // Also bind pattern for searching UUID string in text fields
                    q = q.bind(search.text_pattern()); // project name
                    q = q.bind(search.text_pattern()); // run_type
                } else if let Some(search) = &id_search
                    && let Some(suffix_pattern) = search.suffix_pattern()
                {
                    // Bind short ID suffix pattern for UUID fields + regular pattern for text fields
                    q = q.bind(suffix_pattern); // run_id suffix
                    q = q.bind(suffix_pattern); // stream_id suffix
                    q = q.bind(search.text_pattern()); // project name
                    q = q.bind(search.text_pattern()); // run_type
                } else if let Some(search) = &id_search {
                    // Bind for LIKE pattern matching
                    q = q.bind(search.text_pattern()); // project name
                    q = q.bind(search.text_pattern()); // run_type
                }

                // Bind status parameters
                if let Some(statuses) = statuses {
                    for status in statuses {
                        q = q.bind(*status);
                    }
                }

                // Bind run type parameters
                if let Some(run_types) = run_types {
                    for run_type in run_types {
                        q = q.bind(*run_type);
                    }
                }

                let count = q.fetch_one(sqlite_pool).await?;
                Ok(count)
            }
        }
    }

    /// Get active runs (no stop_time)
    pub async fn get_active_runs(
        db: &crate::db::DatabasePool,
        limit: Option<i64>,
    ) -> Result<Vec<Run>, RunError> {
        let limit = limit.unwrap_or(10);

        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let runs = sqlx::query_as::<_, Run>(
                    "SELECT r.run_id, r.env_id, r.stream_id, r.build_id, r.run_type_id, rt.run_type, r.origin_run_id, r.event_id, r.start_time, r.stop_time, r.status_id, rs.status, r.by_user_id, r.result, r.info, r.retry_attempt, r.next_retry_at, b.project_id, p.name as project_name, COALESCE(e.event_data->>'fn', (SELECT function_name FROM call WHERE run_id = r.run_id AND parent_call_id IS NULL LIMIT 1), (SELECT function_name FROM task WHERE run_id = r.run_id LIMIT 1)) as event_fn, e.created_at as queued_at FROM run r JOIN run_status rs ON r.status_id = rs.status_id JOIN run_type rt ON r.run_type_id = rt.run_type_id LEFT JOIN build b ON r.build_id = b.build_id LEFT JOIN project p ON b.project_id = p.project_id LEFT JOIN event e ON r.event_id = e.event_id WHERE r.stop_time IS NULL AND r.run_type_id != 7 ORDER BY r.start_time DESC LIMIT $1"
                )
                .bind(limit)
                .fetch_all(pg_pool)
                .await?;
                Ok(runs)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let runs = sqlx::query_as::<_, Run>(
                    "SELECT r.run_id, r.env_id, r.stream_id, r.build_id, r.run_type_id, rt.run_type, r.origin_run_id, r.event_id, r.start_time, r.stop_time, r.status_id, rs.status, r.by_user_id, r.result, r.info, r.retry_attempt, r.next_retry_at, b.project_id, p.name as project_name, COALESCE(json_extract(e.event_data, '$.fn'), (SELECT function_name FROM call WHERE run_id = r.run_id AND parent_call_id IS NULL LIMIT 1), (SELECT function_name FROM task WHERE run_id = r.run_id LIMIT 1)) as event_fn, e.created_at as queued_at FROM run r JOIN run_status rs ON r.status_id = rs.status_id JOIN run_type rt ON r.run_type_id = rt.run_type_id LEFT JOIN build b ON r.build_id = b.build_id LEFT JOIN project p ON b.project_id = p.project_id LEFT JOIN event e ON r.event_id = e.event_id WHERE r.stop_time IS NULL AND r.run_type_id != 7 ORDER BY r.start_time DESC LIMIT ?"
                )
                .bind(limit)
                .fetch_all(sqlite_pool)
                .await?;
                Ok(runs)
            }
        }
    }

    /// Insert a new run
    ///
    /// This function automatically ensures the stream exists before inserting the run.
    /// If the stream doesn't exist, it will be created with the provided parameters.
    #[allow(clippy::too_many_arguments)]
    pub async fn insert_run(
        db: &crate::db::DatabasePool,
        run_id: &Uuid,
        env_id: &Uuid,
        stream_id: &Uuid,
        build_id: Option<&Uuid>,
        run_type_id: i16,
        origin_run_id: Option<&Uuid>,
        by_user_id: &Uuid,
        start_time: Option<DateTime<Utc>>,
        access_id: Option<&Uuid>,
    ) -> Result<(), RunError> {
        let start_time = start_time.unwrap_or_else(Utc::now);

        // Ensure the stream exists (create if it doesn't)
        crate::db::stream::Stream::create_or_get_stream(db, *stream_id, *env_id)
            .await
            .map_err(|e| {
                RunError::Database(sqlx::Error::Protocol(format!(
                    "Failed to create stream: {}",
                    e
                )))
            })?;

        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                sqlx::query("INSERT INTO run (run_id, env_id, stream_id, build_id, run_type_id, origin_run_id, start_time, status_id, by_user_id, access_id) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)")
                    .bind(run_id)
                    .bind(env_id)
                    .bind(stream_id)
                    .bind(build_id)
                    .bind(run_type_id)
                    .bind(origin_run_id)
                    .bind(start_time)
                    .bind(RunStatus::Running.as_id())
                    .bind(by_user_id)
                    .bind(access_id)
                    .execute(pg_pool)
                    .await?;
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                sqlx::query("INSERT INTO run (run_id, env_id, stream_id, build_id, run_type_id, origin_run_id, start_time, status_id, by_user_id, access_id) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)")
                    .bind(run_id)
                    .bind(env_id)
                    .bind(stream_id)
                    .bind(build_id)
                    .bind(run_type_id)
                    .bind(origin_run_id)
                    .bind(start_time)
                    .bind(RunStatus::Running.as_id())
                    .bind(by_user_id)
                    .bind(access_id)
                    .execute(sqlite_pool)
                    .await?;
            }
        }

        // Update stream metrics after inserting the run
        crate::db::stream::Stream::update_metrics(db, stream_id)
            .await
            .map_err(|e| {
                RunError::Database(sqlx::Error::Protocol(format!(
                    "Failed to update stream metrics: {}",
                    e
                )))
            })?;

        Ok(())
    }

    /// Ensure a run row exists in the DB, inserting it if absent.
    /// Uses INSERT ... ON CONFLICT DO NOTHING so it's safe to call even if the
    /// emitter's async writer already committed the row.
    /// This is used by `::hot::task/start` to guarantee the origin run exists
    /// before inserting a task row with an `origin_run_id` FK reference.
    #[allow(clippy::too_many_arguments)]
    pub async fn ensure_run_exists(
        db: &crate::db::DatabasePool,
        run_id: &Uuid,
        env_id: &Uuid,
        stream_id: &Uuid,
        build_id: Option<&Uuid>,
        run_type_id: i16,
        origin_run_id: Option<&Uuid>,
        by_user_id: &Uuid,
        access_id: Option<&Uuid>,
    ) -> Result<(), RunError> {
        let now = Utc::now();

        // Ensure the stream exists first (same as insert_run)
        crate::db::stream::Stream::create_or_get_stream(db, *stream_id, *env_id)
            .await
            .map_err(|e| {
                RunError::Database(sqlx::Error::Protocol(format!(
                    "Failed to create stream: {}",
                    e
                )))
            })?;

        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                sqlx::query(
                    "INSERT INTO run (run_id, env_id, stream_id, build_id, run_type_id, origin_run_id, start_time, status_id, by_user_id, access_id) \
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) \
                     ON CONFLICT (run_id) DO NOTHING",
                )
                .bind(run_id)
                .bind(env_id)
                .bind(stream_id)
                .bind(build_id)
                .bind(run_type_id)
                .bind(origin_run_id)
                .bind(now)
                .bind(RunStatus::Running.as_id())
                .bind(by_user_id)
                .bind(access_id)
                .execute(pg_pool)
                .await?;
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                sqlx::query(
                    "INSERT OR IGNORE INTO run (run_id, env_id, stream_id, build_id, run_type_id, origin_run_id, start_time, status_id, by_user_id, access_id) \
                     VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                )
                .bind(run_id)
                .bind(env_id)
                .bind(stream_id)
                .bind(build_id)
                .bind(run_type_id)
                .bind(origin_run_id)
                .bind(now)
                .bind(RunStatus::Running.as_id())
                .bind(by_user_id)
                .bind(access_id)
                .execute(sqlite_pool)
                .await?;
            }
        }

        Ok(())
    }

    /// Update run stop time and status
    pub async fn update_stop_time_and_status(
        db: &crate::db::DatabasePool,
        run_id: &Uuid,
        stop_time: Option<DateTime<Utc>>,
        status: &RunStatus,
    ) -> Result<(), RunError> {
        let stop_time = stop_time.unwrap_or_else(Utc::now);

        // First get the stream_id for this run so we can update stream metrics
        let stream_id: Option<Uuid> = match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                sqlx::query_scalar("SELECT stream_id FROM run WHERE run_id = $1")
                    .bind(run_id)
                    .fetch_optional(pg_pool)
                    .await?
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                sqlx::query_scalar("SELECT stream_id FROM run WHERE run_id = ?")
                    .bind(run_id)
                    .fetch_optional(sqlite_pool)
                    .await?
            }
        };

        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                sqlx::query("UPDATE run SET stop_time = $2, status_id = $3 WHERE run_id = $1")
                    .bind(run_id)
                    .bind(stop_time)
                    .bind(status.as_id())
                    .execute(pg_pool)
                    .await?;
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                sqlx::query("UPDATE run SET stop_time = ?, status_id = ? WHERE run_id = ?")
                    .bind(stop_time)
                    .bind(status.as_id())
                    .bind(run_id)
                    .execute(sqlite_pool)
                    .await?;
            }
        }

        // Update stream metrics after updating run completion
        if let Some(stream_id) = stream_id {
            crate::db::stream::Stream::update_metrics(db, &stream_id)
                .await
                .map_err(|e| {
                    RunError::Database(sqlx::Error::Protocol(format!(
                        "Failed to update stream metrics: {}",
                        e
                    )))
                })?;
        }

        Ok(())
    }

    /// Update run stop time (legacy method, now sets status to succeeded)
    pub async fn update_stop_time(
        db: &crate::db::DatabasePool,
        run_id: &Uuid,
        stop_time: Option<DateTime<Utc>>,
    ) -> Result<(), RunError> {
        Self::update_stop_time_and_status(db, run_id, stop_time, &RunStatus::Succeeded).await
    }

    /// Update run info (for warnings, routing decisions, diagnostics)
    /// Only updates if info is Some - does not clear existing info with None
    pub async fn update_info(
        db: &crate::db::DatabasePool,
        run_id: &Uuid,
        info: Option<&serde_json::Value>,
    ) -> Result<(), RunError> {
        // Only update if we have info to set
        let Some(info_val) = info else {
            return Ok(());
        };

        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                sqlx::query("UPDATE run SET info = $2 WHERE run_id = $1")
                    .bind(run_id)
                    .bind(info_val)
                    .execute(pg_pool)
                    .await?;
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let info_str = info_val.to_string();
                sqlx::query("UPDATE run SET info = ? WHERE run_id = ?")
                    .bind(&info_str)
                    .bind(run_id)
                    .execute(sqlite_pool)
                    .await?;
            }
        }

        Ok(())
    }

    /// Fail a run with an error message (sets status to Failed, stop_time to now, and result to error)
    pub async fn fail_run(
        db: &crate::db::DatabasePool,
        run_id: &Uuid,
        error_message: &str,
    ) -> Result<(), RunError> {
        let stop_time = Utc::now();
        // Use proper Hot type format: {$type: "::hot::run/Failure", $val: {msg: ..., err: ...}}
        let result_val = serde_json::json!({
            "$type": "::hot::run/Failure",
            "$val": {
                "msg": error_message,
                "err": serde_json::Value::Null
            }
        });

        // First get the stream_id for this run so we can update stream metrics
        let stream_id: Option<Uuid> = match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                sqlx::query_scalar("SELECT stream_id FROM run WHERE run_id = $1")
                    .bind(run_id)
                    .fetch_optional(pg_pool)
                    .await?
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                sqlx::query_scalar("SELECT stream_id FROM run WHERE run_id = ?")
                    .bind(run_id)
                    .fetch_optional(sqlite_pool)
                    .await?
            }
        };

        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                // PostgreSQL: bind serde_json::Value directly for jsonb column
                sqlx::query(
                    "UPDATE run SET stop_time = $2, status_id = $3, result = $4 WHERE run_id = $1 AND status_id = $5",
                )
                .bind(run_id)
                .bind(stop_time)
                .bind(RunStatus::Failed.as_id())
                .bind(&result_val)
                .bind(RunStatus::Running.as_id())
                .execute(pg_pool)
                .await?;
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                // SQLite: convert to string since it doesn't have native jsonb
                let result_json = result_val.to_string();
                sqlx::query(
                    "UPDATE run SET stop_time = ?, status_id = ?, result = ? WHERE run_id = ? AND status_id = ?",
                )
                .bind(stop_time)
                .bind(RunStatus::Failed.as_id())
                .bind(&result_json)
                .bind(run_id)
                .bind(RunStatus::Running.as_id())
                .execute(sqlite_pool)
                .await?;
            }
        }

        // Update stream metrics after updating run completion
        if let Some(stream_id) = stream_id {
            crate::db::stream::Stream::update_metrics(db, &stream_id)
                .await
                .map_err(|e| {
                    RunError::Database(sqlx::Error::Protocol(format!(
                        "Failed to update stream metrics: {}",
                        e
                    )))
                })?;
        }

        Ok(())
    }

    /// Fail all runs for a given event_id that are still in "running" status.
    /// This is used during orphaned event recovery to clean up runs that were
    /// interrupted when a worker crashed.
    ///
    /// Returns the number of runs that were marked as failed.
    pub async fn fail_orphaned_runs_by_event_id(
        db: &crate::db::DatabasePool,
        event_id: &Uuid,
        error_message: &str,
    ) -> Result<u64, RunError> {
        let stop_time = Utc::now();
        // Use proper Hot type format: {$type: "::hot::run/Failure", $val: {msg: ..., err: ...}}
        let result_val = serde_json::json!({
            "$type": "::hot::run/Failure",
            "$val": {
                "msg": error_message,
                "err": serde_json::Value::Null
            }
        });

        let rows_affected = match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let result = sqlx::query(
                    "UPDATE run SET stop_time = $2, status_id = $3, result = $4 WHERE event_id = $1 AND status_id = $5",
                )
                .bind(event_id)
                .bind(stop_time)
                .bind(RunStatus::Failed.as_id())
                .bind(&result_val)
                .bind(RunStatus::Running.as_id())
                .execute(pg_pool)
                .await?;
                result.rows_affected()
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let result_json = result_val.to_string();
                let result = sqlx::query(
                    "UPDATE run SET stop_time = ?, status_id = ?, result = ? WHERE event_id = ? AND status_id = ?",
                )
                .bind(stop_time)
                .bind(RunStatus::Failed.as_id())
                .bind(&result_json)
                .bind(event_id)
                .bind(RunStatus::Running.as_id())
                .execute(sqlite_pool)
                .await?;
                result.rows_affected()
            }
        };

        if rows_affected > 0 {
            tracing::info!(
                "Marked {} orphaned run(s) as failed for event_id {}",
                rows_affected,
                event_id
            );
        }

        Ok(rows_affected)
    }

    /// Fail all runs in a stream that are still in "running" status.
    /// This is used during orphaned event recovery - when a worker crashes,
    /// all runs in the affected stream(s) that were in progress should be marked as failed.
    ///
    /// Returns the number of runs that were marked as failed.
    pub async fn fail_orphaned_runs_by_stream_id(
        db: &crate::db::DatabasePool,
        stream_id: &Uuid,
        error_message: &str,
    ) -> Result<u64, RunError> {
        let stop_time = Utc::now();
        // Use proper Hot type format: {$type: "::hot::run/Failure", $val: {msg: ..., err: ...}}
        let result_val = serde_json::json!({
            "$type": "::hot::run/Failure",
            "$val": {
                "msg": error_message,
                "err": serde_json::Value::Null
            }
        });

        let rows_affected = match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let result = sqlx::query(
                    "UPDATE run SET stop_time = $2, status_id = $3, result = $4 WHERE stream_id = $1 AND status_id = $5",
                )
                .bind(stream_id)
                .bind(stop_time)
                .bind(RunStatus::Failed.as_id())
                .bind(&result_val)
                .bind(RunStatus::Running.as_id())
                .execute(pg_pool)
                .await?;
                result.rows_affected()
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let result_json = result_val.to_string();
                let result = sqlx::query(
                    "UPDATE run SET stop_time = ?, status_id = ?, result = ? WHERE stream_id = ? AND status_id = ?",
                )
                .bind(stop_time)
                .bind(RunStatus::Failed.as_id())
                .bind(&result_json)
                .bind(stream_id)
                .bind(RunStatus::Running.as_id())
                .execute(sqlite_pool)
                .await?;
                result.rows_affected()
            }
        };

        if rows_affected > 0 {
            tracing::info!(
                "Marked {} orphaned run(s) as failed for stream_id {}",
                rows_affected,
                stream_id
            );
        }

        Ok(rows_affected)
    }

    /// Fail every run still flagged `running` whose `start_time` is older than
    /// `older_than`. This is the daily zombie-run reaper: runs can remain in
    /// `running` if the post-execution DB write is blocked or a worker exits
    /// before marking its tasks. Even with per-call DB timeouts in
    /// `hot_task_worker`, a periodic sweep is still needed as a final backstop.
    ///
    /// Caps at `limit` rows per call so a multi-thousand-row backlog can't
    /// stall the maintenance worker.
    ///
    /// Returns the number of runs that were marked as failed.
    pub async fn fail_stale_runs(
        db: &crate::db::DatabasePool,
        older_than: DateTime<Utc>,
        error_message: &str,
        limit: i64,
    ) -> Result<u64, RunError> {
        let stop_time = Utc::now();
        let result_val = serde_json::json!({
            "$type": "::hot::run/Failure",
            "$val": {
                "msg": error_message,
                "err": serde_json::Value::Null,
            }
        });

        let rows_affected = match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                // Use a CTE so we can both bound the work (`LIMIT`) and report
                // back the affected rows in one round-trip.
                let result = sqlx::query(
                    "WITH stale AS (
                        SELECT run_id FROM run
                        WHERE status_id = $1 AND start_time < $2
                        ORDER BY start_time
                        LIMIT $5
                     )
                     UPDATE run
                        SET stop_time = $3, status_id = $4, result = $6
                      WHERE run_id IN (SELECT run_id FROM stale)",
                )
                .bind(RunStatus::Running.as_id())
                .bind(older_than)
                .bind(stop_time)
                .bind(RunStatus::Failed.as_id())
                .bind(limit)
                .bind(&result_val)
                .execute(pg_pool)
                .await?;
                result.rows_affected()
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let result_json = result_val.to_string();
                let result = sqlx::query(
                    "UPDATE run
                        SET stop_time = ?, status_id = ?, result = ?
                      WHERE run_id IN (
                          SELECT run_id FROM run
                          WHERE status_id = ? AND start_time < ?
                          ORDER BY start_time
                          LIMIT ?
                      )",
                )
                .bind(stop_time)
                .bind(RunStatus::Failed.as_id())
                .bind(&result_json)
                .bind(RunStatus::Running.as_id())
                .bind(older_than)
                .bind(limit)
                .execute(sqlite_pool)
                .await?;
                result.rows_affected()
            }
        };

        if rows_affected > 0 {
            tracing::warn!(
                cutoff = %older_than,
                "Reaped {} zombie run(s) (status=running, start_time < cutoff)",
                rows_affected,
            );
        }

        Ok(rows_affected)
    }

    /// Update run status only
    pub async fn update_status(
        db: &crate::db::DatabasePool,
        run_id: &str,
        status: &RunStatus,
    ) -> Result<(), RunError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                sqlx::query("UPDATE run SET status_id = $2 WHERE run_id = $1")
                    .bind(run_id)
                    .bind(status.as_id())
                    .execute(pg_pool)
                    .await?;
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                sqlx::query("UPDATE run SET status_id = ? WHERE run_id = ?")
                    .bind(status.as_id())
                    .bind(run_id)
                    .execute(sqlite_pool)
                    .await?;
            }
        }
        Ok(())
    }

    /// Get recent runs within a time range
    pub async fn get_runs_by_time_range(
        db: &crate::db::DatabasePool,
        start_after: DateTime<Utc>,
        start_before: Option<DateTime<Utc>>,
        limit: Option<i64>,
    ) -> Result<Vec<Run>, RunError> {
        let limit = limit.unwrap_or(10);

        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                if let Some(start_before) = start_before {
                    let runs = sqlx::query_as::<_, Run>(
                        "SELECT r.run_id, r.env_id, r.stream_id, r.build_id, r.run_type_id, rt.run_type, r.origin_run_id, r.event_id, r.start_time, r.stop_time, r.status_id, rs.status, r.by_user_id, r.result, r.info, r.retry_attempt, r.next_retry_at FROM run r JOIN run_status rs ON r.status_id = rs.status_id JOIN run_type rt ON r.run_type_id = rt.run_type_id WHERE r.start_time >= $1 AND r.start_time <= $2 ORDER BY r.start_time DESC LIMIT $3"
                    )
                    .bind(start_after)
                    .bind(start_before)
                    .bind(limit)
                    .fetch_all(pg_pool)
                    .await?;
                    Ok(runs)
                } else {
                    let runs = sqlx::query_as::<_, Run>(
                        "SELECT r.run_id, r.env_id, r.stream_id, r.build_id, r.run_type_id, rt.run_type, r.origin_run_id, r.event_id, r.start_time, r.stop_time, r.status_id, rs.status, r.by_user_id, r.result, r.info, r.retry_attempt, r.next_retry_at FROM run r JOIN run_status rs ON r.status_id = rs.status_id JOIN run_type rt ON r.run_type_id = rt.run_type_id WHERE r.start_time >= $1 ORDER BY r.start_time DESC LIMIT $2"
                    )
                    .bind(start_after)
                    .bind(limit)
                    .fetch_all(pg_pool)
                    .await?;
                    Ok(runs)
                }
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                if let Some(start_before) = start_before {
                    let runs = sqlx::query_as::<_, Run>(
                        "SELECT r.run_id, r.env_id, r.stream_id, r.build_id, r.run_type_id, rt.run_type, r.origin_run_id, r.event_id, r.start_time, r.stop_time, r.status_id, rs.status, r.by_user_id, r.result, r.info, r.retry_attempt, r.next_retry_at FROM run r JOIN run_status rs ON r.status_id = rs.status_id JOIN run_type rt ON r.run_type_id = rt.run_type_id WHERE r.start_time >= ? AND r.start_time <= ? ORDER BY r.start_time DESC LIMIT ?"
                    )
                    .bind(start_after)
                    .bind(start_before)
                    .bind(limit)
                    .fetch_all(sqlite_pool)
                    .await?;
                    Ok(runs)
                } else {
                    let runs = sqlx::query_as::<_, Run>(
                        "SELECT r.run_id, r.env_id, r.stream_id, r.build_id, r.run_type_id, rt.run_type, r.origin_run_id, r.event_id, r.start_time, r.stop_time, r.status_id, rs.status, r.by_user_id, r.result, r.info, r.retry_attempt, r.next_retry_at FROM run r JOIN run_status rs ON r.status_id = rs.status_id JOIN run_type rt ON r.run_type_id = rt.run_type_id WHERE r.start_time >= ? ORDER BY r.start_time DESC LIMIT ?"
                    )
                    .bind(start_after)
                    .bind(limit)
                    .fetch_all(sqlite_pool)
                    .await?;
                    Ok(runs)
                }
            }
        }
    }

    /// Get recent runs within a time range filtered by environment
    pub async fn get_runs_by_time_range_and_env(
        db: &crate::db::DatabasePool,
        start_after: DateTime<Utc>,
        start_before: Option<DateTime<Utc>>,
        env_id: &Uuid,
        limit: Option<i64>,
    ) -> Result<Vec<Run>, RunError> {
        let limit = limit.unwrap_or(10);

        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                if let Some(start_before) = start_before {
                    let runs = sqlx::query_as::<_, Run>(
                        "SELECT r.run_id, r.env_id, r.stream_id, r.build_id, r.run_type_id, rt.run_type, r.origin_run_id, r.event_id, r.start_time, r.stop_time, r.status_id, rs.status, r.by_user_id, r.result, r.info, r.retry_attempt, r.next_retry_at FROM run r JOIN run_status rs ON r.status_id = rs.status_id JOIN run_type rt ON r.run_type_id = rt.run_type_id WHERE r.start_time >= $1 AND r.start_time <= $2 AND r.env_id = $3 ORDER BY r.start_time DESC LIMIT $4"
                    )
                    .bind(start_after)
                    .bind(start_before)
                    .bind(env_id)
                    .bind(limit)
                    .fetch_all(pg_pool)
                    .await?;
                    Ok(runs)
                } else {
                    let runs = sqlx::query_as::<_, Run>(
                        "SELECT r.run_id, r.env_id, r.stream_id, r.build_id, r.run_type_id, rt.run_type, r.origin_run_id, r.event_id, r.start_time, r.stop_time, r.status_id, rs.status, r.by_user_id, r.result, r.info, r.retry_attempt, r.next_retry_at FROM run r JOIN run_status rs ON r.status_id = rs.status_id JOIN run_type rt ON r.run_type_id = rt.run_type_id WHERE r.start_time >= $1 AND r.env_id = $2 ORDER BY r.start_time DESC LIMIT $3"
                    )
                    .bind(start_after)
                    .bind(env_id)
                    .bind(limit)
                    .fetch_all(pg_pool)
                    .await?;
                    Ok(runs)
                }
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                if let Some(start_before) = start_before {
                    let runs = sqlx::query_as::<_, Run>(
                        "SELECT r.run_id, r.env_id, r.stream_id, r.build_id, r.run_type_id, rt.run_type, r.origin_run_id, r.event_id, r.start_time, r.stop_time, r.status_id, rs.status, r.by_user_id, r.result, r.info, r.retry_attempt, r.next_retry_at FROM run r JOIN run_status rs ON r.status_id = rs.status_id JOIN run_type rt ON r.run_type_id = rt.run_type_id WHERE r.start_time >= ? AND r.start_time <= ? AND r.env_id = ? ORDER BY r.start_time DESC LIMIT ?"
                    )
                    .bind(start_after)
                    .bind(start_before)
                    .bind(env_id)
                    .bind(limit)
                    .fetch_all(sqlite_pool)
                    .await?;
                    Ok(runs)
                } else {
                    let runs = sqlx::query_as::<_, Run>(
                        "SELECT r.run_id, r.env_id, r.stream_id, r.build_id, r.run_type_id, rt.run_type, r.origin_run_id, r.event_id, r.start_time, r.stop_time, r.status_id, rs.status, r.by_user_id, r.result, r.info, r.retry_attempt, r.next_retry_at FROM run r JOIN run_status rs ON r.status_id = rs.status_id JOIN run_type rt ON r.run_type_id = rt.run_type_id WHERE r.start_time >= ? AND r.env_id = ? ORDER BY r.start_time DESC LIMIT ?"
                    )
                    .bind(start_after)
                    .bind(env_id)
                    .bind(limit)
                    .fetch_all(sqlite_pool)
                    .await?;
                    Ok(runs)
                }
            }
        }
    }

    /// Check if a run belongs to one of the specified environments
    pub async fn is_run_in_envs(
        db: &crate::db::DatabasePool,
        run_id: &Uuid,
        env_ids: &[Uuid],
    ) -> Result<bool, RunError> {
        if env_ids.is_empty() {
            return Ok(false);
        }

        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let placeholders = (1..=env_ids.len())
                    .map(|i| format!("${}", i + 1))
                    .collect::<Vec<_>>()
                    .join(", ");

                let query = format!(
                    "SELECT COUNT(*) FROM run WHERE run_id = $1 AND env_id IN ({})",
                    placeholders
                );

                let mut q =
                    sqlx::query_scalar::<_, i64>(sqlx::AssertSqlSafe(query.as_str())).bind(run_id);

                for env_id in env_ids {
                    q = q.bind(env_id);
                }

                let count = q.fetch_one(pg_pool).await?;
                Ok(count > 0)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let placeholders = (1..=env_ids.len())
                    .map(|_| "?")
                    .collect::<Vec<_>>()
                    .join(", ");

                let query = format!(
                    "SELECT COUNT(*) FROM run WHERE run_id = ? AND env_id IN ({})",
                    placeholders
                );

                let mut q =
                    sqlx::query_scalar::<_, i64>(sqlx::AssertSqlSafe(query.as_str())).bind(run_id);

                for env_id in env_ids {
                    q = q.bind(env_id);
                }

                let count = q.fetch_one(sqlite_pool).await?;
                Ok(count > 0)
            }
        }
    }

    /// Delete run
    pub async fn delete_run(db: &crate::db::DatabasePool, run_id: &Uuid) -> Result<(), RunError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                sqlx::query("DELETE FROM run WHERE run_id = $1")
                    .bind(run_id)
                    .execute(pg_pool)
                    .await?;
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                sqlx::query("DELETE FROM run WHERE run_id = ?")
                    .bind(run_id)
                    .execute(sqlite_pool)
                    .await?;
            }
        }
        Ok(())
    }

    /// Get run duration statistics by environment
    pub async fn get_duration_stats_by_env(
        db: &crate::db::DatabasePool,
        env_id: &Uuid,
        limit: Option<i64>,
    ) -> Result<Vec<(String, Option<i64>)>, RunError> {
        let limit = limit.unwrap_or(10);

        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let rows = sqlx::query(
                    r#"
                    SELECT
                        run_id,
                        EXTRACT(EPOCH FROM (stop_time - start_time))::bigint as duration_seconds
                    FROM run
                    WHERE env_id = $1 AND stop_time IS NOT NULL
                    ORDER BY start_time DESC
                    LIMIT $2
                    "#,
                )
                .bind(env_id)
                .bind(limit)
                .fetch_all(pg_pool)
                .await?;

                let mut stats = Vec::new();
                for row in rows {
                    let run_id: String = row.get("run_id");
                    let duration: Option<i64> = row.get("duration_seconds");
                    stats.push((run_id, duration));
                }
                Ok(stats)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let rows = sqlx::query(
                    r#"
                    SELECT
                        run_id,
                        CAST((julianday(stop_time) - julianday(start_time)) * 86400 AS INTEGER) as duration_seconds
                    FROM run
                    WHERE env_id = ? AND stop_time IS NOT NULL
                    ORDER BY start_time DESC
                    LIMIT ?
                    "#
                )
                .bind(env_id)
                .bind(limit)
                .fetch_all(sqlite_pool)
                .await?;

                let mut stats = Vec::new();
                for row in rows {
                    let run_id: String = row.get("run_id");
                    let duration: Option<i32> = row.get("duration_seconds");
                    stats.push((run_id, duration.map(|d| d as i64)));
                }
                Ok(stats)
            }
        }
    }

    /// Get daily run counts by run type for the last 30 days
    pub async fn get_daily_run_counts_by_type(
        db: &crate::db::DatabasePool,
        env_id: &Uuid,
        days: i32,
    ) -> Result<AHashMap<String, AHashMap<String, i64>>, RunError> {
        let days_ago = Utc::now() - chrono::Duration::days(days as i64);

        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let rows = sqlx::query(
                    r#"
                    SELECT
                        (r.start_time AT TIME ZONE 'UTC')::date as run_date,
                        rt.run_type,
                        COUNT(*) as count
                    FROM run r
                    JOIN run_type rt ON r.run_type_id = rt.run_type_id
                    WHERE r.env_id = $1 AND r.start_time >= $2 AND r.run_type_id != 7
                    GROUP BY (r.start_time AT TIME ZONE 'UTC')::date, rt.run_type
                    ORDER BY run_date ASC, rt.run_type ASC
                    "#,
                )
                .bind(env_id)
                .bind(days_ago)
                .fetch_all(pg_pool)
                .await?;

                let mut result = AHashMap::new();
                for row in rows {
                    let date: chrono::NaiveDate = row.get("run_date");
                    let run_type: String = row.get("run_type");
                    let count: i64 = row.get("count");

                    let date_str = date.format("%Y-%m-%d").to_string();
                    result
                        .entry(date_str)
                        .or_insert_with(AHashMap::new)
                        .insert(run_type, count);
                }
                Ok(result)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let rows = sqlx::query(
                    r#"
                    SELECT
                        DATE(r.start_time) as run_date,
                        rt.run_type,
                        COUNT(*) as count
                    FROM run r
                    JOIN run_type rt ON r.run_type_id = rt.run_type_id
                    WHERE r.env_id = ? AND r.start_time >= ? AND r.run_type_id != 7
                    GROUP BY DATE(r.start_time), rt.run_type
                    ORDER BY run_date ASC, rt.run_type ASC
                    "#,
                )
                .bind(env_id)
                .bind(days_ago)
                .fetch_all(sqlite_pool)
                .await?;

                let mut result = AHashMap::new();
                for row in rows {
                    let date: String = row.get("run_date");
                    let run_type: String = row.get("run_type");
                    let count: i64 = row.get("count");

                    result
                        .entry(date)
                        .or_insert_with(AHashMap::new)
                        .insert(run_type, count);
                }
                Ok(result)
            }
        }
    }

    /// Get run type chart data with filters for time range, time unit, and run types
    pub async fn get_run_type_chart_data_with_filters(
        db: &crate::db::DatabasePool,
        env_id: &Uuid,
        days: i32,
        time_unit: &str,
        run_types: &[&str],
    ) -> Result<AHashMap<String, AHashMap<String, i64>>, RunError> {
        let days_ago = Utc::now() - chrono::Duration::days(days as i64);

        // Create run type filter for SQL IN clause
        let run_type_placeholders = run_types.iter().map(|_| "?").collect::<Vec<_>>().join(", ");

        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let (group_by_clause, date_format) = match time_unit {
                    "hour" => ("DATE_TRUNC('hour', r.start_time)", "%Y-%m-%d %H:00"),
                    "month" => ("DATE_TRUNC('month', r.start_time)", "%Y-%m"),
                    _ => ("DATE_TRUNC('day', r.start_time)", "%Y-%m-%d"), // default to day
                };

                let run_type_placeholders = (1..=run_types.len())
                    .map(|i| format!("${}", i + 2))
                    .collect::<Vec<_>>()
                    .join(", ");

                let query = format!(
                    r#"
                    SELECT
                        {} as time_period,
                        rt.run_type,
                        COUNT(*) as count
                    FROM run r
                    JOIN run_type rt ON r.run_type_id = rt.run_type_id
                    WHERE r.env_id = $1 AND r.start_time >= $2 AND rt.run_type IN ({})
                    GROUP BY time_period, rt.run_type
                    ORDER BY time_period ASC, rt.run_type ASC
                    "#,
                    group_by_clause, run_type_placeholders
                );

                let mut q = sqlx::query(sqlx::AssertSqlSafe(query.as_str()))
                    .bind(env_id)
                    .bind(days_ago);
                for run_type in run_types {
                    q = q.bind(*run_type);
                }

                let rows = q.fetch_all(pg_pool).await?;

                let mut result = AHashMap::new();
                for row in rows {
                    let time_period: chrono::DateTime<Utc> = row.get("time_period");
                    let run_type: String = row.get("run_type");
                    let count: i64 = row.get("count");

                    let time_str = time_period.format(date_format).to_string();
                    result
                        .entry(time_str)
                        .or_insert_with(AHashMap::new)
                        .insert(run_type, count);
                }
                Ok(result)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let (group_by_clause, _date_format) = match time_unit {
                    "hour" => ("strftime('%Y-%m-%d %H:00', r.start_time)", "%Y-%m-%d %H:00"),
                    "month" => ("strftime('%Y-%m', r.start_time)", "%Y-%m"),
                    _ => ("DATE(r.start_time)", "%Y-%m-%d"), // default to day
                };

                let query = format!(
                    r#"
                    SELECT
                        {} as time_period,
                        rt.run_type,
                        COUNT(*) as count
                    FROM run r
                    JOIN run_type rt ON r.run_type_id = rt.run_type_id
                    WHERE r.env_id = ? AND r.start_time >= ? AND rt.run_type IN ({})
                    GROUP BY time_period, rt.run_type
                    ORDER BY time_period ASC, rt.run_type ASC
                    "#,
                    group_by_clause, run_type_placeholders
                );

                let mut q = sqlx::query(sqlx::AssertSqlSafe(query.as_str()))
                    .bind(env_id)
                    .bind(days_ago);
                for run_type in run_types {
                    q = q.bind(*run_type);
                }

                let rows = q.fetch_all(sqlite_pool).await?;

                let mut result = AHashMap::new();
                for row in rows {
                    let time_period: String = row.get("time_period");
                    let run_type: String = row.get("run_type");
                    let count: i64 = row.get("count");

                    result
                        .entry(time_period)
                        .or_insert_with(AHashMap::new)
                        .insert(run_type, count);
                }
                Ok(result)
            }
        }
    }

    /// Get run type chart data with filters for time range, time unit, and run types.
    ///
    /// `time_range_cutoff` is `None` for "all time" or `Some(cutoff)` to limit
    /// to runs whose `start_time` is at or after the cutoff. Compute via
    /// [`crate::time_range::parse_time_range_cutoff`].
    pub async fn get_run_type_chart_data_with_interval_filters(
        db: &crate::db::DatabasePool,
        env_id: &Uuid,
        time_range_cutoff: Option<chrono::DateTime<chrono::Utc>>,
        time_unit: &str,
        run_types: &[&str],
    ) -> Result<AHashMap<String, AHashMap<String, i64>>, RunError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let (group_by_clause, date_format) = match time_unit {
                    "hour" => ("DATE_TRUNC('hour', r.start_time)", "%Y-%m-%d %H:00"),
                    "month" => ("DATE_TRUNC('month', r.start_time)", "%Y-%m"),
                    _ => ("DATE_TRUNC('day', r.start_time)", "%Y-%m-%d"), // default to day
                };

                // $1 = env_id; $2 = optional cutoff; run_types start at $2 or $3.
                let run_type_start = if time_range_cutoff.is_some() { 3 } else { 2 };
                let run_type_placeholders = (run_type_start..run_type_start + run_types.len())
                    .map(|i| format!("${}", i))
                    .collect::<Vec<_>>()
                    .join(", ");

                let time_clause = if time_range_cutoff.is_some() {
                    "AND r.start_time >= $2 "
                } else {
                    ""
                };

                let query = format!(
                    r#"
                    SELECT
                        {} as time_period,
                        rt.run_type,
                        COUNT(*) as count
                    FROM run r
                    JOIN run_type rt ON r.run_type_id = rt.run_type_id
                    WHERE r.env_id = $1 {}AND rt.run_type IN ({})
                    GROUP BY time_period, rt.run_type
                    ORDER BY time_period ASC, rt.run_type ASC
                    "#,
                    group_by_clause, time_clause, run_type_placeholders
                );

                let mut q = sqlx::query(sqlx::AssertSqlSafe(query.as_str())).bind(env_id);

                if let Some(cutoff) = time_range_cutoff {
                    q = q.bind(cutoff);
                }

                for run_type in run_types {
                    q = q.bind(*run_type);
                }

                let rows = q.fetch_all(pg_pool).await?;

                let mut result = AHashMap::new();
                for row in rows {
                    let time_period: chrono::DateTime<Utc> = row.get("time_period");
                    let run_type: String = row.get("run_type");
                    let count: i64 = row.get("count");

                    let time_str = time_period.format(date_format).to_string();
                    result
                        .entry(time_str)
                        .or_insert_with(AHashMap::new)
                        .insert(run_type, count);
                }
                Ok(result)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let (group_by_clause, _date_format) = match time_unit {
                    "hour" => ("strftime('%Y-%m-%d %H:00', r.start_time)", "%Y-%m-%d %H:00"),
                    "month" => ("strftime('%Y-%m', r.start_time)", "%Y-%m"),
                    _ => ("DATE(r.start_time)", "%Y-%m-%d"), // default to day
                };

                let time_cutoff_str =
                    time_range_cutoff.map(|c| c.format("%Y-%m-%d %H:%M:%S").to_string());
                let time_filter = if time_cutoff_str.is_some() {
                    "r.start_time >= ?"
                } else {
                    "1=1"
                };

                let run_type_placeholders = (1..=run_types.len())
                    .map(|_| "?")
                    .collect::<Vec<_>>()
                    .join(", ");

                let query = format!(
                    r#"
                    SELECT
                        {} as time_period,
                        rt.run_type,
                        COUNT(*) as count
                    FROM run r
                    JOIN run_type rt ON r.run_type_id = rt.run_type_id
                    WHERE r.env_id = ? AND {} AND rt.run_type IN ({})
                    GROUP BY time_period, rt.run_type
                    ORDER BY time_period ASC, rt.run_type ASC
                    "#,
                    group_by_clause, time_filter, run_type_placeholders
                );

                let mut q = sqlx::query(sqlx::AssertSqlSafe(query.as_str())).bind(env_id);
                if let Some(ref c) = time_cutoff_str {
                    q = q.bind(c);
                }
                for run_type in run_types {
                    q = q.bind(*run_type);
                }

                let rows = q.fetch_all(sqlite_pool).await?;

                let mut result = AHashMap::new();
                for row in rows {
                    let time_period: String = row.get("time_period");
                    let run_type: String = row.get("run_type");
                    let count: i64 = row.get("count");

                    result
                        .entry(time_period)
                        .or_insert_with(AHashMap::new)
                        .insert(run_type, count);
                }
                Ok(result)
            }
        }
    }

    /// Get run status chart data with filters for time range, time unit, and run statuses
    pub async fn get_run_status_chart_data_with_filters(
        db: &crate::db::DatabasePool,
        env_id: &Uuid,
        days: i32,
        time_unit: &str,
        statuses: &[&str],
    ) -> Result<AHashMap<String, AHashMap<String, i64>>, RunError> {
        let days_ago = Utc::now() - chrono::Duration::days(days as i64);

        // Create status filter for SQL IN clause
        let status_placeholders = statuses.iter().map(|_| "?").collect::<Vec<_>>().join(", ");

        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let (group_by_clause, date_format) = match time_unit {
                    "hour" => ("DATE_TRUNC('hour', r.start_time)", "%Y-%m-%d %H:00"),
                    "month" => ("DATE_TRUNC('month', r.start_time)", "%Y-%m"),
                    _ => ("DATE_TRUNC('day', r.start_time)", "%Y-%m-%d"), // default to day
                };

                let status_placeholders = (1..=statuses.len())
                    .map(|i| format!("${}", i + 2))
                    .collect::<Vec<_>>()
                    .join(", ");

                let query = format!(
                    r#"
                    SELECT
                        {} as time_period,
                        rs.status,
                        COUNT(*) as count
                    FROM run r
                    JOIN run_status rs ON r.status_id = rs.status_id
                    WHERE r.env_id = $1 AND r.start_time >= $2 AND r.run_type_id != 7 AND rs.status IN ({})
                    GROUP BY time_period, rs.status
                    ORDER BY time_period ASC, rs.status ASC
                    "#,
                    group_by_clause, status_placeholders
                );

                let mut q = sqlx::query(sqlx::AssertSqlSafe(query.as_str()))
                    .bind(env_id)
                    .bind(days_ago);
                for status in statuses {
                    q = q.bind(*status);
                }

                let rows = q.fetch_all(pg_pool).await?;

                let mut result = AHashMap::new();
                for row in rows {
                    let time_period: chrono::DateTime<Utc> = row.get("time_period");
                    let status: String = row.get("status");
                    let count: i64 = row.get("count");

                    let time_str = time_period.format(date_format).to_string();
                    result
                        .entry(time_str)
                        .or_insert_with(AHashMap::new)
                        .insert(status, count);
                }
                Ok(result)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let (group_by_clause, _date_format) = match time_unit {
                    "hour" => ("strftime('%Y-%m-%d %H:00', r.start_time)", "%Y-%m-%d %H:00"),
                    "month" => ("strftime('%Y-%m', r.start_time)", "%Y-%m"),
                    _ => ("DATE(r.start_time)", "%Y-%m-%d"), // default to day
                };

                let query = format!(
                    r#"
                    SELECT
                        {} as time_period,
                        rs.status,
                        COUNT(*) as count
                    FROM run r
                    JOIN run_status rs ON r.status_id = rs.status_id
                    WHERE r.env_id = ? AND r.start_time >= ? AND r.run_type_id != 7 AND rs.status IN ({})
                    GROUP BY time_period, rs.status
                    ORDER BY time_period ASC, rs.status ASC
                    "#,
                    group_by_clause, status_placeholders
                );

                let mut q = sqlx::query(sqlx::AssertSqlSafe(query.as_str()))
                    .bind(env_id)
                    .bind(days_ago);
                for status in statuses {
                    q = q.bind(*status);
                }

                let rows = q.fetch_all(sqlite_pool).await?;

                let mut result = AHashMap::new();
                for row in rows {
                    let time_period: String = row.get("time_period");
                    let status: String = row.get("status");
                    let count: i64 = row.get("count");

                    result
                        .entry(time_period)
                        .or_insert_with(AHashMap::new)
                        .insert(status, count);
                }
                Ok(result)
            }
        }
    }

    /// Get run status chart data with filters for time range, time unit, and run statuses.
    ///
    /// See [`Self::get_run_type_chart_data_with_interval_filters`] for the
    /// meaning of `time_range_cutoff`.
    pub async fn get_run_status_chart_data_with_interval_filters(
        db: &crate::db::DatabasePool,
        env_id: &Uuid,
        time_range_cutoff: Option<chrono::DateTime<chrono::Utc>>,
        time_unit: &str,
        statuses: &[&str],
    ) -> Result<AHashMap<String, AHashMap<String, i64>>, RunError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let (group_by_clause, date_format) = match time_unit {
                    "hour" => ("DATE_TRUNC('hour', r.start_time)", "%Y-%m-%d %H:00"),
                    "month" => ("DATE_TRUNC('month', r.start_time)", "%Y-%m"),
                    _ => ("DATE_TRUNC('day', r.start_time)", "%Y-%m-%d"), // default to day
                };

                let status_start = if time_range_cutoff.is_some() { 3 } else { 2 };
                let status_placeholders = (status_start..status_start + statuses.len())
                    .map(|i| format!("${}", i))
                    .collect::<Vec<_>>()
                    .join(", ");

                let time_clause = if time_range_cutoff.is_some() {
                    "AND r.start_time >= $2 "
                } else {
                    ""
                };

                let query = format!(
                    r#"
                    SELECT
                        {} as time_period,
                        rs.status,
                        COUNT(*) as count
                    FROM run r
                    JOIN run_status rs ON r.status_id = rs.status_id
                    WHERE r.env_id = $1 {}AND rs.status IN ({})
                    GROUP BY time_period, rs.status
                    ORDER BY time_period ASC, rs.status ASC
                    "#,
                    group_by_clause, time_clause, status_placeholders
                );

                let mut q = sqlx::query(sqlx::AssertSqlSafe(query.as_str())).bind(env_id);

                if let Some(cutoff) = time_range_cutoff {
                    q = q.bind(cutoff);
                }

                for status in statuses {
                    q = q.bind(*status);
                }

                let rows = q.fetch_all(pg_pool).await?;

                let mut result = AHashMap::new();
                for row in rows {
                    let time_period: chrono::DateTime<Utc> = row.get("time_period");
                    let status: String = row.get("status");
                    let count: i64 = row.get("count");

                    let time_str = time_period.format(date_format).to_string();
                    result
                        .entry(time_str)
                        .or_insert_with(AHashMap::new)
                        .insert(status, count);
                }
                Ok(result)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let (group_by_clause, _date_format) = match time_unit {
                    "hour" => ("strftime('%Y-%m-%d %H:00', r.start_time)", "%Y-%m-%d %H:00"),
                    "month" => ("strftime('%Y-%m', r.start_time)", "%Y-%m"),
                    _ => ("DATE(r.start_time)", "%Y-%m-%d"), // default to day
                };

                let time_cutoff_str =
                    time_range_cutoff.map(|c| c.format("%Y-%m-%d %H:%M:%S").to_string());
                let time_filter = if time_cutoff_str.is_some() {
                    "r.start_time >= ?"
                } else {
                    "1=1"
                };

                let status_placeholders = (1..=statuses.len())
                    .map(|_| "?")
                    .collect::<Vec<_>>()
                    .join(", ");

                let query = format!(
                    r#"
                    SELECT
                        {} as time_period,
                        rs.status,
                        COUNT(*) as count
                    FROM run r
                    JOIN run_status rs ON r.status_id = rs.status_id
                    WHERE r.env_id = ? AND {} AND rs.status IN ({})
                    GROUP BY time_period, rs.status
                    ORDER BY time_period ASC, rs.status ASC
                    "#,
                    group_by_clause, time_filter, status_placeholders
                );

                let mut q = sqlx::query(sqlx::AssertSqlSafe(query.as_str())).bind(env_id);

                if let Some(ref c) = time_cutoff_str {
                    q = q.bind(c);
                }

                for status in statuses {
                    q = q.bind(*status);
                }

                let rows = q.fetch_all(sqlite_pool).await?;

                let mut result = AHashMap::new();
                for row in rows {
                    let time_period: String = row.get("time_period");
                    let status: String = row.get("status");
                    let count: i64 = row.get("count");

                    result
                        .entry(time_period)
                        .or_insert_with(AHashMap::new)
                        .insert(status, count);
                }
                Ok(result)
            }
        }
    }

    /// Get the earliest run start time for an environment
    pub async fn get_earliest_run_time(
        db: &crate::db::DatabasePool,
        env_id: &Uuid,
    ) -> Result<Option<DateTime<Utc>>, RunError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let earliest: Option<DateTime<Utc>> =
                    sqlx::query_scalar("SELECT MIN(start_time) FROM run WHERE env_id = $1")
                        .bind(env_id)
                        .fetch_one(pg_pool)
                        .await?;
                Ok(earliest)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let earliest: Option<DateTime<Utc>> =
                    sqlx::query_scalar("SELECT MIN(start_time) FROM run WHERE env_id = ?")
                        .bind(env_id)
                        .fetch_one(sqlite_pool)
                        .await?;
                Ok(earliest)
            }
        }
    }

    /// Get run type chart data with cross-filters across run types AND statuses.
    ///
    /// See [`Self::get_run_type_chart_data_with_interval_filters`] for the
    /// meaning of `time_range_cutoff`.
    pub async fn get_run_type_chart_data_with_cross_filters(
        db: &crate::db::DatabasePool,
        env_id: &Uuid,
        time_range_cutoff: Option<chrono::DateTime<chrono::Utc>>,
        time_unit: &str,
        run_types: &[&str],
        statuses: &[&str],
    ) -> Result<AHashMap<String, AHashMap<String, i64>>, RunError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let (group_by_clause, date_format) = match time_unit {
                    "hour" => ("DATE_TRUNC('hour', r.start_time)", "%Y-%m-%d %H:00"),
                    "month" => ("DATE_TRUNC('month', r.start_time)", "%Y-%m"),
                    _ => ("DATE_TRUNC('day', r.start_time)", "%Y-%m-%d"), // default to day
                };

                let mut param_count = 1; // env_id is $1

                if time_range_cutoff.is_some() {
                    param_count += 1; // cutoff is $2
                }

                let run_type_placeholders = (0..run_types.len())
                    .map(|_| {
                        param_count += 1;
                        format!("${}", param_count)
                    })
                    .collect::<Vec<String>>()
                    .join(", ");

                let status_placeholders = (0..statuses.len())
                    .map(|_| {
                        param_count += 1;
                        format!("${}", param_count)
                    })
                    .collect::<Vec<String>>()
                    .join(", ");

                let time_clause = if time_range_cutoff.is_some() {
                    "AND r.start_time >= $2 "
                } else {
                    ""
                };

                let query = format!(
                    r#"
                    SELECT
                        {} as time_period,
                        rt.run_type,
                        COUNT(*) as count
                    FROM run r
                    JOIN run_type rt ON r.run_type_id = rt.run_type_id
                    JOIN run_status rs ON r.status_id = rs.status_id
                    WHERE r.env_id = $1 {}AND rt.run_type IN ({}) AND rs.status IN ({})
                    GROUP BY time_period, rt.run_type
                    ORDER BY time_period ASC, rt.run_type ASC
                    "#,
                    group_by_clause, time_clause, run_type_placeholders, status_placeholders
                );

                let mut q = sqlx::query(sqlx::AssertSqlSafe(query.as_str())).bind(env_id);

                if let Some(cutoff) = time_range_cutoff {
                    q = q.bind(cutoff);
                }

                for run_type in run_types {
                    q = q.bind(*run_type);
                }

                for status in statuses {
                    q = q.bind(*status);
                }

                let rows = q.fetch_all(pg_pool).await?;

                let mut result = AHashMap::new();
                for row in rows {
                    let time_period: chrono::DateTime<Utc> = row.get("time_period");
                    let run_type: String = row.get("run_type");
                    let count: i64 = row.get("count");

                    let time_str = time_period.format(date_format).to_string();
                    result
                        .entry(time_str)
                        .or_insert_with(AHashMap::new)
                        .insert(run_type, count);
                }
                Ok(result)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let (group_by_clause, _date_format) = match time_unit {
                    "hour" => ("strftime('%Y-%m-%d %H:00', r.start_time)", "%Y-%m-%d %H:00"),
                    "month" => ("strftime('%Y-%m', r.start_time)", "%Y-%m"),
                    _ => ("DATE(r.start_time)", "%Y-%m-%d"), // default to day
                };

                let time_cutoff_str =
                    time_range_cutoff.map(|c| c.format("%Y-%m-%d %H:%M:%S").to_string());
                let time_filter = if time_cutoff_str.is_some() {
                    "r.start_time >= ?"
                } else {
                    "1=1"
                };

                let run_type_placeholders = (0..run_types.len())
                    .map(|_| "?")
                    .collect::<Vec<&str>>()
                    .join(", ");

                let status_placeholders = (0..statuses.len())
                    .map(|_| "?")
                    .collect::<Vec<&str>>()
                    .join(", ");

                let query = format!(
                    r#"
                    SELECT
                        {} as time_period,
                        rt.run_type,
                        COUNT(*) as count
                    FROM run r
                    JOIN run_type rt ON r.run_type_id = rt.run_type_id
                    JOIN run_status rs ON r.status_id = rs.status_id
                    WHERE r.env_id = ? AND {} AND rt.run_type IN ({}) AND rs.status IN ({})
                    GROUP BY time_period, rt.run_type
                    ORDER BY time_period ASC, rt.run_type ASC
                    "#,
                    group_by_clause, time_filter, run_type_placeholders, status_placeholders
                );

                let mut q = sqlx::query(sqlx::AssertSqlSafe(query.as_str())).bind(env_id);

                if let Some(ref c) = time_cutoff_str {
                    q = q.bind(c);
                }

                for run_type in run_types {
                    q = q.bind(*run_type);
                }

                for status in statuses {
                    q = q.bind(*status);
                }

                let rows = q.fetch_all(sqlite_pool).await?;

                let mut result = AHashMap::new();
                for row in rows {
                    let time_period: String = row.get("time_period");
                    let run_type: String = row.get("run_type");
                    let count: i64 = row.get("count");

                    result
                        .entry(time_period)
                        .or_insert_with(AHashMap::new)
                        .insert(run_type, count);
                }
                Ok(result)
            }
        }
    }

    /// Get run status chart data with cross-filters across statuses AND run types.
    ///
    /// See [`Self::get_run_type_chart_data_with_interval_filters`] for the
    /// meaning of `time_range_cutoff`.
    pub async fn get_run_status_chart_data_with_cross_filters(
        db: &crate::db::DatabasePool,
        env_id: &Uuid,
        time_range_cutoff: Option<chrono::DateTime<chrono::Utc>>,
        time_unit: &str,
        statuses: &[&str],
        run_types: &[&str],
    ) -> Result<AHashMap<String, AHashMap<String, i64>>, RunError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let (group_by_clause, date_format) = match time_unit {
                    "hour" => ("DATE_TRUNC('hour', r.start_time)", "%Y-%m-%d %H:00"),
                    "month" => ("DATE_TRUNC('month', r.start_time)", "%Y-%m"),
                    _ => ("DATE_TRUNC('day', r.start_time)", "%Y-%m-%d"), // default to day
                };

                let mut param_count = 1; // env_id is $1

                if time_range_cutoff.is_some() {
                    param_count += 1; // cutoff is $2
                }

                let status_placeholders = (0..statuses.len())
                    .map(|_| {
                        param_count += 1;
                        format!("${}", param_count)
                    })
                    .collect::<Vec<String>>()
                    .join(", ");

                let run_type_placeholders = (0..run_types.len())
                    .map(|_| {
                        param_count += 1;
                        format!("${}", param_count)
                    })
                    .collect::<Vec<String>>()
                    .join(", ");

                let time_clause = if time_range_cutoff.is_some() {
                    "AND r.start_time >= $2 "
                } else {
                    ""
                };

                let query = format!(
                    r#"
                    SELECT
                        {} as time_period,
                        rs.status,
                        COUNT(*) as count
                    FROM run r
                    JOIN run_status rs ON r.status_id = rs.status_id
                    JOIN run_type rt ON r.run_type_id = rt.run_type_id
                    WHERE r.env_id = $1 {}AND rs.status IN ({}) AND rt.run_type IN ({})
                    GROUP BY time_period, rs.status
                    ORDER BY time_period ASC, rs.status ASC
                    "#,
                    group_by_clause, time_clause, status_placeholders, run_type_placeholders
                );

                let mut q = sqlx::query(sqlx::AssertSqlSafe(query.as_str())).bind(env_id);

                if let Some(cutoff) = time_range_cutoff {
                    q = q.bind(cutoff);
                }

                for status in statuses {
                    q = q.bind(*status);
                }

                for run_type in run_types {
                    q = q.bind(*run_type);
                }

                let rows = q.fetch_all(pg_pool).await?;

                let mut result = AHashMap::new();
                for row in rows {
                    let time_period: chrono::DateTime<Utc> = row.get("time_period");
                    let status: String = row.get("status");
                    let count: i64 = row.get("count");

                    let time_str = time_period.format(date_format).to_string();
                    result
                        .entry(time_str)
                        .or_insert_with(AHashMap::new)
                        .insert(status, count);
                }
                Ok(result)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let (group_by_clause, _date_format) = match time_unit {
                    "hour" => ("strftime('%Y-%m-%d %H:00', r.start_time)", "%Y-%m-%d %H:00"),
                    "month" => ("strftime('%Y-%m', r.start_time)", "%Y-%m"),
                    _ => ("DATE(r.start_time)", "%Y-%m-%d"), // default to day
                };

                let time_cutoff_str =
                    time_range_cutoff.map(|c| c.format("%Y-%m-%d %H:%M:%S").to_string());
                let time_filter = if time_cutoff_str.is_some() {
                    "r.start_time >= ?"
                } else {
                    "1=1"
                };

                let status_placeholders = (0..statuses.len())
                    .map(|_| "?")
                    .collect::<Vec<_>>()
                    .join(", ");

                let run_type_placeholders = (0..run_types.len())
                    .map(|_| "?")
                    .collect::<Vec<_>>()
                    .join(", ");

                let query = format!(
                    r#"
                    SELECT
                        {} as time_period,
                        rs.status,
                        COUNT(*) as count
                    FROM run r
                    JOIN run_status rs ON r.status_id = rs.status_id
                    JOIN run_type rt ON r.run_type_id = rt.run_type_id
                    WHERE r.env_id = ? AND {} AND rs.status IN ({}) AND rt.run_type IN ({})
                    GROUP BY time_period, rs.status
                    ORDER BY time_period ASC, rs.status ASC
                    "#,
                    group_by_clause, time_filter, status_placeholders, run_type_placeholders
                );

                let mut q = sqlx::query(sqlx::AssertSqlSafe(query.as_str())).bind(env_id);

                if let Some(ref c) = time_cutoff_str {
                    q = q.bind(c);
                }

                for status in statuses {
                    q = q.bind(*status);
                }

                for run_type in run_types {
                    q = q.bind(*run_type);
                }

                let rows = q.fetch_all(sqlite_pool).await?;

                let mut result = AHashMap::new();
                for row in rows {
                    let time_period: String = row.get("time_period");
                    let status: String = row.get("status");
                    let count: i64 = row.get("count");

                    result
                        .entry(time_period)
                        .or_insert_with(AHashMap::new)
                        .insert(status, count);
                }
                Ok(result)
            }
        }
    }

    /// Get runs by stream_id with pagination
    pub async fn get_runs_by_stream(
        db: &crate::db::DatabasePool,
        stream_id: &Uuid,
        env_id: &Uuid,
        limit: Option<i64>,
        offset: Option<i64>,
    ) -> Result<Vec<Run>, RunError> {
        let limit = limit.unwrap_or(50);
        let offset = offset.unwrap_or(0);

        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let runs = sqlx::query_as::<_, Run>(
                    "SELECT r.run_id, r.env_id, r.stream_id, r.build_id, r.run_type_id, rt.run_type, r.origin_run_id, r.event_id, r.start_time, r.stop_time, r.status_id, rs.status, r.by_user_id, r.result, r.info, r.retry_attempt, r.next_retry_at, b.project_id, p.name as project_name, COALESCE(e.event_data->>'fn', (SELECT function_name FROM call WHERE run_id = r.run_id AND parent_call_id IS NULL LIMIT 1), (SELECT function_name FROM task WHERE run_id = r.run_id LIMIT 1)) as event_fn, e.created_at as queued_at FROM run r JOIN run_status rs ON r.status_id = rs.status_id JOIN run_type rt ON r.run_type_id = rt.run_type_id LEFT JOIN build b ON r.build_id = b.build_id LEFT JOIN project p ON b.project_id = p.project_id LEFT JOIN event e ON r.event_id = e.event_id WHERE r.stream_id = $1 AND r.env_id = $2 ORDER BY r.start_time DESC LIMIT $3 OFFSET $4"
                )
                .bind(stream_id)
                .bind(env_id)
                .bind(limit)
                .bind(offset)
                .fetch_all(pg_pool)
                .await?;
                Ok(runs)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let runs = sqlx::query_as::<_, Run>(
                    "SELECT r.run_id, r.env_id, r.stream_id, r.build_id, r.run_type_id, rt.run_type, r.origin_run_id, r.event_id, r.start_time, r.stop_time, r.status_id, rs.status, r.by_user_id, r.result, r.info, r.retry_attempt, r.next_retry_at, b.project_id, p.name as project_name, COALESCE(json_extract(e.event_data, '$.fn'), (SELECT function_name FROM call WHERE run_id = r.run_id AND parent_call_id IS NULL LIMIT 1), (SELECT function_name FROM task WHERE run_id = r.run_id LIMIT 1)) as event_fn, e.created_at as queued_at FROM run r JOIN run_status rs ON r.status_id = rs.status_id JOIN run_type rt ON r.run_type_id = rt.run_type_id LEFT JOIN build b ON r.build_id = b.build_id LEFT JOIN project p ON b.project_id = p.project_id LEFT JOIN event e ON r.event_id = e.event_id WHERE r.stream_id = ? AND r.env_id = ? ORDER BY r.start_time DESC LIMIT ? OFFSET ?"
                )
                .bind(stream_id)
                .bind(env_id)
                .bind(limit)
                .bind(offset)
                .fetch_all(sqlite_pool)
                .await?;
                Ok(runs)
            }
        }
    }

    /// Get count of runs by stream_id
    pub async fn get_count_by_stream(
        db: &crate::db::DatabasePool,
        stream_id: &Uuid,
        env_id: &Uuid,
    ) -> Result<i64, RunError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let count: i64 = sqlx::query_scalar(
                    "SELECT COUNT(*) FROM run WHERE stream_id = $1 AND env_id = $2",
                )
                .bind(stream_id)
                .bind(env_id)
                .fetch_one(pg_pool)
                .await?;
                Ok(count)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let count: i64 = sqlx::query_scalar(
                    "SELECT COUNT(*) FROM run WHERE stream_id = ? AND env_id = ?",
                )
                .bind(stream_id)
                .bind(env_id)
                .fetch_one(sqlite_pool)
                .await?;
                Ok(count)
            }
        }
    }

    /// Get runs that are pending retry and ready to be retried (next_retry_at <= now)
    /// Limited to batch_size runs to avoid overwhelming the system
    pub async fn get_pending_retries(
        db: &crate::db::DatabasePool,
        batch_size: i64,
    ) -> Result<Vec<Run>, RunError> {
        let now = chrono::Utc::now();

        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let runs = sqlx::query_as::<_, Run>(
                    "SELECT r.run_id, r.env_id, r.stream_id, r.build_id, r.run_type_id, rt.run_type, r.origin_run_id, r.event_id, r.start_time, r.stop_time, r.status_id, rs.status, r.by_user_id, r.result, r.info, r.retry_attempt, r.next_retry_at, b.project_id, p.name as project_name, COALESCE(e.event_data->>'fn', (SELECT function_name FROM call WHERE run_id = r.run_id AND parent_call_id IS NULL LIMIT 1), (SELECT function_name FROM task WHERE run_id = r.run_id LIMIT 1)) as event_fn, e.created_at as queued_at FROM run r JOIN run_status rs ON r.status_id = rs.status_id JOIN run_type rt ON r.run_type_id = rt.run_type_id LEFT JOIN build b ON r.build_id = b.build_id LEFT JOIN project p ON b.project_id = p.project_id LEFT JOIN event e ON r.event_id = e.event_id WHERE r.status_id = 5 AND r.next_retry_at IS NOT NULL AND r.next_retry_at <= $1 ORDER BY r.next_retry_at ASC LIMIT $2"
                )
                .bind(now)
                .bind(batch_size)
                .fetch_all(pg_pool)
                .await?;
                Ok(runs)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let runs = sqlx::query_as::<_, Run>(
                    "SELECT r.run_id, r.env_id, r.stream_id, r.build_id, r.run_type_id, rt.run_type, r.origin_run_id, r.event_id, r.start_time, r.stop_time, r.status_id, rs.status, r.by_user_id, r.result, r.info, r.retry_attempt, r.next_retry_at, b.project_id, p.name as project_name, COALESCE(json_extract(e.event_data, '$.fn'), (SELECT function_name FROM call WHERE run_id = r.run_id AND parent_call_id IS NULL LIMIT 1), (SELECT function_name FROM task WHERE run_id = r.run_id LIMIT 1)) as event_fn, e.created_at as queued_at FROM run r JOIN run_status rs ON r.status_id = rs.status_id JOIN run_type rt ON r.run_type_id = rt.run_type_id LEFT JOIN build b ON r.build_id = b.build_id LEFT JOIN project p ON b.project_id = p.project_id LEFT JOIN event e ON r.event_id = e.event_id WHERE r.status_id = 5 AND r.next_retry_at IS NOT NULL AND r.next_retry_at <= ? ORDER BY r.next_retry_at ASC LIMIT ?"
                )
                .bind(now)
                .bind(batch_size)
                .fetch_all(sqlite_pool)
                .await?;
                Ok(runs)
            }
        }
    }

    /// Get runs for a specific agent_type (qualified name) with pagination.
    pub async fn get_runs_by_agent_type(
        db: &crate::db::DatabasePool,
        env_id: &Uuid,
        agent_type: &str,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<Run>, RunError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let runs = sqlx::query_as::<_, Run>(
                    "SELECT r.run_id, r.env_id, r.stream_id, r.build_id, r.run_type_id, rt.run_type, r.origin_run_id, r.event_id, r.start_time, r.stop_time, r.status_id, rs.status, r.by_user_id, r.result, r.info, r.retry_attempt, r.next_retry_at, b.project_id, p.name as project_name, COALESCE(e.event_data->>'fn', (SELECT function_name FROM call WHERE run_id = r.run_id AND parent_call_id IS NULL LIMIT 1), (SELECT function_name FROM task WHERE run_id = r.run_id LIMIT 1)) as event_fn, e.created_at as queued_at, r.access_id, r.agent_type FROM run r JOIN run_status rs ON r.status_id = rs.status_id JOIN run_type rt ON r.run_type_id = rt.run_type_id LEFT JOIN build b ON r.build_id = b.build_id LEFT JOIN project p ON b.project_id = p.project_id LEFT JOIN event e ON r.event_id = e.event_id WHERE r.env_id = $1 AND r.agent_type = $2 ORDER BY r.start_time DESC LIMIT $3 OFFSET $4",
                )
                .bind(env_id)
                .bind(agent_type)
                .bind(limit)
                .bind(offset)
                .fetch_all(pg_pool)
                .await?;
                Ok(runs)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let runs = sqlx::query_as::<_, Run>(
                    "SELECT r.run_id, r.env_id, r.stream_id, r.build_id, r.run_type_id, rt.run_type, r.origin_run_id, r.event_id, r.start_time, r.stop_time, r.status_id, rs.status, r.by_user_id, r.result, r.info, r.retry_attempt, r.next_retry_at, b.project_id, p.name as project_name, COALESCE(json_extract(e.event_data, '$.fn'), (SELECT function_name FROM call WHERE run_id = r.run_id AND parent_call_id IS NULL LIMIT 1), (SELECT function_name FROM task WHERE run_id = r.run_id LIMIT 1)) as event_fn, e.created_at as queued_at, r.access_id, r.agent_type FROM run r JOIN run_status rs ON r.status_id = rs.status_id JOIN run_type rt ON r.run_type_id = rt.run_type_id LEFT JOIN build b ON r.build_id = b.build_id LEFT JOIN project p ON b.project_id = p.project_id LEFT JOIN event e ON r.event_id = e.event_id WHERE r.env_id = ? AND r.agent_type = ? ORDER BY r.start_time DESC LIMIT ? OFFSET ?",
                )
                .bind(env_id)
                .bind(agent_type)
                .bind(limit)
                .bind(offset)
                .fetch_all(sqlite_pool)
                .await?;
                Ok(runs)
            }
        }
    }

    /// Count runs for a specific agent_type.
    pub async fn get_count_by_agent_type(
        db: &crate::db::DatabasePool,
        env_id: &Uuid,
        agent_type: &str,
    ) -> Result<i64, RunError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let count = sqlx::query_scalar::<_, i64>(
                    "SELECT COUNT(*) FROM run WHERE env_id = $1 AND agent_type = $2",
                )
                .bind(env_id)
                .bind(agent_type)
                .fetch_one(pg_pool)
                .await?;
                Ok(count)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let count = sqlx::query_scalar::<_, i64>(
                    "SELECT COUNT(*) FROM run WHERE env_id = ? AND agent_type = ?",
                )
                .bind(env_id)
                .bind(agent_type)
                .fetch_one(sqlite_pool)
                .await?;
                Ok(count)
            }
        }
    }

    /// Count distinct stream_ids for a specific agent_type (recent 24h).
    pub async fn count_active_streams_by_agent_type(
        db: &crate::db::DatabasePool,
        env_id: &Uuid,
        agent_type: &str,
    ) -> Result<i64, RunError> {
        let since = chrono::Utc::now() - chrono::Duration::hours(24);
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let count = sqlx::query_scalar::<_, i64>(
                    "SELECT COUNT(DISTINCT stream_id) FROM run WHERE env_id = $1 AND agent_type = $2 AND start_time >= $3",
                )
                .bind(env_id)
                .bind(agent_type)
                .bind(since)
                .fetch_one(pg_pool)
                .await?;
                Ok(count)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let count = sqlx::query_scalar::<_, i64>(
                    "SELECT COUNT(DISTINCT stream_id) FROM run WHERE env_id = ? AND agent_type = ? AND start_time >= ?",
                )
                .bind(env_id)
                .bind(agent_type)
                .bind(since)
                .fetch_one(sqlite_pool)
                .await?;
                Ok(count)
            }
        }
    }

    /// Mark a pending retry run as failed (exhausted retries or manual cancellation)
    pub async fn mark_retry_as_failed(
        db: &crate::db::DatabasePool,
        run_id: &Uuid,
    ) -> Result<(), RunError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                sqlx::query(
                    "UPDATE hot.run SET status_id = 3, next_retry_at = NULL WHERE run_id = $1",
                )
                .bind(run_id)
                .execute(pg_pool)
                .await?;
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                sqlx::query("UPDATE run SET status_id = 3, next_retry_at = NULL WHERE run_id = ?")
                    .bind(run_id)
                    .execute(sqlite_pool)
                    .await?;
            }
        }
        Ok(())
    }
}

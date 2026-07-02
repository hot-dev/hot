//! Database access for content-addressed blob objects and blob references.
//!
//! `blob_object` stores one row per unique content hash per org/env and points
//! at bytes in file storage. `blob_ref` is the authoritative liveness table:
//! an object may be physically deleted only when it has no active refs and its
//! `last_referenced_at` is older than the GC grace window.

use chrono::{DateTime, Utc};
use serde_json::Value as JsonValue;
use sqlx::FromRow;
use thiserror::Error;
use uuid::Uuid;

use super::DatabasePool;

#[derive(Error, Debug)]
pub enum BlobDbError {
    #[error("Database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("Blob object not found")]
    ObjectNotFound,
    #[error("Blob ref not found")]
    RefNotFound,
}

/// Lifecycle status of a blob object's bytes in file storage.
pub mod object_status {
    pub const PENDING: &str = "pending";
    pub const AVAILABLE: &str = "available";
    pub const DELETE_PENDING: &str = "delete_pending";
    pub const DELETED: &str = "deleted";
}

#[derive(Debug, Clone, FromRow)]
pub struct BlobObjectRecord {
    pub blob_object_id: Uuid,
    pub org_id: Uuid,
    pub env_id: Option<Uuid>,
    pub hash_alg: String,
    pub hash: String,
    pub size: i64,
    pub content_type: Option<String>,
    pub storage_backend: String,
    pub storage_path: String,
    pub status: String,
    pub created_at: DateTime<Utc>,
    pub last_referenced_at: DateTime<Utc>,
}

#[derive(Debug, Clone, FromRow)]
pub struct BlobRefRecord {
    pub blob_ref_id: Uuid,
    pub blob_object_id: Uuid,
    pub org_id: Uuid,
    pub env_id: Option<Uuid>,
    pub source_kind: String,
    pub source_id: Option<String>,
    pub json_paths: Option<JsonValue>,
    pub created_by_run_id: Option<Uuid>,
    pub active: bool,
    pub created_at: DateTime<Utc>,
    pub expires_at: Option<DateTime<Utc>>,
    pub deactivated_at: Option<DateTime<Utc>>,
}

const OBJECT_COLUMNS: &str = "blob_object_id, org_id, env_id, hash_alg, hash, size, content_type, \
     storage_backend, storage_path, status, created_at, last_referenced_at";

const REF_COLUMNS: &str = "blob_ref_id, blob_object_id, org_id, env_id, source_kind, source_id, \
     json_paths, created_by_run_id, active, created_at, expires_at, deactivated_at";

/// Find an existing object by content hash within an org/env scope.
pub async fn get_object_by_hash(
    db: &DatabasePool,
    org_id: Uuid,
    env_id: Option<Uuid>,
    hash_alg: &str,
    hash: &str,
) -> Result<Option<BlobObjectRecord>, BlobDbError> {
    match db {
        DatabasePool::Postgres(pool) => {
            let query = format!(
                "SELECT {OBJECT_COLUMNS} FROM blob_object \
                 WHERE org_id = $1 AND env_id IS NOT DISTINCT FROM $2 \
                 AND hash_alg = $3 AND hash = $4"
            );
            Ok(
                sqlx::query_as::<_, BlobObjectRecord>(sqlx::AssertSqlSafe(query.as_str()))
                    .bind(org_id)
                    .bind(env_id)
                    .bind(hash_alg)
                    .bind(hash)
                    .fetch_optional(pool)
                    .await?,
            )
        }
        DatabasePool::Sqlite(pool) => {
            let query = format!(
                "SELECT {OBJECT_COLUMNS} FROM blob_object \
                 WHERE org_id = ? AND (env_id IS ? OR env_id = ?) \
                 AND hash_alg = ? AND hash = ?"
            );
            Ok(
                sqlx::query_as::<_, BlobObjectRecord>(sqlx::AssertSqlSafe(query.as_str()))
                    .bind(org_id)
                    .bind(env_id)
                    .bind(env_id)
                    .bind(hash_alg)
                    .bind(hash)
                    .fetch_optional(pool)
                    .await?,
            )
        }
    }
}

/// Insert a new blob object in `pending` status.
#[allow(clippy::too_many_arguments)]
pub async fn insert_pending_object(
    db: &DatabasePool,
    org_id: Uuid,
    env_id: Option<Uuid>,
    hash_alg: &str,
    hash: &str,
    size: i64,
    content_type: Option<&str>,
    storage_backend: &str,
    storage_path: &str,
) -> Result<BlobObjectRecord, BlobDbError> {
    let blob_object_id = Uuid::now_v7();
    let now = Utc::now();

    match db {
        DatabasePool::Postgres(pool) => {
            let query = format!(
                "INSERT INTO blob_object ( \
                     blob_object_id, org_id, env_id, hash_alg, hash, size, content_type, \
                     storage_backend, storage_path, status, created_at, last_referenced_at \
                 ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, '{}', $10, $10) \
                 RETURNING {OBJECT_COLUMNS}",
                object_status::PENDING
            );
            Ok(
                sqlx::query_as::<_, BlobObjectRecord>(sqlx::AssertSqlSafe(query.as_str()))
                    .bind(blob_object_id)
                    .bind(org_id)
                    .bind(env_id)
                    .bind(hash_alg)
                    .bind(hash)
                    .bind(size)
                    .bind(content_type)
                    .bind(storage_backend)
                    .bind(storage_path)
                    .bind(now)
                    .fetch_one(pool)
                    .await?,
            )
        }
        DatabasePool::Sqlite(pool) => {
            let query = format!(
                "INSERT INTO blob_object ( \
                     blob_object_id, org_id, env_id, hash_alg, hash, size, content_type, \
                     storage_backend, storage_path, status, created_at, last_referenced_at \
                 ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, '{}', ?, ?)",
                object_status::PENDING
            );
            sqlx::query(sqlx::AssertSqlSafe(query.as_str()))
                .bind(blob_object_id)
                .bind(org_id)
                .bind(env_id)
                .bind(hash_alg)
                .bind(hash)
                .bind(size)
                .bind(content_type)
                .bind(storage_backend)
                .bind(storage_path)
                .bind(now)
                .bind(now)
                .execute(pool)
                .await?;
            get_object_by_id(db, blob_object_id)
                .await?
                .ok_or(BlobDbError::ObjectNotFound)
        }
    }
}

pub async fn get_object_by_id(
    db: &DatabasePool,
    blob_object_id: Uuid,
) -> Result<Option<BlobObjectRecord>, BlobDbError> {
    match db {
        DatabasePool::Postgres(pool) => {
            let query =
                format!("SELECT {OBJECT_COLUMNS} FROM blob_object WHERE blob_object_id = $1");
            Ok(
                sqlx::query_as::<_, BlobObjectRecord>(sqlx::AssertSqlSafe(query.as_str()))
                    .bind(blob_object_id)
                    .fetch_optional(pool)
                    .await?,
            )
        }
        DatabasePool::Sqlite(pool) => {
            let query =
                format!("SELECT {OBJECT_COLUMNS} FROM blob_object WHERE blob_object_id = ?");
            Ok(
                sqlx::query_as::<_, BlobObjectRecord>(sqlx::AssertSqlSafe(query.as_str()))
                    .bind(blob_object_id)
                    .fetch_optional(pool)
                    .await?,
            )
        }
    }
}

/// Update object status (`pending` -> `available`, `available` -> `delete_pending`, ...).
pub async fn set_object_status(
    db: &DatabasePool,
    blob_object_id: Uuid,
    status: &str,
) -> Result<(), BlobDbError> {
    match db {
        DatabasePool::Postgres(pool) => {
            sqlx::query("UPDATE blob_object SET status = $1 WHERE blob_object_id = $2")
                .bind(status)
                .bind(blob_object_id)
                .execute(pool)
                .await?;
        }
        DatabasePool::Sqlite(pool) => {
            sqlx::query("UPDATE blob_object SET status = ? WHERE blob_object_id = ?")
                .bind(status)
                .bind(blob_object_id)
                .execute(pool)
                .await?;
        }
    }
    Ok(())
}

/// Touch `last_referenced_at` so GC grace-window checks see recent use.
/// Must be called before inserting a ref on a dedupe hit to close the
/// dedupe-vs-GC race.
pub async fn touch_object(db: &DatabasePool, blob_object_id: Uuid) -> Result<(), BlobDbError> {
    let now = Utc::now();
    match db {
        DatabasePool::Postgres(pool) => {
            sqlx::query("UPDATE blob_object SET last_referenced_at = $1 WHERE blob_object_id = $2")
                .bind(now)
                .bind(blob_object_id)
                .execute(pool)
                .await?;
        }
        DatabasePool::Sqlite(pool) => {
            sqlx::query("UPDATE blob_object SET last_referenced_at = ? WHERE blob_object_id = ?")
                .bind(now)
                .bind(blob_object_id)
                .execute(pool)
                .await?;
        }
    }
    Ok(())
}

/// Insert a blob ref, or return the existing active ref for the same
/// (org, env, source_kind, source_id, object) tuple.
#[allow(clippy::too_many_arguments)]
pub async fn insert_ref(
    db: &DatabasePool,
    blob_object_id: Uuid,
    org_id: Uuid,
    env_id: Option<Uuid>,
    source_kind: &str,
    source_id: Option<&str>,
    json_paths: Option<&JsonValue>,
    created_by_run_id: Option<Uuid>,
) -> Result<BlobRefRecord, BlobDbError> {
    let blob_ref_id = Uuid::now_v7();
    let now = Utc::now();

    match db {
        DatabasePool::Postgres(pool) => {
            let query = format!(
                "INSERT INTO blob_ref ( \
                     blob_ref_id, blob_object_id, org_id, env_id, source_kind, source_id, \
                     json_paths, created_by_run_id, active, created_at \
                 ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, true, $9) \
                 ON CONFLICT (org_id, COALESCE(env_id, '00000000-0000-0000-0000-000000000000'::uuid), source_kind, COALESCE(source_id, ''), blob_object_id) \
                 DO UPDATE SET active = true, deactivated_at = NULL \
                 RETURNING {REF_COLUMNS}"
            );
            Ok(
                sqlx::query_as::<_, BlobRefRecord>(sqlx::AssertSqlSafe(query.as_str()))
                    .bind(blob_ref_id)
                    .bind(blob_object_id)
                    .bind(org_id)
                    .bind(env_id)
                    .bind(source_kind)
                    .bind(source_id)
                    .bind(json_paths)
                    .bind(created_by_run_id)
                    .bind(now)
                    .fetch_one(pool)
                    .await?,
            )
        }
        DatabasePool::Sqlite(pool) => {
            let json_paths_text = json_paths.map(|v| v.to_string());
            let query = "INSERT INTO blob_ref ( \
                     blob_ref_id, blob_object_id, org_id, env_id, source_kind, source_id, \
                     json_paths, created_by_run_id, active, created_at \
                 ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, 1, ?) \
                 ON CONFLICT (org_id, COALESCE(env_id, X''), source_kind, COALESCE(source_id, ''), blob_object_id) \
                 DO UPDATE SET active = 1, deactivated_at = NULL";
            sqlx::query(query)
                .bind(blob_ref_id)
                .bind(blob_object_id)
                .bind(org_id)
                .bind(env_id)
                .bind(source_kind)
                .bind(source_id)
                .bind(json_paths_text)
                .bind(created_by_run_id)
                .bind(now)
                .execute(pool)
                .await?;
            // The insert may have hit the unique conflict, so re-select by the
            // dedupe key rather than by blob_ref_id.
            let select = format!(
                "SELECT {REF_COLUMNS} FROM blob_ref \
                 WHERE org_id = ? AND (env_id IS ? OR env_id = ?) \
                 AND source_kind = ? AND COALESCE(source_id, '') = COALESCE(?, '') \
                 AND blob_object_id = ?"
            );
            sqlx::query_as::<_, BlobRefRecord>(sqlx::AssertSqlSafe(select.as_str()))
                .bind(org_id)
                .bind(env_id)
                .bind(env_id)
                .bind(source_kind)
                .bind(source_id)
                .bind(blob_object_id)
                .fetch_optional(pool)
                .await?
                .ok_or(BlobDbError::RefNotFound)
        }
    }
}

pub async fn get_ref_by_id(
    db: &DatabasePool,
    blob_ref_id: Uuid,
) -> Result<Option<BlobRefRecord>, BlobDbError> {
    match db {
        DatabasePool::Postgres(pool) => {
            let query = format!("SELECT {REF_COLUMNS} FROM blob_ref WHERE blob_ref_id = $1");
            Ok(
                sqlx::query_as::<_, BlobRefRecord>(sqlx::AssertSqlSafe(query.as_str()))
                    .bind(blob_ref_id)
                    .fetch_optional(pool)
                    .await?,
            )
        }
        DatabasePool::Sqlite(pool) => {
            let query = format!("SELECT {REF_COLUMNS} FROM blob_ref WHERE blob_ref_id = ?");
            Ok(
                sqlx::query_as::<_, BlobRefRecord>(sqlx::AssertSqlSafe(query.as_str()))
                    .bind(blob_ref_id)
                    .fetch_optional(pool)
                    .await?,
            )
        }
    }
}

/// Count active refs pointing at an object.
pub async fn count_active_refs(
    db: &DatabasePool,
    blob_object_id: Uuid,
) -> Result<i64, BlobDbError> {
    match db {
        DatabasePool::Postgres(pool) => {
            let (count,): (i64,) = sqlx::query_as(
                "SELECT COUNT(*) FROM blob_ref WHERE blob_object_id = $1 AND active = true",
            )
            .bind(blob_object_id)
            .fetch_one(pool)
            .await?;
            Ok(count)
        }
        DatabasePool::Sqlite(pool) => {
            let (count,): (i64,) = sqlx::query_as(
                "SELECT COUNT(*) FROM blob_ref WHERE blob_object_id = ? AND active = 1",
            )
            .bind(blob_object_id)
            .fetch_one(pool)
            .await?;
            Ok(count)
        }
    }
}

/// Deactivate all refs for a given source (e.g. when a call row is deleted by
/// retention or a store value is overwritten).
pub async fn deactivate_refs_by_source(
    db: &DatabasePool,
    org_id: Uuid,
    env_id: Option<Uuid>,
    source_kind: &str,
    source_id: &str,
) -> Result<u64, BlobDbError> {
    let now = Utc::now();
    match db {
        DatabasePool::Postgres(pool) => {
            let result = sqlx::query(
                "UPDATE blob_ref SET active = false, deactivated_at = $1 \
                 WHERE org_id = $2 AND env_id IS NOT DISTINCT FROM $3 \
                 AND source_kind = $4 AND source_id = $5 AND active = true",
            )
            .bind(now)
            .bind(org_id)
            .bind(env_id)
            .bind(source_kind)
            .bind(source_id)
            .execute(pool)
            .await?;
            Ok(result.rows_affected())
        }
        DatabasePool::Sqlite(pool) => {
            let result = sqlx::query(
                "UPDATE blob_ref SET active = 0, deactivated_at = ? \
                 WHERE org_id = ? AND (env_id IS ? OR env_id = ?) \
                 AND source_kind = ? AND source_id = ? AND active = 1",
            )
            .bind(now)
            .bind(org_id)
            .bind(env_id)
            .bind(env_id)
            .bind(source_kind)
            .bind(source_id)
            .execute(pool)
            .await?;
            Ok(result.rows_affected())
        }
    }
}

/// Deactivate observability refs created before a cutoff for the given
/// source kinds. Used by retention cleanup (e.g. call retention) to release
/// blob refs when their source rows age out. Never pass `store_value` or
/// `manual` kinds here: durable user data is exempt from time-based GC.
pub async fn deactivate_refs_older_than(
    db: &DatabasePool,
    org_id: Option<Uuid>,
    source_kinds: &[&str],
    cutoff: DateTime<Utc>,
) -> Result<u64, BlobDbError> {
    if source_kinds.is_empty() {
        return Ok(0);
    }
    let now = Utc::now();
    match db {
        DatabasePool::Postgres(pool) => {
            let kind_params: Vec<String> = (0..source_kinds.len())
                .map(|i| format!("${}", i + 3))
                .collect();
            let org_clause = if org_id.is_some() {
                format!("AND org_id = ${}", source_kinds.len() + 3)
            } else {
                String::new()
            };
            let query = format!(
                "UPDATE blob_ref SET active = false, deactivated_at = $1 \
                 WHERE active = true AND created_at < $2 \
                 AND source_kind IN ({}) {}",
                kind_params.join(", "),
                org_clause
            );
            let mut q = sqlx::query(sqlx::AssertSqlSafe(query.as_str()))
                .bind(now)
                .bind(cutoff);
            for kind in source_kinds {
                q = q.bind(*kind);
            }
            if let Some(org_id) = org_id {
                q = q.bind(org_id);
            }
            Ok(q.execute(pool).await?.rows_affected())
        }
        DatabasePool::Sqlite(pool) => {
            let kind_params: Vec<&str> = source_kinds.iter().map(|_| "?").collect();
            let org_clause = if org_id.is_some() {
                "AND org_id = ?"
            } else {
                ""
            };
            let query = format!(
                "UPDATE blob_ref SET active = 0, deactivated_at = ? \
                 WHERE active = 1 AND created_at < ? \
                 AND source_kind IN ({}) {}",
                kind_params.join(", "),
                org_clause
            );
            let mut q = sqlx::query(sqlx::AssertSqlSafe(query.as_str()))
                .bind(now)
                .bind(cutoff);
            for kind in source_kinds {
                q = q.bind(*kind);
            }
            if let Some(org_id) = org_id {
                q = q.bind(org_id);
            }
            Ok(q.execute(pool).await?.rows_affected())
        }
    }
}

/// Deactivate refs whose `expires_at` has passed.
pub async fn deactivate_expired_refs(db: &DatabasePool) -> Result<u64, BlobDbError> {
    let now = Utc::now();
    match db {
        DatabasePool::Postgres(pool) => {
            let result = sqlx::query(
                "UPDATE blob_ref SET active = false, deactivated_at = $1 \
                 WHERE active = true AND expires_at IS NOT NULL AND expires_at < $1",
            )
            .bind(now)
            .execute(pool)
            .await?;
            Ok(result.rows_affected())
        }
        DatabasePool::Sqlite(pool) => {
            let result = sqlx::query(
                "UPDATE blob_ref SET active = 0, deactivated_at = ? \
                 WHERE active = 1 AND expires_at IS NOT NULL AND expires_at < ?",
            )
            .bind(now)
            .bind(now)
            .execute(pool)
            .await?;
            Ok(result.rows_affected())
        }
    }
}

/// Objects eligible for physical deletion: no active refs and not referenced
/// within the grace window. Includes stale `pending` objects older than the
/// grace window (failed writes that never became available).
pub async fn gc_candidate_objects(
    db: &DatabasePool,
    grace_cutoff: DateTime<Utc>,
    limit: i64,
) -> Result<Vec<BlobObjectRecord>, BlobDbError> {
    match db {
        DatabasePool::Postgres(pool) => {
            let query = format!(
                "SELECT {OBJECT_COLUMNS} FROM blob_object o \
                 WHERE o.status IN ('pending', 'available', 'delete_pending') \
                 AND o.last_referenced_at < $1 \
                 AND NOT EXISTS ( \
                     SELECT 1 FROM blob_ref r \
                     WHERE r.blob_object_id = o.blob_object_id AND r.active = true \
                 ) \
                 ORDER BY o.last_referenced_at ASC LIMIT $2"
            );
            Ok(
                sqlx::query_as::<_, BlobObjectRecord>(sqlx::AssertSqlSafe(query.as_str()))
                    .bind(grace_cutoff)
                    .bind(limit)
                    .fetch_all(pool)
                    .await?,
            )
        }
        DatabasePool::Sqlite(pool) => {
            let query = format!(
                "SELECT {OBJECT_COLUMNS} FROM blob_object o \
                 WHERE o.status IN ('pending', 'available', 'delete_pending') \
                 AND o.last_referenced_at < ? \
                 AND NOT EXISTS ( \
                     SELECT 1 FROM blob_ref r \
                     WHERE r.blob_object_id = o.blob_object_id AND r.active = 1 \
                 ) \
                 ORDER BY o.last_referenced_at ASC LIMIT ?"
            );
            Ok(
                sqlx::query_as::<_, BlobObjectRecord>(sqlx::AssertSqlSafe(query.as_str()))
                    .bind(grace_cutoff)
                    .bind(limit)
                    .fetch_all(pool)
                    .await?,
            )
        }
    }
}

/// Total bytes of available blob objects for an org (for quota accounting).
/// Each object is counted once regardless of ref count.
pub async fn get_blob_usage_by_org(db: &DatabasePool, org_id: Uuid) -> Result<i64, BlobDbError> {
    match db {
        DatabasePool::Postgres(pool) => {
            let (total,): (Option<i64>,) = sqlx::query_as(
                "SELECT SUM(size)::bigint FROM blob_object \
                 WHERE org_id = $1 AND status = 'available'",
            )
            .bind(org_id)
            .fetch_one(pool)
            .await?;
            Ok(total.unwrap_or(0))
        }
        DatabasePool::Sqlite(pool) => {
            let (total,): (Option<i64>,) = sqlx::query_as(
                "SELECT SUM(size) FROM blob_object \
                 WHERE org_id = ? AND status = 'available'",
            )
            .bind(org_id)
            .fetch_one(pool)
            .await?;
            Ok(total.unwrap_or(0))
        }
    }
}

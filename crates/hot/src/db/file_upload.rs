//! Database operations for multipart file upload tracking

use crate::db::DatabasePool;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct FileUploadRecord {
    pub upload_id: Uuid,
    pub path: String,
    pub org_id: Uuid,
    pub env_id: Option<Uuid>,
    pub created_by_user_id: Uuid,
    pub status: String,
    pub expected_size: Option<i64>,
    pub content_type: Option<String>,
    pub part_size: i64,
    pub parts_expected: Option<i32>,
    pub parts_received: i32,
    pub bytes_received: i64,
    pub backend_upload_id: Option<String>,
    pub parts_manifest: serde_json::Value,
    pub storage_backend: String,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    /// The file version the completion was claimed against: the active
    /// file's storage location, `""` when the path had none, or NULL before
    /// the first claim (and for uploads claimed by older releases).
    pub completion_base: Option<String>,
    /// Stable response saved atomically with the completed status.
    pub completion_result: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PartInfo {
    pub part_number: i32,
    pub size: i64,
    pub etag: String,
}

/// What `insert_upload` returns when the account has no room for another
/// pending upload. It is the caller's limit, not a server failure.
pub const RESERVATION_LIMIT_EXCEEDED: &str =
    "Pending upload count or byte reservation limit exceeded";

#[allow(clippy::too_many_arguments)]
pub async fn insert_upload(
    db: &DatabasePool,
    path: &str,
    org_id: Uuid,
    env_id: Option<Uuid>,
    user_id: Uuid,
    expected_size: Option<i64>,
    content_type: Option<&str>,
    part_size: i64,
    parts_expected: Option<i32>,
    backend_upload_id: Option<&str>,
    storage_backend: &str,
    expires_at: DateTime<Utc>,
    max_pending_uploads: i64,
    max_pending_bytes: i64,
) -> Result<FileUploadRecord, String> {
    let upload_id = Uuid::now_v7();
    let now = Utc::now();

    match db {
        DatabasePool::Postgres(pool) => {
            let mut tx = pool
                .begin()
                .await
                .map_err(|e| format!("Failed to begin upload reservation: {e}"))?;
            sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
                .bind(org_id.to_string())
                .execute(&mut *tx)
                .await
                .map_err(|e| format!("Failed to lock upload reservation: {e}"))?;
            let row = sqlx::query(
                r#"
                INSERT INTO file_upload (
                    upload_id, path, org_id, env_id, created_by_user_id, status,
                    expected_size, content_type, part_size, parts_expected,
                    parts_received, bytes_received, backend_upload_id,
                    parts_manifest, storage_backend, created_at, expires_at
                ) SELECT $1, $2, $3, $4, $5, 'pending', $6, $7, $8, $9, 0, 0, $10, '[]'::jsonb, $11, $12, $13
                  WHERE (
                      SELECT COUNT(*) FROM file_upload
                      WHERE org_id = $3 AND env_id = $4 AND status IN ('pending', 'completing')
                  ) < $14
                  AND COALESCE((
                      SELECT SUM(expected_size) FROM file_upload
                      WHERE org_id = $3 AND status IN ('pending', 'completing')
                  ), 0) + $6 <= $15
                RETURNING upload_id, path, org_id, env_id, created_by_user_id, status,
                          expected_size, content_type, part_size, parts_expected,
                          parts_received, bytes_received, backend_upload_id,
                          parts_manifest, storage_backend, created_at, expires_at, completion_base, completion_result
                "#,
            )
            .bind(upload_id)
            .bind(path)
            .bind(org_id)
            .bind(env_id)
            .bind(user_id)
            .bind(expected_size)
            .bind(content_type)
            .bind(part_size)
            .bind(parts_expected)
            .bind(backend_upload_id)
            .bind(storage_backend)
            .bind(now)
            .bind(expires_at)
            .bind(max_pending_uploads)
            .bind(max_pending_bytes)
            .fetch_one(&mut *tx)
            .await
            .map_err(|e| match e {
                // The guarded INSERT ... SELECT returns no row when a limit fails.
                sqlx::Error::RowNotFound => RESERVATION_LIMIT_EXCEEDED.to_string(),
                e => format!("Failed to insert upload: {e}"),
            })?;
            let record = row_to_upload_pg(&row)?;
            tx.commit()
                .await
                .map_err(|e| format!("Failed to commit upload reservation: {e}"))?;
            Ok(record)
        }
        DatabasePool::Sqlite(pool) => {
            let env_id =
                env_id.ok_or_else(|| "Multipart uploads require an environment".to_string())?;
            let mut tx = pool
                .begin()
                .await
                .map_err(|e| format!("Failed to begin upload reservation: {e}"))?;
            sqlx::query("UPDATE org SET updated_at = updated_at WHERE org_id = ?")
                .bind(org_id)
                .execute(&mut *tx)
                .await
                .map_err(|e| format!("Failed to lock upload reservation: {e}"))?;
            let rows_affected = sqlx::query(
                r#"
                INSERT INTO file_upload (
                    upload_id, path, org_id, env_id, created_by_user_id, status,
                    expected_size, content_type, part_size, parts_expected,
                    parts_received, bytes_received, backend_upload_id,
                    parts_manifest, storage_backend, created_at, expires_at
                ) SELECT ?, ?, ?, ?, ?, 'pending', ?, ?, ?, ?, 0, 0, ?, '[]', ?, ?, ?
                  WHERE (
                      SELECT COUNT(*) FROM file_upload
                      WHERE org_id = ? AND env_id = ? AND status IN ('pending', 'completing')
                  ) < ?
                  AND COALESCE((
                      SELECT SUM(expected_size) FROM file_upload
                      WHERE org_id = ? AND status IN ('pending', 'completing')
                  ), 0) + ? <= ?
                "#,
            )
            .bind(upload_id)
            .bind(path)
            .bind(org_id)
            .bind(env_id)
            .bind(user_id)
            .bind(expected_size)
            .bind(content_type)
            .bind(part_size)
            .bind(parts_expected)
            .bind(backend_upload_id)
            .bind(storage_backend)
            .bind(now)
            .bind(expires_at)
            .bind(org_id)
            .bind(env_id)
            .bind(max_pending_uploads)
            .bind(org_id)
            .bind(expected_size)
            .bind(max_pending_bytes)
            .execute(&mut *tx)
            .await
            .map_err(|e| format!("Failed to insert upload: {}", e))?
            .rows_affected();
            if rows_affected == 0 {
                return Err(RESERVATION_LIMIT_EXCEEDED.to_string());
            }
            let row = sqlx::query(
                r#"
                SELECT upload_id, path, org_id, env_id, created_by_user_id, status,
                       expected_size, content_type, part_size, parts_expected,
                       parts_received, bytes_received, backend_upload_id,
                       parts_manifest, storage_backend, created_at, expires_at, completion_base, completion_result
                FROM file_upload
                WHERE upload_id = ? AND org_id = ? AND env_id = ?
                "#,
            )
            .bind(upload_id)
            .bind(org_id)
            .bind(env_id)
            .fetch_one(&mut *tx)
            .await
            .map_err(|e| format!("Upload not found after reservation: {e}"))?;
            let record = row_to_upload_sqlite(&row)?;
            tx.commit()
                .await
                .map_err(|e| format!("Failed to commit upload reservation: {e}"))?;
            Ok(record)
        }
    }
}

pub async fn get_upload(
    db: &DatabasePool,
    upload_id: Uuid,
    org_id: Uuid,
    env_id: Uuid,
) -> Result<FileUploadRecord, String> {
    match db {
        DatabasePool::Postgres(pool) => {
            let row = sqlx::query(
                r#"
                SELECT upload_id, path, org_id, env_id, created_by_user_id, status,
                       expected_size, content_type, part_size, parts_expected,
                       parts_received, bytes_received, backend_upload_id,
                       parts_manifest, storage_backend, created_at, expires_at, completion_base, completion_result
                FROM file_upload
                WHERE upload_id = $1 AND org_id = $2 AND env_id = $3
                "#,
            )
            .bind(upload_id)
            .bind(org_id)
            .bind(env_id)
            .fetch_one(pool)
            .await
            .map_err(|e| format!("Upload not found: {}", e))?;

            row_to_upload_pg(&row)
        }
        DatabasePool::Sqlite(pool) => {
            let row = sqlx::query(
                r#"
                SELECT upload_id, path, org_id, env_id, created_by_user_id, status,
                       expected_size, content_type, part_size, parts_expected,
                       parts_received, bytes_received, backend_upload_id,
                       parts_manifest, storage_backend, created_at, expires_at, completion_base, completion_result
                FROM file_upload
                WHERE upload_id = ? AND org_id = ? AND env_id = ?
                "#,
            )
            .bind(upload_id)
            .bind(org_id)
            .bind(env_id)
            .fetch_one(pool)
            .await
            .map_err(|e| format!("Upload not found: {}", e))?;

            row_to_upload_sqlite(&row)
        }
    }
}

/// A reservation fixes a part's content before touching a mutable backend
/// such as S3. Only the lease owner may upload and record it. A crashed owner
/// can be replaced, but the reserved bytes can never change.
pub enum PartReservation {
    Claimed(Uuid),
    Busy,
    DifferentContent,
}

pub async fn part_content_hash(
    db: &DatabasePool,
    upload_id: Uuid,
    part_number: i32,
) -> Result<Option<String>, String> {
    match db {
        DatabasePool::Postgres(pool) => sqlx::query_scalar(
            "SELECT content_hash FROM file_upload_part_claim WHERE upload_id = $1 AND part_number = $2",
        ).bind(upload_id).bind(part_number).fetch_optional(pool).await,
        DatabasePool::Sqlite(pool) => sqlx::query_scalar(
            "SELECT content_hash FROM file_upload_part_claim WHERE upload_id = ? AND part_number = ?",
        ).bind(upload_id).bind(part_number).fetch_optional(pool).await,
    }.map_err(|e| format!("Failed to read part reservation: {e}"))
}

pub async fn reserve_part(
    db: &DatabasePool,
    upload_id: Uuid,
    org_id: Uuid,
    env_id: Uuid,
    part_number: i32,
    content_hash: &str,
) -> Result<PartReservation, String> {
    let owner = Uuid::new_v4();
    let now = Utc::now();
    // Longer than the API's bounded five-minute upload operation.
    let until = now + chrono::Duration::minutes(10);
    let claimed: Option<Uuid> = match db {
        DatabasePool::Postgres(pool) => sqlx::query_scalar(
            "INSERT INTO file_upload_part_claim (upload_id, part_number, content_hash, owner, lease_until)
             SELECT $1, $2, $3, $4, $5 FROM file_upload
             WHERE upload_id = $1 AND org_id = $6 AND env_id = $7 AND status = 'pending'
             ON CONFLICT (upload_id, part_number) DO UPDATE
             SET owner = EXCLUDED.owner, lease_until = EXCLUDED.lease_until
             WHERE file_upload_part_claim.content_hash = EXCLUDED.content_hash
               AND file_upload_part_claim.lease_until <= $8
             RETURNING owner",
        ).bind(upload_id).bind(part_number).bind(content_hash).bind(owner).bind(until)
            .bind(org_id).bind(env_id).bind(now).fetch_optional(pool).await,
        DatabasePool::Sqlite(pool) => sqlx::query_scalar(
            "INSERT INTO file_upload_part_claim (upload_id, part_number, content_hash, owner, lease_until)
             SELECT ?, ?, ?, ?, ? FROM file_upload
             WHERE upload_id = ? AND org_id = ? AND env_id = ? AND status = 'pending'
             ON CONFLICT (upload_id, part_number) DO UPDATE
             SET owner = excluded.owner, lease_until = excluded.lease_until
             WHERE file_upload_part_claim.content_hash = excluded.content_hash
               AND file_upload_part_claim.lease_until <= ?
             RETURNING owner",
        ).bind(upload_id).bind(part_number).bind(content_hash).bind(owner).bind(until)
            .bind(upload_id).bind(org_id).bind(env_id).bind(now).fetch_optional(pool).await,
    }.map_err(|e| format!("Failed to reserve part: {e}"))?;
    if let Some(owner) = claimed {
        return Ok(PartReservation::Claimed(owner));
    }
    match part_content_hash(db, upload_id, part_number).await? {
        Some(hash) if hash != content_hash => Ok(PartReservation::DifferentContent),
        Some(_) => Ok(PartReservation::Busy),
        None => Err("Upload is no longer pending".to_string()),
    }
}

pub async fn release_part(
    db: &DatabasePool,
    upload_id: Uuid,
    part_number: i32,
    owner: Uuid,
) -> Result<(), String> {
    match db {
        DatabasePool::Postgres(pool) => sqlx::query(
            "UPDATE file_upload_part_claim SET lease_until = $4
             WHERE upload_id = $1 AND part_number = $2 AND owner = $3",
        )
        .bind(upload_id)
        .bind(part_number)
        .bind(owner)
        .bind(Utc::now())
        .execute(pool)
        .await
        .map(|_| ()),
        DatabasePool::Sqlite(pool) => sqlx::query(
            "UPDATE file_upload_part_claim SET lease_until = ?
             WHERE upload_id = ? AND part_number = ? AND owner = ?",
        )
        .bind(Utc::now())
        .bind(upload_id)
        .bind(part_number)
        .bind(owner)
        .execute(pool)
        .await
        .map(|_| ()),
    }
    .map_err(|e| format!("Failed to release part reservation: {e}"))?;
    Ok(())
}

pub async fn record_part(
    db: &DatabasePool,
    upload_id: Uuid,
    org_id: Uuid,
    env_id: Uuid,
    part_info: &PartInfo,
    claim_token: Uuid,
) -> Result<FileUploadRecord, String> {
    let part_json = serde_json::to_value(part_info)
        .map_err(|e| format!("Failed to serialize part info: {}", e))?;

    match db {
        DatabasePool::Postgres(pool) => {
            let row = sqlx::query(
                r#"
                UPDATE file_upload SET
                    parts_manifest = parts_manifest || jsonb_build_array($3),
                    parts_received = parts_received + 1,
                    bytes_received = bytes_received + $4
                WHERE upload_id = $1 AND org_id = $2 AND env_id = $5
                  AND status = 'pending'
                  AND EXISTS (SELECT 1 FROM file_upload_part_claim c
                      WHERE c.upload_id = file_upload.upload_id AND c.part_number = $6
                        AND c.owner = $7 AND c.lease_until > $8)
                  AND NOT parts_manifest @> jsonb_build_array(jsonb_build_object('part_number', $6))
                  AND (parts_expected IS NULL OR $6 <= parts_expected)
                  AND (expected_size IS NULL OR bytes_received + $4 <= expected_size)
                RETURNING upload_id, path, org_id, env_id, created_by_user_id, status,
                          expected_size, content_type, part_size, parts_expected,
                          parts_received, bytes_received, backend_upload_id,
                          parts_manifest, storage_backend, created_at, expires_at, completion_base, completion_result
                "#,
            )
            .bind(upload_id)
            .bind(org_id)
            .bind(&part_json)
            .bind(part_info.size)
            .bind(env_id)
            .bind(part_info.part_number)
            .bind(claim_token)
            .bind(Utc::now())
            .fetch_one(pool)
            .await
            .map_err(|e| format!("Failed to record part: {}", e))?;

            row_to_upload_pg(&row)
        }
        DatabasePool::Sqlite(pool) => {
            let part_json_str = serde_json::to_string(part_info)
                .map_err(|e| format!("Failed to serialize part info: {}", e))?;

            let rows_affected = sqlx::query(
                r#"
                UPDATE file_upload SET
                    parts_manifest = json_insert(parts_manifest, '$[#]', json(?)),
                    parts_received = parts_received + 1,
                    bytes_received = bytes_received + ?
                WHERE upload_id = ? AND org_id = ? AND env_id = ?
                  AND status = 'pending'
                  AND EXISTS (SELECT 1 FROM file_upload_part_claim c
                      WHERE c.upload_id = file_upload.upload_id AND c.part_number = ?
                        AND c.owner = ? AND c.lease_until > ?)
                  AND NOT EXISTS (
                      SELECT 1 FROM json_each(parts_manifest)
                      WHERE json_extract(value, '$.part_number') = ?
                  )
                  AND (parts_expected IS NULL OR ? <= parts_expected)
                  AND (expected_size IS NULL OR bytes_received + ? <= expected_size)
                "#,
            )
            .bind(&part_json_str)
            .bind(part_info.size)
            .bind(upload_id)
            .bind(org_id)
            .bind(env_id)
            .bind(part_info.part_number)
            .bind(claim_token)
            .bind(Utc::now())
            .bind(part_info.part_number)
            .bind(part_info.part_number)
            .bind(part_info.size)
            .execute(pool)
            .await
            .map_err(|e| format!("Failed to record part: {}", e))?
            .rows_affected();

            if rows_affected == 0 {
                return Err("Upload not found or not in pending status".to_string());
            }

            get_upload_internal_sqlite(pool, upload_id, org_id, env_id).await
        }
    }
}

/// Durably claim an upload for completion (`pending` -> `completing`).
///
/// Once claimed, a client abort and late part uploads are refused, and the
/// expiry sweep leaves the upload alone until `lease_until`: the claim
/// extends `expires_at` to at least that time, so only a completion that
/// stopped making progress (a crash) is ever swept. Completion stays
/// retryable: a `completing` upload may be claimed again by any service
/// instance after a crash, lost response, or failed commit, which renews
/// the lease.
///
/// The first claim also records the file version the completion replaces
/// (`completion_base`): the active file's storage location, or `""` when the
/// path has no active file. Re-claims keep it, so a retried completion
/// commits against the same version and can never revert a write that
/// landed after an earlier attempt committed. Returns the recorded base.
pub async fn begin_completion(
    db: &DatabasePool,
    upload_id: Uuid,
    org_id: Uuid,
    env_id: Uuid,
    lease_until: DateTime<Utc>,
) -> Result<String, String> {
    let rows_affected = match db {
        DatabasePool::Postgres(pool) => sqlx::query(
            r#"
            UPDATE file_upload SET
                status = 'completing',
                expires_at = CASE WHEN expires_at < $4 THEN $4 ELSE expires_at END,
                completion_base = CASE
                    WHEN completion_base IS NULL THEN COALESCE((
                        SELECT f.storage_path FROM file f
                        WHERE f.path = file_upload.path
                          AND f.org_id = file_upload.org_id
                          AND f.env_id = file_upload.env_id
                          AND f.active = true
                    ), '')
                    ELSE completion_base
                END
            WHERE upload_id = $1 AND org_id = $2 AND env_id = $3
              AND status IN ('pending', 'completing')
              AND (parts_expected IS NULL OR parts_received = parts_expected)
              AND (parts_expected IS NULL OR bytes_received = expected_size)
            "#,
        )
        .bind(upload_id)
        .bind(org_id)
        .bind(env_id)
        .bind(lease_until)
        .execute(pool)
        .await
        .map_err(|e| format!("Failed to begin upload completion: {}", e))?
        .rows_affected(),
        DatabasePool::Sqlite(pool) => sqlx::query(
            r#"
            UPDATE file_upload SET
                status = 'completing',
                expires_at = CASE WHEN expires_at < ? THEN ? ELSE expires_at END,
                completion_base = CASE
                    WHEN completion_base IS NULL THEN COALESCE((
                        SELECT f.storage_path FROM file f
                        WHERE f.path = file_upload.path
                          AND f.org_id = file_upload.org_id
                          AND f.env_id = file_upload.env_id
                          AND f.active = 1
                    ), '')
                    ELSE completion_base
                END
            WHERE upload_id = ? AND org_id = ? AND env_id = ?
              AND status IN ('pending', 'completing')
              AND (parts_expected IS NULL OR parts_received = parts_expected)
              AND (parts_expected IS NULL OR bytes_received = expected_size)
            "#,
        )
        .bind(lease_until)
        .bind(lease_until)
        .bind(upload_id)
        .bind(org_id)
        .bind(env_id)
        .execute(pool)
        .await
        .map_err(|e| format!("Failed to begin upload completion: {}", e))?
        .rows_affected(),
    };

    if rows_affected == 0 {
        return Err("Upload not found, incomplete, or no longer pending".to_string());
    }

    get_upload(db, upload_id, org_id, env_id)
        .await?
        .completion_base
        .ok_or_else(|| "Upload completion base was not recorded".to_string())
}

/// Claim a `pending` upload for a client abort (`pending` -> `aborting`)
/// before any staged bytes are deleted, so an abort can never race a
/// completion that is assembling those bytes.
pub async fn begin_abort(
    db: &DatabasePool,
    upload_id: Uuid,
    org_id: Uuid,
    env_id: Uuid,
) -> Result<(), String> {
    claim_abort(db, upload_id, org_id, env_id, false).await
}

/// Turn an irreconcilable completion into a recoverable abort.
pub async fn abandon_completion(
    db: &DatabasePool,
    upload_id: Uuid,
    org_id: Uuid,
    env_id: Uuid,
) -> Result<(), String> {
    claim_abort(db, upload_id, org_id, env_id, true).await
}

async fn claim_abort(
    db: &DatabasePool,
    upload_id: Uuid,
    org_id: Uuid,
    env_id: Uuid,
    completion: bool,
) -> Result<(), String> {
    let now = Utc::now();
    let until = now + chrono::Duration::hours(1);
    let from = if completion { "completing" } else { "pending" };
    let changed = match db {
        DatabasePool::Postgres(pool) => sqlx::query(
            "UPDATE file_upload SET status = 'aborting', expires_at = $5
             WHERE upload_id = $1 AND org_id = $2 AND env_id = $3
             AND (status = $4 OR (status = 'aborting' AND expires_at <= $6))",
        )
        .bind(upload_id)
        .bind(org_id)
        .bind(env_id)
        .bind(from)
        .bind(until)
        .bind(now)
        .execute(pool)
        .await
        .map_err(|e| e.to_string())?
        .rows_affected(),
        DatabasePool::Sqlite(pool) => sqlx::query(
            "UPDATE file_upload SET status = 'aborting', expires_at = ?
             WHERE upload_id = ? AND org_id = ? AND env_id = ?
             AND (status = ? OR (status = 'aborting' AND expires_at <= ?))",
        )
        .bind(until)
        .bind(upload_id)
        .bind(org_id)
        .bind(env_id)
        .bind(from)
        .bind(now)
        .execute(pool)
        .await
        .map_err(|e| e.to_string())?
        .rows_affected(),
    };
    if changed == 0 {
        return Err("Upload is not available for abort".to_string());
    }
    Ok(())
}

/// Return a failed completion claim to `pending` so the client can retry or
/// abort. A missing claim (already completed or released) is not an error.
pub async fn release_completion(
    db: &DatabasePool,
    upload_id: Uuid,
    org_id: Uuid,
    env_id: Uuid,
) -> Result<(), String> {
    match db {
        DatabasePool::Postgres(pool) => sqlx::query(
            "UPDATE file_upload SET status = 'pending' WHERE upload_id = $1 AND org_id = $2 AND env_id = $3 AND status = 'completing'",
        )
        .bind(upload_id)
        .bind(org_id)
        .bind(env_id)
        .execute(pool)
        .await
        .map_err(|e| format!("Failed to release upload completion: {e}"))?
        .rows_affected(),
        DatabasePool::Sqlite(pool) => sqlx::query(
            "UPDATE file_upload SET status = 'pending' WHERE upload_id = ? AND org_id = ? AND env_id = ? AND status = 'completing'",
        )
        .bind(upload_id)
        .bind(org_id)
        .bind(env_id)
        .execute(pool)
        .await
        .map_err(|e| format!("Failed to release upload completion: {e}"))?
        .rows_affected(),
    };
    Ok(())
}

pub async fn complete_upload(
    db: &DatabasePool,
    upload_id: Uuid,
    org_id: Uuid,
    env_id: Uuid,
    result: &serde_json::Value,
) -> Result<(), String> {
    let query = match db {
        DatabasePool::Postgres(_) => {
            "UPDATE file_upload SET status = 'completed', completion_result = COALESCE(completion_result, $4) WHERE upload_id = $1 AND org_id = $2 AND env_id = $3 AND status IN ('pending', 'completing', 'completed') AND (parts_expected IS NULL OR parts_received = parts_expected) AND (parts_expected IS NULL OR bytes_received = expected_size)"
        }
        DatabasePool::Sqlite(_) => {
            "UPDATE file_upload SET status = 'completed', completion_result = COALESCE(completion_result, ?) WHERE upload_id = ? AND org_id = ? AND env_id = ? AND status IN ('pending', 'completing', 'completed') AND (parts_expected IS NULL OR parts_received = parts_expected) AND (parts_expected IS NULL OR bytes_received = expected_size)"
        }
    };

    let rows_affected = match db {
        DatabasePool::Postgres(pool) => sqlx::query(query)
            .bind(upload_id)
            .bind(org_id)
            .bind(env_id)
            .bind(result)
            .execute(pool)
            .await
            .map_err(|e| format!("Failed to complete upload: {}", e))?
            .rows_affected(),
        DatabasePool::Sqlite(pool) => sqlx::query(query)
            .bind(result.to_string())
            .bind(upload_id)
            .bind(org_id)
            .bind(env_id)
            .execute(pool)
            .await
            .map_err(|e| format!("Failed to complete upload: {}", e))?
            .rows_affected(),
    };

    if rows_affected == 0 {
        return Err("Upload not found or not in pending status".to_string());
    }

    Ok(())
}

pub async fn abort_upload(
    db: &DatabasePool,
    upload_id: Uuid,
    org_id: Uuid,
    env_id: Uuid,
) -> Result<(), String> {
    let query = match db {
        DatabasePool::Postgres(_) => {
            "UPDATE file_upload SET status = 'aborted' WHERE upload_id = $1 AND org_id = $2 AND env_id = $3 AND status IN ('pending', 'aborting', 'aborted')"
        }
        DatabasePool::Sqlite(_) => {
            "UPDATE file_upload SET status = 'aborted' WHERE upload_id = ? AND org_id = ? AND env_id = ? AND status IN ('pending', 'aborting', 'aborted')"
        }
    };

    let rows_affected = match db {
        DatabasePool::Postgres(pool) => sqlx::query(query)
            .bind(upload_id)
            .bind(org_id)
            .bind(env_id)
            .execute(pool)
            .await
            .map_err(|e| format!("Failed to abort upload: {}", e))?
            .rows_affected(),
        DatabasePool::Sqlite(pool) => sqlx::query(query)
            .bind(upload_id)
            .bind(org_id)
            .bind(env_id)
            .execute(pool)
            .await
            .map_err(|e| format!("Failed to abort upload: {}", e))?
            .rows_affected(),
    };

    if rows_affected == 0 {
        return Err("Upload not found or not in pending status".to_string());
    }

    Ok(())
}

pub async fn cleanup_expired_uploads(db: &DatabasePool) -> Result<Vec<FileUploadRecord>, String> {
    let now = Utc::now();

    match db {
        DatabasePool::Postgres(pool) => {
            let rows = sqlx::query(
                r#"
                UPDATE file_upload
                SET status = 'aborting', expires_at = $2
                WHERE status IN ('pending', 'completing', 'aborting') AND expires_at < $1 AND env_id IS NOT NULL
                RETURNING upload_id, path, org_id, env_id, created_by_user_id, status,
                          expected_size, content_type, part_size, parts_expected,
                          parts_received, bytes_received, backend_upload_id,
                          parts_manifest, storage_backend, created_at, expires_at, completion_base, completion_result
                "#,
            )
            .bind(now)
            .bind(now + chrono::Duration::hours(1))
            .fetch_all(pool)
            .await
            .map_err(|e| format!("Failed to query expired uploads: {}", e))?;

            rows.iter().map(row_to_upload_pg).collect()
        }
        DatabasePool::Sqlite(pool) => {
            let rows = sqlx::query(
                r#"
                UPDATE file_upload
                SET status = 'aborting', expires_at = ?
                WHERE status IN ('pending', 'completing', 'aborting') AND expires_at < ? AND env_id IS NOT NULL
                RETURNING upload_id, path, org_id, env_id, created_by_user_id, status,
                          expected_size, content_type, part_size, parts_expected,
                          parts_received, bytes_received, backend_upload_id,
                          parts_manifest, storage_backend, created_at, expires_at, completion_base, completion_result
                "#,
            )
            .bind(now + chrono::Duration::hours(1))
            .bind(now)
            .fetch_all(pool)
            .await
            .map_err(|e| format!("Failed to query expired uploads: {}", e))?;

            rows.iter().map(row_to_upload_sqlite).collect()
        }
    }
}

pub async fn release_cleanup_claim(
    db: &DatabasePool,
    upload_id: Uuid,
    org_id: Uuid,
    env_id: Uuid,
) -> Result<(), String> {
    let rows_affected = match db {
        DatabasePool::Postgres(pool) => sqlx::query(
            "UPDATE file_upload SET expires_at = $4 WHERE upload_id = $1 AND org_id = $2 AND env_id = $3 AND status = 'aborting'",
        )
        .bind(upload_id)
        .bind(org_id)
        .bind(env_id)
        .bind(Utc::now())
        .execute(pool)
        .await
        .map_err(|e| format!("Failed to release upload cleanup claim: {e}"))?
        .rows_affected(),
        DatabasePool::Sqlite(pool) => sqlx::query(
            "UPDATE file_upload SET expires_at = ? WHERE upload_id = ? AND org_id = ? AND env_id = ? AND status = 'aborting'",
        )
        .bind(Utc::now())
        .bind(upload_id)
        .bind(org_id)
        .bind(env_id)
        .execute(pool)
        .await
        .map_err(|e| format!("Failed to release upload cleanup claim: {e}"))?
        .rows_affected(),
    };
    if rows_affected == 0 {
        return Err("Upload cleanup claim was not held".to_string());
    }
    Ok(())
}

fn row_to_upload_pg(row: &sqlx::postgres::PgRow) -> Result<FileUploadRecord, String> {
    Ok(FileUploadRecord {
        upload_id: row.try_get("upload_id").map_err(|e| e.to_string())?,
        path: row.try_get("path").map_err(|e| e.to_string())?,
        org_id: row.try_get("org_id").map_err(|e| e.to_string())?,
        env_id: row.try_get("env_id").map_err(|e| e.to_string())?,
        created_by_user_id: row
            .try_get("created_by_user_id")
            .map_err(|e| e.to_string())?,
        status: row.try_get("status").map_err(|e| e.to_string())?,
        expected_size: row.try_get("expected_size").map_err(|e| e.to_string())?,
        content_type: row.try_get("content_type").map_err(|e| e.to_string())?,
        part_size: row.try_get("part_size").map_err(|e| e.to_string())?,
        parts_expected: row.try_get("parts_expected").map_err(|e| e.to_string())?,
        parts_received: row.try_get("parts_received").map_err(|e| e.to_string())?,
        bytes_received: row.try_get("bytes_received").map_err(|e| e.to_string())?,
        backend_upload_id: row
            .try_get("backend_upload_id")
            .map_err(|e| e.to_string())?,
        parts_manifest: row.try_get("parts_manifest").map_err(|e| e.to_string())?,
        storage_backend: row.try_get("storage_backend").map_err(|e| e.to_string())?,
        created_at: row.try_get("created_at").map_err(|e| e.to_string())?,
        expires_at: row.try_get("expires_at").map_err(|e| e.to_string())?,
        completion_base: row.try_get("completion_base").map_err(|e| e.to_string())?,
        completion_result: row
            .try_get("completion_result")
            .map_err(|e| e.to_string())?,
    })
}

fn row_to_upload_sqlite(row: &sqlx::sqlite::SqliteRow) -> Result<FileUploadRecord, String> {
    let upload_id_bytes: Vec<u8> = row.try_get("upload_id").map_err(|e| e.to_string())?;
    let org_id_bytes: Vec<u8> = row.try_get("org_id").map_err(|e| e.to_string())?;
    let env_id_bytes: Option<Vec<u8>> = row.try_get("env_id").map_err(|e| e.to_string())?;
    let created_by_user_id_bytes: Vec<u8> = row
        .try_get("created_by_user_id")
        .map_err(|e| e.to_string())?;
    let parts_manifest_str: String = row.try_get("parts_manifest").map_err(|e| e.to_string())?;

    Ok(FileUploadRecord {
        upload_id: Uuid::from_slice(&upload_id_bytes).map_err(|e| e.to_string())?,
        path: row.try_get("path").map_err(|e| e.to_string())?,
        org_id: Uuid::from_slice(&org_id_bytes).map_err(|e| e.to_string())?,
        env_id: env_id_bytes
            .map(|b| Uuid::from_slice(&b))
            .transpose()
            .map_err(|e| e.to_string())?,
        created_by_user_id: Uuid::from_slice(&created_by_user_id_bytes)
            .map_err(|e| e.to_string())?,
        status: row.try_get("status").map_err(|e| e.to_string())?,
        expected_size: row.try_get("expected_size").map_err(|e| e.to_string())?,
        content_type: row.try_get("content_type").map_err(|e| e.to_string())?,
        part_size: row.try_get("part_size").map_err(|e| e.to_string())?,
        parts_expected: row.try_get("parts_expected").map_err(|e| e.to_string())?,
        parts_received: row.try_get("parts_received").map_err(|e| e.to_string())?,
        bytes_received: row.try_get("bytes_received").map_err(|e| e.to_string())?,
        backend_upload_id: row
            .try_get("backend_upload_id")
            .map_err(|e| e.to_string())?,
        parts_manifest: serde_json::from_str(&parts_manifest_str).map_err(|e| e.to_string())?,
        storage_backend: row.try_get("storage_backend").map_err(|e| e.to_string())?,
        created_at: row.try_get("created_at").map_err(|e| e.to_string())?,
        expires_at: row.try_get("expires_at").map_err(|e| e.to_string())?,
        completion_base: row.try_get("completion_base").map_err(|e| e.to_string())?,
        completion_result: row
            .try_get::<Option<String>, _>("completion_result")
            .map_err(|e| e.to_string())?
            .map(|value| serde_json::from_str(&value))
            .transpose()
            .map_err(|e| e.to_string())?,
    })
}

async fn get_upload_internal_sqlite(
    pool: &sqlx::Pool<sqlx::Sqlite>,
    upload_id: Uuid,
    org_id: Uuid,
    env_id: Uuid,
) -> Result<FileUploadRecord, String> {
    let row = sqlx::query(
        r#"
        SELECT upload_id, path, org_id, env_id, created_by_user_id, status,
               expected_size, content_type, part_size, parts_expected,
               parts_received, bytes_received, backend_upload_id,
               parts_manifest, storage_backend, created_at, expires_at, completion_base, completion_result
        FROM file_upload
        WHERE upload_id = ? AND org_id = ? AND env_id = ?
        "#,
    )
    .bind(upload_id)
    .bind(org_id)
    .bind(env_id)
    .fetch_one(pool)
    .await
    .map_err(|e| format!("Upload not found: {}", e))?;

    row_to_upload_sqlite(&row)
}

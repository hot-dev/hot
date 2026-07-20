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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PartInfo {
    pub part_number: i32,
    pub size: i64,
    pub etag: String,
}

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
                      WHERE org_id = $3 AND env_id = $4 AND status = 'pending'
                  ) < $14
                  AND COALESCE((
                      SELECT SUM(expected_size) FROM file_upload
                      WHERE org_id = $3 AND status = 'pending'
                  ), 0) + $6 <= $15
                RETURNING upload_id, path, org_id, env_id, created_by_user_id, status,
                          expected_size, content_type, part_size, parts_expected,
                          parts_received, bytes_received, backend_upload_id,
                          parts_manifest, storage_backend, created_at, expires_at
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
            .map_err(|e| format!("Failed to insert upload: {}", e))?;
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
                      WHERE org_id = ? AND env_id = ? AND status = 'pending'
                  ) < ?
                  AND COALESCE((
                      SELECT SUM(expected_size) FROM file_upload
                      WHERE org_id = ? AND status = 'pending'
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
                return Err("Pending upload count or byte reservation limit exceeded".to_string());
            }
            let row = sqlx::query(
                r#"
                SELECT upload_id, path, org_id, env_id, created_by_user_id, status,
                       expected_size, content_type, part_size, parts_expected,
                       parts_received, bytes_received, backend_upload_id,
                       parts_manifest, storage_backend, created_at, expires_at
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
                       parts_manifest, storage_backend, created_at, expires_at
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
                       parts_manifest, storage_backend, created_at, expires_at
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

pub async fn record_part(
    db: &DatabasePool,
    upload_id: Uuid,
    org_id: Uuid,
    env_id: Uuid,
    part_info: &PartInfo,
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
                  AND NOT parts_manifest @> jsonb_build_array(jsonb_build_object('part_number', $6))
                  AND (parts_expected IS NULL OR $6 <= parts_expected)
                  AND (expected_size IS NULL OR bytes_received + $4 <= expected_size)
                RETURNING upload_id, path, org_id, env_id, created_by_user_id, status,
                          expected_size, content_type, part_size, parts_expected,
                          parts_received, bytes_received, backend_upload_id,
                          parts_manifest, storage_backend, created_at, expires_at
                "#,
            )
            .bind(upload_id)
            .bind(org_id)
            .bind(&part_json)
            .bind(part_info.size)
            .bind(env_id)
            .bind(part_info.part_number)
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

pub async fn complete_upload(
    db: &DatabasePool,
    upload_id: Uuid,
    org_id: Uuid,
    env_id: Uuid,
) -> Result<(), String> {
    let query = match db {
        DatabasePool::Postgres(_) => {
            "UPDATE file_upload SET status = 'completed' WHERE upload_id = $1 AND org_id = $2 AND env_id = $3 AND status = 'pending' AND (parts_expected IS NULL OR parts_received = parts_expected) AND (parts_expected IS NULL OR bytes_received = expected_size)"
        }
        DatabasePool::Sqlite(_) => {
            "UPDATE file_upload SET status = 'completed' WHERE upload_id = ? AND org_id = ? AND env_id = ? AND status = 'pending' AND (parts_expected IS NULL OR parts_received = parts_expected) AND (parts_expected IS NULL OR bytes_received = expected_size)"
        }
    };

    let rows_affected = match db {
        DatabasePool::Postgres(pool) => sqlx::query(query)
            .bind(upload_id)
            .bind(org_id)
            .bind(env_id)
            .execute(pool)
            .await
            .map_err(|e| format!("Failed to complete upload: {}", e))?
            .rows_affected(),
        DatabasePool::Sqlite(pool) => sqlx::query(query)
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
            "UPDATE file_upload SET status = 'aborted' WHERE upload_id = $1 AND org_id = $2 AND env_id = $3 AND status IN ('pending', 'aborting')"
        }
        DatabasePool::Sqlite(_) => {
            "UPDATE file_upload SET status = 'aborted' WHERE upload_id = ? AND org_id = ? AND env_id = ? AND status IN ('pending', 'aborting')"
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
                SET status = 'aborting'
                WHERE status = 'pending' AND expires_at < $1 AND env_id IS NOT NULL
                RETURNING upload_id, path, org_id, env_id, created_by_user_id, status,
                          expected_size, content_type, part_size, parts_expected,
                          parts_received, bytes_received, backend_upload_id,
                          parts_manifest, storage_backend, created_at, expires_at
                "#,
            )
            .bind(now)
            .fetch_all(pool)
            .await
            .map_err(|e| format!("Failed to query expired uploads: {}", e))?;

            rows.iter().map(row_to_upload_pg).collect()
        }
        DatabasePool::Sqlite(pool) => {
            let rows = sqlx::query(
                r#"
                UPDATE file_upload
                SET status = 'aborting'
                WHERE status = 'pending' AND expires_at < ? AND env_id IS NOT NULL
                RETURNING upload_id, path, org_id, env_id, created_by_user_id, status,
                          expected_size, content_type, part_size, parts_expected,
                          parts_received, bytes_received, backend_upload_id,
                          parts_manifest, storage_backend, created_at, expires_at
                "#,
            )
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
            "UPDATE file_upload SET status = 'pending' WHERE upload_id = $1 AND org_id = $2 AND env_id = $3 AND status = 'aborting'",
        )
        .bind(upload_id)
        .bind(org_id)
        .bind(env_id)
        .execute(pool)
        .await
        .map_err(|e| format!("Failed to release upload cleanup claim: {e}"))?
        .rows_affected(),
        DatabasePool::Sqlite(pool) => sqlx::query(
            "UPDATE file_upload SET status = 'pending' WHERE upload_id = ? AND org_id = ? AND env_id = ? AND status = 'aborting'",
        )
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
               parts_manifest, storage_backend, created_at, expires_at
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

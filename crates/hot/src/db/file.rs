//! Database operations for file storage tracking

use crate::db::DatabasePool;
use chrono::{DateTime, Utc};
use sqlx::Row;
use uuid::Uuid;

/// File metadata from database
#[derive(Debug, Clone)]
pub struct FileRecord {
    pub file_id: Uuid,
    pub path: String,
    pub size: i64,
    pub etag: Option<String>,
    pub content_type: Option<String>,
    pub storage_backend: String,
    pub storage_path: String,
    pub org_id: Uuid,
    pub env_id: Option<Uuid>, // Environment isolation
    pub created_by_run_id: Option<Uuid>,
    pub updated_by_run_id: Option<Uuid>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub created_by_user_id: Uuid,
    pub updated_by_user_id: Option<Uuid>,
}

/// Insert a new file record into the database
#[allow(clippy::too_many_arguments)]
pub async fn insert_file_record(
    db: &DatabasePool,
    path: &str,
    size: i64,
    etag: Option<&str>,
    content_type: Option<&str>,
    storage_backend: &str,
    storage_path: &str,
    org_id: Uuid,
    env_id: Option<Uuid>, // Added env_id for security
    created_by_user_id: Uuid,
    created_by_run_id: Option<Uuid>,
) -> Result<FileRecord, String> {
    let file_id = Uuid::now_v7();
    let now = Utc::now();

    let query = match db {
        DatabasePool::Postgres(_) => {
            r#"
            INSERT INTO file (
                file_id, path, size, etag, content_type,
                storage_backend, storage_path, org_id, env_id,
                created_by_user_id, created_by_run_id,
                created_at, updated_at, active
            ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, true)
            RETURNING file_id, path, size, etag, content_type,
                      storage_backend, storage_path, org_id, env_id,
                      created_by_run_id, updated_by_run_id,
                      created_at, updated_at, created_by_user_id, updated_by_user_id
            "#
        }
        DatabasePool::Sqlite(_) => {
            r#"
            INSERT INTO file (
                file_id, path, size, etag, content_type,
                storage_backend, storage_path, org_id, env_id,
                created_by_user_id, created_by_run_id,
                created_at, updated_at, active
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 1)
            "#
        }
    };

    match db {
        DatabasePool::Postgres(pool) => {
            let row = sqlx::query(query)
                .bind(file_id)
                .bind(path)
                .bind(size)
                .bind(etag)
                .bind(content_type)
                .bind(storage_backend)
                .bind(storage_path)
                .bind(org_id)
                .bind(env_id) // Added env_id binding
                .bind(created_by_user_id)
                .bind(created_by_run_id)
                .bind(now)
                .bind(now)
                .fetch_one(pool)
                .await
                .map_err(|e| format!("Failed to insert file record: {}", e))?;

            Ok(FileRecord {
                file_id: row.try_get("file_id").map_err(|e| e.to_string())?,
                path: row.try_get("path").map_err(|e| e.to_string())?,
                size: row.try_get("size").map_err(|e| e.to_string())?,
                etag: row.try_get("etag").map_err(|e| e.to_string())?,
                content_type: row.try_get("content_type").map_err(|e| e.to_string())?,
                storage_backend: row.try_get("storage_backend").map_err(|e| e.to_string())?,
                storage_path: row.try_get("storage_path").map_err(|e| e.to_string())?,
                org_id: row.try_get("org_id").map_err(|e| e.to_string())?,
                env_id: row.try_get("env_id").map_err(|e| e.to_string())?, // Added env_id
                created_by_run_id: row
                    .try_get("created_by_run_id")
                    .map_err(|e| e.to_string())?,
                updated_by_run_id: row
                    .try_get("updated_by_run_id")
                    .map_err(|e| e.to_string())?,
                created_at: row.try_get("created_at").map_err(|e| e.to_string())?,
                updated_at: row.try_get("updated_at").map_err(|e| e.to_string())?,
                created_by_user_id: row
                    .try_get("created_by_user_id")
                    .map_err(|e| e.to_string())?,
                updated_by_user_id: row
                    .try_get("updated_by_user_id")
                    .map_err(|e| e.to_string())?,
            })
        }
        DatabasePool::Sqlite(pool) => {
            sqlx::query(query)
                .bind(file_id)
                .bind(path)
                .bind(size)
                .bind(etag)
                .bind(content_type)
                .bind(storage_backend)
                .bind(storage_path)
                .bind(org_id)
                .bind(env_id) // Added env_id binding
                .bind(created_by_user_id)
                .bind(created_by_run_id)
                .bind(now)
                .bind(now)
                .execute(pool)
                .await
                .map_err(|e| format!("Failed to insert file record: {}", e))?;

            // Fetch the inserted record - now requires env_id for security
            get_file_by_path(db, path, org_id, env_id).await
        }
    }
}

/// Update an existing file record
#[allow(clippy::too_many_arguments)]
pub async fn update_file_record(
    db: &DatabasePool,
    file_id: Uuid,
    size: i64,
    etag: Option<&str>,
    content_type: Option<&str>,
    storage_path: &str,
    updated_by_user_id: Uuid,
    updated_by_run_id: Option<Uuid>,
) -> Result<FileRecord, String> {
    let now = Utc::now();

    let query = match db {
        DatabasePool::Postgres(_) => {
            r#"
            UPDATE file SET
                size = $2,
                etag = $3,
                content_type = $4,
                storage_path = $5,
                updated_by_user_id = $6,
                updated_by_run_id = $7,
                updated_at = $8
            WHERE file_id = $1
            RETURNING file_id, path, size, etag, content_type,
                      storage_backend, storage_path, org_id, env_id,
                      created_by_run_id, updated_by_run_id,
                      created_at, updated_at, created_by_user_id, updated_by_user_id
            "#
        }
        DatabasePool::Sqlite(_pool) => {
            r#"
            UPDATE file SET
                size = ?,
                etag = ?,
                content_type = ?,
                storage_path = ?,
                updated_by_user_id = ?,
                updated_by_run_id = ?,
                updated_at = ?
            WHERE file_id = ?
            "#
        }
    };

    match db {
        DatabasePool::Postgres(pool) => {
            let row = sqlx::query(query)
                .bind(file_id)
                .bind(size)
                .bind(etag)
                .bind(content_type)
                .bind(storage_path)
                .bind(updated_by_user_id)
                .bind(updated_by_run_id)
                .bind(now)
                .fetch_one(pool)
                .await
                .map_err(|e| format!("Failed to update file record: {}", e))?;

            Ok(FileRecord {
                file_id: row.try_get("file_id").map_err(|e| e.to_string())?,
                path: row.try_get("path").map_err(|e| e.to_string())?,
                size: row.try_get("size").map_err(|e| e.to_string())?,
                etag: row.try_get("etag").map_err(|e| e.to_string())?,
                content_type: row.try_get("content_type").map_err(|e| e.to_string())?,
                storage_backend: row.try_get("storage_backend").map_err(|e| e.to_string())?,
                storage_path: row.try_get("storage_path").map_err(|e| e.to_string())?,
                org_id: row.try_get("org_id").map_err(|e| e.to_string())?,
                env_id: row.try_get("env_id").map_err(|e| e.to_string())?,
                created_by_run_id: row
                    .try_get("created_by_run_id")
                    .map_err(|e| e.to_string())?,
                updated_by_run_id: row
                    .try_get("updated_by_run_id")
                    .map_err(|e| e.to_string())?,
                created_at: row.try_get("created_at").map_err(|e| e.to_string())?,
                updated_at: row.try_get("updated_at").map_err(|e| e.to_string())?,
                created_by_user_id: row
                    .try_get("created_by_user_id")
                    .map_err(|e| e.to_string())?,
                updated_by_user_id: row
                    .try_get("updated_by_user_id")
                    .map_err(|e| e.to_string())?,
            })
        }
        DatabasePool::Sqlite(pool) => {
            sqlx::query(query)
                .bind(size)
                .bind(etag)
                .bind(content_type)
                .bind(storage_path)
                .bind(updated_by_user_id)
                .bind(updated_by_run_id)
                .bind(now)
                .bind(file_id)
                .execute(pool)
                .await
                .map_err(|e| format!("Failed to update file record: {}", e))?;

            // Fetch the updated record by file_id
            get_file_by_id(db, file_id).await
        }
    }
}

/// Get file record by path, org_id, and env_id (SECURITY: prevents cross-org and cross-env access)
pub async fn get_file_by_path(
    db: &DatabasePool,
    path: &str,
    org_id: Uuid,
    env_id: Option<Uuid>, // Added env_id for security
) -> Result<FileRecord, String> {
    let query = match db {
        DatabasePool::Postgres(_) => {
            r#"
            SELECT file_id, path, size, etag, content_type,
                   storage_backend, storage_path, org_id, env_id,
                   created_by_run_id, updated_by_run_id,
                   created_at, updated_at, created_by_user_id, updated_by_user_id
            FROM file
            WHERE path = $1 AND org_id = $2 AND (env_id = $3 OR (env_id IS NULL AND $3 IS NULL)) AND active = true
            "#
        }
        DatabasePool::Sqlite(_) => {
            r#"
            SELECT file_id, path, size, etag, content_type,
                   storage_backend, storage_path, org_id, env_id,
                   created_by_run_id, updated_by_run_id,
                   created_at, updated_at, created_by_user_id, updated_by_user_id
            FROM file
            WHERE path = ? AND org_id = ? AND (env_id = ? OR (env_id IS NULL AND ? IS NULL)) AND active = 1
            "#
        }
    };

    match db {
        DatabasePool::Postgres(pool) => {
            let row = sqlx::query(query)
                .bind(path)
                .bind(org_id)
                .bind(env_id) // First bind for env_id = $3
                .bind(env_id) // Second bind for $3 IS NULL check
                .fetch_one(pool)
                .await
                .map_err(|e| format!("File not found: {}", e))?;

            Ok(FileRecord {
                file_id: row.try_get("file_id").map_err(|e| e.to_string())?,
                path: row.try_get("path").map_err(|e| e.to_string())?,
                size: row.try_get("size").map_err(|e| e.to_string())?,
                etag: row.try_get("etag").map_err(|e| e.to_string())?,
                content_type: row.try_get("content_type").map_err(|e| e.to_string())?,
                storage_backend: row.try_get("storage_backend").map_err(|e| e.to_string())?,
                storage_path: row.try_get("storage_path").map_err(|e| e.to_string())?,
                org_id: row.try_get("org_id").map_err(|e| e.to_string())?,
                env_id: row.try_get("env_id").map_err(|e| e.to_string())?, // Added env_id
                created_by_run_id: row
                    .try_get("created_by_run_id")
                    .map_err(|e| e.to_string())?,
                updated_by_run_id: row
                    .try_get("updated_by_run_id")
                    .map_err(|e| e.to_string())?,
                created_at: row.try_get("created_at").map_err(|e| e.to_string())?,
                updated_at: row.try_get("updated_at").map_err(|e| e.to_string())?,
                created_by_user_id: row
                    .try_get("created_by_user_id")
                    .map_err(|e| e.to_string())?,
                updated_by_user_id: row
                    .try_get("updated_by_user_id")
                    .map_err(|e| e.to_string())?,
            })
        }
        DatabasePool::Sqlite(pool) => {
            let row = sqlx::query(query)
                .bind(path)
                .bind(org_id)
                .bind(env_id) // Added env_id for security
                .bind(env_id) // Need to bind twice for IS NULL check
                .fetch_one(pool)
                .await
                .map_err(|e| format!("File not found: {}", e))?;

            let file_id_bytes: Vec<u8> = row.try_get("file_id").map_err(|e| e.to_string())?;
            let org_id_bytes: Vec<u8> = row.try_get("org_id").map_err(|e| e.to_string())?;
            let env_id_bytes: Option<Vec<u8>> = row.try_get("env_id").map_err(|e| e.to_string())?; // Added env_id
            let created_by_user_id_bytes: Vec<u8> = row
                .try_get("created_by_user_id")
                .map_err(|e| e.to_string())?;
            let updated_by_user_id_bytes: Option<Vec<u8>> = row
                .try_get("updated_by_user_id")
                .map_err(|e| e.to_string())?;
            let created_by_run_id_bytes: Option<Vec<u8>> = row
                .try_get("created_by_run_id")
                .map_err(|e| e.to_string())?;
            let updated_by_run_id_bytes: Option<Vec<u8>> = row
                .try_get("updated_by_run_id")
                .map_err(|e| e.to_string())?;

            Ok(FileRecord {
                file_id: Uuid::from_slice(&file_id_bytes).map_err(|e| e.to_string())?,
                path: row.try_get("path").map_err(|e| e.to_string())?,
                size: row.try_get("size").map_err(|e| e.to_string())?,
                etag: row.try_get("etag").map_err(|e| e.to_string())?,
                content_type: row.try_get("content_type").map_err(|e| e.to_string())?,
                storage_backend: row.try_get("storage_backend").map_err(|e| e.to_string())?,
                storage_path: row.try_get("storage_path").map_err(|e| e.to_string())?,
                org_id: Uuid::from_slice(&org_id_bytes).map_err(|e| e.to_string())?,
                env_id: env_id_bytes // Added env_id
                    .map(|b| Uuid::from_slice(b.as_slice()))
                    .transpose()
                    .map_err(|e| e.to_string())?,
                created_by_run_id: created_by_run_id_bytes
                    .map(|b| Uuid::from_slice(b.as_slice()))
                    .transpose()
                    .map_err(|e| e.to_string())?,
                updated_by_run_id: updated_by_run_id_bytes
                    .map(|b| Uuid::from_slice(b.as_slice()))
                    .transpose()
                    .map_err(|e| e.to_string())?,
                created_at: row.try_get("created_at").map_err(|e| e.to_string())?,
                updated_at: row.try_get("updated_at").map_err(|e| e.to_string())?,
                created_by_user_id: Uuid::from_slice(&created_by_user_id_bytes)
                    .map_err(|e| e.to_string())?,
                updated_by_user_id: updated_by_user_id_bytes
                    .map(|b| Uuid::from_slice(b.as_slice()))
                    .transpose()
                    .map_err(|e| e.to_string())?,
            })
        }
    }
}

/// Get file record by file_id
pub async fn get_file_by_id(db: &DatabasePool, file_id: Uuid) -> Result<FileRecord, String> {
    let query = match db {
        DatabasePool::Postgres(_) => {
            r#"
            SELECT file_id, path, size, etag, content_type,
                   storage_backend, storage_path, org_id, env_id,
                   created_by_run_id, updated_by_run_id,
                   created_at, updated_at, created_by_user_id, updated_by_user_id
            FROM file
            WHERE file_id = $1 AND active = true
            "#
        }
        DatabasePool::Sqlite(_) => {
            r#"
            SELECT file_id, path, size, etag, content_type,
                   storage_backend, storage_path, org_id, env_id,
                   created_by_run_id, updated_by_run_id,
                   created_at, updated_at, created_by_user_id, updated_by_user_id
            FROM file
            WHERE file_id = ? AND active = 1
            "#
        }
    };

    match db {
        DatabasePool::Postgres(pool) => {
            let row = sqlx::query(query)
                .bind(file_id)
                .fetch_one(pool)
                .await
                .map_err(|e| format!("File not found: {}", e))?;

            Ok(FileRecord {
                file_id: row.try_get("file_id").map_err(|e| e.to_string())?,
                path: row.try_get("path").map_err(|e| e.to_string())?,
                size: row.try_get("size").map_err(|e| e.to_string())?,
                etag: row.try_get("etag").map_err(|e| e.to_string())?,
                content_type: row.try_get("content_type").map_err(|e| e.to_string())?,
                storage_backend: row.try_get("storage_backend").map_err(|e| e.to_string())?,
                storage_path: row.try_get("storage_path").map_err(|e| e.to_string())?,
                org_id: row.try_get("org_id").map_err(|e| e.to_string())?,
                env_id: row.try_get("env_id").map_err(|e| e.to_string())?,
                created_by_run_id: row
                    .try_get("created_by_run_id")
                    .map_err(|e| e.to_string())?,
                updated_by_run_id: row
                    .try_get("updated_by_run_id")
                    .map_err(|e| e.to_string())?,
                created_at: row.try_get("created_at").map_err(|e| e.to_string())?,
                updated_at: row.try_get("updated_at").map_err(|e| e.to_string())?,
                created_by_user_id: row
                    .try_get("created_by_user_id")
                    .map_err(|e| e.to_string())?,
                updated_by_user_id: row
                    .try_get("updated_by_user_id")
                    .map_err(|e| e.to_string())?,
            })
        }
        DatabasePool::Sqlite(pool) => {
            let row = sqlx::query(query)
                .bind(file_id)
                .fetch_one(pool)
                .await
                .map_err(|e| format!("File not found: {}", e))?;

            let file_id_bytes: Vec<u8> = row.try_get("file_id").map_err(|e| e.to_string())?;
            let org_id_bytes: Vec<u8> = row.try_get("org_id").map_err(|e| e.to_string())?;
            let env_id_bytes: Option<Vec<u8>> = row.try_get("env_id").map_err(|e| e.to_string())?;
            let created_by_user_id_bytes: Vec<u8> = row
                .try_get("created_by_user_id")
                .map_err(|e| e.to_string())?;
            let updated_by_user_id_bytes: Option<Vec<u8>> = row
                .try_get("updated_by_user_id")
                .map_err(|e| e.to_string())?;
            let created_by_run_id_bytes: Option<Vec<u8>> = row
                .try_get("created_by_run_id")
                .map_err(|e| e.to_string())?;
            let updated_by_run_id_bytes: Option<Vec<u8>> = row
                .try_get("updated_by_run_id")
                .map_err(|e| e.to_string())?;

            Ok(FileRecord {
                file_id: Uuid::from_slice(&file_id_bytes).map_err(|e| e.to_string())?,
                path: row.try_get("path").map_err(|e| e.to_string())?,
                size: row.try_get("size").map_err(|e| e.to_string())?,
                etag: row.try_get("etag").map_err(|e| e.to_string())?,
                content_type: row.try_get("content_type").map_err(|e| e.to_string())?,
                storage_backend: row.try_get("storage_backend").map_err(|e| e.to_string())?,
                storage_path: row.try_get("storage_path").map_err(|e| e.to_string())?,
                org_id: Uuid::from_slice(&org_id_bytes).map_err(|e| e.to_string())?,
                env_id: env_id_bytes
                    .map(|b| Uuid::from_slice(b.as_slice()))
                    .transpose()
                    .map_err(|e| e.to_string())?,
                created_by_run_id: created_by_run_id_bytes
                    .map(|b| Uuid::from_slice(b.as_slice()))
                    .transpose()
                    .map_err(|e| e.to_string())?,
                updated_by_run_id: updated_by_run_id_bytes
                    .map(|b| Uuid::from_slice(b.as_slice()))
                    .transpose()
                    .map_err(|e| e.to_string())?,
                created_at: row.try_get("created_at").map_err(|e| e.to_string())?,
                updated_at: row.try_get("updated_at").map_err(|e| e.to_string())?,
                created_by_user_id: Uuid::from_slice(&created_by_user_id_bytes)
                    .map_err(|e| e.to_string())?,
                updated_by_user_id: updated_by_user_id_bytes
                    .map(|b| Uuid::from_slice(b.as_slice()))
                    .transpose()
                    .map_err(|e| e.to_string())?,
            })
        }
    }
}

/// Mark file as inactive (soft delete)
pub async fn mark_file_inactive(
    db: &DatabasePool,
    path: &str,
    org_id: Uuid,
    env_id: Option<Uuid>,
    updated_by_user_id: Uuid,
    updated_by_run_id: Option<Uuid>,
) -> Result<(), String> {
    let now = Utc::now();

    let query = match db {
        DatabasePool::Postgres(_) => {
            r#"
            UPDATE file SET
                active = false,
                active_toggle_at = $5,
                active_toggle_by_user_id = $4,
                updated_by_run_id = $6,
                updated_at = $7
            WHERE path = $1
              AND org_id = $2
              AND (env_id = $3 OR (env_id IS NULL AND $3 IS NULL))
              AND active = true
            "#
        }
        DatabasePool::Sqlite(_pool) => {
            r#"
            UPDATE file SET
                active = 0,
                active_toggle_at = ?,
                active_toggle_by_user_id = ?,
                updated_by_run_id = ?,
                updated_at = ?
            WHERE path = ?
              AND org_id = ?
              AND (env_id = ? OR (env_id IS NULL AND ? IS NULL))
              AND active = 1
            "#
        }
    };

    match db {
        DatabasePool::Postgres(pool) => {
            sqlx::query(query)
                .bind(path)
                .bind(org_id)
                .bind(env_id)
                .bind(updated_by_user_id)
                .bind(now)
                .bind(updated_by_run_id)
                .bind(now)
                .execute(pool)
                .await
                .map_err(|e| format!("Failed to mark file inactive: {}", e))?;
        }
        DatabasePool::Sqlite(pool) => {
            sqlx::query(query)
                .bind(now)
                .bind(updated_by_user_id)
                .bind(updated_by_run_id)
                .bind(now)
                .bind(path)
                .bind(org_id)
                .bind(env_id)
                .bind(env_id)
                .execute(pool)
                .await
                .map_err(|e| format!("Failed to mark file inactive: {}", e))?;
        }
    }

    Ok(())
}

/// List files by prefix
pub async fn list_files_by_prefix(
    db: &DatabasePool,
    prefix: &str,
    org_id: Uuid,
    env_id: Option<Uuid>,
) -> Result<Vec<FileRecord>, String> {
    let query = match db {
        DatabasePool::Postgres(_) => {
            r#"
            SELECT file_id, path, size, etag, content_type,
                   storage_backend, storage_path, org_id, env_id,
                   created_by_run_id, updated_by_run_id,
                   created_at, updated_at, created_by_user_id, updated_by_user_id
            FROM file
            WHERE org_id = $1
              AND (env_id = $2 OR (env_id IS NULL AND $2 IS NULL))
              AND path LIKE $3
              AND active = true
            ORDER BY path
            "#
        }
        DatabasePool::Sqlite(_) => {
            r#"
            SELECT file_id, path, size, etag, content_type,
                   storage_backend, storage_path, org_id, env_id,
                   created_by_run_id, updated_by_run_id,
                   created_at, updated_at, created_by_user_id, updated_by_user_id
            FROM file
            WHERE org_id = ?
              AND (env_id = ? OR (env_id IS NULL AND ? IS NULL))
              AND path LIKE ?
              AND active = 1
            ORDER BY path
            "#
        }
    };

    let pattern = format!("{}%", prefix);

    match db {
        DatabasePool::Postgres(pool) => {
            let rows = sqlx::query(query)
                .bind(org_id)
                .bind(env_id)
                .bind(&pattern)
                .fetch_all(pool)
                .await
                .map_err(|e| format!("Failed to list files: {}", e))?;

            rows.iter()
                .map(|row| {
                    Ok(FileRecord {
                        file_id: row.try_get("file_id").map_err(|e| e.to_string())?,
                        path: row.try_get("path").map_err(|e| e.to_string())?,
                        size: row.try_get("size").map_err(|e| e.to_string())?,
                        etag: row.try_get("etag").map_err(|e| e.to_string())?,
                        content_type: row.try_get("content_type").map_err(|e| e.to_string())?,
                        storage_backend: row
                            .try_get("storage_backend")
                            .map_err(|e| e.to_string())?,
                        storage_path: row.try_get("storage_path").map_err(|e| e.to_string())?,
                        org_id: row.try_get("org_id").map_err(|e| e.to_string())?,
                        env_id: row.try_get("env_id").map_err(|e| e.to_string())?,
                        created_by_run_id: row
                            .try_get("created_by_run_id")
                            .map_err(|e| e.to_string())?,
                        updated_by_run_id: row
                            .try_get("updated_by_run_id")
                            .map_err(|e| e.to_string())?,
                        created_at: row.try_get("created_at").map_err(|e| e.to_string())?,
                        updated_at: row.try_get("updated_at").map_err(|e| e.to_string())?,
                        created_by_user_id: row
                            .try_get("created_by_user_id")
                            .map_err(|e| e.to_string())?,
                        updated_by_user_id: row
                            .try_get("updated_by_user_id")
                            .map_err(|e| e.to_string())?,
                    })
                })
                .collect()
        }
        DatabasePool::Sqlite(pool) => {
            let rows = sqlx::query(query)
                .bind(org_id)
                .bind(env_id)
                .bind(env_id)
                .bind(&pattern)
                .fetch_all(pool)
                .await
                .map_err(|e| format!("Failed to list files: {}", e))?;

            rows.iter()
                .map(|row| {
                    let file_id_bytes: Vec<u8> =
                        row.try_get("file_id").map_err(|e| e.to_string())?;
                    let org_id_bytes: Vec<u8> = row.try_get("org_id").map_err(|e| e.to_string())?;
                    let env_id_bytes: Option<Vec<u8>> =
                        row.try_get("env_id").map_err(|e| e.to_string())?;
                    let created_by_user_id_bytes: Vec<u8> = row
                        .try_get("created_by_user_id")
                        .map_err(|e| e.to_string())?;
                    let updated_by_user_id_bytes: Option<Vec<u8>> = row
                        .try_get("updated_by_user_id")
                        .map_err(|e| e.to_string())?;
                    let created_by_run_id_bytes: Option<Vec<u8>> = row
                        .try_get("created_by_run_id")
                        .map_err(|e| e.to_string())?;
                    let updated_by_run_id_bytes: Option<Vec<u8>> = row
                        .try_get("updated_by_run_id")
                        .map_err(|e| e.to_string())?;

                    Ok(FileRecord {
                        file_id: Uuid::from_slice(&file_id_bytes).map_err(|e| e.to_string())?,
                        path: row.try_get("path").map_err(|e| e.to_string())?,
                        size: row.try_get("size").map_err(|e| e.to_string())?,
                        etag: row.try_get("etag").map_err(|e| e.to_string())?,
                        content_type: row.try_get("content_type").map_err(|e| e.to_string())?,
                        storage_backend: row
                            .try_get("storage_backend")
                            .map_err(|e| e.to_string())?,
                        storage_path: row.try_get("storage_path").map_err(|e| e.to_string())?,
                        org_id: Uuid::from_slice(&org_id_bytes).map_err(|e| e.to_string())?,
                        env_id: env_id_bytes
                            .map(|b| Uuid::from_slice(&b))
                            .transpose()
                            .map_err(|e| e.to_string())?,
                        created_by_run_id: created_by_run_id_bytes
                            .map(|b| Uuid::from_slice(&b))
                            .transpose()
                            .map_err(|e| e.to_string())?,
                        updated_by_run_id: updated_by_run_id_bytes
                            .map(|b| Uuid::from_slice(&b))
                            .transpose()
                            .map_err(|e| e.to_string())?,
                        created_at: row.try_get("created_at").map_err(|e| e.to_string())?,
                        updated_at: row.try_get("updated_at").map_err(|e| e.to_string())?,
                        created_by_user_id: Uuid::from_slice(&created_by_user_id_bytes)
                            .map_err(|e| e.to_string())?,
                        updated_by_user_id: updated_by_user_id_bytes
                            .map(|b| Uuid::from_slice(&b))
                            .transpose()
                            .map_err(|e| e.to_string())?,
                    })
                })
                .collect()
        }
    }
}

/// Get files by run_id (created or updated by a specific run)
pub async fn get_files_by_run(
    db: &DatabasePool,
    run_id: Uuid,
    limit: Option<i64>,
    offset: Option<i64>,
) -> Result<Vec<FileRecord>, String> {
    let limit = limit.unwrap_or(50);
    let offset = offset.unwrap_or(0);

    let query = match db {
        DatabasePool::Postgres(_) => {
            r#"
            SELECT file_id, path, size, etag, content_type,
                   storage_backend, storage_path, org_id, env_id,
                   created_by_run_id, updated_by_run_id,
                   created_at, updated_at, created_by_user_id, updated_by_user_id
            FROM file
            WHERE (created_by_run_id = $1 OR updated_by_run_id = $1) AND active = true
            ORDER BY updated_at DESC
            LIMIT $2 OFFSET $3
            "#
        }
        DatabasePool::Sqlite(_) => {
            r#"
            SELECT file_id, path, size, etag, content_type,
                   storage_backend, storage_path, org_id, env_id,
                   created_by_run_id, updated_by_run_id,
                   created_at, updated_at, created_by_user_id, updated_by_user_id
            FROM file
            WHERE (created_by_run_id = ? OR updated_by_run_id = ?) AND active = 1
            ORDER BY updated_at DESC
            LIMIT ? OFFSET ?
            "#
        }
    };

    match db {
        DatabasePool::Postgres(pool) => {
            let rows = sqlx::query(query)
                .bind(run_id)
                .bind(limit)
                .bind(offset)
                .fetch_all(pool)
                .await
                .map_err(|e| format!("Failed to get files by run: {}", e))?;

            rows.iter()
                .map(|row| {
                    Ok(FileRecord {
                        file_id: row.try_get("file_id").map_err(|e| e.to_string())?,
                        path: row.try_get("path").map_err(|e| e.to_string())?,
                        size: row.try_get("size").map_err(|e| e.to_string())?,
                        etag: row.try_get("etag").map_err(|e| e.to_string())?,
                        content_type: row.try_get("content_type").map_err(|e| e.to_string())?,
                        storage_backend: row
                            .try_get("storage_backend")
                            .map_err(|e| e.to_string())?,
                        storage_path: row.try_get("storage_path").map_err(|e| e.to_string())?,
                        org_id: row.try_get("org_id").map_err(|e| e.to_string())?,
                        env_id: row.try_get("env_id").map_err(|e| e.to_string())?,
                        created_by_run_id: row
                            .try_get("created_by_run_id")
                            .map_err(|e| e.to_string())?,
                        updated_by_run_id: row
                            .try_get("updated_by_run_id")
                            .map_err(|e| e.to_string())?,
                        created_at: row.try_get("created_at").map_err(|e| e.to_string())?,
                        updated_at: row.try_get("updated_at").map_err(|e| e.to_string())?,
                        created_by_user_id: row
                            .try_get("created_by_user_id")
                            .map_err(|e| e.to_string())?,
                        updated_by_user_id: row
                            .try_get("updated_by_user_id")
                            .map_err(|e| e.to_string())?,
                    })
                })
                .collect()
        }
        DatabasePool::Sqlite(pool) => {
            let rows = sqlx::query(query)
                .bind(run_id)
                .bind(run_id)
                .bind(limit)
                .bind(offset)
                .fetch_all(pool)
                .await
                .map_err(|e| format!("Failed to get files by run: {}", e))?;

            rows.iter()
                .map(|row| {
                    let file_id_bytes: Vec<u8> =
                        row.try_get("file_id").map_err(|e| e.to_string())?;
                    let org_id_bytes: Vec<u8> = row.try_get("org_id").map_err(|e| e.to_string())?;
                    let env_id_bytes: Option<Vec<u8>> =
                        row.try_get("env_id").map_err(|e| e.to_string())?;
                    let created_by_user_id_bytes: Vec<u8> = row
                        .try_get("created_by_user_id")
                        .map_err(|e| e.to_string())?;
                    let updated_by_user_id_bytes: Option<Vec<u8>> = row
                        .try_get("updated_by_user_id")
                        .map_err(|e| e.to_string())?;
                    let created_by_run_id_bytes: Option<Vec<u8>> = row
                        .try_get("created_by_run_id")
                        .map_err(|e| e.to_string())?;
                    let updated_by_run_id_bytes: Option<Vec<u8>> = row
                        .try_get("updated_by_run_id")
                        .map_err(|e| e.to_string())?;

                    Ok(FileRecord {
                        file_id: Uuid::from_slice(&file_id_bytes).map_err(|e| e.to_string())?,
                        path: row.try_get("path").map_err(|e| e.to_string())?,
                        size: row.try_get("size").map_err(|e| e.to_string())?,
                        etag: row.try_get("etag").map_err(|e| e.to_string())?,
                        content_type: row.try_get("content_type").map_err(|e| e.to_string())?,
                        storage_backend: row
                            .try_get("storage_backend")
                            .map_err(|e| e.to_string())?,
                        storage_path: row.try_get("storage_path").map_err(|e| e.to_string())?,
                        org_id: Uuid::from_slice(&org_id_bytes).map_err(|e| e.to_string())?,
                        env_id: env_id_bytes
                            .map(|b| Uuid::from_slice(&b))
                            .transpose()
                            .map_err(|e| e.to_string())?,
                        created_by_run_id: created_by_run_id_bytes
                            .map(|b| Uuid::from_slice(&b))
                            .transpose()
                            .map_err(|e| e.to_string())?,
                        updated_by_run_id: updated_by_run_id_bytes
                            .map(|b| Uuid::from_slice(&b))
                            .transpose()
                            .map_err(|e| e.to_string())?,
                        created_at: row.try_get("created_at").map_err(|e| e.to_string())?,
                        updated_at: row.try_get("updated_at").map_err(|e| e.to_string())?,
                        created_by_user_id: Uuid::from_slice(&created_by_user_id_bytes)
                            .map_err(|e| e.to_string())?,
                        updated_by_user_id: updated_by_user_id_bytes
                            .map(|b| Uuid::from_slice(&b))
                            .transpose()
                            .map_err(|e| e.to_string())?,
                    })
                })
                .collect()
        }
    }
}

/// Get count of files by run_id
pub async fn get_file_count_by_run(db: &DatabasePool, run_id: Uuid) -> Result<i64, String> {
    let query = match db {
        DatabasePool::Postgres(_) => {
            r#"
            SELECT COUNT(*) as count
            FROM file
            WHERE (created_by_run_id = $1 OR updated_by_run_id = $1) AND active = true
            "#
        }
        DatabasePool::Sqlite(_) => {
            r#"
            SELECT COUNT(*) as count
            FROM file
            WHERE (created_by_run_id = ? OR updated_by_run_id = ?) AND active = 1
            "#
        }
    };

    match db {
        DatabasePool::Postgres(pool) => {
            let row = sqlx::query(query)
                .bind(run_id)
                .fetch_one(pool)
                .await
                .map_err(|e| format!("Failed to get file count by run: {}", e))?;

            row.try_get("count").map_err(|e| e.to_string())
        }
        DatabasePool::Sqlite(pool) => {
            let row = sqlx::query(query)
                .bind(run_id)
                .bind(run_id)
                .fetch_one(pool)
                .await
                .map_err(|e| format!("Failed to get file count by run: {}", e))?;

            row.try_get("count").map_err(|e| e.to_string())
        }
    }
}

/// Get file by ID with org/env validation for security
pub async fn get_file_by_id_secure(
    db: &DatabasePool,
    file_id: Uuid,
    org_id: Uuid,
    env_id: Option<Uuid>,
) -> Result<FileRecord, String> {
    let query = match db {
        DatabasePool::Postgres(_) => {
            r#"
            SELECT file_id, path, size, etag, content_type,
                   storage_backend, storage_path, org_id, env_id,
                   created_by_run_id, updated_by_run_id,
                   created_at, updated_at, created_by_user_id, updated_by_user_id
            FROM file
            WHERE file_id = $1 AND org_id = $2 AND (env_id = $3 OR (env_id IS NULL AND $3 IS NULL)) AND active = true
            "#
        }
        DatabasePool::Sqlite(_) => {
            r#"
            SELECT file_id, path, size, etag, content_type,
                   storage_backend, storage_path, org_id, env_id,
                   created_by_run_id, updated_by_run_id,
                   created_at, updated_at, created_by_user_id, updated_by_user_id
            FROM file
            WHERE file_id = ? AND org_id = ? AND (env_id = ? OR (env_id IS NULL AND ? IS NULL)) AND active = 1
            "#
        }
    };

    match db {
        DatabasePool::Postgres(pool) => {
            let row = sqlx::query(query)
                .bind(file_id)
                .bind(org_id)
                .bind(env_id)
                .fetch_one(pool)
                .await
                .map_err(|e| format!("File not found: {}", e))?;

            Ok(FileRecord {
                file_id: row.try_get("file_id").map_err(|e| e.to_string())?,
                path: row.try_get("path").map_err(|e| e.to_string())?,
                size: row.try_get("size").map_err(|e| e.to_string())?,
                etag: row.try_get("etag").map_err(|e| e.to_string())?,
                content_type: row.try_get("content_type").map_err(|e| e.to_string())?,
                storage_backend: row.try_get("storage_backend").map_err(|e| e.to_string())?,
                storage_path: row.try_get("storage_path").map_err(|e| e.to_string())?,
                org_id: row.try_get("org_id").map_err(|e| e.to_string())?,
                env_id: row.try_get("env_id").map_err(|e| e.to_string())?,
                created_by_run_id: row
                    .try_get("created_by_run_id")
                    .map_err(|e| e.to_string())?,
                updated_by_run_id: row
                    .try_get("updated_by_run_id")
                    .map_err(|e| e.to_string())?,
                created_at: row.try_get("created_at").map_err(|e| e.to_string())?,
                updated_at: row.try_get("updated_at").map_err(|e| e.to_string())?,
                created_by_user_id: row
                    .try_get("created_by_user_id")
                    .map_err(|e| e.to_string())?,
                updated_by_user_id: row
                    .try_get("updated_by_user_id")
                    .map_err(|e| e.to_string())?,
            })
        }
        DatabasePool::Sqlite(pool) => {
            let row = sqlx::query(query)
                .bind(file_id)
                .bind(org_id)
                .bind(env_id)
                .bind(env_id)
                .fetch_one(pool)
                .await
                .map_err(|e| format!("File not found: {}", e))?;

            let file_id_bytes: Vec<u8> = row.try_get("file_id").map_err(|e| e.to_string())?;
            let org_id_bytes: Vec<u8> = row.try_get("org_id").map_err(|e| e.to_string())?;
            let env_id_bytes: Option<Vec<u8>> = row.try_get("env_id").map_err(|e| e.to_string())?;
            let created_by_user_id_bytes: Vec<u8> = row
                .try_get("created_by_user_id")
                .map_err(|e| e.to_string())?;
            let updated_by_user_id_bytes: Option<Vec<u8>> = row
                .try_get("updated_by_user_id")
                .map_err(|e| e.to_string())?;
            let created_by_run_id_bytes: Option<Vec<u8>> = row
                .try_get("created_by_run_id")
                .map_err(|e| e.to_string())?;
            let updated_by_run_id_bytes: Option<Vec<u8>> = row
                .try_get("updated_by_run_id")
                .map_err(|e| e.to_string())?;

            Ok(FileRecord {
                file_id: Uuid::from_slice(&file_id_bytes).map_err(|e| e.to_string())?,
                path: row.try_get("path").map_err(|e| e.to_string())?,
                size: row.try_get("size").map_err(|e| e.to_string())?,
                etag: row.try_get("etag").map_err(|e| e.to_string())?,
                content_type: row.try_get("content_type").map_err(|e| e.to_string())?,
                storage_backend: row.try_get("storage_backend").map_err(|e| e.to_string())?,
                storage_path: row.try_get("storage_path").map_err(|e| e.to_string())?,
                org_id: Uuid::from_slice(&org_id_bytes).map_err(|e| e.to_string())?,
                env_id: env_id_bytes
                    .map(|b| Uuid::from_slice(b.as_slice()))
                    .transpose()
                    .map_err(|e| e.to_string())?,
                created_by_run_id: created_by_run_id_bytes
                    .map(|b| Uuid::from_slice(b.as_slice()))
                    .transpose()
                    .map_err(|e| e.to_string())?,
                updated_by_run_id: updated_by_run_id_bytes
                    .map(|b| Uuid::from_slice(b.as_slice()))
                    .transpose()
                    .map_err(|e| e.to_string())?,
                created_at: row.try_get("created_at").map_err(|e| e.to_string())?,
                updated_at: row.try_get("updated_at").map_err(|e| e.to_string())?,
                created_by_user_id: Uuid::from_slice(&created_by_user_id_bytes)
                    .map_err(|e| e.to_string())?,
                updated_by_user_id: updated_by_user_id_bytes
                    .map(|b| Uuid::from_slice(b.as_slice()))
                    .transpose()
                    .map_err(|e| e.to_string())?,
            })
        }
    }
}

/// List files by environment with pagination, search, and time range filtering.
///
/// `time_range_cutoff` is the earliest `created_at` timestamp to include
/// (inclusive); pass `None` for "all time". Compute via
/// [`crate::time_range::parse_time_range_cutoff`].
#[allow(clippy::too_many_arguments)]
pub async fn list_files_by_env(
    db: &DatabasePool,
    env_id: Uuid,
    search_term: Option<&str>,
    time_range_cutoff: Option<chrono::DateTime<chrono::Utc>>,
    limit: Option<i64>,
    offset: Option<i64>,
) -> Result<Vec<FileRecord>, String> {
    let limit = limit.unwrap_or(50);
    let offset = offset.unwrap_or(0);

    // Check if search term is a UUID (for run_id search)
    let search_uuid: Option<Uuid> = search_term.and_then(|s| Uuid::parse_str(s).ok());

    // Build dynamic WHERE clauses
    let mut conditions = Vec::new();
    conditions.push("env_id = $1".to_string());
    conditions.push("active = true".to_string());

    let mut param_idx = 2;

    if search_term.is_some() {
        if search_uuid.is_some() {
            // Search by run_id (created_by or updated_by)
            conditions.push(format!(
                "(created_by_run_id = ${} OR updated_by_run_id = ${})",
                param_idx, param_idx
            ));
            param_idx += 1;
        } else {
            // Search by path
            conditions.push(format!("path ILIKE ${}", param_idx));
            param_idx += 1;
        }
    }

    if time_range_cutoff.is_some() {
        conditions.push(format!("created_at >= ${}", param_idx));
        param_idx += 1;
    }

    let where_clause = conditions.join(" AND ");

    let query = format!(
        r#"
        SELECT file_id, path, size, etag, content_type,
               storage_backend, storage_path, org_id, env_id,
               created_by_run_id, updated_by_run_id,
               created_at, updated_at, created_by_user_id, updated_by_user_id
        FROM file
        WHERE {}
        ORDER BY updated_at DESC
        LIMIT ${} OFFSET ${}
        "#,
        where_clause,
        param_idx,
        param_idx + 1
    );

    match db {
        DatabasePool::Postgres(pool) => {
            let mut q = sqlx::query(&query).bind(env_id);

            if let Some(uuid) = search_uuid {
                q = q.bind(uuid);
            } else if let Some(term) = search_term {
                q = q.bind(format!("%{}%", term));
            }

            if let Some(cutoff) = time_range_cutoff {
                q = q.bind(cutoff);
            }

            q = q.bind(limit).bind(offset);

            let rows = q
                .fetch_all(pool)
                .await
                .map_err(|e| format!("Failed to list files: {}", e))?;

            rows.iter()
                .map(|row| {
                    Ok(FileRecord {
                        file_id: row.try_get("file_id").map_err(|e| e.to_string())?,
                        path: row.try_get("path").map_err(|e| e.to_string())?,
                        size: row.try_get("size").map_err(|e| e.to_string())?,
                        etag: row.try_get("etag").map_err(|e| e.to_string())?,
                        content_type: row.try_get("content_type").map_err(|e| e.to_string())?,
                        storage_backend: row
                            .try_get("storage_backend")
                            .map_err(|e| e.to_string())?,
                        storage_path: row.try_get("storage_path").map_err(|e| e.to_string())?,
                        org_id: row.try_get("org_id").map_err(|e| e.to_string())?,
                        env_id: row.try_get("env_id").map_err(|e| e.to_string())?,
                        created_by_run_id: row
                            .try_get("created_by_run_id")
                            .map_err(|e| e.to_string())?,
                        updated_by_run_id: row
                            .try_get("updated_by_run_id")
                            .map_err(|e| e.to_string())?,
                        created_at: row.try_get("created_at").map_err(|e| e.to_string())?,
                        updated_at: row.try_get("updated_at").map_err(|e| e.to_string())?,
                        created_by_user_id: row
                            .try_get("created_by_user_id")
                            .map_err(|e| e.to_string())?,
                        updated_by_user_id: row
                            .try_get("updated_by_user_id")
                            .map_err(|e| e.to_string())?,
                    })
                })
                .collect()
        }
        DatabasePool::Sqlite(pool) => {
            // Build SQLite query with ? placeholders
            let mut conditions = Vec::new();
            conditions.push("env_id = ?".to_string());
            conditions.push("active = 1".to_string());

            if search_term.is_some() {
                if search_uuid.is_some() {
                    // Search by run_id (created_by or updated_by)
                    conditions.push("(created_by_run_id = ? OR updated_by_run_id = ?)".to_string());
                } else {
                    conditions.push("path LIKE ?".to_string());
                }
            }

            if time_range_cutoff.is_some() {
                conditions.push("created_at >= ?".to_string());
            }

            let where_clause = conditions.join(" AND ");
            let sqlite_query = format!(
                r#"
                SELECT file_id, path, size, etag, content_type,
                       storage_backend, storage_path, org_id, env_id,
                       created_by_run_id, updated_by_run_id,
                       created_at, updated_at, created_by_user_id, updated_by_user_id
                FROM file
                WHERE {}
                ORDER BY updated_at DESC
                LIMIT ? OFFSET ?
                "#,
                where_clause
            );

            let mut q = sqlx::query(&sqlite_query).bind(env_id);

            if let Some(uuid) = search_uuid {
                // Bind the UUID twice for the OR condition
                q = q.bind(uuid).bind(uuid);
            } else if let Some(term) = search_term {
                q = q.bind(format!("%{}%", term));
            }

            if let Some(cutoff) = time_range_cutoff {
                q = q.bind(cutoff);
            }

            q = q.bind(limit).bind(offset);

            let rows = q
                .fetch_all(pool)
                .await
                .map_err(|e| format!("Failed to list files: {}", e))?;

            rows.iter()
                .map(|row| {
                    let file_id_bytes: Vec<u8> =
                        row.try_get("file_id").map_err(|e| e.to_string())?;
                    let org_id_bytes: Vec<u8> = row.try_get("org_id").map_err(|e| e.to_string())?;
                    let env_id_bytes: Option<Vec<u8>> =
                        row.try_get("env_id").map_err(|e| e.to_string())?;
                    let created_by_user_id_bytes: Vec<u8> = row
                        .try_get("created_by_user_id")
                        .map_err(|e| e.to_string())?;
                    let updated_by_user_id_bytes: Option<Vec<u8>> = row
                        .try_get("updated_by_user_id")
                        .map_err(|e| e.to_string())?;
                    let created_by_run_id_bytes: Option<Vec<u8>> = row
                        .try_get("created_by_run_id")
                        .map_err(|e| e.to_string())?;
                    let updated_by_run_id_bytes: Option<Vec<u8>> = row
                        .try_get("updated_by_run_id")
                        .map_err(|e| e.to_string())?;

                    Ok(FileRecord {
                        file_id: Uuid::from_slice(&file_id_bytes).map_err(|e| e.to_string())?,
                        path: row.try_get("path").map_err(|e| e.to_string())?,
                        size: row.try_get("size").map_err(|e| e.to_string())?,
                        etag: row.try_get("etag").map_err(|e| e.to_string())?,
                        content_type: row.try_get("content_type").map_err(|e| e.to_string())?,
                        storage_backend: row
                            .try_get("storage_backend")
                            .map_err(|e| e.to_string())?,
                        storage_path: row.try_get("storage_path").map_err(|e| e.to_string())?,
                        org_id: Uuid::from_slice(&org_id_bytes).map_err(|e| e.to_string())?,
                        env_id: env_id_bytes
                            .map(|b| Uuid::from_slice(&b))
                            .transpose()
                            .map_err(|e| e.to_string())?,
                        created_by_run_id: created_by_run_id_bytes
                            .map(|b| Uuid::from_slice(&b))
                            .transpose()
                            .map_err(|e| e.to_string())?,
                        updated_by_run_id: updated_by_run_id_bytes
                            .map(|b| Uuid::from_slice(&b))
                            .transpose()
                            .map_err(|e| e.to_string())?,
                        created_at: row.try_get("created_at").map_err(|e| e.to_string())?,
                        updated_at: row.try_get("updated_at").map_err(|e| e.to_string())?,
                        created_by_user_id: Uuid::from_slice(&created_by_user_id_bytes)
                            .map_err(|e| e.to_string())?,
                        updated_by_user_id: updated_by_user_id_bytes
                            .map(|b| Uuid::from_slice(&b))
                            .transpose()
                            .map_err(|e| e.to_string())?,
                    })
                })
                .collect()
        }
    }
}

/// Get count of files by environment with optional filters.
///
/// See [`list_files_by_env`] for the meaning of `time_range_cutoff`.
pub async fn get_files_count_by_env(
    db: &DatabasePool,
    env_id: Uuid,
    search_term: Option<&str>,
    time_range_cutoff: Option<chrono::DateTime<chrono::Utc>>,
) -> Result<i64, String> {
    // Check if search term is a UUID (for run_id search)
    let search_uuid: Option<Uuid> = search_term.and_then(|s| Uuid::parse_str(s).ok());

    match db {
        DatabasePool::Postgres(pool) => {
            let mut conditions = Vec::new();
            conditions.push("env_id = $1".to_string());
            conditions.push("active = true".to_string());

            let mut param_idx = 2;

            if search_term.is_some() {
                if search_uuid.is_some() {
                    // Search by run_id (created_by or updated_by)
                    conditions.push(format!(
                        "(created_by_run_id = ${} OR updated_by_run_id = ${})",
                        param_idx, param_idx
                    ));
                    param_idx += 1;
                } else {
                    conditions.push(format!("path ILIKE ${}", param_idx));
                    param_idx += 1;
                }
            }

            if time_range_cutoff.is_some() {
                conditions.push(format!("created_at >= ${}", param_idx));
            }

            let where_clause = conditions.join(" AND ");
            let query = format!("SELECT COUNT(*) as count FROM file WHERE {}", where_clause);

            let mut q = sqlx::query_scalar::<_, i64>(&query).bind(env_id);

            if let Some(uuid) = search_uuid {
                q = q.bind(uuid);
            } else if let Some(term) = search_term {
                q = q.bind(format!("%{}%", term));
            }

            if let Some(cutoff) = time_range_cutoff {
                q = q.bind(cutoff);
            }

            q.fetch_one(pool)
                .await
                .map_err(|e| format!("Failed to get files count: {}", e))
        }
        DatabasePool::Sqlite(pool) => {
            let mut conditions = Vec::new();
            conditions.push("env_id = ?".to_string());
            conditions.push("active = 1".to_string());

            if search_term.is_some() {
                if search_uuid.is_some() {
                    // Search by run_id (created_by or updated_by)
                    conditions.push("(created_by_run_id = ? OR updated_by_run_id = ?)".to_string());
                } else {
                    conditions.push("path LIKE ?".to_string());
                }
            }

            if time_range_cutoff.is_some() {
                conditions.push("created_at >= ?".to_string());
            }

            let where_clause = conditions.join(" AND ");
            let query = format!("SELECT COUNT(*) as count FROM file WHERE {}", where_clause);

            let mut q = sqlx::query_scalar::<_, i64>(&query).bind(env_id);

            if let Some(uuid) = search_uuid {
                // Bind the UUID twice for the OR condition
                q = q.bind(uuid).bind(uuid);
            } else if let Some(term) = search_term {
                q = q.bind(format!("%{}%", term));
            }

            if let Some(cutoff) = time_range_cutoff {
                q = q.bind(cutoff);
            }

            q.fetch_one(pool)
                .await
                .map_err(|e| format!("Failed to get files count: {}", e))
        }
    }
}

/// List files created or updated by a specific run
pub async fn list_files_by_run(
    db: &DatabasePool,
    run_id: Uuid,
    limit: Option<i64>,
    offset: Option<i64>,
) -> Result<Vec<FileRecord>, String> {
    let limit = limit.unwrap_or(50);
    let offset = offset.unwrap_or(0);

    let query = match db {
        DatabasePool::Postgres(_) => {
            r#"
            SELECT file_id, path, size, etag, content_type,
                   storage_backend, storage_path, org_id, env_id,
                   created_by_run_id, updated_by_run_id,
                   created_at, updated_at, created_by_user_id, updated_by_user_id
            FROM file
            WHERE (created_by_run_id = $1 OR updated_by_run_id = $1) AND active = true
            ORDER BY updated_at DESC
            LIMIT $2 OFFSET $3
            "#
        }
        DatabasePool::Sqlite(_) => {
            r#"
            SELECT file_id, path, size, etag, content_type,
                   storage_backend, storage_path, org_id, env_id,
                   created_by_run_id, updated_by_run_id,
                   created_at, updated_at, created_by_user_id, updated_by_user_id
            FROM file
            WHERE (created_by_run_id = ? OR updated_by_run_id = ?) AND active = 1
            ORDER BY updated_at DESC
            LIMIT ? OFFSET ?
            "#
        }
    };

    match db {
        DatabasePool::Postgres(pool) => {
            let rows = sqlx::query(query)
                .bind(run_id)
                .bind(limit)
                .bind(offset)
                .fetch_all(pool)
                .await
                .map_err(|e| format!("Failed to list files by run: {}", e))?;

            rows.iter()
                .map(|row| {
                    Ok(FileRecord {
                        file_id: row.try_get("file_id").map_err(|e| e.to_string())?,
                        path: row.try_get("path").map_err(|e| e.to_string())?,
                        size: row.try_get("size").map_err(|e| e.to_string())?,
                        etag: row.try_get("etag").map_err(|e| e.to_string())?,
                        content_type: row.try_get("content_type").map_err(|e| e.to_string())?,
                        storage_backend: row
                            .try_get("storage_backend")
                            .map_err(|e| e.to_string())?,
                        storage_path: row.try_get("storage_path").map_err(|e| e.to_string())?,
                        org_id: row.try_get("org_id").map_err(|e| e.to_string())?,
                        env_id: row.try_get("env_id").map_err(|e| e.to_string())?,
                        created_by_run_id: row
                            .try_get("created_by_run_id")
                            .map_err(|e| e.to_string())?,
                        updated_by_run_id: row
                            .try_get("updated_by_run_id")
                            .map_err(|e| e.to_string())?,
                        created_at: row.try_get("created_at").map_err(|e| e.to_string())?,
                        updated_at: row.try_get("updated_at").map_err(|e| e.to_string())?,
                        created_by_user_id: row
                            .try_get("created_by_user_id")
                            .map_err(|e| e.to_string())?,
                        updated_by_user_id: row
                            .try_get("updated_by_user_id")
                            .map_err(|e| e.to_string())?,
                    })
                })
                .collect()
        }
        DatabasePool::Sqlite(pool) => {
            let rows = sqlx::query(query)
                .bind(run_id)
                .bind(run_id)
                .bind(limit)
                .bind(offset)
                .fetch_all(pool)
                .await
                .map_err(|e| format!("Failed to list files by run: {}", e))?;

            rows.iter()
                .map(|row| {
                    let file_id_bytes: Vec<u8> =
                        row.try_get("file_id").map_err(|e| e.to_string())?;
                    let org_id_bytes: Vec<u8> = row.try_get("org_id").map_err(|e| e.to_string())?;
                    let env_id_bytes: Option<Vec<u8>> =
                        row.try_get("env_id").map_err(|e| e.to_string())?;
                    let created_by_user_id_bytes: Vec<u8> = row
                        .try_get("created_by_user_id")
                        .map_err(|e| e.to_string())?;
                    let updated_by_user_id_bytes: Option<Vec<u8>> = row
                        .try_get("updated_by_user_id")
                        .map_err(|e| e.to_string())?;
                    let created_by_run_id_bytes: Option<Vec<u8>> = row
                        .try_get("created_by_run_id")
                        .map_err(|e| e.to_string())?;
                    let updated_by_run_id_bytes: Option<Vec<u8>> = row
                        .try_get("updated_by_run_id")
                        .map_err(|e| e.to_string())?;

                    Ok(FileRecord {
                        file_id: Uuid::from_slice(&file_id_bytes).map_err(|e| e.to_string())?,
                        path: row.try_get("path").map_err(|e| e.to_string())?,
                        size: row.try_get("size").map_err(|e| e.to_string())?,
                        etag: row.try_get("etag").map_err(|e| e.to_string())?,
                        content_type: row.try_get("content_type").map_err(|e| e.to_string())?,
                        storage_backend: row
                            .try_get("storage_backend")
                            .map_err(|e| e.to_string())?,
                        storage_path: row.try_get("storage_path").map_err(|e| e.to_string())?,
                        org_id: Uuid::from_slice(&org_id_bytes).map_err(|e| e.to_string())?,
                        env_id: env_id_bytes
                            .map(|b| Uuid::from_slice(&b))
                            .transpose()
                            .map_err(|e| e.to_string())?,
                        created_by_run_id: created_by_run_id_bytes
                            .map(|b| Uuid::from_slice(&b))
                            .transpose()
                            .map_err(|e| e.to_string())?,
                        updated_by_run_id: updated_by_run_id_bytes
                            .map(|b| Uuid::from_slice(&b))
                            .transpose()
                            .map_err(|e| e.to_string())?,
                        created_at: row.try_get("created_at").map_err(|e| e.to_string())?,
                        updated_at: row.try_get("updated_at").map_err(|e| e.to_string())?,
                        created_by_user_id: Uuid::from_slice(&created_by_user_id_bytes)
                            .map_err(|e| e.to_string())?,
                        updated_by_user_id: updated_by_user_id_bytes
                            .map(|b| Uuid::from_slice(&b))
                            .transpose()
                            .map_err(|e| e.to_string())?,
                    })
                })
                .collect()
        }
    }
}

/// Get count of files by run
pub async fn get_files_count_by_run(db: &DatabasePool, run_id: Uuid) -> Result<i64, String> {
    let query = match db {
        DatabasePool::Postgres(_) => {
            r#"
            SELECT COUNT(*) as count FROM file
            WHERE (created_by_run_id = $1 OR updated_by_run_id = $1) AND active = true
            "#
        }
        DatabasePool::Sqlite(_) => {
            r#"
            SELECT COUNT(*) as count FROM file
            WHERE (created_by_run_id = ? OR updated_by_run_id = ?) AND active = 1
            "#
        }
    };

    match db {
        DatabasePool::Postgres(pool) => sqlx::query_scalar::<_, i64>(query)
            .bind(run_id)
            .fetch_one(pool)
            .await
            .map_err(|e| format!("Failed to get files count by run: {}", e)),
        DatabasePool::Sqlite(pool) => sqlx::query_scalar::<_, i64>(query)
            .bind(run_id)
            .bind(run_id)
            .fetch_one(pool)
            .await
            .map_err(|e| format!("Failed to get files count by run: {}", e)),
    }
}

/// Get file by ID with environment access check
pub async fn get_file_by_id_with_env_check(
    db: &DatabasePool,
    file_id: Uuid,
    env_id: Uuid,
) -> Result<FileRecord, String> {
    let query = match db {
        DatabasePool::Postgres(_) => {
            r#"
            SELECT file_id, path, size, etag, content_type,
                   storage_backend, storage_path, org_id, env_id,
                   created_by_run_id, updated_by_run_id,
                   created_at, updated_at, created_by_user_id, updated_by_user_id
            FROM file
            WHERE file_id = $1 AND env_id = $2 AND active = true
            "#
        }
        DatabasePool::Sqlite(_) => {
            r#"
            SELECT file_id, path, size, etag, content_type,
                   storage_backend, storage_path, org_id, env_id,
                   created_by_run_id, updated_by_run_id,
                   created_at, updated_at, created_by_user_id, updated_by_user_id
            FROM file
            WHERE file_id = ? AND env_id = ? AND active = 1
            "#
        }
    };

    match db {
        DatabasePool::Postgres(pool) => {
            let row = sqlx::query(query)
                .bind(file_id)
                .bind(env_id)
                .fetch_one(pool)
                .await
                .map_err(|e| format!("File not found: {}", e))?;

            Ok(FileRecord {
                file_id: row.try_get("file_id").map_err(|e| e.to_string())?,
                path: row.try_get("path").map_err(|e| e.to_string())?,
                size: row.try_get("size").map_err(|e| e.to_string())?,
                etag: row.try_get("etag").map_err(|e| e.to_string())?,
                content_type: row.try_get("content_type").map_err(|e| e.to_string())?,
                storage_backend: row.try_get("storage_backend").map_err(|e| e.to_string())?,
                storage_path: row.try_get("storage_path").map_err(|e| e.to_string())?,
                org_id: row.try_get("org_id").map_err(|e| e.to_string())?,
                env_id: row.try_get("env_id").map_err(|e| e.to_string())?,
                created_by_run_id: row
                    .try_get("created_by_run_id")
                    .map_err(|e| e.to_string())?,
                updated_by_run_id: row
                    .try_get("updated_by_run_id")
                    .map_err(|e| e.to_string())?,
                created_at: row.try_get("created_at").map_err(|e| e.to_string())?,
                updated_at: row.try_get("updated_at").map_err(|e| e.to_string())?,
                created_by_user_id: row
                    .try_get("created_by_user_id")
                    .map_err(|e| e.to_string())?,
                updated_by_user_id: row
                    .try_get("updated_by_user_id")
                    .map_err(|e| e.to_string())?,
            })
        }
        DatabasePool::Sqlite(pool) => {
            let row = sqlx::query(query)
                .bind(file_id)
                .bind(env_id)
                .fetch_one(pool)
                .await
                .map_err(|e| format!("File not found: {}", e))?;

            let file_id_bytes: Vec<u8> = row.try_get("file_id").map_err(|e| e.to_string())?;
            let org_id_bytes: Vec<u8> = row.try_get("org_id").map_err(|e| e.to_string())?;
            let env_id_bytes: Option<Vec<u8>> = row.try_get("env_id").map_err(|e| e.to_string())?;
            let created_by_user_id_bytes: Vec<u8> = row
                .try_get("created_by_user_id")
                .map_err(|e| e.to_string())?;
            let updated_by_user_id_bytes: Option<Vec<u8>> = row
                .try_get("updated_by_user_id")
                .map_err(|e| e.to_string())?;
            let created_by_run_id_bytes: Option<Vec<u8>> = row
                .try_get("created_by_run_id")
                .map_err(|e| e.to_string())?;
            let updated_by_run_id_bytes: Option<Vec<u8>> = row
                .try_get("updated_by_run_id")
                .map_err(|e| e.to_string())?;

            Ok(FileRecord {
                file_id: Uuid::from_slice(&file_id_bytes).map_err(|e| e.to_string())?,
                path: row.try_get("path").map_err(|e| e.to_string())?,
                size: row.try_get("size").map_err(|e| e.to_string())?,
                etag: row.try_get("etag").map_err(|e| e.to_string())?,
                content_type: row.try_get("content_type").map_err(|e| e.to_string())?,
                storage_backend: row.try_get("storage_backend").map_err(|e| e.to_string())?,
                storage_path: row.try_get("storage_path").map_err(|e| e.to_string())?,
                org_id: Uuid::from_slice(&org_id_bytes).map_err(|e| e.to_string())?,
                env_id: env_id_bytes
                    .map(|b| Uuid::from_slice(b.as_slice()))
                    .transpose()
                    .map_err(|e| e.to_string())?,
                created_by_run_id: created_by_run_id_bytes
                    .map(|b| Uuid::from_slice(b.as_slice()))
                    .transpose()
                    .map_err(|e| e.to_string())?,
                updated_by_run_id: updated_by_run_id_bytes
                    .map(|b| Uuid::from_slice(b.as_slice()))
                    .transpose()
                    .map_err(|e| e.to_string())?,
                created_at: row.try_get("created_at").map_err(|e| e.to_string())?,
                updated_at: row.try_get("updated_at").map_err(|e| e.to_string())?,
                created_by_user_id: Uuid::from_slice(&created_by_user_id_bytes)
                    .map_err(|e| e.to_string())?,
                updated_by_user_id: updated_by_user_id_bytes
                    .map(|b| Uuid::from_slice(b.as_slice()))
                    .transpose()
                    .map_err(|e| e.to_string())?,
            })
        }
    }
}

/// Get total storage usage for an organization
pub async fn get_storage_usage_by_org(db: &DatabasePool, org_id: Uuid) -> Result<i64, String> {
    let query = match db {
        DatabasePool::Postgres(_) => {
            r#"
            SELECT COALESCE(SUM(size), 0) as total_size
            FROM file
            WHERE org_id = $1 AND active = true
            "#
        }
        DatabasePool::Sqlite(_pool) => {
            r#"
            SELECT COALESCE(SUM(size), 0) as total_size
            FROM file
            WHERE org_id = ? AND active = 1
            "#
        }
    };

    match db {
        DatabasePool::Postgres(pool) => {
            let row = sqlx::query(query)
                .bind(org_id)
                .fetch_one(pool)
                .await
                .map_err(|e| format!("Failed to get storage usage: {}", e))?;

            row.try_get("total_size").map_err(|e| e.to_string())
        }
        DatabasePool::Sqlite(pool) => {
            let row = sqlx::query(query)
                .bind(org_id)
                .fetch_one(pool)
                .await
                .map_err(|e| format!("Failed to get storage usage: {}", e))?;

            row.try_get("total_size").map_err(|e| e.to_string())
        }
    }
}

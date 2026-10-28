//! Database operations for file storage tracking

use crate::db::{DatabasePool, DatabaseScope, MaintenanceOperation};
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

/// Metadata is the commit point for a file upload. On a limited account,
/// every size-increasing write holds the billing root row lock across the
/// account total and metadata update; an unlimited account takes neither the
/// lock nor the total. Staged object bytes are never counted as committed
/// storage.
enum StorageWriteTransaction {
    Postgres(sqlx::Transaction<'static, sqlx::Postgres>),
    Sqlite(sqlx::Transaction<'static, sqlx::Sqlite>),
}

async fn begin_storage_write(
    db: &DatabasePool,
    org_id: Uuid,
    env_id: Option<Uuid>,
    file_id: Option<Uuid>,
    new_size: i64,
) -> Result<StorageWriteTransaction, FileWriteError> {
    if new_size < 0 {
        return Err(fwe("File size cannot be negative"));
    }
    // Validate caller authority before entering the internal billing scope.
    if let DatabasePool::Postgres(pool) = db {
        let scope = pool.scope().map_err(fwe)?;
        let allowed = match scope {
            DatabaseScope::Environment {
                org_id: org,
                env_id: env,
            } => org == org_id && Some(env) == env_id,
            DatabaseScope::Organization { org_id: org } => org == org_id && env_id.is_none(),
            _ => false,
        };
        if !allowed {
            return Err(fwe(
                "File write requires its exact organization/environment scope",
            ));
        }
    }
    let root_id = super::org::Org::billing_owner_id_for(db, &org_id)
        .await
        .map_err(fwe)?;
    let billing = db.with_scope(DatabaseScope::Maintenance {
        operation: MaintenanceOperation::Billing,
        org_id: Some(root_id),
    });
    match &billing {
        DatabasePool::Postgres(pool) => {
            let mut tx = pool.begin().await.map_err(fwe)?;
            // Decide on the plan before locking: an unlimited account needs
            // neither the root lock nor a total over every active file.
            let plan: Option<serde_json::Value> = sqlx::query_scalar(
                "SELECT COALESCE(p.features,'{}'::jsonb) FROM org_plan op JOIN plan p ON p.plan_uuid=op.plan_uuid WHERE op.org_id=$1"
            ).bind(root_id).fetch_optional(&mut *tx).await.map_err(fwe)?.flatten();
            let overrides: Option<serde_json::Value> =
                sqlx::query_scalar("SELECT features FROM org WHERE org_id=$1")
                    .bind(root_id)
                    .fetch_one(&mut *tx)
                    .await
                    .map_err(fwe)?;
            if storage_limit(plan.as_ref(), overrides.as_ref()) >= 0 {
                // NO KEY UPDATE serializes quota writers without blocking the
                // foreign-key checks of inserts that reference the org.
                let overrides: Option<serde_json::Value> = sqlx::query_scalar(
                    "SELECT features FROM org WHERE org_id=$1 FOR NO KEY UPDATE",
                )
                .bind(root_id)
                .fetch_one(&mut *tx)
                .await
                .map_err(fwe)?;
                // The account's active file bytes, kept per environment and
                // organization by triggers on `file` (migration 016).
                let counted: i64 = sqlx::query_scalar(
                    "SELECT COALESCE(SUM(u.active_bytes),0)::bigint FROM usage_file u JOIN org o ON o.org_id=u.org_id WHERE o.org_id=$1 OR o.parent_org_id=$1"
                ).bind(root_id).fetch_one(&mut *tx).await.map_err(fwe)?;
                let replaced: Option<i64> = match file_id {
                    Some(file_id) => {
                        sqlx::query_scalar("SELECT size FROM file WHERE file_id=$1 AND active")
                            .bind(file_id)
                            .fetch_optional(&mut *tx)
                            .await
                            .map_err(fwe)?
                    }
                    None => None,
                };
                // The usage besides the file this write replaces.
                let usage = counted - replaced.unwrap_or(0);
                validate_storage_limit(
                    plan.as_ref(),
                    overrides.as_ref(),
                    usage,
                    new_size,
                    replaced,
                )?;
            }
            let scope = match env_id {
                Some(env_id) => DatabaseScope::Environment { org_id, env_id },
                None => DatabaseScope::Organization { org_id },
            };
            scope.apply_to_connection(&mut tx).await.map_err(fwe)?;
            Ok(StorageWriteTransaction::Postgres(tx))
        }
        DatabasePool::Sqlite(pool) => {
            let mut tx = pool.begin().await.map_err(fwe)?;
            let read_limits = async |tx: &mut sqlx::Transaction<'static, sqlx::Sqlite>| {
                let overrides: Option<String> =
                    sqlx::query_scalar("SELECT features FROM org WHERE org_id=?")
                        .bind(root_id)
                        .fetch_one(&mut **tx)
                        .await
                        .map_err(fwe)?;
                let plan: Option<String> = sqlx::query_scalar(
                    "SELECT COALESCE(p.features,'{}') FROM org_plan op JOIN plan p ON p.plan_uuid=op.plan_uuid WHERE op.org_id=?"
                ).bind(root_id).fetch_optional(&mut **tx).await.map_err(fwe)?.flatten();
                let parse = |raw: Option<String>| {
                    raw.map(|s| serde_json::from_str::<serde_json::Value>(&s))
                        .transpose()
                        .map_err(fwe)
                };
                Ok::<_, FileWriteError>((parse(plan)?, parse(overrides)?))
            };
            let (plan, overrides) = read_limits(&mut tx).await?;
            if storage_limit(plan.as_ref(), overrides.as_ref()) >= 0 {
                // Take the write lock, then read the limits again under it.
                sqlx::query("UPDATE org SET updated_at=updated_at WHERE org_id=?")
                    .bind(root_id)
                    .execute(&mut *tx)
                    .await
                    .map_err(fwe)?;
                let (plan, overrides) = read_limits(&mut tx).await?;
                let usage: i64 = sqlx::query_scalar(
                    "SELECT COALESCE(SUM(f.size),0) FROM file f JOIN org o ON o.org_id=f.org_id WHERE f.active=1 AND (o.org_id=? OR o.parent_org_id=?) AND (? IS NULL OR f.file_id<>?)"
                ).bind(root_id).bind(root_id).bind(file_id).bind(file_id).fetch_one(&mut *tx).await.map_err(fwe)?;
                let replaced: Option<i64> = match file_id {
                    Some(file_id) => {
                        sqlx::query_scalar("SELECT size FROM file WHERE file_id=? AND active=1")
                            .bind(file_id)
                            .fetch_optional(&mut *tx)
                            .await
                            .map_err(fwe)?
                    }
                    None => None,
                };
                validate_storage_limit(
                    plan.as_ref(),
                    overrides.as_ref(),
                    usage,
                    new_size,
                    replaced,
                )?;
            }
            Ok(StorageWriteTransaction::Sqlite(tx))
        }
    }
}

/// Prefix of the error a write gets when it would exceed the account's file
/// storage limit. API handlers match it to answer 413 rather than 500.
pub const STORAGE_QUOTA_EXCEEDED: &str = "Storage quota exceeded";

/// The account's file storage limit in bytes; negative means unlimited.
fn storage_limit(plan: Option<&serde_json::Value>, overrides: Option<&serde_json::Value>) -> i64 {
    super::Features::for_account(plan, overrides).storage_bytes()
}

/// `usage` excludes the file a write replaces, whose size is `replaced`. A
/// replacement that doesn't grow the file always passes, so an account over
/// its limit (after the limit was lowered) can still rewrite or shrink files.
fn validate_storage_limit(
    plan: Option<&serde_json::Value>,
    overrides: Option<&serde_json::Value>,
    usage: i64,
    size: i64,
    replaced: Option<i64>,
) -> Result<(), FileWriteError> {
    if replaced.is_some_and(|replaced| size <= replaced) {
        return Ok(());
    }
    let limit = storage_limit(plan, overrides);
    if limit >= 0 && usage.checked_add(size).is_none_or(|total| total > limit) {
        return Err(fwe(format!(
            "{STORAGE_QUOTA_EXCEEDED}: account usage {} + file {} > limit {}",
            usage, size, limit
        )));
    }
    Ok(())
}

/// Insert/update error that distinguishes foreign-key violations so callers
/// can degrade gracefully (e.g. drop run provenance) instead of failing the
/// user's file write.
enum FileWriteError {
    ForeignKey(String),
    Other(String),
}

impl FileWriteError {
    fn from_sqlx(context: &str, e: sqlx::Error) -> Self {
        let is_fk = e
            .as_database_error()
            .is_some_and(|d| d.is_foreign_key_violation());
        let msg = format!("{}: {}", context, e);
        if is_fk {
            FileWriteError::ForeignKey(msg)
        } else {
            FileWriteError::Other(msg)
        }
    }

    fn into_message(self) -> String {
        match self {
            FileWriteError::ForeignKey(msg) | FileWriteError::Other(msg) => msg,
        }
    }
}

/// Insert a new file record into the database.
///
/// The `created_by_run_id` column has a foreign key to `run`, but the run row
/// is written asynchronously by the event emitter (and never written at all
/// when the emitter is disabled). File writes are user data and must not
/// depend on observability timing, so a foreign-key violation is retried once
/// with a NULL run id rather than failing the write.
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
    let attempt = insert_file_record_attempt(
        db,
        path,
        size,
        etag,
        content_type,
        storage_backend,
        storage_path,
        org_id,
        env_id,
        created_by_user_id,
        created_by_run_id,
    )
    .await;

    match attempt {
        Ok(record) => Ok(record),
        Err(FileWriteError::ForeignKey(msg)) if created_by_run_id.is_some() => {
            tracing::warn!(
                "File record insert for '{}' hit a foreign-key violation ({}); retrying with \
                 created_by_run_id = NULL because run {} is not persisted yet (async emitter \
                 backlog) or run tracking is disabled",
                path,
                msg,
                created_by_run_id.unwrap_or_default(),
            );
            insert_file_record_attempt(
                db,
                path,
                size,
                etag,
                content_type,
                storage_backend,
                storage_path,
                org_id,
                env_id,
                created_by_user_id,
                None,
            )
            .await
            .map_err(FileWriteError::into_message)
        }
        Err(e) => Err(e.into_message()),
    }
}

#[allow(clippy::too_many_arguments)]
async fn insert_file_record_attempt(
    db: &DatabasePool,
    path: &str,
    size: i64,
    etag: Option<&str>,
    content_type: Option<&str>,
    storage_backend: &str,
    storage_path: &str,
    org_id: Uuid,
    env_id: Option<Uuid>,
    created_by_user_id: Uuid,
    created_by_run_id: Option<Uuid>,
) -> Result<FileRecord, FileWriteError> {
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

    let transaction = begin_storage_write(db, org_id, env_id, None, size).await?;
    match transaction {
        StorageWriteTransaction::Postgres(mut tx) => {
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
                .fetch_one(&mut *tx)
                .await
                .map_err(|e| FileWriteError::from_sqlx("Failed to insert file record", e))?;

            tx.commit().await.map_err(fwe)?;
            Ok(FileRecord {
                file_id: row.try_get("file_id").map_err(fwe)?,
                path: row.try_get("path").map_err(fwe)?,
                size: row.try_get("size").map_err(fwe)?,
                etag: row.try_get("etag").map_err(fwe)?,
                content_type: row.try_get("content_type").map_err(fwe)?,
                storage_backend: row.try_get("storage_backend").map_err(fwe)?,
                storage_path: row.try_get("storage_path").map_err(fwe)?,
                org_id: row.try_get("org_id").map_err(fwe)?,
                env_id: row.try_get("env_id").map_err(fwe)?, // Added env_id
                created_by_run_id: row.try_get("created_by_run_id").map_err(fwe)?,
                updated_by_run_id: row.try_get("updated_by_run_id").map_err(fwe)?,
                created_at: row.try_get("created_at").map_err(fwe)?,
                updated_at: row.try_get("updated_at").map_err(fwe)?,
                created_by_user_id: row.try_get("created_by_user_id").map_err(fwe)?,
                updated_by_user_id: row.try_get("updated_by_user_id").map_err(fwe)?,
            })
        }
        StorageWriteTransaction::Sqlite(mut tx) => {
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
                .execute(&mut *tx)
                .await
                .map_err(|e| FileWriteError::from_sqlx("Failed to insert file record", e))?;

            tx.commit().await.map_err(fwe)?;
            // Fetch the inserted record - now requires env_id for security
            get_file_by_path(db, path, org_id, env_id)
                .await
                .map_err(FileWriteError::Other)
        }
    }
}

/// Map a non-FK error into `FileWriteError::Other` (row decoding, etc).
fn fwe<E: std::fmt::Display>(e: E) -> FileWriteError {
    FileWriteError::Other(e.to_string())
}

/// Update an existing file record.
///
/// Like [`insert_file_record`], `updated_by_run_id` references a run row that
/// may not be persisted yet (async emitter) or ever (emitter disabled), so a
/// foreign-key violation is retried once with a NULL run id.
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
    let attempt = update_file_record_attempt(
        db,
        file_id,
        size,
        etag,
        content_type,
        storage_path,
        updated_by_user_id,
        updated_by_run_id,
    )
    .await;

    match attempt {
        Ok(record) => Ok(record),
        Err(FileWriteError::ForeignKey(msg)) if updated_by_run_id.is_some() => {
            tracing::warn!(
                "File record update for '{}' hit a foreign-key violation ({}); retrying with \
                 updated_by_run_id = NULL because run {} is not persisted yet (async emitter \
                 backlog) or run tracking is disabled",
                file_id,
                msg,
                updated_by_run_id.unwrap_or_default(),
            );
            update_file_record_attempt(
                db,
                file_id,
                size,
                etag,
                content_type,
                storage_path,
                updated_by_user_id,
                None,
            )
            .await
            .map_err(FileWriteError::into_message)
        }
        Err(e) => Err(e.into_message()),
    }
}

#[allow(clippy::too_many_arguments)]
async fn update_file_record_attempt(
    db: &DatabasePool,
    file_id: Uuid,
    size: i64,
    etag: Option<&str>,
    content_type: Option<&str>,
    storage_path: &str,
    updated_by_user_id: Uuid,
    updated_by_run_id: Option<Uuid>,
) -> Result<FileRecord, FileWriteError> {
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

    let existing = get_file_by_id(db, file_id).await.map_err(fwe)?;
    let transaction =
        begin_storage_write(db, existing.org_id, existing.env_id, Some(file_id), size).await?;
    match transaction {
        StorageWriteTransaction::Postgres(mut tx) => {
            let row = sqlx::query(query)
                .bind(file_id)
                .bind(size)
                .bind(etag)
                .bind(content_type)
                .bind(storage_path)
                .bind(updated_by_user_id)
                .bind(updated_by_run_id)
                .bind(now)
                .fetch_one(&mut *tx)
                .await
                .map_err(|e| FileWriteError::from_sqlx("Failed to update file record", e))?;

            tx.commit().await.map_err(fwe)?;
            Ok(FileRecord {
                file_id: row.try_get("file_id").map_err(fwe)?,
                path: row.try_get("path").map_err(fwe)?,
                size: row.try_get("size").map_err(fwe)?,
                etag: row.try_get("etag").map_err(fwe)?,
                content_type: row.try_get("content_type").map_err(fwe)?,
                storage_backend: row.try_get("storage_backend").map_err(fwe)?,
                storage_path: row.try_get("storage_path").map_err(fwe)?,
                org_id: row.try_get("org_id").map_err(fwe)?,
                env_id: row.try_get("env_id").map_err(fwe)?,
                created_by_run_id: row.try_get("created_by_run_id").map_err(fwe)?,
                updated_by_run_id: row.try_get("updated_by_run_id").map_err(fwe)?,
                created_at: row.try_get("created_at").map_err(fwe)?,
                updated_at: row.try_get("updated_at").map_err(fwe)?,
                created_by_user_id: row.try_get("created_by_user_id").map_err(fwe)?,
                updated_by_user_id: row.try_get("updated_by_user_id").map_err(fwe)?,
            })
        }
        StorageWriteTransaction::Sqlite(mut tx) => {
            sqlx::query(query)
                .bind(size)
                .bind(etag)
                .bind(content_type)
                .bind(storage_path)
                .bind(updated_by_user_id)
                .bind(updated_by_run_id)
                .bind(now)
                .bind(file_id)
                .execute(&mut *tx)
                .await
                .map_err(|e| FileWriteError::from_sqlx("Failed to update file record", e))?;

            tx.commit().await.map_err(fwe)?;
            // Fetch the updated record by file_id
            get_file_by_id(db, file_id)
                .await
                .map_err(FileWriteError::Other)
        }
    }
}

/// Conditionally update a file record: succeeds only while the record is
/// still active and its stored etag equals `expected_etag`. Returns Ok(None)
/// when the record changed (or was deleted) since it was read — the
/// compare-and-swap lost and the caller must not treat its bytes as current.
///
/// Legacy records with a NULL etag carry no version information, so any
/// expectation matches them; the row-level atomicity of the UPDATE still
/// guarantees exactly one concurrent writer wins, and the winner stamps a
/// real etag so later swaps compare strictly.
///
/// Like `update_file_record`, a foreign-key violation on `updated_by_run_id`
/// is retried once with a NULL run id (the run row may not be flushed yet).
#[allow(clippy::too_many_arguments)]
pub async fn update_file_record_if_etag(
    db: &DatabasePool,
    file_id: Uuid,
    expected_etag: &str,
    size: i64,
    etag: Option<&str>,
    content_type: Option<&str>,
    storage_path: &str,
    updated_by_user_id: Uuid,
    updated_by_run_id: Option<Uuid>,
) -> Result<Option<FileRecord>, String> {
    update_file_record_if(
        db,
        file_id,
        RecordGuard::Etag(expected_etag),
        size,
        etag,
        content_type,
        storage_path,
        updated_by_user_id,
        updated_by_run_id,
    )
    .await
}

/// Conditionally update a file record: succeeds only while the record is
/// still active and still references `expected_storage_path`. Every write
/// stages its bytes at a unique location, so the location identifies the
/// version exactly; Ok(None) means another writer (or a delete) got there
/// first and the caller must re-read before trying again.
#[allow(clippy::too_many_arguments)]
pub async fn update_file_record_if_storage_path(
    db: &DatabasePool,
    file_id: Uuid,
    expected_storage_path: &str,
    size: i64,
    etag: Option<&str>,
    content_type: Option<&str>,
    storage_path: &str,
    updated_by_user_id: Uuid,
    updated_by_run_id: Option<Uuid>,
) -> Result<Option<FileRecord>, String> {
    update_file_record_if(
        db,
        file_id,
        RecordGuard::StoragePath(expected_storage_path),
        size,
        etag,
        content_type,
        storage_path,
        updated_by_user_id,
        updated_by_run_id,
    )
    .await
}

/// The version a conditional update requires the record to still have.
#[derive(Clone, Copy)]
enum RecordGuard<'a> {
    /// Application etag; legacy NULL etags match any expectation.
    Etag(&'a str),
    /// Exact stored location.
    StoragePath(&'a str),
}

#[allow(clippy::too_many_arguments)]
async fn update_file_record_if(
    db: &DatabasePool,
    file_id: Uuid,
    guard: RecordGuard<'_>,
    size: i64,
    etag: Option<&str>,
    content_type: Option<&str>,
    storage_path: &str,
    updated_by_user_id: Uuid,
    updated_by_run_id: Option<Uuid>,
) -> Result<Option<FileRecord>, String> {
    let attempt = update_file_record_if_attempt(
        db,
        file_id,
        guard,
        size,
        etag,
        content_type,
        storage_path,
        updated_by_user_id,
        updated_by_run_id,
    )
    .await;

    match attempt {
        Ok(record) => Ok(record),
        Err(FileWriteError::ForeignKey(msg)) if updated_by_run_id.is_some() => {
            tracing::warn!(
                "Conditional file record update for '{}' hit a foreign-key violation ({}); \
                 retrying with updated_by_run_id = NULL because run {} is not persisted yet \
                 (async emitter backlog) or run tracking is disabled",
                file_id,
                msg,
                updated_by_run_id.unwrap_or_default(),
            );
            update_file_record_if_attempt(
                db,
                file_id,
                guard,
                size,
                etag,
                content_type,
                storage_path,
                updated_by_user_id,
                None,
            )
            .await
            .map_err(FileWriteError::into_message)
        }
        Err(e) => Err(e.into_message()),
    }
}

#[allow(clippy::too_many_arguments)]
async fn update_file_record_if_attempt(
    db: &DatabasePool,
    file_id: Uuid,
    guard: RecordGuard<'_>,
    size: i64,
    etag: Option<&str>,
    content_type: Option<&str>,
    storage_path: &str,
    updated_by_user_id: Uuid,
    updated_by_run_id: Option<Uuid>,
) -> Result<Option<FileRecord>, FileWriteError> {
    let now = Utc::now();
    // A missing, inactive, or no-longer-visible row is a lost comparison. Keep
    // database failures distinct, and check the guard before quota work.
    let existing: Option<(Uuid, Option<Uuid>, Option<String>, String)> = match db {
        DatabasePool::Postgres(pool) => sqlx::query_as(
            "SELECT org_id,env_id,etag,storage_path FROM file WHERE file_id=$1 AND active=true",
        )
        .bind(file_id)
        .fetch_optional(pool)
        .await
        .map_err(fwe)?,
        DatabasePool::Sqlite(pool) => sqlx::query_as(
            "SELECT org_id,env_id,etag,storage_path FROM file WHERE file_id=? AND active=1",
        )
        .bind(file_id)
        .fetch_optional(pool)
        .await
        .map_err(fwe)?,
    };
    let Some((org_id, env_id, current_etag, current_path)) = existing else {
        return Ok(None);
    };
    let (expected, matches) = match guard {
        RecordGuard::Etag(expected) => (
            expected,
            current_etag.as_deref().is_none_or(|tag| tag == expected),
        ),
        RecordGuard::StoragePath(expected) => (expected, current_path == expected),
    };
    if !matches {
        return Ok(None);
    }
    let transaction = begin_storage_write(db, org_id, env_id, Some(file_id), size).await?;
    match transaction {
        StorageWriteTransaction::Postgres(mut tx) => {
            let query = match guard {
                RecordGuard::Etag(_) => {
                    r#"
            UPDATE file SET
                size = $2,
                etag = $3,
                content_type = $4,
                storage_path = $5,
                updated_by_user_id = $6,
                updated_by_run_id = $7,
                updated_at = $8
            WHERE file_id = $1 AND active = true AND (etag = $9 OR etag IS NULL)
            RETURNING file_id, path, size, etag, content_type,
                      storage_backend, storage_path, org_id, env_id,
                      created_by_run_id, updated_by_run_id,
                      created_at, updated_at, created_by_user_id, updated_by_user_id
            "#
                }
                RecordGuard::StoragePath(_) => {
                    r#"
            UPDATE file SET
                size = $2,
                etag = $3,
                content_type = $4,
                storage_path = $5,
                updated_by_user_id = $6,
                updated_by_run_id = $7,
                updated_at = $8
            WHERE file_id = $1 AND active = true AND storage_path = $9
            RETURNING file_id, path, size, etag, content_type,
                      storage_backend, storage_path, org_id, env_id,
                      created_by_run_id, updated_by_run_id,
                      created_at, updated_at, created_by_user_id, updated_by_user_id
            "#
                }
            };
            let row = sqlx::query(query)
                .bind(file_id)
                .bind(size)
                .bind(etag)
                .bind(content_type)
                .bind(storage_path)
                .bind(updated_by_user_id)
                .bind(updated_by_run_id)
                .bind(now)
                .bind(expected)
                .fetch_optional(&mut *tx)
                .await
                .map_err(|e| {
                    FileWriteError::from_sqlx("Failed to conditionally update file record", e)
                })?;
            tx.commit().await.map_err(fwe)?;
            let Some(row) = row else {
                return Ok(None);
            };
            Ok(Some(FileRecord {
                file_id: row.try_get("file_id").map_err(fwe)?,
                path: row.try_get("path").map_err(fwe)?,
                size: row.try_get("size").map_err(fwe)?,
                etag: row.try_get("etag").map_err(fwe)?,
                content_type: row.try_get("content_type").map_err(fwe)?,
                storage_backend: row.try_get("storage_backend").map_err(fwe)?,
                storage_path: row.try_get("storage_path").map_err(fwe)?,
                org_id: row.try_get("org_id").map_err(fwe)?,
                env_id: row.try_get("env_id").map_err(fwe)?,
                created_by_run_id: row.try_get("created_by_run_id").map_err(fwe)?,
                updated_by_run_id: row.try_get("updated_by_run_id").map_err(fwe)?,
                created_at: row.try_get("created_at").map_err(fwe)?,
                updated_at: row.try_get("updated_at").map_err(fwe)?,
                created_by_user_id: row.try_get("created_by_user_id").map_err(fwe)?,
                updated_by_user_id: row.try_get("updated_by_user_id").map_err(fwe)?,
            }))
        }
        StorageWriteTransaction::Sqlite(mut tx) => {
            let query = match guard {
                RecordGuard::Etag(_) => {
                    r#"
            UPDATE file SET
                size = ?,
                etag = ?,
                content_type = ?,
                storage_path = ?,
                updated_by_user_id = ?,
                updated_by_run_id = ?,
                updated_at = ?
            WHERE file_id = ? AND active = 1 AND (etag = ? OR etag IS NULL)
            "#
                }
                RecordGuard::StoragePath(_) => {
                    r#"
            UPDATE file SET
                size = ?,
                etag = ?,
                content_type = ?,
                storage_path = ?,
                updated_by_user_id = ?,
                updated_by_run_id = ?,
                updated_at = ?
            WHERE file_id = ? AND active = 1 AND storage_path = ?
            "#
                }
            };
            let result = sqlx::query(query)
                .bind(size)
                .bind(etag)
                .bind(content_type)
                .bind(storage_path)
                .bind(updated_by_user_id)
                .bind(updated_by_run_id)
                .bind(now)
                .bind(file_id)
                .bind(expected)
                .execute(&mut *tx)
                .await
                .map_err(|e| {
                    FileWriteError::from_sqlx("Failed to conditionally update file record", e)
                })?;
            tx.commit().await.map_err(fwe)?;
            if result.rows_affected() == 0 {
                return Ok(None);
            }
            get_file_by_id(db, file_id)
                .await
                .map(Some)
                .map_err(FileWriteError::Other)
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
    find_file_by_path(db, path, org_id, env_id)
        .await?
        .ok_or_else(|| format!("File not found: {}", sqlx::Error::RowNotFound))
}

/// Like [`get_file_by_path`], but distinguishes "no active record" (`None`)
/// from a failed query, for callers whose next step depends on which.
pub async fn find_file_by_path(
    db: &DatabasePool,
    path: &str,
    org_id: Uuid,
    env_id: Option<Uuid>,
) -> Result<Option<FileRecord>, String> {
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
                .fetch_optional(pool)
                .await
                .map_err(|e| format!("File not found: {}", e))?;
            let Some(row) = row else {
                return Ok(None);
            };

            Ok(Some(FileRecord {
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
            }))
        }
        DatabasePool::Sqlite(pool) => {
            let row = sqlx::query(query)
                .bind(path)
                .bind(org_id)
                .bind(env_id) // Added env_id for security
                .bind(env_id) // Need to bind twice for IS NULL check
                .fetch_optional(pool)
                .await
                .map_err(|e| format!("File not found: {}", e))?;
            let Some(row) = row else {
                return Ok(None);
            };

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

            Ok(Some(FileRecord {
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
            }))
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

/// Mark one version of a file inactive (soft delete).
///
/// A compare-and-swap on `expected_storage_path`, like the write path's
/// record swap: it deactivates the record only while it still names the
/// version the caller read. Returns false when a concurrent overwrite (or
/// delete) moved the record first; the caller re-reads, so it never
/// deactivates bytes it won't delete.
pub async fn mark_file_inactive(
    db: &DatabasePool,
    file_id: Uuid,
    expected_storage_path: &str,
    updated_by_user_id: Uuid,
    updated_by_run_id: Option<Uuid>,
) -> Result<bool, String> {
    let now = Utc::now();

    let rows_affected = match db {
        DatabasePool::Postgres(pool) => sqlx::query(
            r#"
            UPDATE file SET
                active = false,
                active_toggle_at = $4,
                active_toggle_by_user_id = $3,
                updated_by_run_id = $5,
                updated_at = $4
            WHERE file_id = $1
              AND storage_path = $2
              AND active = true
            "#,
        )
        .bind(file_id)
        .bind(expected_storage_path)
        .bind(updated_by_user_id)
        .bind(now)
        .bind(updated_by_run_id)
        .execute(pool)
        .await
        .map_err(|e| format!("Failed to mark file inactive: {}", e))?
        .rows_affected(),
        DatabasePool::Sqlite(pool) => sqlx::query(
            r#"
            UPDATE file SET
                active = 0,
                active_toggle_at = ?,
                active_toggle_by_user_id = ?,
                updated_by_run_id = ?,
                updated_at = ?
            WHERE file_id = ?
              AND storage_path = ?
              AND active = 1
            "#,
        )
        .bind(now)
        .bind(updated_by_user_id)
        .bind(updated_by_run_id)
        .bind(now)
        .bind(file_id)
        .bind(expected_storage_path)
        .execute(pool)
        .await
        .map_err(|e| format!("Failed to mark file inactive: {}", e))?
        .rows_affected(),
    };

    Ok(rows_affected == 1)
}

/// List files by prefix
pub async fn list_files_by_prefix(
    db: &DatabasePool,
    prefix: &str,
    org_id: Uuid,
    env_id: Option<Uuid>,
) -> Result<Vec<FileRecord>, String> {
    list_files_by_prefix_limit(db, prefix, org_id, env_id, i64::MAX).await
}

/// List files by prefix with a database-enforced result bound.
pub async fn list_files_by_prefix_bounded(
    db: &DatabasePool,
    prefix: &str,
    org_id: Uuid,
    env_id: Option<Uuid>,
    limit: usize,
) -> Result<Vec<FileRecord>, String> {
    let limit = i64::try_from(limit).unwrap_or(i64::MAX);
    list_files_by_prefix_limit(db, prefix, org_id, env_id, limit).await
}

async fn list_files_by_prefix_limit(
    db: &DatabasePool,
    prefix: &str,
    org_id: Uuid,
    env_id: Option<Uuid>,
    limit: i64,
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
            LIMIT $4
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
            LIMIT ?
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
                .bind(limit)
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
                .bind(limit)
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
            let mut q = sqlx::query(sqlx::AssertSqlSafe(query.as_str())).bind(env_id);

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

            let mut q = sqlx::query(sqlx::AssertSqlSafe(sqlite_query.as_str())).bind(env_id);

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

            let mut q =
                sqlx::query_scalar::<_, i64>(sqlx::AssertSqlSafe(query.as_str())).bind(env_id);

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

            let mut q =
                sqlx::query_scalar::<_, i64>(sqlx::AssertSqlSafe(query.as_str())).bind(env_id);

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

#[cfg(test)]
pub(crate) async fn seed_storage_quota_test_org(db: &DatabasePool, org_id: Uuid) {
    let DatabasePool::Sqlite(pool) = db else {
        panic!("SQLite fixture")
    };
    sqlx::raw_sql("CREATE TABLE IF NOT EXISTS org (
        org_id blob PRIMARY KEY, parent_org_id blob, external_id text, lifecycle_state text NOT NULL DEFAULT 'active',
        name text NOT NULL, slug text NOT NULL, org_type text NOT NULL DEFAULT 'organization', settings text, features text,
        active integer NOT NULL DEFAULT 1, created_at datetime NOT NULL DEFAULT current_timestamp, created_by_user_id blob NOT NULL,
        updated_at datetime NOT NULL DEFAULT current_timestamp, updated_by_user_id blob, active_toggle_at datetime, active_toggle_by_user_id blob);
        CREATE TABLE IF NOT EXISTS plan (plan_uuid blob PRIMARY KEY, features text);
        CREATE TABLE IF NOT EXISTS org_plan (org_id blob PRIMARY KEY, plan_uuid blob);")
        .execute(pool).await.unwrap();
    sqlx::query(
        "INSERT INTO org (org_id,name,slug,created_by_user_id) VALUES (?,'Storage test',?,?)",
    )
    .bind(org_id)
    .bind(org_id.to_string())
    .bind(org_id)
    .execute(pool)
    .await
    .unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replacements_that_dont_grow_pass_an_exceeded_limit() {
        let plan = serde_json::json!({"storage_bytes": 100});
        let check = |usage, size, replaced| {
            validate_storage_limit(Some(&plan), None, usage, size, replaced).is_ok()
        };
        // Usage (excluding the replaced file) is already over the limit.
        assert!(check(150, 10, Some(10)));
        assert!(check(150, 10, Some(20)));
        assert!(!check(150, 11, Some(10)));
        assert!(check(50, 50, None));
        assert!(!check(50, 51, None));
    }

    /// SQLite DB with FK enforcement on and a `run` table, mirroring the
    /// production race: the file insert carries a run id whose run row has
    /// not been persisted yet by the async emitter.
    async fn setup_fk_db() -> DatabasePool {
        use sqlx::sqlite::SqlitePoolOptions;

        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();

        sqlx::query("PRAGMA foreign_keys = ON")
            .execute(&pool)
            .await
            .unwrap();

        sqlx::query("CREATE TABLE run (run_id blob PRIMARY KEY)")
            .execute(&pool)
            .await
            .unwrap();

        sqlx::query(
            r#"CREATE TABLE file (
                file_id blob PRIMARY KEY,
                path text NOT NULL,
                size integer NOT NULL,
                etag text,
                content_type text,
                storage_backend text NOT NULL,
                storage_path text,
                org_id blob NOT NULL,
                env_id blob,
                created_by_run_id blob REFERENCES run(run_id),
                updated_by_run_id blob REFERENCES run(run_id),
                active integer DEFAULT 1,
                created_at datetime NOT NULL DEFAULT current_timestamp,
                created_by_user_id blob NOT NULL,
                updated_at datetime NOT NULL DEFAULT current_timestamp,
                updated_by_user_id blob,
                active_toggle_at datetime,
                active_toggle_by_user_id blob
            )"#,
        )
        .execute(&pool)
        .await
        .unwrap();

        DatabasePool::Sqlite(pool)
    }

    #[tokio::test]
    async fn siblings_share_atomic_storage_capacity_and_replacements_use_delta() {
        let db = setup_fk_db().await;
        let root = Uuid::now_v7();
        let first = Uuid::now_v7();
        let second = Uuid::now_v7();
        for id in [root, first, second] {
            seed_storage_quota_test_org(&db, id).await;
        }
        let DatabasePool::Sqlite(pool) = &db else {
            unreachable!()
        };
        sqlx::query("UPDATE org SET parent_org_id=? WHERE org_id IN (?,?)")
            .bind(root)
            .bind(first)
            .bind(second)
            .execute(pool)
            .await
            .unwrap();
        sqlx::query("UPDATE org SET features=? WHERE org_id=?")
            .bind(r#"{"storage_bytes":10}"#)
            .bind(root)
            .execute(pool)
            .await
            .unwrap();
        let a = insert_file_record(
            &db, "a", 6, None, None, "local", "a", first, None, root, None,
        );
        let b = insert_file_record(
            &db, "b", 6, None, None, "local", "b", second, None, root, None,
        );
        let (a, b) = tokio::join!(a, b);
        assert_eq!(usize::from(a.is_ok()) + usize::from(b.is_ok()), 1);
        let record = a.ok().or_else(|| b.ok()).unwrap();
        let replacement = update_file_record(&db, record.file_id, 7, None, None, "new", root, None)
            .await
            .unwrap();
        assert_eq!(replacement.size, 7);
        let other = if record.org_id == first {
            second
        } else {
            first
        };
        insert_file_record(
            &db, "other", 3, None, None, "local", "other", other, None, root, None,
        )
        .await
        .unwrap();
        assert!(
            update_file_record(&db, record.file_id, 8, None, None, "rejected", root, None)
                .await
                .is_err()
        );
        let current = get_file_by_id(&db, record.file_id).await.unwrap();
        assert_eq!((current.size, current.storage_path.as_str()), (7, "new"));
    }

    #[tokio::test]
    async fn insert_falls_back_to_null_run_id_when_run_row_is_missing() {
        let db = setup_fk_db().await;
        let org_id = Uuid::now_v7();
        seed_storage_quota_test_org(&db, org_id).await;
        let env_id = Uuid::now_v7();
        let user_id = Uuid::now_v7();
        let missing_run_id = Uuid::now_v7();

        let record = insert_file_record(
            &db,
            "hot-live/windows/a/1.webm",
            42,
            Some("etag"),
            Some("video/webm"),
            "local",
            "/tmp/storage/a/1.webm",
            org_id,
            Some(env_id),
            user_id,
            Some(missing_run_id),
        )
        .await
        .expect("insert should survive a missing run row by dropping run provenance");

        assert_eq!(record.path, "hot-live/windows/a/1.webm");
        assert_eq!(record.created_by_run_id, None);
    }

    #[tokio::test]
    async fn insert_keeps_run_id_when_run_row_exists() {
        let db = setup_fk_db().await;
        let org_id = Uuid::now_v7();
        seed_storage_quota_test_org(&db, org_id).await;
        let user_id = Uuid::now_v7();
        let run_id = Uuid::now_v7();

        if let DatabasePool::Sqlite(pool) = &db {
            sqlx::query("INSERT INTO run (run_id) VALUES (?)")
                .bind(run_id)
                .execute(pool)
                .await
                .unwrap();
        }

        let record = insert_file_record(
            &db,
            "hot-live/windows/a/2.webm",
            42,
            None,
            None,
            "local",
            "/tmp/storage/a/2.webm",
            org_id,
            None,
            user_id,
            Some(run_id),
        )
        .await
        .unwrap();

        assert_eq!(record.created_by_run_id, Some(run_id));
    }

    #[tokio::test]
    async fn conditional_update_wins_once_then_conflicts() {
        let db = setup_fk_db().await;
        let org_id = Uuid::now_v7();
        seed_storage_quota_test_org(&db, org_id).await;
        let user_id = Uuid::now_v7();

        let inserted = insert_file_record(
            &db,
            "dbs/app.db",
            10,
            Some("etag-v1"),
            None,
            "local",
            "/tmp/storage/dbs/app.db",
            org_id,
            None,
            user_id,
            None,
        )
        .await
        .unwrap();

        // First CAS with the current etag wins.
        let won = update_file_record_if_etag(
            &db,
            inserted.file_id,
            "etag-v1",
            20,
            Some("etag-v2"),
            None,
            "/tmp/storage/dbs/app.db",
            user_id,
            None,
        )
        .await
        .unwrap();
        assert_eq!(
            won.as_ref().and_then(|r| r.etag.clone()),
            Some("etag-v2".to_string())
        );

        // A second writer still holding the old etag loses cleanly.
        let lost = update_file_record_if_etag(
            &db,
            inserted.file_id,
            "etag-v1",
            30,
            Some("etag-v3"),
            None,
            "/tmp/storage/dbs/app.db",
            user_id,
            None,
        )
        .await
        .unwrap();
        assert!(lost.is_none(), "stale etag must not win the CAS");

        // The record still reflects the winner.
        let current = get_file_by_id(&db, inserted.file_id).await.unwrap();
        assert_eq!(current.etag.as_deref(), Some("etag-v2"));
        assert_eq!(current.size, 20);
    }

    #[tokio::test]
    async fn update_falls_back_to_null_run_id_when_run_row_is_missing() {
        let db = setup_fk_db().await;
        let org_id = Uuid::now_v7();
        seed_storage_quota_test_org(&db, org_id).await;
        let user_id = Uuid::now_v7();

        let inserted = insert_file_record(
            &db,
            "hot-live/windows/a/3.webm",
            42,
            None,
            None,
            "local",
            "/tmp/storage/a/3.webm",
            org_id,
            None,
            user_id,
            None,
        )
        .await
        .unwrap();

        let updated = update_file_record(
            &db,
            inserted.file_id,
            100,
            Some("etag2"),
            None,
            "/tmp/storage/a/3-v2.webm",
            user_id,
            Some(Uuid::now_v7()),
        )
        .await
        .expect("update should survive a missing run row by dropping run provenance");

        assert_eq!(updated.size, 100);
        assert_eq!(updated.updated_by_run_id, None);
    }

    #[tokio::test]
    async fn non_fk_errors_still_fail() {
        let db = setup_fk_db().await;
        let org_id = Uuid::now_v7();
        seed_storage_quota_test_org(&db, org_id).await;
        let user_id = Uuid::now_v7();
        let file_id = Uuid::now_v7();

        // Duplicate primary key is a constraint error but NOT an FK
        // violation; it must not be retried/absorbed.
        if let DatabasePool::Sqlite(pool) = &db {
            for attempt in 0..2 {
                let result = sqlx::query(
                    "INSERT INTO file (file_id, path, size, storage_backend, org_id, created_by_user_id)
                     VALUES (?, ?, ?, ?, ?, ?)",
                )
                .bind(file_id)
                .bind("dup/path.bin")
                .bind(1_i64)
                .bind("local")
                .bind(org_id)
                .bind(user_id)
                .execute(pool)
                .await;
                if attempt == 0 {
                    result.unwrap();
                } else {
                    let err = FileWriteError::from_sqlx(
                        "Failed to insert file record",
                        result.unwrap_err(),
                    );
                    assert!(matches!(err, FileWriteError::Other(_)));
                }
            }
        }
    }

    #[tokio::test]
    async fn conditional_update_matches_null_etag_exactly_once() {
        let db = setup_fk_db().await;
        let org_id = Uuid::now_v7();
        seed_storage_quota_test_org(&db, org_id).await;
        let user_id = Uuid::now_v7();

        // Multipart uploads and legacy rows store a NULL etag; a CAS whose
        // expectation is a checkout-computed hash must still be able to win.
        let inserted = insert_file_record(
            &db,
            "dbs/legacy.db",
            10,
            None,
            None,
            "local",
            "/tmp/storage/dbs/legacy.db",
            org_id,
            None,
            user_id,
            None,
        )
        .await
        .unwrap();
        assert_eq!(inserted.etag, None);

        let won = update_file_record_if_etag(
            &db,
            inserted.file_id,
            "any-checkout-hash",
            20,
            Some("etag-v1"),
            None,
            "/tmp/storage/dbs/legacy-v1.db",
            user_id,
            None,
        )
        .await
        .unwrap();
        assert!(won.is_some(), "NULL-etag record must accept the first CAS");

        // The winner stamped a real etag; a second writer with the same
        // stale expectation now loses strictly.
        let lost = update_file_record_if_etag(
            &db,
            inserted.file_id,
            "any-checkout-hash",
            30,
            Some("etag-v2"),
            None,
            "/tmp/storage/dbs/legacy-v2.db",
            user_id,
            None,
        )
        .await
        .unwrap();
        assert!(lost.is_none());
    }

    #[tokio::test]
    async fn conditional_update_loses_against_inactive_record() {
        let db = setup_fk_db().await;
        let org_id = Uuid::now_v7();
        seed_storage_quota_test_org(&db, org_id).await;
        let user_id = Uuid::now_v7();

        let inserted = insert_file_record(
            &db,
            "dbs/deleted.db",
            10,
            Some("etag-v1"),
            None,
            "local",
            "/tmp/storage/dbs/deleted.db",
            org_id,
            None,
            user_id,
            None,
        )
        .await
        .unwrap();

        // Soft-delete the record out from under a checked-out writer.
        if let DatabasePool::Sqlite(pool) = &db {
            sqlx::query("UPDATE file SET active = 0 WHERE file_id = ?")
                .bind(inserted.file_id)
                .execute(pool)
                .await
                .unwrap();
        }

        // Even with a matching etag, the CAS must not write into (or
        // resurrect) a deleted record.
        let lost = update_file_record_if_etag(
            &db,
            inserted.file_id,
            "etag-v1",
            20,
            Some("etag-v2"),
            None,
            "/tmp/storage/dbs/deleted-v2.db",
            user_id,
            None,
        )
        .await
        .unwrap();
        assert!(lost.is_none());

        // The inactive row is untouched.
        if let DatabasePool::Sqlite(pool) = &db {
            let row = sqlx::query("SELECT etag, active FROM file WHERE file_id = ?")
                .bind(inserted.file_id)
                .fetch_one(pool)
                .await
                .unwrap();
            use sqlx::Row;
            assert_eq!(
                row.get::<Option<String>, _>("etag").as_deref(),
                Some("etag-v1")
            );
            assert_eq!(row.get::<i64, _>("active"), 0);
        }
    }

    #[tokio::test]
    async fn conditional_update_falls_back_to_null_run_id_when_run_row_is_missing() {
        let db = setup_fk_db().await;
        let org_id = Uuid::now_v7();
        seed_storage_quota_test_org(&db, org_id).await;
        let user_id = Uuid::now_v7();

        let inserted = insert_file_record(
            &db,
            "dbs/fk.db",
            10,
            Some("etag-v1"),
            None,
            "local",
            "/tmp/storage/dbs/fk.db",
            org_id,
            None,
            user_id,
            None,
        )
        .await
        .unwrap();

        // The run row for this run id was never flushed (async emitter);
        // the CAS must drop the provenance instead of failing hard.
        let won = update_file_record_if_etag(
            &db,
            inserted.file_id,
            "etag-v1",
            20,
            Some("etag-v2"),
            None,
            "/tmp/storage/dbs/fk-v2.db",
            user_id,
            Some(Uuid::now_v7()),
        )
        .await
        .expect("conditional update should survive a missing run row");
        let won = won.expect("CAS with matching etag should win");
        assert_eq!(won.etag.as_deref(), Some("etag-v2"));
        assert_eq!(won.updated_by_run_id, None);
    }
}

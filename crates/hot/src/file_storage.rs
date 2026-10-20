//! File storage abstraction with database integration
//!
//! This module provides a pluggable storage backend system for Hot file operations.
//! Files can be stored locally on the filesystem, in AWS S3, or using other backends.
//! All file operations are tracked in the database for billing and audit purposes.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use indexmap::IndexMap;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use tokio::io::AsyncRead;
use uuid::Uuid;

use crate::db::DatabasePool;
use crate::db::file::{
    FileRecord, find_file_by_path, get_file_by_path, insert_file_record, mark_file_inactive,
    update_file_record_if_active, update_file_record_if_storage_path,
};
use crate::val::Val;

/// File metadata including database tracking information
#[derive(Debug, Clone)]
pub struct FileMetadata {
    pub file_id: Uuid,
    pub path: String,
    pub size: i64,
    pub etag: Option<String>,
    pub content_type: Option<String>,
    pub storage_backend: String,
    pub storage_path: String,
    pub org_id: Uuid,
    pub env_id: Option<Uuid>, // Added for environment isolation
    pub created_by_run_id: Option<Uuid>,
    pub updated_by_run_id: Option<Uuid>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub created_by_user_id: Uuid,
    pub updated_by_user_id: Option<Uuid>,
}

/// An authorized file body that can be forwarded without materializing it.
pub struct FileReadStream {
    pub metadata: FileMetadata,
    pub reader: Pin<Box<dyn AsyncRead + Send>>,
}

impl FileMetadata {
    /// Convert FileMetadata to Hot Val (Map)
    pub fn to_val(&self) -> Val {
        let mut map = IndexMap::new();
        map.insert(Val::from("file-id"), Val::from(self.file_id.to_string()));
        map.insert(Val::from("path"), Val::from(self.path.clone()));
        map.insert(Val::from("size"), Val::Int(self.size));
        if let Some(etag) = &self.etag {
            map.insert(Val::from("etag"), Val::from(etag.clone()));
        }
        if let Some(ct) = &self.content_type {
            map.insert(Val::from("content-type"), Val::from(ct.clone()));
        }
        map.insert(
            Val::from("storage-backend"),
            Val::from(self.storage_backend.clone()),
        );
        map.insert(
            Val::from("created-at"),
            Val::Int(self.created_at.timestamp()),
        );
        map.insert(
            Val::from("updated-at"),
            Val::Int(self.updated_at.timestamp()),
        );
        Val::Map(Box::new(map))
    }

    /// Create from database FileRecord
    pub fn from_file_record(record: FileRecord) -> Self {
        Self {
            file_id: record.file_id,
            path: record.path,
            size: record.size,
            etag: record.etag,
            content_type: record.content_type,
            storage_backend: record.storage_backend,
            storage_path: record.storage_path,
            org_id: record.org_id,
            env_id: record.env_id, // Added env_id
            created_by_run_id: record.created_by_run_id,
            updated_by_run_id: record.updated_by_run_id,
            created_at: record.created_at,
            updated_at: record.updated_at,
            created_by_user_id: record.created_by_user_id,
            updated_by_user_id: record.updated_by_user_id,
        }
    }
}

/// Execution context for file storage operations
#[derive(Clone)]
pub struct FileStorageContext {
    pub db: std::sync::Arc<DatabasePool>,
    pub org_id: Uuid,
    pub env_id: Option<Uuid>,
    pub user_id: Uuid,
    pub run_id: Option<Uuid>,
    /// When set alongside `run_id`, used to insert the run row synchronously
    /// before file metadata so `created_by_run_id` FK retries are unnecessary.
    pub run_provenance: Option<FileRunProvenance>,
    /// Config-level ceiling for file size (`hot.file.max-bytes`).
    /// Negative or None = no config ceiling, defer to plan limits.
    pub file_max_bytes_conf: Option<i64>,
}

/// Fields required to upsert a run row before a file metadata write.
///
/// Deliberately excludes `origin_run_id` and `event_id`: the parent run row
/// may itself be unflushed (same emitter backlog), so the placeholder insert
/// must not carry FKs that can cascade-fail. The authoritative `run:start`
/// write backfills those fields on placeholder rows.
#[derive(Clone, Debug)]
pub struct FileRunProvenance {
    pub stream_id: Uuid,
    pub build_id: Option<Uuid>,
    pub run_type_id: i16,
    pub access_id: Option<Uuid>,
}

impl FileStorageContext {
    /// Create from Hot execution context
    pub fn from_execution_context(
        db: std::sync::Arc<DatabasePool>,
        exec_ctx: &crate::lang::event::ExecutionContext,
    ) -> Result<Self, String> {
        let org_id = exec_ctx
            .org_id
            .ok_or_else(|| "No org_id in execution context".to_string())?;
        let user_id = exec_ctx
            .user_id
            .ok_or_else(|| "No user_id in execution context".to_string())?;

        let scope = exec_ctx
            .env_id
            .map(|env_id| crate::db::DatabaseScope::Environment { org_id, env_id })
            .unwrap_or(crate::db::DatabaseScope::Organization { org_id });
        let db = std::sync::Arc::new(db.with_scope(scope));
        Ok(Self {
            db,
            org_id,
            env_id: exec_ctx.env_id,
            user_id,
            run_id: Some(exec_ctx.run_id),
            run_provenance: Some(FileRunProvenance {
                stream_id: exec_ctx.stream_id,
                build_id: exec_ctx.build_id,
                run_type_id: exec_ctx.run_type_id,
                access_id: exec_ctx.access_id,
            }),
            file_max_bytes_conf: None,
        })
    }

    /// Create minimal context (for CLI usage without full execution context)
    pub fn minimal(
        db: std::sync::Arc<DatabasePool>,
        org_id: Uuid,
        env_id: Option<Uuid>,
        user_id: Uuid,
    ) -> Self {
        let scope = env_id
            .map(|env_id| crate::db::DatabaseScope::Environment { org_id, env_id })
            .unwrap_or(crate::db::DatabaseScope::Organization { org_id });
        let db = std::sync::Arc::new(db.with_scope(scope));
        Self {
            db,
            org_id,
            env_id,
            user_id,
            run_id: None,
            run_provenance: None,
            file_max_bytes_conf: None,
        }
    }

    /// Ensure the current run row exists before writing file metadata that
    /// references it. Mirrors `::hot::task/start` — the emitter pipeline is
    /// async, so run:start may not be committed yet when user file I/O runs.
    pub async fn ensure_run_row(&self) -> Result<(), String> {
        let (Some(run_id), Some(env_id), Some(provenance)) =
            (self.run_id, self.env_id, self.run_provenance.as_ref())
        else {
            return Ok(());
        };

        crate::db::run::Run::ensure_run_exists(
            &self.db,
            &run_id,
            &env_id,
            &provenance.stream_id,
            provenance.build_id.as_ref(),
            provenance.run_type_id,
            // No origin_run_id: the parent run row may be equally unflushed,
            // and a cascading FK failure here would defeat the placeholder.
            // The real run:start backfills origin on placeholder rows.
            None,
            &self.user_id,
            provenance.access_id.as_ref(),
        )
        .await
        .map_err(|e| e.to_string())
    }

    /// Set the config-level file size ceiling.
    pub fn with_file_max_bytes(mut self, conf: &crate::val::Val) -> Self {
        self.file_max_bytes_conf = Self::conf_file_max_bytes(conf);
        self
    }

    /// Effective max file bytes: min(config ceiling, plan limit).
    /// Config values < 0 mean "no ceiling, defer to plan".
    pub fn effective_file_max_bytes(&self, plan_max: i64) -> i64 {
        Self::resolve_file_max_bytes(self.file_max_bytes_conf, plan_max)
    }

    /// Static version for use without a full context (e.g. API handlers
    /// that check before constructing a context).
    pub fn resolve_file_max_bytes(conf_max: Option<i64>, plan_max: i64) -> i64 {
        match conf_max {
            Some(v) if v >= 0 => std::cmp::min(v, plan_max),
            _ => plan_max,
        }
    }

    /// Extract `hot.file.max-bytes` from a config Val.
    pub fn conf_file_max_bytes(conf: &crate::val::Val) -> Option<i64> {
        conf.get("file")
            .and_then(|f| f.get("max-bytes"))
            .and_then(|v| match v {
                crate::val::Val::Int(i) => Some(i),
                _ => None,
            })
    }
}

/// Trait for file storage backends
#[async_trait]
pub trait FileStorage: Send + Sync {
    /// Write file to storage and database (transactional)
    async fn write_file(
        &self,
        path: &str,
        content: &[u8],
        content_type: Option<&str>,
        ctx: &FileStorageContext,
    ) -> Result<FileMetadata, String>;

    /// Conditionally write: succeeds only while the stored record still
    /// matches `expected_etag` (or does not exist yet, for None). Returns
    /// Ok(None) when the compare-and-swap loses.
    ///
    /// Atomicity: the bytes are staged at a unique per-attempt storage key
    /// first, and the record swap to that key is the commit point. A failed
    /// byte write or a lost race leaves the record (and the bytes it
    /// references) untouched, and concurrent winners can never clobber each
    /// other's blobs — this is the serialization point for read-modify-write
    /// consumers like ::hot::sqlite.
    async fn write_file_if(
        &self,
        path: &str,
        content: &[u8],
        content_type: Option<&str>,
        expected_etag: Option<&str>,
        ctx: &FileStorageContext,
    ) -> Result<Option<FileMetadata>, String>;

    async fn read_file(&self, path: &str, ctx: &FileStorageContext) -> Result<Vec<u8>, String>;

    /// Open an authorized file for streaming.
    ///
    /// Backends that can expose an async reader should override this. The
    /// default keeps compatibility for external storage implementations while
    /// allowing callers to reject non-streaming reads before materialization.
    async fn open_file_stream(
        &self,
        _path: &str,
        _ctx: &FileStorageContext,
    ) -> Result<Option<FileReadStream>, String> {
        Ok(None)
    }

    /// Delete file from storage and mark inactive in database
    /// Returns true if the file existed and was deleted, false if it didn't exist
    async fn delete_file(&self, path: &str, ctx: &FileStorageContext) -> Result<bool, String>;

    /// Check if file exists in storage
    async fn file_exists(&self, path: &str, ctx: &FileStorageContext) -> Result<bool, String>;

    /// Get file metadata from database
    async fn get_file_metadata(
        &self,
        path: &str,
        ctx: &FileStorageContext,
    ) -> Result<FileMetadata, String>;

    /// List files by prefix from database
    async fn list_files(
        &self,
        prefix: &str,
        ctx: &FileStorageContext,
    ) -> Result<Vec<FileMetadata>, String>;

    /// List at most `limit` files. Backends should override this to apply the
    /// bound in their metadata store instead of materializing the full list.
    async fn list_files_bounded(
        &self,
        prefix: &str,
        limit: usize,
        ctx: &FileStorageContext,
    ) -> Result<Vec<FileMetadata>, String> {
        let mut files = self.list_files(prefix, ctx).await?;
        files.truncate(limit);
        Ok(files)
    }

    /// Get storage backend type
    fn storage_type(&self) -> &str;

    /// Initiate a multipart upload. Returns a backend-specific upload identifier
    /// (S3 upload ID or local temp directory name).
    async fn initiate_multipart_upload(
        &self,
        _path: &str,
        _content_type: Option<&str>,
        _ctx: &FileStorageContext,
    ) -> Result<String, String> {
        Err("Multipart upload not supported by this storage backend".to_string())
    }

    /// Upload a single part. Returns the part's etag.
    async fn upload_part(
        &self,
        _backend_upload_id: &str,
        _path: &str,
        _part_number: i32,
        _content: &[u8],
        _ctx: &FileStorageContext,
    ) -> Result<String, String> {
        Err("Multipart upload not supported by this storage backend".to_string())
    }

    /// Complete a multipart upload by assembling all parts.
    /// `parts` is a list of (part_number, etag) pairs in order.
    /// Returns the final file metadata.
    ///
    /// Each upload assembles into its own destination, so a retry (after a
    /// crash, lost response, or failed commit) recognizes and reuses what an
    /// earlier attempt produced. The record is only swapped from `base`, the
    /// version the completion was claimed against; if the file changed since,
    /// nothing is committed and the error starts with
    /// [`COMPLETION_CONFLICT`].
    #[allow(clippy::too_many_arguments)]
    async fn complete_multipart_upload(
        &self,
        _backend_upload_id: &str,
        _path: &str,
        _parts: &[(i32, String)],
        _total_size: i64,
        _content_type: Option<&str>,
        _base: &CommitBase,
        _ctx: &FileStorageContext,
    ) -> Result<FileMetadata, String> {
        Err("Multipart upload not supported by this storage backend".to_string())
    }

    /// Abort a multipart upload and clean up any uploaded parts, plus an
    /// assembled destination that no record references. Aborting an upload
    /// whose backend state is already gone succeeds, so aborts and the expiry
    /// sweep converge.
    async fn abort_multipart_upload(
        &self,
        _backend_upload_id: &str,
        _path: &str,
        _ctx: &FileStorageContext,
    ) -> Result<(), String> {
        Err("Multipart upload not supported by this storage backend".to_string())
    }

    /// Write internal blob bytes to storage without creating a `file` row.
    /// Used by the content-addressed blob store; blob metadata lives in
    /// `blob_object`/`blob_ref`, so these bytes must never appear in the
    /// Files UI or file listings. Returns the backend storage path.
    async fn write_blob_bytes(
        &self,
        _storage_key: &str,
        _content: &[u8],
        _content_type: Option<&str>,
        _ctx: &FileStorageContext,
    ) -> Result<String, String> {
        Err("Blob storage not supported by this storage backend".to_string())
    }

    /// Read internal blob bytes written by `write_blob_bytes`. No `file` row
    /// access control applies; callers must authorize via `blob_ref` first.
    async fn read_blob_bytes(
        &self,
        _storage_key: &str,
        _ctx: &FileStorageContext,
    ) -> Result<Vec<u8>, String> {
        Err("Blob storage not supported by this storage backend".to_string())
    }

    /// Delete internal blob bytes written by `write_blob_bytes`.
    async fn delete_blob_bytes(
        &self,
        _storage_key: &str,
        _ctx: &FileStorageContext,
    ) -> Result<(), String> {
        Err("Blob storage not supported by this storage backend".to_string())
    }

    /// Delete a file object collection found unreferenced (see
    /// [`collect_retired_objects`]). An object that is already gone, or that
    /// lies outside this storage (another bucket or base directory), counts
    /// as deleted: it is not this storage's to remove.
    async fn delete_storage_object(&self, _storage_path: &str) -> Result<(), String> {
        Err("Object collection is not supported by this storage backend".to_string())
    }
}

/// How long a retired file object stays readable before collection deletes
/// it: a read that resolved the object before an overwrite or delete
/// replaced it can finish.
pub const RETIRED_OBJECT_GRACE: std::time::Duration = std::time::Duration::from_secs(15 * 60);

/// Delete the objects of `storage`'s backend that were retired before
/// `retired_before` and that no active file version references. Returns how
/// many were deleted; an object that can't be deleted now stays retired for
/// the next collection.
pub async fn collect_retired_objects(
    db: &DatabasePool,
    storage: &dyn FileStorage,
    retired_before: DateTime<Utc>,
    limit: i64,
) -> Result<usize, String> {
    use crate::db::file::{
        collectable_retired_objects, forget_retired_object, retired_object_collectable,
    };

    let db = db.with_scope(crate::db::DatabaseScope::Maintenance {
        operation: crate::db::MaintenanceOperation::Retention,
        org_id: None,
    });
    let backend = storage.storage_type();
    let mut collected = 0;
    for storage_path in collectable_retired_objects(&db, backend, retired_before, limit).await? {
        // Checked again right before deleting: a retried multipart completion
        // can commit its (deterministic) object again meanwhile.
        if !retired_object_collectable(&db, backend, &storage_path, retired_before).await? {
            continue;
        }
        if let Err(error) = storage.delete_storage_object(&storage_path).await {
            tracing::warn!(%error, "Could not delete a retired file object; the next collection retries it");
            continue;
        }
        forget_retired_object(&db, backend, &storage_path, retired_before).await?;
        collected += 1;
    }
    Ok(collected)
}

/// Register a staged object for collection just before committing it; see
/// [`crate::db::file::register_staged_object`]. Multipart destinations are not
/// registered: an upload's expiry aborts and removes one that never commits.
async fn register_staged(
    ctx: &FileStorageContext,
    backend: &str,
    storage_path: &str,
) -> Result<(), String> {
    crate::db::file::register_staged_object(&ctx.db, ctx.org_id, ctx.env_id, backend, storage_path)
        .await
}

// ============================================================================
// Path Utilities
// ============================================================================

/// Normalize a file path to prevent directory traversal
/// Quota checks shared by the conditional write path (mirrors write_file).
async fn check_write_quota(ctx: &FileStorageContext, content_len: i64) -> Result<(), String> {
    use crate::db::Features;

    let features = Features::try_resolve_for_org(&ctx.db, &ctx.org_id)
        .await
        .map_err(|e| e.to_string())?;
    let max_file_bytes = ctx.effective_file_max_bytes(features.file_upload_max_bytes());
    if content_len > max_file_bytes {
        return Err(format!(
            "File size {} bytes exceeds maximum allowed {} bytes",
            content_len, max_file_bytes
        ));
    }
    // Account storage is checked atomically with the metadata commit, using
    // the replacement delta. A pre-upload total cannot reserve sibling capacity.
    Ok(())
}

/// Longest file name a local filesystem accepts, in bytes (NAME_MAX).
const LOCAL_NAME_MAX: usize = 255;

/// Object stores bound whole keys, not their segments.
const KEY_NAME_MAX: usize = usize::MAX;

/// `normalized_path` with `suffix` appended to its file name. The suffix makes
/// the location unique, so the name is shortened instead when the two would
/// exceed `name_max` bytes.
fn suffixed_rel_path(normalized_path: &str, suffix: &str, name_max: usize) -> String {
    let (dir, name) = match normalized_path.rsplit_once('/') {
        Some((dir, name)) => (Some(dir), name),
        None => (None, normalized_path),
    };
    let mut keep = name.len().min(name_max.saturating_sub(suffix.len()));
    while !name.is_char_boundary(keep) {
        keep -= 1;
    }
    let name = &name[..keep];
    match dir {
        Some(dir) => format!("{dir}/{name}{suffix}"),
        None => format!("{name}{suffix}"),
    }
}

/// Unique per-attempt relative storage location for a conditional write.
/// Each attempt stages its bytes at its own key and then swaps the record's
/// storage_path to reference them, so no two attempts ever share a blob and
/// a loser's cleanup can never delete a winner's bytes.
fn cas_blob_rel_path(normalized_path: &str, name_max: usize) -> String {
    let suffix = format!(".cas-{}", Uuid::new_v4().simple());
    suffixed_rel_path(normalized_path, &suffix, name_max)
}

/// The record compare-and-swap behind write_file_if: conditional update when
/// an etag is expected, plain insert (racing on the partial unique index
/// over active (org_id, env_id, path)) when the file must not exist yet.
///
/// The blob bytes must already be durably written at `storage_path` when
/// this runs — the record swap is the commit point, and a reader may follow
/// the new storage_path the instant the swap lands.
///
/// Ok(None) = the CAS lost. On a win, the superseded blob was retired for
/// collection by the same statement.
#[allow(clippy::too_many_arguments)]
async fn cas_file_record(
    backend: &str,
    storage_path: &str,
    normalized_path: &str,
    content_len: i64,
    etag: &str,
    content_type: Option<&str>,
    expected_etag: Option<&str>,
    ctx: &FileStorageContext,
) -> Result<Option<FileMetadata>, String> {
    if let Err(e) = ctx.ensure_run_row().await {
        tracing::debug!(
            path = %normalized_path,
            "Conditional file write: could not ensure run row: {}",
            e
        );
    }
    match expected_etag {
        Some(expected) => {
            let existing =
                match get_file_by_path(&ctx.db, normalized_path, ctx.org_id, ctx.env_id).await {
                    Ok(r) => r,
                    // Deleted (or never existed) since checkout: the CAS loses.
                    Err(_) => return Ok(None),
                };
            let updated = crate::db::file::update_file_record_if_etag(
                &ctx.db,
                existing.file_id,
                expected,
                content_len,
                Some(etag),
                content_type,
                storage_path,
                ctx.user_id,
                ctx.run_id,
            )
            .await?;
            Ok(updated.map(FileMetadata::from_file_record))
        }
        None => {
            match insert_file_record(
                &ctx.db,
                normalized_path,
                content_len,
                Some(etag),
                content_type,
                backend,
                storage_path,
                ctx.org_id,
                ctx.env_id,
                ctx.user_id,
                ctx.run_id,
            )
            .await
            {
                Ok(rec) => Ok(Some(FileMetadata::from_file_record(rec))),
                // A concurrent creator won the unique index race: CAS lost.
                Err(msg) if is_unique_violation(&msg) => Ok(None),
                Err(e) => Err(e),
            }
        }
    }
}

/// Which file version a commit may replace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommitBase {
    /// Whatever is current: an unconditional write (last writer wins).
    Any,
    /// No active file: the path was empty when the completion was claimed.
    Absent,
    /// The active file still stored at this location.
    Path(String),
}

impl CommitBase {
    /// Decode `file_upload.completion_base`: NULL for uploads claimed before
    /// the base was recorded, `""` when the path had no active file.
    pub fn from_column(value: Option<&str>) -> Self {
        match value {
            None => CommitBase::Any,
            Some("") => CommitBase::Absent,
            Some(path) => CommitBase::Path(path.to_string()),
        }
    }
}

/// Prefix of the error a multipart completion returns when the file changed
/// after the completion was claimed. Nothing was written; the upload can
/// only be aborted.
pub const COMPLETION_CONFLICT: &str = "Upload completion conflict";

/// Outcome of [`commit_file_record`].
enum Committed {
    /// The record now references the new location. The location it replaced
    /// was retired for collection by the same statement.
    Swapped(FileMetadata),
    /// The record already referenced the new location (a retried multipart
    /// completion whose earlier attempt committed).
    Already(FileMetadata),
    /// The file changed since `base`; nothing was written.
    Conflict,
}

impl Committed {
    /// The result of an unconditional write ([`CommitBase::Any`]).
    fn into_write(self, normalized_path: &str) -> Result<FileMetadata, String> {
        match self {
            Committed::Swapped(meta) | Committed::Already(meta) => Ok(meta),
            Committed::Conflict => Err(format!(
                "{COMPLETION_CONFLICT}: {normalized_path} changed during the write"
            )),
        }
    }
}

/// Attempts at the record swap before giving up on a hot path.
const COMMIT_ATTEMPTS: usize = 8;

/// Commit a write whose bytes are already durable at `storage_path`: swap
/// the active record for the path to it, or insert one.
///
/// An unconditional write replaces whatever version is active, and a
/// completion only the version it was claimed against. The location a swap
/// replaces is retired for collection by the same statement, so concurrent
/// writers never need to know which version they replaced, and contention
/// alone never fails a write. A write that races a delete re-inserts rather
/// than landing on the deactivated row, and a concurrent creator winning the
/// unique index turns the insert into a swap. Records owned by another
/// backend are refused: an update would leave `storage_backend` pointing at
/// the old backend.
#[allow(clippy::too_many_arguments)]
async fn commit_file_record(
    backend: &str,
    storage_path: &str,
    normalized_path: &str,
    content_len: i64,
    etag: Option<&str>,
    content_type: Option<&str>,
    base: &CommitBase,
    ctx: &FileStorageContext,
) -> Result<Committed, String> {
    if let Err(e) = ctx.ensure_run_row().await {
        tracing::debug!(
            run_id = ?ctx.run_id,
            path = %normalized_path,
            "File write: could not ensure run row before metadata insert: {}",
            e
        );
    }
    for _ in 0..COMMIT_ATTEMPTS {
        let Some(existing) =
            find_file_by_path(&ctx.db, normalized_path, ctx.org_id, ctx.env_id).await?
        else {
            if matches!(base, CommitBase::Path(_)) {
                // The file the completion was claimed against was deleted.
                return Ok(Committed::Conflict);
            }
            match insert_file_record(
                &ctx.db,
                normalized_path,
                content_len,
                etag,
                content_type,
                backend,
                storage_path,
                ctx.org_id,
                ctx.env_id,
                ctx.user_id,
                ctx.run_id,
            )
            .await
            {
                Ok(rec) => return Ok(Committed::Swapped(FileMetadata::from_file_record(rec))),
                // A concurrent creator won the unique index: swap instead.
                Err(msg) if is_unique_violation(&msg) => continue,
                Err(e) => return Err(e),
            }
        };
        ensure_record_backend(&existing.storage_backend, backend, normalized_path)?;
        if existing.storage_path == storage_path {
            return Ok(Committed::Already(FileMetadata::from_file_record(existing)));
        }
        let updated = match base {
            CommitBase::Any => {
                update_file_record_if_active(
                    &ctx.db,
                    existing.file_id,
                    content_len,
                    etag,
                    content_type,
                    storage_path,
                    ctx.user_id,
                    ctx.run_id,
                )
                .await?
            }
            CommitBase::Path(path) if *path == existing.storage_path => {
                update_file_record_if_storage_path(
                    &ctx.db,
                    existing.file_id,
                    &existing.storage_path,
                    content_len,
                    etag,
                    content_type,
                    storage_path,
                    ctx.user_id,
                    ctx.run_id,
                )
                .await?
            }
            CommitBase::Absent | CommitBase::Path(_) => return Ok(Committed::Conflict),
        };
        if let Some(updated) = updated {
            return Ok(Committed::Swapped(FileMetadata::from_file_record(updated)));
        }
        // Deleted, or (for a claimed base) changed, since the read: look again.
    }
    Err(format!(
        "File record for {} kept changing during write; retry",
        normalized_path
    ))
}

/// Whether an insert failed on the active-path unique index.
fn is_unique_violation(msg: &str) -> bool {
    msg.contains("UNIQUE") || msg.contains("unique") || msg.contains("duplicate key")
}

/// What staged bytes are to the active record.
enum StagedBytes {
    /// The record references them (after a failed commit: it landed even
    /// though it reported an error).
    Committed(Box<FileMetadata>),
    /// Nothing references them; they can be deleted.
    Unreferenced,
    /// The record could not be read; keep the bytes.
    Unknown,
}

/// Whether the active record references the bytes at `storage_path`.
///
/// Used after a failed commit, which can fail after its row change landed
/// (SQLite re-reads the row in a second statement; a Postgres
/// acknowledgement can be lost), and before deleting an assembled upload:
/// bytes are only deleted once the record provably references something
/// else. An orphaned object is cheaper than a record pointing at deleted
/// bytes.
async fn staged_bytes_state(
    ctx: &FileStorageContext,
    normalized_path: &str,
    storage_path: &str,
) -> StagedBytes {
    match find_file_by_path(&ctx.db, normalized_path, ctx.org_id, ctx.env_id).await {
        Ok(Some(record)) if record.storage_path == storage_path => {
            StagedBytes::Committed(Box::new(FileMetadata::from_file_record(record)))
        }
        Ok(_) => StagedBytes::Unreferenced,
        Err(_) => StagedBytes::Unknown,
    }
}

/// Lookups of a record whose bytes vanished before giving up on a read.
const READ_ATTEMPTS: usize = 3;

/// Read the active record for `normalized_path` and hand it to `fetch`,
/// which returns `Ok(None)` when the bytes it names are gone. An overwrite
/// retires the superseded bytes, and collection deletes them after
/// [`RETIRED_OBJECT_GRACE`], so a reader slower than that can lose the race
/// between its lookup and its fetch; the record is read again and, if it
/// moved, the fetch is retried.
async fn fetch_current<'a, T>(
    ctx: &FileStorageContext,
    normalized_path: &str,
    mut fetch: impl FnMut(&FileRecord) -> futures::future::BoxFuture<'a, Result<Option<T>, String>>,
) -> Result<(FileRecord, T), String> {
    let mut record = get_file_by_path(&ctx.db, normalized_path, ctx.org_id, ctx.env_id).await?;
    for _ in 0..READ_ATTEMPTS {
        if let Some(found) = fetch(&record).await? {
            return Ok((record, found));
        }
        let latest = get_file_by_path(&ctx.db, normalized_path, ctx.org_id, ctx.env_id).await?;
        if latest.storage_path == record.storage_path {
            break;
        }
        record = latest;
    }
    Err(format!("File not found in storage: {}", normalized_path))
}

/// Deactivate the current version of `normalized_path` and return it; None
/// when no file exists. Deactivating retires the version's bytes for
/// collection, except for a record that predates storage paths, whose bytes
/// the caller removes. An overwrite committing between the lookup and the
/// deactivation moves the record to new bytes, so the compare-and-swap fails
/// and the lookup repeats: the returned version is the one deactivated.
async fn deactivate_current(
    ctx: &FileStorageContext,
    normalized_path: &str,
) -> Result<Option<FileRecord>, String> {
    for _ in 0..COMMIT_ATTEMPTS {
        let record = match get_file_by_path(&ctx.db, normalized_path, ctx.org_id, ctx.env_id).await
        {
            Ok(record) => record,
            Err(_) => return Ok(None), // File didn't exist
        };
        if mark_file_inactive(
            &ctx.db,
            record.file_id,
            &record.storage_path,
            ctx.user_id,
            ctx.run_id,
        )
        .await?
        {
            return Ok(Some(record));
        }
    }
    Err(format!(
        "File {} kept changing during delete; try again",
        normalized_path
    ))
}

/// Refuse to operate on bytes persisted by a different backend. Changing the
/// configured backend does not migrate data, and silently routing an old
/// reference through the new backend would read or delete the wrong object.
pub fn ensure_record_backend(
    record_backend: &str,
    configured_backend: &str,
    path: &str,
) -> Result<(), String> {
    if record_backend == configured_backend {
        Ok(())
    } else {
        Err(format!(
            "File {} is stored in the '{}' backend, but this installation uses '{}'; \
             migrate the bytes and persisted references before switching backends",
            path, record_backend, configured_backend
        ))
    }
}

/// Refuse a write over an active record owned by another backend before any
/// bytes are staged. `update_file_record` keeps the record's backend, so
/// letting the write through would pair one backend's id with another
/// backend's storage path.
async fn ensure_path_writable(
    backend: &str,
    normalized_path: &str,
    ctx: &FileStorageContext,
) -> Result<(), String> {
    if let Ok(existing) = get_file_by_path(&ctx.db, normalized_path, ctx.org_id, ctx.env_id).await {
        ensure_record_backend(&existing.storage_backend, backend, normalized_path)?;
    }
    Ok(())
}

/// Unique per-write object location. Each unconditional write stages its
/// bytes at its own key and the record swap commits it, so concurrent
/// writers never overwrite each other's bytes.
fn unique_write_rel_path(normalized_path: &str, name_max: usize) -> String {
    let suffix = format!(".v-{}", Uuid::new_v4().simple());
    suffixed_rel_path(normalized_path, &suffix, name_max)
}

/// Location of a completed multipart upload: one per upload, so a retried
/// completion finds what an earlier attempt assembled.
fn multipart_rel_path(normalized_path: &str, upload: &str, name_max: usize) -> String {
    suffixed_rel_path(normalized_path, &format!(".mp-{}", upload), name_max)
}

/// The error for a completion whose file changed after it was claimed.
fn completion_conflict(normalized_path: &str) -> String {
    format!(
        "{COMPLETION_CONFLICT}: {normalized_path} changed after the upload's completion began; \
         abort the upload and upload again to replace it"
    )
}

pub fn normalize_path(path: &str) -> Result<String, String> {
    // Accept the same hot:// paths that the in-container hotbox CLI accepts.
    // hotbox strips this scheme before it asks the file server for /files/<path>,
    // so language-level file writes must canonicalize the same way or a file
    // written as hot://foo/bar will be stored under a different DB path than
    // hotbox later reads.
    let path = path.strip_prefix("hot://").unwrap_or(path);

    // Remove leading slash if present
    let path = path.trim_start_matches('/');

    // Split into components and resolve . and ..
    let mut components: Vec<&str> = Vec::new();
    for component in path.split('/') {
        match component {
            "" | "." => continue,
            ".." => {
                if components.is_empty() {
                    return Err("Path escapes storage root".to_string());
                }
                components.pop();
            }
            c => components.push(c),
        }
    }

    if components.is_empty() {
        return Err("Empty path after normalization".to_string());
    }

    Ok(components.join("/"))
}

/// Validate that a path is safe (no traversal attacks)
pub fn validate_path_security(path: &str) -> Result<(), String> {
    if path.is_empty() {
        return Err("Empty path not allowed".to_string());
    }

    // Check for suspicious patterns
    if path.contains("..") {
        return Err("Path contains '..' which is not allowed".to_string());
    }

    if path.starts_with('/') {
        return Err("Absolute paths are not allowed".to_string());
    }

    // Check for null bytes
    if path.contains('\0') {
        return Err("Path contains null bytes".to_string());
    }

    Ok(())
}

/// Content identity used for multipart retries, independent of backend ETags.
pub fn compute_part_hash(content: &[u8]) -> String {
    blake3::hash(content).to_hex().to_string()
}

/// Compute MD5 hash of content.
pub fn compute_md5(content: &[u8]) -> String {
    format!("{:x}", md5::compute(content))
}

/// Detect content type from file extension and content
pub fn detect_content_type(path: &str, _content: &[u8]) -> Option<String> {
    let path = Path::new(path);
    let extension = path.extension()?.to_str()?;

    let content_type = match extension.to_lowercase().as_str() {
        "txt" => "text/plain",
        "html" | "htm" => "text/html",
        "css" => "text/css",
        "js" => "application/javascript",
        "json" => "application/json",
        "xml" => "application/xml",
        "pdf" => "application/pdf",
        "zip" => "application/zip",
        "tar" => "application/x-tar",
        "gz" => "application/gzip",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "svg" => "image/svg+xml",
        "webp" => "image/webp",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "csv" => "text/csv",
        "md" => "text/markdown",
        "hot" => "text/x-hot",
        _ => "application/octet-stream",
    };

    Some(content_type.to_string())
}

// ============================================================================
// Multipart Upload Helpers
// ============================================================================

/// Default part size for multipart uploads (64 MB).
pub const DEFAULT_PART_SIZE: u64 = 64 * 1024 * 1024;

/// S3 minimum part size (5 MB) — hard constraint from AWS.
pub const S3_MIN_PART_SIZE: u64 = 5 * 1024 * 1024;

/// Maximum number of parts per upload (S3 constraint).
pub const MAX_UPLOAD_PARTS: u64 = 10_000;

/// Compute the recommended part size for a given file size.
/// Ensures we never exceed MAX_UPLOAD_PARTS while preferring DEFAULT_PART_SIZE.
pub fn compute_part_size(expected_size: u64) -> u64 {
    let min_for_count = expected_size.div_ceil(MAX_UPLOAD_PARTS);
    std::cmp::max(DEFAULT_PART_SIZE, min_for_count)
}

/// Validate a part number (1-based, max 10,000).
pub fn validate_part_number(part_number: i32) -> Result<(), String> {
    if part_number < 1 || part_number > MAX_UPLOAD_PARTS as i32 {
        return Err(format!(
            "Part number must be between 1 and {}, got {}",
            MAX_UPLOAD_PARTS, part_number
        ));
    }
    Ok(())
}

// ============================================================================
// Local Filesystem Storage
// ============================================================================

/// Local filesystem storage for files
pub struct LocalFileStorage {
    base_path: PathBuf,
}

impl LocalFileStorage {
    /// Create a new local file storage with the specified directory
    pub fn new(base_path: PathBuf) -> Self {
        Self { base_path }
    }

    /// Create from environment or use default
    pub fn from_env() -> Self {
        Self::new(
            std::env::var("HOT_FILE_STORAGE_PATH")
                .map(PathBuf::from)
                .unwrap_or_else(|_| crate::lang::cache::paths::get_file_storage_dir()),
        )
    }

    /// Get the full filesystem path for a relative file path with multi-tenant isolation
    /// Structure: base_path/org_id/env_id/user_path
    fn full_path(&self, path: &str, ctx: &FileStorageContext) -> PathBuf {
        let mut full = self.base_path.clone();

        // Add org_id for tenant isolation (without dashes for cleaner paths)
        full.push(ctx.org_id.simple().to_string());

        // Add env_id if present for environment isolation
        if let Some(env_id) = ctx.env_id {
            full.push(env_id.simple().to_string());
        } else {
            // Default to "default" environment if no env_id
            full.push("default");
        }

        // Add the user's file path
        full.push(path);

        full
    }

    /// Where a record's bytes live. The record's storage_path is
    /// authoritative (every write commits by swapping it to the bytes it
    /// staged); records that predate storage_path fall back to the
    /// deterministic location.
    fn record_location(
        &self,
        record: &FileRecord,
        normalized_path: &str,
        ctx: &FileStorageContext,
    ) -> PathBuf {
        if record.storage_backend == "local" && !record.storage_path.is_empty() {
            let recorded = PathBuf::from(&record.storage_path);
            if recorded.exists() {
                return recorded;
            }
        }
        self.full_path(normalized_path, ctx)
    }

    /// Concatenate uploaded parts (already sorted) into `dest`, computing
    /// the MD5 etag as bytes are written. Returns (etag, size).
    async fn assemble_parts(
        upload_dir: &Path,
        sorted_parts: &[(i32, String)],
        dest: &Path,
    ) -> Result<(String, i64), String> {
        use tokio::io::AsyncWriteExt;

        let mut file = tokio::fs::File::create(dest)
            .await
            .map_err(|e| format!("Failed to create output file: {}", e))?;
        let mut hasher = md5::Context::new();
        let mut size = 0i64;
        for (part_number, etag) in sorted_parts {
            validate_part_number(*part_number)?;
            let mut part_path = Self::part_path(upload_dir, *part_number, etag);
            if !tokio::fs::try_exists(&part_path).await.unwrap_or(false) {
                // Uploads started before parts were named by content.
                part_path = upload_dir.join(format!("part_{:05}", part_number));
            }
            let part_data = tokio::fs::read(&part_path)
                .await
                .map_err(|e| format!("Failed to read part {}: {}", part_number, e))?;
            hasher.consume(&part_data);
            size += part_data.len() as i64;
            file.write_all(&part_data)
                .await
                .map_err(|e| format!("Failed to write part {}: {}", part_number, e))?;
        }
        // tokio completes writes on a blocking thread: `flush` surfaces an
        // error from the last one, which `sync_all` would silently drop.
        file.flush()
            .await
            .map_err(|e| format!("Failed to write assembled file: {}", e))?;
        file.sync_all()
            .await
            .map_err(|e| format!("Failed to flush assembled file: {}", e))?;
        let written = file
            .metadata()
            .await
            .map_err(|e| format!("Failed to inspect assembled file: {}", e))?
            .len();
        if written != size as u64 {
            return Err(format!(
                "Assembled file has {} bytes on disk, expected {}",
                written, size
            ));
        }
        Ok((format!("{:x}", hasher.finalize()), size))
    }

    /// File for one uploaded part body. The name carries the content hash
    /// (the part's etag), so different bodies for one part number never
    /// share a file.
    fn part_path(upload_dir: &Path, part_number: i32, etag: &str) -> PathBuf {
        if etag.len() == 32 && etag.bytes().all(|b| b.is_ascii_hexdigit()) {
            upload_dir.join(format!("part_{:05}-{}", part_number, etag))
        } else {
            upload_dir.join(format!("part_{:05}", part_number))
        }
    }
}

#[async_trait]
impl FileStorage for LocalFileStorage {
    async fn write_file(
        &self,
        path: &str,
        content: &[u8],
        content_type: Option<&str>,
        ctx: &FileStorageContext,
    ) -> Result<FileMetadata, String> {
        let normalized_path = normalize_path(path)?;
        validate_path_security(&normalized_path)?;
        check_write_quota(ctx, content.len() as i64).await?;
        ensure_path_writable("local", &normalized_path, ctx).await?;

        let etag = compute_md5(content);
        let content_type_final = content_type
            .map(|s| s.to_string())
            .or_else(|| detect_content_type(&normalized_path, content));

        // Stage at a unique path; the record swap below is the commit point.
        // Concurrent writers never share bytes, so the committed record's
        // size and etag always describe the file it references, and a
        // failed write leaves the current file untouched.
        let staged_path = self.full_path(
            &unique_write_rel_path(&normalized_path, LOCAL_NAME_MAX),
            ctx,
        );
        if let Some(parent) = staged_path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| format!("Failed to create parent directories: {}", e))?;
        }
        if let Err(e) = tokio::fs::write(&staged_path, content).await {
            let _ = tokio::fs::remove_file(&staged_path).await;
            return Err(format!("Failed to write file to storage: {}", e));
        }
        let storage_path = staged_path.to_string_lossy().to_string();
        if let Err(e) = register_staged(ctx, "local", &storage_path).await {
            let _ = tokio::fs::remove_file(&staged_path).await;
            return Err(e);
        }

        let committed = commit_file_record(
            "local",
            &storage_path,
            &normalized_path,
            content.len() as i64,
            Some(&etag),
            content_type_final.as_deref(),
            &CommitBase::Any,
            ctx,
        )
        .await
        .and_then(|committed| committed.into_write(&normalized_path));
        match committed {
            Ok(meta) => {
                tracing::debug!("Wrote file {} to local storage", normalized_path);
                Ok(meta)
            }
            Err(e) => match staged_bytes_state(ctx, &normalized_path, &storage_path).await {
                StagedBytes::Committed(meta) => Ok(*meta),
                StagedBytes::Unreferenced => {
                    let _ = tokio::fs::remove_file(&staged_path).await;
                    Err(e)
                }
                StagedBytes::Unknown => Err(e),
            },
        }
    }

    async fn write_file_if(
        &self,
        path: &str,
        content: &[u8],
        content_type: Option<&str>,
        expected_etag: Option<&str>,
        ctx: &FileStorageContext,
    ) -> Result<Option<FileMetadata>, String> {
        let normalized_path = normalize_path(path)?;
        validate_path_security(&normalized_path)?;
        check_write_quota(ctx, content.len() as i64).await?;
        ensure_path_writable("local", &normalized_path, ctx).await?;

        let etag = compute_md5(content);
        let content_type_final = content_type
            .map(|s| s.to_string())
            .or_else(|| detect_content_type(&normalized_path, content));

        // Stage the bytes at a unique key first; the record swap below is
        // the commit point. A failure or lost race past this write leaves
        // only an unreferenced staged blob, never a record pointing at
        // missing or stale bytes.
        let staged_path = self.full_path(&cas_blob_rel_path(&normalized_path, LOCAL_NAME_MAX), ctx);
        let storage_path = staged_path.to_string_lossy().to_string();
        if let Some(parent) = staged_path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| format!("Failed to create parent directories: {}", e))?;
        }
        if let Err(e) = tokio::fs::write(&staged_path, content).await {
            let _ = tokio::fs::remove_file(&staged_path).await;
            return Err(format!("Failed to write file to storage: {}", e));
        }
        if let Err(e) = register_staged(ctx, "local", &storage_path).await {
            let _ = tokio::fs::remove_file(&staged_path).await;
            return Err(e);
        }

        match cas_file_record(
            "local",
            &storage_path,
            &normalized_path,
            content.len() as i64,
            &etag,
            content_type_final.as_deref(),
            expected_etag,
            ctx,
        )
        .await
        {
            Ok(Some(meta)) => Ok(Some(meta)),
            Ok(None) => {
                let _ = tokio::fs::remove_file(&staged_path).await;
                Ok(None)
            }
            Err(e) => match staged_bytes_state(ctx, &normalized_path, &storage_path).await {
                StagedBytes::Committed(meta) => Ok(Some(*meta)),
                StagedBytes::Unreferenced => {
                    let _ = tokio::fs::remove_file(&staged_path).await;
                    Err(e)
                }
                StagedBytes::Unknown => Err(e),
            },
        }
    }

    async fn read_file(&self, path: &str, ctx: &FileStorageContext) -> Result<Vec<u8>, String> {
        let normalized_path = normalize_path(path)?;
        validate_path_security(&normalized_path)?;

        // The database record authorizes the read and names the bytes.
        let (_, bytes) = fetch_current(ctx, &normalized_path, |record| {
            let location = self.record_location(record, &normalized_path, ctx);
            Box::pin(async move {
                match tokio::fs::read(location).await {
                    Ok(bytes) => Ok(Some(bytes)),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                    Err(e) => Err(format!("Failed to read file: {}", e)),
                }
            })
        })
        .await?;
        Ok(bytes)
    }

    async fn open_file_stream(
        &self,
        path: &str,
        ctx: &FileStorageContext,
    ) -> Result<Option<FileReadStream>, String> {
        let normalized_path = normalize_path(path)?;
        validate_path_security(&normalized_path)?;

        let (record, file) = fetch_current(ctx, &normalized_path, |record| {
            let location = self.record_location(record, &normalized_path, ctx);
            Box::pin(async move {
                match tokio::fs::File::open(location).await {
                    Ok(file) => Ok(Some(file)),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                    Err(e) => Err(format!("Failed to open file for streaming: {}", e)),
                }
            })
        })
        .await?;

        Ok(Some(FileReadStream {
            metadata: FileMetadata::from_file_record(record),
            reader: Box::pin(file),
        }))
    }

    async fn delete_file(&self, path: &str, ctx: &FileStorageContext) -> Result<bool, String> {
        // Normalize and validate path
        let normalized_path = normalize_path(path)?;
        validate_path_security(&normalized_path)?;

        let Some(record) = deactivate_current(ctx, &normalized_path).await? else {
            return Ok(false);
        };

        // Deactivating retired the recorded bytes for collection. A record
        // that predates storage paths keeps its bytes at the path's own
        // location, which can also be a directory of other files.
        if record.storage_path.is_empty() {
            let legacy = self.full_path(&normalized_path, ctx);
            if tokio::fs::metadata(&legacy)
                .await
                .is_ok_and(|meta| meta.is_file())
            {
                tokio::fs::remove_file(&legacy)
                    .await
                    .map_err(|e| format!("Failed to delete file from storage: {}", e))?;
            }
        }

        tracing::debug!("Deleted file {} from local storage", normalized_path);
        Ok(true) // File existed and was deleted
    }

    async fn delete_storage_object(&self, storage_path: &str) -> Result<(), String> {
        let path = Path::new(storage_path);
        let inside = path.starts_with(&self.base_path)
            && path
                .components()
                .all(|part| !matches!(part, std::path::Component::ParentDir));
        if !inside {
            tracing::warn!(
                storage_path,
                "Not deleting a retired file object outside this storage directory"
            );
            return Ok(());
        }
        match tokio::fs::remove_file(path).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(format!("Failed to delete file object: {}", e)),
        }
    }

    async fn file_exists(&self, path: &str, ctx: &FileStorageContext) -> Result<bool, String> {
        let normalized_path = normalize_path(path)?;
        validate_path_security(&normalized_path)?;

        // Check database for active file
        match get_file_by_path(&ctx.db, &normalized_path, ctx.org_id, ctx.env_id).await {
            Ok(_) => Ok(true),
            Err(_) => Ok(false),
        }
    }

    async fn get_file_metadata(
        &self,
        path: &str,
        ctx: &FileStorageContext,
    ) -> Result<FileMetadata, String> {
        let normalized_path = normalize_path(path)?;
        validate_path_security(&normalized_path)?;

        let record = get_file_by_path(&ctx.db, &normalized_path, ctx.org_id, ctx.env_id).await?;
        Ok(FileMetadata::from_file_record(record))
    }

    async fn list_files(
        &self,
        prefix: &str,
        ctx: &FileStorageContext,
    ) -> Result<Vec<FileMetadata>, String> {
        use crate::db::file::list_files_by_prefix;

        let normalized_prefix = normalize_path(prefix).unwrap_or_else(|_| prefix.to_string());

        let records =
            list_files_by_prefix(&ctx.db, &normalized_prefix, ctx.org_id, ctx.env_id).await?;
        Ok(records
            .into_iter()
            .map(FileMetadata::from_file_record)
            .collect())
    }

    async fn list_files_bounded(
        &self,
        prefix: &str,
        limit: usize,
        ctx: &FileStorageContext,
    ) -> Result<Vec<FileMetadata>, String> {
        use crate::db::file::list_files_by_prefix_bounded;

        let normalized_prefix = normalize_path(prefix).unwrap_or_else(|_| prefix.to_string());
        let records = list_files_by_prefix_bounded(
            &ctx.db,
            &normalized_prefix,
            ctx.org_id,
            ctx.env_id,
            limit,
        )
        .await?;
        Ok(records
            .into_iter()
            .map(FileMetadata::from_file_record)
            .collect())
    }

    fn storage_type(&self) -> &str {
        "local"
    }

    async fn initiate_multipart_upload(
        &self,
        _path: &str,
        _content_type: Option<&str>,
        _ctx: &FileStorageContext,
    ) -> Result<String, String> {
        let backend_upload_id = Uuid::new_v4().to_string();
        let upload_dir = self.base_path.join(".uploads").join(&backend_upload_id);
        tokio::fs::create_dir_all(&upload_dir)
            .await
            .map_err(|e| format!("Failed to create multipart upload directory: {}", e))?;
        Ok(backend_upload_id)
    }

    async fn upload_part(
        &self,
        backend_upload_id: &str,
        _path: &str,
        part_number: i32,
        content: &[u8],
        _ctx: &FileStorageContext,
    ) -> Result<String, String> {
        validate_part_number(part_number)?;
        let upload_dir = self.base_path.join(".uploads").join(backend_upload_id);
        let etag = compute_md5(content);
        // Each distinct body gets its own file, named by part number and
        // content hash, so a retried or duplicate PUT can never change the
        // bytes behind a part the manifest already recorded. It is written
        // under a temporary name and renamed, so part files are always whole.
        let part_path = Self::part_path(&upload_dir, part_number, &etag);
        let tmp = upload_dir.join(format!(
            ".part_{:05}.tmp-{}",
            part_number,
            Uuid::new_v4().simple()
        ));
        let written = match tokio::fs::write(&tmp, content).await {
            Ok(()) => tokio::fs::rename(&tmp, &part_path).await,
            Err(e) => Err(e),
        };
        if let Err(e) = written {
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err(format!("Failed to write part {}: {}", part_number, e));
        }
        Ok(etag)
    }

    async fn complete_multipart_upload(
        &self,
        backend_upload_id: &str,
        path: &str,
        parts: &[(i32, String)],
        total_size: i64,
        content_type: Option<&str>,
        base: &CommitBase,
        ctx: &FileStorageContext,
    ) -> Result<FileMetadata, String> {
        let normalized_path = normalize_path(path)?;
        validate_path_security(&normalized_path)?;
        ensure_path_writable("local", &normalized_path, ctx).await?;

        let upload_dir = self.base_path.join(".uploads").join(backend_upload_id);
        let dest = self.full_path(
            &multipart_rel_path(&normalized_path, backend_upload_id, LOCAL_NAME_MAX),
            ctx,
        );
        let storage_path = dest.to_string_lossy().to_string();

        // A retried completion whose earlier attempt already committed.
        if let StagedBytes::Committed(meta) =
            staged_bytes_state(ctx, &normalized_path, &storage_path).await
        {
            let _ = tokio::fs::remove_dir_all(&upload_dir).await;
            return Ok(*meta);
        }

        let mut sorted_parts = parts.to_vec();
        sorted_parts.sort_by_key(|(num, _)| *num);

        // Assemble beside the destination and rename it into place, never
        // over the live file: the record swap is the commit point, so a
        // failed assembly or commit leaves the current file and its metadata
        // untouched, and the destination only ever holds a whole file.
        if let Some(parent) = dest.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| format!("Failed to create parent directories: {}", e))?;
        }
        // A short name of its own: the destination's name may already be as
        // long as the filesystem allows.
        let tmp = dest.with_file_name(format!(".tmp-{}", Uuid::new_v4().simple()));
        let (etag, actual_size) = match Self::assemble_parts(&upload_dir, &sorted_parts, &tmp).await
        {
            Ok(assembled) => assembled,
            Err(e) => {
                let _ = tokio::fs::remove_file(&tmp).await;
                return Err(e);
            }
        };
        if actual_size != total_size {
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err(format!(
                "Assembled size {} does not match the {} bytes received",
                actual_size, total_size
            ));
        }
        if let Err(e) = tokio::fs::rename(&tmp, &dest).await {
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err(format!("Failed to move assembled file into place: {}", e));
        }

        let content_type_final = content_type
            .map(|s| s.to_string())
            .or_else(|| detect_content_type(&normalized_path, &[]));

        let committed = commit_file_record(
            "local",
            &storage_path,
            &normalized_path,
            actual_size,
            Some(&etag),
            content_type_final.as_deref(),
            base,
            ctx,
        )
        .await;
        let meta = match committed {
            Ok(Committed::Swapped(meta) | Committed::Already(meta)) => meta,
            Ok(Committed::Conflict) => {
                let _ = tokio::fs::remove_file(&dest).await;
                return Err(completion_conflict(&normalized_path));
            }
            Err(e) => match staged_bytes_state(ctx, &normalized_path, &storage_path).await {
                StagedBytes::Committed(meta) => *meta,
                StagedBytes::Unreferenced => {
                    // Parts are kept so the completion can be retried.
                    let _ = tokio::fs::remove_file(&dest).await;
                    return Err(e);
                }
                StagedBytes::Unknown => return Err(e),
            },
        };
        let _ = tokio::fs::remove_dir_all(&upload_dir).await;

        tracing::debug!(
            "Completed multipart upload for {} to local storage",
            normalized_path
        );
        Ok(meta)
    }

    async fn abort_multipart_upload(
        &self,
        backend_upload_id: &str,
        path: &str,
        ctx: &FileStorageContext,
    ) -> Result<(), String> {
        let upload_dir = self.base_path.join(".uploads").join(backend_upload_id);
        match tokio::fs::remove_dir_all(&upload_dir).await {
            Ok(()) => {}
            // Already cleaned up by a completion or an earlier abort.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("Failed to clean up multipart upload: {}", e)),
        }
        // A completion that assembled but never committed leaves its
        // destination behind.
        let normalized_path = normalize_path(path)?;
        let dest = self.full_path(
            &multipart_rel_path(&normalized_path, backend_upload_id, LOCAL_NAME_MAX),
            ctx,
        );
        if let StagedBytes::Unreferenced =
            staged_bytes_state(ctx, &normalized_path, &dest.to_string_lossy()).await
        {
            let _ = tokio::fs::remove_file(&dest).await;
        }
        Ok(())
    }

    async fn write_blob_bytes(
        &self,
        storage_key: &str,
        content: &[u8],
        _content_type: Option<&str>,
        ctx: &FileStorageContext,
    ) -> Result<String, String> {
        let normalized = normalize_path(storage_key)?;
        validate_path_security(&normalized)?;

        let full_path = self.full_path(&normalized, ctx);
        if let Some(parent) = full_path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| format!("Failed to create blob directories: {}", e))?;
        }
        tokio::fs::write(&full_path, content)
            .await
            .map_err(|e| format!("Failed to write blob to storage: {}", e))?;

        Ok(full_path.to_string_lossy().to_string())
    }

    async fn read_blob_bytes(
        &self,
        storage_key: &str,
        ctx: &FileStorageContext,
    ) -> Result<Vec<u8>, String> {
        let normalized = normalize_path(storage_key)?;
        validate_path_security(&normalized)?;

        let full_path = self.full_path(&normalized, ctx);
        if !full_path.exists() {
            return Err(format!("Blob not found in storage: {}", normalized));
        }
        tokio::fs::read(&full_path)
            .await
            .map_err(|e| format!("Failed to read blob: {}", e))
    }

    async fn delete_blob_bytes(
        &self,
        storage_key: &str,
        ctx: &FileStorageContext,
    ) -> Result<(), String> {
        let normalized = normalize_path(storage_key)?;
        validate_path_security(&normalized)?;

        let full_path = self.full_path(&normalized, ctx);
        if full_path.exists() {
            tokio::fs::remove_file(&full_path)
                .await
                .map_err(|e| format!("Failed to delete blob: {}", e))?;
        }
        Ok(())
    }
}

// ============================================================================
// AWS S3 Storage
// ============================================================================

#[cfg(feature = "s3-storage")]
pub mod s3 {
    use super::*;
    use aws_config::BehaviorVersion;
    use aws_sdk_s3::Client as S3Client;
    use aws_sdk_s3::primitives::ByteStream;
    use aws_sdk_s3::types::{CompletedMultipartUpload, CompletedPart};

    /// AWS S3 storage for files
    pub struct S3FileStorage {
        client: S3Client,
        bucket: String,
        prefix: Option<String>,
    }

    impl S3FileStorage {
        /// Create a new S3 file storage
        pub fn new(client: S3Client, bucket: String, prefix: Option<String>) -> Self {
            Self {
                client,
                bucket,
                prefix,
            }
        }

        /// Create from Hot configuration
        pub async fn from_config(conf: &Val) -> Result<Self, String> {
            let file_conf = conf
                .get("file")
                .ok_or_else(|| "file configuration section not found".to_string())?;

            let aws_conf = file_conf
                .get("aws")
                .ok_or_else(|| "file.aws configuration section not found".to_string())?;

            let s3_conf = aws_conf
                .get("s3")
                .ok_or_else(|| "file.aws.s3 configuration section not found".to_string())?;

            let bucket = s3_conf.get_str_or_default("bucket", "");
            if bucket.is_empty() {
                return Err("file.aws.s3.bucket not configured".to_string());
            }

            let prefix_str = s3_conf.get_str_or_default("prefix", "");
            let prefix = if prefix_str.is_empty() {
                None
            } else {
                Some(prefix_str)
            };

            // Load AWS config
            let mut config_loader = aws_config::defaults(BehaviorVersion::latest());

            // Override region if specified in S3 config
            let s3_region = s3_conf.get_str_or_default("region", "");
            if !s3_region.is_empty() {
                config_loader = config_loader.region(aws_config::Region::new(s3_region));
            }

            // Check for custom file-specific AWS credentials from config
            // Prioritize: 1) Config values, 2) HOT_FILE_AWS_* env vars, 3) Standard AWS credential chain
            let access_key = aws_conf.get_str_or_default("access-key-id", "");
            let secret_key = aws_conf.get_str_or_default("secret-access-key", "");
            let session_token = aws_conf.get_str_or_default("session-token", "");
            if access_key.is_empty() != secret_key.is_empty()
                || (!session_token.is_empty() && access_key.is_empty())
            {
                return Err(
                    "AWS storage credentials require both access-key-id and secret-access-key"
                        .into(),
                );
            }

            if !access_key.is_empty() && !secret_key.is_empty() {
                use aws_runtime::env_config::file::{EnvConfigFileKind, EnvConfigFiles};
                use aws_sdk_s3::config::Credentials;
                let credentials = Credentials::new(
                    access_key,
                    secret_key,
                    (!session_token.is_empty()).then_some(session_token), // temporary credentials
                    None,                                                 // expiry
                    "hot-file-config",
                );
                config_loader = config_loader
                    .credentials_provider(credentials)
                    .profile_files(
                        EnvConfigFiles::builder()
                            .with_contents(EnvConfigFileKind::Config, "")
                            .with_contents(EnvConfigFileKind::Credentials, "")
                            .build(),
                    );
                tracing::debug!("Using file-specific AWS credentials from configuration");
            } else {
                tracing::debug!("Using standard AWS credential chain for file storage");
            }

            let config = config_loader.load().await;
            let client = S3Client::new(&config);

            Ok(Self::new(client, bucket, prefix))
        }

        /// Construct the S3 key for a file with multi-tenant isolation
        /// Structure: prefix/org_id/env_id/user_path
        fn s3_key(&self, path: &str, ctx: &FileStorageContext) -> String {
            let mut key_parts = Vec::new();

            // Add prefix if configured
            if let Some(prefix) = &self.prefix {
                key_parts.push(prefix.trim_end_matches('/').to_string());
            }

            // Add org_id for tenant isolation (without dashes for cleaner paths)
            key_parts.push(ctx.org_id.simple().to_string());

            // Add env_id if present for environment isolation
            if let Some(env_id) = ctx.env_id {
                key_parts.push(env_id.simple().to_string());
            } else {
                // Default to "default" environment if no env_id
                key_parts.push("default".to_string());
            }

            // Add the user's file path
            key_parts.push(path.to_string());

            key_parts.join("/")
        }

        /// Resolve a stored storage_path back to (bucket, key). Accepts the
        /// canonical `s3://bucket/key` form; a bare value is treated as a key
        /// in this storage's bucket. Returns None for empty paths.
        fn parse_storage_path(&self, storage_path: &str) -> Option<(String, String)> {
            if storage_path.is_empty() {
                return None;
            }
            match storage_path.strip_prefix("s3://") {
                Some(rest) => {
                    let (bucket, key) = rest.split_once('/')?;
                    Some((bucket.to_string(), key.to_string()))
                }
                None => Some((self.bucket.clone(), storage_path.to_string())),
            }
        }

        /// (bucket, key) of a record's bytes. The record's storage_path is
        /// authoritative; records that predate it fall back to the
        /// deterministic key.
        fn record_location(
            &self,
            record: &FileRecord,
            normalized_path: &str,
            ctx: &FileStorageContext,
        ) -> (String, String) {
            if record.storage_backend == "s3"
                && let Some(location) = self.parse_storage_path(&record.storage_path)
            {
                return location;
            }
            (self.bucket.clone(), self.s3_key(normalized_path, ctx))
        }

        /// Best-effort delete of a location no record references.
        async fn delete_location_best_effort(&self, storage_path: &str) {
            if let Some((bucket, key)) = self.parse_storage_path(storage_path)
                && let Err(e) = self
                    .client
                    .delete_object()
                    .bucket(&bucket)
                    .key(&key)
                    .send()
                    .await
            {
                tracing::warn!("Failed to delete unreferenced S3 object: {}", e);
            }
        }

        /// Resolve a backend upload id to (object key, S3 upload id).
        ///
        /// New uploads complete into a unique per-upload key, encoded in the
        /// backend id as `hot1:{nonce}:{s3UploadId}`, so completion never
        /// overwrites the live object before the record swap commits it.
        /// Bare ids from uploads started before this format complete at the
        /// deterministic key, as they always did.
        pub(super) fn multipart_target(
            &self,
            backend_upload_id: &str,
            normalized_path: &str,
            ctx: &FileStorageContext,
        ) -> (String, String) {
            if let Some((nonce, upload_id)) = backend_upload_id
                .strip_prefix(MULTIPART_ID_PREFIX)
                .and_then(|rest| rest.split_once(':'))
            {
                let rel = multipart_rel_path(normalized_path, nonce, KEY_NAME_MAX);
                (self.s3_key(&rel, ctx), upload_id.to_string())
            } else {
                (
                    self.s3_key(normalized_path, ctx),
                    backend_upload_id.to_string(),
                )
            }
        }
    }

    const MULTIPART_ID_PREFIX: &str = "hot1:";

    #[async_trait]
    impl FileStorage for S3FileStorage {
        async fn write_file(
            &self,
            path: &str,
            content: &[u8],
            content_type: Option<&str>,
            ctx: &FileStorageContext,
        ) -> Result<FileMetadata, String> {
            let normalized_path = normalize_path(path)?;
            validate_path_security(&normalized_path)?;
            check_write_quota(ctx, content.len() as i64).await?;
            ensure_path_writable("s3", &normalized_path, ctx).await?;

            let etag = compute_md5(content);
            let content_type_final = content_type
                .map(|s| s.to_string())
                .or_else(|| detect_content_type(&normalized_path, content));

            // Stage at a unique key; the record swap below is the commit
            // point. Concurrent writers never share an object, so the
            // committed record's size and etag always describe the object it
            // references, and a failed write leaves the current file intact.
            let key = self.s3_key(&unique_write_rel_path(&normalized_path, KEY_NAME_MAX), ctx);
            let mut put_request = self
                .client
                .put_object()
                .bucket(&self.bucket)
                .key(&key)
                .body(ByteStream::from(content.to_vec()));
            if let Some(ct) = &content_type_final {
                put_request = put_request.content_type(ct);
            }

            let response = match put_request.send().await {
                Ok(resp) => resp,
                Err(e) => {
                    let error_msg = format!(
                        "Failed to upload file to S3 (bucket: {}, key: {}, path: {}): {}",
                        self.bucket, key, normalized_path, e
                    );
                    tracing::error!("{}", error_msg);
                    if let Some(raw) = e.raw_response() {
                        tracing::error!(
                            "S3 file upload error details - status: {:?}, headers: {:?}",
                            raw.status(),
                            raw.headers()
                        );
                    }
                    return Err(error_msg);
                }
            };

            let s3_etag = response
                .e_tag()
                .unwrap_or(&etag)
                .trim_matches('"')
                .to_string();
            let storage_path = format!("s3://{}/{}", self.bucket, key);
            if let Err(e) = register_staged(ctx, "s3", &storage_path).await {
                self.delete_location_best_effort(&storage_path).await;
                return Err(e);
            }

            let committed = commit_file_record(
                "s3",
                &storage_path,
                &normalized_path,
                content.len() as i64,
                Some(&s3_etag),
                content_type_final.as_deref(),
                &CommitBase::Any,
                ctx,
            )
            .await
            .and_then(|committed| committed.into_write(&normalized_path));
            match committed {
                Ok(meta) => {
                    tracing::debug!("Wrote file {} to S3: {}", normalized_path, storage_path);
                    Ok(meta)
                }
                Err(e) => match staged_bytes_state(ctx, &normalized_path, &storage_path).await {
                    StagedBytes::Committed(meta) => Ok(*meta),
                    StagedBytes::Unreferenced => {
                        self.delete_location_best_effort(&storage_path).await;
                        Err(e)
                    }
                    StagedBytes::Unknown => Err(e),
                },
            }
        }

        async fn write_file_if(
            &self,
            path: &str,
            content: &[u8],
            content_type: Option<&str>,
            expected_etag: Option<&str>,
            ctx: &FileStorageContext,
        ) -> Result<Option<FileMetadata>, String> {
            let normalized_path = normalize_path(path)?;
            validate_path_security(&normalized_path)?;
            check_write_quota(ctx, content.len() as i64).await?;
            ensure_path_writable("s3", &normalized_path, ctx).await?;

            let etag = compute_md5(content);
            let content_type_final = content_type
                .map(|s| s.to_string())
                .or_else(|| detect_content_type(&normalized_path, content));

            // Stage the bytes at a unique key first; the record swap below
            // is the commit point. A failed upload or a lost race leaves
            // only an unreferenced staged object, never a record pointing
            // at missing or stale bytes.
            let key = self.s3_key(&cas_blob_rel_path(&normalized_path, KEY_NAME_MAX), ctx);
            let storage_path = format!("s3://{}/{}", self.bucket, key);

            let byte_stream = ByteStream::from(content.to_vec());
            let mut put_request = self
                .client
                .put_object()
                .bucket(&self.bucket)
                .key(&key)
                .body(byte_stream);
            if let Some(ct) = &content_type_final {
                put_request = put_request.content_type(ct);
            }
            put_request.send().await.map_err(|e| {
                format!(
                    "Failed to upload file to S3 (bucket: {}, key: {}, path: {}): {}",
                    self.bucket, key, normalized_path, e
                )
            })?;
            if let Err(e) = register_staged(ctx, "s3", &storage_path).await {
                self.delete_location_best_effort(&storage_path).await;
                return Err(e);
            }

            match cas_file_record(
                "s3",
                &storage_path,
                &normalized_path,
                content.len() as i64,
                &etag,
                content_type_final.as_deref(),
                expected_etag,
                ctx,
            )
            .await
            {
                Ok(Some(meta)) => Ok(Some(meta)),
                Ok(None) => {
                    self.delete_location_best_effort(&storage_path).await;
                    Ok(None)
                }
                Err(e) => match staged_bytes_state(ctx, &normalized_path, &storage_path).await {
                    StagedBytes::Committed(meta) => Ok(Some(*meta)),
                    StagedBytes::Unreferenced => {
                        self.delete_location_best_effort(&storage_path).await;
                        Err(e)
                    }
                    StagedBytes::Unknown => Err(e),
                },
            }
        }
        async fn read_file(&self, path: &str, ctx: &FileStorageContext) -> Result<Vec<u8>, String> {
            let normalized_path = normalize_path(path)?;
            validate_path_security(&normalized_path)?;

            // The database record authorizes the read and names the object.
            let (_, data) = fetch_current(ctx, &normalized_path, |record| {
                let (bucket, key) = self.record_location(record, &normalized_path, ctx);
                let normalized_path = normalized_path.clone();
                Box::pin(async move {
                    let result = self.client.get_object().bucket(&bucket).key(&key).send().await;
                    let response = match result {
                        Ok(resp) => resp,
                        Err(e) if e.as_service_error().is_some_and(|se| se.is_no_such_key()) => {
                            return Ok(None);
                        }
                        Err(e) => {
                            let error_msg = format!(
                                "Failed to retrieve file from S3 (bucket: {}, key: {}, path: {}): {}",
                                bucket, key, normalized_path, e
                            );
                            tracing::error!("{}", error_msg);
                            if let Some(raw) = e.raw_response() {
                                tracing::error!(
                                    "S3 file download error details - status: {:?}, headers: {:?}",
                                    raw.status(),
                                    raw.headers()
                                );
                            }
                            return Err(error_msg);
                        }
                    };
                    let data = response.body.collect().await.map_err(|e| {
                        let error_msg = format!(
                            "Failed to read S3 response body for file {} (bucket: {}, key: {}): {}",
                            normalized_path, bucket, key, e
                        );
                        tracing::error!("{}", error_msg);
                        error_msg
                    })?;
                    Ok(Some(data.into_bytes().to_vec()))
                })
            })
            .await?;

            tracing::info!("Read file {} from S3", normalized_path);
            Ok(data)
        }

        async fn open_file_stream(
            &self,
            path: &str,
            ctx: &FileStorageContext,
        ) -> Result<Option<FileReadStream>, String> {
            let normalized_path = normalize_path(path)?;
            validate_path_security(&normalized_path)?;

            let (record, response) = fetch_current(ctx, &normalized_path, |record| {
                let (bucket, key) = self.record_location(record, &normalized_path, ctx);
                let normalized_path = normalized_path.clone();
                Box::pin(async move {
                    match self
                        .client
                        .get_object()
                        .bucket(&bucket)
                        .key(&key)
                        .send()
                        .await
                    {
                        Ok(resp) => Ok(Some(resp)),
                        Err(e) if e.as_service_error().is_some_and(|se| se.is_no_such_key()) => {
                            Ok(None)
                        }
                        Err(e) => Err(format!(
                            "Failed to retrieve file from S3 (bucket: {}, key: {}, path: {}): {}",
                            bucket, key, normalized_path, e
                        )),
                    }
                })
            })
            .await?;

            Ok(Some(FileReadStream {
                metadata: FileMetadata::from_file_record(record),
                reader: Box::pin(response.body.into_async_read()),
            }))
        }

        async fn delete_file(&self, path: &str, ctx: &FileStorageContext) -> Result<bool, String> {
            // Normalize and validate path
            let normalized_path = normalize_path(path)?;
            validate_path_security(&normalized_path)?;

            let Some(record) = deactivate_current(ctx, &normalized_path).await? else {
                return Ok(false);
            };

            // Deactivating retired the recorded object for collection. A
            // record that predates storage paths keeps its object at the
            // path's deterministic key.
            if !record.storage_path.is_empty() {
                tracing::debug!("Deleted file {} from S3", normalized_path);
                return Ok(true);
            }
            let (bucket, key) = (self.bucket.clone(), self.s3_key(&normalized_path, ctx));

            let result = self
                .client
                .delete_object()
                .bucket(&bucket)
                .key(&key)
                .send()
                .await;

            match result {
                Ok(_) => {
                    tracing::debug!("Deleted file {} from S3", normalized_path);
                    Ok(true) // File existed and was deleted
                }
                Err(e) => {
                    // Extract detailed error information
                    let error_msg = format!(
                        "Failed to delete file from S3 (bucket: {}, key: {}, path: {}): {}",
                        bucket, key, normalized_path, e
                    );
                    tracing::error!("{}", error_msg);

                    // Log additional context if available
                    if let Some(raw) = e.raw_response() {
                        tracing::error!(
                            "S3 file delete error details - status: {:?}, headers: {:?}",
                            raw.status(),
                            raw.headers()
                        );
                    }

                    Err(error_msg)
                }
            }
        }

        async fn delete_storage_object(&self, storage_path: &str) -> Result<(), String> {
            let Some((bucket, key)) = self
                .parse_storage_path(storage_path)
                .filter(|(bucket, _)| *bucket == self.bucket)
            else {
                tracing::warn!(
                    storage_path,
                    "Not deleting a retired file object outside this bucket"
                );
                return Ok(());
            };
            self.client
                .delete_object()
                .bucket(&bucket)
                .key(&key)
                .send()
                .await
                .map(|_| ())
                .map_err(|e| format!("Failed to delete S3 object {}: {}", key, e))
        }

        async fn file_exists(&self, path: &str, ctx: &FileStorageContext) -> Result<bool, String> {
            let normalized_path = normalize_path(path)?;
            validate_path_security(&normalized_path)?;

            // Check database for active file
            match get_file_by_path(&ctx.db, &normalized_path, ctx.org_id, ctx.env_id).await {
                Ok(_) => Ok(true),
                Err(_) => Ok(false),
            }
        }

        async fn get_file_metadata(
            &self,
            path: &str,
            ctx: &FileStorageContext,
        ) -> Result<FileMetadata, String> {
            let normalized_path = normalize_path(path)?;
            validate_path_security(&normalized_path)?;

            let record =
                get_file_by_path(&ctx.db, &normalized_path, ctx.org_id, ctx.env_id).await?;
            Ok(FileMetadata::from_file_record(record))
        }

        async fn list_files(
            &self,
            prefix: &str,
            ctx: &FileStorageContext,
        ) -> Result<Vec<FileMetadata>, String> {
            use crate::db::file::list_files_by_prefix;

            let normalized_prefix = normalize_path(prefix).unwrap_or_else(|_| prefix.to_string());

            let records =
                list_files_by_prefix(&ctx.db, &normalized_prefix, ctx.org_id, ctx.env_id).await?;
            Ok(records
                .into_iter()
                .map(FileMetadata::from_file_record)
                .collect())
        }

        async fn list_files_bounded(
            &self,
            prefix: &str,
            limit: usize,
            ctx: &FileStorageContext,
        ) -> Result<Vec<FileMetadata>, String> {
            use crate::db::file::list_files_by_prefix_bounded;

            let normalized_prefix = normalize_path(prefix).unwrap_or_else(|_| prefix.to_string());
            let records = list_files_by_prefix_bounded(
                &ctx.db,
                &normalized_prefix,
                ctx.org_id,
                ctx.env_id,
                limit,
            )
            .await?;
            Ok(records
                .into_iter()
                .map(FileMetadata::from_file_record)
                .collect())
        }

        fn storage_type(&self) -> &str {
            "s3"
        }

        async fn initiate_multipart_upload(
            &self,
            path: &str,
            content_type: Option<&str>,
            ctx: &FileStorageContext,
        ) -> Result<String, String> {
            let normalized_path = normalize_path(path)?;
            validate_path_security(&normalized_path)?;
            ensure_path_writable("s3", &normalized_path, ctx).await?;

            let nonce = Uuid::new_v4().simple().to_string();
            let key = self.s3_key(
                &multipart_rel_path(&normalized_path, &nonce, KEY_NAME_MAX),
                ctx,
            );

            let mut req = self
                .client
                .create_multipart_upload()
                .bucket(&self.bucket)
                .key(&key);

            if let Some(ct) = content_type {
                req = req.content_type(ct);
            }

            let resp = req
                .send()
                .await
                .map_err(|e| format!("Failed to initiate multipart upload: {}", e))?;

            resp.upload_id()
                .map(|id| format!("{MULTIPART_ID_PREFIX}{nonce}:{id}"))
                .ok_or_else(|| "S3 did not return an upload ID".to_string())
        }

        async fn upload_part(
            &self,
            backend_upload_id: &str,
            path: &str,
            part_number: i32,
            content: &[u8],
            ctx: &FileStorageContext,
        ) -> Result<String, String> {
            validate_part_number(part_number)?;
            let normalized_path = normalize_path(path)?;
            let (key, upload_id) = self.multipart_target(backend_upload_id, &normalized_path, ctx);

            let resp = self
                .client
                .upload_part()
                .bucket(&self.bucket)
                .key(&key)
                .upload_id(upload_id)
                .part_number(part_number)
                .body(ByteStream::from(content.to_vec()))
                .send()
                .await
                .map_err(|e| format!("Failed to upload part {}: {}", part_number, e))?;

            resp.e_tag()
                .map(|t| t.trim_matches('"').to_string())
                .ok_or_else(|| format!("S3 did not return ETag for part {}", part_number))
        }

        async fn complete_multipart_upload(
            &self,
            backend_upload_id: &str,
            path: &str,
            parts: &[(i32, String)],
            total_size: i64,
            content_type: Option<&str>,
            base: &CommitBase,
            ctx: &FileStorageContext,
        ) -> Result<FileMetadata, String> {
            use aws_sdk_s3::error::ProvideErrorMetadata;

            let normalized_path = normalize_path(path)?;
            validate_path_security(&normalized_path)?;
            ensure_path_writable("s3", &normalized_path, ctx).await?;
            let (key, upload_id) = self.multipart_target(backend_upload_id, &normalized_path, ctx);
            let storage_path = format!("s3://{}/{}", self.bucket, key);
            // Uploads started before the encoded id format complete into the
            // live key itself, so they keep last-writer-wins semantics.
            let legacy = !backend_upload_id.starts_with(MULTIPART_ID_PREFIX);
            let base = if legacy { &CommitBase::Any } else { base };

            // A retried completion whose earlier attempt already committed.
            if !legacy
                && let StagedBytes::Committed(meta) =
                    staged_bytes_state(ctx, &normalized_path, &storage_path).await
            {
                return Ok(*meta);
            }

            let completed_parts: Vec<CompletedPart> = parts
                .iter()
                .map(|(num, etag)| {
                    CompletedPart::builder()
                        .part_number(*num)
                        .e_tag(etag)
                        .build()
                })
                .collect();

            let completed_upload = CompletedMultipartUpload::builder()
                .set_parts(Some(completed_parts))
                .build();

            let completed = self
                .client
                .complete_multipart_upload()
                .bucket(&self.bucket)
                .key(&key)
                .upload_id(upload_id)
                .multipart_upload(completed_upload)
                .send()
                .await;
            let etag = match completed {
                Ok(resp) => resp.e_tag().map(|t| t.trim_matches('"').to_string()),
                // An earlier attempt completed the upload but never committed
                // it (lost response, crash, or failed commit): its object is
                // still at this upload's key.
                Err(e)
                    if !legacy
                        && e.as_service_error().and_then(|se| se.code())
                            == Some("NoSuchUpload") =>
                {
                    match self
                        .client
                        .head_object()
                        .bucket(&self.bucket)
                        .key(&key)
                        .send()
                        .await
                    {
                        Ok(head) if head.content_length() == Some(total_size) => {
                            head.e_tag().map(|t| t.trim_matches('"').to_string())
                        }
                        _ => return Err(format!("Failed to complete multipart upload: {}", e)),
                    }
                }
                Err(e) => return Err(format!("Failed to complete multipart upload: {}", e)),
            };

            let content_type_final = content_type
                .map(|s| s.to_string())
                .or_else(|| detect_content_type(&normalized_path, &[]));

            // The completed object lives at its own key; the record swap is
            // the commit point.
            let committed = commit_file_record(
                "s3",
                &storage_path,
                &normalized_path,
                total_size,
                etag.as_deref(),
                content_type_final.as_deref(),
                base,
                ctx,
            )
            .await;
            let meta = match committed {
                Ok(Committed::Swapped(meta) | Committed::Already(meta)) => meta,
                Ok(Committed::Conflict) => {
                    self.delete_location_best_effort(&storage_path).await;
                    return Err(completion_conflict(&normalized_path));
                }
                // The completed object is kept: a retry recovers it, and an
                // abort deletes it.
                Err(e) => match staged_bytes_state(ctx, &normalized_path, &storage_path).await {
                    StagedBytes::Committed(meta) => *meta,
                    StagedBytes::Unreferenced | StagedBytes::Unknown => return Err(e),
                },
            };

            tracing::debug!(
                "Completed multipart upload for {} to S3: {}",
                normalized_path,
                storage_path
            );
            Ok(meta)
        }

        async fn abort_multipart_upload(
            &self,
            backend_upload_id: &str,
            path: &str,
            ctx: &FileStorageContext,
        ) -> Result<(), String> {
            let normalized_path = normalize_path(path)?;
            let (key, upload_id) = self.multipart_target(backend_upload_id, &normalized_path, ctx);

            match self
                .client
                .abort_multipart_upload()
                .bucket(&self.bucket)
                .key(&key)
                .upload_id(upload_id)
                .send()
                .await
            {
                Ok(_) => {}
                // Already completed or aborted.
                Err(e)
                    if e.as_service_error()
                        .is_some_and(|se| se.is_no_such_upload()) => {}
                Err(e) => return Err(format!("Failed to abort multipart upload: {}", e)),
            }

            // A completion that finished on S3 but never committed leaves its
            // object at this upload's own key. Legacy ids target the live key,
            // which is never removed here.
            if backend_upload_id.starts_with(MULTIPART_ID_PREFIX) {
                let storage_path = format!("s3://{}/{}", self.bucket, key);
                if let StagedBytes::Unreferenced =
                    staged_bytes_state(ctx, &normalized_path, &storage_path).await
                {
                    self.delete_location_best_effort(&storage_path).await;
                }
            }
            Ok(())
        }

        async fn write_blob_bytes(
            &self,
            storage_key: &str,
            content: &[u8],
            content_type: Option<&str>,
            ctx: &FileStorageContext,
        ) -> Result<String, String> {
            let normalized = normalize_path(storage_key)?;
            validate_path_security(&normalized)?;

            let key = self.s3_key(&normalized, ctx);
            let mut put_request = self
                .client
                .put_object()
                .bucket(&self.bucket)
                .key(&key)
                .body(ByteStream::from(content.to_vec()));
            if let Some(ct) = content_type {
                put_request = put_request.content_type(ct);
            }

            put_request.send().await.map_err(|e| {
                format!(
                    "Failed to upload blob to S3 (bucket: {}, key: {}): {}",
                    self.bucket, key, e
                )
            })?;

            Ok(format!("s3://{}/{}", self.bucket, key))
        }

        async fn read_blob_bytes(
            &self,
            storage_key: &str,
            ctx: &FileStorageContext,
        ) -> Result<Vec<u8>, String> {
            let normalized = normalize_path(storage_key)?;
            validate_path_security(&normalized)?;

            let key = self.s3_key(&normalized, ctx);
            let response = self
                .client
                .get_object()
                .bucket(&self.bucket)
                .key(&key)
                .send()
                .await
                .map_err(|e| {
                    format!(
                        "Failed to retrieve blob from S3 (bucket: {}, key: {}): {}",
                        self.bucket, key, e
                    )
                })?;

            Ok(response
                .body
                .collect()
                .await
                .map_err(|e| format!("Failed to read S3 blob body (key: {}): {}", key, e))?
                .into_bytes()
                .to_vec())
        }

        async fn delete_blob_bytes(
            &self,
            storage_key: &str,
            ctx: &FileStorageContext,
        ) -> Result<(), String> {
            let normalized = normalize_path(storage_key)?;
            validate_path_security(&normalized)?;

            let key = self.s3_key(&normalized, ctx);
            self.client
                .delete_object()
                .bucket(&self.bucket)
                .key(&key)
                .send()
                .await
                .map_err(|e| {
                    format!(
                        "Failed to delete blob from S3 (bucket: {}, key: {}): {}",
                        self.bucket, key, e
                    )
                })?;
            Ok(())
        }
    }
}

// ============================================================================
// Google Cloud Storage
// ============================================================================

#[cfg(feature = "gcs-storage")]
pub mod gcs {
    //! GCS file storage.
    //!
    //! Layout under the configured prefix:
    //!
    //! - `{org}/{env}/{path}.v-{uuid}`: committed file bytes. Every write
    //!   (plain, conditional, or multipart) lands at a fresh key and the
    //!   database record swap is the commit point.
    //! - `{org}/{env}/{path}.cas-{uuid}`: conditional-write bytes.
    //! - `{org}/{env}/{path}.mp-{upload}`: completed multipart uploads.
    //! - `{org}/{env}/{blob-key}`: content-addressed internal blobs.
    //! - `.uploads/{org}/{env}/{upload}/...`: multipart staging (parts and
    //!   intermediate compositions). Nothing committed ever lives here, so a
    //!   bucket lifecycle rule scoped to this prefix is safe.
    //!
    //! Database metadata stays authoritative for authorization and listings;
    //! the bucket is never listed outside the staging prefix.

    use super::*;
    use crate::gcs::{BACKEND_ID, GcsClient, GcsConfig, GcsError, plan_compose_levels};
    use bytes::Bytes;

    const STAGING_DIR: &str = ".uploads";
    const META_UPLOAD_ID: &str = "hot-upload-id";
    const META_MANIFEST: &str = "hot-upload-manifest";

    /// Google Cloud Storage backend for files and internal blobs.
    pub struct GcsFileStorage {
        client: GcsClient,
    }

    impl GcsFileStorage {
        pub fn new(client: GcsClient) -> Self {
            Self { client }
        }

        /// Create from Hot configuration (`file.gcs.*` / `HOT_FILE_GCS_*`).
        pub async fn from_config(conf: &Val) -> Result<Self, String> {
            let config = GcsConfig::from_conf(conf, "file")?;
            let client = GcsClient::connect(&config).await?;
            tracing::info!(bucket = %config.bucket, "Using GCS file storage");
            Ok(Self::new(client))
        }

        fn env_segment(ctx: &FileStorageContext) -> String {
            ctx.env_id
                .map(|id| id.simple().to_string())
                .unwrap_or_else(|| "default".to_string())
        }

        /// Tenant-isolated object key: prefix/org/env/rel.
        fn object_key(&self, rel: &str, ctx: &FileStorageContext) -> String {
            let org = ctx.org_id.simple().to_string();
            let env = Self::env_segment(ctx);
            self.client.key([org.as_str(), env.as_str(), rel])
        }

        /// Staging prefix for one multipart upload (trailing slash included
        /// so listing never matches a sibling upload id).
        fn staging_prefix(&self, backend_upload_id: &str, ctx: &FileStorageContext) -> String {
            let org = ctx.org_id.simple().to_string();
            let env = Self::env_segment(ctx);
            format!(
                "{}/",
                self.client
                    .key([STAGING_DIR, org.as_str(), env.as_str(), backend_upload_id])
            )
        }

        /// Object key for an authorized record. Records from another backend
        /// or another bucket are refused rather than guessed at.
        fn record_key(&self, record: &FileRecord) -> Result<String, String> {
            ensure_record_backend(&record.storage_backend, BACKEND_ID, &record.path)?;
            if record.storage_path.is_empty() {
                return Err(format!("File {} has no stored location", record.path));
            }
            Ok(self.client.key_from_uri(&record.storage_path)?)
        }

        /// Refuse writes over a record owned by another backend before any
        /// bytes are uploaded.
        async fn ensure_writable(
            &self,
            normalized_path: &str,
            ctx: &FileStorageContext,
        ) -> Result<(), String> {
            ensure_path_writable(BACKEND_ID, normalized_path, ctx).await
        }

        /// Create `key` (which no other writer can share). A precondition
        /// failure means an earlier attempt of this same request already
        /// created it (lost response, retried); accept it when the size
        /// matches.
        async fn put_unique(
            &self,
            key: &str,
            data: Bytes,
            content_type: Option<&str>,
            metadata: &[(&str, &str)],
        ) -> Result<(String, i64), GcsError> {
            let len = data.len() as i64;
            match self
                .client
                .put_bytes(key, data, content_type, Some(0), metadata)
                .await
            {
                Ok(obj) => Ok((obj.name, obj.generation)),
                Err(e) if e.is_precondition_failed() => match self.client.head(key).await? {
                    Some(obj) if obj.size == len => Ok((obj.name, obj.generation)),
                    _ => Err(e),
                },
                Err(e) => Err(e),
            }
        }

        /// Remove a finished upload's staging objects (best-effort; the
        /// staging lifecycle rule is the backstop).
        async fn cleanup_staging(&self, staging: &str) {
            if let Err(e) = self.client.delete_prefix(staging).await {
                tracing::warn!(error = %e, "Failed to clean up multipart staging objects");
            }
        }

        async fn delete_key_best_effort(&self, key: &str, generation: Option<i64>) {
            if let Err(e) = self.client.delete(key, generation).await {
                tracing::warn!(error = %e, "Failed to delete unreferenced GCS object");
            }
        }

        /// Parse a part token returned by `upload_part`: `{attempt}.{generation}`.
        fn parse_part_token(token: &str) -> Option<(&str, i64)> {
            let (attempt, generation) = token.split_once('.')?;
            if attempt.len() != 32 || !attempt.chars().all(|c| c.is_ascii_hexdigit()) {
                return None;
            }
            let generation = generation.parse::<i64>().ok().filter(|g| *g > 0)?;
            Some((attempt, generation))
        }

        fn part_key(staging: &str, part_number: i32, attempt: &str) -> String {
            format!("{staging}part-{part_number:05}-{attempt}")
        }

        /// Stable digest of the completion manifest, recorded on the final
        /// object so a retried completion can recognize its own result.
        fn manifest_digest(parts: &[(i32, String)]) -> String {
            let mut hasher = blake3::Hasher::new();
            for (number, token) in parts {
                hasher.update(format!("{number}:{token}\n").as_bytes());
            }
            hasher.finalize().to_hex()[..32].to_string()
        }

        /// Compose generation-pinned `sources` into `dest`, staging through
        /// intermediate objects when there are more than 32.
        async fn compose_staged(
            &self,
            staging: &str,
            dest: &str,
            mut sources: Vec<(String, i64)>,
            content_type: Option<&str>,
            metadata: &[(&str, &str)],
        ) -> Result<(String, i64), GcsError> {
            let attempt = Uuid::new_v4().simple().to_string();
            for (level, groups) in plan_compose_levels(sources.len()).into_iter().enumerate() {
                let mut offset = 0;
                let mut jobs = Vec::with_capacity(groups.len());
                for (index, size) in groups.into_iter().enumerate() {
                    let group = sources[offset..offset + size].to_vec();
                    offset += size;
                    let key = format!("{staging}compose-{attempt}-{level}-{index:05}");
                    jobs.push(async move {
                        match self.client.compose(&key, &group, None, Some(0), &[]).await {
                            Ok(obj) => Ok((obj.name, obj.generation)),
                            // The key is unique to this attempt, so an
                            // existing object is our own earlier try whose
                            // response was lost before the SDK retried.
                            Err(e) if e.is_precondition_failed() => {
                                match self.client.head(&key).await? {
                                    Some(obj) => Ok((obj.name, obj.generation)),
                                    None => Err(e),
                                }
                            }
                            Err(e) => Err(e),
                        }
                    });
                }
                sources = futures::future::try_join_all(jobs).await?;
            }
            let obj = self
                .client
                .compose(dest, &sources, content_type, Some(0), metadata)
                .await?;
            Ok((obj.name, obj.generation))
        }
    }

    #[async_trait]
    impl FileStorage for GcsFileStorage {
        async fn write_file(
            &self,
            path: &str,
            content: &[u8],
            content_type: Option<&str>,
            ctx: &FileStorageContext,
        ) -> Result<FileMetadata, String> {
            let normalized_path = normalize_path(path)?;
            validate_path_security(&normalized_path)?;
            check_write_quota(ctx, content.len() as i64).await?;
            self.ensure_writable(&normalized_path, ctx).await?;

            let etag = compute_md5(content);
            let content_type_final = content_type
                .map(|s| s.to_string())
                .or_else(|| detect_content_type(&normalized_path, content));

            let key = self.object_key(&unique_write_rel_path(&normalized_path, KEY_NAME_MAX), ctx);
            let (_, generation) = self
                .put_unique(
                    &key,
                    Bytes::copy_from_slice(content),
                    content_type_final.as_deref(),
                    &[],
                )
                .await
                .map_err(|e| format!("Failed to upload file {} to GCS: {}", normalized_path, e))?;
            let storage_path = self.client.uri(&key);
            if let Err(e) = register_staged(ctx, BACKEND_ID, &storage_path).await {
                self.delete_key_best_effort(&key, Some(generation)).await;
                return Err(e);
            }

            let committed = commit_file_record(
                BACKEND_ID,
                &storage_path,
                &normalized_path,
                content.len() as i64,
                Some(&etag),
                content_type_final.as_deref(),
                &CommitBase::Any,
                ctx,
            )
            .await
            .and_then(|committed| committed.into_write(&normalized_path));
            match committed {
                Ok(meta) => {
                    tracing::debug!("Wrote file {} to GCS", normalized_path);
                    Ok(meta)
                }
                Err(e) => match staged_bytes_state(ctx, &normalized_path, &storage_path).await {
                    StagedBytes::Committed(meta) => Ok(*meta),
                    StagedBytes::Unreferenced => {
                        self.delete_key_best_effort(&key, Some(generation)).await;
                        Err(e)
                    }
                    StagedBytes::Unknown => Err(e),
                },
            }
        }

        async fn write_file_if(
            &self,
            path: &str,
            content: &[u8],
            content_type: Option<&str>,
            expected_etag: Option<&str>,
            ctx: &FileStorageContext,
        ) -> Result<Option<FileMetadata>, String> {
            let normalized_path = normalize_path(path)?;
            validate_path_security(&normalized_path)?;
            check_write_quota(ctx, content.len() as i64).await?;
            self.ensure_writable(&normalized_path, ctx).await?;

            let etag = compute_md5(content);
            let content_type_final = content_type
                .map(|s| s.to_string())
                .or_else(|| detect_content_type(&normalized_path, content));

            // Stage at a unique key; the record swap below is the commit
            // point. Hot's application ETag (MD5) is independent of the GCS
            // generation, which only guards this object's own lifecycle.
            let key = self.object_key(&cas_blob_rel_path(&normalized_path, KEY_NAME_MAX), ctx);
            let (_, generation) = self
                .put_unique(
                    &key,
                    Bytes::copy_from_slice(content),
                    content_type_final.as_deref(),
                    &[],
                )
                .await
                .map_err(|e| format!("Failed to upload file {} to GCS: {}", normalized_path, e))?;
            let storage_path = self.client.uri(&key);
            if let Err(e) = register_staged(ctx, BACKEND_ID, &storage_path).await {
                self.delete_key_best_effort(&key, Some(generation)).await;
                return Err(e);
            }

            match cas_file_record(
                BACKEND_ID,
                &storage_path,
                &normalized_path,
                content.len() as i64,
                &etag,
                content_type_final.as_deref(),
                expected_etag,
                ctx,
            )
            .await
            {
                Ok(Some(meta)) => Ok(Some(meta)),
                // Only this attempt's generation is ever removed.
                Ok(None) => {
                    self.delete_key_best_effort(&key, Some(generation)).await;
                    Ok(None)
                }
                Err(e) => match staged_bytes_state(ctx, &normalized_path, &storage_path).await {
                    StagedBytes::Committed(meta) => Ok(Some(*meta)),
                    StagedBytes::Unreferenced => {
                        self.delete_key_best_effort(&key, Some(generation)).await;
                        Err(e)
                    }
                    StagedBytes::Unknown => Err(e),
                },
            }
        }

        async fn read_file(&self, path: &str, ctx: &FileStorageContext) -> Result<Vec<u8>, String> {
            let normalized_path = normalize_path(path)?;
            validate_path_security(&normalized_path)?;
            let (_, bytes) = fetch_current(ctx, &normalized_path, |record| {
                let key = self.record_key(record);
                let normalized_path = normalized_path.clone();
                Box::pin(async move {
                    match self.client.get_bytes(&key?, None).await {
                        Ok(bytes) => Ok(Some(bytes)),
                        Err(GcsError::NotFound(_)) => Ok(None),
                        Err(e) => Err(format!(
                            "Failed to read file {} from GCS: {}",
                            normalized_path, e
                        )),
                    }
                })
            })
            .await?;
            Ok(bytes)
        }

        async fn open_file_stream(
            &self,
            path: &str,
            ctx: &FileStorageContext,
        ) -> Result<Option<FileReadStream>, String> {
            let normalized_path = normalize_path(path)?;
            validate_path_security(&normalized_path)?;
            let (record, reader) = fetch_current(ctx, &normalized_path, |record| {
                let key = self.record_key(record);
                let normalized_path = normalized_path.clone();
                Box::pin(async move {
                    match self.client.open_reader(&key?).await {
                        Ok((reader, _size)) => Ok(Some(reader)),
                        Err(GcsError::NotFound(_)) => Ok(None),
                        Err(e) => Err(format!(
                            "Failed to open file {} from GCS: {}",
                            normalized_path, e
                        )),
                    }
                })
            })
            .await?;
            Ok(Some(FileReadStream {
                metadata: FileMetadata::from_file_record(record),
                reader,
            }))
        }

        async fn delete_file(&self, path: &str, ctx: &FileStorageContext) -> Result<bool, String> {
            let normalized_path = normalize_path(path)?;
            validate_path_security(&normalized_path)?;
            // Deactivating retired the recorded object for collection.
            if deactivate_current(ctx, &normalized_path).await?.is_none() {
                return Ok(false);
            }
            tracing::debug!("Deleted file {} from GCS", normalized_path);
            Ok(true)
        }

        async fn delete_storage_object(&self, storage_path: &str) -> Result<(), String> {
            let key = match self.client.key_from_uri(storage_path) {
                Ok(key) => key,
                Err(e) => {
                    tracing::warn!(error = %e, "Not deleting a retired file object outside this bucket");
                    return Ok(());
                }
            };
            // An object that is already gone is reported as deleted.
            self.client
                .delete(&key, None)
                .await
                .map(|_| ())
                .map_err(|e| format!("Failed to delete GCS object {}: {}", key, e))
        }

        async fn file_exists(&self, path: &str, ctx: &FileStorageContext) -> Result<bool, String> {
            let normalized_path = normalize_path(path)?;
            validate_path_security(&normalized_path)?;
            Ok(
                get_file_by_path(&ctx.db, &normalized_path, ctx.org_id, ctx.env_id)
                    .await
                    .is_ok(),
            )
        }

        async fn get_file_metadata(
            &self,
            path: &str,
            ctx: &FileStorageContext,
        ) -> Result<FileMetadata, String> {
            let normalized_path = normalize_path(path)?;
            validate_path_security(&normalized_path)?;
            let record =
                get_file_by_path(&ctx.db, &normalized_path, ctx.org_id, ctx.env_id).await?;
            Ok(FileMetadata::from_file_record(record))
        }

        async fn list_files(
            &self,
            prefix: &str,
            ctx: &FileStorageContext,
        ) -> Result<Vec<FileMetadata>, String> {
            use crate::db::file::list_files_by_prefix;
            let normalized_prefix = normalize_path(prefix).unwrap_or_else(|_| prefix.to_string());
            let records =
                list_files_by_prefix(&ctx.db, &normalized_prefix, ctx.org_id, ctx.env_id).await?;
            Ok(records
                .into_iter()
                .map(FileMetadata::from_file_record)
                .collect())
        }

        async fn list_files_bounded(
            &self,
            prefix: &str,
            limit: usize,
            ctx: &FileStorageContext,
        ) -> Result<Vec<FileMetadata>, String> {
            use crate::db::file::list_files_by_prefix_bounded;
            let normalized_prefix = normalize_path(prefix).unwrap_or_else(|_| prefix.to_string());
            let records = list_files_by_prefix_bounded(
                &ctx.db,
                &normalized_prefix,
                ctx.org_id,
                ctx.env_id,
                limit,
            )
            .await?;
            Ok(records
                .into_iter()
                .map(FileMetadata::from_file_record)
                .collect())
        }

        fn storage_type(&self) -> &str {
            BACKEND_ID
        }

        async fn initiate_multipart_upload(
            &self,
            path: &str,
            _content_type: Option<&str>,
            ctx: &FileStorageContext,
        ) -> Result<String, String> {
            let normalized_path = normalize_path(path)?;
            validate_path_security(&normalized_path)?;
            self.ensure_writable(&normalized_path, ctx).await?;
            // Parts are independent objects; no server-side session exists
            // until completion composes them.
            Ok(Uuid::new_v4().simple().to_string())
        }

        async fn upload_part(
            &self,
            backend_upload_id: &str,
            _path: &str,
            part_number: i32,
            content: &[u8],
            ctx: &FileStorageContext,
        ) -> Result<String, String> {
            validate_part_number(part_number)?;
            let staging = self.staging_prefix(backend_upload_id, ctx);
            // Every attempt (retry, replacement, concurrent duplicate) gets
            // its own object, so the token recorded in the manifest pins the
            // exact bytes that completion will compose.
            let attempt = Uuid::new_v4().simple().to_string();
            let key = Self::part_key(&staging, part_number, &attempt);
            let (_, generation) = self
                .put_unique(&key, Bytes::copy_from_slice(content), None, &[])
                .await
                .map_err(|e| format!("Failed to upload part {}: {}", part_number, e))?;
            Ok(format!("{attempt}.{generation}"))
        }

        async fn complete_multipart_upload(
            &self,
            backend_upload_id: &str,
            path: &str,
            parts: &[(i32, String)],
            total_size: i64,
            content_type: Option<&str>,
            base: &CommitBase,
            ctx: &FileStorageContext,
        ) -> Result<FileMetadata, String> {
            let normalized_path = normalize_path(path)?;
            validate_path_security(&normalized_path)?;
            self.ensure_writable(&normalized_path, ctx).await?;

            let mut ordered = parts.to_vec();
            ordered.sort_by_key(|(number, _)| *number);
            if ordered.windows(2).any(|w| w[0].0 == w[1].0) {
                return Err("Invalid manifest: duplicate part numbers".to_string());
            }
            let staging = self.staging_prefix(backend_upload_id, ctx);
            let mut sources = Vec::with_capacity(ordered.len());
            for (number, token) in &ordered {
                validate_part_number(*number)?;
                let (attempt, generation) = Self::parse_part_token(token)
                    .ok_or_else(|| format!("Invalid manifest: bad token for part {}", number))?;
                sources.push((Self::part_key(&staging, *number, attempt), generation));
            }

            let content_type_final = content_type
                .map(|s| s.to_string())
                .or_else(|| detect_content_type(&normalized_path, &[]));
            let digest = Self::manifest_digest(&ordered);
            let metadata = [
                (META_UPLOAD_ID, backend_upload_id),
                (META_MANIFEST, digest.as_str()),
            ];
            let dest = self.object_key(
                &multipart_rel_path(&normalized_path, backend_upload_id, KEY_NAME_MAX),
                ctx,
            );
            let storage_path = self.client.uri(&dest);

            // A retried completion whose earlier attempt already committed.
            if let StagedBytes::Committed(meta) =
                staged_bytes_state(ctx, &normalized_path, &storage_path).await
            {
                self.cleanup_staging(&staging).await;
                return Ok(*meta);
            }

            // Recoverable completion: the destination is deterministic per
            // upload, so a retry after a crash or lost response finds the
            // object it (or a concurrent completer) already composed and
            // verifies it was built from this exact manifest.
            let existing = self
                .client
                .head(&dest)
                .await
                .map_err(|e| format!("Failed to complete upload: {}", e))?;
            let reusable = |obj: &google_cloud_storage::model::Object| {
                obj.metadata.get(META_UPLOAD_ID).map(String::as_str) == Some(backend_upload_id)
                    && obj.metadata.get(META_MANIFEST) == Some(&digest)
            };
            let (dest_generation, composed_size) = match existing {
                Some(obj) if reusable(&obj) => (obj.generation, obj.size),
                Some(_) => {
                    return Err(
                        "Upload destination exists with a different manifest; abort and retry"
                            .to_string(),
                    );
                }
                None => {
                    let composed = if sources.is_empty() {
                        self.client
                            .put_bytes(
                                &dest,
                                Bytes::new(),
                                content_type_final.as_deref(),
                                Some(0),
                                &metadata,
                            )
                            .await
                            .map(|obj| (obj.name, obj.generation))
                    } else {
                        self.compose_staged(
                            &staging,
                            &dest,
                            sources,
                            content_type_final.as_deref(),
                            &metadata,
                        )
                        .await
                    };
                    match composed {
                        Ok(_) => {}
                        // A concurrent completer created it first.
                        Err(e) if e.is_precondition_failed() => {}
                        Err(GcsError::NotFound(_)) => {
                            return Err(
                                "Upload parts are missing (aborted or expired upload)".to_string()
                            );
                        }
                        Err(e) => return Err(format!("Failed to complete upload: {}", e)),
                    }
                    match self.client.head(&dest).await {
                        Ok(Some(obj)) if reusable(&obj) => (obj.generation, obj.size),
                        Ok(_) => {
                            return Err("Upload destination changed during completion".to_string());
                        }
                        Err(e) => return Err(format!("Failed to complete upload: {}", e)),
                    }
                }
            };

            if composed_size != total_size {
                self.delete_key_best_effort(&dest, Some(dest_generation))
                    .await;
                return Err(format!(
                    "Assembled size {} does not match the {} bytes received",
                    composed_size, total_size
                ));
            }

            // Composite objects carry no MD5. The manifest digest names the
            // exact parts composed, so it serves as the version token: a
            // NULL etag would match any expectation in a conditional write.
            let etag = format!("{}-{}", digest, ordered.len());
            let committed = commit_file_record(
                BACKEND_ID,
                &storage_path,
                &normalized_path,
                composed_size,
                Some(&etag),
                content_type_final.as_deref(),
                base,
                ctx,
            )
            .await;
            let meta = match committed {
                Ok(Committed::Swapped(meta) | Committed::Already(meta)) => meta,
                Ok(Committed::Conflict) => {
                    self.delete_key_best_effort(&dest, Some(dest_generation))
                        .await;
                    return Err(completion_conflict(&normalized_path));
                }
                // The composed object is kept: a retry reuses it, and an
                // abort deletes it.
                Err(e) => match staged_bytes_state(ctx, &normalized_path, &storage_path).await {
                    StagedBytes::Committed(meta) => *meta,
                    StagedBytes::Unreferenced | StagedBytes::Unknown => return Err(e),
                },
            };
            self.cleanup_staging(&staging).await;
            tracing::debug!("Completed multipart upload for {} to GCS", normalized_path);
            Ok(meta)
        }

        async fn abort_multipart_upload(
            &self,
            backend_upload_id: &str,
            path: &str,
            ctx: &FileStorageContext,
        ) -> Result<(), String> {
            let staging = self.staging_prefix(backend_upload_id, ctx);
            let failures = self
                .client
                .delete_prefix(&staging)
                .await
                .map_err(|e| format!("Failed to clean up multipart upload: {}", e))?;
            // A completion that composed but never committed leaves its
            // destination outside the staging prefix.
            let normalized_path = normalize_path(path)?;
            let dest = self.object_key(
                &multipart_rel_path(&normalized_path, backend_upload_id, KEY_NAME_MAX),
                ctx,
            );
            if let StagedBytes::Unreferenced =
                staged_bytes_state(ctx, &normalized_path, &self.client.uri(&dest)).await
            {
                self.delete_key_best_effort(&dest, None).await;
            }
            if failures > 0 {
                return Err(format!(
                    "Failed to delete {} multipart staging objects",
                    failures
                ));
            }
            Ok(())
        }

        async fn write_blob_bytes(
            &self,
            storage_key: &str,
            content: &[u8],
            content_type: Option<&str>,
            ctx: &FileStorageContext,
        ) -> Result<String, String> {
            let normalized = normalize_path(storage_key)?;
            validate_path_security(&normalized)?;
            let key = self.object_key(&normalized, ctx);
            let data = Bytes::copy_from_slice(content);
            // Content-addressed: an existing object with the same key holds
            // the same bytes, so create-only avoids rewriting it.
            match self
                .client
                .put_bytes(&key, data.clone(), content_type, Some(0), &[])
                .await
            {
                Ok(_) => {}
                Err(e) if e.is_precondition_failed() => match self.client.head(&key).await {
                    Ok(Some(obj)) if obj.size == content.len() as i64 => {}
                    // A truncated or foreign object at a content address is
                    // replaced, pinned to the generation we saw.
                    Ok(Some(obj)) => {
                        self.client
                            .put_bytes(&key, data, content_type, Some(obj.generation), &[])
                            .await
                            .map_err(|e| format!("Failed to upload blob to GCS: {}", e))?;
                    }
                    Ok(None) => {
                        self.client
                            .put_bytes(&key, data, content_type, Some(0), &[])
                            .await
                            .map_err(|e| format!("Failed to upload blob to GCS: {}", e))?;
                    }
                    Err(e) => return Err(format!("Failed to upload blob to GCS: {}", e)),
                },
                Err(e) => return Err(format!("Failed to upload blob to GCS: {}", e)),
            }
            Ok(self.client.uri(&key))
        }

        async fn read_blob_bytes(
            &self,
            storage_key: &str,
            ctx: &FileStorageContext,
        ) -> Result<Vec<u8>, String> {
            let normalized = normalize_path(storage_key)?;
            validate_path_security(&normalized)?;
            let key = self.object_key(&normalized, ctx);
            self.client
                .get_bytes(&key, None)
                .await
                .map_err(|e| match e {
                    GcsError::NotFound(_) => format!("Blob not found in storage: {}", normalized),
                    e => format!("Failed to read blob from GCS: {}", e),
                })
        }

        async fn delete_blob_bytes(
            &self,
            storage_key: &str,
            ctx: &FileStorageContext,
        ) -> Result<(), String> {
            let normalized = normalize_path(storage_key)?;
            validate_path_security(&normalized)?;
            let key = self.object_key(&normalized, ctx);
            self.client
                .delete(&key, None)
                .await
                .map(|_| ())
                .map_err(|e| format!("Failed to delete blob from GCS: {}", e))
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn part_tokens_round_trip_and_reject_garbage() {
            let attempt = Uuid::new_v4().simple().to_string();
            let token = format!("{attempt}.1712345678901234");
            assert_eq!(
                GcsFileStorage::parse_part_token(&token),
                Some((attempt.as_str(), 1712345678901234))
            );
            assert!(GcsFileStorage::parse_part_token("etag").is_none());
            assert!(GcsFileStorage::parse_part_token(&format!("{attempt}.0")).is_none());
            assert!(GcsFileStorage::parse_part_token(&format!("{attempt}.-1")).is_none());
            assert!(GcsFileStorage::parse_part_token("../../x.1").is_none());
        }

        #[test]
        fn manifest_digest_depends_on_order_and_tokens() {
            let a = vec![(1, "x.1".to_string()), (2, "y.2".to_string())];
            let b = vec![(1, "x.1".to_string()), (2, "z.2".to_string())];
            assert_eq!(
                GcsFileStorage::manifest_digest(&a),
                GcsFileStorage::manifest_digest(&a.clone())
            );
            assert_ne!(
                GcsFileStorage::manifest_digest(&a),
                GcsFileStorage::manifest_digest(&b)
            );
        }
    }
}

// ============================================================================
// Storage Factory
// ============================================================================

/// File storage type from configuration
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileStorageType {
    Local,
    S3,
    Gcs,
}

/// Parse a storage backend name shared by file and build storage. Empty
/// means the default (local); unknown names are rejected rather than
/// silently falling back to local storage.
pub fn parse_storage_backend_name(raw: &str, setting: &str) -> Result<FileStorageType, String> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "" | "null" | "local" => Ok(FileStorageType::Local),
        "s3" => Ok(FileStorageType::S3),
        "gcs" => Ok(FileStorageType::Gcs),
        other => Err(format!(
            "Unknown {setting} '{other}'; expected one of: local, s3, gcs"
        )),
    }
}

/// A backend's initialization error, naming the settings that selected it.
/// Configuration generated by older role templates sets `storage.type` to
/// "s3" without a bucket; it was ignored before config values were honored.
pub fn backend_init_error(error: String, role: &str, backend: &str) -> String {
    format!(
        "{error} ({role} storage type '{backend}' is selected by HOT_{}_STORAGE_TYPE or \
         {role}.storage.type; set it to \"local\" to keep local storage)",
        role.to_ascii_uppercase()
    )
}

/// Resolve `<role>.storage.type`: the `HOT_<ROLE>_STORAGE_TYPE` environment
/// variable wins, then Hot configuration, then local.
pub fn resolve_storage_backend(conf: &Val, role: &str) -> Result<FileStorageType, String> {
    let env_name = format!("HOT_{}_STORAGE_TYPE", role.to_ascii_uppercase());
    let raw = std::env::var(&env_name)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| conf.get_str_or_default(&format!("{role}.storage.type"), ""));
    parse_storage_backend_name(&raw, &format!("{role}.storage.type ({env_name})"))
}

impl FileStorageType {
    /// Backend selected by `HOT_FILE_STORAGE_TYPE` / `file.storage.type`.
    pub fn resolve(conf: &Val) -> Result<Self, String> {
        resolve_storage_backend(conf, "file")
    }
}

/// Create a file storage instance from Hot configuration
pub async fn file_storage_from_config(conf: &Val) -> Result<Box<dyn FileStorage>, String> {
    let storage_type = FileStorageType::resolve(conf)?;

    match storage_type {
        FileStorageType::Local => {
            // Try to get path from config, fallback to environment or default
            let path = if let Some(Val::Str(path_str)) = conf.get("file.storage.path") {
                PathBuf::from(&*path_str)
            } else {
                std::env::var("HOT_FILE_STORAGE_PATH")
                    .map(PathBuf::from)
                    .unwrap_or_else(|_| crate::lang::cache::paths::get_file_storage_dir())
            };
            if path.as_os_str().is_empty() {
                return Err("file.storage.path must not be empty".into());
            }
            Ok(Box::new(LocalFileStorage::new(path)))
        }
        #[cfg(feature = "s3-storage")]
        FileStorageType::S3 => {
            let storage = s3::S3FileStorage::from_config(conf)
                .await
                .map_err(|e| backend_init_error(e, "file", "s3"))?;
            Ok(Box::new(storage))
        }
        #[cfg(not(feature = "s3-storage"))]
        FileStorageType::S3 => {
            Err("S3 storage requested but s3-storage feature is not enabled".to_string())
        }
        #[cfg(feature = "gcs-storage")]
        FileStorageType::Gcs => Ok(Box::new(
            gcs::GcsFileStorage::from_config(conf)
                .await
                .map_err(|e| backend_init_error(e, "file", "gcs"))?,
        )),
        #[cfg(not(feature = "gcs-storage"))]
        FileStorageType::Gcs => Err(
            "GCS file storage requested but this binary was built without the gcs-storage feature"
                .to_string(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "s3-storage")]
    #[tokio::test]
    async fn s3_configuration_errors_do_not_panic_or_contact_aws() {
        let missing_bucket = crate::val!({"file": {"aws": {"s3": {}}}});
        assert!(
            s3::S3FileStorage::from_config(&missing_bucket)
                .await
                .is_err()
        );
        let incomplete_credentials = crate::val!({"file": {"aws": {
            "access-key-id": "only-one-half",
            "s3": {"bucket": "customer-bucket", "region": "us-east-1"}
        }}});
        assert!(
            s3::S3FileStorage::from_config(&incomplete_credentials)
                .await
                .is_err()
        );
    }

    #[test]
    fn test_normalize_path() {
        assert_eq!(normalize_path("foo/bar").unwrap(), "foo/bar");
        assert_eq!(normalize_path("hot://foo/bar").unwrap(), "foo/bar");
        assert_eq!(
            normalize_path("hot://hot-live/windows/a/b/1.webm").unwrap(),
            "hot-live/windows/a/b/1.webm"
        );
        assert_eq!(normalize_path("/foo/bar").unwrap(), "foo/bar");
        assert_eq!(normalize_path("./foo/bar").unwrap(), "foo/bar");
        assert_eq!(normalize_path("foo/./bar").unwrap(), "foo/bar");
        assert_eq!(normalize_path("foo/../bar").unwrap(), "bar");
        assert_eq!(normalize_path("foo/bar/../baz").unwrap(), "foo/baz");

        // Should fail
        assert!(normalize_path("../foo").is_err());
        assert!(normalize_path("").is_err());
        assert!(normalize_path(".").is_err());
        assert!(normalize_path("./").is_err());
    }

    #[test]
    fn test_compute_md5() {
        let content = b"hello world";
        let hash = compute_md5(content);
        assert_eq!(hash, "5eb63bbbe01eeed093cb22bb8f5acdc3");
    }

    #[test]
    fn test_detect_content_type() {
        assert_eq!(
            detect_content_type("file.json", b"{}"),
            Some("application/json".to_string())
        );
        assert_eq!(
            detect_content_type("file.txt", b"text"),
            Some("text/plain".to_string())
        );
        assert_eq!(
            detect_content_type("image.png", b""),
            Some("image/png".to_string())
        );
        assert_eq!(
            detect_content_type("app.hot", b""),
            Some("text/x-hot".to_string())
        );
    }

    #[test]
    fn test_compute_part_size_small_file() {
        // Files smaller than default part size: should use DEFAULT_PART_SIZE
        assert_eq!(compute_part_size(100 * 1024 * 1024), DEFAULT_PART_SIZE); // 100MB
        assert_eq!(compute_part_size(200 * 1024 * 1024), DEFAULT_PART_SIZE); // 200MB
        assert_eq!(compute_part_size(1024 * 1024 * 1024), DEFAULT_PART_SIZE); // 1GB
    }

    #[test]
    fn test_compute_part_size_large_file() {
        // At 640GB, min_for_count = ceil(640GB/10000) > 64MB, so we use the larger value
        let part_640gb = compute_part_size(640 * 1024 * 1024 * 1024);
        assert!(part_640gb >= DEFAULT_PART_SIZE);
        assert!(640u64 * 1024 * 1024 * 1024 / part_640gb <= MAX_UPLOAD_PARTS);

        // At 1TB, parts must grow to stay under 10,000 parts
        let part_1tb = compute_part_size(1024 * 1024 * 1024 * 1024);
        assert!(part_1tb > DEFAULT_PART_SIZE);
        assert!(1024u64 * 1024 * 1024 * 1024 / part_1tb <= MAX_UPLOAD_PARTS);

        // At 5TB, parts should be ~500MB
        let part_5tb = compute_part_size(5 * 1024 * 1024 * 1024 * 1024);
        assert!(part_5tb >= 500 * 1024 * 1024); // at least 500MB
        assert!(5u64 * 1024 * 1024 * 1024 * 1024 / part_5tb <= MAX_UPLOAD_PARTS);
    }

    #[test]
    fn test_compute_part_size_edge_cases() {
        // Zero bytes
        assert_eq!(compute_part_size(0), DEFAULT_PART_SIZE);
        // 1 byte
        assert_eq!(compute_part_size(1), DEFAULT_PART_SIZE);
        // Exactly at boundary: 640GB = 10,000 * 64MB
        let boundary = MAX_UPLOAD_PARTS * DEFAULT_PART_SIZE;
        assert_eq!(compute_part_size(boundary), DEFAULT_PART_SIZE);
        // Just over boundary
        assert!(compute_part_size(boundary + 1) >= DEFAULT_PART_SIZE);
    }

    #[test]
    fn test_validate_part_number() {
        // Valid range
        assert!(validate_part_number(1).is_ok());
        assert!(validate_part_number(5000).is_ok());
        assert!(validate_part_number(10_000).is_ok());

        // Invalid
        assert!(validate_part_number(0).is_err());
        assert!(validate_part_number(-1).is_err());
        assert!(validate_part_number(10_001).is_err());
    }

    async fn setup_test_storage() -> (LocalFileStorage, FileStorageContext, tempfile::TempDir) {
        use sqlx::sqlite::SqlitePoolOptions;

        let temp_dir = tempfile::TempDir::new().unwrap();
        let storage = LocalFileStorage::new(temp_dir.path().to_path_buf());

        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();

        sqlx::query("PRAGMA foreign_keys = OFF")
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
                created_by_run_id blob,
                updated_by_run_id blob,
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

        // Mirrors migration 007: NULL env_id collapses to a sentinel so
        // env-less contexts still dedupe active paths.
        sqlx::query(
            "CREATE UNIQUE INDEX idx_file_org_env_path_active_unique ON file(org_id, coalesce(env_id, x'00000000000000000000000000000000'), path) WHERE active = 1",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::raw_sql(include_str!(
            "../../../resources/db/sqlite/migrations/021_retire_file_objects.sql"
        ))
        .execute(&pool)
        .await
        .unwrap();

        let db = crate::db::DatabasePool::Sqlite(pool);
        let db_arc = std::sync::Arc::new(db);

        let org_id = uuid::Uuid::now_v7();
        crate::db::file::seed_storage_quota_test_org(&db_arc, org_id).await;
        let env_id = uuid::Uuid::now_v7();
        let user_id = uuid::Uuid::now_v7();

        let ctx = FileStorageContext {
            db: db_arc,
            org_id,
            env_id: Some(env_id),
            user_id,
            run_id: None,
            run_provenance: None,
            file_max_bytes_conf: None,
        };

        (storage, ctx, temp_dir)
    }

    /// Collect every retired object, as if its grace period had passed.
    async fn collect_retired(storage: &dyn FileStorage, ctx: &FileStorageContext) -> usize {
        let after_grace = Utc::now() + chrono::Duration::days(1);
        collect_retired_objects(&ctx.db, storage, after_grace, 1_000)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn many_concurrent_writers_all_commit() {
        let (storage, ctx, _temp) = setup_test_storage().await;
        let writes = (0..16).map(|i| {
            let (storage, ctx) = (&storage, &ctx);
            async move {
                let body = format!("version-{i}");
                storage
                    .write_file("busy.txt", body.as_bytes(), None, ctx)
                    .await
            }
        });
        for result in futures::future::join_all(writes).await {
            result.unwrap();
        }
    }

    #[tokio::test]
    async fn longest_file_names_are_storable() {
        let (storage, ctx, _temp) = setup_test_storage().await;
        let name = format!("dir/{}", "n".repeat(255));
        storage.write_file(&name, b"one", None, &ctx).await.unwrap();
        let etag = storage.get_file_metadata(&name, &ctx).await.unwrap().etag;
        storage
            .write_file_if(&name, b"two", None, etag.as_deref(), &ctx)
            .await
            .unwrap()
            .expect("the etag matches");
        assert_eq!(storage.read_file(&name, &ctx).await.unwrap(), b"two");

        let upload = storage
            .initiate_multipart_upload(&name, None, &ctx)
            .await
            .unwrap();
        let part = storage
            .upload_part(&upload, &name, 1, b"three", &ctx)
            .await
            .unwrap();
        storage
            .complete_multipart_upload(
                &upload,
                &name,
                &[(1, part)],
                5,
                None,
                &CommitBase::Any,
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(storage.read_file(&name, &ctx).await.unwrap(), b"three");
        assert!(storage.delete_file(&name, &ctx).await.unwrap());
    }

    #[tokio::test]
    async fn deleting_a_file_leaves_a_directory_of_the_same_name() {
        let (storage, ctx, _temp) = setup_test_storage().await;
        storage
            .write_file("data/x", b"child", None, &ctx)
            .await
            .unwrap();
        storage
            .write_file("data", b"parent", None, &ctx)
            .await
            .unwrap();
        assert!(storage.delete_file("data", &ctx).await.unwrap());
        assert!(!storage.file_exists("data", &ctx).await.unwrap());
        assert_eq!(storage.read_file("data/x", &ctx).await.unwrap(), b"child");
    }

    #[tokio::test]
    async fn rejected_storage_growth_preserves_committed_bytes() {
        let (storage, ctx, _temp) = setup_test_storage().await;
        let crate::db::DatabasePool::Sqlite(pool) = ctx.db.as_ref() else {
            unreachable!()
        };
        sqlx::query("UPDATE org SET features=? WHERE org_id=?")
            .bind(r#"{"storage_bytes":5}"#)
            .bind(ctx.org_id)
            .execute(pool)
            .await
            .unwrap();
        storage
            .write_file("same.txt", b"first", None, &ctx)
            .await
            .unwrap();
        assert!(
            storage
                .write_file("same.txt", b"too long", None, &ctx)
                .await
                .is_err()
        );
        let bytes = storage.read_file("same.txt", &ctx).await.unwrap();
        assert_eq!(bytes, b"first");
        storage
            .write_file("same.txt", b"short", None, &ctx)
            .await
            .unwrap();
        let bytes = storage.read_file("same.txt", &ctx).await.unwrap();
        assert_eq!(bytes, b"short");
    }

    #[tokio::test]
    async fn test_delete_file_only_deactivates_current_env() {
        let (storage, ctx, _temp_dir) = setup_test_storage().await;
        let other_env_id = uuid::Uuid::now_v7();
        let mut other_ctx = ctx.clone();
        other_ctx.env_id = Some(other_env_id);

        storage
            .write_file("shared/report.txt", b"env one", None, &ctx)
            .await
            .unwrap();
        storage
            .write_file("shared/report.txt", b"env two", None, &other_ctx)
            .await
            .unwrap();

        assert!(
            storage
                .delete_file("shared/report.txt", &ctx)
                .await
                .unwrap()
        );

        assert!(
            !storage
                .file_exists("shared/report.txt", &ctx)
                .await
                .unwrap()
        );
        assert!(
            storage
                .file_exists("shared/report.txt", &other_ctx)
                .await
                .unwrap()
        );
        assert_eq!(
            storage
                .read_file("shared/report.txt", &other_ctx)
                .await
                .unwrap(),
            b"env two"
        );
    }

    #[tokio::test]
    async fn test_list_files_only_returns_current_env() {
        let (storage, ctx, _temp_dir) = setup_test_storage().await;
        let other_env_id = uuid::Uuid::now_v7();
        let mut other_ctx = ctx.clone();
        other_ctx.env_id = Some(other_env_id);

        storage
            .write_file("docs/current.txt", b"current", None, &ctx)
            .await
            .unwrap();
        storage
            .write_file("docs/other.txt", b"other", None, &other_ctx)
            .await
            .unwrap();

        let files = storage.list_files("docs/", &ctx).await.unwrap();
        let paths: Vec<_> = files.into_iter().map(|file| file.path).collect();

        assert_eq!(paths, vec!["docs/current.txt".to_string()]);
    }

    #[tokio::test]
    async fn test_hot_scheme_paths_are_canonical_for_hotbox_reads() {
        let (storage, ctx, _temp_dir) = setup_test_storage().await;

        storage
            .write_file("hot://hot-live/windows/a/b/153.webm", b"video", None, &ctx)
            .await
            .unwrap();

        assert_eq!(
            storage
                .read_file("hot-live/windows/a/b/153.webm", &ctx)
                .await
                .unwrap(),
            b"video"
        );
    }

    #[tokio::test]
    async fn test_write_file_if_create_and_conditional_update() {
        let (storage, ctx, _temp_dir) = setup_test_storage().await;

        // Create-if-absent succeeds and the bytes are readable back.
        let v1 = storage
            .write_file_if("dbs/app.db", b"version-1", None, None, &ctx)
            .await
            .unwrap()
            .expect("create should win");
        assert_eq!(
            storage.read_file("dbs/app.db", &ctx).await.unwrap(),
            b"version-1"
        );

        // A second create-if-absent loses without touching the content.
        let lost = storage
            .write_file_if("dbs/app.db", b"intruder", None, None, &ctx)
            .await
            .unwrap();
        assert!(lost.is_none());
        assert_eq!(
            storage.read_file("dbs/app.db", &ctx).await.unwrap(),
            b"version-1"
        );

        // Conditional update with the current etag wins; the superseded
        // staged blob stays readable until collection removes it.
        let v1_storage_path = v1.storage_path.clone();
        let v2 = storage
            .write_file_if("dbs/app.db", b"version-2", None, v1.etag.as_deref(), &ctx)
            .await
            .unwrap()
            .expect("matching etag should win");
        assert_eq!(
            storage.read_file("dbs/app.db", &ctx).await.unwrap(),
            b"version-2"
        );
        assert!(std::path::Path::new(&v1_storage_path).exists());
        collect_retired(&storage, &ctx).await;
        assert!(!std::path::Path::new(&v1_storage_path).exists());

        // A writer still holding the old etag loses; content stays intact
        // and its staged bytes are discarded.
        let stale = storage
            .write_file_if("dbs/app.db", b"stale-write", None, v1.etag.as_deref(), &ctx)
            .await
            .unwrap();
        assert!(stale.is_none());
        assert_eq!(
            storage.read_file("dbs/app.db", &ctx).await.unwrap(),
            b"version-2"
        );
        assert!(std::path::Path::new(&v2.storage_path).exists());
    }

    #[tokio::test]
    async fn test_write_file_if_null_env_create_race_is_serialized() {
        let (storage, ctx, _temp_dir) = setup_test_storage().await;
        let mut envless_ctx = ctx.clone();
        envless_ctx.env_id = None;

        let won = storage
            .write_file_if("dbs/shared.db", b"first", None, None, &envless_ctx)
            .await
            .unwrap();
        assert!(won.is_some());

        // Without the NULL-collapsing unique index, this second insert
        // would create a duplicate active record instead of losing.
        let lost = storage
            .write_file_if("dbs/shared.db", b"second", None, None, &envless_ctx)
            .await
            .unwrap();
        assert!(lost.is_none());
        assert_eq!(
            storage
                .read_file("dbs/shared.db", &envless_ctx)
                .await
                .unwrap(),
            b"first"
        );
    }

    #[tokio::test]
    async fn test_write_file_if_loses_after_delete_and_can_recreate() {
        let (storage, ctx, _temp_dir) = setup_test_storage().await;

        let v1 = storage
            .write_file_if("dbs/cycle.db", b"alive", None, None, &ctx)
            .await
            .unwrap()
            .expect("create should win");

        assert!(storage.delete_file("dbs/cycle.db", &ctx).await.unwrap());
        // Collection removes the CAS blob from disk too.
        collect_retired(&storage, &ctx).await;
        assert!(!std::path::Path::new(&v1.storage_path).exists());

        // An update expecting the pre-delete etag must lose (the active
        // record is gone), not resurrect the inactive row.
        let stale = storage
            .write_file_if("dbs/cycle.db", b"zombie", None, v1.etag.as_deref(), &ctx)
            .await
            .unwrap();
        assert!(stale.is_none());
        assert!(!storage.file_exists("dbs/cycle.db", &ctx).await.unwrap());

        // Create-if-absent works again after the delete.
        let recreated = storage
            .write_file_if("dbs/cycle.db", b"rebirth", None, None, &ctx)
            .await
            .unwrap();
        assert!(recreated.is_some());
        assert_eq!(
            storage.read_file("dbs/cycle.db", &ctx).await.unwrap(),
            b"rebirth"
        );
    }

    #[tokio::test]
    async fn test_write_file_if_matches_null_etag_record_once() {
        let (storage, ctx, _temp_dir) = setup_test_storage().await;

        // Simulate a record without an etag (multipart uploads and legacy
        // rows store NULL): write normally, then null the etag out.
        storage
            .write_file("dbs/legacy.db", b"legacy-bytes", None, &ctx)
            .await
            .unwrap();
        let crate::db::DatabasePool::Sqlite(pool) = ctx.db.as_ref() else {
            panic!("test uses sqlite");
        };
        sqlx::query("UPDATE file SET etag = NULL")
            .execute(pool)
            .await
            .unwrap();

        // A commit whose expectation is a content hash (what ::hot::sqlite
        // checkout computes for etag-less records) must still win once...
        let checkout_hash = compute_md5(b"legacy-bytes");
        let won = storage
            .write_file_if(
                "dbs/legacy.db",
                b"migrated",
                None,
                Some(&checkout_hash),
                &ctx,
            )
            .await
            .unwrap();
        let won = won.expect("NULL-etag record should accept the first CAS");
        assert_eq!(
            storage.read_file("dbs/legacy.db", &ctx).await.unwrap(),
            b"migrated"
        );

        // ...and the winner stamps a real etag, so a second stale writer
        // now loses strictly.
        let stale = storage
            .write_file_if(
                "dbs/legacy.db",
                b"late-write",
                None,
                Some(&checkout_hash),
                &ctx,
            )
            .await
            .unwrap();
        assert!(stale.is_none());
        assert_eq!(won.etag, Some(compute_md5(b"migrated")));
    }

    #[tokio::test]
    async fn test_local_multipart_full_lifecycle() {
        let (storage, ctx, _temp_dir) = setup_test_storage().await;

        let upload_id = storage
            .initiate_multipart_upload("data/video.mp4", Some("video/mp4"), &ctx)
            .await
            .unwrap();
        assert!(!upload_id.is_empty());

        let part1_data = vec![1u8; 1024];
        let part2_data = vec![2u8; 1024];
        let part3_data = vec![3u8; 512];

        let etag1 = storage
            .upload_part(&upload_id, "data/video.mp4", 1, &part1_data, &ctx)
            .await
            .unwrap();
        let etag2 = storage
            .upload_part(&upload_id, "data/video.mp4", 2, &part2_data, &ctx)
            .await
            .unwrap();
        let etag3 = storage
            .upload_part(&upload_id, "data/video.mp4", 3, &part3_data, &ctx)
            .await
            .unwrap();

        assert!(!etag1.is_empty());
        assert!(!etag2.is_empty());
        assert!(!etag3.is_empty());

        let parts = vec![(1, etag1), (2, etag2), (3, etag3)];
        let total_size = (part1_data.len() + part2_data.len() + part3_data.len()) as i64;
        let metadata = storage
            .complete_multipart_upload(
                &upload_id,
                "data/video.mp4",
                &parts,
                total_size,
                Some("video/mp4"),
                &CommitBase::Any,
                &ctx,
            )
            .await
            .unwrap();

        assert_eq!(metadata.path, "data/video.mp4");
        assert_eq!(metadata.size, total_size);
        assert_eq!(metadata.content_type.as_deref(), Some("video/mp4"));

        let content = storage.read_file("data/video.mp4", &ctx).await.unwrap();
        let mut expected = Vec::new();
        expected.extend_from_slice(&part1_data);
        expected.extend_from_slice(&part2_data);
        expected.extend_from_slice(&part3_data);
        assert_eq!(content, expected);
    }

    #[tokio::test]
    async fn test_local_multipart_abort() {
        let (storage, ctx, temp_dir) = setup_test_storage().await;

        let upload_id = storage
            .initiate_multipart_upload("data/aborted.bin", None, &ctx)
            .await
            .unwrap();

        storage
            .upload_part(&upload_id, "data/aborted.bin", 1, &[0u8; 256], &ctx)
            .await
            .unwrap();

        let upload_dir = temp_dir.path().join(".uploads").join(&upload_id);
        assert!(upload_dir.exists());

        storage
            .abort_multipart_upload(&upload_id, "data/aborted.bin", &ctx)
            .await
            .unwrap();

        assert!(!upload_dir.exists());
    }

    #[tokio::test]
    async fn test_local_multipart_part_order() {
        let (storage, ctx, _temp_dir) = setup_test_storage().await;

        let upload_id = storage
            .initiate_multipart_upload("data/ordered.bin", None, &ctx)
            .await
            .unwrap();

        let part3 = vec![3u8; 100];
        let part1 = vec![1u8; 100];
        let part2 = vec![2u8; 100];

        let etag3 = storage
            .upload_part(&upload_id, "data/ordered.bin", 3, &part3, &ctx)
            .await
            .unwrap();
        let etag1 = storage
            .upload_part(&upload_id, "data/ordered.bin", 1, &part1, &ctx)
            .await
            .unwrap();
        let etag2 = storage
            .upload_part(&upload_id, "data/ordered.bin", 2, &part2, &ctx)
            .await
            .unwrap();

        let parts = vec![(1, etag1), (2, etag2), (3, etag3)];
        let total_size = 300i64;
        storage
            .complete_multipart_upload(
                &upload_id,
                "data/ordered.bin",
                &parts,
                total_size,
                None,
                &CommitBase::Any,
                &ctx,
            )
            .await
            .unwrap();

        let content = storage.read_file("data/ordered.bin", &ctx).await.unwrap();
        let mut expected = Vec::new();
        expected.extend_from_slice(&part1);
        expected.extend_from_slice(&part2);
        expected.extend_from_slice(&part3);
        assert_eq!(content, expected);
    }

    #[tokio::test]
    async fn test_local_multipart_part_overwrite() {
        let (storage, ctx, _temp_dir) = setup_test_storage().await;

        let upload_id = storage
            .initiate_multipart_upload("data/overwrite.bin", None, &ctx)
            .await
            .unwrap();

        let first_data = vec![0xAAu8; 100];
        let second_data = vec![0xBBu8; 100];

        storage
            .upload_part(&upload_id, "data/overwrite.bin", 1, &first_data, &ctx)
            .await
            .unwrap();
        let etag = storage
            .upload_part(&upload_id, "data/overwrite.bin", 1, &second_data, &ctx)
            .await
            .unwrap();

        let parts = vec![(1, etag)];
        storage
            .complete_multipart_upload(
                &upload_id,
                "data/overwrite.bin",
                &parts,
                100,
                None,
                &CommitBase::Any,
                &ctx,
            )
            .await
            .unwrap();

        let content = storage.read_file("data/overwrite.bin", &ctx).await.unwrap();
        assert_eq!(content, second_data);
    }

    #[tokio::test]
    async fn test_concurrent_writes_leave_no_orphaned_bytes() {
        let (storage, ctx, _temp_dir) = setup_test_storage().await;

        // Overlapping writers each retire the version they replaced, so once
        // collection runs only the committed version is left on disk.
        for round in 0..10 {
            let one = format!("one-{round}");
            let two = format!("two-two-{round}");
            let three = format!("three-three-three-{round}");
            let (a, b, c) = tokio::join!(
                storage.write_file("race/leak.txt", one.as_bytes(), None, &ctx),
                storage.write_file("race/leak.txt", two.as_bytes(), None, &ctx),
                storage.write_file("race/leak.txt", three.as_bytes(), None, &ctx),
            );
            a.unwrap();
            b.unwrap();
            c.unwrap();
        }
        collect_retired(&storage, &ctx).await;
        let names: Vec<String> = std::fs::read_dir(storage.full_path("race", &ctx))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(names.len(), 1, "{names:?}");
        let meta = storage
            .get_file_metadata("race/leak.txt", &ctx)
            .await
            .unwrap();
        assert!(meta.storage_path.ends_with(&names[0]));
    }

    #[tokio::test]
    async fn test_commit_respects_completion_base() {
        let (storage, ctx, _temp_dir) = setup_test_storage().await;
        let path = "base/f.bin";
        let dest = "/nonexistent/f.bin.mp-upload";
        let v0 = storage.write_file(path, b"v0", None, &ctx).await.unwrap();
        let claimed_at_v0 = CommitBase::Path(v0.storage_path.clone());
        let commit = async |base: &CommitBase| {
            commit_file_record("local", dest, path, 3, Some("e"), None, base, &ctx)
                .await
                .unwrap()
        };

        // A completion claimed against v0 commits, and retrying it is a no-op.
        assert!(matches!(
            commit(&claimed_at_v0).await,
            Committed::Swapped(_)
        ));
        assert!(matches!(
            commit(&claimed_at_v0).await,
            Committed::Already(_)
        ));

        // Once a newer write supersedes it, a retried completion can no
        // longer revert that write.
        let v2 = storage.write_file(path, b"v2", None, &ctx).await.unwrap();
        assert!(matches!(commit(&claimed_at_v0).await, Committed::Conflict));
        assert!(matches!(
            commit(&CommitBase::Absent).await,
            Committed::Conflict
        ));
        assert_eq!(storage.read_file(path, &ctx).await.unwrap(), b"v2");

        // Nor can it resurrect a deleted file.
        storage.delete_file(path, &ctx).await.unwrap();
        let claimed_at_v2 = CommitBase::Path(v2.storage_path.clone());
        assert!(matches!(commit(&claimed_at_v2).await, Committed::Conflict));
        assert!(!storage.file_exists(path, &ctx).await.unwrap());

        // A completion claimed while the path was empty creates the file.
        assert!(matches!(
            commit(&CommitBase::Absent).await,
            Committed::Swapped(_)
        ));
    }

    #[tokio::test]
    async fn test_local_multipart_completion_retry_is_idempotent() {
        let (storage, ctx, _temp_dir) = setup_test_storage().await;
        let path = "mp/retry.bin";
        let id = storage
            .initiate_multipart_upload(path, None, &ctx)
            .await
            .unwrap();
        let etag = storage
            .upload_part(&id, path, 1, b"abc", &ctx)
            .await
            .unwrap();
        let parts = [(1, etag)];

        let first = storage
            .complete_multipart_upload(&id, path, &parts, 3, None, &CommitBase::Absent, &ctx)
            .await
            .unwrap();
        // A retry after a lost response, or after the process died before the
        // upload row was marked complete, finds its own committed result even
        // though the parts are gone.
        let retried = storage
            .complete_multipart_upload(&id, path, &parts, 3, None, &CommitBase::Absent, &ctx)
            .await
            .unwrap();
        assert_eq!(first.storage_path, retried.storage_path);

        // Aborting afterwards succeeds and leaves the committed file alone.
        storage
            .abort_multipart_upload(&id, path, &ctx)
            .await
            .unwrap();
        assert_eq!(storage.read_file(path, &ctx).await.unwrap(), b"abc");
    }

    #[tokio::test]
    async fn test_local_part_retries_cannot_change_recorded_bytes() {
        let (storage, ctx, _temp_dir) = setup_test_storage().await;
        let path = "mp/parts.bin";
        let id = storage
            .initiate_multipart_upload(path, None, &ctx)
            .await
            .unwrap();
        let recorded = storage
            .upload_part(&id, path, 1, b"aaa", &ctx)
            .await
            .unwrap();
        // A late duplicate of part 1 with other bytes does not replace the
        // bytes behind the etag the manifest recorded.
        storage
            .upload_part(&id, path, 1, b"bbb", &ctx)
            .await
            .unwrap();
        storage
            .complete_multipart_upload(&id, path, &[(1, recorded)], 3, None, &CommitBase::Any, &ctx)
            .await
            .unwrap();
        assert_eq!(storage.read_file(path, &ctx).await.unwrap(), b"aaa");
    }

    #[tokio::test]
    async fn test_reads_follow_a_superseding_write() {
        let (storage, ctx, _temp_dir) = setup_test_storage().await;
        let v1 = storage
            .write_file("race/read.txt", b"one", None, &ctx)
            .await
            .unwrap();

        // The reader looked up v1 so long before a writer replaced it that
        // collection deleted it; the read follows the record to the new
        // version.
        let mut stale = true;
        let (record, bytes) = fetch_current(&ctx, "race/read.txt", |record| {
            let overwrite = std::mem::replace(&mut stale, false);
            let location = PathBuf::from(&record.storage_path);
            let (storage, ctx) = (&storage, &ctx);
            Box::pin(async move {
                if overwrite {
                    storage
                        .write_file("race/read.txt", b"two", None, ctx)
                        .await
                        .unwrap();
                    collect_retired(storage, ctx).await;
                }
                match tokio::fs::read(&location).await {
                    Ok(bytes) => Ok(Some(bytes)),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                    Err(e) => Err(e.to_string()),
                }
            })
        })
        .await
        .unwrap();
        assert_ne!(record.storage_path, v1.storage_path);
        assert_eq!(bytes, b"two");
    }

    #[tokio::test]
    async fn test_concurrent_writes_commit_consistent_metadata() {
        let (storage, ctx, _temp_dir) = setup_test_storage().await;

        // Racing first-time creates both land (the loser becomes an update)
        // instead of the loser failing on the unique index.
        let (a, b) = tokio::join!(
            storage.write_file("race/new.txt", b"first-writer", None, &ctx),
            storage.write_file("race/new.txt", b"second-writer-longer", None, &ctx),
        );
        a.unwrap();
        b.unwrap();

        // Racing overwrites: whichever commits last, the record's size and
        // etag describe exactly the bytes it references.
        for round in 0..10 {
            let one = format!("one-{round}");
            let two = format!("two-two-{round}");
            let (a, b) = tokio::join!(
                storage.write_file("race/new.txt", one.as_bytes(), None, &ctx),
                storage.write_file("race/new.txt", two.as_bytes(), None, &ctx),
            );
            a.unwrap();
            b.unwrap();
            let meta = storage
                .get_file_metadata("race/new.txt", &ctx)
                .await
                .unwrap();
            let bytes = storage.read_file("race/new.txt", &ctx).await.unwrap();
            assert_eq!(meta.size, bytes.len() as i64);
            assert_eq!(meta.etag, Some(compute_md5(&bytes)));
        }
    }

    #[tokio::test]
    async fn test_collection_removes_superseded_and_deleted_bytes() {
        let (storage, ctx, _temp_dir) = setup_test_storage().await;
        let v1 = storage
            .write_file("docs/a.txt", b"one", None, &ctx)
            .await
            .unwrap();
        let v2 = storage
            .write_file("docs/a.txt", b"two", None, &ctx)
            .await
            .unwrap();
        assert_ne!(v1.storage_path, v2.storage_path);
        // A read that resolved v1 before the overwrite can still finish.
        assert_eq!(tokio::fs::read(&v1.storage_path).await.unwrap(), b"one");
        let within_grace = Utc::now() - chrono::Duration::minutes(1);
        assert_eq!(
            collect_retired_objects(&ctx.db, &storage, within_grace, 100)
                .await
                .unwrap(),
            0
        );
        assert!(Path::new(&v1.storage_path).exists());

        assert_eq!(collect_retired(&storage, &ctx).await, 1);
        assert!(!Path::new(&v1.storage_path).exists());
        assert_eq!(storage.read_file("docs/a.txt", &ctx).await.unwrap(), b"two");
        assert!(storage.delete_file("docs/a.txt", &ctx).await.unwrap());
        assert!(Path::new(&v2.storage_path).exists());
        assert_eq!(collect_retired(&storage, &ctx).await, 1);
        assert!(!Path::new(&v2.storage_path).exists());
        assert_eq!(collect_retired(&storage, &ctx).await, 0);
    }

    #[tokio::test]
    async fn interrupted_writes_are_collected_but_referenced_objects_never() {
        let (storage, ctx, _temp_dir) = setup_test_storage().await;
        let live = storage
            .write_file("docs/live.txt", b"live", None, &ctx)
            .await
            .unwrap();
        // A write that staged and registered its bytes, then never committed.
        let staged = storage.full_path(".interrupted.v-1", &ctx);
        tokio::fs::write(&staged, b"orphan").await.unwrap();
        let staged = staged.to_string_lossy().to_string();
        register_staged(&ctx, "local", &staged).await.unwrap();
        // A retired entry for an object a record still references.
        register_staged(&ctx, "local", &live.storage_path)
            .await
            .unwrap();

        assert_eq!(collect_retired(&storage, &ctx).await, 1);
        assert!(!Path::new(&staged).exists());
        assert_eq!(
            storage.read_file("docs/live.txt", &ctx).await.unwrap(),
            b"live"
        );
    }

    #[test]
    fn staged_names_keep_their_unique_suffix_within_the_name_limit() {
        let long = format!("dir/{}", "é".repeat(200));
        let staged = unique_write_rel_path(&long, LOCAL_NAME_MAX);
        let name = staged.rsplit_once('/').unwrap().1;
        assert!(name.len() <= LOCAL_NAME_MAX, "{}", name.len());
        assert!(staged.starts_with("dir/é"));
        assert!(name.contains(".v-"));
        assert_ne!(staged, unique_write_rel_path(&long, LOCAL_NAME_MAX));
        // Object store keys keep the whole name.
        assert!(unique_write_rel_path(&long, KEY_NAME_MAX).starts_with(&long));
        assert_eq!(
            multipart_rel_path("a/b.txt", "u", LOCAL_NAME_MAX),
            "a/b.txt.mp-u"
        );
    }

    #[tokio::test]
    async fn test_writes_refuse_records_owned_by_another_backend() {
        let (storage, ctx, _temp_dir) = setup_test_storage().await;
        let v1 = storage
            .write_file("moved/data.bin", b"original", None, &ctx)
            .await
            .unwrap();
        let crate::db::DatabasePool::Sqlite(pool) = ctx.db.as_ref() else {
            panic!("test uses sqlite");
        };
        sqlx::query("UPDATE file SET storage_backend = 's3'")
            .execute(pool)
            .await
            .unwrap();

        let err = storage
            .write_file("moved/data.bin", b"new", None, &ctx)
            .await
            .unwrap_err();
        assert!(err.contains("'s3'"), "{err}");
        assert!(
            storage
                .write_file_if("moved/data.bin", b"new", None, v1.etag.as_deref(), &ctx)
                .await
                .is_err()
        );
        let id = storage
            .initiate_multipart_upload("moved/data.bin", None, &ctx)
            .await
            .unwrap();
        let etag = storage
            .upload_part(&id, "moved/data.bin", 1, b"abc", &ctx)
            .await
            .unwrap();
        assert!(
            storage
                .complete_multipart_upload(
                    &id,
                    "moved/data.bin",
                    &[(1, etag)],
                    3,
                    None,
                    &CommitBase::Any,
                    &ctx
                )
                .await
                .is_err()
        );

        // Nothing was rewritten, and the record still has its own backend.
        let record = get_file_by_path(&ctx.db, "moved/data.bin", ctx.org_id, ctx.env_id)
            .await
            .unwrap();
        assert_eq!(record.storage_backend, "s3");
        assert_eq!(record.storage_path, v1.storage_path);
        assert_eq!(
            tokio::fs::read(&v1.storage_path).await.unwrap(),
            b"original"
        );
    }

    #[tokio::test]
    async fn test_failed_multipart_assembly_keeps_live_file() {
        let (storage, ctx, _temp_dir) = setup_test_storage().await;
        storage
            .write_file("data/live.bin", b"live-bytes", None, &ctx)
            .await
            .unwrap();

        let id = storage
            .initiate_multipart_upload("data/live.bin", None, &ctx)
            .await
            .unwrap();
        let e1 = storage
            .upload_part(&id, "data/live.bin", 1, b"new-", &ctx)
            .await
            .unwrap();
        // Part 2 is listed in the manifest but was never uploaded.
        let manifest = vec![(1, e1.clone()), (2, "missing".to_string())];
        assert!(
            storage
                .complete_multipart_upload(
                    &id,
                    "data/live.bin",
                    &manifest,
                    8,
                    None,
                    &CommitBase::Any,
                    &ctx
                )
                .await
                .is_err()
        );
        assert_eq!(
            storage.read_file("data/live.bin", &ctx).await.unwrap(),
            b"live-bytes"
        );
        let meta = storage
            .get_file_metadata("data/live.bin", &ctx)
            .await
            .unwrap();
        assert_eq!(meta.size, 10);

        // Parts survive the failure, so completion can be retried.
        let e2 = storage
            .upload_part(&id, "data/live.bin", 2, b"data", &ctx)
            .await
            .unwrap();
        let done = storage
            .complete_multipart_upload(
                &id,
                "data/live.bin",
                &[(1, e1), (2, e2)],
                8,
                None,
                &CommitBase::Any,
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(done.etag, Some(compute_md5(b"new-data")));
        assert_eq!(
            storage.read_file("data/live.bin", &ctx).await.unwrap(),
            b"new-data"
        );
    }

    #[cfg(feature = "s3-storage")]
    #[tokio::test]
    async fn test_s3_multipart_ids_target_unique_keys_and_keep_legacy_ids() {
        let config = aws_sdk_s3::Config::builder()
            .behavior_version(aws_config::BehaviorVersion::latest())
            .region(aws_config::Region::new("us-east-1"))
            .build();
        let storage = s3::S3FileStorage::new(
            aws_sdk_s3::Client::from_conf(config),
            "bucket".to_string(),
            Some("files".to_string()),
        );
        let db = std::sync::Arc::new(crate::db::DatabasePool::Sqlite(
            sqlx::sqlite::SqlitePoolOptions::new()
                .connect_lazy("sqlite::memory:")
                .unwrap(),
        ));
        let ctx = FileStorageContext::minimal(db, Uuid::nil(), None, Uuid::nil());
        let org = Uuid::nil().simple().to_string();

        let (key, upload) = storage.multipart_target("hot1:abc123:S3-UPLOAD.id", "a/b.bin", &ctx);
        assert_eq!(key, format!("files/{org}/default/a/b.bin.mp-abc123"));
        assert_eq!(upload, "S3-UPLOAD.id");

        // Uploads started before the encoded format keep the old key.
        let (key, upload) = storage.multipart_target("LEGACY-ID", "a/b.bin", &ctx);
        assert_eq!(key, format!("files/{org}/default/a/b.bin"));
        assert_eq!(upload, "LEGACY-ID");
    }

    /// Storage contract shared by every backend. Local always runs; GCS runs
    /// against a real bucket when `HOT_TEST_GCS_BUCKET` is set (credentials
    /// from Application Default Credentials / workload identity). Emulators
    /// do not qualify IAM, composition, or generation semantics.
    mod contract {
        use super::*;
        use tokio::io::AsyncReadExt;

        async fn assert_write_read_overwrite_delete(
            storage: &dyn FileStorage,
            ctx: &FileStorageContext,
        ) {
            let v1 = storage
                .write_file("contract/a.json", b"{\"v\":1}", None, ctx)
                .await
                .unwrap();
            assert_eq!(v1.size, 7);
            assert_eq!(v1.etag.as_deref(), Some(compute_md5(b"{\"v\":1}").as_str()));
            assert_eq!(v1.content_type.as_deref(), Some("application/json"));
            assert_eq!(v1.storage_backend, storage.storage_type());

            let v2 = storage
                .write_file("contract/a.json", b"{\"v\":22}", Some("text/plain"), ctx)
                .await
                .unwrap();
            assert_eq!(v2.file_id, v1.file_id);
            assert_eq!(v2.size, 8);
            assert_eq!(
                storage.read_file("contract/a.json", ctx).await.unwrap(),
                b"{\"v\":22}"
            );
            let meta = storage
                .get_file_metadata("contract/a.json", ctx)
                .await
                .unwrap();
            assert_eq!(meta.content_type.as_deref(), Some("text/plain"));

            let mut stream = storage
                .open_file_stream("contract/a.json", ctx)
                .await
                .unwrap()
                .expect("backend supports streaming");
            let mut streamed = Vec::new();
            stream.reader.read_to_end(&mut streamed).await.unwrap();
            assert_eq!(streamed, b"{\"v\":22}");

            assert!(storage.delete_file("contract/a.json", ctx).await.unwrap());
            assert!(!storage.file_exists("contract/a.json", ctx).await.unwrap());
            assert!(storage.read_file("contract/a.json", ctx).await.is_err());
            assert!(!storage.delete_file("contract/a.json", ctx).await.unwrap());

            // Both versions were retired, and collection deletes them once.
            assert!(collect_retired(storage, ctx).await >= 2);
            assert_eq!(collect_retired(storage, ctx).await, 0);
        }

        async fn assert_env_isolation(storage: &dyn FileStorage, ctx: &FileStorageContext) {
            let mut other = ctx.clone();
            other.env_id = Some(Uuid::now_v7());
            storage
                .write_file("iso/x.txt", b"mine", None, ctx)
                .await
                .unwrap();
            assert!(storage.read_file("iso/x.txt", &other).await.is_err());
            assert!(storage.list_files("iso/", &other).await.unwrap().is_empty());
            assert_eq!(
                storage
                    .list_files_bounded("iso/", 1, ctx)
                    .await
                    .unwrap()
                    .len(),
                1
            );
            storage.delete_file("iso/x.txt", ctx).await.unwrap();
        }

        async fn assert_conditional_writes(storage: &dyn FileStorage, ctx: &FileStorageContext) {
            let v1 = storage
                .write_file_if("cas/app.db", b"one", None, None, ctx)
                .await
                .unwrap()
                .expect("create wins");
            assert!(
                storage
                    .write_file_if("cas/app.db", b"intruder", None, None, ctx)
                    .await
                    .unwrap()
                    .is_none()
            );
            let v2 = storage
                .write_file_if("cas/app.db", b"two", None, v1.etag.as_deref(), ctx)
                .await
                .unwrap()
                .expect("matching etag wins");
            assert_ne!(v2.storage_path, v1.storage_path);
            assert!(
                storage
                    .write_file_if("cas/app.db", b"stale", None, v1.etag.as_deref(), ctx)
                    .await
                    .unwrap()
                    .is_none()
            );
            assert_eq!(storage.read_file("cas/app.db", ctx).await.unwrap(), b"two");

            // Racing writers with the same expectation: exactly one wins and
            // the committed bytes match the winner.
            let (a, b) = tokio::join!(
                storage.write_file_if("cas/app.db", b"racer-a", None, v2.etag.as_deref(), ctx),
                storage.write_file_if("cas/app.db", b"racer-b", None, v2.etag.as_deref(), ctx),
            );
            let winners: Vec<_> = [a.unwrap(), b.unwrap()].into_iter().flatten().collect();
            assert_eq!(winners.len(), 1);
            let committed = storage.read_file("cas/app.db", ctx).await.unwrap();
            assert_eq!(Some(compute_md5(&committed)), winners[0].etag);
            storage.delete_file("cas/app.db", ctx).await.unwrap();
        }

        async fn assert_blob_bytes(storage: &dyn FileStorage, ctx: &FileStorageContext) {
            let key = format!("blobs/blake3/ab/{}", Uuid::new_v4().simple());
            storage
                .write_blob_bytes(&key, b"blob", None, ctx)
                .await
                .unwrap();
            // Idempotent for content-addressed keys.
            storage
                .write_blob_bytes(&key, b"blob", None, ctx)
                .await
                .unwrap();
            assert_eq!(storage.read_blob_bytes(&key, ctx).await.unwrap(), b"blob");
            // Blob bytes never create user-visible file records.
            assert!(storage.list_files("blobs", ctx).await.unwrap().is_empty());
            storage.delete_blob_bytes(&key, ctx).await.unwrap();
            storage.delete_blob_bytes(&key, ctx).await.unwrap();
            assert!(storage.read_blob_bytes(&key, ctx).await.is_err());
        }

        async fn assert_multipart(storage: &dyn FileStorage, ctx: &FileStorageContext, parts: i32) {
            let path = format!("mp/{}.bin", Uuid::new_v4().simple());
            let id = storage
                .initiate_multipart_upload(&path, Some("application/octet-stream"), ctx)
                .await
                .unwrap();
            // Upload out of order; part 1 is replaced and only the second
            // token is selected by the manifest.
            let mut manifest = Vec::new();
            for n in (1..=parts).rev() {
                if n == 1 {
                    storage
                        .upload_part(&id, &path, 1, b"old", ctx)
                        .await
                        .unwrap();
                }
                let token = storage
                    .upload_part(&id, &path, n, &[n as u8; 3], ctx)
                    .await
                    .unwrap();
                manifest.push((n, token));
            }
            manifest.reverse();
            let expected: Vec<u8> = (1..=parts).flat_map(|n| [n as u8; 3]).collect();
            let meta = storage
                .complete_multipart_upload(
                    &id,
                    &path,
                    &manifest,
                    expected.len() as i64,
                    None,
                    &CommitBase::Any,
                    ctx,
                )
                .await
                .unwrap();
            assert_eq!(meta.size, expected.len() as i64);
            assert_eq!(storage.read_file(&path, ctx).await.unwrap(), expected);
            storage.delete_file(&path, ctx).await.unwrap();
        }

        async fn assert_multipart_abort(storage: &dyn FileStorage, ctx: &FileStorageContext) {
            let path = "mp/aborted.bin";
            let id = storage
                .initiate_multipart_upload(path, None, ctx)
                .await
                .unwrap();
            let token = storage
                .upload_part(&id, path, 1, b"abc", ctx)
                .await
                .unwrap();
            storage
                .abort_multipart_upload(&id, path, ctx)
                .await
                .unwrap();
            assert!(
                storage
                    .complete_multipart_upload(
                        &id,
                        path,
                        &[(1, token)],
                        3,
                        None,
                        &CommitBase::Any,
                        ctx
                    )
                    .await
                    .is_err()
            );
            assert!(!storage.file_exists(path, ctx).await.unwrap());
        }

        #[tokio::test]
        async fn local_contract() {
            let (storage, ctx, _dir) = setup_test_storage().await;
            assert_write_read_overwrite_delete(&storage, &ctx).await;
            assert_env_isolation(&storage, &ctx).await;
            assert_conditional_writes(&storage, &ctx).await;
            assert_blob_bytes(&storage, &ctx).await;
            assert_multipart(&storage, &ctx, 3).await;
            assert_multipart_abort(&storage, &ctx).await;
        }

        #[test]
        fn backend_names_are_strict() {
            assert_eq!(
                parse_storage_backend_name("", "t").unwrap(),
                FileStorageType::Local
            );
            assert_eq!(
                parse_storage_backend_name("GCS", "t").unwrap(),
                FileStorageType::Gcs
            );
            assert_eq!(
                parse_storage_backend_name("s3", "t").unwrap(),
                FileStorageType::S3
            );
            assert!(parse_storage_backend_name("gcp", "t").is_err());
            assert!(parse_storage_backend_name("filesystem", "t").is_err());
        }

        #[test]
        fn foreign_backend_records_are_refused() {
            assert!(ensure_record_backend("gcs", "gcs", "a").is_ok());
            let err = ensure_record_backend("s3", "gcs", "a").unwrap_err();
            assert!(err.contains("'s3'") && err.contains("'gcs'"));
        }

        #[cfg(feature = "gcs-storage")]
        async fn gcs_storage() -> Option<gcs::GcsFileStorage> {
            let bucket = std::env::var("HOT_TEST_GCS_BUCKET").ok()?;
            let prefix = format!("hot-contract-tests/{}", Uuid::new_v4().simple());
            let conf = crate::val!({"file": {"gcs": {"bucket": bucket, "prefix": prefix}}});
            let config = crate::gcs::GcsConfig::resolve(&conf, "file", |_| None).unwrap();
            let client = crate::gcs::GcsClient::connect(&config).await.unwrap();
            Some(gcs::GcsFileStorage::new(client))
        }

        #[cfg(feature = "gcs-storage")]
        #[tokio::test]
        async fn gcs_contract_against_real_bucket() {
            let Some(storage) = gcs_storage().await else {
                eprintln!("skipping: HOT_TEST_GCS_BUCKET not set");
                return;
            };
            let (_local, ctx, _dir) = setup_test_storage().await;
            assert_write_read_overwrite_delete(&storage, &ctx).await;
            assert_env_isolation(&storage, &ctx).await;
            assert_conditional_writes(&storage, &ctx).await;
            assert_blob_bytes(&storage, &ctx).await;
            // More than 32 parts exercises staged composition.
            assert_multipart(&storage, &ctx, 70).await;
            assert_multipart_abort(&storage, &ctx).await;
        }

        #[cfg(feature = "gcs-storage")]
        #[tokio::test]
        async fn gcs_multipart_completion_is_retryable_and_validates_manifest() {
            let Some(storage) = gcs_storage().await else {
                eprintln!("skipping: HOT_TEST_GCS_BUCKET not set");
                return;
            };
            let (_local, ctx, _dir) = setup_test_storage().await;
            let path = "mp/retry.bin";
            let id = storage
                .initiate_multipart_upload(path, None, &ctx)
                .await
                .unwrap();
            let t1 = storage
                .upload_part(&id, path, 1, b"aa", &ctx)
                .await
                .unwrap();
            let t2 = storage
                .upload_part(&id, path, 2, b"bb", &ctx)
                .await
                .unwrap();

            // Invalid manifests are rejected before any composition.
            let bogus = [(1, "bogus".to_string())];
            assert!(
                storage
                    .complete_multipart_upload(&id, path, &bogus, 2, None, &CommitBase::Any, &ctx)
                    .await
                    .is_err()
            );
            let duplicate = [(1, t1.clone()), (1, t2.clone())];
            assert!(
                storage
                    .complete_multipart_upload(
                        &id,
                        path,
                        &duplicate,
                        4,
                        None,
                        &CommitBase::Any,
                        &ctx
                    )
                    .await
                    .is_err()
            );

            let manifest = [(1, t1), (2, t2)];
            let first = storage
                .complete_multipart_upload(&id, path, &manifest, 4, None, &CommitBase::Any, &ctx)
                .await
                .unwrap();
            // A retried completion (lost response, restarted instance) finds
            // the composed object even though staging was cleaned up.
            let retried = storage
                .complete_multipart_upload(&id, path, &manifest, 4, None, &CommitBase::Any, &ctx)
                .await
                .unwrap();
            assert_eq!(first.storage_path, retried.storage_path);
            assert_eq!(storage.read_file(path, &ctx).await.unwrap(), b"aabb");
            storage.delete_file(path, &ctx).await.unwrap();
        }
    }
}

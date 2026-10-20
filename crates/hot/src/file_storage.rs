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
    FileRecord, get_file_by_path, insert_file_record, mark_file_inactive, update_file_record,
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
    async fn complete_multipart_upload(
        &self,
        _backend_upload_id: &str,
        _path: &str,
        _parts: &[(i32, String)],
        _total_size: i64,
        _content_type: Option<&str>,
        _ctx: &FileStorageContext,
    ) -> Result<FileMetadata, String> {
        Err("Multipart upload not supported by this storage backend".to_string())
    }

    /// Abort a multipart upload and clean up any uploaded parts.
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
}

// ============================================================================
// Path Utilities
// ============================================================================

/// Normalize a file path to prevent directory traversal
/// Quota checks shared by the conditional write path (mirrors write_file).
async fn check_write_quota(ctx: &FileStorageContext, content_len: i64) -> Result<(), String> {
    use crate::db::Features;
    use crate::db::file::get_storage_usage_by_org;

    let features = Features::resolve_for_org(&ctx.db, &ctx.org_id).await;
    let max_file_bytes = ctx.effective_file_max_bytes(features.file_upload_max_bytes());
    if content_len > max_file_bytes {
        return Err(format!(
            "File size {} bytes exceeds maximum allowed {} bytes",
            content_len, max_file_bytes
        ));
    }
    let storage_limit = features.storage_bytes();
    if storage_limit >= 0 {
        let current_usage = get_storage_usage_by_org(&ctx.db, ctx.org_id)
            .await
            .unwrap_or(0);
        if current_usage + content_len > storage_limit {
            return Err(format!(
                "Storage quota exceeded: current usage {} + file {} > limit {}",
                current_usage, content_len, storage_limit
            ));
        }
    }
    Ok(())
}

/// Unique per-attempt relative storage location for a conditional write.
/// Each attempt stages its bytes at its own key and then swaps the record's
/// storage_path to reference them, so no two attempts ever share a blob and
/// a loser's cleanup can never delete a winner's bytes.
fn cas_blob_rel_path(normalized_path: &str) -> String {
    format!("{}.cas-{}", normalized_path, Uuid::new_v4().simple())
}

/// The record compare-and-swap behind write_file_if: conditional update when
/// an etag is expected, plain insert (racing on the partial unique index
/// over active (org_id, env_id, path)) when the file must not exist yet.
///
/// The blob bytes must already be durably written at `storage_path` when
/// this runs — the record swap is the commit point, and a reader may follow
/// the new storage_path the instant the swap lands.
///
/// Ok(None) = the CAS lost. On a win, the second tuple element carries the
/// storage path the record previously referenced (now unreachable) so the
/// caller can clean up the superseded blob.
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
) -> Result<Option<(FileMetadata, Option<String>)>, String> {
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
            // Accurate when the update below wins: a win means the record was
            // untouched between this read and the swap (any interleaved write
            // would have changed the etag and made the swap lose).
            let previous_storage_path =
                (!existing.storage_path.is_empty()).then(|| existing.storage_path.clone());
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
            Ok(updated.map(|r| (FileMetadata::from_file_record(r), previous_storage_path)))
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
                Ok(rec) => Ok(Some((FileMetadata::from_file_record(rec), None))),
                // A concurrent creator won the unique index race: CAS lost.
                Err(msg)
                    if msg.contains("UNIQUE")
                        || msg.contains("unique")
                        || msg.contains("duplicate key") =>
                {
                    Ok(None)
                }
                Err(e) => Err(e),
            }
        }
    }
}

/// Commit an unconditional write whose bytes are already durable at
/// `storage_path`: update the active record for the path, or insert one.
/// A concurrent creator winning the unique index turns the insert into an
/// update, so the write still lands instead of failing.
///
/// On success the second element carries the storage path the record
/// previously referenced when it differs from `storage_path` (the caller
/// owns cleaning it up). Records owned by another backend are refused: an
/// update would leave `storage_backend` pointing at the old backend.
#[allow(clippy::too_many_arguments)]
async fn commit_file_record(
    backend: &str,
    storage_path: &str,
    normalized_path: &str,
    content_len: i64,
    etag: Option<&str>,
    content_type: Option<&str>,
    ctx: &FileStorageContext,
) -> Result<(FileMetadata, Option<String>), String> {
    if let Err(e) = ctx.ensure_run_row().await {
        tracing::debug!(
            run_id = ?ctx.run_id,
            path = %normalized_path,
            "File write: could not ensure run row before metadata insert: {}",
            e
        );
    }
    for _ in 0..3 {
        match get_file_by_path(&ctx.db, normalized_path, ctx.org_id, ctx.env_id).await {
            Ok(existing) => {
                ensure_record_backend(&existing.storage_backend, backend, normalized_path)?;
                let updated = update_file_record(
                    &ctx.db,
                    existing.file_id,
                    content_len,
                    etag,
                    content_type,
                    storage_path,
                    ctx.user_id,
                    ctx.run_id,
                )
                .await?;
                let previous = (!existing.storage_path.is_empty()
                    && existing.storage_path != storage_path)
                    .then_some(existing.storage_path);
                return Ok((FileMetadata::from_file_record(updated), previous));
            }
            Err(_) => match insert_file_record(
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
                Ok(rec) => return Ok((FileMetadata::from_file_record(rec), None)),
                // A concurrent creator won the unique index: update instead.
                Err(msg)
                    if msg.contains("UNIQUE")
                        || msg.contains("unique")
                        || msg.contains("duplicate key") => {}
                Err(e) => return Err(e),
            },
        }
    }
    Err(format!(
        "File record for {} kept changing during write; retry",
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
fn unique_write_rel_path(normalized_path: &str) -> String {
    format!("{}.v-{}", normalized_path, Uuid::new_v4().simple())
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

/// Compute MD5 hash of content
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
        let path =
            std::env::var("HOT_FILE_STORAGE_PATH").unwrap_or_else(|_| "./.hot/file".to_string());
        Self::new(PathBuf::from(path))
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
        for (part_number, _etag) in sorted_parts {
            validate_part_number(*part_number)?;
            let part_path = upload_dir.join(format!("part_{:05}", part_number));
            let part_data = tokio::fs::read(&part_path)
                .await
                .map_err(|e| format!("Failed to read part {}: {}", part_number, e))?;
            hasher.consume(&part_data);
            size += part_data.len() as i64;
            file.write_all(&part_data)
                .await
                .map_err(|e| format!("Failed to write part {}: {}", part_number, e))?;
        }
        file.sync_all()
            .await
            .map_err(|e| format!("Failed to flush assembled file: {}", e))?;
        Ok((format!("{:x}", hasher.finalize()), size))
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
        let staged_path = self.full_path(&unique_write_rel_path(&normalized_path), ctx);
        if let Some(parent) = staged_path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| format!("Failed to create parent directories: {}", e))?;
        }
        tokio::fs::write(&staged_path, content)
            .await
            .map_err(|e| format!("Failed to write file to storage: {}", e))?;
        let storage_path = staged_path.to_string_lossy().to_string();

        match commit_file_record(
            "local",
            &storage_path,
            &normalized_path,
            content.len() as i64,
            Some(&etag),
            content_type_final.as_deref(),
            ctx,
        )
        .await
        {
            Ok((meta, previous)) => {
                // The superseded file is unreachable now (best-effort).
                if let Some(previous) = previous {
                    let _ = tokio::fs::remove_file(&previous).await;
                }
                tracing::debug!("Wrote file {} to local storage", normalized_path);
                Ok(meta)
            }
            Err(e) => {
                let _ = tokio::fs::remove_file(&staged_path).await;
                Err(e)
            }
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
        let staged_path = self.full_path(&cas_blob_rel_path(&normalized_path), ctx);
        let storage_path = staged_path.to_string_lossy().to_string();
        if let Some(parent) = staged_path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| format!("Failed to create parent directories: {}", e))?;
        }
        tokio::fs::write(&staged_path, content)
            .await
            .map_err(|e| format!("Failed to write file to storage: {}", e))?;

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
            Ok(Some((meta, previous_storage_path))) => {
                // The record now references the staged bytes; the superseded
                // blob is unreachable and can go (best-effort).
                if let Some(old) = previous_storage_path
                    && old != storage_path
                {
                    let _ = tokio::fs::remove_file(&old).await;
                }
                Ok(Some(meta))
            }
            Ok(None) => {
                let _ = tokio::fs::remove_file(&staged_path).await;
                Ok(None)
            }
            Err(e) => {
                let _ = tokio::fs::remove_file(&staged_path).await;
                Err(e)
            }
        }
    }

    async fn read_file(&self, path: &str, ctx: &FileStorageContext) -> Result<Vec<u8>, String> {
        // Normalize and validate path
        let normalized_path = normalize_path(path)?;
        validate_path_security(&normalized_path)?;

        // Check database first for access control
        let record = get_file_by_path(&ctx.db, &normalized_path, ctx.org_id, ctx.env_id).await?;

        // The record's storage_path is authoritative — conditional writes
        // commit by swapping it to a staged blob key. Fall back to the
        // deterministic location for records that predate storage_path or
        // whose base directory moved.
        let recorded_path = (record.storage_backend == "local" && !record.storage_path.is_empty())
            .then(|| PathBuf::from(&record.storage_path));
        let full_path = match recorded_path {
            Some(p) if p.exists() => p,
            _ => self.full_path(&normalized_path, ctx),
        };

        if !full_path.exists() {
            return Err(format!("File not found in storage: {}", normalized_path));
        }

        tokio::fs::read(&full_path)
            .await
            .map_err(|e| format!("Failed to read file: {}", e))
    }

    async fn open_file_stream(
        &self,
        path: &str,
        ctx: &FileStorageContext,
    ) -> Result<Option<FileReadStream>, String> {
        let normalized_path = normalize_path(path)?;
        validate_path_security(&normalized_path)?;

        let record = get_file_by_path(&ctx.db, &normalized_path, ctx.org_id, ctx.env_id).await?;
        let recorded_path = (record.storage_backend == "local" && !record.storage_path.is_empty())
            .then(|| PathBuf::from(&record.storage_path));
        let full_path = match recorded_path {
            Some(path) if path.exists() => path,
            _ => self.full_path(&normalized_path, ctx),
        };
        let file = tokio::fs::File::open(&full_path)
            .await
            .map_err(|e| format!("Failed to open file for streaming: {}", e))?;

        Ok(Some(FileReadStream {
            metadata: FileMetadata::from_file_record(record),
            reader: Box::pin(file),
        }))
    }

    async fn delete_file(&self, path: &str, ctx: &FileStorageContext) -> Result<bool, String> {
        // Normalize and validate path
        let normalized_path = normalize_path(path)?;
        validate_path_security(&normalized_path)?;

        // Check if file exists in database first
        let record = match get_file_by_path(&ctx.db, &normalized_path, ctx.org_id, ctx.env_id).await
        {
            Ok(record) => record,
            Err(_) => return Ok(false), // File didn't exist
        };

        // Mark inactive in database
        mark_file_inactive(
            &ctx.db,
            &normalized_path,
            ctx.org_id,
            ctx.env_id,
            ctx.user_id,
            ctx.run_id,
        )
        .await?;

        // Delete from the filesystem: the recorded storage path (which may be
        // a conditional-write staged blob) plus the deterministic location.
        let full_path = self.full_path(&normalized_path, ctx);
        let mut targets = vec![full_path];
        if record.storage_backend == "local" && !record.storage_path.is_empty() {
            let recorded = PathBuf::from(&record.storage_path);
            if !targets.contains(&recorded) {
                targets.push(recorded);
            }
        }
        for target in targets {
            if target.exists() {
                tokio::fs::remove_file(&target)
                    .await
                    .map_err(|e| format!("Failed to delete file from storage: {}", e))?;
            }
        }

        tracing::debug!("Deleted file {} from local storage", normalized_path);
        Ok(true) // File existed and was deleted
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
        let part_path = self
            .base_path
            .join(".uploads")
            .join(backend_upload_id)
            .join(format!("part_{:05}", part_number));
        tokio::fs::write(&part_path, content)
            .await
            .map_err(|e| format!("Failed to write part {}: {}", part_number, e))?;
        Ok(compute_md5(content))
    }

    async fn complete_multipart_upload(
        &self,
        backend_upload_id: &str,
        path: &str,
        parts: &[(i32, String)],
        _total_size: i64,
        content_type: Option<&str>,
        ctx: &FileStorageContext,
    ) -> Result<FileMetadata, String> {
        let normalized_path = normalize_path(path)?;
        validate_path_security(&normalized_path)?;
        ensure_path_writable("local", &normalized_path, ctx).await?;

        let upload_dir = self.base_path.join(".uploads").join(backend_upload_id);
        let mut sorted_parts = parts.to_vec();
        sorted_parts.sort_by_key(|(num, _)| *num);

        // Assemble into a unique path, never the live file: the record swap
        // is the commit point, so a failed assembly or commit leaves the
        // current file and its metadata untouched.
        let staged_path = self.full_path(&unique_write_rel_path(&normalized_path), ctx);
        if let Some(parent) = staged_path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| format!("Failed to create parent directories: {}", e))?;
        }
        let assembled = Self::assemble_parts(&upload_dir, &sorted_parts, &staged_path).await;
        let (etag, actual_size) = match assembled {
            Ok(result) => result,
            Err(e) => {
                let _ = tokio::fs::remove_file(&staged_path).await;
                return Err(e);
            }
        };

        let content_type_final = content_type
            .map(|s| s.to_string())
            .or_else(|| detect_content_type(&normalized_path, &[]));
        let storage_path = staged_path.to_string_lossy().to_string();

        let (meta, previous) = match commit_file_record(
            "local",
            &storage_path,
            &normalized_path,
            actual_size,
            Some(&etag),
            content_type_final.as_deref(),
            ctx,
        )
        .await
        {
            Ok(committed) => committed,
            Err(e) => {
                // Parts are kept so the completion can be retried.
                let _ = tokio::fs::remove_file(&staged_path).await;
                return Err(e);
            }
        };
        if let Some(previous) = previous {
            let _ = tokio::fs::remove_file(&previous).await;
        }
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
        _path: &str,
        _ctx: &FileStorageContext,
    ) -> Result<(), String> {
        let upload_dir = self.base_path.join(".uploads").join(backend_upload_id);
        tokio::fs::remove_dir_all(&upload_dir)
            .await
            .map_err(|e| format!("Failed to clean up multipart upload: {}", e))?;
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

            let bucket = s3_conf.get_str("bucket");
            if bucket.is_empty() {
                return Err("file.aws.s3.bucket not configured".to_string());
            }

            let prefix_str = s3_conf.get_str("prefix");
            let prefix = if prefix_str.is_empty() {
                None
            } else {
                Some(prefix_str)
            };

            // Load AWS config
            let mut config_loader = aws_config::defaults(BehaviorVersion::latest());

            // Override region if specified in S3 config
            let s3_region = s3_conf.get_str("region");
            if !s3_region.is_empty() {
                config_loader = config_loader.region(aws_config::Region::new(s3_region));
            }

            // Check for custom file-specific AWS credentials from config
            // Prioritize: 1) Config values, 2) HOT_FILE_AWS_* env vars, 3) Standard AWS credential chain
            let access_key = aws_conf.get_str("access-key-id");
            let secret_key = aws_conf.get_str("secret-access-key");

            if !access_key.is_empty() && !secret_key.is_empty() {
                use aws_runtime::env_config::file::{EnvConfigFileKind, EnvConfigFiles};
                use aws_sdk_s3::config::Credentials;
                let credentials = Credentials::new(
                    access_key,
                    secret_key,
                    None, // session token
                    None, // expiry
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
                let rel = format!("{}.mp-{}", normalized_path, nonce);
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
            let key = self.s3_key(&unique_write_rel_path(&normalized_path), ctx);
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

            match commit_file_record(
                "s3",
                &storage_path,
                &normalized_path,
                content.len() as i64,
                Some(&s3_etag),
                content_type_final.as_deref(),
                ctx,
            )
            .await
            {
                Ok((meta, previous)) => {
                    // The superseded object is unreachable now (best-effort).
                    if let Some(previous) = previous {
                        self.delete_location_best_effort(&previous).await;
                    }
                    tracing::debug!("Wrote file {} to S3: {}", normalized_path, storage_path);
                    Ok(meta)
                }
                Err(e) => {
                    self.delete_location_best_effort(&storage_path).await;
                    Err(e)
                }
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
            let key = self.s3_key(&cas_blob_rel_path(&normalized_path), ctx);
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
                Ok(Some((meta, previous_storage_path))) => {
                    // The record now references the staged object; the
                    // superseded blob is unreachable and can go (best-effort).
                    if let Some(old) = previous_storage_path
                        && old != storage_path
                        && let Some((old_bucket, old_key)) = self.parse_storage_path(&old)
                    {
                        let _ = self
                            .client
                            .delete_object()
                            .bucket(&old_bucket)
                            .key(&old_key)
                            .send()
                            .await;
                    }
                    Ok(Some(meta))
                }
                lost_or_err => {
                    let _ = self
                        .client
                        .delete_object()
                        .bucket(&self.bucket)
                        .key(&key)
                        .send()
                        .await;
                    lost_or_err.map(|_| None)
                }
            }
        }
        async fn read_file(&self, path: &str, ctx: &FileStorageContext) -> Result<Vec<u8>, String> {
            // Normalize and validate path
            let normalized_path = normalize_path(path)?;
            validate_path_security(&normalized_path)?;

            // Check database first for access control
            let record =
                get_file_by_path(&ctx.db, &normalized_path, ctx.org_id, ctx.env_id).await?;

            // The record's storage_path is authoritative — conditional writes
            // commit by swapping it to a staged blob key. Fall back to the
            // deterministic key for records that predate storage_path.
            let (bucket, key) = if record.storage_backend == "s3" {
                self.parse_storage_path(&record.storage_path)
                    .unwrap_or_else(|| (self.bucket.clone(), self.s3_key(&normalized_path, ctx)))
            } else {
                (self.bucket.clone(), self.s3_key(&normalized_path, ctx))
            };

            let result = self
                .client
                .get_object()
                .bucket(&bucket)
                .key(&key)
                .send()
                .await;

            let response = match result {
                Ok(resp) => resp,
                Err(e) => {
                    // Extract detailed error information
                    let error_msg = format!(
                        "Failed to retrieve file from S3 (bucket: {}, key: {}, path: {}): {}",
                        bucket, key, normalized_path, e
                    );
                    tracing::error!("{}", error_msg);

                    // Log additional context if available
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

            let data = response
                .body
                .collect()
                .await
                .map_err(|e| {
                    let error_msg = format!(
                        "Failed to read S3 response body for file {} (bucket: {}, key: {}): {}",
                        normalized_path, bucket, key, e
                    );
                    tracing::error!("{}", error_msg);
                    error_msg
                })?
                .into_bytes()
                .to_vec();

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

            let record =
                get_file_by_path(&ctx.db, &normalized_path, ctx.org_id, ctx.env_id).await?;
            let (bucket, key) = if record.storage_backend == "s3" {
                self.parse_storage_path(&record.storage_path)
                    .unwrap_or_else(|| (self.bucket.clone(), self.s3_key(&normalized_path, ctx)))
            } else {
                (self.bucket.clone(), self.s3_key(&normalized_path, ctx))
            };
            let response = self
                .client
                .get_object()
                .bucket(&bucket)
                .key(&key)
                .send()
                .await
                .map_err(|e| {
                    format!(
                        "Failed to retrieve file from S3 (bucket: {}, key: {}, path: {}): {}",
                        bucket, key, normalized_path, e
                    )
                })?;

            Ok(Some(FileReadStream {
                metadata: FileMetadata::from_file_record(record),
                reader: Box::pin(response.body.into_async_read()),
            }))
        }

        async fn delete_file(&self, path: &str, ctx: &FileStorageContext) -> Result<bool, String> {
            // Normalize and validate path
            let normalized_path = normalize_path(path)?;
            validate_path_security(&normalized_path)?;

            // Check if file exists in database first
            let record =
                match get_file_by_path(&ctx.db, &normalized_path, ctx.org_id, ctx.env_id).await {
                    Ok(record) => record,
                    Err(_) => return Ok(false), // File didn't exist
                };

            // Mark inactive in database
            mark_file_inactive(
                &ctx.db,
                &normalized_path,
                ctx.org_id,
                ctx.env_id,
                ctx.user_id,
                ctx.run_id,
            )
            .await?;

            // Delete from S3 at the recorded storage path (which may be a
            // conditional-write staged key), falling back to the
            // deterministic key.
            let (bucket, key) = if record.storage_backend == "s3" {
                self.parse_storage_path(&record.storage_path)
                    .unwrap_or_else(|| (self.bucket.clone(), self.s3_key(&normalized_path, ctx)))
            } else {
                (self.bucket.clone(), self.s3_key(&normalized_path, ctx))
            };

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
            let key = self.s3_key(&format!("{}.mp-{}", normalized_path, nonce), ctx);

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
            ctx: &FileStorageContext,
        ) -> Result<FileMetadata, String> {
            let normalized_path = normalize_path(path)?;
            validate_path_security(&normalized_path)?;
            ensure_path_writable("s3", &normalized_path, ctx).await?;
            let (key, upload_id) = self.multipart_target(backend_upload_id, &normalized_path, ctx);

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

            self.client
                .complete_multipart_upload()
                .bucket(&self.bucket)
                .key(&key)
                .upload_id(upload_id)
                .multipart_upload(completed_upload)
                .send()
                .await
                .map_err(|e| format!("Failed to complete multipart upload: {}", e))?;

            let content_type_final = content_type
                .map(|s| s.to_string())
                .or_else(|| detect_content_type(&normalized_path, &[]));

            let storage_path = format!("s3://{}/{}", self.bucket, key);

            // The completed object lives at its own key; the record swap is
            // the commit point.
            let (meta, previous) = commit_file_record(
                "s3",
                &storage_path,
                &normalized_path,
                total_size,
                None,
                content_type_final.as_deref(),
                ctx,
            )
            .await?;
            if let Some(previous) = previous {
                self.delete_location_best_effort(&previous).await;
            }

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

            self.client
                .abort_multipart_upload()
                .bucket(&self.bucket)
                .key(&key)
                .upload_id(upload_id)
                .send()
                .await
                .map_err(|e| format!("Failed to abort multipart upload: {}", e))?;

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

        /// Best-effort delete of a location a record no longer references.
        async fn delete_superseded(&self, previous: Option<String>) {
            let Some(previous) = previous else { return };
            match self.client.key_from_uri(&previous) {
                Ok(key) => {
                    if let Err(e) = self.client.delete(&key, None).await {
                        tracing::warn!(error = %e, "Failed to delete superseded GCS object");
                    }
                }
                Err(e) => tracing::warn!(error = %e, "Not deleting superseded foreign object"),
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
                        self.client
                            .compose(&key, &group, None, Some(0), &[])
                            .await
                            .map(|obj| (obj.name, obj.generation))
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

            let key = self.object_key(&unique_write_rel_path(&normalized_path), ctx);
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

            match commit_file_record(
                BACKEND_ID,
                &storage_path,
                &normalized_path,
                content.len() as i64,
                Some(&etag),
                content_type_final.as_deref(),
                ctx,
            )
            .await
            {
                Ok((meta, previous)) => {
                    self.delete_superseded(previous).await;
                    tracing::debug!("Wrote file {} to GCS", normalized_path);
                    Ok(meta)
                }
                Err(e) => {
                    self.delete_key_best_effort(&key, Some(generation)).await;
                    Err(e)
                }
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
            let key = self.object_key(&cas_blob_rel_path(&normalized_path), ctx);
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
                Ok(Some((meta, previous))) => {
                    self.delete_superseded(previous.filter(|old| *old != storage_path))
                        .await;
                    Ok(Some(meta))
                }
                lost_or_err => {
                    // Only this attempt's generation is removed.
                    self.delete_key_best_effort(&key, Some(generation)).await;
                    lost_or_err.map(|_| None)
                }
            }
        }

        async fn read_file(&self, path: &str, ctx: &FileStorageContext) -> Result<Vec<u8>, String> {
            let normalized_path = normalize_path(path)?;
            validate_path_security(&normalized_path)?;
            let record =
                get_file_by_path(&ctx.db, &normalized_path, ctx.org_id, ctx.env_id).await?;
            let key = self.record_key(&record)?;
            self.client
                .get_bytes(&key, None)
                .await
                .map_err(|e| match e {
                    GcsError::NotFound(_) => {
                        format!("File not found in storage: {}", normalized_path)
                    }
                    e => format!("Failed to read file {} from GCS: {}", normalized_path, e),
                })
        }

        async fn open_file_stream(
            &self,
            path: &str,
            ctx: &FileStorageContext,
        ) -> Result<Option<FileReadStream>, String> {
            let normalized_path = normalize_path(path)?;
            validate_path_security(&normalized_path)?;
            let record =
                get_file_by_path(&ctx.db, &normalized_path, ctx.org_id, ctx.env_id).await?;
            let key = self.record_key(&record)?;
            let (reader, _size) =
                self.client.open_reader(&key).await.map_err(|e| {
                    format!("Failed to open file {} from GCS: {}", normalized_path, e)
                })?;
            Ok(Some(FileReadStream {
                metadata: FileMetadata::from_file_record(record),
                reader,
            }))
        }

        async fn delete_file(&self, path: &str, ctx: &FileStorageContext) -> Result<bool, String> {
            let normalized_path = normalize_path(path)?;
            validate_path_security(&normalized_path)?;
            let record =
                match get_file_by_path(&ctx.db, &normalized_path, ctx.org_id, ctx.env_id).await {
                    Ok(record) => record,
                    Err(_) => return Ok(false),
                };

            mark_file_inactive(
                &ctx.db,
                &normalized_path,
                ctx.org_id,
                ctx.env_id,
                ctx.user_id,
                ctx.run_id,
            )
            .await?;

            match self.record_key(&record) {
                Ok(key) => {
                    self.client.delete(&key, None).await.map_err(|e| {
                        format!(
                            "File {} was deleted but its GCS object could not be removed: {}",
                            normalized_path, e
                        )
                    })?;
                }
                // The record is gone either way; bytes owned by another
                // backend are left for the migration to reconcile.
                Err(e) => tracing::warn!("Deleted file record without removing bytes: {}", e),
            }
            tracing::debug!("Deleted file {} from GCS", normalized_path);
            Ok(true)
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
                &format!("{}.mp-{}", normalized_path, backend_upload_id),
                ctx,
            );

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

            let storage_path = self.client.uri(&dest);
            // Composite objects carry no MD5; like S3 multipart uploads the
            // record gets no application ETag until a later write sets one.
            let (meta, previous) = commit_file_record(
                BACKEND_ID,
                &storage_path,
                &normalized_path,
                composed_size,
                None,
                content_type_final.as_deref(),
                ctx,
            )
            .await?;
            self.delete_superseded(previous).await;
            if let Err(e) = self.client.delete_prefix(&staging).await {
                tracing::warn!(error = %e, "Failed to clean up multipart staging objects");
            }
            tracing::debug!("Completed multipart upload for {} to GCS", normalized_path);
            Ok(meta)
        }

        async fn abort_multipart_upload(
            &self,
            backend_upload_id: &str,
            _path: &str,
            ctx: &FileStorageContext,
        ) -> Result<(), String> {
            // Only staging objects are removed. A composed destination is
            // never deleted here: once completion has started the upload is
            // no longer abortable (see `file_upload::begin_completion`).
            let staging = self.staging_prefix(backend_upload_id, ctx);
            let failures = self
                .client
                .delete_prefix(&staging)
                .await
                .map_err(|e| format!("Failed to clean up multipart upload: {}", e))?;
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
            let path: String = if let Some(Val::Str(path_str)) = conf.get("file.storage.path") {
                (*path_str).to_string()
            } else {
                std::env::var("HOT_FILE_STORAGE_PATH").unwrap_or_else(|_| "./.hot/file".to_string())
            };
            Ok(Box::new(LocalFileStorage::new(PathBuf::from(path))))
        }
        #[cfg(feature = "s3-storage")]
        FileStorageType::S3 => {
            let storage = s3::S3FileStorage::from_config(conf).await?;
            Ok(Box::new(storage))
        }
        #[cfg(not(feature = "s3-storage"))]
        FileStorageType::S3 => {
            Err("S3 storage requested but s3-storage feature is not enabled".to_string())
        }
        #[cfg(feature = "gcs-storage")]
        FileStorageType::Gcs => Ok(Box::new(gcs::GcsFileStorage::from_config(conf).await?)),
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

        let db = crate::db::DatabasePool::Sqlite(pool);
        let db_arc = std::sync::Arc::new(db);

        let org_id = uuid::Uuid::now_v7();
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
        // staged blob is cleaned up.
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
        // The CAS blob is gone from disk too.
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
            .complete_multipart_upload(&upload_id, "data/overwrite.bin", &parts, 100, None, &ctx)
            .await
            .unwrap();

        let content = storage.read_file("data/overwrite.bin", &ctx).await.unwrap();
        assert_eq!(content, second_data);
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
    async fn test_overwrite_removes_superseded_bytes() {
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
        assert!(!Path::new(&v1.storage_path).exists());
        assert!(Path::new(&v2.storage_path).exists());
        assert!(storage.delete_file("docs/a.txt", &ctx).await.unwrap());
        assert!(!Path::new(&v2.storage_path).exists());
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
                .complete_multipart_upload(&id, "moved/data.bin", &[(1, etag)], 3, None, &ctx)
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
                .complete_multipart_upload(&id, "data/live.bin", &manifest, 8, None, &ctx)
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
            .complete_multipart_upload(&id, "data/live.bin", &[(1, e1), (2, e2)], 8, None, &ctx)
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
                .complete_multipart_upload(&id, &path, &manifest, expected.len() as i64, None, ctx)
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
                    .complete_multipart_upload(&id, path, &[(1, token)], 3, None, ctx)
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
                    .complete_multipart_upload(&id, path, &bogus, 2, None, &ctx)
                    .await
                    .is_err()
            );
            let duplicate = [(1, t1.clone()), (1, t2.clone())];
            assert!(
                storage
                    .complete_multipart_upload(&id, path, &duplicate, 4, None, &ctx)
                    .await
                    .is_err()
            );

            let manifest = [(1, t1), (2, t2)];
            let first = storage
                .complete_multipart_upload(&id, path, &manifest, 4, None, &ctx)
                .await
                .unwrap();
            // A retried completion (lost response, restarted instance) finds
            // the composed object even though staging was cleaned up.
            let retried = storage
                .complete_multipart_upload(&id, path, &manifest, 4, None, &ctx)
                .await
                .unwrap();
            assert_eq!(first.storage_path, retried.storage_path);
            assert_eq!(storage.read_file(path, &ctx).await.unwrap(), b"aabb");
            storage.delete_file(path, &ctx).await.unwrap();
        }
    }
}

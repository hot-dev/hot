//! File storage abstraction with database integration
//!
//! This module provides a pluggable storage backend system for Hot file operations.
//! Files can be stored locally on the filesystem, in AWS S3, or using other backends.
//! All file operations are tracked in the database for billing and audit purposes.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use indexmap::IndexMap;
use std::path::{Path, PathBuf};
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
    /// Config-level ceiling for file size (`hot.file.max-bytes`).
    /// Negative or None = no config ceiling, defer to plan limits.
    pub file_max_bytes_conf: Option<i64>,
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
            run_id: None,
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
            file_max_bytes_conf: None,
        }
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

    /// Read file from storage
    async fn read_file(&self, path: &str, ctx: &FileStorageContext) -> Result<Vec<u8>, String>;

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
}

// ============================================================================
// Path Utilities
// ============================================================================

/// Normalize a file path to prevent directory traversal
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
        // Step 1: Normalize and validate path
        let normalized_path = normalize_path(path)?;
        validate_path_security(&normalized_path)?;

        // Check storage quota before writing
        {
            use crate::db::Features;
            use crate::db::file::get_storage_usage_by_org;

            let features = Features::resolve_for_org(&ctx.db, &ctx.org_id).await;
            let max_file_bytes = ctx.effective_file_max_bytes(features.file_upload_max_bytes());
            let content_size = content.len() as i64;

            if content_size > max_file_bytes {
                return Err(format!(
                    "File size {} bytes exceeds maximum allowed {} bytes",
                    content_size, max_file_bytes
                ));
            }

            let storage_limit = features.storage_bytes();
            if storage_limit >= 0 {
                let current_usage = get_storage_usage_by_org(&ctx.db, ctx.org_id)
                    .await
                    .unwrap_or(0);
                if current_usage + content_size > storage_limit {
                    return Err(format!(
                        "Storage quota exceeded: current usage {} + file {} > limit {}",
                        current_usage, content_size, storage_limit
                    ));
                }
            }
        }

        // Step 2: Compute etag (MD5)
        let etag = compute_md5(content);

        // Step 3: Detect content type if not provided
        let content_type_final = content_type
            .map(|s| s.to_string())
            .or_else(|| detect_content_type(&normalized_path, content));

        // Step 4: Write to filesystem
        let full_path = self.full_path(&normalized_path, ctx);

        // Create parent directories if they don't exist
        if let Some(parent) = full_path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| format!("Failed to create parent directories: {}", e))?;
        }

        tokio::fs::write(&full_path, content)
            .await
            .map_err(|e| format!("Failed to write file to storage: {}", e))?;

        let storage_path = full_path.to_string_lossy().to_string();

        // Step 5: Create or update database record
        let file_metadata =
            match get_file_by_path(&ctx.db, &normalized_path, ctx.org_id, ctx.env_id).await {
                Ok(existing) => {
                    // File exists, update it
                    let updated = update_file_record(
                        &ctx.db,
                        existing.file_id,
                        content.len() as i64,
                        Some(&etag),
                        content_type_final.as_deref(),
                        &storage_path,
                        ctx.user_id,
                        ctx.run_id,
                    )
                    .await?;

                    FileMetadata::from_file_record(updated)
                }
                Err(_) => {
                    // New file, insert record
                    let inserted = insert_file_record(
                        &ctx.db,
                        &normalized_path,
                        content.len() as i64,
                        Some(&etag),
                        content_type_final.as_deref(),
                        "local",
                        &storage_path,
                        ctx.org_id,
                        ctx.env_id, // Added env_id for security
                        ctx.user_id,
                        ctx.run_id,
                    )
                    .await?;

                    FileMetadata::from_file_record(inserted)
                }
            };

        tracing::debug!("Wrote file {} to local storage", normalized_path);
        Ok(file_metadata)
    }

    async fn read_file(&self, path: &str, ctx: &FileStorageContext) -> Result<Vec<u8>, String> {
        // Normalize and validate path
        let normalized_path = normalize_path(path)?;
        validate_path_security(&normalized_path)?;

        // Check database first for access control
        get_file_by_path(&ctx.db, &normalized_path, ctx.org_id, ctx.env_id).await?;

        // Read from filesystem
        let full_path = self.full_path(&normalized_path, ctx);

        if !full_path.exists() {
            return Err(format!("File not found in storage: {}", normalized_path));
        }

        tokio::fs::read(&full_path)
            .await
            .map_err(|e| format!("Failed to read file: {}", e))
    }

    async fn delete_file(&self, path: &str, ctx: &FileStorageContext) -> Result<bool, String> {
        // Normalize and validate path
        let normalized_path = normalize_path(path)?;
        validate_path_security(&normalized_path)?;

        // Check if file exists in database first
        let file_existed =
            (get_file_by_path(&ctx.db, &normalized_path, ctx.org_id, ctx.env_id).await).is_ok();

        if !file_existed {
            return Ok(false); // File didn't exist
        }

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

        // Delete from filesystem
        let full_path = self.full_path(&normalized_path, ctx);

        if full_path.exists() {
            tokio::fs::remove_file(&full_path)
                .await
                .map_err(|e| format!("Failed to delete file from storage: {}", e))?;
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

        let full_path = self.full_path(&normalized_path, ctx);
        if let Some(parent) = full_path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| format!("Failed to create parent directories: {}", e))?;
        }

        let upload_dir = self.base_path.join(".uploads").join(backend_upload_id);

        let mut sorted_parts = parts.to_vec();
        sorted_parts.sort_by_key(|(num, _)| *num);

        // Assemble parts into the final file
        let mut file = tokio::fs::File::create(&full_path)
            .await
            .map_err(|e| format!("Failed to create output file: {}", e))?;

        for (part_number, _etag) in &sorted_parts {
            let part_path = upload_dir.join(format!("part_{:05}", part_number));
            let part_data = tokio::fs::read(&part_path)
                .await
                .map_err(|e| format!("Failed to read part {}: {}", part_number, e))?;
            tokio::io::AsyncWriteExt::write_all(&mut file, &part_data)
                .await
                .map_err(|e| format!("Failed to write part {}: {}", part_number, e))?;
        }
        tokio::io::AsyncWriteExt::shutdown(&mut file)
            .await
            .map_err(|e| format!("Failed to flush assembled file: {}", e))?;

        // Compute etag on the assembled file
        let assembled = tokio::fs::read(&full_path)
            .await
            .map_err(|e| format!("Failed to read assembled file: {}", e))?;
        let etag = compute_md5(&assembled);
        let actual_size = assembled.len() as i64;

        // Clean up temp directory
        let _ = tokio::fs::remove_dir_all(&upload_dir).await;

        let content_type_final = content_type
            .map(|s| s.to_string())
            .or_else(|| detect_content_type(&normalized_path, &[]));

        let storage_path = full_path.to_string_lossy().to_string();

        let file_metadata =
            match get_file_by_path(&ctx.db, &normalized_path, ctx.org_id, ctx.env_id).await {
                Ok(existing) => {
                    let updated = update_file_record(
                        &ctx.db,
                        existing.file_id,
                        actual_size,
                        Some(&etag),
                        content_type_final.as_deref(),
                        &storage_path,
                        ctx.user_id,
                        ctx.run_id,
                    )
                    .await?;
                    FileMetadata::from_file_record(updated)
                }
                Err(_) => {
                    let inserted = insert_file_record(
                        &ctx.db,
                        &normalized_path,
                        actual_size,
                        Some(&etag),
                        content_type_final.as_deref(),
                        "local",
                        &storage_path,
                        ctx.org_id,
                        ctx.env_id,
                        ctx.user_id,
                        ctx.run_id,
                    )
                    .await?;
                    FileMetadata::from_file_record(inserted)
                }
            };

        tracing::debug!(
            "Completed multipart upload for {} to local storage",
            normalized_path
        );
        Ok(file_metadata)
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
    }

    #[async_trait]
    impl FileStorage for S3FileStorage {
        async fn write_file(
            &self,
            path: &str,
            content: &[u8],
            content_type: Option<&str>,
            ctx: &FileStorageContext,
        ) -> Result<FileMetadata, String> {
            // Step 1: Normalize and validate path
            let normalized_path = normalize_path(path)?;
            validate_path_security(&normalized_path)?;

            // Check storage quota before writing
            {
                use crate::db::Features;
                use crate::db::file::get_storage_usage_by_org;

                let features = Features::resolve_for_org(&ctx.db, &ctx.org_id).await;
                let max_file_bytes = ctx.effective_file_max_bytes(features.file_upload_max_bytes());
                let content_size = content.len() as i64;

                if content_size > max_file_bytes {
                    return Err(format!(
                        "File size {} bytes exceeds maximum allowed {} bytes",
                        content_size, max_file_bytes
                    ));
                }

                let storage_limit = features.storage_bytes();
                if storage_limit >= 0 {
                    let current_usage = get_storage_usage_by_org(&ctx.db, ctx.org_id)
                        .await
                        .unwrap_or(0);
                    if current_usage + content_size > storage_limit {
                        return Err(format!(
                            "Storage quota exceeded: current usage {} + file {} > limit {}",
                            current_usage, content_size, storage_limit
                        ));
                    }
                }
            }

            // Step 2: Compute etag (MD5)
            let etag = compute_md5(content);

            // Step 3: Detect content type if not provided
            let content_type_final = content_type
                .map(|s| s.to_string())
                .or_else(|| detect_content_type(&normalized_path, content));

            // Step 4: Write to S3
            let key = self.s3_key(&normalized_path, ctx);
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

            let result = put_request.send().await;

            let response = match result {
                Ok(resp) => resp,
                Err(e) => {
                    // Extract detailed error information
                    let error_msg = format!(
                        "Failed to upload file to S3 (bucket: {}, key: {}, path: {}): {}",
                        self.bucket, key, normalized_path, e
                    );
                    tracing::error!("{}", error_msg);

                    // Log additional context if available
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

            // Step 5: Create or update database record
            let file_metadata =
                match get_file_by_path(&ctx.db, &normalized_path, ctx.org_id, ctx.env_id).await {
                    Ok(existing) => {
                        // File exists, update it
                        let updated = update_file_record(
                            &ctx.db,
                            existing.file_id,
                            content.len() as i64,
                            Some(&s3_etag),
                            content_type_final.as_deref(),
                            &storage_path,
                            ctx.user_id,
                            ctx.run_id,
                        )
                        .await?;

                        FileMetadata::from_file_record(updated)
                    }
                    Err(_) => {
                        // New file, insert record
                        let inserted = insert_file_record(
                            &ctx.db,
                            &normalized_path,
                            content.len() as i64,
                            Some(&s3_etag),
                            content_type_final.as_deref(),
                            "s3",
                            &storage_path,
                            ctx.org_id,
                            ctx.env_id, // Added env_id for security
                            ctx.user_id,
                            ctx.run_id,
                        )
                        .await?;

                        FileMetadata::from_file_record(inserted)
                    }
                };

            tracing::debug!("Wrote file {} to S3: {}", normalized_path, storage_path);
            Ok(file_metadata)
        }

        async fn read_file(&self, path: &str, ctx: &FileStorageContext) -> Result<Vec<u8>, String> {
            // Normalize and validate path
            let normalized_path = normalize_path(path)?;
            validate_path_security(&normalized_path)?;

            // Check database first for access control
            get_file_by_path(&ctx.db, &normalized_path, ctx.org_id, ctx.env_id).await?;

            // Read from S3
            let key = self.s3_key(&normalized_path, ctx);

            let result = self
                .client
                .get_object()
                .bucket(&self.bucket)
                .key(&key)
                .send()
                .await;

            let response = match result {
                Ok(resp) => resp,
                Err(e) => {
                    // Extract detailed error information
                    let error_msg = format!(
                        "Failed to retrieve file from S3 (bucket: {}, key: {}, path: {}): {}",
                        self.bucket, key, normalized_path, e
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
                        normalized_path, self.bucket, key, e
                    );
                    tracing::error!("{}", error_msg);
                    error_msg
                })?
                .into_bytes()
                .to_vec();

            tracing::info!("Read file {} from S3", normalized_path);
            Ok(data)
        }

        async fn delete_file(&self, path: &str, ctx: &FileStorageContext) -> Result<bool, String> {
            // Normalize and validate path
            let normalized_path = normalize_path(path)?;
            validate_path_security(&normalized_path)?;

            // Check if file exists in database first
            let file_existed =
                (get_file_by_path(&ctx.db, &normalized_path, ctx.org_id, ctx.env_id).await).is_ok();

            if !file_existed {
                return Ok(false); // File didn't exist
            }

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

            // Delete from S3
            let key = self.s3_key(&normalized_path, ctx);

            let result = self
                .client
                .delete_object()
                .bucket(&self.bucket)
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
                        self.bucket, key, normalized_path, e
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
            let key = self.s3_key(&normalized_path, ctx);

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
                .map(|id| id.to_string())
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
            let key = self.s3_key(&normalized_path, ctx);

            let resp = self
                .client
                .upload_part()
                .bucket(&self.bucket)
                .key(&key)
                .upload_id(backend_upload_id)
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
            let key = self.s3_key(&normalized_path, ctx);

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
                .upload_id(backend_upload_id)
                .multipart_upload(completed_upload)
                .send()
                .await
                .map_err(|e| format!("Failed to complete multipart upload: {}", e))?;

            let content_type_final = content_type
                .map(|s| s.to_string())
                .or_else(|| detect_content_type(&normalized_path, &[]));

            let storage_path = format!("s3://{}/{}", self.bucket, key);

            let file_metadata =
                match get_file_by_path(&ctx.db, &normalized_path, ctx.org_id, ctx.env_id).await {
                    Ok(existing) => {
                        let updated = update_file_record(
                            &ctx.db,
                            existing.file_id,
                            total_size,
                            None,
                            content_type_final.as_deref(),
                            &storage_path,
                            ctx.user_id,
                            ctx.run_id,
                        )
                        .await?;
                        FileMetadata::from_file_record(updated)
                    }
                    Err(_) => {
                        let inserted = insert_file_record(
                            &ctx.db,
                            &normalized_path,
                            total_size,
                            None,
                            content_type_final.as_deref(),
                            "s3",
                            &storage_path,
                            ctx.org_id,
                            ctx.env_id,
                            ctx.user_id,
                            ctx.run_id,
                        )
                        .await?;
                        FileMetadata::from_file_record(inserted)
                    }
                };

            tracing::debug!(
                "Completed multipart upload for {} to S3: {}",
                normalized_path,
                storage_path
            );
            Ok(file_metadata)
        }

        async fn abort_multipart_upload(
            &self,
            backend_upload_id: &str,
            path: &str,
            ctx: &FileStorageContext,
        ) -> Result<(), String> {
            let normalized_path = normalize_path(path)?;
            let key = self.s3_key(&normalized_path, ctx);

            self.client
                .abort_multipart_upload()
                .bucket(&self.bucket)
                .key(&key)
                .upload_id(backend_upload_id)
                .send()
                .await
                .map_err(|e| format!("Failed to abort multipart upload: {}", e))?;

            Ok(())
        }
    }
}

// ============================================================================
// Storage Factory
// ============================================================================

/// File storage type from configuration
#[derive(Debug, Clone, PartialEq)]
pub enum FileStorageType {
    Local,
    S3,
}

impl FileStorageType {
    pub fn from_env() -> Self {
        match std::env::var("HOT_FILE_STORAGE_TYPE")
            .unwrap_or_else(|_| "local".to_string())
            .to_lowercase()
            .as_str()
        {
            "s3" => FileStorageType::S3,
            _ => FileStorageType::Local,
        }
    }
}

/// Create a file storage instance from Hot configuration
pub async fn file_storage_from_config(conf: &Val) -> Result<Box<dyn FileStorage>, String> {
    let storage_type = FileStorageType::from_env();

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

        sqlx::query(
            "CREATE UNIQUE INDEX idx_file_org_env_path_active_unique ON file(org_id, env_id, path) WHERE active = 1",
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
}

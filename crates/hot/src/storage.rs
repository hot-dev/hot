//! Build storage abstraction
//!
//! This module provides a pluggable storage backend system for Hot build files.
//! Builds can be stored locally on the filesystem, in AWS S3, or using other backends.

use async_trait::async_trait;
use std::path::PathBuf;
use uuid::Uuid;

/// Get resolved configuration for AWS settings for build storage
pub fn get_aws_resolved_conf(conf: crate::val::Val) -> crate::val::Val {
    use crate::val;

    // Start with defaults
    let default_conf = val!({
        "region": "",
        "access-key-id": "",
        "secret-access-key": "",
        "s3": {
            "bucket": "",
            "prefix": "",
            "region": ""
        }
    });

    // Extract build.aws section from full config
    let build_section = conf.get("build").unwrap_or(crate::val::Val::map_empty());
    let aws_section = build_section
        .get("aws")
        .unwrap_or(crate::val::Val::map_empty());

    // Merge defaults with aws-specific config (config overrides defaults)
    default_conf.merge(&aws_section)
}

/// Trait for storing and retrieving build files
#[async_trait]
pub trait BuildStorage: Send + Sync {
    /// Store a build zip file, returns storage path/key
    async fn store_build(
        &self,
        build_id: &Uuid,
        org_id: &Uuid,
        env_id: &Uuid,
        data: Vec<u8>,
    ) -> Result<String, String>;

    /// Retrieve a build zip file as bytes
    async fn retrieve_build(
        &self,
        build_id: &Uuid,
        org_id: &Uuid,
        env_id: &Uuid,
    ) -> Result<Vec<u8>, String>;

    /// Check if build exists in storage
    async fn exists(&self, build_id: &Uuid, org_id: &Uuid, env_id: &Uuid) -> Result<bool, String>;

    /// Delete a build (for cleanup)
    async fn delete_build(
        &self,
        build_id: &Uuid,
        org_id: &Uuid,
        env_id: &Uuid,
    ) -> Result<(), String>;

    /// Get storage path/key for build
    fn build_path(&self, build_id: &Uuid, org_id: &Uuid, env_id: &Uuid) -> String;

    /// Get the storage backend type identifier
    fn storage_type(&self) -> &str;
}

/// Construct the standard zip filename from build metadata
/// Format: {build_id}.hot.zip (with dashes removed from UUID)
pub fn build_zip_filename(build_id: &Uuid) -> String {
    format!("{}.hot.zip", build_id.simple())
}

// ============================================================================
// Local Filesystem Storage
// ============================================================================

/// Local filesystem storage for builds
pub struct LocalBuildStorage {
    build_dir: PathBuf,
}

impl LocalBuildStorage {
    /// Create a new local build storage with the specified directory
    pub fn new(build_dir: PathBuf) -> Self {
        Self { build_dir }
    }

    /// Create from environment or use default
    pub fn from_env() -> Self {
        let build_dir =
            std::env::var("HOT_BUILD_STORAGE_PATH").unwrap_or_else(|_| ".hot/build".to_string());
        Self::new(PathBuf::from(build_dir))
    }
}

#[async_trait]
impl BuildStorage for LocalBuildStorage {
    async fn store_build(
        &self,
        build_id: &Uuid,
        org_id: &Uuid,
        env_id: &Uuid,
        data: Vec<u8>,
    ) -> Result<String, String> {
        // Create org/env directory structure
        let org_dir = self.build_dir.join(org_id.simple().to_string());
        let env_dir = org_dir.join(env_id.simple().to_string());

        if let Err(e) = std::fs::create_dir_all(&env_dir) {
            return Err(format!("Failed to create build directory: {}", e));
        }

        let filename = build_zip_filename(build_id);
        let path = env_dir.join(&filename);

        // Write the file
        if let Err(e) = std::fs::write(&path, data) {
            return Err(format!("Failed to write build file: {}", e));
        }

        tracing::info!("Stored build {} to {}", build_id, path.display());
        Ok(path.to_string_lossy().to_string())
    }

    async fn retrieve_build(
        &self,
        build_id: &Uuid,
        org_id: &Uuid,
        env_id: &Uuid,
    ) -> Result<Vec<u8>, String> {
        let filename = build_zip_filename(build_id);
        let path = self
            .build_dir
            .join(org_id.simple().to_string())
            .join(env_id.simple().to_string())
            .join(&filename);

        if !path.exists() {
            return Err(format!("Build file not found: {}", path.display()));
        }

        std::fs::read(&path).map_err(|e| format!("Failed to read build file: {}", e))
    }

    async fn exists(&self, build_id: &Uuid, org_id: &Uuid, env_id: &Uuid) -> Result<bool, String> {
        let filename = build_zip_filename(build_id);
        let path = self
            .build_dir
            .join(org_id.simple().to_string())
            .join(env_id.simple().to_string())
            .join(&filename);
        Ok(path.exists())
    }

    async fn delete_build(
        &self,
        build_id: &Uuid,
        org_id: &Uuid,
        env_id: &Uuid,
    ) -> Result<(), String> {
        let filename = build_zip_filename(build_id);
        let path = self
            .build_dir
            .join(org_id.simple().to_string())
            .join(env_id.simple().to_string())
            .join(&filename);

        if path.exists() {
            std::fs::remove_file(&path)
                .map_err(|e| format!("Failed to delete build file: {}", e))?;
            tracing::info!("Deleted build {} from local storage", build_id);
        }

        Ok(())
    }

    fn build_path(&self, build_id: &Uuid, org_id: &Uuid, env_id: &Uuid) -> String {
        let filename = build_zip_filename(build_id);
        self.build_dir
            .join(org_id.simple().to_string())
            .join(env_id.simple().to_string())
            .join(&filename)
            .to_string_lossy()
            .to_string()
    }

    fn storage_type(&self) -> &str {
        "local"
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

    /// AWS S3 storage for builds
    pub struct S3BuildStorage {
        client: S3Client,
        bucket: String,
        prefix: Option<String>,
    }

    impl S3BuildStorage {
        /// Create a new S3 build storage
        pub fn new(client: S3Client, bucket: String, prefix: Option<String>) -> Self {
            Self {
                client,
                bucket,
                prefix,
            }
        }

        /// Create from Hot configuration
        /// Required: hot.build.aws.s3.bucket
        /// Optional: hot.build.aws.s3.prefix, hot.build.aws.s3.region (or hot.build.aws.region)
        pub async fn from_config(conf: &crate::val::Val) -> Result<Self, String> {
            let build_conf = conf
                .get("build")
                .ok_or_else(|| "build configuration section not found".to_string())?;

            let aws_conf = build_conf
                .get("aws")
                .ok_or_else(|| "build.aws configuration section not found".to_string())?;

            let s3_conf = aws_conf
                .get("s3")
                .ok_or_else(|| "build.aws.s3 configuration section not found".to_string())?;

            let bucket = s3_conf.get_str("bucket");
            if bucket.is_empty() {
                return Err("build.aws.s3.bucket not configured".to_string());
            }

            let prefix_str = s3_conf.get_str("prefix");
            let prefix = if prefix_str.is_empty() {
                None
            } else {
                Some(prefix_str)
            };

            // Load AWS config (respects AWS_REGION, AWS_PROFILE, AWS_ACCESS_KEY_ID, AWS_SECRET_ACCESS_KEY, etc.)
            let mut config_loader = aws_config::defaults(BehaviorVersion::latest());

            // Override region if specified in S3 config
            let s3_region = s3_conf.get_str("region");
            if !s3_region.is_empty() {
                config_loader = config_loader.region(aws_config::Region::new(s3_region));
            }

            // Check for custom build-specific AWS credentials from config
            // Prioritize: 1) Config values, 2) HOT_BUILD_AWS_* env vars, 3) Standard AWS credential chain
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
                    "hot-build-config",
                );
                config_loader = config_loader
                    .credentials_provider(credentials)
                    .profile_files(
                        EnvConfigFiles::builder()
                            .with_contents(EnvConfigFileKind::Config, "")
                            .with_contents(EnvConfigFileKind::Credentials, "")
                            .build(),
                    );
                tracing::debug!("Using build-specific AWS credentials from configuration");
            } else {
                tracing::debug!("Using standard AWS credential chain for build storage");
            }

            let config = config_loader.load().await;
            let client = S3Client::new(&config);

            Ok(Self::new(client, bucket, prefix))
        }

        /// Construct the S3 key for a build
        /// Format: {prefix}/{org_id}/{env_id}/{build_id}.hot.zip
        /// Note: The prefix (e.g., "builds/") controls the top-level path structure
        fn s3_key(&self, build_id: &Uuid, org_id: &Uuid, env_id: &Uuid) -> String {
            let filename = build_zip_filename(build_id);
            let base_path = format!("{}/{}/{}", org_id.simple(), env_id.simple(), filename);

            if let Some(prefix) = &self.prefix {
                format!("{}/{}", prefix.trim_end_matches('/'), base_path)
            } else {
                base_path
            }
        }
    }

    #[async_trait]
    impl BuildStorage for S3BuildStorage {
        async fn store_build(
            &self,
            build_id: &Uuid,
            org_id: &Uuid,
            env_id: &Uuid,
            data: Vec<u8>,
        ) -> Result<String, String> {
            let key = self.s3_key(build_id, org_id, env_id);
            let byte_stream = ByteStream::from(data);

            let result = self
                .client
                .put_object()
                .bucket(&self.bucket)
                .key(&key)
                .body(byte_stream)
                .content_type("application/zip")
                .send()
                .await;

            match result {
                Ok(_) => {
                    tracing::info!(
                        "Stored build {} to S3: s3://{}/{}",
                        build_id,
                        self.bucket,
                        key
                    );
                    Ok(format!("s3://{}/{}", self.bucket, key))
                }
                Err(e) => {
                    // Extract detailed error information
                    let error_msg = format!(
                        "Failed to upload build to S3 (bucket: {}, key: {}): {}",
                        self.bucket, key, e
                    );
                    tracing::error!("{}", error_msg);

                    // Log additional context if available
                    if let Some(raw) = e.raw_response() {
                        tracing::error!(
                            "S3 upload error details - status: {:?}, headers: {:?}",
                            raw.status(),
                            raw.headers()
                        );
                    }

                    Err(error_msg)
                }
            }
        }

        async fn retrieve_build(
            &self,
            build_id: &Uuid,
            org_id: &Uuid,
            env_id: &Uuid,
        ) -> Result<Vec<u8>, String> {
            let key = self.s3_key(build_id, org_id, env_id);

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
                        "Failed to retrieve build from S3 (bucket: {}, key: {}, build_id: {}): {}",
                        self.bucket, key, build_id, e
                    );
                    tracing::error!("{}", error_msg);

                    // Log additional context if available
                    if let Some(raw) = e.raw_response() {
                        tracing::error!(
                            "S3 build download error details - status: {:?}, headers: {:?}",
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
                        "Failed to read S3 response body for build {} (bucket: {}, key: {}): {}",
                        build_id, self.bucket, key, e
                    );
                    tracing::error!("{}", error_msg);
                    error_msg
                })?
                .into_bytes()
                .to_vec();

            tracing::info!(
                "Retrieved build {} from S3: s3://{}/{}",
                build_id,
                self.bucket,
                key
            );
            Ok(data)
        }

        async fn exists(
            &self,
            build_id: &Uuid,
            org_id: &Uuid,
            env_id: &Uuid,
        ) -> Result<bool, String> {
            let key = self.s3_key(build_id, org_id, env_id);

            match self
                .client
                .head_object()
                .bucket(&self.bucket)
                .key(&key)
                .send()
                .await
            {
                Ok(_) => Ok(true),
                Err(e) => {
                    // Check if it's a NotFound error
                    let error_string = e.to_string();
                    if error_string.contains("NotFound") || error_string.contains("404") {
                        Ok(false)
                    } else {
                        Err(format!("Failed to check if build exists in S3: {}", e))
                    }
                }
            }
        }

        async fn delete_build(
            &self,
            build_id: &Uuid,
            org_id: &Uuid,
            env_id: &Uuid,
        ) -> Result<(), String> {
            let key = self.s3_key(build_id, org_id, env_id);

            self.client
                .delete_object()
                .bucket(&self.bucket)
                .key(&key)
                .send()
                .await
                .map_err(|e| format!("Failed to delete build from S3: {}", e))?;

            tracing::info!(
                "Deleted build {} from S3: s3://{}/{}",
                build_id,
                self.bucket,
                key
            );
            Ok(())
        }

        fn build_path(&self, build_id: &Uuid, org_id: &Uuid, env_id: &Uuid) -> String {
            format!(
                "s3://{}/{}",
                self.bucket,
                self.s3_key(build_id, org_id, env_id)
            )
        }

        fn storage_type(&self) -> &str {
            "s3"
        }
    }
}

// ============================================================================
// Storage Factory
// ============================================================================

/// Build storage type from configuration
#[derive(Debug, Clone, PartialEq)]
pub enum BuildStorageType {
    Local,
    S3,
}

impl BuildStorageType {
    pub fn from_env() -> Self {
        match std::env::var("HOT_BUILD_STORAGE_TYPE")
            .unwrap_or_else(|_| "local".to_string())
            .to_lowercase()
            .as_str()
        {
            "s3" => BuildStorageType::S3,
            _ => BuildStorageType::Local,
        }
    }
}

/// Create a build storage instance from Hot configuration
pub async fn build_storage_from_config(
    _conf: &crate::val::Val,
) -> Result<Box<dyn BuildStorage>, String> {
    let storage_type = BuildStorageType::from_env();

    match storage_type {
        BuildStorageType::Local => Ok(Box::new(LocalBuildStorage::from_env())),
        #[cfg(feature = "s3-storage")]
        BuildStorageType::S3 => {
            let storage = s3::S3BuildStorage::from_config(_conf).await?;
            Ok(Box::new(storage))
        }
        #[cfg(not(feature = "s3-storage"))]
        BuildStorageType::S3 => {
            Err("S3 storage requested but s3-storage feature is not enabled".to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_zip_filename() {
        let build_id = Uuid::parse_str("018c8d7a-1234-7890-abcd-ef0123456789").unwrap();

        let filename = build_zip_filename(&build_id);
        assert_eq!(filename, "018c8d7a12347890abcdef0123456789.hot.zip");
    }

    #[tokio::test]
    async fn test_local_storage() {
        let temp_dir = tempfile::tempdir().unwrap();
        let storage = LocalBuildStorage::new(temp_dir.path().to_path_buf());

        let build_id = Uuid::now_v7();
        let org_id = Uuid::now_v7();
        let env_id = Uuid::now_v7();
        let data = b"test build data".to_vec();

        // Store
        let path = storage
            .store_build(&build_id, &org_id, &env_id, data.clone())
            .await
            .unwrap();
        assert!(path.contains(&org_id.simple().to_string()));

        // Check exists
        let exists = storage.exists(&build_id, &org_id, &env_id).await.unwrap();
        assert!(exists);

        // Retrieve
        let retrieved = storage
            .retrieve_build(&build_id, &org_id, &env_id)
            .await
            .unwrap();
        assert_eq!(retrieved, data);

        // Delete
        storage
            .delete_build(&build_id, &org_id, &env_id)
            .await
            .unwrap();
        let exists_after = storage.exists(&build_id, &org_id, &env_id).await.unwrap();
        assert!(!exists_after);
    }
}

//! Service Key model for long-lived, permission-scoped credentials.
//!
//! Service keys are issued by API key holders to their customers and external systems
//! for scoped access to MCP tools, webhooks, and other API resources.
//! Not intended for browser sessions — use session tokens for that.
//!
//! Token format: `<service_key_id_hex_32>_<secret_hex_32>`

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, Pool, Postgres, Sqlite};
use std::error::Error;
use std::fmt;
use uuid::Uuid;

use crate::context_encryption::ContextEncryption;
use crate::permission::Permissions;

// ============================================================================
// Error Type
// ============================================================================

#[derive(Debug)]
pub enum ServiceKeyError {
    Database(sqlx::Error),
    NotFound,
    InvalidToken,
    Expired,
    Revoked,
    ParentKeyInactive,
    HashingError,
    SerializationError,
    EncryptionError,
    PermissionError(crate::permission::PermissionError),
}

impl fmt::Display for ServiceKeyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ServiceKeyError::Database(e) => write!(f, "Database error: {}", e),
            ServiceKeyError::NotFound => write!(f, "Service key not found"),
            ServiceKeyError::InvalidToken => write!(f, "Invalid service key token"),
            ServiceKeyError::Expired => write!(f, "Service key has expired"),
            ServiceKeyError::Revoked => write!(f, "Service key has been revoked"),
            ServiceKeyError::ParentKeyInactive => {
                write!(f, "Parent API key is no longer active")
            }
            ServiceKeyError::HashingError => write!(f, "Failed to hash service key secret"),
            ServiceKeyError::SerializationError => {
                write!(f, "Failed to serialize service key data")
            }
            ServiceKeyError::EncryptionError => {
                write!(f, "Failed to encrypt/decrypt service key metadata")
            }
            ServiceKeyError::PermissionError(e) => write!(f, "Permission error: {}", e),
        }
    }
}

impl Error for ServiceKeyError {}

impl From<sqlx::Error> for ServiceKeyError {
    fn from(error: sqlx::Error) -> Self {
        ServiceKeyError::Database(error)
    }
}

impl From<crate::permission::PermissionError> for ServiceKeyError {
    fn from(error: crate::permission::PermissionError) -> Self {
        ServiceKeyError::PermissionError(error)
    }
}

// ============================================================================
// ServiceKey Struct
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct ServiceKey {
    pub service_key_id: Uuid,
    pub api_key_id: Uuid,
    pub env_id: Uuid,
    pub name: Option<String>,
    pub description: Option<String>,
    #[sqlx(skip)]
    #[serde(skip)]
    pub secret_hash: Vec<u8>,
    pub permissions: serde_json::Value,
    /// Metadata stored as encrypted text (AES-256-GCM, same as context variables).
    pub metadata: Option<String>,
    pub expires_at: Option<DateTime<Utc>>,
    pub revoked_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub last_used_at: Option<DateTime<Utc>>,
}

/// Length of the random secret in bytes (produces 32 hex chars)
const SECRET_BYTES: usize = 16;

impl ServiceKey {
    // ========================================================================
    // Token generation and parsing
    // ========================================================================

    /// Generate a new service key token and return (full_token, secret_hash_bytes).
    ///
    /// Token format: `<service_key_id_hex_32>_<secret_hex_32>`
    /// No prefix — suitable for embedding in third-party systems.
    /// The secret portion is hashed with SHA-256 for storage.
    pub fn generate_token(service_key_id: &Uuid) -> Result<(String, Vec<u8>), ServiceKeyError> {
        use rand::RngCore;
        use sha2::{Digest, Sha256};

        // Generate random secret
        let mut secret_bytes = [0u8; SECRET_BYTES];
        rand::thread_rng().fill_bytes(&mut secret_bytes);
        let secret_hex = hex::encode(secret_bytes);

        // Hash the secret with SHA-256
        let mut hasher = Sha256::new();
        hasher.update(secret_hex.as_bytes());
        let hash = hasher.finalize().to_vec();

        // Build the token: no prefix, just uuid_secret
        let uuid_hex = service_key_id.to_string().replace("-", "");
        let token = format!("{}_{}", uuid_hex, secret_hex);

        Ok((token, hash))
    }

    /// Parse a service key token to extract the service key ID and secret hex.
    ///
    /// Expected format: `<uuid_hex_32>_<secret_hex_32>`
    /// Returns (service_key_id, secret_hex) or an error if the format is invalid.
    pub fn parse_token(token: &str) -> Result<(Uuid, String), ServiceKeyError> {
        let parts: Vec<&str> = token.split('_').collect();
        if parts.len() != 2 {
            return Err(ServiceKeyError::InvalidToken);
        }

        let uuid_hex = parts[0];
        let secret_hex = parts[1];

        // Validate UUID hex (32 hex characters)
        if uuid_hex.len() != 32 || !uuid_hex.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(ServiceKeyError::InvalidToken);
        }

        // Validate secret hex (32 hex characters)
        if secret_hex.len() != 32 || !secret_hex.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(ServiceKeyError::InvalidToken);
        }

        // Convert UUID hex to standard format with dashes
        let uuid_with_dashes = format!(
            "{}-{}-{}-{}-{}",
            &uuid_hex[0..8],
            &uuid_hex[8..12],
            &uuid_hex[12..16],
            &uuid_hex[16..20],
            &uuid_hex[20..32]
        );

        let service_key_id =
            Uuid::parse_str(&uuid_with_dashes).map_err(|_| ServiceKeyError::InvalidToken)?;

        Ok((service_key_id, secret_hex.to_string()))
    }

    /// Verify a secret hex against a stored SHA-256 hash.
    pub fn verify_secret(secret_hex: &str, stored_hash: &[u8]) -> bool {
        use sha2::{Digest, Sha256};

        let mut hasher = Sha256::new();
        hasher.update(secret_hex.as_bytes());
        let computed_hash = hasher.finalize();

        // Constant-time comparison
        computed_hash[..] == stored_hash[..]
    }

    /// Check if this service key is still valid (not expired, not revoked).
    pub fn is_valid(&self) -> bool {
        if self.revoked_at.is_some() {
            return false;
        }
        if let Some(expires_at) = self.expires_at {
            return expires_at > Utc::now();
        }
        true // No expiry = always valid (unless revoked)
    }

    /// Check if this service key has expired.
    pub fn is_expired(&self) -> bool {
        match self.expires_at {
            Some(expires_at) => expires_at <= Utc::now(),
            None => false, // No expiry
        }
    }

    /// Check if this service key has been revoked.
    pub fn is_revoked(&self) -> bool {
        self.revoked_at.is_some()
    }

    /// Get the permissions as a Permissions struct.
    pub fn get_permissions(&self) -> Result<Permissions, ServiceKeyError> {
        Permissions::from_json(&self.permissions).map_err(ServiceKeyError::PermissionError)
    }

    /// Check if this service key grants a specific action on a specific resource.
    pub fn has_permission(&self, resource: &str, action: &str) -> bool {
        match self.get_permissions() {
            Ok(perms) => perms.has_permission(resource, action),
            Err(_) => false,
        }
    }

    // ========================================================================
    // Metadata encryption
    // ========================================================================

    /// Encrypt a metadata JSON value for storage.
    pub fn encrypt_metadata(
        metadata: &serde_json::Value,
        encryption: &ContextEncryption,
        org_id: &Uuid,
    ) -> Result<String, ServiceKeyError> {
        let json_str =
            serde_json::to_string(metadata).map_err(|_| ServiceKeyError::SerializationError)?;
        encryption
            .encrypt(&json_str, org_id)
            .map_err(|_| ServiceKeyError::EncryptionError)
    }

    /// Decrypt stored metadata to a JSON value.
    pub fn decrypt_metadata(
        stored: &str,
        encryption: &ContextEncryption,
        org_id: &Uuid,
    ) -> Result<serde_json::Value, ServiceKeyError> {
        let json_str = encryption
            .decrypt(stored, org_id)
            .map_err(|_| ServiceKeyError::EncryptionError)?;
        serde_json::from_str(&json_str).map_err(|_| ServiceKeyError::SerializationError)
    }

    /// Get decrypted metadata as a JSON value, if present.
    pub fn get_decrypted_metadata(
        &self,
        encryption: &ContextEncryption,
        org_id: &Uuid,
    ) -> Result<Option<serde_json::Value>, ServiceKeyError> {
        match &self.metadata {
            Some(stored) => Ok(Some(Self::decrypt_metadata(stored, encryption, org_id)?)),
            None => Ok(None),
        }
    }

    // ========================================================================
    // Database operations
    // ========================================================================

    /// Create a new service key.
    /// Returns (ServiceKey, token_string). The token is only available at creation time.
    ///
    /// `metadata` should be pre-encrypted via `ServiceKey::encrypt_metadata()` when
    /// encryption is available, or passed as raw JSON string for local dev.
    #[allow(clippy::too_many_arguments)]
    pub async fn create(
        db: &crate::db::DatabasePool,
        api_key_id: &Uuid,
        env_id: &Uuid,
        name: Option<&str>,
        description: Option<&str>,
        permissions: &serde_json::Value,
        metadata: Option<&str>,
        expires_at: Option<DateTime<Utc>>,
    ) -> Result<(ServiceKey, String), ServiceKeyError> {
        let service_key_id = Uuid::now_v7();

        // Generate token and hash
        let (token, secret_hash) = Self::generate_token(&service_key_id)?;

        match db {
            crate::db::DatabasePool::Sqlite(db) => {
                Self::insert_sqlite(
                    db,
                    &service_key_id,
                    api_key_id,
                    env_id,
                    name,
                    description,
                    &secret_hash,
                    permissions,
                    metadata,
                    expires_at.as_ref(),
                )
                .await?;
            }
            crate::db::DatabasePool::Postgres(db) => {
                Self::insert_postgres(
                    db,
                    &service_key_id,
                    api_key_id,
                    env_id,
                    name,
                    description,
                    &secret_hash,
                    permissions,
                    metadata,
                    expires_at.as_ref(),
                )
                .await?;
            }
        }

        let service_key = ServiceKey {
            service_key_id,
            api_key_id: *api_key_id,
            env_id: *env_id,
            name: name.map(|s| s.to_string()),
            description: description.map(|s| s.to_string()),
            secret_hash,
            permissions: permissions.clone(),
            metadata: metadata.map(|s| s.to_string()),
            expires_at,
            revoked_at: None,
            created_at: Utc::now(),
            last_used_at: None,
        };

        Ok((service_key, token))
    }

    #[allow(clippy::too_many_arguments)]
    async fn insert_sqlite(
        db: &Pool<Sqlite>,
        service_key_id: &Uuid,
        api_key_id: &Uuid,
        env_id: &Uuid,
        name: Option<&str>,
        description: Option<&str>,
        secret_hash: &[u8],
        permissions: &serde_json::Value,
        metadata: Option<&str>,
        expires_at: Option<&DateTime<Utc>>,
    ) -> Result<(), ServiceKeyError> {
        let permissions_str =
            serde_json::to_string(permissions).map_err(|_| ServiceKeyError::SerializationError)?;

        sqlx::query(
            r#"
            INSERT INTO service_key (service_key_id, api_key_id, env_id, name, description,
                                    secret_hash, permissions, metadata, expires_at)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
            "#,
        )
        .bind(service_key_id)
        .bind(api_key_id)
        .bind(env_id)
        .bind(name)
        .bind(description)
        .bind(secret_hash)
        .bind(permissions_str)
        .bind(metadata)
        .bind(expires_at)
        .execute(db)
        .await?;

        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn insert_postgres(
        db: &Pool<Postgres>,
        service_key_id: &Uuid,
        api_key_id: &Uuid,
        env_id: &Uuid,
        name: Option<&str>,
        description: Option<&str>,
        secret_hash: &[u8],
        permissions: &serde_json::Value,
        metadata: Option<&str>,
        expires_at: Option<&DateTime<Utc>>,
    ) -> Result<(), ServiceKeyError> {
        sqlx::query(
            r#"
            INSERT INTO service_key (service_key_id, api_key_id, env_id, name, description,
                                    secret_hash, permissions, metadata, expires_at)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
            "#,
        )
        .bind(service_key_id)
        .bind(api_key_id)
        .bind(env_id)
        .bind(name)
        .bind(description)
        .bind(secret_hash)
        .bind(permissions)
        .bind(metadata)
        .bind(expires_at)
        .execute(db)
        .await?;

        Ok(())
    }

    /// Get a service key by ID (includes secret_hash for verification).
    pub async fn get_service_key(
        db: &crate::db::DatabasePool,
        service_key_id: &Uuid,
    ) -> Result<ServiceKey, ServiceKeyError> {
        match db {
            crate::db::DatabasePool::Sqlite(db) => {
                Self::get_service_key_sqlite(db, service_key_id).await
            }
            crate::db::DatabasePool::Postgres(db) => {
                Self::get_service_key_postgres(db, service_key_id).await
            }
        }
    }

    async fn get_service_key_sqlite(
        db: &Pool<Sqlite>,
        service_key_id: &Uuid,
    ) -> Result<ServiceKey, ServiceKeyError> {
        let row = sqlx::query_as::<_, SqliteServiceKeyRow>(
            r#"
            SELECT service_key_id, api_key_id, env_id, name, description, secret_hash,
                   permissions, metadata, expires_at, revoked_at, created_at, last_used_at
            FROM service_key
            WHERE service_key_id = ?
            "#,
        )
        .bind(service_key_id)
        .fetch_one(db)
        .await
        .map_err(|e| match e {
            sqlx::Error::RowNotFound => ServiceKeyError::NotFound,
            other => ServiceKeyError::Database(other),
        })?;

        row.into_service_key()
    }

    async fn get_service_key_postgres(
        db: &Pool<Postgres>,
        service_key_id: &Uuid,
    ) -> Result<ServiceKey, ServiceKeyError> {
        let row = sqlx::query_as::<_, PostgresServiceKeyRow>(
            r#"
            SELECT service_key_id, api_key_id, env_id, name, description, secret_hash,
                   permissions, metadata, expires_at, revoked_at, created_at, last_used_at
            FROM service_key
            WHERE service_key_id = $1
            "#,
        )
        .bind(service_key_id)
        .fetch_one(db)
        .await
        .map_err(|e| match e {
            sqlx::Error::RowNotFound => ServiceKeyError::NotFound,
            other => ServiceKeyError::Database(other),
        })?;

        row.into_service_key()
    }

    /// Verify a service key token and return the service key if valid.
    /// Checks: token format, existence, expiration, revocation, secret match.
    /// Does NOT check parent API key status (caller should do that separately).
    pub async fn verify_token(
        db: &crate::db::DatabasePool,
        token: &str,
    ) -> Result<ServiceKey, ServiceKeyError> {
        // Parse the token
        let (service_key_id, secret_hex) = Self::parse_token(token)?;

        // Look up the service key
        let service_key = Self::get_service_key(db, &service_key_id).await?;

        // Check expiration
        if service_key.is_expired() {
            return Err(ServiceKeyError::Expired);
        }

        // Check revocation
        if service_key.is_revoked() {
            return Err(ServiceKeyError::Revoked);
        }

        // Verify the secret
        if !Self::verify_secret(&secret_hex, &service_key.secret_hash) {
            return Err(ServiceKeyError::InvalidToken);
        }

        Ok(service_key)
    }

    /// List service keys for an API key (active, expired, and revoked).
    pub async fn list_by_api_key(
        db: &crate::db::DatabasePool,
        api_key_id: &Uuid,
        limit: Option<i64>,
        offset: Option<i64>,
    ) -> Result<Vec<ServiceKey>, ServiceKeyError> {
        match db {
            crate::db::DatabasePool::Sqlite(db) => {
                Self::list_by_api_key_sqlite(db, api_key_id, limit, offset).await
            }
            crate::db::DatabasePool::Postgres(db) => {
                Self::list_by_api_key_postgres(db, api_key_id, limit, offset).await
            }
        }
    }

    async fn list_by_api_key_sqlite(
        db: &Pool<Sqlite>,
        api_key_id: &Uuid,
        limit: Option<i64>,
        offset: Option<i64>,
    ) -> Result<Vec<ServiceKey>, ServiceKeyError> {
        let limit = limit.unwrap_or(50);
        let offset = offset.unwrap_or(0);

        let rows = sqlx::query_as::<_, SqliteServiceKeyRow>(
            r#"
            SELECT service_key_id, api_key_id, env_id, name, description, secret_hash,
                   permissions, metadata, expires_at, revoked_at, created_at, last_used_at
            FROM service_key
            WHERE api_key_id = ?
            ORDER BY created_at DESC
            LIMIT ? OFFSET ?
            "#,
        )
        .bind(api_key_id)
        .bind(limit)
        .bind(offset)
        .fetch_all(db)
        .await?;

        rows.into_iter().map(|r| r.into_service_key()).collect()
    }

    async fn list_by_api_key_postgres(
        db: &Pool<Postgres>,
        api_key_id: &Uuid,
        limit: Option<i64>,
        offset: Option<i64>,
    ) -> Result<Vec<ServiceKey>, ServiceKeyError> {
        let limit = limit.unwrap_or(50);
        let offset = offset.unwrap_or(0);

        let rows = sqlx::query_as::<_, PostgresServiceKeyRow>(
            r#"
            SELECT service_key_id, api_key_id, env_id, name, description, secret_hash,
                   permissions, metadata, expires_at, revoked_at, created_at, last_used_at
            FROM service_key
            WHERE api_key_id = $1
            ORDER BY created_at DESC
            LIMIT $2 OFFSET $3
            "#,
        )
        .bind(api_key_id)
        .bind(limit)
        .bind(offset)
        .fetch_all(db)
        .await?;

        rows.into_iter().map(|r| r.into_service_key()).collect()
    }

    /// List all service keys for an environment (active, revoked, expired).
    /// Used by the dashboard to show full history.
    pub async fn list_by_env(
        db: &crate::db::DatabasePool,
        env_id: &Uuid,
    ) -> Result<Vec<ServiceKey>, ServiceKeyError> {
        match db {
            crate::db::DatabasePool::Sqlite(db) => Self::list_by_env_sqlite(db, env_id).await,
            crate::db::DatabasePool::Postgres(db) => Self::list_by_env_postgres(db, env_id).await,
        }
    }

    async fn list_by_env_sqlite(
        db: &Pool<Sqlite>,
        env_id: &Uuid,
    ) -> Result<Vec<ServiceKey>, ServiceKeyError> {
        let rows = sqlx::query_as::<_, SqliteServiceKeyRow>(
            r#"
            SELECT service_key_id, api_key_id, env_id, name, description, secret_hash,
                   permissions, metadata, expires_at, revoked_at, created_at, last_used_at
            FROM service_key
            WHERE env_id = ?
            ORDER BY created_at DESC
            "#,
        )
        .bind(env_id)
        .fetch_all(db)
        .await?;

        rows.into_iter().map(|r| r.into_service_key()).collect()
    }

    async fn list_by_env_postgres(
        db: &Pool<Postgres>,
        env_id: &Uuid,
    ) -> Result<Vec<ServiceKey>, ServiceKeyError> {
        let rows = sqlx::query_as::<_, PostgresServiceKeyRow>(
            r#"
            SELECT service_key_id, api_key_id, env_id, name, description, secret_hash,
                   permissions, metadata, expires_at, revoked_at, created_at, last_used_at
            FROM service_key
            WHERE env_id = $1
            ORDER BY created_at DESC
            "#,
        )
        .bind(env_id)
        .fetch_all(db)
        .await?;

        rows.into_iter().map(|r| r.into_service_key()).collect()
    }

    /// List active (non-revoked, non-expired) service keys for an environment.
    pub async fn list_active_by_env(
        db: &crate::db::DatabasePool,
        env_id: &Uuid,
        limit: Option<i64>,
        offset: Option<i64>,
    ) -> Result<Vec<ServiceKey>, ServiceKeyError> {
        match db {
            crate::db::DatabasePool::Sqlite(db) => {
                Self::list_active_by_env_sqlite(db, env_id, limit, offset).await
            }
            crate::db::DatabasePool::Postgres(db) => {
                Self::list_active_by_env_postgres(db, env_id, limit, offset).await
            }
        }
    }

    async fn list_active_by_env_sqlite(
        db: &Pool<Sqlite>,
        env_id: &Uuid,
        limit: Option<i64>,
        offset: Option<i64>,
    ) -> Result<Vec<ServiceKey>, ServiceKeyError> {
        let limit = limit.unwrap_or(50);
        let offset = offset.unwrap_or(0);

        let rows = sqlx::query_as::<_, SqliteServiceKeyRow>(
            r#"
            SELECT service_key_id, api_key_id, env_id, name, description, secret_hash,
                   permissions, metadata, expires_at, revoked_at, created_at, last_used_at
            FROM service_key
            WHERE env_id = ?
              AND revoked_at IS NULL
              AND (expires_at IS NULL OR expires_at > datetime('now'))
            ORDER BY created_at DESC
            LIMIT ? OFFSET ?
            "#,
        )
        .bind(env_id)
        .bind(limit)
        .bind(offset)
        .fetch_all(db)
        .await?;

        rows.into_iter().map(|r| r.into_service_key()).collect()
    }

    async fn list_active_by_env_postgres(
        db: &Pool<Postgres>,
        env_id: &Uuid,
        limit: Option<i64>,
        offset: Option<i64>,
    ) -> Result<Vec<ServiceKey>, ServiceKeyError> {
        let limit = limit.unwrap_or(50);
        let offset = offset.unwrap_or(0);

        let rows = sqlx::query_as::<_, PostgresServiceKeyRow>(
            r#"
            SELECT service_key_id, api_key_id, env_id, name, description, secret_hash,
                   permissions, metadata, expires_at, revoked_at, created_at, last_used_at
            FROM service_key
            WHERE env_id = $1
              AND revoked_at IS NULL
              AND (expires_at IS NULL OR expires_at > now())
            ORDER BY created_at DESC
            LIMIT $2 OFFSET $3
            "#,
        )
        .bind(env_id)
        .bind(limit)
        .bind(offset)
        .fetch_all(db)
        .await?;

        rows.into_iter().map(|r| r.into_service_key()).collect()
    }

    /// Revoke a specific service key.
    pub async fn revoke(
        db: &crate::db::DatabasePool,
        service_key_id: &Uuid,
    ) -> Result<(), ServiceKeyError> {
        match db {
            crate::db::DatabasePool::Sqlite(db) => Self::revoke_sqlite(db, service_key_id).await,
            crate::db::DatabasePool::Postgres(db) => {
                Self::revoke_postgres(db, service_key_id).await
            }
        }
    }

    async fn revoke_sqlite(
        db: &Pool<Sqlite>,
        service_key_id: &Uuid,
    ) -> Result<(), ServiceKeyError> {
        sqlx::query(
            r#"
            UPDATE service_key SET revoked_at = datetime('now')
            WHERE service_key_id = ? AND revoked_at IS NULL
            "#,
        )
        .bind(service_key_id)
        .execute(db)
        .await?;

        Ok(())
    }

    async fn revoke_postgres(
        db: &Pool<Postgres>,
        service_key_id: &Uuid,
    ) -> Result<(), ServiceKeyError> {
        sqlx::query(
            r#"
            UPDATE service_key SET revoked_at = now()
            WHERE service_key_id = $1 AND revoked_at IS NULL
            "#,
        )
        .bind(service_key_id)
        .execute(db)
        .await?;

        Ok(())
    }

    /// Revoke all active service keys for an API key.
    pub async fn revoke_all_by_api_key(
        db: &crate::db::DatabasePool,
        api_key_id: &Uuid,
    ) -> Result<u64, ServiceKeyError> {
        match db {
            crate::db::DatabasePool::Sqlite(db) => {
                Self::revoke_all_by_api_key_sqlite(db, api_key_id).await
            }
            crate::db::DatabasePool::Postgres(db) => {
                Self::revoke_all_by_api_key_postgres(db, api_key_id).await
            }
        }
    }

    async fn revoke_all_by_api_key_sqlite(
        db: &Pool<Sqlite>,
        api_key_id: &Uuid,
    ) -> Result<u64, ServiceKeyError> {
        let result = sqlx::query(
            r#"
            UPDATE service_key SET revoked_at = datetime('now')
            WHERE api_key_id = ? AND revoked_at IS NULL
            "#,
        )
        .bind(api_key_id)
        .execute(db)
        .await?;

        Ok(result.rows_affected())
    }

    async fn revoke_all_by_api_key_postgres(
        db: &Pool<Postgres>,
        api_key_id: &Uuid,
    ) -> Result<u64, ServiceKeyError> {
        let result = sqlx::query(
            r#"
            UPDATE service_key SET revoked_at = now()
            WHERE api_key_id = $1 AND revoked_at IS NULL
            "#,
        )
        .bind(api_key_id)
        .execute(db)
        .await?;

        Ok(result.rows_affected())
    }

    /// Update metadata for a service key.
    /// `metadata` should be pre-encrypted via `ServiceKey::encrypt_metadata()`.
    pub async fn update_metadata(
        db: &crate::db::DatabasePool,
        service_key_id: &Uuid,
        metadata: Option<&str>,
    ) -> Result<(), ServiceKeyError> {
        match db {
            crate::db::DatabasePool::Sqlite(db) => {
                sqlx::query(
                    "UPDATE service_key SET metadata = ? WHERE service_key_id = ? AND revoked_at IS NULL",
                )
                .bind(metadata)
                .bind(service_key_id)
                .execute(db)
                .await?;
            }
            crate::db::DatabasePool::Postgres(db) => {
                sqlx::query(
                    "UPDATE service_key SET metadata = $1 WHERE service_key_id = $2 AND revoked_at IS NULL",
                )
                .bind(metadata)
                .bind(service_key_id)
                .execute(db)
                .await?;
            }
        }
        Ok(())
    }

    /// Update last_used_at timestamp (fire and forget, non-critical).
    pub async fn touch(db: &crate::db::DatabasePool, service_key_id: &Uuid) {
        match db {
            crate::db::DatabasePool::Sqlite(db) => {
                if let Err(e) = sqlx::query(
                    "UPDATE service_key SET last_used_at = datetime('now') WHERE service_key_id = ?",
                )
                .bind(service_key_id)
                .execute(db)
                .await
                {
                    tracing::warn!("Failed to update service_key last_used_at: {}", e);
                }
            }
            crate::db::DatabasePool::Postgres(db) => {
                if let Err(e) = sqlx::query(
                    "UPDATE service_key SET last_used_at = now() WHERE service_key_id = $1",
                )
                .bind(service_key_id)
                .execute(db)
                .await
                {
                    tracing::warn!("Failed to update service_key last_used_at: {}", e);
                }
            }
        }
    }
}

// ============================================================================
// Database row types (to handle SQLite/Postgres differences)
// ============================================================================

/// SQLite row type
#[derive(Debug, FromRow)]
struct SqliteServiceKeyRow {
    service_key_id: Uuid,
    api_key_id: Uuid,
    env_id: Uuid,
    name: Option<String>,
    description: Option<String>,
    secret_hash: Vec<u8>,
    permissions: String,
    metadata: Option<String>,
    expires_at: Option<DateTime<Utc>>,
    revoked_at: Option<DateTime<Utc>>,
    created_at: DateTime<Utc>,
    last_used_at: Option<DateTime<Utc>>,
}

impl SqliteServiceKeyRow {
    fn into_service_key(self) -> Result<ServiceKey, ServiceKeyError> {
        let permissions: serde_json::Value = serde_json::from_str(&self.permissions)
            .map_err(|_| ServiceKeyError::SerializationError)?;

        Ok(ServiceKey {
            service_key_id: self.service_key_id,
            api_key_id: self.api_key_id,
            env_id: self.env_id,
            name: self.name,
            description: self.description,
            secret_hash: self.secret_hash,
            permissions,
            metadata: self.metadata, // Stored as text (encrypted or legacy plaintext)
            expires_at: self.expires_at,
            revoked_at: self.revoked_at,
            created_at: self.created_at,
            last_used_at: self.last_used_at,
        })
    }
}

/// Postgres row type (metadata is now text, not jsonb)
#[derive(Debug, FromRow)]
struct PostgresServiceKeyRow {
    service_key_id: Uuid,
    api_key_id: Uuid,
    env_id: Uuid,
    name: Option<String>,
    description: Option<String>,
    secret_hash: Vec<u8>,
    permissions: serde_json::Value,
    metadata: Option<String>,
    expires_at: Option<DateTime<Utc>>,
    revoked_at: Option<DateTime<Utc>>,
    created_at: DateTime<Utc>,
    last_used_at: Option<DateTime<Utc>>,
}

impl PostgresServiceKeyRow {
    fn into_service_key(self) -> Result<ServiceKey, ServiceKeyError> {
        Ok(ServiceKey {
            service_key_id: self.service_key_id,
            api_key_id: self.api_key_id,
            env_id: self.env_id,
            name: self.name,
            description: self.description,
            secret_hash: self.secret_hash,
            permissions: self.permissions,
            metadata: self.metadata, // Stored as text (encrypted or legacy plaintext)
            expires_at: self.expires_at,
            revoked_at: self.revoked_at,
            created_at: self.created_at,
            last_used_at: self.last_used_at,
        })
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_token_generation_and_parsing() {
        let service_key_id = Uuid::parse_str("0193a7b2-1234-7def-8abc-123456789012").unwrap();
        let (token, hash) = ServiceKey::generate_token(&service_key_id).unwrap();

        // Check format: no prefix, just uuid_secret
        assert!(!token.starts_with("hot_"));
        assert!(!token.starts_with("s_"));
        let parts: Vec<&str> = token.split('_').collect();
        assert_eq!(parts.len(), 2); // uuid, secret
        assert_eq!(parts[0].len(), 32); // UUID hex
        assert_eq!(parts[1].len(), 32); // secret hex

        // Parse back
        let (parsed_id, secret_hex) = ServiceKey::parse_token(&token).unwrap();
        assert_eq!(parsed_id, service_key_id);
        assert_eq!(secret_hex.len(), 32);

        // Verify secret
        assert!(ServiceKey::verify_secret(&secret_hex, &hash));
        assert!(!ServiceKey::verify_secret(
            "wrong_secret_0000000000000000000",
            &hash
        ));
    }

    #[test]
    fn test_invalid_token_formats() {
        // Too few parts
        assert!(ServiceKey::parse_token("onlyonepart").is_err());
        // Too many parts (prefixed tokens should fail)
        assert!(ServiceKey::parse_token("s_abc_def").is_err());
        assert!(ServiceKey::parse_token("hot_abc_def").is_err());
        // Wrong UUID length
        assert!(ServiceKey::parse_token("short_a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4").is_err());
        // Wrong secret length
        assert!(ServiceKey::parse_token("0193a7b212347def8abc123456789012_short").is_err());
        // Empty
        assert!(ServiceKey::parse_token("").is_err());
    }

    #[test]
    fn test_secret_verification() {
        use sha2::{Digest, Sha256};

        let secret = "a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4";
        let mut hasher = Sha256::new();
        hasher.update(secret.as_bytes());
        let hash = hasher.finalize().to_vec();

        assert!(ServiceKey::verify_secret(secret, &hash));
        assert!(!ServiceKey::verify_secret(
            "wrong_secret_000000000000000000",
            &hash
        ));
    }

    #[test]
    fn test_validity_checks() {
        let now = Utc::now();

        // Active, no expiry
        let key = ServiceKey {
            service_key_id: Uuid::now_v7(),
            api_key_id: Uuid::now_v7(),
            env_id: Uuid::now_v7(),
            name: Some("test".to_string()),
            description: None,
            secret_hash: vec![],
            permissions: serde_json::json!({}),
            metadata: None, // Option<String> now
            expires_at: None,
            revoked_at: None,
            created_at: now,
            last_used_at: None,
        };
        assert!(key.is_valid());
        assert!(!key.is_expired());
        assert!(!key.is_revoked());

        // Expired
        let expired_key = ServiceKey {
            expires_at: Some(now - chrono::Duration::hours(1)),
            ..key.clone()
        };
        assert!(!expired_key.is_valid());
        assert!(expired_key.is_expired());

        // Revoked
        let revoked_key = ServiceKey {
            revoked_at: Some(now),
            ..key.clone()
        };
        assert!(!revoked_key.is_valid());
        assert!(revoked_key.is_revoked());
    }
}

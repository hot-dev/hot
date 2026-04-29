//! Session model for short-lived, scoped access tokens.
//!
//! Sessions are created by API key holders to grant narrowly-scoped permissions
//! to consumers (end-users, services, pipelines, etc.).
//!
//! Token format: `s_<session_id_hex_32>_<secret_hex_32>`

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, Pool, Postgres, Sqlite};
use std::error::Error;
use std::fmt;
use uuid::Uuid;

use crate::permission::Permissions;

// ============================================================================
// Error Type
// ============================================================================

#[derive(Debug)]
pub enum SessionError {
    Database(sqlx::Error),
    NotFound,
    InvalidToken,
    Expired,
    Revoked,
    ParentKeyInactive,
    HashingError,
    SerializationError,
    PermissionError(crate::permission::PermissionError),
    LimitExceeded { max: i64 },
}

impl fmt::Display for SessionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SessionError::Database(e) => write!(f, "Database error: {}", e),
            SessionError::NotFound => write!(f, "Session not found"),
            SessionError::InvalidToken => write!(f, "Invalid session token"),
            SessionError::Expired => write!(f, "Session has expired"),
            SessionError::Revoked => write!(f, "Session has been revoked"),
            SessionError::ParentKeyInactive => write!(f, "Parent API key is no longer active"),
            SessionError::HashingError => write!(f, "Failed to hash session secret"),
            SessionError::SerializationError => write!(f, "Failed to serialize session data"),
            SessionError::PermissionError(e) => write!(f, "Permission error: {}", e),
            SessionError::LimitExceeded { max } => {
                write!(f, "Maximum active sessions exceeded (limit: {})", max)
            }
        }
    }
}

impl Error for SessionError {}

impl From<sqlx::Error> for SessionError {
    fn from(error: sqlx::Error) -> Self {
        SessionError::Database(error)
    }
}

impl From<crate::permission::PermissionError> for SessionError {
    fn from(error: crate::permission::PermissionError) -> Self {
        SessionError::PermissionError(error)
    }
}

// ============================================================================
// Session Struct
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct Session {
    pub session_id: Uuid,
    pub api_key_id: Uuid,
    pub env_id: Uuid,
    #[sqlx(skip)]
    #[serde(skip)]
    pub secret_hash: Vec<u8>,
    pub permissions: serde_json::Value,
    pub metadata: Option<serde_json::Value>,
    pub expires_at: DateTime<Utc>,
    pub revoked_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub last_used_at: Option<DateTime<Utc>>,
}

/// Token prefix for session tokens (minimal, non-branded for white-label compatibility)
const TOKEN_PREFIX: &str = "s_";

/// Default session TTL: 1 hour
const DEFAULT_TTL_SECS: i64 = 3600;

/// Maximum session TTL: 24 hours
const MAX_TTL_SECS: i64 = 86400;

/// Default maximum active sessions per API key
const DEFAULT_MAX_SESSIONS: i64 = 1000;

/// Length of the random secret in bytes (produces 32 hex chars)
const SECRET_BYTES: usize = 16;

impl Session {
    // ========================================================================
    // Token generation and parsing
    // ========================================================================

    /// Generate a new session token and return (full_token, secret_hash_bytes).
    ///
    /// Token format: `s_<session_id_hex_32>_<secret_hex_32>`
    /// The secret portion is hashed with SHA-256 for storage.
    pub fn generate_token(session_id: &Uuid) -> Result<(String, Vec<u8>), SessionError> {
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

        // Build the token
        let uuid_hex = session_id.to_string().replace("-", "");
        let token = format!("{}{}{}{}", TOKEN_PREFIX, uuid_hex, "_", secret_hex);

        Ok((token, hash))
    }

    /// Parse a session token to extract the session ID and secret hex.
    ///
    /// Returns (session_id, secret_hex) or an error if the format is invalid.
    pub fn parse_token(token: &str) -> Result<(Uuid, String), SessionError> {
        if !token.starts_with(TOKEN_PREFIX) {
            return Err(SessionError::InvalidToken);
        }

        let rest = &token[TOKEN_PREFIX.len()..];
        let parts: Vec<&str> = rest.split('_').collect();
        if parts.len() != 2 {
            return Err(SessionError::InvalidToken);
        }

        let uuid_hex = parts[0];
        let secret_hex = parts[1];

        // Validate UUID hex (32 hex characters)
        if uuid_hex.len() != 32 || !uuid_hex.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(SessionError::InvalidToken);
        }

        // Validate secret hex (32 hex characters)
        if secret_hex.len() != 32 || !secret_hex.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(SessionError::InvalidToken);
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

        let session_id =
            Uuid::parse_str(&uuid_with_dashes).map_err(|_| SessionError::InvalidToken)?;

        Ok((session_id, secret_hex.to_string()))
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

    /// Check if this session is still valid (not expired, not revoked).
    pub fn is_valid(&self) -> bool {
        self.revoked_at.is_none() && self.expires_at > Utc::now()
    }

    /// Check if this session has expired.
    pub fn is_expired(&self) -> bool {
        self.expires_at <= Utc::now()
    }

    /// Check if this session has been revoked.
    pub fn is_revoked(&self) -> bool {
        self.revoked_at.is_some()
    }

    /// Get the permissions as a Permissions struct.
    pub fn get_permissions(&self) -> Result<Permissions, SessionError> {
        Permissions::from_json(&self.permissions).map_err(SessionError::PermissionError)
    }

    /// Check if this session grants a specific action on a specific resource.
    pub fn has_permission(&self, resource: &str, action: &str) -> bool {
        match self.get_permissions() {
            Ok(perms) => perms.has_permission(resource, action),
            Err(_) => false,
        }
    }

    /// Get the default TTL in seconds.
    pub fn default_ttl() -> i64 {
        DEFAULT_TTL_SECS
    }

    /// Get the maximum TTL in seconds.
    pub fn max_ttl() -> i64 {
        MAX_TTL_SECS
    }

    /// Check if a token string looks like a session token (starts with prefix).
    pub fn is_session_token(token: &str) -> bool {
        token.starts_with(TOKEN_PREFIX)
    }

    // ========================================================================
    // Database operations
    // ========================================================================

    /// Create a new session.
    /// Returns (Session, token_string). The token is only available at creation time.
    pub async fn create(
        db: &crate::db::DatabasePool,
        api_key_id: &Uuid,
        env_id: &Uuid,
        permissions: &serde_json::Value,
        metadata: Option<&serde_json::Value>,
        expires_in_secs: Option<i64>,
    ) -> Result<(Session, String), SessionError> {
        let session_id = Uuid::now_v7();

        // Calculate expiration
        let ttl = expires_in_secs
            .unwrap_or(DEFAULT_TTL_SECS)
            .min(MAX_TTL_SECS);
        let expires_at = Utc::now() + chrono::Duration::seconds(ttl);

        // Generate token and hash
        let (token, secret_hash) = Self::generate_token(&session_id)?;

        match db {
            crate::db::DatabasePool::Sqlite(db) => {
                Self::insert_sqlite(
                    db,
                    &session_id,
                    api_key_id,
                    env_id,
                    &secret_hash,
                    permissions,
                    metadata,
                    &expires_at,
                )
                .await?;
            }
            crate::db::DatabasePool::Postgres(db) => {
                Self::insert_postgres(
                    db,
                    &session_id,
                    api_key_id,
                    env_id,
                    &secret_hash,
                    permissions,
                    metadata,
                    &expires_at,
                )
                .await?;
            }
        }

        let session = Session {
            session_id,
            api_key_id: *api_key_id,
            env_id: *env_id,
            secret_hash,
            permissions: permissions.clone(),
            metadata: metadata.cloned(),
            expires_at,
            revoked_at: None,
            created_at: Utc::now(),
            last_used_at: None,
        };

        Ok((session, token))
    }

    #[allow(clippy::too_many_arguments)]
    async fn insert_sqlite(
        db: &Pool<Sqlite>,
        session_id: &Uuid,
        api_key_id: &Uuid,
        env_id: &Uuid,
        secret_hash: &[u8],
        permissions: &serde_json::Value,
        metadata: Option<&serde_json::Value>,
        expires_at: &DateTime<Utc>,
    ) -> Result<(), SessionError> {
        let permissions_str =
            serde_json::to_string(permissions).map_err(|_| SessionError::SerializationError)?;
        let metadata_str = metadata
            .map(|m| serde_json::to_string(m).map_err(|_| SessionError::SerializationError))
            .transpose()?;

        sqlx::query(
            r#"
            INSERT INTO session (session_id, api_key_id, env_id, secret_hash, permissions, metadata, expires_at)
            VALUES (?, ?, ?, ?, ?, ?, ?)
            "#,
        )
        .bind(session_id)
        .bind(api_key_id)
        .bind(env_id)
        .bind(secret_hash)
        .bind(permissions_str)
        .bind(metadata_str)
        .bind(expires_at)
        .execute(db)
        .await?;

        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn insert_postgres(
        db: &Pool<Postgres>,
        session_id: &Uuid,
        api_key_id: &Uuid,
        env_id: &Uuid,
        secret_hash: &[u8],
        permissions: &serde_json::Value,
        metadata: Option<&serde_json::Value>,
        expires_at: &DateTime<Utc>,
    ) -> Result<(), SessionError> {
        sqlx::query(
            r#"
            INSERT INTO session (session_id, api_key_id, env_id, secret_hash, permissions, metadata, expires_at)
            VALUES ($1, $2, $3, $4, $5, $6, $7)
            "#,
        )
        .bind(session_id)
        .bind(api_key_id)
        .bind(env_id)
        .bind(secret_hash)
        .bind(permissions)
        .bind(metadata)
        .bind(expires_at)
        .execute(db)
        .await?;

        Ok(())
    }

    /// Get a session by ID (includes secret_hash for verification).
    pub async fn get_session(
        db: &crate::db::DatabasePool,
        session_id: &Uuid,
    ) -> Result<Session, SessionError> {
        match db {
            crate::db::DatabasePool::Sqlite(db) => Self::get_session_sqlite(db, session_id).await,
            crate::db::DatabasePool::Postgres(db) => {
                Self::get_session_postgres(db, session_id).await
            }
        }
    }

    async fn get_session_sqlite(
        db: &Pool<Sqlite>,
        session_id: &Uuid,
    ) -> Result<Session, SessionError> {
        // SQLite doesn't support FromRow well with Vec<u8>, so use query_as with manual handling
        let row = sqlx::query_as::<_, SqliteSessionRow>(
            r#"
            SELECT session_id, api_key_id, env_id, secret_hash, permissions, metadata,
                   expires_at, revoked_at, created_at, last_used_at
            FROM session
            WHERE session_id = ?
            "#,
        )
        .bind(session_id)
        .fetch_one(db)
        .await
        .map_err(|e| match e {
            sqlx::Error::RowNotFound => SessionError::NotFound,
            other => SessionError::Database(other),
        })?;

        row.into_session()
    }

    async fn get_session_postgres(
        db: &Pool<Postgres>,
        session_id: &Uuid,
    ) -> Result<Session, SessionError> {
        let row = sqlx::query_as::<_, PostgresSessionRow>(
            r#"
            SELECT session_id, api_key_id, env_id, secret_hash, permissions, metadata,
                   expires_at, revoked_at, created_at, last_used_at
            FROM session
            WHERE session_id = $1
            "#,
        )
        .bind(session_id)
        .fetch_one(db)
        .await
        .map_err(|e| match e {
            sqlx::Error::RowNotFound => SessionError::NotFound,
            other => SessionError::Database(other),
        })?;

        row.into_session()
    }

    /// Verify a session token and return the session if valid.
    /// Checks: token format, existence, expiration, revocation, secret match.
    /// Does NOT check parent API key status (caller should do that separately).
    pub async fn verify_token(
        db: &crate::db::DatabasePool,
        token: &str,
    ) -> Result<Session, SessionError> {
        // Parse the token
        let (session_id, secret_hex) = Self::parse_token(token)?;

        // Look up the session
        let session = Self::get_session(db, &session_id).await?;

        // Check expiration
        if session.is_expired() {
            return Err(SessionError::Expired);
        }

        // Check revocation
        if session.is_revoked() {
            return Err(SessionError::Revoked);
        }

        // Verify the secret
        if !Self::verify_secret(&secret_hex, &session.secret_hash) {
            return Err(SessionError::InvalidToken);
        }

        Ok(session)
    }

    /// List active (non-revoked, non-expired) sessions for an API key.
    pub async fn list_by_api_key(
        db: &crate::db::DatabasePool,
        api_key_id: &Uuid,
        limit: Option<i64>,
        offset: Option<i64>,
    ) -> Result<Vec<Session>, SessionError> {
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
    ) -> Result<Vec<Session>, SessionError> {
        let limit = limit.unwrap_or(50);
        let offset = offset.unwrap_or(0);

        let rows = sqlx::query_as::<_, SqliteSessionRow>(
            r#"
            SELECT session_id, api_key_id, env_id, secret_hash, permissions, metadata,
                   expires_at, revoked_at, created_at, last_used_at
            FROM session
            WHERE api_key_id = ?
              AND revoked_at IS NULL
              AND expires_at > datetime('now')
            ORDER BY created_at DESC
            LIMIT ? OFFSET ?
            "#,
        )
        .bind(api_key_id)
        .bind(limit)
        .bind(offset)
        .fetch_all(db)
        .await?;

        rows.into_iter().map(|r| r.into_session()).collect()
    }

    async fn list_by_api_key_postgres(
        db: &Pool<Postgres>,
        api_key_id: &Uuid,
        limit: Option<i64>,
        offset: Option<i64>,
    ) -> Result<Vec<Session>, SessionError> {
        let limit = limit.unwrap_or(50);
        let offset = offset.unwrap_or(0);

        let rows = sqlx::query_as::<_, PostgresSessionRow>(
            r#"
            SELECT session_id, api_key_id, env_id, secret_hash, permissions, metadata,
                   expires_at, revoked_at, created_at, last_used_at
            FROM session
            WHERE api_key_id = $1
              AND revoked_at IS NULL
              AND expires_at > now()
            ORDER BY created_at DESC
            LIMIT $2 OFFSET $3
            "#,
        )
        .bind(api_key_id)
        .bind(limit)
        .bind(offset)
        .fetch_all(db)
        .await?;

        rows.into_iter().map(|r| r.into_session()).collect()
    }

    /// Count active sessions for an API key (for limit enforcement).
    pub async fn count_active_by_api_key(
        db: &crate::db::DatabasePool,
        api_key_id: &Uuid,
    ) -> Result<i64, SessionError> {
        match db {
            crate::db::DatabasePool::Sqlite(db) => {
                Self::count_active_by_api_key_sqlite(db, api_key_id).await
            }
            crate::db::DatabasePool::Postgres(db) => {
                Self::count_active_by_api_key_postgres(db, api_key_id).await
            }
        }
    }

    async fn count_active_by_api_key_sqlite(
        db: &Pool<Sqlite>,
        api_key_id: &Uuid,
    ) -> Result<i64, SessionError> {
        let row: (i64,) = sqlx::query_as(
            r#"
            SELECT COUNT(*) FROM session
            WHERE api_key_id = ?
              AND revoked_at IS NULL
              AND expires_at > datetime('now')
            "#,
        )
        .bind(api_key_id)
        .fetch_one(db)
        .await?;

        Ok(row.0)
    }

    async fn count_active_by_api_key_postgres(
        db: &Pool<Postgres>,
        api_key_id: &Uuid,
    ) -> Result<i64, SessionError> {
        let row: (i64,) = sqlx::query_as(
            r#"
            SELECT COUNT(*) FROM session
            WHERE api_key_id = $1
              AND revoked_at IS NULL
              AND expires_at > now()
            "#,
        )
        .bind(api_key_id)
        .fetch_one(db)
        .await?;

        Ok(row.0)
    }

    /// Revoke a specific session.
    pub async fn revoke(
        db: &crate::db::DatabasePool,
        session_id: &Uuid,
    ) -> Result<(), SessionError> {
        match db {
            crate::db::DatabasePool::Sqlite(db) => Self::revoke_sqlite(db, session_id).await,
            crate::db::DatabasePool::Postgres(db) => Self::revoke_postgres(db, session_id).await,
        }
    }

    async fn revoke_sqlite(db: &Pool<Sqlite>, session_id: &Uuid) -> Result<(), SessionError> {
        sqlx::query(
            r#"
            UPDATE session SET revoked_at = datetime('now')
            WHERE session_id = ? AND revoked_at IS NULL
            "#,
        )
        .bind(session_id)
        .execute(db)
        .await?;

        Ok(())
    }

    async fn revoke_postgres(db: &Pool<Postgres>, session_id: &Uuid) -> Result<(), SessionError> {
        sqlx::query(
            r#"
            UPDATE session SET revoked_at = now()
            WHERE session_id = $1 AND revoked_at IS NULL
            "#,
        )
        .bind(session_id)
        .execute(db)
        .await?;

        Ok(())
    }

    /// Revoke all active sessions for an API key.
    pub async fn revoke_all_by_api_key(
        db: &crate::db::DatabasePool,
        api_key_id: &Uuid,
    ) -> Result<u64, SessionError> {
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
    ) -> Result<u64, SessionError> {
        let result = sqlx::query(
            r#"
            UPDATE session SET revoked_at = datetime('now')
            WHERE api_key_id = ? AND revoked_at IS NULL AND expires_at > datetime('now')
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
    ) -> Result<u64, SessionError> {
        let result = sqlx::query(
            r#"
            UPDATE session SET revoked_at = now()
            WHERE api_key_id = $1 AND revoked_at IS NULL AND expires_at > now()
            "#,
        )
        .bind(api_key_id)
        .execute(db)
        .await?;

        Ok(result.rows_affected())
    }

    /// Update last_used_at timestamp (fire and forget, non-critical).
    pub async fn touch(db: &crate::db::DatabasePool, session_id: &Uuid) {
        match db {
            crate::db::DatabasePool::Sqlite(db) => {
                if let Err(e) = sqlx::query(
                    "UPDATE session SET last_used_at = datetime('now') WHERE session_id = ?",
                )
                .bind(session_id)
                .execute(db)
                .await
                {
                    tracing::warn!("Failed to update session last_used_at: {}", e);
                }
            }
            crate::db::DatabasePool::Postgres(db) => {
                if let Err(e) =
                    sqlx::query("UPDATE session SET last_used_at = now() WHERE session_id = $1")
                        .bind(session_id)
                        .execute(db)
                        .await
                {
                    tracing::warn!("Failed to update session last_used_at: {}", e);
                }
            }
        }
    }

    /// Clean up expired sessions (periodic maintenance).
    pub async fn cleanup_expired(db: &crate::db::DatabasePool) -> Result<u64, SessionError> {
        match db {
            crate::db::DatabasePool::Sqlite(db) => {
                let result =
                    sqlx::query("DELETE FROM session WHERE expires_at < datetime('now', '-1 day')")
                        .execute(db)
                        .await?;
                Ok(result.rows_affected())
            }
            crate::db::DatabasePool::Postgres(db) => {
                let result =
                    sqlx::query("DELETE FROM session WHERE expires_at < now() - interval '1 day'")
                        .execute(db)
                        .await?;
                Ok(result.rows_affected())
            }
        }
    }

    /// Get the max active sessions limit.
    pub fn max_active_sessions() -> i64 {
        DEFAULT_MAX_SESSIONS
    }
}

// ============================================================================
// Database row types (to handle SQLite/Postgres differences)
// ============================================================================

/// SQLite row type (stores JSON as text, timestamps as text, hash as blob)
#[derive(Debug, FromRow)]
struct SqliteSessionRow {
    session_id: Uuid,
    api_key_id: Uuid,
    env_id: Uuid,
    secret_hash: Vec<u8>,
    permissions: String,
    metadata: Option<String>,
    expires_at: DateTime<Utc>,
    revoked_at: Option<DateTime<Utc>>,
    created_at: DateTime<Utc>,
    last_used_at: Option<DateTime<Utc>>,
}

impl SqliteSessionRow {
    fn into_session(self) -> Result<Session, SessionError> {
        let permissions: serde_json::Value = serde_json::from_str(&self.permissions)
            .map_err(|_| SessionError::SerializationError)?;
        let metadata: Option<serde_json::Value> = self
            .metadata
            .map(|m| serde_json::from_str(&m))
            .transpose()
            .map_err(|_| SessionError::SerializationError)?;

        Ok(Session {
            session_id: self.session_id,
            api_key_id: self.api_key_id,
            env_id: self.env_id,
            secret_hash: self.secret_hash,
            permissions,
            metadata,
            expires_at: self.expires_at,
            revoked_at: self.revoked_at,
            created_at: self.created_at,
            last_used_at: self.last_used_at,
        })
    }
}

/// Postgres row type (stores JSON as jsonb, timestamps as timestamptz, hash as bytea)
#[derive(Debug, FromRow)]
struct PostgresSessionRow {
    session_id: Uuid,
    api_key_id: Uuid,
    env_id: Uuid,
    secret_hash: Vec<u8>,
    permissions: serde_json::Value,
    metadata: Option<serde_json::Value>,
    expires_at: DateTime<Utc>,
    revoked_at: Option<DateTime<Utc>>,
    created_at: DateTime<Utc>,
    last_used_at: Option<DateTime<Utc>>,
}

impl PostgresSessionRow {
    fn into_session(self) -> Result<Session, SessionError> {
        Ok(Session {
            session_id: self.session_id,
            api_key_id: self.api_key_id,
            env_id: self.env_id,
            secret_hash: self.secret_hash,
            permissions: self.permissions,
            metadata: self.metadata,
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
        let session_id = Uuid::parse_str("0193a7b2-1234-7def-8abc-123456789012").unwrap();
        let (token, hash) = Session::generate_token(&session_id).unwrap();

        // Check format
        assert!(token.starts_with("s_"));
        let parts: Vec<&str> = token.split('_').collect();
        assert_eq!(parts.len(), 3); // s, uuid, secret
        assert_eq!(parts[0], "s");
        assert_eq!(parts[1].len(), 32); // UUID hex
        assert_eq!(parts[2].len(), 32); // secret hex

        // Parse back
        let (parsed_id, secret_hex) = Session::parse_token(&token).unwrap();
        assert_eq!(parsed_id, session_id);
        assert_eq!(secret_hex.len(), 32);

        // Verify secret
        assert!(Session::verify_secret(&secret_hex, &hash));
        assert!(!Session::verify_secret(
            "wrong_secret_0000000000000000000",
            &hash
        ));
    }

    #[test]
    fn test_invalid_token_formats() {
        // Wrong prefix
        assert!(Session::parse_token("hot_abc_def").is_err());
        assert!(Session::parse_token("hot_st_abc_def").is_err());
        // Too few parts
        assert!(Session::parse_token("s_onlyonepart").is_err());
        // Wrong UUID length
        assert!(Session::parse_token("s_short_a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4").is_err());
        // Wrong secret length
        assert!(Session::parse_token("s_0193a7b212347def8abc123456789012_short").is_err());
        // Empty
        assert!(Session::parse_token("").is_err());
        // API key format (should not match)
        assert!(Session::parse_token(
            "hot_0193a7b212347def8abc123456789012_a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4"
        )
        .is_err());
    }

    #[test]
    fn test_is_session_token() {
        assert!(Session::is_session_token("s_abc_def"));
        assert!(!Session::is_session_token("hot_abc_def"));
        assert!(!Session::is_session_token("hot_st_abc_def"));
        assert!(!Session::is_session_token(""));
        assert!(!Session::is_session_token("Bearer s_abc"));
    }

    #[test]
    fn test_secret_verification() {
        use sha2::{Digest, Sha256};

        let secret = "a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4";
        let mut hasher = Sha256::new();
        hasher.update(secret.as_bytes());
        let hash = hasher.finalize().to_vec();

        assert!(Session::verify_secret(secret, &hash));
        assert!(!Session::verify_secret(
            "wrong_secret_000000000000000000",
            &hash
        ));
    }
}

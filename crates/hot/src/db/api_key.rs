use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, Pool, Postgres, Sqlite};
use std::error::Error;
use std::fmt;
use uuid::Uuid;

#[derive(Debug)]
pub enum ApiKeyError {
    Database(sqlx::Error),
    NotFound,
    InvalidKey,
    HashingError,
    SerializationError,
}

impl fmt::Display for ApiKeyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ApiKeyError::Database(e) => write!(f, "Database error: {}", e),
            ApiKeyError::NotFound => write!(f, "API key not found"),
            ApiKeyError::InvalidKey => write!(f, "Invalid API key"),
            ApiKeyError::HashingError => write!(f, "Failed to hash API key"),
            ApiKeyError::SerializationError => write!(f, "Failed to serialize API key data"),
        }
    }
}

impl Error for ApiKeyError {}

impl From<sqlx::Error> for ApiKeyError {
    fn from(error: sqlx::Error) -> Self {
        ApiKeyError::Database(error)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct ApiKey {
    pub api_key_id: Uuid,
    pub env_id: Uuid,
    pub description: String,
    pub key_data: serde_json::Value,
    pub active: bool,
    pub created_by_user_id: Uuid,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub updated_by_user_id: Option<Uuid>,
    pub active_toggle_at: Option<DateTime<Utc>>,
    pub active_toggle_by_user_id: Option<Uuid>,
    /// Unified permissions map for this API key.
    /// Stored as JSON: {"resource:path": ["action1", "action2"], ...}
    /// Empty {} means no access. {"*:*": ["*"]} means full access.
    pub permissions: serde_json::Value,
}

impl ApiKey {
    // ========================================================================
    // Permission checking
    // ========================================================================

    /// Get the unified permissions for this API key.
    pub fn get_permissions(
        &self,
    ) -> Result<crate::permission::Permissions, crate::permission::PermissionError> {
        crate::permission::Permissions::from_json(&self.permissions)
    }

    /// Check if this key has full (unrestricted) access via the permissions column.
    pub fn has_full_permissions(&self) -> bool {
        if let Ok(perms) = crate::permission::Permissions::from_json(&self.permissions) {
            perms.has_permission("*:*", "*")
        } else {
            false
        }
    }

    // ========================================================================
    // Database operations
    // ========================================================================

    /// Get API key by ID
    pub async fn get_api_key(
        db: &crate::db::DatabasePool,
        api_key_id: &Uuid,
    ) -> Result<ApiKey, ApiKeyError> {
        match db {
            crate::db::DatabasePool::Sqlite(db) => Self::get_api_key_sqlite(db, api_key_id).await,
            crate::db::DatabasePool::Postgres(db) => {
                Self::get_api_key_postgres(db, api_key_id).await
            }
        }
    }

    async fn get_api_key_sqlite(
        db: &Pool<Sqlite>,
        api_key_id: &Uuid,
    ) -> Result<ApiKey, ApiKeyError> {
        let row = sqlx::query_as::<_, ApiKey>(
            r#"
            SELECT api_key_id, env_id, description, key_data, active,
                   created_by_user_id, created_at, updated_at, updated_by_user_id,
                   active_toggle_at, active_toggle_by_user_id, permissions
            FROM api_key
            WHERE api_key_id = ?
            "#,
        )
        .bind(api_key_id)
        .fetch_one(db)
        .await
        .map_err(|e| match e {
            sqlx::Error::RowNotFound => ApiKeyError::NotFound,
            other => ApiKeyError::Database(other),
        })?;

        Ok(row)
    }

    async fn get_api_key_postgres(
        db: &Pool<Postgres>,
        api_key_id: &Uuid,
    ) -> Result<ApiKey, ApiKeyError> {
        let row = sqlx::query_as::<_, ApiKey>(
            r#"
            SELECT api_key_id, env_id, description, key_data, active,
                   created_by_user_id, created_at, updated_at, updated_by_user_id,
                   active_toggle_at, active_toggle_by_user_id, permissions
            FROM api_key
            WHERE api_key_id = $1
            "#,
        )
        .bind(api_key_id)
        .fetch_one(db)
        .await
        .map_err(|e| match e {
            sqlx::Error::RowNotFound => ApiKeyError::NotFound,
            other => ApiKeyError::Database(other),
        })?;

        Ok(row)
    }

    /// Get API keys by environment
    pub async fn get_api_keys_by_env(
        db: &crate::db::DatabasePool,
        env_id: &Uuid,
        limit: Option<i64>,
        offset: Option<i64>,
    ) -> Result<Vec<ApiKey>, ApiKeyError> {
        match db {
            crate::db::DatabasePool::Sqlite(db) => {
                Self::get_api_keys_by_env_sqlite(db, env_id, limit, offset).await
            }
            crate::db::DatabasePool::Postgres(db) => {
                Self::get_api_keys_by_env_postgres(db, env_id, limit, offset).await
            }
        }
    }

    async fn get_api_keys_by_env_sqlite(
        db: &Pool<Sqlite>,
        env_id: &Uuid,
        limit: Option<i64>,
        offset: Option<i64>,
    ) -> Result<Vec<ApiKey>, ApiKeyError> {
        let limit = limit.unwrap_or(10);
        let offset = offset.unwrap_or(0);

        let rows = sqlx::query_as::<_, ApiKey>(
            r#"
            SELECT api_key_id, env_id, description, key_data, active,
                   created_by_user_id, created_at, updated_at, updated_by_user_id,
                   active_toggle_at, active_toggle_by_user_id, permissions
            FROM api_key
            WHERE env_id = ?
            ORDER BY created_at DESC
            LIMIT ? OFFSET ?
            "#,
        )
        .bind(env_id)
        .bind(limit)
        .bind(offset)
        .fetch_all(db)
        .await?;

        Ok(rows)
    }

    async fn get_api_keys_by_env_postgres(
        db: &Pool<Postgres>,
        env_id: &Uuid,
        limit: Option<i64>,
        offset: Option<i64>,
    ) -> Result<Vec<ApiKey>, ApiKeyError> {
        let limit = limit.unwrap_or(10);
        let offset = offset.unwrap_or(0);

        let rows = sqlx::query_as::<_, ApiKey>(
            r#"
            SELECT api_key_id, env_id, description, key_data, active,
                   created_by_user_id, created_at, updated_at, updated_by_user_id,
                   active_toggle_at, active_toggle_by_user_id, permissions
            FROM api_key
            WHERE env_id = $1
            ORDER BY created_at DESC
            LIMIT $2 OFFSET $3
            "#,
        )
        .bind(env_id)
        .bind(limit)
        .bind(offset)
        .fetch_all(db)
        .await?;

        Ok(rows)
    }

    /// Get the most recently created active API key for an environment.
    pub async fn get_active_api_key_by_env(
        db: &crate::db::DatabasePool,
        env_id: &Uuid,
    ) -> Result<Option<ApiKey>, ApiKeyError> {
        match db {
            crate::db::DatabasePool::Sqlite(db) => Ok(sqlx::query_as::<_, ApiKey>(
                r#"
                SELECT api_key_id, env_id, description, key_data, active,
                       created_by_user_id, created_at, updated_at, updated_by_user_id,
                       active_toggle_at, active_toggle_by_user_id, permissions
                FROM api_key
                WHERE env_id = ? AND active = 1
                ORDER BY created_at DESC
                LIMIT 1
                "#,
            )
            .bind(env_id)
            .fetch_optional(db)
            .await?),
            crate::db::DatabasePool::Postgres(db) => Ok(sqlx::query_as::<_, ApiKey>(
                r#"
                SELECT api_key_id, env_id, description, key_data, active,
                       created_by_user_id, created_at, updated_at, updated_by_user_id,
                       active_toggle_at, active_toggle_by_user_id, permissions
                FROM api_key
                WHERE env_id = $1 AND active = TRUE
                ORDER BY created_at DESC
                LIMIT 1
                "#,
            )
            .bind(env_id)
            .fetch_optional(db)
            .await?),
        }
    }

    /// Get API keys by organization (via environments)
    pub async fn get_api_keys_by_org(
        db: &crate::db::DatabasePool,
        org_id: &Uuid,
        limit: Option<i64>,
        offset: Option<i64>,
    ) -> Result<Vec<ApiKey>, ApiKeyError> {
        match db {
            crate::db::DatabasePool::Sqlite(db) => {
                Self::get_api_keys_by_org_sqlite(db, org_id, limit, offset).await
            }
            crate::db::DatabasePool::Postgres(db) => {
                Self::get_api_keys_by_org_postgres(db, org_id, limit, offset).await
            }
        }
    }

    async fn get_api_keys_by_org_sqlite(
        db: &Pool<Sqlite>,
        org_id: &Uuid,
        limit: Option<i64>,
        offset: Option<i64>,
    ) -> Result<Vec<ApiKey>, ApiKeyError> {
        let limit = limit.unwrap_or(10);
        let offset = offset.unwrap_or(0);

        let rows = sqlx::query_as::<_, ApiKey>(
            r#"
            SELECT ak.api_key_id, ak.env_id, ak.description, ak.key_data, ak.active,
                   ak.created_by_user_id, ak.created_at, ak.updated_at, ak.updated_by_user_id,
                   ak.active_toggle_at, ak.active_toggle_by_user_id, ak.permissions
            FROM api_key ak
            JOIN env e ON ak.env_id = e.env_id
            WHERE e.org_id = ?
            ORDER BY ak.created_at DESC
            LIMIT ? OFFSET ?
            "#,
        )
        .bind(org_id)
        .bind(limit)
        .bind(offset)
        .fetch_all(db)
        .await?;

        Ok(rows)
    }

    async fn get_api_keys_by_org_postgres(
        db: &Pool<Postgres>,
        org_id: &Uuid,
        limit: Option<i64>,
        offset: Option<i64>,
    ) -> Result<Vec<ApiKey>, ApiKeyError> {
        let limit = limit.unwrap_or(10);
        let offset = offset.unwrap_or(0);

        let rows = sqlx::query_as::<_, ApiKey>(
            r#"
            SELECT ak.api_key_id, ak.env_id, ak.description, ak.key_data, ak.active,
                   ak.created_by_user_id, ak.created_at, ak.updated_at, ak.updated_by_user_id,
                   ak.active_toggle_at, ak.active_toggle_by_user_id, ak.permissions
            FROM api_key ak
            JOIN env e ON ak.env_id = e.env_id
            WHERE e.org_id = $1
            ORDER BY ak.created_at DESC
            LIMIT $2 OFFSET $3
            "#,
        )
        .bind(org_id)
        .bind(limit)
        .bind(offset)
        .fetch_all(db)
        .await?;

        Ok(rows)
    }

    /// Insert a new API key
    #[allow(clippy::too_many_arguments)]
    pub async fn insert_api_key(
        db: &crate::db::DatabasePool,
        api_key_id: &Uuid,
        env_id: &Uuid,
        description: &str,
        key_data: &serde_json::Value,
        created_by_user_id: &Uuid,
        permissions: &serde_json::Value,
    ) -> Result<(), ApiKeyError> {
        match db {
            crate::db::DatabasePool::Sqlite(db) => {
                Self::insert_api_key_sqlite(
                    db,
                    api_key_id,
                    env_id,
                    description,
                    key_data,
                    created_by_user_id,
                    permissions,
                )
                .await
            }
            crate::db::DatabasePool::Postgres(db) => {
                Self::insert_api_key_postgres(
                    db,
                    api_key_id,
                    env_id,
                    description,
                    key_data,
                    created_by_user_id,
                    permissions,
                )
                .await
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn insert_api_key_sqlite(
        db: &Pool<Sqlite>,
        api_key_id: &Uuid,
        env_id: &Uuid,
        description: &str,
        key_data: &serde_json::Value,
        created_by_user_id: &Uuid,
        permissions: &serde_json::Value,
    ) -> Result<(), ApiKeyError> {
        let key_data_str =
            serde_json::to_string(key_data).map_err(|_| ApiKeyError::SerializationError)?;
        let permissions_str =
            serde_json::to_string(permissions).map_err(|_| ApiKeyError::SerializationError)?;

        sqlx::query(
            r#"
            INSERT INTO api_key (api_key_id, env_id, description, key_data, created_by_user_id, permissions)
            VALUES (?, ?, ?, ?, ?, ?)
            "#,
        )
        .bind(api_key_id)
        .bind(env_id)
        .bind(description)
        .bind(key_data_str)
        .bind(created_by_user_id)
        .bind(permissions_str)
        .execute(db)
        .await?;

        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn insert_api_key_postgres(
        db: &Pool<Postgres>,
        api_key_id: &Uuid,
        env_id: &Uuid,
        description: &str,
        key_data: &serde_json::Value,
        created_by_user_id: &Uuid,
        permissions: &serde_json::Value,
    ) -> Result<(), ApiKeyError> {
        sqlx::query(
            r#"
            INSERT INTO api_key (api_key_id, env_id, description, key_data, created_by_user_id, permissions)
            VALUES ($1, $2, $3, $4, $5, $6)
            "#,
        )
        .bind(api_key_id)
        .bind(env_id)
        .bind(description)
        .bind(key_data)
        .bind(created_by_user_id)
        .bind(permissions)
        .execute(db)
        .await?;

        Ok(())
    }

    /// Update API key permissions
    pub async fn update_permissions(
        db: &crate::db::DatabasePool,
        api_key_id: &Uuid,
        permissions: &serde_json::Value,
        updated_by_user_id: &Uuid,
    ) -> Result<(), ApiKeyError> {
        match db {
            crate::db::DatabasePool::Sqlite(db) => {
                let permissions_str = serde_json::to_string(permissions)
                    .map_err(|_| ApiKeyError::SerializationError)?;

                sqlx::query(
                    r#"
                    UPDATE api_key
                    SET permissions = ?, updated_at = CURRENT_TIMESTAMP, updated_by_user_id = ?
                    WHERE api_key_id = ?
                    "#,
                )
                .bind(permissions_str)
                .bind(updated_by_user_id)
                .bind(api_key_id)
                .execute(db)
                .await?;
            }
            crate::db::DatabasePool::Postgres(db) => {
                sqlx::query(
                    r#"
                    UPDATE api_key
                    SET permissions = $1, updated_at = now(), updated_by_user_id = $2
                    WHERE api_key_id = $3
                    "#,
                )
                .bind(permissions)
                .bind(updated_by_user_id)
                .bind(api_key_id)
                .execute(db)
                .await?;
            }
        }
        Ok(())
    }

    /// Update API key description
    pub async fn update_description(
        db: &crate::db::DatabasePool,
        api_key_id: &Uuid,
        description: &str,
        updated_by_user_id: &Uuid,
    ) -> Result<(), ApiKeyError> {
        match db {
            crate::db::DatabasePool::Sqlite(db) => {
                Self::update_description_sqlite(db, api_key_id, description, updated_by_user_id)
                    .await
            }
            crate::db::DatabasePool::Postgres(db) => {
                Self::update_description_postgres(db, api_key_id, description, updated_by_user_id)
                    .await
            }
        }
    }

    async fn update_description_sqlite(
        db: &Pool<Sqlite>,
        api_key_id: &Uuid,
        description: &str,
        updated_by_user_id: &Uuid,
    ) -> Result<(), ApiKeyError> {
        sqlx::query(
            r#"
            UPDATE api_key
            SET description = ?, updated_at = CURRENT_TIMESTAMP, updated_by_user_id = ?
            WHERE api_key_id = ?
            "#,
        )
        .bind(description)
        .bind(updated_by_user_id)
        .bind(api_key_id)
        .execute(db)
        .await?;

        Ok(())
    }

    async fn update_description_postgres(
        db: &Pool<Postgres>,
        api_key_id: &Uuid,
        description: &str,
        updated_by_user_id: &Uuid,
    ) -> Result<(), ApiKeyError> {
        sqlx::query(
            r#"
            UPDATE api_key
            SET description = $1, updated_at = now(), updated_by_user_id = $2
            WHERE api_key_id = $3
            "#,
        )
        .bind(description)
        .bind(updated_by_user_id)
        .bind(api_key_id)
        .execute(db)
        .await?;

        Ok(())
    }

    /// Toggle API key active status
    pub async fn toggle_active(
        db: &crate::db::DatabasePool,
        api_key_id: &Uuid,
        active: bool,
        updated_by_user_id: &Uuid,
    ) -> Result<(), ApiKeyError> {
        match db {
            crate::db::DatabasePool::Sqlite(db) => {
                Self::toggle_active_sqlite(db, api_key_id, active, updated_by_user_id).await
            }
            crate::db::DatabasePool::Postgres(db) => {
                Self::toggle_active_postgres(db, api_key_id, active, updated_by_user_id).await
            }
        }
    }

    async fn toggle_active_sqlite(
        db: &Pool<Sqlite>,
        api_key_id: &Uuid,
        active: bool,
        updated_by_user_id: &Uuid,
    ) -> Result<(), ApiKeyError> {
        let active_int = if active { 1 } else { 0 };

        sqlx::query(
            r#"
            UPDATE api_key
            SET active = ?, updated_at = CURRENT_TIMESTAMP, updated_by_user_id = ?,
                active_toggle_at = CURRENT_TIMESTAMP, active_toggle_by_user_id = ?
            WHERE api_key_id = ?
            "#,
        )
        .bind(active_int)
        .bind(updated_by_user_id)
        .bind(updated_by_user_id)
        .bind(api_key_id)
        .execute(db)
        .await?;

        Ok(())
    }

    async fn toggle_active_postgres(
        db: &Pool<Postgres>,
        api_key_id: &Uuid,
        active: bool,
        updated_by_user_id: &Uuid,
    ) -> Result<(), ApiKeyError> {
        sqlx::query(
            r#"
            UPDATE api_key
            SET active = $1, updated_at = now(), updated_by_user_id = $2,
                active_toggle_at = now(), active_toggle_by_user_id = $2
            WHERE api_key_id = $3
            "#,
        )
        .bind(active)
        .bind(updated_by_user_id)
        .bind(api_key_id)
        .execute(db)
        .await?;

        Ok(())
    }

    /// Generate a new API key and return both the key and its hash.
    ///
    /// New keys use SHA-256 hashing (fast, appropriate for high-entropy secrets).
    /// Existing keys with PBKDF2 hashes continue to verify correctly thanks to
    /// algorithm dispatch in `verify_password`.
    pub fn generate_api_key(api_key_id: &Uuid) -> Result<(String, String), ApiKeyError> {
        // Generate 32 random bytes and convert to 64 hex characters
        let mut random_bytes = [0u8; 32];
        use rand::RngCore;
        rand::thread_rng().fill_bytes(&mut random_bytes);

        // Convert to hex string (64 characters)
        let random_hex = hex::encode(random_bytes);

        // Remove dashes from UUID for the API key format
        let uuid_no_dashes = api_key_id.to_string().replace("-", "");

        // Create the API key in the format: hot_<uuid_without_dashes>_<64_hex>
        let api_key = format!("hot_{}_{}", uuid_no_dashes, random_hex);

        // Hash only the random hex portion using SHA-256.
        // SHA-256 is appropriate here because the secret has 256 bits of entropy.
        let key_data_json =
            crate::auth::hash_secret_sha256(&random_hex).map_err(|_| ApiKeyError::HashingError)?;

        Ok((api_key, key_data_json))
    }

    /// Parse an API key to extract UUID and random hex parts
    pub fn parse_api_key(key: &str) -> Result<(Uuid, String), ApiKeyError> {
        // Expected format: hot_<uuid_without_dashes>_<64_hex>
        if !key.starts_with("hot_") {
            return Err(ApiKeyError::InvalidKey);
        }

        let parts: Vec<&str> = key.split('_').collect();
        if parts.len() != 3 {
            return Err(ApiKeyError::InvalidKey);
        }

        let uuid_no_dashes = parts[1];
        let random_hex = parts[2];

        // Validate UUID format (32 hex characters)
        if uuid_no_dashes.len() != 32 || !uuid_no_dashes.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(ApiKeyError::InvalidKey);
        }

        // Validate random hex (64 hex characters)
        if random_hex.len() != 64 || !random_hex.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(ApiKeyError::InvalidKey);
        }

        // Convert UUID back to standard format with dashes
        let uuid_with_dashes = format!(
            "{}-{}-{}-{}-{}",
            &uuid_no_dashes[0..8],
            &uuid_no_dashes[8..12],
            &uuid_no_dashes[12..16],
            &uuid_no_dashes[16..20],
            &uuid_no_dashes[20..32]
        );

        let uuid = Uuid::parse_str(&uuid_with_dashes).map_err(|_| ApiKeyError::InvalidKey)?;

        Ok((uuid, random_hex.to_string()))
    }

    /// Verify an API key against stored hash data
    pub fn verify_key(key: &str, key_data: &serde_json::Value) -> Result<bool, ApiKeyError> {
        // Parse the API key to extract the random hex portion
        let (_, random_hex) = match Self::parse_api_key(key) {
            Ok(result) => result,
            Err(ApiKeyError::InvalidKey) => {
                // Invalid key format should return false, not an error
                return Ok(false);
            }
            Err(e) => return Err(e), // Other errors should still be propagated
        };

        // Convert the key_data to a compact JSON string format that auth::verify_password expects
        let key_data_str =
            serde_json::to_string(key_data).map_err(|_| ApiKeyError::SerializationError)?;

        // Use the auth module's verification function on only the random hex portion
        crate::auth::verify_password(&random_hex, &key_data_str)
            .map_err(|_| ApiKeyError::HashingError)
    }

    /// Verify an API key and return the associated API key record if valid and active
    pub async fn verify_api_key(
        db: &crate::db::DatabasePool,
        api_key: &str,
    ) -> Result<Option<ApiKey>, ApiKeyError> {
        // Parse the API key once to extract both the api_key_id and random portion
        let (api_key_id, random_hex) = match Self::parse_api_key(api_key) {
            Ok(result) => result,
            Err(ApiKeyError::InvalidKey) => {
                // Invalid key format should return None, not an error
                return Ok(None);
            }
            Err(e) => return Err(e), // Other errors should still be propagated
        };

        // Look up the API key record directly by api_key_id
        let key_record = match Self::get_api_key(db, &api_key_id).await {
            Ok(key) => key,
            Err(ApiKeyError::NotFound) => return Ok(None),
            Err(e) => return Err(e),
        };

        // Check if the key is active
        if !key_record.active {
            return Ok(None);
        }

        // Verify the random portion directly against the stored hash data
        let key_data_str = serde_json::to_string(&key_record.key_data)
            .map_err(|_| ApiKeyError::SerializationError)?;

        match crate::auth::verify_password(&random_hex, &key_data_str) {
            Ok(true) => Ok(Some(key_record)),
            Ok(false) => Ok(None),
            Err(_) => Err(ApiKeyError::HashingError),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_api_key_generation() {
        let api_key_id = Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap();
        let (full_key, hash_data_json) = ApiKey::generate_api_key(&api_key_id).unwrap();

        // Check format - should start with "hot_" and have correct structure
        assert!(full_key.starts_with("hot_"));

        // Should have 3 parts when split by '_'
        let parts: Vec<&str> = full_key.split('_').collect();
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[0], "hot");

        // UUID part should be 32 hex characters (UUID without dashes)
        assert_eq!(parts[1].len(), 32);
        assert!(parts[1].chars().all(|c| c.is_ascii_hexdigit()));

        // Random part should be 64 hex characters
        assert_eq!(parts[2].len(), 64);
        assert!(parts[2].chars().all(|c| c.is_ascii_hexdigit()));

        // Verify we can parse the hash data as JSON
        let _hash_data: serde_json::Value = serde_json::from_str(&hash_data_json).unwrap();

        // Test that verification works
        assert!(ApiKey::verify_key(&full_key, &_hash_data).unwrap());

        // Test that wrong key fails verification
        assert!(!ApiKey::verify_key("hot_550e8400e29b41d4a716446655440000_0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef", &_hash_data).unwrap());
    }

    #[test]
    fn test_api_key_parsing() {
        let valid_key = "hot_550e8400e29b41d4a716446655440000_0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let (uuid, random_hex) = ApiKey::parse_api_key(valid_key).unwrap();

        assert_eq!(
            uuid,
            Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap()
        );
        assert_eq!(
            random_hex,
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
        );

        // Test invalid formats
        assert!(ApiKey::parse_api_key("invalid_key").is_err());
        assert!(ApiKey::parse_api_key("hot_invalid_uuid").is_err());
        assert!(ApiKey::parse_api_key("hot_550e8400e29b41d4a716446655440000_short").is_err());
        assert!(ApiKey::parse_api_key("hot_550e8400e29b41d4a716446655440000").is_err()); // Missing random part
    }

    #[test]
    fn test_api_key_verification() {
        let api_key_id = Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap();
        let (api_key, hash_data_json) = ApiKey::generate_api_key(&api_key_id).unwrap();
        let hash_data: serde_json::Value = serde_json::from_str(&hash_data_json).unwrap();

        // Correct key should verify
        assert!(ApiKey::verify_key(&api_key, &hash_data).unwrap());

        // Wrong key should not verify (different random hex)
        assert!(!ApiKey::verify_key("hot_550e8400e29b41d4a716446655440000_1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef", &hash_data).unwrap());

        // Empty key should not verify
        assert!(!ApiKey::verify_key("", &hash_data).unwrap());

        // Invalid format should not verify
        assert!(!ApiKey::verify_key("invalid-key", &hash_data).unwrap());
    }
}

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, Pool, Sqlite};
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
        db: &crate::db::ScopedPgPool,
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

    /// API keys of an environment, newest first. `None` limit returns every key.
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
        // SQLite treats a negative LIMIT as no limit.
        let limit = limit.unwrap_or(-1);
        let offset = offset.unwrap_or(0);

        let rows = sqlx::query_as::<_, ApiKey>(
            r#"
            SELECT api_key_id, env_id, description, key_data, active,
                   created_by_user_id, created_at, updated_at, updated_by_user_id,
                   active_toggle_at, active_toggle_by_user_id, permissions
            FROM api_key
            WHERE env_id = ?
            ORDER BY created_at DESC, api_key_id DESC
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
        db: &crate::db::ScopedPgPool,
        env_id: &Uuid,
        limit: Option<i64>,
        offset: Option<i64>,
    ) -> Result<Vec<ApiKey>, ApiKeyError> {
        // PostgreSQL treats LIMIT NULL as no limit.
        let offset = offset.unwrap_or(0);

        let rows = sqlx::query_as::<_, ApiKey>(
            r#"
            SELECT api_key_id, env_id, description, key_data, active,
                   created_by_user_id, created_at, updated_at, updated_by_user_id,
                   active_toggle_at, active_toggle_by_user_id, permissions
            FROM api_key
            WHERE env_id = $1
            ORDER BY created_at DESC, api_key_id DESC
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

    /// Number of API keys in an environment.
    pub async fn count_api_keys_by_env(
        db: &crate::db::DatabasePool,
        env_id: &Uuid,
    ) -> Result<i64, ApiKeyError> {
        let sql = "SELECT COUNT(*) FROM api_key WHERE env_id = $1";
        Ok(match db {
            crate::db::DatabasePool::Sqlite(pool) => {
                sqlx::query_scalar(sql).bind(env_id).fetch_one(pool).await?
            }
            crate::db::DatabasePool::Postgres(pool) => {
                sqlx::query_scalar(sql).bind(env_id).fetch_one(pool).await?
            }
        })
    }

    /// Get the most recently created active API key for an environment that
    /// no org API key issued. Dashboard service keys take this key as
    /// their parent, and an issued key's grants stay bounded by its issuer.
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
                WHERE env_id = ? AND active = 1 AND NOT EXISTS (SELECT 1 FROM api_key_issuer m WHERE m.api_key_id=api_key.api_key_id)
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
                WHERE env_id = $1 AND active = TRUE AND NOT EXISTS (SELECT 1 FROM api_key_issuer m WHERE m.api_key_id=api_key.api_key_id)
                ORDER BY created_at DESC
                LIMIT 1
                "#,
            )
            .bind(env_id)
            .fetch_optional(db)
            .await?),
        }
    }

    /// API keys of an organization's environments, newest first. `None` limit
    /// returns every key.
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
        let limit = limit.unwrap_or(-1);
        let offset = offset.unwrap_or(0);

        let rows = sqlx::query_as::<_, ApiKey>(
            r#"
            SELECT ak.api_key_id, ak.env_id, ak.description, ak.key_data, ak.active,
                   ak.created_by_user_id, ak.created_at, ak.updated_at, ak.updated_by_user_id,
                   ak.active_toggle_at, ak.active_toggle_by_user_id, ak.permissions
            FROM api_key ak
            JOIN env e ON ak.env_id = e.env_id
            WHERE e.org_id = ?
            ORDER BY ak.created_at DESC, ak.api_key_id DESC
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
        db: &crate::db::ScopedPgPool,
        org_id: &Uuid,
        limit: Option<i64>,
        offset: Option<i64>,
    ) -> Result<Vec<ApiKey>, ApiKeyError> {
        let offset = offset.unwrap_or(0);

        let rows = sqlx::query_as::<_, ApiKey>(
            r#"
            SELECT ak.api_key_id, ak.env_id, ak.description, ak.key_data, ak.active,
                   ak.created_by_user_id, ak.created_at, ak.updated_at, ak.updated_by_user_id,
                   ak.active_toggle_at, ak.active_toggle_by_user_id, ak.permissions
            FROM api_key ak
            JOIN env e ON ak.env_id = e.env_id
            WHERE e.org_id = $1
            ORDER BY ak.created_at DESC, ak.api_key_id DESC
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
        db: &crate::db::ScopedPgPool,
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
        db: &crate::db::ScopedPgPool,
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
        db: &crate::db::ScopedPgPool,
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
        use rand::Rng;
        rand::rng().fill_bytes(&mut random_bytes);

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
        let state = Self::get_credential_state(db, &api_key_id).await?;
        if state.is_expired() {
            return Ok(None);
        }

        // Verify the random portion directly against the stored hash data
        if Self::secret_matches(&random_hex, &key_record.key_data)? {
            return Ok(Some(key_record));
        }
        // A rotated key's previous secret works until its overlap ends.
        match (&state.previous_key_data, state.previous_key_expires_at) {
            (Some(previous), Some(until))
                if until > Utc::now() && Self::secret_matches(&random_hex, previous)? =>
            {
                Ok(Some(key_record))
            }
            _ => Ok(None),
        }
    }

    pub(crate) fn secret_matches(
        random_hex: &str,
        key_data: &serde_json::Value,
    ) -> Result<bool, ApiKeyError> {
        let key_data_str =
            serde_json::to_string(key_data).map_err(|_| ApiKeyError::SerializationError)?;
        crate::auth::verify_password(random_hex, &key_data_str)
            .map_err(|_| ApiKeyError::HashingError)
    }

    /// Expiry and rotation state. Kept out of `ApiKey` so existing queries and
    /// constructors do not change.
    pub async fn get_credential_state(
        db: &crate::db::DatabasePool,
        api_key_id: &Uuid,
    ) -> Result<ApiKeyCredentialState, ApiKeyError> {
        type Row = (Option<DateTime<Utc>>, Option<String>, Option<DateTime<Utc>>);
        let sql = "SELECT expires_at, CAST(previous_key_data AS TEXT), previous_key_expires_at FROM api_key WHERE api_key_id = $1";
        let row: Row = match db {
            crate::db::DatabasePool::Sqlite(pool) => {
                sqlx::query_as(sql).bind(api_key_id).fetch_one(pool).await
            }
            crate::db::DatabasePool::Postgres(pool) => {
                sqlx::query_as(sql).bind(api_key_id).fetch_one(pool).await
            }
        }
        .map_err(|e| match e {
            sqlx::Error::RowNotFound => ApiKeyError::NotFound,
            other => ApiKeyError::Database(other),
        })?;
        Ok(ApiKeyCredentialState {
            expires_at: row.0,
            previous_key_data: row
                .1
                .map(|value| serde_json::from_str(&value))
                .transpose()
                .map_err(|_| ApiKeyError::SerializationError)?,
            previous_key_expires_at: row.2,
        })
    }

    /// Issue a new secret for the same key ID. The previous secret keeps
    /// working until `overlap` ends; lineage, grants and expiry are unchanged.
    pub async fn rotate(
        db: &crate::db::DatabasePool,
        api_key_id: &Uuid,
        overlap: chrono::Duration,
        updated_by_user_id: &Uuid,
    ) -> Result<String, ApiKeyError> {
        let (token, hash) = Self::generate_api_key(api_key_id)?;
        let key_data: serde_json::Value =
            serde_json::from_str(&hash).map_err(|_| ApiKeyError::SerializationError)?;
        let until = Utc::now() + overlap;
        let result = match db {
            crate::db::DatabasePool::Sqlite(pool) => {
                sqlx::query("UPDATE api_key SET previous_key_data = key_data, previous_key_expires_at = ?, key_data = ?, updated_at = CURRENT_TIMESTAMP, updated_by_user_id = ? WHERE api_key_id = ? AND active = 1")
                    .bind(until)
                    .bind(key_data.to_string())
                    .bind(updated_by_user_id)
                    .bind(api_key_id)
                    .execute(pool)
                    .await?
                    .rows_affected()
            }
            crate::db::DatabasePool::Postgres(pool) => {
                sqlx::query("UPDATE api_key SET previous_key_data = key_data, previous_key_expires_at = $1, key_data = $2, updated_at = now(), updated_by_user_id = $3 WHERE api_key_id = $4 AND active")
                    .bind(until)
                    .bind(&key_data)
                    .bind(updated_by_user_id)
                    .bind(api_key_id)
                    .execute(pool)
                    .await?
                    .rows_affected()
            }
        };
        if result == 0 {
            return Err(ApiKeyError::NotFound);
        }
        Ok(token)
    }
}

/// Expiry and rotation state of an environment API key.
#[derive(Debug, Clone, Default)]
pub struct ApiKeyCredentialState {
    pub expires_at: Option<DateTime<Utc>>,
    pub previous_key_data: Option<serde_json::Value>,
    pub previous_key_expires_at: Option<DateTime<Utc>>,
}

impl ApiKeyCredentialState {
    pub fn is_expired(&self) -> bool {
        self.expires_at.is_some_and(|at| at <= Utc::now())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn default_environment_key_skips_org_key_issued_keys() {
        let db = crate::db::test_db().await;
        let data = crate::db::insert_test_data(&db).await.unwrap();
        let crate::db::DatabasePool::Sqlite(pool) = &db else {
            return;
        };
        let issued = Uuid::now_v7();
        ApiKey::insert_api_key(
            &db,
            &issued,
            &data.env_id,
            "issued",
            &serde_json::json!({}),
            &data.user_id,
            &serde_json::json!({"*:*": ["*"]}),
        )
        .await
        .unwrap();
        // The issued key is the environment's newest.
        sqlx::query("UPDATE api_key SET created_at=datetime('now','+1 minute') WHERE api_key_id=?")
            .bind(issued)
            .execute(pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO api_key_issuer(api_key_id,issuer_org_api_key_id) VALUES(?,?)")
            .bind(issued)
            .bind(Uuid::now_v7())
            .execute(pool)
            .await
            .unwrap();
        let chosen = ApiKey::get_active_api_key_by_env(&db, &data.env_id)
            .await
            .unwrap();
        assert!(chosen.is_none_or(|key| key.api_key_id != issued));
    }

    #[tokio::test]
    async fn environment_key_listing_has_no_default_cap() {
        let db = crate::db::test_db().await;
        let data = crate::db::insert_test_data(&db).await.unwrap();
        let before = ApiKey::count_api_keys_by_env(&db, &data.env_id)
            .await
            .unwrap();
        for n in 0..12 {
            ApiKey::insert_api_key(
                &db,
                &Uuid::now_v7(),
                &data.env_id,
                &format!("key {n}"),
                &serde_json::json!({}),
                &data.user_id,
                &serde_json::json!({}),
            )
            .await
            .unwrap();
        }
        let total = ApiKey::count_api_keys_by_env(&db, &data.env_id)
            .await
            .unwrap();
        assert_eq!(total, before + 12);
        let all = ApiKey::get_api_keys_by_env(&db, &data.env_id, None, None)
            .await
            .unwrap();
        assert_eq!(all.len() as i64, total);
        // Pages follow one stable order, even for keys created in the same instant.
        let mut paged = Vec::new();
        for offset in (0..total).step_by(5) {
            paged.extend(
                ApiKey::get_api_keys_by_env(&db, &data.env_id, Some(5), Some(offset))
                    .await
                    .unwrap()
                    .into_iter()
                    .map(|key| key.api_key_id),
            );
        }
        assert_eq!(
            paged,
            all.iter().map(|key| key.api_key_id).collect::<Vec<_>>()
        );
    }

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

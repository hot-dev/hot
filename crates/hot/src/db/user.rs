use chrono::{DateTime, Utc};
use serde_json::Value as JsonValue;
use sqlx::FromRow;
use sqlx::types::Uuid;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum UserError {
    #[error("Database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("User not found")]
    NotFound,
}

impl UserError {
    /// Lookups report a missing row either way, depending on the query.
    pub fn is_not_found(&self) -> bool {
        matches!(
            self,
            Self::NotFound | Self::Database(sqlx::Error::RowNotFound)
        )
    }
}

/// User settings structure stored as JSON
/// {
///   "display": {
///     "timezone": "America/New_York",  // IANA timezone; null = use org default
///     "value_format": "hot"            // "hot" (default) or "json" for value display
///   },
///   "notifications": {
///     "newsletter": true,
///     "product_updates": true
///   }
/// }
#[derive(Debug, Clone, FromRow)]
pub struct User {
    pub user_id: Uuid,
    pub email: String,
    pub name: Option<String>,
    pub settings: Option<JsonValue>,
    pub active: bool,
    pub created_at: DateTime<Utc>,
    pub created_by_user_id: Uuid,
    pub updated_at: DateTime<Utc>,
    pub updated_by_user_id: Option<Uuid>,
    pub active_toggle_at: Option<DateTime<Utc>>,
    pub active_toggle_by_user_id: Option<Uuid>,
}

#[derive(Debug, FromRow)]
pub struct UserAuth {
    pub user_auth_id: Uuid,
    pub user_id: Uuid,
    pub auth_type: String,
    pub auth_identifier: String,
    pub auth_data: Option<JsonValue>,
    pub active: bool,
    pub created_at: DateTime<Utc>,
    pub created_by_user_id: Uuid,
    pub updated_at: DateTime<Utc>,
    pub updated_by_user_id: Option<Uuid>,
    pub active_toggle_at: Option<DateTime<Utc>>,
    pub active_toggle_by_user_id: Option<Uuid>,
}

impl User {
    /// Get user by ID
    pub async fn get_user(db: &crate::db::DatabasePool, user_id: &Uuid) -> Result<User, UserError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let user = sqlx::query_as::<_, User>(
                    r#"SELECT user_id, email, name, settings, active, created_at, created_by_user_id, updated_at, updated_by_user_id, active_toggle_at, active_toggle_by_user_id FROM "user" WHERE user_id = $1"#
                )
                .bind(user_id)
                .fetch_one(pg_pool)
                .await?;
                Ok(user)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let user = sqlx::query_as::<_, User>(
                    "SELECT user_id, email, name, settings, active, created_at, created_by_user_id, updated_at, updated_by_user_id, active_toggle_at, active_toggle_by_user_id FROM user WHERE user_id = ?"
                )
                .bind(user_id)
                .fetch_one(sqlite_pool)
                .await?;
                Ok(user)
            }
        }
    }

    /// Get user by email
    pub async fn get_user_by_email(
        db: &crate::db::DatabasePool,
        email: &str,
    ) -> Result<User, UserError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let user = sqlx::query_as::<_, User>(
                    r#"SELECT user_id, email, name, settings, active, created_at, created_by_user_id, updated_at, updated_by_user_id, active_toggle_at, active_toggle_by_user_id FROM "user" WHERE email = $1"#
                )
                .bind(email)
                .fetch_one(pg_pool)
                .await?;
                Ok(user)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let user = sqlx::query_as::<_, User>(
                    "SELECT user_id, email, name, settings, active, created_at, created_by_user_id, updated_at, updated_by_user_id, active_toggle_at, active_toggle_by_user_id FROM user WHERE email = ?"
                )
                .bind(email)
                .fetch_one(sqlite_pool)
                .await?;
                Ok(user)
            }
        }
    }

    /// Get count of users
    pub async fn get_count(db: &crate::db::DatabasePool) -> Result<i64, UserError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let count: i64 = sqlx::query_scalar(r#"SELECT COUNT(*) FROM "user""#)
                    .fetch_one(pg_pool)
                    .await?;
                Ok(count)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM user")
                    .fetch_one(sqlite_pool)
                    .await?;
                Ok(count)
            }
        }
    }

    /// Get the default (first) user
    pub async fn get_default_user(db: &crate::db::DatabasePool) -> Result<User, UserError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let user = sqlx::query_as::<_, User>(
                    r#"SELECT user_id, email, name, settings, active, created_at, created_by_user_id, updated_at, updated_by_user_id, active_toggle_at, active_toggle_by_user_id FROM "user" ORDER BY created_at LIMIT 1"#
                )
                .fetch_one(pg_pool)
                .await?;
                Ok(user)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let user = sqlx::query_as::<_, User>(
                    "SELECT user_id, email, name, settings, active, created_at, created_by_user_id, updated_at, updated_by_user_id, active_toggle_at, active_toggle_by_user_id FROM user ORDER BY created_at LIMIT 1"
                )
                .fetch_one(sqlite_pool)
                .await?;
                Ok(user)
            }
        }
    }

    /// Get the first user (alias for get_default_user)
    pub async fn get_first_user(db: &crate::db::DatabasePool) -> Result<User, UserError> {
        Self::get_default_user(db).await
    }

    /// Insert a new user
    pub async fn insert_user(
        db: &crate::db::DatabasePool,
        user_id: &Uuid,
        email: &str,
        name: Option<&str>,
        created_by_user_id: Option<&Uuid>,
    ) -> Result<(), UserError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                sqlx::query(r#"INSERT INTO "user" (user_id, email, name, created_by_user_id) VALUES ($1, $2, $3, $4)"#)
                    .bind(user_id)
                    .bind(email)
                    .bind(name)
                    .bind(created_by_user_id)
                    .execute(pg_pool)
                    .await?;
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                sqlx::query("INSERT INTO user (user_id, email, name, created_by_user_id) VALUES (?, ?, ?, ?)")
                    .bind(user_id)
                    .bind(email)
                    .bind(name)
                    .bind(created_by_user_id)
                    .execute(sqlite_pool)
                    .await?;
            }
        }
        Ok(())
    }

    /// An org's directory: active users who still belong to its account and
    /// either hold an active grant on the org or administer its root, whose
    /// access the org inherits. It reads under the membership bootstrap: an
    /// environment-scoped caller, such as an alert published from code,
    /// can't see the root's memberships.
    pub async fn get_users_by_org(
        db: &crate::db::DatabasePool,
        org_id: &Uuid,
    ) -> Result<Vec<User>, UserError> {
        let directory = db.with_scope(crate::db::DatabaseScope::Bootstrap(
            crate::db::BootstrapOperation::Membership,
        ));
        let query = r#"
            SELECT u.user_id, u.email, u.name, u.settings, u.active, u.created_at, u.created_by_user_id, u.updated_at, u.updated_by_user_id, u.active_toggle_at, u.active_toggle_by_user_id
            FROM "user" u
            JOIN org o ON o.org_id = $1
            JOIN org_user ru ON ru.org_id = COALESCE(o.parent_org_id, o.org_id)
                AND ru.user_id = u.user_id AND ru.active = true
            WHERE u.active = true
              AND (ru.org_user_role_id = 2 OR o.parent_org_id IS NULL OR EXISTS (
                  SELECT 1 FROM org_user cu
                  WHERE cu.org_id = o.org_id AND cu.user_id = u.user_id AND cu.active = true))
            ORDER BY u.created_at
        "#;
        match &directory {
            crate::db::DatabasePool::Postgres(pool) => {
                Ok(sqlx::query_as(query).bind(org_id).fetch_all(pool).await?)
            }
            crate::db::DatabasePool::Sqlite(pool) => {
                Ok(sqlx::query_as(query).bind(org_id).fetch_all(pool).await?)
            }
        }
    }

    /// Update user name
    pub async fn update_name(
        db: &crate::db::DatabasePool,
        user_id: &Uuid,
        name: Option<&str>,
    ) -> Result<(), UserError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                sqlx::query(
                    r#"UPDATE "user" SET name = $2, updated_at = NOW() WHERE user_id = $1"#,
                )
                .bind(user_id)
                .bind(name)
                .execute(pg_pool)
                .await?;
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                sqlx::query(
                    "UPDATE user SET name = ?, updated_at = CURRENT_TIMESTAMP WHERE user_id = ?",
                )
                .bind(name)
                .bind(user_id)
                .execute(sqlite_pool)
                .await?;
            }
        }
        Ok(())
    }

    /// Update notification preferences within the settings JSON
    /// This updates settings.notifications without affecting other settings
    pub async fn update_notification_preferences(
        db: &crate::db::DatabasePool,
        user_id: &Uuid,
        preferences: &JsonValue,
    ) -> Result<(), UserError> {
        Self::update_setting(db, user_id, "notifications", preferences).await
    }

    /// Update a specific setting path within the settings JSON
    /// This uses JSON merge to update only the specified key without overwriting other settings
    pub async fn update_setting(
        db: &crate::db::DatabasePool,
        user_id: &Uuid,
        key: &str,
        value: &JsonValue,
    ) -> Result<(), UserError> {
        // Build the update object with just this key
        let update_obj = serde_json::json!({ key: value });

        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                // Use COALESCE to handle NULL settings, then merge with jsonb_concat (||)
                sqlx::query(
                    r#"UPDATE "user" SET settings = COALESCE(settings, '{}'::jsonb) || $2::jsonb, updated_at = NOW() WHERE user_id = $1"#,
                )
                .bind(user_id)
                .bind(&update_obj)
                .execute(pg_pool)
                .await?;
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                // For SQLite, we need to fetch, merge in Rust, then update
                let current = Self::get_user(db, user_id).await?;
                let mut settings = current.settings.unwrap_or(serde_json::json!({}));
                if let Some(obj) = settings.as_object_mut() {
                    obj.insert(key.to_string(), value.clone());
                }
                let settings_str = serde_json::to_string(&settings)
                    .map_err(|e| UserError::Database(sqlx::Error::Encode(Box::new(e))))?;
                sqlx::query(
                    "UPDATE user SET settings = ?, updated_at = CURRENT_TIMESTAMP WHERE user_id = ?",
                )
                .bind(settings_str)
                .bind(user_id)
                .execute(sqlite_pool)
                .await?;
            }
        }
        Ok(())
    }

    /// Update the display timezone setting
    /// Pass None to clear the user's timezone (will fall back to org default)
    pub async fn update_display_timezone(
        db: &crate::db::DatabasePool,
        user_id: &Uuid,
        timezone: Option<&str>,
    ) -> Result<(), UserError> {
        Self::update_display_setting(db, user_id, "timezone", timezone).await
    }

    /// Get the user's display timezone from settings
    /// Returns None if not set (should fall back to org default)
    pub fn get_display_timezone(&self) -> Option<String> {
        self.get_display_setting("timezone")
    }

    /// Update the value format preference setting
    /// Accepts "hot" (default) or "json"
    pub async fn update_value_format(
        db: &crate::db::DatabasePool,
        user_id: &Uuid,
        format: Option<&str>,
    ) -> Result<(), UserError> {
        Self::update_display_setting(db, user_id, "value_format", format).await
    }

    /// Get the user's value format preference
    /// Returns "hot" as default if not set
    pub fn get_value_format(&self) -> String {
        self.get_display_setting("value_format")
            .unwrap_or_else(|| "hot".to_string())
    }

    /// Helper to get a display setting by key
    fn get_display_setting(&self, key: &str) -> Option<String> {
        self.settings
            .as_ref()
            .and_then(|s| s.get("display"))
            .and_then(|d| d.get(key))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
    }

    /// Helper to update a single display setting without overwriting others
    async fn update_display_setting(
        db: &crate::db::DatabasePool,
        user_id: &Uuid,
        key: &str,
        value: Option<&str>,
    ) -> Result<(), UserError> {
        // Get current user to preserve existing display settings
        let user = Self::get_user(db, user_id).await?;
        let mut display = user
            .settings
            .as_ref()
            .and_then(|s| s.get("display"))
            .cloned()
            .unwrap_or(serde_json::json!({}));

        // Update the specific key
        if let Some(obj) = display.as_object_mut() {
            match value {
                Some(v) => {
                    obj.insert(key.to_string(), serde_json::json!(v));
                }
                None => {
                    obj.insert(key.to_string(), serde_json::Value::Null);
                }
            }
        }

        Self::update_setting(db, user_id, "display", &display).await
    }

    /// Get notification preferences from settings
    pub fn get_notification_preferences(&self) -> JsonValue {
        self.settings
            .as_ref()
            .and_then(|s| s.get("notifications"))
            .cloned()
            .unwrap_or(serde_json::json!({
                "newsletter": true,
                "product_updates": true,
                "alerts": true
            }))
    }

    /// Check if user has opted in to alert emails (default: true)
    pub fn alerts_enabled(&self) -> bool {
        self.settings
            .as_ref()
            .and_then(|s| s.get("notifications"))
            .and_then(|n| n.get("alerts"))
            .and_then(|v| v.as_bool())
            .unwrap_or(true)
    }

    /// Get active users in an org who have alerts enabled, returning (user_id, email)
    pub async fn get_alert_recipients_by_org(
        db: &crate::db::DatabasePool,
        org_id: &Uuid,
    ) -> Result<Vec<(Uuid, String)>, UserError> {
        let users = Self::get_users_by_org(db, org_id).await?;
        Ok(users
            .into_iter()
            .filter(|u| u.active && u.alerts_enabled())
            .map(|u| (u.user_id, u.email))
            .collect())
    }

    /// Get active users in a team who have alerts enabled, returning (user_id, email)
    pub async fn get_alert_recipients_by_team(
        db: &crate::db::DatabasePool,
        team_id: &Uuid,
    ) -> Result<Vec<(Uuid, String)>, UserError> {
        let users: Vec<User> = match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                sqlx::query_as::<_, User>(
                    "SELECT u.* FROM \"user\" u
                     JOIN team_user tu ON u.user_id = tu.user_id
                     WHERE tu.team_id = $1 AND tu.active = true AND u.active = true",
                )
                .bind(team_id)
                .fetch_all(pg_pool)
                .await?
            }
            crate::db::DatabasePool::Sqlite(pool) => {
                sqlx::query_as::<_, User>(
                    "SELECT u.* FROM user u
                     JOIN team_user tu ON u.user_id = tu.user_id
                     WHERE tu.team_id = ? AND tu.active = 1 AND u.active = 1",
                )
                .bind(team_id)
                .fetch_all(pool)
                .await?
            }
        };
        let team = crate::db::Team::get_team(db, team_id)
            .await
            .map_err(|e| UserError::Database(sqlx::Error::Protocol(e.to_string())))?;
        let allowed = Self::get_users_by_org(db, &team.org_id).await?;
        let users: Vec<_> = users
            .into_iter()
            .filter(|user| allowed.iter().any(|member| member.user_id == user.user_id))
            .collect();
        Ok(users
            .into_iter()
            .filter(|u: &User| u.alerts_enabled())
            .map(|u| (u.user_id, u.email))
            .collect())
    }
}

impl UserAuth {
    /// Get user auth by auth type and identifier
    pub async fn get_user_auth(
        db: &crate::db::DatabasePool,
        auth_type: &str,
        auth_identifier: &str,
    ) -> Result<UserAuth, UserError> {
        tracing::debug!(
            "UserAuth::get_user_auth called with auth_type='{}', auth_identifier='{}'",
            auth_type,
            auth_identifier
        );

        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                tracing::debug!("Using Postgres database");
                let user_auth = sqlx::query_as::<_, UserAuth>(
                    "SELECT user_auth_id, user_id, auth_type, auth_identifier, auth_data, active, created_at, created_by_user_id, updated_at, updated_by_user_id, active_toggle_at, active_toggle_by_user_id FROM user_auth WHERE auth_type = $1 AND auth_identifier = $2",
                )
                .bind(auth_type)
                .bind(auth_identifier)
                .fetch_one(pg_pool)
                .await
                .map_err(|e| {
                    // An unknown sign-in is routine; only real failures are errors.
                    if matches!(e, sqlx::Error::RowNotFound) {
                        tracing::debug!("No user_auth record found for auth_type='{}', auth_identifier='{}'", auth_type, auth_identifier);
                    } else {
                        tracing::error!("Postgres query failed for auth_type='{}', auth_identifier='{}': {:?}", auth_type, auth_identifier, e);
                    }
                    UserError::Database(e)
                })?;
                tracing::debug!(
                    "Successfully found user_auth record in Postgres: user_id={}",
                    user_auth.user_id
                );
                Ok(user_auth)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                tracing::debug!("Using SQLite database");
                let user_auth = sqlx::query_as::<_, UserAuth>(
                    "SELECT user_auth_id, user_id, auth_type, auth_identifier, auth_data, active, created_at, created_by_user_id, updated_at, updated_by_user_id, active_toggle_at, active_toggle_by_user_id FROM user_auth WHERE auth_type = ? AND auth_identifier = ?",
                )
                .bind(auth_type)
                .bind(auth_identifier)
                .fetch_one(sqlite_pool)
                .await
                .map_err(|e| {
                    tracing::error!("SQLite query failed for auth_type='{}', auth_identifier='{}': {:?}", auth_type, auth_identifier, e);
                    // Log additional details for specific error types
                    match &e {
                        sqlx::Error::RowNotFound => {
                            tracing::warn!("No user_auth record found for auth_type='{}', auth_identifier='{}'", auth_type, auth_identifier);
                        }
                        sqlx::Error::Database(db_err) => {
                            tracing::error!("Database-specific error: code={:?}, message={}", db_err.code(), db_err.message());
                        }
                        sqlx::Error::ColumnNotFound(col) => {
                            tracing::error!("Column not found: {}", col);
                        }
                        sqlx::Error::TypeNotFound { type_name } => {
                            tracing::error!("Type not found: {}", type_name);
                        }
                        _ => {
                            tracing::error!("Other SQLite error: {:?}", e);
                        }
                    }
                    UserError::Database(e)
                })?;
                tracing::debug!(
                    "Successfully found user_auth record in SQLite: user_id={}",
                    user_auth.user_id
                );
                Ok(user_auth)
            }
        }
    }

    /// Insert user auth
    pub async fn insert_user_auth(
        db: &crate::db::DatabasePool,
        user_auth_id: &Uuid,
        user_id: &Uuid,
        auth_type: &str,
        auth_identifier: &str,
        auth_data: Option<&JsonValue>,
        created_by_user_id: &Uuid,
    ) -> Result<(), UserError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                sqlx::query("INSERT INTO user_auth (user_auth_id, user_id, auth_type, auth_identifier, auth_data, created_by_user_id) VALUES ($1, $2, $3, $4, $5, $6)")
                    .bind(user_auth_id)
                    .bind(user_id)
                    .bind(auth_type)
                    .bind(auth_identifier)
                    .bind(auth_data)
                    .bind(created_by_user_id)
                    .execute(pg_pool)
                    .await?;
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let auth_data_str = match auth_data {
                    Some(data) => Some(
                        serde_json::to_string(data)
                            .map_err(|e| UserError::Database(sqlx::Error::Encode(Box::new(e))))?,
                    ),
                    None => None,
                };
                sqlx::query("INSERT INTO user_auth (user_auth_id, user_id, auth_type, auth_identifier, auth_data, created_by_user_id) VALUES (?, ?, ?, ?, ?, ?)")
                    .bind(user_auth_id)
                    .bind(user_id)
                    .bind(auth_type)
                    .bind(auth_identifier)
                    .bind(auth_data_str)
                    .bind(created_by_user_id)
                    .execute(sqlite_pool)
                    .await?;
            }
        }
        Ok(())
    }

    /// Replace an auth row's stored credential, such as a password hash.
    pub async fn update_auth_data(
        db: &crate::db::DatabasePool,
        user_auth_id: &Uuid,
        auth_data: &JsonValue,
        updated_by_user_id: &Uuid,
    ) -> Result<(), UserError> {
        let updated = match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                sqlx::query("UPDATE user_auth SET auth_data = $1, updated_at = NOW(), updated_by_user_id = $2 WHERE user_auth_id = $3")
                    .bind(auth_data)
                    .bind(updated_by_user_id)
                    .bind(user_auth_id)
                    .execute(pg_pool)
                    .await?
                    .rows_affected()
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let auth_data = serde_json::to_string(auth_data)
                    .map_err(|e| UserError::Database(sqlx::Error::Encode(Box::new(e))))?;
                sqlx::query("UPDATE user_auth SET auth_data = ?, updated_at = CURRENT_TIMESTAMP, updated_by_user_id = ? WHERE user_auth_id = ?")
                    .bind(auth_data)
                    .bind(updated_by_user_id)
                    .bind(user_auth_id)
                    .execute(sqlite_pool)
                    .await?
                    .rows_affected()
            }
        };
        if updated == 0 {
            return Err(UserError::NotFound);
        }
        Ok(())
    }

    /// Get an OAuth user_auth row by the provider's stable user id.
    ///
    /// `auth_data` stores `{"provider_user_id": ...}` for every OAuth row, so
    /// this is the lookup that survives a user changing their email at the
    /// provider (the `auth_identifier` email is just what it was at link time).
    pub async fn get_by_provider_user_id(
        db: &crate::db::DatabasePool,
        auth_type: &str,
        provider_user_id: &str,
    ) -> Result<UserAuth, UserError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let user_auth = sqlx::query_as::<_, UserAuth>(
                    "SELECT user_auth_id, user_id, auth_type, auth_identifier, auth_data, active, created_at, created_by_user_id, updated_at, updated_by_user_id, active_toggle_at, active_toggle_by_user_id FROM user_auth WHERE auth_type = $1 AND auth_data->>'provider_user_id' = $2",
                )
                .bind(auth_type)
                .bind(provider_user_id)
                .fetch_one(pg_pool)
                .await
                .map_err(|e| match e {
                    sqlx::Error::RowNotFound => UserError::NotFound,
                    other => UserError::Database(other),
                })?;
                Ok(user_auth)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let user_auth = sqlx::query_as::<_, UserAuth>(
                    "SELECT user_auth_id, user_id, auth_type, auth_identifier, auth_data, active, created_at, created_by_user_id, updated_at, updated_by_user_id, active_toggle_at, active_toggle_by_user_id FROM user_auth WHERE auth_type = ? AND json_extract(auth_data, '$.provider_user_id') = ?",
                )
                .bind(auth_type)
                .bind(provider_user_id)
                .fetch_one(sqlite_pool)
                .await
                .map_err(|e| match e {
                    sqlx::Error::RowNotFound => UserError::NotFound,
                    other => UserError::Database(other),
                })?;
                Ok(user_auth)
            }
        }
    }

    /// Get user auths by user ID
    pub async fn get_user_auths_by_user(
        db: &crate::db::DatabasePool,
        user_id: &Uuid,
    ) -> Result<Vec<UserAuth>, UserError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let user_auths = sqlx::query_as::<_, UserAuth>(
                    "SELECT user_auth_id, user_id, auth_type, auth_identifier, auth_data, active, created_at, created_by_user_id, updated_at, updated_by_user_id, active_toggle_at, active_toggle_by_user_id FROM user_auth WHERE user_id = $1",
                )
                .bind(user_id)
                .fetch_all(pg_pool)
                .await?;
                Ok(user_auths)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let user_auths = sqlx::query_as::<_, UserAuth>(
                    "SELECT user_auth_id, user_id, auth_type, auth_identifier, auth_data, active, created_at, created_by_user_id, updated_at, updated_by_user_id, active_toggle_at, active_toggle_by_user_id FROM user_auth WHERE user_id = ?",
                )
                .bind(user_id)
                .fetch_all(sqlite_pool)
                .await?;

                Ok(user_auths)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A child org's alert directory reaches the administrators it inherits
    /// from its root and its active members, not a member whose grant was
    /// revoked.
    #[tokio::test]
    async fn org_directory_includes_inherited_admins_and_excludes_revoked_members() {
        let db = crate::db::test_db().await;
        let crate::db::DatabasePool::Sqlite(pool) = &db else {
            return;
        };
        let [admin, member, revoked] = [Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7()];
        for id in [admin, member, revoked] {
            User::insert_user(&db, &id, &format!("{id}@example.test"), None, Some(&id))
                .await
                .unwrap();
        }
        let root = Uuid::now_v7();
        let child = Uuid::now_v7();
        crate::db::Org::insert_org(
            &db,
            &root,
            "Root",
            &root.to_string(),
            "organization",
            &admin,
        )
        .await
        .unwrap();
        sqlx::query("INSERT INTO org (org_id,parent_org_id,name,slug,org_type,created_by_user_id) VALUES (?,?,'Child',?,'organization',?)")
            .bind(child).bind(root).bind(child.to_string()).bind(admin).execute(pool).await.unwrap();
        // A child grant requires an active root membership.
        for (org, user, role, active) in [
            (root, admin, 2_i16, true),
            (root, member, 1, true),
            (root, revoked, 1, true),
            (child, member, 1, true),
            (child, revoked, 1, false),
        ] {
            sqlx::query("INSERT INTO org_user (org_user_id,org_id,user_id,org_user_role_id,active,created_by_user_id) VALUES (?,?,?,?,?,?)")
                .bind(Uuid::now_v7()).bind(org).bind(user).bind(role).bind(active).bind(admin)
                .execute(pool).await.unwrap();
        }
        let directory: Vec<Uuid> = User::get_users_by_org(&db, &child)
            .await
            .unwrap()
            .into_iter()
            .map(|user| user.user_id)
            .collect();
        assert!(directory.contains(&admin), "inherited root administrator");
        assert!(directory.contains(&member));
        assert!(!directory.contains(&revoked), "revoked child grant");
    }
}

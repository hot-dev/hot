use chrono::{DateTime, Utc};
use sqlx::{FromRow, Pool, Postgres, Sqlite};
use thiserror::Error;
use uuid::Uuid;

use crate::context_encryption::{ContextEncryption, EncryptionError};
use crate::val::Val;

#[derive(Error, Debug)]
pub enum ContextError {
    #[error("Database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("Context variable not found")]
    NotFound,
    #[error("Context variable already exists")]
    AlreadyExists,
    #[error("Encryption error: {0}")]
    Encryption(#[from] EncryptionError),
    #[error("Serialization error: {0}")]
    Serialization(String),
    #[error("Validation error: {0}")]
    Validation(String),
}

#[derive(Debug, Clone, FromRow)]
pub struct Context {
    pub context_id: Uuid,
    pub env_id: Option<Uuid>,
    pub project_id: Option<Uuid>,
    pub key: String,
    pub value: String, // Always encrypted
    pub description: Option<String>,
    pub active: bool,
    pub created_at: DateTime<Utc>,
    pub created_by_user_id: Uuid,
    pub updated_at: DateTime<Utc>,
    pub updated_by_user_id: Option<Uuid>,
}

impl Context {
    /// Get a context variable by ID
    pub async fn get_by_id(
        db: &crate::db::DatabasePool,
        id: &Uuid,
    ) -> Result<Context, ContextError> {
        match db {
            crate::db::DatabasePool::Sqlite(db) => Self::get_by_id_sqlite(db, id).await,
            crate::db::DatabasePool::Postgres(db) => Self::get_by_id_postgres(db, id).await,
        }
    }

    async fn get_by_id_sqlite(db: &Pool<Sqlite>, id: &Uuid) -> Result<Context, ContextError> {
        let row = sqlx::query_as::<_, Context>(
            r#"
            SELECT context_id, env_id, project_id, key, value, description, active,
                   created_at, created_by_user_id, updated_at, updated_by_user_id
            FROM context
            WHERE context_id = ? AND active = 1
            "#,
        )
        .bind(id)
        .fetch_one(db)
        .await?;

        Ok(row)
    }

    async fn get_by_id_postgres(db: &Pool<Postgres>, id: &Uuid) -> Result<Context, ContextError> {
        let row = sqlx::query_as::<_, Context>(
            r#"
            SELECT context_id, env_id, project_id, key, value, description, active,
                   created_at, created_by_user_id, updated_at, updated_by_user_id
            FROM context
            WHERE context_id = $1 AND active = true
            "#,
        )
        .bind(id)
        .fetch_one(db)
        .await?;

        Ok(row)
    }

    /// Get all context variables for an environment (env-level only)
    pub async fn get_by_env(
        db: &crate::db::DatabasePool,
        env_id: &Uuid,
    ) -> Result<Vec<Context>, ContextError> {
        match db {
            crate::db::DatabasePool::Sqlite(db) => Self::get_by_env_sqlite(db, env_id).await,
            crate::db::DatabasePool::Postgres(db) => Self::get_by_env_postgres(db, env_id).await,
        }
    }

    async fn get_by_env_sqlite(
        db: &Pool<Sqlite>,
        env_id: &Uuid,
    ) -> Result<Vec<Context>, ContextError> {
        let rows = sqlx::query_as::<_, Context>(
            r#"
            SELECT context_id, env_id, project_id, key, value, description, active,
                   created_at, created_by_user_id, updated_at, updated_by_user_id
            FROM context
            WHERE env_id = ? AND project_id IS NULL AND active = 1
            ORDER BY key
            "#,
        )
        .bind(env_id)
        .fetch_all(db)
        .await?;

        Ok(rows)
    }

    async fn get_by_env_postgres(
        db: &Pool<Postgres>,
        env_id: &Uuid,
    ) -> Result<Vec<Context>, ContextError> {
        let rows = sqlx::query_as::<_, Context>(
            r#"
            SELECT context_id, env_id, project_id, key, value, description, active,
                   created_at, created_by_user_id, updated_at, updated_by_user_id
            FROM context
            WHERE env_id = $1 AND project_id IS NULL AND active = true
            ORDER BY key
            "#,
        )
        .bind(env_id)
        .fetch_all(db)
        .await?;

        Ok(rows)
    }

    /// Get all context variables for a project (project-level only)
    pub async fn get_by_project(
        db: &crate::db::DatabasePool,
        project_id: &Uuid,
    ) -> Result<Vec<Context>, ContextError> {
        match db {
            crate::db::DatabasePool::Sqlite(db) => {
                Self::get_by_project_sqlite(db, project_id).await
            }
            crate::db::DatabasePool::Postgres(db) => {
                Self::get_by_project_postgres(db, project_id).await
            }
        }
    }

    async fn get_by_project_sqlite(
        db: &Pool<Sqlite>,
        project_id: &Uuid,
    ) -> Result<Vec<Context>, ContextError> {
        let rows = sqlx::query_as::<_, Context>(
            r#"
            SELECT context_id, env_id, project_id, key, value, description, active,
                   created_at, created_by_user_id, updated_at, updated_by_user_id
            FROM context
            WHERE project_id = ? AND env_id IS NULL AND active = 1
            ORDER BY key
            "#,
        )
        .bind(project_id)
        .fetch_all(db)
        .await?;

        Ok(rows)
    }

    async fn get_by_project_postgres(
        db: &Pool<Postgres>,
        project_id: &Uuid,
    ) -> Result<Vec<Context>, ContextError> {
        let rows = sqlx::query_as::<_, Context>(
            r#"
            SELECT context_id, env_id, project_id, key, value, description, active,
                   created_at, created_by_user_id, updated_at, updated_by_user_id
            FROM context
            WHERE project_id = $1 AND env_id IS NULL AND active = true
            ORDER BY key
            "#,
        )
        .bind(project_id)
        .fetch_all(db)
        .await?;

        Ok(rows)
    }

    /// Get context variables for a project with pagination
    /// Returns (contexts, total_count)
    pub async fn get_by_project_paginated(
        db: &crate::db::DatabasePool,
        project_id: &Uuid,
        limit: Option<i64>,
        offset: Option<i64>,
    ) -> Result<(Vec<Context>, i64), ContextError> {
        match db {
            crate::db::DatabasePool::Sqlite(db) => {
                Self::get_by_project_paginated_sqlite(db, project_id, limit, offset).await
            }
            crate::db::DatabasePool::Postgres(db) => {
                Self::get_by_project_paginated_postgres(db, project_id, limit, offset).await
            }
        }
    }

    async fn get_by_project_paginated_sqlite(
        db: &Pool<Sqlite>,
        project_id: &Uuid,
        limit: Option<i64>,
        offset: Option<i64>,
    ) -> Result<(Vec<Context>, i64), ContextError> {
        // Get total count
        let count: (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM context WHERE project_id = ? AND env_id IS NULL AND active = 1",
        )
        .bind(project_id)
        .fetch_one(db)
        .await?;

        // Get paginated results
        let rows = sqlx::query_as::<_, Context>(
            r#"
            SELECT context_id, env_id, project_id, key, value, description, active,
                   created_at, created_by_user_id, updated_at, updated_by_user_id
            FROM context
            WHERE project_id = ? AND env_id IS NULL AND active = 1
            ORDER BY key
            LIMIT ? OFFSET ?
            "#,
        )
        .bind(project_id)
        .bind(limit.unwrap_or(20))
        .bind(offset.unwrap_or(0))
        .fetch_all(db)
        .await?;

        Ok((rows, count.0))
    }

    async fn get_by_project_paginated_postgres(
        db: &Pool<Postgres>,
        project_id: &Uuid,
        limit: Option<i64>,
        offset: Option<i64>,
    ) -> Result<(Vec<Context>, i64), ContextError> {
        // Get total count
        let count: (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM context WHERE project_id = $1 AND env_id IS NULL AND active = true",
        )
        .bind(project_id)
        .fetch_one(db)
        .await?;

        // Get paginated results
        let rows = sqlx::query_as::<_, Context>(
            r#"
            SELECT context_id, env_id, project_id, key, value, description, active,
                   created_at, created_by_user_id, updated_at, updated_by_user_id
            FROM context
            WHERE project_id = $1 AND env_id IS NULL AND active = true
            ORDER BY key
            LIMIT $2 OFFSET $3
            "#,
        )
        .bind(project_id)
        .bind(limit.unwrap_or(20))
        .bind(offset.unwrap_or(0))
        .fetch_all(db)
        .await?;

        Ok((rows, count.0))
    }

    /// Get a specific context variable by env and key (env-level)
    pub async fn get_by_env_and_key(
        db: &crate::db::DatabasePool,
        env_id: &Uuid,
        key: &str,
    ) -> Result<Option<Context>, ContextError> {
        match db {
            crate::db::DatabasePool::Sqlite(db) => {
                Self::get_by_env_and_key_sqlite(db, env_id, key).await
            }
            crate::db::DatabasePool::Postgres(db) => {
                Self::get_by_env_and_key_postgres(db, env_id, key).await
            }
        }
    }

    async fn get_by_env_and_key_sqlite(
        db: &Pool<Sqlite>,
        env_id: &Uuid,
        key: &str,
    ) -> Result<Option<Context>, ContextError> {
        let row = sqlx::query_as::<_, Context>(
            r#"
            SELECT context_id, env_id, project_id, key, value, description, active,
                   created_at, created_by_user_id, updated_at, updated_by_user_id
            FROM context
            WHERE env_id = ? AND project_id IS NULL AND key = ? AND active = 1
            "#,
        )
        .bind(env_id)
        .bind(key)
        .fetch_optional(db)
        .await?;

        Ok(row)
    }

    async fn get_by_env_and_key_postgres(
        db: &Pool<Postgres>,
        env_id: &Uuid,
        key: &str,
    ) -> Result<Option<Context>, ContextError> {
        let row = sqlx::query_as::<_, Context>(
            r#"
            SELECT context_id, env_id, project_id, key, value, description, active,
                   created_at, created_by_user_id, updated_at, updated_by_user_id
            FROM context
            WHERE env_id = $1 AND project_id IS NULL AND key = $2 AND active = true
            "#,
        )
        .bind(env_id)
        .bind(key)
        .fetch_optional(db)
        .await?;

        Ok(row)
    }

    /// Get a specific context variable by project and key (project-level)
    pub async fn get_by_project_and_key(
        db: &crate::db::DatabasePool,
        project_id: &Uuid,
        key: &str,
    ) -> Result<Option<Context>, ContextError> {
        match db {
            crate::db::DatabasePool::Sqlite(db) => {
                Self::get_by_project_and_key_sqlite(db, project_id, key).await
            }
            crate::db::DatabasePool::Postgres(db) => {
                Self::get_by_project_and_key_postgres(db, project_id, key).await
            }
        }
    }

    async fn get_by_project_and_key_sqlite(
        db: &Pool<Sqlite>,
        project_id: &Uuid,
        key: &str,
    ) -> Result<Option<Context>, ContextError> {
        let row = sqlx::query_as::<_, Context>(
            r#"
            SELECT context_id, env_id, project_id, key, value, description, active,
                   created_at, created_by_user_id, updated_at, updated_by_user_id
            FROM context
            WHERE project_id = ? AND env_id IS NULL AND key = ? AND active = 1
            "#,
        )
        .bind(project_id)
        .bind(key)
        .fetch_optional(db)
        .await?;

        Ok(row)
    }

    async fn get_by_project_and_key_postgres(
        db: &Pool<Postgres>,
        project_id: &Uuid,
        key: &str,
    ) -> Result<Option<Context>, ContextError> {
        let row = sqlx::query_as::<_, Context>(
            r#"
            SELECT context_id, env_id, project_id, key, value, description, active,
                   created_at, created_by_user_id, updated_at, updated_by_user_id
            FROM context
            WHERE project_id = $1 AND env_id IS NULL AND key = $2 AND active = true
            "#,
        )
        .bind(project_id)
        .bind(key)
        .fetch_optional(db)
        .await?;

        Ok(row)
    }

    /// Insert a new environment-level context variable
    pub async fn insert_env(
        db: &crate::db::DatabasePool,
        context_id: &Uuid,
        env_id: &Uuid,
        key: &str,
        value: &str,
        description: Option<&str>,
        created_by_user_id: &Uuid,
    ) -> Result<Context, ContextError> {
        match db {
            crate::db::DatabasePool::Sqlite(db) => {
                Self::insert_env_sqlite(
                    db,
                    context_id,
                    env_id,
                    key,
                    value,
                    description,
                    created_by_user_id,
                )
                .await
            }
            crate::db::DatabasePool::Postgres(db) => {
                Self::insert_env_postgres(
                    db,
                    context_id,
                    env_id,
                    key,
                    value,
                    description,
                    created_by_user_id,
                )
                .await
            }
        }
    }

    async fn insert_env_sqlite(
        db: &Pool<Sqlite>,
        context_id: &Uuid,
        env_id: &Uuid,
        key: &str,
        value: &str,
        description: Option<&str>,
        created_by_user_id: &Uuid,
    ) -> Result<Context, ContextError> {
        sqlx::query(
            r#"
            INSERT INTO context (
                context_id, env_id, project_id, key, value, description,
                created_by_user_id, updated_by_user_id
            )
            VALUES (?, ?, NULL, ?, ?, ?, ?, ?)
            "#,
        )
        .bind(context_id)
        .bind(env_id)
        .bind(key)
        .bind(value)
        .bind(description)
        .bind(created_by_user_id)
        .bind(created_by_user_id)
        .execute(db)
        .await?;

        Self::get_by_id_sqlite(db, context_id).await
    }

    async fn insert_env_postgres(
        db: &Pool<Postgres>,
        context_id: &Uuid,
        env_id: &Uuid,
        key: &str,
        value: &str,
        description: Option<&str>,
        created_by_user_id: &Uuid,
    ) -> Result<Context, ContextError> {
        sqlx::query(
            r#"
            INSERT INTO context (
                context_id, env_id, project_id, key, value, description,
                created_by_user_id, updated_by_user_id
            )
            VALUES ($1, $2, NULL, $3, $4, $5, $6, $7)
            "#,
        )
        .bind(context_id)
        .bind(env_id)
        .bind(key)
        .bind(value)
        .bind(description)
        .bind(created_by_user_id)
        .bind(created_by_user_id)
        .execute(db)
        .await?;

        Self::get_by_id_postgres(db, context_id).await
    }

    /// Insert a new project-level context variable
    pub async fn insert_project(
        db: &crate::db::DatabasePool,
        context_id: &Uuid,
        project_id: &Uuid,
        key: &str,
        value: &str,
        description: Option<&str>,
        created_by_user_id: &Uuid,
    ) -> Result<Context, ContextError> {
        match db {
            crate::db::DatabasePool::Sqlite(db) => {
                Self::insert_project_sqlite(
                    db,
                    context_id,
                    project_id,
                    key,
                    value,
                    description,
                    created_by_user_id,
                )
                .await
            }
            crate::db::DatabasePool::Postgres(db) => {
                Self::insert_project_postgres(
                    db,
                    context_id,
                    project_id,
                    key,
                    value,
                    description,
                    created_by_user_id,
                )
                .await
            }
        }
    }

    async fn insert_project_sqlite(
        db: &Pool<Sqlite>,
        context_id: &Uuid,
        project_id: &Uuid,
        key: &str,
        value: &str,
        description: Option<&str>,
        created_by_user_id: &Uuid,
    ) -> Result<Context, ContextError> {
        sqlx::query(
            r#"
            INSERT INTO context (
                context_id, env_id, project_id, key, value, description,
                created_by_user_id, updated_by_user_id
            )
            VALUES (?, NULL, ?, ?, ?, ?, ?, ?)
            "#,
        )
        .bind(context_id)
        .bind(project_id)
        .bind(key)
        .bind(value)
        .bind(description)
        .bind(created_by_user_id)
        .bind(created_by_user_id)
        .execute(db)
        .await?;

        Self::get_by_id_sqlite(db, context_id).await
    }

    async fn insert_project_postgres(
        db: &Pool<Postgres>,
        context_id: &Uuid,
        project_id: &Uuid,
        key: &str,
        value: &str,
        description: Option<&str>,
        created_by_user_id: &Uuid,
    ) -> Result<Context, ContextError> {
        sqlx::query(
            r#"
            INSERT INTO context (
                context_id, env_id, project_id, key, value, description,
                created_by_user_id, updated_by_user_id
            )
            VALUES ($1, NULL, $2, $3, $4, $5, $6, $7)
            "#,
        )
        .bind(context_id)
        .bind(project_id)
        .bind(key)
        .bind(value)
        .bind(description)
        .bind(created_by_user_id)
        .bind(created_by_user_id)
        .execute(db)
        .await?;

        Self::get_by_id_postgres(db, context_id).await
    }

    /// Legacy insert method for project-level context variables (for backwards compatibility)
    #[deprecated(note = "Use insert_project instead")]
    pub async fn insert(
        db: &crate::db::DatabasePool,
        context_id: &Uuid,
        project_id: &Uuid,
        key: &str,
        value: &str,
        description: Option<&str>,
        created_by_user_id: &Uuid,
    ) -> Result<Context, ContextError> {
        Self::insert_project(
            db,
            context_id,
            project_id,
            key,
            value,
            description,
            created_by_user_id,
        )
        .await
    }

    /// Update an existing context variable
    pub async fn update(
        db: &crate::db::DatabasePool,
        context_id: &Uuid,
        value: &str,
        description: Option<&str>,
        updated_by_user_id: &Uuid,
    ) -> Result<Context, ContextError> {
        match db {
            crate::db::DatabasePool::Sqlite(db) => {
                Self::update_sqlite(db, context_id, value, description, updated_by_user_id).await
            }
            crate::db::DatabasePool::Postgres(db) => {
                Self::update_postgres(db, context_id, value, description, updated_by_user_id).await
            }
        }
    }

    async fn update_sqlite(
        db: &Pool<Sqlite>,
        context_id: &Uuid,
        value: &str,
        description: Option<&str>,
        updated_by_user_id: &Uuid,
    ) -> Result<Context, ContextError> {
        sqlx::query(
            r#"
            UPDATE context
            SET value = ?, description = ?, updated_at = CURRENT_TIMESTAMP, updated_by_user_id = ?
            WHERE context_id = ?
            "#,
        )
        .bind(value)
        .bind(description)
        .bind(updated_by_user_id)
        .bind(context_id)
        .execute(db)
        .await?;

        Self::get_by_id_sqlite(db, context_id).await
    }

    async fn update_postgres(
        db: &Pool<Postgres>,
        context_id: &Uuid,
        value: &str,
        description: Option<&str>,
        updated_by_user_id: &Uuid,
    ) -> Result<Context, ContextError> {
        sqlx::query(
            r#"
            UPDATE context
            SET value = $1, description = $2, updated_at = NOW(), updated_by_user_id = $3
            WHERE context_id = $4
            "#,
        )
        .bind(value)
        .bind(description)
        .bind(updated_by_user_id)
        .bind(context_id)
        .execute(db)
        .await?;

        Self::get_by_id_postgres(db, context_id).await
    }

    /// Delete a context variable (soft delete)
    pub async fn delete(
        db: &crate::db::DatabasePool,
        context_id: &Uuid,
    ) -> Result<(), ContextError> {
        match db {
            crate::db::DatabasePool::Sqlite(db) => Self::delete_sqlite(db, context_id).await,
            crate::db::DatabasePool::Postgres(db) => Self::delete_postgres(db, context_id).await,
        }
    }

    async fn delete_sqlite(db: &Pool<Sqlite>, context_id: &Uuid) -> Result<(), ContextError> {
        sqlx::query(
            r#"
            UPDATE context
            SET active = 0
            WHERE context_id = ?
            "#,
        )
        .bind(context_id)
        .execute(db)
        .await?;

        Ok(())
    }

    async fn delete_postgres(db: &Pool<Postgres>, context_id: &Uuid) -> Result<(), ContextError> {
        sqlx::query(
            r#"
            UPDATE context
            SET active = false
            WHERE context_id = $1
            "#,
        )
        .bind(context_id)
        .execute(db)
        .await?;

        Ok(())
    }

    /// Decrypt and deserialize value (requires org_id for key derivation)
    pub fn get_decrypted_value(
        &self,
        encryption: &ContextEncryption,
        org_id: &Uuid,
    ) -> Result<Val, ContextError> {
        let json_str = encryption.decrypt(&self.value, org_id)?;
        serde_json::from_str(&json_str).map_err(|e| ContextError::Serialization(e.to_string()))
    }

    /// Get count of context variables for an environment (env-level only)
    pub async fn get_count_by_env(
        db: &crate::db::DatabasePool,
        env_id: &Uuid,
    ) -> Result<i64, ContextError> {
        match db {
            crate::db::DatabasePool::Sqlite(db) => Self::get_count_by_env_sqlite(db, env_id).await,
            crate::db::DatabasePool::Postgres(db) => {
                Self::get_count_by_env_postgres(db, env_id).await
            }
        }
    }

    async fn get_count_by_env_sqlite(
        db: &Pool<Sqlite>,
        env_id: &Uuid,
    ) -> Result<i64, ContextError> {
        let count: i64 = sqlx::query_scalar(
            r#"
            SELECT COUNT(*) FROM context
            WHERE env_id = ? AND project_id IS NULL AND active = 1
            "#,
        )
        .bind(env_id)
        .fetch_one(db)
        .await?;

        Ok(count)
    }

    async fn get_count_by_env_postgres(
        db: &Pool<Postgres>,
        env_id: &Uuid,
    ) -> Result<i64, ContextError> {
        let count: i64 = sqlx::query_scalar(
            r#"
            SELECT COUNT(*) FROM context
            WHERE env_id = $1 AND project_id IS NULL AND active = true
            "#,
        )
        .bind(env_id)
        .fetch_one(db)
        .await?;

        Ok(count)
    }

    /// Get count of context variables for a project (project-level only)
    pub async fn get_count_by_project(
        db: &crate::db::DatabasePool,
        project_id: &Uuid,
    ) -> Result<i64, ContextError> {
        match db {
            crate::db::DatabasePool::Sqlite(db) => {
                Self::get_count_by_project_sqlite(db, project_id).await
            }
            crate::db::DatabasePool::Postgres(db) => {
                Self::get_count_by_project_postgres(db, project_id).await
            }
        }
    }

    async fn get_count_by_project_sqlite(
        db: &Pool<Sqlite>,
        project_id: &Uuid,
    ) -> Result<i64, ContextError> {
        let count: i64 = sqlx::query_scalar(
            r#"
            SELECT COUNT(*) FROM context
            WHERE project_id = ? AND env_id IS NULL AND active = 1
            "#,
        )
        .bind(project_id)
        .fetch_one(db)
        .await?;

        Ok(count)
    }

    async fn get_count_by_project_postgres(
        db: &Pool<Postgres>,
        project_id: &Uuid,
    ) -> Result<i64, ContextError> {
        let count: i64 = sqlx::query_scalar(
            r#"
            SELECT COUNT(*) FROM context
            WHERE project_id = $1 AND env_id IS NULL AND active = true
            "#,
        )
        .bind(project_id)
        .fetch_one(db)
        .await?;

        Ok(count)
    }

    /// Serialize and encrypt value (requires org_id for key derivation)
    pub fn set_value_from_val(
        val: &Val,
        encryption: &ContextEncryption,
        org_id: &Uuid,
    ) -> Result<String, ContextError> {
        let json_str =
            serde_json::to_string(val).map_err(|e| ContextError::Serialization(e.to_string()))?;
        Ok(encryption.encrypt(&json_str, org_id)?)
    }

    /// Validate that value is valid Hot code
    pub fn validate_hot_value(value_str: &str) -> Result<Val, ContextError> {
        crate::lang::engine::Engine::eval_simple(value_str).map_err(ContextError::Validation)
    }
}

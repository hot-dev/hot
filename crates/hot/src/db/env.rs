use chrono::{DateTime, Utc};
use sqlx::FromRow;
use thiserror::Error;
use uuid::Uuid;

#[derive(Error, Debug)]
pub enum EnvError {
    #[error("Database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("Environment not found")]
    NotFound,
}

#[derive(Debug, Clone, FromRow)]
pub struct Env {
    pub env_id: Uuid,
    pub org_id: Uuid,
    pub name: String,
    pub active: bool,
    pub created_by_user_id: Uuid,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub updated_by_user_id: Option<Uuid>,
    pub active_toggle_at: Option<DateTime<Utc>>,
    pub active_toggle_by_user_id: Option<Uuid>,
}

impl Env {
    /// Get all env_ids for an organization.
    pub async fn get_ids_for_org(
        db: &crate::db::DatabasePool,
        org_id: &Uuid,
    ) -> Result<Vec<Uuid>, EnvError> {
        match db {
            crate::db::DatabasePool::Postgres(pool) => Ok(sqlx::query_scalar(
                "SELECT env_id FROM env WHERE org_id = $1",
            )
            .bind(org_id)
            .fetch_all(pool)
            .await?),
            crate::db::DatabasePool::Sqlite(pool) => Ok(sqlx::query_scalar(
                "SELECT env_id FROM env WHERE org_id = ?",
            )
            .bind(org_id)
            .fetch_all(pool)
            .await?),
        }
    }

    /// Get environment by ID
    pub async fn get_env(db: &crate::db::DatabasePool, env_id: &Uuid) -> Result<Env, EnvError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let env = sqlx::query_as::<_, Env>(
                    "SELECT env_id, org_id, name, active, created_by_user_id, created_at, updated_at, updated_by_user_id, active_toggle_at, active_toggle_by_user_id FROM env WHERE env_id = $1",
                )
                .bind(env_id)
                .fetch_one(pg_pool)
                .await?;
                Ok(env)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let env = sqlx::query_as::<_, Env>(
                    "SELECT env_id, org_id, name, active, created_by_user_id, created_at, updated_at, updated_by_user_id, active_toggle_at, active_toggle_by_user_id FROM env WHERE env_id = ?"
                )
                .bind(env_id)
                .fetch_one(sqlite_pool)
                .await?;
                Ok(env)
            }
        }
    }

    /// Get count of environments
    pub async fn get_count(db: &crate::db::DatabasePool) -> Result<i64, EnvError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM env")
                    .fetch_one(pg_pool)
                    .await?;
                Ok(count)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM env")
                    .fetch_one(sqlite_pool)
                    .await?;
                Ok(count)
            }
        }
    }

    /// Get the default (first) environment
    pub async fn get_default_env(db: &crate::db::DatabasePool) -> Result<Env, EnvError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let env = sqlx::query_as::<_, Env>(
                    "SELECT env_id, org_id, name, active, created_by_user_id, created_at, updated_at, updated_by_user_id, active_toggle_at, active_toggle_by_user_id FROM env ORDER BY created_at LIMIT 1"
                )
                .fetch_one(pg_pool)
                .await?;
                Ok(env)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let env = sqlx::query_as::<_, Env>(
                    "SELECT env_id, org_id, name, active, created_by_user_id, created_at, updated_at, updated_by_user_id, active_toggle_at, active_toggle_by_user_id FROM env ORDER BY created_at LIMIT 1"
                )
                .fetch_one(sqlite_pool)
                .await?;
                Ok(env)
            }
        }
    }

    /// Insert a new environment
    pub async fn insert_env(
        db: &crate::db::DatabasePool,
        env_id: &Uuid,
        org_id: &Uuid,
        name: &str,
        created_by_user_id: &Uuid,
    ) -> Result<(), EnvError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                sqlx::query("INSERT INTO env (env_id, org_id, name, created_by_user_id) VALUES ($1, $2, $3, $4)")
                    .bind(env_id)
                    .bind(org_id)
                    .bind(name)
                    .bind(created_by_user_id)
                    .execute(pg_pool)
                    .await?;
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                sqlx::query(
                    "INSERT INTO env (env_id, org_id, name, created_by_user_id) VALUES (?, ?, ?, ?)",
                )
                .bind(env_id)
                .bind(org_id)
                .bind(name)
                .bind(created_by_user_id)
                .execute(sqlite_pool)
                .await?;
            }
        }
        Ok(())
    }

    /// Get environments by organization ID
    pub async fn get_envs_by_org(
        db: &crate::db::DatabasePool,
        org_id: &Uuid,
    ) -> Result<Vec<Env>, EnvError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let envs = sqlx::query_as::<_, Env>(
                    "SELECT env_id, org_id, name, active, created_by_user_id, created_at, updated_at, updated_by_user_id, active_toggle_at, active_toggle_by_user_id FROM env WHERE org_id = $1 ORDER BY created_at",
                )
                .bind(org_id)
                .fetch_all(pg_pool)
                .await?;
                Ok(envs)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let envs = sqlx::query_as::<_, Env>(
                    "SELECT env_id, org_id, name, active, created_by_user_id, created_at, updated_at, updated_by_user_id, active_toggle_at, active_toggle_by_user_id FROM env WHERE org_id = ? ORDER BY created_at"
                )
                .bind(org_id)
                .fetch_all(sqlite_pool)
                .await?;
                Ok(envs)
            }
        }
    }

    /// Get the default (first) environment for an organization
    pub async fn get_default_env_by_org(
        db: &crate::db::DatabasePool,
        org_id: &Uuid,
    ) -> Result<Env, EnvError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let env = sqlx::query_as::<_, Env>(
                    "SELECT env_id, org_id, name, active, created_by_user_id, created_at, updated_at, updated_by_user_id, active_toggle_at, active_toggle_by_user_id FROM env WHERE org_id = $1 ORDER BY created_at LIMIT 1"
                )
                .bind(org_id)
                .fetch_one(pg_pool)
                .await?;
                Ok(env)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let env = sqlx::query_as::<_, Env>(
                    "SELECT env_id, org_id, name, active, created_by_user_id, created_at, updated_at, updated_by_user_id, active_toggle_at, active_toggle_by_user_id FROM env WHERE org_id = ? ORDER BY created_at LIMIT 1"
                )
                .bind(org_id)
                .fetch_one(sqlite_pool)
                .await?;
                Ok(env)
            }
        }
    }

    /// Get environment by organization ID and name
    pub async fn get_env_by_org_and_name(
        db: &crate::db::DatabasePool,
        org_id: &Uuid,
        name: &str,
    ) -> Result<Env, EnvError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let env = sqlx::query_as::<_, Env>(
                    "SELECT env_id, org_id, name, active, created_by_user_id, created_at, updated_at, updated_by_user_id, active_toggle_at, active_toggle_by_user_id FROM env WHERE org_id = $1 AND name = $2",
                )
                .bind(org_id)
                .bind(name)
                .fetch_one(pg_pool)
                .await?;
                Ok(env)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let env = sqlx::query_as::<_, Env>(
                    r#"SELECT env_id, org_id, name, active,
                              created_by_user_id, created_at, updated_at,
                              updated_by_user_id, active_toggle_at,
                              active_toggle_by_user_id
                       FROM env WHERE org_id = ? AND name = ?"#,
                )
                .bind(org_id)
                .bind(name)
                .fetch_one(sqlite_pool)
                .await?;
                Ok(env)
            }
        }
    }

    /// Get all environments with pagination
    pub async fn get_all_envs(
        db: &crate::db::DatabasePool,
        limit: Option<i64>,
        offset: Option<i64>,
    ) -> Result<Vec<Env>, EnvError> {
        let limit = limit.unwrap_or(10);
        let offset = offset.unwrap_or(0);

        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let envs = sqlx::query_as::<_, Env>(
                    "SELECT env_id, org_id, name, active, created_by_user_id, created_at, updated_at, updated_by_user_id, active_toggle_at, active_toggle_by_user_id FROM env ORDER BY created_at DESC LIMIT $1 OFFSET $2",
                )
                .bind(limit)
                .bind(offset)
                .fetch_all(pg_pool)
                .await?;
                Ok(envs)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let envs = sqlx::query_as::<_, Env>(
                    "SELECT env_id, org_id, name, active, created_by_user_id, created_at, updated_at, updated_by_user_id, active_toggle_at, active_toggle_by_user_id FROM env ORDER BY created_at DESC LIMIT ? OFFSET ?",
                )
                .bind(limit)
                .bind(offset)
                .fetch_all(sqlite_pool)
                .await?;
                Ok(envs)
            }
        }
    }

    /// Update environment details
    pub async fn update_env(
        db: &crate::db::DatabasePool,
        env_id: &Uuid,
        name: &str,
        updated_by_user_id: &Uuid,
    ) -> Result<(), EnvError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                sqlx::query("UPDATE env SET name = $2, updated_at = NOW(), updated_by_user_id = $3 WHERE env_id = $1")
                    .bind(env_id)
                    .bind(name)
                    .bind(updated_by_user_id)
                    .execute(pg_pool)
                    .await?;
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                sqlx::query("UPDATE env SET name = ?, updated_at = CURRENT_TIMESTAMP, updated_by_user_id = ? WHERE env_id = ?")
                    .bind(name)
                    .bind(updated_by_user_id)
                    .bind(env_id)
                    .execute(sqlite_pool)
                    .await?;
            }
        }
        Ok(())
    }

    /// Toggle environment active status
    pub async fn toggle_active(
        db: &crate::db::DatabasePool,
        env_id: &Uuid,
        active: bool,
        toggled_by_user_id: &Uuid,
    ) -> Result<(), EnvError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                sqlx::query("UPDATE env SET active = $2, active_toggle_at = NOW(), active_toggle_by_user_id = $3, updated_at = NOW() WHERE env_id = $1")
                    .bind(env_id)
                    .bind(active)
                    .bind(toggled_by_user_id)
                    .execute(pg_pool)
                    .await?;
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let active_int = if active { 1 } else { 0 };
                sqlx::query("UPDATE env SET active = ?, active_toggle_at = CURRENT_TIMESTAMP, active_toggle_by_user_id = ?, updated_at = CURRENT_TIMESTAMP WHERE env_id = ?")
                    .bind(active_int)
                    .bind(toggled_by_user_id)
                    .bind(env_id)
                    .execute(sqlite_pool)
                    .await?;
            }
        }
        Ok(())
    }

    /// Delete environment
    pub async fn delete_env(db: &crate::db::DatabasePool, env_id: &Uuid) -> Result<(), EnvError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                sqlx::query("DELETE FROM env WHERE env_id = $1")
                    .bind(env_id)
                    .execute(pg_pool)
                    .await?;
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                sqlx::query("DELETE FROM env WHERE env_id = ?")
                    .bind(env_id)
                    .execute(sqlite_pool)
                    .await?;
            }
        }
        Ok(())
    }
}

use chrono::{DateTime, Utc};
use sqlx::FromRow;
use std::sync::LazyLock;
use thiserror::Error;
use uuid::Uuid;

use super::entity_cache::EntityCache;

static BUILD_CACHE: LazyLock<EntityCache<Uuid, Build>> = LazyLock::new(|| EntityCache::new(2_000));

#[derive(Error, Debug)]
pub enum BuildError {
    #[error("Database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("Build not found")]
    NotFound,
}

#[derive(Debug, Clone, FromRow)]
pub struct Build {
    pub build_id: Uuid,
    pub project_id: Uuid,
    pub hash: String,
    pub size: i32,
    pub build_type_id: i16,
    pub build_type: String,
    pub deployed: bool,
    pub active: bool,
    pub created_by_user_id: Uuid,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub updated_by_user_id: Option<Uuid>,
    pub active_toggle_at: Option<DateTime<Utc>>,
    pub active_toggle_by_user_id: Option<Uuid>,
    pub storage_path: Option<String>,
    pub storage_backend: Option<String>,
}

impl Build {
    /// Get build by ID
    pub async fn get_build(
        db: &crate::db::DatabasePool,
        build_id: &Uuid,
    ) -> Result<Build, BuildError> {
        if let Some(build) = BUILD_CACHE.get(build_id) {
            return Ok(build);
        }

        let build = match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                sqlx::query_as::<_, Build>(
                    "SELECT b.build_id, b.project_id, b.hash, b.size, b.build_type_id, bt.build_type, b.deployed, b.active, b.created_by_user_id, b.created_at, b.updated_at, b.updated_by_user_id, b.active_toggle_at, b.active_toggle_by_user_id, b.storage_path, b.storage_backend FROM build b JOIN build_type bt ON b.build_type_id = bt.build_type_id WHERE b.build_id = $1"
                )
                .bind(build_id)
                .fetch_one(pg_pool)
                .await
                .map_err(|e| match e {
                    sqlx::Error::RowNotFound => BuildError::NotFound,
                    _ => {
                        tracing::error!("db error in Build::get_build: {}", e);
                        BuildError::Database(e)
                    }
                })
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                sqlx::query_as::<_, Build>(
                    "SELECT b.build_id, b.project_id, b.hash, b.size, b.build_type_id, bt.build_type, b.deployed, b.active, b.created_by_user_id, b.created_at, b.updated_at, b.updated_by_user_id, b.active_toggle_at, b.active_toggle_by_user_id, b.storage_path, b.storage_backend FROM build b JOIN build_type bt ON b.build_type_id = bt.build_type_id WHERE b.build_id = ?"
                )
                .bind(build_id)
                .fetch_one(sqlite_pool)
                .await
                .map_err(|e| match e {
                    sqlx::Error::RowNotFound => BuildError::NotFound,
                    _ => {
                        tracing::error!("db error in Build::get_build: {}", e);
                        BuildError::Database(e)
                    }
                })
            }
        }?;

        BUILD_CACHE.insert(*build_id, build.clone());
        Ok(build)
    }

    pub fn invalidate_build_cache(build_id: &Uuid) {
        BUILD_CACHE.invalidate(build_id);
    }

    pub fn invalidate_all_build_cache() {
        BUILD_CACHE.clear();
    }

    /// Get builds by project ID
    pub async fn get_builds_by_project(
        db: &crate::db::DatabasePool,
        project_id: &Uuid,
        limit: Option<i64>,
        offset: Option<i64>,
    ) -> Result<Vec<Build>, BuildError> {
        let limit = limit.unwrap_or(10);
        let offset = offset.unwrap_or(0);

        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let builds = sqlx::query_as::<_, Build>(
                    "SELECT b.build_id, b.project_id, b.hash, b.size, b.build_type_id, bt.build_type, b.deployed, b.active, b.created_by_user_id, b.created_at, b.updated_at, b.updated_by_user_id, b.active_toggle_at, b.active_toggle_by_user_id, b.storage_path, b.storage_backend FROM build b JOIN build_type bt ON b.build_type_id = bt.build_type_id WHERE b.project_id = $1 ORDER BY b.deployed DESC, b.created_at DESC LIMIT $2 OFFSET $3"
                )
                .bind(project_id)
                .bind(limit)
                .bind(offset)
                .fetch_all(pg_pool)
                .await?;
                Ok(builds)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let builds = sqlx::query_as::<_, Build>(
                    "SELECT b.build_id, b.project_id, b.hash, b.size, b.build_type_id, bt.build_type, b.deployed, b.active, b.created_by_user_id, b.created_at, b.updated_at, b.updated_by_user_id, b.active_toggle_at, b.active_toggle_by_user_id, b.storage_path, b.storage_backend FROM build b JOIN build_type bt ON b.build_type_id = bt.build_type_id WHERE b.project_id = ? ORDER BY b.deployed DESC, b.created_at DESC LIMIT ? OFFSET ?"
                )
                .bind(project_id)
                .bind(limit)
                .bind(offset)
                .fetch_all(sqlite_pool)
                .await?;
                Ok(builds)
            }
        }
    }

    /// Get builds by environment ID (across all projects in that environment)
    pub async fn get_builds_by_env(
        db: &crate::db::DatabasePool,
        env_id: &Uuid,
        limit: Option<i64>,
        offset: Option<i64>,
    ) -> Result<Vec<Build>, BuildError> {
        let limit = limit.unwrap_or(10);
        let offset = offset.unwrap_or(0);

        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let builds = sqlx::query_as::<_, Build>(
                    "SELECT b.build_id, b.project_id, b.hash, b.size, b.build_type_id, bt.build_type, b.deployed, b.active, b.created_by_user_id, b.created_at, b.updated_at, b.updated_by_user_id, b.active_toggle_at, b.active_toggle_by_user_id, b.storage_path, b.storage_backend
                     FROM build b
                     JOIN build_type bt ON b.build_type_id = bt.build_type_id
                     JOIN project p ON b.project_id = p.project_id
                     WHERE p.env_id = $1
                     ORDER BY b.deployed DESC, b.created_at DESC
                     LIMIT $2 OFFSET $3"
                )
                .bind(env_id)
                .bind(limit)
                .bind(offset)
                .fetch_all(pg_pool)
                .await?;
                Ok(builds)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let builds = sqlx::query_as::<_, Build>(
                    "SELECT b.build_id, b.project_id, b.hash, b.size, b.build_type_id, bt.build_type, b.deployed, b.active, b.created_by_user_id, b.created_at, b.updated_at, b.updated_by_user_id, b.active_toggle_at, b.active_toggle_by_user_id, b.storage_path, b.storage_backend
                     FROM build b
                     JOIN build_type bt ON b.build_type_id = bt.build_type_id
                     JOIN project p ON b.project_id = p.project_id
                     WHERE p.env_id = ?
                     ORDER BY b.deployed DESC, b.created_at DESC
                     LIMIT ? OFFSET ?"
                )
                .bind(env_id)
                .bind(limit)
                .bind(offset)
                .fetch_all(sqlite_pool)
                .await?;
                Ok(builds)
            }
        }
    }

    /// Get count of builds by environment ID
    pub async fn get_count_by_env(
        db: &crate::db::DatabasePool,
        env_id: &Uuid,
    ) -> Result<i64, BuildError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let count: i64 = sqlx::query_scalar(
                    "SELECT COUNT(*) FROM build b
                     JOIN project p ON b.project_id = p.project_id
                     WHERE p.env_id = $1",
                )
                .bind(env_id)
                .fetch_one(pg_pool)
                .await?;
                Ok(count)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let count: i64 = sqlx::query_scalar(
                    "SELECT COUNT(*) FROM build b
                     JOIN project p ON b.project_id = p.project_id
                     WHERE p.env_id = ?",
                )
                .bind(env_id)
                .fetch_one(sqlite_pool)
                .await?;
                Ok(count)
            }
        }
    }

    /// Get count of builds
    pub async fn get_count(db: &crate::db::DatabasePool) -> Result<i64, BuildError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM build")
                    .fetch_one(pg_pool)
                    .await?;
                Ok(count)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM build")
                    .fetch_one(sqlite_pool)
                    .await?;
                Ok(count)
            }
        }
    }

    /// Get count of builds by project ID
    pub async fn get_count_by_project(
        db: &crate::db::DatabasePool,
        project_id: &Uuid,
    ) -> Result<i64, BuildError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let count: i64 =
                    sqlx::query_scalar("SELECT COUNT(*) FROM build WHERE project_id = $1")
                        .bind(project_id)
                        .fetch_one(pg_pool)
                        .await?;
                Ok(count)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let count: i64 =
                    sqlx::query_scalar("SELECT COUNT(*) FROM build WHERE project_id = ?")
                        .bind(project_id)
                        .fetch_one(sqlite_pool)
                        .await?;
                Ok(count)
            }
        }
    }

    /// Insert a new build with storage information
    #[allow(clippy::too_many_arguments)]
    pub async fn insert_build_with_storage(
        db: &crate::db::DatabasePool,
        build_id: &Uuid,
        project_id: &Uuid,
        hash: &str,
        size: i32,
        build_type_id: i16,
        created_by_user_id: &Uuid,
        storage_path: Option<&str>,
        storage_backend: Option<&str>,
    ) -> Result<(), BuildError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                sqlx::query("INSERT INTO build (build_id, project_id, hash, size, build_type_id, created_by_user_id, storage_path, storage_backend) VALUES ($1, $2, $3, $4, $5, $6, $7, $8)")
                    .bind(build_id)
                    .bind(project_id)
                    .bind(hash)
                    .bind(size)
                    .bind(build_type_id)
                    .bind(created_by_user_id)
                    .bind(storage_path)
                    .bind(storage_backend)
                    .execute(pg_pool)
                    .await?;
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                sqlx::query("INSERT INTO build (build_id, project_id, hash, size, build_type_id, created_by_user_id, storage_path, storage_backend) VALUES (?, ?, ?, ?, ?, ?, ?, ?)")
                    .bind(build_id)
                    .bind(project_id)
                    .bind(hash)
                    .bind(size)
                    .bind(build_type_id)
                    .bind(created_by_user_id)
                    .bind(storage_path)
                    .bind(storage_backend)
                    .execute(sqlite_pool)
                    .await?;
            }
        }
        Self::invalidate_build_cache(build_id);
        Ok(())
    }

    /// Insert a new build (backward compatible - calls insert_build_with_storage with None for storage fields)
    pub async fn insert_build(
        db: &crate::db::DatabasePool,
        build_id: &Uuid,
        project_id: &Uuid,
        hash: &str,
        size: i32,
        build_type_id: i16,
        created_by_user_id: &Uuid,
    ) -> Result<(), BuildError> {
        Self::insert_build_with_storage(
            db,
            build_id,
            project_id,
            hash,
            size,
            build_type_id,
            created_by_user_id,
            None,
            None,
        )
        .await
    }

    /// Toggle build active status
    pub async fn toggle_active(
        db: &crate::db::DatabasePool,
        build_id: &Uuid,
        active: bool,
        toggled_by_user_id: &Uuid,
    ) -> Result<(), BuildError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                sqlx::query("UPDATE build SET active = $2, active_toggle_at = NOW(), active_toggle_by_user_id = $3, updated_at = NOW() WHERE build_id = $1")
                    .bind(build_id)
                    .bind(active)
                    .bind(toggled_by_user_id)
                    .execute(pg_pool)
                    .await?;
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let active_int = if active { 1 } else { 0 };
                sqlx::query("UPDATE build SET active = ?, active_toggle_at = CURRENT_TIMESTAMP, active_toggle_by_user_id = ?, updated_at = CURRENT_TIMESTAMP WHERE build_id = ?")
                    .bind(active_int)
                    .bind(toggled_by_user_id)
                    .bind(build_id)
                    .execute(sqlite_pool)
                    .await?;
            }
        }
        Self::invalidate_build_cache(build_id);
        Ok(())
    }

    /// Delete build
    pub async fn delete_build(
        db: &crate::db::DatabasePool,
        build_id: &Uuid,
    ) -> Result<(), BuildError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                sqlx::query("DELETE FROM build WHERE build_id = $1")
                    .bind(build_id)
                    .execute(pg_pool)
                    .await?;
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                sqlx::query("DELETE FROM build WHERE build_id = ?")
                    .bind(build_id)
                    .execute(sqlite_pool)
                    .await?;
            }
        }
        Self::invalidate_build_cache(build_id);
        Ok(())
    }

    /// Get recent builds across all bundles
    pub async fn get_recent_builds(
        db: &crate::db::DatabasePool,
        limit: Option<i64>,
    ) -> Result<Vec<Build>, BuildError> {
        let limit = limit.unwrap_or(10);

        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let builds = sqlx::query_as::<_, Build>(
                    "SELECT b.build_id, b.project_id, b.hash, b.size, b.build_type_id, bt.build_type, b.deployed, b.active, b.created_by_user_id, b.created_at, b.updated_at, b.updated_by_user_id, b.active_toggle_at, b.active_toggle_by_user_id, b.storage_path, b.storage_backend FROM build b JOIN build_type bt ON b.build_type_id = bt.build_type_id ORDER BY b.deployed DESC, b.created_at DESC LIMIT $1"
                )
                .bind(limit)
                .fetch_all(pg_pool)
                .await?;
                Ok(builds)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let builds = sqlx::query_as::<_, Build>(
                    "SELECT b.build_id, b.project_id, b.hash, b.size, b.build_type_id, bt.build_type, b.deployed, b.active, b.created_by_user_id, b.created_at, b.updated_at, b.updated_by_user_id, b.active_toggle_at, b.active_toggle_by_user_id, b.storage_path, b.storage_backend FROM build b JOIN build_type bt ON b.build_type_id = bt.build_type_id ORDER BY b.deployed DESC, b.created_at DESC LIMIT ?"
                )
                .bind(limit)
                .fetch_all(sqlite_pool)
                .await?;
                Ok(builds)
            }
        }
    }

    /// Get builds by hash (for deduplication)
    pub async fn get_builds_by_hash(
        db: &crate::db::DatabasePool,
        hash: &str,
    ) -> Result<Vec<Build>, BuildError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let builds = sqlx::query_as::<_, Build>(
                    "SELECT b.build_id, b.project_id, b.hash, b.size, b.build_type_id, bt.build_type, b.deployed, b.active, b.created_by_user_id, b.created_at, b.updated_at, b.updated_by_user_id, b.active_toggle_at, b.active_toggle_by_user_id, b.storage_path, b.storage_backend FROM build b JOIN build_type bt ON b.build_type_id = bt.build_type_id WHERE b.hash = $1"
                )
                .bind(hash)
                .fetch_all(pg_pool)
                .await?;
                Ok(builds)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let builds = sqlx::query_as::<_, Build>(
                    "SELECT b.build_id, b.project_id, b.hash, b.size, b.build_type_id, bt.build_type, b.deployed, b.active, b.created_by_user_id, b.created_at, b.updated_at, b.updated_by_user_id, b.active_toggle_at, b.active_toggle_by_user_id, b.storage_path, b.storage_backend FROM build b JOIN build_type bt ON b.build_type_id = bt.build_type_id WHERE b.hash = ?"
                )
                .bind(hash)
                .fetch_all(sqlite_pool)
                .await?;
                Ok(builds)
            }
        }
    }

    /// Check if this build is a live (directory-based) build
    pub fn is_live(&self) -> bool {
        self.build_type_id == Self::BUILD_TYPE_LIVE
    }

    /// Check if this build is a bundle (zip file) build
    pub fn is_bundle(&self) -> bool {
        self.build_type_id == Self::BUILD_TYPE_BUNDLE
    }

    /// Build type constants
    pub const BUILD_TYPE_BUNDLE: i16 = 1;
    pub const BUILD_TYPE_LIVE: i16 = 2;

    /// Get the live build for a project (there should only be one)
    pub async fn get_live_build_by_project(
        db: &crate::db::DatabasePool,
        project_id: &Uuid,
    ) -> Result<Option<Build>, BuildError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let build = sqlx::query_as::<_, Build>(
                    "SELECT b.build_id, b.project_id, b.hash, b.size, b.build_type_id, bt.build_type, b.deployed, b.active, b.created_by_user_id, b.created_at, b.updated_at, b.updated_by_user_id, b.active_toggle_at, b.active_toggle_by_user_id, b.storage_path, b.storage_backend FROM build b JOIN build_type bt ON b.build_type_id = bt.build_type_id WHERE b.project_id = $1 AND b.build_type_id = $2"
                )
                .bind(project_id)
                .bind(Self::BUILD_TYPE_LIVE)
                .fetch_optional(pg_pool)
                .await?;
                Ok(build)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let build = sqlx::query_as::<_, Build>(
                    "SELECT b.build_id, b.project_id, b.hash, b.size, b.build_type_id, bt.build_type, b.deployed, b.active, b.created_by_user_id, b.created_at, b.updated_at, b.updated_by_user_id, b.active_toggle_at, b.active_toggle_by_user_id, b.storage_path, b.storage_backend FROM build b JOIN build_type bt ON b.build_type_id = bt.build_type_id WHERE b.project_id = ? AND b.build_type_id = ?"
                )
                .bind(project_id)
                .bind(Self::BUILD_TYPE_LIVE)
                .fetch_optional(sqlite_pool)
                .await?;
                Ok(build)
            }
        }
    }

    /// Insert or update the live build for a bundle
    /// If a live build already exists for this bundle, update it; otherwise create a new one
    pub async fn insert_or_update_live_build(
        db: &crate::db::DatabasePool,
        project_id: &Uuid,
        hash: &str,
        size: i32,
        created_by_user_id: &Uuid,
    ) -> Result<Build, BuildError> {
        // Check if live build already exists
        if let Some(existing_build) = Self::get_live_build_by_project(db, project_id).await? {
            // Update existing live build
            match db {
                crate::db::DatabasePool::Postgres(pg_pool) => {
                    sqlx::query(
                        "UPDATE build SET hash = $2, size = $3, updated_at = NOW(), updated_by_user_id = $4 WHERE build_id = $1"
                    )
                    .bind(existing_build.build_id)
                    .bind(hash)
                    .bind(size)
                    .bind(created_by_user_id)
                    .execute(pg_pool)
                    .await?;
                }
                crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                    sqlx::query(
                        "UPDATE build SET hash = ?, size = ?, updated_at = CURRENT_TIMESTAMP, updated_by_user_id = ? WHERE build_id = ?"
                    )
                    .bind(hash)
                    .bind(size)
                    .bind(created_by_user_id)
                    .bind(existing_build.build_id)
                    .execute(sqlite_pool)
                    .await?;
                }
            }
            Self::invalidate_build_cache(&existing_build.build_id);
            // Return updated build
            Self::get_build(db, &existing_build.build_id).await
        } else {
            // Create new live build
            let build_id = Uuid::now_v7();
            Self::insert_build(
                db,
                &build_id,
                project_id,
                hash,
                size,
                Self::BUILD_TYPE_LIVE,
                created_by_user_id,
            )
            .await?;
            Self::get_build(db, &build_id).await
        }
    }

    /// Deploy a build (sets it to deployed and all other builds in the same bundle to not deployed)
    pub async fn deploy_build(
        db: &crate::db::DatabasePool,
        build_id: &Uuid,
        deployed_by_user_id: &Uuid,
    ) -> Result<(), BuildError> {
        // First get the build to find its project_id
        let build = Self::get_build(db, build_id).await?;

        // Get env_id for cache invalidation
        let env_id = Self::get_env_id_for_build(db, build_id).await?;

        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                // Start a transaction
                let mut tx = pg_pool.begin().await?;

                // Set all builds in this bundle to not deployed
                sqlx::query(
                    "UPDATE build SET deployed = false, updated_at = NOW(), updated_by_user_id = $2 WHERE project_id = $1"
                )
                .bind(build.project_id)
                .bind(deployed_by_user_id)
                .execute(&mut *tx)
                .await?;

                // Set this specific build to deployed
                sqlx::query(
                    "UPDATE build SET deployed = true, updated_at = NOW(), updated_by_user_id = $2 WHERE build_id = $1"
                )
                .bind(build_id)
                .bind(deployed_by_user_id)
                .execute(&mut *tx)
                .await?;

                // Commit the transaction
                tx.commit().await?;
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                // Start a transaction
                let mut tx = sqlite_pool.begin().await?;

                // Set all builds in this bundle to not deployed
                sqlx::query(
                    "UPDATE build SET deployed = 0, updated_at = CURRENT_TIMESTAMP, updated_by_user_id = ? WHERE project_id = ?"
                )
                .bind(deployed_by_user_id)
                .bind(build.project_id)
                .execute(&mut *tx)
                .await?;

                // Set this specific build to deployed
                sqlx::query(
                    "UPDATE build SET deployed = 1, updated_at = CURRENT_TIMESTAMP, updated_by_user_id = ? WHERE build_id = ?"
                )
                .bind(deployed_by_user_id)
                .bind(build_id)
                .execute(&mut *tx)
                .await?;

                // Commit the transaction
                tx.commit().await?;
            }
        }

        // Invalidate event handler cache for this environment
        crate::db::event_handler::EventHandler::invalidate_event_handler_cache_for_env(&env_id);
        Self::invalidate_all_build_cache();

        Ok(())
    }

    /// Update storage path and backend for a build
    /// Used when uploading a build file for an existing build record
    pub async fn update_build_storage(
        db: &crate::db::DatabasePool,
        build_id: &Uuid,
        storage_path: &str,
        storage_backend: &str,
    ) -> Result<(), BuildError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                sqlx::query(
                    "UPDATE build SET storage_path = $2, storage_backend = $3, updated_at = NOW() WHERE build_id = $1",
                )
                .bind(build_id)
                .bind(storage_path)
                .bind(storage_backend)
                .execute(pg_pool)
                .await?;
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                sqlx::query(
                    "UPDATE build SET storage_path = ?, storage_backend = ?, updated_at = CURRENT_TIMESTAMP WHERE build_id = ?",
                )
                .bind(storage_path)
                .bind(storage_backend)
                .bind(build_id)
                .execute(sqlite_pool)
                .await?;
            }
        }
        Self::invalidate_build_cache(build_id);
        Ok(())
    }

    /// Get the currently deployed build for a project
    pub async fn get_deployed_build_by_project(
        db: &crate::db::DatabasePool,
        project_id: &Uuid,
    ) -> Result<Option<Build>, BuildError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let build = sqlx::query_as::<_, Build>(
                    "SELECT b.build_id, b.project_id, b.hash, b.size, b.build_type_id, bt.build_type, b.deployed, b.active, b.created_by_user_id, b.created_at, b.updated_at, b.updated_by_user_id, b.active_toggle_at, b.active_toggle_by_user_id, b.storage_path, b.storage_backend FROM build b JOIN build_type bt ON b.build_type_id = bt.build_type_id WHERE b.project_id = $1 AND b.deployed = true"
                )
                .bind(project_id)
                .fetch_optional(pg_pool)
                .await?;
                Ok(build)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let build = sqlx::query_as::<_, Build>(
                    "SELECT b.build_id, b.project_id, b.hash, b.size, b.build_type_id, bt.build_type, b.deployed, b.active, b.created_by_user_id, b.created_at, b.updated_at, b.updated_by_user_id, b.active_toggle_at, b.active_toggle_by_user_id, b.storage_path, b.storage_backend FROM build b JOIN build_type bt ON b.build_type_id = bt.build_type_id WHERE b.project_id = ? AND b.deployed = 1"
                )
                .bind(project_id)
                .fetch_optional(sqlite_pool)
                .await?;
                Ok(build)
            }
        }
    }

    /// Check if this build is currently deployed
    pub fn is_deployed(&self) -> bool {
        self.deployed
    }

    /// Get the most recently created build for a project, regardless of
    /// `deployed` / `active` flags. Used by the project reactivation path to
    /// pick the build to auto-redeploy after the user reactivates a project
    /// (which deactivation had marked all builds undeployed).
    pub async fn get_latest_build_for_project(
        db: &crate::db::DatabasePool,
        project_id: &Uuid,
    ) -> Result<Option<Build>, BuildError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let build = sqlx::query_as::<_, Build>(
                    "SELECT b.build_id, b.project_id, b.hash, b.size, b.build_type_id, bt.build_type, b.deployed, b.active, b.created_by_user_id, b.created_at, b.updated_at, b.updated_by_user_id, b.active_toggle_at, b.active_toggle_by_user_id, b.storage_path, b.storage_backend FROM build b JOIN build_type bt ON b.build_type_id = bt.build_type_id WHERE b.project_id = $1 ORDER BY b.created_at DESC LIMIT 1"
                )
                .bind(project_id)
                .fetch_optional(pg_pool)
                .await?;
                Ok(build)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let build = sqlx::query_as::<_, Build>(
                    "SELECT b.build_id, b.project_id, b.hash, b.size, b.build_type_id, bt.build_type, b.deployed, b.active, b.created_by_user_id, b.created_at, b.updated_at, b.updated_by_user_id, b.active_toggle_at, b.active_toggle_by_user_id, b.storage_path, b.storage_backend FROM build b JOIN build_type bt ON b.build_type_id = bt.build_type_id WHERE b.project_id = ? ORDER BY b.created_at DESC LIMIT 1"
                )
                .bind(project_id)
                .fetch_optional(sqlite_pool)
                .await?;
                Ok(build)
            }
        }
    }

    /// Undeploy all builds for a project
    /// Used when deactivating a project to ensure no builds are running
    pub async fn undeploy_all_builds_for_project(
        db: &crate::db::DatabasePool,
        project_id: &Uuid,
    ) -> Result<u64, BuildError> {
        let rows_affected = match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let result = sqlx::query(
                    "UPDATE build SET deployed = false, updated_at = NOW() WHERE project_id = $1 AND deployed = true"
                )
                .bind(project_id)
                .execute(pg_pool)
                .await?;
                result.rows_affected()
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let result = sqlx::query(
                    "UPDATE build SET deployed = 0, updated_at = CURRENT_TIMESTAMP WHERE project_id = ? AND deployed = 1"
                )
                .bind(project_id)
                .execute(sqlite_pool)
                .await?;
                result.rows_affected()
            }
        };

        // If any builds were undeployed, invalidate event handler cache
        // (Invalidate all since we don't have env_id here and this is a rare operation)
        if rows_affected > 0 {
            crate::db::event_handler::EventHandler::invalidate_all_event_handler_cache();
            Self::invalidate_all_build_cache();
        }

        Ok(rows_affected)
    }

    /// Get environment ID for a build (follows build -> project -> env_id chain)
    pub async fn get_env_id_for_build(
        db: &crate::db::DatabasePool,
        build_id: &Uuid,
    ) -> Result<Uuid, BuildError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => sqlx::query_scalar::<_, Uuid>(
                "SELECT p.env_id
                     FROM build b
                     JOIN project p ON b.project_id = p.project_id
                     WHERE b.build_id = $1",
            )
            .bind(build_id)
            .fetch_one(pg_pool)
            .await
            .map_err(|e| match e {
                sqlx::Error::RowNotFound => BuildError::NotFound,
                _ => BuildError::Database(e),
            }),
            crate::db::DatabasePool::Sqlite(sqlite_pool) => sqlx::query_scalar::<_, Uuid>(
                "SELECT p.env_id
                     FROM build b
                     JOIN project p ON b.project_id = p.project_id
                     WHERE b.build_id = ?",
            )
            .bind(build_id)
            .fetch_one(sqlite_pool)
            .await
            .map_err(|e| match e {
                sqlx::Error::RowNotFound => BuildError::NotFound,
                _ => BuildError::Database(e),
            }),
        }
    }

    /// Get project_id for a build (helper function for quick lookups)
    pub async fn get_project_id_for_build(
        db: &crate::db::DatabasePool,
        build_id: &Uuid,
    ) -> Result<Uuid, BuildError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                sqlx::query_scalar::<_, Uuid>("SELECT project_id FROM build WHERE build_id = $1")
                    .bind(build_id)
                    .fetch_one(pg_pool)
                    .await
                    .map_err(|e| match e {
                        sqlx::Error::RowNotFound => BuildError::NotFound,
                        _ => BuildError::Database(e),
                    })
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                sqlx::query_scalar::<_, Uuid>("SELECT project_id FROM build WHERE build_id = ?")
                    .bind(build_id)
                    .fetch_one(sqlite_pool)
                    .await
                    .map_err(|e| match e {
                        sqlx::Error::RowNotFound => BuildError::NotFound,
                        _ => BuildError::Database(e),
                    })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::create_db_pool;
    use crate::val;
    use uuid::Uuid;

    #[tokio::test]
    #[ignore]
    async fn test_build_operations() {
        // Use an in-memory SQLite database for testing
        let conf = val!({
            "db": {
                "uri": "sqlite::memory:"
            }
        });
        let db = create_db_pool(&conf).await.unwrap();

        // Run migrations
        crate::db::run_migrations(&conf).await.unwrap();

        // Create test data
        let build_id = Uuid::now_v7();
        let org_id = Uuid::now_v7();
        let user_id = Uuid::now_v7();

        // Insert test user, org, and bundle first
        crate::db::User::insert_user(&db, &user_id, "test@example.com", Some("Test User"), None)
            .await
            .unwrap();
        crate::db::Org::insert_org(
            &db,
            &org_id,
            "Test Org",
            "test-org",
            "organization",
            &user_id,
        )
        .await
        .unwrap();
        // Insert test project first
        let project_id = Uuid::now_v7();
        let env_id = Uuid::now_v7();
        crate::db::Env::insert_env(&db, &env_id, &org_id, "test-env", &user_id)
            .await
            .unwrap();
        crate::db::Project::insert_project(&db, &project_id, &env_id, "test-project", &user_id)
            .await
            .unwrap();

        // Test insert_build
        Build::insert_build(
            &db,
            &build_id,
            &project_id,
            "test-hash",
            1024,
            Build::BUILD_TYPE_BUNDLE,
            &user_id,
        )
        .await
        .unwrap();

        // Test get_build
        let build = Build::get_build(&db, &build_id).await.unwrap();
        assert_eq!(build.hash, "test-hash");
        assert_eq!(build.size, 1024);
        assert_eq!(build.project_id, project_id);

        // Test get_builds_by_project
        let builds = Build::get_builds_by_project(&db, &project_id, None, None)
            .await
            .unwrap();
        assert_eq!(builds.len(), 1);
        assert_eq!(builds[0].build_id, build_id);
    }
}

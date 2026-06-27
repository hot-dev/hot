use chrono::{DateTime, Utc};
use sqlx::{FromRow, Pool, Postgres, Sqlite};
use std::sync::LazyLock;
use thiserror::Error;
use uuid::Uuid;

use super::entity_cache::EntityCache;

static PROJECT_CACHE: LazyLock<EntityCache<Uuid, Project>> =
    LazyLock::new(|| EntityCache::new(1_000));

#[derive(Error, Debug)]
pub enum ProjectError {
    #[error("Database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("Project not found")]
    NotFound,
    #[error("Project already exists")]
    AlreadyExists,
    #[error("{0}")]
    Other(String),
}

#[derive(Debug, Clone, FromRow)]
pub struct Project {
    pub project_id: Uuid,
    pub env_id: Uuid,
    pub name: String,
    pub active: bool,
    pub created_by_user_id: Uuid,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub updated_by_user_id: Option<Uuid>,
    pub active_toggle_at: Option<DateTime<Utc>>,
    pub active_toggle_by_user_id: Option<Uuid>,
    pub deployment_sequence: i64,
}

impl Project {
    /// Get a project by its ID
    pub async fn get_project(
        db: &crate::db::DatabasePool,
        project_id: &Uuid,
    ) -> Result<Project, ProjectError> {
        if let Some(project) = PROJECT_CACHE.get(project_id) {
            return Ok(project);
        }

        let project = match db {
            crate::db::DatabasePool::Sqlite(db) => Self::get_project_sqlite(db, project_id).await,
            crate::db::DatabasePool::Postgres(db) => {
                Self::get_project_postgres(db, project_id).await
            }
        }?;

        PROJECT_CACHE.insert(*project_id, project.clone());
        Ok(project)
    }

    /// Get a project by ID without filtering inactive projects.
    ///
    /// Most runtime/read paths should use `get_project`, which only returns
    /// active projects. Write paths that can intentionally reactivate a project
    /// need to resolve the row first, even while it is inactive.
    pub async fn get_project_including_inactive(
        db: &crate::db::DatabasePool,
        project_id: &Uuid,
    ) -> Result<Project, ProjectError> {
        match db {
            crate::db::DatabasePool::Sqlite(db) => {
                Self::get_project_including_inactive_sqlite(db, project_id).await
            }
            crate::db::DatabasePool::Postgres(db) => {
                Self::get_project_including_inactive_postgres(db, project_id).await
            }
        }
    }

    pub fn invalidate_project_cache(project_id: &Uuid) {
        PROJECT_CACHE.invalidate(project_id);
    }

    pub fn invalidate_all_project_cache() {
        PROJECT_CACHE.clear();
    }

    pub async fn get_deployment_sequence(
        db: &crate::db::DatabasePool,
        project_id: &Uuid,
    ) -> Result<i64, ProjectError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let sequence = sqlx::query_scalar(
                    "SELECT deployment_sequence FROM project WHERE project_id = $1",
                )
                .bind(project_id)
                .fetch_one(pg_pool)
                .await?;
                Ok(sequence)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let sequence = sqlx::query_scalar(
                    "SELECT deployment_sequence FROM project WHERE project_id = ?",
                )
                .bind(project_id)
                .fetch_one(sqlite_pool)
                .await?;
                Ok(sequence)
            }
        }
    }

    pub async fn bump_deployment_sequence(
        db: &crate::db::DatabasePool,
        project_id: &Uuid,
        updated_by_user_id: &Uuid,
    ) -> Result<i64, ProjectError> {
        let sequence = match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                sqlx::query_scalar(
                    "UPDATE project SET deployment_sequence = deployment_sequence + 1, updated_at = NOW(), updated_by_user_id = $2 WHERE project_id = $1 RETURNING deployment_sequence"
                )
                .bind(project_id)
                .bind(updated_by_user_id)
                .fetch_one(pg_pool)
                .await?
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                sqlx::query(
                    "UPDATE project SET deployment_sequence = deployment_sequence + 1, updated_at = CURRENT_TIMESTAMP, updated_by_user_id = ? WHERE project_id = ?"
                )
                .bind(updated_by_user_id)
                .bind(project_id)
                .execute(sqlite_pool)
                .await?;

                sqlx::query_scalar("SELECT deployment_sequence FROM project WHERE project_id = ?")
                    .bind(project_id)
                    .fetch_one(sqlite_pool)
                    .await?
            }
        };
        Self::invalidate_project_cache(project_id);
        Ok(sequence)
    }

    async fn get_project_sqlite(
        db: &Pool<Sqlite>,
        project_id: &Uuid,
    ) -> Result<Project, ProjectError> {
        let row = sqlx::query_as::<_, Project>(
            r#"
            SELECT project_id, env_id, name, active, created_by_user_id, created_at, updated_at,
                   updated_by_user_id, active_toggle_at, active_toggle_by_user_id,
                   deployment_sequence
            FROM project
            WHERE project_id = ? AND active = 1
            "#,
        )
        .bind(project_id)
        .fetch_one(db)
        .await?;

        Ok(row)
    }

    async fn get_project_including_inactive_sqlite(
        db: &Pool<Sqlite>,
        project_id: &Uuid,
    ) -> Result<Project, ProjectError> {
        let row = sqlx::query_as::<_, Project>(
            r#"
            SELECT project_id, env_id, name, active, created_by_user_id, created_at, updated_at,
                   updated_by_user_id, active_toggle_at, active_toggle_by_user_id,
                   deployment_sequence
            FROM project
            WHERE project_id = ?
            "#,
        )
        .bind(project_id)
        .fetch_one(db)
        .await?;

        Ok(row)
    }

    async fn get_project_postgres(
        db: &Pool<Postgres>,
        project_id: &Uuid,
    ) -> Result<Project, ProjectError> {
        let row = sqlx::query_as::<_, Project>(
            r#"
            SELECT project_id, env_id, name, active,
                   created_by_user_id, created_at, updated_at,
                   updated_by_user_id, active_toggle_at, active_toggle_by_user_id,
                   deployment_sequence
            FROM project
            WHERE project_id = $1 AND active = true
            "#,
        )
        .bind(project_id)
        .fetch_one(db)
        .await?;

        Ok(row)
    }

    async fn get_project_including_inactive_postgres(
        db: &Pool<Postgres>,
        project_id: &Uuid,
    ) -> Result<Project, ProjectError> {
        let row = sqlx::query_as::<_, Project>(
            r#"
            SELECT project_id, env_id, name, active,
                   created_by_user_id, created_at, updated_at,
                   updated_by_user_id, active_toggle_at, active_toggle_by_user_id,
                   deployment_sequence
            FROM project
            WHERE project_id = $1
            "#,
        )
        .bind(project_id)
        .fetch_one(db)
        .await?;

        Ok(row)
    }

    /// Get a project by environment ID and name
    pub async fn get_project_by_env_and_name(
        db: &crate::db::DatabasePool,
        env_id: &Uuid,
        name: &str,
    ) -> Result<Option<Project>, ProjectError> {
        match db {
            crate::db::DatabasePool::Sqlite(db) => {
                Self::get_project_by_env_and_name_sqlite(db, env_id, name).await
            }
            crate::db::DatabasePool::Postgres(db) => {
                Self::get_project_by_env_and_name_postgres(db, env_id, name).await
            }
        }
    }

    async fn get_project_by_env_and_name_sqlite(
        db: &Pool<Sqlite>,
        env_id: &Uuid,
        name: &str,
    ) -> Result<Option<Project>, ProjectError> {
        let row = sqlx::query_as::<_, Project>(
            r#"
            SELECT project_id, env_id, name, active, created_by_user_id, created_at, updated_at,
                   updated_by_user_id, active_toggle_at, active_toggle_by_user_id,
                   deployment_sequence
            FROM project
            WHERE env_id = ? AND name = ?
            "#,
        )
        .bind(env_id)
        .bind(name)
        .fetch_optional(db)
        .await?;

        Ok(row)
    }

    async fn get_project_by_env_and_name_postgres(
        db: &Pool<Postgres>,
        env_id: &Uuid,
        name: &str,
    ) -> Result<Option<Project>, ProjectError> {
        let row = sqlx::query_as::<_, Project>(
            r#"
            SELECT project_id, env_id, name, active,
                   created_by_user_id, created_at, updated_at,
                   updated_by_user_id, active_toggle_at, active_toggle_by_user_id,
                   deployment_sequence
            FROM project
            WHERE env_id = $1 AND name = $2
            "#,
        )
        .bind(env_id)
        .bind(name)
        .fetch_optional(db)
        .await?;

        Ok(row)
    }

    /// Get count of projects
    pub async fn get_count(db: &crate::db::DatabasePool) -> Result<i64, ProjectError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM project")
                    .fetch_one(pg_pool)
                    .await?;
                Ok(count)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM project")
                    .fetch_one(sqlite_pool)
                    .await?;
                Ok(count)
            }
        }
    }

    /// Get count of projects within an environment.
    pub async fn get_count_by_env(
        db: &crate::db::DatabasePool,
        env_id: &Uuid,
    ) -> Result<i64, ProjectError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let count: i64 =
                    sqlx::query_scalar("SELECT COUNT(*) FROM project WHERE env_id = $1")
                        .bind(env_id)
                        .fetch_one(pg_pool)
                        .await?;
                Ok(count)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let count: i64 =
                    sqlx::query_scalar("SELECT COUNT(*) FROM project WHERE env_id = ?")
                        .bind(env_id)
                        .fetch_one(sqlite_pool)
                        .await?;
                Ok(count)
            }
        }
    }

    /// Get projects by environment ID
    pub async fn get_projects_by_env(
        db: &crate::db::DatabasePool,
        env_id: &Uuid,
        limit: Option<i64>,
        offset: Option<i64>,
    ) -> Result<Vec<Project>, ProjectError> {
        match db {
            crate::db::DatabasePool::Sqlite(db) => {
                Self::get_projects_by_env_sqlite(db, env_id, limit, offset).await
            }
            crate::db::DatabasePool::Postgres(db) => {
                Self::get_projects_by_env_postgres(db, env_id, limit, offset).await
            }
        }
    }

    async fn get_projects_by_env_sqlite(
        db: &Pool<Sqlite>,
        env_id: &Uuid,
        limit: Option<i64>,
        offset: Option<i64>,
    ) -> Result<Vec<Project>, ProjectError> {
        let limit = limit.unwrap_or(10);
        let offset = offset.unwrap_or(0);

        let rows = sqlx::query_as::<_, Project>(
            r#"
            SELECT project_id, env_id, name, active, created_by_user_id, created_at, updated_at,
                   updated_by_user_id, active_toggle_at, active_toggle_by_user_id,
                   deployment_sequence
            FROM project
            WHERE env_id = ?
            ORDER BY active DESC, created_at DESC
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

    async fn get_projects_by_env_postgres(
        db: &Pool<Postgres>,
        env_id: &Uuid,
        limit: Option<i64>,
        offset: Option<i64>,
    ) -> Result<Vec<Project>, ProjectError> {
        let limit = limit.unwrap_or(10);
        let offset = offset.unwrap_or(0);

        let rows = sqlx::query_as::<_, Project>(
            r#"
            SELECT project_id, env_id, name, active,
                   created_by_user_id, created_at, updated_at,
                   updated_by_user_id, active_toggle_at, active_toggle_by_user_id,
                   deployment_sequence
            FROM project
            WHERE env_id = $1
            ORDER BY active DESC, created_at DESC
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

    /// Insert a new project if it doesn't exist, return existing project if it does
    /// If an inactive project with the same name exists, it will be reactivated
    pub async fn insert_or_get_project(
        db: &crate::db::DatabasePool,
        project_id: &Uuid,
        env_id: &Uuid,
        name: &str,
        created_by_user_id: &Uuid,
    ) -> Result<Project, ProjectError> {
        // First try to get existing project
        if let Some(existing_project) = Self::get_project_by_env_and_name(db, env_id, name).await? {
            // If the project is inactive, reactivate it
            if !existing_project.active {
                tracing::info!(
                    "Reactivating inactive project '{}' ({})",
                    name,
                    existing_project.project_id
                );
                Self::toggle_active(db, &existing_project.project_id, true, created_by_user_id)
                    .await?;
                // Return the reactivated project
                return Self::get_project(db, &existing_project.project_id).await;
            }
            return Ok(existing_project);
        }

        // Insert new project if it doesn't exist
        Self::insert_project(db, project_id, env_id, name, created_by_user_id).await?;

        // Return the newly created project
        Self::get_project(db, project_id).await
    }

    /// Insert a new project
    pub async fn insert_project(
        db: &crate::db::DatabasePool,
        project_id: &Uuid,
        env_id: &Uuid,
        name: &str,
        created_by_user_id: &Uuid,
    ) -> Result<(), ProjectError> {
        let result = match db {
            crate::db::DatabasePool::Sqlite(db) => {
                Self::insert_project_sqlite(db, project_id, env_id, name, created_by_user_id).await
            }
            crate::db::DatabasePool::Postgres(db) => {
                Self::insert_project_postgres(db, project_id, env_id, name, created_by_user_id)
                    .await
            }
        };
        if result.is_ok() {
            Self::invalidate_project_cache(project_id);
        }
        result
    }

    async fn insert_project_sqlite(
        db: &Pool<Sqlite>,
        project_id: &Uuid,
        env_id: &Uuid,
        name: &str,
        created_by_user_id: &Uuid,
    ) -> Result<(), ProjectError> {
        sqlx::query(
            r#"
            INSERT INTO project (project_id, env_id, name, created_by_user_id)
            VALUES (?, ?, ?, ?)
            "#,
        )
        .bind(project_id)
        .bind(env_id)
        .bind(name)
        .bind(created_by_user_id)
        .execute(db)
        .await?;

        Ok(())
    }

    async fn insert_project_postgres(
        db: &Pool<Postgres>,
        project_id: &Uuid,
        env_id: &Uuid,
        name: &str,
        created_by_user_id: &Uuid,
    ) -> Result<(), ProjectError> {
        sqlx::query(
            r#"
            INSERT INTO project (project_id, env_id, name, created_by_user_id)
            VALUES ($1, $2, $3, $4)
            "#,
        )
        .bind(project_id)
        .bind(env_id)
        .bind(name)
        .bind(created_by_user_id)
        .execute(db)
        .await?;

        Ok(())
    }

    /// Update project name
    pub async fn update_name(
        db: &crate::db::DatabasePool,
        project_id: &Uuid,
        name: &str,
        updated_by_user_id: &Uuid,
    ) -> Result<(), ProjectError> {
        let result = match db {
            crate::db::DatabasePool::Sqlite(db) => {
                Self::update_name_sqlite(db, project_id, name, updated_by_user_id).await
            }
            crate::db::DatabasePool::Postgres(db) => {
                Self::update_name_postgres(db, project_id, name, updated_by_user_id).await
            }
        };
        if result.is_ok() {
            Self::invalidate_project_cache(project_id);
        }
        result
    }

    async fn update_name_sqlite(
        db: &Pool<Sqlite>,
        project_id: &Uuid,
        name: &str,
        updated_by_user_id: &Uuid,
    ) -> Result<(), ProjectError> {
        sqlx::query(
            r#"
            UPDATE project
            SET name = ?, updated_at = CURRENT_TIMESTAMP, updated_by_user_id = ?
            WHERE project_id = ?
            "#,
        )
        .bind(name)
        .bind(updated_by_user_id)
        .bind(project_id)
        .execute(db)
        .await?;

        Ok(())
    }

    async fn update_name_postgres(
        db: &Pool<Postgres>,
        project_id: &Uuid,
        name: &str,
        updated_by_user_id: &Uuid,
    ) -> Result<(), ProjectError> {
        sqlx::query(
            r#"
            UPDATE project
            SET name = $2, updated_at = now(), updated_by_user_id = $3
            WHERE project_id = $1
            "#,
        )
        .bind(project_id)
        .bind(name)
        .bind(updated_by_user_id)
        .execute(db)
        .await?;

        Ok(())
    }

    /// Toggle project active status
    /// When deactivating, this also undeploys all builds and deactivates all schedules
    pub async fn toggle_active(
        db: &crate::db::DatabasePool,
        project_id: &Uuid,
        active: bool,
        toggled_by_user_id: &Uuid,
    ) -> Result<(), ProjectError> {
        // When deactivating, clean up builds and schedules first
        if !active {
            // Snapshot the artifact counts that are about to become invisible
            // via the build undeploy below. Counting BEFORE undeploy means the
            // numbers reflect what was actually live for this project, which
            // is the most useful operator-facing summary. These artifacts are
            // not deleted — they're hidden by the runtime's `b.deployed` and
            // `p.active` filters, and they come back when the project is
            // reactivated and its build is redeployed.
            let visible_artifacts = Self::count_visible_artifacts_for_project(db, project_id)
                .await
                .unwrap_or((0, 0, 0, 0));

            // Undeploy all builds for this project
            let builds_undeployed =
                crate::db::Build::undeploy_all_builds_for_project(db, project_id)
                    .await
                    .map_err(|e| {
                        ProjectError::Other(format!("Failed to undeploy builds: {}", e))
                    })?;
            if builds_undeployed > 0 {
                tracing::info!(
                    "Undeployed {} build(s) for project {} during deactivation",
                    builds_undeployed,
                    project_id
                );
            }

            // Deactivate all schedules for this project
            let schedules_deactivated =
                crate::db::Schedule::deactivate_schedules_by_project(db, project_id)
                    .await
                    .map_err(|e| {
                        ProjectError::Other(format!("Failed to deactivate schedules: {}", e))
                    })?;
            if schedules_deactivated > 0 {
                tracing::info!(
                    "Deactivated {} schedule(s) for project {} during deactivation",
                    schedules_deactivated,
                    project_id
                );
            }

            // Surface the implicit cleanup so operators know what else just
            // got hidden. These rows aren't touched directly — they vanish
            // from runtime queries because their builds are no longer
            // deployed and the project is no longer active.
            let (handlers, webhooks, mcp_tools, agents) = visible_artifacts;
            if handlers + webhooks + mcp_tools + agents > 0 {
                tracing::info!(
                    "Project {} deactivation also hides {} event handler(s), {} webhook(s), {} MCP tool(s), {} agent(s) (rows preserved; restored on redeploy)",
                    project_id,
                    handlers,
                    webhooks,
                    mcp_tools,
                    agents
                );
            }
        }

        let result = match db {
            crate::db::DatabasePool::Sqlite(db) => {
                Self::toggle_active_sqlite(db, project_id, active, toggled_by_user_id).await
            }
            crate::db::DatabasePool::Postgres(db) => {
                Self::toggle_active_postgres(db, project_id, active, toggled_by_user_id).await
            }
        };
        if result.is_ok() {
            Self::invalidate_project_cache(project_id);
        }
        result
    }

    async fn toggle_active_sqlite(
        db: &Pool<Sqlite>,
        project_id: &Uuid,
        active: bool,
        toggled_by_user_id: &Uuid,
    ) -> Result<(), ProjectError> {
        let active_int = if active { 1 } else { 0 };

        sqlx::query(
            r#"
            UPDATE project
            SET active = ?, active_toggle_at = CURRENT_TIMESTAMP,
                active_toggle_by_user_id = ?, updated_at = CURRENT_TIMESTAMP,
                updated_by_user_id = ?
            WHERE project_id = ?
            "#,
        )
        .bind(active_int)
        .bind(toggled_by_user_id)
        .bind(toggled_by_user_id)
        .bind(project_id)
        .execute(db)
        .await?;

        Ok(())
    }

    async fn toggle_active_postgres(
        db: &Pool<Postgres>,
        project_id: &Uuid,
        active: bool,
        toggled_by_user_id: &Uuid,
    ) -> Result<(), ProjectError> {
        sqlx::query(
            r#"
            UPDATE project
            SET active = $2, active_toggle_at = now(),
                active_toggle_by_user_id = $3, updated_at = now(),
                updated_by_user_id = $3
            WHERE project_id = $1
            "#,
        )
        .bind(project_id)
        .bind(active)
        .bind(toggled_by_user_id)
        .execute(db)
        .await?;

        Ok(())
    }

    /// Count event handlers, webhooks, MCP tools, and agents that are
    /// currently visible at runtime for this project (i.e. attached to a
    /// build with `deployed = true`). Used by `toggle_active` to log a
    /// summary of what the implicit-cleanup is about to hide. Returns a
    /// `(handlers, webhooks, mcp_tools, agents)` tuple.
    ///
    /// This is intentionally tolerant: any individual COUNT failure
    /// returns 0 for that bucket so a transient DB hiccup doesn't block
    /// project deactivation.
    async fn count_visible_artifacts_for_project(
        db: &crate::db::DatabasePool,
        project_id: &Uuid,
    ) -> Result<(i64, i64, i64, i64), ProjectError> {
        match db {
            crate::db::DatabasePool::Sqlite(pool) => {
                let q = |table: &str| {
                    format!(
                        "SELECT COUNT(*) FROM {} t \
                         JOIN build b ON t.build_id = b.build_id \
                         WHERE b.project_id = ? AND b.deployed = 1 AND b.runtime_status = 'ready'",
                        table
                    )
                };
                let handlers =
                    sqlx::query_scalar::<_, i64>(sqlx::AssertSqlSafe(q("event_handler")))
                        .bind(project_id)
                        .fetch_one(pool)
                        .await
                        .unwrap_or(0);
                let webhooks = sqlx::query_scalar::<_, i64>(sqlx::AssertSqlSafe(q("webhook")))
                    .bind(project_id)
                    .fetch_one(pool)
                    .await
                    .unwrap_or(0);
                let mcp_tools = sqlx::query_scalar::<_, i64>(sqlx::AssertSqlSafe(q("mcp_tool")))
                    .bind(project_id)
                    .fetch_one(pool)
                    .await
                    .unwrap_or(0);
                let agents = sqlx::query_scalar::<_, i64>(sqlx::AssertSqlSafe(q("agent")))
                    .bind(project_id)
                    .fetch_one(pool)
                    .await
                    .unwrap_or(0);
                Ok((handlers, webhooks, mcp_tools, agents))
            }
            crate::db::DatabasePool::Postgres(pool) => {
                let q = |table: &str| {
                    format!(
                        "SELECT COUNT(*) FROM {} t \
                         JOIN build b ON t.build_id = b.build_id \
                         WHERE b.project_id = $1 AND b.deployed = true AND b.runtime_status = 'ready'",
                        table
                    )
                };
                let handlers =
                    sqlx::query_scalar::<_, i64>(sqlx::AssertSqlSafe(q("event_handler")))
                        .bind(project_id)
                        .fetch_one(pool)
                        .await
                        .unwrap_or(0);
                let webhooks = sqlx::query_scalar::<_, i64>(sqlx::AssertSqlSafe(q("webhook")))
                    .bind(project_id)
                    .fetch_one(pool)
                    .await
                    .unwrap_or(0);
                let mcp_tools = sqlx::query_scalar::<_, i64>(sqlx::AssertSqlSafe(q("mcp_tool")))
                    .bind(project_id)
                    .fetch_one(pool)
                    .await
                    .unwrap_or(0);
                let agents = sqlx::query_scalar::<_, i64>(sqlx::AssertSqlSafe(q("agent")))
                    .bind(project_id)
                    .fetch_one(pool)
                    .await
                    .unwrap_or(0);
                Ok((handlers, webhooks, mcp_tools, agents))
            }
        }
    }

    /// Delete project
    pub async fn delete_project(
        db: &crate::db::DatabasePool,
        project_id: &Uuid,
    ) -> Result<(), ProjectError> {
        match db {
            crate::db::DatabasePool::Sqlite(pool) => {
                sqlx::query("DELETE FROM project WHERE project_id = ?")
                    .bind(project_id)
                    .execute(pool)
                    .await?;
            }
            crate::db::DatabasePool::Postgres(pool) => {
                sqlx::query("DELETE FROM project WHERE project_id = $1")
                    .bind(project_id)
                    .execute(pool)
                    .await?;
            }
        }
        Self::invalidate_project_cache(project_id);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::create_db_pool;
    use crate::val;
    use uuid::Uuid;

    async fn cleanup_test_db(_db: &crate::db::DatabasePool) {
        // In a real test environment, we might want to clean up test data
        // For now, we're using in-memory SQLite so cleanup happens automatically
    }

    #[tokio::test]
    #[ignore]
    async fn test_project_operations() {
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
        let project_id = Uuid::now_v7();
        let env_id = Uuid::now_v7();
        let org_id = Uuid::now_v7();
        let user_id = Uuid::now_v7();

        // Insert test org and user first
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
        crate::db::User::insert_user(
            &db,
            &user_id,
            "test@example.com",
            Some("Test User"),
            Some(&user_id),
        )
        .await
        .unwrap();

        // Insert test env
        crate::db::Env::insert_env(&db, &env_id, &org_id, "test-env", &user_id)
            .await
            .unwrap();

        // Test insert_or_get_project (new project)
        let project_name = "test-project";
        let project =
            Project::insert_or_get_project(&db, &project_id, &env_id, project_name, &user_id)
                .await
                .unwrap();
        assert_eq!(project.name, project_name);
        assert_eq!(project.env_id, env_id);

        // Test insert_or_get_project (existing project)
        let project2 =
            Project::insert_or_get_project(&db, &Uuid::now_v7(), &env_id, project_name, &user_id)
                .await
                .unwrap();
        assert_eq!(project2.project_id, project.project_id); // Should return the same project

        // Test get_project_by_env_and_name
        let found_project = Project::get_project_by_env_and_name(&db, &env_id, project_name)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(found_project.project_id, project.project_id);

        // Test update_name
        let new_name = "updated-project-name";
        Project::update_name(&db, &project_id, new_name, &user_id)
            .await
            .expect("Failed to update project name");

        let updated_project = Project::get_project(&db, &project_id)
            .await
            .expect("Failed to get updated project");
        assert_eq!(updated_project.name, new_name);

        // Test toggle active
        Project::toggle_active(&db, &project_id, false, &user_id)
            .await
            .expect("Failed to toggle project active");

        // Should not find inactive project
        let result = Project::get_project(&db, &project_id).await;
        assert!(result.is_err());

        cleanup_test_db(&db).await;
    }
}

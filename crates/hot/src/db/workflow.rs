use serde_json::Value as JsonValue;
use sqlx::{FromRow, Pool, Postgres, Sqlite};
use thiserror::Error;
use uuid::Uuid;

#[derive(Error, Debug)]
pub enum WorkflowError {
    #[error("Database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("Workflow not found")]
    NotFound,
    #[error("Serialization error: {0}")]
    SerializationError(String),
}

#[derive(Debug, Clone, FromRow)]
pub struct Workflow {
    pub workflow_id: Uuid,
    pub build_id: Uuid,
    pub env_id: Uuid,
    pub type_name: String,
    pub namespace: String,
    pub name: Option<String>,
    pub description: Option<String>,
    pub tags: Option<JsonValue>,
    pub meta: Option<JsonValue>,
    pub file: Option<String>,
    pub line: Option<i32>,
    pub column: Option<i32>,
    pub position: Option<i32>,
}

#[derive(Debug, Clone, FromRow)]
pub struct WorkflowWithProject {
    pub workflow_id: Uuid,
    pub build_id: Uuid,
    pub env_id: Uuid,
    pub type_name: String,
    pub namespace: String,
    pub name: Option<String>,
    pub description: Option<String>,
    pub tags: Option<JsonValue>,
    pub meta: Option<JsonValue>,
    pub file: Option<String>,
    pub line: Option<i32>,
    pub column: Option<i32>,
    pub position: Option<i32>,
    pub project_id: Uuid,
    pub project_name: String,
}

impl Workflow {
    pub async fn get_workflows_by_build(
        db: &crate::db::DatabasePool,
        build_id: &Uuid,
    ) -> Result<Vec<Workflow>, WorkflowError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let workflows = sqlx::query_as::<_, Workflow>(
                    r#"SELECT workflow_id, build_id, env_id, type_name, namespace,
                              name, description, tags, meta,
                              file, line, "column", position
                       FROM workflow WHERE build_id = $1
                       ORDER BY namespace, type_name"#,
                )
                .bind(build_id)
                .fetch_all(pg_pool)
                .await?;
                Ok(workflows)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let workflows = sqlx::query_as::<_, Workflow>(
                    r#"SELECT workflow_id, build_id, env_id, type_name, namespace,
                              name, description, tags, meta,
                              file, line, "column", position
                       FROM workflow WHERE build_id = ?
                       ORDER BY namespace, type_name"#,
                )
                .bind(build_id)
                .fetch_all(sqlite_pool)
                .await?;
                Ok(workflows)
            }
        }
    }

    pub async fn get_workflows_by_env_deployed(
        db: &crate::db::DatabasePool,
        env_id: &Uuid,
    ) -> Result<Vec<WorkflowWithProject>, WorkflowError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let workflows = sqlx::query_as::<_, WorkflowWithProject>(
                    r#"SELECT w.workflow_id, w.build_id, w.env_id, w.type_name, w.namespace,
                              w.name, w.description, w.tags, w.meta,
                              w.file, w.line, w."column", w.position,
                              p.project_id, p.name as project_name
                       FROM workflow w
                       JOIN build b ON w.build_id = b.build_id
                       JOIN project p ON b.project_id = p.project_id
                       WHERE b.deployed = true AND b.active = true AND p.active = true AND p.env_id = $1
                       ORDER BY w.namespace, w.type_name"#,
                )
                .bind(env_id)
                .fetch_all(pg_pool)
                .await?;
                Ok(workflows)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let workflows = sqlx::query_as::<_, WorkflowWithProject>(
                    r#"SELECT w.workflow_id, w.build_id, w.env_id, w.type_name, w.namespace,
                              w.name, w.description, w.tags, w.meta,
                              w.file, w.line, w."column", w.position,
                              p.project_id, p.name as project_name
                       FROM workflow w
                       JOIN build b ON w.build_id = b.build_id
                       JOIN project p ON b.project_id = p.project_id
                       WHERE b.deployed = 1 AND b.active = 1 AND p.active = 1 AND p.env_id = ?
                       ORDER BY w.namespace, w.type_name"#,
                )
                .bind(env_id)
                .fetch_all(sqlite_pool)
                .await?;
                Ok(workflows)
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn insert_workflow_postgres(
        db: &Pool<Postgres>,
        workflow_id: &Uuid,
        build_id: &Uuid,
        env_id: &Uuid,
        type_name: &str,
        namespace: &str,
        name: Option<&str>,
        description: Option<&str>,
        tags: Option<&JsonValue>,
        meta: Option<&JsonValue>,
        file: Option<&str>,
        line: Option<i32>,
        column: Option<i32>,
        position: Option<i32>,
    ) -> Result<(), WorkflowError> {
        sqlx::query(
            r#"INSERT INTO workflow
               (workflow_id, build_id, env_id, type_name, namespace,
                name, description, tags, meta,
                file, line, "column", position)
               VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)
               ON CONFLICT (build_id, namespace, type_name) DO UPDATE SET
                name = EXCLUDED.name,
                description = EXCLUDED.description,
                tags = EXCLUDED.tags,
                meta = EXCLUDED.meta,
                file = EXCLUDED.file,
                line = EXCLUDED.line,
                "column" = EXCLUDED."column",
                position = EXCLUDED.position"#,
        )
        .bind(workflow_id)
        .bind(build_id)
        .bind(env_id)
        .bind(type_name)
        .bind(namespace)
        .bind(name)
        .bind(description)
        .bind(tags)
        .bind(meta)
        .bind(file)
        .bind(line)
        .bind(column)
        .bind(position)
        .execute(db)
        .await?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn insert_workflow_sqlite(
        db: &Pool<Sqlite>,
        workflow_id: &Uuid,
        build_id: &Uuid,
        env_id: &Uuid,
        type_name: &str,
        namespace: &str,
        name: Option<&str>,
        description: Option<&str>,
        tags: Option<&JsonValue>,
        meta: Option<&JsonValue>,
        file: Option<&str>,
        line: Option<i32>,
        column: Option<i32>,
        position: Option<i32>,
    ) -> Result<(), WorkflowError> {
        let tags_json = tags
            .map(serde_json::to_string)
            .transpose()
            .map_err(|e| WorkflowError::SerializationError(e.to_string()))?;
        let meta_json = meta
            .map(serde_json::to_string)
            .transpose()
            .map_err(|e| WorkflowError::SerializationError(e.to_string()))?;

        sqlx::query(
            r#"INSERT INTO workflow
               (workflow_id, build_id, env_id, type_name, namespace,
                name, description, tags, meta,
                file, line, "column", position)
               VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
               ON CONFLICT (build_id, namespace, type_name) DO UPDATE SET
                name = excluded.name,
                description = excluded.description,
                tags = excluded.tags,
                meta = excluded.meta,
                file = excluded.file,
                line = excluded.line,
                "column" = excluded."column",
                position = excluded.position"#,
        )
        .bind(workflow_id)
        .bind(build_id)
        .bind(env_id)
        .bind(type_name)
        .bind(namespace)
        .bind(name)
        .bind(description)
        .bind(tags_json)
        .bind(meta_json)
        .bind(file)
        .bind(line)
        .bind(column)
        .bind(position)
        .execute(db)
        .await?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn insert_workflow(
        db: &crate::db::DatabasePool,
        workflow_id: &Uuid,
        build_id: &Uuid,
        env_id: &Uuid,
        type_name: &str,
        namespace: &str,
        name: Option<&str>,
        description: Option<&str>,
        tags: Option<&JsonValue>,
        meta: Option<&JsonValue>,
        file: Option<&str>,
        line: Option<i32>,
        column: Option<i32>,
        position: Option<i32>,
    ) -> Result<(), WorkflowError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                Self::insert_workflow_postgres(
                    pg_pool,
                    workflow_id,
                    build_id,
                    env_id,
                    type_name,
                    namespace,
                    name,
                    description,
                    tags,
                    meta,
                    file,
                    line,
                    column,
                    position,
                )
                .await
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                Self::insert_workflow_sqlite(
                    sqlite_pool,
                    workflow_id,
                    build_id,
                    env_id,
                    type_name,
                    namespace,
                    name,
                    description,
                    tags,
                    meta,
                    file,
                    line,
                    column,
                    position,
                )
                .await
            }
        }
    }

    /// Insert a single named workflow from a Val map. Used by both the local
    /// batch path and the remote manifest path.
    pub async fn insert_workflow_from_val(
        db: &crate::db::DatabasePool,
        build_id: &Uuid,
        env_id: &Uuid,
        workflow_val: &crate::val::Val,
    ) -> Result<(), WorkflowError> {
        use crate::val::Val;

        let workflow_map = match workflow_val {
            Val::Map(map) => map,
            _ => {
                return Err(WorkflowError::SerializationError(
                    "Workflow is not a map".to_string(),
                ));
            }
        };

        let type_name = workflow_map
            .get(&Val::from("type_name"))
            .and_then(|v| match v {
                Val::Str(s) => Some((**s).to_owned()),
                _ => None,
            })
            .unwrap_or_default();

        let namespace = workflow_map
            .get(&Val::from("namespace"))
            .and_then(|v| match v {
                Val::Str(s) => Some((**s).to_owned()),
                _ => None,
            })
            .unwrap_or_default();

        let name = workflow_map.get(&Val::from("name")).and_then(|v| match v {
            Val::Str(s) => Some((**s).to_owned()),
            _ => None,
        });

        let description = workflow_map
            .get(&Val::from("description"))
            .and_then(|v| match v {
                Val::Str(s) => Some((**s).to_owned()),
                _ => None,
            });

        let tags = workflow_map
            .get(&Val::from("tags"))
            .and_then(|v| serde_json::to_value(v).ok());

        let meta = workflow_map
            .get(&Val::from("meta"))
            .and_then(|v| serde_json::to_value(v).ok());

        let file = workflow_map.get(&Val::from("file")).and_then(|v| match v {
            Val::Str(s) => Some((**s).to_owned()),
            _ => None,
        });

        let line = workflow_map.get(&Val::from("line")).and_then(|v| match v {
            Val::Int(i) => Some(*i as i32),
            _ => None,
        });

        let column = workflow_map
            .get(&Val::from("column"))
            .and_then(|v| match v {
                Val::Int(i) => Some(*i as i32),
                _ => None,
            });

        let position = workflow_map
            .get(&Val::from("position"))
            .and_then(|v| match v {
                Val::Int(i) => Some(*i as i32),
                _ => None,
            });

        let workflow_id = Uuid::now_v7();
        Self::insert_workflow(
            db,
            &workflow_id,
            build_id,
            env_id,
            &type_name,
            &namespace,
            name.as_deref(),
            description.as_deref(),
            tags.as_ref(),
            meta.as_ref(),
            file.as_deref(),
            line,
            column,
            position,
        )
        .await
    }

    pub async fn insert_workflows_for_build(
        db: &crate::db::DatabasePool,
        build_id: &Uuid,
        env_id: &Uuid,
        workflows: &crate::lang::compiler::WorkflowDefs,
    ) -> Result<(), WorkflowError> {
        for workflow_def in workflows {
            Self::insert_workflow_from_val(db, build_id, env_id, &workflow_def.workflow_val)
                .await?;
        }
        Ok(())
    }

    pub async fn delete_workflows_by_build(
        db: &crate::db::DatabasePool,
        build_id: &Uuid,
    ) -> Result<u64, WorkflowError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let result = sqlx::query("DELETE FROM workflow WHERE build_id = $1")
                    .bind(build_id)
                    .execute(pg_pool)
                    .await?;
                if result.rows_affected() > 0 {
                    tracing::info!(
                        "Deleted {} workflow(s) for build {}",
                        result.rows_affected(),
                        build_id
                    );
                }
                Ok(result.rows_affected())
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let result = sqlx::query("DELETE FROM workflow WHERE build_id = ?")
                    .bind(build_id)
                    .execute(sqlite_pool)
                    .await?;
                if result.rows_affected() > 0 {
                    tracing::info!(
                        "Deleted {} workflow(s) for build {}",
                        result.rows_affected(),
                        build_id
                    );
                }
                Ok(result.rows_affected())
            }
        }
    }

    pub async fn get_count_by_build(
        db: &crate::db::DatabasePool,
        build_id: &Uuid,
    ) -> Result<i64, WorkflowError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let count = sqlx::query_scalar::<_, i64>(
                    "SELECT COUNT(*) FROM workflow WHERE build_id = $1",
                )
                .bind(build_id)
                .fetch_one(pg_pool)
                .await?;
                Ok(count)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let count = sqlx::query_scalar::<_, i64>(
                    "SELECT COUNT(*) FROM workflow WHERE build_id = ?",
                )
                .bind(build_id)
                .fetch_one(sqlite_pool)
                .await?;
                Ok(count)
            }
        }
    }

    /// Get a single deployed named workflow by qualified name
    /// (e.g. "::acme::sales/LeadQualification").
    pub async fn get_workflow_by_qualified_name(
        db: &crate::db::DatabasePool,
        env_id: &Uuid,
        qualified_name: &str,
    ) -> Result<WorkflowWithProject, WorkflowError> {
        let (namespace, type_name) = qualified_name
            .rsplit_once('/')
            .ok_or(WorkflowError::NotFound)?;

        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                sqlx::query_as::<_, WorkflowWithProject>(
                    r#"SELECT w.workflow_id, w.build_id, w.env_id, w.type_name, w.namespace,
                              w.name, w.description, w.tags, w.meta,
                              w.file, w.line, w."column", w.position,
                              p.project_id, p.name as project_name
                       FROM workflow w
                       JOIN build b ON w.build_id = b.build_id
                       JOIN project p ON b.project_id = p.project_id
                       WHERE b.deployed = true AND b.active = true AND p.active = true AND p.env_id = $1
                         AND w.namespace = $2 AND w.type_name = $3
                       LIMIT 1"#,
                )
                .bind(env_id)
                .bind(namespace)
                .bind(type_name)
                .fetch_optional(pg_pool)
                .await?
                .ok_or(WorkflowError::NotFound)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                sqlx::query_as::<_, WorkflowWithProject>(
                    r#"SELECT w.workflow_id, w.build_id, w.env_id, w.type_name, w.namespace,
                              w.name, w.description, w.tags, w.meta,
                              w.file, w.line, w."column", w.position,
                              p.project_id, p.name as project_name
                       FROM workflow w
                       JOIN build b ON w.build_id = b.build_id
                       JOIN project p ON b.project_id = p.project_id
                       WHERE b.deployed = 1 AND b.active = 1 AND p.active = 1 AND p.env_id = ?
                         AND w.namespace = ? AND w.type_name = ?
                       LIMIT 1"#,
                )
                .bind(env_id)
                .bind(namespace)
                .bind(type_name)
                .fetch_optional(sqlite_pool)
                .await?
                .ok_or(WorkflowError::NotFound)
            }
        }
    }
}

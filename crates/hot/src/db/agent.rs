use serde_json::Value as JsonValue;
use sqlx::{FromRow, Pool, Postgres, Sqlite};
use thiserror::Error;
use uuid::Uuid;

#[derive(Error, Debug)]
pub enum AgentError {
    #[error("Database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("Agent not found")]
    NotFound,
    #[error("Serialization error: {0}")]
    SerializationError(String),
}

#[derive(Debug, Clone, FromRow)]
pub struct Agent {
    pub agent_id: Uuid,
    pub build_id: Uuid,
    pub env_id: Uuid,
    pub type_name: String,
    pub namespace: String,
    pub name: Option<String>,
    pub description: Option<String>,
    pub tags: Option<JsonValue>,
    pub config_fields: Option<JsonValue>,
    pub meta: Option<JsonValue>,
    pub file: Option<String>,
    pub line: Option<i32>,
    pub column: Option<i32>,
    pub position: Option<i32>,
}

#[derive(Debug, Clone, FromRow)]
pub struct AgentWithProject {
    pub agent_id: Uuid,
    pub build_id: Uuid,
    pub env_id: Uuid,
    pub type_name: String,
    pub namespace: String,
    pub name: Option<String>,
    pub description: Option<String>,
    pub tags: Option<JsonValue>,
    pub config_fields: Option<JsonValue>,
    pub meta: Option<JsonValue>,
    pub file: Option<String>,
    pub line: Option<i32>,
    pub column: Option<i32>,
    pub position: Option<i32>,
    pub project_id: Uuid,
    pub project_name: String,
}

impl Agent {
    pub async fn get_agents_by_build(
        db: &crate::db::DatabasePool,
        build_id: &Uuid,
    ) -> Result<Vec<Agent>, AgentError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let agents = sqlx::query_as::<_, Agent>(
                    r#"SELECT agent_id, build_id, env_id, type_name, namespace,
                              name, description, tags, config_fields, meta,
                              file, line, "column", position
                       FROM agent WHERE build_id = $1
                       ORDER BY namespace, type_name"#,
                )
                .bind(build_id)
                .fetch_all(pg_pool)
                .await?;
                Ok(agents)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let agents = sqlx::query_as::<_, Agent>(
                    r#"SELECT agent_id, build_id, env_id, type_name, namespace,
                              name, description, tags, config_fields, meta,
                              file, line, "column", position
                       FROM agent WHERE build_id = ?
                       ORDER BY namespace, type_name"#,
                )
                .bind(build_id)
                .fetch_all(sqlite_pool)
                .await?;
                Ok(agents)
            }
        }
    }

    pub async fn get_agents_by_env_deployed(
        db: &crate::db::DatabasePool,
        env_id: &Uuid,
    ) -> Result<Vec<AgentWithProject>, AgentError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let agents = sqlx::query_as::<_, AgentWithProject>(
                    r#"SELECT a.agent_id, a.build_id, a.env_id, a.type_name, a.namespace,
                              a.name, a.description, a.tags, a.config_fields, a.meta,
                              a.file, a.line, a."column", a.position,
                              p.project_id, p.name as project_name
                       FROM agent a
                       JOIN build b ON a.build_id = b.build_id
                       JOIN project p ON b.project_id = p.project_id
                       WHERE b.deployed = true AND b.runtime_status = 'ready' AND b.active = true AND p.active = true AND p.env_id = $1
                       ORDER BY a.namespace, a.type_name"#,
                )
                .bind(env_id)
                .fetch_all(pg_pool)
                .await?;
                Ok(agents)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let agents = sqlx::query_as::<_, AgentWithProject>(
                    r#"SELECT a.agent_id, a.build_id, a.env_id, a.type_name, a.namespace,
                              a.name, a.description, a.tags, a.config_fields, a.meta,
                              a.file, a.line, a."column", a.position,
                              p.project_id, p.name as project_name
                       FROM agent a
                       JOIN build b ON a.build_id = b.build_id
                       JOIN project p ON b.project_id = p.project_id
                       WHERE b.deployed = 1 AND b.runtime_status = 'ready' AND b.active = 1 AND p.active = 1 AND p.env_id = ?
                       ORDER BY a.namespace, a.type_name"#,
                )
                .bind(env_id)
                .fetch_all(sqlite_pool)
                .await?;
                Ok(agents)
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn insert_agent_postgres(
        db: &Pool<Postgres>,
        agent_id: &Uuid,
        build_id: &Uuid,
        env_id: &Uuid,
        type_name: &str,
        namespace: &str,
        name: Option<&str>,
        description: Option<&str>,
        tags: Option<&JsonValue>,
        config_fields: Option<&JsonValue>,
        meta: Option<&JsonValue>,
        file: Option<&str>,
        line: Option<i32>,
        column: Option<i32>,
        position: Option<i32>,
    ) -> Result<(), AgentError> {
        sqlx::query(
            r#"INSERT INTO agent
               (agent_id, build_id, env_id, type_name, namespace,
                name, description, tags, config_fields, meta,
                file, line, "column", position)
               VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14)
               ON CONFLICT (build_id, namespace, type_name) DO UPDATE SET
                name = EXCLUDED.name,
                description = EXCLUDED.description,
                tags = EXCLUDED.tags,
                config_fields = EXCLUDED.config_fields,
                meta = EXCLUDED.meta,
                file = EXCLUDED.file,
                line = EXCLUDED.line,
                "column" = EXCLUDED."column",
                position = EXCLUDED.position"#,
        )
        .bind(agent_id)
        .bind(build_id)
        .bind(env_id)
        .bind(type_name)
        .bind(namespace)
        .bind(name)
        .bind(description)
        .bind(tags)
        .bind(config_fields)
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
    async fn insert_agent_sqlite(
        db: &Pool<Sqlite>,
        agent_id: &Uuid,
        build_id: &Uuid,
        env_id: &Uuid,
        type_name: &str,
        namespace: &str,
        name: Option<&str>,
        description: Option<&str>,
        tags: Option<&JsonValue>,
        config_fields: Option<&JsonValue>,
        meta: Option<&JsonValue>,
        file: Option<&str>,
        line: Option<i32>,
        column: Option<i32>,
        position: Option<i32>,
    ) -> Result<(), AgentError> {
        let tags_json = tags
            .map(serde_json::to_string)
            .transpose()
            .map_err(|e| AgentError::SerializationError(e.to_string()))?;
        let config_fields_json = config_fields
            .map(serde_json::to_string)
            .transpose()
            .map_err(|e| AgentError::SerializationError(e.to_string()))?;
        let meta_json = meta
            .map(serde_json::to_string)
            .transpose()
            .map_err(|e| AgentError::SerializationError(e.to_string()))?;

        sqlx::query(
            r#"INSERT INTO agent
               (agent_id, build_id, env_id, type_name, namespace,
                name, description, tags, config_fields, meta,
                file, line, "column", position)
               VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
               ON CONFLICT (build_id, namespace, type_name) DO UPDATE SET
                name = excluded.name,
                description = excluded.description,
                tags = excluded.tags,
                config_fields = excluded.config_fields,
                meta = excluded.meta,
                file = excluded.file,
                line = excluded.line,
                "column" = excluded."column",
                position = excluded.position"#,
        )
        .bind(agent_id)
        .bind(build_id)
        .bind(env_id)
        .bind(type_name)
        .bind(namespace)
        .bind(name)
        .bind(description)
        .bind(tags_json)
        .bind(config_fields_json)
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
    async fn insert_agent(
        db: &crate::db::DatabasePool,
        agent_id: &Uuid,
        build_id: &Uuid,
        env_id: &Uuid,
        type_name: &str,
        namespace: &str,
        name: Option<&str>,
        description: Option<&str>,
        tags: Option<&JsonValue>,
        config_fields: Option<&JsonValue>,
        meta: Option<&JsonValue>,
        file: Option<&str>,
        line: Option<i32>,
        column: Option<i32>,
        position: Option<i32>,
    ) -> Result<(), AgentError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                Self::insert_agent_postgres(
                    pg_pool,
                    agent_id,
                    build_id,
                    env_id,
                    type_name,
                    namespace,
                    name,
                    description,
                    tags,
                    config_fields,
                    meta,
                    file,
                    line,
                    column,
                    position,
                )
                .await
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                Self::insert_agent_sqlite(
                    sqlite_pool,
                    agent_id,
                    build_id,
                    env_id,
                    type_name,
                    namespace,
                    name,
                    description,
                    tags,
                    config_fields,
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

    /// Insert a single agent from a Val map.  Used by both the local batch path
    /// and the remote manifest path.
    pub async fn insert_agent_from_val(
        db: &crate::db::DatabasePool,
        build_id: &Uuid,
        env_id: &Uuid,
        agent_val: &crate::val::Val,
    ) -> Result<(), AgentError> {
        use crate::val::Val;

        let agent_map = match agent_val {
            Val::Map(map) => map,
            _ => {
                return Err(AgentError::SerializationError(
                    "Agent is not a map".to_string(),
                ));
            }
        };

        let type_name = agent_map
            .get(&Val::from("type_name"))
            .and_then(|v| match v {
                Val::Str(s) => Some((**s).to_owned()),
                _ => None,
            })
            .unwrap_or_default();

        let namespace = agent_map
            .get(&Val::from("namespace"))
            .and_then(|v| match v {
                Val::Str(s) => Some((**s).to_owned()),
                _ => None,
            })
            .unwrap_or_default();

        let name = agent_map.get(&Val::from("name")).and_then(|v| match v {
            Val::Str(s) => Some((**s).to_owned()),
            _ => None,
        });

        let description = agent_map
            .get(&Val::from("description"))
            .and_then(|v| match v {
                Val::Str(s) => Some((**s).to_owned()),
                _ => None,
            });

        let tags = agent_map
            .get(&Val::from("tags"))
            .and_then(|v| serde_json::to_value(v).ok());

        let config_fields = agent_map
            .get(&Val::from("config_fields"))
            .and_then(|v| serde_json::to_value(v).ok());

        let meta = agent_map
            .get(&Val::from("meta"))
            .and_then(|v| serde_json::to_value(v).ok());

        let file = agent_map.get(&Val::from("file")).and_then(|v| match v {
            Val::Str(s) => Some((**s).to_owned()),
            _ => None,
        });

        let line = agent_map.get(&Val::from("line")).and_then(|v| match v {
            Val::Int(i) => Some(*i as i32),
            _ => None,
        });

        let column = agent_map.get(&Val::from("column")).and_then(|v| match v {
            Val::Int(i) => Some(*i as i32),
            _ => None,
        });

        let position = agent_map.get(&Val::from("position")).and_then(|v| match v {
            Val::Int(i) => Some(*i as i32),
            _ => None,
        });

        let agent_id = Uuid::now_v7();
        Self::insert_agent(
            db,
            &agent_id,
            build_id,
            env_id,
            &type_name,
            &namespace,
            name.as_deref(),
            description.as_deref(),
            tags.as_ref(),
            config_fields.as_ref(),
            meta.as_ref(),
            file.as_deref(),
            line,
            column,
            position,
        )
        .await
    }

    pub async fn insert_agents_for_build(
        db: &crate::db::DatabasePool,
        build_id: &Uuid,
        env_id: &Uuid,
        agents: &crate::lang::compiler::AgentDefs,
    ) -> Result<(), AgentError> {
        for agent_def in agents {
            Self::insert_agent_from_val(db, build_id, env_id, &agent_def.agent_val).await?;
        }
        Ok(())
    }

    pub async fn delete_agents_by_build(
        db: &crate::db::DatabasePool,
        build_id: &Uuid,
    ) -> Result<u64, AgentError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let result = sqlx::query("DELETE FROM agent WHERE build_id = $1")
                    .bind(build_id)
                    .execute(pg_pool)
                    .await?;
                if result.rows_affected() > 0 {
                    tracing::debug!(
                        "Deleted {} agent(s) for build {}",
                        result.rows_affected(),
                        build_id
                    );
                }
                Ok(result.rows_affected())
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let result = sqlx::query("DELETE FROM agent WHERE build_id = ?")
                    .bind(build_id)
                    .execute(sqlite_pool)
                    .await?;
                if result.rows_affected() > 0 {
                    tracing::debug!(
                        "Deleted {} agent(s) for build {}",
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
    ) -> Result<i64, AgentError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let count =
                    sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM agent WHERE build_id = $1")
                        .bind(build_id)
                        .fetch_one(pg_pool)
                        .await?;
                Ok(count)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let count =
                    sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM agent WHERE build_id = ?")
                        .bind(build_id)
                        .fetch_one(sqlite_pool)
                        .await?;
                Ok(count)
            }
        }
    }

    /// Get a single deployed agent by its qualified name (e.g. "::acme::support/SupportAgent").
    pub async fn get_agent_by_qualified_name(
        db: &crate::db::DatabasePool,
        env_id: &Uuid,
        qualified_name: &str,
    ) -> Result<AgentWithProject, AgentError> {
        let (namespace, type_name) = qualified_name
            .rsplit_once('/')
            .ok_or(AgentError::NotFound)?;

        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => sqlx::query_as::<_, AgentWithProject>(
                r#"SELECT a.agent_id, a.build_id, a.env_id, a.type_name, a.namespace,
                              a.name, a.description, a.tags, a.config_fields, a.meta,
                              a.file, a.line, a."column", a.position,
                              p.project_id, p.name as project_name
                       FROM agent a
                       JOIN build b ON a.build_id = b.build_id
                       JOIN project p ON b.project_id = p.project_id
                       WHERE b.deployed = true AND b.runtime_status = 'ready' AND b.active = true AND p.active = true AND p.env_id = $1
                         AND a.namespace = $2 AND a.type_name = $3
                       LIMIT 1"#,
            )
            .bind(env_id)
            .bind(namespace)
            .bind(type_name)
            .fetch_optional(pg_pool)
            .await?
            .ok_or(AgentError::NotFound),
            crate::db::DatabasePool::Sqlite(sqlite_pool) => sqlx::query_as::<_, AgentWithProject>(
                r#"SELECT a.agent_id, a.build_id, a.env_id, a.type_name, a.namespace,
                              a.name, a.description, a.tags, a.config_fields, a.meta,
                              a.file, a.line, a."column", a.position,
                              p.project_id, p.name as project_name
                       FROM agent a
                       JOIN build b ON a.build_id = b.build_id
                       JOIN project p ON b.project_id = p.project_id
                       WHERE b.deployed = 1 AND b.runtime_status = 'ready' AND b.active = 1 AND p.active = 1 AND p.env_id = ?
                         AND a.namespace = ? AND a.type_name = ?
                       LIMIT 1"#,
            )
            .bind(env_id)
            .bind(namespace)
            .bind(type_name)
            .fetch_optional(sqlite_pool)
            .await?
            .ok_or(AgentError::NotFound),
        }
    }
}

/// Run statistics for an agent, used for hero metrics on the Agent Dashboard.
#[derive(Debug, Clone, FromRow)]
pub struct AgentRunStats {
    pub total_runs: i64,
    pub ok_runs: i64,
    pub err_runs: i64,
    pub avg_duration_ms: Option<i64>,
}

impl AgentRunStats {
    pub fn success_rate(&self) -> f64 {
        if self.total_runs == 0 {
            100.0
        } else {
            (self.ok_runs as f64 / self.total_runs as f64) * 100.0
        }
    }

    /// Same thresholds as [`AgentHealthSummary::health_color`].
    pub fn health_color(&self) -> &'static str {
        if self.total_runs == 0 {
            return "idle";
        }
        let rate = self.success_rate();
        if rate >= 95.0 {
            "green"
        } else if rate >= 80.0 {
            "yellow"
        } else {
            "red"
        }
    }
}

/// Agent/non-agent run counts for the main Dashboard breakdown.
#[derive(Debug, Clone, FromRow)]
pub struct AgentNonAgentCounts {
    pub agent_runs: i64,
    pub non_agent_runs: i64,
}

/// Per-agent health summary for the Dashboard health widget.
#[derive(Debug, Clone)]
pub struct AgentHealthSummary {
    pub agent_id: Uuid,
    pub qualified_name: String,
    pub display_name: String,
    pub total_runs: i64,
    pub ok_runs: i64,
    pub err_runs: i64,
}

impl AgentHealthSummary {
    pub fn success_rate(&self) -> f64 {
        if self.total_runs == 0 {
            100.0
        } else {
            (self.ok_runs as f64 / self.total_runs as f64) * 100.0
        }
    }

    pub fn health_color(&self) -> &'static str {
        if self.total_runs == 0 {
            return "idle";
        }
        let rate = self.success_rate();
        if rate >= 95.0 {
            "green"
        } else if rate >= 80.0 {
            "yellow"
        } else {
            "red"
        }
    }
}

/// Queries for agent run statistics (used by both Agent Dashboard and main Dashboard).
pub struct AgentStats;

impl AgentStats {
    /// Get run stats for a single agent within a time window.
    pub async fn get_agent_run_stats(
        db: &crate::db::DatabasePool,
        env_id: &Uuid,
        agent_type: &str,
        hours: i64,
    ) -> Result<AgentRunStats, AgentError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let row = sqlx::query_as::<_, AgentRunStats>(
                    r#"SELECT
                         COUNT(*)::bigint as total_runs,
                         COUNT(*) FILTER (WHERE rs.status = 'succeeded')::bigint as ok_runs,
                         COUNT(*) FILTER (WHERE rs.status = 'failed')::bigint as err_runs,
                         AVG(EXTRACT(EPOCH FROM (r.stop_time - r.start_time)) * 1000)::bigint as avg_duration_ms
                       FROM run r
                       JOIN run_status rs ON r.status_id = rs.status_id
                       JOIN env e ON r.env_id = e.env_id
                       WHERE r.env_id = $1
                         AND r.agent_type = $2
                         AND r.start_time >= NOW() - make_interval(hours => $3)
                    "#,
                )
                .bind(env_id)
                .bind(agent_type)
                .bind(hours as i32)
                .fetch_one(pg_pool)
                .await?;
                Ok(row)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let row = sqlx::query_as::<_, AgentRunStats>(
                    r#"SELECT
                         COUNT(*) as total_runs,
                         COALESCE(SUM(CASE WHEN rs.status = 'succeeded' THEN 1 ELSE 0 END), 0) as ok_runs,
                         COALESCE(SUM(CASE WHEN rs.status = 'failed' THEN 1 ELSE 0 END), 0) as err_runs,
                         CAST(AVG((julianday(r.stop_time) - julianday(r.start_time)) * 86400000) AS INTEGER) as avg_duration_ms
                       FROM run r
                       JOIN run_status rs ON r.status_id = rs.status_id
                       WHERE r.env_id = ?
                         AND r.agent_type = ?
                         AND r.start_time >= datetime('now', '-' || ? || ' hours')
                    "#,
                )
                .bind(env_id)
                .bind(agent_type)
                .bind(hours)
                .fetch_one(sqlite_pool)
                .await?;
                Ok(row)
            }
        }
    }

    /// Get agent vs non-agent run counts for the main Dashboard.
    pub async fn get_agent_vs_nonagent_counts(
        db: &crate::db::DatabasePool,
        env_id: &Uuid,
        hours: i64,
    ) -> Result<AgentNonAgentCounts, AgentError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let row = sqlx::query_as::<_, AgentNonAgentCounts>(
                    r#"SELECT
                         COUNT(*) FILTER (WHERE r.agent_type IS NOT NULL)::bigint as agent_runs,
                         COUNT(*) FILTER (WHERE r.agent_type IS NULL)::bigint as non_agent_runs
                       FROM run r
                       WHERE r.env_id = $1
                         AND r.start_time >= NOW() - make_interval(hours => $2)
                    "#,
                )
                .bind(env_id)
                .bind(hours as i32)
                .fetch_one(pg_pool)
                .await?;
                Ok(row)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let row = sqlx::query_as::<_, AgentNonAgentCounts>(
                    r#"SELECT
                         SUM(CASE WHEN r.agent_type IS NOT NULL THEN 1 ELSE 0 END) as agent_runs,
                         SUM(CASE WHEN r.agent_type IS NULL THEN 1 ELSE 0 END) as non_agent_runs
                       FROM run r
                       WHERE r.env_id = ?
                         AND r.start_time >= datetime('now', '-' || ? || ' hours')
                    "#,
                )
                .bind(env_id)
                .bind(hours)
                .fetch_one(sqlite_pool)
                .await?;
                Ok(row)
            }
        }
    }

    /// Per-agent health summaries for the Dashboard widget.
    /// Joins against deployed agents to get display names, then counts runs.
    ///
    /// `cutoff: None` means "all time"; `project_id: None` means all projects.
    pub async fn get_per_agent_health(
        db: &crate::db::DatabasePool,
        env_id: &Uuid,
        cutoff: Option<chrono::DateTime<chrono::Utc>>,
        project_id: Option<&Uuid>,
    ) -> Result<Vec<AgentHealthSummary>, AgentError> {
        #[derive(FromRow)]
        struct Row {
            agent_id: Uuid,
            namespace: String,
            type_name: String,
            name: Option<String>,
            total_runs: i64,
            ok_runs: i64,
            err_runs: i64,
        }

        let rows: Vec<Row> = match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                sqlx::query_as::<_, Row>(
                    r#"SELECT a.agent_id, a.namespace, a.type_name, a.name,
                         COALESCE(stats.total_runs, 0)::bigint as total_runs,
                         COALESCE(stats.ok_runs, 0)::bigint as ok_runs,
                         COALESCE(stats.err_runs, 0)::bigint as err_runs
                       FROM agent a
                       JOIN build b ON a.build_id = b.build_id
                       JOIN project p ON b.project_id = p.project_id
                       LEFT JOIN LATERAL (
                         SELECT
                           COUNT(*) as total_runs,
                           COUNT(*) FILTER (WHERE rs.status = 'succeeded') as ok_runs,
                           COUNT(*) FILTER (WHERE rs.status = 'failed') as err_runs
                         FROM run r
                         JOIN run_status rs ON r.status_id = rs.status_id
                         WHERE r.env_id = $1
                           AND r.agent_type = a.namespace || '/' || a.type_name
                           AND ($2::timestamptz IS NULL OR r.start_time >= $2)
                       ) stats ON true
                       WHERE b.deployed = true AND b.runtime_status = 'ready' AND b.active = true AND p.active = true AND p.env_id = $1
                         AND ($3::uuid IS NULL OR p.project_id = $3)
                       ORDER BY COALESCE(stats.total_runs, 0) DESC"#,
                )
                .bind(env_id)
                .bind(cutoff)
                .bind(project_id.copied())
                .fetch_all(pg_pool)
                .await?
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                // Cutoff is bound as a formatted string to match SQLite's
                // stored text timestamps (same pattern as run.rs).
                let cutoff_str = cutoff.map(|c| c.format("%Y-%m-%d %H:%M:%S").to_string());
                sqlx::query_as::<_, Row>(
                    r#"SELECT a.agent_id, a.namespace, a.type_name, a.name,
                         COALESCE(stats.total_runs, 0) as total_runs,
                         COALESCE(stats.ok_runs, 0) as ok_runs,
                         COALESCE(stats.err_runs, 0) as err_runs
                       FROM agent a
                       JOIN build b ON a.build_id = b.build_id
                       JOIN project p ON b.project_id = p.project_id
                       LEFT JOIN (
                         SELECT r.agent_type,
                           COUNT(*) as total_runs,
                           SUM(CASE WHEN rs.status = 'succeeded' THEN 1 ELSE 0 END) as ok_runs,
                           SUM(CASE WHEN rs.status = 'failed' THEN 1 ELSE 0 END) as err_runs
                         FROM run r
                         JOIN run_status rs ON r.status_id = rs.status_id
                         WHERE r.env_id = ?1
                           AND r.agent_type IS NOT NULL
                           AND (?2 IS NULL OR r.start_time >= ?2)
                         GROUP BY r.agent_type
                       ) stats ON stats.agent_type = a.namespace || '/' || a.type_name
                       WHERE b.deployed = 1 AND b.runtime_status = 'ready' AND b.active = 1 AND p.active = 1 AND p.env_id = ?1
                         AND (?3 IS NULL OR p.project_id = ?3)
                       ORDER BY COALESCE(stats.total_runs, 0) DESC"#,
                )
                .bind(env_id)
                .bind(cutoff_str)
                .bind(project_id.copied())
                .fetch_all(sqlite_pool)
                .await?
            }
        };

        Ok(rows
            .into_iter()
            .map(|r| AgentHealthSummary {
                agent_id: r.agent_id,
                qualified_name: format!("{}/{}", r.namespace, r.type_name),
                display_name: r.name.unwrap_or_else(|| r.type_name.clone()),
                total_runs: r.total_runs,
                ok_runs: r.ok_runs,
                err_runs: r.err_runs,
            })
            .collect())
    }
}

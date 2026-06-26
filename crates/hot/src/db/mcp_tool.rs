use serde_json::Value as JsonValue;
use sqlx::{FromRow, Pool, Postgres, Sqlite};
use thiserror::Error;
use uuid::Uuid;

fn normalize_auth_mode(auth_mode: Option<&str>) -> &'static str {
    match auth_mode {
        Some("none") => "none",
        _ => "required",
    }
}

fn auth_mode_from_meta(meta: Option<&JsonValue>) -> &'static str {
    normalize_auth_mode(
        meta.and_then(|m| m.get("mcp"))
            .and_then(|mcp| mcp.get("auth"))
            .and_then(|a| a.as_str()),
    )
}

fn with_normalized_auth_mode(
    meta: Option<JsonValue>,
    auth_mode: &'static str,
) -> Option<JsonValue> {
    if meta.is_none() && auth_mode == "required" {
        return None;
    }

    let mut meta_obj = match meta {
        Some(JsonValue::Object(obj)) => obj,
        Some(other) => return Some(other),
        None => serde_json::Map::new(),
    };

    let mut mcp_obj = meta_obj
        .remove("mcp")
        .and_then(|mcp| match mcp {
            JsonValue::Object(obj) => Some(obj),
            _ => None,
        })
        .unwrap_or_default();

    mcp_obj.insert("auth".to_string(), JsonValue::String(auth_mode.to_string()));
    meta_obj.insert("mcp".to_string(), JsonValue::Object(mcp_obj));

    Some(JsonValue::Object(meta_obj))
}

#[derive(Error, Debug)]
pub enum McpToolError {
    #[error("Database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("MCP tool not found")]
    NotFound,
    #[error("Serialization error: {0}")]
    SerializationError(String),
}

#[derive(Debug, Clone, FromRow)]
pub struct McpTool {
    pub mcp_tool_id: Uuid,
    pub build_id: Uuid,
    pub service: String,
    pub ns: String,
    pub var: String,
    pub name: String,
    pub description: Option<String>,
    pub input_schema: Option<JsonValue>,
    pub output_schema: Option<JsonValue>,
    pub title: Option<String>,
    pub icons: Option<JsonValue>,
    pub annotations: Option<JsonValue>,
    pub meta: Option<JsonValue>,
    pub file: Option<String>,
    pub line: Option<i32>,
    pub column: Option<i32>,
    pub position: Option<i32>,
}

impl McpTool {
    /// Extract auth_mode from the meta JSON. Defaults to "required".
    ///
    /// Only exact `none` is public. Missing, malformed, or unknown values fail
    /// closed as `required`.
    pub fn auth_mode(&self) -> &'static str {
        auth_mode_from_meta(self.meta.as_ref())
    }

    /// Returns true if this tool allows unauthenticated access.
    pub fn is_public(&self) -> bool {
        self.auth_mode() == "none"
    }

    /// Extract user-declared secret header names from top-level `meta.secret-headers`.
    pub fn secret_headers(&self) -> Vec<String> {
        self.meta
            .as_ref()
            .and_then(|m| m.get("secret-headers"))
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_lowercase()))
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// McpTool with project information for display purposes
#[derive(Debug, FromRow)]
pub struct McpToolWithProject {
    pub mcp_tool_id: Uuid,
    pub build_id: Uuid,
    pub service: String,
    pub ns: String,
    pub var: String,
    pub name: String,
    pub description: Option<String>,
    pub input_schema: Option<JsonValue>,
    pub output_schema: Option<JsonValue>,
    pub title: Option<String>,
    pub icons: Option<JsonValue>,
    pub annotations: Option<JsonValue>,
    pub meta: Option<JsonValue>,
    pub file: Option<String>,
    pub line: Option<i32>,
    pub column: Option<i32>,
    pub position: Option<i32>,
    pub project_id: Uuid,
    pub project_name: String,
}

impl McpToolWithProject {
    /// Extract auth_mode from the meta JSON. Defaults to "required".
    ///
    /// Only exact `none` is public. Missing, malformed, or unknown values fail
    /// closed as `required`.
    pub fn auth_mode(&self) -> &'static str {
        auth_mode_from_meta(self.meta.as_ref())
    }
}

/// Summary of an MCP service for list display
#[derive(Debug, Clone)]
pub struct McpServiceSummary {
    pub service: String,
    pub tool_count: i64,
    pub projects: Vec<String>,
}

impl McpTool {
    /// Get MCP tool by ID
    pub async fn get_mcp_tool(
        db: &crate::db::DatabasePool,
        mcp_tool_id: &Uuid,
    ) -> Result<McpTool, McpToolError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                Self::get_mcp_tool_postgres(pg_pool, mcp_tool_id).await
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                Self::get_mcp_tool_sqlite(sqlite_pool, mcp_tool_id).await
            }
        }
    }

    async fn get_mcp_tool_sqlite(
        db: &Pool<Sqlite>,
        mcp_tool_id: &Uuid,
    ) -> Result<McpTool, McpToolError> {
        let tool = sqlx::query_as::<_, McpTool>(
            r#"SELECT mcp_tool_id, build_id, service, ns, var, name, description, 
                      input_schema, output_schema, title, icons, annotations, meta,
                      file, line, "column", position 
               FROM mcp_tool WHERE mcp_tool_id = ?"#,
        )
        .bind(mcp_tool_id)
        .fetch_one(db)
        .await?;
        Ok(tool)
    }

    async fn get_mcp_tool_postgres(
        db: &Pool<Postgres>,
        mcp_tool_id: &Uuid,
    ) -> Result<McpTool, McpToolError> {
        let tool = sqlx::query_as::<_, McpTool>(
            r#"SELECT mcp_tool_id, build_id, service, ns, var, name, description,
                      input_schema, output_schema, title, icons, annotations, meta,
                      file, line, "column", position 
               FROM mcp_tool WHERE mcp_tool_id = $1"#,
        )
        .bind(mcp_tool_id)
        .fetch_one(db)
        .await?;
        Ok(tool)
    }

    /// Get MCP tools by build ID
    pub async fn get_mcp_tools_by_build(
        db: &crate::db::DatabasePool,
        build_id: &Uuid,
        limit: Option<i64>,
        offset: Option<i64>,
    ) -> Result<Vec<McpTool>, McpToolError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                Self::get_mcp_tools_by_build_postgres(pg_pool, build_id, limit, offset).await
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                Self::get_mcp_tools_by_build_sqlite(sqlite_pool, build_id, limit, offset).await
            }
        }
    }

    async fn get_mcp_tools_by_build_sqlite(
        db: &Pool<Sqlite>,
        build_id: &Uuid,
        limit: Option<i64>,
        offset: Option<i64>,
    ) -> Result<Vec<McpTool>, McpToolError> {
        let limit = limit.unwrap_or(100);
        let offset = offset.unwrap_or(0);

        let tools = sqlx::query_as::<_, McpTool>(
            r#"SELECT mcp_tool_id, build_id, service, ns, var, name, description,
                      input_schema, output_schema, title, icons, annotations, meta,
                      file, line, "column", position 
               FROM mcp_tool WHERE build_id = ? 
               ORDER BY service, name LIMIT ? OFFSET ?"#,
        )
        .bind(build_id)
        .bind(limit)
        .bind(offset)
        .fetch_all(db)
        .await?;
        Ok(tools)
    }

    async fn get_mcp_tools_by_build_postgres(
        db: &Pool<Postgres>,
        build_id: &Uuid,
        limit: Option<i64>,
        offset: Option<i64>,
    ) -> Result<Vec<McpTool>, McpToolError> {
        let limit = limit.unwrap_or(100);
        let offset = offset.unwrap_or(0);

        let tools = sqlx::query_as::<_, McpTool>(
            r#"SELECT mcp_tool_id, build_id, service, ns, var, name, description,
                      input_schema, output_schema, title, icons, annotations, meta,
                      file, line, "column", position 
               FROM mcp_tool WHERE build_id = $1 
               ORDER BY service, name LIMIT $2 OFFSET $3"#,
        )
        .bind(build_id)
        .bind(limit)
        .bind(offset)
        .fetch_all(db)
        .await?;
        Ok(tools)
    }

    /// Get MCP tools by build ID and service
    pub async fn get_mcp_tools_by_build_and_service(
        db: &crate::db::DatabasePool,
        build_id: &Uuid,
        service: &str,
        limit: Option<i64>,
        offset: Option<i64>,
    ) -> Result<Vec<McpTool>, McpToolError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                Self::get_mcp_tools_by_build_and_service_postgres(
                    pg_pool, build_id, service, limit, offset,
                )
                .await
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                Self::get_mcp_tools_by_build_and_service_sqlite(
                    sqlite_pool,
                    build_id,
                    service,
                    limit,
                    offset,
                )
                .await
            }
        }
    }

    async fn get_mcp_tools_by_build_and_service_sqlite(
        db: &Pool<Sqlite>,
        build_id: &Uuid,
        service: &str,
        limit: Option<i64>,
        offset: Option<i64>,
    ) -> Result<Vec<McpTool>, McpToolError> {
        let limit = limit.unwrap_or(100);
        let offset = offset.unwrap_or(0);

        let tools = sqlx::query_as::<_, McpTool>(
            r#"SELECT mcp_tool_id, build_id, service, ns, var, name, description,
                      input_schema, output_schema, title, icons, annotations, meta,
                      file, line, "column", position 
               FROM mcp_tool WHERE build_id = ? AND service = ?
               ORDER BY name LIMIT ? OFFSET ?"#,
        )
        .bind(build_id)
        .bind(service)
        .bind(limit)
        .bind(offset)
        .fetch_all(db)
        .await?;
        Ok(tools)
    }

    async fn get_mcp_tools_by_build_and_service_postgres(
        db: &Pool<Postgres>,
        build_id: &Uuid,
        service: &str,
        limit: Option<i64>,
        offset: Option<i64>,
    ) -> Result<Vec<McpTool>, McpToolError> {
        let limit = limit.unwrap_or(100);
        let offset = offset.unwrap_or(0);

        let tools = sqlx::query_as::<_, McpTool>(
            r#"SELECT mcp_tool_id, build_id, service, ns, var, name, description,
                      input_schema, output_schema, title, icons, annotations, meta,
                      file, line, "column", position 
               FROM mcp_tool WHERE build_id = $1 AND service = $2
               ORDER BY name LIMIT $3 OFFSET $4"#,
        )
        .bind(build_id)
        .bind(service)
        .bind(limit)
        .bind(offset)
        .fetch_all(db)
        .await?;
        Ok(tools)
    }

    /// Get MCP tools by environment ID and service
    /// This queries across ALL deployed builds in the environment
    /// Used for MCP tools/list endpoint
    pub async fn get_mcp_tools_by_env_and_service(
        db: &crate::db::DatabasePool,
        env_id: &Uuid,
        service: &str,
    ) -> Result<Vec<McpTool>, McpToolError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                Self::get_mcp_tools_by_env_and_service_postgres(pg_pool, env_id, service).await
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                Self::get_mcp_tools_by_env_and_service_sqlite(sqlite_pool, env_id, service).await
            }
        }
    }

    async fn get_mcp_tools_by_env_and_service_sqlite(
        db: &Pool<Sqlite>,
        env_id: &Uuid,
        service: &str,
    ) -> Result<Vec<McpTool>, McpToolError> {
        let tools = sqlx::query_as::<_, McpTool>(
            r#"
            SELECT mt.mcp_tool_id, mt.build_id, mt.service, mt.ns, mt.var, mt.name, 
                   mt.description, mt.input_schema, mt.output_schema, mt.title,
                   mt.icons, mt.annotations, mt.meta, mt.file, mt.line, mt."column", mt.position
            FROM mcp_tool mt
            INNER JOIN build b ON mt.build_id = b.build_id
            INNER JOIN project p ON b.project_id = p.project_id
            WHERE p.env_id = ?
              AND p.active = 1
              AND b.deployed = 1
              AND b.runtime_status = 'ready'
              AND mt.service = ?
            ORDER BY mt.name
            "#,
        )
        .bind(env_id)
        .bind(service)
        .fetch_all(db)
        .await?;
        Ok(tools)
    }

    async fn get_mcp_tools_by_env_and_service_postgres(
        db: &Pool<Postgres>,
        env_id: &Uuid,
        service: &str,
    ) -> Result<Vec<McpTool>, McpToolError> {
        let tools = sqlx::query_as::<_, McpTool>(
            r#"
            SELECT mt.mcp_tool_id, mt.build_id, mt.service, mt.ns, mt.var, mt.name,
                   mt.description, mt.input_schema, mt.output_schema, mt.title,
                   mt.icons, mt.annotations, mt.meta, mt.file, mt.line, mt."column", mt.position
            FROM mcp_tool mt
            INNER JOIN build b ON mt.build_id = b.build_id
            INNER JOIN project p ON b.project_id = p.project_id
            WHERE p.env_id = $1
              AND p.active = true
              AND b.deployed = true
              AND b.runtime_status = 'ready'
              AND mt.service = $2
            ORDER BY mt.name
            "#,
        )
        .bind(env_id)
        .bind(service)
        .fetch_all(db)
        .await?;
        Ok(tools)
    }

    /// Get MCP tool by environment, service, and tool name
    /// Used for MCP tools/call validation
    pub async fn get_mcp_tool_by_env_service_and_name(
        db: &crate::db::DatabasePool,
        env_id: &Uuid,
        service: &str,
        name: &str,
    ) -> Result<McpTool, McpToolError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                Self::get_mcp_tool_by_env_service_and_name_postgres(pg_pool, env_id, service, name)
                    .await
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                Self::get_mcp_tool_by_env_service_and_name_sqlite(
                    sqlite_pool,
                    env_id,
                    service,
                    name,
                )
                .await
            }
        }
    }

    async fn get_mcp_tool_by_env_service_and_name_sqlite(
        db: &Pool<Sqlite>,
        env_id: &Uuid,
        service: &str,
        name: &str,
    ) -> Result<McpTool, McpToolError> {
        let tool = sqlx::query_as::<_, McpTool>(
            r#"
            SELECT mt.mcp_tool_id, mt.build_id, mt.service, mt.ns, mt.var, mt.name,
                   mt.description, mt.input_schema, mt.output_schema, mt.title,
                   mt.icons, mt.annotations, mt.meta, mt.file, mt.line, mt."column", mt.position
            FROM mcp_tool mt
            INNER JOIN build b ON mt.build_id = b.build_id
            INNER JOIN project p ON b.project_id = p.project_id
            WHERE p.env_id = ?
              AND p.active = 1
              AND b.deployed = 1
              AND b.runtime_status = 'ready'
              AND mt.service = ?
              AND mt.name = ?
            LIMIT 1
            "#,
        )
        .bind(env_id)
        .bind(service)
        .bind(name)
        .fetch_optional(db)
        .await?
        .ok_or(McpToolError::NotFound)?;
        Ok(tool)
    }

    async fn get_mcp_tool_by_env_service_and_name_postgres(
        db: &Pool<Postgres>,
        env_id: &Uuid,
        service: &str,
        name: &str,
    ) -> Result<McpTool, McpToolError> {
        let tool = sqlx::query_as::<_, McpTool>(
            r#"
            SELECT mt.mcp_tool_id, mt.build_id, mt.service, mt.ns, mt.var, mt.name,
                   mt.description, mt.input_schema, mt.output_schema, mt.title,
                   mt.icons, mt.annotations, mt.meta, mt.file, mt.line, mt."column", mt.position
            FROM mcp_tool mt
            INNER JOIN build b ON mt.build_id = b.build_id
            INNER JOIN project p ON b.project_id = p.project_id
            WHERE p.env_id = $1
              AND p.active = true
              AND b.deployed = true
              AND b.runtime_status = 'ready'
              AND mt.service = $2
              AND mt.name = $3
            LIMIT 1
            "#,
        )
        .bind(env_id)
        .bind(service)
        .bind(name)
        .fetch_optional(db)
        .await?
        .ok_or(McpToolError::NotFound)?;
        Ok(tool)
    }

    /// Insert a new MCP tool
    #[allow(clippy::too_many_arguments)]
    pub async fn insert_mcp_tool(
        db: &crate::db::DatabasePool,
        mcp_tool_id: &Uuid,
        build_id: &Uuid,
        service: &str,
        ns: &str,
        var: &str,
        name: &str,
        description: Option<&str>,
        input_schema: Option<&JsonValue>,
        output_schema: Option<&JsonValue>,
        title: Option<&str>,
        icons: Option<&JsonValue>,
        annotations: Option<&JsonValue>,
        meta: Option<&JsonValue>,
        file: Option<&str>,
        line: Option<i32>,
        column: Option<i32>,
        position: Option<i32>,
    ) -> Result<(), McpToolError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                Self::insert_mcp_tool_postgres(
                    pg_pool,
                    mcp_tool_id,
                    build_id,
                    service,
                    ns,
                    var,
                    name,
                    description,
                    input_schema,
                    output_schema,
                    title,
                    icons,
                    annotations,
                    meta,
                    file,
                    line,
                    column,
                    position,
                )
                .await
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                Self::insert_mcp_tool_sqlite(
                    sqlite_pool,
                    mcp_tool_id,
                    build_id,
                    service,
                    ns,
                    var,
                    name,
                    description,
                    input_schema,
                    output_schema,
                    title,
                    icons,
                    annotations,
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

    #[allow(clippy::too_many_arguments)]
    async fn insert_mcp_tool_sqlite(
        db: &Pool<Sqlite>,
        mcp_tool_id: &Uuid,
        build_id: &Uuid,
        service: &str,
        ns: &str,
        var: &str,
        name: &str,
        description: Option<&str>,
        input_schema: Option<&JsonValue>,
        output_schema: Option<&JsonValue>,
        title: Option<&str>,
        icons: Option<&JsonValue>,
        annotations: Option<&JsonValue>,
        meta: Option<&JsonValue>,
        file: Option<&str>,
        line: Option<i32>,
        column: Option<i32>,
        position: Option<i32>,
    ) -> Result<(), McpToolError> {
        let input_schema_json = input_schema
            .map(serde_json::to_string)
            .transpose()
            .map_err(|e| McpToolError::SerializationError(e.to_string()))?;
        let output_schema_json = output_schema
            .map(serde_json::to_string)
            .transpose()
            .map_err(|e| McpToolError::SerializationError(e.to_string()))?;
        let icons_json = icons
            .map(serde_json::to_string)
            .transpose()
            .map_err(|e| McpToolError::SerializationError(e.to_string()))?;
        let annotations_json = annotations
            .map(serde_json::to_string)
            .transpose()
            .map_err(|e| McpToolError::SerializationError(e.to_string()))?;
        let meta_json = meta
            .map(serde_json::to_string)
            .transpose()
            .map_err(|e| McpToolError::SerializationError(e.to_string()))?;

        sqlx::query(
            r#"INSERT OR IGNORE INTO mcp_tool 
               (mcp_tool_id, build_id, service, ns, var, name, description, 
                input_schema, output_schema, title, icons, annotations, meta,
                file, line, "column", position) 
               VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"#,
        )
        .bind(mcp_tool_id)
        .bind(build_id)
        .bind(service)
        .bind(ns)
        .bind(var)
        .bind(name)
        .bind(description)
        .bind(input_schema_json)
        .bind(output_schema_json)
        .bind(title)
        .bind(icons_json)
        .bind(annotations_json)
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
    async fn insert_mcp_tool_postgres(
        db: &Pool<Postgres>,
        mcp_tool_id: &Uuid,
        build_id: &Uuid,
        service: &str,
        ns: &str,
        var: &str,
        name: &str,
        description: Option<&str>,
        input_schema: Option<&JsonValue>,
        output_schema: Option<&JsonValue>,
        title: Option<&str>,
        icons: Option<&JsonValue>,
        annotations: Option<&JsonValue>,
        meta: Option<&JsonValue>,
        file: Option<&str>,
        line: Option<i32>,
        column: Option<i32>,
        position: Option<i32>,
    ) -> Result<(), McpToolError> {
        sqlx::query(
            r#"INSERT INTO mcp_tool 
               (mcp_tool_id, build_id, service, ns, var, name, description,
                input_schema, output_schema, title, icons, annotations, meta,
                file, line, "column", position)
               VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17)
               ON CONFLICT (build_id, service, name) DO NOTHING"#,
        )
        .bind(mcp_tool_id)
        .bind(build_id)
        .bind(service)
        .bind(ns)
        .bind(var)
        .bind(name)
        .bind(description)
        .bind(input_schema)
        .bind(output_schema)
        .bind(title)
        .bind(icons)
        .bind(annotations)
        .bind(meta)
        .bind(file)
        .bind(line)
        .bind(column)
        .bind(position)
        .execute(db)
        .await?;
        Ok(())
    }

    /// Delete MCP tools by build ID
    pub async fn delete_mcp_tools_by_build(
        db: &crate::db::DatabasePool,
        build_id: &Uuid,
    ) -> Result<u64, McpToolError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let result = sqlx::query("DELETE FROM mcp_tool WHERE build_id = $1")
                    .bind(build_id)
                    .execute(pg_pool)
                    .await?;
                if result.rows_affected() > 0 {
                    tracing::debug!(
                        "Deleted {} MCP tool(s) for build {}",
                        result.rows_affected(),
                        build_id
                    );
                }
                Ok(result.rows_affected())
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let result = sqlx::query("DELETE FROM mcp_tool WHERE build_id = ?")
                    .bind(build_id)
                    .execute(sqlite_pool)
                    .await?;
                if result.rows_affected() > 0 {
                    tracing::debug!(
                        "Deleted {} MCP tool(s) for build {}",
                        result.rows_affected(),
                        build_id
                    );
                }
                Ok(result.rows_affected())
            }
        }
    }

    /// Get MCP tools for deployed builds in a specific environment
    pub async fn get_mcp_tools_by_env_deployed(
        db: &crate::db::DatabasePool,
        env_id: &Uuid,
        limit: Option<i64>,
        offset: Option<i64>,
    ) -> Result<Vec<McpToolWithProject>, McpToolError> {
        let limit = limit.unwrap_or(100);
        let offset = offset.unwrap_or(0);

        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let tools = sqlx::query_as::<_, McpToolWithProject>(
                    r#"SELECT mt.mcp_tool_id, mt.build_id, mt.service, mt.ns, mt.var, mt.name,
                              mt.description, mt.input_schema, mt.output_schema, mt.title,
                              mt.icons, mt.annotations, mt.meta, mt.file, mt.line, mt."column", mt.position,
                              p.project_id, p.name as project_name
                       FROM mcp_tool mt
                       JOIN build b ON mt.build_id = b.build_id
                       JOIN project p ON b.project_id = p.project_id
                       WHERE b.deployed = true AND b.runtime_status = 'ready' AND b.active = true AND p.env_id = $1
                       ORDER BY mt.service, mt.name
                       LIMIT $2 OFFSET $3"#
                )
                .bind(env_id)
                .bind(limit)
                .bind(offset)
                .fetch_all(pg_pool)
                .await?;
                Ok(tools)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let tools = sqlx::query_as::<_, McpToolWithProject>(
                    r#"SELECT mt.mcp_tool_id, mt.build_id, mt.service, mt.ns, mt.var, mt.name,
                              mt.description, mt.input_schema, mt.output_schema, mt.title,
                              mt.icons, mt.annotations, mt.meta, mt.file, mt.line, mt."column", mt.position,
                              p.project_id, p.name as project_name
                       FROM mcp_tool mt
                       JOIN build b ON mt.build_id = b.build_id
                       JOIN project p ON b.project_id = p.project_id
                       WHERE b.deployed = 1 AND b.runtime_status = 'ready' AND b.active = 1 AND p.env_id = ?
                       ORDER BY mt.service, mt.name
                       LIMIT ? OFFSET ?"#
                )
                .bind(env_id)
                .bind(limit)
                .bind(offset)
                .fetch_all(sqlite_pool)
                .await?;
                Ok(tools)
            }
        }
    }

    /// Get count of MCP tools by build ID
    pub async fn get_count_by_build(
        db: &crate::db::DatabasePool,
        build_id: &Uuid,
    ) -> Result<i64, McpToolError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let count = sqlx::query_scalar::<_, i64>(
                    "SELECT COUNT(*) FROM mcp_tool WHERE build_id = $1",
                )
                .bind(build_id)
                .fetch_one(pg_pool)
                .await?;
                Ok(count)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let count = sqlx::query_scalar::<_, i64>(
                    "SELECT COUNT(*) FROM mcp_tool WHERE build_id = ?",
                )
                .bind(build_id)
                .fetch_one(sqlite_pool)
                .await?;
                Ok(count)
            }
        }
    }

    /// Get list of distinct services in an environment
    pub async fn get_services_by_env(
        db: &crate::db::DatabasePool,
        env_id: &Uuid,
    ) -> Result<Vec<String>, McpToolError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let services = sqlx::query_scalar::<_, String>(
                    r#"SELECT DISTINCT mt.service
                       FROM mcp_tool mt
                       JOIN build b ON mt.build_id = b.build_id
                       JOIN project p ON b.project_id = p.project_id
                       WHERE p.env_id = $1 AND p.active = true AND b.deployed = true AND b.runtime_status = 'ready'
                       ORDER BY mt.service"#,
                )
                .bind(env_id)
                .fetch_all(pg_pool)
                .await?;
                Ok(services)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let services = sqlx::query_scalar::<_, String>(
                    r#"SELECT DISTINCT mt.service
                       FROM mcp_tool mt
                       JOIN build b ON mt.build_id = b.build_id
                       JOIN project p ON b.project_id = p.project_id
                       WHERE p.env_id = ? AND p.active = 1 AND b.deployed = 1 AND b.runtime_status = 'ready'
                       ORDER BY mt.service"#,
                )
                .bind(env_id)
                .fetch_all(sqlite_pool)
                .await?;
                Ok(services)
            }
        }
    }

    /// Get service summaries (name, tool count, contributing projects) for an environment
    pub async fn get_service_summaries_by_env(
        db: &crate::db::DatabasePool,
        env_id: &Uuid,
    ) -> Result<Vec<McpServiceSummary>, McpToolError> {
        // We query service + project pairs with tool counts, then aggregate in Rust
        // to build the Vec<String> of project names per service.
        #[derive(FromRow)]
        struct ServiceProjectRow {
            service: String,
            project_name: String,
            tool_count: i64,
        }

        let rows: Vec<ServiceProjectRow> = match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                sqlx::query_as::<_, ServiceProjectRow>(
                    r#"SELECT mt.service, p.name as project_name, COUNT(*) as tool_count
                       FROM mcp_tool mt
                       JOIN build b ON mt.build_id = b.build_id
                       JOIN project p ON b.project_id = p.project_id
                       WHERE p.env_id = $1 AND p.active = true AND b.deployed = true AND b.runtime_status = 'ready'
                       GROUP BY mt.service, p.name
                       ORDER BY mt.service, p.name"#,
                )
                .bind(env_id)
                .fetch_all(pg_pool)
                .await?
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                sqlx::query_as::<_, ServiceProjectRow>(
                    r#"SELECT mt.service, p.name as project_name, COUNT(*) as tool_count
                       FROM mcp_tool mt
                       JOIN build b ON mt.build_id = b.build_id
                       JOIN project p ON b.project_id = p.project_id
                       WHERE p.env_id = ? AND p.active = 1 AND b.deployed = 1 AND b.runtime_status = 'ready'
                       GROUP BY mt.service, p.name
                       ORDER BY mt.service, p.name"#,
                )
                .bind(env_id)
                .fetch_all(sqlite_pool)
                .await?
            }
        };

        // Aggregate rows by service
        let mut summaries: Vec<McpServiceSummary> = Vec::new();
        for row in rows {
            if let Some(last) = summaries.last_mut()
                && last.service == row.service
            {
                last.tool_count += row.tool_count;
                last.projects.push(row.project_name);
                continue;
            }
            summaries.push(McpServiceSummary {
                service: row.service,
                tool_count: row.tool_count,
                projects: vec![row.project_name],
            });
        }

        Ok(summaries)
    }

    /// Insert a single MCP tool from a Val map, resolving meta references.
    /// Used by both the local batch path and the remote manifest path.
    pub async fn insert_mcp_tool_from_val(
        db: &crate::db::DatabasePool,
        build_id: &Uuid,
        service: &str,
        tool_val: &crate::val::Val,
    ) -> Result<(), McpToolError> {
        use crate::val::Val;

        let tool_map = match tool_val {
            Val::Map(map) => map,
            _ => {
                return Err(McpToolError::SerializationError(
                    "MCP tool is not a map".to_string(),
                ));
            }
        };

        let fn_name = tool_map
            .get(&Val::from("fn"))
            .and_then(|v| match v {
                Val::Str(s) => Some((**s).to_owned()),
                _ => None,
            })
            .unwrap_or_default();

        let (ns, var) = fn_name
            .rsplit_once('/')
            .map(|(ns, var)| (ns.to_string(), var.to_string()))
            .unwrap_or_default();

        let name = tool_map
            .get(&Val::from("name"))
            .and_then(|v| match v {
                Val::Str(s) => Some((**s).to_owned()),
                _ => None,
            })
            .unwrap_or_else(|| var.clone());

        let description = tool_map
            .get(&Val::from("description"))
            .and_then(|v| match v {
                Val::Str(s) => Some((**s).to_owned()),
                _ => None,
            });

        let input_schema = tool_map
            .get(&Val::from("input_schema"))
            .and_then(|v| serde_json::to_value(v).ok());

        let output_schema = tool_map
            .get(&Val::from("output_schema"))
            .and_then(|v| serde_json::to_value(v).ok());

        let title = tool_map.get(&Val::from("title")).and_then(|v| match v {
            Val::Str(s) => Some((**s).to_owned()),
            _ => None,
        });

        let icons = tool_map
            .get(&Val::from("icons"))
            .and_then(|v| serde_json::to_value(v).ok());

        let annotations = tool_map
            .get(&Val::from("annotations"))
            .and_then(|v| serde_json::to_value(v).ok());

        let raw_meta = tool_map
            .get(&Val::from("meta"))
            .map(crate::db::resolve_meta_val)
            .and_then(|v| serde_json::to_value(&v).ok());

        let top_level_auth_mode = tool_map.get(&Val::from("auth_mode")).and_then(|v| match v {
            Val::Str(s) => Some(s.as_ref()),
            _ => None,
        });
        let normalized_auth_mode = normalize_auth_mode(top_level_auth_mode.or_else(|| {
            raw_meta
                .as_ref()
                .and_then(|m| m.get("mcp"))
                .and_then(|mcp| mcp.get("auth"))
                .and_then(|a| a.as_str())
        }));
        let meta = with_normalized_auth_mode(raw_meta, normalized_auth_mode);

        let file = tool_map.get(&Val::from("file")).and_then(|v| match v {
            Val::Str(s) => Some((**s).to_owned()),
            Val::Null => None,
            _ => None,
        });

        let line = tool_map.get(&Val::from("line")).and_then(|v| match v {
            Val::Int(i) => Some(*i as i32),
            Val::Null => None,
            _ => None,
        });

        let column = tool_map.get(&Val::from("column")).and_then(|v| match v {
            Val::Int(i) => Some(*i as i32),
            Val::Null => None,
            _ => None,
        });

        let position = tool_map.get(&Val::from("position")).and_then(|v| match v {
            Val::Int(i) => Some(*i as i32),
            Val::Null => None,
            _ => None,
        });

        let mcp_tool_id = Uuid::now_v7();
        Self::insert_mcp_tool(
            db,
            &mcp_tool_id,
            build_id,
            service,
            &ns,
            &var,
            &name,
            description.as_deref(),
            input_schema.as_ref(),
            output_schema.as_ref(),
            title.as_deref(),
            icons.as_ref(),
            annotations.as_ref(),
            meta.as_ref(),
            file.as_deref(),
            line,
            column,
            position,
        )
        .await
    }

    /// Insert multiple MCP tools for a build from compiler output
    pub async fn insert_mcp_tools_for_build(
        db: &crate::db::DatabasePool,
        build_id: &Uuid,
        mcp_tools: &crate::lang::compiler::McpTools,
    ) -> Result<(), McpToolError> {
        for (service, tools) in mcp_tools {
            for tool in tools {
                Self::insert_mcp_tool_from_val(db, build_id, service, &tool.mcp_tool).await?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool_with_meta(meta: Option<JsonValue>) -> McpTool {
        McpTool {
            mcp_tool_id: Uuid::now_v7(),
            build_id: Uuid::now_v7(),
            service: "svc".to_string(),
            ns: "::test".to_string(),
            var: "tool".to_string(),
            name: "tool".to_string(),
            description: None,
            input_schema: None,
            output_schema: None,
            title: None,
            icons: None,
            annotations: None,
            meta,
            file: None,
            line: None,
            column: None,
            position: None,
        }
    }

    #[test]
    fn auth_mode_allows_only_exact_none_to_be_public() {
        let public = tool_with_meta(Some(serde_json::json!({
            "mcp": {"auth": "none"}
        })));
        assert_eq!(public.auth_mode(), "none");
        assert!(public.is_public());

        for value in ["required", "optional", "Required", ""] {
            let tool = tool_with_meta(Some(serde_json::json!({
                "mcp": {"auth": value}
            })));
            assert_eq!(tool.auth_mode(), "required");
            assert!(!tool.is_public());
        }

        let missing = tool_with_meta(None);
        assert_eq!(missing.auth_mode(), "required");
        assert!(!missing.is_public());
    }

    #[test]
    fn normalized_auth_mode_overrides_raw_meta_on_insert_path() {
        let meta = Some(serde_json::json!({
            "mcp": {"auth": "optional"},
            "other": true
        }));
        let normalized = with_normalized_auth_mode(meta, "required").unwrap();

        assert_eq!(normalized["mcp"]["auth"], "required");
        assert_eq!(normalized["other"], true);
    }
}

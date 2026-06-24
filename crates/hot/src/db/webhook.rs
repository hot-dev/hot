use serde_json::Value as JsonValue;
use sqlx::{FromRow, Pool, Postgres, Sqlite};
use thiserror::Error;
use uuid::Uuid;

#[derive(Error, Debug)]
pub enum WebhookError {
    #[error("Database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("Webhook not found")]
    NotFound,
    #[error("Serialization error: {0}")]
    SerializationError(String),
}

#[derive(Debug, Clone, FromRow)]
pub struct Webhook {
    pub webhook_id: Uuid,
    pub build_id: Uuid,
    pub service: String,
    pub path: String,
    pub method: String,
    pub ns: String,
    pub var: String,
    pub name: String,
    pub description: Option<String>,
    pub meta: Option<JsonValue>,
    pub file: Option<String>,
    pub line: Option<i32>,
    pub column: Option<i32>,
    pub position: Option<i32>,
}

impl Webhook {
    /// Extract auth_mode from the meta JSON. Defaults to "none" for webhooks.
    pub fn auth_mode(&self) -> &str {
        webhook_auth_mode_from_meta(self.meta.as_ref())
    }

    /// Extract user-declared secret header names from top-level `meta.secret-headers`.
    pub fn secret_headers(&self) -> Vec<String> {
        secret_headers_from_meta(self.meta.as_ref())
    }
}

/// Webhook with project information for display purposes
#[derive(Debug, FromRow)]
pub struct WebhookWithProject {
    pub webhook_id: Uuid,
    pub build_id: Uuid,
    pub service: String,
    pub path: String,
    pub method: String,
    pub ns: String,
    pub var: String,
    pub name: String,
    pub description: Option<String>,
    pub meta: Option<JsonValue>,
    pub file: Option<String>,
    pub line: Option<i32>,
    pub column: Option<i32>,
    pub position: Option<i32>,
    pub project_id: Uuid,
    pub project_name: String,
}

/// Extract auth_mode from webhook meta JSON. Defaults to "none".
fn webhook_auth_mode_from_meta(meta: Option<&JsonValue>) -> &str {
    meta.and_then(|m| m.get("webhook"))
        .and_then(|w| w.get("auth"))
        .and_then(|a| a.as_str())
        .unwrap_or("none")
}

/// Extract user-declared secret header names from top-level `meta.secret-headers`.
fn secret_headers_from_meta(meta: Option<&JsonValue>) -> Vec<String> {
    meta.and_then(|m| m.get("secret-headers"))
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_lowercase()))
                .collect()
        })
        .unwrap_or_default()
}

impl WebhookWithProject {
    /// Extract auth_mode from the meta JSON. Defaults to "none" for webhooks.
    pub fn auth_mode(&self) -> &str {
        webhook_auth_mode_from_meta(self.meta.as_ref())
    }
}

impl Webhook {
    /// Get the short token for URL obscurity (last 12 hex chars of webhook_id, no hyphens).
    /// This matches the `get_uuid_short` helper used in the UI.
    pub fn token(&self) -> String {
        uuid_short(&self.webhook_id)
    }

    /// Get webhook by ID
    pub async fn get_webhook(
        db: &crate::db::DatabasePool,
        webhook_id: &Uuid,
    ) -> Result<Webhook, WebhookError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                Self::get_webhook_postgres(pg_pool, webhook_id).await
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                Self::get_webhook_sqlite(sqlite_pool, webhook_id).await
            }
        }
    }

    async fn get_webhook_sqlite(
        db: &Pool<Sqlite>,
        webhook_id: &Uuid,
    ) -> Result<Webhook, WebhookError> {
        let endpoint = sqlx::query_as::<_, Webhook>(
            r#"SELECT webhook_id, build_id, service, path, method, ns, var, name,
                      description, meta, file, line, "column", position
               FROM webhook WHERE webhook_id = ?"#,
        )
        .bind(webhook_id)
        .fetch_one(db)
        .await?;
        Ok(endpoint)
    }

    async fn get_webhook_postgres(
        db: &Pool<Postgres>,
        webhook_id: &Uuid,
    ) -> Result<Webhook, WebhookError> {
        let endpoint = sqlx::query_as::<_, Webhook>(
            r#"SELECT webhook_id, build_id, service, path, method, ns, var, name,
                      description, meta, file, line, "column", position
               FROM webhook WHERE webhook_id = $1"#,
        )
        .bind(webhook_id)
        .fetch_one(db)
        .await?;
        Ok(endpoint)
    }

    /// Get webhook by environment, service, path, and method
    /// This is the primary lookup for routing incoming webhook requests
    pub async fn get_by_env_service_path_method(
        db: &crate::db::DatabasePool,
        env_id: &Uuid,
        service: &str,
        path: &str,
        method: &str,
    ) -> Result<Webhook, WebhookError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                Self::get_by_env_service_path_method_postgres(
                    pg_pool, env_id, service, path, method,
                )
                .await
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                Self::get_by_env_service_path_method_sqlite(
                    sqlite_pool,
                    env_id,
                    service,
                    path,
                    method,
                )
                .await
            }
        }
    }

    async fn get_by_env_service_path_method_sqlite(
        db: &Pool<Sqlite>,
        env_id: &Uuid,
        service: &str,
        path: &str,
        method: &str,
    ) -> Result<Webhook, WebhookError> {
        let endpoint = sqlx::query_as::<_, Webhook>(
            r#"
            SELECT we.webhook_id, we.build_id, we.service, we.path, we.method,
                   we.ns, we.var, we.name, we.description, we.meta,
                   we.file, we.line, we."column", we.position
            FROM webhook we
            INNER JOIN build b ON we.build_id = b.build_id
            INNER JOIN project p ON b.project_id = p.project_id
            WHERE p.env_id = ?
              AND p.active = 1
              AND b.deployed = 1
              AND b.runtime_status = 'ready'
              AND we.service = ?
              AND we.path = ?
              AND we.method = ?
            LIMIT 1
            "#,
        )
        .bind(env_id)
        .bind(service)
        .bind(path)
        .bind(method)
        .fetch_optional(db)
        .await?
        .ok_or(WebhookError::NotFound)?;
        Ok(endpoint)
    }

    async fn get_by_env_service_path_method_postgres(
        db: &Pool<Postgres>,
        env_id: &Uuid,
        service: &str,
        path: &str,
        method: &str,
    ) -> Result<Webhook, WebhookError> {
        let endpoint = sqlx::query_as::<_, Webhook>(
            r#"
            SELECT we.webhook_id, we.build_id, we.service, we.path, we.method,
                   we.ns, we.var, we.name, we.description, we.meta,
                   we.file, we.line, we."column", we.position
            FROM webhook we
            INNER JOIN build b ON we.build_id = b.build_id
            INNER JOIN project p ON b.project_id = p.project_id
            WHERE p.env_id = $1
              AND p.active = true
              AND b.deployed = true
              AND b.runtime_status = 'ready'
              AND we.service = $2
              AND we.path = $3
              AND we.method = $4
            LIMIT 1
            "#,
        )
        .bind(env_id)
        .bind(service)
        .bind(path)
        .bind(method)
        .fetch_optional(db)
        .await?
        .ok_or(WebhookError::NotFound)?;
        Ok(endpoint)
    }

    /// Get webhooks by environment and service
    pub async fn get_by_env_and_service(
        db: &crate::db::DatabasePool,
        env_id: &Uuid,
        service: &str,
    ) -> Result<Vec<Webhook>, WebhookError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                Self::get_by_env_and_service_postgres(pg_pool, env_id, service).await
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                Self::get_by_env_and_service_sqlite(sqlite_pool, env_id, service).await
            }
        }
    }

    async fn get_by_env_and_service_sqlite(
        db: &Pool<Sqlite>,
        env_id: &Uuid,
        service: &str,
    ) -> Result<Vec<Webhook>, WebhookError> {
        let endpoints = sqlx::query_as::<_, Webhook>(
            r#"
            SELECT we.webhook_id, we.build_id, we.service, we.path, we.method,
                   we.ns, we.var, we.name, we.description, we.meta,
                   we.file, we.line, we."column", we.position
            FROM webhook we
            INNER JOIN build b ON we.build_id = b.build_id
            INNER JOIN project p ON b.project_id = p.project_id
            WHERE p.env_id = ?
              AND p.active = 1
              AND b.deployed = 1
              AND b.runtime_status = 'ready'
              AND we.service = ?
            ORDER BY we.path
            "#,
        )
        .bind(env_id)
        .bind(service)
        .fetch_all(db)
        .await?;
        Ok(endpoints)
    }

    async fn get_by_env_and_service_postgres(
        db: &Pool<Postgres>,
        env_id: &Uuid,
        service: &str,
    ) -> Result<Vec<Webhook>, WebhookError> {
        let endpoints = sqlx::query_as::<_, Webhook>(
            r#"
            SELECT we.webhook_id, we.build_id, we.service, we.path, we.method,
                   we.ns, we.var, we.name, we.description, we.meta,
                   we.file, we.line, we."column", we.position
            FROM webhook we
            INNER JOIN build b ON we.build_id = b.build_id
            INNER JOIN project p ON b.project_id = p.project_id
            WHERE p.env_id = $1
              AND p.active = true
              AND b.deployed = true
              AND b.runtime_status = 'ready'
              AND we.service = $2
            ORDER BY we.path
            "#,
        )
        .bind(env_id)
        .bind(service)
        .fetch_all(db)
        .await?;
        Ok(endpoints)
    }

    /// Get all webhooks for deployed builds in an environment
    pub async fn get_by_env(
        db: &crate::db::DatabasePool,
        env_id: &Uuid,
    ) -> Result<Vec<WebhookWithProject>, WebhookError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                Self::get_by_env_postgres(pg_pool, env_id).await
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                Self::get_by_env_sqlite(sqlite_pool, env_id).await
            }
        }
    }

    async fn get_by_env_sqlite(
        db: &Pool<Sqlite>,
        env_id: &Uuid,
    ) -> Result<Vec<WebhookWithProject>, WebhookError> {
        let endpoints = sqlx::query_as::<_, WebhookWithProject>(
            r#"SELECT we.webhook_id, we.build_id, we.service, we.path, we.method,
                      we.ns, we.var, we.name, we.description, we.meta,
                      we.file, we.line, we."column", we.position,
                      p.project_id, p.name as project_name
               FROM webhook we
               JOIN build b ON we.build_id = b.build_id
               JOIN project p ON b.project_id = p.project_id
               WHERE b.deployed = 1 AND b.runtime_status = 'ready' AND b.active = 1 AND p.env_id = ?
               ORDER BY we.service, we.path"#,
        )
        .bind(env_id)
        .fetch_all(db)
        .await?;
        Ok(endpoints)
    }

    async fn get_by_env_postgres(
        db: &Pool<Postgres>,
        env_id: &Uuid,
    ) -> Result<Vec<WebhookWithProject>, WebhookError> {
        let endpoints = sqlx::query_as::<_, WebhookWithProject>(
            r#"SELECT we.webhook_id, we.build_id, we.service, we.path, we.method,
                      we.ns, we.var, we.name, we.description, we.meta,
                      we.file, we.line, we."column", we.position,
                      p.project_id, p.name as project_name
               FROM webhook we
               JOIN build b ON we.build_id = b.build_id
               JOIN project p ON b.project_id = p.project_id
               WHERE b.deployed = true AND b.runtime_status = 'ready' AND b.active = true AND p.env_id = $1
               ORDER BY we.service, we.path"#,
        )
        .bind(env_id)
        .fetch_all(db)
        .await?;
        Ok(endpoints)
    }

    /// Insert a new webhook
    #[allow(clippy::too_many_arguments)]
    pub async fn insert_webhook(
        db: &crate::db::DatabasePool,
        webhook_id: &Uuid,
        build_id: &Uuid,
        service: &str,
        path: &str,
        method: &str,
        ns: &str,
        var: &str,
        name: &str,
        description: Option<&str>,
        meta: Option<&JsonValue>,
        file: Option<&str>,
        line: Option<i32>,
        column: Option<i32>,
        position: Option<i32>,
    ) -> Result<(), WebhookError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                Self::insert_webhook_postgres(
                    pg_pool,
                    webhook_id,
                    build_id,
                    service,
                    path,
                    method,
                    ns,
                    var,
                    name,
                    description,
                    meta,
                    file,
                    line,
                    column,
                    position,
                )
                .await
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                Self::insert_webhook_sqlite(
                    sqlite_pool,
                    webhook_id,
                    build_id,
                    service,
                    path,
                    method,
                    ns,
                    var,
                    name,
                    description,
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
    async fn insert_webhook_sqlite(
        db: &Pool<Sqlite>,
        webhook_id: &Uuid,
        build_id: &Uuid,
        service: &str,
        path: &str,
        method: &str,
        ns: &str,
        var: &str,
        name: &str,
        description: Option<&str>,
        meta: Option<&JsonValue>,
        file: Option<&str>,
        line: Option<i32>,
        column: Option<i32>,
        position: Option<i32>,
    ) -> Result<(), WebhookError> {
        let meta_json = meta
            .map(serde_json::to_string)
            .transpose()
            .map_err(|e| WebhookError::SerializationError(e.to_string()))?;

        sqlx::query(
            r#"INSERT INTO webhook
               (webhook_id, build_id, service, path, method, ns, var, name,
                description, meta, file, line, "column", position)
               VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
               ON CONFLICT (webhook_id) DO UPDATE SET
                build_id = excluded.build_id,
                service = excluded.service,
                path = excluded.path,
                method = excluded.method,
                ns = excluded.ns,
                var = excluded.var,
                name = excluded.name,
                description = excluded.description,
                meta = excluded.meta,
                file = excluded.file,
                line = excluded.line,
                "column" = excluded."column",
                position = excluded.position"#,
        )
        .bind(webhook_id)
        .bind(build_id)
        .bind(service)
        .bind(path)
        .bind(method)
        .bind(ns)
        .bind(var)
        .bind(name)
        .bind(description)
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
    async fn insert_webhook_postgres(
        db: &Pool<Postgres>,
        webhook_id: &Uuid,
        build_id: &Uuid,
        service: &str,
        path: &str,
        method: &str,
        ns: &str,
        var: &str,
        name: &str,
        description: Option<&str>,
        meta: Option<&JsonValue>,
        file: Option<&str>,
        line: Option<i32>,
        column: Option<i32>,
        position: Option<i32>,
    ) -> Result<(), WebhookError> {
        sqlx::query(
            r#"INSERT INTO webhook
               (webhook_id, build_id, service, path, method, ns, var, name,
                description, meta, file, line, "column", position)
               VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14)
               ON CONFLICT (webhook_id) DO UPDATE SET
                build_id = EXCLUDED.build_id,
                service = EXCLUDED.service,
                path = EXCLUDED.path,
                method = EXCLUDED.method,
                ns = EXCLUDED.ns,
                var = EXCLUDED.var,
                name = EXCLUDED.name,
                description = EXCLUDED.description,
                meta = EXCLUDED.meta,
                file = EXCLUDED.file,
                line = EXCLUDED.line,
                "column" = EXCLUDED."column",
                position = EXCLUDED.position"#,
        )
        .bind(webhook_id)
        .bind(build_id)
        .bind(service)
        .bind(path)
        .bind(method)
        .bind(ns)
        .bind(var)
        .bind(name)
        .bind(description)
        .bind(meta)
        .bind(file)
        .bind(line)
        .bind(column)
        .bind(position)
        .execute(db)
        .await?;
        Ok(())
    }

    /// Delete webhooks by build ID
    pub async fn delete_by_build(
        db: &crate::db::DatabasePool,
        build_id: &Uuid,
    ) -> Result<u64, WebhookError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let result = sqlx::query("DELETE FROM webhook WHERE build_id = $1")
                    .bind(build_id)
                    .execute(pg_pool)
                    .await?;
                if result.rows_affected() > 0 {
                    tracing::info!(
                        "Deleted {} webhook(s) for build {}",
                        result.rows_affected(),
                        build_id
                    );
                }
                Ok(result.rows_affected())
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let result = sqlx::query("DELETE FROM webhook WHERE build_id = ?")
                    .bind(build_id)
                    .execute(sqlite_pool)
                    .await?;
                if result.rows_affected() > 0 {
                    tracing::info!(
                        "Deleted {} webhook(s) for build {}",
                        result.rows_affected(),
                        build_id
                    );
                }
                Ok(result.rows_affected())
            }
        }
    }

    /// Delete webhooks from previous builds of the same project that are NOT in the current build.
    /// Called after upserting the current build's webhooks to clean up stale entries
    /// (e.g., when a webhook is removed from code).
    pub async fn delete_stale_for_project(
        db: &crate::db::DatabasePool,
        build_id: &Uuid,
    ) -> Result<u64, WebhookError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let result = sqlx::query(
                    r#"DELETE FROM webhook
                       WHERE build_id != $1
                         AND build_id IN (
                           SELECT b.build_id FROM build b
                           JOIN build b2 ON b.project_id = b2.project_id
                           WHERE b2.build_id = $1
                         )"#,
                )
                .bind(build_id)
                .execute(pg_pool)
                .await?;
                Ok(result.rows_affected())
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let result = sqlx::query(
                    r#"DELETE FROM webhook
                       WHERE build_id != ?
                         AND build_id IN (
                           SELECT b.build_id FROM build b
                           JOIN build b2 ON b.project_id = b2.project_id
                           WHERE b2.build_id = ?
                         )"#,
                )
                .bind(build_id)
                .bind(build_id)
                .execute(sqlite_pool)
                .await?;
                Ok(result.rows_affected())
            }
        }
    }

    /// Find an existing webhook_id for the same (service, path, method) in any build
    /// of the same project. This ensures webhook URLs are stable across deploys —
    /// redeploying doesn't change the URL token that external services (Slack, Stripe, etc.) use.
    pub async fn find_existing_webhook_id_for_build(
        db: &crate::db::DatabasePool,
        build_id: &Uuid,
        service: &str,
        path: &str,
        method: &str,
    ) -> Result<Option<Uuid>, WebhookError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                Self::find_existing_webhook_id_postgres(pg_pool, build_id, service, path, method)
                    .await
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                Self::find_existing_webhook_id_sqlite(sqlite_pool, build_id, service, path, method)
                    .await
            }
        }
    }

    async fn find_existing_webhook_id_sqlite(
        db: &Pool<Sqlite>,
        build_id: &Uuid,
        service: &str,
        path: &str,
        method: &str,
    ) -> Result<Option<Uuid>, WebhookError> {
        // Find webhook_id from any previous build of the same project
        let row: Option<(Uuid,)> = sqlx::query_as(
            r#"SELECT we.webhook_id
               FROM webhook we
               JOIN build b ON we.build_id = b.build_id
               JOIN build b2 ON b.project_id = b2.project_id
               WHERE b2.build_id = ?
                 AND we.service = ?
                 AND we.path = ?
                 AND we.method = ?
               ORDER BY we.webhook_id ASC
               LIMIT 1"#,
        )
        .bind(build_id)
        .bind(service)
        .bind(path)
        .bind(method)
        .fetch_optional(db)
        .await?;
        Ok(row.map(|r| r.0))
    }

    async fn find_existing_webhook_id_postgres(
        db: &Pool<Postgres>,
        build_id: &Uuid,
        service: &str,
        path: &str,
        method: &str,
    ) -> Result<Option<Uuid>, WebhookError> {
        // Find webhook_id from any previous build of the same project
        let row: Option<(Uuid,)> = sqlx::query_as(
            r#"SELECT we.webhook_id
               FROM webhook we
               JOIN build b ON we.build_id = b.build_id
               JOIN build b2 ON b.project_id = b2.project_id
               WHERE b2.build_id = $1
                 AND we.service = $2
                 AND we.path = $3
                 AND we.method = $4
               ORDER BY we.webhook_id ASC
               LIMIT 1"#,
        )
        .bind(build_id)
        .bind(service)
        .bind(path)
        .bind(method)
        .fetch_optional(db)
        .await?;
        Ok(row.map(|r| r.0))
    }

    /// Insert a single webhook from a Val map, resolving meta, looking up stable
    /// webhook IDs, and merging send targets.  Used by both the local batch path
    /// and the remote manifest path.
    pub async fn insert_webhook_from_val(
        db: &crate::db::DatabasePool,
        build_id: &Uuid,
        service: &str,
        webhook_val: &crate::val::Val,
        send_targets: &crate::lang::compiler::SendTargets,
    ) -> Result<(), WebhookError> {
        use crate::val::Val;

        let webhook_map = match webhook_val {
            Val::Map(map) => map,
            _ => {
                return Err(WebhookError::SerializationError(
                    "Webhook is not a map".to_string(),
                ));
            }
        };

        let fn_name = webhook_map
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

        let path = webhook_map
            .get(&Val::from("path"))
            .and_then(|v| match v {
                Val::Str(s) => Some((**s).to_owned()),
                _ => None,
            })
            .unwrap_or_default();

        let method = webhook_map
            .get(&Val::from("method"))
            .and_then(|v| match v {
                Val::Str(s) => Some((**s).to_owned()),
                _ => None,
            })
            .unwrap_or_else(|| "POST".to_string());

        let name = webhook_map
            .get(&Val::from("name"))
            .and_then(|v| match v {
                Val::Str(s) => Some((**s).to_owned()),
                _ => None,
            })
            .unwrap_or_else(|| var.clone());

        let description = webhook_map
            .get(&Val::from("description"))
            .and_then(|v| match v {
                Val::Str(s) => Some((**s).to_owned()),
                _ => None,
            });

        let meta = webhook_map
            .get(&Val::from("meta"))
            .map(crate::db::resolve_meta_val)
            .and_then(|v| serde_json::to_value(&v).ok());

        let file = webhook_map.get(&Val::from("file")).and_then(|v| match v {
            Val::Str(s) => Some((**s).to_owned()),
            Val::Null => None,
            _ => None,
        });

        let line = webhook_map.get(&Val::from("line")).and_then(|v| match v {
            Val::Int(i) => Some(*i as i32),
            Val::Null => None,
            _ => None,
        });

        let column = webhook_map.get(&Val::from("column")).and_then(|v| match v {
            Val::Int(i) => Some(*i as i32),
            Val::Null => None,
            _ => None,
        });

        let position = webhook_map
            .get(&Val::from("position"))
            .and_then(|v| match v {
                Val::Int(i) => Some(*i as i32),
                Val::Null => None,
                _ => None,
            });

        let webhook_id =
            match Self::find_existing_webhook_id_for_build(db, build_id, service, &path, &method)
                .await?
            {
                Some(existing_id) => {
                    tracing::debug!(
                        "Reusing webhook_id {} for {}/{} {} (stable URL)",
                        existing_id,
                        service,
                        path,
                        method
                    );
                    existing_id
                }
                None => Uuid::now_v7(),
            };

        let fn_key = format!("{}/{}", ns, var);
        let static_sends: Vec<String> = send_targets
            .get(&fn_key)
            .map(|targets| targets.iter().map(|t| t.event_name.clone()).collect())
            .unwrap_or_default();
        let merged_meta = crate::db::merge_sends_into_meta(meta, &static_sends);

        Self::insert_webhook(
            db,
            &webhook_id,
            build_id,
            service,
            &path,
            &method,
            &ns,
            &var,
            &name,
            description.as_deref(),
            merged_meta.as_ref(),
            file.as_deref(),
            line,
            column,
            position,
        )
        .await
    }

    /// Insert multiple webhooks for a build from compiler output.
    /// Reuses existing webhook_ids for the same (service, path, method) within the
    /// same project so that webhook URLs remain stable across deploys.
    pub async fn insert_webhooks_for_build(
        db: &crate::db::DatabasePool,
        build_id: &Uuid,
        webhooks: &crate::lang::compiler::Webhooks,
        send_targets: &crate::lang::compiler::SendTargets,
    ) -> Result<(), WebhookError> {
        for (_service, endpoints) in webhooks {
            for endpoint in endpoints {
                Self::insert_webhook_from_val(
                    db,
                    build_id,
                    &endpoint.service,
                    &endpoint.webhook,
                    send_targets,
                )
                .await?;
            }
        }
        Ok(())
    }

    /// Get service summaries (name, endpoint count, contributing projects) for an environment
    pub async fn get_service_summaries_by_env(
        db: &crate::db::DatabasePool,
        env_id: &Uuid,
    ) -> Result<Vec<WebhookServiceSummary>, WebhookError> {
        #[derive(FromRow)]
        struct ServiceProjectRow {
            service: String,
            project_name: String,
            endpoint_count: i64,
        }

        let rows: Vec<ServiceProjectRow> = match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                sqlx::query_as::<_, ServiceProjectRow>(
                    r#"SELECT we.service, p.name as project_name, COUNT(*) as endpoint_count
                       FROM webhook we
                       JOIN build b ON we.build_id = b.build_id
                       JOIN project p ON b.project_id = p.project_id
                       WHERE p.env_id = $1 AND p.active = true AND b.deployed = true AND b.runtime_status = 'ready'
                       GROUP BY we.service, p.name
                       ORDER BY we.service, p.name"#,
                )
                .bind(env_id)
                .fetch_all(pg_pool)
                .await?
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                sqlx::query_as::<_, ServiceProjectRow>(
                    r#"SELECT we.service, p.name as project_name, COUNT(*) as endpoint_count
                       FROM webhook we
                       JOIN build b ON we.build_id = b.build_id
                       JOIN project p ON b.project_id = p.project_id
                       WHERE p.env_id = ? AND p.active = 1 AND b.deployed = 1 AND b.runtime_status = 'ready'
                       GROUP BY we.service, p.name
                       ORDER BY we.service, p.name"#,
                )
                .bind(env_id)
                .fetch_all(sqlite_pool)
                .await?
            }
        };

        // Aggregate rows by service
        let mut summaries: Vec<WebhookServiceSummary> = Vec::new();
        for row in rows {
            if let Some(last) = summaries.last_mut()
                && last.service == row.service
            {
                last.endpoint_count += row.endpoint_count;
                last.projects.push(row.project_name);
                continue;
            }
            summaries.push(WebhookServiceSummary {
                service: row.service,
                endpoint_count: row.endpoint_count,
                projects: vec![row.project_name],
            });
        }

        Ok(summaries)
    }
}

/// Summary of a webhook service for list display
#[derive(Debug, Clone)]
pub struct WebhookServiceSummary {
    pub service: String,
    pub endpoint_count: i64,
    pub projects: Vec<String>,
}

/// Get shortened UUID string (last 12 hex characters, no hyphens).
/// Matches `get_uuid_short` in hot_app templates.
pub fn uuid_short(uuid: &Uuid) -> String {
    let hex = uuid.to_string().replace('-', "");
    if hex.len() >= 12 {
        hex[hex.len() - 12..].to_string()
    } else {
        hex
    }
}

/// Validate that a token string matches a webhook's short ID.
pub fn validate_token(webhook_id: &Uuid, token: &str) -> bool {
    uuid_short(webhook_id) == token
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn make_webhook(meta: Option<JsonValue>) -> Webhook {
        Webhook {
            webhook_id: Uuid::nil(),
            build_id: Uuid::nil(),
            service: "test".to_string(),
            path: "/test".to_string(),
            method: "POST".to_string(),
            ns: "::test".to_string(),
            var: "handler".to_string(),
            name: "handler".to_string(),
            description: None,
            meta,
            file: None,
            line: None,
            column: None,
            position: None,
        }
    }

    #[test]
    fn test_secret_headers_from_meta() {
        let wh = make_webhook(Some(json!({
            "webhook": {"service": "test", "path": "/test"},
            "secret-headers": ["stripe-signature", "x-api-key"]
        })));
        assert_eq!(wh.secret_headers(), vec!["stripe-signature", "x-api-key"]);
    }

    #[test]
    fn test_secret_headers_empty_when_absent() {
        let wh = make_webhook(Some(
            json!({"webhook": {"service": "test", "path": "/test"}}),
        ));
        assert!(wh.secret_headers().is_empty());
    }

    #[test]
    fn test_secret_headers_empty_when_no_meta() {
        let wh = make_webhook(None);
        assert!(wh.secret_headers().is_empty());
    }

    #[test]
    fn test_secret_headers_lowercased() {
        let wh = make_webhook(Some(json!({
            "webhook": {"service": "test", "path": "/test"},
            "secret-headers": ["X-Api-Key"]
        })));
        assert_eq!(wh.secret_headers(), vec!["x-api-key"]);
    }

    #[test]
    fn test_auth_mode_defaults_to_none() {
        let wh = make_webhook(Some(
            json!({"webhook": {"service": "test", "path": "/test"}}),
        ));
        assert_eq!(wh.auth_mode(), "none");
    }

    #[test]
    fn test_auth_mode_required() {
        let wh = make_webhook(Some(json!({
            "webhook": {"service": "test", "path": "/test", "auth": "required"}
        })));
        assert_eq!(wh.auth_mode(), "required");
    }
}

//! Access model for per-request audit logging.
//!
//! Records which credential was used, from where, and what was requested.
//! This is an append-only log with no FK constraints, kept indefinitely.
//! Runs and events reference `access_id` for attribution.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, Pool, Postgres, Sqlite};
use std::error::Error;
use std::fmt;
use uuid::Uuid;

// ============================================================================
// Error Type
// ============================================================================

#[derive(Debug)]
pub enum AccessError {
    Database(sqlx::Error),
    NotFound,
}

impl fmt::Display for AccessError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AccessError::Database(e) => write!(f, "Database error: {}", e),
            AccessError::NotFound => write!(f, "Access record not found"),
        }
    }
}

impl Error for AccessError {}

impl From<sqlx::Error> for AccessError {
    fn from(error: sqlx::Error) -> Self {
        AccessError::Database(error)
    }
}

// ============================================================================
// Access Struct
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct Access {
    pub access_id: Uuid,
    pub env_id: Uuid,
    pub api_key_id: Option<Uuid>,
    pub service_key_id: Option<Uuid>,
    pub session_id: Option<Uuid>,
    pub source: String,
    pub ip_address: Option<String>,
    pub user_agent: Option<String>,
    pub host: Option<String>,
    pub method: Option<String>,
    pub path: Option<String>,
    pub query_params: Option<String>,
    pub created_at: DateTime<Utc>,
}

/// Source types for access records.
pub mod source {
    pub const API: &str = "api";
    pub const SCHEDULER: &str = "scheduler";
    pub const SYSTEM: &str = "system";
}

/// Builder for creating access records.
pub struct AccessBuilder {
    env_id: Uuid,
    api_key_id: Option<Uuid>,
    service_key_id: Option<Uuid>,
    session_id: Option<Uuid>,
    source: String,
    ip_address: Option<String>,
    user_agent: Option<String>,
    host: Option<String>,
    method: Option<String>,
    path: Option<String>,
    query_params: Option<String>,
}

impl AccessBuilder {
    pub fn new(env_id: Uuid, source: &str) -> Self {
        Self {
            env_id,
            api_key_id: None,
            service_key_id: None,
            session_id: None,
            source: source.to_string(),
            ip_address: None,
            user_agent: None,
            host: None,
            method: None,
            path: None,
            query_params: None,
        }
    }

    pub fn api_key_id(mut self, id: Uuid) -> Self {
        self.api_key_id = Some(id);
        self
    }

    pub fn service_key_id(mut self, id: Uuid) -> Self {
        self.service_key_id = Some(id);
        self
    }

    pub fn session_id(mut self, id: Uuid) -> Self {
        self.session_id = Some(id);
        self
    }

    pub fn ip_address(mut self, ip: impl Into<String>) -> Self {
        self.ip_address = Some(ip.into());
        self
    }

    pub fn user_agent(mut self, ua: impl Into<String>) -> Self {
        self.user_agent = Some(ua.into());
        self
    }

    pub fn host(mut self, host: impl Into<String>) -> Self {
        self.host = Some(host.into());
        self
    }

    pub fn method(mut self, method: impl Into<String>) -> Self {
        self.method = Some(method.into());
        self
    }

    pub fn path(mut self, path: impl Into<String>) -> Self {
        self.path = Some(path.into());
        self
    }

    pub fn query_params(mut self, params: impl Into<String>) -> Self {
        self.query_params = Some(params.into());
        self
    }

    /// Insert the access record and return it.
    pub async fn insert(self, db: &crate::db::DatabasePool) -> Result<Access, AccessError> {
        Access::create(
            db,
            &self.env_id,
            self.api_key_id.as_ref(),
            self.service_key_id.as_ref(),
            self.session_id.as_ref(),
            &self.source,
            self.ip_address.as_deref(),
            self.user_agent.as_deref(),
            self.host.as_deref(),
            self.method.as_deref(),
            self.path.as_deref(),
            self.query_params.as_deref(),
        )
        .await
    }
}

impl Access {
    // ========================================================================
    // Database operations
    // ========================================================================

    /// Create a new access record.
    #[allow(clippy::too_many_arguments)]
    pub async fn create(
        db: &crate::db::DatabasePool,
        env_id: &Uuid,
        api_key_id: Option<&Uuid>,
        service_key_id: Option<&Uuid>,
        session_id: Option<&Uuid>,
        source: &str,
        ip_address: Option<&str>,
        user_agent: Option<&str>,
        host: Option<&str>,
        method: Option<&str>,
        path: Option<&str>,
        query_params: Option<&str>,
    ) -> Result<Access, AccessError> {
        let access_id = Uuid::now_v7();
        let now = Utc::now();

        match db {
            crate::db::DatabasePool::Sqlite(db) => {
                sqlx::query(
                    r#"
                    INSERT INTO access (access_id, env_id, api_key_id, service_key_id, session_id,
                                        source, ip_address, user_agent, host, method, path, query_params)
                    VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                    "#,
                )
                .bind(access_id)
                .bind(env_id)
                .bind(api_key_id)
                .bind(service_key_id)
                .bind(session_id)
                .bind(source)
                .bind(ip_address)
                .bind(user_agent)
                .bind(host)
                .bind(method)
                .bind(path)
                .bind(query_params)
                .execute(db)
                .await?;
            }
            crate::db::DatabasePool::Postgres(db) => {
                sqlx::query(
                    r#"
                    INSERT INTO access (access_id, env_id, api_key_id, service_key_id, session_id,
                                        source, ip_address, user_agent, host, method, path, query_params)
                    VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)
                    "#,
                )
                .bind(access_id)
                .bind(env_id)
                .bind(api_key_id)
                .bind(service_key_id)
                .bind(session_id)
                .bind(source)
                .bind(ip_address)
                .bind(user_agent)
                .bind(host)
                .bind(method)
                .bind(path)
                .bind(query_params)
                .execute(db)
                .await?;
            }
        }

        Ok(Access {
            access_id,
            env_id: *env_id,
            api_key_id: api_key_id.copied(),
            service_key_id: service_key_id.copied(),
            session_id: session_id.copied(),
            source: source.to_string(),
            ip_address: ip_address.map(|s| s.to_string()),
            user_agent: user_agent.map(|s| s.to_string()),
            host: host.map(|s| s.to_string()),
            method: method.map(|s| s.to_string()),
            path: path.map(|s| s.to_string()),
            query_params: query_params.map(|s| s.to_string()),
            created_at: now,
        })
    }

    /// Convenience: create an access record via builder.
    pub fn builder(env_id: Uuid, source: &str) -> AccessBuilder {
        AccessBuilder::new(env_id, source)
    }

    /// Get an access record by ID.
    pub async fn get_access(
        db: &crate::db::DatabasePool,
        access_id: &Uuid,
    ) -> Result<Access, AccessError> {
        match db {
            crate::db::DatabasePool::Sqlite(db) => Self::get_access_sqlite(db, access_id).await,
            crate::db::DatabasePool::Postgres(db) => Self::get_access_postgres(db, access_id).await,
        }
    }

    async fn get_access_sqlite(db: &Pool<Sqlite>, access_id: &Uuid) -> Result<Access, AccessError> {
        sqlx::query_as::<_, Access>(
            r#"
            SELECT access_id, env_id, api_key_id, service_key_id, session_id,
                   source, ip_address, user_agent, host, method, path, query_params, created_at
            FROM access WHERE access_id = ?
            "#,
        )
        .bind(access_id)
        .fetch_one(db)
        .await
        .map_err(|e| match e {
            sqlx::Error::RowNotFound => AccessError::NotFound,
            other => AccessError::Database(other),
        })
    }

    async fn get_access_postgres(
        db: &Pool<Postgres>,
        access_id: &Uuid,
    ) -> Result<Access, AccessError> {
        sqlx::query_as::<_, Access>(
            r#"
            SELECT access_id, env_id, api_key_id, service_key_id, session_id,
                   source, ip_address, user_agent, host, method, path, query_params, created_at
            FROM access WHERE access_id = $1
            "#,
        )
        .bind(access_id)
        .fetch_one(db)
        .await
        .map_err(|e| match e {
            sqlx::Error::RowNotFound => AccessError::NotFound,
            other => AccessError::Database(other),
        })
    }

    /// List access records for an environment, ordered by most recent first.
    pub async fn list_by_env(
        db: &crate::db::DatabasePool,
        env_id: &Uuid,
        limit: Option<i64>,
        offset: Option<i64>,
    ) -> Result<Vec<Access>, AccessError> {
        let limit = limit.unwrap_or(50);
        let offset = offset.unwrap_or(0);

        match db {
            crate::db::DatabasePool::Sqlite(db) => {
                Ok(sqlx::query_as::<_, Access>(
                    r#"
                    SELECT access_id, env_id, api_key_id, service_key_id, session_id,
                           source, ip_address, user_agent, host, method, path, query_params, created_at
                    FROM access WHERE env_id = ?
                    ORDER BY created_at DESC
                    LIMIT ? OFFSET ?
                    "#,
                )
                .bind(env_id)
                .bind(limit)
                .bind(offset)
                .fetch_all(db)
                .await?)
            }
            crate::db::DatabasePool::Postgres(db) => {
                Ok(sqlx::query_as::<_, Access>(
                    r#"
                    SELECT access_id, env_id, api_key_id, service_key_id, session_id,
                           source, ip_address, user_agent, host, method, path, query_params, created_at
                    FROM access WHERE env_id = $1
                    ORDER BY created_at DESC
                    LIMIT $2 OFFSET $3
                    "#,
                )
                .bind(env_id)
                .bind(limit)
                .bind(offset)
                .fetch_all(db)
                .await?)
            }
        }
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_builder() {
        let env_id = Uuid::now_v7();
        let api_key_id = Uuid::now_v7();

        let builder = Access::builder(env_id, source::API)
            .api_key_id(api_key_id)
            .ip_address("1.2.3.4")
            .user_agent("curl/8.0")
            .host("api.hot.dev")
            .method("POST")
            .path("/v1/mcp/weather/get-forecast")
            .query_params("");

        assert_eq!(builder.env_id, env_id);
        assert_eq!(builder.api_key_id, Some(api_key_id));
        assert_eq!(builder.source, "api");
        assert_eq!(builder.ip_address.as_deref(), Some("1.2.3.4"));
        assert_eq!(builder.method.as_deref(), Some("POST"));
    }
}

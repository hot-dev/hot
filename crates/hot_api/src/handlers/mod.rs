//! API Handlers organized by functional area

mod builds;
mod context;
mod domains;
mod env;
mod event_handlers;
mod events;
mod files;
mod health;
mod mcp;
mod org;
mod projects;
pub(crate) mod request;
mod runs;
mod schedules;
mod service_keys;
mod sessions;
mod streams;
mod webhook;

// Re-export all handlers
pub use builds::*;
pub use context::*;
pub use domains::*;
pub use env::*;
pub use event_handlers::*;
pub use events::*;
pub use files::*;
pub use health::*;
pub use mcp::*;
pub use org::*;
pub use projects::*;
pub use runs::*;
pub use schedules::*;
pub use service_keys::*;
pub use sessions::*;
pub use streams::*;
pub use webhook::*;

// ============================================================================
// Shared Types
// ============================================================================

use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct ListQueryParams {
    #[serde(
        default = "default_limit",
        deserialize_with = "deserialize_clamped_limit"
    )]
    pub limit: i64,
    #[serde(default)]
    pub offset: i64,
}

pub const MAX_LIST_LIMIT: i64 = 500;

fn default_limit() -> i64 {
    20
}

pub fn deserialize_clamped_limit<'de, D>(deserializer: D) -> Result<i64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let v = i64::deserialize(deserializer)?;
    Ok(v.clamp(1, MAX_LIST_LIMIT))
}

// ============================================================================
// Shared Helper Functions
// ============================================================================

use axum::{Json, http::StatusCode};
use hot::db::{DatabasePool, api_key::ApiKey, project::Project};
use uuid::Uuid;

use crate::models::ApiErrorResponse;

/// Helper to get project and verify ownership
pub async fn get_and_verify_project(
    db: &DatabasePool,
    api_key: &ApiKey,
    project_id_or_slug: &str,
) -> Result<Project, (StatusCode, Json<ApiErrorResponse>)> {
    let project = if let Ok(project_id) = Uuid::parse_str(project_id_or_slug) {
        Project::get_project(db, &project_id).await
    } else {
        Project::get_project_by_env_and_name(db, &api_key.env_id, project_id_or_slug)
            .await
            .and_then(|opt| opt.ok_or(hot::db::project::ProjectError::NotFound))
    }
    .map_err(|e| match e {
        hot::db::project::ProjectError::NotFound => (
            StatusCode::NOT_FOUND,
            Json(ApiErrorResponse::not_found("Project")),
        ),
        _ => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ApiErrorResponse::internal_error(&e.to_string())),
        ),
    })?;

    // Verify ownership
    if project.env_id != api_key.env_id {
        return Err((
            StatusCode::NOT_FOUND,
            Json(ApiErrorResponse::not_found("Project")),
        ));
    }

    Ok(project)
}

/// Get org_id for the API key's environment
pub async fn get_org_id_for_env(
    db: &DatabasePool,
    env_id: &Uuid,
) -> Result<Uuid, (StatusCode, Json<ApiErrorResponse>)> {
    let env = hot::db::Env::get_env(db, env_id).await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ApiErrorResponse::internal_error(&e.to_string())),
        )
    })?;
    Ok(env.org_id)
}

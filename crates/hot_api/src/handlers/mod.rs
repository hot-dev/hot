//! API Handlers organized by functional area

mod blobs;
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
pub use blobs::*;
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
use utoipa::ToSchema;

#[derive(Debug, Deserialize, ToSchema)]
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

use crate::auth::AuthContext;
use crate::models::ApiErrorResponse;

/// Require a first-party API key principal for administrative endpoints.
pub fn require_api_key(
    auth: &AuthContext,
    message: &'static str,
) -> Result<(), (StatusCode, Json<ApiErrorResponse>)> {
    if auth.is_api_key() {
        return Ok(());
    }

    Err((
        StatusCode::FORBIDDEN,
        Json(ApiErrorResponse::new("forbidden", message)),
    ))
}

fn project_error_response(
    error: hot::db::project::ProjectError,
) -> (StatusCode, Json<ApiErrorResponse>) {
    match error {
        hot::db::project::ProjectError::NotFound => (
            StatusCode::NOT_FOUND,
            Json(ApiErrorResponse::not_found("Project")),
        ),
        _ => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ApiErrorResponse::internal_error(&error.to_string())),
        ),
    }
}

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
    .map_err(project_error_response)?;

    // Verify ownership
    if project.env_id != api_key.env_id {
        return Err((
            StatusCode::NOT_FOUND,
            Json(ApiErrorResponse::not_found("Project")),
        ));
    }

    Ok(project)
}

/// Helper for write paths that should bring an inactive project back online.
pub async fn get_and_ensure_active_project(
    db: &DatabasePool,
    api_key: &ApiKey,
    project_id_or_slug: &str,
) -> Result<Project, (StatusCode, Json<ApiErrorResponse>)> {
    let project = if let Ok(project_id) = Uuid::parse_str(project_id_or_slug) {
        Project::get_project_including_inactive(db, &project_id).await
    } else {
        Project::get_project_by_env_and_name(db, &api_key.env_id, project_id_or_slug)
            .await
            .and_then(|opt| opt.ok_or(hot::db::project::ProjectError::NotFound))
    }
    .map_err(project_error_response)?;

    if project.env_id != api_key.env_id {
        return Err((
            StatusCode::NOT_FOUND,
            Json(ApiErrorResponse::not_found("Project")),
        ));
    }

    if project.active {
        return Ok(project);
    }

    Project::toggle_active(db, &project.project_id, true, &api_key.created_by_user_id)
        .await
        .map_err(project_error_response)?;

    tracing::info!(
        "Reactivated project {} while handling write request",
        project.project_id
    );

    Project::get_project(db, &project.project_id)
        .await
        .map_err(project_error_response)
}

/// Unwrap the optional blob-store request extension. The layer is absent in
/// some test routers, and the store itself is None when blob spill is
/// disabled; both mean "no rehydration".
pub(crate) fn blob_store_from_ext(
    ext: Option<axum::Extension<Option<std::sync::Arc<hot::blob::BlobStore>>>>,
) -> Option<std::sync::Arc<hot::blob::BlobStore>> {
    ext.and_then(|axum::Extension(store)| store)
}

/// Rehydrate spilled BlobRefs in a persisted JSON payload (run result, event
/// data, call args) before returning it to API callers. Display reads fail
/// open: on rehydration errors the original JSON (still holding the BlobRef
/// map) is returned and a warning is logged, so one oversized or missing blob
/// does not break list endpoints.
pub(crate) async fn rehydrate_payload_json(
    db: &DatabasePool,
    blob_store: Option<&std::sync::Arc<hot::blob::BlobStore>>,
    env_id: Uuid,
    payload: Option<serde_json::Value>,
) -> Option<serde_json::Value> {
    let payload = payload?;
    let Some(store) = blob_store else {
        return Some(payload);
    };
    if !hot::blob::json_contains_blob_ref(&payload) {
        return Some(payload);
    }
    let org_id = match hot::db::Env::get_env(db, &env_id).await {
        Ok(env) => env.org_id,
        Err(e) => {
            tracing::warn!(%env_id, error = %e, "cannot resolve org for blob rehydration; returning payload as-is");
            return Some(payload);
        }
    };
    let scope = hot::blob::BlobScope {
        org_id,
        env_id: Some(env_id),
        run_id: None,
    };
    let budget = hot::blob::RehydrateBudget::from_config(store.config());
    match store.rehydrate_json(payload.clone(), scope, budget).await {
        Ok(rehydrated) => Some(rehydrated),
        Err(e) => {
            tracing::warn!(%env_id, error = %e, "blob rehydration failed; returning payload as-is");
            Some(payload)
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use hot::db::{self, Project, api_key::ApiKey};

    fn test_api_key(env_id: Uuid, user_id: Uuid) -> ApiKey {
        ApiKey {
            api_key_id: Uuid::now_v7(),
            env_id,
            description: "test".to_string(),
            key_data: serde_json::json!({}),
            active: true,
            created_by_user_id: user_id,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            updated_by_user_id: None,
            active_toggle_at: None,
            active_toggle_by_user_id: None,
            permissions: serde_json::json!({"*:*": ["*"]}),
        }
    }

    #[tokio::test]
    async fn write_project_lookup_reactivates_inactive_project_by_slug() {
        let db = db::test_db().await;
        let data = db::insert_test_data(&db).await.unwrap();
        let api_key = test_api_key(data.env_id, data.user_id);

        Project::toggle_active(&db, &data.project_id, false, &data.user_id)
            .await
            .unwrap();

        let project = get_and_ensure_active_project(&db, &api_key, "test-project")
            .await
            .unwrap();

        assert_eq!(project.project_id, data.project_id);
        assert!(project.active);
        assert!(
            Project::get_project(&db, &data.project_id)
                .await
                .unwrap()
                .active
        );
    }

    #[tokio::test]
    async fn write_project_lookup_reactivates_inactive_project_by_id() {
        let db = db::test_db().await;
        let data = db::insert_test_data(&db).await.unwrap();
        let api_key = test_api_key(data.env_id, data.user_id);

        Project::toggle_active(&db, &data.project_id, false, &data.user_id)
            .await
            .unwrap();

        let project = get_and_ensure_active_project(&db, &api_key, &data.project_id.to_string())
            .await
            .unwrap();

        assert_eq!(project.project_id, data.project_id);
        assert!(project.active);
        assert!(
            Project::get_project(&db, &data.project_id)
                .await
                .unwrap()
                .active
        );
    }
}

//! Project management handlers

use axum::{
    Extension, Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use hot::db::{
    api_key::ApiKey,
    project::{Project, ProjectError},
};
use uuid::Uuid;

use super::{ListQueryParams, get_and_verify_project_including_inactive};
use crate::ApiStateData;
use crate::auth::AuthContext;
use crate::models::*;

pub async fn list_projects(
    State((db, _storage, _conf, _stream_pubsub)): State<ApiStateData>,
    Extension(auth): Extension<AuthContext>,
    Extension(api_key): Extension<ApiKey>,
    Query(params): Query<ListQueryParams>,
) -> Result<Json<ApiListResponse<ProjectResponse>>, (StatusCode, Json<ApiErrorResponse>)> {
    super::require_api_key(&auth, "Only API keys can list projects.")?;

    let (projects, total) =
        Project::get_page_by_env(&db, api_key.env_id, params.limit, params.offset)
            .await
            .map_err(|e| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(ApiErrorResponse::internal_error(&e.to_string())),
                )
            })?;

    let project_responses: Vec<ProjectResponse> = projects
        .into_iter()
        .map(|p| ProjectResponse {
            project_id: p.project_id,
            env_id: p.env_id,
            name: p.name,
            active: p.active,
            created_at: p.created_at,
            updated_at: p.updated_at,
        })
        .collect();

    Ok(Json(ApiListResponse::new(
        project_responses,
        total,
        params.limit,
        params.offset,
    )))
}

pub async fn get_project(
    State((db, _storage, _conf, _stream_pubsub)): State<ApiStateData>,
    Extension(auth): Extension<AuthContext>,
    Extension(api_key): Extension<ApiKey>,
    Path(project_id_or_slug): Path<String>,
) -> Result<Json<ApiResponse<ProjectResponse>>, (StatusCode, Json<ApiErrorResponse>)> {
    super::require_api_key(&auth, "Only API keys can read projects.")?;

    // A name already resolves inactive projects, so an ID does too.
    let project =
        get_and_verify_project_including_inactive(&db, &api_key, &project_id_or_slug).await?;

    Ok(Json(ApiResponse::new(ProjectResponse {
        project_id: project.project_id,
        env_id: project.env_id,
        name: project.name,
        active: project.active,
        created_at: project.created_at,
        updated_at: project.updated_at,
    })))
}

pub async fn create_project(
    State((db, _storage, _conf, _stream_pubsub)): State<ApiStateData>,
    Extension(auth): Extension<AuthContext>,
    Extension(api_key): Extension<ApiKey>,
    Json(req): Json<CreateProjectRequest>,
) -> Result<(StatusCode, Json<ApiResponse<ProjectResponse>>), (StatusCode, Json<ApiErrorResponse>)>
{
    super::require_api_key(&auth, "Only API keys can create projects.")?;

    let project_id = Uuid::now_v7();

    let project = Project::insert_or_get_project(
        &db,
        &project_id,
        &api_key.env_id,
        &req.name,
        &api_key.created_by_user_id,
    )
    .await
    .map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ApiErrorResponse::internal_error(&e.to_string())),
        )
    })?;

    Ok((
        StatusCode::CREATED,
        Json(ApiResponse::new(ProjectResponse {
            project_id: project.project_id,
            env_id: project.env_id,
            name: project.name,
            active: project.active,
            created_at: project.created_at,
            updated_at: project.updated_at,
        })),
    ))
}

pub async fn update_project(
    State((db, _storage, _conf, _stream_pubsub)): State<ApiStateData>,
    Extension(auth): Extension<AuthContext>,
    Extension(api_key): Extension<ApiKey>,
    Path(project_id_or_slug): Path<String>,
    Json(req): Json<UpdateProjectRequest>,
) -> Result<Json<ApiResponse<ProjectResponse>>, (StatusCode, Json<ApiErrorResponse>)> {
    super::require_api_key(&auth, "Only API keys can update projects.")?;

    let project =
        get_and_verify_project_including_inactive(&db, &api_key, &project_id_or_slug).await?;

    // Update the project
    Project::update_name(
        &db,
        &project.project_id,
        &req.name,
        &api_key.created_by_user_id,
    )
    .await
    .map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ApiErrorResponse::internal_error(&e.to_string())),
        )
    })?;

    // Get updated project
    let updated_project = Project::get_project_including_inactive(&db, &project.project_id)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiErrorResponse::internal_error(&e.to_string())),
            )
        })?;

    Ok(Json(ApiResponse::new(ProjectResponse {
        project_id: updated_project.project_id,
        env_id: updated_project.env_id,
        name: updated_project.name,
        active: updated_project.active,
        created_at: updated_project.created_at,
        updated_at: updated_project.updated_at,
    })))
}

pub async fn delete_project(
    State((db, _storage, _conf, _stream_pubsub)): State<ApiStateData>,
    Extension(auth): Extension<AuthContext>,
    Extension(api_key): Extension<ApiKey>,
    Path(project_id_or_slug): Path<String>,
) -> Result<StatusCode, (StatusCode, Json<ApiErrorResponse>)> {
    super::require_api_key(&auth, "Only API keys can delete projects.")?;

    let project =
        get_and_verify_project_including_inactive(&db, &api_key, &project_id_or_slug).await?;

    Project::delete_project(&db, &project.project_id)
        .await
        .map_err(|e| match e {
            ProjectError::HasBuilds => (
                StatusCode::CONFLICT,
                Json(ApiErrorResponse::new("project_has_builds", e.to_string())),
            ),
            _ => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiErrorResponse::internal_error(&e.to_string())),
            ),
        })?;

    Ok(StatusCode::NO_CONTENT)
}

pub async fn activate_project(
    State((db, _storage, conf, _stream_pubsub)): State<ApiStateData>,
    Extension(auth): Extension<AuthContext>,
    Extension(api_key): Extension<ApiKey>,
    Path(project_id_or_slug): Path<String>,
) -> Result<Json<ApiResponse<ProjectActivateResponse>>, (StatusCode, Json<ApiErrorResponse>)> {
    super::require_api_key(&auth, "Only API keys can activate projects.")?;

    toggle_project_active(db, conf, api_key, project_id_or_slug, true).await
}

pub async fn deactivate_project(
    State((db, _storage, conf, _stream_pubsub)): State<ApiStateData>,
    Extension(auth): Extension<AuthContext>,
    Extension(api_key): Extension<ApiKey>,
    Path(project_id_or_slug): Path<String>,
) -> Result<Json<ApiResponse<ProjectActivateResponse>>, (StatusCode, Json<ApiErrorResponse>)> {
    super::require_api_key(&auth, "Only API keys can deactivate projects.")?;

    toggle_project_active(db, conf, api_key, project_id_or_slug, false).await
}

async fn toggle_project_active(
    db: std::sync::Arc<hot::db::DatabasePool>,
    conf: std::sync::Arc<hot::val::Val>,
    api_key: ApiKey,
    project_id_or_slug: String,
    active: bool,
) -> Result<Json<ApiResponse<ProjectActivateResponse>>, (StatusCode, Json<ApiErrorResponse>)> {
    let project =
        get_and_verify_project_including_inactive(&db, &api_key, &project_id_or_slug).await?;

    // Project::toggle_active also undeploys all builds and deactivates schedules
    // when transitioning to inactive (see crates/hot/src/db/project.rs).
    Project::toggle_active(
        &db,
        &project.project_id,
        active,
        &api_key.created_by_user_id,
    )
    .await
    .map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ApiErrorResponse::internal_error(&e.to_string())),
        )
    })?;

    // On reactivation, redeploy the latest build so schedules / event handlers
    // / MCP tools / webhooks / agents come back online — deactivation tore those
    // down via undeploy + schedule deactivation, and only the worker-side
    // manifest reload (triggered by a DeploymentMessage) restores them. Mirrors
    // the local CLI path and the in-app dashboard handler.
    let mut redeployed_build_id: Option<Uuid> = None;
    if active {
        match hot::lang::event::enqueue_redeploy_for_project_reactivation(
            &db,
            &conf,
            &project.project_id,
            &api_key.created_by_user_id,
        )
        .await
        {
            Ok(Some(build_id)) => {
                tracing::info!(
                    "Reactivated project {} and queued redeploy of latest build {}",
                    project.project_id,
                    build_id
                );
                redeployed_build_id = Some(build_id);
            }
            Ok(None) => {
                tracing::info!(
                    "Reactivated project {} (no builds yet, nothing to redeploy)",
                    project.project_id
                );
            }
            Err(e) => {
                // Toggle already succeeded; surface as 500 so the caller knows
                // the worker won't repaint handlers/schedules until they redeploy.
                tracing::error!(
                    "Reactivated project {} but failed to enqueue redeploy: {}",
                    project.project_id,
                    e
                );
                return Err((
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(ApiErrorResponse::internal_error(&format!(
                        "Project activated but failed to enqueue redeploy: {}",
                        e
                    ))),
                ));
            }
        }
    }

    // Re-fetch to return the post-toggle state, which may be inactive.
    let updated = Project::get_project_including_inactive(&db, &project.project_id)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiErrorResponse::internal_error(&e.to_string())),
            )
        })?;

    Ok(Json(ApiResponse::new(ProjectActivateResponse {
        project: ProjectResponse {
            project_id: updated.project_id,
            env_id: updated.env_id,
            name: updated.name,
            active: updated.active,
            created_at: updated.created_at,
            updated_at: updated.updated_at,
        },
        redeployed_build_id,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Router,
        body::Body,
        middleware,
        routing::{get, post},
    };
    use serde_json::{Value, json};
    use std::sync::Arc;
    use tower::ServiceExt;

    async fn app() -> (
        Router,
        Arc<hot::db::DatabasePool>,
        hot::db::TestData,
        String,
    ) {
        let db = Arc::new(hot::db::test_db().await);
        let data = hot::db::insert_test_data(&db).await.unwrap();
        let hot::db::DatabasePool::Sqlite(pool) = db.as_ref() else {
            unreachable!("SQLite fixture");
        };
        sqlx::query("PRAGMA foreign_keys = ON")
            .execute(pool)
            .await
            .unwrap();
        let key_id = Uuid::now_v7();
        let (token, hash) = ApiKey::generate_api_key(&key_id).unwrap();
        ApiKey::insert_api_key(
            &db,
            &key_id,
            &data.env_id,
            "projects",
            &serde_json::from_str(&hash).unwrap(),
            &data.user_id,
            &json!({"*:*": ["*"]}),
        )
        .await
        .unwrap();
        let state: ApiStateData = (
            db.clone(),
            Arc::new(Box::new(hot::storage::LocalBuildStorage::new(
                std::env::temp_dir(),
            ))),
            Arc::new(hot::val!({})),
            None,
        );
        let router = Router::new()
            .route(
                "/v1/projects/{project_id_or_slug}",
                get(get_project)
                    .patch(update_project)
                    .delete(delete_project),
            )
            .route(
                "/v1/projects/{project_id_or_slug}/activate",
                post(activate_project),
            )
            .route(
                "/v1/projects/{project_id_or_slug}/deactivate",
                post(deactivate_project),
            )
            .route_layer(middleware::from_fn_with_state(
                state.clone(),
                crate::auth::api_key_auth_middleware,
            ))
            .with_state(state);
        (router, db, data, token)
    }

    async fn call(
        app: &Router,
        method: &str,
        uri: &str,
        token: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let response = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method(method)
                    .uri(uri)
                    .header("authorization", format!("Bearer {token}"))
                    .header("content-type", "application/json")
                    .body(body.map_or_else(Body::empty, |body| Body::from(body.to_string())))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    #[tokio::test]
    async fn lifecycle_routes_resolve_inactive_projects_by_id() {
        let (app, _db, data, token) = app().await;
        let uri = format!("/v1/projects/{}", data.project_id);

        let (status, body) = call(&app, "POST", &format!("{uri}/deactivate"), &token, None).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["data"]["project"]["active"], false);

        let (status, body) = call(&app, "GET", &uri, &token, None).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["data"]["active"], false);

        let rename = json!({"name": "renamed"});
        let (status, body) = call(&app, "PATCH", &uri, &token, Some(rename)).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["data"]["name"], "renamed");
        assert_eq!(body["data"]["active"], false);

        let (status, body) = call(&app, "POST", &format!("{uri}/activate"), &token, None).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["data"]["project"]["active"], true);
    }

    #[tokio::test]
    async fn deleting_a_project_with_builds_conflicts_and_keeps_it() {
        let (app, db, data, token) = app().await;
        let uri = format!("/v1/projects/{}", data.project_id);
        let (status, body) = call(&app, "DELETE", &uri, &token, None).await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert_eq!(body["error"]["code"], "project_has_builds");
        assert!(
            Project::get_project_including_inactive(&db, &data.project_id)
                .await
                .is_ok()
        );

        // A project that never had a build is deleted, inactive or not.
        let empty = Uuid::now_v7();
        Project::insert_project(&db, &empty, &data.env_id, "empty", &data.user_id)
            .await
            .unwrap();
        let uri = format!("/v1/projects/{empty}");
        let (status, _) = call(&app, "POST", &format!("{uri}/deactivate"), &token, None).await;
        assert_eq!(status, StatusCode::OK);
        let (status, body) = call(&app, "DELETE", &uri, &token, None).await;
        assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
        let (status, _) = call(&app, "GET", &uri, &token, None).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }
}

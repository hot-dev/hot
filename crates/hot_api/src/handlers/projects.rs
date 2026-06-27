//! Project management handlers

use axum::{
    Extension, Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use hot::db::{api_key::ApiKey, project::Project};
use uuid::Uuid;

use super::{ListQueryParams, get_and_verify_project};
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

    let projects = Project::get_projects_by_env(
        &db,
        &api_key.env_id,
        Some(params.limit),
        Some(params.offset),
    )
    .await
    .map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ApiErrorResponse::internal_error(&e.to_string())),
        )
    })?;

    let total = Project::get_count_by_env(&db, &api_key.env_id)
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

    // Try to parse as UUID first, otherwise treat as slug (project name)
    let project = if let Ok(project_id) = Uuid::parse_str(&project_id_or_slug) {
        Project::get_project(&db, &project_id).await
    } else {
        // Find by name within the env
        Project::get_project_by_env_and_name(&db, &api_key.env_id, &project_id_or_slug)
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

    // Verify project belongs to the API key's environment
    if project.env_id != api_key.env_id {
        return Err((
            StatusCode::NOT_FOUND,
            Json(ApiErrorResponse::not_found("Project")),
        ));
    }

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

    // Get the project first to verify ownership
    let project = if let Ok(project_id) = Uuid::parse_str(&project_id_or_slug) {
        Project::get_project(&db, &project_id).await
    } else {
        Project::get_project_by_env_and_name(&db, &api_key.env_id, &project_id_or_slug)
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
    let updated_project = Project::get_project(&db, &project.project_id)
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

    // Get the project first to verify ownership
    let project = if let Ok(project_id) = Uuid::parse_str(&project_id_or_slug) {
        Project::get_project(&db, &project_id).await
    } else {
        Project::get_project_by_env_and_name(&db, &api_key.env_id, &project_id_or_slug)
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

    Project::delete_project(&db, &project.project_id)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiErrorResponse::internal_error(&e.to_string())),
            )
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
    let project = get_and_verify_project(&db, &api_key, &project_id_or_slug).await?;

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

    // Re-fetch to return the post-toggle state
    let updated = Project::get_project(&db, &project.project_id)
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

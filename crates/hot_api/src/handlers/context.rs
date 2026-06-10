//! Context variable (encrypted secrets) handlers

use axum::{
    Extension, Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use hot::context_encryption::ContextEncryption;
use hot::db::{api_key::ApiKey, context::Context};
use uuid::Uuid;

use super::{ListQueryParams, get_and_verify_project, get_org_id_for_env};
use crate::ApiStateData;
use crate::auth::AuthContext;
use crate::models::*;

pub async fn list_context_variables(
    State((db, _storage, _conf, _stream_pubsub)): State<ApiStateData>,
    Extension(auth): Extension<AuthContext>,
    Extension(api_key): Extension<ApiKey>,
    Path(project_id_or_slug): Path<String>,
    Query(params): Query<ListQueryParams>,
) -> Result<Json<ApiListResponse<ContextVariableResponse>>, (StatusCode, Json<ApiErrorResponse>)> {
    super::require_api_key(&auth, "Only API keys can list context variables.")?;

    let project = get_and_verify_project(&db, &api_key, &project_id_or_slug).await?;

    let (context_vars, total) = Context::get_by_project_paginated(
        &db,
        &project.project_id,
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

    // Only return metadata, NOT the encrypted values
    let responses: Vec<ContextVariableResponse> = context_vars
        .into_iter()
        .map(|cv| ContextVariableResponse {
            key: cv.key,
            description: cv.description,
            created_at: cv.created_at,
            updated_at: cv.updated_at,
        })
        .collect();

    Ok(Json(ApiListResponse::new(
        responses,
        total,
        params.limit,
        params.offset,
    )))
}

pub async fn create_context_variable(
    State((db, _storage, _conf, _stream_pubsub)): State<ApiStateData>,
    Extension(auth): Extension<AuthContext>,
    Extension(api_key): Extension<ApiKey>,
    Path(project_id_or_slug): Path<String>,
    Json(req): Json<CreateContextVariableRequest>,
) -> Result<
    (StatusCode, Json<ApiResponse<ContextVariableResponse>>),
    (StatusCode, Json<ApiErrorResponse>),
> {
    super::require_api_key(&auth, "Only API keys can create context variables.")?;

    let project = get_and_verify_project(&db, &api_key, &project_id_or_slug).await?;

    // Get org_id for encryption
    let org_id = get_org_id_for_env(&db, &api_key.env_id).await?;

    // Initialize encryption
    let encryption = ContextEncryption::from_env().map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ApiErrorResponse::internal_error(&format!(
                "Encryption not configured: {}",
                e
            ))),
        )
    })?;

    // Encrypt the value
    let encrypted_value = encryption.encrypt(&req.value, &org_id).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ApiErrorResponse::internal_error(&format!(
                "Encryption failed: {}",
                e
            ))),
        )
    })?;

    let context_id = Uuid::now_v7();

    #[allow(deprecated)]
    Context::insert(
        &db,
        &context_id,
        &project.project_id,
        &req.key,
        &encrypted_value,
        req.description.as_deref(),
        &api_key.created_by_user_id,
    )
    .await
    .map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ApiErrorResponse::internal_error(&e.to_string())),
        )
    })?;

    // Get the created context variable
    let context_var = Context::get_by_id(&db, &context_id).await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ApiErrorResponse::internal_error(&e.to_string())),
        )
    })?;

    Ok((
        StatusCode::CREATED,
        Json(ApiResponse::new(ContextVariableResponse {
            key: context_var.key,
            description: context_var.description,
            created_at: context_var.created_at,
            updated_at: context_var.updated_at,
        })),
    ))
}

pub async fn update_context_variable(
    State((db, _storage, _conf, _stream_pubsub)): State<ApiStateData>,
    Extension(auth): Extension<AuthContext>,
    Extension(api_key): Extension<ApiKey>,
    Path((project_id_or_slug, key)): Path<(String, String)>,
    Json(req): Json<UpdateContextVariableRequest>,
) -> Result<Json<ApiResponse<ContextVariableResponse>>, (StatusCode, Json<ApiErrorResponse>)> {
    super::require_api_key(&auth, "Only API keys can update context variables.")?;

    let project = get_and_verify_project(&db, &api_key, &project_id_or_slug).await?;

    // Get org_id for encryption
    let org_id = get_org_id_for_env(&db, &api_key.env_id).await?;

    // Get existing context variable
    let context_var = Context::get_by_project_and_key(&db, &project.project_id, &key)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiErrorResponse::internal_error(&e.to_string())),
            )
        })?
        .ok_or((
            StatusCode::NOT_FOUND,
            Json(ApiErrorResponse::not_found("Context variable")),
        ))?;

    // Initialize encryption
    let encryption = ContextEncryption::from_env().map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ApiErrorResponse::internal_error(&format!(
                "Encryption not configured: {}",
                e
            ))),
        )
    })?;

    // Encrypt the new value
    let encrypted_value = encryption.encrypt(&req.value, &org_id).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ApiErrorResponse::internal_error(&format!(
                "Encryption failed: {}",
                e
            ))),
        )
    })?;

    // Update the context variable
    let updated = Context::update(
        &db,
        &context_var.context_id,
        &encrypted_value,
        req.description.as_deref(),
        &api_key.created_by_user_id,
    )
    .await
    .map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ApiErrorResponse::internal_error(&e.to_string())),
        )
    })?;

    Ok(Json(ApiResponse::new(ContextVariableResponse {
        key: updated.key,
        description: updated.description,
        created_at: updated.created_at,
        updated_at: updated.updated_at,
    })))
}

pub async fn delete_context_variable(
    State((db, _storage, _conf, _stream_pubsub)): State<ApiStateData>,
    Extension(auth): Extension<AuthContext>,
    Extension(api_key): Extension<ApiKey>,
    Path((project_id_or_slug, key)): Path<(String, String)>,
) -> Result<StatusCode, (StatusCode, Json<ApiErrorResponse>)> {
    super::require_api_key(&auth, "Only API keys can delete context variables.")?;

    let project = get_and_verify_project(&db, &api_key, &project_id_or_slug).await?;

    // Get existing context variable
    let context_var = Context::get_by_project_and_key(&db, &project.project_id, &key)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiErrorResponse::internal_error(&e.to_string())),
            )
        })?
        .ok_or((
            StatusCode::NOT_FOUND,
            Json(ApiErrorResponse::not_found("Context variable")),
        ))?;

    Context::delete(&db, &context_var.context_id)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiErrorResponse::internal_error(&e.to_string())),
            )
        })?;

    Ok(StatusCode::NO_CONTENT)
}

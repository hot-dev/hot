//! Service key management handlers
//!
//! Service keys are long-lived, permission-scoped credentials issued by API key
//! holders to their customers and external systems for scoped access to MCP tools, webhooks,
//! and other API resources. These endpoints allow creating, listing,
//! updating metadata, and revoking service keys.

use axum::{
    Extension, Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use hot::context_encryption::ContextEncryption;
use hot::db::api_key::ApiKey;
use hot::db::service_key::ServiceKey;
use hot::permission::Permissions;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::ListQueryParams;
use crate::ApiStateData;
use crate::auth::AuthContext;
use crate::models::*;

// ============================================================================
// Request/Response DTOs
// ============================================================================

#[derive(Debug, Deserialize)]
pub struct CreateServiceKeyRequest {
    /// Human-readable name for the key
    pub name: Option<String>,
    /// Description of what this key is for
    pub description: Option<String>,
    /// Permissions map: resource URN -> action array
    pub permissions: serde_json::Value,
    /// Optional metadata
    pub metadata: Option<serde_json::Value>,
    /// Optional expiration in seconds from now (null = never expires)
    pub expires_in: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct ServiceKeyResponse {
    pub service_key_id: Uuid,
    pub name: Option<String>,
    pub description: Option<String>,
    /// Only present on creation
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    pub permissions: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revoked_at: Option<chrono::DateTime<chrono::Utc>>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_used_at: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Debug, Deserialize)]
pub struct UpdateServiceKeyRequest {
    /// Updated metadata (replaces existing metadata entirely)
    pub metadata: Option<serde_json::Value>,
}

#[derive(Debug, Serialize)]
pub struct RevokeAllServiceKeysResponse {
    pub revoked_count: u64,
}

// ============================================================================
// Helpers
// ============================================================================

/// Load encryption for metadata encrypt/decrypt. Returns None if not configured.
fn get_encryption() -> Option<ContextEncryption> {
    ContextEncryption::from_env_or_existing_dev_key().ok()
}

/// Require encryption (error if not configured). Used when metadata is provided.
fn require_encryption() -> Result<ContextEncryption, (StatusCode, Json<ApiErrorResponse>)> {
    get_encryption().ok_or_else(|| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ApiErrorResponse::internal_error(
                "Encryption not configured for metadata",
            )),
        )
    })
}

/// Build a ServiceKeyResponse with decrypted metadata.
fn build_response(
    ck: ServiceKey,
    token: Option<String>,
    encryption: Option<&ContextEncryption>,
    org_id: &Uuid,
) -> ServiceKeyResponse {
    let decrypted_metadata =
        encryption.and_then(|enc| ck.get_decrypted_metadata(enc, org_id).ok().flatten());

    ServiceKeyResponse {
        service_key_id: ck.service_key_id,
        name: ck.name,
        description: ck.description,
        token,
        permissions: ck.permissions,
        metadata: decrypted_metadata,
        expires_at: ck.expires_at,
        revoked_at: ck.revoked_at,
        created_at: ck.created_at,
        last_used_at: ck.last_used_at,
    }
}

// ============================================================================
// Handlers
// ============================================================================

/// POST /v1/service-keys — Create a new service key
pub async fn create_service_key(
    State((db, _storage, _conf, _stream_pubsub)): State<ApiStateData>,
    Extension(auth): Extension<AuthContext>,
    Extension(api_key): Extension<ApiKey>,
    Json(body): Json<CreateServiceKeyRequest>,
) -> Result<(StatusCode, Json<ApiResponse<ServiceKeyResponse>>), (StatusCode, Json<ApiErrorResponse>)>
{
    // Feature gate: service keys require Pro+ plan
    let org_id = super::get_org_id_for_env(&db, &api_key.env_id).await?;
    let features = hot::db::Features::resolve_for_org(&db, &org_id).await;
    if !features.has_service_keys() {
        return Err((
            StatusCode::FORBIDDEN,
            Json(ApiErrorResponse::new(
                "plan_required",
                "Service keys require a Pro or Scale plan. Upgrade at https://hot.dev/pricing"
                    .to_string(),
            )),
        ));
    }

    // Service keys can only be created by API keys. Sessions and service keys
    // cannot create service keys (no privilege escalation).
    if !auth.is_api_key() {
        return Err((
            StatusCode::FORBIDDEN,
            Json(ApiErrorResponse::new(
                "forbidden",
                "Only API keys can create service keys. Sessions and service keys cannot create service keys.",
            )),
        ));
    }

    // Parse and validate permissions
    let permissions = Permissions::from_json_validated(&body.permissions).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            Json(ApiErrorResponse::bad_request(&format!(
                "Invalid permissions: {}",
                e
            ))),
        )
    })?;

    // Service keys may only use customer-facing resource types
    permissions
        .validate_resource_types(hot::permission::resource_types::SERVICE_KEY_ALLOWED)
        .map_err(|e| {
            (
                StatusCode::BAD_REQUEST,
                Json(ApiErrorResponse::bad_request(&format!(
                    "Invalid permissions: {}",
                    e
                ))),
            )
        })?;

    // Validate permissions are a subset of the parent API key's permissions
    let parent_permissions = api_key.get_permissions().map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ApiErrorResponse::internal_error(&format!(
                "Failed to parse API key permissions: {}",
                e
            ))),
        )
    })?;
    permissions
        .validate_subset_of(&parent_permissions)
        .map_err(|e| {
            (
                StatusCode::FORBIDDEN,
                Json(ApiErrorResponse::new(
                    "permission_escalation",
                    format!("Requested permissions exceed API key permissions: {}", e),
                )),
            )
        })?;

    // Validate expiration if provided
    let expires_at = if let Some(expires_in) = body.expires_in {
        if expires_in <= 0 {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(ApiErrorResponse::bad_request(
                    "expires_in must be a positive number of seconds",
                )),
            ));
        }
        Some(chrono::Utc::now() + chrono::Duration::seconds(expires_in))
    } else {
        None // Never expires
    };

    // Encrypt metadata if provided (requires encryption to be configured)
    let encrypted_metadata = if let Some(ref meta) = body.metadata {
        let enc = require_encryption()?;
        Some(
            ServiceKey::encrypt_metadata(meta, &enc, &org_id).map_err(|e| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(ApiErrorResponse::internal_error(&e.to_string())),
                )
            })?,
        )
    } else {
        None
    };

    // Create the service key
    let (service_key, token) = ServiceKey::create(
        &db,
        &api_key.api_key_id,
        &api_key.env_id,
        body.name.as_deref(),
        body.description.as_deref(),
        &body.permissions,
        encrypted_metadata.as_deref(),
        expires_at,
    )
    .await
    .map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ApiErrorResponse::internal_error(&e.to_string())),
        )
    })?;

    let encryption = get_encryption();
    let response = build_response(service_key, Some(token), encryption.as_ref(), &org_id);

    Ok((
        StatusCode::CREATED,
        Json(ApiResponse {
            data: response,
            meta: ResponseMeta {
                request_id: Uuid::new_v4(),
                timestamp: chrono::Utc::now(),
            },
        }),
    ))
}

/// GET /v1/service-keys — List service keys for the authenticated API key
pub async fn list_service_keys(
    State((db, _storage, _conf, _stream_pubsub)): State<ApiStateData>,
    Extension(api_key): Extension<ApiKey>,
    Query(params): Query<ListQueryParams>,
) -> Result<Json<ApiListResponse<ServiceKeyResponse>>, (StatusCode, Json<ApiErrorResponse>)> {
    let encryption = get_encryption();
    let org_id = super::get_org_id_for_env(&db, &api_key.env_id).await?;

    let service_keys = ServiceKey::list_by_api_key(
        &db,
        &api_key.api_key_id,
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

    let total = service_keys.len() as i64;

    let data: Vec<ServiceKeyResponse> = service_keys
        .into_iter()
        .map(|ck| build_response(ck, None, encryption.as_ref(), &org_id))
        .collect();

    Ok(Json(ApiListResponse {
        pagination: PaginationMeta {
            total,
            limit: params.limit,
            offset: params.offset,
            has_more: params.offset + params.limit < total,
        },
        data,
        meta: ResponseMeta {
            request_id: Uuid::new_v4(),
            timestamp: chrono::Utc::now(),
        },
    }))
}

/// GET /v1/service-keys/{service_key_id} — Get a specific service key
pub async fn get_service_key(
    State((db, _storage, _conf, _stream_pubsub)): State<ApiStateData>,
    Extension(api_key): Extension<ApiKey>,
    Path(service_key_id): Path<Uuid>,
) -> Result<Json<ApiResponse<ServiceKeyResponse>>, (StatusCode, Json<ApiErrorResponse>)> {
    let encryption = get_encryption();
    let org_id = super::get_org_id_for_env(&db, &api_key.env_id).await?;

    let service_key = ServiceKey::get_service_key(&db, &service_key_id)
        .await
        .map_err(|e| match e {
            hot::db::service_key::ServiceKeyError::NotFound => (
                StatusCode::NOT_FOUND,
                Json(ApiErrorResponse::not_found("Service key")),
            ),
            _ => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiErrorResponse::internal_error(&e.to_string())),
            ),
        })?;

    // Verify the service key belongs to this API key
    if service_key.api_key_id != api_key.api_key_id {
        return Err((
            StatusCode::NOT_FOUND,
            Json(ApiErrorResponse::not_found("Service key")),
        ));
    }

    let response = build_response(service_key, None, encryption.as_ref(), &org_id);

    Ok(Json(ApiResponse {
        data: response,
        meta: ResponseMeta {
            request_id: Uuid::new_v4(),
            timestamp: chrono::Utc::now(),
        },
    }))
}

/// PATCH /v1/service-keys/{service_key_id} — Update service key metadata
pub async fn update_service_key(
    State((db, _storage, _conf, _stream_pubsub)): State<ApiStateData>,
    Extension(auth): Extension<AuthContext>,
    Extension(api_key): Extension<ApiKey>,
    Path(service_key_id): Path<Uuid>,
    Json(body): Json<UpdateServiceKeyRequest>,
) -> Result<Json<ApiResponse<ServiceKeyResponse>>, (StatusCode, Json<ApiErrorResponse>)> {
    // Only API keys can update service keys
    if !auth.is_api_key() {
        return Err((
            StatusCode::FORBIDDEN,
            Json(ApiErrorResponse::new(
                "forbidden",
                "Only API keys can update service keys.",
            )),
        ));
    }

    let org_id = super::get_org_id_for_env(&db, &api_key.env_id).await?;

    // Verify the service key exists and belongs to this API key
    let service_key = ServiceKey::get_service_key(&db, &service_key_id)
        .await
        .map_err(|e| match e {
            hot::db::service_key::ServiceKeyError::NotFound => (
                StatusCode::NOT_FOUND,
                Json(ApiErrorResponse::not_found("Service key")),
            ),
            _ => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiErrorResponse::internal_error(&e.to_string())),
            ),
        })?;

    if service_key.api_key_id != api_key.api_key_id {
        return Err((
            StatusCode::NOT_FOUND,
            Json(ApiErrorResponse::not_found("Service key")),
        ));
    }

    if service_key.is_revoked() {
        return Err((
            StatusCode::CONFLICT,
            Json(ApiErrorResponse::new(
                "revoked",
                "Cannot update a revoked service key",
            )),
        ));
    }

    // Encrypt new metadata if provided (requires encryption to be configured)
    let encrypted_metadata = if let Some(ref meta) = body.metadata {
        let enc = require_encryption()?;
        Some(
            ServiceKey::encrypt_metadata(meta, &enc, &org_id).map_err(|e| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(ApiErrorResponse::internal_error(&e.to_string())),
                )
            })?,
        )
    } else {
        None
    };

    ServiceKey::update_metadata(&db, &service_key_id, encrypted_metadata.as_deref())
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiErrorResponse::internal_error(&e.to_string())),
            )
        })?;

    // Re-fetch to return updated state
    let updated = ServiceKey::get_service_key(&db, &service_key_id)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiErrorResponse::internal_error(&e.to_string())),
            )
        })?;

    let encryption = get_encryption();
    let response = build_response(updated, None, encryption.as_ref(), &org_id);

    Ok(Json(ApiResponse {
        data: response,
        meta: ResponseMeta {
            request_id: Uuid::new_v4(),
            timestamp: chrono::Utc::now(),
        },
    }))
}

/// DELETE /v1/service-keys/{service_key_id} — Revoke a specific service key
pub async fn revoke_service_key(
    State((db, _storage, _conf, _stream_pubsub)): State<ApiStateData>,
    Extension(api_key): Extension<ApiKey>,
    Path(service_key_id): Path<Uuid>,
) -> Result<StatusCode, (StatusCode, Json<ApiErrorResponse>)> {
    // Verify the service key belongs to this API key
    let service_key = ServiceKey::get_service_key(&db, &service_key_id)
        .await
        .map_err(|e| match e {
            hot::db::service_key::ServiceKeyError::NotFound => (
                StatusCode::NOT_FOUND,
                Json(ApiErrorResponse::not_found("Service key")),
            ),
            _ => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiErrorResponse::internal_error(&e.to_string())),
            ),
        })?;

    if service_key.api_key_id != api_key.api_key_id {
        return Err((
            StatusCode::NOT_FOUND,
            Json(ApiErrorResponse::not_found("Service key")),
        ));
    }

    ServiceKey::revoke(&db, &service_key_id)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiErrorResponse::internal_error(&e.to_string())),
            )
        })?;

    Ok(StatusCode::NO_CONTENT)
}

/// DELETE /v1/service-keys — Revoke all active service keys for the authenticated API key
pub async fn revoke_all_service_keys(
    State((db, _storage, _conf, _stream_pubsub)): State<ApiStateData>,
    Extension(api_key): Extension<ApiKey>,
) -> Result<Json<ApiResponse<RevokeAllServiceKeysResponse>>, (StatusCode, Json<ApiErrorResponse>)> {
    let revoked_count = ServiceKey::revoke_all_by_api_key(&db, &api_key.api_key_id)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiErrorResponse::internal_error(&e.to_string())),
            )
        })?;

    Ok(Json(ApiResponse {
        data: RevokeAllServiceKeysResponse { revoked_count },
        meta: ResponseMeta {
            request_id: Uuid::new_v4(),
            timestamp: chrono::Utc::now(),
        },
    }))
}

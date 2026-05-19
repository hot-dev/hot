//! Session management handlers
//!
//! Sessions are short-lived, scoped access tokens created by API key holders.
//! These endpoints allow creating, listing, and revoking sessions.

use axum::{
    Extension, Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use hot::db::api_key::ApiKey;
use hot::db::session::Session;
use hot::permission::Permissions;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use super::ListQueryParams;
use crate::ApiStateData;
use crate::auth::AuthContext;
use crate::models::*;

// ============================================================================
// Request/Response DTOs
// ============================================================================

#[derive(Debug, Deserialize, ToSchema)]
pub struct CreateSessionRequest {
    /// Permissions map: resource URN -> action array
    pub permissions: serde_json::Value,
    /// Optional metadata (user ID, purpose, etc.)
    pub metadata: Option<serde_json::Value>,
    /// TTL in seconds (default: 3600, max: 86400)
    pub expires_in: Option<i64>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct SessionResponse {
    pub session_id: Uuid,
    /// Only present on creation
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    pub permissions: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
    pub expires_at: chrono::DateTime<chrono::Utc>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_used_at: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct RevokeAllResponse {
    pub revoked_count: u64,
}

// ============================================================================
// Handlers
// ============================================================================

/// POST /v1/sessions — Create a new session token
pub async fn create_session(
    State((db, _storage, _conf, _stream_pubsub)): State<ApiStateData>,
    Extension(auth): Extension<AuthContext>,
    Extension(api_key): Extension<ApiKey>,
    Json(body): Json<CreateSessionRequest>,
) -> Result<(StatusCode, Json<ApiResponse<SessionResponse>>), (StatusCode, Json<ApiErrorResponse>)>
{
    // Sessions can only be created by API keys. Sessions and service keys
    // cannot create sessions (no sub-sessions, no privilege escalation).
    // Service key → session support may be added later with proper subset
    // validation against the service key's permissions rather than the
    // parent API key's permissions.
    if !auth.is_api_key() {
        return Err((
            StatusCode::FORBIDDEN,
            Json(ApiErrorResponse::new(
                "forbidden",
                "Only API keys can create sessions. Sessions and service keys cannot create sessions.",
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

    // Validate TTL
    let expires_in = body.expires_in.unwrap_or(Session::default_ttl());
    if expires_in <= 0 {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ApiErrorResponse::bad_request(
                "expires_in must be a positive number of seconds",
            )),
        ));
    }
    if expires_in > Session::max_ttl() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ApiErrorResponse::bad_request(&format!(
                "expires_in cannot exceed {} seconds (24 hours)",
                Session::max_ttl()
            ))),
        ));
    }

    // Check active session count limit
    let active_count = Session::count_active_by_api_key(&db, &api_key.api_key_id)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiErrorResponse::internal_error(&e.to_string())),
            )
        })?;

    if active_count >= Session::max_active_sessions() {
        return Err((
            StatusCode::TOO_MANY_REQUESTS,
            Json(ApiErrorResponse::new(
                "session_limit_exceeded",
                format!(
                    "Maximum active sessions ({}) exceeded for this API key. Revoke existing sessions first.",
                    Session::max_active_sessions()
                ),
            )),
        ));
    }

    // Create the session
    let (session, token) = Session::create(
        &db,
        &api_key.api_key_id,
        &api_key.env_id,
        &body.permissions,
        body.metadata.as_ref(),
        Some(expires_in),
    )
    .await
    .map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ApiErrorResponse::internal_error(&e.to_string())),
        )
    })?;

    let response = SessionResponse {
        session_id: session.session_id,
        token: Some(token),
        permissions: session.permissions,
        metadata: session.metadata,
        expires_at: session.expires_at,
        created_at: session.created_at,
        last_used_at: session.last_used_at,
    };

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

/// GET /v1/sessions — List active sessions for the authenticated API key
pub async fn list_sessions(
    State((db, _storage, _conf, _stream_pubsub)): State<ApiStateData>,
    Extension(api_key): Extension<ApiKey>,
    Query(params): Query<ListQueryParams>,
) -> Result<Json<ApiListResponse<SessionResponse>>, (StatusCode, Json<ApiErrorResponse>)> {
    let sessions = Session::list_by_api_key(
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

    let total = Session::count_active_by_api_key(&db, &api_key.api_key_id)
        .await
        .unwrap_or(0);

    let data: Vec<SessionResponse> = sessions
        .into_iter()
        .map(|s| SessionResponse {
            session_id: s.session_id,
            token: None, // Never return tokens after creation
            permissions: s.permissions,
            metadata: s.metadata,
            expires_at: s.expires_at,
            created_at: s.created_at,
            last_used_at: s.last_used_at,
        })
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

/// DELETE /v1/sessions/{session_id} — Revoke a specific session
pub async fn revoke_session(
    State((db, _storage, _conf, _stream_pubsub)): State<ApiStateData>,
    Extension(api_key): Extension<ApiKey>,
    Path(session_id): Path<Uuid>,
) -> Result<StatusCode, (StatusCode, Json<ApiErrorResponse>)> {
    // Verify the session belongs to this API key
    let session = Session::get_session(&db, &session_id)
        .await
        .map_err(|e| match e {
            hot::db::session::SessionError::NotFound => (
                StatusCode::NOT_FOUND,
                Json(ApiErrorResponse::not_found("Session")),
            ),
            _ => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiErrorResponse::internal_error(&e.to_string())),
            ),
        })?;

    if session.api_key_id != api_key.api_key_id {
        return Err((
            StatusCode::NOT_FOUND,
            Json(ApiErrorResponse::not_found("Session")),
        ));
    }

    Session::revoke(&db, &session_id).await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ApiErrorResponse::internal_error(&e.to_string())),
        )
    })?;

    Ok(StatusCode::NO_CONTENT)
}

/// DELETE /v1/sessions — Revoke all active sessions for the authenticated API key
pub async fn revoke_all_sessions(
    State((db, _storage, _conf, _stream_pubsub)): State<ApiStateData>,
    Extension(api_key): Extension<ApiKey>,
) -> Result<Json<ApiResponse<RevokeAllResponse>>, (StatusCode, Json<ApiErrorResponse>)> {
    let revoked_count = Session::revoke_all_by_api_key(&db, &api_key.api_key_id)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiErrorResponse::internal_error(&e.to_string())),
            )
        })?;

    Ok(Json(ApiResponse {
        data: RevokeAllResponse { revoked_count },
        meta: ResponseMeta {
            request_id: Uuid::new_v4(),
            timestamp: chrono::Utc::now(),
        },
    }))
}

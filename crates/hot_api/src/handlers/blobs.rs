//! Blob reference download handler
//!
//! Serves the full content behind a `::hot::blob/BlobRef` typed map. Access
//! is authorized by active `blob_ref_id` plus the caller's tenant scope
//! (org/env derived from the API key), never by content hash or object id.

use crate::ApiStateData;
use crate::models::*;
use axum::{
    Extension, Json,
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
};
use hot::blob::{BlobError, BlobScope, BlobStore};
use hot::db::Env;
use hot::db::api_key::ApiKey;
use std::sync::Arc;
use uuid::Uuid;

use super::blob_store_from_ext;
use crate::auth::AuthContext;

/// GET /v1/blobs/{blob_ref_id}/download
///
/// Returns the raw bytes for an active blob reference in the caller's
/// org/env. String-encoded blobs are served as UTF-8 text.
pub async fn download_blob(
    State((db, _storage, _conf, _stream_pubsub)): State<ApiStateData>,
    Extension(auth): Extension<AuthContext>,
    Extension(api_key): Extension<ApiKey>,
    blob_store: Option<Extension<Option<Arc<BlobStore>>>>,
    Path(blob_ref_id): Path<Uuid>,
) -> Result<impl IntoResponse, (StatusCode, Json<ApiErrorResponse>)> {
    super::require_api_key(&auth, "Only API keys can download blobs.")?;

    let Some(blob_store) = blob_store_from_ext(blob_store) else {
        return Err((
            StatusCode::NOT_FOUND,
            Json(ApiErrorResponse::not_found("Blob")),
        ));
    };

    let org_id = Env::get_env(&db, &api_key.env_id)
        .await
        .map(|env| env.org_id)
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiErrorResponse::internal_error(&e.to_string())),
            )
        })?;

    let scope = BlobScope {
        org_id,
        env_id: Some(api_key.env_id),
        run_id: None,
    };

    let (bytes, object) = blob_store
        .read_ref_bytes(blob_ref_id, &scope)
        .await
        .map_err(|e| match e {
            // Unauthorized is reported as not-found so refs are not probeable
            // across tenants.
            BlobError::RefNotFound(_)
            | BlobError::ObjectNotAvailable(_)
            | BlobError::Unauthorized => (
                StatusCode::NOT_FOUND,
                Json(ApiErrorResponse::not_found("Blob")),
            ),
            other => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiErrorResponse::internal_error(&other.to_string())),
            ),
        })?;

    let content_type = object
        .content_type
        .as_deref()
        .unwrap_or("application/octet-stream")
        .to_string();

    Ok((
        StatusCode::OK,
        [
            (axum::http::header::CONTENT_TYPE, content_type),
            (
                axum::http::header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"blob-{}\"", blob_ref_id),
            ),
            (axum::http::header::CONTENT_LENGTH, bytes.len().to_string()),
        ],
        bytes,
    ))
}

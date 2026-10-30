//! Call trace purges. Environment keys purge one run, one stream or their
//! whole environment; org keys purge organizations in `orgs`.

use crate::ApiStateData;
use crate::auth::AuthContext;
use crate::models::{ApiErrorResponse, ApiResponse};
use axum::{
    Extension, Json,
    extract::{Path, State},
    http::StatusCode,
};
use hot::db::{Call, CallPurge, CallPurgeScope, Stream, api_key::ApiKey, run::Run};
use serde::Serialize;
use std::time::{Duration, Instant};
use utoipa::ToSchema;
use uuid::Uuid;

/// Calls deleted per transaction, as in call retention.
pub(crate) const PURGE_BATCH_SIZE: i64 = 10_000;

/// How long one request keeps purging. A larger scope reports
/// `complete: false`, and repeating the request continues it.
pub(crate) const PURGE_BUDGET: Duration = Duration::from_secs(20);

pub(crate) fn purge_deadline() -> Instant {
    Instant::now() + PURGE_BUDGET
}

/// The outcome of one purge request.
#[derive(Debug, Serialize, ToSchema)]
pub struct CallPurgeResponse {
    /// Call traces this request deleted.
    pub calls_deleted: i64,
    /// Spilled call payloads this request released. Blob garbage collection
    /// removes them after its grace period.
    pub blob_refs_released: u64,
    /// False when the request ran out of time. Repeat it to continue.
    pub complete: bool,
}

impl From<CallPurge> for CallPurgeResponse {
    fn from(purged: CallPurge) -> Self {
        Self {
            calls_deleted: purged.calls,
            blob_refs_released: purged.blob_refs,
            complete: purged.complete,
        }
    }
}

type PurgeResult =
    Result<Json<ApiResponse<CallPurgeResponse>>, (StatusCode, Json<ApiErrorResponse>)>;

pub(crate) fn purge_failed(error: impl std::fmt::Display) -> (StatusCode, Json<ApiErrorResponse>) {
    tracing::error!(%error, "call purge failed");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(ApiErrorResponse::internal_error("Call purge failed")),
    )
}

async fn purge(db: &hot::db::DatabasePool, env_id: &Uuid, scope: CallPurgeScope) -> PurgeResult {
    let purged = Call::purge(db, env_id, scope, PURGE_BATCH_SIZE, Some(purge_deadline()))
        .await
        .map_err(purge_failed)?;
    Ok(Json(ApiResponse::new(purged.into())))
}

/// Delete the call traces of one run.
pub async fn purge_run_calls(
    State((db, _, _, _)): State<ApiStateData>,
    Extension(auth): Extension<AuthContext>,
    Extension(api_key): Extension<ApiKey>,
    Path(run_id): Path<Uuid>,
) -> PurgeResult {
    super::require_api_key(&auth, "Only API keys can purge calls.")?;
    let not_found = || {
        (
            StatusCode::NOT_FOUND,
            Json(ApiErrorResponse::not_found("Run")),
        )
    };
    let run = Run::get_run(&db, &run_id)
        .await
        .map_err(|error| match error {
            hot::db::run::RunError::NotFound => not_found(),
            other => purge_failed(other),
        })?;
    if run.env_id != api_key.env_id {
        return Err(not_found());
    }
    purge(&db, &api_key.env_id, CallPurgeScope::Run(run_id)).await
}

/// Delete the call traces of every run in one stream.
pub async fn purge_stream_calls(
    State((db, _, _, _)): State<ApiStateData>,
    Extension(auth): Extension<AuthContext>,
    Extension(api_key): Extension<ApiKey>,
    Path(stream_id): Path<Uuid>,
) -> PurgeResult {
    super::require_api_key(&auth, "Only API keys can purge calls.")?;
    let owned = Stream::belongs_to_env(&db, stream_id, api_key.env_id)
        .await
        .map_err(purge_failed)?;
    if !owned {
        return Err((
            StatusCode::NOT_FOUND,
            Json(ApiErrorResponse::not_found("Stream")),
        ));
    }
    purge(&db, &api_key.env_id, CallPurgeScope::Stream(stream_id)).await
}

/// Delete every call trace in the key's environment.
pub async fn purge_env_calls(
    State((db, _, _, _)): State<ApiStateData>,
    Extension(auth): Extension<AuthContext>,
    Extension(api_key): Extension<ApiKey>,
) -> PurgeResult {
    super::require_api_key(&auth, "Only API keys can purge calls.")?;
    purge(&db, &api_key.env_id, CallPurgeScope::Env).await
}

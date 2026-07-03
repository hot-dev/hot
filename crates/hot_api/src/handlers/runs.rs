//! Run execution tracking handlers

use crate::ApiStateData;
use crate::models::*;
use axum::{
    Extension, Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use hot::blob::BlobStore;
use hot::db::{api_key::ApiKey, run::Run};
use hot::time_range::parse_time_range_cutoff;
use serde::Deserialize;
use std::sync::Arc;
use utoipa::ToSchema;
use uuid::Uuid;

use super::{blob_store_from_ext, deserialize_clamped_limit, rehydrate_payload_json};
use crate::auth::AuthContext;

fn default_limit() -> i64 {
    20
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct RunFilters {
    #[serde(
        default = "default_limit",
        deserialize_with = "deserialize_clamped_limit"
    )]
    pub limit: i64,
    #[serde(default)]
    pub offset: i64,
    pub status: Option<String>, // running|succeeded|failed|cancelled
    #[serde(rename = "type")]
    pub run_type: Option<String>, // call|event|schedule|run|eval|repl
    pub time_range: Option<String>, // P7D|P30D (ISO 8601 duration)
}

pub async fn list_runs(
    State((db, _storage, _conf, _stream_pubsub)): State<ApiStateData>,
    Extension(auth): Extension<AuthContext>,
    Extension(api_key): Extension<ApiKey>,
    blob_store: Option<Extension<Option<Arc<BlobStore>>>>,
    Query(filters): Query<RunFilters>,
) -> Result<Json<ApiListResponse<RunResponse>>, (StatusCode, Json<ApiErrorResponse>)> {
    super::require_api_key(&auth, "Only API keys can list runs.")?;
    let blob_store = blob_store_from_ext(blob_store);

    // Parse filters into arrays
    let statuses: Option<Vec<&str>> = filters.status.as_ref().map(|s| vec![s.as_str()]);
    let run_types: Option<Vec<&str>> = filters.run_type.as_ref().map(|t| vec![t.as_str()]);
    let time_range_cutoff =
        parse_time_range_cutoff(filters.time_range.as_deref(), chrono::Utc::now());

    let runs = Run::get_filtered_runs_by_env(
        &db,
        &api_key.env_id,
        statuses.as_deref(),
        run_types.as_deref(),
        time_range_cutoff,
        None, // project_id filter not supported in API yet
        None, // search_term - not supported in API yet
        Some(filters.limit),
        Some(filters.offset),
    )
    .await
    .map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ApiErrorResponse::internal_error(&e.to_string())),
        )
    })?;

    let total = Run::get_filtered_count_by_env(
        &db,
        &api_key.env_id,
        statuses.as_deref(),
        run_types.as_deref(),
        time_range_cutoff,
        None, // project_id filter not supported in API yet
        None, // search_term - not supported in API yet
    )
    .await
    .map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ApiErrorResponse::internal_error(&e.to_string())),
        )
    })?;

    let mut run_responses: Vec<RunResponse> = Vec::with_capacity(runs.len());
    for r in runs {
        let result = rehydrate_payload_json(&db, blob_store.as_ref(), r.env_id, r.result).await;
        run_responses.push(RunResponse {
            run_id: r.run_id,
            env_id: r.env_id,
            stream_id: r.stream_id,
            build_id: r.build_id,
            run_type: r.run_type,
            status: r.status,
            start_time: r.start_time,
            stop_time: r.stop_time,
            origin_run_id: r.origin_run_id,
            event_id: r.event_id,
            result,
            project_id: r.project_id,
            project_name: r.project_name,
            retry_attempt: r.retry_attempt,
            next_retry_at: r.next_retry_at,
        });
    }

    Ok(Json(ApiListResponse::new(
        run_responses,
        total,
        filters.limit,
        filters.offset,
    )))
}

pub async fn get_run(
    State((db, _storage, _conf, _stream_pubsub)): State<ApiStateData>,
    Extension(auth): Extension<AuthContext>,
    Extension(api_key): Extension<ApiKey>,
    blob_store: Option<Extension<Option<Arc<BlobStore>>>>,
    Path(run_id): Path<Uuid>,
) -> Result<Json<ApiResponse<RunResponse>>, (StatusCode, Json<ApiErrorResponse>)> {
    super::require_api_key(&auth, "Only API keys can read runs.")?;
    let blob_store = blob_store_from_ext(blob_store);

    let run = Run::get_run(&db, &run_id).await.map_err(|e| match e {
        hot::db::run::RunError::NotFound => (
            StatusCode::NOT_FOUND,
            Json(ApiErrorResponse::not_found("Run")),
        ),
        _ => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ApiErrorResponse::internal_error(&e.to_string())),
        ),
    })?;

    // Verify the run belongs to this environment
    if run.env_id != api_key.env_id {
        return Err((
            StatusCode::NOT_FOUND,
            Json(ApiErrorResponse::not_found("Run")),
        ));
    }

    let result = rehydrate_payload_json(&db, blob_store.as_ref(), run.env_id, run.result).await;

    Ok(Json(ApiResponse::new(RunResponse {
        run_id: run.run_id,
        env_id: run.env_id,
        stream_id: run.stream_id,
        build_id: run.build_id,
        run_type: run.run_type,
        status: run.status,
        start_time: run.start_time,
        stop_time: run.stop_time,
        origin_run_id: run.origin_run_id,
        event_id: run.event_id,
        result,
        project_id: run.project_id,
        project_name: run.project_name,
        retry_attempt: run.retry_attempt,
        next_retry_at: run.next_retry_at,
    })))
}

pub async fn get_run_stats(
    State((db, _storage, _conf, _stream_pubsub)): State<ApiStateData>,
    Extension(auth): Extension<AuthContext>,
    Extension(api_key): Extension<ApiKey>,
) -> Result<Json<ApiResponse<RunStatsResponse>>, (StatusCode, Json<ApiErrorResponse>)> {
    super::require_api_key(&auth, "Only API keys can read run stats.")?;

    use hot::db::run::RunStatus;

    let total = Run::get_count_by_env(&db, &api_key.env_id)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiErrorResponse::internal_error(&e.to_string())),
            )
        })?;

    let running = Run::get_count_by_status_and_env(&db, &RunStatus::Running, &api_key.env_id)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiErrorResponse::internal_error(&e.to_string())),
            )
        })?;

    let succeeded = Run::get_count_by_status_and_env(&db, &RunStatus::Succeeded, &api_key.env_id)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiErrorResponse::internal_error(&e.to_string())),
            )
        })?;

    let failed = Run::get_count_by_status_and_env(&db, &RunStatus::Failed, &api_key.env_id)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiErrorResponse::internal_error(&e.to_string())),
            )
        })?;

    let cancelled = Run::get_count_by_status_and_env(&db, &RunStatus::Cancelled, &api_key.env_id)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiErrorResponse::internal_error(&e.to_string())),
            )
        })?;

    Ok(Json(ApiResponse::new(RunStatsResponse {
        total_runs: total,
        running,
        succeeded,
        failed,
        cancelled,
    })))
}

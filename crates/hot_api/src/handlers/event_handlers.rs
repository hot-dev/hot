//! Event handler listing (read-only)

use axum::{
    Extension, Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use hot::db::{api_key::ApiKey, build::Build, event_handler::EventHandler};

use super::{ListQueryParams, get_and_verify_project};
use crate::ApiStateData;
use crate::auth::AuthContext;
use crate::models::*;

pub async fn list_project_event_handlers(
    State((db, _storage, _conf, _stream_pubsub)): State<ApiStateData>,
    Extension(auth): Extension<AuthContext>,
    Extension(api_key): Extension<ApiKey>,
    Path(project_id_or_slug): Path<String>,
    Query(params): Query<ListQueryParams>,
) -> Result<Json<ApiListResponse<EventHandlerResponse>>, (StatusCode, Json<ApiErrorResponse>)> {
    super::require_api_key(&auth, "Only API keys can list event handlers.")?;

    let project = get_and_verify_project(&db, &api_key, &project_id_or_slug).await?;

    // Get the deployed build
    let build = Build::get_deployed_build_by_project(&db, &project.project_id)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiErrorResponse::internal_error(&e.to_string())),
            )
        })?;

    let build = match build {
        Some(b) => b,
        None => {
            return Ok(Json(ApiListResponse::new(
                vec![],
                0,
                params.limit,
                params.offset,
            )));
        }
    };

    let handlers = EventHandler::get_event_handlers_by_build(
        &db,
        &build.build_id,
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

    let total = EventHandler::get_count_by_build(&db, &build.build_id)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiErrorResponse::internal_error(&e.to_string())),
            )
        })?;

    let handler_responses: Vec<EventHandlerResponse> = handlers
        .into_iter()
        .map(|h| EventHandlerResponse {
            event_handler_id: h.event_handler_id,
            build_id: h.build_id,
            event_type: h.event_type,
            ns: h.ns,
            var: h.var,
        })
        .collect();

    Ok(Json(ApiListResponse::new(
        handler_responses,
        total,
        params.limit,
        params.offset,
    )))
}

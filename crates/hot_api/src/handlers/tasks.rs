//! Task status and completion subscription handlers.

use axum::{
    Extension, Json,
    extract::{Path, State},
    http::StatusCode,
    response::sse::{Event as SseEvent, KeepAlive, Sse},
};
use futures::stream::Stream;
use hot::blob::BlobStore;
use hot::db::{task::Task, task::TaskError};
use hot::permission::actions;
use serde::Serialize;
use std::{convert::Infallible, sync::Arc, time::Duration};
use utoipa::ToSchema;
use uuid::Uuid;

use super::{blob_store_from_ext, rehydrate_payload_json};
use crate::auth::AuthContext;
use crate::models::{ApiErrorResponse, ApiResponse, TaskResponse};
use crate::{ApiStateData, rate_limit};

#[derive(Debug, Serialize, ToSchema)]
#[serde(tag = "type")]
pub enum TaskEvent {
    #[serde(rename = "task:update")]
    TaskUpdate { task: TaskResponse },
}

fn task_error(error: TaskError) -> (StatusCode, Json<ApiErrorResponse>) {
    match error {
        TaskError::NotFound => (
            StatusCode::NOT_FOUND,
            Json(ApiErrorResponse::not_found("Task")),
        ),
        other => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ApiErrorResponse::internal_error(&other.to_string())),
        ),
    }
}

fn task_is_terminal(task: &Task) -> bool {
    matches!(
        task.status.as_str(),
        "completed" | "failed" | "cancelled" | "timed_out"
    )
}

pub(crate) async fn task_to_response(
    db: &hot::db::DatabasePool,
    blob_store: Option<&Arc<BlobStore>>,
    task: &Task,
) -> TaskResponse {
    let result = rehydrate_payload_json(db, blob_store, task.env_id, task.result.clone()).await;
    TaskResponse {
        task_id: task.task_id,
        env_id: task.env_id,
        stream_id: task.stream_id,
        build_id: task.build_id,
        run_id: task.run_id,
        origin_run_id: task.origin_run_id,
        function_name: task.function_name.clone(),
        task_type: task.task_type.clone(),
        status: task.status.clone(),
        start_time: task.start_time,
        stop_time: task.stop_time,
        duration_ms: task.duration_ms,
        result,
        timeout_ms: task.timeout_ms,
        retry_attempt: task.retry_attempt,
        next_retry_at: task.next_retry_at,
        created_at: task.created_at,
    }
}

async fn authorize_task(
    db: &hot::db::DatabasePool,
    auth: &AuthContext,
    task_id: &Uuid,
) -> Result<Task, (StatusCode, Json<ApiErrorResponse>)> {
    let task = Task::get(db, task_id).await.map_err(task_error)?;
    if task.env_id != auth.env_id() {
        return Err((
            StatusCode::NOT_FOUND,
            Json(ApiErrorResponse::not_found("Task")),
        ));
    }

    let resource = format!("stream:{}", task.stream_id);
    super::require_permission(
        auth,
        &resource,
        actions::READ,
        "Credential does not have read access to this task's stream",
    )?;
    Ok(task)
}

/// Return the latest durable snapshot for a task.
pub async fn get_task(
    State((db, _storage, _conf, _stream_pubsub)): State<ApiStateData>,
    Extension(auth): Extension<AuthContext>,
    blob_store: Option<Extension<Option<Arc<BlobStore>>>>,
    Path(task_id): Path<Uuid>,
) -> Result<Json<ApiResponse<TaskResponse>>, (StatusCode, Json<ApiErrorResponse>)> {
    let blob_store = blob_store_from_ext(blob_store);
    let task = authorize_task(&db, &auth, &task_id).await?;
    Ok(Json(ApiResponse::new(
        task_to_response(&db, blob_store.as_ref(), &task).await,
    )))
}

/// Subscribe to durable task snapshots until the task reaches a terminal state.
///
/// The first event is always the task's current persisted state. This makes a
/// reconnect safe even when completion happened while the client was offline.
pub async fn subscribe_to_task(
    State((db, _storage, conf, _stream_pubsub)): State<ApiStateData>,
    Extension(auth): Extension<AuthContext>,
    blob_store: Option<Extension<Option<Arc<BlobStore>>>>,
    Path(task_id): Path<Uuid>,
) -> Result<
    Sse<impl Stream<Item = Result<SseEvent, Infallible>>>,
    (StatusCode, Json<ApiErrorResponse>),
> {
    let blob_store = blob_store_from_ext(blob_store);
    let initial = authorize_task(&db, &auth, &task_id).await?;
    let connection_guard = rate_limit::acquire_sse_connection(&db, &conf, &auth, "task-subscribe")
        .await
        .map_err(|exceeded| {
            (
                StatusCode::TOO_MANY_REQUESTS,
                Json(rate_limit::rate_limit_error_body(exceeded)),
            )
        })?;

    let db_clone = db.clone();
    let blob_store_clone = blob_store.clone();
    let stream = async_stream::stream! {
        let _connection_guard = connection_guard;
        let mut task = initial;

        loop {
            let terminal = task_is_terminal(&task);
            let response = task_to_response(&db_clone, blob_store_clone.as_ref(), &task).await;
            let event = TaskEvent::TaskUpdate { task: response };
            if let Ok(json) = serde_json::to_string(&event) {
                yield Ok(SseEvent::default().event("task:update").data(json));
            }
            if terminal {
                break;
            }

            let next = loop {
                tokio::time::sleep(Duration::from_millis(250)).await;
                match Task::get(&db_clone, &task_id).await {
                    Ok(candidate)
                        if candidate.task_status_id != task.task_status_id
                            || candidate.run_id != task.run_id
                            || candidate.start_time != task.start_time
                            || candidate.stop_time != task.stop_time
                            || candidate.duration_ms != task.duration_ms
                            || candidate.result != task.result
                            || candidate.retry_attempt != task.retry_attempt
                            || candidate.next_retry_at != task.next_retry_at => break Some(candidate),
                    Ok(_) => {}
                    Err(error) => {
                        tracing::warn!(%task_id, %error, "task subscription polling failed");
                        break None;
                    }
                }
            };
            let Some(next) = next else { break; };
            task = next;
        }
    };

    Ok(Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("keepalive"),
    ))
}

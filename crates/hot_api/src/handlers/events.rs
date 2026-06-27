//! Event pub/sub handlers

use ahash::{AHashMap, AHashSet};
use axum::{
    Extension, Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use hot::db::DatabasePool;
use hot::db::api_key::ApiKey;
use hot::db::event::Event;
use hot::db::stream::{Stream as DbStream, StreamError, StreamSummary};
use hot::permission::actions;
use hot::val::Val;
use once_cell::sync::OnceCell;
use std::str::FromStr;
use std::sync::Arc;
use uuid::Uuid;

use super::ListQueryParams;
use crate::ApiStateData;
use crate::access_log::OptionalAccessId;
use crate::auth::AuthContext;
use crate::models::*;

/// The queue name that API-published events are sent to.
/// This MUST match the worker's event queue name ("hot:event").
const API_EVENT_QUEUE_NAME: &str = "hot:event";

static API_EVENT_QUEUE: OnceCell<Arc<hot::queue::ProcessingQueue<hot::data::msg::Message>>> =
    OnceCell::new();

fn get_event_queue(
    conf: &Val,
) -> Result<&'static Arc<hot::queue::ProcessingQueue<hot::data::msg::Message>>, String> {
    API_EVENT_QUEUE.get_or_try_init(|| {
        let queue_type_str = conf.get_str_or_default("queue.type", "memory");
        let queue_type = hot::queue::QueueType::from_str(&queue_type_str)
            .unwrap_or(hot::queue::QueueType::Memory);

        let redis_uri_str = conf.get_str_or_default("redis.uri", "");
        let redis_uri = if redis_uri_str.is_empty() || redis_uri_str == "null" {
            None
        } else {
            Some(redis_uri_str)
        };

        let redis_cluster = conf.get_bool_or_default("redis.cluster", false);

        let serialization_str = conf.get_str_or_default("serialization.type", "zstd-json");
        let serialization = hot::data::serialization::Serialization::from_str(&serialization_str)
            .unwrap_or_default();

        let queue = hot::queue::ProcessingQueue::<hot::data::msg::Message>::new_with_cluster(
            queue_type,
            API_EVENT_QUEUE_NAME.to_string(),
            redis_uri,
            redis_cluster,
            serialization,
        )
        .map_err(|e| format!("Failed to create event queue: {}", e))?;

        tracing::debug!(
            "API events: initialized shared event queue (type: {})",
            queue_type_str
        );
        Ok(Arc::new(queue))
    })
}

/// Result of publishing an event internally
pub struct PublishedEvent {
    pub event_id: Uuid,
    pub stream_id: Uuid,
    pub event_type: String,
    pub event_time: chrono::DateTime<chrono::Utc>,
}

/// Internal function to publish an event - reused by both publish_event and subscribe_with_event
///
/// This handles:
/// 1. Creating the event record in the database
/// 2. Setting up the execution context
/// 3. Enqueuing the event for worker processing
///
/// The stream must already exist before calling this function.
pub async fn publish_event_internal(
    db: &DatabasePool,
    conf: &Val,
    api_key: &ApiKey,
    event_type: &str,
    event_data: &serde_json::Value,
    stream_id: Uuid,
    access_id: Option<Uuid>,
) -> Result<PublishedEvent, (StatusCode, Json<ApiErrorResponse>)> {
    let event_id = Uuid::now_v7();
    let event_time = chrono::Utc::now();

    Event::insert_event(
        db,
        &event_id,
        &api_key.env_id,
        &stream_id,
        event_type,
        event_data,
        event_time,
        &api_key.created_by_user_id,
        access_id.as_ref(),
    )
    .await
    .map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ApiErrorResponse::internal_error(&e.to_string())),
        )
    })?;

    // Enqueue event to hot:event queue for worker processing
    // Get org_id for execution context
    let env = hot::db::Env::get_env(db, &api_key.env_id)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiErrorResponse::internal_error(&format!(
                    "Failed to get environment: {}",
                    e
                ))),
            )
        })?;

    // Create execution context for the event
    let run_id = Uuid::now_v7();
    let execution_context = hot::lang::event::ExecutionContext {
        run_id,
        stream_id,
        run_type_id: hot::db::run::RunType::Event.as_id(),
        env_id: Some(api_key.env_id),
        env_name: None, // Will be populated later if needed
        user_id: Some(api_key.created_by_user_id),
        org_id: Some(env.org_id),
        org_slug: None,     // Will be populated later if needed
        build_id: None,     // Will be resolved by worker
        build_hash: None,   // Will be populated later if needed
        project_id: None,   // Will be populated later if needed
        project_name: None, // Will be populated later if needed
        event_id: Some(event_id),
        origin_run_id: None,
        retry_attempt: 0,
        secret_keys: AHashSet::new(), // Will be populated from ctx metadata
        secret_value_hashes: AHashSet::new(),
        access_id,
        agent_type: None,
    };

    // Create the event message
    let event_data_val: Val = serde_json::from_value(event_data.clone()).unwrap_or(Val::Null);
    let hot_event = hot::lang::event::Event {
        event_id,
        env_id: api_key.env_id,
        stream_id,
        event_type: event_type.to_string(),
        event_data: event_data_val,
        event_time,
        // API-published events have no target project (uses default routing)
        target_project_id: None,
        target_project_name: None,
    };

    let event_message = hot::lang::event::EventMessage {
        id: event_id,
        head: AHashMap::from_iter([
            ("env_id".to_string(), api_key.env_id.to_string()),
            ("event_type".to_string(), event_type.to_string()),
        ]),
        body: hot::lang::event::EventMessageBody {
            event: hot_event,
            execution_context,
        },
    };

    // Convert to unified Message format and enqueue
    let message: hot::data::msg::Message = event_message.into();

    use hot::queue::Queue;

    let queue = get_event_queue(conf).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ApiErrorResponse::internal_error(&e)),
        )
    })?;

    queue.enqueue(message).await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ApiErrorResponse::internal_error(&format!(
                "Failed to enqueue event: {}",
                e
            ))),
        )
    })?;

    tracing::info!(
        "Event {} ({}) published and queued for processing",
        event_id,
        event_type
    );

    Ok(PublishedEvent {
        event_id,
        stream_id,
        event_type: event_type.to_string(),
        event_time,
    })
}

pub async fn publish_event(
    State((db, _storage, _conf, _stream_pubsub)): State<ApiStateData>,
    Extension(auth): Extension<AuthContext>,
    Extension(api_key): Extension<ApiKey>,
    OptionalAccessId(access_id): OptionalAccessId,
    Json(req): Json<PublishEventRequest>,
) -> Result<(StatusCode, Json<ApiResponse<EventResponse>>), (StatusCode, Json<ApiErrorResponse>)> {
    // Permission check for scoped credentials (sessions and service keys)
    if !auth.is_api_key() {
        let event_resource = format!("event:{}", req.event_type);
        if !auth.has_permission(&event_resource, actions::CREATE)
            && !auth.has_permission("event:*", actions::CREATE)
        {
            return Err((
                StatusCode::FORBIDDEN,
                Json(ApiErrorResponse::new(
                    "forbidden",
                    "Credential does not have create permission for this event type",
                )),
            ));
        }
    }

    // Use provided stream_id or create a new one, then bind it to the caller env.
    let stream_id = req.stream_id.unwrap_or_else(Uuid::now_v7);
    if req.stream_id.is_some() {
        match StreamSummary::get_stream(&db, &stream_id).await {
            Ok(stream) if stream.env_id != auth.env_id() => {
                return Err((
                    StatusCode::NOT_FOUND,
                    Json(ApiErrorResponse::not_found("Stream")),
                ));
            }
            Ok(_) | Err(StreamError::NotFound) => {}
            Err(e) => {
                return Err((
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(ApiErrorResponse::internal_error(&e.to_string())),
                ));
            }
        }
    }
    DbStream::create_or_get_stream(&db, stream_id, auth.env_id())
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiErrorResponse::internal_error(&e.to_string())),
            )
        })?;

    // Publish the event using the internal function
    let published = publish_event_internal(
        &db,
        &_conf,
        &api_key,
        &req.event_type,
        &req.event_data,
        stream_id,
        access_id,
    )
    .await?;

    // Fetch the full event record to return
    let event = Event::get_event(&db, &published.event_id)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiErrorResponse::internal_error(&e.to_string())),
            )
        })?;

    Ok((
        StatusCode::CREATED,
        Json(ApiResponse::new(EventResponse {
            event_id: event.event_id,
            env_id: event.env_id,
            stream_id: event.stream_id,
            event_type: event.event_type,
            event_data: event.event_data,
            event_time: event.event_time,
            created_at: event.created_at,
        })),
    ))
}

pub async fn list_events(
    State((db, _storage, _conf, _stream_pubsub)): State<ApiStateData>,
    Extension(auth): Extension<AuthContext>,
    Extension(api_key): Extension<ApiKey>,
    Query(params): Query<ListQueryParams>,
) -> Result<Json<ApiListResponse<EventResponse>>, (StatusCode, Json<ApiErrorResponse>)> {
    // Permission check for scoped credentials (sessions and service keys)
    if !auth.is_api_key() && !auth.has_permission("event:*", actions::READ) {
        return Err((
            StatusCode::FORBIDDEN,
            Json(ApiErrorResponse::new(
                "forbidden",
                "Credential does not have read access to events",
            )),
        ));
    }

    let events = Event::get_events_by_env(
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

    let total = Event::get_count_by_env(&db, &api_key.env_id)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiErrorResponse::internal_error(&e.to_string())),
            )
        })?;

    let event_responses: Vec<EventResponse> = events
        .into_iter()
        .map(|e| EventResponse {
            event_id: e.event_id,
            env_id: e.env_id,
            stream_id: e.stream_id,
            event_type: e.event_type,
            event_data: e.event_data,
            event_time: e.event_time,
            created_at: e.created_at,
        })
        .collect();

    Ok(Json(ApiListResponse::new(
        event_responses,
        total,
        params.limit,
        params.offset,
    )))
}

pub async fn get_event(
    State((db, _storage, _conf, _stream_pubsub)): State<ApiStateData>,
    Extension(auth): Extension<AuthContext>,
    Extension(api_key): Extension<ApiKey>,
    Path(event_id): Path<Uuid>,
) -> Result<Json<ApiResponse<EventResponse>>, (StatusCode, Json<ApiErrorResponse>)> {
    // Permission check for scoped credentials (sessions and service keys)
    if !auth.is_api_key() && !auth.has_permission("event:*", actions::READ) {
        return Err((
            StatusCode::FORBIDDEN,
            Json(ApiErrorResponse::new(
                "forbidden",
                "Credential does not have read access to events",
            )),
        ));
    }

    let event = Event::get_event(&db, &event_id)
        .await
        .map_err(|e| match e {
            hot::db::event::EventError::NotFound => (
                StatusCode::NOT_FOUND,
                Json(ApiErrorResponse::not_found("Event")),
            ),
            _ => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiErrorResponse::internal_error(&e.to_string())),
            ),
        })?;

    // Verify the event belongs to this environment
    if event.env_id != api_key.env_id {
        return Err((
            StatusCode::NOT_FOUND,
            Json(ApiErrorResponse::not_found("Event")),
        ));
    }

    Ok(Json(ApiResponse::new(EventResponse {
        event_id: event.event_id,
        env_id: event.env_id,
        stream_id: event.stream_id,
        event_type: event.event_type,
        event_data: event.event_data,
        event_time: event.event_time,
        created_at: event.created_at,
    })))
}

pub async fn get_event_runs(
    State((db, _storage, _conf, _stream_pubsub)): State<ApiStateData>,
    Extension(auth): Extension<AuthContext>,
    Extension(api_key): Extension<ApiKey>,
    Path(event_id): Path<Uuid>,
    Query(params): Query<ListQueryParams>,
) -> Result<Json<ApiListResponse<RunResponse>>, (StatusCode, Json<ApiErrorResponse>)> {
    // Permission check for scoped credentials (sessions and service keys)
    if !auth.is_api_key() && !auth.has_permission("event:*", actions::READ) {
        return Err((
            StatusCode::FORBIDDEN,
            Json(ApiErrorResponse::new(
                "forbidden",
                "Credential does not have read access to events",
            )),
        ));
    }

    // First verify the event exists and belongs to this environment
    let event = Event::get_event(&db, &event_id)
        .await
        .map_err(|e| match e {
            hot::db::event::EventError::NotFound => (
                StatusCode::NOT_FOUND,
                Json(ApiErrorResponse::not_found("Event")),
            ),
            _ => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiErrorResponse::internal_error(&e.to_string())),
            ),
        })?;

    if event.env_id != api_key.env_id {
        return Err((
            StatusCode::NOT_FOUND,
            Json(ApiErrorResponse::not_found("Event")),
        ));
    }

    let runs = Event::get_runs_by_event(&db, &event_id, Some(params.limit), Some(params.offset))
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiErrorResponse::internal_error(&e.to_string())),
            )
        })?;

    let total = Event::get_run_count_by_event(&db, &event_id)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiErrorResponse::internal_error(&e.to_string())),
            )
        })?;

    let run_responses: Vec<RunResponse> = runs
        .into_iter()
        .map(|r| RunResponse {
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
            result: r.result,
            project_id: r.project_id,
            project_name: r.project_name,
            retry_attempt: r.retry_attempt,
            next_retry_at: r.next_retry_at,
        })
        .collect();

    Ok(Json(ApiListResponse::new(
        run_responses,
        total,
        params.limit,
        params.offset,
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use hot::val;

    #[test]
    fn api_event_queue_is_cached() {
        let conf = val!({
            "queue": {
                "type": "memory",
            },
            "serialization": {
                "type": "json",
            },
        });

        let first = get_event_queue(&conf).expect("first queue init");
        let second = get_event_queue(&conf).expect("second queue lookup");

        assert!(
            Arc::ptr_eq(first, second),
            "API event publishing should reuse the shared event queue"
        );
    }
}

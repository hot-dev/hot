//! Stream subscription (SSE) handlers
//!
//! Supports two transport styles:
//!   - **GET /v1/streams/{id}/subscribe** — Classic SSE (persistent stream, client subscribes)
//!   - **POST /v1/streams/{id}/subscribe** — Streamable HTTP (subscribe via POST, SSE response)
//!
//! Both return the same SSE events; the difference is how the connection starts.

use axum::{
    Extension, Json,
    extract::{Path, Query, State},
    http::StatusCode,
    response::sse::{Event as SseEvent, KeepAlive, Sse},
};
use futures::stream::Stream;
use hot::db::{
    api_key::ApiKey,
    run::Run,
    stream::{Stream as DbStream, StreamError, StreamSummary},
};
use hot::permission::actions;
use hot::stream::{
    StreamEvent as PubSubEvent, StreamNext, StreamSubscriber, StreamSubscriberFactory,
};
use serde::{Deserialize, Serialize};
use std::convert::Infallible;
use std::sync::Arc;
use utoipa::ToSchema;
use uuid::Uuid;

use hot::blob::BlobStore;

use super::{
    blob_store_from_ext, get_and_verify_project, publish_event_internal, rehydrate_payload_json,
};
use crate::ApiStateData;
use crate::access_log::OptionalAccessId;
use crate::auth::AuthContext;
use crate::models::*;

fn stream_not_found() -> (StatusCode, Json<ApiErrorResponse>) {
    (
        StatusCode::NOT_FOUND,
        Json(ApiErrorResponse::not_found("Stream")),
    )
}

fn stream_lookup_error(e: StreamError) -> (StatusCode, Json<ApiErrorResponse>) {
    match e {
        StreamError::NotFound => stream_not_found(),
        other => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ApiErrorResponse::internal_error(&format!(
                "Failed to look up stream: {}",
                other
            ))),
        ),
    }
}

enum StreamLookup {
    Found(Box<StreamSummary>),
    Missing,
    WrongEnv,
}

async fn lookup_stream_for_env(
    db: &hot::db::DatabasePool,
    stream_id: &Uuid,
    env_id: Uuid,
) -> Result<StreamLookup, (StatusCode, Json<ApiErrorResponse>)> {
    let stream = match StreamSummary::get_stream(db, stream_id).await {
        Ok(stream) => stream,
        Err(StreamError::NotFound) => return Ok(StreamLookup::Missing),
        Err(e) => return Err(stream_lookup_error(e)),
    };

    if stream.env_id != env_id {
        return Ok(StreamLookup::WrongEnv);
    }

    Ok(StreamLookup::Found(Box::new(stream)))
}

async fn get_stream_for_env(
    db: &hot::db::DatabasePool,
    stream_id: &Uuid,
    env_id: Uuid,
) -> Result<StreamSummary, (StatusCode, Json<ApiErrorResponse>)> {
    match lookup_stream_for_env(db, stream_id, env_id).await? {
        StreamLookup::Found(stream) => Ok(*stream),
        StreamLookup::Missing | StreamLookup::WrongEnv => Err(stream_not_found()),
    }
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct StreamSubscribeParams {
    /// Optional project filter - only include runs from this project
    pub project: Option<String>,
}

/// SSE event types for stream subscription
#[derive(Debug, Serialize, ToSchema)]
#[serde(tag = "type")]
pub enum StreamEvent {
    #[serde(rename = "run:start")]
    RunStart { run: RunResponse },
    #[serde(rename = "run:stop")]
    RunStop { run: RunResponse },
    #[serde(rename = "run:fail")]
    RunFail { run: RunResponse },
    #[serde(rename = "run:cancel")]
    RunCancel { run: RunResponse },
    /// User-emitted stream data (partial results, progress, SSE events, etc.)
    #[serde(rename = "stream:data")]
    StreamData {
        stream_data_id: Uuid,
        run_id: Uuid,
        data_type: String,
        payload: serde_json::Value,
    },
    #[serde(rename = "stream:complete")]
    StreamComplete { stream_id: Uuid },
    #[serde(rename = "keepalive")]
    Keepalive,
}

/// Subscribe to a stream via Server-Sent Events
///
/// This endpoint provides real-time updates for all events and runs in a stream.
/// Uses push-based pub/sub when available, with database polling as fallback.
///
/// Events emitted:
/// - `run:start` - A new run has started
/// - `run:stop` - A run completed successfully
/// - `run:fail` - A run failed
/// - `stream:complete` - The stream has completed (no more updates expected)
pub async fn subscribe_to_stream(
    State((db, _storage, _conf, stream_pubsub)): State<ApiStateData>,
    Extension(auth): Extension<AuthContext>,
    Extension(api_key): Extension<ApiKey>,
    blob_store: Option<Extension<Option<Arc<BlobStore>>>>,
    Path(stream_id): Path<Uuid>,
    Query(params): Query<StreamSubscribeParams>,
) -> Result<
    Sse<impl Stream<Item = Result<SseEvent, Infallible>>>,
    (StatusCode, Json<ApiErrorResponse>),
> {
    let blob_store = blob_store_from_ext(blob_store);
    let env_id = auth.env_id();

    // Verify the stream exists and belongs to this environment
    let _stream = get_stream_for_env(&db, &stream_id, env_id).await?;

    // Permission check for scoped credentials (sessions and service keys)
    if !auth.is_api_key() {
        let resource = format!("stream:{}", stream_id);
        if !auth.has_permission(&resource, actions::READ)
            && !auth.has_permission("stream:*", actions::READ)
        {
            return Err((
                StatusCode::FORBIDDEN,
                Json(ApiErrorResponse::new(
                    "forbidden",
                    "Credential does not have read access to this stream",
                )),
            ));
        }
    }

    // Optional: resolve project filter to project_id
    let project_filter: Option<Uuid> = if let Some(ref project_name_or_id) = params.project {
        let project = get_and_verify_project(&db, &api_key, project_name_or_id).await?;
        Some(project.project_id)
    } else {
        None
    };
    let db_clone = db.clone();
    let blob_store_clone = blob_store.clone();

    // Try to subscribe to pub/sub for real-time updates
    let subscriber: Option<Box<dyn StreamSubscriber>> = if let Some(ref pubsub) = stream_pubsub {
        match pubsub.subscribe_in_env(env_id, stream_id).await {
            Ok(sub) => {
                tracing::debug!("SSE handler subscribed to stream pub/sub for {}", stream_id);
                Some(sub)
            }
            Err(e) => {
                tracing::debug!(
                    "SSE handler falling back to polling for stream {} (pub/sub error: {})",
                    stream_id,
                    e
                );
                None
            }
        }
    } else {
        tracing::debug!(
            "SSE handler using polling for stream {} (no pub/sub configured)",
            stream_id
        );
        None
    };

    // Create the SSE stream with push from Redis Streams + poll fallback for run status
    let stream = async_stream::stream! {
        let mut seen_run_ids: ahash::AHashSet<Uuid> = ahash::AHashSet::new();
        let mut completed_run_ids: ahash::AHashSet<Uuid> = ahash::AHashSet::new();

        // Poll interval for run status updates (stream:data comes via Redis Streams push)
        let poll_interval = if subscriber.is_some() {
            tokio::time::Duration::from_secs(1) // Safety net poll every 1s when using pub/sub
        } else {
            tokio::time::Duration::from_millis(100) // Poll every 100ms without pub/sub
        };

        let stream_timeout = tokio::time::Duration::from_secs(300); // 5 minute timeout
        let start_time = tokio::time::Instant::now();

        // Wrap subscriber in Option for ownership in the loop
        let mut subscriber = subscriber;

        loop {
            // Check for timeout
            if start_time.elapsed() > stream_timeout {
                let complete_event = StreamEvent::StreamComplete { stream_id };
                if let Ok(json) = serde_json::to_string(&complete_event) {
                    yield Ok(SseEvent::default().event("stream:complete").data(json));
                }
                break;
            }

            // Use tokio::select! to wait for either pub/sub event or poll interval
            tokio::select! {
                // Branch 1: Receive push event from pub/sub (if available)
                pubsub_event = async {
                    if let Some(ref mut sub) = subscriber {
                        sub.next().await
                    } else {
                        // No subscriber - this branch never completes
                        std::future::pending::<StreamNext>().await
                    }
                } => {
                    match pubsub_event {
                        StreamNext::Event(event) => {
                        // Convert pub/sub event to SSE event
                        match event {
                            PubSubEvent::RunStart { run_id, stream_id: event_stream_id, event_id: _, .. } => {
                                if event_stream_id != stream_id {
                                    continue;
                                }

                                // For run:start, we need to fetch full run details from DB
                                if !seen_run_ids.contains(&run_id)
                                    && let Ok(run) = Run::get_run(&db_clone, &run_id).await {
                                        if run.env_id != env_id || run.stream_id != stream_id {
                                            continue;
                                        }

                                        seen_run_ids.insert(run_id);

                                        // Apply project filter
                                        if let Some(filter_project_id) = project_filter
                                            && run.project_id != Some(filter_project_id) {
                                                continue;
                                            }

                                        let run_response = run_to_response(&db_clone, blob_store_clone.as_ref(), &run).await;
                                        let sse_event = StreamEvent::RunStart { run: run_response };
                                        if let Ok(json) = serde_json::to_string(&sse_event) {
                                            tracing::debug!("SSE push: run:start for {}", run_id);
                                            yield Ok(SseEvent::default().event("run:start").data(json));
                                        }
                                    }
                            }
                            PubSubEvent::RunStop { run_id, stream_id: event_stream_id, event_id: _, result: _, .. } => {
                                if event_stream_id != stream_id {
                                    continue;
                                }

                                if !completed_run_ids.contains(&run_id)
                                    && let Ok(run) = Run::get_run(&db_clone, &run_id).await {
                                        if run.env_id != env_id || run.stream_id != stream_id {
                                            continue;
                                        }

                                        completed_run_ids.insert(run_id);

                                        // Apply project filter
                                        if let Some(filter_project_id) = project_filter
                                            && run.project_id != Some(filter_project_id) {
                                                continue;
                                            }

                                        let run_response = run_to_response(&db_clone, blob_store_clone.as_ref(), &run).await;
                                        let sse_event = StreamEvent::RunStop { run: run_response };
                                        if let Ok(json) = serde_json::to_string(&sse_event) {
                                            tracing::debug!("SSE push: run:stop for {}", run_id);
                                            yield Ok(SseEvent::default().event("run:stop").data(json));
                                        }
                                    }
                            }
                            PubSubEvent::RunFail { run_id, stream_id: event_stream_id, event_id: _, error: _, .. } => {
                                if event_stream_id != stream_id {
                                    continue;
                                }

                                if !completed_run_ids.contains(&run_id)
                                    && let Ok(run) = Run::get_run(&db_clone, &run_id).await {
                                        if run.env_id != env_id || run.stream_id != stream_id {
                                            continue;
                                        }

                                        completed_run_ids.insert(run_id);

                                        // Apply project filter
                                        if let Some(filter_project_id) = project_filter
                                            && run.project_id != Some(filter_project_id) {
                                                continue;
                                            }

                                        let run_response = run_to_response(&db_clone, blob_store_clone.as_ref(), &run).await;
                                        let sse_event = StreamEvent::RunFail { run: run_response };
                                        if let Ok(json) = serde_json::to_string(&sse_event) {
                                            tracing::debug!("SSE push: run:fail for {}", run_id);
                                            yield Ok(SseEvent::default().event("run:fail").data(json));
                                        }
                                    }
                            }
                            PubSubEvent::RunCancel { run_id, stream_id: event_stream_id, event_id: _, reason: _, .. } => {
                                if event_stream_id != stream_id {
                                    continue;
                                }

                                if !completed_run_ids.contains(&run_id)
                                    && let Ok(run) = Run::get_run(&db_clone, &run_id).await {
                                        if run.env_id != env_id || run.stream_id != stream_id {
                                            continue;
                                        }

                                        completed_run_ids.insert(run_id);

                                        // Apply project filter
                                        if let Some(filter_project_id) = project_filter
                                            && run.project_id != Some(filter_project_id) {
                                                continue;
                                            }

                                        let run_response = run_to_response(&db_clone, blob_store_clone.as_ref(), &run).await;
                                        let sse_event = StreamEvent::RunCancel { run: run_response };
                                        if let Ok(json) = serde_json::to_string(&sse_event) {
                                            tracing::debug!("SSE push: run:cancel for {}", run_id);
                                            yield Ok(SseEvent::default().event("run:cancel").data(json));
                                        }
                                    }
                            }
                            // Handle stream:data events - these come with full payload, no DB lookup needed
                            PubSubEvent::StreamData { stream_data_id, run_id, env_id: event_env_id, stream_id: event_stream_id, data_type, payload } => {
                                if event_stream_id != stream_id || event_env_id != Some(env_id) {
                                    continue;
                                }

                                // Stream data is emitted immediately - no deduplication needed
                                // The payload is already in the pub/sub message
                                let sse_event = StreamEvent::StreamData {
                                    stream_data_id,
                                    run_id,
                                    data_type,
                                    payload,
                                };
                                if let Ok(json) = serde_json::to_string(&sse_event) {
                                    tracing::debug!("SSE push: stream:data {} for run {}", stream_data_id, run_id);
                                    yield Ok(SseEvent::default().event("stream:data").data(json));
                                }
                            }
                            PubSubEvent::TaskMessage { .. } => {}
                        }
                        }
                        StreamNext::Idle => {}
                        StreamNext::Closed => {
                            // Subscriber closed - disable pub/sub and continue with polling
                            tracing::debug!("SSE pub/sub subscription closed, falling back to polling");
                            subscriber = None;
                        }
                    }
                }

                // Branch 2: Poll database on interval (fallback/safety net)
                _ = tokio::time::sleep(poll_interval) => {
                    // Poll for runs on this stream
                    match Run::get_runs_by_stream(&db_clone, &stream_id, &env_id, Some(100), Some(0)).await {
                        Ok(runs) => {
                            for run in runs {
                                // Apply project filter if specified
                                if let Some(filter_project_id) = project_filter
                                    && run.project_id != Some(filter_project_id) {
                                        continue;
                                    }

                                // Check if this is a new run we haven't seen
                                if !seen_run_ids.contains(&run.run_id) {
                                    seen_run_ids.insert(run.run_id);

                                    let run_response = run_to_response(&db_clone, blob_store_clone.as_ref(), &run).await;
                                    let event = StreamEvent::RunStart { run: run_response };
                                    if let Ok(json) = serde_json::to_string(&event) {
                                        tracing::debug!("SSE poll: run:start for {}", run.run_id);
                                        yield Ok(SseEvent::default().event("run:start").data(json));
                                    }
                                }

                                // Check if run has completed since we last saw it
                                if run.stop_time.is_some() && !completed_run_ids.contains(&run.run_id) {
                                    completed_run_ids.insert(run.run_id);

                                    let run_response = run_to_response(&db_clone, blob_store_clone.as_ref(), &run).await;

                                    let (event_type, event) = match run.status.as_str() {
                                        "failed" => ("run:fail", StreamEvent::RunFail { run: run_response }),
                                        "cancelled" => ("run:cancel", StreamEvent::RunCancel { run: run_response }),
                                        _ => ("run:stop", StreamEvent::RunStop { run: run_response }),
                                    };

                                    if let Ok(json) = serde_json::to_string(&event) {
                                        tracing::debug!("SSE poll: {} for {}", event_type, run.run_id);
                                        yield Ok(SseEvent::default().event(event_type).data(json));
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            tracing::error!("Error polling runs for stream {}: {}", stream_id, e);
                        }
                    }
                    // Note: stream:data events are delivered via Redis Streams push (not database polling)
                    // They are ephemeral and not persisted to the database
                }
            }
        }
    };

    // KeepAlive sends a comment every 15 seconds by default, preventing ALB idle timeout
    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

/// POST /v1/streams/{stream_id}/subscribe — Streamable HTTP variant
///
/// Same as the GET variant but triggered via POST. This allows clients that
/// prefer the Streamable HTTP pattern (request → SSE response on the same
/// connection) to subscribe without maintaining a separate persistent stream.
///
/// The response is identical: an SSE event stream with run lifecycle events.
pub async fn subscribe_to_stream_post(
    State(state): State<ApiStateData>,
    Extension(auth): Extension<AuthContext>,
    Extension(api_key): Extension<ApiKey>,
    blob_store: Option<Extension<Option<Arc<BlobStore>>>>,
    Path(stream_id): Path<Uuid>,
    Query(params): Query<StreamSubscribeParams>,
) -> Result<
    Sse<impl Stream<Item = Result<SseEvent, Infallible>>>,
    (StatusCode, Json<ApiErrorResponse>),
> {
    // Delegate to the same implementation as the GET handler
    subscribe_to_stream(
        State(state),
        Extension(auth),
        Extension(api_key),
        blob_store,
        Path(stream_id),
        Query(params),
    )
    .await
}

/// Helper to convert a Run database record to RunResponse, rehydrating any
/// spilled BlobRefs in the run result before it goes out over SSE.
async fn run_to_response(
    db: &hot::db::DatabasePool,
    blob_store: Option<&Arc<BlobStore>>,
    run: &Run,
) -> RunResponse {
    let result = rehydrate_payload_json(db, blob_store, run.env_id, run.result.clone()).await;
    RunResponse {
        run_id: run.run_id,
        env_id: run.env_id,
        stream_id: run.stream_id,
        build_id: run.build_id,
        run_type: run.run_type.clone(),
        status: run.status.clone(),
        start_time: run.start_time,
        stop_time: run.stop_time,
        origin_run_id: run.origin_run_id,
        event_id: run.event_id,
        result,
        project_id: run.project_id,
        project_name: run.project_name.clone(),
        retry_attempt: run.retry_attempt,
        next_retry_at: run.next_retry_at,
    }
}

// ============================================================================
// Subscribe with Event (Atomic SSE + Publish)
// ============================================================================

/// Request body for subscribe-with-event endpoint
#[derive(Debug, Deserialize, ToSchema)]
pub struct SubscribeWithEventRequest {
    /// The event type to publish
    pub event_type: String,
    /// The event data payload
    pub event_data: serde_json::Value,
    /// Optional stream ID to add this event to an existing stream.
    /// If not provided, a new stream will be created.
    #[serde(default)]
    pub stream_id: Option<Uuid>,
}

/// Query parameters for subscribe-with-event endpoint
#[derive(Debug, Deserialize, ToSchema)]
pub struct SubscribeWithEventParams {
    /// Optional project filter - only include runs from this project
    pub project: Option<String>,
}

/// SSE event for confirming event publication
#[derive(Debug, Serialize, ToSchema)]
pub struct EventPublishedEvent {
    pub event_id: Uuid,
    pub stream_id: Uuid,
    pub event_type: String,
}

/// Subscribe to a stream and publish an event atomically
///
/// This endpoint establishes the SSE subscription BEFORE publishing the event,
/// guaranteeing that no events are missed. This eliminates race conditions
/// present in the separate publish-then-subscribe pattern.
///
/// The endpoint:
/// 1. Creates/validates the stream (ensuring it belongs to this API key)
/// 2. Establishes the pub/sub subscription
/// 3. Publishes the event
/// 4. Streams all events (run:start, stream:data, run:stop, etc.)
///
/// The connection stays open until timeout (5 minutes) or client disconnect,
/// allowing multiple events and runs on the same stream.
///
/// Events emitted:
/// - `event:published` - Confirms the event was published (first event)
/// - `run:start` - A new run has started
/// - `stream:data` - Real-time streaming data from the run
/// - `run:stop` - A run completed successfully
/// - `run:fail` - A run failed
/// - `run:cancel` - A run was cancelled
/// - `stream:complete` - The stream subscription timed out
pub async fn subscribe_with_event(
    State((db, _storage, conf, stream_pubsub)): State<ApiStateData>,
    Extension(auth): Extension<AuthContext>,
    Extension(api_key): Extension<ApiKey>,
    blob_store: Option<Extension<Option<Arc<BlobStore>>>>,
    OptionalAccessId(access_id): OptionalAccessId,
    Query(params): Query<SubscribeWithEventParams>,
    Json(req): Json<SubscribeWithEventRequest>,
) -> Result<
    Sse<impl Stream<Item = Result<SseEvent, Infallible>>>,
    (StatusCode, Json<ApiErrorResponse>),
> {
    let blob_store = blob_store_from_ext(blob_store);
    let env_id = auth.env_id();

    // Step 1: Determine stream_id - use provided or create new
    let stream_id = req.stream_id.unwrap_or_else(Uuid::now_v7);

    // Step 2: If stream_id was provided, verify it belongs to this principal's environment
    if req.stream_id.is_some() {
        match lookup_stream_for_env(&db, &stream_id, env_id).await? {
            StreamLookup::Found(_) => {}
            StreamLookup::Missing => {
                // If the stream doesn't exist yet, that's OK - we'll create it below.
            }
            StreamLookup::WrongEnv => return Err(stream_not_found()),
        }
    }

    // Permission check for scoped credentials (sessions and service keys)
    if !auth.is_api_key() {
        let stream_resource = format!("stream:{}", stream_id);
        if !auth.has_permission(&stream_resource, actions::READ)
            && !auth.has_permission("stream:*", actions::READ)
        {
            return Err((
                StatusCode::FORBIDDEN,
                Json(ApiErrorResponse::new(
                    "forbidden",
                    "Credential does not have read access to this stream",
                )),
            ));
        }
        // Also check event create permission since this endpoint publishes an event
        if !auth.has_permission("event:*", actions::CREATE)
            && !auth.has_permission(&format!("event:{}", req.event_type), actions::CREATE)
        {
            return Err((
                StatusCode::FORBIDDEN,
                Json(ApiErrorResponse::new(
                    "forbidden",
                    "Credential does not have create permission for events",
                )),
            ));
        }
    }

    // Step 3: Create the stream record in the database BEFORE subscribing
    // This ensures the stream exists for the subscription and avoids race conditions
    DbStream::create_or_get_stream(&db, stream_id, env_id)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiErrorResponse::internal_error(&format!(
                    "Failed to create stream: {}",
                    e
                ))),
            )
        })?;

    // Step 4: Subscribe to pub/sub BEFORE publishing the event
    // This is the key to eliminating race conditions
    let subscriber: Option<Box<dyn StreamSubscriber>> = if let Some(ref pubsub) = stream_pubsub {
        match pubsub.subscribe_in_env(env_id, stream_id).await {
            Ok(sub) => {
                tracing::debug!(
                    "subscribe_with_event: subscribed to stream {} before publishing",
                    stream_id
                );
                Some(sub)
            }
            Err(e) => {
                tracing::debug!(
                    "subscribe_with_event: falling back to polling for stream {} (pub/sub error: {})",
                    stream_id,
                    e
                );
                None
            }
        }
    } else {
        tracing::debug!(
            "subscribe_with_event: using polling for stream {} (no pub/sub configured)",
            stream_id
        );
        None
    };

    // Step 5: NOW publish the event - subscriber is ready to receive all events
    let published = publish_event_internal(
        &db,
        &conf,
        &api_key,
        &req.event_type,
        &req.event_data,
        stream_id,
        access_id,
    )
    .await?;

    tracing::info!(
        "subscribe_with_event: event {} ({}) published to stream {}",
        published.event_id,
        published.event_type,
        stream_id
    );

    // Optional: resolve project filter to project_id
    let project_filter: Option<Uuid> = if let Some(ref project_name_or_id) = params.project {
        let project = get_and_verify_project(&db, &api_key, project_name_or_id).await?;
        Some(project.project_id)
    } else {
        None
    };
    let db_clone = db.clone();
    let blob_store_clone = blob_store.clone();
    let event_id = published.event_id;
    let event_type = published.event_type.clone();

    // Step 6: Create the SSE stream
    let stream = async_stream::stream! {
        // First event: confirm the event was published
        let published_event = EventPublishedEvent {
            event_id,
            stream_id,
            event_type,
        };
        if let Ok(json) = serde_json::to_string(&serde_json::json!({
            "type": "event:published",
            "event_id": event_id,
            "stream_id": stream_id,
            "event_type": published_event.event_type,
        })) {
            yield Ok(SseEvent::default().event("event:published").data(json));
        }

        // Now run the same subscription loop as subscribe_to_stream
        let mut seen_run_ids: ahash::AHashSet<Uuid> = ahash::AHashSet::new();
        let mut completed_run_ids: ahash::AHashSet<Uuid> = ahash::AHashSet::new();

        // Poll interval for run status updates (stream:data comes via pub/sub push)
        let poll_interval = if subscriber.is_some() {
            tokio::time::Duration::from_secs(1) // Safety net poll every 1s when using pub/sub
        } else {
            tokio::time::Duration::from_millis(100) // Poll every 100ms without pub/sub
        };

        let stream_timeout = tokio::time::Duration::from_secs(300); // 5 minute timeout
        let start_time = tokio::time::Instant::now();

        // Wrap subscriber in Option for ownership in the loop
        let mut subscriber = subscriber;

        loop {
            // Check for timeout
            if start_time.elapsed() > stream_timeout {
                let complete_event = StreamEvent::StreamComplete { stream_id };
                if let Ok(json) = serde_json::to_string(&complete_event) {
                    yield Ok(SseEvent::default().event("stream:complete").data(json));
                }
                break;
            }

            // Use tokio::select! to wait for either pub/sub event or poll interval
            tokio::select! {
                // Branch 1: Receive push event from pub/sub (if available)
                pubsub_event = async {
                    if let Some(ref mut sub) = subscriber {
                        sub.next().await
                    } else {
                        // No subscriber - this branch never completes
                        std::future::pending::<StreamNext>().await
                    }
                } => {
                    match pubsub_event {
                        StreamNext::Event(event) => {
                        // Convert pub/sub event to SSE event
                        match event {
                            PubSubEvent::RunStart { run_id, stream_id: event_stream_id, event_id: _, .. } => {
                                if event_stream_id != stream_id {
                                    continue;
                                }

                                // For run:start, we need to fetch full run details from DB
                                if !seen_run_ids.contains(&run_id)
                                    && let Ok(run) = Run::get_run(&db_clone, &run_id).await {
                                        if run.env_id != env_id || run.stream_id != stream_id {
                                            continue;
                                        }

                                        seen_run_ids.insert(run_id);

                                        // Apply project filter
                                        if let Some(filter_project_id) = project_filter
                                            && run.project_id != Some(filter_project_id) {
                                                continue;
                                            }

                                        let run_response = run_to_response(&db_clone, blob_store_clone.as_ref(), &run).await;
                                        let sse_event = StreamEvent::RunStart { run: run_response };
                                        if let Ok(json) = serde_json::to_string(&sse_event) {
                                            tracing::debug!("SSE push: run:start for {}", run_id);
                                            yield Ok(SseEvent::default().event("run:start").data(json));
                                        }
                                    }
                            }
                            PubSubEvent::RunStop { run_id, stream_id: event_stream_id, event_id: _, result: _, .. } => {
                                if event_stream_id != stream_id {
                                    continue;
                                }

                                if !completed_run_ids.contains(&run_id)
                                    && let Ok(run) = Run::get_run(&db_clone, &run_id).await {
                                        if run.env_id != env_id || run.stream_id != stream_id {
                                            continue;
                                        }

                                        completed_run_ids.insert(run_id);

                                        // Apply project filter
                                        if let Some(filter_project_id) = project_filter
                                            && run.project_id != Some(filter_project_id) {
                                                continue;
                                            }

                                        let run_response = run_to_response(&db_clone, blob_store_clone.as_ref(), &run).await;
                                        let sse_event = StreamEvent::RunStop { run: run_response };
                                        if let Ok(json) = serde_json::to_string(&sse_event) {
                                            tracing::debug!("SSE push: run:stop for {}", run_id);
                                            yield Ok(SseEvent::default().event("run:stop").data(json));
                                        }
                                    }
                            }
                            PubSubEvent::RunFail { run_id, stream_id: event_stream_id, event_id: _, error: _, .. } => {
                                if event_stream_id != stream_id {
                                    continue;
                                }

                                if !completed_run_ids.contains(&run_id)
                                    && let Ok(run) = Run::get_run(&db_clone, &run_id).await {
                                        if run.env_id != env_id || run.stream_id != stream_id {
                                            continue;
                                        }

                                        completed_run_ids.insert(run_id);

                                        // Apply project filter
                                        if let Some(filter_project_id) = project_filter
                                            && run.project_id != Some(filter_project_id) {
                                                continue;
                                            }

                                        let run_response = run_to_response(&db_clone, blob_store_clone.as_ref(), &run).await;
                                        let sse_event = StreamEvent::RunFail { run: run_response };
                                        if let Ok(json) = serde_json::to_string(&sse_event) {
                                            tracing::debug!("SSE push: run:fail for {}", run_id);
                                            yield Ok(SseEvent::default().event("run:fail").data(json));
                                        }
                                    }
                            }
                            PubSubEvent::RunCancel { run_id, stream_id: event_stream_id, event_id: _, reason: _, .. } => {
                                if event_stream_id != stream_id {
                                    continue;
                                }

                                if !completed_run_ids.contains(&run_id)
                                    && let Ok(run) = Run::get_run(&db_clone, &run_id).await {
                                        if run.env_id != env_id || run.stream_id != stream_id {
                                            continue;
                                        }

                                        completed_run_ids.insert(run_id);

                                        // Apply project filter
                                        if let Some(filter_project_id) = project_filter
                                            && run.project_id != Some(filter_project_id) {
                                                continue;
                                            }

                                        let run_response = run_to_response(&db_clone, blob_store_clone.as_ref(), &run).await;
                                        let sse_event = StreamEvent::RunCancel { run: run_response };
                                        if let Ok(json) = serde_json::to_string(&sse_event) {
                                            tracing::debug!("SSE push: run:cancel for {}", run_id);
                                            yield Ok(SseEvent::default().event("run:cancel").data(json));
                                        }
                                    }
                            }
                            // Handle stream:data events - these come with full payload, no DB lookup needed
                            PubSubEvent::StreamData { stream_data_id, run_id, env_id: event_env_id, stream_id: event_stream_id, data_type, payload } => {
                                if event_stream_id != stream_id || event_env_id != Some(env_id) {
                                    continue;
                                }

                                // Stream data is emitted immediately - no deduplication needed
                                // The payload is already in the pub/sub message
                                let sse_event = StreamEvent::StreamData {
                                    stream_data_id,
                                    run_id,
                                    data_type,
                                    payload,
                                };
                                if let Ok(json) = serde_json::to_string(&sse_event) {
                                    tracing::debug!("SSE push: stream:data {} for run {}", stream_data_id, run_id);
                                    yield Ok(SseEvent::default().event("stream:data").data(json));
                                }
                            }
                            PubSubEvent::TaskMessage { .. } => {}
                        }
                        }
                        StreamNext::Idle => {}
                        StreamNext::Closed => {
                            // Subscriber closed - disable pub/sub and continue with polling
                            tracing::debug!("SSE pub/sub subscription closed, falling back to polling");
                            subscriber = None;
                        }
                    }
                }

                // Branch 2: Poll database on interval (fallback/safety net)
                _ = tokio::time::sleep(poll_interval) => {
                    // Poll for runs on this stream
                    match Run::get_runs_by_stream(&db_clone, &stream_id, &env_id, Some(100), Some(0)).await {
                        Ok(runs) => {
                            for run in runs {
                                // Apply project filter if specified
                                if let Some(filter_project_id) = project_filter
                                    && run.project_id != Some(filter_project_id) {
                                        continue;
                                    }

                                // Check if this is a new run we haven't seen
                                if !seen_run_ids.contains(&run.run_id) {
                                    seen_run_ids.insert(run.run_id);

                                    let run_response = run_to_response(&db_clone, blob_store_clone.as_ref(), &run).await;
                                    let event = StreamEvent::RunStart { run: run_response };
                                    if let Ok(json) = serde_json::to_string(&event) {
                                        tracing::debug!("SSE poll: run:start for {}", run.run_id);
                                        yield Ok(SseEvent::default().event("run:start").data(json));
                                    }
                                }

                                // Check if run has completed since we last saw it
                                if run.stop_time.is_some() && !completed_run_ids.contains(&run.run_id) {
                                    completed_run_ids.insert(run.run_id);

                                    let run_response = run_to_response(&db_clone, blob_store_clone.as_ref(), &run).await;

                                    let (event_type, event) = match run.status.as_str() {
                                        "failed" => ("run:fail", StreamEvent::RunFail { run: run_response }),
                                        "cancelled" => ("run:cancel", StreamEvent::RunCancel { run: run_response }),
                                        _ => ("run:stop", StreamEvent::RunStop { run: run_response }),
                                    };

                                    if let Ok(json) = serde_json::to_string(&event) {
                                        tracing::debug!("SSE poll: {} for {}", event_type, run.run_id);
                                        yield Ok(SseEvent::default().event(event_type).data(json));
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            tracing::error!("Error polling runs for stream {}: {}", stream_id, e);
                        }
                    }
                    // Note: stream:data events are delivered via pub/sub push (not database polling)
                    // They are ephemeral and not persisted to the database
                }
            }
        }
    };

    // KeepAlive sends a comment every 15 seconds by default, preventing ALB idle timeout
    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

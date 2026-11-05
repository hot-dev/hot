//! Event pub/sub handlers

use ahash::{AHashMap, AHashSet};
use axum::{
    Extension, Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use hot::data::msg::Message;
use hot::db::DatabasePool;
use hot::db::api_key::ApiKey;
use hot::db::event::{EnqueueClaim, Event, EventError};
use hot::db::stream::Stream as DbStream;
use hot::permission::actions;
use hot::queue::Queue;
use hot::val::Val;
use once_cell::sync::OnceCell;
use std::str::FromStr;
use std::sync::Arc;
use uuid::Uuid;

use super::{ListQueryParams, blob_store_from_ext, rehydrate_payload_json};
use crate::ApiStateData;
use crate::access_log::OptionalAccessId;
use crate::auth::AuthContext;
use crate::models::*;
use hot::blob::BlobStore;

/// The queue name that API-published events are sent to.
/// This MUST match the worker's event queue name ("hot:event").
const API_EVENT_QUEUE_NAME: &str = "hot:event";
const RESERVED_EVENT_PREFIX: &str = "hot:";
const EXTERNAL_CALL_EVENT_TYPE: &str = "hot:call";

/// How long a publish that names its event holds the event's enqueue. A resend
/// takes over only after this, so it has to outlast the slowest
/// `enqueue_with_retries` on a queue that hangs instead of failing.
const ENQUEUE_LEASE_SECONDS: i64 = 120;
/// When a resend that found the enqueue held should try again.
const PUBLISH_IN_PROGRESS_RETRY_SECONDS: u64 = 5;

static API_EVENT_QUEUE: OnceCell<Arc<hot::queue::ProcessingQueue<Message>>> = OnceCell::new();

fn get_event_queue(
    conf: &Val,
) -> Result<&'static Arc<hot::queue::ProcessingQueue<Message>>, String> {
    API_EVENT_QUEUE.get_or_try_init(|| {
        let queue_type_str = conf.get_str_or_default("queue.type", "sqlite");
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

        let queue = hot::queue::ProcessingQueue::<Message>::new_with_cluster(
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

type PublishError = (StatusCode, Json<ApiErrorResponse>);

fn internal_error(error: impl ToString) -> PublishError {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(ApiErrorResponse::internal_error(&error.to_string())),
    )
}

pub(super) fn require_external_event_type(
    event_type: &str,
) -> Result<(), (StatusCode, Json<ApiErrorResponse>)> {
    if event_type.starts_with(RESERVED_EVENT_PREFIX) && event_type != EXTERNAL_CALL_EVENT_TYPE {
        return Err((
            StatusCode::FORBIDDEN,
            Json(ApiErrorResponse::new(
                "forbidden",
                "Event types beginning with 'hot:' are reserved for internal control flows",
            )),
        ));
    }
    Ok(())
}

/// Internal function to publish an event - reused by both publish_event and subscribe_with_event
///
/// This handles:
/// 1. Creating the event record in the database
/// 2. Setting up the execution context
/// 3. Enqueuing the event for worker processing
///
/// The stream must already exist before calling this function.
// Keep the effective credential separate from historical actor attribution.
#[allow(clippy::too_many_arguments)]
pub async fn publish_event_internal(
    db: &DatabasePool,
    conf: &Val,
    auth: &AuthContext,
    actor_user_id: Uuid,
    event_type: &str,
    event_data: &serde_json::Value,
    stream_id: Uuid,
    access_id: Option<Uuid>,
) -> Result<PublishedEvent, (StatusCode, Json<ApiErrorResponse>)> {
    let queue = get_event_queue(conf).map_err(internal_error)?;

    publish_event_to_queue(
        queue.as_ref(),
        db,
        auth,
        actor_user_id,
        event_type,
        event_data,
        stream_id,
        access_id,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn publish_event_to_queue(
    queue: &dyn Queue<Message>,
    db: &DatabasePool,
    auth: &AuthContext,
    actor_user_id: Uuid,
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
        &auth.env_id(),
        &stream_id,
        event_type,
        event_data,
        event_time,
        &actor_user_id,
        access_id.as_ref(),
    )
    .await
    .map_err(internal_error)?;

    enqueue_event(
        queue,
        db,
        auth,
        actor_user_id,
        &StoredEvent {
            event_id,
            stream_id,
            event_type,
            event_data,
            event_time,
            access_id,
        },
        false,
    )
    .await?;

    Ok(PublishedEvent {
        event_id,
        stream_id,
        event_type: event_type.to_string(),
        event_time,
    })
}

/// An event's row, as its enqueue needs it.
struct StoredEvent<'a> {
    event_id: Uuid,
    stream_id: Uuid,
    event_type: &'a str,
    event_data: &'a serde_json::Value,
    event_time: chrono::DateTime<chrono::Utc>,
    access_id: Option<Uuid>,
}

/// Enqueue a stored event to the hot:event queue for worker processing.
/// `may_repeat` says another message of the event may have reached a worker.
async fn enqueue_event(
    queue: &dyn Queue<Message>,
    db: &DatabasePool,
    auth: &AuthContext,
    actor_user_id: Uuid,
    event: &StoredEvent<'_>,
    may_repeat: bool,
) -> Result<(), PublishError> {
    let StoredEvent {
        event_id,
        stream_id,
        event_type,
        event_data,
        event_time,
        access_id,
    } = *event;

    // Get org_id for execution context
    let env = hot::db::Env::get_env(db, &auth.env_id())
        .await
        .map_err(|e| internal_error(format!("Failed to get environment: {e}")))?;

    // Create execution context for the event
    let run_id = Uuid::now_v7();
    let execution_context = hot::lang::event::ExecutionContext {
        run_id,
        stream_id,
        run_type_id: hot::db::run::RunType::Event.as_id(),
        env_id: Some(auth.env_id()),
        env_name: None, // Will be populated later if needed
        user_id: Some(actor_user_id),
        org_id: Some(env.org_id),
        org_slug: None,     // Will be populated later if needed
        build_id: None,     // Will be resolved by worker
        build_hash: None,   // Will be populated later if needed
        project_id: None,   // Will be populated later if needed
        project_name: None, // Will be populated later if needed
        event_id: Some(event_id),
        origin_run_id: None,
        retry_attempt: 0,
        max_retries: 0,
        retry_delay_ms: 0,
        secret_keys: AHashSet::new(), // Will be populated from ctx metadata
        secret_value_hashes: AHashSet::new(),
        secret_values: Default::default(),
        access_id,
        agent_type: None,
        event_handler_fn: None,
        queue_timing: None,
        // Enqueue-only producer context; the worker stamps deadline_at at admission
        deadline_at: None,
        trace_calls: true,
    };

    // Create the event message
    let event_data_val: Val = serde_json::from_value(event_data.clone()).unwrap_or(Val::Null);
    let hot_event = hot::lang::event::Event {
        event_id,
        env_id: auth.env_id(),
        stream_id,
        event_type: event_type.to_string(),
        event_data: event_data_val,
        event_time,
        // API-published events have no target project (uses default routing)
        target_project_id: None,
        target_project_name: None,
    };

    let mut head = AHashMap::from_iter([
        ("env_id".to_string(), auth.env_id().to_string()),
        ("event_type".to_string(), event_type.to_string()),
    ]);
    if may_repeat {
        head.insert(
            hot::lang::event::MAY_REPEAT_HEAD.to_string(),
            "true".to_string(),
        );
    }
    let event_message = hot::lang::event::EventMessage {
        id: event_id,
        head,
        body: hot::lang::event::EventMessageBody {
            event: hot_event,
            execution_context,
        },
    };

    // Convert to unified Message format and enqueue
    let message: Message = event_message.into();

    // The event row is already written, so a failed enqueue would lose the
    // event: try a few times, as a handler's `send()` does.
    hot::queue::enqueue_with_retries(queue, message, &hot::queue::PUBLISH_RETRY_DELAYS)
        .await
        .map_err(|e| internal_error(format!("Failed to enqueue event: {e}")))?;

    tracing::info!(
        "Event {} ({}) published and queued for processing",
        event_id,
        event_type
    );

    Ok(())
}

/// Enqueue an event whose lease this publish holds, and settle the lease. A
/// lease left behind only makes a later resend look again.
async fn enqueue_leased_event(
    queue: &dyn Queue<Message>,
    db: &DatabasePool,
    auth: &AuthContext,
    actor_user_id: Uuid,
    event: &StoredEvent<'_>,
) -> Result<(), PublishError> {
    let event_id = event.event_id;
    let enqueued = enqueue_event(queue, db, auth, actor_user_id, event, true).await;
    let settled = match enqueued {
        Ok(()) => Event::finish_enqueue(db, &event_id).await,
        Err(_) => Event::release_enqueue(db, &event_id, chrono::Utc::now()).await,
    };
    if let Err(error) = settled {
        tracing::warn!(%event_id, "Could not settle the event's enqueue lease: {error}");
    }
    enqueued
}

fn enqueue_lease_end(now: chrono::DateTime<chrono::Utc>) -> chrono::DateTime<chrono::Utc> {
    now + chrono::Duration::seconds(ENQUEUE_LEASE_SECONDS)
}

/// The answer to an `event_id` that names another event. One answer covers an
/// id in another environment too, so it says no more about that environment
/// than that the id is taken.
fn event_id_conflict() -> PublishError {
    (
        StatusCode::CONFLICT,
        Json(ApiErrorResponse::new(
            "event_id_conflict",
            "This event_id is in use by a different event",
        )),
    )
}

/// Answer a publish whose `event_id` may already name an event. Returns false
/// when no event visible to this caller has the id.
///
/// The same publish gets the first one's answer. It can only give that once
/// the event is enqueued, so it finishes what an earlier publish left: that
/// one may have failed or died after writing the row, and then the caller's
/// resend is the only thing left that can get the event to the workers.
async fn resume_publish(
    queue: &dyn Queue<Message>,
    db: &DatabasePool,
    auth: &AuthContext,
    req: &PublishEventRequest,
    event_id: Uuid,
) -> Result<bool, PublishError> {
    let event = match Event::get_event(db, &event_id).await {
        Ok(event) => event,
        Err(EventError::NotFound) => return Ok(false),
        Err(e) => return Err(internal_error(e)),
    };
    if event.env_id != auth.env_id() || !is_same_publish(&event, req) {
        return Err(event_id_conflict());
    }

    let now = chrono::Utc::now();
    match Event::claim_enqueue(db, &event_id, now, enqueue_lease_end(now))
        .await
        .map_err(internal_error)?
    {
        EnqueueClaim::Settled => {}
        EnqueueClaim::Held => {
            return Err((
                StatusCode::SERVICE_UNAVAILABLE,
                Json(
                    ApiErrorResponse::new(
                        "event_publish_in_progress",
                        "An earlier publish of this event_id hasn't finished. Send it again shortly",
                    )
                    .with_retry_after(PUBLISH_IN_PROGRESS_RETRY_SECONDS),
                ),
            ));
        }
        // Runs mean the event reached a worker, and only its lease was left.
        EnqueueClaim::Claimed if event.handled => {
            Event::finish_enqueue(db, &event_id)
                .await
                .map_err(internal_error)?;
        }
        EnqueueClaim::Claimed => {
            enqueue_leased_event(
                queue,
                db,
                auth,
                event.created_by_user_id,
                &StoredEvent {
                    event_id,
                    stream_id: event.stream_id,
                    event_type: &event.event_type,
                    event_data: &event.event_data,
                    event_time: event.event_time,
                    access_id: event.access_id,
                },
            )
            .await?;
        }
    }
    Ok(true)
}

/// Whether `req` asks for the event already stored under its id. A request
/// without a `stream_id` leaves the stream to the first publish.
fn is_same_publish(event: &Event, req: &PublishEventRequest) -> bool {
    event.event_type == req.event_type
        && req
            .stream_id
            .is_none_or(|stream_id| stream_id == event.stream_id)
        && same_json(&event.event_data, &req.event_data)
}

/// JSON equality that stored data can't fail by accident: PostgreSQL's jsonb
/// keeps a number's value, not its spelling, so `1e2` comes back as `100`.
fn same_json(a: &serde_json::Value, b: &serde_json::Value) -> bool {
    use serde_json::Value;
    match (a, b) {
        (Value::Number(a), Value::Number(b)) => {
            a == b
                || number_value(&a.to_string())
                    .is_some_and(|value| Some(value) == number_value(&b.to_string()))
        }
        (Value::Array(a), Value::Array(b)) => {
            a.len() == b.len() && a.iter().zip(b).all(|(a, b)| same_json(a, b))
        }
        (Value::Object(a), Value::Object(b)) => {
            a.len() == b.len()
                && a.iter()
                    .all(|(key, a)| b.get(key).is_some_and(|b| same_json(a, b)))
        }
        _ => a == b,
    }
}

/// A JSON number as sign, significant digits and power of ten. `None` for an
/// exponent too large to read, which then only equals its own spelling.
fn number_value(number: &str) -> Option<(bool, String, i64)> {
    let (mantissa, exponent) = match number.split_once(['e', 'E']) {
        Some((mantissa, exponent)) => (mantissa, exponent.parse::<i64>().ok()?),
        None => (number, 0),
    };
    let (negative, mantissa) = match mantissa.strip_prefix('-') {
        Some(mantissa) => (true, mantissa),
        None => (false, mantissa),
    };
    let (whole, fraction) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    let digits = format!("{whole}{fraction}");
    let significant = digits.trim_start_matches('0').trim_end_matches('0');
    if significant.is_empty() {
        return Some((false, String::new(), 0));
    }
    let trailing_zeros = digits.len() - digits.trim_end_matches('0').len();
    let power = exponent
        .checked_sub(fraction.len() as i64)?
        .checked_add(trailing_zeros as i64)?;
    Some((negative, significant.to_string(), power))
}

/// Publish what `POST /v1/events` asked for and return the event's id.
async fn publish_request(
    queue: &dyn Queue<Message>,
    db: &DatabasePool,
    auth: &AuthContext,
    actor_user_id: Uuid,
    req: &PublishEventRequest,
    access_id: Option<Uuid>,
) -> Result<Uuid, PublishError> {
    if let Some(event_id) = req.event_id {
        if event_id.is_nil() {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(ApiErrorResponse::bad_request(
                    "event_id must not be the nil UUID",
                )),
            ));
        }
        // Before the stream: a resend that names none must not make one.
        if resume_publish(queue, db, auth, req, event_id).await? {
            return Ok(event_id);
        }
    }

    // Use provided stream_id or create a new one, then bind it to the caller env.
    let stream_id = req.stream_id.unwrap_or_else(Uuid::now_v7);
    DbStream::create_or_get_stream(db, stream_id, auth.env_id())
        .await
        .map_err(internal_error)?;
    if req.stream_id.is_some()
        && !DbStream::belongs_to_env(db, stream_id, auth.env_id())
            .await
            .map_err(internal_error)?
    {
        return Err((
            StatusCode::NOT_FOUND,
            Json(ApiErrorResponse::not_found("Stream")),
        ));
    }

    let Some(event_id) = req.event_id else {
        let published = publish_event_to_queue(
            queue,
            db,
            auth,
            actor_user_id,
            &req.event_type,
            &req.event_data,
            stream_id,
            access_id,
        )
        .await?;
        return Ok(published.event_id);
    };

    let event_time = chrono::Utc::now();
    let inserted = Event::insert_event_if_absent(
        db,
        &event_id,
        &auth.env_id(),
        &stream_id,
        &req.event_type,
        &req.event_data,
        event_time,
        &actor_user_id,
        access_id.as_ref(),
        enqueue_lease_end(event_time),
    )
    .await
    .map_err(internal_error)?;
    if !inserted {
        // Another publish got there first, or the id is in an environment
        // this caller can't see.
        return if resume_publish(queue, db, auth, req, event_id).await? {
            Ok(event_id)
        } else {
            Err(event_id_conflict())
        };
    }

    enqueue_leased_event(
        queue,
        db,
        auth,
        actor_user_id,
        &StoredEvent {
            event_id,
            stream_id,
            event_type: &req.event_type,
            event_data: &req.event_data,
            event_time,
            access_id,
        },
    )
    .await?;
    Ok(event_id)
}

pub async fn publish_event(
    State((db, _storage, _conf, _stream_pubsub)): State<ApiStateData>,
    Extension(auth): Extension<AuthContext>,
    Extension(api_key): Extension<ApiKey>,
    OptionalAccessId(access_id): OptionalAccessId,
    Json(req): Json<PublishEventRequest>,
) -> Result<(StatusCode, Json<ApiResponse<EventResponse>>), (StatusCode, Json<ApiErrorResponse>)> {
    require_external_event_type(&req.event_type)?;
    let event_resource = format!("event:{}", req.event_type);
    super::require_permission(
        &auth,
        &event_resource,
        actions::CREATE,
        "Credential does not have create permission for this event type",
    )?;

    let queue = get_event_queue(&_conf).map_err(internal_error)?;
    let event_id = publish_request(
        queue.as_ref(),
        &db,
        &auth,
        api_key.created_by_user_id,
        &req,
        access_id,
    )
    .await?;

    // Fetch the full event record to return
    let event = Event::get_event(&db, &event_id)
        .await
        .map_err(internal_error)?;

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
    blob_store: Option<Extension<Option<Arc<BlobStore>>>>,
    Query(params): Query<ListQueryParams>,
) -> Result<Json<ApiListResponse<EventResponse>>, (StatusCode, Json<ApiErrorResponse>)> {
    let blob_store = blob_store_from_ext(blob_store);
    super::require_permission(
        &auth,
        "event:*",
        actions::READ,
        "Credential does not have read access to events",
    )?;

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

    let mut event_responses: Vec<EventResponse> = Vec::with_capacity(events.len());
    for e in events {
        let event_data =
            rehydrate_payload_json(&db, blob_store.as_ref(), e.env_id, Some(e.event_data))
                .await
                .unwrap_or(serde_json::Value::Null);
        event_responses.push(EventResponse {
            event_id: e.event_id,
            env_id: e.env_id,
            stream_id: e.stream_id,
            event_type: e.event_type,
            event_data,
            event_time: e.event_time,
            created_at: e.created_at,
        });
    }

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
    blob_store: Option<Extension<Option<Arc<BlobStore>>>>,
    Path(event_id): Path<Uuid>,
) -> Result<Json<ApiResponse<EventResponse>>, (StatusCode, Json<ApiErrorResponse>)> {
    let blob_store = blob_store_from_ext(blob_store);
    super::require_permission(
        &auth,
        "event:*",
        actions::READ,
        "Credential does not have read access to events",
    )?;

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

    let event_data = rehydrate_payload_json(
        &db,
        blob_store.as_ref(),
        event.env_id,
        Some(event.event_data),
    )
    .await
    .unwrap_or(serde_json::Value::Null);

    Ok(Json(ApiResponse::new(EventResponse {
        event_id: event.event_id,
        env_id: event.env_id,
        stream_id: event.stream_id,
        event_type: event.event_type,
        event_data,
        event_time: event.event_time,
        created_at: event.created_at,
    })))
}

pub async fn get_event_runs(
    State((db, _storage, _conf, _stream_pubsub)): State<ApiStateData>,
    Extension(auth): Extension<AuthContext>,
    Extension(api_key): Extension<ApiKey>,
    blob_store: Option<Extension<Option<Arc<BlobStore>>>>,
    Path(event_id): Path<Uuid>,
    Query(params): Query<ListQueryParams>,
) -> Result<Json<ApiListResponse<RunResponse>>, (StatusCode, Json<ApiErrorResponse>)> {
    let blob_store = blob_store_from_ext(blob_store);
    super::require_permission(
        &auth,
        "event:*",
        actions::READ,
        "Credential does not have read access to events",
    )?;

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
        params.limit,
        params.offset,
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use hot::val;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

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

    #[test]
    fn external_event_types_only_allow_public_hot_call() {
        assert!(require_external_event_type("hot:call").is_ok());
        assert!(require_external_event_type("hot:rerun").is_err());
        assert!(require_external_event_type("hot:schedule").is_err());
        assert!(require_external_event_type("hot:mcp:call").is_err());
        assert!(require_external_event_type("user:created").is_ok());
    }

    fn test_api_key(data: &hot::db::TestData) -> ApiKey {
        let now = chrono::Utc::now();
        ApiKey {
            api_key_id: Uuid::now_v7(),
            env_id: data.env_id,
            description: "test".to_string(),
            key_data: serde_json::json!({"hash": "test"}),
            active: true,
            created_by_user_id: data.user_id,
            created_at: now,
            updated_at: now,
            updated_by_user_id: None,
            active_toggle_at: None,
            active_toggle_by_user_id: None,
            permissions: serde_json::Value::Null,
        }
    }

    /// A queue whose first enqueues fail, as Redis's do through a restart.
    #[derive(Default)]
    struct FlakyQueue {
        failures_left: AtomicUsize,
        attempts: AtomicUsize,
        messages: Mutex<Vec<Message>>,
    }

    impl FlakyQueue {
        fn failing(failures: usize) -> Self {
            Self {
                failures_left: AtomicUsize::new(failures),
                ..Self::default()
            }
        }
    }

    #[async_trait::async_trait]
    impl Queue<Message> for FlakyQueue {
        async fn enqueue(
            &self,
            item: Message,
        ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
            self.attempts.fetch_add(1, Ordering::SeqCst);
            let failing = self
                .failures_left
                .try_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
                    left.checked_sub(1)
                })
                .is_ok();
            if failing {
                return Err("queue unavailable".into());
            }
            self.messages.lock().unwrap().push(item);
            Ok(())
        }

        async fn dequeue(
            &self,
        ) -> Result<Option<Message>, Box<dyn std::error::Error + Send + Sync>> {
            Ok(None)
        }

        async fn len(&self) -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
            Ok(self.messages.lock().unwrap().len())
        }

        async fn move_to_dead_letter_queue(
            &self,
            _item: Message,
            _reason: String,
        ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
            Ok(())
        }
    }

    async fn publish_to(
        queue: &FlakyQueue,
        db: &DatabasePool,
        data: &hot::db::TestData,
    ) -> Result<PublishedEvent, (StatusCode, Json<ApiErrorResponse>)> {
        let api_key = test_api_key(data);
        publish_event_to_queue(
            queue,
            db,
            &AuthContext::ApiKey(api_key.clone()),
            api_key.created_by_user_id,
            "order:created",
            &serde_json::json!({"answer": 42}),
            data.stream_id,
            None,
        )
        .await
    }

    /// The event row is written before the enqueue, and nothing else queues
    /// it: a failed enqueue leaves the row for no worker to process.
    #[tokio::test]
    async fn a_publish_outlasts_a_queue_that_fails_for_a_moment() {
        let db = hot::db::test_db().await;
        let data = hot::db::insert_test_data(&db).await.unwrap();
        let queue = FlakyQueue::failing(1);

        let published = publish_to(&queue, &db, &data)
            .await
            .expect("the retried enqueue should succeed");

        assert_eq!(queue.attempts.load(Ordering::SeqCst), 2);
        let messages = queue.messages.lock().unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].id, published.event_id);
    }

    #[tokio::test]
    async fn a_publish_reports_a_queue_that_stays_down() {
        let db = hot::db::test_db().await;
        let data = hot::db::insert_test_data(&db).await.unwrap();
        let queue = FlakyQueue::failing(usize::MAX);

        let (status, _) = publish_to(&queue, &db, &data)
            .await
            .err()
            .expect("the publish should fail");

        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(
            queue.attempts.load(Ordering::SeqCst),
            1 + hot::queue::PUBLISH_RETRY_DELAYS.len()
        );
    }

    fn named_publish(event_id: Uuid) -> PublishEventRequest {
        PublishEventRequest {
            event_id: Some(event_id),
            event_type: "order:created".to_string(),
            event_data: serde_json::json!({"answer": 42}),
            stream_id: None,
        }
    }

    async fn publish_as(
        api_key: &ApiKey,
        queue: &FlakyQueue,
        db: &DatabasePool,
        req: &PublishEventRequest,
    ) -> Result<Uuid, PublishError> {
        publish_request(
            queue,
            db,
            &AuthContext::ApiKey(api_key.clone()),
            api_key.created_by_user_id,
            req,
            None,
        )
        .await
    }

    fn queued_ids(queue: &FlakyQueue) -> Vec<Uuid> {
        let messages = queue.messages.lock().unwrap();
        messages.iter().map(|message| message.id).collect()
    }

    async fn enqueue_lease(
        db: &DatabasePool,
        event_id: Uuid,
    ) -> Option<chrono::DateTime<chrono::Utc>> {
        let DatabasePool::Sqlite(pool) = db else {
            unreachable!()
        };
        sqlx::query_scalar("SELECT enqueue_leased_until FROM event WHERE event_id = ?")
            .bind(event_id)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    /// An event as a publish that named it left it, leased until `lease`.
    async fn leftover_event(
        db: &DatabasePool,
        data: &hot::db::TestData,
        req: &PublishEventRequest,
        lease: chrono::DateTime<chrono::Utc>,
    ) {
        let inserted = Event::insert_event_if_absent(
            db,
            &req.event_id.unwrap(),
            &data.env_id,
            &data.stream_id,
            &req.event_type,
            &req.event_data,
            chrono::Utc::now(),
            &data.user_id,
            None,
            lease,
        )
        .await
        .unwrap();
        assert!(inserted);
    }

    fn lease_ago(seconds: i64) -> chrono::DateTime<chrono::Utc> {
        chrono::Utc::now() - chrono::Duration::seconds(seconds)
    }

    #[tokio::test]
    async fn a_resend_starts_nothing_new() {
        let db = hot::db::test_db().await;
        let data = hot::db::insert_test_data(&db).await.unwrap();
        let api_key = test_api_key(&data);
        let queue = FlakyQueue::default();
        let event_id = Uuid::new_v4();
        let req = named_publish(event_id);
        let DatabasePool::Sqlite(pool) = &db else {
            unreachable!()
        };
        let streams = || async {
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM stream")
                .fetch_one(pool)
                .await
                .unwrap()
        };
        let streams_before = streams().await;

        assert_eq!(
            publish_as(&api_key, &queue, &db, &req).await.unwrap(),
            event_id
        );
        let first = Event::get_event(&db, &event_id).await.unwrap();
        assert_eq!(enqueue_lease(&db, event_id).await, None);

        assert_eq!(
            publish_as(&api_key, &queue, &db, &req).await.unwrap(),
            event_id
        );

        assert_eq!(queued_ids(&queue), vec![event_id]);
        let message = queue.messages.lock().unwrap()[0].clone();
        let message = hot::lang::event::EventMessage::try_from(message).unwrap();
        assert!(message.may_repeat(), "a retried enqueue may repeat it");
        let resent = Event::get_event(&db, &event_id).await.unwrap();
        assert_eq!(resent.event_time, first.event_time);
        assert_eq!(resent.stream_id, first.stream_id);
        assert_eq!(Event::get_count(&db).await.unwrap(), 1);
        assert_eq!(streams().await, streams_before + 1);
    }

    #[tokio::test]
    async fn publishes_without_an_id_stay_separate_events() {
        let db = hot::db::test_db().await;
        let data = hot::db::insert_test_data(&db).await.unwrap();
        let api_key = test_api_key(&data);
        let queue = FlakyQueue::default();
        let req = PublishEventRequest {
            event_id: None,
            ..named_publish(Uuid::nil())
        };

        let first = publish_as(&api_key, &queue, &db, &req).await.unwrap();
        let second = publish_as(&api_key, &queue, &db, &req).await.unwrap();

        assert_ne!(first, second);
        assert_eq!(queued_ids(&queue), vec![first, second]);
        let message = queue.messages.lock().unwrap()[0].clone();
        let message = hot::lang::event::EventMessage::try_from(message).unwrap();
        assert!(
            !message.may_repeat(),
            "this path pays for no earlier-run read"
        );
    }

    /// The caller reused an id for something else. Answering as the first
    /// publish did would lose this event without a word.
    #[tokio::test]
    async fn an_id_cannot_name_a_second_event() {
        let db = hot::db::test_db().await;
        let data = hot::db::insert_test_data(&db).await.unwrap();
        let api_key = test_api_key(&data);
        let queue = FlakyQueue::default();
        let event_id = Uuid::new_v4();
        let first = named_publish(event_id);
        publish_as(&api_key, &queue, &db, &first).await.unwrap();

        for different in [
            PublishEventRequest {
                event_data: serde_json::json!({"answer": 43}),
                ..named_publish(event_id)
            },
            PublishEventRequest {
                event_type: "order:paid".to_string(),
                ..named_publish(event_id)
            },
            PublishEventRequest {
                stream_id: Some(data.stream_id),
                ..named_publish(event_id)
            },
        ] {
            let (status, Json(body)) = publish_as(&api_key, &queue, &db, &different)
                .await
                .expect_err("a different event should be refused");
            assert_eq!(status, StatusCode::CONFLICT);
            assert_eq!(body.error.code, "event_id_conflict");
        }

        assert_eq!(queued_ids(&queue), vec![event_id]);
        let stored = Event::get_event(&db, &event_id).await.unwrap();
        assert_eq!(stored.event_data, first.event_data);
    }

    /// Event ids are unique across environments, so the second environment
    /// can't have its event. It must hear that, and learn nothing else.
    #[tokio::test]
    async fn an_id_in_another_environment_is_refused_as_a_different_event() {
        let db = hot::db::test_db().await;
        let data = hot::db::insert_test_data(&db).await.unwrap();
        let api_key = test_api_key(&data);
        let queue = FlakyQueue::default();
        let event_id = Uuid::new_v4();
        let req = named_publish(event_id);
        publish_as(&api_key, &queue, &db, &req).await.unwrap();

        let other_env = Uuid::now_v7();
        hot::db::Env::insert_env(&db, &other_env, &data.org_id, "other", &data.user_id)
            .await
            .unwrap();
        let other_key = ApiKey {
            env_id: other_env,
            ..test_api_key(&data)
        };

        let (status, Json(body)) = publish_as(&other_key, &queue, &db, &req)
            .await
            .expect_err("the id is taken");

        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body.error.code, "event_id_conflict");
        assert_eq!(queued_ids(&queue), vec![event_id]);
        let stored = Event::get_event(&db, &event_id).await.unwrap();
        assert_eq!(stored.env_id, data.env_id);
    }

    #[tokio::test]
    async fn the_nil_uuid_is_not_an_id() {
        let db = hot::db::test_db().await;
        let data = hot::db::insert_test_data(&db).await.unwrap();
        let queue = FlakyQueue::default();

        let (status, _) = publish_as(
            &test_api_key(&data),
            &queue,
            &db,
            &named_publish(Uuid::nil()),
        )
        .await
        .expect_err("a zero value is a caller's bug");

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(Event::get_count(&db).await.unwrap(), 0);
    }

    /// The row outlives the failed enqueue. If the resend took the row for a
    /// finished publish, it would report success for an event nothing runs.
    #[tokio::test]
    async fn a_resend_enqueues_what_a_failed_publish_could_not() {
        let db = hot::db::test_db().await;
        let data = hot::db::insert_test_data(&db).await.unwrap();
        let api_key = test_api_key(&data);
        let queue = FlakyQueue::failing(1 + hot::queue::PUBLISH_RETRY_DELAYS.len());
        let event_id = Uuid::new_v4();
        let req = named_publish(event_id);

        let (status, _) = publish_as(&api_key, &queue, &db, &req)
            .await
            .expect_err("the queue is down");
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(Event::get_count(&db).await.unwrap(), 1);
        assert!(queued_ids(&queue).is_empty());

        publish_as(&api_key, &queue, &db, &req).await.unwrap();

        assert_eq!(queued_ids(&queue), vec![event_id]);
        assert_eq!(enqueue_lease(&db, event_id).await, None);
        let message = queue.messages.lock().unwrap().remove(0);
        let message = hot::lang::event::EventMessage::try_from(message).unwrap();
        assert!(message.may_repeat());
        assert_eq!(message.body.event.event_id, event_id);

        // Now it's done, a further resend starts nothing.
        publish_as(&api_key, &queue, &db, &req).await.unwrap();
        assert!(queued_ids(&queue).is_empty());
    }

    /// The first publish may be enqueuing right now, so a second enqueue would
    /// run the event twice. Its outcome isn't known, so neither is the answer.
    #[tokio::test]
    async fn a_resend_waits_for_a_publish_that_holds_the_enqueue() {
        let db = hot::db::test_db().await;
        let data = hot::db::insert_test_data(&db).await.unwrap();
        let queue = FlakyQueue::default();
        let event_id = Uuid::new_v4();
        let req = named_publish(event_id);
        leftover_event(&db, &data, &req, lease_ago(-60)).await;

        let (status, Json(body)) = publish_as(&test_api_key(&data), &queue, &db, &req)
            .await
            .expect_err("the first publish hasn't finished");

        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body.error.code, "event_publish_in_progress");
        assert_eq!(
            body.error.retry_after,
            Some(PUBLISH_IN_PROGRESS_RETRY_SECONDS)
        );
        assert!(queued_ids(&queue).is_empty());
    }

    /// The first publish died after writing the row, before it could enqueue.
    #[tokio::test]
    async fn a_resend_takes_over_from_a_publish_that_died() {
        let db = hot::db::test_db().await;
        let data = hot::db::insert_test_data(&db).await.unwrap();
        let queue = FlakyQueue::default();
        let event_id = Uuid::new_v4();
        let req = named_publish(event_id);
        leftover_event(&db, &data, &req, lease_ago(1)).await;

        publish_as(&test_api_key(&data), &queue, &db, &req)
            .await
            .unwrap();

        assert_eq!(queued_ids(&queue), vec![event_id]);
        assert_eq!(enqueue_lease(&db, event_id).await, None);
    }

    /// The first publish died after its enqueue. Runs show the event got there.
    #[tokio::test]
    async fn a_resend_enqueues_nothing_for_an_event_with_runs() {
        let db = hot::db::test_db().await;
        let data = hot::db::insert_test_data(&db).await.unwrap();
        let queue = FlakyQueue::default();
        let event_id = Uuid::new_v4();
        let req = named_publish(event_id);
        leftover_event(&db, &data, &req, lease_ago(1)).await;
        Event::mark_event_as_handled(&db, &event_id).await.unwrap();

        publish_as(&test_api_key(&data), &queue, &db, &req)
            .await
            .unwrap();

        assert!(queued_ids(&queue).is_empty());
        assert_eq!(enqueue_lease(&db, event_id).await, None);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn concurrent_publishes_of_an_id_enqueue_it_once() {
        let db = hot::db::test_db().await;
        let data = hot::db::insert_test_data(&db).await.unwrap();
        let api_key = test_api_key(&data);
        let queue = FlakyQueue::default();
        let event_id = Uuid::new_v4();
        let req = named_publish(event_id);

        let results =
            futures::future::join_all((0..8).map(|_| publish_as(&api_key, &queue, &db, &req)))
                .await;

        assert_eq!(queued_ids(&queue), vec![event_id]);
        assert!(results.iter().any(Result::is_ok));
        for result in results {
            if let Err((status, _)) = result {
                assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
            }
        }
    }

    #[test]
    fn stored_json_is_the_same_as_the_json_that_was_sent() {
        let json = |text: &str| serde_json::from_str::<serde_json::Value>(text).unwrap();
        let same = |a: &str, b: &str| same_json(&json(a), &json(b));

        // What jsonb does to a payload.
        assert!(same(r#"{"a": 1, "b": 2}"#, r#"{"b": 2, "a": 1}"#));
        assert!(same("1e2", "100"));
        assert!(same("1E+2", "100"));
        assert!(same("1.5e1", "15"));
        assert!(same("1e-2", "0.01"));
        assert!(same("-0", "0"));
        assert!(same(r#"{"n": [1e2]}"#, r#"{"n": [100]}"#));

        assert!(!same("100", "1000"));
        assert!(!same("1", "-1"));
        assert!(!same("0.1", "0.01"));
        assert!(!same("12345678901234567890", "12345678901234567891"));
        assert!(!same("1e99999999999999999999", "2e99999999999999999999"));
        assert!(!same("[1, 2]", "[2, 1]"));
        assert!(!same("[1, 2]", "[1, 2, 3]"));
        assert!(!same(r#"{"a": 1}"#, r#"{"a": 1, "b": 2}"#));
        assert!(!same("1", r#""1""#));
    }

    #[tokio::test]
    async fn published_event_context_carries_no_deadline() {
        let db = hot::db::test_db().await;
        let data = hot::db::insert_test_data(&db).await.unwrap();

        let conf = val!({
            "queue": {
                "type": "memory",
            },
            "serialization": {
                "type": "json",
            },
        });

        let api_key = test_api_key(&data);

        let published = publish_event_internal(
            &db,
            &conf,
            &AuthContext::ApiKey(api_key.clone()),
            api_key.created_by_user_id,
            "order:created",
            &serde_json::json!({"answer": 42}),
            Uuid::now_v7(),
            None,
        )
        .await
        .expect("publish_event_internal should succeed");

        // The "hot:event" memory channels are process-wide, so other tests in
        // this binary may have enqueued messages too. Ours must be present
        // because the enqueue above completed; skip any that are not ours.
        let queue = get_event_queue(&conf).expect("event queue");
        let mut published_message = None;
        while let Some(message) = queue.dequeue().await.expect("dequeue should not fail") {
            if message.id == published.event_id {
                published_message = Some(message);
                break;
            }
        }
        let message = published_message.expect("published event should be in the queue");

        let event_message = hot::lang::event::EventMessage::try_from(message)
            .expect("queued message should decode as EventMessage");
        assert!(
            event_message.body.execution_context.deadline_at.is_none(),
            "producer contexts must not carry a deadline; the worker stamps it at admission"
        );
        assert_eq!(
            event_message.body.execution_context.run_type_id,
            hot::db::run::RunType::Event.as_id()
        );
        assert_eq!(
            event_message.body.execution_context.event_id,
            Some(published.event_id)
        );
    }
}

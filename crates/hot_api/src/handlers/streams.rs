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
    DatabasePool,
    api_key::ApiKey,
    run::Run,
    stream::{Stream as DbStream, StreamError, StreamSummary},
    task::Task,
};
use hot::permission::actions;
use hot::stream::{
    StreamEvent as PubSubEvent, StreamNext, StreamSubscriber, StreamSubscriberFactory,
};
use serde::{Deserialize, Serialize};
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::{Instant, MissedTickBehavior};
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
use crate::rate_limit;

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
    /// A run failed. When the failure is retried, `run.status` is
    /// `pending_retry` (also after the retry scheduler stores the attempt as
    /// `failed`), and `retry_run_id` names the retry once it has started.
    #[serde(rename = "run:fail")]
    RunFail {
        run: RunResponse,
        #[serde(skip_serializing_if = "Option::is_none")]
        retry_run_id: Option<Uuid>,
    },
    #[serde(rename = "run:cancel")]
    RunCancel { run: RunResponse },
    /// Durable task snapshot for a task associated with this stream.
    #[serde(rename = "task:update")]
    TaskUpdate { task: TaskResponse },
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

#[derive(Clone, PartialEq)]
struct TaskSnapshotVersion {
    task_status_id: i16,
    run_id: Option<Uuid>,
    start_time: Option<chrono::DateTime<chrono::Utc>>,
    stop_time: Option<chrono::DateTime<chrono::Utc>>,
    duration_ms: Option<i64>,
    result: Option<serde_json::Value>,
    retry_attempt: i16,
    next_retry_at: Option<chrono::DateTime<chrono::Utc>>,
}

impl From<&Task> for TaskSnapshotVersion {
    fn from(task: &Task) -> Self {
        Self {
            task_status_id: task.task_status_id,
            run_id: task.run_id,
            start_time: task.start_time,
            stop_time: task.stop_time,
            duration_ms: task.duration_ms,
            result: task.result.clone(),
            retry_attempt: task.retry_attempt,
            next_retry_at: task.next_retry_at,
        }
    }
}

async fn task_update_sse(
    db: &hot::db::DatabasePool,
    blob_store: Option<&Arc<BlobStore>>,
    task: &Task,
) -> Option<SseEvent> {
    let task = super::task_to_response(db, blob_store, task).await;
    let event = StreamEvent::TaskUpdate { task };
    serde_json::to_string(&event)
        .ok()
        .map(|json| SseEvent::default().event("task:update").data(json))
}

/// Subscribe to a stream via Server-Sent Events
///
/// This endpoint provides real-time updates for all events and runs in a stream.
/// Uses push-based pub/sub when available, with database polling as fallback.
///
/// Events emitted:
/// - `run:start` - A new run has started
/// - `stream:data` - Real-time streaming data from a run
/// - `run:stop` - A run completed successfully
/// - `run:fail` - A run failed
/// - `run:cancel` - A run was cancelled
/// - `task:update` - A durable task snapshot changed
/// - `stream:complete` - The stream has completed (no more updates expected)
///
/// Each run's `run:start` comes before its `stream:data` and its end (see
/// `StreamSubscription`).
pub async fn subscribe_to_stream(
    State((db, _storage, conf, stream_pubsub)): State<ApiStateData>,
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

    let resource = format!("stream:{}", stream_id);
    super::require_permission(
        &auth,
        &resource,
        actions::READ,
        "Credential does not have read access to this stream",
    )?;

    // Optional: resolve project filter to project_id
    let project_filter: Option<Uuid> = if let Some(ref project_name_or_id) = params.project {
        let project = get_and_verify_project(&db, &api_key, project_name_or_id).await?;
        Some(project.project_id)
    } else {
        None
    };
    let connection_guard =
        rate_limit::acquire_sse_connection(&db, &conf, &auth, "stream-subscribe")
            .await
            .map_err(|exceeded| {
                (
                    StatusCode::TOO_MANY_REQUESTS,
                    Json(rate_limit::rate_limit_error_body(exceeded)),
                )
            })?;

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
    let stream_scope = crate::auth::runtime_database_scope(&db, auth.env_id())
        .await
        .map_err(|status| {
            (
                status,
                Json(ApiErrorResponse::new(
                    "forbidden",
                    "Environment is not admitting requests",
                )),
            )
        })?;
    let stream_auth = auth.clone();
    let guard_permissions = vec![(format!("stream:{}", stream_id), "read".into())];
    let stream_db = db.clone();
    let subscription = StreamSubscription::new(db, blob_store, env_id, stream_id, project_filter);
    let stream = subscription_events(subscription, subscriber, connection_guard, None);

    // KeepAlive sends a comment every 15 seconds by default, preventing ALB idle timeout
    Ok(Sse::new(crate::auth::guard_stream(
        stream,
        stream_db,
        Some(stream_auth),
        stream_scope,
        guard_permissions,
    ))
    .keep_alive(KeepAlive::default()))
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

// ============================================================================
// Shared subscription loop
// ============================================================================

/// DB poll interval while pub/sub pushes `stream:data` and run events: the
/// poll is only a safety net then.
const PUSHED_POLL_INTERVAL: Duration = Duration::from_secs(1);
/// DB poll interval without pub/sub.
const POLL_ONLY_INTERVAL: Duration = Duration::from_millis(100);
/// A subscription ends with `stream:complete` after this long.
const SUBSCRIPTION_TIMEOUT: Duration = Duration::from_secs(300);
/// Attempts to read a run whose pushed event arrived before its row is
/// visible (the worker flushes the row first, so this is rare).
const RUN_LOOKUP_ATTEMPTS: u32 = 5;
const RUN_LOOKUP_DELAY: Duration = Duration::from_millis(50);
/// While pub/sub delivers, the poll holds back the end of a run it sees for
/// this long. The worker publishes a run's end after its `stream:data`, so
/// the pushed end delivers it after them; the poll covers a lost push.
const POLLED_RUN_END_GRACE: Duration = Duration::from_secs(2);

/// How a run ended, as its SSE event names it.
#[derive(Clone, Copy, Debug, PartialEq)]
enum RunEnd {
    Stop,
    Fail,
    Cancel,
}

impl RunEnd {
    /// The end a stored run status reports. A run waiting to be retried
    /// failed; its retry is a run of its own.
    fn from_status(status: &str) -> Self {
        match status {
            "failed" | "pending_retry" => RunEnd::Fail,
            "cancelled" => RunEnd::Cancel,
            _ => RunEnd::Stop,
        }
    }

    fn event_type(self) -> &'static str {
        match self {
            RunEnd::Stop => "run:stop",
            RunEnd::Fail => "run:fail",
            RunEnd::Cancel => "run:cancel",
        }
    }
}

/// What one SSE stream subscription has told its client.
///
/// Every run the subscription reports gets `run:start` before any of its
/// `stream:data` and before its end, and each of those at most once, however
/// pub/sub and the DB poll interleave: a pushed `stream:data` or end for a
/// run not yet announced announces it first, from the run's row.
struct StreamSubscription {
    db: Arc<DatabasePool>,
    blob_store: Option<Arc<BlobStore>>,
    env_id: Uuid,
    stream_id: Uuid,
    project_filter: Option<Uuid>,
    /// Runs announced with `run:start` (or skipped by the project filter).
    started: ahash::AHashSet<Uuid>,
    /// Runs whose end was sent (or skipped by the project filter).
    ended: ahash::AHashSet<Uuid>,
    /// Runs of pushed `stream:data` that had no row to announce them.
    unknown: ahash::AHashSet<Uuid>,
    /// When the poll first saw each run's end, while pub/sub is delivering.
    polled_ends: ahash::AHashMap<Uuid, Instant>,
    task_versions: ahash::AHashMap<Uuid, TaskSnapshotVersion>,
}

impl StreamSubscription {
    fn new(
        db: Arc<DatabasePool>,
        blob_store: Option<Arc<BlobStore>>,
        env_id: Uuid,
        stream_id: Uuid,
        project_filter: Option<Uuid>,
    ) -> Self {
        Self {
            db,
            blob_store,
            env_id,
            stream_id,
            project_filter,
            started: Default::default(),
            ended: Default::default(),
            unknown: Default::default(),
            polled_ends: Default::default(),
            task_versions: Default::default(),
        }
    }

    /// This stream's run `run_id`, trying `attempts` times.
    async fn load_run(&self, run_id: Uuid, attempts: u32) -> Option<Run> {
        for attempt in 1..=attempts {
            if let Ok(run) = Run::get_run(&self.db, &run_id).await {
                return (run.env_id == self.env_id && run.stream_id == self.stream_id)
                    .then_some(run);
            }
            if attempt < attempts {
                tokio::time::sleep(RUN_LOOKUP_DELAY).await;
            }
        }
        None
    }

    fn passes_filter(&self, run: &Run) -> bool {
        self.project_filter
            .is_none_or(|project_id| run.project_id == Some(project_id))
    }

    /// `retry` is the run's scheduled retry (see `scheduled_retry`).
    async fn run_event(
        &self,
        event_type: &'static str,
        run: &Run,
        retry: Option<Option<Uuid>>,
    ) -> Option<SseEvent> {
        let mut run = run_to_response(&self.db, self.blob_store.as_ref(), run).await;
        if retry.is_some() {
            run.status = "pending_retry".to_string();
        }
        let event = match event_type {
            "run:start" => StreamEvent::RunStart { run },
            "run:fail" => StreamEvent::RunFail {
                run,
                retry_run_id: retry.flatten(),
            },
            "run:cancel" => StreamEvent::RunCancel { run },
            _ => StreamEvent::RunStop { run },
        };
        serde_json::to_string(&event)
            .ok()
            .map(|json| SseEvent::default().event(event_type).data(json))
    }

    async fn announce(&mut self, run: &Run, out: &mut Vec<SseEvent>) {
        if self.started.contains(&run.run_id) {
            return;
        }
        let retry = self.scheduled_retry(run).await;
        self.announce_with(run, retry, out).await;
    }

    async fn announce_with(
        &mut self,
        run: &Run,
        retry: Option<Option<Uuid>>,
        out: &mut Vec<SseEvent>,
    ) {
        if !self.started.insert(run.run_id) || !self.passes_filter(run) {
            return;
        }
        if let Some(event) = self.run_event("run:start", run, retry).await {
            tracing::debug!("SSE: run:start for {}", run.run_id);
            out.push(event);
        }
    }

    async fn end(&mut self, run: &Run, end: RunEnd, out: &mut Vec<SseEvent>) {
        // A retried attempt is never a success, whatever reported its end.
        let retry = self.scheduled_retry(run).await;
        let end = if retry.is_some() { RunEnd::Fail } else { end };
        self.announce_with(run, retry, out).await;
        self.polled_ends.remove(&run.run_id);
        if !self.ended.insert(run.run_id) || !self.passes_filter(run) {
            return;
        }
        if let Some(event) = self.run_event(end.event_type(), run, retry).await {
            tracing::debug!("SSE: {} for {}", end.event_type(), run.run_id);
            out.push(event);
        }
    }

    /// Whether `run` is a failed attempt whose retry was scheduled (`Some`),
    /// with the retry's run once it has started.
    ///
    /// The worker stores a retryable failure as `pending_retry` and raises
    /// `retry_attempt` by one. When the retry scheduler queues the retry it
    /// stores the attempt as `failed`, before the retry's run exists, so the
    /// raised `retry_attempt` is what tells it apart from a final failure,
    /// which keeps the attempt number it ran with: 0, or its origin's when it
    /// is itself a retry (retries share their origin's run type; runs of
    /// events another run sent need not).
    async fn scheduled_retry(&self, run: &Run) -> Option<Option<Uuid>> {
        if run.status == "pending_retry" {
            return Some(None);
        }
        if run.status != "failed" || run.retry_attempt == 0 {
            return None;
        }
        let recorded = run
            .info
            .as_ref()
            .and_then(|info| info.get("retry_run_id"))
            .and_then(|id| id.as_str())
            .and_then(|id| Uuid::parse_str(id).ok());
        if recorded.is_some() {
            return Some(recorded);
        }
        if let Ok(runs) =
            Run::get_runs_by_stream(&self.db, &self.stream_id, &self.env_id, Some(100), Some(0))
                .await
            && let Some(retry) = runs.iter().find(|retry| {
                retry.origin_run_id == Some(run.run_id)
                    && retry.run_type_id == run.run_type_id
                    && retry.retry_attempt >= run.retry_attempt
            })
        {
            return Some(Some(retry.run_id));
        }
        let ran_as = match run.origin_run_id {
            None => 0,
            Some(origin_id) => match Run::get_run(&self.db, &origin_id).await {
                Ok(origin) if origin.run_type_id == run.run_type_id => origin.retry_attempt,
                Ok(_) => 0,
                Err(_) => return None,
            },
        };
        (run.retry_attempt > ran_as).then_some(None)
    }

    /// Turn one pushed event into the SSE events it calls for.
    async fn pushed(&mut self, event: PubSubEvent, out: &mut Vec<SseEvent>) {
        let (run_id, end) = match event {
            PubSubEvent::RunStart {
                run_id, stream_id, ..
            } => {
                if stream_id == self.stream_id
                    && !self.started.contains(&run_id)
                    && let Some(run) = self.load_run(run_id, RUN_LOOKUP_ATTEMPTS).await
                {
                    self.announce(&run, out).await;
                }
                return;
            }
            PubSubEvent::RunStop {
                run_id, stream_id, ..
            } if stream_id == self.stream_id => (run_id, RunEnd::Stop),
            PubSubEvent::RunFail {
                run_id, stream_id, ..
            } if stream_id == self.stream_id => (run_id, RunEnd::Fail),
            PubSubEvent::RunCancel {
                run_id, stream_id, ..
            } if stream_id == self.stream_id => (run_id, RunEnd::Cancel),
            PubSubEvent::StreamData {
                stream_data_id,
                run_id,
                env_id,
                stream_id,
                data_type,
                payload,
            } => {
                if stream_id != self.stream_id || env_id != Some(self.env_id) {
                    return;
                }
                // Announce the run first; a run with no row is looked up once.
                if !self.started.contains(&run_id)
                    && self.unknown.insert(run_id)
                    && let Some(run) = self.load_run(run_id, 1).await
                {
                    self.unknown.remove(&run_id);
                    self.announce(&run, out).await;
                }
                let event = StreamEvent::StreamData {
                    stream_data_id,
                    run_id,
                    data_type,
                    payload,
                };
                if let Ok(json) = serde_json::to_string(&event) {
                    tracing::debug!(
                        "SSE push: stream:data {} for run {}",
                        stream_data_id,
                        run_id
                    );
                    out.push(SseEvent::default().event("stream:data").data(json));
                }
                return;
            }
            _ => return,
        };
        if self.ended.contains(&run_id) {
            return;
        }
        if let Some(run) = self.load_run(run_id, RUN_LOOKUP_ATTEMPTS).await {
            self.end(&run, end, out).await;
        }
    }

    /// The DB poll: announce and end the stream's runs, and send changed
    /// task snapshots. `pushing` says pub/sub is delivering.
    async fn poll(&mut self, pushing: bool, out: &mut Vec<SseEvent>) {
        match Run::get_runs_by_stream(&self.db, &self.stream_id, &self.env_id, Some(100), Some(0))
            .await
        {
            Ok(runs) => {
                // Newest first from the query; report them in start order.
                for run in runs.iter().rev() {
                    self.announce(run, out).await;
                    if run.stop_time.is_none() || self.ended.contains(&run.run_id) {
                        continue;
                    }
                    if pushing {
                        let seen = *self
                            .polled_ends
                            .entry(run.run_id)
                            .or_insert_with(Instant::now);
                        if seen.elapsed() < POLLED_RUN_END_GRACE {
                            continue;
                        }
                    }
                    self.end(run, RunEnd::from_status(&run.status), out).await;
                }
            }
            Err(e) => {
                tracing::error!("Error polling runs for stream {}: {}", self.stream_id, e);
            }
        }
        match Task::get_by_stream(&self.db, &self.stream_id, &self.env_id, Some(100)).await {
            Ok(tasks) => {
                for task in tasks {
                    let version = TaskSnapshotVersion::from(&task);
                    if self.task_versions.get(&task.task_id) == Some(&version) {
                        continue;
                    }
                    self.task_versions.insert(task.task_id, version);
                    if let Some(event) =
                        task_update_sse(&self.db, self.blob_store.as_ref(), &task).await
                    {
                        tracing::debug!(task_id = %task.task_id, "SSE poll: task:update");
                        out.push(event);
                    }
                }
            }
            Err(error) => {
                tracing::error!(stream_id = %self.stream_id, %error, "Error polling tasks for stream");
            }
        }
        // stream:data is not stored; it only arrives through pub/sub.
    }
}

fn poll_timer(pushing: bool) -> tokio::time::Interval {
    let mut timer = tokio::time::interval(if pushing {
        PUSHED_POLL_INTERVAL
    } else {
        POLL_ONLY_INTERVAL
    });
    timer.set_missed_tick_behavior(MissedTickBehavior::Delay);
    timer
}

async fn next_pushed(subscriber: &mut Option<Box<dyn StreamSubscriber>>) -> StreamNext {
    match subscriber {
        Some(subscriber) => subscriber.next().await,
        None => std::future::pending().await,
    }
}

/// The SSE events of one stream subscription: `first` (if any), then pushed
/// events and the DB poll until the subscription times out.
///
/// The poll runs on its own interval. It used to be a fresh sleep raced
/// against each pushed event, so steady `stream:data` kept resetting it and
/// a run's `run:start` came from the poll only after the run had ended.
fn subscription_events(
    mut subscription: StreamSubscription,
    subscriber: Option<Box<dyn StreamSubscriber>>,
    connection_guard: rate_limit::SseConnectionGuard,
    first: Option<SseEvent>,
) -> impl Stream<Item = Result<SseEvent, Infallible>> {
    async_stream::stream! {
        let _connection_guard = connection_guard;
        if let Some(first) = first {
            yield Ok(first);
        }
        let mut subscriber = subscriber;
        let mut poll = poll_timer(subscriber.is_some());
        let deadline = Instant::now() + SUBSCRIPTION_TIMEOUT;

        loop {
            let mut out = Vec::new();
            tokio::select! {
                _ = tokio::time::sleep_until(deadline) => {
                    let complete = StreamEvent::StreamComplete { stream_id: subscription.stream_id };
                    if let Ok(json) = serde_json::to_string(&complete) {
                        yield Ok(SseEvent::default().event("stream:complete").data(json));
                    }
                    break;
                }
                next = next_pushed(&mut subscriber) => match next {
                    StreamNext::Event(event) => subscription.pushed(event, &mut out).await,
                    StreamNext::Idle => {}
                    StreamNext::Closed => {
                        // The Redis subscriber resubscribes on its own; this
                        // is a backend that cannot continue at all.
                        tracing::debug!("SSE pub/sub subscription closed, falling back to polling");
                        subscriber = None;
                        poll = poll_timer(false);
                    }
                },
                _ = poll.tick() => subscription.poll(subscriber.is_some(), &mut out).await,
            }
            for event in out {
                yield Ok(event);
            }
        }
    }
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
/// - `task:update` - A durable task snapshot changed
/// - `stream:complete` - The stream subscription timed out
///
/// Each run's `run:start` comes before its `stream:data` and its end (see
/// `StreamSubscription`).
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
    super::events::require_external_event_type(&req.event_type)?;

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

    let stream_resource = format!("stream:{}", stream_id);
    super::require_permission(
        &auth,
        &stream_resource,
        actions::READ,
        "Credential does not have read access to this stream",
    )?;
    let event_resource = format!("event:{}", req.event_type);
    super::require_permission(
        &auth,
        &event_resource,
        actions::CREATE,
        "Credential does not have create permission for events",
    )?;
    let connection_guard =
        rate_limit::acquire_sse_connection(&db, &conf, &auth, "stream-subscribe-with-event")
            .await
            .map_err(|exceeded| {
                (
                    StatusCode::TOO_MANY_REQUESTS,
                    Json(rate_limit::rate_limit_error_body(exceeded)),
                )
            })?;

    // Resolve the stream's scope before publishing: failing after the event
    // is accepted would make the caller retry and publish it twice.
    let stream_scope = crate::auth::runtime_database_scope(&db, auth.env_id())
        .await
        .map_err(|status| {
            (
                status,
                Json(ApiErrorResponse::new(
                    "forbidden",
                    "Environment is not admitting requests",
                )),
            )
        })?;
    // Step 3: Create the stream record in the database BEFORE subscribing
    // This ensures the stream exists for the subscription and avoids race conditions
    let create_error = |e: StreamError| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ApiErrorResponse::internal_error(&format!(
                "Failed to create stream: {}",
                e
            ))),
        )
    };
    DbStream::create_or_get_stream(&db, stream_id, env_id)
        .await
        .map_err(create_error)?;
    // PostgreSQL RLS hides another environment's stream from the lookup above.
    if req.stream_id.is_some()
        && !DbStream::belongs_to_env(&db, stream_id, env_id)
            .await
            .map_err(create_error)?
    {
        return Err(stream_not_found());
    }

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
        &auth,
        api_key.created_by_user_id,
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
    // Step 6: Create the SSE stream, opening with the publish confirmation
    let published_event = serde_json::to_string(&serde_json::json!({
        "type": "event:published",
        "event_id": published.event_id,
        "stream_id": stream_id,
        "event_type": published.event_type,
    }))
    .ok()
    .map(|json| SseEvent::default().event("event:published").data(json));
    let stream_auth = auth.clone();
    let guard_permissions = vec![(format!("stream:{}", stream_id), "read".into())];
    let stream_db = db.clone();
    let subscription = StreamSubscription::new(db, blob_store, env_id, stream_id, project_filter);
    let stream = subscription_events(subscription, subscriber, connection_guard, published_event);

    // KeepAlive sends a comment every 15 seconds by default, preventing ALB idle timeout
    Ok(Sse::new(crate::auth::guard_stream(
        stream,
        stream_db,
        Some(stream_auth),
        stream_scope,
        guard_permissions,
    ))
    .keep_alive(KeepAlive::default()))
}

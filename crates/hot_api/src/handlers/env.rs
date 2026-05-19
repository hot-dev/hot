//! Environment info handlers

use axum::{
    Extension, Json,
    extract::State,
    http::StatusCode,
    response::sse::{Event as SseEvent, KeepAlive, Sse},
};
use futures::stream::Stream;
use hot::db::{api_key::ApiKey, env::Env};
use hot::stream::{EnvEvent as PubSubEnvEvent, EnvSubscriber, EnvSubscriberFactory};
use serde::Serialize;
use std::convert::Infallible;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::ApiStateData;
use crate::models::*;

pub async fn get_env_info(
    State((db, _storage, _conf, _stream_pubsub)): State<ApiStateData>,
    Extension(api_key): Extension<ApiKey>,
) -> Result<Json<ApiResponse<EnvironmentResponse>>, (StatusCode, Json<ApiErrorResponse>)> {
    let env = Env::get_env(&db, &api_key.env_id).await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ApiErrorResponse::internal_error(&e.to_string())),
        )
    })?;

    Ok(Json(ApiResponse::new(EnvironmentResponse {
        env_id: env.env_id,
        org_id: env.org_id,
        name: env.name,
        active: env.active,
    })))
}

// ============================================================================
// Environment SSE Subscription (Real-time Dashboard Updates)
// ============================================================================

/// SSE event types for environment subscription
#[derive(Debug, Serialize, ToSchema)]
#[serde(tag = "type")]
pub enum EnvSseEvent {
    #[serde(rename = "run:start")]
    RunStart {
        run_id: Uuid,
        env_id: Uuid,
        stream_id: Uuid,
        event_id: Option<Uuid>,
        project_id: Option<Uuid>,
        fn_name: Option<String>,
        run_type: String,
    },
    #[serde(rename = "run:stop")]
    RunStop {
        run_id: Uuid,
        env_id: Uuid,
        stream_id: Uuid,
        event_id: Option<Uuid>,
        project_id: Option<Uuid>,
        fn_name: Option<String>,
        run_type: String,
        duration_ms: Option<i64>,
    },
    #[serde(rename = "run:fail")]
    RunFail {
        run_id: Uuid,
        env_id: Uuid,
        stream_id: Uuid,
        event_id: Option<Uuid>,
        project_id: Option<Uuid>,
        fn_name: Option<String>,
        run_type: String,
        duration_ms: Option<i64>,
        error: Option<String>,
    },
    #[serde(rename = "run:cancel")]
    RunCancel {
        run_id: Uuid,
        env_id: Uuid,
        stream_id: Uuid,
        event_id: Option<Uuid>,
        project_id: Option<Uuid>,
        fn_name: Option<String>,
        run_type: String,
        duration_ms: Option<i64>,
        reason: Option<String>,
    },
    #[serde(rename = "event:created")]
    EventCreated {
        event_id: Uuid,
        env_id: Uuid,
        stream_id: Uuid,
        event_type: String,
        project_id: Option<Uuid>,
    },
    #[serde(rename = "event:handled")]
    EventHandled {
        event_id: Uuid,
        env_id: Uuid,
        stream_id: Uuid,
        event_type: String,
        project_id: Option<Uuid>,
    },
    #[serde(rename = "stream:created")]
    StreamCreated {
        stream_id: Uuid,
        env_id: Uuid,
        project_id: Option<Uuid>,
    },
    #[serde(rename = "task:started")]
    TaskStarted {
        task_id: Uuid,
        env_id: Uuid,
        stream_id: Uuid,
        function_name: String,
        task_type: String,
    },
    #[serde(rename = "task:complete")]
    TaskComplete {
        task_id: Uuid,
        env_id: Uuid,
        stream_id: Uuid,
        function_name: String,
        status: String,
        duration_ms: Option<i64>,
        error: Option<serde_json::Value>,
    },
    #[serde(rename = "keepalive")]
    Keepalive,
}

/// Subscribe to environment events via Server-Sent Events
///
/// This endpoint provides real-time updates for all runs, events, and streams
/// in the environment for API clients.
///
/// Events emitted:
/// - `run:start` - Emitted when a run starts
/// - `run:stop` - Emitted when a run stops successfully
/// - `run:fail` - Emitted when a run fails
/// - `run:cancel` - Emitted when a run is canceled
/// - `event:created` - Emitted when an event is created
/// - `event:handled` - Emitted when an event is handled
/// - `stream:created` - Emitted when a stream is created
/// - `task:started` - Emitted when a task starts running
/// - `task:complete` - Emitted when a task completes
pub async fn subscribe_to_env(
    State((_db, _storage, _conf, stream_pubsub)): State<ApiStateData>,
    Extension(api_key): Extension<ApiKey>,
) -> Result<
    Sse<impl Stream<Item = Result<SseEvent, Infallible>>>,
    (StatusCode, Json<ApiErrorResponse>),
> {
    let env_id = api_key.env_id;

    // Try to subscribe to pub/sub for real-time updates
    let subscriber: Option<Box<dyn EnvSubscriber>> = if let Some(ref pubsub) = stream_pubsub {
        match pubsub.subscribe_env(env_id).await {
            Ok(sub) => {
                tracing::debug!("SSE handler subscribed to env pub/sub for {}", env_id);
                Some(sub)
            }
            Err(e) => {
                tracing::debug!(
                    "SSE handler falling back to polling for env {} (pub/sub error: {})",
                    env_id,
                    e
                );
                None
            }
        }
    } else {
        tracing::debug!(
            "SSE handler using polling for env {} (no pub/sub configured)",
            env_id
        );
        None
    };

    // If no pub/sub available, return an error (or we could implement polling)
    if subscriber.is_none() {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ApiErrorResponse::internal_error(
                "Real-time updates not available (pub/sub not configured)",
            )),
        ));
    }

    // Create the SSE stream with push from pub/sub
    let stream = async_stream::stream! {
        let mut subscriber = subscriber;
        let stream_timeout = tokio::time::Duration::from_secs(300); // 5 minute timeout
        let start_time = tokio::time::Instant::now();

        loop {
            // Check for timeout
            if start_time.elapsed() > stream_timeout {
                tracing::debug!("SSE env subscription timed out for env {}", env_id);
                break;
            }

            // Receive push event from pub/sub
            if let Some(ref mut sub) = subscriber {
                match sub.next().await {
                    Some(event) => {
                        // Convert pub/sub event to SSE event
                        let (event_type, sse_event) = convert_pubsub_to_sse(event);
                        if let Ok(json) = serde_json::to_string(&sse_event) {
                            tracing::debug!("SSE push: {} for env {}", event_type, env_id);
                            yield Ok(SseEvent::default().event(event_type).data(json));
                        }
                    }
                    None => {
                        // Subscriber returned None - could be timeout or closed
                        // For Redis Streams, this is a 30s timeout, so we just continue
                        tracing::trace!("SSE env subscription poll returned None for env {}", env_id);
                        continue;
                    }
                }
            } else {
                // No subscriber - shouldn't happen but break to be safe
                break;
            }
        }
    };

    // KeepAlive sends a comment every 15 seconds by default, preventing ALB idle timeout
    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

/// Convert pub/sub EnvEvent to SSE event
fn convert_pubsub_to_sse(event: PubSubEnvEvent) -> (&'static str, EnvSseEvent) {
    match event {
        PubSubEnvEvent::RunStart {
            run_id,
            env_id,
            stream_id,
            event_id,
            project_id,
            fn_name,
            run_type,
        } => (
            "run:start",
            EnvSseEvent::RunStart {
                run_id,
                env_id,
                stream_id,
                event_id,
                project_id,
                fn_name,
                run_type,
            },
        ),
        PubSubEnvEvent::RunStop {
            run_id,
            env_id,
            stream_id,
            event_id,
            project_id,
            fn_name,
            run_type,
            duration_ms,
        } => (
            "run:stop",
            EnvSseEvent::RunStop {
                run_id,
                env_id,
                stream_id,
                event_id,
                project_id,
                fn_name,
                run_type,
                duration_ms,
            },
        ),
        PubSubEnvEvent::RunFail {
            run_id,
            env_id,
            stream_id,
            event_id,
            project_id,
            fn_name,
            run_type,
            duration_ms,
            error,
        } => (
            "run:fail",
            EnvSseEvent::RunFail {
                run_id,
                env_id,
                stream_id,
                event_id,
                project_id,
                fn_name,
                run_type,
                duration_ms,
                error,
            },
        ),
        PubSubEnvEvent::RunCancel {
            run_id,
            env_id,
            stream_id,
            event_id,
            project_id,
            fn_name,
            run_type,
            duration_ms,
            reason,
        } => (
            "run:cancel",
            EnvSseEvent::RunCancel {
                run_id,
                env_id,
                stream_id,
                event_id,
                project_id,
                fn_name,
                run_type,
                duration_ms,
                reason,
            },
        ),
        PubSubEnvEvent::EventCreated {
            event_id,
            env_id,
            stream_id,
            event_type,
            project_id,
        } => (
            "event:created",
            EnvSseEvent::EventCreated {
                event_id,
                env_id,
                stream_id,
                event_type,
                project_id,
            },
        ),
        PubSubEnvEvent::EventHandled {
            event_id,
            env_id,
            stream_id,
            event_type,
            project_id,
        } => (
            "event:handled",
            EnvSseEvent::EventHandled {
                event_id,
                env_id,
                stream_id,
                event_type,
                project_id,
            },
        ),
        PubSubEnvEvent::StreamCreated {
            stream_id,
            env_id,
            project_id,
        } => (
            "stream:created",
            EnvSseEvent::StreamCreated {
                stream_id,
                env_id,
                project_id,
            },
        ),
        PubSubEnvEvent::TaskStarted {
            task_id,
            env_id,
            stream_id,
            function_name,
            task_type,
        } => (
            "task:started",
            EnvSseEvent::TaskStarted {
                task_id,
                env_id,
                stream_id,
                function_name,
                task_type,
            },
        ),
        PubSubEnvEvent::TaskComplete {
            task_id,
            env_id,
            stream_id,
            function_name,
            status,
            duration_ms,
            error,
        } => (
            "task:complete",
            EnvSseEvent::TaskComplete {
                task_id,
                env_id,
                stream_id,
                function_name,
                status,
                duration_ms,
                error,
            },
        ),
    }
}

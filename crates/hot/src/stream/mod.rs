//! Stream pub/sub module for real-time SSE delivery
//!
//! This module provides a publish/subscribe mechanism for stream events,
//! enabling real-time updates to SSE clients when runs complete.
//!
//! ## Architecture
//!
//! - **Publisher**: Used by the worker to broadcast run events
//! - **Subscriber**: Used by the API to receive events for SSE delivery
//!
//! ## Deployment Modes
//!
//! - **Memory**: In-memory broadcast channels for single-process (`hot dev`)
//! - **Redis**: Redis Pub/Sub for distributed deployments

use crate::val;
use crate::val::Val;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::error::Error;
use std::fmt;
use std::str::FromStr;
use std::sync::Arc;
use uuid::Uuid;

pub mod mem;
pub mod streams;

/// Stream event types that can be published/subscribed
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum StreamEvent {
    /// A new run has started
    #[serde(rename = "run:start")]
    RunStart {
        run_id: Uuid,
        stream_id: Uuid,
        event_id: Option<Uuid>,
    },
    /// A run completed successfully
    #[serde(rename = "run:stop")]
    RunStop {
        run_id: Uuid,
        stream_id: Uuid,
        event_id: Option<Uuid>,
        result: Option<serde_json::Value>,
    },
    /// A run failed (error condition)
    #[serde(rename = "run:fail")]
    RunFail {
        run_id: Uuid,
        stream_id: Uuid,
        event_id: Option<Uuid>,
        error: Option<String>,
    },
    /// A run was cancelled (not an error, deliberate early termination)
    #[serde(rename = "run:cancel")]
    RunCancel {
        run_id: Uuid,
        stream_id: Uuid,
        event_id: Option<Uuid>,
        reason: Option<String>,
    },
    /// User-emitted stream data (partial results, progress, SSE events, etc.)
    /// Emitted via ::hot::stream/data(type, payload)
    #[serde(rename = "stream:data")]
    StreamData {
        /// Unique ID for this data emission (UUIDv7 for ordering)
        stream_data_id: Uuid,
        run_id: Uuid,
        stream_id: Uuid,
        /// User-defined data type (e.g., "http:sse:event", "progress", "llm:token")
        data_type: String,
        /// The actual payload data
        payload: serde_json::Value,
    },
    /// Inbound message to a running task, sent via ::hot::task/send.
    /// Routed via pub/sub using the task_id as the channel key.
    #[serde(rename = "task:message")]
    TaskMessage {
        task_id: String,
        payload: serde_json::Value,
    },
}

/// Environment-level events for dashboard real-time updates
/// These are published to a per-environment channel for broad visibility
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum EnvEvent {
    /// Emitted when a run starts
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
    /// Emitted when a run stops successfully
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
    /// Emitted when a run fails
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
    /// Emitted when a run is canceled
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
    /// Emitted when an event is created
    #[serde(rename = "event:created")]
    EventCreated {
        event_id: Uuid,
        env_id: Uuid,
        stream_id: Uuid,
        event_type: String,
        project_id: Option<Uuid>,
    },
    /// Emitted when an event is handled
    #[serde(rename = "event:handled")]
    EventHandled {
        event_id: Uuid,
        env_id: Uuid,
        stream_id: Uuid,
        event_type: String,
        project_id: Option<Uuid>,
    },
    /// Emitted when a stream is created
    #[serde(rename = "stream:created")]
    StreamCreated {
        stream_id: Uuid,
        env_id: Uuid,
        project_id: Option<Uuid>,
    },
    /// Emitted when a task starts running
    #[serde(rename = "task:started")]
    TaskStarted {
        task_id: Uuid,
        env_id: Uuid,
        stream_id: Uuid,
        function_name: String,
        task_type: String,
    },
    /// Emitted when a task completes (succeeded, failed, or timed out)
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
}

impl EnvEvent {
    /// Get the env_id from any event variant
    pub fn env_id(&self) -> Uuid {
        match self {
            EnvEvent::RunStart { env_id, .. } => *env_id,
            EnvEvent::RunStop { env_id, .. } => *env_id,
            EnvEvent::RunFail { env_id, .. } => *env_id,
            EnvEvent::RunCancel { env_id, .. } => *env_id,
            EnvEvent::EventCreated { env_id, .. } => *env_id,
            EnvEvent::EventHandled { env_id, .. } => *env_id,
            EnvEvent::StreamCreated { env_id, .. } => *env_id,
            EnvEvent::TaskStarted { env_id, .. } => *env_id,
            EnvEvent::TaskComplete { env_id, .. } => *env_id,
        }
    }

    /// Get the event type name for SSE
    pub fn event_type_name(&self) -> &'static str {
        match self {
            EnvEvent::RunStart { .. } => "run:start",
            EnvEvent::RunStop { .. } => "run:stop",
            EnvEvent::RunFail { .. } => "run:fail",
            EnvEvent::RunCancel { .. } => "run:cancel",
            EnvEvent::EventCreated { .. } => "event:created",
            EnvEvent::EventHandled { .. } => "event:handled",
            EnvEvent::StreamCreated { .. } => "stream:created",
            EnvEvent::TaskStarted { .. } => "task:started",
            EnvEvent::TaskComplete { .. } => "task:complete",
        }
    }
}

impl StreamEvent {
    /// Get the stream_id from any event variant.
    /// For TaskMessage, the task_id (parsed as UUID) is used as the routing key.
    pub fn stream_id(&self) -> Uuid {
        match self {
            StreamEvent::RunStart { stream_id, .. } => *stream_id,
            StreamEvent::RunStop { stream_id, .. } => *stream_id,
            StreamEvent::RunFail { stream_id, .. } => *stream_id,
            StreamEvent::RunCancel { stream_id, .. } => *stream_id,
            StreamEvent::StreamData { stream_id, .. } => *stream_id,
            StreamEvent::TaskMessage { task_id, .. } => {
                Uuid::parse_str(task_id).unwrap_or(Uuid::nil())
            }
        }
    }

    /// Get the event_id from any event variant (None for StreamData/TaskMessage)
    pub fn event_id(&self) -> Option<Uuid> {
        match self {
            StreamEvent::RunStart { event_id, .. } => *event_id,
            StreamEvent::RunStop { event_id, .. } => *event_id,
            StreamEvent::RunFail { event_id, .. } => *event_id,
            StreamEvent::RunCancel { event_id, .. } => *event_id,
            StreamEvent::StreamData { .. } | StreamEvent::TaskMessage { .. } => None,
        }
    }

    /// Get the run_id from any event variant (nil for TaskMessage)
    pub fn run_id(&self) -> Uuid {
        match self {
            StreamEvent::RunStart { run_id, .. } => *run_id,
            StreamEvent::RunStop { run_id, .. } => *run_id,
            StreamEvent::RunFail { run_id, .. } => *run_id,
            StreamEvent::RunCancel { run_id, .. } => *run_id,
            StreamEvent::StreamData { run_id, .. } => *run_id,
            StreamEvent::TaskMessage { .. } => Uuid::nil(),
        }
    }
}

/// Type of stream pub/sub backend
#[derive(Debug, PartialEq, Clone, Copy, Default)]
pub enum StreamPubSubType {
    #[default]
    Memory, // In-memory broadcast channels (single process)
    Redis, // Redis Pub/Sub (distributed)
}

impl fmt::Display for StreamPubSubType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StreamPubSubType::Memory => write!(f, "memory"),
            StreamPubSubType::Redis => write!(f, "redis"),
        }
    }
}

impl FromStr for StreamPubSubType {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "memory" | "mem" => Ok(StreamPubSubType::Memory),
            "redis" => Ok(StreamPubSubType::Redis),
            _ => Err(format!("Invalid stream pubsub type: {}", s)),
        }
    }
}

/// Error types for stream pub/sub operations
#[derive(Debug)]
pub enum StreamPubSubError {
    PublishError(String),
    SubscribeError(String),
    ConnectionError(String),
    SerializationError(String),
}

impl fmt::Display for StreamPubSubError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PublishError(e) => write!(f, "Publish error: {}", e),
            Self::SubscribeError(e) => write!(f, "Subscribe error: {}", e),
            Self::ConnectionError(e) => write!(f, "Connection error: {}", e),
            Self::SerializationError(e) => write!(f, "Serialization error: {}", e),
        }
    }
}

impl std::error::Error for StreamPubSubError {}

/// Trait for publishing stream events
#[async_trait]
pub trait StreamPublisher: Send + Sync {
    /// Publish an event to a stream channel
    ///
    /// This is fire-and-forget - if no subscribers are listening, the event is dropped.
    /// This is intentional for real-time events where the polling fallback catches missed events.
    async fn publish(&self, event: StreamEvent) -> Result<(), StreamPubSubError>;
}

/// Trait for subscribing to stream events
#[async_trait]
pub trait StreamSubscriber: Send {
    /// Receive the next event from the subscription
    ///
    /// Returns `None` if the subscription is closed or an error occurred.
    async fn next(&mut self) -> Option<StreamEvent>;
}

/// Factory for creating stream subscribers
#[async_trait]
pub trait StreamSubscriberFactory: Send + Sync {
    /// Subscribe to events for a specific stream
    async fn subscribe(
        &self,
        stream_id: Uuid,
    ) -> Result<Box<dyn StreamSubscriber>, StreamPubSubError>;
}

/// Trait for publishing environment-level events (for dashboard real-time updates)
#[async_trait]
pub trait EnvPublisher: Send + Sync {
    /// Publish an event to an environment channel
    ///
    /// This is fire-and-forget - if no subscribers are listening, the event is dropped.
    async fn publish_env(&self, event: EnvEvent) -> Result<(), StreamPubSubError>;
}

/// Trait for subscribing to environment-level events
#[async_trait]
pub trait EnvSubscriber: Send {
    /// Receive the next event from the subscription
    ///
    /// Returns `None` if the subscription is closed or an error occurred.
    async fn next(&mut self) -> Option<EnvEvent>;
}

/// Factory for creating environment subscribers
#[async_trait]
pub trait EnvSubscriberFactory: Send + Sync {
    /// Subscribe to events for a specific environment
    async fn subscribe_env(
        &self,
        env_id: Uuid,
    ) -> Result<Box<dyn EnvSubscriber>, StreamPubSubError>;
}

/// Unified stream pub/sub implementation that can use either Memory or Redis Streams backend
pub enum StreamPubSub {
    Memory(mem::MemStreamPubSub),
    Redis(streams::RedisStreamsPubSub),
}

impl Clone for StreamPubSub {
    fn clone(&self) -> Self {
        match self {
            StreamPubSub::Memory(m) => StreamPubSub::Memory(m.clone()),
            StreamPubSub::Redis(r) => StreamPubSub::Redis(r.clone()),
        }
    }
}

impl StreamPubSub {
    /// Create a new StreamPubSub with the specified backend
    pub fn new(
        pubsub_type: StreamPubSubType,
        redis_uri: Option<String>,
        redis_cluster: bool,
    ) -> Result<Self, Box<dyn Error + Send + Sync>> {
        match pubsub_type {
            StreamPubSubType::Memory => {
                tracing::debug!("Creating in-memory stream pub/sub");
                Ok(StreamPubSub::Memory(mem::MemStreamPubSub::new()))
            }
            StreamPubSubType::Redis => {
                let url = redis_uri.unwrap_or_else(|| "redis://127.0.0.1/".to_string());

                // Initialize Rustls crypto provider if using TLS (rediss://)
                if url.starts_with("rediss://") {
                    crate::redis::init_crypto_provider();
                }

                // Check if cluster mode is enabled or auto-detect from URI
                let is_cluster = redis_cluster || crate::redis::is_cluster_uri(&url);

                if is_cluster {
                    tracing::debug!("Creating Redis Streams cluster pub/sub");
                    let client = ::redis::cluster::ClusterClient::new(vec![url.as_str()])?;
                    Ok(StreamPubSub::Redis(
                        streams::RedisStreamsPubSub::new_cluster(client),
                    ))
                } else {
                    tracing::debug!("Creating Redis Streams standalone pub/sub");
                    let client = ::redis::Client::open(url.as_str())?;
                    Ok(StreamPubSub::Redis(streams::RedisStreamsPubSub::new(
                        client,
                    )))
                }
            }
        }
    }
}

#[async_trait]
impl StreamPublisher for StreamPubSub {
    async fn publish(&self, event: StreamEvent) -> Result<(), StreamPubSubError> {
        match self {
            StreamPubSub::Memory(m) => m.publish(event).await,
            StreamPubSub::Redis(r) => r.publish(event).await,
        }
    }
}

#[async_trait]
impl StreamSubscriberFactory for StreamPubSub {
    async fn subscribe(
        &self,
        stream_id: Uuid,
    ) -> Result<Box<dyn StreamSubscriber>, StreamPubSubError> {
        match self {
            StreamPubSub::Memory(m) => m.subscribe(stream_id).await,
            StreamPubSub::Redis(r) => r.subscribe(stream_id).await,
        }
    }
}

#[async_trait]
impl EnvPublisher for StreamPubSub {
    async fn publish_env(&self, event: EnvEvent) -> Result<(), StreamPubSubError> {
        match self {
            StreamPubSub::Memory(m) => m.publish_env(event).await,
            StreamPubSub::Redis(r) => r.publish_env(event).await,
        }
    }
}

#[async_trait]
impl EnvSubscriberFactory for StreamPubSub {
    async fn subscribe_env(
        &self,
        env_id: Uuid,
    ) -> Result<Box<dyn EnvSubscriber>, StreamPubSubError> {
        match self {
            StreamPubSub::Memory(m) => m.subscribe_env(env_id).await,
            StreamPubSub::Redis(r) => r.subscribe_env(env_id).await,
        }
    }
}

/// Create a StreamPubSub from configuration
pub fn create_stream_pubsub(
    pubsub_type: StreamPubSubType,
    redis_uri: Option<String>,
    redis_cluster: bool,
) -> Result<Arc<StreamPubSub>, Box<dyn Error + Send + Sync>> {
    Ok(Arc::new(StreamPubSub::new(
        pubsub_type,
        redis_uri,
        redis_cluster,
    )?))
}

/// Get resolved configuration for stream pub/sub settings
pub fn get_resolved_conf(conf: Val) -> Val {
    // Start with defaults
    let default_conf = val!({
        "type": StreamPubSubType::default().to_string()
    });

    // Merge with provided conf (the provided conf will override defaults)
    default_conf.merge(&conf)
}

/// Channel name prefix for stream events
pub const STREAM_CHANNEL_PREFIX: &str = "hot:stream";

/// Channel name prefix for environment events
pub const ENV_CHANNEL_PREFIX: &str = "hot:env";

/// Generate a Redis channel name for a stream
/// Uses hash tags for Redis cluster compatibility
pub fn channel_name(stream_id: &Uuid) -> String {
    // Hash tag around stream_id ensures streams distribute across cluster nodes
    // while related operations (pub/sub) for the same stream hit the same node
    format!("{}:{{{}}}", STREAM_CHANNEL_PREFIX, stream_id)
}

/// Generate a Redis channel name for environment events
/// Uses hash tags for Redis cluster compatibility
pub fn env_channel_name(env_id: &Uuid) -> String {
    // Hash tag around env_id ensures environments distribute across cluster nodes
    // while all subscribers for the same env connect to the same node
    format!("{}:{{{}}}", ENV_CHANNEL_PREFIX, env_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    #[test]
    fn test_task_message_stream_id() {
        let uuid = Uuid::now_v7();
        let event = StreamEvent::TaskMessage {
            task_id: uuid.to_string(),
            payload: serde_json::json!({"data": 1}),
        };
        assert_eq!(event.stream_id(), uuid);
    }

    #[test]
    fn test_task_message_stream_id_invalid_uuid() {
        let event = StreamEvent::TaskMessage {
            task_id: "not-a-uuid".to_string(),
            payload: serde_json::json!(null),
        };
        assert_eq!(event.stream_id(), Uuid::nil());
    }

    #[test]
    fn test_task_message_event_id_is_none() {
        let event = StreamEvent::TaskMessage {
            task_id: Uuid::now_v7().to_string(),
            payload: serde_json::json!(null),
        };
        assert_eq!(event.event_id(), None);
    }

    #[test]
    fn test_task_message_run_id_is_nil() {
        let event = StreamEvent::TaskMessage {
            task_id: Uuid::now_v7().to_string(),
            payload: serde_json::json!(null),
        };
        assert_eq!(event.run_id(), Uuid::nil());
    }

    #[test]
    fn test_task_message_serde_round_trip() {
        let event = StreamEvent::TaskMessage {
            task_id: "019506ab-1234-7000-8000-000000000001".to_string(),
            payload: serde_json::json!({"command": "stop", "reason": "user request"}),
        };

        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"type\":\"task:message\""));

        let deserialized: StreamEvent = serde_json::from_str(&json).unwrap();
        match deserialized {
            StreamEvent::TaskMessage { task_id, payload } => {
                assert_eq!(task_id, "019506ab-1234-7000-8000-000000000001");
                assert_eq!(payload["command"], "stop");
                assert_eq!(payload["reason"], "user request");
            }
            _ => panic!("Expected TaskMessage variant"),
        }
    }

    #[test]
    fn test_run_start_stream_id() {
        let sid = Uuid::now_v7();
        let event = StreamEvent::RunStart {
            run_id: Uuid::now_v7(),
            stream_id: sid,
            event_id: None,
        };
        assert_eq!(event.stream_id(), sid);
    }

    #[test]
    fn test_stream_data_accessors() {
        let rid = Uuid::now_v7();
        let sid = Uuid::now_v7();
        let event = StreamEvent::StreamData {
            stream_data_id: Uuid::now_v7(),
            run_id: rid,
            stream_id: sid,
            data_type: "progress".to_string(),
            payload: serde_json::json!(50),
        };
        assert_eq!(event.stream_id(), sid);
        assert_eq!(event.run_id(), rid);
        assert_eq!(event.event_id(), None);
    }
}

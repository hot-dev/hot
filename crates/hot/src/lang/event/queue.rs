use super::{Event, EventPublisher, ExecutionContext};
use crate::data::msg::Message;
use crate::data::serialization::Serialization;
use crate::db::DatabasePool;
use crate::queue::{ProcessingQueue, Queue, QueueType};
use crate::val::Val;
use ahash::{AHashMap, AHashSet};
use serde::{Deserialize, Serialize};
use std::str::FromStr;
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

/// Message format for deployment events
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeploymentMessage {
    pub id: Uuid,
    pub head: AHashMap<String, String>,
    pub body: DeploymentMessageBody,
}

/// Deployment message body containing build information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeploymentMessageBody {
    pub build_id: Uuid,
}

impl From<DeploymentMessage> for Message {
    fn from(deployment_msg: DeploymentMessage) -> Self {
        // Convert head HashMap to Val
        let head_val = Val::from(
            deployment_msg
                .head
                .into_iter()
                .map(|(k, v)| (Val::from(k), Val::from(v)))
                .collect::<Vec<_>>(),
        );

        // Create head with type discriminator and head
        let head = crate::val!({
            "__type": "DeploymentMessage",
            "head": head_val
        });

        // Convert deployment message body to Val
        let body = crate::val!({
            "build_id": deployment_msg.body.build_id.to_string()
        });

        Message {
            id: deployment_msg.id,
            head,
            body,
        }
    }
}

impl TryFrom<Message> for DeploymentMessage {
    type Error = String;

    fn try_from(msg: Message) -> Result<Self, Self::Error> {
        // Debug: log the actual message structure
        tracing::debug!(
            "Attempting to parse DeploymentMessage from: head={}, body={}",
            msg.head.pretty_print(),
            msg.body.pretty_print()
        );

        // Check type discriminator
        let msg_type = msg.head.get_str("__type");
        if msg_type != "DeploymentMessage" {
            return Err(format!(
                "Expected DeploymentMessage type, got: '{}'. Message head: {}",
                msg_type,
                msg.head.pretty_print()
            ));
        }

        let head_val = msg.head.get("head").ok_or_else(|| {
            format!(
                "Missing head in DeploymentMessage. Message head: {}",
                msg.head.pretty_print()
            )
        })?;

        let mut head = AHashMap::new();
        if let Val::Map(header_map) = head_val {
            for (k, v) in header_map.iter() {
                if let (Val::Str(key), Val::Str(value)) = (k, v) {
                    head.insert((**key).to_owned(), (**value).to_owned());
                }
            }
        }

        // Extract deployment data
        let build_id = msg
            .body
            .get_str("build_id")
            .parse::<Uuid>()
            .map_err(|e| format!("Invalid build_id: {}", e))?;

        Ok(DeploymentMessage {
            id: msg.id,
            head,
            body: DeploymentMessageBody { build_id },
        })
    }
}

/// Enqueue a `DeploymentMessage` on the shared `hot:event` queue so the worker
/// re-runs the full deploy pipeline for `build_id`.
///
/// The worker side (`hot::build::load_build_manifest_data`) is what re-derives
/// event handlers, schedules, MCP tools, webhooks, and agents from the bundle's
/// `manifest.hot`. Without this enqueue, flipping `build.deployed = true` in
/// the database is not enough — schedules stay deactivated (e.g. after a
/// project deactivate/reactivate cycle) and worker-side handler caches stay
/// stale. Every deploy code path (API, UI, future automation) MUST go through
/// this helper to keep behavior consistent.
pub async fn enqueue_deployment_message(conf: &Val, build_id: Uuid) -> Result<(), String> {
    let deployment_message = DeploymentMessage {
        id: Uuid::now_v7(),
        head: AHashMap::from_iter([("build_id".to_string(), build_id.to_string())]),
        body: DeploymentMessageBody { build_id },
    };
    let message: Message = deployment_message.into();

    let queue_type_str = conf.get_str_or_default("queue.type", "memory");
    let queue_type = QueueType::from_str(&queue_type_str).unwrap_or(QueueType::Memory);

    let redis_uri_str = conf.get_str_or_default("redis.uri", "");
    let redis_uri = if redis_uri_str.is_empty() || redis_uri_str == "null" {
        None
    } else {
        Some(redis_uri_str)
    };
    let redis_cluster = conf.get_bool_or_default("redis.cluster", false);

    let serialization_str = conf.get_str_or_default("serialization.type", "zstd-json");
    let serialization = Serialization::from_str(&serialization_str).unwrap_or_default();

    let queue = ProcessingQueue::<Message>::new_with_cluster(
        queue_type,
        "hot:event".to_string(),
        redis_uri,
        redis_cluster,
        serialization,
    )
    .map_err(|e| format!("Failed to create deployment queue: {}", e))?;

    queue
        .enqueue(message)
        .await
        .map_err(|e| format!("Failed to enqueue deployment message: {}", e))?;

    Ok(())
}

/// On project reactivation, find the latest build for the project and put it
/// back in the same state a fresh `hot deploy` would: bundle builds are queued
/// for worker preparation/activation, while live builds activate directly.
///
/// This is the inverse of what `Project::toggle_active(active=false)` did:
/// it had set `deployed = false` on every build and `active = false` on every
/// schedule, which is enough to silence the project but not enough to bring it
/// back online — schedules in particular stay deactivated forever otherwise.
///
/// Returns `Ok(Some(build_id))` if a build was found and redeployed,
/// `Ok(None)` if the project has no builds yet (nothing to do — typical for a
/// project that was just reactivated as part of a fresh upload flow).
pub async fn enqueue_redeploy_for_project_reactivation(
    db: &DatabasePool,
    conf: &Val,
    project_id: &Uuid,
    deploying_user_id: &Uuid,
) -> Result<Option<Uuid>, String> {
    let latest = crate::db::Build::get_latest_build_for_project(db, project_id)
        .await
        .map_err(|e| format!("Failed to look up latest build for project: {}", e))?;

    let Some(build) = latest else {
        return Ok(None);
    };

    if build.is_bundle() {
        crate::db::Build::request_bundle_deployment(db, &build.build_id, deploying_user_id)
            .await
            .map_err(|e| {
                format!(
                    "Failed to request bundle redeploy during reactivation: {}",
                    e
                )
            })?;

        enqueue_deployment_message(conf, build.build_id).await?;
    } else {
        crate::db::Build::activate_build_directly(db, &build.build_id, deploying_user_id)
            .await
            .map_err(|e| format!("Failed to activate live build during reactivation: {}", e))?;
    }

    Ok(Some(build.build_id))
}

// ============================================================================
// MaintenanceMessage — system maintenance tasks
// ============================================================================

/// Message format for system maintenance events.
///
/// The scheduler enqueues these daily; the worker handles them by running
/// DB cleanup tasks (session expiry, call record purging, etc.).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MaintenanceMessage {
    pub id: Uuid,
    pub head: AHashMap<String, String>,
    pub body: MaintenanceMessageBody,
}

/// Body containing the list of maintenance tasks to perform.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MaintenanceMessageBody {
    /// Which tasks to run. e.g. ["session_cleanup", "call_record_purge", "inactive_schedule_cleanup"]
    pub tasks: Vec<String>,
}

impl MaintenanceMessage {
    /// Create a new maintenance message for a single task.
    pub fn single_task(task: &str) -> Self {
        Self {
            id: Uuid::now_v7(),
            head: AHashMap::new(),
            body: MaintenanceMessageBody {
                tasks: vec![task.to_string()],
            },
        }
    }

    /// Session + inactive-schedule + queue + call-retention cleanup (no custom-domain / AWS work).
    pub fn daily_core_tasks() -> Self {
        Self {
            id: Uuid::now_v7(),
            head: AHashMap::new(),
            body: MaintenanceMessageBody {
                tasks: vec![
                    "session_cleanup".to_string(),
                    "inactive_schedule_cleanup".to_string(),
                    "queue_cleanup".to_string(),
                    "zombie_run_cleanup".to_string(),
                    "call_retention_cleanup".to_string(),
                ],
            },
        }
    }

    /// Create a new maintenance message requesting all standard tasks (includes domain verification).
    pub fn all_tasks() -> Self {
        let mut msg = Self::daily_core_tasks();
        msg.body.tasks.push("domain_verification".to_string());
        msg
    }
}

impl From<MaintenanceMessage> for Message {
    fn from(msg: MaintenanceMessage) -> Self {
        let head_val = Val::from(
            msg.head
                .into_iter()
                .map(|(k, v)| (Val::from(k), Val::from(v)))
                .collect::<Vec<_>>(),
        );

        let head = crate::val!({
            "__type": "MaintenanceMessage",
            "head": head_val
        });

        let tasks_val = Val::from(
            msg.body
                .tasks
                .into_iter()
                .map(Val::from)
                .collect::<Vec<_>>(),
        );

        let body = crate::val!({
            "tasks": tasks_val
        });

        Message {
            id: msg.id,
            head,
            body,
        }
    }
}

impl TryFrom<Message> for MaintenanceMessage {
    type Error = String;

    fn try_from(msg: Message) -> Result<Self, Self::Error> {
        let msg_type = msg.head.get_str("__type");
        if msg_type != "MaintenanceMessage" {
            return Err(format!(
                "Expected MaintenanceMessage type, got: '{}'",
                msg_type
            ));
        }

        let head_val = msg
            .head
            .get("head")
            .ok_or("Missing head in MaintenanceMessage")?;
        let mut head = AHashMap::new();
        if let Val::Map(header_map) = head_val {
            for (k, v) in header_map.iter() {
                if let (Val::Str(key), Val::Str(value)) = (k, v) {
                    head.insert((**key).to_owned(), (**value).to_owned());
                }
            }
        }

        // Extract tasks array
        let tasks = if let Some(Val::Vec(task_vec)) = msg.body.get("tasks") {
            task_vec
                .iter()
                .filter_map(|v| {
                    if let Val::Str(s) = v {
                        Some((**s).to_owned())
                    } else {
                        None
                    }
                })
                .collect()
        } else {
            Vec::new()
        };

        Ok(MaintenanceMessage {
            id: msg.id,
            head,
            body: MaintenanceMessageBody { tasks },
        })
    }
}

/// Message format for queue events
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventMessage {
    pub id: Uuid,
    pub head: AHashMap<String, String>,
    pub body: EventMessageBody,
}

/// Event message body containing the event and execution context
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventMessageBody {
    pub event: Event,
    pub execution_context: ExecutionContext,
}

impl From<EventMessage> for Message {
    fn from(event_msg: EventMessage) -> Self {
        // Convert head HashMap to Val
        let head_val = Val::from(
            event_msg
                .head
                .into_iter()
                .map(|(k, v)| (Val::from(k), Val::from(v)))
                .collect::<Vec<_>>(),
        );

        // Create head with type discriminator and head
        let head = crate::val!({
            "__type": "EventMessage",
            "head": head_val
        });

        // Convert event message body to Val. Keep the queued event envelope
        // intentionally thin: the worker hydrates authoritative event_data
        // from the database by event_id before routing/execution.
        let body = crate::val!({
            "event": {
                "event_id": event_msg.body.event.event_id.to_string(),
                "env_id": event_msg.body.event.env_id.to_string(),
                "stream_id": event_msg.body.event.stream_id.to_string(),
                "event_type": event_msg.body.event.event_type,
                "event_time": event_msg.body.event.event_time.to_rfc3339(),
                "target_project_id": event_msg.body.event.target_project_id.map(|id| id.to_string()),
                "target_project_name": event_msg.body.event.target_project_name
            },
            "execution_context": {
                "run_id": event_msg.body.execution_context.run_id.to_string(),
                "stream_id": event_msg.body.execution_context.stream_id.to_string(),
                "run_type_id": event_msg.body.execution_context.run_type_id as i64,
                "env_id": event_msg.body.execution_context.env_id.map(|id| id.to_string()),
                "user_id": event_msg.body.execution_context.user_id.map(|id| id.to_string()),
                "org_id": event_msg.body.execution_context.org_id.map(|id| id.to_string()),
                "build_id": event_msg.body.execution_context.build_id.map(|id| id.to_string()),
                "event_id": event_msg.body.execution_context.event_id.map(|id| id.to_string()),
                "origin_run_id": event_msg.body.execution_context.origin_run_id.map(|id| id.to_string()),
                "retry_attempt": event_msg.body.execution_context.retry_attempt as i64,
            }
        });

        Message {
            id: event_msg.id,
            head,
            body,
        }
    }
}

impl TryFrom<Message> for EventMessage {
    type Error = String;

    fn try_from(msg: Message) -> Result<Self, Self::Error> {
        // Debug: log the actual message structure
        tracing::debug!(
            "Attempting to parse EventMessage from: head={}, body={}",
            msg.head.pretty_print(),
            msg.body.pretty_print()
        );

        // Check type discriminator
        let msg_type = msg.head.get_str("__type");
        if msg_type != "EventMessage" {
            return Err(format!(
                "Expected EventMessage type, got: '{}'. Message head: {}",
                msg_type,
                msg.head.pretty_print()
            ));
        }

        let head_val = msg.head.get("head").ok_or_else(|| {
            format!(
                "Missing head in EventMessage. Message head: {}",
                msg.head.pretty_print()
            )
        })?;

        let mut head = AHashMap::new();
        if let Val::Map(header_map) = head_val {
            for (k, v) in header_map.iter() {
                if let (Val::Str(key), Val::Str(value)) = (k, v) {
                    head.insert((**key).to_owned(), (**value).to_owned());
                }
            }
        }

        // Extract event data
        let event_val = msg
            .body
            .get("event")
            .ok_or("Missing event in EventMessage body")?;

        let event_id = event_val
            .get_str("event_id")
            .parse::<Uuid>()
            .map_err(|e| format!("Invalid event_id: {}", e))?;

        let env_id = event_val
            .get_str("env_id")
            .parse::<Uuid>()
            .map_err(|e| format!("Invalid env_id: {}", e))?;

        let event_stream_id = event_val
            .get_str("stream_id")
            .parse::<Uuid>()
            .map_err(|e| format!("Invalid stream_id in event: {}", e))?;

        let event_type = event_val.get_str("event_type");
        // New queue messages are thin and omit event_data; older in-flight
        // messages may still carry it. The worker hydrates from DB before
        // execution, so a placeholder keeps parsing backward-compatible.
        let event_data = event_val.get("event_data").unwrap_or_else(|| {
            crate::val!({
                "event_id": event_id.to_string(),
            })
        });

        let event_time_str = event_val.get_str("event_time");
        let event_time = chrono::DateTime::parse_from_rfc3339(&event_time_str)
            .map_err(|e| format!("Invalid event_time: {}", e))?
            .with_timezone(&chrono::Utc);

        // Extract target project info for routing (optional, for tie-breaking)
        let target_project_id = if let Some(Val::Str(s)) = event_val.get("target_project_id") {
            Some(
                s.parse::<Uuid>()
                    .map_err(|e| format!("Invalid target_project_id: {}", e))?,
            )
        } else {
            None
        };

        let target_project_name = if let Some(Val::Str(s)) = event_val.get("target_project_name") {
            Some(s.to_string())
        } else {
            None
        };

        // Extract execution context
        let ctx_val = msg
            .body
            .get("execution_context")
            .ok_or("Missing execution_context in EventMessage body")?;

        let run_id = ctx_val
            .get_str("run_id")
            .parse::<Uuid>()
            .map_err(|e| format!("Invalid run_id: {}", e))?;

        let run_type_id = ctx_val.get_int("run_type_id");

        // Handle stream_id - generate new one if not present (backward compatibility)
        let stream_id = if let Some(Val::Str(s)) = ctx_val.get("stream_id") {
            s.parse::<Uuid>()
                .map_err(|e| format!("Invalid stream_id: {}", e))?
        } else {
            // Generate new stream_id for old messages that don't have one
            Uuid::now_v7()
        };

        let env_id_opt = if let Some(Val::Str(s)) = ctx_val.get("env_id") {
            Some(
                s.parse::<Uuid>()
                    .map_err(|e| format!("Invalid env_id in context: {}", e))?,
            )
        } else {
            None
        };

        let user_id_opt = if let Some(Val::Str(s)) = ctx_val.get("user_id") {
            Some(
                s.parse::<Uuid>()
                    .map_err(|e| format!("Invalid user_id: {}", e))?,
            )
        } else {
            None
        };

        let org_id_opt = if let Some(Val::Str(s)) = ctx_val.get("org_id") {
            Some(
                s.parse::<Uuid>()
                    .map_err(|e| format!("Invalid org_id: {}", e))?,
            )
        } else {
            None
        };

        let build_id_opt = if let Some(Val::Str(s)) = ctx_val.get("build_id") {
            Some(
                s.parse::<Uuid>()
                    .map_err(|e| format!("Invalid build_id: {}", e))?,
            )
        } else {
            None
        };

        let event_id_opt = if let Some(Val::Str(s)) = ctx_val.get("event_id") {
            Some(
                s.parse::<Uuid>()
                    .map_err(|e| format!("Invalid event_id in context: {}", e))?,
            )
        } else {
            None
        };

        let origin_run_id_opt = if let Some(Val::Str(s)) = ctx_val.get("origin_run_id") {
            Some(
                s.parse::<Uuid>()
                    .map_err(|e| format!("Invalid origin_run_id in context: {}", e))?,
            )
        } else {
            None
        };

        let event = Event {
            event_id,
            env_id,
            stream_id: event_stream_id,
            event_type,
            event_data,
            event_time,
            target_project_id,
            target_project_name,
        };

        let execution_context = ExecutionContext {
            run_id,
            stream_id,
            run_type_id: run_type_id as i16,
            env_id: env_id_opt,
            env_name: None,
            user_id: user_id_opt,
            org_id: org_id_opt,
            org_slug: None,
            build_id: build_id_opt,
            build_hash: None,
            project_id: None,
            project_name: None,
            event_id: event_id_opt,
            origin_run_id: origin_run_id_opt,
            retry_attempt: ctx_val.get_int_or_default("retry_attempt", 0) as i16,
            secret_keys: AHashSet::new(),
            secret_value_hashes: AHashSet::new(),
            access_id: ctx_val.get("access_id").and_then(|v| {
                if let crate::val::Val::Str(s) = v {
                    Uuid::parse_str(s.as_ref()).ok()
                } else {
                    None
                }
            }),
            agent_type: ctx_val.get("agent_type").and_then(|v| {
                if let crate::val::Val::Str(s) = v {
                    Some(s.to_string())
                } else {
                    None
                }
            }),
        };

        Ok(EventMessage {
            id: msg.id,
            head,
            body: EventMessageBody {
                event,
                execution_context,
            },
        })
    }
}

/// Event queue interface - simplified wrapper around the hot queue system
///
/// Event publisher that sends events to a queue
pub struct QueueEventPublisher {
    queue: ProcessingQueue<Message>,
    queue_name: String,
    shutdown_sender: mpsc::UnboundedSender<oneshot::Sender<()>>,
}

impl QueueEventPublisher {
    /// Create a new QueueEventPublisher
    pub fn new(
        queue_type: QueueType,
        queue_name: String,
        redis_uri: Option<String>,
        serialization: Serialization,
    ) -> Self {
        Self::new_with_cluster(queue_type, queue_name, redis_uri, false, serialization)
    }

    /// Create a new QueueEventPublisher with cluster support
    pub fn new_with_cluster(
        queue_type: QueueType,
        queue_name: String,
        redis_uri: Option<String>,
        redis_cluster: bool,
        serialization: Serialization,
    ) -> Self {
        let (shutdown_sender, _shutdown_receiver) =
            mpsc::unbounded_channel::<oneshot::Sender<()>>();

        // Create the actual queue using the hot queue system with cluster support
        let queue = ProcessingQueue::<Message>::new_with_cluster(
            queue_type,
            queue_name.clone(),
            redis_uri,
            redis_cluster,
            serialization,
        )
        .expect("Failed to create event queue");

        Self {
            queue,
            queue_name,
            shutdown_sender,
        }
    }

    /// Create a QueueEventPublisher with default settings for the "hot:event" queue
    pub fn new_default() -> Self {
        Self::new(
            QueueType::Memory,
            "hot:event".to_string(),
            None,
            Serialization::Json,
        )
    }

    /// Shutdown implementation
    async fn shutdown_impl(&self) -> Result<(), String> {
        // For ProcessingQueue, there's no background processing to shut down
        // The queue itself handles its own lifecycle
        Ok(())
    }
}

impl EventPublisher for QueueEventPublisher {
    fn publish(&self, ctx: &ExecutionContext, event: Event) {
        // Create head with context information
        let mut head = AHashMap::new();
        head.insert("queue".to_string(), self.queue_name.clone());
        head.insert("event_type".to_string(), event.event_type.clone());

        if let Some(env_id) = ctx.env_id {
            head.insert("env_id".to_string(), env_id.to_string());
        }
        if let Some(user_id) = ctx.user_id {
            head.insert("user_id".to_string(), user_id.to_string());
        }
        if let Some(org_id) = ctx.org_id {
            head.insert("org_id".to_string(), org_id.to_string());
        }
        head.insert("run_id".to_string(), ctx.run_id.to_string());

        // Create the EventMessage
        let event_message = EventMessage {
            id: event.event_id,
            head,
            body: EventMessageBody {
                event,
                execution_context: ctx.clone(),
            },
        };

        // Convert to unified Message format
        let message: Message = event_message.into();

        // Send to queue (fire and forget)
        let queue = self.queue.clone();
        tokio::spawn(async move {
            if let Err(e) = queue.enqueue(message).await {
                tracing::error!("QueueEventPublisher: Failed to enqueue event: {}", e);
            }
        });
    }

    fn shutdown(
        &self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + '_>> {
        Box::pin(self.shutdown_impl())
    }
}

impl Clone for QueueEventPublisher {
    fn clone(&self) -> Self {
        Self {
            queue: self.queue.clone(),
            queue_name: self.queue_name.clone(),
            shutdown_sender: self.shutdown_sender.clone(),
        }
    }
}

/// Event publisher that publishes to both a queue and database
pub struct QueueAndDatabaseEventPublisher {
    queue_publisher: QueueEventPublisher,
    database_publisher: super::database::DatabaseEventPublisher,
}

impl QueueAndDatabaseEventPublisher {
    /// Create a new QueueAndDatabaseEventPublisher
    pub fn new(
        queue_publisher: QueueEventPublisher,
        database_publisher: super::database::DatabaseEventPublisher,
    ) -> Self {
        Self {
            queue_publisher,
            database_publisher,
        }
    }

    /// Create a QueueAndDatabaseEventPublisher with default queue settings
    pub fn new_with_database(database_publisher: super::database::DatabaseEventPublisher) -> Self {
        let queue_publisher = QueueEventPublisher::new_default();
        Self::new(queue_publisher, database_publisher)
    }

    /// Shutdown implementation that shuts down both publishers
    async fn shutdown_impl(&self) -> Result<(), String> {
        // Shutdown both publishers concurrently
        let queue_result = self.queue_publisher.shutdown_impl();
        let database_result = self.database_publisher.shutdown_impl();

        let (queue_res, db_res) = tokio::join!(queue_result, database_result);

        // Collect any errors
        let mut errors = Vec::new();
        if let Err(e) = queue_res {
            errors.push(format!("Queue shutdown error: {}", e));
        }
        if let Err(e) = db_res {
            errors.push(format!("Database shutdown error: {}", e));
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("; "))
        }
    }
}

impl EventPublisher for QueueAndDatabaseEventPublisher {
    fn publish(&self, ctx: &ExecutionContext, event: Event) {
        // CRITICAL: Database write MUST complete before queue enqueue
        // to prevent race condition where worker dequeues event before
        // the event record exists in the database (causing FK violations on run.event_id)

        // Step 1: Write to database and WAIT for completion
        if let Err(e) = self.database_publisher.publish_and_wait(ctx, event.clone()) {
            tracing::error!("Failed to write event to database: {}", e);
            // Don't enqueue if database write failed - this prevents orphaned queue messages
            return;
        }

        // Step 2: Now that database write is complete, enqueue to queue
        // Worker can now safely process this event knowing the event record exists
        self.queue_publisher.publish(ctx, event);
    }

    fn shutdown(
        &self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + '_>> {
        Box::pin(self.shutdown_impl())
    }
}

impl Clone for QueueAndDatabaseEventPublisher {
    fn clone(&self) -> Self {
        Self {
            queue_publisher: self.queue_publisher.clone(),
            database_publisher: self.database_publisher.clone(),
        }
    }
}

// =============================================================================
// Alert Delivery Message (for hot:alert queue)
// =============================================================================

/// Message format for alert delivery processing
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlertDeliveryMessage {
    pub id: Uuid,
    pub head: AHashMap<String, String>,
    pub body: AlertDeliveryMessageBody,
}

/// Alert delivery message body
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlertDeliveryMessageBody {
    /// The alert_delivery_id referencing the alert_delivery table row
    pub alert_delivery_id: Uuid,
    /// The alert_id for logging/correlation
    pub alert_id: Uuid,
    /// Destination type hint for quick routing ("email", "slack", "pagerduty", "webhook")
    pub destination_type: String,
}

impl From<AlertDeliveryMessage> for Message {
    fn from(msg: AlertDeliveryMessage) -> Self {
        let head_val = Val::from(
            msg.head
                .into_iter()
                .map(|(k, v)| (Val::from(k), Val::from(v)))
                .collect::<Vec<_>>(),
        );

        let head = crate::val!({
            "__type": "AlertDeliveryMessage",
            "head": head_val
        });

        let body = crate::val!({
            "alert_delivery_id": msg.body.alert_delivery_id.to_string(),
            "alert_id": msg.body.alert_id.to_string(),
            "destination_type": msg.body.destination_type
        });

        Message {
            id: msg.id,
            head,
            body,
        }
    }
}

impl TryFrom<Message> for AlertDeliveryMessage {
    type Error = String;

    fn try_from(msg: Message) -> Result<Self, Self::Error> {
        let msg_type = msg.head.get_str("__type");
        if msg_type != "AlertDeliveryMessage" {
            return Err(format!(
                "Expected AlertDeliveryMessage type, got: '{}'",
                msg_type
            ));
        }

        let head_val = msg
            .head
            .get("head")
            .ok_or("Missing head in AlertDeliveryMessage")?;
        let mut head = AHashMap::new();
        if let Val::Map(header_map) = head_val {
            for (k, v) in header_map.iter() {
                if let (Val::Str(key), Val::Str(value)) = (k, v) {
                    head.insert((**key).to_owned(), (**value).to_owned());
                }
            }
        }

        let alert_delivery_id = msg
            .body
            .get_str("alert_delivery_id")
            .parse::<Uuid>()
            .map_err(|e| format!("Invalid alert_delivery_id: {}", e))?;

        let alert_id = msg
            .body
            .get_str("alert_id")
            .parse::<Uuid>()
            .map_err(|e| format!("Invalid alert_id: {}", e))?;

        let destination_type = msg.body.get_str("destination_type");

        Ok(AlertDeliveryMessage {
            id: msg.id,
            head,
            body: AlertDeliveryMessageBody {
                alert_delivery_id,
                alert_id,
                destination_type,
            },
        })
    }
}

// =============================================================================
// Email Message (for hot:email queue)
// =============================================================================

/// Message format for app email sending
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmailMessage {
    pub id: Uuid,
    pub head: AHashMap<String, String>,
    pub body: EmailMessageBody,
}

/// Email message body containing pre-rendered email content
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmailMessageBody {
    /// Reference to email_queue table row for status updates
    pub email_queue_id: Uuid,
    /// Recipient email address
    pub to_address: String,
    /// Email subject line
    pub subject: String,
    /// Pre-rendered HTML content
    pub html_body: Option<String>,
    /// Pre-rendered plain text content
    pub text_body: Option<String>,
    /// Formatted "From" address (e.g. "Hot Dev <hi@notifications.hot.dev>")
    pub from_address: String,
}

impl From<EmailMessage> for Message {
    fn from(msg: EmailMessage) -> Self {
        let head_val = Val::from(
            msg.head
                .into_iter()
                .map(|(k, v)| (Val::from(k), Val::from(v)))
                .collect::<Vec<_>>(),
        );

        let head = crate::val!({
            "__type": "EmailMessage",
            "head": head_val
        });

        let body = crate::val!({
            "email_queue_id": msg.body.email_queue_id.to_string(),
            "to_address": msg.body.to_address,
            "subject": msg.body.subject,
            "html_body": msg.body.html_body.unwrap_or_default(),
            "text_body": msg.body.text_body.unwrap_or_default(),
            "from_address": msg.body.from_address
        });

        Message {
            id: msg.id,
            head,
            body,
        }
    }
}

impl TryFrom<Message> for EmailMessage {
    type Error = String;

    fn try_from(msg: Message) -> Result<Self, Self::Error> {
        let msg_type = msg.head.get_str("__type");
        if msg_type != "EmailMessage" {
            return Err(format!("Expected EmailMessage type, got: '{}'", msg_type));
        }

        let head_val = msg.head.get("head").ok_or("Missing head in EmailMessage")?;
        let mut head = AHashMap::new();
        if let Val::Map(header_map) = head_val {
            for (k, v) in header_map.iter() {
                if let (Val::Str(key), Val::Str(value)) = (k, v) {
                    head.insert((**key).to_owned(), (**value).to_owned());
                }
            }
        }

        let email_queue_id = msg
            .body
            .get_str("email_queue_id")
            .parse::<Uuid>()
            .map_err(|e| format!("Invalid email_queue_id: {}", e))?;

        let to_address = msg.body.get_str("to_address");
        let subject = msg.body.get_str("subject");
        let from_address = msg.body.get_str("from_address");

        let html_body = {
            let s = msg.body.get_str("html_body");
            if s.is_empty() { None } else { Some(s) }
        };

        let text_body = {
            let s = msg.body.get_str("text_body");
            if s.is_empty() { None } else { Some(s) }
        };

        Ok(EmailMessage {
            id: msg.id,
            head,
            body: EmailMessageBody {
                email_queue_id,
                to_address,
                subject,
                html_body,
                text_body,
                from_address,
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::DatabaseEventPublisher;
    use super::*;
    use uuid::Uuid;

    #[tokio::test]
    async fn test_queue_event_publisher_creation() {
        let publisher = QueueEventPublisher::new_default();

        // Create a test event
        let event = Event::new(
            Uuid::now_v7(),
            Uuid::now_v7(), // stream_id
            "test_event".to_string(),
            crate::val::Val::from("test_data"),
        );
        let ctx = ExecutionContext::new(
            Uuid::now_v7(),
            Uuid::now_v7(), // stream_id
            4,              // 'run' type for test
            Some(Uuid::now_v7()),
            Some(Uuid::now_v7()),
            Some(Uuid::now_v7()),
            Some(Uuid::now_v7()), // build_id
        );

        // This should not panic
        publisher.publish(&ctx, event);

        // Test shutdown
        assert!(publisher.shutdown().await.is_ok());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_queue_and_database_event_publisher() {
        let queue_publisher = QueueEventPublisher::new_default();
        let database_publisher = DatabaseEventPublisher::new("sqlite::memory:".to_string());
        let combined_publisher =
            QueueAndDatabaseEventPublisher::new(queue_publisher, database_publisher);

        // Create a test event
        let event = Event::new(
            Uuid::now_v7(),
            Uuid::now_v7(), // stream_id
            "test_combined_event".to_string(),
            crate::val::Val::from("test_data"),
        );
        let ctx = ExecutionContext::new(
            Uuid::now_v7(),
            Uuid::now_v7(), // stream_id
            4,              // 'run' type for test
            Some(Uuid::now_v7()),
            Some(Uuid::now_v7()),
            Some(Uuid::now_v7()),
            Some(Uuid::now_v7()), // build_id
        );

        let publisher_for_vm = combined_publisher.clone();
        tokio::task::spawn_blocking(move || {
            publisher_for_vm.publish(&ctx, event);
        })
        .await
        .expect("spawn_blocking publisher call should not panic");

        // Test shutdown
        assert!(combined_publisher.shutdown().await.is_ok());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_queue_and_database_event_publisher_from_spawn_blocking() {
        let queue_publisher = QueueEventPublisher::new_default();
        let database_publisher = DatabaseEventPublisher::new("sqlite::memory:".to_string());
        let combined_publisher =
            QueueAndDatabaseEventPublisher::new(queue_publisher, database_publisher);
        let publisher_for_vm = combined_publisher.clone();

        let event = Event::new(
            Uuid::now_v7(),
            Uuid::now_v7(),
            "test_combined_event_from_vm".to_string(),
            crate::val::Val::from("test_data"),
        );
        let ctx = ExecutionContext::new(
            Uuid::now_v7(),
            Uuid::now_v7(),
            4,
            Some(Uuid::now_v7()),
            Some(Uuid::now_v7()),
            Some(Uuid::now_v7()),
            Some(Uuid::now_v7()),
        );

        tokio::task::spawn_blocking(move || {
            publisher_for_vm.publish(&ctx, event);
        })
        .await
        .expect("spawn_blocking publisher call should not panic");

        assert!(combined_publisher.shutdown().await.is_ok());
    }

    #[test]
    fn test_event_message_omits_event_data_from_queue_payload() {
        let event = Event::new(
            Uuid::now_v7(),
            Uuid::now_v7(),
            "test_event".to_string(),
            crate::val!({
                "large": "payload",
                "nested": {
                    "ok": true,
                    "count": 3,
                },
            }),
        );
        let ctx = ExecutionContext::new(
            Uuid::now_v7(),
            Uuid::now_v7(),
            4,
            Some(Uuid::now_v7()),
            Some(Uuid::now_v7()),
            Some(Uuid::now_v7()),
            Some(Uuid::now_v7()),
        );
        let message: Message = EventMessage {
            id: event.event_id,
            head: AHashMap::new(),
            body: EventMessageBody {
                event,
                execution_context: ctx,
            },
        }
        .into();

        let event_val = message.body.get("event").expect("event envelope");
        assert!(event_val.get("event_id").is_some());
        assert!(event_val.get("event_data").is_none());
    }

    #[test]
    fn test_thin_event_message_roundtrip_uses_placeholder_data() {
        let event_id = Uuid::now_v7();
        let env_id = Uuid::now_v7();
        let stream_id = Uuid::now_v7();
        let run_id = Uuid::now_v7();
        let message = Message {
            id: event_id,
            head: crate::val!({
                "__type": "EventMessage",
                "head": {},
            }),
            body: crate::val!({
                "event": {
                    "event_id": event_id.to_string(),
                    "env_id": env_id.to_string(),
                    "stream_id": stream_id.to_string(),
                    "event_type": "test_event",
                    "event_time": chrono::Utc::now().to_rfc3339(),
                },
                "execution_context": {
                    "run_id": run_id.to_string(),
                    "stream_id": stream_id.to_string(),
                    "run_type_id": 4,
                    "env_id": env_id.to_string(),
                    "retry_attempt": 0,
                },
            }),
        };

        let parsed: EventMessage = message.try_into().unwrap();

        assert_eq!(parsed.body.event.event_id, event_id);
        assert_eq!(parsed.body.event.env_id, env_id);
        assert_eq!(parsed.body.event.event_type, "test_event");
        assert_eq!(
            parsed.body.event.event_data.get_str("event_id"),
            event_id.to_string()
        );
    }

    #[test]
    fn test_alert_delivery_message_roundtrip() {
        let original = AlertDeliveryMessage {
            id: Uuid::now_v7(),
            head: AHashMap::new(),
            body: AlertDeliveryMessageBody {
                alert_delivery_id: Uuid::now_v7(),
                alert_id: Uuid::now_v7(),
                destination_type: "email".to_string(),
            },
        };

        let original_id = original.id;
        let original_delivery_id = original.body.alert_delivery_id;
        let original_alert_id = original.body.alert_id;

        // Convert to Message
        let message: Message = original.into();

        // Verify type discriminator
        assert_eq!(message.head.get_str("__type"), "AlertDeliveryMessage");

        // Convert back
        let restored: AlertDeliveryMessage = message.try_into().unwrap();

        assert_eq!(restored.id, original_id);
        assert_eq!(restored.body.alert_delivery_id, original_delivery_id);
        assert_eq!(restored.body.alert_id, original_alert_id);
        assert_eq!(restored.body.destination_type, "email");
    }

    #[test]
    fn test_alert_delivery_message_with_headers() {
        let mut head = AHashMap::new();
        head.insert("org_id".to_string(), "test-org".to_string());
        head.insert("env_id".to_string(), "test-env".to_string());

        let original = AlertDeliveryMessage {
            id: Uuid::now_v7(),
            head,
            body: AlertDeliveryMessageBody {
                alert_delivery_id: Uuid::now_v7(),
                alert_id: Uuid::now_v7(),
                destination_type: "slack".to_string(),
            },
        };

        let message: Message = original.into();
        let restored: AlertDeliveryMessage = message.try_into().unwrap();

        assert_eq!(restored.head.get("org_id").unwrap(), "test-org");
        assert_eq!(restored.head.get("env_id").unwrap(), "test-env");
        assert_eq!(restored.body.destination_type, "slack");
    }

    #[test]
    fn test_alert_delivery_message_wrong_type_fails() {
        // Create a message with the wrong __type
        let head = crate::val!({
            "__type": "EventMessage"
        });
        let msg = Message {
            id: Uuid::now_v7(),
            head,
            body: crate::val!({}),
        };

        let result: Result<AlertDeliveryMessage, String> = msg.try_into();
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .contains("Expected AlertDeliveryMessage")
        );
    }

    #[test]
    fn test_email_message_roundtrip() {
        let original = EmailMessage {
            id: Uuid::now_v7(),
            head: AHashMap::new(),
            body: EmailMessageBody {
                email_queue_id: Uuid::now_v7(),
                to_address: "user@example.com".to_string(),
                subject: "Test Subject".to_string(),
                html_body: Some("<p>Hello</p>".to_string()),
                text_body: Some("Hello".to_string()),
                from_address: "Hot Dev <hi@notifications.hot.dev>".to_string(),
            },
        };

        let original_id = original.id;
        let original_queue_id = original.body.email_queue_id;

        // Convert to Message
        let message: Message = original.into();

        // Verify type discriminator
        assert_eq!(message.head.get_str("__type"), "EmailMessage");

        // Convert back
        let restored: EmailMessage = message.try_into().unwrap();

        assert_eq!(restored.id, original_id);
        assert_eq!(restored.body.email_queue_id, original_queue_id);
        assert_eq!(restored.body.to_address, "user@example.com");
        assert_eq!(restored.body.subject, "Test Subject");
        assert_eq!(restored.body.html_body, Some("<p>Hello</p>".to_string()));
        assert_eq!(restored.body.text_body, Some("Hello".to_string()));
        assert_eq!(
            restored.body.from_address,
            "Hot Dev <hi@notifications.hot.dev>"
        );
    }

    #[test]
    fn test_email_message_with_no_body_content() {
        let original = EmailMessage {
            id: Uuid::now_v7(),
            head: AHashMap::new(),
            body: EmailMessageBody {
                email_queue_id: Uuid::now_v7(),
                to_address: "user@example.com".to_string(),
                subject: "No Body".to_string(),
                html_body: None,
                text_body: None,
                from_address: "test@example.com".to_string(),
            },
        };

        let message: Message = original.into();
        let restored: EmailMessage = message.try_into().unwrap();

        // None values become empty strings in Val, then back to None
        assert_eq!(restored.body.html_body, None);
        assert_eq!(restored.body.text_body, None);
    }

    #[test]
    fn test_email_message_wrong_type_fails() {
        let head = crate::val!({
            "__type": "DeploymentMessage"
        });
        let msg = Message {
            id: Uuid::now_v7(),
            head,
            body: crate::val!({}),
        };

        let result: Result<EmailMessage, String> = msg.try_into();
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Expected EmailMessage"));
    }
}

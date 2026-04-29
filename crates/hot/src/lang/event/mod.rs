use crate::val::Val;
use ahash::AHashSet;
use chrono::{DateTime, Utc};
use std::future::Future;
use std::pin::Pin;
use uuid::Uuid;

pub mod database;
pub mod queue;

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Event {
    pub event_id: Uuid,
    pub env_id: Uuid,
    pub stream_id: Uuid,
    pub event_type: String,
    pub event_data: Val,
    pub event_time: DateTime<Utc>,
    /// Optional target project for routing (used as tie-breaker when multiple builds have same function)
    /// Propagated from originating run's project context
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_project_id: Option<Uuid>,
    /// Target project name (for logging/debugging)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_project_name: Option<String>,
}

impl Event {
    pub fn new(env_id: Uuid, stream_id: Uuid, event_type: String, event_data: Val) -> Self {
        Self {
            event_id: Uuid::now_v7(),
            env_id,
            stream_id,
            event_type,
            event_data,
            event_time: Utc::now(),
            target_project_id: None,
            target_project_name: None,
        }
    }

    /// Create a new event with target project context (for routing)
    pub fn new_with_project(
        env_id: Uuid,
        stream_id: Uuid,
        event_type: String,
        event_data: Val,
        target_project_id: Option<Uuid>,
        target_project_name: Option<String>,
    ) -> Self {
        Self {
            event_id: Uuid::now_v7(),
            env_id,
            stream_id,
            event_type,
            event_data,
            event_time: Utc::now(),
            target_project_id,
            target_project_name,
        }
    }
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ExecutionContext {
    pub env_id: Option<Uuid>,
    pub env_name: Option<String>,
    pub user_id: Option<Uuid>,
    pub org_id: Option<Uuid>,
    pub org_slug: Option<String>,
    pub run_id: Uuid,
    pub stream_id: Uuid,
    pub run_type_id: i16,
    pub build_id: Option<Uuid>,
    pub build_hash: Option<String>,
    pub project_id: Option<Uuid>,
    pub project_name: Option<String>,
    pub event_id: Option<Uuid>,
    pub origin_run_id: Option<Uuid>,
    // Retry state (config is read from handler/schedule meta when needed)
    #[serde(default)]
    pub retry_attempt: i16,
    /// Context keys that are secrets (should be masked in call return values)
    #[serde(default)]
    pub secret_keys: AHashSet<String>,
    /// Secret value hashes that should be masked in call logs
    /// Populated when ctx/get returns a secret value
    /// We store hashes (u64) for efficient comparison and to handle any Val type
    #[serde(default)]
    pub secret_value_hashes: AHashSet<u64>,
    /// Access log ID from the API request that triggered this execution.
    /// Propagated from access_log_middleware → event → execution context → run.
    #[serde(default)]
    pub access_id: Option<Uuid>,
    /// Qualified agent type name (e.g. "::acme::support/SupportAgent") when the
    /// handler belongs to an agent via `meta {agent: "TypeName"}`.
    #[serde(default)]
    pub agent_type: Option<String>,
}

impl ExecutionContext {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        run_id: Uuid,
        stream_id: Uuid,
        run_type_id: i16,
        env_id: Option<Uuid>,
        user_id: Option<Uuid>,
        org_id: Option<Uuid>,
        build_id: Option<Uuid>,
    ) -> Self {
        Self {
            env_id,
            env_name: None,
            user_id,
            org_id,
            org_slug: None,
            run_id,
            stream_id,
            run_type_id,
            build_id,
            build_hash: None,
            project_id: None,
            project_name: None,
            event_id: None,
            origin_run_id: None,
            retry_attempt: 0,
            secret_keys: AHashSet::new(),
            secret_value_hashes: AHashSet::new(),
            access_id: None,
            agent_type: None,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new_with_event_and_origin(
        run_id: Uuid,
        stream_id: Uuid,
        run_type_id: i16,
        env_id: Option<Uuid>,
        user_id: Option<Uuid>,
        org_id: Option<Uuid>,
        build_id: Option<Uuid>,
        event_id: Option<Uuid>,
        origin_run_id: Option<Uuid>,
    ) -> Self {
        Self {
            env_id,
            env_name: None,
            user_id,
            org_id,
            org_slug: None,
            run_id,
            stream_id,
            run_type_id,
            build_id,
            build_hash: None,
            project_id: None,
            project_name: None,
            event_id,
            origin_run_id,
            retry_attempt: 0,
            secret_keys: AHashSet::new(),
            secret_value_hashes: AHashSet::new(),
            access_id: None,
            agent_type: None,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new_full(
        run_id: Uuid,
        stream_id: Uuid,
        run_type_id: i16,
        env_id: Option<Uuid>,
        env_name: Option<String>,
        user_id: Option<Uuid>,
        org_id: Option<Uuid>,
        build_id: Option<Uuid>,
        build_hash: Option<String>,
        project_id: Option<Uuid>,
        project_name: Option<String>,
        event_id: Option<Uuid>,
        origin_run_id: Option<Uuid>,
    ) -> Self {
        Self {
            env_id,
            env_name,
            user_id,
            org_id,
            org_slug: None,
            run_id,
            stream_id,
            run_type_id,
            build_id,
            build_hash,
            project_id,
            project_name,
            event_id,
            origin_run_id,
            retry_attempt: 0,
            secret_keys: AHashSet::new(),
            secret_value_hashes: AHashSet::new(),
            access_id: None,
            agent_type: None,
        }
    }

    pub fn minimal(run_id: Uuid, stream_id: Uuid, run_type_id: i16) -> Self {
        Self {
            env_id: None,
            env_name: None,
            user_id: None,
            org_id: None,
            org_slug: None,
            run_id,
            stream_id,
            run_type_id,
            build_id: None,
            build_hash: None,
            project_id: None,
            project_name: None,
            event_id: None,
            origin_run_id: None,
            retry_attempt: 0,
            secret_keys: AHashSet::new(),
            secret_value_hashes: AHashSet::new(),
            access_id: None,
            agent_type: None,
        }
    }

    /// Helper to set secret keys for masking ctx/get return values
    pub fn with_secret_keys(mut self, secret_keys: AHashSet<String>) -> Self {
        self.secret_keys = secret_keys;
        self
    }

    /// Helper to set project information after construction
    pub fn with_project(mut self, project_id: Option<Uuid>, project_name: Option<String>) -> Self {
        self.project_id = project_id;
        self.project_name = project_name;
        self
    }

    /// Helper to set build hash after construction
    pub fn with_build_hash(mut self, build_hash: Option<String>) -> Self {
        self.build_hash = build_hash;
        self
    }

    /// Helper to set environment name after construction
    pub fn with_env_name(mut self, env_name: Option<String>) -> Self {
        self.env_name = env_name;
        self
    }

    /// Helper to set organization slug after construction
    pub fn with_org_slug(mut self, org_slug: Option<String>) -> Self {
        self.org_slug = org_slug;
        self
    }

    /// Helper to set retry attempt after construction (for retries)
    pub fn with_retry_attempt(mut self, retry_attempt: i16) -> Self {
        self.retry_attempt = retry_attempt;
        self
    }

    /// Helper to set access_id after construction (for API request attribution)
    pub fn with_access_id(mut self, access_id: Option<Uuid>) -> Self {
        self.access_id = access_id;
        self
    }

    /// Helper to set agent_type after construction (for agent handler runs)
    pub fn with_agent_type(mut self, agent_type: Option<String>) -> Self {
        self.agent_type = agent_type;
        self
    }
}

pub trait EventPublisher: Send + Sync {
    fn publish(&self, ctx: &ExecutionContext, event: Event);

    fn shutdown(&self) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + '_>> {
        Box::pin(async { Ok(()) })
    }
}

// Re-export the publisher implementations
pub use database::DatabaseEventPublisher;
pub use queue::{
    DeploymentMessage, DeploymentMessageBody, EventMessage, EventMessageBody, MaintenanceMessage,
    MaintenanceMessageBody, QueueAndDatabaseEventPublisher, QueueEventPublisher,
    enqueue_deployment_message, enqueue_redeploy_for_project_reactivation,
};

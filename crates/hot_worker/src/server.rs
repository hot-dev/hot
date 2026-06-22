use ahash::AHashMap;
use futures::future::join_all;
use hot::data::msg::Message;
use hot::data::serialization::Serialization;
use hot::lang::event::EventMessage;
use hot::queue::{ProcessingQueue, ProcessingQueueLease, Queue, QueueType};
use hot::val;
use hot::val::Val;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::{Arc, RwLock};
use tokio::sync::{Mutex, mpsc, watch};
use tokio::task::JoinHandle;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

pub type DevContextStorage = Arc<RwLock<Option<AHashMap<String, hot::val::Val>>>>;

enum ClaimedWorkerQueue {
    Request(ProcessingQueueLease<Message>),
    Event(ProcessingQueueLease<Message>),
    Alert(ProcessingQueueLease<Message>),
    Email(ProcessingQueueLease<Message>),
}

fn spawn_worker_queue_claimer(
    queue_name: &'static str,
    queue: ProcessingQueue<Message>,
    claimed_tx: mpsc::Sender<ClaimedWorkerQueue>,
    mut shutdown_rx: watch::Receiver<bool>,
    wrap: fn(ProcessingQueueLease<Message>) -> ClaimedWorkerQueue,
) -> JoinHandle<Result<(), Box<dyn std::error::Error + Send + Sync>>> {
    tokio::spawn(async move {
        info!("hot.dev: WORKER {} claimer started", queue_name);

        loop {
            if *shutdown_rx.borrow() {
                break;
            }

            let lease = match &queue {
                ProcessingQueue::Memory(_) => {
                    tokio::select! {
                        biased;

                        changed = shutdown_rx.changed() => {
                            if changed.is_err() || *shutdown_rx.borrow() {
                                break;
                            }
                            continue;
                        }
                        result = queue.claim_blocking() => {
                            match result {
                                Ok(Some(lease)) => lease,
                                Ok(None) => continue,
                                Err(e) => {
                                    debug!("hot.dev: WORKER {} claimer error: {}", queue_name, e);
                                    continue;
                                }
                            }
                        }
                    }
                }
                ProcessingQueue::Redis(_) => match queue.claim_blocking().await {
                    Ok(Some(lease)) => lease,
                    Ok(None) => continue,
                    Err(e) => {
                        debug!("hot.dev: WORKER {} claimer error: {}", queue_name, e);
                        continue;
                    }
                },
            };

            if claimed_tx.send(wrap(lease)).await.is_err() {
                debug!(
                    "hot.dev: WORKER {} claimer stopping because executors are gone",
                    queue_name
                );
                break;
            }
        }

        info!("hot.dev: WORKER {} claimer stopped", queue_name);
        Ok(())
    })
}

// Add imports for database operations and event handler execution
use hot::db::{Build, Context, DatabasePool, Env, EventHandler, Project};
use hot::lang::event::ExecutionContext;

// Add import for context encryption
use hot::context_encryption::ContextEncryption;

// Add imports for emitter and event publisher
use hot::lang::emitter::EngineEventEmitter;
use hot::lang::emitter::{ConsoleEngineEventEmitter, DatabaseEngineEventEmitter};
use hot::lang::event::{DatabaseEventPublisher, EventPublisher, QueueAndDatabaseEventPublisher};

// Add imports for stream pub/sub
use hot::lang::hot::task::TaskRequest;
use hot::stream::{
    EnvEvent, EnvPublisher, StreamEvent, StreamPubSub, StreamPubSubType, StreamPublisher,
};

// Graceful shutdown module
mod graceful_shutdown {
    use super::*;
    use ahash::AHashSet;
    use hot::db::Run;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, RwLock};
    use tokio::time::{Duration, Instant, timeout};
    use tracing::{error, info, warn};
    use uuid::Uuid;

    /// Tracks the shutdown state and active runs for graceful shutdown
    #[derive(Clone)]
    pub struct ShutdownCoordinator {
        /// Active run IDs being processed by all workers
        active_runs: Arc<RwLock<AHashSet<Uuid>>>,
        /// Cancellation tokens for active VMs, keyed by run ID
        active_cancel_tokens: Arc<RwLock<AHashMap<Uuid, Arc<AtomicBool>>>>,
        /// Shutdown timeout duration
        timeout_duration: Duration,
        /// Shutdown initiated flag
        shutdown_initiated: Arc<RwLock<bool>>,
    }

    impl ShutdownCoordinator {
        /// Create a new shutdown coordinator
        pub fn new(timeout_secs: u64) -> Self {
            Self {
                active_runs: Arc::new(RwLock::new(AHashSet::new())),
                active_cancel_tokens: Arc::new(RwLock::new(AHashMap::new())),
                timeout_duration: Duration::from_secs(timeout_secs),
                shutdown_initiated: Arc::new(RwLock::new(false)),
            }
        }

        /// Register a run as active (being processed)
        pub fn register_run(&self, run_id: Uuid) {
            if let Ok(mut runs) = self.active_runs.write() {
                runs.insert(run_id);
            }
        }

        /// Unregister a run when it completes
        pub fn unregister_run(&self, run_id: &Uuid) {
            if let Ok(mut runs) = self.active_runs.write() {
                runs.remove(run_id);
            }
            if let Ok(mut tokens) = self.active_cancel_tokens.write() {
                tokens.remove(run_id);
            }
        }

        pub fn register_cancel_token(&self, run_id: Uuid, token: Arc<AtomicBool>) {
            if let Ok(mut tokens) = self.active_cancel_tokens.write() {
                tokens.insert(run_id, token);
            }
        }

        fn cancel_active_tokens(&self, reason: &str) {
            let tokens: Vec<Arc<AtomicBool>> = self
                .active_cancel_tokens
                .read()
                .map(|tokens| tokens.values().cloned().collect())
                .unwrap_or_default();
            if tokens.is_empty() {
                return;
            }

            warn!(
                "Signaling cancellation to {} active VM run(s): {}",
                tokens.len(),
                reason
            );
            for token in tokens {
                token.store(true, Ordering::Relaxed);
            }
        }

        /// Check if shutdown has been initiated
        pub fn is_shutting_down(&self) -> bool {
            self.shutdown_initiated.read().map(|s| *s).unwrap_or(false)
        }

        /// Get count of active runs
        pub fn active_run_count(&self) -> usize {
            self.active_runs.read().map(|runs| runs.len()).unwrap_or(0)
        }

        /// Initiate graceful shutdown
        /// Phase 1: Stop dequeueing (handled by caller checking is_shutting_down)
        /// Phase 2: Wait for in-flight work to complete or timeout
        /// Phase 3: Cancel remaining runs on timeout
        pub async fn initiate_shutdown(&self, db: Option<&DatabasePool>) -> Result<(), String> {
            // Mark shutdown as initiated
            if let Ok(mut shutdown) = self.shutdown_initiated.write() {
                if *shutdown {
                    info!("Shutdown already initiated, skipping duplicate signal");
                    return Ok(());
                }
                *shutdown = true;
            }

            let active_count = self.active_run_count();
            if active_count == 0 {
                info!("No active runs, proceeding with immediate shutdown");
                return Ok(());
            }

            info!(
                "Worker shutdown initiated, waiting for {} in-flight runs to complete (timeout: {}s)",
                active_count,
                self.timeout_duration.as_secs()
            );

            // Wait for completion or timeout
            let start = Instant::now();
            let result = timeout(self.timeout_duration, async {
                loop {
                    let remaining = self.active_run_count();
                    if remaining == 0 {
                        info!("All in-flight work completed, shutting down cleanly");
                        return Ok(());
                    }

                    let elapsed = start.elapsed().as_secs();
                    if elapsed.is_multiple_of(5) && elapsed > 0 {
                        info!(
                            "Waiting for {} runs to complete ({}/{}s)",
                            remaining,
                            elapsed,
                            self.timeout_duration.as_secs()
                        );
                    }

                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
            })
            .await;

            match result {
                Ok(Ok(())) => {
                    info!("Graceful shutdown completed successfully");
                    Ok(())
                }
                Ok(Err(e)) => Err(e),
                Err(_) => {
                    // Timeout reached
                    warn!(
                        "Shutdown timeout reached after {}s",
                        self.timeout_duration.as_secs()
                    );
                    self.cancel_active_tokens("worker shutdown timeout");

                    // Fail remaining active runs (marks as Failed so retry can kick in)
                    if let Some(db) = db {
                        let failed_count = self.fail_active_runs(db).await?;
                        warn!(
                            "Failed {} remaining runs due to shutdown timeout (will retry if configured)",
                            failed_count
                        );
                    } else {
                        warn!("Database not available, cannot fail remaining runs");
                    }

                    Ok(())
                }
            }
        }

        /// Fail all active runs due to shutdown timeout
        /// Marks runs as Failed (not Cancelled) so retry logic can kick in for functions with retry meta
        async fn fail_active_runs(&self, db: &DatabasePool) -> Result<usize, String> {
            let active_runs: Vec<Uuid> = self
                .active_runs
                .read()
                .map(|runs| runs.iter().copied().collect())
                .unwrap_or_default();

            let mut failed_count = 0;

            for run_id in active_runs {
                match Run::fail_run(db, &run_id, "Run interrupted by worker shutdown").await {
                    Ok(_) => {
                        warn!("Failed run {} due to shutdown timeout", run_id);
                        failed_count += 1;
                        // Unregister the run
                        self.unregister_run(&run_id);
                    }
                    Err(e) => {
                        error!("Failed to mark run {} as failed: {}", run_id, e);
                    }
                }
            }

            Ok(failed_count)
        }
    }
}

use graceful_shutdown::ShutdownCoordinator;

/// Cache for extracted build paths
/// Maps build_id -> extracted directory path
/// Uses per-build locks to prevent race conditions during extraction (both in-process and cross-process).
///
/// The outer `extraction_locks` map uses parking_lot (no poisoning) because it is shared
/// across worker requests and a panic in one extraction must not permanently break the cache.
#[derive(Clone)]
struct BuildPathCache {
    /// Completed extractions: build_id -> extracted path
    paths: Arc<RwLock<AHashMap<Uuid, PathBuf>>>,
    /// Per-build extraction locks to ensure only one thread extracts at a time (in-process)
    extraction_locks: Arc<parking_lot::Mutex<AHashMap<Uuid, Arc<tokio::sync::Mutex<()>>>>>,
}

impl BuildPathCache {
    fn new() -> Self {
        Self {
            paths: Arc::new(RwLock::new(AHashMap::new())),
            extraction_locks: Arc::new(parking_lot::Mutex::new(AHashMap::new())),
        }
    }

    fn get(&self, build_id: &Uuid) -> Option<PathBuf> {
        self.paths.read().ok()?.get(build_id).cloned()
    }

    fn insert(&self, build_id: Uuid, path: PathBuf) {
        if let Ok(mut paths) = self.paths.write() {
            paths.insert(build_id, path);
        }
    }

    fn remove(&self, build_id: &Uuid) {
        if let Ok(mut paths) = self.paths.write() {
            paths.remove(build_id);
        }
    }

    /// Remove cache entries for builds that are no longer deployed.
    /// Called periodically to prevent unbounded memory growth.
    fn retain_only(&self, valid_build_ids: &ahash::AHashSet<Uuid>) {
        // Clean up paths cache
        if let Ok(mut paths) = self.paths.write() {
            let before_count = paths.len();
            paths.retain(|build_id, _| valid_build_ids.contains(build_id));
            let removed = before_count - paths.len();
            if removed > 0 {
                tracing::debug!(
                    "BuildPathCache: pruned {} stale path entries, {} remaining",
                    removed,
                    paths.len()
                );
            }
        }

        // Clean up extraction locks for old builds
        {
            let mut locks = self.extraction_locks.lock();
            let before_count = locks.len();
            locks.retain(|build_id, _| valid_build_ids.contains(build_id));
            let removed = before_count - locks.len();
            if removed > 0 {
                tracing::debug!(
                    "BuildPathCache: pruned {} stale lock entries, {} remaining",
                    removed,
                    locks.len()
                );
            }
        }
    }

    /// Get or create an in-process extraction lock for a build.
    /// Callers should acquire this lock before extracting to prevent thread races.
    fn get_extraction_lock(&self, build_id: &Uuid) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = self.extraction_locks.lock();
        locks
            .entry(*build_id)
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    /// Check if extraction is complete by looking for the completion marker file.
    /// This is used for cross-process coordination - another process may have extracted.
    fn is_extraction_complete(extract_dir: &std::path::Path) -> bool {
        extract_dir.join(".extraction_complete").exists()
    }

    /// Mark extraction as complete by creating a marker file.
    /// This signals to other processes that the extraction is done.
    fn mark_extraction_complete(extract_dir: &std::path::Path) {
        let marker = extract_dir.join(".extraction_complete");
        if let Err(e) = std::fs::write(&marker, "") {
            tracing::warn!("Failed to write extraction marker: {}", e);
        }
    }

    /// Acquire a cross-process file lock for extraction.
    /// Returns the lock file handle (which releases the lock when dropped).
    fn acquire_file_lock(
        build_id: &Uuid,
    ) -> Result<fd_lock::RwLock<std::fs::File>, std::io::Error> {
        // Ensure .hot/run directory exists
        let lock_dir = std::path::PathBuf::from(".hot/run");
        std::fs::create_dir_all(&lock_dir)?;

        let lock_path = lock_dir.join(format!("build-{}.lock", build_id.as_simple()));
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)?;

        Ok(fd_lock::RwLock::new(file))
    }
}

// Request message type for hot:request queue
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestMessage {
    pub id: Uuid,
    pub head: AHashMap<String, String>,
    pub body: serde_json::Value,
}

impl From<RequestMessage> for Message {
    fn from(request_msg: RequestMessage) -> Self {
        // Convert head HashMap to Val
        let head_val = Val::from(
            request_msg
                .head
                .into_iter()
                .map(|(k, v)| (Val::from(k), Val::from(v)))
                .collect::<Vec<_>>(),
        );

        // Create head with type discriminator and head
        let head = val!({
            "__type": "RequestMessage",
            "head": head_val
        });

        // Convert JSON Value to Val - this is a simplified conversion
        let body_val = match serde_json::to_string(&request_msg.body) {
            Ok(json_str) => Val::from(json_str),
            Err(_) => Val::Null,
        };

        Message {
            id: request_msg.id,
            head,
            body: body_val,
        }
    }
}

impl TryFrom<Message> for RequestMessage {
    type Error = String;

    fn try_from(msg: Message) -> Result<Self, Self::Error> {
        // Debug: log the actual message structure
        tracing::debug!(
            "Attempting to parse RequestMessage from: head={}, body={}",
            msg.head.pretty_print(),
            msg.body.pretty_print()
        );

        // Check type discriminator
        let msg_type = msg.head.get_str("__type");
        if msg_type != "RequestMessage" {
            return Err(format!(
                "Expected RequestMessage type, got: '{}'. Message head: {}",
                msg_type,
                msg.head.pretty_print()
            ));
        }

        // Extract head - handle both old "headers" and new "head" field names for backwards compatibility
        let head_val = msg
            .head
            .get("head")
            .or_else(|| msg.head.get("headers")) // Fallback for old messages
            .ok_or_else(|| {
                format!(
                    "Missing head/headers in RequestMessage. Message head: {}",
                    msg.head.pretty_print()
                )
            })?;

        let mut head = AHashMap::new();
        if let Val::Map(header_map) = head_val {
            for (k, v) in header_map.iter() {
                if let (Val::Str(key), Val::Str(value)) = (k, v) {
                    head.insert((*key).to_string(), (*value).to_string());
                }
            }
        }

        // Convert body from Val to JSON Value
        let body = if let Val::Str(json_str) = &msg.body {
            serde_json::from_str(json_str)
                .map_err(|e| format!("Failed to parse JSON body: {}", e))?
        } else {
            // Try to convert Val directly to JSON Value
            match serde_json::to_value(&msg.body) {
                Ok(json_val) => json_val,
                Err(e) => return Err(format!("Failed to convert body to JSON: {}", e)),
            }
        };

        Ok(RequestMessage {
            id: msg.id,
            head,
            body,
        })
    }
}

// Response message type for hot:response queue
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseMessage {
    pub id: Uuid,
    pub head: AHashMap<String, String>,
    pub body: serde_json::Value,
}

impl From<ResponseMessage> for Message {
    fn from(response_msg: ResponseMessage) -> Self {
        // Convert head HashMap to Val
        let head_val = Val::from(
            response_msg
                .head
                .into_iter()
                .map(|(k, v)| (Val::from(k), Val::from(v)))
                .collect::<Vec<_>>(),
        );

        // Create head with type discriminator and head
        let head = val!({
            "__type": "ResponseMessage",
            "head": head_val
        });

        // Convert JSON Value to Val - this is a simplified conversion
        let body_val = match serde_json::to_string(&response_msg.body) {
            Ok(json_str) => Val::from(json_str),
            Err(_) => Val::Null,
        };

        Message {
            id: response_msg.id,
            head,
            body: body_val,
        }
    }
}

impl TryFrom<Message> for ResponseMessage {
    type Error = String;

    fn try_from(msg: Message) -> Result<Self, Self::Error> {
        // Debug: log the actual message structure
        tracing::debug!(
            "Attempting to parse ResponseMessage from: head={}, body={}",
            msg.head.pretty_print(),
            msg.body.pretty_print()
        );

        // Check type discriminator
        let msg_type = msg.head.get_str("__type");
        if msg_type != "ResponseMessage" {
            return Err(format!(
                "Expected ResponseMessage type, got: '{}'. Message head: {}",
                msg_type,
                msg.head.pretty_print()
            ));
        }

        // Extract head - handle both old "headers" and new "head" field names for backwards compatibility
        let head_val = msg
            .head
            .get("head")
            .or_else(|| msg.head.get("headers")) // Fallback for old messages
            .ok_or_else(|| {
                format!(
                    "Missing head/headers in ResponseMessage. Message head: {}",
                    msg.head.pretty_print()
                )
            })?;

        let mut head = AHashMap::new();
        if let Val::Map(header_map) = head_val {
            for (k, v) in header_map.iter() {
                if let (Val::Str(key), Val::Str(value)) = (k, v) {
                    head.insert((*key).to_string(), (*value).to_string());
                }
            }
        }

        // Convert body from Val to JSON Value
        let body = if let Val::Str(json_str) = &msg.body {
            serde_json::from_str(json_str)
                .map_err(|e| format!("Failed to parse JSON body: {}", e))?
        } else {
            // Try to convert Val directly to JSON Value
            match serde_json::to_value(&msg.body) {
                Ok(json_val) => json_val,
                Err(e) => return Err(format!("Failed to convert body to JSON: {}", e)),
            }
        };

        Ok(ResponseMessage {
            id: msg.id,
            head,
            body,
        })
    }
}

pub const DEFAULT_WORKER_THREADS: usize = 2;
pub const DEFAULT_RUN_TIMEOUT_SECONDS: u64 = 300; // 5 minutes
pub const DEFAULT_QUEUE_TYPE: QueueType = QueueType::Memory;
pub const DEFAULT_SERIALIZATION: Serialization = Serialization::ZstdJson; // must match Serialization's #[default]

// Simplified approach: ensure proper build isolation without complex caching
// Each build execution will be isolated by using fresh compilation contexts

// Helper functions for creating emitter and event publisher

/// Create a emitter based on worker configuration
fn create_emitter(
    conf: &Val,
    db_pool: &DatabasePool,
) -> Result<Option<std::sync::Arc<dyn EngineEventEmitter>>, String> {
    // Get resolved emitter configuration (hot.emitter.type in config becomes emitter.type after load_conf)
    let emitter_conf = conf.get("emitter").unwrap_or_else(Val::map_empty);

    // Get emitter type
    let emitter_type = emitter_conf.get_str("type");

    tracing::info!(
        "create_emitter: emitter_conf={:?}, emitter_type='{}'",
        emitter_conf,
        emitter_type
    );

    // Return None if emitter type is "none" or empty
    if emitter_type == "none" || emitter_type.is_empty() {
        tracing::warn!(
            "create_emitter: returning None (type is '{}' or empty)",
            emitter_type
        );
        return Ok(None);
    }

    // Get filter configuration
    let filter_conf = emitter_conf.get("filter");

    // Create the base emitter and wrap with filtering based on type
    match emitter_type.as_str() {
        "console" => {
            tracing::info!("create_emitter: creating console emitter");
            let console_emitter = ConsoleEngineEventEmitter::new();
            let filtered_emitter =
                hot::lang::emitter::FilteredEmitter::new(console_emitter, filter_conf.as_ref())?;
            Ok(Some(std::sync::Arc::new(filtered_emitter)))
        }
        "db" => {
            tracing::info!("create_emitter: creating db emitter");
            // Use existing database pool instead of creating a new one
            // Note: stream_data is no longer persisted to DB - delivered via Redis Streams only
            let db_emitter = DatabaseEngineEventEmitter::new_with_pool(db_pool.clone());
            let filtered_emitter =
                hot::lang::emitter::FilteredEmitter::new(db_emitter, filter_conf.as_ref())?;
            Ok(Some(std::sync::Arc::new(filtered_emitter)))
        }
        unknown => Err(format!(
            "Unknown emitter type: {}. Available options: none, console, db",
            unknown
        )),
    }
}

/// Create a event publisher based on worker configuration
fn create_event_publisher(
    conf: &Val,
    db_pool: &DatabasePool,
) -> Result<Option<std::sync::Arc<dyn EventPublisher>>, String> {
    // Extract queue configuration
    let queue_type_str = conf.get_str("queue.type");
    let queue_type = QueueType::from_str(&queue_type_str).unwrap_or(QueueType::Memory);

    let redis_uri_str = conf.get_str("redis.uri");
    let redis_uri = if redis_uri_str == "null" || redis_uri_str.is_empty() {
        None
    } else {
        Some(redis_uri_str)
    };

    // Extract cluster mode configuration
    let redis_cluster = conf.get_bool_or_default("redis.cluster", false);

    let serialization_str = conf.get_str("serialization.type");
    let serialization = Serialization::from_str(&serialization_str).unwrap_or_default();

    // Create database publisher with existing pool (ensures connection is ready)
    let database_publisher = DatabaseEventPublisher::new_with_pool(db_pool.clone());

    // Create queue publisher with extracted configuration including cluster mode
    let queue_publisher = hot::lang::event::QueueEventPublisher::new_with_cluster(
        queue_type,
        "hot:event".to_string(),
        redis_uri,
        redis_cluster,
        serialization,
    );

    // Create combined publisher
    let combined_publisher =
        QueueAndDatabaseEventPublisher::new(queue_publisher, database_publisher);
    Ok(Some(std::sync::Arc::new(combined_publisher)))
}

// Helper functions for event handler execution

/// Find ALL deployed builds for an environment (for multi-project support)
async fn find_all_deployed_builds_for_env(
    db: &DatabasePool,
    env_id: &Uuid,
) -> Result<Vec<Build>, String> {
    let projects = Project::get_projects_by_env(db, env_id, None, None)
        .await
        .map_err(|e| format!("Failed to get projects for environment: {}", e))?;

    let mut deployed_builds = Vec::new();
    for project in projects {
        if let Ok(Some(deployed_build)) =
            Build::get_deployed_build_by_project(db, &project.project_id).await
        {
            deployed_builds.push(deployed_build);
        }
    }

    Ok(deployed_builds)
}

/// Extract the target function name from hot:call or hot:schedule event data
fn extract_target_function_from_event(event_data: &Val) -> Option<String> {
    // Event data structure: { "fn": "::namespace/function", "args": [...] }
    match event_data {
        Val::Map(map) => map.get(&Val::from("fn")).and_then(|v| match v {
            Val::Str(s) => Some((*s).to_string()),
            _ => None,
        }),
        _ => None,
    }
}

/// IDs extracted from an orphaned queue message
#[derive(Debug, Clone)]
struct OrphanedMessageIds {
    /// The event_id (message.id for EventMessage)
    event_id: uuid::Uuid,
    /// The stream_id from execution_context (if available)
    stream_id: Option<uuid::Uuid>,
}

/// Extract event_id and stream_id from orphaned queue data.
/// The data might be either:
/// - A raw Message (JSON)
/// - A RetryWrapper<Message> (JSON, from the retry mechanism)
/// - Zstd compressed version of either
fn extract_ids_from_orphaned_data(raw_bytes: &[u8]) -> Option<OrphanedMessageIds> {
    use hot::data::msg::Message;
    use serde::Deserialize;
    use std::io::Read;

    // RetryWrapper structure (matches the one in queue/redis.rs)
    #[derive(Deserialize)]
    struct RetryWrapper<T> {
        item: T,
        #[allow(dead_code)]
        retry_count: usize,
    }

    // Helper to extract stream_id from message body
    fn extract_stream_id(msg: &Message) -> Option<uuid::Uuid> {
        // Try execution_context.stream_id first
        if let Some(ctx) = msg.body.get("execution_context")
            && let Some(stream_id_val) = ctx.get("stream_id")
        {
            let id_str = stream_id_val.get_str("");
            if !id_str.is_empty()
                && let Ok(uuid) = uuid::Uuid::parse_str(&id_str)
            {
                return Some(uuid);
            }
        }
        // Try event.stream_id as fallback
        if let Some(event) = msg.body.get("event")
            && let Some(stream_id_val) = event.get("stream_id")
        {
            let id_str = stream_id_val.get_str("");
            if !id_str.is_empty()
                && let Ok(uuid) = uuid::Uuid::parse_str(&id_str)
            {
                return Some(uuid);
            }
        }
        None
    }

    // Helper to create OrphanedMessageIds from a Message
    fn ids_from_message(msg: &Message) -> OrphanedMessageIds {
        OrphanedMessageIds {
            event_id: msg.id,
            stream_id: extract_stream_id(msg),
        }
    }

    // Try to decompress if it's zstd compressed
    // Zstd magic number: 0x28, 0xB5, 0x2F, 0xFD
    let data = if raw_bytes.len() >= 4 && raw_bytes[0..4] == [0x28, 0xb5, 0x2f, 0xfd] {
        // Zstd compressed - decompress first
        let mut decoder = match zstd::Decoder::new(raw_bytes) {
            Ok(d) => d,
            Err(_) => return None,
        };
        let mut buf = Vec::new();
        if decoder.read_to_end(&mut buf).is_err() {
            return None;
        }
        buf
    } else {
        raw_bytes.to_vec()
    };

    // Try to deserialize as RetryWrapper<Message> first (JSON)
    if let Ok(wrapper) = serde_json::from_slice::<RetryWrapper<Message>>(&data) {
        return Some(ids_from_message(&wrapper.item));
    }

    // Try to deserialize as raw Message (JSON)
    if let Ok(msg) = serde_json::from_slice::<Message>(&data) {
        return Some(ids_from_message(&msg));
    }

    // Could not extract IDs
    tracing::debug!("Could not extract IDs from orphaned data");
    None
}

/// Result of checking if a function exists in bytecode cache
#[derive(Debug, Clone, PartialEq)]
enum CacheCheckResult {
    /// Function was found in cache
    Found,
    /// Function was not found (cache loaded successfully, function just doesn't exist)
    NotFound,
    /// Cache exists but has version mismatch - needs recompilation
    NeedsRecompile,
    /// Cache doesn't exist at all
    CacheNotFound,
}

/// Check if a cached bytecode contains a specific function
fn bytecode_has_function(
    cache: &hot::lang::cache::bytecode_cache::BytecodeCache,
    cache_key: &str,
    function_name: &str,
) -> CacheCheckResult {
    // Only check if cache exists to avoid expensive disk I/O
    if !cache.exists(cache_key) {
        tracing::debug!(
            "ROUTING: Cache does not exist for key {}",
            &cache_key[..12.min(cache_key.len())]
        );
        return CacheCheckResult::CacheNotFound;
    }

    // Try to load from cache
    match cache.load(cache_key) {
        Ok(cached) => {
            // Check if the function exists in the function mapping
            // Function names in mapping are like "::namespace/function" or "::namespace/function/arity"
            if cached.function_mapping.contains_key(function_name) {
                tracing::debug!(
                    "ROUTING: Found exact match for function '{}'",
                    function_name
                );
                return CacheCheckResult::Found;
            }
            // Also try with various arities
            for arity in 0..=5 {
                let arity_key = format!("{}/{}", function_name, arity);
                if cached.function_mapping.contains_key(&arity_key) {
                    tracing::debug!(
                        "ROUTING: Found arity match for function '{}' at '{}'",
                        function_name,
                        arity_key
                    );
                    return CacheCheckResult::Found;
                }
            }
            // Log some functions from the mapping to help debug
            let sample_functions: Vec<_> = cached
                .function_mapping
                .keys()
                .filter(|k| k.contains("spark") || k.contains("nextjs"))
                .take(10)
                .collect();
            tracing::debug!(
                "ROUTING: Function '{}' not found. Sample matching functions: {:?}",
                function_name,
                sample_functions
            );
            CacheCheckResult::NotFound
        }
        Err(e) => {
            // Check if this is a version mismatch error
            if e.contains("version mismatch") {
                tracing::warn!(
                    "ROUTING: Cache version mismatch, needs recompilation: {}",
                    e
                );
                CacheCheckResult::NeedsRecompile
            } else {
                tracing::warn!("ROUTING: Failed to load cache: {}", e);
                CacheCheckResult::CacheNotFound
            }
        }
    }
}

/// Recompile a bundle build's bytecode cache
/// Returns true if recompilation succeeded
fn recompile_bundle_cache(
    extracted_path: &std::path::Path,
    manifest: &hot::bundle::BundleManifest,
) -> bool {
    tracing::info!(
        "ROUTING: Recompiling bundle cache for '{}' due to version mismatch",
        manifest.bundle_name
    );

    // Derive source paths from standard bundle structure (same as extraction code)
    let build_src_path = extracted_path.join("hot/src");
    let build_pkg_path = extracted_path.join("hot/pkg");
    let mut paths = vec![build_src_path.to_string_lossy().to_string()];

    // Include pkg path if it exists (bundled dependencies)
    if build_pkg_path.exists() {
        paths.push(build_pkg_path.to_string_lossy().to_string());
    }

    if !build_src_path.exists() {
        tracing::error!(
            "ROUTING: Bundle source path {:?} does not exist",
            build_src_path
        );
        return false;
    }

    // Clear stale cache files before recompiling
    // Bundle may contain embedded cache from an older version that needs to be replaced
    let bundle_cache_dir = extracted_path.join(".hot").join("cache");
    if bundle_cache_dir.exists()
        && let Ok(entries) = std::fs::read_dir(&bundle_cache_dir)
    {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.to_string_lossy().ends_with(".bc.zst") {
                if let Err(e) = std::fs::remove_file(&path) {
                    tracing::warn!(
                        "ROUTING: Failed to remove stale cache file {:?}: {}",
                        path,
                        e
                    );
                } else {
                    tracing::debug!("ROUTING: Removed stale cache file {:?}", path);
                }
            }
        }
    }

    let bundle_cache = hot::lang::cache::bytecode_cache::BytecodeCache::new(bundle_cache_dir);

    // Recompile
    match hot::lang::engine::Engine::compile_to_cache(
        &paths,
        &bundle_cache,
        &manifest.bundle_name,
        manifest.cache_key.as_deref(),
        Some(manifest.file_hashes.clone()),
        None, // Bundle builds have deps pre-bundled
    ) {
        Ok(()) => {
            tracing::info!(
                "ROUTING: Bundle '{}' recompiled successfully",
                manifest.bundle_name
            );
            true
        }
        Err(e) => {
            tracing::error!(
                "ROUTING: Failed to recompile bundle '{}': {}",
                manifest.bundle_name,
                e
            );
            false
        }
    }
}

/// Recompile a live build's bytecode cache
/// Returns true if recompilation succeeded
fn recompile_live_build_cache(
    cache: &hot::lang::cache::bytecode_cache::BytecodeCache,
    src_paths: &[String],
    project_name: &str,
    cache_key: &str,
    file_hashes: &[hot::lang::cache::bytecode_cache::FileHash],
    conf: Option<&hot::val::Val>,
) -> bool {
    tracing::info!(
        "ROUTING: Recompiling live build cache for '{}' (key={})",
        project_name,
        &cache_key[..12.min(cache_key.len())]
    );

    if src_paths.is_empty() {
        tracing::error!("ROUTING: No source paths for live build recompilation");
        return false;
    }

    // Recompile using the pre-calculated cache key to ensure consistency
    // For live builds, pass conf so dependencies can be resolved from project config.
    match hot::lang::engine::Engine::compile_to_cache(
        src_paths,
        cache,
        project_name,
        Some(cache_key), // Use the pre-calculated cache key
        Some(file_hashes.to_vec()),
        conf, // Pass config for live build dependency resolution
    ) {
        Ok(()) => {
            tracing::info!(
                "ROUTING: Live build '{}' recompiled successfully",
                project_name
            );
            true
        }
        Err(e) => {
            tracing::error!(
                "ROUTING: Failed to recompile live build '{}': {}",
                project_name,
                e
            );
            false
        }
    }
}

/// Extract the qualified agent type name from an event handler's meta.
/// If the handler has `meta {agent: "SupportAgent"}`, returns the qualified name
/// by combining the handler's namespace with the type name.
fn extract_agent_type_from_handler(handler: &EventHandler) -> Option<String> {
    let meta = handler.meta.as_ref()?;
    let agent_val = meta.get("agent")?;
    let agent_name = agent_val.as_str()?;
    Some(format!("{}/{}", handler.ns, agent_name))
}

/// Check if an event handler has the `once: true` metadata flag
fn handler_has_once_flag(handler: &EventHandler) -> bool {
    if let Some(ref meta) = handler.meta
        && let Some(once_val) = meta.get("once")
    {
        return once_val.as_bool().unwrap_or(false);
    }
    false
}

/// Partition event handlers into (once_handlers, multi_handlers)
/// - once_handlers: handlers with `once: true` in metadata (run only one)
/// - multi_handlers: handlers without the flag (run all of them)
fn partition_handlers_by_once_flag(
    handlers: Vec<EventHandler>,
) -> (Vec<EventHandler>, Vec<EventHandler>) {
    let mut once_handlers = Vec::new();
    let mut multi_handlers = Vec::new();

    for handler in handlers {
        if handler_has_once_flag(&handler) {
            once_handlers.push(handler);
        } else {
            multi_handlers.push(handler);
        }
    }

    (once_handlers, multi_handlers)
}

/// Result of routing a function call to a build
#[derive(Debug)]
struct RoutingResult {
    /// The selected build to execute the function
    build: Build,
    /// Project name for logging
    project_name: String,
    /// Whether a tie-breaker was used (multiple builds had the function)
    tie_breaker_used: bool,
    /// Names of all projects that had the function (for warning info)
    matched_project_names: Vec<String>,
}

/// Get the extracted path for a bundle build.
/// Returns None if the bundle is not yet extracted or extraction failed.
async fn get_bundle_extracted_path(
    build: &Build,
    build_path_cache: &Arc<BuildPathCache>,
) -> Option<std::path::PathBuf> {
    // Helper to check if bytecode cache is ready
    let bytecode_ready = |dir: &std::path::Path| -> bool {
        let cache_dir = dir.join(".hot").join("cache");
        cache_dir.exists()
            && std::fs::read_dir(&cache_dir)
                .map(|mut d| d.next().is_some())
                .unwrap_or(false)
    };

    // First check in-memory cache
    if let Some(path) = build_path_cache.get(&build.build_id) {
        let cache_dir = path.join(".hot").join("cache");
        if cache_dir.exists()
            && std::fs::read_dir(&cache_dir)
                .map(|mut d| d.next().is_some())
                .unwrap_or(false)
        {
            tracing::debug!(
                "ROUTING: Bundle {} found in memory cache at {:?}",
                build.build_id,
                path
            );
            return Some(path);
        } else {
            // Cache not ready - remove from memory cache
            tracing::debug!(
                "ROUTING: Bundle {} in memory cache but bytecode not ready, removing",
                build.build_id
            );
            build_path_cache.remove(&build.build_id);
        }
    }

    // Check disk
    let extract_dir =
        std::path::PathBuf::from(format!(".hot/run/build-{}", build.build_id.simple()));
    tracing::debug!(
        "ROUTING: Checking for extracted bundle at {:?} (exists={})",
        extract_dir,
        extract_dir.exists()
    );

    if BuildPathCache::is_extraction_complete(&extract_dir) {
        if bytecode_ready(&extract_dir) {
            tracing::debug!(
                "ROUTING: Bundle {} found on disk with bytecode, adding to cache",
                build.build_id
            );
            build_path_cache.insert(build.build_id, extract_dir.clone());
            return Some(extract_dir);
        } else {
            // Extraction complete but bytecode not ready - wait on extraction lock
            tracing::debug!(
                "ROUTING: Bundle {} extracted but bytecode not ready, waiting for lock",
                build.build_id
            );
            let extraction_lock = build_path_cache.get_extraction_lock(&build.build_id);
            match tokio::time::timeout(std::time::Duration::from_secs(5), extraction_lock.lock())
                .await
            {
                Ok(_guard) => {
                    if bytecode_ready(&extract_dir) {
                        tracing::debug!(
                            "ROUTING: Bundle {} bytecode ready after lock wait",
                            build.build_id
                        );
                        build_path_cache.insert(build.build_id, extract_dir.clone());
                        return Some(extract_dir);
                    } else {
                        tracing::warn!(
                            "ROUTING: Bundle {} bytecode still not ready after lock wait",
                            build.build_id
                        );
                    }
                }
                Err(_) => {
                    tracing::warn!(
                        "ROUTING: Bundle {} timed out waiting for extraction lock",
                        build.build_id
                    );
                }
            }
        }
    } else if extract_dir.exists() {
        // Directory exists but no marker - extraction in progress, wait
        tracing::debug!(
            "ROUTING: Bundle {} extraction in progress, waiting for lock",
            build.build_id
        );
        let extraction_lock = build_path_cache.get_extraction_lock(&build.build_id);
        match tokio::time::timeout(std::time::Duration::from_secs(5), extraction_lock.lock()).await
        {
            Ok(_guard) => {
                if BuildPathCache::is_extraction_complete(&extract_dir)
                    && bytecode_ready(&extract_dir)
                {
                    tracing::debug!(
                        "ROUTING: Bundle {} ready after waiting for extraction",
                        build.build_id
                    );
                    build_path_cache.insert(build.build_id, extract_dir.clone());
                    return Some(extract_dir);
                } else {
                    tracing::warn!(
                        "ROUTING: Bundle {} still not ready after waiting",
                        build.build_id
                    );
                }
            }
            Err(_) => {
                tracing::warn!(
                    "ROUTING: Bundle {} timed out waiting for extraction",
                    build.build_id
                );
            }
        }
    }

    None
}

/// Find the build that contains a specific function (for hot:call routing)
/// Returns routing result with the selected build and whether tie-breaker was used.
/// Uses target_project_id as tie-breaker when multiple builds have the same function.
async fn find_build_for_function(
    db: &DatabasePool,
    env_id: &Uuid,
    function_name: &str,
    cache: &hot::lang::cache::bytecode_cache::BytecodeCache,
    worker_conf: &Val,
    target_project_id: Option<Uuid>,
    build_path_cache: &Arc<BuildPathCache>,
) -> Result<Option<RoutingResult>, String> {
    let deployed_builds = find_all_deployed_builds_for_env(db, env_id).await?;

    // Clean up memory cache: remove entries for builds that are no longer deployed
    let deployed_build_ids: ahash::AHashSet<Uuid> =
        deployed_builds.iter().map(|b| b.build_id).collect();
    build_path_cache.retain_only(&deployed_build_ids);

    // Find ALL builds that have this function
    let mut matching_builds: Vec<(Build, String)> = Vec::new();

    for build in deployed_builds {
        // Get project for this build to generate cache key
        let project = Project::get_project(db, &build.project_id)
            .await
            .map_err(|e| format!("Failed to get project: {}", e))?;

        // Route based on explicit build type - no fall-throughs
        match build.build_type.as_str() {
            "bundle" => {
                // BUNDLE BUILD: Must have an extracted path with manifest and cache
                let mut extracted_path = get_bundle_extracted_path(&build, build_path_cache).await;

                // If bundle not extracted, try to extract it on demand
                if extracted_path.is_none() {
                    tracing::info!(
                        "ROUTING: Bundle build {} (project {}) not extracted yet, extracting on demand...",
                        build.build_id,
                        project.name
                    );

                    // Get environment for storage retrieval
                    let env = match hot::db::Env::get_env(db, &project.env_id).await {
                        Ok(env) => env,
                        Err(e) => {
                            tracing::warn!(
                                "ROUTING: Failed to get env for bundle {} extraction: {}",
                                build.build_id,
                                e
                            );
                            continue;
                        }
                    };

                    // Try to retrieve and extract the build
                    match hot::storage::build_storage_from_config(worker_conf).await {
                        Ok(storage) => {
                            match storage
                                .retrieve_build(&build.build_id, &env.org_id, &project.env_id)
                                .await
                            {
                                Ok(build_data) => {
                                    let extract_dir = std::path::PathBuf::from(format!(
                                        ".hot/run/build-{}",
                                        build.build_id.simple()
                                    ));

                                    // Use extraction lock to prevent race conditions
                                    let extraction_lock =
                                        build_path_cache.get_extraction_lock(&build.build_id);
                                    let _lock_guard = extraction_lock.lock().await;

                                    // Double-check after acquiring lock
                                    if let Some(path) =
                                        get_bundle_extracted_path(&build, build_path_cache).await
                                    {
                                        tracing::debug!(
                                            "ROUTING: Bundle {} was extracted by another task while waiting for lock",
                                            build.build_id
                                        );
                                        extracted_path = Some(path);
                                    } else {
                                        // Extract the bundle
                                        match hot::bundle::extract_bundle_from_bytes(
                                            &build_data,
                                            &extract_dir,
                                        ) {
                                            Ok(()) => {
                                                tracing::info!(
                                                    "ROUTING: Extracted bundle {} to {:?}",
                                                    build.build_id,
                                                    extract_dir
                                                );

                                                // Read manifest for cache key
                                                if let Ok(manifest) =
                                                    hot::bundle::read_bundle_manifest(&extract_dir)
                                                {
                                                    // Clear any stale embedded cache
                                                    let bundle_cache_dir =
                                                        extract_dir.join(".hot").join("cache");
                                                    if bundle_cache_dir.exists()
                                                        && let Ok(entries) =
                                                            std::fs::read_dir(&bundle_cache_dir)
                                                    {
                                                        for entry in entries.flatten() {
                                                            let path = entry.path();
                                                            if path
                                                                .to_string_lossy()
                                                                .ends_with(".bc.zst")
                                                            {
                                                                let _ = std::fs::remove_file(&path);
                                                            }
                                                        }
                                                    }

                                                    // Pre-compile bytecode
                                                    let build_src_path =
                                                        extract_dir.join("hot/src");
                                                    let build_pkg_path =
                                                        extract_dir.join("hot/pkg");
                                                    let mut paths = vec![
                                                        build_src_path
                                                            .to_string_lossy()
                                                            .to_string(),
                                                    ];
                                                    if build_pkg_path.exists() {
                                                        paths.push(
                                                            build_pkg_path
                                                                .to_string_lossy()
                                                                .to_string(),
                                                        );
                                                    }

                                                    let _ =
                                                        std::fs::create_dir_all(&bundle_cache_dir);
                                                    let bundle_cache =
                                                        hot::lang::cache::bytecode_cache::BytecodeCache::new(
                                                            bundle_cache_dir,
                                                        );

                                                    if let Err(e) =
                                                        hot::lang::engine::Engine::compile_to_cache(
                                                            &paths,
                                                            &bundle_cache,
                                                            &manifest.bundle_name,
                                                            manifest.cache_key.as_deref(),
                                                            Some(manifest.file_hashes.clone()),
                                                            None, // Bundle builds have deps pre-bundled
                                                        )
                                                    {
                                                        tracing::warn!(
                                                            "ROUTING: Failed to pre-compile bundle {}: {}",
                                                            build.build_id,
                                                            e
                                                        );
                                                    }

                                                    // Mark extraction complete
                                                    BuildPathCache::mark_extraction_complete(
                                                        &extract_dir,
                                                    );
                                                    build_path_cache.insert(
                                                        build.build_id,
                                                        extract_dir.clone(),
                                                    );
                                                    extracted_path = Some(extract_dir);
                                                }
                                            }
                                            Err(e) => {
                                                tracing::warn!(
                                                    "ROUTING: Failed to extract bundle {}: {}",
                                                    build.build_id,
                                                    e
                                                );
                                            }
                                        }
                                    }
                                }
                                Err(e) => {
                                    tracing::warn!(
                                        "ROUTING: Failed to retrieve bundle {} from storage: {}",
                                        build.build_id,
                                        e
                                    );
                                }
                            }
                        }
                        Err(e) => {
                            tracing::warn!(
                                "ROUTING: Failed to create storage for bundle {} extraction: {}",
                                build.build_id,
                                e
                            );
                        }
                    }
                }

                let Some(extracted_path) = extracted_path else {
                    tracing::debug!(
                        "ROUTING: Bundle build {} (project {}) could not be extracted, skipping",
                        build.build_id,
                        project.name
                    );
                    continue;
                };

                // Use manifest's pre-computed cache key
                // This is critical - the bundle's bytecode was compiled with specific file hashes
                // that are stored in the manifest. Recalculating would produce different keys.
                match hot::bundle::read_bundle_manifest(&extracted_path) {
                    Ok(manifest) => {
                        tracing::debug!(
                            "ROUTING: Read manifest for bundle {}, cache_key={:?}",
                            manifest.bundle_name,
                            manifest.cache_key.as_ref().map(|k| &k[..12.min(k.len())])
                        );
                        if let Some(ref cache_key) = manifest.cache_key {
                            let bundle_cache_dir = extracted_path.join(".hot").join("cache");
                            let bundle_cache = hot::lang::cache::bytecode_cache::BytecodeCache::new(
                                bundle_cache_dir.clone(),
                            );
                            let cache_exists = bundle_cache.exists(cache_key);
                            tracing::debug!(
                                "ROUTING: Bundle cache dir={:?}, cache_exists={}",
                                bundle_cache_dir,
                                cache_exists
                            );

                            let mut check_result =
                                bytecode_has_function(&bundle_cache, cache_key, function_name);

                            // If cache doesn't exist or needs recompilation, compile it now
                            if check_result == CacheCheckResult::NeedsRecompile
                                || check_result == CacheCheckResult::CacheNotFound
                            {
                                let reason = if check_result == CacheCheckResult::NeedsRecompile {
                                    "version mismatch"
                                } else {
                                    "cache not found"
                                };
                                tracing::info!(
                                    "ROUTING: Bundle build {} cache needs compilation ({}), compiling...",
                                    build.build_id,
                                    reason
                                );
                                if recompile_bundle_cache(&extracted_path, &manifest) {
                                    // Create a fresh cache instance after compilation to avoid stale memory cache
                                    let fresh_cache =
                                        hot::lang::cache::bytecode_cache::BytecodeCache::new(
                                            bundle_cache_dir.clone(),
                                        );
                                    check_result = bytecode_has_function(
                                        &fresh_cache,
                                        cache_key,
                                        function_name,
                                    );
                                } else {
                                    tracing::error!(
                                        "ROUTING: Bundle build {} compilation failed",
                                        build.build_id
                                    );
                                }
                            }

                            match check_result {
                                CacheCheckResult::Found => {
                                    tracing::debug!(
                                        "ROUTING: Found function '{}' in bundle build {} (project {})",
                                        function_name,
                                        build.build_id,
                                        project.name
                                    );
                                    matching_builds.push((build, project.name));
                                }
                                CacheCheckResult::NotFound => {
                                    tracing::debug!(
                                        "ROUTING: Function '{}' NOT found in bundle {} (project {})",
                                        function_name,
                                        build.build_id,
                                        project.name
                                    );
                                }
                                CacheCheckResult::NeedsRecompile
                                | CacheCheckResult::CacheNotFound => {
                                    // Compilation was attempted but failed or still has issues
                                    tracing::error!(
                                        "ROUTING: Bundle build {} still has cache issues after compilation attempt",
                                        build.build_id
                                    );
                                }
                            }
                        } else {
                            tracing::warn!(
                                "ROUTING: Bundle build {} manifest has no cache_key",
                                build.build_id
                            );
                        }
                    }
                    Err(e) => {
                        tracing::warn!(
                            "ROUTING: Failed to read manifest for bundle build {} at {:?}: {}",
                            build.build_id,
                            extracted_path,
                            e
                        );
                    }
                }
            }
            "live" => {
                // LIVE BUILD: Calculate cache key from discovered source files
                let src_paths = hot::project::get_project_src_paths(worker_conf, &project.name);

                if src_paths.is_empty() {
                    tracing::warn!(
                        "ROUTING: Live build {} (project {}) has no source paths configured, skipping",
                        build.build_id,
                        project.name
                    );
                    continue;
                }

                // Collect source files for cache key
                let mut all_source_files = Vec::new();
                if let Ok(resolved_deps) =
                    hot::project::get_resolved_project_dependencies(worker_conf, &project.name)
                {
                    for dep in &resolved_deps {
                        let dep_path = dep.resolved_path.to_string_lossy().to_string();
                        if let Ok(files) = hot::lang::engine::Engine::discover_hot_files(&dep_path)
                        {
                            all_source_files.extend(files);
                        }
                    }
                }
                for src_path in &src_paths {
                    if let Ok(files) = hot::lang::engine::Engine::discover_hot_files(src_path) {
                        all_source_files.extend(files);
                    }
                }

                // Calculate cache key
                let file_hashes =
                    hot::lang::cache::bytecode_cache::BytecodeCache::hash_files(&all_source_files)
                        .unwrap_or_default();
                let Ok(cache_key) =
                    hot::lang::cache::bytecode_cache::BytecodeCache::calculate_cache_key(
                        &project.name,
                        &file_hashes,
                    )
                else {
                    tracing::error!(
                        "ROUTING: Live build {} failed to calculate cache key",
                        build.build_id
                    );
                    continue;
                };

                let mut check_result = bytecode_has_function(cache, &cache_key, function_name);

                // If cache doesn't exist or needs recompilation, compile it now
                if check_result == CacheCheckResult::NeedsRecompile
                    || check_result == CacheCheckResult::CacheNotFound
                {
                    let reason = if check_result == CacheCheckResult::NeedsRecompile {
                        "version mismatch"
                    } else {
                        "cache not found"
                    };
                    tracing::info!(
                        "ROUTING: Live build {} cache needs compilation ({}), compiling...",
                        build.build_id,
                        reason
                    );
                    if recompile_live_build_cache(
                        cache,
                        &src_paths,
                        &project.name,
                        &cache_key,
                        &file_hashes,
                        Some(worker_conf),
                    ) {
                        // Create a fresh cache instance after compilation
                        let fresh_cache = hot::lang::cache::bytecode_cache::BytecodeCache::new(
                            cache.cache_dir().to_path_buf(),
                        );
                        check_result =
                            bytecode_has_function(&fresh_cache, &cache_key, function_name);
                    } else {
                        tracing::error!(
                            "ROUTING: Live build {} compilation failed",
                            build.build_id
                        );
                    }
                }

                if check_result == CacheCheckResult::Found {
                    debug!(
                        "Found function '{}' in live build {} (project {})",
                        function_name, build.build_id, project.name
                    );
                    matching_builds.push((build, project.name));
                } else {
                    tracing::debug!(
                        "ROUTING: Function '{}' NOT found in live build {} (project {})",
                        function_name,
                        build.build_id,
                        project.name
                    );
                }
            }
            other => {
                tracing::warn!(
                    "ROUTING: Build {} has unknown build_type '{}', skipping",
                    build.build_id,
                    other
                );
            }
        }
    }

    if matching_builds.is_empty() {
        // If not found in cache, try to infer from namespace
        if let Some((namespace, _)) = function_name.rsplit_once('/') {
            debug!(
                "Function '{}' not found in cache, trying namespace-based lookup for '{}'",
                function_name, namespace
            );
            // Future improvement: store namespace -> project mapping in DB.
        }
        return Ok(None);
    }

    // Single match - no tie-breaker needed
    if matching_builds.len() == 1 {
        let (build, project_name) = matching_builds.into_iter().next().unwrap();
        return Ok(Some(RoutingResult {
            build,
            project_name,
            tie_breaker_used: false,
            matched_project_names: vec![],
        }));
    }

    // Multiple matches - use target_project_id as tie-breaker if available
    let matched_project_names: Vec<String> = matching_builds
        .iter()
        .map(|(_, name)| name.clone())
        .collect();

    if let Some(target_id) = target_project_id {
        // Try to find the build from the target project
        if let Some((build, project_name)) = matching_builds
            .iter()
            .find(|(b, _)| b.project_id == target_id)
            .cloned()
        {
            debug!(
                "Using target_project_id {} as tie-breaker for function '{}' (matched {} projects)",
                target_id,
                function_name,
                matched_project_names.len()
            );
            return Ok(Some(RoutingResult {
                build,
                project_name,
                tie_breaker_used: true,
                matched_project_names,
            }));
        }
    }

    // No tie-breaker available or target project not in matches - use first match
    let (build, project_name) = matching_builds.into_iter().next().unwrap();
    warn!(
        "Multiple builds have function '{}', using first match '{}' (no tie-breaker available). Matched projects: {:?}",
        function_name, project_name, matched_project_names
    );
    Ok(Some(RoutingResult {
        build,
        project_name,
        tie_breaker_used: true, // Still flag as tie-breaker situation even though we used default
        matched_project_names,
    }))
}

/// Execute a single event handler with isolated build context
#[allow(clippy::too_many_arguments)]
async fn execute_single_event_handler(
    db: &DatabasePool,
    build: &Build,
    _env_id: &Uuid,
    worker_conf: &Val,
    event_handler: &EventHandler,
    event_message: &EventMessage,
    emitter: Option<std::sync::Arc<dyn EngineEventEmitter>>,
    event_publisher: Option<std::sync::Arc<dyn EventPublisher>>,
    encryption: Option<Arc<ContextEncryption>>,
    cache: Arc<hot::lang::cache::bytecode_cache::BytecodeCache>,
    shutdown_coordinator: Arc<ShutdownCoordinator>,
    run_id: Uuid, // Accept run_id as parameter (already registered by caller)
    build_path_cache: Arc<BuildPathCache>, // NEW: Cache for extracted build paths
    dev_context_storage: Option<DevContextStorage>, // Context from hot/ctx.hot for dev mode
    stream_publisher: Option<Arc<StreamPubSub>>, // Stream pub/sub for real-time SSE updates
    task_queue: Arc<ProcessingQueue<TaskRequest>>,
) -> Result<(), String> {
    // TIMING: Track execution phases
    let timing_start = std::time::Instant::now();

    // Get project information for execution
    debug!("Executing event handler for build: {}", build.build_id);

    // Get project from build
    let project = Project::get_project(db, &build.project_id)
        .await
        .map_err(|e| format!("Failed to get project for build: {}", e))?;

    let timing_after_project = timing_start.elapsed();
    debug!(
        "TIMING [{}]: get_project: {:?}",
        run_id.as_simple(),
        timing_after_project
    );

    // NOTE: run_id is now passed in and already registered by the caller
    // This allows the caller to handle timeout scenarios and unregister the run

    debug!(
        "Executing event handler: {} ({})",
        event_handler.event_handler_id, run_id
    );

    // Create execution context with same env_id, user_id, org_id but new run_id
    // Use appropriate run type based on event type
    let run_type_id = if event_message.body.event.event_type == "hot:call" {
        hot::db::run::RunType::Call.as_id()
    } else if event_message.body.event.event_type == "hot:schedule" {
        hot::db::run::RunType::Schedule.as_id()
    } else {
        hot::db::run::RunType::Event.as_id()
    };

    // Check for manual retry context in event data (for UI-triggered retries)
    let retry_context = hot::env::retry::RetryContext::from_event_data(
        &event_message.body.event.event_data,
        worker_conf,
    );

    // For origin_run_id: determine based on event type and context
    let origin_run_id = if let Some(ref ctx) = retry_context {
        // Manual retry: use the origin_run_id from retry context
        ctx.origin_run_id
    } else if event_message.body.event.event_type == "hot:schedule" {
        // Scheduler retries carry origin_run_id in execution context;
        // normal scheduled events have it as None
        event_message.body.execution_context.origin_run_id
    } else if event_message.body.event.event_type == "hot:call" {
        // For hot:call events (UI-triggered retry/rerun), use origin_run_id from execution context
        // The app sets this correctly: Some(original_run_id) for retries, None for re-runs
        // We do NOT use execution_context.run_id here because that's the NEW run's ID
        event_message.body.execution_context.origin_run_id
    } else {
        // Check if this event was triggered by Hot code (send-event) or published via API
        // API-published events have origin_run_id = None in the execution context
        // Hot code send-event sets origin_run_id to the current run's ID
        //
        // We use the origin_run_id from the execution context if it exists,
        // otherwise None (for API-published events where run_id is just a new UUID placeholder)
        event_message.body.execution_context.origin_run_id
    };

    // For stream_id: use the stream_id from the event
    // The event was already created with a stream_id by the scheduler/publisher
    let stream_id = event_message.body.event.stream_id;

    // For user_id: if not provided in event context (e.g. for scheduled events),
    // use the build's creator as the user for this execution
    let user_id = event_message
        .body
        .execution_context
        .user_id
        .or(Some(build.created_by_user_id));

    // For org_id: if not provided in event context (e.g. scheduler sets it to None),
    // resolve it from the project's environment
    // Also resolve env_name and org_slug for RunInfo
    let (org_id, env_name, org_slug) =
        if let Some(org_id) = event_message.body.execution_context.org_id {
            // org_id provided, but we may still need env_name and org_slug
            let env_name = if event_message.body.execution_context.env_name.is_some() {
                event_message.body.execution_context.env_name.clone()
            } else if let Some(env_id) = event_message.body.execution_context.env_id {
                Env::get_env(db, &env_id).await.ok().map(|e| e.name)
            } else {
                None
            };
            let org_slug = if event_message.body.execution_context.org_slug.is_some() {
                event_message.body.execution_context.org_slug.clone()
            } else {
                hot::db::org::Org::get_org(db, &org_id)
                    .await
                    .ok()
                    .map(|o| o.slug)
            };
            (Some(org_id), env_name, org_slug)
        } else {
            // Fall back to getting org_id from project -> env
            debug!(
                "org_id not in execution context, resolving from env_id={}",
                project.env_id
            );
            match Env::get_env(db, &project.env_id).await {
                Ok(env) => {
                    debug!("Resolved org_id={} from environment", env.org_id);
                    let org_slug = hot::db::org::Org::get_org(db, &env.org_id)
                        .await
                        .ok()
                        .map(|o| o.slug);
                    (Some(env.org_id), Some(env.name), org_slug)
                }
                Err(e) => {
                    error!(
                        "Failed to resolve org_id from env {}: {}",
                        project.env_id, e
                    );
                    (None, None, None)
                }
            }
        };

    // Extract retry attempt: use retry context from event data if present (for retried runs)
    let retry_attempt = if let Some(ref ctx) = retry_context {
        ctx.attempt
    } else {
        // For scheduler-initiated retries, the retry_attempt is carried
        // in the execution context (not in event data "retry" key).
        // Normal (non-retry) scheduled events have retry_attempt = 0.
        event_message.body.execution_context.retry_attempt
    };

    let mut _execution_context = ExecutionContext::new_with_event_and_origin(
        run_id,
        stream_id,
        run_type_id,
        event_message.body.execution_context.env_id,
        user_id,
        org_id,
        Some(build.build_id), // Use resolved deployed build_id (not event context which may be None for API-published events)
        Some(event_message.body.event.event_id), // Include the event_id (now exists in DB)
        origin_run_id,        // From retry context or event type
    )
    .with_build_hash(Some(build.hash.clone()))
    .with_project(Some(build.project_id), Some(project.name.clone()))
    .with_retry_attempt(retry_attempt)
    .with_env_name(env_name)
    .with_org_slug(org_slug)
    .with_agent_type(extract_agent_type_from_handler(event_handler));

    let emitter_for_events = emitter.clone();

    // Create event object with type and data structure
    let event_object = val!({
        "type": event_message.body.event.event_type.clone(),
        "data": event_message.body.event.event_data.clone()
    });

    // Build the function name in the format expected: namespace/variable
    let function_name = format!("{}/{}", event_handler.ns, event_handler.var);

    debug!(
        "Executing event handler function: {} with event: {}",
        function_name, event_object
    );

    // Note: run:start event will be emitted by the VM when execution begins
    // We don't emit it here to avoid duplicate emissions
    //
    // CRITICAL: If ANY error occurs between here and VM.execute(), run:start will NEVER be emitted!
    // This means the run won't exist in the database even though the event was attempted.

    // Get source paths for the project
    // CRITICAL: If this build was extracted from storage, use the extracted path
    // Otherwise fall back to the config paths
    // Also track if this is a bundle build so we skip global dependency resolution
    // and use the bundle's own cache directory
    let (src_paths, is_bundle_build, bundle_extract_path, bundle_manifest) = if let Some(
        extracted_path,
    ) =
        build_path_cache.get(&build.build_id)
    {
        // Use extracted build path - source files are in hot/src, dependencies in hot/pkg
        let build_src_path = extracted_path.join("hot/src");
        let build_pkg_path = extracted_path.join("hot/pkg");
        debug!(
            "Using extracted build source path: {}",
            build_src_path.display()
        );
        let mut paths = vec![build_src_path.to_string_lossy().to_string()];
        // Include pkg path if it exists (bundled dependencies)
        if build_pkg_path.exists() {
            debug!(
                "Including bundled dependencies from: {}",
                build_pkg_path.display()
            );
            paths.push(build_pkg_path.to_string_lossy().to_string());
        }
        // Read the bundle manifest for metadata (cache key, file hashes)
        let manifest = hot::bundle::read_bundle_manifest(&extracted_path).ok();
        if let Some(ref m) = manifest {
            debug!(
                "Bundle manifest (cached): name={}, cache_key={:?}",
                m.bundle_name, m.cache_key
            );
        }
        (paths, true, Some(extracted_path.clone()), manifest) // This is a bundle build
    } else {
        // Build not in cache - need to extract it on-demand from storage
        // Use extraction lock to prevent race conditions between threads (in-process)
        let extraction_lock = build_path_cache.get_extraction_lock(&build.build_id);
        let _lock_guard = extraction_lock.lock().await;

        // Double-check in-memory cache after acquiring lock (another thread may have just finished)
        if let Some(extracted_path) = build_path_cache.get(&build.build_id) {
            debug!(
                "Build {} was extracted by another thread while waiting for lock",
                build.build_id
            );
            let build_src_path = extracted_path.join("hot/src");
            let build_pkg_path = extracted_path.join("hot/pkg");
            let mut paths = vec![build_src_path.to_string_lossy().to_string()];
            if build_pkg_path.exists() {
                paths.push(build_pkg_path.to_string_lossy().to_string());
            }
            let manifest = hot::bundle::read_bundle_manifest(&extracted_path).ok();
            (paths, true, Some(extracted_path.clone()), manifest)
        } else {
            // Acquire cross-process file lock to prevent races with other hot processes
            let extract_dir =
                std::path::PathBuf::from(format!(".hot/run/build-{}", build.build_id.as_simple()));

            // Create file lock - must live for duration of extraction
            // The lock file itself provides cross-process synchronization
            let mut file_lock = BuildPathCache::acquire_file_lock(&build.build_id).ok();
            let _file_lock_guard = file_lock.as_mut().and_then(|lock| lock.try_write().ok());

            // Check if another process already extracted (via marker file)
            if BuildPathCache::is_extraction_complete(&extract_dir) {
                debug!("Build {} was extracted by another process", build.build_id);
                // Add to in-memory cache
                build_path_cache.insert(build.build_id, extract_dir.clone());
                let build_src_path = extract_dir.join("hot/src");
                let build_pkg_path = extract_dir.join("hot/pkg");
                let mut paths = vec![build_src_path.to_string_lossy().to_string()];
                if build_pkg_path.exists() {
                    paths.push(build_pkg_path.to_string_lossy().to_string());
                }
                let manifest = hot::bundle::read_bundle_manifest(&extract_dir).ok();
                (paths, true, Some(extract_dir.clone()), manifest)
            } else {
                debug!(
                    "Build {} not in cache, attempting to extract from storage",
                    build.build_id
                );

                // Get environment for this build to retrieve from storage
                match hot::db::Env::get_env(db, &project.env_id).await {
                    Ok(env) => {
                        // Try to retrieve and extract the build
                        match hot::storage::build_storage_from_config(worker_conf).await {
                            Ok(storage) => {
                                match storage
                                    .retrieve_build(&build.build_id, &env.org_id, &project.env_id)
                                    .await
                                {
                                    Ok(build_data) => {
                                        debug!(
                                            "Retrieved build {} from storage ({} bytes), extracting...",
                                            build.build_id,
                                            build_data.len()
                                        );

                                        match hot::bundle::extract_bundle_from_bytes(
                                            &build_data,
                                            &extract_dir,
                                        ) {
                                            Ok(()) => {
                                                debug!(
                                                    "Successfully extracted build {} to {}",
                                                    build.build_id,
                                                    extract_dir.display()
                                                );

                                                // Read the bundle manifest for metadata
                                                let manifest =
                                                    hot::bundle::read_bundle_manifest(&extract_dir);
                                                if let Ok(ref m) = manifest {
                                                    debug!(
                                                        "Bundle manifest: name={}, cache_key={:?}, files={}",
                                                        m.bundle_name,
                                                        m.cache_key,
                                                        m.file_hashes.len()
                                                    );
                                                }

                                                // Use the extracted paths for both src and pkg (dependencies)
                                                let build_src_path = extract_dir.join("hot/src");
                                                let build_pkg_path = extract_dir.join("hot/pkg");
                                                let mut paths = vec![
                                                    build_src_path.to_string_lossy().to_string(),
                                                ];
                                                // Include pkg path if it exists (bundled dependencies)
                                                if build_pkg_path.exists() {
                                                    paths.push(
                                                        build_pkg_path
                                                            .to_string_lossy()
                                                            .to_string(),
                                                    );
                                                }

                                                // Pre-compile the bundle to generate bytecode cache
                                                // This ensures routing can find functions immediately after extraction
                                                let bundle_cache_dir =
                                                    extract_dir.join(".hot").join("cache");

                                                // Clear any stale embedded cache files from the bundle
                                                // Bundles may contain pre-compiled cache from an older Hot version
                                                if bundle_cache_dir.exists()
                                                    && let Ok(entries) =
                                                        std::fs::read_dir(&bundle_cache_dir)
                                                {
                                                    for entry in entries.flatten() {
                                                        let path = entry.path();
                                                        if path
                                                            .to_string_lossy()
                                                            .ends_with(".bc.zst")
                                                        {
                                                            if let Err(e) =
                                                                std::fs::remove_file(&path)
                                                            {
                                                                tracing::warn!(
                                                                    "Failed to remove stale cache file {:?}: {}",
                                                                    path,
                                                                    e
                                                                );
                                                            } else {
                                                                tracing::debug!(
                                                                    "Removed stale embedded cache file {:?}",
                                                                    path
                                                                );
                                                            }
                                                        }
                                                    }
                                                }

                                                let bundle_cache =
                                                    hot::lang::cache::bytecode_cache::BytecodeCache::new(
                                                        bundle_cache_dir,
                                                    );

                                                // Get project name and cache key from manifest for correct cache key
                                                let (project_name, cache_key, file_hashes) =
                                                    match &manifest {
                                                        Ok(m) => (
                                                            m.bundle_name.clone(),
                                                            m.cache_key.clone(),
                                                            Some(m.file_hashes.clone()),
                                                        ),
                                                        Err(_) => {
                                                            (project.name.clone(), None, None)
                                                        }
                                                    };

                                                info!(
                                                    "Pre-compiling bundle {} to generate bytecode cache",
                                                    build.build_id
                                                );
                                                if let Err(e) =
                                                    hot::lang::engine::Engine::compile_to_cache(
                                                        &paths,
                                                        &bundle_cache,
                                                        &project_name,
                                                        cache_key.as_deref(),
                                                        file_hashes,
                                                        None, // Bundle builds have deps pre-bundled
                                                    )
                                                {
                                                    warn!(
                                                        "Failed to pre-compile bundle {}: {}",
                                                        build.build_id, e
                                                    );
                                                } else {
                                                    info!(
                                                        "Bundle {} pre-compiled successfully",
                                                        build.build_id
                                                    );
                                                }

                                                // Mark extraction complete AFTER bytecode is generated
                                                // This ensures routing won't find the bundle until it's fully ready
                                                BuildPathCache::mark_extraction_complete(
                                                    &extract_dir,
                                                );

                                                // Store in cache for future use
                                                build_path_cache
                                                    .insert(build.build_id, extract_dir.clone());

                                                (
                                                    paths,
                                                    true,
                                                    Some(extract_dir.clone()),
                                                    manifest.ok(),
                                                )
                                                // This is a bundle build
                                            }
                                            Err(e) => {
                                                error!(
                                                    "Failed to extract build {} from storage: {}. Falling back to config paths",
                                                    build.build_id, e
                                                );
                                                // Fall back to config paths (not a bundle)
                                                (
                                                    hot::project::get_project_src_paths(
                                                        worker_conf,
                                                        &project.name,
                                                    ),
                                                    false,
                                                    None,
                                                    None,
                                                )
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        // In local dev mode, build storage is not expected to exist
                                        // Log at debug level to avoid noise
                                        if hot::env::is_local_dev() {
                                            debug!(
                                                "Build {} not in storage (expected in local dev), using config paths: {}",
                                                build.build_id, e
                                            );
                                        } else {
                                            error!(
                                                "Failed to retrieve build {} from storage: {}. Falling back to config paths",
                                                build.build_id, e
                                            );
                                        }
                                        // Fall back to config paths (not a bundle)
                                        (
                                            hot::project::get_project_src_paths(
                                                worker_conf,
                                                &project.name,
                                            ),
                                            false,
                                            None,
                                            None,
                                        )
                                    }
                                }
                            }
                            Err(e) => {
                                error!(
                                    "Failed to create storage client: {}. Falling back to config paths",
                                    e
                                );
                                // Fall back to config paths (not a bundle)
                                (
                                    hot::project::get_project_src_paths(worker_conf, &project.name),
                                    false,
                                    None,
                                    None,
                                )
                            }
                        }
                    }
                    Err(e) => {
                        error!(
                            "Failed to get environment for build {}: {}. Falling back to config paths",
                            build.build_id, e
                        );
                        // Fall back to config paths (not a bundle)
                        (
                            hot::project::get_project_src_paths(worker_conf, &project.name),
                            false,
                            None,
                            None,
                        )
                    }
                }
            } // Close the else block for marker file check
        } // Close the else block for in-memory cache check
    };

    let timing_after_build_path = timing_start.elapsed();
    debug!(
        "TIMING [{}]: build_path_resolution: {:?} (delta: {:?})",
        run_id.as_simple(),
        timing_after_build_path,
        timing_after_build_path - timing_after_project
    );

    // ===== BYTECODE CACHE INTEGRATION =====
    // Calculate cache key for this project's bytecode
    // On first run: compile and save cache
    // On subsequent runs: load from cache (from memory if available, otherwise disk)
    // Note: cache is now a shared Arc passed from worker initialization

    // For bundle builds, use the manifest's pre-computed cache key and file hashes
    // This is more efficient and ensures we use the exact same hashes from build time
    let (cache_key, file_hashes) = if let Some(ref manifest) = bundle_manifest {
        // Use manifest's pre-computed values
        let key = manifest.cache_key.clone().unwrap_or_default();
        let hashes = manifest.file_hashes.clone();
        debug!(
            "Using manifest cache key for bundle '{}': {} ({} files)",
            manifest.bundle_name,
            if key.len() > 12 { &key[..12] } else { &key },
            hashes.len()
        );
        (key, hashes)
    } else {
        // Non-bundle: discover and hash files dynamically
        let mut all_source_files = Vec::new();

        // Load project dependencies for cache key
        if let Ok(resolved_deps) =
            hot::project::get_resolved_project_dependencies(worker_conf, &project.name)
        {
            for dep in &resolved_deps {
                let dep_path = dep.resolved_path.to_string_lossy().to_string();
                if let Ok(files) = hot::lang::engine::Engine::discover_hot_files(&dep_path) {
                    all_source_files.extend(files);
                }
            }
        }

        // Add source files
        for src_path in &src_paths {
            if let Ok(files) = hot::lang::engine::Engine::discover_hot_files(src_path) {
                all_source_files.extend(files);
            }
        }

        // Calculate file hashes for cache validation
        let hashes =
            match hot::lang::cache::bytecode_cache::BytecodeCache::hash_files(&all_source_files) {
                Ok(h) => h,
                Err(e) => {
                    tracing::warn!("Failed to calculate file hashes for cache: {}", e);
                    Vec::new()
                }
            };

        // Calculate cache key
        let key = match hot::lang::cache::bytecode_cache::BytecodeCache::calculate_cache_key(
            &project.name,
            &hashes,
        ) {
            Ok(k) => k,
            Err(e) => {
                tracing::warn!("Failed to calculate cache key: {}", e);
                String::new()
            }
        };

        (key, hashes)
    };

    let timing_after_cache_key = timing_start.elapsed();
    debug!(
        "TIMING [{}]: cache_key_calc: {:?} (delta: {:?})",
        run_id.as_simple(),
        timing_after_cache_key,
        timing_after_cache_key - timing_after_build_path
    );

    // Try to load from cache with registries
    // IMPORTANT: For bundle builds, use the bundle's own cache directory
    // Each extracted bundle has its own .hot/cache/ with pre-compiled bytecode
    // We store the cache directory path so we can recreate the cache in spawn_blocking
    let effective_cache_dir: std::path::PathBuf =
        if let Some(ref extract_path) = bundle_extract_path {
            let bundle_cache_dir = extract_path.join(".hot").join("cache");
            debug!(
                "Using bundle-specific cache directory: {}",
                bundle_cache_dir.display()
            );
            bundle_cache_dir
        } else {
            cache.cache_dir().to_path_buf()
        };
    let effective_cache =
        hot::lang::cache::bytecode_cache::BytecodeCache::new(effective_cache_dir.clone());

    let cached_bytecode = if !cache_key.is_empty() && effective_cache.exists(&cache_key) {
        match effective_cache.load(&cache_key) {
            Ok(cached) => {
                // For bundle builds with manifest, we trust the manifest's hashes
                // (they were computed at build time and are authoritative)
                // For non-bundle builds, validate files haven't changed
                if bundle_manifest.is_some() {
                    debug!(
                        "✓ Bytecode cache hit for bundle '{}' (key: {}) - trusting manifest hashes",
                        project.name,
                        &cache_key[..12]
                    );
                    Some(cached)
                } else {
                    // Non-bundle: discover files and validate hashes
                    let mut all_source_files = Vec::new();
                    for src_path in &src_paths {
                        if let Ok(files) = hot::lang::engine::Engine::discover_hot_files(src_path) {
                            all_source_files.extend(files);
                        }
                    }

                    match hot::lang::cache::bytecode_cache::BytecodeCache::validate_file_hashes(
                        &all_source_files,
                        &cached.metadata.file_hashes,
                    ) {
                        Ok(true) => {
                            debug!(
                                "✓ Bytecode cache hit for project '{}' (key: {})",
                                project.name,
                                &cache_key[..12]
                            );
                            Some(cached)
                        }
                        Ok(false) => {
                            debug!(
                                "✗ Bytecode cache invalid for project '{}' - files changed, will recompile",
                                project.name
                            );
                            None
                        }
                        Err(e) => {
                            tracing::warn!("Cache validation failed: {}, will recompile", e);
                            None
                        }
                    }
                }
            }
            Err(e) => {
                tracing::warn!("Failed to load cache: {}, will compile from source", e);
                None
            }
        }
    } else {
        debug!(
            "✗ No cache found for project '{}', will compile and cache",
            project.name
        );
        None
    };
    // ===== END CACHE INTEGRATION =====

    let timing_after_bytecode_cache = timing_start.elapsed();
    debug!(
        "TIMING [{}]: bytecode_cache_check: {:?} (delta: {:?})",
        run_id.as_simple(),
        timing_after_bytecode_cache,
        timing_after_bytecode_cache - timing_after_cache_key
    );

    // Load context variables for this project
    // Merge from multiple sources (later sources override earlier for same keys):
    // 1. Dev context storage (from hot/ctx.hot) - provides defaults/fallback for local dev
    // 2. Environment-level ctx vars (shared across all projects in this env) - override hot/ctx.hot
    // 3. Project-level ctx vars (project-specific) - override env-level and hot/ctx.hot
    //
    // This order ensures database values always win over hot/ctx.hot placeholders,
    // while hot/ctx.hot provides fallback values for keys not set in the database.
    let context_storage: Option<AHashMap<String, hot::val::Val>> = {
        let mut storage = AHashMap::new();

        // 1. First, load dev context storage (from hot/ctx.hot) as the base/fallback
        if let Some(ref dev_ctx_storage) = dev_context_storage {
            match dev_ctx_storage.read() {
                Ok(dev_ctx_guard) => {
                    if let Some(ref dev_ctx) = *dev_ctx_guard {
                        debug!(
                            "Loading dev context storage with {} variables for project '{}' (as fallback/defaults)",
                            dev_ctx.len(),
                            project.name
                        );
                        for (key, val) in dev_ctx.iter() {
                            storage.insert(key.clone(), val.clone());
                        }
                    }
                }
                Err(_) => {
                    warn!(
                        "Failed to read dev context storage for project '{}'; continuing without ctx.hot values",
                        project.name
                    );
                }
            }
        }

        // Load from database if encryption is available (database values override hot/ctx.hot)
        if let Some(encryption) = &encryption {
            debug!(
                "Loading context variables for project '{}' from database (encryption available)",
                project.name
            );
            // Get org_id for encryption key derivation (already resolved earlier for ExecutionContext)
            let org_id = match org_id {
                Some(id) => id,
                None => {
                    // Should not happen — org_id was resolved above — but fall back just in case
                    debug!(
                        "Fetching env {} from database for org_id (fallback)",
                        project.env_id
                    );
                    let env = Env::get_env(db, &project.env_id)
                        .await
                        .map_err(|e| format!("Failed to get env for project: {}", e))?;
                    debug!("Successfully fetched env, org_id={}", env.org_id);
                    env.org_id
                }
            };

            // 2. Load environment-level context variables (override hot/ctx.hot)
            debug!(
                "Fetching environment-level context variables for env {} from database",
                project.env_id
            );
            match Context::get_by_env(db, &project.env_id).await {
                Ok(context_vars) => {
                    debug!(
                        "Successfully fetched {} env-level context variables from database",
                        context_vars.len()
                    );
                    for cv in context_vars {
                        if !cv.active {
                            continue;
                        }
                        match cv.get_decrypted_value(encryption, &org_id) {
                            Ok(val) => {
                                debug!(
                                    "Loaded env-level context variable '{}' for env '{}' (overrides hot/ctx.hot)",
                                    cv.key, project.env_id
                                );
                                storage.insert(cv.key.clone(), val);
                            }
                            Err(e) => {
                                tracing::warn!(
                                    "Failed to decrypt env-level context variable '{}': {}",
                                    cv.key,
                                    e
                                );
                            }
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        "Failed to load env-level context variables from database: {}",
                        e
                    );
                }
            }

            // 3. Load project-level context variables (override env-level and hot/ctx.hot)
            debug!(
                "Fetching project-level context variables for project {} from database",
                project.project_id
            );
            match Context::get_by_project(db, &project.project_id).await {
                Ok(context_vars) => {
                    debug!(
                        "Successfully fetched {} project-level context variables from database",
                        context_vars.len()
                    );
                    for cv in context_vars {
                        if !cv.active {
                            continue;
                        }
                        match cv.get_decrypted_value(encryption, &org_id) {
                            Ok(val) => {
                                debug!(
                                    "Loaded project-level context variable '{}' for project '{}' (overrides env and hot/ctx.hot)",
                                    cv.key, project.name
                                );
                                storage.insert(cv.key.clone(), val);
                            }
                            Err(e) => {
                                tracing::warn!(
                                    "Failed to decrypt project-level context variable '{}': {}",
                                    cv.key,
                                    e
                                );
                            }
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        "Failed to load project-level context variables from database: {}",
                        e
                    );
                }
            }
        }

        if storage.is_empty() {
            debug!(
                "No context available for project '{}' (no dev context, no database context)",
                project.name
            );
            None
        } else {
            debug!(
                "Context storage for project '{}': {} total variables (env + project + dev)",
                project.name,
                storage.len()
            );
            Some(storage)
        }
    };

    // Inject hot.request context from caller identity in event data.
    // The MCP handler includes a "caller" map in event.data for service key / session / api key
    // identity. We extract it as a ctx variable so the Hot function can access caller metadata.
    // Secret masking is handled via pre-computed hashes in execution_context.secret_value_hashes
    // (set by the API handler for only the sensitive fields, not the entire hot.request).
    let context_storage = {
        let mut storage = context_storage.unwrap_or_default();
        if let hot::val::Val::Map(ref data_map) = event_message.body.event.event_data
            && let Some(caller) = data_map.get(&hot::val::Val::from("caller"))
        {
            storage.insert("hot.request".to_string(), caller.clone());
            debug!(
                "Injected hot.request ctx from caller identity for project '{}'",
                project.name
            );
        }
        if storage.is_empty() {
            None
        } else {
            Some(storage)
        }
    };

    // Clone execution context (secret_value_hashes already populated by API handler)
    let execution_context_for_events = _execution_context.clone();

    let timing_after_context = timing_start.elapsed();
    debug!(
        "TIMING [{}]: context_loading: {:?} (delta: {:?})",
        run_id.as_simple(),
        timing_after_context,
        timing_after_context - timing_after_bytecode_cache
    );

    // Use engine to execute the event handler function with emitter and event publisher
    // NEW: Call function directly with Val arguments (no source code generation/parsing!)
    // This avoids the massive parsing overhead when event_object contains large data (e.g., base64 images)
    debug!(
        "Calling function '{}' directly with Val argument (skipping source code generation)",
        function_name
    );

    // Execute using cached bytecode if available.
    // Engine execution is CPU-intensive and must run on a blocking thread
    // to avoid starving the Tokio runtime. The runtime is configured with
    // a 64 MB thread stack (see hot_cli main) because the Hot VM is deeply
    // recursive and overflows the default ~8 MB stack on complex workloads.
    let timing_before_spawn = timing_start.elapsed();
    debug!(
        "TIMING [{}]: pre_spawn_blocking: {:?} (total setup)",
        run_id.as_simple(),
        timing_before_spawn
    );

    let file_storage: Option<Arc<dyn hot::file_storage::FileStorage>> =
        match hot::file_storage::file_storage_from_config(worker_conf).await {
            Ok(s) => Some(Arc::from(s)),
            Err(e) => {
                tracing::debug!("File storage not available for event handler: {}", e);
                None
            }
        };
    let store: Option<Arc<dyn hot::store::Store>> = match hot::store::store_from_config_with_db(
        worker_conf,
        Some(Arc::new(db.clone())),
        org_id,
        Some(project.env_id),
    )
    .await
    {
        Ok(s) => Some(Arc::from(s)),
        Err(e) => {
            tracing::debug!("Store not available for event handler: {}", e);
            None
        }
    };
    let embedding_provider: Option<Arc<dyn hot::store::embedding::EmbeddingProvider>> =
        hot::store::embedding::embedding_provider_from_config(worker_conf).map(Arc::from);

    let external_cancel = if worker_conf.get_bool_or_default("worker.cancel-on-timeout", true) {
        let token = Arc::new(std::sync::atomic::AtomicBool::new(false));
        shutdown_coordinator.register_cancel_token(run_id, Arc::clone(&token));
        Some(token)
    } else {
        None
    };
    let cancel_timer = external_cancel.as_ref().map(|token| {
        let token = Arc::clone(token);
        let timeout = get_run_timeout(worker_conf);
        tokio::spawn(async move {
            tokio::time::sleep(timeout).await;
            token.store(true, std::sync::atomic::Ordering::Relaxed);
        })
    });

    let result = {
        // Clone/move all data needed for the blocking task
        let function_name = function_name.clone();
        let event_object = event_object.clone();
        let worker_conf = worker_conf.clone();
        let emitter_for_events = emitter_for_events.clone();
        let execution_context_for_events = execution_context_for_events.clone();
        let event_publisher = event_publisher.clone();
        let db = db.clone();
        let stream_publisher = stream_publisher.clone();
        let project_name = project.name.clone();
        let src_paths = src_paths.clone();
        let cache_key = cache_key.clone();
        let file_hashes = file_hashes.clone();
        let effective_cache_dir = effective_cache_dir.clone();
        let task_queue = task_queue.clone();
        let file_storage = file_storage.clone();
        let run_id_for_timing = run_id;
        let external_cancel = external_cancel.clone();

        let panic_label = format!("worker:{}", function_name);
        tokio::task::spawn_blocking(move || {
            let spawn_entered = std::time::Instant::now();
            debug!("TIMING [{}]: spawn_blocking entered", run_id_for_timing.as_simple());

            // Wrap all user-code execution in run_user_code so any panic from
            // user-supplied Hot code (or from a third-party crate it triggers)
            // is converted to a structured UserCodePanic and surfaced as a
            // request error, rather than crashing the worker process.
            // spawn_blocking still provides defense-in-depth catching outside
            // this boundary in case run_user_code itself misses something.
            hot::lang::user_code::run_user_code(&panic_label, || {
            if let Some(cached) = cached_bytecode {
                // ===== CACHE HIT: Call function directly with cached bytecode =====
                debug!("TIMING [{}]: cache HIT - calling function (spawn wait: {:?})", run_id_for_timing.as_simple(), spawn_entered.elapsed());
                debug!(
                    "✓ Calling function with cached bytecode for project '{}' (no parsing!)",
                    project_name
                );
                let result = hot::lang::engine::Engine::call_function_with_cached_bytecode(
                    &function_name,
                    std::slice::from_ref(&event_object),
                    cached,
                    Some(&worker_conf),
                    emitter_for_events.clone(),
                    Some(execution_context_for_events.clone()),
                    event_publisher.clone(),
                    context_storage,
                    Some(Arc::new(db.clone())),
                    stream_publisher.clone(),
                    Some(task_queue.clone()),
                    file_storage.clone(),
                    store.clone(),
                    embedding_provider.clone(),
                    external_cancel.clone(),
                );
                debug!("TIMING [{}]: function execution complete (total spawn_blocking: {:?})", run_id_for_timing.as_simple(), spawn_entered.elapsed());
                result
            } else {
                // ===== CACHE MISS: Compile project, cache, then call function directly =====
                // Use locking to prevent duplicate compilation across threads and processes
                let cache_for_locking = hot::lang::cache::bytecode_cache::BytecodeCache::new(effective_cache_dir.clone());

                // In-process lock (prevents thread races)
                let compilation_lock = cache_for_locking.get_compilation_lock(&cache_key);
                let _compilation_guard = compilation_lock.lock();

                // Re-check cache after acquiring in-process lock (another thread may have just finished)
                if !cache_key.is_empty() && cache_for_locking.exists(&cache_key)
                    && let Ok(cached) = cache_for_locking.load(&cache_key) {
                        debug!(
                            "✓ Bytecode cache populated by another thread for '{}' while waiting",
                            project_name
                        );
                        return hot::lang::engine::Engine::call_function_with_cached_bytecode(
                            &function_name,
                            std::slice::from_ref(&event_object),
                            cached,
                            Some(&worker_conf),
                            emitter_for_events.clone(),
                            Some(execution_context_for_events.clone()),
                            event_publisher.clone(),
                            context_storage,
                            Some(Arc::new(db.clone())),
                            stream_publisher.clone(),
                            Some(task_queue.clone()),
                            file_storage.clone(),
                            store.clone(),
                            embedding_provider.clone(),
                            external_cancel.clone(),
                        );
                    }

                // Cross-process file lock (prevents process races)
                let mut file_lock = cache_for_locking.acquire_file_lock(&cache_key).ok();
                let _file_lock_guard = file_lock.as_mut().and_then(|lock| lock.try_write().ok());

                // Re-check cache after acquiring file lock (another process may have just finished)
                if !cache_key.is_empty() && cache_for_locking.exists(&cache_key)
                    && let Ok(cached) = cache_for_locking.load(&cache_key) {
                        debug!(
                            "✓ Bytecode cache populated by another process for '{}' while waiting",
                            project_name
                        );
                        return hot::lang::engine::Engine::call_function_with_cached_bytecode(
                            &function_name,
                            std::slice::from_ref(&event_object),
                            cached,
                            Some(&worker_conf),
                            emitter_for_events.clone(),
                            Some(execution_context_for_events.clone()),
                            event_publisher.clone(),
                            context_storage,
                            Some(Arc::new(db.clone())),
                            stream_publisher.clone(),
                            Some(task_queue.clone()),
                            file_storage.clone(),
                            store.clone(),
                            embedding_provider.clone(),
                            external_cancel.clone(),
                        );
                    }

                debug!("Compiling project from source for '{}'", project_name);

                // Compile the project sources ONLY (no function call code to parse!)
                // Don't pass emitter/execution_context here - we're just compiling, not recording a run
                // The run will be recorded when we call the function via call_function_with_cached_bytecode
                //
                // IMPORTANT: For bundle builds, pass None for project_name to skip global dependency
                // resolution in the engine. Bundle builds have dependencies pre-bundled in hot/pkg/
                // which is already included in src_paths. Passing project_name would cause the engine
                // to also load dependencies from the global cache, causing double-loading or version
                // mismatches.
                let compile_project_name: Option<&str> = if is_bundle_build { None } else { Some(&project_name) };
                let compile_result = hot::lang::engine::Engine::compile_project_for_cache(
                    &src_paths,
                    Some(&worker_conf),
                    compile_project_name,
                    None, // No emitter during compilation
                    None, // No execution_context during compilation
                    event_publisher.clone(),
                    context_storage.clone(),
                    Some(Arc::new(db.clone())),
                    stream_publisher.clone(),
                );

                match compile_result {
                    Ok((_init_result, artifacts)) => {
                        // Save to cache before calling function
                        // Use bundle-specific cache for bundle builds, shared cache otherwise
                        if !cache_key.is_empty() && !file_hashes.is_empty() {
                            let metadata = hot::lang::cache::bytecode_cache::create_cache_metadata(
                                &project_name,
                                file_hashes.clone(),
                                cache_key.clone(),
                            );

                            let cache_for_save = hot::lang::cache::bytecode_cache::BytecodeCache::new(effective_cache_dir.clone());
                            // Bake tool/skill spec registries into the on-disk
                            // cache so that fresh worker processes (and zip
                            // builds) can serve `::hot::internal::mcp/schema-from-fn`
                            // without recompiling.
                            let spec_compiler = hot::lang::compiler::Compiler::new();
                            let tool_specs = spec_compiler.build_tool_specs(&artifacts.ast_program);
                            let skill_specs = spec_compiler.build_skill_specs(&artifacts.ast_program);
                            if let Err(e) = cache_for_save.save(
                                &cache_key,
                                &artifacts.program,
                                metadata,
                                &artifacts.function_mapping,
                                &artifacts.core_functions,
                                &artifacts.type_implementations,
                                &artifacts.ast_program,
                                &artifacts.hot_ast,
                                &tool_specs,
                                &skill_specs,
                            ) {
                                tracing::warn!("Failed to save bytecode cache: {}", e);
                            } else {
                                debug!(
                                    "✓ Saved bytecode cache for project '{}' (key: {}) with {} functions",
                                    project_name,
                                    &cache_key[..12.min(cache_key.len())],
                                    artifacts.function_mapping.len(),
                                );
                            }
                        }

                        // Convert artifacts to cached bytecode and call function directly
                        let cached = hot::lang::engine::Engine::artifacts_to_cached_bytecode(artifacts);
                        hot::lang::engine::Engine::call_function_with_cached_bytecode(
                            &function_name,
                            std::slice::from_ref(&event_object),
                            cached,
                            Some(&worker_conf),
                            emitter_for_events.clone(),
                            Some(execution_context_for_events.clone()),
                            event_publisher.clone(),
                            context_storage,
                            Some(Arc::new(db.clone())),
                            stream_publisher.clone(),
                            Some(task_queue.clone()),
                            file_storage.clone(),
                            store.clone(),
                            embedding_provider.clone(),
                            external_cancel.clone(),
                        )
                    }
                    Err(e) => Err(e),
                }
            }
            }) // end run_user_code
        }).await
        .map_err(|e| format!("Blocking task failed: {}", e))?
        .unwrap_or_else(|panic| {
            tracing::error!(
                target: "hot::panic",
                location = panic.location.as_deref().unwrap_or("<unknown>"),
                thread = %panic.thread,
                "user code panicked in worker event handler: {}",
                panic.message,
            );
            Err(format!("Event handler panicked: {}", panic.summary()))
        })
    };
    if let Some(timer) = cancel_timer {
        timer.abort();
    }

    // Handle result (rest of function continues as before)
    let result = match result {
        Ok(val) => val,
        Err(e) => {
            error!(
                "Event handler '{}' failed for event '{}': {}",
                function_name, event_message.body.event.event_type, e
            );
            return Err(e);
        }
    };

    // Check if the result is an error Result type (from fail() or other failures)
    // or a Cancellation type (from cancel())
    // Result.Err format: { $type: "::hot::type/Result.Err", $val: error }
    // Cancellation format: { $type: "::hot::run/Cancellation" | "::hot::task/Cancellation", $val: {...} }
    let is_error_result = result.is_err();
    let is_cancelled_result = result.is_cancelled();

    // Note: An error Result from Hot code (e.g., Result.Err("...")) is NOT
    // a worker-level failure. The event handler executed successfully and emitted
    // run:start + run:fail. We should return Ok() to indicate successful execution.
    // Similarly, a Cancellation is not an error - it's a deliberate early termination.
    if is_error_result {
        debug!(
            "Event handler {} completed with error Result (failure)",
            event_handler.event_handler_id
        );
    } else if is_cancelled_result {
        debug!(
            "Event handler {} completed with Cancellation",
            event_handler.event_handler_id
        );
    } else {
        debug!(
            "Event handler {} completed successfully",
            event_handler.event_handler_id
        );
    }

    // Flush emitter to ensure run is written to database BEFORE publishing stream event
    // This guarantees the SSE handler can query the run from the database
    if let Some(ref em) = emitter
        && let Err(e) = em.flush()
    {
        tracing::warn!("Failed to flush emitter before stream publish: {}", e);
    }

    // Publish stream event for real-time SSE updates (fire-and-forget)
    if let Some(ref publisher) = stream_publisher {
        let stream_id = event_message.body.execution_context.stream_id;
        let event_id = event_message.body.execution_context.event_id;
        // env_id is required for events being processed - use event's env_id which is always present
        let env_id = event_message.body.event.env_id;

        // Convert result to JSON for the stream event
        let result_json = serde_json::to_value(result.to_hot_data_repr()).ok();

        // Get run type as string for env event
        let run_type = if event_message.body.event.event_type == "hot:call" {
            "call"
        } else if event_message.body.event.event_type == "hot:schedule" {
            "schedule"
        } else {
            "event"
        }
        .to_string();

        // Get project_id from build (project was already looked up earlier)
        let project_id = Some(build.project_id);

        let stream_event = if is_error_result {
            StreamEvent::RunFail {
                run_id,
                env_id,
                stream_id,
                event_id,
                error: result_json.map(|v| v.to_string()),
            }
        } else if is_cancelled_result {
            // Extract cancellation reason from the Cancellation type if available
            let reason = result.unwrap_cancelled().and_then(|v| {
                if let Val::Map(m) = v {
                    m.get(&Val::from("msg")).and_then(|msg| match msg {
                        Val::Str(s) => Some((**s).to_owned()),
                        _ => None,
                    })
                } else {
                    None
                }
            });
            StreamEvent::RunCancel {
                run_id,
                env_id,
                stream_id,
                event_id,
                reason,
            }
        } else {
            StreamEvent::RunStop {
                run_id,
                env_id,
                stream_id,
                event_id,
                result: result_json,
            }
        };

        // Create env event for dashboard real-time updates
        let env_event = if is_error_result {
            EnvEvent::RunFail {
                run_id,
                env_id,
                stream_id,
                event_id,
                project_id,
                fn_name: Some(function_name.clone()),
                run_type: run_type.clone(),
                duration_ms: None, // Duration is tracked by the run record
                error: result.unwrap_err().map(|v| v.to_string()),
            }
        } else if is_cancelled_result {
            let reason = result.unwrap_cancelled().and_then(|v| {
                if let Val::Map(m) = v {
                    m.get(&Val::from("msg")).and_then(|msg| match msg {
                        Val::Str(s) => Some((**s).to_owned()),
                        _ => None,
                    })
                } else {
                    None
                }
            });
            EnvEvent::RunCancel {
                run_id,
                env_id,
                stream_id,
                event_id,
                project_id,
                fn_name: Some(function_name.clone()),
                run_type: run_type.clone(),
                duration_ms: None,
                reason,
            }
        } else {
            EnvEvent::RunStop {
                run_id,
                env_id,
                stream_id,
                event_id,
                project_id,
                fn_name: Some(function_name.clone()),
                run_type,
                duration_ms: None,
            }
        };

        // Spawn fire-and-forget task to publish both events (don't block execution)
        let publisher_clone = Arc::clone(publisher);
        tokio::spawn(async move {
            if let Err(e) = publisher_clone.publish(stream_event).await {
                tracing::warn!("Failed to publish stream event: {}", e);
            }
            if let Err(e) = publisher_clone.publish_env(env_event).await {
                tracing::warn!("Failed to publish env event: {}", e);
            }
        });
    }

    // Unregister run on successful completion (whether Ok or Err Result)
    shutdown_coordinator.unregister_run(&run_id);
    Ok(())
}

pub const DEFAULT_SHUTDOWN_TIMEOUT_SECONDS: u64 = 60;

pub fn get_resolved_conf(conf: Val) -> Val {
    // Start with defaults
    let default_conf = val!({
        "threads": DEFAULT_WORKER_THREADS as i64,
        "run-timeout": DEFAULT_RUN_TIMEOUT_SECONDS as i64,
        "shutdown-timeout": DEFAULT_SHUTDOWN_TIMEOUT_SECONDS as i64,
        "queue-concurrency": "auto",
        "vm-concurrency": "auto",
        "vm-memory-mb": 256i64,
        "reserved-memory-mb": 512i64,
        "db-reserved-connections": 4i64,
        "cancel-on-timeout": true,
        "event-ordering": "current",
        "handler-concurrency": "serial",
        "shared-process": false,
        "local-write-concurrency": 1i64
    });

    // Merge with provided conf (the provided conf will override defaults)
    default_conf.merge(&conf)
}

/// Get the run timeout from worker config (in seconds)
pub fn get_run_timeout(worker_conf: &Val) -> std::time::Duration {
    let timeout_secs =
        worker_conf.get_int_or_default("worker.run-timeout", DEFAULT_RUN_TIMEOUT_SECONDS as i64);
    let timeout = if timeout_secs > 0 {
        timeout_secs as u64
    } else {
        DEFAULT_RUN_TIMEOUT_SECONDS
    };
    std::time::Duration::from_secs(timeout)
}

pub async fn run(
    queue_type: QueueType,
    redis_uri: Option<String>,
    redis_cluster: bool,
    serialization: Serialization,
    threads: Option<usize>,
    worker_conf: Val,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    run_with_components(
        queue_type,
        redis_uri,
        redis_cluster,
        serialization,
        threads,
        worker_conf,
        None, // No pre-created emitter
        None, // No pre-created event publisher
        None, // No dev context storage
        None, // No pre-created stream publisher
    )
    .await
}

/// Run the worker with optional pre-created emitter and event publisher components
/// This allows the CLI to create and pass configured components
#[allow(clippy::too_many_arguments)]
pub async fn run_with_components(
    queue_type: QueueType,
    redis_uri: Option<String>,
    redis_cluster: bool,
    serialization: Serialization,
    threads: Option<usize>,
    worker_conf: Val,
    emitter: Option<std::sync::Arc<dyn EngineEventEmitter>>,
    event_publisher: Option<std::sync::Arc<dyn EventPublisher>>,
    dev_context_storage: Option<AHashMap<String, hot::val::Val>>,
    stream_publisher: Option<Arc<StreamPubSub>>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let dev_context_storage =
        dev_context_storage.map(|ctx| Arc::new(RwLock::new(Some(ctx))) as DevContextStorage);

    run_with_components_shared_context(
        queue_type,
        redis_uri,
        redis_cluster,
        serialization,
        threads,
        worker_conf,
        emitter,
        event_publisher,
        dev_context_storage,
        stream_publisher,
    )
    .await
}

/// Run the worker with reloadable dev context storage.
///
/// `hot dev` uses this so local `ctx.hot` values can be refreshed after `.env`
/// changes without restarting the worker.
#[allow(clippy::too_many_arguments)]
pub async fn run_with_components_shared_context(
    queue_type: QueueType,
    redis_uri: Option<String>,
    redis_cluster: bool,
    serialization: Serialization,
    threads: Option<usize>,
    worker_conf: Val,
    emitter: Option<std::sync::Arc<dyn EngineEventEmitter>>,
    event_publisher: Option<std::sync::Arc<dyn EventPublisher>>,
    dev_context_storage: Option<DevContextStorage>,
    stream_publisher: Option<Arc<StreamPubSub>>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    debug!("hot.dev: WORKER starting");

    // Create database connection for event handler processing FIRST
    // This is needed by the emitter and event publisher
    let db = match hot::db::create_db_pool(&worker_conf).await {
        Ok(pool) => {
            // Test the database connection
            debug!("hot.dev: WORKER verifying database connectivity");
            match hot::db::test_connection(&pool).await {
                Ok(_) => {
                    debug!("hot.dev: WORKER successfully connected to database");
                    Some(Arc::new(pool))
                }
                Err(e) => {
                    error!(
                        "hot.dev: WORKER failed to verify database connection: {}",
                        e
                    );
                    return Err(format!("Database connection test failed: {}", e).into());
                }
            }
        }
        Err(e) => {
            error!("hot.dev: WORKER failed to create database pool: {}", e);
            return Err(format!("Database pool creation failed: {}", e).into());
        }
    };

    // Use provided emitter and event publisher, or create them from configuration
    // NOTE: These need the database pool to be created first
    let emitter = if emitter.is_some() {
        emitter
    } else {
        // emitter requires database pool, so only create if db is available
        if let Some(ref db_pool) = db {
            match create_emitter(&worker_conf, db_pool.as_ref()) {
                Ok(emitter) => emitter,
                Err(e) => {
                    error!("Failed to create emitter: {}", e);
                    None
                }
            }
        } else {
            error!("Cannot create emitter without database connection");
            None
        }
    };

    let event_publisher = if event_publisher.is_some() {
        event_publisher
    } else {
        // event_publisher requires database pool, so only create if db is available
        if let Some(ref db_pool) = db {
            match create_event_publisher(&worker_conf, db_pool.as_ref()) {
                Ok(publisher) => publisher,
                Err(e) => {
                    error!("Failed to create event publisher: {}", e);
                    None
                }
            }
        } else {
            error!("Cannot create event publisher without database connection");
            None
        }
    };

    // Load context variable encryption key
    // For local development, auto-generate key if not configured
    let profile = worker_conf.get_str("runtime.profile");
    let encryption = match ContextEncryption::from_env_or_generate_for_dev(&profile) {
        Ok(enc) => {
            debug!("hot.dev: WORKER loaded context encryption key");
            Some(Arc::new(enc))
        }
        Err(e) => {
            tracing::warn!(
                "hot.dev: WORKER context encryption key not available: {}. Context variables will not be loaded.",
                e
            );
            None
        }
    };

    if let Some(ref ctx_storage) = dev_context_storage
        && let Ok(ctx_guard) = ctx_storage.read()
        && let Some(ref ctx) = *ctx_guard
    {
        debug!(
            "hot.dev: WORKER using dev context storage with {} variables",
            ctx.len()
        );
    }

    // Create or use provided stream publisher for real-time SSE updates
    let stream_publisher: Option<Arc<StreamPubSub>> = if stream_publisher.is_some() {
        stream_publisher
    } else {
        // Create stream publisher matching the queue type (Memory or Redis)
        let pubsub_type = match queue_type {
            QueueType::Memory => StreamPubSubType::Memory,
            QueueType::Redis => StreamPubSubType::Redis,
        };

        match StreamPubSub::new(pubsub_type, redis_uri.clone(), redis_cluster) {
            Ok(pubsub) => {
                debug!(
                    "hot.dev: WORKER created stream publisher (type: {:?})",
                    pubsub_type
                );
                Some(Arc::new(pubsub))
            }
            Err(e) => {
                tracing::warn!(
                    "hot.dev: WORKER failed to create stream publisher: {}. SSE will fall back to polling.",
                    e
                );
                None
            }
        }
    };

    // Create the request, response, and event queues based on queue_type
    let request_queue_name = "hot:request".to_string();
    let response_queue_name = "hot:response".to_string();
    let event_queue_name = "hot:event".to_string();

    // Per-queue startup retention windows. When a brand-new consumer group is
    // created, it starts at `<now - window>-0` instead of `0`; an existing
    // group whose last-delivered-id is older than `now - window` is fast-
    // forwarded past the stale backlog at startup. This prevents workers
    // coming back from outages (or new deploys against streams with retained
    // history) from draining a multi-day flood of late events.
    //
    // Picked per queue:
    //   - request/response: 5m; RPC-shaped, callers have almost certainly
    //     already timed out for anything older than this.
    //   - event: 4h; covers Redis maintenance windows, deploy + rollback
    //     cycles, and most off-hours recovery timelines. Beyond
    //     that you're in manual ops-recovery territory and the DB-backed
    //     scheduler/backfill paths take over.
    //   - alert/email: 1h. Notifications are not user *jobs* — they're
    //     side effects of jobs that already ran and persisted to the DB.
    //     Freshness is part of the payload here: a 4h-late "high error
    //     rate" alert misleads operators (the issue may be long resolved,
    //     and a fresh alert will have fired in the meantime); a 4h-late
    //     password-reset email confuses users who already hit "resend"
    //     and possibly received a newer code. Replaying a multi-hour
    //     burst of stale notifications after a queue outage is the
    //     worst-case UX we want to avoid. The 4h preservation rule we
    //     apply elsewhere is for runs/tasks (the user's own code
    //     executing); here, dropping > 1h late is the kinder default.
    //   - task: 24h; tasks legitimately run for hours, after a day the user
    //     has moved on.
    let request_window = std::time::Duration::from_secs(5 * 60); // 5m
    let response_window = std::time::Duration::from_secs(5 * 60); // 5m
    let event_window = std::time::Duration::from_secs(4 * 60 * 60); // 4h
    let alert_window = std::time::Duration::from_secs(60 * 60); // 1h
    let email_window = std::time::Duration::from_secs(60 * 60); // 1h
    // Stable consumer-name prefix for this worker process. Computed early so we
    // can pin admin-path consumer names (startup recovery, janitor, daily
    // queue_cleanup) to the same identity as worker_id=0. Without that pinning
    // any XAUTOCLAIM done from an admin path lands in a *fresh UUID consumer*
    // (RedisStreamQueue::clone regenerates the name), and because no real
    // worker dequeues under that UUID identity the reclaimed PEL entries sit
    // there forever — the next janitor tick re-claims them into yet another
    // UUID, etc. Pinning the admin path to w0 prevents that orphaned-consumer
    // loop and lets the live worker drain reclaimed entries.
    //
    // Uses $HOSTNAME (set by Kubernetes/Docker/most shells) and the OS PID to
    // disambiguate multiple worker processes on the same host.
    let host = std::env::var("HOSTNAME").unwrap_or_else(|_| "host".to_string());
    let pid = std::process::id();
    let consumer_prefix = format!("{}-{}", host, pid);
    // Admin consumer names == worker_id=0's identity, so anything reclaimed by
    // an admin path lands in w0's PEL and gets drained by w0's normal dequeue
    // loop (which reads its own PEL via `XREADGROUP ... 0` before pulling new
    // entries with `>`). Sharing a Redis consumer name across multiple
    // connections is safe: it just means they share PEL ownership.
    let admin_request_name = format!("{}-w0-request", consumer_prefix);
    let admin_response_name = format!("{}-w0-response", consumer_prefix);
    let admin_event_name = format!("{}-w0-event", consumer_prefix);
    let admin_alert_name = format!("{}-w0-alert", consumer_prefix);
    let admin_email_name = format!("{}-w0-email", consumer_prefix);

    // The main worker executes one message per loop per handle, so keep Redis
    // reads at one entry to avoid hiding backlog in PEL/local prefetch.
    let worker_read_batch_size = 1usize;

    // Create processing queues with dequeue_and_work support - all using unified Message type
    let request_queue = ProcessingQueue::<Message>::new_with_cluster(
        queue_type,
        request_queue_name,
        redis_uri.clone(),
        redis_cluster,
        serialization,
    )?
    .with_startup_window(request_window)
    .with_read_batch_size(worker_read_batch_size)
    .with_consumer_name(admin_request_name.clone());

    let response_queue = ProcessingQueue::<Message>::new_with_cluster(
        queue_type,
        response_queue_name,
        redis_uri.clone(),
        redis_cluster,
        serialization,
    )?
    .with_startup_window(response_window)
    .with_read_batch_size(worker_read_batch_size)
    .with_consumer_name(admin_response_name.clone());

    let event_queue = ProcessingQueue::<Message>::new_with_cluster(
        queue_type,
        event_queue_name,
        redis_uri.clone(),
        redis_cluster,
        serialization,
    )?
    .with_startup_window(event_window)
    .with_read_batch_size(worker_read_batch_size)
    .with_consumer_name(admin_event_name.clone());

    // Create alert and email notification queues
    let alert_queue_name = "hot:alert".to_string();
    let email_queue_name = "hot:email".to_string();

    let alert_queue = ProcessingQueue::<Message>::new_with_cluster(
        queue_type,
        alert_queue_name,
        redis_uri.clone(),
        redis_cluster,
        serialization,
    )?
    .with_startup_window(alert_window)
    .with_read_batch_size(worker_read_batch_size)
    .with_consumer_name(admin_alert_name.clone());

    let email_queue = ProcessingQueue::<Message>::new_with_cluster(
        queue_type,
        email_queue_name,
        redis_uri.clone(),
        redis_cluster,
        serialization,
    )?
    .with_startup_window(email_window)
    .with_read_batch_size(worker_read_batch_size)
    .with_consumer_name(admin_email_name.clone());

    // hot:task queue is owned by hot_task_worker; hot_worker only ENQUEUEs to
    // it (event handlers can spawn tasks). We deliberately do NOT participate
    // in PEL ownership for {hot:task}: no startup recovery, no janitor reclaim,
    // no daily cleanup. hot_task_worker has its own janitor that uses its
    // stable `{host}-{pid}-task` consumer name and is the rightful drain
    // target for orphaned task PEL entries. If hot_worker reclaimed task
    // orphans into its own consumer it would be a dead end (we don't run
    // process_blocking on this queue here).
    let task_queue = Arc::new(ProcessingQueue::<TaskRequest>::new_with_cluster(
        queue_type,
        "hot:task".to_string(),
        redis_uri.clone(),
        redis_cluster,
        serialization,
    )?);

    // Initialize global notification queue registry so publish_alert() can enqueue
    hot::notification_queue::init_alert_queue(std::sync::Arc::new(alert_queue.clone()));
    hot::notification_queue::init_email_queue(std::sync::Arc::new(email_queue.clone()));

    // Verify queue connectivity with a quick health check
    debug!("hot.dev: WORKER verifying queue connectivity");
    match queue_type {
        QueueType::Memory => {
            debug!("hot.dev: WORKER using in-memory queue (no connectivity check needed)");
        }
        QueueType::Redis => {
            // Test Redis connection with a simple PING command
            match event_queue.is_empty().await {
                Ok(_) => {
                    debug!("hot.dev: WORKER successfully connected to Redis queue");
                }
                Err(e) => {
                    error!("hot.dev: WORKER failed to connect to Redis queue: {}", e);
                    return Err(format!("Redis queue connectivity check failed: {}", e).into());
                }
            }
        }
    }

    // CRITICAL: Recover orphaned items from previous crashes/shutdowns
    // When a worker is terminated mid-processing, events get stuck in processing keys
    // and would be lost forever without this recovery mechanism
    debug!("hot.dev: WORKER checking for orphaned items from previous runs");

    // In local dev, purge messages older than 1 hour before recovery.
    // Old messages (from previous sessions hours/days ago) cause a "catch-up flood"
    // that bogs down the worker — they're not useful in local dev.
    // This purges both pending (delivered but un-ACKed) and undelivered stream entries.
    if hot::env::is_local_dev() {
        const LOCAL_DEV_MAX_AGE_MS: u64 = 60 * 60 * 1000; // 1 hour
        if let Err(e) = event_queue.purge_old_pending(LOCAL_DEV_MAX_AGE_MS).await {
            warn!(
                "hot.dev: WORKER failed to purge old pending messages: {}",
                e
            );
        }
    }

    // Add timeout to prevent hanging on slow Redis operations
    // Also respect Ctrl-C during recovery (otherwise shutdown is blocked for 30s)
    let recovery_timeout = std::time::Duration::from_secs(30);
    let recovery_result = tokio::select! {
        result = tokio::time::timeout(
            recovery_timeout,
            event_queue.recover_orphaned_items_with_data(),
        ) => result,
        _ = hot::signal::shutdown_signal() => {
            info!("hot.dev: WORKER received shutdown signal during orphaned item recovery");
            return Ok(());
        }
    };
    match recovery_result {
        Ok(Ok((count, recovered_data))) if count > 0 => {
            info!(
                "hot.dev: WORKER recovered {} orphaned items - these will be reprocessed",
                count
            );

            // Mark any orphaned runs as failed (runs that were in progress when the worker crashed)
            // We track stream_ids we've already processed to avoid duplicate work
            if let Some(ref db_pool) = db {
                let mut processed_streams: ahash::AHashSet<uuid::Uuid> = ahash::AHashSet::new();

                for raw_bytes in recovered_data {
                    // Try to deserialize and extract event_id and stream_id
                    if let Some(ids) = extract_ids_from_orphaned_data(&raw_bytes) {
                        // If we have a stream_id, fail all runs in that stream
                        // This handles the case where a parent run spawned child runs
                        if let Some(stream_id) = ids.stream_id {
                            // Skip if we've already processed this stream
                            if processed_streams.contains(&stream_id) {
                                continue;
                            }
                            processed_streams.insert(stream_id);

                            match hot::db::run::Run::fail_orphaned_runs_by_stream_id(
                                db_pool,
                                &stream_id,
                                "Worker crashed during event processing - event will be retried",
                            )
                            .await
                            {
                                Ok(failed_count) if failed_count > 0 => {
                                    info!(
                                        "hot.dev: WORKER marked {} orphaned run(s) as failed for stream {}",
                                        failed_count, stream_id
                                    );
                                }
                                Ok(_) => {
                                    // No runs were in running state for this stream
                                }
                                Err(e) => {
                                    warn!(
                                        "hot.dev: WORKER failed to mark orphaned runs as failed for stream {}: {}",
                                        stream_id, e
                                    );
                                }
                            }
                        } else {
                            // No stream_id available, fall back to event_id
                            match hot::db::run::Run::fail_orphaned_runs_by_event_id(
                                db_pool,
                                &ids.event_id,
                                "Worker crashed during event processing - event will be retried",
                            )
                            .await
                            {
                                Ok(failed_count) if failed_count > 0 => {
                                    info!(
                                        "hot.dev: WORKER marked {} orphaned run(s) as failed for event {}",
                                        failed_count, ids.event_id
                                    );
                                }
                                Ok(_) => {
                                    // No runs were in running state for this event
                                }
                                Err(e) => {
                                    warn!(
                                        "hot.dev: WORKER failed to mark orphaned runs as failed for event {}: {}",
                                        ids.event_id, e
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }
        Ok(Ok(_)) => {
            debug!("hot.dev: WORKER no orphaned items found");
        }
        Ok(Err(e)) => {
            error!("hot.dev: WORKER failed to recover orphaned items: {}", e);
            // Don't fail startup - we can continue without recovery
        }
        Err(_) => {
            error!(
                "hot.dev: WORKER orphaned item recovery timed out after {}s - continuing without recovery",
                recovery_timeout.as_secs()
            );
            // Don't fail startup - we can continue without recovery
        }
    }

    // Fast-forward consumer groups past any stale backlog older than each
    // queue's startup window. Critical for workers coming back from a long
    // outage: without this, `XREADGROUP > ` would happily drain the entire
    // multi-day backlog as soon as the worker reconnects, replaying stale work
    // long after callers have moved on.
    //
    // Only matters for Redis queues; Memory queues no-op. Failures are
    // logged and swallowed — fast-forwarding is best-effort and shouldn't
    // block worker startup.
    debug!("hot.dev: WORKER fast-forwarding stale consumer groups past startup window");
    {
        let ff_timeout = std::time::Duration::from_secs(10);
        // Tuples of (queue name, fast-forward future). We can't store these
        // as `&dyn` because the method isn't on a trait — so we just spell
        // out the calls inline.
        let ff_fut = async {
            for (name, result) in [
                (
                    "hot:event",
                    tokio::time::timeout(ff_timeout, event_queue.fast_forward_if_stale()).await,
                ),
                (
                    "hot:request",
                    tokio::time::timeout(ff_timeout, request_queue.fast_forward_if_stale()).await,
                ),
                (
                    "hot:response",
                    tokio::time::timeout(ff_timeout, response_queue.fast_forward_if_stale()).await,
                ),
                (
                    "hot:alert",
                    tokio::time::timeout(ff_timeout, alert_queue.fast_forward_if_stale()).await,
                ),
                (
                    "hot:email",
                    tokio::time::timeout(ff_timeout, email_queue.fast_forward_if_stale()).await,
                ),
            ] {
                match result {
                    Ok(Ok(skipped)) if skipped > 0 => {
                        info!(
                            "hot.dev: WORKER fast-forwarded {} past {} stale entr{}",
                            name,
                            skipped,
                            if skipped == 1 { "y" } else { "ies" },
                        );
                    }
                    Ok(Ok(_)) => {
                        debug!(
                            "hot.dev: WORKER {} consumer group within startup window (no fast-forward needed)",
                            name
                        );
                    }
                    Ok(Err(e)) => {
                        warn!(
                            "hot.dev: WORKER fast-forward failed for {}: {} (continuing)",
                            name, e
                        );
                    }
                    Err(_) => {
                        warn!(
                            "hot.dev: WORKER fast-forward timed out for {} after {}s (continuing)",
                            name,
                            ff_timeout.as_secs()
                        );
                    }
                }
            }
        };

        tokio::select! {
            _ = ff_fut => {}
            _ = hot::signal::shutdown_signal() => {
                info!("hot.dev: WORKER received shutdown signal during fast-forward");
                return Ok(());
            }
        }
    }

    // Purge stuck PEL entries older than each queue's startup window. This is
    // the complement to fast-forward: fast-forward advances the *read cursor*
    // past undelivered backlog, while purge_old_pending ACKs *delivered-but-
    // stuck* PEL entries that no fast-forward can touch.
    //
    // This catches consumers that hold entries in PEL while refreshing their
    // idle timer faster than ORPHAN_IDLE_MS, so XAUTOCLAIM never reclaims
    // them. Purging by age (rather than delivery count) fully releases such
    // entries.
    //
    // Only matters for Redis queues; Memory queues no-op. Failures are
    // logged and swallowed — purge is best-effort and shouldn't block worker
    // startup.
    debug!("hot.dev: WORKER purging stuck PEL entries older than per-queue windows");
    {
        let purge_timeout = std::time::Duration::from_secs(30);
        let purge_fut = async {
            for (name, window, result) in [
                (
                    "hot:event",
                    event_window,
                    tokio::time::timeout(
                        purge_timeout,
                        event_queue.purge_old_pending(event_window.as_millis() as u64),
                    )
                    .await,
                ),
                (
                    "hot:request",
                    request_window,
                    tokio::time::timeout(
                        purge_timeout,
                        request_queue.purge_old_pending(request_window.as_millis() as u64),
                    )
                    .await,
                ),
                (
                    "hot:response",
                    response_window,
                    tokio::time::timeout(
                        purge_timeout,
                        response_queue.purge_old_pending(response_window.as_millis() as u64),
                    )
                    .await,
                ),
                (
                    "hot:alert",
                    alert_window,
                    tokio::time::timeout(
                        purge_timeout,
                        alert_queue.purge_old_pending(alert_window.as_millis() as u64),
                    )
                    .await,
                ),
                (
                    "hot:email",
                    email_window,
                    tokio::time::timeout(
                        purge_timeout,
                        email_queue.purge_old_pending(email_window.as_millis() as u64),
                    )
                    .await,
                ),
            ] {
                match result {
                    Ok(Ok(purged)) if purged > 0 => {
                        info!(
                            "hot.dev: WORKER purged {} stuck PEL entr{} on {} (window: {}s)",
                            purged,
                            if purged == 1 { "y" } else { "ies" },
                            name,
                            window.as_secs()
                        );
                    }
                    Ok(Ok(_)) => {
                        debug!(
                            "hot.dev: WORKER no stuck PEL entries to purge on {} (window: {}s)",
                            name,
                            window.as_secs()
                        );
                    }
                    Ok(Err(e)) => {
                        warn!(
                            "hot.dev: WORKER purge_old_pending failed for {}: {} (continuing)",
                            name, e
                        );
                    }
                    Err(_) => {
                        warn!(
                            "hot.dev: WORKER purge_old_pending timed out for {} after {}s (continuing)",
                            name,
                            purge_timeout.as_secs()
                        );
                    }
                }
            }
        };

        tokio::select! {
            _ = purge_fut => {}
            _ = hot::signal::shutdown_signal() => {
                info!("hot.dev: WORKER received shutdown signal during PEL purge");
                return Ok(());
            }
        }
    }

    // Clean up stale consumers and trim old stream entries
    debug!("hot.dev: WORKER cleaning up stale consumers and trimming streams");
    {
        use hot::queue::StreamCleanup;
        let cleanup_timeout = std::time::Duration::from_secs(30);
        let queues: Vec<(&str, &dyn StreamCleanup)> = vec![
            ("hot:event", &event_queue),
            ("hot:request", &request_queue),
            ("hot:response", &response_queue),
            ("hot:alert", &alert_queue),
            ("hot:email", &email_queue),
        ];

        let cleanup_fut = async {
            for (name, queue) in &queues {
                match tokio::time::timeout(cleanup_timeout, queue.cleanup_streams()).await {
                    Ok(Ok((consumers, trimmed))) => {
                        if consumers > 0 || trimmed > 0 {
                            info!(
                                "hot.dev: WORKER stream cleanup on {}: removed {} stale consumers, trimmed {} entries",
                                name, consumers, trimmed
                            );
                        }
                    }
                    Ok(Err(e)) => {
                        warn!("hot.dev: WORKER stream cleanup failed for {}: {}", name, e);
                    }
                    Err(_) => {
                        warn!(
                            "hot.dev: WORKER stream cleanup timed out for {} after {}s",
                            name,
                            cleanup_timeout.as_secs()
                        );
                    }
                }
            }
        };

        tokio::select! {
            _ = cleanup_fut => {}
            _ = hot::signal::shutdown_signal() => {
                info!("hot.dev: WORKER received shutdown signal during stream cleanup");
                return Ok(());
            }
        }
    }

    // In local dev, run call retention cleanup on startup (runs in background so it
    // doesn't block worker startup — the delete batches yield between iterations).
    if hot::env::is_local_dev()
        && let Some(ref db) = db
    {
        let cleanup_db = db.clone();
        let cleanup_conf = worker_conf.clone();
        tokio::spawn(async move {
            let days = hot::db::get_local_call_retention_days(&cleanup_conf);
            if days >= 0 {
                match hot::db::call::Call::delete_older_than(&cleanup_db, days, 10_000).await {
                    Ok(count) if count > 0 => {
                        info!(
                            "hot.dev: WORKER startup call retention cleanup deleted {} rows (>{} days)",
                            count, days
                        );
                    }
                    Ok(_) => {}
                    Err(e) => {
                        error!(
                            "hot.dev: WORKER startup call retention cleanup failed: {}",
                            e
                        );
                    }
                }
            }
        });
    }

    // Number of worker threads to spawn. This first second-batch step only
    // caps concurrency; it does not raise fan-out beyond worker.threads.
    let requested_worker_count = threads.unwrap_or(DEFAULT_WORKER_THREADS);
    let vm_budget =
        hot::runtime_budget::derive_worker_vm_concurrency(&worker_conf, requested_worker_count);
    let worker_count = vm_budget.resolved;

    info!(
        "hot.dev: WORKER starting with {} concurrent workers (requested={}, cpu_limit={}, memory_limit={:?}, memory_limit_mb={:?}, explicit_vm_concurrency={}, shared_process={})",
        worker_count,
        vm_budget.requested,
        vm_budget.cpu_limit,
        vm_budget.memory_limit,
        vm_budget.memory_limit_mb,
        vm_budget.explicit,
        vm_budget.shared_process,
    );

    // Create shared bytecode cache for all workers (in-memory LRU cache)
    // This allows workers to share cached bytecode in memory, avoiding disk I/O
    let shared_cache =
        Arc::new(hot::lang::cache::bytecode_cache::BytecodeCache::default_location());
    debug!("hot.dev: WORKER initialized shared bytecode cache");

    // Create shared build path cache for all workers
    // This maps build_id -> extracted directory path for deployed builds
    let build_path_cache = Arc::new(BuildPathCache::new());
    debug!("hot.dev: WORKER initialized build path cache");

    // Create shutdown coordinator for graceful shutdown
    // Config: hot.worker.shutdown-timeout / HOT_WORKER_SHUTDOWN_TIMEOUT (default 60s)
    let shutdown_timeout_secs = worker_conf.get_int_or_default(
        "worker.shutdown-timeout",
        DEFAULT_SHUTDOWN_TIMEOUT_SECONDS as i64,
    );
    let shutdown_timeout = if shutdown_timeout_secs > 0 {
        shutdown_timeout_secs as u64
    } else {
        DEFAULT_SHUTDOWN_TIMEOUT_SECONDS
    };
    let shutdown_coordinator = Arc::new(ShutdownCoordinator::new(shutdown_timeout));
    debug!(
        "hot.dev: WORKER graceful shutdown enabled (timeout: {}s)",
        shutdown_timeout
    );

    // Channel to signal workers to shutdown
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    // Spawn a single per-process janitor task that performs two periodic
    // maintenance jobs against all queues:
    //
    //   1. Every tick: reclaim_orphans (XAUTOCLAIM) — picks up entries
    //      delivered to a now-dead consumer that are past ORPHAN_IDLE_MS.
    //   2. Every CLEANUP_EVERY_N_TICKS: cleanup_streams — reaps stale
    //      consumers (XGROUP DELCONSUMER) and trims old entries (XTRIM).
    //
    // This replaces the old per-worker, per-call autoclaim that ran on every
    // dequeue thread (N_threads × N_queues background scans every 30s); one
    // task here serves the whole process. The cleanup_streams piece closes a
    // gap where stale UUID-named consumers from previous deploys would only
    // be reaped by the daily scheduler maintenance run (and even then, only
    // for hot:event and hot:task — see the queue_cleanup handler below).
    //
    // Tick interval rationale: 60s aligns with ORPHAN_IDLE_MS=60s. Polling
    // faster would do guaranteed-empty XAUTOCLAIM calls (an entry can't
    // possibly have crossed the idle threshold in less than ORPHAN_IDLE_MS).
    // Worst-case orphan recovery latency = ORPHAN_IDLE_MS + interval = 120s,
    // which is well under any user-visible threshold.
    if matches!(queue_type, QueueType::Redis) {
        use hot::queue::StreamCleanup;
        // Janitor uses w0's stable consumer name as the XAUTOCLAIM destination
        // so reclaimed PEL entries land in a real worker's PEL (drained by w0
        // via `XREADGROUP ... 0` on its next dequeue tick). Without this the
        // janitor's UUID consumer would collect entries that nobody dequeues.
        // We deliberately omit hot:task
        // here: that queue is owned by hot_task_worker, which runs its own
        // janitor under the proper consumer identity.
        let event_q = event_queue
            .clone()
            .with_consumer_name(admin_event_name.clone());
        let request_q = request_queue
            .clone()
            .with_consumer_name(admin_request_name.clone());
        let response_q = response_queue
            .clone()
            .with_consumer_name(admin_response_name.clone());
        let alert_q = alert_queue
            .clone()
            .with_consumer_name(admin_alert_name.clone());
        let email_q = email_queue
            .clone()
            .with_consumer_name(admin_email_name.clone());
        let mut janitor_shutdown_rx = shutdown_rx.clone();
        tokio::spawn(async move {
            const TICK: std::time::Duration = std::time::Duration::from_secs(60);
            // Run cleanup_streams every 5 ticks (= every 5 minutes). Slower
            // than reclaim_orphans because it does XINFO CONSUMERS + per-
            // consumer XGROUP DELCONSUMER + XTRIM, which is more work and
            // doesn't need 60s freshness — IDLE_CONSUMER_NO_PENDING_MS is
            // 1h, so 5min cleanup latency adds at most ~8% to a stale
            // consumer's dwell time.
            const CLEANUP_EVERY_N_TICKS: u64 = 5;
            let mut tick: u64 = 0;
            loop {
                tokio::select! {
                    biased;
                    changed = janitor_shutdown_rx.changed() => {
                        if changed.is_err() || *janitor_shutdown_rx.borrow() {
                            debug!("hot.dev: WORKER janitor shutting down");
                            break;
                        }
                    }
                    _ = tokio::time::sleep(TICK) => {
                        tick = tick.wrapping_add(1);
                        let queues: [(&str, &dyn StreamCleanup); 5] = [
                            ("hot:event", &event_q),
                            ("hot:request", &request_q),
                            ("hot:response", &response_q),
                            ("hot:alert", &alert_q),
                            ("hot:email", &email_q),
                        ];

                        // Phase 1: reclaim orphaned PEL entries from dead
                        // consumers (every tick).
                        for (name, q) in &queues {
                            match q.reclaim_orphans().await {
                                Ok(0) => {}
                                Ok(n) => {
                                    info!(
                                        "hot.dev: WORKER janitor reclaimed {} orphaned messages on {}",
                                        n, name
                                    );
                                }
                                Err(e) => {
                                    debug!(
                                        "hot.dev: WORKER janitor reclaim failed on {}: {}",
                                        name, e
                                    );
                                }
                            }
                        }

                        // Phase 2: reap stale consumers + trim streams
                        // (every CLEANUP_EVERY_N_TICKS).
                        if tick.is_multiple_of(CLEANUP_EVERY_N_TICKS) {
                            for (name, q) in &queues {
                                match q.cleanup_streams().await {
                                    Ok((0, 0)) => {}
                                    Ok((consumers, trimmed)) => {
                                        info!(
                                            "hot.dev: WORKER janitor cleanup on {}: removed {} stale consumers, trimmed {} entries",
                                            name, consumers, trimmed
                                        );
                                    }
                                    Err(e) => {
                                        debug!(
                                            "hot.dev: WORKER janitor cleanup failed on {}: {}",
                                            name, e
                                        );
                                    }
                                }
                            }
                        }
                    }
                }
            }
        });
    }

    // Spawn one claimer per queue. Each claimer blocks on exactly one Redis
    // stream key (cluster-safe) and hands off at most one claimed lease at a
    // time into this bounded channel. Executors below provide the VM/run
    // concurrency bound.
    let handoff_capacity = worker_count.max(1);
    let (claimed_tx, claimed_rx) = mpsc::channel::<ClaimedWorkerQueue>(handoff_capacity);
    let claimed_rx = Arc::new(Mutex::new(claimed_rx));
    let mut worker_handles = Vec::new();

    worker_handles.push(spawn_worker_queue_claimer(
        "hot:request",
        request_queue
            .clone()
            .with_consumer_name(admin_request_name.clone()),
        claimed_tx.clone(),
        shutdown_rx.clone(),
        ClaimedWorkerQueue::Request,
    ));
    worker_handles.push(spawn_worker_queue_claimer(
        "hot:event",
        event_queue
            .clone()
            .with_consumer_name(admin_event_name.clone()),
        claimed_tx.clone(),
        shutdown_rx.clone(),
        ClaimedWorkerQueue::Event,
    ));
    if db.is_some() {
        worker_handles.push(spawn_worker_queue_claimer(
            "hot:alert",
            alert_queue
                .clone()
                .with_consumer_name(admin_alert_name.clone()),
            claimed_tx.clone(),
            shutdown_rx.clone(),
            ClaimedWorkerQueue::Alert,
        ));
        worker_handles.push(spawn_worker_queue_claimer(
            "hot:email",
            email_queue
                .clone()
                .with_consumer_name(admin_email_name.clone()),
            claimed_tx.clone(),
            shutdown_rx.clone(),
            ClaimedWorkerQueue::Email,
        ));
    }
    drop(claimed_tx);

    // Spawn bounded executor tasks. Queue consumers live in the claimers above;
    // these handles are only for enqueue/admin maintenance paths.
    for worker_id in 0..worker_count {
        let request_queue_clone = request_queue
            .clone()
            .with_consumer_name(admin_request_name.clone());
        let response_queue_clone = response_queue
            .clone()
            .with_consumer_name(admin_response_name.clone());
        let event_queue_clone = event_queue
            .clone()
            .with_consumer_name(admin_event_name.clone());
        let alert_queue_clone = alert_queue
            .clone()
            .with_consumer_name(admin_alert_name.clone());
        let email_queue_clone = email_queue
            .clone()
            .with_consumer_name(admin_email_name.clone());
        let claimed_rx_clone = claimed_rx.clone();
        let db_clone = db.clone();
        let worker_conf_clone = worker_conf.clone();
        let emitter_clone = emitter.clone();
        let event_publisher_clone = event_publisher.clone();
        let encryption_clone = encryption.clone();
        let cache_clone = shared_cache.clone();
        let build_path_cache_clone = build_path_cache.clone();
        let shutdown_coordinator_clone = shutdown_coordinator.clone();
        let dev_context_storage_clone = dev_context_storage.clone();
        let stream_publisher_clone = stream_publisher.clone();
        let task_queue_clone = task_queue.clone();
        // Per-worker captures of the admin consumer names so the daily
        // queue_cleanup handler (built inside the worker spawn) can pin its
        // queue handles to w0's identity. See comment on `admin_*_name`
        // declarations above for the rationale.
        let admin_event_name_for_worker = admin_event_name.clone();
        let admin_request_name_for_worker = admin_request_name.clone();
        let admin_response_name_for_worker = admin_response_name.clone();
        let admin_alert_name_for_worker = admin_alert_name.clone();
        let admin_email_name_for_worker = admin_email_name.clone();

        let handle: JoinHandle<Result<(), Box<dyn std::error::Error + Send + Sync>>> = tokio::spawn(
            async move {
                info!("hot.dev: WORKER {} started", worker_id);

                loop {
                    let claimed_queue = {
                        let mut claimed_rx = claimed_rx_clone.lock().await;
                        claimed_rx.recv().await
                    };

                    let Some(claimed_queue) = claimed_queue else {
                        debug!("hot.dev: WORKER {} executor stopping", worker_id);
                        break;
                    };

                    match claimed_queue {
                        ClaimedWorkerQueue::Request(lease) => {
                            // Process ONE request message (atomic operation)
                            match lease.process(|message| {
                        // Clone the response queue for use inside the async closure
                        let response_queue = response_queue_clone.clone();
                        async move {
                            // Check message type and process accordingly
                            let msg_type = message.head.get_str("__type");

                            match msg_type.as_str() {
                                "RequestMessage" => {
                                    // Convert to RequestMessage and process
                                    let request_msg: RequestMessage = message.try_into()
                                        .map_err(|e| Box::new(std::io::Error::other(e)) as Box<dyn std::error::Error + Send + Sync>)?;

                                    // Log the received message
                                    info!("hot.dev: WORKER {} received request from hot:request queue: id={} head={:?} body={}",
                                        worker_id, request_msg.id, request_msg.head, request_msg.body.to_string());

                                    // Process the message and create a response
                                    let response_msg = ResponseMessage {
                                        id: request_msg.id, // Use the same ID for correlation
                                        head: request_msg.head.clone(), // Clone the head
                                        body: request_msg.body.clone(), // Clone the body for simplicity
                                    };

                                    // Convert response to unified Message format
                                    let response_message: Message = response_msg.into();

                                    // Send the response to the response queue
                                    response_queue.enqueue(response_message).await.map_err(|e| {
                                        let error_msg = format!("Failed to send response message: {}", e);
                                        info!("hot.dev: WORKER {} {}", worker_id, error_msg);
                                        Box::new(std::io::Error::other(error_msg)) as Box<dyn std::error::Error + Send + Sync>
                                    })?;

                                    info!("hot.dev: WORKER {} sent response to hot:response queue", worker_id);
                                },
                                _ => {
                                    info!("hot.dev: WORKER {} received unknown message type '{}' on request queue", worker_id, msg_type);
                                }
                            }

                            // Return some value as the processing result
                            Ok(()) as Result<(), Box<dyn std::error::Error + Send + Sync>>
                        }
                    }).await {
                        Ok(Some(_)) => {
                        },
                        Ok(None) => {
                            // No messages in queue - continue to events
                        },
                        Err(e) => {
                            info!("hot.dev: WORKER {} error processing request message: {}", worker_id, e);
                        }
                    }
                        }
                        ClaimedWorkerQueue::Event(lease) => {
                            // Process ONE event message (atomic operation)
                            match lease.process(|message| {
                                let db_ref = db_clone.clone();
                                let worker_conf_ref = worker_conf_clone.clone();
                                let _emitter_ref = emitter_clone.clone();
                                let _event_publisher_ref = event_publisher_clone.clone();
                                let encryption_ref = encryption_clone.clone();
                                let cache_ref = cache_clone.clone();
                                let build_path_cache_ref = build_path_cache_clone.clone();
                                let shutdown_coord_ref = shutdown_coordinator_clone.clone();
                                let dev_context_storage_ref = dev_context_storage_clone.clone();
                                let stream_publisher_ref = stream_publisher_clone.clone();
                                let task_queue_ref = task_queue_clone.clone();
                                // Daily queue_cleanup admin handles. Each is pinned to w0's
                                // stable consumer name so any XAUTOCLAIM done by
                                // cleanup_stale_consumers (when draining a stale consumer that
                                // still has PEL) lands in w0's PEL — drained by the live worker.
                                // Without the pin, RedisStreamQueue::clone regenerates a UUID
                                // consumer name and reclaimed entries become unreachable.
                                // Note: hot:task is intentionally NOT in the cleanup list here;
                                // hot_task_worker's janitor owns that queue.
                                let event_queue_ref = event_queue_clone
                                    .clone()
                                    .with_consumer_name(admin_event_name_for_worker.clone());
                                let request_queue_ref = request_queue_clone
                                    .clone()
                                    .with_consumer_name(admin_request_name_for_worker.clone());
                                let response_queue_ref = response_queue_clone
                                    .clone()
                                    .with_consumer_name(admin_response_name_for_worker.clone());
                                let alert_queue_ref = alert_queue_clone
                                    .clone()
                                    .with_consumer_name(admin_alert_name_for_worker.clone());
                                let email_queue_ref = email_queue_clone
                                    .clone()
                                    .with_consumer_name(admin_email_name_for_worker.clone());
                                async move {
                                    // Check message type and process accordingly
                                    let msg_type = message.head.get_str("__type");

                                    match msg_type.as_str() {
                                        "EventMessage" => {
                                            // TIMING: Start timing from dequeue
                                            let event_dequeue_time = std::time::Instant::now();

                                            // Convert to EventMessage and process
                                            let event_message: EventMessage = message.try_into()
                                                .map_err(|e| Box::new(std::io::Error::other(e)) as Box<dyn std::error::Error + Send + Sync>)?;

                                            // Log the received event message (keep run_id/event_id/fn visible, hide event_data)
                                            debug!("hot.dev: WORKER {} received event: id={} type={} (event.created_at={})",
                                                worker_id,
                                                event_message.id,
                                                event_message.body.event.event_type,
                                                event_message.body.event.event_time);

                                            // Process the event with multi-project support
                                            // Uses `once: true` metadata to determine which handlers run once vs all
                                            if let Some(ref db) = db_ref {
                                                let env_id = event_message.body.event.env_id;

                                                debug!("TIMING: event {} dequeued, starting processing", event_message.id);

                                                // SECURITY: Verify event exists in database and env_id matches
                                                // This prevents attacks where a malicious actor with queue access
                                                // injects messages with spoofed env_ids to trigger handlers in other environments
                                                let event_id = event_message.body.event.event_id;
                                                match hot::db::event::Event::get_event(db, &event_id).await {
                                                    Ok(stored_event) => {
                                                        if stored_event.env_id != env_id {
                                                            let err_msg = format!(
                                                                "Security: env_id mismatch - message claims {} but event {} belongs to {}",
                                                                env_id, event_id, stored_event.env_id
                                                            );
                                                            error!("hot.dev: WORKER {} {}", worker_id, err_msg);
                                                            return Err(Box::new(std::io::Error::other(err_msg)) as Box<dyn std::error::Error + Send + Sync>);
                                                        }
                                                        debug!("hot.dev: WORKER {} verified event {} belongs to env {}", worker_id, event_id, env_id);
                                                    }
                                                    Err(hot::db::event::EventError::NotFound) => {
                                                        // Event not in database - could be a race condition or malicious message
                                                        // For internal events (hot:call, hot:schedule), the event should exist
                                                        // For API-published events, the event is written before queueing
                                                        let err_msg = format!(
                                                            "Security: event {} not found in database - cannot verify env_id",
                                                            event_id
                                                        );
                                                        error!("hot.dev: WORKER {} {}", worker_id, err_msg);
                                                        return Err(Box::new(std::io::Error::other(err_msg)) as Box<dyn std::error::Error + Send + Sync>);
                                                    }
                                                    Err(e) => {
                                                        let err_msg = format!(
                                                            "Security: failed to verify event {} env_id - database error: {}",
                                                            event_id, e
                                                        );
                                                        error!("hot.dev: WORKER {} {}", worker_id, err_msg);
                                                        return Err(Box::new(std::io::Error::other(err_msg)) as Box<dyn std::error::Error + Send + Sync>);
                                                    }
                                                }

                                                debug!("TIMING: event {} security check done: {:?}", event_message.id, event_dequeue_time.elapsed());

                                                // Step 1: Get ALL event handlers across ALL deployed builds in this environment
                                                match EventHandler::get_event_handlers_by_env_and_event_type(
                                                    db,
                                                    &env_id,
                                                    &event_message.body.event.event_type,
                                                ).await {
                                                    Ok(all_handlers) => {
                                                        if all_handlers.is_empty() {
                                                            let err_msg = format!("No event handlers found for event type '{}'", event_message.body.event.event_type);
                                                            debug!("hot.dev: WORKER {} {}", worker_id, err_msg);
                                                            return Err(Box::new(std::io::Error::other(err_msg)) as Box<dyn std::error::Error + Send + Sync>);
                                                        }

                                                        // Step 2: Partition handlers by `once` flag
                                                        let (once_handlers, multi_handlers) = partition_handlers_by_once_flag(all_handlers);

                                                        debug!("hot.dev: WORKER {} found {} once-handler(s) and {} multi-handler(s) for event type '{}'",
                                                            worker_id, once_handlers.len(), multi_handlers.len(), event_message.body.event.event_type);

                                                        // Step 3: Determine which handlers to execute
                                                        // - For once handlers: pick the appropriate one (for hot:call/schedule, route by function)
                                                        // - For multi handlers: execute all of them
                                                        let mut handlers_to_execute: Vec<EventHandler> = Vec::new();

                                                        // Handle `once: true` handlers - pick ONE to execute
                                                        // Track routing info for warning in run.info
                                                        let mut routing_warning: Option<serde_json::Value> = None;

                                                        if !once_handlers.is_empty() {
                                                            // For hot:call and hot:schedule, use function-based routing
                                                            if event_message.body.event.event_type == "hot:call"
                                                                || event_message.body.event.event_type == "hot:schedule"
                                                            {
                                                                if let Some(target_fn) = extract_target_function_from_event(&event_message.body.event.event_data) {
                                                                    debug!("hot.dev: WORKER {} routing {} to build with function '{}'",
                                                                        worker_id, event_message.body.event.event_type, target_fn);

                                                                    // Get target_project_id from event for tie-breaking
                                                                    let target_project_id = event_message.body.event.target_project_id;

                                                                    // Find the handler whose build contains the target function
                                                                    match find_build_for_function(db, &env_id, &target_fn, &cache_ref, &worker_conf_ref, target_project_id, &build_path_cache_ref).await {
                                                                        Ok(Some(routing_result)) => {
                                                                            // Find the once handler from that build
                                                                            if let Some(handler) = once_handlers.iter().find(|h| h.build_id == routing_result.build.build_id) {
                                                                                handlers_to_execute.push(handler.clone());

                                                                                // If tie-breaker was used, record warning info
                                                                                if routing_result.tie_breaker_used {
                                                                                    routing_warning = Some(serde_json::json!({
                                                                                        "warning": "route:dup",
                                                                                        "matched_projects": routing_result.matched_project_names,
                                                                                        "selected_project": routing_result.project_name,
                                                                                        "function": target_fn
                                                                                    }));
                                                                                }
                                                                            } else {
                                                                                // Build found but no handler for that build - this shouldn't happen
                                                                                let err_msg = format!(
                                                                                    "Function '{}' found in build {} but no event handler registered for that build",
                                                                                    target_fn, routing_result.build.build_id
                                                                                );
                                                                                error!("hot.dev: WORKER {} {}", worker_id, err_msg);
                                                                                return Err(Box::new(std::io::Error::other(err_msg)) as Box<dyn std::error::Error + Send + Sync>);
                                                                            }
                                                                        }
                                                                        Ok(None) => {
                                                                            // Function not in any active build — expected during deploy transitions
                                                                            // when previously enqueued events/schedules reference old code
                                                                            let err_msg = format!(
                                                                                "Function '{}' not found in any deployed build. This may be due to cache issues - try redeploying the build.",
                                                                                target_fn
                                                                            );
                                                                            warn!("hot.dev: WORKER {} {}", worker_id, err_msg);
                                                                            return Err(Box::new(std::io::Error::other(err_msg)) as Box<dyn std::error::Error + Send + Sync>);
                                                                        }
                                                                        Err(e) => {
                                                                            let err_msg = format!(
                                                                                "Failed to route function '{}': {}",
                                                                                target_fn, e
                                                                            );
                                                                            error!("hot.dev: WORKER {} {}", worker_id, err_msg);
                                                                            return Err(Box::new(std::io::Error::other(err_msg)) as Box<dyn std::error::Error + Send + Sync>);
                                                                        }
                                                                    }
                                                                } else {
                                                                    // Can't extract function from event data - fail explicitly
                                                                    let err_msg = format!(
                                                                        "Cannot extract target function from {} event data",
                                                                        event_message.body.event.event_type
                                                                    );
                                                                    error!("hot.dev: WORKER {} {}", worker_id, err_msg);
                                                                    return Err(Box::new(std::io::Error::other(err_msg)) as Box<dyn std::error::Error + Send + Sync>);
                                                                }
                                                            } else {
                                                                // For other event types with once handlers, just pick the first one
                                                                handlers_to_execute.push(once_handlers[0].clone());
                                                            }
                                                        }

                                                        // Add all multi handlers (no `once` flag)
                                                        handlers_to_execute.extend(multi_handlers);

                                                        if handlers_to_execute.is_empty() {
                                                            let err_msg = format!("No handlers to execute for event type '{}'", event_message.body.event.event_type);
                                                            error!("hot.dev: WORKER {} {}", worker_id, err_msg);
                                                            return Err(Box::new(std::io::Error::other(err_msg)) as Box<dyn std::error::Error + Send + Sync>);
                                                        }

                                                        debug!("hot.dev: WORKER {} executing {} handler(s) for event type '{}'",
                                                            worker_id, handlers_to_execute.len(), event_message.body.event.event_type);

                                                        // Step 4: Execute the selected handlers
                                                        let execution_result: (Result<(), String>, bool) = {
                                                            let mut all_success = true;
                                                            let mut is_first_handler = true;
                                                            for event_handler in handlers_to_execute {
                                                                // Get the build for this handler
                                                                let handler_build = match Build::get_build(db, &event_handler.build_id).await {
                                                                    Ok(b) => b,
                                                                    Err(e) => {
                                                                        error!("hot.dev: WORKER {} failed to get build for handler {}: {}",
                                                                            worker_id, event_handler.event_handler_id, e);
                                                                        all_success = false;
                                                                        continue;
                                                                    }
                                                                };

                                                                // Skip if build is no longer deployed (deactivated while event was in queue)
                                                                if !handler_build.deployed {
                                                                    warn!("hot.dev: WORKER {} skipping handler {}/{} - build {} is no longer deployed",
                                                                        worker_id, event_handler.ns, event_handler.var, handler_build.build_id);
                                                                    continue;
                                                                }

                                                                // Get timeout from config (hot.worker.run-timeout)
                                                                let timeout_duration = get_run_timeout(&worker_conf_ref);

                                                                // Generate run_id here so we can unregister it on timeout
                                                                let run_id = Uuid::now_v7();
                                                                shutdown_coord_ref.register_run(run_id);

                                                                debug!("hot.dev: WORKER {} executing {}/{} (event_id={}, run_id={}, event_handler_id={}, build_id={})",
                                                                    worker_id,
                                                                    event_handler.ns,
                                                                    event_handler.var,
                                                                    event_message.body.event.event_id,
                                                                    run_id,
                                                                    event_handler.event_handler_id,
                                                                    handler_build.build_id);

                                                                debug!("TIMING: event {} handler lookup done, calling execute_single_event_handler: {:?}", event_message.id, event_dequeue_time.elapsed());

                                                                let execution_future = execute_single_event_handler(db, &handler_build, &env_id, &worker_conf_ref, &event_handler, &event_message, _emitter_ref.clone(), _event_publisher_ref.clone(), encryption_ref.clone(), cache_ref.clone(), shutdown_coord_ref.clone(), run_id, build_path_cache_ref.clone(), dev_context_storage_ref.clone(), stream_publisher_ref.clone(), task_queue_ref.clone());

                                                                                            match tokio::time::timeout(timeout_duration, execution_future).await {
                                                                                                Ok(Ok(())) => {
                                                                                                    debug!("Successfully executed event handler: {}", event_handler.event_handler_id);
                                                                                                    // Run is already unregistered by execute_single_event_handler

                                                                                                    // Update run info with routing warning if this is the first handler
                                                                                                    // and a tie-breaker was used (routing_warning is set)
                                                                                                    if is_first_handler
                                                                                                        && let Some(ref warning_info) = routing_warning
                                                                                                            && let Err(e) = hot::db::run::Run::update_info(db, &run_id, Some(warning_info)).await {
                                                                                                                warn!("hot.dev: WORKER {} failed to update run {} info with routing warning: {}", worker_id, run_id, e);
                                                                                                            }
                                                                                                }
                                                                                                Ok(Err(e)) => {
                                                                                                    error!("Failed to execute event handler {}: {}", event_handler.event_handler_id, e);
                                                                                                    all_success = false;
                                                                                                    // Run is already unregistered by execute_single_event_handler

                                                                                                    // Still update run info with routing warning even on failure
                                                                                                    if is_first_handler
                                                                                                        && let Some(ref warning_info) = routing_warning
                                                                                                            && let Err(e) = hot::db::run::Run::update_info(db, &run_id, Some(warning_info)).await {
                                                                                                                warn!("hot.dev: WORKER {} failed to update run {} info with routing warning: {}", worker_id, run_id, e);
                                                                                                            }
                                                                                                    // Continue with other handlers even if one fails
                                                                                                }
                                                                                                Err(_) => {
                                                                                                    let timeout_secs = timeout_duration.as_secs();
                                                                                                    error!("hot.dev: WORKER {} run {} TIMED OUT after {}s executing {}/{}",
                                                                                                        worker_id, run_id, timeout_secs, event_handler.ns, event_handler.var);

                                                                                                    // Mark the run as failed with timeout error
                                                                                                    let timeout_error = format!(
                                                                                                        "Run timed out after {} seconds. Configure HOT_WORKER_RUN_TIMEOUT to increase the limit.",
                                                                                                        timeout_secs
                                                                                                    );
                                                                                                    if let Err(e) = hot::db::run::Run::fail_run(
                                                                                                        db,
                                                                                                        &run_id,
                                                                                                        &timeout_error
                                                                                                    ).await {
                                                                                                        error!("hot.dev: WORKER {} failed to mark run {} as failed: {}", worker_id, run_id, e);
                                                                                                    }

                                                                                                    // Update run info with routing warning even on timeout
                                                                                                    if is_first_handler
                                                                                                        && let Some(ref warning_info) = routing_warning
                                                                                                            && let Err(e) = hot::db::run::Run::update_info(db, &run_id, Some(warning_info)).await {
                                                                                                                warn!("hot.dev: WORKER {} failed to update run {} info with routing warning: {}", worker_id, run_id, e);
                                                                                                            }

                                                                                                    // Unregister the run since timeout prevented normal cleanup
                                                                                                    shutdown_coord_ref.unregister_run(&run_id);
                                                                                                    all_success = false;
                                                                                                    // Continue with other handlers even if one times out
                                                                                                }
                                                                                            }
                                                                                            is_first_handler = false;
                                                                                        }
                                                                                        // Always return Ok() - run was created, event should not be retried
                                                                                        (Ok(()), all_success)
                                                                                    };

                                                                                    match execution_result {
                                                                                        (Ok(()), success) => {
                                                                                            if success {
                                                                                                debug!("hot.dev: WORKER {} successfully executed all event handlers for event type '{}'",
                                                                                                      worker_id, event_message.body.event.event_type);
                                                                                            } else {
                                                                                                debug!("hot.dev: WORKER {} completed event handlers for event type '{}' with some failures (run was created)",
                                                                                                      worker_id, event_message.body.event.event_type);
                                                                                            }
                                                                                        }
                                                                                        (Err(e), _) => {
                                                                                            // This should never happen now since we always return Ok()
                                                                                            let err_msg = format!("Failed to execute event handlers for event type '{}': {}", event_message.body.event.event_type, e);
                                                                                            error!("hot.dev: WORKER {} {}", worker_id, err_msg);
                                                                                            return Err(Box::new(std::io::Error::other(err_msg)) as Box<dyn std::error::Error + Send + Sync>);
                                                                                        }
                                                                                    }
                                                                    }
                                                                    Err(e) => {
                                                                        let err_msg = format!("Failed to get event handlers: {}", e);
                                                                        error!("hot.dev: WORKER {} {}", worker_id, err_msg);
                                                                        return Err(Box::new(std::io::Error::other(err_msg)) as Box<dyn std::error::Error + Send + Sync>);
                                                                    }
                                                                }
                                                            } else {
                                                                let err_msg = "Cannot process events: no database connection".to_string();
                                                                error!("hot.dev: WORKER {} {}", worker_id, err_msg);
                                                                return Err(Box::new(std::io::Error::other(err_msg)) as Box<dyn std::error::Error + Send + Sync>);
                                                            }

                                            debug!("hot.dev: WORKER {} processed event '{}' successfully",
                                                worker_id,
                                                event_message.body.event.event_type);
                                        },
                                        "DeploymentMessage" => {
                                            // Convert to DeploymentMessage and process
                                            let deployment_message: hot::lang::event::DeploymentMessage = message.try_into()
                                                .map_err(|e| Box::new(std::io::Error::other(e)) as Box<dyn std::error::Error + Send + Sync>)?;

                                            debug!("hot.dev: WORKER {} received deployment from hot:event queue: id={} build_id={}",
                                                worker_id,
                                                deployment_message.id,
                                                deployment_message.body.build_id);

                                            // Process the deployment
                                            if let Some(ref db) = db_ref {
                                                // Get the build from database to derive env_id and org_id
                                                match Build::get_build(db, &deployment_message.body.build_id).await {
                                                    Ok(build) => {
                                                        // Get project to derive env_id
                                                        match Project::get_project(db, &build.project_id).await {
                                                            Ok(project) => {
                                                                // Get environment to derive org_id
                                                                match hot::db::Env::get_env(db, &project.env_id).await {
                                                                    Ok(env) => {
                                                                        debug!("hot.dev: WORKER {} processing deployment for build {} (env: {}, org: {})",
                                                                            worker_id,
                                                                            build.build_id,
                                                                            project.env_id,
                                                                            env.org_id);

                                                                        // Download the build from storage
                                                                        match hot::storage::build_storage_from_config(&worker_conf_ref).await {
                                                                            Ok(storage) => {
                                                                                match storage.retrieve_build(
                                                                                    &build.build_id,
                                                                                    &env.org_id,
                                                                                    &project.env_id,
                                                                                ).await {
                                                                                    Ok(build_data) => {
                                                                                        debug!("hot.dev: WORKER {} retrieved build {} from storage ({} bytes)",
                                                                                            worker_id,
                                                                                            build.build_id,
                                                                                            build_data.len());

                                                                                        // Extract the build with locking to prevent race conditions
                                                                                        // In-process lock (thread safety)
                                                                                        let extraction_lock = build_path_cache_ref.get_extraction_lock(&build.build_id);
                                                                                        let _lock_guard = extraction_lock.lock().await;

                                                                                        // Double-check in-memory cache after acquiring lock
                                                                                        if build_path_cache_ref.get(&build.build_id).is_some() {
                                                                                            debug!("hot.dev: WORKER {} build {} already extracted by another thread",
                                                                                                worker_id,
                                                                                                build.build_id);
                                                                                        } else {
                                                                                            let extract_dir = std::path::PathBuf::from(format!(".hot/run/build-{}", build.build_id.as_simple()));

                                                                                            // Cross-process file lock
                                                                                            let mut file_lock = BuildPathCache::acquire_file_lock(&build.build_id).ok();
                                                                                            let _file_lock_guard = file_lock.as_mut().and_then(|lock| lock.try_write().ok());

                                                                                            // Check if another process already extracted
                                                                                            if BuildPathCache::is_extraction_complete(&extract_dir) {
                                                                                                debug!("hot.dev: WORKER {} build {} already extracted by another process",
                                                                                                    worker_id,
                                                                                                    build.build_id);
                                                                                                build_path_cache_ref.insert(build.build_id, extract_dir);
                                                                                            } else {
                                                                                                match hot::bundle::extract_bundle_from_bytes(&build_data, &extract_dir) {
                                                                                                    Ok(()) => {
                                                                                                        debug!("hot.dev: WORKER {} extracted build {} to {}",
                                                                                                            worker_id,
                                                                                                            build.build_id,
                                                                                                            extract_dir.display());

                                                                                                        // Read the manifest to get the correct cache key
                                                                                                        let manifest = hot::bundle::read_bundle_manifest(&extract_dir);
                                                                                                        let (proj_name, cache_key, file_hashes) = match &manifest {
                                                                                                            Ok(m) => {
                                                                                                                (m.bundle_name.clone(), m.cache_key.clone(), Some(m.file_hashes.clone()))
                                                                                                            }
                                                                                                            Err(_) => (project.name.clone(), None, None)
                                                                                                        };

                                                                                                        // Pre-compile the bundle to generate bytecode cache
                                                                                                        let build_src_path = extract_dir.join("hot/src");
                                                                                                        let build_pkg_path = extract_dir.join("hot/pkg");
                                                                                                        let mut paths = vec![build_src_path.to_string_lossy().to_string()];
                                                                                                        if build_pkg_path.exists() {
                                                                                                            paths.push(build_pkg_path.to_string_lossy().to_string());
                                                                                                        }

                                                                                                        let bundle_cache_dir = extract_dir.join(".hot").join("cache");

                                                                                                        // Clear any stale embedded cache files from the bundle
                                                                                                        if bundle_cache_dir.exists()
                                                                                                            && let Ok(entries) = std::fs::read_dir(&bundle_cache_dir) {
                                                                                                                for entry in entries.flatten() {
                                                                                                                    let path = entry.path();
                                                                                                                    if path.to_string_lossy().ends_with(".bc.zst") {
                                                                                                                        if let Err(e) = std::fs::remove_file(&path) {
                                                                                                                            tracing::warn!("Failed to remove stale cache file {:?}: {}", path, e);
                                                                                                                        } else {
                                                                                                                            tracing::debug!("Removed stale embedded cache file {:?}", path);
                                                                                                                        }
                                                                                                                    }
                                                                                                                }
                                                                                                            }

                                                                                                        let bundle_cache = hot::lang::cache::bytecode_cache::BytecodeCache::new(bundle_cache_dir);
                                                                                                        info!("Pre-compiling bundle {} to generate bytecode cache", build.build_id);
                                                                                                        if let Err(e) = hot::lang::engine::Engine::compile_to_cache(
                                                                                                            &paths,
                                                                                                            &bundle_cache,
                                                                                                            &proj_name,
                                                                                                            cache_key.as_deref(),
                                                                                                            file_hashes,
                                                                                                            None, // Bundle builds have deps pre-bundled
                                                                                                        ) {
                                                                                                            warn!("Failed to pre-compile bundle {}: {}", build.build_id, e);
                                                                                                        } else {
                                                                                                            info!("Bundle {} pre-compiled successfully", build.build_id);
                                                                                                        }

                                                                                                        // Mark extraction complete AFTER bytecode is ready
                                                                                                        BuildPathCache::mark_extraction_complete(&extract_dir);

                                                                                                        // Store the extracted path in the cache
                                                                                                        build_path_cache_ref.insert(build.build_id, extract_dir);
                                                                                                    }
                                                                                                    Err(e) => {
                                                                                                        let err_msg = format!("Failed to extract build {}: {}", build.build_id, e);
                                                                                                        error!("hot.dev: WORKER {} {}", worker_id, err_msg);
                                                                                                        hot::db::alert::publish_deploy_failed_alert(
                                                                                                            db,
                                                                                                            &env.org_id,
                                                                                                            &project.env_id,
                                                                                                            &build.build_id,
                                                                                                            &project.name,
                                                                                                            &err_msg,
                                                                                                        ).await;
                                                                                                        return Err(Box::new(std::io::Error::other(err_msg)) as Box<dyn std::error::Error + Send + Sync>);
                                                                                                    }
                                                                                                }
                                                                                            }
                                                                                        }

                                                                                        // Load handlers, schedules, and agents from the build
                                                                                        match hot::build::load_build_manifest_data(
                                                                                            db,
                                                                                            &build.build_id,
                                                                                            &project.env_id,
                                                                                            &build_data,
                                                                                        ).await {
                                                                                            Ok(()) => {
                                                                                                debug!("hot.dev: WORKER {} successfully loaded handlers and schedules for build {}",
                                                                                                    worker_id,
                                                                                                    build.build_id);

                                                                                                // Publish deploy:succeeded alert
                                                                                                hot::db::alert::publish_deploy_succeeded_alert(
                                                                                                    db,
                                                                                                    &env.org_id,
                                                                                                    &project.env_id,
                                                                                                    &build.build_id,
                                                                                                    &project.name,
                                                                                                ).await;
                                                                                            }
                                                                                            Err(e) => {
                                                                                                let err_msg = format!("Failed to load handlers and schedules for build {}: {}", build.build_id, e);
                                                                                                error!("hot.dev: WORKER {} {}", worker_id, err_msg);
                                                                                                hot::db::alert::publish_deploy_failed_alert(
                                                                                                    db,
                                                                                                    &env.org_id,
                                                                                                    &project.env_id,
                                                                                                    &build.build_id,
                                                                                                    &project.name,
                                                                                                    &err_msg,
                                                                                                ).await;
                                                                                                return Err(Box::new(std::io::Error::other(err_msg)) as Box<dyn std::error::Error + Send + Sync>);
                                                                                            }
                                                                                        }
                                                                                    }
                                                                                    Err(e) => {
                                                                                        let err_msg = format!("Failed to retrieve build {} from storage: {}", build.build_id, e);
                                                                                        error!("hot.dev: WORKER {} {}", worker_id, err_msg);
                                                                                        hot::db::alert::publish_deploy_failed_alert(
                                                                                            db,
                                                                                            &env.org_id,
                                                                                            &project.env_id,
                                                                                            &build.build_id,
                                                                                            &project.name,
                                                                                            &err_msg,
                                                                                        ).await;
                                                                                        return Err(Box::new(std::io::Error::other(err_msg)) as Box<dyn std::error::Error + Send + Sync>);
                                                                                    }
                                                                                }
                                                                            }
                                                                            Err(e) => {
                                                                                let err_msg = format!("Failed to create build storage: {}", e);
                                                                                error!("hot.dev: WORKER {} {}", worker_id, err_msg);
                                                                                hot::db::alert::publish_deploy_failed_alert(
                                                                                    db,
                                                                                    &env.org_id,
                                                                                    &project.env_id,
                                                                                    &build.build_id,
                                                                                    &project.name,
                                                                                    &err_msg,
                                                                                ).await;
                                                                                return Err(Box::new(std::io::Error::other(err_msg)) as Box<dyn std::error::Error + Send + Sync>);
                                                                            }
                                                                        }
                                                                    }
                                                                    Err(e) => {
                                                                        let err_msg = format!("Failed to get environment for project {}: {}", project.project_id, e);
                                                                        error!("hot.dev: WORKER {} {}", worker_id, err_msg);
                                                                        return Err(Box::new(std::io::Error::other(err_msg)) as Box<dyn std::error::Error + Send + Sync>);
                                                                    }
                                                                }
                                                            }
                                                            Err(e) => {
                                                                let err_msg = format!("Failed to get project for build {}: {}", build.build_id, e);
                                                                error!("hot.dev: WORKER {} {}", worker_id, err_msg);
                                                                return Err(Box::new(std::io::Error::other(err_msg)) as Box<dyn std::error::Error + Send + Sync>);
                                                            }
                                                        }
                                                    }
                                                    Err(e) => {
                                                        let err_msg = format!("Failed to get build {}: {}", deployment_message.body.build_id, e);
                                                        error!("hot.dev: WORKER {} {}", worker_id, err_msg);
                                                        return Err(Box::new(std::io::Error::other(err_msg)) as Box<dyn std::error::Error + Send + Sync>);
                                                    }
                                                }
                                            } else {
                                                let err_msg = "Cannot process deployment: no database connection".to_string();
                                                error!("hot.dev: WORKER {} {}", worker_id, err_msg);
                                                return Err(Box::new(std::io::Error::other(err_msg)) as Box<dyn std::error::Error + Send + Sync>);
                                            }

                                            debug!("hot.dev: WORKER {} processed deployment for build {} successfully",
                                                worker_id,
                                                deployment_message.body.build_id);
                                        },
                                        "MaintenanceMessage" => {
                                            let maint_message: hot::lang::event::MaintenanceMessage = message.try_into()
                                                .map_err(|e| Box::new(std::io::Error::other(e)) as Box<dyn std::error::Error + Send + Sync>)?;

                                            info!("hot.dev: WORKER {} received maintenance task: id={} tasks={:?}",
                                                worker_id,
                                                maint_message.id,
                                                maint_message.body.tasks);

                                            if let Some(ref db) = db_ref {
                                                for task in &maint_message.body.tasks {
                                                    match task.as_str() {
                                                        "session_cleanup" => {
                                                            match hot::db::session::Session::cleanup_expired(db).await {
                                                                Ok(count) => {
                                                                    if count > 0 {
                                                                        info!("hot.dev: WORKER {} maintenance: cleaned up {} expired sessions", worker_id, count);
                                                                    }
                                                                }
                                                                Err(e) => {
                                                                    error!("hot.dev: WORKER {} maintenance: session cleanup failed: {}", worker_id, e);
                                                                }
                                                            }
                                                        }
                                                        "inactive_schedule_cleanup" => {
                                                            match hot::db::Schedule::delete_old_inactive_schedules(db, 30).await {
                                                                Ok(count) => {
                                                                    if count > 0 {
                                                                        info!("hot.dev: WORKER {} maintenance: cleaned up {} inactive schedules", worker_id, count);
                                                                    }
                                                                }
                                                                Err(e) => {
                                                                    error!("hot.dev: WORKER {} maintenance: inactive schedule cleanup failed: {}", worker_id, e);
                                                                }
                                                            }
                                                        }
                                                        "domain_verification" => {
                                                            if !hot::domain::custom_domains_enabled(&worker_conf_ref) {
                                                                tracing::debug!("hot.dev: WORKER {} maintenance: domain verification skipped because domain provisioning is disabled", worker_id);
                                                                continue;
                                                            }

                                                            // Check unverified domains — poll provider certificate status
                                                            match hot::db::domain::Domain::list_unverified(db).await {
                                                                Ok(domains) => {
                                                                    if !domains.is_empty() {
                                                                        info!("hot.dev: WORKER {} maintenance: checking {} unverified domains", worker_id, domains.len());
                                                                    }
                                                                    for domain in &domains {
                                                                        if let Some(arn) = &domain.certificate_ref {
                                                                            match hot::domain_provider::domain_provider().certificate_status(&worker_conf_ref, domain, arn).await {
                                                                                Ok(hot::domain_provider::DomainCertificateStatus::Issued) => {
                                                                                    match hot::db::domain::Domain::mark_verified(db, &domain.domain_id).await {
                                                                                        Ok(()) => {
                                                                                            info!("hot.dev: WORKER {} maintenance: domain '{}' certificate issued, marked verified", worker_id, domain.domain);
                                                                                            let prov_msg: hot::data::msg::Message = hot::lang::event::MaintenanceMessage::single_task("domain_provisioning").into();
                                                                                            if let Err(e) = event_queue_ref.enqueue(prov_msg).await {
                                                                                                error!("hot.dev: WORKER {} maintenance: failed to enqueue domain_provisioning after verification: {}", worker_id, e);
                                                                                            }
                                                                                        }
                                                                                        Err(e) => {
                                                                                            error!("hot.dev: WORKER {} maintenance: failed to mark domain '{}' as verified: {}", worker_id, domain.domain, e);
                                                                                        }
                                                                                    }
                                                                                }
                                                                                Ok(hot::domain_provider::DomainCertificateStatus::Failed(reason)) => {
                                                                                    error!("hot.dev: WORKER {} maintenance: certificate failed for '{}': {}", worker_id, domain.domain, reason);
                                                                                }
                                                                                Ok(_) => {
                                                                                    tracing::debug!("hot.dev: WORKER {} maintenance: domain '{}' certificate validation pending", worker_id, domain.domain);
                                                                                }
                                                                                Err(e) => {
                                                                                    error!("hot.dev: WORKER {} maintenance: certificate status check failed for '{}': {}", worker_id, domain.domain, e);
                                                                                }
                                                                            }
                                                                        } else {
                                                                            tracing::debug!("hot.dev: WORKER {} maintenance: domain '{}' has no certificate, skipping", worker_id, domain.domain);
                                                                        }
                                                                    }
                                                                }
                                                                Err(e) => {
                                                                    error!("hot.dev: WORKER {} maintenance: domain verification query failed: {}", worker_id, e);
                                                                }
                                                            }
                                                        }
                                                        "domain_provisioning" => {
                                                            if !hot::domain::custom_domains_enabled(&worker_conf_ref) {
                                                                tracing::debug!("hot.dev: WORKER {} maintenance: domain provisioning skipped because domain provisioning is disabled", worker_id);
                                                                continue;
                                                            }

                                                            // Step 0: Request certificates for domains that don't have one yet
                                                            match hot::db::domain::Domain::list_pending_certificate(db).await {
                                                                Ok(domains) => {
                                                                    for domain in &domains {
                                                                        info!("hot.dev: WORKER {} provisioning: requesting certificate for '{}'", worker_id, domain.domain);
                                                                        if let Err(e) = hot::domain_provider::domain_provider().request_certificate(&worker_conf_ref, db, domain).await {
                                                                            error!("hot.dev: WORKER {} provisioning: certificate request failed for '{}': {}", worker_id, domain.domain, e);
                                                                        }
                                                                    }
                                                                }
                                                                Err(e) => {
                                                                    error!("hot.dev: WORKER {} provisioning: list_pending_certificate failed: {}", worker_id, e);
                                                                }
                                                            }

                                                            // Step 1: Create provider distributions for verified domains without one
                                                            match hot::db::domain::Domain::list_pending_routing(db).await {
                                                                Ok(domains) => {
                                                                    for domain in &domains {
                                                                        if let Some(arn) = &domain.certificate_ref {
                                                                            info!("hot.dev: WORKER {} provisioning: creating distribution for '{}'", worker_id, domain.domain);
                                                                            match hot::domain_provider::domain_provider().create_distribution(&worker_conf_ref, db, domain, arn).await {
                                                                                Ok(()) => {
                                                                                    let _ = hot::db::domain::Domain::clear_provisioning_error(db, &domain.domain_id).await;
                                                                                }
                                                                                Err(e) if e.cname_already_exists => {
                                                                                    error!("hot.dev: WORKER {} provisioning: domain '{}' has a DNS CNAME that already points to another distribution. The user must remove the stale DNS record before this domain can be provisioned.", worker_id, domain.domain);
                                                                                    let user_msg = format!(
                                                                                        "Your domain '{}' has a DNS CNAME record that already points to another distribution. Please remove or update the existing CNAME record for this domain in your DNS provider, then click Check Status to retry.",
                                                                                        domain.domain
                                                                                    );
                                                                                    if let Err(err) = hot::db::domain::Domain::set_provisioning_error(db, &domain.domain_id, &user_msg).await {
                                                                                        error!("hot.dev: WORKER {} provisioning: failed to save provisioning error for '{}': {}", worker_id, domain.domain, err);
                                                                                    }
                                                                                }
                                                                                Err(e) => {
                                                                                    error!("hot.dev: WORKER {} provisioning: distribution creation failed for '{}': {}", worker_id, domain.domain, e);
                                                                                }
                                                                            }
                                                                        }
                                                                    }
                                                                }
                                                                Err(e) => {
                                                                    error!("hot.dev: WORKER {} provisioning: list_pending_routing failed: {}", worker_id, e);
                                                                }
                                                            }

                                                            // Step 2: Check deploying distributions
                                                            match hot::db::domain::Domain::list_deploying(db).await {
                                                                Ok(domains) => {
                                                                    for domain in &domains {
                                                                        if let Some(dist_id) = &domain.routing_ref {
                                                                            match hot::domain_provider::domain_provider().distribution_status(&worker_conf_ref, dist_id).await {
                                                                                Ok(hot::domain_provider::DomainDistributionStatus::Deployed) => {
                                                                                    match hot::db::domain::Domain::mark_tls_provisioned(db, &domain.domain_id).await {
                                                                                        Ok(()) => {
                                                                                            info!("hot.dev: WORKER {} provisioning: distribution deployed for '{}', domain is now active", worker_id, domain.domain);
                                                                                        }
                                                                                        Err(e) => {
                                                                                            error!("hot.dev: WORKER {} provisioning: failed to mark TLS provisioned for '{}': {}", worker_id, domain.domain, e);
                                                                                        }
                                                                                    }
                                                                                }
                                                                                Ok(hot::domain_provider::DomainDistributionStatus::InProgress) => {
                                                                                    tracing::debug!("hot.dev: WORKER {} provisioning: distribution still deploying for '{}'", worker_id, domain.domain);
                                                                                }
                                                                                Ok(hot::domain_provider::DomainDistributionStatus::Unknown(s)) => {
                                                                                    warn!("hot.dev: WORKER {} provisioning: distribution unknown status '{}' for '{}'", worker_id, s, domain.domain);
                                                                                }
                                                                                Err(e) => {
                                                                                    error!("hot.dev: WORKER {} provisioning: distribution status check failed for '{}': {}", worker_id, domain.domain, e);
                                                                                }
                                                                            }
                                                                        }
                                                                    }
                                                                }
                                                                Err(e) => {
                                                                    error!("hot.dev: WORKER {} provisioning: list_deploying failed: {}", worker_id, e);
                                                                }
                                                            }
                                                        }
                                                        "domain_cleanup" => {
                                                            // Clean up provider resources for soft-deleted domains, then hard-delete.
                                                            match hot::db::domain::Domain::list_pending_deletion(db).await {
                                                                Ok(domains) => {
                                                                    for domain in &domains {
                                                                        info!("hot.dev: WORKER {} cleanup: processing deletion for '{}'", worker_id, domain.domain);

                                                                        if let Err(e) = hot::domain_provider::domain_provider().cleanup_domain(&worker_conf_ref, domain).await {
                                                                            error!("hot.dev: WORKER {} cleanup: provider cleanup failed for '{}': {}", worker_id, domain.domain, e);
                                                                            continue;
                                                                        }

                                                                        // Step 2: Hard-delete the DB record
                                                                        match hot::db::domain::Domain::hard_delete(db, &domain.domain_id).await {
                                                                            Ok(()) => {
                                                                                info!("hot.dev: WORKER {} cleanup: domain '{}' fully removed", worker_id, domain.domain);
                                                                            }
                                                                            Err(e) => {
                                                                                error!("hot.dev: WORKER {} cleanup: hard_delete failed for '{}': {}", worker_id, domain.domain, e);
                                                                            }
                                                                        }
                                                                    }
                                                                }
                                                                Err(e) => {
                                                                    error!("hot.dev: WORKER {} cleanup: list_pending_deletion failed: {}", worker_id, e);
                                                                }
                                                            }
                                                        }
                                                        "upload_cleanup" => {
                                                            match hot::db::file_upload::cleanup_expired_uploads(db).await {
                                                                Ok(expired) => {
                                                                    if !expired.is_empty() {
                                                                        info!("hot.dev: WORKER {} upload_cleanup: aborting {} expired uploads", worker_id, expired.len());
                                                                        for upload in &expired {
                                                                            if let Err(e) = hot::db::file_upload::abort_upload(db, upload.upload_id, upload.org_id).await {
                                                                                error!("hot.dev: WORKER {} upload_cleanup: failed to abort upload {}: {}", worker_id, upload.upload_id, e);
                                                                            }
                                                                        }
                                                                    }
                                                                }
                                                                Err(e) => {
                                                                    error!("hot.dev: WORKER {} upload_cleanup: failed: {}", worker_id, e);
                                                                }
                                                            }
                                                        }
                                                        "queue_cleanup" => {
                                                            use hot::queue::StreamCleanup;
                                                            // 5 of the 6 production queues. The per-process janitor
                                                            // (server.rs ~3653) handles this every 5min too, but
                                                            // running it from the daily maintenance path is cheap
                                                            // defense-in-depth and survives a janitor that's stuck
                                                            // or crashed. hot:task is intentionally excluded —
                                                            // hot_task_worker owns it and runs its own janitor under
                                                            // the correct consumer identity.
                                                            for (qname, result) in [
                                                                ("hot:event", event_queue_ref.cleanup_streams().await),
                                                                ("hot:request", request_queue_ref.cleanup_streams().await),
                                                                ("hot:response", response_queue_ref.cleanup_streams().await),
                                                                ("hot:alert", alert_queue_ref.cleanup_streams().await),
                                                                ("hot:email", email_queue_ref.cleanup_streams().await),
                                                            ] {
                                                                match result {
                                                                    Ok((consumers, trimmed)) => {
                                                                        if consumers > 0 || trimmed > 0 {
                                                                            info!(
                                                                                "hot.dev: WORKER {} maintenance: {} cleanup removed {} stale consumers, trimmed {} entries",
                                                                                worker_id, qname, consumers, trimmed
                                                                            );
                                                                        }
                                                                    }
                                                                    Err(e) => {
                                                                        error!("hot.dev: WORKER {} maintenance: {} cleanup failed: {}", worker_id, qname, e);
                                                                    }
                                                                }
                                                            }
                                                        }
                                                        "zombie_run_cleanup" => {
                                                            // Reap runs left in `running` for longer than any
                                                            // possible legitimate execution. Max code-task timeout
                                                            // is 24h (`MAX_TIMEOUT_MS` in lang/hot/task.rs); event
                                                            // handlers are capped at 5 minutes. A 25h cutoff is
                                                            // safely past both. We process in chunks so a large
                                                            // backlog doesn't pin the maintenance worker.
                                                            let cutoff = chrono::Utc::now() - chrono::Duration::hours(25);
                                                            const ZOMBIE_BATCH_LIMIT: i64 = 5_000;
                                                            match hot::db::Run::fail_stale_runs(
                                                                db,
                                                                cutoff,
                                                                "Run abandoned: still in 'running' state past max timeout (likely worker crash)",
                                                                ZOMBIE_BATCH_LIMIT,
                                                            )
                                                            .await
                                                            {
                                                                Ok(n) if n > 0 => {
                                                                    info!(
                                                                        "hot.dev: WORKER {} maintenance: zombie_run_cleanup reaped {} stale run(s) (cutoff={})",
                                                                        worker_id, n, cutoff
                                                                    );
                                                                }
                                                                Ok(_) => {}
                                                                Err(e) => {
                                                                    error!("hot.dev: WORKER {} maintenance: zombie_run_cleanup failed: {}", worker_id, e);
                                                                }
                                                            }
                                                        }
                                                        "call_retention_cleanup" => {
                                                            if hot::env::is_local_dev() {
                                                                let days = hot::db::get_local_call_retention_days(&worker_conf_ref);
                                                                if days >= 0 {
                                                                    match hot::db::call::Call::delete_older_than(db, days, 10_000).await {
                                                                        Ok(count) if count > 0 => {
                                                                            info!("hot.dev: WORKER {} maintenance: call retention cleanup deleted {} rows (>{} days)", worker_id, count, days);
                                                                        }
                                                                        Ok(_) => {}
                                                                        Err(e) => {
                                                                            error!("hot.dev: WORKER {} maintenance: call retention cleanup failed: {}", worker_id, e);
                                                                        }
                                                                    }
                                                                }
                                                            } else {
                                                                let org_ids = hot::db::subscription::OrgPlan::get_all_active_org_ids(db).await.unwrap_or_default();
                                                                for org_id in &org_ids {
                                                                    let days = hot::db::subscription::call_deletion_days_for_org(db, org_id).await;
                                                                    if days < 0 { continue; }

                                                                    let env_ids = hot::db::Env::get_ids_for_org(db, org_id)
                                                                        .await
                                                                        .unwrap_or_default();

                                                                    match hot::db::call::Call::delete_older_than_for_org(db, &env_ids, days, 10_000).await {
                                                                        Ok(count) if count > 0 => {
                                                                            info!("hot.dev: WORKER {} maintenance: call retention cleanup for org {} deleted {} rows (>{} days)", worker_id, org_id, count, days);
                                                                        }
                                                                        Ok(_) => {}
                                                                        Err(e) => {
                                                                            error!("hot.dev: WORKER {} maintenance: call retention cleanup for org {} failed: {}", worker_id, org_id, e);
                                                                        }
                                                                    }
                                                                }
                                                            }
                                                        }
                                                        other => {
                                                            warn!("hot.dev: WORKER {} maintenance: unknown task '{}'", worker_id, other);
                                                        }
                                                    }
                                                }
                                            } else {
                                                error!("hot.dev: WORKER {} cannot process maintenance: no database connection", worker_id);
                                            }
                                        },
                                        _ => {
                                            info!("hot.dev: WORKER {} received unknown message type '{}' on event queue", worker_id, msg_type);
                                        }
                                    }

                                    // Return some value as the processing result
                                    Ok(()) as Result<(), Box<dyn std::error::Error + Send + Sync>>
                                }
                            }).await {
                                Ok(Some(_)) => {
                                },
                                Ok(None) => {
                                    // No events in queue - continue to next iteration
                                },
                                Err(e) => {
                                    debug!("hot.dev: WORKER {} error processing event: {}", worker_id, e);
                                }
                            }
                        }
                        ClaimedWorkerQueue::Alert(lease) => {
                            // Process ONE alert delivery from hot:alert queue (if any)
                            // This allows main workers to help with alert processing when idle
                            if let Some(ref db_ref) = db_clone {
                                match lease.process(|message| {
                            let db = db_ref.clone();
                            let worker_conf = worker_conf_clone.clone();
                            async move {
                                let alert_msg: hot::lang::event::queue::AlertDeliveryMessage = message.try_into()
                                    .map_err(|e: String| Box::new(std::io::Error::other(e)) as Box<dyn std::error::Error + Send + Sync>)?;

                                tracing::debug!(
                                    "hot.dev: WORKER {} processing alert delivery {} from hot:alert queue",
                                    worker_id,
                                    alert_msg.body.alert_delivery_id
                                );

                                let http_client = reqwest::Client::builder()
                                    .timeout(std::time::Duration::from_secs(10))
                                    .build()
                                    .map_err(|e| Box::new(std::io::Error::other(e.to_string())) as Box<dyn std::error::Error + Send + Sync>)?;

                                let alert_email_sender = hot::email::EmailSender::alerts_from_conf(&worker_conf);
                                let alert_email_config = hot::email::EmailConfig::alerts_from_conf(&worker_conf);
                                let email_sender_ref: Option<&dyn hot::db::alert::AlertEmailSender> = if alert_email_sender.is_available() {
                                    Some(&alert_email_sender)
                                } else {
                                    None
                                };

                                match hot::db::alert::process_single_alert_delivery(
                                    &db,
                                    &http_client,
                                    email_sender_ref,
                                    &alert_email_config,
                                    &alert_msg.body.alert_delivery_id,
                                ).await {
                                    Ok(success) => {
                                        if success {
                                            tracing::info!(
                                                "Alert delivery {} sent successfully",
                                                alert_msg.body.alert_delivery_id
                                            );
                                        } else {
                                            tracing::warn!(
                                                "Alert delivery {} failed (DB retry state owns redelivery if attempts remain)",
                                                alert_msg.body.alert_delivery_id
                                            );
                                        }
                                        Ok(()) as Result<(), Box<dyn std::error::Error + Send + Sync>>
                                    }
                                    Err(e) => {
                                        tracing::error!(
                                            "Alert delivery {} processing error: {}",
                                            alert_msg.body.alert_delivery_id,
                                            e
                                        );
                                        Err(Box::new(std::io::Error::other(e.to_string()))
                                            as Box<dyn std::error::Error + Send + Sync>)
                                    }
                                }
                            }
                        }).await {
                            Ok(Some(_)) => {},
                            Ok(None) => { /* no alerts in queue */ },
                            Err(e) => {
                                tracing::debug!("hot.dev: WORKER {} alert queue error: {}", worker_id, e);
                            }
                        }
                            }
                        }
                        ClaimedWorkerQueue::Email(lease) => {
                            // Process ONE app email from hot:email queue (if any)
                            if let Some(ref db_ref) = db_clone {
                                match lease.process(|message| {
                            let db = db_ref.clone();
                            let worker_conf = worker_conf_clone.clone();
                            async move {
                                let email_msg: hot::lang::event::queue::EmailMessage = message.try_into()
                                    .map_err(|e: String| Box::new(std::io::Error::other(e)) as Box<dyn std::error::Error + Send + Sync>)?;

                                tracing::debug!(
                                    "hot.dev: WORKER {} processing app email to {} from hot:email queue",
                                    worker_id,
                                    email_msg.body.to_address
                                );

                                let sender = hot::email::EmailSender::from_conf(&worker_conf);
                                if !sender.is_available() {
                                    let _ = hot::db::email_queue::EmailQueueEntry::mark_failed(
                                        &db,
                                        &email_msg.body.email_queue_id,
                                        "Email sender not configured",
                                    ).await;
                                    return Ok(());
                                }

                                let email = hot::email::Email {
                                    to: email_msg.body.to_address.clone(),
                                    subject: email_msg.body.subject.clone(),
                                    html: email_msg.body.html_body.clone(),
                                    text: email_msg.body.text_body.clone(),
                                };

                                match sender.send_email_with_from(&email, &email_msg.body.from_address).await {
                                    Ok(()) => {
                                        let _ = hot::db::email_queue::EmailQueueEntry::mark_sent(
                                            &db,
                                            &email_msg.body.email_queue_id,
                                        ).await;
                                    }
                                    Err(e) => {
                                        tracing::warn!(
                                            "hot.dev: WORKER {} failed to send app email {} (to: {}), will retry via queue: {}",
                                            worker_id,
                                            email_msg.body.email_queue_id,
                                            email_msg.body.to_address,
                                            e
                                        );
                                        // Add a small interval before requeue retry to avoid hot-looping.
                                        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                                        return Err(
                                            Box::new(std::io::Error::other(format!(
                                                "app email send failed for {}: {}",
                                                email_msg.body.email_queue_id, e
                                            ))) as Box<dyn std::error::Error + Send + Sync>
                                        );
                                    }
                                }

                                Ok(()) as Result<(), Box<dyn std::error::Error + Send + Sync>>
                            }
                        }).await {
                            Ok(Some(_)) => {},
                            Ok(None) => { /* no emails in queue */ },
                            Err(e) => {
                                tracing::debug!("hot.dev: WORKER {} email queue error: {}", worker_id, e);
                            }
                        }
                            }
                        }
                    }
                }

                Ok(())
            },
        );

        worker_handles.push(handle);
    }

    // Wait for SIGINT or SIGTERM
    hot::signal::shutdown_signal().await;
    info!("hot.dev: WORKER received shutdown signal");

    // Phase 1: Initiate graceful shutdown (sets flag to stop dequeueing new items)
    if let Err(e) = shutdown_coordinator
        .initiate_shutdown(db.as_ref().map(|d| d.as_ref()))
        .await
    {
        error!("hot.dev: WORKER graceful shutdown error: {}", e);
    }
    debug!(
        "hot.dev: WORKER graceful shutdown flag set: {}",
        shutdown_coordinator.is_shutting_down()
    );

    // Phase 2: Signal all workers to shutdown and wait for them to complete
    let _ = shutdown_tx.send(true);

    let worker_shutdown_timeout =
        tokio::time::Duration::from_secs(if hot::env::is_local_dev() { 0 } else { 30 });
    info!(
        "hot.dev: WORKER waiting for workers to complete (timeout: {}s)",
        worker_shutdown_timeout.as_secs()
    );
    let workers_drained =
        match tokio::time::timeout(worker_shutdown_timeout, join_all(&mut worker_handles)).await {
            Ok(results) => {
                info!("hot.dev: WORKER all workers completed gracefully");
                for (i, result) in results.into_iter().enumerate() {
                    if let Err(e) = result {
                        error!("hot.dev: WORKER {} task error: {}", i, e);
                    }
                }
                true
            }
            Err(_) => {
                warn!(
                    "hot.dev: WORKER shutdown timeout ({}s), aborting remaining worker tasks",
                    worker_shutdown_timeout.as_secs()
                );
                for handle in &worker_handles {
                    handle.abort();
                }
                tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
                drop(worker_handles);
                false
            }
        };

    if workers_drained {
        use hot::queue::ConsumerLifecycle;
        for queue in [
            &event_queue
                .clone()
                .with_consumer_name(admin_event_name.clone()) as &dyn ConsumerLifecycle,
            &request_queue
                .clone()
                .with_consumer_name(admin_request_name.clone()),
        ] {
            if let Err(e) = queue.unregister_consumer().await {
                warn!("hot.dev: WORKER failed to unregister consumer: {}", e);
            }
        }
        if db.is_some() {
            for queue in [
                &alert_queue
                    .clone()
                    .with_consumer_name(admin_alert_name.clone())
                    as &dyn ConsumerLifecycle,
                &email_queue
                    .clone()
                    .with_consumer_name(admin_email_name.clone()),
            ] {
                if let Err(e) = queue.unregister_consumer().await {
                    warn!("hot.dev: WORKER failed to unregister consumer: {}", e);
                }
            }
        }
    }

    // Phase 3: Flush emitter/publisher after workers have stopped
    if let Some(emitter) = &emitter {
        debug!("hot.dev: WORKER shutting down emitter to flush remaining events");
        if let Err(e) = emitter.shutdown().await {
            error!("hot.dev: WORKER emitter shutdown error: {}", e);
        } else {
            debug!("hot.dev: WORKER emitter shutdown complete");
        }
    }

    if let Some(publisher) = &event_publisher {
        debug!("hot.dev: WORKER shutting down event publisher");
        if let Err(e) = publisher.shutdown().await {
            error!("hot.dev: WORKER event publisher shutdown error: {}", e);
        } else {
            debug!("hot.dev: WORKER event publisher shutdown complete");
        }
    }

    info!("hot.dev: WORKER shutdown complete");

    Ok(())
}

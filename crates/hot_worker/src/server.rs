use ahash::AHashMap;
use futures::future::join_all;
use hot::data::msg::Message;
use hot::data::serialization::Serialization;
use hot::lang::event::EventMessage;
use hot::queue::{
    ConsumerLifecycle, ProcessingQueue, ProcessingQueueLease, Queue, QueueInfrastructureError,
    QueueType, RecoveredQueueMessage, streams::PRESERVE_DELIVERY_COUNT_TOKEN,
};
use hot::val;
use hot::val::Val;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::{Arc, OnceLock, RwLock};
use tokio::sync::{Mutex, Semaphore, mpsc, watch};
use tokio::task::JoinHandle;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

/// Context from `hot/ctx.hot` for dev mode. Keys set there with `set-secret`
/// are listed under [`hot::lang::hot::ctx::LOADED_SECRET_KEYS`]; runs mask
/// their values.
pub type DevContextStorage = Arc<RwLock<Option<AHashMap<String, hot::val::Val>>>>;

/// Copy `dev_ctx` into a run's context `storage`, and record the hashes of the
/// values `hot/ctx.hot` set with `set-secret`, so call records and logs mask
/// them as they mask declared secrets.
fn merge_dev_context(
    dev_ctx: &AHashMap<String, Val>,
    storage: &mut AHashMap<String, Val>,
    secret_value_hashes: &mut ahash::AHashSet<u64>,
) {
    let secret_keys = dev_ctx.get(hot::lang::hot::ctx::LOADED_SECRET_KEYS);
    for (key, val) in dev_ctx.iter() {
        if key == hot::lang::hot::ctx::LOADED_SECRET_KEYS {
            continue;
        }
        storage.insert(key.clone(), val.clone());
    }
    if let Some(Val::Vec(secret_keys)) = secret_keys {
        for key in secret_keys.iter() {
            if let Val::Str(key) = key
                && let Some(value) = dev_ctx.get(&**key)
            {
                hot::lang::hot::ctx::hash_secret_value_recursive(value, secret_value_hashes);
            }
        }
    }
}
type WorkerError = Box<dyn std::error::Error + Send + Sync>;

enum ClaimedWorkerQueue {
    Request(ProcessingQueueLease<Message>),
    Event(ProcessingQueueLease<Message>),
    Alert(ProcessingQueueLease<Message>),
    Email(ProcessingQueueLease<Message>),
}

const WORKER_SHUTDOWN_DRAIN_SECONDS: u64 = 30;
const LOCAL_DEV_WORKER_SHUTDOWN_DRAIN_SECONDS: u64 = 5;
const WORKER_HANDOFF_WAIT_LOG_MS: u64 = 100;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct TenantStoreKey {
    org_id: Uuid,
    env_id: Uuid,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct TenantContextKey {
    org_id: Uuid,
    env_id: Uuid,
    project_id: Uuid,
}

#[derive(Clone)]
struct CachedTenantContext {
    values: AHashMap<String, Val>,
    loaded_at: std::time::Instant,
}

#[derive(Clone)]
struct CachedTenantStore {
    store: Arc<dyn hot::store::Store>,
    last_used: std::time::Instant,
}

/// Worker-lifetime clients and strictly tenant-keyed execution state.
///
/// File storage is safe to share because every operation requires a
/// `FileStorageContext` and both local/S3 paths include org_id + env_id.
/// Stores embed tenant identity, so they are cached under that exact UUID
/// scope. Decrypted context is even narrower and also includes project_id.
struct WorkerExecutionServices {
    db: Arc<DatabasePool>,
    file_storage: Option<Arc<dyn hot::file_storage::FileStorage>>,
    embedding_provider: Option<Arc<dyn hot::store::embedding::EmbeddingProvider>>,
    stores: Mutex<AHashMap<TenantStoreKey, CachedTenantStore>>,
    contexts: Mutex<AHashMap<TenantContextKey, CachedTenantContext>>,
    context_ttl: std::time::Duration,
    cache_max_entries: usize,
}

impl WorkerExecutionServices {
    async fn new(db: Arc<DatabasePool>, conf: &Val) -> Self {
        let file_storage = match hot::file_storage::file_storage_from_config(conf).await {
            Ok(storage) => Some(Arc::from(storage)),
            Err(e) => {
                tracing::debug!("File storage not available for worker: {}", e);
                None
            }
        };
        let embedding_provider =
            hot::store::embedding::embedding_provider_from_config(conf).map(Arc::from);
        let context_ttl_ms = conf
            .get_int_or_default("worker.context-cache-ttl-ms", 1_000)
            .max(0) as u64;
        let cache_max_entries = conf
            .get_int_or_default("worker.tenant-service-cache-max-entries", 1_024)
            .max(0) as usize;

        Self {
            db,
            file_storage,
            embedding_provider,
            stores: Mutex::new(AHashMap::new()),
            contexts: Mutex::new(AHashMap::new()),
            context_ttl: std::time::Duration::from_millis(context_ttl_ms),
            cache_max_entries,
        }
    }

    async fn store_for(
        &self,
        conf: &Val,
        org_id: Uuid,
        env_id: Uuid,
    ) -> Option<Arc<dyn hot::store::Store>> {
        let key = TenantStoreKey { org_id, env_id };
        {
            let mut stores = self.stores.lock().await;
            if let Some(cached) = stores.get_mut(&key) {
                cached.last_used = std::time::Instant::now();
                return Some(Arc::clone(&cached.store));
            }
        }

        let store = match hot::store::store_from_config_with_db(
            conf,
            Some(Arc::new(
                self.db
                    .with_scope(DatabaseScope::Environment { org_id, env_id }),
            )),
            Some(org_id),
            Some(env_id),
        )
        .await
        {
            Ok(store) => Arc::from(store),
            Err(e) => {
                tracing::debug!(
                    org_id = %org_id,
                    env_id = %env_id,
                    "Store not available for tenant-scoped event handler: {}",
                    e
                );
                return None;
            }
        };

        // A concurrent miss may have initialized the same tenant. Reuse the
        // first inserted instance; UUID-keyed scopes can never cross tenants.
        if self.cache_max_entries == 0 {
            return Some(store);
        }

        let mut stores = self.stores.lock().await;
        if let Some(cached) = stores.get_mut(&key) {
            cached.last_used = std::time::Instant::now();
            return Some(Arc::clone(&cached.store));
        }
        Self::evict_oldest_store_if_full(&mut stores, self.cache_max_entries);
        stores.insert(
            key,
            CachedTenantStore {
                store: Arc::clone(&store),
                last_used: std::time::Instant::now(),
            },
        );
        Some(store)
    }

    async fn context_for(
        &self,
        encryption: &ContextEncryption,
        key: TenantContextKey,
    ) -> AHashMap<String, Val> {
        {
            let mut contexts = self.contexts.lock().await;
            if self.context_ttl.is_zero() {
                // Drop decrypted values immediately when caching is disabled.
                contexts.clear();
            } else {
                Self::purge_expired_contexts(&mut contexts, self.context_ttl);
                if let Some(cached) = contexts.get(&key) {
                    return cached.values.clone();
                }
            }
        }

        let db = self.db.with_scope(DatabaseScope::Environment {
            org_id: key.org_id,
            env_id: key.env_id,
        });
        let mut values = AHashMap::new();
        let mut cacheable = true;

        match Context::get_by_env(&db, &key.env_id).await {
            Ok(context_vars) => {
                for context_var in context_vars.into_iter().filter(|value| value.active) {
                    match context_var.get_decrypted_value(encryption, &key.org_id) {
                        Ok(value) => {
                            values.insert(context_var.key, value);
                        }
                        Err(e) => {
                            cacheable = false;
                            tracing::warn!(
                                org_id = %key.org_id,
                                env_id = %key.env_id,
                                "Failed to decrypt environment context variable '{}': {}",
                                context_var.key,
                                e
                            );
                        }
                    }
                }
            }
            Err(e) => {
                cacheable = false;
                tracing::warn!(
                    org_id = %key.org_id,
                    env_id = %key.env_id,
                    "Failed to load environment context variables: {}",
                    e
                );
            }
        }

        match Context::get_by_project(&db, &key.project_id).await {
            Ok(context_vars) => {
                for context_var in context_vars.into_iter().filter(|value| value.active) {
                    match context_var.get_decrypted_value(encryption, &key.org_id) {
                        Ok(value) => {
                            // Project context intentionally overrides env context.
                            values.insert(context_var.key, value);
                        }
                        Err(e) => {
                            cacheable = false;
                            tracing::warn!(
                                org_id = %key.org_id,
                                env_id = %key.env_id,
                                project_id = %key.project_id,
                                "Failed to decrypt project context variable '{}': {}",
                                context_var.key,
                                e
                            );
                        }
                    }
                }
            }
            Err(e) => {
                cacheable = false;
                tracing::warn!(
                    org_id = %key.org_id,
                    env_id = %key.env_id,
                    project_id = %key.project_id,
                    "Failed to load project context variables: {}",
                    e
                );
            }
        }

        if cacheable && !self.context_ttl.is_zero() && self.cache_max_entries > 0 {
            let mut contexts = self.contexts.lock().await;
            Self::purge_expired_contexts(&mut contexts, self.context_ttl);
            Self::evict_oldest_context_if_full(&mut contexts, self.cache_max_entries);
            contexts.insert(
                key,
                CachedTenantContext {
                    values: values.clone(),
                    loaded_at: std::time::Instant::now(),
                },
            );
        }

        values
    }

    fn evict_oldest_store_if_full(
        stores: &mut AHashMap<TenantStoreKey, CachedTenantStore>,
        max_entries: usize,
    ) {
        if stores.len() < max_entries {
            return;
        }
        if let Some(oldest_key) = stores
            .iter()
            .min_by_key(|(_, value)| value.last_used)
            .map(|(key, _)| *key)
        {
            stores.remove(&oldest_key);
        }
    }

    fn evict_oldest_context_if_full(
        contexts: &mut AHashMap<TenantContextKey, CachedTenantContext>,
        max_entries: usize,
    ) {
        if contexts.len() < max_entries {
            return;
        }
        if let Some(oldest_key) = contexts
            .iter()
            .min_by_key(|(_, value)| value.loaded_at)
            .map(|(key, _)| *key)
        {
            contexts.remove(&oldest_key);
        }
    }

    fn purge_expired_contexts(
        contexts: &mut AHashMap<TenantContextKey, CachedTenantContext>,
        ttl: std::time::Duration,
    ) {
        contexts.retain(|_, value| value.loaded_at.elapsed() < ttl);
    }
}

fn worker_queue_claimer_count(include_notifications: bool) -> usize {
    if include_notifications { 4 } else { 2 }
}

fn worker_handoff_capacity(worker_count: usize) -> usize {
    worker_count.max(1)
}

fn blocking_execution_capacity(worker_count: usize, configured_max: i64) -> usize {
    let workers = worker_count.max(1);
    let capacity = if configured_max < 0 {
        workers.saturating_mul(2)
    } else {
        usize::try_from(configured_max)
            .unwrap_or(usize::MAX)
            .max(workers)
    };
    // Semaphore::new panics above MAX_PERMITS, so an oversized configured
    // value must degrade to the largest legal ceiling instead of crashing
    // the worker at startup.
    capacity.min(Semaphore::MAX_PERMITS)
}

fn duration_ms(duration: std::time::Duration) -> u64 {
    duration.as_millis().min(u128::from(u64::MAX)) as u64
}

fn worker_shutdown_drain_timeout(is_local_dev: bool) -> tokio::time::Duration {
    tokio::time::Duration::from_secs(if is_local_dev {
        LOCAL_DEV_WORKER_SHUTDOWN_DRAIN_SECONDS
    } else {
        WORKER_SHUTDOWN_DRAIN_SECONDS
    })
}

fn worker_error(message: impl Into<String>) -> WorkerError {
    Box::new(std::io::Error::other(message.into()))
}

fn worker_infrastructure_retry_error(message: impl Into<String>, backoff_ms: u64) -> WorkerError {
    Box::new(QueueInfrastructureError::new(
        message,
        std::time::Duration::from_millis(backoff_ms),
    ))
}

/// Infrastructure-retry error for setup/admission-timeout redeliveries,
/// which opt IN to cumulative delivery counting.
///
/// These redeliveries flow through `requeue_msg_for_infrastructure_retry`,
/// which XADDs a FRESH stream entry and thereby resets the Redis delivery
/// counter — so without the opt-in a deterministically failing setup (e.g.
/// a bundle that can never finish downloading) would requeue forever. The
/// token makes the queue carry the cumulative count across requeues so
/// repeat offenders eventually reach the DLQ.
///
/// Only the `[event-setup-timeout]` and `[blocking-slot-admission-timeout]`
/// redeliveries may use this helper. Every other infrastructure retry —
/// especially the healthy running-task redelivery deferrals, which
/// legitimately requeue dozens of times while an hours-long task executes —
/// must keep the default reset behavior via
/// `worker_infrastructure_retry_error`.
fn worker_counted_redelivery_error(message: impl Into<String>, backoff_ms: u64) -> WorkerError {
    worker_infrastructure_retry_error(
        format!("{} {}", message.into(), PRESERVE_DELIVERY_COUNT_TOKEN),
        backoff_ms,
    )
}

fn validate_worker_semantics_conf(conf: &Val) -> Result<(), WorkerError> {
    let event_ordering = conf.get_str_or_default("worker.event-ordering", "current");
    if event_ordering != "current" {
        return Err(worker_error(format!(
            "Unsupported worker.event-ordering '{}'; only 'current' is implemented",
            event_ordering
        )));
    }

    let handler_concurrency = conf.get_str_or_default("worker.handler-concurrency", "serial");
    if handler_concurrency != "serial" {
        return Err(worker_error(format!(
            "Unsupported worker.handler-concurrency '{}'; only 'serial' is implemented",
            handler_concurrency
        )));
    }

    Ok(())
}

fn alert_http_client() -> reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            hot::outbound::client_builder()
                .timeout(std::time::Duration::from_secs(10))
                .build()
                .expect("static alert HTTP client configuration should be valid")
        })
        .clone()
}

async fn unregister_idle_consumer(queue_name: &str, queue: &ProcessingQueue<Message>) {
    match queue.consumer_has_pending().await {
        Ok(false) => {
            if let Err(e) = queue.unregister_consumer().await {
                warn!(
                    "hot.dev: WORKER failed to unregister {} consumer: {}",
                    queue_name, e
                );
            }
        }
        Ok(true) => {
            warn!(
                "hot.dev: WORKER leaving {} consumer registered because it still has pending messages",
                queue_name
            );
        }
        Err(e) => {
            warn!(
                "hot.dev: WORKER could not inspect {} consumer pending state before unregister: {}",
                queue_name, e
            );
        }
    }
}

fn spawn_worker_queue_claimer(
    queue_name: &'static str,
    queue: ProcessingQueue<Message>,
    claimed_tx: mpsc::Sender<ClaimedWorkerQueue>,
    mut shutdown_rx: watch::Receiver<bool>,
    wrap: fn(ProcessingQueueLease<Message>) -> ClaimedWorkerQueue,
    queue_metrics_enabled: bool,
) -> JoinHandle<Result<(), WorkerError>> {
    tokio::spawn(async move {
        debug!("hot.dev: WORKER {} claimer started", queue_name);

        loop {
            if *shutdown_rx.borrow() {
                break;
            }

            let lease = match &queue {
                ProcessingQueue::Memory(_) | ProcessingQueue::Sqlite(_) => {
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
                ProcessingQueue::Redis(_) => {
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
            };

            let handoff_started = std::time::Instant::now();
            tokio::select! {
                biased;

                changed = shutdown_rx.changed() => {
                    if changed.is_err() || *shutdown_rx.borrow() {
                        debug!(
                            "hot.dev: WORKER {} claimer dropping claimed lease during shutdown",
                            queue_name
                        );
                        break;
                    }
                    continue;
                }
                send_result = claimed_tx.send(wrap(lease)) => {
                    let handoff_wait_ms = duration_ms(handoff_started.elapsed());
                    if queue_metrics_enabled && handoff_wait_ms >= WORKER_HANDOFF_WAIT_LOG_MS {
                        tracing::info!(
                            target: "hot::queue::handoff",
                            queue = queue_name,
                            handoff_wait_ms = handoff_wait_ms,
                            threshold_ms = WORKER_HANDOFF_WAIT_LOG_MS,
                            "worker queue claimer handoff waited"
                        );
                    }
                    if send_result.is_err() {
                        debug!(
                            "hot.dev: WORKER {} claimer stopping because executors are gone",
                            queue_name
                        );
                        break;
                    }
                }
            }
        }

        info!("hot.dev: WORKER {} claimer stopped", queue_name);
        Ok(())
    })
}

// Add imports for database operations and event handler execution
use hot::db::{
    Build, Context, DatabasePool, DatabaseScope, Env, EventHandler, MaintenanceOperation, Project,
    Schedule,
};
use hot::lang::event::ExecutionContext;
use serde_json::Value as JsonValue;

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

    /// RAII guard that unregisters a run from the `ShutdownCoordinator` when
    /// dropped. `execute_single_event_handler` arms one as its first action so
    /// every exit path — success, VM/join errors, blocking-slot admission
    /// failure, early `?` returns, and the hard-timeout backstop — releases
    /// the caller's `register_run` entry. Without it, error exits leak
    /// `active_runs`/`active_cancel_tokens` entries forever, so
    /// `active_run_count()` never returns to zero and every graceful shutdown
    /// waits out the full drain timeout.
    ///
    /// Deliberately held by the async function and NOT moved into the blocking
    /// VM closure: on hard timeout the async function returns while the
    /// detached VM thread keeps running, and the run must stop counting as
    /// active the moment the executor slot is freed — not when the thread
    /// eventually dies. `unregister_run` is idempotent, so overlapping cleanup
    /// (e.g. `fail_active_runs` during shutdown) stays harmless.
    pub struct RunRegistration {
        coordinator: Arc<ShutdownCoordinator>,
        run_id: Uuid,
    }

    impl RunRegistration {
        pub fn new(coordinator: Arc<ShutdownCoordinator>, run_id: Uuid) -> Self {
            Self {
                coordinator,
                run_id,
            }
        }
    }

    impl Drop for RunRegistration {
        fn drop(&mut self) {
            self.coordinator.unregister_run(&self.run_id);
        }
    }
}

use graceful_shutdown::{RunRegistration, ShutdownCoordinator};

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

/// Bundle execution must never inherit the host's process-global resources,
/// including when a legacy bundle has no resource manifest.
fn install_event_resource_scope(
    is_bundle: bool,
    extract_dir: Option<&std::path::Path>,
) -> Option<hot::lang::hot::resource::ThreadRegistryGuard> {
    if !is_bundle {
        return None;
    }
    let registry = extract_dir
        .map(|directory| {
            let resources = hot::bundle::read_bundle_resources(directory).unwrap_or_else(|error| {
                tracing::warn!(%error, "Cannot read bundle resources; using an empty registry");
                Val::Null
            });
            hot::lang::hot::resource::build_registry_from_manifest(&resources, directory)
        })
        .unwrap_or_default();
    Some(hot::lang::hot::resource::ThreadRegistryGuard::install(
        Arc::new(registry),
    ))
}

/// Keep bundle resource authority around the compiler entry point as well as
/// handler execution. The guard belongs on the synchronous compiler thread,
/// including callers running in `spawn_blocking`.
fn compile_event_bundle_to_cache(
    extract_dir: &std::path::Path,
    src_paths: &[String],
    cache: &hot::lang::cache::bytecode_cache::BytecodeCache,
    project_name: &str,
    cache_key: Option<&str>,
    file_hashes: Option<Vec<hot::lang::cache::bytecode_cache::FileHash>>,
) -> Result<(), String> {
    let _resource_guard = install_event_resource_scope(true, Some(extract_dir));
    hot::lang::engine::Engine::compile_to_cache(
        src_paths,
        cache,
        project_name,
        cache_key,
        file_hashes,
        None, // Deployed dependencies are already included in the bundle.
    )
}

struct PreparedEventBundle {
    src_paths: Vec<String>,
    extract_dir: std::path::PathBuf,
    manifest: Option<hot::bundle::BundleManifest>,
}

/// Perform bundle extraction and pre-compilation away from the Tokio runtime.
/// The caller moves a blocking-execution permit into this function's
/// spawn_blocking closure, so a timed-out setup cannot accumulate unlimited
/// detached filesystem/compiler work.
fn extract_and_precompile_event_bundle(
    build_data: Vec<u8>,
    extract_dir: std::path::PathBuf,
    build_id: Uuid,
    fallback_project_name: String,
    holds_file_lock: bool,
) -> Result<PreparedEventBundle, String> {
    clear_partial_bundle_extraction(&extract_dir, holds_file_lock)?;
    let prepared =
        extract_and_precompile_into(&build_data, &extract_dir, build_id, fallback_project_name);
    if prepared.is_err() && holds_file_lock {
        // Left behind, it would make every later extraction fail.
        let _ = std::fs::remove_dir_all(&extract_dir);
    }
    prepared
}

/// Remove a partial extraction of a bundle, left by a failed attempt or a
/// worker that stopped mid-way: extracting over it fails on every file that
/// exists. Only the holder of the build's file lock may, since another
/// process may be extracting it.
fn clear_partial_bundle_extraction(
    extract_dir: &std::path::Path,
    holds_file_lock: bool,
) -> Result<(), String> {
    if !extract_dir.exists() || BuildPathCache::is_extraction_complete(extract_dir) {
        return Ok(());
    }
    if !holds_file_lock {
        return Err(format!(
            "{} is being prepared by another process",
            extract_dir.display()
        ));
    }
    std::fs::remove_dir_all(extract_dir).map_err(|error| {
        format!(
            "Failed to remove the partial extraction {}: {error}",
            extract_dir.display()
        )
    })
}

fn extract_and_precompile_into(
    build_data: &[u8],
    extract_dir: &std::path::Path,
    build_id: Uuid,
    fallback_project_name: String,
) -> Result<PreparedEventBundle, String> {
    let extract_dir = extract_dir.to_path_buf();
    hot::bundle::extract_bundle_from_bytes(build_data, &extract_dir)?;

    let manifest = hot::bundle::read_bundle_manifest(&extract_dir);
    let build_src_path = extract_dir.join("hot/src");
    let build_pkg_path = extract_dir.join("hot/pkg");
    let mut src_paths = vec![build_src_path.to_string_lossy().to_string()];
    if build_pkg_path.exists() {
        src_paths.push(build_pkg_path.to_string_lossy().to_string());
    }

    let bundle_cache_dir = extract_dir.join(".hot").join("cache");
    if bundle_cache_dir.exists()
        && let Ok(entries) = std::fs::read_dir(&bundle_cache_dir)
    {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.to_string_lossy().ends_with(".bc.zst")
                && let Err(error) = std::fs::remove_file(&path)
            {
                tracing::warn!(?path, %error, "Failed to remove stale bundle cache file");
            }
        }
    }

    let bundle_cache = hot::lang::cache::bytecode_cache::BytecodeCache::new(bundle_cache_dir);
    let (project_name, cache_key, file_hashes) = match &manifest {
        Ok(manifest) => (
            manifest.bundle_name.clone(),
            manifest.cache_key.clone(),
            Some(manifest.file_hashes.clone()),
        ),
        Err(_) => (fallback_project_name, None, None),
    };
    compile_event_bundle_to_cache(
        &extract_dir,
        &src_paths,
        &bundle_cache,
        &project_name,
        cache_key.as_deref(),
        file_hashes,
    )
    .map_err(|error| format!("Failed to pre-compile bundle {build_id}: {error}"))?;

    BuildPathCache::mark_extraction_complete(&extract_dir);
    Ok(PreparedEventBundle {
        src_paths,
        extract_dir,
        manifest: manifest.ok(),
    })
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
/// Default cap on a function's own `timeout` meta (`worker.max-run-timeout`).
///
/// One hour is twice the half-hour model loops that motivated per-handler
/// timeouts, while still bounding how long one run can hold an executor slot
/// and a blocking VM thread. Longer work belongs in a task, whose timeout
/// goes to 24 hours. A run longer than `run-timeout` outlives the orphan
/// reclaim floor, so the keepalive keeps its event claimed and the claim
/// fence stops it if the claim lapses (see `ClaimFence`).
pub const DEFAULT_MAX_RUN_TIMEOUT_SECONDS: u64 = 3_600; // 1 hour
/// Ceiling for `worker.max-run-timeout`: a run must end well before the
/// maintenance reaper fails runs still `running` after 25 hours.
pub const MAX_RUN_TIMEOUT_CEILING_SECONDS: u64 = 86_400; // 24 hours
/// Extra grace period granted after `run-timeout` for a cooperatively-cancelled
/// VM to unwind before the worker hard-detaches the blocking thread and frees the
/// executor slot. Only applied when `worker.cancel-on-timeout` is enabled.
pub const RUN_TIMEOUT_HARD_GRACE: std::time::Duration = std::time::Duration::from_secs(5);
/// Persisting the timeout/result is best-effort after VM execution. It must not
/// contradict the hard-timeout guarantee by pinning the executor slot forever.
pub const RUN_RESULT_FLUSH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
/// Wall-clock ceiling for the post-retrieval half of handler preparation:
/// bytecode-cache work, context loading, store creation, and blocking-slot
/// admission. The watchdog window for this half starts once source
/// resolution completes (see `await_event_handler_setup`); the resolution
/// half — which may include a full bundle download — is bounded separately
/// by `EVENT_SOURCE_RETRIEVAL_TIMEOUT`.
pub const EVENT_SETUP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Wall-clock ceiling for the source-resolution phase of handler setup —
/// tenant/build lookups plus, for cold bundle builds, retrieving the bundle
/// from storage and extracting/precompiling it.
///
/// Storage retrieval needs a bigger budget than the rest of setup because it
/// is plain async work with NO partial progress: a redelivery restarts the
/// download from byte zero (extraction/compile at least self-heal across
/// attempts via their on-disk completion marker). Under the flat 30s
/// watchdog a bundle that deterministically takes longer to download would
/// requeue forever — watchdog fires, the event requeues with a fresh
/// delivery counter, and the next attempt starts the download over. 120s
/// covers a 2 GB bundle at ~20 MB/s (~100s) with slack; before the setup
/// watchdog existed this phase was unbounded. For warm builds (live or
/// already-extracted) resolution completes in microseconds, so the
/// effective setup bound stays `EVENT_SETUP_TIMEOUT`.
pub const EVENT_SOURCE_RETRIEVAL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);
pub const DEFAULT_QUEUE_TYPE: QueueType = QueueType::Sqlite;
pub const DEFAULT_SERIALIZATION: Serialization = Serialization::ZstdJson; // must match Serialization's #[default]

async fn bounded_run_result_write<F, T>(
    timeout: std::time::Duration,
    write: F,
) -> Result<T, tokio::time::error::Elapsed>
where
    F: std::future::Future<Output = T>,
{
    tokio::time::timeout(timeout, write).await
}

/// Ceiling on how long an admitted event handler may wait for a blocking
/// execution slot.
///
/// The wait happens while the handler already holds one of the worker's
/// executor slots and BEFORE its run budget starts (`deadline_at` is stamped
/// only after admission), so an unbounded `acquire` here can pin every
/// executor slot indefinitely once enough detached timed-out VM threads
/// accumulate — each detached thread intentionally holds its permit until it
/// actually exits. 30 seconds is long enough to ride out a burst of detached
/// work draining, but short enough that the un-ACKed event goes back for
/// redelivery promptly. `min_event_orphan_idle_floor_ms` includes this
/// ceiling so orphan reclaim can never start a duplicate execution while a
/// handler is still legally waiting at this gate.
pub const BLOCKING_SLOT_ACQUIRE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Error message for blocking-slot admission timeouts. The event executor
/// matches on it (via `is_blocking_slot_timeout_error`) to recognize that the
/// handler produced no run side effects and the event is safe to leave
/// un-ACKed for redelivery. The bracketed token exists solely for that
/// classification: engine errors are propagated verbatim, so matching on
/// human-readable words alone could misclassify a future engine error that
/// happens to start the same way. Only this constant may contain the token.
const BLOCKING_SLOT_TIMEOUT_ERROR: &str =
    "[blocking-slot-admission-timeout] Timed out waiting for a blocking execution slot";
const EVENT_SETUP_TIMEOUT_ERROR: &str =
    "[event-setup-timeout] Timed out preparing an event handler for execution";

fn is_blocking_slot_timeout_error(error: &str) -> bool {
    error.contains("[blocking-slot-admission-timeout]")
}

fn is_event_setup_timeout_error(error: &str) -> bool {
    error.contains("[event-setup-timeout]")
}

/// Work for a deployment this runtime can't run, made with another major
/// version of Hot, waits until the project is redeployed: minutes at best.
const AWAITING_REDEPLOY_BACKOFF_MS: u64 = 300_000;

/// Whether to log now that work waits on `key`, an account (run admission) or
/// a build (a redeploy): at most once a minute per key, so a pending backlog
/// can't flood the logs.
fn wait_log_due(key: Uuid) -> bool {
    static LAST_LOGGED: OnceLock<std::sync::Mutex<AHashMap<Uuid, std::time::Instant>>> =
        OnceLock::new();
    const INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);
    let mut last = LAST_LOGGED
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let now = std::time::Instant::now();
    if last.len() > 10_000 {
        last.retain(|_, at| now.duration_since(*at) < INTERVAL);
    }
    match last.get(&key) {
        Some(at) if now.duration_since(*at) < INTERVAL => false,
        _ => {
            last.insert(key, now);
            true
        }
    }
}

/// Whether a failed handler should stay pending without spending a delivery:
/// only for the engine's own run-admission deferral (a suspended account or a
/// transient lookup), which happens before any user code, and only when no
/// earlier handler for the same event already started a run. Redelivering after
/// a run started would duplicate its side effects.
fn should_defer_run_admission(error: &str, started_any_handler: bool) -> bool {
    error.starts_with(hot::lang::runtime::vm::RUN_ADMISSION_DEFERRED) && !started_any_handler
}

fn should_redeliver_event_after_handler_error(error: &str, started_any_handler: bool) -> bool {
    (is_blocking_slot_timeout_error(error) || is_event_setup_timeout_error(error))
        && !started_any_handler
}

/// Appended to a handler error that came before its run started: no
/// `run:start` was emitted, so none of the run's code ran and no run row
/// records it. Only `execute_single_event_handler` appends it.
const RUN_NOT_STARTED_TOKEN: &str = "[run-not-started]";

/// A handler error from before its run started (bundle download, extraction,
/// compilation, module initialization): the error without its token.
fn run_never_started(error: &str) -> Option<&str> {
    error.strip_suffix(RUN_NOT_STARTED_TOKEN).map(str::trim_end)
}

/// Redelivery backoff after a handler's setup failed: long enough for a
/// storage or database blip to pass before the next counted attempt.
const SETUP_FAILURE_BACKOFF_MS: u64 = 5_000;

/// Backoff for work this runtime is too old for: a newer worker of a rolling
/// deploy can take it meanwhile.
const NEWER_BUILD_RETRY_BACKOFF_MS: u64 = 15_000;

/// How long a deployment of a build only a newer runtime can run waits for a
/// newer worker, as during a rolling deploy, before this one fails it. A
/// fleet that is all older (a deploy from a newer CLI) fails it then.
const NEWER_RUNTIME_DEPLOY_GRACE: std::time::Duration = std::time::Duration::from_secs(15 * 60);

/// How long a deployment has waited: since its message was published (a
/// version 7 id), else since its build last changed.
fn deployment_wait(
    message_id: Uuid,
    build_updated_at: chrono::DateTime<chrono::Utc>,
) -> std::time::Duration {
    let published = message_id
        .get_timestamp()
        .and_then(|timestamp| {
            let (seconds, nanos) = timestamp.to_unix();
            chrono::DateTime::from_timestamp(i64::try_from(seconds).ok()?, nanos)
        })
        .unwrap_or(build_updated_at);
    chrono::Utc::now()
        .signed_duration_since(published)
        .to_std()
        .unwrap_or_default()
}

/// Whether a build this runtime can't run needs only a newer runtime of the
/// same major version, which a rolling deploy may be starting.
fn build_needs_newer_runtime(build: &Build) -> bool {
    build.is_bundle()
        && hot::version::needs_newer_runtime(
            build.engine_version.as_deref(),
            build.hot_std_version.as_deref(),
            hot::version::current_runtime_version(),
        )
}

/// Watchdog for event-handler preparation, split into two sequential
/// windows so a legitimate cold-bundle download is not confused with a hung
/// setup:
///
/// 1. Source resolution (everything up to the `source_resolved` signal,
///    which may include a full bundle retrieval from storage): bounded by
///    `retrieval_timeout`.
/// 2. Post-retrieval setup (bytecode cache, context loading, stores,
///    blocking-slot admission — until `setup_complete`): a fresh
///    `setup_timeout` that starts only when window 1 closes.
///
/// Builds that never touch storage pass the checkpoint immediately, so
/// their effective bound remains `setup_timeout`. Both windows produce the
/// same `[event-setup-timeout]` token: either way no run side effects have
/// happened and the event is safe to leave un-ACKed for redelivery.
async fn await_event_handler_setup<F>(
    execution: F,
    mut setup_complete: tokio::sync::oneshot::Receiver<()>,
    mut source_resolved: tokio::sync::oneshot::Receiver<()>,
    retrieval_timeout: std::time::Duration,
    setup_timeout: std::time::Duration,
) -> Result<(), String>
where
    F: std::future::Future<Output = Result<(), String>>,
{
    let watchdog = async {
        // A closed channel counts as passing the checkpoint: the execution
        // returned through an early error path, and the biased select below
        // surfaces that original error before this watchdog can fire.
        if tokio::time::timeout(retrieval_timeout, &mut source_resolved)
            .await
            .is_err()
        {
            return format!(
                "{} after {}s resolving handler sources",
                EVENT_SETUP_TIMEOUT_ERROR,
                retrieval_timeout.as_secs_f64(),
            );
        }
        tokio::time::sleep(setup_timeout).await;
        format!(
            "{} after {}s",
            EVENT_SETUP_TIMEOUT_ERROR,
            setup_timeout.as_secs_f64(),
        )
    };
    tokio::pin!(execution);
    tokio::pin!(watchdog);
    tokio::select! {
        biased;
        result = &mut execution => result,
        setup = &mut setup_complete => {
            // A closed channel means the execution returned through an early
            // error path before setup completed; await it to surface that
            // original error instead of misclassifying it as a timeout.
            let _ = setup;
            execution.await
        }
        message = &mut watchdog => Err(message),
    }
}

/// Acquire a blocking-execution permit, bounded by `acquire_timeout`
/// (production passes `BLOCKING_SLOT_ACQUIRE_TIMEOUT`; tests pass a short
/// duration to stay deterministic without waiting out the real ceiling).
async fn acquire_blocking_execution_slot(
    blocking_execution_slots: Arc<Semaphore>,
    run_id: Uuid,
    acquire_timeout: std::time::Duration,
) -> Result<tokio::sync::OwnedSemaphorePermit, String> {
    match tokio::time::timeout(
        acquire_timeout,
        Arc::clone(&blocking_execution_slots).acquire_owned(),
    )
    .await
    {
        Ok(Ok(permit)) => Ok(permit),
        Ok(Err(_)) => Err("Blocking execution limiter closed".to_string()),
        Err(_) => {
            warn!(
                run_id = %run_id,
                timeout_secs = acquire_timeout.as_secs(),
                available_permits = blocking_execution_slots.available_permits(),
                "Blocking execution slot admission timed out; detached timed-out VM threads are holding the permits"
            );
            Err(format!(
                "{} after {}s",
                BLOCKING_SLOT_TIMEOUT_ERROR,
                acquire_timeout.as_secs()
            ))
        }
    }
}

/// Floor for the hot:event orphan-reclaim idle threshold.
///
/// A handler that claimed an event may legally spend up to
/// `EVENT_SOURCE_RETRIEVAL_TIMEOUT` resolving sources (including a cold
/// bundle download), plus `EVENT_SETUP_TIMEOUT` in post-retrieval
/// preparation (including blocking-slot admission), then `run-timeout` plus
/// `RUN_TIMEOUT_HARD_GRACE` executing, before the worker gives up — and the
/// event is only ACKed after that. This floor budgets that FULL legal
/// pre-ACK lifetime: for a SINGLE-handler event, reclaiming below it would
/// XAUTOCLAIM an un-ACKed event whose handler is still legally live and start
/// a duplicate execution on another worker. The extra 5s covers result-flush
/// and scheduling slack.
///
/// In practice the retrieval window is already covered live by the
/// PEL-touch keepalive, which starts at executor pickup — BEFORE handler
/// setup begins (see the executor loop in `start_server`) — so this floor
/// is backstop-only math for the case where the keepalive dies with its
/// worker.
///
/// Multi-handler liveness: this floor budgets ONE full setup (retrieval,
/// preparation, admission wait) plus ONE run budget, while an event with N
/// matching handlers executes them serially — each with its own setup and
/// run budget — before the
/// single trailing ACK. That gap is closed by the PEL-touch keepalive
/// (`spawn_event_lease_keepalive`): while an event is being processed, the
/// executor periodically resets the delivery's PEL idle clock
/// (ownership-guarded `XCLAIM ... JUSTID`, delivery counter untouched) so
/// orphan reclaim keeps seeing it as live no matter how many handlers run.
/// The floor remains the backstop for the keepalive itself: if the touch
/// task dies with its worker, reclaim still waits out one full legal
/// single-handler lifetime before redelivering. Raising
/// `queue.event-orphan-idle-ms` (the configured value is only floored here,
/// never lowered) is therefore no longer required for multi-handler events,
/// though it remains available for a wider reclaim margin.
///
/// Long runs: a function's `timeout` meta can give a run more than
/// `run-timeout`, up to `max-run-timeout`. Like a multi-handler event, such a
/// run outlives this floor and relies on the keepalive. The floor stays on
/// `run-timeout` rather than the cap so a crashed worker's events are
/// redelivered as promptly as before; instead, the claim fence
/// (`ClaimFence`) stops a run whose worker can no longer vouch for its claim,
/// before another worker could reclaim the event.
fn min_event_orphan_idle_floor_ms(run_timeout: std::time::Duration) -> u64 {
    run_timeout
        .saturating_add(EVENT_SOURCE_RETRIEVAL_TIMEOUT)
        .saturating_add(EVENT_SETUP_TIMEOUT)
        .saturating_add(RUN_TIMEOUT_HARD_GRACE)
        .saturating_add(std::time::Duration::from_secs(5))
        .as_millis()
        .min(u64::MAX as u128) as u64
}

/// Floor for the PEL-touch keepalive interval. Guards against a tiny
/// (test-sized) orphan-idle configuration turning the keepalive into a
/// Redis-hammering busy loop.
const EVENT_KEEPALIVE_MIN_INTERVAL_MS: u64 = 5_000;

/// Cadence of the PEL-touch keepalive for an executing event: one third of
/// the effective orphan-reclaim idle threshold, floored at
/// `EVENT_KEEPALIVE_MIN_INTERVAL_MS`. A live delivery gets at least two
/// touch opportunities inside every reclaim window, and failed touches
/// retry at half cadence, so a single missed or failed touch cannot let a
/// sibling janitor XAUTOCLAIM it mid-execution even at small configured
/// run-timeouts.
fn event_keepalive_interval_ms(effective_orphan_idle_ms: u64) -> u64 {
    (effective_orphan_idle_ms / 3).max(EVENT_KEEPALIVE_MIN_INTERVAL_MS)
}

/// How long the keepalive may go without confirming a delivery's claim
/// before the claim fence stops its runs: two thirds of the effective
/// reclaim idle. Another worker can reclaim the entry only once it has sat a
/// full idle window since its last touch, so the remaining third leaves time
/// for a stopped run to unwind (`RUN_TIMEOUT_HARD_GRACE`) and record its
/// failure before any redelivery could start it again.
fn event_claim_fence_after(effective_orphan_idle_ms: u64) -> std::time::Duration {
    std::time::Duration::from_millis(effective_orphan_idle_ms / 3 * 2)
}

/// Why a worker can no longer vouch for its claim on an event delivery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClaimLoss {
    /// The delivery is no longer pending for this consumer: another worker
    /// reclaimed it.
    Lost,
    /// No touch has confirmed the claim for `unconfirmed`. Another worker
    /// can reclaim the delivery from `reclaimable_at`.
    Unconfirmed {
        unconfirmed: std::time::Duration,
        reclaimable_at: tokio::time::Instant,
    },
}

/// When the runs of a delivery whose claim is unconfirmed stop: at the last
/// moment that still leaves a stopped run time to unwind and record its
/// failure before another worker could reclaim the event, so a run that ends
/// before then is not stopped.
fn unconfirmed_claim_stop_at(reclaimable_at: tokio::time::Instant) -> tokio::time::Instant {
    reclaimable_at
        .checked_sub(RUN_TIMEOUT_HARD_GRACE + RUN_RESULT_FLUSH_TIMEOUT)
        .unwrap_or(reclaimable_at)
}

/// Whether a delivery may still start a run, checked before each handler:
/// not once another worker has the event, nor once an unconfirmed claim's
/// stop time has passed (the run would be stopped as soon as it started).
fn claim_stops_new_runs(
    claim: Option<&tokio::sync::watch::Receiver<Option<ClaimLoss>>>,
    now: tokio::time::Instant,
) -> Option<ClaimLoss> {
    let loss = (*claim?.borrow())?;
    match loss {
        ClaimLoss::Lost => Some(loss),
        ClaimLoss::Unconfirmed { reclaimable_at, .. } => {
            (unconfirmed_claim_stop_at(reclaimable_at) <= now).then_some(loss)
        }
    }
}

/// The claim fence: from the keepalive's touch results, decide when a worker
/// can no longer vouch for its claim on an event delivery, and tell that
/// delivery's runs, which stop (see `run_must_stop`). A run longer than the
/// orphan-reclaim floor relies on the keepalive alone, so without the fence
/// a run whose touches failed long enough would keep running while another
/// worker started it again.
///
/// Only `Lost` is final. An unconfirmed claim is revised by every later
/// touch: a confirmation clears it, so a Redis outage that ends in time
/// stops nothing, and the stop time always follows the latest confirmation.
struct ClaimFence {
    last_confirmed: tokio::time::Instant,
    fence_after: std::time::Duration,
    /// How long after its last confirmed touch another worker can reclaim
    /// the delivery: the effective orphan-reclaim idle.
    reclaim_after: std::time::Duration,
    loss: tokio::sync::watch::Sender<Option<ClaimLoss>>,
}

impl ClaimFence {
    /// A fence for a delivery whose PEL idle clock started at `delivered_at`:
    /// until a touch confirms it, that is when it was last vouched for.
    fn new(
        fence_after: std::time::Duration,
        reclaim_after: std::time::Duration,
        delivered_at: tokio::time::Instant,
    ) -> (Self, tokio::sync::watch::Receiver<Option<ClaimLoss>>) {
        let (loss, claim) = tokio::sync::watch::channel(None);
        (
            Self {
                last_confirmed: delivered_at,
                fence_after,
                reclaim_after,
                loss,
            },
            claim,
        )
    }

    /// The current verdict.
    fn verdict(&self) -> Option<ClaimLoss> {
        *self.loss.borrow()
    }

    /// Record one touch: its outcome, or `None` when it failed to reach
    /// Redis. Returns the loss when this touch trips the fence.
    fn observe(
        &mut self,
        touched: Option<hot::queue::streams::PendingTouch>,
        now: tokio::time::Instant,
    ) -> Option<ClaimLoss> {
        use hot::queue::streams::PendingTouch;
        let current = self.verdict();
        if current == Some(ClaimLoss::Lost) {
            return None;
        }
        let verdict = match touched {
            Some(PendingTouch::Touched) => {
                self.last_confirmed = now;
                None
            }
            // Trimmed from the stream or acknowledged: no other worker can
            // be handed the delivery any more, so the claim holds for good.
            Some(PendingTouch::Trimmed | PendingTouch::Acked) => None,
            Some(PendingTouch::Reclaimed) => Some(ClaimLoss::Lost),
            None => {
                let unconfirmed = now.saturating_duration_since(self.last_confirmed);
                let reclaimable_at = self.last_confirmed + self.reclaim_after;
                match current {
                    // Still unconfirmed since the same touch: keep the verdict.
                    Some(
                        loss @ ClaimLoss::Unconfirmed {
                            reclaimable_at: at, ..
                        },
                    ) if at == reclaimable_at => Some(loss),
                    _ => (unconfirmed >= self.fence_after).then_some(ClaimLoss::Unconfirmed {
                        unconfirmed,
                        reclaimable_at,
                    }),
                }
            }
        };
        if verdict == current {
            return None;
        }
        self.loss.send_replace(verdict);
        verdict
    }
}

/// Abort-on-drop guard around the keepalive task spawned by
/// `spawn_event_lease_keepalive`. Dropping the guard aborts the touch loop,
/// so it stops on EVERY processing exit path — ACK, handler error, executor
/// panic — and can never keep refreshing an entry the worker is no longer
/// working on. It also hands the delivery's runs the claim fence's verdict.
struct EventKeepaliveGuard {
    touch_loop: tokio::task::JoinHandle<()>,
    claim: tokio::sync::watch::Receiver<Option<ClaimLoss>>,
}

impl EventKeepaliveGuard {
    /// The claim fence's verdict for this delivery, for its runs to watch.
    fn claim(&self) -> tokio::sync::watch::Receiver<Option<ClaimLoss>> {
        self.claim.clone()
    }
}

impl Drop for EventKeepaliveGuard {
    fn drop(&mut self) {
        self.touch_loop.abort();
    }
}

/// Keep a claimed hot:event delivery visibly alive in the Redis PEL while
/// its (possibly many) handlers execute serially.
///
/// `min_event_orphan_idle_floor_ms` budgets one full setup (source
/// retrieval plus post-retrieval preparation including admission) plus one
/// run budget, but an N-handler event pays the per-handler cost N times
/// before the single trailing ACK. Instead of asking operators to
/// over-provision the reclaim
/// idle, the executor touches the delivery every
/// `event_keepalive_interval_ms` (ownership-guarded `XCLAIM ... JUSTID`,
/// which does not increment the delivery counter) so sibling janitors keep
/// seeing it as live.
///
/// Returns `None` for memory queues (no PEL, no orphan reclaim — nothing to
/// keep alive) and for already-consumed leases. The returned guard must live
/// exactly as long as processing: dropping it aborts the touch task.
///
/// The touch results feed the claim fence (`ClaimFence`): when a touch finds
/// the delivery gone, or none has confirmed it for `event_claim_fence_after`,
/// the delivery's runs stop.
///
/// Coverage window: from executor pickup (the first touch fires
/// immediately, resetting any idle accrued in the prefetch buffer or the
/// claimer->executor handoff) through the trailing ACK. Time spent buffered
/// BEFORE pickup is covered only by the orphan-idle floor, so under
/// sustained executor saturation a still-buffered delivery can exceed the
/// reclaim idle and be XAUTOCLAIMed — a pre-existing tail risk the
/// keepalive narrows but does not close.
fn spawn_event_lease_keepalive(
    lease: &ProcessingQueueLease<Message>,
    effective_orphan_idle_ms: u64,
    worker_id: usize,
) -> Option<EventKeepaliveGuard> {
    let keepalive = match lease {
        ProcessingQueueLease::Memory(_) => return None,
        // SQLite leases renew inside `SqliteQueueLease::process`; the durable
        // lease itself is also the handoff backstop before processing starts.
        ProcessingQueueLease::Sqlite(_) => return None,
        ProcessingQueueLease::Redis(lease) => lease.keepalive()?,
    };
    let interval =
        std::time::Duration::from_millis(event_keepalive_interval_ms(effective_orphan_idle_ms));
    // The delivery's PEL idle clock has run since Redis handed it over, not
    // since this executor picked it up.
    let (mut fence, claim) = ClaimFence::new(
        event_claim_fence_after(effective_orphan_idle_ms),
        std::time::Duration::from_millis(effective_orphan_idle_ms),
        keepalive.delivered_at(),
    );
    let touch_loop = tokio::spawn(async move {
        use hot::queue::streams::PendingTouch;
        // First touch immediately: the delivery may have accrued idle in the
        // prefetch buffer or the claimer->executor handoff before this
        // executor picked it up, so the reclaim window is reset at execution
        // start rather than assumed fresh. (Time spent buffered BEFORE
        // pickup is covered only by the orphan-idle floor; see the fn doc.)
        let mut delay = std::time::Duration::ZERO;
        loop {
            tokio::time::sleep(delay).await;
            let touched = keepalive.touch().await;
            let unconfirmed = fence.verdict().is_some();
            if let Some(loss) =
                fence.observe(touched.as_ref().ok().copied(), tokio::time::Instant::now())
            {
                warn!(
                    "hot.dev: WORKER {} stopping the runs of event delivery {}: {:?}",
                    worker_id,
                    keepalive.msg_id(),
                    loss
                );
            } else if unconfirmed && fence.verdict().is_none() {
                info!(
                    "hot.dev: WORKER {} confirmed its claim on event delivery {} again; its runs continue",
                    worker_id,
                    keepalive.msg_id()
                );
            }
            match touched {
                Ok(PendingTouch::Touched) => {
                    debug!(
                        "hot.dev: WORKER {} keepalive touched event delivery {}",
                        worker_id,
                        keepalive.msg_id()
                    );
                    delay = interval;
                }
                Ok(PendingTouch::Reclaimed) => {
                    // A sibling reclaimed the entry: there is nothing left to
                    // keep alive (and touching must never steal it back).
                    warn!(
                        "hot.dev: WORKER {} keepalive stopping: another worker reclaimed event delivery {}",
                        worker_id,
                        keepalive.msg_id()
                    );
                    break;
                }
                Ok(PendingTouch::Trimmed) => {
                    // The stream's length cap trimmed the entry. Nobody else
                    // can be handed it, so its runs go on, but its idle clock
                    // can't be reset, and if this worker dies the event is
                    // not redelivered.
                    warn!(
                        "hot.dev: WORKER {} keepalive stopping: event delivery {} was trimmed from the stream; its runs continue",
                        worker_id,
                        keepalive.msg_id()
                    );
                    break;
                }
                Ok(PendingTouch::Acked) => {
                    debug!(
                        "hot.dev: WORKER {} keepalive stopping: event delivery {} was acknowledged",
                        worker_id,
                        keepalive.msg_id()
                    );
                    break;
                }
                Err(e) => {
                    // Transient Redis failure: retry at half cadence so a
                    // single failed touch (worst case one command timeout
                    // plus a reconnect) cannot stack a full extra interval
                    // onto the gap between successful touches — this keeps
                    // the margin at small configured run-timeouts too.
                    debug!(
                        "hot.dev: WORKER {} keepalive touch failed for event delivery {}: {}",
                        worker_id,
                        keepalive.msg_id(),
                        e
                    );
                    delay = interval / 2;
                }
            }
        }
    });
    Some(EventKeepaliveGuard { touch_loop, claim })
}

// Simplified approach: ensure proper build isolation without complex caching
// Each build execution will be isolated by using fresh compilation contexts

// Helper functions for creating emitter and event publisher

/// Create a emitter based on worker configuration
async fn create_emitter(
    conf: &Val,
    db_pool: &DatabasePool,
    blob_store: Option<Arc<hot::blob::BlobStore>>,
) -> Result<Option<std::sync::Arc<dyn EngineEventEmitter>>, String> {
    // Get resolved emitter configuration (hot.emitter.type in config becomes emitter.type after load_conf)
    let emitter_conf = conf.get("emitter").unwrap_or_else(Val::map_empty);

    // Get emitter type
    let emitter_type = emitter_conf.get_str("type");

    tracing::debug!(
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
            tracing::debug!("create_emitter: creating console emitter");
            let console_emitter = ConsoleEngineEventEmitter::new();
            let filtered_emitter =
                hot::lang::emitter::FilteredEmitter::new(console_emitter, filter_conf.as_ref())?;
            Ok(Some(std::sync::Arc::new(filtered_emitter)))
        }
        "db" => {
            tracing::debug!("create_emitter: creating db emitter");
            // Use existing database pool instead of creating a new one
            // Note: stream_data is no longer persisted to DB - delivered via Redis Streams only
            let max_calls_per_run = emitter_conf
                .get_int_or_default(
                    "max-calls-per-run",
                    hot::lang::emitter::database::DEFAULT_MAX_CALLS_PER_RUN as i64,
                )
                .max(0) as usize;
            let db_emitter = DatabaseEngineEventEmitter::new_with_pool_and_blob_store(
                db_pool.clone(),
                blob_store,
            )
            .with_max_calls_per_run(max_calls_per_run);
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
async fn create_event_publisher(
    conf: &Val,
    db_pool: &DatabasePool,
    blob_store: Option<Arc<hot::blob::BlobStore>>,
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
    let database_publisher =
        DatabaseEventPublisher::new_with_pool_and_blob_store(db_pool.clone(), blob_store);

    // Create queue publisher with extracted configuration including cluster mode
    let queue_publisher = hot::lang::event::QueueEventPublisher::new_with_cluster(
        queue_type,
        "hot:event".to_string(),
        redis_uri,
        redis_cluster,
        serialization,
    )?;

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

/// Whether the environment has a deployment this runtime can't run, made
/// with another major version of Hot.
async fn env_awaits_redeploy(db: &DatabasePool, env_id: &Uuid) -> Result<bool, String> {
    Ok(find_all_deployed_builds_for_env(db, env_id)
        .await?
        .iter()
        .any(|build| build.runtime_version_error_message().is_some()))
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

#[derive(Debug)]
struct ParkedEvent;
impl std::fmt::Display for ParkedEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Event is durably parked until its environment or project is active")
    }
}
impl std::error::Error for ParkedEvent {}

async fn park_event(db: &DatabasePool, message: &EventMessage) -> Result<(), WorkerError> {
    let payload = serde_json::to_string(message)?;
    let sql = "INSERT INTO parked_event(message_id,env_id,build_id,payload) VALUES($1,$2,$3,$4) ON CONFLICT(message_id) DO NOTHING";
    match db {
        DatabasePool::Postgres(pool) => {
            sqlx::query(sql)
                .bind(message.id)
                .bind(message.body.event.env_id)
                .bind(message.body.execution_context.build_id)
                .bind(payload)
                .execute(pool)
                .await?;
        }
        DatabasePool::Sqlite(pool) => {
            sqlx::query(sql)
                .bind(message.id)
                .bind(message.body.event.env_id)
                .bind(message.body.execution_context.build_id)
                .bind(payload)
                .execute(pool)
                .await?;
        }
    }
    Ok(())
}

/// The environments with parked events that may run again. Their parked rows
/// can far outnumber them, and only a few environments are ever suspended, so
/// this skips through the distinct environments on the env index instead of
/// reading every parked row on each tick.
const ADMITTED_PARKED_ENVS: &str = "WITH RECURSIVE parked(env_id) AS (
        SELECT (SELECT env_id FROM parked_event ORDER BY env_id LIMIT 1)
        UNION ALL
        SELECT (SELECT pe.env_id FROM parked_event pe WHERE pe.env_id > parked.env_id ORDER BY pe.env_id LIMIT 1)
        FROM parked WHERE parked.env_id IS NOT NULL
    )
    SELECT e.env_id FROM parked JOIN env e ON e.env_id=parked.env_id
    JOIN org o ON o.org_id=e.org_id JOIN org r ON r.org_id=CASE WHEN o.parent_org_id IS NULL THEN o.org_id ELSE o.parent_org_id END
    WHERE e.active=true AND o.active=true AND r.active=true
    AND o.lifecycle_state='active' AND r.lifecycle_state='active'";

/// How long a parked event whose project is still inactive waits before its
/// environment's next replay looks at it again, so it doesn't hold the head of
/// that environment's due rows.
const PARKED_PROJECT_BACKOFF_SECS: i64 = 300;

async fn replay_parked_events(
    db: &DatabasePool,
    queue: &ProcessingQueue<Message>,
) -> Result<(), WorkerError> {
    let now = chrono::Utc::now();
    let lease = now + chrono::Duration::seconds(90);
    let backoff = now + chrono::Duration::seconds(PARKED_PROJECT_BACKOFF_SECS);
    let envs: Vec<Uuid> = match db {
        DatabasePool::Postgres(pool) => {
            sqlx::query_scalar(ADMITTED_PARKED_ENVS)
                .fetch_all(pool)
                .await?
        }
        DatabasePool::Sqlite(pool) => {
            sqlx::query_scalar(ADMITTED_PARKED_ENVS)
                .fetch_all(pool)
                .await?
        }
    };
    // Lease an admitted environment's due rows: those whose project is active
    // for replay, the others for a back-off.
    let sql = "UPDATE parked_event SET leased_until=CASE WHEN build_id IS NULL OR EXISTS (
            SELECT 1 FROM build b JOIN project p ON p.project_id=b.project_id
            WHERE b.build_id=parked_event.build_id AND p.active=true
        ) THEN $2 ELSE $3 END
        WHERE message_id IN (
            SELECT message_id FROM parked_event WHERE env_id=$4 AND leased_until <= $1
            ORDER BY leased_until LIMIT 100
        ) AND leased_until <= $1 RETURNING message_id,payload,leased_until=$2";
    for env_id in envs {
        let rows: Vec<(Uuid, String, bool)> = match db {
            DatabasePool::Postgres(pool) => {
                sqlx::query_as(sql)
                    .bind(now)
                    .bind(lease)
                    .bind(backoff)
                    .bind(env_id)
                    .fetch_all(pool)
                    .await?
            }
            DatabasePool::Sqlite(pool) => {
                sqlx::query_as(sql)
                    .bind(now)
                    .bind(lease)
                    .bind(backoff)
                    .bind(env_id)
                    .fetch_all(pool)
                    .await?
            }
        };
        for (id, payload, _) in rows.into_iter().filter(|(_, _, replay)| *replay) {
            let message: EventMessage = serde_json::from_str(&payload)?;
            queue.enqueue(message.into()).await?;
            let sql = "DELETE FROM parked_event WHERE message_id=$1 AND leased_until=$2";
            match db {
                DatabasePool::Postgres(pool) => {
                    sqlx::query(sql).bind(id).bind(lease).execute(pool).await?;
                }
                DatabasePool::Sqlite(pool) => {
                    sqlx::query(sql).bind(id).bind(lease).execute(pool).await?;
                }
            }
        }
    }
    Ok(())
}

async fn hydrate_event_message_from_db(
    worker_id: usize,
    db: &DatabasePool,
    blob_store: Option<&Arc<hot::blob::BlobStore>>,
    event_message: EventMessage,
    infra_retry_backoff_ms: u64,
) -> Result<EventMessage, WorkerError> {
    let env_id = event_message.body.event.env_id;
    let event_id = event_message.body.event.event_id;

    // The queue carries a thin event envelope. Load the authoritative event
    // from DB, ensure its env_id matches the queue message, then route using
    // hydrated event_data.
    let stored_event = match hot::db::event::Event::get_event(db, &event_id).await {
        Ok(stored_event) => {
            if stored_event.env_id != env_id {
                let err_msg = format!(
                    "Event env_id mismatch - message claims {} but event {} belongs to {}",
                    env_id, event_id, stored_event.env_id
                );
                error!("hot.dev: WORKER {} {}", worker_id, err_msg);
                return Err(worker_error(err_msg));
            }
            debug!(
                "hot.dev: WORKER {} verified event {} belongs to env {}",
                worker_id, event_id, env_id
            );
            stored_event
        }
        Err(hot::db::event::EventError::NotFound) => {
            let err_msg = format!(
                "Event {} not found in database - cannot verify env_id",
                event_id
            );
            error!("hot.dev: WORKER {} {}", worker_id, err_msg);
            return Err(worker_error(err_msg));
        }
        Err(e) => {
            let err_msg = format!(
                "Failed to verify event {} env_id - database error: {}",
                event_id, e
            );
            error!("hot.dev: WORKER {} {}", worker_id, err_msg);
            return Err(worker_infrastructure_retry_error(
                err_msg,
                infra_retry_backoff_ms,
            ));
        }
    };
    let stored_env = Env::get_env(db, &stored_event.env_id).await.map_err(|e| {
        worker_infrastructure_retry_error(
            format!(
                "Failed to load authoritative environment {} for event {}: {}",
                stored_event.env_id, event_id, e
            ),
            infra_retry_backoff_ms,
        )
    })?;
    if event_message
        .body
        .execution_context
        .org_id
        .is_some_and(|id| id != stored_env.org_id)
        || event_message
            .body
            .execution_context
            .env_id
            .is_some_and(|id| id != stored_env.env_id)
        || event_message.body.event.stream_id != stored_event.stream_id
        || event_message.body.execution_context.stream_id != stored_event.stream_id
    {
        return Err(worker_error(
            "Queued event scope does not match durable event ownership",
        ));
    }
    if !hot::db::Org::is_environment_admitted(db, &stored_env.env_id)
        .await
        .map_err(|e| {
            worker_infrastructure_retry_error(
                format!("Failed to verify execution admission: {e}"),
                infra_retry_backoff_ms,
            )
        })?
    {
        park_event(db, &event_message).await?;
        return Err(Box::new(ParkedEvent));
    }
    let scoped_db = db.with_scope(DatabaseScope::Environment {
        org_id: stored_env.org_id,
        env_id: stored_env.env_id,
    });
    let db = &scoped_db;
    let org_slug = hot::db::org::Org::get_org(db, &stored_env.org_id)
        .await
        .ok()
        .map(|org| org.slug);

    let (target_project_id, target_project_name) =
        if let Some(project_id) = event_message.body.event.target_project_id {
            let project = Project::get_project(db, &project_id).await.map_err(|e| {
                worker_error(format!(
                    "Target project {} for event {} is invalid: {}",
                    project_id, event_id, e
                ))
            })?;
            if project.env_id != stored_event.env_id {
                return Err(worker_error(format!(
                    "Target project {} does not belong to event environment {}",
                    project_id, stored_event.env_id
                )));
            }
            (Some(project.project_id), Some(project.name))
        } else {
            (None, None)
        };

    let claimed_context = &event_message.body.execution_context;
    let persisted_target_build_id = stored_event
        .event_data
        .get("_hot_target_build_id")
        .and_then(|value| value.as_str())
        .map(|value| {
            value.parse::<Uuid>().map_err(|e| {
                worker_error(format!(
                    "Persisted target build for event {} is invalid: {}",
                    event_id, e
                ))
            })
        })
        .transpose()?;
    if let (Some(persisted), Some(claimed)) = (persisted_target_build_id, claimed_context.build_id)
        && persisted != claimed
    {
        return Err(worker_error(format!(
            "Queued target build does not match persisted event {}",
            event_id
        )));
    }
    let target_build_id = persisted_target_build_id.or(claimed_context.build_id);
    let authoritative_build = if let Some(build_id) = target_build_id {
        let build = Build::get_build(db, &build_id).await.map_err(|e| {
            worker_error(format!(
                "Target build {} for event {} is invalid: {}",
                build_id, event_id, e
            ))
        })?;
        let project = Project::get_project(db, &build.project_id)
            .await
            .map_err(|e| {
                worker_error(format!(
                    "Project {} for target build {} is invalid: {}",
                    build.project_id, build_id, e
                ))
            })?;
        if project.env_id != stored_event.env_id {
            return Err(worker_error(format!(
                "Target build {} does not belong to event environment {}",
                build_id, stored_event.env_id
            )));
        }
        if target_project_id.is_some_and(|project_id| project_id != build.project_id) {
            return Err(worker_error(format!(
                "Target build {} does not belong to target project",
                build_id
            )));
        }
        Some((build, project))
    } else {
        None
    };
    if authoritative_build
        .as_ref()
        .is_some_and(|(_, project)| !project.active)
    {
        park_event(db, &event_message).await?;
        return Err(Box::new(ParkedEvent));
    }
    if !(0..=100).contains(&claimed_context.retry_attempt) {
        return Err(worker_error(format!(
            "Invalid retry attempt {} for event {}",
            claimed_context.retry_attempt, event_id
        )));
    }
    if let Some(origin_run_id) = claimed_context.origin_run_id {
        let origin = hot::db::run::Run::get_run(db, &origin_run_id)
            .await
            .map_err(|e| {
                worker_error(format!(
                    "Origin run {} for event {} is invalid: {}",
                    origin_run_id, event_id, e
                ))
            })?;
        if origin.env_id != stored_event.env_id {
            return Err(worker_error(format!(
                "Origin run {} does not belong to event environment {}",
                origin_run_id, stored_event.env_id
            )));
        }
    }

    let event_data: Val = serde_json::from_value(stored_event.event_data).map_err(|e| {
        let err_msg = format!(
            "Invalid event_data for event {} - cannot hydrate queued event: {}",
            event_id, e
        );
        error!("hot.dev: WORKER {} {}", worker_id, err_msg);
        worker_error(err_msg)
    })?;

    // Rehydrate spilled blob references before the payload reaches handler
    // code. Executable payloads fail closed: a BlobRef the store cannot
    // resolve (missing, inactive, or wrong tenant) fails the event rather
    // than silently handing user code a ref map.
    let event_data = if hot::blob::contains_blob_ref(&event_data) {
        let Some(store) = blob_store else {
            let err_msg = format!(
                "Event {} contains blob references but blob storage is not configured - cannot hydrate",
                event_id
            );
            error!("hot.dev: WORKER {} {}", worker_id, err_msg);
            return Err(worker_error(err_msg));
        };
        let scope = hot::blob::BlobScope {
            org_id: stored_env.org_id,
            env_id: Some(stored_event.env_id),
            run_id: None,
        };
        let budget = hot::blob::RehydrateBudget::from_config(store.config());
        store
            .rehydrate_blob_refs(event_data, scope, budget)
            .await
            .map_err(|e| {
                let err_msg = format!(
                    "Failed to rehydrate blob references for event {}: {}",
                    event_id, e
                );
                error!("hot.dev: WORKER {} {}", worker_id, err_msg);
                worker_error(err_msg)
            })?
    } else {
        event_data
    };

    // Webhook original-url values are persisted token-redacted; the queue
    // envelope carries the raw URL. Re-attach it for delivery — verifiers
    // hash the exact URL the provider called. Slots the envelope cannot
    // corroborate are dropped so verifiers fail closed on absence instead
    // of hashing a placeholder.
    let mut event_data = event_data;
    hot::webhook_url::restore_event_data_original_urls(
        &mut event_data,
        &event_message.body.event.event_data,
    );

    let run_type_id = match stored_event.event_type.as_str() {
        "hot:call" => hot::db::run::RunType::Call.as_id(),
        "hot:schedule" => hot::db::run::RunType::Schedule.as_id(),
        _ => hot::db::run::RunType::Event.as_id(),
    };
    let execution_context = ExecutionContext::new_with_event_and_origin(
        claimed_context.run_id,
        stored_event.stream_id,
        run_type_id,
        Some(stored_event.env_id),
        Some(stored_event.created_by_user_id),
        Some(stored_env.org_id),
        authoritative_build
            .as_ref()
            .map(|(build, _)| build.build_id),
        Some(stored_event.event_id),
        claimed_context.origin_run_id,
    )
    .with_env_name(Some(stored_env.name))
    .with_org_slug(org_slug)
    .with_project(
        authoritative_build
            .as_ref()
            .map(|(build, _)| build.project_id),
        authoritative_build
            .as_ref()
            .map(|(_, project)| project.name.clone()),
    )
    .with_build_hash(
        authoritative_build
            .as_ref()
            .map(|(build, _)| build.hash.clone()),
    )
    .with_retry_attempt(claimed_context.retry_attempt)
    .with_access_id(stored_event.access_id);

    Ok(EventMessage {
        body: hot::lang::event::queue::EventMessageBody {
            event: hot::lang::event::Event {
                event_id: stored_event.event_id,
                env_id: stored_event.env_id,
                stream_id: stored_event.stream_id,
                event_type: stored_event.event_type,
                event_data,
                event_time: stored_event.event_time,
                target_project_id,
                target_project_name,
            },
            execution_context,
        },
        ..event_message
    })
}

async fn should_defer_missing_event_handlers(
    worker_id: usize,
    db: &DatabasePool,
    env_id: &Uuid,
    event_type: &str,
    infra_retry_backoff_ms: u64,
) -> Result<bool, WorkerError> {
    let builds = Build::get_builds_by_env(db, env_id, Some(8), None)
        .await
        .map_err(|e| {
            let err_msg = format!(
                "Failed to inspect deployed builds before handling missing event handlers for '{}': {}",
                event_type, e
            );
            error!("hot.dev: WORKER {} {}", worker_id, err_msg);
            worker_infrastructure_retry_error(err_msg, infra_retry_backoff_ms)
        })?;

    let deployment_in_progress = builds.iter().any(|build| {
        build.is_bundle()
            && build.deployment_sequence > 0
            && (build.runtime_status == Build::RUNTIME_STATUS_PENDING
                || build.runtime_status == Build::RUNTIME_STATUS_LOADING)
    });

    if deployment_in_progress {
        debug!(
            "hot.dev: WORKER {} deferring event '{}' while bundle deployment preparation is in progress",
            worker_id, event_type
        );
    }

    Ok(deployment_in_progress)
}

async fn acknowledge_unhandled_event_if_stable(
    worker_id: usize,
    db: &DatabasePool,
    env_id: &Uuid,
    event_type: &str,
    err_msg: String,
    infra_retry_backoff_ms: u64,
) -> Result<(), WorkerError> {
    if should_defer_missing_event_handlers(
        worker_id,
        db,
        env_id,
        event_type,
        infra_retry_backoff_ms,
    )
    .await?
    {
        return Err(worker_infrastructure_retry_error(
            err_msg,
            infra_retry_backoff_ms,
        ));
    }

    debug!(
        "hot.dev: WORKER {} {}; acknowledging event as unhandled",
        worker_id, err_msg
    );
    Ok(())
}

async fn process_alert_delivery_message(
    worker_id: usize,
    db: Arc<DatabasePool>,
    worker_conf: Val,
    message: Message,
) -> Result<(), WorkerError> {
    let db = Arc::new(db.with_scope(DatabaseScope::Maintenance {
        operation: MaintenanceOperation::Alerts,
        org_id: None,
    }));
    let alert_msg: hot::lang::event::queue::AlertDeliveryMessage =
        message.try_into().map_err(|e: String| worker_error(e))?;

    tracing::debug!(
        "hot.dev: WORKER {} processing alert delivery {} from hot:alert queue",
        worker_id,
        alert_msg.body.alert_delivery_id
    );

    let http_client = alert_http_client();

    let alert_email_sender = hot::email::EmailSender::alerts_from_conf(&worker_conf);
    let alert_email_config = hot::email::EmailConfig::alerts_from_conf(&worker_conf);
    let email_sender_ref: Option<&dyn hot::db::alert::AlertEmailSender> =
        if alert_email_sender.is_available() {
            Some(&alert_email_sender)
        } else {
            None
        };

    match hot::db::alert::process_single_alert_delivery(
        &db,
        &http_client,
        hot::outbound::DestinationPolicy::for_alert_delivery(&worker_conf),
        email_sender_ref,
        &alert_email_config,
        &alert_msg.body.alert_delivery_id,
    )
    .await
    {
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
            Ok(())
        }
        Err(e) => {
            tracing::error!(
                "Alert delivery {} processing error: {}",
                alert_msg.body.alert_delivery_id,
                e
            );
            Err(worker_error(e.to_string()))
        }
    }
}

async fn process_email_message(
    worker_id: usize,
    db: Arc<DatabasePool>,
    worker_conf: Val,
    message: Message,
) -> Result<(), WorkerError> {
    let db = Arc::new(db.with_scope(DatabaseScope::Maintenance {
        operation: MaintenanceOperation::Email,
        org_id: None,
    }));
    let email_msg: hot::lang::event::queue::EmailMessage =
        message.try_into().map_err(|e: String| worker_error(e))?;

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
        )
        .await;
        return Ok(());
    }

    let email = hot::email::Email {
        to: email_msg.body.to_address.clone(),
        subject: email_msg.body.subject.clone(),
        html: email_msg.body.html_body.clone(),
        text: email_msg.body.text_body.clone(),
    };

    match sender
        .send_email_with_from(&email, &email_msg.body.from_address)
        .await
    {
        Ok(()) => {
            let _ = hot::db::email_queue::EmailQueueEntry::mark_sent(
                &db,
                &email_msg.body.email_queue_id,
            )
            .await;
            Ok(())
        }
        Err(e) => {
            tracing::warn!(
                "hot.dev: WORKER {} failed to send app email {} (to: {}), will retry via queue: {}",
                worker_id,
                email_msg.body.email_queue_id,
                email_msg.body.to_address,
                e
            );
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            Err(worker_error(format!(
                "app email send failed for {}: {}",
                email_msg.body.email_queue_id, e
            )))
        }
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

    const MAX_ORPHANED_MESSAGE_BYTES: u64 = 8 * 1024 * 1024;

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
        let decoder = match zstd::Decoder::new(raw_bytes) {
            Ok(d) => d,
            Err(_) => return None,
        };
        let mut buf = Vec::new();
        if decoder
            .take(MAX_ORPHANED_MESSAGE_BYTES + 1)
            .read_to_end(&mut buf)
            .is_err()
            || buf.len() as u64 > MAX_ORPHANED_MESSAGE_BYTES
        {
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

/// The failure of a run whose event its stopped worker left and queue
/// recovery dead-lettered. The SQLite event queue dead-letters only events
/// that stopped their worker while running alone (see `CrashQuarantine`).
const CRASHED_EVENT_DEAD_LETTERED: &str = "The worker stopped while running this event, each time it ran alone, until the event exhausted its queue retry limit";

/// What queue recovery did with orphaned event messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OrphanedEvents {
    /// Requeued: each event runs again, under the same handler run ids.
    Requeued,
    /// Dead-lettered: the events don't run again.
    DeadLettered,
}

async fn reconcile_orphaned_event_runs(
    db: &DatabasePool,
    messages: &[RecoveredQueueMessage],
    reason: &str,
    orphaned: OrphanedEvents,
) {
    let orphans = messages
        .iter()
        .filter_map(|message| extract_ids_from_orphaned_data(message.payload()));
    settle_orphaned_event_runs(db, orphans, reason, orphaned).await;
}

/// A dead-lettered event is never run again: fail the runs its earlier
/// deliveries left running (a worker that died mid-run) instead of leaving
/// them to the 25-hour reaper. Only its own runs: unlike after a crash, the
/// run that sent it, in the same stream, may well still be running.
async fn settle_dead_lettered_event_runs(db: &DatabasePool, payload: &[u8]) {
    let Some(ids) = extract_ids_from_orphaned_data(payload) else {
        return;
    };
    match hot::db::run::Run::fail_orphaned_runs_by_event_id(
        db,
        &ids.event_id,
        "The event exhausted its delivery retries and was moved to the dead-letter queue",
    )
    .await
    {
        Ok(0) => {}
        Ok(failed) => info!(
            "hot.dev: WORKER marked {} run(s) of dead-lettered event {} as failed",
            failed, ids.event_id
        ),
        Err(error) => warn!(
            "hot.dev: WORKER failed to settle the runs of dead-lettered event {}: {}",
            ids.event_id, error
        ),
    }
}

async fn settle_orphaned_event_runs(
    db: &DatabasePool,
    orphans: impl IntoIterator<Item = OrphanedMessageIds>,
    reason: &str,
    orphaned: OrphanedEvents,
) {
    let orphans: ahash::AHashMap<uuid::Uuid, Option<uuid::Uuid>> = orphans
        .into_iter()
        .map(|ids| (ids.event_id, ids.stream_id))
        .collect();

    // Each event's own runs first: a requeued event's redelivered handlers
    // reuse their rows, so those are held for them rather than failed.
    let mut unsettled_streams = ahash::AHashSet::new();
    for (event_id, stream_id) in &orphans {
        let own_runs = match orphaned {
            OrphanedEvents::Requeued => {
                hot::db::run::Run::hold_orphaned_runs_for_redelivery(db, event_id).await
            }
            OrphanedEvents::DeadLettered => {
                hot::db::run::Run::fail_orphaned_runs_by_event_id(db, event_id, reason).await
            }
        };
        match own_runs {
            Ok(0) => {}
            Ok(count) => info!(
                "hot.dev: WORKER marked {} orphaned run(s) as {} for event {}",
                count,
                if orphaned == OrphanedEvents::Requeued {
                    "pending retry"
                } else {
                    "failed"
                },
                event_id
            ),
            Err(error) => {
                warn!(
                    "hot.dev: WORKER failed to reconcile orphaned runs for event {}: {}",
                    event_id, error
                );
                // Failing its stream would fail the event's own runs too.
                unsettled_streams.extend(*stream_id);
            }
        }
    }

    // Then whatever else in the events' streams is still running.
    let streams: ahash::AHashSet<uuid::Uuid> = orphans
        .into_values()
        .flatten()
        .filter(|stream_id| !unsettled_streams.contains(stream_id))
        .collect();
    for stream_id in streams {
        match hot::db::run::Run::fail_orphaned_runs_by_stream_id(db, &stream_id, reason).await {
            Ok(failed) if failed > 0 => info!(
                "hot.dev: WORKER marked {} orphaned run(s) as failed for stream {}",
                failed, stream_id
            ),
            Ok(_) => {}
            Err(error) => warn!(
                "hot.dev: WORKER failed to mark orphaned runs as failed for stream {}: {}",
                stream_id, error
            ),
        }
    }
}

/// The run id of an event's handler: the same for every delivery of the
/// event, so a redelivery finds the runs earlier deliveries wrote. It derives
/// from what a redeploy keeps (the handler's project, namespace and name),
/// not from the deployment's `event_handler_id`, which every deploy and live
/// build refresh replaces.
fn event_handler_run_id(
    event_id: Uuid,
    project_id: Uuid,
    event_type: &str,
    handler: &EventHandler,
    retry_attempt: i16,
) -> Uuid {
    let identity = format!(
        "{event_id}:{project_id}:{event_type}:{}/{}:{retry_attempt}",
        handler.ns, handler.var
    );
    Uuid::new_v5(&Uuid::NAMESPACE_OID, identity.as_bytes())
}

fn event_run_type_id(event_type: &str) -> i16 {
    match event_type {
        "hot:call" => hot::db::run::RunType::Call.as_id(),
        "hot:schedule" => hot::db::run::RunType::Schedule.as_id(),
        _ => hot::db::run::RunType::Event.as_id(),
    }
}

/// The row the worker writes for an event handler run it executes.
fn event_handler_run_row(
    event_message: &EventMessage,
    build: &Build,
    run_id: Uuid,
) -> hot::db::run::UnstartedRun {
    let context = &event_message.body.execution_context;
    hot::db::run::UnstartedRun {
        run_id,
        env_id: event_message.body.event.env_id,
        stream_id: event_message.body.event.stream_id,
        build_id: build.build_id,
        run_type_id: event_run_type_id(&event_message.body.event.event_type),
        origin_run_id: context.origin_run_id,
        event_id: event_message.body.event.event_id,
        by_user_id: context.user_id.unwrap_or(build.created_by_user_id),
        retry_attempt: context.retry_attempt,
    }
}

/// Record a handler whose setup failed before its run started, so the
/// event's owner sees why it waits (`held`, for a redelivery) or why it
/// failed. Best-effort: the event's delivery goes on either way.
async fn record_unstarted_run(
    db: &DatabasePool,
    event_message: &EventMessage,
    build: &Build,
    run_id: Uuid,
    error: &str,
    held: bool,
) {
    let run = event_handler_run_row(event_message, build, run_id);
    if let Err(record_error) = hot::db::run::Run::record_unstarted_run(db, &run, error, held).await
    {
        warn!(
            %run_id,
            "hot.dev: WORKER could not record the setup failure of run {}: {}",
            run_id,
            record_error
        );
    }
}

/// Fail runs an earlier delivery left running or held whose handlers this
/// delivery doesn't run. Best-effort: the event's own handlers run either way.
async fn fail_unrun_earlier_deliveries(db: &DatabasePool, run_ids: &[uuid::Uuid]) {
    if run_ids.is_empty() {
        return;
    }
    if let Err(error) = hot::db::run::Run::fail_unrun_earlier_deliveries(
        db,
        run_ids,
        "Worker stopped during event processing; the redelivered event no longer runs this handler",
    )
    .await
    {
        warn!(
            "hot.dev: WORKER could not mark {} earlier run(s) as failed after their redelivered event dropped the handlers: {}",
            run_ids.len(),
            error
        );
    }
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
    tracing::debug!(
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
    match compile_event_bundle_to_cache(
        extracted_path,
        &paths,
        &bundle_cache,
        &manifest.bundle_name,
        manifest.cache_key.as_deref(),
        Some(manifest.file_hashes.clone()),
    ) {
        Ok(()) => {
            tracing::debug!(
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

struct PreparedBundleRuntime {
    extract_dir: PathBuf,
    build_data: Option<Vec<u8>>,
}

fn bundle_extract_dir(build_id: &Uuid) -> PathBuf {
    PathBuf::from(format!(".hot/run/build-{}", build_id.simple()))
}

fn bundle_bytecode_ready(extracted_path: &std::path::Path) -> bool {
    let cache_dir = extracted_path.join(".hot").join("cache");
    cache_dir.exists()
        && std::fs::read_dir(&cache_dir)
            .map(|entries| {
                entries
                    .flatten()
                    .any(|entry| entry.path().to_string_lossy().ends_with(".bc.zst"))
            })
            .unwrap_or(false)
}

fn get_completed_bundle_extracted_path(
    build: &Build,
    build_path_cache: &Arc<BuildPathCache>,
) -> Option<PathBuf> {
    if let Some(path) = build_path_cache.get(&build.build_id) {
        if BuildPathCache::is_extraction_complete(&path) && bundle_bytecode_ready(&path) {
            tracing::debug!(
                "ROUTING: Bundle {} found in memory cache at {:?}",
                build.build_id,
                path
            );
            return Some(path);
        }

        tracing::debug!(
            "ROUTING: Bundle {} in memory cache but bytecode not ready, removing",
            build.build_id
        );
        build_path_cache.remove(&build.build_id);
    }

    let extract_dir = bundle_extract_dir(&build.build_id);
    if BuildPathCache::is_extraction_complete(&extract_dir) && bundle_bytecode_ready(&extract_dir) {
        tracing::debug!(
            "ROUTING: Bundle {} found on disk with bytecode, adding to cache",
            build.build_id
        );
        build_path_cache.insert(build.build_id, extract_dir.clone());
        return Some(extract_dir);
    }

    None
}

fn bundle_source_paths(extracted_path: &std::path::Path) -> Result<Vec<String>, String> {
    let build_src_path = extracted_path.join("hot/src");
    if !build_src_path.exists() {
        return Err(format!(
            "Bundle source path {} does not exist",
            build_src_path.display()
        ));
    }

    let build_pkg_path = extracted_path.join("hot/pkg");
    let mut paths = vec![build_src_path.to_string_lossy().to_string()];
    if build_pkg_path.exists() {
        paths.push(build_pkg_path.to_string_lossy().to_string());
    }
    Ok(paths)
}

fn clear_bundle_bytecode_cache(bundle_cache_dir: &std::path::Path) {
    if bundle_cache_dir.exists()
        && let Ok(entries) = std::fs::read_dir(bundle_cache_dir)
    {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.to_string_lossy().ends_with(".bc.zst") {
                if let Err(e) = std::fs::remove_file(&path) {
                    tracing::warn!("Failed to remove stale bundle cache file {:?}: {}", path, e);
                } else {
                    tracing::debug!("Removed stale bundle cache file {:?}", path);
                }
            }
        }
    }
}

async fn compile_bundle_cache(
    extracted_path: PathBuf,
    manifest: hot::bundle::BundleManifest,
) -> Result<(), String> {
    // Bytecode compilation is CPU-bound. Run it on the blocking pool so it does
    // not stall the async runtime (and event consumption) during startup warm-up.
    tokio::task::spawn_blocking(move || {
        let paths = bundle_source_paths(&extracted_path)?;
        let bundle_cache_dir = extracted_path.join(".hot").join("cache");
        std::fs::create_dir_all(&bundle_cache_dir).map_err(|e| {
            format!(
                "Failed to create bundle cache directory {}: {}",
                bundle_cache_dir.display(),
                e
            )
        })?;
        clear_bundle_bytecode_cache(&bundle_cache_dir);

        let bundle_cache = hot::lang::cache::bytecode_cache::BytecodeCache::new(bundle_cache_dir);
        compile_event_bundle_to_cache(
            &extracted_path,
            &paths,
            &bundle_cache,
            &manifest.bundle_name,
            manifest.cache_key.as_deref(),
            Some(manifest.file_hashes.clone()),
        )
        .map_err(|e| {
            format!(
                "Failed to compile bundle '{}' bytecode cache: {}",
                manifest.bundle_name, e
            )
        })
    })
    .await
    .map_err(|e| format!("Bundle compile task panicked or was cancelled: {e}"))?
}

async fn retrieve_build_data(
    build: &Build,
    project: &Project,
    env: &Env,
    worker_conf: &Val,
) -> Result<Vec<u8>, String> {
    let storage = hot::storage::build_storage_from_config(worker_conf)
        .await
        .map_err(|e| format!("Failed to create build storage: {}", e))?;

    storage
        .retrieve_build(&build.build_id, &env.org_id, &project.env_id)
        .await
        .map_err(|e| {
            format!(
                "Failed to retrieve build {} from storage: {}",
                build.build_id, e
            )
        })
}

async fn prepare_bundle_runtime(
    db: &DatabasePool,
    build: &Build,
    project: &Project,
    env: &Env,
    worker_conf: &Val,
    build_path_cache: &Arc<BuildPathCache>,
    include_build_data: bool,
) -> Result<PreparedBundleRuntime, String> {
    if !build.is_bundle() {
        return Err(format!("Build {} is not a bundle build", build.build_id));
    }
    if let Some(reason) = build.runtime_version_error_message() {
        return Err(reason);
    }

    let mut build_data = None;
    let mut existing_path = get_bundle_extracted_path(build, build_path_cache).await;

    if existing_path.is_none() {
        let extraction_lock = build_path_cache.get_extraction_lock(&build.build_id);
        let _lock_guard = extraction_lock.lock().await;

        existing_path = get_completed_bundle_extracted_path(build, build_path_cache);
        if existing_path.is_none() {
            let extract_dir = bundle_extract_dir(&build.build_id);
            let mut file_lock = BuildPathCache::acquire_file_lock(&build.build_id).ok();
            let _file_lock_guard = file_lock.as_mut().and_then(|lock| lock.try_write().ok());

            if BuildPathCache::is_extraction_complete(&extract_dir) {
                let manifest = hot::bundle::read_bundle_manifest(&extract_dir)?;
                compile_bundle_cache(extract_dir.clone(), manifest).await?;
                build_path_cache.insert(build.build_id, extract_dir.clone());
                existing_path = Some(extract_dir);
            } else {
                let data = retrieve_build_data(build, project, env, worker_conf).await?;
                clear_partial_bundle_extraction(&extract_dir, _file_lock_guard.is_some())?;
                let prepared = async {
                    hot::bundle::extract_bundle_from_bytes(&data, &extract_dir).map_err(|e| {
                        format!(
                            "Failed to extract build {} to {}: {}",
                            build.build_id,
                            extract_dir.display(),
                            e
                        )
                    })?;
                    // Recorded before the version check, so a build this
                    // runtime can't run is known without fetching it again.
                    persist_bundle_manifest_versions(db, build, project, env, &extract_dir).await;
                    let manifest = hot::bundle::read_bundle_manifest(&extract_dir)?;
                    compile_bundle_cache(extract_dir.clone(), manifest).await
                }
                .await;
                if let Err(error) = prepared {
                    // A partial extraction would make every later one fail.
                    let _ = std::fs::remove_dir_all(&extract_dir);
                    return Err(error);
                }
                BuildPathCache::mark_extraction_complete(&extract_dir);
                build_path_cache.insert(build.build_id, extract_dir.clone());
                existing_path = Some(extract_dir);
                build_data = Some(data);
            }
        }
    }

    if include_build_data && build_data.is_none() {
        build_data = Some(retrieve_build_data(build, project, env, worker_conf).await?);
    }

    let extract_dir = existing_path.ok_or_else(|| {
        format!(
            "Build {} did not produce a prepared extraction path",
            build.build_id
        )
    })?;

    persist_bundle_manifest_versions(db, build, project, env, &extract_dir).await;

    Ok(PreparedBundleRuntime {
        extract_dir,
        build_data,
    })
}

async fn persist_bundle_manifest_versions(
    db: &DatabasePool,
    build: &Build,
    project: &Project,
    env: &Env,
    extract_dir: &std::path::Path,
) {
    let Ok(manifest) = hot::bundle::read_bundle_manifest_unchecked(extract_dir) else {
        return;
    };
    let compatibility = hot::version::check_runtime_version_compatibility(
        Some(manifest.engine_version.as_str()),
        Some(manifest.hot_std_version.as_str()),
        hot::version::current_runtime_version(),
    );
    if let Some(warning) = &compatibility.warning {
        warn!(
            build_id = %build.build_id,
            project_id = %project.project_id,
            env_id = %env.env_id,
            bundle_engine_version = %manifest.engine_version,
            bundle_hot_std_version = %manifest.hot_std_version,
            runtime_version = %warning.runtime_version,
            "ROUTING: Bundle runtime version drift detected"
        );
    }
    if build.engine_version.as_deref() == Some(manifest.engine_version.as_str())
        && build.hot_std_version.as_deref() == Some(manifest.hot_std_version.as_str())
    {
        return;
    }
    if let Err(e) = Build::update_manifest_versions(
        db,
        &build.build_id,
        Some(manifest.engine_version.as_str()),
        Some(manifest.hot_std_version.as_str()),
    )
    .await
    {
        if Build::manifest_version_metadata_unavailable(&e) {
            debug!(
                "ROUTING: Manifest version metadata columns are not available yet; skipping persist for build {}",
                build.build_id
            );
        } else {
            warn!(
                "ROUTING: Failed to persist manifest versions for build {}: {}",
                build.build_id, e
            );
        }
    }
}

/// Advisory lock id electing a single worker to perform startup deployment
/// recovery (the DB-mutating manifest reload + activation). Warm-up of the
/// local cache is read-only and always runs on every worker regardless of this
/// lock, because each worker process has its own on-disk extraction cache.
const WORKER_RUNTIME_REPAIR_LOCK_ID: i64 = 4_849_672_113_600_037;

async fn startup_repair_and_warm_bundle_runtimes(
    db: &DatabasePool,
    worker_conf: &Val,
    build_path_cache: &Arc<BuildPathCache>,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    let limit = worker_conf
        .get_int_or_default("worker.startup-runtime-build-limit", 1_000)
        .max(1);

    if *shutdown_rx.borrow() {
        debug!("hot.dev: WORKER startup runtime repair skipped during shutdown");
        return;
    }

    // Phase 1 (every worker): warm this process's local cache for builds the
    // database already considers ready. This is read-only and must run on each
    // worker because the on-disk extraction/bytecode cache is per-process.
    tokio::select! {
        _ = warm_ready_bundle_runtimes(db, worker_conf, build_path_cache, limit) => {}
        changed = shutdown_rx.changed() => {
            if changed.is_err() || *shutdown_rx.borrow() {
                info!("hot.dev: WORKER startup warm-up stopped for shutdown");
                return;
            }
        }
    }

    if *shutdown_rx.borrow() {
        info!("hot.dev: WORKER startup recovery skipped for shutdown");
        return;
    }

    // Phase 2 (single elected worker): recover deployments interrupted by a
    // prior worker rollout. This mutates the database (manifest reload +
    // activation), so we elect one owner via an advisory lock to avoid every
    // worker performing the same churn concurrently.
    match hot::db::try_acquire_advisory_lock(db, WORKER_RUNTIME_REPAIR_LOCK_ID).await {
        Ok(Some(lock)) => {
            if *shutdown_rx.borrow() {
                info!("hot.dev: WORKER startup recovery skipped for shutdown");
                lock.release().await;
                return;
            }

            tokio::select! {
                _ = backfill_deployed_bundle_manifest_versions(db, worker_conf, build_path_cache, limit) => {}
                changed = shutdown_rx.changed() => {
                    if changed.is_err() || *shutdown_rx.borrow() {
                        info!("hot.dev: WORKER startup version backfill stopped for shutdown");
                        lock.release().await;
                        return;
                    }
                }
            }

            if *shutdown_rx.borrow() {
                info!("hot.dev: WORKER startup recovery skipped for shutdown");
                lock.release().await;
                return;
            }

            tokio::select! {
                _ = recover_interrupted_bundle_deployments(db, worker_conf, build_path_cache, limit) => {}
                changed = shutdown_rx.changed() => {
                    if changed.is_err() || *shutdown_rx.borrow() {
                        info!("hot.dev: WORKER startup recovery stopped for shutdown");
                    }
                }
            }
            lock.release().await;
        }
        Ok(None) => {
            debug!(
                "hot.dev: WORKER startup deployment recovery skipped (another worker holds the repair lock)"
            );
        }
        Err(e) => {
            warn!(
                "hot.dev: WORKER startup deployment recovery skipped (could not acquire repair lock): {}",
                e
            );
        }
    }
}

async fn backfill_deployed_bundle_manifest_versions(
    db: &DatabasePool,
    worker_conf: &Val,
    build_path_cache: &Arc<BuildPathCache>,
    limit: i64,
) {
    let builds = match Build::get_deployed_bundle_builds_missing_versions(db, limit).await {
        Ok(builds) => builds,
        Err(e) => {
            if Build::manifest_version_metadata_unavailable(&e) {
                debug!(
                    "hot.dev: WORKER startup version backfill skipped because manifest version metadata columns are not available yet"
                );
            } else {
                warn!(
                    "hot.dev: WORKER startup version backfill could not list builds: {}",
                    e
                );
            }
            return;
        }
    };

    if builds.is_empty() {
        debug!("hot.dev: WORKER startup version backfill found no bundle builds missing versions");
        return;
    }

    info!(
        "hot.dev: WORKER startup version backfill preparing {} deployed bundle build(s)",
        builds.len()
    );

    let mut backfilled_count = 0usize;
    let mut failed_count = 0usize;
    for build in builds {
        let (project, env) = match resolve_build_project_env(db, &build).await {
            Ok(pair) => pair,
            Err(e) => {
                failed_count += 1;
                warn!(
                    "hot.dev: WORKER startup version backfill skipped build {}: {}",
                    build.build_id, e
                );
                continue;
            }
        };
        let scoped_db = db.with_scope(DatabaseScope::Environment {
            org_id: env.org_id,
            env_id: env.env_id,
        });
        let db = &scoped_db;

        match prepare_bundle_runtime(
            db,
            &build,
            &project,
            &env,
            worker_conf,
            build_path_cache,
            false,
        )
        .await
        {
            Ok(_) => backfilled_count += 1,
            Err(e) => {
                failed_count += 1;
                warn!(
                    "hot.dev: WORKER startup version backfill failed for build {} (project {}): {}",
                    build.build_id, project.name, e
                );
            }
        }
    }

    info!(
        "hot.dev: WORKER startup version backfill finished (backfilled={}, failed={})",
        backfilled_count, failed_count
    );
}

/// Resolve the owning project and env for a build, mapping lookup errors to a
/// human-readable string for log lines.
async fn resolve_build_project_env(
    db: &DatabasePool,
    build: &Build,
) -> Result<(Project, Env), String> {
    let project = Project::get_project(db, &build.project_id)
        .await
        .map_err(|e| format!("project lookup failed: {e}"))?;
    let env = Env::get_env(db, &project.env_id)
        .await
        .map_err(|e| format!("env lookup failed: {e}"))?;
    if !hot::db::Org::is_environment_admitted(db, &env.env_id)
        .await
        .map_err(|error| error.to_string())?
    {
        return Err("environment is suspended; deployment remains pending".into());
    }
    Ok((project, env))
}

/// Warm this worker process's local extraction/bytecode cache for every build
/// the database already reports as ready. Read-only; safe to run on all workers.
async fn warm_ready_bundle_runtimes(
    db: &DatabasePool,
    worker_conf: &Val,
    build_path_cache: &Arc<BuildPathCache>,
    limit: i64,
) {
    let builds = match Build::get_ready_bundle_runtime_builds(db, limit).await {
        Ok(builds) => builds,
        Err(e) => {
            warn!(
                "hot.dev: WORKER startup warm-up could not list ready builds: {}",
                e
            );
            return;
        }
    };

    if builds.is_empty() {
        debug!("hot.dev: WORKER startup warm-up found no ready bundle builds");
        return;
    }

    info!(
        "hot.dev: WORKER startup warm-up preparing {} ready bundle build(s)",
        builds.len()
    );

    let mut prepared_count = 0usize;
    let mut failed_count = 0usize;
    let mut awaiting_redeploy = 0usize;

    for build in builds {
        if build.runtime_version_error_message().is_some() {
            awaiting_redeploy += 1;
            continue;
        }
        let (project, env) = match resolve_build_project_env(db, &build).await {
            Ok(pair) => pair,
            Err(e) => {
                failed_count += 1;
                warn!(
                    "hot.dev: WORKER startup warm-up skipped build {}: {}",
                    build.build_id, e
                );
                continue;
            }
        };
        let scoped_db = db.with_scope(DatabaseScope::Environment {
            org_id: env.org_id,
            env_id: env.env_id,
        });
        let db = &scoped_db;

        match prepare_bundle_runtime(
            db,
            &build,
            &project,
            &env,
            worker_conf,
            build_path_cache,
            false,
        )
        .await
        {
            Ok(_) => prepared_count += 1,
            Err(e) => {
                failed_count += 1;
                warn!(
                    "hot.dev: WORKER startup warm-up failed to prepare build {} (project {}): {}",
                    build.build_id, project.name, e
                );
            }
        }
    }

    info!(
        "hot.dev: WORKER startup warm-up finished (prepared={}, failed={})",
        prepared_count, failed_count
    );
    if awaiting_redeploy > 0 {
        warn!(
            "hot.dev: WORKER found {} deployed build(s) made with another major version of Hot. Their work waits until they're redeployed: run `hot db redeploy`.",
            awaiting_redeploy
        );
    }
}

/// Re-prepare and activate bundle deployments left interrupted (pending/loading/
/// failed with no ready build) by a prior worker rollout. Mutates the database,
/// so callers MUST run this under [`WORKER_RUNTIME_REPAIR_LOCK_ID`].
async fn recover_interrupted_bundle_deployments(
    db: &DatabasePool,
    worker_conf: &Val,
    build_path_cache: &Arc<BuildPathCache>,
    limit: i64,
) {
    let builds = match Build::get_recoverable_bundle_deployments(db, limit).await {
        Ok(builds) => builds,
        Err(e) => {
            warn!(
                "hot.dev: WORKER startup recovery could not list interrupted deployments: {}",
                e
            );
            return;
        }
    };

    if builds.is_empty() {
        debug!("hot.dev: WORKER startup recovery found no interrupted bundle deployments");
        return;
    }

    info!(
        "hot.dev: WORKER startup recovery preparing {} interrupted bundle deployment(s)",
        builds.len()
    );

    let mut repaired_count = 0usize;
    let mut failed_count = 0usize;

    for build in builds {
        let (project, env) = match resolve_build_project_env(db, &build).await {
            Ok(pair) => pair,
            Err(e) => {
                failed_count += 1;
                warn!(
                    "hot.dev: WORKER startup recovery skipped build {}: {}",
                    build.build_id, e
                );
                continue;
            }
        };
        let scoped_db = db.with_scope(DatabaseScope::Environment {
            org_id: env.org_id,
            env_id: env.env_id,
        });
        let db = &scoped_db;

        let prepared = match prepare_bundle_runtime(
            db,
            &build,
            &project,
            &env,
            worker_conf,
            build_path_cache,
            true,
        )
        .await
        {
            Ok(prepared) => prepared,
            Err(e) => {
                failed_count += 1;
                warn!(
                    "hot.dev: WORKER startup recovery failed to prepare build {} (project {}): {}",
                    build.build_id, project.name, e
                );
                continue;
            }
        };

        let Some(build_data) = prepared.build_data.as_deref() else {
            failed_count += 1;
            warn!(
                "hot.dev: WORKER startup recovery could not activate build {}: missing build data",
                build.build_id
            );
            continue;
        };

        if let Err(e) =
            hot::build::load_build_manifest_data(db, &build.build_id, &project.env_id, build_data)
                .await
        {
            failed_count += 1;
            warn!(
                "hot.dev: WORKER startup recovery could not load manifest for build {}: {}",
                build.build_id, e
            );
            continue;
        }

        let deployment_user_id = build.updated_by_user_id.unwrap_or(build.created_by_user_id);
        match Build::activate_prepared_build(db, &build.build_id, &deployment_user_id).await {
            Ok(true) => {
                repaired_count += 1;
                info!(
                    "hot.dev: WORKER startup recovery activated build {} for project {} from {}",
                    build.build_id,
                    project.name,
                    prepared.extract_dir.display()
                );
            }
            Ok(false) => {
                debug!(
                    "hot.dev: WORKER startup recovery skipped activation for build {} (already ready, stale, or inactive)",
                    build.build_id
                );
            }
            Err(e) => {
                failed_count += 1;
                warn!(
                    "hot.dev: WORKER startup recovery could not activate build {}: {}",
                    build.build_id, e
                );
            }
        }
    }

    info!(
        "hot.dev: WORKER startup recovery finished (repaired={}, failed={})",
        repaired_count, failed_count
    );
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
    tracing::debug!(
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
            tracing::debug!(
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

/// Build the qualified agent type name stored on runs from handler/schedule meta.
///
/// Hot source writes `meta {agent: TeamAgent}`; manifest meta resolves the type
/// reference to a short name like `"TeamAgent"`. Agent health matches runs using
/// `{namespace}/{type_name}` from the deployed agent table.
fn agent_type_from_meta(meta: &JsonValue, namespace: &str) -> Option<String> {
    let agent_name = meta.get("agent")?.as_str()?;
    if agent_name.contains('/') {
        Some(agent_name.to_string())
    } else {
        Some(format!("{}/{}", namespace, agent_name))
    }
}

/// Extract the qualified agent type name from an event handler's meta.
fn extract_agent_type_from_handler(handler: &EventHandler) -> Option<String> {
    handler
        .meta
        .as_ref()
        .and_then(|meta| agent_type_from_meta(meta, &handler.ns))
}

/// Read the `schedule_id` of a hot:schedule event. Events from schedulers
/// before 3.0.1 carry it only in the synthetic schedule arg; a dynamic
/// schedule's args are the user's own.
fn extract_schedule_id_from_event_data(event_data: &Val) -> Option<Uuid> {
    let schedule_id = match event_data.get("schedule_id") {
        Some(schedule_id) => schedule_id,
        None => {
            let Val::Vec(args_vec) = event_data.get("args")? else {
                return None;
            };
            args_vec.first()?.get("schedule_id")?
        }
    };
    match schedule_id {
        Val::Str(s) => Uuid::parse_str(s.as_ref()).ok(),
        _ => None,
    }
}

/// Scheduled runs execute through the generic `schedule-event-handler`, but they
/// should attribute agent health to the target scheduled function's meta.
async fn extract_agent_type_from_scheduled_event(
    db: &DatabasePool,
    build_id: &Uuid,
    event_data: &Val,
) -> Option<String> {
    if let Some(schedule_id) = extract_schedule_id_from_event_data(event_data)
        && let Ok(schedule) = Schedule::get_schedule(db, &schedule_id).await
        && let Some(ref meta) = schedule.meta
        && let Some(agent_type) = agent_type_from_meta(meta, &schedule.ns)
    {
        return Some(agent_type);
    }

    let target_fn = extract_target_function_from_event(event_data)?;
    let (ns, var) = target_fn.rsplit_once('/')?;

    if let Ok(schedule) = Schedule::get_active_schedule_by_build_fn(db, build_id, ns, var).await
        && let Some(ref meta) = schedule.meta
    {
        return agent_type_from_meta(meta, &schedule.ns);
    }

    None
}

async fn resolve_agent_type_for_execution(
    db: &DatabasePool,
    build_id: &Uuid,
    event_handler: &EventHandler,
    event_message: &EventMessage,
) -> Option<String> {
    if let Some(agent_type) = extract_agent_type_from_handler(event_handler) {
        return Some(agent_type);
    }

    if event_message.body.event.event_type == "hot:schedule" {
        return extract_agent_type_from_scheduled_event(
            db,
            build_id,
            &event_message.body.event.event_data,
        )
        .await;
    }

    None
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

fn build_matches_exact_target(candidate: Uuid, target: Option<Uuid>) -> bool {
    target.is_none_or(|target| candidate == target)
}

/// Get the extracted path for a bundle build.
/// Returns None if the bundle is not yet extracted or extraction failed.
async fn get_bundle_extracted_path(
    build: &Build,
    build_path_cache: &Arc<BuildPathCache>,
) -> Option<std::path::PathBuf> {
    if let Some(path) = get_completed_bundle_extracted_path(build, build_path_cache) {
        return Some(path);
    }

    // Check disk
    let extract_dir = bundle_extract_dir(&build.build_id);
    tracing::debug!(
        "ROUTING: Checking for extracted bundle at {:?} (exists={})",
        extract_dir,
        extract_dir.exists()
    );

    if BuildPathCache::is_extraction_complete(&extract_dir) {
        // Extraction is marked complete but bytecode is missing, likely because
        // compilation is in progress or a prior compile failed. Wait for any
        // active extractor/compiler, then re-check without recursively waiting.
        tracing::debug!(
            "ROUTING: Bundle {} extracted but bytecode not ready, waiting for lock",
            build.build_id
        );
        let extraction_lock = build_path_cache.get_extraction_lock(&build.build_id);
        match tokio::time::timeout(std::time::Duration::from_secs(5), extraction_lock.lock()).await
        {
            Ok(_guard) => {
                if let Some(path) = get_completed_bundle_extracted_path(build, build_path_cache) {
                    tracing::debug!(
                        "ROUTING: Bundle {} bytecode ready after lock wait",
                        build.build_id
                    );
                    return Some(path);
                }

                tracing::warn!(
                    "ROUTING: Bundle {} bytecode still not ready after lock wait",
                    build.build_id
                );
            }
            Err(_) => {
                tracing::warn!(
                    "ROUTING: Bundle {} timed out waiting for extraction lock",
                    build.build_id
                );
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
                if let Some(path) = get_completed_bundle_extracted_path(build, build_path_cache) {
                    tracing::debug!(
                        "ROUTING: Bundle {} ready after waiting for extraction",
                        build.build_id
                    );
                    return Some(path);
                }

                tracing::warn!(
                    "ROUTING: Bundle {} still not ready after waiting",
                    build.build_id
                );
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
/// An exact target build takes precedence; target_project_id remains the
/// compatibility tie-breaker for events that predate exact build routing.
async fn find_build_for_function(
    db: &DatabasePool,
    env_id: &Uuid,
    function_name: &str,
    cache: &hot::lang::cache::bytecode_cache::BytecodeCache,
    worker_conf: &Val,
    target: (Option<Uuid>, Option<Uuid>),
    build_path_cache: &Arc<BuildPathCache>,
) -> Result<Option<RoutingResult>, String> {
    let (target_build_id, target_project_id) = target;
    let deployed_builds = find_all_deployed_builds_for_env(db, env_id).await?;

    // Clean up memory cache: remove entries for builds that are no longer deployed
    let deployed_build_ids: ahash::AHashSet<Uuid> =
        deployed_builds.iter().map(|b| b.build_id).collect();
    build_path_cache.retain_only(&deployed_build_ids);

    // Find ALL builds that have this function
    let mut matching_builds: Vec<(Build, String)> = Vec::new();
    // Builds that could not be inspected; the function may be in one.
    let mut unavailable_builds = 0usize;

    for build in deployed_builds {
        if !build_matches_exact_target(build.build_id, target_build_id) {
            continue;
        }
        // Get project for this build to generate cache key
        let project = Project::get_project(db, &build.project_id)
            .await
            .map_err(|e| format!("Failed to get project: {}", e))?;

        // Route based on explicit build type - no fall-throughs
        match build.build_type.as_str() {
            "bundle" if build.runtime_version_error_message().is_some() => {
                tracing::debug!(
                    "ROUTING: Bundle build {} (project {}) awaits a redeploy for this runtime, skipping",
                    build.build_id,
                    project.name
                );
                continue;
            }
            "bundle" => {
                // BUNDLE BUILD: Must have an extracted path with manifest and cache
                let mut extracted_path = get_bundle_extracted_path(&build, build_path_cache).await;

                // If bundle not extracted, try to extract it on demand
                if extracted_path.is_none() {
                    tracing::debug!(
                        "ROUTING: Bundle build {} (project {}) not extracted yet, extracting on demand...",
                        build.build_id,
                        project.name
                    );

                    let env = match hot::db::Env::get_env(db, &project.env_id).await {
                        Ok(env) => env,
                        Err(e) => {
                            tracing::warn!(
                                "ROUTING: Failed to get env for bundle {} extraction: {}",
                                build.build_id,
                                e
                            );
                            unavailable_builds += 1;
                            continue;
                        }
                    };

                    match prepare_bundle_runtime(
                        db,
                        &build,
                        &project,
                        &env,
                        worker_conf,
                        build_path_cache,
                        false,
                    )
                    .await
                    {
                        Ok(prepared) => {
                            tracing::debug!(
                                "ROUTING: Prepared bundle {} at {:?}",
                                build.build_id,
                                prepared.extract_dir
                            );
                            extracted_path = Some(prepared.extract_dir);
                        }
                        Err(e) => {
                            tracing::warn!(
                                "ROUTING: Failed to prepare bundle {} on demand: {}",
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
                    unavailable_builds += 1;
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
                                tracing::debug!(
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
                                    unavailable_builds += 1;
                                }
                            }
                        } else {
                            tracing::warn!(
                                "ROUTING: Bundle build {} manifest has no cache_key",
                                build.build_id
                            );
                            unavailable_builds += 1;
                        }
                    }
                    Err(e) => {
                        tracing::warn!(
                            "ROUTING: Failed to read manifest for bundle build {} at {:?}: {}",
                            build.build_id,
                            extracted_path,
                            e
                        );
                        unavailable_builds += 1;
                    }
                }
            }
            "live" => {
                if let Err(error) =
                    hot::profile::authorized_live_project(db, worker_conf, &build).await
                {
                    tracing::warn!(build_id = %build.build_id, %error, "Skipping live build outside host profile");
                    continue;
                }
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
                    unavailable_builds += 1;
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
                    tracing::debug!(
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
                    if check_result != CacheCheckResult::NotFound {
                        unavailable_builds += 1;
                    }
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
        // `None` claims no deployed build has the function, which fails the
        // call; a build that could not be inspected leaves that open.
        if unavailable_builds > 0 {
            return Err(format!(
                "{} deployed build(s) could not be inspected for it",
                unavailable_builds
            ));
        }
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

/// A runnable `once` handler for a hot:call whose function no deployed
/// build has, preferring the call's target project. Its run fails resolving
/// the function, so the caller gets a run:fail instead of waiting out its
/// timeout.
async fn handler_for_missing_function(
    db: &DatabasePool,
    once_handlers: &[EventHandler],
    target_project_id: Option<Uuid>,
) -> Result<Option<EventHandler>, String> {
    let mut first_runnable = None;
    for handler in once_handlers {
        let build = Build::get_build(db, &handler.build_id)
            .await
            .map_err(|e| format!("Cannot resolve handler build: {e}"))?;
        if !build.deployed
            || !build.active
            || build.runtime_status != Build::RUNTIME_STATUS_READY
            || build.runtime_version_error_message().is_some()
        {
            continue;
        }
        if target_project_id == Some(build.project_id) {
            return Ok(Some(handler.clone()));
        }
        first_runnable.get_or_insert_with(|| handler.clone());
    }
    Ok(first_runnable)
}

/// Execute a single event handler with isolated build context
#[allow(clippy::too_many_arguments)]
async fn execute_single_event_handler(
    db: &DatabasePool,
    build: &Build,
    env_id: &Uuid,
    worker_conf: &Val,
    event_handler: &EventHandler,
    event_message: &EventMessage,
    emitter: Option<std::sync::Arc<dyn EngineEventEmitter>>,
    event_publisher: Option<std::sync::Arc<dyn EventPublisher>>,
    encryption: Option<Arc<ContextEncryption>>,
    cache: Arc<hot::lang::cache::bytecode_cache::BytecodeCache>,
    shutdown_coordinator: Arc<ShutdownCoordinator>,
    run_id: Uuid,
    build_path_cache: Arc<BuildPathCache>,
    dev_context_storage: Option<DevContextStorage>,
    stream_publisher: Option<Arc<StreamPubSub>>,
    task_queue: Arc<ProcessingQueue<TaskRequest>>,
    execution_services: Arc<WorkerExecutionServices>,
    blocking_execution_slots: Arc<Semaphore>,
    claim: Option<watch::Receiver<Option<ClaimLoss>>>,
) -> Result<(), String> {
    let (setup_complete_tx, setup_complete_rx) = tokio::sync::oneshot::channel();
    let (source_resolved_tx, source_resolved_rx) = tokio::sync::oneshot::channel();
    let run_started = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let execution = execute_single_event_handler_inner(
        db,
        build,
        env_id,
        worker_conf,
        event_handler,
        event_message,
        emitter,
        event_publisher,
        encryption,
        cache,
        shutdown_coordinator,
        run_id,
        build_path_cache,
        dev_context_storage,
        stream_publisher,
        task_queue,
        execution_services,
        blocking_execution_slots,
        claim,
        source_resolved_tx,
        setup_complete_tx,
        Arc::clone(&run_started),
    );

    await_event_handler_setup(
        execution,
        setup_complete_rx,
        source_resolved_rx,
        EVENT_SOURCE_RETRIEVAL_TIMEOUT,
        EVENT_SETUP_TIMEOUT,
    )
    .await
    .map_err(|error| {
        if run_started.load(std::sync::atomic::Ordering::Acquire) {
            error
        } else {
            format!("{error} {RUN_NOT_STARTED_TOKEN}")
        }
    })
}

async fn validate_event_origin_scope(
    db: &DatabasePool,
    env_id: Uuid,
    origin_run_id: Option<Uuid>,
) -> Result<(), String> {
    if let Some(origin_run_id) = origin_run_id {
        let origin = hot::db::Run::get_run(db, &origin_run_id)
            .await
            .map_err(|error| format!("Cannot resolve event origin run: {error}"))?;
        if origin.env_id != env_id {
            return Err("Event origin run belongs to another environment".into());
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn execute_single_event_handler_inner(
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
    execution_services: Arc<WorkerExecutionServices>,
    blocking_execution_slots: Arc<Semaphore>,
    claim: Option<watch::Receiver<Option<ClaimLoss>>>,
    source_resolved: tokio::sync::oneshot::Sender<()>,
    setup_complete: tokio::sync::oneshot::Sender<()>,
    // Set once the run has started (see `RunStartWatch`).
    run_has_started: Arc<std::sync::atomic::AtomicBool>,
) -> Result<(), String> {
    // The caller already registered `run_id`; from here this guard owns
    // unregistration on every exit path (see `RunRegistration` docs).
    let _run_registration = RunRegistration::new(Arc::clone(&shutdown_coordinator), run_id);

    let org_id = event_message
        .body
        .execution_context
        .org_id
        .ok_or("Hydrated event has no organization scope")?;
    if event_message.body.event.env_id != *_env_id {
        return Err("Execution environment does not match hydrated event".into());
    }
    let scoped_db = db.with_scope(DatabaseScope::Environment {
        org_id,
        env_id: *_env_id,
    });
    let db = &scoped_db;

    // TIMING: Track execution phases
    let timing_start = std::time::Instant::now();

    // Get project information for execution
    debug!("Executing event handler for build: {}", build.build_id);

    // Get project from build
    let project = Project::get_project(db, &build.project_id)
        .await
        .map_err(|e| format!("Failed to get project for build: {}", e))?;
    if project.env_id != event_message.body.event.env_id {
        return Err(format!(
            "Build {} belongs to environment {}, not event environment {}",
            build.build_id, project.env_id, event_message.body.event.env_id
        ));
    }

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

    // Payload retry metadata can override the queue context checked during
    // hydration. Validate the final origin before any execution/trace writes,
    // including on SQLite where a scoped handle does not filter reads.
    validate_event_origin_scope(db, project.env_id, origin_run_id).await?;

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
    let org_id = org_id.ok_or_else(|| {
        format!(
            "Cannot execute build {} without an authoritative org_id for tenant-scoped services",
            build.build_id
        )
    })?;

    // Extract retry attempt: use retry context from event data if present (for retried runs)
    let retry_attempt = if let Some(ref ctx) = retry_context {
        ctx.attempt
    } else {
        // For scheduler-initiated retries, the retry_attempt is carried
        // in the execution context (not in event data "retry" key).
        // Normal (non-retry) scheduled events have retry_attempt = 0.
        event_message.body.execution_context.retry_attempt
    };

    // Until the run's program loads and names its function's own budget, it
    // has the default one.
    let default_budget = RunBudget::new(
        None,
        get_run_timeout(worker_conf),
        get_max_run_timeout(worker_conf),
    );

    let mut _execution_context = ExecutionContext::new_with_event_and_origin(
        run_id,
        stream_id,
        run_type_id,
        event_message.body.execution_context.env_id,
        user_id,
        Some(org_id),
        Some(build.build_id), // Use resolved deployed build_id (not event context which may be None for API-published events)
        Some(event_message.body.event.event_id), // Include the event_id (now exists in DB)
        origin_run_id,        // From retry context or event type
    )
    .with_build_hash(Some(build.hash.clone()))
    .with_project(Some(build.project_id), Some(project.name.clone()))
    .with_retry_attempt(retry_attempt)
    .with_env_name(env_name)
    .with_org_slug(org_slug)
    .with_agent_type(
        resolve_agent_type_for_execution(db, &build.build_id, event_handler, event_message).await,
    );
    if let Some(queue_timing) = event_message.body.execution_context.queue_timing.clone() {
        _execution_context = _execution_context.with_queue_timing(queue_timing);
    }
    // An org can turn call tracing off (the `call_tracing` feature); the run
    // still records its lifecycle.
    if emitter.is_some() {
        _execution_context = _execution_context.with_trace_calls(
            hot::db::features::Features::call_tracing_for_org_cached(db, &org_id).await,
        );
    }

    // Set once the worker decides to stop this run (see `run_must_stop`), so
    // the failure the VM records as it unwinds names the reason.
    let stopped = Arc::new(OnceLock::new());
    let emitter_for_events = emitter.clone().map(|inner| {
        let inner = RunStartWatch::wrap(inner, run_id, Arc::clone(&run_has_started));
        Arc::new(StoppedRunEmitter {
            inner,
            run_id,
            stopped: Arc::clone(&stopped),
        }) as Arc<dyn EngineEventEmitter>
    });

    // Create event object with type and data structure
    let event_object = val!({
        "type": event_message.body.event.event_type.clone(),
        "data": event_message.body.event.event_data.clone()
    });

    // Build the function name in the format expected: namespace/variable
    let function_name = format!("{}/{}", event_handler.ns, event_handler.var);
    // The function whose `timeout` meta sets the run's budget: the target of
    // a call or a schedule, which run through hot-std's generic handlers, or
    // else the handler itself.
    let timeout_function = if event_message.body.event.event_type == "hot:call"
        || event_message.body.event.event_type == "hot:schedule"
    {
        extract_target_function_from_event(&event_message.body.event.event_data)
    } else {
        None
    }
    .unwrap_or_else(|| function_name.clone());

    debug!(
        "Executing event handler function: {} with event: {}",
        function_name, event_object
    );

    // Note: run:start event will be emitted by the VM when execution begins
    // We don't emit it here to avoid duplicate emissions
    //
    // CRITICAL: If ANY error occurs between here and VM.execute(), run:start will NEVER be emitted!
    // This means the run won't exist in the database even though the event was attempted.
    //
    // Stream and dashboard subscribers hear of the start from
    // `RunStartPublisher` once the VM has recorded it.
    let emitter_for_events = match (emitter_for_events, stream_publisher.as_ref()) {
        (Some(inner), Some(publisher)) => Some(Arc::new(RunStartPublisher {
            inner,
            publisher: Arc::clone(publisher),
            run_id,
            env_id: event_message.body.event.env_id,
            stream_id,
            event_id: event_message.body.execution_context.event_id,
            project_id: Some(build.project_id),
            fn_name: function_name.clone(),
            run_type: run_type_name(&event_message.body.event.event_type),
            published: std::sync::atomic::AtomicBool::new(false),
        }) as Arc<dyn EngineEventEmitter>),
        (emitter_for_events, _) => emitter_for_events,
    };

    // A live build may use host files and development context only after
    // matching the current profile. Deployed bundles must resolve their own bytes.
    if build.is_live() {
        hot::profile::authorized_live_project(db, worker_conf, build).await?;
    }

    // Get source paths for the project
    // Also track if this is a bundle build so we skip global dependency resolution
    // and use the bundle's own cache directory
    let (src_paths, is_bundle_build, bundle_extract_path, bundle_manifest) = if build.is_live() {
        // Live builds (including local dev) always execute from configured
        // project source paths. Do not attempt bundle storage/extraction for
        // these builds; storage may not exist in local dev.
        (
            hot::project::get_project_src_paths(worker_conf, &project.name),
            false,
            None,
            None,
        )
    } else if let Some(extracted_path) =
        get_completed_bundle_extracted_path(build, &build_path_cache)
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
        if let Some(extracted_path) = get_completed_bundle_extracted_path(build, &build_path_cache)
        {
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

                let env = Env::get_env(db, &project.env_id)
                    .await
                    .map_err(|error| format!("Cannot resolve bundle environment: {error}"))?;
                let build_data = retrieve_build_data(build, &project, &env, worker_conf).await?;
                let prep_permit = acquire_blocking_execution_slot(
                    Arc::clone(&blocking_execution_slots),
                    run_id,
                    BLOCKING_SLOT_ACQUIRE_TIMEOUT,
                )
                .await?;
                let prep_build_id = build.build_id;
                let prep_project_name = project.name.clone();
                let holds_file_lock = _file_lock_guard.is_some();
                let prepared = tokio::task::spawn_blocking(move || {
                    let _prep_permit = prep_permit;
                    extract_and_precompile_event_bundle(
                        build_data,
                        extract_dir,
                        prep_build_id,
                        prep_project_name,
                        holds_file_lock,
                    )
                })
                .await
                .map_err(|error| format!("Bundle preparation task failed: {error}"))??;
                build_path_cache.insert(build.build_id, prepared.extract_dir.clone());
                (
                    prepared.src_paths,
                    true,
                    Some(prepared.extract_dir),
                    prepared.manifest,
                )
            } // Close the else block for marker file check
        } // Close the else block for in-memory cache check
    };

    // Source resolution is done — every branch above (live paths, cached
    // extraction, and the cold storage-retrieval path) has produced its
    // src_paths. Hand the setup watchdog over from the roomy
    // EVENT_SOURCE_RETRIEVAL_TIMEOUT window to the tight EVENT_SETUP_TIMEOUT
    // window for the remaining preparation.
    let _ = source_resolved.send(());

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
        if build.is_live()
            && let Some(ref dev_ctx_storage) = dev_context_storage
        {
            match dev_ctx_storage.read() {
                Ok(dev_ctx_guard) => {
                    if let Some(ref dev_ctx) = *dev_ctx_guard {
                        debug!(
                            "Loading dev context storage with {} variables for project '{}' (as fallback/defaults)",
                            dev_ctx.len(),
                            project.name
                        );
                        merge_dev_context(
                            dev_ctx,
                            &mut storage,
                            &mut _execution_context.secret_value_hashes,
                        );
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

        // Load tenant-scoped database values from the short-lived cache.
        // Database values override hot/ctx.hot; the cache key includes the
        // authoritative org, environment, and project UUIDs.
        if let Some(encryption) = &encryption {
            debug!(
                "Loading context variables for project '{}' from database (encryption available)",
                project.name
            );
            let database_context = execution_services
                .context_for(
                    encryption,
                    TenantContextKey {
                        org_id,
                        env_id: project.env_id,
                        project_id: project.project_id,
                    },
                )
                .await;
            for (key, value) in database_context {
                storage.insert(key, value);
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

    // A Tokio spawn_blocking task cannot be forcefully cancelled. Keep the
    // permit inside the blocking closure so a timed-out, detached VM continues
    // to count against the process-wide ceiling until it actually exits.
    //
    // The acquire is bounded by BLOCKING_SLOT_ACQUIRE_TIMEOUT: this await runs
    // while the handler already holds an executor slot, so waiting forever here
    // would let accumulated detached threads pin the whole worker. On timeout
    // the error propagates to the event executor, which leaves the event
    // un-ACKed for redelivery (no run side effects have happened yet).
    let blocking_slot_wait_started = std::time::Instant::now();
    let blocking_execution_permit = acquire_blocking_execution_slot(
        blocking_execution_slots,
        run_id,
        BLOCKING_SLOT_ACQUIRE_TIMEOUT,
    )
    .await?;
    let blocking_slot_wait = blocking_slot_wait_started.elapsed();
    if blocking_slot_wait >= std::time::Duration::from_millis(100) {
        warn!(
            run_id = %run_id,
            wait_ms = blocking_slot_wait.as_millis(),
            "Event handler waited for a blocking execution slot; detached timed-out work is applying backpressure"
        );
    }

    let file_storage = execution_services.file_storage.clone();
    let store = execution_services
        .store_for(worker_conf, org_id, project.env_id)
        .await;
    let embedding_provider = execution_services.embedding_provider.clone();

    // From this point onward all potentially blocking user-code compilation
    // and execution runs in spawn_blocking behind its owned permit. Disarm the
    // setup watchdog immediately before starting the independent run budget.
    let _ = setup_complete.send(());
    // Without an emitter no run:start can be seen, and none records a run:
    // the run counts as started once its code goes to the VM.
    if emitter.is_none() {
        run_has_started.store(true, std::sync::atomic::Ordering::Release);
    }

    // The run budget starts now. Time spent queued behind detached blocking
    // work is backpressure, not part of the newly admitted handler's budget.
    // The blocking thread stamps the propagated parent deadline once it has
    // resolved the function's own budget (see `arm_run_budget`).
    let run_started = tokio::time::Instant::now();
    let run_started_at = chrono::Utc::now();
    let (budget_tx, budget_rx) = tokio::sync::oneshot::channel();
    let execution_context_for_events = _execution_context.clone();

    let external_cancel = if worker_conf.get_bool_or_default("worker.cancel-on-timeout", true) {
        let token = Arc::new(std::sync::atomic::AtomicBool::new(false));
        shutdown_coordinator.register_cancel_token(run_id, Arc::clone(&token));
        Some(token)
    } else {
        None
    };

    let handle = {
        // Clone/move all data needed for the blocking task
        let function_name = function_name.clone();
        let event_object = event_object.clone();
        let worker_conf = worker_conf.clone();
        let emitter_for_events = emitter_for_events.clone();
        let mut execution_context_for_events = execution_context_for_events.clone();
        let timeout_function = timeout_function.clone();
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
        let blocking_execution_permit = blocking_execution_permit;

        let panic_label = format!("worker:{}", function_name);
        let spawn_scheduled = std::time::Instant::now();
        tokio::task::spawn_blocking(move || {
            let _blocking_execution_permit = blocking_execution_permit;
            let _resource_scope =
                install_event_resource_scope(is_bundle_build, bundle_extract_path.as_deref());
            let spawn_entered = std::time::Instant::now();
            debug!(
                "TIMING [{}]: spawn_blocking entered",
                run_id_for_timing.as_simple()
            );
            debug!(
                "TIMING [{}]: spawn_blocking_queue_wait: {:?}",
                run_id_for_timing.as_simple(),
                spawn_entered.duration_since(spawn_scheduled)
            );

            // Wrap all user-code execution in run_user_code so any panic from
            // user-supplied Hot code (or from a third-party crate it triggers)
            // is converted to a structured UserCodePanic and surfaced as a
            // request error, rather than crashing the worker process.
            // spawn_blocking still provides defense-in-depth catching outside
            // this boundary in case run_user_code itself misses something.
            hot::lang::user_code::run_user_code(&panic_label, || {
                if let Some(cached) = cached_bytecode {
                    // ===== CACHE HIT: Call function directly with cached bytecode =====
                    debug!(
                        "TIMING [{}]: cache HIT - calling function (spawn wait: {:?})",
                        run_id_for_timing.as_simple(),
                        spawn_entered.elapsed()
                    );
                    debug!(
                        "✓ Calling function with cached bytecode for project '{}' (no parsing!)",
                        project_name
                    );
                    arm_run_budget(
                        &cached.ast_program,
                        &timeout_function,
                        &worker_conf,
                        run_started_at,
                        &mut execution_context_for_events,
                        budget_tx,
                    );
                    let call_started = std::time::Instant::now();
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
                    debug!(
                        "TIMING [{}]: call_function_with_cached_bytecode: {:?}",
                        run_id_for_timing.as_simple(),
                        call_started.elapsed()
                    );
                    debug!(
                        "TIMING [{}]: function execution complete (total spawn_blocking: {:?})",
                        run_id_for_timing.as_simple(),
                        spawn_entered.elapsed()
                    );
                    result
                } else {
                    // ===== CACHE MISS: Compile project, cache, then call function directly =====
                    // Use locking to prevent duplicate compilation across threads and processes
                    let cache_for_locking = hot::lang::cache::bytecode_cache::BytecodeCache::new(
                        effective_cache_dir.clone(),
                    );

                    // In-process lock (prevents thread races)
                    let compilation_lock = cache_for_locking.get_compilation_lock(&cache_key);
                    let lock_wait_started = std::time::Instant::now();
                    let _compilation_guard = compilation_lock.lock();
                    debug!(
                        "TIMING [{}]: bytecode_lock_wait: {:?}",
                        run_id_for_timing.as_simple(),
                        lock_wait_started.elapsed()
                    );

                    // Re-check cache after acquiring in-process lock (another thread may have just finished)
                    let reload_started = std::time::Instant::now();
                    let cached_after_lock =
                        if !cache_key.is_empty() && cache_for_locking.exists(&cache_key) {
                            cache_for_locking.load(&cache_key).ok()
                        } else {
                            None
                        };
                    debug!(
                        "TIMING [{}]: bytecode_reload_after_lock: {:?}",
                        run_id_for_timing.as_simple(),
                        reload_started.elapsed()
                    );
                    let cached_result = if let Some(cached) = cached_after_lock {
                        debug!(
                            "✓ Bytecode cache populated by another thread for '{}' while waiting",
                            project_name
                        );
                        Ok(cached)
                    } else {
                        // Cross-process file lock (prevents process races)
                        let file_lock_wait_started = std::time::Instant::now();
                        let mut file_lock = cache_for_locking.acquire_file_lock(&cache_key).ok();
                        let _file_lock_guard =
                            file_lock.as_mut().and_then(|lock| lock.try_write().ok());
                        debug!(
                            "TIMING [{}]: bytecode_file_lock_wait: {:?}",
                            run_id_for_timing.as_simple(),
                            file_lock_wait_started.elapsed()
                        );

                        // Re-check cache after acquiring file lock (another process may have just finished)
                        let reload_started = std::time::Instant::now();
                        let cached_after_file_lock =
                            if !cache_key.is_empty() && cache_for_locking.exists(&cache_key) {
                                cache_for_locking.load(&cache_key).ok()
                            } else {
                                None
                            };
                        debug!(
                            "TIMING [{}]: bytecode_reload_after_file_lock: {:?}",
                            run_id_for_timing.as_simple(),
                            reload_started.elapsed()
                        );
                        if let Some(cached) = cached_after_file_lock {
                            debug!(
                                "✓ Bytecode cache populated by another process for '{}' while waiting",
                                project_name
                            );
                            Ok(cached)
                        } else {
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
                            let compile_project_name: Option<&str> = if is_bundle_build {
                                None
                            } else {
                                Some(&project_name)
                            };
                            let compile_started = std::time::Instant::now();
                            let compile_result =
                                hot::lang::engine::Engine::compile_project_for_cache(
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
                            debug!(
                                "TIMING [{}]: compile_project_for_cache: {:?}",
                                run_id_for_timing.as_simple(),
                                compile_started.elapsed()
                            );

                            match compile_result {
                                Ok((_init_result, artifacts)) => {
                                    // Save to cache before calling function
                                    // Use bundle-specific cache for bundle builds, shared cache otherwise
                                    if !cache_key.is_empty() && !file_hashes.is_empty() {
                                        let metadata =
                                            hot::lang::cache::bytecode_cache::create_cache_metadata(
                                                &project_name,
                                                file_hashes.clone(),
                                                cache_key.clone(),
                                            );

                                        let cache_for_save =
                                            hot::lang::cache::bytecode_cache::BytecodeCache::new(
                                                effective_cache_dir.clone(),
                                            );
                                        // Bake tool/skill spec registries into the on-disk
                                        // cache so that fresh worker processes (and zip
                                        // builds) can serve `::hot::internal::mcp/schema-from-fn`
                                        // without recompiling.
                                        let spec_compiler = hot::lang::compiler::Compiler::new();
                                        let tool_specs =
                                            spec_compiler.build_tool_specs(&artifacts.ast_program);
                                        let skill_specs =
                                            spec_compiler.build_skill_specs(&artifacts.ast_program);
                                        let cache_save_started = std::time::Instant::now();
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
                                        debug!(
                                            "TIMING [{}]: bytecode_cache_save: {:?}",
                                            run_id_for_timing.as_simple(),
                                            cache_save_started.elapsed()
                                        );
                                    }

                                    // Convert artifacts to cached bytecode before releasing
                                    // the compile lock; handler execution happens below.
                                    let artifacts_convert_started = std::time::Instant::now();
                                    let cached =
                                        hot::lang::engine::Engine::artifacts_to_cached_bytecode(
                                            artifacts,
                                        );
                                    debug!(
                                        "TIMING [{}]: artifacts_to_cached_bytecode: {:?}",
                                        run_id_for_timing.as_simple(),
                                        artifacts_convert_started.elapsed()
                                    );
                                    Ok(cached)
                                }
                                Err(e) => Err(e),
                            }
                        }
                    };

                    drop(_compilation_guard);

                    match cached_result {
                        Ok(cached) => {
                            arm_run_budget(
                                &cached.ast_program,
                                &timeout_function,
                                &worker_conf,
                                run_started_at,
                                &mut execution_context_for_events,
                                budget_tx,
                            );
                            let call_started = std::time::Instant::now();
                            let result =
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
                                );
                            debug!(
                                "TIMING [{}]: call_function_with_cached_bytecode: {:?}",
                                run_id_for_timing.as_simple(),
                                call_started.elapsed()
                            );
                            debug!(
                                "TIMING [{}]: function execution complete (total spawn_blocking: {:?})",
                                run_id_for_timing.as_simple(),
                                spawn_entered.elapsed()
                            );
                            result
                        }
                        Err(e) => Err(e),
                    }
                }
            }) // end run_user_code
        }) // end spawn_blocking
    };

    // Hard run-timeout backstop.
    //
    // The run's budget is its function's `timeout` meta, else `run-timeout`,
    // capped at `max-run-timeout`; the claim fence can also stop it early (see
    // `run_must_stop`). The cooperative cancel token (when
    // `worker.cancel-on-timeout` is enabled) fires at the stop and asks the
    // VM to unwind. This select! guarantees
    // the executor slot is freed even when the VM cannot observe cancellation
    // (tight native loop, blocking syscall, or `cancel-on-timeout` disabled). On
    // the backstop we record the run as failed and detach the blocking thread:
    // it keeps running until it returns naturally, but no longer holds up the
    // worker. We still set the cancel token so a cooperative VM can exit.
    //
    // Caveat: the timeout failure flows through the retry policy, so a
    // retry-configured handler may start a fresh run while a detached,
    // non-cooperative orphan is still executing. Cooperative VMs unwind promptly
    // and avoid overlap; for non-cooperative ones the two can briefly run
    // concurrently, which is why event handlers must be idempotent.
    let must_stop = run_must_stop(
        run_started,
        default_budget,
        budget_rx,
        claim,
        stopped,
        external_cancel.clone(),
    );
    let vm_wait_started = std::time::Instant::now();
    let result = tokio::select! {
        joined = handle => {
            let joined_result = joined
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
                });
            debug!(
                "TIMING [{}]: vm_spawn_blocking_wait: {:?}",
                run_id.as_simple(),
                vm_wait_started.elapsed()
            );
            joined_result
        }
        stop = must_stop => {
            // The detached VM may still start the run: never run it again.
            run_has_started.store(true, std::sync::atomic::Ordering::Release);
            if let Some(token) = &external_cancel {
                token.store(true, std::sync::atomic::Ordering::Relaxed);
            }
            let stop_error = stop.error();
            error!(
                "Event handler '{}' stopped for event '{}' (run {}); freeing executor slot: {}",
                function_name, event_message.body.event.event_type, run_id, stop_error
            );
            // Route the timeout through the emitter so it is treated like any other
            // run failure: the database writer applies the handler/schedule retry
            // policy (a timed-out run may be retried) and publishes the standard
            // `run:failed` alert. This is intentional — a timeout is a failure.
            // The direct `fail_run` fallback (no emitter, e.g. in tests) is
            // best-effort and applies neither retry nor alerting.
            if !stop.records_failure() {
                // Another worker took the event over and records how the run ends.
            } else if let Some(ref em) = emitter {
                let failure = val!({
                    "$type": "::hot::run/Failure",
                    "$val": {
                        "msg": stop_error.clone(),
                        "err": Val::Null,
                    },
                });
                em.emit(hot::lang::emitter::EngineEvent::run_fail(
                    &execution_context_for_events,
                    failure,
                ));
                let flush_started = std::time::Instant::now();
                match bounded_run_result_write(RUN_RESULT_FLUSH_TIMEOUT, em.flush_run(run_id)).await {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => {
                        error!("Failed to flush timeout failure for run {}: {}", run_id, e);
                    }
                    Err(_) => {
                        error!(
                            run_id = %run_id,
                            timeout_secs = RUN_RESULT_FLUSH_TIMEOUT.as_secs(),
                            "Flushing timeout failure timed out; releasing executor slot"
                        );
                    }
                }
                debug!(
                    "TIMING [{}]: timeout_flush_run: {:?}",
                    run_id.as_simple(),
                    flush_started.elapsed()
                );
            } else {
                match bounded_run_result_write(
                    RUN_RESULT_FLUSH_TIMEOUT,
                    hot::db::Run::fail_run(db, &run_id, &stop_error),
                )
                .await
                {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => {
                        error!("Failed to mark run {} as timed out: {}", run_id, e);
                    }
                    Err(_) => {
                        error!(
                            run_id = %run_id,
                            timeout_secs = RUN_RESULT_FLUSH_TIMEOUT.as_secs(),
                            "Marking timed-out run failed to finish before executor release"
                        );
                    }
                }
            }
            // The RunRegistration guard unregisters the run as this return
            // unwinds — the run stops counting as active now, even though the
            // detached VM thread may keep executing.
            return Err(stop_error);
        }
    };

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
    if let Some(ref em) = emitter {
        let flush_started = std::time::Instant::now();
        match bounded_run_result_write(RUN_RESULT_FLUSH_TIMEOUT, em.flush_run(run_id)).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                tracing::warn!("Failed to flush emitter before stream publish: {}", e);
            }
            Err(_) => {
                tracing::warn!(
                    run_id = %run_id,
                    timeout_secs = RUN_RESULT_FLUSH_TIMEOUT.as_secs(),
                    "Emitter flush timed out before stream publish"
                );
            }
        }
        debug!(
            "TIMING [{}]: flush_run: {:?}",
            run_id.as_simple(),
            flush_started.elapsed()
        );
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

    // The RunRegistration guard unregisters the run as this function returns.
    Ok(())
}

pub const DEFAULT_SHUTDOWN_TIMEOUT_SECONDS: u64 = 60;

pub fn get_resolved_conf(conf: Val) -> Val {
    // Start with defaults
    let default_conf = val!({
        "threads": DEFAULT_WORKER_THREADS as i64,
        "run-timeout": DEFAULT_RUN_TIMEOUT_SECONDS as i64,
        "max-run-timeout": DEFAULT_MAX_RUN_TIMEOUT_SECONDS as i64,
        "shutdown-timeout": DEFAULT_SHUTDOWN_TIMEOUT_SECONDS as i64,
        "vm-concurrency": "auto",
        "vm-memory-mb": 256i64,
        "reserved-memory-mb": 512i64,
        "db-reserved-connections": 4i64,
        "cancel-on-timeout": true,
        "event-ordering": "current",
        "handler-concurrency": "serial",
        "shared-process": false,
        "local-write-concurrency": 1i64,
        "startup-runtime-build-limit": 1_000i64,
        "startup-runtime-repair": true,
        "max-blocking-executions": -1i64
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

/// Get the cap on a function's own `timeout` meta from worker config (in
/// seconds). It never rises above `MAX_RUN_TIMEOUT_CEILING_SECONDS`, and
/// below that never falls below `worker.run-timeout`, so a function may
/// always ask for the default.
pub fn get_max_run_timeout(worker_conf: &Val) -> std::time::Duration {
    let cap_secs = worker_conf.get_int_or_default(
        "worker.max-run-timeout",
        DEFAULT_MAX_RUN_TIMEOUT_SECONDS as i64,
    );
    let cap = if cap_secs > 0 {
        (cap_secs as u64).min(MAX_RUN_TIMEOUT_CEILING_SECONDS)
    } else {
        DEFAULT_MAX_RUN_TIMEOUT_SECONDS
    };
    std::time::Duration::from_secs(cap)
        .max(get_run_timeout(worker_conf))
        .min(std::time::Duration::from_secs(
            MAX_RUN_TIMEOUT_CEILING_SECONDS,
        ))
}

/// How long one run may take: the `timeout` meta of the function it runs,
/// else `worker.run-timeout`, capped at `worker.max-run-timeout`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RunBudget {
    timeout: std::time::Duration,
    /// The meta's seconds, when the cap lowered them.
    capped_from: Option<u64>,
}

impl RunBudget {
    fn new(
        meta_timeout: Option<u64>,
        default: std::time::Duration,
        cap: std::time::Duration,
    ) -> Self {
        match meta_timeout {
            Some(secs) if std::time::Duration::from_secs(secs) > cap => Self {
                timeout: cap,
                capped_from: Some(secs),
            },
            Some(secs) => Self {
                timeout: std::time::Duration::from_secs(secs),
                capped_from: None,
            },
            None => Self {
                timeout: default,
                capped_from: None,
            },
        }
    }

    /// Resolve the budget of a run of `function` (`::ns/var`) from that
    /// function's meta in the program the run executes. A value `hot check`
    /// rejects leaves the default in place.
    fn for_function(program: &hot::lang::ast::Program, function: &str, worker_conf: &Val) -> Self {
        let meta_timeout = function
            .rsplit_once('/')
            .and_then(|(ns, var)| {
                hot::lang::compiler::artifact_extraction::effective_function_meta(program, ns, var)
            })
            .and_then(|meta| hot::lang::compiler::artifact_extraction::run_timeout_from_meta(&meta))
            .and_then(|timeout| match timeout {
                Ok(secs) => Some(secs),
                Err(message) => {
                    warn!(function, "Ignoring invalid timeout meta: {}", message);
                    None
                }
            });
        let budget = Self::new(
            meta_timeout,
            get_run_timeout(worker_conf),
            get_max_run_timeout(worker_conf),
        );
        if let Some(requested) = budget.capped_from {
            warn!(
                function,
                requested_secs = requested,
                timeout_secs = budget.timeout.as_secs(),
                "Function timeout meta exceeds worker.max-run-timeout; capping the run"
            );
        }
        budget
    }

    /// The failure a run records when it outlives this budget.
    fn timeout_error(&self) -> String {
        match self.capped_from {
            Some(requested) => format!(
                "Run timed out after {} seconds (its timeout of {} seconds is capped by worker.max-run-timeout)",
                self.timeout.as_secs(),
                requested
            ),
            None => format!("Run timed out after {} seconds", self.timeout.as_secs()),
        }
    }

    /// `start` plus this budget, saturating far in the future.
    fn deadline(&self, start: tokio::time::Instant) -> tokio::time::Instant {
        start
            .checked_add(self.timeout)
            .unwrap_or_else(|| start + std::time::Duration::from_secs(86_400 * 365 * 30))
    }
}

/// Wait for a run to reach its budget. Until its program loads a run has the
/// default budget; the blocking thread then sends the function's own budget,
/// which moves the deadline to `run_started` plus that budget.
async fn run_budget_reached(
    run_started: tokio::time::Instant,
    default_budget: RunBudget,
    mut budget_rx: tokio::sync::oneshot::Receiver<RunBudget>,
) -> RunBudget {
    let budget = tokio::select! {
        biased;
        Ok(budget) = &mut budget_rx => budget,
        _ = tokio::time::sleep_until(default_budget.deadline(run_started)) => default_budget,
    };
    tokio::time::sleep_until(budget.deadline(run_started)).await;
    budget
}

/// Wait for the claim fence to report a loss; never resolves without a fence
/// (memory and SQLite queues) or once the keepalive ends without one. An
/// unconfirmed claim resolves only at its stop time (see
/// `unconfirmed_claim_stop_at`), and not at all if a later touch confirms
/// the claim first; a touch that fails again after a confirmation moves the
/// stop time.
async fn claim_lost(
    claim: Option<tokio::sync::watch::Receiver<Option<ClaimLoss>>>,
) -> Option<ClaimLoss> {
    let Some(mut claim) = claim else {
        return std::future::pending().await;
    };
    loop {
        let loss = *claim.borrow_and_update();
        let stop_at = match loss {
            Some(ClaimLoss::Lost) => return loss,
            Some(ClaimLoss::Unconfirmed { reclaimable_at, .. }) => {
                Some(unconfirmed_claim_stop_at(reclaimable_at))
            }
            None => None,
        };
        let stop = async {
            match stop_at {
                Some(stop_at) => tokio::time::sleep_until(stop_at).await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            biased;
            changed = claim.changed() => {
                if changed.is_err() {
                    // The keepalive ended: the last verdict is final.
                    let Some(stop_at) = stop_at else {
                        return std::future::pending().await;
                    };
                    tokio::time::sleep_until(stop_at).await;
                    return loss;
                }
            }
            _ = stop => return loss,
        }
    }
}

/// Why the worker stops a run it is executing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RunStop {
    TimedOut(RunBudget),
    ClaimLost(ClaimLoss),
}

impl RunStop {
    /// Whether this worker records the stopped run's failure. Not once
    /// another worker has taken the event over: that worker runs the event's
    /// handlers again under the same run ids and records how they end.
    fn records_failure(&self) -> bool {
        !matches!(self, RunStop::ClaimLost(ClaimLoss::Lost))
    }

    /// Why the run stopped, and the failure it records.
    fn error(&self) -> String {
        match self {
            RunStop::TimedOut(budget) => budget.timeout_error(),
            RunStop::ClaimLost(ClaimLoss::Lost) => {
                "Run stopped: another worker took over its event".to_string()
            }
            RunStop::ClaimLost(ClaimLoss::Unconfirmed { unconfirmed, .. }) => format!(
                "Run stopped: its worker could not refresh its claim on the event for {} seconds",
                unconfirmed.as_secs()
            ),
        }
    }
}

/// Wait until the worker must stop a run: at its budget (see
/// `run_budget_reached`), or when the claim fence reports its event's claim
/// lost. Then note the stop in `stopped` (see `StoppedRunEmitter`), fire the
/// cooperative cancel token, and after `RUN_TIMEOUT_HARD_GRACE` more (none
/// without a token) return, so the caller frees the executor slot.
async fn run_must_stop(
    run_started: tokio::time::Instant,
    default_budget: RunBudget,
    budget_rx: tokio::sync::oneshot::Receiver<RunBudget>,
    claim: Option<tokio::sync::watch::Receiver<Option<ClaimLoss>>>,
    stopped: Arc<OnceLock<RunStop>>,
    cancel: Option<Arc<std::sync::atomic::AtomicBool>>,
) -> RunStop {
    let stop = tokio::select! {
        budget = run_budget_reached(run_started, default_budget, budget_rx) => RunStop::TimedOut(budget),
        Some(loss) = claim_lost(claim) => RunStop::ClaimLost(loss),
    };
    let _ = stopped.set(stop);
    if let Some(token) = cancel {
        token.store(true, std::sync::atomic::Ordering::Relaxed);
        tokio::time::sleep(RUN_TIMEOUT_HARD_GRACE).await;
    }
    stop
}

/// Wraps a run's emitter so that, once the worker has decided to stop the
/// run, the failure the VM records as it unwinds from the cancel names the
/// worker's reason instead of a bare "Execution cancelled". A run whose event
/// another worker took over records no end at all (see
/// `RunStop::records_failure`).
struct StoppedRunEmitter {
    inner: Arc<dyn EngineEventEmitter>,
    run_id: Uuid,
    stopped: Arc<OnceLock<RunStop>>,
}

impl EngineEventEmitter for StoppedRunEmitter {
    fn emit(&self, event: hot::lang::emitter::EngineEvent) {
        if event.execution_context.run_id == self.run_id
            && let Some(stop) = self.stopped.get()
        {
            match event.event_type.as_str() {
                "run:fail" | "run:stop" | "run:cancel" if !stop.records_failure() => return,
                "run:fail" => {
                    self.inner.emit(hot::lang::emitter::EngineEvent::run_fail(
                        &event.execution_context,
                        val!({
                            "$type": "::hot::run/Failure",
                            "$val": {
                                "msg": stop.error(),
                                "err": Val::Null,
                            },
                        }),
                    ));
                    return;
                }
                _ => {}
            }
        }
        self.inner.emit(event);
    }

    fn flush(&self) -> Result<(), String> {
        self.inner.flush()
    }

    fn flush_async(
        &self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + '_>> {
        self.inner.flush_async()
    }

    fn flush_run(
        &self,
        run_id: Uuid,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + '_>> {
        self.inner.flush_run(run_id)
    }

    fn shutdown(
        &self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + '_>> {
        self.inner.shutdown()
    }
}

/// Notes when a run's `run:start` passes its emitter: until then a failure is
/// a setup failure, with no run row and none of the run's code executed, so
/// the event can be delivered again (see `run_never_started`).
struct RunStartWatch {
    inner: Arc<dyn EngineEventEmitter>,
    run_id: Uuid,
    started: Arc<std::sync::atomic::AtomicBool>,
}

impl RunStartWatch {
    fn wrap(
        inner: Arc<dyn EngineEventEmitter>,
        run_id: Uuid,
        started: Arc<std::sync::atomic::AtomicBool>,
    ) -> Arc<dyn EngineEventEmitter> {
        Arc::new(Self {
            inner,
            run_id,
            started,
        })
    }
}

impl EngineEventEmitter for RunStartWatch {
    fn emit(&self, event: hot::lang::emitter::EngineEvent) {
        if event.event_type == "run:start" && event.execution_context.run_id == self.run_id {
            self.started
                .store(true, std::sync::atomic::Ordering::Release);
        }
        self.inner.emit(event);
    }

    fn flush(&self) -> Result<(), String> {
        self.inner.flush()
    }

    fn flush_async(
        &self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + '_>> {
        self.inner.flush_async()
    }

    fn flush_run(
        &self,
        run_id: Uuid,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + '_>> {
        self.inner.flush_run(run_id)
    }

    fn shutdown(
        &self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + '_>> {
        self.inner.shutdown()
    }
}

/// Runs the deliveries a stopped worker left alone. Such a delivery may be
/// what stopped that worker, and every delivery in flight with it was
/// redelivered too, so each executes with no other event of this process,
/// and the queue records that it did (see `SqliteQueue::with_crash_quarantine`).
/// If it stops this worker as well, nothing else stops with it, and only its
/// own retry limit counts the stop.
#[derive(Clone, Default)]
struct CrashQuarantine(Arc<tokio::sync::RwLock<()>>);

/// Held while a delivery executes (see `CrashQuarantine::enter`).
struct QuarantinePass {
    _shared: Option<tokio::sync::OwnedRwLockReadGuard<()>>,
    _alone: Option<tokio::sync::OwnedRwLockWriteGuard<()>>,
}

impl CrashQuarantine {
    /// Wait until the delivery may execute: a recovered delivery once no
    /// other delivery executes, the others while no recovered one does.
    async fn enter(
        &self,
        recovered: bool,
        sole_delivery: Option<&hot::queue::SoleDeliveryMarker>,
        worker_id: usize,
    ) -> QuarantinePass {
        if !recovered {
            return QuarantinePass {
                _shared: Some(Arc::clone(&self.0).read_owned().await),
                _alone: None,
            };
        }
        let alone = Arc::clone(&self.0).write_owned().await;
        info!(
            "hot.dev: WORKER {} runs a delivery recovered from a stopped worker alone",
            worker_id
        );
        if let Some(marker) = sole_delivery
            && let Err(error) = marker.mark().await
        {
            warn!(
                "hot.dev: WORKER {} could not record that a recovered delivery runs alone: {}",
                worker_id, error
            );
        }
        QuarantinePass {
            _shared: None,
            _alone: Some(alone),
        }
    }
}

/// On the blocking thread, once a run's program has loaded: resolve the run's
/// budget, stamp the deadline the VM caps child waits to, and hand the budget
/// to `run_budget_expired`. A program that took longer than the default
/// budget to load has already been timed out by then.
fn arm_run_budget(
    program: &hot::lang::ast::Program,
    function: &str,
    worker_conf: &Val,
    run_started_at: chrono::DateTime<chrono::Utc>,
    context: &mut ExecutionContext,
    budget_tx: tokio::sync::oneshot::Sender<RunBudget>,
) {
    let budget = RunBudget::for_function(program, function, worker_conf);
    context.deadline_at = chrono::Duration::from_std(budget.timeout)
        .ok()
        .and_then(|timeout| run_started_at.checked_add_signed(timeout));
    let _ = budget_tx.send(budget);
}

/// How long a run start waits for its row before it is published anyway.
const RUN_START_FLUSH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// The run type stream and dashboard events carry for an event type.
fn run_type_name(event_type: &str) -> String {
    match event_type {
        "hot:call" => "call",
        "hot:schedule" => "schedule",
        _ => "event",
    }
    .to_string()
}

/// Wraps a run's emitter to publish `run:start` to the run's stream and its
/// environment once the VM has recorded the run.
///
/// The VM emits `run:start` on its blocking thread before the run's first
/// instruction, and that thread also publishes the run's `stream:data`
/// synchronously, so publishing here, before returning, puts the start ahead
/// of every chunk and of the run's end in the subscriber's stream. The run
/// row is flushed first because SSE handlers read it to describe the run.
struct RunStartPublisher {
    inner: Arc<dyn EngineEventEmitter>,
    publisher: Arc<StreamPubSub>,
    run_id: Uuid,
    env_id: Uuid,
    stream_id: Uuid,
    event_id: Option<Uuid>,
    project_id: Option<Uuid>,
    fn_name: String,
    run_type: String,
    published: std::sync::atomic::AtomicBool,
}

impl RunStartPublisher {
    fn publish_start(&self) {
        // The VM runs on a blocking thread, where `block_on` is the bridge
        // (as for `stream:data`).
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        handle.block_on(async {
            match tokio::time::timeout(RUN_START_FLUSH_TIMEOUT, self.inner.flush_run(self.run_id))
                .await
            {
                Ok(Ok(())) => {}
                Ok(Err(e)) => warn!(run_id = %self.run_id, "Failed to flush run start before publishing it: {}", e),
                Err(_) => warn!(run_id = %self.run_id, "Flushing run start timed out; publishing it anyway"),
            }
            let stream_event = StreamEvent::RunStart {
                run_id: self.run_id,
                env_id: self.env_id,
                stream_id: self.stream_id,
                event_id: self.event_id,
            };
            if let Err(e) = self.publisher.publish(stream_event).await {
                warn!(run_id = %self.run_id, "Failed to publish run start: {}", e);
            }
            let env_event = EnvEvent::RunStart {
                run_id: self.run_id,
                env_id: self.env_id,
                stream_id: self.stream_id,
                event_id: self.event_id,
                project_id: self.project_id,
                fn_name: Some(self.fn_name.clone()),
                run_type: self.run_type.clone(),
            };
            if let Err(e) = self.publisher.publish_env(env_event).await {
                warn!(run_id = %self.run_id, "Failed to publish run start env event: {}", e);
            }
        });
    }
}

impl EngineEventEmitter for RunStartPublisher {
    fn emit(&self, event: hot::lang::emitter::EngineEvent) {
        let starts_run =
            event.event_type == "run:start" && event.execution_context.run_id == self.run_id;
        self.inner.emit(event);
        if starts_run
            && !self
                .published
                .swap(true, std::sync::atomic::Ordering::Relaxed)
        {
            self.publish_start();
        }
    }

    fn flush(&self) -> Result<(), String> {
        self.inner.flush()
    }

    fn flush_async(
        &self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + '_>> {
        self.inner.flush_async()
    }

    fn flush_run(
        &self,
        run_id: Uuid,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + '_>> {
        self.inner.flush_run(run_id)
    }

    fn shutdown(
        &self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + '_>> {
        self.inner.shutdown()
    }
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
    run_with_components_shared_context_and_db(
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
        None,
    )
    .await
}

/// Run the worker with reloadable dev context storage and an optional
/// pre-created database pool. The CLI uses this to share the same PostgreSQL
/// pool with the emitter and event publisher instead of opening a second pool.
#[allow(clippy::too_many_arguments)]
pub async fn run_with_components_shared_context_and_db(
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
    precreated_db: Option<Arc<DatabasePool>>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    debug!("hot.dev: WORKER starting");
    validate_worker_semantics_conf(&worker_conf)?;
    // Before the executors' blocking threads start allocating.
    hot::lang::runtime::oom_logger::limit_malloc_arenas(
        hot::lang::runtime::oom_logger::DEFAULT_MALLOC_ARENAS,
    );
    let queue_metrics_enabled = worker_conf.get_bool_or_default("queue.metrics-enabled", true);
    let queue_wait_target_p99_ms = worker_conf
        .get_int_or_default("queue.wait-target-p99-ms", 1_000)
        .max(1) as u64;
    hot::queue::set_metrics_enabled(queue_metrics_enabled);
    hot::queue::set_wait_target_p99_ms(queue_wait_target_p99_ms);
    info!(
        "hot.dev: WORKER queue metrics configured (metrics_enabled={}, wait_target_p99_ms={})",
        queue_metrics_enabled, queue_wait_target_p99_ms,
    );
    let queue_orphan_idle_ms = worker_conf
        .get_int_or_default("queue.event-orphan-idle-ms", 60_000)
        .max(1) as u64;
    // Invariant: reclaim idle must exceed the longest an un-ACKed
    // single-handler event can legally be alive on a healthy worker —
    // source retrieval plus post-retrieval setup (including blocking-slot
    // admission) plus run budget plus hard grace. Events whose serial
    // multi-handler execution, or whose function's own `timeout`, outlives
    // this window stay reclaim-safe via the PEL-touch keepalive
    // (`spawn_event_lease_keepalive`), for which this floor is the crash
    // backstop, and the claim fence; see `min_event_orphan_idle_floor_ms`.
    let min_event_orphan_idle_ms = min_event_orphan_idle_floor_ms(get_run_timeout(&worker_conf));
    let event_orphan_idle_ms = queue_orphan_idle_ms.max(min_event_orphan_idle_ms);
    if event_orphan_idle_ms != queue_orphan_idle_ms {
        tracing::warn!(
            configured_ms = queue_orphan_idle_ms,
            effective_ms = event_orphan_idle_ms,
            "Raising hot:event orphan reclaim idle to stay beyond blocking-slot admission plus worker.run-timeout and avoid duplicate live handler execution"
        );
    }

    // Create database connection for event handler processing FIRST
    // This is needed by the emitter and event publisher
    let db = match precreated_db {
        Some(pool) => {
            debug!("hot.dev: WORKER reusing pre-created database pool");
            hot::db::test_connection(pool.as_ref()).await.map_err(|e| {
                error!(
                    "hot.dev: WORKER failed to verify pre-created database connection: {}",
                    e
                );
                format!("Database connection test failed: {}", e)
            })?;
            Some(pool)
        }
        None => match hot::db::create_db_pool(&worker_conf).await {
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
        },
    };

    // The worker coordinator discovers durable queue work globally. Execution
    // receives a fresh exact-environment handle after hydration below.
    let db = db.map(|db| {
        Arc::new(db.with_scope(DatabaseScope::Maintenance {
            operation: MaintenanceOperation::Queue,
            org_id: None,
        }))
    });

    // Shared blob store for spilling large payloads on write (emitter/event
    // publisher) and rehydrating BlobRefs when events are loaded for execution.
    // None when blob spill is disabled or storage init fails.
    let blob_store = if let Some(ref db_pool) = db {
        hot::blob::blob_store_from_conf(db_pool.clone(), &worker_conf).await
    } else {
        None
    };

    // Use provided emitter and event publisher, or create them from configuration
    // NOTE: These need the database pool to be created first
    let emitter = if emitter.is_some() {
        emitter
    } else {
        // emitter requires database pool, so only create if db is available
        if let Some(ref db_pool) = db {
            match create_emitter(&worker_conf, db_pool.as_ref(), blob_store.clone()).await {
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
            match create_event_publisher(&worker_conf, db_pool.as_ref(), blob_store.clone()).await {
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

    let execution_services = Arc::new(
        WorkerExecutionServices::new(
            db.as_ref()
                .cloned()
                .ok_or_else(|| worker_error("Worker database pool is unavailable"))?,
            &worker_conf,
        )
        .await,
    );

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
        // SQLite queues use process-local pub/sub for live UI updates; Redis
        // retains distributed pub/sub.
        let pubsub_type = match queue_type {
            QueueType::Memory | QueueType::Sqlite => StreamPubSubType::Memory,
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
            // A Redis queue means API and app replicas subscribe through
            // Redis. Publishing nowhere would silently stall their streams.
            Err(e) if matches!(pubsub_type, StreamPubSubType::Redis) => {
                return Err(worker_error(format!(
                    "Failed to create Redis stream publisher: {e}"
                )));
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

    // Read a small batch per Redis XREADGROUP round-trip. Claimer/executor
    // handoff still bounds local concurrency, but batching avoids one Redis
    // round-trip per event when backlog is already present.
    let worker_read_batch_size = worker_conf
        .get_int_or_default("worker.read-batch-size", 8)
        .max(1) as usize;

    // Create processing queues with dequeue_and_work support - all using unified Message type
    let request_queue = ProcessingQueue::<Message>::new_with_cluster(
        queue_type,
        request_queue_name,
        redis_uri.clone(),
        redis_cluster,
        serialization,
    )?
    .with_startup_window(request_window)
    .with_orphan_idle_ms(queue_orphan_idle_ms)
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
    .with_orphan_idle_ms(queue_orphan_idle_ms)
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
    .with_orphan_idle_ms(event_orphan_idle_ms)
    .with_read_batch_size(worker_read_batch_size)
    .with_consumer_name(admin_event_name.clone())
    // Executors run recovered deliveries alone (see `CrashQuarantine`).
    .with_crash_quarantine();

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
    .with_orphan_idle_ms(queue_orphan_idle_ms)
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
    .with_orphan_idle_ms(queue_orphan_idle_ms)
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
        QueueType::Sqlite | QueueType::Redis => {
            // Force SQLite bootstrap or Redis connectivity before worker startup.
            match event_queue.is_empty().await {
                Ok(_) => {
                    debug!(
                        "hot.dev: WORKER successfully connected to {} queue",
                        queue_type
                    );
                }
                Err(e) => {
                    error!(
                        "hot.dev: WORKER failed to connect to {} queue: {}",
                        queue_type, e
                    );
                    return Err(
                        format!("{} queue connectivity check failed: {}", queue_type, e).into(),
                    );
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
        for queue in [&alert_queue, &email_queue] {
            if let Err(error) = queue.purge_old_pending(LOCAL_DEV_MAX_AGE_MS).await {
                warn!(%error,"Could not purge stale local notifications");
            }
        }
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
    // Settle the recovered events' runs before their messages can be
    // claimed again: a redelivery that started first would have its running
    // runs held under it.
    let recovery_db = db.clone();
    let recovery_result = tokio::select! {
        result = tokio::time::timeout(
            recovery_timeout,
            event_queue.recover_orphaned_items_then(|recovery| {
                let requeued = recovery.requeued_messages.clone();
                let dead_lettered = recovery.dead_lettered_messages.clone();
                async move {
                    if let Some(ref db_pool) = recovery_db {
                        reconcile_orphaned_event_runs(
                            db_pool,
                            &requeued,
                            "Worker crashed during event processing - event will be retried",
                            OrphanedEvents::Requeued,
                        )
                        .await;
                        reconcile_orphaned_event_runs(
                            db_pool,
                            &dead_lettered,
                            CRASHED_EVENT_DEAD_LETTERED,
                            OrphanedEvents::DeadLettered,
                        )
                        .await;
                    }
                }
            }),
        ) => result,
        _ = hot::signal::shutdown_signal() => {
            info!("hot.dev: WORKER received shutdown signal during orphaned item recovery");
            return Ok(());
        }
    };
    match recovery_result {
        Ok(Ok(recovery)) if recovery.total_count() > 0 => {
            if recovery.requeued_count > 0 {
                info!(
                    "hot.dev: WORKER recovered {} orphaned items - these will be reprocessed",
                    recovery.requeued_count
                );
            }
            if !recovery.dead_lettered_messages.is_empty() {
                warn!(
                    "hot.dev: WORKER moved {} expired orphaned item(s) to the dead-letter queue",
                    recovery.dead_lettered_messages.len()
                );
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
            if let hot::db::DatabasePool::Sqlite(pool) = cleanup_db.as_ref() {
                match hot::db::sqlite::run_maintenance(pool, &cleanup_conf).await {
                    Ok(stats) if stats.wal_checkpointed_pages > 0 => {
                        info!(
                            "hot.dev: WORKER startup sqlite maintenance checkpointed {} WAL page(s)",
                            stats.wal_checkpointed_pages
                        );
                    }
                    Ok(_) => {}
                    Err(e) => {
                        error!("hot.dev: WORKER startup sqlite maintenance failed: {}", e);
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
    let configured_max_blocking_executions =
        worker_conf.get_int_or_default("worker.max-blocking-executions", -1);
    let max_blocking_executions =
        blocking_execution_capacity(worker_count, configured_max_blocking_executions);
    let blocking_execution_slots = Arc::new(Semaphore::new(max_blocking_executions));
    let crash_quarantine = CrashQuarantine::default();

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
    info!(
        max_blocking_executions,
        configured_max = configured_max_blocking_executions,
        "hot.dev: WORKER bounded blocking VM executions"
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
    // Republish events parked while their environment or project was inactive.
    // The coordinator pool already runs under the Queue operation.
    if let Some(parked_db) = db.clone() {
        let parked_queue = event_queue.clone();
        let mut parked_shutdown = shutdown_rx.clone();
        tokio::spawn(async move {
            let mut ticks = tokio::time::interval(std::time::Duration::from_secs(30));
            loop {
                tokio::select! {
                    changed = parked_shutdown.changed() => {
                        if changed.is_err() || *parked_shutdown.borrow() { break; }
                    }
                    _ = ticks.tick() => {
                        if let Err(error) = replay_parked_events(&parked_db, &parked_queue).await {
                            warn!(%error, "Could not replay parked events; retrying on the next tick");
                        }
                    }
                }
            }
        });
    }

    if let Some(ref db_pool) = db
        && worker_conf.get_bool_or_default("worker.startup-runtime-repair", true)
    {
        let startup_db = db_pool.clone();
        let startup_worker_conf = worker_conf.clone();
        let startup_build_path_cache = build_path_cache.clone();
        let startup_shutdown_rx = shutdown_rx.clone();
        tokio::spawn(async move {
            startup_repair_and_warm_bundle_runtimes(
                &startup_db,
                &startup_worker_conf,
                &startup_build_path_cache,
                startup_shutdown_rx,
            )
            .await;
        });
    }

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
    } else if matches!(queue_type, QueueType::Sqlite) {
        let event_q = event_queue.clone();
        let other_queues = [
            ("hot:request", request_queue.clone()),
            ("hot:response", response_queue.clone()),
            ("hot:alert", alert_queue.clone()),
            ("hot:email", email_queue.clone()),
        ];
        let janitor_db = db.clone();
        let mut janitor_shutdown_rx = shutdown_rx.clone();
        tokio::spawn(async move {
            const TICK: std::time::Duration = std::time::Duration::from_secs(60);
            loop {
                tokio::select! {
                    biased;
                    changed = janitor_shutdown_rx.changed() => {
                        if changed.is_err() || *janitor_shutdown_rx.borrow() {
                            debug!("hot.dev: WORKER SQLite queue janitor shutting down");
                            break;
                        }
                    }
                    _ = tokio::time::sleep(TICK) => {
                        let settle = |recovery: &hot::queue::OrphanRecovery| {
                            let requeued = recovery.requeued_messages.clone();
                            let dead_lettered = recovery.dead_lettered_messages.clone();
                            let janitor_db = janitor_db.clone();
                            async move {
                                if let Some(ref db) = janitor_db {
                                    reconcile_orphaned_event_runs(
                                        db,
                                        &requeued,
                                        "Worker lease expired during event processing - event will be retried",
                                        OrphanedEvents::Requeued,
                                    )
                                    .await;
                                    reconcile_orphaned_event_runs(
                                        db,
                                        &dead_lettered,
                                        CRASHED_EVENT_DEAD_LETTERED,
                                        OrphanedEvents::DeadLettered,
                                    )
                                    .await;
                                }
                            }
                        };
                        match event_q.recover_orphaned_items_then(settle).await {
                            Ok(recovery) => {
                                if recovery.total_count() > 0 {
                                    info!(
                                        "hot.dev: WORKER SQLite janitor handled {} orphaned message(s) on hot:event",
                                        recovery.total_count()
                                    );
                                }
                            }
                            Err(error) => debug!(
                                "hot.dev: WORKER SQLite janitor recovery failed on hot:event: {}",
                                error
                            ),
                        }

                        for (name, queue) in &other_queues {
                            match queue.recover_orphaned_items().await {
                                Ok(0) => {}
                                Ok(count) => info!(
                                    "hot.dev: WORKER SQLite janitor handled {} orphaned message(s) on {}",
                                    count, name
                                ),
                                Err(error) => debug!(
                                    "hot.dev: WORKER SQLite janitor recovery failed on {}: {}",
                                    name, error
                                ),
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
    let claimer_count = worker_queue_claimer_count(db.is_some());
    let handoff_capacity = worker_handoff_capacity(worker_count);
    let redis_blocking_claim_connections = if matches!(queue_type, QueueType::Redis) {
        claimer_count
    } else {
        0
    };
    info!(
        "hot.dev: WORKER queue claimers enabled (claimers={}, executors={}, handoff_capacity={}, redis_blocking_claim_connections={})",
        claimer_count, worker_count, handoff_capacity, redis_blocking_claim_connections,
    );
    let (request_claimed_tx, request_claimed_rx) =
        mpsc::channel::<ClaimedWorkerQueue>(worker_handoff_capacity(1));
    let request_claimed_rx = Arc::new(Mutex::new(request_claimed_rx));
    let (event_claimed_tx, event_claimed_rx) =
        mpsc::channel::<ClaimedWorkerQueue>(handoff_capacity);
    let event_claimed_rx = Arc::new(Mutex::new(event_claimed_rx));
    let (notification_claimed_tx, notification_claimed_rx) =
        mpsc::channel::<ClaimedWorkerQueue>(worker_handoff_capacity(1));
    let notification_claimed_rx = Arc::new(Mutex::new(notification_claimed_rx));
    let mut worker_handles = Vec::new();

    worker_handles.push(spawn_worker_queue_claimer(
        "hot:request",
        request_queue
            .clone()
            .with_consumer_name(admin_request_name.clone()),
        request_claimed_tx.clone(),
        shutdown_rx.clone(),
        ClaimedWorkerQueue::Request,
        queue_metrics_enabled,
    ));
    worker_handles.push(spawn_worker_queue_claimer(
        "hot:event",
        event_queue
            .clone()
            .with_consumer_name(admin_event_name.clone()),
        event_claimed_tx.clone(),
        shutdown_rx.clone(),
        ClaimedWorkerQueue::Event,
        queue_metrics_enabled,
    ));
    if db.is_some() {
        worker_handles.push(spawn_worker_queue_claimer(
            "hot:alert",
            alert_queue
                .clone()
                .with_consumer_name(admin_alert_name.clone()),
            notification_claimed_tx.clone(),
            shutdown_rx.clone(),
            ClaimedWorkerQueue::Alert,
            queue_metrics_enabled,
        ));
        worker_handles.push(spawn_worker_queue_claimer(
            "hot:email",
            email_queue
                .clone()
                .with_consumer_name(admin_email_name.clone()),
            notification_claimed_tx.clone(),
            shutdown_rx.clone(),
            ClaimedWorkerQueue::Email,
            queue_metrics_enabled,
        ));
    }
    drop(request_claimed_tx);
    drop(event_claimed_tx);
    drop(notification_claimed_tx);

    if let Some(db_for_notifications) = db.clone() {
        let notification_rx = notification_claimed_rx.clone();
        let notification_worker_conf = worker_conf.clone();
        let notification_shutdown_rx = shutdown_rx.clone();
        let handle: JoinHandle<Result<(), Box<dyn std::error::Error + Send + Sync>>> = tokio::spawn(
            async move {
                let worker_id = worker_count;
                debug!("hot.dev: WORKER notification executor started");

                loop {
                    let claimed_queue = {
                        let mut claimed_rx = notification_rx.lock().await;
                        claimed_rx.recv().await
                    };

                    let Some(claimed_queue) = claimed_queue else {
                        debug!("hot.dev: WORKER notification executor stopping");
                        break;
                    };

                    if *notification_shutdown_rx.borrow() {
                        debug!(
                            "hot.dev: WORKER notification executor dropping lease during shutdown"
                        );
                        drop(claimed_queue);
                        break;
                    }

                    match claimed_queue {
                        ClaimedWorkerQueue::Alert(lease) => {
                            let db = db_for_notifications.clone();
                            let worker_conf = notification_worker_conf.clone();
                            if let Err(e) = lease
                                .process(|message| async move {
                                    process_alert_delivery_message(
                                        worker_id,
                                        db,
                                        worker_conf,
                                        message,
                                    )
                                    .await
                                })
                                .await
                            {
                                tracing::debug!(
                                    "hot.dev: WORKER notification alert queue error: {}",
                                    e
                                );
                            }
                        }
                        ClaimedWorkerQueue::Email(lease) => {
                            let db = db_for_notifications.clone();
                            let worker_conf = notification_worker_conf.clone();
                            if let Err(e) = lease
                                .process(|message| async move {
                                    process_email_message(worker_id, db, worker_conf, message).await
                                })
                                .await
                            {
                                tracing::debug!(
                                    "hot.dev: WORKER notification email queue error: {}",
                                    e
                                );
                            }
                        }
                        other => {
                            drop(other);
                            tracing::warn!(
                                "hot.dev: WORKER notification executor received non-notification lease"
                            );
                        }
                    }
                }

                Ok(())
            },
        );
        worker_handles.push(handle);
    }

    {
        let request_rx = request_claimed_rx.clone();
        let request_shutdown_rx = shutdown_rx.clone();
        let response_queue_for_requests = response_queue
            .clone()
            .with_consumer_name(admin_response_name.clone());
        let handle: JoinHandle<Result<(), Box<dyn std::error::Error + Send + Sync>>> = tokio::spawn(
            async move {
                let worker_id = 0usize;
                debug!("hot.dev: WORKER request executor started");

                loop {
                    let claimed_queue = {
                        let mut claimed_rx = request_rx.lock().await;
                        claimed_rx.recv().await
                    };

                    let Some(claimed_queue) = claimed_queue else {
                        debug!("hot.dev: WORKER request executor stopping");
                        break;
                    };

                    if *request_shutdown_rx.borrow() {
                        debug!("hot.dev: WORKER request executor dropping lease during shutdown");
                        drop(claimed_queue);
                        break;
                    }

                    let ClaimedWorkerQueue::Request(lease) = claimed_queue else {
                        tracing::warn!(
                            "hot.dev: WORKER request executor received non-request lease"
                        );
                        continue;
                    };

                    let response_queue = response_queue_for_requests.clone();
                    match lease
                        .process(|message| async move {
                            let msg_type = message.head.get_str("__type");
                            match msg_type.as_str() {
                                "RequestMessage" => {
                                    let request_msg: RequestMessage = message.try_into().map_err(
                                        |e| {
                                            Box::new(std::io::Error::other(e))
                                                as Box<dyn std::error::Error + Send + Sync>
                                        },
                                    )?;

                                    debug!(
                                        "hot.dev: WORKER {} received request from hot:request queue: id={} head={:?} body={}",
                                        worker_id,
                                        request_msg.id,
                                        request_msg.head,
                                        request_msg.body.to_string()
                                    );

                                    let response_msg = ResponseMessage {
                                        id: request_msg.id,
                                        head: request_msg.head.clone(),
                                        body: request_msg.body.clone(),
                                    };

                                    let response_message: Message = response_msg.into();
                                    response_queue.enqueue(response_message).await.map_err(|e| {
                                        let error_msg =
                                            format!("Failed to send response message: {}", e);
                                        warn!("hot.dev: WORKER {} {}", worker_id, error_msg);
                                        Box::new(std::io::Error::other(error_msg))
                                            as Box<dyn std::error::Error + Send + Sync>
                                    })?;

                                    debug!(
                                        "hot.dev: WORKER {} sent response to hot:response queue",
                                        worker_id
                                    );
                                }
                                _ => {
                                    warn!(
                                        "hot.dev: WORKER {} received unknown message type '{}' on request queue",
                                        worker_id, msg_type
                                    );
                                }
                            }

                            Ok(()) as Result<(), Box<dyn std::error::Error + Send + Sync>>
                        })
                        .await
                    {
                        Ok(Some(_)) | Ok(None) => {}
                        Err(e) => {
                            warn!(
                                "hot.dev: WORKER {} error processing request message: {}",
                                worker_id, e
                            );
                        }
                    }
                }

                Ok(())
            },
        );
        worker_handles.push(handle);
    }

    // Spawn bounded event executor tasks. Queue consumers live in the claimers
    // above; these handles are only for event execution plus enqueue/admin
    // maintenance paths.
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
        let claimed_rx_clone = event_claimed_rx.clone();
        let db_clone = db.clone();
        let blob_store_clone = blob_store.clone();
        let worker_conf_clone = worker_conf.clone();
        let emitter_clone = emitter.clone();
        let event_publisher_clone = event_publisher.clone();
        let encryption_clone = encryption.clone();
        let cache_clone = shared_cache.clone();
        let build_path_cache_clone = build_path_cache.clone();
        let shutdown_coordinator_clone = shutdown_coordinator.clone();
        let shutdown_rx_clone = shutdown_rx.clone();
        let dev_context_storage_clone = dev_context_storage.clone();
        let stream_publisher_clone = stream_publisher.clone();
        let task_queue_clone = task_queue.clone();
        let execution_services_clone = execution_services.clone();
        let blocking_execution_slots_clone = blocking_execution_slots.clone();
        let crash_quarantine_clone = crash_quarantine.clone();
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
                debug!("hot.dev: WORKER {} started", worker_id);

                loop {
                    let claimed_queue = {
                        let mut claimed_rx = claimed_rx_clone.lock().await;
                        claimed_rx.recv().await
                    };

                    let Some(claimed_queue) = claimed_queue else {
                        debug!("hot.dev: WORKER {} executor stopping", worker_id);
                        break;
                    };

                    if *shutdown_rx_clone.borrow() {
                        debug!(
                            "hot.dev: WORKER {} dropping claimed queue lease during shutdown",
                            worker_id
                        );
                        drop(claimed_queue);
                        break;
                    }

                    match claimed_queue {
                        ClaimedWorkerQueue::Request(lease) => {
                            drop(lease);
                            tracing::warn!(
                                "hot.dev: WORKER {} event executor received request lease",
                                worker_id
                            );
                        }
                        ClaimedWorkerQueue::Event(lease) => {
                            let queue_lease_timing = lease.timing();
                            // A delivery its stopped worker left runs alone
                            // (see `CrashQuarantine`).
                            let recovered_delivery = lease.recovered();
                            let sole_delivery_marker = lease.sole_delivery_marker();
                            let last_delivery_attempt = lease.last_attempt();
                            // Keep this delivery's PEL idle clock refreshed while
                            // (possibly many) handlers execute serially, so orphan
                            // reclaim never duplicates a live multi-handler event.
                            // The guard aborts the touch task on every exit path
                            // (ACK, error, panic); see spawn_event_lease_keepalive.
                            let _keepalive_guard = spawn_event_lease_keepalive(
                                &lease,
                                event_orphan_idle_ms,
                                worker_id,
                            );
                            // The claim fence's verdict, for the delivery's
                            // runs to stop on (see `ClaimFence`).
                            let claim = _keepalive_guard.as_ref().map(EventKeepaliveGuard::claim);
                            // Process ONE event message (atomic operation)
                            match lease.process(|message| {
                                let db_ref = db_clone.clone();
                                let blob_store_ref = blob_store_clone.clone();
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
                                let claim_ref = claim.clone();
                                let execution_services_ref = execution_services_clone.clone();
                                let blocking_execution_slots_ref = blocking_execution_slots_clone.clone();
                                let crash_quarantine_ref = crash_quarantine_clone.clone();
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
                                    let _quarantine_pass = crash_quarantine_ref
                                        .enter(recovered_delivery, sole_delivery_marker.as_ref(), worker_id)
                                        .await;
                                    // Check message type and process accordingly
                                    let msg_type = message.head.get_str("__type");

                                    match msg_type.as_str() {
                                        "EventMessage" => {
                                            // TIMING: Start timing from dequeue
                                            let event_dequeue_time = std::time::Instant::now();

                                            // Convert to EventMessage and process
                                            let event_message: EventMessage = message.try_into()
                                                .map_err(|e| Box::new(std::io::Error::other(e)) as Box<dyn std::error::Error + Send + Sync>)?;
                                            let received_event_type =
                                                event_message.body.event.event_type.clone();

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
                                                let infra_retry_backoff_ms = worker_conf_ref
                                                    .get_int_or_default(
                                                        "queue.infra-retry-backoff-ms",
                                                        1_000,
                                                    )
                                                    .max(0) as u64;

                                                debug!("TIMING: event {} dequeued, starting processing", event_message.id);

                                                let mut event_message = match
                                                    hydrate_event_message_from_db(
                                                        worker_id,
                                                        db,
                                                        blob_store_ref.as_ref(),
                                                        event_message,
                                                        infra_retry_backoff_ms,
                                                    )
                                                    .await {
                                                        Ok(message) => message,
                                                        Err(error) if error.downcast_ref::<ParkedEvent>().is_some() => return Ok(()),
                                                        Err(error) => return Err(error),
                                                    };
                                                let scoped_db = Arc::new(db.with_scope(DatabaseScope::Environment {
                                                    org_id: event_message.body.execution_context.org_id.ok_or_else(|| worker_error("Hydrated event has no org scope"))?,
                                                    env_id: event_message.body.event.env_id,
                                                }));
                                                let db = &scoped_db;
                                                event_message.body.execution_context.queue_timing =
                                                    Some(hot::lang::event::QueueExecutionTiming {
                                                        backend: if queue_lease_timing
                                                            .enqueued_at
                                                            .is_some()
                                                        {
                                                            "redis".to_string()
                                                        } else {
                                                            "memory".to_string()
                                                        },
                                                        enqueued_at: queue_lease_timing.enqueued_at,
                                                        claimed_at: queue_lease_timing.claimed_at,
                                                        queue_wait_us: queue_lease_timing
                                                            .queue_wait
                                                            .as_micros()
                                                            .min(u128::from(u64::MAX))
                                                            as u64,
                                                        redelivered: queue_lease_timing.redelivered,
                                                        handler_dispatched_at: None,
                                                    });

                                                debug!("TIMING: event {} security check done: {:?}", event_message.id, event_dequeue_time.elapsed());

                                                // Exact call/schedule build targets bypass the
                                                // environment revision query and use immutable
                                                // build-scoped handler metadata. General events
                                                // retain environment-wide multi-handler routing.
                                                let exact_target_build = event_message
                                                    .body
                                                    .execution_context
                                                    .build_id
                                                    .filter(|_| {
                                                        event_message.body.event.event_type
                                                            == "hot:call"
                                                            || event_message.body.event.event_type
                                                                == "hot:schedule"
                                                    });
                                                let handlers_result =
                                                    if let Some(build_id) = exact_target_build {
                                                        EventHandler::get_event_handlers_by_build_and_event_type_cached(
                                                            db,
                                                            &build_id,
                                                            &event_message.body.event.event_type,
                                                        )
                                                        .await
                                                    } else {
                                                        EventHandler::get_event_handlers_by_env_and_event_type(
                                                            db,
                                                            &env_id,
                                                            &event_message.body.event.event_type,
                                                        )
                                                        .await
                                                    };

                                                match handlers_result {
                                                    Ok(all_handlers) => {
                                                        if all_handlers.is_empty() {
                                                            let err_msg = format!("No event handlers found for event type '{}'", event_message.body.event.event_type);
                                                            if exact_target_build.is_some() {
                                                                // A build-scoped event was published while its deployment
                                                                // was changing. Keep it in the backend retry/DLQ path rather
                                                                // than acknowledging it as permanently unhandled.
                                                                return Err(Box::new(std::io::Error::other(err_msg))
                                                                    as Box<dyn std::error::Error + Send + Sync>);
                                                            }
                                                            acknowledge_unhandled_event_if_stable(
                                                                worker_id,
                                                                db,
                                                                &env_id,
                                                                &event_message.body.event.event_type,
                                                                err_msg,
                                                                infra_retry_backoff_ms,
                                                            ).await?;
                                                            return Ok(());
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
                                                                    let target_build_id = event_message.body.execution_context.build_id;
                                                                    let target_project_id = event_message.body.event.target_project_id;

                                                                    // Exact build targets were already verified during DB hydration to
                                                                    // belong to this environment/project. Route directly to that build's
                                                                    // once-handler instead of scanning every project/build and reopening
                                                                    // bytecode just to rediscover the authoritative target.
                                                                    if let Some(target_build_id) = target_build_id {
                                                                        if let Some(handler) = once_handlers.iter().find(|h| h.build_id == target_build_id) {
                                                                            handlers_to_execute.push(handler.clone());
                                                                        } else {
                                                                            let err_msg = format!(
                                                                                "Function '{}' targeted build {} but no event handler is registered for that build",
                                                                                target_fn, target_build_id
                                                                            );
                                                                            error!("hot.dev: WORKER {} {}", worker_id, err_msg);
                                                                            return Err(Box::new(std::io::Error::other(err_msg)) as Box<dyn std::error::Error + Send + Sync>);
                                                                        }
                                                                    // Legacy events without an immutable build target retain function
                                                                    // discovery and project tie-breaking behavior.
                                                                    } else {
                                                                        match find_build_for_function(db, &env_id, &target_fn, &cache_ref, &worker_conf_ref, (None, target_project_id), &build_path_cache_ref).await {
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
                                                                            // A deployment this runtime can't run may have it:
                                                                            // the call waits for its redeploy.
                                                                            if env_awaits_redeploy(db, &env_id).await.map_err(|e| worker_infrastructure_retry_error(e, infra_retry_backoff_ms.max(5_000)))? {
                                                                                return Err(worker_infrastructure_retry_error(
                                                                                    format!("Function '{}' waits for a deployment to be redeployed for this version of Hot", target_fn),
                                                                                    infra_retry_backoff_ms.max(AWAITING_REDEPLOY_BACKOFF_MS),
                                                                                ));
                                                                            }
                                                                            // Function not in any active build — expected during deploy transitions
                                                                            // when previously enqueued events/schedules reference old code
                                                                            let err_msg = format!(
                                                                                "Function '{}' not found in any deployed build. This may be due to cache issues - try redeploying the build.",
                                                                                target_fn
                                                                            );
                                                                            let deploying = should_defer_missing_event_handlers(worker_id, db, &env_id, &event_message.body.event.event_type, infra_retry_backoff_ms).await?;
                                                                            let call_handler = if event_message.body.event.event_type == "hot:call" && !deploying {
                                                                                handler_for_missing_function(db, &once_handlers, target_project_id).await
                                                                                    .map_err(|e| worker_infrastructure_retry_error(e, infra_retry_backoff_ms.max(5_000)))?
                                                                            } else {
                                                                                None
                                                                            };
                                                                            let Some(handler) = call_handler else {
                                                                                warn!("hot.dev: WORKER {} {}", worker_id, err_msg);
                                                                                return Err(Box::new(std::io::Error::other(err_msg)) as Box<dyn std::error::Error + Send + Sync>);
                                                                            };
                                                                            // With no deployment under way the function is gone, and
                                                                            // retrying only makes its caller wait for nothing. The call
                                                                            // handler's run fails resolving it, which records the
                                                                            // failure and publishes run:fail.
                                                                            warn!("hot.dev: WORKER {} {}; failing the call", worker_id, err_msg);
                                                                            handlers_to_execute.push(handler);
                                                                            routing_warning = Some(serde_json::json!({
                                                                                "warning": "route:missing",
                                                                                "function": target_fn
                                                                            }));
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
                                                            acknowledge_unhandled_event_if_stable(
                                                                worker_id,
                                                                db,
                                                                &env_id,
                                                                &event_message.body.event.event_type,
                                                                err_msg,
                                                                infra_retry_backoff_ms,
                                                            ).await?;
                                                            return Ok(());
                                                        }

                                                        debug!("hot.dev: WORKER {} executing {} handler(s) for event type '{}'",
                                                            worker_id, handlers_to_execute.len(), event_message.body.event.event_type);

                                                        // A redelivered event derives the same handler run ids, so
                                                        // the runs of its earlier deliveries say what is left:
                                                        // handlers that finished don't run again, and runs crash
                                                        // recovery held are reopened before their handlers execute.
                                                        // A first delivery has no earlier runs to read.
                                                        let earlier_runs = if queue_lease_timing.redelivered {
                                                            hot::db::run::Run::earlier_deliveries(db, &event_message.body.event.event_id).await
                                                                .map_err(|e| worker_infrastructure_retry_error(format!("Cannot read the event's earlier runs: {e}"), infra_retry_backoff_ms.max(5_000)))?
                                                        } else {
                                                            Default::default()
                                                        };
                                                        let mut finished_earlier = 0usize;
                                                        let mut held_runs = Vec::new();

                                                        // Resolve and reserve the entire event fan-out before any
                                                        // handler can execute. Durable IDs prevent infra redelivery
                                                        // from consuming additional monthly run slots.
                                                        let mut admitted_handlers = Vec::new();
                                                        // Handlers of a deployment this runtime can't run.
                                                        let mut awaiting_redeploy = Vec::new();
                                                        for event_handler in handlers_to_execute {
                                                            let handler_build = Build::get_build(db, &event_handler.build_id).await
                                                                .map_err(|e| worker_infrastructure_retry_error(format!("Cannot resolve handler build: {e}"), infra_retry_backoff_ms.max(5_000)))?;
                                                            if !handler_build.deployed || !handler_build.active || handler_build.runtime_status != Build::RUNTIME_STATUS_READY {
                                                                continue;
                                                            }
                                                            if let Some(reason) = handler_build.runtime_version_error_message() {
                                                                // A newer worker, as a rolling deploy starts, can run
                                                                // the build's work.
                                                                if exact_target_build.is_some() && build_needs_newer_runtime(&handler_build) {
                                                                    if wait_log_due(handler_build.build_id) {
                                                                        warn!(
                                                                            event_id = %event_message.body.event.event_id,
                                                                            build_id = %handler_build.build_id,
                                                                            "hot.dev: WORKER {} leaves event '{}' to a newer worker: {}",
                                                                            worker_id,
                                                                            event_message.body.event.event_type,
                                                                            reason
                                                                        );
                                                                    }
                                                                    return Err(worker_infrastructure_retry_error(
                                                                        format!("{reason}; the event waits for a newer worker"),
                                                                        infra_retry_backoff_ms.max(NEWER_BUILD_RETRY_BACKOFF_MS),
                                                                    ));
                                                                }
                                                                // Other work bound to the build can never run here.
                                                                if exact_target_build.is_some() {
                                                                    return Err(worker_error(format!(
                                                                        "{reason}. Redeploy the project, and send its work again"
                                                                    )));
                                                                }
                                                                awaiting_redeploy.push((handler_build.build_id, reason, build_needs_newer_runtime(&handler_build)));
                                                                continue;
                                                            }
                                                            let run_id = event_handler_run_id(
                                                                event_message.body.event.event_id,
                                                                handler_build.project_id,
                                                                &event_message.body.event.event_type,
                                                                &event_handler,
                                                                event_message.body.execution_context.retry_attempt,
                                                            );
                                                            match earlier_runs.get(&run_id) {
                                                                Some(hot::db::run::EarlierDelivery::Finished) => {
                                                                    finished_earlier += 1;
                                                                    continue;
                                                                }
                                                                Some(hot::db::run::EarlierDelivery::Held) => held_runs.push(run_id),
                                                                Some(hot::db::run::EarlierDelivery::Running) | None => {}
                                                            }
                                                            admitted_handlers.push((event_handler, handler_build, run_id));
                                                        }
                                                        if let Some((build_id, reason, _)) = awaiting_redeploy.first() {
                                                            // An event only such deployments handle waits for
                                                            // their redeploy. Others run their handlers without it.
                                                            let waits = admitted_handlers.is_empty();
                                                            if wait_log_due(*build_id) {
                                                                warn!(
                                                                    event_id = %event_message.body.event.event_id,
                                                                    %build_id,
                                                                    "hot.dev: WORKER {} {} the handlers of {} deployment(s) waiting for a redeploy for event '{}': {}",
                                                                    worker_id,
                                                                    if waits { "holds the event for" } else { "skips" },
                                                                    awaiting_redeploy.len(),
                                                                    event_message.body.event.event_type,
                                                                    reason
                                                                );
                                                            }
                                                            if waits {
                                                                // A newer worker may come soon; a redeploy, later.
                                                                let backoff_ms = if awaiting_redeploy.iter().all(|(_, _, newer)| *newer) {
                                                                    NEWER_BUILD_RETRY_BACKOFF_MS
                                                                } else {
                                                                    AWAITING_REDEPLOY_BACKOFF_MS
                                                                };
                                                                return Err(worker_infrastructure_retry_error(
                                                                    format!("{reason}; the event waits for a redeploy"),
                                                                    infra_retry_backoff_ms.max(backoff_ms),
                                                                ));
                                                            }
                                                        }
                                                        // Runs an earlier delivery left running or held for
                                                        // handlers a delivery that settles the event doesn't run,
                                                        // after a redeploy removed them, would never end.
                                                        let unrun_earlier: Vec<Uuid> = earlier_runs
                                                            .iter()
                                                            .filter(|(run_id, state)| {
                                                                **state != hot::db::run::EarlierDelivery::Finished
                                                                    && !admitted_handlers.iter().any(|(_, _, admitted)| admitted == *run_id)
                                                            })
                                                            .map(|(run_id, _)| *run_id)
                                                            .collect();
                                                        if admitted_handlers.is_empty() && finished_earlier > 0 {
                                                            debug!(
                                                                "hot.dev: WORKER {} acknowledges redelivered event {}: its {} handler run(s) finished in an earlier delivery",
                                                                worker_id, event_message.body.event.event_id, finished_earlier
                                                            );
                                                            fail_unrun_earlier_deliveries(db, &unrun_earlier).await;
                                                            return Ok(());
                                                        }
                                                        let run_ids: Vec<Uuid> = admitted_handlers.iter().map(|(_, _, id)| *id).collect();
                                                        let org_id = event_message.body.execution_context.org_id.ok_or_else(|| worker_error("Hydrated event has no organization"))?;
                                                        hot::db::run::Run::admit_executions(db, &org_id, &env_id, &run_ids).await
                                                            .map_err(|e| match e {
                                                                // Database errors leave accepted work pending.
                                                                hot::db::run::RunError::Database(_) => {
                                                                    worker_infrastructure_retry_error(e.to_string(), infra_retry_backoff_ms.max(5_000))
                                                                }
                                                                // A suspended account or environment, or a reached account
                                                                // limit, also leaves the event pending: it runs once the
                                                                // account resumes, the period rolls over or the limit is raised.
                                                                hot::db::run::RunError::Suspended(_) | hot::db::run::RunError::QuotaExceeded(_) => {
                                                                    if wait_log_due(org_id) {
                                                                        warn!(
                                                                            event_id = %event_message.body.event.event_id,
                                                                            %org_id,
                                                                            "hot.dev: WORKER {} events wait for run admission: {}",
                                                                            worker_id,
                                                                            e
                                                                        );
                                                                    }
                                                                    worker_infrastructure_retry_error(e.to_string(), infra_retry_backoff_ms.max(60_000))
                                                                }
                                                                _ => worker_error(e.to_string()),
                                                            })?;
                                                        // No handler has started yet, so a failure here can still
                                                        // leave the whole event for redelivery.
                                                        hot::db::run::Run::reopen_held_runs(db, &held_runs).await
                                                            .map_err(|e| worker_infrastructure_retry_error(format!("Cannot reopen the event's held runs: {e}"), infra_retry_backoff_ms.max(5_000)))?;
                                                        // With no handler to run, the event is delivered again.
                                                        if !admitted_handlers.is_empty() {
                                                            fail_unrun_earlier_deliveries(db, &unrun_earlier).await;
                                                        }
                                                        // A recovered delivery may stop this worker before its
                                                        // runs record themselves: write their rows first, so
                                                        // dead-letter settlement can fail them.
                                                        if recovered_delivery {
                                                            for (_, handler_build, run_id) in &admitted_handlers {
                                                                let row = event_handler_run_row(&event_message, handler_build, *run_id);
                                                                if let Err(error) = hot::db::run::Run::open_event_run(db, &row).await {
                                                                    warn!("hot.dev: WORKER {} could not write run {} before running its recovered event: {}", worker_id, run_id, error);
                                                                }
                                                            }
                                                        }

                                                        // Step 4: Execute the selected handlers
                                                        let execution_result: (Result<(), String>, bool) = {
                                                            let mut all_success = true;
                                                            let mut started_any_handler = false;
                                                            let mut is_first_handler = true;
                                                            for (event_handler, handler_build, run_id) in admitted_handlers {
                                                                // A delivery whose claim is gone starts no more
                                                                // runs, and is not ACKed: the worker that has the
                                                                // event, or this one's next delivery of it, runs
                                                                // the rest (finished handlers are skipped).
                                                                if let Some(loss) = claim_stops_new_runs(claim_ref.as_ref(), tokio::time::Instant::now()) {
                                                                    warn!("hot.dev: WORKER {} starts no more handlers for event {}: {:?}", worker_id, event_message.body.event.event_id, loss);
                                                                    return Err(worker_error(format!("Event handlers not started: {}", RunStop::ClaimLost(loss).error())));
                                                                }
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

                                                                let mut handler_event_message = event_message.clone();
                                                                if let Some(timing) = handler_event_message
                                                                    .body
                                                                    .execution_context
                                                                    .queue_timing
                                                                    .as_mut()
                                                                {
                                                                    timing.handler_dispatched_at = Some(chrono::Utc::now());
                                                                }
                                                                let execution_future = execute_single_event_handler(db, &handler_build, &env_id, &worker_conf_ref, &event_handler, &handler_event_message, _emitter_ref.clone(), _event_publisher_ref.clone(), encryption_ref.clone(), cache_ref.clone(), shutdown_coord_ref.clone(), run_id, build_path_cache_ref.clone(), dev_context_storage_ref.clone(), stream_publisher_ref.clone(), task_queue_ref.clone(), execution_services_ref.clone(), blocking_execution_slots_ref.clone(), claim_ref.clone());

                                                                                            match execution_future.await {
                                                                                                Ok(()) => {
                                                                                                    started_any_handler = true;
                                                                                                    debug!("Successfully executed event handler: {}", event_handler.event_handler_id);
                                                                                                    // Run is unregistered by execute_single_event_handler's RAII guard on every exit path

                                                                                                    // Update run info with routing warning if this is the first handler
                                                                                                    // and a tie-breaker was used (routing_warning is set)
                                                                                                    if is_first_handler
                                                                                                        && let Some(ref warning_info) = routing_warning
                                                                                                            && let Err(e) = hot::db::run::Run::update_info(db, &run_id, Some(warning_info)).await {
                                                                                                                warn!("hot.dev: WORKER {} failed to update run {} info with routing warning: {}", worker_id, run_id, e);
                                                                                                            }
                                                                                                }
                                                                                                Err(e) if should_defer_run_admission(&e, started_any_handler) => {
                                                                                                    // No bytecode or run:start was emitted. A suspension
                                                                                                    // or transient quota lookup must leave accepted work
                                                                                                    // pending without exhausting its delivery budget.
                                                                                                    return Err(worker_infrastructure_retry_error(e, infra_retry_backoff_ms.max(5_000)));
                                                                                                }
                                                                                                Err(e) if should_redeliver_event_after_handler_error(&e, started_any_handler) => {
                                                                                                    // Setup or blocking-slot admission timed out before any run side
                                                                                                    // effects for this event: leave it un-ACKed via the standard
                                                                                                    // infrastructure-retry path so it is redelivered once detached
                                                                                                    // blocking work frees capacity. (If an earlier handler already
                                                                                                    // started a run, the arm below applies instead: redelivering the
                                                                                                    // whole event would duplicate that run's side effects.)
                                                                                                    //
                                                                                                    // These redeliveries opt into cumulative delivery counting: the
                                                                                                    // infra requeue otherwise resets the delivery counter, and a
                                                                                                    // deterministically failing setup would loop forever instead of
                                                                                                    // escalating to the DLQ.
                                                                                                    warn!("hot.dev: WORKER {} deferring event '{}' for redelivery: {}", worker_id, event_message.body.event.event_type, e);
                                                                                                    return Err(worker_counted_redelivery_error(e, infra_retry_backoff_ms));
                                                                                                }
                                                                                                Err(e) if run_never_started(&e).is_some() => {
                                                                                                    let reason = run_never_started(&e).unwrap_or(&e);
                                                                                                    // The handler's setup failed before its run started:
                                                                                                    // nothing ran, and no run records it. Before any handler
                                                                                                    // started, the event is delivered again, counted toward
                                                                                                    // its retry limit, and the run is held with the failure
                                                                                                    // for the redelivery to reopen. After one started, or on
                                                                                                    // the event's last attempt, the failure is the run's.
                                                                                                    let failure = format!("Handler setup failed before the run started: {reason}");
                                                                                                    if !started_any_handler && !last_delivery_attempt {
                                                                                                        warn!("hot.dev: WORKER {} deferring event '{}' for redelivery: {}", worker_id, event_message.body.event.event_type, failure);
                                                                                                        record_unstarted_run(db, &event_message, &handler_build, run_id, &failure, true).await;
                                                                                                        return Err(worker_counted_redelivery_error(failure, infra_retry_backoff_ms.max(SETUP_FAILURE_BACKOFF_MS)));
                                                                                                    }
                                                                                                    error!("hot.dev: WORKER {} failed event handler {}: {}", worker_id, event_handler.event_handler_id, failure);
                                                                                                    record_unstarted_run(db, &event_message, &handler_build, run_id, &failure, false).await;
                                                                                                    started_any_handler = true;
                                                                                                    all_success = false;
                                                                                                }
                                                                                                Err(e) => {
                                                                                                    started_any_handler = true;
                                                                                                    error!("Failed to execute event handler {}: {}", event_handler.event_handler_id, e);
                                                                                                    all_success = false;
                                                                                                    // Run is unregistered by execute_single_event_handler's RAII guard on every exit path

                                                                                                    // Still update run info with routing warning even on failure
                                                                                                    if is_first_handler
                                                                                                        && let Some(ref warning_info) = routing_warning
                                                                                                            && let Err(e) = hot::db::run::Run::update_info(db, &run_id, Some(warning_info)).await {
                                                                                                                warn!("hot.dev: WORKER {} failed to update run {} info with routing warning: {}", worker_id, run_id, e);
                                                                                                            }
                                                                                                    // Continue with other handlers even if one fails
                                                                                                }
                                                                                            }
                                                                                            is_first_handler = false;
                                                                                        }
                                                                                        if started_any_handler {
                                                                                            // At least one run was created. Do not retry the whole
                                                                                            // event and duplicate already-started side effects.
                                                                                            (Ok(()), all_success)
                                                                                        } else {
                                                                                            (
                                                                                                Err("No runtime-visible event handler could be started".to_string()),
                                                                                                false,
                                                                                            )
                                                                                        }
                                                                                    };

                                                                                    // The ACK is group-wide: once another worker has the
                                                                                    // event, it would settle that worker's delivery.
                                                                                    if matches!(claim_ref.as_ref().map(|claim| *claim.borrow()), Some(Some(ClaimLoss::Lost))) {
                                                                                        return Err(worker_error(RunStop::ClaimLost(ClaimLoss::Lost).error()));
                                                                                    }

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
                                                                                            let err_msg = format!("Failed to execute event handlers for event type '{}': {}", event_message.body.event.event_type, e);
                                                                                            error!("hot.dev: WORKER {} {}", worker_id, err_msg);
                                                                                            return Err(Box::new(std::io::Error::other(err_msg)) as Box<dyn std::error::Error + Send + Sync>);
                                                                                        }
                                                                                    }
                                                                    }
                                                                    Err(e) => {
                                                                        let err_msg = format!("Failed to get event handlers: {}", e);
                                                                        error!("hot.dev: WORKER {} {}", worker_id, err_msg);
                                                                        return Err(worker_infrastructure_retry_error(
                                                                            err_msg,
                                                                            infra_retry_backoff_ms,
                                                                        ));
                                                                    }
                                                                }
                                                            } else {
                                                                let err_msg = "Cannot process events: no database connection".to_string();
                                                                error!("hot.dev: WORKER {} {}", worker_id, err_msg);
                                                                return Err(Box::new(std::io::Error::other(err_msg)) as Box<dyn std::error::Error + Send + Sync>);
                                                            }

                                            debug!("hot.dev: WORKER {} processed event '{}' successfully",
                                                worker_id,
                                                received_event_type);
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
                                                                        if !hot::db::Org::is_environment_admitted(db, &env.env_id).await.map_err(|error| worker_infrastructure_retry_error(error.to_string(), 5_000))? {
                                                                            return Err(worker_infrastructure_retry_error("environment is suspended; deployment remains pending", 5_000));
                                                                        }
                                                                        let scoped_db = Arc::new(db.with_scope(DatabaseScope::Environment { org_id: env.org_id, env_id: env.env_id }));
                                                                        let db = &scoped_db;
                                                                        debug!("hot.dev: WORKER {} processing deployment for build {} (env: {}, org: {})",
                                                                            worker_id,
                                                                            build.build_id,
                                                                            project.env_id,
                                                                            env.org_id);

                                                                        let already_ready = build.deployed
                                                                            && build.runtime_status == Build::RUNTIME_STATUS_READY;
                                                                        if already_ready {
                                                                            debug!("hot.dev: WORKER {} deployment for build {} is already ready and active",
                                                                                worker_id,
                                                                                build.build_id);
                                                                        } else if let Err(e) = Build::mark_runtime_loading(db, &build.build_id).await {
                                                                            warn!("hot.dev: WORKER {} failed to mark build {} loading: {}",
                                                                                worker_id,
                                                                                build.build_id,
                                                                                e);
                                                                        }

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

                                                                                        // A build of a newer Hot of this major version waits a
                                                                                        // while for a newer worker of a rolling deploy instead
                                                                                        // of failing here.
                                                                                        if let Ok(Some(reason)) = hot::build::newer_runtime_requirement(&build_data) {
                                                                                            let waited = deployment_wait(deployment_message.id, build.updated_at);
                                                                                            if waited < NEWER_RUNTIME_DEPLOY_GRACE {
                                                                                                if wait_log_due(build.build_id) {
                                                                                                    warn!(
                                                                                                        build_id = %build.build_id,
                                                                                                        "hot.dev: WORKER {} leaves deployment of build {} to a newer worker: {}",
                                                                                                        worker_id,
                                                                                                        build.build_id,
                                                                                                        reason
                                                                                                    );
                                                                                                }
                                                                                                return Err(worker_infrastructure_retry_error(
                                                                                                    format!("{reason}; the deployment waits for a newer worker"),
                                                                                                    NEWER_BUILD_RETRY_BACKOFF_MS,
                                                                                                ));
                                                                                            }
                                                                                            let err_msg = format!(
                                                                                                "{reason}. No worker of a newer Hot took the deployment within {} minutes; deploy with this Hot version or upgrade the workers",
                                                                                                NEWER_RUNTIME_DEPLOY_GRACE.as_secs() / 60
                                                                                            );
                                                                                            error!("hot.dev: WORKER {} {}", worker_id, err_msg);
                                                                                            if let Err(status_err) = Build::mark_runtime_failed(db, &build.build_id, &err_msg).await {
                                                                                                warn!("hot.dev: WORKER {} failed to mark build {} failed: {}",
                                                                                                    worker_id,
                                                                                                    build.build_id,
                                                                                                    status_err);
                                                                                            }
                                                                                            hot::db::alert::publish_deploy_failed_alert(
                                                                                                db,
                                                                                                &env.org_id,
                                                                                                &project.env_id,
                                                                                                &build.build_id,
                                                                                                &project.name,
                                                                                                &err_msg,
                                                                                            ).await;
                                                                                            return Err(worker_error(err_msg));
                                                                                        }

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
                                                                                                let holds_file_lock = _file_lock_guard.is_some();
                                                                                                let extracted = clear_partial_bundle_extraction(&extract_dir, holds_file_lock)
                                                                                                    .and_then(|()| hot::bundle::extract_bundle_from_bytes(&build_data, &extract_dir));
                                                                                                if extracted.is_err() && holds_file_lock {
                                                                                                    // Left behind, it would make every later extraction fail.
                                                                                                    let _ = std::fs::remove_dir_all(&extract_dir);
                                                                                                }
                                                                                                match extracted {
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
                                                                                                        debug!("Pre-compiling bundle {} to generate bytecode cache", build.build_id);
                                                                                                        match compile_event_bundle_to_cache(
                                                                                                            &extract_dir,
                                                                                                            &paths,
                                                                                                            &bundle_cache,
                                                                                                            &proj_name,
                                                                                                            cache_key.as_deref(),
                                                                                                            file_hashes,
                                                                                                        ) {
                                                                                                            Ok(()) => {
                                                                                                                debug!("Bundle {} pre-compiled successfully", build.build_id);

                                                                                                                // Mark extraction complete AFTER bytecode is ready
                                                                                                                BuildPathCache::mark_extraction_complete(&extract_dir);

                                                                                                                // Store the extracted path in the cache
                                                                                                                build_path_cache_ref.insert(build.build_id, extract_dir);
                                                                                                            }
                                                                                                            Err(e) => {
                                                                                                                let err_msg = format!("Failed to pre-compile bundle {}: {}", build.build_id, e);
                                                                                                                error!("hot.dev: WORKER {} {}", worker_id, err_msg);
                                                                                                                if let Err(status_err) = Build::mark_runtime_failed(db, &build.build_id, &err_msg).await {
                                                                                                                    warn!("hot.dev: WORKER {} failed to mark build {} failed: {}",
                                                                                                                        worker_id,
                                                                                                                        build.build_id,
                                                                                                                        status_err);
                                                                                                                }
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
                                                                                                        let err_msg = format!("Failed to extract build {}: {}", build.build_id, e);
                                                                                                        error!("hot.dev: WORKER {} {}", worker_id, err_msg);
                                                                                                        if let Err(status_err) = Build::mark_runtime_failed(db, &build.build_id, &err_msg).await {
                                                                                                            warn!("hot.dev: WORKER {} failed to mark build {} failed: {}",
                                                                                                                worker_id,
                                                                                                                build.build_id,
                                                                                                                status_err);
                                                                                                        }
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

                                                                                                let deployment_user_id = build
                                                                                                    .updated_by_user_id
                                                                                                    .unwrap_or(build.created_by_user_id);
                                                                                                match Build::activate_prepared_build(db, &build.build_id, &deployment_user_id).await {
                                                                                                    Ok(true) => {
                                                                                                        // Publish deploy:succeeded alert only after the atomic activation flip succeeds.
                                                                                                        hot::db::alert::publish_deploy_succeeded_alert(
                                                                                                            db,
                                                                                                            &env.org_id,
                                                                                                            &project.env_id,
                                                                                                            &build.build_id,
                                                                                                            &project.name,
                                                                                                        ).await;
                                                                                                    }
                                                                                                    Ok(false) => {
                                                                                                        debug!("hot.dev: WORKER {} skipped activation for stale or inactive build {}",
                                                                                                            worker_id,
                                                                                                            build.build_id);
                                                                                                        if !already_ready
                                                                                                            && let Err(status_err) = Build::mark_runtime_superseded(db, &build.build_id, "superseded by newer deployment or inactive environment").await
                                                                                                        {
                                                                                                            warn!("hot.dev: WORKER {} failed to mark build {} superseded: {}",
                                                                                                                worker_id,
                                                                                                                build.build_id,
                                                                                                                status_err);
                                                                                                        }
                                                                                                    }
                                                                                                    Err(hot::db::build::BuildError::Database(error))=>return Err(worker_infrastructure_retry_error(format!("Build activation is waiting for its database: {error}"),5_000)),
                                                                                                    Err(e) => {
                                                                                                        let err_msg = format!("Failed to activate build {}: {}", build.build_id, e);
                                                                                                        error!("hot.dev: WORKER {} {}", worker_id, err_msg);
                                                                                                        if let Err(status_err) = Build::mark_runtime_failed(db, &build.build_id, &err_msg).await {
                                                                                                            warn!("hot.dev: WORKER {} failed to mark build {} failed: {}",
                                                                                                                worker_id,
                                                                                                                build.build_id,
                                                                                                                status_err);
                                                                                                        }
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
                                                                                                let err_msg = format!("Failed to load handlers and schedules for build {}: {}", build.build_id, e);
                                                                                                error!("hot.dev: WORKER {} {}", worker_id, err_msg);
                                                                                                if let Err(status_err) = Build::mark_runtime_failed(db, &build.build_id, &err_msg).await {
                                                                                                    warn!("hot.dev: WORKER {} failed to mark build {} failed: {}",
                                                                                                        worker_id,
                                                                                                        build.build_id,
                                                                                                        status_err);
                                                                                                }
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
                                                                                        if let Err(status_err) = Build::mark_runtime_failed(db, &build.build_id, &err_msg).await {
                                                                                            warn!("hot.dev: WORKER {} failed to mark build {} failed: {}",
                                                                                                worker_id,
                                                                                                build.build_id,
                                                                                                status_err);
                                                                                        }
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
                                                                                if let Err(status_err) = Build::mark_runtime_failed(db, &build.build_id, &err_msg).await {
                                                                                    warn!("hot.dev: WORKER {} failed to mark build {} failed: {}",
                                                                                        worker_id,
                                                                                        build.build_id,
                                                                                        status_err);
                                                                                }
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

                                            debug!("hot.dev: WORKER {} received maintenance task: id={} tasks={:?}",
                                                worker_id,
                                                maint_message.id,
                                                maint_message.body.tasks);

                                            if let Some(ref db) = db_ref {
                                                for task in &maint_message.body.tasks {
                                                    let operation = match task.as_str() {
                                                        "session_cleanup" | "inactive_schedule_cleanup" | "upload_cleanup" | "file_object_cleanup" | "call_retention_cleanup" | "run_admission_cleanup" => MaintenanceOperation::Retention,
                                                        "blob_cleanup" => MaintenanceOperation::BlobGc,
                                                        _ => MaintenanceOperation::Queue,
                                                    };
                                                    let scoped_db = Arc::new(db.with_scope(DatabaseScope::Maintenance { operation, org_id: None }));
                                                    let db = &scoped_db;
                                                    match task.as_str() {
                                                        "session_cleanup" => {
                                                            match hot::db::session::Session::cleanup_expired(db).await {
                                                                Ok(count) => {
                                                                    if count > 0 {
                                                                        debug!("hot.dev: WORKER {} maintenance: cleaned up {} expired sessions", worker_id, count);
                                                                    }
                                                                }
                                                                Err(e) => {
                                                                    error!("hot.dev: WORKER {} maintenance: session cleanup failed: {}", worker_id, e);
                                                                }
                                                            }
                                                        }
                                                        "run_admission_cleanup" => {
                                                            match hot::db::Run::delete_admissions_older_than(db, hot::db::run::ADMISSION_RETENTION_DAYS, 10_000).await {
                                                                Ok(count) => {
                                                                    if count > 0 {
                                                                        debug!("hot.dev: WORKER {} maintenance: pruned {} run admission(s)", worker_id, count);
                                                                    }
                                                                }
                                                                Err(e) => {
                                                                    error!("hot.dev: WORKER {} maintenance: run admission cleanup failed: {}", worker_id, e);
                                                                }
                                                            }
                                                        }
                                                        "inactive_schedule_cleanup" => {
                                                            match hot::db::Schedule::delete_old_inactive_schedules(db, 30).await {
                                                                Ok(count) => {
                                                                    if count > 0 {
                                                                        debug!("hot.dev: WORKER {} maintenance: cleaned up {} inactive schedules", worker_id, count);
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
                                                                        debug!("hot.dev: WORKER {} maintenance: checking {} unverified domains", worker_id, domains.len());
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
                                                        "file_object_cleanup" => {
                                                            // Objects retired by overwrites, deletes and interrupted
                                                            // writes, once their grace period has passed.
                                                            match hot::file_storage::file_storage_from_config(&worker_conf_ref).await {
                                                                Ok(storage) => {
                                                                    const BATCH: usize = 1_000;
                                                                    // Batches until the backlog is cleared, within a
                                                                    // budget well inside the 5-minute schedule.
                                                                    let budget = std::time::Instant::now() + std::time::Duration::from_secs(60);
                                                                    let retired_before = chrono::Utc::now()
                                                                        - hot::file_storage::RETIRED_OBJECT_GRACE;
                                                                    let mut deleted = 0;
                                                                    loop {
                                                                        match hot::file_storage::collect_retired_objects(db, storage.as_ref(), retired_before, BATCH as i64).await {
                                                                            Ok(n) => {
                                                                                deleted += n;
                                                                                if n < BATCH || std::time::Instant::now() >= budget {
                                                                                    break;
                                                                                }
                                                                            }
                                                                            Err(e) => {
                                                                                error!("hot.dev: WORKER {} file_object_cleanup: failed: {}", worker_id, e);
                                                                                break;
                                                                            }
                                                                        }
                                                                    }
                                                                    if deleted > 0 {
                                                                        info!("hot.dev: WORKER {} file_object_cleanup: deleted {} retired file object(s)", worker_id, deleted);
                                                                    }
                                                                }
                                                                Err(e) => {
                                                                    error!("hot.dev: WORKER {} file_object_cleanup: storage unavailable: {}", worker_id, e);
                                                                }
                                                            }
                                                        }
                                                        "upload_cleanup" => {
                                                            match hot::db::file_upload::cleanup_expired_uploads(db).await {
                                                                Ok(expired) => {
                                                                    if !expired.is_empty() {
                                                                        info!("hot.dev: WORKER {} upload_cleanup: aborting {} expired uploads", worker_id, expired.len());
                                                                        let cleanup_storage = hot::file_storage::file_storage_from_config(&worker_conf_ref).await;
                                                                        for upload in &expired {
                                                                            let Some(env_id) = upload.env_id else {
                                                                                error!("hot.dev: WORKER {} upload_cleanup: upload {} has no environment; refusing unscoped cleanup", worker_id, upload.upload_id);
                                                                                continue;
                                                                            };
                                                                            let Ok(storage) = &cleanup_storage else {
                                                                                error!("hot.dev: WORKER {} upload_cleanup: storage unavailable for upload {}", worker_id, upload.upload_id);
                                                                                if let Err(e) = hot::db::file_upload::release_cleanup_claim(db, upload.upload_id, upload.org_id, env_id).await {
                                                                                    error!("hot.dev: WORKER {} upload_cleanup: failed to release claim for upload {}: {}", worker_id, upload.upload_id, e);
                                                                                }
                                                                                continue;
                                                                            };
                                                                            if upload.storage_backend != storage.storage_type() {
                                                                                // Never route an upload through a backend that did not
                                                                                // create it; its staged bytes need the migration's cleanup.
                                                                                error!("hot.dev: WORKER {} upload_cleanup: upload {} belongs to the '{}' backend but '{}' is configured; marking aborted without deleting staged bytes", worker_id, upload.upload_id, upload.storage_backend, storage.storage_type());
                                                                            } else if let Some(backend_upload_id) = upload.backend_upload_id.as_deref() {
                                                                                let ctx = hot::file_storage::FileStorageContext::minimal(
                                                                                    db.clone(),
                                                                                    upload.org_id,
                                                                                    Some(env_id),
                                                                                    upload.created_by_user_id,
                                                                                );
                                                                                if let Err(e) = storage.abort_multipart_upload(backend_upload_id, &upload.path, &ctx).await {
                                                                                    error!("hot.dev: WORKER {} upload_cleanup: backend abort failed for upload {}: {}", worker_id, upload.upload_id, e);
                                                                                    if let Err(release_error) = hot::db::file_upload::release_cleanup_claim(db, upload.upload_id, upload.org_id, env_id).await {
                                                                                        error!("hot.dev: WORKER {} upload_cleanup: failed to release claim for upload {}: {}", worker_id, upload.upload_id, release_error);
                                                                                    }
                                                                                    continue;
                                                                                }
                                                                            }
                                                                            if let Err(e) = hot::db::file_upload::abort_upload(db, upload.upload_id, upload.org_id, env_id).await {
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
                                                                "Run abandoned: unfinished past max timeout (likely worker crash)",
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
                                                            // Blob refs sourced from calls follow the same retention:
                                                            // when call rows age out, their call_* refs deactivate so
                                                            // blob GC can reclaim the backing objects.
                                                            const CALL_REF_KINDS: &[&str] = &["call_args", "call_return", "call_flow"];
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
                                                                    let cutoff = chrono::Utc::now() - chrono::Duration::days(days as i64);
                                                                    match hot::db::blob::deactivate_refs_older_than(&db.with_scope(DatabaseScope::Maintenance { operation: MaintenanceOperation::BlobGc, org_id: None }), None, CALL_REF_KINDS, cutoff).await {
                                                                        Ok(n) if n > 0 => {
                                                                            info!("hot.dev: WORKER {} maintenance: deactivated {} expired call blob ref(s)", worker_id, n);
                                                                        }
                                                                        Ok(_) => {}
                                                                        Err(e) => {
                                                                            error!("hot.dev: WORKER {} maintenance: call blob ref deactivation failed: {}", worker_id, e);
                                                                        }
                                                                    }
                                                                }
                                                            } else {
                                                                let mut org_ids = hot::db::subscription::OrgPlan::get_all_root_org_ids(db).await.unwrap_or_default();
                                                                for root_id in org_ids.clone() {
                                                                    if let Ok(children) = hot::db::Org::list_children(db, &root_id).await {
                                                                        org_ids.extend(children.into_iter().map(|child| child.org_id));
                                                                    }
                                                                }
                                                                org_ids.sort_unstable();
                                                                org_ids.dedup();
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

                                                                    let cutoff = chrono::Utc::now() - chrono::Duration::days(days as i64);
                                                                    match hot::db::blob::deactivate_refs_older_than(&db.with_scope(DatabaseScope::Maintenance { operation: MaintenanceOperation::BlobGc, org_id: None }), Some(*org_id), CALL_REF_KINDS, cutoff).await {
                                                                        Ok(n) if n > 0 => {
                                                                            info!("hot.dev: WORKER {} maintenance: deactivated {} expired call blob ref(s) for org {}", worker_id, n, org_id);
                                                                        }
                                                                        Ok(_) => {}
                                                                        Err(e) => {
                                                                            error!("hot.dev: WORKER {} maintenance: call blob ref deactivation for org {} failed: {}", worker_id, org_id, e);
                                                                        }
                                                                    }
                                                                }
                                                            }
                                                        }
                                                        "blob_cleanup" => {
                                                            // TTL-expired refs (expires_at) across all sources.
                                                            match hot::db::blob::deactivate_expired_refs(db).await {
                                                                Ok(n) if n > 0 => {
                                                                    info!("hot.dev: WORKER {} maintenance: blob_cleanup deactivated {} TTL-expired ref(s)", worker_id, n);
                                                                }
                                                                Ok(_) => {}
                                                                Err(e) => {
                                                                    error!("hot.dev: WORKER {} maintenance: blob_cleanup ref expiry failed: {}", worker_id, e);
                                                                }
                                                            }
                                                            // Physical GC needs the blob store (storage byte deletion).
                                                            // 24h grace after last reference closes the dedupe-vs-GC race.
                                                            if let Some(store) = &blob_store_ref {
                                                                let grace_cutoff = chrono::Utc::now() - chrono::Duration::hours(24);
                                                                match store.gc_objects(grace_cutoff, 1_000).await {
                                                                    Ok(n) if n > 0 => {
                                                                        info!("hot.dev: WORKER {} maintenance: blob_cleanup deleted {} unreferenced blob object(s)", worker_id, n);
                                                                    }
                                                                    Ok(_) => {}
                                                                    Err(e) => {
                                                                        error!("hot.dev: WORKER {} maintenance: blob_cleanup object GC failed: {}", worker_id, e);
                                                                    }
                                                                }
                                                            }
                                                        }
                                                        "sqlite_maintenance" => {
                                                            if let hot::db::DatabasePool::Sqlite(pool) = db.as_ref() {
                                                                match hot::db::sqlite::run_maintenance(
                                                                    pool,
                                                                    &worker_conf_ref,
                                                                )
                                                                .await
                                                                {
                                                                    Ok(stats) => {
                                                                        if stats.wal_checkpointed_pages > 0
                                                                            || stats.incremental_vacuum_pages_freed
                                                                                > 0
                                                                        {
                                                                            info!(
                                                                                "hot.dev: WORKER {} maintenance: sqlite checkpointed {} WAL page(s), incremental_vacuum {} page(s)",
                                                                                worker_id,
                                                                                stats.wal_checkpointed_pages,
                                                                                stats.incremental_vacuum_pages_freed
                                                                            );
                                                                        }
                                                                    }
                                                                    Err(e) => {
                                                                        error!(
                                                                            "hot.dev: WORKER {} maintenance: sqlite_maintenance failed: {}",
                                                                            worker_id, e
                                                                        );
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
                                            warn!("hot.dev: WORKER {} received unknown message type '{}' on event queue", worker_id, msg_type);
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
                                    if let (Some(hot::queue::QueueProcessingError::DeadLettered(message)), Some(db)) =
                                        (e.downcast_ref::<hot::queue::QueueProcessingError>(), db_clone.as_ref())
                                    {
                                        settle_dead_lettered_event_runs(db, message.payload()).await;
                                    }
                                }
                            }
                        }
                        ClaimedWorkerQueue::Alert(lease) => {
                            // Process ONE alert delivery from hot:alert queue (if any)
                            // This allows main workers to help with alert processing when idle
                            if let Some(ref db_ref) = db_clone {
                                match lease
                                    .process(|message| {
                                        let db = db_ref.clone();
                                        let worker_conf = worker_conf_clone.clone();
                                        async move {
                                            process_alert_delivery_message(
                                                worker_id,
                                                db,
                                                worker_conf,
                                                message,
                                            )
                                            .await
                                        }
                                    })
                                    .await
                                {
                                    Ok(Some(_)) => {}
                                    Ok(None) => { /* no alerts in queue */ }
                                    Err(e) => {
                                        tracing::debug!(
                                            "hot.dev: WORKER {} alert queue error: {}",
                                            worker_id,
                                            e
                                        );
                                    }
                                }
                            }
                        }
                        ClaimedWorkerQueue::Email(lease) => {
                            // Process ONE app email from hot:email queue (if any)
                            if let Some(ref db_ref) = db_clone {
                                match lease
                                    .process(|message| {
                                        let db = db_ref.clone();
                                        let worker_conf = worker_conf_clone.clone();
                                        async move {
                                            process_email_message(
                                                worker_id,
                                                db,
                                                worker_conf,
                                                message,
                                            )
                                            .await
                                        }
                                    })
                                    .await
                                {
                                    Ok(Some(_)) => {}
                                    Ok(None) => { /* no emails in queue */ }
                                    Err(e) => {
                                        tracing::debug!(
                                            "hot.dev: WORKER {} email queue error: {}",
                                            worker_id,
                                            e
                                        );
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

    // Phase 1: Stop claimers first so the graceful drain only waits on work
    // that had already started executing before shutdown.
    let _ = shutdown_tx.send(true);

    // Phase 2: Initiate graceful shutdown and wait for active runs to complete
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

    let worker_shutdown_timeout = worker_shutdown_drain_timeout(hot::env::is_local_dev());
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

    if !workers_drained {
        warn!("hot.dev: WORKER workers were aborted; only idle consumers will be unregistered");
    }
    unregister_idle_consumer(
        "hot:event",
        &event_queue
            .clone()
            .with_consumer_name(admin_event_name.clone()),
    )
    .await;
    unregister_idle_consumer(
        "hot:request",
        &request_queue
            .clone()
            .with_consumer_name(admin_request_name.clone()),
    )
    .await;
    unregister_idle_consumer(
        "hot:response",
        &response_queue
            .clone()
            .with_consumer_name(admin_response_name.clone()),
    )
    .await;
    if db.is_some() {
        unregister_idle_consumer(
            "hot:alert",
            &alert_queue
                .clone()
                .with_consumer_name(admin_alert_name.clone()),
        )
        .await;
        unregister_idle_consumer(
            "hot:email",
            &email_queue
                .clone()
                .with_consumer_name(admin_email_name.clone()),
        )
        .await;
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

#[cfg(test)]
mod tests {
    use super::*;
    use hot::val;

    #[test]
    fn dev_context_secrets_are_masked_in_runs() {
        let credential = val!({"registry": "ghcr.io", "username": "bot", "token": "ghp_tok"});
        let mut dev_ctx = AHashMap::new();
        dev_ctx.insert("regpull".to_string(), credential.clone());
        dev_ctx.insert("greeting".to_string(), Val::from("hello"));
        dev_ctx.insert(
            hot::lang::hot::ctx::LOADED_SECRET_KEYS.to_string(),
            Val::from(vec![Val::from("regpull")]),
        );

        let mut storage = AHashMap::new();
        let mut hashes = ahash::AHashSet::new();
        merge_dev_context(&dev_ctx, &mut storage, &mut hashes);

        assert_eq!(storage.get("regpull"), Some(&credential));
        assert_eq!(storage.get("greeting"), Some(&Val::from("hello")));
        assert!(!storage.contains_key(hot::lang::hot::ctx::LOADED_SECRET_KEYS));
        let mut expected = ahash::AHashSet::new();
        hot::lang::hot::ctx::hash_secret_value_recursive(&credential, &mut expected);
        assert_eq!(hashes, expected, "only the secret's values are masked");
        let mut token = ahash::AHashSet::new();
        hot::lang::hot::ctx::hash_secret_value_recursive(&Val::from("ghp_tok"), &mut token);
        assert!(
            token.is_subset(&hashes),
            "fields read from the secret are masked too"
        );
    }

    #[tokio::test]
    async fn run_result_write_timeout_bounds_a_stuck_flush() {
        let started = std::time::Instant::now();
        let result = bounded_run_result_write(
            std::time::Duration::from_millis(20),
            std::future::pending::<Result<(), String>>(),
        )
        .await;

        assert!(result.is_err());
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
        assert_eq!(
            bounded_run_result_write(
                std::time::Duration::from_secs(1),
                std::future::ready(Ok::<_, String>(())),
            )
            .await
            .unwrap(),
            Ok(())
        );
    }

    #[test]
    fn duplicate_function_routing_honors_exact_endpoint_build() {
        let first = Uuid::now_v7();
        let second = Uuid::now_v7();

        assert!(!build_matches_exact_target(first, Some(second)));
        assert!(build_matches_exact_target(second, Some(second)));
        assert!(build_matches_exact_target(first, None));
    }

    #[test]
    fn execution_service_keys_do_not_cross_tenant_scopes() {
        let org_a = Uuid::now_v7();
        let org_b = Uuid::now_v7();
        let env_a = Uuid::now_v7();
        let env_b = Uuid::now_v7();
        let project_a = Uuid::now_v7();
        let project_b = Uuid::now_v7();

        assert_ne!(
            TenantStoreKey {
                org_id: org_a,
                env_id: env_a,
            },
            TenantStoreKey {
                org_id: org_b,
                env_id: env_a,
            }
        );
        assert_ne!(
            TenantStoreKey {
                org_id: org_a,
                env_id: env_a,
            },
            TenantStoreKey {
                org_id: org_a,
                env_id: env_b,
            }
        );
        assert_ne!(
            TenantContextKey {
                org_id: org_a,
                env_id: env_a,
                project_id: project_a,
            },
            TenantContextKey {
                org_id: org_a,
                env_id: env_a,
                project_id: project_b,
            }
        );
    }

    #[test]
    fn tenant_context_cache_expires_and_evicts_decrypted_values() {
        let org_id = Uuid::now_v7();
        let env_id = Uuid::now_v7();
        let oldest_key = TenantContextKey {
            org_id,
            env_id,
            project_id: Uuid::now_v7(),
        };
        let current_key = TenantContextKey {
            org_id,
            env_id,
            project_id: Uuid::now_v7(),
        };
        let mut contexts = AHashMap::new();
        contexts.insert(
            oldest_key,
            CachedTenantContext {
                values: AHashMap::new(),
                loaded_at: std::time::Instant::now() - std::time::Duration::from_secs(2),
            },
        );
        contexts.insert(
            current_key,
            CachedTenantContext {
                values: AHashMap::new(),
                loaded_at: std::time::Instant::now(),
            },
        );

        WorkerExecutionServices::purge_expired_contexts(
            &mut contexts,
            std::time::Duration::from_secs(1),
        );
        assert!(!contexts.contains_key(&oldest_key));
        assert!(contexts.contains_key(&current_key));

        let next_key = TenantContextKey {
            org_id,
            env_id,
            project_id: Uuid::now_v7(),
        };
        WorkerExecutionServices::evict_oldest_context_if_full(&mut contexts, 1);
        contexts.insert(
            next_key,
            CachedTenantContext {
                values: AHashMap::new(),
                loaded_at: std::time::Instant::now(),
            },
        );
        assert_eq!(contexts.len(), 1);
        assert!(contexts.contains_key(&next_key));
    }

    async fn migrated_sqlite_file_db() -> (DatabasePool, PathBuf) {
        let db_path =
            std::env::temp_dir().join(format!("hot-worker-test-{}.sqlite", Uuid::new_v4()));
        let db_uri = format!("sqlite:{}", db_path.display());
        let conf = val!({
            "db": {
                "uri": db_uri,
                "schema": "hot",
            },
        });

        hot::db::run_migrations(&conf)
            .await
            .expect("migrations should run");
        let db = hot::db::create_db_pool(&conf)
            .await
            .expect("db pool should open");

        (db, db_path)
    }

    fn test_message() -> Message {
        Message {
            id: Uuid::now_v7(),
            head: val!({
                "__type": "RequestMessage",
            }),
            body: val!({
                "ok": true,
            }),
        }
    }

    #[tokio::test]
    async fn requeued_orphans_keep_their_handler_runs_for_the_redelivery() {
        use hot::db::run::{Run, RunStatus, RunType};

        let (db, db_path) = migrated_sqlite_file_db().await;
        let data = hot::db::insert_test_data(&db)
            .await
            .expect("test data should insert");
        let DatabasePool::Sqlite(pool) = &db else {
            unreachable!()
        };
        // Two orphaned events in one stream, each with the handler run it was
        // executing, beside another run of the stream.
        let mut handler_runs = Vec::new();
        for _ in 0..2 {
            let event_id = Uuid::now_v7();
            hot::db::Event::insert_event(
                &db,
                &event_id,
                &data.env_id,
                &data.stream_id,
                "demo:event",
                &serde_json::json!({}),
                chrono::Utc::now(),
                &data.user_id,
                None,
            )
            .await
            .unwrap();
            let run_id = Uuid::now_v7();
            Run::insert_run(
                &db,
                &run_id,
                &data.env_id,
                &data.stream_id,
                Some(&data.build_id),
                RunType::Event.as_id(),
                None,
                &data.user_id,
                None,
                None,
            )
            .await
            .unwrap();
            sqlx::query("UPDATE run SET event_id = ? WHERE run_id = ?")
                .bind(event_id)
                .bind(run_id)
                .execute(pool)
                .await
                .unwrap();
            handler_runs.push((event_id, run_id));
        }
        sqlx::query("UPDATE run SET status_id = 1 WHERE run_id = ?")
            .bind(data.run_id)
            .execute(pool)
            .await
            .unwrap();
        let status = async |run_id: Uuid| Run::get_run(&db, &run_id).await.unwrap().status_id;
        let orphan = |event_id: Uuid| OrphanedMessageIds {
            event_id,
            stream_id: Some(data.stream_id),
        };

        settle_orphaned_event_runs(
            &db,
            handler_runs.iter().map(|(event_id, _)| orphan(*event_id)),
            "requeued",
            OrphanedEvents::Requeued,
        )
        .await;
        for (_, run_id) in &handler_runs {
            assert_eq!(status(*run_id).await, RunStatus::PendingRetry.as_id());
        }
        // Nothing redelivers the rest of the stream.
        assert_eq!(status(data.run_id).await, RunStatus::Failed.as_id());

        // A dead-lettered event is never redelivered: its held run fails.
        let (dead_event, dead_run) = handler_runs[0];
        settle_orphaned_event_runs(
            &db,
            [orphan(dead_event)],
            "dead-lettered",
            OrphanedEvents::DeadLettered,
        )
        .await;
        assert_eq!(status(dead_run).await, RunStatus::Failed.as_id());
        assert_eq!(
            status(handler_runs[1].1).await,
            RunStatus::PendingRetry.as_id()
        );

        pool.close().await;
        let _ = std::fs::remove_file(db_path);
    }

    /// The Redis queue hands back the payload of an event it dead-letters:
    /// the event's own runs fail, one left running by a worker that died and
    /// one held for a redelivery that will not come; the stream's other runs,
    /// such as the one that sent the event, go on.
    #[tokio::test]
    async fn dead_lettered_event_fails_only_its_own_runs() {
        use hot::db::run::{Run, RunStatus, RunType};

        let (db, db_path) = migrated_sqlite_file_db().await;
        let data = hot::db::insert_test_data(&db)
            .await
            .expect("test data should insert");
        let DatabasePool::Sqlite(pool) = &db else {
            unreachable!()
        };
        let event_id = Uuid::now_v7();
        hot::db::Event::insert_event(
            &db,
            &event_id,
            &data.env_id,
            &data.stream_id,
            "demo:event",
            &serde_json::json!({}),
            chrono::Utc::now(),
            &data.user_id,
            None,
        )
        .await
        .unwrap();
        // A running run and a held one (pending retry with no retry time).
        let mut event_runs = Vec::new();
        for status_id in [RunStatus::Running.as_id(), RunStatus::PendingRetry.as_id()] {
            let run_id = Uuid::now_v7();
            Run::insert_run(
                &db,
                &run_id,
                &data.env_id,
                &data.stream_id,
                Some(&data.build_id),
                RunType::Event.as_id(),
                None,
                &data.user_id,
                None,
                None,
            )
            .await
            .unwrap();
            sqlx::query(
                "UPDATE run SET event_id = ?, status_id = ?, next_retry_at = NULL WHERE run_id = ?",
            )
            .bind(event_id)
            .bind(status_id)
            .bind(run_id)
            .execute(pool)
            .await
            .unwrap();
            event_runs.push(run_id);
        }
        sqlx::query("UPDATE run SET status_id = 1 WHERE run_id = ?")
            .bind(data.run_id)
            .execute(pool)
            .await
            .unwrap();

        let message = Message {
            id: event_id,
            head: val!({}),
            body: val!({"execution_context": {"stream_id": data.stream_id.to_string()}}),
        };
        let payload = zstd::encode_all(
            std::io::Cursor::new(serde_json::to_vec(&message).unwrap()),
            1,
        )
        .unwrap();

        settle_dead_lettered_event_runs(&db, &payload).await;
        let status = async |run_id: Uuid| Run::get_run(&db, &run_id).await.unwrap().status_id;
        for run_id in event_runs {
            assert_eq!(status(run_id).await, RunStatus::Failed.as_id());
        }
        assert_eq!(status(data.run_id).await, RunStatus::Running.as_id());

        pool.close().await;
        let _ = std::fs::remove_file(db_path);
    }

    #[test]
    fn orphaned_message_decompression_is_bounded() {
        let oversized = vec![b'x'; 8 * 1024 * 1024 + 1];
        let compressed = zstd::encode_all(std::io::Cursor::new(oversized), 1).unwrap();
        assert!(extract_ids_from_orphaned_data(&compressed).is_none());
    }

    #[test]
    fn agent_type_from_meta_uses_namespace_for_short_agent_names() {
        let meta = serde_json::json!({"agent": "TeamAgent"});
        assert_eq!(
            agent_type_from_meta(&meta, "team-agent/agent").as_deref(),
            Some("team-agent/agent/TeamAgent")
        );
    }

    #[test]
    fn agent_type_from_meta_preserves_qualified_agent_names() {
        let meta = serde_json::json!({"agent": "demo/agents/LeadQualifier"});
        assert_eq!(
            agent_type_from_meta(&meta, "demo/agents").as_deref(),
            Some("demo/agents/LeadQualifier")
        );
    }

    #[test]
    fn extract_schedule_id_from_event_data_reads_first_arg() {
        let schedule_id = Uuid::now_v7();
        let event_data = val!({
            "fn": "demo/agents/tick",
            "args": [{
                "type": "hot:schedule",
                "schedule_id": schedule_id.to_string(),
            }],
        });

        assert_eq!(
            extract_schedule_id_from_event_data(&event_data),
            Some(schedule_id)
        );

        // A dynamic schedule's event carries the user's args and the id
        // beside them.
        let dynamic = val!({
            "fn": "demo/steps/check-due",
            "args": [{"schedule_id": "not-this-one"}],
            "schedule_id": schedule_id.to_string(),
        });
        assert_eq!(
            extract_schedule_id_from_event_data(&dynamic),
            Some(schedule_id)
        );
    }

    #[tokio::test]
    async fn scheduled_event_agent_type_uses_schedule_id_meta() {
        let (db, _db_path) = migrated_sqlite_file_db().await;
        let test_data = hot::db::insert_test_data(&db)
            .await
            .expect("test data should insert");
        let schedule_id = Uuid::now_v7();
        let meta = serde_json::json!({"agent": "LeadQualifier"});

        Schedule::insert_dynamic_schedule(
            &db,
            &schedule_id,
            &test_data.build_id,
            &hot::db::ScheduleType::Cron("0 * * * * *".to_string()),
            Some(&test_data.org_id),
            hot::db::SchedulePolicy::self_hosted_default(),
            "demo/agents",
            "tick",
            Some(&meta),
            None,
        )
        .await
        .expect("schedule should insert");

        let event_data = val!({
            "fn": "demo/agents/tick",
            "args": [{
                "type": "hot:schedule",
                "schedule_id": schedule_id.to_string(),
            }],
        });

        assert_eq!(
            extract_agent_type_from_scheduled_event(&db, &test_data.build_id, &event_data).await,
            Some("demo/agents/LeadQualifier".to_string())
        );
    }

    #[tokio::test]
    async fn scheduled_event_agent_type_falls_back_to_target_fn_meta() {
        let (db, _db_path) = migrated_sqlite_file_db().await;
        let test_data = hot::db::insert_test_data(&db)
            .await
            .expect("test data should insert");
        let schedule_id = Uuid::now_v7();
        let meta = serde_json::json!({"agent": "LeadQualifier"});

        Schedule::insert_dynamic_schedule(
            &db,
            &schedule_id,
            &test_data.build_id,
            &hot::db::ScheduleType::Cron("0 * * * * *".to_string()),
            Some(&test_data.org_id),
            hot::db::SchedulePolicy::self_hosted_default(),
            "demo/agents",
            "tick",
            Some(&meta),
            None,
        )
        .await
        .expect("schedule should insert");

        let event_data = val!({
            "fn": "demo/agents/tick",
            "args": [],
        });

        assert_eq!(
            extract_agent_type_from_scheduled_event(&db, &test_data.build_id, &event_data).await,
            Some("demo/agents/LeadQualifier".to_string())
        );
    }

    #[test]
    fn worker_queue_claimer_count_tracks_notification_queues() {
        assert_eq!(worker_queue_claimer_count(false), 2);
        assert_eq!(worker_queue_claimer_count(true), 4);
    }

    #[test]
    fn worker_handoff_capacity_never_drops_to_zero() {
        assert_eq!(worker_handoff_capacity(0), 1);
        assert_eq!(worker_handoff_capacity(4), 4);
    }

    #[test]
    fn blocking_execution_capacity_bounds_detached_work() {
        assert_eq!(blocking_execution_capacity(0, -1), 2);
        assert_eq!(blocking_execution_capacity(4, -1), 8);
        assert_eq!(blocking_execution_capacity(4, 0), 4);
        assert_eq!(blocking_execution_capacity(4, 2), 4);
        assert_eq!(blocking_execution_capacity(4, 6), 6);
    }

    #[test]
    fn blocking_execution_capacity_clamps_to_semaphore_limit() {
        // Semaphore::new panics above MAX_PERMITS; oversized config must
        // degrade to the largest legal ceiling instead of crashing startup.
        assert_eq!(
            blocking_execution_capacity(4, i64::MAX),
            Semaphore::MAX_PERMITS
        );
        let _ = Semaphore::new(blocking_execution_capacity(4, i64::MAX));
    }

    #[tokio::test]
    async fn blocking_slot_admission_times_out_instead_of_pinning_executor() {
        let slots = Arc::new(Semaphore::new(1));
        // Simulate a detached timed-out VM thread that still holds its permit.
        let held = Arc::clone(&slots)
            .acquire_owned()
            .await
            .expect("permit should be available");

        let err = acquire_blocking_execution_slot(
            Arc::clone(&slots),
            Uuid::now_v7(),
            std::time::Duration::from_millis(20),
        )
        .await
        .expect_err("admission must time out while all permits are held");
        assert!(
            is_blocking_slot_timeout_error(&err),
            "unexpected admission error: {err}"
        );

        drop(held);
        let permit = acquire_blocking_execution_slot(
            slots,
            Uuid::now_v7(),
            std::time::Duration::from_millis(20),
        )
        .await
        .expect("freed capacity must admit immediately");
        drop(permit);
    }

    #[test]
    fn admission_wait_logs_once_a_minute_per_account() {
        let (a, b) = (Uuid::now_v7(), Uuid::now_v7());
        assert!(wait_log_due(a));
        assert!(!wait_log_due(a));
        assert!(wait_log_due(b));
    }

    #[test]
    fn run_admission_defers_only_before_any_handler_side_effects() {
        assert!(should_defer_run_admission(
            "[run-admission] Account suspended",
            false
        ));
        assert!(!should_defer_run_admission(
            "[run-admission] Account suspended",
            true
        ));
        assert!(!should_defer_run_admission("User handler failed", false));
        // User code can put the tag in its own error text; only the engine's
        // admission step may begin an error with it.
        assert!(!should_defer_run_admission(
            "Failed to call function 'handler': Runtime error: Function '[run-admission] x' not found",
            false
        ));
        let admission = hot::lang::runtime::vm::admission_failure(
            hot::lang::runtime::vm::VmError::runtime("[run-admission] Account suspended".into()),
        );
        assert!(should_defer_run_admission(&admission, false));
        let quota = hot::lang::runtime::vm::admission_failure(
            hot::lang::runtime::vm::VmError::runtime("[run-quota] Monthly limit".into()),
        );
        assert!(!should_defer_run_admission(&quota, false));
    }

    #[test]
    fn slot_admission_timeout_redelivers_only_side_effect_free_events() {
        let timeout_error = format!("{} after 30s", BLOCKING_SLOT_TIMEOUT_ERROR);
        let setup_timeout_error = format!("{} after 30s", EVENT_SETUP_TIMEOUT_ERROR);

        // No handler started a run yet: safe to leave the event un-ACKed.
        assert!(should_redeliver_event_after_handler_error(
            &timeout_error,
            false
        ));
        // An earlier handler already ran: redelivery would duplicate its run.
        assert!(!should_redeliver_event_after_handler_error(
            &timeout_error,
            true
        ));
        // Any other handler failure keeps the existing ACK-and-continue path.
        assert!(!should_redeliver_event_after_handler_error(
            "Event handler panicked: boom",
            false
        ));
        // An engine error that merely starts with the same human-readable
        // words must NOT be classified as side-effect-free: only the unique
        // bracketed token marks the admission timeout.
        assert!(!should_redeliver_event_after_handler_error(
            "Timed out waiting for a blocking execution slot in the engine",
            false
        ));
        assert!(should_redeliver_event_after_handler_error(
            &setup_timeout_error,
            false
        ));
        assert!(!should_redeliver_event_after_handler_error(
            &setup_timeout_error,
            true
        ));
    }

    /// Pins the escalation contract: the setup-timeout and blocking-slot
    /// admission-timeout redeliveries opt INTO cumulative delivery counting
    /// (their infra requeues would otherwise reset the delivery counter and
    /// loop forever on a deterministic failure), while ordinary
    /// infrastructure retries — the shape used by healthy running-task
    /// deferrals — stay opted out.
    #[test]
    fn setup_and_admission_timeout_redeliveries_opt_into_delivery_counting() {
        for timeout_error in [
            format!("{} after 30s", EVENT_SETUP_TIMEOUT_ERROR),
            format!(
                "{} after 120s resolving handler sources",
                EVENT_SETUP_TIMEOUT_ERROR
            ),
            format!("{} after 30s", BLOCKING_SLOT_TIMEOUT_ERROR),
        ] {
            // Sanity: these are exactly the errors routed through the
            // counted-redelivery arm of the event executor.
            assert!(should_redeliver_event_after_handler_error(
                &timeout_error,
                false
            ));

            let err = worker_counted_redelivery_error(timeout_error.clone(), 0);
            let infra = err
                .downcast_ref::<QueueInfrastructureError>()
                .expect("counted redelivery must remain an infrastructure retry");
            assert!(
                hot::queue::streams::infrastructure_requeue_preserves_delivery_count(
                    &infra.to_string()
                ),
                "redelivery for '{timeout_error}' must opt into delivery counting"
            );
        }

        // The default constructor — used by every other infra retry,
        // including running-task deferrals — must NOT carry the counter.
        let err = worker_infrastructure_retry_error("transient database error", 0);
        let infra = err
            .downcast_ref::<QueueInfrastructureError>()
            .expect("plain infrastructure retry");
        assert!(
            !hot::queue::streams::infrastructure_requeue_preserves_delivery_count(
                &infra.to_string()
            ),
            "plain infrastructure retries must keep the reset-on-requeue behavior"
        );
    }

    #[tokio::test]
    async fn event_setup_watchdog_bounds_pending_preparation() {
        let (setup_tx, setup_rx) = tokio::sync::oneshot::channel();
        let (source_tx, source_rx) = tokio::sync::oneshot::channel();
        let execution = async move {
            let _keep_senders_alive = (setup_tx, source_tx);
            std::future::pending::<Result<(), String>>().await
        };

        // Neither checkpoint fires: the source-resolution window bounds the
        // whole preparation.
        let err = await_event_handler_setup(
            execution,
            setup_rx,
            source_rx,
            std::time::Duration::from_millis(20),
            std::time::Duration::from_secs(3600),
        )
        .await
        .expect_err("pending setup must hit its wall-clock ceiling");
        assert!(is_event_setup_timeout_error(&err));
        assert!(
            err.contains("resolving handler sources"),
            "a pre-checkpoint hang must be attributed to source resolution: {err}"
        );
    }

    #[tokio::test]
    async fn event_setup_watchdog_rearms_after_source_resolution() {
        let (setup_tx, setup_rx) = tokio::sync::oneshot::channel();
        let (source_tx, source_rx) = tokio::sync::oneshot::channel();
        let execution = async move {
            let _keep_sender_alive = setup_tx;
            source_tx.send(()).unwrap();
            std::future::pending::<Result<(), String>>().await
        };

        // The checkpoint fires immediately, so the post-retrieval window is
        // the binding one even though the retrieval window is enormous.
        let err = await_event_handler_setup(
            execution,
            setup_rx,
            source_rx,
            std::time::Duration::from_secs(3600),
            std::time::Duration::from_millis(20),
        )
        .await
        .expect_err("post-retrieval setup must hit its own wall-clock ceiling");
        assert!(is_event_setup_timeout_error(&err));
        assert!(
            !err.contains("resolving handler sources"),
            "a post-checkpoint hang must not be attributed to source resolution: {err}"
        );
    }

    #[tokio::test]
    async fn event_setup_watchdog_gives_source_retrieval_its_own_budget() {
        let (setup_tx, setup_rx) = tokio::sync::oneshot::channel();
        let (source_tx, source_rx) = tokio::sync::oneshot::channel();
        let execution = async move {
            // Simulate a storage retrieval that legitimately outlives the
            // post-retrieval setup window.
            tokio::time::sleep(std::time::Duration::from_millis(60)).await;
            source_tx.send(()).unwrap();
            setup_tx.send(()).unwrap();
            Ok(())
        };

        await_event_handler_setup(
            execution,
            setup_rx,
            source_rx,
            std::time::Duration::from_millis(500),
            std::time::Duration::from_millis(20),
        )
        .await
        .expect("a slow retrieval within its own budget must not trip the setup watchdog");
    }

    #[tokio::test]
    async fn event_setup_watchdog_disarms_for_vm_execution() {
        let (setup_tx, setup_rx) = tokio::sync::oneshot::channel();
        let (source_tx, source_rx) = tokio::sync::oneshot::channel();
        let execution = async move {
            source_tx.send(()).unwrap();
            setup_tx.send(()).unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(40)).await;
            Ok(())
        };

        await_event_handler_setup(
            execution,
            setup_rx,
            source_rx,
            std::time::Duration::from_millis(10),
            std::time::Duration::from_millis(10),
        )
        .await
        .expect("the setup ceiling must be disabled once VM execution starts");
    }

    #[test]
    fn event_orphan_reclaim_floor_covers_slot_admission_wait() {
        let run_timeout = std::time::Duration::from_secs(DEFAULT_RUN_TIMEOUT_SECONDS);
        let legal_lifetime_ms = run_timeout
            .saturating_add(EVENT_SOURCE_RETRIEVAL_TIMEOUT)
            .saturating_add(EVENT_SETUP_TIMEOUT)
            .saturating_add(RUN_TIMEOUT_HARD_GRACE)
            .as_millis() as u64;

        // The floor must exceed the longest an un-ACKed SINGLE-handler event
        // can legally be alive on a healthy worker (source retrieval +
        // post-retrieval setup incl. admission wait + run budget + hard
        // grace), or reclaim would start a duplicate live execution.
        // Multi-handler events and runs with a longer `timeout` of their own
        // can legally exceed the floor; the PEL-touch keepalive keeps those
        // visibly alive, with this floor as its crash backstop and the claim
        // fence to stop them if the claim lapses — see
        // `min_event_orphan_idle_floor_ms` docs.
        assert!(min_event_orphan_idle_floor_ms(run_timeout) > legal_lifetime_ms);
        assert!(min_event_orphan_idle_floor_ms(std::time::Duration::MAX) > 0);
    }

    #[test]
    fn max_run_timeout_defaults_and_stays_between_the_default_and_the_ceiling() {
        let secs = std::time::Duration::from_secs;
        assert_eq!(get_max_run_timeout(&val!({})), secs(3_600));
        assert_eq!(
            get_max_run_timeout(&val!({"worker": {"max-run-timeout": 1800}})),
            secs(1_800)
        );
        assert_eq!(
            get_max_run_timeout(&val!({"worker": {"max-run-timeout": 0}})),
            secs(3_600)
        );
        // A function may always ask for the default.
        assert_eq!(
            get_max_run_timeout(&val!({"worker": {"run-timeout": 600, "max-run-timeout": 60}})),
            secs(600)
        );
        // A run ends before the maintenance reaper's 25-hour cutoff.
        assert_eq!(
            get_max_run_timeout(&val!({"worker": {"max-run-timeout": 200_000}})),
            secs(86_400)
        );
        assert_eq!(
            get_max_run_timeout(&val!({"worker": {"run-timeout": 100_000}})),
            secs(86_400)
        );
    }

    #[test]
    fn run_budget_takes_the_function_timeout_up_to_the_cap() {
        let secs = std::time::Duration::from_secs;
        let (default, cap) = (secs(300), secs(3_600));

        let none = RunBudget::new(None, default, cap);
        assert_eq!(none.timeout, default);
        assert_eq!(none.timeout_error(), "Run timed out after 300 seconds");
        assert_eq!(RunBudget::new(Some(2), default, cap).timeout, secs(2));
        assert_eq!(
            RunBudget::new(Some(1_800), default, cap).timeout,
            secs(1_800)
        );
        assert_eq!(RunBudget::new(Some(3_600), default, cap).capped_from, None);

        let capped = RunBudget::new(Some(7_200), default, cap);
        assert_eq!(capped.timeout, cap);
        assert_eq!(capped.capped_from, Some(7_200));
        assert_eq!(
            capped.timeout_error(),
            "Run timed out after 3600 seconds (its timeout of 7200 seconds is capped by worker.max-run-timeout)"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn run_stops_at_the_function_budget_then_grants_the_grace() {
        let secs = std::time::Duration::from_secs;
        let default = RunBudget::new(None, secs(300), secs(3_600));
        let started = tokio::time::Instant::now();
        let (budget_tx, budget_rx) = tokio::sync::oneshot::channel();
        let stopped = Arc::new(OnceLock::new());
        let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop = tokio::spawn(run_must_stop(
            started,
            default,
            budget_rx,
            None,
            Arc::clone(&stopped),
            Some(Arc::clone(&cancel)),
        ));
        let budget = RunBudget::new(Some(1_800), secs(300), secs(3_600));
        budget_tx.send(budget).unwrap();

        // Past the default, short of the function's own budget.
        tokio::time::sleep(secs(1_799)).await;
        assert!(!cancel.load(std::sync::atomic::Ordering::Relaxed));
        tokio::time::sleep(secs(2)).await;
        assert!(
            cancel.load(std::sync::atomic::Ordering::Relaxed),
            "the cooperative cancel fires at the function's own budget"
        );
        assert_eq!(stopped.get(), Some(&RunStop::TimedOut(budget)));
        assert!(!stop.is_finished(), "the hard grace follows the cancel");

        assert_eq!(stop.await.unwrap(), RunStop::TimedOut(budget));
        assert_eq!(started.elapsed(), secs(1_800) + RUN_TIMEOUT_HARD_GRACE);
    }

    #[tokio::test(start_paused = true)]
    async fn run_budget_keeps_the_default_until_a_budget_arrives() {
        let secs = std::time::Duration::from_secs;
        let default = RunBudget::new(None, secs(300), secs(3_600));
        let started = tokio::time::Instant::now();
        // A program still loading at the default deadline gets the default.
        let (_budget_tx, budget_rx) = tokio::sync::oneshot::channel();
        assert_eq!(
            run_budget_reached(started, default, budget_rx).await,
            default
        );
        assert_eq!(started.elapsed(), secs(300));

        // So does a run whose load failed before it could send one.
        let started = tokio::time::Instant::now();
        let (budget_tx, budget_rx) = tokio::sync::oneshot::channel();
        drop(budget_tx);
        assert_eq!(
            run_budget_reached(started, default, budget_rx).await,
            default
        );
        assert_eq!(started.elapsed(), secs(300));
    }

    #[test]
    fn claim_fence_trips_when_the_claim_is_lost_or_long_unconfirmed() {
        use hot::queue::streams::PendingTouch;
        let secs = std::time::Duration::from_secs;
        let idle_ms = min_event_orphan_idle_floor_ms(secs(DEFAULT_RUN_TIMEOUT_SECONDS));
        let fence_after = event_claim_fence_after(idle_ms);
        // A tripped fence leaves time to unwind and record the failure
        // before another worker could reclaim the event.
        assert!(
            std::time::Duration::from_millis(idle_ms) - fence_after
                > RUN_TIMEOUT_HARD_GRACE + RUN_RESULT_FLUSH_TIMEOUT
        );

        let reclaim_after = std::time::Duration::from_millis(idle_ms);
        let start = tokio::time::Instant::now();
        let (mut fence, claim) = ClaimFence::new(fence_after, reclaim_after, start);
        // Failed touches short of the fence leave the run alone, and a
        // confirmed touch restarts the clock.
        assert_eq!(fence.observe(None, start + fence_after - secs(1)), None);
        assert_eq!(
            fence.observe(Some(PendingTouch::Touched), start + fence_after),
            None
        );
        assert_eq!(fence.observe(None, start + fence_after * 2 - secs(1)), None);
        assert_eq!(*claim.borrow(), None);
        // Failing for the whole fence window trips it, once.
        let unconfirmed = ClaimLoss::Unconfirmed {
            unconfirmed: fence_after,
            reclaimable_at: start + fence_after + reclaim_after,
        };
        assert_eq!(
            fence.observe(None, start + fence_after * 2),
            Some(unconfirmed)
        );
        assert_eq!(fence.observe(None, start + fence_after * 2 + secs(5)), None);
        assert_eq!(*claim.borrow(), Some(unconfirmed));
        // A confirmation clears an unconfirmed claim, and the next outage is
        // measured from it.
        let confirmed_at = start + fence_after * 2 + secs(10);
        assert_eq!(
            fence.observe(Some(PendingTouch::Touched), confirmed_at),
            None
        );
        assert_eq!(*claim.borrow(), None);
        assert_eq!(
            fence.observe(None, confirmed_at + fence_after),
            Some(ClaimLoss::Unconfirmed {
                unconfirmed: fence_after,
                reclaimable_at: confirmed_at + reclaim_after,
            })
        );
        // A reclaimed delivery is lost, for good.
        let later = confirmed_at + fence_after + secs(1);
        assert_eq!(
            fence.observe(Some(PendingTouch::Reclaimed), later),
            Some(ClaimLoss::Lost)
        );
        assert_eq!(fence.observe(Some(PendingTouch::Touched), later), None);
        assert_eq!(fence.observe(Some(PendingTouch::Trimmed), later), None);
        assert_eq!(*claim.borrow(), Some(ClaimLoss::Lost));

        // A touch that finds the delivery reclaimed trips it at once.
        let (mut fence, claim) = ClaimFence::new(fence_after, reclaim_after, start);
        assert_eq!(
            fence.observe(Some(PendingTouch::Reclaimed), start),
            Some(ClaimLoss::Lost)
        );
        assert_eq!(*claim.borrow(), Some(ClaimLoss::Lost));

        // One that finds it trimmed or acknowledged does not: nobody else
        // can be handed the event, so its runs go on. That also settles an
        // unconfirmed claim.
        for settled in [PendingTouch::Trimmed, PendingTouch::Acked] {
            let (mut fence, claim) = ClaimFence::new(fence_after, reclaim_after, start);
            assert!(fence.observe(None, start + fence_after).is_some());
            assert_eq!(fence.observe(Some(settled), start + fence_after), None);
            assert_eq!(*claim.borrow(), None, "{settled:?}");
        }
    }

    /// The fence's clock starts when Redis handed the delivery over, not at
    /// executor pickup: a delivery that waited in the prefetch buffer is
    /// that much closer to being reclaimable.
    #[test]
    fn claim_fence_clock_starts_at_delivery() {
        let secs = std::time::Duration::from_secs;
        let delivered_at = tokio::time::Instant::now();
        let (mut fence, _claim) = ClaimFence::new(secs(300), secs(450), delivered_at);
        // The first touch, at pickup 300 s later, fails.
        assert_eq!(
            fence.observe(None, delivered_at + secs(300)),
            Some(ClaimLoss::Unconfirmed {
                unconfirmed: secs(300),
                reclaimable_at: delivered_at + secs(450),
            })
        );
    }

    /// Before each handler, a delivery checks its claim: none starts once
    /// another worker has the event, or once an unconfirmed claim's stop time
    /// has passed; one still in time starts.
    #[test]
    fn handlers_do_not_start_after_the_claim_is_gone() {
        let secs = std::time::Duration::from_secs;
        let now = tokio::time::Instant::now();
        assert_eq!(claim_stops_new_runs(None, now), None);
        let (tx, claim) = tokio::sync::watch::channel(None);
        assert_eq!(claim_stops_new_runs(Some(&claim), now), None);
        tx.send_replace(Some(ClaimLoss::Lost));
        assert_eq!(
            claim_stops_new_runs(Some(&claim), now),
            Some(ClaimLoss::Lost)
        );
        let unwind = RUN_TIMEOUT_HARD_GRACE + RUN_RESULT_FLUSH_TIMEOUT;
        let in_time = ClaimLoss::Unconfirmed {
            unconfirmed: secs(300),
            reclaimable_at: now + unwind + secs(1),
        };
        tx.send_replace(Some(in_time));
        assert_eq!(claim_stops_new_runs(Some(&claim), now), None);
        assert_eq!(
            claim_stops_new_runs(Some(&claim), now + secs(1)),
            Some(in_time)
        );
    }

    /// Regression for #157's invariant: a claim confirmed again after a
    /// Redis outage stops nothing, even past the outage's reclaim time, and
    /// a later outage stops the run only at its own stop time.
    #[tokio::test(start_paused = true)]
    async fn reconfirmed_claim_does_not_stop_runs() {
        use hot::queue::streams::PendingTouch;
        let secs = std::time::Duration::from_secs;
        let started = tokio::time::Instant::now();
        let budget = RunBudget::new(Some(3_600), secs(300), secs(7_200));
        let (budget_tx, budget_rx) = tokio::sync::oneshot::channel();
        budget_tx.send(budget).unwrap();
        let (mut fence, claim) = ClaimFence::new(secs(300), secs(450), started);
        let stop = tokio::spawn(run_must_stop(
            started,
            budget,
            budget_rx,
            Some(claim),
            Arc::new(OnceLock::new()),
            None,
        ));

        // Touches fail for 300 s: the run would stop at 450 s minus unwind.
        tokio::time::sleep(secs(300)).await;
        assert!(fence.observe(None, tokio::time::Instant::now()).is_some());
        // Redis is back at 333 s.
        tokio::time::sleep(secs(33)).await;
        assert_eq!(
            fence.observe(Some(PendingTouch::Touched), tokio::time::Instant::now()),
            None
        );
        tokio::time::sleep(secs(600)).await;
        assert!(!stop.is_finished(), "a confirmed claim stops nothing");

        // The next outage stops the run 450 s after the last confirmation.
        let confirmed = tokio::time::Instant::now();
        fence.observe(Some(PendingTouch::Touched), confirmed);
        tokio::time::sleep(secs(300)).await;
        assert!(fence.observe(None, tokio::time::Instant::now()).is_some());
        let RunStop::ClaimLost(ClaimLoss::Unconfirmed { reclaimable_at, .. }) = stop.await.unwrap()
        else {
            panic!("expected the second outage to stop the run");
        };
        assert_eq!(reclaimable_at, confirmed + secs(450));
        assert_eq!(
            tokio::time::Instant::now(),
            confirmed + secs(450) - RUN_TIMEOUT_HARD_GRACE - RUN_RESULT_FLUSH_TIMEOUT
        );
    }

    #[tokio::test(start_paused = true)]
    async fn run_stops_when_its_claim_is_lost_before_its_budget() {
        let secs = std::time::Duration::from_secs;
        let budget = RunBudget::new(Some(1_800), secs(300), secs(3_600));
        let started = tokio::time::Instant::now();
        let (budget_tx, budget_rx) = tokio::sync::oneshot::channel();
        budget_tx.send(budget).unwrap();
        let (mut fence, claim) = ClaimFence::new(secs(300), secs(450), started);
        let stopped = Arc::new(OnceLock::new());
        let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop = tokio::spawn(run_must_stop(
            started,
            budget,
            budget_rx,
            Some(claim),
            Arc::clone(&stopped),
            Some(Arc::clone(&cancel)),
        ));

        tokio::time::sleep(secs(600)).await;
        assert!(!cancel.load(std::sync::atomic::Ordering::Relaxed));
        fence.observe(
            Some(hot::queue::streams::PendingTouch::Reclaimed),
            tokio::time::Instant::now(),
        );
        tokio::task::yield_now().await;
        assert!(
            cancel.load(std::sync::atomic::Ordering::Relaxed),
            "a lost claim cancels the run long before its budget"
        );
        assert_eq!(stop.await.unwrap(), RunStop::ClaimLost(ClaimLoss::Lost));
        assert_eq!(started.elapsed(), secs(600) + RUN_TIMEOUT_HARD_GRACE);
        assert_eq!(stopped.get(), Some(&RunStop::ClaimLost(ClaimLoss::Lost)));
        assert_eq!(
            RunStop::ClaimLost(ClaimLoss::Lost).error(),
            "Run stopped: another worker took over its event"
        );
        assert_eq!(
            RunStop::ClaimLost(ClaimLoss::Unconfirmed {
                unconfirmed: secs(307),
                reclaimable_at: started,
            })
            .error(),
            "Run stopped: its worker could not refresh its claim on the event for 307 seconds"
        );
    }

    /// An unconfirmed claim stops a run only once it could still be running
    /// when another worker reclaims the event; a run whose budget ends first
    /// stops at its budget instead. With an hour's `run-timeout` the fence
    /// trips at about 42 minutes, well inside a default run's budget.
    #[tokio::test(start_paused = true)]
    async fn unconfirmed_claim_stops_only_runs_that_could_outlast_it() {
        let secs = std::time::Duration::from_secs;
        let run_timeout = secs(3_600);
        let idle_ms = min_event_orphan_idle_floor_ms(run_timeout);
        let reclaim_after = std::time::Duration::from_millis(idle_ms);
        let fence_after = event_claim_fence_after(idle_ms);
        assert!(fence_after < run_timeout);
        let unwind = RUN_TIMEOUT_HARD_GRACE + RUN_RESULT_FLUSH_TIMEOUT;
        let must_stop = |budget: RunBudget| {
            let started = tokio::time::Instant::now();
            let (budget_tx, budget_rx) = tokio::sync::oneshot::channel();
            budget_tx.send(budget).unwrap();
            let (mut fence, claim) = ClaimFence::new(fence_after, reclaim_after, started);
            let trip = tokio::spawn(async move {
                tokio::time::sleep(fence_after).await;
                fence.observe(None, tokio::time::Instant::now())
            });
            let stop = run_must_stop(
                started,
                budget,
                budget_rx,
                Some(claim),
                Arc::new(OnceLock::new()),
                None,
            );
            (started, trip, stop)
        };

        // A default-budget run ends before anyone could reclaim its event.
        let default = RunBudget::new(None, run_timeout, secs(86_400));
        let (started, trip, stop) = must_stop(default);
        assert_eq!(stop.await, RunStop::TimedOut(default));
        assert_eq!(started.elapsed(), run_timeout);
        assert!(trip.await.unwrap().is_some(), "the fence tripped first");

        // A longer run is stopped with just enough time left to unwind.
        let long = RunBudget::new(Some(7_200), run_timeout, secs(86_400));
        let (started, trip, stop) = must_stop(long);
        let RunStop::ClaimLost(ClaimLoss::Unconfirmed { reclaimable_at, .. }) = stop.await else {
            panic!("expected an unconfirmed claim to stop the run");
        };
        assert_eq!(reclaimable_at, started + reclaim_after);
        assert_eq!(started.elapsed(), reclaim_after - unwind);
        assert!(trip.await.unwrap().is_some());
    }

    /// `run:start` reaches the run's stream and environment once, after the
    /// run's row is flushed and ahead of what the VM publishes next.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn run_start_publisher_publishes_once_after_the_row_is_flushed() {
        use hot::lang::emitter::EngineEvent;
        use hot::stream::{EnvSubscriberFactory, StreamNext, StreamSubscriberFactory};

        #[derive(Default)]
        struct Record(std::sync::Mutex<Vec<String>>);
        impl EngineEventEmitter for Record {
            fn emit(&self, event: EngineEvent) {
                self.0.lock().unwrap().push(event.event_type);
            }
            fn flush_run(
                &self,
                _run_id: Uuid,
            ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + '_>>
            {
                self.0.lock().unwrap().push("flush".to_string());
                Box::pin(async { Ok(()) })
            }
        }

        let record = Arc::new(Record::default());
        let pubsub = Arc::new(StreamPubSub::new(StreamPubSubType::Memory, None, false).unwrap());
        let (run_id, env_id, stream_id) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
        let mut stream_sub = pubsub.subscribe_in_env(env_id, stream_id).await.unwrap();
        let mut env_sub = pubsub.subscribe_env(env_id).await.unwrap();
        let emitter = Arc::new(RunStartPublisher {
            inner: record.clone(),
            publisher: Arc::clone(&pubsub),
            run_id,
            env_id,
            stream_id,
            event_id: None,
            project_id: None,
            fn_name: "::app/f".to_string(),
            run_type: run_type_name("hot:call"),
            published: std::sync::atomic::AtomicBool::new(false),
        });
        let context = move |run_id| {
            ExecutionContext::new(
                run_id,
                stream_id,
                hot::db::run::RunType::Call.as_id(),
                Some(env_id),
                None,
                None,
                None,
            )
        };

        let vm_pubsub = Arc::clone(&pubsub);
        tokio::task::spawn_blocking(move || {
            emitter.emit(EngineEvent::run_start(&context(run_id)));
            // A repeated start, or another run's, publishes nothing more.
            emitter.emit(EngineEvent::run_start(&context(run_id)));
            emitter.emit(EngineEvent::run_start(&context(Uuid::now_v7())));
            tokio::runtime::Handle::current().block_on(async {
                vm_pubsub
                    .publish(StreamEvent::StreamData {
                        stream_data_id: Uuid::now_v7(),
                        run_id,
                        env_id: Some(env_id),
                        stream_id,
                        data_type: "chunk".to_string(),
                        payload: serde_json::json!(1),
                    })
                    .await
                    .unwrap();
            });
        })
        .await
        .unwrap();

        assert_eq!(
            record.0.lock().unwrap()[..2],
            ["run:start".to_string(), "flush".to_string()]
        );
        let first = stream_sub.next().await;
        assert!(
            matches!(first, StreamNext::Event(StreamEvent::RunStart { run_id: got, .. }) if got == run_id),
            "{first:?}"
        );
        let second = stream_sub.next().await;
        assert!(
            matches!(second, StreamNext::Event(StreamEvent::StreamData { .. })),
            "{second:?}"
        );
        let env_event = env_sub.next().await;
        assert!(
            matches!(
                &env_event,
                Some(EnvEvent::RunStart { run_id: got, run_type, .. }) if *got == run_id && run_type == "call"
            ),
            "{env_event:?}"
        );
        let more =
            tokio::time::timeout(std::time::Duration::from_millis(50), stream_sub.next()).await;
        assert!(more.is_err(), "{more:?}");
    }

    #[test]
    fn stopped_run_emitter_records_the_worker_reason_for_its_run() {
        #[derive(Default)]
        struct Collect(std::sync::Mutex<Vec<hot::lang::emitter::EngineEvent>>);
        impl EngineEventEmitter for Collect {
            fn emit(&self, event: hot::lang::emitter::EngineEvent) {
                self.0.lock().unwrap().push(event);
            }
        }
        let collected = Arc::new(Collect::default());
        let run_id = Uuid::now_v7();
        let stopped = Arc::new(OnceLock::new());
        let emitter = StoppedRunEmitter {
            inner: collected.clone(),
            run_id,
            stopped: Arc::clone(&stopped),
        };
        let context = |run_id| {
            ExecutionContext::new(
                run_id,
                Uuid::now_v7(),
                hot::db::run::RunType::Event.as_id(),
                None,
                None,
                None,
                None,
            )
        };
        let cancelled = hot::lang::emitter::EngineEvent::run_fail(
            &context(run_id),
            Val::from("Execution cancelled"),
        );

        let original = cancelled.event_data.get_str("msg");

        // Before the worker stops the run, its failures pass through.
        emitter.emit(cancelled.clone());
        let secs = std::time::Duration::from_secs;
        stopped
            .set(RunStop::TimedOut(RunBudget::new(
                Some(2),
                secs(300),
                secs(3_600),
            )))
            .unwrap();
        emitter.emit(cancelled.clone());
        // Other runs' events, and other events, pass through untouched.
        emitter.emit(hot::lang::emitter::EngineEvent::run_fail(
            &context(Uuid::now_v7()),
            Val::from("Execution cancelled"),
        ));
        emitter.emit(hot::lang::emitter::EngineEvent::run_start(&context(run_id)));

        let messages: Vec<_> = collected
            .0
            .lock()
            .unwrap()
            .iter()
            .map(|event| {
                let msg = event
                    .event_data
                    .get("msg")
                    .map(|_| event.event_data.get_str("msg"));
                (event.event_type.clone(), msg.unwrap_or_default())
            })
            .collect();
        assert_eq!(
            messages,
            [
                ("run:fail".to_string(), original.clone()),
                (
                    "run:fail".to_string(),
                    "Run timed out after 2 seconds".to_string()
                ),
                ("run:fail".to_string(), original.clone()),
                ("run:start".to_string(), String::new()),
            ]
        );

        // A run whose event another worker took over records no end here.
        let collected = Arc::new(Collect::default());
        let lost = StoppedRunEmitter {
            inner: collected.clone(),
            run_id,
            stopped: Arc::new(OnceLock::from(RunStop::ClaimLost(ClaimLoss::Lost))),
        };
        lost.emit(cancelled.clone());
        lost.emit(hot::lang::emitter::EngineEvent::run_stop(
            &context(run_id),
            Val::Null,
        ));
        lost.emit(hot::lang::emitter::EngineEvent::run_start(&context(run_id)));
        let types: Vec<_> = collected
            .0
            .lock()
            .unwrap()
            .iter()
            .map(|event| event.event_type.clone())
            .collect();
        assert_eq!(types, ["run:start"]);
    }

    #[test]
    fn event_keepalive_cadence_gives_multiple_touches_per_reclaim_window() {
        // Exactly one third of the effective reclaim idle once above the floor.
        assert_eq!(event_keepalive_interval_ms(3 * 340_000), 340_000);
        // Tiny (test-sized) configurations are floored so the touch loop
        // cannot become a Redis-hammering busy loop.
        assert_eq!(
            event_keepalive_interval_ms(0),
            EVENT_KEEPALIVE_MIN_INTERVAL_MS
        );
        assert_eq!(
            event_keepalive_interval_ms(EVENT_KEEPALIVE_MIN_INTERVAL_MS),
            EVENT_KEEPALIVE_MIN_INTERVAL_MS
        );
        // At the default effective orphan idle (>= the floor), a delivery
        // gets at least two touch opportunities per reclaim window, so one
        // missed/failed touch cannot orphan it.
        let floor = min_event_orphan_idle_floor_ms(std::time::Duration::from_secs(
            DEFAULT_RUN_TIMEOUT_SECONDS,
        ));
        assert!(event_keepalive_interval_ms(floor) * 3 <= floor);
    }

    #[tokio::test]
    async fn event_keepalive_guard_aborts_touch_task_on_drop() {
        struct SignalOnDrop(Option<tokio::sync::oneshot::Sender<()>>);
        impl Drop for SignalOnDrop {
            fn drop(&mut self) {
                if let Some(tx) = self.0.take() {
                    let _ = tx.send(());
                }
            }
        }

        let (dropped_tx, dropped_rx) = tokio::sync::oneshot::channel::<()>();
        // Construct the drop signal OUTSIDE the async block so it is owned by
        // the future itself: whether the abort lands before or after the
        // task's first poll, dropping the future runs the destructor.
        let signal = SignalOnDrop(Some(dropped_tx));
        let guard = EventKeepaliveGuard {
            touch_loop: tokio::spawn(async move {
                let _signal = signal;
                std::future::pending::<()>().await;
            }),
            claim: tokio::sync::watch::channel(None).1,
        };
        drop(guard);

        tokio::time::timeout(std::time::Duration::from_secs(5), dropped_rx)
            .await
            .expect("dropping the guard must abort the keepalive task")
            .expect("the aborted task must run its destructors");
    }

    #[tokio::test]
    async fn memory_event_lease_spawns_no_keepalive() {
        let queue = ProcessingQueue::<Message>::new(
            QueueType::Memory,
            "hot:event-keepalive-test".to_string(),
            None,
            Serialization::Json,
        )
        .expect("memory queue should build");
        queue
            .enqueue(Message {
                id: Uuid::now_v7(),
                head: Val::map_empty(),
                body: Val::map_empty(),
            })
            .await
            .expect("enqueue should succeed");
        let lease = queue
            .claim_blocking()
            .await
            .expect("claim should succeed")
            .expect("memory claim always yields a lease");

        assert!(
            spawn_event_lease_keepalive(&lease, 60_000, 0).is_none(),
            "memory backend has no PEL / orphan reclaim: keepalive must be a no-op"
        );

        // Drain the lease so the memory queue does not re-enqueue on drop.
        lease
            .process(|_message| async move { Ok::<(), WorkerError>(()) })
            .await
            .expect("processing the drained lease should succeed");
    }

    #[test]
    fn run_registration_guard_unregisters_run_on_drop() {
        let coordinator = Arc::new(ShutdownCoordinator::new(1));
        let run_id = Uuid::now_v7();
        coordinator.register_run(run_id);
        coordinator
            .register_cancel_token(run_id, Arc::new(std::sync::atomic::AtomicBool::new(false)));

        let guard = RunRegistration::new(Arc::clone(&coordinator), run_id);
        assert_eq!(coordinator.active_run_count(), 1);
        drop(guard);
        assert_eq!(coordinator.active_run_count(), 0);

        // unregister_run is idempotent, so explicit cleanup racing the guard
        // (e.g. fail_active_runs during shutdown) stays harmless.
        coordinator.unregister_run(&run_id);
        assert_eq!(coordinator.active_run_count(), 0);
    }

    #[tokio::test]
    async fn error_exit_from_event_handler_unregisters_active_run() {
        let (db, db_path) = migrated_sqlite_file_db().await;
        let test_data = hot::db::insert_test_data(&db)
            .await
            .expect("test data should insert");
        let build = Build::get_build(&db, &test_data.build_id)
            .await
            .expect("test build should exist");

        // An event env that differs from the build's project env forces the
        // early error exit in execute_single_event_handler, before any VM
        // execution or run side effects.
        let mismatched_env_id = Uuid::now_v7();
        let event_id = Uuid::now_v7();
        let stream_id = Uuid::now_v7();
        let event_message = EventMessage {
            id: event_id,
            head: AHashMap::new(),
            body: hot::lang::event::queue::EventMessageBody {
                event: hot::lang::event::Event {
                    event_id,
                    env_id: mismatched_env_id,
                    stream_id,
                    event_type: "hot:call".to_string(),
                    event_data: val!({
                        "fn": "demo/handler",
                        "args": [],
                    }),
                    event_time: chrono::Utc::now(),
                    target_project_id: None,
                    target_project_name: None,
                },
                execution_context: ExecutionContext::new(
                    Uuid::now_v7(),
                    stream_id,
                    hot::db::run::RunType::Call.as_id(),
                    Some(mismatched_env_id),
                    Some(test_data.user_id),
                    Some(test_data.org_id),
                    None,
                ),
            },
        };
        let event_handler = EventHandler {
            event_handler_id: Uuid::now_v7(),
            build_id: build.build_id,
            event_type: "hot:call".to_string(),
            ns: "demo".to_string(),
            var: "handler".to_string(),
            meta: None,
            value: None,
            file: None,
            line: None,
            column: None,
            position: None,
        };

        let worker_conf = get_resolved_conf(val!({}));
        let cache_dir =
            std::env::temp_dir().join(format!("hot-worker-test-cache-{}", Uuid::new_v4()));
        let cache = Arc::new(hot::lang::cache::bytecode_cache::BytecodeCache::new(
            cache_dir.clone(),
        ));
        let task_queue = Arc::new(
            ProcessingQueue::<TaskRequest>::new(
                QueueType::Memory,
                format!("worker-run-unregister-test-{}", Uuid::new_v4()),
                None,
                Serialization::Json,
            )
            .expect("task queue should build"),
        );
        let execution_services =
            Arc::new(WorkerExecutionServices::new(Arc::new(db.clone()), &worker_conf).await);

        // Mirror the event executor: the caller registers the run, then hands
        // unregistration responsibility to execute_single_event_handler.
        let shutdown_coordinator = Arc::new(ShutdownCoordinator::new(1));
        let run_id = Uuid::now_v7();
        shutdown_coordinator.register_run(run_id);
        assert_eq!(shutdown_coordinator.active_run_count(), 1);

        let result = execute_single_event_handler(
            &db,
            &build,
            &mismatched_env_id,
            &worker_conf,
            &event_handler,
            &event_message,
            None,
            None,
            None,
            cache,
            Arc::clone(&shutdown_coordinator),
            run_id,
            Arc::new(BuildPathCache::new()),
            None,
            None,
            task_queue,
            execution_services,
            Arc::new(Semaphore::new(1)),
            None,
        )
        .await;

        assert!(result.is_err(), "env mismatch must fail the handler");
        assert_eq!(
            shutdown_coordinator.active_run_count(),
            0,
            "an error exit must unregister the run so shutdown does not hang"
        );

        if let DatabasePool::Sqlite(pool) = &db {
            pool.close().await;
        }
        let _ = std::fs::remove_file(db_path);
        let _ = std::fs::remove_dir_all(cache_dir);
    }

    /// A hot:call to a function no build has runs a deployed call handler,
    /// the target project's when it has one, so the call fails visibly.
    #[tokio::test]
    async fn missing_function_calls_run_a_deployed_call_handler() {
        let (db, db_path) = migrated_sqlite_file_db().await;
        let test_data = hot::db::insert_test_data(&db)
            .await
            .expect("test data should insert");
        let other_project_id = Uuid::now_v7();
        hot::db::Project::insert_project(
            &db,
            &other_project_id,
            &test_data.env_id,
            "other-project",
            &test_data.user_id,
        )
        .await
        .unwrap();
        let other_build_id = Uuid::now_v7();
        Build::insert_build(
            &db,
            &other_build_id,
            &other_project_id,
            "other-hash",
            0,
            Build::BUILD_TYPE_LIVE,
            &test_data.user_id,
        )
        .await
        .unwrap();
        Build::activate_build_directly(&db, &other_build_id, &test_data.user_id)
            .await
            .unwrap();
        let handler = |build_id| EventHandler {
            event_handler_id: Uuid::now_v7(),
            build_id,
            event_type: "hot:call".to_string(),
            ns: "::hot::lang".to_string(),
            var: "call-event-handler".to_string(),
            meta: None,
            value: None,
            file: None,
            line: None,
            column: None,
            position: None,
        };
        let handlers = [handler(test_data.build_id), handler(other_build_id)];
        let picked = |target| {
            let db = &db;
            let handlers = &handlers;
            async move {
                handler_for_missing_function(db, handlers, target)
                    .await
                    .unwrap()
                    .map(|h| h.build_id)
            }
        };

        // Only deployed, runnable builds qualify.
        assert_eq!(
            picked(Some(test_data.project_id)).await,
            Some(other_build_id)
        );

        Build::activate_build_directly(&db, &test_data.build_id, &test_data.user_id)
            .await
            .unwrap();
        assert_eq!(
            picked(Some(test_data.project_id)).await,
            Some(test_data.build_id)
        );
        assert_eq!(picked(Some(other_project_id)).await, Some(other_build_id));
        assert_eq!(picked(None).await, Some(test_data.build_id));
        let none: [EventHandler; 0] = [];
        assert!(
            handler_for_missing_function(&db, &none, None)
                .await
                .unwrap()
                .is_none()
        );

        if let DatabasePool::Sqlite(pool) = &db {
            pool.close().await;
        }
        let _ = std::fs::remove_file(db_path);
    }

    #[tokio::test]
    async fn missing_deployed_bundle_never_falls_back_to_host_sources() {
        let (db, db_path) = migrated_sqlite_file_db().await;
        let test_data = hot::db::insert_test_data(&db)
            .await
            .expect("test data should insert");
        let bundle_id = Uuid::now_v7();
        Build::insert_build(
            &db,
            &bundle_id,
            &test_data.project_id,
            "missing-bundle",
            0,
            Build::BUILD_TYPE_BUNDLE,
            &test_data.user_id,
        )
        .await
        .unwrap();
        let build = Build::get_build(&db, &bundle_id)
            .await
            .expect("test build should exist");

        // A matching project name on the host is never a fallback for a deployed bundle.
        let event_env_id = test_data.env_id;
        let event_id = Uuid::now_v7();
        let stream_id = Uuid::now_v7();
        let event_message = EventMessage {
            id: event_id,
            head: AHashMap::new(),
            body: hot::lang::event::queue::EventMessageBody {
                event: hot::lang::event::Event {
                    event_id,
                    env_id: event_env_id,
                    stream_id,
                    event_type: "hot:call".to_string(),
                    event_data: val!({
                        "fn": "demo/handler",
                        "args": [],
                    }),
                    event_time: chrono::Utc::now(),
                    target_project_id: None,
                    target_project_name: None,
                },
                execution_context: ExecutionContext::new(
                    Uuid::now_v7(),
                    stream_id,
                    hot::db::run::RunType::Call.as_id(),
                    Some(event_env_id),
                    Some(test_data.user_id),
                    Some(test_data.org_id),
                    None,
                ),
            },
        };
        let event_handler = EventHandler {
            event_handler_id: Uuid::now_v7(),
            build_id: build.build_id,
            event_type: "hot:call".to_string(),
            ns: "demo".to_string(),
            var: "handler".to_string(),
            meta: None,
            value: None,
            file: None,
            line: None,
            column: None,
            position: None,
        };

        let worker_conf = get_resolved_conf(val!({
            "project": {"test-project": {"src": {"paths": ["hot/src"]}}}
        }));
        let cache_dir =
            std::env::temp_dir().join(format!("hot-worker-test-cache-{}", Uuid::new_v4()));
        let cache = Arc::new(hot::lang::cache::bytecode_cache::BytecodeCache::new(
            cache_dir.clone(),
        ));
        let task_queue = Arc::new(
            ProcessingQueue::<TaskRequest>::new(
                QueueType::Memory,
                format!("worker-run-unregister-test-{}", Uuid::new_v4()),
                None,
                Serialization::Json,
            )
            .expect("task queue should build"),
        );
        let execution_services =
            Arc::new(WorkerExecutionServices::new(Arc::new(db.clone()), &worker_conf).await);

        // Mirror the event executor: the caller registers the run, then hands
        // unregistration responsibility to execute_single_event_handler.
        let shutdown_coordinator = Arc::new(ShutdownCoordinator::new(1));
        let run_id = Uuid::now_v7();
        shutdown_coordinator.register_run(run_id);
        assert_eq!(shutdown_coordinator.active_run_count(), 1);

        let result = execute_single_event_handler(
            &db,
            &build,
            &event_env_id,
            &worker_conf,
            &event_handler,
            &event_message,
            None,
            None,
            None,
            cache,
            Arc::clone(&shutdown_coordinator),
            run_id,
            Arc::new(BuildPathCache::new()),
            None,
            None,
            task_queue,
            execution_services,
            Arc::new(Semaphore::new(1)),
            None,
        )
        .await;

        let error = result.unwrap_err();
        assert!(
            error.contains("Failed to retrieve build"),
            "missing bundle must stop source resolution before host source compilation"
        );
        // Nothing ran: the event is delivered again rather than acknowledged.
        assert!(run_never_started(&error).is_some(), "{error}");
        assert_eq!(
            shutdown_coordinator.active_run_count(),
            0,
            "an error exit must unregister the run so shutdown does not hang"
        );

        if let DatabasePool::Sqlite(pool) = &db {
            pool.close().await;
        }
        let _ = std::fs::remove_file(db_path);
        let _ = std::fs::remove_dir_all(cache_dir);
    }

    /// Answer `GET /<ms>` after that many milliseconds, so a handler can wait
    /// on a real call for as long as a test needs.
    async fn spawn_slow_http_server() -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut request = Vec::new();
                    let mut buffer = [0u8; 1024];
                    while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                        match socket.read(&mut buffer).await {
                            Ok(0) | Err(_) => return,
                            Ok(read) => request.extend_from_slice(&buffer[..read]),
                        }
                    }
                    let delay_ms = String::from_utf8_lossy(&request)
                        .split_whitespace()
                        .nth(1)
                        .and_then(|path| path.trim_start_matches('/').parse().ok())
                        .unwrap_or(0);
                    tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                    let _ = socket
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                        )
                        .await;
                });
            }
        });
        format!("http://{address}")
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn function_timeout_meta_sets_the_run_budget_up_to_the_cap() {
        let (db, db_path) = migrated_sqlite_file_db().await;
        let test_data = hot::db::insert_test_data(&db).await.unwrap();
        let slow_server = spawn_slow_http_server().await;

        // A deployed bundle of handlers that wait on the slow server.
        let workspace = tempfile::tempdir().unwrap();
        let src_dir = workspace.path().join("src");
        std::fs::create_dir_all(&src_dir).unwrap();
        std::fs::write(
            src_dir.join("timeout_test.hot"),
            r#"::timeout-test ns

::http ::hot::http

quick meta {on-event: "timeout-test:quick", timeout: 2}
fn (event) {
    ::http/get(`${event.data.url}/${event.data.wait}`).status
}

patient meta {on-event: "timeout-test:patient", timeout: 10}
fn (event) {
    ::http/get(`${event.data.url}/${event.data.wait}`).status
}

greedy meta {on-event: "timeout-test:greedy", timeout: 60}
fn (event) {
    ::http/get(`${event.data.url}/${event.data.wait}`).status
}

called meta {timeout: 10}
fn (url: Str, wait: Int): Int {
    ::http/get(`${url}/${wait}`).status
}
"#,
        )
        .unwrap();
        // Like a deploy, the bundle leaves hot-std to the runtime.
        let bundle = hot::bundle::create_bundle(
            "timeout-test",
            &[src_dir.to_string_lossy().to_string()],
            &[],
            &[],
            true,
            &[],
        )
        .unwrap();
        let zip_path =
            hot::bundle::write_bundle_zip(&bundle, &workspace.path().join("build")).unwrap();
        let build_id = Uuid::now_v7();
        Build::insert_build(
            &db,
            &build_id,
            &test_data.project_id,
            "timeout-test",
            0,
            Build::BUILD_TYPE_BUNDLE,
            &test_data.user_id,
        )
        .await
        .unwrap();
        let build = Build::get_build(&db, &build_id).await.unwrap();
        let prepared = extract_and_precompile_event_bundle(
            std::fs::read(&zip_path).unwrap(),
            workspace.path().join("extracted"),
            build_id,
            "timeout-test".to_string(),
            true,
        )
        .unwrap();
        let build_path_cache = Arc::new(BuildPathCache::new());
        build_path_cache.insert(build_id, prepared.extract_dir);

        let execution_services =
            Arc::new(WorkerExecutionServices::new(Arc::new(db.clone()), &val!({})).await);
        let cache = Arc::new(hot::lang::cache::bytecode_cache::BytecodeCache::new(
            workspace.path().join("cache"),
        ));
        let task_queue = Arc::new(
            ProcessingQueue::<TaskRequest>::new(
                QueueType::Memory,
                format!("worker-timeout-meta-test-{}", Uuid::new_v4()),
                None,
                Serialization::Json,
            )
            .unwrap(),
        );
        // Run one event through the worker, timing it from the moment the
        // handler is handed over.
        let run = |event_type: &str,
                   handler: (&str, &str),
                   event_data: Val,
                   worker_conf: Val,
                   claim: Option<watch::Receiver<Option<ClaimLoss>>>| {
            let event_id = Uuid::now_v7();
            let stream_id = Uuid::now_v7();
            let event_message = EventMessage {
                id: event_id,
                head: AHashMap::new(),
                body: hot::lang::event::queue::EventMessageBody {
                    event: hot::lang::event::Event {
                        event_id,
                        env_id: test_data.env_id,
                        stream_id,
                        event_type: event_type.to_string(),
                        event_data,
                        event_time: chrono::Utc::now(),
                        target_project_id: None,
                        target_project_name: None,
                    },
                    execution_context: ExecutionContext::new(
                        Uuid::now_v7(),
                        stream_id,
                        hot::db::run::RunType::Event.as_id(),
                        Some(test_data.env_id),
                        Some(test_data.user_id),
                        Some(test_data.org_id),
                        None,
                    ),
                },
            };
            let event_handler = EventHandler {
                event_handler_id: Uuid::now_v7(),
                build_id,
                event_type: event_type.to_string(),
                ns: handler.0.to_string(),
                var: handler.1.to_string(),
                meta: None,
                value: None,
                file: None,
                line: None,
                column: None,
                position: None,
            };
            let (db, build, cache, build_path_cache, task_queue, execution_services) = (
                db.clone(),
                build.clone(),
                Arc::clone(&cache),
                Arc::clone(&build_path_cache),
                Arc::clone(&task_queue),
                Arc::clone(&execution_services),
            );
            let env_id = test_data.env_id;
            async move {
                let shutdown_coordinator = Arc::new(ShutdownCoordinator::new(1));
                let run_id = Uuid::now_v7();
                shutdown_coordinator.register_run(run_id);
                let started = std::time::Instant::now();
                let result = execute_single_event_handler(
                    &db,
                    &build,
                    &env_id,
                    &worker_conf,
                    &event_handler,
                    &event_message,
                    None,
                    None,
                    None,
                    cache,
                    shutdown_coordinator,
                    run_id,
                    build_path_cache,
                    None,
                    None,
                    task_queue,
                    execution_services,
                    Arc::new(Semaphore::new(4)),
                    claim,
                )
                .await;
                (result, started.elapsed())
            }
        };
        let secs = std::time::Duration::from_secs_f64;
        // A detached VM keeps waiting on its call after a hard timeout, so
        // the hard timeout lands exactly at the budget without the grace.
        let no_cancel = |run_timeout: i64, max_run_timeout: i64| {
            val!({"worker": {
                "run-timeout": run_timeout,
                "max-run-timeout": max_run_timeout,
                "cancel-on-timeout": false,
            }})
        };

        // `timeout: 2` ends the run at 2 seconds, well short of the default.
        let (result, elapsed) = run(
            "timeout-test:quick",
            ("::timeout-test", "quick"),
            val!({"url": slow_server.clone(), "wait": 30_000}),
            no_cancel(30, 3_600),
            None,
        )
        .await;
        assert_eq!(result.unwrap_err(), "Run timed out after 2 seconds");
        assert!(elapsed >= secs(2.0) && elapsed < secs(10.0), "{elapsed:?}");

        // `timeout: 10` outlives a 2-second default.
        let (result, elapsed) = run(
            "timeout-test:patient",
            ("::timeout-test", "patient"),
            val!({"url": slow_server.clone(), "wait": 3_500}),
            no_cancel(2, 3_600),
            None,
        )
        .await;
        result.unwrap();
        assert!(elapsed >= secs(3.5), "{elapsed:?}");

        // `timeout: 60` above a 3-second cap ends at the cap, and says so.
        let (result, elapsed) = run(
            "timeout-test:greedy",
            ("::timeout-test", "greedy"),
            val!({"url": slow_server.clone(), "wait": 30_000}),
            no_cancel(2, 3),
            None,
        )
        .await;
        assert_eq!(
            result.unwrap_err(),
            "Run timed out after 3 seconds (its timeout of 60 seconds is capped by worker.max-run-timeout)"
        );
        assert!(elapsed >= secs(3.0) && elapsed < secs(10.0), "{elapsed:?}");

        // A call takes the called function's timeout, not the generic
        // call handler's default.
        let (result, elapsed) = run(
            "hot:call",
            ("::hot::lang", "call-event-handler"),
            val!({"fn": "::timeout-test/called", "args": [slow_server.clone(), 3_500]}),
            no_cancel(2, 3_600),
            None,
        )
        .await;
        result.unwrap();
        assert!(elapsed >= secs(3.5), "{elapsed:?}");

        // A run whose worker loses its claim on the event stops at once,
        // long before its own timeout.
        let (mut fence, claim) = ClaimFence::new(
            std::time::Duration::from_secs(300),
            std::time::Duration::from_secs(450),
            tokio::time::Instant::now(),
        );
        let lose_claim = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            fence.observe(
                Some(hot::queue::streams::PendingTouch::Reclaimed),
                tokio::time::Instant::now(),
            )
        });
        let (result, elapsed) = run(
            "timeout-test:patient",
            ("::timeout-test", "patient"),
            val!({"url": slow_server.clone(), "wait": 30_000}),
            no_cancel(2, 3_600),
            Some(claim),
        )
        .await;
        assert_eq!(
            result.unwrap_err(),
            "Run stopped: another worker took over its event"
        );
        assert!(elapsed >= secs(1.0) && elapsed < secs(10.0), "{elapsed:?}");
        assert_eq!(lose_claim.await.unwrap(), Some(ClaimLoss::Lost));

        if let DatabasePool::Sqlite(pool) = &db {
            pool.close().await;
        }
        let _ = std::fs::remove_file(db_path);
    }

    #[tokio::test]
    async fn payload_retry_origin_cannot_cross_environment_scope() {
        let db = hot::db::test_db().await;
        let fixture = hot::db::insert_test_data(&db).await.unwrap();
        let retry = hot::env::retry::RetryContext::from_event_data(
            &val!({"retry": {"origin-run-id": fixture.run_id.to_string(), "attempt": 1}}),
            &val!({}),
        )
        .unwrap();
        validate_event_origin_scope(&db, fixture.env_id, retry.origin_run_id)
            .await
            .unwrap();
        let other_env = Uuid::now_v7();
        Env::insert_env(&db, &other_env, &fixture.org_id, "other", &fixture.user_id)
            .await
            .unwrap();
        assert!(
            validate_event_origin_scope(&db, other_env, retry.origin_run_id)
                .await
                .unwrap_err()
                .contains("another environment")
        );
        assert!(
            validate_event_origin_scope(&db, fixture.env_id, Some(Uuid::now_v7()))
                .await
                .is_err()
        );
        validate_event_origin_scope(&db, other_env, None)
            .await
            .unwrap();
    }

    #[test]
    fn event_bundles_never_inherit_host_resources() {
        use hot::lang::hot::resource::{self, ThreadRegistryGuard};
        let host = tempfile::tempdir().unwrap();
        std::fs::write(host.path().join("host-secret.txt"), "host-only").unwrap();
        let _host = ThreadRegistryGuard::install(Arc::new(resource::build_registry(
            &[host.path().to_path_buf()],
            false,
            &[],
        )));
        assert!(
            resource::get_registry()
                .entries
                .contains_key("host-secret.txt")
        );
        let bundle = tempfile::tempdir().unwrap();
        {
            let _bundle = install_event_resource_scope(true, Some(bundle.path()));
            assert!(resource::get_registry().entries.is_empty());
        }
        assert!(
            resource::get_registry()
                .entries
                .contains_key("host-secret.txt")
        );
        std::fs::create_dir_all(bundle.path().join("resources")).unwrap();
        std::fs::write(bundle.path().join("resources/bundle.txt"), "bundle-only").unwrap();
        std::fs::write(
            bundle.path().join("manifest.hot"),
            r#"{"hot.bundle.test":{"resources":[{"path":"bundle.txt","hash":"","size":11}]}}"#,
        )
        .unwrap();
        let _bundle = install_event_resource_scope(true, Some(bundle.path()));
        let registry = resource::get_registry();
        assert!(registry.entries.contains_key("bundle.txt"));
        assert!(!registry.entries.contains_key("host-secret.txt"));
    }

    #[tokio::test]
    async fn event_bundle_initializers_use_bundle_resources_on_every_cache_path() {
        use hot::lang::cache::bytecode_cache::BytecodeCache;
        use hot::lang::hot::resource::{self, ThreadRegistryGuard};

        fn invoke_initializer(
            extract_dir: &std::path::Path,
            manifest: &hot::bundle::BundleManifest,
            host_registry: Arc<resource::ResourceRegistry>,
            expected: &str,
        ) {
            let _host = ThreadRegistryGuard::install(host_registry);
            let _bundle = install_event_resource_scope(true, Some(extract_dir));
            let cache = BytecodeCache::new(extract_dir.join(".hot/cache"));
            let cached = cache.load(manifest.cache_key.as_deref().unwrap()).unwrap();
            let result = hot::lang::engine::Engine::call_function_with_cached_bytecode(
                "::bundle_scope/handler",
                &[],
                cached,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
            )
            .unwrap();
            assert_eq!(result.unwrap_ok().unwrap_or(&result), &Val::from(expected));
        }

        let directory = tempfile::tempdir().unwrap();
        let host = directory.path().join("host");
        std::fs::create_dir_all(&host).unwrap();
        std::fs::write(host.join("scope.txt"), "host-only-secret").unwrap();
        let host_registry = Arc::new(resource::build_registry(&[host], false, &[]));
        let source = directory.path().join("src");
        let resources = directory.path().join("resources");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::create_dir_all(&resources).unwrap();
        std::fs::write(resources.join("scope.txt"), "bundle-only").unwrap();
        std::fs::write(
            source.join("scope.hot"),
            r#"::bundle_scope ns
initialized ::hot::resource/load-str("scope.txt") |> if-err("missing")
handler fn () { initialized }
"#,
        )
        .unwrap();

        for include_resources in [true, false] {
            let resource_roots = if include_resources {
                vec![resources.clone()]
            } else {
                Vec::new()
            };
            let bundle = hot::bundle::create_bundle(
                "scope-project",
                &[source.to_string_lossy().into_owned()],
                &[],
                &resource_roots,
                false,
                &[],
            )
            .unwrap();
            let zip = hot::bundle::write_bundle_zip(&bundle, directory.path()).unwrap();
            let extract_dir = directory.path().join(format!("build-{include_resources}"));
            let prepared = {
                let _host = ThreadRegistryGuard::install(host_registry.clone());
                extract_and_precompile_event_bundle(
                    std::fs::read(zip).unwrap(),
                    extract_dir.clone(),
                    Uuid::now_v7(),
                    "scope-project".into(),
                    true,
                )
                .unwrap()
            };
            let manifest = prepared.manifest.unwrap();
            let expected = if include_resources {
                "bundle-only"
            } else {
                "missing"
            };
            invoke_initializer(&extract_dir, &manifest, host_registry.clone(), expected);

            // Older bundles can have no resources manifest. Recompilation and
            // warmup must preserve an explicit empty scope for those too.
            if !include_resources {
                std::fs::remove_file(extract_dir.join("manifest.hot")).unwrap();
            }
            {
                let _host = ThreadRegistryGuard::install(host_registry.clone());
                assert!(recompile_bundle_cache(&extract_dir, &manifest));
            }
            invoke_initializer(&extract_dir, &manifest, host_registry.clone(), expected);
            compile_bundle_cache(extract_dir.clone(), manifest.clone())
                .await
                .unwrap();
            invoke_initializer(&extract_dir, &manifest, host_registry.clone(), expected);
        }
    }

    #[test]
    fn local_dev_shutdown_still_gets_a_drain_window() {
        assert_eq!(
            worker_shutdown_drain_timeout(true),
            tokio::time::Duration::from_secs(LOCAL_DEV_WORKER_SHUTDOWN_DRAIN_SECONDS)
        );
        assert!(worker_shutdown_drain_timeout(true) > tokio::time::Duration::ZERO);
        assert_eq!(
            worker_shutdown_drain_timeout(false),
            tokio::time::Duration::from_secs(WORKER_SHUTDOWN_DRAIN_SECONDS)
        );
    }

    #[test]
    fn worker_semantics_config_accepts_current_serial_contract() {
        let conf = val!({
            "worker": {
                "event-ordering": "current",
                "handler-concurrency": "serial",
            },
        });

        assert!(validate_worker_semantics_conf(&conf).is_ok());
    }

    #[test]
    fn worker_semantics_config_rejects_unimplemented_modes() {
        let ordering_conf = val!({
            "worker": {
                "event-ordering": "strict",
                "handler-concurrency": "serial",
            },
        });
        let ordering_err = validate_worker_semantics_conf(&ordering_conf)
            .unwrap_err()
            .to_string();
        assert!(ordering_err.contains("Unsupported worker.event-ordering"));

        let concurrency_conf = val!({
            "worker": {
                "event-ordering": "current",
                "handler-concurrency": "parallel",
            },
        });
        let concurrency_err = validate_worker_semantics_conf(&concurrency_conf)
            .unwrap_err()
            .to_string();
        assert!(concurrency_err.contains("Unsupported worker.handler-concurrency"));
    }

    #[tokio::test]
    async fn thin_event_message_hydrates_event_data_from_db_before_routing() {
        let (db, db_path) = migrated_sqlite_file_db().await;
        let test_data = hot::db::insert_test_data(&db)
            .await
            .expect("test data should insert");
        let event_id = Uuid::now_v7();
        let stream_id = Uuid::now_v7();
        let target_fn = "::demo::run-all/run-all-demos";
        let event_time = chrono::Utc::now();
        let db_event_data = serde_json::json!({
            "fn": target_fn,
            "args": [],
        });

        hot::db::event::Event::insert_event(
            &db,
            &event_id,
            &test_data.env_id,
            &stream_id,
            "hot:call",
            &db_event_data,
            event_time,
            &test_data.user_id,
            None,
        )
        .await
        .expect("event should insert");

        let thin_message = EventMessage {
            id: event_id,
            head: AHashMap::new(),
            body: hot::lang::event::queue::EventMessageBody {
                event: hot::lang::event::Event {
                    event_id,
                    env_id: test_data.env_id,
                    stream_id,
                    event_type: "hot:call".to_string(),
                    event_data: val!({
                        "event_id": event_id.to_string(),
                    }),
                    event_time,
                    target_project_id: None,
                    target_project_name: None,
                },
                execution_context: ExecutionContext::new(
                    Uuid::now_v7(),
                    stream_id,
                    hot::db::run::RunType::Call.as_id(),
                    Some(test_data.env_id),
                    Some(Uuid::now_v7()),
                    Some(test_data.org_id),
                    None,
                ),
            },
        };

        let mut wrong_org = thin_message.clone();
        wrong_org.body.execution_context.org_id = Some(Uuid::now_v7());
        assert!(
            hydrate_event_message_from_db(0, &db, None, wrong_org, 0)
                .await
                .is_err()
        );
        let DatabasePool::Sqlite(pool) = &db else {
            unreachable!()
        };
        sqlx::query("UPDATE org SET lifecycle_state = 'suspended' WHERE org_id = ?")
            .bind(test_data.org_id)
            .execute(pool)
            .await
            .unwrap();
        let held = hydrate_event_message_from_db(0, &db, None, thin_message.clone(), 0)
            .await
            .unwrap_err();
        assert!(
            held.downcast_ref::<ParkedEvent>().is_some(),
            "Suspension must park work without consuming the queue retry budget"
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM parked_event WHERE message_id=?")
                .bind(thin_message.id)
                .fetch_one(pool)
                .await
                .unwrap(),
            1
        );
        let replay_queue = ProcessingQueue::<Message>::new(
            QueueType::Memory,
            "hot:event-parked-test".to_string(),
            None,
            Serialization::Json,
        )
        .expect("memory queue should build");
        // While the account is suspended, replay leaves its events parked.
        replay_parked_events(&db, &replay_queue)
            .await
            .expect("parked events should replay");
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM parked_event WHERE message_id=?")
                .bind(thin_message.id)
                .fetch_one(pool)
                .await
                .unwrap(),
            1
        );
        sqlx::query("UPDATE org SET lifecycle_state = 'active' WHERE org_id = ?")
            .bind(test_data.org_id)
            .execute(pool)
            .await
            .unwrap();
        // Reactivation republishes the parked event and releases its row.
        replay_parked_events(&db, &replay_queue)
            .await
            .expect("parked events should replay");
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM parked_event WHERE message_id=?")
                .bind(thin_message.id)
                .fetch_one(pool)
                .await
                .unwrap(),
            0
        );

        let hydrated = hydrate_event_message_from_db(0, &db, None, thin_message, 0)
            .await
            .expect("thin event should hydrate");

        assert_eq!(hydrated.body.event.event_data.get_str("fn"), target_fn);
        assert_eq!(
            extract_target_function_from_event(&hydrated.body.event.event_data).as_deref(),
            Some(target_fn)
        );
        assert_eq!(
            hydrated.body.execution_context.user_id,
            Some(test_data.user_id)
        );
        assert_eq!(
            hydrated.body.execution_context.org_id,
            Some(test_data.org_id)
        );
        assert_eq!(hydrated.body.execution_context.build_id, None);

        if let DatabasePool::Sqlite(pool) = &db {
            pool.close().await;
        }
        let _ = std::fs::remove_file(db_path);
    }

    #[tokio::test]
    async fn webhook_url_hints_restore_after_real_queue_roundtrip_and_db_hydration() {
        let (db, db_path) = migrated_sqlite_file_db().await;
        let data = hot::db::insert_test_data(&db).await.unwrap();
        let event_id = Uuid::now_v7();
        let stream_id = Uuid::now_v7();
        let raw_url = "https://hooks.example.test/webhook/svc/path/abcdef123456?x=1";
        let request = val!({"original-url":raw_url,"body":"authoritative body"});
        let original = val!({
            "fn":"::durable/handler",
            "args": Val::Vec(vec![request.clone()]),
            "caller":request
        });
        let stored = hot::webhook_url::redact_event_data_original_urls(&original);
        let stored_json = serde_json::to_value(stored.to_hot_data_repr()).unwrap();
        assert!(!stored_json.to_string().contains("abcdef123456"));
        hot::db::Event::insert_event(
            &db,
            &event_id,
            &data.env_id,
            &stream_id,
            "hot:call",
            &stored_json,
            chrono::Utc::now(),
            &data.user_id,
            None,
        )
        .await
        .unwrap();

        for (hint_url, accepted) in [
            (raw_url, true),
            (
                "https://sibling.example.test/webhook/svc/path/abcdef123456?x=1",
                false,
            ),
        ] {
            let hint_request = val!({"original-url":hint_url,"body":"untrusted body"});
            let envelope = EventMessage {
                id: event_id,
                head: AHashMap::new(),
                body: hot::lang::event::EventMessageBody {
                    event: hot::lang::event::Event {
                        event_id,
                        env_id: data.env_id,
                        stream_id,
                        event_type: "hot:call".into(),
                        event_data: val!({
                            "fn":"::untrusted/handler",
                            "args":Val::Vec(vec![hint_request.clone()]),
                            "caller":hint_request
                        }),
                        event_time: chrono::Utc::now(),
                        target_project_id: None,
                        target_project_name: None,
                    },
                    execution_context: ExecutionContext::new(
                        Uuid::now_v7(),
                        stream_id,
                        hot::db::run::RunType::Call.as_id(),
                        Some(data.env_id),
                        Some(data.user_id),
                        Some(data.org_id),
                        None,
                    ),
                },
            };
            let serialized: hot::data::msg::Message = envelope.into();
            let parsed = EventMessage::try_from(serialized).unwrap();
            let hydrated = hydrate_event_message_from_db(0, &db, None, parsed, 0)
                .await
                .unwrap();
            assert_eq!(
                hydrated.body.event.event_data.get_str("fn"),
                "::durable/handler"
            );
            let caller = hydrated.body.event.event_data.get("caller").unwrap();
            assert_eq!(caller.get_str("body"), "authoritative body");
            if accepted {
                assert_eq!(hydrated.body.event.event_data, original);
            } else {
                assert!(caller.get("original-url").is_none());
                let Val::Vec(args) = hydrated.body.event.event_data.get("args").unwrap() else {
                    panic!("hydrated arguments must remain intact");
                };
                assert!(args[0].get("original-url").is_none());
            }
            assert_eq!(
                hot::db::Event::get_event(&db, &event_id)
                    .await
                    .unwrap()
                    .event_data,
                stored_json
            );
        }
        if let DatabasePool::Sqlite(pool) = &db {
            pool.close().await;
        }
        let _ = std::fs::remove_file(db_path);
    }

    #[tokio::test]
    async fn hydrate_rehydrates_spilled_event_data_blob_refs() {
        use hot::blob::{BlobConfig, BlobScope, BlobStore, SpillSource};

        let (db, db_path) = migrated_sqlite_file_db().await;
        let test_data = hot::db::insert_test_data(&db)
            .await
            .expect("test data should insert");
        let temp_dir = tempfile::TempDir::new().unwrap();
        let storage = Arc::new(hot::file_storage::LocalFileStorage::new(
            temp_dir.path().to_path_buf(),
        ));
        let blob_store = Arc::new(BlobStore::new(
            Arc::new(db.clone()),
            storage,
            BlobConfig {
                mode: hot::blob::BlobMode::Service,
                spill_threshold_bytes: 1024,
                ..BlobConfig::default()
            },
        ));

        let event_id = Uuid::now_v7();
        let stream_id = Uuid::now_v7();
        let event_time = chrono::Utc::now();
        let large_payload: String = "x".repeat(8192);
        let original_event_data = val!({
            "fn": "::demo/big",
            "payload": large_payload.clone(),
        });

        let scope = BlobScope {
            org_id: test_data.org_id,
            env_id: Some(test_data.env_id),
            run_id: None,
        };
        let spilled = blob_store
            .spill_large_val(
                original_event_data.clone(),
                scope,
                SpillSource::EventData,
                Some(&event_id.to_string()),
            )
            .await
            .expect("spill should succeed");
        assert!(hot::blob::contains_blob_ref(&spilled));

        hot::db::event::Event::insert_event(
            &db,
            &event_id,
            &test_data.env_id,
            &stream_id,
            "hot:call",
            &serde_json::to_value(&spilled).unwrap(),
            event_time,
            &test_data.user_id,
            None,
        )
        .await
        .expect("event should insert");

        let thin_message = EventMessage {
            id: event_id,
            head: AHashMap::new(),
            body: hot::lang::event::queue::EventMessageBody {
                event: hot::lang::event::Event {
                    event_id,
                    env_id: test_data.env_id,
                    stream_id,
                    event_type: "hot:call".to_string(),
                    event_data: val!({
                        "event_id": event_id.to_string(),
                    }),
                    event_time,
                    target_project_id: None,
                    target_project_name: None,
                },
                execution_context: ExecutionContext::new(
                    Uuid::now_v7(),
                    stream_id,
                    hot::db::run::RunType::Call.as_id(),
                    Some(test_data.env_id),
                    Some(test_data.user_id),
                    Some(test_data.org_id),
                    None,
                ),
            },
        };

        // Without a blob store, executable payloads with BlobRefs fail closed.
        let no_store = hydrate_event_message_from_db(0, &db, None, thin_message.clone(), 0).await;
        assert!(no_store.is_err());

        let hydrated = hydrate_event_message_from_db(0, &db, Some(&blob_store), thin_message, 0)
            .await
            .expect("spilled event should rehydrate");

        assert_eq!(hydrated.body.event.event_data, original_event_data);
        assert_eq!(
            hydrated.body.event.event_data.get_str("payload"),
            large_payload
        );

        if let DatabasePool::Sqlite(pool) = &db {
            pool.close().await;
        }
        let _ = std::fs::remove_file(db_path);
    }

    #[tokio::test]
    async fn missing_event_handlers_acknowledge_unhandled_event_when_stable() {
        let (db, db_path) = migrated_sqlite_file_db().await;
        let test_data = hot::db::insert_test_data(&db)
            .await
            .expect("test data should insert");
        let event_id = Uuid::now_v7();
        let stream_id = Uuid::now_v7();
        let event_time = chrono::Utc::now();

        hot::db::event::Event::insert_event(
            &db,
            &event_id,
            &test_data.env_id,
            &stream_id,
            "slack:post",
            &serde_json::json!({
                "channel": "#sales",
                "text": "hello",
            }),
            event_time,
            &test_data.user_id,
            None,
        )
        .await
        .expect("event should insert");

        let result = acknowledge_unhandled_event_if_stable(
            0,
            &db,
            &test_data.env_id,
            "slack:post",
            "No event handlers found for event type 'slack:post'".to_string(),
            0,
        )
        .await;

        assert!(result.is_ok());
        let event = hot::db::event::Event::get_event(&db, &event_id)
            .await
            .expect("event should still exist");
        assert!(!event.handled);

        if let DatabasePool::Sqlite(pool) = &db {
            pool.close().await;
        }
        let _ = std::fs::remove_file(db_path);
    }

    #[tokio::test]
    async fn missing_event_handlers_still_defer_during_bundle_deployment() {
        let (db, db_path) = migrated_sqlite_file_db().await;
        let test_data = hot::db::insert_test_data(&db)
            .await
            .expect("test data should insert");
        let bundle_build_id = Uuid::now_v7();

        Build::insert_build(
            &db,
            &bundle_build_id,
            &test_data.project_id,
            "pending-bundle-hash",
            0,
            Build::BUILD_TYPE_BUNDLE,
            &test_data.user_id,
        )
        .await
        .expect("bundle build should insert");
        Build::request_bundle_deployment(&db, &bundle_build_id, &test_data.user_id)
            .await
            .expect("bundle deployment should be requested");

        let err = acknowledge_unhandled_event_if_stable(
            0,
            &db,
            &test_data.env_id,
            "slack:post",
            "No event handlers found for event type 'slack:post'".to_string(),
            250,
        )
        .await
        .expect_err("deployment in progress should defer missing handlers");

        assert!(err.downcast_ref::<QueueInfrastructureError>().is_some());

        if let DatabasePool::Sqlite(pool) = &db {
            pool.close().await;
        }
        let _ = std::fs::remove_file(db_path);
    }

    #[tokio::test]
    async fn memory_claimer_hands_off_one_lease_and_stops_on_shutdown() {
        let queue_name = format!("worker-claimer-test-{}", Uuid::new_v4());
        let queue = ProcessingQueue::<Message>::new(
            QueueType::Memory,
            queue_name,
            None,
            Serialization::Json,
        )
        .unwrap();
        queue.enqueue(test_message()).await.unwrap();

        let (claimed_tx, mut claimed_rx) = mpsc::channel::<ClaimedWorkerQueue>(1);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let handle = spawn_worker_queue_claimer(
            "test:request",
            queue,
            claimed_tx,
            shutdown_rx,
            ClaimedWorkerQueue::Request,
            false,
        );

        let claimed = tokio::time::timeout(tokio::time::Duration::from_secs(1), claimed_rx.recv())
            .await
            .expect("claimer should hand off promptly")
            .expect("claimer should send a lease");
        let ClaimedWorkerQueue::Request(lease) = claimed else {
            panic!("expected request lease");
        };
        let processed = lease
            .process(|message| async move {
                Ok::<Uuid, Box<dyn std::error::Error + Send + Sync>>(message.id)
            })
            .await
            .unwrap();
        assert!(processed.is_some());

        shutdown_tx.send(true).unwrap();
        tokio::time::timeout(tokio::time::Duration::from_secs(1), handle)
            .await
            .expect("claimer should stop on shutdown")
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn memory_claimer_backpressure_leaves_backlog_visible() {
        let queue_name = format!("worker-claimer-backpressure-test-{}", Uuid::new_v4());
        let queue = ProcessingQueue::<Message>::new(
            QueueType::Memory,
            queue_name,
            None,
            Serialization::Json,
        )
        .unwrap();
        for _ in 0..3 {
            queue.enqueue(test_message()).await.unwrap();
        }

        let (claimed_tx, mut claimed_rx) = mpsc::channel::<ClaimedWorkerQueue>(1);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let handle = spawn_worker_queue_claimer(
            "test:request",
            queue.clone(),
            claimed_tx,
            shutdown_rx,
            ClaimedWorkerQueue::Request,
            false,
        );

        // With a channel capacity of 1 and no executor draining, the claimer
        // may hold one extra lease while blocked on send, but it must not drain
        // the entire queue into local memory.
        tokio::time::timeout(tokio::time::Duration::from_secs(1), async {
            loop {
                if queue.len().await.unwrap() == 1 {
                    break;
                }
                tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("backpressure should leave one queued item visible");

        let first = claimed_rx
            .recv()
            .await
            .expect("first lease should be buffered");
        drop(first);
        let second = claimed_rx
            .recv()
            .await
            .expect("second lease should send after space opens");
        drop(second);

        shutdown_tx.send(true).unwrap();
        tokio::time::timeout(tokio::time::Duration::from_secs(1), handle)
            .await
            .expect("claimer should stop on shutdown")
            .unwrap()
            .unwrap();
    }

    fn test_handler(ns: &str, var: &str) -> EventHandler {
        EventHandler {
            event_handler_id: Uuid::now_v7(),
            build_id: Uuid::now_v7(),
            event_type: "demo:event".to_string(),
            ns: ns.to_string(),
            var: var.to_string(),
            meta: None,
            value: None,
            file: None,
            line: None,
            column: None,
            position: None,
        }
    }

    #[test]
    fn handler_run_ids_survive_redeploys() {
        let event_id = Uuid::now_v7();
        let project_id = Uuid::now_v7();
        let run_id = |handler: &EventHandler, project_id: Uuid, retry: i16| {
            event_handler_run_id(event_id, project_id, "demo:event", handler, retry)
        };
        let deployed = test_handler("demo", "on-event");
        // A redeploy gives the handler a new id in a new build.
        let redeployed = test_handler("demo", "on-event");
        assert_eq!(
            run_id(&deployed, project_id, 0),
            run_id(&redeployed, project_id, 0)
        );
        assert_ne!(
            run_id(&deployed, project_id, 0),
            run_id(&deployed, project_id, 1)
        );
        assert_ne!(
            run_id(&deployed, project_id, 0),
            run_id(&deployed, Uuid::now_v7(), 0)
        );
        assert_ne!(
            run_id(&deployed, project_id, 0),
            run_id(&test_handler("demo", "other"), project_id, 0)
        );
    }

    #[test]
    fn errors_from_before_the_run_started_are_recognized() {
        assert_eq!(
            run_never_started(&format!("Parse errors {RUN_NOT_STARTED_TOKEN}")),
            Some("Parse errors")
        );
        assert_eq!(run_never_started("Parse errors"), None);
        let deferred = format!(
            "{} account suspended {RUN_NOT_STARTED_TOKEN}",
            hot::lang::runtime::vm::RUN_ADMISSION_DEFERRED
        );
        assert!(should_defer_run_admission(&deferred, false));
        let timeout = format!("{EVENT_SETUP_TIMEOUT_ERROR} after 30s {RUN_NOT_STARTED_TOKEN}");
        assert!(should_redeliver_event_after_handler_error(&timeout, false));
    }

    /// An event of `data`'s environment with one handler run of `build`.
    async fn event_with_run(
        db: &DatabasePool,
        data: &hot::db::TestData,
    ) -> (EventMessage, Build, Uuid) {
        let event_id = Uuid::now_v7();
        hot::db::Event::insert_event(
            db,
            &event_id,
            &data.env_id,
            &data.stream_id,
            "demo:event",
            &serde_json::json!({}),
            chrono::Utc::now(),
            &data.user_id,
            None,
        )
        .await
        .unwrap();
        let event_message = EventMessage {
            id: event_id,
            head: AHashMap::new(),
            body: hot::lang::event::queue::EventMessageBody {
                event: hot::lang::event::Event {
                    event_id,
                    env_id: data.env_id,
                    stream_id: data.stream_id,
                    event_type: "demo:event".to_string(),
                    event_data: val!({}),
                    event_time: chrono::Utc::now(),
                    target_project_id: None,
                    target_project_name: None,
                },
                execution_context: ExecutionContext::new(
                    Uuid::now_v7(),
                    data.stream_id,
                    hot::db::run::RunType::Event.as_id(),
                    Some(data.env_id),
                    Some(data.user_id),
                    Some(data.org_id),
                    None,
                ),
            },
        };
        let build = Build::get_build(db, &data.build_id).await.unwrap();
        let handler = test_handler("demo", "on-event");
        let run_id = event_handler_run_id(event_id, build.project_id, "demo:event", &handler, 0);
        (event_message, build, run_id)
    }

    #[tokio::test]
    async fn setup_failures_hold_their_run_for_the_redelivery_then_fail_it() {
        use hot::db::run::{EarlierDelivery, Run, RunStatus};

        let (db, db_path) = migrated_sqlite_file_db().await;
        let data = hot::db::insert_test_data(&db).await.unwrap();
        let (event_message, build, run_id) = event_with_run(&db, &data).await;
        let event_id = event_message.body.event.event_id;

        record_unstarted_run(&db, &event_message, &build, run_id, "Parse errors", true).await;
        let run = Run::get_run(&db, &run_id).await.unwrap();
        assert_eq!(run.status_id, RunStatus::PendingRetry.as_id());
        assert_eq!(run.event_id, Some(event_id));
        assert!(run.result.unwrap().to_string().contains("Parse errors"));
        let earlier = Run::earlier_deliveries(&db, &event_id).await.unwrap();
        assert_eq!(earlier.get(&run_id), Some(&EarlierDelivery::Held));

        // The redelivery reopens it; when its setup fails for good, so does the run.
        Run::reopen_held_runs(&db, &[run_id]).await.unwrap();
        assert_eq!(
            Run::get_run(&db, &run_id).await.unwrap().status_id,
            RunStatus::Running.as_id()
        );
        record_unstarted_run(&db, &event_message, &build, run_id, "Still broken", false).await;
        let run = Run::get_run(&db, &run_id).await.unwrap();
        assert_eq!(run.status_id, RunStatus::Failed.as_id());
        assert!(run.stop_time.is_some());

        // A finished run stays as it finished.
        record_unstarted_run(&db, &event_message, &build, run_id, "Late", true).await;
        let run = Run::get_run(&db, &run_id).await.unwrap();
        assert_eq!(run.status_id, RunStatus::Failed.as_id());
        assert!(run.result.unwrap().to_string().contains("Still broken"));

        if let DatabasePool::Sqlite(pool) = &db {
            pool.close().await;
        }
        let _ = std::fs::remove_file(db_path);
    }

    #[tokio::test]
    async fn redeliveries_settle_earlier_runs_they_no_longer_run() {
        use hot::db::run::{Run, RunStatus};

        let (db, db_path) = migrated_sqlite_file_db().await;
        let data = hot::db::insert_test_data(&db).await.unwrap();
        let (event_message, build, running) = event_with_run(&db, &data).await;
        let row = |run_id| event_handler_run_row(&event_message, &build, run_id);
        let held = Uuid::now_v7();
        let finished = Uuid::now_v7();
        for run_id in [running, held, finished] {
            Run::open_event_run(&db, &row(run_id)).await.unwrap();
        }
        Run::hold_orphaned_runs_for_redelivery(&db, &event_message.body.event.event_id)
            .await
            .unwrap();
        Run::reopen_held_runs(&db, &[running]).await.unwrap();
        Run::update_stop_time_and_status(&db, &finished, None, &RunStatus::Succeeded)
            .await
            .unwrap();

        fail_unrun_earlier_deliveries(&db, &[running, held, finished]).await;
        let status = async |run_id| Run::get_run(&db, &run_id).await.unwrap().status_id;
        assert_eq!(status(running).await, RunStatus::Failed.as_id());
        assert_eq!(status(held).await, RunStatus::Failed.as_id());
        assert_eq!(status(finished).await, RunStatus::Succeeded.as_id());

        if let DatabasePool::Sqlite(pool) = &db {
            pool.close().await;
        }
        let _ = std::fs::remove_file(db_path);
    }

    #[tokio::test]
    async fn recovered_runs_written_first_fail_when_their_event_is_dead_lettered() {
        use hot::db::run::{Run, RunStatus};

        let (db, db_path) = migrated_sqlite_file_db().await;
        let data = hot::db::insert_test_data(&db).await.unwrap();
        let (event_message, build, run_id) = event_with_run(&db, &data).await;
        let event_id = event_message.body.event.event_id;

        Run::open_event_run(&db, &event_handler_run_row(&event_message, &build, run_id))
            .await
            .unwrap();
        settle_orphaned_event_runs(
            &db,
            [OrphanedMessageIds {
                event_id,
                stream_id: None,
            }],
            CRASHED_EVENT_DEAD_LETTERED,
            OrphanedEvents::DeadLettered,
        )
        .await;
        let run = Run::get_run(&db, &run_id).await.unwrap();
        assert_eq!(run.status_id, RunStatus::Failed.as_id());
        assert!(
            run.result
                .unwrap()
                .to_string()
                .contains("each time it ran alone")
        );

        if let DatabasePool::Sqlite(pool) = &db {
            pool.close().await;
        }
        let _ = std::fs::remove_file(db_path);
    }

    #[test]
    fn partial_bundle_extractions_are_cleared_only_by_the_lock_holder() {
        let temp = tempfile::tempdir().unwrap();
        let extract_dir = temp.path().join("build");
        assert!(clear_partial_bundle_extraction(&extract_dir, false).is_ok());

        std::fs::create_dir_all(extract_dir.join("hot/src")).unwrap();
        std::fs::write(extract_dir.join("manifest.hot"), "{}").unwrap();
        assert!(clear_partial_bundle_extraction(&extract_dir, false).is_err());
        assert!(extract_dir.exists());
        clear_partial_bundle_extraction(&extract_dir, true).unwrap();
        assert!(!extract_dir.exists());

        std::fs::create_dir_all(&extract_dir).unwrap();
        BuildPathCache::mark_extraction_complete(&extract_dir);
        clear_partial_bundle_extraction(&extract_dir, true).unwrap();
        assert!(BuildPathCache::is_extraction_complete(&extract_dir));
    }

    #[test]
    fn deployment_wait_counts_from_the_deployment_message() {
        let published = chrono::Utc::now() - chrono::Duration::minutes(20);
        let timestamp = uuid::Timestamp::from_unix(
            uuid::NoContext,
            published.timestamp() as u64,
            published.timestamp_subsec_nanos(),
        );
        let waited = deployment_wait(Uuid::new_v7(timestamp), chrono::Utc::now());
        assert!(
            waited >= std::time::Duration::from_secs(19 * 60),
            "{waited:?}"
        );
        assert!(waited > NEWER_RUNTIME_DEPLOY_GRACE);

        let waited = deployment_wait(Uuid::new_v4(), chrono::Utc::now());
        assert!(waited < NEWER_RUNTIME_DEPLOY_GRACE);
    }

    #[tokio::test]
    async fn crash_quarantine_runs_recovered_deliveries_alone() {
        let quarantine = CrashQuarantine::default();
        let short = tokio::time::Duration::from_millis(50);

        let shared = quarantine.enter(false, None, 0).await;
        let also_shared = quarantine.enter(false, None, 1).await;
        // A recovered delivery waits for the others to finish.
        let alone = tokio::spawn({
            let quarantine = quarantine.clone();
            async move {
                let _alone = quarantine.enter(true, None, 2).await;
                tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
            }
        });
        tokio::time::sleep(short).await;
        assert!(!alone.is_finished());
        drop(shared);
        drop(also_shared);
        // While it runs, no other delivery starts.
        tokio::time::sleep(short).await;
        assert!(
            tokio::time::timeout(short / 5, quarantine.enter(false, None, 0))
                .await
                .is_err()
        );
        alone.await.unwrap();
        let _shared = tokio::time::timeout(short, quarantine.enter(false, None, 0))
            .await
            .expect("deliveries run again once the recovered one is done");
    }
}

use crate::data::serialization::Serialization;
use crate::val::{Val, val};
use async_trait::async_trait;
use std::error::Error;
use std::fmt;
use std::future::Future;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

pub mod mem;
pub mod streams;

static QUEUE_METRICS_ENABLED: AtomicBool = AtomicBool::new(true);
static QUEUE_WAIT_TARGET_P99_MS: AtomicU64 = AtomicU64::new(1_000);

pub fn set_metrics_enabled(enabled: bool) {
    QUEUE_METRICS_ENABLED.store(enabled, Ordering::Relaxed);
}

pub fn set_wait_target_p99_ms(target_ms: u64) {
    QUEUE_WAIT_TARGET_P99_MS.store(target_ms, Ordering::Relaxed);
}

pub(crate) fn queue_timing_enabled() -> bool {
    QUEUE_METRICS_ENABLED.load(Ordering::Relaxed)
}

pub(crate) fn queue_wait_target_p99_ms() -> u64 {
    QUEUE_WAIT_TARGET_P99_MS.load(Ordering::Relaxed)
}

#[derive(Debug, PartialEq, Clone, Copy, Default)]
pub enum QueueType {
    #[default]
    Memory, // In-process channel-backed queue (single process only)
    Redis, // Redis-backed queue (multi-process / production)
}

impl fmt::Display for QueueType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            QueueType::Memory => write!(f, "memory"),
            QueueType::Redis => write!(f, "redis"),
        }
    }
}

impl FromStr for QueueType {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "memory" | "mem" => Ok(QueueType::Memory),
            "redis" => Ok(QueueType::Redis),
            _ => Err(format!("Invalid queue type: {}", s)),
        }
    }
}

#[derive(Debug)]
pub enum QueueProcessingError {
    QueueError(Box<dyn Error + Send + Sync>),
    WorkerError(Box<dyn Error + Send + Sync>),
    RetryLimitExceeded,
}

impl std::fmt::Display for QueueProcessingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::QueueError(e) => write!(f, "Queue error: {}", e),
            Self::WorkerError(e) => write!(f, "Worker error: {}", e),
            Self::RetryLimitExceeded => write!(f, "Retry limit exceeded"),
        }
    }
}

impl std::error::Error for QueueProcessingError {}

#[derive(Debug)]
pub struct QueueInfrastructureError {
    message: String,
    backoff: Duration,
}

impl QueueInfrastructureError {
    pub fn new(message: impl Into<String>, backoff: Duration) -> Self {
        Self {
            message: message.into(),
            backoff,
        }
    }

    pub fn backoff(&self) -> Duration {
        self.backoff
    }
}

impl std::fmt::Display for QueueInfrastructureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for QueueInfrastructureError {}

/// Trait for stream-level cleanup operations (consumer pruning, stream trimming).
/// Not parameterised on item type so callers can clean up heterogeneous queues uniformly.
#[async_trait]
pub trait StreamCleanup: Send + Sync {
    /// Remove stale consumers and trim old entries.
    /// Returns (consumers_removed, entries_trimmed).
    async fn cleanup_streams(&self) -> Result<(usize, usize), Box<dyn Error + Send + Sync>>;

    /// Reclaim orphaned messages (delivered to a now-dead consumer but never
    /// ACKed) into this consumer. Returns the number of messages reclaimed.
    /// For Memory queues this is a no-op returning 0.
    async fn reclaim_orphans(&self) -> Result<usize, Box<dyn Error + Send + Sync>>;
}

/// Trait for consumer lifecycle management (graceful unregistration).
#[async_trait]
pub trait ConsumerLifecycle: Send + Sync {
    /// Unregister this consumer from the group on graceful shutdown.
    async fn unregister_consumer(&self) -> Result<(), Box<dyn Error + Send + Sync>>;
}

#[async_trait]
pub trait Queue<T>: Send + Sync {
    async fn enqueue(&self, item: T) -> Result<(), Box<dyn Error + Send + Sync>>;
    async fn dequeue(&self) -> Result<Option<T>, Box<dyn Error + Send + Sync>>;
    async fn len(&self) -> Result<usize, Box<dyn Error + Send + Sync>>;
    async fn is_empty(&self) -> Result<bool, Box<dyn Error + Send + Sync>> {
        Ok(self.len().await? == 0)
    }

    /// Move an item to a dead letter queue after it has failed processing too many times
    async fn move_to_dead_letter_queue(
        &self,
        item: T,
        reason: String,
    ) -> Result<(), Box<dyn Error + Send + Sync>>;
}

/// A trait for safely processing queue items with automatic retry handling.
/// This is separated from the main Queue trait to maintain dyn compatibility.
#[async_trait]
pub trait QueueProcessor<T>: Queue<T> {
    /// Safely processes a queue item with the provided worker function.
    /// If the worker function fails, the item will be re-enqueued.
    ///
    /// # Arguments
    /// * `worker` - A function that takes an item and returns a future that resolves to a result
    ///
    /// # Returns
    /// * `Ok(Some(R))` - If the worker successfully processed an item
    /// * `Ok(None)` - If there was no item to process
    /// * `Err(e)` - If there was an error during processing
    async fn dequeue_and_work<F, Fut, R>(
        &self,
        worker: F,
    ) -> Result<Option<R>, Box<dyn Error + Send + Sync>>
    where
        F: FnOnce(T) -> Fut + Send,
        Fut: Future<Output = Result<R, Box<dyn Error + Send + Sync>>> + Send,
        R: Send + Sync;
}

/// Helper function to create a Queue that uses a specific queue type
///
/// # Arguments
/// * `queue_type` - The type of queue to create (Memory or Redis)
/// * `queue_name` - The name of the queue
/// * `redis_uri` - Optional Redis URL (only used for Redis queue type)
/// * `redis_cluster` - Whether to use Redis cluster mode (default: false)
/// * `serialization` - Serialization format to use
///
/// # Returns
/// * `Arc<dyn Queue<T>>` - A trait object that can be used for basic queue operations
pub fn create_queue<T>(
    queue_type: QueueType,
    queue_name: String,
    redis_uri: Option<String>,
    serialization: Serialization,
) -> Result<Arc<dyn Queue<T> + Send + Sync + 'static>, Box<dyn Error + Send + Sync>>
where
    T: Send + Sync + serde::Serialize + serde::de::DeserializeOwned + 'static,
{
    create_queue_with_cluster(queue_type, queue_name, redis_uri, false, serialization)
}

/// Helper function to create a Queue with optional cluster mode support
///
/// # Arguments
/// * `queue_type` - The type of queue to create (Memory or Redis)
/// * `queue_name` - The name of the queue
/// * `redis_uri` - Optional Redis URL (only used for Redis queue type)
/// * `redis_cluster` - Whether to use Redis cluster mode (default: false)
/// * `serialization` - Serialization format to use
///
/// # Returns
/// * `Arc<dyn Queue<T>>` - A trait object that can be used for basic queue operations
pub fn create_queue_with_cluster<T>(
    queue_type: QueueType,
    queue_name: String,
    redis_uri: Option<String>,
    redis_cluster: bool,
    serialization: Serialization,
) -> Result<Arc<dyn Queue<T> + Send + Sync + 'static>, Box<dyn Error + Send + Sync>>
where
    T: Send + Sync + serde::Serialize + serde::de::DeserializeOwned + 'static,
{
    match queue_type {
        QueueType::Memory => {
            let queue = mem::MemQueue::<T>::new(queue_name)?.with_serialization(serialization);
            Ok(Arc::new(queue))
        }
        QueueType::Redis => {
            let url = redis_uri.unwrap_or_else(|| "redis://127.0.0.1/".to_string());

            // Initialize Rustls crypto provider if using TLS (rediss://)
            if url.starts_with("rediss://") {
                crate::redis::init_crypto_provider();
            }

            // Check if cluster mode is enabled or auto-detect from URI
            let is_cluster = redis_cluster || crate::redis::is_cluster_uri(&url);

            if is_cluster {
                tracing::debug!("Creating Redis Streams cluster queue for: {}", queue_name);
                let client = ::redis::cluster::ClusterClient::new(vec![url.as_str()])?;
                let queue = streams::RedisStreamQueue::<T>::new_cluster(client, queue_name)
                    .with_serialization(serialization);
                Ok(Arc::new(queue))
            } else {
                tracing::debug!(
                    "Creating Redis Streams standalone queue for: {}",
                    queue_name
                );
                let client = ::redis::Client::open(url.as_str())?;
                let queue = streams::RedisStreamQueue::<T>::new(client, queue_name)
                    .with_serialization(serialization);
                Ok(Arc::new(queue))
            }
        }
    }
}

/// Helper function to create a queue implementation with processing capabilities.
/// This returns a concrete type rather than a trait object, since QueueProcessor
/// cannot be turned into a trait object due to its generic methods.
///
/// # Arguments
/// * `queue_type` - The type of queue to create (Memory or Redis)
/// * `queue_name` - The name of the queue
/// * `redis_uri` - Optional Redis URL (only used for Redis queue type)
/// * `serialization` - Serialization format to use
///
/// # Returns
/// * A specific queue implementation that implements QueueProcessor
pub enum ProcessingQueue<T> {
    Memory(mem::MemQueue<T>),
    Redis(Box<streams::RedisStreamQueue<T>>),
}

pub enum ProcessingQueueLease<T>
where
    T: Send + Sync + serde::Serialize + serde::de::DeserializeOwned + Clone + 'static,
{
    Memory(mem::MemQueueLease<T>),
    Redis(Box<streams::RedisQueueLease<T>>),
}

impl<T> ProcessingQueueLease<T>
where
    T: Send + Sync + serde::Serialize + serde::de::DeserializeOwned + Clone + 'static,
{
    pub async fn process<F, Fut, R>(
        self,
        worker: F,
    ) -> Result<Option<R>, Box<dyn Error + Send + Sync>>
    where
        F: FnOnce(T) -> Fut + Send,
        Fut: Future<Output = Result<R, Box<dyn Error + Send + Sync>>> + Send,
        R: Send + Sync,
    {
        match self {
            ProcessingQueueLease::Memory(lease) => lease.process(worker).await,
            ProcessingQueueLease::Redis(lease) => lease.process(worker).await,
        }
    }
}

impl<T: Clone> Clone for ProcessingQueue<T> {
    fn clone(&self) -> Self {
        match self {
            ProcessingQueue::Memory(q) => ProcessingQueue::Memory(q.clone()),
            ProcessingQueue::Redis(q) => ProcessingQueue::Redis(Box::new((**q).clone())),
        }
    }
}

impl<T> ProcessingQueue<T> {
    /// Create a new ProcessingQueue with the specified parameters
    pub fn new(
        queue_type: QueueType,
        queue_name: String,
        redis_uri: Option<String>,
        serialization: Serialization,
    ) -> Result<Self, Box<dyn Error + Send + Sync>>
    where
        T: Clone + Send + Sync + serde::Serialize + serde::de::DeserializeOwned + 'static,
    {
        Self::new_with_cluster(queue_type, queue_name, redis_uri, false, serialization)
    }

    /// Create a new ProcessingQueue with optional cluster mode support
    pub fn new_with_cluster(
        queue_type: QueueType,
        queue_name: String,
        redis_uri: Option<String>,
        redis_cluster: bool,
        serialization: Serialization,
    ) -> Result<Self, Box<dyn Error + Send + Sync>>
    where
        T: Clone + Send + Sync + serde::Serialize + serde::de::DeserializeOwned + 'static,
    {
        match queue_type {
            QueueType::Memory => {
                let queue = mem::MemQueue::<T>::new(queue_name)?.with_serialization(serialization);
                Ok(ProcessingQueue::Memory(queue))
            }
            QueueType::Redis => {
                let url = redis_uri.unwrap_or_else(|| "redis://127.0.0.1/".to_string());

                // Initialize Rustls crypto provider if using TLS (rediss://)
                if url.starts_with("rediss://") {
                    crate::redis::init_crypto_provider();
                }

                // Check if cluster mode is enabled or auto-detect from URI
                let is_cluster = redis_cluster || crate::redis::is_cluster_uri(&url);

                if is_cluster {
                    tracing::debug!(
                        "Creating Redis Streams cluster processing queue for: {}",
                        queue_name
                    );
                    let client = ::redis::cluster::ClusterClient::new(vec![url.as_str()])?;
                    let queue = streams::RedisStreamQueue::<T>::new_cluster(client, queue_name)
                        .with_serialization(serialization);
                    Ok(ProcessingQueue::Redis(Box::new(queue)))
                } else {
                    tracing::debug!(
                        "Creating Redis Streams standalone processing queue for: {}",
                        queue_name
                    );
                    let client = ::redis::Client::open(url.as_str())?;
                    let queue = streams::RedisStreamQueue::<T>::new(client, queue_name)
                        .with_serialization(serialization);
                    Ok(ProcessingQueue::Redis(Box::new(queue)))
                }
            }
        }
    }

    /// Override the consumer name on the underlying Redis Streams queue.
    /// No-op for Memory queues. Returns self by value to support a builder
    /// pattern after `clone()` in worker spawn loops:
    ///
    /// ```ignore
    /// let q = base_queue.clone().with_consumer_name(format!("worker-{i}"));
    /// ```
    pub fn with_consumer_name(self, name: String) -> Self {
        match self {
            ProcessingQueue::Memory(q) => ProcessingQueue::Memory(q),
            ProcessingQueue::Redis(q) => {
                ProcessingQueue::Redis(Box::new((*q).with_consumer_name(name)))
            }
        }
    }

    /// Configure the startup retention window on the underlying Redis Streams
    /// queue. No-op for Memory queues (which never persist anything anyway).
    /// See `RedisStreamQueue::with_startup_window` for full semantics.
    pub fn with_startup_window(self, window: std::time::Duration) -> Self {
        match self {
            ProcessingQueue::Memory(q) => ProcessingQueue::Memory(q),
            ProcessingQueue::Redis(q) => {
                ProcessingQueue::Redis(Box::new((*q).with_startup_window(window)))
            }
        }
    }

    /// Configure how many Redis stream entries this handle may claim per read.
    /// Memory queues ignore this. Keeping this at 1 is useful when local
    /// execution capacity is 1-per-handle and we do not want to hide backlog in
    /// Redis PEL or the local prefetch buffer.
    pub fn with_read_batch_size(self, size: usize) -> Self {
        match self {
            ProcessingQueue::Memory(q) => ProcessingQueue::Memory(q),
            ProcessingQueue::Redis(q) => {
                ProcessingQueue::Redis(Box::new((*q).with_read_batch_size(size)))
            }
        }
    }

    /// Configure Redis orphan reclaim idle time. No-op for Memory queues.
    pub fn with_orphan_idle_ms(self, orphan_idle_ms: u64) -> Self {
        match self {
            ProcessingQueue::Memory(q) => ProcessingQueue::Memory(q),
            ProcessingQueue::Redis(q) => {
                ProcessingQueue::Redis(Box::new((*q).with_orphan_idle_ms(orphan_idle_ms)))
            }
        }
    }

    /// Fast-forward the consumer group's last-delivered-id past any backlog
    /// older than the configured startup window, so a worker coming back from
    /// a long outage doesn't drain a stale 4-day flood.
    ///
    /// Returns the number of stream entries skipped (lower bound — capped at
    /// 10k to keep the count cheap). `Ok(0)` if no window was configured, the
    /// group is already within the window, or the queue is in-memory.
    ///
    /// Should be called once per queue at worker startup, **before** the
    /// first dequeue.
    pub async fn fast_forward_if_stale(&self) -> Result<usize, Box<dyn Error + Send + Sync>> {
        match self {
            ProcessingQueue::Memory(_) => Ok(0),
            ProcessingQueue::Redis(q) => q
                .fast_forward_if_stale()
                .await
                .map_err(|e| Box::new(e) as Box<dyn Error + Send + Sync>),
        }
    }

    pub async fn consumer_has_pending(&self) -> Result<bool, Box<dyn Error + Send + Sync>> {
        match self {
            ProcessingQueue::Memory(_) => Ok(false),
            ProcessingQueue::Redis(q) => q
                .consumer_has_pending()
                .await
                .map_err(|e| Box::new(e) as Box<dyn Error + Send + Sync>),
        }
    }

    /// Recover orphaned items from processing keys (Redis only)
    ///
    /// This method scans for orphaned processing keys (items that were being processed
    /// when the worker crashed) and moves them back to the main queue. Should be called
    /// on worker startup to prevent data loss.
    ///
    /// For Memory queues, this is a no-op (returns Ok(0)).
    pub async fn recover_orphaned_items(&self) -> Result<usize, Box<dyn Error + Send + Sync>> {
        match self {
            ProcessingQueue::Memory(_) => {
                // Memory queues don't persist across restarts, so no orphaned items
                Ok(0)
            }
            ProcessingQueue::Redis(q) => q
                .recover_orphaned_items()
                .await
                .map_err(|e| Box::new(e) as Box<dyn Error + Send + Sync>),
        }
    }

    /// ACK and discard all pending messages older than `max_age_ms`.
    /// Returns the number of messages purged.
    /// For Memory queues, this is a no-op (returns Ok(0)).
    pub async fn purge_old_pending(
        &self,
        max_age_ms: u64,
    ) -> Result<usize, Box<dyn Error + Send + Sync>> {
        match self {
            ProcessingQueue::Memory(_) => Ok(0),
            ProcessingQueue::Redis(q) => q
                .purge_old_pending(max_age_ms)
                .await
                .map_err(|e| Box::new(e) as Box<dyn Error + Send + Sync>),
        }
    }

    /// Claim one item, parking until work is available.
    ///
    /// The returned lease must be processed to completion. Dropping it is only
    /// a best-effort recovery path: memory queues try to re-enqueue, while
    /// Redis queues leave the stream entry pending for redelivery.
    pub async fn claim_blocking(
        &self,
    ) -> Result<Option<ProcessingQueueLease<T>>, Box<dyn Error + Send + Sync>>
    where
        T: Send + Sync + Clone + serde::Serialize + serde::de::DeserializeOwned + 'static,
    {
        match self {
            ProcessingQueue::Memory(q) => Ok(Some(ProcessingQueueLease::Memory(
                q.claim_blocking().await?,
            ))),
            ProcessingQueue::Redis(q) => Ok(q
                .claim_blocking()
                .await
                .map_err(|e| Box::new(e) as Box<dyn Error + Send + Sync>)?
                .map(|lease| ProcessingQueueLease::Redis(Box::new(lease)))),
        }
    }

    /// Blocking variant of `dequeue_and_work`.
    ///
    /// - Memory: parks the future on the underlying channel until a message
    ///   is enqueued (or the channel closes).
    /// - Redis: delegates to `dequeue_and_work`; the internal `XREADGROUP BLOCK`
    ///   provides the natural park time.
    ///
    /// Do not race this full claim+process future as a losing branch in
    /// `tokio::select!`; use `claim_blocking` and process the winning lease
    /// explicitly if the wait must be cancellation-safe.
    pub async fn process_blocking<F, Fut, R>(
        &self,
        worker: F,
    ) -> Result<Option<R>, Box<dyn Error + Send + Sync>>
    where
        T: Send + Sync + Clone + serde::Serialize + serde::de::DeserializeOwned + 'static,
        F: FnOnce(T) -> Fut + Send,
        Fut: Future<Output = Result<R, Box<dyn Error + Send + Sync>>> + Send,
        R: Send + Sync,
    {
        match self {
            ProcessingQueue::Memory(q) => q.process_blocking(worker).await,
            ProcessingQueue::Redis(q) => q.process_blocking(worker).await,
        }
    }

    /// Recover orphaned items and return the raw bytes of each recovered item.
    ///
    /// This is useful when you need to inspect the recovered items (e.g., to extract
    /// event_ids and mark associated runs as failed due to worker crash).
    ///
    /// Returns (count, Vec<raw_bytes>) - the raw bytes can be deserialized by the caller.
    ///
    /// For Memory queues, this is a no-op (returns Ok((0, vec![]))).
    pub async fn recover_orphaned_items_with_data(
        &self,
    ) -> Result<(usize, Vec<Vec<u8>>), Box<dyn Error + Send + Sync>> {
        match self {
            ProcessingQueue::Memory(_) => {
                // Memory queues don't persist across restarts, so no orphaned items
                Ok((0, vec![]))
            }
            ProcessingQueue::Redis(q) => q
                .recover_orphaned_items_with_data()
                .await
                .map_err(|e| Box::new(e) as Box<dyn Error + Send + Sync>),
        }
    }
}

#[async_trait]
impl<T: Send + Sync> StreamCleanup for ProcessingQueue<T> {
    async fn cleanup_streams(&self) -> Result<(usize, usize), Box<dyn Error + Send + Sync>> {
        match self {
            ProcessingQueue::Memory(_) => Ok((0, 0)),
            ProcessingQueue::Redis(q) => {
                let consumers_removed = q
                    .cleanup_stale_consumers()
                    .await
                    .map_err(|e| Box::new(e) as Box<dyn Error + Send + Sync>)?;
                let entries_trimmed = q
                    .trim_stream()
                    .await
                    .map_err(|e| Box::new(e) as Box<dyn Error + Send + Sync>)?;
                Ok((consumers_removed, entries_trimmed))
            }
        }
    }

    async fn reclaim_orphans(&self) -> Result<usize, Box<dyn Error + Send + Sync>> {
        match self {
            ProcessingQueue::Memory(_) => Ok(0),
            ProcessingQueue::Redis(q) => q
                .recover_orphaned_items()
                .await
                .map_err(|e| Box::new(e) as Box<dyn Error + Send + Sync>),
        }
    }
}

#[async_trait]
impl<T: Send + Sync> ConsumerLifecycle for ProcessingQueue<T> {
    async fn unregister_consumer(&self) -> Result<(), Box<dyn Error + Send + Sync>> {
        match self {
            ProcessingQueue::Memory(_) => Ok(()),
            ProcessingQueue::Redis(q) => q
                .unregister_consumer()
                .await
                .map_err(|e| Box::new(e) as Box<dyn Error + Send + Sync>),
        }
    }
}

#[async_trait]
impl<T: Send + Sync + serde::Serialize + serde::de::DeserializeOwned + Clone + 'static> Queue<T>
    for ProcessingQueue<T>
{
    async fn enqueue(&self, item: T) -> Result<(), Box<dyn Error + Send + Sync>> {
        match self {
            ProcessingQueue::Memory(q) => q.enqueue(item).await,
            ProcessingQueue::Redis(q) => q.enqueue(item).await,
        }
    }

    async fn dequeue(&self) -> Result<Option<T>, Box<dyn Error + Send + Sync>> {
        match self {
            ProcessingQueue::Memory(q) => q.dequeue().await,
            ProcessingQueue::Redis(q) => q.dequeue().await,
        }
    }

    async fn len(&self) -> Result<usize, Box<dyn Error + Send + Sync>> {
        match self {
            ProcessingQueue::Memory(q) => q.len().await,
            ProcessingQueue::Redis(q) => q.len().await,
        }
    }

    async fn move_to_dead_letter_queue(
        &self,
        item: T,
        reason: String,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        match self {
            ProcessingQueue::Memory(q) => q.move_to_dead_letter_queue(item, reason).await,
            ProcessingQueue::Redis(q) => q.move_to_dead_letter_queue(item, reason).await,
        }
    }
}

#[async_trait]
impl<T: Send + Sync + serde::Serialize + serde::de::DeserializeOwned + Clone + 'static>
    QueueProcessor<T> for ProcessingQueue<T>
{
    async fn dequeue_and_work<F, Fut, R>(
        &self,
        worker: F,
    ) -> Result<Option<R>, Box<dyn Error + Send + Sync>>
    where
        F: FnOnce(T) -> Fut + Send,
        Fut: Future<Output = Result<R, Box<dyn Error + Send + Sync>>> + Send,
        R: Send + Sync,
    {
        match self {
            ProcessingQueue::Memory(q) => q.dequeue_and_work(worker).await,
            ProcessingQueue::Redis(q) => q.dequeue_and_work(worker).await,
        }
    }
}

// Example usage:
//
// ```
// use crate::queue::{QueueType, ProcessingQueue, QueueProcessor};
// use crate::data::serialization::Serialization;
//
// async fn process_items<T: Clone + Send + Sync + serde::Serialize + serde::de::DeserializeOwned + 'static>() {
//     // Create a queue processor
//     let queue = ProcessingQueue::<T>::new(
//         QueueType::Memory,
//         "my_queue".to_string(),
//         None,
//         Serialization::Json
//     ).unwrap();
//
//     // Dequeue and process items with automatic retry on failure
//     let result = queue.dequeue_and_work(|item| async move {
//         // Process the item...
//         println!("Processing item...");
//
//         // Return success
//         Ok(42) // Or return an error to have the item automatically requeued
//     }).await;
//
//     match result {
//         Ok(Some(value)) => println!("Processed successfully: {}", value),
//         Ok(None) => println!("No items to process"),
//         Err(e) => println!("Error: {}", e),
//     }
// }
// ```

/// Get resolved configuration for queue settings.
///
/// Type resolution order (highest priority wins):
/// 1. User-provided type (from env var HOT_QUEUE_TYPE, hot.hot, or CLI --queue.type)
/// 2. Context default: "memory" when `in_project` is true, "none" otherwise
///
/// When `in_project` is true (hot.hot exists), defaults to "memory" for local event publishing.
/// When `in_project` is false, defaults to "none" (queue disabled).
pub fn get_resolved_conf(conf: Val, in_project: bool) -> Val {
    let default_conf = val!({
        "event-orphan-idle-ms": 60_000i64,
        "task-orphan-idle-ms": 120_000i64,
        "infra-retry-backoff-ms": 1_000i64,
        "wait-target-p99-ms": 1_000i64,
        "metrics-enabled": true
    });
    let conf = default_conf.merge(&conf);

    // Check if user explicitly set a type (from hot.hot or CLI)
    // Empty string means not set, so use context-based default
    let user_type = conf.get_str_or_default("type", "");
    let user_explicitly_set_type = !user_type.is_empty();

    // Default type depends on context:
    // - In project: "memory" enables send() for local event publishing
    // - Outside project: "none" disables queue
    // But if user explicitly set a type, use that
    let queue_type = if user_explicitly_set_type {
        user_type
    } else if in_project {
        "memory".to_string()
    } else {
        "none".to_string()
    };

    // Set the type explicitly on merged conf so it doesn't get overwritten by empty string
    conf.set_str("type", Some(queue_type), "")
}

// ============================================================================
// Queue Admin Utilities
// ============================================================================

/// Standard Hot queue names (without hash tags)
pub const QUEUE_NAMES: &[&str] = &[
    "hot:event",
    "hot:request",
    "hot:response",
    "hot:alert",
    "hot:email",
    "hot:task",
    "hot:deploy",
];

/// Convert a queue name to cluster-compatible format with hash tags.
/// In Redis Cluster, keys are distributed across slots based on a hash of the key.
/// Hash tags `{...}` allow forcing related keys to the same slot.
///
/// Example: `hot:event` becomes `{hot:event}` in cluster mode
pub fn cluster_queue_name(queue_name: &str, is_cluster: bool) -> String {
    if is_cluster && !queue_name.contains('{') {
        format!("{{{}}}", queue_name)
    } else {
        queue_name.to_string()
    }
}

/// Get the list of standard queue names, optionally with cluster hash tags
pub fn get_queue_names(is_cluster: bool) -> Vec<String> {
    QUEUE_NAMES
        .iter()
        .map(|name| cluster_queue_name(name, is_cluster))
        .collect()
}

/// Queue status information
#[derive(Debug, Clone, Default)]
pub struct QueueStatus {
    pub name: String,
    pub pending: i64,
    pub processing: i64,
    pub deadletter: i64,
}

/// Summary of all queue statuses
#[derive(Debug, Clone, Default)]
pub struct QueueStatusSummary {
    pub queues: Vec<QueueStatus>,
    pub total_pending: i64,
    pub total_processing: i64,
    pub total_deadletter: i64,
}

/// Admin operations for Redis queues
pub struct RedisQueueAdmin {
    is_cluster: bool,
    uri: String,
}

impl RedisQueueAdmin {
    /// Create a new RedisQueueAdmin
    pub fn new(uri: String, is_cluster: bool) -> Self {
        // Initialize TLS if needed
        if uri.starts_with("rediss://") {
            crate::redis::init_crypto_provider();
        }
        Self { is_cluster, uri }
    }

    /// Get queue names with hash tags matching the actual Redis Stream keys.
    ///
    /// `RedisStreamQueue` always wraps stream names in hash tags `{...}` for
    /// cluster compatibility, regardless of whether cluster mode is enabled.
    /// Admin operations must use the same key format to target the correct keys.
    pub fn queue_names(&self) -> Vec<String> {
        get_queue_names(true)
    }

    /// Clear all standard queues (main, processing, and deadletter)
    pub fn clear_all(&self) -> Result<Vec<String>, String> {
        let queue_names = self.queue_names();

        let cleared = if self.is_cluster {
            let client = ::redis::cluster::ClusterClient::new(vec![self.uri.as_str()])
                .map_err(|e| format!("Failed to create cluster client: {}", e))?;
            let mut conn = client
                .get_connection()
                .map_err(|e| format!("Failed to get cluster connection: {}", e))?;
            self.clear_queues_impl(&mut conn, &queue_names)?
        } else {
            let client = ::redis::Client::open(self.uri.as_str())
                .map_err(|e| format!("Failed to connect to Redis: {}", e))?;
            let mut conn = client
                .get_connection()
                .map_err(|e| format!("Failed to get Redis connection: {}", e))?;
            self.clear_queues_impl(&mut conn, &queue_names)?
        };

        Ok(cleared)
    }

    fn clear_queues_impl(
        &self,
        conn: &mut dyn ::redis::ConnectionLike,
        queue_names: &[String],
    ) -> Result<Vec<String>, String> {
        let mut cleared = Vec::new();

        for queue_name in queue_names {
            // Keys to delete: main stream and dead letter stream
            // Note: With Redis Streams, there's no separate "processing" key -
            // pending messages are tracked within the stream via consumer groups
            let keys = vec![queue_name.clone(), format!("{}:deadletter", queue_name)];

            for key in keys {
                let deleted: i64 = ::redis::cmd("DEL")
                    .arg(&key)
                    .query(conn)
                    .map_err(|e| format!("Failed to delete {}: {}", key, e))?;
                if deleted > 0 {
                    cleared.push(key);
                }
            }
        }

        Ok(cleared)
    }

    /// Get status of all standard queues
    pub fn status(&self) -> Result<QueueStatusSummary, String> {
        let queue_names = self.queue_names();

        if self.is_cluster {
            let client = ::redis::cluster::ClusterClient::new(vec![self.uri.as_str()])
                .map_err(|e| format!("Failed to create cluster client: {}", e))?;
            let mut conn = client
                .get_connection()
                .map_err(|e| format!("Failed to get cluster connection: {}", e))?;
            self.status_impl(&mut conn, &queue_names)
        } else {
            let client = ::redis::Client::open(self.uri.as_str())
                .map_err(|e| format!("Failed to connect to Redis: {}", e))?;
            let mut conn = client
                .get_connection()
                .map_err(|e| format!("Failed to get Redis connection: {}", e))?;
            self.status_impl(&mut conn, &queue_names)
        }
    }

    fn status_impl(
        &self,
        conn: &mut dyn ::redis::ConnectionLike,
        queue_names: &[String],
    ) -> Result<QueueStatusSummary, String> {
        let mut summary = QueueStatusSummary::default();

        for queue_name in queue_names {
            // For Redis Streams, use XLEN for total stream length
            // and XPENDING for pending (unacked) messages
            let stream_len: i64 = ::redis::cmd("XLEN")
                .arg(queue_name.as_str())
                .query(conn)
                .unwrap_or(0);

            // XPENDING returns [count, min-id, max-id, [[consumer, count], ...]]
            let pending_result: redis::Value = ::redis::cmd("XPENDING")
                .arg(queue_name.as_str())
                .arg("hot-workers")
                .query(conn)
                .unwrap_or(redis::Value::Nil);

            let processing: i64 = match pending_result {
                redis::Value::Array(ref parts) if !parts.is_empty() => {
                    redis::from_redis_value_ref(&parts[0]).unwrap_or(0)
                }
                _ => 0,
            };

            // pending = total in stream - currently processing (already delivered but not acked)
            let pending = stream_len - processing;

            // Dead letter queue is also a stream now
            let deadletter_key = format!("{}:deadletter", queue_name);
            let deadletter: i64 = ::redis::cmd("XLEN")
                .arg(&deadletter_key)
                .query(conn)
                .unwrap_or(0);

            summary.total_pending += pending;
            summary.total_processing += processing;
            summary.total_deadletter += deadletter;

            summary.queues.push(QueueStatus {
                name: queue_name.clone(),
                pending,
                processing,
                deadletter,
            });
        }

        Ok(summary)
    }

    /// Check if running in cluster mode
    pub fn is_cluster(&self) -> bool {
        self.is_cluster
    }

    /// Get the Redis URI
    pub fn uri(&self) -> &str {
        &self.uri
    }
}

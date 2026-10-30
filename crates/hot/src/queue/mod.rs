use crate::data::serialization::Serialization;
use crate::val::{Val, val};
use async_trait::async_trait;
use std::error::Error;
use std::fmt;
use std::future::Future;
use std::io::Read;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

pub mod mem;
pub mod sqlite;
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
    Memory, // In-process channel-backed queue (single process only)
    #[default]
    Sqlite, // Project-local durable queue with in-process fast notifications
    Redis,  // Redis-backed queue (multi-process / production)
}

/// A persisted queue message returned while recovering expired leases.
#[derive(Debug, Clone)]
pub struct RecoveredQueueMessage {
    payload: Vec<u8>,
    serialization: Serialization,
}

impl RecoveredQueueMessage {
    pub(crate) fn new(payload: Vec<u8>, serialization: Serialization) -> Self {
        Self {
            payload,
            serialization,
        }
    }

    pub fn decode<T: serde::de::DeserializeOwned>(&self) -> Result<T, String> {
        match self.serialization {
            Serialization::Json => {
                serde_json::from_slice(&self.payload).map_err(|error| error.to_string())
            }
            Serialization::ZstdJson => {
                let mut json = Vec::new();
                zstd::Decoder::new(self.payload.as_slice())
                    .map_err(|error| error.to_string())?
                    .read_to_end(&mut json)
                    .map_err(|error| error.to_string())?;
                serde_json::from_slice(&json).map_err(|error| error.to_string())
            }
        }
    }

    pub fn payload(&self) -> &[u8] {
        &self.payload
    }
}

/// Result of recovering queue messages whose processing leases expired.
#[derive(Debug, Default)]
pub struct OrphanRecovery {
    /// Number of messages made available for another processing attempt.
    pub requeued_count: usize,
    /// Requeued messages available to backends that can return their payloads.
    pub requeued_messages: Vec<RecoveredQueueMessage>,
    /// Messages moved to the durable dead-letter state during this recovery.
    pub dead_lettered_messages: Vec<RecoveredQueueMessage>,
}

impl OrphanRecovery {
    pub fn total_count(&self) -> usize {
        self.requeued_count + self.dead_lettered_messages.len()
    }
}

impl fmt::Display for QueueType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            QueueType::Memory => write!(f, "memory"),
            QueueType::Sqlite => write!(f, "sqlite"),
            QueueType::Redis => write!(f, "redis"),
        }
    }
}

impl FromStr for QueueType {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "memory" | "mem" => Ok(QueueType::Memory),
            "sqlite" => Ok(QueueType::Sqlite),
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
/// * `queue_type` - The type of queue to create (Memory, SQLite, or Redis)
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
/// * `queue_type` - The type of queue to create (Memory, SQLite, or Redis)
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
        QueueType::Sqlite => {
            let queue =
                sqlite::SqliteQueue::<T>::new(queue_name)?.with_serialization(serialization);
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
                let client = crate::redis::client(url.as_str())?;
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
/// * `queue_type` - The type of queue to create (Memory, SQLite, or Redis)
/// * `queue_name` - The name of the queue
/// * `redis_uri` - Optional Redis URL (only used for Redis queue type)
/// * `serialization` - Serialization format to use
///
/// # Returns
/// * A specific queue implementation that implements QueueProcessor
pub enum ProcessingQueue<T> {
    Memory(mem::MemQueue<T>),
    Sqlite(sqlite::SqliteQueue<T>),
    Redis(Box<streams::RedisStreamQueue<T>>),
}

pub enum ProcessingQueueLease<T>
where
    T: Send + Sync + serde::Serialize + serde::de::DeserializeOwned + Clone + 'static,
{
    Memory(mem::MemQueueLease<T>),
    Sqlite(sqlite::SqliteQueueLease<T>),
    Redis(Box<streams::RedisQueueLease<T>>),
}

#[derive(Clone, Copy, Debug)]
pub struct QueueLeaseTiming {
    /// Timestamp captured when Redis/memory handed the item to the claimer.
    pub claimed_at: chrono::DateTime<chrono::Utc>,
    /// Redis XADD timestamp. Memory queues do not expose a wall-clock enqueue
    /// timestamp, so this is None for that backend.
    pub enqueued_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Backend queue residence measured at claim time.
    pub queue_wait: std::time::Duration,
    /// True when this claim came from a backend retry/reclaim path. In that
    /// case queue_wait is the item's age, not a fresh queue residence.
    pub redelivered: bool,
}

impl<T> ProcessingQueueLease<T>
where
    T: Send + Sync + serde::Serialize + serde::de::DeserializeOwned + Clone + 'static,
{
    pub fn timing(&self) -> QueueLeaseTiming {
        match self {
            ProcessingQueueLease::Memory(lease) => lease.timing(),
            ProcessingQueueLease::Sqlite(lease) => lease.timing(),
            ProcessingQueueLease::Redis(lease) => lease.timing(),
        }
    }

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
            ProcessingQueueLease::Sqlite(lease) => lease.process(worker).await,
            ProcessingQueueLease::Redis(lease) => lease.process(worker).await,
        }
    }
}

impl<T: Clone> Clone for ProcessingQueue<T> {
    fn clone(&self) -> Self {
        match self {
            ProcessingQueue::Memory(q) => ProcessingQueue::Memory(q.clone()),
            ProcessingQueue::Sqlite(q) => ProcessingQueue::Sqlite(q.clone()),
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
            QueueType::Sqlite => {
                let queue =
                    sqlite::SqliteQueue::<T>::new(queue_name)?.with_serialization(serialization);
                Ok(ProcessingQueue::Sqlite(queue))
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
                    let client = crate::redis::client(url.as_str())?;
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
            ProcessingQueue::Sqlite(q) => ProcessingQueue::Sqlite(q.with_consumer_name(name)),
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
            ProcessingQueue::Sqlite(q) => ProcessingQueue::Sqlite(q.with_startup_window(window)),
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
            ProcessingQueue::Sqlite(q) => ProcessingQueue::Sqlite(q),
            ProcessingQueue::Redis(q) => {
                ProcessingQueue::Redis(Box::new((*q).with_read_batch_size(size)))
            }
        }
    }

    /// Configure Redis orphan reclaim idle time. No-op for Memory queues.
    pub fn with_orphan_idle_ms(self, orphan_idle_ms: u64) -> Self {
        match self {
            ProcessingQueue::Memory(q) => ProcessingQueue::Memory(q),
            ProcessingQueue::Sqlite(q) => {
                ProcessingQueue::Sqlite(q.with_orphan_idle_ms(orphan_idle_ms))
            }
            ProcessingQueue::Redis(q) => {
                ProcessingQueue::Redis(Box::new((*q).with_orphan_idle_ms(orphan_idle_ms)))
            }
        }
    }

    /// Fast-forward a Redis consumer group's last-delivered-id past backlog
    /// older than the configured startup window. SQLite deliberately no-ops:
    /// its rows are the authoritative copy and remain durable until processed.
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
            ProcessingQueue::Sqlite(q) => q
                .fast_forward_if_stale()
                .await
                .map_err(|e| Box::new(e) as Box<dyn Error + Send + Sync>),
            ProcessingQueue::Redis(q) => q
                .fast_forward_if_stale()
                .await
                .map_err(|e| Box::new(e) as Box<dyn Error + Send + Sync>),
        }
    }

    pub async fn consumer_has_pending(&self) -> Result<bool, Box<dyn Error + Send + Sync>> {
        match self {
            ProcessingQueue::Memory(_) => Ok(false),
            ProcessingQueue::Sqlite(q) => q
                .consumer_has_pending()
                .await
                .map_err(|e| Box::new(e) as Box<dyn Error + Send + Sync>),
            ProcessingQueue::Redis(q) => q
                .consumer_has_pending()
                .await
                .map_err(|e| Box::new(e) as Box<dyn Error + Send + Sync>),
        }
    }

    /// Recover orphaned items after a worker lease expires.
    ///
    /// This method requeues recoverable Redis/SQLite items. SQLite items that
    /// exhausted their retry allowance enter the durable DLQ; callers that own
    /// associated application state should use `recover_orphaned_items_with_data`.
    ///
    /// For Memory queues, this is a no-op (returns Ok(0)).
    pub async fn recover_orphaned_items(&self) -> Result<usize, Box<dyn Error + Send + Sync>> {
        match self {
            ProcessingQueue::Memory(_) => {
                // Memory queues don't persist across restarts, so no orphaned items
                Ok(0)
            }
            ProcessingQueue::Sqlite(q) => q
                .recover_orphaned_items()
                .await
                .map_err(|e| Box::new(e) as Box<dyn Error + Send + Sync>),
            ProcessingQueue::Redis(q) => q
                .recover_orphaned_items()
                .await
                .map_err(|e| Box::new(e) as Box<dyn Error + Send + Sync>),
        }
    }

    /// ACK and discard Redis messages older than `max_age_ms`.
    /// Returns the number of messages purged.
    /// Memory and authoritative SQLite queues are no-ops (return Ok(0)).
    pub async fn purge_old_pending(
        &self,
        max_age_ms: u64,
    ) -> Result<usize, Box<dyn Error + Send + Sync>> {
        match self {
            ProcessingQueue::Memory(_) => Ok(0),
            ProcessingQueue::Sqlite(q) => q
                .purge_old_pending(max_age_ms)
                .await
                .map_err(|e| Box::new(e) as Box<dyn Error + Send + Sync>),
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
            ProcessingQueue::Sqlite(q) => Ok(q
                .claim_blocking()
                .await
                .map_err(|e| Box::new(e) as Box<dyn Error + Send + Sync>)?
                .map(ProcessingQueueLease::Sqlite)),
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
            ProcessingQueue::Sqlite(q) => q.process_blocking(worker).await,
            ProcessingQueue::Redis(q) => q.process_blocking(worker).await,
        }
    }

    /// Recover orphaned items and return payloads when the backend supports it.
    ///
    /// This is useful when you need to inspect the recovered items (e.g., to extract
    /// event_ids and mark associated runs as failed due to worker crash).
    ///
    /// SQLite distinguishes messages requeued for another attempt from messages
    /// that exhausted their retry allowance and entered the durable DLQ. Redis
    /// reports only a requeue count because XAUTOCLAIM leaves payload delivery
    /// to the normal consumer flow.
    pub async fn recover_orphaned_items_with_data(
        &self,
    ) -> Result<OrphanRecovery, Box<dyn Error + Send + Sync>> {
        match self {
            ProcessingQueue::Memory(_) => {
                // Memory queues don't persist across restarts, so no orphaned items
                Ok(OrphanRecovery::default())
            }
            ProcessingQueue::Sqlite(q) => q
                .recover_orphaned_items_with_data()
                .await
                .map_err(|e| Box::new(e) as Box<dyn Error + Send + Sync>),
            ProcessingQueue::Redis(q) => q
                .recover_orphaned_items_with_data()
                .await
                .map_err(|e| Box::new(e) as Box<dyn Error + Send + Sync>),
        }
    }
}

#[async_trait]
impl<T> StreamCleanup for ProcessingQueue<T>
where
    T: Clone + Send + Sync + serde::Serialize + serde::de::DeserializeOwned + 'static,
{
    async fn cleanup_streams(&self) -> Result<(usize, usize), Box<dyn Error + Send + Sync>> {
        match self {
            ProcessingQueue::Memory(_) => Ok((0, 0)),
            ProcessingQueue::Sqlite(_) => Ok((0, 0)),
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
            ProcessingQueue::Sqlite(q) => q
                .recover_orphaned_items()
                .await
                .map_err(|e| Box::new(e) as Box<dyn Error + Send + Sync>),
            ProcessingQueue::Redis(q) => q
                .recover_orphaned_items()
                .await
                .map_err(|e| Box::new(e) as Box<dyn Error + Send + Sync>),
        }
    }
}

#[async_trait]
impl<T> ConsumerLifecycle for ProcessingQueue<T>
where
    T: Clone + Send + Sync + serde::Serialize + serde::de::DeserializeOwned + 'static,
{
    async fn unregister_consumer(&self) -> Result<(), Box<dyn Error + Send + Sync>> {
        match self {
            ProcessingQueue::Memory(_) => Ok(()),
            ProcessingQueue::Sqlite(q) => q
                .unregister_consumer()
                .await
                .map_err(|e| Box::new(e) as Box<dyn Error + Send + Sync>),
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
            ProcessingQueue::Sqlite(q) => q.enqueue(item).await,
            ProcessingQueue::Redis(q) => q.enqueue(item).await,
        }
    }

    async fn dequeue(&self) -> Result<Option<T>, Box<dyn Error + Send + Sync>> {
        match self {
            ProcessingQueue::Memory(q) => q.dequeue().await,
            ProcessingQueue::Sqlite(q) => q.dequeue().await,
            ProcessingQueue::Redis(q) => q.dequeue().await,
        }
    }

    async fn len(&self) -> Result<usize, Box<dyn Error + Send + Sync>> {
        match self {
            ProcessingQueue::Memory(q) => q.len().await,
            ProcessingQueue::Sqlite(q) => q.len().await,
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
            ProcessingQueue::Sqlite(q) => q.move_to_dead_letter_queue(item, reason).await,
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
            ProcessingQueue::Sqlite(q) => q.dequeue_and_work(worker).await,
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
/// 2. Context default: "sqlite" when `in_project` is true, "none" otherwise
///
/// When `in_project` is true (hot.hot exists), defaults to the project-local
/// SQLite queue for durable, cross-process local delivery.
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
    // - In project: "sqlite" enables durable local event publishing
    // - Outside project: "none" disables queue
    // But if user explicitly set a type, use that
    let queue_type = if user_explicitly_set_type {
        user_type
    } else if in_project {
        "sqlite".to_string()
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
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct QueueStatus {
    pub name: String,
    /// Messages no worker has taken yet.
    pub pending: i64,
    /// Messages a worker has taken and not yet finished.
    pub processing: i64,
    pub deadletter: i64,
    /// How long the oldest pending message has waited, in seconds; `None`
    /// when nothing is pending.
    pub oldest_pending_secs: Option<i64>,
}

/// Summary of all queue statuses
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct QueueStatusSummary {
    pub queues: Vec<QueueStatus>,
    pub total_pending: i64,
    pub total_processing: i64,
    pub total_deadletter: i64,
    /// The longest wait among every queue's oldest pending message.
    pub oldest_pending_secs: Option<i64>,
}

impl QueueStatusSummary {
    /// Add one queue's status to the totals.
    pub fn push(&mut self, queue: QueueStatus) {
        self.total_pending += queue.pending;
        self.total_processing += queue.processing;
        self.total_deadletter += queue.deadletter;
        self.oldest_pending_secs = self.oldest_pending_secs.max(queue.oldest_pending_secs);
        self.queues.push(queue);
    }
}

/// Upper bound on the entries counted one by one when a consumer group
/// cannot report its lag. Beyond it the count is reported as this bound.
const PENDING_COUNT_LIMIT: i64 = 10_000;

/// What `XINFO GROUPS` reports about one consumer group.
struct GroupInfo {
    last_delivered_id: String,
    /// Entries delivered and not yet acknowledged.
    pending: i64,
    /// Entries not yet delivered, when Redis can tell (it cannot after the
    /// group was moved with `XGROUP SETID` or entries were deleted).
    lag: Option<i64>,
}

/// Admin operations for Redis queues
pub struct RedisQueueAdmin {
    is_cluster: bool,
    uri: String,
    password: Option<String>,
}

impl RedisQueueAdmin {
    /// Create a new RedisQueueAdmin
    pub fn new(uri: String, is_cluster: bool) -> Self {
        // Initialize TLS if needed
        if uri.starts_with("rediss://") {
            crate::redis::init_crypto_provider();
        }
        Self {
            is_cluster,
            uri,
            password: None,
        }
    }

    /// Authenticate standalone connections with this password, as the
    /// default user; see [`crate::redis::one_off_password`].
    pub fn with_password(mut self, password: Option<String>) -> Self {
        self.password = password;
        self
    }

    fn standalone_connection(&self) -> Result<::redis::Connection, String> {
        let client = crate::redis::client(self.uri.as_str())
            .map_err(|e| format!("Failed to connect to Redis: {}", e))?;
        let mut conn = client
            .get_connection()
            .map_err(|e| format!("Failed to get Redis connection: {}", e))?;
        if let Some(password) = &self.password {
            let _: () = ::redis::cmd("AUTH")
                .arg("default")
                .arg(password)
                .query(&mut conn)
                .map_err(|e| format!("Failed to authenticate to Redis: {}", e))?;
        }
        Ok(conn)
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
            let mut conn = self.standalone_connection()?;
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
            let mut touched_stream = false;
            for group_name in Self::stream_groups(conn, queue_name)? {
                Self::ack_all_pending(conn, queue_name, &group_name)?;
                let _: () = ::redis::cmd("XGROUP")
                    .arg("SETID")
                    .arg(queue_name)
                    .arg(&group_name)
                    .arg("$")
                    .query(conn)
                    .map_err(|e| {
                        format!(
                            "Failed to reset consumer group {} on {}: {}",
                            group_name, queue_name, e
                        )
                    })?;
                touched_stream = true;
            }

            let trimmed: i64 = ::redis::cmd("XTRIM")
                .arg(queue_name)
                .arg("MAXLEN")
                .arg("=")
                .arg(0)
                .query(conn)
                .unwrap_or(0);
            if trimmed > 0 {
                touched_stream = true;
            }
            if touched_stream {
                cleared.push(queue_name.clone());
            }

            // Dead-letter streams have no consumer groups; deleting them is safe.
            let deadletter_key = format!("{}:deadletter", queue_name);
            let deleted: i64 = ::redis::cmd("DEL")
                .arg(&deadletter_key)
                .query(conn)
                .map_err(|e| format!("Failed to delete {}: {}", deadletter_key, e))?;
            if deleted > 0 {
                cleared.push(deadletter_key);
            }
        }

        Ok(cleared)
    }

    fn stream_groups(
        conn: &mut dyn ::redis::ConnectionLike,
        queue_name: &str,
    ) -> Result<Vec<String>, String> {
        let result: ::redis::Value = match ::redis::cmd("XINFO")
            .arg("GROUPS")
            .arg(queue_name)
            .query(conn)
        {
            Ok(result) => result,
            Err(e) => {
                let msg = e.to_string();
                if msg.contains("no such key") || msg.contains("ERR no such key") {
                    return Ok(Vec::new());
                }
                return Err(format!(
                    "Failed to list consumer groups for {}: {}",
                    queue_name, e
                ));
            }
        };

        let records: Vec<::redis::Value> = ::redis::from_redis_value_ref(&result)
            .map_err(|e| format!("Failed to parse consumer groups for {}: {}", queue_name, e))?;
        let mut groups = Vec::new();
        for record in records {
            let fields: Vec<::redis::Value> = ::redis::from_redis_value_ref(&record)
                .map_err(|e| format!("Failed to parse consumer group for {}: {}", queue_name, e))?;
            let mut iter = fields.iter();
            while let (Some(key), Some(value)) = (iter.next(), iter.next()) {
                let key: String = ::redis::from_redis_value_ref(key).unwrap_or_default();
                if key == "name" {
                    let group_name: String = ::redis::from_redis_value_ref(value).map_err(|e| {
                        format!("Failed to parse group name for {}: {}", queue_name, e)
                    })?;
                    groups.push(group_name);
                    break;
                }
            }
        }
        Ok(groups)
    }

    fn ack_all_pending(
        conn: &mut dyn ::redis::ConnectionLike,
        queue_name: &str,
        group_name: &str,
    ) -> Result<usize, String> {
        let mut total = 0usize;
        loop {
            let result: ::redis::Value = ::redis::cmd("XPENDING")
                .arg(queue_name)
                .arg(group_name)
                .arg("-")
                .arg("+")
                .arg(100)
                .query(conn)
                .map_err(|e| {
                    format!(
                        "Failed to list pending messages for {} group {}: {}",
                        queue_name, group_name, e
                    )
                })?;
            let entries: Vec<::redis::Value> =
                ::redis::from_redis_value_ref(&result).map_err(|e| {
                    format!(
                        "Failed to parse pending messages for {} group {}: {}",
                        queue_name, group_name, e
                    )
                })?;
            if entries.is_empty() {
                break;
            }

            let mut ids = Vec::with_capacity(entries.len());
            for entry in entries {
                let fields: Vec<::redis::Value> =
                    ::redis::from_redis_value_ref(&entry).map_err(|e| {
                        format!(
                            "Failed to parse pending message for {} group {}: {}",
                            queue_name, group_name, e
                        )
                    })?;
                if let Some(id) = fields.first() {
                    ids.push(::redis::from_redis_value_ref::<String>(id).map_err(|e| {
                        format!(
                            "Failed to parse pending message id for {} group {}: {}",
                            queue_name, group_name, e
                        )
                    })?);
                }
            }

            if ids.is_empty() {
                break;
            }
            let mut cmd = ::redis::cmd("XACK");
            cmd.arg(queue_name).arg(group_name);
            for id in &ids {
                cmd.arg(id);
            }
            let acked: i64 = cmd.query(conn).map_err(|e| {
                format!(
                    "Failed to ACK pending messages for {} group {}: {}",
                    queue_name, group_name, e
                )
            })?;
            total += acked.max(0) as usize;

            if ids.len() < 100 {
                break;
            }
        }

        Ok(total)
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
            let mut conn = self.standalone_connection()?;
            self.status_impl(&mut conn, &queue_names)
        }
    }

    fn status_impl(
        &self,
        conn: &mut dyn ::redis::ConnectionLike,
        queue_names: &[String],
    ) -> Result<QueueStatusSummary, String> {
        let mut summary = QueueStatusSummary::default();
        let now_ms = Self::server_time_ms(conn)?;

        for queue_name in queue_names {
            // Acknowledged entries stay in the stream until it is trimmed,
            // so its length is not the backlog. The group's position is:
            // entries after its last-delivered ID are waiting, and its
            // pending entries list holds those taken and not yet finished.
            let (pending, processing, after) =
                match Self::group_info(conn, queue_name, streams::DEFAULT_CONSUMER_GROUP)? {
                    Some(group) => {
                        let after = format!("({}", group.last_delivered_id);
                        let pending = match group.lag {
                            Some(lag) => lag,
                            None => Self::count_entries(conn, queue_name, &after)?,
                        };
                        (pending, group.pending, after)
                    }
                    // No worker has read this queue yet: everything waits.
                    None => {
                        let length: i64 = ::redis::cmd("XLEN")
                            .arg(queue_name.as_str())
                            .query(conn)
                            .unwrap_or(0);
                        (length, 0, "-".to_string())
                    }
                };
            let oldest_pending_secs = if pending > 0 {
                Self::first_entry_ms(conn, queue_name, &after)?
                    .map(|enqueued_ms| (now_ms - enqueued_ms).max(0) / 1000)
            } else {
                None
            };

            let deadletter_key = format!("{}:deadletter", queue_name);
            let deadletter: i64 = ::redis::cmd("XLEN")
                .arg(&deadletter_key)
                .query(conn)
                .unwrap_or(0);

            summary.push(QueueStatus {
                name: queue_name.clone(),
                pending,
                processing,
                deadletter,
                oldest_pending_secs,
            });
        }

        Ok(summary)
    }

    /// The Redis server's clock, which stamped the stream entry IDs.
    fn server_time_ms(conn: &mut dyn ::redis::ConnectionLike) -> Result<i64, String> {
        let (seconds, micros): (i64, i64) = ::redis::cmd("TIME")
            .query(conn)
            .map_err(|e| format!("Failed to read the Redis server time: {}", e))?;
        Ok(seconds * 1000 + micros / 1000)
    }

    fn group_info(
        conn: &mut dyn ::redis::ConnectionLike,
        queue_name: &str,
        group_name: &str,
    ) -> Result<Option<GroupInfo>, String> {
        let result: ::redis::Value =
            match ::redis::cmd("XINFO").arg("GROUPS").arg(queue_name).query(conn) {
                Ok(result) => result,
                Err(e) if e.to_string().contains("no such key") => return Ok(None),
                Err(e) => {
                    return Err(format!(
                        "Failed to list consumer groups for {}: {}",
                        queue_name, e
                    ));
                }
            };
        let records: Vec<Vec<::redis::Value>> = ::redis::from_redis_value_ref(&result)
            .map_err(|e| format!("Failed to parse consumer groups for {}: {}", queue_name, e))?;
        for fields in records {
            let field = |name: &str| {
                fields.chunks(2).find_map(|pair| match pair {
                    [key, value]
                        if ::redis::from_redis_value_ref::<String>(key).ok().as_deref()
                            == Some(name) =>
                    {
                        Some(value)
                    }
                    _ => None,
                })
            };
            let name: Option<String> =
                field("name").and_then(|v| ::redis::from_redis_value_ref(v).ok());
            if name.as_deref() != Some(group_name) {
                continue;
            }
            return Ok(Some(GroupInfo {
                last_delivered_id: field("last-delivered-id")
                    .and_then(|v| ::redis::from_redis_value_ref(v).ok())
                    .unwrap_or_else(|| "0-0".to_string()),
                pending: field("pending")
                    .and_then(|v| ::redis::from_redis_value_ref(v).ok())
                    .unwrap_or(0),
                lag: field("lag").and_then(|v| ::redis::from_redis_value_ref(v).ok()),
            }));
        }
        Ok(None)
    }

    /// Entries from `start` (an `XRANGE` start, such as `(<id>`) to the end
    /// of the stream, counted up to `PENDING_COUNT_LIMIT`.
    fn count_entries(
        conn: &mut dyn ::redis::ConnectionLike,
        queue_name: &str,
        start: &str,
    ) -> Result<i64, String> {
        let mut start = start.to_string();
        let mut counted = 0;
        while counted < PENDING_COUNT_LIMIT {
            let entries: Vec<(String, ::redis::Value)> = ::redis::cmd("XRANGE")
                .arg(queue_name)
                .arg(&start)
                .arg("+")
                .arg("COUNT")
                .arg(1000)
                .query(conn)
                .map_err(|e| format!("Failed to count entries in {}: {}", queue_name, e))?;
            counted += entries.len() as i64;
            match entries.last() {
                Some((id, _)) if entries.len() == 1000 => start = format!("({}", id),
                _ => break,
            }
        }
        Ok(counted.min(PENDING_COUNT_LIMIT))
    }

    /// When the first entry from `start` on was added, in milliseconds
    /// since the epoch, from its ID.
    fn first_entry_ms(
        conn: &mut dyn ::redis::ConnectionLike,
        queue_name: &str,
        start: &str,
    ) -> Result<Option<i64>, String> {
        let entries: Vec<(String, ::redis::Value)> = ::redis::cmd("XRANGE")
            .arg(queue_name)
            .arg(start)
            .arg("+")
            .arg("COUNT")
            .arg(1)
            .query(conn)
            .map_err(|e| format!("Failed to read the oldest entry in {}: {}", queue_name, e))?;
        Ok(entries
            .first()
            .and_then(|(id, _)| id.split('-').next()?.parse().ok()))
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

#[cfg(test)]
mod admin_status_tests {
    use super::*;

    /// A local Redis, or None to skip where none runs.
    fn connection() -> Option<::redis::Connection> {
        ::redis::Client::open("redis://127.0.0.1/")
            .ok()?
            .get_connection()
            .ok()
    }

    fn add(conn: &mut ::redis::Connection, queue: &str, at_ms: i64, seq: usize) -> String {
        ::redis::cmd("XADD")
            .arg(queue)
            .arg(format!("{}-{}", at_ms, seq))
            .arg("payload")
            .arg("x")
            .query(conn)
            .unwrap()
    }

    fn status(conn: &mut ::redis::Connection, queue: &str) -> QueueStatus {
        let admin = RedisQueueAdmin::new("redis://127.0.0.1/".to_string(), false);
        let summary = admin.status_impl(conn, &[queue.to_string()]).unwrap();
        assert_eq!(summary.oldest_pending_secs, summary.queues[0].oldest_pending_secs);
        summary.queues[0].clone()
    }

    #[test]
    fn status_counts_only_undelivered_entries_and_how_long_the_oldest_waited() {
        let Some(mut conn) = connection() else {
            return;
        };
        let queue = format!("{{test-status-{}}}", uuid::Uuid::now_v7());
        let now_ms = RedisQueueAdmin::server_time_ms(&mut conn).unwrap();
        let ids: Vec<String> = [90_000, 60_000, 30_000]
            .iter()
            .enumerate()
            .map(|(seq, age)| add(&mut conn, &queue, now_ms - age, seq))
            .collect();

        // Before any worker reads the queue, everything waits.
        let before = status(&mut conn, &queue);
        assert_eq!((before.pending, before.processing), (3, 0));
        assert!((90..=92).contains(&before.oldest_pending_secs.unwrap()));

        // A worker takes two and finishes one. The finished entry stays in
        // the stream, but is no longer waiting.
        let _: () = ::redis::cmd("XGROUP")
            .arg("CREATE")
            .arg(&queue)
            .arg(streams::DEFAULT_CONSUMER_GROUP)
            .arg("0")
            .query(&mut conn)
            .unwrap();
        let _: ::redis::Value = ::redis::cmd("XREADGROUP")
            .arg("GROUP")
            .arg(streams::DEFAULT_CONSUMER_GROUP)
            .arg("test")
            .arg("COUNT")
            .arg(2)
            .arg("STREAMS")
            .arg(&queue)
            .arg(">")
            .query(&mut conn)
            .unwrap();
        let _: i64 = ::redis::cmd("XACK")
            .arg(&queue)
            .arg(streams::DEFAULT_CONSUMER_GROUP)
            .arg(&ids[0])
            .query(&mut conn)
            .unwrap();
        let taken = status(&mut conn, &queue);
        assert_eq!((taken.pending, taken.processing), (1, 1));
        assert!((30..=32).contains(&taken.oldest_pending_secs.unwrap()));

        // Moving the group (as the startup window does) leaves Redis unable to
        // report its lag; the waiting entries are counted instead.
        let _: () = ::redis::cmd("XGROUP")
            .arg("SETID")
            .arg(&queue)
            .arg(streams::DEFAULT_CONSUMER_GROUP)
            .arg(&ids[0])
            .query(&mut conn)
            .unwrap();
        let moved = status(&mut conn, &queue);
        assert_eq!((moved.pending, moved.processing), (2, 1));
        assert!((60..=62).contains(&moved.oldest_pending_secs.unwrap()));

        // Nothing waiting: no wait to report.
        let _: () = ::redis::cmd("XGROUP")
            .arg("SETID")
            .arg(&queue)
            .arg(streams::DEFAULT_CONSUMER_GROUP)
            .arg("$")
            .query(&mut conn)
            .unwrap();
        let idle = status(&mut conn, &queue);
        assert_eq!((idle.pending, idle.oldest_pending_secs), (0, None));

        let _: i64 = ::redis::cmd("DEL").arg(&queue).query(&mut conn).unwrap();
    }
}

#[cfg(test)]
mod config_tests {
    use super::*;

    #[test]
    fn sqlite_is_default_queue_type() {
        assert_eq!(QueueType::default(), QueueType::Sqlite);
        assert_eq!(QueueType::from_str("sqlite").unwrap(), QueueType::Sqlite);
        assert!(QueueType::from_str("sqlite3").is_err());
    }

    #[test]
    fn in_project_configuration_defaults_to_sqlite() {
        let conf = get_resolved_conf(Val::map_empty(), true);
        assert_eq!(conf.get_str("type"), "sqlite");
    }

    #[test]
    fn explicit_memory_configuration_is_preserved() {
        let conf = get_resolved_conf(val!({"type": "memory"}), true);
        assert_eq!(conf.get_str("type"), "memory");
    }
}

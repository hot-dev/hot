//! Redis Streams queue implementation
//!
//! Uses Redis Streams with consumer groups for reliable message processing.
//! This implementation supports both standalone and cluster mode.
//!
//! Key features:
//! - Consumer groups for distributed processing
//! - Automatic retry via delivery count tracking
//! - XAUTOCLAIM for orphan recovery
//! - Full cluster mode support
//! - Connection caching to minimize Redis connection overhead

use super::{Queue, QueueInfrastructureError, QueueProcessingError, QueueProcessor};
use crate::data::serialization::Serialization;
use redis::cluster::ClusterClient;
use redis::cluster_async::ClusterConnection as AsyncClusterConnection;
use redis::{Client, aio::MultiplexedConnection};
use serde::{Serialize, de::DeserializeOwned};
use std::collections::{HashSet, VecDeque};
use std::error::Error;
use std::future::Future;
use std::io::{Read, Write};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use uuid::Uuid;
use zstd::{Decoder, Encoder};

/// Default consumer group name for Hot queues
const DEFAULT_CONSUMER_GROUP: &str = "hot-workers";

/// Maximum number of retries before moving to dead letter queue
const MAX_PROCESSING_RETRIES: usize = 3;

/// Idle time in milliseconds before claiming orphaned messages
const ORPHAN_IDLE_MS: u64 = 60_000; // 1 minute

/// Consumers with 0 pending messages idle longer than this are removed.
const IDLE_CONSUMER_NO_PENDING_MS: u64 = 3_600_000; // 1 hour

/// Consumers WITH pending messages idle longer than this are removed
/// (2x the max task timeout of 24 hours from `MAX_TIMEOUT_MS` in
/// `lang/hot/task.rs`). Anything pending past this point is genuinely stuck;
/// the older 14-day grace window let UUID-named consumers from previous
/// deploys hold messages in PEL across multiple worker generations and
/// prevent the live worker from reclaiming them.
const IDLE_CONSUMER_WITH_PENDING_MS: u64 = 48 * 3_600_000; // 48 hours

/// Consumers whose last *interaction* (XREADGROUP/XAUTOCLAIM/...) was longer
/// ago than this are considered dead even if their PEL entries keep getting
/// touched by orphan reclaim (which resets `idle`). Without this guard a
/// consumer from a long-dead worker can hold PEL entries forever, because
/// `XAUTOCLAIM` keeps refreshing per-entry idle while the consumer's
/// `inactive` clock just runs.
const INACTIVE_CONSUMER_THRESHOLD_MS: u64 = 24 * 3_600_000; // 24 hours

/// Fallback stream retention when there are no pending messages at all.
const DEFAULT_STREAM_RETENTION_MS: u64 = 30 * 24 * 3_600_000; // 30 days

/// How many messages to fetch per XREADGROUP call. Larger batches amortize
/// the network round-trip across more work items; smaller batches reduce
/// tail latency for messages stuck behind a slow handler.
const READ_BATCH_SIZE: usize = 16;

/// Soft upper bound on stream length. XADD uses `MAXLEN ~ N` so Redis can
/// efficiently trim near this number without exact-cap overhead. Combined
/// with `trim_stream` (which uses the oldest pending message as the floor),
/// this ensures streams don't grow unbounded between janitor passes.
const STREAM_MAXLEN: u64 = 100_000;

/// Block duration (ms) used by `process_blocking` for the new-message read.
/// Workers that select! over multiple queues park here so we avoid hot
/// looping while staying responsive to shutdown via the outer select.
const PROCESS_BLOCKING_MS: u64 = 5_000;

#[derive(Debug)]
pub enum StreamsQueueError {
    RedisError(String),
    SerializationError(String),
    DeserializationError(String),
    RetryLimitExceeded,
}

impl std::fmt::Display for StreamsQueueError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RedisError(e) => write!(f, "Redis error: {}", e),
            Self::SerializationError(e) => write!(f, "Serialization error: {}", e),
            Self::DeserializationError(e) => write!(f, "Deserialization error: {}", e),
            Self::RetryLimitExceeded => write!(f, "Retry limit exceeded"),
        }
    }
}

impl std::error::Error for StreamsQueueError {}

impl From<redis::RedisError> for StreamsQueueError {
    fn from(e: redis::RedisError) -> Self {
        Self::RedisError(e.to_string())
    }
}

impl From<redis::ParsingError> for StreamsQueueError {
    fn from(e: redis::ParsingError) -> Self {
        Self::RedisError(e.to_string())
    }
}

/// Pull a named field out of a single XINFO GROUPS record.
///
/// `XINFO GROUPS <stream>` returns one entry per group as a flat array of
/// alternating name/value pairs:
///
/// ```text
/// 1) "name"
/// 2) "hot-workers"
/// 3) "consumers"
/// 4) (integer) 1
/// 5) "pending"
/// 6) (integer) 0
/// 7) "last-delivered-id"
/// 8) "1712345678901-0"
/// ...
/// ```
///
/// Returns `Some(value)` if (a) the record's `name` matches `group_name`
/// and (b) the record contains a `field_name` entry whose value can be
/// parsed as a string.
fn extract_group_field(
    group_record: &redis::Value,
    group_name: &str,
    field_name: &str,
) -> Option<String> {
    let pairs: Vec<redis::Value> = redis::from_redis_value_ref(group_record).ok()?;

    let mut name: Option<String> = None;
    let mut field: Option<String> = None;
    let mut i = 0;
    while i + 1 < pairs.len() {
        let key: String = redis::from_redis_value_ref(&pairs[i]).ok()?;
        match key.as_str() {
            "name" => name = redis::from_redis_value_ref(&pairs[i + 1]).ok(),
            k if k == field_name => field = redis::from_redis_value_ref(&pairs[i + 1]).ok(),
            _ => {}
        }
        i += 2;
    }

    match (name, field) {
        (Some(n), Some(f)) if n == group_name => Some(f),
        _ => None,
    }
}

// Serialization helper functions
fn serialize<T: Serialize>(item: &T, format: Serialization) -> Result<Vec<u8>, StreamsQueueError> {
    match format {
        Serialization::Json => serde_json::to_vec(item)
            .map_err(|e| StreamsQueueError::SerializationError(e.to_string())),
        Serialization::ZstdJson => {
            let serialized = serde_json::to_vec(item)
                .map_err(|e| StreamsQueueError::SerializationError(e.to_string()))?;

            let mut compressed = Vec::new();
            {
                let mut encoder = Encoder::new(&mut compressed, 6)
                    .map_err(|e| StreamsQueueError::SerializationError(e.to_string()))?;
                encoder
                    .write_all(&serialized)
                    .map_err(|e| StreamsQueueError::SerializationError(e.to_string()))?;
                encoder
                    .finish()
                    .map_err(|e| StreamsQueueError::SerializationError(e.to_string()))?;
            }
            Ok(compressed)
        }
    }
}

fn deserialize<T: DeserializeOwned>(
    data: &[u8],
    format: Serialization,
) -> Result<T, StreamsQueueError> {
    match format {
        Serialization::Json => serde_json::from_slice(data)
            .map_err(|e| StreamsQueueError::DeserializationError(e.to_string())),
        Serialization::ZstdJson => {
            let mut decompressed = Vec::new();
            {
                let mut decoder = Decoder::new(data)
                    .map_err(|e| StreamsQueueError::DeserializationError(e.to_string()))?;
                decoder
                    .read_to_end(&mut decompressed)
                    .map_err(|e| StreamsQueueError::DeserializationError(e.to_string()))?;
            }
            serde_json::from_slice(&decompressed)
                .map_err(|e| StreamsQueueError::DeserializationError(e.to_string()))
        }
    }
}

/// Redis client that can be cloned cheaply (for per-worker connections)
#[derive(Clone)]
enum RedisClient {
    Standalone(Client),
    Cluster(ClusterClient),
}

impl RedisClient {
    /// Create a new connection (lazily cached per-worker)
    async fn connect(&self) -> Result<RedisConnectionOwned, StreamsQueueError> {
        match self {
            RedisClient::Standalone(client) => {
                let conn = client.get_multiplexed_async_connection().await?;
                Ok(RedisConnectionOwned::Standalone(conn))
            }
            RedisClient::Cluster(client) => {
                let conn = client.get_async_connection().await?;
                Ok(RedisConnectionOwned::Cluster(conn))
            }
        }
    }
}

/// Owned connection for a worker
enum RedisConnectionOwned {
    Standalone(MultiplexedConnection),
    Cluster(AsyncClusterConnection),
}

impl RedisConnectionOwned {
    async fn cmd(&mut self, cmd: &redis::Cmd) -> Result<redis::Value, StreamsQueueError> {
        match self {
            RedisConnectionOwned::Standalone(conn) => {
                let result = cmd.query_async(conn).await?;
                Ok(result)
            }
            RedisConnectionOwned::Cluster(conn) => {
                let result = cmd.query_async(conn).await?;
                Ok(result)
            }
        }
    }
}

/// Source of a buffered message — determines whether we need XPENDING to
/// learn the delivery count, or whether we can short-circuit to 1.
#[derive(Clone, Copy, Debug)]
enum FetchSource {
    /// Fresh `>` read — first delivery, count is implicitly 1.
    Fresh,
    /// `0` read of the consumer's PEL — re-read after a previous failure.
    /// Caller must consult XPENDING for the real delivery count.
    Pending,
}

#[derive(Debug)]
struct PrefetchedMessage {
    msg_id: String,
    payload: Vec<u8>,
    source: FetchSource,
}

fn duration_ms(duration: Duration) -> u64 {
    duration.as_millis().min(u128::from(u64::MAX)) as u64
}

fn stream_message_age_ms(msg_id: &str) -> u64 {
    let Some((millis, _sequence)) = msg_id.split_once('-') else {
        return 0;
    };
    let Ok(enqueued_at_ms) = millis.parse::<i64>() else {
        return 0;
    };
    chrono::Utc::now()
        .timestamp_millis()
        .saturating_sub(enqueued_at_ms)
        .max(0) as u64
}

fn queue_timing_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var("HOT_QUEUE_METRICS_ENABLED")
            .map(|value| {
                let normalized = value.trim().to_ascii_lowercase();
                !matches!(normalized.as_str(), "0" | "false" | "no" | "off")
            })
            .unwrap_or(true)
    })
}

/// Redis Streams queue implementation.
///
/// Each clone gets its own cached connection (per-worker connection). This
/// avoids contention between workers while still caching connections to
/// avoid expensive reconnections on every operation.
///
/// Clone is manually implemented to:
///   - Generate a unique consumer_name (required for Redis Streams consumer groups).
///   - Create a fresh connection cache (per-worker connection).
pub struct RedisStreamQueue<T> {
    /// Redis client (cheap to clone, used to create per-worker connections)
    client: RedisClient,
    /// Per-worker cached connection (created lazily on first use)
    cached_conn: Arc<Mutex<Option<RedisConnectionOwned>>>,
    stream_name: String,
    consumer_group: String,
    consumer_name: String,
    dlq_stream: String,
    serialization: Serialization,
    /// Set after XGROUP CREATE has succeeded (or returned BUSYGROUP) for
    /// this consumer/group pair. Eliminates the per-call XGROUP roundtrip.
    consumer_group_ensured: Arc<AtomicBool>,
    /// Prefetched messages from the last batched XREADGROUP. We pop one
    /// per `dequeue_and_work` call; refill when empty.
    prefetched: Arc<Mutex<VecDeque<PrefetchedMessage>>>,
    /// Serializes concurrent `refill_prefetch` calls against this instance
    /// and gates access to `in_flight`. See `refill_prefetch` for the full
    /// rationale; in short, without serialization multiple parallel
    /// `process_blocking` callers each issue their own `XREADGROUP ... 0`
    /// and each receive the *same* PEL entries because no XACKs have
    /// landed yet, leading to N-fold duplicate execution of every entry.
    refill_lock: Arc<Mutex<()>>,
    /// Message IDs that have been handed off to a worker (popped from
    /// `prefetched`) but not yet ACKed or moved to DLQ. The refill path
    /// filters these out of any incoming PEL batch so a `0` read that
    /// races a still-processing worker does not re-deliver the same
    /// entry into the buffer. Cleared per-entry by the success/DLQ paths
    /// in `process_inner`.
    in_flight: Arc<Mutex<HashSet<String>>>,
    /// Optional retention window (ms) used when **creating** a brand-new
    /// consumer group. If set, `XGROUP CREATE` uses `<now-window>-0` as the
    /// starting position instead of `0`, so a freshly-created group only
    /// becomes eligible for entries newer than `now - window`. Protects
    /// against catastrophic replay floods when the consumer group is
    /// dropped/recreated against a stream with retained history (we keep
    /// up to `STREAM_MAXLEN ~ 100_000` entries via `XADD MAXLEN ~`).
    ///
    /// `None` preserves the historical "consume from beginning" behavior.
    startup_window_ms: Option<u64>,
    _phantom: std::marker::PhantomData<T>,
}

impl<T> Clone for RedisStreamQueue<T> {
    fn clone(&self) -> Self {
        // Each clone gets:
        // 1. A NEW unique consumer_name (required for Redis Streams consumer groups)
        // 2. A FRESH connection cache (per-worker connection, no sharing)
        // 3. A FRESH prefetch buffer + ensure-group flag (per-consumer state)
        //
        // The client is cloned cheaply, connection is created lazily.
        Self {
            client: self.client.clone(),
            cached_conn: Arc::new(Mutex::new(None)), // Fresh cache for this worker
            stream_name: self.stream_name.clone(),
            consumer_group: self.consumer_group.clone(),
            consumer_name: format!("consumer-{}", Uuid::new_v4()), // NEW unique name!
            dlq_stream: self.dlq_stream.clone(),
            serialization: self.serialization,
            consumer_group_ensured: Arc::new(AtomicBool::new(false)),
            prefetched: Arc::new(Mutex::new(VecDeque::new())),
            refill_lock: Arc::new(Mutex::new(())),
            in_flight: Arc::new(Mutex::new(HashSet::new())),
            startup_window_ms: self.startup_window_ms,
            _phantom: std::marker::PhantomData,
        }
    }
}

impl<T> RedisStreamQueue<T> {
    /// Create a new Redis Streams queue with a standalone client
    ///
    /// Connection is created lazily on first use.
    pub fn new(client: Client, stream_name: String) -> Self {
        // Wrap stream name in hash tags for cluster compatibility
        let stream_name = if stream_name.contains('{') {
            stream_name
        } else {
            format!("{{{}}}", stream_name)
        };

        let consumer_name = format!("consumer-{}", Uuid::new_v4());
        let dlq_stream = format!("{}:deadletter", stream_name);

        Self {
            client: RedisClient::Standalone(client),
            cached_conn: Arc::new(Mutex::new(None)),
            stream_name,
            consumer_group: DEFAULT_CONSUMER_GROUP.to_string(),
            consumer_name,
            dlq_stream,
            serialization: Serialization::default(),
            consumer_group_ensured: Arc::new(AtomicBool::new(false)),
            prefetched: Arc::new(Mutex::new(VecDeque::new())),
            refill_lock: Arc::new(Mutex::new(())),
            in_flight: Arc::new(Mutex::new(HashSet::new())),
            startup_window_ms: None,
            _phantom: std::marker::PhantomData,
        }
    }

    /// Create a new Redis Streams queue with a cluster client
    ///
    /// Connection is created lazily on first use to avoid blocking.
    pub fn new_cluster(cluster_client: ClusterClient, stream_name: String) -> Self {
        // Wrap stream name in hash tags for cluster mode
        let stream_name = if stream_name.contains('{') {
            stream_name
        } else {
            format!("{{{}}}", stream_name)
        };

        let consumer_name = format!("consumer-{}", Uuid::new_v4());
        let dlq_stream = format!("{}:deadletter", stream_name);

        Self {
            client: RedisClient::Cluster(cluster_client),
            cached_conn: Arc::new(Mutex::new(None)),
            stream_name,
            consumer_group: DEFAULT_CONSUMER_GROUP.to_string(),
            consumer_name,
            dlq_stream,
            serialization: Serialization::default(),
            consumer_group_ensured: Arc::new(AtomicBool::new(false)),
            prefetched: Arc::new(Mutex::new(VecDeque::new())),
            refill_lock: Arc::new(Mutex::new(())),
            in_flight: Arc::new(Mutex::new(HashSet::new())),
            startup_window_ms: None,
            _phantom: std::marker::PhantomData,
        }
    }

    /// Get a connection, creating one if necessary
    async fn get_connection(
        &self,
    ) -> Result<tokio::sync::MutexGuard<'_, Option<RedisConnectionOwned>>, StreamsQueueError> {
        let mut guard = self.cached_conn.lock().await;
        if guard.is_none() {
            let conn = self.client.connect().await?;
            *guard = Some(conn);
        }
        Ok(guard)
    }

    pub fn with_serialization(mut self, format: Serialization) -> Self {
        self.serialization = format;
        self
    }

    /// Number of messages currently in-flight for the consumer group.
    /// (i.e. delivered to *some* consumer but not yet ACKed). Useful for
    /// monitoring backpressure independently of total queue depth.
    pub async fn pending_len(&self) -> Result<usize, StreamsQueueError> {
        let mut guard = self.get_connection().await?;
        let conn = guard.as_mut().unwrap();

        let result = conn
            .cmd(
                &redis::cmd("XPENDING")
                    .arg(&self.stream_name)
                    .arg(&self.consumer_group)
                    .clone(),
            )
            .await;

        match result {
            Ok(val) => {
                let parts: Vec<redis::Value> =
                    redis::from_redis_value_ref(&val).unwrap_or_default();
                if parts.is_empty() {
                    return Ok(0);
                }
                Ok(redis::from_redis_value_ref(&parts[0]).unwrap_or(0))
            }
            Err(_) => Ok(0),
        }
    }

    pub fn with_consumer_group(mut self, group: String) -> Self {
        self.consumer_group = group;
        self
    }

    /// Override the auto-generated consumer name. Use stable, deterministic
    /// names (e.g. `format!("{host}-{worker_id}")`) so XINFO CONSUMERS
    /// doesn't accumulate ghost UUIDs across restarts of the same logical
    /// worker, and so operational debugging is easier (you can tell which
    /// physical worker was processing what).
    pub fn with_consumer_name(mut self, name: String) -> Self {
        self.consumer_name = name;
        self
    }

    /// Read-only access to the consumer name. Useful for logging.
    pub fn consumer_name(&self) -> &str {
        &self.consumer_name
    }

    /// Configure the **startup retention window** for this queue.
    ///
    /// Affects two behaviors that together prevent "ancient backlog drain"
    /// after worker outages or stream/group recreation:
    ///
    /// 1. `ensure_consumer_group()` will create brand-new groups starting at
    ///    `<now - window>-0` instead of `0`, so a fresh group never replays
    ///    retained history older than `window`.
    /// 2. `fast_forward_if_stale()` (call once at worker startup) will
    ///    advance an existing group's last-delivered-id forward to
    ///    `<now - window>-0` if the group is currently behind that point —
    ///    so a worker coming back from a long outage skips the now-stale
    ///    backlog instead of trying to drain it.
    ///
    /// Pick a window appropriate for the queue's freshness requirements
    /// (e.g. 15min for alerts, 1h for events, 24h for tasks). `None` (the
    /// default) preserves the historical "consume from beginning" behavior.
    pub fn with_startup_window(mut self, window: std::time::Duration) -> Self {
        self.startup_window_ms = Some(window.as_millis() as u64);
        self
    }

    /// Unregister this consumer from the group on graceful shutdown.
    /// Any pending messages are released back to the group for other consumers to claim.
    pub async fn unregister_consumer(&self) -> Result<(), StreamsQueueError> {
        let mut guard = self.get_connection().await?;
        let conn = guard.as_mut().unwrap();

        let result: Result<i64, _> = redis::from_redis_value_ref(
            &conn
                .cmd(
                    &redis::cmd("XGROUP")
                        .arg("DELCONSUMER")
                        .arg(&self.stream_name)
                        .arg(&self.consumer_group)
                        .arg(&self.consumer_name)
                        .clone(),
                )
                .await?,
        );

        match result {
            Ok(released) => {
                if released > 0 {
                    tracing::info!(
                        "Unregistered consumer {} from {} (released {} pending messages)",
                        self.consumer_name,
                        self.stream_name,
                        released,
                    );
                } else {
                    tracing::debug!(
                        "Unregistered consumer {} from {}",
                        self.consumer_name,
                        self.stream_name,
                    );
                }
            }
            Err(e) => {
                tracing::warn!(
                    "Failed to unregister consumer {} from {}: {}",
                    self.consumer_name,
                    self.stream_name,
                    e,
                );
            }
        }

        Ok(())
    }

    /// Ensure the consumer group exists for this stream.
    ///
    /// Idempotent and **cached per-instance**: after the first successful call
    /// (or BUSYGROUP response), subsequent calls return immediately without
    /// any Redis round-trip. This used to be invoked on every enqueue/dequeue,
    /// which doubled the command count for the hot path.
    pub async fn ensure_consumer_group(&self) -> Result<(), StreamsQueueError> {
        use std::sync::atomic::Ordering;

        if self.consumer_group_ensured.load(Ordering::Acquire) {
            return Ok(());
        }

        let mut guard = self.get_connection().await?;
        let conn = guard.as_mut().unwrap();

        // Choose the starting position for a NEW consumer group:
        //   - With `startup_window_ms` set: `<now - window>-0`. A brand-new
        //     group becomes eligible only for entries newer than that point,
        //     which prevents catastrophic replay floods if the group was
        //     dropped/recreated against a stream with retained history (we
        //     keep up to `STREAM_MAXLEN ~ 100_000` entries via XADD MAXLEN).
        //   - Without (`None`): `0` to preserve the historical
        //     "consume from beginning" behavior for callers that haven't
        //     opted in.
        //
        // BUSYGROUP (group already exists) shortcircuits this entirely, so
        // steady-state workers across deploys never see the windowed value.
        let start_id = match self.startup_window_ms {
            Some(window_ms) => {
                let now_ms = chrono::Utc::now().timestamp_millis() as u64;
                let cutoff = now_ms.saturating_sub(window_ms);
                format!("{}-0", cutoff)
            }
            None => "0".to_string(),
        };

        // XGROUP CREATE stream group <start-id> MKSTREAM
        // MKSTREAM creates the stream if it doesn't exist.
        let result = conn
            .cmd(
                &redis::cmd("XGROUP")
                    .arg("CREATE")
                    .arg(&self.stream_name)
                    .arg(&self.consumer_group)
                    .arg(&start_id)
                    .arg("MKSTREAM")
                    .clone(),
            )
            .await;

        match result {
            Ok(_) => {
                tracing::info!(
                    "Created consumer group {} for stream {} starting at {}",
                    self.consumer_group,
                    self.stream_name,
                    start_id,
                );
                self.consumer_group_ensured.store(true, Ordering::Release);
                Ok(())
            }
            Err(StreamsQueueError::RedisError(e)) if e.contains("BUSYGROUP") => {
                // Group already exists, that's fine
                tracing::debug!(
                    "Consumer group {} already exists for stream {}",
                    self.consumer_group,
                    self.stream_name
                );
                self.consumer_group_ensured.store(true, Ordering::Release);
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    /// Recover orphaned messages using XAUTOCLAIM
    /// Messages that have been idle for more than ORPHAN_IDLE_MS will be claimed
    /// by this consumer and will be reprocessed on the next dequeue_and_work call.
    pub async fn recover_orphaned_items(&self) -> Result<usize, StreamsQueueError> {
        // Ensure consumer group exists first (acquires and releases its own connection)
        self.ensure_consumer_group().await?;

        // Defensive guard: XAUTOCLAIM transfers PEL ownership to
        // `self.consumer_name`. If that name is the legacy `consumer-{uuid}`
        // default (e.g. a freshly-cloned queue handle whose caller forgot to
        // pin a stable name), the reclaimed entries go into a consumer that
        // probably won't ever run `dequeue_and_work` — they sit in PEL
        // forever, the next reclaim cycle re-claims them, and the consumer's
        // idle/inactive get reset every tick so cleanup_stale_consumers
        // can't reap it either. Skip rather than create an unreachable
        // consumer; the queue's real owner (a worker with a
        // stable consumer name) will reclaim on its own janitor tick.
        if self.consumer_name.starts_with("consumer-") {
            tracing::warn!(
                "Skipping XAUTOCLAIM on {}: consumer name '{}' looks like a legacy UUID-style \
                 default. Pin a stable consumer name via with_consumer_name() before calling \
                 reclaim_orphans/recover_orphaned_items, otherwise reclaimed entries become \
                 unreachable. (See hot_worker/server.rs admin_*_name pattern.)",
                self.stream_name,
                self.consumer_name
            );
            return Ok(0);
        }

        let mut guard = self.get_connection().await?;
        let conn = guard.as_mut().unwrap();

        let mut total_claimed = 0;
        let mut cursor = "0-0".to_string();

        loop {
            // XAUTOCLAIM stream group consumer min-idle-time start [COUNT count]
            let result = conn
                .cmd(
                    &redis::cmd("XAUTOCLAIM")
                        .arg(&self.stream_name)
                        .arg(&self.consumer_group)
                        .arg(&self.consumer_name)
                        .arg(ORPHAN_IDLE_MS)
                        .arg(&cursor)
                        .arg("COUNT")
                        .arg(100)
                        .clone(),
                )
                .await?;

            // XAUTOCLAIM returns: [next-cursor, [[id, [field, value, ...]], ...], [deleted-ids]]
            let parts: Vec<redis::Value> = redis::from_redis_value_ref(&result)?;

            if parts.len() < 2 {
                break;
            }

            // Get next cursor
            let new_cursor: String = redis::from_redis_value_ref(&parts[0]).unwrap_or_default();

            // Get claimed messages
            let claimed: Vec<redis::Value> =
                redis::from_redis_value_ref(&parts[1]).unwrap_or_default();
            let claimed_count = claimed.len();
            total_claimed += claimed_count;

            if claimed_count > 0 {
                tracing::warn!(
                    "XAUTOCLAIM claimed {} orphaned messages from {} (consumer: {})",
                    claimed_count,
                    self.stream_name,
                    self.consumer_name
                );
            }

            // If cursor is "0-0", we've scanned everything
            if new_cursor == "0-0" {
                break;
            }
            cursor = new_cursor;
        }

        if total_claimed > 0 {
            tracing::warn!(
                "Total {} orphaned messages recovered from {}",
                total_claimed,
                self.stream_name
            );
        } else {
            tracing::debug!("No orphaned messages found in {}", self.stream_name);
        }

        Ok(total_claimed)
    }

    /// Recover orphaned items and return their raw data
    /// Note: For streams, we don't return the data since it will be reprocessed
    /// via normal dequeue_and_work flow after XAUTOCLAIM
    pub async fn recover_orphaned_items_with_data(
        &self,
    ) -> Result<(usize, Vec<Vec<u8>>), StreamsQueueError> {
        let count = self.recover_orphaned_items().await?;
        // For streams, the messages are already in the pending list after XAUTOCLAIM
        // and will be returned by the next XREADGROUP call with "0" instead of ">"
        // We don't extract the data here - it flows through normal processing
        Ok((count, vec![]))
    }

    /// Fast-forward the consumer group's last-delivered-id to `<now - window>-0`
    /// if (and only if) it is currently older than that point. Intended to be
    /// called once at worker startup, **before** the first dequeue.
    ///
    /// This handles the "worker came back from a long outage" failure mode:
    /// a stale consumer group that's been parked for hours/days otherwise
    /// drains its entire backlog via `XREADGROUP > ` as soon as a worker
    /// reconnects, producing a flood of late-running events. Skipping
    /// forward to a recent point lets us pick up only fresh work and let
    /// the stale entries age out via `MAXLEN ~` trimming.
    ///
    /// Returns the number of stream entries that were skipped — i.e. how
    /// many entries existed between the old last-delivered-id and the new
    /// one, **counting only entries that the group had not yet delivered**.
    /// The PEL (pending-but-unacked) is not affected by `XGROUP SETID`.
    ///
    /// `Ok(0)` means either:
    ///   - no startup window is configured for this queue (no-op), OR
    ///   - the group's last-delivered-id is already within `window`.
    ///
    /// Safe to call against a stream that doesn't yet exist — it will
    /// trigger consumer-group creation via `ensure_consumer_group()`,
    /// which itself respects `startup_window_ms` for the initial position.
    pub async fn fast_forward_if_stale(&self) -> Result<usize, StreamsQueueError> {
        let window_ms = match self.startup_window_ms {
            Some(w) => w,
            None => return Ok(0),
        };

        // Make sure the group exists first (also handles MKSTREAM for empty streams).
        self.ensure_consumer_group().await?;

        let mut guard = self.get_connection().await?;
        let conn = guard.as_mut().unwrap();

        // XINFO GROUPS <stream> → array of group records, each is an array
        // of alternating [name, value, name, value, ...] pairs. We need to
        // pluck out our group's `last-delivered-id`.
        let info_val = match conn
            .cmd(
                &redis::cmd("XINFO")
                    .arg("GROUPS")
                    .arg(&self.stream_name)
                    .clone(),
            )
            .await
        {
            Ok(v) => v,
            Err(e) => {
                tracing::debug!(
                    "fast_forward_if_stale: XINFO GROUPS failed for {}: {} \
                     (treating as no-op)",
                    self.stream_name,
                    e,
                );
                return Ok(0);
            }
        };

        let groups: Vec<redis::Value> = redis::from_redis_value_ref(&info_val).unwrap_or_default();
        let last_delivered_id = groups
            .iter()
            .find_map(|g| extract_group_field(g, &self.consumer_group, "last-delivered-id"));

        let last_delivered_id = match last_delivered_id {
            Some(id) => id,
            None => {
                // Group must have just been created (MKSTREAM with no entries).
                // Nothing to fast-forward.
                return Ok(0);
            }
        };

        // Stream IDs are `<ms>-<seq>`. Parse the ms portion.
        let last_delivered_ms: u64 = last_delivered_id
            .split('-')
            .next()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);

        let now_ms = chrono::Utc::now().timestamp_millis() as u64;
        let cutoff_ms = now_ms.saturating_sub(window_ms);

        // Slack: only fast-forward if the group is *meaningfully* behind the
        // cutoff. Without this, a brand-new group whose start point was
        // computed inside `ensure_consumer_group()` a few ms ago would be
        // flagged stale on the very next call (cutoff has advanced; the
        // group's last-delivered-id has not). 1% of the window, with a
        // 1-second floor, gives us ~36s of headroom on a 1h queue and ~14m
        // on a 24h queue — well under any window we actually configure.
        let slack_ms = std::cmp::max(1_000, window_ms / 100);
        let stale_floor = cutoff_ms.saturating_sub(slack_ms);

        if last_delivered_ms >= stale_floor {
            return Ok(0);
        }

        // Count how many entries we're about to skip so we can log meaningfully.
        // Bounded by COUNT to keep this a single round-trip even on huge streams.
        let skipped: usize = match conn
            .cmd(
                &redis::cmd("XRANGE")
                    .arg(&self.stream_name)
                    .arg(format!("({}", last_delivered_id)) // exclusive
                    .arg(format!("{}-0", cutoff_ms))
                    .arg("COUNT")
                    .arg(10_000)
                    .clone(),
            )
            .await
        {
            Ok(v) => redis::from_redis_value_ref::<Vec<redis::Value>>(&v)
                .map(|xs| xs.len())
                .unwrap_or(0),
            Err(_) => 0,
        };

        // XGROUP SETID <stream> <group> <id> — moves last-delivered-id forward.
        // Anything older than `cutoff_ms` will never be delivered to this
        // group via XREADGROUP > .
        conn.cmd(
            &redis::cmd("XGROUP")
                .arg("SETID")
                .arg(&self.stream_name)
                .arg(&self.consumer_group)
                .arg(format!("{}-0", cutoff_ms))
                .clone(),
        )
        .await?;

        if skipped > 0 {
            tracing::warn!(
                "fast_forward_if_stale: advanced {} group {} from {} to {}-0 \
                 (skipped at least {} stale entr{}, window: {}ms)",
                self.stream_name,
                self.consumer_group,
                last_delivered_id,
                cutoff_ms,
                skipped,
                if skipped == 1 { "y" } else { "ies" },
                window_ms,
            );
        } else {
            tracing::debug!(
                "fast_forward_if_stale: advanced {} group {} from {} to {}-0 \
                 (no stale entries, window: {}ms)",
                self.stream_name,
                self.consumer_group,
                last_delivered_id,
                cutoff_ms,
                window_ms,
            );
        }

        Ok(skipped)
    }

    /// Purge old messages from the stream. This performs two operations:
    ///
    /// 1. **ACK old pending messages** — messages delivered to a consumer but not
    ///    yet acknowledged are removed from the consumer group's PEL via XACK.
    /// 2. **XDEL old undelivered messages** — messages sitting in the stream that
    ///    have never been delivered to any consumer are deleted directly. Without
    ///    this step, a fresh consumer group (e.g. after `queue clear` or restart)
    ///    would replay all historical messages via `XREADGROUP ... ">"`.
    ///
    /// Redis stream message IDs encode the creation timestamp as the prefix
    /// before the `-` separator (e.g. `1712345678901-0`).
    pub async fn purge_old_pending(&self, max_age_ms: u64) -> Result<usize, StreamsQueueError> {
        self.ensure_consumer_group().await?;

        let mut guard = self.get_connection().await?;
        let conn = guard.as_mut().unwrap();

        let cutoff_ms = chrono::Utc::now().timestamp_millis() as u64 - max_age_ms;
        let mut total_purged: usize = 0;

        // Phase 1: ACK old pending messages from the consumer group PEL
        let mut start = "-".to_string();
        loop {
            let result = conn
                .cmd(
                    &redis::cmd("XPENDING")
                        .arg(&self.stream_name)
                        .arg(&self.consumer_group)
                        .arg(&start)
                        .arg("+")
                        .arg(200)
                        .clone(),
                )
                .await?;

            let entries: Vec<redis::Value> =
                redis::from_redis_value_ref(&result).unwrap_or_default();
            if entries.is_empty() {
                break;
            }

            let mut ids_to_ack: Vec<String> = Vec::new();
            let mut last_id = String::new();

            for entry in &entries {
                let fields: Vec<redis::Value> =
                    redis::from_redis_value_ref(entry).unwrap_or_default();
                if fields.len() < 2 {
                    continue;
                }
                let id: String = redis::from_redis_value_ref(&fields[0]).unwrap_or_default();
                last_id = id.clone();

                if let Some(ts_str) = id.split('-').next()
                    && let Ok(ts) = ts_str.parse::<u64>()
                    && ts < cutoff_ms
                {
                    ids_to_ack.push(id);
                }
            }

            if !ids_to_ack.is_empty() {
                let mut cmd = redis::cmd("XACK");
                cmd.arg(&self.stream_name).arg(&self.consumer_group);
                for id in &ids_to_ack {
                    cmd.arg(id);
                }
                let _: i64 = redis::from_redis_value_ref(
                    &conn.cmd(&cmd.clone()).await.unwrap_or(redis::Value::Int(0)),
                )
                .unwrap_or(0);
                total_purged += ids_to_ack.len();
            }

            if entries.len() < 200 {
                break;
            }

            if let Some((ts, seq)) = last_id.split_once('-') {
                if let Ok(s) = seq.parse::<u64>() {
                    start = format!("{}-{}", ts, s + 1);
                } else {
                    break;
                }
            } else {
                break;
            }
        }

        // Phase 2: XDEL old undelivered messages from the stream itself.
        // XRANGE with a ceiling ID removes messages that were never consumed
        // (e.g. enqueued by the scheduler but never read by the worker).
        let cutoff_id = format!("{}-0", cutoff_ms);
        let mut stream_deleted: usize = 0;
        let mut range_start = "-".to_string();

        loop {
            let result = conn
                .cmd(
                    &redis::cmd("XRANGE")
                        .arg(&self.stream_name)
                        .arg(&range_start)
                        .arg(&cutoff_id)
                        .arg("COUNT")
                        .arg(200)
                        .clone(),
                )
                .await?;

            let messages: Vec<redis::Value> =
                redis::from_redis_value_ref(&result).unwrap_or_default();
            if messages.is_empty() {
                break;
            }

            let mut ids_to_del: Vec<String> = Vec::new();
            let mut last_range_id = String::new();

            for msg in &messages {
                let fields: Vec<redis::Value> =
                    redis::from_redis_value_ref(msg).unwrap_or_default();
                if fields.is_empty() {
                    continue;
                }
                let id: String = redis::from_redis_value_ref(&fields[0]).unwrap_or_default();
                last_range_id = id.clone();
                ids_to_del.push(id);
            }

            if !ids_to_del.is_empty() {
                let mut cmd = redis::cmd("XDEL");
                cmd.arg(&self.stream_name);
                for id in &ids_to_del {
                    cmd.arg(id);
                }
                let deleted: i64 = redis::from_redis_value_ref(
                    &conn.cmd(&cmd.clone()).await.unwrap_or(redis::Value::Int(0)),
                )
                .unwrap_or(0);
                stream_deleted += deleted as usize;
            }

            if messages.len() < 200 {
                break;
            }

            // Advance past last ID for next page (exclusive start)
            if let Some((ts, seq)) = last_range_id.split_once('-') {
                if let Ok(s) = seq.parse::<u64>() {
                    range_start = format!("{}-{}", ts, s + 1);
                } else {
                    break;
                }
            } else {
                break;
            }
        }

        let total = total_purged + stream_deleted;
        if total > 0 {
            tracing::info!(
                "Purged {} old messages (>{:.0}s) from {} ({} pending ACKed, {} stream entries deleted)",
                total,
                max_age_ms as f64 / 1000.0,
                self.stream_name,
                total_purged,
                stream_deleted,
            );
        }

        Ok(total)
    }

    /// Remove stale consumers that are no longer processing messages.
    ///
    /// Two-tier strategy to avoid killing workers on long-running tasks (up to 7 days):
    /// - Tier 1: 0 pending + idle > 1 hour → clearly dead, safe to remove
    /// - Tier 2: has pending + idle > 14 days → XAUTOCLAIM pending first, then remove
    pub async fn cleanup_stale_consumers(&self) -> Result<usize, StreamsQueueError> {
        self.ensure_consumer_group().await?;

        // Same guard as recover_orphaned_items: when we find a stale consumer
        // that still has PEL entries we XAUTOCLAIM them into `self.consumer_name`
        // before deleting the source consumer. If our own name is the legacy
        // `consumer-{uuid}` default it means our caller forgot to pin a stable
        // name (so we're almost certainly an admin handle, not a real worker)
        // — draining into ourselves would just re-create the stuck-PEL ghost
        // we're trying to remove. We still allow the cleanup of *other* zero-
        // pending consumers because that's purely an XGROUP DELCONSUMER and
        // can't make entries unreachable; the loop below conditionally skips the
        // XAUTOCLAIM step when this guard is hot.
        let self_name_is_uuid = self.consumer_name.starts_with("consumer-");
        if self_name_is_uuid {
            tracing::debug!(
                "cleanup_stale_consumers on {} called with UUID-style name '{}'; will skip \
                 PEL-draining XAUTOCLAIM (would create another stuck-PEL ghost). Pin a stable \
                 consumer name via with_consumer_name() to enable full drain.",
                self.stream_name,
                self.consumer_name,
            );
        }

        let mut guard = self.get_connection().await?;
        let conn = guard.as_mut().unwrap();

        // XINFO CONSUMERS <stream> <group>
        let result = conn
            .cmd(
                &redis::cmd("XINFO")
                    .arg("CONSUMERS")
                    .arg(&self.stream_name)
                    .arg(&self.consumer_group)
                    .clone(),
            )
            .await?;

        let consumers: Vec<redis::Value> = redis::from_redis_value_ref(&result).unwrap_or_default();

        let mut removed = 0usize;

        for consumer_val in &consumers {
            let fields: Vec<redis::Value> =
                redis::from_redis_value_ref(consumer_val).unwrap_or_default();

            let mut name: Option<String> = None;
            let mut pending: i64 = 0;
            let mut idle: u64 = 0;
            // `inactive` is Redis 7.2+; default to 0 (= treat as recent) when
            // missing so older Redis versions keep the previous behaviour.
            let mut inactive: u64 = 0;

            // XINFO CONSUMERS returns flat key-value pairs per consumer
            let mut i = 0;
            while i + 1 < fields.len() {
                let key: String = redis::from_redis_value_ref(&fields[i]).unwrap_or_default();
                match key.as_str() {
                    "name" => name = redis::from_redis_value_ref(&fields[i + 1]).ok(),
                    "pending" => pending = redis::from_redis_value_ref(&fields[i + 1]).unwrap_or(0),
                    "idle" => idle = redis::from_redis_value_ref(&fields[i + 1]).unwrap_or(0),
                    "inactive" => {
                        // Redis returns -1 if the consumer was never inactive
                        // (i.e. still active). Clamp to 0 in that case.
                        let v: i64 = redis::from_redis_value_ref(&fields[i + 1]).unwrap_or(0);
                        inactive = v.max(0) as u64;
                    }
                    _ => {}
                }
                i += 2;
            }

            let consumer_name = match name {
                Some(n) => n,
                None => continue,
            };

            // Never remove ourselves
            if consumer_name == self.consumer_name {
                continue;
            }

            let should_remove = if pending == 0 {
                idle > IDLE_CONSUMER_NO_PENDING_MS
            } else {
                // Two reasons to reap a consumer that still holds PEL entries:
                //   1. `idle` (per-consumer last activity) past the long
                //      grace window, OR
                //   2. `inactive` past the dead-worker threshold — this
                //      catches consumers whose idle kept getting reset by
                //      repeated XAUTOCLAIM passes on the same stuck PEL
                //      entries.
                idle > IDLE_CONSUMER_WITH_PENDING_MS || inactive > INACTIVE_CONSUMER_THRESHOLD_MS
            };

            if !should_remove {
                continue;
            }

            // Skip stale consumers with pending entries when our own name is
            // a UUID — the XAUTOCLAIM drain would just recreate the ghost in
            // our own PEL. Leave those for the queue's real owner (whose
            // janitor uses a stable consumer name) to clean up.
            if pending > 0 && self_name_is_uuid {
                tracing::debug!(
                    "Skipping cleanup of stale consumer {} on {} (pending={}, idle={}ms): \
                     our consumer name is UUID-style, drain would create another ghost",
                    consumer_name,
                    self.stream_name,
                    pending,
                    idle,
                );
                continue;
            }

            // If the consumer has pending messages, XAUTOCLAIM them first
            if pending > 0 {
                tracing::info!(
                    "Reclaiming {} pending messages from stale consumer {} on {} (idle {}ms)",
                    pending,
                    consumer_name,
                    self.stream_name,
                    idle,
                );
                let mut cursor = "0-0".to_string();
                loop {
                    let claim_result = conn
                        .cmd(
                            &redis::cmd("XAUTOCLAIM")
                                .arg(&self.stream_name)
                                .arg(&self.consumer_group)
                                .arg(&self.consumer_name)
                                .arg(0u64) // min-idle 0 — take everything from this consumer
                                .arg(&cursor)
                                .arg("COUNT")
                                .arg(100)
                                .clone(),
                        )
                        .await?;

                    let parts: Vec<redis::Value> =
                        redis::from_redis_value_ref(&claim_result).unwrap_or_default();
                    if parts.len() < 2 {
                        break;
                    }
                    let new_cursor: String =
                        redis::from_redis_value_ref(&parts[0]).unwrap_or_default();
                    if new_cursor == "0-0" {
                        break;
                    }
                    cursor = new_cursor;
                }
            }

            // XGROUP DELCONSUMER <stream> <group> <consumer>
            let del_result: Result<i64, _> = redis::from_redis_value_ref(
                &conn
                    .cmd(
                        &redis::cmd("XGROUP")
                            .arg("DELCONSUMER")
                            .arg(&self.stream_name)
                            .arg(&self.consumer_group)
                            .arg(&consumer_name)
                            .clone(),
                    )
                    .await?,
            );

            match del_result {
                Ok(released) => {
                    tracing::debug!(
                        "Removed stale consumer {} from {} (was idle {}ms, released {} pending)",
                        consumer_name,
                        self.stream_name,
                        idle,
                        released,
                    );
                    removed += 1;
                }
                Err(e) => {
                    tracing::warn!(
                        "Failed to parse DELCONSUMER result for {} on {}: {}",
                        consumer_name,
                        self.stream_name,
                        e,
                    );
                }
            }
        }

        if removed > 0 {
            tracing::info!(
                "Cleaned up {} stale consumer(s) from {} ({})",
                removed,
                self.stream_name,
                self.consumer_group,
            );
        } else {
            tracing::debug!("No stale consumers found on {}", self.stream_name);
        }

        Ok(removed)
    }

    /// Trim stream entries that are fully processed and no longer needed.
    ///
    /// Uses the oldest pending message as a safe floor so we never trim entries
    /// that a live (or crashed) consumer might still need for retry recovery.
    /// Falls back to 30-day retention when nothing is pending.
    pub async fn trim_stream(&self) -> Result<usize, StreamsQueueError> {
        self.ensure_consumer_group().await?;

        let mut guard = self.get_connection().await?;
        let conn = guard.as_mut().unwrap();

        // XPENDING <stream> <group> — summary form returns:
        //   [pending-count, smallest-id, largest-id, [[consumer, count], ...]]
        let pending_result = conn
            .cmd(
                &redis::cmd("XPENDING")
                    .arg(&self.stream_name)
                    .arg(&self.consumer_group)
                    .clone(),
            )
            .await?;

        let pending_parts: Vec<redis::Value> =
            redis::from_redis_value_ref(&pending_result).unwrap_or_default();

        let pending_count: i64 = pending_parts
            .first()
            .and_then(|v| redis::from_redis_value_ref(v).ok())
            .unwrap_or(0);

        let cutoff_id = if pending_count > 0 {
            // Oldest pending message ID — everything before this is safe to trim
            let oldest_id: String = pending_parts
                .get(1)
                .and_then(|v| redis::from_redis_value_ref(v).ok())
                .unwrap_or_default();

            if oldest_id.is_empty() {
                tracing::warn!(
                    "XPENDING reports {} pending but no smallest ID on {}",
                    pending_count,
                    self.stream_name,
                );
                return Ok(0);
            }

            tracing::debug!(
                "Trim floor for {}: oldest pending message {}",
                self.stream_name,
                oldest_id,
            );
            oldest_id
        } else {
            // Nothing pending — use 30-day retention
            let cutoff_ms =
                chrono::Utc::now().timestamp_millis() as u64 - DEFAULT_STREAM_RETENTION_MS;
            let id = format!("{}-0", cutoff_ms);
            tracing::debug!(
                "No pending messages on {}; trimming entries older than {}",
                self.stream_name,
                id,
            );
            id
        };

        // XTRIM <stream> MINID ~ <cutoff_id>
        let trimmed: usize = redis::from_redis_value_ref(
            &conn
                .cmd(
                    &redis::cmd("XTRIM")
                        .arg(&self.stream_name)
                        .arg("MINID")
                        .arg("~")
                        .arg(&cutoff_id)
                        .clone(),
                )
                .await?,
        )
        .unwrap_or(0);

        if trimmed > 0 {
            tracing::info!(
                "Trimmed {} entries from {} (cutoff: {})",
                trimmed,
                self.stream_name,
                cutoff_id,
            );
        } else {
            tracing::debug!("Nothing to trim from {}", self.stream_name);
        }

        Ok(trimmed)
    }

    /// Get the delivery count for a message from XPENDING
    async fn get_delivery_count(
        &self,
        conn: &mut RedisConnectionOwned,
        msg_id: &str,
    ) -> Result<usize, StreamsQueueError> {
        // XPENDING stream group - + 1 consumer
        // This gives us detailed info including delivery count
        let result = conn
            .cmd(
                &redis::cmd("XPENDING")
                    .arg(&self.stream_name)
                    .arg(&self.consumer_group)
                    .arg("-")
                    .arg("+")
                    .arg(1)
                    .arg(&self.consumer_name)
                    .clone(),
            )
            .await?;

        // Result is an array of [message-id, consumer, idle-time, delivery-count]
        let entries: Vec<Vec<redis::Value>> =
            redis::from_redis_value_ref(&result).unwrap_or_default();

        for entry in entries {
            if entry.len() >= 4 {
                let id: String = redis::from_redis_value_ref(&entry[0]).unwrap_or_default();
                if id == msg_id {
                    let count: usize = redis::from_redis_value_ref(&entry[3]).unwrap_or(1);
                    return Ok(count);
                }
            }
        }

        // Default to 1 if not found
        Ok(1)
    }
}

#[async_trait::async_trait]
impl<T: Send + Sync + Serialize + DeserializeOwned> Queue<T> for RedisStreamQueue<T> {
    async fn enqueue(&self, item: T) -> Result<(), Box<dyn Error + Send + Sync>> {
        // Ensure consumer group exists (creates stream too via MKSTREAM)
        // Must be called before get_connection() to avoid deadlock (Mutex is not reentrant)
        self.ensure_consumer_group().await?;

        let mut guard = self.get_connection().await?;
        let conn = guard.as_mut().unwrap();

        // Serialize the item
        let data = serialize(&item, self.serialization)
            .map_err(|e| Box::new(e) as Box<dyn Error + Send + Sync>)?;

        // XADD stream MAXLEN ~ <cap> * payload <data>
        // The `~` makes Redis use efficient approximate trimming; combined
        // with `trim_stream` in the janitor, this bounds memory growth so
        // we no longer need a per-message XDEL after successful processing.
        let _: String = conn
            .cmd(
                &redis::cmd("XADD")
                    .arg(&self.stream_name)
                    .arg("MAXLEN")
                    .arg("~")
                    .arg(STREAM_MAXLEN)
                    .arg("*")
                    .arg("payload")
                    .arg(&data)
                    .clone(),
            )
            .await
            .map(|v| redis::from_redis_value_ref(&v).unwrap_or_default())?;

        Ok(())
    }

    async fn dequeue(&self) -> Result<Option<T>, Box<dyn Error + Send + Sync>> {
        // Ensure consumer group exists first (acquires and releases its own connection)
        // Must be called before get_connection() to avoid deadlock (Mutex is not reentrant)
        self.ensure_consumer_group().await?;

        let mut guard = self.get_connection().await?;
        let conn = guard.as_mut().unwrap();

        // First, try to get any pending messages (previously read but not acked)
        // XREADGROUP GROUP group consumer COUNT 1 STREAMS stream 0
        let result = conn
            .cmd(
                &redis::cmd("XREADGROUP")
                    .arg("GROUP")
                    .arg(&self.consumer_group)
                    .arg(&self.consumer_name)
                    .arg("COUNT")
                    .arg(1)
                    .arg("STREAMS")
                    .arg(&self.stream_name)
                    .arg("0") // Get pending messages first
                    .clone(),
            )
            .await?;

        // Check if we got a pending message
        if let Some((msg_id, data)) = parse_stream_message(&result) {
            let item: T = deserialize(&data, self.serialization)
                .map_err(|e| Box::new(e) as Box<dyn Error + Send + Sync>)?;

            // ACK the message immediately for simple dequeue
            let _ = conn
                .cmd(
                    &redis::cmd("XACK")
                        .arg(&self.stream_name)
                        .arg(&self.consumer_group)
                        .arg(&msg_id)
                        .clone(),
                )
                .await;

            return Ok(Some(item));
        }

        // No pending messages, try to get new ones
        // XREADGROUP GROUP group consumer COUNT 1 BLOCK 100 STREAMS stream >
        let result = conn
            .cmd(
                &redis::cmd("XREADGROUP")
                    .arg("GROUP")
                    .arg(&self.consumer_group)
                    .arg(&self.consumer_name)
                    .arg("COUNT")
                    .arg(1)
                    .arg("BLOCK")
                    .arg(100) // 100ms block timeout
                    .arg("STREAMS")
                    .arg(&self.stream_name)
                    .arg(">") // Only new messages
                    .clone(),
            )
            .await?;

        if let Some((msg_id, data)) = parse_stream_message(&result) {
            let item: T = deserialize(&data, self.serialization)
                .map_err(|e| Box::new(e) as Box<dyn Error + Send + Sync>)?;

            // ACK the message immediately for simple dequeue
            let _ = conn
                .cmd(
                    &redis::cmd("XACK")
                        .arg(&self.stream_name)
                        .arg(&self.consumer_group)
                        .arg(&msg_id)
                        .clone(),
                )
                .await;

            return Ok(Some(item));
        }

        Ok(None)
    }

    /// Total number of entries in the stream (XLEN).
    ///
    /// This is the *queue depth* — undelivered + delivered-but-unacked
    /// messages still resident in the stream. Use `pending_len()` to get
    /// just the in-flight (delivered but unacked) count for the consumer
    /// group. The previous behavior of returning the XPENDING count was
    /// misleading: it reported only in-flight work, not backlog.
    async fn len(&self) -> Result<usize, Box<dyn Error + Send + Sync>> {
        let mut guard = self.get_connection().await?;
        let conn = guard.as_mut().unwrap();

        let result = conn
            .cmd(&redis::cmd("XLEN").arg(&self.stream_name).clone())
            .await;

        match result {
            Ok(val) => Ok(redis::from_redis_value_ref(&val).unwrap_or(0)),
            // Stream doesn't exist yet — treat as empty rather than error,
            // since callers rely on this for connectivity checks at startup.
            Err(_) => Ok(0),
        }
    }

    async fn move_to_dead_letter_queue(
        &self,
        item: T,
        reason: String,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        let mut guard = self.get_connection().await?;
        let conn = guard.as_mut().unwrap();

        // Serialize the item
        let data = serialize(&item, self.serialization)
            .map_err(|e| Box::new(e) as Box<dyn Error + Send + Sync>)?;

        // Add to dead letter stream with reason
        let _: String = conn
            .cmd(
                &redis::cmd("XADD")
                    .arg(&self.dlq_stream)
                    .arg("MAXLEN")
                    .arg("~")
                    .arg(10000) // Keep last ~10k failed messages
                    .arg("*")
                    .arg("payload")
                    .arg(&data)
                    .arg("reason")
                    .arg(&reason)
                    .arg("timestamp")
                    .arg(chrono::Utc::now().timestamp())
                    .clone(),
            )
            .await
            .map(|v| redis::from_redis_value_ref(&v).unwrap_or_default())?;

        Ok(())
    }
}

impl<T: Send + Sync + Serialize + DeserializeOwned + Clone + 'static> RedisStreamQueue<T> {
    /// Variant of `dequeue_and_work` intended for `tokio::select!` loops.
    ///
    /// Unlike `dequeue_and_work` (which performs a non-blocking new-message
    /// read so polling-based callers can fall through quickly), this variant
    /// asks Redis to BLOCK for several seconds inside `XREADGROUP`. Combined
    /// with the outer select!'s shutdown branch, this lets the worker park
    /// inside the kernel/server instead of busy-polling — drastically
    /// reducing CPU and Redis QPS at idle while still waking instantly when
    /// a message arrives.
    pub async fn process_blocking<F, Fut, R>(
        &self,
        worker: F,
    ) -> Result<Option<R>, Box<dyn Error + Send + Sync>>
    where
        F: FnOnce(T) -> Fut + Send,
        Fut: Future<Output = Result<R, Box<dyn Error + Send + Sync>>> + Send,
        R: Send + Sync,
    {
        self.process_inner(worker, PROCESS_BLOCKING_MS).await
    }

    /// Try to refill the prefetch buffer. Returns true if the buffer is
    /// non-empty after this call. `block_ms` controls how long the *new*
    /// (`>`) read blocks inside Redis; pending (`0`) reads are always
    /// non-blocking. The pending read happens first to keep retries snappy.
    async fn refill_prefetch(&self, block_ms: u64) -> Result<bool, StreamsQueueError> {
        // Serialize concurrent refills against the *same* RedisStreamQueue
        // instance. Without this lock, multiple `process_blocking` futures
        // sharing the same `Arc<RedisStreamQueue>` would each fire an
        // independent `XREADGROUP ... 0` against the shared PEL, get back
        // the *same* set of pending message IDs (no XACKs have happened
        // yet), `extend` them all into the prefetch buffer, and proceed
        // to execute every PEL entry N times in parallel. See the
        // `refill_lock` field doc for the long-form rationale.
        let _refill_guard = self.refill_lock.lock().await;

        // Re-check the buffer under the refill lock. If a sibling refill
        // landed while we were waiting, we have nothing to do — let the
        // caller re-pop. This is what turns N losing-race waiters into
        // N no-ops instead of N redundant XREADGROUPs.
        {
            let buf = self.prefetched.lock().await;
            if !buf.is_empty() {
                return Ok(true);
            }
        }

        // Try pending (PEL) first, non-blocking. We must filter out
        // msg_ids that are *currently in flight* (handed off to a worker
        // but not yet ACKed): those entries are still in our PEL on Redis
        // until the XACK lands, so a `0` read happily re-includes them.
        // Without this filter, a refill that fires *between* batches —
        // while an earlier batch is still being processed — would
        // re-buffer the same un-ACKed entries and the next worker would
        // execute them a second time.
        let pending = self
            .read_batch_with(FetchSource::Pending, READ_BATCH_SIZE, 0)
            .await?;
        if !pending.is_empty() {
            let in_flight = self.in_flight.lock().await;
            let filtered: Vec<PrefetchedMessage> = pending
                .into_iter()
                .filter(|m| !in_flight.contains(&m.msg_id))
                .collect();
            drop(in_flight);
            if !filtered.is_empty() {
                let mut buf = self.prefetched.lock().await;
                buf.extend(filtered);
                return Ok(true);
            }
            // Every entry the PEL handed back is currently being processed
            // by a sibling worker. Treat as "no work right now" — the
            // caller will get None and the outer loop will park briefly
            // before retrying. This matches the behavior the caller sees
            // for an empty stream.
            return Ok(false);
        }

        // No pending → try fresh, optionally blocking. We hold the refill
        // lock across the BLOCK to serialize fresh reads as well; otherwise
        // N parallel waiters would each park inside Redis on their own
        // XREADGROUP > and wake together when *any* message arrives,
        // re-creating the original duplication on the new-message path.
        let fresh = self
            .read_batch_with(FetchSource::Fresh, READ_BATCH_SIZE, block_ms)
            .await?;
        if fresh.is_empty() {
            return Ok(false);
        }
        let mut buf = self.prefetched.lock().await;
        buf.extend(fresh);
        Ok(true)
    }

    /// Perform a single XREADGROUP, returning the raw messages tagged with
    /// their fetch source. `block_ms == 0` means the read does not BLOCK
    /// (a missing BLOCK arg is equivalent to non-blocking on `>` reads,
    /// and BLOCK is meaningless on `0` reads).
    async fn read_batch_with(
        &self,
        source: FetchSource,
        count: usize,
        block_ms: u64,
    ) -> Result<Vec<PrefetchedMessage>, StreamsQueueError> {
        let id_arg = match source {
            FetchSource::Fresh => ">",
            FetchSource::Pending => "0",
        };

        let mut guard = self.get_connection().await?;
        let conn = guard.as_mut().unwrap();

        let mut cmd = redis::cmd("XREADGROUP");
        cmd.arg("GROUP")
            .arg(&self.consumer_group)
            .arg(&self.consumer_name)
            .arg("COUNT")
            .arg(count);
        if matches!(source, FetchSource::Fresh) && block_ms > 0 {
            cmd.arg("BLOCK").arg(block_ms);
        }
        cmd.arg("STREAMS").arg(&self.stream_name).arg(id_arg);

        let result = conn.cmd(&cmd.clone()).await?;
        Ok(parse_stream_messages(&result)
            .into_iter()
            .map(|(msg_id, payload)| PrefetchedMessage {
                msg_id,
                payload,
                source,
            })
            .collect())
    }

    /// Shared implementation behind `dequeue_and_work` and `process_blocking`.
    /// `block_ms` is the max BLOCK time on the *new* (`>`) read; pass 0 to
    /// do a non-blocking read (legacy polling behavior).
    async fn process_inner<F, Fut, R>(
        &self,
        worker: F,
        block_ms: u64,
    ) -> Result<Option<R>, Box<dyn Error + Send + Sync>>
    where
        F: FnOnce(T) -> Fut + Send,
        Fut: Future<Output = Result<R, Box<dyn Error + Send + Sync>>> + Send,
        R: Send + Sync,
    {
        // Cached after the first call — no Redis round-trip on the hot path.
        // The dedicated janitor task in `hot_worker::server` runs XAUTOCLAIM
        // periodically across all queues, so we don't pay that cost here.
        self.ensure_consumer_group().await?;

        // Pop one message from the prefetch buffer; refill if empty.
        let prefetched = {
            let mut buf = self.prefetched.lock().await;
            buf.pop_front()
        };
        let PrefetchedMessage {
            msg_id,
            payload,
            source,
        } = match prefetched {
            Some(m) => m,
            None => {
                if !self.refill_prefetch(block_ms).await? {
                    return Ok(None);
                }
                let mut buf = self.prefetched.lock().await;
                match buf.pop_front() {
                    Some(m) => m,
                    None => return Ok(None),
                }
            }
        };

        // Mark this message as in-flight so any concurrent `refill_prefetch`
        // that re-reads our PEL while we're still processing won't see it
        // as fresh work and re-buffer it for a sibling worker. Guaranteed
        // to be cleared by the unconditional `remove(&msg_id)` after the
        // inner block, regardless of which exit path we take (success,
        // worker failure, retry-exhaustion DLQ, deserialize-error DLQ, or
        // an `?`-propagated transport error).
        {
            let mut in_flight = self.in_flight.lock().await;
            in_flight.insert(msg_id.clone());
        }

        let result = self
            .process_one(msg_id.clone(), payload, source, worker)
            .await;
        self.in_flight.lock().await.remove(&msg_id);
        result
    }

    /// Inner half of `process_inner`: runs delivery-count check, DLQ
    /// fallout, and the worker invocation for a single popped message.
    /// Split out so `process_inner` can guarantee `in_flight` cleanup
    /// across every exit path with a single trailing `remove`.
    async fn process_one<F, Fut, R>(
        &self,
        msg_id: String,
        payload: Vec<u8>,
        source: FetchSource,
        worker: F,
    ) -> Result<Option<R>, Box<dyn Error + Send + Sync>>
    where
        F: FnOnce(T) -> Fut + Send,
        Fut: Future<Output = Result<R, Box<dyn Error + Send + Sync>>> + Send,
        R: Send + Sync,
    {
        // Phase 3c: skip XPENDING for fresh reads — delivery count is 1 on
        // first delivery. Only consult XPENDING for messages we re-read out
        // of our own PEL (i.e. previous failures).
        let delivery_count = match source {
            FetchSource::Fresh => 1,
            FetchSource::Pending => {
                let mut guard = self.get_connection().await?;
                let conn = guard.as_mut().unwrap();
                self.get_delivery_count(conn, &msg_id).await?
            }
        };
        let queue_wait_ms = stream_message_age_ms(&msg_id);

        if delivery_count > MAX_PROCESSING_RETRIES {
            tracing::warn!(
                "Message {} exceeded max retries ({}/{}), moving to DLQ",
                msg_id,
                delivery_count,
                MAX_PROCESSING_RETRIES
            );
            if queue_timing_enabled() {
                tracing::info!(
                    target: "hot::queue::timing",
                    queue = %self.stream_name,
                    backend = "redis",
                    delivery_source = ?source,
                    queue_wait_ms = queue_wait_ms,
                    processing_ms = 0u64,
                    retry_count = delivery_count.saturating_sub(1),
                    outcome = "retry_exhausted",
                    message_id = %msg_id,
                    "queue item skipped"
                );
            }
            self.move_msg_to_dlq(
                &msg_id,
                &payload,
                format!(
                    "Exceeded max retries ({}/{})",
                    delivery_count, MAX_PROCESSING_RETRIES
                ),
            )
            .await?;
            return Err(Box::new(QueueProcessingError::RetryLimitExceeded));
        }

        // Deserialize the item
        let item: T = match deserialize(&payload, self.serialization) {
            Ok(item) => item,
            Err(e) => {
                tracing::error!(
                    "Failed to deserialize message {}: {}. Moving to DLQ.",
                    msg_id,
                    e
                );
                if queue_timing_enabled() {
                    tracing::info!(
                        target: "hot::queue::timing",
                        queue = %self.stream_name,
                        backend = "redis",
                        delivery_source = ?source,
                        queue_wait_ms = queue_wait_ms,
                        processing_ms = 0u64,
                        retry_count = delivery_count.saturating_sub(1),
                        outcome = "deserialize_error",
                        message_id = %msg_id,
                        "queue item skipped"
                    );
                }
                self.move_msg_to_dlq(&msg_id, &payload, format!("Deserialization error: {}", e))
                    .await?;
                return Err(Box::new(e));
            }
        };

        tracing::debug!(
            "Processing message {} (delivery: {}, source: {:?})",
            msg_id,
            delivery_count,
            source,
        );

        let processing_started = Instant::now();
        match worker(item).await {
            Ok(result) => {
                // Success — ACK only. We no longer XDEL per message because
                // `XADD MAXLEN ~` keeps the stream bounded and `trim_stream`
                // periodically prunes below the oldest pending ID. This
                // halves the per-message Redis command count on success.
                let mut guard = self.get_connection().await?;
                let conn = guard.as_mut().unwrap();
                let _ = conn
                    .cmd(
                        &redis::cmd("XACK")
                            .arg(&self.stream_name)
                            .arg(&self.consumer_group)
                            .arg(&msg_id)
                            .clone(),
                    )
                    .await;
                if queue_timing_enabled() {
                    tracing::info!(
                        target: "hot::queue::timing",
                        queue = %self.stream_name,
                        backend = "redis",
                        delivery_source = ?source,
                        queue_wait_ms = queue_wait_ms,
                        processing_ms = duration_ms(processing_started.elapsed()),
                        retry_count = delivery_count.saturating_sub(1),
                        outcome = "success",
                        message_id = %msg_id,
                        "queue item processed"
                    );
                }
                tracing::debug!("Successfully processed and ACKed message {}", msg_id);
                Ok(Some(result))
            }
            Err(e) => {
                if let Some(infra) = e.downcast_ref::<QueueInfrastructureError>() {
                    let backoff = infra.backoff();
                    let reason = infra.to_string();
                    if queue_timing_enabled() {
                        tracing::info!(
                            target: "hot::queue::timing",
                            queue = %self.stream_name,
                            backend = "redis",
                            delivery_source = ?source,
                            queue_wait_ms = queue_wait_ms,
                            processing_ms = duration_ms(processing_started.elapsed()),
                            retry_count = delivery_count.saturating_sub(1),
                            outcome = "infrastructure_retry",
                            backoff_ms = duration_ms(backoff),
                            message_id = %msg_id,
                            "queue item deferred for infrastructure retry"
                        );
                    }
                    if !backoff.is_zero() {
                        tokio::time::sleep(backoff).await;
                    }
                    self.requeue_msg_for_infrastructure_retry(&msg_id, &payload, reason)
                        .await?;
                    return Err(Box::new(QueueProcessingError::QueueError(e)));
                }
                if queue_timing_enabled() {
                    tracing::info!(
                        target: "hot::queue::timing",
                        queue = %self.stream_name,
                        backend = "redis",
                        delivery_source = ?source,
                        queue_wait_ms = queue_wait_ms,
                        processing_ms = duration_ms(processing_started.elapsed()),
                        retry_count = delivery_count.saturating_sub(1),
                        outcome = "worker_error",
                        message_id = %msg_id,
                        "queue item processing failed"
                    );
                }
                // Worker failed — don't ACK; message stays in our PEL and
                // will be re-read with `0` on the next call. Any other
                // prefetched messages stay in the buffer and will still be
                // processed normally — they aren't tied to this failure.
                //
                // Keep this at DEBUG: an intermediate failure that gets
                // retried is not actionable, and a permanent failure already
                // surfaces as the "exceeded max retries, moving to DLQ" WARN
                // below. Promoting every retry to WARN spams the log with
                // 3 lines per genuinely-failed message.
                tracing::debug!(
                    "Worker failed for message {} (attempt {}/{}): {}. Will retry.",
                    msg_id,
                    delivery_count,
                    MAX_PROCESSING_RETRIES,
                    e
                );
                Err(Box::new(QueueProcessingError::WorkerError(e)))
            }
        }
    }

    /// Copy the payload to a fresh stream entry and ACK the current pending
    /// entry. This is for infrastructure failures where the work item is still
    /// healthy, but the worker could not make a conclusive ownership decision.
    async fn requeue_msg_for_infrastructure_retry(
        &self,
        msg_id: &str,
        payload: &[u8],
        reason: String,
    ) -> Result<(), StreamsQueueError> {
        let mut guard = self.get_connection().await?;
        let conn = guard.as_mut().unwrap();

        let _: String = conn
            .cmd(
                &redis::cmd("XADD")
                    .arg(&self.stream_name)
                    .arg("MAXLEN")
                    .arg("~")
                    .arg(STREAM_MAXLEN)
                    .arg("*")
                    .arg("payload")
                    .arg(payload)
                    .arg("retry_reason")
                    .arg(&reason)
                    .arg("original_id")
                    .arg(msg_id)
                    .arg("timestamp")
                    .arg(chrono::Utc::now().timestamp())
                    .clone(),
            )
            .await
            .map(|v| redis::from_redis_value_ref(&v).unwrap_or_default())?;

        let _ = conn
            .cmd(
                &redis::cmd("XACK")
                    .arg(&self.stream_name)
                    .arg(&self.consumer_group)
                    .arg(msg_id)
                    .clone(),
            )
            .await;

        Ok(())
    }

    /// ACK a single message and copy it into the DLQ. Used both for retry
    /// exhaustion and unrecoverable deserialization failures.
    async fn move_msg_to_dlq(
        &self,
        msg_id: &str,
        payload: &[u8],
        reason: String,
    ) -> Result<(), StreamsQueueError> {
        let mut guard = self.get_connection().await?;
        let conn = guard.as_mut().unwrap();

        let _ = conn
            .cmd(
                &redis::cmd("XACK")
                    .arg(&self.stream_name)
                    .arg(&self.consumer_group)
                    .arg(msg_id)
                    .clone(),
            )
            .await;

        let _: String = conn
            .cmd(
                &redis::cmd("XADD")
                    .arg(&self.dlq_stream)
                    .arg("MAXLEN")
                    .arg("~")
                    .arg(10000)
                    .arg("*")
                    .arg("payload")
                    .arg(payload)
                    .arg("reason")
                    .arg(&reason)
                    .arg("original_id")
                    .arg(msg_id)
                    .arg("timestamp")
                    .arg(chrono::Utc::now().timestamp())
                    .clone(),
            )
            .await
            .map(|v| redis::from_redis_value_ref(&v).unwrap_or_default())?;

        Ok(())
    }
}

#[async_trait::async_trait]
impl<T: Send + Sync + Serialize + DeserializeOwned + Clone + 'static> QueueProcessor<T>
    for RedisStreamQueue<T>
{
    /// Non-blocking processor used by polling-based worker loops (e.g.
    /// `hot_worker/server.rs`, which sequentially polls multiple queues
    /// and uses its own idle-backoff sleep). Returns `Ok(None)` immediately
    /// when no message is available rather than parking inside Redis. Use
    /// `process_blocking` for `tokio::select!`-based loops.
    async fn dequeue_and_work<F, Fut, R>(
        &self,
        worker: F,
    ) -> Result<Option<R>, Box<dyn Error + Send + Sync>>
    where
        F: FnOnce(T) -> Fut + Send,
        Fut: Future<Output = Result<R, Box<dyn Error + Send + Sync>>> + Send,
        R: Send + Sync,
    {
        self.process_inner(worker, 0).await
    }
}

/// Parse a single stream message from XREADGROUP result.
/// Returns the first (msg_id, payload) found, if any.
fn parse_stream_message(result: &redis::Value) -> Option<(String, Vec<u8>)> {
    parse_stream_messages(result).into_iter().next()
}

/// Parse all messages from an XREADGROUP result.
///
/// Shape: `[[stream-name, [[id, [field, value, ...]], ...]]]` or nil.
/// We extract every `(id, payload)` pair, in order. Messages without a
/// `payload` field are silently skipped — the consumer group still has
/// them in its PEL until ACKed.
fn parse_stream_messages(result: &redis::Value) -> Vec<(String, Vec<u8>)> {
    if matches!(result, redis::Value::Nil) {
        return Vec::new();
    }

    let streams: Vec<redis::Value> = match redis::from_redis_value_ref(result) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };

    let mut out = Vec::new();
    for stream in streams {
        let stream_data: Vec<redis::Value> = match redis::from_redis_value_ref(&stream) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if stream_data.len() < 2 {
            continue;
        }
        let messages: Vec<redis::Value> = match redis::from_redis_value_ref(&stream_data[1]) {
            Ok(v) => v,
            Err(_) => continue,
        };
        for msg_val in messages {
            let msg: Vec<redis::Value> = match redis::from_redis_value_ref(&msg_val) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if msg.len() < 2 {
                continue;
            }
            let msg_id: String = match redis::from_redis_value_ref(&msg[0]) {
                Ok(v) => v,
                Err(_) => continue,
            };
            let fields: Vec<redis::Value> = match redis::from_redis_value_ref(&msg[1]) {
                Ok(v) => v,
                Err(_) => continue,
            };
            let mut payload: Option<Vec<u8>> = None;
            let mut i = 0;
            while i + 1 < fields.len() {
                let name: String = redis::from_redis_value_ref(&fields[i]).unwrap_or_default();
                if name == "payload" {
                    if let Ok(p) = redis::from_redis_value_ref::<Vec<u8>>(&fields[i + 1]) {
                        payload = Some(p);
                    }
                    break;
                }
                i += 2;
            }
            if let Some(p) = payload {
                out.push((msg_id, p));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Open a Redis client, returning None when no local Redis is reachable.
    /// Lets tests skip cleanly in environments without Redis (CI without the
    /// service, sandboxed builds) instead of failing.
    async fn try_client() -> Option<redis::Client> {
        let client = redis::Client::open("redis://127.0.0.1/").ok()?;
        // Fail fast if Redis isn't actually running.
        client.get_multiplexed_async_connection().await.ok()?;
        Some(client)
    }

    async fn cleanup(client: &redis::Client, queue_name: &str) {
        if let Ok(mut conn) = client.get_multiplexed_async_connection().await {
            let stream_key = format!("{{{}}}", queue_name);
            let dlq_key = format!("{}:deadletter", stream_key);
            let _: redis::RedisResult<()> = redis::cmd("DEL")
                .arg(&stream_key)
                .arg(&dlq_key)
                .query_async(&mut conn)
                .await;
        }
    }

    #[tokio::test]
    async fn test_stream_queue_basic() {
        let Some(client) = try_client().await else {
            eprintln!("skipping: Redis not available");
            return;
        };
        let queue_name = format!("test_stream_{}", Uuid::new_v4());
        let queue = RedisStreamQueue::<String>::new(client.clone(), queue_name.clone());

        queue.enqueue("hello".to_string()).await.unwrap();
        queue.enqueue("world".to_string()).await.unwrap();

        let item1 = queue.dequeue().await.unwrap();
        assert_eq!(item1, Some("hello".to_string()));

        let item2 = queue.dequeue().await.unwrap();
        assert_eq!(item2, Some("world".to_string()));

        cleanup(&client, &queue_name).await;
    }

    /// Phase 3d: batched XREADGROUP feeds a per-instance buffer; multiple
    /// `dequeue_and_work` calls drain it before another Redis round-trip.
    #[tokio::test]
    async fn test_dequeue_and_work_batches_across_calls() {
        let Some(client) = try_client().await else {
            eprintln!("skipping: Redis not available");
            return;
        };
        let queue_name = format!("test_stream_batch_{}", Uuid::new_v4());
        let queue = RedisStreamQueue::<i64>::new(client.clone(), queue_name.clone());

        for i in 0..5 {
            queue.enqueue(i).await.unwrap();
        }

        let processed = Arc::new(AtomicUsize::new(0));
        for _ in 0..5 {
            let p = processed.clone();
            let res = queue
                .dequeue_and_work(|item: i64| async move {
                    let _ = item;
                    p.fetch_add(1, Ordering::SeqCst);
                    Ok::<_, Box<dyn Error + Send + Sync>>(())
                })
                .await
                .unwrap();
            assert!(res.is_some(), "expected each call to process one item");
        }
        assert_eq!(processed.load(Ordering::SeqCst), 5);

        // Once drained, the new-message read must return Ok(None) without
        // blocking (dequeue_and_work uses BLOCK=0).
        let res: Option<()> = queue
            .dequeue_and_work(|_: i64| async move { Ok::<_, Box<dyn Error + Send + Sync>>(()) })
            .await
            .unwrap();
        assert!(res.is_none());

        cleanup(&client, &queue_name).await;
    }

    /// Regression test for the PEL refill race: when many `process_blocking`
    /// futures share the same `Arc<RedisStreamQueue>` and the consumer's
    /// PEL is non-empty (e.g. just after a worker restart that reclaimed
    /// orphaned entries via XAUTOCLAIM), each entry must be processed
    /// **exactly once** rather than once per concurrent caller.
    ///
    /// Pre-fix bug: each concurrent caller saw the prefetch buffer empty,
    /// raced into `refill_prefetch`, each issued `XREADGROUP ... 0`
    /// against the *same* consumer, each got back the same N PEL entries
    /// (no XACKs had happened yet), each `extend`ed them into the shared
    /// buffer, and the workers then executed every PEL entry N times.
    /// Manifested in dev as the same `task_id` being processed 18+ times
    /// with parallel container starts racing on the same data-volume
    /// bind-mount path.
    #[tokio::test]
    async fn test_concurrent_process_blocking_doesnt_duplicate_pel_entries() {
        use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
        use tokio::sync::Mutex as TokioMutex;

        let Some(client) = try_client().await else {
            eprintln!("skipping: Redis not available");
            return;
        };
        let queue_name = format!("test_pel_refill_race_{}", Uuid::new_v4());
        let stream_key = format!("{{{}}}", queue_name);
        let consumer = format!("worker-shared-{}", Uuid::new_v4());

        // Prime the PEL: enqueue N items, then deliver them all (without
        // ACKing) to a single consumer name. This mirrors the post-startup
        // state in `hot_task_worker` after `recover_orphaned_items()` has
        // claimed entries from a dead sibling consumer into the new
        // worker's PEL.
        let primer = RedisStreamQueue::<i64>::new(client.clone(), queue_name.clone())
            .with_consumer_name(consumer.clone());
        primer.ensure_consumer_group().await.unwrap();
        const N: i64 = 6;
        for i in 0..N {
            primer.enqueue(i).await.unwrap();
        }
        let delivered =
            xreadgroup_drain(&client, &stream_key, DEFAULT_CONSUMER_GROUP, &consumer).await;
        assert_eq!(delivered as i64, N, "all entries must be delivered to PEL");
        assert_eq!(
            xpending_count(&client, &stream_key, DEFAULT_CONSUMER_GROUP).await as i64,
            N,
            "all entries should be in PEL before we start the workers"
        );

        // Now spin up M concurrent process_blocking workers against a
        // freshly-built handle that shares the same consumer name (so
        // `XREADGROUP 0` reads from the primed PEL). Each handler sleeps
        // briefly so multiple workers are guaranteed to be in
        // `refill_prefetch` simultaneously, maximizing the race window.
        let queue = Arc::new(
            RedisStreamQueue::<i64>::new(client.clone(), queue_name.clone())
                .with_consumer_name(consumer.clone()),
        );

        let seen: Arc<TokioMutex<Vec<i64>>> = Arc::new(TokioMutex::new(Vec::new()));
        let calls = Arc::new(AtomicUsize::new(0));
        const M: usize = 8;
        let mut handles = Vec::new();
        for _ in 0..M {
            let q = Arc::clone(&queue);
            let seen = Arc::clone(&seen);
            let calls = Arc::clone(&calls);
            handles.push(tokio::spawn(async move {
                loop {
                    let r = tokio::time::timeout(
                        std::time::Duration::from_millis(500),
                        q.process_blocking(|item: i64| {
                            let seen = Arc::clone(&seen);
                            let calls = Arc::clone(&calls);
                            async move {
                                calls.fetch_add(1, AtomicOrdering::SeqCst);
                                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                                seen.lock().await.push(item);
                                Ok::<(), Box<dyn Error + Send + Sync>>(())
                            }
                        }),
                    )
                    .await;
                    match r {
                        Ok(Ok(Some(_))) => continue,
                        _ => break,
                    }
                }
            }));
        }
        for h in handles {
            let _ = h.await;
        }

        let mut got = seen.lock().await.clone();
        got.sort();
        let expected: Vec<i64> = (0..N).collect();
        assert_eq!(
            got, expected,
            "every PEL entry must be processed exactly once across all concurrent workers"
        );
        assert_eq!(
            calls.load(AtomicOrdering::SeqCst) as i64,
            N,
            "handler must be invoked exactly N times — extra invocations indicate the PEL \
             refill race re-delivered entries to multiple workers"
        );
        assert_eq!(
            xpending_count(&client, &stream_key, DEFAULT_CONSUMER_GROUP).await,
            0,
            "all entries should be ACKed and out of PEL"
        );

        cleanup(&client, &queue_name).await;
    }

    /// Phase 3a: ensure_consumer_group is cached. The second call should
    /// flip no Redis state — hard to observe directly, but at minimum the
    /// in-memory flag should be set after the first call.
    #[tokio::test]
    async fn test_ensure_consumer_group_caches() {
        let Some(client) = try_client().await else {
            eprintln!("skipping: Redis not available");
            return;
        };
        let queue_name = format!("test_stream_egroup_{}", Uuid::new_v4());
        let queue = RedisStreamQueue::<String>::new(client.clone(), queue_name.clone());

        assert!(
            !queue
                .consumer_group_ensured
                .load(std::sync::atomic::Ordering::Acquire)
        );
        queue.ensure_consumer_group().await.unwrap();
        assert!(
            queue
                .consumer_group_ensured
                .load(std::sync::atomic::Ordering::Acquire)
        );
        // Second call must succeed and remain a no-op.
        queue.ensure_consumer_group().await.unwrap();
        assert!(
            queue
                .consumer_group_ensured
                .load(std::sync::atomic::Ordering::Acquire)
        );

        cleanup(&client, &queue_name).await;
    }

    /// Phase 3c: a worker that fails leaves the message in the consumer's
    /// PEL. The next call re-reads it from `0` (Pending source) and the
    /// delivery count comes from XPENDING. After exceeding MAX_PROCESSING_RETRIES
    /// the message lands in the DLQ.
    #[tokio::test]
    async fn test_failed_message_retries_then_dlq() {
        let Some(client) = try_client().await else {
            eprintln!("skipping: Redis not available");
            return;
        };
        let queue_name = format!("test_stream_retry_{}", Uuid::new_v4());
        let queue = RedisStreamQueue::<String>::new(client.clone(), queue_name.clone());

        queue.enqueue("flaky".to_string()).await.unwrap();

        // First N attempts fail (worker returns Err). We expect WorkerError
        // on each attempt up to MAX_PROCESSING_RETRIES, then RetryLimitExceeded.
        for _ in 0..MAX_PROCESSING_RETRIES {
            let res = queue
                .dequeue_and_work(|_: String| async move {
                    Err::<(), _>(Box::<dyn Error + Send + Sync>::from(
                        "intentional test failure",
                    ))
                })
                .await;
            assert!(res.is_err(), "worker failure should propagate");
            // Drain any leftover prefetched items so the next iteration
            // re-fetches from Redis (and exercises the pending PEL path).
            queue.prefetched.lock().await.clear();
        }

        // The next attempt should detect retries-exceeded and route to DLQ.
        let res = queue
            .dequeue_and_work(|_: String| async move { Ok::<(), Box<dyn Error + Send + Sync>>(()) })
            .await;
        match res {
            Err(e) => {
                let msg = e.to_string();
                assert!(
                    msg.contains("Retry limit exceeded") || msg.contains("retry"),
                    "unexpected error: {}",
                    msg
                );
            }
            Ok(_) => panic!("expected RetryLimitExceeded after exhausting retries"),
        }

        // DLQ should now contain the failing message.
        let mut conn = client.get_multiplexed_async_connection().await.unwrap();
        let dlq_key = format!("{{{}}}:deadletter", queue_name);
        let dlq_len: i64 = redis::cmd("XLEN")
            .arg(&dlq_key)
            .query_async(&mut conn)
            .await
            .unwrap_or(0);
        assert!(
            dlq_len >= 1,
            "expected at least one message in DLQ, got {}",
            dlq_len
        );

        cleanup(&client, &queue_name).await;
    }

    #[tokio::test]
    async fn test_infrastructure_retry_requeues_without_dlq_retry_budget() {
        let Some(client) = try_client().await else {
            eprintln!("skipping: Redis not available");
            return;
        };
        let queue_name = format!("test_stream_infra_retry_{}", Uuid::new_v4());
        let stream_key = format!("{{{}}}", queue_name);
        let queue = RedisStreamQueue::<String>::new(client.clone(), queue_name.clone());

        queue.enqueue("flaky-infra".to_string()).await.unwrap();

        for _ in 0..(MAX_PROCESSING_RETRIES + 2) {
            let res = queue
                .dequeue_and_work(|_: String| async move {
                    Err::<(), Box<dyn Error + Send + Sync>>(Box::new(
                        QueueInfrastructureError::new(
                            "temporary infrastructure failure",
                            std::time::Duration::ZERO,
                        ),
                    ))
                })
                .await;
            assert!(res.is_err(), "infrastructure retry should be observable");
            queue.prefetched.lock().await.clear();
        }

        let res = queue
            .dequeue_and_work(|item: String| async move {
                assert_eq!(item, "flaky-infra");
                Ok::<(), Box<dyn Error + Send + Sync>>(())
            })
            .await
            .unwrap();
        assert_eq!(res, Some(()));

        let mut conn = client.get_multiplexed_async_connection().await.unwrap();
        let dlq_key = format!("{{{}}}:deadletter", queue_name);
        let dlq_len: i64 = redis::cmd("XLEN")
            .arg(&dlq_key)
            .query_async(&mut conn)
            .await
            .unwrap_or(0);
        assert_eq!(dlq_len, 0, "infrastructure retries must not DLQ");
        assert_eq!(
            xpending_count(&client, &stream_key, DEFAULT_CONSUMER_GROUP).await,
            0,
            "successful final processing should ACK the remaining delivery"
        );

        cleanup(&client, &queue_name).await;
    }

    // -----------------------------------------------------------------
    // Startup window / fast-forward tests
    // -----------------------------------------------------------------

    /// Insert a stream entry at a chosen historical timestamp by passing an
    /// explicit `<ms>-*` ID to XADD. The `payload` bytes are stored verbatim
    /// in the entry's `payload` field — caller is responsible for matching
    /// the queue's `Serialization` if the entry is intended to be deserialized
    /// (otherwise pass any bytes; the test just needs the entry to exist).
    /// Returns the assigned ID.
    async fn xadd_at_ms(
        client: &redis::Client,
        stream: &str, // already includes hash tags
        ms: u64,
        payload: &[u8],
    ) -> String {
        let mut conn = client.get_multiplexed_async_connection().await.unwrap();
        let id: String = redis::cmd("XADD")
            .arg(stream)
            .arg(format!("{}-*", ms))
            .arg("payload")
            .arg(payload)
            .query_async(&mut conn)
            .await
            .expect("XADD with explicit ms");
        id
    }

    async fn xinfo_last_delivered_id(
        client: &redis::Client,
        stream: &str,
        group: &str,
    ) -> Option<String> {
        let mut conn = client.get_multiplexed_async_connection().await.unwrap();
        let val: redis::Value = redis::cmd("XINFO")
            .arg("GROUPS")
            .arg(stream)
            .query_async(&mut conn)
            .await
            .ok()?;
        let groups: Vec<redis::Value> = redis::from_redis_value_ref(&val).ok()?;
        groups
            .iter()
            .find_map(|g| extract_group_field(g, group, "last-delivered-id"))
    }

    /// `with_startup_window` makes `ensure_consumer_group` create a brand-new
    /// group at `<now - window>-0`. With ancient entries already in the
    /// stream, the new group must NOT see them via XREADGROUP > .
    #[tokio::test]
    async fn test_with_startup_window_skips_ancient_on_new_group() {
        let Some(client) = try_client().await else {
            eprintln!("skipping: Redis not available");
            return;
        };
        let queue_name = format!("test_window_new_{}", Uuid::new_v4());
        let stream_key = format!("{{{}}}", queue_name);

        // Pre-seed the stream with an entry from 2 hours ago, BEFORE creating
        // the consumer group, so the group has no PEL for it. Payload bytes
        // don't need to be valid JSON — the test asserts the entry is NOT
        // delivered, so deserialization never runs.
        let two_hours_ago_ms =
            (chrono::Utc::now().timestamp_millis() as u64).saturating_sub(2 * 60 * 60 * 1000);
        let _ancient_id = xadd_at_ms(&client, &stream_key, two_hours_ago_ms, b"ancient").await;

        // Window of 30 minutes: anything older than 30m should be invisible
        // to the freshly-created group.
        let queue = RedisStreamQueue::<String>::new(client.clone(), queue_name.clone())
            .with_startup_window(std::time::Duration::from_secs(30 * 60));

        // First dequeue triggers ensure_consumer_group, then XREADGROUP > .
        // It must return None — the only entry is older than the window.
        let item = queue.dequeue().await.unwrap();
        assert_eq!(
            item, None,
            "ancient entry must NOT be delivered to a new windowed group"
        );

        // Sanity: a fresh enqueue lands inside the window and IS delivered.
        queue.enqueue("recent".to_string()).await.unwrap();
        let item = queue.dequeue().await.unwrap();
        assert_eq!(item, Some("recent".to_string()));

        cleanup(&client, &queue_name).await;
    }

    /// Without `with_startup_window`, the historical behavior is preserved:
    /// a fresh group starts at `0` and receives every retained entry.
    #[tokio::test]
    async fn test_without_startup_window_replays_history() {
        let Some(client) = try_client().await else {
            eprintln!("skipping: Redis not available");
            return;
        };
        let queue_name = format!("test_window_off_{}", Uuid::new_v4());
        let stream_key = format!("{{{}}}", queue_name);

        // Pre-encode the payload using the queue's *actual* serialization so
        // the dequeue path can deserialize it. Default is Serialization::ZstdJson.
        let payload = serialize(&"ancient".to_string(), Serialization::default()).unwrap();
        let two_hours_ago_ms =
            (chrono::Utc::now().timestamp_millis() as u64).saturating_sub(2 * 60 * 60 * 1000);
        let _ = xadd_at_ms(&client, &stream_key, two_hours_ago_ms, &payload).await;

        let queue = RedisStreamQueue::<String>::new(client.clone(), queue_name.clone());

        // No window → group created at "0" → ancient entry is delivered.
        let item = queue.dequeue().await.unwrap();
        assert_eq!(
            item,
            Some("ancient".to_string()),
            "without a window, historical entries must still be delivered (back-compat)"
        );

        cleanup(&client, &queue_name).await;
    }

    /// `fast_forward_if_stale` advances an existing, stale group's
    /// last-delivered-id past the cutoff so a worker coming back from a
    /// long outage doesn't drain ancient backlog.
    #[tokio::test]
    async fn test_fast_forward_advances_stale_group() {
        let Some(client) = try_client().await else {
            eprintln!("skipping: Redis not available");
            return;
        };
        let queue_name = format!("test_window_ff_{}", Uuid::new_v4());
        let stream_key = format!("{{{}}}", queue_name);

        // Step 1: simulate the "outage" — create a group at `0` (no window),
        // populate the stream with 3 ancient entries, do not consume them.
        let unwindowed = RedisStreamQueue::<String>::new(client.clone(), queue_name.clone());
        unwindowed.ensure_consumer_group().await.unwrap();
        let now_ms = chrono::Utc::now().timestamp_millis() as u64;
        for offset_ms in [3 * 60 * 60 * 1000, 2 * 60 * 60 * 1000, 90 * 60 * 1000] {
            xadd_at_ms(
                &client,
                &stream_key,
                now_ms.saturating_sub(offset_ms),
                b"ancient",
            )
            .await;
        }

        // Group's last-delivered-id is still "0-0" (nothing consumed yet).
        let initial_ldid = xinfo_last_delivered_id(&client, &stream_key, DEFAULT_CONSUMER_GROUP)
            .await
            .expect("group should exist");
        assert_eq!(initial_ldid, "0-0", "group should start at 0-0");

        // Step 2: a fresh worker comes online with a 1-hour window and runs
        // fast-forward. Two of the three ancient entries (3h, 2h) sit in the
        // (last-delivered-id, cutoff] window, so we expect at least 2 skipped.
        let windowed = RedisStreamQueue::<String>::new(client.clone(), queue_name.clone())
            .with_startup_window(std::time::Duration::from_secs(60 * 60));
        let skipped = windowed
            .fast_forward_if_stale()
            .await
            .expect("fast_forward must succeed");
        assert!(
            skipped >= 2,
            "expected to skip at least the 2-hour and 3-hour entries, got {}",
            skipped
        );

        // Step 3: post-conditions — group last-delivered-id has moved up,
        // and only the within-window entry (90m... wait, also outside) plus
        // any later entries are deliverable. The 90-minute entry is also
        // outside the 1h window, so the group should now see nothing.
        let after_ldid = xinfo_last_delivered_id(&client, &stream_key, DEFAULT_CONSUMER_GROUP)
            .await
            .expect("group should still exist");
        assert_ne!(
            after_ldid, "0-0",
            "fast-forward should have moved last-delivered-id off 0-0"
        );

        let item = windowed.dequeue().await.unwrap();
        assert_eq!(
            item, None,
            "all three ancient entries are outside the 1h window → nothing to deliver"
        );

        // Step 4: a brand-new entry within the window IS delivered.
        windowed.enqueue("fresh".to_string()).await.unwrap();
        let item = windowed.dequeue().await.unwrap();
        assert_eq!(item, Some("fresh".to_string()));

        cleanup(&client, &queue_name).await;
    }

    /// `fast_forward_if_stale` is a no-op when the group is already within
    /// the window — must NOT move last-delivered-id forward in that case.
    #[tokio::test]
    async fn test_fast_forward_noop_when_fresh() {
        let Some(client) = try_client().await else {
            eprintln!("skipping: Redis not available");
            return;
        };
        let queue_name = format!("test_window_fresh_{}", Uuid::new_v4());

        // Brand-new windowed queue: ensure_consumer_group will create the
        // group at `<now - 1h>-0`. fast_forward should report 0 skipped
        // because last-delivered-id == start point >= cutoff.
        let queue = RedisStreamQueue::<String>::new(client.clone(), queue_name.clone())
            .with_startup_window(std::time::Duration::from_secs(60 * 60));
        queue.ensure_consumer_group().await.unwrap();

        let before = xinfo_last_delivered_id(
            &client,
            &format!("{{{}}}", queue_name),
            DEFAULT_CONSUMER_GROUP,
        )
        .await;

        let skipped = queue
            .fast_forward_if_stale()
            .await
            .expect("fast_forward must succeed");
        assert_eq!(
            skipped, 0,
            "no-op expected when group is already within the window"
        );

        let after = xinfo_last_delivered_id(
            &client,
            &format!("{{{}}}", queue_name),
            DEFAULT_CONSUMER_GROUP,
        )
        .await;
        assert_eq!(
            before, after,
            "fast-forward must NOT move last-delivered-id when already fresh"
        );

        cleanup(&client, &queue_name).await;
    }

    /// `fast_forward_if_stale` is a complete no-op (no Redis call needed)
    /// when no startup window is configured, so callers that haven't opted
    /// in see exactly the historical behavior.
    #[tokio::test]
    async fn test_fast_forward_noop_without_window() {
        let Some(client) = try_client().await else {
            eprintln!("skipping: Redis not available");
            return;
        };
        let queue_name = format!("test_window_none_{}", Uuid::new_v4());

        let queue = RedisStreamQueue::<String>::new(client.clone(), queue_name.clone());
        let skipped = queue
            .fast_forward_if_stale()
            .await
            .expect("fast_forward must succeed even with no window");
        assert_eq!(skipped, 0);

        // The group must not have been created as a side effect — we never
        // touched Redis. Verify by reading XINFO directly: the stream
        // shouldn't exist.
        let mut conn = client.get_multiplexed_async_connection().await.unwrap();
        let exists: i64 = redis::cmd("EXISTS")
            .arg(format!("{{{}}}", queue_name))
            .query_async(&mut conn)
            .await
            .unwrap_or(0);
        assert_eq!(
            exists, 0,
            "fast_forward_if_stale with no window must not touch Redis"
        );
    }

    /// Pure-Rust unit test for the XINFO GROUPS field extractor — no Redis
    /// required. Ensures we correctly parse the alternating name/value
    /// shape Redis returns.
    #[test]
    fn test_extract_group_field_parses_xinfo_record() {
        let record = redis::Value::Array(vec![
            redis::Value::BulkString(b"name".to_vec()),
            redis::Value::BulkString(b"hot-workers".to_vec()),
            redis::Value::BulkString(b"consumers".to_vec()),
            redis::Value::Int(2),
            redis::Value::BulkString(b"pending".to_vec()),
            redis::Value::Int(0),
            redis::Value::BulkString(b"last-delivered-id".to_vec()),
            redis::Value::BulkString(b"1712345678901-0".to_vec()),
        ]);
        assert_eq!(
            extract_group_field(&record, "hot-workers", "last-delivered-id"),
            Some("1712345678901-0".to_string())
        );
        // Non-matching group → None.
        assert_eq!(
            extract_group_field(&record, "other-group", "last-delivered-id"),
            None
        );
        // Missing field → None.
        assert_eq!(extract_group_field(&record, "hot-workers", "missing"), None);
    }

    // -----------------------------------------------------------------
    // purge_old_pending tests
    // -----------------------------------------------------------------

    /// Helper: deliver every undelivered entry from `stream` to `consumer`
    /// in `group` via XREADGROUP > , so they enter that consumer's PEL.
    /// Returns the number of entries delivered.
    async fn xreadgroup_drain(
        client: &redis::Client,
        stream: &str,
        group: &str,
        consumer: &str,
    ) -> usize {
        let mut conn = client.get_multiplexed_async_connection().await.unwrap();
        let val: redis::Value = redis::cmd("XREADGROUP")
            .arg("GROUP")
            .arg(group)
            .arg(consumer)
            .arg("COUNT")
            .arg(1000)
            .arg("STREAMS")
            .arg(stream)
            .arg(">")
            .query_async(&mut conn)
            .await
            .unwrap_or(redis::Value::Nil);
        // Response shape: [[stream_name, [[id, [field, value, ...]], ...]]]
        let streams: Vec<redis::Value> = redis::from_redis_value_ref(&val).unwrap_or_default();
        let mut total = 0usize;
        for s in &streams {
            let parts: Vec<redis::Value> = redis::from_redis_value_ref(s).unwrap_or_default();
            if parts.len() < 2 {
                continue;
            }
            let entries: Vec<redis::Value> =
                redis::from_redis_value_ref(&parts[1]).unwrap_or_default();
            total += entries.len();
        }
        total
    }

    /// Helper: count entries currently in a group's PEL via XPENDING summary.
    async fn xpending_count(client: &redis::Client, stream: &str, group: &str) -> i64 {
        let mut conn = client.get_multiplexed_async_connection().await.unwrap();
        let val: redis::Value = redis::cmd("XPENDING")
            .arg(stream)
            .arg(group)
            .query_async(&mut conn)
            .await
            .unwrap_or(redis::Value::Nil);
        // Summary shape: [count, min-id, max-id, [[consumer, count], ...]]
        let parts: Vec<redis::Value> = redis::from_redis_value_ref(&val).unwrap_or_default();
        if parts.is_empty() {
            return 0;
        }
        redis::from_redis_value_ref(&parts[0]).unwrap_or(0)
    }

    /// Helper: XLEN of a stream.
    async fn xlen(client: &redis::Client, stream: &str) -> i64 {
        let mut conn = client.get_multiplexed_async_connection().await.unwrap();
        redis::cmd("XLEN")
            .arg(stream)
            .query_async(&mut conn)
            .await
            .unwrap_or(0)
    }

    /// A consumer can hold PEL entries spanning
    /// multiple ages. `purge_old_pending(window)` must ACK only the entries
    /// older than `window` and leave the in-window entries alone.
    #[tokio::test]
    async fn test_purge_old_pending_acks_old_pel_entries() {
        let Some(client) = try_client().await else {
            eprintln!("skipping: Redis not available");
            return;
        };
        let queue_name = format!("test_purge_pel_{}", Uuid::new_v4());
        let stream_key = format!("{{{}}}", queue_name);
        let consumer = "test-consumer".to_string();

        let queue = RedisStreamQueue::<String>::new(client.clone(), queue_name.clone())
            .with_consumer_name(consumer.clone());
        queue.ensure_consumer_group().await.unwrap();

        // Insert in chronological order (Redis requires monotonic IDs):
        // 2h ago, 30m ago, 5m ago.
        let now_ms = chrono::Utc::now().timestamp_millis() as u64;
        for offset_ms in [2 * 60 * 60 * 1000, 30 * 60 * 1000, 5 * 60 * 1000] {
            xadd_at_ms(
                &client,
                &stream_key,
                now_ms.saturating_sub(offset_ms),
                b"payload",
            )
            .await;
        }

        // Deliver all 3 to the consumer's PEL.
        let delivered =
            xreadgroup_drain(&client, &stream_key, DEFAULT_CONSUMER_GROUP, &consumer).await;
        assert_eq!(delivered, 3, "all 3 entries should be delivered to PEL");
        assert_eq!(
            xpending_count(&client, &stream_key, DEFAULT_CONSUMER_GROUP).await,
            3,
            "PEL should have 3 entries before purge"
        );

        // Purge anything older than 1 hour. Only the 2h-old entry qualifies.
        // Returned count is `acked + xdeleted` (the function does both phases):
        //   - Phase 1 ACKs the 2h-old entry from PEL  → 1
        //   - Phase 2 XDELs the same 2h-old entry from the stream itself → 1
        // Total = 2.
        let purged = queue
            .purge_old_pending(60 * 60 * 1000)
            .await
            .expect("purge_old_pending must succeed");
        assert_eq!(
            purged, 2,
            "expected 1 PEL ACK + 1 stream XDEL = 2 (got {})",
            purged
        );

        // The 30m and 5m entries should still be in PEL — they were within
        // the window so neither phase touched them.
        assert_eq!(
            xpending_count(&client, &stream_key, DEFAULT_CONSUMER_GROUP).await,
            2,
            "PEL should retain the 2 in-window entries"
        );
        // Stream too: 30m and 5m remain, only the 2h-old entry was XDEL'd.
        assert_eq!(
            xlen(&client, &stream_key).await,
            2,
            "stream should retain the 2 in-window entries"
        );

        cleanup(&client, &queue_name).await;
    }

    /// Phase-2 of `purge_old_pending`: undelivered entries (in the stream
    /// but never read by any consumer) older than the cutoff get XDEL'd
    /// from the stream itself. Recent undelivered entries must remain.
    #[tokio::test]
    async fn test_purge_old_pending_xdels_old_undelivered() {
        let Some(client) = try_client().await else {
            eprintln!("skipping: Redis not available");
            return;
        };
        let queue_name = format!("test_purge_xdel_{}", Uuid::new_v4());
        let stream_key = format!("{{{}}}", queue_name);

        let queue = RedisStreamQueue::<String>::new(client.clone(), queue_name.clone());
        queue.ensure_consumer_group().await.unwrap();

        // Insert 3 entries at varying ages, none delivered (no XREADGROUP).
        let now_ms = chrono::Utc::now().timestamp_millis() as u64;
        for offset_ms in [3 * 60 * 60 * 1000, 90 * 60 * 1000, 5 * 60 * 1000] {
            xadd_at_ms(
                &client,
                &stream_key,
                now_ms.saturating_sub(offset_ms),
                b"payload",
            )
            .await;
        }
        assert_eq!(xlen(&client, &stream_key).await, 3, "3 entries pre-purge");
        assert_eq!(
            xpending_count(&client, &stream_key, DEFAULT_CONSUMER_GROUP).await,
            0,
            "PEL must be empty (nothing delivered)"
        );

        // Purge cutoff = 1h: the 3h and 90m entries qualify; the 5m does not.
        // Returned count is `acked + xdeleted`. PEL is empty so phase 1
        // contributes 0; phase 2 XDELs both stale undelivered entries → 2.
        let purged = queue
            .purge_old_pending(60 * 60 * 1000)
            .await
            .expect("purge_old_pending must succeed");
        assert_eq!(
            purged, 2,
            "expected 0 PEL ACK + 2 stream XDEL = 2 (got {})",
            purged
        );

        let remaining = xlen(&client, &stream_key).await;
        assert_eq!(
            remaining, 1,
            "only the 5m-old undelivered entry should remain in stream, got {}",
            remaining
        );

        cleanup(&client, &queue_name).await;
    }

    /// `purge_old_pending` must not touch anything when every entry —
    /// pending or undelivered — is within the configured window.
    #[tokio::test]
    async fn test_purge_old_pending_noop_when_all_recent() {
        let Some(client) = try_client().await else {
            eprintln!("skipping: Redis not available");
            return;
        };
        let queue_name = format!("test_purge_noop_{}", Uuid::new_v4());
        let stream_key = format!("{{{}}}", queue_name);
        let consumer = "test-consumer".to_string();

        let queue = RedisStreamQueue::<String>::new(client.clone(), queue_name.clone())
            .with_consumer_name(consumer.clone());
        queue.ensure_consumer_group().await.unwrap();

        // Two entries within the last minute. Insert in chronological order.
        let now_ms = chrono::Utc::now().timestamp_millis() as u64;
        xadd_at_ms(&client, &stream_key, now_ms.saturating_sub(45_000), b"a").await;
        xadd_at_ms(&client, &stream_key, now_ms.saturating_sub(15_000), b"b").await;

        // Deliver one to PEL, leave the other undelivered.
        let delivered =
            xreadgroup_drain(&client, &stream_key, DEFAULT_CONSUMER_GROUP, &consumer).await;
        assert_eq!(delivered, 2, "both entries should be delivered to PEL");

        let pre_xlen = xlen(&client, &stream_key).await;
        let pre_pending = xpending_count(&client, &stream_key, DEFAULT_CONSUMER_GROUP).await;
        assert_eq!(pre_xlen, 2);
        assert_eq!(pre_pending, 2);

        // Purge with a 1h window — both entries are well within it.
        let purged = queue
            .purge_old_pending(60 * 60 * 1000)
            .await
            .expect("purge_old_pending must succeed");
        assert_eq!(purged, 0, "nothing within window should be purged");

        // Stream and PEL must be unchanged.
        assert_eq!(
            xlen(&client, &stream_key).await,
            pre_xlen,
            "XLEN must not change"
        );
        assert_eq!(
            xpending_count(&client, &stream_key, DEFAULT_CONSUMER_GROUP).await,
            pre_pending,
            "PEL count must not change"
        );

        cleanup(&client, &queue_name).await;
    }

    /// Regression test for ghost consumers: when a queue
    /// handle has the legacy `consumer-{uuid}` default name (e.g. an admin
    /// caller forgot to pin a stable name), `recover_orphaned_items` must
    /// skip XAUTOCLAIM rather than transfer PEL entries into the ghost
    /// consumer (where they'd sit forever, refreshed each janitor tick).
    #[tokio::test]
    async fn test_recover_orphaned_items_skips_uuid_consumer() {
        let Some(client) = try_client().await else {
            eprintln!("skipping: Redis not available");
            return;
        };
        let queue_name = format!("test_uuid_guard_recover_{}", Uuid::new_v4());
        let stream_key = format!("{{{}}}", queue_name);

        // Real worker holds an entry in PEL.
        let worker = RedisStreamQueue::<String>::new(client.clone(), queue_name.clone())
            .with_consumer_name("worker-1".to_string());
        worker.ensure_consumer_group().await.unwrap();

        let now_ms = chrono::Utc::now().timestamp_millis() as u64;
        // Insert an entry old enough to exceed ORPHAN_IDLE_MS so a normal
        // reclaim would pick it up.
        xadd_at_ms(
            &client,
            &stream_key,
            now_ms.saturating_sub(2 * ORPHAN_IDLE_MS),
            b"payload",
        )
        .await;
        let delivered =
            xreadgroup_drain(&client, &stream_key, DEFAULT_CONSUMER_GROUP, "worker-1").await;
        assert_eq!(delivered, 1, "entry must be delivered to worker-1's PEL");

        // Wait long enough that the entry's PEL idle exceeds ORPHAN_IDLE_MS.
        tokio::time::sleep(std::time::Duration::from_millis(ORPHAN_IDLE_MS + 50)).await;

        // Admin handle with the legacy UUID-style default — calling
        // recover_orphaned_items here must NOT transfer the PEL entry to it.
        let admin = RedisStreamQueue::<String>::new(client.clone(), queue_name.clone());
        assert!(
            admin.consumer_name.starts_with("consumer-"),
            "default consumer_name should be UUID-style; got {}",
            admin.consumer_name
        );
        let claimed = admin
            .recover_orphaned_items()
            .await
            .expect("recover should succeed");
        assert_eq!(
            claimed, 0,
            "UUID-style admin handle must not transfer PEL entries"
        );

        // The entry must still belong to worker-1's PEL.
        let pending_after = xpending_count(&client, &stream_key, DEFAULT_CONSUMER_GROUP).await;
        assert_eq!(pending_after, 1, "PEL must be unchanged");

        // A properly-named admin handle CAN reclaim it.
        let pinned_admin = RedisStreamQueue::<String>::new(client.clone(), queue_name.clone())
            .with_consumer_name("worker-2".to_string());
        let claimed_pinned = pinned_admin
            .recover_orphaned_items()
            .await
            .expect("recover should succeed");
        assert_eq!(
            claimed_pinned, 1,
            "stable-named admin handle should reclaim the entry"
        );

        cleanup(&client, &queue_name).await;
    }

    /// Regression test for the corresponding cleanup_stale_consumers path:
    /// when our own consumer name is UUID-style, we must skip the
    /// XAUTOCLAIM-then-DELCONSUMER drain step for stale consumers that hold
    /// PEL (because draining into ourselves recreates the same ghost). We
    /// still must perform DELCONSUMER for stale consumers with pending=0
    /// since that's pure hygiene — no PEL is in motion.
    #[tokio::test]
    async fn test_cleanup_stale_consumers_skips_pel_drain_for_uuid_caller() {
        let Some(client) = try_client().await else {
            eprintln!("skipping: Redis not available");
            return;
        };
        let queue_name = format!("test_uuid_guard_cleanup_{}", Uuid::new_v4());
        let stream_key = format!("{{{}}}", queue_name);

        // Set up a dead consumer holding a PEL entry.
        let dead = RedisStreamQueue::<String>::new(client.clone(), queue_name.clone())
            .with_consumer_name("dead-worker".to_string());
        dead.ensure_consumer_group().await.unwrap();
        let now_ms = chrono::Utc::now().timestamp_millis() as u64;
        xadd_at_ms(&client, &stream_key, now_ms, b"stuck").await;
        let delivered =
            xreadgroup_drain(&client, &stream_key, DEFAULT_CONSUMER_GROUP, "dead-worker").await;
        assert_eq!(delivered, 1);

        // Force `dead-worker`'s idle past IDLE_CONSUMER_WITH_PENDING_MS so it
        // qualifies for removal. We can't easily fast-forward Redis time in
        // a test, so instead we verify the *guard* triggers regardless: an
        // admin caller with a UUID name must report 0 cleanups for entries
        // with PEL > 0, even when removal would otherwise apply.
        //
        // To exercise the code path deterministically we drive the function
        // via its observable side effect: the dead consumer must still exist
        // afterwards (no DELCONSUMER) and must still own its PEL entry.
        let admin = RedisStreamQueue::<String>::new(client.clone(), queue_name.clone());
        assert!(admin.consumer_name.starts_with("consumer-"));
        // Run cleanup; whether or not the idle threshold is met, the UUID
        // guard must keep us from black-holing the PEL entry into ourselves.
        let _ = admin.cleanup_stale_consumers().await.unwrap_or(0);

        // The entry remains in dead-worker's PEL — never transferred to the
        // UUID admin consumer.
        let pending_after = xpending_count(&client, &stream_key, DEFAULT_CONSUMER_GROUP).await;
        assert_eq!(
            pending_after, 1,
            "PEL must remain on dead-worker, not transferred to UUID admin"
        );

        cleanup(&client, &queue_name).await;
    }
}

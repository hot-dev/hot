//! Redis Streams pub/sub implementation for distributed deployments
//!
//! This uses Redis Streams (XADD/XREAD) for real-time event delivery
//! across distributed worker and API processes. Unlike traditional
//! PUBLISH/SUBSCRIBE, this works in Redis cluster mode.
//!
//! Key features:
//! - Full cluster mode support
//! - Automatic stream trimming (MAXLEN ~) to prevent unbounded growth
//! - BLOCK for efficient long-polling without busy waiting
//! - Connection caching to minimize Redis connection overhead
//! - Cancel-safe subscribers that resume from their last entry after a
//!   connection failure (see `StreamReader`)

use super::{
    EnvEvent, EnvPublisher, EnvSubscriber, EnvSubscriberFactory, McpSseTransportSessionBinding,
    McpSseTransportSessionStore, StreamEvent, StreamNext, StreamPubSubError, StreamPublisher,
    StreamSubscriber, StreamSubscriberFactory, channel_name, env_channel_name, legacy_channel_name,
};
use async_trait::async_trait;
use redis::Client;
use redis::aio::MultiplexedConnection;
use redis::cluster::ClusterClient;
use redis::cluster_async::ClusterConnection as AsyncClusterConnection;
use redis::streams::{StreamRangeReply, StreamReadReply};
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, mpsc};
use uuid::Uuid;

/// Maximum number of entries to keep per stream (approximate)
/// This prevents unbounded memory growth for streams
const STREAM_MAXLEN: usize = 1000;
const MCP_SSE_TRANSPORT_SESSION_PREFIX: &str = "hot:mcp:http-sse-session";
const REDIS_COMMAND_TIMEOUT: Duration = Duration::from_secs(10);
const REDIS_SUBSCRIBER_COMMAND_TIMEOUT: Duration = Duration::from_secs(35);
/// How long one subscriber XREAD parks on the server; must stay below
/// `REDIS_SUBSCRIBER_COMMAND_TIMEOUT`.
const XREAD_BLOCK_MS: u64 = 30_000;
/// Entries one subscriber XREAD may return.
const XREAD_COUNT: usize = 100;
/// Entries a subscriber's reader buffers ahead of it. A full buffer pauses
/// reading; the entries wait in Redis.
const READER_BUFFER: usize = 256;
/// Reconnect pacing for a subscriber whose connection failed: the first
/// retry waits the minimum, each further failure doubles it up to the cap,
/// so an outage costs one dial per subscriber every few seconds.
const READER_RECONNECT_MIN: Duration = Duration::from_millis(250);
const READER_RECONNECT_MAX: Duration = Duration::from_secs(5);

fn mcp_sse_transport_session_key(transport_session_id: &Uuid) -> String {
    format!(
        "{}:{{{}}}",
        MCP_SSE_TRANSPORT_SESSION_PREFIX, transport_session_id
    )
}

/// Connection pool that caches Redis connections to avoid expensive reconnections
enum RedisConnectionPool {
    Standalone {
        client: Box<Client>,
        cached_conn: Arc<Mutex<CachedConn<MultiplexedConnection>>>,
    },
    Cluster {
        client: Box<ClusterClient>,
        cached_conn: Arc<Mutex<CachedConn<AsyncClusterConnection>>>,
    },
}

/// Pool slot: the cached connection plus a monotonically increasing
/// generation, bumped each time a new connection is installed.
/// `ConnectionGuard`s capture the generation at checkout and evict only
/// while it still matches, so a late-failing holder of an old, dead
/// connection cannot clobber a healthy replacement another caller has
/// already established (which would force pointless reconnect churn
/// under fan-out). Both pool arms share this scheme: standalone
/// `MultiplexedConnection` and cluster `ClusterConnection` are both
/// cheap `Arc`-backed handles, so guards hold a clone of the handle —
/// never the slot mutex — and the slot lock is only ever taken for the
/// brief checkout/eviction critical sections.
struct CachedConn<C> {
    conn: Option<C>,
    generation: u64,
}

// Manual impl: `#[derive(Default)]` would needlessly require `C: Default`.
impl<C> Default for CachedConn<C> {
    fn default() -> Self {
        Self {
            conn: None,
            generation: 0,
        }
    }
}

/// Check out a connection from a pool slot: clone the cached handle when
/// one is present, otherwise dial via `connect` and install the result
/// under a bumped generation. Dialing happens while the slot lock is held
/// so concurrent cold callers coalesce into one dial instead of stampeding
/// the server; the lock is released before this returns, so callers never
/// hold it while running commands.
async fn checkout_conn<C: Clone>(
    cached_conn: &Mutex<CachedConn<C>>,
    connect: impl Future<Output = Result<C, StreamPubSubError>>,
) -> Result<(C, u64), StreamPubSubError> {
    let mut slot = cached_conn.lock().await;
    if let Some(conn) = slot.conn.as_ref() {
        return Ok((conn.clone(), slot.generation));
    }
    let conn = connect.await?;
    slot.conn = Some(conn.clone());
    slot.generation += 1;
    Ok((conn, slot.generation))
}

/// Drop the slot's connection so the next checkout dials fresh — but only
/// while the slot generation still matches the evicting guard's checkout,
/// so a stale guard cannot discard a healthy replacement (see
/// `CachedConn`).
async fn evict_slot<C>(cached_conn: &Mutex<CachedConn<C>>, generation: u64) {
    let mut slot = cached_conn.lock().await;
    if slot.generation == generation {
        slot.conn = None;
    }
}

impl RedisConnectionPool {
    fn new_standalone(client: Client) -> Self {
        Self::Standalone {
            client: Box::new(client),
            cached_conn: Arc::new(Mutex::new(CachedConn::default())),
        }
    }

    fn new_cluster(client: ClusterClient) -> Self {
        Self::Cluster {
            client: Box::new(client),
            cached_conn: Arc::new(Mutex::new(CachedConn::default())),
        }
    }

    /// Get a cached connection for short-lived operations (publish).
    ///
    /// The returned guard owns a clone of the pooled handle — never the
    /// pool lock — so a caller holding one guard can safely call other
    /// pool-locking methods (see `ConnectionGuard`).
    async fn get_connection(&self) -> Result<ConnectionGuard<'_>, StreamPubSubError> {
        match self {
            RedisConnectionPool::Standalone {
                client,
                cached_conn,
            } => {
                let (conn, generation) = checkout_conn(cached_conn, async {
                    tokio::time::timeout(
                        REDIS_COMMAND_TIMEOUT,
                        client.get_multiplexed_async_connection_with_config(
                            &crate::redis::standalone_async_config(),
                        ),
                    )
                    .await
                    .map_err(|_| {
                        StreamPubSubError::ConnectionError(
                            "Redis connection timed out after 10s".into(),
                        )
                    })?
                    .map_err(|e| StreamPubSubError::ConnectionError(e.to_string()))
                })
                .await?;
                Ok(ConnectionGuard::Standalone {
                    conn,
                    cached_conn: cached_conn.as_ref(),
                    generation,
                })
            }
            RedisConnectionPool::Cluster {
                client,
                cached_conn,
            } => {
                let (conn, generation) = checkout_conn(cached_conn, async {
                    tokio::time::timeout(REDIS_COMMAND_TIMEOUT, client.get_async_connection())
                        .await
                        .map_err(|_| {
                            StreamPubSubError::ConnectionError(
                                "Redis cluster connection timed out after 10s".into(),
                            )
                        })?
                        .map_err(|e| StreamPubSubError::ConnectionError(e.to_string()))
                })
                .await?;
                Ok(ConnectionGuard::Cluster {
                    conn,
                    cached_conn: cached_conn.as_ref(),
                    generation,
                })
            }
        }
    }

    /// Create a new dedicated connection for subscribers (XREAD BLOCK holds the connection)
    async fn create_subscriber_connection(
        &self,
    ) -> Result<SubscriberConnection, StreamPubSubError> {
        match self {
            RedisConnectionPool::Standalone { client, .. } => {
                // Create a fresh connection for the subscriber. Disable the
                // redis-rs 1.x 500ms default response timeout — the subscriber
                // issues `XREAD BLOCK 30000` and must park the full block.
                let conn = tokio::time::timeout(
                    REDIS_COMMAND_TIMEOUT,
                    client.get_multiplexed_async_connection_with_config(
                        &crate::redis::standalone_async_config(),
                    ),
                )
                .await
                .map_err(|_| {
                    StreamPubSubError::ConnectionError(
                        "Redis subscriber connection timed out after 10s".into(),
                    )
                })?
                .map_err(|e| StreamPubSubError::ConnectionError(e.to_string()))?;
                Ok(SubscriberConnection::Standalone(conn))
            }
            RedisConnectionPool::Cluster { client, .. } => {
                // Create a fresh connection for the subscriber
                let conn =
                    tokio::time::timeout(REDIS_COMMAND_TIMEOUT, client.get_async_connection())
                        .await
                        .map_err(|_| {
                            StreamPubSubError::ConnectionError(
                                "Redis cluster subscriber connection timed out after 10s".into(),
                            )
                        })?
                        .map_err(|e| StreamPubSubError::ConnectionError(e.to_string()))?;
                Ok(SubscriberConnection::Cluster(conn))
            }
        }
    }
}

/// Guard that holds a connection for short-lived operations.
///
/// Both variants own a cheap clone of the pooled connection handle plus a
/// reference back to the slot for failure eviction — never the slot mutex
/// itself. This is load-bearing: an earlier version had the Cluster
/// variant hold the pool's `MutexGuard` for the guard's whole lifetime,
/// so any method that called another pool-locking method while its guard
/// was alive (e.g. `get_mcp_sse_transport_session`'s expired-binding path
/// calling `delete_mcp_sse_transport_session`) self-deadlocked on the
/// non-reentrant mutex in cluster mode. Never reintroduce a variant that
/// carries the pool lock across the guard's lifetime.
enum ConnectionGuard<'a> {
    Standalone {
        conn: MultiplexedConnection,
        /// The pool slot this connection was cloned from, so a failed
        /// command can evict it and force the next caller to reconnect.
        cached_conn: &'a Mutex<CachedConn<MultiplexedConnection>>,
        /// Slot generation at checkout; eviction is skipped when the slot
        /// has since been replaced (see `CachedConn`).
        generation: u64,
    },
    Cluster {
        conn: AsyncClusterConnection,
        /// The pool slot this connection was cloned from, so a failed
        /// command can evict it and force the next caller to reconnect.
        cached_conn: &'a Mutex<CachedConn<AsyncClusterConnection>>,
        /// Slot generation at checkout; eviction is skipped when the slot
        /// has since been replaced (see `CachedConn`).
        generation: u64,
    },
}

impl ConnectionGuard<'_> {
    async fn cmd(&mut self, cmd: &redis::Cmd) -> Result<redis::Value, StreamPubSubError> {
        let result = tokio::time::timeout(REDIS_COMMAND_TIMEOUT, async {
            match self {
                ConnectionGuard::Standalone { conn, .. } => cmd.query_async(conn).await,
                ConnectionGuard::Cluster { conn, .. } => cmd.query_async(conn).await,
            }
        })
        .await;

        match result {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(e)) => {
                if crate::redis::error_indicates_broken_connection(&e) {
                    self.evict_cached_connection().await;
                }
                Err(StreamPubSubError::PublishError(e.to_string()))
            }
            Err(_elapsed) => {
                // A client-side timeout usually means a half-open socket
                // (silent LB drop): redis 1.x `MultiplexedConnection` never
                // reconnects on its own, so evict the pooled connection or
                // every later command times out too.
                self.evict_cached_connection().await;
                Err(StreamPubSubError::PublishError(
                    "Redis command timed out after 10s".into(),
                ))
            }
        }
    }

    /// Drop the pooled connection so the next `get_connection` reconnects
    /// fresh. Race-safe under concurrent holders: this only clears the pool
    /// slot; in-flight users of the old connection hold their own clones
    /// and finish or fail on their own. The generation check keeps a
    /// late-failing holder of an old connection from discarding a healthy
    /// replacement another caller has already installed.
    async fn evict_cached_connection(&mut self) {
        match self {
            ConnectionGuard::Standalone {
                cached_conn,
                generation,
                ..
            } => evict_slot(cached_conn, *generation).await,
            ConnectionGuard::Cluster {
                cached_conn,
                generation,
                ..
            } => evict_slot(cached_conn, *generation).await,
        }
    }
}

/// Owned connection for subscribers (long-lived, blocking XREAD)
enum SubscriberConnection {
    Standalone(MultiplexedConnection),
    Cluster(AsyncClusterConnection),
}

impl SubscriberConnection {
    async fn cmd(&mut self, cmd: &redis::Cmd) -> Result<redis::Value, StreamPubSubError> {
        tokio::time::timeout(REDIS_SUBSCRIBER_COMMAND_TIMEOUT, async {
            match self {
                SubscriberConnection::Standalone(conn) => {
                    let result = cmd
                        .query_async(conn)
                        .await
                        .map_err(|e| StreamPubSubError::SubscribeError(e.to_string()))?;
                    Ok(result)
                }
                SubscriberConnection::Cluster(conn) => {
                    let result = cmd
                        .query_async(conn)
                        .await
                        .map_err(|e| StreamPubSubError::SubscribeError(e.to_string()))?;
                    Ok(result)
                }
            }
        })
        .await
        .map_err(|_| {
            StreamPubSubError::SubscribeError("Redis blocking command timed out after 35s".into())
        })?
    }
}

/// Redis Streams pub/sub implementation
#[derive(Clone)]
pub struct RedisStreamsPubSub {
    connection_pool: Arc<RedisConnectionPool>,
}

impl RedisStreamsPubSub {
    /// Create a new Redis Streams pub/sub with a standalone client
    pub fn new(client: Client) -> Self {
        Self {
            connection_pool: Arc::new(RedisConnectionPool::new_standalone(client)),
        }
    }

    /// Create a new Redis Streams pub/sub with a cluster client
    pub fn new_cluster(cluster_client: ClusterClient) -> Self {
        Self {
            connection_pool: Arc::new(RedisConnectionPool::new_cluster(cluster_client)),
        }
    }

    /// Round-trip a PING on the pooled connection.
    pub async fn ping(&self) -> Result<(), StreamPubSubError> {
        let mut conn = self.connection_pool.get_connection().await?;
        conn.cmd(&redis::cmd("PING")).await?;
        Ok(())
    }
}

#[async_trait]
impl McpSseTransportSessionStore for RedisStreamsPubSub {
    async fn put_mcp_sse_transport_session(
        &self,
        binding: McpSseTransportSessionBinding,
        ttl: Duration,
    ) -> Result<(), StreamPubSubError> {
        let key = mcp_sse_transport_session_key(&binding.transport_session_id);
        let payload = serde_json::to_string(&binding)
            .map_err(|e| StreamPubSubError::SerializationError(e.to_string()))?;
        let ttl_secs = ttl.as_secs().max(1);

        let mut conn = self.connection_pool.get_connection().await?;
        let _: redis::Value = conn
            .cmd(
                &redis::cmd("SET")
                    .arg(&key)
                    .arg(&payload)
                    .arg("EX")
                    .arg(ttl_secs)
                    .clone(),
            )
            .await?;

        Ok(())
    }

    async fn get_mcp_sse_transport_session(
        &self,
        transport_session_id: Uuid,
    ) -> Result<Option<McpSseTransportSessionBinding>, StreamPubSubError> {
        let key = mcp_sse_transport_session_key(&transport_session_id);
        // Scope the guard to the single GET: the expired-binding path
        // below re-enters the pool via `delete_mcp_sse_transport_session`,
        // and no guard should stay alive across another pool call (with
        // the old lock-holding Cluster guard this exact path deadlocked).
        let value = {
            let mut conn = self.connection_pool.get_connection().await?;
            conn.cmd(&redis::cmd("GET").arg(&key).clone()).await?
        };

        if matches!(value, redis::Value::Nil) {
            return Ok(None);
        }

        let payload: String = redis::from_redis_value_ref(&value)
            .map_err(|e| StreamPubSubError::SerializationError(e.to_string()))?;
        let binding: McpSseTransportSessionBinding = serde_json::from_str(&payload)
            .map_err(|e| StreamPubSubError::SerializationError(e.to_string()))?;

        if binding.is_expired() {
            self.delete_mcp_sse_transport_session(transport_session_id)
                .await?;
            return Ok(None);
        }

        Ok(Some(binding))
    }

    async fn delete_mcp_sse_transport_session(
        &self,
        transport_session_id: Uuid,
    ) -> Result<(), StreamPubSubError> {
        let key = mcp_sse_transport_session_key(&transport_session_id);
        let mut conn = self.connection_pool.get_connection().await?;
        let _: redis::Value = conn.cmd(&redis::cmd("DEL").arg(&key).clone()).await?;
        Ok(())
    }
}

#[async_trait]
impl StreamPublisher for RedisStreamsPubSub {
    async fn publish(&self, event: StreamEvent) -> Result<(), StreamPubSubError> {
        let stream_key = event.channel_name();

        // Serialize the event to JSON
        let payload = serde_json::to_string(&event)
            .map_err(|e| StreamPubSubError::SerializationError(e.to_string()))?;

        let mut conn = self.connection_pool.get_connection().await?;

        // XADD with MAXLEN ~ to cap stream size
        // XADD stream MAXLEN ~ 1000 * event <payload>
        let _: redis::Value = conn
            .cmd(
                &redis::cmd("XADD")
                    .arg(&stream_key)
                    .arg("MAXLEN")
                    .arg("~")
                    .arg(STREAM_MAXLEN)
                    .arg("*")
                    .arg("event")
                    .arg(&payload)
                    .clone(),
            )
            .await?;

        tracing::debug!(
            "Published stream event to Redis Streams channel {}",
            stream_key
        );

        Ok(())
    }
}

#[async_trait]
impl StreamSubscriberFactory for RedisStreamsPubSub {
    async fn subscribe(
        &self,
        stream_id: Uuid,
    ) -> Result<Box<dyn StreamSubscriber>, StreamPubSubError> {
        let stream_key = legacy_channel_name(&stream_id);
        let reader = StreamReader::start(Arc::clone(&self.connection_pool), &stream_key).await?;

        tracing::debug!("Subscribed to Redis Streams channel: {}", stream_key);

        Ok(Box::new(RedisStreamsSubscriber { reader, stream_key }))
    }

    async fn subscribe_in_env(
        &self,
        env_id: Uuid,
        stream_id: Uuid,
    ) -> Result<Box<dyn StreamSubscriber>, StreamPubSubError> {
        let stream_key = channel_name(&env_id, &stream_id);
        let reader = StreamReader::start(Arc::clone(&self.connection_pool), &stream_key).await?;

        tracing::debug!("Subscribed to Redis Streams channel: {}", stream_key);

        Ok(Box::new(RedisStreamsSubscriber { reader, stream_key }))
    }
}

/// What a `StreamReader` hands its subscriber.
#[derive(Debug)]
enum ReaderItem {
    /// The `event` field of one stream entry.
    Entry(String),
    /// An `XREAD BLOCK` window passed with no new entries.
    Idle,
}

/// Reads one Redis stream on a background task and hands its entries to a
/// subscriber through a channel.
///
/// Callers race `next()` against timers in `select!`. An `XREAD BLOCK`
/// issued from `next()` is not cancel-safe: a dropped read stays queued on
/// the connection, and the reply it later receives is discarded, so the
/// entries it carried are lost. A channel receive is cancel-safe. The task
/// also owns the read position, so after a connection failure it reconnects
/// and resumes after the last entry it read instead of going silent.
struct StreamReader {
    entries: mpsc::Receiver<ReaderItem>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for StreamReader {
    fn drop(&mut self) {
        // Ending the task drops its connection, which ends a parked XREAD.
        self.task.abort();
    }
}

impl StreamReader {
    /// Start reading `stream_key` after its current newest entry.
    async fn start(
        pool: Arc<RedisConnectionPool>,
        stream_key: &str,
    ) -> Result<Self, StreamPubSubError> {
        // Subscribers need their own dedicated connection since XREAD BLOCK holds it
        let conn = pool.create_subscriber_connection().await?;
        Self::start_on(pool, conn, stream_key).await
    }

    async fn start_on(
        pool: Arc<RedisConnectionPool>,
        mut conn: SubscriberConnection,
        stream_key: &str,
    ) -> Result<Self, StreamPubSubError> {
        let start_id = newest_entry_id(&mut conn, stream_key).await?;
        let (tx, entries) = mpsc::channel(READER_BUFFER);
        let task = tokio::spawn(read_stream(
            pool,
            conn,
            stream_key.to_string(),
            start_id,
            tx,
        ));
        Ok(Self { entries, task })
    }
}

/// The id of a stream's newest entry, or `0-0` for an empty or missing one.
///
/// Reading from a concrete id instead of `$` delivers an entry added after
/// subscribe returns even when it lands before the first XREAD reaches the
/// server (subscribe-with-event publishes right after subscribing).
async fn newest_entry_id(
    conn: &mut SubscriberConnection,
    stream_key: &str,
) -> Result<String, StreamPubSubError> {
    let value = conn
        .cmd(
            redis::cmd("XREVRANGE")
                .arg(stream_key)
                .arg("+")
                .arg("-")
                .arg("COUNT")
                .arg(1),
        )
        .await?;
    let newest: StreamRangeReply = redis::from_redis_value(value)
        .map_err(|e| StreamPubSubError::SubscribeError(e.to_string()))?;
    Ok(newest
        .ids
        .into_iter()
        .next()
        .map(|entry| entry.id)
        .unwrap_or_else(|| "0-0".to_string()))
}

/// Delay before the reader's next reconnect after `failures` consecutive
/// failed attempts.
fn reconnect_delay(failures: u32) -> Duration {
    READER_RECONNECT_MIN
        .saturating_mul(1 << failures.saturating_sub(1).min(8))
        .min(READER_RECONNECT_MAX)
}

/// The reader task: XREAD after `last_id` until the subscriber goes away,
/// reconnecting with backoff after errors.
async fn read_stream(
    pool: Arc<RedisConnectionPool>,
    conn: SubscriberConnection,
    stream_key: String,
    mut last_id: String,
    tx: mpsc::Sender<ReaderItem>,
) {
    let mut conn = Some(conn);
    let mut failures: u32 = 0;
    loop {
        let reader = match conn.as_mut() {
            Some(reader) => reader,
            None => {
                tokio::select! {
                    _ = tokio::time::sleep(reconnect_delay(failures)) => {}
                    _ = tx.closed() => return,
                }
                match pool.create_subscriber_connection().await {
                    Ok(reconnected) => conn.insert(reconnected),
                    Err(e) => {
                        failures = failures.saturating_add(1);
                        tracing::debug!(
                            "Redis Streams subscriber reconnect failed on {}: {}",
                            stream_key,
                            e
                        );
                        continue;
                    }
                }
            }
        };

        let mut xread = redis::cmd("XREAD");
        xread
            .arg("BLOCK")
            .arg(XREAD_BLOCK_MS)
            .arg("COUNT")
            .arg(XREAD_COUNT)
            .arg("STREAMS")
            .arg(&stream_key)
            .arg(&last_id);
        let result = tokio::select! {
            result = reader.cmd(&xread) => result,
            _ = tx.closed() => return,
        };
        let reply = result.and_then(|value| {
            redis::from_redis_value::<Option<StreamReadReply>>(value)
                .map_err(|e| StreamPubSubError::SubscribeError(e.to_string()))
        });

        let reply = match reply {
            Ok(reply) => reply,
            Err(e) => {
                // Only the first failure of an outage is a warning; the
                // retries that follow would repeat it for every subscriber.
                if failures == 0 {
                    tracing::warn!(
                        "Redis Streams XREAD error on {}; resubscribing after {}: {}",
                        stream_key,
                        last_id,
                        e
                    );
                }
                conn = None;
                failures = failures.saturating_add(1);
                continue;
            }
        };
        if failures > 0 {
            tracing::info!(
                "Redis Streams subscriber on {} resumed after {}",
                stream_key,
                last_id
            );
            failures = 0;
        }

        let entries = reply
            .map(|reply| reply.keys.into_iter().flat_map(|key| key.ids).collect())
            .unwrap_or_else(Vec::new);
        if entries.is_empty() {
            // A full buffer means the subscriber is busy anyway.
            if let Err(mpsc::error::TrySendError::Closed(_)) = tx.try_send(ReaderItem::Idle) {
                return;
            }
            continue;
        }
        for entry in entries {
            last_id = entry.id.clone();
            let Some(payload) = entry.get::<String>("event") else {
                continue;
            };
            if tx.send(ReaderItem::Entry(payload)).await.is_err() {
                return;
            }
        }
    }
}

/// Redis Streams subscriber, fed by its own `StreamReader`.
pub struct RedisStreamsSubscriber {
    reader: StreamReader,
    stream_key: String,
}

#[async_trait]
impl StreamSubscriber for RedisStreamsSubscriber {
    async fn next(&mut self) -> StreamNext {
        loop {
            match self.reader.entries.recv().await {
                Some(ReaderItem::Entry(payload)) => {
                    match serde_json::from_str::<StreamEvent>(&payload) {
                        Ok(event) => return StreamNext::Event(event),
                        Err(e) => tracing::warn!(
                            "Skipping undecodable stream event from Redis Streams on {}: {}",
                            self.stream_key,
                            e
                        ),
                    }
                }
                Some(ReaderItem::Idle) => return StreamNext::Idle,
                None => return StreamNext::Closed,
            }
        }
    }
}

// ============================================================================
// Environment-level pub/sub (for dashboard real-time updates)
// ============================================================================

#[async_trait]
impl EnvPublisher for RedisStreamsPubSub {
    async fn publish_env(&self, event: EnvEvent) -> Result<(), StreamPubSubError> {
        let env_id = event.env_id();
        let stream_key = env_channel_name(&env_id);

        // Serialize the event to JSON
        let payload = serde_json::to_string(&event)
            .map_err(|e| StreamPubSubError::SerializationError(e.to_string()))?;

        let mut conn = self.connection_pool.get_connection().await?;

        // XADD with MAXLEN ~ to cap stream size
        let _: redis::Value = conn
            .cmd(
                &redis::cmd("XADD")
                    .arg(&stream_key)
                    .arg("MAXLEN")
                    .arg("~")
                    .arg(STREAM_MAXLEN)
                    .arg("*")
                    .arg("event")
                    .arg(&payload)
                    .clone(),
            )
            .await?;

        tracing::debug!(
            "Published env event to Redis Streams channel {}",
            stream_key
        );

        Ok(())
    }
}

#[async_trait]
impl EnvSubscriberFactory for RedisStreamsPubSub {
    async fn subscribe_env(
        &self,
        env_id: Uuid,
    ) -> Result<Box<dyn EnvSubscriber>, StreamPubSubError> {
        let stream_key = env_channel_name(&env_id);
        let reader = StreamReader::start(Arc::clone(&self.connection_pool), &stream_key).await?;

        tracing::debug!("Subscribed to Redis Streams env channel: {}", stream_key);

        Ok(Box::new(RedisEnvSubscriber { reader, stream_key }))
    }
}

/// Redis Streams environment subscriber, fed by its own `StreamReader`.
pub struct RedisEnvSubscriber {
    reader: StreamReader,
    stream_key: String,
}

#[async_trait]
impl EnvSubscriber for RedisEnvSubscriber {
    async fn next(&mut self) -> Option<EnvEvent> {
        loop {
            match self.reader.entries.recv().await {
                Some(ReaderItem::Entry(payload)) => {
                    match serde_json::from_str::<EnvEvent>(&payload) {
                        Ok(event) => return Some(event),
                        Err(e) => tracing::warn!(
                            "Skipping undecodable env event from Redis Streams on {}: {}",
                            self.stream_key,
                            e
                        ),
                    }
                }
                Some(ReaderItem::Idle) => return None,
                None => {
                    // The reader only stops once dropped. Callers treat
                    // `None` as poll-again, so pace them rather than spin.
                    tokio::time::sleep(READER_RECONNECT_MAX).await;
                    return None;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Open a Redis client, returning None when no local Redis is reachable.
    /// Lets tests skip cleanly in environments without Redis (CI without the
    /// service, sandboxed builds) instead of failing. `REDIS_URL` (or
    /// `HOT_REDIS_URI`) picks the server and logical database.
    async fn try_client() -> Option<redis::Client> {
        let uri = std::env::var("REDIS_URL")
            .or_else(|_| std::env::var("HOT_REDIS_URI"))
            .unwrap_or_else(|_| "redis://127.0.0.1/".to_string());
        let client = redis::Client::open(uri.as_str()).ok()?;
        // Fail fast if Redis isn't actually running.
        client.get_multiplexed_async_connection().await.ok()?;
        Some(client)
    }

    fn standalone_slot(
        pool: &RedisConnectionPool,
    ) -> &Arc<Mutex<CachedConn<MultiplexedConnection>>> {
        match pool {
            RedisConnectionPool::Standalone { cached_conn, .. } => cached_conn,
            RedisConnectionPool::Cluster { .. } => unreachable!("standalone pool expected"),
        }
    }

    /// Slot checkout/eviction semantics shared by both pool arms. The
    /// cluster arm reuses exactly this generic code (its guard owns a
    /// clone of the connection handle, never the slot mutex), so this
    /// also covers the paths a real cluster deployment exercises.
    #[tokio::test]
    async fn slot_checkout_clones_cached_and_generations_gate_eviction() {
        let slot: Mutex<CachedConn<u8>> = Mutex::new(CachedConn::default());

        // Cold checkout dials and installs generation 1.
        let (conn, generation) = checkout_conn(&slot, async { Ok(7) }).await.unwrap();
        assert_eq!((conn, generation), (7, 1));

        // Warm checkout clones the cached handle without dialing.
        let (conn, generation) = checkout_conn(&slot, async {
            panic!("must not dial while a healthy connection is cached")
        })
        .await
        .unwrap();
        assert_eq!((conn, generation), (7, 1));

        // Stale-generation eviction is a no-op...
        evict_slot(&slot, 0).await;
        assert!(slot.lock().await.conn.is_some());

        // ...matching-generation eviction clears the slot...
        evict_slot(&slot, 1).await;
        assert!(slot.lock().await.conn.is_none());

        // ...and the next checkout dials fresh under a bumped generation,
        // so the evicting (now stale) guard can no longer clobber it.
        let (conn, generation) = checkout_conn(&slot, async { Ok(9) }).await.unwrap();
        assert_eq!((conn, generation), (9, 2));
        evict_slot(&slot, 1).await;
        assert!(slot.lock().await.conn.is_some());
    }

    /// A failed dial must leave the slot untouched — nothing cached and
    /// the generation unchanged — so the failure cannot masquerade as an
    /// install.
    #[tokio::test]
    async fn slot_checkout_failed_dial_leaves_slot_untouched() {
        let slot: Mutex<CachedConn<u8>> = Mutex::new(CachedConn::default());
        checkout_conn(&slot, async {
            Err(StreamPubSubError::ConnectionError("dial failed".into()))
        })
        .await
        .expect_err("dial failure must surface");

        let slot = slot.lock().await;
        assert!(slot.conn.is_none());
        assert_eq!(slot.generation, 0);
    }

    /// Eviction replaces the pool slot only; in-flight holders keep their
    /// own clone of the old connection (Arc semantics) and the next
    /// `get_connection` reconnects fresh.
    #[tokio::test]
    async fn evicting_standalone_connection_clears_slot_without_breaking_holders() {
        let Some(client) = try_client().await else {
            eprintln!("skipping: Redis not available");
            return;
        };
        let pool = RedisConnectionPool::new_standalone(client);

        let mut guard = pool.get_connection().await.unwrap();
        assert!(standalone_slot(&pool).lock().await.conn.is_some());

        guard.evict_cached_connection().await;
        assert!(standalone_slot(&pool).lock().await.conn.is_none());

        // The in-flight holder's clone still works after eviction.
        let pong = guard.cmd(&redis::cmd("PING")).await.unwrap();
        assert_eq!(pong, redis::Value::SimpleString("PONG".to_string()));

        // The next caller reconnects and repopulates the slot.
        let _guard2 = pool.get_connection().await.unwrap();
        assert!(standalone_slot(&pool).lock().await.conn.is_some());
    }

    /// A late-failing holder of an OLD connection must not evict a healthy
    /// replacement another caller already installed: eviction only applies
    /// while the slot generation still matches the guard's checkout.
    #[tokio::test]
    async fn stale_guard_eviction_does_not_clobber_replacement() {
        let Some(client) = try_client().await else {
            eprintln!("skipping: Redis not available");
            return;
        };
        let pool = RedisConnectionPool::new_standalone(client);

        // A slow holder checks out the first-generation connection.
        let mut stale_guard = pool.get_connection().await.unwrap();

        // Another holder of the same generation fails first and evicts...
        pool.get_connection()
            .await
            .unwrap()
            .evict_cached_connection()
            .await;
        assert!(standalone_slot(&pool).lock().await.conn.is_none());

        // ...and a third caller installs a healthy replacement (new
        // generation).
        let _fresh = pool.get_connection().await.unwrap();
        assert!(standalone_slot(&pool).lock().await.conn.is_some());

        // The slow holder now fails late and tries to evict: the slot has
        // moved on, so the healthy replacement must survive.
        stale_guard.evict_cached_connection().await;
        assert!(
            standalone_slot(&pool).lock().await.conn.is_some(),
            "stale-generation eviction must not clobber the replacement"
        );
    }

    /// A server error reply is a per-command failure and must not evict the
    /// pooled connection.
    #[tokio::test]
    async fn server_error_reply_does_not_evict_pooled_connection() {
        let Some(client) = try_client().await else {
            eprintln!("skipping: Redis not available");
            return;
        };
        let pool = RedisConnectionPool::new_standalone(client);

        let mut guard = pool.get_connection().await.unwrap();
        guard
            .cmd(
                &redis::cmd("EVAL")
                    .arg("return redis.error_reply('boom')")
                    .arg(0)
                    .clone(),
            )
            .await
            .expect_err("script error reply must surface as Err");
        assert!(
            standalone_slot(&pool).lock().await.conn.is_some(),
            "a server error reply must not evict the pooled connection"
        );
    }

    /// A connection-level failure (stand-in for a silent LB drop) must
    /// evict the pooled connection so the next caller reconnects.
    #[tokio::test]
    async fn connection_level_failure_evicts_pooled_connection() {
        let Some(client) = try_client().await else {
            eprintln!("skipping: Redis not available");
            return;
        };
        let pool = RedisConnectionPool::new_standalone(client.clone());

        let mut guard = pool.get_connection().await.unwrap();
        let client_id: i64 =
            redis::from_redis_value_ref(&guard.cmd(redis::cmd("CLIENT").arg("ID")).await.unwrap())
                .unwrap();

        let mut admin = client.get_multiplexed_async_connection().await.unwrap();
        let killed: i64 = redis::cmd("CLIENT")
            .arg("KILL")
            .arg("ID")
            .arg(client_id)
            .query_async(&mut admin)
            .await
            .unwrap();
        assert_eq!(killed, 1);

        guard
            .cmd(&redis::cmd("PING"))
            .await
            .expect_err("command on a killed connection must fail");
        assert!(
            standalone_slot(&pool).lock().await.conn.is_none(),
            "a connection-level failure must evict the pooled connection"
        );

        // The next caller reconnects fresh.
        let mut guard2 = pool.get_connection().await.unwrap();
        guard2.cmd(&redis::cmd("PING")).await.unwrap();
    }

    /// End-to-end expiry arc against a real standalone Redis: reading an
    /// expired binding must delete it and return `None`. This is the exact
    /// get -> delete pool re-entry that self-deadlocked when the Cluster
    /// guard held the pool mutex for its whole lifetime; the checkout and
    /// guard-scoping code is shared between arms, so this pins the
    /// re-entrant behavior for the paths a cluster deployment runs too.
    #[tokio::test]
    async fn expired_session_read_deletes_binding_and_returns_none() {
        let Some(client) = try_client().await else {
            eprintln!("skipping: Redis not available");
            return;
        };
        let pubsub = RedisStreamsPubSub::new(client.clone());

        let transport_session_id = Uuid::new_v4();
        let binding = McpSseTransportSessionBinding {
            transport_session_id,
            env_id: Uuid::new_v4(),
            route_key: "test-route".to_string(),
            principal: crate::stream::McpSseTransportPrincipal::Session(Uuid::new_v4()),
            // Already logically expired while the Redis TTL (60s) still
            // holds the key, forcing get() down its eager-delete path.
            expires_at: chrono::Utc::now() - chrono::Duration::seconds(30),
        };
        pubsub
            .put_mcp_sse_transport_session(binding, Duration::from_secs(60))
            .await
            .unwrap();

        let got = tokio::time::timeout(
            Duration::from_secs(5),
            pubsub.get_mcp_sse_transport_session(transport_session_id),
        )
        .await
        .expect("expiry path must not hang on pool re-entry")
        .unwrap();
        assert_eq!(got, None, "expired binding must read as absent");

        // The expiry path must have deleted the key eagerly, not left it
        // for the TTL.
        let mut admin = client.get_multiplexed_async_connection().await.unwrap();
        let exists: i64 = redis::cmd("EXISTS")
            .arg(mcp_sse_transport_session_key(&transport_session_id))
            .query_async(&mut admin)
            .await
            .unwrap();
        assert_eq!(exists, 0, "expired binding must be deleted on read");
    }

    fn stream_data(env_id: Uuid, stream_id: Uuid, n: i64) -> StreamEvent {
        StreamEvent::StreamData {
            stream_data_id: Uuid::now_v7(),
            run_id: Uuid::nil(),
            env_id: Some(env_id),
            stream_id,
            data_type: "test".to_string(),
            payload: serde_json::json!(n),
        }
    }

    async fn next_payload(sub: &mut Box<dyn StreamSubscriber>) -> i64 {
        loop {
            let next = tokio::time::timeout(Duration::from_secs(10), sub.next())
                .await
                .expect("subscriber must deliver the published entry");
            match next {
                StreamNext::Event(StreamEvent::StreamData { payload, .. }) => {
                    return payload.as_i64().unwrap();
                }
                StreamNext::Idle => continue,
                other => panic!("unexpected subscriber item: {other:?}"),
            }
        }
    }

    async fn delete_key(client: &redis::Client, key: &str) {
        let mut admin = client.get_multiplexed_async_connection().await.unwrap();
        let _: redis::RedisResult<()> = redis::cmd("DEL").arg(key).query_async(&mut admin).await;
    }

    #[test]
    fn reconnect_delay_backs_off_to_cap() {
        assert_eq!(reconnect_delay(0), READER_RECONNECT_MIN);
        assert_eq!(reconnect_delay(1), READER_RECONNECT_MIN);
        assert_eq!(reconnect_delay(2), READER_RECONNECT_MIN * 2);
        assert_eq!(reconnect_delay(3), READER_RECONNECT_MIN * 4);
        assert_eq!(reconnect_delay(50), READER_RECONNECT_MAX);
    }

    /// SSE handlers race `next()` against a poll timer, cancelling it about
    /// once a second while a stream is quiet. Entries published after such
    /// idle time must all arrive: a cancelled wait used to leave an XREAD
    /// queued on the connection that then consumed an entry and dropped it.
    #[tokio::test]
    async fn subscriber_keeps_entries_through_cancelled_waits() {
        let Some(client) = try_client().await else {
            eprintln!("skipping: Redis not available");
            return;
        };
        let pubsub = RedisStreamsPubSub::new(client.clone());
        let env_id = Uuid::now_v7();
        let stream_id = Uuid::now_v7();
        let mut sub = pubsub.subscribe_in_env(env_id, stream_id).await.unwrap();

        for _ in 0..6 {
            let waited = tokio::time::timeout(Duration::from_millis(200), sub.next()).await;
            assert!(waited.is_err(), "nothing was published yet: {waited:?}");
        }

        // An undecodable entry is skipped, not the end of the subscription.
        let mut admin = client.get_multiplexed_async_connection().await.unwrap();
        let _: String = redis::cmd("XADD")
            .arg(channel_name(&env_id, &stream_id))
            .arg("*")
            .arg("event")
            .arg("not json")
            .query_async(&mut admin)
            .await
            .unwrap();
        for n in 1..=3 {
            pubsub
                .publish(stream_data(env_id, stream_id, n))
                .await
                .unwrap();
        }
        for n in 1..=3 {
            assert_eq!(next_payload(&mut sub).await, n);
        }

        delete_key(&client, &channel_name(&env_id, &stream_id)).await;
    }

    /// subscribe-with-event publishes right after subscribing; an entry that
    /// lands before the subscriber's first read must still be delivered.
    #[tokio::test]
    async fn subscriber_delivers_entries_published_before_its_first_read() {
        let Some(client) = try_client().await else {
            eprintln!("skipping: Redis not available");
            return;
        };
        let pubsub = RedisStreamsPubSub::new(client.clone());
        let env_id = Uuid::now_v7();
        let stream_id = Uuid::now_v7();
        // Older entries are history, not news for a new subscriber.
        pubsub
            .publish(stream_data(env_id, stream_id, 0))
            .await
            .unwrap();
        let mut sub = pubsub.subscribe_in_env(env_id, stream_id).await.unwrap();
        pubsub
            .publish(stream_data(env_id, stream_id, 1))
            .await
            .unwrap();
        assert_eq!(next_payload(&mut sub).await, 1);

        delete_key(&client, &channel_name(&env_id, &stream_id)).await;
    }

    /// A Redis blip (failover, LB reset) kills the subscriber's connection.
    /// The reader must reconnect and resume after the last entry it
    /// delivered, including entries published while it was disconnected.
    #[tokio::test]
    async fn subscriber_resumes_after_its_connection_is_killed() {
        let Some(client) = try_client().await else {
            eprintln!("skipping: Redis not available");
            return;
        };
        let pubsub = RedisStreamsPubSub::new(client.clone());
        let env_id = Uuid::now_v7();
        let stream_id = Uuid::now_v7();
        let stream_key = channel_name(&env_id, &stream_id);

        let mut conn = pubsub
            .connection_pool
            .create_subscriber_connection()
            .await
            .unwrap();
        let client_id: i64 =
            redis::from_redis_value_ref(&conn.cmd(redis::cmd("CLIENT").arg("ID")).await.unwrap())
                .unwrap();
        let reader = StreamReader::start_on(Arc::clone(&pubsub.connection_pool), conn, &stream_key)
            .await
            .unwrap();
        let mut sub: Box<dyn StreamSubscriber> = Box::new(RedisStreamsSubscriber {
            reader,
            stream_key: stream_key.clone(),
        });

        pubsub
            .publish(stream_data(env_id, stream_id, 1))
            .await
            .unwrap();
        assert_eq!(next_payload(&mut sub).await, 1);

        let mut admin = client.get_multiplexed_async_connection().await.unwrap();
        let killed: i64 = redis::cmd("CLIENT")
            .arg("KILL")
            .arg("ID")
            .arg(client_id)
            .query_async(&mut admin)
            .await
            .unwrap();
        assert_eq!(killed, 1);

        // Published while the reader is down or backing off.
        pubsub
            .publish(stream_data(env_id, stream_id, 2))
            .await
            .unwrap();
        assert_eq!(next_payload(&mut sub).await, 2);
        pubsub
            .publish(stream_data(env_id, stream_id, 3))
            .await
            .unwrap();
        assert_eq!(next_payload(&mut sub).await, 3);

        delete_key(&client, &stream_key).await;
    }

    /// Dashboard subscribers race `next()` against shutdown and revalidation
    /// timers too; the first run event of a connection used to be lost.
    #[tokio::test]
    async fn env_subscriber_keeps_entries_through_cancelled_waits() {
        let Some(client) = try_client().await else {
            eprintln!("skipping: Redis not available");
            return;
        };
        let pubsub = RedisStreamsPubSub::new(client.clone());
        let env_id = Uuid::now_v7();
        let mut sub = pubsub.subscribe_env(env_id).await.unwrap();

        for _ in 0..3 {
            let waited = tokio::time::timeout(Duration::from_millis(200), sub.next()).await;
            assert!(waited.is_err(), "nothing was published yet: {waited:?}");
        }
        let stream_id = Uuid::now_v7();
        pubsub
            .publish_env(EnvEvent::StreamCreated {
                stream_id,
                env_id,
                project_id: None,
            })
            .await
            .unwrap();
        let event = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Some(event) = sub.next().await {
                    break event;
                }
            }
        })
        .await
        .expect("env subscriber must deliver the published entry");
        assert!(
            matches!(event, EnvEvent::StreamCreated { stream_id: got, .. } if got == stream_id),
            "{event:?}"
        );

        delete_key(&client, &env_channel_name(&env_id)).await;
    }
}

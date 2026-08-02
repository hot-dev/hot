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
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use uuid::Uuid;

/// Maximum number of entries to keep per stream (approximate)
/// This prevents unbounded memory growth for streams
const STREAM_MAXLEN: usize = 1000;
const MCP_SSE_TRANSPORT_SESSION_PREFIX: &str = "hot:mcp:http-sse-session";
const REDIS_COMMAND_TIMEOUT: Duration = Duration::from_secs(10);
const REDIS_SUBSCRIBER_COMMAND_TIMEOUT: Duration = Duration::from_secs(35);
/// Minimum spacing between env-subscriber reconnect attempts. The SSE
/// caller treats a `None` from `next()` as poll-again and loops, so
/// without pacing a Redis outage becomes hundreds of dial attempts (and
/// warn lines) per second per client. The first failure stays fast; only
/// redials that follow a recent failure wait out the remainder.
const ENV_SUBSCRIBER_RECONNECT_BACKOFF: Duration = Duration::from_secs(1);

fn mcp_sse_transport_session_key(transport_session_id: &Uuid) -> String {
    format!(
        "{}:{{{}}}",
        MCP_SSE_TRANSPORT_SESSION_PREFIX, transport_session_id
    )
}

/// Connection pool that caches Redis connections to avoid expensive reconnections
enum RedisConnectionPool {
    Standalone {
        client: Client,
        cached_conn: Arc<Mutex<CachedStandaloneConn>>,
    },
    Cluster {
        client: ClusterClient,
        cached_conn: Arc<Mutex<Option<AsyncClusterConnection>>>,
    },
}

/// Standalone pool slot: the cached connection plus a monotonically
/// increasing generation, bumped each time a new connection is installed.
/// `ConnectionGuard`s capture the generation at checkout and evict only
/// while it still matches, so a late-failing holder of an old, dead
/// connection cannot clobber a healthy replacement another caller has
/// already established (which would force pointless reconnect churn
/// under fan-out). The Cluster arm needs no generation: its guard holds
/// the slot mutex for its whole lifetime, so no replacement can
/// interleave between checkout and eviction.
#[derive(Default)]
struct CachedStandaloneConn {
    conn: Option<MultiplexedConnection>,
    generation: u64,
}

impl RedisConnectionPool {
    fn new_standalone(client: Client) -> Self {
        Self::Standalone {
            client,
            cached_conn: Arc::new(Mutex::new(CachedStandaloneConn::default())),
        }
    }

    fn new_cluster(client: ClusterClient) -> Self {
        Self::Cluster {
            client,
            cached_conn: Arc::new(Mutex::new(None)),
        }
    }

    /// Get a cached connection for short-lived operations (publish)
    async fn get_connection(&self) -> Result<ConnectionGuard<'_>, StreamPubSubError> {
        match self {
            RedisConnectionPool::Standalone {
                client,
                cached_conn,
            } => {
                let mut guard = cached_conn.lock().await;
                let (conn, generation) = if let Some(conn) = guard.conn.as_ref() {
                    (conn.clone(), guard.generation)
                } else {
                    let conn = tokio::time::timeout(
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
                    .map_err(|e| StreamPubSubError::ConnectionError(e.to_string()))?;
                    guard.conn = Some(conn.clone());
                    guard.generation += 1;
                    (conn, guard.generation)
                };
                drop(guard);
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
                let mut guard = cached_conn.lock().await;
                if guard.is_none() {
                    let conn =
                        tokio::time::timeout(REDIS_COMMAND_TIMEOUT, client.get_async_connection())
                            .await
                            .map_err(|_| {
                                StreamPubSubError::ConnectionError(
                                    "Redis cluster connection timed out after 10s".into(),
                                )
                            })?
                            .map_err(|e| StreamPubSubError::ConnectionError(e.to_string()))?;
                    *guard = Some(conn);
                }
                Ok(ConnectionGuard::Cluster(guard))
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

/// Guard that holds a connection for short-lived operations
enum ConnectionGuard<'a> {
    Standalone {
        conn: MultiplexedConnection,
        /// The pool slot this connection was cloned from, so a failed
        /// command can evict it and force the next caller to reconnect.
        cached_conn: &'a Mutex<CachedStandaloneConn>,
        /// Slot generation at checkout; eviction is skipped when the slot
        /// has since been replaced (see `CachedStandaloneConn`).
        generation: u64,
    },
    Cluster(tokio::sync::MutexGuard<'a, Option<AsyncClusterConnection>>),
}

impl ConnectionGuard<'_> {
    async fn cmd(&mut self, cmd: &redis::Cmd) -> Result<redis::Value, StreamPubSubError> {
        let result = tokio::time::timeout(REDIS_COMMAND_TIMEOUT, async {
            match self {
                ConnectionGuard::Standalone { conn, .. } => cmd.query_async(conn).await,
                ConnectionGuard::Cluster(guard) => match guard.as_mut() {
                    Some(conn) => cmd.query_async(conn).await,
                    // A previous failing command on this guard evicted the
                    // slot; surface an error instead of panicking.
                    None => Err(redis::RedisError::from((
                        redis::ErrorKind::Io,
                        "cluster connection was evicted after a previous failure",
                    ))),
                },
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
            } => {
                let mut slot = cached_conn.lock().await;
                if slot.generation == *generation {
                    slot.conn = None;
                }
            }
            ConnectionGuard::Cluster(guard) => {
                **guard = None;
            }
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
        let mut conn = self.connection_pool.get_connection().await?;
        let value = conn.cmd(&redis::cmd("GET").arg(&key).clone()).await?;

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

        // Subscribers need their own dedicated connection since XREAD BLOCK holds it
        let conn = self.connection_pool.create_subscriber_connection().await?;

        tracing::debug!("Subscribed to Redis Streams channel: {}", stream_key);

        Ok(Box::new(RedisStreamsSubscriber {
            conn,
            stream_key,
            // Start from latest - "$" means "only new messages"
            last_id: "$".to_string(),
        }))
    }

    async fn subscribe_in_env(
        &self,
        env_id: Uuid,
        stream_id: Uuid,
    ) -> Result<Box<dyn StreamSubscriber>, StreamPubSubError> {
        let stream_key = channel_name(&env_id, &stream_id);

        // Subscribers need their own dedicated connection since XREAD BLOCK holds it
        let conn = self.connection_pool.create_subscriber_connection().await?;

        tracing::debug!("Subscribed to Redis Streams channel: {}", stream_key);

        Ok(Box::new(RedisStreamsSubscriber {
            conn,
            stream_key,
            // Start from latest - "$" means "only new messages"
            last_id: "$".to_string(),
        }))
    }
}

/// Redis Streams subscriber - owns its connection since XREAD BLOCK holds it
pub struct RedisStreamsSubscriber {
    conn: SubscriberConnection,
    stream_key: String,
    last_id: String,
}

#[async_trait]
impl StreamSubscriber for RedisStreamsSubscriber {
    async fn next(&mut self) -> StreamNext {
        // XREAD BLOCK 30000 STREAMS stream last_id
        // 30 second block timeout - after which we return Idle and the caller can retry
        let result = self
            .conn
            .cmd(
                &redis::cmd("XREAD")
                    .arg("BLOCK")
                    .arg(30000) // 30 second timeout
                    .arg("COUNT")
                    .arg(1)
                    .arg("STREAMS")
                    .arg(&self.stream_key)
                    .arg(&self.last_id)
                    .clone(),
            )
            .await;

        let value = match result {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("Redis Streams XREAD error on {}: {}", self.stream_key, e);
                return StreamNext::Closed;
            }
        };

        // Parse the XREAD response
        // Format: [[stream-name, [[id, [field, value, ...]], ...]]]
        // or nil on timeout
        if matches!(value, redis::Value::Nil) {
            return StreamNext::Idle;
        }

        let streams: Vec<redis::Value> = match redis::from_redis_value_ref(&value) {
            Ok(s) => s,
            Err(_) => return StreamNext::Closed,
        };

        let stream_data: Vec<redis::Value> = match streams.first() {
            Some(s) => match redis::from_redis_value_ref(s) {
                Ok(d) => d,
                Err(_) => return StreamNext::Closed,
            },
            None => return StreamNext::Closed,
        };

        if stream_data.len() < 2 {
            return StreamNext::Closed;
        }

        let messages: Vec<redis::Value> = match redis::from_redis_value_ref(&stream_data[1]) {
            Ok(m) => m,
            Err(_) => return StreamNext::Closed,
        };

        if messages.is_empty() {
            return StreamNext::Idle;
        }

        // Get first message: [id, [field, value, ...]]
        let Some(first_message) = messages.first() else {
            return StreamNext::Idle;
        };
        let msg: Vec<redis::Value> = match redis::from_redis_value_ref(first_message) {
            Ok(m) => m,
            Err(_) => return StreamNext::Closed,
        };

        if msg.len() < 2 {
            return StreamNext::Closed;
        }

        // Extract message ID and update last_id for next read
        let msg_id: String = match redis::from_redis_value_ref(&msg[0]) {
            Ok(id) => id,
            Err(_) => return StreamNext::Closed,
        };
        self.last_id = msg_id;

        // Extract fields
        let fields: Vec<redis::Value> = match redis::from_redis_value_ref(&msg[1]) {
            Ok(f) => f,
            Err(_) => return StreamNext::Closed,
        };

        // Find the "event" field
        let mut i = 0;
        while i < fields.len() - 1 {
            let field_name: String = match redis::from_redis_value_ref(&fields[i]) {
                Ok(n) => n,
                Err(_) => {
                    i += 2;
                    continue;
                }
            };

            if field_name == "event" {
                let payload: String = match redis::from_redis_value_ref(&fields[i + 1]) {
                    Ok(p) => p,
                    Err(_) => return StreamNext::Closed,
                };

                // Deserialize the event
                match serde_json::from_str::<StreamEvent>(&payload) {
                    Ok(event) => return StreamNext::Event(event),
                    Err(e) => {
                        tracing::warn!(
                            "Failed to deserialize stream event from Redis Streams on {}: {}",
                            self.stream_key,
                            e
                        );
                        return StreamNext::Closed;
                    }
                }
            }
            i += 2;
        }

        StreamNext::Idle
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

        // Subscribers need their own dedicated connection since XREAD BLOCK holds it
        let conn = self.connection_pool.create_subscriber_connection().await?;

        tracing::debug!("Subscribed to Redis Streams env channel: {}", stream_key);

        Ok(Box::new(RedisEnvSubscriber {
            connection_pool: Arc::clone(&self.connection_pool),
            conn: Some(conn),
            stream_key,
            // Start from latest - "$" means "only new messages"
            last_id: "$".to_string(),
            last_failure: None,
        }))
    }
}

/// Redis Streams environment subscriber - owns its connection since XREAD BLOCK holds it
pub struct RedisEnvSubscriber {
    connection_pool: Arc<RedisConnectionPool>,
    /// `None` after a command failure: redis 1.x connections never reconnect
    /// on their own, so the next `next()` call creates a fresh connection
    /// instead of retrying a dead one forever (callers treat `None` results
    /// as idle and keep polling the same subscriber).
    conn: Option<SubscriberConnection>,
    stream_key: String,
    last_id: String,
    /// When the last connection-level failure happened. Redials within
    /// `ENV_SUBSCRIBER_RECONNECT_BACKOFF` of it sleep out the remainder
    /// first, so a poll-again caller can't spin dial attempts while Redis
    /// is down.
    last_failure: Option<Instant>,
}

impl RedisEnvSubscriber {
    /// Remaining wait before the next redial attempt, or `None` when the
    /// last failure is absent or old enough to dial immediately.
    fn reconnect_delay(&self) -> Option<Duration> {
        let last_failure = self.last_failure?;
        ENV_SUBSCRIBER_RECONNECT_BACKOFF
            .checked_sub(last_failure.elapsed())
            .filter(|remaining| !remaining.is_zero())
    }
}

#[async_trait]
impl EnvSubscriber for RedisEnvSubscriber {
    async fn next(&mut self) -> Option<EnvEvent> {
        let conn = match &mut self.conn {
            Some(conn) => conn,
            None => {
                if let Some(delay) = self.reconnect_delay() {
                    tokio::time::sleep(delay).await;
                }
                match self.connection_pool.create_subscriber_connection().await {
                    Ok(conn) => {
                        self.last_failure = None;
                        self.conn.insert(conn)
                    }
                    Err(e) => {
                        self.last_failure = Some(Instant::now());
                        tracing::warn!(
                            "Redis Streams env subscriber reconnect failed on {}: {}",
                            self.stream_key,
                            e
                        );
                        return None;
                    }
                }
            }
        };

        // XREAD BLOCK 30000 STREAMS stream last_id
        // 30 second block timeout - after which we return None and the caller can retry
        let result = conn
            .cmd(
                &redis::cmd("XREAD")
                    .arg("BLOCK")
                    .arg(30000) // 30 second timeout
                    .arg("COUNT")
                    .arg(1)
                    .arg("STREAMS")
                    .arg(&self.stream_key)
                    .arg(&self.last_id)
                    .clone(),
            )
            .await;

        let value = match result {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("Redis Streams XREAD error on {}: {}", self.stream_key, e);
                // Drop the (likely dead) connection; the next call
                // reconnects fresh instead of spinning on it forever.
                // Stamp the failure so that redial is paced, not immediate.
                self.conn = None;
                self.last_failure = Some(Instant::now());
                return None;
            }
        };

        // Parse the XREAD response
        if matches!(value, redis::Value::Nil) {
            // Timeout - return None to allow the caller to retry
            return None;
        }

        let streams: Vec<redis::Value> = match redis::from_redis_value_ref(&value) {
            Ok(s) => s,
            Err(_) => return None,
        };

        let stream_data: Vec<redis::Value> = match streams.first() {
            Some(s) => match redis::from_redis_value_ref(s) {
                Ok(d) => d,
                Err(_) => return None,
            },
            None => return None,
        };

        if stream_data.len() < 2 {
            return None;
        }

        let messages: Vec<redis::Value> = match redis::from_redis_value_ref(&stream_data[1]) {
            Ok(m) => m,
            Err(_) => return None,
        };

        if messages.is_empty() {
            return None;
        }

        // Get first message: [id, [field, value, ...]]
        let msg: Vec<redis::Value> = match redis::from_redis_value_ref(messages.first()?) {
            Ok(m) => m,
            Err(_) => return None,
        };

        if msg.len() < 2 {
            return None;
        }

        // Extract message ID and update last_id for next read
        let msg_id: String = match redis::from_redis_value_ref(&msg[0]) {
            Ok(id) => id,
            Err(_) => return None,
        };
        self.last_id = msg_id;

        // Extract fields
        let fields: Vec<redis::Value> = match redis::from_redis_value_ref(&msg[1]) {
            Ok(f) => f,
            Err(_) => return None,
        };

        // Find the "event" field
        let mut i = 0;
        while i < fields.len() - 1 {
            let field_name: String = match redis::from_redis_value_ref(&fields[i]) {
                Ok(n) => n,
                Err(_) => {
                    i += 2;
                    continue;
                }
            };

            if field_name == "event" {
                let payload: String = match redis::from_redis_value_ref(&fields[i + 1]) {
                    Ok(p) => p,
                    Err(_) => return None,
                };

                // Deserialize the event
                match serde_json::from_str::<EnvEvent>(&payload) {
                    Ok(event) => return Some(event),
                    Err(e) => {
                        tracing::warn!(
                            "Failed to deserialize env event from Redis Streams on {}: {}",
                            self.stream_key,
                            e
                        );
                        return None;
                    }
                }
            }
            i += 2;
        }

        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Open a Redis client, returning None when no local Redis is reachable.
    /// Lets tests skip cleanly in environments without Redis (CI without the
    /// service, sandboxed builds) instead of failing.
    async fn try_client() -> Option<redis::Client> {
        let client = redis::Client::open("redis://127.0.0.1/").ok()?;
        // Fail fast if Redis isn't actually running.
        client.get_multiplexed_async_connection().await.ok()?;
        Some(client)
    }

    fn standalone_slot(pool: &RedisConnectionPool) -> &Arc<Mutex<CachedStandaloneConn>> {
        match pool {
            RedisConnectionPool::Standalone { cached_conn, .. } => cached_conn,
            RedisConnectionPool::Cluster { .. } => unreachable!("standalone pool expected"),
        }
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

    /// After a command error the env subscriber must drop its dead
    /// connection and transparently recreate one on the next `next()` call,
    /// instead of retrying the dead connection forever.
    #[tokio::test]
    async fn env_subscriber_recreates_connection_after_error() {
        let Some(client) = try_client().await else {
            eprintln!("skipping: Redis not available");
            return;
        };
        let pool = Arc::new(RedisConnectionPool::new_standalone(client.clone()));
        let stream_key = format!("test:stream:env:{{{}}}", Uuid::new_v4());

        let mut conn = pool.create_subscriber_connection().await.unwrap();
        let client_id: i64 =
            redis::from_redis_value_ref(&conn.cmd(redis::cmd("CLIENT").arg("ID")).await.unwrap())
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

        let mut sub = RedisEnvSubscriber {
            connection_pool: Arc::clone(&pool),
            conn: Some(conn),
            stream_key: stream_key.clone(),
            last_id: "0".to_string(),
            last_failure: None,
        };

        assert!(sub.next().await.is_none());
        assert!(
            sub.conn.is_none(),
            "a failed XREAD must drop the dead connection"
        );
        assert!(
            sub.last_failure.is_some(),
            "a failed XREAD must stamp last_failure so the redial is paced"
        );
        // Skip the redial backoff to keep the test fast; pacing itself is
        // covered by `env_subscriber_reconnect_delay_paces_redials`.
        sub.last_failure = None;

        // Seed a non-event entry so the reconnected XREAD returns
        // immediately instead of parking in BLOCK.
        let _: String = redis::cmd("XADD")
            .arg(&stream_key)
            .arg("*")
            .arg("note")
            .arg("x")
            .query_async(&mut admin)
            .await
            .unwrap();

        assert!(sub.next().await.is_none(), "entry has no event field");
        assert!(
            sub.conn.is_some(),
            "next() after an error must recreate the connection"
        );
        assert_ne!(
            sub.last_id, "0",
            "the recreated connection must have served the read"
        );

        let _: redis::RedisResult<()> = redis::cmd("DEL")
            .arg(&stream_key)
            .query_async(&mut admin)
            .await;
    }

    /// Build an env subscriber against a client that cannot connect
    /// (nothing listens on port 1). `Client::open` only parses the URL, so
    /// no Redis is needed.
    fn unreachable_env_subscriber() -> RedisEnvSubscriber {
        let client = redis::Client::open("redis://127.0.0.1:1/").unwrap();
        RedisEnvSubscriber {
            connection_pool: Arc::new(RedisConnectionPool::new_standalone(client)),
            conn: None,
            stream_key: "test:stream:env:backoff".to_string(),
            last_id: "$".to_string(),
            last_failure: None,
        }
    }

    /// Reconnect pacing: no delay without a failure (first failure stays
    /// fast), the remainder of the window after a fresh failure, and no
    /// delay again once the window has passed.
    #[tokio::test]
    async fn env_subscriber_reconnect_delay_paces_redials() {
        let mut sub = unreachable_env_subscriber();

        // No failure yet: dial immediately.
        assert_eq!(sub.reconnect_delay(), None);

        // Fresh failure: wait out (most of) the backoff window.
        sub.last_failure = Some(Instant::now());
        let delay = sub
            .reconnect_delay()
            .expect("a recent failure must delay the redial");
        assert!(delay <= ENV_SUBSCRIBER_RECONNECT_BACKOFF);
        assert!(
            delay > ENV_SUBSCRIBER_RECONNECT_BACKOFF / 2,
            "a just-stamped failure should wait close to the full window, got {:?}",
            delay
        );

        // Stale failure: the window has passed, dial immediately.
        let stale = Instant::now()
            .checked_sub(ENV_SUBSCRIBER_RECONNECT_BACKOFF * 2)
            .expect("test setup: past instant must be representable");
        sub.last_failure = Some(stale);
        assert_eq!(sub.reconnect_delay(), None);
    }

    /// A failed dial must be recorded (so the next call backs off) without
    /// disturbing the last-seen stream id, which must survive reconnects.
    #[tokio::test]
    async fn env_subscriber_records_dial_failures_and_keeps_last_id() {
        let mut sub = unreachable_env_subscriber();
        sub.last_id = "42-1".to_string();

        assert!(sub.next().await.is_none());
        assert!(sub.conn.is_none(), "the failed dial must not cache a conn");
        assert!(
            sub.last_failure.is_some(),
            "a failed dial must stamp last_failure"
        );
        assert_eq!(
            sub.last_id, "42-1",
            "last-seen id must survive failed redials"
        );
    }
}

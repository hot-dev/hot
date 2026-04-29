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
    EnvEvent, EnvPublisher, EnvSubscriber, EnvSubscriberFactory, StreamEvent, StreamPubSubError,
    StreamPublisher, StreamSubscriber, StreamSubscriberFactory, channel_name, env_channel_name,
};
use async_trait::async_trait;
use redis::Client;
use redis::aio::MultiplexedConnection;
use redis::cluster::ClusterClient;
use redis::cluster_async::ClusterConnection as AsyncClusterConnection;
use std::sync::Arc;
use tokio::sync::Mutex;
use uuid::Uuid;

/// Maximum number of entries to keep per stream (approximate)
/// This prevents unbounded memory growth for streams
const STREAM_MAXLEN: usize = 1000;

/// Connection pool that caches Redis connections to avoid expensive reconnections
enum RedisConnectionPool {
    Standalone {
        client: Client,
        cached_conn: Arc<Mutex<Option<MultiplexedConnection>>>,
    },
    Cluster {
        client: ClusterClient,
        cached_conn: Arc<Mutex<Option<AsyncClusterConnection>>>,
    },
}

impl RedisConnectionPool {
    fn new_standalone(client: Client) -> Self {
        Self::Standalone {
            client,
            cached_conn: Arc::new(Mutex::new(None)),
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
                let conn = if let Some(conn) = guard.as_ref() {
                    conn.clone()
                } else {
                    let conn = client
                        .get_multiplexed_async_connection()
                        .await
                        .map_err(|e| StreamPubSubError::ConnectionError(e.to_string()))?;
                    *guard = Some(conn.clone());
                    conn
                };
                drop(guard);
                Ok(ConnectionGuard::Standalone(conn))
            }
            RedisConnectionPool::Cluster {
                client,
                cached_conn,
            } => {
                let mut guard = cached_conn.lock().await;
                if guard.is_none() {
                    let conn = client
                        .get_async_connection()
                        .await
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
                // Create a fresh connection for the subscriber
                let conn = client
                    .get_multiplexed_async_connection()
                    .await
                    .map_err(|e| StreamPubSubError::ConnectionError(e.to_string()))?;
                Ok(SubscriberConnection::Standalone(conn))
            }
            RedisConnectionPool::Cluster { client, .. } => {
                // Create a fresh connection for the subscriber
                let conn = client
                    .get_async_connection()
                    .await
                    .map_err(|e| StreamPubSubError::ConnectionError(e.to_string()))?;
                Ok(SubscriberConnection::Cluster(conn))
            }
        }
    }
}

/// Guard that holds a connection for short-lived operations
enum ConnectionGuard<'a> {
    Standalone(MultiplexedConnection),
    Cluster(tokio::sync::MutexGuard<'a, Option<AsyncClusterConnection>>),
}

impl ConnectionGuard<'_> {
    async fn cmd(&mut self, cmd: &redis::Cmd) -> Result<redis::Value, StreamPubSubError> {
        match self {
            ConnectionGuard::Standalone(conn) => {
                let result = cmd
                    .query_async(conn)
                    .await
                    .map_err(|e| StreamPubSubError::PublishError(e.to_string()))?;
                Ok(result)
            }
            ConnectionGuard::Cluster(guard) => {
                let conn = guard.as_mut().expect("cluster connection should exist");
                let result = cmd
                    .query_async(conn)
                    .await
                    .map_err(|e| StreamPubSubError::PublishError(e.to_string()))?;
                Ok(result)
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
impl StreamPublisher for RedisStreamsPubSub {
    async fn publish(&self, event: StreamEvent) -> Result<(), StreamPubSubError> {
        let stream_id = event.stream_id();
        let stream_key = channel_name(&stream_id);

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
        let stream_key = channel_name(&stream_id);

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
    async fn next(&mut self) -> Option<StreamEvent> {
        // XREAD BLOCK 30000 STREAMS stream last_id
        // 30 second block timeout - after which we return None and the caller can retry
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
                return None;
            }
        };

        // Parse the XREAD response
        // Format: [[stream-name, [[id, [field, value, ...]], ...]]]
        // or nil on timeout
        if matches!(value, redis::Value::Nil) {
            // Timeout - return None to allow the caller to retry or check other conditions
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
                match serde_json::from_str::<StreamEvent>(&payload) {
                    Ok(event) => return Some(event),
                    Err(e) => {
                        tracing::warn!(
                            "Failed to deserialize stream event from Redis Streams on {}: {}",
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
            conn,
            stream_key,
            // Start from latest - "$" means "only new messages"
            last_id: "$".to_string(),
        }))
    }
}

/// Redis Streams environment subscriber - owns its connection since XREAD BLOCK holds it
pub struct RedisEnvSubscriber {
    conn: SubscriberConnection,
    stream_key: String,
    last_id: String,
}

#[async_trait]
impl EnvSubscriber for RedisEnvSubscriber {
    async fn next(&mut self) -> Option<EnvEvent> {
        // XREAD BLOCK 30000 STREAMS stream last_id
        // 30 second block timeout - after which we return None and the caller can retry
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

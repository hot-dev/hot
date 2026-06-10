//! In-memory stream pub/sub implementation using tokio broadcast channels
//!
//! This is used for single-process deployments (`hot dev`) where the worker
//! and API run in the same process.

use super::{
    EnvEvent, EnvPublisher, EnvSubscriber, EnvSubscriberFactory, McpSseTransportSessionBinding,
    McpSseTransportSessionStore, StreamEvent, StreamNext, StreamPubSubError, StreamPublisher,
    StreamSubscriber, StreamSubscriberFactory, channel_name, legacy_channel_name,
};
use ahash::AHashMap;
use async_trait::async_trait;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{RwLock, broadcast};
use uuid::Uuid;

/// Channel capacity for broadcast channels
/// This determines how many events can be buffered before subscribers lag
const CHANNEL_CAPACITY: usize = 256;

/// In-memory stream pub/sub using tokio broadcast channels
///
/// Uses a RwLock<HashMap> to store broadcast senders per stream_id.
/// When a subscriber subscribes, they get a receiver from the corresponding sender.
/// When a publisher publishes, they send to the corresponding sender.
#[derive(Clone)]
pub struct MemStreamPubSub {
    /// Map of channel key -> broadcast sender
    /// The sender is created lazily when first needed
    channels: Arc<RwLock<AHashMap<String, broadcast::Sender<StreamEvent>>>>,
    /// Map of env_id -> broadcast sender for environment-level events
    env_channels: Arc<RwLock<AHashMap<Uuid, broadcast::Sender<EnvEvent>>>>,
    /// Ephemeral MCP HTTP+SSE transport session bindings.
    mcp_sse_transport_sessions: Arc<RwLock<AHashMap<Uuid, McpSseTransportSessionBinding>>>,
}

impl MemStreamPubSub {
    /// Create a new in-memory stream pub/sub
    pub fn new() -> Self {
        Self {
            channels: Arc::new(RwLock::new(AHashMap::new())),
            env_channels: Arc::new(RwLock::new(AHashMap::new())),
            mcp_sse_transport_sessions: Arc::new(RwLock::new(AHashMap::new())),
        }
    }

    /// Get or create a broadcast sender for a stream
    async fn get_or_create_sender(&self, channel_key: String) -> broadcast::Sender<StreamEvent> {
        // Try read lock first for the common case where channel exists
        {
            let channels = self.channels.read().await;
            if let Some(sender) = channels.get(&channel_key) {
                return sender.clone();
            }
        }

        // Need to create - acquire write lock
        let mut channels = self.channels.write().await;
        // Double-check in case another task created it while we were waiting
        if let Some(sender) = channels.get(&channel_key) {
            return sender.clone();
        }

        // Create new channel
        let (tx, _) = broadcast::channel(CHANNEL_CAPACITY);
        channels.insert(channel_key, tx.clone());
        tx
    }

    /// Get or create a broadcast sender for an environment
    async fn get_or_create_env_sender(&self, env_id: Uuid) -> broadcast::Sender<EnvEvent> {
        // Try read lock first for the common case where channel exists
        {
            let channels = self.env_channels.read().await;
            if let Some(sender) = channels.get(&env_id) {
                return sender.clone();
            }
        }

        // Need to create - acquire write lock
        let mut channels = self.env_channels.write().await;
        // Double-check in case another task created it while we were waiting
        if let Some(sender) = channels.get(&env_id) {
            return sender.clone();
        }

        // Create new channel
        let (tx, _) = broadcast::channel(CHANNEL_CAPACITY);
        channels.insert(env_id, tx.clone());
        tx
    }

    /// Clean up channels with no active receivers
    /// This prevents memory leaks from accumulated channels
    pub async fn cleanup_empty_channels(&self) {
        let mut channels = self.channels.write().await;
        channels.retain(|_, sender| {
            // Keep channels that have at least one receiver
            sender.receiver_count() > 0
        });

        let mut env_channels = self.env_channels.write().await;
        env_channels.retain(|_, sender| sender.receiver_count() > 0);

        let mut transport_sessions = self.mcp_sse_transport_sessions.write().await;
        transport_sessions.retain(|_, binding| !binding.is_expired());
    }
}

impl Default for MemStreamPubSub {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl StreamPublisher for MemStreamPubSub {
    async fn publish(&self, event: StreamEvent) -> Result<(), StreamPubSubError> {
        let channel_key = event.channel_name();
        let sender = self.get_or_create_sender(channel_key.clone()).await;

        // Send to all subscribers - ignore if no receivers (fire-and-forget)
        match sender.send(event) {
            Ok(receiver_count) => {
                tracing::debug!(
                    "Published stream event to {} receivers for stream {}",
                    receiver_count,
                    channel_key
                );
                Ok(())
            }
            Err(_) => {
                // No receivers - this is fine for fire-and-forget
                tracing::trace!(
                    "No receivers for stream event on stream {} (event dropped)",
                    channel_key
                );
                Ok(())
            }
        }
    }
}

#[async_trait]
impl StreamSubscriberFactory for MemStreamPubSub {
    async fn subscribe(
        &self,
        stream_id: Uuid,
    ) -> Result<Box<dyn StreamSubscriber>, StreamPubSubError> {
        let channel_key = legacy_channel_name(&stream_id);
        let sender = self.get_or_create_sender(channel_key.clone()).await;
        let receiver = sender.subscribe();

        tracing::debug!(
            "Created new subscriber for stream {} (total receivers: {})",
            stream_id,
            sender.receiver_count()
        );

        Ok(Box::new(MemStreamSubscriber {
            receiver,
            channel_key,
        }))
    }

    async fn subscribe_in_env(
        &self,
        env_id: Uuid,
        stream_id: Uuid,
    ) -> Result<Box<dyn StreamSubscriber>, StreamPubSubError> {
        let channel_key = channel_name(&env_id, &stream_id);
        let sender = self.get_or_create_sender(channel_key.clone()).await;
        let receiver = sender.subscribe();

        tracing::debug!(
            "Created new subscriber for env {} stream {} (total receivers: {})",
            env_id,
            stream_id,
            sender.receiver_count()
        );

        Ok(Box::new(MemStreamSubscriber {
            receiver,
            channel_key,
        }))
    }
}

/// In-memory stream subscriber
pub struct MemStreamSubscriber {
    receiver: broadcast::Receiver<StreamEvent>,
    channel_key: String,
}

#[async_trait]
impl StreamSubscriber for MemStreamSubscriber {
    async fn next(&mut self) -> StreamNext {
        loop {
            match self.receiver.recv().await {
                Ok(event) => {
                    return StreamNext::Event(event);
                }
                Err(broadcast::error::RecvError::Lagged(count)) => {
                    // We missed some messages due to slow consumption
                    tracing::warn!(
                        "Stream subscriber for {} lagged, missed {} events",
                        self.channel_key,
                        count
                    );
                    // Continue to receive the next available event
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => {
                    tracing::debug!("Stream subscription closed for {}", self.channel_key);
                    return StreamNext::Closed;
                }
            }
        }
    }
}

#[async_trait]
impl McpSseTransportSessionStore for MemStreamPubSub {
    async fn put_mcp_sse_transport_session(
        &self,
        binding: McpSseTransportSessionBinding,
        _ttl: Duration,
    ) -> Result<(), StreamPubSubError> {
        self.cleanup_empty_channels().await;
        self.mcp_sse_transport_sessions
            .write()
            .await
            .insert(binding.transport_session_id, binding);
        Ok(())
    }

    async fn get_mcp_sse_transport_session(
        &self,
        transport_session_id: Uuid,
    ) -> Result<Option<McpSseTransportSessionBinding>, StreamPubSubError> {
        let binding = self
            .mcp_sse_transport_sessions
            .read()
            .await
            .get(&transport_session_id)
            .cloned();

        if matches!(binding, Some(ref b) if b.is_expired()) {
            self.delete_mcp_sse_transport_session(transport_session_id)
                .await?;
            return Ok(None);
        }

        Ok(binding)
    }

    async fn delete_mcp_sse_transport_session(
        &self,
        transport_session_id: Uuid,
    ) -> Result<(), StreamPubSubError> {
        self.mcp_sse_transport_sessions
            .write()
            .await
            .remove(&transport_session_id);
        Ok(())
    }
}

// ============================================================================
// Environment-level pub/sub (for dashboard real-time updates)
// ============================================================================

#[async_trait]
impl EnvPublisher for MemStreamPubSub {
    async fn publish_env(&self, event: EnvEvent) -> Result<(), StreamPubSubError> {
        let env_id = event.env_id();
        let sender = self.get_or_create_env_sender(env_id).await;

        // Send to all subscribers - ignore if no receivers (fire-and-forget)
        match sender.send(event) {
            Ok(receiver_count) => {
                tracing::debug!(
                    "Published env event to {} receivers for env {}",
                    receiver_count,
                    env_id
                );
                Ok(())
            }
            Err(_) => {
                // No receivers - this is fine for fire-and-forget
                tracing::trace!(
                    "No receivers for env event on env {} (event dropped)",
                    env_id
                );
                Ok(())
            }
        }
    }
}

#[async_trait]
impl EnvSubscriberFactory for MemStreamPubSub {
    async fn subscribe_env(
        &self,
        env_id: Uuid,
    ) -> Result<Box<dyn EnvSubscriber>, StreamPubSubError> {
        let sender = self.get_or_create_env_sender(env_id).await;
        let receiver = sender.subscribe();

        tracing::debug!(
            "Created new env subscriber for env {} (total receivers: {})",
            env_id,
            sender.receiver_count()
        );

        Ok(Box::new(MemEnvSubscriber { receiver, env_id }))
    }
}

/// In-memory environment subscriber
pub struct MemEnvSubscriber {
    receiver: broadcast::Receiver<EnvEvent>,
    env_id: Uuid,
}

#[async_trait]
impl EnvSubscriber for MemEnvSubscriber {
    async fn next(&mut self) -> Option<EnvEvent> {
        loop {
            match self.receiver.recv().await {
                Ok(event) => {
                    return Some(event);
                }
                Err(broadcast::error::RecvError::Lagged(count)) => {
                    // We missed some messages due to slow consumption
                    tracing::warn!(
                        "Env subscriber for {} lagged, missed {} events",
                        self.env_id,
                        count
                    );
                    // Continue to receive the next available event
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => {
                    tracing::debug!("Env subscription closed for env {}", self.env_id);
                    return None;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn expect_event(next: StreamNext) -> StreamEvent {
        match next {
            StreamNext::Event(event) => event,
            other => panic!("expected stream event, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_publish_subscribe() {
        let pubsub = MemStreamPubSub::new();
        let env_id = Uuid::new_v4();
        let stream_id = Uuid::new_v4();
        let run_id = Uuid::new_v4();

        // Create subscriber first
        let mut subscriber = pubsub.subscribe_in_env(env_id, stream_id).await.unwrap();

        // Publish an event
        let event = StreamEvent::RunStop {
            run_id,
            env_id,
            stream_id,
            event_id: None,
            result: Some(serde_json::json!({"test": "value"})),
        };
        pubsub.publish(event.clone()).await.unwrap();

        // Receive the event
        let received =
            tokio::time::timeout(std::time::Duration::from_millis(100), subscriber.next())
                .await
                .expect("timeout");
        let received = expect_event(received);

        assert_eq!(received.run_id(), run_id);
        assert_eq!(received.stream_id(), stream_id);
    }

    #[tokio::test]
    async fn test_multiple_subscribers() {
        let pubsub = MemStreamPubSub::new();
        let env_id = Uuid::new_v4();
        let stream_id = Uuid::new_v4();
        let run_id = Uuid::new_v4();

        // Create multiple subscribers
        let mut sub1 = pubsub.subscribe_in_env(env_id, stream_id).await.unwrap();
        let mut sub2 = pubsub.subscribe_in_env(env_id, stream_id).await.unwrap();

        // Publish an event
        let event = StreamEvent::RunStart {
            run_id,
            env_id,
            stream_id,
            event_id: None,
        };
        pubsub.publish(event).await.unwrap();

        // Both should receive the event
        let received1 = tokio::time::timeout(std::time::Duration::from_millis(100), sub1.next())
            .await
            .expect("timeout");
        let received1 = expect_event(received1);

        let received2 = tokio::time::timeout(std::time::Duration::from_millis(100), sub2.next())
            .await
            .expect("timeout");
        let received2 = expect_event(received2);

        assert_eq!(received1.run_id(), run_id);
        assert_eq!(received2.run_id(), run_id);
    }

    #[tokio::test]
    async fn test_env_scoped_stream_subscriber_does_not_receive_other_env_event() {
        let pubsub = MemStreamPubSub::new();
        let env_a = Uuid::new_v4();
        let env_b = Uuid::new_v4();
        let stream_id = Uuid::new_v4();

        let mut sub_a = pubsub.subscribe_in_env(env_a, stream_id).await.unwrap();

        let event = StreamEvent::RunStop {
            run_id: Uuid::new_v4(),
            env_id: env_b,
            stream_id,
            event_id: None,
            result: None,
        };
        pubsub.publish(event).await.unwrap();

        let received =
            tokio::time::timeout(std::time::Duration::from_millis(50), sub_a.next()).await;
        assert!(received.is_err());
    }

    #[tokio::test]
    async fn test_no_subscribers_ok() {
        let pubsub = MemStreamPubSub::new();
        let env_id = Uuid::new_v4();
        let stream_id = Uuid::new_v4();
        let run_id = Uuid::new_v4();

        // Publish without subscribers - should not error
        let event = StreamEvent::RunStop {
            run_id,
            env_id,
            stream_id,
            event_id: None,
            result: None,
        };
        let result = pubsub.publish(event).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_cleanup_empty_channels() {
        let pubsub = MemStreamPubSub::new();
        let stream_id = Uuid::new_v4();

        // Create and drop a subscriber
        {
            let _subscriber = pubsub.subscribe(stream_id).await.unwrap();
            assert_eq!(pubsub.channels.read().await.len(), 1);
        }

        // Cleanup should remove the channel since no active receivers
        pubsub.cleanup_empty_channels().await;
        assert_eq!(pubsub.channels.read().await.len(), 0);
    }
}

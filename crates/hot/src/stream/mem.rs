//! In-memory stream pub/sub implementation using tokio broadcast channels
//!
//! This is used for single-process deployments (`hot dev`) where the worker
//! and API run in the same process.

use super::{
    EnvEvent, EnvPublisher, EnvSubscriber, EnvSubscriberFactory, StreamEvent, StreamPubSubError,
    StreamPublisher, StreamSubscriber, StreamSubscriberFactory,
};
use ahash::AHashMap;
use async_trait::async_trait;
use std::sync::Arc;
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
    /// Map of stream_id -> broadcast sender
    /// The sender is created lazily when first needed
    channels: Arc<RwLock<AHashMap<Uuid, broadcast::Sender<StreamEvent>>>>,
    /// Map of env_id -> broadcast sender for environment-level events
    env_channels: Arc<RwLock<AHashMap<Uuid, broadcast::Sender<EnvEvent>>>>,
}

impl MemStreamPubSub {
    /// Create a new in-memory stream pub/sub
    pub fn new() -> Self {
        Self {
            channels: Arc::new(RwLock::new(AHashMap::new())),
            env_channels: Arc::new(RwLock::new(AHashMap::new())),
        }
    }

    /// Get or create a broadcast sender for a stream
    async fn get_or_create_sender(&self, stream_id: Uuid) -> broadcast::Sender<StreamEvent> {
        // Try read lock first for the common case where channel exists
        {
            let channels = self.channels.read().await;
            if let Some(sender) = channels.get(&stream_id) {
                return sender.clone();
            }
        }

        // Need to create - acquire write lock
        let mut channels = self.channels.write().await;
        // Double-check in case another task created it while we were waiting
        if let Some(sender) = channels.get(&stream_id) {
            return sender.clone();
        }

        // Create new channel
        let (tx, _) = broadcast::channel(CHANNEL_CAPACITY);
        channels.insert(stream_id, tx.clone());
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
        let stream_id = event.stream_id();
        let sender = self.get_or_create_sender(stream_id).await;

        // Send to all subscribers - ignore if no receivers (fire-and-forget)
        match sender.send(event) {
            Ok(receiver_count) => {
                tracing::debug!(
                    "Published stream event to {} receivers for stream {}",
                    receiver_count,
                    stream_id
                );
                Ok(())
            }
            Err(_) => {
                // No receivers - this is fine for fire-and-forget
                tracing::trace!(
                    "No receivers for stream event on stream {} (event dropped)",
                    stream_id
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
        let sender = self.get_or_create_sender(stream_id).await;
        let receiver = sender.subscribe();

        tracing::debug!(
            "Created new subscriber for stream {} (total receivers: {})",
            stream_id,
            sender.receiver_count()
        );

        Ok(Box::new(MemStreamSubscriber {
            receiver,
            stream_id,
        }))
    }
}

/// In-memory stream subscriber
pub struct MemStreamSubscriber {
    receiver: broadcast::Receiver<StreamEvent>,
    stream_id: Uuid,
}

#[async_trait]
impl StreamSubscriber for MemStreamSubscriber {
    async fn next(&mut self) -> Option<StreamEvent> {
        loop {
            match self.receiver.recv().await {
                Ok(event) => {
                    return Some(event);
                }
                Err(broadcast::error::RecvError::Lagged(count)) => {
                    // We missed some messages due to slow consumption
                    tracing::warn!(
                        "Stream subscriber for {} lagged, missed {} events",
                        self.stream_id,
                        count
                    );
                    // Continue to receive the next available event
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => {
                    tracing::debug!("Stream subscription closed for stream {}", self.stream_id);
                    return None;
                }
            }
        }
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

    #[tokio::test]
    async fn test_publish_subscribe() {
        let pubsub = MemStreamPubSub::new();
        let stream_id = Uuid::new_v4();
        let run_id = Uuid::new_v4();

        // Create subscriber first
        let mut subscriber = pubsub.subscribe(stream_id).await.unwrap();

        // Publish an event
        let event = StreamEvent::RunStop {
            run_id,
            stream_id,
            event_id: None,
            result: Some(serde_json::json!({"test": "value"})),
        };
        pubsub.publish(event.clone()).await.unwrap();

        // Receive the event
        let received =
            tokio::time::timeout(std::time::Duration::from_millis(100), subscriber.next())
                .await
                .expect("timeout")
                .expect("should receive event");

        assert_eq!(received.run_id(), run_id);
        assert_eq!(received.stream_id(), stream_id);
    }

    #[tokio::test]
    async fn test_multiple_subscribers() {
        let pubsub = MemStreamPubSub::new();
        let stream_id = Uuid::new_v4();
        let run_id = Uuid::new_v4();

        // Create multiple subscribers
        let mut sub1 = pubsub.subscribe(stream_id).await.unwrap();
        let mut sub2 = pubsub.subscribe(stream_id).await.unwrap();

        // Publish an event
        let event = StreamEvent::RunStart {
            run_id,
            stream_id,
            event_id: None,
        };
        pubsub.publish(event).await.unwrap();

        // Both should receive the event
        let received1 = tokio::time::timeout(std::time::Duration::from_millis(100), sub1.next())
            .await
            .expect("timeout")
            .expect("should receive event");

        let received2 = tokio::time::timeout(std::time::Duration::from_millis(100), sub2.next())
            .await
            .expect("timeout")
            .expect("should receive event");

        assert_eq!(received1.run_id(), run_id);
        assert_eq!(received2.run_id(), run_id);
    }

    #[tokio::test]
    async fn test_no_subscribers_ok() {
        let pubsub = MemStreamPubSub::new();
        let stream_id = Uuid::new_v4();
        let run_id = Uuid::new_v4();

        // Publish without subscribers - should not error
        let event = StreamEvent::RunStop {
            run_id,
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

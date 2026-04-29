//! In-process MPMC memory queue.
//!
//! This is a single-process queue backed by `async_channel`. Multiple
//! `MemQueue` handles created with the same name and item type within a
//! single process share the same underlying channel — preserving the
//! "same name → same queue" contract used by the scheduler/worker/api
//! when they all run inside `hot dev`.
//!
//! For multi-process local development (e.g. running `hot worker` and
//! `hot scheduler` as separate processes), use the Redis Streams backend
//! instead by setting `HOT_QUEUE_TYPE=redis`.

use super::{Queue, QueueProcessingError, QueueProcessor, Serialization};
use serde::{Serialize, de::DeserializeOwned};
use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::error::Error;
use std::future::Future;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

/// Maximum number of in-flight delivery retries for a worker function.
const MAX_PROCESSING_RETRIES: usize = 3;

/// Channel capacity for in-process queues. Bounded so a runaway producer
/// can apply backpressure to upstream callers rather than exhausting heap.
/// 100k is large enough that healthy bursty workloads never block.
const CHANNEL_CAPACITY: usize = 100_000;

#[derive(Debug)]
pub enum MemQueueError {
    /// Queue is closed (all senders/receivers dropped).
    Closed,
    /// Send back-pressure (channel is at capacity).
    Full,
}

impl std::fmt::Display for MemQueueError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Closed => write!(f, "Queue closed"),
            Self::Full => write!(f, "Queue is full"),
        }
    }
}

impl std::error::Error for MemQueueError {}

/// Wrapper that tracks delivery retries for a queued item.
#[derive(Clone, Debug)]
struct RetryItem<T> {
    item: T,
    retry_count: usize,
    first_attempt: Instant,
}

impl<T> RetryItem<T> {
    fn new(item: T) -> Self {
        Self {
            item,
            retry_count: 0,
            first_attempt: Instant::now(),
        }
    }

    fn from_parts(item: T, retry_count: usize, first_attempt: Instant) -> Self {
        Self {
            item,
            retry_count,
            first_attempt,
        }
    }

    fn exceeded_retries(&self) -> bool {
        self.retry_count >= MAX_PROCESSING_RETRIES
    }
}

/// Channels for a single named queue. Stored boxed inside the registry as
/// `Arc<dyn Any + Send + Sync>` so the registry can hold queues of
/// heterogeneous item types.
struct Channels<T> {
    tx: async_channel::Sender<RetryItem<T>>,
    rx: async_channel::Receiver<RetryItem<T>>,
    dlq_tx: async_channel::Sender<T>,
    dlq_rx: async_channel::Receiver<T>,
}

impl<T> Clone for Channels<T> {
    fn clone(&self) -> Self {
        Self {
            tx: self.tx.clone(),
            rx: self.rx.clone(),
            dlq_tx: self.dlq_tx.clone(),
            dlq_rx: self.dlq_rx.clone(),
        }
    }
}

impl<T: Send + Sync + 'static> Channels<T> {
    fn create() -> Self {
        let (tx, rx) = async_channel::bounded(CHANNEL_CAPACITY);
        let (dlq_tx, dlq_rx) = async_channel::bounded(CHANNEL_CAPACITY);
        Self {
            tx,
            rx,
            dlq_tx,
            dlq_rx,
        }
    }
}

/// Type-erased channel handle stored in the global registry.
type RegisteredChannels = Arc<dyn Any + Send + Sync>;
type ChannelRegistry = Mutex<HashMap<(String, TypeId), RegisteredChannels>>;

/// Process-wide registry of named channels. Keyed by `(name, TypeId)` so
/// the same queue name with different item types stays isolated.
fn registry() -> &'static ChannelRegistry {
    static REG: OnceLock<ChannelRegistry> = OnceLock::new();
    REG.get_or_init(|| Mutex::new(HashMap::new()))
}

fn get_or_create_channels<T: Send + Sync + 'static>(name: &str) -> Channels<T> {
    let key = (name.to_string(), TypeId::of::<T>());
    let mut reg = registry().lock().expect("queue registry poisoned");
    if let Some(existing) = reg.get(&key)
        && let Some(typed) = existing.clone().downcast::<Channels<T>>().ok()
    {
        return (*typed).clone();
    }

    let channels = Channels::<T>::create();
    let stored: RegisteredChannels = Arc::new(channels.clone());
    reg.insert(key, stored);
    channels
}

/// In-process FIFO queue. Cheap to clone — clones share the same channel.
pub struct MemQueue<T> {
    channels: Channels<T>,
    queue_name: String,
    /// Carried for API compatibility; ignored on the in-process path.
    serialization: Serialization,
}

impl<T> Clone for MemQueue<T> {
    fn clone(&self) -> Self {
        Self {
            channels: self.channels.clone(),
            queue_name: self.queue_name.clone(),
            serialization: self.serialization,
        }
    }
}

impl<T: Send + Sync + 'static> MemQueue<T> {
    /// Construct (or attach to the existing) named in-process queue.
    pub fn new(queue_name: String) -> Result<Self, MemQueueError> {
        let channels = get_or_create_channels::<T>(&queue_name);
        Ok(Self {
            channels,
            queue_name,
            serialization: Serialization::default(),
        })
    }

    /// Set serialization format. Recorded for API compatibility but unused
    /// on the in-process path (no serialization happens here).
    pub fn with_serialization(mut self, format: Serialization) -> Self {
        self.serialization = format;
        self
    }

    pub fn name(&self) -> &str {
        &self.queue_name
    }

    /// Drain the dead-letter channel for inspection / metrics.
    pub fn try_drain_dlq(&self) -> Vec<T> {
        let mut out = Vec::new();
        while let Ok(item) = self.channels.dlq_rx.try_recv() {
            out.push(item);
        }
        out
    }

    /// Internal hook for `tokio::select!`-based loops that want to await a
    /// message without polling. Phase 2 uses this to replace polling loops
    /// with truly async waits across multiple queues.
    ///
    /// Returns the item plus its current retry count (so callers can
    /// re-enqueue with the right retry state on failure).
    pub async fn recv_async(&self) -> Result<T, MemQueueError> {
        match self.channels.rx.recv().await {
            Ok(retry) => Ok(retry.item),
            Err(_) => Err(MemQueueError::Closed),
        }
    }
}

impl<T: Send + Sync + Serialize + DeserializeOwned + Clone + 'static> MemQueue<T> {
    /// Blocking variant of `dequeue_and_work`: parks the future on the
    /// underlying channel until a message is enqueued (or the channel
    /// closes). Designed to be used inside `tokio::select!` so worker
    /// loops can wake instantly on enqueue without polling.
    pub async fn process_blocking<F, Fut, R>(
        &self,
        worker: F,
    ) -> Result<Option<R>, Box<dyn Error + Send + Sync>>
    where
        F: FnOnce(T) -> Fut + Send,
        Fut: Future<Output = Result<R, Box<dyn Error + Send + Sync>>> + Send,
        R: Send + Sync,
    {
        let retry_item = match self.channels.rx.recv().await {
            Ok(item) => item,
            Err(_) => return Err(Box::new(MemQueueError::Closed)),
        };

        if retry_item.exceeded_retries() {
            let _ = self.channels.dlq_tx.try_send(retry_item.item);
            return Err(Box::new(QueueProcessingError::RetryLimitExceeded));
        }

        let item_for_worker = retry_item.item.clone();
        let next_retry_count = retry_item.retry_count + 1;
        let first_attempt = retry_item.first_attempt;

        match worker(item_for_worker).await {
            Ok(result) => Ok(Some(result)),
            Err(e) => {
                let updated =
                    RetryItem::from_parts(retry_item.item, next_retry_count, first_attempt);
                if updated.exceeded_retries() {
                    let _ = self.channels.dlq_tx.try_send(updated.item);
                    Err(Box::new(QueueProcessingError::WorkerError(e)))
                } else {
                    if let Err(send_err) = self.channels.tx.send(updated).await {
                        tracing::warn!(
                            queue = self.queue_name,
                            "Failed to re-enqueue retry: {}",
                            send_err
                        );
                    }
                    Err(Box::new(QueueProcessingError::WorkerError(e)))
                }
            }
        }
    }
}

#[async_trait::async_trait]
impl<T: Send + Sync + Serialize + DeserializeOwned + 'static> Queue<T> for MemQueue<T> {
    async fn enqueue(&self, item: T) -> Result<(), Box<dyn Error + Send + Sync>> {
        match self.channels.tx.send(RetryItem::new(item)).await {
            Ok(()) => Ok(()),
            Err(_) => Err(Box::new(MemQueueError::Closed)),
        }
    }

    async fn dequeue(&self) -> Result<Option<T>, Box<dyn Error + Send + Sync>> {
        match self.channels.rx.try_recv() {
            Ok(retry) => Ok(Some(retry.item)),
            Err(async_channel::TryRecvError::Empty) => Ok(None),
            Err(async_channel::TryRecvError::Closed) => Err(Box::new(MemQueueError::Closed)),
        }
    }

    async fn len(&self) -> Result<usize, Box<dyn Error + Send + Sync>> {
        Ok(self.channels.rx.len())
    }

    async fn move_to_dead_letter_queue(
        &self,
        item: T,
        reason: String,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        tracing::debug!(
            queue = self.queue_name,
            reason = reason,
            "Moving item to dead letter queue"
        );
        self.channels
            .dlq_tx
            .send(item)
            .await
            .map_err(|_| Box::new(MemQueueError::Closed) as Box<dyn Error + Send + Sync>)?;
        Ok(())
    }
}

#[async_trait::async_trait]
impl<T: Send + Sync + Serialize + DeserializeOwned + Clone + 'static> QueueProcessor<T>
    for MemQueue<T>
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
        // Non-blocking pop. Returning `Ok(None)` when empty preserves the
        // existing polling-loop semantics in callers; Phase 2 introduces
        // `tokio::select!`-based loops that use `recv_async` for true
        // async wakeup.
        let retry_item = match self.channels.rx.try_recv() {
            Ok(item) => item,
            Err(async_channel::TryRecvError::Empty) => return Ok(None),
            Err(async_channel::TryRecvError::Closed) => {
                return Err(Box::new(MemQueueError::Closed));
            }
        };

        if retry_item.exceeded_retries() {
            // Capture into DLQ before bailing.
            let _ = self.channels.dlq_tx.try_send(retry_item.item);
            return Err(Box::new(QueueProcessingError::RetryLimitExceeded));
        }

        // Clone for the worker so we still have the original to re-enqueue
        // if the worker fails (avoids requiring `T: Clone` on the basic Queue trait).
        let item_for_worker = retry_item.item.clone();
        let next_retry_count = retry_item.retry_count + 1;
        let first_attempt = retry_item.first_attempt;

        match worker(item_for_worker).await {
            Ok(result) => Ok(Some(result)),
            Err(e) => {
                let updated =
                    RetryItem::from_parts(retry_item.item, next_retry_count, first_attempt);

                if updated.exceeded_retries() {
                    // Final failure — DLQ and surface error.
                    let _ = self.channels.dlq_tx.try_send(updated.item);
                    Err(Box::new(QueueProcessingError::WorkerError(e)))
                } else {
                    // Re-enqueue at the BACK of the queue so other items
                    // can interleave (matches prior behavior).
                    if let Err(send_err) = self.channels.tx.send(updated).await {
                        tracing::warn!(
                            queue = self.queue_name,
                            "Failed to re-enqueue retry: {}",
                            send_err
                        );
                    }
                    Err(Box::new(QueueProcessingError::WorkerError(e)))
                }
            }
        }
    }
}

impl<T> Default for MemQueue<T>
where
    T: Send + Sync + Serialize + DeserializeOwned + Clone + 'static,
{
    fn default() -> Self {
        Self::new("default_mem_queue".to_string()).unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};
    use std::sync::Arc;
    use tokio::task;
    use uuid::Uuid;

    fn unique_name(prefix: &str) -> String {
        format!("{}-{}", prefix, Uuid::new_v4().as_simple())
    }

    #[tokio::test]
    async fn test_enqueue_dequeue() {
        let queue = MemQueue::<i32>::new(unique_name("tq")).unwrap();

        queue.enqueue(1).await.unwrap();
        queue.enqueue(2).await.unwrap();
        queue.enqueue(3).await.unwrap();

        assert_eq!(queue.len().await.unwrap(), 3);

        assert_eq!(queue.dequeue().await.unwrap(), Some(1));
        assert_eq!(queue.dequeue().await.unwrap(), Some(2));
        assert_eq!(queue.dequeue().await.unwrap(), Some(3));

        assert_eq!(queue.dequeue().await.unwrap(), None);
        assert_eq!(queue.len().await.unwrap(), 0);
    }

    #[tokio::test]
    async fn test_is_empty() {
        let queue = MemQueue::<i32>::new(unique_name("tq")).unwrap();

        assert!(queue.is_empty().await.unwrap());

        queue.enqueue(1).await.unwrap();
        assert!(!queue.is_empty().await.unwrap());

        queue.dequeue().await.unwrap();
        assert!(queue.is_empty().await.unwrap());
    }

    #[tokio::test]
    async fn test_concurrent_operations() {
        let queue = Arc::new(MemQueue::<i32>::new(unique_name("tq")).unwrap());

        let mut handles = vec![];
        for i in 0..10 {
            let queue = Arc::clone(&queue);
            handles.push(task::spawn(async move {
                queue.enqueue(i).await.unwrap();
            }));
        }

        for handle in handles {
            handle.await.unwrap();
        }

        assert_eq!(queue.len().await.unwrap(), 10);

        let mut handles = vec![];
        for _ in 0..10 {
            let queue = Arc::clone(&queue);
            handles.push(task::spawn(async move { queue.dequeue().await.unwrap() }));
        }

        let mut results = vec![];
        for handle in handles {
            results.push(handle.await.unwrap());
        }

        let mut values: Vec<i32> = results.into_iter().flatten().collect();
        values.sort();
        assert_eq!(values, (0..10).collect::<Vec<i32>>());

        assert_eq!(queue.len().await.unwrap(), 0);
    }

    #[tokio::test]
    async fn test_string_items() {
        let queue = MemQueue::<String>::new(unique_name("tq")).unwrap();

        queue.enqueue("hello".to_string()).await.unwrap();
        queue.enqueue("world".to_string()).await.unwrap();

        assert_eq!(queue.dequeue().await.unwrap(), Some("hello".to_string()));
        assert_eq!(queue.dequeue().await.unwrap(), Some("world".to_string()));
    }

    #[tokio::test]
    async fn test_complex_types() {
        #[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
        struct TestStruct {
            id: i32,
            name: String,
            tags: Vec<String>,
        }

        let queue = MemQueue::<TestStruct>::new(unique_name("tq")).unwrap();

        let item1 = TestStruct {
            id: 1,
            name: "test1".to_string(),
            tags: vec!["tag1".to_string(), "tag2".to_string()],
        };
        let item2 = TestStruct {
            id: 2,
            name: "test2".to_string(),
            tags: vec!["tag3".to_string()],
        };

        queue.enqueue(item1.clone()).await.unwrap();
        queue.enqueue(item2.clone()).await.unwrap();

        assert_eq!(queue.dequeue().await.unwrap(), Some(item1));
        assert_eq!(queue.dequeue().await.unwrap(), Some(item2));
    }

    #[tokio::test]
    async fn test_shared_queue_between_instances() {
        // Two handles to the same name share the same channel.
        let name = unique_name("shared");
        let q1 = MemQueue::<String>::new(name.clone()).unwrap();
        let q2 = MemQueue::<String>::new(name).unwrap();

        q1.enqueue("item1".to_string()).await.unwrap();
        q1.enqueue("item2".to_string()).await.unwrap();

        assert_eq!(q2.len().await.unwrap(), 2);
        assert_eq!(q2.dequeue().await.unwrap(), Some("item1".to_string()));
        assert_eq!(q1.len().await.unwrap(), 1);
        assert_eq!(q1.dequeue().await.unwrap(), Some("item2".to_string()));
    }

    #[tokio::test]
    async fn test_dequeue_and_work_success() {
        let queue = MemQueue::<i32>::new(unique_name("tq")).unwrap();

        queue.enqueue(1).await.unwrap();
        queue.enqueue(2).await.unwrap();
        queue.enqueue(3).await.unwrap();

        let result = queue
            .dequeue_and_work(|item| async move {
                let result: Result<i32, Box<dyn Error + Send + Sync>> = Ok(item * 2);
                result
            })
            .await
            .unwrap();

        assert_eq!(result, Some(2));
        assert_eq!(queue.len().await.unwrap(), 2);
    }

    #[tokio::test]
    async fn test_dequeue_and_work_failure_retry() {
        let queue = MemQueue::<i32>::new(unique_name("tq")).unwrap();
        queue.enqueue(5).await.unwrap();

        let result = queue
            .dequeue_and_work(|_item| async move {
                let error: Box<dyn Error + Send + Sync> = "Processing failed".into();
                let result: Result<i32, Box<dyn Error + Send + Sync>> = Err(error);
                result
            })
            .await;

        assert!(result.is_err());
        // Item is re-queued at the back with retry_count incremented.
        assert_eq!(queue.len().await.unwrap(), 1);
    }

    #[tokio::test]
    async fn test_dequeue_and_work_retry_limit_to_dlq() {
        let queue = MemQueue::<i32>::new(unique_name("tq")).unwrap();
        queue.enqueue(99).await.unwrap();

        // Fail until retries exhausted.
        for _ in 0..(MAX_PROCESSING_RETRIES + 1) {
            let _ = queue
                .dequeue_and_work(|_item| async move {
                    let error: Box<dyn Error + Send + Sync> = "fail".into();
                    let result: Result<i32, Box<dyn Error + Send + Sync>> = Err(error);
                    result
                })
                .await;
        }

        assert!(queue.is_empty().await.unwrap());
        let dlq = queue.try_drain_dlq();
        assert_eq!(dlq, vec![99]);
    }

    #[tokio::test]
    async fn test_move_to_dead_letter_queue() {
        let queue = MemQueue::<String>::new(unique_name("tq")).unwrap();

        queue
            .move_to_dead_letter_queue("dropped".to_string(), "test reason".to_string())
            .await
            .unwrap();

        let dlq = queue.try_drain_dlq();
        assert_eq!(dlq, vec!["dropped".to_string()]);
    }

    #[tokio::test]
    async fn test_dequeue_and_work_returns_none_when_empty() {
        let queue = MemQueue::<i32>::new(unique_name("tq")).unwrap();

        let result = queue
            .dequeue_and_work(|item| async move { Ok::<i32, Box<dyn Error + Send + Sync>>(item) })
            .await
            .unwrap();

        assert_eq!(result, None);
    }

    #[tokio::test]
    async fn test_process_blocking_concurrent_consumers() {
        // Phase 5 invariant: multiple `process_blocking` futures against the
        // same logical queue must process distinct messages concurrently
        // (no two consumers see the same item, and total work is the union).
        let queue = Arc::new(MemQueue::<i32>::new(unique_name("tq")).unwrap());

        let n_items = 32_i32;
        let n_consumers = 8_usize;

        for i in 0..n_items {
            queue.enqueue(i).await.unwrap();
        }

        let seen: Arc<tokio::sync::Mutex<Vec<i32>>> = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let mut handles = Vec::new();

        for _ in 0..n_consumers {
            let q = Arc::clone(&queue);
            let seen = Arc::clone(&seen);
            handles.push(tokio::spawn(async move {
                loop {
                    let r = tokio::time::timeout(
                        std::time::Duration::from_millis(100),
                        q.process_blocking(|item: i32| {
                            let seen = Arc::clone(&seen);
                            async move {
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
        let expected: Vec<i32> = (0..n_items).collect();
        assert_eq!(got, expected, "all items should be processed exactly once");
    }

    #[tokio::test]
    async fn test_recv_async_blocks_until_item_available() {
        let queue = MemQueue::<i32>::new(unique_name("tq")).unwrap();
        let q2 = queue.clone();

        let handle = tokio::spawn(async move { q2.recv_async().await });

        // Briefly allow the worker to park on recv().
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        queue.enqueue(42).await.unwrap();

        let result = tokio::time::timeout(std::time::Duration::from_millis(500), handle)
            .await
            .expect("recv_async should wake on enqueue")
            .unwrap()
            .unwrap();
        assert_eq!(result, 42);
    }
}

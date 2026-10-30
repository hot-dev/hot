//! One queue contract, checked against every `ProcessingQueue` backend.
//!
//! Each case in `cases` runs once per backend: `memory_queue::<case>`,
//! `sqlite_queue::<case>`, and `redis_queue::<case>`. Where the backends differ by
//! design (a startup window, purging old pending work, timing fields), the
//! case states each backend's expected behavior next to the assertion.
//!
//! The Redis cases use `HOT_REDIS_URI`, then `REDIS_URL`, then
//! `redis://127.0.0.1:6379`, and skip when no Redis answers.

use hot::data::serialization::Serialization;
use hot::queue::mem::MemQueue;
use hot::queue::sqlite::{SqliteQueue, SqliteQueueAdmin};
use hot::queue::streams::RedisStreamQueue;
use hot::queue::{
    ProcessingQueue, ProcessingQueueLease, Queue, QueueInfrastructureError, QueueProcessingError,
};
use serde::{Deserialize, Serialize};
use std::error::Error;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use uuid::Uuid;

type BoxError = Box<dyn Error + Send + Sync>;

/// How long a case waits for a delivery it expects.
const DELIVERY_TIMEOUT: Duration = Duration::from_secs(3);
/// How long a case waits before concluding nothing more will be delivered.
const QUIET: Duration = Duration::from_millis(600);
/// Lease idle time before another consumer may reclaim it. SQLite's floor.
const ORPHAN_IDLE: Duration = Duration::from_secs(1);
/// Attempts before a message that keeps failing is dead-lettered.
const MAX_ATTEMPTS: usize = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Backend {
    Memory,
    Sqlite,
    Redis,
}

impl Backend {
    /// Whether queued work outlives the process, so a lease can be orphaned
    /// by a worker that stopped and reclaimed by another.
    fn is_durable(self) -> bool {
        self != Backend::Memory
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Job {
    id: u32,
}

/// One queue on one backend, with handles for as many consumers as a case
/// needs.
struct Harness {
    backend: Backend,
    name: String,
    directory: Option<tempfile::TempDir>,
    redis: Option<redis::Client>,
    /// Memory dead letters drained so far; draining is the only way to see them.
    memory_dead_letters: Mutex<usize>,
}

impl Harness {
    /// None when the backend is unavailable here (no Redis).
    async fn start(backend: Backend) -> Option<Self> {
        let redis = match backend {
            Backend::Redis => Some(redis_client().await?),
            _ => None,
        };
        let directory = match backend {
            Backend::Sqlite => Some(tempfile::tempdir().expect("temporary queue directory")),
            _ => None,
        };
        Some(Self {
            backend,
            name: format!("conformance-{}", Uuid::now_v7()),
            directory,
            redis,
            memory_dead_letters: Mutex::new(0),
        })
    }

    /// A handle on this queue for the named consumer.
    fn consumer(&self, consumer: &str) -> ProcessingQueue<Job> {
        let queue = match self.backend {
            Backend::Memory => ProcessingQueue::Memory(
                MemQueue::new(self.name.clone())
                    .expect("memory queue")
                    .with_serialization(Serialization::Json),
            ),
            Backend::Sqlite => ProcessingQueue::Sqlite(
                SqliteQueue::new_at(self.name.clone(), self.directory_path())
                    .expect("SQLite queue")
                    .with_serialization(Serialization::Json),
            ),
            Backend::Redis => ProcessingQueue::Redis(Box::new(
                RedisStreamQueue::new(self.redis.clone().expect("Redis client"), self.name.clone())
                    .with_serialization(Serialization::Json),
            )),
        };
        queue
            .with_consumer_name(consumer.to_string())
            .with_orphan_idle_ms(ORPHAN_IDLE.as_millis() as u64)
    }

    fn queue(&self) -> ProcessingQueue<Job> {
        self.consumer("conformance-main")
    }

    fn directory_path(&self) -> std::path::PathBuf {
        self.directory
            .as_ref()
            .expect("SQLite queue directory")
            .path()
            .to_path_buf()
    }

    /// Messages in this queue's dead-letter store.
    async fn dead_letters(&self) -> usize {
        match self.backend {
            Backend::Memory => {
                let ProcessingQueue::Memory(queue) = self.queue() else {
                    unreachable!();
                };
                let mut total = self.memory_dead_letters.lock().unwrap();
                *total += queue.try_drain_dlq().len();
                *total
            }
            Backend::Sqlite => {
                let summary = SqliteQueueAdmin::new(self.directory_path())
                    .status()
                    .await
                    .expect("SQLite queue status");
                summary
                    .queues
                    .iter()
                    .find(|queue| queue.name == self.name)
                    .map_or(0, |queue| queue.deadletter as usize)
            }
            Backend::Redis => {
                let mut conn = self.redis_connection().await;
                redis::cmd("XLEN")
                    .arg(format!("{{{}}}:deadletter", self.name))
                    .query_async(&mut conn)
                    .await
                    .unwrap_or(0)
            }
        }
    }

    async fn redis_connection(&self) -> redis::aio::MultiplexedConnection {
        self.redis
            .as_ref()
            .expect("Redis client")
            .get_multiplexed_async_connection()
            .await
            .expect("Redis connection")
    }

    async fn cleanup(self) {
        if self.backend == Backend::Redis {
            let mut conn = self.redis_connection().await;
            let stream = format!("{{{}}}", self.name);
            let _: redis::RedisResult<()> = redis::cmd("DEL")
                .arg(&stream)
                .arg(format!("{}:deadletter", stream))
                .query_async(&mut conn)
                .await;
        }
    }
}

async fn redis_client() -> Option<redis::Client> {
    let uri = std::env::var("HOT_REDIS_URI")
        .or_else(|_| std::env::var("REDIS_URL"))
        .unwrap_or_else(|_| "redis://127.0.0.1:6379".to_string());
    let client = redis::Client::open(uri.as_str()).ok()?;
    match client.get_multiplexed_async_connection().await {
        Ok(_) => Some(client),
        Err(error) => {
            eprintln!("skipping Redis queue conformance: {error}");
            None
        }
    }
}

/// The next lease within `within`, or None if nothing is delivered.
async fn claim(
    queue: &ProcessingQueue<Job>,
    within: Duration,
) -> Option<ProcessingQueueLease<Job>> {
    let deadline = Instant::now() + within;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return None;
        }
        match tokio::time::timeout(remaining, queue.claim_blocking()).await {
            Err(_elapsed) => return None,
            Ok(Ok(Some(lease))) => return Some(lease),
            // Redis returns after its own blocking window.
            Ok(Ok(None)) => continue,
            // Memory dead-letters an exhausted message when it is claimed.
            Ok(Err(error))
                if matches!(
                    error.downcast_ref::<QueueProcessingError>(),
                    Some(QueueProcessingError::RetryLimitExceeded)
                ) =>
            {
                continue;
            }
            Ok(Err(error)) => panic!("claim failed: {error}"),
        }
    }
}

async fn expect_claim(queue: &ProcessingQueue<Job>) -> ProcessingQueueLease<Job> {
    claim(queue, DELIVERY_TIMEOUT)
        .await
        .expect("a message should be delivered")
}

async fn expect_nothing(queue: &ProcessingQueue<Job>) {
    if let Some(lease) = claim(queue, QUIET).await {
        let job = lease
            .process(|job| async move { Ok::<_, BoxError>(job) })
            .await;
        panic!("expected no delivery, got {job:?}");
    }
}

async fn succeed(lease: ProcessingQueueLease<Job>) -> Job {
    lease
        .process(|job| async move { Ok::<_, BoxError>(job) })
        .await
        .expect("processing should succeed")
        .expect("a processed job")
}

async fn fail(lease: ProcessingQueueLease<Job>) {
    let result = lease
        .process(|_| async { Err::<Job, BoxError>("handler failed".into()) })
        .await;
    assert!(result.is_err(), "a failed handler reports an error");
}

async fn fail_for_infrastructure(lease: ProcessingQueueLease<Job>, backoff: Duration) {
    let result = lease
        .process(move |_| async move {
            Err::<Job, BoxError>(Box::new(QueueInfrastructureError::new(
                "dependency unavailable",
                backoff,
            )))
        })
        .await;
    assert!(result.is_err(), "an infrastructure retry reports an error");
}

mod cases {
    use super::*;

    pub async fn delivers_a_message_once(harness: &Harness) {
        let queue = harness.queue();
        queue.enqueue(Job { id: 1 }).await.unwrap();

        let lease = expect_claim(&queue).await;
        assert!(!lease.timing().redelivered);
        assert_eq!(succeed(lease).await, Job { id: 1 });

        expect_nothing(&queue).await;
        assert_eq!(harness.dead_letters().await, 0);
    }

    pub async fn failed_message_is_redelivered(harness: &Harness) {
        let queue = harness.queue();
        queue.enqueue(Job { id: 1 }).await.unwrap();

        fail(expect_claim(&queue).await).await;

        let lease = expect_claim(&queue).await;
        assert!(lease.timing().redelivered);
        assert_eq!(succeed(lease).await, Job { id: 1 });
        expect_nothing(&queue).await;
    }

    /// Counts handler runs rather than leases: Redis hands out one more
    /// lease for an exhausted message and dead-letters it in `process`
    /// without running the handler.
    pub async fn failing_message_is_dead_lettered(harness: &Harness) {
        let queue = harness.queue();
        queue.enqueue(Job { id: 1 }).await.unwrap();

        let runs = Arc::new(AtomicUsize::new(0));
        let mut leases = 0;
        while let Some(lease) = claim(&queue, DELIVERY_TIMEOUT).await {
            leases += 1;
            assert!(leases <= MAX_ATTEMPTS + 1, "delivered past the retry limit");
            let runs = Arc::clone(&runs);
            let result = lease
                .process(move |_| async move {
                    runs.fetch_add(1, Ordering::SeqCst);
                    Err::<Job, BoxError>("handler failed".into())
                })
                .await;
            assert!(result.is_err(), "a failed handler reports an error");
        }

        assert_eq!(runs.load(Ordering::SeqCst), MAX_ATTEMPTS);
        assert_eq!(harness.dead_letters().await, 1);
    }

    pub async fn dropped_lease_is_redelivered(harness: &Harness) {
        let queue = harness.queue();
        queue.enqueue(Job { id: 1 }).await.unwrap();

        drop(expect_claim(&queue).await);

        let lease = expect_claim(&queue).await;
        assert!(lease.timing().redelivered);
        assert_eq!(succeed(lease).await, Job { id: 1 });
        expect_nothing(&queue).await;
    }

    /// A handler that panics drops its lease while unwinding. Dropping must
    /// spend the retry budget, or a deterministic panic redelivers forever.
    pub async fn panicking_handler_is_dead_lettered(harness: &Harness) {
        // Shared, not cloned: a Redis clone is a new consumer, and the
        // dropped delivery stays pending on the consumer that took it.
        let queue = Arc::new(harness.queue());
        queue.enqueue(Job { id: 1 }).await.unwrap();

        let runs = Arc::new(AtomicUsize::new(0));
        for _ in 0..=MAX_ATTEMPTS + 1 {
            let queue = Arc::clone(&queue);
            let runs = Arc::clone(&runs);
            let delivered = tokio::spawn(async move {
                let lease = claim(&queue, DELIVERY_TIMEOUT).await?;
                // Redis dead-letters its extra lease here without a run.
                let _ = lease
                    .process(move |_| async move {
                        runs.fetch_add(1, Ordering::SeqCst);
                        panic!("handler panicked");
                        #[allow(unreachable_code)]
                        Ok::<Job, BoxError>(Job { id: 0 })
                    })
                    .await;
                Some(())
            })
            .await;
            match delivered {
                Ok(None) => break,
                Ok(Some(())) => {}
                Err(error) => assert!(error.is_panic()),
            }
        }

        assert_eq!(runs.load(Ordering::SeqCst), MAX_ATTEMPTS);
        assert_eq!(harness.dead_letters().await, 1);
        expect_nothing(&queue).await;
    }

    pub async fn infrastructure_retry_keeps_the_retry_budget(harness: &Harness) {
        let queue = harness.queue();
        queue.enqueue(Job { id: 1 }).await.unwrap();

        for _ in 0..MAX_ATTEMPTS + 2 {
            fail_for_infrastructure(expect_claim(&queue).await, Duration::ZERO).await;
        }

        let lease = expect_claim(&queue).await;
        assert!(lease.timing().redelivered);
        assert_eq!(succeed(lease).await, Job { id: 1 });
        expect_nothing(&queue).await;
        assert_eq!(harness.dead_letters().await, 0);
    }

    pub async fn infrastructure_retry_waits_for_its_backoff(harness: &Harness) {
        let queue = harness.queue();
        queue.enqueue(Job { id: 1 }).await.unwrap();
        let backoff = Duration::from_millis(400);

        let lease = expect_claim(&queue).await;
        let failed_at = Instant::now();
        fail_for_infrastructure(lease, backoff).await;

        let lease = expect_claim(&queue).await;
        assert!(
            failed_at.elapsed() >= backoff,
            "redelivered {:?} after an infrastructure retry asking for {:?}",
            failed_at.elapsed(),
            backoff
        );
        succeed(lease).await;
    }

    pub async fn orphaned_lease_is_reclaimed_after_idle(harness: &Harness) {
        if !harness.backend.is_durable() {
            return;
        }
        let stopped = harness.consumer("conformance-stopped");
        let survivor = harness.consumer("conformance-survivor");
        stopped.enqueue(Job { id: 1 }).await.unwrap();

        // A worker that stops mid-message never releases its lease.
        std::mem::forget(expect_claim(&stopped).await);

        // A live lease is not reclaimed.
        assert_eq!(survivor.recover_orphaned_items().await.unwrap(), 0);
        expect_nothing(&survivor).await;

        tokio::time::sleep(ORPHAN_IDLE + Duration::from_millis(300)).await;
        assert_eq!(survivor.recover_orphaned_items().await.unwrap(), 1);

        let lease = expect_claim(&survivor).await;
        assert!(lease.timing().redelivered);
        assert_eq!(succeed(lease).await, Job { id: 1 });
        expect_nothing(&survivor).await;
    }

    pub async fn concurrent_consumers_process_each_message_once(harness: &Harness) {
        const MESSAGES: u32 = 40;
        const CONSUMERS: usize = 4;
        let producer = harness.queue();
        for id in 0..MESSAGES {
            producer.enqueue(Job { id }).await.unwrap();
        }

        let processed = Arc::new(Mutex::new(Vec::new()));
        let workers = (0..CONSUMERS).map(|index| {
            let queue = harness.consumer(&format!("conformance-worker-{index}"));
            let processed = Arc::clone(&processed);
            async move {
                while let Some(lease) = claim(&queue, QUIET).await {
                    let job = succeed(lease).await;
                    processed.lock().unwrap().push(job.id);
                }
            }
        });
        futures::future::join_all(workers).await;

        let mut processed = processed.lock().unwrap().clone();
        processed.sort_unstable();
        assert_eq!(processed, (0..MESSAGES).collect::<Vec<_>>());
    }

    pub async fn lease_reports_how_long_the_message_waited(harness: &Harness) {
        let queue = harness.queue();
        queue.enqueue(Job { id: 1 }).await.unwrap();
        let waited = Duration::from_millis(250);
        tokio::time::sleep(waited).await;

        let before_claim = chrono::Utc::now();
        let lease = expect_claim(&queue).await;
        let timing = lease.timing();
        assert!(!timing.redelivered);
        assert!(
            timing.queue_wait >= waited - Duration::from_millis(50),
            "queue wait {:?} is shorter than the {:?} the message waited",
            timing.queue_wait,
            waited
        );
        assert!(timing.claimed_at >= before_claim - chrono::Duration::milliseconds(50));
        // Memory has no wall-clock enqueue time.
        assert_eq!(
            timing.enqueued_at.is_some(),
            harness.backend != Backend::Memory
        );
        succeed(lease).await;
    }

    /// A worker processes one message, stops, and restarts after a backlog
    /// built up. Redis skips the backlog older than the startup window.
    /// SQLite keeps it, because its rows are the authoritative copy. Memory
    /// keeps it here only because the process never stopped.
    pub async fn startup_window_applies_only_to_redis(harness: &Harness) {
        let window = Duration::from_millis(500);
        // Redis fast-forwards only past max(1s, window / 100) of slack.
        let stale = window + Duration::from_millis(1_300);
        let worker = harness.queue();
        worker.enqueue(Job { id: 0 }).await.unwrap();
        succeed(expect_claim(&worker).await).await;
        worker.enqueue(Job { id: 1 }).await.unwrap();
        tokio::time::sleep(stale).await;
        worker.enqueue(Job { id: 2 }).await.unwrap();

        let restarted = harness
            .consumer("conformance-restarted")
            .with_startup_window(window);
        let skipped = restarted.fast_forward_if_stale().await.unwrap();

        let mut delivered = Vec::new();
        while let Some(lease) = claim(&restarted, QUIET).await {
            delivered.push(succeed(lease).await.id);
        }
        delivered.sort_unstable();
        if harness.backend == Backend::Redis {
            assert_eq!(skipped, 1);
            assert_eq!(delivered, [2]);
        } else {
            assert_eq!(skipped, 0);
            assert_eq!(delivered, [1, 2]);
        }
    }

    /// Redis acknowledges and discards pending work older than the given
    /// age, so a stopped worker's message is never reclaimed. SQLite keeps
    /// it for reclaim. Memory has nothing to purge or reclaim.
    pub async fn purge_discards_old_pending_only_on_redis(harness: &Harness) {
        if !harness.backend.is_durable() {
            return;
        }
        let stopped = harness.consumer("conformance-stopped");
        let survivor = harness.consumer("conformance-survivor");
        stopped.enqueue(Job { id: 1 }).await.unwrap();
        std::mem::forget(expect_claim(&stopped).await);
        tokio::time::sleep(Duration::from_millis(300)).await;

        let purged = survivor.purge_old_pending(100).await.unwrap();

        tokio::time::sleep(ORPHAN_IDLE).await;
        survivor.recover_orphaned_items().await.unwrap();
        if harness.backend == Backend::Redis {
            // Not exact: the count includes each purged entry twice, once
            // acknowledged and once deleted from the stream.
            assert!(purged >= 1);
            expect_nothing(&survivor).await;
        } else {
            assert_eq!(purged, 0);
            assert_eq!(succeed(expect_claim(&survivor).await).await, Job { id: 1 });
        }
    }

    pub async fn moved_message_lands_in_the_dead_letters(harness: &Harness) {
        let queue = harness.queue();
        queue
            .move_to_dead_letter_queue(Job { id: 1 }, "operator request".to_string())
            .await
            .unwrap();

        assert_eq!(harness.dead_letters().await, 1);
        expect_nothing(&queue).await;
    }
}

macro_rules! backend_tests {
    ($module:ident, $backend:expr, [$($case:ident),* $(,)?]) => {
        mod $module {
            use super::*;
            $(
                #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
                async fn $case() {
                    let Some(harness) = Harness::start($backend).await else {
                        return;
                    };
                    cases::$case(&harness).await;
                    harness.cleanup().await;
                }
            )*
        }
    };
}

macro_rules! conformance {
    ($($case:ident),* $(,)?) => {
        backend_tests!(memory_queue, Backend::Memory, [$($case),*]);
        backend_tests!(sqlite_queue, Backend::Sqlite, [$($case),*]);
        backend_tests!(redis_queue, Backend::Redis, [$($case),*]);
    };
}

conformance!(
    delivers_a_message_once,
    failed_message_is_redelivered,
    failing_message_is_dead_lettered,
    dropped_lease_is_redelivered,
    panicking_handler_is_dead_lettered,
    infrastructure_retry_keeps_the_retry_budget,
    infrastructure_retry_waits_for_its_backoff,
    orphaned_lease_is_reclaimed_after_idle,
    concurrent_consumers_process_each_message_once,
    lease_reports_how_long_the_message_waited,
    startup_window_applies_only_to_redis,
    purge_discards_old_pending_only_on_redis,
    moved_message_lands_in_the_dead_letters,
);

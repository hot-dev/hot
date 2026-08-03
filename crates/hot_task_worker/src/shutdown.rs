//! Task worker shutdown coordinator.
//!
//! Orchestrates graceful drain when the worker process receives SIGTERM
//! (typically from ECS during deploy or scale-in).
//!
//! ## Flow
//!
//! ```text
//! T+0  : SIGTERM received
//!         ├─ set is_shutting_down=true   (workers stop accepting new dequeues)
//!         └─ initiate_shutdown() begins
//!
//! T+0..30s: wait up to CODE_DRAIN_SECS for in-flight tasks to finish naturally
//!           (most user code-tasks are sub-second; container tasks rarely)
//!
//! T+30s  : signal cancel_token on every still-active task (cooperative
//!          interrupt for the Hot VM and the box executor)
//!
//! T+33s  : grace window for cancellation to land
//!
//! T+33s..: for each task still registered:
//!            1. in one DB transaction, create the queued infra-retry child
//!               and mark the original failed (or persist an honest exhausted
//!               result when MAX_INFRA_RETRY_GENERATIONS has been consumed)
//!            2. publish task:complete from that durable outcome
//!            3. enqueue the already-durable child onto {hot:task}; a crash or
//!               timeout here is repaired by queued-task reconciliation
//!
//! T+~50s : If Redis reports that our consumer has no pending messages,
//!          XGROUP DELCONSUMER our consumer name on {hot:task}. If pending
//!          messages remain, leave the consumer registered so a later
//!          janitor/XAUTOCLAIM pass can recover them.
//!
//! T+~55s : process exits cleanly. ECS stopTimeout is 120s, so we have
//!          ~65s of slack.
//! ```
//!
//! ## Why re-enqueue instead of leave-in-PEL
//!
//! Two reasons:
//!
//! 1. **Speed**. Leaving entries in PEL relies on another worker's janitor
//!    XAUTOCLAIM-ing them after `ORPHAN_IDLE_MS` (60s). The new instance
//!    might not even be up yet during a deploy. Re-enqueueing makes the
//!    work *immediately* available to any live worker.
//!
//! 2. **Cleanliness**. We can DELCONSUMER our own consumer at the end if
//!    Redis confirms there are no pending entries left for that consumer,
//!    so the consumer group doesn't accumulate ghost entries.
//!
//! ## Container vs code task handling
//!
//! Earlier versions of this coordinator left container tasks running on
//! shutdown, betting that another worker would adopt them via heartbeat.
//! That doesn't work when the *instance* is being terminated (which is the
//! common case): the Kata VM and its data volume are bound to the host.
//! So we now treat container tasks the same as code tasks — re-enqueue and
//! fail the original. Most container tasks (`::box/start`) are designed to
//! be idempotent; re-running on a fresh worker is the correct behavior.

use hot::db::{DatabasePool, InfraRetryFinalizeOutcome, Task};
use hot::lang::hot::task::TaskRequest;
use hot::queue::{ConsumerLifecycle, ProcessingQueue, Queue};
use hot::stream::{EnvEvent, EnvPublisher, StreamPubSub};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use tokio::time::{Duration, Instant, timeout};
use uuid::Uuid;

/// How long we wait for in-flight tasks to finish naturally before
/// signalling cancellation. Reduced from 90s to 30s based on observation
/// that the long tail of code tasks doesn't finish in 90s either — beyond
/// ~30s we're better off cancelling and retrying than continuing to wait.
pub const DEFAULT_CODE_DRAIN_SECS: u64 = 30;

/// Brief grace period after signalling cancel_token before we start
/// failing tasks. Lets the cooperative-cancel path actually reach the VM.
const CANCEL_GRACE_SECS: u64 = 3;

/// Per-task DB / stream timeout during shutdown. Bounded so a single slow
/// DB call can't push the whole drain past the ECS stopTimeout window.
const SHUTDOWN_OP_TIMEOUT_SECS: u64 = 5;

/// Maximum number of budget-exempt shutdown retries in one task lineage.
/// Each retry is a fresh queue entry, so Redis delivery counts cannot enforce
/// this policy across generations; the count is persisted on the task row.
const MAX_INFRA_RETRY_GENERATIONS: i16 = 3;

/// Reason string written to the original task's failure result and to the
/// task:complete stream event. Distinct from user-error reasons so dashboards
/// and retry analytics can separate "infra interruption" from "user bug".
const SHUTDOWN_REASON: &str = "Task interrupted by worker shutdown — re-enqueued for retry";

/// Reason string for a lineage that has already consumed all
/// [`MAX_INFRA_RETRY_GENERATIONS`] budget-exempt retries. Unlike
/// [`SHUTDOWN_REASON`] this is a *permanent* failure — no fresh generation is
/// enqueued — so the payload must not claim a re-run is coming: it carries
/// `infra_interrupted: false` (alerting that suppresses transient
/// interruptions must fire for a dead lineage) plus `infra_retry_exhausted:
/// true` so event consumers can still tell exhaustion apart from a user bug.
fn shutdown_exhausted_reason() -> String {
    format!(
        "Task interrupted by worker shutdown — infrastructure retry budget exhausted ({} generations); not re-enqueued",
        MAX_INFRA_RETRY_GENERATIONS,
    )
}

/// Metadata tracked for each active task during shutdown.
///
/// `original_request` is the full `TaskRequest` payload as it was originally
/// dequeued. We keep it so the shutdown path can re-enqueue an identical
/// retry without round-tripping through the DB to reassemble the args.
#[derive(Clone)]
pub struct ActiveTask {
    pub task_id: Uuid,
    pub env_id: Uuid,
    pub stream_id: Uuid,
    pub function_name: String,
    pub task_type: String,
    pub cancel_token: Option<Arc<AtomicBool>>,
    pub original_request: TaskRequest,
}

#[derive(Clone)]
pub struct TaskShutdownCoordinator {
    active_tasks: Arc<RwLock<Vec<ActiveTask>>>,
    shutdown_initiated: Arc<AtomicBool>,
    /// How long to wait for in-flight tasks to finish before cancelling.
    /// Constructor-overrideable so tests can drive the flow without
    /// waiting 30 real seconds.
    code_drain_timeout: Duration,
}

impl TaskShutdownCoordinator {
    /// Construct with the production default drain (30s).
    pub fn new() -> Self {
        Self::with_drain_secs(DEFAULT_CODE_DRAIN_SECS)
    }

    /// Construct with a custom drain timeout (used by tests).
    pub fn with_drain_secs(code_drain_timeout_secs: u64) -> Self {
        Self {
            active_tasks: Arc::new(RwLock::new(Vec::new())),
            shutdown_initiated: Arc::new(AtomicBool::new(false)),
            code_drain_timeout: Duration::from_secs(code_drain_timeout_secs),
        }
    }

    pub fn register_task(&self, task: ActiveTask) {
        if let Ok(mut tasks) = self.active_tasks.write() {
            tasks.push(task);
        }
    }

    /// Like [`register_task`], but refuses to register a task whose
    /// `task_id` is already in flight. Returns `true` on success and
    /// `false` if a duplicate was rejected.
    ///
    /// Guards against the queue redelivering the same `task_id` while a
    /// previous invocation is still running. Concurrent invocations would
    /// race on shared per-task resources such as data volumes, container
    /// labels, and DB rows.
    pub fn try_register_task(&self, task: ActiveTask) -> bool {
        if let Ok(mut tasks) = self.active_tasks.write() {
            if tasks.iter().any(|t| t.task_id == task.task_id) {
                return false;
            }
            tasks.push(task);
            true
        } else {
            false
        }
    }

    pub fn unregister_task(&self, task_id: &Uuid) {
        if let Ok(mut tasks) = self.active_tasks.write() {
            tasks.retain(|t| &t.task_id != task_id);
        }
    }

    /// Return whether this process is still actively executing `task_id`.
    pub fn is_task_active(&self, task_id: &Uuid) -> bool {
        self.active_tasks
            .read()
            .map(|tasks| tasks.iter().any(|task| &task.task_id == task_id))
            .unwrap_or(false)
    }

    pub fn is_shutting_down(&self) -> bool {
        self.shutdown_initiated.load(Ordering::Acquire)
    }

    /// Set the VM cancel token for a registered task (called after VM spawn).
    pub fn set_cancel_token(&self, task_id: &Uuid, cancel_token: Arc<AtomicBool>) {
        if let Ok(mut tasks) = self.active_tasks.write()
            && let Some(entry) = tasks.iter_mut().find(|t| &t.task_id == task_id)
        {
            entry.cancel_token = Some(cancel_token);
        }
    }

    /// Signal cooperative cancellation for one active task, if it has
    /// registered a VM/container cancel token.
    pub fn cancel_task(&self, task_id: &Uuid) {
        if let Ok(tasks) = self.active_tasks.read()
            && let Some(token) = tasks
                .iter()
                .find(|t| &t.task_id == task_id)
                .and_then(|t| t.cancel_token.as_ref())
        {
            token.store(true, Ordering::Release);
        }
    }

    fn snapshot_active_tasks(&self) -> Vec<ActiveTask> {
        self.active_tasks
            .read()
            .map(|tasks| tasks.iter().cloned().collect())
            .unwrap_or_default()
    }

    fn active_count(&self) -> usize {
        self.active_tasks.read().map(|t| t.len()).unwrap_or(0)
    }

    /// Initiate graceful shutdown.
    ///
    /// See module docs for the full timeline. Returns when one of:
    ///   - all tasks finished naturally, OR
    ///   - we've cancelled, retried, and unregistered the consumer.
    pub async fn initiate_shutdown(
        &self,
        db: &DatabasePool,
        stream_publisher: &StreamPubSub,
        task_queue: &ProcessingQueue<TaskRequest>,
    ) {
        if self.shutdown_initiated.swap(true, Ordering::AcqRel) {
            tracing::info!("Task worker shutdown already initiated, skipping duplicate signal");
            return;
        }

        let initial = self.snapshot_active_tasks();
        tracing::info!(
            "Task worker shutdown initiated: {} task(s) in-flight (drain timeout {}s)",
            initial.len(),
            self.code_drain_timeout.as_secs(),
        );

        if initial.is_empty() {
            // Still unregister our consumer so the next worker doesn't
            // see us as an idle ghost in XINFO CONSUMERS.
            Self::unregister_consumer(task_queue).await;
            return;
        }

        // Phase 1: drain — wait for natural completion.
        self.drain_phase().await;

        // Phase 2: cancel anything still running.
        let remaining = self.snapshot_active_tasks();
        if remaining.is_empty() {
            tracing::info!("All in-flight tasks completed during drain window");
            Self::unregister_consumer(task_queue).await;
            return;
        }

        tracing::warn!(
            "Drain timeout reached, cancelling {} remaining task(s)",
            remaining.len(),
        );
        for task in &remaining {
            if let Some(cancel) = &task.cancel_token {
                cancel.store(true, Ordering::Relaxed);
            }
        }
        tokio::time::sleep(Duration::from_secs(CANCEL_GRACE_SECS)).await;

        // Phase 3: re-enqueue + fail anything still registered.
        let to_finalize = self.snapshot_active_tasks();
        if !to_finalize.is_empty() {
            tracing::warn!(
                "Re-enqueueing and failing {} task(s) interrupted by shutdown",
                to_finalize.len(),
            );
            for task in to_finalize {
                Self::finalize_interrupted_task(db, stream_publisher, task_queue, &task).await;
                self.unregister_task(&task.task_id);
            }
        }

        // Phase 4: clean Redis state only if the consumer has no pending
        // entries left. If Redis still sees PEL entries, leave the consumer
        // registered so another worker's janitor can XAUTOCLAIM them.
        Self::unregister_consumer(task_queue).await;

        tracing::info!(
            "Task worker shutdown complete (final active count: {})",
            self.active_count(),
        );
    }

    async fn drain_phase(&self) {
        let start = Instant::now();
        let _ = timeout(self.code_drain_timeout, async {
            loop {
                if self.active_count() == 0 {
                    return;
                }
                let elapsed = start.elapsed().as_secs();
                if elapsed > 0 && elapsed.is_multiple_of(5) {
                    tracing::info!(
                        "Waiting for {} task(s) to complete ({}/{}s)",
                        self.active_count(),
                        elapsed,
                        self.code_drain_timeout.as_secs(),
                    );
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        })
        .await;
    }

    /// Atomically fail the task and make its retry child durable, then publish
    /// and enqueue only from that committed result. A timeout can no longer
    /// leave the original terminal without a durable recovery row.
    async fn finalize_interrupted_task(
        db: &DatabasePool,
        stream_publisher: &StreamPubSub,
        task_queue: &ProcessingQueue<TaskRequest>,
        task: &ActiveTask,
    ) {
        let op_timeout = Duration::from_secs(SHUTDOWN_OP_TIMEOUT_SECS);
        let new_task_id = Uuid::now_v7();
        let retry_error = serde_json::json!({
            "$type": "::hot::task/Failure",
            "$val": {
                "msg": SHUTDOWN_REASON,
                "err": null,
                "infra_interrupted": true,
            }
        });
        let exhausted_error = serde_json::json!({
            "$type": "::hot::task/Failure",
            "$val": {
                "msg": shutdown_exhausted_reason(),
                "err": null,
                "infra_interrupted": false,
                "infra_retry_exhausted": true,
            }
        });

        let outcome = match timeout(
            op_timeout,
            Task::finalize_with_infra_retry(
                db,
                &task.task_id,
                &new_task_id,
                MAX_INFRA_RETRY_GENERATIONS,
                &retry_error,
                &exhausted_error,
                chrono::Utc::now(),
            ),
        )
        .await
        {
            Ok(Ok(outcome)) => outcome,
            Ok(Err(error)) => {
                tracing::error!(
                    task_id = %task.task_id,
                    "Shutdown: atomic terminal/retry transaction failed: {error}"
                );
                return;
            }
            Err(_) => {
                tracing::error!(
                    task_id = %task.task_id,
                    "Shutdown: atomic terminal/retry transaction timed out after {}s; suppressing event and enqueue (a committed retry row will be recovered by reconciliation)",
                    SHUTDOWN_OP_TIMEOUT_SECS,
                );
                return;
            }
        };

        let (error, duration_ms, retry) = match outcome {
            InfraRetryFinalizeOutcome::AlreadyTerminal => {
                tracing::info!(
                    task_id = %task.task_id,
                    "Shutdown: task was already terminal; skipping stale completion event and retry"
                );
                return;
            }
            InfraRetryFinalizeOutcome::Exhausted { duration_ms } => {
                tracing::error!(
                    task_id = %task.task_id,
                    max_infra_retries = MAX_INFRA_RETRY_GENERATIONS,
                    "Shutdown: infra-retry lineage limit reached — task failed permanently"
                );
                (&exhausted_error, duration_ms, None)
            }
            InfraRetryFinalizeOutcome::RetryReady {
                retry_task_id,
                retry_attempt,
                infra_retry_count,
                should_enqueue,
                duration_ms,
            } => (
                &retry_error,
                duration_ms,
                Some((
                    retry_task_id,
                    retry_attempt,
                    infra_retry_count,
                    should_enqueue,
                )),
            ),
        };

        let event = EnvEvent::TaskComplete {
            task_id: task.task_id,
            env_id: task.env_id,
            stream_id: task.stream_id,
            function_name: task.function_name.clone(),
            status: "failed".to_string(),
            duration_ms,
            error: Some(error.clone()),
        };
        match timeout(op_timeout, stream_publisher.publish_env(event)).await {
            Ok(Ok(_)) => {}
            Ok(Err(error)) => tracing::warn!(
                task_id = %task.task_id,
                "Shutdown: failed to publish task:complete: {error}"
            ),
            Err(_) => tracing::error!(
                task_id = %task.task_id,
                "Shutdown: publishing task:complete timed out after {}s",
                SHUTDOWN_OP_TIMEOUT_SECS,
            ),
        }

        let Some((retry_task_id, retry_attempt, infra_retry_count, should_enqueue)) = retry else {
            return;
        };
        if !should_enqueue {
            tracing::info!(
                task_id = %task.task_id,
                retry_task_id = %retry_task_id,
                "Shutdown: retry row already existed; queued-row reconciliation owns delivery"
            );
            return;
        }

        let mut retry_request = task.original_request.clone();
        retry_request.task_id = retry_task_id.to_string();
        retry_request.created_at_unix_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        match timeout(op_timeout, task_queue.enqueue(retry_request)).await {
            Ok(Ok(())) => tracing::info!(
                task_id = %task.task_id,
                retry_task_id = %retry_task_id,
                retry_attempt,
                infra_retry_count,
                "Shutdown: enqueued durable infra retry"
            ),
            Ok(Err(error)) => tracing::error!(
                task_id = %task.task_id,
                retry_task_id = %retry_task_id,
                "Shutdown: failed to enqueue durable infra retry (reconciler will recover it): {error}"
            ),
            Err(_) => {
                tracing::error!(
                    task_id = %task.task_id,
                    retry_task_id = %retry_task_id,
                    "Shutdown: durable infra-retry enqueue timed out (reconciler will recover it)"
                )
            }
        }
    }

    /// Fire-and-log XGROUP DELCONSUMER only when this consumer has no pending
    /// entries. Wrapped in a short timeout — even if Redis is unresponsive we
    /// want to exit promptly so ECS doesn't hard-kill us.
    async fn unregister_consumer(task_queue: &ProcessingQueue<TaskRequest>) {
        let op_timeout = Duration::from_secs(SHUTDOWN_OP_TIMEOUT_SECS);
        match timeout(op_timeout, task_queue.consumer_has_pending()).await {
            Ok(Ok(false)) => match timeout(op_timeout, task_queue.unregister_consumer()).await {
                Ok(Ok(())) => {
                    tracing::info!("Shutdown: unregistered idle consumer from {{hot:task}}");
                }
                Ok(Err(e)) => {
                    tracing::warn!("Shutdown: failed to unregister consumer: {}", e);
                }
                Err(_) => {
                    tracing::warn!(
                        "Shutdown: unregister_consumer timed out after {}s",
                        SHUTDOWN_OP_TIMEOUT_SECS,
                    );
                }
            },
            Ok(Ok(true)) => {
                tracing::warn!(
                    "Shutdown: leaving {{hot:task}} consumer registered because it still has pending messages"
                );
            }
            Ok(Err(e)) => {
                tracing::warn!(
                    "Shutdown: could not inspect consumer pending state before unregister: {}",
                    e
                );
            }
            Err(_) => {
                tracing::warn!(
                    "Shutdown: consumer pending-state check timed out after {}s",
                    SHUTDOWN_OP_TIMEOUT_SECS,
                );
            }
        }
    }
}

impl Default for TaskShutdownCoordinator {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hot::data::serialization::Serialization;
    use hot::db::TaskStatus;
    use hot::queue::QueueType;
    use hot::stream::{EnvSubscriberFactory, StreamPubSubType};

    fn dummy_active_task(task_id: Uuid) -> ActiveTask {
        ActiveTask {
            task_id,
            env_id: Uuid::nil(),
            stream_id: Uuid::nil(),
            function_name: "::test/fn".to_string(),
            task_type: "code".to_string(),
            cancel_token: None,
            original_request: TaskRequest {
                task_id: task_id.to_string(),
                function_name: "::test/fn".to_string(),
                args: serde_json::Value::Null,
                stream_id: Uuid::nil().to_string(),
                env_id: Uuid::nil().to_string(),
                build_id: Uuid::nil().to_string(),
                org_id: None,
                user_id: None,
                project_id: None,
                project_name: None,
                timeout_ms: 1000,
                task_type: "code".to_string(),
                created_at_unix_ms: 0,
                origin_run_id: None,
            },
        }
    }

    #[test]
    fn try_register_task_rejects_duplicate_task_id() {
        let coord = TaskShutdownCoordinator::new();
        let task_id = Uuid::now_v7();

        assert!(coord.try_register_task(dummy_active_task(task_id)));
        assert_eq!(coord.active_count(), 1);
        assert!(coord.is_task_active(&task_id));

        // Second dispatch of the same task_id is rejected — this is the
        // in-process dedup that prevents the data-volume bind-mount race.
        assert!(!coord.try_register_task(dummy_active_task(task_id)));
        assert_eq!(coord.active_count(), 1);

        // After unregister, the same task_id can be registered again
        // (legitimate fresh redelivery once the original is done).
        coord.unregister_task(&task_id);
        assert_eq!(coord.active_count(), 0);
        assert!(!coord.is_task_active(&task_id));
        assert!(coord.try_register_task(dummy_active_task(task_id)));
        assert_eq!(coord.active_count(), 1);
    }

    #[test]
    fn try_register_task_allows_distinct_task_ids() {
        let coord = TaskShutdownCoordinator::new();
        assert!(coord.try_register_task(dummy_active_task(Uuid::now_v7())));
        assert!(coord.try_register_task(dummy_active_task(Uuid::now_v7())));
        assert!(coord.try_register_task(dummy_active_task(Uuid::now_v7())));
        assert_eq!(coord.active_count(), 3);
    }

    fn memory_task_queue() -> ProcessingQueue<TaskRequest> {
        ProcessingQueue::new(
            QueueType::Memory,
            format!("test-task-queue-{}", Uuid::now_v7()),
            None,
            Serialization::Json,
        )
        .expect("memory queue should construct")
    }

    fn memory_stream_pubsub() -> StreamPubSub {
        StreamPubSub::new(StreamPubSubType::Memory, None, false)
            .expect("memory pubsub should construct")
    }

    async fn insert_running_task(db: &DatabasePool, active: &ActiveTask) {
        Task::insert(
            db,
            &active.task_id,
            &active.env_id,
            &active.stream_id,
            &Uuid::parse_str(&active.original_request.build_id).unwrap(),
            None,
            &active.function_name,
            None,
            None,
            &active.task_type,
            active.original_request.timeout_ms as i64,
            None,
        )
        .await
        .unwrap();
        Task::mark_running(db, &active.task_id).await.unwrap();
    }

    async fn claim_enqueued_request(task_queue: &ProcessingQueue<TaskRequest>) -> TaskRequest {
        let lease = tokio::time::timeout(Duration::from_secs(1), task_queue.claim_blocking())
            .await
            .expect("an infra retry must be enqueued")
            .unwrap()
            .expect("memory queue claim should yield a lease");
        lease
            .process(
                |request| async move { Ok::<_, Box<dyn std::error::Error + Send + Sync>>(request) },
            )
            .await
            .unwrap()
            .expect("memory lease should surface the claimed request")
    }

    #[tokio::test]
    async fn already_terminal_task_publishes_nothing_and_enqueues_nothing() {
        let db = hot::db::test_db().await;
        let task_queue = memory_task_queue();
        let stream_publisher = memory_stream_pubsub();
        let active = dummy_active_task(Uuid::now_v7());
        insert_running_task(&db, &active).await;
        let mut env_events = stream_publisher.subscribe_env(active.env_id).await.unwrap();
        assert!(
            Task::complete(
                &db,
                &active.task_id,
                &TaskStatus::Completed,
                None,
                None,
                None,
            )
            .await
            .unwrap()
        );

        TaskShutdownCoordinator::finalize_interrupted_task(
            &db,
            &stream_publisher,
            &task_queue,
            &active,
        )
        .await;

        assert!(
            tokio::time::timeout(Duration::from_millis(100), task_queue.claim_blocking())
                .await
                .is_err(),
            "a task that lost the terminal race must not be re-enqueued"
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(100), env_events.next())
                .await
                .is_err(),
            "a task that lost the terminal race must not publish a stale event"
        );
    }

    #[tokio::test]
    async fn interrupted_task_finalize_fails_row_publishes_event_and_enqueues_retry() {
        let db = hot::db::test_db().await;
        let task_queue = memory_task_queue();
        let stream_publisher = memory_stream_pubsub();
        let active = dummy_active_task(Uuid::now_v7());
        insert_running_task(&db, &active).await;
        let mut env_events = stream_publisher.subscribe_env(active.env_id).await.unwrap();

        TaskShutdownCoordinator::finalize_interrupted_task(
            &db,
            &stream_publisher,
            &task_queue,
            &active,
        )
        .await;

        let row = Task::get(&db, &active.task_id).await.unwrap();
        assert_eq!(row.task_status_id, TaskStatus::Failed.as_id());

        // Under the infra-retry cap the record keeps its historical shape:
        // "re-enqueued for retry" with the alert-suppression marker set.
        let result = row.result.expect("failure result should be set");
        let val = result.get("$val").expect("result should be tagged Failure");
        assert_eq!(
            val.get("msg").and_then(|m| m.as_str()),
            Some(SHUTDOWN_REASON)
        );
        assert_eq!(
            val.get("infra_interrupted").and_then(|v| v.as_bool()),
            Some(true),
        );
        assert!(
            val.get("infra_retry_exhausted").is_none(),
            "an under-cap interruption must not carry the exhaustion marker"
        );

        let event = tokio::time::timeout(Duration::from_secs(1), env_events.next())
            .await
            .expect("a durable terminal write must publish task:complete")
            .expect("subscription should stay open");
        match event {
            EnvEvent::TaskComplete {
                task_id, status, ..
            } => {
                assert_eq!(task_id, active.task_id);
                assert_eq!(status, "failed");
            }
            other => panic!("expected TaskComplete, got {:?}", other),
        }

        let retry = claim_enqueued_request(&task_queue).await;
        assert_ne!(retry.task_id, active.task_id.to_string());
    }

    #[tokio::test]
    async fn infra_retry_skips_enqueue_when_retry_row_already_exists() {
        let db = hot::db::test_db().await;
        let task_queue = memory_task_queue();
        let stream_publisher = memory_stream_pubsub();
        let active = dummy_active_task(Uuid::now_v7());
        insert_running_task(&db, &active).await;

        // A previous drain of the same task (or a racing writer during a DB
        // brownout) already created the infra retry for this parent +
        // attempt, but never enqueued it. Infra retries preserve the
        // parent's retry_attempt, so the duplicate targets the same slot.
        let row = Task::get(&db, &active.task_id).await.unwrap();
        assert!(
            Task::insert_retry(
                &db,
                &Uuid::now_v7(),
                &row,
                row.retry_attempt,
                chrono::Utc::now()
            )
            .await
            .unwrap()
        );

        TaskShutdownCoordinator::finalize_interrupted_task(
            &db,
            &stream_publisher,
            &task_queue,
            &active,
        )
        .await;

        assert!(
            tokio::time::timeout(Duration::from_millis(100), task_queue.claim_blocking())
                .await
                .is_err(),
            "a duplicate infra retry must not be enqueued — the existing row \
             is recovered by reconcile_queued_tasks"
        );
        assert_eq!(
            Task::get_count_by_env(&db, &active.env_id).await.unwrap(),
            2,
            "no second retry row may be created"
        );
    }

    #[tokio::test]
    async fn cap_blocked_finalize_writes_exhausted_record_and_skips_enqueue() {
        let db = hot::db::test_db().await;
        let task_queue = memory_task_queue();
        let stream_publisher = memory_stream_pubsub();
        let root_active = dummy_active_task(Uuid::now_v7());
        insert_running_task(&db, &root_active).await;

        // Walk the lineage to the persisted generation cap, then interrupt
        // the last (running) generation.
        let mut row = Task::get(&db, &root_active.task_id).await.unwrap();
        for _ in 0..MAX_INFRA_RETRY_GENERATIONS {
            let child_id = Uuid::now_v7();
            assert!(
                Task::insert_infra_retry(&db, &child_id, &row, chrono::Utc::now())
                    .await
                    .unwrap()
            );
            row = Task::get(&db, &child_id).await.unwrap();
        }
        Task::mark_running(&db, &row.task_id).await.unwrap();

        let mut capped_active = dummy_active_task(row.task_id);
        capped_active.env_id = row.env_id;
        capped_active.stream_id = row.stream_id;
        capped_active.original_request.env_id = row.env_id.to_string();
        capped_active.original_request.stream_id = row.stream_id.to_string();
        capped_active.original_request.build_id = row.build_id.to_string();

        let mut env_events = stream_publisher
            .subscribe_env(capped_active.env_id)
            .await
            .unwrap();

        TaskShutdownCoordinator::finalize_interrupted_task(
            &db,
            &stream_publisher,
            &task_queue,
            &capped_active,
        )
        .await;

        // The terminal record must be honest: budget exhausted, no re-run
        // coming, and NOT flagged as a transient (alert-suppressed)
        // interruption.
        let failed = Task::get(&db, &row.task_id).await.unwrap();
        assert_eq!(failed.task_status_id, TaskStatus::Failed.as_id());
        let result = failed.result.expect("failure result should be set");
        let val = result.get("$val").expect("result should be tagged Failure");
        assert_eq!(
            val.get("msg").and_then(|m| m.as_str()),
            Some(shutdown_exhausted_reason().as_str()),
        );
        assert_eq!(
            val.get("infra_interrupted").and_then(|v| v.as_bool()),
            Some(false),
            "a dead lineage must not carry the alert-suppression flag"
        );
        assert_eq!(
            val.get("infra_retry_exhausted").and_then(|v| v.as_bool()),
            Some(true),
        );

        // The published task:complete carries the same exhausted payload.
        let event = tokio::time::timeout(Duration::from_secs(1), env_events.next())
            .await
            .expect("a durable terminal write must publish task:complete")
            .expect("subscription should stay open");
        match event {
            EnvEvent::TaskComplete {
                task_id,
                status,
                error,
                ..
            } => {
                assert_eq!(task_id, row.task_id);
                assert_eq!(status, "failed");
                let error = error.expect("exhausted completion must carry the error payload");
                let val = error
                    .get("$val")
                    .expect("event error should be tagged Failure");
                assert_eq!(
                    val.get("infra_retry_exhausted").and_then(|v| v.as_bool()),
                    Some(true),
                );
                assert_eq!(
                    val.get("infra_interrupted").and_then(|v| v.as_bool()),
                    Some(false),
                );
            }
            other => panic!("expected TaskComplete, got {:?}", other),
        }

        // No fresh queue entry and no generation beyond the cap.
        assert!(
            tokio::time::timeout(Duration::from_millis(100), task_queue.claim_blocking())
                .await
                .is_err(),
            "a cap-blocked lineage must not be re-enqueued"
        );
        assert_eq!(
            Task::get_count_by_env(&db, &root_active.env_id)
                .await
                .unwrap(),
            i64::from(MAX_INFRA_RETRY_GENERATIONS) + 1,
            "no generation beyond the cap may be inserted"
        );
    }
}

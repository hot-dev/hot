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
//!            1. enqueue an "infra retry" copy onto {hot:task} (immediate,
//!               no delay, doesn't count against the user's max_retries
//!               budget — this is a system-initiated re-run, not a failure
//!               of the user's code)
//!            2. mark the original task row failed in DB
//!            3. publish task:complete event so consumers see the failure
//!
//! T+~50s : XGROUP DELCONSUMER our consumer name on {hot:task}
//!          (releases any PEL entries we still own — the retry copies we
//!           just enqueued are independent stream entries, so abandoning
//!           the originals is safe)
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
//! 2. **Cleanliness**. We can DELCONSUMER our own consumer at the end so
//!    the consumer group doesn't accumulate ghost entries. The original
//!    PEL entries become unreachable but that's fine — we have fresh
//!    copies in the stream.
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

use hot::db::{DatabasePool, Task, TaskStatus};
use hot::env::retry::RetryConfig;
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
const CODE_DRAIN_SECS: u64 = 30;

/// Brief grace period after signalling cancel_token before we start
/// failing tasks. Lets the cooperative-cancel path actually reach the VM.
const CANCEL_GRACE_SECS: u64 = 3;

/// Per-task DB / stream timeout during shutdown. Bounded so a single slow
/// DB call can't push the whole drain past the ECS stopTimeout window.
const SHUTDOWN_OP_TIMEOUT_SECS: u64 = 5;

/// Reason string written to the original task's failure result and to the
/// task:complete stream event. Distinct from user-error reasons so dashboards
/// and retry analytics can separate "infra interruption" from "user bug".
const SHUTDOWN_REASON: &str = "Task interrupted by worker shutdown — re-enqueued for retry";

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
        Self::with_drain_secs(CODE_DRAIN_SECS)
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

        // Phase 4: clean Redis state. DELCONSUMER releases any PEL entries
        // we still own. The retry copies we enqueued in phase 3 are
        // independent stream entries, so abandoning the originals is safe.
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

    /// Fail the task in DB, publish the task:complete event, and enqueue an
    /// infra-retry copy onto the queue. Each step is wrapped in its own
    /// timeout so a slow DB or Redis can't stall the whole drain.
    async fn finalize_interrupted_task(
        db: &DatabasePool,
        stream_publisher: &StreamPubSub,
        task_queue: &ProcessingQueue<TaskRequest>,
        task: &ActiveTask,
    ) {
        let op_timeout = Duration::from_secs(SHUTDOWN_OP_TIMEOUT_SECS);

        let error = serde_json::json!({
            "$type": "::hot::task/Failure",
            "$val": {
                "$msg": SHUTDOWN_REASON,
                "$err": null,
                "infra_interrupted": true,
            }
        });

        // 1. Mark the original task as failed.
        match timeout(
            op_timeout,
            Task::complete(db, &task.task_id, &TaskStatus::Failed, Some(&error)),
        )
        .await
        {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => {
                tracing::warn!(
                    task_id = %task.task_id,
                    "Shutdown: Task::complete failed: {}", e
                );
            }
            Err(_) => {
                tracing::error!(
                    task_id = %task.task_id,
                    "Shutdown: Task::complete timed out after {}s",
                    SHUTDOWN_OP_TIMEOUT_SECS,
                );
            }
        }

        // 2. Publish the task:complete event so subscribers see it.
        let duration_ms = match timeout(op_timeout, Task::get(db, &task.task_id)).await {
            Ok(Ok(t)) => t.duration_ms,
            _ => None,
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
        if let Err(e) = stream_publisher.publish_env(event).await {
            tracing::warn!(
                task_id = %task.task_id,
                "Shutdown: failed to publish task:complete: {}", e
            );
        }

        // 3. Enqueue an infra-retry. We re-enqueue regardless of the user's
        // `retry` meta — this isn't a user-error retry, it's a system-
        // initiated re-run. We DO still cap effective replay via the queue's
        // own MAX_PROCESSING_RETRIES (delivery count) to prevent infinite
        // shutdown-driven loops if a task somehow keeps getting interrupted.
        Self::enqueue_infra_retry(db, task_queue, task).await;
    }

    /// Insert a retry task row + enqueue onto the queue. Distinct from the
    /// user-error retry path (`maybe_retry_task` in lib.rs) in two ways:
    ///   - bypasses the user's `max_retries` cap (infra retry is free),
    ///   - keeps `retry_attempt` from the original (this isn't "the user's
    ///     2nd try", it's "the infra's 1st re-attempt of the user's nth try").
    async fn enqueue_infra_retry(
        db: &DatabasePool,
        task_queue: &ProcessingQueue<TaskRequest>,
        task: &ActiveTask,
    ) {
        let op_timeout = Duration::from_secs(SHUTDOWN_OP_TIMEOUT_SECS);

        // We need the task row to insert the retry — it carries env_id,
        // stream_id, build_id, options, etc. that aren't all on TaskRequest.
        let task_row = match timeout(op_timeout, Task::get(db, &task.task_id)).await {
            Ok(Ok(t)) => t,
            Ok(Err(e)) => {
                tracing::warn!(
                    task_id = %task.task_id,
                    "Shutdown: Task::get failed (won't retry): {}", e
                );
                return;
            }
            Err(_) => {
                tracing::error!(
                    task_id = %task.task_id,
                    "Shutdown: Task::get timed out — skipping retry"
                );
                return;
            }
        };

        // Carry the same retry_attempt as the original. Use the user's
        // configured retry delay for backoff if they set one, otherwise
        // re-enqueue immediately (infra interrupt isn't an error to back
        // off from).
        let retry_config = RetryConfig::from_meta(task_row.options.as_ref());
        let next_attempt = task_row.retry_attempt;
        let next_retry_at = chrono::Utc::now();
        let new_task_id = Uuid::now_v7();

        match timeout(
            op_timeout,
            Task::insert_retry(db, &new_task_id, &task_row, next_attempt, next_retry_at),
        )
        .await
        {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => {
                tracing::error!(
                    task_id = %task.task_id,
                    new_task_id = %new_task_id,
                    "Shutdown: Task::insert_retry failed: {}", e
                );
                return;
            }
            Err(_) => {
                tracing::error!(
                    task_id = %task.task_id,
                    new_task_id = %new_task_id,
                    "Shutdown: Task::insert_retry timed out"
                );
                return;
            }
        }

        let mut retry_request = task.original_request.clone();
        retry_request.task_id = new_task_id.to_string();
        retry_request.created_at_unix_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;

        match timeout(op_timeout, task_queue.enqueue(retry_request)).await {
            Ok(Ok(())) => {
                tracing::info!(
                    task_id = %task.task_id,
                    new_task_id = %new_task_id,
                    attempt = next_attempt,
                    user_max_retries = retry_config.max_retries,
                    "Shutdown: enqueued infra-retry for interrupted task"
                );
            }
            Ok(Err(e)) => {
                tracing::error!(
                    task_id = %task.task_id,
                    new_task_id = %new_task_id,
                    "Shutdown: failed to enqueue infra-retry: {}", e
                );
            }
            Err(_) => {
                tracing::error!(
                    task_id = %task.task_id,
                    new_task_id = %new_task_id,
                    "Shutdown: enqueue infra-retry timed out"
                );
            }
        }
    }

    /// Fire-and-log XGROUP DELCONSUMER. Wrapped in a short timeout — even
    /// if Redis is unresponsive we want to exit promptly so ECS doesn't
    /// hard-kill us.
    async fn unregister_consumer(task_queue: &ProcessingQueue<TaskRequest>) {
        let op_timeout = Duration::from_secs(SHUTDOWN_OP_TIMEOUT_SECS);
        match timeout(op_timeout, task_queue.unregister_consumer()).await {
            Ok(Ok(())) => {
                tracing::info!("Shutdown: unregistered consumer from {{hot:task}}");
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

        // Second dispatch of the same task_id is rejected — this is the
        // in-process dedup that prevents the data-volume bind-mount race.
        assert!(!coord.try_register_task(dummy_active_task(task_id)));
        assert_eq!(coord.active_count(), 1);

        // After unregister, the same task_id can be registered again
        // (legitimate fresh redelivery once the original is done).
        coord.unregister_task(&task_id);
        assert_eq!(coord.active_count(), 0);
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
}

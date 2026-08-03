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
//!               of the user's code). Skipped when the lineage has already
//!               burned MAX_INFRA_RETRY_GENERATIONS such retries — the
//!               terminal record then says "budget exhausted" instead of
//!               "re-enqueued", so nobody waits on a re-run that won't come.
//!            2. mark the original task row failed in DB
//!            3. publish task:complete event so consumers see the failure
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

use hot::db::{DatabasePool, Task, TaskError, TaskStatus};
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

/// What `finalize_interrupted_task` is allowed to do after the shutdown-time
/// terminal write. The task:complete event may only be published once the
/// Failed state is durably ours (persist-before-event invariant), but the
/// budget-exempt infra retry is the interrupted task's *guaranteed* re-run
/// and must survive an unknown write outcome — dropping it would leave
/// recovery to the zombie reaper, which consumes the user's retry budget or
/// does nothing when retries are unconfigured.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FinalizeOutcome {
    /// Terminal write is durable: publish task:complete, then enqueue retry.
    PublishAndRetry,
    /// Terminal write failed or timed out client-side; the row's state is
    /// unknown (the statement may still commit server-side). Suppress the
    /// event but still enqueue the infra retry.
    RetryOnly,
    /// Another actor already made the task terminal: publish nothing,
    /// retry nothing.
    Skip,
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

    /// Fail the task in DB, publish the task:complete event, and enqueue an
    /// infra-retry copy onto the queue. Each step is wrapped in its own
    /// timeout so a slow DB or Redis can't stall the whole drain.
    ///
    /// A lineage that has already exhausted its infra-retry budget is NOT
    /// re-enqueued, so the cap is decided *before* the terminal payload is
    /// composed: cap-blocked tasks get an honest "budget exhausted" record
    /// instead of one claiming a re-run is coming.
    async fn finalize_interrupted_task(
        db: &DatabasePool,
        stream_publisher: &StreamPubSub,
        task_queue: &ProcessingQueue<TaskRequest>,
        task: &ActiveTask,
    ) {
        let op_timeout = Duration::from_secs(SHUTDOWN_OP_TIMEOUT_SECS);

        // 0. Decide the infra-retry budget up front so the terminal record
        // matches what actually happens next. If the row can't be read we
        // assume budget remains — enqueue_infra_retry re-checks the persisted
        // counter before creating a generation, so the cap still holds.
        let budget_exhausted = match timeout(op_timeout, Task::get(db, &task.task_id)).await {
            Ok(Ok(row)) => row.infra_retry_count >= MAX_INFRA_RETRY_GENERATIONS,
            _ => false,
        };

        let error = if budget_exhausted {
            serde_json::json!({
                "$type": "::hot::task/Failure",
                "$val": {
                    "msg": shutdown_exhausted_reason(),
                    "err": null,
                    "infra_interrupted": false,
                    "infra_retry_exhausted": true,
                }
            })
        } else {
            serde_json::json!({
                "$type": "::hot::task/Failure",
                "$val": {
                    "msg": SHUTDOWN_REASON,
                    "err": null,
                    "infra_interrupted": true,
                }
            })
        };

        // 1. Mark the original task as failed. `None` timeout means the
        // client-side future was dropped — the statement may still commit
        // server-side, so the outcome is classified as unknown, not as a
        // definite failure.
        let write = timeout(
            op_timeout,
            Task::complete(
                db,
                &task.task_id,
                &TaskStatus::Failed,
                Some(&error),
                None,
                None,
            ),
        )
        .await
        .ok();
        let outcome = Self::classify_terminal_write(&task.task_id, write);
        Self::apply_finalize_outcome(
            db,
            stream_publisher,
            task_queue,
            task,
            &error,
            outcome,
            budget_exhausted,
        )
        .await;
    }

    /// Map the shutdown-time `Task::complete` result onto what finalize may
    /// do next. `None` means the call timed out client-side.
    fn classify_terminal_write(
        task_id: &Uuid,
        write: Option<Result<bool, TaskError>>,
    ) -> FinalizeOutcome {
        match write {
            Some(Ok(true)) => FinalizeOutcome::PublishAndRetry,
            Some(Ok(false)) => {
                tracing::info!(
                    task_id = %task_id,
                    "Shutdown: task was already terminal; skipping stale completion event and retry"
                );
                FinalizeOutcome::Skip
            }
            Some(Err(e)) => {
                tracing::error!(
                    task_id = %task_id,
                    "Shutdown: Task::complete failed; suppressing completion event but still enqueueing infra retry: {}", e
                );
                FinalizeOutcome::RetryOnly
            }
            None => {
                tracing::error!(
                    task_id = %task_id,
                    "Shutdown: Task::complete timed out after {}s; suppressing completion event but still enqueueing infra retry",
                    SHUTDOWN_OP_TIMEOUT_SECS,
                );
                FinalizeOutcome::RetryOnly
            }
        }
    }

    async fn apply_finalize_outcome(
        db: &DatabasePool,
        stream_publisher: &StreamPubSub,
        task_queue: &ProcessingQueue<TaskRequest>,
        task: &ActiveTask,
        error: &serde_json::Value,
        outcome: FinalizeOutcome,
        retry_budget_exhausted: bool,
    ) {
        let op_timeout = Duration::from_secs(SHUTDOWN_OP_TIMEOUT_SECS);

        match outcome {
            FinalizeOutcome::Skip => return,
            FinalizeOutcome::RetryOnly => {}
            FinalizeOutcome::PublishAndRetry => {
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
                // Bounded like every other drain step: a black-holed Redis
                // connection here would otherwise stall Phase 3 past the ECS
                // stopTimeout and drop the remaining tasks' infra retries.
                match timeout(op_timeout, stream_publisher.publish_env(event)).await {
                    Ok(Ok(_)) => {}
                    Ok(Err(e)) => {
                        tracing::warn!(
                            task_id = %task.task_id,
                            "Shutdown: failed to publish task:complete: {}", e
                        );
                    }
                    Err(_) => {
                        tracing::error!(
                            task_id = %task.task_id,
                            "Shutdown: publishing task:complete timed out after {}s; continuing to the retry enqueue",
                            SHUTDOWN_OP_TIMEOUT_SECS,
                        );
                    }
                }
            }
        }

        // 3. Enqueue an infra-retry. We re-enqueue regardless of the user's
        // `retry` meta — this isn't a user-error retry, it's a system-
        // initiated re-run. A persisted lineage counter caps repeated
        // shutdown-driven generations; a fresh queue entry starts with a fresh
        // delivery count and therefore cannot be bounded by Redis alone.
        // Safe even when an unknown terminal write later turns out to have
        // committed: the retry is a fresh task row, and the original queue
        // message resolves against the (then terminal) original row.
        //
        // A cap-blocked lineage skips the enqueue entirely — its terminal
        // record (written above) already says the budget is exhausted.
        if retry_budget_exhausted {
            tracing::error!(
                task_id = %task.task_id,
                max_infra_retries = MAX_INFRA_RETRY_GENERATIONS,
                "Shutdown: infra-retry lineage limit reached — task failed permanently, not re-enqueued"
            );
            return;
        }
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

        if task_row.infra_retry_count >= MAX_INFRA_RETRY_GENERATIONS {
            tracing::error!(
                task_id = %task.task_id,
                infra_retry_count = task_row.infra_retry_count,
                max_infra_retries = MAX_INFRA_RETRY_GENERATIONS,
                "Shutdown: infra-retry lineage limit reached — not creating another generation"
            );
            return;
        }

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
            Task::insert_infra_retry(db, &new_task_id, &task_row, next_retry_at),
        )
        .await
        {
            Ok(Ok(true)) => {}
            Ok(Ok(false)) => {
                // The (parent, attempt) unique key says an infra retry of
                // this task already exists — e.g. a prior drain of the same
                // row got as far as insert_retry before a brownout. Skip the
                // enqueue: the row's creator owns delivery, and if it crashed
                // before enqueueing, reconcile_queued_tasks re-enqueues the
                // stale queued row, so the retry cannot be stranded.
                tracing::info!(
                    task_id = %task.task_id,
                    attempt = next_attempt,
                    "Shutdown: infra-retry row already exists for this attempt — skipping duplicate enqueue"
                );
                return;
            }
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
                    infra_retry_count = task_row.infra_retry_count + 1,
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

    #[test]
    fn terminal_write_classification_keeps_retry_when_write_state_is_unknown() {
        let task_id = Uuid::now_v7();

        assert_eq!(
            TaskShutdownCoordinator::classify_terminal_write(&task_id, Some(Ok(true))),
            FinalizeOutcome::PublishAndRetry
        );
        assert_eq!(
            TaskShutdownCoordinator::classify_terminal_write(&task_id, Some(Ok(false))),
            FinalizeOutcome::Skip,
            "an already-terminal task must skip both the stale event and the retry"
        );
        assert_eq!(
            TaskShutdownCoordinator::classify_terminal_write(
                &task_id,
                Some(Err(TaskError::NotFound))
            ),
            FinalizeOutcome::RetryOnly,
            "a failed terminal write must still yield the guaranteed infra retry"
        );
        assert_eq!(
            TaskShutdownCoordinator::classify_terminal_write(&task_id, None),
            FinalizeOutcome::RetryOnly,
            "a timed-out terminal write must still yield the guaranteed infra retry"
        );
    }

    #[tokio::test]
    async fn unknown_terminal_write_enqueues_infra_retry_without_completion_event() {
        let db = hot::db::test_db().await;
        let task_queue = memory_task_queue();
        let stream_publisher = memory_stream_pubsub();
        let active = dummy_active_task(Uuid::now_v7());
        insert_running_task(&db, &active).await;
        let mut env_events = stream_publisher.subscribe_env(active.env_id).await.unwrap();

        TaskShutdownCoordinator::apply_finalize_outcome(
            &db,
            &stream_publisher,
            &task_queue,
            &active,
            &serde_json::json!({"msg": SHUTDOWN_REASON}),
            FinalizeOutcome::RetryOnly,
            false,
        )
        .await;

        let retry = claim_enqueued_request(&task_queue).await;
        assert_ne!(retry.task_id, active.task_id.to_string());
        assert_eq!(retry.function_name, active.function_name);
        let retry_task_id = Uuid::parse_str(&retry.task_id).unwrap();
        let retry_row = Task::get(&db, &retry_task_id).await.unwrap();
        assert_eq!(retry_row.retry_attempt, 0, "infra retry is budget-exempt");

        assert!(
            tokio::time::timeout(Duration::from_millis(100), env_events.next())
                .await
                .is_err(),
            "no task:complete may be published while the terminal state is not durably known"
        );
    }

    #[tokio::test]
    async fn already_terminal_task_publishes_nothing_and_enqueues_nothing() {
        let db = hot::db::test_db().await;
        let task_queue = memory_task_queue();
        let stream_publisher = memory_stream_pubsub();
        let active = dummy_active_task(Uuid::now_v7());
        insert_running_task(&db, &active).await;
        let mut env_events = stream_publisher.subscribe_env(active.env_id).await.unwrap();

        TaskShutdownCoordinator::apply_finalize_outcome(
            &db,
            &stream_publisher,
            &task_queue,
            &active,
            &serde_json::json!({"msg": SHUTDOWN_REASON}),
            FinalizeOutcome::Skip,
            false,
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

        TaskShutdownCoordinator::apply_finalize_outcome(
            &db,
            &stream_publisher,
            &task_queue,
            &active,
            &serde_json::json!({"msg": SHUTDOWN_REASON}),
            FinalizeOutcome::RetryOnly,
            false,
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
    async fn infra_retry_lineage_stops_at_persisted_generation_limit() {
        let db = hot::db::test_db().await;
        let task_queue = memory_task_queue();
        let root_active = dummy_active_task(Uuid::now_v7());
        insert_running_task(&db, &root_active).await;

        let mut row = Task::get(&db, &root_active.task_id).await.unwrap();
        for expected_count in 1..=MAX_INFRA_RETRY_GENERATIONS {
            let child_id = Uuid::now_v7();
            assert!(
                Task::insert_infra_retry(&db, &child_id, &row, chrono::Utc::now())
                    .await
                    .unwrap()
            );
            row = Task::get(&db, &child_id).await.unwrap();
            assert_eq!(row.infra_retry_count, expected_count);
        }

        let mut capped_active = dummy_active_task(row.task_id);
        capped_active.env_id = row.env_id;
        capped_active.stream_id = row.stream_id;
        capped_active.original_request.env_id = row.env_id.to_string();
        capped_active.original_request.stream_id = row.stream_id.to_string();
        capped_active.original_request.build_id = row.build_id.to_string();

        TaskShutdownCoordinator::enqueue_infra_retry(&db, &task_queue, &capped_active).await;

        assert!(
            tokio::time::timeout(Duration::from_millis(100), task_queue.claim_blocking())
                .await
                .is_err(),
            "a task at the persisted infra retry limit must not create a fresh queue entry"
        );
        assert_eq!(
            Task::get_count_by_env(&db, &root_active.env_id)
                .await
                .unwrap(),
            i64::from(MAX_INFRA_RETRY_GENERATIONS) + 1,
            "no generation beyond the cap may be inserted"
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

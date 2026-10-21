/// Database Writer - Dedicated sequential writer for reliable, ordered database operations
///
/// This module provides a production-grade database writer that:
/// - Never blocks the event emitter
/// - Guarantees ordering of all writes
/// - Handles both SQLite and PostgreSQL optimally
/// - Provides backpressure visibility
/// - Ensures graceful shutdown with full data persistence
use crate::db::{DatabasePool, DatabaseScope, current_database_scope};
use crate::lang::emitter::postgres_safety::{sanitize_json_for_jsonb, sanitize_text_for_postgres};
use crate::lang::event::ExecutionContext;
use crate::val::Val;
use ahash::{AHashMap, AHashSet};
use sqlx::Sqlite;
use std::sync::{
    Arc, Mutex as StdMutex,
    atomic::{AtomicU64, Ordering},
};
use std::time::Instant;
use tokio::sync::{RwLock, mpsc, oneshot};
use uuid::Uuid;

/// Serialize a `Val` to JSON for storage in a Postgres `jsonb` column,
/// scrubbing characters Postgres rejects (`\u0000`, lone UTF-16
/// surrogates) but JSON otherwise allows. Returns `"{}"` if
/// serialization fails. See [`crate::lang::emitter::postgres_safety`]
/// for the full forbidden-character rationale.
fn val_to_jsonb_string(v: &Val) -> String {
    let storage_val = v.to_hot_data_repr();
    let raw = serde_json::to_string(&storage_val).unwrap_or_else(|_| "{}".to_string());
    sanitize_json_for_jsonb(&raw).into_owned()
}

fn duration_ms_between(
    start: chrono::DateTime<chrono::Utc>,
    stop: chrono::DateTime<chrono::Utc>,
) -> i64 {
    stop.signed_duration_since(start).num_milliseconds().max(0)
}

/// Maximum number of pending writes before backpressure warning
const BACKPRESSURE_THRESHOLD: usize = 1000;

/// Batch size before the writer shard flushes pending call writes (SQLite and Postgres).
const WRITE_BATCH_SIZE: usize = 100;

/// Run info tuple: (run_type_id, build_id, event_id, env_id, retry_attempt, status_id)
type RunInfo = (i16, Option<Uuid>, Option<Uuid>, Uuid, i16, i16);

fn elapsed_ms(elapsed: std::time::Duration) -> u64 {
    elapsed.as_millis().min(u128::from(u64::MAX)) as u64
}

fn atomic_max(cell: &AtomicU64, value: u64) {
    let mut current = cell.load(Ordering::Relaxed);
    while value > current {
        match cell.compare_exchange_weak(current, value, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => break,
            Err(next) => current = next,
        }
    }
}

struct DatabaseWriterPressureStats {
    enqueued: AtomicU64,
    enqueued_run: AtomicU64,
    enqueued_call: AtomicU64,
    batches: AtomicU64,
    input_writes: AtomicU64,
    run_writes: AtomicU64,
    other_writes: AtomicU64,
    other_write_errors: AtomicU64,
    batch_ms: AtomicU64,
    max_batch_ms: AtomicU64,
    run_write_ms: AtomicU64,
    max_run_write_ms: AtomicU64,
    other_write_ms: AtomicU64,
    max_other_write_ms: AtomicU64,
    max_local_pending: AtomicU64,
    last_log_at: StdMutex<Instant>,
}

impl DatabaseWriterPressureStats {
    fn new() -> Self {
        Self {
            enqueued: AtomicU64::new(0),
            enqueued_run: AtomicU64::new(0),
            enqueued_call: AtomicU64::new(0),
            batches: AtomicU64::new(0),
            input_writes: AtomicU64::new(0),
            run_writes: AtomicU64::new(0),
            other_writes: AtomicU64::new(0),
            other_write_errors: AtomicU64::new(0),
            batch_ms: AtomicU64::new(0),
            max_batch_ms: AtomicU64::new(0),
            run_write_ms: AtomicU64::new(0),
            max_run_write_ms: AtomicU64::new(0),
            other_write_ms: AtomicU64::new(0),
            max_other_write_ms: AtomicU64::new(0),
            max_local_pending: AtomicU64::new(0),
            last_log_at: StdMutex::new(Instant::now()),
        }
    }

    fn observe_enqueued(&self, write: &DatabaseWrite) {
        self.enqueued.fetch_add(1, Ordering::Relaxed);
        if matches!(
            write,
            DatabaseWrite::RunStart { .. }
                | DatabaseWrite::RunStop { .. }
                | DatabaseWrite::RunFail { .. }
                | DatabaseWrite::RunCancel { .. }
        ) {
            self.enqueued_run.fetch_add(1, Ordering::Relaxed);
        } else {
            self.enqueued_call.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn observe_local_pending(&self, pending: usize) {
        atomic_max(&self.max_local_pending, pending as u64);
    }

    fn observe_batch(
        &self,
        input_writes: usize,
        run_writes: usize,
        other_writes: usize,
        other_write_errors: usize,
        elapsed_ms: u64,
    ) {
        self.batches.fetch_add(1, Ordering::Relaxed);
        self.input_writes
            .fetch_add(input_writes as u64, Ordering::Relaxed);
        self.run_writes
            .fetch_add(run_writes as u64, Ordering::Relaxed);
        self.other_writes
            .fetch_add(other_writes as u64, Ordering::Relaxed);
        self.other_write_errors
            .fetch_add(other_write_errors as u64, Ordering::Relaxed);
        self.batch_ms.fetch_add(elapsed_ms, Ordering::Relaxed);
        atomic_max(&self.max_batch_ms, elapsed_ms);
    }

    fn observe_run_write_ms(&self, elapsed_ms: u64) {
        self.run_write_ms.fetch_add(elapsed_ms, Ordering::Relaxed);
        atomic_max(&self.max_run_write_ms, elapsed_ms);
    }

    fn observe_other_write_ms(&self, elapsed_ms: u64) {
        self.other_write_ms.fetch_add(elapsed_ms, Ordering::Relaxed);
        atomic_max(&self.max_other_write_ms, elapsed_ms);
    }

    fn maybe_log(&self) {
        let Ok(mut last_log_at) = self.last_log_at.try_lock() else {
            return;
        };
        if last_log_at.elapsed() < std::time::Duration::from_secs(5) {
            return;
        }
        *last_log_at = Instant::now();

        let batches = self.batches.load(Ordering::Relaxed);
        let input_writes = self.input_writes.load(Ordering::Relaxed);
        if batches == 0 && input_writes == 0 {
            return;
        }

        let batch_ms = self.batch_ms.load(Ordering::Relaxed);
        let run_writes = self.run_writes.load(Ordering::Relaxed);
        let run_write_ms = self.run_write_ms.load(Ordering::Relaxed);
        let other_writes = self.other_writes.load(Ordering::Relaxed);
        let other_write_ms = self.other_write_ms.load(Ordering::Relaxed);

        tracing::info!(
            target: "hot::db_writer::pressure",
            enqueued = self.enqueued.load(Ordering::Relaxed),
            enqueued_run = self.enqueued_run.load(Ordering::Relaxed),
            enqueued_call = self.enqueued_call.load(Ordering::Relaxed),
            batches,
            input_writes,
            run_writes,
            other_writes,
            other_write_errors = self.other_write_errors.load(Ordering::Relaxed),
            avg_batch_ms = batch_ms.checked_div(batches).unwrap_or(0),
            max_batch_ms = self.max_batch_ms.load(Ordering::Relaxed),
            avg_run_write_ms = run_write_ms.checked_div(run_writes).unwrap_or(0),
            max_run_write_ms = self.max_run_write_ms.load(Ordering::Relaxed),
            avg_other_write_ms = other_write_ms.checked_div(other_writes).unwrap_or(0),
            max_other_write_ms = self.max_other_write_ms.load(Ordering::Relaxed),
            max_local_pending = self.max_local_pending.load(Ordering::Relaxed),
            "DatabaseWriter pressure"
        );
    }
}

#[derive(Clone, Copy, Debug)]
enum DatabaseWriterFlushTrigger {
    AutoRunWrite,
    BatchSize,
    ChannelEmpty,
    ExplicitFlush,
    ChannelClosed,
    Shutdown,
}

impl DatabaseWriterFlushTrigger {
    fn as_str(self) -> &'static str {
        match self {
            DatabaseWriterFlushTrigger::AutoRunWrite => "auto_run_write",
            DatabaseWriterFlushTrigger::BatchSize => "batch_size",
            DatabaseWriterFlushTrigger::ChannelEmpty => "channel_empty",
            DatabaseWriterFlushTrigger::ExplicitFlush => "explicit_flush",
            DatabaseWriterFlushTrigger::ChannelClosed => "channel_closed",
            DatabaseWriterFlushTrigger::Shutdown => "shutdown",
        }
    }
}

/// All possible database write operations
#[derive(Debug, Clone)]
pub enum DatabaseWrite {
    /// Insert a new run record
    RunStart {
        execution_context: ExecutionContext,
        event_time: chrono::DateTime<chrono::Utc>,
    },
    /// Update run with stop time, success status, and result
    RunStop {
        run_id: Uuid,
        event_time: chrono::DateTime<chrono::Utc>,
        result: Val,
    },
    /// Update run with stop time, failed status, and failure data
    RunFail {
        run_id: Uuid,
        event_time: chrono::DateTime<chrono::Utc>,
        failure: Val,
    },
    /// Update run with stop time, cancelled status, and cancellation data
    RunCancel {
        run_id: Uuid,
        event_time: chrono::DateTime<chrono::Utc>,
        cancellation: Val,
    },
    /// Insert or update a call record
    Call {
        execution_context: Box<ExecutionContext>,
        call_id: Uuid,
        parent_call_id: Option<Uuid>,
        function_name: String,
        static_scope: String,
        runtime_path: String,
        call_depth: i64,
        args: Option<String>,
        return_value: Option<String>,
        error: Option<String>,
        flow: Option<String>,
        file: Option<String>,
        line: Option<i64>,
        column: Option<i64>,
        position: Option<i64>,
        start_time: Option<chrono::DateTime<chrono::Utc>>,
        stop_time: Option<chrono::DateTime<chrono::Utc>>,
        duration_us: Option<i64>,
    },
}

impl DatabaseWrite {
    fn run_id(&self) -> Uuid {
        match self {
            DatabaseWrite::RunStart {
                execution_context, ..
            } => execution_context.run_id,
            DatabaseWrite::Call {
                execution_context, ..
            } => execution_context.run_id,
            DatabaseWrite::RunStop { run_id, .. }
            | DatabaseWrite::RunFail { run_id, .. }
            | DatabaseWrite::RunCancel { run_id, .. } => *run_id,
        }
    }
}

struct ScopedWrite {
    write: DatabaseWrite,
    scope: Option<DatabaseScope>,
}

#[derive(Clone)]
struct DatabaseWriterShard {
    write_sender: mpsc::UnboundedSender<ScopedWrite>,
    flush_sender: mpsc::UnboundedSender<oneshot::Sender<()>>,
    shutdown_sender: mpsc::UnboundedSender<oneshot::Sender<()>>,
}

/// How long a finished run keeps its scope for trailing writes. The emitter
/// batches calls, so they routinely arrive after the run's terminal write.
const FINISHED_RUN_SCOPE_TTL: std::time::Duration = std::time::Duration::from_secs(300);
const RUN_SCOPE_PRUNE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);

enum RunScope {
    Active(DatabaseScope),
    Finished(DatabaseScope, Instant),
}

impl RunScope {
    fn scope(&self) -> &DatabaseScope {
        match self {
            Self::Active(scope) | Self::Finished(scope, _) => scope,
        }
    }
}

/// Execution scope of each run seen by this writer. Terminal writes carry no
/// execution context, so they rely on it.
#[derive(Default)]
struct RunScopes {
    runs: AHashMap<Uuid, RunScope>,
    pruned_at: Option<Instant>,
}

impl RunScopes {
    /// Drop finished runs once trailing writes can no longer need them.
    fn prune(&mut self, now: Instant) {
        if self
            .pruned_at
            .is_some_and(|at| now.duration_since(at) < RUN_SCOPE_PRUNE_INTERVAL)
        {
            return;
        }
        self.runs.retain(|_, run| match run {
            RunScope::Active(_) => true,
            RunScope::Finished(_, at) => now.duration_since(*at) < FINISHED_RUN_SCOPE_TTL,
        });
        self.pruned_at = Some(now);
    }
}

/// Handle to the database writer - allows sending writes without blocking
#[derive(Clone)]
pub struct DatabaseWriter {
    shards: Arc<Vec<DatabaseWriterShard>>,
    run_scopes: Arc<StdMutex<RunScopes>>,
    failed_runs: Arc<StdMutex<AHashSet<Uuid>>>,
    pressure_stats: Option<Arc<DatabaseWriterPressureStats>>,
}

impl DatabaseWriter {
    /// Create a new database writer with the given database pool
    pub fn new(db: Arc<RwLock<Option<DatabasePool>>>) -> Self {
        let shard_count = Self::shard_count_for_db(&db);
        let database_required = db.try_read().map(|state| state.is_some()).unwrap_or(true);
        let mut shards = Vec::with_capacity(shard_count);
        let pressure_stats = Self::pressure_stats_from_env();
        let failed_runs = Arc::new(StdMutex::new(AHashSet::new()));

        for shard_idx in 0..shard_count {
            let (write_sender, mut write_receiver) = mpsc::unbounded_channel::<ScopedWrite>();
            let (flush_sender, mut flush_receiver) =
                mpsc::unbounded_channel::<oneshot::Sender<()>>();
            let (shutdown_sender, mut shutdown_receiver) =
                mpsc::unbounded_channel::<oneshot::Sender<()>>();
            let db = Arc::clone(&db);
            let pressure_stats_for_task = pressure_stats.clone();
            let failed_runs = failed_runs.clone();

            // Spawn the dedicated writer shard task.
            tokio::spawn(async move {
                tracing::debug!(
                    shard_idx,
                    shard_count,
                    "DatabaseWriter shard started, entering event loop"
                );
                let mut pending_writes = Vec::new();
                let mut last_backpressure_warning = std::time::Instant::now();

                loop {
                    tokio::select! {
                        // Receive write operations
                        write = write_receiver.recv() => {
                            match write {
                                Some(w) => {
                                    pending_writes.push(w);
                                    if let Some(stats) = pressure_stats_for_task.as_deref() {
                                        stats.observe_local_pending(pending_writes.len());
                                    }

                                    // Check for backpressure
                                    if pending_writes.len() > BACKPRESSURE_THRESHOLD
                                        && last_backpressure_warning.elapsed() > std::time::Duration::from_secs(5)
                                    {
                                        tracing::warn!(
                                            shard_idx,
                                            pending = pending_writes.len(),
                                            "DatabaseWriter shard high backpressure detected"
                                        );
                                        last_backpressure_warning = std::time::Instant::now();
                                    }

                                    // Run writes stay ordered with their run's call writes by
                                    // shard affinity. They still flush promptly, but only this
                                    // shard is blocked; sibling runs drain on sibling shards.
                                    let has_run_write = pending_writes.iter().any(|w| {
                                        matches!(w.write, DatabaseWrite::RunStart { .. } | DatabaseWrite::RunStop { .. } | DatabaseWrite::RunFail { .. } | DatabaseWrite::RunCancel { .. })
                                    });

                                    let should_flush = has_run_write
                                        || pending_writes.len() >= WRITE_BATCH_SIZE
                                        || write_receiver.is_empty();

                                    if should_flush {
                                        let trigger = if has_run_write {
                                            DatabaseWriterFlushTrigger::AutoRunWrite
                                        } else if pending_writes.len() >= WRITE_BATCH_SIZE {
                                            DatabaseWriterFlushTrigger::BatchSize
                                        } else {
                                            DatabaseWriterFlushTrigger::ChannelEmpty
                                        };
                                        tracing::debug!(
                                            shard_idx,
                                            pending = pending_writes.len(),
                                            has_run_write,
                                            batch_size = pending_writes.len() >= WRITE_BATCH_SIZE,
                                            channel_empty = write_receiver.is_empty(),
                                            trigger = trigger.as_str(),
                                            "DatabaseWriter shard flushing writes"
                                        );
                                        Self::process_batch(
                                            &db,
                                            &mut pending_writes,
                                            shard_idx,
                                            trigger,
                                            pressure_stats_for_task.as_deref(),
                                        &failed_runs,
                                        database_required,
                                        )
                                        .await;
                                    }
                                }
                                None => {
                                    tracing::debug!(
                                        shard_idx,
                                        pending = pending_writes.len(),
                                        "DatabaseWriter shard write channel closed"
                                    );
                                    if !pending_writes.is_empty() {
                                        Self::process_batch(
                                            &db,
                                            &mut pending_writes,
                                            shard_idx,
                                            DatabaseWriterFlushTrigger::ChannelClosed,
                                            pressure_stats_for_task.as_deref(),
                                        &failed_runs,
                                        database_required,
                                        )
                                        .await;
                                    }
                                    if let Some(sender) = shutdown_receiver.recv().await {
                                        let _ = sender.send(());
                                    }
                                    break;
                                }
                            }
                        }
                        // Handle flush requests - flush all pending writes and acknowledge
                        flush_ack = flush_receiver.recv() => {
                            if let Some(ack_sender) = flush_ack {
                                while let Ok(write) = write_receiver.try_recv() {
                                    pending_writes.push(write);
                                }

                                if !pending_writes.is_empty() {
                                    tracing::debug!(
                                        shard_idx,
                                        pending = pending_writes.len(),
                                        "DatabaseWriter shard flushing pending writes on explicit request"
                                    );
                                    Self::process_batch(
                                        &db,
                                        &mut pending_writes,
                                        shard_idx,
                                        DatabaseWriterFlushTrigger::ExplicitFlush,
                                        pressure_stats_for_task.as_deref(),
                                        &failed_runs,
                                        database_required,
                                    )
                                    .await;
                                }

                                let _ = ack_sender.send(());
                            }
                        }
                        // Handle shutdown
                        completion_sender = shutdown_receiver.recv() => {
                            if let Some(sender) = completion_sender {
                                while let Ok(write) = write_receiver.try_recv() {
                                    pending_writes.push(write);
                                }

                                if !pending_writes.is_empty() {
                                    tracing::debug!(
                                        shard_idx,
                                        pending = pending_writes.len(),
                                        "DatabaseWriter shard flushing pending writes before shutdown"
                                    );
                                    Self::process_batch(
                                        &db,
                                        &mut pending_writes,
                                        shard_idx,
                                        DatabaseWriterFlushTrigger::Shutdown,
                                        pressure_stats_for_task.as_deref(),
                                        &failed_runs,
                                        database_required,
                                    )
                                    .await;
                                }

                                let _ = sender.send(());
                                break;
                            }
                        }
                    }
                }

                tracing::debug!(shard_idx, "DatabaseWriter shard shutdown complete");
            });

            shards.push(DatabaseWriterShard {
                write_sender,
                flush_sender,
                shutdown_sender,
            });
        }

        Self {
            shards: Arc::new(shards),
            run_scopes: Arc::new(StdMutex::new(RunScopes::default())),
            failed_runs,
            pressure_stats,
        }
    }

    /// Send a write operation to the database writer (never blocks)
    pub fn write(&self, write: DatabaseWrite) {
        if let Some(stats) = &self.pressure_stats {
            stats.observe_enqueued(&write);
        }
        let run_id = write.run_id();
        let context = match &write {
            DatabaseWrite::RunStart {
                execution_context, ..
            } => Some(execution_context),
            DatabaseWrite::Call {
                execution_context, ..
            } => Some(execution_context.as_ref()),
            _ => None,
        };
        let provided_scope = context
            .and_then(|context| {
                Some(DatabaseScope::Environment {
                    org_id: context.org_id?,
                    env_id: context.env_id?,
                })
            })
            .or_else(|| {
                current_database_scope()
                    .filter(|scope| matches!(scope, DatabaseScope::Environment { .. }))
            });
        let scope = {
            let mut scopes = self
                .run_scopes
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if let Some(scope) = &provided_scope {
                if scopes
                    .runs
                    .get(&run_id)
                    .is_some_and(|existing| existing.scope() != scope)
                {
                    self.failed_runs
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .insert(run_id);
                    tracing::error!(%run_id, "DatabaseWriter rejected a changed execution scope");
                    return;
                }
                // Writes trailing a terminal write must not re-register a
                // finished run, or every run would leave a permanent entry.
                scopes
                    .runs
                    .entry(run_id)
                    .or_insert_with(|| RunScope::Active(scope.clone()));
            }
            let scope =
                provided_scope.or_else(|| scopes.runs.get(&run_id).map(|run| run.scope().clone()));
            if matches!(
                write,
                DatabaseWrite::RunStop { .. }
                    | DatabaseWrite::RunFail { .. }
                    | DatabaseWrite::RunCancel { .. }
            ) {
                let now = Instant::now();
                if let Some(scope) = &scope {
                    scopes
                        .runs
                        .insert(run_id, RunScope::Finished(scope.clone(), now));
                }
                scopes.prune(now);
            }
            scope
        };
        let shard = self.shard_for_run(run_id);
        if shard
            .write_sender
            .send(ScopedWrite { write, scope })
            .is_err()
        {
            tracing::error!("DatabaseWriter: Failed to send write - writer task has shut down");
        }
    }

    /// Flush all pending writes and wait for completion
    /// This ensures all writes (including run:start) are committed to the database
    /// before returning. Use this before publishing events that reference the current run.
    pub fn flush(&self) -> Result<(), String> {
        // VM code runs in a blocking context, where `Handle::block_on` is the
        // correct sync-to-async bridge. Async worker code should call
        // `flush_async()` instead.
        tokio::runtime::Handle::current().block_on(self.flush_async())
    }

    pub async fn flush_async(&self) -> Result<(), String> {
        let mut receivers = Vec::with_capacity(self.shards.len());
        for shard in self.shards.iter() {
            let (ack_sender, ack_receiver) = oneshot::channel();

            if shard.flush_sender.send(ack_sender).is_err() {
                return Err("Failed to send flush request - writer task has shut down".to_string());
            }
            receivers.push(ack_receiver);
        }

        for ack_receiver in receivers {
            ack_receiver
                .await
                .map_err(|_| "Flush acknowledgment was dropped".to_string())?;
        }
        self.check_failures(None)
    }

    pub async fn flush_run(&self, run_id: Uuid) -> Result<(), String> {
        let shard = self.shard_for_run(run_id);
        let (ack_sender, ack_receiver) = oneshot::channel();

        if shard.flush_sender.send(ack_sender).is_err() {
            return Err("Failed to send flush request - writer task has shut down".to_string());
        }

        ack_receiver
            .await
            .map_err(|_| "Flush acknowledgment was dropped".to_string())?;
        self.check_failures(Some(run_id))
    }

    /// Gracefully shutdown the writer, ensuring all pending writes are flushed
    pub async fn shutdown(&self) -> Result<(), String> {
        let mut receivers = Vec::with_capacity(self.shards.len());
        for shard in self.shards.iter() {
            let (completion_sender, completion_receiver) = oneshot::channel();

            if shard.shutdown_sender.send(completion_sender).is_err() {
                return Err("Failed to send shutdown signal - writer already shut down".to_string());
            }
            receivers.push(completion_receiver);
        }

        for completion_receiver in receivers {
            completion_receiver
                .await
                .map_err(|_| "Shutdown completion signal was dropped".to_string())?;
        }
        self.check_failures(None)
    }

    fn check_failures(&self, run_id: Option<Uuid>) -> Result<(), String> {
        let failed = self
            .failed_runs
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let has_failure = match run_id {
            Some(id) => failed.contains(&id),
            None => !failed.is_empty(),
        };
        if has_failure {
            Err("Database writer could not persist the execution; retry is required".into())
        } else {
            Ok(())
        }
    }

    fn shard_count_for_db(db: &Arc<RwLock<Option<DatabasePool>>>) -> usize {
        match db.try_read().ok().and_then(|guard| guard.as_ref().cloned()) {
            Some(DatabasePool::Postgres(_)) => std::env::var("HOT_DB_WRITER_SHARDS")
                .ok()
                .and_then(|value| value.parse::<usize>().ok())
                .unwrap_or(4)
                .max(1),
            _ => 1,
        }
    }

    fn pressure_stats_from_env() -> Option<Arc<DatabaseWriterPressureStats>> {
        let enabled = std::env::var("HOT_DB_WRITER_METRICS_ENABLED")
            .ok()
            .map(|value| matches!(value.as_str(), "1" | "true" | "TRUE" | "yes" | "YES"))
            .unwrap_or(false);
        enabled.then(|| Arc::new(DatabaseWriterPressureStats::new()))
    }

    fn shard_for_run(&self, run_id: Uuid) -> &DatabaseWriterShard {
        let bytes = run_id.as_bytes();
        let hash = u64::from_be_bytes([
            bytes[0] ^ bytes[8],
            bytes[1] ^ bytes[9],
            bytes[2] ^ bytes[10],
            bytes[3] ^ bytes[11],
            bytes[4] ^ bytes[12],
            bytes[5] ^ bytes[13],
            bytes[6] ^ bytes[14],
            bytes[7] ^ bytes[15],
        ]);
        let idx = (hash as usize) % self.shards.len().max(1);
        &self.shards[idx]
    }

    /// Process a batch of writes optimally based on database type
    /// Critical: Separates run writes from other writes to maintain foreign key ordering
    async fn process_batch(
        db: &Arc<RwLock<Option<DatabasePool>>>,
        writes: &mut Vec<ScopedWrite>,
        shard_idx: usize,
        trigger: DatabaseWriterFlushTrigger,
        pressure_stats: Option<&DatabaseWriterPressureStats>,
        failed_runs: &StdMutex<AHashSet<Uuid>>,
        database_required: bool,
    ) {
        // A shard is a scheduling optimization, not an authorization boundary.
        // Preserve per-run ordering but never mix owners in a transaction.
        let mut groups: Vec<(Option<DatabaseScope>, Vec<DatabaseWrite>)> = Vec::new();
        for entry in std::mem::take(writes) {
            if let Some((_, batch)) = groups.iter_mut().find(|(scope, _)| *scope == entry.scope) {
                batch.push(entry.write);
            } else {
                groups.push((entry.scope, vec![entry.write]));
            }
        }
        let pool = db.read().await.clone();
        if database_required && pool.is_none() {
            failed_runs
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .extend(
                    groups
                        .iter()
                        .flat_map(|(_, batch)| batch.iter().map(DatabaseWrite::run_id)),
                );
            tracing::error!("Configured database disappeared before emitter flush");
            return;
        }
        for (scope, mut batch) in groups {
            let scoped_pool = match (&pool, scope) {
                (Some(pool), Some(scope)) => Some(pool.with_scope(scope)),
                // Local CLI evaluations may have no persisted organization. They
                // cannot acquire PostgreSQL authority from a writer task.
                (Some(DatabasePool::Postgres(_)), None) => {
                    failed_runs
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .extend(batch.iter().map(DatabaseWrite::run_id));
                    tracing::error!(
                        writes = batch.len(),
                        "DatabaseWriter rejected missing execution scope"
                    );
                    continue;
                }
                (pool, None) => pool.clone(),
                (None, Some(_)) => None,
            };
            let scoped_db = Arc::new(RwLock::new(scoped_pool));
            let run_ids: Vec<_> = batch.iter().map(DatabaseWrite::run_id).collect();
            if !Self::process_scoped_batch(
                &scoped_db,
                &mut batch,
                shard_idx,
                trigger,
                pressure_stats,
            )
            .await
            {
                failed_runs
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .extend(run_ids);
            }
        }
    }

    async fn process_scoped_batch(
        db: &Arc<RwLock<Option<DatabasePool>>>,
        writes: &mut Vec<DatabaseWrite>,
        shard_idx: usize,
        trigger: DatabaseWriterFlushTrigger,
        pressure_stats: Option<&DatabaseWriterPressureStats>,
    ) -> bool {
        let mut batch_failed = false;
        if writes.is_empty() {
            return !batch_failed;
        }

        let batch_started = std::time::Instant::now();
        let input_writes = writes.len();
        let db_guard = db.read().await;
        let pool = match db_guard.as_ref() {
            Some(p) => p,
            None => {
                tracing::warn!(
                    "DatabaseWriter: No database connection, dropping {} writes",
                    writes.len()
                );
                writes.clear();
                tracing::debug!(
                    target: "hot::db_writer::timing",
                    shard_idx,
                    trigger = trigger.as_str(),
                    backend = "none",
                    input_writes,
                    run_writes = 0usize,
                    other_writes = 0usize,
                    elapsed_ms = batch_started.elapsed().as_millis() as u64,
                    outcome = "no_db",
                    "TIMING: process_batch"
                );
                if let Some(stats) = pressure_stats {
                    stats.observe_batch(input_writes, 0, 0, 0, elapsed_ms(batch_started.elapsed()));
                    stats.maybe_log();
                }
                return !batch_failed;
            }
        };
        let backend = match pool {
            DatabasePool::Sqlite(_) => "sqlite",
            DatabasePool::Postgres(_) => "postgres",
        };

        // Separate critical run writes from other writes to maintain ordering
        // run:start MUST complete before var/flow/call writes that reference it
        let batch = std::mem::take(writes);
        let (run_writes, other_writes): (Vec<_>, Vec<_>) = batch.into_iter().partition(|w| {
            matches!(
                w,
                DatabaseWrite::RunStart { .. }
                    | DatabaseWrite::RunStop { .. }
                    | DatabaseWrite::RunFail { .. }
                    | DatabaseWrite::RunCancel { .. }
            )
        });
        let run_write_count = run_writes.len();
        let other_write_count = other_writes.len();

        // Process run writes first. If a short successful run has both
        // run:start and run:stop pending in this shard flush, insert it once
        // already terminal instead of doing insert+update.
        let run_write_started = Instant::now();
        let mut pending_starts: AHashMap<Uuid, (ExecutionContext, chrono::DateTime<chrono::Utc>)> =
            AHashMap::new();
        for write in run_writes {
            match write {
                DatabaseWrite::RunStart {
                    execution_context,
                    event_time,
                } => {
                    pending_starts
                        .insert(execution_context.run_id, (execution_context, event_time));
                }
                DatabaseWrite::RunStop {
                    run_id,
                    event_time,
                    result,
                } => {
                    if let Some((execution_context, start_time)) = pending_starts.remove(&run_id) {
                        if let Err(e) = Self::write_run_start_stop(
                            pool,
                            &execution_context,
                            start_time,
                            event_time,
                            &result,
                        )
                        .await
                        {
                            batch_failed = true;
                            tracing::error!(
                                "DatabaseWriter: Coalesced run write failed for {}: {}",
                                run_id,
                                e
                            );
                        }
                    } else if let Err(e) =
                        Self::write_run_stop(pool, run_id, event_time, &result).await
                    {
                        batch_failed = true;
                        tracing::error!("DatabaseWriter: Critical run write failed: {}", e);
                    }
                }
                DatabaseWrite::RunFail {
                    run_id,
                    event_time,
                    failure,
                } => {
                    if let Some((execution_context, start_time)) = pending_starts.remove(&run_id)
                        && let Err(e) =
                            Self::write_run_start(pool, &execution_context, start_time).await
                    {
                        batch_failed = true;
                        tracing::error!("DatabaseWriter: Critical run:start write failed: {}", e);
                    }
                    if let Err(e) = Self::write_run_fail(pool, run_id, event_time, &failure).await {
                        batch_failed = true;
                        tracing::error!("DatabaseWriter: Critical run write failed: {}", e);
                    }
                }
                DatabaseWrite::RunCancel {
                    run_id,
                    event_time,
                    cancellation,
                } => {
                    if let Some((execution_context, start_time)) = pending_starts.remove(&run_id)
                        && let Err(e) =
                            Self::write_run_start(pool, &execution_context, start_time).await
                    {
                        batch_failed = true;
                        tracing::error!("DatabaseWriter: Critical run:start write failed: {}", e);
                    }
                    if let Err(e) =
                        Self::write_run_cancel(pool, run_id, event_time, &cancellation).await
                    {
                        batch_failed = true;
                        tracing::error!("DatabaseWriter: Critical run write failed: {}", e);
                    }
                }
                DatabaseWrite::Call { .. } => unreachable!("call writes were partitioned out"),
            }
        }

        for (_, (execution_context, start_time)) in pending_starts {
            if let Err(e) = Self::write_run_start(pool, &execution_context, start_time).await {
                batch_failed = true;
                tracing::error!("DatabaseWriter: Critical run:start write failed: {}", e);
            }
        }
        if let Some(stats) = pressure_stats
            && run_write_count > 0
        {
            stats.observe_run_write_ms(elapsed_ms(run_write_started.elapsed()));
        }

        let mut other_write_errors = 0usize;
        let mut outcome = "run_writes_only";

        // Then process other writes
        if other_writes.is_empty() {
            let batch_elapsed_ms = elapsed_ms(batch_started.elapsed());
            tracing::debug!(
                target: "hot::db_writer::timing",
                shard_idx,
                trigger = trigger.as_str(),
                backend,
                input_writes,
                run_writes = run_write_count,
                other_writes = other_write_count,
                other_write_errors,
                elapsed_ms = batch_elapsed_ms,
                outcome,
                "TIMING: process_batch"
            );
            if let Some(stats) = pressure_stats {
                stats.observe_batch(
                    input_writes,
                    run_write_count,
                    other_write_count,
                    other_write_errors,
                    batch_elapsed_ms,
                );
                stats.maybe_log();
            }
            return !batch_failed;
        }

        let (org_id, env_id) = match Self::trace_batch_scope(pool, &other_writes).await {
            Ok(scope) => scope,
            Err(error) => {
                tracing::error!("DatabaseWriter: Rejected trace batch ownership: {error}");
                return false;
            }
        };
        let other_write_started = Instant::now();
        match pool {
            DatabasePool::Sqlite(_) => {
                match crate::store::quota::begin_sqlite(
                    pool,
                    org_id,
                    env_id,
                    crate::store::quota::QuotaKind::Call,
                )
                .await
                {
                    Ok((mut tx, budget)) => {
                        let mut failed = false;

                        for write in &other_writes {
                            if let Err(e) = Self::execute_write_sqlite(&mut *tx, write).await {
                                batch_failed = true;
                                tracing::error!(
                                    "DatabaseWriter: SQLite write failed in transaction: {}",
                                    e
                                );
                                other_write_errors += 1;
                                failed = true;
                                break;
                            }
                        }

                        if !failed && let Err(error) = budget.check_sqlite(&mut tx).await {
                            tracing::error!(
                                "DatabaseWriter: Trace storage quota rejected SQLite batch: {error}"
                            );
                            batch_failed = true;
                            other_write_errors += 1;
                            failed = true;
                        }
                        if failed {
                            if let Err(e) = tx.rollback().await {
                                batch_failed = true;
                                tracing::error!("DatabaseWriter: SQLite rollback failed: {}", e);
                                outcome = "rollback_failed";
                            } else {
                                outcome = "rolled_back";
                            }
                        } else if let Err(e) = tx.commit().await {
                            batch_failed = true;
                            tracing::error!("DatabaseWriter: SQLite commit failed: {}", e);
                            outcome = "commit_failed";
                        } else {
                            outcome = "committed";
                        }
                    }
                    Err(e) => {
                        batch_failed = true;
                        tracing::error!(
                            "DatabaseWriter: Failed to begin SQLite transaction: {}",
                            e
                        );
                        other_write_errors += other_write_count;
                        outcome = "quota_transaction_failed";
                    }
                }
            }
            DatabasePool::Postgres(_) => {
                match crate::store::quota::begin_postgres(
                    pool,
                    org_id,
                    env_id,
                    crate::store::quota::QuotaKind::Call,
                )
                .await
                {
                    Ok((mut tx, budget)) => {
                        let mut failed = false;

                        // Execute writes IN ORDER within the transaction
                        for write in &other_writes {
                            // Log what we're about to write
                            if let DatabaseWrite::Call { call_id, .. } = write {
                                tracing::debug!("DatabaseWriter: TX inserting call_id={}", call_id);
                            }

                            let result = match write {
                                DatabaseWrite::Call {
                                    execution_context,
                                    call_id,
                                    parent_call_id,
                                    function_name,
                                    static_scope,
                                    runtime_path,
                                    call_depth,
                                    args,
                                    return_value,
                                    error,
                                    flow,
                                    file,
                                    line,
                                    column,
                                    position,
                                    start_time,
                                    stop_time,
                                    duration_us,
                                } => {
                                    // Defense in depth: scrub Postgres-rejected
                                    // chars from `jsonb` columns (`args`,
                                    // `return_value`, `flow`) and the `text`
                                    // column (`error`). Upstream serialization
                                    // already does this, but a single
                                    // unsanitized payload from a future caller
                                    // would 22P05 and roll back the whole
                                    // batch without this net. The `Cow`s
                                    // returned are bound here so they outlive
                                    // the `.bind(... .as_deref())` calls.
                                    // Common (clean) path is one
                                    // `str::contains` early-return, no alloc.
                                    let args_safe = args.as_deref().map(sanitize_json_for_jsonb);
                                    let return_value_safe =
                                        return_value.as_deref().map(sanitize_json_for_jsonb);
                                    let error_safe =
                                        error.as_deref().map(sanitize_text_for_postgres);
                                    let flow_safe = flow.as_deref().map(sanitize_json_for_jsonb);
                                    sqlx::query(
                                        "
INSERT INTO hot.call (call_id, run_id, parent_call_id, function_name, static_scope, runtime_path, call_depth, args, return_value, error, flow, file, line, \"column\", position, start_time, stop_time, duration_us, size, env_id)
VALUES ($1, $2, $3, $4, $5, $6, $7, $8::jsonb, $9::jsonb, $10, $11::jsonb, $12, $13, $14, $15, $16, $17, $18,
    COALESCE(octet_length(($8::jsonb)::text), 0) + COALESCE(octet_length(($9::jsonb)::text), 0) +
    COALESCE(octet_length($10::text), 0) + COALESCE(octet_length(($11::jsonb)::text), 0) + 50, $19)
ON CONFLICT (call_id) DO UPDATE SET
    stop_time = COALESCE(EXCLUDED.stop_time, hot.call.stop_time),
    return_value = COALESCE(EXCLUDED.return_value, hot.call.return_value),
    error = COALESCE(EXCLUDED.error, hot.call.error),
    flow = COALESCE(EXCLUDED.flow, hot.call.flow),
    duration_us = COALESCE(EXCLUDED.duration_us, hot.call.duration_us),
    start_time = COALESCE(EXCLUDED.start_time, hot.call.start_time),
    parent_call_id = COALESCE(EXCLUDED.parent_call_id, hot.call.parent_call_id),
    -- Use NULLIF to treat 'unknown' as NULL, preferring real function names
    function_name = COALESCE(NULLIF(EXCLUDED.function_name, 'unknown'), NULLIF(hot.call.function_name, 'unknown'), 'unknown'),
    static_scope = COALESCE(NULLIF(EXCLUDED.static_scope, 'unknown'), NULLIF(hot.call.static_scope, 'unknown'), 'unknown'),
    runtime_path = COALESCE(NULLIF(EXCLUDED.runtime_path, 'unknown'), NULLIF(hot.call.runtime_path, 'unknown'), 'unknown'),
    call_depth = CASE WHEN EXCLUDED.call_depth = 0 AND hot.call.call_depth != 0 THEN hot.call.call_depth ELSE COALESCE(EXCLUDED.call_depth, hot.call.call_depth) END,
    args = COALESCE(EXCLUDED.args, hot.call.args),
    file = COALESCE(EXCLUDED.file, hot.call.file),
    line = COALESCE(EXCLUDED.line, hot.call.line),
    \"column\" = COALESCE(EXCLUDED.\"column\", hot.call.\"column\"),
    position = COALESCE(EXCLUDED.position, hot.call.position),
    size = COALESCE(octet_length(COALESCE(EXCLUDED.args, hot.call.args)::text), 0) +
           COALESCE(octet_length(COALESCE(EXCLUDED.return_value, hot.call.return_value)::text), 0) +
           COALESCE(octet_length(COALESCE(EXCLUDED.flow, hot.call.flow)::text), 0) +
           COALESCE(octet_length(COALESCE(EXCLUDED.error, hot.call.error)), 0) + 50
WHERE hot.call.run_id = EXCLUDED.run_id
"
                                    )
                                    .bind(call_id)
                                    .bind(execution_context.run_id)
                                    .bind(parent_call_id)
                                    .bind(function_name)
                                    .bind(static_scope)
                                    .bind(runtime_path)
                                    .bind(*call_depth as i32)
                                    .bind(args_safe.as_deref())
                                    .bind(return_value_safe.as_deref())
                                    .bind(error_safe.as_deref())
                                    .bind(flow_safe.as_deref())
                                    .bind(file.as_deref())
                                    .bind(line)
                                    .bind(column)
                                    .bind(position)
                                    .bind(start_time)
                                    .bind(stop_time)
                                    .bind(duration_us)
                                    .bind(execution_context.env_id)
                                    .execute(&mut *tx)
                                    .await
                                    .and_then(|result| {
                                        if result.rows_affected() == 1 { Ok(result) }
                                        else { Err(sqlx::Error::Protocol("Trace call ownership changed".into())) }
                                    })
                                }
                                _ => {
                                    // Run events shouldn't be in other_writes, but handle gracefully
                                    tracing::warn!(
                                        "DatabaseWriter: Unexpected write type in transaction batch: {:?}",
                                        write
                                    );
                                    continue;
                                }
                            };

                            if let Err(e) = result {
                                batch_failed = true;
                                tracing::error!(
                                    "DatabaseWriter: Write failed in transaction: {}",
                                    e
                                );
                                other_write_errors += 1;
                                failed = true;
                                break; // Stop on first error
                            }
                        }

                        if !failed && let Err(error) = budget.check_postgres(&mut tx).await {
                            tracing::error!(
                                "DatabaseWriter: Trace storage quota rejected PostgreSQL batch: {error}"
                            );
                            batch_failed = true;
                            other_write_errors += 1;
                            failed = true;
                        }
                        if failed {
                            // Rollback on failure
                            if let Err(e) = tx.rollback().await {
                                batch_failed = true;
                                tracing::error!("DatabaseWriter: Rollback failed: {}", e);
                                outcome = "rollback_failed";
                            } else {
                                outcome = "rolled_back";
                            }
                        } else {
                            // Commit on success
                            if let Err(e) = tx.commit().await {
                                batch_failed = true;
                                tracing::error!("DatabaseWriter: Commit failed: {}", e);
                                outcome = "commit_failed";
                            } else {
                                outcome = "committed";
                            }
                        }
                    }
                    Err(e) => {
                        batch_failed = true;
                        tracing::error!("DatabaseWriter: Failed to begin transaction: {}", e);
                        other_write_errors += other_write_count;
                        outcome = "quota_transaction_failed";
                    }
                }
            }
        }
        if let Some(stats) = pressure_stats {
            stats.observe_other_write_ms(elapsed_ms(other_write_started.elapsed()));
        }
        let batch_elapsed_ms = elapsed_ms(batch_started.elapsed());
        tracing::debug!(
            target: "hot::db_writer::timing",
            shard_idx,
            trigger = trigger.as_str(),
            backend,
            input_writes,
            run_writes = run_write_count,
            other_writes = other_write_count,
            other_write_errors,
            elapsed_ms = batch_elapsed_ms,
            outcome,
            "TIMING: process_batch"
        );
        if let Some(stats) = pressure_stats {
            stats.observe_batch(
                input_writes,
                run_write_count,
                other_write_count,
                other_write_errors,
                batch_elapsed_ms,
            );
            stats.maybe_log();
        }
        !batch_failed
    }

    /// Verify trace authority using the original scope before acquiring the
    /// account-wide billing lock. SQLite needs these explicit owner checks too.
    async fn trace_batch_scope(
        pool: &DatabasePool,
        writes: &[DatabaseWrite],
    ) -> Result<(Uuid, Uuid), String> {
        let mut scope = None;
        let mut run_ids = AHashSet::new();
        let mut call_owners = AHashMap::new();
        for write in writes {
            let DatabaseWrite::Call {
                execution_context,
                call_id,
                ..
            } = write
            else {
                return Err("Unexpected non-call write in trace batch".into());
            };
            let (Some(org), Some(env)) = (execution_context.org_id, execution_context.env_id)
            else {
                return Err(
                    "Trace persistence requires an exact organization and environment".into(),
                );
            };
            if org.is_nil() || env.is_nil() || scope.is_some_and(|owner| owner != (org, env)) {
                return Err("Trace batch contains incompatible execution scopes".into());
            }
            scope = Some((org, env));
            run_ids.insert(execution_context.run_id);
            if call_owners
                .insert(*call_id, execution_context.run_id)
                .is_some_and(|run| run != execution_context.run_id)
            {
                return Err("Trace call identity changed run ownership".into());
            }
        }
        let (org_id, env_id) = scope.ok_or("Empty trace batch")?;
        if let DatabasePool::Postgres(pg) = pool
            && !matches!(pg.scope().map_err(|e| e.to_string())?, DatabaseScope::Environment { org_id: org, env_id: env } if org == org_id && env == env_id)
        {
            return Err("Trace writes require their exact environment scope".into());
        }
        for run_id in run_ids {
            let owner: Option<Uuid> = match pool {
                DatabasePool::Postgres(pg) => {
                    sqlx::query_scalar("SELECT env_id FROM run WHERE run_id=$1")
                        .bind(run_id)
                        .fetch_optional(pg)
                        .await
                }
                DatabasePool::Sqlite(sqlite) => {
                    sqlx::query_scalar("SELECT env_id FROM run WHERE run_id=$1")
                        .bind(run_id)
                        .fetch_optional(sqlite)
                        .await
                }
            }
            .map_err(|e| e.to_string())?;
            if owner != Some(env_id) {
                return Err("Trace context does not own its durable run".into());
            }
        }
        for write in writes {
            let DatabaseWrite::Call {
                execution_context,
                call_id,
                parent_call_id,
                ..
            } = write
            else {
                unreachable!("validated call batch");
            };
            for id in std::iter::once(call_id).chain(parent_call_id.iter()) {
                if call_owners
                    .get(id)
                    .is_some_and(|run| *run != execution_context.run_id)
                {
                    return Err("Trace parent belongs to a different run".into());
                }
            }
        }
        Ok((org_id, env_id))
    }

    /// Execute a call write inside the admitted SQLite transaction.
    async fn execute_write_sqlite<'e, E>(
        executor: E,
        write: &DatabaseWrite,
    ) -> Result<(), sqlx::Error>
    where
        E: sqlx::Executor<'e, Database = Sqlite>,
    {
        match write {
            DatabaseWrite::Call {
                execution_context,
                call_id,
                parent_call_id,
                function_name,
                static_scope,
                runtime_path,
                call_depth,
                args,
                return_value,
                error,
                flow,
                file,
                line,
                column,
                position,
                start_time,
                stop_time,
                duration_us,
            } => {
                Self::write_call_sqlite(
                    executor,
                    execution_context,
                    *call_id,
                    *parent_call_id,
                    function_name,
                    static_scope,
                    runtime_path,
                    *call_depth,
                    args.as_deref(),
                    return_value.as_deref(),
                    error.as_deref(),
                    flow.as_deref(),
                    file.as_deref(),
                    *line,
                    *column,
                    *position,
                    *start_time,
                    *stop_time,
                    *duration_us,
                )
                .await
            }
            other => {
                tracing::warn!(
                    "DatabaseWriter: Unexpected write type in SQLite transaction batch: {:?}",
                    other
                );
                Err(sqlx::Error::Protocol(
                    "unexpected write type in SQLite batch".into(),
                ))
            }
        }
    }

    /// Build phase timing diagnostics for the run's existing JSON info field.
    fn queue_timing_info(
        ctx: &ExecutionContext,
        start_time: chrono::DateTime<chrono::Utc>,
    ) -> Option<serde_json::Value> {
        let timing = ctx.queue_timing.as_ref()?;
        let handler_dispatched_at = timing.handler_dispatched_at.unwrap_or(timing.claimed_at);
        let handler_dispatch_wait_us = handler_dispatched_at
            .signed_duration_since(timing.claimed_at)
            .num_microseconds()
            .unwrap_or(0)
            .max(0);
        let worker_preparation_us = start_time
            .signed_duration_since(handler_dispatched_at)
            .num_microseconds()
            .unwrap_or(0)
            .max(0);
        let (queue_wait_us, retry_queue_age_us) = if timing.redelivered {
            (None, Some(timing.queue_wait_us))
        } else {
            (Some(timing.queue_wait_us), None)
        };

        Some(serde_json::json!({
            "queue_timing": {
                "backend": timing.backend.clone(),
                "enqueued_at": timing.enqueued_at.map(|value| value.to_rfc3339()),
                "claimed_at": timing.claimed_at.to_rfc3339(),
                "queue_wait_us": queue_wait_us,
                "retry_queue_age_us": retry_queue_age_us,
                "redelivered": timing.redelivered,
                "handler_dispatched_at": handler_dispatched_at.to_rfc3339(),
                "handler_dispatch_wait_us": handler_dispatch_wait_us,
                "worker_preparation_us": worker_preparation_us,
            }
        }))
    }

    /// Write a run:start record.
    ///
    /// `start_time` is the EngineEvent timestamp captured when execution
    /// actually begins. Origin FK failures are retried because a child event
    /// can race its parent's asynchronous run:start write.
    async fn write_run_start(
        pool: &DatabasePool,
        ctx: &ExecutionContext,
        event_time: chrono::DateTime<chrono::Utc>,
    ) -> Result<(), sqlx::Error> {
        // Use event_time which is captured when EngineEvent::run_start() is created
        // (the actual moment the run starts executing), not when this DB write happens
        let start_time = event_time;
        let timing_info = Self::queue_timing_info(ctx, start_time);
        tracing::debug!(
            "DatabaseWriter: Writing run:start for run_id={}, event_id={:?}, origin_run_id={:?}",
            ctx.run_id,
            ctx.event_id,
            ctx.origin_run_id
        );
        tracing::debug!(
            "DatabaseWriter: run:start values: env_id={:?}, stream_id={}, build_id={:?}, run_type_id={}, user_id={:?}",
            ctx.env_id,
            ctx.stream_id,
            ctx.build_id,
            ctx.run_type_id,
            ctx.user_id
        );

        // Ensure stream exists
        crate::db::stream::Stream::create_or_get_stream(
            pool,
            ctx.stream_id,
            ctx.env_id.unwrap_or_default(),
        )
        .await
        .map_err(|e| sqlx::Error::Protocol(format!("Failed to create stream: {}", e)))?;

        // Retry logic for foreign key violations on origin_run_id
        // This handles race conditions where the parent run's run:start hasn't been written yet
        const MAX_RETRIES: u32 = 5;
        const RETRY_DELAY_MS: u64 = 50;

        let mut last_error: Option<sqlx::Error> = None;

        for attempt in 0..MAX_RETRIES {
            let result: Result<u64, sqlx::Error> = match pool {
                DatabasePool::Postgres(pg_pool) => {
                    sqlx::query(
                        "INSERT INTO run (run_id, env_id, stream_id, build_id, run_type_id, origin_run_id, event_id, start_time, status_id, by_user_id, retry_attempt, access_id, agent_type, info)
                         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14)
                         ON CONFLICT (run_id) DO NOTHING"
                    )
                    .bind(ctx.run_id)
                    .bind(ctx.env_id)
                    .bind(ctx.stream_id)
                    .bind(ctx.build_id)
                    .bind(ctx.run_type_id)
                    .bind(ctx.origin_run_id)
                    .bind(ctx.event_id)
                    .bind(start_time)
                    .bind(1i16) // running
                    .bind(ctx.user_id)
                    .bind(ctx.retry_attempt)
                    .bind(ctx.access_id)
                    .bind(&ctx.agent_type)
                    .bind(&timing_info)
                    .execute(pg_pool)
                    .await
                    .map(|result| result.rows_affected())
                }
                DatabasePool::Sqlite(sqlite_pool) => {
                    sqlx::query(
                        "INSERT OR IGNORE INTO run (run_id, env_id, stream_id, build_id, run_type_id, origin_run_id, event_id, start_time, status_id, by_user_id, retry_attempt, access_id, agent_type, info)
                         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"
                    )
                    .bind(ctx.run_id)
                    .bind(ctx.env_id)
                    .bind(ctx.stream_id)
                    .bind(ctx.build_id)
                    .bind(ctx.run_type_id)
                    .bind(ctx.origin_run_id)
                    .bind(ctx.event_id)
                    .bind(start_time)
                    .bind(1i16) // running
                    .bind(ctx.user_id)
                    .bind(ctx.retry_attempt)
                    .bind(ctx.access_id)
                    .bind(&ctx.agent_type)
                    .bind(timing_info.as_ref().map(serde_json::Value::to_string))
                    .execute(sqlite_pool)
                    .await
                    .map(|result| result.rows_affected())
                }
            };

            match result {
                Ok(rows_affected) => {
                    if attempt > 0 {
                        tracing::debug!(
                            "DatabaseWriter: run:start for run_id={} succeeded on attempt {}",
                            ctx.run_id,
                            attempt + 1
                        );
                    }
                    if rows_affected > 0 {
                        crate::db::stream::Stream::record_run_started(pool, &ctx.stream_id)
                            .await
                            .map_err(|e| {
                                sqlx::Error::Protocol(format!(
                                    "Failed to update stream run metrics: {}",
                                    e
                                ))
                            })?;
                    } else {
                        // Row already exists: either a placeholder from
                        // ensure_run_exists (task/file provenance) or a
                        // duplicate run:start. Backfill authoritative fields
                        // the placeholder lacks; idempotent for duplicates.
                        // Best-effort: origin_run_id can FK-fail if the parent
                        // run is also still unflushed.
                        if let Err(e) = Self::backfill_run_provenance(pool, ctx, start_time).await {
                            tracing::warn!(
                                "DatabaseWriter: run provenance backfill failed for {}: {}",
                                ctx.run_id,
                                e
                            );
                        }
                    }
                    last_error = None;
                    break;
                }
                Err(e) => {
                    let error_str = e.to_string();
                    // Check if this is a foreign key violation on origin_run_id
                    let is_origin_fk_error = error_str.contains("origin_run_id_fkey")
                        || (error_str.contains("FOREIGN KEY constraint failed")
                            && ctx.origin_run_id.is_some());

                    if is_origin_fk_error && attempt < MAX_RETRIES - 1 {
                        tracing::debug!(
                            "DatabaseWriter: FK violation for origin_run_id={:?}, retrying in {}ms (attempt {}/{})",
                            ctx.origin_run_id,
                            RETRY_DELAY_MS,
                            attempt + 1,
                            MAX_RETRIES
                        );
                        tokio::time::sleep(std::time::Duration::from_millis(RETRY_DELAY_MS)).await;
                        last_error = Some(e);
                    } else {
                        // Not a retryable error or max retries reached
                        return Err(e);
                    }
                }
            }
        }

        // If we exited the loop with an error, return it
        if let Some(e) = last_error {
            return Err(e);
        }

        if let Some(evt_id) = ctx.event_id
            && let Err(e) = crate::db::Event::mark_event_as_handled(pool, &evt_id).await
        {
            tracing::error!(
                "DatabaseWriter: Failed to mark event {} as handled: {}",
                evt_id,
                e
            );
        }

        Ok(())
    }

    /// Backfill provenance on a pre-existing run row.
    ///
    /// `Run::ensure_run_exists` (used by `::hot::task/start`, `::hot::box/start`,
    /// and file-metadata writes) inserts placeholder rows before the emitter's
    /// authoritative `run:start` lands. Those placeholders lack `event_id`,
    /// `origin_run_id`, and `agent_type`, and carry an approximate
    /// `start_time`. When the real run:start hits the insert-or-ignore
    /// conflict, this fills in what the placeholder is missing. COALESCE keeps
    /// it idempotent for genuine duplicate run:start events.
    async fn backfill_run_provenance(
        pool: &DatabasePool,
        ctx: &ExecutionContext,
        start_time: chrono::DateTime<chrono::Utc>,
    ) -> Result<(), sqlx::Error> {
        let timing_info = Self::queue_timing_info(ctx, start_time);
        match pool {
            DatabasePool::Postgres(pg_pool) => {
                sqlx::query(
                    "UPDATE hot.run SET
                         origin_run_id = COALESCE(origin_run_id, $2),
                         event_id = COALESCE(event_id, $3),
                         agent_type = COALESCE(agent_type, $4),
                         start_time = LEAST(start_time, $5),
                         info = COALESCE(info, '{}'::jsonb) || COALESCE($6, '{}'::jsonb)
                     WHERE run_id = $1",
                )
                .bind(ctx.run_id)
                .bind(ctx.origin_run_id)
                .bind(ctx.event_id)
                .bind(&ctx.agent_type)
                .bind(start_time)
                .bind(&timing_info)
                .execute(pg_pool)
                .await?;
            }
            DatabasePool::Sqlite(sqlite_pool) => {
                let timing_info = timing_info.as_ref().map(serde_json::Value::to_string);
                sqlx::query(
                    "WITH patch AS (
                         SELECT key, value, type FROM json_each(COALESCE(?6, '{}'))
                     ),
                     existing AS (
                         SELECT key, value, type
                         FROM run, json_each(COALESCE(run.info, '{}'))
                         WHERE run.run_id = ?1
                           AND key NOT IN (SELECT key FROM patch)
                     ),
                     merged AS (
                         SELECT key, value, type FROM existing
                         UNION ALL
                         SELECT key, value, type FROM patch
                     ),
                     replacement AS (
                         SELECT COALESCE(
                             json_group_object(
                                 key,
                                 CASE
                                     WHEN type IN ('array', 'object') THEN json(value)
                                     WHEN type = 'true' THEN json('true')
                                     WHEN type = 'false' THEN json('false')
                                     WHEN type = 'null' THEN json('null')
                                     ELSE value
                                 END
                             ),
                             '{}'
                         ) AS info
                         FROM merged
                     )
                     UPDATE run SET
                         origin_run_id = COALESCE(origin_run_id, ?2),
                         event_id = COALESCE(event_id, ?3),
                         agent_type = COALESCE(agent_type, ?4),
                         start_time = MIN(start_time, ?5),
                         info = CASE
                             WHEN ?6 IS NULL THEN info
                             ELSE (SELECT info FROM replacement)
                         END
                     WHERE run_id = ?1",
                )
                .bind(ctx.run_id)
                .bind(ctx.origin_run_id)
                .bind(ctx.event_id)
                .bind(&ctx.agent_type)
                .bind(start_time)
                .bind(&timing_info)
                .execute(sqlite_pool)
                .await?;
            }
        }
        Ok(())
    }

    async fn write_run_start_stop(
        pool: &DatabasePool,
        ctx: &ExecutionContext,
        start_time: chrono::DateTime<chrono::Utc>,
        stop_time: chrono::DateTime<chrono::Utc>,
        result: &Val,
    ) -> Result<(), sqlx::Error> {
        let result_json = val_to_jsonb_string(result);
        let timing_info = Self::queue_timing_info(ctx, start_time);

        crate::db::stream::Stream::create_or_get_stream(
            pool,
            ctx.stream_id,
            ctx.env_id.unwrap_or_default(),
        )
        .await
        .map_err(|e| sqlx::Error::Protocol(format!("Failed to create stream: {}", e)))?;

        let rows_affected = match pool {
            DatabasePool::Postgres(pg_pool) => {
                sqlx::query(
                    "INSERT INTO hot.run (run_id, env_id, stream_id, build_id, run_type_id, origin_run_id, event_id, start_time, stop_time, status_id, result, by_user_id, retry_attempt, access_id, agent_type, info)
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11::jsonb, $12, $13, $14, $15, $16)
                     ON CONFLICT (run_id) DO UPDATE
                     SET stop_time = EXCLUDED.stop_time,
                         status_id = EXCLUDED.status_id,
                         result = EXCLUDED.result,
                         info = COALESCE(hot.run.info, '{}'::jsonb) || COALESCE(EXCLUDED.info, '{}'::jsonb)
                     WHERE hot.run.status_id = 1"
                )
                .bind(ctx.run_id)
                .bind(ctx.env_id)
                .bind(ctx.stream_id)
                .bind(ctx.build_id)
                .bind(ctx.run_type_id)
                .bind(ctx.origin_run_id)
                .bind(ctx.event_id)
                .bind(start_time)
                .bind(stop_time)
                .bind(2i16)
                .bind(&result_json)
                .bind(ctx.user_id)
                .bind(ctx.retry_attempt)
                .bind(ctx.access_id)
                .bind(&ctx.agent_type)
                .bind(&timing_info)
                .execute(pg_pool)
                .await?
                .rows_affected()
            }
            DatabasePool::Sqlite(sqlite_pool) => {
                sqlx::query(
                    "INSERT OR IGNORE INTO run (run_id, env_id, stream_id, build_id, run_type_id, origin_run_id, event_id, start_time, stop_time, status_id, result, by_user_id, retry_attempt, access_id, agent_type, info)
                     VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"
                )
                .bind(ctx.run_id)
                .bind(ctx.env_id)
                .bind(ctx.stream_id)
                .bind(ctx.build_id)
                .bind(ctx.run_type_id)
                .bind(ctx.origin_run_id)
                .bind(ctx.event_id)
                .bind(start_time)
                .bind(stop_time)
                .bind(2i16)
                .bind(&result_json)
                .bind(ctx.user_id)
                .bind(ctx.retry_attempt)
                .bind(ctx.access_id)
                .bind(&ctx.agent_type)
                .bind(timing_info.as_ref().map(serde_json::Value::to_string))
                .execute(sqlite_pool)
                .await?
                .rows_affected()
            }
        };

        if rows_affected == 0 {
            // Placeholder row exists (ensure_run_exists): backfill provenance
            // the placeholder lacks before finalizing the terminal status.
            // Best-effort: origin_run_id can FK-fail if the parent run is
            // also still unflushed.
            if let Err(e) = Self::backfill_run_provenance(pool, ctx, start_time).await {
                tracing::warn!(
                    "DatabaseWriter: run provenance backfill failed for {}: {}",
                    ctx.run_id,
                    e
                );
            }
            Self::write_run_stop(pool, ctx.run_id, stop_time, result).await?;
        } else {
            crate::db::stream::Stream::record_run_started_and_finished(
                pool,
                &ctx.stream_id,
                duration_ms_between(start_time, stop_time),
            )
            .await
            .map_err(|e| {
                sqlx::Error::Protocol(format!("Failed to update stream run metrics: {}", e))
            })?;
        }

        if let Some(evt_id) = ctx.event_id
            && let Err(e) = crate::db::Event::mark_event_as_handled(pool, &evt_id).await
        {
            tracing::error!(
                "DatabaseWriter: Failed to mark event {} as handled: {}",
                evt_id,
                e
            );
        }

        Ok(())
    }

    /// Write a run:stop record
    async fn write_run_stop(
        pool: &DatabasePool,
        run_id: Uuid,
        event_time: chrono::DateTime<chrono::Utc>,
        result: &Val,
    ) -> Result<(), sqlx::Error> {
        let result_json = val_to_jsonb_string(result);

        let rows_affected = match pool {
            DatabasePool::Postgres(pg_pool) => {
                sqlx::query(
                    "UPDATE hot.run SET stop_time = $1, status_id = $2, result = $3::jsonb WHERE run_id = $4 AND status_id = $5"
                )
                .bind(event_time)
                .bind(2i16) // succeeded
                .bind(&result_json)
                .bind(run_id)
                .bind(1i16) // running
                .execute(pg_pool)
                .await?
                .rows_affected()
            }
            DatabasePool::Sqlite(sqlite_pool) => {
                sqlx::query(
                    "UPDATE run SET stop_time = ?, status_id = ?, result = ? WHERE run_id = ? AND status_id = ?",
                )
                .bind(event_time)
                .bind(2i16) // succeeded
                .bind(&result_json)
                .bind(run_id)
                .bind(1i16) // running
                .execute(sqlite_pool)
                .await?
                .rows_affected()
            }
        };

        if rows_affected > 0 {
            Self::record_run_finished(pool, run_id).await?;
        } else {
            Self::require_visible_terminal_run(pool, run_id).await?;
        }
        Ok(())
    }

    async fn require_visible_terminal_run(
        pool: &DatabasePool,
        run_id: Uuid,
    ) -> Result<(), sqlx::Error> {
        let status: Option<i16> = match pool {
            DatabasePool::Postgres(pool) => {
                sqlx::query_scalar("SELECT status_id FROM run WHERE run_id=$1")
                    .bind(run_id)
                    .fetch_optional(pool)
                    .await?
            }
            DatabasePool::Sqlite(pool) => {
                sqlx::query_scalar("SELECT status_id FROM run WHERE run_id=?")
                    .bind(run_id)
                    .fetch_optional(pool)
                    .await?
            }
        };
        match status {
            Some(status) if status != 1 => Ok(()),
            _ => Err(sqlx::Error::RowNotFound),
        }
    }

    /// Write a run:fail record
    /// If the run has retry config and hasn't exhausted retries, sets status to pending_retry
    /// Retry config is looked up from the event handler or schedule metadata
    async fn write_run_fail(
        pool: &DatabasePool,
        run_id: Uuid,
        event_time: chrono::DateTime<chrono::Utc>,
        failure: &Val,
    ) -> Result<(), sqlx::Error> {
        use crate::env::retry::{RetryConfig, calculate_retry_delay};

        // Store the failure value directly - the status_id already indicates failure
        let result_json = val_to_jsonb_string(failure);

        // Query run to get type, build_id, event_id, env_id, current retry attempt,
        // and status. Late `run:fail` events from detached timed-out VMs must not
        // overwrite a terminal run or schedule a duplicate retry.
        let run_info: Option<RunInfo> = match pool {
            DatabasePool::Postgres(pg_pool) => {
                sqlx::query_as("SELECT run_type_id, build_id, event_id, env_id, retry_attempt, status_id FROM hot.run WHERE run_id = $1")
                    .bind(run_id)
                    .fetch_optional(pg_pool)
                    .await?
            }
            DatabasePool::Sqlite(sqlite_pool) => {
                sqlx::query_as("SELECT run_type_id, build_id, event_id, env_id, retry_attempt, status_id FROM run WHERE run_id = ?")
                    .bind(run_id)
                    .fetch_optional(sqlite_pool)
                    .await?
            }
        };

        let Some((run_type_id, build_id, event_id, env_id, retry_attempt, status_id)) = run_info
        else {
            return Err(sqlx::Error::RowNotFound);
        };

        if status_id != 1 {
            tracing::debug!(
                run_id = %run_id,
                status_id,
                "Ignoring late run:fail for non-running run"
            );
            return Ok(());
        }

        // Look up retry config from handler/schedule metadata based on run type
        let retry_config = match run_type_id {
            2 => {
                // Event run - look up event handler
                Self::get_event_handler_retry_config(pool, build_id, event_id).await
            }
            3 => {
                // Schedule run - look up schedule
                Self::get_schedule_retry_config(pool, build_id, event_id).await
            }
            _ => {
                // Call, Run, Eval, Repl - no automatic retry
                RetryConfig::default()
            }
        };

        // Check if we should retry (current attempt < max_retries)
        let should_retry = retry_config.max_retries > 0 && retry_attempt < retry_config.max_retries;

        if should_retry {
            // Calculate next retry time using backoff strategy
            let delay_ms = calculate_retry_delay(
                retry_config.delay_ms,
                retry_attempt,
                retry_config.backoff,
                retry_config.max_delay_ms,
                retry_config.jitter,
            );
            let next_retry_at = event_time + chrono::Duration::milliseconds(delay_ms);
            let new_attempt = retry_attempt + 1;

            tracing::info!(
                "Run {} failed, scheduling retry {}/{} at {} (delay: {}ms, backoff: {:?})",
                run_id,
                new_attempt,
                retry_config.max_retries,
                next_retry_at,
                delay_ms,
                retry_config.backoff
            );

            match pool {
                DatabasePool::Postgres(pg_pool) => {
                    let result = sqlx::query(
                        "UPDATE hot.run SET stop_time = $1, status_id = $2, result = $3::jsonb, retry_attempt = $4, next_retry_at = $5 WHERE run_id = $6 AND status_id = $7"
                    )
                    .bind(event_time)
                    .bind(5i16) // pending_retry
                    .bind(&result_json)
                    .bind(new_attempt)
                    .bind(next_retry_at)
                    .bind(run_id)
                    .bind(1i16) // running
                    .execute(pg_pool)
                    .await?;
                    if result.rows_affected() == 0 {
                        tracing::debug!(
                            run_id = %run_id,
                            "Skipped retry scheduling for run that is no longer running"
                        );
                    } else {
                        Self::record_run_finished(pool, run_id).await?;
                    }
                }
                DatabasePool::Sqlite(sqlite_pool) => {
                    let result = sqlx::query(
                        "UPDATE run SET stop_time = ?, status_id = ?, result = ?, retry_attempt = ?, next_retry_at = ? WHERE run_id = ? AND status_id = ?",
                    )
                    .bind(event_time)
                    .bind(5i16) // pending_retry
                    .bind(&result_json)
                    .bind(new_attempt)
                    .bind(next_retry_at)
                    .bind(run_id)
                    .bind(1i16) // running
                    .execute(sqlite_pool)
                    .await?;
                    if result.rows_affected() == 0 {
                        tracing::debug!(
                            run_id = %run_id,
                            "Skipped retry scheduling for run that is no longer running"
                        );
                    } else {
                        Self::record_run_finished(pool, run_id).await?;
                    }
                }
            }
        } else {
            // No retries or exhausted - mark as failed and publish alert
            if retry_config.max_retries > 0 {
                tracing::info!(
                    "Run {} failed after {} retry attempts (max: {})",
                    run_id,
                    retry_attempt,
                    retry_config.max_retries
                );
            }

            let updated = Self::update_run_failed(
                pool,
                run_id,
                event_time,
                &result_json,
                Some(env_id),
                Some(run_type_id),
            )
            .await?;
            if !updated {
                tracing::debug!(
                    run_id = %run_id,
                    "Skipped run:fail update for run that is no longer running"
                );
            } else {
                Self::record_run_finished(pool, run_id).await?;
            }
        }

        Ok(())
    }

    async fn record_run_finished(pool: &DatabasePool, run_id: Uuid) -> Result<(), sqlx::Error> {
        crate::db::stream::Stream::record_run_finished(pool, &run_id)
            .await
            .map_err(|e| {
                sqlx::Error::Protocol(format!("Failed to update stream run metrics: {}", e))
            })
    }

    /// Helper to update run as failed and publish alert
    async fn update_run_failed(
        pool: &DatabasePool,
        run_id: Uuid,
        event_time: chrono::DateTime<chrono::Utc>,
        result_json: &str,
        env_id: Option<Uuid>,
        run_type_id: Option<i16>,
    ) -> Result<bool, sqlx::Error> {
        let rows_affected = match pool {
            DatabasePool::Postgres(pg_pool) => {
                sqlx::query(
                    "UPDATE hot.run SET stop_time = $1, status_id = $2, result = $3::jsonb WHERE run_id = $4 AND status_id = $5"
                )
                .bind(event_time)
                .bind(3i16) // failed
                .bind(result_json)
                .bind(run_id)
                .bind(1i16) // running
                .execute(pg_pool)
                .await?
                .rows_affected()
            }
            DatabasePool::Sqlite(sqlite_pool) => {
                sqlx::query(
                    "UPDATE run SET stop_time = ?, status_id = ?, result = ? WHERE run_id = ? AND status_id = ?",
                )
                .bind(event_time)
                .bind(3i16) // failed
                .bind(result_json)
                .bind(run_id)
                .bind(1i16) // running
                .execute(sqlite_pool)
                .await?
                .rows_affected()
            }
        };

        if rows_affected == 0 {
            return Ok(false);
        }

        // Publish run:failed alert if we have env_id
        // Skip for task-type runs (id=7) — the task worker handles its own task:failed alerts
        if let Some(env_id) = env_id
            && run_type_id != Some(7)
        {
            Self::publish_run_failed_alert(pool, run_id, env_id, result_json).await;
        }

        Ok(true)
    }

    /// Publish a run:failed alert (async, doesn't block)
    async fn publish_run_failed_alert(
        pool: &DatabasePool,
        run_id: Uuid,
        env_id: Uuid,
        result_json: &str,
    ) {
        use crate::db::alert::publish_alert;

        // Get org_id from env
        let org_id: Option<Uuid> = match pool {
            DatabasePool::Postgres(pg_pool) => {
                sqlx::query_scalar("SELECT org_id FROM env WHERE env_id = $1")
                    .bind(env_id)
                    .fetch_optional(pg_pool)
                    .await
                    .ok()
                    .flatten()
            }
            DatabasePool::Sqlite(sqlite_pool) => {
                sqlx::query_scalar("SELECT org_id FROM env WHERE env_id = ?")
                    .bind(env_id)
                    .fetch_optional(sqlite_pool)
                    .await
                    .ok()
                    .flatten()
            }
        };

        let Some(org_id) = org_id else {
            tracing::warn!(
                "Could not find org_id for env {} when publishing run:failed alert",
                env_id
            );
            return;
        };

        // Build alert data
        let data = serde_json::json!({
            "run_id": run_id.to_string(),
            "env_id": env_id.to_string(),
            "error": serde_json::from_str::<serde_json::Value>(result_json).unwrap_or_default(),
            "timestamp": chrono::Utc::now().to_rfc3339(),
        });

        // Publish alert (fire and forget - don't fail the run update if alert fails)
        match publish_alert(pool, &org_id, &env_id, "run:failed", &data).await {
            Ok(alert) => {
                tracing::debug!(
                    "Published run:failed alert {} for run {}",
                    alert.alert_id,
                    run_id
                );
            }
            Err(e) => {
                tracing::error!(
                    "Failed to publish run:failed alert for run {}: {}",
                    run_id,
                    e
                );
            }
        }
    }

    /// Get retry config from event handler metadata
    async fn get_event_handler_retry_config(
        pool: &DatabasePool,
        build_id: Option<Uuid>,
        event_id: Option<Uuid>,
    ) -> crate::env::retry::RetryConfig {
        use crate::env::retry::RetryConfig;

        let Some(build_id) = build_id else {
            return RetryConfig::default();
        };
        let Some(event_id) = event_id else {
            return RetryConfig::default();
        };

        // Get event type from event
        let event_type: Option<String> = match pool {
            DatabasePool::Postgres(pg_pool) => {
                sqlx::query_scalar("SELECT event_type FROM hot.event WHERE event_id = $1")
                    .bind(event_id)
                    .fetch_optional(pg_pool)
                    .await
                    .ok()
                    .flatten()
            }
            DatabasePool::Sqlite(sqlite_pool) => {
                sqlx::query_scalar("SELECT event_type FROM event WHERE event_id = ?")
                    .bind(event_id)
                    .fetch_optional(sqlite_pool)
                    .await
                    .ok()
                    .flatten()
            }
        };

        let Some(event_type) = event_type else {
            return RetryConfig::default();
        };

        // Look up event handler by build_id and event_type
        let meta: Option<serde_json::Value> = match pool {
            DatabasePool::Postgres(pg_pool) => {
                sqlx::query_scalar(
                    "SELECT meta FROM hot.event_handler WHERE build_id = $1 AND event_type = $2 LIMIT 1"
                )
                .bind(build_id)
                .bind(&event_type)
                .fetch_optional(pg_pool)
                .await
                .ok()
                .flatten()
            }
            DatabasePool::Sqlite(sqlite_pool) => {
                sqlx::query_scalar(
                    "SELECT meta FROM event_handler WHERE build_id = ? AND event_type = ? LIMIT 1"
                )
                .bind(build_id)
                .bind(&event_type)
                .fetch_optional(sqlite_pool)
                .await
                .ok()
                .flatten()
            }
        };

        RetryConfig::from_meta(meta.as_ref())
    }

    /// Get retry config from schedule metadata
    async fn get_schedule_retry_config(
        pool: &DatabasePool,
        build_id: Option<Uuid>,
        event_id: Option<Uuid>,
    ) -> crate::env::retry::RetryConfig {
        use crate::env::retry::RetryConfig;

        let Some(build_id) = build_id else {
            return RetryConfig::default();
        };
        let Some(event_id) = event_id else {
            return RetryConfig::default();
        };

        // Get function name from event data (hot:schedule events store fn in event_data)
        let fn_name: Option<String> = match pool {
            DatabasePool::Postgres(pg_pool) => {
                sqlx::query_scalar("SELECT event_data->>'fn' FROM hot.event WHERE event_id = $1")
                    .bind(event_id)
                    .fetch_optional(pg_pool)
                    .await
                    .ok()
                    .flatten()
            }
            DatabasePool::Sqlite(sqlite_pool) => sqlx::query_scalar(
                "SELECT json_extract(event_data, '$.fn') FROM event WHERE event_id = ?",
            )
            .bind(event_id)
            .fetch_optional(sqlite_pool)
            .await
            .ok()
            .flatten(),
        };

        let Some(fn_name) = fn_name else {
            return RetryConfig::default();
        };

        // Parse ns/var from function name (format: "namespace/variable")
        let parts: Vec<&str> = fn_name.rsplitn(2, '/').collect();
        if parts.len() != 2 {
            return RetryConfig::default();
        }
        let (var, ns) = (parts[0], parts[1]);

        // Look up schedule by build_id, ns, and var
        let meta: Option<serde_json::Value> = match pool {
            DatabasePool::Postgres(pg_pool) => {
                sqlx::query_scalar(
                    "SELECT meta FROM hot.schedule WHERE build_id = $1 AND ns = $2 AND var = $3 LIMIT 1"
                )
                .bind(build_id)
                .bind(ns)
                .bind(var)
                .fetch_optional(pg_pool)
                .await
                .ok()
                .flatten()
            }
            DatabasePool::Sqlite(sqlite_pool) => {
                sqlx::query_scalar(
                    "SELECT meta FROM schedule WHERE build_id = ? AND ns = ? AND var = ? LIMIT 1"
                )
                .bind(build_id)
                .bind(ns)
                .bind(var)
                .fetch_optional(sqlite_pool)
                .await
                .ok()
                .flatten()
            }
        };

        RetryConfig::from_meta(meta.as_ref())
    }

    /// Write a run:cancel record
    async fn write_run_cancel(
        pool: &DatabasePool,
        run_id: Uuid,
        event_time: chrono::DateTime<chrono::Utc>,
        cancellation: &Val,
    ) -> Result<(), sqlx::Error> {
        // Store the cancellation value directly - the status_id already indicates cancellation
        // No need to wrap in a Result type since that's implied by the status
        let result_json = val_to_jsonb_string(cancellation);

        // Get env_id and run_type_id before updating so we can publish the alert
        let run_cancel_info: Option<(Uuid, i16)> = match pool {
            DatabasePool::Postgres(pg_pool) => {
                sqlx::query_as("SELECT env_id, run_type_id FROM hot.run WHERE run_id = $1")
                    .bind(run_id)
                    .fetch_optional(pg_pool)
                    .await
                    .ok()
                    .flatten()
            }
            DatabasePool::Sqlite(sqlite_pool) => {
                sqlx::query_as("SELECT env_id, run_type_id FROM run WHERE run_id = ?")
                    .bind(run_id)
                    .fetch_optional(sqlite_pool)
                    .await
                    .ok()
                    .flatten()
            }
        };

        // Only transition a *running* run to cancelled. A late run:cancel from a
        // detached, cooperatively-cancelled VM (e.g. one signalled by the worker
        // run-timeout backstop, which already recorded the run as failed) must not
        // overwrite a terminal status.
        let rows_affected = match pool {
            DatabasePool::Postgres(pg_pool) => {
                sqlx::query(
                    "UPDATE hot.run SET stop_time = $1, status_id = $2, result = $3::jsonb WHERE run_id = $4 AND status_id = $5"
                )
                .bind(event_time)
                .bind(4i16) // cancelled
                .bind(&result_json)
                .bind(run_id)
                .bind(1i16) // running
                .execute(pg_pool)
                .await?
                .rows_affected()
            }
            DatabasePool::Sqlite(sqlite_pool) => {
                sqlx::query(
                    "UPDATE run SET stop_time = ?, status_id = ?, result = ? WHERE run_id = ? AND status_id = ?",
                )
                .bind(event_time)
                .bind(4i16) // cancelled
                .bind(&result_json)
                .bind(run_id)
                .bind(1i16) // running
                .execute(sqlite_pool)
                .await?
                .rows_affected()
            }
        };

        if rows_affected == 0 {
            Self::require_visible_terminal_run(pool, run_id).await?;
            tracing::debug!(
                run_id = %run_id,
                "Ignoring late run:cancel for run that is no longer running"
            );
            return Ok(());
        }

        Self::record_run_finished(pool, run_id).await?;

        // Publish run:cancelled alert if we have env_id
        // Skip for task-type runs (id=7) — the task worker handles its own task:cancelled alerts
        if let Some((env_id, run_type_id)) = run_cancel_info
            && run_type_id != 7
        {
            Self::publish_run_cancelled_alert(pool, run_id, env_id, &result_json).await;
        }

        Ok(())
    }

    /// Publish a run:cancelled alert (async, doesn't block)
    async fn publish_run_cancelled_alert(
        pool: &DatabasePool,
        run_id: Uuid,
        env_id: Uuid,
        result_json: &str,
    ) {
        use crate::db::alert::publish_alert;

        // Get org_id from env
        let org_id: Option<Uuid> = match pool {
            DatabasePool::Postgres(pg_pool) => {
                sqlx::query_scalar("SELECT org_id FROM env WHERE env_id = $1")
                    .bind(env_id)
                    .fetch_optional(pg_pool)
                    .await
                    .ok()
                    .flatten()
            }
            DatabasePool::Sqlite(sqlite_pool) => {
                sqlx::query_scalar("SELECT org_id FROM env WHERE env_id = ?")
                    .bind(env_id)
                    .fetch_optional(sqlite_pool)
                    .await
                    .ok()
                    .flatten()
            }
        };

        let Some(org_id) = org_id else {
            tracing::warn!(
                "Could not find org_id for env {} when publishing run:cancelled alert",
                env_id
            );
            return;
        };

        // Build alert data
        let data = serde_json::json!({
            "run_id": run_id.to_string(),
            "env_id": env_id.to_string(),
            "reason": serde_json::from_str::<serde_json::Value>(result_json).unwrap_or_default(),
            "timestamp": chrono::Utc::now().to_rfc3339(),
        });

        // Publish alert (fire and forget)
        match publish_alert(pool, &org_id, &env_id, "run:cancelled", &data).await {
            Ok(alert) => {
                tracing::debug!(
                    "Published run:cancelled alert {} for run {}",
                    alert.alert_id,
                    run_id
                );
            }
            Err(e) => {
                tracing::error!(
                    "Failed to publish run:cancelled alert for run {}: {}",
                    run_id,
                    e
                );
            }
        }
    }

    /// Write a call record to SQLite using UPSERT (pool connection or transaction).
    #[allow(clippy::too_many_arguments)]
    async fn write_call_sqlite<'e, E>(
        executor: E,
        ctx: &ExecutionContext,
        call_id: Uuid,
        parent_call_id: Option<Uuid>,
        function_name: &str,
        static_scope: &str,
        runtime_path: &str,
        call_depth: i64,
        args: Option<&str>,
        return_value: Option<&str>,
        error: Option<&str>,
        flow: Option<&str>,
        file: Option<&str>,
        line: Option<i64>,
        column: Option<i64>,
        position: Option<i64>,
        start_time: Option<chrono::DateTime<chrono::Utc>>,
        stop_time: Option<chrono::DateTime<chrono::Utc>>,
        duration_us: Option<i64>,
    ) -> Result<(), sqlx::Error>
    where
        E: sqlx::Executor<'e, Database = Sqlite>,
    {
        let args_safe = args.map(sanitize_json_for_jsonb);
        let return_value_safe = return_value.map(sanitize_json_for_jsonb);
        let error_safe = error.map(sanitize_text_for_postgres);
        let flow_safe = flow.map(sanitize_json_for_jsonb);

        let result = sqlx::query(
            "INSERT INTO call (call_id, run_id, parent_call_id, function_name, static_scope, runtime_path, call_depth, args, return_value, error, flow, start_time, stop_time, duration_us, file, line, \"column\", position, size, env_id)
             SELECT ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18,
                 COALESCE(length(CAST(?8 AS BLOB)), 0) + COALESCE(length(CAST(?9 AS BLOB)), 0) +
                 COALESCE(length(CAST(?10 AS BLOB)), 0) + COALESCE(length(CAST(?11 AS BLOB)), 0) + 50, ?19
             WHERE ?3 IS NULL OR EXISTS (SELECT 1 FROM call parent WHERE parent.call_id=?3 AND parent.run_id=?2)
             ON CONFLICT (call_id) DO UPDATE SET
                 stop_time = COALESCE(excluded.stop_time, call.stop_time),
                 return_value = COALESCE(excluded.return_value, call.return_value),
                 error = COALESCE(excluded.error, call.error),
                 flow = COALESCE(excluded.flow, call.flow),
                 duration_us = COALESCE(excluded.duration_us, call.duration_us),
                 start_time = COALESCE(excluded.start_time, call.start_time),
                 parent_call_id = COALESCE(excluded.parent_call_id, call.parent_call_id),
                 -- Use NULLIF to treat 'unknown' as NULL, preferring real function names
                 function_name = COALESCE(NULLIF(excluded.function_name, 'unknown'), NULLIF(call.function_name, 'unknown'), 'unknown'),
                 static_scope = COALESCE(NULLIF(excluded.static_scope, 'unknown'), NULLIF(call.static_scope, 'unknown'), 'unknown'),
                 runtime_path = COALESCE(NULLIF(excluded.runtime_path, 'unknown'), NULLIF(call.runtime_path, 'unknown'), 'unknown'),
                 call_depth = CASE WHEN excluded.call_depth = 0 AND call.call_depth != 0 THEN call.call_depth ELSE COALESCE(excluded.call_depth, call.call_depth) END,
                 args = COALESCE(excluded.args, call.args),
                 file = COALESCE(excluded.file, call.file),
                 line = COALESCE(excluded.line, call.line),
                 \"column\" = COALESCE(excluded.\"column\", call.\"column\"),
                 position = COALESCE(excluded.position, call.position),
                 size = COALESCE(length(CAST(COALESCE(excluded.args, call.args) AS BLOB)), 0) +
                        COALESCE(length(CAST(COALESCE(excluded.return_value, call.return_value) AS BLOB)), 0) +
                        COALESCE(length(CAST(COALESCE(excluded.flow, call.flow) AS BLOB)), 0) +
                        COALESCE(length(CAST(COALESCE(excluded.error, call.error) AS BLOB)), 0) + 50
             WHERE call.run_id = excluded.run_id",
        )
        .bind(call_id)
        .bind(ctx.run_id)
        .bind(parent_call_id)
        .bind(function_name)
        .bind(static_scope)
        .bind(runtime_path)
        .bind(call_depth as i32)
        .bind(args_safe.as_deref())
        .bind(return_value_safe.as_deref())
        .bind(error_safe.as_deref())
        .bind(flow_safe.as_deref())
        .bind(start_time)
        .bind(stop_time)
        .bind(duration_us)
        .bind(file)
        .bind(line)
        .bind(column)
        .bind(position)
        .bind(ctx.env_id)
        .execute(executor)
        .await?;
        if result.rows_affected() != 1 {
            return Err(sqlx::Error::Protocol(
                "Trace call or parent ownership changed".into(),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lang::bytecode::LambdaInfo;

    fn trace_write(
        ctx: &ExecutionContext,
        call_id: Uuid,
        args: Option<&str>,
        error: &str,
    ) -> DatabaseWrite {
        DatabaseWrite::Call {
            execution_context: Box::new(ctx.clone()),
            call_id,
            parent_call_id: None,
            function_name: "test/trace".into(),
            static_scope: "test/trace".into(),
            runtime_path: "test/trace".into(),
            call_depth: 0,
            args: args.map(str::to_string),
            return_value: None,
            error: Some(error.into()),
            flow: None,
            file: None,
            line: None,
            column: None,
            position: None,
            start_time: Some(chrono::Utc::now()),
            stop_time: None,
            duration_us: None,
        }
    }

    #[tokio::test]
    async fn trace_quota_counts_utf8_errors_and_rolls_back_upsert_without_losing_terminal_run() {
        let db = crate::db::test_db().await;
        let data = crate::db::insert_test_data(&db).await.unwrap();
        let DatabasePool::Sqlite(pool) = &db else {
            unreachable!()
        };
        sqlx::query("UPDATE org SET features=? WHERE org_id=?")
            .bind(r#"{"call_storage_bytes":57}"#)
            .bind(data.org_id)
            .execute(pool)
            .await
            .unwrap();
        let context = ExecutionContext::new(
            data.run_id,
            data.stream_id,
            4,
            Some(data.env_id),
            Some(data.user_id),
            Some(data.org_id),
            Some(data.build_id),
        );
        let call = Uuid::new_v4();
        let writer = DatabaseWriter::new(Arc::new(RwLock::new(Some(db.clone()))));
        writer.write(trace_write(&context, call, Some("\"é\""), "ø"));
        writer.flush_run(data.run_id).await.unwrap();
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT size FROM call WHERE call_id=?")
                .bind(call)
                .fetch_one(pool)
                .await
                .unwrap(),
            56
        );
        // The merged update retains the old args and grows the error by two
        // bytes. Only its trace transaction rolls back when the account is full.
        writer.write(trace_write(&context, call, None, "øø"));
        writer.write(DatabaseWrite::RunStop {
            run_id: data.run_id,
            event_time: chrono::Utc::now(),
            result: Val::Null,
        });
        assert!(writer.flush_run(data.run_id).await.is_err());
        assert!(writer.flush_run(data.run_id).await.is_err());
        assert!(writer.shutdown().await.is_err());
        let row: (i64, String) = sqlx::query_as("SELECT size,error FROM call WHERE call_id=?")
            .bind(call)
            .fetch_one(pool)
            .await
            .unwrap();
        assert_eq!(row, (56, "ø".into()));
        assert_eq!(
            crate::db::Run::get_run(&db, &data.run_id)
                .await
                .unwrap()
                .status_id,
            crate::db::RunStatus::Succeeded.as_id()
        );

        // The upgrade backfill uses the same byte accounting as new writes.
        sqlx::query("UPDATE call SET size=0 WHERE call_id=?")
            .bind(call)
            .execute(pool)
            .await
            .unwrap();
        let migration = include_str!(
            "../../../../../resources/db/sqlite/migrations/015_call_storage_accounting.sql"
        );
        let start = migration.find("UPDATE call SET").unwrap();
        let end = start + migration[start..].find(';').unwrap() + 1;
        sqlx::raw_sql(&migration[start..end])
            .execute(pool)
            .await
            .unwrap();
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT size FROM call WHERE call_id=?")
                .bind(call)
                .fetch_one(pool)
                .await
                .unwrap(),
            56
        );
    }

    #[tokio::test]
    async fn calls_after_terminal_write_do_not_retain_run_scope() {
        let db = crate::db::test_db().await;
        let data = crate::db::insert_test_data(&db).await.unwrap();
        let context = ExecutionContext::new(
            data.run_id,
            data.stream_id,
            4,
            Some(data.env_id),
            Some(data.user_id),
            Some(data.org_id),
            Some(data.build_id),
        );
        let writer = DatabaseWriter::new(Arc::new(RwLock::new(Some(db.clone()))));
        writer.write(trace_write(&context, Uuid::new_v4(), None, "early"));
        writer.write(DatabaseWrite::RunStop {
            run_id: data.run_id,
            event_time: chrono::Utc::now(),
            result: Val::Null,
        });
        // The emitter batches calls, so they routinely follow run:stop.
        writer.write(trace_write(&context, Uuid::new_v4(), None, "late"));
        writer.flush_run(data.run_id).await.unwrap();
        let mut scopes = writer
            .run_scopes
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        assert!(matches!(
            scopes.runs.get(&data.run_id),
            Some(RunScope::Finished(..))
        ));
        scopes.prune(Instant::now() + FINISHED_RUN_SCOPE_TTL + RUN_SCOPE_PRUNE_INTERVAL);
        assert!(scopes.runs.is_empty());
    }

    #[tokio::test]
    async fn missing_run_failure_is_sticky_across_flushes() {
        let db = crate::db::test_db().await;
        let writer = DatabaseWriter::new(Arc::new(RwLock::new(Some(db))));
        let run_id = Uuid::new_v4();
        writer.write(DatabaseWrite::RunStop {
            run_id,
            event_time: chrono::Utc::now(),
            result: Val::Null,
        });
        assert!(writer.flush_run(run_id).await.is_err());
        assert!(writer.flush_run(run_id).await.is_err());
        assert!(writer.shutdown().await.is_err());
    }

    #[test]
    fn queue_timing_info_splits_queue_and_worker_preparation() {
        let claimed_at = chrono::Utc::now();
        let mut context = ExecutionContext::minimal(Uuid::now_v7(), Uuid::now_v7(), 4);
        context.queue_timing = Some(crate::lang::event::QueueExecutionTiming {
            backend: "redis".to_string(),
            enqueued_at: Some(claimed_at - chrono::Duration::milliseconds(7)),
            claimed_at,
            queue_wait_us: 7_000,
            redelivered: false,
            handler_dispatched_at: Some(claimed_at + chrono::Duration::milliseconds(3)),
        });

        let info = DatabaseWriter::queue_timing_info(
            &context,
            claimed_at + chrono::Duration::milliseconds(11),
        )
        .unwrap();
        assert_eq!(info["queue_timing"]["backend"], "redis");
        assert_eq!(info["queue_timing"]["queue_wait_us"], 7_000);
        assert_eq!(info["queue_timing"]["handler_dispatch_wait_us"], 3_000);
        assert_eq!(info["queue_timing"]["worker_preparation_us"], 8_000);
    }

    #[test]
    fn queue_timing_info_does_not_label_redelivery_age_as_queue_wait() {
        let claimed_at = chrono::Utc::now();
        let mut context = ExecutionContext::minimal(Uuid::now_v7(), Uuid::now_v7(), 4);
        context.queue_timing = Some(crate::lang::event::QueueExecutionTiming {
            backend: "redis".to_string(),
            enqueued_at: Some(claimed_at - chrono::Duration::seconds(20)),
            claimed_at,
            queue_wait_us: 20_000_000,
            redelivered: true,
            handler_dispatched_at: Some(claimed_at),
        });

        let info = DatabaseWriter::queue_timing_info(
            &context,
            claimed_at + chrono::Duration::milliseconds(2),
        )
        .unwrap();
        assert!(info["queue_timing"]["queue_wait_us"].is_null());
        assert_eq!(
            info["queue_timing"]["retry_queue_age_us"],
            serde_json::json!(20_000_000)
        );
        assert_eq!(info["queue_timing"]["redelivered"], true);
    }

    #[tokio::test]
    async fn sqlite_backfill_uses_postgres_shallow_merge_semantics() {
        let db = crate::db::test_db().await;
        let DatabasePool::Sqlite(pool) = &db else {
            panic!("test_db should return SQLite");
        };
        let data = crate::db::insert_test_data(&db).await.unwrap();
        let env_id = data.env_id;
        let stream_id = data.stream_id;
        let run_id = data.run_id;
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM run WHERE run_id = ?")
                .bind(run_id)
                .fetch_one(pool)
                .await
                .unwrap(),
            1
        );
        sqlx::query(
            r#"UPDATE run
               SET info = '{"queue_timing":{"old":true},"nullable":null,"flag":true,"nested":{"keep":true}}'
               WHERE run_id = ?"#,
        )
        .bind(run_id)
        .execute(pool)
        .await
        .unwrap();
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM run WHERE run_id = ?")
                .bind(run_id)
                .fetch_one(pool)
                .await
                .unwrap(),
            1
        );

        let claimed_at = chrono::Utc::now();
        let mut context = ExecutionContext::minimal(run_id, stream_id, 4);
        context.env_id = Some(env_id);
        context.queue_timing = Some(crate::lang::event::QueueExecutionTiming {
            backend: "memory".to_string(),
            enqueued_at: None,
            claimed_at,
            queue_wait_us: 10,
            redelivered: false,
            handler_dispatched_at: Some(claimed_at),
        });
        DatabaseWriter::backfill_run_provenance(
            &db,
            &context,
            claimed_at + chrono::Duration::milliseconds(1),
        )
        .await
        .unwrap();
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM run WHERE run_id = ?")
                .bind(run_id)
                .fetch_one(pool)
                .await
                .unwrap(),
            1
        );

        let info: String = sqlx::query_scalar("SELECT info FROM run WHERE run_id = ?")
            .bind(run_id)
            .fetch_one(pool)
            .await
            .unwrap();
        let info: serde_json::Value = serde_json::from_str(&info).unwrap();
        assert_eq!(info["nullable"], serde_json::Value::Null);
        assert_eq!(info["flag"], true);
        assert_eq!(info["nested"]["keep"], true);
        assert!(info["queue_timing"].get("old").is_none());
        assert_eq!(info["queue_timing"]["backend"], "memory");
    }

    #[test]
    fn test_val_to_jsonb_string_uses_hot_data_repr() {
        let mut closure_env = ahash::AHashMap::new();
        closure_env.insert("value".to_string(), Val::Bool(false));

        let lazy_thunk = LambdaInfo {
            parameters: vec![],
            instructions: vec![
                crate::lang::bytecode::Instruction::LoadVar {
                    dest: 0,
                    var_name: 0,
                },
                crate::lang::bytecode::Instruction::LoadConst {
                    dest: 1,
                    constant: 0,
                },
                crate::lang::bytecode::Instruction::Return { value: 1 },
            ],
            register_count: 2,
            capture_vars: vec!["value".to_string()],
            closure_env,
            defining_namespace: "::hot::test".to_string(),
            is_lazy_param: true,
            used_registers: vec![],
            structural_hash_cache: Default::default(),
        };

        let json = val_to_jsonb_string(&Val::Box(Box::new(lazy_thunk)));

        assert!(!json.contains("\"$box\""));
        assert!(!json.contains("instructions"));
        assert!(!json.contains("register_count"));

        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["$type"], "::hot::type/Fn");
        assert_eq!(parsed["$val"]["lazy"], true);
        assert_eq!(parsed["$val"]["captures"]["value"], false);
    }
}

/// Database Writer - Dedicated sequential writer for reliable, ordered database operations
///
/// This module provides a production-grade database writer that:
/// - Never blocks the event emitter
/// - Guarantees ordering of all writes
/// - Handles both SQLite and PostgreSQL optimally
/// - Provides backpressure visibility
/// - Ensures graceful shutdown with full data persistence
use crate::db::DatabasePool;
use crate::lang::emitter::postgres_safety::{sanitize_json_for_jsonb, sanitize_text_for_postgres};
use crate::lang::event::ExecutionContext;
use crate::val::Val;
use ahash::AHashMap;
use std::sync::Arc;
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

/// Maximum number of pending writes before backpressure warning
const BACKPRESSURE_THRESHOLD: usize = 1000;

/// Batch size for PostgreSQL transaction batching
const POSTGRES_BATCH_SIZE: usize = 100;

/// Run info tuple: (run_type_id, build_id, event_id, env_id, retry_attempt, status_id)
type RunInfo = (i16, Option<Uuid>, Option<Uuid>, Uuid, i16, i16);

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

#[derive(Clone)]
struct DatabaseWriterShard {
    write_sender: mpsc::UnboundedSender<DatabaseWrite>,
    flush_sender: mpsc::UnboundedSender<oneshot::Sender<()>>,
    shutdown_sender: mpsc::UnboundedSender<oneshot::Sender<()>>,
}

/// Handle to the database writer - allows sending writes without blocking
#[derive(Clone)]
pub struct DatabaseWriter {
    shards: Arc<Vec<DatabaseWriterShard>>,
}

impl DatabaseWriter {
    /// Create a new database writer with the given database pool
    pub fn new(db: Arc<RwLock<Option<DatabasePool>>>) -> Self {
        let shard_count = Self::shard_count_for_db(&db);
        let mut shards = Vec::with_capacity(shard_count);

        for shard_idx in 0..shard_count {
            let (write_sender, mut write_receiver) = mpsc::unbounded_channel::<DatabaseWrite>();
            let (flush_sender, mut flush_receiver) =
                mpsc::unbounded_channel::<oneshot::Sender<()>>();
            let (shutdown_sender, mut shutdown_receiver) =
                mpsc::unbounded_channel::<oneshot::Sender<()>>();
            let db = Arc::clone(&db);

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
                                        matches!(w, DatabaseWrite::RunStart { .. } | DatabaseWrite::RunStop { .. } | DatabaseWrite::RunFail { .. } | DatabaseWrite::RunCancel { .. })
                                    });

                                    let should_flush = has_run_write
                                        || pending_writes.len() >= POSTGRES_BATCH_SIZE
                                        || write_receiver.is_empty();

                                    if should_flush {
                                        tracing::debug!(
                                            shard_idx,
                                            pending = pending_writes.len(),
                                            has_run_write,
                                            batch_size = pending_writes.len() >= POSTGRES_BATCH_SIZE,
                                            channel_empty = write_receiver.is_empty(),
                                            "DatabaseWriter shard flushing writes"
                                        );
                                        Self::process_batch(&db, &mut pending_writes).await;
                                    }
                                }
                                None => {
                                    tracing::debug!(
                                        shard_idx,
                                        pending = pending_writes.len(),
                                        "DatabaseWriter shard write channel closed"
                                    );
                                    if !pending_writes.is_empty() {
                                        Self::process_batch(&db, &mut pending_writes).await;
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
                                    Self::process_batch(&db, &mut pending_writes).await;
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
                                    Self::process_batch(&db, &mut pending_writes).await;
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
        }
    }

    /// Send a write operation to the database writer (never blocks)
    pub fn write(&self, write: DatabaseWrite) {
        let shard = self.shard_for_run(write.run_id());
        if shard.write_sender.send(write).is_err() {
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
        Ok(())
    }

    pub async fn flush_run(&self, run_id: Uuid) -> Result<(), String> {
        let shard = self.shard_for_run(run_id);
        let (ack_sender, ack_receiver) = oneshot::channel();

        if shard.flush_sender.send(ack_sender).is_err() {
            return Err("Failed to send flush request - writer task has shut down".to_string());
        }

        ack_receiver
            .await
            .map_err(|_| "Flush acknowledgment was dropped".to_string())
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
        Ok(())
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
        writes: &mut Vec<DatabaseWrite>,
    ) {
        if writes.is_empty() {
            return;
        }

        let db_guard = db.read().await;
        let pool = match db_guard.as_ref() {
            Some(p) => p,
            None => {
                tracing::warn!(
                    "DatabaseWriter: No database connection, dropping {} writes",
                    writes.len()
                );
                writes.clear();
                return;
            }
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

        // Process run writes first. If a short successful run has both
        // run:start and run:stop pending in this shard flush, insert it once
        // already terminal instead of doing insert+update.
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
                            tracing::error!(
                                "DatabaseWriter: Coalesced run write failed for {}: {}",
                                run_id,
                                e
                            );
                        }
                    } else if let Err(e) =
                        Self::write_run_stop(pool, run_id, event_time, &result).await
                    {
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
                        tracing::error!("DatabaseWriter: Critical run:start write failed: {}", e);
                    }
                    if let Err(e) = Self::write_run_fail(pool, run_id, event_time, &failure).await {
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
                        tracing::error!("DatabaseWriter: Critical run:start write failed: {}", e);
                    }
                    if let Err(e) =
                        Self::write_run_cancel(pool, run_id, event_time, &cancellation).await
                    {
                        tracing::error!("DatabaseWriter: Critical run write failed: {}", e);
                    }
                }
                DatabaseWrite::Call { .. } => unreachable!("call writes were partitioned out"),
            }
        }

        for (_, (execution_context, start_time)) in pending_starts {
            if let Err(e) = Self::write_run_start(pool, &execution_context, start_time).await {
                tracing::error!("DatabaseWriter: Critical run:start write failed: {}", e);
            }
        }

        // Then process other writes
        if other_writes.is_empty() {
            return;
        }

        match pool {
            DatabasePool::Sqlite(_) => {
                // SQLite: Process each write sequentially (no transaction batching)
                // SQLite has single-writer bottleneck, transactions don't help here
                for write in other_writes {
                    if let Err(e) = Self::execute_write(pool, &write).await {
                        tracing::error!("DatabaseWriter: SQLite write failed: {}", e);
                    }
                }
            }
            DatabasePool::Postgres(pg_pool) => {
                // PostgreSQL: Use transaction for batch atomicity and performance
                // Writes are already sorted (calls by depth, etc.) so we maintain order
                match pg_pool.begin().await {
                    Ok(mut tx) => {
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
                                    let size: i64 = args.as_deref().map_or(0, |s| s.len() as i64)
                                        + return_value.as_deref().map_or(0, |s| s.len() as i64)
                                        + flow.as_deref().map_or(0, |s| s.len() as i64)
                                        + 50;
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
INSERT INTO hot.call (call_id, run_id, parent_call_id, function_name, static_scope, runtime_path, call_depth, args, return_value, error, flow, file, line, \"column\", position, start_time, stop_time, duration_us, size)
VALUES ($1, $2, $3, $4, $5, $6, $7, $8::jsonb, $9::jsonb, $10, $11::jsonb, $12, $13, $14, $15, $16, $17, $18, $19)
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
           COALESCE(octet_length(COALESCE(EXCLUDED.flow, hot.call.flow)::text), 0) + 50
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
                                    .bind(size)
                                    .execute(&mut *tx)
                                    .await
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
                                tracing::error!(
                                    "DatabaseWriter: Write failed in transaction: {}",
                                    e
                                );
                                failed = true;
                                break; // Stop on first error
                            }
                        }

                        if failed {
                            // Rollback on failure
                            if let Err(e) = tx.rollback().await {
                                tracing::error!("DatabaseWriter: Rollback failed: {}", e);
                            }
                        } else {
                            // Commit on success
                            if let Err(e) = tx.commit().await {
                                tracing::error!("DatabaseWriter: Commit failed: {}", e);
                            }
                        }
                    }
                    Err(e) => {
                        tracing::error!("DatabaseWriter: Failed to begin transaction: {}", e);
                        // Fallback: Process writes individually without transaction
                        for write in other_writes {
                            if let Err(e) = Self::execute_write(pool, &write).await {
                                tracing::error!(
                                    "DatabaseWriter: Write failed (no transaction): {}",
                                    e
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    /// Execute a single write operation
    async fn execute_write(pool: &DatabasePool, write: &DatabaseWrite) -> Result<(), sqlx::Error> {
        match write {
            DatabaseWrite::RunStart {
                execution_context,
                event_time,
            } => Self::write_run_start(pool, execution_context, *event_time).await,

            DatabaseWrite::RunStop {
                run_id,
                event_time,
                result,
            } => Self::write_run_stop(pool, *run_id, *event_time, result).await,

            DatabaseWrite::RunFail {
                run_id,
                event_time,
                failure,
            } => Self::write_run_fail(pool, *run_id, *event_time, failure).await,

            DatabaseWrite::RunCancel {
                run_id,
                event_time,
                cancellation,
            } => Self::write_run_cancel(pool, *run_id, *event_time, cancellation).await,

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
                Self::write_call(
                    pool,
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
        }
    }

    /// Write a run:start record
    /// Includes retry logic for foreign key violations on origin_run_id, which can happen
    /// due to race conditions when a child run is processed before the parent run's
    /// run:start has been written to the database.
    ///
    /// Note: start_time uses event_time from the EngineEvent, which is captured at the moment
    /// EngineEvent::run_start() is called (when the run actually begins executing).
    /// This allows calculating queue wait time as: start_time - event.created_at
    async fn write_run_start(
        pool: &DatabasePool,
        ctx: &ExecutionContext,
        event_time: chrono::DateTime<chrono::Utc>,
    ) -> Result<(), sqlx::Error> {
        // Use event_time which is captured when EngineEvent::run_start() is created
        // (the actual moment the run starts executing), not when this DB write happens
        let start_time = event_time;
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
            let result: Result<(), sqlx::Error> = match pool {
                DatabasePool::Postgres(pg_pool) => {
                    sqlx::query(
                        "INSERT INTO run (run_id, env_id, stream_id, build_id, run_type_id, origin_run_id, event_id, start_time, status_id, by_user_id, retry_attempt, access_id, agent_type)
                         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)
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
                    .execute(pg_pool)
                    .await
                    .map(|_| ())
                }
                DatabasePool::Sqlite(sqlite_pool) => {
                    sqlx::query(
                        "INSERT OR IGNORE INTO run (run_id, env_id, stream_id, build_id, run_type_id, origin_run_id, event_id, start_time, status_id, by_user_id, retry_attempt, access_id, agent_type)
                         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"
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
                    .execute(sqlite_pool)
                    .await
                    .map(|_| ())
                }
            };

            match result {
                Ok(()) => {
                    if attempt > 0 {
                        tracing::debug!(
                            "DatabaseWriter: run:start for run_id={} succeeded on attempt {}",
                            ctx.run_id,
                            attempt + 1
                        );
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

        // Update stream metrics
        crate::db::stream::Stream::update_metrics(pool, &ctx.stream_id)
            .await
            .map_err(|e| {
                sqlx::Error::Protocol(format!("Failed to update stream metrics: {}", e))
            })?;

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
                    "INSERT INTO hot.run (run_id, env_id, stream_id, build_id, run_type_id, origin_run_id, event_id, start_time, stop_time, status_id, result, by_user_id, retry_attempt, access_id, agent_type)
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11::jsonb, $12, $13, $14, $15)
                     ON CONFLICT (run_id) DO UPDATE
                     SET stop_time = EXCLUDED.stop_time,
                         status_id = EXCLUDED.status_id,
                         result = EXCLUDED.result
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
                .execute(pg_pool)
                .await?
                .rows_affected()
            }
            DatabasePool::Sqlite(sqlite_pool) => {
                sqlx::query(
                    "INSERT OR IGNORE INTO run (run_id, env_id, stream_id, build_id, run_type_id, origin_run_id, event_id, start_time, stop_time, status_id, result, by_user_id, retry_attempt, access_id, agent_type)
                     VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"
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
                .execute(sqlite_pool)
                .await?
                .rows_affected()
            }
        };

        if rows_affected == 0 {
            Self::write_run_stop(pool, ctx.run_id, stop_time, result).await?;
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

        crate::db::stream::Stream::update_metrics(pool, &ctx.stream_id)
            .await
            .map_err(|e| {
                sqlx::Error::Protocol(format!("Failed to update stream metrics: {}", e))
            })?;

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
            Self::refresh_stream_metrics(pool, run_id).await;
        }
        Ok(())
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
            // Run not found - just mark as failed without retry
            tracing::warn!("Run {} not found when processing failure", run_id);
            Self::update_run_failed(pool, run_id, event_time, &result_json, None, None).await?;
            return Ok(());
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
            }
        }

        // The run transitioned to a terminal/pending_retry state above; keep the
        // stream's aggregate run metrics current.
        Self::refresh_stream_metrics(pool, run_id).await;
        Ok(())
    }

    /// Best-effort refresh of a stream's aggregate run metrics after a run leaves
    /// the `running` state. Metrics are derived data, so failures are logged but
    /// never abort the originating write.
    async fn refresh_stream_metrics(pool: &DatabasePool, run_id: Uuid) {
        let stream_id: Option<Uuid> = match pool {
            DatabasePool::Postgres(pg_pool) => {
                sqlx::query_scalar("SELECT stream_id FROM hot.run WHERE run_id = $1")
                    .bind(run_id)
                    .fetch_optional(pg_pool)
                    .await
                    .ok()
                    .flatten()
            }
            DatabasePool::Sqlite(sqlite_pool) => {
                sqlx::query_scalar("SELECT stream_id FROM run WHERE run_id = ?")
                    .bind(run_id)
                    .fetch_optional(sqlite_pool)
                    .await
                    .ok()
                    .flatten()
            }
        };

        if let Some(stream_id) = stream_id
            && let Err(e) = crate::db::stream::Stream::update_metrics(pool, &stream_id).await
        {
            tracing::warn!(
                run_id = %run_id,
                "Failed to refresh stream metrics after run completion: {}",
                e
            );
        }
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
            tracing::debug!(
                run_id = %run_id,
                "Ignoring late run:cancel for run that is no longer running"
            );
            return Ok(());
        }

        Self::refresh_stream_metrics(pool, run_id).await;

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

    /// Write a call record using UPSERT to handle both INSERT and UPDATE
    #[allow(clippy::too_many_arguments)]
    async fn write_call(
        pool: &DatabasePool,
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
    ) -> Result<(), sqlx::Error> {
        let size: i64 = args.map_or(0, |s| s.len() as i64)
            + return_value.map_or(0, |s| s.len() as i64)
            + flow.map_or(0, |s| s.len() as i64)
            + 50;

        // Defense in depth: scrub Postgres-rejected chars from `jsonb`
        // columns and the `text` `error` column before bind. Upstream
        // serialization sanitizes too; this catches anything from a future
        // caller that bypasses the upstream path.
        let args_safe = args.map(sanitize_json_for_jsonb);
        let return_value_safe = return_value.map(sanitize_json_for_jsonb);
        let error_safe = error.map(sanitize_text_for_postgres);
        let flow_safe = flow.map(sanitize_json_for_jsonb);

        match pool {
            DatabasePool::Postgres(pg_pool) => {
                // PostgreSQL UPSERT
                sqlx::query(
                    "INSERT INTO hot.call (call_id, run_id, parent_call_id, function_name, static_scope, runtime_path, call_depth, args, return_value, error, flow, start_time, stop_time, duration_us, file, line, \"column\", position, size)
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8::jsonb, $9::jsonb, $10, $11::jsonb, $12, $13, $14, $15, $16, $17, $18, $19)
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
                                COALESCE(octet_length(COALESCE(EXCLUDED.flow, hot.call.flow)::text), 0) + 50"
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
                .bind(line.map(|v| v as i32))
                .bind(column.map(|v| v as i32))
                .bind(position.map(|v| v as i32))
                .bind(size)
                .execute(pg_pool)
                .await?;
            }
            DatabasePool::Sqlite(sqlite_pool) => {
                // SQLite UPSERT
                sqlx::query(
                    "INSERT INTO call (call_id, run_id, parent_call_id, function_name, static_scope, runtime_path, call_depth, args, return_value, error, flow, start_time, stop_time, duration_us, file, line, \"column\", position, size)
                     VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
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
                         size = COALESCE(length(COALESCE(excluded.args, call.args)), 0) +
                                COALESCE(length(COALESCE(excluded.return_value, call.return_value)), 0) +
                                COALESCE(length(COALESCE(excluded.flow, call.flow)), 0) + 50"
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
                .bind(size)
                .execute(sqlite_pool)
                .await?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lang::bytecode::LambdaInfo;

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

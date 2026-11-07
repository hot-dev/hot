use super::database_writer::{DatabaseWrite, DatabaseWriter};
use super::postgres_safety::{sanitize_json_for_jsonb, sanitize_text_for_postgres};
use super::{EngineEvent, EngineEventEmitter};
use crate::blob::{BlobScope, BlobStore, MAX_INLINE_HARD_LIMIT_BYTES, SpillSource, omitted_val};
use crate::db::DatabasePool;
use crate::lang::event::ExecutionContext;
use crate::lang::masking::RecordMask;
use crate::val::{ShapeIdentity, Val};
use ahash::{AHashMap, AHashSet};
use indexmap::IndexMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::{RwLock, mpsc, oneshot};
use tokio::time::interval;
use uuid::Uuid;

/// DatabaseEngineEventEmitter that stores events in a database (SQLite or PostgreSQL)
///
/// This emitter uses a dedicated DatabaseWriter for all database operations, ensuring:
/// - No blocking on the event emission path
/// - Ordered writes for data consistency
/// - Optimal batching for both SQLite and PostgreSQL
/// - Graceful shutdown with full data persistence
pub struct DatabaseEngineEventEmitter {
    db: Arc<RwLock<Option<DatabasePool>>>,
    writer: DatabaseWriter,
    event_sender: mpsc::UnboundedSender<EngineEvent>,
    flush_sender: mpsc::UnboundedSender<ProcessorFlushRequest>,
    shutdown_sender: mpsc::UnboundedSender<oneshot::Sender<Result<(), String>>>,
    /// Flag to suppress errors during shutdown (when channel is expected to be closed)
    shutdown_initiated: Arc<AtomicBool>,
    /// How many calls a run records (see `with_max_calls_per_run`).
    max_calls_per_run: Arc<AtomicUsize>,
    call_budgets: CallBudgets,
}

/// Calls a run records by default. A heavily interpreted run makes hundreds
/// of thousands of calls; recording them all takes seconds per thousand and a
/// gigabyte of memory.
pub const DEFAULT_MAX_CALLS_PER_RUN: usize = 10_000;

/// How long a finished run's call budget stays, for calls that trail its end.
const FINISHED_CALL_BUDGET_TTL: Duration = Duration::from_secs(300);
/// How long an unfinished run's call budget stays without a new call: longer
/// than the longest run, so a long call's stop still finds its start.
const IDLE_CALL_BUDGET_TTL: Duration = Duration::from_secs(25 * 3_600);
const CALL_BUDGET_PRUNE_INTERVAL: Duration = Duration::from_secs(60);

/// Sent in place of the first call a run doesn't record.
const CALLS_TRUNCATED_EVENT: &str = "call:truncated";

/// Each run's calls so far. Decided as a call is emitted, not as it's processed:
/// the queue is unbounded, and an event waiting in it keeps its args alive. A
/// run that reads a table can make a million calls past its limit while the
/// processor is busy, and each one would hold its row until it's discarded.
type CallBudgets = Arc<std::sync::Mutex<AHashMap<Uuid, RunCallBudget>>>;

/// What happens to a call event.
#[derive(PartialEq, Debug)]
enum CallDecision {
    Record,
    Skip,
    /// Skip it, and it's the first the run skips.
    Truncate,
}

/// The calls a run has recorded, and those it no longer records.
#[derive(Default)]
struct RunCallBudget {
    recorded: usize,
    /// Calls recorded whose `call:stop` hasn't come yet.
    open: AHashSet<Uuid>,
    unrecorded: usize,
    last_seen: Option<Instant>,
    finished: bool,
}

/// Internal processor for handling database events with batching
struct DatabaseEngineEventEmitterProcessor {
    writer: DatabaseWriter,
    pending_calls: IndexMap<(Uuid, Uuid), CallEngineEventBatch>,
    /// Calls written before they stopped, as the run passed their args: a
    /// secret the call registers later must still mask its row.
    open_calls: AHashMap<(Uuid, Uuid), OpenCall>,
    /// Runs that ended: nothing more will stop their open calls.
    ended_runs: Vec<Uuid>,
    /// Runs with rows written, and how far those rows are masked.
    written_runs: AHashMap<Uuid, WrittenRun>,
    /// When present, large call/run payload leaves are spilled to blob storage
    /// (after secret masking) before JSON serialization.
    blob_store: Option<Arc<BlobStore>>,
    max_calls_per_run: Arc<AtomicUsize>,
    call_budgets: CallBudgets,
    last_budget_prune: Instant,
}

struct ProcessorFlushRequest {
    run_id: Option<Uuid>,
    completion: oneshot::Sender<Result<(), String>>,
}

/// A run's call rows hold nothing of what it registered after `generation`,
/// unless a later generation is on its way to the writer.
struct WrittenRun {
    execution_context: ExecutionContext,
    generation: u64,
    /// A run may register secrets in a loop, and each pass reads all its rows.
    remask_after: Instant,
    remask_interval: Duration,
    last_written: Instant,
}

/// A call whose row has args masked as of `generation` of its run's secrets.
struct OpenCall {
    batch: CallEngineEventBatch,
    generation: u64,
}

/// Represents a batched call event that can be start-only, stop-only, or combined
#[derive(Debug, Clone)]
struct CallEngineEventBatch {
    execution_context: ExecutionContext,
    call_id: Uuid,
    parent_call_id: Option<Uuid>,
    function_name: String,
    static_scope: String,
    runtime_path: String,
    call_depth: i64,
    args: Option<CallPayload>,
    return_value: Option<CallPayload>,
    error: Option<String>,
    flow: Option<CallPayload>,
    file: Option<String>,
    line: Option<i64>,
    column: Option<i64>,
    position: Option<i64>,
    start_time: Option<chrono::DateTime<chrono::Utc>>,
    stop_time: Option<chrono::DateTime<chrono::Utc>>,
    duration_us: Option<i64>,
}

/// A call's args, return value or flow, waiting to be masked and written.
#[derive(Debug, Clone)]
enum CallPayload {
    Json(String),
    /// A value with a large leaf stays a value. The run already holds it, so
    /// this costs nothing, where JSON would write Bytes out at four times their
    /// size on every call they pass through, and blob storage takes the leaf as
    /// it is.
    Large(Val),
}

/// The size of leaf that keeps a call's payload a [`CallPayload::Large`].
const LARGE_PAYLOAD_LEAF_BYTES: usize = 64 * 1024;

const BATCH_INTERVAL: Duration = Duration::from_millis(500);
const MAX_REMASK_INTERVAL: Duration = Duration::from_secs(30);

impl DatabaseEngineEventEmitter {
    /// Creates a new DatabaseEngineEventEmitter with an existing database pool (preferred)
    /// This ensures the database connection is ready before events are processed
    pub fn new_with_pool(db_pool: DatabasePool) -> Self {
        Self::new_with_pool_and_blob_store(db_pool, None)
    }

    /// Like [`Self::new_with_pool`], but with an optional blob store used to
    /// spill large call/run payloads to content-addressed storage before they
    /// are serialized into JSON/JSONB columns.
    pub fn new_with_pool_and_blob_store(
        db_pool: DatabasePool,
        blob_store: Option<Arc<BlobStore>>,
    ) -> Self {
        let db = Arc::new(RwLock::new(Some(db_pool)));

        // Create the dedicated database writer
        let writer = DatabaseWriter::new(Arc::clone(&db));

        // Create event processing channel
        let (event_sender, mut event_receiver) = mpsc::unbounded_channel::<EngineEvent>();
        let (flush_sender, mut flush_receiver) = mpsc::unbounded_channel::<ProcessorFlushRequest>();

        // Create shutdown channel
        let (shutdown_sender, mut shutdown_receiver) =
            mpsc::unbounded_channel::<oneshot::Sender<Result<(), String>>>();

        let writer_for_processor = writer.clone();
        let max_calls_per_run = Arc::new(AtomicUsize::new(DEFAULT_MAX_CALLS_PER_RUN));
        let max_calls_for_processor = Arc::clone(&max_calls_per_run);
        let call_budgets = CallBudgets::default();
        let budgets_for_processor = Arc::clone(&call_budgets);

        // Start event processing task
        tokio::spawn(async move {
            let mut processor = DatabaseEngineEventEmitterProcessor::new(
                writer_for_processor,
                blob_store,
                max_calls_for_processor,
                budgets_for_processor,
            );

            let mut flush_timer = interval(BATCH_INTERVAL);
            flush_timer.tick().await; // Skip first immediate tick

            loop {
                tokio::select! {
                    // Process incoming events
                    event = event_receiver.recv() => {
                        match event {
                            Some(evt) => processor.process_event(evt).await,
                            None => {
                                processor.flush_all().await;
                                if let Err(e) = processor.writer.shutdown().await {
                                    tracing::debug!("DatabaseEngineEventEmitter: Writer shutdown on channel close: {}", e);
                                }
                                break;
                            }
                        }
                    }
                    // Flush batched events every 500ms
                    _ = flush_timer.tick() => {
                        processor.flush_all().await;
                    }
                    // Handle explicit flush requests by draining queued events
                    // before waiting on the writer.
                    flush_request = flush_receiver.recv() => {
                        if let Some(request) = flush_request {
                            while let Ok(evt) = event_receiver.try_recv() {
                                processor.process_event(evt).await;
                            }
                            processor.flush_all().await;
                            let result = match request.run_id {
                                Some(run_id) => processor.writer.flush_run(run_id).await,
                                None => processor.writer.flush_async().await,
                            };
                            let _ = request.completion.send(result);
                        }
                    }
                    // Handle shutdown signal
                    completion_sender = shutdown_receiver.recv() => {
                        if let Some(sender) = completion_sender {
                            // Process any remaining events
                            while let Ok(evt) = event_receiver.try_recv() {
                                processor.process_event(evt).await;
                            }

                            // Final flush
                            processor.flush_all().await;

                            // Delivery failures remain observable at the public emitter
                            // boundary; a completed shutdown is not proof of persistence.
                            let result = processor.writer.shutdown().await;
                            let _ = sender.send(result);
                            break;
                        }
                    }
                }
            }
        });

        Self {
            db,
            writer,
            event_sender,
            flush_sender,
            shutdown_sender,
            shutdown_initiated: Arc::new(AtomicBool::new(false)),
            max_calls_per_run,
            call_budgets,
        }
    }

    /// Creates a new DatabaseEngineEventEmitter with the given database config
    /// Note: Database connection is initialized asynchronously. Use new_with_pool() for synchronous creation.
    pub fn new(db_conf: Val) -> Self {
        let db = Arc::new(RwLock::new(None));

        // Create the dedicated database writer
        let writer = DatabaseWriter::new(Arc::clone(&db));

        // Create event processing channel
        let (event_sender, mut event_receiver) = mpsc::unbounded_channel::<EngineEvent>();
        let (flush_sender, mut flush_receiver) = mpsc::unbounded_channel::<ProcessorFlushRequest>();

        // Create shutdown channel
        let (shutdown_sender, mut shutdown_receiver) =
            mpsc::unbounded_channel::<oneshot::Sender<Result<(), String>>>();

        let db_for_processor = Arc::clone(&db);
        let writer_for_processor = writer.clone();
        let max_calls_per_run = Arc::new(AtomicUsize::new(DEFAULT_MAX_CALLS_PER_RUN));
        let max_calls_for_processor = Arc::clone(&max_calls_per_run);
        let call_budgets = CallBudgets::default();
        let budgets_for_processor = Arc::clone(&call_budgets);

        // Start event processing task
        tokio::spawn(async move {
            // Initialize database connection
            match crate::db::create_db_pool(&db_conf).await {
                Ok(pool) => {
                    let mut db_guard = db_for_processor.write().await;
                    *db_guard = Some(pool);
                    tracing::debug!("DatabaseEngineEventEmitter: Database connection established");
                }
                Err(e) => {
                    tracing::error!(
                        "DatabaseEngineEventEmitter: Failed to connect to database: {}",
                        e
                    );
                    return;
                }
            }

            let mut processor = DatabaseEngineEventEmitterProcessor::new(
                writer_for_processor,
                None,
                max_calls_for_processor,
                budgets_for_processor,
            );

            let mut flush_timer = interval(BATCH_INTERVAL);
            flush_timer.tick().await; // Skip first immediate tick

            loop {
                tokio::select! {
                    // Process incoming events
                    event = event_receiver.recv() => {
                        match event {
                            Some(evt) => processor.process_event(evt).await,
                            None => {
                                processor.flush_all().await;
                                if let Err(e) = processor.writer.shutdown().await {
                                    tracing::debug!("DatabaseEngineEventEmitter: Writer shutdown on channel close: {}", e);
                                }
                                break;
                            }
                        }
                    }
                    // Flush batched events every 500ms
                    _ = flush_timer.tick() => {
                        processor.flush_all().await;
                    }
                    // Handle explicit flush requests by draining queued events
                    // before waiting on the writer.
                    flush_request = flush_receiver.recv() => {
                        if let Some(request) = flush_request {
                            while let Ok(evt) = event_receiver.try_recv() {
                                processor.process_event(evt).await;
                            }
                            processor.flush_all().await;
                            let result = match request.run_id {
                                Some(run_id) => processor.writer.flush_run(run_id).await,
                                None => processor.writer.flush_async().await,
                            };
                            let _ = request.completion.send(result);
                        }
                    }
                    // Handle shutdown signal
                    completion_sender = shutdown_receiver.recv() => {
                        if let Some(sender) = completion_sender {
                            // Process any remaining events
                            while let Ok(evt) = event_receiver.try_recv() {
                                processor.process_event(evt).await;
                            }

                            // Final flush
                            processor.flush_all().await;

                            // Delivery failures remain observable at the public emitter
                            // boundary; a completed shutdown is not proof of persistence.
                            let result = processor.writer.shutdown().await;
                            let _ = sender.send(result);
                            break;
                        }
                    }
                }
            }

            tracing::info!("DatabaseEngineEventEmitter: Shutdown complete");
        });

        Self {
            db,
            writer,
            event_sender,
            flush_sender,
            shutdown_sender,
            shutdown_initiated: Arc::new(AtomicBool::new(false)),
            max_calls_per_run,
            call_budgets,
        }
    }

    /// Record at most `max` calls per run (0: all of them). A run that makes
    /// more records its first `max`, and its info says that it stopped
    /// recording calls (`call_tracing: "truncated"`) and, once it ends, how
    /// many it didn't record (`untraced_calls`). The cap also bounds how many
    /// call records a run can queue for the database.
    pub fn with_max_calls_per_run(self, max: usize) -> Self {
        self.max_calls_per_run.store(max, Ordering::Relaxed);
        self
    }

    async fn flush_processor(&self, run_id: Option<Uuid>) -> Result<(), String> {
        let (completion, receiver) = oneshot::channel();
        let request = ProcessorFlushRequest { run_id, completion };

        self.flush_sender
            .send(request)
            .map_err(|_| "Failed to send flush request - processor has shut down".to_string())?;

        receiver
            .await
            .map_err(|_| "Flush acknowledgment was dropped".to_string())?
    }

    /// Gracefully shutdown the database emitter and wait for all events to be processed
    pub async fn shutdown_impl(&self) -> Result<(), String> {
        // Set shutdown flag FIRST to suppress "channel closed" errors from stragglers
        self.shutdown_initiated.store(true, Ordering::SeqCst);

        let (completion_sender, completion_receiver) = oneshot::channel();

        if self.shutdown_sender.send(completion_sender).is_err() {
            return Err(
                "Failed to send shutdown signal - processor may have already finished".to_string(),
            );
        }

        completion_receiver
            .await
            .map_err(|_| "Shutdown completion signal was dropped".to_string())?
    }
}

impl DatabaseEngineEventEmitterProcessor {
    fn new(
        writer: DatabaseWriter,
        blob_store: Option<Arc<BlobStore>>,
        max_calls_per_run: Arc<AtomicUsize>,
        call_budgets: CallBudgets,
    ) -> Self {
        Self {
            writer,
            pending_calls: IndexMap::new(),
            open_calls: AHashMap::new(),
            ended_runs: Vec::new(),
            written_runs: AHashMap::new(),
            blob_store,
            max_calls_per_run,
            call_budgets,
            last_budget_prune: Instant::now(),
        }
    }

    /// A run has made its first call past its limit.
    fn record_truncation(&mut self, event: &EngineEvent) {
        let run_id = event.execution_context.run_id;
        let max = self.max_calls_per_run.load(Ordering::Relaxed);
        tracing::warn!(
            %run_id,
            max_calls = max,
            "Run made more calls than it records; recording no more"
        );
        self.writer.write(DatabaseWrite::RunInfo {
            run_id,
            info: serde_json::json!({
                "call_tracing": "truncated",
                "traced_calls_limit": max,
            }),
        });
    }

    /// Once a run ends, record how many of its calls it didn't.
    fn finish_call_budget(&mut self, run_id: Uuid) {
        self.ended_runs.push(run_id);
        let mut budgets = lock_call_budgets(&self.call_budgets);
        let Some(budget) = budgets.get_mut(&run_id) else {
            return;
        };
        if !budget.finished && budget.unrecorded > 0 {
            self.writer.write(DatabaseWrite::RunInfo {
                run_id,
                info: serde_json::json!({ "untraced_calls": budget.unrecorded }),
            });
        }
        budget.finished = true;
        budget.open = AHashSet::new();
        budget.last_seen = Some(Instant::now());
    }

    fn prune_call_budgets(&mut self) {
        if self.last_budget_prune.elapsed() < CALL_BUDGET_PRUNE_INTERVAL {
            return;
        }
        self.last_budget_prune = Instant::now();
        self.written_runs
            .retain(|_, run| run.last_written.elapsed() < IDLE_CALL_BUDGET_TTL);
        lock_call_budgets(&self.call_budgets).retain(|_, budget| {
            let ttl = if budget.finished {
                FINISHED_CALL_BUDGET_TTL
            } else {
                IDLE_CALL_BUDGET_TTL
            };
            budget.last_seen.is_some_and(|seen| seen.elapsed() < ttl)
        });
    }

    /// Process a single event - sends critical events immediately, batches others
    async fn process_event(&mut self, event: EngineEvent) {
        if event.event_type == CALLS_TRUNCATED_EVENT {
            self.record_truncation(&event);
            return;
        }
        if matches!(
            event.event_type.as_str(),
            "run:stop" | "run:fail" | "run:cancel"
        ) {
            // After the run's own write, which the match below sends.
            let run_id = event.execution_context.run_id;
            self.process_run_event(event).await;
            self.finish_call_budget(run_id);
            return;
        }
        self.process_run_event(event).await;
    }

    async fn process_run_event(&mut self, event: EngineEvent) {
        match event.event_type.as_str() {
            "run:start" => {
                // Critical - send immediately to writer
                tracing::debug!(
                    "DatabaseEngineEventEmitter: Processing run:start for run_id {}",
                    event.execution_context.run_id
                );
                self.writer.write(DatabaseWrite::RunStart {
                    execution_context: event.execution_context.clone(),
                    event_time: event.event_time,
                });
            }
            "run:stop" => {
                // Critical - send immediately to writer
                let result = if let Some(r) = event.event_data.get("result") {
                    // Mask any secret values in the result before storing
                    Self::mask_run_val(&r, &event.execution_context)
                } else {
                    Val::Null
                };
                // Spill AFTER masking so secret bytes never reach blob storage
                let result = self
                    .maybe_spill_run_val(result, &event.execution_context, SpillSource::RunResult)
                    .await;
                self.writer.write(DatabaseWrite::RunStop {
                    run_id: event.execution_context.run_id,
                    event_time: event.event_time,
                    result,
                });
            }
            "run:fail" => {
                // Critical - send immediately to writer
                let failure = if let Some(f) = event.event_data.get("failure") {
                    // Mask any secret values in the failure before storing
                    Self::mask_run_val(&f, &event.execution_context)
                } else {
                    Val::Null
                };
                let failure = self
                    .maybe_spill_run_val(failure, &event.execution_context, SpillSource::RunFailure)
                    .await;
                self.writer.write(DatabaseWrite::RunFail {
                    run_id: event.execution_context.run_id,
                    event_time: event.event_time,
                    failure,
                });
            }
            "run:cancel" => {
                // Critical - send immediately to writer
                let cancellation = if let Some(c) = event.event_data.get("cancellation") {
                    // Mask any secret values in the cancellation before storing
                    Self::mask_run_val(&c, &event.execution_context)
                } else {
                    Val::Null
                };
                let cancellation = self
                    .maybe_spill_run_val(
                        cancellation,
                        &event.execution_context,
                        SpillSource::RunFailure,
                    )
                    .await;
                self.writer.write(DatabaseWrite::RunCancel {
                    run_id: event.execution_context.run_id,
                    event_time: event.event_time,
                    cancellation,
                });
            }
            "call:start" => {
                self.handle_call_start_batch(&event);
            }
            "call:stop" => {
                self.handle_call_stop_batch(&event);
            }
            // Note: "stream:data" events are no longer persisted to the database.
            // They are delivered in real-time via Redis Streams and are ephemeral.
            _ => {} // Ignore unknown event types
        }
    }

    /// Store an already-masked run value's large parts in blob storage: its
    /// large leaves, or all of it if that wouldn't leave little enough inline.
    /// Observability path: on spill failure, fall back to the inline value,
    /// unless it's too large to store at all.
    async fn maybe_spill_run_val(
        &self,
        val: Val,
        ctx: &ExecutionContext,
        source: SpillSource,
    ) -> Val {
        let size = crate::blob::estimate_val_size(&val);
        let inline = |val| {
            if size > MAX_INLINE_HARD_LIMIT_BYTES {
                omitted_val(size)
            } else {
                val
            }
        };
        let Some(store) = self.blob_store.as_ref().filter(|s| s.config().enabled()) else {
            return inline(val);
        };
        let threshold = store.config().spill_threshold_bytes;
        if size < threshold {
            return inline(val);
        }
        let Some(org_id) = ctx.org_id else {
            return inline(val);
        };
        let scope = BlobScope {
            org_id,
            env_id: ctx.env_id,
            run_id: Some(ctx.run_id),
        };
        let source_id = ctx.run_id.to_string();
        let spilled = if crate::blob::val_bytes_outside_large_leaves(&val, threshold)
            > store.config().max_inline_bytes
        {
            let json = serde_json::to_string(&val).unwrap_or_default();
            store
                .spill_whole_json(
                    &sanitize_json_for_jsonb(&json),
                    scope,
                    source,
                    Some(&source_id),
                )
                .await
        } else {
            store
                .spill_large_val(val.clone(), scope, source, Some(&source_id))
                .await
        };
        match spilled {
            Ok(spilled) => spilled,
            Err(e) => {
                tracing::warn!(
                    run_id = %ctx.run_id,
                    source = source.as_str(),
                    error = %e,
                    "blob spill failed for run value; storing inline"
                );
                inline(val)
            }
        }
    }

    /// Mask a call payload, then store its large parts in blob storage: its
    /// large leaves, or all of it if that wouldn't leave little enough inline.
    /// Masking MUST precede spill so secret bytes never reach blob storage.
    /// Observability path: on spill failure, fall back to the masked inline
    /// JSON, unless it's too large to store at all.
    async fn finalize_call_payload(
        blob_store: &Option<Arc<BlobStore>>,
        payload: Option<CallPayload>,
        ctx: &ExecutionContext,
        source: SpillSource,
        source_id: &str,
    ) -> Option<String> {
        match payload? {
            CallPayload::Json(json) => {
                let masked = Self::mask_call_payload(Some(json), ctx)?;
                Self::finalize_masked_json(blob_store, masked, ctx, source, source_id).await
            }
            CallPayload::Large(value) => {
                Self::finalize_large_payload(blob_store, value, ctx, source, source_id).await
            }
        }
    }

    /// As [`Self::finalize_call_payload`], without writing a value's large
    /// leaves out as JSON, or reading them back to mask them.
    async fn finalize_large_payload(
        blob_store: &Option<Arc<BlobStore>>,
        value: Val,
        ctx: &ExecutionContext,
        source: SpillSource,
        source_id: &str,
    ) -> Option<String> {
        let masked = RecordMask::new(&ctx.secret_values, &ctx.secret_value_hashes).mask_val(value);
        if let Some(store) = blob_store.as_ref().filter(|s| s.config().enabled())
            && let Some(org_id) = ctx.org_id
            && crate::blob::val_bytes_outside_large_leaves(
                &masked,
                store.config().spill_threshold_bytes,
            ) <= store.config().max_inline_bytes
        {
            let scope = BlobScope {
                org_id,
                env_id: ctx.env_id,
                run_id: Some(ctx.run_id),
            };
            match store
                .spill_large_val(masked.clone(), scope, source, Some(source_id))
                .await
            {
                Ok(spilled) => {
                    if let Ok(json) = serde_json::to_string(&spilled) {
                        return Some(Self::inline_call_payload(
                            sanitize_json_for_jsonb(&json).into_owned(),
                        ));
                    }
                }
                Err(e) => tracing::warn!(
                    source = source.as_str(),
                    source_id,
                    error = %e,
                    "blob spill failed for call payload; storing inline"
                ),
            }
        }
        let json = serde_json::to_string(&masked).unwrap_or_default();
        let json = sanitize_json_for_jsonb(&json).into_owned();
        Self::finalize_masked_json(blob_store, json, ctx, source, source_id).await
    }

    async fn finalize_masked_json(
        blob_store: &Option<Arc<BlobStore>>,
        masked: String,
        ctx: &ExecutionContext,
        source: SpillSource,
        source_id: &str,
    ) -> Option<String> {
        let Some(store) = blob_store.as_ref().filter(|s| s.config().enabled()) else {
            return Some(Self::inline_call_payload(masked));
        };
        let threshold = store.config().spill_threshold_bytes;
        if masked.len() < threshold {
            return Some(Self::inline_call_payload(masked));
        }
        let Some(org_id) = ctx.org_id else {
            return Some(Self::inline_call_payload(masked));
        };
        let scope = BlobScope {
            org_id,
            env_id: ctx.env_id,
            run_id: Some(ctx.run_id),
        };
        let spilled = if crate::blob::json_bytes_outside_large_strings(&masked, threshold)
            > store.config().max_inline_bytes
        {
            store
                .spill_whole_json(&masked, scope, source, Some(source_id))
                .await
        } else {
            match serde_json::from_str::<Val>(&masked) {
                Ok(val) => {
                    store
                        .spill_large_val(val, scope, source, Some(source_id))
                        .await
                }
                Err(_) => return Some(Self::inline_call_payload(masked)),
            }
        };
        match spilled.map(|spilled| serde_json::to_string(&spilled)) {
            Ok(Ok(json)) => Some(Self::inline_call_payload(
                sanitize_json_for_jsonb(&json).into_owned(),
            )),
            Ok(Err(_)) => Some(Self::inline_call_payload(masked)),
            Err(e) => {
                tracing::warn!(
                    source = source.as_str(),
                    source_id,
                    error = %e,
                    "blob spill failed for call payload; storing inline"
                );
                Some(Self::inline_call_payload(masked))
            }
        }
    }

    /// `json`, unless it's too large to store, which would fail the run's writes.
    fn inline_call_payload(json: String) -> String {
        if json.len() > MAX_INLINE_HARD_LIMIT_BYTES {
            serde_json::to_string(&omitted_val(json.len())).unwrap_or_default()
        } else {
            json
        }
    }

    /// Flush all pending batched events
    async fn flush_all(&mut self) {
        self.flush_pending_calls().await;
        self.prune_call_budgets();
    }

    /// Put back the calls whose rows were masked before their run registered
    /// a secret, as their args were passed, to be masked and written again.
    /// Writing `args` and `flow` again replaces them: see the writer's UPSERT.
    fn reopen_stale_calls(&mut self) {
        let mut done = Vec::new();
        for (key, open) in &self.open_calls {
            let pending = self.pending_calls.get_mut(key);
            let stopped = pending.as_ref().is_some_and(|b| b.stop_time.is_some());
            let stale = open.batch.execution_context.secret_values.generation() != open.generation;
            if stale {
                match pending {
                    Some(batch) => {
                        batch.args = open.batch.args.clone();
                        batch.flow = open.batch.flow.clone();
                    }
                    None => {
                        self.pending_calls.insert(*key, open.batch.clone());
                    }
                }
            }
            if stopped {
                done.push(*key);
            }
        }
        for key in done {
            self.open_calls.remove(&key);
        }
    }

    /// Flush all pending call events
    /// Uses UPSERT to handle INSERT (on start) and UPDATE (on stop) seamlessly
    /// Sorts by call_depth to ensure parent calls are written before children
    ///
    /// Spill happens here (not at batch-insert time) on purpose: a call's
    /// secrets are only complete once it stops, and masking must precede spill
    /// so secret bytes never reach blob storage. Payloads therefore sit inline
    /// in `pending_calls` for at most one BATCH_INTERVAL before being spilled.
    ///
    /// Masking reads the run's registry now, not a snapshot from the event, so
    /// every record in this flush sees every secret registered so far.
    async fn flush_pending_calls(&mut self) {
        self.reopen_stale_calls();
        let pending = std::mem::take(&mut self.pending_calls);

        // CRITICAL: Sort calls by call_depth to ensure parent calls are written before children
        // This prevents foreign key violations on parent_call_id
        let mut sorted_batches: Vec<_> = pending.into_iter().map(|(_, batch)| batch).collect();
        sorted_batches.sort_by_key(|batch| batch.call_depth);

        for batch in sorted_batches {
            let call_id_str = batch.call_id.to_string();

            // Read before masking, like the generation below.
            self.written_runs
                .entry(batch.execution_context.run_id)
                .or_insert_with(|| WrittenRun {
                    execution_context: batch.execution_context.clone(),
                    generation: batch.execution_context.secret_values.generation(),
                    remask_after: Instant::now(),
                    remask_interval: BATCH_INTERVAL,
                    last_written: Instant::now(),
                })
                .last_written = Instant::now();

            // Read before masking: a secret registered during this write makes
            // the row stale, and writing it again is harmless.
            if batch.stop_time.is_none() {
                self.open_calls.insert(
                    (batch.execution_context.run_id, batch.call_id),
                    OpenCall {
                        batch: batch.clone(),
                        generation: batch.execution_context.secret_values.generation(),
                    },
                );
            }

            // Mask secrets, then spill large leaves (mask-before-spill)
            let args = Self::finalize_call_payload(
                &self.blob_store,
                batch.args,
                &batch.execution_context,
                SpillSource::CallArgs,
                &call_id_str,
            )
            .await;

            let return_value = Self::finalize_call_payload(
                &self.blob_store,
                batch.return_value,
                &batch.execution_context,
                SpillSource::CallReturn,
                &call_id_str,
            )
            .await;

            let flow = Self::finalize_call_payload(
                &self.blob_store,
                batch.flow,
                &batch.execution_context,
                SpillSource::CallFlow,
                &call_id_str,
            )
            .await;

            let error = batch.error.map(|error| {
                let mask = RecordMask::new(
                    &batch.execution_context.secret_values,
                    &batch.execution_context.secret_value_hashes,
                );
                let masked = mask.mask_text(&error).into_owned();
                sanitize_text_for_postgres(&masked).into_owned()
            });

            self.writer.write(DatabaseWrite::Call {
                execution_context: Box::new(batch.execution_context),
                call_id: batch.call_id,
                parent_call_id: batch.parent_call_id,
                function_name: batch.function_name,
                static_scope: batch.static_scope,
                runtime_path: batch.runtime_path,
                call_depth: batch.call_depth,
                args,
                return_value,
                error,
                flow,
                file: batch.file,
                line: batch.line,
                column: batch.column,
                position: batch.position,
                start_time: batch.start_time,
                stop_time: batch.stop_time,
                duration_us: batch.duration_us,
            });
        }

        let ended = std::mem::take(&mut self.ended_runs);
        self.remask_written_runs(&ended);

        // After the writes above, for calls of a run that ended without their
        // stop: they had this last chance, and hold what the run did.
        if !ended.is_empty() {
            self.open_calls
                .retain(|(run_id, _), _| !ended.contains(run_id));
        }
    }

    /// Have the rows of runs that registered a secret since they were written
    /// masked again, after the writes already queued. A run that ended doesn't
    /// wait its turn, as this is its last chance.
    fn remask_written_runs(&mut self, ended: &[Uuid]) {
        let now = Instant::now();
        for (run_id, run) in &mut self.written_runs {
            let generation = run.execution_context.secret_values.generation();
            if generation != run.generation && (run.remask_after <= now || ended.contains(run_id)) {
                run.generation = generation;
                run.remask_after = now + run.remask_interval;
                run.remask_interval = (run.remask_interval * 2).min(MAX_REMASK_INTERVAL);
                self.writer.write(DatabaseWrite::RemaskCalls {
                    execution_context: Box::new(run.execution_context.clone()),
                });
            }
        }
        self.written_runs
            .retain(|run_id, _| !ended.contains(run_id));
    }

    /// A value as a call payload, in the form it's recorded. Its JSON is for
    /// a Postgres `jsonb` column, so it's scrubbed of characters Postgres
    /// rejects (`\u0000`, lone UTF-16 surrogates) but JSON otherwise allows.
    /// Without this, a single bad byte in a container/webhook payload would
    /// 22P05 the whole transaction batch and we'd silently drop a window of
    /// trace data. See [`super::postgres_safety`] for the full rationale.
    #[cfg(test)]
    fn call_payload(v: &Val) -> CallPayload {
        Self::prepared_call_payload(&v.to_record_repr())
    }

    fn prepared_call_payload(v: &Val) -> CallPayload {
        let storage_val = v.clone();
        if crate::blob::count_large_leaves(&storage_val, LARGE_PAYLOAD_LEAF_BYTES) > 0 {
            return CallPayload::Large(storage_val);
        }
        let raw = serde_json::to_string(&storage_val).unwrap_or_default();
        CallPayload::Json(sanitize_json_for_jsonb(&raw).into_owned())
    }

    /// Mask a serialized call payload. See [`crate::lang::masking`].
    fn mask_call_payload(json_value: Option<String>, ctx: &ExecutionContext) -> Option<String> {
        let json_str = json_value.as_deref()?;
        if json_str == "null" {
            return json_value;
        }
        let mask = RecordMask::new(&ctx.secret_values, &ctx.secret_value_hashes);
        // Re-serialized, so run through the Postgres-jsonb scrubber again:
        // nothing NUL or lone-surrogate may survive from the mask pass or from
        // an upstream path that didn't sanitize.
        match mask.mask_json(json_str) {
            Some(masked) => Some(sanitize_json_for_jsonb(&masked).into_owned()),
            None => json_value,
        }
    }

    /// Mask a value recorded on a run. Masks its storage form, so that values
    /// inside a closure's captures, say, aren't missed.
    fn mask_run_val(val: &Val, ctx: &ExecutionContext) -> Val {
        RecordMask::new(&ctx.secret_values, &ctx.secret_value_hashes).mask_val(val.clone())
    }

    fn handle_call_start_batch(&mut self, event: &EngineEvent) {
        if let Val::Map(data) = &event.event_data {
            let run_id = event.execution_context.run_id;
            let call_id = data.get(&Val::from("call_id")).and_then(|v| {
                if let Val::Str(s) = v {
                    Some(Uuid::parse_str(s).unwrap_or_default())
                } else {
                    None
                }
            });

            if let Some(call_id) = call_id {
                let parent_call_id = data.get(&Val::from("parent_call_id")).and_then(|v| {
                    if let Val::Str(s) = v {
                        Uuid::parse_str(s).ok()
                    } else {
                        None
                    }
                });

                let function_name = data
                    .get(&Val::from("function_name"))
                    .and_then(|v| {
                        if let Val::Str(s) = v {
                            Some((**s).to_owned())
                        } else {
                            None
                        }
                    })
                    .unwrap_or_default();

                let static_scope = data
                    .get(&Val::from("static_scope"))
                    .and_then(|v| {
                        if let Val::Str(s) = v {
                            Some((**s).to_owned())
                        } else {
                            None
                        }
                    })
                    .unwrap_or_default();

                let runtime_path = data
                    .get(&Val::from("runtime_path"))
                    .and_then(|v| {
                        if let Val::Str(s) = v {
                            Some((**s).to_owned())
                        } else {
                            None
                        }
                    })
                    .unwrap_or_default();

                let call_depth = data
                    .get(&Val::from("call_depth"))
                    .and_then(|v| if let Val::Int(i) = v { Some(*i) } else { None })
                    .unwrap_or(0);

                let args = data.get(&Val::from("args")).and_then(|v| {
                    if matches!(v, Val::Null) {
                        None
                    } else {
                        Some(Self::prepared_call_payload(v))
                    }
                });

                let file = data.get(&Val::from("file")).and_then(|v| {
                    if let Val::Str(s) = v {
                        Some((**s).to_owned())
                    } else {
                        None
                    }
                });

                let line = data
                    .get(&Val::from("line"))
                    .and_then(|v| if let Val::Int(i) = v { Some(*i) } else { None });
                let column = data
                    .get(&Val::from("column"))
                    .and_then(|v| if let Val::Int(i) = v { Some(*i) } else { None });
                let position = data
                    .get(&Val::from("position"))
                    .and_then(|v| if let Val::Int(i) = v { Some(*i) } else { None });

                // Extract flow context if present (for parallel, cond, cond_all, pipe flows)
                let flow = data.get(&Val::from("flow")).and_then(|v| {
                    if matches!(v, Val::Null) {
                        None
                    } else {
                        Some(Self::prepared_call_payload(v))
                    }
                });

                let key = (run_id, call_id);

                let batch = self
                    .pending_calls
                    .entry(key)
                    .or_insert_with(|| CallEngineEventBatch {
                        execution_context: event.execution_context.clone(),
                        call_id,
                        parent_call_id,
                        function_name: function_name.clone(),
                        static_scope: static_scope.clone(),
                        runtime_path: runtime_path.clone(),
                        call_depth,
                        args: None,
                        return_value: None,
                        error: None,
                        flow: None,
                        file: None,
                        line: None,
                        column: None,
                        position: None,
                        start_time: None,
                        stop_time: None,
                        duration_us: None,
                    });

                // Update with start event data
                batch.function_name = function_name;
                batch.static_scope = static_scope;
                batch.runtime_path = runtime_path;
                batch.call_depth = call_depth;
                batch.args = args;
                batch.flow = flow;
                batch.file = file;
                batch.line = line;
                batch.column = column;
                batch.position = position;
                batch.start_time = Some(event.event_time);
            }
        }
    }

    fn handle_call_stop_batch(&mut self, event: &EngineEvent) {
        if let Val::Map(data) = &event.event_data {
            let run_id = event.execution_context.run_id;
            let call_id = data.get(&Val::from("call_id")).and_then(|v| {
                if let Val::Str(s) = v {
                    Some(Uuid::parse_str(s).unwrap_or_default())
                } else {
                    None
                }
            });

            if let Some(call_id) = call_id {
                let return_value = data.get(&Val::from("return_value")).and_then(|v| {
                    if matches!(v, Val::Null) {
                        None
                    } else {
                        Some(Self::prepared_call_payload(v))
                    }
                });

                // `error` is bound as plain `text` (not jsonb), but Postgres
                // text columns also reject raw NUL bytes. Sanitize so a
                // process exit string with embedded `\0` doesn't tank the
                // batch with a "invalid byte sequence" error.
                let error = data.get(&Val::from("error")).and_then(|v| {
                    if let Val::Str(s) = v {
                        Some(sanitize_text_for_postgres(s.as_ref()).into_owned())
                    } else {
                        None
                    }
                });

                let duration_us = data
                    .get(&Val::from("duration_us"))
                    .and_then(|v| if let Val::Int(i) = v { Some(*i) } else { None });

                let key = (run_id, call_id);

                let batch = self
                    .pending_calls
                    .entry(key)
                    .or_insert_with(|| CallEngineEventBatch {
                        execution_context: event.execution_context.clone(),
                        call_id,
                        parent_call_id: None,
                        function_name: "unknown".to_string(),
                        static_scope: "unknown".to_string(),
                        runtime_path: "unknown".to_string(),
                        call_depth: 0,
                        args: None,
                        return_value: None,
                        error: None,
                        flow: None,
                        file: None,
                        line: None,
                        column: None,
                        position: None,
                        // Use event time as fallback start_time to avoid NULL constraint violations
                        // This can happen if call:stop arrives without a corresponding call:start
                        start_time: Some(event.event_time),
                        stop_time: None,
                        duration_us: None,
                    });

                // Update with stop event data
                batch.stop_time = Some(event.event_time);
                batch.return_value = return_value;
                batch.error = error;
                batch.duration_us = duration_us;
            }
        }
    }
}

fn lock_call_budgets(
    budgets: &CallBudgets,
) -> std::sync::MutexGuard<'_, AHashMap<Uuid, RunCallBudget>> {
    // A budget is a few counters: one a panic left half-updated still serves.
    budgets
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl DatabaseEngineEventEmitter {
    /// Whether a call event is recorded: a run records its first calls up
    /// to the cap, and a call's stop only with its start.
    fn decide_call(&self, event: &EngineEvent) -> CallDecision {
        let max = self.max_calls_per_run.load(Ordering::Relaxed);
        if max == 0 {
            return CallDecision::Record;
        }
        let Val::Map(data) = &event.event_data else {
            return CallDecision::Record;
        };
        let Some(Val::Str(call_id)) = data.get(&Val::from("call_id")) else {
            return CallDecision::Record;
        };
        let Ok(call_id) = Uuid::parse_str(call_id) else {
            return CallDecision::Record;
        };
        let run_id = event.execution_context.run_id;
        let mut budgets = lock_call_budgets(&self.call_budgets);
        if event.event_type == "call:stop" {
            // A stop without a recorded start is recorded as before, unless
            // the run's unrecorded starts may include its own.
            return match budgets.get_mut(&run_id) {
                Some(budget) => {
                    if budget.open.remove(&call_id) || budget.unrecorded == 0 {
                        CallDecision::Record
                    } else {
                        CallDecision::Skip
                    }
                }
                None => CallDecision::Record,
            };
        }
        let budget = budgets.entry(run_id).or_default();
        budget.last_seen = Some(Instant::now());
        if budget.recorded < max {
            budget.recorded += 1;
            budget.open.insert(call_id);
            return CallDecision::Record;
        }
        budget.unrecorded += 1;
        if budget.unrecorded == 1 {
            CallDecision::Truncate
        } else {
            CallDecision::Skip
        }
    }
}

// Each VM thread keeps a small cache for its current run. Weak identities
// prevent pointer reuse without retaining discarded inputs; COW invalidates them.
// Shape decisions and data conversion happen before entering the shared queue.
type RecordCache = (Uuid, Vec<(ShapeIdentity, Val)>, usize);
thread_local! {
    static RECORD_CACHE: std::cell::RefCell<RecordCache> =
        const { std::cell::RefCell::new((Uuid::nil(), Vec::new(), 0)) };
}
fn prepare_record_value(run: Uuid, value: &Val) -> Val {
    let Some(identity) = ShapeIdentity::of(value) else {
        return value.to_record_repr();
    };
    RECORD_CACHE.with(|slot| {
        let mut cache = slot.borrow_mut();
        if cache.0 != run {
            *cache = (run, Vec::new(), 0);
        }
        if let Some((_, prepared)) = cache.1.iter().find(|(identity, _)| identity.matches(value)) {
            return prepared.clone();
        }
        let prepared = value.to_record_repr();
        let bytes = crate::blob::estimate_val_size(&prepared);
        if cache.1.len() < 64 && bytes <= (4 * 1024 * 1024usize).saturating_sub(cache.2) {
            cache.2 += bytes;
            cache.1.push((identity, prepared.clone()));
        }
        prepared
    })
}
fn prepare_record_event(mut event: EngineEvent) -> EngineEvent {
    let fields: &[&str] = match event.event_type.as_str() {
        "call:start" => &["args", "flow"],
        "call:stop" => &["return_value", "flow"],
        "run:stop" => &["result"],
        "run:fail" => &["failure", "err", "origin"],
        "run:cancel" => &["cancellation", "data", "origin"],
        _ => &[],
    };
    if let Val::Map(data) = &mut event.event_data {
        for field in fields {
            if let Some(value) = Arc::make_mut(data).get_mut(&Val::from(*field)) {
                *value = prepare_record_value(event.execution_context.run_id, value);
            }
        }
    }
    event
}

impl EngineEventEmitter for DatabaseEngineEventEmitter {
    fn emit(&self, event: EngineEvent) {
        let event_type = event.event_type.clone();
        let run_id = event.execution_context.run_id;

        if event_type == "run:start" || event_type == "run:stop" || event_type == "run:fail" {
            tracing::debug!(
                "DatabaseEngineEventEmitter: Emitting {} for run_id {}",
                event_type,
                run_id
            );
        }

        let event = if matches!(event_type.as_str(), "call:start" | "call:stop") {
            match self.decide_call(&event) {
                CallDecision::Record => event,
                CallDecision::Skip => return,
                CallDecision::Truncate => EngineEvent {
                    event_type: CALLS_TRUNCATED_EVENT.to_string(),
                    event_data: Val::Null,
                    ..event
                },
            }
        } else {
            event
        };

        let event = prepare_record_event(event);
        // Send bounded data through the processor for batching and masking.
        if self.event_sender.send(event).is_err() {
            // Only log error if shutdown hasn't been initiated (expected during shutdown)
            if !self.shutdown_initiated.load(Ordering::SeqCst) {
                tracing::error!(
                    "DatabaseEngineEventEmitter: Failed to send {} event - channel closed",
                    event_type
                );
            }
        }
    }

    fn flush(&self) -> Result<(), String> {
        tokio::runtime::Handle::current().block_on(self.flush_async())
    }

    fn flush_async(&self) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + '_>> {
        Box::pin(async move { self.flush_processor(None).await })
    }

    fn flush_run(
        &self,
        run_id: Uuid,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + '_>> {
        Box::pin(async move { self.flush_processor(Some(run_id)).await })
    }

    fn shutdown(&self) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + '_>> {
        Box::pin(async move { self.shutdown_impl().await })
    }
}

impl Clone for DatabaseEngineEventEmitter {
    fn clone(&self) -> Self {
        Self {
            db: Arc::clone(&self.db),
            writer: self.writer.clone(),
            event_sender: self.event_sender.clone(),
            flush_sender: self.flush_sender.clone(),
            shutdown_sender: self.shutdown_sender.clone(),
            shutdown_initiated: Arc::clone(&self.shutdown_initiated),
            max_calls_per_run: Arc::clone(&self.max_calls_per_run),
            call_budgets: Arc::clone(&self.call_budgets),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lang::bytecode::LambdaInfo;
    use crate::lang::runtime::function_ref::FunctionRef;
    use crate::val;
    use sqlx::Row;
    use uuid::Uuid;

    #[tokio::test]
    async fn test_database_emitter_creation() {
        let emitter = DatabaseEngineEventEmitter::new(val!({
            "uri": "sqlite::memory:",
        }));

        let execution_context = ExecutionContext::new(
            Uuid::now_v7(),
            Uuid::now_v7(),
            crate::db::run::RunType::Run.as_id(),
            Some(Uuid::now_v7()),
            Some(Uuid::now_v7()),
            Some(Uuid::now_v7()),
            None,
        );

        let start_event = EngineEvent::run_start(&execution_context);
        let stop_event = EngineEvent::run_stop(&execution_context, Val::Null);

        emitter.emit(start_event);
        emitter.emit(stop_event);
    }

    #[test]
    fn record_shape_cache_does_not_keep_discarded_inputs_alive() {
        let value = Val::vec(vec![Val::Int(1)]);
        let Val::Vec(items) = &value else {
            unreachable!()
        };
        let weak = Arc::downgrade(items);
        assert_eq!(prepare_record_value(Uuid::now_v7(), &value), value);
        drop(value);
        assert!(weak.upgrade().is_none());
    }

    #[test]
    fn test_call_value_serialization_hides_vm_instructions() {
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
            defining_frame: None,
            result_params: Vec::new(),
        };

        let args = Val::vec(vec![
            Val::Box(Box::new(FunctionRef::new("::hot::math/add".to_string()))),
            Val::Box(Box::new(lazy_thunk)),
        ]);

        let CallPayload::Json(json) = DatabaseEngineEventEmitterProcessor::call_payload(&args)
        else {
            panic!("a small value is recorded as JSON");
        };

        assert!(!json.contains("\"$box\""));
        assert!(!json.contains("instructions"));
        assert!(!json.contains("register_count"));

        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed[0]["$type"], "::hot::type/Fn");
        assert_eq!(parsed[0]["$val"], "::hot::math/add");
        assert_eq!(parsed[1]["$type"], "::hot::type/Fn");
        assert_eq!(parsed[1]["$val"]["lazy"], true);
        assert_eq!(parsed[1]["$val"]["captures"]["value"], false);
    }

    #[tokio::test]
    async fn test_database_emitter_persists_hot_data_repr_for_call_args() {
        let db = crate::db::test_db().await;
        let emitter = DatabaseEngineEventEmitter::new_with_pool(db.clone());
        let execution_context = persisted_execution_context(&db).await;
        let call_id = Uuid::now_v7();

        let mut closure_env = ahash::AHashMap::new();
        closure_env.insert("value".to_string(), Val::Bool(false));

        let lazy_thunk = LambdaInfo {
            parameters: vec![],
            instructions: vec![
                crate::lang::bytecode::Instruction::LoadVar {
                    dest: 0,
                    var_name: 0,
                },
                crate::lang::bytecode::Instruction::Return { value: 0 },
            ],
            register_count: 1,
            capture_vars: vec!["value".to_string()],
            closure_env,
            defining_namespace: "::hot::test".to_string(),
            is_lazy_param: true,
            used_registers: vec![],
            structural_hash_cache: Default::default(),
            defining_frame: None,
            result_params: Vec::new(),
        };

        emitter.emit(EngineEvent::call_start(
            &execution_context,
            call_id,
            None,
            "::hot::test/f".to_string(),
            "::hot::test/f".to_string(),
            "run/test".to_string(),
            0,
            vec![Val::Box(Box::new(lazy_thunk))],
            None,
            chrono::Utc::now(),
            None,
        ));
        emitter.emit(EngineEvent::call_stop(
            &execution_context,
            call_id,
            Some(Val::from("ok")),
            None,
            chrono::Utc::now(),
            10,
        ));
        emitter.shutdown().await.unwrap();

        let crate::db::DatabasePool::Sqlite(pool) = db else {
            panic!("test_db should return SQLite");
        };
        let args: String = sqlx::query("SELECT args FROM call WHERE call_id = ?")
            .bind(call_id)
            .fetch_one(&pool)
            .await
            .unwrap()
            .get("args");

        assert!(!args.contains("\"$box\""));
        assert!(!args.contains("instructions"));
        assert!(!args.contains("register_count"));

        let parsed: serde_json::Value = serde_json::from_str(&args).unwrap();
        assert_eq!(parsed[0], false);
    }

    fn test_blob_store(
        db: &crate::db::DatabasePool,
    ) -> (Arc<crate::blob::BlobStore>, tempfile::TempDir) {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let storage = Arc::new(crate::file_storage::LocalFileStorage::new(
            temp_dir.path().to_path_buf(),
        ));
        let config = crate::blob::BlobConfig {
            mode: crate::blob::BlobMode::Service,
            spill_threshold_bytes: 1024,
            ..crate::blob::BlobConfig::default()
        };
        (
            Arc::new(crate::blob::BlobStore::new(
                Arc::new(db.clone()),
                storage,
                config,
            )),
            temp_dir,
        )
    }

    async fn persisted_execution_context(db: &crate::db::DatabasePool) -> ExecutionContext {
        let fixture = crate::db::insert_test_data(db).await.unwrap();
        ExecutionContext::new(
            fixture.run_id,
            fixture.stream_id,
            crate::db::run::RunType::Run.as_id(),
            Some(fixture.env_id),
            Some(fixture.user_id),
            Some(fixture.org_id),
            Some(fixture.build_id),
        )
    }

    #[tokio::test]
    async fn emitter_reports_rejected_trace_on_flush_and_shutdown() {
        let db = crate::db::test_db().await;
        let emitter = DatabaseEngineEventEmitter::new_with_pool(db.clone());
        let mut execution_context = persisted_execution_context(&db).await;
        execution_context.env_id = None;
        let call_id = Uuid::now_v7();
        emitter.emit(EngineEvent::call_start(
            &execution_context,
            call_id,
            None,
            "::hot::test/invalid-scope".to_string(),
            "::hot::test/invalid-scope".to_string(),
            "run/test".to_string(),
            0,
            vec![],
            None,
            chrono::Utc::now(),
            None,
        ));
        assert!(emitter.flush_async().await.is_err());
        assert!(emitter.flush_async().await.is_err());
        assert!(emitter.shutdown().await.is_err());
        let crate::db::DatabasePool::Sqlite(pool) = db else {
            panic!("test_db should return SQLite");
        };
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM call WHERE call_id = ?")
            .bind(call_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 0, "scope-less traces must never be persisted");
    }

    #[tokio::test]
    async fn a_run_records_calls_up_to_its_cap_and_says_it_stopped() {
        let db = crate::db::test_db().await;
        let emitter =
            DatabaseEngineEventEmitter::new_with_pool(db.clone()).with_max_calls_per_run(3);
        let execution_context = persisted_execution_context(&db).await;

        emitter.emit(EngineEvent::run_start(&execution_context));
        let root = Uuid::now_v7();
        let call = |call_id: Uuid, parent: Option<Uuid>| {
            EngineEvent::call_start(
                &execution_context,
                call_id,
                parent,
                "busy".to_string(),
                "::demo".to_string(),
                "::demo/busy".to_string(),
                usize::from(parent.is_some()),
                Vec::new(),
                None,
                chrono::Utc::now(),
                None,
            )
        };
        let stop = |call_id: Uuid| {
            EngineEvent::call_stop(
                &execution_context,
                call_id,
                None,
                None,
                chrono::Utc::now(),
                1,
            )
        };
        emitter.emit(call(root, None));
        for _ in 0..5 {
            let child = Uuid::now_v7();
            emitter.emit(call(child, Some(root)));
            emitter.emit(stop(child));
        }
        emitter.emit(stop(root));
        emitter.emit(EngineEvent::run_stop(&execution_context, Val::Null));
        emitter.shutdown().await.unwrap();

        let crate::db::DatabasePool::Sqlite(pool) = db else {
            panic!("test_db should return SQLite");
        };
        // The calls it records keep their stops, and no stop comes alone.
        let (calls, stopped): (i64, i64) =
            sqlx::query_as("SELECT COUNT(*), COUNT(stop_time) FROM call WHERE run_id = ?")
                .bind(execution_context.run_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!((calls, stopped), (3, 3));
        let info: String = sqlx::query_scalar("SELECT info FROM run WHERE run_id = ?")
            .bind(execution_context.run_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        let info: serde_json::Value = serde_json::from_str(&info).unwrap();
        assert_eq!(info["call_tracing"], "truncated");
        assert_eq!(info["traced_calls_limit"], 3);
        assert_eq!(info["untraced_calls"], 3);
    }

    #[tokio::test]
    async fn run_start_persists_queue_phase_timing() {
        let db = crate::db::test_db().await;
        let emitter = DatabaseEngineEventEmitter::new_with_pool(db.clone());
        let mut execution_context = persisted_execution_context(&db).await;
        let claimed_at = chrono::Utc::now();
        execution_context.queue_timing = Some(crate::lang::event::QueueExecutionTiming {
            backend: "redis".to_string(),
            enqueued_at: Some(claimed_at - chrono::Duration::milliseconds(5)),
            claimed_at,
            queue_wait_us: 5_000,
            redelivered: false,
            handler_dispatched_at: Some(claimed_at),
        });

        emitter.emit(EngineEvent::run_start(&execution_context));
        emitter.emit(EngineEvent::run_stop(&execution_context, Val::Null));
        emitter.shutdown().await.unwrap();

        let crate::db::DatabasePool::Sqlite(pool) = db else {
            panic!("test_db should return SQLite");
        };
        let info: String = sqlx::query_scalar("SELECT info FROM run WHERE run_id = ?")
            .bind(execution_context.run_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        let info: serde_json::Value = serde_json::from_str(&info).unwrap();
        assert_eq!(info["queue_timing"]["backend"], "redis");
        assert_eq!(info["queue_timing"]["queue_wait_us"], 5_000);
        assert!(
            info["queue_timing"]["worker_preparation_us"]
                .as_i64()
                .is_some_and(|value| value >= 0)
        );
    }

    /// Rows of small cells: nothing in them is large enough to spill alone.
    fn many_small_rows(rows: usize, cell: &str) -> Val {
        Val::vec(
            (0..rows)
                .map(|i| crate::val!({"id": (i as i64), "note": cell, "zip": "02134"}))
                .collect(),
        )
    }

    /// Run one call returning `returned`, and read back its stored value.
    async fn record_return(
        db: &crate::db::DatabasePool,
        blob_store: Option<Arc<BlobStore>>,
        execution_context: &ExecutionContext,
        returned: Val,
    ) -> Val {
        let emitter =
            DatabaseEngineEventEmitter::new_with_pool_and_blob_store(db.clone(), blob_store);
        let call_id = Uuid::now_v7();
        emitter.emit(EngineEvent::call_start(
            execution_context,
            call_id,
            None,
            "::hot::test/rows".to_string(),
            "::hot::test/rows".to_string(),
            "run/test".to_string(),
            0,
            vec![],
            None,
            chrono::Utc::now(),
            None,
        ));
        emitter.emit(EngineEvent::call_stop(
            execution_context,
            call_id,
            Some(returned),
            None,
            chrono::Utc::now(),
            10,
        ));
        emitter.shutdown().await.unwrap();
        let crate::db::DatabasePool::Sqlite(pool) = db else {
            panic!("test_db should return SQLite");
        };
        let stored: String = sqlx::query_scalar("SELECT return_value FROM call WHERE call_id = ?")
            .bind(call_id)
            .fetch_one(pool)
            .await
            .unwrap();
        serde_json::from_str(&stored).unwrap()
    }

    #[tokio::test]
    async fn many_small_values_are_stored_whole_in_a_blob() {
        let db = crate::db::test_db().await;
        let (blob_store, _tmp) = test_blob_store(&db);
        let execution_context = persisted_execution_context(&db).await;
        let rows = many_small_rows(40_000, "a small cell");
        let stored = record_return(
            &db,
            Some(blob_store.clone()),
            &execution_context,
            rows.clone(),
        )
        .await;

        let blob_ref = crate::blob::BlobRefVal::from_val(&stored)
            .unwrap()
            .expect("the record should hold one reference");
        assert_eq!(blob_ref.encoding, crate::blob::ENCODING_JSON);

        let scope = BlobScope {
            org_id: execution_context.org_id.unwrap(),
            env_id: execution_context.env_id,
            run_id: Some(execution_context.run_id),
        };
        let budget = crate::blob::RehydrateBudget::from_config(blob_store.config());
        let restored = blob_store
            .rehydrate_blob_refs(stored, scope, budget)
            .await
            .unwrap();
        assert_eq!(restored, rows);
    }

    #[tokio::test]
    async fn secrets_are_masked_before_a_whole_value_is_stored() {
        const SECRET: &str = "SENTINEL-WHOLE-VALUE-SECRET";
        let db = crate::db::test_db().await;
        let (blob_store, tmp) = test_blob_store(&db);
        let execution_context = persisted_execution_context(&db).await;
        execution_context.secret_values.register(&Val::from(SECRET));
        let stored = record_return(
            &db,
            Some(blob_store.clone()),
            &execution_context,
            many_small_rows(40_000, SECRET),
        )
        .await;

        let scope = BlobScope {
            org_id: execution_context.org_id.unwrap(),
            env_id: execution_context.env_id,
            run_id: Some(execution_context.run_id),
        };
        let budget = crate::blob::RehydrateBudget::from_config(blob_store.config());
        let restored = blob_store
            .rehydrate_blob_refs(stored, scope, budget)
            .await
            .unwrap();
        let text = serde_json::to_string(&restored).unwrap();
        assert!(!text.contains(SECRET));
        assert!(text.contains(crate::lang::masking::MASK));

        // And nothing with it reached storage.
        for entry in walkdir_files(tmp.path()) {
            let bytes = std::fs::read(&entry).unwrap();
            assert!(
                !bytes.windows(SECRET.len()).any(|w| w == SECRET.as_bytes()),
                "{} holds the secret",
                entry.display()
            );
        }
    }

    fn walkdir_files(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
        let mut files = Vec::new();
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                files.extend(walkdir_files(&path));
            } else {
                files.push(path);
            }
        }
        files
    }

    #[tokio::test]
    async fn one_large_leaf_is_still_spilled_alone() {
        let db = crate::db::test_db().await;
        let (blob_store, _tmp) = test_blob_store(&db);
        let execution_context = persisted_execution_context(&db).await;
        let value = crate::val!({"kept": "inline", "text": ("x".repeat(3_000_000))});
        let stored = record_return(&db, Some(blob_store), &execution_context, value).await;
        let Val::Map(map) = &stored else {
            panic!("expected the Map inline: {stored:?}");
        };
        assert_eq!(map.get(&Val::from("kept")), Some(&Val::from("inline")));
        assert!(crate::blob::is_blob_ref_val(
            map.get(&Val::from("text")).unwrap()
        ));
    }

    fn omitted_reason(stored: &Val) -> String {
        let Val::Map(map) = stored else {
            panic!("expected a marker: {stored:?}");
        };
        assert_eq!(
            map.get(&Val::from("$type")),
            Some(&Val::from(crate::blob::OMITTED_TYPE))
        );
        let Some(Val::Map(inner)) = map.get(&Val::from("$val")) else {
            panic!("expected a reason: {stored:?}");
        };
        inner.get(&Val::from("reason")).unwrap().to_string()
    }

    #[tokio::test]
    async fn repeats_of_shared_storage_are_left_out_with_blob_storage_on() {
        let db = crate::db::test_db().await;
        let (blob_store, _tmp) = test_blob_store(&db);
        let execution_context = persisted_execution_context(&db).await;
        let text = Val::from("x".repeat(1024 * 1024));
        let stored = record_return(
            &db,
            Some(blob_store),
            &execution_context,
            Val::vec(vec![text; 100_000]),
        )
        .await;
        assert!(omitted_reason(&stored).contains("too large"), "{stored:?}");
    }

    #[tokio::test]
    async fn a_large_value_with_no_sharing_still_reaches_blob_storage() {
        let db = crate::db::test_db().await;
        let (blob_store, _tmp) = test_blob_store(&db);
        let execution_context = persisted_execution_context(&db).await;
        let cell = "x".repeat(1024);
        let stored = record_return(
            &db,
            Some(blob_store),
            &execution_context,
            many_small_rows(40_000, &cell),
        )
        .await;
        assert!(crate::blob::is_blob_ref_val(&stored), "{stored:.200?}");
    }

    #[tokio::test]
    async fn one_deep_value_leaves_a_run_calls_readable() {
        let db = crate::db::test_db().await;
        let execution_context = persisted_execution_context(&db).await;
        let emitter = DatabaseEngineEventEmitter::new_with_pool(db.clone());
        let deep = (0..1_000).fold(Val::from("password"), |inner, _| Val::vec(vec![inner]));
        let values = [Val::Int(1), deep.clone()];
        let mut call_ids = Vec::new();
        for value in values {
            let call_id = Uuid::now_v7();
            call_ids.push(call_id);
            emitter.emit(EngineEvent::call_start(
                &execution_context,
                call_id,
                None,
                "::hot::test/deep".to_string(),
                "::hot::test/deep".to_string(),
                "run/test".to_string(),
                0,
                vec![value.clone()],
                None,
                chrono::Utc::now(),
                None,
            ));
            emitter.emit(EngineEvent::call_stop(
                &execution_context,
                call_id,
                Some(value),
                None,
                chrono::Utc::now(),
                10,
            ));
        }
        emitter.shutdown().await.unwrap();

        let calls = crate::db::call::Call::get_calls_by_run(&db, &execution_context.run_id)
            .await
            .expect("a deep value must not make the run's calls unreadable");
        assert_eq!(calls.len(), 2);

        let call = crate::db::call::Call::get_call(&db, &call_ids[1])
            .await
            .expect("the deep call itself reads back");
        for stored in [call.return_value.unwrap(), call.args.unwrap()] {
            assert_eq!(stored["$type"], crate::blob::OMITTED_TYPE);
            assert_eq!(stored["$val"]["reason"], "too deeply nested to record");
        }
    }

    #[tokio::test]
    async fn a_run_result_of_shared_storage_is_left_out() {
        let db = crate::db::test_db().await;
        let execution_context = persisted_execution_context(&db).await;
        let emitter = DatabaseEngineEventEmitter::new_with_pool(db.clone());
        let text = Val::from("x".repeat(1024 * 1024));
        emitter.emit(EngineEvent::run_start(&execution_context));
        emitter.emit(EngineEvent::run_stop(
            &execution_context,
            Val::vec(vec![text; 100_000]),
        ));
        emitter.shutdown().await.unwrap();

        let crate::db::DatabasePool::Sqlite(pool) = db else {
            panic!("test_db should return SQLite");
        };
        let (stored,): (String,) = sqlx::query_as("SELECT result FROM run WHERE run_id = ?")
            .bind(execution_context.run_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert!(stored.contains(crate::blob::OMITTED_TYPE), "{stored:.200}");
    }

    #[tokio::test]
    async fn without_blob_storage_a_value_over_the_limit_is_left_out() {
        let db = crate::db::test_db().await;
        let execution_context = persisted_execution_context(&db).await;
        let cell = "x".repeat(1024);
        let stored = record_return(
            &db,
            None,
            &execution_context,
            many_small_rows(40_000, &cell),
        )
        .await;
        let Val::Map(map) = &stored else {
            panic!("expected a marker: {stored:?}");
        };
        assert_eq!(
            map.get(&Val::from("$type")),
            Some(&Val::from(crate::blob::OMITTED_TYPE))
        );
    }

    #[tokio::test]
    async fn without_blob_storage_a_smaller_value_stays_inline() {
        let db = crate::db::test_db().await;
        let execution_context = persisted_execution_context(&db).await;
        let rows = many_small_rows(40_000, "a small cell");
        let stored = record_return(&db, None, &execution_context, rows.clone()).await;
        assert_eq!(stored, rows);
    }

    #[tokio::test]
    async fn run_results_are_stored_the_same_way() {
        let db = crate::db::test_db().await;
        let (blob_store, _tmp) = test_blob_store(&db);
        let execution_context = persisted_execution_context(&db).await;
        let rows = many_small_rows(40_000, "a small cell");
        let emitter = DatabaseEngineEventEmitter::new_with_pool_and_blob_store(
            db.clone(),
            Some(blob_store.clone()),
        );
        emitter.emit(EngineEvent::run_start(&execution_context));
        emitter.emit(EngineEvent::run_stop(&execution_context, rows.clone()));
        emitter.shutdown().await.unwrap();
        let crate::db::DatabasePool::Sqlite(pool) = &db else {
            panic!("test_db should return SQLite");
        };
        let stored: String = sqlx::query_scalar("SELECT result FROM run WHERE run_id = ?")
            .bind(execution_context.run_id)
            .fetch_one(pool)
            .await
            .unwrap();
        let stored: Val = serde_json::from_str(&stored).unwrap();
        assert!(crate::blob::is_blob_ref_val(&stored), "{stored:?}");
        let scope = BlobScope {
            org_id: execution_context.org_id.unwrap(),
            env_id: execution_context.env_id,
            run_id: Some(execution_context.run_id),
        };
        let budget = crate::blob::RehydrateBudget::from_config(blob_store.config());
        let restored = blob_store
            .rehydrate_blob_refs(stored, scope, budget)
            .await
            .unwrap();
        assert_eq!(restored, rows);
    }

    #[tokio::test]
    async fn without_blob_storage_a_run_result_over_the_limit_is_left_out() {
        let db = crate::db::test_db().await;
        let execution_context = persisted_execution_context(&db).await;
        let cell = "x".repeat(1024);
        let emitter = DatabaseEngineEventEmitter::new_with_pool_and_blob_store(db.clone(), None);
        emitter.emit(EngineEvent::run_start(&execution_context));
        emitter.emit(EngineEvent::run_stop(
            &execution_context,
            many_small_rows(40_000, &cell),
        ));
        emitter.shutdown().await.unwrap();
        let crate::db::DatabasePool::Sqlite(pool) = &db else {
            panic!("test_db should return SQLite");
        };
        let stored: String = sqlx::query_scalar("SELECT result FROM run WHERE run_id = ?")
            .bind(execution_context.run_id)
            .fetch_one(pool)
            .await
            .unwrap();
        assert!(stored.contains(crate::blob::OMITTED_TYPE), "{stored:.200}");
    }

    #[tokio::test]
    async fn test_database_emitter_spills_large_call_args_to_blob() {
        let db = crate::db::test_db().await;
        let (blob_store, _tmp) = test_blob_store(&db);
        let emitter =
            DatabaseEngineEventEmitter::new_with_pool_and_blob_store(db.clone(), Some(blob_store));
        let execution_context = persisted_execution_context(&db).await;
        let call_id = Uuid::now_v7();
        let big_bytes = vec![7u8; 8192];

        emitter.emit(EngineEvent::call_start(
            &execution_context,
            call_id,
            None,
            "::hot::test/big".to_string(),
            "::hot::test/big".to_string(),
            "run/test".to_string(),
            0,
            vec![Val::bytes(big_bytes.clone())],
            None,
            chrono::Utc::now(),
            None,
        ));
        emitter.emit(EngineEvent::call_stop(
            &execution_context,
            call_id,
            Some(Val::from("ok")),
            None,
            chrono::Utc::now(),
            10,
        ));
        emitter.shutdown().await.unwrap();

        let crate::db::DatabasePool::Sqlite(pool) = db.clone() else {
            panic!("test_db should return SQLite");
        };
        let args: String = sqlx::query("SELECT args FROM call WHERE call_id = ?")
            .bind(call_id)
            .fetch_one(&pool)
            .await
            .unwrap()
            .get("args");

        // The stored JSON must be a compact BlobRef, not an inline byte array.
        assert!(args.contains("::hot::blob/BlobRef"), "args: {}", args);
        assert!(
            args.len() < 4096,
            "args should be compact, got {} bytes",
            args.len()
        );

        // The blob must round-trip through rehydration in the correct scope.
        let (blob_store2, _tmp2) = (
            // Rebuild a store over the same db to parse and verify records
            crate::db::blob::get_object_by_hash(
                &db,
                execution_context.org_id.unwrap(),
                execution_context.env_id,
                "blake3",
                blake3::hash(&big_bytes).to_hex().as_ref(),
            )
            .await
            .unwrap(),
            (),
        );
        let object = blob_store2.expect("blob object should exist for spilled bytes");
        assert_eq!(object.size, big_bytes.len() as i64);
        assert_eq!(object.status, "available");
    }

    async fn record_one_call(
        emitter: &DatabaseEngineEventEmitter,
        execution_context: &ExecutionContext,
        args: Vec<Val>,
    ) -> Uuid {
        let call_id = Uuid::now_v7();
        emitter.emit(EngineEvent::call_start(
            execution_context,
            call_id,
            None,
            "::hot::test/big".to_string(),
            "::hot::test/big".to_string(),
            "run/test".to_string(),
            0,
            args,
            None,
            chrono::Utc::now(),
            None,
        ));
        emitter.emit(EngineEvent::call_stop(
            execution_context,
            call_id,
            Some(Val::from("ok")),
            None,
            chrono::Utc::now(),
            10,
        ));
        call_id
    }

    async fn stored_args(db: &crate::db::DatabasePool, call_id: Uuid) -> String {
        let crate::db::DatabasePool::Sqlite(pool) = db else {
            panic!("test_db should return SQLite");
        };
        sqlx::query("SELECT args FROM call WHERE call_id = ?")
            .bind(call_id)
            .fetch_one(pool)
            .await
            .unwrap()
            .get("args")
    }

    #[tokio::test]
    async fn a_large_leaf_is_stored_once_as_it_is_however_often_it_is_passed() {
        let db = crate::db::test_db().await;
        let (blob_store, _tmp) = test_blob_store(&db);
        let emitter = DatabaseEngineEventEmitter::new_with_pool_and_blob_store(
            db.clone(),
            Some(blob_store.clone()),
        );
        let execution_context = persisted_execution_context(&db).await;
        let bytes = vec![7u8; 2 * LARGE_PAYLOAD_LEAF_BYTES];
        let shared = Val::bytes(bytes.clone());

        let first = record_one_call(&emitter, &execution_context, vec![shared.clone()]).await;
        let second = record_one_call(&emitter, &execution_context, vec![shared]).await;
        emitter.shutdown().await.unwrap();

        let (first, second) = (
            stored_args(&db, first).await,
            stored_args(&db, second).await,
        );
        assert!(first.contains("::hot::blob/BlobRef"), "args: {first}");
        assert!(second.contains("::hot::blob/BlobRef"), "args: {second}");

        // Stored raw, not as JSON: the blob's hash and size are the bytes' own.
        let object = crate::db::blob::get_object_by_hash(
            &db,
            execution_context.org_id.unwrap(),
            execution_context.env_id,
            "blake3",
            blake3::hash(&bytes).to_hex().as_ref(),
        )
        .await
        .unwrap()
        .expect("the bytes are one blob object");
        assert_eq!(object.size, bytes.len() as i64);

        // What a reader gets back is what the call was passed.
        let scope = crate::blob::BlobScope {
            org_id: execution_context.org_id.unwrap(),
            env_id: execution_context.env_id,
            run_id: Some(execution_context.run_id),
        };
        let rehydrated = blob_store
            .rehydrate_blob_refs(
                serde_json::from_str(&first).unwrap(),
                scope,
                crate::blob::RehydrateBudget::from_config(blob_store.config()),
            )
            .await
            .unwrap();
        assert_eq!(
            format!("{rehydrated:?}"),
            format!("{:?}", Val::vec(vec![Val::bytes(bytes)]))
        );
    }

    #[tokio::test]
    async fn a_secret_in_a_large_leaf_never_reaches_blob_storage() {
        let db = crate::db::test_db().await;
        let (blob_store, _tmp) = test_blob_store(&db);
        let emitter =
            DatabaseEngineEventEmitter::new_with_pool_and_blob_store(db.clone(), Some(blob_store));
        let execution_context = persisted_execution_context(&db).await;
        // A credential field, in text big enough to be held as a value.
        let mut bytes = br#"{"password":"hunter2-hunter2"}"#.to_vec();
        bytes.resize(2 * LARGE_PAYLOAD_LEAF_BYTES, b' ');

        let call_id = record_one_call(
            &emitter,
            &execution_context,
            vec![Val::bytes(bytes.clone())],
        )
        .await;
        emitter.shutdown().await.unwrap();

        let args = stored_args(&db, call_id).await;
        assert!(args.contains("<secret>"), "args: {args}");
        let object = crate::db::blob::get_object_by_hash(
            &db,
            execution_context.org_id.unwrap(),
            execution_context.env_id,
            "blake3",
            blake3::hash(&bytes).to_hex().as_ref(),
        )
        .await
        .unwrap();
        assert!(object.is_none(), "the bytes must not be stored");
    }

    #[tokio::test]
    async fn a_large_leaf_is_recorded_inline_without_blob_storage() {
        let db = crate::db::test_db().await;
        let emitter = DatabaseEngineEventEmitter::new_with_pool(db.clone());
        let execution_context = persisted_execution_context(&db).await;

        let bytes = Val::bytes(vec![7u8; 2 * LARGE_PAYLOAD_LEAF_BYTES]);
        let call_id = record_one_call(&emitter, &execution_context, vec![bytes.clone()]).await;
        emitter.shutdown().await.unwrap();

        let args: Val = serde_json::from_str(&stored_args(&db, call_id).await).unwrap();
        assert_eq!(format!("{args:?}"), format!("{:?}", Val::vec(vec![bytes])));
    }

    /// An event held in the queue holds its args, so a call past the cap has to
    /// go as it's emitted. Three million of them held 20 GB in one run.
    #[tokio::test]
    async fn a_call_past_the_cap_goes_as_it_is_emitted() {
        let db = crate::db::test_db().await;
        let emitter =
            DatabaseEngineEventEmitter::new_with_pool(db.clone()).with_max_calls_per_run(1);
        let execution_context = persisted_execution_context(&db).await;
        let start = |call_id: Uuid| {
            EngineEvent::call_start(
                &execution_context,
                call_id,
                None,
                "busy".to_string(),
                "::demo".to_string(),
                "::demo/busy".to_string(),
                0,
                Vec::new(),
                None,
                chrono::Utc::now(),
                None,
            )
        };
        let stop = |call_id: Uuid| {
            EngineEvent::call_stop(
                &execution_context,
                call_id,
                None,
                None,
                chrono::Utc::now(),
                1,
            )
        };
        let (recorded, truncated, skipped) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());

        assert_eq!(emitter.decide_call(&start(recorded)), CallDecision::Record);
        assert_eq!(
            emitter.decide_call(&start(truncated)),
            CallDecision::Truncate
        );
        assert_eq!(emitter.decide_call(&start(skipped)), CallDecision::Skip);
        assert_eq!(emitter.decide_call(&stop(skipped)), CallDecision::Skip);
        assert_eq!(emitter.decide_call(&stop(recorded)), CallDecision::Record);
        emitter.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn test_database_emitter_spills_large_run_result_when_enabled() {
        let db = crate::db::test_db().await;
        let (blob_store, _tmp) = test_blob_store(&db);
        let emitter =
            DatabaseEngineEventEmitter::new_with_pool_and_blob_store(db.clone(), Some(blob_store));
        let execution_context = persisted_execution_context(&db).await;

        emitter.emit(EngineEvent::run_start(&execution_context));
        emitter.emit(EngineEvent::run_stop(
            &execution_context,
            Val::bytes(vec![9u8; 8192]),
        ));
        emitter.shutdown().await.unwrap();

        let crate::db::DatabasePool::Sqlite(pool) = db else {
            panic!("test_db should return SQLite");
        };
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM run")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 1, "expected exactly one run row");
        let result: String = sqlx::query("SELECT result FROM run WHERE run_id = ?")
            .bind(execution_context.run_id)
            .fetch_one(&pool)
            .await
            .unwrap()
            .get("result");

        assert!(result.contains("::hot::blob/BlobRef"), "result: {}", result);
        assert!(result.len() < 4096);
    }

    #[tokio::test]
    async fn test_database_emitter_masks_secrets_before_spill() {
        let db = crate::db::test_db().await;
        let (blob_store, tmp) = test_blob_store(&db);
        let emitter = DatabaseEngineEventEmitter::new_with_pool_and_blob_store(
            db.clone(),
            Some(blob_store.clone()),
        );
        let mut execution_context = persisted_execution_context(&db).await;

        // Register a large string as a secret.
        let secret_text = "s".repeat(4096);
        let secret_val = Val::from(secret_text.clone());
        {
            use std::hash::{Hash, Hasher};
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            secret_val.hash(&mut hasher);
            execution_context
                .secret_value_hashes
                .insert(hasher.finish());
        }

        let call_id = Uuid::now_v7();
        emitter.emit(EngineEvent::call_start(
            &execution_context,
            call_id,
            None,
            "::hot::test/secret".to_string(),
            "::hot::test/secret".to_string(),
            "run/test".to_string(),
            0,
            vec![secret_val],
            None,
            chrono::Utc::now(),
            None,
        ));
        emitter.emit(EngineEvent::call_stop(
            &execution_context,
            call_id,
            Some(Val::from("ok")),
            None,
            chrono::Utc::now(),
            10,
        ));
        emitter.shutdown().await.unwrap();

        let crate::db::DatabasePool::Sqlite(pool) = db.clone() else {
            panic!("test_db should return SQLite");
        };
        let args: String = sqlx::query("SELECT args FROM call WHERE call_id = ?")
            .bind(call_id)
            .fetch_one(&pool)
            .await
            .unwrap()
            .get("args");

        // The secret was masked before any spill could happen; nothing large
        // remained, so no blob was written and no secret bytes are on disk.
        assert!(args.contains("<secret>"), "args: {}", args);
        assert!(!args.contains(&secret_text));
        let secret_hash = blake3::hash(secret_text.as_bytes()).to_hex().to_string();
        let object = crate::db::blob::get_object_by_hash(
            &db,
            execution_context.org_id.unwrap(),
            execution_context.env_id,
            "blake3",
            &secret_hash,
        )
        .await
        .unwrap();
        assert!(
            object.is_none(),
            "secret bytes must never reach blob storage"
        );
        drop(tmp);
    }

    async fn stored_call(
        db: &crate::db::DatabasePool,
        call_id: Uuid,
    ) -> (Option<String>, Option<String>, Option<String>) {
        let crate::db::DatabasePool::Sqlite(pool) = db.clone() else {
            panic!("test_db should return SQLite");
        };
        let row = sqlx::query("SELECT args, return_value, error FROM call WHERE call_id = ?")
            .bind(call_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        (row.get("args"), row.get("return_value"), row.get("error"))
    }

    fn start_event(
        ctx: &ExecutionContext,
        call_id: Uuid,
        parent: Option<Uuid>,
        arg: &str,
    ) -> EngineEvent {
        EngineEvent::call_start(
            ctx,
            call_id,
            parent,
            "::hot::test/call".to_string(),
            "::hot::test/call".to_string(),
            "run/test".to_string(),
            usize::from(parent.is_some()),
            vec![Val::from(arg)],
            None,
            chrono::Utc::now(),
            None,
        )
    }

    fn stop_event(
        ctx: &ExecutionContext,
        call_id: Uuid,
        result: &str,
        error: Option<&str>,
    ) -> EngineEvent {
        EngineEvent::call_stop(
            ctx,
            call_id,
            Some(Val::from(result)),
            error.map(str::to_string),
            chrono::Utc::now(),
            10,
        )
    }

    const LATE_SECRET: &str = "registered-late-secret";

    /// A secret registered during a call masks that call's row and its
    /// parents', though both were written before it existed, in earlier
    /// batches, and neither has stopped.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_secret_registered_later_masks_open_calls_already_written() {
        let db = crate::db::test_db().await;
        let execution_context = persisted_execution_context(&db).await;
        let emitter = DatabaseEngineEventEmitter::new_with_pool(db.clone());
        let bearer = format!("Bearer {LATE_SECRET}");

        emitter.emit(EngineEvent::run_start(&execution_context));
        let parent = Uuid::now_v7();
        let child = Uuid::now_v7();
        emitter.emit(start_event(&execution_context, parent, None, &bearer));
        emitter.flush_async().await.unwrap();
        emitter.emit(start_event(
            &execution_context,
            child,
            Some(parent),
            &bearer,
        ));
        emitter.flush_async().await.unwrap();
        let written = stored_call(&db, parent).await.0.unwrap();
        assert!(written.contains(LATE_SECRET), "{written}");

        execution_context
            .secret_values
            .register(&Val::from(LATE_SECRET));

        // Neither call has stopped: the next flush masks them all the same.
        emitter.flush_async().await.unwrap();
        for call in [parent, child] {
            let args = stored_call(&db, call).await.0.unwrap();
            assert!(!args.contains(LATE_SECRET), "{args}");
            assert!(args.contains("Bearer <secret>"), "{args}");
        }

        emitter.emit(stop_event(&execution_context, child, LATE_SECRET, None));
        emitter.emit(stop_event(&execution_context, parent, "done", None));
        emitter.emit(EngineEvent::run_stop(&execution_context, Val::Null));
        emitter.shutdown().await.unwrap();
        let (args, return_value, _) = stored_call(&db, child).await;
        assert!(args.unwrap().contains("Bearer <secret>"));
        assert_eq!(return_value.unwrap(), "\"<secret>\"");
        let args = stored_call(&db, parent).await.0.unwrap();
        assert!(args.contains("Bearer <secret>"), "{args}");
    }

    /// A call that stops after the secret was registered masks its args as it
    /// stops, even when no flush came between.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_secret_registered_just_before_a_stop_masks_the_args_written_earlier() {
        let db = crate::db::test_db().await;
        let execution_context = persisted_execution_context(&db).await;
        let emitter = DatabaseEngineEventEmitter::new_with_pool(db.clone());
        emitter.emit(EngineEvent::run_start(&execution_context));
        let call = Uuid::now_v7();
        emitter.emit(start_event(&execution_context, call, None, LATE_SECRET));
        emitter.flush_async().await.unwrap();

        execution_context
            .secret_values
            .register(&Val::from(LATE_SECRET));
        emitter.emit(stop_event(&execution_context, call, "done", None));
        emitter.emit(EngineEvent::run_stop(&execution_context, Val::Null));
        emitter.shutdown().await.unwrap();

        let (args, return_value, _) = stored_call(&db, call).await;
        assert!(!args.unwrap().contains(LATE_SECRET));
        assert_eq!(return_value.unwrap(), "\"done\"");
    }

    /// Records in the same flush as the registration are covered, but a call
    /// that stopped and was written before it keeps what it had: the limit
    /// `resources/docs/language/meta.md` documents.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_secret_registered_later_masks_finished_calls_already_written() {
        let db = crate::db::test_db().await;
        let execution_context = persisted_execution_context(&db).await;
        let emitter = DatabaseEngineEventEmitter::new_with_pool(db.clone());
        emitter.emit(EngineEvent::run_start(&execution_context));

        let written = Uuid::now_v7();
        emitter.emit(start_event(&execution_context, written, None, LATE_SECRET));
        emitter.emit(stop_event(&execution_context, written, "done", None));
        emitter.flush_async().await.unwrap();

        let pending = Uuid::now_v7();
        emitter.emit(start_event(&execution_context, pending, None, LATE_SECRET));
        emitter.emit(stop_event(&execution_context, pending, "done", None));
        execution_context
            .secret_values
            .register(&Val::from(LATE_SECRET));
        emitter.emit(EngineEvent::run_stop(&execution_context, Val::Null));
        emitter.shutdown().await.unwrap();

        for call_id in [written, pending] {
            let args = stored_call(&db, call_id).await.0.unwrap();
            assert!(!args.contains(LATE_SECRET), "{args}");
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_running_run_masks_rows_it_wrote_before_a_secret() {
        let db = crate::db::test_db().await;
        let execution_context = persisted_execution_context(&db).await;
        let emitter = DatabaseEngineEventEmitter::new_with_pool(db.clone());
        emitter.emit(EngineEvent::run_start(&execution_context));

        let written = Uuid::now_v7();
        emitter.emit(start_event(&execution_context, written, None, LATE_SECRET));
        emitter.emit(stop_event(&execution_context, written, LATE_SECRET, None));
        emitter.flush_async().await.unwrap();
        let stored = stored_call(&db, written).await;
        assert!(stored.1.unwrap().contains(LATE_SECRET));

        execution_context
            .secret_values
            .register(&Val::from(LATE_SECRET));
        emitter.flush_async().await.unwrap();
        let (args, return_value, _) = stored_call(&db, written).await;
        assert!(!args.unwrap().contains(LATE_SECRET));
        assert!(!return_value.unwrap().contains(LATE_SECRET));
        emitter.shutdown().await.unwrap();
    }

    /// Calls can't be updated until this trigger is dropped.
    async fn block_call_updates(db: &crate::db::DatabasePool) {
        let crate::db::DatabasePool::Sqlite(pool) = db else {
            panic!("test_db should return SQLite");
        };
        sqlx::query(
            "CREATE TRIGGER block_call_updates BEFORE UPDATE ON call \
             BEGIN SELECT RAISE(ABORT, 'blocked'); END",
        )
        .execute(pool)
        .await
        .unwrap();
    }

    async fn unblock_call_updates(db: &crate::db::DatabasePool) {
        let crate::db::DatabasePool::Sqlite(pool) = db else {
            panic!("test_db should return SQLite");
        };
        sqlx::query("DROP TRIGGER block_call_updates")
            .execute(pool)
            .await
            .unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn masking_written_calls_survives_a_brief_failure() {
        let db = crate::db::test_db().await;
        let execution_context = persisted_execution_context(&db).await;
        let emitter = DatabaseEngineEventEmitter::new_with_pool(db.clone());
        emitter.emit(EngineEvent::run_start(&execution_context));
        let written = Uuid::now_v7();
        emitter.emit(start_event(&execution_context, written, None, LATE_SECRET));
        emitter.emit(stop_event(&execution_context, written, "done", None));
        emitter.flush_async().await.unwrap();

        block_call_updates(&db).await;
        execution_context
            .secret_values
            .register(&Val::from(LATE_SECRET));
        let unblocking = {
            let db = db.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(150)).await;
                unblock_call_updates(&db).await;
            })
        };
        emitter.flush_async().await.unwrap();
        unblocking.await.unwrap();

        let args = stored_call(&db, written).await.0.unwrap();
        assert!(!args.contains(LATE_SECRET), "{args}");
        emitter.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_run_reports_written_calls_it_could_not_mask() {
        let db = crate::db::test_db().await;
        let execution_context = persisted_execution_context(&db).await;
        let emitter = DatabaseEngineEventEmitter::new_with_pool(db.clone());
        emitter.emit(EngineEvent::run_start(&execution_context));
        let written = Uuid::now_v7();
        emitter.emit(start_event(&execution_context, written, None, LATE_SECRET));
        emitter.emit(stop_event(&execution_context, written, "done", None));
        emitter.flush_async().await.unwrap();

        block_call_updates(&db).await;
        execution_context
            .secret_values
            .register(&Val::from(LATE_SECRET));
        assert!(emitter.flush_async().await.is_err());
        assert!(emitter.shutdown().await.is_err());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_run_that_keeps_registering_is_not_read_again_each_flush() {
        let db = crate::db::test_db().await;
        let execution_context = persisted_execution_context(&db).await;
        let emitter = DatabaseEngineEventEmitter::new_with_pool(db.clone());
        emitter.emit(EngineEvent::run_start(&execution_context));
        let written = Uuid::now_v7();
        emitter.emit(start_event(&execution_context, written, None, LATE_SECRET));
        emitter.emit(stop_event(&execution_context, written, "done", None));
        emitter.flush_async().await.unwrap();

        // The first registration masks at once, the next waits its turn, and
        // the end of the run doesn't wait.
        execution_context
            .secret_values
            .register(&Val::from("first-registered-secret"));
        emitter.flush_async().await.unwrap();
        execution_context
            .secret_values
            .register(&Val::from(LATE_SECRET));
        emitter.flush_async().await.unwrap();
        let args = stored_call(&db, written).await.0.unwrap();
        assert!(args.contains(LATE_SECRET), "{args}");

        emitter.emit(EngineEvent::run_stop(&execution_context, Val::Null));
        emitter.shutdown().await.unwrap();
        let args = stored_call(&db, written).await.0.unwrap();
        assert!(!args.contains(LATE_SECRET), "{args}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn masks_call_errors_and_run_failures() {
        let db = crate::db::test_db().await;
        let execution_context = persisted_execution_context(&db).await;
        execution_context
            .secret_values
            .register(&Val::from(LATE_SECRET));
        let emitter = DatabaseEngineEventEmitter::new_with_pool(db.clone());
        emitter.emit(EngineEvent::run_start(&execution_context));
        let call = Uuid::now_v7();
        emitter.emit(start_event(&execution_context, call, None, "input"));
        emitter.emit(stop_event(
            &execution_context,
            call,
            "x",
            Some(&format!("rejected {LATE_SECRET}, password=hunter2")),
        ));
        emitter.emit(EngineEvent::run_fail(
            &execution_context,
            Val::from(format!("failed with {LATE_SECRET}")),
        ));
        emitter.shutdown().await.unwrap();

        assert_eq!(
            stored_call(&db, call).await.2.unwrap(),
            "rejected <secret>, password=<secret>"
        );
        let crate::db::DatabasePool::Sqlite(pool) = db.clone() else {
            panic!("test_db should return SQLite");
        };
        let result: String = sqlx::query("SELECT result FROM run WHERE run_id = ?")
            .bind(execution_context.run_id)
            .fetch_one(&pool)
            .await
            .unwrap()
            .get("result");
        assert!(!result.contains(LATE_SECRET), "{result}");
    }

    /// Without any secret registered, credential fields go and the rest of a
    /// record is written as it was.
    #[tokio::test(flavor = "multi_thread")]
    async fn redacts_credential_fields_and_preserves_the_rest() {
        let db = crate::db::test_db().await;
        let execution_context = persisted_execution_context(&db).await;
        let emitter = DatabaseEngineEventEmitter::new_with_pool(db.clone());
        emitter.emit(EngineEvent::run_start(&execution_context));
        let call = Uuid::now_v7();
        emitter.emit(EngineEvent::call_start(
            &execution_context,
            call,
            None,
            "::hot::test/call".to_string(),
            "::hot::test/call".to_string(),
            "run/test".to_string(),
            0,
            vec![
                crate::val!({
                    "$type": "::hot::http/HttpRequest",
                    "$val": {
                        "headers": {"Authorization": "Bearer unregistered", "Accept": "text/plain"},
                        "body": "client_secret=unregistered&page=2",
                    },
                }),
                Val::Int(7),
            ],
            None,
            chrono::Utc::now(),
            None,
        ));
        emitter.emit(EngineEvent::run_stop(&execution_context, Val::Null));
        emitter.shutdown().await.unwrap();

        let args = stored_call(&db, call).await.0.unwrap();
        assert!(!args.contains("unregistered"), "{args}");
        assert!(args.contains("::hot::http/HttpRequest"), "{args}");
        assert!(args.contains("\"Accept\":\"text/plain\""), "{args}");
        assert!(args.contains("client_secret=<secret>&page=2"), "{args}");
        assert!(args.contains(",7]"), "{args}");
    }
}

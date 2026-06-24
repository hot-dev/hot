use super::database_writer::{DatabaseWrite, DatabaseWriter};
use super::postgres_safety::{sanitize_json_for_jsonb, sanitize_text_for_postgres};
use super::{EngineEvent, EngineEventEmitter};
use crate::db::DatabasePool;
use crate::lang::event::ExecutionContext;
use crate::val::Val;
use ahash::AHashSet;
use indexmap::IndexMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
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
    shutdown_sender: mpsc::UnboundedSender<oneshot::Sender<()>>,
    /// Flag to suppress errors during shutdown (when channel is expected to be closed)
    shutdown_initiated: Arc<AtomicBool>,
}

/// Internal processor for handling database events with batching
struct DatabaseEngineEventEmitterProcessor {
    writer: DatabaseWriter,
    pending_calls: IndexMap<(Uuid, Uuid), CallEngineEventBatch>,
}

struct ProcessorFlushRequest {
    run_id: Option<Uuid>,
    completion: oneshot::Sender<Result<(), String>>,
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
}

const BATCH_INTERVAL: Duration = Duration::from_millis(500);

impl DatabaseEngineEventEmitter {
    /// Creates a new DatabaseEngineEventEmitter with an existing database pool (preferred)
    /// This ensures the database connection is ready before events are processed
    pub fn new_with_pool(db_pool: DatabasePool) -> Self {
        let db = Arc::new(RwLock::new(Some(db_pool)));

        // Create the dedicated database writer
        let writer = DatabaseWriter::new(Arc::clone(&db));

        // Create event processing channel
        let (event_sender, mut event_receiver) = mpsc::unbounded_channel::<EngineEvent>();
        let (flush_sender, mut flush_receiver) = mpsc::unbounded_channel::<ProcessorFlushRequest>();

        // Create shutdown channel
        let (shutdown_sender, mut shutdown_receiver) =
            mpsc::unbounded_channel::<oneshot::Sender<()>>();

        let writer_for_processor = writer.clone();

        // Start event processing task
        tokio::spawn(async move {
            let mut processor = DatabaseEngineEventEmitterProcessor {
                writer: writer_for_processor,
                pending_calls: IndexMap::new(),
            };

            let mut flush_timer = interval(BATCH_INTERVAL);
            flush_timer.tick().await; // Skip first immediate tick

            loop {
                tokio::select! {
                    // Process incoming events
                    event = event_receiver.recv() => {
                        match event {
                            Some(evt) => processor.process_event(evt),
                            None => {
                                processor.flush_all();
                                if let Err(e) = processor.writer.shutdown().await {
                                    tracing::debug!("DatabaseEngineEventEmitter: Writer shutdown on channel close: {}", e);
                                }
                                break;
                            }
                        }
                    }
                    // Flush batched events every 500ms
                    _ = flush_timer.tick() => {
                        processor.flush_all();
                    }
                    // Handle explicit flush requests by draining queued events
                    // before waiting on the writer.
                    flush_request = flush_receiver.recv() => {
                        if let Some(request) = flush_request {
                            while let Ok(evt) = event_receiver.try_recv() {
                                processor.process_event(evt);
                            }
                            processor.flush_all();
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
                                processor.process_event(evt);
                            }

                            // Final flush
                            processor.flush_all();

                            // Shutdown the writer and wait for completion
                            if let Err(e) = processor.writer.shutdown().await {
                                tracing::error!("DatabaseEngineEventEmitter: Writer shutdown error: {}", e);
                            }

                            // Signal completion
                            let _ = sender.send(());
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
            mpsc::unbounded_channel::<oneshot::Sender<()>>();

        let db_for_processor = Arc::clone(&db);
        let writer_for_processor = writer.clone();

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

            let mut processor = DatabaseEngineEventEmitterProcessor {
                writer: writer_for_processor,
                pending_calls: IndexMap::new(),
            };

            let mut flush_timer = interval(BATCH_INTERVAL);
            flush_timer.tick().await; // Skip first immediate tick

            loop {
                tokio::select! {
                    // Process incoming events
                    event = event_receiver.recv() => {
                        match event {
                            Some(evt) => processor.process_event(evt),
                            None => {
                                processor.flush_all();
                                if let Err(e) = processor.writer.shutdown().await {
                                    tracing::debug!("DatabaseEngineEventEmitter: Writer shutdown on channel close: {}", e);
                                }
                                break;
                            }
                        }
                    }
                    // Flush batched events every 500ms
                    _ = flush_timer.tick() => {
                        processor.flush_all();
                    }
                    // Handle explicit flush requests by draining queued events
                    // before waiting on the writer.
                    flush_request = flush_receiver.recv() => {
                        if let Some(request) = flush_request {
                            while let Ok(evt) = event_receiver.try_recv() {
                                processor.process_event(evt);
                            }
                            processor.flush_all();
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
                                processor.process_event(evt);
                            }

                            // Final flush
                            processor.flush_all();

                            // Shutdown the writer and wait for completion
                            if let Err(e) = processor.writer.shutdown().await {
                                tracing::error!("DatabaseEngineEventEmitter: Writer shutdown error: {}", e);
                            }

                            // Signal completion
                            let _ = sender.send(());
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
        }
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

        match completion_receiver.await {
            Ok(()) => Ok(()),
            Err(_) => Err("Shutdown completion signal was dropped".to_string()),
        }
    }
}

impl DatabaseEngineEventEmitterProcessor {
    /// Process a single event - sends critical events immediately, batches others
    fn process_event(&mut self, event: EngineEvent) {
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
                    Self::mask_secrets_in_val(
                        r.clone(),
                        &event.execution_context.secret_value_hashes,
                    )
                } else {
                    Val::Null
                };
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
                    Self::mask_secrets_in_val(
                        f.clone(),
                        &event.execution_context.secret_value_hashes,
                    )
                } else {
                    Val::Null
                };
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
                    Self::mask_secrets_in_val(
                        c.clone(),
                        &event.execution_context.secret_value_hashes,
                    )
                } else {
                    Val::Null
                };
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

    /// Flush all pending batched events
    fn flush_all(&mut self) {
        self.flush_pending_calls();
    }

    /// Flush all pending call events
    /// Uses UPSERT to handle INSERT (on start) and UPDATE (on stop) seamlessly
    /// Sorts by call_depth to ensure parent calls are written before children
    fn flush_pending_calls(&mut self) {
        let pending = std::mem::take(&mut self.pending_calls);

        // CRITICAL: Sort calls by call_depth to ensure parent calls are written before children
        // This prevents foreign key violations on parent_call_id
        let mut sorted_batches: Vec<_> = pending.into_iter().map(|(_, batch)| batch).collect();
        sorted_batches.sort_by_key(|batch| batch.call_depth);

        for batch in sorted_batches {
            // Mask args that contain known secret values (deep scan)
            let args = Self::maybe_mask_secret_value(
                batch.args,
                &batch.execution_context.secret_value_hashes,
            );

            // Mask return values that contain known secret values (deep scan)
            let return_value = Self::maybe_mask_secret_value(
                batch.return_value,
                &batch.execution_context.secret_value_hashes,
            );

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
                error: batch.error,
                flow: batch.flow,
                file: batch.file,
                line: batch.line,
                column: batch.column,
                position: batch.position,
                start_time: batch.start_time,
                stop_time: batch.stop_time,
                duration_us: batch.duration_us,
            });
        }
    }

    /// Serialize a `Val` to JSON for storage in a Postgres `jsonb` column,
    /// scrubbing out characters Postgres rejects (`\u0000`, lone UTF-16
    /// surrogates) but JSON otherwise allows. Without this, a single bad
    /// byte in a container/webhook payload would 22P05 the whole
    /// transaction batch and we'd silently drop a window of trace data.
    /// See [`super::postgres_safety`] for the full rationale.
    fn serialize_val_for_jsonb(v: &Val) -> String {
        let storage_val = v.to_hot_data_repr();
        let raw = serde_json::to_string(&storage_val).unwrap_or_default();
        sanitize_json_for_jsonb(&raw).into_owned()
    }

    /// Check if a JSON value contains any secret values (deep scan)
    /// Uses hash-based matching for efficiency and to handle any Val type
    /// Recursively scans through Maps and Vecs to find and mask secrets
    fn maybe_mask_secret_value(
        json_value: Option<String>,
        secret_value_hashes: &AHashSet<u64>,
    ) -> Option<String> {
        // If no value or no secrets to check, nothing to mask
        let json_str = match &json_value {
            Some(v) => v,
            None => return json_value,
        };

        // If value is null or no secrets registered, nothing to mask
        if json_str == "null" || secret_value_hashes.is_empty() {
            return json_value;
        }

        // Parse the JSON value into a Val
        let val: Val = match serde_json::from_str(json_str) {
            Ok(v) => v,
            Err(_) => return json_value, // Can't parse, return as-is
        };

        // Recursively mask secrets in the value
        let masked_val = Self::mask_secrets_in_val(val, secret_value_hashes);

        // Re-serialize to JSON. Run through the Postgres-jsonb scrubber
        // again so any NUL/lone-surrogate that was introduced by the
        // mask pass (or that survived an upstream path that didn't
        // sanitize) doesn't sneak through.
        match serde_json::to_string(&masked_val) {
            Ok(json) => Some(sanitize_json_for_jsonb(&json).into_owned()),
            Err(_) => json_value, // Serialization failed, return original
        }
    }

    /// Recursively scan a Val and replace any values matching secret hashes with "<secret>"
    fn mask_secrets_in_val(val: Val, secret_hashes: &AHashSet<u64>) -> Val {
        use std::hash::{Hash, Hasher};

        // First check if this entire value is a secret
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        val.hash(&mut hasher);
        let hash = hasher.finish();

        if secret_hashes.contains(&hash) {
            return Val::from("<secret>");
        }

        // If not a direct match, recursively scan compound types
        match val {
            Val::Map(map) => {
                let masked_map: indexmap::IndexMap<Val, Val> = map
                    .into_iter()
                    .map(|(k, v)| (k, Self::mask_secrets_in_val(v, secret_hashes)))
                    .collect();
                Val::Map(Box::new(masked_map))
            }
            Val::Vec(vec) => {
                let masked_vec: Vec<Val> = vec
                    .into_iter()
                    .map(|v| Self::mask_secrets_in_val(v, secret_hashes))
                    .collect();
                Val::Vec(masked_vec)
            }
            // Leaf values that weren't a direct match - return as-is
            other => other,
        }
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
                        Some(Self::serialize_val_for_jsonb(v))
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
                        Some(Self::serialize_val_for_jsonb(v))
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
                        Some(Self::serialize_val_for_jsonb(v))
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

                // CRITICAL: Merge secret_value_hashes from call:stop's execution_context
                // The call:start event is emitted BEFORE the function runs, so it doesn't
                // have secret hashes from ctx/get calls made during the function.
                // The call:stop event is emitted AFTER, so it has the updated hashes.
                for hash in &event.execution_context.secret_value_hashes {
                    batch.execution_context.secret_value_hashes.insert(*hash);
                }
            }
        }
    }
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

        // Send all events through the processor for batching, secret masking, etc.
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
        };

        let args = Val::Vec(vec![
            Val::Box(Box::new(FunctionRef::new("::hot::math/add".to_string()))),
            Val::Box(Box::new(lazy_thunk)),
        ]);

        let json = DatabaseEngineEventEmitterProcessor::serialize_val_for_jsonb(&args);

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
        let execution_context = ExecutionContext::new(
            Uuid::now_v7(),
            Uuid::now_v7(),
            crate::db::run::RunType::Run.as_id(),
            None,
            None,
            None,
            None,
        );
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
}

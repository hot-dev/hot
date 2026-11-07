use super::{Event, EventPublisher, ExecutionContext};
use crate::blob::{BlobScope, BlobStore, SpillSource};
use crate::db::DatabasePool;
use crate::val::Val;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::{RwLock, mpsc, oneshot};

type EventWriteAck = oneshot::Sender<Result<(), String>>;
type EventWrite = (ExecutionContext, Event, Option<EventWriteAck>);

pub struct DatabaseEventPublisher {
    outbox_queue: Arc<std::sync::RwLock<Option<String>>>,
    db: Arc<RwLock<Option<DatabasePool>>>,
    blob_store: Option<Arc<BlobStore>>,
    event_sender: mpsc::UnboundedSender<EventWrite>,
    shutdown_sender: mpsc::UnboundedSender<oneshot::Sender<()>>,
    /// Flag to suppress errors during shutdown (when channel is expected to be closed)
    shutdown_initiated: Arc<AtomicBool>,
}

impl DatabaseEventPublisher {
    pub(crate) fn enable_outbox(&self, queue_name: &str) {
        *self.outbox_queue.write().unwrap() = Some(queue_name.to_string());
    }
    async fn outbox_db(&self, ctx: &ExecutionContext) -> Result<DatabasePool, String> {
        let db = self
            .db
            .read()
            .await
            .clone()
            .ok_or("Database not initialized")?;
        match &db {
            DatabasePool::Postgres(_) => Ok(db.with_scope(crate::db::DatabaseScope::Environment {
                org_id: ctx.org_id.ok_or("Missing org_id for event enqueue")?,
                env_id: ctx.env_id.ok_or("Missing env_id for event enqueue")?,
            })),
            _ => Ok(db),
        }
    }
    pub(crate) async fn pending_message(
        &self,
        ctx: &ExecutionContext,
        event_id: uuid::Uuid,
    ) -> Result<crate::data::msg::Message, String> {
        let db = self.outbox_db(ctx).await?;
        let payload = crate::db::event::Event::enqueue_payload(&db, &event_id)
            .await
            .map_err(|error| error.to_string())?
            .ok_or("Event enqueue packet was not stored")?;
        serde_json::from_value(payload)
            .map_err(|error| format!("Cannot decode event enqueue packet: {error}"))
    }
    pub(crate) async fn finish_enqueue(
        &self,
        ctx: &ExecutionContext,
        event_id: uuid::Uuid,
    ) -> Result<(), String> {
        crate::db::event::Event::finish_enqueue(&self.outbox_db(ctx).await?, &event_id)
            .await
            .map_err(|error| error.to_string())
    }

    pub(crate) async fn release_enqueue(
        &self,
        ctx: &ExecutionContext,
        event_id: uuid::Uuid,
    ) -> Result<(), String> {
        crate::db::event::Event::release_enqueue(
            &self.outbox_db(ctx).await?,
            &event_id,
            chrono::Utc::now(),
        )
        .await
        .map_err(|error| error.to_string())
    }

    /// Create a new DatabaseEventPublisher with an existing database pool (preferred)
    /// This ensures the database connection is ready before events are published
    pub fn new_with_pool(db_pool: DatabasePool) -> Self {
        Self::new_with_pool_and_blob_store(db_pool, None)
    }

    /// Like [`Self::new_with_pool`], but with an optional blob store used to
    /// spill large event payload leaves to content-addressed storage before
    /// the event row insert.
    pub fn new_with_pool_and_blob_store(
        db_pool: DatabasePool,
        blob_store: Option<Arc<BlobStore>>,
    ) -> Self {
        let (event_sender, mut event_receiver) = mpsc::unbounded_channel::<EventWrite>();
        let (shutdown_sender, mut shutdown_receiver) =
            mpsc::unbounded_channel::<oneshot::Sender<()>>();

        let db = Arc::new(RwLock::new(Some(db_pool)));
        let outbox_queue = Arc::new(std::sync::RwLock::new(None));
        let outbox_for_processor = Arc::clone(&outbox_queue);
        let db_for_processor = Arc::clone(&db);
        let blob_store_for_processor = blob_store.clone();

        // Start event processing task
        tokio::spawn(async move {
            let mut processor = DatabaseEventProcessor {
                outbox_queue: outbox_for_processor,
                db: db_for_processor,
                blob_store: blob_store_for_processor,
            };

            loop {
                tokio::select! {
                    event = event_receiver.recv() => {
                        match event {
                            Some((ctx, evt, ack_sender)) => {
                                processor.write_acked(ctx, evt, ack_sender).await;
                            }
                            None => break, // Channel closed
                        }
                    }
                    shutdown = shutdown_receiver.recv() => {
                        if let Some(sender) = shutdown {
                            // Process any remaining events
                            while let Ok((ctx, evt, ack_sender)) = event_receiver.try_recv() {
                                processor.write_acked(ctx, evt, ack_sender).await;
                            }
                            // Signal completion
                            let _ = sender.send(());
                            break;
                        }
                    }
                }
            }
        });

        // Return the publisher
        Self {
            outbox_queue,
            db,
            blob_store,
            event_sender,
            shutdown_sender,
            shutdown_initiated: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Create a new DatabaseEventPublisher
    /// Note: Database connection is initialized asynchronously. Use new_with_pool() for synchronous creation.
    pub fn new(db_uri: String) -> Self {
        let (event_sender, mut event_receiver) = mpsc::unbounded_channel::<EventWrite>();
        let (shutdown_sender, mut shutdown_receiver) =
            mpsc::unbounded_channel::<oneshot::Sender<()>>();

        let db = Arc::new(RwLock::new(None));
        let outbox_queue = Arc::new(std::sync::RwLock::new(None));
        let outbox_for_processor = Arc::clone(&outbox_queue);
        let db_for_processor = Arc::clone(&db);

        // Initialize database connection in the background
        let db_for_init = Arc::clone(&db);
        let db_uri_clone = db_uri.clone();
        tokio::spawn(async move {
            // Create a configuration with the database URI
            let db_conf = crate::val!({
                "uri": db_uri_clone,
            });

            match crate::db::create_db_pool(&db_conf).await {
                Ok(pool) => {
                    tracing::debug!("DatabaseEventPublisher: Database connection established");
                    let mut db_write = db_for_init.write().await;
                    *db_write = Some(pool);
                }
                Err(e) => {
                    tracing::error!(
                        "DatabaseEventPublisher: Failed to initialize database connection: {}",
                        e
                    );
                }
            }
        });

        // Start event processing task
        tokio::spawn(async move {
            let mut processor = DatabaseEventProcessor {
                outbox_queue: outbox_for_processor,
                db: db_for_processor,
                blob_store: None,
            };

            loop {
                tokio::select! {
                    event = event_receiver.recv() => {
                        match event {
                            Some((ctx, evt, ack_sender)) => {
                                processor.write_acked(ctx, evt, ack_sender).await;
                            }
                            None => break, // Channel closed
                        }
                    }
                    shutdown = shutdown_receiver.recv() => {
                        if let Some(sender) = shutdown {
                            // Process any remaining events
                            while let Ok((ctx, evt, ack_sender)) = event_receiver.try_recv() {
                                processor.write_acked(ctx, evt, ack_sender).await;
                            }
                            // Signal completion
                            let _ = sender.send(());
                            break;
                        }
                    }
                }
            }
        });

        // Return the publisher
        Self {
            outbox_queue,
            db,
            blob_store: None,
            event_sender,
            shutdown_sender,
            shutdown_initiated: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Shutdown the event publisher and wait for all events to be processed
    pub async fn shutdown_impl(&self) -> Result<(), String> {
        // Set shutdown flag FIRST to suppress "channel closed" errors from stragglers
        self.shutdown_initiated.store(true, Ordering::SeqCst);

        let (sender, receiver) = oneshot::channel();

        // Send shutdown signal
        self.shutdown_sender.send(sender).map_err(|_| {
            "Failed to send shutdown signal - processor may have already stopped".to_string()
        })?;

        // Wait for completion
        receiver
            .await
            .map_err(|_| "Failed to receive shutdown completion signal".to_string())
    }

    /// Check if shutdown has been initiated
    pub fn is_shutting_down(&self) -> bool {
        self.shutdown_initiated.load(Ordering::SeqCst)
    }
}

struct DatabaseEventProcessor {
    outbox_queue: Arc<std::sync::RwLock<Option<String>>>,
    db: Arc<RwLock<Option<DatabasePool>>>,
    /// When present, large event payload leaves are spilled to blob storage
    /// before the event row insert.
    blob_store: Option<Arc<BlobStore>>,
}

impl DatabaseEventProcessor {
    /// Write an event and report the result to its publisher. A publisher that
    /// stopped waiting has already told `send` the event was not sent, so its
    /// event must not be delivered: skip a write it no longer waits for, and
    /// settle the outbox packet of one that committed after it gave up.
    async fn write_acked(
        &mut self,
        ctx: ExecutionContext,
        event: Event,
        ack: Option<EventWriteAck>,
    ) {
        if ack.as_ref().is_some_and(|ack| ack.is_closed()) {
            tracing::warn!(event_id = %event.event_id, "Skipping an event write its publisher stopped waiting for");
            return;
        }
        let event_id = event.event_id;
        let result = self.process_event(ctx.clone(), event).await;
        let Some(ack) = ack else { return };
        let committed = result.is_ok();
        if ack.send(result).is_err() && committed {
            if let Err(error) = self.settle_unsent(&ctx, event_id).await {
                tracing::error!(%event_id, %error, "Event committed after its publisher timed out, and its outbox packet could not be settled");
            } else {
                tracing::warn!(%event_id, "Event committed after its publisher timed out; it will not be enqueued");
            }
        }
    }

    async fn settle_unsent(
        &self,
        ctx: &ExecutionContext,
        event_id: uuid::Uuid,
    ) -> Result<(), String> {
        let db = self
            .db
            .read()
            .await
            .clone()
            .ok_or("Database not initialized")?;
        let db = match (&db, ctx.org_id, ctx.env_id) {
            (DatabasePool::Postgres(_), Some(org_id), Some(env_id)) => {
                db.with_scope(crate::db::DatabaseScope::Environment { org_id, env_id })
            }
            (DatabasePool::Postgres(_), _, _) => return Err("Missing org_id or env_id".into()),
            (DatabasePool::Sqlite(_), _, _) => db,
        };
        crate::db::event::Event::finish_enqueue(&db, &event_id)
            .await
            .map_err(|error| error.to_string())
    }

    /// Process an event with its execution context
    async fn process_event(&mut self, ctx: ExecutionContext, event: Event) -> Result<(), String> {
        if let Err(e) = self.insert_event(&ctx, &event).await {
            tracing::error!(
                "DatabaseEventPublisher: Failed to insert event {} (type: {}): {}",
                event.event_id,
                event.event_type,
                e
            );
            return Err(e);
        }
        Ok(())
    }

    /// Insert an event into the database
    async fn insert_event(&self, ctx: &ExecutionContext, event: &Event) -> Result<(), String> {
        let db_guard = self.db.read().await;
        if let Some(db) = db_guard.as_ref() {
            // Require both env_id and user_id to be present - no fallbacks
            let env_id = ctx.env_id.ok_or_else(|| {
                "Missing env_id in ExecutionContext - cannot publish event".to_string()
            })?;
            let user_id = ctx.user_id.ok_or_else(|| {
                "Missing user_id in ExecutionContext - cannot publish event".to_string()
            })?;
            // This processor runs in a spawned task that no task-local scope
            // reaches, and the shared handle may be unscoped or cross-tenant:
            // bind every write to the context's exact environment.
            let scoped_db;
            let db = match (db, ctx.org_id) {
                (crate::db::DatabasePool::Postgres(_), Some(org_id)) => {
                    scoped_db =
                        db.with_scope(crate::db::DatabaseScope::Environment { org_id, env_id });
                    &scoped_db
                }
                (crate::db::DatabasePool::Postgres(_), None) => {
                    return Err(
                        "Missing org_id in ExecutionContext - cannot publish event".to_string()
                    );
                }
                (crate::db::DatabasePool::Sqlite(_), _) => db,
            };

            let event_data_val =
                event_data_to_store(self.blob_store.as_deref(), ctx, event).await?;

            let event_data = serde_json::to_value(&event_data_val)
                .map_err(|e| format!("Failed to serialize event data: {}", e))?;

            let outbox_queue = self.outbox_queue.read().unwrap().clone();
            let inserted = if let Some(queue) = outbox_queue {
                let stored_event = Event {
                    event_data: event_data_val,
                    ..event.clone()
                };
                let message = super::queue::durable_event_message(&queue, ctx, stored_event);
                let payload = serde_json::to_value(&message)
                    .map_err(|error| format!("Cannot serialize event outbox: {error}"))?;
                crate::db::event::Event::insert_event_with_outbox(
                    db,
                    &event.event_id,
                    &env_id,
                    &event.stream_id,
                    &event.event_type,
                    &event_data,
                    event.event_time,
                    &user_id,
                    None,
                    &payload,
                )
                .await
                .and_then(|inserted| {
                    if inserted {
                        Ok(())
                    } else {
                        Err(crate::db::event::EventError::AlreadyExists(event.event_id))
                    }
                })
            } else {
                crate::db::event::Event::insert_event(
                    db,
                    &event.event_id,
                    &env_id,
                    &event.stream_id,
                    &event.event_type,
                    &event_data,
                    event.event_time,
                    &user_id,
                    None,
                )
                .await
            };
            if let Err(e) = inserted {
                self.release_unstored_event(db, ctx, event).await;
                return Err(format!("Database error: {}", e));
            }

            Ok(())
        } else {
            Err("Database not initialized".to_string())
        }
    }
}

impl DatabaseEventProcessor {
    /// Release the blobs written for an event whose insert failed, unless the
    /// row made it anyway, as when a connection fails after a commit.
    async fn release_unstored_event(
        &self,
        db: &DatabasePool,
        ctx: &ExecutionContext,
        event: &Event,
    ) {
        let (Some(store), Some(org_id)) = (self.blob_store.as_ref(), ctx.org_id) else {
            return;
        };
        if !matches!(
            crate::db::event::Event::get_event(db, &event.event_id).await,
            Err(crate::db::event::EventError::NotFound)
        ) {
            return;
        }
        let scope = BlobScope {
            org_id,
            env_id: ctx.env_id,
            run_id: None,
        };
        let source_id = event.event_id.to_string();
        if let Err(e) = store
            .release_source(scope, SpillSource::EventData, &source_id)
            .await
        {
            tracing::warn!(event_id = %event.event_id, error = %e, "failed to release an unstored event's blobs");
        }
    }
}

/// Event data as it's stored: its large leaves in blob storage, or all of it if
/// spilling them would leave too much inline or too many references to read
/// back. Event data is executable payload: fail closed on spill errors rather
/// than silently persisting a partially-spilled value.
async fn event_data_to_store(
    blob_store: Option<&BlobStore>,
    ctx: &ExecutionContext,
    event: &Event,
) -> Result<Val, String> {
    let data = &event.event_data;
    data.check_shape("send event")?;
    let (Some(store), Some(org_id)) = (blob_store.filter(|s| s.config().enabled()), ctx.org_id)
    else {
        return Ok(data.clone());
    };
    let config = store.config();
    let threshold = config.spill_threshold_bytes;
    let size = crate::blob::estimate_val_size(data);
    if size < threshold {
        return Ok(data.clone());
    }
    let scope = BlobScope {
        org_id,
        env_id: ctx.env_id,
        run_id: Some(ctx.run_id),
    };
    let source_id = event.event_id.to_string();
    let spill_failed = |e| format!("Failed to spill event data to blob storage: {}", e);
    store.check_readable(size).map_err(spill_failed)?;
    if crate::blob::val_bytes_outside_large_leaves(data, threshold) > config.max_inline_bytes
        || crate::blob::count_large_leaves(data, threshold) > config.rehydrate_max_refs
    {
        let json = serde_json::to_string(data)
            .map_err(|e| format!("Failed to serialize event data: {}", e))?;
        store
            .spill_readable_json(&json, scope, SpillSource::EventData, Some(&source_id))
            .await
    } else {
        store
            .spill_large_val(
                data.clone(),
                scope,
                SpillSource::EventData,
                Some(&source_id),
            )
            .await
    }
    .map_err(spill_failed)
}

/// Maximum time a publisher will wait for the serial database processor to
/// acknowledge an event insert. The processor is a single serial task, so one
/// slow/stuck insert (network outage, database contention) would otherwise
/// wedge every publisher - and the worker's serial event pipeline behind it -
/// indefinitely.
const PUBLISH_ACK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

impl DatabaseEventPublisher {
    /// Publish an event and wait for database write to complete
    /// This blocks until the event has been written to the database, bounded
    /// by [`PUBLISH_ACK_TIMEOUT`].
    pub fn publish_and_wait(&self, ctx: &ExecutionContext, event: Event) -> Result<(), String> {
        self.publish_and_wait_with_timeout(ctx, event, PUBLISH_ACK_TIMEOUT)
    }

    fn publish_and_wait_with_timeout(
        &self,
        ctx: &ExecutionContext,
        event: Event,
        timeout: std::time::Duration,
    ) -> Result<(), String> {
        // Validate that ExecutionContext has required fields
        if ctx.env_id.is_none() {
            return Err("Cannot publish event - missing env_id in ExecutionContext".to_string());
        }
        if ctx.user_id.is_none() {
            return Err("Cannot publish event - missing user_id in ExecutionContext".to_string());
        }

        // Store large parts here, not in the processor: that can take longer
        // than the timeout below, and would hold up every event behind this one.
        let event_data = crate::lang::hot::native_io::block_on_result("send data", async {
            event_data_to_store(self.blob_store.as_deref(), ctx, &event)
                .await
                .map_err(Val::from)
        })
        .map_err(|error| error.to_string())?;
        // What's still inline takes the writer longer the larger it is.
        let timeout =
            crate::blob::timeout_for_size(timeout, crate::blob::estimate_val_size(&event_data));
        let event = Event {
            event_data,
            ..event
        };

        // Create oneshot channel for acknowledgment
        let (ack_sender, ack_receiver) = oneshot::channel();

        // Send event to the sequential processor with acknowledgment channel
        self.event_sender
            .send((ctx.clone(), event, Some(ack_sender)))
            .map_err(|_| {
                "Failed to send event to database processor - channel closed".to_string()
            })?;

        // VM code runs in a blocking context, where `Handle::block_on` is the
        // correct sync-to-async bridge. Do not use `block_in_place` here:
        // this path is called from Hot VM execution, not from async worker code.
        crate::lang::hot::native_io::block_on_result("send commit", async {
            match tokio::time::timeout(timeout, ack_receiver).await {
                Ok(Ok(result)) => result,
                Ok(Err(_)) => Err("Database write acknowledgment was dropped".to_string()),
                Err(_) => Err(format!(
                    "Timed out after {:?} waiting for event database write (writer backlog or \
                     database outage)",
                    timeout
                )),
            }
            .map_err(Val::from)
        })
        .map_err(|error| error.to_string())
    }
}

impl EventPublisher for DatabaseEventPublisher {
    fn publish_checked(&self, ctx: &ExecutionContext, event: Event) -> Result<(), String> {
        self.publish_and_wait(ctx, event)
    }

    fn publish(&self, ctx: &ExecutionContext, event: Event) {
        // Validate that ExecutionContext has required fields before sending to processor
        if ctx.env_id.is_none() {
            tracing::error!(
                "DatabaseEventPublisher: Cannot publish event {} - missing env_id in ExecutionContext",
                event.event_id
            );
            return;
        }
        if ctx.user_id.is_none() {
            tracing::error!(
                "DatabaseEventPublisher: Cannot publish event {} - missing user_id in ExecutionContext",
                event.event_id
            );
            return;
        }

        // Send event to the sequential processor without acknowledgment (fire-and-forget)
        if self.event_sender.send((ctx.clone(), event, None)).is_err() {
            // Only log error if shutdown hasn't been initiated (expected during shutdown)
            if !self.shutdown_initiated.load(Ordering::SeqCst) {
                tracing::error!(
                    "DatabaseEventPublisher: Failed to send event to database processor - channel closed"
                );
            }
        }
    }

    fn shutdown(
        &self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + '_>> {
        Box::pin(self.shutdown_impl())
    }
}

impl Clone for DatabaseEventPublisher {
    fn clone(&self) -> Self {
        Self {
            outbox_queue: Arc::clone(&self.outbox_queue),
            db: Arc::clone(&self.db),
            blob_store: self.blob_store.clone(),
            event_sender: self.event_sender.clone(),
            shutdown_sender: self.shutdown_sender.clone(),
            shutdown_initiated: Arc::clone(&self.shutdown_initiated),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    #[tokio::test]
    async fn test_database_event_publisher_creation() {
        let publisher = DatabaseEventPublisher::new("sqlite::memory:".to_string());

        // Create a test event
        let stream_id = Uuid::now_v7();
        let event = Event::new(
            Uuid::now_v7(),
            stream_id,
            "test_event".to_string(),
            crate::val::Val::from("test_data"),
        );
        let ctx = ExecutionContext::new(
            Uuid::now_v7(),
            stream_id,
            crate::db::run::RunType::Run.as_id(),
            Some(Uuid::now_v7()),
            Some(Uuid::now_v7()),
            Some(Uuid::now_v7()),
            Some(Uuid::now_v7()),
        );

        // This should not panic
        publisher.publish(&ctx, event);
    }

    /// A processor over a test database, and what it takes to store with it.
    async fn spilling_processor(
        configure: impl FnOnce(&mut crate::blob::BlobConfig),
    ) -> (
        DatabaseEventProcessor,
        Arc<BlobStore>,
        crate::db::TestData,
        ExecutionContext,
        tempfile::TempDir,
    ) {
        let db = Arc::new(crate::db::test_db().await);
        let data = crate::db::insert_test_data(&db).await.unwrap();
        let temp_dir = tempfile::TempDir::new().unwrap();
        let storage = Arc::new(crate::file_storage::LocalFileStorage::new(
            temp_dir.path().to_path_buf(),
        ));
        let mut config = crate::blob::BlobConfig {
            mode: crate::blob::BlobMode::Service,
            ..Default::default()
        };
        configure(&mut config);
        let blob_store = Arc::new(BlobStore::new(db.clone(), storage, config));
        let processor = DatabaseEventProcessor {
            outbox_queue: Arc::new(std::sync::RwLock::new(None)),
            db: Arc::new(RwLock::new(Some((*db).clone()))),
            blob_store: Some(blob_store.clone()),
        };
        let ctx = ExecutionContext::new(
            data.run_id,
            data.stream_id,
            crate::db::run::RunType::Run.as_id(),
            Some(data.env_id),
            Some(data.user_id),
            Some(data.org_id),
            Some(data.build_id),
        );
        (processor, blob_store, data, ctx, temp_dir)
    }

    /// Store `event_data`, and check that it's one reference that reads back.
    async fn assert_stored_whole(
        processor: &DatabaseEventProcessor,
        blob_store: &BlobStore,
        data: &crate::db::TestData,
        ctx: &ExecutionContext,
        event_data: Val,
    ) {
        let event = Event::new(
            data.env_id,
            data.stream_id,
            "test_event".to_string(),
            event_data.clone(),
        );
        processor.insert_event(ctx, &event).await.unwrap();
        let db = processor.db.read().await;
        let stored = crate::db::event::Event::get_event(db.as_ref().unwrap(), &event.event_id)
            .await
            .unwrap();
        let stored: Val = serde_json::from_value(stored.event_data).unwrap();
        assert!(crate::blob::is_blob_ref_val(&stored), "{stored:.200?}");
        let scope = BlobScope {
            org_id: data.org_id,
            env_id: Some(data.env_id),
            run_id: None,
        };
        let budget = crate::blob::RehydrateBudget::from_config(blob_store.config());
        let restored = blob_store
            .rehydrate_blob_refs(stored, scope, budget)
            .await
            .unwrap();
        assert_eq!(restored, event_data);
    }

    #[tokio::test]
    async fn an_event_that_cant_be_inserted_releases_its_blob() {
        let (processor, _blob_store, data, ctx, _dir) = spilling_processor(|_| {}).await;
        let rows = (0..40_000i64)
            .map(|i| crate::val!({"id": i, "note": "a small cell"}))
            .collect();
        let event = Event::new(
            data.env_id,
            data.stream_id,
            "test_event".to_string(),
            Val::vec(rows),
        );
        let db = processor.db.read().await;
        let Some(DatabasePool::Sqlite(pool)) = db.as_ref() else {
            panic!("test_db should return SQLite");
        };
        sqlx::query(
            "CREATE TRIGGER refuse_events BEFORE INSERT ON event BEGIN SELECT RAISE(ABORT, 'refused'); END",
        )
        .execute(pool)
        .await
        .unwrap();

        assert!(processor.insert_event(&ctx, &event).await.is_err());

        let (total, active): (i64, i64) = sqlx::query_as(
            "SELECT COUNT(*), COALESCE(SUM(active), 0) FROM blob_ref WHERE source_kind = 'event_data'",
        )
        .fetch_one(pool)
        .await
        .unwrap();
        assert_eq!((total, active), (1, 0));
    }

    #[tokio::test]
    async fn event_data_of_many_small_values_is_stored_whole() {
        let (processor, blob_store, data, ctx, _dir) = spilling_processor(|_| {}).await;
        let rows = (0..40_000i64)
            .map(|i| crate::val!({"id": i, "note": "a small cell"}))
            .collect();
        assert_stored_whole(&processor, &blob_store, &data, &ctx, Val::vec(rows)).await;
    }

    #[tokio::test]
    async fn event_data_with_more_large_leaves_than_can_be_read_back_is_stored_whole() {
        let (processor, blob_store, data, ctx, _dir) = spilling_processor(|config| {
            config.spill_threshold_bytes = 16;
            config.rehydrate_max_refs = 10;
        })
        .await;
        let leaves = (0..11)
            .map(|i| Val::from(format!("a large enough leaf {i}")))
            .collect();
        assert_stored_whole(&processor, &blob_store, &data, &ctx, Val::vec(leaves)).await;
    }

    #[tokio::test]
    async fn event_data_too_large_to_read_back_is_refused_when_sent() {
        let (processor, _blob_store, data, ctx, _dir) = spilling_processor(|config| {
            config.spill_threshold_bytes = 1024;
            config.rehydrate_max_total_bytes = 2048;
        })
        .await;
        for event_data in [
            Val::from("x".repeat(4096)),
            Val::vec(vec![Val::Int(0); 200_000]),
        ] {
            let event = Event::new(
                data.env_id,
                data.stream_id,
                "test_event".to_string(),
                event_data,
            );
            let error = processor.insert_event(&ctx, &event).await.unwrap_err();
            assert!(error.contains("max-total-bytes"), "{error}");
        }
    }

    #[tokio::test]
    async fn test_event_publisher_spills_large_event_data() {
        let db = Arc::new(crate::db::test_db().await);
        let data = crate::db::insert_test_data(&db).await.unwrap();

        let temp_dir = tempfile::TempDir::new().unwrap();
        let storage = Arc::new(crate::file_storage::LocalFileStorage::new(
            temp_dir.path().to_path_buf(),
        ));
        let blob_store = Arc::new(BlobStore::new(
            db.clone(),
            storage,
            crate::blob::BlobConfig {
                mode: crate::blob::BlobMode::Service,
                spill_threshold_bytes: 1024,
                ..Default::default()
            },
        ));

        let processor = DatabaseEventProcessor {
            outbox_queue: Arc::new(std::sync::RwLock::new(None)),
            db: Arc::new(RwLock::new(Some((*db).clone()))),
            blob_store: Some(blob_store.clone()),
        };

        let payload = "e".repeat(4096);
        let event = Event::new(
            data.env_id,
            data.stream_id,
            "test_event".to_string(),
            crate::val!({"body": payload.clone()}),
        );
        let event_id = event.event_id;
        let ctx = ExecutionContext::new(
            data.run_id,
            data.stream_id,
            crate::db::run::RunType::Run.as_id(),
            Some(data.env_id),
            Some(data.user_id),
            Some(data.org_id),
            Some(data.build_id),
        );

        processor.insert_event(&ctx, &event).await.unwrap();

        // The stored row holds a compact BlobRef, not the large payload.
        let stored = crate::db::event::Event::get_event(&db, &event_id)
            .await
            .unwrap();
        let stored_val: crate::val::Val =
            serde_json::from_value(stored.event_data.clone()).unwrap();
        assert!(crate::blob::contains_blob_ref(&stored_val));
        assert!(stored.event_data.to_string().len() < 1024);

        // Rehydration restores the original data.
        let scope = BlobScope {
            org_id: data.org_id,
            env_id: Some(data.env_id),
            run_id: None,
        };
        let rehydrated = blob_store
            .rehydrate_blob_refs(
                stored_val,
                scope,
                crate::blob::RehydrateBudget::from_config(blob_store.config()),
            )
            .await
            .unwrap();
        assert_eq!(rehydrated.get_str("body"), payload);
    }

    /// A wedged database processor must not block publishers forever: the ack
    /// wait times out and returns an error instead of hanging the caller (and
    /// the worker's serial event pipeline behind it).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_publish_and_wait_times_out_when_processor_is_stuck() {
        let (event_sender, event_receiver) = mpsc::unbounded_channel::<EventWrite>();
        let (shutdown_sender, _shutdown_receiver) =
            mpsc::unbounded_channel::<oneshot::Sender<()>>();

        // No processor task drains the channel: simulates a stuck insert.
        let publisher = DatabaseEventPublisher {
            outbox_queue: Arc::new(std::sync::RwLock::new(None)),
            db: Arc::new(RwLock::new(None)),
            blob_store: None,
            event_sender,
            shutdown_sender,
            shutdown_initiated: Arc::new(AtomicBool::new(false)),
        };

        let stream_id = Uuid::now_v7();
        let event = Event::new(
            Uuid::now_v7(),
            stream_id,
            "test_event".to_string(),
            crate::val::Val::from("test_data"),
        );
        let ctx = ExecutionContext::new(
            Uuid::now_v7(),
            stream_id,
            crate::db::run::RunType::Run.as_id(),
            Some(Uuid::now_v7()),
            Some(Uuid::now_v7()),
            Some(Uuid::now_v7()),
            Some(Uuid::now_v7()),
        );

        // publish_and_wait uses Handle::block_on, which is only legal from a
        // blocking (non-core) thread - same as the real VM execution path.
        let result = tokio::task::spawn_blocking(move || {
            publisher.publish_and_wait_with_timeout(
                &ctx,
                event,
                std::time::Duration::from_millis(200),
            )
        })
        .await
        .unwrap();

        let err = result.unwrap_err();
        assert!(err.contains("Timed out"), "unexpected error: {err}");
        drop(event_receiver);
    }
}

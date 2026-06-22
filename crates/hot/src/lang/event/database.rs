use super::{Event, EventPublisher, ExecutionContext};
use crate::db::DatabasePool;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::{RwLock, mpsc, oneshot};

pub struct DatabaseEventPublisher {
    db: Arc<RwLock<Option<DatabasePool>>>,
    event_sender: mpsc::UnboundedSender<(ExecutionContext, Event, Option<oneshot::Sender<()>>)>,
    shutdown_sender: mpsc::UnboundedSender<oneshot::Sender<()>>,
    /// Flag to suppress errors during shutdown (when channel is expected to be closed)
    shutdown_initiated: Arc<AtomicBool>,
}

impl DatabaseEventPublisher {
    /// Create a new DatabaseEventPublisher with an existing database pool (preferred)
    /// This ensures the database connection is ready before events are published
    pub fn new_with_pool(db_pool: DatabasePool) -> Self {
        let (event_sender, mut event_receiver) =
            mpsc::unbounded_channel::<(ExecutionContext, Event, Option<oneshot::Sender<()>>)>();
        let (shutdown_sender, mut shutdown_receiver) =
            mpsc::unbounded_channel::<oneshot::Sender<()>>();

        let db = Arc::new(RwLock::new(Some(db_pool)));
        let db_for_processor = Arc::clone(&db);

        // Start event processing task
        tokio::spawn(async move {
            let mut processor = DatabaseEventProcessor {
                db: db_for_processor,
            };

            loop {
                tokio::select! {
                    event = event_receiver.recv() => {
                        match event {
                            Some((ctx, evt, ack_sender)) => {
                                processor.process_event(ctx, evt).await;
                                // Send acknowledgment if requested
                                if let Some(sender) = ack_sender {
                                    let _ = sender.send(());
                                }
                            }
                            None => break, // Channel closed
                        }
                    }
                    shutdown = shutdown_receiver.recv() => {
                        if let Some(sender) = shutdown {
                            // Process any remaining events
                            while let Ok((ctx, evt, ack_sender)) = event_receiver.try_recv() {
                                processor.process_event(ctx, evt).await;
                                if let Some(ack) = ack_sender {
                                    let _ = ack.send(());
                                }
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
            db,
            event_sender,
            shutdown_sender,
            shutdown_initiated: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Create a new DatabaseEventPublisher
    /// Note: Database connection is initialized asynchronously. Use new_with_pool() for synchronous creation.
    pub fn new(db_uri: String) -> Self {
        let (event_sender, mut event_receiver) =
            mpsc::unbounded_channel::<(ExecutionContext, Event, Option<oneshot::Sender<()>>)>();
        let (shutdown_sender, mut shutdown_receiver) =
            mpsc::unbounded_channel::<oneshot::Sender<()>>();

        let db = Arc::new(RwLock::new(None));
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
                db: db_for_processor,
            };

            loop {
                tokio::select! {
                    event = event_receiver.recv() => {
                        match event {
                            Some((ctx, evt, ack_sender)) => {
                                processor.process_event(ctx, evt).await;
                                // Send acknowledgment if requested
                                if let Some(sender) = ack_sender {
                                    let _ = sender.send(());
                                }
                            }
                            None => break, // Channel closed
                        }
                    }
                    shutdown = shutdown_receiver.recv() => {
                        if let Some(sender) = shutdown {
                            // Process any remaining events
                            while let Ok((ctx, evt, ack_sender)) = event_receiver.try_recv() {
                                processor.process_event(ctx, evt).await;
                                if let Some(ack) = ack_sender {
                                    let _ = ack.send(());
                                }
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
            db,
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
    db: Arc<RwLock<Option<DatabasePool>>>,
}

impl DatabaseEventProcessor {
    /// Process an event with its execution context
    async fn process_event(&mut self, ctx: ExecutionContext, event: Event) {
        if let Err(e) = self.insert_event(&ctx, &event).await {
            tracing::error!(
                "DatabaseEventPublisher: Failed to insert event {} (type: {}): {}",
                event.event_id,
                event.event_type,
                e
            );
        }
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

            let event_data = serde_json::to_value(&event.event_data)
                .map_err(|e| format!("Failed to serialize event data: {}", e))?;

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
            .map_err(|e| format!("Database error: {}", e))?;

            Ok(())
        } else {
            Err("Database not initialized".to_string())
        }
    }
}

impl DatabaseEventPublisher {
    /// Publish an event and wait for database write to complete
    /// This blocks until the event has been written to the database
    pub fn publish_and_wait(&self, ctx: &ExecutionContext, event: Event) -> Result<(), String> {
        // Validate that ExecutionContext has required fields
        if ctx.env_id.is_none() {
            return Err("Cannot publish event - missing env_id in ExecutionContext".to_string());
        }
        if ctx.user_id.is_none() {
            return Err("Cannot publish event - missing user_id in ExecutionContext".to_string());
        }

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
        tokio::runtime::Handle::current().block_on(async {
            ack_receiver
                .await
                .map_err(|_| "Database write acknowledgment was dropped".to_string())
        })
    }
}

impl EventPublisher for DatabaseEventPublisher {
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
            db: Arc::clone(&self.db),
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
}

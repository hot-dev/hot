//! Project-local SQLite queue with in-process fast notifications.
//!
//! SQLite is the authoritative store.  The process-wide notification channel
//! carries only durable message IDs, so a full/lost notification is harmless:
//! consumers periodically inspect SQLite and recover work written by sibling
//! processes.  Each named queue owns a separate, self-managed database below
//! `.hot/db/queue/`; these files deliberately do not use Hot's application
//! database migrations.

use super::{
    Queue, QueueInfrastructureError, QueueProcessingError, QueueProcessor, QueueStatus,
    QueueStatusSummary, queue_timing_enabled, queue_wait_target_p99_ms,
};
use crate::data::serialization::Serialization;
use async_trait::async_trait;
use serde::{Serialize, de::DeserializeOwned};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::{FromRow, SqlitePool};
use std::collections::HashMap;
use std::error::Error;
use std::future::Future;
use std::io::{Read, Write};
use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use thiserror::Error;
use tokio::sync::OnceCell;
use uuid::Uuid;
use zstd::{Decoder, Encoder};

const SCHEMA_VERSION: i64 = 1;
const NOTIFICATION_CAPACITY: usize = 100_000;
const MAX_NOTIFICATION_CLAIM_ATTEMPTS: usize = 64;
const CROSS_PROCESS_POLL_INTERVAL: Duration = Duration::from_millis(50);
const DEFAULT_LEASE_DURATION: Duration = Duration::from_secs(60);
const MIN_LEASE_DURATION: Duration = Duration::from_secs(1);
const MAX_PROCESSING_RETRIES: i64 = 3;
const DLQ_MAX_MESSAGES: i64 = 10_000;
const DROPPED_LEASE_BACKOFF: Duration = Duration::from_millis(100);

const STATE_READY: i64 = 0;
const STATE_LEASED: i64 = 1;
const STATE_DEAD_LETTER: i64 = 2;

#[derive(Debug, Error)]
pub enum SqliteQueueError {
    #[error("queue filesystem error: {0}")]
    Io(#[from] std::io::Error),
    #[error("queue database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("queue serialization error: {0}")]
    Serialization(String),
    #[error(
        "unsupported queue schema version {found} in {path}; this Hot build supports {supported}"
    )]
    UnsupportedSchema {
        path: String,
        found: i64,
        supported: i64,
    },
    #[error("queue file {path} belongs to '{found}', not '{expected}'")]
    QueueNameMismatch {
        path: String,
        expected: String,
        found: String,
    },
    #[error("queue lease for message {0} is no longer owned by this worker")]
    LeaseLost(String),
}

fn duration_ms(duration: Duration) -> u64 {
    duration.as_millis().min(u128::from(u64::MAX)) as u64
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn serialization_id(serialization: Serialization) -> i64 {
    match serialization {
        Serialization::Json => 1,
        Serialization::ZstdJson => 2,
    }
}

fn serialization_from_id(id: i64) -> Result<Serialization, SqliteQueueError> {
    match id {
        1 => Ok(Serialization::Json),
        2 => Ok(Serialization::ZstdJson),
        other => Err(SqliteQueueError::Serialization(format!(
            "unknown serialization format id {other}"
        ))),
    }
}

fn serialize<T: Serialize>(
    item: &T,
    serialization: Serialization,
) -> Result<Vec<u8>, SqliteQueueError> {
    let json =
        serde_json::to_vec(item).map_err(|e| SqliteQueueError::Serialization(e.to_string()))?;
    match serialization {
        Serialization::Json => Ok(json),
        Serialization::ZstdJson => {
            let mut compressed = Vec::new();
            let mut encoder = Encoder::new(&mut compressed, 6)
                .map_err(|e| SqliteQueueError::Serialization(e.to_string()))?;
            encoder
                .write_all(&json)
                .map_err(|e| SqliteQueueError::Serialization(e.to_string()))?;
            encoder
                .finish()
                .map_err(|e| SqliteQueueError::Serialization(e.to_string()))?;
            Ok(compressed)
        }
    }
}

fn deserialize<T: DeserializeOwned>(
    payload: &[u8],
    serialization: Serialization,
) -> Result<T, SqliteQueueError> {
    match serialization {
        Serialization::Json => serde_json::from_slice(payload)
            .map_err(|e| SqliteQueueError::Serialization(e.to_string())),
        Serialization::ZstdJson => {
            let mut json = Vec::new();
            let mut decoder = Decoder::new(payload)
                .map_err(|e| SqliteQueueError::Serialization(e.to_string()))?;
            decoder
                .read_to_end(&mut json)
                .map_err(|e| SqliteQueueError::Serialization(e.to_string()))?;
            serde_json::from_slice(&json)
                .map_err(|e| SqliteQueueError::Serialization(e.to_string()))
        }
    }
}

/// Queue databases are always project-local and are not affected by `db.uri`.
pub fn default_queue_dir() -> PathBuf {
    PathBuf::from(".hot/db/queue")
}

fn queue_file_name(queue_name: &str) -> String {
    let mut slug = String::with_capacity(queue_name.len().min(48));
    let mut previous_dash = false;
    for ch in queue_name.chars().take(48) {
        let mapped = if ch.is_ascii_alphanumeric() || ch == '_' {
            previous_dash = false;
            ch.to_ascii_lowercase()
        } else if !previous_dash {
            previous_dash = true;
            '-'
        } else {
            continue;
        };
        slug.push(mapped);
    }
    let slug = slug.trim_matches('-');
    let slug = if slug.is_empty() { "queue" } else { slug };
    let hash = blake3::hash(queue_name.as_bytes()).to_hex();
    format!("{slug}-{}.sqlite3", &hash[..12])
}

pub fn queue_path(queue_name: &str) -> PathBuf {
    default_queue_dir().join(queue_file_name(queue_name))
}

struct SharedQueue {
    queue_name: String,
    path: PathBuf,
    connect_options: SqliteConnectOptions,
    pool: OnceCell<SqlitePool>,
    notifications_tx: async_channel::Sender<String>,
    notifications_rx: async_channel::Receiver<String>,
    initialized: OnceCell<()>,
}

type SharedRegistry = Mutex<HashMap<PathBuf, Arc<SharedQueue>>>;

fn shared_registry() -> &'static SharedRegistry {
    static REGISTRY: OnceLock<SharedRegistry> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

fn create_shared(queue_name: &str, path: &Path) -> Result<Arc<SharedQueue>, SqliteQueueError> {
    let options = SqliteConnectOptions::from_str(&format!("sqlite://{}", path.display()))?
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Normal)
        .busy_timeout(Duration::from_secs(5))
        .foreign_keys(true);
    let (notifications_tx, notifications_rx) = async_channel::bounded(NOTIFICATION_CAPACITY);

    Ok(Arc::new(SharedQueue {
        queue_name: queue_name.to_string(),
        path: path.to_path_buf(),
        connect_options: options,
        pool: OnceCell::new(),
        notifications_tx,
        notifications_rx,
        initialized: OnceCell::new(),
    }))
}

fn get_or_create_shared(
    queue_name: &str,
    path: PathBuf,
) -> Result<Arc<SharedQueue>, SqliteQueueError> {
    let mut registry = shared_registry()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(shared) = registry.get(&path) {
        if shared.queue_name != queue_name {
            return Err(SqliteQueueError::QueueNameMismatch {
                path: path.display().to_string(),
                expected: queue_name.to_string(),
                found: shared.queue_name.clone(),
            });
        }
        return Ok(Arc::clone(shared));
    }
    let shared = create_shared(queue_name, &path)?;
    registry.insert(path, Arc::clone(&shared));
    Ok(shared)
}

impl SharedQueue {
    async fn ensure_pool(&self) -> &SqlitePool {
        self.pool
            .get_or_init(|| async {
                SqlitePoolOptions::new()
                    .min_connections(0)
                    .max_connections(8)
                    .acquire_timeout(Duration::from_secs(5))
                    .connect_lazy_with(self.connect_options.clone())
            })
            .await
    }

    fn pool(&self) -> &SqlitePool {
        self.pool
            .get()
            .expect("SQLite queue pool used before initialization")
    }

    async fn ensure_initialized(&self) -> Result<(), SqliteQueueError> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let pool = self.ensure_pool().await;
        self.initialized
            .get_or_try_init(|| async {
                // Keep bootstrap atomic. A process killed during schema setup
                // leaves either the complete schema or an empty SQLite file,
                // both of which the next process can safely open.
                let mut tx = pool.begin().await?;
                let version: i64 = sqlx::query_scalar("PRAGMA user_version")
                    .fetch_one(&mut *tx)
                    .await?;
                if version > SCHEMA_VERSION {
                    return Err(SqliteQueueError::UnsupportedSchema {
                        path: self.path.display().to_string(),
                        found: version,
                        supported: SCHEMA_VERSION,
                    });
                }

                sqlx::query(
                    "CREATE TABLE IF NOT EXISTS queue_metadata (\
                        singleton INTEGER PRIMARY KEY CHECK (singleton = 1),\
                        queue_name TEXT NOT NULL,\
                        schema_version INTEGER NOT NULL,\
                        created_at_ms INTEGER NOT NULL\
                    )",
                )
                .execute(&mut *tx)
                .await?;
                sqlx::query(
                    "CREATE TABLE IF NOT EXISTS queue_message (\
                        message_id TEXT PRIMARY KEY NOT NULL,\
                        payload BLOB NOT NULL,\
                        serialization INTEGER NOT NULL,\
                        state INTEGER NOT NULL DEFAULT 0 CHECK (state IN (0, 1, 2)),\
                        created_at_ms INTEGER NOT NULL,\
                        available_at_ms INTEGER NOT NULL,\
                        retry_count INTEGER NOT NULL DEFAULT 0,\
                        redelivered INTEGER NOT NULL DEFAULT 0 CHECK (redelivered IN (0, 1)),\
                        lease_owner TEXT,\
                        lease_token TEXT,\
                        lease_until_ms INTEGER,\
                        last_error TEXT\
                    )",
                )
                .execute(&mut *tx)
                .await?;
                sqlx::query(
                    "CREATE INDEX IF NOT EXISTS queue_message_ready_idx \
                     ON queue_message (state, available_at_ms, created_at_ms, message_id)",
                )
                .execute(&mut *tx)
                .await?;
                sqlx::query(
                    "CREATE INDEX IF NOT EXISTS queue_message_lease_idx \
                     ON queue_message (state, lease_until_ms)",
                )
                .execute(&mut *tx)
                .await?;
                sqlx::query(
                    "CREATE INDEX IF NOT EXISTS queue_message_dlq_idx \
                     ON queue_message (state, created_at_ms)",
                )
                .execute(&mut *tx)
                .await?;

                sqlx::query(
                    "INSERT OR IGNORE INTO queue_metadata \
                     (singleton, queue_name, schema_version, created_at_ms) VALUES (1, ?, ?, ?)",
                )
                .bind(&self.queue_name)
                .bind(SCHEMA_VERSION)
                .bind(now_ms())
                .execute(&mut *tx)
                .await?;
                let stored_name: String =
                    sqlx::query_scalar("SELECT queue_name FROM queue_metadata WHERE singleton = 1")
                        .fetch_one(&mut *tx)
                        .await?;
                if stored_name != self.queue_name {
                    return Err(SqliteQueueError::QueueNameMismatch {
                        path: self.path.display().to_string(),
                        expected: self.queue_name.clone(),
                        found: stored_name,
                    });
                }
                sqlx::query("PRAGMA user_version = 1")
                    .execute(&mut *tx)
                    .await?;
                tx.commit().await?;
                Ok(())
            })
            .await
            .map(|_| ())
    }

    fn notify(&self, message_id: String) {
        match self.notifications_tx.try_send(message_id) {
            Ok(()) | Err(async_channel::TrySendError::Full(_)) => {}
            Err(async_channel::TrySendError::Closed(_)) => {
                tracing::debug!(queue = self.queue_name, "SQLite queue notifier is closed");
            }
        }
    }
}

#[derive(Debug, FromRow)]
struct StoredMessage {
    message_id: String,
    payload: Vec<u8>,
    serialization: i64,
    created_at_ms: i64,
    retry_count: i64,
    redelivered: i64,
    lease_token: String,
}

pub struct SqliteQueue<T> {
    shared: Arc<SharedQueue>,
    serialization: Serialization,
    consumer_name: String,
    lease_duration: Duration,
    poll_interval: Duration,
    startup_window: Option<Duration>,
    _item: PhantomData<fn() -> T>,
}

impl<T> Clone for SqliteQueue<T> {
    fn clone(&self) -> Self {
        Self {
            shared: Arc::clone(&self.shared),
            serialization: self.serialization,
            consumer_name: self.consumer_name.clone(),
            lease_duration: self.lease_duration,
            poll_interval: self.poll_interval,
            startup_window: self.startup_window,
            _item: PhantomData,
        }
    }
}

impl<T> SqliteQueue<T> {
    pub fn new(queue_name: String) -> Result<Self, SqliteQueueError> {
        Self::new_at(queue_name, default_queue_dir())
    }

    pub fn new_at(queue_name: String, directory: PathBuf) -> Result<Self, SqliteQueueError> {
        let path = directory.join(queue_file_name(&queue_name));
        let shared = get_or_create_shared(&queue_name, path)?;
        Ok(Self {
            shared,
            serialization: Serialization::default(),
            consumer_name: format!("{}-{}", std::process::id(), Uuid::now_v7()),
            lease_duration: DEFAULT_LEASE_DURATION,
            poll_interval: CROSS_PROCESS_POLL_INTERVAL,
            startup_window: None,
            _item: PhantomData,
        })
    }

    pub fn with_serialization(mut self, serialization: Serialization) -> Self {
        self.serialization = serialization;
        self
    }

    pub fn with_consumer_name(mut self, consumer_name: String) -> Self {
        self.consumer_name = consumer_name;
        self
    }

    pub fn with_orphan_idle_ms(mut self, orphan_idle_ms: u64) -> Self {
        self.lease_duration = Duration::from_millis(orphan_idle_ms).max(MIN_LEASE_DURATION);
        self
    }

    pub fn with_startup_window(mut self, startup_window: Duration) -> Self {
        self.startup_window = Some(startup_window);
        self
    }

    pub fn path(&self) -> &Path {
        &self.shared.path
    }

    fn lease_until_ms(&self) -> i64 {
        now_ms().saturating_add(self.lease_duration.as_millis().min(i64::MAX as u128) as i64)
    }

    async fn trim_dead_letters(&self) -> Result<(), SqliteQueueError> {
        sqlx::query(
            "DELETE FROM queue_message WHERE state = 2 AND message_id NOT IN (\
                SELECT message_id FROM queue_message WHERE state = 2 \
                ORDER BY created_at_ms DESC, message_id DESC LIMIT ?\
             )",
        )
        .bind(DLQ_MAX_MESSAGES)
        .execute(self.shared.pool())
        .await?;
        Ok(())
    }

    pub async fn recover_orphaned_items(&self) -> Result<usize, SqliteQueueError> {
        Ok(self.recover_orphaned_items_with_data().await?.0)
    }

    pub async fn recover_orphaned_items_with_data(
        &self,
    ) -> Result<(usize, Vec<Vec<u8>>), SqliteQueueError> {
        self.shared.ensure_initialized().await?;
        let current_ms = now_ms();
        sqlx::query(
            "UPDATE queue_message SET state = 2, retry_count = retry_count + 1, \
             redelivered = 1, last_error = COALESCE(last_error, 'lease expired after retry limit'), \
             lease_owner = NULL, lease_token = NULL, lease_until_ms = NULL \
             WHERE state = 1 AND lease_until_ms <= ? AND retry_count + 1 >= ?",
        )
        .bind(current_ms)
        .bind(MAX_PROCESSING_RETRIES)
        .execute(self.shared.pool())
        .await?;
        let rows: Vec<(String, Vec<u8>)> = sqlx::query_as(
            "UPDATE queue_message SET state = 0, retry_count = retry_count + 1, redelivered = 1, \
             available_at_ms = ?, lease_owner = NULL, lease_token = NULL, lease_until_ms = NULL \
             WHERE state = 1 AND lease_until_ms <= ? AND retry_count + 1 < ? \
             RETURNING message_id, payload",
        )
        .bind(current_ms)
        .bind(current_ms)
        .bind(MAX_PROCESSING_RETRIES)
        .fetch_all(self.shared.pool())
        .await?;
        for (message_id, _) in &rows {
            self.shared.notify(message_id.clone());
        }
        self.trim_dead_letters().await?;
        let payloads = rows
            .into_iter()
            .map(|(_, payload)| payload)
            .collect::<Vec<_>>();
        Ok((payloads.len(), payloads))
    }

    pub async fn purge_old_pending(&self, max_age_ms: u64) -> Result<usize, SqliteQueueError> {
        self.shared.ensure_initialized().await?;
        let cutoff = now_ms().saturating_sub(max_age_ms.min(i64::MAX as u64) as i64);
        let result =
            sqlx::query("DELETE FROM queue_message WHERE state IN (0, 1) AND created_at_ms < ?")
                .bind(cutoff)
                .execute(self.shared.pool())
                .await?;
        Ok(result.rows_affected() as usize)
    }

    pub async fn fast_forward_if_stale(&self) -> Result<usize, SqliteQueueError> {
        match self.startup_window {
            Some(window) => self.purge_old_pending(duration_ms(window)).await,
            None => Ok(0),
        }
    }

    pub async fn consumer_has_pending(&self) -> Result<bool, SqliteQueueError> {
        self.shared.ensure_initialized().await?;
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM queue_message WHERE state = 1 AND lease_owner = ?",
        )
        .bind(&self.consumer_name)
        .fetch_one(self.shared.pool())
        .await?;
        Ok(count > 0)
    }

    pub async fn unregister_consumer(&self) -> Result<(), SqliteQueueError> {
        self.shared.ensure_initialized().await?;
        let ids: Vec<(String,)> = sqlx::query_as(
            "UPDATE queue_message SET state = 0, redelivered = 1, available_at_ms = ?, \
             lease_owner = NULL, lease_token = NULL, lease_until_ms = NULL \
             WHERE state = 1 AND lease_owner = ? RETURNING message_id",
        )
        .bind(now_ms())
        .bind(&self.consumer_name)
        .fetch_all(self.shared.pool())
        .await?;
        for (message_id,) in ids {
            self.shared.notify(message_id);
        }
        Ok(())
    }
}

impl<T> SqliteQueue<T>
where
    T: Send + Sync + Serialize + DeserializeOwned + Clone + 'static,
{
    async fn claim_id(
        &self,
        message_id: Option<&str>,
    ) -> Result<Option<SqliteQueueLease<T>>, SqliteQueueError> {
        self.shared.ensure_initialized().await?;
        let current_ms = now_ms();

        // A lease that expired after exhausting its retry allowance moves to
        // the DLQ before another worker can acquire it.
        sqlx::query(
            "UPDATE queue_message SET state = ?, last_error = COALESCE(last_error, 'lease expired after retry limit'), \
             lease_owner = NULL, lease_token = NULL, lease_until_ms = NULL \
             WHERE state = ? AND lease_until_ms <= ? AND retry_count + 1 >= ?",
        )
        .bind(STATE_DEAD_LETTER)
        .bind(STATE_LEASED)
        .bind(current_ms)
        .bind(MAX_PROCESSING_RETRIES)
        .execute(self.shared.pool())
        .await?;

        let lease_token = Uuid::now_v7().to_string();
        let row = if let Some(message_id) = message_id {
            sqlx::query_as::<_, StoredMessage>(
                "UPDATE queue_message SET \
                    retry_count = retry_count + CASE WHEN state = 1 THEN 1 ELSE 0 END, \
                    redelivered = CASE WHEN state = 1 OR retry_count > 0 THEN 1 ELSE redelivered END, \
                    state = 1, lease_owner = ?, lease_token = ?, lease_until_ms = ? \
                 WHERE message_id = ? AND (\
                    (state = 0 AND available_at_ms <= ?) OR \
                    (state = 1 AND lease_until_ms <= ? AND retry_count + 1 < ?)\
                 ) \
                 RETURNING message_id, payload, serialization, created_at_ms, retry_count, redelivered, lease_token",
            )
            .bind(&self.consumer_name)
            .bind(&lease_token)
            .bind(self.lease_until_ms())
            .bind(message_id)
            .bind(current_ms)
            .bind(current_ms)
            .bind(MAX_PROCESSING_RETRIES)
            .fetch_optional(self.shared.pool())
            .await?
        } else {
            sqlx::query_as::<_, StoredMessage>(
                "UPDATE queue_message SET \
                    retry_count = retry_count + CASE WHEN state = 1 THEN 1 ELSE 0 END, \
                    redelivered = CASE WHEN state = 1 OR retry_count > 0 THEN 1 ELSE redelivered END, \
                    state = 1, lease_owner = ?, lease_token = ?, lease_until_ms = ? \
                 WHERE message_id = (\
                    SELECT message_id FROM queue_message WHERE \
                        (state = 0 AND available_at_ms <= ?) OR \
                        (state = 1 AND lease_until_ms <= ? AND retry_count + 1 < ?) \
                    ORDER BY created_at_ms, message_id LIMIT 1\
                 ) \
                 RETURNING message_id, payload, serialization, created_at_ms, retry_count, redelivered, lease_token",
            )
            .bind(&self.consumer_name)
            .bind(&lease_token)
            .bind(self.lease_until_ms())
            .bind(current_ms)
            .bind(current_ms)
            .bind(MAX_PROCESSING_RETRIES)
            .fetch_optional(self.shared.pool())
            .await?
        };

        Ok(row.map(|row| {
            let claimed_at = chrono::Utc::now();
            let enqueued_at = chrono::DateTime::from_timestamp_millis(row.created_at_ms);
            let queue_wait = enqueued_at
                .and_then(|created| claimed_at.signed_duration_since(created).to_std().ok())
                .unwrap_or_default();
            SqliteQueueLease {
                queue: self.clone(),
                message_id: Some(row.message_id),
                payload: Some(row.payload),
                serialization: row.serialization,
                retry_count: row.retry_count,
                redelivered: row.redelivered != 0,
                lease_token: row.lease_token,
                claimed_at,
                enqueued_at,
                queue_wait,
                completed: false,
            }
        }))
    }

    pub async fn claim_now(&self) -> Result<Option<SqliteQueueLease<T>>, SqliteQueueError> {
        // IDs can become stale when a sibling process claims the durable row.
        // Cap per-call ID lookups so a large stale notification backlog cannot
        // delay the authoritative indexed claim below.
        for _ in 0..MAX_NOTIFICATION_CLAIM_ATTEMPTS {
            match self.shared.notifications_rx.try_recv() {
                Ok(message_id) => {
                    if let Some(lease) = self.claim_id(Some(&message_id)).await? {
                        return Ok(Some(lease));
                    }
                }
                Err(_) => break,
            }
        }
        self.claim_id(None).await
    }

    pub async fn claim_blocking(&self) -> Result<Option<SqliteQueueLease<T>>, SqliteQueueError> {
        loop {
            if let Some(lease) = self.claim_now().await? {
                return Ok(Some(lease));
            }
            tokio::select! {
                notification = self.shared.notifications_rx.recv() => {
                    if let Ok(message_id) = notification
                        && let Some(lease) = self.claim_id(Some(&message_id)).await?
                    {
                        return Ok(Some(lease));
                    }
                }
                _ = tokio::time::sleep(self.poll_interval) => {}
            }
        }
    }

    pub async fn process_blocking<F, Fut, R>(
        &self,
        worker: F,
    ) -> Result<Option<R>, Box<dyn Error + Send + Sync>>
    where
        F: FnOnce(T) -> Fut + Send,
        Fut: Future<Output = Result<R, Box<dyn Error + Send + Sync>>> + Send,
        R: Send + Sync,
    {
        match self.claim_blocking().await? {
            Some(lease) => lease.process(worker).await,
            None => Ok(None),
        }
    }

    async fn renew_lease(
        &self,
        message_id: &str,
        lease_token: &str,
    ) -> Result<bool, SqliteQueueError> {
        let result = sqlx::query(
            "UPDATE queue_message SET lease_until_ms = ? \
             WHERE message_id = ? AND state = 1 AND lease_token = ?",
        )
        .bind(self.lease_until_ms())
        .bind(message_id)
        .bind(lease_token)
        .execute(self.shared.pool())
        .await?;
        Ok(result.rows_affected() == 1)
    }

    async fn acknowledge(
        &self,
        message_id: &str,
        lease_token: &str,
    ) -> Result<(), SqliteQueueError> {
        let result = sqlx::query(
            "DELETE FROM queue_message WHERE message_id = ? AND state = 1 AND lease_token = ?",
        )
        .bind(message_id)
        .bind(lease_token)
        .execute(self.shared.pool())
        .await?;
        if result.rows_affected() != 1 {
            return Err(SqliteQueueError::LeaseLost(message_id.to_string()));
        }
        Ok(())
    }

    async fn release_for_retry(
        &self,
        message_id: &str,
        lease_token: &str,
        reason: &str,
        backoff: Duration,
        count_retry: bool,
    ) -> Result<(), SqliteQueueError> {
        let retry_increment = i64::from(count_retry);
        let available_at =
            now_ms().saturating_add(backoff.as_millis().min(i64::MAX as u128) as i64);
        let result = sqlx::query(
            "UPDATE queue_message SET \
                retry_count = retry_count + ?, \
                state = CASE WHEN retry_count + ? >= ? THEN ? ELSE ? END, \
                available_at_ms = ?, redelivered = 1, last_error = ?, \
                lease_owner = NULL, lease_token = NULL, lease_until_ms = NULL \
             WHERE message_id = ? AND state = 1 AND lease_token = ?",
        )
        .bind(retry_increment)
        .bind(retry_increment)
        .bind(MAX_PROCESSING_RETRIES)
        .bind(STATE_DEAD_LETTER)
        .bind(STATE_READY)
        .bind(available_at)
        .bind(reason)
        .bind(message_id)
        .bind(lease_token)
        .execute(self.shared.pool())
        .await?;
        if result.rows_affected() != 1 {
            return Err(SqliteQueueError::LeaseLost(message_id.to_string()));
        }
        self.shared.notify(message_id.to_string());
        self.trim_dead_letters().await?;
        Ok(())
    }

    async fn dead_letter_existing(
        &self,
        message_id: &str,
        lease_token: &str,
        reason: &str,
    ) -> Result<(), SqliteQueueError> {
        let result = sqlx::query(
            "UPDATE queue_message SET state = 2, last_error = ?, \
             lease_owner = NULL, lease_token = NULL, lease_until_ms = NULL \
             WHERE message_id = ? AND state = 1 AND lease_token = ?",
        )
        .bind(reason)
        .bind(message_id)
        .bind(lease_token)
        .execute(self.shared.pool())
        .await?;
        if result.rows_affected() != 1 {
            return Err(SqliteQueueError::LeaseLost(message_id.to_string()));
        }
        self.trim_dead_letters().await
    }

    async fn release_dropped_lease(
        &self,
        message_id: &str,
        lease_token: &str,
    ) -> Result<(), SqliteQueueError> {
        self.release_for_retry(
            message_id,
            lease_token,
            "handler dropped before completion (panic or cancellation)",
            DROPPED_LEASE_BACKOFF,
            true,
        )
        .await
    }
}

pub struct SqliteQueueLease<T>
where
    T: Send + Sync + Serialize + DeserializeOwned + Clone + 'static,
{
    queue: SqliteQueue<T>,
    message_id: Option<String>,
    payload: Option<Vec<u8>>,
    serialization: i64,
    retry_count: i64,
    redelivered: bool,
    lease_token: String,
    claimed_at: chrono::DateTime<chrono::Utc>,
    enqueued_at: Option<chrono::DateTime<chrono::Utc>>,
    queue_wait: Duration,
    completed: bool,
}

impl<T> SqliteQueueLease<T>
where
    T: Send + Sync + Serialize + DeserializeOwned + Clone + 'static,
{
    pub fn timing(&self) -> super::QueueLeaseTiming {
        super::QueueLeaseTiming {
            claimed_at: self.claimed_at,
            enqueued_at: self.enqueued_at,
            queue_wait: self.queue_wait,
            redelivered: self.redelivered,
        }
    }

    pub async fn process<F, Fut, R>(
        mut self,
        worker: F,
    ) -> Result<Option<R>, Box<dyn Error + Send + Sync>>
    where
        F: FnOnce(T) -> Fut + Send,
        Fut: Future<Output = Result<R, Box<dyn Error + Send + Sync>>> + Send,
        R: Send + Sync,
    {
        let message_id = self
            .message_id
            .as_ref()
            .cloned()
            .expect("SQLite queue lease already completed");
        let payload = self
            .payload
            .take()
            .expect("SQLite queue lease already completed");
        let serialization = serialization_from_id(self.serialization)?;
        let item: T = match deserialize(&payload, serialization) {
            Ok(item) => item,
            Err(error) => {
                self.queue
                    .dead_letter_existing(
                        &message_id,
                        &self.lease_token,
                        &format!("deserialization error: {error}"),
                    )
                    .await?;
                self.message_id.take();
                self.completed = true;
                return Err(Box::new(error));
            }
        };

        let queue = self.queue.clone();
        let renew_message_id = message_id.clone();
        let renew_token = self.lease_token.clone();
        let renewal_interval = (self.queue.lease_duration / 3).max(Duration::from_millis(250));
        let (stop_tx, mut stop_rx) = tokio::sync::oneshot::channel::<()>();
        let renewal = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = tokio::time::sleep(renewal_interval) => {
                        match queue.renew_lease(&renew_message_id, &renew_token).await {
                            Ok(true) => {}
                            Ok(false) => break,
                            Err(error) => {
                                tracing::warn!(
                                    queue = queue.shared.queue_name,
                                    message_id = renew_message_id,
                                    "Failed to renew SQLite queue lease: {}",
                                    error
                                );
                            }
                        }
                    }
                    _ = &mut stop_rx => break,
                }
            }
        });

        let wait_target_p99_ms = queue_wait_target_p99_ms();
        let processing_started = Instant::now();
        let result = worker(item).await;
        let _ = stop_tx.send(());
        let _ = renewal.await;

        let outcome = match result {
            Ok(value) => {
                self.queue
                    .acknowledge(&message_id, &self.lease_token)
                    .await?;
                if queue_timing_enabled() {
                    tracing::info!(
                        target: "hot::queue::timing",
                        queue = %self.queue.shared.queue_name,
                        backend = "sqlite",
                        delivery_source = if self.redelivered { "retry" } else { "fresh" },
                        queue_wait_ms = duration_ms(self.queue_wait),
                        wait_target_p99_ms,
                        processing_ms = duration_ms(processing_started.elapsed()),
                        retry_count = self.retry_count,
                        outcome = "success",
                        message_id = %message_id,
                        "queue item processed"
                    );
                }
                Ok(Some(value))
            }
            Err(error) => {
                if let Some(infra) = error.downcast_ref::<QueueInfrastructureError>() {
                    let reason = infra.to_string();
                    let count_retry =
                        super::streams::infrastructure_requeue_preserves_delivery_count(&reason);
                    self.queue
                        .release_for_retry(
                            &message_id,
                            &self.lease_token,
                            &reason,
                            infra.backoff(),
                            count_retry,
                        )
                        .await?;
                    Err(Box::new(QueueProcessingError::QueueError(error))
                        as Box<dyn Error + Send + Sync>)
                } else {
                    self.queue
                        .release_for_retry(
                            &message_id,
                            &self.lease_token,
                            &error.to_string(),
                            Duration::ZERO,
                            true,
                        )
                        .await?;
                    Err(Box::new(QueueProcessingError::WorkerError(error))
                        as Box<dyn Error + Send + Sync>)
                }
            }
        };

        self.message_id.take();
        self.completed = true;
        outcome
    }
}

impl<T> Drop for SqliteQueueLease<T>
where
    T: Send + Sync + Serialize + DeserializeOwned + Clone + 'static,
{
    fn drop(&mut self) {
        if self.completed {
            return;
        }
        let Some(message_id) = self.message_id.take() else {
            return;
        };
        let queue = self.queue.clone();
        let lease_token = self.lease_token.clone();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                if let Err(error) = queue.release_dropped_lease(&message_id, &lease_token).await {
                    tracing::warn!(
                        queue = queue.shared.queue_name,
                        message_id,
                        "Failed to release dropped SQLite queue lease: {}",
                        error
                    );
                }
            });
        }
    }
}

#[async_trait]
impl<T> Queue<T> for SqliteQueue<T>
where
    T: Send + Sync + Serialize + DeserializeOwned + 'static,
{
    async fn enqueue(&self, item: T) -> Result<(), Box<dyn Error + Send + Sync>> {
        self.shared.ensure_initialized().await?;
        let message_id = Uuid::now_v7().to_string();
        let payload = serialize(&item, self.serialization)?;
        let created_at = now_ms();
        sqlx::query(
            "INSERT INTO queue_message (\
                message_id, payload, serialization, state, created_at_ms, available_at_ms\
             ) VALUES (?, ?, ?, 0, ?, ?)",
        )
        .bind(&message_id)
        .bind(payload)
        .bind(serialization_id(self.serialization))
        .bind(created_at)
        .bind(created_at)
        .execute(self.shared.pool())
        .await?;
        self.shared.notify(message_id);
        Ok(())
    }

    async fn dequeue(&self) -> Result<Option<T>, Box<dyn Error + Send + Sync>> {
        self.shared.ensure_initialized().await?;
        let row: Option<(Vec<u8>, i64)> = sqlx::query_as(
            "DELETE FROM queue_message WHERE message_id = (\
                SELECT message_id FROM queue_message WHERE state = 0 AND available_at_ms <= ? \
                ORDER BY created_at_ms, message_id LIMIT 1\
             ) RETURNING payload, serialization",
        )
        .bind(now_ms())
        .fetch_optional(self.shared.pool())
        .await?;
        row.map(|(payload, format)| {
            let serialization = serialization_from_id(format)?;
            deserialize(&payload, serialization)
        })
        .transpose()
        .map_err(|e| Box::new(e) as Box<dyn Error + Send + Sync>)
    }

    async fn len(&self) -> Result<usize, Box<dyn Error + Send + Sync>> {
        self.shared.ensure_initialized().await?;
        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM queue_message WHERE state IN (0, 1)")
                .fetch_one(self.shared.pool())
                .await?;
        Ok(count.max(0) as usize)
    }

    async fn move_to_dead_letter_queue(
        &self,
        item: T,
        reason: String,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        self.shared.ensure_initialized().await?;
        let payload = serialize(&item, self.serialization)?;
        let created_at = now_ms();
        sqlx::query(
            "INSERT INTO queue_message (\
                message_id, payload, serialization, state, created_at_ms, available_at_ms, last_error\
             ) VALUES (?, ?, ?, 2, ?, ?, ?)",
        )
        .bind(Uuid::now_v7().to_string())
        .bind(payload)
        .bind(serialization_id(self.serialization))
        .bind(created_at)
        .bind(created_at)
        .bind(reason)
        .execute(self.shared.pool())
        .await?;
        self.trim_dead_letters().await?;
        Ok(())
    }
}

#[async_trait]
impl<T> QueueProcessor<T> for SqliteQueue<T>
where
    T: Send + Sync + Serialize + DeserializeOwned + Clone + 'static,
{
    async fn dequeue_and_work<F, Fut, R>(
        &self,
        worker: F,
    ) -> Result<Option<R>, Box<dyn Error + Send + Sync>>
    where
        F: FnOnce(T) -> Fut + Send,
        Fut: Future<Output = Result<R, Box<dyn Error + Send + Sync>>> + Send,
        R: Send + Sync,
    {
        match self.claim_now().await? {
            Some(lease) => lease.process(worker).await,
            None => Ok(None),
        }
    }
}

pub struct SqliteQueueAdmin {
    directory: PathBuf,
}

/// A durable queue row captured before a dev-session reset.
///
/// Callers can decode the row and reconcile application database state before
/// deleting the queue. This deliberately exposes no SQLite implementation
/// details beyond the owning queue and persisted serialization format.
#[derive(Debug, Clone)]
pub struct SqliteQueueMessageSnapshot {
    pub queue_name: String,
    pub state: SqliteQueueMessageState,
    message_id: String,
    queue_path: PathBuf,
    payload: Vec<u8>,
    serialization: Serialization,
}

impl SqliteQueueMessageSnapshot {
    pub fn decode<T: DeserializeOwned>(&self) -> Result<T, SqliteQueueError> {
        deserialize(&self.payload, self.serialization)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SqliteQueueMessageState {
    Ready,
    Leased,
    DeadLetter,
}

impl SqliteQueueMessageState {
    fn from_id(id: i64) -> Result<Self, SqliteQueueError> {
        match id {
            STATE_READY => Ok(Self::Ready),
            STATE_LEASED => Ok(Self::Leased),
            STATE_DEAD_LETTER => Ok(Self::DeadLetter),
            other => Err(SqliteQueueError::Serialization(format!(
                "unknown queue message state id {other}"
            ))),
        }
    }
}

impl Default for SqliteQueueAdmin {
    fn default() -> Self {
        Self::new(default_queue_dir())
    }
}

impl SqliteQueueAdmin {
    pub fn new(directory: PathBuf) -> Self {
        Self { directory }
    }

    fn queue_files(&self) -> Result<Vec<PathBuf>, SqliteQueueError> {
        if !self.directory.exists() {
            return Ok(Vec::new());
        }
        let mut files = std::fs::read_dir(&self.directory)?
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "sqlite3"))
            .collect::<Vec<_>>();
        files.sort();
        Ok(files)
    }

    async fn open_existing(path: &Path) -> Result<SqlitePool, SqliteQueueError> {
        let options = SqliteConnectOptions::from_str(&format!("sqlite://{}", path.display()))?
            .create_if_missing(false)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Normal)
            .busy_timeout(Duration::from_secs(5));
        Ok(SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await?)
    }

    fn is_resettable_database_error(error: &SqliteQueueError) -> bool {
        matches!(
            error,
            SqliteQueueError::Database(sqlx::Error::Database(database))
                if matches!(database.code().as_deref(), Some("11" | "26"))
        )
    }

    fn remove_queue_file(path: &Path) -> Result<(), SqliteQueueError> {
        for candidate in [
            path.to_path_buf(),
            PathBuf::from(format!("{}-wal", path.display())),
            PathBuf::from(format!("{}-shm", path.display())),
        ] {
            match std::fs::remove_file(candidate) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }

    async fn prepare_schema(pool: &SqlitePool, path: &Path) -> Result<bool, SqliteQueueError> {
        let version: i64 = sqlx::query_scalar("PRAGMA user_version")
            .fetch_one(pool)
            .await?;
        if version > SCHEMA_VERSION {
            return Err(SqliteQueueError::UnsupportedSchema {
                path: path.display().to_string(),
                found: version,
                supported: SCHEMA_VERSION,
            });
        }

        let tables: Vec<String> = sqlx::query_scalar(
            "SELECT name FROM sqlite_master WHERE type = 'table' \
             AND name IN ('queue_metadata', 'queue_message')",
        )
        .fetch_all(pool)
        .await?;
        let has_metadata = tables.iter().any(|table| table == "queue_metadata");
        let has_messages = tables.iter().any(|table| table == "queue_message");

        let mut metadata_schema_version = None;
        let complete = if has_metadata && has_messages {
            let metadata_columns: Vec<String> =
                sqlx::query_scalar("SELECT name FROM pragma_table_info('queue_metadata')")
                    .fetch_all(pool)
                    .await?;
            let message_columns: Vec<String> =
                sqlx::query_scalar("SELECT name FROM pragma_table_info('queue_message')")
                    .fetch_all(pool)
                    .await?;
            let columns_complete = ["singleton", "queue_name", "schema_version", "created_at_ms"]
                .iter()
                .all(|column| metadata_columns.iter().any(|found| found == column))
                && [
                    "message_id",
                    "payload",
                    "serialization",
                    "state",
                    "created_at_ms",
                    "available_at_ms",
                    "retry_count",
                    "redelivered",
                    "lease_owner",
                    "lease_token",
                    "lease_until_ms",
                    "last_error",
                ]
                .iter()
                .all(|column| message_columns.iter().any(|found| found == column));
            if columns_complete {
                metadata_schema_version = sqlx::query_scalar(
                    "SELECT schema_version FROM queue_metadata WHERE singleton = 1",
                )
                .fetch_optional(pool)
                .await?;
            }
            columns_complete && metadata_schema_version.is_some()
        } else {
            false
        };
        if let Some(found) = metadata_schema_version
            && found > SCHEMA_VERSION
        {
            return Err(SqliteQueueError::UnsupportedSchema {
                path: path.display().to_string(),
                found,
                supported: SCHEMA_VERSION,
            });
        }
        if complete {
            return Ok(true);
        }

        // An empty or partially initialized managed file contains no queue
        // state we can safely interpret. Reset its private schema to empty;
        // the next real queue connection will atomically bootstrap it.
        let mut tx = pool.begin().await?;
        sqlx::query("DROP TABLE IF EXISTS queue_message")
            .execute(&mut *tx)
            .await?;
        sqlx::query("DROP TABLE IF EXISTS queue_metadata")
            .execute(&mut *tx)
            .await?;
        sqlx::query("PRAGMA user_version = 0")
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(false)
    }

    async fn open_prepared(path: &Path) -> Result<Option<SqlitePool>, SqliteQueueError> {
        let pool = match Self::open_existing(path).await {
            Ok(pool) => pool,
            Err(error) if Self::is_resettable_database_error(&error) => {
                Self::remove_queue_file(path)?;
                return Ok(None);
            }
            Err(error) => return Err(error),
        };
        match Self::prepare_schema(&pool, path).await {
            Ok(true) => Ok(Some(pool)),
            Ok(false) => {
                pool.close().await;
                Ok(None)
            }
            Err(error) if Self::is_resettable_database_error(&error) => {
                pool.close().await;
                Self::remove_queue_file(path)?;
                Ok(None)
            }
            Err(error) => {
                pool.close().await;
                Err(error)
            }
        }
    }

    /// Snapshot every persisted message without changing queue state.
    ///
    /// `hot dev` uses this before clearing its session-scoped queues so it can
    /// first mark interrupted Runs and Tasks terminal in the application DB.
    pub async fn message_snapshots(
        &self,
    ) -> Result<Vec<SqliteQueueMessageSnapshot>, SqliteQueueError> {
        let mut messages = Vec::new();
        for path in self.queue_files()? {
            let Some(pool) = Self::open_prepared(&path).await? else {
                continue;
            };
            let queue_name: Option<String> =
                sqlx::query_scalar("SELECT queue_name FROM queue_metadata WHERE singleton = 1")
                    .fetch_optional(&pool)
                    .await?;
            if let Some(queue_name) = queue_name {
                let rows: Vec<(String, Vec<u8>, i64, i64)> = sqlx::query_as(
                    "SELECT message_id, payload, serialization, state FROM queue_message \
                     ORDER BY created_at_ms, message_id",
                )
                .fetch_all(&pool)
                .await?;
                for (message_id, payload, serialization, state) in rows {
                    messages.push(SqliteQueueMessageSnapshot {
                        queue_name: queue_name.clone(),
                        state: SqliteQueueMessageState::from_id(state)?,
                        message_id,
                        queue_path: path.clone(),
                        payload,
                        serialization: serialization_from_id(serialization)?,
                    });
                }
            }
            pool.close().await;
        }
        Ok(messages)
    }

    /// Delete exactly the rows returned by [`Self::message_snapshots`].
    /// Messages enqueued after the snapshot boundary are intentionally kept.
    pub async fn clear_snapshots(
        &self,
        snapshots: &[SqliteQueueMessageSnapshot],
    ) -> Result<Vec<String>, SqliteQueueError> {
        let mut by_file: HashMap<PathBuf, (String, Vec<String>)> = HashMap::new();
        for snapshot in snapshots {
            let entry = by_file
                .entry(snapshot.queue_path.clone())
                .or_insert_with(|| (snapshot.queue_name.clone(), Vec::new()));
            entry.1.push(snapshot.message_id.clone());
        }

        let mut files = by_file.into_iter().collect::<Vec<_>>();
        files.sort_by(|left, right| left.0.cmp(&right.0));
        let mut cleared = Vec::with_capacity(files.len());
        for (path, (queue_name, message_ids)) in files {
            let Some(pool) = Self::open_prepared(&path).await? else {
                continue;
            };
            let mut tx = pool.begin().await?;
            for message_id in message_ids {
                sqlx::query("DELETE FROM queue_message WHERE message_id = ?")
                    .bind(message_id)
                    .execute(&mut *tx)
                    .await?;
            }
            tx.commit().await?;
            pool.close().await;
            cleared.push(queue_name);
        }
        Ok(cleared)
    }

    pub async fn clear_all(&self) -> Result<Vec<String>, SqliteQueueError> {
        let mut cleared = Vec::new();
        for path in self.queue_files()? {
            let Some(pool) = Self::open_prepared(&path).await? else {
                continue;
            };
            let queue_name: Option<String> =
                sqlx::query_scalar("SELECT queue_name FROM queue_metadata WHERE singleton = 1")
                    .fetch_optional(&pool)
                    .await?;
            sqlx::query("DELETE FROM queue_message")
                .execute(&pool)
                .await?;
            if let Some(queue_name) = queue_name {
                cleared.push(queue_name);
            }
            pool.close().await;
        }
        Ok(cleared)
    }

    pub async fn status(&self) -> Result<QueueStatusSummary, SqliteQueueError> {
        let mut summary = QueueStatusSummary::default();
        for path in self.queue_files()? {
            let Some(pool) = Self::open_prepared(&path).await? else {
                continue;
            };
            let queue_name: Option<String> =
                sqlx::query_scalar("SELECT queue_name FROM queue_metadata WHERE singleton = 1")
                    .fetch_optional(&pool)
                    .await?;
            if let Some(name) = queue_name {
                let pending: i64 =
                    sqlx::query_scalar("SELECT COUNT(*) FROM queue_message WHERE state = 0")
                        .fetch_one(&pool)
                        .await?;
                let processing: i64 =
                    sqlx::query_scalar("SELECT COUNT(*) FROM queue_message WHERE state = 1")
                        .fetch_one(&pool)
                        .await?;
                let deadletter: i64 =
                    sqlx::query_scalar("SELECT COUNT(*) FROM queue_message WHERE state = 2")
                        .fetch_one(&pool)
                        .await?;
                summary.total_pending += pending;
                summary.total_processing += processing;
                summary.total_deadletter += deadletter;
                summary.queues.push(QueueStatus {
                    name,
                    pending,
                    processing,
                    deadletter,
                });
            }
            pool.close().await;
        }
        summary
            .queues
            .sort_by(|left, right| left.name.cmp(&right.name));
        Ok(summary)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn test_queue<T>(directory: &Path, name: &str) -> SqliteQueue<T> {
        SqliteQueue::new_at(name.to_string(), directory.to_path_buf())
            .expect("test SQLite queue should construct")
            .with_serialization(Serialization::Json)
    }

    fn isolated_queue<T>(directory: &Path, name: &str) -> SqliteQueue<T> {
        let path = directory.join(queue_file_name(name));
        SqliteQueue {
            shared: create_shared(name, &path).expect("isolated SQLite queue should construct"),
            serialization: Serialization::Json,
            consumer_name: format!("test-{}", Uuid::now_v7()),
            lease_duration: DEFAULT_LEASE_DURATION,
            poll_interval: Duration::from_millis(10),
            startup_window: None,
            _item: PhantomData,
        }
    }

    #[tokio::test]
    async fn manages_queue_file_and_schema() {
        let temp = tempfile::tempdir().unwrap();
        let queue = test_queue::<String>(temp.path(), "hot:event");

        queue.enqueue("hello".to_string()).await.unwrap();

        assert!(queue.path().starts_with(temp.path()));
        assert!(queue.path().exists());
        let version: i64 = sqlx::query_scalar("PRAGMA user_version")
            .fetch_one(queue.shared.pool())
            .await
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        let stored_name: String =
            sqlx::query_scalar("SELECT queue_name FROM queue_metadata WHERE singleton = 1")
                .fetch_one(queue.shared.pool())
                .await
                .unwrap();
        assert_eq!(stored_name, "hot:event");
        assert_eq!(queue.dequeue().await.unwrap(), Some("hello".to_string()));
    }

    #[tokio::test]
    async fn constructor_defers_queue_directory_creation_until_first_use() {
        let temp = tempfile::tempdir().unwrap();
        let directory = temp.path().join("not-created-yet");
        let queue = test_queue::<String>(&directory, "lazy-filesystem");
        assert!(!directory.exists());

        queue.enqueue("hello".to_string()).await.unwrap();
        assert!(directory.exists());
    }

    #[tokio::test]
    async fn rejects_a_queue_file_from_a_newer_schema() {
        let temp = tempfile::tempdir().unwrap();
        let queue = isolated_queue::<String>(temp.path(), "future-schema");
        sqlx::query("PRAGMA user_version = 2")
            .execute(queue.shared.ensure_pool().await)
            .await
            .unwrap();

        let error = queue.len().await.unwrap_err().to_string();
        assert!(error.contains("unsupported queue schema version 2"));
    }

    #[tokio::test]
    async fn admin_clear_all_removes_ready_leased_and_dead_letter_messages() {
        let temp = tempfile::tempdir().unwrap();
        let event_queue = test_queue::<String>(temp.path(), "hot:event");
        let task_queue = test_queue::<String>(temp.path(), "hot:task");

        event_queue.enqueue("leased".to_string()).await.unwrap();
        event_queue.enqueue("ready".to_string()).await.unwrap();
        let leased = event_queue.claim_now().await.unwrap().unwrap();
        task_queue
            .move_to_dead_letter_queue("failed".to_string(), "test".to_string())
            .await
            .unwrap();

        let admin = SqliteQueueAdmin::new(temp.path().to_path_buf());
        let before = admin.status().await.unwrap();
        assert_eq!(before.total_pending, 1);
        assert_eq!(before.total_processing, 1);
        assert_eq!(before.total_deadletter, 1);

        let snapshots = admin.message_snapshots().await.unwrap();
        assert_eq!(snapshots.len(), 3);
        assert!(snapshots.iter().any(|message| {
            message.queue_name == "hot:event"
                && message.state == SqliteQueueMessageState::Ready
                && matches!(message.decode::<String>().as_deref(), Ok("ready"))
        }));
        assert!(snapshots.iter().any(|message| {
            message.queue_name == "hot:event"
                && message.state == SqliteQueueMessageState::Leased
                && matches!(message.decode::<String>().as_deref(), Ok("leased"))
        }));
        assert!(snapshots.iter().any(|message| {
            message.queue_name == "hot:task"
                && message.state == SqliteQueueMessageState::DeadLetter
                && matches!(message.decode::<String>().as_deref(), Ok("failed"))
        }));

        let cleared = admin.clear_all().await.unwrap();
        assert!(cleared.contains(&"hot:event".to_string()));
        assert!(cleared.contains(&"hot:task".to_string()));
        let after = admin.status().await.unwrap();
        assert_eq!(after.total_pending, 0);
        assert_eq!(after.total_processing, 0);
        assert_eq!(after.total_deadletter, 0);

        drop(leased);
    }

    #[tokio::test]
    async fn admin_self_heals_empty_and_partially_initialized_files() {
        let temp = tempfile::tempdir().unwrap();
        let empty_name = "empty-bootstrap";
        let empty_path = temp.path().join(queue_file_name(empty_name));
        std::fs::File::create(&empty_path).unwrap();

        let partial_name = "partial-bootstrap";
        let partial_path = temp.path().join(queue_file_name(partial_name));
        let partial = SqlitePoolOptions::new()
            .max_connections(1)
            .connect(&format!("sqlite://{}?mode=rwc", partial_path.display()))
            .await
            .unwrap();
        sqlx::query("CREATE TABLE queue_metadata (singleton INTEGER PRIMARY KEY)")
            .execute(&partial)
            .await
            .unwrap();
        partial.close().await;

        let admin = SqliteQueueAdmin::new(temp.path().to_path_buf());
        assert!(admin.message_snapshots().await.unwrap().is_empty());

        for name in [empty_name, partial_name] {
            let queue = test_queue::<String>(temp.path(), name);
            queue.enqueue("recovered".to_string()).await.unwrap();
            assert_eq!(queue.dequeue().await.unwrap().as_deref(), Some("recovered"));
        }
    }

    #[tokio::test]
    async fn admin_self_heals_corrupt_managed_file() {
        let temp = tempfile::tempdir().unwrap();
        let name = "corrupt-bootstrap";
        let path = temp.path().join(queue_file_name(name));
        std::fs::write(&path, b"not a sqlite database").unwrap();

        let admin = SqliteQueueAdmin::new(temp.path().to_path_buf());
        assert!(admin.message_snapshots().await.unwrap().is_empty());
        assert!(!path.exists());

        let queue = test_queue::<String>(temp.path(), name);
        queue.enqueue("recovered".to_string()).await.unwrap();
        assert_eq!(queue.dequeue().await.unwrap().as_deref(), Some("recovered"));
    }

    #[tokio::test]
    async fn snapshot_clear_preserves_messages_enqueued_after_boundary() {
        let temp = tempfile::tempdir().unwrap();
        let queue = test_queue::<String>(temp.path(), "reset-boundary");
        let admin = SqliteQueueAdmin::new(temp.path().to_path_buf());

        queue.enqueue("previous session".to_string()).await.unwrap();
        let snapshots = admin.message_snapshots().await.unwrap();
        queue.enqueue("new session".to_string()).await.unwrap();

        admin.clear_snapshots(&snapshots).await.unwrap();
        assert_eq!(
            queue.dequeue().await.unwrap().as_deref(),
            Some("new session")
        );
        assert!(queue.dequeue().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn same_process_notification_wakes_blocking_claim_immediately() {
        let temp = tempfile::tempdir().unwrap();
        let producer = test_queue::<String>(temp.path(), "fast-wake");
        let mut consumer = producer.clone();
        consumer.poll_interval = Duration::from_secs(5);

        let waiter = tokio::spawn(async move { consumer.claim_blocking().await.unwrap().unwrap() });
        tokio::time::sleep(Duration::from_millis(10)).await;
        producer.enqueue("ready".to_string()).await.unwrap();

        let lease = tokio::time::timeout(Duration::from_millis(250), waiter)
            .await
            .expect("in-process notification should avoid poll latency")
            .unwrap();
        let value = lease
            .process(|item| async move { Ok::<_, Box<dyn Error + Send + Sync>>(item) })
            .await
            .unwrap();
        assert_eq!(value, Some("ready".to_string()));
    }

    #[tokio::test]
    async fn independent_process_view_discovers_persisted_message_by_polling() {
        let temp = tempfile::tempdir().unwrap();
        let producer = isolated_queue::<String>(temp.path(), "cross-process");
        let consumer = isolated_queue::<String>(temp.path(), "cross-process");

        // The independent shared state models another process: its notification
        // channel is not visible to the consumer.
        assert_eq!(consumer.shared.notifications_rx.len(), 0);
        let waiter = tokio::spawn(async move { consumer.claim_blocking().await.unwrap().unwrap() });
        tokio::time::sleep(Duration::from_millis(25)).await;
        producer.enqueue("from sibling".to_string()).await.unwrap();

        let lease = tokio::time::timeout(Duration::from_millis(300), waiter)
            .await
            .expect("fallback polling should discover sibling-process work")
            .unwrap();
        let value = lease
            .process(|item| async move { Ok::<_, Box<dyn Error + Send + Sync>>(item) })
            .await
            .unwrap();
        assert_eq!(value, Some("from sibling".to_string()));
    }

    #[tokio::test]
    async fn concurrent_claimers_execute_message_once() {
        let temp = tempfile::tempdir().unwrap();
        let queue = test_queue::<usize>(temp.path(), "atomic-claim");
        queue.enqueue(7).await.unwrap();

        let (left, right) = tokio::join!(queue.claim_now(), queue.claim_now());
        let claims = usize::from(left.unwrap().is_some()) + usize::from(right.unwrap().is_some());
        assert_eq!(claims, 1);
    }

    #[tokio::test]
    async fn worker_failures_exhaust_into_dead_letter_state() {
        let temp = tempfile::tempdir().unwrap();
        let queue = test_queue::<String>(temp.path(), "retry-dlq");
        queue.enqueue("poison".to_string()).await.unwrap();

        for _ in 0..MAX_PROCESSING_RETRIES {
            let result = queue
                .dequeue_and_work(|_| async {
                    Err::<(), _>(
                        Box::new(std::io::Error::other("boom")) as Box<dyn Error + Send + Sync>
                    )
                })
                .await;
            assert!(result.is_err());
        }

        assert!(queue.claim_now().await.unwrap().is_none());
        let state: i64 = sqlx::query_scalar("SELECT state FROM queue_message LIMIT 1")
            .fetch_one(queue.shared.pool())
            .await
            .unwrap();
        assert_eq!(state, STATE_DEAD_LETTER);
    }

    #[tokio::test]
    async fn dropped_lease_is_released_for_redelivery() {
        let temp = tempfile::tempdir().unwrap();
        let queue = test_queue::<String>(temp.path(), "drop-release");
        queue.enqueue("again".to_string()).await.unwrap();
        let lease = queue.claim_now().await.unwrap().unwrap();
        drop(lease);

        let lease = tokio::time::timeout(Duration::from_secs(1), queue.claim_blocking())
            .await
            .expect("dropped lease should be made ready")
            .unwrap()
            .unwrap();
        assert!(lease.timing().redelivered);
        lease
            .process(|_| async { Ok::<_, Box<dyn Error + Send + Sync>>(()) })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn panicking_handler_is_paced_and_eventually_dead_lettered() {
        let temp = tempfile::tempdir().unwrap();
        let queue = test_queue::<String>(temp.path(), "panic-dlq");
        queue.enqueue("panic".to_string()).await.unwrap();

        for attempt in 1..=MAX_PROCESSING_RETRIES {
            let worker_queue = queue.clone();
            let panicked = tokio::spawn(async move {
                worker_queue
                    .dequeue_and_work(|_| async move {
                        panic!("deterministic handler panic");
                        #[allow(unreachable_code)]
                        Ok::<(), Box<dyn Error + Send + Sync>>(())
                    })
                    .await
            })
            .await;
            assert!(panicked.is_err_and(|error| error.is_panic()));

            tokio::time::timeout(Duration::from_secs(1), async {
                loop {
                    let row: (i64, i64) =
                        sqlx::query_as("SELECT state, retry_count FROM queue_message LIMIT 1")
                            .fetch_one(queue.shared.pool())
                            .await
                            .unwrap();
                    let expected_state = if attempt == MAX_PROCESSING_RETRIES {
                        STATE_DEAD_LETTER
                    } else {
                        STATE_READY
                    };
                    if row == (expected_state, attempt) {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("dropped panic lease should be retried or dead-lettered");

            if attempt < MAX_PROCESSING_RETRIES {
                tokio::time::sleep(DROPPED_LEASE_BACKOFF).await;
            }
        }

        assert!(queue.claim_now().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn cancelled_handler_consumes_a_paced_retry() {
        let temp = tempfile::tempdir().unwrap();
        let queue = test_queue::<String>(temp.path(), "cancel-retry");
        queue.enqueue("cancel".to_string()).await.unwrap();
        let claimed = Arc::new(tokio::sync::Notify::new());
        let worker_queue = queue.clone();
        let worker_claimed = Arc::clone(&claimed);
        let task = tokio::spawn(async move {
            worker_queue
                .dequeue_and_work(|_| async move {
                    worker_claimed.notify_one();
                    std::future::pending::<Result<(), Box<dyn Error + Send + Sync>>>().await
                })
                .await
        });
        claimed.notified().await;
        task.abort();
        let _ = task.await;

        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let row: (i64, i64) =
                    sqlx::query_as("SELECT state, retry_count FROM queue_message LIMIT 1")
                        .fetch_one(queue.shared.pool())
                        .await
                        .unwrap();
                if row == (STATE_READY, 1) {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("cancelled lease should consume one retry");

        assert!(queue.claim_now().await.unwrap().is_none());
        tokio::time::sleep(DROPPED_LEASE_BACKOFF).await;
        assert!(queue.claim_now().await.unwrap().is_some());
    }

    #[tokio::test]
    async fn multiple_consumers_drain_without_duplicates() {
        let temp = tempfile::tempdir().unwrap();
        let queue = test_queue::<usize>(temp.path(), "parallel-drain");
        const ITEMS: usize = 40;
        for item in 0..ITEMS {
            queue.enqueue(item).await.unwrap();
        }

        let seen = Arc::new(AtomicUsize::new(0));
        let mut workers = Vec::new();
        for _ in 0..4 {
            let queue = queue.clone();
            let seen = Arc::clone(&seen);
            workers.push(tokio::spawn(async move {
                while let Some(lease) = queue.claim_now().await.unwrap() {
                    lease
                        .process(|_| async {
                            seen.fetch_add(1, Ordering::SeqCst);
                            Ok::<_, Box<dyn Error + Send + Sync>>(())
                        })
                        .await
                        .unwrap();
                }
            }));
        }
        for worker in workers {
            worker.await.unwrap();
        }

        assert_eq!(seen.load(Ordering::SeqCst), ITEMS);
        assert_eq!(queue.len().await.unwrap(), 0);
    }
}

//! Project-local SQLite queue with in-process fast notifications.
//!
//! SQLite is the authoritative store.  The process-wide notification channel
//! carries only durable message IDs, so a full/lost notification is harmless:
//! consumers periodically inspect SQLite and recover work written by sibling
//! processes.  Each named queue owns a separate, self-managed database below
//! `.hot/db/queue/`; these files deliberately do not use Hot's application
//! database migrations.

use super::{
    OrphanRecovery, Queue, QueueInfrastructureError, QueueProcessingError, QueueProcessor,
    QueueStatus, QueueStatusSummary, RecoveredQueueMessage, queue_timing_enabled,
    queue_wait_target_p99_ms,
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
use std::sync::atomic::{AtomicI64, Ordering as AtomicOrdering};
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
const SCHEMA_INIT_MAX_ATTEMPTS: usize = 5;
const SCHEMA_INIT_RETRY_DELAY: Duration = Duration::from_millis(25);
/// How often an initialized queue checks that its tables still exist.
const SCHEMA_RECHECK_INTERVAL_MS: i64 = 1_000;
const FILE_REMOVE_MAX_ATTEMPTS: usize = 20;
const FILE_REMOVE_RETRY_DELAY: Duration = Duration::from_millis(10);
const FILE_REMOVE_MAX_RETRY_DELAY: Duration = Duration::from_millis(100);

const STATE_READY: i64 = 0;
const STATE_LEASED: i64 = 1;
const STATE_DEAD_LETTER: i64 = 2;

/// `last_error` of a message recovered from a lease whose holder stopped
/// without settling it. Its next delivery is `recovered`.
const RECOVERED_MARK: &str = "[recovered: its worker stopped while processing it]";
/// `last_error` of a delivery its worker processes alone (see
/// `SqliteQueue::with_crash_quarantine`). Its next delivery is `recovered`
/// too.
const PROCESSING_ALONE_MARK: &str = "[processing alone in its worker]";

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

fn is_transient_database_lock(error: &SqliteQueueError) -> bool {
    let SqliteQueueError::Database(sqlx::Error::Database(database)) = error else {
        return false;
    };
    database
        .code()
        .as_deref()
        .and_then(|code| code.parse::<i32>().ok())
        .is_some_and(|code| matches!(code & 0xff, 5 | 6))
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

fn recovered_messages(
    requeued: impl IntoIterator<Item = (Vec<u8>, i64)>,
    dead_lettered: impl IntoIterator<Item = (Vec<u8>, i64)>,
) -> Result<OrphanRecovery, SqliteQueueError> {
    let decode = |(payload, serialization): (Vec<u8>, i64)| {
        Ok::<_, SqliteQueueError>(RecoveredQueueMessage::new(
            payload,
            serialization_from_id(serialization)?,
        ))
    };
    let requeued_messages = requeued
        .into_iter()
        .map(decode)
        .collect::<Result<Vec<_>, _>>()?;
    let dead_lettered_messages = dead_lettered
        .into_iter()
        .map(decode)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(OrphanRecovery {
        requeued_count: requeued_messages.len(),
        requeued_messages,
        dead_lettered_messages,
    })
}

/// This process's host as lease recovery compares it: its host name and, on
/// Linux, its PID namespace, so a process of another container on the same
/// host is never taken for one of this host's. Empty when unknown.
fn process_host() -> &'static str {
    static HOST: OnceLock<String> = OnceLock::new();
    HOST.get_or_init(|| {
        let host = host_name();
        #[cfg(target_os = "linux")]
        let host = match std::fs::read_link("/proc/self/ns/pid") {
            Ok(namespace) if !host.is_empty() => {
                format!("{host}/{}", namespace.to_string_lossy())
            }
            _ => String::new(),
        };
        host
    })
}

fn host_name() -> String {
    #[cfg(unix)]
    {
        let mut buffer = [0u8; 256];
        // SAFETY: the buffer is valid for writes of its whole length.
        if unsafe { libc::gethostname(buffer.as_mut_ptr().cast(), buffer.len()) } == 0 {
            let end = buffer.iter().position(|byte| *byte == 0).unwrap_or(0);
            return String::from_utf8_lossy(&buffer[..end]).into_owned();
        }
    }
    String::new()
}

/// Whether this host's process `process_id` no longer exists. Unknown counts
/// as alive.
fn process_gone(process_id: i64) -> bool {
    #[cfg(unix)]
    {
        let Ok(pid) = libc::pid_t::try_from(process_id) else {
            return false;
        };
        if pid <= 0 {
            return false;
        }
        // SAFETY: signal 0 only checks that the process exists.
        if unsafe { libc::kill(pid, 0) } == 0 {
            return false;
        }
        std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
    }
    #[cfg(not(unix))]
    {
        let _ = process_id;
        false
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
    /// When the schema was last initialized or seen in place (see
    /// `ensure_initialized`).
    schema_checked_at_ms: AtomicI64,
    /// `queue_metadata.created_at_ms` as last read: it changes when the file
    /// is initialized again, by this process or another.
    schema_created_at_ms: AtomicI64,
    /// Consumers of this process already recorded in `queue_consumer`.
    registered_consumers: Mutex<std::collections::HashSet<String>>,
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
        schema_checked_at_ms: AtomicI64::new(0),
        schema_created_at_ms: AtomicI64::new(0),
        registered_consumers: Mutex::new(std::collections::HashSet::new()),
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
                self.initialize_schema_with_retry(pool).await?;
                self.forget_consumers_of_a_replaced_schema(pool).await;
                Ok::<_, SqliteQueueError>(())
            })
            .await?;
        // The tables can vanish under an initialized queue: a `hot` that
        // found the file before its first initialization finished resets it.
        // Initialize it again rather than fail every operation until restart.
        if self.schema_check_due() {
            if !self.schema_present(pool).await? {
                tracing::warn!(
                    queue = self.queue_name,
                    "SQLite queue tables are gone; initializing the queue file again"
                );
                self.initialize_schema_with_retry(pool).await?;
            }
            self.forget_consumers_of_a_replaced_schema(pool).await;
        }
        Ok(())
    }

    /// The new `queue_consumer` table of a file initialized again, here or
    /// by another process, holds none of this process's consumers: forget
    /// that they were recorded, so `register_consumer` records them again.
    async fn forget_consumers_of_a_replaced_schema(&self, pool: &SqlitePool) {
        let created_at_ms: Result<Option<i64>, _> =
            sqlx::query_scalar("SELECT created_at_ms FROM queue_metadata WHERE singleton = 1")
                .fetch_optional(pool)
                .await;
        // Unreadable: the next check reads it again.
        let Ok(Some(created_at_ms)) = created_at_ms else {
            return;
        };
        let seen = self
            .schema_created_at_ms
            .swap(created_at_ms, AtomicOrdering::Relaxed);
        if seen != created_at_ms {
            self.registered_consumers
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clear();
        }
    }

    /// Remember that `consumer_name` was recorded in `queue_consumer`, by a
    /// write begun while the schema was `schema_created_at_ms`. A schema
    /// replaced since may not hold that row, so the write is then forgotten
    /// and `register_consumer` records the consumer again. The check is made
    /// under the set's lock: `forget_consumers_of_a_replaced_schema` moves
    /// the schema on before it clears the set, so a stale name either fails
    /// the check here or is cleared there.
    fn note_consumer_registered(&self, consumer_name: &str, schema_created_at_ms: i64) -> bool {
        let mut registered = self
            .registered_consumers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.schema_created_at_ms.load(AtomicOrdering::Relaxed) != schema_created_at_ms {
            return false;
        }
        registered.insert(consumer_name.to_string());
        true
    }

    async fn initialize_schema_with_retry(
        &self,
        pool: &SqlitePool,
    ) -> Result<(), SqliteQueueError> {
        for attempt in 0..SCHEMA_INIT_MAX_ATTEMPTS {
            match self.initialize_schema(pool).await {
                Ok(()) => {
                    self.schema_checked_at_ms
                        .store(now_ms(), AtomicOrdering::Relaxed);
                    return Ok(());
                }
                Err(error)
                    if is_transient_database_lock(&error)
                        && attempt + 1 < SCHEMA_INIT_MAX_ATTEMPTS =>
                {
                    tokio::time::sleep(SCHEMA_INIT_RETRY_DELAY).await;
                }
                Err(error) => return Err(error),
            }
        }
        unreachable!("schema initialization retry loop always returns")
    }

    /// Whether to check now that the tables still exist: once per
    /// `SCHEMA_RECHECK_INTERVAL_MS`, by one caller.
    fn schema_check_due(&self) -> bool {
        let now = now_ms();
        let last = self.schema_checked_at_ms.load(AtomicOrdering::Relaxed);
        now.saturating_sub(last) >= SCHEMA_RECHECK_INTERVAL_MS
            && self
                .schema_checked_at_ms
                .compare_exchange(last, now, AtomicOrdering::Relaxed, AtomicOrdering::Relaxed)
                .is_ok()
    }

    async fn schema_present(&self, pool: &SqlitePool) -> Result<bool, SqliteQueueError> {
        let tables: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' \
             AND name IN ('queue_metadata', 'queue_message', 'queue_consumer')",
        )
        .fetch_one(pool)
        .await?;
        Ok(tables == 3)
    }

    async fn initialize_schema(&self, pool: &SqlitePool) -> Result<(), SqliteQueueError> {
        // Acquire the write reservation before reading schema state. A deferred
        // transaction can deadlock with another process when both readers try
        // to upgrade during first-use bootstrap.
        let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
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
        // Which process holds a consumer's leases, so recovery can take the
        // leases of a process that is gone before they expire. Older builds
        // ignore the table.
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS queue_consumer (\
                        consumer_name TEXT PRIMARY KEY NOT NULL,\
                        process_host TEXT NOT NULL,\
                        process_id INTEGER NOT NULL,\
                        registered_at_ms INTEGER NOT NULL\
                    )",
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
    last_error: Option<String>,
}

pub struct SqliteQueue<T> {
    shared: Arc<SharedQueue>,
    serialization: Serialization,
    consumer_name: String,
    lease_duration: Duration,
    poll_interval: Duration,
    startup_window: Option<Duration>,
    crash_quarantine: bool,
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
            crash_quarantine: self.crash_quarantine,
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
            crash_quarantine: false,
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

    /// For a consumer that processes the deliveries of stopped workers alone
    /// (`SqliteQueueLease::recovered`, `SoleDeliveryMarker`): a worker that
    /// stops kills every delivery it holds, but only one it processed alone
    /// is known to have stopped it. So a lease whose holder stopped counts
    /// toward the message's retry limit only when its delivery was marked as
    /// processing alone; the others are requeued for another attempt.
    /// Expired leases are then recovered only through
    /// `recover_orphaned_items*`, never taken over by a claim, so the caller
    /// can settle a recovered message before it runs again.
    pub fn with_crash_quarantine(mut self) -> Self {
        self.crash_quarantine = true;
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
        Ok(self.recover_orphaned_items_with_data().await?.total_count())
    }

    pub async fn recover_orphaned_items_with_data(
        &self,
    ) -> Result<OrphanRecovery, SqliteQueueError> {
        self.recover_orphaned_items_then(|_| async {}).await
    }

    /// Recover the messages of leases whose holders stopped without settling
    /// them: expired leases and, with crash quarantine, the leases of this
    /// host's processes that are gone. A message is dead-lettered once its
    /// retries are spent; otherwise it is requeued, marked `recovered`.
    /// `settle` sees the recovery before a requeued message can be claimed
    /// again, so it can prepare the message's next delivery.
    pub async fn recover_orphaned_items_then<F, Fut>(
        &self,
        settle: F,
    ) -> Result<OrphanRecovery, SqliteQueueError>
    where
        F: FnOnce(&OrphanRecovery) -> Fut,
        Fut: Future<Output = ()>,
    {
        self.shared.ensure_initialized().await?;
        if self.crash_quarantine {
            self.expire_leases_of_gone_processes().await?;
        }
        let current_ms = now_ms();
        let recovery_token = Uuid::now_v7().to_string();
        let mut tx = self.shared.pool().begin().await?;
        // With crash quarantine only a delivery processed alone counts its
        // worker's stop: the others were bystanders.
        let dead_lettered: Vec<(Vec<u8>, i64)> = if self.crash_quarantine {
            sqlx::query_as(
                "UPDATE queue_message SET state = 2, retry_count = retry_count + 1, \
                 redelivered = 1, last_error = ?, \
                 lease_owner = NULL, lease_token = NULL, lease_until_ms = NULL \
                 WHERE state = 1 AND lease_until_ms <= ? AND IFNULL(last_error, '') = ? \
                 AND retry_count + 1 >= ? \
                 RETURNING payload, serialization",
            )
            .bind(format!(
                "its worker stopped each time it processed it alone, {MAX_PROCESSING_RETRIES} times"
            ))
            .bind(current_ms)
            .bind(PROCESSING_ALONE_MARK)
            .bind(MAX_PROCESSING_RETRIES)
            .fetch_all(&mut *tx)
            .await?
        } else {
            sqlx::query_as(
                "UPDATE queue_message SET state = 2, retry_count = retry_count + 1, \
                 redelivered = 1, last_error = COALESCE(last_error, 'lease expired after retry limit'), \
                 lease_owner = NULL, lease_token = NULL, lease_until_ms = NULL \
                 WHERE state = 1 AND lease_until_ms <= ? AND retry_count + 1 >= ? \
                 RETURNING payload, serialization",
            )
            .bind(current_ms)
            .bind(MAX_PROCESSING_RETRIES)
            .fetch_all(&mut *tx)
            .await?
        };
        // Requeued messages stay leased to the recovery until `settle` has
        // seen them.
        let requeued: Vec<(String, Vec<u8>, i64)> = if self.crash_quarantine {
            sqlx::query_as(
                "UPDATE queue_message SET \
                    retry_count = retry_count + CASE WHEN IFNULL(last_error, '') = ? THEN 1 ELSE 0 END, \
                    redelivered = 1, last_error = ?, \
                    lease_owner = ?, lease_token = ?, lease_until_ms = ? \
                 WHERE state = 1 AND lease_until_ms <= ? \
                 RETURNING message_id, payload, serialization",
            )
            .bind(PROCESSING_ALONE_MARK)
            .bind(RECOVERED_MARK)
            .bind(&self.consumer_name)
            .bind(&recovery_token)
            .bind(self.lease_until_ms())
            .bind(current_ms)
            .fetch_all(&mut *tx)
            .await?
        } else {
            sqlx::query_as(
                "UPDATE queue_message SET retry_count = retry_count + 1, \
                    redelivered = 1, last_error = ?, \
                    lease_owner = ?, lease_token = ?, lease_until_ms = ? \
                 WHERE state = 1 AND lease_until_ms <= ? AND retry_count + 1 < ? \
                 RETURNING message_id, payload, serialization",
            )
            .bind(RECOVERED_MARK)
            .bind(&self.consumer_name)
            .bind(&recovery_token)
            .bind(self.lease_until_ms())
            .bind(current_ms)
            .bind(MAX_PROCESSING_RETRIES)
            .fetch_all(&mut *tx)
            .await?
        };
        tx.commit().await?;
        self.trim_dead_letters().await?;

        let recovery = recovered_messages(
            requeued
                .iter()
                .map(|(_, payload, serialization)| (payload.clone(), *serialization)),
            dead_lettered,
        );
        if let Ok(recovery) = &recovery {
            settle(recovery).await;
        }
        // Release the requeued messages even when one could not be decoded.
        if !requeued.is_empty() {
            let released: Vec<(String,)> = sqlx::query_as(
                "UPDATE queue_message SET state = 0, available_at_ms = ?, \
                 lease_owner = NULL, lease_token = NULL, lease_until_ms = NULL \
                 WHERE state = 1 AND lease_token = ? RETURNING message_id",
            )
            .bind(now_ms())
            .bind(&recovery_token)
            .fetch_all(self.shared.pool())
            .await?;
            for (message_id,) in released {
                self.shared.notify(message_id);
            }
        }
        recovery
    }

    /// With crash quarantine: expire the leases of this host's consumers
    /// whose processes are gone, so a restarted worker recovers the
    /// deliveries of the one that stopped at once instead of after the lease.
    async fn expire_leases_of_gone_processes(&self) -> Result<(), SqliteQueueError> {
        let host = process_host();
        if host.is_empty() {
            return Ok(());
        }
        let consumers: Vec<(String, i64)> = sqlx::query_as(
            "SELECT consumer_name, process_id FROM queue_consumer WHERE process_host = ?",
        )
        .bind(host)
        .fetch_all(self.shared.pool())
        .await?;
        for (consumer_name, process_id) in consumers {
            if process_id == i64::from(std::process::id()) || !process_gone(process_id) {
                continue;
            }
            sqlx::query(
                "UPDATE queue_message SET lease_until_ms = 0 \
                 WHERE state = 1 AND lease_owner = ?",
            )
            .bind(&consumer_name)
            .execute(self.shared.pool())
            .await?;
            sqlx::query("DELETE FROM queue_consumer WHERE consumer_name = ? AND process_id = ?")
                .bind(&consumer_name)
                .bind(process_id)
                .execute(self.shared.pool())
                .await?;
        }
        Ok(())
    }

    /// With crash quarantine: record which process holds this consumer's
    /// leases (see `expire_leases_of_gone_processes`).
    async fn register_consumer(&self) {
        let host = process_host();
        if !self.crash_quarantine || host.is_empty() {
            return;
        }
        if self
            .shared
            .registered_consumers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(&self.consumer_name)
        {
            return;
        }
        // The schema this row is written against: see `note_consumer_registered`.
        let schema_created_at_ms = self
            .shared
            .schema_created_at_ms
            .load(AtomicOrdering::Relaxed);
        let registered = sqlx::query(
            "INSERT OR REPLACE INTO queue_consumer \
             (consumer_name, process_host, process_id, registered_at_ms) VALUES (?, ?, ?, ?)",
        )
        .bind(&self.consumer_name)
        .bind(host)
        .bind(i64::from(std::process::id()))
        .bind(now_ms())
        .execute(self.shared.pool())
        .await;
        match registered {
            Ok(_) => {
                self.shared
                    .note_consumer_registered(&self.consumer_name, schema_created_at_ms);
            }
            Err(error) => tracing::warn!(
                queue = self.shared.queue_name,
                consumer = self.consumer_name,
                "Failed to register SQLite queue consumer: {}",
                error
            ),
        }
    }

    pub async fn purge_old_pending(&self, _max_age_ms: u64) -> Result<usize, SqliteQueueError> {
        self.shared.ensure_initialized().await?;
        // Redis uses this API to discard stale stream/PEL backlog. SQLite is
        // the authoritative copy of standalone work, so age alone must never
        // delete a ready row or another worker's active lease.
        Ok(0)
    }

    pub async fn fast_forward_if_stale(&self) -> Result<usize, SqliteQueueError> {
        self.shared.ensure_initialized().await?;
        // A Redis consumer cursor can skip old stream history without deleting
        // the stream's sole copy. SQLite has no independent cursor, so its
        // durable rows remain claimable regardless of the startup window.
        Ok(0)
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
        self.register_consumer().await;
        let current_ms = now_ms();
        // With crash quarantine only recovery takes over expired leases.
        let takeover = i64::from(!self.crash_quarantine);

        let lease_token = Uuid::now_v7().to_string();
        let row = if let Some(message_id) = message_id {
            sqlx::query_as::<_, StoredMessage>(
                "UPDATE queue_message SET \
                    retry_count = retry_count + CASE WHEN state = 1 THEN 1 ELSE 0 END, \
                    redelivered = CASE WHEN state = 1 OR retry_count > 0 THEN 1 ELSE redelivered END, \
                    last_error = CASE WHEN state = 1 THEN ? ELSE last_error END, \
                    state = 1, lease_owner = ?, lease_token = ?, lease_until_ms = ? \
                 WHERE message_id = ? AND (\
                    (state = 0 AND available_at_ms <= ?) OR \
                    (state = 1 AND lease_until_ms <= ? AND retry_count + 1 < ? AND ? = 1)\
                 ) \
                 RETURNING message_id, payload, serialization, created_at_ms, retry_count, redelivered, lease_token, last_error",
            )
            .bind(RECOVERED_MARK)
            .bind(&self.consumer_name)
            .bind(&lease_token)
            .bind(self.lease_until_ms())
            .bind(message_id)
            .bind(current_ms)
            .bind(current_ms)
            .bind(MAX_PROCESSING_RETRIES)
            .bind(takeover)
            .fetch_optional(self.shared.pool())
            .await?
        } else {
            sqlx::query_as::<_, StoredMessage>(
                "UPDATE queue_message SET \
                    retry_count = retry_count + CASE WHEN state = 1 THEN 1 ELSE 0 END, \
                    redelivered = CASE WHEN state = 1 OR retry_count > 0 THEN 1 ELSE redelivered END, \
                    last_error = CASE WHEN state = 1 THEN ? ELSE last_error END, \
                    state = 1, lease_owner = ?, lease_token = ?, lease_until_ms = ? \
                 WHERE message_id = (\
                    SELECT message_id FROM queue_message WHERE \
                        (state = 0 AND available_at_ms <= ?) OR \
                        (state = 1 AND lease_until_ms <= ? AND retry_count + 1 < ? AND ? = 1) \
                    ORDER BY created_at_ms, message_id LIMIT 1\
                 ) \
                 RETURNING message_id, payload, serialization, created_at_ms, retry_count, redelivered, lease_token, last_error",
            )
            .bind(RECOVERED_MARK)
            .bind(&self.consumer_name)
            .bind(&lease_token)
            .bind(self.lease_until_ms())
            .bind(current_ms)
            .bind(current_ms)
            .bind(MAX_PROCESSING_RETRIES)
            .bind(takeover)
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
                payload: Some(row.payload),
                serialization: row.serialization,
                retry_count: row.retry_count,
                redelivered: row.redelivered != 0,
                recovered: matches!(
                    row.last_error.as_deref(),
                    Some(RECOVERED_MARK | PROCESSING_ALONE_MARK)
                ),
                keepalive: Some(LeaseKeepalive::start(
                    self.clone(),
                    row.message_id.clone(),
                    row.lease_token.clone(),
                )),
                message_id: Some(row.message_id),
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

/// Marks a delivery as processing alone in its worker, so the worker's stop
/// counts toward the message's retry limit.
pub struct SoleDeliveryMarker {
    shared: Arc<SharedQueue>,
    message_id: String,
    lease_token: String,
}

impl SoleDeliveryMarker {
    pub async fn mark(&self) -> Result<(), SqliteQueueError> {
        sqlx::query(
            "UPDATE queue_message SET last_error = ? \
             WHERE message_id = ? AND state = 1 AND lease_token = ?",
        )
        .bind(PROCESSING_ALONE_MARK)
        .bind(&self.message_id)
        .bind(&self.lease_token)
        .execute(self.shared.pool())
        .await?;
        Ok(())
    }
}

/// Renews a lease from the moment it is claimed until its
/// `SqliteQueueLease` is settled or dropped. A lease can wait in memory
/// between its claim and its handler for longer than the lease lasts;
/// unrenewed, it would expire there and this same process's recovery would
/// requeue a message that is about to run. A process that stops takes the
/// renewal with it, so its leases still expire.
struct LeaseKeepalive {
    stop: tokio::sync::oneshot::Sender<()>,
    renewal: tokio::task::JoinHandle<()>,
}

impl LeaseKeepalive {
    fn start<T>(queue: SqliteQueue<T>, message_id: String, lease_token: String) -> Self
    where
        T: Send + Sync + Serialize + DeserializeOwned + Clone + 'static,
    {
        let renewal_interval = (queue.lease_duration / 3).max(Duration::from_millis(250));
        let (stop, mut stopped) = tokio::sync::oneshot::channel::<()>();
        let renewal = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = tokio::time::sleep(renewal_interval) => {
                        match queue.renew_lease(&message_id, &lease_token).await {
                            Ok(true) => {}
                            Ok(false) => break,
                            Err(error) => {
                                tracing::warn!(
                                    queue = queue.shared.queue_name,
                                    message_id = message_id,
                                    "Failed to renew SQLite queue lease: {}",
                                    error
                                );
                            }
                        }
                    }
                    // Stopped, or the lease was dropped.
                    _ = &mut stopped => break,
                }
            }
        });
        Self { stop, renewal }
    }

    async fn stop(self) {
        let _ = self.stop.send(());
        let _ = self.renewal.await;
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
    recovered: bool,
    keepalive: Option<LeaseKeepalive>,
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

    /// Whether the message's previous delivery never settled: its worker
    /// stopped while holding the lease.
    pub fn recovered(&self) -> bool {
        self.recovered
    }

    /// Whether a counted failure of this delivery dead-letters the message.
    pub fn last_attempt(&self) -> bool {
        self.retry_count + 1 >= MAX_PROCESSING_RETRIES
    }

    /// Deliveries of this message that already counted toward its retry limit.
    pub fn prior_attempts(&self) -> usize {
        usize::try_from(self.retry_count).unwrap_or(0)
    }

    /// With crash quarantine, a handle to mark this delivery as processing
    /// alone in its worker (see `SqliteQueue::with_crash_quarantine`).
    pub fn sole_delivery_marker(&self) -> Option<super::SoleDeliveryMarker> {
        if !self.queue.crash_quarantine {
            return None;
        }
        Some(super::SoleDeliveryMarker(SoleDeliveryMarker {
            shared: Arc::clone(&self.queue.shared),
            message_id: self.message_id.clone()?,
            lease_token: self.lease_token.clone(),
        }))
    }

    /// Stop holding this lease without settling it, as its worker stopping
    /// would: the message stays leased until the lease expires and recovery
    /// takes it. `std::mem::forget` would leave the keepalive renewing it.
    #[doc(hidden)]
    pub fn abandon(mut self) {
        self.keepalive.take();
        self.completed = true;
    }

    /// Run `worker` on the message and settle the lease with its result.
    /// `Ok(None)` without running `worker` when the lease was lost before
    /// the handler could start.
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
        // Run the handler only while the message is still leased to this
        // delivery. Once the lease is lost the message belongs to another
        // delivery, which runs it; running it here too would run it twice.
        match self.queue.renew_lease(&message_id, &self.lease_token).await {
            Ok(true) => {}
            Ok(false) => {
                tracing::warn!(
                    queue = self.queue.shared.queue_name,
                    message_id = message_id,
                    "SQLite queue lease was lost before its handler ran; leaving the message to its new delivery"
                );
                self.message_id.take();
                self.completed = true;
                return Ok(None);
            }
            Err(error) => {
                // Unverified: hand the message back without running it or
                // counting an attempt. If that fails too, the drop releases it.
                if self
                    .queue
                    .release_for_retry(
                        &message_id,
                        &self.lease_token,
                        "lease could not be verified before processing",
                        DROPPED_LEASE_BACKOFF,
                        false,
                    )
                    .await
                    .is_ok()
                {
                    self.message_id.take();
                    self.completed = true;
                }
                return Err(Box::new(error));
            }
        }
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

        let wait_target_p99_ms = queue_wait_target_p99_ms();
        let processing_started = Instant::now();
        // The keepalive started at the claim renews the lease meanwhile.
        let result = worker(item).await;
        if let Some(keepalive) = self.keepalive.take() {
            keepalive.stop().await;
        }

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

/// See `SqliteQueueAdmin::inspect_schema`.
enum ManagedSchema {
    Complete,
    Empty,
    Incompatible,
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

    /// The directory holding the queue files.
    pub fn directory(&self) -> &Path {
        &self.directory
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
                if database.code().as_deref().is_some_and(Self::is_resettable_result_code)
        )
    }

    /// Whether a SQLite result code says the file is corrupt
    /// (SQLITE_CORRUPT, 11) or not a database (SQLITE_NOTADB, 26). SQLite
    /// reports extended codes, such as 779 for SQLITE_CORRUPT_INDEX, whose
    /// low byte is the primary code.
    fn is_resettable_result_code(code: &str) -> bool {
        code.parse::<i64>()
            .is_ok_and(|code| matches!(code & 0xff, 11 | 26))
    }

    fn is_transient_file_lock(error: &std::io::Error) -> bool {
        matches!(
            error.kind(),
            std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::WouldBlock
        ) || matches!(error.raw_os_error(), Some(32 | 33))
    }

    async fn remove_queue_file(path: &Path) -> Result<(), SqliteQueueError> {
        for candidate in [
            path.to_path_buf(),
            PathBuf::from(format!("{}-wal", path.display())),
            PathBuf::from(format!("{}-shm", path.display())),
        ] {
            let mut delay = FILE_REMOVE_RETRY_DELAY;
            for attempt in 0..FILE_REMOVE_MAX_ATTEMPTS {
                match std::fs::remove_file(&candidate) {
                    Ok(()) => break,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
                    Err(error)
                        if Self::is_transient_file_lock(&error)
                            && attempt + 1 < FILE_REMOVE_MAX_ATTEMPTS =>
                    {
                        tokio::time::sleep(delay).await;
                        delay = delay.saturating_mul(2).min(FILE_REMOVE_MAX_RETRY_DELAY);
                    }
                    Err(error) => return Err(error.into()),
                }
            }
        }
        Ok(())
    }

    /// What a managed file holds: the queue's schema, nothing yet, or
    /// tables this build cannot use.
    async fn inspect_schema(
        conn: &mut sqlx::SqliteConnection,
        path: &Path,
    ) -> Result<ManagedSchema, SqliteQueueError> {
        let version: i64 = sqlx::query_scalar("PRAGMA user_version")
            .fetch_one(&mut *conn)
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
        .fetch_all(&mut *conn)
        .await?;
        let has_metadata = tables.iter().any(|table| table == "queue_metadata");
        let has_messages = tables.iter().any(|table| table == "queue_message");
        if !has_metadata && !has_messages {
            return Ok(ManagedSchema::Empty);
        }

        let mut metadata_schema_version = None;
        let complete = if has_metadata && has_messages {
            let metadata_columns: Vec<String> =
                sqlx::query_scalar("SELECT name FROM pragma_table_info('queue_metadata')")
                    .fetch_all(&mut *conn)
                    .await?;
            let message_columns: Vec<String> =
                sqlx::query_scalar("SELECT name FROM pragma_table_info('queue_message')")
                    .fetch_all(&mut *conn)
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
                .fetch_optional(&mut *conn)
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
        Ok(if complete {
            ManagedSchema::Complete
        } else {
            ManagedSchema::Incompatible
        })
    }

    async fn prepare_schema(pool: &SqlitePool, path: &Path) -> Result<bool, SqliteQueueError> {
        {
            let mut conn = pool.acquire().await?;
            match Self::inspect_schema(&mut conn, path).await? {
                ManagedSchema::Complete => return Ok(true),
                // Nothing to read, and nothing to reset: a queue creates its
                // whole schema in one transaction, so its process may be
                // doing that right now. Dropping "leftover" tables here
                // would drop the ones it is about to commit.
                ManagedSchema::Empty => return Ok(false),
                ManagedSchema::Incompatible => {}
            }
        }

        // Tables this build cannot use hold no queue state we can safely
        // interpret. Reset the file's private schema to empty; the next real
        // queue connection will atomically bootstrap it. Decide again under
        // the write lock, so the reset cannot race a queue's initialization.
        let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
        match Self::inspect_schema(&mut tx, path).await? {
            ManagedSchema::Complete => {
                tx.rollback().await?;
                return Ok(true);
            }
            ManagedSchema::Empty => {
                tx.rollback().await?;
                return Ok(false);
            }
            ManagedSchema::Incompatible => {}
        }
        sqlx::query("DROP TABLE IF EXISTS queue_message")
            .execute(&mut *tx)
            .await?;
        sqlx::query("DROP TABLE IF EXISTS queue_metadata")
            .execute(&mut *tx)
            .await?;
        sqlx::query("DROP TABLE IF EXISTS queue_consumer")
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
                Self::remove_queue_file(path).await?;
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
                drop(pool);
                Self::remove_queue_file(path).await?;
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
                // A retried message waits from when it became available again.
                let now = now_ms();
                let oldest_available_ms: Option<i64> = sqlx::query_scalar(
                    "SELECT MIN(available_at_ms) FROM queue_message WHERE state = 0 AND available_at_ms <= ?",
                )
                .bind(now)
                .fetch_one(&pool)
                .await?;
                summary.push(QueueStatus {
                    name,
                    pending,
                    processing,
                    deadletter,
                    oldest_pending_secs: oldest_available_ms
                        .map(|available_ms| (now - available_ms).max(0) / 1000),
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
            crash_quarantine: false,
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

        let started = std::time::Instant::now();
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
        let waits: Vec<_> = before
            .queues
            .iter()
            .map(|queue| (queue.name.as_str(), queue.oldest_pending_secs))
            .collect();
        // "ready" has waited since it was enqueued, which a slow runner can
        // put past a second ago (the extra second absorbs clock rounding).
        let waited = started.elapsed().as_secs() as i64 + 1;
        let event_wait = before.queues[0].oldest_pending_secs.unwrap_or(-1);
        assert!((0..=waited).contains(&event_wait), "{waits:?}");
        assert_eq!(waits, [("hot:event", Some(event_wait)), ("hot:task", None)]);
        assert_eq!(before.oldest_pending_secs, Some(event_wait));

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
        assert_eq!(after.oldest_pending_secs, None);

        drop(leased);
    }

    #[tokio::test]
    async fn admin_status_counts_a_retry_backing_off_as_pending_with_no_wait() {
        let temp = tempfile::tempdir().unwrap();
        let queue = test_queue::<String>(temp.path(), "hot:event");
        queue.enqueue("retry".to_string()).await.unwrap();
        // As release_for_retry leaves it: ready again once the backoff ends.
        sqlx::query("UPDATE queue_message SET available_at_ms = ?")
            .bind(now_ms() + 60_000)
            .execute(queue.shared.pool())
            .await
            .unwrap();

        let status = SqliteQueueAdmin::new(temp.path().to_path_buf())
            .status()
            .await
            .unwrap();
        assert_eq!(
            (status.total_pending, status.oldest_pending_secs),
            (1, None)
        );
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
    async fn startup_retention_apis_do_not_delete_durable_sqlite_work() {
        let temp = tempfile::tempdir().unwrap();
        let queue = test_queue::<String>(temp.path(), "durable-startup")
            .with_startup_window(Duration::from_secs(60));
        queue.enqueue("leased".to_string()).await.unwrap();
        queue.enqueue("ready".to_string()).await.unwrap();
        let mut lease = queue.claim_now().await.unwrap().unwrap();
        lease.completed = true;

        sqlx::query("UPDATE queue_message SET created_at_ms = ?")
            .bind(now_ms() - 2 * 60 * 60 * 1000)
            .execute(queue.shared.pool())
            .await
            .unwrap();

        assert_eq!(queue.purge_old_pending(60 * 60 * 1000).await.unwrap(), 0);
        assert_eq!(queue.fast_forward_if_stale().await.unwrap(), 0);
        let states: Vec<(i64,)> = sqlx::query_as("SELECT state FROM queue_message ORDER BY state")
            .fetch_all(queue.shared.pool())
            .await
            .unwrap();
        assert_eq!(states, vec![(STATE_READY,), (STATE_LEASED,)]);
    }

    #[tokio::test]
    async fn expired_lease_dead_letter_is_reported_before_claiming_more_work() {
        let temp = tempfile::tempdir().unwrap();
        let queue = test_queue::<String>(temp.path(), "expired-dlq-report");
        queue.enqueue("poison".to_string()).await.unwrap();
        let mut lease = queue.claim_now().await.unwrap().unwrap();
        let message_id = lease.message_id.clone().unwrap();
        lease.completed = true;

        sqlx::query(
            "UPDATE queue_message SET retry_count = ?, lease_until_ms = ? WHERE message_id = ?",
        )
        .bind(MAX_PROCESSING_RETRIES - 1)
        .bind(now_ms() - 1)
        .bind(&message_id)
        .execute(queue.shared.pool())
        .await
        .unwrap();

        assert!(queue.claim_now().await.unwrap().is_none());
        let state_before_recovery: i64 =
            sqlx::query_scalar("SELECT state FROM queue_message WHERE message_id = ?")
                .bind(&message_id)
                .fetch_one(queue.shared.pool())
                .await
                .unwrap();
        assert_eq!(state_before_recovery, STATE_LEASED);

        let recovery = queue.recover_orphaned_items_with_data().await.unwrap();
        assert_eq!(recovery.requeued_count, 0);
        assert_eq!(recovery.dead_lettered_messages.len(), 1);
        assert_eq!(
            recovery.dead_lettered_messages[0]
                .decode::<String>()
                .unwrap(),
            "poison"
        );
        let row: (i64, i64) =
            sqlx::query_as("SELECT state, retry_count FROM queue_message WHERE message_id = ?")
                .bind(&message_id)
                .fetch_one(queue.shared.pool())
                .await
                .unwrap();
        assert_eq!(row, (STATE_DEAD_LETTER, MAX_PROCESSING_RETRIES));
    }

    /// Expire a lease as if its holder stopped, keeping its delivery's mark.
    async fn expire_lease<T>(queue: &SqliteQueue<T>, message_id: &str) {
        sqlx::query("UPDATE queue_message SET lease_until_ms = ? WHERE message_id = ?")
            .bind(now_ms() - 1)
            .bind(message_id)
            .execute(queue.shared.pool())
            .await
            .unwrap();
    }

    async fn state_and_retries<T>(queue: &SqliteQueue<T>, message_id: &str) -> (i64, i64) {
        sqlx::query_as("SELECT state, retry_count FROM queue_message WHERE message_id = ?")
            .bind(message_id)
            .fetch_one(queue.shared.pool())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn crash_quarantine_counts_only_the_stops_of_deliveries_processed_alone() {
        let temp = tempfile::tempdir().unwrap();
        let queue =
            test_queue::<String>(temp.path(), "quarantine-counting").with_crash_quarantine();
        queue.enqueue("bystander".to_string()).await.unwrap();
        let mut lease = queue.claim_now().await.unwrap().unwrap();
        assert!(!lease.recovered());
        let message_id = lease.message_id.clone().unwrap();
        lease.completed = true;

        // A stopped worker's bystander comes back, uncounted, however often,
        // and only recovery takes over its expired lease.
        for _ in 0..MAX_PROCESSING_RETRIES + 1 {
            expire_lease(&queue, &message_id).await;
            assert!(queue.claim_now().await.unwrap().is_none());
            let recovery = queue.recover_orphaned_items_with_data().await.unwrap();
            assert_eq!(recovery.requeued_count, 1);
            assert!(recovery.dead_lettered_messages.is_empty());
            assert_eq!(
                state_and_retries(&queue, &message_id).await,
                (STATE_READY, 0)
            );
            let mut lease = queue.claim_now().await.unwrap().unwrap();
            assert!(lease.recovered());
            lease.completed = true;
        }

        // Processed alone, each stop counts, until the message is dead-lettered.
        let mut lease = queue.claim_now().await.unwrap_or(None);
        assert!(lease.is_none());
        for attempt in 1..=MAX_PROCESSING_RETRIES {
            let mark = SoleDeliveryMarker {
                shared: Arc::clone(&queue.shared),
                message_id: message_id.clone(),
                lease_token: sqlx::query_scalar(
                    "SELECT lease_token FROM queue_message WHERE message_id = ?",
                )
                .bind(&message_id)
                .fetch_one(queue.shared.pool())
                .await
                .unwrap(),
            };
            mark.mark().await.unwrap();
            expire_lease(&queue, &message_id).await;
            let recovery = queue.recover_orphaned_items_with_data().await.unwrap();
            if attempt < MAX_PROCESSING_RETRIES {
                assert_eq!(recovery.requeued_count, 1);
                assert_eq!(
                    state_and_retries(&queue, &message_id).await,
                    (STATE_READY, attempt)
                );
                lease = queue.claim_now().await.unwrap();
                let lease = lease.as_mut().unwrap();
                assert!(lease.recovered());
                assert_eq!(lease.last_attempt(), attempt + 1 == MAX_PROCESSING_RETRIES);
                lease.completed = true;
            } else {
                assert_eq!(recovery.dead_lettered_messages.len(), 1);
                assert_eq!(
                    state_and_retries(&queue, &message_id).await,
                    (STATE_DEAD_LETTER, MAX_PROCESSING_RETRIES)
                );
            }
        }
    }

    #[tokio::test]
    async fn recovery_settles_requeued_messages_before_they_can_be_claimed() {
        let temp = tempfile::tempdir().unwrap();
        let queue = test_queue::<String>(temp.path(), "settle-before-release");
        queue.enqueue("orphan".to_string()).await.unwrap();
        let mut lease = queue.claim_now().await.unwrap().unwrap();
        let message_id = lease.message_id.clone().unwrap();
        lease.completed = true;
        expire_lease(&queue, &message_id).await;

        let claimer = queue.clone().with_consumer_name("claimer".to_string());
        let claimed_while_settling = Arc::new(AtomicUsize::new(0));
        let claimed = Arc::clone(&claimed_while_settling);
        let recovery = queue
            .recover_orphaned_items_then(|recovery| {
                assert_eq!(recovery.requeued_count, 1);
                let claimer = claimer.clone();
                async move {
                    if let Some(mut lease) = claimer.claim_now().await.unwrap() {
                        lease.completed = true;
                        claimed.fetch_add(1, Ordering::SeqCst);
                    }
                }
            })
            .await
            .unwrap();
        assert_eq!(recovery.requeued_count, 1);
        assert_eq!(claimed_while_settling.load(Ordering::SeqCst), 0);
        let mut lease = claimer.claim_now().await.unwrap().unwrap();
        assert!(lease.recovered());
        lease.completed = true;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn crash_quarantine_recovers_the_leases_of_a_process_that_is_gone() {
        let temp = tempfile::tempdir().unwrap();
        let queue = test_queue::<String>(temp.path(), "gone-process").with_crash_quarantine();
        queue.enqueue("orphan".to_string()).await.unwrap();
        let mut lease = queue.claim_now().await.unwrap().unwrap();
        let message_id = lease.message_id.clone().unwrap();
        lease.completed = true;
        if process_host().is_empty() {
            return;
        }

        // A live process keeps its unexpired lease.
        assert_eq!(
            queue
                .recover_orphaned_items_with_data()
                .await
                .unwrap()
                .total_count(),
            0
        );

        // Hand the lease to a process that has exited.
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let gone_pid = i64::from(child.id());
        child.wait().unwrap();
        assert!(process_gone(gone_pid));
        sqlx::query("UPDATE queue_message SET lease_owner = 'stopped-worker' WHERE message_id = ?")
            .bind(&message_id)
            .execute(queue.shared.pool())
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO queue_consumer (consumer_name, process_host, process_id, registered_at_ms) \
             VALUES ('stopped-worker', ?, ?, 0)",
        )
        .bind(process_host())
        .bind(gone_pid)
        .execute(queue.shared.pool())
        .await
        .unwrap();

        let recovery = queue.recover_orphaned_items_with_data().await.unwrap();
        assert_eq!(recovery.requeued_count, 1);
        assert_eq!(
            state_and_retries(&queue, &message_id).await,
            (STATE_READY, 0)
        );
        let registered: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM queue_consumer WHERE consumer_name = 'stopped-worker'",
        )
        .fetch_one(queue.shared.pool())
        .await
        .unwrap();
        assert_eq!(registered, 0);
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

    /// A claimed lease can wait for its handler longer than the lease lasts.
    /// It stays this delivery's lease: recovery leaves it alone, and it runs
    /// once.
    #[tokio::test]
    async fn waiting_lease_outlives_its_duration_without_being_recovered() {
        let temp = tempfile::tempdir().unwrap();
        let queue = test_queue::<String>(temp.path(), "waiting-lease")
            .with_orphan_idle_ms(MIN_LEASE_DURATION.as_millis() as u64);
        queue.enqueue("waiting".to_string()).await.unwrap();
        let lease = queue.claim_now().await.unwrap().unwrap();

        tokio::time::sleep(MIN_LEASE_DURATION + Duration::from_millis(500)).await;
        assert_eq!(queue.recover_orphaned_items().await.unwrap(), 0);
        assert!(queue.claim_now().await.unwrap().is_none());

        let runs = Arc::new(AtomicUsize::new(0));
        let ran = Arc::clone(&runs);
        let processed = lease
            .process(|item| async move {
                ran.fetch_add(1, Ordering::SeqCst);
                Ok::<_, Box<dyn Error + Send + Sync>>(item)
            })
            .await
            .unwrap();
        assert_eq!(processed.as_deref(), Some("waiting"));
        assert_eq!(runs.load(Ordering::SeqCst), 1);
        assert_eq!(queue.len().await.unwrap(), 0);
    }

    /// An abandoned lease expires as a stopped worker's would.
    #[tokio::test]
    async fn abandoned_lease_expires_and_is_recovered() {
        let temp = tempfile::tempdir().unwrap();
        let queue = test_queue::<String>(temp.path(), "abandoned-lease")
            .with_orphan_idle_ms(MIN_LEASE_DURATION.as_millis() as u64);
        queue.enqueue("orphan".to_string()).await.unwrap();
        queue.claim_now().await.unwrap().unwrap().abandon();

        tokio::time::sleep(MIN_LEASE_DURATION + Duration::from_millis(500)).await;
        assert_eq!(queue.recover_orphaned_items().await.unwrap(), 1);
        let mut lease = queue.claim_now().await.unwrap().unwrap();
        assert!(lease.recovered());
        lease.completed = true;
    }

    /// A lease that was recovered while it waited does not run its handler:
    /// the message runs once, in its new delivery.
    #[tokio::test]
    async fn lost_lease_does_not_run_its_handler() {
        let temp = tempfile::tempdir().unwrap();
        let queue = test_queue::<String>(temp.path(), "lost-lease");
        queue.enqueue("once".to_string()).await.unwrap();
        let stale = queue.claim_now().await.unwrap().unwrap();
        let message_id = stale.message_id.clone().unwrap();
        expire_lease(&queue, &message_id).await;
        assert_eq!(queue.recover_orphaned_items().await.unwrap(), 1);

        let runs = Arc::new(AtomicUsize::new(0));
        let ran = Arc::clone(&runs);
        let skipped = stale
            .process(|item| async move {
                ran.fetch_add(1, Ordering::SeqCst);
                Ok::<_, Box<dyn Error + Send + Sync>>(item)
            })
            .await
            .unwrap();
        assert_eq!(skipped, None);
        assert_eq!(runs.load(Ordering::SeqCst), 0);

        // Skipping it neither released nor counted the new delivery's message.
        assert_eq!(
            state_and_retries(&queue, &message_id).await,
            (STATE_READY, 1)
        );
        let lease = queue.claim_now().await.unwrap().unwrap();
        assert!(lease.recovered());
        let ran = Arc::clone(&runs);
        lease
            .process(|item| async move {
                ran.fetch_add(1, Ordering::SeqCst);
                Ok::<_, Box<dyn Error + Send + Sync>>(item)
            })
            .await
            .unwrap();
        assert_eq!(runs.load(Ordering::SeqCst), 1);
        assert_eq!(queue.len().await.unwrap(), 0);
    }

    #[test]
    fn admin_resets_on_extended_corruption_codes() {
        // SQLITE_CORRUPT, SQLITE_NOTADB, and SQLITE_CORRUPT_VTAB, _SEQUENCE
        // and _INDEX.
        for code in ["11", "26", "267", "523", "779"] {
            assert!(SqliteQueueAdmin::is_resettable_result_code(code), "{code}");
        }
        // SQLITE_BUSY, SQLITE_CANTOPEN, SQLITE_IOERR_READ, SQLITE_BUSY_SNAPSHOT.
        for code in ["5", "14", "266", "517", "not a code"] {
            assert!(!SqliteQueueAdmin::is_resettable_result_code(code), "{code}");
        }
    }

    /// A queue creates its schema in one transaction. An admin that opens the
    /// file before that commits sees no tables, and must not then drop the
    /// ones the queue commits.
    #[tokio::test]
    async fn admin_leaves_a_file_another_process_is_initializing() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(queue_file_name("mid-bootstrap"));
        let initializer = SqlitePoolOptions::new()
            .max_connections(1)
            .connect(&format!("sqlite://{}?mode=rwc", path.display()))
            .await
            .unwrap();
        sqlx::query("PRAGMA journal_mode = WAL")
            .execute(&initializer)
            .await
            .unwrap();
        let mut tx = initializer.begin_with("BEGIN IMMEDIATE").await.unwrap();
        for create_table in [
            "CREATE TABLE queue_metadata (id INTEGER PRIMARY KEY)",
            "CREATE TABLE queue_message (id INTEGER PRIMARY KEY)",
        ] {
            sqlx::query(create_table).execute(&mut *tx).await.unwrap();
        }

        let admin = SqliteQueueAdmin::new(temp.path().to_path_buf());
        let status = tokio::spawn(async move { admin.status().await });
        tokio::time::sleep(Duration::from_millis(200)).await;
        tx.commit().await.unwrap();
        assert!(status.await.unwrap().unwrap().queues.is_empty());

        let tables: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' \
             AND name IN ('queue_metadata', 'queue_message')",
        )
        .fetch_one(&initializer)
        .await
        .unwrap();
        assert_eq!(tables, 2);
        initializer.close().await;
    }

    /// A queue whose tables were dropped under it initializes them again,
    /// and records its consumer in the new `queue_consumer` table.
    #[tokio::test]
    async fn initialized_queue_recovers_when_its_tables_vanish() {
        let temp = tempfile::tempdir().unwrap();
        let queue = test_queue::<String>(temp.path(), "vanished-tables").with_crash_quarantine();
        let registered = async || {
            let consumers: i64 =
                sqlx::query_scalar("SELECT COUNT(*) FROM queue_consumer WHERE consumer_name = ?")
                    .bind(&queue.consumer_name)
                    .fetch_one(queue.shared.pool())
                    .await
                    .unwrap();
            // A process that cannot name its host registers no consumer.
            consumers == 1 || process_host().is_empty()
        };
        queue.enqueue("before".to_string()).await.unwrap();
        let mut lease = queue.claim_now().await.unwrap().unwrap();
        lease.completed = true;
        drop(lease);
        assert!(registered().await);
        let replaced_schema = queue
            .shared
            .schema_created_at_ms
            .load(AtomicOrdering::Relaxed);

        for drop_table in [
            "DROP TABLE queue_message",
            "DROP TABLE queue_metadata",
            "DROP TABLE queue_consumer",
        ] {
            sqlx::query(drop_table)
                .execute(queue.shared.pool())
                .await
                .unwrap();
        }
        sqlx::query("PRAGMA user_version = 0")
            .execute(queue.shared.pool())
            .await
            .unwrap();
        // The new metadata row is written a moment after the old one was.
        tokio::time::sleep(Duration::from_millis(5)).await;
        // Make the next operation's schema check due.
        queue
            .shared
            .schema_checked_at_ms
            .store(0, AtomicOrdering::Relaxed);

        queue.enqueue("after".to_string()).await.unwrap();
        queue.enqueue("claimed".to_string()).await.unwrap();
        assert_eq!(queue.dequeue().await.unwrap().as_deref(), Some("after"));
        // A registration written to the old table that only now gets its
        // answer must not count as recorded in the new one.
        assert_ne!(
            queue
                .shared
                .schema_created_at_ms
                .load(AtomicOrdering::Relaxed),
            replaced_schema
        );
        assert!(
            !queue
                .shared
                .note_consumer_registered(&queue.consumer_name, replaced_schema)
        );
        // The claim finds the consumer missing from the new table.
        let mut lease = queue.claim_now().await.unwrap().unwrap();
        lease.completed = true;
        drop(lease);
        assert!(registered().await);
    }
}

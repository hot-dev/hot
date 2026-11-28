//! Redis Streams pub/sub implementation for distributed deployments
//!
//! This uses Redis Streams (XADD/XREAD) for real-time event delivery
//! across distributed worker and API processes. Unlike traditional
//! PUBLISH/SUBSCRIBE, this works in Redis cluster mode.
//!
//! Key features:
//! - Full cluster mode support
//! - Automatic stream trimming (MAXLEN ~) to prevent unbounded growth
//! - Idle stream expiry to bound the number of per-run/per-task keys
//! - BLOCK for efficient long-polling without busy waiting
//! - Connection caching to minimize Redis connection overhead
//! - Cancel-safe subscribers that resume from their last entry after a
//!   connection failure (see `StreamReader`)

use super::wakeup::{TaskWakeupGuard, TaskWakeupHub};
use super::{
    EnvEvent, EnvPublisher, EnvSubscriber, EnvSubscriberFactory, McpSseTransportSessionBinding,
    McpSseTransportSessionStore, StreamEvent, StreamNext, StreamPubSubError, StreamPublisher,
    StreamSubscriber, StreamSubscriberFactory, channel_name, env_channel_name, legacy_channel_name,
};
use async_trait::async_trait;
use redis::Client;
use redis::aio::MultiplexedConnection;
use redis::cluster::ClusterClient;
use redis::cluster_async::ClusterConnection as AsyncClusterConnection;
use redis::streams::{StreamRangeReply, StreamReadReply};
use std::collections::VecDeque;
use std::future::Future;
use std::sync::{Arc, Weak};
use std::time::Duration;
use tokio::sync::{Mutex, mpsc, watch};
use uuid::Uuid;

/// Maximum number of entries to keep per stream (approximate)
/// This bounds entry count; expiry separately bounds idle key lifetime.
const STREAM_MAXLEN: usize = 1000;
/// Task inboxes survive the longest supported task (7 days) plus delivery grace.
const TASK_STREAM_IDLE_TTL_SECS: u64 = 8 * 24 * 60 * 60;
/// Default run ceiling (1 hour) plus one hour for reconnect/final delivery.
const DEFAULT_RUN_STREAM_IDLE_TTL_SECS: u64 = 2 * 60 * 60;
/// One server-side operation: a process/connection loss after XADD must not
/// leave a permanent key. The single explicit key also works in Redis Cluster.
const PUBLISH_STREAM_SCRIPT: &str = "\
local id = redis.call('XADD', KEYS[1], 'MAXLEN', '~', ARGV[1], '*', 'event', ARGV[2])
redis.call('EXPIRE', KEYS[1], ARGV[3])
return id";
const MCP_SSE_TRANSPORT_SESSION_PREFIX: &str = "hot:mcp:http-sse-session";
const REDIS_COMMAND_TIMEOUT: Duration = Duration::from_secs(10);
const REDIS_SUBSCRIBER_COMMAND_TIMEOUT: Duration = Duration::from_secs(35);
/// How long one subscriber XREAD parks on the server; must stay below
/// `REDIS_SUBSCRIBER_COMMAND_TIMEOUT`.
const XREAD_BLOCK_MS: u64 = 30_000;
/// Entries one subscriber XREAD may return.
const XREAD_COUNT: usize = 100;
/// Entries a subscriber's reader buffers ahead of it. A full buffer pauses
/// reading; the entries wait in Redis until `STREAM_MAXLEN` trims them.
const READER_BUFFER: usize = 256;
/// Reconnect pacing for a subscriber whose connection failed: the first
/// retry waits the minimum, each further failure doubles it up to the cap,
/// so an outage costs one dial per subscriber every few seconds.
const READER_RECONNECT_MIN: Duration = Duration::from_millis(250);
const READER_RECONNECT_MAX: Duration = Duration::from_secs(5);
/// Shared Redis Stream carrying lightweight task-completion wakeup hints
/// (task UUIDs only, no payloads or tenant data). A single key maps to one
/// hash slot in Redis Cluster, allowing every worker pod to multiplex all
/// concurrent `::hot::task/await` calls over ONE shared subscriber connection.
pub(crate) const TASK_WAKEUP_STREAM_KEY: &str = "hot:task:wakeup";
/// Shared Redis Stream carrying lightweight task-inbox wakeup hints
/// (task UUIDs only, no payloads or tenant data). A single key maps to one
/// hash slot in Redis Cluster, allowing task workers to multiplex all
/// concurrent `subscribe_task` inboxes over ONE shared subscriber connection
/// while durable payloads stay in per-task `stream:{task_id}` keys.
pub(crate) const TASK_INBOX_WAKEUP_STREAM_KEY: &str = "hot:task:inbox:wakeup";
const TASK_WAKEUP_STREAM_MAXLEN: usize = 4096;
const TASK_WAKEUP_STREAM_IDLE_TTL_SECS: u64 = 60 * 60;
/// Cap on how long the first cold `watch_task_completion` or `subscribe_task`
/// waits for the shared Redis reader to connect and establish its initial
/// cursor before continuing.
const TASK_WAKEUP_COLD_START_WAIT: Duration = Duration::from_millis(250);
/// Fallback `XRANGE` interval for a long-running task inbox that has not seen
/// a wakeup hint (covers rolling upgrades with older producers that write only
/// `stream:{task_id}`, and stream trimming past `TASK_WAKEUP_STREAM_MAXLEN`).
const TASK_INBOX_FALLBACK_POLL: Duration = Duration::from_secs(2);
/// Continuous wall-clock duration with zero active in-process waiters before
/// the shared wakeup reader parks itself and closes its Redis connection, even
/// when other pods continue writing completions to `hot:task:wakeup`.
const TASK_WAKEUP_IDLE_STOP_AFTER: Duration = Duration::from_secs(60);

fn mcp_sse_transport_session_key(transport_session_id: &Uuid) -> String {
    format!(
        "{}:{{{}}}",
        MCP_SSE_TRANSPORT_SESSION_PREFIX, transport_session_id
    )
}

/// Connection pool that caches Redis connections to avoid expensive reconnections
enum RedisConnectionPool {
    Standalone {
        client: Box<Client>,
        cached_conn: Arc<Mutex<CachedConn<MultiplexedConnection>>>,
    },
    Cluster {
        client: Box<ClusterClient>,
        cached_conn: Arc<Mutex<CachedConn<AsyncClusterConnection>>>,
    },
}

/// Pool slot: the cached connection plus a monotonically increasing
/// generation, bumped each time a new connection is installed.
/// `ConnectionGuard`s capture the generation at checkout and evict only
/// while it still matches, so a late-failing holder of an old, dead
/// connection cannot clobber a healthy replacement another caller has
/// already established (which would force pointless reconnect churn
/// under fan-out). Both pool arms share this scheme: standalone
/// `MultiplexedConnection` and cluster `ClusterConnection` are both
/// cheap `Arc`-backed handles, so guards hold a clone of the handle —
/// never the slot mutex — and the slot lock is only ever taken for the
/// brief checkout/eviction critical sections.
struct CachedConn<C> {
    conn: Option<C>,
    generation: u64,
}

// Manual impl: `#[derive(Default)]` would needlessly require `C: Default`.
impl<C> Default for CachedConn<C> {
    fn default() -> Self {
        Self {
            conn: None,
            generation: 0,
        }
    }
}

/// Check out a connection from a pool slot: clone the cached handle when
/// one is present, otherwise dial via `connect` and install the result
/// under a bumped generation. Dialing happens while the slot lock is held
/// so concurrent cold callers coalesce into one dial instead of stampeding
/// the server; the lock is released before this returns, so callers never
/// hold it while running commands.
async fn checkout_conn<C: Clone>(
    cached_conn: &Mutex<CachedConn<C>>,
    connect: impl Future<Output = Result<C, StreamPubSubError>>,
) -> Result<(C, u64), StreamPubSubError> {
    let mut slot = cached_conn.lock().await;
    if let Some(conn) = slot.conn.as_ref() {
        return Ok((conn.clone(), slot.generation));
    }
    let conn = connect.await?;
    slot.conn = Some(conn.clone());
    slot.generation += 1;
    Ok((conn, slot.generation))
}

/// Drop the slot's connection so the next checkout dials fresh — but only
/// while the slot generation still matches the evicting guard's checkout,
/// so a stale guard cannot discard a healthy replacement (see
/// `CachedConn`).
async fn evict_slot<C>(cached_conn: &Mutex<CachedConn<C>>, generation: u64) {
    let mut slot = cached_conn.lock().await;
    if slot.generation == generation {
        slot.conn = None;
    }
}

impl RedisConnectionPool {
    fn new_standalone(client: Client) -> Self {
        Self::Standalone {
            client: Box::new(client),
            cached_conn: Arc::new(Mutex::new(CachedConn::default())),
        }
    }

    fn new_cluster(client: ClusterClient) -> Self {
        Self::Cluster {
            client: Box::new(client),
            cached_conn: Arc::new(Mutex::new(CachedConn::default())),
        }
    }

    /// Get a cached connection for short-lived operations (publish).
    ///
    /// The returned guard owns a clone of the pooled handle — never the
    /// pool lock — so a caller holding one guard can safely call other
    /// pool-locking methods (see `ConnectionGuard`).
    async fn get_connection(&self) -> Result<ConnectionGuard<'_>, StreamPubSubError> {
        match self {
            RedisConnectionPool::Standalone {
                client,
                cached_conn,
            } => {
                let (conn, generation) = checkout_conn(cached_conn, async {
                    tokio::time::timeout(
                        REDIS_COMMAND_TIMEOUT,
                        client.get_multiplexed_async_connection_with_config(
                            &crate::redis::standalone_async_config(),
                        ),
                    )
                    .await
                    .map_err(|_| {
                        StreamPubSubError::ConnectionError(
                            "Redis connection timed out after 10s".into(),
                        )
                    })?
                    .map_err(|e| StreamPubSubError::ConnectionError(e.to_string()))
                })
                .await?;
                Ok(ConnectionGuard::Standalone {
                    conn,
                    cached_conn: cached_conn.as_ref(),
                    generation,
                })
            }
            RedisConnectionPool::Cluster {
                client,
                cached_conn,
            } => {
                let (conn, generation) = checkout_conn(cached_conn, async {
                    tokio::time::timeout(REDIS_COMMAND_TIMEOUT, client.get_async_connection())
                        .await
                        .map_err(|_| {
                            StreamPubSubError::ConnectionError(
                                "Redis cluster connection timed out after 10s".into(),
                            )
                        })?
                        .map_err(|e| StreamPubSubError::ConnectionError(e.to_string()))
                })
                .await?;
                Ok(ConnectionGuard::Cluster {
                    conn,
                    cached_conn: cached_conn.as_ref(),
                    generation,
                })
            }
        }
    }

    /// Create a new dedicated connection for subscribers (XREAD BLOCK holds the connection)
    async fn create_subscriber_connection(
        &self,
    ) -> Result<SubscriberConnection, StreamPubSubError> {
        match self {
            RedisConnectionPool::Standalone { client, .. } => {
                // Create a fresh connection for the subscriber. Disable the
                // redis-rs 1.x 500ms default response timeout — the subscriber
                // issues `XREAD BLOCK 30000` and must park the full block.
                let conn = tokio::time::timeout(
                    REDIS_COMMAND_TIMEOUT,
                    client.get_multiplexed_async_connection_with_config(
                        &crate::redis::standalone_async_config(),
                    ),
                )
                .await
                .map_err(|_| {
                    StreamPubSubError::ConnectionError(
                        "Redis subscriber connection timed out after 10s".into(),
                    )
                })?
                .map_err(|e| StreamPubSubError::ConnectionError(e.to_string()))?;
                Ok(SubscriberConnection::Standalone(conn))
            }
            RedisConnectionPool::Cluster { client, .. } => {
                // Create a fresh connection for the subscriber
                let conn =
                    tokio::time::timeout(REDIS_COMMAND_TIMEOUT, client.get_async_connection())
                        .await
                        .map_err(|_| {
                            StreamPubSubError::ConnectionError(
                                "Redis cluster subscriber connection timed out after 10s".into(),
                            )
                        })?
                        .map_err(|e| StreamPubSubError::ConnectionError(e.to_string()))?;
                Ok(SubscriberConnection::Cluster(conn))
            }
        }
    }
}

/// Guard that holds a connection for short-lived operations.
///
/// Both variants own a cheap clone of the pooled connection handle plus a
/// reference back to the slot for failure eviction — never the slot mutex
/// itself. This is load-bearing: an earlier version had the Cluster
/// variant hold the pool's `MutexGuard` for the guard's whole lifetime,
/// so any method that called another pool-locking method while its guard
/// was alive (e.g. `get_mcp_sse_transport_session`'s expired-binding path
/// calling `delete_mcp_sse_transport_session`) self-deadlocked on the
/// non-reentrant mutex in cluster mode. Never reintroduce a variant that
/// carries the pool lock across the guard's lifetime.
enum ConnectionGuard<'a> {
    Standalone {
        conn: MultiplexedConnection,
        /// The pool slot this connection was cloned from, so a failed
        /// command can evict it and force the next caller to reconnect.
        cached_conn: &'a Mutex<CachedConn<MultiplexedConnection>>,
        /// Slot generation at checkout; eviction is skipped when the slot
        /// has since been replaced (see `CachedConn`).
        generation: u64,
    },
    Cluster {
        conn: AsyncClusterConnection,
        /// The pool slot this connection was cloned from, so a failed
        /// command can evict it and force the next caller to reconnect.
        cached_conn: &'a Mutex<CachedConn<AsyncClusterConnection>>,
        /// Slot generation at checkout; eviction is skipped when the slot
        /// has since been replaced (see `CachedConn`).
        generation: u64,
    },
}

impl ConnectionGuard<'_> {
    async fn cmd(&mut self, cmd: &redis::Cmd) -> Result<redis::Value, StreamPubSubError> {
        let result = tokio::time::timeout(REDIS_COMMAND_TIMEOUT, async {
            match self {
                ConnectionGuard::Standalone { conn, .. } => cmd.query_async(conn).await,
                ConnectionGuard::Cluster { conn, .. } => cmd.query_async(conn).await,
            }
        })
        .await;

        match result {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(e)) => {
                if crate::redis::error_indicates_broken_connection(&e) {
                    self.evict_cached_connection().await;
                }
                Err(StreamPubSubError::PublishError(e.to_string()))
            }
            Err(_elapsed) => {
                // A client-side timeout usually means a half-open socket
                // (silent LB drop): redis 1.x `MultiplexedConnection` never
                // reconnects on its own, so evict the pooled connection or
                // every later command times out too.
                self.evict_cached_connection().await;
                Err(StreamPubSubError::PublishError(
                    "Redis command timed out after 10s".into(),
                ))
            }
        }
    }

    /// Drop the pooled connection so the next `get_connection` reconnects
    /// fresh. Race-safe under concurrent holders: this only clears the pool
    /// slot; in-flight users of the old connection hold their own clones
    /// and finish or fail on their own. The generation check keeps a
    /// late-failing holder of an old connection from discarding a healthy
    /// replacement another caller has already installed.
    async fn evict_cached_connection(&mut self) {
        match self {
            ConnectionGuard::Standalone {
                cached_conn,
                generation,
                ..
            } => evict_slot(cached_conn, *generation).await,
            ConnectionGuard::Cluster {
                cached_conn,
                generation,
                ..
            } => evict_slot(cached_conn, *generation).await,
        }
    }
}

/// Owned connection for subscribers (long-lived, blocking XREAD)
enum SubscriberConnection {
    Standalone(MultiplexedConnection),
    Cluster(AsyncClusterConnection),
}

impl SubscriberConnection {
    /// A blocking read (`XREAD BLOCK`), given time to reach its server-side
    /// timeout.
    async fn blocking_cmd(&mut self, cmd: &redis::Cmd) -> Result<redis::Value, StreamPubSubError> {
        self.cmd_within(cmd, REDIS_SUBSCRIBER_COMMAND_TIMEOUT, "blocking command")
            .await
    }

    /// A command that answers at once, bounded like the pool's.
    async fn cmd(&mut self, cmd: &redis::Cmd) -> Result<redis::Value, StreamPubSubError> {
        self.cmd_within(cmd, REDIS_COMMAND_TIMEOUT, "command").await
    }

    async fn cmd_within(
        &mut self,
        cmd: &redis::Cmd,
        timeout: Duration,
        kind: &str,
    ) -> Result<redis::Value, StreamPubSubError> {
        tokio::time::timeout(timeout, async {
            match self {
                SubscriberConnection::Standalone(conn) => {
                    let result = cmd
                        .query_async(conn)
                        .await
                        .map_err(|e| StreamPubSubError::SubscribeError(e.to_string()))?;
                    Ok(result)
                }
                SubscriberConnection::Cluster(conn) => {
                    let result = cmd
                        .query_async(conn)
                        .await
                        .map_err(|e| StreamPubSubError::SubscribeError(e.to_string()))?;
                    Ok(result)
                }
            }
        })
        .await
        .map_err(|_| {
            StreamPubSubError::SubscribeError(format!("Redis {kind} timed out after {timeout:?}"))
        })?
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum WakeupReaderReadiness {
    Starting,
    Ready,
    Failed,
}

struct WakeupReaderState {
    task: Option<tokio::task::JoinHandle<()>>,
    generation: u64,
    readiness: Option<watch::Receiver<WakeupReaderReadiness>>,
}

/// Shared per-`RedisStreamsPubSub` multiplexer for `::hot::task/await` wakeup
/// hints.
///
/// Opens at most ONE dedicated Redis subscriber connection per `StreamPubSub`
/// (lazily on the first waiter, parked after `idle_stop_after` with no local
/// waiters regardless of remote traffic, aborted on drop) and fans out
/// received `task_id` hints to matching in-process `TaskWakeupGuard` waiters.
struct RedisTaskWakeupMux {
    pool: Arc<RedisConnectionPool>,
    hub: Arc<TaskWakeupHub>,
    stream_key: String,
    xread_block: Duration,
    idle_stop_after: Duration,
    reader: parking_lot::Mutex<WakeupReaderState>,
}

impl Drop for RedisTaskWakeupMux {
    fn drop(&mut self) {
        if let Some(task) = self.reader.get_mut().task.take() {
            task.abort();
        }
    }
}

impl RedisTaskWakeupMux {
    fn new(pool: Arc<RedisConnectionPool>, stream_key: String) -> Self {
        Self::with_timings(
            pool,
            stream_key,
            Duration::from_millis(XREAD_BLOCK_MS),
            TASK_WAKEUP_IDLE_STOP_AFTER,
        )
    }

    fn with_timings(
        pool: Arc<RedisConnectionPool>,
        stream_key: String,
        xread_block: Duration,
        idle_stop_after: Duration,
    ) -> Self {
        Self {
            pool,
            hub: Arc::new(TaskWakeupHub::new()),
            stream_key,
            xread_block,
            idle_stop_after,
            reader: parking_lot::Mutex::new(WakeupReaderState {
                task: None,
                generation: 0,
                readiness: None,
            }),
        }
    }

    #[cfg(test)]
    fn is_reader_running(&self) -> bool {
        let state = self.reader.lock();
        state.task.as_ref().is_some_and(|task| !task.is_finished())
    }

    #[cfg(test)]
    fn reader_generation(&self) -> u64 {
        self.reader.lock().generation
    }

    async fn watch(self: &Arc<Self>, task_id: Uuid, cold_start_wait: Duration) -> TaskWakeupGuard {
        // Register BEFORE ensuring the reader is running so `active_waiter_count`
        // is already >= 1 before any concurrent idle-stop check runs, and so any
        // hint delivered while the initial DB check runs is stored as a permit.
        let guard = self.hub.register(task_id);
        let mut readiness = self.ensure_reader_started();

        if *readiness.borrow() == WakeupReaderReadiness::Starting {
            let wait_cap = cold_start_wait.min(TASK_WAKEUP_COLD_START_WAIT);
            if !wait_cap.is_zero() {
                let _ = tokio::time::timeout(wait_cap, async {
                    let _ = readiness
                        .wait_for(|state| *state != WakeupReaderReadiness::Starting)
                        .await;
                })
                .await;
            }
        }

        guard
    }

    fn ensure_reader_started(self: &Arc<Self>) -> watch::Receiver<WakeupReaderReadiness> {
        let mut state = self.reader.lock();
        if let Some(task) = state.task.as_ref()
            && !task.is_finished()
            && let Some(readiness) = state.readiness.as_ref()
        {
            return readiness.clone();
        }

        state.generation = state.generation.wrapping_add(1);
        let generation = state.generation;
        let (ready_tx, ready_rx) = watch::channel(WakeupReaderReadiness::Starting);
        let weak = Arc::downgrade(self);
        let handle = tokio::spawn(run_task_wakeup_reader(weak, generation, ready_tx));
        state.task = Some(handle);
        state.readiness = Some(ready_rx.clone());
        ready_rx
    }

    /// Stop this reader generation if no waiters are currently registered.
    /// Serialized under `self.reader` so it cannot race with `watch()`.
    fn try_stop_if_idle(&self, generation: u64) -> bool {
        let mut state = self.reader.lock();
        if state.generation != generation {
            return true;
        }
        if self.hub.active_waiter_count() == 0 {
            state.task = None;
            state.readiness = None;
            return true;
        }
        false
    }
}

async fn run_task_wakeup_reader(
    mux: Weak<RedisTaskWakeupMux>,
    generation: u64,
    ready_tx: watch::Sender<WakeupReaderReadiness>,
) {
    let mut conn: Option<SubscriberConnection> = None;
    let mut last_id: Option<String> = None;
    let mut failures: u32 = 0;
    let mut idle_since: Option<tokio::time::Instant> = None;
    let mut signalled_initial = false;

    loop {
        let Some(mux_strong) = mux.upgrade() else {
            return;
        };
        let pool = Arc::clone(&mux_strong.pool);
        let stream_key = mux_strong.stream_key.clone();
        let xread_block = mux_strong.xread_block;
        let idle_stop_after = mux_strong.idle_stop_after;
        let default_block_ms = xread_block.as_millis().clamp(1, u64::MAX as u128) as u64;

        // Track idle duration across ALL iterations (including non-empty XREAD
        // replies carrying other pods' completions) so an idle pod on a busy
        // multi-tenant deployment still parks and releases its Redis connection.
        let block_ms = if mux_strong.hub.active_waiter_count() == 0 {
            let since = *idle_since.get_or_insert_with(tokio::time::Instant::now);
            let elapsed = since.elapsed();
            if elapsed >= idle_stop_after {
                if mux_strong.try_stop_if_idle(generation) {
                    return;
                }
                // A new waiter raced in before we locked `reader`.
                idle_since = None;
                default_block_ms
            } else {
                let remaining = idle_stop_after.saturating_sub(elapsed);
                // Redis interprets `XREAD BLOCK 0` as "block forever", so
                // always clamp to at least 1 ms.
                remaining
                    .min(xread_block)
                    .as_millis()
                    .clamp(1, u64::MAX as u128) as u64
            }
        } else {
            idle_since = None;
            default_block_ms
        };

        // Release the strong Arc before awaiting I/O so dropping all
        // `RedisStreamsPubSub` handles drops `RedisTaskWakeupMux` and aborts
        // this background reader immediately.
        drop(mux_strong);

        let reader = match conn.as_mut() {
            Some(reader) => reader,
            None => {
                if failures > 0 {
                    tokio::time::sleep(reconnect_delay(failures)).await;
                    let Some(mux_strong) = mux.upgrade() else {
                        return;
                    };
                    if mux_strong.try_stop_if_idle(generation) {
                        return;
                    }
                }
                match pool.create_subscriber_connection().await {
                    Ok(mut fresh) => {
                        if last_id.is_none() {
                            match newest_entry_id(&mut fresh, &stream_key).await {
                                Ok(id) => last_id = Some(id),
                                Err(e) => {
                                    if !signalled_initial {
                                        signalled_initial = true;
                                        ready_tx.send_replace(WakeupReaderReadiness::Failed);
                                    }
                                    if failures == 0 {
                                        tracing::warn!(
                                            "Task wakeup Redis reader failed initial XREVRANGE on {}: {}",
                                            stream_key,
                                            e
                                        );
                                    }
                                    failures = failures.saturating_add(1);
                                    continue;
                                }
                            }
                        }
                        if !signalled_initial {
                            signalled_initial = true;
                            ready_tx.send_replace(WakeupReaderReadiness::Ready);
                        }
                        // Do NOT reset `failures` or call `hub.notify_all()` here:
                        // wait until `XREAD` itself succeeds below (matching
                        // `read_stream`) so persistent XREAD errors (e.g. ACL
                        // NOPERM or WRONGTYPE) still back off instead of
                        // spinning every 250ms and waking waiters on every loop.
                        conn.insert(fresh)
                    }
                    Err(e) => {
                        if !signalled_initial {
                            signalled_initial = true;
                            ready_tx.send_replace(WakeupReaderReadiness::Failed);
                        }
                        if failures == 0 {
                            tracing::warn!(
                                "Task wakeup Redis reader connect failed on {}: {}; falling back to DB polling while reconnecting",
                                stream_key,
                                e
                            );
                        }
                        failures = failures.saturating_add(1);
                        continue;
                    }
                }
            }
        };

        let read_after = last_id.as_deref().unwrap_or("$");
        let mut xread = redis::cmd("XREAD");
        xread
            .arg("BLOCK")
            .arg(block_ms)
            .arg("COUNT")
            .arg(XREAD_COUNT)
            .arg("STREAMS")
            .arg(&stream_key)
            .arg(read_after);

        let result = reader.blocking_cmd(&xread).await;
        let reply = result.and_then(|value| {
            redis::from_redis_value::<Option<StreamReadReply>>(value)
                .map_err(|e| StreamPubSubError::SubscribeError(e.to_string()))
        });

        let reply = match reply {
            Ok(reply) => reply,
            Err(e) => {
                if failures == 0 {
                    tracing::warn!(
                        "Task wakeup Redis XREAD error on {}; reconnecting after {:?}: {}",
                        stream_key,
                        last_id,
                        e
                    );
                }
                conn = None;
                failures = failures.saturating_add(1);
                continue;
            }
        };

        let Some(mux_strong) = mux.upgrade() else {
            return;
        };

        if failures > 0 {
            tracing::info!(
                "Task wakeup Redis reader on {} resumed after {:?}",
                stream_key,
                last_id
            );
            failures = 0;
            // Wake active waiters once after an outage in case their
            // completion occurred while disconnected and was trimmed past
            // `TASK_WAKEUP_STREAM_MAXLEN`.
            mux_strong.hub.notify_all();
        }

        let entries: Vec<_> = reply
            .map(|reply| reply.keys.into_iter().flat_map(|key| key.ids).collect())
            .unwrap_or_default();

        for entry in entries {
            last_id = Some(entry.id.clone());
            let Some(payload) = entry.get::<String>("event") else {
                continue;
            };
            if let Ok(task_id) = Uuid::parse_str(&payload) {
                mux_strong.hub.notify(task_id);
            }
        }
    }
}

/// Redis Streams pub/sub implementation
#[derive(Clone)]
pub struct RedisStreamsPubSub {
    connection_pool: Arc<RedisConnectionPool>,
    run_idle_ttl_secs: u64,
    task_wakeups: Arc<RedisTaskWakeupMux>,
    task_inboxes: Arc<RedisTaskWakeupMux>,
    task_inbox_fallback_poll: Duration,
    task_inbox_idle_interval: Duration,
}

impl RedisStreamsPubSub {
    /// Create a new Redis Streams pub/sub with a standalone client
    pub fn new(client: Client) -> Self {
        let connection_pool = Arc::new(RedisConnectionPool::new_standalone(client));
        let task_wakeups = Arc::new(RedisTaskWakeupMux::new(
            Arc::clone(&connection_pool),
            TASK_WAKEUP_STREAM_KEY.to_string(),
        ));
        let task_inboxes = Arc::new(RedisTaskWakeupMux::new(
            Arc::clone(&connection_pool),
            TASK_INBOX_WAKEUP_STREAM_KEY.to_string(),
        ));
        Self {
            connection_pool,
            run_idle_ttl_secs: DEFAULT_RUN_STREAM_IDLE_TTL_SECS,
            task_wakeups,
            task_inboxes,
            task_inbox_fallback_poll: TASK_INBOX_FALLBACK_POLL,
            task_inbox_idle_interval: Duration::from_millis(XREAD_BLOCK_MS),
        }
    }

    /// Create a new Redis Streams pub/sub with a cluster client
    pub fn new_cluster(cluster_client: ClusterClient) -> Self {
        let connection_pool = Arc::new(RedisConnectionPool::new_cluster(cluster_client));
        let task_wakeups = Arc::new(RedisTaskWakeupMux::new(
            Arc::clone(&connection_pool),
            TASK_WAKEUP_STREAM_KEY.to_string(),
        ));
        let task_inboxes = Arc::new(RedisTaskWakeupMux::new(
            Arc::clone(&connection_pool),
            TASK_INBOX_WAKEUP_STREAM_KEY.to_string(),
        ));
        Self {
            connection_pool,
            run_idle_ttl_secs: DEFAULT_RUN_STREAM_IDLE_TTL_SECS,
            task_wakeups,
            task_inboxes,
            task_inbox_fallback_poll: TASK_INBOX_FALLBACK_POLL,
            task_inbox_idle_interval: Duration::from_millis(XREAD_BLOCK_MS),
        }
    }

    #[cfg(test)]
    fn with_wakeup_stream_key(client: Client, stream_key: String) -> Self {
        let connection_pool = Arc::new(RedisConnectionPool::new_standalone(client));
        let task_wakeups = Arc::new(RedisTaskWakeupMux::new(
            Arc::clone(&connection_pool),
            stream_key,
        ));
        let task_inboxes = Arc::new(RedisTaskWakeupMux::new(
            Arc::clone(&connection_pool),
            TASK_INBOX_WAKEUP_STREAM_KEY.to_string(),
        ));
        Self {
            connection_pool,
            run_idle_ttl_secs: DEFAULT_RUN_STREAM_IDLE_TTL_SECS,
            task_wakeups,
            task_inboxes,
            task_inbox_fallback_poll: TASK_INBOX_FALLBACK_POLL,
            task_inbox_idle_interval: Duration::from_millis(XREAD_BLOCK_MS),
        }
    }

    #[cfg(test)]
    fn with_wakeup_test_config(
        client: Client,
        stream_key: String,
        xread_block: Duration,
        idle_stop_after: Duration,
    ) -> Self {
        let connection_pool = Arc::new(RedisConnectionPool::new_standalone(client));
        let task_wakeups = Arc::new(RedisTaskWakeupMux::with_timings(
            Arc::clone(&connection_pool),
            stream_key,
            xread_block,
            idle_stop_after,
        ));
        let task_inboxes = Arc::new(RedisTaskWakeupMux::new(
            Arc::clone(&connection_pool),
            TASK_INBOX_WAKEUP_STREAM_KEY.to_string(),
        ));
        Self {
            connection_pool,
            run_idle_ttl_secs: DEFAULT_RUN_STREAM_IDLE_TTL_SECS,
            task_wakeups,
            task_inboxes,
            task_inbox_fallback_poll: TASK_INBOX_FALLBACK_POLL,
            task_inbox_idle_interval: Duration::from_millis(XREAD_BLOCK_MS),
        }
    }

    #[cfg(test)]
    fn with_inbox_test_config(
        client: Client,
        inbox_wakeup_stream_key: String,
        xread_block: Duration,
        idle_stop_after: Duration,
        fallback_poll: Duration,
        idle_interval: Duration,
    ) -> Self {
        let connection_pool = Arc::new(RedisConnectionPool::new_standalone(client));
        let task_wakeups = Arc::new(RedisTaskWakeupMux::new(
            Arc::clone(&connection_pool),
            TASK_WAKEUP_STREAM_KEY.to_string(),
        ));
        let task_inboxes = Arc::new(RedisTaskWakeupMux::with_timings(
            Arc::clone(&connection_pool),
            inbox_wakeup_stream_key,
            xread_block,
            idle_stop_after,
        ));
        Self {
            connection_pool,
            run_idle_ttl_secs: DEFAULT_RUN_STREAM_IDLE_TTL_SECS,
            task_wakeups,
            task_inboxes,
            task_inbox_fallback_poll: fallback_poll,
            task_inbox_idle_interval: idle_interval,
        }
    }

    #[cfg(test)]
    fn is_task_wakeup_reader_running(&self) -> bool {
        self.task_wakeups.is_reader_running()
    }

    #[cfg(test)]
    fn is_task_inbox_reader_running(&self) -> bool {
        self.task_inboxes.is_reader_running()
    }

    #[cfg(test)]
    fn task_inbox_waiter_count(&self) -> usize {
        self.task_inboxes.hub.active_waiter_count()
    }

    #[cfg(test)]
    fn task_inbox_reader_generation(&self) -> u64 {
        self.task_inboxes.reader_generation()
    }

    /// Register a wakeup waiter for `task_id` on this pub/sub's shared
    /// background reader.
    pub async fn watch_task_completion(
        &self,
        task_id: Uuid,
        cold_start_wait: Duration,
    ) -> TaskWakeupGuard {
        self.task_wakeups.watch(task_id, cold_start_wait).await
    }

    /// Publish a best-effort task-completion wakeup hint both in-process and
    /// onto the shared Redis wakeup stream for remote pods.
    pub async fn notify_task_wakeup(&self, task_id: Uuid) -> Result<(), StreamPubSubError> {
        self.task_wakeups.hub.notify(task_id);

        let mut conn = self.connection_pool.get_connection().await?;
        let _: redis::Value = conn
            .cmd(
                &redis::cmd("EVAL")
                    .arg(PUBLISH_STREAM_SCRIPT)
                    .arg(1)
                    .arg(&self.task_wakeups.stream_key)
                    .arg(TASK_WAKEUP_STREAM_MAXLEN)
                    .arg(task_id.to_string())
                    .arg(TASK_WAKEUP_STREAM_IDLE_TTL_SECS)
                    .clone(),
            )
            .await?;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn task_wakeup_waiter_count(&self) -> usize {
        self.task_wakeups.hub.active_waiter_count()
    }

    /// Keep run notifications through the configured maximum run plus delivery grace.
    pub fn with_run_timeout(mut self, timeout: Duration) -> Self {
        self.run_idle_ttl_secs = timeout.as_secs().saturating_add(60 * 60);
        self
    }

    /// Round-trip a PING on the pooled connection.
    pub async fn ping(&self) -> Result<(), StreamPubSubError> {
        let mut conn = self.connection_pool.get_connection().await?;
        conn.cmd(&redis::cmd("PING")).await?;
        Ok(())
    }
}

#[async_trait]
impl McpSseTransportSessionStore for RedisStreamsPubSub {
    async fn put_mcp_sse_transport_session(
        &self,
        binding: McpSseTransportSessionBinding,
        ttl: Duration,
    ) -> Result<(), StreamPubSubError> {
        let key = mcp_sse_transport_session_key(&binding.transport_session_id);
        let payload = serde_json::to_string(&binding)
            .map_err(|e| StreamPubSubError::SerializationError(e.to_string()))?;
        let ttl_secs = ttl.as_secs().max(1);

        let mut conn = self.connection_pool.get_connection().await?;
        let _: redis::Value = conn
            .cmd(
                &redis::cmd("SET")
                    .arg(&key)
                    .arg(&payload)
                    .arg("EX")
                    .arg(ttl_secs)
                    .clone(),
            )
            .await?;

        Ok(())
    }

    async fn get_mcp_sse_transport_session(
        &self,
        transport_session_id: Uuid,
    ) -> Result<Option<McpSseTransportSessionBinding>, StreamPubSubError> {
        let key = mcp_sse_transport_session_key(&transport_session_id);
        // Scope the guard to the single GET: the expired-binding path
        // below re-enters the pool via `delete_mcp_sse_transport_session`,
        // and no guard should stay alive across another pool call (with
        // the old lock-holding Cluster guard this exact path deadlocked).
        let value = {
            let mut conn = self.connection_pool.get_connection().await?;
            conn.cmd(&redis::cmd("GET").arg(&key).clone()).await?
        };

        if matches!(value, redis::Value::Nil) {
            return Ok(None);
        }

        let payload: String = redis::from_redis_value_ref(&value)
            .map_err(|e| StreamPubSubError::SerializationError(e.to_string()))?;
        let binding: McpSseTransportSessionBinding = serde_json::from_str(&payload)
            .map_err(|e| StreamPubSubError::SerializationError(e.to_string()))?;

        if binding.is_expired() {
            self.delete_mcp_sse_transport_session(transport_session_id)
                .await?;
            return Ok(None);
        }

        Ok(Some(binding))
    }

    async fn delete_mcp_sse_transport_session(
        &self,
        transport_session_id: Uuid,
    ) -> Result<(), StreamPubSubError> {
        let key = mcp_sse_transport_session_key(&transport_session_id);
        let mut conn = self.connection_pool.get_connection().await?;
        let _: redis::Value = conn.cmd(&redis::cmd("DEL").arg(&key).clone()).await?;
        Ok(())
    }
}

#[async_trait]
impl StreamPublisher for RedisStreamsPubSub {
    async fn publish(&self, event: StreamEvent) -> Result<(), StreamPubSubError> {
        let stream_key = event.channel_name();
        let task_inbox_id = match &event {
            StreamEvent::TaskMessage { task_id, .. } => Uuid::parse_str(task_id).ok(),
            _ => None,
        };
        let idle_ttl = if matches!(event, StreamEvent::TaskMessage { .. }) {
            TASK_STREAM_IDLE_TTL_SECS
        } else {
            self.run_idle_ttl_secs
        };

        // Serialize the event to JSON
        let payload = serde_json::to_string(&event)
            .map_err(|e| StreamPubSubError::SerializationError(e.to_string()))?;

        let mut conn = self.connection_pool.get_connection().await?;

        // Append and renew expiry atomically, including for task inbox/cancel
        // streams and legacy unscoped keys.
        let _: redis::Value = conn
            .cmd(
                &redis::cmd("EVAL")
                    .arg(PUBLISH_STREAM_SCRIPT)
                    .arg(1)
                    .arg(&stream_key)
                    .arg(STREAM_MAXLEN)
                    .arg(&payload)
                    .arg(idle_ttl)
                    .clone(),
            )
            .await?;

        if let Some(task_id) = task_inbox_id {
            // Wake same-process inbox subscribers AFTER `stream:{task_id}` is
            // durable so their immediate `XRANGE` sees this entry.
            self.task_inboxes.hub.notify(task_id);
            // Best-effort cross-pod wakeup hint onto the single shared inbox
            // wakeup stream; `MultiplexedTaskSubscriber` keeps a fallback
            // `XRANGE` poll so a transient wakeup publish error never fails
            // durable message delivery.
            if let Err(e) = conn
                .cmd(
                    &redis::cmd("EVAL")
                        .arg(PUBLISH_STREAM_SCRIPT)
                        .arg(1)
                        .arg(&self.task_inboxes.stream_key)
                        .arg(TASK_WAKEUP_STREAM_MAXLEN)
                        .arg(task_id.to_string())
                        .arg(TASK_WAKEUP_STREAM_IDLE_TTL_SECS)
                        .clone(),
                )
                .await
            {
                tracing::debug!(
                    task_id = %task_id,
                    "Failed to publish task inbox wakeup hint to {}: {}",
                    self.task_inboxes.stream_key,
                    e
                );
            }
        }

        tracing::debug!(
            "Published stream event to Redis Streams channel {}",
            stream_key
        );

        Ok(())
    }
}

#[async_trait]
impl StreamSubscriberFactory for RedisStreamsPubSub {
    async fn subscribe_task(
        &self,
        task_id: Uuid,
    ) -> Result<Box<dyn StreamSubscriber>, StreamPubSubError> {
        let stream_key = legacy_channel_name(&task_id);
        // Register on the shared wakeup mux BEFORE the initial `XRANGE` replay
        // so any message published while replaying leaves a stored wakeup
        // permit and cannot be lost between replay and live delivery.
        let guard = self
            .task_inboxes
            .watch(task_id, TASK_WAKEUP_COLD_START_WAIT)
            .await;
        let (next_last_id, events, full_page) =
            drain_task_stream(&self.connection_pool, &stream_key, "0-0").await?;
        let last_id = next_last_id.unwrap_or_else(|| "0-0".to_string());
        Ok(Box::new(MultiplexedTaskSubscriber {
            pool: Arc::clone(&self.connection_pool),
            stream_key,
            guard,
            last_id,
            buffered: events.into(),
            needs_drain: full_page,
            failures: 0,
            fallback_poll_interval: self.task_inbox_fallback_poll,
            next_fallback_at: tokio::time::Instant::now() + self.task_inbox_fallback_poll,
            idle_interval: self.task_inbox_idle_interval,
        }))
    }
    async fn subscribe(
        &self,
        stream_id: Uuid,
    ) -> Result<Box<dyn StreamSubscriber>, StreamPubSubError> {
        let stream_key = legacy_channel_name(&stream_id);
        let reader = StreamReader::start(Arc::clone(&self.connection_pool), &stream_key).await?;

        tracing::debug!("Subscribed to Redis Streams channel: {}", stream_key);

        Ok(Box::new(RedisStreamsSubscriber { reader, stream_key }))
    }

    async fn subscribe_in_env(
        &self,
        env_id: Uuid,
        stream_id: Uuid,
    ) -> Result<Box<dyn StreamSubscriber>, StreamPubSubError> {
        let stream_key = channel_name(&env_id, &stream_id);
        let reader = StreamReader::start(Arc::clone(&self.connection_pool), &stream_key).await?;

        tracing::debug!("Subscribed to Redis Streams channel: {}", stream_key);

        Ok(Box::new(RedisStreamsSubscriber { reader, stream_key }))
    }
}

/// What a `StreamReader` hands its subscriber.
#[derive(Debug)]
enum ReaderItem {
    /// The `event` field of one stream entry.
    Entry(String),
    /// An `XREAD BLOCK` window passed with no new entries.
    Idle,
}

/// Reads one Redis stream on a background task and hands its entries to a
/// subscriber through a channel.
///
/// Callers race `next()` against timers in `select!`. An `XREAD BLOCK`
/// issued from `next()` is not cancel-safe: a dropped read stays queued on
/// the connection, and the reply it later receives is discarded, so the
/// entries it carried are lost. A channel receive is cancel-safe. The task
/// also owns the read position, so after a connection failure it reconnects
/// and resumes after the last entry it read instead of going silent. It
/// resumes at the oldest entry still kept, so entries trimmed meanwhile
/// (beyond the newest `STREAM_MAXLEN`) are skipped.
struct StreamReader {
    entries: mpsc::Receiver<ReaderItem>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for StreamReader {
    fn drop(&mut self) {
        // Ending the task drops its connection, which ends a parked XREAD.
        self.task.abort();
    }
}

impl StreamReader {
    /// Start reading `stream_key` after its current newest entry.
    async fn start(
        pool: Arc<RedisConnectionPool>,
        stream_key: &str,
    ) -> Result<Self, StreamPubSubError> {
        // Subscribers need their own dedicated connection since XREAD BLOCK holds it
        let conn = pool.create_subscriber_connection().await?;
        Self::start_on(pool, conn, stream_key).await
    }

    async fn start_on(
        pool: Arc<RedisConnectionPool>,
        conn: SubscriberConnection,
        stream_key: &str,
    ) -> Result<Self, StreamPubSubError> {
        Self::start_on_at(pool, conn, stream_key, None).await
    }

    async fn start_on_at(
        pool: Arc<RedisConnectionPool>,
        mut conn: SubscriberConnection,
        stream_key: &str,
        start: Option<&str>,
    ) -> Result<Self, StreamPubSubError> {
        let start_id = match start {
            Some(id) => id.to_string(),
            None => newest_entry_id(&mut conn, stream_key).await?,
        };
        let (tx, entries) = mpsc::channel(READER_BUFFER);
        let task = tokio::spawn(read_stream(
            pool,
            conn,
            stream_key.to_string(),
            start_id,
            tx,
        ));
        Ok(Self { entries, task })
    }
}

/// The id of a stream's newest entry, or `0-0` for an empty or missing one.
///
/// Reading from a concrete id instead of `$` delivers an entry added after
/// subscribe returns even when it lands before the first XREAD reaches the
/// server (subscribe-with-event publishes right after subscribing).
async fn newest_entry_id(
    conn: &mut SubscriberConnection,
    stream_key: &str,
) -> Result<String, StreamPubSubError> {
    let value = conn
        .cmd(
            redis::cmd("XREVRANGE")
                .arg(stream_key)
                .arg("+")
                .arg("-")
                .arg("COUNT")
                .arg(1),
        )
        .await?;
    let newest: StreamRangeReply = redis::from_redis_value(value)
        .map_err(|e| StreamPubSubError::SubscribeError(e.to_string()))?;
    Ok(newest
        .ids
        .into_iter()
        .next()
        .map(|entry| entry.id)
        .unwrap_or_else(|| "0-0".to_string()))
}

/// Delay before the reader's next reconnect after `failures` consecutive
/// failed attempts.
fn reconnect_delay(failures: u32) -> Duration {
    READER_RECONNECT_MIN
        .saturating_mul(1 << failures.saturating_sub(1).min(8))
        .min(READER_RECONNECT_MAX)
}

/// The reader task: XREAD after `last_id` until the subscriber goes away,
/// reconnecting with backoff after errors.
async fn read_stream(
    pool: Arc<RedisConnectionPool>,
    conn: SubscriberConnection,
    stream_key: String,
    mut last_id: String,
    tx: mpsc::Sender<ReaderItem>,
) {
    let mut conn = Some(conn);
    let mut failures: u32 = 0;
    loop {
        let reader = match conn.as_mut() {
            Some(reader) => reader,
            None => {
                tokio::select! {
                    _ = tokio::time::sleep(reconnect_delay(failures)) => {}
                    _ = tx.closed() => return,
                }
                match pool.create_subscriber_connection().await {
                    Ok(reconnected) => conn.insert(reconnected),
                    Err(e) => {
                        failures = failures.saturating_add(1);
                        tracing::debug!(
                            "Redis Streams subscriber reconnect failed on {}: {}",
                            stream_key,
                            e
                        );
                        continue;
                    }
                }
            }
        };

        let mut xread = redis::cmd("XREAD");
        xread
            .arg("BLOCK")
            .arg(XREAD_BLOCK_MS)
            .arg("COUNT")
            .arg(XREAD_COUNT)
            .arg("STREAMS")
            .arg(&stream_key)
            .arg(&last_id);
        let result = tokio::select! {
            result = reader.blocking_cmd(&xread) => result,
            _ = tx.closed() => return,
        };
        let reply = result.and_then(|value| {
            redis::from_redis_value::<Option<StreamReadReply>>(value)
                .map_err(|e| StreamPubSubError::SubscribeError(e.to_string()))
        });

        let reply = match reply {
            Ok(reply) => reply,
            Err(e) => {
                // Only the first failure of an outage is a warning; the
                // retries that follow would repeat it for every subscriber.
                if failures == 0 {
                    tracing::warn!(
                        "Redis Streams XREAD error on {}; resubscribing after {}: {}",
                        stream_key,
                        last_id,
                        e
                    );
                }
                conn = None;
                failures = failures.saturating_add(1);
                continue;
            }
        };
        if failures > 0 {
            tracing::info!(
                "Redis Streams subscriber on {} resumed after {}",
                stream_key,
                last_id
            );
            failures = 0;
        }

        let entries = reply
            .map(|reply| reply.keys.into_iter().flat_map(|key| key.ids).collect())
            .unwrap_or_else(Vec::new);
        if entries.is_empty() {
            // A full buffer means the subscriber is busy anyway.
            if let Err(mpsc::error::TrySendError::Closed(_)) = tx.try_send(ReaderItem::Idle) {
                return;
            }
            continue;
        }
        for entry in entries {
            last_id = entry.id.clone();
            let Some(payload) = entry.get::<String>("event") else {
                continue;
            };
            if tx.send(ReaderItem::Entry(payload)).await.is_err() {
                return;
            }
        }
    }
}

/// Fetch up to `XREAD_COUNT` entries from a per-task inbox stream strictly
/// after `last_id` (`"0-0"` reads from the beginning) using the shared pooled
/// connection — no dedicated subscriber TCP connection required.
async fn drain_task_stream(
    pool: &RedisConnectionPool,
    stream_key: &str,
    last_id: &str,
) -> Result<(Option<String>, Vec<StreamEvent>, bool), StreamPubSubError> {
    let start_arg = if last_id == "0-0" {
        "-".to_string()
    } else {
        format!("({last_id}")
    };
    let mut conn = pool.get_connection().await?;
    let value = conn
        .cmd(
            redis::cmd("XRANGE")
                .arg(stream_key)
                .arg(&start_arg)
                .arg("+")
                .arg("COUNT")
                .arg(XREAD_COUNT),
        )
        .await?;
    let range: StreamRangeReply = redis::from_redis_value(value)
        .map_err(|e| StreamPubSubError::SubscribeError(e.to_string()))?;
    let entry_count = range.ids.len();
    let mut next_last_id = None;
    let mut events = Vec::with_capacity(entry_count);
    for entry in range.ids {
        next_last_id = Some(entry.id.clone());
        let Some(payload) = entry.get::<String>("event") else {
            continue;
        };
        match serde_json::from_str::<StreamEvent>(&payload) {
            Ok(event) => events.push(event),
            Err(e) => tracing::warn!(
                "Skipping undecodable task inbox event from Redis Streams on {}: {}",
                stream_key,
                e
            ),
        }
    }
    Ok((next_last_id, events, entry_count >= XREAD_COUNT))
}

/// Task inbox subscriber backed by one shared `RedisTaskWakeupMux` reader plus
/// pooled `XRANGE` reads on `stream:{task_id}`.
///
/// Opens zero per-task Redis connections and spawns zero per-task background
/// tasks. Cancel-safe across `tokio::select!`: `needs_drain` is cleared only
/// after `XRANGE` succeeds and advances `last_id`, so cancelling `next()`
/// mid-read retries from the same `last_id` with no lost or duplicate entries.
struct MultiplexedTaskSubscriber {
    pool: Arc<RedisConnectionPool>,
    stream_key: String,
    guard: TaskWakeupGuard,
    last_id: String,
    buffered: VecDeque<StreamEvent>,
    needs_drain: bool,
    failures: u32,
    fallback_poll_interval: Duration,
    /// Persistent fallback deadline so cancelling `next()` in a caller's
    /// `tokio::select!` (such as the 2s DB cancel poll) never starves the
    /// rolling-upgrade `XRANGE` fallback.
    next_fallback_at: tokio::time::Instant,
    idle_interval: Duration,
}

#[async_trait]
impl StreamSubscriber for MultiplexedTaskSubscriber {
    async fn next(&mut self) -> StreamNext {
        let idle_deadline = tokio::time::Instant::now() + self.idle_interval;
        loop {
            if let Some(event) = self.buffered.pop_front() {
                return StreamNext::Event(event);
            }

            let now = tokio::time::Instant::now();
            if now >= self.next_fallback_at {
                self.needs_drain = true;
            }

            if self.needs_drain {
                if self.failures > 0 {
                    tokio::time::sleep(reconnect_delay(self.failures)).await;
                }
                match drain_task_stream(&self.pool, &self.stream_key, &self.last_id).await {
                    Ok((next_last_id, events, full_page)) => {
                        self.failures = 0;
                        if let Some(id) = next_last_id {
                            self.last_id = id;
                        }
                        self.buffered.extend(events);
                        self.needs_drain = full_page;
                        self.next_fallback_at =
                            tokio::time::Instant::now() + self.fallback_poll_interval;
                        if let Some(event) = self.buffered.pop_front() {
                            return StreamNext::Event(event);
                        }
                    }
                    Err(e) => {
                        if self.failures == 0 {
                            tracing::warn!(
                                "Redis task inbox XRANGE error on {} after {}: {}",
                                self.stream_key,
                                self.last_id,
                                e
                            );
                        }
                        self.failures = self.failures.saturating_add(1);
                        continue;
                    }
                }
            }

            let now = tokio::time::Instant::now();
            if now >= idle_deadline {
                return StreamNext::Idle;
            }
            let fallback_at = self.next_fallback_at.min(idle_deadline);

            tokio::select! {
                biased;
                _ = self.guard.notified() => {
                    self.needs_drain = true;
                }
                _ = tokio::time::sleep_until(fallback_at) => {
                    self.needs_drain = true;
                }
            }
        }
    }
}

/// Redis Streams subscriber, fed by its own `StreamReader`.
pub struct RedisStreamsSubscriber {
    reader: StreamReader,
    stream_key: String,
}

#[async_trait]
impl StreamSubscriber for RedisStreamsSubscriber {
    async fn next(&mut self) -> StreamNext {
        loop {
            match self.reader.entries.recv().await {
                Some(ReaderItem::Entry(payload)) => {
                    match serde_json::from_str::<StreamEvent>(&payload) {
                        Ok(event) => return StreamNext::Event(event),
                        Err(e) => tracing::warn!(
                            "Skipping undecodable stream event from Redis Streams on {}: {}",
                            self.stream_key,
                            e
                        ),
                    }
                }
                Some(ReaderItem::Idle) => return StreamNext::Idle,
                None => return StreamNext::Closed,
            }
        }
    }
}

// ============================================================================
// Environment-level pub/sub (for dashboard real-time updates)
// ============================================================================

#[async_trait]
impl EnvPublisher for RedisStreamsPubSub {
    async fn publish_env(&self, event: EnvEvent) -> Result<(), StreamPubSubError> {
        let completed_task_id = match &event {
            EnvEvent::TaskComplete { task_id, .. } => {
                // Wake same-process waiters immediately even before Redis I/O.
                self.task_wakeups.hub.notify(*task_id);
                Some(*task_id)
            }
            _ => None,
        };
        let env_id = event.env_id();
        let stream_key = env_channel_name(&env_id);

        // Serialize the event to JSON
        let payload = serde_json::to_string(&event)
            .map_err(|e| StreamPubSubError::SerializationError(e.to_string()))?;

        let mut conn = self.connection_pool.get_connection().await?;

        // XADD with MAXLEN ~ to cap stream size
        let _: redis::Value = conn
            .cmd(
                &redis::cmd("XADD")
                    .arg(&stream_key)
                    .arg("MAXLEN")
                    .arg("~")
                    .arg(STREAM_MAXLEN)
                    .arg("*")
                    .arg("event")
                    .arg(&payload)
                    .clone(),
            )
            .await?;

        tracing::debug!(
            "Published env event to Redis Streams channel {}",
            stream_key
        );

        if let Some(task_id) = completed_task_id {
            // Best-effort cross-pod wakeup hint; `::hot::task/await` keeps a
            // DB polling fallback so a transient wakeup publish error never
            // fails `publish_env`.
            if let Err(e) = conn
                .cmd(
                    &redis::cmd("EVAL")
                        .arg(PUBLISH_STREAM_SCRIPT)
                        .arg(1)
                        .arg(&self.task_wakeups.stream_key)
                        .arg(TASK_WAKEUP_STREAM_MAXLEN)
                        .arg(task_id.to_string())
                        .arg(TASK_WAKEUP_STREAM_IDLE_TTL_SECS)
                        .clone(),
                )
                .await
            {
                tracing::debug!(
                    task_id = %task_id,
                    "Failed to publish task completion wakeup hint to {}: {}",
                    self.task_wakeups.stream_key,
                    e
                );
            }
        }

        Ok(())
    }
}

#[async_trait]
impl EnvSubscriberFactory for RedisStreamsPubSub {
    async fn subscribe_env(
        &self,
        env_id: Uuid,
    ) -> Result<Box<dyn EnvSubscriber>, StreamPubSubError> {
        let stream_key = env_channel_name(&env_id);
        let reader = StreamReader::start(Arc::clone(&self.connection_pool), &stream_key).await?;

        tracing::debug!("Subscribed to Redis Streams env channel: {}", stream_key);

        Ok(Box::new(RedisEnvSubscriber { reader, stream_key }))
    }
}

/// Redis Streams environment subscriber, fed by its own `StreamReader`.
pub struct RedisEnvSubscriber {
    reader: StreamReader,
    stream_key: String,
}

#[async_trait]
impl EnvSubscriber for RedisEnvSubscriber {
    async fn next(&mut self) -> Option<EnvEvent> {
        loop {
            match self.reader.entries.recv().await {
                Some(ReaderItem::Entry(payload)) => {
                    match serde_json::from_str::<EnvEvent>(&payload) {
                        Ok(event) => return Some(event),
                        Err(e) => tracing::warn!(
                            "Skipping undecodable env event from Redis Streams on {}: {}",
                            self.stream_key,
                            e
                        ),
                    }
                }
                Some(ReaderItem::Idle) => return None,
                None => {
                    // The reader only stops once dropped. Callers treat
                    // `None` as poll-again, so pace them rather than spin.
                    tokio::time::sleep(READER_RECONNECT_MAX).await;
                    return None;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Open a Redis client, returning None when no local Redis is reachable.
    /// Lets tests skip cleanly in environments without Redis (CI without the
    /// service, sandboxed builds) instead of failing. `REDIS_URL` (or
    /// `HOT_REDIS_URI`) picks the server and logical database.
    async fn try_client() -> Option<redis::Client> {
        let uri = std::env::var("REDIS_URL")
            .ok()
            .or_else(|| {
                crate::environment::read_optional("HOT_REDIS_URI")
                    .ok()
                    .flatten()
            })
            .unwrap_or_else(|| "redis://127.0.0.1/".to_string());
        let client = redis::Client::open(uri.as_str()).ok()?;
        // Fail fast if Redis isn't actually running.
        client.get_multiplexed_async_connection().await.ok()?;
        Some(client)
    }

    #[tokio::test]
    async fn task_inbox_replays_early_messages_and_keys_expire() {
        let Some(client) = try_client().await else {
            return;
        };
        let pubsub = RedisStreamsPubSub::new(client.clone());
        let task = Uuid::new_v4();
        let env = Uuid::new_v4();
        pubsub
            .publish(StreamEvent::TaskMessage {
                task_id: task.to_string(),
                env_id: Some(env),
                payload: serde_json::json!("early"),
            })
            .await
            .unwrap();
        let mut inbox = pubsub.subscribe_task(task).await.unwrap();
        let item = tokio::time::timeout(Duration::from_secs(2), inbox.next())
            .await
            .unwrap();
        assert!(
            matches!(item, StreamNext::Event(StreamEvent::TaskMessage { payload, .. }) if payload == "early")
        );
        let mut conn = client.get_multiplexed_async_connection().await.unwrap();
        let key = legacy_channel_name(&task);
        let ttl: i64 = redis::cmd("TTL")
            .arg(&key)
            .query_async(&mut conn)
            .await
            .unwrap();
        assert!(ttl > 7 * 86400 && ttl <= 8 * 86400, "ttl={ttl}");
        let _: i64 = redis::cmd("DEL")
            .arg(&key)
            .query_async(&mut conn)
            .await
            .unwrap();
    }

    /// A server that answers each command with OK until one names
    /// `stall_on`, then never answers again.
    async fn stalling_server(stall_on: &'static str) -> std::net::SocketAddr {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buffer = vec![0; 4096];
            while let Ok(read) = socket.read(&mut buffer).await {
                let request = String::from_utf8_lossy(&buffer[..read]).to_string();
                if read == 0 || request.contains(stall_on) {
                    break;
                }
                let commands = request.split("\r\n").filter(|line| line.starts_with('*'));
                let replies = "+OK\r\n".repeat(commands.count());
                if socket.write_all(replies.as_bytes()).await.is_err() {
                    break;
                }
            }
            // Hold the connection open, silent.
            std::future::pending::<()>().await;
        });
        address
    }

    #[tokio::test]
    async fn subscribing_bounds_its_first_read_like_a_short_command() {
        let address = stalling_server("XREVRANGE").await;
        let client = redis::Client::open(format!("redis://{address}/")).unwrap();
        let pool = RedisConnectionPool::new_standalone(client);
        let mut conn = pool.create_subscriber_connection().await.unwrap();
        // While the server is silent, the paused clock jumps to the next
        // deadline.
        tokio::time::pause();
        let started = tokio::time::Instant::now();
        let error = newest_entry_id(&mut conn, "stream:stalled")
            .await
            .unwrap_err()
            .to_string();
        let waited = started.elapsed();
        assert!(
            waited >= REDIS_COMMAND_TIMEOUT && waited < REDIS_SUBSCRIBER_COMMAND_TIMEOUT,
            "{waited:?}"
        );
        assert!(
            error.contains("Redis command timed out after 10s"),
            "{error}"
        );
    }

    fn standalone_slot(
        pool: &RedisConnectionPool,
    ) -> &Arc<Mutex<CachedConn<MultiplexedConnection>>> {
        match pool {
            RedisConnectionPool::Standalone { cached_conn, .. } => cached_conn,
            RedisConnectionPool::Cluster { .. } => unreachable!("standalone pool expected"),
        }
    }

    /// Slot checkout/eviction semantics shared by both pool arms. The
    /// cluster arm reuses exactly this generic code (its guard owns a
    /// clone of the connection handle, never the slot mutex), so this
    /// also covers the paths a real cluster deployment exercises.
    #[tokio::test]
    async fn slot_checkout_clones_cached_and_generations_gate_eviction() {
        let slot: Mutex<CachedConn<u8>> = Mutex::new(CachedConn::default());

        // Cold checkout dials and installs generation 1.
        let (conn, generation) = checkout_conn(&slot, async { Ok(7) }).await.unwrap();
        assert_eq!((conn, generation), (7, 1));

        // Warm checkout clones the cached handle without dialing.
        let (conn, generation) = checkout_conn(&slot, async {
            panic!("must not dial while a healthy connection is cached")
        })
        .await
        .unwrap();
        assert_eq!((conn, generation), (7, 1));

        // Stale-generation eviction is a no-op...
        evict_slot(&slot, 0).await;
        assert!(slot.lock().await.conn.is_some());

        // ...matching-generation eviction clears the slot...
        evict_slot(&slot, 1).await;
        assert!(slot.lock().await.conn.is_none());

        // ...and the next checkout dials fresh under a bumped generation,
        // so the evicting (now stale) guard can no longer clobber it.
        let (conn, generation) = checkout_conn(&slot, async { Ok(9) }).await.unwrap();
        assert_eq!((conn, generation), (9, 2));
        evict_slot(&slot, 1).await;
        assert!(slot.lock().await.conn.is_some());
    }

    /// A failed dial must leave the slot untouched — nothing cached and
    /// the generation unchanged — so the failure cannot masquerade as an
    /// install.
    #[tokio::test]
    async fn slot_checkout_failed_dial_leaves_slot_untouched() {
        let slot: Mutex<CachedConn<u8>> = Mutex::new(CachedConn::default());
        checkout_conn(&slot, async {
            Err(StreamPubSubError::ConnectionError("dial failed".into()))
        })
        .await
        .expect_err("dial failure must surface");

        let slot = slot.lock().await;
        assert!(slot.conn.is_none());
        assert_eq!(slot.generation, 0);
    }

    /// Eviction replaces the pool slot only; in-flight holders keep their
    /// own clone of the old connection (Arc semantics) and the next
    /// `get_connection` reconnects fresh.
    #[tokio::test]
    async fn evicting_standalone_connection_clears_slot_without_breaking_holders() {
        let Some(client) = try_client().await else {
            eprintln!("skipping: Redis not available");
            return;
        };
        let pool = RedisConnectionPool::new_standalone(client);

        let mut guard = pool.get_connection().await.unwrap();
        assert!(standalone_slot(&pool).lock().await.conn.is_some());

        guard.evict_cached_connection().await;
        assert!(standalone_slot(&pool).lock().await.conn.is_none());

        // The in-flight holder's clone still works after eviction.
        let pong = guard.cmd(&redis::cmd("PING")).await.unwrap();
        assert_eq!(pong, redis::Value::SimpleString("PONG".to_string()));

        // The next caller reconnects and repopulates the slot.
        let _guard2 = pool.get_connection().await.unwrap();
        assert!(standalone_slot(&pool).lock().await.conn.is_some());
    }

    /// A late-failing holder of an OLD connection must not evict a healthy
    /// replacement another caller already installed: eviction only applies
    /// while the slot generation still matches the guard's checkout.
    #[tokio::test]
    async fn stale_guard_eviction_does_not_clobber_replacement() {
        let Some(client) = try_client().await else {
            eprintln!("skipping: Redis not available");
            return;
        };
        let pool = RedisConnectionPool::new_standalone(client);

        // A slow holder checks out the first-generation connection.
        let mut stale_guard = pool.get_connection().await.unwrap();

        // Another holder of the same generation fails first and evicts...
        pool.get_connection()
            .await
            .unwrap()
            .evict_cached_connection()
            .await;
        assert!(standalone_slot(&pool).lock().await.conn.is_none());

        // ...and a third caller installs a healthy replacement (new
        // generation).
        let _fresh = pool.get_connection().await.unwrap();
        assert!(standalone_slot(&pool).lock().await.conn.is_some());

        // The slow holder now fails late and tries to evict: the slot has
        // moved on, so the healthy replacement must survive.
        stale_guard.evict_cached_connection().await;
        assert!(
            standalone_slot(&pool).lock().await.conn.is_some(),
            "stale-generation eviction must not clobber the replacement"
        );
    }

    /// A server error reply is a per-command failure and must not evict the
    /// pooled connection.
    #[tokio::test]
    async fn server_error_reply_does_not_evict_pooled_connection() {
        let Some(client) = try_client().await else {
            eprintln!("skipping: Redis not available");
            return;
        };
        let pool = RedisConnectionPool::new_standalone(client);

        let mut guard = pool.get_connection().await.unwrap();
        guard
            .cmd(
                &redis::cmd("EVAL")
                    .arg("return redis.error_reply('boom')")
                    .arg(0)
                    .clone(),
            )
            .await
            .expect_err("script error reply must surface as Err");
        assert!(
            standalone_slot(&pool).lock().await.conn.is_some(),
            "a server error reply must not evict the pooled connection"
        );
    }

    /// A connection-level failure (stand-in for a silent LB drop) must
    /// evict the pooled connection so the next caller reconnects.
    #[tokio::test]
    async fn connection_level_failure_evicts_pooled_connection() {
        let Some(client) = try_client().await else {
            eprintln!("skipping: Redis not available");
            return;
        };
        let pool = RedisConnectionPool::new_standalone(client.clone());

        let mut guard = pool.get_connection().await.unwrap();
        let client_id: i64 =
            redis::from_redis_value_ref(&guard.cmd(redis::cmd("CLIENT").arg("ID")).await.unwrap())
                .unwrap();

        let mut admin = client.get_multiplexed_async_connection().await.unwrap();
        let killed: i64 = redis::cmd("CLIENT")
            .arg("KILL")
            .arg("ID")
            .arg(client_id)
            .query_async(&mut admin)
            .await
            .unwrap();
        assert_eq!(killed, 1);

        guard
            .cmd(&redis::cmd("PING"))
            .await
            .expect_err("command on a killed connection must fail");
        assert!(
            standalone_slot(&pool).lock().await.conn.is_none(),
            "a connection-level failure must evict the pooled connection"
        );

        // The next caller reconnects fresh.
        let mut guard2 = pool.get_connection().await.unwrap();
        guard2.cmd(&redis::cmd("PING")).await.unwrap();
    }

    /// End-to-end expiry arc against a real standalone Redis: reading an
    /// expired binding must delete it and return `None`. This is the exact
    /// get -> delete pool re-entry that self-deadlocked when the Cluster
    /// guard held the pool mutex for its whole lifetime; the checkout and
    /// guard-scoping code is shared between arms, so this pins the
    /// re-entrant behavior for the paths a cluster deployment runs too.
    #[tokio::test]
    async fn expired_session_read_deletes_binding_and_returns_none() {
        let Some(client) = try_client().await else {
            eprintln!("skipping: Redis not available");
            return;
        };
        let pubsub = RedisStreamsPubSub::new(client.clone());

        let transport_session_id = Uuid::new_v4();
        let binding = McpSseTransportSessionBinding {
            transport_session_id,
            env_id: Uuid::new_v4(),
            route_key: "test-route".to_string(),
            principal: crate::stream::McpSseTransportPrincipal::Session(Uuid::new_v4()),
            // Already logically expired while the Redis TTL (60s) still
            // holds the key, forcing get() down its eager-delete path.
            expires_at: chrono::Utc::now() - chrono::Duration::seconds(30),
        };
        pubsub
            .put_mcp_sse_transport_session(binding, Duration::from_secs(60))
            .await
            .unwrap();

        let got = tokio::time::timeout(
            Duration::from_secs(5),
            pubsub.get_mcp_sse_transport_session(transport_session_id),
        )
        .await
        .expect("expiry path must not hang on pool re-entry")
        .unwrap();
        assert_eq!(got, None, "expired binding must read as absent");

        // The expiry path must have deleted the key eagerly, not left it
        // for the TTL.
        let mut admin = client.get_multiplexed_async_connection().await.unwrap();
        let exists: i64 = redis::cmd("EXISTS")
            .arg(mcp_sse_transport_session_key(&transport_session_id))
            .query_async(&mut admin)
            .await
            .unwrap();
        assert_eq!(exists, 0, "expired binding must be deleted on read");
    }

    fn stream_data(env_id: Uuid, stream_id: Uuid, n: i64) -> StreamEvent {
        StreamEvent::StreamData {
            stream_data_id: Uuid::now_v7(),
            run_id: Uuid::nil(),
            env_id: Some(env_id),
            stream_id,
            data_type: "test".to_string(),
            payload: serde_json::json!(n),
        }
    }

    async fn next_payload(sub: &mut Box<dyn StreamSubscriber>) -> i64 {
        loop {
            let next = tokio::time::timeout(Duration::from_secs(10), sub.next())
                .await
                .expect("subscriber must deliver the published entry");
            match next {
                StreamNext::Event(StreamEvent::StreamData { payload, .. }) => {
                    return payload.as_i64().unwrap();
                }
                StreamNext::Idle => continue,
                other => panic!("unexpected subscriber item: {other:?}"),
            }
        }
    }

    async fn delete_key(client: &redis::Client, key: &str) {
        let mut admin = client.get_multiplexed_async_connection().await.unwrap();
        let _: redis::RedisResult<()> = redis::cmd("DEL").arg(key).query_async(&mut admin).await;
    }

    #[tokio::test]
    async fn run_stream_expiry_covers_configured_long_runs() {
        let Some(client) = try_client().await else {
            return;
        };
        let pubsub = RedisStreamsPubSub::new(client.clone())
            .with_run_timeout(Duration::from_secs(24 * 3600));
        let event = stream_data(Uuid::now_v7(), Uuid::now_v7(), 1);
        let key = event.channel_name();
        pubsub.publish(event).await.unwrap();
        let mut admin = client.get_multiplexed_async_connection().await.unwrap();
        let ttl: i64 = redis::cmd("TTL")
            .arg(&key)
            .query_async(&mut admin)
            .await
            .unwrap();
        assert!((25 * 3600 - 1..=25 * 3600).contains(&ttl));
        delete_key(&client, &key).await;
    }

    #[tokio::test]
    async fn publishing_expires_and_renews_scoped_and_task_streams() {
        let Some(client) = try_client().await else {
            eprintln!("skipping: Redis not available");
            return;
        };
        let pubsub = RedisStreamsPubSub::new(client.clone());
        let env_id = Uuid::now_v7();
        let stream_id = Uuid::now_v7();
        let task_id = Uuid::now_v7();
        let events = [
            stream_data(env_id, stream_id, 1),
            StreamEvent::TaskMessage {
                task_id: task_id.to_string(),
                env_id: Some(env_id),
                payload: serde_json::json!({"$cancel": true}),
            },
        ];
        let mut admin = client.get_multiplexed_async_connection().await.unwrap();
        for event in events {
            let expected_ttl = if matches!(event, StreamEvent::TaskMessage { .. }) {
                TASK_STREAM_IDLE_TTL_SECS
            } else {
                DEFAULT_RUN_STREAM_IDLE_TTL_SECS
            } as i64;
            let key = event.channel_name();
            for _ in 0..2 {
                pubsub.publish(event.clone()).await.unwrap();
                let ttl: i64 = redis::cmd("TTL")
                    .arg(&key)
                    .query_async(&mut admin)
                    .await
                    .unwrap();
                assert!((expected_ttl - 1..=expected_ttl).contains(&ttl));
                // Model a nearly expired key; the next publish must renew it.
                let _: i64 = redis::cmd("EXPIRE")
                    .arg(&key)
                    .arg(60)
                    .query_async(&mut admin)
                    .await
                    .unwrap();
            }
            let count: i64 = redis::cmd("XLEN")
                .arg(&key)
                .query_async(&mut admin)
                .await
                .unwrap();
            assert_eq!(count, 2, "expiry must preserve the entries");
            delete_key(&client, &key).await;
        }
    }

    #[test]
    fn reconnect_delay_backs_off_to_cap() {
        assert_eq!(reconnect_delay(0), READER_RECONNECT_MIN);
        assert_eq!(reconnect_delay(1), READER_RECONNECT_MIN);
        assert_eq!(reconnect_delay(2), READER_RECONNECT_MIN * 2);
        assert_eq!(reconnect_delay(3), READER_RECONNECT_MIN * 4);
        assert_eq!(reconnect_delay(50), READER_RECONNECT_MAX);
    }

    /// SSE handlers race `next()` against a poll timer, cancelling it about
    /// once a second while a stream is quiet. Entries published after such
    /// idle time must all arrive: a cancelled wait used to leave an XREAD
    /// queued on the connection that then consumed an entry and dropped it.
    #[tokio::test]
    async fn subscriber_keeps_entries_through_cancelled_waits() {
        let Some(client) = try_client().await else {
            eprintln!("skipping: Redis not available");
            return;
        };
        let pubsub = RedisStreamsPubSub::new(client.clone());
        let env_id = Uuid::now_v7();
        let stream_id = Uuid::now_v7();
        let mut sub = pubsub.subscribe_in_env(env_id, stream_id).await.unwrap();

        for _ in 0..6 {
            let waited = tokio::time::timeout(Duration::from_millis(200), sub.next()).await;
            assert!(waited.is_err(), "nothing was published yet: {waited:?}");
        }

        // An undecodable entry is skipped, not the end of the subscription.
        let mut admin = client.get_multiplexed_async_connection().await.unwrap();
        let _: String = redis::cmd("XADD")
            .arg(channel_name(&env_id, &stream_id))
            .arg("*")
            .arg("event")
            .arg("not json")
            .query_async(&mut admin)
            .await
            .unwrap();
        for n in 1..=3 {
            pubsub
                .publish(stream_data(env_id, stream_id, n))
                .await
                .unwrap();
        }
        for n in 1..=3 {
            assert_eq!(next_payload(&mut sub).await, n);
        }

        delete_key(&client, &channel_name(&env_id, &stream_id)).await;
    }

    /// subscribe-with-event publishes right after subscribing; an entry that
    /// lands before the subscriber's first read must still be delivered.
    #[tokio::test]
    async fn subscriber_delivers_entries_published_before_its_first_read() {
        let Some(client) = try_client().await else {
            eprintln!("skipping: Redis not available");
            return;
        };
        let pubsub = RedisStreamsPubSub::new(client.clone());
        let env_id = Uuid::now_v7();
        let stream_id = Uuid::now_v7();
        // Older entries are history, not news for a new subscriber.
        pubsub
            .publish(stream_data(env_id, stream_id, 0))
            .await
            .unwrap();
        let mut sub = pubsub.subscribe_in_env(env_id, stream_id).await.unwrap();
        pubsub
            .publish(stream_data(env_id, stream_id, 1))
            .await
            .unwrap();
        assert_eq!(next_payload(&mut sub).await, 1);

        delete_key(&client, &channel_name(&env_id, &stream_id)).await;
    }

    /// A Redis blip (failover, LB reset) kills the subscriber's connection.
    /// The reader must reconnect and resume after the last entry it
    /// delivered, including entries published while it was disconnected.
    #[tokio::test]
    async fn subscriber_resumes_after_its_connection_is_killed() {
        let Some(client) = try_client().await else {
            eprintln!("skipping: Redis not available");
            return;
        };
        let pubsub = RedisStreamsPubSub::new(client.clone());
        let env_id = Uuid::now_v7();
        let stream_id = Uuid::now_v7();
        let stream_key = channel_name(&env_id, &stream_id);

        let mut conn = pubsub
            .connection_pool
            .create_subscriber_connection()
            .await
            .unwrap();
        let client_id: i64 =
            redis::from_redis_value_ref(&conn.cmd(redis::cmd("CLIENT").arg("ID")).await.unwrap())
                .unwrap();
        let reader = StreamReader::start_on(Arc::clone(&pubsub.connection_pool), conn, &stream_key)
            .await
            .unwrap();
        let mut sub: Box<dyn StreamSubscriber> = Box::new(RedisStreamsSubscriber {
            reader,
            stream_key: stream_key.clone(),
        });

        pubsub
            .publish(stream_data(env_id, stream_id, 1))
            .await
            .unwrap();
        assert_eq!(next_payload(&mut sub).await, 1);

        let mut admin = client.get_multiplexed_async_connection().await.unwrap();
        let killed: i64 = redis::cmd("CLIENT")
            .arg("KILL")
            .arg("ID")
            .arg(client_id)
            .query_async(&mut admin)
            .await
            .unwrap();
        assert_eq!(killed, 1);

        // Published while the reader is down or backing off.
        pubsub
            .publish(stream_data(env_id, stream_id, 2))
            .await
            .unwrap();
        assert_eq!(next_payload(&mut sub).await, 2);
        pubsub
            .publish(stream_data(env_id, stream_id, 3))
            .await
            .unwrap();
        assert_eq!(next_payload(&mut sub).await, 3);

        delete_key(&client, &stream_key).await;
    }

    /// Dashboard subscribers race `next()` against shutdown and revalidation
    /// timers too; the first run event of a connection used to be lost.
    #[tokio::test]
    async fn env_subscriber_keeps_entries_through_cancelled_waits() {
        let Some(client) = try_client().await else {
            eprintln!("skipping: Redis not available");
            return;
        };
        let pubsub = RedisStreamsPubSub::new(client.clone());
        let env_id = Uuid::now_v7();
        let mut sub = pubsub.subscribe_env(env_id).await.unwrap();

        for _ in 0..3 {
            let waited = tokio::time::timeout(Duration::from_millis(200), sub.next()).await;
            assert!(waited.is_err(), "nothing was published yet: {waited:?}");
        }
        let stream_id = Uuid::now_v7();
        pubsub
            .publish_env(EnvEvent::StreamCreated {
                stream_id,
                env_id,
                project_id: None,
            })
            .await
            .unwrap();
        let event = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Some(event) = sub.next().await {
                    break event;
                }
            }
        })
        .await
        .expect("env subscriber must deliver the published entry");
        assert!(
            matches!(event, EnvEvent::StreamCreated { stream_id: got, .. } if got == stream_id),
            "{event:?}"
        );

        delete_key(&client, &env_channel_name(&env_id)).await;
    }

    /// Parse one RESP2 array command from `buf`, returning its uppercased
    /// command name if a full frame is present and draining the consumed bytes.
    fn try_take_resp_command(buf: &mut Vec<u8>) -> Option<String> {
        let text = std::str::from_utf8(buf).ok()?;
        if !text.starts_with('*') {
            return None;
        }
        let first_crlf = text.find("\r\n")?;
        let arg_count: usize = text[1..first_crlf].parse().ok()?;
        let mut cursor = first_crlf + 2;
        let mut cmd_name: Option<String> = None;
        for idx in 0..arg_count {
            let rest = text.get(cursor..)?;
            if !rest.starts_with('$') {
                return None;
            }
            let len_end = rest.find("\r\n")?;
            let bulk_len: usize = rest[1..len_end].parse().ok()?;
            let body_start = cursor + len_end + 2;
            let body_end = body_start + bulk_len;
            if text.len() < body_end + 2 {
                return None;
            }
            if idx == 0 {
                cmd_name = Some(text[body_start..body_end].to_ascii_uppercase());
            }
            cursor = body_end + 2;
        }
        buf.drain(..cursor);
        cmd_name
    }

    fn encode_xread_single_wakeup(stream_key: &str, entry_id: &str, task_id: Uuid) -> Vec<u8> {
        let payload = task_id.to_string();
        format!(
            "*1\r\n*2\r\n${}\r\n{}\r\n*1\r\n*2\r\n${}\r\n{}\r\n*2\r\n$5\r\nevent\r\n${}\r\n{}\r\n",
            stream_key.len(),
            stream_key,
            entry_id.len(),
            entry_id,
            payload.len(),
            payload
        )
        .into_bytes()
    }

    /// Minimal mock RESP server supporting `XREVRANGE` and blocking `XREAD`
    /// delivery of task wakeup IDs, counting every accepted TCP connection.
    async fn mock_wakeup_redis_server(
        stream_key: String,
    ) -> (
        std::net::SocketAddr,
        Arc<std::sync::atomic::AtomicUsize>,
        mpsc::UnboundedSender<Uuid>,
    ) {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let accept_count = Arc::new(AtomicUsize::new(0));
        let accept_count_bg = Arc::clone(&accept_count);
        let (tx, rx) = mpsc::unbounded_channel::<Uuid>();
        let rx = Arc::new(Mutex::new(rx));

        tokio::spawn(async move {
            let seq = Arc::new(AtomicUsize::new(1));
            while let Ok((mut socket, _)) = listener.accept().await {
                accept_count_bg.fetch_add(1, Ordering::SeqCst);
                let rx = Arc::clone(&rx);
                let seq = Arc::clone(&seq);
                let stream_key = stream_key.clone();
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 4096];
                    loop {
                        while let Some(cmd) = try_take_resp_command(&mut buf) {
                            let reply: Vec<u8> = match cmd.as_str() {
                                "XREVRANGE" => b"*0\r\n".to_vec(),
                                "XREAD" => {
                                    let Some(task_id) = rx.lock().await.recv().await else {
                                        return;
                                    };
                                    let id = format!(
                                        "1700000000000-{}",
                                        seq.fetch_add(1, Ordering::SeqCst)
                                    );
                                    encode_xread_single_wakeup(&stream_key, &id, task_id)
                                }
                                _ => b"+OK\r\n".to_vec(),
                            };
                            if socket.write_all(&reply).await.is_err() {
                                return;
                            }
                        }
                        match socket.read(&mut chunk).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => buf.extend_from_slice(&chunk[..n]),
                        }
                    }
                });
            }
        });

        (address, accept_count, tx)
    }

    #[tokio::test]
    async fn redis_task_wakeup_multiplexes_many_waiters_over_one_connection_and_cleans_up() {
        use std::sync::atomic::Ordering;

        let stream_key = format!("hot:task:wakeup:test:{}", Uuid::now_v7());
        let (address, accept_count, push_wakeup) =
            mock_wakeup_redis_server(stream_key.clone()).await;
        let client = redis::Client::open(format!("redis://{address}/")).unwrap();
        let pubsub = RedisStreamsPubSub::with_wakeup_stream_key(client, stream_key);

        let task_ids: Vec<Uuid> = (0..10).map(|_| Uuid::now_v7()).collect();
        let mut guards = Vec::with_capacity(task_ids.len());
        for &id in &task_ids {
            guards.push(
                pubsub
                    .watch_task_completion(id, Duration::from_secs(1))
                    .await,
            );
        }
        // A second waiter on the first task through a cloned handle must reuse
        // the exact same hub and single background subscriber connection.
        let cloned = pubsub.clone();
        let dup_guard = cloned
            .watch_task_completion(task_ids[0], Duration::from_secs(1))
            .await;

        assert_eq!(pubsub.task_wakeup_waiter_count(), 11);
        assert_eq!(pubsub.task_wakeups.hub.tracked_task_count(), 10);
        assert_eq!(
            accept_count.load(Ordering::SeqCst),
            1,
            "all concurrent task/await waiters must share ONE Redis subscriber connection"
        );

        let started = std::time::Instant::now();
        push_wakeup.send(task_ids[0]).unwrap();
        tokio::time::timeout(Duration::from_millis(200), guards[0].notified())
            .await
            .expect("first waiter for task_ids[0] must wake");
        tokio::time::timeout(Duration::from_millis(200), dup_guard.notified())
            .await
            .expect("second waiter for task_ids[0] must wake");
        assert!(
            started.elapsed() < Duration::from_millis(100),
            "wakeup should arrive well below the 100ms poll floor (took {:?})",
            started.elapsed()
        );

        // Unrelated waiter stays silent until its own task_id is published.
        assert!(
            tokio::time::timeout(Duration::from_millis(20), guards[1].notified())
                .await
                .is_err()
        );
        push_wakeup.send(task_ids[1]).unwrap();
        tokio::time::timeout(Duration::from_millis(200), guards[1].notified())
            .await
            .expect("waiter for task_ids[1] must wake on its own id");

        assert_eq!(
            accept_count.load(Ordering::SeqCst),
            1,
            "still only one subscriber connection after multiple deliveries"
        );

        drop(dup_guard);
        drop(guards);
        assert_eq!(pubsub.task_wakeup_waiter_count(), 0);
        assert_eq!(pubsub.task_wakeups.hub.tracked_task_count(), 0);
    }

    #[tokio::test]
    async fn redis_task_wakeup_cold_start_does_not_hang_on_stalling_server() {
        let address = stalling_server("XREVRANGE").await;
        let client = redis::Client::open(format!("redis://{address}/")).unwrap();
        let pubsub = RedisStreamsPubSub::new(client);

        let started = std::time::Instant::now();
        let guard = pubsub
            .watch_task_completion(Uuid::now_v7(), Duration::from_millis(40))
            .await;
        assert!(
            started.elapsed() < Duration::from_millis(250),
            "stalling Redis must not delay task/await past its cold-start cap: {:?}",
            started.elapsed()
        );
        assert_eq!(pubsub.task_wakeup_waiter_count(), 1);
        drop(guard);
        assert_eq!(pubsub.task_wakeup_waiter_count(), 0);
    }

    #[tokio::test]
    async fn redis_task_wakeup_delivers_cross_pod_publish_env_over_shared_reader() {
        let Some(client) = try_client().await else {
            eprintln!("skipping: Redis not available");
            return;
        };
        let stream_key = format!("hot:task:wakeup:test:{}", Uuid::now_v7());
        // Two independent instances simulate Pod A awaiting and Pod B completing.
        let pod_a = RedisStreamsPubSub::with_wakeup_stream_key(client.clone(), stream_key.clone());
        let pod_b = RedisStreamsPubSub::with_wakeup_stream_key(client.clone(), stream_key.clone());

        let env_id = Uuid::now_v7();
        let task_id = Uuid::now_v7();
        let guard = pod_a
            .watch_task_completion(task_id, Duration::from_secs(1))
            .await;

        let started = std::time::Instant::now();
        pod_b
            .publish_env(EnvEvent::TaskComplete {
                task_id,
                env_id,
                stream_id: Uuid::now_v7(),
                function_name: "::app/work".to_string(),
                status: "completed".to_string(),
                duration_ms: Some(12),
                error: None,
            })
            .await
            .unwrap();

        tokio::time::timeout(Duration::from_secs(2), guard.notified())
            .await
            .expect("Pod A must receive cross-pod wakeup from Pod B's publish_env");
        assert!(
            started.elapsed() < Duration::from_millis(100),
            "cross-pod wakeup should beat the 100ms poll floor: {:?}",
            started.elapsed()
        );

        drop(guard);
        assert_eq!(pod_a.task_wakeup_waiter_count(), 0);
        delete_key(&client, &stream_key).await;
        delete_key(&client, &env_channel_name(&env_id)).await;
    }

    /// When connecting succeeds but `XREAD` persistently fails (for example
    /// ACL `NOPERM` or `WRONGTYPE`), the reader must NOT reset its failure
    /// counter on connect (which would spin every 250ms) and must NOT call
    /// `notify_all()` on failed reconnects. Once `XREAD` finally succeeds, it
    /// recovers and wakes waiting callers once.
    #[tokio::test]
    async fn redis_task_wakeup_backs_off_without_waking_waiters_on_persistent_xread_error() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let fail_xread = Arc::new(AtomicBool::new(true));
        let fail_xread_bg = Arc::clone(&fail_xread);
        let xread_times: Arc<parking_lot::Mutex<Vec<std::time::Instant>>> =
            Arc::new(parking_lot::Mutex::new(Vec::new()));
        let xread_times_bg = Arc::clone(&xread_times);

        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let fail_xread = Arc::clone(&fail_xread_bg);
                let xread_times = Arc::clone(&xread_times_bg);
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 4096];
                    loop {
                        while let Some(cmd) = try_take_resp_command(&mut buf) {
                            let reply: Vec<u8> = match cmd.as_str() {
                                "XREVRANGE" => b"*0\r\n".to_vec(),
                                "XREAD" => {
                                    xread_times.lock().push(std::time::Instant::now());
                                    if fail_xread.load(Ordering::SeqCst) {
                                        b"-NOPERM this user has no permissions to run the 'xread' command\r\n".to_vec()
                                    } else {
                                        // Empty XREAD reply: signals healthy recovery.
                                        b"$-1\r\n".to_vec()
                                    }
                                }
                                _ => b"+OK\r\n".to_vec(),
                            };
                            if socket.write_all(&reply).await.is_err() {
                                return;
                            }
                        }
                        match socket.read(&mut chunk).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => buf.extend_from_slice(&chunk[..n]),
                        }
                    }
                });
            }
        });

        let stream_key = format!("hot:task:wakeup:noperm:{}", Uuid::now_v7());
        let client = redis::Client::open(format!("redis://{address}/")).unwrap();
        let pubsub = RedisStreamsPubSub::with_wakeup_stream_key(client, stream_key);

        let guard = pubsub
            .watch_task_completion(Uuid::now_v7(), Duration::from_secs(1))
            .await;

        // Wait until 3 failing XREAD attempts have occurred:
        // attempt 1 @ ~0ms -> fail (failures=1) -> sleep 250ms
        // attempt 2 @ ~250ms -> fail (failures=2) -> sleep 500ms
        // attempt 3 @ ~750ms -> fail (failures=3) -> sleep 1000ms
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        loop {
            if xread_times.lock().len() >= 3 {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for 3 XREAD attempts; saw {}",
                xread_times.lock().len()
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        let times = xread_times.lock().clone();
        let first_gap = times[1].saturating_duration_since(times[0]);
        let second_gap = times[2].saturating_duration_since(times[1]);
        assert!(
            first_gap >= Duration::from_millis(200),
            "first backoff should be ~250ms, got {first_gap:?}"
        );
        assert!(
            second_gap >= Duration::from_millis(420),
            "second backoff must double to ~500ms instead of resetting to 250ms; got {second_gap:?}"
        );

        // Waiter must NOT have been spuriously woken during the failing-XREAD
        // reconnect loop.
        assert!(
            tokio::time::timeout(Duration::from_millis(50), guard.notified())
                .await
                .is_err(),
            "failing reconnects must not spam hub.notify_all() before XREAD succeeds"
        );

        // Now allow XREAD to succeed: the next retry must recover and wake the
        // waiter once so it re-checks DB after the outage.
        fail_xread.store(false, Ordering::SeqCst);
        tokio::time::timeout(Duration::from_secs(2), guard.notified())
            .await
            .expect("waiter must be woken once XREAD recovers after an outage");
    }

    /// Even on a busy multi-tenant stream where other pods keep publishing
    /// completions continuously, a pod with zero local waiters must park its
    /// wakeup reader and close its Redis connection once the idle window
    /// elapses, then transparently reconnect when a new waiter arrives.
    #[tokio::test]
    async fn redis_task_wakeup_parks_and_closes_connection_when_idle_even_on_busy_stream() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let stream_key = format!("hot:task:wakeup:busy:{}", Uuid::now_v7());
        let stream_key_bg = stream_key.clone();
        let accept_count = Arc::new(AtomicUsize::new(0));
        let accept_count_bg = Arc::clone(&accept_count);
        let open_connections = Arc::new(AtomicUsize::new(0));
        let open_connections_bg = Arc::clone(&open_connections);
        let target_override: Arc<parking_lot::Mutex<Option<Uuid>>> =
            Arc::new(parking_lot::Mutex::new(None));
        let target_override_bg = Arc::clone(&target_override);

        tokio::spawn(async move {
            let seq = Arc::new(AtomicUsize::new(1));
            while let Ok((mut socket, _)) = listener.accept().await {
                accept_count_bg.fetch_add(1, Ordering::SeqCst);
                open_connections_bg.fetch_add(1, Ordering::SeqCst);
                let open_connections = Arc::clone(&open_connections_bg);
                let seq = Arc::clone(&seq);
                let stream_key = stream_key_bg.clone();
                let target_override = Arc::clone(&target_override_bg);
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 4096];
                    'conn: loop {
                        while let Some(cmd) = try_take_resp_command(&mut buf) {
                            let reply: Vec<u8> = match cmd.as_str() {
                                "XREVRANGE" => b"*0\r\n".to_vec(),
                                "XREAD" => {
                                    // Continuously stream non-empty remote task
                                    // completions so `entries` is never empty.
                                    tokio::time::sleep(Duration::from_millis(15)).await;
                                    let task_id =
                                        target_override.lock().take().unwrap_or_else(Uuid::now_v7);
                                    let id = format!(
                                        "1700000000000-{}",
                                        seq.fetch_add(1, Ordering::SeqCst)
                                    );
                                    encode_xread_single_wakeup(&stream_key, &id, task_id)
                                }
                                _ => b"+OK\r\n".to_vec(),
                            };
                            if socket.write_all(&reply).await.is_err() {
                                break 'conn;
                            }
                        }
                        match socket.read(&mut chunk).await {
                            Ok(0) | Err(_) => break 'conn,
                            Ok(n) => buf.extend_from_slice(&chunk[..n]),
                        }
                    }
                    open_connections.fetch_sub(1, Ordering::SeqCst);
                });
            }
        });

        let client = redis::Client::open(format!("redis://{address}/")).unwrap();
        let pubsub = RedisStreamsPubSub::with_wakeup_test_config(
            client,
            stream_key,
            Duration::from_millis(50),
            Duration::from_millis(120),
        );

        let guard = pubsub
            .watch_task_completion(Uuid::now_v7(), Duration::from_secs(1))
            .await;
        assert!(pubsub.is_task_wakeup_reader_running());
        assert_eq!(accept_count.load(Ordering::SeqCst), 1);
        assert_eq!(open_connections.load(Ordering::SeqCst), 1);

        // Drop the only local waiter while remote traffic keeps arriving.
        drop(guard);
        assert_eq!(pubsub.task_wakeup_waiter_count(), 0);

        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            if !pubsub.is_task_wakeup_reader_running()
                && open_connections.load(Ordering::SeqCst) == 0
            {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "idle wakeup reader failed to park and close its connection on a busy stream \
                 (running={}, open_conns={})",
                pubsub.is_task_wakeup_reader_running(),
                open_connections.load(Ordering::SeqCst)
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        // A new waiter must transparently restart the reader on a fresh
        // connection and receive its wakeup.
        let next_task = Uuid::now_v7();
        let next_guard = pubsub
            .watch_task_completion(next_task, Duration::from_secs(1))
            .await;
        assert_eq!(accept_count.load(Ordering::SeqCst), 2);
        *target_override.lock() = Some(next_task);
        tokio::time::timeout(Duration::from_secs(1), next_guard.notified())
            .await
            .expect("restarted wakeup reader must deliver wakeup to new waiter");
    }

    #[tokio::test]
    async fn redis_task_wakeup_resumes_after_connection_killed_on_real_redis() {
        let Some(client) = try_client().await else {
            eprintln!("skipping: Redis not available");
            return;
        };
        let stream_key = format!("hot:task:wakeup:kill:{}", Uuid::now_v7());
        let pod_a = RedisStreamsPubSub::with_wakeup_stream_key(client.clone(), stream_key.clone());
        let pod_b = RedisStreamsPubSub::with_wakeup_stream_key(client.clone(), stream_key.clone());

        let env_id = Uuid::now_v7();
        let task_1 = Uuid::now_v7();
        let guard_1 = pod_a
            .watch_task_completion(task_1, Duration::from_secs(1))
            .await;

        pod_b.notify_task_wakeup(task_1).await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), guard_1.notified())
            .await
            .expect("first wakeup must arrive before connection kill");
        drop(guard_1);

        // Register a second waiter, then kill all blocked XREAD clients on
        // Redis (simulating an LB idle drop or failover).
        let task_2 = Uuid::now_v7();
        let guard_2 = pod_a
            .watch_task_completion(task_2, Duration::from_secs(1))
            .await;

        let mut admin = client.get_multiplexed_async_connection().await.unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        let mut killed_any = false;
        while tokio::time::Instant::now() < deadline {
            let list: String = redis::cmd("CLIENT")
                .arg("LIST")
                .query_async(&mut admin)
                .await
                .unwrap();
            for line in list.lines() {
                if line.contains("cmd=xread")
                    && let Some(id_token) = line.split_whitespace().find(|t| t.starts_with("id="))
                {
                    let id: i64 = id_token.trim_start_matches("id=").parse().unwrap();
                    let killed: i64 = redis::cmd("CLIENT")
                        .arg("KILL")
                        .arg("ID")
                        .arg(id)
                        .query_async(&mut admin)
                        .await
                        .unwrap();
                    if killed > 0 {
                        killed_any = true;
                    }
                }
            }
            if killed_any {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            killed_any,
            "expected to find and kill the blocked XREAD reader"
        );

        // Publish completion while the reader is reconnecting; it must recover
        // and deliver the wakeup well within the reconnect window.
        pod_b
            .publish_env(EnvEvent::TaskComplete {
                task_id: task_2,
                env_id,
                stream_id: Uuid::now_v7(),
                function_name: "::app/work".to_string(),
                status: "completed".to_string(),
                duration_ms: Some(7),
                error: None,
            })
            .await
            .unwrap();

        tokio::time::timeout(Duration::from_secs(3), guard_2.notified())
            .await
            .expect("reader must reconnect after connection kill and wake task_2");

        drop(guard_2);
        delete_key(&client, &stream_key).await;
        delete_key(&client, &env_channel_name(&env_id)).await;
    }

    #[tokio::test]
    async fn task_inbox_multiplexes_concurrent_tasks_over_one_reader_and_parks_when_idle() {
        let Some(client) = try_client().await else {
            eprintln!("skipping: Redis not available");
            return;
        };
        let wakeup_key = format!("hot:task:inbox:wakeup:test:{}", Uuid::now_v7());
        let pod_a = RedisStreamsPubSub::with_inbox_test_config(
            client.clone(),
            wakeup_key.clone(),
            Duration::from_millis(50),
            Duration::from_millis(120),
            Duration::from_secs(30),
            Duration::from_secs(30),
        );
        let pod_b = RedisStreamsPubSub::with_inbox_test_config(
            client.clone(),
            wakeup_key.clone(),
            Duration::from_millis(50),
            Duration::from_millis(120),
            Duration::from_secs(30),
            Duration::from_secs(30),
        );

        let env_id = Uuid::now_v7();
        let tasks: Vec<Uuid> = (0..5).map(|_| Uuid::now_v7()).collect();

        // Publish an early message on tasks[0] before subscribing.
        pod_b
            .publish(StreamEvent::TaskMessage {
                task_id: tasks[0].to_string(),
                env_id: Some(env_id),
                payload: serde_json::json!("early-0"),
            })
            .await
            .unwrap();

        let mut subs = Vec::new();
        for &task_id in &tasks {
            subs.push(pod_a.subscribe_task(task_id).await.unwrap());
        }
        assert!(pod_a.is_task_inbox_reader_running());
        assert_eq!(pod_a.task_inbox_waiter_count(), tasks.len());
        assert_eq!(
            pod_a.task_inbox_reader_generation(),
            1,
            "all 5 concurrent task inboxes must share one reader generation"
        );

        // Early message is replayed on tasks[0] without duplicates.
        let early = tokio::time::timeout(Duration::from_secs(2), subs[0].next())
            .await
            .expect("early message should replay immediately");
        assert!(
            matches!(&early, StreamNext::Event(StreamEvent::TaskMessage { payload, .. }) if payload.as_str() == Some("early-0")),
            "{early:?}"
        );

        // Publish live messages from remote pod_b to every task.
        for (idx, &task_id) in tasks.iter().enumerate() {
            pod_b
                .publish(StreamEvent::TaskMessage {
                    task_id: task_id.to_string(),
                    env_id: Some(env_id),
                    payload: serde_json::json!(format!("live-{idx}")),
                })
                .await
                .unwrap();
        }
        for (idx, sub) in subs.iter_mut().enumerate() {
            let expected = format!("live-{idx}");
            let item = tokio::time::timeout(Duration::from_secs(2), sub.next())
                .await
                .expect("live task message should arrive via shared wakeup reader");
            assert!(
                matches!(&item, StreamNext::Event(StreamEvent::TaskMessage { payload, .. }) if payload.as_str() == Some(expected.as_str())),
                "task {idx}: {item:?}"
            );
        }

        // Dropping all subscribers lets the shared inbox reader park and close
        // its Redis connection after `idle_stop_after`.
        drop(subs);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while pod_a.is_task_inbox_reader_running() && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            !pod_a.is_task_inbox_reader_running(),
            "shared task inbox reader must park once all subscribers drop"
        );

        delete_key(&client, &wakeup_key).await;
        for task_id in tasks {
            delete_key(&client, &legacy_channel_name(&task_id)).await;
        }
    }

    #[tokio::test]
    async fn task_inbox_fallback_delivers_messages_without_wakeup_hint_across_select_cancels() {
        let Some(client) = try_client().await else {
            eprintln!("skipping: Redis not available");
            return;
        };
        let wakeup_key = format!("hot:task:inbox:wakeup:fallback:{}", Uuid::now_v7());
        let pubsub = RedisStreamsPubSub::with_inbox_test_config(
            client.clone(),
            wakeup_key.clone(),
            Duration::from_millis(50),
            Duration::from_millis(120),
            Duration::from_millis(120),
            Duration::from_secs(30),
        );

        let env_id = Uuid::now_v7();
        let task_id = Uuid::now_v7();
        let mut sub = pubsub.subscribe_task(task_id).await.unwrap();

        // Simulate an older producer during a rolling upgrade: append directly
        // to `stream:{task_id}` without writing to `hot:task:inbox:wakeup`.
        let legacy_event = StreamEvent::TaskMessage {
            task_id: task_id.to_string(),
            env_id: Some(env_id),
            payload: serde_json::json!("from-old-producer"),
        };
        let payload = serde_json::to_string(&legacy_event).unwrap();
        let stream_key = legacy_channel_name(&task_id);
        let mut conn = client.get_multiplexed_async_connection().await.unwrap();
        let _: redis::Value = redis::cmd("XADD")
            .arg(&stream_key)
            .arg("*")
            .arg("event")
            .arg(&payload)
            .query_async(&mut conn)
            .await
            .unwrap();

        // Repeatedly cancel `sub.next()` with a faster 30ms timer (mirroring
        // `inbox_forwarder`'s periodic DB poll) and verify the persistent
        // fallback timer still fires and delivers the message cleanly.
        let mut cancel_tick = tokio::time::interval(Duration::from_millis(30));
        cancel_tick.tick().await;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        let received = loop {
            assert!(
                tokio::time::Instant::now() < deadline,
                "fallback poll must not be starved by select! cancellations"
            );
            tokio::select! {
                biased;
                next = sub.next() => {
                    if let StreamNext::Event(event) = next {
                        break event;
                    }
                }
                _ = cancel_tick.tick() => {}
            }
        };
        assert!(
            matches!(&received, StreamEvent::TaskMessage { payload, .. } if payload == "from-old-producer"),
            "{received:?}"
        );

        drop(sub);
        delete_key(&client, &wakeup_key).await;
        delete_key(&client, &stream_key).await;
    }
}

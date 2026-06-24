//! Cross-worker task processing lease.
//!
//! Provides cross-pod mutual exclusion on `task_id` processing using a
//! Redis `SET NX PX` token-keyed lock with a background heartbeat that
//! refreshes the TTL while the task runs. A token-matched release Lua
//! script guarantees we only ever delete our own lease, never a sibling's.
//!
//! ## Why this exists
//!
//! After the `RedisStreamQueue::refill_lock` + `in_flight` fix
//! (`crates/hot/src/queue/streams.rs`), a single worker process never
//! re-buffers an in-flight PEL entry. But there is a separate cross-pod
//! race that fix doesn't touch:
//!
//! 1. Worker pod **A** is mid-processing task `T` (slow container, takes 90s).
//! 2. Worker pod **B**'s janitor runs `XAUTOCLAIM` after
//!    `queue.task-orphan-idle-ms` of PEL idle on `T`'s stream entry.
//! 3. PEL ownership of `T`'s stream entry transfers to **B**'s consumer.
//! 4. **B** reads its PEL via `XREADGROUP ... 0`, gets `T`, processes it.
//! 5. Now **A** and **B** are both running the same task concurrently —
//!    both will write results to the DB, both will publish completion
//!    events, and any per-task external side effect happens twice.
//!
//! The in-process `TaskShutdownCoordinator::try_register_task` dedup
//! catches duplicate dispatches *within* one worker, but cannot see across
//! pod boundaries. This lease fills that gap.
//!
//! ## Design
//!
//! - One Redis key per task: `hot:task:lease:<task_id>` (hash-tagged
//!   `{hot:task}:lease:<task_id>` in cluster mode so it co-locates with
//!   the `{hot:task}` stream slot — required for any future Lua script
//!   that wants to touch both).
//! - Value is a worker-token `<worker_id>:<random-uuid>` so we can
//!   distinguish "I own this" from "someone else owns this" — required
//!   for safe release after a sibling's lease expired and they
//!   re-acquired.
//! - `SET NX PX <ttl_ms>` is the atomic acquire. `NX` = only if absent,
//!   `PX` = millisecond TTL.
//! - Heartbeat task refreshes the TTL via a Lua script that only
//!   `PEXPIRE`s if the value still matches our token. We can never
//!   accidentally extend a sibling's lease after our own expired.
//! - `Drop` aborts the heartbeat and fires a best-effort token-matched
//!   release Lua via the runtime handle. Drop is sync, so failures (e.g.
//!   no current runtime in tests) just leave the key to TTL out — never
//!   incorrect, just slightly slower.
//!
//! ## Failure modes covered
//!
//! - **We crash mid-task**: heartbeat stops, lease expires after `ttl`,
//!   another worker can legitimately acquire. Bounded by `ttl` (default
//!   2 min) rather than the more permissive `ORPHAN_IDLE_MS`.
//! - **Redis briefly unavailable during heartbeat**: we log and try again
//!   on the next tick. As long as Redis returns before `ttl` elapses we
//!   keep the lease. The heartbeat interval is sized so we miss 3+ ticks
//!   before TTL.
//! - **Race release after sibling re-acquired**: the Lua script's `GET ==
//!   token` check refuses to touch a key we don't own. Bounded leak (the
//!   sibling keeps owning), never an incorrect release.
//! - **Local-dev / memory queue**: see [`NoopTaskLease`] — leases are a
//!   no-op when no Redis is configured (in-process queues already
//!   guarantee single-delivery and the `try_register_task` dedup is
//!   defense-in-depth on top of that).

use redis::cluster::ClusterClient;
use redis::{Client, Cmd, Script};
use std::error::Error;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::sync::{Mutex, Notify};
use tokio::task::JoinHandle;
use uuid::Uuid;

/// Default lease TTL. `hot_task_worker` validates that
/// `queue.task-orphan-idle-ms` is at least this long in Redis mode; otherwise
/// a crashed worker's queue entry could be reclaimed before its task lease has
/// expired. Refreshed by the heartbeat well before this fires.
pub const DEFAULT_LEASE_TTL: Duration = Duration::from_secs(120);

/// How often the heartbeat refreshes the lease TTL.
///
/// Sized so we can miss 3+ refreshes before TTL expires, surviving brief
/// Redis blips. With `DEFAULT_LEASE_TTL = 120s` and this at 30s, a single
/// missed tick still leaves ~90s of TTL.
pub const DEFAULT_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);

/// Token-matched extend Lua: only PEXPIRE if the key still holds our token.
///
/// Returns `1` on success, `0` if the key is missing or held by someone
/// else. We treat `0` as "lost the lease" and stop heartbeating.
const EXTEND_SCRIPT: &str = r#"
if redis.call('GET', KEYS[1]) == ARGV[1] then
    return redis.call('PEXPIRE', KEYS[1], ARGV[2])
else
    return 0
end
"#;

/// Token-matched release: DEL only if value matches.
///
/// Returns `1` if we owned and deleted, `0` otherwise. `0` is fine — it
/// just means our lease already expired (or was reclaimed) and someone
/// else legitimately owns the key now.
const RELEASE_SCRIPT: &str = r#"
if redis.call('GET', KEYS[1]) == ARGV[1] then
    return redis.call('DEL', KEYS[1])
else
    return 0
end
"#;

/// Errors surfaced by the lease layer. Wrapped as `Box<dyn Error>` at the
/// public API boundary to stay consistent with the rest of the worker.
#[derive(Debug)]
pub enum LeaseError {
    Redis(String),
    Build(String),
}

impl fmt::Display for LeaseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Redis(e) => write!(f, "Redis error: {}", e),
            Self::Build(e) => write!(f, "Lease config error: {}", e),
        }
    }
}

impl Error for LeaseError {}

impl From<redis::RedisError> for LeaseError {
    fn from(e: redis::RedisError) -> Self {
        Self::Redis(e.to_string())
    }
}

/// Backend-erased lease provider.
///
/// `Arc<dyn TaskLease>` is what `process_task` sees. Production wires up
/// [`RedisTaskLease`]; in-memory queue mode and tests get
/// [`NoopTaskLease`].
#[async_trait::async_trait]
pub trait TaskLease: Send + Sync + std::fmt::Debug {
    /// Try to acquire the lease for `task_id`.
    ///
    /// - `Ok(Some(guard))` — lease is held; drop the guard to release.
    /// - `Ok(None)` — another worker holds the lease. Caller should ACK
    ///   the queue entry (we're not the rightful processor) and move on.
    /// - `Err(_)` — transport / Redis error. Caller should treat as
    ///   "couldn't decide" — the safer choice depends on context but
    ///   usually means letting the queue retry later.
    async fn try_acquire(
        &self,
        task_id: Uuid,
        ttl: Duration,
    ) -> Result<Option<TaskLeaseGuard>, LeaseError>;
}

/// RAII handle for an active lease. The heartbeat task is aborted and a
/// best-effort token-matched release fires when this guard is dropped.
///
/// ```ignore
/// let Some(_guard) = lease.try_acquire(task_id, DEFAULT_LEASE_TTL).await? else {
///     // Sibling owns this task — skip and ACK the queue entry.
///     return Ok(());
/// };
/// // ... long-running work ...
/// // _guard's Drop fires release on scope exit / unwind / early return.
/// ```
#[must_use = "TaskLeaseGuard releases the lease on drop — bind it for the duration of task processing"]
pub struct TaskLeaseGuard {
    /// `Option` so `Drop` can `take()` and move the inner state into the
    /// best-effort release future without `&mut self` aliasing tricks.
    inner: Option<TaskLeaseGuardInner>,
}

impl TaskLeaseGuard {
    /// Construct a no-op guard (used by `NoopTaskLease` and tests).
    pub fn noop() -> Self {
        Self { inner: None }
    }

    /// Notification fired if the heartbeat observes that this worker no
    /// longer owns the Redis lease. No-op leases never lose ownership.
    pub fn lost_notify(&self) -> Option<Arc<Notify>> {
        self.inner
            .as_ref()
            .map(|inner| Arc::clone(&inner.lost_notify))
    }

    pub fn is_lost(&self) -> bool {
        self.inner
            .as_ref()
            .map(|inner| inner.lost.load(Ordering::Acquire))
            .unwrap_or(false)
    }

    /// Test helper: did we end up with a real (Redis-backed) lease?
    #[cfg(test)]
    pub(crate) fn is_real(&self) -> bool {
        self.inner.is_some()
    }
}

struct TaskLeaseGuardInner {
    task_id: Uuid,
    token: String,
    backend: Arc<RedisLeaseInner>,
    heartbeat: JoinHandle<()>,
    lost: Arc<AtomicBool>,
    lost_notify: Arc<Notify>,
}

impl Drop for TaskLeaseGuard {
    fn drop(&mut self) {
        let Some(inner) = self.inner.take() else {
            return;
        };
        inner.heartbeat.abort();

        // Best-effort token-matched release. `Drop` is sync so we detach
        // onto the runtime. If we're being dropped *outside* a tokio
        // runtime (sync tests, panic during shutdown of the runtime
        // itself, etc.) `try_current` returns `Err` and we silently drop
        // the cleanup — the Redis TTL will eventually GC the key, which
        // is the same behavior as a worker crash.
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let backend = inner.backend.clone();
            let task_id = inner.task_id;
            let token = inner.token;
            handle.spawn(async move {
                match backend.release(&task_id, &token).await {
                    Ok(true) => {
                        tracing::debug!(
                            task_id = %task_id,
                            "Task lease released cleanly on drop"
                        );
                    }
                    Ok(false) => {
                        tracing::debug!(
                            task_id = %task_id,
                            "Task lease drop: key already gone or owned by sibling (TTL'd or reclaimed)"
                        );
                    }
                    Err(e) => {
                        tracing::warn!(
                            task_id = %task_id,
                            error = %e,
                            "Task lease release failed on drop (will TTL out)"
                        );
                    }
                }
            });
        }
    }
}

// ============================================================================
// Noop backend (memory queue / tests)
// ============================================================================

/// No-op lease used when the queue backend is in-process (memory) or when
/// leases are explicitly disabled. Always succeeds with a [`TaskLeaseGuard`]
/// that does nothing on drop.
///
/// Memory-mode workers don't need cross-worker mutual exclusion because
/// `MemQueue`'s atomic `async_channel` semantics already guarantee
/// single-delivery within the process, and there is no other process to
/// race with.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopTaskLease;

#[async_trait::async_trait]
impl TaskLease for NoopTaskLease {
    async fn try_acquire(
        &self,
        _task_id: Uuid,
        _ttl: Duration,
    ) -> Result<Option<TaskLeaseGuard>, LeaseError> {
        Ok(Some(TaskLeaseGuard::noop()))
    }
}

// ============================================================================
// Redis backend
// ============================================================================

/// Internal client wrapper that lets us issue commands against either a
/// standalone or cluster Redis without leaking the choice into the public
/// API. Mirrors the pattern in `crates/hot/src/queue/streams.rs`.
#[derive(Clone)]
enum RedisLeaseClient {
    Standalone(Client),
    Cluster(ClusterClient),
}

impl fmt::Debug for RedisLeaseClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Standalone(_) => write!(f, "RedisLeaseClient::Standalone"),
            Self::Cluster(_) => write!(f, "RedisLeaseClient::Cluster"),
        }
    }
}

/// Cached connection per backend. Lazily created on first command, then
/// reused for the life of the lease provider.
enum RedisLeaseConn {
    Standalone(redis::aio::MultiplexedConnection),
    Cluster(redis::cluster_async::ClusterConnection),
}

impl RedisLeaseConn {
    async fn run_cmd(&mut self, cmd: &Cmd) -> Result<redis::Value, LeaseError> {
        match self {
            Self::Standalone(c) => Ok(cmd.query_async(c).await?),
            Self::Cluster(c) => Ok(cmd.query_async(c).await?),
        }
    }

    async fn run_script(
        &mut self,
        script: &Script,
        keys: &[String],
        args: &[Vec<u8>],
    ) -> Result<redis::Value, LeaseError> {
        let mut invocation = script.prepare_invoke();
        for k in keys {
            invocation.key(k);
        }
        for a in args {
            invocation.arg(a.as_slice());
        }
        match self {
            Self::Standalone(c) => Ok(invocation.invoke_async(c).await?),
            Self::Cluster(c) => Ok(invocation.invoke_async(c).await?),
        }
    }
}

/// Reference-counted inner that holds the connection, scripts, and
/// per-instance config. The public [`RedisTaskLease`] is a thin wrapper
/// around `Arc<RedisLeaseInner>` so we can share it between the
/// `TaskLease` impl, the heartbeat task, and the guard's `Drop` release.
struct RedisLeaseInner {
    // Note: manual Debug impl below — `Mutex<Option<RedisLeaseConn>>` and
    // `Script` don't derive `Debug`, and we only need to surface the
    // configuration fields anyway.
    client: RedisLeaseClient,
    /// Worker-id prefix included in every token so `MONITOR`, debug logs,
    /// and `GET hot:task:lease:<id>` show which pod owns which lease.
    worker_id: String,
    /// Key prefix. `hot:task:lease:` for standalone, `{hot:task}:lease:`
    /// for cluster (so leases hash to the same slot as `{hot:task}`).
    key_prefix: String,
    /// Lazy shared connection. Recreated transparently on connect failure.
    conn: Mutex<Option<RedisLeaseConn>>,
    /// Heartbeat cadence. Exposed for tests to drive sub-second.
    heartbeat_interval: Duration,
    /// Compiled Lua scripts (cached per process — `Script` interns the
    /// SHA1 and routes through `EVALSHA` with `EVAL` fallback automatically).
    extend_script: Script,
    release_script: Script,
}

impl fmt::Debug for RedisLeaseInner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RedisLeaseInner")
            .field("client", &self.client)
            .field("worker_id", &self.worker_id)
            .field("key_prefix", &self.key_prefix)
            .field("heartbeat_interval", &self.heartbeat_interval)
            .finish()
    }
}

impl RedisLeaseInner {
    fn key_for(&self, task_id: &Uuid) -> String {
        format!("{}{}", self.key_prefix, task_id.simple())
    }

    fn make_token(&self) -> String {
        format!("{}:{}", self.worker_id, Uuid::new_v4())
    }

    /// Get-or-establish the cached connection.
    async fn ensure_connection(&self) -> Result<(), LeaseError> {
        let mut guard = self.conn.lock().await;
        if guard.is_some() {
            return Ok(());
        }
        let new = match &self.client {
            RedisLeaseClient::Standalone(c) => {
                // Disable redis-rs 1.x's 500ms default response timeout so a
                // lease `SET NX` / extend script isn't clipped under load and
                // spuriously deferred (which feeds the queue's retry budget).
                // See `hot::redis::standalone_async_config` for the rationale.
                let mc = c
                    .get_multiplexed_async_connection_with_config(
                        &hot::redis::standalone_async_config(),
                    )
                    .await?;
                RedisLeaseConn::Standalone(mc)
            }
            RedisLeaseClient::Cluster(c) => {
                let cc = c.get_async_connection().await?;
                RedisLeaseConn::Cluster(cc)
            }
        };
        *guard = Some(new);
        Ok(())
    }

    async fn run_cmd(&self, cmd: &Cmd) -> Result<redis::Value, LeaseError> {
        self.ensure_connection().await?;
        let mut guard = self.conn.lock().await;
        let conn = guard.as_mut().ok_or_else(|| {
            LeaseError::Redis("connection vanished after ensure_connection".to_string())
        })?;
        match conn.run_cmd(cmd).await {
            Ok(v) => Ok(v),
            Err(e) => {
                tracing::debug!(error = %e, "Lease Redis command failed; clearing cached connection");
                *guard = None;
                Err(e)
            }
        }
    }

    async fn run_script(
        &self,
        script: &Script,
        keys: &[String],
        args: &[Vec<u8>],
    ) -> Result<redis::Value, LeaseError> {
        self.ensure_connection().await?;
        let mut guard = self.conn.lock().await;
        let conn = guard.as_mut().ok_or_else(|| {
            LeaseError::Redis("connection vanished after ensure_connection".to_string())
        })?;
        match conn.run_script(script, keys, args).await {
            Ok(v) => Ok(v),
            Err(e) => {
                tracing::debug!(error = %e, "Lease Redis script failed; clearing cached connection");
                *guard = None;
                Err(e)
            }
        }
    }

    /// Token-matched release. `Ok(true)` if we owned and deleted,
    /// `Ok(false)` if the key was already gone or held by someone else.
    async fn release(&self, task_id: &Uuid, token: &str) -> Result<bool, LeaseError> {
        let key = self.key_for(task_id);
        let val = self
            .run_script(&self.release_script, &[key], &[token.as_bytes().to_vec()])
            .await?;
        Ok(matches!(val, redis::Value::Int(1)))
    }

    /// Token-matched extend. `Ok(true)` if we still owned and refreshed,
    /// `Ok(false)` if the key was missing or owned by a sibling.
    async fn extend(&self, task_id: &Uuid, token: &str, ttl: Duration) -> Result<bool, LeaseError> {
        let key = self.key_for(task_id);
        let ttl_ms = ttl.as_millis().to_string();
        let val = self
            .run_script(
                &self.extend_script,
                &[key],
                &[token.as_bytes().to_vec(), ttl_ms.into_bytes()],
            )
            .await?;
        Ok(matches!(val, redis::Value::Int(1)))
    }
}

/// Redis-backed [`TaskLease`] implementation.
///
/// Cheap to clone — wraps an `Arc` internally so all clones share the
/// connection, script cache, and heartbeat plumbing. Construct once per
/// worker process and clone freely (or hand out as `Arc<dyn TaskLease>`).
#[derive(Clone, Debug)]
pub struct RedisTaskLease {
    inner: Arc<RedisLeaseInner>,
}

impl RedisTaskLease {
    /// Build a standalone-Redis lease provider.
    pub fn standalone(client: Client, worker_id: String) -> Self {
        Self {
            inner: Arc::new(RedisLeaseInner {
                client: RedisLeaseClient::Standalone(client),
                worker_id,
                key_prefix: "hot:task:lease:".to_string(),
                conn: Mutex::new(None),
                heartbeat_interval: DEFAULT_HEARTBEAT_INTERVAL,
                extend_script: Script::new(EXTEND_SCRIPT),
                release_script: Script::new(RELEASE_SCRIPT),
            }),
        }
    }

    /// Build a cluster-Redis lease provider. The key prefix is
    /// hash-tagged so leases co-locate with the `{hot:task}` stream slot.
    pub fn cluster(client: ClusterClient, worker_id: String) -> Self {
        Self {
            inner: Arc::new(RedisLeaseInner {
                client: RedisLeaseClient::Cluster(client),
                worker_id,
                key_prefix: "{hot:task}:lease:".to_string(),
                conn: Mutex::new(None),
                heartbeat_interval: DEFAULT_HEARTBEAT_INTERVAL,
                extend_script: Script::new(EXTEND_SCRIPT),
                release_script: Script::new(RELEASE_SCRIPT),
            }),
        }
    }

    /// Build from a Redis URI, choosing standalone vs cluster the same
    /// way the queue layer does (`crates/hot/src/queue/mod.rs`). Centralizes
    /// the TLS init and cluster auto-detect so callers don't repeat them.
    pub fn from_uri(uri: &str, cluster_mode: bool, worker_id: String) -> Result<Self, LeaseError> {
        if uri.starts_with("rediss://") {
            hot::redis::init_crypto_provider();
        }
        let is_cluster = cluster_mode || hot::redis::is_cluster_uri(uri);
        if is_cluster {
            let c = ClusterClient::new(vec![uri])
                .map_err(|e| LeaseError::Build(format!("cluster client: {}", e)))?;
            Ok(Self::cluster(c, worker_id))
        } else {
            let c = Client::open(uri)
                .map_err(|e| LeaseError::Build(format!("standalone client: {}", e)))?;
            Ok(Self::standalone(c, worker_id))
        }
    }

    /// Override the heartbeat interval (test hook).
    #[cfg(test)]
    pub(crate) fn with_heartbeat_interval(mut self, interval: Duration) -> Self {
        let inner =
            Arc::get_mut(&mut self.inner).expect("with_heartbeat_interval before any clone/spawn");
        inner.heartbeat_interval = interval;
        self
    }

    /// Spawn the heartbeat task. Runs until aborted by the guard's `Drop`
    /// or until the lease is observed lost (extend returns `false`).
    fn spawn_heartbeat(
        inner: Arc<RedisLeaseInner>,
        task_id: Uuid,
        token: String,
        ttl: Duration,
        lost: Arc<AtomicBool>,
        lost_notify: Arc<Notify>,
    ) -> JoinHandle<()> {
        let interval = inner.heartbeat_interval;
        tokio::spawn(async move {
            // Stagger the first tick so a burst of new acquires doesn't
            // all hit Redis on the same wall-clock millisecond.
            let mut ticker =
                tokio::time::interval_at(tokio::time::Instant::now() + interval, interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                ticker.tick().await;
                match inner.extend(&task_id, &token, ttl).await {
                    Ok(true) => {
                        tracing::trace!(
                            task_id = %task_id,
                            ttl_secs = ttl.as_secs(),
                            "Task lease heartbeat refreshed"
                        );
                    }
                    Ok(false) => {
                        // We no longer own the key — either it TTL'd out
                        // (we missed too many heartbeats) or a sibling
                        // forcibly took it. Stop heartbeating so we don't
                        // accidentally hammer Redis trying to extend
                        // someone else's lease (the script's token check
                        // already protects against the bad write, but
                        // there's no point in repeated wasted RTTs).
                        tracing::warn!(
                            task_id = %task_id,
                            "Task lease lost during heartbeat — stopping refresh (task may now run concurrently on a sibling)"
                        );
                        lost.store(true, Ordering::Release);
                        lost_notify.notify_waiters();
                        return;
                    }
                    Err(e) => {
                        // Transient — keep ticking. As long as Redis
                        // recovers before `ttl` we keep the lease.
                        tracing::debug!(
                            task_id = %task_id,
                            error = %e,
                            "Task lease heartbeat failed (will retry next tick)"
                        );
                    }
                }
            }
        })
    }
}

#[async_trait::async_trait]
impl TaskLease for RedisTaskLease {
    async fn try_acquire(
        &self,
        task_id: Uuid,
        ttl: Duration,
    ) -> Result<Option<TaskLeaseGuard>, LeaseError> {
        let token = self.inner.make_token();
        let key = self.inner.key_for(&task_id);
        let ttl_ms = ttl.as_millis() as u64;

        // SET key token NX PX <ttl_ms>
        // - NX: atomic acquire-or-fail
        // - PX: millisecond TTL (must comfortably exceed ORPHAN_IDLE_MS)
        let mut cmd = redis::cmd("SET");
        cmd.arg(&key).arg(&token).arg("NX").arg("PX").arg(ttl_ms);

        let result = self.inner.run_cmd(&cmd).await?;
        let acquired = match &result {
            redis::Value::Okay => true,
            redis::Value::SimpleString(s) if s == "OK" => true,
            // Redis returns Nil when SET NX is rejected (key already exists).
            _ => false,
        };
        if !acquired {
            tracing::debug!(
                task_id = %task_id,
                worker_id = %self.inner.worker_id,
                "Task lease held by another worker — declining duplicate dispatch"
            );
            return Ok(None);
        }

        let backend = Arc::clone(&self.inner);
        let lost = Arc::new(AtomicBool::new(false));
        let lost_notify = Arc::new(Notify::new());
        let heartbeat = Self::spawn_heartbeat(
            Arc::clone(&self.inner),
            task_id,
            token.clone(),
            ttl,
            Arc::clone(&lost),
            Arc::clone(&lost_notify),
        );

        Ok(Some(TaskLeaseGuard {
            inner: Some(TaskLeaseGuardInner {
                task_id,
                token,
                backend,
                heartbeat,
                lost,
                lost_notify,
            }),
        }))
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// Skip cleanly when Redis isn't available (CI without service, sandbox).
    async fn try_redis() -> Option<RedisTaskLease> {
        let client = redis::Client::open("redis://127.0.0.1/").ok()?;
        client.get_multiplexed_async_connection().await.ok()?;
        Some(RedisTaskLease::standalone(
            client,
            "test-worker".to_string(),
        ))
    }

    async fn cleanup_key(lease: &RedisTaskLease, task_id: &Uuid) {
        let key = lease.inner.key_for(task_id);
        let _ = lease.inner.run_cmd(redis::cmd("DEL").arg(&key)).await;
    }

    #[tokio::test]
    async fn noop_lease_always_acquires_and_release_is_a_noop() {
        let l = NoopTaskLease;
        let g1 = l
            .try_acquire(Uuid::new_v4(), DEFAULT_LEASE_TTL)
            .await
            .unwrap();
        assert!(g1.is_some(), "noop lease must always acquire");
        // Two concurrent acquires for the same task should also succeed
        // — the noop lease provides no mutual exclusion.
        let id = Uuid::new_v4();
        let g2 = l.try_acquire(id, DEFAULT_LEASE_TTL).await.unwrap();
        let g3 = l.try_acquire(id, DEFAULT_LEASE_TTL).await.unwrap();
        assert!(g2.is_some() && g3.is_some());
        drop((g1, g2, g3));
    }

    #[tokio::test]
    async fn redis_lease_acquire_is_mutually_exclusive_per_task_id() {
        let Some(lease) = try_redis().await else {
            eprintln!("skipping: Redis not available");
            return;
        };
        let task_id = Uuid::new_v4();
        cleanup_key(&lease, &task_id).await;

        let g1 = lease.try_acquire(task_id, DEFAULT_LEASE_TTL).await.unwrap();
        assert!(g1.is_some(), "first acquire must succeed");
        assert!(g1.as_ref().unwrap().is_real());

        // A second concurrent acquire for the same task_id must be denied.
        let g2 = lease.try_acquire(task_id, DEFAULT_LEASE_TTL).await.unwrap();
        assert!(
            g2.is_none(),
            "second acquire must be denied while first is held"
        );

        // Different task_id is independent and must succeed.
        let other_id = Uuid::new_v4();
        cleanup_key(&lease, &other_id).await;
        let g3 = lease
            .try_acquire(other_id, DEFAULT_LEASE_TTL)
            .await
            .unwrap();
        assert!(g3.is_some(), "different task_id must acquire independently");

        cleanup_key(&lease, &task_id).await;
        cleanup_key(&lease, &other_id).await;
    }

    #[tokio::test]
    async fn redis_lease_drop_releases_so_sibling_can_re_acquire() {
        let Some(lease) = try_redis().await else {
            eprintln!("skipping: Redis not available");
            return;
        };
        let task_id = Uuid::new_v4();
        cleanup_key(&lease, &task_id).await;

        {
            let g = lease.try_acquire(task_id, DEFAULT_LEASE_TTL).await.unwrap();
            assert!(g.is_some());
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        // Drop fires release on a detached task. Wait for it to land
        // (the release is sub-millisecond on local Redis but we're on
        // a multi-task runtime, so give it a few ms of slack).
        for _ in 0..50 {
            tokio::time::sleep(Duration::from_millis(20)).await;
            let g = lease.try_acquire(task_id, DEFAULT_LEASE_TTL).await.unwrap();
            if g.is_some() {
                cleanup_key(&lease, &task_id).await;
                return;
            }
        }
        cleanup_key(&lease, &task_id).await;
        panic!("re-acquire after Drop never succeeded — release on Drop is broken");
    }

    #[tokio::test]
    async fn redis_lease_expires_when_holder_crashes_without_release() {
        let Some(client) = redis::Client::open("redis://127.0.0.1/").ok() else {
            eprintln!("skipping: Redis not available");
            return;
        };
        if client.get_multiplexed_async_connection().await.is_err() {
            eprintln!("skipping: Redis not available");
            return;
        }
        // Long heartbeat so it doesn't refresh during the test, simulating
        // a worker that crashed mid-task and lost its heartbeat thread.
        let lease = RedisTaskLease::standalone(client.clone(), "test-crash".to_string())
            .with_heartbeat_interval(Duration::from_secs(60));

        let task_id = Uuid::new_v4();
        cleanup_key(&lease, &task_id).await;

        // Acquire with a 500ms TTL.
        let g = lease
            .try_acquire(task_id, Duration::from_millis(500))
            .await
            .unwrap();
        assert!(g.is_some());

        // Forget the guard — simulates the worker process dying without
        // running Drop. (In a real crash the heartbeat task also dies; the
        // long heartbeat interval above has the same effect here.)
        std::mem::forget(g);

        let denied = lease
            .try_acquire(task_id, Duration::from_millis(500))
            .await
            .unwrap();
        assert!(denied.is_none(), "must be denied while TTL hasn't expired");

        // Wait past the TTL.
        tokio::time::sleep(Duration::from_millis(700)).await;

        let recovered = lease
            .try_acquire(task_id, Duration::from_millis(500))
            .await
            .unwrap();
        assert!(
            recovered.is_some(),
            "sibling must acquire after the original lease TTL'd out"
        );

        cleanup_key(&lease, &task_id).await;
    }

    #[tokio::test]
    async fn redis_lease_heartbeat_keeps_lease_alive_past_initial_ttl() {
        let Some(client) = redis::Client::open("redis://127.0.0.1/").ok() else {
            eprintln!("skipping: Redis not available");
            return;
        };
        if client.get_multiplexed_async_connection().await.is_err() {
            eprintln!("skipping: Redis not available");
            return;
        }
        // Sub-second heartbeat so the test runs fast.
        let lease = RedisTaskLease::standalone(client.clone(), "test-heartbeat".to_string())
            .with_heartbeat_interval(Duration::from_millis(150));

        let task_id = Uuid::new_v4();
        cleanup_key(&lease, &task_id).await;

        // 500ms TTL, 150ms heartbeat — heartbeat fires ~3x and keeps the
        // key alive even though the original TTL would have expired.
        let g = lease
            .try_acquire(task_id, Duration::from_millis(500))
            .await
            .unwrap();
        assert!(g.is_some());

        tokio::time::sleep(Duration::from_millis(900)).await;

        let denied = lease
            .try_acquire(task_id, Duration::from_millis(500))
            .await
            .unwrap();
        assert!(
            denied.is_none(),
            "heartbeat should have kept the lease alive past the initial TTL"
        );

        // Drop the original; the heartbeat aborts and release fires.
        drop(g);

        for _ in 0..50 {
            tokio::time::sleep(Duration::from_millis(20)).await;
            let g2 = lease
                .try_acquire(task_id, Duration::from_millis(500))
                .await
                .unwrap();
            if g2.is_some() {
                cleanup_key(&lease, &task_id).await;
                return;
            }
        }
        cleanup_key(&lease, &task_id).await;
        panic!("re-acquire after holder dropped never succeeded");
    }

    #[tokio::test]
    async fn redis_lease_guard_signals_when_heartbeat_loses_ownership() {
        let Some(client) = redis::Client::open("redis://127.0.0.1/").ok() else {
            eprintln!("skipping: Redis not available");
            return;
        };
        if client.get_multiplexed_async_connection().await.is_err() {
            eprintln!("skipping: Redis not available");
            return;
        }

        let lease = RedisTaskLease::standalone(client.clone(), "test-lost".to_string())
            .with_heartbeat_interval(Duration::from_millis(50));
        let task_id = Uuid::new_v4();
        cleanup_key(&lease, &task_id).await;

        let guard = lease
            .try_acquire(task_id, Duration::from_secs(5))
            .await
            .unwrap()
            .expect("first acquire must succeed");
        let lost_notify = guard
            .lost_notify()
            .expect("Redis guard must expose loss notify");

        cleanup_key(&lease, &task_id).await;
        tokio::time::timeout(Duration::from_secs(2), lost_notify.notified())
            .await
            .expect("heartbeat should signal loss after lease key disappears");

        assert!(guard.is_lost());
        cleanup_key(&lease, &task_id).await;
    }

    #[tokio::test]
    async fn redis_lease_release_does_not_delete_a_siblings_key() {
        let Some(lease) = try_redis().await else {
            eprintln!("skipping: Redis not available");
            return;
        };
        let task_id = Uuid::new_v4();
        cleanup_key(&lease, &task_id).await;

        // Manually plant a key with a foreign token.
        let foreign_token = format!("foreign-worker:{}", Uuid::new_v4());
        let key = lease.inner.key_for(&task_id);
        let _ = lease
            .inner
            .run_cmd(
                redis::cmd("SET")
                    .arg(&key)
                    .arg(&foreign_token)
                    .arg("PX")
                    .arg(60_000u64),
            )
            .await
            .unwrap();

        // Try to release with our (different) token via the inner API.
        let our_token = lease.inner.make_token();
        let released = lease.inner.release(&task_id, &our_token).await.unwrap();
        assert!(!released, "release with non-matching token must be a no-op");

        // The foreign key must still exist.
        let v = lease
            .inner
            .run_cmd(redis::cmd("GET").arg(&key))
            .await
            .unwrap();
        match v {
            redis::Value::BulkString(bytes) => {
                assert_eq!(
                    bytes,
                    foreign_token.as_bytes(),
                    "foreign key must still hold the foreign token"
                );
            }
            other => panic!("expected foreign key to still exist, got {:?}", other),
        }

        cleanup_key(&lease, &task_id).await;
    }
}

//! Hot Task Worker
//!
//! Consumes `TaskRequest` messages from the task queue and executes long-running
//! Hot functions. Each task runs in a `spawn_blocking` thread with its own VM,
//! inherits the originating Run's stream_id, and can receive inbound messages
//! via `::hot::task/receive()`.
//!
//! Container tasks support pluggable backends:
//! - **Docker** (default): Uses bollard, works everywhere Docker runs
//! - **Kata** (optional): MicroVM isolation via Kata Containers + QEMU/containerd, requires Linux + KVM

pub mod box_limits;
pub mod build_info;
#[cfg(all(target_os = "linux", feature = "kata"))]
mod cni;
mod data_volume;
mod executor;
pub mod file_server;
mod log_accumulator;
mod orphan_reaper;
pub mod resource_budget;
pub mod shutdown;
pub mod task_lease;

pub use executor::Backend;
use executor::ExecutorError;

use base64::Engine;
use base64::engine::general_purpose;
use hot::data::serialization::Serialization;
use hot::db::{self, Build, DatabasePool, Env, Project, Task, TaskStatus};
use hot::env::retry::RetryConfig;
use hot::lang::cache::bytecode_cache::{BytecodeCache, CachedBytecode};
use hot::lang::emitter::EngineEventEmitter;
use hot::lang::event::{EventPublisher, ExecutionContext};
use hot::lang::hot::task::TaskRequest;
use hot::queue::{
    ConsumerLifecycle, ProcessingQueue, Queue, QueueInfrastructureError, QueueLeaseTiming,
    QueueType,
};
use hot::stream::{
    EnvEvent, EnvPublisher, StreamEvent, StreamNext, StreamPubSub, StreamPublisher,
    StreamSubscriberFactory,
};
use hot::val::Val;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{Mutex, Semaphore, mpsc};
use tokio::task::JoinSet;
use uuid::Uuid;

type UsageStatsCache =
    Arc<Mutex<HashMap<Uuid, (std::time::Instant, hot::db::subscription::OrgUsageStats)>>>;

static USAGE_STATS_LOCKS: std::sync::OnceLock<Mutex<HashMap<Uuid, std::sync::Weak<Mutex<()>>>>> =
    std::sync::OnceLock::new();

async fn usage_stats_org_lock(org_id: Uuid) -> Arc<Mutex<()>> {
    let locks = USAGE_STATS_LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut locks = locks.lock().await;
    // The registry only coordinates currently active callers. Weak entries
    // avoid retaining one mutex forever for every org a long-lived worker has
    // ever observed.
    locks.retain(|_, lock| lock.strong_count() > 0);
    if let Some(lock) = locks.get(&org_id).and_then(std::sync::Weak::upgrade) {
        return lock;
    }
    let lock = Arc::new(Mutex::new(()));
    locks.insert(org_id, Arc::downgrade(&lock));
    lock
}

async fn fresh_cached_usage(
    cache: &UsageStatsCache,
    org_id: &Uuid,
) -> Option<hot::db::subscription::OrgUsageStats> {
    let cache = cache.lock().await;
    cache
        .get(org_id)
        .filter(|(ts, _)| ts.elapsed() < USAGE_STATS_CACHE_TTL)
        .map(|(_, stats)| stats.clone())
}

async fn cached_usage_stats(
    db: &DatabasePool,
    org_id: Uuid,
    period_start: chrono::DateTime<chrono::Utc>,
    retention_days: i32,
    cache: &UsageStatsCache,
) -> Option<hot::db::subscription::OrgUsageStats> {
    if let Some(stats) = fresh_cached_usage(cache, &org_id).await {
        return Some(stats);
    }

    let org_lock = usage_stats_org_lock(org_id).await;
    let _guard = org_lock.lock().await;
    if let Some(stats) = fresh_cached_usage(cache, &org_id).await {
        return Some(stats);
    }

    let stats = match tokio::time::timeout(
        DB_CALL_TIMEOUT,
        hot::db::subscription::OrgUsageStats::calculate(db, &org_id, period_start, retention_days),
    )
    .await
    {
        Ok(Ok(stats)) => stats,
        Ok(Err(e)) => {
            tracing::warn!(org_id = %org_id, "Failed to calculate org usage: {}", e);
            return None;
        }
        Err(_) => {
            tracing::warn!(
                org_id = %org_id,
                timeout_secs = DB_CALL_TIMEOUT.as_secs(),
                "Org usage calculation timed out"
            );
            return None;
        }
    };
    cache
        .lock()
        .await
        .insert(org_id, (std::time::Instant::now(), stats.clone()));
    Some(stats)
}

const USAGE_STATS_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(60);

/// Maximum time to spend on per-task post-execution cleanup (DB writes,
/// stream publishes, retry enqueue). When this fires we log + drop the rest
/// of the cleanup so a stuck DB pool can't pin the worker on a single task.
const POST_TASK_CLEANUP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// Per-DB-call timeout used inside cleanup helpers. A single hung query must
/// not be allowed to consume the entire `POST_TASK_CLEANUP_TIMEOUT` budget,
/// so we apply a tighter per-call ceiling.
const DB_CALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// Timing data is diagnostic and must never materially delay task execution
/// or completion. Keep this much tighter than ordinary state transitions.
const TASK_TIMING_DB_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// Terminal state is retried briefly before the queue lease is released. The
/// postcondition check in `process_task` still prevents ACK if every attempt
/// fails.
const TASK_COMPLETION_DB_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
const TASK_COMPLETION_ATTEMPTS: usize = 3;

/// Maximum time to spend tearing down a container (Docker remove or Kata
/// shim/VM kill). After this we log and move on; the executor itself or the
/// background reaper is responsible for finishing the cleanup.
const CONTAINER_KILL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
const CONTAINER_LOGS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Outer backstop for the multi-step Kata timeout cleanup.
/// `cleanup_after_timeout` sweeps every setup-retry container id and, for
/// each one, kills the task, deletes the task/container records, removes the
/// devmapper snapshot and IO FIFOs, and tears down CNI. Bounding that whole
/// sequence with the single-step `CONTAINER_KILL_TIMEOUT` can cancel it
/// mid-way — e.g. after DeleteContainer but before RemoveSnapshot/CNI
/// teardown — and a snapshot or netns that outlives its container record has
/// no discovery key left: startup recovery enumerates containerd Container
/// records (`list_orphan_containers`), so it can never rediscover the leak
/// even though snapshot/FIFO/netns names are deterministically derived from
/// the container id. Size the envelope for the sum of the per-id sweeps
/// (`MAX_SETUP_ATTEMPTS` container ids plus the final CNI teardown) rather
/// than a single step. Per-step bounding *inside* `cleanup_orphan` (so one
/// wedged containerd call cannot starve the remaining steps) belongs to the
/// executor and is flagged for the kata cross-check.
const KATA_TIMEOUT_CLEANUP_ENVELOPE: std::time::Duration = std::time::Duration::from_secs(120);

/// Worker-side heartbeat interval (the background heartbeat task ticks every
/// 15s and bumps `last_heartbeat_at` on every task this worker owns).
/// Co-located here so the reaper threshold can be expressed as a multiple of
/// it without two constants drifting apart.
const HEARTBEAT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(15);

/// A `running` task whose `last_heartbeat_at` is older than this is
/// considered a zombie (its owning worker is dead or hung). Set to
/// 2 × `HEARTBEAT_INTERVAL` so a worker that misses one tick is still
/// considered alive but a worker that misses two consecutive ticks is not.
const ZOMBIE_HEARTBEAT_STALE_SECS: i64 = 30;

/// How often the background reaper re-checks for zombie tasks. Running once
/// at startup is not enough: a previous worker's last heartbeat may have been
/// fresher than `ZOMBIE_HEARTBEAT_STALE_SECS` at startup time, in which case
/// the row is leaked forever without a periodic re-check.
const ZOMBIE_REAPER_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);

/// Bounded retry budget for the DB reads/writes that container adoption
/// depends on. Adoption runs at worker startup, often against a database
/// still riding out the same fault that killed the previous worker; a single
/// transient error must not leave a LIVE container unmanaged, because the
/// zombie reaper (startup pass + every `ZOMBIE_REAPER_INTERVAL`) would then
/// fail the still-running row and enqueue a duplicate run.
const ADOPTION_DB_ATTEMPTS: usize = 3;
const ADOPTION_DB_RETRY_BACKOFF: std::time::Duration = std::time::Duration::from_millis(150);

/// Maximum time a claimed code task will park waiting for a `code_semaphore`
/// permit before releasing its queue lease and deferring the message back to the
/// queue. The outer inflight cap is `max(code_max, container_max)`, so a burst of
/// code claims can exceed `code_max`; without this bound those extra claims would
/// hold their queue leases (and shared inflight slots) while parked, head-of-line
/// blocking container claims. Kept well under the task lease orphan-idle window so
/// we defer before any sibling can reclaim the message.
const CODE_SLOT_ACQUIRE_GRACE: std::time::Duration = std::time::Duration::from_secs(30);
const CONTAINER_SLOT_ACQUIRE_GRACE: std::time::Duration = std::time::Duration::from_secs(30);

/// Maximum time a claimed code task will wait for a blocking-execution slot
/// (the cap on TOTAL live VM threads, including detached ones whose task
/// already timed out) before deferring the message back to the queue.
/// Sequential with `CODE_SLOT_ACQUIRE_GRACE`, so the worst-case park is 60s —
/// still well under the 120s task-lease TTL / orphan-idle floor.
const BLOCKING_SLOT_ACQUIRE_GRACE: std::time::Duration = std::time::Duration::from_secs(30);

/// Upper bound on concurrently *live* VM executions, counting detached
/// `spawn_blocking` threads whose task already timed out. A timed-out code
/// task frees its `code_semaphore` permit immediately, so that cap alone
/// cannot stop admission while wedged VM threads accumulate toward the tokio
/// blocking-pool ceiling. Mirrors hot_worker's `worker.max-blocking-executions`
/// derivation: default 2× the code-task cap, floor = the cap itself (a
/// configured value below the cap would deadlock admission), clamped to the
/// largest legal semaphore size.
fn blocking_execution_capacity(code_max: usize, configured_max: i64) -> usize {
    let floor = code_max.max(1);
    let capacity = if configured_max < 0 {
        floor.saturating_mul(2)
    } else {
        usize::try_from(configured_max)
            .unwrap_or(usize::MAX)
            .max(floor)
    };
    capacity.min(Semaphore::MAX_PERMITS)
}

/// Kill and remove a container with a wall-clock ceiling. A wedged Kata shim
/// or hung Docker daemon can otherwise pin the worker indefinitely on
/// teardown, leaving orphan runtime processes to accumulate on the host.
async fn kill_and_remove_with_timeout(
    executor: &executor::BoxExecutor,
    container_id: &str,
    task_id: Option<&Uuid>,
) {
    if tokio::time::timeout(
        CONTAINER_KILL_TIMEOUT,
        executor.kill_and_remove(container_id),
    )
    .await
    .is_err()
    {
        tracing::error!(
            task_id = ?task_id,
            container_id = %container_id,
            backend = %executor.backend(),
            timeout_secs = CONTAINER_KILL_TIMEOUT.as_secs(),
            "kill_and_remove timed out — container may be leaked, will be cleaned up by orphan reaper"
        );
    }
}

async fn remove_container_with_timeout(
    executor: &executor::BoxExecutor,
    container_id: &str,
    task_id: &Uuid,
) {
    if tokio::time::timeout(
        CONTAINER_KILL_TIMEOUT,
        executor.remove_container(container_id),
    )
    .await
    .is_err()
    {
        tracing::error!(
            task_id = %task_id,
            container_id = %container_id,
            backend = %executor.backend(),
            timeout_secs = CONTAINER_KILL_TIMEOUT.as_secs(),
            "remove_container timed out — orphan reaper will retry cleanup"
        );
    }
}

/// Run best-effort cleanup behind a hard wall-clock ceiling. Cleanup talks to
/// the same runtime services that may have caused the execution timeout, so it
/// must never become an unbounded second wait after the primary deadline.
async fn bounded_cleanup<F>(deadline: std::time::Duration, cleanup: F) -> bool
where
    F: std::future::Future<Output = ()>,
{
    tokio::time::timeout(deadline, cleanup).await.is_ok()
}

async fn await_container_setup<F>(
    deadline: tokio::time::Instant,
    setup: F,
) -> Result<F::Output, tokio::time::error::Elapsed>
where
    F: std::future::Future,
{
    tokio::time::timeout_at(deadline, setup).await
}

/// Bounded data-volume cleanup. When `cleanup()` completes it defuses the
/// volume's `Drop` (no double-umount); when it times out the volume is handed
/// to a detached thread instead of being dropped here, because `Drop` re-runs
/// the same umount SYNCHRONOUSLY and a hung (D-state) umount would pin this
/// tokio worker thread forever — a handful of such tasks would stall the
/// whole worker.
async fn cleanup_data_volume_with_timeout(task_id: &Uuid, volume: data_volume::DataVolume) {
    if tokio::time::timeout(CONTAINER_KILL_TIMEOUT, volume.cleanup())
        .await
        .is_err()
    {
        tracing::error!(
            task_id = %task_id,
            mount_point = %volume.mount_point().display(),
            "data volume cleanup timed out — detaching the final unmount to a background thread; the mount and backing file may leak until it completes"
        );
        let _ = volume.drop_detached();
    }
}

/// Ensure a Linux hotbox binary is available for bind-mounting into Docker
/// containers. On non-Linux local development hosts, this cross-compiles
/// `hotbox` automatically when the binary is missing or stale.
#[cfg(not(target_os = "linux"))]
pub fn ensure_hotbox_binary() {
    let arch = match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "x86_64",
        other => other,
    };

    if hot::resources::get_hotbox_path(arch).is_ok() {
        return;
    }

    let binary_name = format!("hotbox-linux-{arch}");
    let target_bin = std::path::PathBuf::from("target").join(&binary_name);
    let needs_build = if target_bin.exists() {
        let bin_mtime = std::fs::metadata(&target_bin)
            .and_then(|m| m.modified())
            .ok();
        let src_dir = std::path::Path::new("crates/hotbox/src");
        if let (true, Some(bin_mtime)) = (src_dir.exists(), bin_mtime) {
            walkdir_newest_mtime(src_dir)
                .map(|src_mtime| src_mtime > bin_mtime)
                .unwrap_or(false)
        } else {
            false
        }
    } else {
        true
    };

    if !needs_build {
        return;
    }

    let script = std::path::Path::new("scripts/build-hotbox.sh");
    if !script.exists() {
        if !target_bin.exists() {
            tracing::warn!(
                "hot.dev: No hotbox Linux binary found for container tasks. \
                 Run `scripts/build-hotbox.sh` to cross-compile."
            );
        }
        return;
    }

    tracing::info!("hot.dev: Building hotbox for linux/{}...", arch);
    match std::process::Command::new("bash").arg(script).status() {
        Ok(status) if status.success() => {
            tracing::info!("hot.dev: hotbox cross-compile complete");
        }
        Ok(status) => {
            tracing::warn!(
                "hot.dev: hotbox build script exited with status {}. \
                 Container tasks may not have access to the hotbox CLI.",
                status
            );
        }
        Err(e) => {
            tracing::warn!(
                "hot.dev: Failed to run hotbox build script: {}. \
                 Container tasks may not have access to the hotbox CLI.",
                e
            );
        }
    }
}

/// Linux task-worker hosts already execute Linux binaries directly or receive
/// hotbox from deployed resources.
#[cfg(target_os = "linux")]
pub fn ensure_hotbox_binary() {}

#[cfg(not(target_os = "linux"))]
fn walkdir_newest_mtime(dir: &std::path::Path) -> Option<std::time::SystemTime> {
    let mut newest: Option<std::time::SystemTime> = None;
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            let mtime = if path.is_dir() {
                walkdir_newest_mtime(&path)
            } else {
                std::fs::metadata(&path).and_then(|m| m.modified()).ok()
            };
            if let Some(t) = mtime {
                newest = Some(newest.map_or(t, |n: std::time::SystemTime| n.max(t)));
            }
        }
    }
    newest
}

/// Configuration for the task worker.
#[derive(Debug, Clone)]
pub struct TaskWorkerConfig {
    pub queue_type: QueueType,
    pub redis_uri: Option<String>,
    pub redis_cluster: bool,
    pub serialization: Serialization,
    pub max_concurrent: usize,
    pub container_backend: Backend,
    pub containerd_socket: Option<String>,
    /// Kata VMM selection: "qemu" (default, works on EC2) or "firecracker" (bare metal only).
    pub kata_vmm: Option<String>,
    pub worker_conf: Val,
    /// Max concurrent code tasks (high-throughput, low-resource). Default: 500.
    pub code_max_concurrent: usize,
    /// Total memory budget for containers (MB). Default: 8192.
    pub worker_memory_mb: u64,
    /// Total disk budget for containers (MB). Default: 51200.
    pub worker_disk_mb: u64,
    /// Base directory for data volume loop mounts.
    pub data_volume_base_dir: Option<String>,
    /// Default per-container memory (MB) when BoxConf omits it. Default: 512.
    pub box_default_memory_mb: Option<u64>,
    /// Default per-container disk (MB) when BoxConf omits it. Default: 5120.
    pub box_default_disk_mb: Option<u64>,
    /// Default per-container tmp size (MB) when BoxConf omits it. Default: 500.
    pub box_default_tmp_mb: Option<u64>,
    /// Default per-container timeout (secs) when BoxConf omits it. Default: 60.
    pub box_default_timeout_secs: Option<u64>,
    /// Default per-container CPU quota when BoxConf omits it. Default: 50000.
    pub box_default_cpu_quota: Option<u64>,
    /// Task queue name. Defaults to "hot:task".
    pub queue_name: Option<String>,
}

fn validate_task_fairness_conf(conf: &Val) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let capacity_fairness = conf.get_str_or_default("task.capacity-fairness", "none");
    if capacity_fairness != "none" {
        return Err(format!(
            "Unsupported task.capacity-fairness '{}'; only 'none' is implemented",
            capacity_fairness
        )
        .into());
    }

    Ok(())
}

fn validate_task_orphan_idle_ms(
    queue_type: QueueType,
    task_orphan_idle_ms: u64,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if queue_type != QueueType::Redis {
        return Ok(());
    }

    let lease_ttl_ms = task_lease::DEFAULT_LEASE_TTL.as_millis() as u64;
    if task_orphan_idle_ms < lease_ttl_ms {
        return Err(format!(
            "queue.task-orphan-idle-ms={} is lower than the Redis task lease TTL ({}ms). Set HOT_QUEUE_TASK_ORPHAN_IDLE_MS to at least {} so a crashed worker's queue message is not reclaimed before its task lease can expire.",
            task_orphan_idle_ms, lease_ttl_ms, lease_ttl_ms
        )
        .into());
    }

    Ok(())
}

const CONTAINER_SCRIPT_PRELUDE: &str = "#!/bin/sh\nset -e\nmkdir -p /data\n";
const CONTAINER_SHELL_FLAGS: &str = "-ec";

/// Run the task worker.
pub async fn run(config: TaskWorkerConfig) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    validate_task_fairness_conf(&config.worker_conf)?;
    let queue_metrics_enabled = config
        .worker_conf
        .get_bool_or_default("queue.metrics-enabled", true);
    let queue_wait_target_p99_ms = config
        .worker_conf
        .get_int_or_default("queue.wait-target-p99-ms", 1_000)
        .max(1) as u64;
    hot::queue::set_metrics_enabled(queue_metrics_enabled);
    hot::queue::set_wait_target_p99_ms(queue_wait_target_p99_ms);
    tracing::info!(
        "hot_task_worker queue metrics configured (metrics_enabled={}, wait_target_p99_ms={})",
        queue_metrics_enabled,
        queue_wait_target_p99_ms,
    );

    let requested_code_max = config.code_max_concurrent.max(1);
    let code_budget =
        hot::runtime_budget::derive_task_code_concurrency(&config.worker_conf, requested_code_max);
    let code_max = code_budget.resolved;
    let worker_mem = config.worker_memory_mb.max(256);
    let worker_disk = config.worker_disk_mb.max(1024);

    let box_defaults = box_limits::BoxDefaults {
        memory_mb: config.box_default_memory_mb,
        disk_size_mb: config.box_default_disk_mb,
        tmp_size_mb: config.box_default_tmp_mb,
        timeout_secs: config.box_default_timeout_secs,
        cpu_quota: config.box_default_cpu_quota,
    };
    let box_defaults = Arc::new(box_defaults);
    let default_container_memory_mb = box_defaults
        .memory_mb
        .unwrap_or(box_limits::BoxLimits::DEFAULT_MEMORY_MB);
    let default_container_disk_mb = box_defaults
        .disk_size_mb
        .unwrap_or(box_limits::BoxLimits::DEFAULT_DISK_SIZE_MB);
    let default_container_tmp_mb = box_defaults
        .tmp_size_mb
        .unwrap_or(box_limits::BoxLimits::DEFAULT_TMP_SIZE_MB);
    let container_budget = hot::runtime_budget::derive_task_container_concurrency(
        &config.worker_conf,
        config.max_concurrent,
        worker_mem,
        worker_disk,
        default_container_memory_mb,
        default_container_disk_mb,
        default_container_tmp_mb,
        config.container_backend.to_string(),
    );
    let container_max = container_budget.resolved;
    let queue_claim_max = code_max.max(container_max);
    let shutdown_container_timeout_secs = config
        .worker_conf
        .get_int_or_default("task.shutdown-container-timeout-seconds", 30)
        .max(1) as u64;

    tracing::info!(
        "Starting hot_task_worker (code_max_concurrent={} requested={} cpu_limit={} memory_limit={:?} memory_limit_mb={:?}, container_max_concurrent={} requested={} explicit={} memory_limit={} disk_limit={} resource_budget={}MB mem / {}MB disk recovery_reserved_slots={} backend={}, shutdown_container_timeout={}s, box_defaults={}MB mem / {}MB disk)",
        code_max,
        code_budget.requested,
        code_budget.cpu_limit,
        code_budget.memory_limit,
        code_budget.memory_limit_mb,
        container_max,
        container_budget.requested,
        container_budget.explicit,
        container_budget.memory_limit,
        container_budget.disk_limit,
        container_budget.memory_budget_mb,
        container_budget.disk_budget_mb,
        container_budget.recovery_reserved_slots,
        container_budget.backend,
        shutdown_container_timeout_secs,
        default_container_memory_mb,
        default_container_disk_mb,
    );

    let queue_name = config
        .queue_name
        .clone()
        .unwrap_or_else(|| "hot:task".to_string());
    // Stable consumer-name prefix (host + pid) so XINFO CONSUMERS doesn't
    // grow unbounded across restarts of the same logical task worker. Each
    // processor adds a stable slot suffix.
    let host = std::env::var("HOSTNAME").unwrap_or_else(|_| "host".to_string());
    let pid = std::process::id();
    let processor_consumer_prefix = format!("{}-{}-task", host, pid);
    let admin_task_consumer_name = format!("{}-0", processor_consumer_prefix);
    // Tasks legitimately run for hours (and we cap at ~7 days) so 24h is the
    // window beyond which any backlog entry is almost certainly stale —
    // either the run was already cancelled, the user moved on, or the task
    // would now race with whatever new state replaced it. See
    // `RedisStreamQueue::with_startup_window` for full semantics.
    let task_startup_window = std::time::Duration::from_secs(24 * 60 * 60);
    let task_orphan_idle_ms = config
        .worker_conf
        .get_int_or_default("queue.task-orphan-idle-ms", 120_000)
        .max(1) as u64;
    validate_task_orphan_idle_ms(config.queue_type, task_orphan_idle_ms)?;

    let task_queue = ProcessingQueue::<TaskRequest>::new_with_cluster(
        config.queue_type,
        queue_name,
        config.redis_uri.clone(),
        config.redis_cluster,
        config.serialization,
    )?
    // Admin/recovery paths XAUTOCLAIM into processor 0's real consumer so
    // reclaimed PEL entries are drained by a live processor.
    .with_consumer_name(admin_task_consumer_name.clone())
    .with_read_batch_size(queue_claim_max)
    .with_orphan_idle_ms(task_orphan_idle_ms)
    .with_startup_window(task_startup_window);

    // Verify queue connectivity with a quick health check. Mirrors hot_worker's
    // pre-startup ping — fails fast on misconfigured Redis URI / TLS / cluster
    // settings instead of letting later operations time out one by one.
    match config.queue_type {
        QueueType::Memory => {
            tracing::debug!("Task worker using in-memory queue (no connectivity check needed)");
        }
        QueueType::Redis => match task_queue.is_empty().await {
            Ok(_) => {
                tracing::debug!("Task worker successfully connected to Redis queue");
            }
            Err(e) => {
                tracing::error!("Task worker failed to connect to Redis queue: {}", e);
                return Err(format!("Redis queue connectivity check failed: {}", e).into());
            }
        },
    }

    // Recover orphaned items from previous crashes. Mirrors hot_worker's
    // recovery path: 30s timeout to bound startup latency on slow Redis,
    // and shutdown_signal cancellation so Ctrl-C / SIGTERM during recovery
    // doesn't have to wait the full timeout.
    let recovery_timeout = std::time::Duration::from_secs(30);
    let recovery_result = tokio::select! {
        result = tokio::time::timeout(
            recovery_timeout,
            task_queue.recover_orphaned_items(),
        ) => result,
        _ = hot::signal::shutdown_signal() => {
            tracing::info!("Task worker received shutdown signal during orphaned item recovery");
            return Ok(());
        }
    };
    match recovery_result {
        Ok(Ok(count)) if count > 0 => {
            tracing::info!(
                "Task worker recovered {} orphaned item(s) — these will be reprocessed",
                count
            );
        }
        Ok(Ok(_)) => {
            tracing::debug!("Task worker no orphaned items found");
        }
        Ok(Err(e)) => {
            tracing::warn!(
                "Task worker failed to recover orphaned items: {} (continuing)",
                e
            );
        }
        Err(_) => {
            tracing::warn!(
                "Task worker orphaned item recovery timed out after {}s (continuing)",
                recovery_timeout.as_secs()
            );
        }
    }

    // In local dev, purge messages older than 1 hour before the rest of
    // startup. Old messages from previous local sessions cause a "catch-up
    // flood" that bogs down the worker — they're not useful in local dev.
    // This purges both pending PEL entries and undelivered stream entries.
    // Mirrors hot_worker's local-dev pre-startup purge.
    if hot::env::is_local_dev() {
        const LOCAL_DEV_MAX_AGE_MS: u64 = 60 * 60 * 1000; // 1h
        if let Err(e) = task_queue.purge_old_pending(LOCAL_DEV_MAX_AGE_MS).await {
            tracing::warn!(
                "Task worker failed to purge old pending messages: {} (continuing)",
                e
            );
        }
    }

    // Skip past any backlog older than the startup window before workers spawn.
    // Critical for task workers coming back from a long outage — without this,
    // they'd happily start draining multi-day-old tasks that nobody is waiting
    // on anymore. Best-effort: failures and timeouts are logged but don't block
    // startup. Wrapped in shutdown_signal cancellation for parity with
    // hot_worker.
    let ff_timeout = std::time::Duration::from_secs(10);
    let ff_result = tokio::select! {
        result = tokio::time::timeout(ff_timeout, task_queue.fast_forward_if_stale()) => result,
        _ = hot::signal::shutdown_signal() => {
            tracing::info!("Task worker received shutdown signal during fast-forward");
            return Ok(());
        }
    };
    match ff_result {
        Ok(Ok(skipped)) if skipped > 0 => {
            tracing::info!(
                "Task queue fast-forwarded past {} stale entr{} (window: {}s)",
                skipped,
                if skipped == 1 { "y" } else { "ies" },
                task_startup_window.as_secs()
            );
        }
        Ok(Ok(_)) => {
            tracing::debug!("Task queue consumer group within startup window (no fast-forward)");
        }
        Ok(Err(e)) => {
            tracing::warn!("Task queue fast-forward failed: {} (continuing)", e);
        }
        Err(_) => {
            tracing::warn!(
                "Task queue fast-forward timed out after {}s (continuing)",
                ff_timeout.as_secs()
            );
        }
    }

    // Purge stuck PEL entries older than the startup window. Complement to
    // fast-forward: fast-forward advances the *read cursor* past undelivered
    // backlog, while purge_old_pending ACKs *delivered-but-stuck* PEL entries
    // that no fast-forward can touch. This handles stale consumers that keep
    // delivered entries in PEL without making progress.
    let purge_timeout = std::time::Duration::from_secs(30);
    let purge_result = tokio::select! {
        result = tokio::time::timeout(
            purge_timeout,
            task_queue.purge_old_pending(task_startup_window.as_millis() as u64),
        ) => result,
        _ = hot::signal::shutdown_signal() => {
            tracing::info!("Task worker received shutdown signal during PEL purge");
            return Ok(());
        }
    };
    match purge_result {
        Ok(Ok(purged)) if purged > 0 => {
            tracing::info!(
                "Task queue purged {} stuck PEL entr{} (window: {}s)",
                purged,
                if purged == 1 { "y" } else { "ies" },
                task_startup_window.as_secs()
            );
        }
        Ok(Ok(_)) => {
            tracing::debug!("Task queue had no stuck PEL entries to purge");
        }
        Ok(Err(e)) => {
            tracing::warn!("Task queue purge_old_pending failed: {} (continuing)", e);
        }
        Err(_) => {
            tracing::warn!(
                "Task queue purge_old_pending timed out after {}s (continuing)",
                purge_timeout.as_secs()
            );
        }
    }

    // Clean up stale consumers and trim old stream entries. Mirrors
    // hot_worker's startup cleanup pass — without this on the task_worker
    // path, the {hot:task} stream's consumer list and entry retention only
    // get maintained when hot_worker is also alive on the same Redis.
    {
        use hot::queue::StreamCleanup;
        let cleanup_timeout = std::time::Duration::from_secs(30);
        let cleanup_result = tokio::select! {
            result = tokio::time::timeout(cleanup_timeout, task_queue.cleanup_streams()) => result,
            _ = hot::signal::shutdown_signal() => {
                tracing::info!("Task worker received shutdown signal during stream cleanup");
                return Ok(());
            }
        };
        match cleanup_result {
            Ok(Ok((consumers, trimmed))) => {
                if consumers > 0 || trimmed > 0 {
                    tracing::info!(
                        "Task queue stream cleanup: removed {} stale consumers, trimmed {} entries",
                        consumers,
                        trimmed
                    );
                }
            }
            Ok(Err(e)) => {
                tracing::warn!("Task queue stream cleanup failed: {} (continuing)", e);
            }
            Err(_) => {
                tracing::warn!(
                    "Task queue stream cleanup timed out after {}s (continuing)",
                    cleanup_timeout.as_secs()
                );
            }
        }
    }

    let db = db::create_db_pool(&config.worker_conf).await?;
    let db = Arc::new(db);

    // Initialize global alert queue so publish_alert() can enqueue delivery messages
    let alert_queue = ProcessingQueue::<hot::data::msg::Message>::new_with_cluster(
        config.queue_type,
        "hot:alert".to_string(),
        config.redis_uri.clone(),
        config.redis_cluster,
        config.serialization,
    )?;
    hot::notification_queue::init_alert_queue(Arc::new(alert_queue));

    let pubsub_type = match config.queue_type {
        QueueType::Memory => hot::stream::StreamPubSubType::Memory,
        QueueType::Redis => hot::stream::StreamPubSubType::Redis,
    };
    let stream_publisher = Arc::new(StreamPubSub::new(
        pubsub_type,
        config.redis_uri.clone(),
        config.redis_cluster,
    )?);

    let bytecode_cache = Arc::new(BytecodeCache::default_location());

    // Split concurrency: task-count caps plus memory/disk admission for containers.
    let code_semaphore = Arc::new(Semaphore::new(code_max));
    // Total live VM executions, including detached spawn_blocking threads
    // from timed-out tasks. Each execution's owned permit rides inside its
    // blocking closure, so a wedged thread keeps consuming capacity until it
    // actually exits instead of admission continuing at full rate while
    // leaked threads pile up to the tokio blocking-pool cap.
    let configured_max_blocking = config
        .worker_conf
        .get_int_or_default("worker.max-blocking-executions", -1);
    let max_blocking_executions = blocking_execution_capacity(code_max, configured_max_blocking);
    let blocking_execution_slots = Arc::new(Semaphore::new(max_blocking_executions));
    tracing::info!(
        max_blocking_executions,
        configured_max = configured_max_blocking,
        code_max,
        "Task worker blocking-execution ceiling configured"
    );
    let container_semaphore = Arc::new(Semaphore::new(container_max));
    let container_budget = resource_budget::ResourceBudget::new(
        container_budget.memory_budget_mb,
        container_budget.disk_budget_mb,
    );

    let container_executor = Arc::new(
        executor::BoxExecutor::new(
            config.container_backend,
            container_max,
            shutdown_container_timeout_secs,
            config.containerd_socket.as_deref(),
            config.kata_vmm.as_deref(),
        )
        .await?,
    );

    let data_vol_base = config
        .data_volume_base_dir
        .clone()
        .unwrap_or_else(|| "/tmp/hot-data-volumes".to_string());
    let data_vol_base = Arc::new(std::path::PathBuf::from(data_vol_base));

    let event_publisher: Option<Arc<dyn EventPublisher>> = create_event_publisher(&config, &db);

    let usage_stats_cache: UsageStatsCache = Arc::new(Mutex::new(HashMap::new()));

    // Unique worker identity for heartbeat ownership
    let worker_id = format!("tw-{}", Uuid::now_v7());
    tracing::debug!("Worker ID: {}", worker_id);

    // Cross-pod task lease provider. Backed by Redis `SET NX PX` with a
    // background heartbeat per active lease. Provides mutual exclusion on
    // `task_id` across multiple worker pods, closing the structural
    // cross-worker race where `XAUTOCLAIM` redelivers a long-running
    // task's PEL entry to a sibling while the original worker is still
    // processing it. See `task_lease.rs` module docs for the full
    // rationale and failure-mode analysis.
    //
    // Memory-mode workers get a no-op lease — `MemQueue`'s atomic
    // single-delivery semantics already guarantee no in-process
    // duplication, and there is no other process to race with.
    let task_lease: Arc<dyn task_lease::TaskLease> = match config.queue_type {
        QueueType::Memory => Arc::new(task_lease::NoopTaskLease),
        QueueType::Redis => {
            let uri = config
                .redis_uri
                .clone()
                .unwrap_or_else(|| "redis://127.0.0.1/".to_string());
            match task_lease::RedisTaskLease::from_uri(
                &uri,
                config.redis_cluster,
                worker_id.clone(),
            ) {
                Ok(l) => {
                    tracing::debug!(
                        worker_id = %worker_id,
                        ttl_secs = task_lease::DEFAULT_LEASE_TTL.as_secs(),
                        "Cross-pod task lease enabled (Redis-backed)"
                    );
                    Arc::new(l)
                }
                Err(e) => {
                    tracing::error!(
                        error = %e,
                        "Failed to construct Redis task lease; refusing to start because cross-pod task dedup would be disabled"
                    );
                    return Err(format!("Redis task lease initialization failed: {}", e).into());
                }
            }
        }
    };

    // Shutdown coordinator - 30s by default, followed by cancel + infra-retry
    // re-enqueue + DELCONSUMER. End-to-end fits comfortably within ECS 120s
    // stopTimeout. See `shutdown.rs` module docs for the full timeline.
    let shutdown_drain_secs = config
        .worker_conf
        .get_int_or_default(
            "task.shutdown-drain-seconds",
            shutdown::DEFAULT_CODE_DRAIN_SECS as i64,
        )
        .max(1) as u64;
    let coordinator = Arc::new(shutdown::TaskShutdownCoordinator::with_drain_secs(
        shutdown_drain_secs,
    ));

    // Clean up orphaned data volumes from a previous crash
    cleanup_stale_data_volumes(&data_vol_base).await;

    // Reap orphaned Kata shims/QEMU VMs from a previously killed worker so
    // they don't keep eating host memory and OOM us in turn. Safe even when
    // the backend isn't Kata: it scans /proc and only acts on processes
    // matching a small allowlist (containerd-shim-kata-v2, qemu-system-*).
    orphan_reaper::reap_orphan_kata_processes().await;

    let task_queue_arc = Arc::new(task_queue);

    // Adopt orphaned containers from a previous worker, or clean them up
    let adopted = adopt_orphaned_containers(
        &container_executor,
        &db,
        &stream_publisher,
        &task_queue_arc,
        &coordinator,
        &worker_id,
    )
    .await;

    // Reap zombie tasks (code tasks with stale heartbeat, or container tasks with no container).
    // Runs after adoption so coordinator-registered adopted tasks are skipped
    // even when their ownership repair is still pending.
    reap_zombie_tasks(&db, &stream_publisher, &task_queue_arc, &coordinator).await;

    let reconcile_after_secs = config
        .worker_conf
        .get_int_or_default("task.reconcile-queued-after-seconds", 60)
        .max(1) as u64;
    let reconcile_interval_secs = config
        .worker_conf
        .get_int_or_default("task.reconcile-interval-seconds", 30)
        .max(1) as u64;
    reconcile_queued_tasks(
        &db,
        &task_queue_arc,
        reconcile_after_secs,
        reconcile_interval_secs,
    )
    .await;

    {
        let reconciler_db = Arc::clone(&db);
        let reconciler_queue = Arc::clone(&task_queue_arc);
        let reconciler_coordinator = Arc::clone(&coordinator);
        tokio::spawn(async move {
            let mut interval =
                tokio::time::interval(std::time::Duration::from_secs(reconcile_interval_secs));
            interval.tick().await; // first tick is immediate; startup reconcile already ran
            loop {
                interval.tick().await;
                if reconciler_coordinator.is_shutting_down() {
                    break;
                }
                reconcile_queued_tasks(
                    &reconciler_db,
                    &reconciler_queue,
                    reconcile_after_secs,
                    reconcile_interval_secs,
                )
                .await;
            }
        });
    }

    // Background zombie reaper: re-runs the same query every
    // `ZOMBIE_REAPER_INTERVAL`. Without this, the only opportunity to fail
    // a stale `running` row is during worker startup; if a previous worker's
    // last heartbeat was fresher than `ZOMBIE_HEARTBEAT_STALE_SECS` at the
    // moment startup fired, the row remains stuck and the org's
    // box-concurrency quota does not recover.
    {
        let reaper_db = Arc::clone(&db);
        let reaper_pub = Arc::clone(&stream_publisher);
        let reaper_queue = Arc::clone(&task_queue_arc);
        let reaper_coordinator = Arc::clone(&coordinator);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(ZOMBIE_REAPER_INTERVAL);
            interval.tick().await; // first tick is immediate; we already reaped at startup
            loop {
                interval.tick().await;
                if reaper_coordinator.is_shutting_down() {
                    break;
                }
                reap_zombie_tasks(&reaper_db, &reaper_pub, &reaper_queue, &reaper_coordinator)
                    .await;
            }
        });
    }

    // Monitor any adopted containers in background poll tasks
    for (adopted_task_id, adopted_container_id, adopted_ownership_resolved) in adopted {
        let db = Arc::clone(&db);
        let sp = Arc::clone(&stream_publisher);
        let tq = Arc::clone(&task_queue_arc);
        let ex = Arc::clone(&container_executor);
        let coord = Arc::clone(&coordinator);
        let monitor_worker_id = worker_id.clone();
        tokio::spawn(async move {
            monitor_adopted_container(
                adopted_task_id,
                adopted_container_id,
                &db,
                &sp,
                &tq,
                &ex,
                &coord,
                &monitor_worker_id,
                adopted_ownership_resolved,
            )
            .await;
        });
    }

    // Background heartbeat: bump last_heartbeat_at on every task owned by
    // this worker, every `HEARTBEAT_INTERVAL`. The reaper threshold
    // (`ZOMBIE_HEARTBEAT_STALE_SECS`) is sized as a small multiple of this.
    let hb_db = Arc::clone(&db);
    let hb_worker_id = worker_id.clone();
    let hb_coordinator = Arc::clone(&coordinator);
    let heartbeat_handle = tokio::spawn(async move {
        let mut interval = tokio::time::interval(HEARTBEAT_INTERVAL);
        interval.tick().await; // first tick is immediate, skip it
        loop {
            interval.tick().await;
            if hb_coordinator.is_shutting_down() {
                break;
            }
            match Task::heartbeat(&hb_db, &hb_worker_id).await {
                Ok(count) if count > 0 => {
                    tracing::debug!("Heartbeat updated for {} task(s)", count);
                }
                Err(e) => {
                    tracing::warn!("Heartbeat update failed: {}", e);
                }
                _ => {}
            }
        }
    });

    // Periodic janitor for {hot:task}: every tick, run XAUTOCLAIM to reclaim
    // orphaned PEL entries from dead consumers; every CLEANUP_EVERY_N_TICKS,
    // also run cleanup_streams to reap stale consumers and trim old entries.
    // Mirrors the per-process janitor in hot_worker (server.rs). Without
    // this on the task_worker path, {hot:task} maintenance only happens when
    // hot_worker is also alive on the same Redis.
    //
    // Tick = 60s (aligned with ORPHAN_IDLE_MS so we don't poll inside a
    // guaranteed-empty window). Cleanup runs every 5 ticks (5min); see the
    // matching hot_worker rationale comment for details.
    if matches!(config.queue_type, QueueType::Redis) {
        use hot::queue::StreamCleanup;
        let janitor_queue = Arc::clone(&task_queue_arc);
        let janitor_coordinator = Arc::clone(&coordinator);
        tokio::spawn(async move {
            const TICK: std::time::Duration = std::time::Duration::from_secs(60);
            const CLEANUP_EVERY_N_TICKS: u64 = 5;
            let mut tick: u64 = 0;
            loop {
                tokio::time::sleep(TICK).await;
                if janitor_coordinator.is_shutting_down() {
                    tracing::debug!("Task worker janitor shutting down");
                    break;
                }
                tick = tick.wrapping_add(1);

                // Phase 1: reclaim orphaned PEL entries (every tick).
                match janitor_queue.reclaim_orphans().await {
                    Ok(0) => {}
                    Ok(n) => {
                        tracing::info!(
                            "Task worker janitor reclaimed {} orphaned message(s) on hot:task",
                            n
                        );
                    }
                    Err(e) => {
                        tracing::debug!("Task worker janitor reclaim failed on hot:task: {}", e);
                    }
                }

                // Phase 2: reap stale consumers + trim stream (every 5 ticks).
                if tick.is_multiple_of(CLEANUP_EVERY_N_TICKS) {
                    match janitor_queue.cleanup_streams().await {
                        Ok((0, 0)) => {}
                        Ok((consumers, trimmed)) => {
                            tracing::info!(
                                "Task worker janitor cleanup on hot:task: removed {} stale consumers, trimmed {} entries",
                                consumers,
                                trimmed
                            );
                        }
                        Err(e) => {
                            tracing::debug!(
                                "Task worker janitor cleanup failed on hot:task: {}",
                                e
                            );
                        }
                    }
                }
            }
        });
    }

    let shutdown = hot::signal::shutdown_signal();
    tokio::pin!(shutdown);
    let (task_loop_shutdown_tx, task_loop_shutdown_rx) = tokio::sync::watch::channel(false);

    // Fixed processor pool. Each processor owns a cloned Redis queue handle,
    // which gives it a distinct connection, consumer name, prefetch buffer, and
    // refill lock. This lets Redis distribute task entries across consumers
    // instead of funneling every claim through one mutexed connection. The task
    // lease below remains the cross-pod / cross-consumer duplicate guard.
    let mut task_processors = JoinSet::new();

    for processor_idx in 0..queue_claim_max {
        let tq = Arc::new(
            task_queue_arc
                .as_ref()
                .clone()
                .with_consumer_name(format!("{}-{}", processor_consumer_prefix, processor_idx))
                .with_read_batch_size(1),
        );
        let db_c = Arc::clone(&db);
        let stream_pub_c = Arc::clone(&stream_publisher);
        let cache_c = Arc::clone(&bytecode_cache);
        let code_sem_c = Arc::clone(&code_semaphore);
        let blocking_slots_c = Arc::clone(&blocking_execution_slots);
        let container_sem_c = Arc::clone(&container_semaphore);
        let ctr_budget_c = Arc::clone(&container_budget);
        let conf_c = config.worker_conf.clone();
        let ep_c = event_publisher.clone();
        let executor_c = Arc::clone(&container_executor);
        let vol_base_c = Arc::clone(&data_vol_base);
        let defaults_c = Arc::clone(&box_defaults);
        let usage_cache_c = Arc::clone(&usage_stats_cache);
        let coord_c = Arc::clone(&coordinator);
        let lease_c = Arc::clone(&task_lease);
        let wid_c = worker_id.clone();
        let mut task_loop_shutdown_rx = task_loop_shutdown_rx.clone();

        task_processors.spawn(async move {
            tracing::debug!(
                processor_idx,
                "Task processor started with dedicated queue consumer"
            );
            loop {
                if coord_c.is_shutting_down() {
                    break;
                }

                let result = tokio::select! {
                    biased;
                    _ = task_loop_shutdown_rx.changed() => break,
                    claim = tq.claim_blocking() => {
                        match claim {
                            Ok(Some(lease_handle)) => {
                                let queue_timing = lease_handle.timing();
                                lease_handle.process(|request: TaskRequest| {
                                    let db = Arc::clone(&db_c);
                                    let tq2 = Arc::clone(&tq);
                                    let stream_pub = Arc::clone(&stream_pub_c);
                                    let cache = Arc::clone(&cache_c);
                                    let code_sem = Arc::clone(&code_sem_c);
                                    let blocking_slots = Arc::clone(&blocking_slots_c);
                                    let container_sem = Arc::clone(&container_sem_c);
                                    let ctr_budget = Arc::clone(&ctr_budget_c);
                                    let conf = conf_c.clone();
                                    let ep = ep_c.clone();
                                    let executor = Arc::clone(&executor_c);
                                    let vol_base = Arc::clone(&vol_base_c);
                                    let defaults = Arc::clone(&defaults_c);
                                    let usage_cache = Arc::clone(&usage_cache_c);
                                    let coord = Arc::clone(&coord_c);
                                    let lease = Arc::clone(&lease_c);
                                    let wid = wid_c.clone();
                                    async move {
                                        process_task(
                                            request,
                                            db,
                                            tq2,
                                            stream_pub,
                                            cache,
                                            code_sem,
                                            blocking_slots,
                                            container_sem,
                                            ctr_budget,
                                            conf,
                                            ep,
                                            executor,
                                            vol_base,
                                            defaults,
                                            usage_cache,
                                            coord,
                                            lease,
                                            queue_timing,
                                            wid,
                                        )
                                        .await
                                    }
                                })
                                .await
                            }
                            Ok(None) => Ok(None),
                            Err(e) => Err(e),
                        }
                    }
                };

                match result {
                    Ok(Some(())) | Ok(None) => {}
                    Err(e) => {
                        tracing::error!("Task processing error: {}", e);
                    }
                }
            }

            match tq.consumer_has_pending().await {
                Ok(false) => {
                    if let Err(e) = tq.unregister_consumer().await {
                        tracing::warn!(
                            processor_idx,
                            "Task processor failed to unregister idle consumer: {}",
                            e
                        );
                    }
                }
                Ok(true) => {
                    tracing::warn!(
                        processor_idx,
                        "Task processor leaving consumer registered because it still has pending messages"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        processor_idx,
                        "Task processor could not inspect consumer pending state before unregister: {}",
                        e
                    );
                }
            }

            tracing::debug!(processor_idx, "Task processor stopped");
        });
    }

    tokio::select! {
        biased;
        _ = &mut shutdown => {
            tracing::info!("Shutting down task worker");
        }
    }

    let _ = task_loop_shutdown_tx.send(true);
    coordinator
        .initiate_shutdown(&db, &stream_publisher, &task_queue_arc)
        .await;
    let drain_processors = async {
        while let Some(join_result) = task_processors.join_next().await {
            if let Err(e) = join_result {
                tracing::error!(
                    "Task processor panicked or was cancelled during drain: {}",
                    e
                );
            }
        }
    };
    if tokio::time::timeout(std::time::Duration::from_secs(10), drain_processors)
        .await
        .is_err()
    {
        tracing::warn!("Timed out waiting for task processors to exit; aborting remaining tasks");
        task_processors.abort_all();
        while let Some(join_result) = task_processors.join_next().await {
            if let Err(e) = join_result
                && !e.is_cancelled()
            {
                tracing::error!("Task processor failed after abort: {}", e);
            }
        }
    }

    heartbeat_handle.abort();
    tracing::info!("Task worker stopped");
    Ok(())
}

/// Find tasks stuck in `running` with a stale heartbeat and fail them.
///
/// Called once at worker startup *and* on a `ZOMBIE_REAPER_INTERVAL` timer
/// from a background task. A task is considered a zombie when its
/// `last_heartbeat_at` is older than `ZOMBIE_HEARTBEAT_STALE_SECS` (or when
/// it was started >5min ago and never wrote a heartbeat at all — the legacy
/// path).
async fn reap_zombie_tasks(
    db: &DatabasePool,
    stream_publisher: &StreamPubSub,
    task_queue: &ProcessingQueue<TaskRequest>,
    coordinator: &shutdown::TaskShutdownCoordinator,
) {
    let mut zombies = match Task::find_zombie_tasks(db, ZOMBIE_HEARTBEAT_STALE_SECS).await {
        Ok(tasks) => tasks,
        Err(e) => {
            tracing::warn!("Failed to query zombie tasks: {}", e);
            return;
        }
    };

    // Also check for legacy running tasks without any heartbeat (pre-migration)
    let mut legacy = match Task::find_running_without_heartbeat(db).await {
        Ok(tasks) => tasks,
        Err(e) => {
            tracing::warn!("Failed to query legacy running tasks: {}", e);
            Vec::new()
        }
    };

    // Tasks registered with this worker's coordinator are alive in-process
    // even when their DB row looks stale — an adopted container whose
    // `set_worker` ownership repair has not landed yet still carries the dead
    // worker's id and heartbeat. Reaping one would fail a live execution and
    // enqueue a duplicate run, so skip anything this worker actively manages.
    let keep_inactive = |task_id: &Uuid| {
        let active = coordinator.is_task_active(task_id);
        if active {
            tracing::info!(
                task_id = %task_id,
                "Skipping zombie candidate actively managed by this worker (adopted or in-flight)"
            );
        }
        !active
    };
    zombies.retain(|task| keep_inactive(&task.task_id));
    legacy.retain(|task| keep_inactive(&task.task_id));

    let total = zombies.len() + legacy.len();
    if total == 0 {
        tracing::debug!("Reaper pass: no zombie tasks");
        return;
    }

    tracing::warn!(
        "Reaper pass: {} zombie task(s) ({} stale heartbeat >{}s, {} no heartbeat)",
        total,
        zombies.len(),
        ZOMBIE_HEARTBEAT_STALE_SECS,
        legacy.len(),
    );

    for task in zombies.into_iter().chain(legacy) {
        let error = serde_json::json!({
            "$type": "::hot::task/Failure",
            "$val": {
                "msg": "Task interrupted by worker crash (zombie reaper)",
                "err": null
            }
        });

        match Task::complete(
            db,
            &task.task_id,
            &db::TaskStatus::Failed,
            Some(&error),
            None,
            None,
        )
        .await
        {
            Ok(true) => {}
            Ok(false) => {
                tracing::info!(task_id = %task.task_id, "Zombie task became terminal before reaper update; skipping stale event and retry");
                continue;
            }
            Err(e) => {
                tracing::error!(task_id = %task.task_id, "Failed to reap zombie task: {}", e);
                continue;
            }
        }

        let duration_ms = task.duration_ms;
        let event = EnvEvent::TaskComplete {
            task_id: task.task_id,
            env_id: task.env_id,
            stream_id: task.stream_id,
            function_name: task.function_name.clone(),
            status: "failed".to_string(),
            duration_ms,
            error: Some(error),
        };
        if let Err(e) = stream_publisher.publish_env(event).await {
            tracing::warn!(task_id = %task.task_id, "Failed to publish zombie reap event: {}", e);
        }

        // Attempt retry if the task had retry config
        maybe_retry_zombie_task(db, &task, task_queue).await;

        tracing::warn!(
            task_id = %task.task_id,
            function = %task.function_name,
            worker_id = ?task.worker_id,
            "Reaped zombie task",
        );
    }
}

/// Build a `TaskRequest` from the task row alone, with no further DB reads.
/// Carries the full execution identity (function, args, env/stream/build,
/// timeout) but no org/project enrichment — `task_request_from_db_row` layers
/// that on top when the extra lookups succeed. Also the adoption fallback
/// when those lookups keep failing: org/project context is best-effort
/// (quota and feature resolution during container re-execution), while
/// refusing adoption over it leaves a live container unmanaged.
fn synthesize_task_request_from_row(task: &Task) -> TaskRequest {
    TaskRequest {
        task_id: task.task_id.to_string(),
        function_name: task.function_name.clone(),
        args: task.args.clone().unwrap_or(serde_json::Value::Null),
        stream_id: task.stream_id.to_string(),
        env_id: task.env_id.to_string(),
        build_id: task.build_id.to_string(),
        org_id: None,
        user_id: task.by_user_id.map(|id| id.to_string()),
        project_id: None,
        project_name: None,
        timeout_ms: task.timeout_ms.max(1_000) as u64,
        task_type: task.task_type.clone(),
        created_at_unix_ms: task.created_at.timestamp_millis().max(0) as u64,
        origin_run_id: task.origin_run_id.map(|id| id.to_string()),
    }
}

async fn task_request_from_db_row(
    db: &DatabasePool,
    task: &Task,
) -> Result<TaskRequest, Box<dyn std::error::Error + Send + Sync>> {
    let env = Env::get_env(db, &task.env_id).await?;
    let (project_id, project_name) = match Build::get_build(db, &task.build_id).await {
        Ok(build) => match Project::get_project(db, &build.project_id).await {
            Ok(project) => (
                Some(project.project_id.to_string()),
                Some(project.name.to_string()),
            ),
            Err(e) => {
                tracing::warn!(
                    task_id = %task.task_id,
                    build_id = %task.build_id,
                    project_id = %build.project_id,
                    "Failed to load project while reconstructing task request: {}", e,
                );
                (Some(build.project_id.to_string()), None)
            }
        },
        Err(e) => {
            tracing::warn!(
                task_id = %task.task_id,
                build_id = %task.build_id,
                "Failed to load build while reconstructing task request: {}", e,
            );
            (None, None)
        }
    };

    let mut request = synthesize_task_request_from_row(task);
    request.org_id = Some(env.org_id.to_string());
    request.project_id = project_id;
    request.project_name = project_name;
    Ok(request)
}

async fn reconcile_queued_tasks(
    db: &DatabasePool,
    task_queue: &ProcessingQueue<TaskRequest>,
    reconcile_after_secs: u64,
    reconcile_interval_secs: u64,
) {
    let now = chrono::Utc::now();
    let cutoff = now - chrono::Duration::seconds(reconcile_after_secs as i64);
    let tasks = match Task::get_stale_queued(db, cutoff, now, 100).await {
        Ok(tasks) => tasks,
        Err(e) => {
            tracing::warn!("Queued-task reconciler failed to query stale tasks: {}", e);
            return;
        }
    };

    for task in tasks {
        let task_id = task.task_id;
        let request = match task_request_from_db_row(db, &task).await {
            Ok(request) => request,
            Err(e) => {
                tracing::warn!(
                    task_id = %task_id,
                    "Queued-task reconciler skipped task because request reconstruction failed: {}", e,
                );
                continue;
            }
        };

        match task_queue.enqueue(request).await {
            Ok(()) => {
                let next_check_at =
                    chrono::Utc::now() + chrono::Duration::seconds(reconcile_interval_secs as i64);
                if let Err(e) = Task::defer_queued_reconcile(db, &task_id, next_check_at).await {
                    tracing::warn!(
                        task_id = %task_id,
                        "Queued-task reconciler enqueued task but failed to defer next check: {}", e,
                    );
                }
                tracing::debug!(
                    task_id = %task_id,
                    "Queued-task reconciler re-enqueued stale queued task"
                );
            }
            Err(e) => {
                tracing::warn!(
                    task_id = %task_id,
                    "Queued-task reconciler failed to enqueue replacement message: {}", e,
                );
            }
        }
    }
}

/// Retry a zombie task if it has retry config and retries remain.
/// Reconstructs a `TaskRequest` from the DB row and enqueues it.
async fn maybe_retry_zombie_task(
    db: &DatabasePool,
    task: &Task,
    task_queue: &ProcessingQueue<TaskRequest>,
) {
    let options = match &task.options {
        Some(opts) => opts,
        None => return,
    };

    let retry_config = RetryConfig::from_meta(Some(options));
    if !retry_config.is_enabled() {
        return;
    }

    if task.retry_attempt >= retry_config.max_retries {
        tracing::info!(
            task_id = %task.task_id,
            attempt = task.retry_attempt,
            max = retry_config.max_retries,
            "Zombie task exhausted all retries",
        );
        return;
    }

    let next_attempt = task.retry_attempt + 1;
    let delay_ms = retry_config.delay_for_attempt(next_attempt);
    let next_retry_at = chrono::Utc::now() + chrono::Duration::milliseconds(delay_ms);
    let new_task_id = Uuid::now_v7();

    match Task::insert_retry(db, &new_task_id, task, next_attempt, next_retry_at).await {
        Ok(true) => {}
        Ok(false) => {
            // The (parent, attempt) unique key says another writer — e.g. the
            // failure path's maybe_retry_task, or a crashed earlier reap that
            // got as far as insert_retry — already created this retry row.
            // Skip the enqueue: the row's creator owns delivery, and if it
            // crashed before enqueueing, reconcile_queued_tasks re-enqueues
            // the stale queued row, so the retry cannot be stranded.
            tracing::info!(
                task_id = %task.task_id,
                attempt = next_attempt,
                "Zombie retry row already exists for this attempt — skipping duplicate retry"
            );
            return;
        }
        Err(e) => {
            tracing::error!(
                task_id = %task.task_id,
                new_task_id = %new_task_id,
                "Failed to insert retry for zombie task: {}", e,
            );
            return;
        }
    }

    let org_id = match hot::db::Env::get_env(db, &task.env_id).await {
        Ok(env) => Some(env.org_id.to_string()),
        Err(e) => {
            tracing::warn!(
                task_id = %task.task_id,
                env_id = %task.env_id,
                "Failed to resolve org for zombie retry: {}", e,
            );
            None
        }
    };

    // Reconstruct a TaskRequest from the DB row and enqueue it.
    // project_id/project_name are not stored on the task row, but org_id is
    // needed for quota and feature resolution in container execution.
    let retry_request = TaskRequest {
        task_id: new_task_id.to_string(),
        env_id: task.env_id.to_string(),
        stream_id: task.stream_id.to_string(),
        build_id: task.build_id.to_string(),
        function_name: task.function_name.clone(),
        args: task.args.clone().unwrap_or(serde_json::Value::Null),
        task_type: task.task_type.clone(),
        timeout_ms: task.timeout_ms as u64,
        origin_run_id: task.origin_run_id.map(|id| id.to_string()),
        org_id,
        user_id: task.by_user_id.map(|id| id.to_string()),
        project_id: None,
        project_name: None,
        created_at_unix_ms: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64,
    };

    if delay_ms > 0 {
        let tq = task_queue.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(delay_ms as u64)).await;
            if let Err(e) = tq.enqueue(retry_request).await {
                tracing::error!(new_task_id = %new_task_id, "Failed to enqueue zombie retry: {}", e);
            } else {
                tracing::debug!(new_task_id = %new_task_id, attempt = next_attempt, "Zombie retry enqueued after delay");
            }
        });
    } else if let Err(e) = task_queue.enqueue(retry_request).await {
        tracing::error!(new_task_id = %new_task_id, "Failed to enqueue zombie retry: {}", e);
    } else {
        tracing::debug!(new_task_id = %new_task_id, attempt = next_attempt, "Zombie retry enqueued immediately");
    }
}

fn validate_task_request_matches_db(
    request: &TaskRequest,
    task: &Task,
    env_id: Uuid,
    stream_id: Uuid,
    build_id: Uuid,
) -> Result<(), String> {
    if task.env_id != env_id {
        return Err(format!(
            "env_id mismatch: queue={} db={}",
            env_id, task.env_id
        ));
    }
    if task.stream_id != stream_id {
        return Err(format!(
            "stream_id mismatch: queue={} db={}",
            stream_id, task.stream_id
        ));
    }
    if task.build_id != build_id {
        return Err(format!(
            "build_id mismatch: queue={} db={}",
            build_id, task.build_id
        ));
    }
    if task.function_name != request.function_name {
        return Err(format!(
            "function_name mismatch: queue={} db={}",
            request.function_name, task.function_name
        ));
    }
    if task.task_type != request.task_type {
        return Err(format!(
            "task_type mismatch: queue={} db={}",
            request.task_type, task.task_type
        ));
    }

    Ok(())
}

/// Decide whether a queue delivery is eligible to execute. Running rows must
/// remain unacknowledged until a terminal state is durable. If this process
/// owns a running row but no longer has it in flight, release ownership so the
/// zombie reaper can reconcile it instead of letting the batch heartbeat keep
/// it alive forever.
async fn task_message_should_execute(
    db: &DatabasePool,
    task: &Task,
    coordinator: &shutdown::TaskShutdownCoordinator,
    worker_id: &str,
    infra_retry_backoff_ms: u64,
) -> Result<bool, QueueInfrastructureError> {
    if task.task_status_id == TaskStatus::Queued.as_id() {
        return Ok(true);
    }
    if task.task_status_id != TaskStatus::Running.as_id() {
        return Ok(false);
    }

    if task.worker_id.as_deref() == Some(worker_id) && !coordinator.is_task_active(&task.task_id) {
        match tokio::time::timeout(
            DB_CALL_TIMEOUT,
            Task::release_worker(db, &task.task_id, worker_id),
        )
        .await
        {
            Ok(Ok(true)) => {}
            Ok(Ok(false)) => {
                tracing::info!(task_id = %task.task_id, "Inactive task ownership changed concurrently; nothing to release");
            }
            Ok(Err(e)) => {
                tracing::warn!(task_id = %task.task_id, "Failed to release inactive task ownership: {}", e);
            }
            Err(_) => {
                tracing::warn!(task_id = %task.task_id, "Timed out releasing inactive task ownership");
            }
        }
    }

    Err(QueueInfrastructureError::new(
        "task is still running",
        std::time::Duration::from_millis(infra_retry_backoff_ms.max(5_000)),
    ))
}

/// Load the task row before execution. `Ok(None)` means the row provably does
/// not exist (`TaskError::NotFound`), which is the only outcome where ACKing
/// the queue message and skipping is safe. Every other failure (transport
/// error, closed/saturated pool, timeout) leaves the row's state unknown and
/// must defer the delivery instead — a fast pool error would otherwise ACK a
/// redelivery for a row this worker keeps alive with its batch heartbeat,
/// leaving the task stuck in `running` forever.
async fn load_task_for_execution(
    db: &DatabasePool,
    task_id: &Uuid,
    infra_retry_backoff_ms: u64,
) -> Result<Option<Task>, QueueInfrastructureError> {
    match tokio::time::timeout(DB_CALL_TIMEOUT, Task::get(db, task_id)).await {
        Ok(Ok(task)) => Ok(Some(task)),
        Ok(Err(hot::db::TaskError::NotFound)) => Ok(None),
        Ok(Err(e)) => Err(QueueInfrastructureError::new(
            format!("failed to load task {} before execution: {}", task_id, e),
            std::time::Duration::from_millis(infra_retry_backoff_ms),
        )),
        Err(_) => Err(QueueInfrastructureError::new(
            format!("timed out loading task {} before execution", task_id),
            std::time::Duration::from_millis(infra_retry_backoff_ms),
        )),
    }
}

async fn claim_task_for_execution(
    db: &DatabasePool,
    task_id: &Uuid,
    worker_id: &str,
    backoff: std::time::Duration,
) -> Result<bool, QueueInfrastructureError> {
    match tokio::time::timeout(
        DB_CALL_TIMEOUT,
        Task::claim_for_worker(db, task_id, worker_id),
    )
    .await
    {
        Ok(Ok(true)) => Ok(true),
        Ok(Ok(false)) => Ok(false),
        Ok(Err(e)) => Err(QueueInfrastructureError::new(
            format!("failed to atomically claim task {}: {}", task_id, e),
            backoff,
        )),
        Err(_) => Err(QueueInfrastructureError::new(
            format!("timed out atomically claiming task {}", task_id),
            backoff,
        )),
    }
}

async fn acquire_container_slot(
    semaphore: Arc<Semaphore>,
    grace: std::time::Duration,
    backoff: std::time::Duration,
) -> Result<tokio::sync::OwnedSemaphorePermit, QueueInfrastructureError> {
    match tokio::time::timeout(grace, semaphore.acquire_owned()).await {
        Ok(Ok(permit)) => Ok(permit),
        Ok(Err(_)) => Err(QueueInfrastructureError::new(
            "container task semaphore closed",
            backoff,
        )),
        Err(_) => Err(QueueInfrastructureError::new(
            "container task slot unavailable; deferring to free claim slot",
            backoff,
        )),
    }
}

/// Acquire a slot against the total-live-VM-executions cap. The returned
/// owned permit must be moved into the execution's `spawn_blocking` closure
/// so a detached (timed-out) thread keeps holding it until the thread itself
/// exits. Saturation defers the queue message for an infrastructure retry,
/// exactly like the container cap's admission behavior.
async fn acquire_blocking_execution_slot(
    slots: Arc<Semaphore>,
    grace: std::time::Duration,
    backoff: std::time::Duration,
) -> Result<tokio::sync::OwnedSemaphorePermit, QueueInfrastructureError> {
    match tokio::time::timeout(grace, slots.acquire_owned()).await {
        Ok(Ok(permit)) => Ok(permit),
        Ok(Err(_)) => Err(QueueInfrastructureError::new(
            "blocking execution limiter closed",
            backoff,
        )),
        Err(_) => Err(QueueInfrastructureError::new(
            "blocking execution slot unavailable; deferring to free claim slot",
            backoff,
        )),
    }
}

fn task_status_is_terminal(status_id: i16) -> bool {
    matches!(
        TaskStatus::from_id(status_id),
        Some(
            TaskStatus::Completed
                | TaskStatus::Failed
                | TaskStatus::Cancelled
                | TaskStatus::TimedOut
        )
    )
}

/// Process a single task request.
#[allow(clippy::too_many_arguments)]
async fn process_task(
    request: TaskRequest,
    db: Arc<DatabasePool>,
    task_queue: Arc<ProcessingQueue<TaskRequest>>,
    stream_publisher: Arc<StreamPubSub>,
    bytecode_cache: Arc<BytecodeCache>,
    code_semaphore: Arc<Semaphore>,
    blocking_execution_slots: Arc<Semaphore>,
    container_semaphore: Arc<Semaphore>,
    container_budget: Arc<resource_budget::ResourceBudget>,
    worker_conf: Val,
    event_publisher: Option<Arc<dyn EventPublisher>>,
    container_executor: Arc<executor::BoxExecutor>,
    data_vol_base: Arc<std::path::PathBuf>,
    box_defaults: Arc<box_limits::BoxDefaults>,
    usage_stats_cache: UsageStatsCache,
    coordinator: Arc<shutdown::TaskShutdownCoordinator>,
    task_lease: Arc<dyn task_lease::TaskLease>,
    queue_timing: QueueLeaseTiming,
    worker_id: String,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let task_id = Uuid::parse_str(&request.task_id)?;
    let stream_id = Uuid::parse_str(&request.stream_id)?;
    let env_id = Uuid::parse_str(&request.env_id)?;
    let build_id = Uuid::parse_str(&request.build_id)?;
    let timeout_ms = request.timeout_ms.max(1000);
    let infra_retry_backoff_ms = worker_conf
        .get_int_or_default("queue.infra-retry-backoff-ms", 1_000)
        .max(0) as u64;

    tracing::info!(
        task_id = %task_id,
        function = %request.function_name,
        task_type = %request.task_type,
        "Processing task"
    );

    let task = match load_task_for_execution(&db, &task_id, infra_retry_backoff_ms).await {
        Ok(Some(task)) => task,
        Ok(None) => {
            tracing::error!(
                task_id = %task_id,
                "Rejecting task queue message with no matching DB row",
            );
            return Ok(());
        }
        Err(e) => {
            tracing::warn!(
                task_id = %task_id,
                backoff_ms = infra_retry_backoff_ms,
                "Failed to load task before execution; deferring queue message: {}", e,
            );
            return Err(Box::new(e));
        }
    };

    match task_message_should_execute(&db, &task, &coordinator, &worker_id, infra_retry_backoff_ms)
        .await
    {
        Ok(true) => {}
        Err(e) => {
            tracing::warn!(
                task_id = %task_id,
                owner = ?task.worker_id,
                "Task is still running; withholding queue ACK until it is terminal"
            );
            return Err(Box::new(e));
        }
        Ok(false) => {
            tracing::info!(
                task_id = %task_id,
                status = %task.status,
                "Task is no longer queued, skipping duplicate queue message"
            );
            return Ok(());
        }
    }

    if let Err(e) = validate_task_request_matches_db(&request, &task, env_id, stream_id, build_id) {
        tracing::error!(
            task_id = %task_id,
            "Rejecting task queue message that does not match DB row: {}", e,
        );
        return Ok(());
    }

    let queue_wait_ms = queue_timing.queue_wait.as_millis().min(i64::MAX as u128) as i64;
    let publish_wait_ms = queue_timing.enqueued_at.map(|enqueued_at| {
        enqueued_at
            .signed_duration_since(task.created_at)
            .num_milliseconds()
            .max(0)
    });
    let initial_timing = serde_json::json!({
        "queue_backend": if queue_timing.enqueued_at.is_some() { "redis" } else { "memory" },
        "enqueued_at": queue_timing.enqueued_at.map(|value| value.to_rfc3339()),
        "claimed_at": queue_timing.claimed_at.to_rfc3339(),
        "publish_wait_ms": publish_wait_ms,
        "queue_wait_ms": if queue_timing.redelivered { None } else { Some(queue_wait_ms) },
        "retry_queue_age_ms": if queue_timing.redelivered { Some(queue_wait_ms) } else { None },
        "redelivered": queue_timing.redelivered,
    });
    match tokio::time::timeout(
        TASK_TIMING_DB_TIMEOUT,
        Task::set_timing(&db, &task_id, &initial_timing),
    )
    .await
    {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            tracing::warn!(task_id = %task_id, "Failed to persist task queue timing: {}", e);
        }
        Err(_) => {
            tracing::warn!(task_id = %task_id, "Task queue timing write timed out; continuing");
        }
    }

    // Register with shutdown coordinator for graceful drain. We stash the
    // full original TaskRequest so the coordinator can re-enqueue an
    // identical retry copy if SIGTERM arrives mid-execution.
    //
    // `try_register_task` also serves as an in-process dedup gate: if the
    // queue redelivers a task_id that's still in flight from a previous
    // dispatch (XAUTOCLAIM reviving a stale PEL entry, a producer with a
    // stuck retry loop, etc.), the second dispatch is silently dropped
    // here. Without this guard, two concurrent runs of the same task_id
    // race on shared per-task resources — most visibly on the bind-mount
    // path of the data volume, where one run's cleanup yanks the
    // directory out from under its sibling and Docker reports
    // `failed to fulfil mount request: ... no such file or directory`.
    if !coordinator.try_register_task(shutdown::ActiveTask {
        task_id,
        env_id,
        stream_id,
        function_name: request.function_name.clone(),
        task_type: request.task_type.clone(),
        cancel_token: None, // Updated by process_code_task after VM spawn
        original_request: request.clone(),
    }) {
        tracing::warn!(
            task_id = %task_id,
            function = %request.function_name,
            task_type = %request.task_type,
            "Skipping duplicate dispatch — task is already in flight on this worker"
        );
        return Ok(());
    }

    // Cross-pod mutual exclusion. The in-process `try_register_task` above
    // catches duplicate dispatches inside this worker, but cannot see across
    // pod boundaries. Without this lease, `XAUTOCLAIM` reclaiming a
    // long-running task's PEL entry to a sibling pod would let both pods
    // run the same `task_id` concurrently — both would write results to the
    // DB, both would publish completion events, and any per-task external
    // side effect would happen twice. See `task_lease.rs` module docs.
    //
    // Acquire failure modes:
    //   - `Ok(None)` (sibling owns it): ACK and walk away. We're not the
    //     rightful processor for this dispatch.
    //   - `Err(_)` (transport): do not ACK/drop and do not consume poison
    //     message retry budget. Surface a queue infrastructure retry so the
    //     queue can defer and requeue the message as fresh work.
    //
    // The guard is bound for the rest of `process_task` — its `Drop`
    // releases the lease when the body returns (success, error, panic).
    let lease_guard = match task_lease
        .try_acquire(task_id, task_lease::DEFAULT_LEASE_TTL)
        .await
    {
        Ok(Some(guard)) => guard,
        Ok(None) => {
            tracing::warn!(
                task_id = %task_id,
                function = %request.function_name,
                task_type = %request.task_type,
                worker_id = %worker_id,
                "Skipping duplicate dispatch — task lease held by sibling worker"
            );
            coordinator.unregister_task(&task_id);
            return Ok(());
        }
        Err(e) => {
            tracing::warn!(
                task_id = %task_id,
                error = %e,
                backoff_ms = infra_retry_backoff_ms,
                "Task lease acquire failed; deferring queue message for infrastructure retry"
            );
            coordinator.unregister_task(&task_id);
            return Err(Box::new(QueueInfrastructureError::new(
                format!("task lease acquire failed: {}", e),
                std::time::Duration::from_millis(infra_retry_backoff_ms),
            )));
        }
    };
    let lease_lost_notify = lease_guard.lost_notify();

    let terminal_state_db = Arc::clone(&db);
    let execute_task = async {
        if request.task_type == "container" {
            // Keep the explicit/derived container max as a hard task count in
            // addition to the per-task memory/disk admission budget.
            let _permit = match acquire_container_slot(
                Arc::clone(&container_semaphore),
                CONTAINER_SLOT_ACQUIRE_GRACE,
                std::time::Duration::from_millis(infra_retry_backoff_ms),
            )
            .await
            {
                Ok(permit) => permit,
                Err(e) => {
                    tracing::warn!(
                        task_id = %task_id,
                        backoff_ms = infra_retry_backoff_ms,
                        "Container task slot unavailable within grace window; deferring queue message"
                    );
                    return Err(Box::new(e) as Box<dyn std::error::Error + Send + Sync>);
                }
            };
            process_container_task(
                request,
                task_id,
                env_id,
                stream_id,
                build_id,
                timeout_ms,
                db,
                task_queue,
                stream_publisher,
                container_executor,
                container_budget,
                data_vol_base,
                worker_conf,
                box_defaults,
                usage_stats_cache,
                queue_timing,
                worker_id.clone(),
            )
            .await
        } else {
            // Code tasks use high-limit semaphore. Bound the wait so a claimed
            // code task does not hold its queue lease + shared inflight slot
            // indefinitely while parked behind `code_max` peers (which would also
            // head-of-line block container claims sharing the inflight cap). If no
            // permit frees up within the grace window, defer the message back to
            // the queue as fresh work so the slot is released promptly.
            let capacity_wait_started = std::time::Instant::now();
            let _permit = match tokio::time::timeout(
                CODE_SLOT_ACQUIRE_GRACE,
                code_semaphore.acquire(),
            )
            .await
            {
                Ok(permit) => permit?,
                Err(_) => {
                    tracing::warn!(
                        task_id = %task_id,
                        backoff_ms = infra_retry_backoff_ms,
                        "Code task slot unavailable within grace window; deferring queue message to free claim slot"
                    );
                    return Err(Box::new(QueueInfrastructureError::new(
                        "code task slot unavailable; deferring to free claim slot",
                        std::time::Duration::from_millis(infra_retry_backoff_ms),
                    ))
                        as Box<dyn std::error::Error + Send + Sync>);
                }
            };
            // Second admission gate: total live VM executions, including
            // detached threads from previously timed-out tasks. The code
            // permit above frees as soon as this task's future completes
            // (even when its VM thread is still wedged), so it alone cannot
            // stop admission while detached threads accumulate toward the
            // tokio blocking-pool cap. This owned permit is moved into the
            // spawn_blocking closure and only released when the thread
            // itself exits. Acquired before the row is claimed so saturation
            // defers a still-queued message instead of stranding a running
            // row.
            let blocking_execution_permit = match acquire_blocking_execution_slot(
                Arc::clone(&blocking_execution_slots),
                BLOCKING_SLOT_ACQUIRE_GRACE,
                std::time::Duration::from_millis(infra_retry_backoff_ms),
            )
            .await
            {
                Ok(permit) => permit,
                Err(e) => {
                    tracing::warn!(
                        task_id = %task_id,
                        backoff_ms = infra_retry_backoff_ms,
                        available_permits = blocking_execution_slots.available_permits(),
                        "Blocking execution slot unavailable within grace window; detached timed-out VM threads are holding the permits — deferring queue message"
                    );
                    return Err(Box::new(e) as Box<dyn std::error::Error + Send + Sync>);
                }
            };
            let capacity_wait_ms = capacity_wait_started
                .elapsed()
                .as_millis()
                .min(i64::MAX as u128) as i64;

            match claim_task_for_execution(
                &db,
                &task_id,
                &worker_id,
                std::time::Duration::from_millis(infra_retry_backoff_ms),
            )
            .await
            {
                Ok(true) => {}
                Ok(false) => return Ok(()),
                Err(e) => {
                    return Err(Box::new(e) as Box<dyn std::error::Error + Send + Sync>);
                }
            }

            emit_task_started(
                &stream_publisher,
                task_id,
                env_id,
                stream_id,
                &request.function_name,
                &request.task_type,
            )
            .await;

            process_code_task(
                request,
                task_id,
                stream_id,
                env_id,
                build_id,
                timeout_ms,
                db,
                task_queue,
                stream_publisher,
                bytecode_cache,
                worker_conf,
                event_publisher,
                Arc::clone(&coordinator),
                queue_timing.claimed_at,
                capacity_wait_ms,
                worker_id.clone(),
                blocking_execution_permit,
            )
            .await
        }
    };

    let result = if let Some(lease_lost_notify) = lease_lost_notify {
        tokio::select! {
            biased;
            _ = lease_lost_notify.notified() => {
                coordinator.cancel_task(&task_id);
                tracing::warn!(
                    task_id = %task_id,
                    backoff_ms = infra_retry_backoff_ms,
                    "Task lease lost while processing; cancelling local work and deferring queue message"
                );
                Err(Box::new(QueueInfrastructureError::new(
                    "task lease lost while processing",
                    std::time::Duration::from_millis(infra_retry_backoff_ms),
                )) as Box<dyn std::error::Error + Send + Sync>)
            }
            result = execute_task => {
                if lease_guard.is_lost() {
                    coordinator.cancel_task(&task_id);
                    tracing::warn!(
                        task_id = %task_id,
                        backoff_ms = infra_retry_backoff_ms,
                        "Task completed after lease loss; deferring queue message instead of acknowledging"
                    );
                    Err(Box::new(QueueInfrastructureError::new(
                        "task lease lost before completion acknowledgement",
                        std::time::Duration::from_millis(infra_retry_backoff_ms),
                    )) as Box<dyn std::error::Error + Send + Sync>)
                } else {
                    result
                }
            }
        }
    } else {
        execute_task.await
    };

    coordinator.unregister_task(&task_id);
    if result.is_ok() {
        let terminal_is_durable = matches!(
            tokio::time::timeout(DB_CALL_TIMEOUT, Task::get(&terminal_state_db, &task_id)).await,
            Ok(Ok(task)) if task_status_is_terminal(task.task_status_id)
        );
        if !terminal_is_durable {
            tracing::error!(
                task_id = %task_id,
                "Task execution ended without a durable terminal DB state; withholding queue ACK"
            );
            match tokio::time::timeout(
                DB_CALL_TIMEOUT,
                Task::release_worker(&terminal_state_db, &task_id, &worker_id),
            )
            .await
            {
                Ok(Ok(true)) => {}
                Ok(Ok(false)) => {
                    tracing::info!(task_id = %task_id, "Task worker ownership changed concurrently; nothing to release");
                }
                Ok(Err(e)) => {
                    tracing::warn!(task_id = %task_id, "Failed to release task worker ownership: {}", e);
                }
                Err(_) => {
                    tracing::warn!(task_id = %task_id, "Timed out releasing task worker ownership");
                }
            }
            return Err(Box::new(QueueInfrastructureError::new(
                "task terminal state was not persisted",
                std::time::Duration::from_millis(infra_retry_backoff_ms.max(5_000)),
            )));
        }
    }
    result
}

#[allow(clippy::too_many_arguments)]
async fn finish_container_setup_timeout<G: Send>(
    db: &DatabasePool,
    stream_publisher: &StreamPubSub,
    task_queue: &ProcessingQueue<TaskRequest>,
    request: &TaskRequest,
    task_id: &Uuid,
    env_id: Uuid,
    stream_id: Uuid,
    org_id: Option<Uuid>,
    function_name: &str,
    task_type: &str,
    worker_id: &str,
    phase: &str,
    file_server_handle: Option<file_server::FileServerHandle>,
    data_volume: Option<data_volume::DataVolume>,
    resource_guard: G,
) {
    // Release shared admission capacity before doing best-effort persistence
    // and cleanup so a slow dependency cannot amplify the timeout into
    // process-wide head-of-line blocking.
    drop(resource_guard);

    let error = task_failure_json(
        &format!("Container task timed out during {phase}"),
        Some(serde_json::json!({"phase": phase})),
    );
    let persisted = complete_task_with_event(
        db,
        stream_publisher,
        task_id,
        env_id,
        stream_id,
        function_name,
        task_type,
        TaskStatus::TimedOut,
        Some(&error),
        None,
        Some(worker_id),
    )
    .await;
    if persisted {
        publish_task_alert(db, org_id, env_id, task_id, "task:failed", &error).await;
        // A setup timeout is a terminal failure of THIS attempt exactly like
        // the runtime failure/timeout arms one await later: honor the user's
        // retry budget through the same gate, strictly after the fenced
        // terminal write persisted (persist-before-retry).
        maybe_retry_task(db, task_queue, task_id, request).await;
    }

    if let Some(handle) = file_server_handle
        && tokio::time::timeout(CONTAINER_KILL_TIMEOUT, handle.shutdown())
            .await
            .is_err()
    {
        tracing::warn!(task_id = %task_id, "file_server shutdown timed out after setup deadline");
    }
    if let Some(volume) = data_volume {
        cleanup_data_volume_with_timeout(task_id, volume).await;
    }
}

/// Execute a container task (task_type == "container").
/// Resolves limits, performs quota checks, acquires resources, then dispatches.
#[allow(clippy::too_many_arguments)]
async fn process_container_task(
    request: TaskRequest,
    task_id: Uuid,
    env_id: Uuid,
    stream_id: Uuid,
    build_id: Uuid,
    timeout_ms: u64,
    db: Arc<DatabasePool>,
    task_queue: Arc<ProcessingQueue<TaskRequest>>,
    stream_publisher: Arc<StreamPubSub>,
    executor: Arc<executor::BoxExecutor>,
    budget: Arc<resource_budget::ResourceBudget>,
    data_vol_base: Arc<std::path::PathBuf>,
    worker_conf: Val,
    box_defaults: Arc<box_limits::BoxDefaults>,
    usage_stats_cache: UsageStatsCache,
    queue_timing: QueueLeaseTiming,
    worker_id: String,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let args = &request.args;
    let function_name = request.function_name.clone();

    // Parse org_id early so it's available for all error paths
    let org_id = request
        .org_id
        .as_deref()
        .and_then(|s| Uuid::parse_str(s).ok());

    let image = args
        .get("image")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();

    if image.is_empty() {
        let error = task_failure_json("Missing 'image' in container task args", None);
        // Pre-claim failure: the row was never claimed by this worker, so no
        // ownership fence applies here (or at the other pre-claim sites).
        if complete_task_with_event(
            &db,
            &stream_publisher,
            &task_id,
            env_id,
            stream_id,
            &function_name,
            &request.task_type,
            TaskStatus::Failed,
            Some(&error),
            None,
            None,
        )
        .await
        {
            publish_task_alert(&db, org_id, env_id, &task_id, "task:failed", &error).await;
        }
        return Ok(());
    }

    let cmd: Option<Vec<String>> = {
        let script = args
            .get("script")
            .and_then(|v| v.as_str())
            .map(String::from);

        if let Some(script_body) = script {
            // `script` field: write to /tmp/hot-run.sh and execute with `set -e`.
            // Do not enable xtrace: expanded command arguments may contain secrets.
            // `mkdir -p /data` ensures the disk-backed working directory exists.
            let full_script = format!("{CONTAINER_SCRIPT_PRELUDE}{}", script_body.trim());
            // Standard base64 (not URL-safe) — `base64 -d` in busybox/Alpine
            // expects this encoding.  The output is placed inside single quotes
            // in the shell command, so +/= are not interpreted by the shell.
            let encoded = general_purpose::STANDARD.encode(full_script.as_bytes());
            Some(vec![
                "sh".to_string(),
                "-c".to_string(),
                format!(
                    "echo '{}' | base64 -d > /tmp/hot-run.sh && sh /tmp/hot-run.sh",
                    encoded
                ),
            ])
        } else {
            // `cmd` field: pass through as-is but inject `-e` when the user is
            // already using `sh -c "..."`. Never inject xtrace because expanded
            // command arguments may contain secrets.
            // Prepend `mkdir -p /data` so the disk-backed working directory exists.
            args.get("cmd").and_then(|v| {
                v.as_array().map(|arr| {
                    let items: Vec<String> = arr
                        .iter()
                        .filter_map(|item| item.as_str().map(String::from))
                        .collect();
                    if items.len() == 3 && items[0] == "sh" && items[1] == "-c" {
                        vec![
                            "sh".to_string(),
                            CONTAINER_SHELL_FLAGS.to_string(),
                            format!("mkdir -p /data && {}", items[2]),
                        ]
                    } else {
                        items
                    }
                })
            })
        }
    };

    let env: Option<Vec<String>> = args.get("env").and_then(|v| {
        v.as_array().map(|arr| {
            arr.iter()
                .filter_map(|item| item.as_str().map(String::from))
                .collect()
        })
    });

    // -- Resolve features for limit/quota checks --
    let features = if let Some(oid) = &org_id {
        hot::db::features::Features::resolve_for_org(&db, oid).await
    } else {
        hot::db::features::Features::unlimited()
    };

    // -- Resolve 5-tier BoxLimits with worker-level defaults --
    let limits = box_limits::BoxLimits::resolve_with_defaults(&features, args, &box_defaults);

    // -- Pre-start quota checks --
    if let Some(oid) = &org_id {
        // Check concurrent container limit
        let concurrent_limit = features.box_concurrent_tasks();
        if concurrent_limit > 0 {
            match Task::count_running_containers_for_org(
                &db,
                oid,
                hot::db::task::QUOTA_HEARTBEAT_FRESH_SECS,
            )
            .await
            {
                Ok(running) if running >= concurrent_limit => {
                    let msg = format!(
                        "Concurrent container limit reached ({}/{})",
                        running, concurrent_limit
                    );
                    let error = task_failure_json(&msg, None);
                    if complete_task_with_event(
                        &db,
                        &stream_publisher,
                        &task_id,
                        env_id,
                        stream_id,
                        &function_name,
                        &request.task_type,
                        TaskStatus::Failed,
                        Some(&error),
                        None,
                        None,
                    )
                    .await
                    {
                        publish_task_alert(&db, org_id, env_id, &task_id, "task:failed", &error)
                            .await;
                    }
                    return Ok(());
                }
                Err(e) => {
                    tracing::warn!(task_id = %task_id, "Failed to check concurrent limit: {}", e);
                }
                _ => {}
            }
        }

        // Check monthly task quotas (hard cap for free plans, informational for paid)
        if let Ok(subscription) = hot::db::subscription::OrgPlan::get_by_org_id(&db, oid).await {
            let is_free = hot::db::subscription::Plan::get_by_id(&db, &subscription.plan_uuid)
                .await
                .map(|plan| plan.is_free_plan())
                .unwrap_or(false);
            let period_start = subscription
                .current_period_start
                .unwrap_or_else(chrono::Utc::now);

            if let Some(usage) = cached_usage_stats(
                &db,
                *oid,
                period_start,
                features.call_retention_days(),
                &usage_stats_cache,
            )
            .await
            {
                // CUS (compute units) per month — hard gate for free plans
                let cus_limit = features.compute_units_per_month();
                if cus_limit > 0 && usage.compute_units >= cus_limit && is_free {
                    let msg = format!(
                        "Monthly compute unit limit reached ({}/{}). Upgrade your plan for more compute.",
                        usage.compute_units, cus_limit
                    );
                    let error = task_failure_json(&msg, None);
                    if complete_task_with_event(
                        &db,
                        &stream_publisher,
                        &task_id,
                        env_id,
                        stream_id,
                        &function_name,
                        &request.task_type,
                        TaskStatus::Failed,
                        Some(&error),
                        None,
                        None,
                    )
                    .await
                    {
                        publish_task_alert(&db, org_id, env_id, &task_id, "task:failed", &error)
                            .await;
                    }
                    return Ok(());
                }

                // Task minutes per month
                let minutes_limit = features.task_minutes_per_month();
                if minutes_limit > 0 {
                    let minutes_used = usage.task_duration_ms / 60_000;
                    if minutes_used >= minutes_limit as i64 && is_free {
                        let msg = format!(
                            "Monthly task minutes exhausted ({}/{}). Upgrade your plan.",
                            minutes_used, minutes_limit
                        );
                        let error = task_failure_json(&msg, None);
                        if complete_task_with_event(
                            &db,
                            &stream_publisher,
                            &task_id,
                            env_id,
                            stream_id,
                            &function_name,
                            &request.task_type,
                            TaskStatus::Failed,
                            Some(&error),
                            None,
                            None,
                        )
                        .await
                        {
                            publish_task_alert(
                                &db,
                                org_id,
                                env_id,
                                &task_id,
                                "task:failed",
                                &error,
                            )
                            .await;
                        }
                        return Ok(());
                    }
                }
            }
        }
    }

    // -- Acquire resource budget --
    let resource_mem = limits.memory_mb + limits.tmp_size_mb;
    let resource_disk = resource_budget::disk_admission_mb(limits.disk_size_mb);
    let infra_retry_backoff_ms = worker_conf
        .get_int_or_default("queue.infra-retry-backoff-ms", 1_000)
        .max(0) as u64;
    let capacity_wait_started = std::time::Instant::now();
    let resource_guard = match budget
        .acquire(
            resource_mem,
            resource_disk,
            std::time::Duration::from_secs(30),
        )
        .await
    {
        Ok(guard) => guard,
        Err(e @ resource_budget::ResourceBudgetError::Timeout { .. }) => {
            tracing::warn!(
                task_id = %task_id,
                requested_memory_mb = resource_mem,
                requested_disk_mb = resource_disk,
                backoff_ms = infra_retry_backoff_ms,
                "Container resources unavailable within admission window; deferring queue message"
            );
            return Err(Box::new(QueueInfrastructureError::new(
                e.to_string(),
                std::time::Duration::from_millis(infra_retry_backoff_ms),
            )));
        }
        Err(e) => {
            let error = task_failure_json(&e.to_string(), None);
            if complete_task_with_event(
                &db,
                &stream_publisher,
                &task_id,
                env_id,
                stream_id,
                &function_name,
                &request.task_type,
                TaskStatus::Failed,
                Some(&error),
                None,
                None,
            )
            .await
            {
                publish_task_alert(&db, org_id, env_id, &task_id, "task:failed", &error).await;
            }
            return Ok(());
        }
    };
    let resource_capacity_wait_ms = capacity_wait_started
        .elapsed()
        .as_millis()
        .min(i64::MAX as u128) as i64;

    // -- Create data volume for /data/ --
    let data_volume = match data_volume::DataVolume::create(
        &data_vol_base,
        &task_id.to_string(),
        limits.disk_size_mb,
    )
    .await
    {
        Ok(vol) => Some(vol),
        Err(e) => {
            tracing::error!(task_id = %task_id, "Data volume creation failed: {}", e);
            let error = task_failure_json(
                &format!("Requested /data volume could not be provisioned: {e}"),
                None,
            );
            if complete_task_with_event(
                &db,
                &stream_publisher,
                &task_id,
                env_id,
                stream_id,
                &function_name,
                &request.task_type,
                TaskStatus::Failed,
                Some(&error),
                None,
                None,
            )
            .await
            {
                publish_task_alert(&db, org_id, env_id, &task_id, "task:failed", &error).await;
            }
            drop(resource_guard);
            return Ok(());
        }
    };

    match claim_task_for_execution(
        &db,
        &task_id,
        &worker_id,
        std::time::Duration::from_millis(
            worker_conf
                .get_int_or_default("queue.infra-retry-backoff-ms", 1_000)
                .max(0) as u64,
        ),
    )
    .await
    {
        Ok(true) => {}
        Ok(false) => {
            // Not ours to run: clean the just-created volume via the bounded
            // helper rather than an implicit (synchronous) Drop.
            if let Some(vol) = data_volume {
                cleanup_data_volume_with_timeout(&task_id, vol).await;
            }
            return Ok(());
        }
        Err(e) => {
            if let Some(vol) = data_volume {
                cleanup_data_volume_with_timeout(&task_id, vol).await;
            }
            return Err(Box::new(e));
        }
    }

    // The user-visible TIMEOUT budget starts once this worker owns the task.
    // Every post-claim setup operation and the container runtime share this
    // one deadline, so storage/file-server preparation cannot pin a running
    // row outside the advertised task budget. Billing deliberately does NOT
    // share this anchor — see `execution_start` below.
    let total_timeout = std::time::Duration::from_millis(timeout_ms);
    let task_deadline = tokio::time::Instant::now() + total_timeout;

    let trace_id = request.task_id.clone();

    // -- Generate run_id early so files written via hotbox are linked to this run --
    let run_id = Uuid::now_v7();
    let user_id = request
        .user_id
        .as_deref()
        .and_then(|s| Uuid::parse_str(s).ok())
        .unwrap_or_else(Uuid::nil);

    let origin_run_id = request
        .origin_run_id
        .as_deref()
        .and_then(|s| Uuid::parse_str(s).ok());

    let durable_setup = async {
        emit_task_started(
            &stream_publisher,
            task_id,
            env_id,
            stream_id,
            &function_name,
            &request.task_type,
        )
        .await;

        if let Err(e) = hot::db::run::Run::ensure_run_exists(
            &db,
            &run_id,
            &env_id,
            &stream_id,
            Some(&build_id),
            hot::db::run::RunType::Task.as_id(),
            origin_run_id.as_ref(),
            &user_id,
            org_id.as_ref(),
        )
        .await
        {
            tracing::warn!(task_id = %task_id, run_id = %run_id, "Failed to ensure container task run exists: {}", e);
        }

        if let Err(e) = Task::set_run_id(&db, &task_id, &run_id).await {
            tracing::warn!(task_id = %task_id, run_id = %run_id, "Failed to set container task run_id: {}", e);
        }
    };
    if await_container_setup(task_deadline, durable_setup)
        .await
        .is_err()
    {
        tracing::error!(task_id = %task_id, "Container task setup timed out while creating its run record");
        finish_container_setup_timeout(
            &db,
            &stream_publisher,
            &task_queue,
            &request,
            &task_id,
            env_id,
            stream_id,
            org_id,
            &function_name,
            &request.task_type,
            &worker_id,
            "durable run setup",
            None,
            data_volume,
            resource_guard,
        )
        .await;
        return Ok(());
    }

    // -- Start per-task file server for hotbox CLI access --
    // For Kata, the file server start is deferred to a pre-start hook
    // because it needs the VM's vsock UDS path (only available after create_task).
    let is_kata = std::cfg_select! {
        all(target_os = "linux", feature = "kata") =>
            matches!(executor.backend(), executor::Backend::Kata),
        _ => false,
    };

    let file_server_handle = if !is_kata {
        match await_container_setup(
            task_deadline,
            start_file_server_for_task(
                &task_id,
                &data_vol_base,
                org_id,
                env_id,
                user_id,
                Some(run_id),
                &db,
                &worker_conf,
                executor.backend(),
            ),
        )
        .await
        {
            Ok(Ok(handle)) => Some(handle),
            Ok(Err(e)) => {
                tracing::error!("File server start failed: {}", e);
                let error = task_failure_json(
                    "File server failed to start — container cannot access hot:// storage",
                    None,
                );
                // Post-claim: this worker owns the row, so fence the terminal
                // write with its id (likewise at the later sites below).
                if complete_task_with_event(
                    &db,
                    &stream_publisher,
                    &task_id,
                    env_id,
                    stream_id,
                    &function_name,
                    &request.task_type,
                    TaskStatus::Failed,
                    Some(&error),
                    None,
                    Some(&worker_id),
                )
                .await
                {
                    publish_task_alert(&db, org_id, env_id, &task_id, "task:failed", &error).await;
                }
                drop(resource_guard);
                if let Some(vol) = data_volume {
                    cleanup_data_volume_with_timeout(&task_id, vol).await;
                }
                return Ok(());
            }
            Err(_) => {
                tracing::error!(task_id = %task_id, "Container task setup timed out while starting file storage/server");
                finish_container_setup_timeout(
                    &db,
                    &stream_publisher,
                    &task_queue,
                    &request,
                    &task_id,
                    env_id,
                    stream_id,
                    org_id,
                    &function_name,
                    &request.task_type,
                    &worker_id,
                    "file-server setup",
                    None,
                    data_volume,
                    resource_guard,
                )
                .await;
                return Ok(());
            }
        }
    } else {
        None
    };

    // Build container extras (bind mounts for hotbox binary + socket)
    #[allow(unused_mut)]
    let mut extras = build_container_extras(
        file_server_handle.as_ref(),
        data_volume.as_ref(),
        executor.backend(),
    );

    // Writable rootfs is on by default; set writable: false to disable
    extras.writable_rootfs = args
        .get("writable")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);

    // Override image entrypoint when specified (e.g. for images with non-shell entrypoints)
    if let Some(ep) = args.get("entrypoint").and_then(|v| v.as_array()) {
        let ep_vec: Vec<String> = ep
            .iter()
            .filter_map(|s| s.as_str().map(String::from))
            .collect();
        if !ep_vec.is_empty() {
            extras.entrypoint = Some(ep_vec);
        }
    }

    // Resource mounts (`mounts: {"/app": "node-app"}`). Each entry binds an
    // extracted bundle resource subtree into the container at the requested
    // path. Currently Docker-only — Kata uses a different OCI mount path
    // and will be wired in a follow-up.
    if let Some(mounts) = args.get("mounts").and_then(|v| v.as_array())
        && !mounts.is_empty()
    {
        let backend = executor.backend();
        if !matches!(backend, executor::Backend::Docker) {
            let error = task_failure_json(
                "container 'mounts' is currently only supported on the Docker backend; \
                 Kata support is in progress",
                None,
            );
            if complete_task_with_event(
                &db,
                &stream_publisher,
                &task_id,
                env_id,
                stream_id,
                &function_name,
                &request.task_type,
                TaskStatus::Failed,
                Some(&error),
                None,
                Some(&worker_id),
            )
            .await
            {
                publish_task_alert(&db, org_id, env_id, &task_id, "task:failed", &error).await;
            }
            drop(resource_guard);
            if let Some(vol) = data_volume {
                cleanup_data_volume_with_timeout(&task_id, vol).await;
            }
            return Ok(());
        }

        // We need the extracted bundle on disk to source the bind mounts
        // from. For container tasks this is the *first* code path that
        // touches the bundle on disk, so do an explicit extraction here
        // (load_bytecode_bundle's path is shared via ensure_bundle_extracted).
        let extract_dir = match org_id {
            Some(oid) => {
                match await_container_setup(
                    task_deadline,
                    ensure_bundle_extracted(&build_id, &oid, &env_id, &worker_conf),
                )
                .await
                {
                    Ok(Ok(p)) => p,
                    Ok(Err(e)) => {
                        let error = task_failure_json(
                            &format!("failed to extract bundle for mounts: {}", e),
                            None,
                        );
                        if complete_task_with_event(
                            &db,
                            &stream_publisher,
                            &task_id,
                            env_id,
                            stream_id,
                            &function_name,
                            &request.task_type,
                            TaskStatus::Failed,
                            Some(&error),
                            None,
                            Some(&worker_id),
                        )
                        .await
                        {
                            publish_task_alert(
                                &db,
                                org_id,
                                env_id,
                                &task_id,
                                "task:failed",
                                &error,
                            )
                            .await;
                        }
                        drop(resource_guard);
                        if let Some(vol) = data_volume {
                            cleanup_data_volume_with_timeout(&task_id, vol).await;
                        }
                        return Ok(());
                    }
                    Err(_) => {
                        tracing::error!(task_id = %task_id, "Container task setup timed out while retrieving/extracting mount bundle");
                        finish_container_setup_timeout(
                            &db,
                            &stream_publisher,
                            &task_queue,
                            &request,
                            &task_id,
                            env_id,
                            stream_id,
                            org_id,
                            &function_name,
                            &request.task_type,
                            &worker_id,
                            "bundle mount preparation",
                            file_server_handle,
                            data_volume,
                            resource_guard,
                        )
                        .await;
                        return Ok(());
                    }
                }
            }
            None => {
                let error = task_failure_json(
                    "container 'mounts' requires an org_id on the task request",
                    None,
                );
                if complete_task_with_event(
                    &db,
                    &stream_publisher,
                    &task_id,
                    env_id,
                    stream_id,
                    &function_name,
                    &request.task_type,
                    TaskStatus::Failed,
                    Some(&error),
                    None,
                    Some(&worker_id),
                )
                .await
                {
                    publish_task_alert(&db, org_id, env_id, &task_id, "task:failed", &error).await;
                }
                drop(resource_guard);
                if let Some(vol) = data_volume {
                    cleanup_data_volume_with_timeout(&task_id, vol).await;
                }
                return Ok(());
            }
        };
        let resources_root = extract_dir.join("resources");

        let mut mount_error: Option<String> = None;
        for m in mounts {
            let container_path = m.get("container_path").and_then(|v| v.as_str());
            let resource_path = m.get("resource_path").and_then(|v| v.as_str());
            let readonly = m.get("readonly").and_then(|v| v.as_bool()).unwrap_or(true);
            let (Some(container_path), Some(resource_path)) = (container_path, resource_path)
            else {
                mount_error = Some(format!("invalid mount spec in args: {}", m));
                break;
            };

            let source = resources_root.join(resource_path);
            // Re-validate at the worker too: defence in depth against a
            // malformed args_json that bypassed the Hot-side parser
            // (e.g. a hand-crafted task insert in the DB).
            let canonical = match source.canonicalize() {
                Ok(p) => p,
                Err(e) => {
                    mount_error = Some(format!(
                        "resource path {:?} not found in bundle ({}). Available roots are under {:?}.",
                        resource_path, e, resources_root
                    ));
                    break;
                }
            };
            let canonical_root = match resources_root.canonicalize() {
                Ok(p) => p,
                Err(e) => {
                    mount_error = Some(format!(
                        "bundle has no resources/ directory ({}); cannot honor mounts",
                        e
                    ));
                    break;
                }
            };
            if !canonical.starts_with(&canonical_root) {
                mount_error = Some(format!(
                    "resource path {:?} escapes the bundle resources/ root",
                    resource_path
                ));
                break;
            }

            let mode = if readonly { "ro" } else { "rw" };
            extras.binds.push(format!(
                "{}:{}:{}",
                canonical.to_string_lossy(),
                container_path,
                mode,
            ));
            tracing::debug!(
                task_id = %task_id,
                container = %container_path,
                resource = %resource_path,
                readonly = readonly,
                "box.mount.bound"
            );
        }

        if let Some(err_msg) = mount_error {
            let error = task_failure_json(&err_msg, None);
            if complete_task_with_event(
                &db,
                &stream_publisher,
                &task_id,
                env_id,
                stream_id,
                &function_name,
                &request.task_type,
                TaskStatus::Failed,
                Some(&error),
                None,
                Some(&worker_id),
            )
            .await
            {
                publish_task_alert(&db, org_id, env_id, &task_id, "task:failed", &error).await;
            }
            drop(resource_guard);
            if let Some(vol) = data_volume {
                cleanup_data_volume_with_timeout(&task_id, vol).await;
            }
            return Ok(());
        }
    }

    // For Kata, prepare the deferred file server hook.
    // The hook runs between create_task (VM ready) and start_task (process begins)
    // so the vsock UDS listener is ready before hotbox connects.
    #[cfg(all(target_os = "linux", feature = "kata"))]
    let (pre_start_hook, kata_fs_rx): (
        Option<executor::PreStartHook>,
        Option<tokio::sync::oneshot::Receiver<file_server::FileServerHandle>>,
    ) = if is_kata {
        let preferred_port = 9200u32 + (task_id.as_u128() & 0xFFFF) as u32;

        // For QEMU (AF_VSOCK): reserve the port now so we know the actual port
        // before creating the container. On collision, picks an alternative.
        let reserved_vsock = if matches!(executor.kata_vmm(), Some(executor::KataVmm::Qemu)) {
            match file_server::reserve_vsock_port(preferred_port) {
                Ok(r) => Some(r),
                Err(e) => {
                    tracing::error!("Failed to reserve vsock port: {}", e);
                    let error = task_failure_json("Failed to reserve vsock port", None);
                    complete_task_with_event(
                        &db,
                        &stream_publisher,
                        &task_id,
                        env_id,
                        stream_id,
                        &function_name,
                        &request.task_type,
                        TaskStatus::Failed,
                        Some(&error),
                        None,
                        Some(&worker_id),
                    )
                    .await;
                    drop(resource_guard);
                    if let Some(vol) = data_volume {
                        cleanup_data_volume_with_timeout(&task_id, vol).await;
                    }
                    return Ok(());
                }
            }
        } else {
            None
        };

        let vsock_port = reserved_vsock.as_ref().map_or(preferred_port, |r| r.port);
        let fs_auth_token = Uuid::new_v4().as_simple().to_string();
        extras.extra_env.push("HOTBOX_TRANSPORT=vsock".to_string());
        extras
            .extra_env
            .push(format!("HOTBOX_VSOCK_PORT={}", vsock_port));
        extras
            .extra_env
            .push(format!("HOTBOX_AUTH_TOKEN={}", fs_auth_token));

        let fs_org_id = match org_id {
            Some(id) => id,
            None => {
                let error = task_failure_json("File server failed to start — no org_id", None);
                complete_task_with_event(
                    &db,
                    &stream_publisher,
                    &task_id,
                    env_id,
                    stream_id,
                    &function_name,
                    &request.task_type,
                    TaskStatus::Failed,
                    Some(&error),
                    None,
                    Some(&worker_id),
                )
                .await;
                drop(resource_guard);
                if let Some(vol) = data_volume {
                    cleanup_data_volume_with_timeout(&task_id, vol).await;
                }
                return Ok(());
            }
        };
        let fs_storage = match await_container_setup(
            task_deadline,
            hot::file_storage::file_storage_from_config(&worker_conf),
        )
        .await
        {
            Ok(Ok(s)) => Arc::from(s),
            Ok(Err(e)) => {
                tracing::error!("File server storage init failed: {}", e);
                let error =
                    task_failure_json("File server failed to start — storage init failed", None);
                complete_task_with_event(
                    &db,
                    &stream_publisher,
                    &task_id,
                    env_id,
                    stream_id,
                    &function_name,
                    &request.task_type,
                    TaskStatus::Failed,
                    Some(&error),
                    None,
                    Some(&worker_id),
                )
                .await;
                drop(resource_guard);
                if let Some(vol) = data_volume {
                    cleanup_data_volume_with_timeout(&task_id, vol).await;
                }
                return Ok(());
            }
            Err(_) => {
                tracing::error!(task_id = %task_id, "Container task setup timed out while initializing Kata file storage");
                finish_container_setup_timeout(
                    &db,
                    &stream_publisher,
                    &task_queue,
                    &request,
                    &task_id,
                    env_id,
                    stream_id,
                    org_id,
                    &function_name,
                    &request.task_type,
                    &worker_id,
                    "Kata file-storage setup",
                    None,
                    data_volume,
                    resource_guard,
                )
                .await;
                return Ok(());
            }
        };
        let fs_ctx = file_server::FileServerContext {
            org_id: fs_org_id,
            env_id,
            user_id,
            run_id: Some(run_id),
            auth_token: fs_auth_token,
            db: Arc::clone(&db),
            storage: fs_storage,
        };

        let (tx, rx) = tokio::sync::oneshot::channel();
        let hook_task_id = task_id;
        let hook: executor::PreStartHook = Box::new(move |vsock_setup: executor::VsockSetup| {
            Box::pin(async move {
                let handle = match vsock_setup {
                    executor::VsockSetup::AfVsock => {
                        let reserved =
                            reserved_vsock.expect("AF_VSOCK setup requires a pre-reserved port");
                        file_server::start_vsock_af(&hook_task_id, reserved, fs_ctx).await
                    }
                    executor::VsockSetup::HybridUds { path } => {
                        let listener_path =
                            std::path::PathBuf::from(format!("{}_{}", path.display(), vsock_port));
                        file_server::start_vsock_uds(
                            &hook_task_id,
                            &listener_path,
                            vsock_port,
                            fs_ctx,
                        )
                        .await
                        .map_err(|e| e.to_string())?
                    }
                };
                let _ = tx.send(handle);
                Ok(())
            })
        });
        (Some(hook), Some(rx))
    } else {
        (None, None)
    };
    #[cfg(not(all(target_os = "linux", feature = "kata")))]
    let pre_start_hook: Option<executor::PreStartHook> = None;

    let command_kind = if args.get("script").is_some() {
        "script"
    } else if args.get("cmd").is_some() {
        "cmd"
    } else {
        "image-default"
    };
    tracing::debug!(
        task_id = %task_id,
        image = %image,
        command_kind,
        size = %limits.size,
        timeout_secs = limits.timeout_secs,
        memory_mb = limits.memory_mb,
        disk_size_mb = limits.disk_size_mb,
        has_data_volume = data_volume.is_some(),
        has_file_server = file_server_handle.is_some() || is_kata,
        network = limits.network,
        backend = %executor.backend(),
        "Starting container command"
    );

    // Billing/duration clock: CUS and the user-visible `duration-ms` measure
    // the workload execution window only. Everything before this point (run
    // record, file server, bundle download/extract for mounts, Kata storage
    // init) is worker-side infrastructure time — it counts against the
    // claim-anchored `task_deadline` above, but must never be billed as user
    // compute or inflate the reported duration.
    let execution_start = std::time::Instant::now();

    let extras_ref = if extras.binds.is_empty()
        && extras.extra_env.is_empty()
        && !extras.writable_rootfs
        && extras.entrypoint.is_none()
        && extras.data_volume_path.is_none()
    {
        None
    } else {
        Some(&extras)
    };

    // Use phased execution for Docker: create, store container_id, poll.
    // Kata still uses atomic execute_with_extras.
    //
    // Every arm snapshots `duration_ms` at the moment execution ends —
    // before log collection and container/VM teardown — so cleanup time
    // (which runs behind its own bounded envelopes, up to
    // KATA_TIMEOUT_CLEANUP_ENVELOPE after a Kata timeout) is never billed
    // as user compute. See `bill_before_cleanup`.
    let is_docker = matches!(executor.backend(), executor::Backend::Docker);
    let (execution_result, duration_ms) = if is_docker {
        let mut timings = executor::ContainerTimings::default();
        match tokio::time::timeout_at(
            task_deadline,
            executor.create_and_start(
                &image,
                cmd,
                env,
                Some(&trace_id),
                Some(&limits),
                extras_ref,
                &mut timings,
            ),
        )
        .await
        {
            Ok(Ok(container_id)) => {
                // Persist container_id so a new worker can adopt it
                match tokio::time::timeout_at(
                    task_deadline.min(tokio::time::Instant::now() + DB_CALL_TIMEOUT),
                    Task::set_container_id(&db, &task_id, &container_id),
                )
                .await
                {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => {
                        tracing::warn!(task_id = %task_id, "Failed to store container_id: {}", e);
                    }
                    Err(_) => {
                        tracing::warn!(task_id = %task_id, "Storing container_id timed out");
                    }
                }
                let workload_execution_started = std::time::Instant::now();

                // Poll-based monitoring loop
                let poll_interval = std::time::Duration::from_secs(2);
                let exit_code = loop {
                    let now = tokio::time::Instant::now();
                    if now >= task_deadline {
                        break None;
                    }
                    tokio::time::sleep(poll_interval.min(task_deadline - now)).await;
                    match tokio::time::timeout_at(
                        task_deadline,
                        executor.inspect_status(&container_id),
                    )
                    .await
                    {
                        Ok(Ok(Some(code))) => break Some(code),
                        Ok(Ok(None)) => {
                            if tokio::time::Instant::now() >= task_deadline {
                                break None; // timed out
                            }
                        }
                        Ok(Err(e)) => {
                            if matches!(e, executor::ExecutorError::ContainerNotFound(_)) {
                                tracing::warn!(
                                    task_id = %task_id,
                                    container_id = %container_id,
                                    "Container disappeared during execution"
                                );
                                break Some(-1);
                            }
                            tracing::warn!(
                                task_id = %task_id,
                                container_id = %container_id,
                                "Poll inspect failed: {}", e
                            );
                            if tokio::time::Instant::now() >= task_deadline {
                                break None;
                            }
                        }
                        Err(_) => break None,
                    }
                };
                timings.execution_ms = workload_execution_started
                    .elapsed()
                    .as_millis()
                    .min(i64::MAX as u128) as i64;

                // Execution has ended (exit observed or wall-clock deadline
                // hit): everything below — log collection, kill/remove — is
                // worker teardown, so snapshot the billable window first.
                let (duration_ms, output) = bill_before_cleanup(execution_start, async {
                    match exit_code {
                        Some(code) => {
                            let logs_start = std::time::Instant::now();
                            let (stdout, stderr) = tokio::time::timeout(
                                CONTAINER_LOGS_TIMEOUT,
                                executor.collect_logs(&container_id),
                            )
                            .await
                            .ok()
                            .and_then(Result::ok)
                            .unwrap_or_default();
                            timings.logs_collect_ms = logs_start.elapsed().as_millis() as i64;
                            remove_container_with_timeout(&executor, &container_id, &task_id).await;

                            executor::ContainerOutput {
                                exit_code: code,
                                stdout,
                                stderr,
                                container_id,
                                timed_out: false,
                                oom_killed: code == 137,
                            }
                        }
                        None => {
                            // Timed out
                            let logs_start = std::time::Instant::now();
                            let (stdout, stderr) = tokio::time::timeout(
                                CONTAINER_LOGS_TIMEOUT,
                                executor.collect_logs(&container_id),
                            )
                            .await
                            .ok()
                            .and_then(Result::ok)
                            .unwrap_or_default();
                            timings.logs_collect_ms = logs_start.elapsed().as_millis() as i64;
                            kill_and_remove_with_timeout(&executor, &container_id, Some(&task_id))
                                .await;

                            executor::ContainerOutput {
                                exit_code: -1,
                                stdout,
                                stderr,
                                container_id,
                                timed_out: true,
                                oom_killed: false,
                            }
                        }
                    }
                })
                .await;

                (Ok(Ok((output, timings))), duration_ms)
            }
            Ok(Err(e)) => (
                Ok(Err((e, timings))),
                billable_execution_ms(execution_start),
            ),
            Err(elapsed) => {
                tracing::warn!(
                    task_id = %task_id,
                    image = %image,
                    timeout_ms,
                    "Docker image pull/create/start exceeded the task wall-clock timeout"
                );
                let (duration_ms, ()) = bill_before_cleanup(execution_start, async {
                    if tokio::time::timeout(
                        CONTAINER_KILL_TIMEOUT,
                        executor.cleanup_task_containers(&trace_id),
                    )
                    .await
                    .is_err()
                    {
                        tracing::error!(
                            task_id = %task_id,
                            "Docker setup-timeout cleanup also timed out; orphan cleanup will retry on restart"
                        );
                    }
                })
                .await;
                (Err(elapsed), duration_ms)
            }
        }
    } else {
        // Kata: use atomic execute_with_extras (phased not supported)
        let result = tokio::time::timeout_at(
            task_deadline,
            executor.execute_with_extras(
                &image,
                cmd,
                env,
                limits.timeout_secs,
                Some(&trace_id),
                Some(&limits),
                extras_ref,
                pre_start_hook,
            ),
        )
        .await;
        // Execution has ended (result returned or the outer timeout fired):
        // determine the billable window before the leaked-VM reaping below,
        // whose bounded envelope (up to KATA_TIMEOUT_CLEANUP_ENVELOPE) is
        // infrastructure teardown, not user compute. Prefer the workload-end
        // instant the executor stamped before its own internal teardown
        // (FIFO/log finalize, kill, VM/snapshot/CNI cleanup): on every path
        // that completed inside the deadline the executor tears down BEFORE
        // returning, so a post-await snapshot alone would bill that teardown.
        // The snapshot below remains the fallback for paths that never stamp
        // it (outer-timeout cancellation, setup failures).
        let workload_ended_at = match &result {
            Ok(Ok((_, timings))) | Ok(Err((_, timings))) => timings.workload_ended_at,
            Err(_) => None,
        };
        let (fallback_ms, ()) = bill_before_cleanup(execution_start, async {
            if result.is_err() {
                // The executor's internal timeout starts counting only once
                // the VM is booted, so this outer wall-clock timeout (which
                // also covers slot wait, image pull, and boot) fires first —
                // dropping the execute future before its own cleanup can run.
                // Reap the leaked VM, snapshot, FIFOs, and netns here.
                tracing::warn!(
                    task_id = %task_id,
                    timeout_ms,
                    "Kata execution cancelled by outer timeout — cleaning up leaked VM state"
                );
                if !bounded_cleanup(
                    KATA_TIMEOUT_CLEANUP_ENVELOPE,
                    executor.cleanup_after_timeout(&trace_id, limits.network),
                )
                .await
                {
                    tracing::error!(
                        task_id = %task_id,
                        timeout_secs = KATA_TIMEOUT_CLEANUP_ENVELOPE.as_secs(),
                        "Kata timeout cleanup also timed out; startup orphan cleanup can only recover ids that still have containerd container records — snapshots/netns past DeleteContainer may leak"
                    );
                }
            }
        })
        .await;
        let duration_ms =
            billable_ms_preferring_executor_window(execution_start, workload_ended_at, fallback_ms);
        (result, duration_ms)
    };

    // Clean up file server (Docker: direct handle; Kata: via oneshot channel).
    // A wedged listener (e.g. blocked on a stuck client connection) must not
    // pin the worker thread, so each shutdown gets its own ceiling.
    if let Some(handle) = file_server_handle
        && tokio::time::timeout(CONTAINER_KILL_TIMEOUT, handle.shutdown())
            .await
            .is_err()
    {
        tracing::warn!(task_id = %task_id, "file_server shutdown timed out");
    }
    #[cfg(all(target_os = "linux", feature = "kata"))]
    if let Some(mut rx) = kata_fs_rx
        && let Ok(handle) = rx.try_recv()
        && tokio::time::timeout(CONTAINER_KILL_TIMEOUT, handle.shutdown())
            .await
            .is_err()
    {
        tracing::warn!(task_id = %task_id, "kata file_server shutdown timed out");
    }

    // Clean up data volume (unmount + remove backing file). A hung loop
    // unmount can pin the task worker, so apply the shared wall-clock cap
    // and detached-drop fallback used by every other cleanup site.
    if let Some(vol) = data_volume {
        cleanup_data_volume_with_timeout(&task_id, vol).await;
    }

    // Release resource budget
    drop(resource_guard);

    match execution_result {
        Ok(Ok((output, timings))) => {
            persist_container_timings(
                &db,
                &task_id,
                queue_timing.claimed_at,
                resource_capacity_wait_ms,
                &timings,
            )
            .await;
            let status = if output.timed_out {
                TaskStatus::TimedOut
            } else if output.exit_code != 0 {
                TaskStatus::Failed
            } else {
                TaskStatus::Completed
            };

            let compute_units = limits.size.compute_units(duration_ms);

            let result_json = if status == TaskStatus::Completed {
                serde_json::json!({
                    "exit-code": output.exit_code,
                    "stdout": output.stdout,
                    "stderr": output.stderr,
                    "duration-ms": duration_ms,
                    "slot-wait-ms": timings.slot_wait_ms,
                    "image-pull-ms": timings.image_pull_ms,
                    "runtime-start-ms": timings.runtime_start_ms,
                    "execution-ms": timings.execution_ms,
                    "logs-collect-ms": timings.logs_collect_ms,
                    "container-id": output.container_id,
                    "backend": executor.backend().to_string(),
                    "size": limits.size.as_str(),
                    "compute-units": compute_units,
                    "cus-multiplier": limits.size.cus_multiplier(),
                })
            } else {
                let msg = if output.timed_out {
                    "Container task timed out".to_string()
                } else if output.oom_killed {
                    format!(
                        "Container killed: out of memory (exit code {}). Try a larger size.",
                        output.exit_code
                    )
                } else if let Some(desc) = executor::describe_exit_code(output.exit_code) {
                    format!("Container {} (exit code {})", desc, output.exit_code)
                } else {
                    format!("Container exited with code {}", output.exit_code)
                };

                let mut err_json = serde_json::json!({
                    "exit-code": output.exit_code,
                    "stdout": output.stdout,
                    "stderr": output.stderr,
                    "duration-ms": duration_ms,
                    "slot-wait-ms": timings.slot_wait_ms,
                    "image-pull-ms": timings.image_pull_ms,
                    "runtime-start-ms": timings.runtime_start_ms,
                    "execution-ms": timings.execution_ms,
                    "logs-collect-ms": timings.logs_collect_ms,
                    "container-id": output.container_id,
                    "backend": executor.backend().to_string(),
                    "size": limits.size.as_str(),
                    "compute-units": compute_units,
                    "cus-multiplier": limits.size.cus_multiplier(),
                });
                if output.oom_killed {
                    err_json["oom-killed"] = serde_json::json!(true);
                }
                if let Some(signal) = executor::describe_exit_code(output.exit_code) {
                    err_json["signal"] = serde_json::json!(signal);
                }

                task_failure_json(&msg, Some(err_json))
            };

            let persisted = complete_task_with_event(
                &db,
                &stream_publisher,
                &task_id,
                env_id,
                stream_id,
                &function_name,
                &request.task_type,
                status.clone(),
                Some(&result_json),
                Some(duration_ms),
                Some(&worker_id),
            )
            .await;

            if persisted && (status == TaskStatus::Failed || status == TaskStatus::TimedOut) {
                publish_task_alert(&db, org_id, env_id, &task_id, "task:failed", &result_json)
                    .await;
            }

            if compute_units > 0 {
                check_cus_thresholds(&db, org_id, env_id, &usage_stats_cache).await;
            }

            tracing::debug!(
                task_id = %task_id,
                image = %image,
                exit_code = output.exit_code,
                timed_out = output.timed_out,
                oom_killed = output.oom_killed,
                backend = %executor.backend(),
                duration_ms,
                "Container task finished"
            );
        }
        Ok(Err((e, timings))) => {
            // Some backend errors can occur after the container has started
            // (for example while waiting for it or collecting its result).
            // Preserve that boundary so the elapsed workload time is not
            // misreported as Waiting.
            persist_container_timings(
                &db,
                &task_id,
                queue_timing.claimed_at,
                resource_capacity_wait_ms,
                &timings,
            )
            .await;
            // Every executor error means the workload itself never ran to
            // completion — the failure is in container infrastructure
            // (pull, create, start, containerd/wait plumbing) — so don't
            // charge CUS and don't surface raw backend errors to the user.
            // (`Start` was previously misclassified as a user failure,
            // charging CUS and leaking containerd internals when Kata
            // setup retries were exhausted.)
            let is_infra_failure = true;
            let compute_units = 0;
            let user_message = match &e {
                ExecutorError::ImageNotAllowed(img) => {
                    format!(
                        "Image '{}' is not allowed by the container image policy",
                        img
                    )
                }
                ExecutorError::SlotTimeout(secs) => {
                    format!("Timed out waiting for execution slot ({}s)", secs)
                }
                ExecutorError::ImagePull(_) => "Failed to pull container image".to_string(),
                ExecutorError::Start(_) => "Failed to start container".to_string(),
                _ => "Container infrastructure error".to_string(),
            };
            let error = task_failure_json(
                &user_message,
                Some(serde_json::json!({
                    "duration-ms": duration_ms,
                    "slot-wait-ms": timings.slot_wait_ms,
                    "image-pull-ms": timings.image_pull_ms,
                    "runtime-start-ms": timings.runtime_start_ms,
                    "execution-ms": timings.execution_ms,
                    "logs-collect-ms": timings.logs_collect_ms,
                    "size": limits.size.as_str(),
                    "compute-units": compute_units,
                    "infra-failure": is_infra_failure,
                })),
            );
            let persisted = complete_task_with_event(
                &db,
                &stream_publisher,
                &task_id,
                env_id,
                stream_id,
                &function_name,
                &request.task_type,
                TaskStatus::Failed,
                Some(&error),
                Some(duration_ms),
                Some(&worker_id),
            )
            .await;
            if persisted {
                publish_task_alert(&db, org_id, env_id, &task_id, "task:failed", &error).await;
            }
            if compute_units > 0 {
                check_cus_thresholds(&db, org_id, env_id, &usage_stats_cache).await;
            }
            if persisted {
                maybe_retry_task(&db, &task_queue, &task_id, &request).await;
            }
            tracing::error!(
                task_id = %task_id,
                image = %image,
                backend = %executor.backend(),
                "Container task failed: {}", e
            );
        }
        Err(_) => {
            let compute_units = limits.size.compute_units(duration_ms);
            let error = task_failure_json(
                "Container task timed out",
                Some(serde_json::json!({
                    "duration-ms": duration_ms,
                    "backend": executor.backend().to_string(),
                    "size": limits.size.as_str(),
                    "compute-units": compute_units,
                })),
            );
            let persisted = complete_task_with_event(
                &db,
                &stream_publisher,
                &task_id,
                env_id,
                stream_id,
                &function_name,
                &request.task_type,
                TaskStatus::TimedOut,
                Some(&error),
                Some(duration_ms),
                Some(&worker_id),
            )
            .await;
            if persisted {
                publish_task_alert(&db, org_id, env_id, &task_id, "task:failed", &error).await;
            }
            if compute_units > 0 {
                check_cus_thresholds(&db, org_id, env_id, &usage_stats_cache).await;
            }
            if persisted {
                maybe_retry_task(&db, &task_queue, &task_id, &request).await;
            }
            tracing::warn!(
                task_id = %task_id,
                image = %image,
                timeout_ms,
                backend = %executor.backend(),
                "Container task timed out"
            );
        }
    }

    Ok(())
}

/// Elapsed billable milliseconds for a container task, measured from the
/// point the worker actually dispatched the workload (post-setup) — never
/// from claim. The task DEADLINE is anchored at claim so setup and runtime
/// share one wall-clock budget, but billing CUS = ceil(duration ×
/// multiplier) and the user-visible `duration-ms` must only cover the
/// execution window: worker-side setup (bundle download/extract,
/// file-server start) is infrastructure time, not user compute.
///
/// The same boundary applies on the way out: snapshot at the moment
/// execution ends (result returned / timeout fired / error surfaced),
/// never after teardown — see `bill_before_cleanup`.
fn billable_execution_ms(execution_start: std::time::Instant) -> i64 {
    billable_execution_ms_at(execution_start, std::time::Instant::now())
}

/// Billable window ending at an explicit instant instead of "now". Used
/// when the executor itself reports when the workload finished (see
/// `ContainerTimings::workload_ended_at`), which is always at or before
/// the caller's own post-await snapshot.
fn billable_execution_ms_at(
    execution_start: std::time::Instant,
    workload_end: std::time::Instant,
) -> i64 {
    workload_end
        .saturating_duration_since(execution_start)
        .as_millis()
        .min(i64::MAX as u128) as i64
}

/// Billable window for the atomic (Kata) arm: prefer the workload-end
/// instant the executor stamped BEFORE its internal teardown (FIFO/log
/// finalize, kill, VM/snapshot/netns cleanup) — a post-await snapshot alone
/// would bill that teardown as user compute. `fallback_ms` is the caller's
/// own pre-cleanup snapshot, used for paths that never stamp the instant
/// (outer-timeout cancellation, setup failures).
fn billable_ms_preferring_executor_window(
    execution_start: std::time::Instant,
    workload_ended_at: Option<std::time::Instant>,
    fallback_ms: i64,
) -> i64 {
    workload_ended_at.map_or(fallback_ms, |ended_at| {
        billable_execution_ms_at(execution_start, ended_at)
    })
}

/// Snapshot the billable execution window at the moment execution ends,
/// THEN run `cleanup`. Teardown — log collection, container kill/remove,
/// leaked-VM reaping — runs behind its own bounded envelopes (up to
/// `KATA_TIMEOUT_CLEANUP_ENVELOPE` after a Kata timeout) and is worker
/// infrastructure time: letting it precede the snapshot would inflate both
/// the CUS charge and the user-visible `duration-ms`.
async fn bill_before_cleanup<F>(execution_start: std::time::Instant, cleanup: F) -> (i64, F::Output)
where
    F: std::future::Future,
{
    let duration_ms = billable_execution_ms(execution_start);
    (duration_ms, cleanup.await)
}

/// Persist the point where user workload code actually begins, plus the
/// non-overlapping Waiting subphases that precede it.
#[allow(clippy::too_many_arguments)]
async fn record_task_workload_start(
    db: &DatabasePool,
    task_id: &Uuid,
    claimed_at: chrono::DateTime<chrono::Utc>,
    workload_started_at: chrono::DateTime<chrono::Utc>,
    capacity_wait_ms: i64,
    image_pull_ms: i64,
    runtime_start_ms: i64,
) {
    let claimed_to_workload_ms = workload_started_at
        .signed_duration_since(claimed_at)
        .num_milliseconds()
        .max(0);
    let worker_preparation_ms = claimed_to_workload_ms
        .saturating_sub(capacity_wait_ms.max(0))
        .saturating_sub(image_pull_ms.max(0))
        .saturating_sub(runtime_start_ms.max(0));
    let patch = serde_json::json!({
        "workload_started_at": workload_started_at.to_rfc3339(),
        "capacity_wait_ms": capacity_wait_ms.max(0),
        "image_pull_ms": image_pull_ms.max(0),
        "runtime_start_ms": runtime_start_ms.max(0),
        "worker_preparation_ms": worker_preparation_ms,
    });
    match tokio::time::timeout(
        TASK_TIMING_DB_TIMEOUT,
        Task::merge_timing(db, task_id, &patch),
    )
    .await
    {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            tracing::warn!(task_id = %task_id, "Failed to persist task workload timing: {}", e);
        }
        Err(_) => {
            tracing::warn!(task_id = %task_id, "Task workload timing write timed out; continuing");
        }
    }
}

async fn persist_container_timings(
    db: &DatabasePool,
    task_id: &Uuid,
    claimed_at: chrono::DateTime<chrono::Utc>,
    resource_capacity_wait_ms: i64,
    timings: &executor::ContainerTimings,
) {
    let Some(workload_started_at) = timings.workload_started_at else {
        return;
    };
    let capacity_wait_ms = resource_capacity_wait_ms.saturating_add(timings.slot_wait_ms.max(0));
    record_task_workload_start(
        db,
        task_id,
        claimed_at,
        workload_started_at,
        capacity_wait_ms,
        timings.image_pull_ms,
        timings.runtime_start_ms,
    )
    .await;
    match tokio::time::timeout(
        TASK_TIMING_DB_TIMEOUT,
        Task::merge_timing(
            db,
            task_id,
            &serde_json::json!({
                "workload_execution_ms": timings.execution_ms.max(0),
                "logs_collect_ms": timings.logs_collect_ms.max(0),
            }),
        ),
    )
    .await
    {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            tracing::warn!(task_id = %task_id, "Failed to persist task execution timing: {}", e);
        }
        Err(_) => {
            tracing::warn!(task_id = %task_id, "Task execution timing write timed out; continuing");
        }
    }
}

async fn finalize_task_timing(db: &DatabasePool, task_id: &Uuid) {
    let task = match tokio::time::timeout(DB_CALL_TIMEOUT, Task::get(db, task_id)).await {
        Ok(Ok(task)) => task,
        Ok(Err(e)) => {
            tracing::warn!(task_id = %task_id, "Failed to load task for timing finalization: {}", e);
            return;
        }
        Err(_) => {
            tracing::warn!(
                task_id = %task_id,
                timeout_secs = DB_CALL_TIMEOUT.as_secs(),
                "Task timing finalization load timed out"
            );
            return;
        }
    };
    let Some(stop_time) = task.stop_time else {
        return;
    };
    let workload_started_at = task
        .timing
        .as_ref()
        .and_then(|timing| timing.get("workload_started_at"))
        .and_then(serde_json::Value::as_str)
        .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
        .map(|value| value.with_timezone(&chrono::Utc));
    let (waiting_ms, execution_ms) = if let Some(workload_started_at) = workload_started_at {
        (
            workload_started_at
                .signed_duration_since(task.created_at)
                .num_milliseconds()
                .max(0),
            stop_time
                .signed_duration_since(workload_started_at)
                .num_milliseconds()
                .max(0),
        )
    } else {
        (
            stop_time
                .signed_duration_since(task.created_at)
                .num_milliseconds()
                .max(0),
            0,
        )
    };
    let total_ms = stop_time
        .signed_duration_since(task.created_at)
        .num_milliseconds()
        .max(0);
    let mut patch = serde_json::json!({
        "completed_at": stop_time.to_rfc3339(),
        "waiting_ms": waiting_ms,
        "execution_ms": execution_ms,
        "total_ms": total_ms,
    });
    if workload_started_at.is_some()
        && task
            .timing
            .as_ref()
            .and_then(|timing| timing.get("workload_execution_ms"))
            .is_none()
    {
        patch["workload_execution_ms"] = serde_json::json!(execution_ms);
    }
    match tokio::time::timeout(DB_CALL_TIMEOUT, Task::merge_timing(db, task_id, &patch)).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            tracing::warn!(task_id = %task_id, "Failed to finalize task timing: {}", e);
        }
        Err(_) => {
            tracing::warn!(
                task_id = %task_id,
                timeout_secs = DB_CALL_TIMEOUT.as_secs(),
                "Task timing finalization write timed out"
            );
        }
    }
}

/// Execute a Hot code task (task_type == "code" or default).
#[allow(clippy::too_many_arguments)]
async fn process_code_task(
    request: TaskRequest,
    task_id: Uuid,
    stream_id: Uuid,
    env_id: Uuid,
    build_id: Uuid,
    timeout_ms: u64,
    db: Arc<DatabasePool>,
    task_queue: Arc<ProcessingQueue<TaskRequest>>,
    stream_publisher: Arc<StreamPubSub>,
    bytecode_cache: Arc<BytecodeCache>,
    worker_conf: Val,
    event_publisher: Option<Arc<dyn EventPublisher>>,
    coordinator: Arc<shutdown::TaskShutdownCoordinator>,
    claimed_at: chrono::DateTime<chrono::Utc>,
    capacity_wait_ms: i64,
    worker_id: String,
    blocking_execution_permit: tokio::sync::OwnedSemaphorePermit,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let function_name = request.function_name.clone();
    let task_type = request.task_type.clone();
    let org_id = request
        .org_id
        .as_deref()
        .and_then(|s| Uuid::parse_str(s).ok());

    let cached =
        match load_bytecode(&build_id, &bytecode_cache, Some(&db), Some(&worker_conf)).await {
            Ok(c) => c,
            Err(e) => {
                let error = task_failure_json(&format!("Build load failed: {}", e), None);
                // Code tasks are claimed before this function runs, so every
                // terminal write below is fenced with this worker's id.
                if complete_task_with_event(
                    &db,
                    &stream_publisher,
                    &task_id,
                    env_id,
                    stream_id,
                    &function_name,
                    &task_type,
                    TaskStatus::Failed,
                    Some(&error),
                    None,
                    Some(&worker_id),
                )
                .await
                {
                    publish_task_alert(&db, org_id, env_id, &task_id, "task:failed", &error).await;
                }
                return Err(e.into());
            }
        };

    // Set up task receive channel (for ::hot::task/receive)
    let (inbox_tx, inbox_rx) = mpsc::channel::<Val>(256);
    let inbox_rx = Arc::new(parking_lot::Mutex::new(inbox_rx));

    // Cancel signal — notified when a $cancel message arrives
    let cancel_notify = Arc::new(tokio::sync::Notify::new());

    // Subscribe to inbound task messages via pub/sub and forward to the inbox channel
    let inbox_tx_clone = inbox_tx.clone();
    let stream_pub_clone = Arc::clone(&stream_publisher);
    let task_id_for_sub = task_id;
    let cancel_notify_fwd = Arc::clone(&cancel_notify);
    let inbox_forwarder = tokio::spawn(async move {
        match stream_pub_clone.subscribe(task_id_for_sub).await {
            Ok(mut sub) => loop {
                match sub.next().await {
                    StreamNext::Event(StreamEvent::TaskMessage { payload, .. }) => {
                        let is_cancel = payload
                            .as_object()
                            .and_then(|m| m.get("$cancel"))
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false);

                        let val: Val = serde_json::from_value(payload).unwrap_or(Val::Null);
                        if inbox_tx_clone.send(val).await.is_err() {
                            break;
                        }

                        if is_cancel {
                            cancel_notify_fwd.notify_one();
                            break;
                        }
                    }
                    StreamNext::Event(_) | StreamNext::Idle => {}
                    StreamNext::Closed => break,
                }
            },
            Err(e) => {
                tracing::warn!(task_id = %task_id_for_sub, "Failed to subscribe for task messages: {}", e);
            }
        }
    });

    // Build execution context
    let run_id = Uuid::now_v7();
    let user_id = request
        .user_id
        .as_deref()
        .and_then(|s| Uuid::parse_str(s).ok());

    let execution_context = ExecutionContext {
        env_id: Some(env_id),
        env_name: None,
        user_id,
        org_id,
        org_slug: None,
        run_id,
        stream_id,
        run_type_id: hot::db::run::RunType::Task.as_id(),
        build_id: Some(build_id),
        build_hash: None,
        project_id: request
            .project_id
            .as_deref()
            .and_then(|s| Uuid::parse_str(s).ok()),
        project_name: request.project_name.clone(),
        event_id: None,
        origin_run_id: request
            .origin_run_id
            .as_deref()
            .and_then(|s| Uuid::parse_str(s).ok()),
        retry_attempt: 0,
        secret_keys: Default::default(),
        secret_value_hashes: Default::default(),
        access_id: None,
        agent_type: None,
        queue_timing: None,
        deadline_at: chrono::Duration::from_std(std::time::Duration::from_millis(timeout_ms))
            .ok()
            .map(|duration| chrono::Utc::now() + duration),
    };

    let origin_run_id = execution_context.origin_run_id;

    let emitter: Option<Arc<dyn EngineEventEmitter>> = create_emitter(&db);

    // Convert JSON args -> Val
    let args_val: Val = serde_json::from_value(request.args.clone()).unwrap_or(Val::Null);

    let db_exec = Arc::clone(&db);
    let sp_exec = Arc::clone(&stream_publisher);
    let fn_name_exec = function_name.clone();
    let conf_exec = worker_conf.clone();
    let vm_cancel_token = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let vm_cancel_for_task = Arc::clone(&vm_cancel_token);

    // Register the cancel token so the shutdown coordinator can signal this VM
    coordinator.set_cancel_token(&task_id, Arc::clone(&vm_cancel_token));

    let store: Option<Arc<dyn hot::store::Store>> = match hot::store::store_from_config_with_db(
        &worker_conf,
        Some(Arc::clone(&db)),
        org_id,
        Some(env_id),
    )
    .await
    {
        Ok(s) => Some(Arc::from(s)),
        Err(e) => {
            tracing::warn!(task_id = %task_id, "Store not available for task: {}", e);
            None
        }
    };
    let embedding_provider: Option<Arc<dyn hot::store::embedding::EmbeddingProvider>> =
        hot::store::embedding::embedding_provider_from_config(&worker_conf).map(Arc::from);

    let file_storage: Option<Arc<dyn hot::file_storage::FileStorage>> =
        match hot::file_storage::file_storage_from_config(&worker_conf).await {
            Ok(s) => Some(Arc::from(s)),
            Err(e) => {
                tracing::warn!(task_id = %task_id, "File storage not available for task: {}", e);
                None
            }
        };

    let panic_label = format!("task_worker:{}:{}", fn_name_exec, task_id);
    let resource_registry_for_task = hot::lang::hot::resource::get_build_registry(&build_id);
    let workload_started_at = chrono::Utc::now();
    let task_handle = tokio::task::spawn_blocking(move || {
        // Hold the blocking-execution slot for the lifetime of the THREAD,
        // not the JoinHandle: when the timeout arm drops the handle, this
        // detached thread keeps consuming a permit until the closure returns,
        // so admission backpressure reflects wedged VM threads.
        let _blocking_execution_permit = blocking_execution_permit;
        // Scope this task's view of `::hot::resource/*` to the bundle that
        // produced its bytecode. The guard installs the per-build registry
        // as a thread-local before user code runs and restores the prior
        // value (typically `None`) on drop, so panics still leave the
        // thread in a clean state. When `None` (live builds, missing
        // manifest, dev mode), the global registry stays in effect.
        let _resource_guard =
            resource_registry_for_task.map(hot::lang::hot::resource::ThreadRegistryGuard::install);
        // Wrap user-code execution in run_user_code so any panic from the
        // user's Hot code becomes a structured UserCodePanic that we surface
        // as a typed task failure (with location, thread, optional backtrace)
        // instead of a generic "Task panicked" string. spawn_blocking still
        // catches panics outside this boundary as defense-in-depth.
        match hot::lang::user_code::run_user_code(&panic_label, || {
            hot::lang::engine::Engine::call_function_with_cached_bytecode_and_task(
                &fn_name_exec,
                std::slice::from_ref(&args_val),
                cached,
                Some(&conf_exec),
                emitter,
                Some(execution_context),
                event_publisher,
                None,
                Some(db_exec),
                Some(sp_exec),
                Some(inbox_rx),
                None,
                file_storage,
                store,
                embedding_provider,
                Some(vm_cancel_for_task),
                Some(task_id),
            )
        }) {
            Ok(result) => result,
            Err(panic) => {
                tracing::error!(
                    target: "hot::panic",
                    task_id = %task_id,
                    location = panic.location.as_deref().unwrap_or("<unknown>"),
                    thread = %panic.thread,
                    "user code panicked in task: {}",
                    panic.message,
                );
                // Render panic as a structured Hot Failure value so downstream
                // code can attach `panic: true`, location, thread, etc. via
                // normalize_val_to_task_failure.
                Ok(panic.to_failure_val())
            }
        }
    });
    record_task_workload_start(
        &db,
        &task_id,
        claimed_at,
        workload_started_at,
        capacity_wait_ms,
        0,
        0,
    )
    .await;

    let timeout_dur = std::time::Duration::from_millis(timeout_ms);
    let execution_result = tokio::select! {
        result = tokio::time::timeout(timeout_dur, task_handle) => result,
        _ = cancel_notify.notified() => {
            tracing::info!(task_id = %task_id, "Task cancelled via $cancel message — signalling VM");
            vm_cancel_token.store(true, std::sync::atomic::Ordering::Relaxed);
            let cancellation = task_cancellation_json("Task cancelled via $cancel message", None);
            if let Err(e) = hot::db::run::Run::ensure_run_exists(
                &db, &run_id, &env_id, &stream_id, Some(&build_id),
                hot::db::run::RunType::Task.as_id(), origin_run_id.as_ref(),
                &user_id.unwrap_or(Uuid::nil()), None,
            ).await {
                tracing::warn!(task_id = %task_id, run_id = %run_id, "Failed to ensure task run exists: {}", e);
            }
            if let Err(e) = hot::db::Task::set_run_id(&db, &task_id, &run_id).await {
                tracing::warn!(task_id = %task_id, run_id = %run_id, "Failed to set task run_id: {}", e);
            }
            if complete_task_with_event(&db, &stream_publisher, &task_id, env_id, stream_id, &function_name, &task_type, TaskStatus::Cancelled, Some(&cancellation), None, Some(&worker_id)).await {
                publish_task_alert(&db, org_id, env_id, &task_id, "task:cancelled", &cancellation).await;
            }
            drop(inbox_tx);
            inbox_forwarder.abort();
            return Ok(());
        }
    };

    drop(inbox_tx);
    inbox_forwarder.abort();

    // Ensure the task's execution run row exists before linking.
    // The DatabaseWriter is async, so run:start may not be committed yet.
    if let Err(e) = hot::db::run::Run::ensure_run_exists(
        &db,
        &run_id,
        &env_id,
        &stream_id,
        Some(&build_id),
        hot::db::run::RunType::Task.as_id(),
        origin_run_id.as_ref(),
        &user_id.unwrap_or(Uuid::nil()),
        None,
    )
    .await
    {
        tracing::warn!(task_id = %task_id, run_id = %run_id, "Failed to ensure task run exists: {}", e);
    }

    // Link the task record to its execution run
    if let Err(e) = hot::db::Task::set_run_id(&db, &task_id, &run_id).await {
        tracing::warn!(task_id = %task_id, run_id = %run_id, "Failed to set task run_id: {}", e);
    }

    match execution_result {
        Ok(Ok(Ok(result_val))) => {
            if let Some((status, result_json, alert_name)) =
                classify_task_terminal_result(&result_val)
            {
                if complete_task_with_event(
                    &db,
                    &stream_publisher,
                    &task_id,
                    env_id,
                    stream_id,
                    &function_name,
                    &task_type,
                    status.clone(),
                    Some(&result_json),
                    None,
                    Some(&worker_id),
                )
                .await
                {
                    publish_task_alert(&db, org_id, env_id, &task_id, alert_name, &result_json)
                        .await;
                    if status == TaskStatus::Failed {
                        maybe_retry_task(&db, &task_queue, &task_id, &request).await;
                    }
                }
                tracing::info!(
                    task_id = %task_id,
                    status = status.as_str(),
                    "Task finished with terminal result"
                );
            } else {
                let result_json = serde_json::to_value(result_val.to_hot_data_repr())
                    .unwrap_or(serde_json::Value::Null);
                complete_task_with_event(
                    &db,
                    &stream_publisher,
                    &task_id,
                    env_id,
                    stream_id,
                    &function_name,
                    &task_type,
                    TaskStatus::Completed,
                    Some(&result_json),
                    None,
                    Some(&worker_id),
                )
                .await;
                tracing::info!(task_id = %task_id, status = "completed", "Task finished");
            }
        }
        Ok(Ok(Err(e))) => {
            let error = task_failure_json(&e, None);
            if complete_task_with_event(
                &db,
                &stream_publisher,
                &task_id,
                env_id,
                stream_id,
                &function_name,
                &task_type,
                TaskStatus::Failed,
                Some(&error),
                None,
                Some(&worker_id),
            )
            .await
            {
                publish_task_alert(&db, org_id, env_id, &task_id, "task:failed", &error).await;
                maybe_retry_task(&db, &task_queue, &task_id, &request).await;
            }
            tracing::error!(task_id = %task_id, "Task execution error: {}", e);
        }
        Ok(Err(e)) => {
            let error = task_failure_json(&format!("Task panicked: {}", e), None);
            if complete_task_with_event(
                &db,
                &stream_publisher,
                &task_id,
                env_id,
                stream_id,
                &function_name,
                &task_type,
                TaskStatus::Failed,
                Some(&error),
                None,
                Some(&worker_id),
            )
            .await
            {
                publish_task_alert(&db, org_id, env_id, &task_id, "task:failed", &error).await;
                maybe_retry_task(&db, &task_queue, &task_id, &request).await;
            }
            tracing::error!(task_id = %task_id, "Task panicked: {}", e);
        }
        Err(_) => {
            // Signal the VM to exit at its next cooperative cancellation point.
            // `tokio::time::timeout` only drops the JoinHandle future; the
            // underlying `spawn_blocking` thread keeps running until the
            // closure returns. The VM polls this token between bytecode ops,
            // so for any cooperative task this is enough to free the blocking
            // thread shortly after the timeout fires. A non-cooperative task
            // (native blocking IO, infinite tight loop) will still leak the
            // thread — see `kill_orphan_thread_after_timeout` follow-up.
            vm_cancel_token.store(true, std::sync::atomic::Ordering::Relaxed);

            let error = task_failure_json("Task timed out", None);
            // Fenced with this worker's id: if the zombie reaper (or any other
            // actor) already made the row terminal, this write affects zero
            // rows, `complete_task_with_event` reports not-persisted, and the
            // alert + retry below are suppressed — the winning writer already
            // published its own event and retry.
            if complete_task_with_event(
                &db,
                &stream_publisher,
                &task_id,
                env_id,
                stream_id,
                &function_name,
                &task_type,
                TaskStatus::TimedOut,
                Some(&error),
                None,
                Some(&worker_id),
            )
            .await
            {
                publish_task_alert(&db, org_id, env_id, &task_id, "task:failed", &error).await;
                maybe_retry_task(&db, &task_queue, &task_id, &request).await;
            }
            tracing::warn!(task_id = %task_id, timeout_ms, "Task timed out — VM cancel signalled");
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Task event emission, alert publishing, and error normalization helpers
// ---------------------------------------------------------------------------

/// Build a `::hot::task/Failure` typed JSON value.
fn task_failure_json(msg: &str, err: Option<serde_json::Value>) -> serde_json::Value {
    serde_json::json!({
        "$type": "::hot::task/Failure",
        "$val": {
            "msg": msg,
            "err": err.unwrap_or(serde_json::Value::Null)
        }
    })
}

/// Build a `::hot::task/Cancellation` typed JSON value.
fn task_cancellation_json(msg: &str, data: Option<serde_json::Value>) -> serde_json::Value {
    serde_json::json!({
        "$type": "::hot::task/Cancellation",
        "$val": {
            "msg": msg,
            "data": data.unwrap_or(serde_json::Value::Null)
        }
    })
}

/// If the VM result is already a typed Failure/Cancellation, keep it as-is.
/// Otherwise wrap it in `::hot::task/Failure`.
fn normalize_val_to_task_failure(val: &Val) -> serde_json::Value {
    let json = serde_json::to_value(val.to_hot_data_repr()).unwrap_or(serde_json::Value::Null);

    // Already a typed value (::hot::run/Failure, ::hot::task/Failure, etc.) — pass through
    if json.get("$type").and_then(|t| t.as_str()).is_some() {
        return json;
    }

    // Wrap bare error value
    let msg = json.as_str().unwrap_or("Task failed").to_string();
    task_failure_json(&msg, Some(json))
}

fn typed_val_name(val: &Val) -> Option<&str> {
    if let Val::Map(map) = val
        && let Some(Val::Str(type_name)) = map.get(&Val::from("$type"))
    {
        return Some(type_name.as_ref());
    }
    None
}

fn is_failure_val(val: &Val) -> bool {
    typed_val_name(val).is_some_and(|name| {
        name == "::hot::run/Failure" || name == "::hot::task/Failure" || name.ends_with("/Failure")
    })
}

fn classify_task_terminal_result(
    result_val: &Val,
) -> Option<(TaskStatus, serde_json::Value, &'static str)> {
    let payload = result_val.unwrap_err().unwrap_or(result_val);

    if payload.is_cancelled() {
        let json =
            serde_json::to_value(payload.to_hot_data_repr()).unwrap_or(serde_json::Value::Null);
        return Some((TaskStatus::Cancelled, json, "task:cancelled"));
    }

    if result_val.is_err() || is_failure_val(payload) {
        return Some((
            TaskStatus::Failed,
            normalize_val_to_task_failure(payload),
            "task:failed",
        ));
    }

    None
}

/// Emit a `task:started` env event via pub/sub.
async fn emit_task_started(
    publisher: &StreamPubSub,
    task_id: Uuid,
    env_id: Uuid,
    stream_id: Uuid,
    function_name: &str,
    task_type: &str,
) {
    let event = EnvEvent::TaskStarted {
        task_id,
        env_id,
        stream_id,
        function_name: function_name.to_string(),
        task_type: task_type.to_string(),
    };
    if let Err(e) = publisher.publish_env(event).await {
        tracing::warn!(task_id = %task_id, "Failed to publish task:started event: {}", e);
    }
}

/// Compare two JSON trees for semantic equality, treating numbers as equal
/// whenever they denote the same numeric value regardless of serde_json's
/// internal variant (e.g. `1e16` parsed as `Number::Float` vs
/// `10000000000000000` parsed as `Number::PosInt`). Postgres jsonb
/// normalizes numeric text on storage, so a payload we wrote can round-trip
/// with a different `Number` variant than the one we hold in memory;
/// variant-sensitive `PartialEq` then yields a false negative, and the
/// payload-authorship check below would wrongly suppress a real completion
/// event. Integer-vs-integer comparisons stay exact (no precision loss for
/// values beyond 2^53); the f64 path only decides mixed-variant cases.
fn json_numeric_tolerant_eq(a: &serde_json::Value, b: &serde_json::Value) -> bool {
    use serde_json::Value;
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => {
            if x == y {
                return true;
            }
            if let (Some(xi), Some(yi)) = (x.as_i64(), y.as_i64()) {
                return xi == yi;
            }
            if let (Some(xu), Some(yu)) = (x.as_u64(), y.as_u64()) {
                return xu == yu;
            }
            match (x.as_f64(), y.as_f64()) {
                (Some(xf), Some(yf)) => xf == yf,
                _ => false,
            }
        }
        (Value::Array(xs), Value::Array(ys)) => {
            xs.len() == ys.len()
                && xs
                    .iter()
                    .zip(ys)
                    .all(|(x, y)| json_numeric_tolerant_eq(x, y))
        }
        (Value::Object(xm), Value::Object(ym)) => {
            xm.len() == ym.len()
                && xm
                    .iter()
                    .all(|(k, v)| ym.get(k).is_some_and(|w| json_numeric_tolerant_eq(v, w)))
        }
        _ => a == b,
    }
}

/// `duration_ms_override` is threaded verbatim to [`Task::complete`]:
/// container tasks pass their billable execution-window snapshot so the
/// row's `duration_ms` doesn't degrade to the claim-to-persist stop-start
/// span (which would fold setup and teardown into billing reads); all other
/// callers pass `None` for the historical computed duration.
#[allow(clippy::too_many_arguments)]
async fn persist_terminal_task(
    db: &DatabasePool,
    task_id: &Uuid,
    status: &TaskStatus,
    result: Option<&serde_json::Value>,
    duration_ms_override: Option<i64>,
    fence_worker: Option<&str>,
    attempt_timeout: std::time::Duration,
    max_attempts: usize,
) -> bool {
    let mut persisted = false;
    for attempt in 1..=max_attempts {
        match tokio::time::timeout(
            attempt_timeout,
            Task::complete(
                db,
                task_id,
                status,
                result,
                duration_ms_override,
                fence_worker,
            ),
        )
        .await
        {
            Ok(Ok(true)) => {
                persisted = true;
                break;
            }
            Ok(Ok(false)) => {
                // A zero-row update means another actor won the terminal-state
                // race (most commonly cancellation), or — when fenced — that
                // this worker no longer owns the row. Treat a replay of our
                // own earlier write as idempotent success, but never publish
                // our completion for a write somebody else made.
                match tokio::time::timeout(DB_CALL_TIMEOUT, Task::get(db, task_id)).await {
                    Ok(Ok(task)) if task.task_status_id == status.as_id() => {
                        // A pre-existing Cancelled row is never a competing
                        // completion. The only writers of `cancelled` are
                        // (1) the user-initiated `::hot::task/cancel` flow
                        // (`Task::cancel`), which flips the row — leaving a
                        // NULL result — BEFORE publishing the `$cancel`
                        // message and never emits completion events itself,
                        // and (2) this worker's own cancellation paths. The
                        // worker owns event emission for cooperative
                        // cancellation, so finding the row already
                        // `cancelled` while persisting `cancelled` is
                        // idempotent success; comparing our cancellation
                        // payload against the row's NULL result would
                        // wrongly suppress task:complete / RunStop /
                        // task:cancelled for every cooperative cancel.
                        if *status == TaskStatus::Cancelled {
                            persisted = true;
                            break;
                        }
                        // A matching terminal status is NOT proof our write
                        // landed: the zombie reaper also writes `failed`, and
                        // neither `Task::complete` nor the reaper touches
                        // `worker_id`, so ownership cannot tell the writers
                        // apart. The stored result payload can: each writer
                        // persists its own payload (the reaper its
                        // "interrupted by worker crash" failure), so only a
                        // payload equal to ours is evidence that an earlier
                        // attempt of OURS committed. Equality must be
                        // numeric-tolerant: Postgres jsonb normalizes number
                        // text, so the stored tree can differ from ours in
                        // serde_json Number variant while denoting the same
                        // payload.
                        let payload_is_ours = match (task.result.as_ref(), result) {
                            (Some(stored), Some(ours)) => json_numeric_tolerant_eq(stored, ours),
                            (None, None) => true,
                            _ => false,
                        };
                        if payload_is_ours {
                            persisted = true;
                            break;
                        }
                        tracing::info!(
                            task_id = %task_id,
                            requested_status = status.as_id(),
                            "Terminal task transition lost a race to a same-status write by another actor; suppressing duplicate completion"
                        );
                        return false;
                    }
                    Ok(Ok(task))
                        if !matches!(
                            task.task_status_id,
                            id if id == TaskStatus::Queued.as_id()
                                || id == TaskStatus::Running.as_id()
                        ) =>
                    {
                        tracing::info!(
                            task_id = %task_id,
                            requested_status = status.as_id(),
                            actual_status = task.task_status_id,
                            "Terminal task transition lost a race; suppressing stale completion"
                        );
                        return false;
                    }
                    Ok(Ok(task)) => {
                        // Row still active. When fenced, a changed owner means
                        // the update can never apply — release/steal took the
                        // row from us — so retrying is futile and the write
                        // must be suppressed instead.
                        if let Some(fence) = fence_worker
                            && task.worker_id.as_deref() != Some(fence)
                        {
                            tracing::warn!(
                                task_id = %task_id,
                                owner = ?task.worker_id,
                                fence_worker = fence,
                                "Terminal task transition fenced out; task ownership changed while completing"
                            );
                            return false;
                        }
                        tracing::warn!(
                            task_id = %task_id,
                            attempt,
                            max_attempts,
                            "Terminal task transition affected no rows while task remains active"
                        );
                    }
                    Ok(Err(e)) => {
                        tracing::warn!(
                            task_id = %task_id,
                            attempt,
                            max_attempts,
                            "Failed to inspect zero-row terminal task transition: {}",
                            e
                        );
                    }
                    Err(_) => {
                        tracing::warn!(
                            task_id = %task_id,
                            attempt,
                            max_attempts,
                            "Timed out inspecting zero-row terminal task transition"
                        );
                    }
                }
            }
            Ok(Err(e)) => {
                tracing::warn!(
                    task_id = %task_id,
                    attempt,
                    max_attempts,
                    "Task::complete failed: {}",
                    e
                );
            }
            Err(_) => {
                tracing::error!(
                    task_id = %task_id,
                    attempt,
                    max_attempts,
                    timeout_ms = attempt_timeout.as_millis(),
                    "Task::complete timed out"
                );
            }
        }
        if attempt < max_attempts {
            tokio::time::sleep(std::time::Duration::from_millis(100 * attempt as u64)).await;
        }
    }
    persisted
}

/// Complete a task in the DB and emit a `task:complete` env event.
///
/// `duration_ms_override`: container terminal writes pass the billable
/// execution-window snapshot so the persisted `duration_ms` (and therefore
/// the re-read event payload and the task-minutes quota) reflects the
/// window the user was billed for instead of the claim-to-persist span;
/// every other caller passes `None` for the stop-start computation.
///
/// `fence_worker` is threaded to `Task::complete`: pass this worker's id when
/// the row was claimed by this worker so a write cannot land after ownership
/// was released or taken over; pass `None` for rows this worker never claimed
/// (pre-claim failures, orphan/adopted reconciliation).
///
/// Returns whether OUR terminal state is durable. `false` means another actor
/// won the terminal race (or persistence failed outright): the caller must
/// suppress every follow-up keyed to the outcome — `publish_task_alert` and
/// `maybe_retry_task`/`maybe_retry_zombie_task` — because the winning writer
/// publishes its own, and retrying an un-persisted (or cancelled) failure
/// double-runs the task.
#[allow(clippy::too_many_arguments)]
async fn complete_task_with_event(
    db: &DatabasePool,
    publisher: &StreamPubSub,
    task_id: &Uuid,
    env_id: Uuid,
    stream_id: Uuid,
    function_name: &str,
    task_type: &str,
    status: TaskStatus,
    result: Option<&serde_json::Value>,
    duration_ms_override: Option<i64>,
    fence_worker: Option<&str>,
) -> bool {
    // A task:complete event is only truthful after the terminal row is
    // durable. Retry briefly, then leave the row non-terminal; process_task's
    // postcondition will withhold the queue ACK and release heartbeat ownership
    // so the zombie reaper can reconcile it.
    let persisted = persist_terminal_task(
        db,
        task_id,
        &status,
        result,
        duration_ms_override,
        fence_worker,
        TASK_COMPLETION_DB_TIMEOUT,
        TASK_COMPLETION_ATTEMPTS,
    )
    .await;
    if !persisted {
        tracing::error!(
            task_id = %task_id,
            "Terminal task state was not persisted; suppressing completion events, alerts, and retries"
        );
        return false;
    }

    finalize_task_timing(db, task_id).await;

    // Re-read the row's duration_ms for the event (best-effort; null on
    // timeout/error). For container tasks the row carries the billable
    // execution-window snapshot persisted via `duration_ms_override` above,
    // so the event reports the same window the user was billed for; for all
    // other tasks it is the stop-start computation from `Task::complete`.
    let duration_ms = match tokio::time::timeout(DB_CALL_TIMEOUT, Task::get(db, task_id)).await {
        Ok(Ok(t)) => t.duration_ms,
        Ok(Err(_)) => None,
        Err(_) => {
            tracing::warn!(task_id = %task_id, "Task::get timed out reading duration_ms");
            None
        }
    };

    // Build error payload for the SSE event (only for non-success statuses)
    let error = match status {
        TaskStatus::Failed | TaskStatus::TimedOut | TaskStatus::Cancelled => result.cloned(),
        _ => None,
    };

    let event = EnvEvent::TaskComplete {
        task_id: *task_id,
        env_id,
        stream_id,
        function_name: function_name.to_string(),
        status: status.as_str().to_string(),
        duration_ms,
        error,
    };
    match tokio::time::timeout(DB_CALL_TIMEOUT, publisher.publish_env(event)).await {
        Ok(Ok(_)) => {}
        Ok(Err(e)) => {
            tracing::warn!(task_id = %task_id, "Failed to publish task:complete event: {}", e);
        }
        Err(_) => {
            tracing::error!(task_id = %task_id, "Publishing task:complete event timed out");
        }
    }

    // Also emit on the stream channel if the task_type is "code" (the originating Run listens)
    if task_type == "code" {
        let stream_event = if matches!(status, TaskStatus::Failed | TaskStatus::TimedOut) {
            StreamEvent::RunFail {
                run_id: *task_id,
                env_id,
                stream_id,
                event_id: None,
                error: result
                    .and_then(|v| v.get("$val"))
                    .and_then(|v| v.get("msg"))
                    .and_then(|v| v.as_str())
                    .map(String::from),
            }
        } else {
            StreamEvent::RunStop {
                run_id: *task_id,
                env_id,
                stream_id,
                event_id: None,
                result: result.cloned(),
            }
        };
        match tokio::time::timeout(DB_CALL_TIMEOUT, publisher.publish(stream_event)).await {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => {
                tracing::warn!(task_id = %task_id, "Failed to publish stream event for code task: {}", e);
            }
            Err(_) => {
                tracing::error!(task_id = %task_id, "Publishing stream event for code task timed out");
            }
        }
    }

    true
}

/// Publish a `task:failed` or `task:cancelled` alert.
async fn publish_task_alert(
    db: &DatabasePool,
    org_id: Option<Uuid>,
    env_id: Uuid,
    task_id: &Uuid,
    channel: &str,
    data: &serde_json::Value,
) {
    let Some(org_id) = org_id else {
        tracing::debug!(task_id = %task_id, "No org_id available, skipping {} alert", channel);
        return;
    };

    let alert_data = serde_json::json!({
        "task_id": task_id.to_string(),
        "env_id": env_id.to_string(),
        "error": data,
        "timestamp": chrono::Utc::now().to_rfc3339(),
    });

    match tokio::time::timeout(
        DB_CALL_TIMEOUT,
        hot::db::alert::publish_alert(db, &org_id, &env_id, channel, &alert_data),
    )
    .await
    {
        Ok(Ok(alert)) => {
            tracing::debug!(task_id = %task_id, "Published {} alert {}", channel, alert.alert_id);
        }
        Ok(Err(e)) => {
            tracing::error!(task_id = %task_id, "Failed to publish {} alert: {}", channel, e);
        }
        Err(_) => {
            tracing::error!(task_id = %task_id, "Publishing {} alert timed out", channel);
        }
    }
}

/// Check CUS usage thresholds and publish alerts at 80% and 100%.
async fn check_cus_thresholds(
    db: &DatabasePool,
    org_id: Option<Uuid>,
    env_id: Uuid,
    usage_stats_cache: &UsageStatsCache,
) {
    // The body issues several non-cheap DB calls (subscription, usage stats,
    // org notes). A stuck pool would otherwise pin the worker on every task.
    if let Err(_elapsed) = tokio::time::timeout(
        POST_TASK_CLEANUP_TIMEOUT,
        check_cus_thresholds_inner(db, org_id, env_id, usage_stats_cache),
    )
    .await
    {
        tracing::warn!(
            timeout_secs = POST_TASK_CLEANUP_TIMEOUT.as_secs(),
            "check_cus_thresholds timed out — skipping CUS alert pass"
        );
    }
}

async fn check_cus_thresholds_inner(
    db: &DatabasePool,
    org_id: Option<Uuid>,
    env_id: Uuid,
    usage_stats_cache: &UsageStatsCache,
) {
    let Some(org_id) = org_id else { return };

    let features = hot::db::features::Features::resolve_for_org(db, &org_id).await;
    let cus_limit = features.compute_units_per_month();
    if cus_limit <= 0 {
        return;
    }

    let subscription = hot::db::subscription::OrgPlan::get_by_org_id(db, &org_id).await;
    let period_start = subscription
        .as_ref()
        .ok()
        .and_then(|s| s.current_period_start)
        .unwrap_or_else(chrono::Utc::now);

    let Some(usage) = cached_usage_stats(
        db,
        org_id,
        period_start,
        features.call_retention_days(),
        usage_stats_cache,
    )
    .await
    else {
        return;
    };

    let pct = (usage.compute_units as f64 / cus_limit as f64) * 100.0;

    for threshold in [80.0_f64, 100.0_f64] {
        if pct >= threshold {
            let note_type = format!("cus_threshold_{}", threshold as i32);
            let existing = hot::db::OrgNote::list_by_category(db, &org_id, "billing", 50)
                .await
                .unwrap_or_default();

            let already_sent = existing.iter().any(|n| {
                n.note_type == note_type
                    && n.created_at > chrono::Utc::now() - chrono::Duration::days(30)
            });

            if !already_sent {
                let channel = if threshold >= 100.0 {
                    "usage:cus_exceeded"
                } else {
                    "usage:cus_warning"
                };
                let data = serde_json::json!({
                    "org_id": org_id.to_string(),
                    "threshold_pct": threshold,
                    "compute_units_used": usage.compute_units,
                    "compute_units_limit": cus_limit,
                    "usage_pct": format!("{:.1}", pct),
                    "timestamp": chrono::Utc::now().to_rfc3339(),
                });

                let _ = hot::db::alert::publish_alert(db, &org_id, &env_id, channel, &data).await;
                let _ = hot::db::OrgNote::create_system(
                    db,
                    &org_id,
                    "billing",
                    &note_type,
                    &format!(
                        "CUS usage reached {}% ({}/{})",
                        threshold, usage.compute_units, cus_limit
                    ),
                    Some(&data),
                )
                .await;

                tracing::info!(
                    org_id = %org_id,
                    threshold = threshold,
                    "CUS threshold alert sent"
                );
            }
        }
    }
}

/// Check whether a failed task should be retried based on its options.retry config.
/// If retries remain, creates a new task row with incremented retry_attempt and enqueues it.
async fn maybe_retry_task(
    db: &DatabasePool,
    task_queue: &ProcessingQueue<TaskRequest>,
    failed_task_id: &Uuid,
    original_request: &TaskRequest,
) {
    // This re-read is defence in depth behind the call-site persistence
    // gates AND the only source of the retry budget (`options` lives on the
    // row, not on the queue request). The caller only reaches here after the
    // terminal failure was durably persisted, so a transient transport
    // error/timeout must not silently drop the owed retry — retry the read
    // briefly. Only a row that provably should not retry may skip: a missing
    // row (NotFound) or one whose status is not a terminal failure.
    const RETRY_CHECK_READ_ATTEMPTS: usize = 3;
    let mut loaded = None;
    for attempt in 1..=RETRY_CHECK_READ_ATTEMPTS {
        match tokio::time::timeout(DB_CALL_TIMEOUT, Task::get(db, failed_task_id)).await {
            Ok(Ok(t)) => {
                loaded = Some(t);
                break;
            }
            Ok(Err(db::TaskError::NotFound)) => {
                tracing::info!(
                    task_id = %failed_task_id,
                    "Retry check: task row no longer exists; skipping retry"
                );
                return;
            }
            Ok(Err(e)) => {
                tracing::warn!(
                    task_id = %failed_task_id,
                    attempt,
                    "Retry check: couldn't load task: {}", e
                );
            }
            Err(_) => {
                tracing::warn!(
                    task_id = %failed_task_id,
                    attempt,
                    "Retry check: Task::get timed out"
                );
            }
        }
        if attempt < RETRY_CHECK_READ_ATTEMPTS {
            tokio::time::sleep(std::time::Duration::from_millis(100 * attempt as u64)).await;
        }
    }
    let Some(task) = loaded else {
        // Proceeding blindly is not an option: without the row we cannot see
        // whether retries are configured at all, or how much budget is left.
        tracing::error!(
            task_id = %failed_task_id,
            "Retry check: task row unreadable after {} attempts — DROPPING the owed retry because its retry budget (options) cannot be loaded",
            RETRY_CHECK_READ_ATTEMPTS,
        );
        return;
    };

    // Defence in depth behind the call-site persistence gates: only a row
    // that actually shows a terminal failure may spawn a retry. A Cancelled
    // row (cancellation won the terminal race), a still-active row (our
    // terminal write never landed), or a Completed row must never be re-run.
    if !matches!(
        TaskStatus::from_id(task.task_status_id),
        Some(TaskStatus::Failed | TaskStatus::TimedOut)
    ) {
        tracing::info!(
            task_id = %failed_task_id,
            status = task.task_status_id,
            "Retry check: task row is not a terminal failure; skipping retry"
        );
        return;
    }

    let options = match &task.options {
        Some(opts) => opts,
        None => return,
    };

    let retry_config = RetryConfig::from_meta(Some(options));
    if !retry_config.is_enabled() {
        return;
    }

    let current_attempt = task.retry_attempt;
    if current_attempt >= retry_config.max_retries {
        tracing::info!(
            task_id = %failed_task_id,
            attempt = current_attempt,
            max = retry_config.max_retries,
            "Task exhausted all retries"
        );
        return;
    }

    let next_attempt = current_attempt + 1;
    let delay_ms = retry_config.delay_for_attempt(next_attempt);
    let next_retry_at = chrono::Utc::now() + chrono::Duration::milliseconds(delay_ms);
    let new_task_id = Uuid::now_v7();

    match tokio::time::timeout(
        DB_CALL_TIMEOUT,
        Task::insert_retry(db, &new_task_id, &task, next_attempt, next_retry_at),
    )
    .await
    {
        Ok(Ok(true)) => {}
        Ok(Ok(false)) => {
            // The (parent, attempt) unique key says another writer — e.g. the
            // zombie reaper, or a crashed earlier attempt that got as far as
            // insert_retry — already created this retry row. Skip the
            // enqueue: the row's creator owns delivery, and if it crashed
            // before enqueueing, reconcile_queued_tasks re-enqueues the stale
            // queued row, so the retry cannot be stranded.
            tracing::info!(
                task_id = %failed_task_id,
                attempt = next_attempt,
                "Retry row already exists for this attempt — skipping duplicate retry"
            );
            return;
        }
        Ok(Err(e)) => {
            tracing::error!(
                task_id = %failed_task_id,
                new_task_id = %new_task_id,
                "Failed to insert retry task: {}", e
            );
            return;
        }
        Err(_) => {
            tracing::error!(
                task_id = %failed_task_id,
                new_task_id = %new_task_id,
                "Task::insert_retry timed out — skipping retry"
            );
            return;
        }
    }

    let mut retry_request = original_request.clone();
    retry_request.task_id = new_task_id.to_string();
    retry_request.created_at_unix_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;

    // If there's a delay, spawn a delayed enqueue; otherwise enqueue immediately
    if delay_ms > 0 {
        let tq = task_queue.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(delay_ms as u64)).await;
            if let Err(e) = tq.enqueue(retry_request).await {
                tracing::error!(new_task_id = %new_task_id, "Failed to enqueue retry task: {}", e);
            } else {
                tracing::debug!(new_task_id = %new_task_id, attempt = next_attempt, "Retry task enqueued after delay");
            }
        });
    } else if let Err(e) = task_queue.enqueue(retry_request).await {
        tracing::error!(new_task_id = %new_task_id, "Failed to enqueue retry task: {}", e);
    } else {
        tracing::debug!(new_task_id = %new_task_id, attempt = next_attempt, "Retry task enqueued immediately");
    }
}

async fn load_bytecode(
    build_id: &Uuid,
    cache: &BytecodeCache,
    db: Option<&DatabasePool>,
    worker_conf: Option<&Val>,
) -> Result<Arc<CachedBytecode>, String> {
    let cache_key = build_id.to_string();

    // Fast path: bytecode already in local cache
    if let Ok(cached) = cache.load(&cache_key) {
        tracing::debug!(build_id = %build_id, "Bytecode cache hit");
        return Ok(cached);
    }

    let (db, conf) = match (db, worker_conf) {
        (Some(d), Some(c)) => (d, c),
        _ => {
            return Err(format!(
                "Bytecode not found in cache for build {} and no DB/config available for fallback",
                build_id
            ));
        }
    };

    let build = hot::db::Build::get_build(db, build_id)
        .await
        .map_err(|e| format!("Failed to fetch build {}: {}", build_id, e))?;

    let project = hot::db::Project::get_project(db, &build.project_id)
        .await
        .map_err(|e| format!("Failed to fetch project: {}", e))?;

    if build.is_live() {
        load_bytecode_live(build_id, &cache_key, cache, &build, &project, conf)
    } else {
        load_bytecode_bundle(build_id, &cache_key, cache, &build, &project, db, conf).await
    }
}

/// Load bytecode for a live build by compiling from source paths on disk.
fn load_bytecode_live(
    build_id: &Uuid,
    cache_key: &str,
    cache: &BytecodeCache,
    _build: &hot::db::Build,
    project: &hot::db::Project,
    conf: &Val,
) -> Result<Arc<CachedBytecode>, String> {
    tracing::debug!(build_id = %build_id, project = %project.name, "Bytecode cache miss — compiling live build from source");

    let src_paths = hot::project::get_project_src_paths(conf, &project.name);
    if src_paths.is_empty() {
        return Err(format!(
            "Live build {} has no source paths configured for project '{}'",
            build_id, project.name
        ));
    }

    // Discover all source files (project sources + dependencies)
    let mut all_source_files = Vec::new();
    if let Ok(resolved_deps) = hot::project::get_resolved_project_dependencies(conf, &project.name)
    {
        for dep in &resolved_deps {
            let dep_path = dep.resolved_path.to_string_lossy().to_string();
            if let Ok(files) = hot::lang::engine::Engine::discover_hot_files(&dep_path) {
                all_source_files.extend(files);
            }
        }
    }
    for src_path in &src_paths {
        if let Ok(files) = hot::lang::engine::Engine::discover_hot_files(src_path) {
            all_source_files.extend(files);
        }
    }

    let file_hashes =
        hot::lang::cache::bytecode_cache::BytecodeCache::hash_files(&all_source_files)
            .unwrap_or_default();

    hot::lang::engine::Engine::compile_to_cache(
        &src_paths,
        cache,
        &project.name,
        Some(cache_key),
        Some(file_hashes),
        Some(conf),
    )
    .map_err(|e| format!("Failed to compile live build: {}", e))?;

    cache
        .load(cache_key)
        .map_err(|e| format!("Failed to load compiled bytecode: {}", e))
}

/// Compute the local on-disk extract directory for a given bundle build.
///
/// All worker code that needs to read files out of an extracted bundle
/// (`hot/src`, `resources/`, `manifest.hot`, …) goes through this helper so
/// the path scheme stays consistent between bytecode loading and other
/// consumers like container-task `mounts:`.
fn bundle_extract_dir(build_id: &Uuid) -> std::path::PathBuf {
    std::path::PathBuf::from(format!(".hot/task-worker/build-{}", build_id.simple()))
}

/// Prefix of the unique per-attempt temp dirs extraction writes into before
/// atomically renaming over the final `build-{id}` path. The final directory
/// can therefore only ever be observed fully formed.
const BUNDLE_EXTRACT_TEMP_PREFIX: &str = ".tmp-";

/// Any in-flight extraction finishes well inside this window; a temp dir
/// older than it belongs to an attempt whose process died mid-extract and
/// would otherwise accumulate on disk forever.
const BUNDLE_EXTRACT_TEMP_MAX_AGE: std::time::Duration = std::time::Duration::from_secs(60 * 60);

static BUNDLE_EXTRACT_LOCKS: std::sync::OnceLock<Mutex<HashMap<Uuid, std::sync::Weak<Mutex<()>>>>> =
    std::sync::OnceLock::new();

/// Per-build single-flight for local bundle extraction (same weak-registry
/// pattern as `usage_stats_org_lock`): two concurrent local attempts for the
/// same build — e.g. a retry racing the detached blocking writer of a
/// cancelled attempt — must not download and extract twice, nor race each
/// other's install into the final path.
async fn bundle_extract_lock(build_id: Uuid) -> Arc<Mutex<()>> {
    let locks = BUNDLE_EXTRACT_LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut locks = locks.lock().await;
    locks.retain(|_, lock| lock.strong_count() > 0);
    if let Some(lock) = locks.get(&build_id).and_then(std::sync::Weak::upgrade) {
        return lock;
    }
    let lock = Arc::new(Mutex::new(()));
    locks.insert(build_id, Arc::downgrade(&lock));
    lock
}

/// A build's extract dir counts as complete only when it holds
/// `manifest.hot` — the pre-existing marker every consumer already relies
/// on. With atomic installs the final directory's existence implies
/// completeness; the marker check additionally heals directories left
/// half-written by older workers that extracted straight into the final
/// path.
fn bundle_extract_is_complete(extract_dir: &std::path::Path) -> bool {
    extract_dir.join("manifest.hot").exists()
}

/// Age-based sweep of orphaned extraction temp dirs (a process death
/// mid-extract leaves one behind; a merely cancelled attempt's detached
/// writer cleans up after itself). Runs whenever an extraction prepares its
/// own temp dir in the same parent. Best-effort.
fn sweep_stale_bundle_extract_temps(parent: &std::path::Path, max_age: std::time::Duration) {
    let Ok(entries) = std::fs::read_dir(parent) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !name.starts_with(BUNDLE_EXTRACT_TEMP_PREFIX) {
            continue;
        }
        let stale = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age >= max_age);
        if stale {
            tracing::warn!(
                path = %entry.path().display(),
                "Removing stale bundle extraction temp dir left by a dead attempt"
            );
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

/// Atomically install a fully extracted temp dir at its final path. The
/// winner renames into place; a loser (the final dir was installed first)
/// removes its own temp dir and succeeds iff the final dir is complete.
fn install_extracted_bundle(
    temp_dir: &std::path::Path,
    extract_dir: &std::path::Path,
) -> Result<(), String> {
    // Heal a partial dir left by an older worker that extracted directly
    // into the final path (pre-atomic-install layout): it would otherwise
    // both fail the rename below and poison every future attempt.
    if extract_dir.exists() && !bundle_extract_is_complete(extract_dir) {
        let _ = std::fs::remove_dir_all(extract_dir);
    }
    match std::fs::rename(temp_dir, extract_dir) {
        Ok(()) => Ok(()),
        Err(rename_err) => {
            // Another attempt (a concurrent worker process, or a cancelled
            // attempt's detached extraction) installed first. Its rename was
            // atomic, so a complete final dir means our work is done.
            let _ = std::fs::remove_dir_all(temp_dir);
            if bundle_extract_is_complete(extract_dir) {
                Ok(())
            } else {
                Err(format!(
                    "Failed to install extracted bundle at {}: {}",
                    extract_dir.display(),
                    rename_err
                ))
            }
        }
    }
}

/// Blocking half of `ensure_bundle_extracted`: sweep stale temps, extract
/// into a fresh per-attempt temp dir NEXT TO the final path (same
/// filesystem, so the install rename is atomic), then rename into place. A
/// cancelled attempt's detached blocking task keeps writing only into its
/// own temp dir, never the shared final path, so a retry can never collide
/// with it ('File exists') or observe its partial files.
fn extract_bundle_to_dir(build_data: &[u8], extract_dir: &std::path::Path) -> Result<(), String> {
    let parent = extract_dir
        .parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    std::fs::create_dir_all(&parent).map_err(|e| {
        format!(
            "Failed to create bundle extract parent dir {}: {}",
            parent.display(),
            e
        )
    })?;
    sweep_stale_bundle_extract_temps(&parent, BUNDLE_EXTRACT_TEMP_MAX_AGE);

    let dir_name = extract_dir
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("bundle");
    let temp_dir = parent.join(format!(
        "{}{}-{}",
        BUNDLE_EXTRACT_TEMP_PREFIX,
        dir_name,
        Uuid::new_v4().simple()
    ));
    if let Err(e) = hot::bundle::extract_bundle_from_bytes(build_data, &temp_dir) {
        let _ = std::fs::remove_dir_all(&temp_dir);
        return Err(format!("Failed to extract bundle: {}", e));
    }
    install_extracted_bundle(&temp_dir, extract_dir)
}

/// Ensure a bundle build is extracted to its local cache dir, downloading
/// from storage if needed. Idempotent — if the extract dir already contains
/// `manifest.hot` we assume a previous worker invocation extracted it and
/// reuse the on-disk copy. Returns the extract directory.
async fn ensure_bundle_extracted(
    build_id: &Uuid,
    org_id: &Uuid,
    env_id: &Uuid,
    conf: &Val,
) -> Result<std::path::PathBuf, String> {
    let extract_dir = bundle_extract_dir(build_id);
    if bundle_extract_is_complete(&extract_dir) {
        return Ok(extract_dir);
    }

    // Per-build single-flight: concurrent local attempts download and
    // extract once, not N times. A cancelled holder releases the lock on
    // drop; its detached blocking writer below keeps to its own temp dir.
    let flight = bundle_extract_lock(*build_id).await;
    let _guard = flight.lock().await;
    if bundle_extract_is_complete(&extract_dir) {
        return Ok(extract_dir);
    }

    let storage = hot::storage::build_storage_from_config(conf)
        .await
        .map_err(|e| format!("Failed to create build storage: {}", e))?;
    let build_data = storage
        .retrieve_build(build_id, org_id, env_id)
        .await
        .map_err(|e| format!("Failed to retrieve build data: {}", e))?;
    let extract_dir_for_task = extract_dir.clone();
    tokio::task::spawn_blocking(move || extract_bundle_to_dir(&build_data, &extract_dir_for_task))
        .await
        .map_err(|e| format!("Bundle extraction task failed: {}", e))??;
    Ok(extract_dir)
}

/// Load bytecode for a bundle build by fetching the zip from storage, extracting, and compiling.
async fn load_bytecode_bundle(
    build_id: &Uuid,
    cache_key: &str,
    cache: &BytecodeCache,
    _build: &hot::db::Build,
    project: &hot::db::Project,
    db: &DatabasePool,
    conf: &Val,
) -> Result<Arc<CachedBytecode>, String> {
    tracing::debug!(build_id = %build_id, "Bytecode cache miss — fetching bundle build from storage");

    let env = hot::db::Env::get_env(db, &project.env_id)
        .await
        .map_err(|e| format!("Failed to fetch env: {}", e))?;

    let extract_dir = ensure_bundle_extracted(build_id, &env.org_id, &project.env_id, conf).await?;

    // Build a per-build resource registry from the manifest and cache it so
    // task threads can install it as a thread-local override during
    // `::hot::resource/*` calls. We do this once per bundle build (idempotent
    // on the per-build cache) so concurrent tasks for the same build share an
    // `Arc<ResourceRegistry>` without re-walking the bundle. Failures here are
    // logged but non-fatal — bytecode loading still proceeds, and Hot code
    // that calls `::hot::resource/*` will see an empty registry (treated as
    // "resource not found").
    match hot::bundle::read_bundle_resources(&extract_dir) {
        Ok(resources_val) => {
            let registry =
                std::sync::Arc::new(hot::lang::hot::resource::build_registry_from_manifest(
                    &resources_val,
                    &extract_dir,
                ));
            tracing::debug!(
                build_id = %build_id,
                resource_count = registry.entries.len(),
                "Installed per-build resource registry"
            );
            hot::lang::hot::resource::set_build_registry(*build_id, registry);
        }
        Err(e) => {
            tracing::warn!(
                build_id = %build_id,
                "Failed to read bundle manifest for resources (continuing without resource registry): {}",
                e
            );
        }
    }

    let src_dir = extract_dir.join("hot/src");
    let pkg_dir = extract_dir.join("hot/pkg");
    let mut src_paths: Vec<String> = Vec::new();

    let opts = hot::discovery::DiscoveryOpts::for_extension("hot");
    for dir in [&src_dir, &pkg_dir] {
        if dir.exists() {
            src_paths.extend(hot::discovery::discover_paths(&[dir], &opts));
        }
    }

    if src_paths.is_empty() {
        return Err(format!("No .hot source files found for build {}", build_id));
    }

    let bundle_cache_dir = extract_dir.join(".hot").join("cache");
    let _ = std::fs::create_dir_all(&bundle_cache_dir);
    let bundle_cache = BytecodeCache::new(bundle_cache_dir);

    hot::lang::engine::Engine::compile_to_cache(
        &src_paths,
        &bundle_cache,
        &project.name,
        Some(cache_key),
        None,
        Some(conf),
    )
    .map_err(|e| format!("Failed to compile build: {}", e))?;

    // Also save to the primary cache for future hits. The
    // tool/skill spec registries were already populated when the
    // bundle cache was written, so we just round-trip them.
    if let Ok(compiled) = bundle_cache.load(cache_key)
        && let Err(e) = cache.save(
            cache_key,
            &compiled.program,
            compiled.metadata.clone(),
            &compiled.function_mapping,
            &compiled.core_functions,
            &compiled.type_implementations,
            &compiled.ast_program,
            &compiled.hot_ast,
            &compiled.tool_specs,
            &compiled.skill_specs,
        )
    {
        tracing::warn!(build_id = %build_id, "Failed to save to primary cache: {}", e);
    }

    cache
        .load(cache_key)
        .map_err(|e| format!("Failed to load compiled bytecode: {}", e))
}

/// Run a fallible async operation up to `attempts` times, sleeping a
/// linearly growing backoff between attempts. Returns the first `Ok` or the
/// last `Err`. `attempts` must be at least 1.
async fn retry_with_backoff<T, E, F, Fut>(
    attempts: usize,
    backoff: std::time::Duration,
    mut op: F,
) -> Result<T, E>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, E>>,
{
    let mut last_err = None;
    for attempt in 1..=attempts {
        match op().await {
            Ok(value) => return Ok(value),
            Err(e) => last_err = Some(e),
        }
        if attempt < attempts {
            tokio::time::sleep(backoff * attempt as u32).await;
        }
    }
    Err(last_err.expect("retry_with_backoff requires at least one attempt"))
}

/// Attempt to (re)take DB ownership of an adopted task. Returns `true` when
/// no further attempts are needed: either the ownership UPDATE landed (so
/// the batch heartbeat now covers the row) or the row is no longer
/// `running` and can never be owned again. Returns `false` on a transport
/// error, in which case the caller must try again — an adopted row without
/// ownership has a frozen heartbeat, and once it crosses
/// `ZOMBIE_HEARTBEAT_STALE_SECS` the reaper fails the live task and
/// enqueues a duplicate run.
async fn try_adopt_task_ownership(db: &DatabasePool, task_id: &Uuid, worker_id: &str) -> bool {
    match Task::set_worker(db, task_id, worker_id).await {
        Ok(true) => {
            tracing::debug!(
                task_id = %task_id,
                "Adopted task ownership established (worker_id + heartbeat current)"
            );
            true
        }
        Ok(false) => {
            // Deliberate: the row left `running` between the adoption read
            // and this write (e.g. a user cancel raced us). Ownership can
            // never be taken now, so stop trying; the container monitor
            // still finishes the container out, and `persist_terminal_task`
            // suppresses our terminal write if another actor won the row.
            tracing::warn!(
                task_id = %task_id,
                "Adopted task row is no longer running; ownership cannot be taken. \
                 The container monitor will finish the container out and any stale terminal write will be suppressed."
            );
            true
        }
        Err(e) => {
            tracing::error!(
                task_id = %task_id,
                "Failed to take ownership of adopted task; its heartbeat stays stale and the zombie reaper will double-run it if this persists: {}",
                e
            );
            false
        }
    }
}

/// Register an adopted container task with the shutdown coordinator so it is
/// tracked exactly like a task dispatched through `process_task`. Without
/// this, the redelivery of the dead worker's un-ACKed queue message observes
/// running + owned-by-us + `!is_task_active` and releases ownership, after
/// which the zombie reaper fails the live container task and enqueues a
/// duplicate retry.
///
/// The registered request has exactly two consumers: coordinator activity
/// checks (`is_task_active` / drain accounting), which only need the task
/// identity, and the shutdown-time infra retry (`enqueue_infra_retry`),
/// which re-reads the task ROW for `Task::insert_retry` and uses the request
/// as the queue payload. A request synthesized from the row alone satisfies
/// both (org/project enrichment there is best-effort context), so when
/// reconstruction keeps failing after bounded retries we fall back to the
/// synthesized request instead of refusing adoption — refusal would leave a
/// LIVE container unmanaged and double-run via the zombie reaper.
///
/// Returns `false` (refusing adoption) only for a genuine duplicate: the
/// task is already in flight on this worker.
async fn register_adopted_task(
    coordinator: &shutdown::TaskShutdownCoordinator,
    db: &DatabasePool,
    task: &Task,
) -> bool {
    let original_request = match retry_with_backoff(
        ADOPTION_DB_ATTEMPTS,
        ADOPTION_DB_RETRY_BACKOFF,
        || task_request_from_db_row(db, task),
    )
    .await
    {
        Ok(request) => request,
        Err(e) => {
            tracing::error!(
                task_id = %task.task_id,
                "Failed to reconstruct adopted task request after {} attempts; registering with a row-synthesized request (a shutdown-time infra retry would lack org/project context): {}",
                ADOPTION_DB_ATTEMPTS,
                e,
            );
            synthesize_task_request_from_row(task)
        }
    };
    coordinator.try_register_task(shutdown::ActiveTask {
        task_id: task.task_id,
        env_id: task.env_id,
        stream_id: task.stream_id,
        function_name: task.function_name.clone(),
        task_type: task.task_type.clone(),
        cancel_token: None,
        original_request,
    })
}

/// Adopt orphaned containers from a previous worker crash.
///
/// Queries the executor's runtime for containers managed by `hot-task-worker`,
/// then:
/// - If the task is still running in DB and the container is alive: adopt it
///   (update `worker_id`).  *Docker only* — Kata containers cannot be adopted
///   live because their IO FIFOs are tied to the previous worker's process
///   handles; instead they are force-cleaned and their tasks are failed.
/// - If the task is still running but the container stopped: complete the
///   task and collect logs (Docker only — Kata FIFOs are gone, so we just
///   fail with a clear "container lost during worker restart" message).
/// - If the task is already terminal: just remove the container.
///
/// Returns a list of `(task_id, container_id, ownership_resolved)` triples
/// for containers that were adopted and need continued monitoring.
/// `ownership_resolved` is `false` when the adoption-time `Task::set_worker`
/// kept failing on transport errors; the monitor must then repair ownership
/// from its poll loop so heartbeat coverage lands as soon as the DB
/// recovers, ahead of the reaper's staleness horizon.
async fn adopt_orphaned_containers(
    executor: &executor::BoxExecutor,
    db: &DatabasePool,
    stream_publisher: &StreamPubSub,
    task_queue: &ProcessingQueue<TaskRequest>,
    coordinator: &shutdown::TaskShutdownCoordinator,
    worker_id: &str,
) -> Vec<(Uuid, String, bool)> {
    let mut adopted = Vec::new();

    #[cfg(all(target_os = "linux", feature = "kata"))]
    if matches!(executor, executor::BoxExecutor::Kata(_)) {
        cleanup_kata_orphans(executor, db, stream_publisher, task_queue).await;
        return adopted;
    }

    let containers = match executor {
        executor::BoxExecutor::Docker(docker_exec) => {
            use bollard::query_parameters::ListContainersOptionsBuilder;
            let mut filters = std::collections::HashMap::new();
            filters.insert(
                "label".to_string(),
                vec!["hot.dev/managed-by=hot-task-worker".to_string()],
            );
            let opts = ListContainersOptionsBuilder::default()
                .all(true)
                .filters(&filters)
                .build();
            match docker_exec.docker.list_containers(Some(opts)).await {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!("Failed to list containers for adoption: {}", e);
                    return adopted;
                }
            }
        }
        #[cfg(all(target_os = "linux", feature = "kata"))]
        executor::BoxExecutor::Kata(_) => unreachable!("Kata handled above"),
    };

    if containers.is_empty() {
        return adopted;
    }

    tracing::info!(
        "Found {} orphaned container(s) from previous worker",
        containers.len()
    );

    for c in &containers {
        let container_id = match &c.id {
            Some(id) => id.clone(),
            None => continue,
        };

        let task_id_str = c
            .labels
            .as_ref()
            .and_then(|l| l.get("hot.dev/task-id"))
            .cloned();

        let task_id = match task_id_str.as_deref().and_then(|s| Uuid::parse_str(s).ok()) {
            Some(id) => id,
            None => {
                tracing::debug!(
                    container_id = %container_id,
                    "Orphaned container has no task-id label, removing"
                );
                kill_and_remove_with_timeout(executor, &container_id, None).await;
                continue;
            }
        };

        let task = match Task::get(db, &task_id).await {
            Ok(t) => t,
            Err(_) => {
                tracing::debug!(
                    task_id = %task_id,
                    container_id = %container_id,
                    "Orphaned container's task not found in DB, removing"
                );
                kill_and_remove_with_timeout(executor, &container_id, Some(&task_id)).await;
                continue;
            }
        };

        let is_running_in_db = task.task_status_id == TaskStatus::Running.as_id();

        if !is_running_in_db {
            tracing::debug!(
                task_id = %task_id,
                container_id = %container_id,
                status = %task.status,
                "Task already terminal, removing orphaned container"
            );
            kill_and_remove_with_timeout(executor, &container_id, Some(&task_id)).await;
            continue;
        }

        // Task is running in DB — check if container is actually alive
        match executor.inspect_status(&container_id).await {
            Ok(None) => {
                // Container is still running — adopt it. Register with the
                // shutdown coordinator BEFORE taking DB ownership: the dead
                // worker's un-ACKed queue message will be XAUTOCLAIMed and
                // redelivered, and `task_message_should_execute` must see the
                // adopted task as active (withhold ACK, defer) instead of
                // releasing ownership out from under the live container.
                if !register_adopted_task(coordinator, db, &task).await {
                    tracing::warn!(
                        task_id = %task_id,
                        container_id = %container_id,
                        "Skipping container adoption; task could not be registered as active"
                    );
                    continue;
                }
                tracing::debug!(
                    task_id = %task_id,
                    container_id = %container_id,
                    "Adopting running container (updating worker_id)"
                );
                // Ownership (worker_id + fresh heartbeat) is required so the
                // batch heartbeat covers the adopted row, but it is
                // repairable: a transient DB fault here must not abandon the
                // live container. Retry briefly, then hand repair to the
                // monitor's poll loop (every 2s — well inside the reaper's
                // ZOMBIE_HEARTBEAT_STALE_SECS horizon once the DB recovers).
                let mut ownership_resolved = false;
                for attempt in 1..=ADOPTION_DB_ATTEMPTS {
                    ownership_resolved = try_adopt_task_ownership(db, &task_id, worker_id).await;
                    if ownership_resolved {
                        break;
                    }
                    if attempt < ADOPTION_DB_ATTEMPTS {
                        tokio::time::sleep(ADOPTION_DB_RETRY_BACKOFF * attempt as u32).await;
                    }
                }
                if !ownership_resolved {
                    tracing::error!(
                        task_id = %task_id,
                        container_id = %container_id,
                        "Adoption could not take DB ownership after {} attempts; keeping the container managed and repairing ownership from the monitor loop. Until repair lands, the row's heartbeat stays stale and the zombie reaper may double-run the task.",
                        ADOPTION_DB_ATTEMPTS,
                    );
                }
                adopted.push((task_id, container_id, ownership_resolved));
            }
            Ok(Some(exit_code)) => {
                // Container stopped — complete the task
                tracing::debug!(
                    task_id = %task_id,
                    container_id = %container_id,
                    exit_code,
                    "Orphaned container already stopped, completing task"
                );
                let (stdout, stderr) = executor
                    .collect_logs(&container_id)
                    .await
                    .unwrap_or_default();
                executor.remove_container(&container_id).await;

                let status = if exit_code == 0 {
                    TaskStatus::Completed
                } else {
                    TaskStatus::Failed
                };
                let result_json = serde_json::json!({
                    "exit-code": exit_code,
                    "stdout": stdout,
                    "stderr": stderr,
                    "adopted": true,
                });
                // Not fenced: the row is still owned by the dead worker at
                // this point (the container was never adopted).
                let persisted = complete_task_with_event(
                    db,
                    stream_publisher,
                    &task_id,
                    task.env_id,
                    task.stream_id,
                    &task.function_name,
                    &task.task_type,
                    status.clone(),
                    Some(&result_json),
                    None,
                    None,
                )
                .await;
                if persisted && status == TaskStatus::Failed {
                    maybe_retry_zombie_task(db, &task, task_queue).await;
                }
            }
            Err(e) => {
                // Inspect failed (container might have been removed) — fail the task
                tracing::warn!(
                    task_id = %task_id,
                    container_id = %container_id,
                    "Failed to inspect orphaned container: {}", e
                );
                tracing::error!(task_id = %task_id, "Container lost during adoption: {}", e);
                let error = task_failure_json("Container lost during worker restart", None);
                if complete_task_with_event(
                    db,
                    stream_publisher,
                    &task_id,
                    task.env_id,
                    task.stream_id,
                    &task.function_name,
                    &task.task_type,
                    TaskStatus::Failed,
                    Some(&error),
                    None,
                    None,
                )
                .await
                {
                    maybe_retry_zombie_task(db, &task, task_queue).await;
                }
            }
        }
    }

    if !adopted.is_empty() {
        tracing::info!(
            "Adopted {} container(s) from previous worker",
            adopted.len()
        );
    }

    adopted
}

/// Force-clean any containers left in the kata-containerd `hot-box`
/// namespace by a previous worker, and fail any DB rows that still believe
/// those containers are running.
///
/// kata-containerd is a host service shared across worker generations: when a
/// worker dies, its containers, snapshots and IO FIFOs are not cleaned up by
/// the runtime. The host-level `orphan_reaper` SIGKILLs the leaked
/// shim/qemu processes, but the *containerd state* (Container records and
/// devmapper snapshots) survives — eventually exhausting the snapshot pool.
///
/// We can't truly adopt a live Kata workload (the IO FIFOs and supervising
/// task in the previous worker process are gone), so the right semantics is
/// "clean up everything we find, fail the corresponding tasks". A subsequent
/// worker startup will see an empty namespace.
///
/// Safe to call only at worker startup, when no other worker is running on
/// the same host (the standard ECS task-worker deployment satisfies this).
#[cfg(all(target_os = "linux", feature = "kata"))]
async fn cleanup_kata_orphans(
    executor: &executor::BoxExecutor,
    db: &DatabasePool,
    stream_publisher: &StreamPubSub,
    task_queue: &ProcessingQueue<TaskRequest>,
) {
    let containers = match executor.list_orphan_containers().await {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("kata.orphans.list_failed: {}", e);
            return;
        }
    };

    if containers.is_empty() {
        return;
    }

    tracing::warn!(
        "kata.orphans: found {} container(s) in hot-box namespace from previous worker",
        containers.len()
    );

    for (container_id, task_id_label) in containers {
        // Force-cleanup the runtime state regardless of the DB outcome — even
        // if the corresponding task can't be found in the DB, the containerd
        // record and snapshot must be reaped.
        executor.cleanup_orphan(&container_id).await;

        let task_id = match task_id_label
            .as_deref()
            .and_then(|s| Uuid::parse_str(s).ok())
        {
            Some(id) => id,
            None => {
                tracing::debug!(
                    container_id = %container_id,
                    "kata.orphans.cleanup: container had no hot.dev/task-id label"
                );
                continue;
            }
        };

        let task = match Task::get(db, &task_id).await {
            Ok(t) => t,
            Err(e) => {
                tracing::debug!(
                    task_id = %task_id,
                    container_id = %container_id,
                    "kata.orphans.cleanup: task not in DB ({})", e
                );
                continue;
            }
        };

        if task.task_status_id != TaskStatus::Running.as_id() {
            tracing::debug!(
                task_id = %task_id,
                container_id = %container_id,
                status = %task.status,
                "kata.orphans.cleanup: container reaped, task already terminal"
            );
            continue;
        }

        tracing::info!(
            task_id = %task_id,
            container_id = %container_id,
            "kata.orphans.cleanup: failing task whose container was reaped"
        );
        let error = task_failure_json(
            "Container lost during worker restart (kata orphan cleanup)",
            None,
        );
        if complete_task_with_event(
            db,
            stream_publisher,
            &task_id,
            task.env_id,
            task.stream_id,
            &task.function_name,
            &task.task_type,
            TaskStatus::Failed,
            Some(&error),
            None,
            None,
        )
        .await
        {
            maybe_retry_zombie_task(db, &task, task_queue).await;
        }
    }
}

/// Unregisters an adopted task from the shutdown coordinator when its monitor
/// exits, whatever the exit path (completion, timeout, disappearance,
/// shutdown, failed initial load, panic). A task that stayed registered after
/// its monitor died would block shutdown drain; one that was unregistered
/// while its monitor lives would be zombified by its own redelivered queue
/// message.
struct AdoptedTaskRegistration<'a> {
    coordinator: &'a shutdown::TaskShutdownCoordinator,
    task_id: Uuid,
}

impl Drop for AdoptedTaskRegistration<'_> {
    fn drop(&mut self) {
        self.coordinator.unregister_task(&self.task_id);
    }
}

/// Monitor an adopted container until completion.
/// Runs as a background task, polls container status, and completes the task when done.
///
/// `ownership_resolved` mirrors the adoption-time `try_adopt_task_ownership`
/// outcome: while `false`, the poll loop keeps re-attempting the idempotent
/// `Task::set_worker` so worker ownership and heartbeat coverage land as
/// soon as the DB recovers — ahead of the reaper's 30s staleness horizon.
#[allow(clippy::too_many_arguments)]
async fn monitor_adopted_container(
    task_id: Uuid,
    container_id: String,
    db: &DatabasePool,
    stream_publisher: &StreamPubSub,
    task_queue: &ProcessingQueue<TaskRequest>,
    executor: &executor::BoxExecutor,
    coordinator: &shutdown::TaskShutdownCoordinator,
    worker_id: &str,
    mut ownership_resolved: bool,
) {
    // Adoption registered the task (see `register_adopted_task`); this guard
    // is the matching unregister on every exit of the monitor.
    let _registration = AdoptedTaskRegistration {
        coordinator,
        task_id,
    };

    // Bounded retry: a transient DB error here would otherwise abandon the
    // monitor, unregister the task, and leave the live container unmanaged.
    let task = match retry_with_backoff(ADOPTION_DB_ATTEMPTS, ADOPTION_DB_RETRY_BACKOFF, || {
        Task::get(db, &task_id)
    })
    .await
    {
        Ok(t) => t,
        Err(e) => {
            tracing::error!(
                task_id = %task_id,
                container_id = %container_id,
                "Failed to load adopted task after {} attempts — abandoning monitor; the live container is now UNMANAGED and the zombie reaper will fail its task and enqueue a duplicate run: {}",
                ADOPTION_DB_ATTEMPTS,
                e,
            );
            return;
        }
    };

    let timeout_deadline = task
        .start_time
        .map(|st| st + chrono::Duration::milliseconds(task.timeout_ms))
        .unwrap_or_else(|| chrono::Utc::now() + chrono::Duration::milliseconds(task.timeout_ms));

    let poll_interval = std::time::Duration::from_secs(2);

    loop {
        if coordinator.is_shutting_down() {
            tracing::debug!(
                task_id = %task_id,
                container_id = %container_id,
                "Stopping adopted container monitor (shutdown)"
            );
            return;
        }

        tokio::time::sleep(poll_interval).await;

        // Ownership repair: adoption may not have persisted worker_id (see
        // `adopt_orphaned_containers`). `Task::set_worker` is an idempotent
        // UPDATE, so keep re-attempting until it lands or the row provably
        // leaves `running`; without it the row's heartbeat stays frozen and
        // the zombie reaper double-runs the task once it crosses
        // ZOMBIE_HEARTBEAT_STALE_SECS.
        if !ownership_resolved {
            ownership_resolved = try_adopt_task_ownership(db, &task_id, worker_id).await;
        }

        match executor.inspect_status(&container_id).await {
            Ok(Some(exit_code)) => {
                // Container finished
                let (stdout, stderr) = executor
                    .collect_logs(&container_id)
                    .await
                    .unwrap_or_default();
                executor.remove_container(&container_id).await;

                let status = if exit_code == 0 {
                    TaskStatus::Completed
                } else {
                    TaskStatus::Failed
                };
                let result_json = serde_json::json!({
                    "exit-code": exit_code,
                    "stdout": stdout,
                    "stderr": stderr,
                    "adopted": true,
                });
                // Not fenced: adoption-time ownership can lag (repair
                // happens in this poll loop), so the row may still carry the
                // dead worker's id; the payload comparison in
                // `persist_terminal_task` still suppresses duplicate events.
                let persisted = complete_task_with_event(
                    db,
                    stream_publisher,
                    &task_id,
                    task.env_id,
                    task.stream_id,
                    &task.function_name,
                    &task.task_type,
                    status.clone(),
                    Some(&result_json),
                    None,
                    None,
                )
                .await;

                if persisted && status == TaskStatus::Failed {
                    maybe_retry_zombie_task(db, &task, task_queue).await;
                }

                tracing::debug!(
                    task_id = %task_id,
                    container_id = %container_id,
                    exit_code,
                    "Adopted container completed"
                );
                return;
            }
            Ok(None) => {
                // Still running — check timeout
                if chrono::Utc::now() >= timeout_deadline {
                    tracing::warn!(
                        task_id = %task_id,
                        container_id = %container_id,
                        "Adopted container timed out"
                    );
                    let (stdout, stderr) = executor
                        .collect_logs(&container_id)
                        .await
                        .unwrap_or_default();
                    kill_and_remove_with_timeout(executor, &container_id, Some(&task_id)).await;

                    let error = task_failure_json(
                        "Container task timed out",
                        Some(serde_json::json!({
                            "stdout": stdout,
                            "stderr": stderr,
                            "adopted": true,
                        })),
                    );
                    if complete_task_with_event(
                        db,
                        stream_publisher,
                        &task_id,
                        task.env_id,
                        task.stream_id,
                        &task.function_name,
                        &task.task_type,
                        TaskStatus::TimedOut,
                        Some(&error),
                        None,
                        None,
                    )
                    .await
                    {
                        maybe_retry_zombie_task(db, &task, task_queue).await;
                    }
                    return;
                }
            }
            Err(e) => {
                if matches!(e, executor::ExecutorError::ContainerNotFound(_)) {
                    tracing::warn!(
                        task_id = %task_id,
                        container_id = %container_id,
                        "Adopted container disappeared (removed externally)"
                    );
                    let error = task_failure_json(
                        "Container was removed before completion",
                        Some(serde_json::json!({
                            "adopted": true,
                        })),
                    );
                    if complete_task_with_event(
                        db,
                        stream_publisher,
                        &task_id,
                        task.env_id,
                        task.stream_id,
                        &task.function_name,
                        &task.task_type,
                        TaskStatus::Failed,
                        Some(&error),
                        None,
                        None,
                    )
                    .await
                    {
                        maybe_retry_zombie_task(db, &task, task_queue).await;
                    }
                    return;
                }
                tracing::warn!(
                    task_id = %task_id,
                    container_id = %container_id,
                    "inspect failed during adopted monitor: {}", e
                );
            }
        }
    }
}

/// Clean up stale data volume mounts from a previous worker crash.
async fn cleanup_stale_data_volumes(data_vol_base: &std::path::Path) {
    if data_vol_base.exists()
        && let Ok(entries) = std::fs::read_dir(data_vol_base)
    {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                #[cfg(target_os = "linux")]
                {
                    let _ = std::process::Command::new("umount")
                        .arg(path.join("mnt"))
                        .output();
                }
                if let Err(e) = std::fs::remove_dir_all(&path) {
                    tracing::warn!("Failed to clean up stale data volume {:?}: {}", path, e);
                }
            }
        }
    }
}

/// Start a per-task file server for hotbox CLI access from inside Docker containers.
///
/// For Docker on Linux: listens on a unix socket that gets bind-mounted into the container.
/// For Docker on macOS: listens on TCP (VirtioFS doesn't support Unix socket bind mounts).
///
/// Kata file servers are started separately via a pre-start hook in the executor,
/// because they need the VM's vsock UDS path which is only available after task creation.
#[allow(clippy::too_many_arguments)]
async fn start_file_server_for_task(
    task_id: &Uuid,
    #[cfg_attr(not(target_os = "linux"), allow(unused))] socket_base: &std::path::Path,
    org_id: Option<Uuid>,
    env_id: Uuid,
    user_id: Uuid,
    run_id: Option<Uuid>,
    db: &Arc<DatabasePool>,
    worker_conf: &Val,
    _backend: executor::Backend,
) -> Result<file_server::FileServerHandle, String> {
    let org_id = org_id.ok_or_else(|| "No org_id for file server".to_string())?;

    let storage = hot::file_storage::file_storage_from_config(worker_conf).await?;
    let storage: Arc<dyn hot::file_storage::FileStorage> = Arc::from(storage);
    let auth_token = Uuid::new_v4().as_simple().to_string();

    let ctx = file_server::FileServerContext {
        org_id,
        env_id,
        user_id,
        run_id,
        auth_token,
        db: Arc::clone(db),
        storage,
    };

    #[cfg(not(target_os = "linux"))]
    {
        file_server::start_tcp(task_id, ctx)
            .await
            .map_err(|e| e.to_string())
    }
    #[cfg(target_os = "linux")]
    {
        let socket_dir = socket_base.join("sockets");
        file_server::start(task_id, &socket_dir, ctx)
            .await
            .map_err(|e| e.to_string())
    }
}

/// Build ContainerExtras with bind mounts for the hotbox binary, socket, and data volume.
///
/// For Docker: bind-mounts the hotbox binary and unix socket into the container.
/// For Kata: sets vsock env vars only (hotbox binary comes from the VM rootfs,
/// injected via OCI bind mount in the Kata executor's build_spec).
fn build_container_extras(
    file_server: Option<&file_server::FileServerHandle>,
    data_volume: Option<&data_volume::DataVolume>,
    backend: executor::Backend,
) -> executor::ContainerExtras {
    let mut extras = executor::ContainerExtras::default();
    let is_docker = matches!(backend, executor::Backend::Docker);

    if is_docker {
        if let Some(path) = find_hotbox_binary() {
            extras.binds.push(format!(
                "{}:/usr/local/bin/hotbox:ro",
                path.to_string_lossy()
            ));
        } else {
            tracing::warn!(
                "hotbox binary not found — container tasks won't have access to hotbox CLI. \
                 Run `scripts/build-hotbox.sh` to cross-compile for Linux."
            );
        }
    }

    if let Some(handle) = file_server {
        extras
            .extra_env
            .push(format!("HOTBOX_AUTH_TOKEN={}", handle.auth_token()));
        if handle.is_vsock() {
            // Kata: guest connects via vsock, no bind mounts needed for the socket
            #[cfg(all(target_os = "linux", feature = "kata"))]
            if let Some(port) = handle.vsock_port() {
                extras.extra_env.push("HOTBOX_TRANSPORT=vsock".to_string());
                extras.extra_env.push(format!("HOTBOX_VSOCK_PORT={}", port));
            }
        } else if handle.is_tcp() {
            // TCP transport (macOS Docker Desktop where VirtioFS doesn't support Unix sockets).
            // The container connects back to the host via host.docker.internal,
            // which requires bridge networking even when the task doesn't request internet.
            if let Some(port) = handle.tcp_port() {
                extras
                    .extra_env
                    .push(format!("HOTBOX_URL=http://host.docker.internal:{}", port));
                extras.needs_host_network = true;
            }
        } else {
            // Docker on Linux: bind-mount the socket's parent directory into the container.
            let socket_path = handle.socket_path();
            if let Some(socket_dir) = socket_path.parent() {
                let socket_filename = socket_path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy();
                extras
                    .binds
                    .push(format!("{}:/hot/sockets:rw", socket_dir.to_string_lossy()));
                extras
                    .extra_env
                    .push(format!("HOTBOX_SOCKET=/hot/sockets/{}", socket_filename));
            }
        }
    }

    // Bind-mount the data volume at /data inside the container.
    // Docker: uses the binds list; Kata: uses data_volume_path for the OCI spec mount.
    if let Some(vol) = data_volume {
        let mount_str = vol.mount_point().to_string_lossy().to_string();
        if is_docker {
            extras.binds.push(format!("{}:/data:rw", mount_str));
        }
        extras.data_volume_path = Some(mount_str);
    }

    extras
}

/// Locate the hotbox Linux binary for bind-mounting into Docker containers.
///
/// Search order (first match wins):
///   1. target/hotbox-linux-{arch}    — dev cross-compile (scripts/build-hotbox.sh)
///   2. resources/bin/hotbox-linux-{arch} — installed package (brew/deb/pkg)
///   3. /opt/hot/bin/hotbox-linux-{arch} — ECS multi-arch bundle
///   4. sibling `hotbox` next to exe   — Linux hosts where native binary matches
///
/// On macOS, Docker Desktop can only bind-mount files from shared paths (typically
/// /Users, /Volumes, /private, /tmp). If the binary is outside these paths (e.g.
/// /usr/local/share/hot/), it's copied to a temp file under /tmp so Docker can
/// access it.
fn find_hotbox_binary() -> Option<std::path::PathBuf> {
    let arch = match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "x86_64",
        other => other,
    };

    // 1. Dev cross-compile output
    let dev_bin = std::env::current_exe().ok().and_then(|exe| {
        // Walk up from exe (e.g. target/debug/hot) to find the workspace target/ dir
        let mut dir = exe.parent()?;
        loop {
            let candidate = dir.join(format!("hotbox-linux-{}", arch));
            if candidate.exists() {
                return Some(candidate);
            }
            dir = dir.parent()?;
        }
    });
    if dev_bin.is_some() {
        return dev_bin;
    }

    // 2. Installed package (resources/bin/)
    if let Ok(path) = hot::resources::get_hotbox_path(arch) {
        return Some(ensure_docker_accessible(path));
    }

    // 3. ECS multi-arch bundle
    let ecs_bin = std::path::PathBuf::from(format!("/opt/hot/bin/hotbox-linux-{}", arch));
    if ecs_bin.exists() {
        return Some(ecs_bin);
    }

    // 4. Sibling binary (Linux hosts where native hotbox is a Linux ELF)
    #[cfg(target_os = "linux")]
    {
        let sibling = std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(|p| p.join("hotbox")))
            .filter(|p| p.exists());
        if sibling.is_some() {
            return sibling;
        }
    }

    None
}

/// On macOS, Docker Desktop can only bind-mount from shared paths (/Users, /Volumes,
/// /private, /tmp). Binaries installed at /usr/local/share/hot/ are outside these
/// paths, causing Docker to silently create an empty directory mount instead of
/// mounting the file. We detect this and copy the binary to /tmp.
fn ensure_docker_accessible(path: std::path::PathBuf) -> std::path::PathBuf {
    #[cfg(target_os = "macos")]
    {
        let path_str = path.to_string_lossy();
        let is_shared = path_str.starts_with("/Users/")
            || path_str.starts_with("/Volumes/")
            || path_str.starts_with("/private/")
            || path_str.starts_with("/tmp/");
        if !is_shared {
            let filename = path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string();
            let tmp_path = std::path::PathBuf::from(format!("/tmp/hot-{}", filename));
            let needs_copy = if tmp_path.exists() {
                let src_modified = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
                let dst_modified = std::fs::metadata(&tmp_path).and_then(|m| m.modified()).ok();
                src_modified != dst_modified
            } else {
                true
            };
            if needs_copy {
                if let Err(e) = std::fs::copy(&path, &tmp_path) {
                    tracing::warn!(
                        "Failed to copy hotbox binary to Docker-accessible path: {}",
                        e
                    );
                    return path;
                }
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let _ =
                        std::fs::set_permissions(&tmp_path, std::fs::Permissions::from_mode(0o755));
                }
                tracing::debug!(
                    "Copied hotbox binary to {} for Docker Desktop access",
                    tmp_path.display()
                );
            }
            return tmp_path;
        }
    }

    let _ = &path; // suppress unused warning on non-macOS
    path
}

fn create_emitter(db: &DatabasePool) -> Option<Arc<dyn EngineEventEmitter>> {
    let emitter = hot::lang::emitter::DatabaseEngineEventEmitter::new_with_pool(db.clone());
    Some(Arc::new(emitter))
}

fn create_event_publisher(
    config: &TaskWorkerConfig,
    db: &DatabasePool,
) -> Option<Arc<dyn EventPublisher>> {
    let database_publisher = hot::lang::event::DatabaseEventPublisher::new_with_pool(db.clone());

    let queue_publisher = hot::lang::event::QueueEventPublisher::new_with_cluster(
        config.queue_type,
        "hot:event".to_string(),
        config.redis_uri.clone(),
        config.redis_cluster,
        config.serialization,
    );

    Some(Arc::new(
        hot::lang::event::QueueAndDatabaseEventPublisher::new(queue_publisher, database_publisher),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use hot::stream::EnvSubscriberFactory;
    use hot::val;

    fn make_task_request_and_row() -> (TaskRequest, Task, Uuid, Uuid, Uuid) {
        let task_id = Uuid::now_v7();
        let env_id = Uuid::now_v7();
        let stream_id = Uuid::now_v7();
        let build_id = Uuid::now_v7();
        let now = chrono::Utc::now();

        let request = TaskRequest {
            task_id: task_id.to_string(),
            env_id: env_id.to_string(),
            stream_id: stream_id.to_string(),
            build_id: build_id.to_string(),
            function_name: "::app/background".to_string(),
            args: serde_json::json!({"input": "ok"}),
            task_type: "code".to_string(),
            timeout_ms: 60_000,
            origin_run_id: None,
            org_id: Some(Uuid::now_v7().to_string()),
            user_id: Some(Uuid::now_v7().to_string()),
            project_id: None,
            project_name: Some("test".to_string()),
            created_at_unix_ms: 0,
        };

        let task = Task {
            task_id,
            env_id,
            stream_id,
            build_id,
            origin_run_id: None,
            task_status_id: TaskStatus::Queued.as_id(),
            status: TaskStatus::Queued.as_str().to_string(),
            function_name: request.function_name.clone(),
            args: Some(request.args.clone()),
            options: None,
            task_type: request.task_type.clone(),
            start_time: None,
            stop_time: None,
            duration_ms: None,
            result: None,
            info: None,
            timing: None,
            timeout_ms: request.timeout_ms as i64,
            retry_attempt: 0,
            infra_retry_count: 0,
            next_retry_at: None,
            parent_task_id: None,
            created_at: now,
            by_user_id: None,
            run_id: None,
            worker_id: None,
            last_heartbeat_at: None,
            container_id: None,
            origin_run_fn: None,
        };

        (request, task, env_id, stream_id, build_id)
    }

    async fn insert_test_task(db: &DatabasePool, task: &Task) {
        Task::insert(
            db,
            &task.task_id,
            &task.env_id,
            &task.stream_id,
            &task.build_id,
            task.origin_run_id.as_ref(),
            &task.function_name,
            task.args.as_ref(),
            task.options.as_ref(),
            &task.task_type,
            task.timeout_ms,
            task.by_user_id.as_ref(),
        )
        .await
        .unwrap();
    }

    #[test]
    fn test_validate_task_request_accepts_matching_db_row() {
        let (request, task, env_id, stream_id, build_id) = make_task_request_and_row();

        assert!(
            validate_task_request_matches_db(&request, &task, env_id, stream_id, build_id).is_ok()
        );
    }

    #[tokio::test]
    async fn running_redelivery_withholds_ack_and_releases_inactive_owner() {
        let db = hot::db::test_db().await;
        let (_request, task, _, _, _) = make_task_request_and_row();
        insert_test_task(&db, &task).await;
        Task::mark_running(&db, &task.task_id).await.unwrap();
        Task::set_worker(&db, &task.task_id, "worker-a")
            .await
            .unwrap();
        let running = Task::get(&db, &task.task_id).await.unwrap();
        let coordinator = shutdown::TaskShutdownCoordinator::new();

        let err = task_message_should_execute(&db, &running, &coordinator, "worker-a", 10)
            .await
            .expect_err("a running task must keep its queue delivery pending");

        assert_eq!(err.backoff(), std::time::Duration::from_secs(5));
        let released = Task::get(&db, &task.task_id).await.unwrap();
        assert!(released.worker_id.is_none());
        assert!(
            Task::find_zombie_tasks(&db, 30)
                .await
                .unwrap()
                .iter()
                .any(|candidate| candidate.task_id == task.task_id)
        );
    }

    #[tokio::test]
    async fn running_redelivery_does_not_release_an_active_owner() {
        let db = hot::db::test_db().await;
        let (request, task, _, _, _) = make_task_request_and_row();
        insert_test_task(&db, &task).await;
        Task::mark_running(&db, &task.task_id).await.unwrap();
        Task::set_worker(&db, &task.task_id, "worker-a")
            .await
            .unwrap();
        let running = Task::get(&db, &task.task_id).await.unwrap();
        let coordinator = shutdown::TaskShutdownCoordinator::new();
        assert!(coordinator.try_register_task(shutdown::ActiveTask {
            task_id: task.task_id,
            env_id: task.env_id,
            stream_id: task.stream_id,
            function_name: task.function_name.clone(),
            task_type: task.task_type.clone(),
            cancel_token: None,
            original_request: request,
        }));

        task_message_should_execute(&db, &running, &coordinator, "worker-a", 10)
            .await
            .expect_err("an active running task must keep its queue delivery pending");

        let still_owned = Task::get(&db, &task.task_id).await.unwrap();
        assert_eq!(still_owned.worker_id.as_deref(), Some("worker-a"));
    }

    #[tokio::test]
    async fn adopted_task_redelivery_does_not_release_ownership() {
        let db = hot::db::test_db().await;
        let (_request, task, _, _, _) = make_task_request_and_row();
        // Adoption registration reconstructs the original request from the DB
        // row, which requires the env row to exist.
        Env::insert_env(
            &db,
            &task.env_id,
            &Uuid::now_v7(),
            "test-env",
            &Uuid::now_v7(),
        )
        .await
        .unwrap();
        insert_test_task(&db, &task).await;
        Task::mark_running(&db, &task.task_id).await.unwrap();
        Task::set_worker(&db, &task.task_id, "worker-a")
            .await
            .unwrap();
        let running = Task::get(&db, &task.task_id).await.unwrap();
        let coordinator = shutdown::TaskShutdownCoordinator::new();

        assert!(register_adopted_task(&coordinator, &db, &running).await);
        assert!(coordinator.is_task_active(&task.task_id));

        // The dead worker's un-ACKed queue message is XAUTOCLAIMed and
        // redelivered. The adopted task is registered, so the redelivery must
        // be withheld (deferred) instead of releasing ownership out from
        // under the live container.
        task_message_should_execute(&db, &running, &coordinator, "worker-a", 10)
            .await
            .expect_err("an adopted running task must keep its queue delivery pending");

        let still_owned = Task::get(&db, &task.task_id).await.unwrap();
        assert_eq!(still_owned.worker_id.as_deref(), Some("worker-a"));

        // Duplicate adoption of an in-flight task is refused.
        assert!(!register_adopted_task(&coordinator, &db, &running).await);

        coordinator.unregister_task(&task.task_id);
        assert!(!coordinator.is_task_active(&task.task_id));
    }

    #[tokio::test]
    async fn adoption_registration_falls_back_when_request_reconstruction_fails() {
        let db = hot::db::test_db().await;
        let (_request, task, _, _, _) = make_task_request_and_row();
        // No env row: `task_request_from_db_row` fails on every attempt.
        // Refusing adoption here would leave a live container unmanaged and
        // double-run via the zombie reaper, so registration must fall back
        // to a request synthesized from the task row alone.
        insert_test_task(&db, &task).await;
        Task::mark_running(&db, &task.task_id).await.unwrap();
        let running = Task::get(&db, &task.task_id).await.unwrap();
        let coordinator = shutdown::TaskShutdownCoordinator::new();

        assert!(register_adopted_task(&coordinator, &db, &running).await);
        assert!(coordinator.is_task_active(&task.task_id));

        // Refusal remains only for genuine duplicates.
        assert!(!register_adopted_task(&coordinator, &db, &running).await);
    }

    #[test]
    fn row_synthesized_request_carries_the_execution_identity() {
        let (_request, mut task, env_id, stream_id, build_id) = make_task_request_and_row();
        task.args = Some(serde_json::json!({"input": "ok"}));

        let request = synthesize_task_request_from_row(&task);

        assert_eq!(request.task_id, task.task_id.to_string());
        assert_eq!(request.function_name, task.function_name);
        assert_eq!(request.args, serde_json::json!({"input": "ok"}));
        assert_eq!(request.env_id, env_id.to_string());
        assert_eq!(request.stream_id, stream_id.to_string());
        assert_eq!(request.build_id, build_id.to_string());
        assert_eq!(request.timeout_ms, task.timeout_ms as u64);
        assert_eq!(request.task_type, task.task_type);
        // Enrichment is unavailable without DB lookups — explicitly absent,
        // not fabricated.
        assert_eq!(request.org_id, None);
        assert_eq!(request.project_id, None);
        assert_eq!(request.project_name, None);
    }

    #[tokio::test]
    async fn retry_with_backoff_returns_first_success_and_last_error() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let calls = AtomicUsize::new(0);
        let result = retry_with_backoff(3, std::time::Duration::from_millis(1), || {
            let attempt = calls.fetch_add(1, Ordering::SeqCst) + 1;
            async move {
                if attempt < 3 {
                    Err(format!("transient {}", attempt))
                } else {
                    Ok(attempt)
                }
            }
        })
        .await;
        assert_eq!(result, Ok(3), "a late success within budget must win");
        assert_eq!(calls.load(Ordering::SeqCst), 3);

        let calls = AtomicUsize::new(0);
        let result: Result<usize, String> =
            retry_with_backoff(3, std::time::Duration::from_millis(1), || {
                let attempt = calls.fetch_add(1, Ordering::SeqCst) + 1;
                async move { Err(format!("persistent {}", attempt)) }
            })
            .await;
        assert_eq!(result, Err("persistent 3".to_string()));
        assert_eq!(calls.load(Ordering::SeqCst), 3, "budget must be bounded");
    }

    #[tokio::test]
    async fn adopted_ownership_repair_stops_on_success_or_unownable_row() {
        let db = hot::db::test_db().await;

        // Running row: ownership lands and repair stops.
        let (_request, task, _, _, _) = make_task_request_and_row();
        insert_test_task(&db, &task).await;
        Task::mark_running(&db, &task.task_id).await.unwrap();
        assert!(try_adopt_task_ownership(&db, &task.task_id, "worker-b").await);
        let owned = Task::get(&db, &task.task_id).await.unwrap();
        assert_eq!(owned.worker_id.as_deref(), Some("worker-b"));
        assert!(
            owned.last_heartbeat_at.is_some(),
            "ownership must refresh the heartbeat so the reaper backs off"
        );

        // Terminal row: unownable, so repair must stop (true) without
        // rewriting ownership.
        let (_request, done, _, _, _) = make_task_request_and_row();
        insert_test_task(&db, &done).await;
        Task::mark_running(&db, &done.task_id).await.unwrap();
        assert!(
            Task::complete(&db, &done.task_id, &TaskStatus::Completed, None, None, None)
                .await
                .unwrap()
        );
        assert!(try_adopt_task_ownership(&db, &done.task_id, "worker-b").await);
        assert_eq!(
            Task::get(&db, &done.task_id).await.unwrap().worker_id,
            None,
            "a terminal row must not be re-owned"
        );

        // Transport error: repair must report not-resolved so the caller
        // (adoption retry loop / monitor poll loop) keeps trying.
        let closed_db = hot::db::test_db().await;
        if let DatabasePool::Sqlite(pool) = &closed_db {
            pool.close().await;
        }
        assert!(!try_adopt_task_ownership(&closed_db, &task.task_id, "worker-b").await);
    }

    #[tokio::test]
    async fn preflight_load_acks_only_a_genuinely_missing_task_row() {
        let db = hot::db::test_db().await;

        // A provably missing row is the only safe ACK-and-skip outcome.
        assert!(
            load_task_for_execution(&db, &Uuid::now_v7(), 25)
                .await
                .unwrap()
                .is_none()
        );

        let (_request, task, _, _, _) = make_task_request_and_row();
        insert_test_task(&db, &task).await;
        let loaded = load_task_for_execution(&db, &task.task_id, 25)
            .await
            .unwrap()
            .expect("an existing row must be loaded for execution");
        assert_eq!(loaded.task_id, task.task_id);
    }

    #[tokio::test]
    async fn preflight_load_defers_on_transport_error_instead_of_acking() {
        let closed_db = hot::db::test_db().await;
        if let DatabasePool::Sqlite(pool) = &closed_db {
            pool.close().await;
        }

        // Pool errors surface fast (well under DB_CALL_TIMEOUT); they must
        // defer the delivery for an infrastructure retry, never ACK it.
        let err = load_task_for_execution(&closed_db, &Uuid::now_v7(), 25)
            .await
            .expect_err("an unknown row state must defer the queue message");
        assert_eq!(err.backoff(), std::time::Duration::from_millis(25));
    }

    #[test]
    fn only_terminal_task_states_are_ack_eligible() {
        for status in [
            TaskStatus::Completed,
            TaskStatus::Failed,
            TaskStatus::Cancelled,
            TaskStatus::TimedOut,
        ] {
            assert!(task_status_is_terminal(status.as_id()));
        }
        assert!(!task_status_is_terminal(TaskStatus::Queued.as_id()));
        assert!(!task_status_is_terminal(TaskStatus::Running.as_id()));
        assert!(!task_status_is_terminal(i16::MAX));
    }

    #[tokio::test]
    async fn terminal_persistence_reports_success_and_database_failure() {
        let db = hot::db::test_db().await;
        let (_request, task, _, _, _) = make_task_request_and_row();
        insert_test_task(&db, &task).await;
        Task::mark_running(&db, &task.task_id).await.unwrap();

        assert!(
            persist_terminal_task(
                &db,
                &task.task_id,
                &TaskStatus::Completed,
                None,
                None,
                None,
                std::time::Duration::from_secs(1),
                2,
            )
            .await
        );
        assert_eq!(
            Task::get(&db, &task.task_id).await.unwrap().task_status_id,
            TaskStatus::Completed.as_id()
        );

        let (_request, cancelled_task, _, _, _) = make_task_request_and_row();
        insert_test_task(&db, &cancelled_task).await;
        assert!(Task::cancel(&db, &cancelled_task.task_id).await.unwrap());
        assert!(
            !persist_terminal_task(
                &db,
                &cancelled_task.task_id,
                &TaskStatus::Failed,
                None,
                None,
                None,
                std::time::Duration::from_secs(1),
                2,
            )
            .await,
            "a cancellation that wins the race must suppress stale completion"
        );
        assert_eq!(
            Task::get(&db, &cancelled_task.task_id)
                .await
                .unwrap()
                .task_status_id,
            TaskStatus::Cancelled.as_id()
        );

        let closed_db = hot::db::test_db().await;
        if let DatabasePool::Sqlite(pool) = &closed_db {
            pool.close().await;
        }
        assert!(
            !persist_terminal_task(
                &closed_db,
                &Uuid::now_v7(),
                &TaskStatus::Failed,
                None,
                None,
                None,
                std::time::Duration::from_millis(20),
                2,
            )
            .await
        );
    }

    #[tokio::test]
    async fn same_status_write_by_another_actor_is_not_mistaken_for_ours() {
        let db = hot::db::test_db().await;
        let (_request, task, _, _, _) = make_task_request_and_row();
        insert_test_task(&db, &task).await;
        Task::mark_running(&db, &task.task_id).await.unwrap();
        assert!(
            Task::set_worker(&db, &task.task_id, "worker-a")
                .await
                .unwrap()
        );

        // The zombie reaper wins the terminal race with the SAME status but
        // its own payload. `Task::complete` never modifies `worker_id`, so
        // the row still carries worker-a's ownership — status and ownership
        // alone cannot identify the writer.
        let reaper_error =
            task_failure_json("Task interrupted by worker crash (zombie reaper)", None);
        assert!(
            Task::complete(
                &db,
                &task.task_id,
                &TaskStatus::Failed,
                Some(&reaper_error),
                None,
                None,
            )
            .await
            .unwrap()
        );

        let our_error = task_failure_json("Task execution error: boom", None);
        assert!(
            !persist_terminal_task(
                &db,
                &task.task_id,
                &TaskStatus::Failed,
                Some(&our_error),
                None,
                Some("worker-a"),
                std::time::Duration::from_secs(1),
                2,
            )
            .await,
            "a same-status write by another actor must not count as our own persisted write"
        );
        let row = Task::get(&db, &task.task_id).await.unwrap();
        assert_eq!(
            row.result,
            Some(reaper_error),
            "the winner's payload must survive"
        );
    }

    #[tokio::test]
    async fn idempotent_replay_of_our_own_terminal_write_reports_persisted() {
        let db = hot::db::test_db().await;
        let (_request, task, _, _, _) = make_task_request_and_row();
        insert_test_task(&db, &task).await;
        Task::mark_running(&db, &task.task_id).await.unwrap();
        assert!(
            Task::set_worker(&db, &task.task_id, "worker-a")
                .await
                .unwrap()
        );

        // First attempt committed server-side but the client saw a timeout.
        let our_error = task_failure_json("Task timed out", None);
        assert!(
            Task::complete(
                &db,
                &task.task_id,
                &TaskStatus::TimedOut,
                Some(&our_error),
                None,
                Some("worker-a"),
            )
            .await
            .unwrap()
        );

        // The replay hits zero rows but the stored payload proves the earlier
        // write was ours, so the completion must still be reported durable.
        assert!(
            persist_terminal_task(
                &db,
                &task.task_id,
                &TaskStatus::TimedOut,
                Some(&our_error),
                None,
                Some("worker-a"),
                std::time::Duration::from_secs(1),
                2,
            )
            .await,
            "a replay of our own committed write must report persisted"
        );
    }

    #[test]
    fn json_equality_is_tolerant_of_number_variants_but_not_of_values() {
        // Postgres jsonb normalizes `1e16` to `10000000000000000`: the same
        // numeric value round-trips as a different serde_json Number variant.
        let ours = serde_json::json!({
            "$type": "::hot::task/Failure",
            "$val": {"msg": "boom", "elapsed": 1e16, "codes": [1e16, 2.5, 7]}
        });
        let stored = serde_json::json!({
            "$type": "::hot::task/Failure",
            "$val": {"msg": "boom", "elapsed": 10000000000000000u64, "codes": [10000000000000000u64, 2.5, 7]}
        });
        assert_ne!(ours, stored, "PartialEq is variant-sensitive (the bug)");
        assert!(json_numeric_tolerant_eq(&ours, &stored));
        assert!(json_numeric_tolerant_eq(&stored, &ours));

        // Negative integers across variants.
        assert!(json_numeric_tolerant_eq(
            &serde_json::json!(-4.0),
            &serde_json::json!(-4)
        ));

        // Same-variant integers beyond 2^53 must stay exact: the f64 path
        // would collapse adjacent values.
        assert!(!json_numeric_tolerant_eq(
            &serde_json::json!(u64::MAX),
            &serde_json::json!(u64::MAX - 1)
        ));

        // Genuinely different payloads must still differ.
        let other = serde_json::json!({
            "$type": "::hot::task/Failure",
            "$val": {"msg": "different", "elapsed": 1e16, "codes": [1e16, 2.5, 7]}
        });
        assert!(!json_numeric_tolerant_eq(&ours, &other));
        assert!(!json_numeric_tolerant_eq(
            &serde_json::json!({"a": 1}),
            &serde_json::json!({"a": 1, "b": 2})
        ));
        assert!(!json_numeric_tolerant_eq(
            &serde_json::json!([1, 2]),
            &serde_json::json!([1, 2, 3])
        ));
        assert!(!json_numeric_tolerant_eq(
            &serde_json::json!(1),
            &serde_json::json!("1")
        ));
    }

    #[tokio::test]
    async fn idempotent_replay_matches_despite_jsonb_number_normalization() {
        let db = hot::db::test_db().await;
        let (_request, task, _, _, _) = make_task_request_and_row();
        insert_test_task(&db, &task).await;
        Task::mark_running(&db, &task.task_id).await.unwrap();
        assert!(
            Task::set_worker(&db, &task.task_id, "worker-a")
                .await
                .unwrap()
        );

        // The earlier committed write stored the integer form (as Postgres
        // jsonb would after normalizing `1e16` text)...
        let stored_form = serde_json::json!({
            "$type": "::hot::task/Failure",
            "$val": {"msg": "boom", "elapsed_ns": 10000000000000000u64}
        });
        assert!(
            Task::complete(
                &db,
                &task.task_id,
                &TaskStatus::Failed,
                Some(&stored_form),
                None,
                Some("worker-a"),
            )
            .await
            .unwrap()
        );

        // ...while the in-memory payload we replay with holds the float
        // variant. The variant difference must not be mistaken for another
        // actor's write — that suppresses the real completion events.
        let our_form = serde_json::json!({
            "$type": "::hot::task/Failure",
            "$val": {"msg": "boom", "elapsed_ns": 1e16}
        });
        assert!(
            persist_terminal_task(
                &db,
                &task.task_id,
                &TaskStatus::Failed,
                Some(&our_form),
                None,
                Some("worker-a"),
                std::time::Duration::from_secs(1),
                2,
            )
            .await,
            "a replay differing only in JSON number variant must count as ours"
        );
    }

    #[tokio::test]
    async fn cooperative_cancel_persists_over_the_row_task_cancel_left_behind() {
        let db = hot::db::test_db().await;
        let (_request, task, _, _, _) = make_task_request_and_row();
        insert_test_task(&db, &task).await;
        Task::mark_running(&db, &task.task_id).await.unwrap();
        assert!(
            Task::set_worker(&db, &task.task_id, "worker-a")
                .await
                .unwrap()
        );

        // ::hot::task/cancel flips the row to Cancelled (result NULL) BEFORE
        // publishing the $cancel message, so the worker's cancel branch
        // always finds the row already Cancelled.
        assert!(Task::cancel(&db, &task.task_id).await.unwrap());
        assert!(
            Task::get(&db, &task.task_id)
                .await
                .unwrap()
                .result
                .is_none()
        );

        // The worker's cancellation persist must report durable success:
        // Task::cancel publishes no completion events, so persisted=false
        // here would suppress task:complete / RunStop / task:cancelled for
        // every cooperative cancellation.
        let cancellation = task_cancellation_json("Task cancelled via $cancel message", None);
        assert!(
            persist_terminal_task(
                &db,
                &task.task_id,
                &TaskStatus::Cancelled,
                Some(&cancellation),
                None,
                Some("worker-a"),
                std::time::Duration::from_secs(1),
                2,
            )
            .await,
            "a pre-Cancelled row is idempotent success for the worker's cancellation write"
        );

        // End to end: complete_task_with_event must publish the completion.
        let publisher =
            StreamPubSub::new(hot::stream::StreamPubSubType::Memory, None, false).unwrap();
        let mut env_events = publisher.subscribe_env(task.env_id).await.unwrap();
        assert!(
            complete_task_with_event(
                &db,
                &publisher,
                &task.task_id,
                task.env_id,
                task.stream_id,
                &task.function_name,
                &task.task_type,
                TaskStatus::Cancelled,
                Some(&cancellation),
                None,
                Some("worker-a"),
            )
            .await,
            "cooperative cancellation must emit its completion events"
        );
        let event = tokio::time::timeout(std::time::Duration::from_secs(1), env_events.next())
            .await
            .expect("a cooperative cancellation must publish task:complete")
            .expect("subscription should stay open");
        match event {
            EnvEvent::TaskComplete {
                task_id, status, ..
            } => {
                assert_eq!(task_id, task.task_id);
                assert_eq!(status, "cancelled");
            }
            other => panic!("expected TaskComplete, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn fenced_terminal_write_is_suppressed_after_ownership_loss() {
        let db = hot::db::test_db().await;
        let (_request, task, _, _, _) = make_task_request_and_row();
        insert_test_task(&db, &task).await;
        Task::mark_running(&db, &task.task_id).await.unwrap();
        assert!(
            Task::set_worker(&db, &task.task_id, "worker-a")
                .await
                .unwrap()
        );
        assert!(
            Task::release_worker(&db, &task.task_id, "worker-a")
                .await
                .unwrap()
        );

        let our_error = task_failure_json("Task timed out", None);
        assert!(
            !persist_terminal_task(
                &db,
                &task.task_id,
                &TaskStatus::TimedOut,
                Some(&our_error),
                None,
                Some("worker-a"),
                std::time::Duration::from_secs(1),
                2,
            )
            .await,
            "a fenced write must not land after ownership was released"
        );
        let row = Task::get(&db, &task.task_id).await.unwrap();
        assert_eq!(row.task_status_id, TaskStatus::Running.as_id());
        assert_eq!(row.result, None);
    }

    #[tokio::test]
    async fn unpersisted_completion_returns_false_so_alerts_and_retries_are_suppressed() {
        let db = hot::db::test_db().await;
        let publisher =
            StreamPubSub::new(hot::stream::StreamPubSubType::Memory, None, false).unwrap();

        // Cancellation already won the terminal race: the failure completion
        // must report not-persisted, which is the callers' signal to skip
        // publish_task_alert and maybe_retry_task.
        let (_request, task, _, _, _) = make_task_request_and_row();
        insert_test_task(&db, &task).await;
        assert!(Task::cancel(&db, &task.task_id).await.unwrap());
        let error = task_failure_json("boom", None);
        assert!(
            !complete_task_with_event(
                &db,
                &publisher,
                &task.task_id,
                task.env_id,
                task.stream_id,
                &task.function_name,
                &task.task_type,
                TaskStatus::Failed,
                Some(&error),
                None,
                None,
            )
            .await,
            "a lost terminal race must tell callers to suppress alerts and retries"
        );
        assert_eq!(
            Task::get(&db, &task.task_id).await.unwrap().task_status_id,
            TaskStatus::Cancelled.as_id()
        );

        // Positive control: an owned running row persists and reports true.
        let (_request, owned, _, _, _) = make_task_request_and_row();
        insert_test_task(&db, &owned).await;
        Task::mark_running(&db, &owned.task_id).await.unwrap();
        assert!(
            Task::set_worker(&db, &owned.task_id, "worker-a")
                .await
                .unwrap()
        );
        assert!(
            complete_task_with_event(
                &db,
                &publisher,
                &owned.task_id,
                owned.env_id,
                owned.stream_id,
                &owned.function_name,
                &owned.task_type,
                TaskStatus::Failed,
                Some(&error),
                None,
                Some("worker-a"),
            )
            .await
        );
    }

    #[tokio::test]
    async fn reaper_skips_zombie_candidates_actively_managed_by_this_worker() {
        let db = hot::db::test_db().await;
        let publisher =
            StreamPubSub::new(hot::stream::StreamPubSubType::Memory, None, false).unwrap();
        let queue_name = format!("{{hot:task}}-reaper-skip-{}", Uuid::now_v7());
        let queue = ProcessingQueue::<TaskRequest>::new(
            QueueType::Memory,
            queue_name,
            None,
            Serialization::Json,
        )
        .unwrap();

        // Adopted-container shape: a running row whose heartbeat is a day
        // stale (ownership repair has not landed yet) but which this worker
        // actively manages in-process.
        let (request, task, _, _, _) = make_task_request_and_row();
        insert_test_task(&db, &task).await;
        Task::mark_running(&db, &task.task_id).await.unwrap();
        assert!(
            Task::set_worker(&db, &task.task_id, "dead-worker")
                .await
                .unwrap()
        );
        assert!(
            Task::release_worker(&db, &task.task_id, "dead-worker")
                .await
                .unwrap()
        );
        assert!(
            Task::find_zombie_tasks(&db, ZOMBIE_HEARTBEAT_STALE_SECS)
                .await
                .unwrap()
                .iter()
                .any(|candidate| candidate.task_id == task.task_id),
            "precondition: the stale running row must be a zombie candidate"
        );

        let coordinator = shutdown::TaskShutdownCoordinator::new();
        assert!(coordinator.try_register_task(shutdown::ActiveTask {
            task_id: task.task_id,
            env_id: task.env_id,
            stream_id: task.stream_id,
            function_name: task.function_name.clone(),
            task_type: task.task_type.clone(),
            cancel_token: None,
            original_request: request,
        }));

        reap_zombie_tasks(&db, &publisher, &queue, &coordinator).await;
        let row = Task::get(&db, &task.task_id).await.unwrap();
        assert_eq!(
            row.task_status_id,
            TaskStatus::Running.as_id(),
            "a coordinator-active task must not be reaped even with a stale heartbeat"
        );

        coordinator.unregister_task(&task.task_id);
        reap_zombie_tasks(&db, &publisher, &queue, &coordinator).await;
        let row = Task::get(&db, &task.task_id).await.unwrap();
        assert_eq!(
            row.task_status_id,
            TaskStatus::Failed.as_id(),
            "once unmanaged, the same stale row must be reaped"
        );
    }

    #[tokio::test]
    async fn retry_check_skips_rows_that_are_not_terminal_failures() {
        let db = hot::db::test_db().await;
        let queue_name = format!("{{hot:task}}-retry-guard-{}", Uuid::now_v7());
        let queue = ProcessingQueue::<TaskRequest>::new(
            QueueType::Memory,
            queue_name,
            None,
            Serialization::Json,
        )
        .unwrap();
        let retry_options = serde_json::json!({"retry": {"attempts": 2, "delay": 100}});

        // A Cancelled row must never spawn a retry, even with retry config —
        // the call-site gate can be bypassed by a stale caller, so
        // maybe_retry_task re-reads the row itself.
        let (request, mut task, _, _, _) = make_task_request_and_row();
        task.options = Some(retry_options.clone());
        insert_test_task(&db, &task).await;
        Task::mark_running(&db, &task.task_id).await.unwrap();
        assert!(Task::cancel(&db, &task.task_id).await.unwrap());
        maybe_retry_task(&db, &queue, &task.task_id, &request).await;
        assert_eq!(
            Task::get_count_by_env(&db, &task.env_id).await.unwrap(),
            1,
            "a cancelled row must not insert a retry task"
        );

        // Positive control: a Failed row with retry budget inserts exactly
        // one retry row.
        let (request, mut failed, _, _, _) = make_task_request_and_row();
        failed.options = Some(retry_options);
        insert_test_task(&db, &failed).await;
        Task::mark_running(&db, &failed.task_id).await.unwrap();
        let error = task_failure_json("boom", None);
        assert!(
            Task::complete(
                &db,
                &failed.task_id,
                &TaskStatus::Failed,
                Some(&error),
                None,
                None
            )
            .await
            .unwrap()
        );
        maybe_retry_task(&db, &queue, &failed.task_id, &request).await;
        assert_eq!(
            Task::get_count_by_env(&db, &failed.env_id).await.unwrap(),
            2,
            "a terminal failure with retry budget must insert its retry task"
        );
    }

    #[tokio::test]
    async fn retry_skips_enqueue_when_retry_row_already_exists() {
        let db = hot::db::test_db().await;
        let queue_name = format!("{{hot:task}}-retry-dup-{}", Uuid::now_v7());
        let queue = ProcessingQueue::<TaskRequest>::new(
            QueueType::Memory,
            queue_name,
            None,
            Serialization::Json,
        )
        .unwrap();
        let retry_options = serde_json::json!({"retry": {"attempts": 2, "delay": 0}});

        let (request, mut task, _, _, _) = make_task_request_and_row();
        task.options = Some(retry_options.clone());
        insert_test_task(&db, &task).await;
        Task::mark_running(&db, &task.task_id).await.unwrap();
        let error = task_failure_json("boom", None);
        assert!(
            Task::complete(
                &db,
                &task.task_id,
                &TaskStatus::Failed,
                Some(&error),
                None,
                None
            )
            .await
            .unwrap()
        );

        // Another writer (e.g. the zombie reaper, or a crashed earlier
        // attempt) already created the retry row for attempt 1 but never
        // enqueued it.
        let row = Task::get(&db, &task.task_id).await.unwrap();
        assert!(
            Task::insert_retry(&db, &Uuid::now_v7(), &row, 1, chrono::Utc::now())
                .await
                .unwrap()
        );

        // maybe_retry_task must treat the duplicate insert as already-retried
        // and skip its enqueue — the existing row is recovered by
        // reconcile_queued_tasks, never by a second retry row.
        maybe_retry_task(&db, &queue, &task.task_id, &request).await;
        // Sleep past the (clamped, 100ms min) retry delay so a buggy delayed
        // enqueue would have landed before the assertion.
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert_eq!(
            queue.len().await.unwrap(),
            0,
            "a duplicate retry must not be enqueued"
        );
        assert_eq!(
            Task::get_count_by_env(&db, &task.env_id).await.unwrap(),
            2,
            "no second retry row may be created"
        );

        // Positive control: with no pre-existing retry row the same path
        // inserts the retry row and enqueues exactly one request.
        let (request, mut fresh, _, _, _) = make_task_request_and_row();
        fresh.options = Some(retry_options);
        insert_test_task(&db, &fresh).await;
        Task::mark_running(&db, &fresh.task_id).await.unwrap();
        assert!(
            Task::complete(
                &db,
                &fresh.task_id,
                &TaskStatus::Failed,
                Some(&error),
                None,
                None
            )
            .await
            .unwrap()
        );
        maybe_retry_task(&db, &queue, &fresh.task_id, &request).await;
        assert_eq!(Task::get_count_by_env(&db, &fresh.env_id).await.unwrap(), 2);
        tokio::time::timeout(std::time::Duration::from_secs(2), queue.claim_blocking())
            .await
            .expect("a fresh failure with budget must enqueue its retry")
            .unwrap()
            .expect("memory queue claim should yield the enqueued retry");
    }

    #[tokio::test]
    async fn zombie_retry_skips_enqueue_when_retry_row_already_exists() {
        let db = hot::db::test_db().await;
        let queue_name = format!("{{hot:task}}-zombie-dup-{}", Uuid::now_v7());
        let queue = ProcessingQueue::<TaskRequest>::new(
            QueueType::Memory,
            queue_name,
            None,
            Serialization::Json,
        )
        .unwrap();
        let retry_options = serde_json::json!({"retry": {"attempts": 2, "delay": 0}});

        let (_request, mut task, _, _, _) = make_task_request_and_row();
        task.options = Some(retry_options.clone());
        insert_test_task(&db, &task).await;
        let row = Task::get(&db, &task.task_id).await.unwrap();

        // The failure path already created the budget retry for attempt 1
        // (row.retry_attempt + 1) — the reaper racing it must not double it.
        assert!(
            Task::insert_retry(
                &db,
                &Uuid::now_v7(),
                &row,
                row.retry_attempt + 1,
                chrono::Utc::now()
            )
            .await
            .unwrap()
        );

        maybe_retry_zombie_task(&db, &row, &queue).await;
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert_eq!(
            queue.len().await.unwrap(),
            0,
            "a duplicate zombie retry must not be enqueued"
        );
        assert_eq!(
            Task::get_count_by_env(&db, &task.env_id).await.unwrap(),
            2,
            "no second retry row may be created"
        );

        // Positive control: a fresh zombie with budget inserts + enqueues.
        let (_request, mut fresh, _, _, _) = make_task_request_and_row();
        fresh.options = Some(retry_options);
        insert_test_task(&db, &fresh).await;
        let fresh_row = Task::get(&db, &fresh.task_id).await.unwrap();
        maybe_retry_zombie_task(&db, &fresh_row, &queue).await;
        assert_eq!(Task::get_count_by_env(&db, &fresh.env_id).await.unwrap(), 2);
        tokio::time::timeout(std::time::Duration::from_secs(2), queue.claim_blocking())
            .await
            .expect("a fresh zombie with budget must enqueue its retry")
            .unwrap()
            .expect("memory queue claim should yield the enqueued retry");
    }

    #[tokio::test]
    async fn container_setup_timeout_honors_user_retry_budget() {
        let db = hot::db::test_db().await;
        let publisher =
            StreamPubSub::new(hot::stream::StreamPubSubType::Memory, None, false).unwrap();
        let queue_name = format!("{{hot:task}}-setup-timeout-retry-{}", Uuid::now_v7());
        let queue = ProcessingQueue::<TaskRequest>::new(
            QueueType::Memory,
            queue_name,
            None,
            Serialization::Json,
        )
        .unwrap();

        // With retry budget: a timeout during SETUP (e.g. bundle mounts) is a
        // terminal failure of this attempt and must spawn exactly one retry
        // row, the same as a timeout one await later in the runtime arms.
        let (request, mut task, _, _, _) = make_task_request_and_row();
        task.options = Some(serde_json::json!({"retry": {"attempts": 2, "delay": 0}}));
        insert_test_task(&db, &task).await;
        Task::mark_running(&db, &task.task_id).await.unwrap();
        assert!(
            Task::set_worker(&db, &task.task_id, "worker-a")
                .await
                .unwrap()
        );
        finish_container_setup_timeout(
            &db,
            &publisher,
            &queue,
            &request,
            &task.task_id,
            task.env_id,
            task.stream_id,
            None,
            &task.function_name,
            &task.task_type,
            "worker-a",
            "bundle mount preparation",
            None,
            None,
            (),
        )
        .await;
        let row = Task::get(&db, &task.task_id).await.unwrap();
        assert_eq!(
            row.task_status_id,
            TaskStatus::TimedOut.as_id(),
            "the fenced TimedOut write must persist before any retry"
        );
        assert_eq!(
            Task::get_count_by_env(&db, &task.env_id).await.unwrap(),
            2,
            "a setup timeout with retry budget must insert exactly one retry row"
        );

        // Without budget: terminal TimedOut only, no retry row.
        let (request, no_budget, _, _, _) = make_task_request_and_row();
        insert_test_task(&db, &no_budget).await;
        Task::mark_running(&db, &no_budget.task_id).await.unwrap();
        assert!(
            Task::set_worker(&db, &no_budget.task_id, "worker-a")
                .await
                .unwrap()
        );
        finish_container_setup_timeout(
            &db,
            &publisher,
            &queue,
            &request,
            &no_budget.task_id,
            no_budget.env_id,
            no_budget.stream_id,
            None,
            &no_budget.function_name,
            &no_budget.task_type,
            "worker-a",
            "file-server setup",
            None,
            None,
            (),
        )
        .await;
        assert_eq!(
            Task::get_count_by_env(&db, &no_budget.env_id)
                .await
                .unwrap(),
            1,
            "a setup timeout without retry budget must not spawn retries"
        );
    }

    #[test]
    fn container_billing_measures_execution_window_not_setup_gap() {
        // A task that spent ~5s in worker-side setup (bundle download and
        // extract, file-server start) before dispatching a ~0.5s workload:
        // the billing clock must anchor at dispatch, not at claim.
        let now = std::time::Instant::now();
        let claimed_at = now
            .checked_sub(std::time::Duration::from_millis(5_500))
            .expect("monotonic clock is past process start");
        let execution_start = now
            .checked_sub(std::time::Duration::from_millis(500))
            .expect("monotonic clock is past process start");

        let billed = billable_execution_ms(execution_start);
        assert!(
            billed >= 500,
            "the workload window itself must be billed (got {billed})"
        );
        assert!(
            billed < 5_000,
            "worker-side setup before dispatch must not be billed as user compute (got {billed})"
        );
        // The DEADLINE stays anchored at claim (covered by
        // claimed_container_setup_obeys_shared_task_deadline); this only
        // pins that the two anchors are genuinely distinct clocks.
        assert!(claimed_at.elapsed() >= std::time::Duration::from_millis(5_500));
    }

    #[tokio::test]
    async fn timed_out_container_bills_pre_cleanup_window_only() {
        // A workload that ran ~200ms before its timeout fired, followed by a
        // slow infrastructure teardown (log collection, kill/remove; the Kata
        // arm's leaked-VM reaping can burn up to KATA_TIMEOUT_CLEANUP_ENVELOPE
        // = 120s): every execution arm snapshots the billable window via
        // bill_before_cleanup the moment execution ends, so teardown time is
        // never charged as user compute or reported in `duration-ms`.
        let execution_start = std::time::Instant::now()
            .checked_sub(std::time::Duration::from_millis(200))
            .expect("monotonic clock is past process start");

        let cleanup_ran = std::sync::atomic::AtomicBool::new(false);
        let (billed, ()) = bill_before_cleanup(execution_start, async {
            // Simulated slow teardown, strictly after the snapshot.
            tokio::time::sleep(std::time::Duration::from_millis(150)).await;
            cleanup_ran.store(true, std::sync::atomic::Ordering::SeqCst);
        })
        .await;

        assert!(
            cleanup_ran.load(std::sync::atomic::Ordering::SeqCst),
            "cleanup must still run to completion after the snapshot"
        );
        assert!(
            billed >= 200,
            "the pre-timeout workload window must be billed (got {billed})"
        );
        let after_cleanup = billable_execution_ms(execution_start);
        assert!(
            after_cleanup >= billed + 150,
            "the clock kept running through the ~150ms cleanup (billed={billed}, \
             after={after_cleanup}); the snapshot preceding it is what keeps \
             teardown out of the charge"
        );
    }

    #[test]
    fn kata_billing_prefers_executor_reported_workload_end_over_fallback() {
        // The Kata executor stamps `workload_ended_at` the moment the in-VM
        // wait resolves (or its internal timeout fires), BEFORE its internal
        // teardown (log finalize, kill, VM/snapshot/CNI cleanup). The lib.rs
        // arm's own post-await snapshot only lands after that teardown, so
        // billing must prefer the executor-reported instant. The Kata
        // runtime path itself is cfg-gated (linux + kata); this exercises
        // the portable selection plumbing.
        let now = std::time::Instant::now();
        let execution_start = now
            .checked_sub(std::time::Duration::from_millis(300))
            .expect("monotonic clock is past process start");
        // Workload finished 100ms in; the remaining ~200ms up to `now` play
        // the role of executor-internal teardown.
        let workload_ended_at = execution_start + std::time::Duration::from_millis(100);
        // The fallback snapshot was taken after that teardown.
        let fallback_ms = billable_execution_ms(execution_start);
        assert!(
            fallback_ms >= 300,
            "fallback covers teardown (got {fallback_ms})"
        );

        let billed = billable_ms_preferring_executor_window(
            execution_start,
            Some(workload_ended_at),
            fallback_ms,
        );
        assert_eq!(
            billed, 100,
            "the executor-reported window must win over the post-teardown fallback"
        );

        // No executor-reported end (outer-timeout cancellation, setup
        // failure): the fallback snapshot is used verbatim.
        assert_eq!(
            billable_ms_preferring_executor_window(execution_start, None, fallback_ms),
            fallback_ms,
            "without an executor-reported end the fallback must be used unchanged"
        );

        // A (theoretical) end before the start saturates to zero rather
        // than going negative.
        let before_start = execution_start
            .checked_sub(std::time::Duration::from_millis(50))
            .expect("monotonic clock is past process start");
        assert_eq!(
            billable_ms_preferring_executor_window(execution_start, Some(before_start), 999),
            0,
            "an end before the start must saturate to zero"
        );
    }

    #[tokio::test]
    async fn container_terminal_persist_stores_billable_snapshot_not_claim_to_persist() {
        // `start_time` is stamped at CLAIM, so Task::complete's default
        // stop-start duration folds worker-side setup and teardown into the
        // row's `duration_ms` — which feeds the re-read `task:complete`
        // event and the task-minutes quota. Container terminal writes must
        // instead persist the billable execution-window snapshot verbatim.
        let db = hot::db::test_db().await;
        let (_request, task, _, _, _) = make_task_request_and_row();
        insert_test_task(&db, &task).await;
        Task::mark_running(&db, &task.task_id).await.unwrap();

        // Let a measurable claim-to-persist gap accrue so the stop-start
        // computation could not coincidentally equal the snapshot below.
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;

        let snapshot_ms = 123_456i64;
        let result = serde_json::json!({"exit-code": 0, "duration-ms": snapshot_ms});
        assert!(
            persist_terminal_task(
                &db,
                &task.task_id,
                &TaskStatus::Completed,
                Some(&result),
                Some(snapshot_ms),
                None,
                std::time::Duration::from_secs(1),
                1,
            )
            .await
        );

        let row = Task::get(&db, &task.task_id).await.unwrap();
        assert_eq!(
            row.duration_ms,
            Some(snapshot_ms),
            "the persisted duration must be the billable snapshot, not the \
             claim-to-persist stop-start span"
        );
    }

    fn bundle_test_parent(label: &str) -> std::path::PathBuf {
        let parent = std::env::temp_dir().join(format!(
            "hot-bundle-test-{}-{}",
            label,
            Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&parent).unwrap();
        parent
    }

    fn make_complete_temp_dir(parent: &std::path::Path, name: &str) -> std::path::PathBuf {
        let dir = parent.join(format!("{}{}", BUNDLE_EXTRACT_TEMP_PREFIX, name));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("manifest.hot"), b"{}").unwrap();
        dir
    }

    #[test]
    fn bundle_install_rename_race_leaves_exactly_one_valid_dir() {
        let parent = bundle_test_parent("race");
        let final_dir = parent.join("build-race");
        let winner = make_complete_temp_dir(&parent, "build-race-a");
        let loser = make_complete_temp_dir(&parent, "build-race-b");

        // While attempts are still writing (temp dirs exist), the final path
        // must not exist — it can only appear via the atomic rename, so a
        // partial dir is never observable at the final path.
        assert!(!final_dir.exists());

        install_extracted_bundle(&winner, &final_dir).unwrap();
        assert!(bundle_extract_is_complete(&final_dir));

        // The loser's rename hits the installed dir; it must clean up its
        // own temp dir and treat the complete final dir as success.
        install_extracted_bundle(&loser, &final_dir).unwrap();
        assert!(
            !loser.exists(),
            "the losing attempt must remove its temp dir"
        );
        assert!(bundle_extract_is_complete(&final_dir));

        let leftovers: Vec<String> = std::fs::read_dir(&parent)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            leftovers,
            vec!["build-race".to_string()],
            "exactly one valid dir must remain after the race"
        );
        std::fs::remove_dir_all(&parent).unwrap();
    }

    #[test]
    fn bundle_install_heals_legacy_partial_final_dir() {
        let parent = bundle_test_parent("heal");
        let final_dir = parent.join("build-poisoned");
        // Older workers extracted straight into the final path; a process
        // death mid-extract left a partial dir with no manifest.hot marker.
        std::fs::create_dir_all(final_dir.join("hot/src")).unwrap();
        std::fs::write(final_dir.join("hot/src/partial.hot"), b"x").unwrap();
        assert!(!bundle_extract_is_complete(&final_dir));

        let temp = make_complete_temp_dir(&parent, "build-poisoned-fresh");
        install_extracted_bundle(&temp, &final_dir).unwrap();
        assert!(bundle_extract_is_complete(&final_dir));
        assert!(
            !final_dir.join("hot/src/partial.hot").exists(),
            "the poisoned partial content must be replaced, not merged into"
        );
        std::fs::remove_dir_all(&parent).unwrap();
    }

    #[test]
    fn stale_bundle_extract_temps_swept_only_past_max_age() {
        let parent = bundle_test_parent("sweep");
        let stale = make_complete_temp_dir(&parent, "build-old");
        let installed = parent.join("build-live");
        std::fs::create_dir_all(&installed).unwrap();
        std::fs::write(installed.join("manifest.hot"), b"{}").unwrap();

        // Zero max age: every temp counts as stale; installed final dirs are
        // never swept regardless.
        sweep_stale_bundle_extract_temps(&parent, std::time::Duration::ZERO);
        assert!(!stale.exists(), "an expired temp dir must be swept");
        assert!(installed.exists(), "final build dirs must never be swept");

        // A fresh temp under the real max age survives (in-flight extract).
        let fresh = make_complete_temp_dir(&parent, "build-inflight");
        sweep_stale_bundle_extract_temps(&parent, BUNDLE_EXTRACT_TEMP_MAX_AGE);
        assert!(fresh.exists(), "an in-flight temp dir must not be swept");
        std::fs::remove_dir_all(&parent).unwrap();
    }

    #[test]
    fn failed_bundle_extraction_leaves_no_temp_or_final_dir() {
        let parent = bundle_test_parent("badzip");
        let final_dir = parent.join("build-bad");

        let err = extract_bundle_to_dir(b"not a zip archive", &final_dir).unwrap_err();
        assert!(err.contains("Failed to extract bundle"), "got: {err}");
        assert!(!final_dir.exists(), "a failed extraction must not install");
        let temps: Vec<String> = std::fs::read_dir(&parent)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with(BUNDLE_EXTRACT_TEMP_PREFIX))
            .collect();
        assert!(
            temps.is_empty(),
            "a failed extraction must remove its temp dir, found {temps:?}"
        );
        std::fs::remove_dir_all(&parent).unwrap();
    }

    #[tokio::test]
    async fn bundle_extraction_lock_is_single_flight_per_build() {
        let build_a = Uuid::now_v7();
        let first = bundle_extract_lock(build_a).await;
        let same_build = bundle_extract_lock(build_a).await;
        let other_build = bundle_extract_lock(Uuid::now_v7()).await;

        assert!(Arc::ptr_eq(&first, &same_build));
        assert!(!Arc::ptr_eq(&first, &other_build));

        let guard = first.lock().await;
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), same_build.lock())
                .await
                .is_err(),
            "a second local attempt for the same build must wait for the in-flight extraction"
        );
        drop(guard);
        let follower =
            tokio::time::timeout(std::time::Duration::from_millis(100), same_build.lock())
                .await
                .expect("the follower must proceed once the first extraction completes");
        drop(follower);

        drop(first);
        drop(same_build);
        let _trigger_cleanup = bundle_extract_lock(Uuid::now_v7()).await;
        let locks = BUNDLE_EXTRACT_LOCKS.get().unwrap().lock().await;
        assert!(
            !locks.contains_key(&build_a),
            "inactive build locks must not accumulate for the worker lifetime"
        );
    }

    #[test]
    fn blocking_execution_capacity_bounds_detached_vm_threads() {
        assert_eq!(blocking_execution_capacity(0, -1), 2);
        assert_eq!(blocking_execution_capacity(4, -1), 8);
        assert_eq!(blocking_execution_capacity(4, 0), 4);
        assert_eq!(blocking_execution_capacity(4, 2), 4);
        assert_eq!(blocking_execution_capacity(4, 6), 6);
        assert_eq!(
            blocking_execution_capacity(4, i64::MAX),
            Semaphore::MAX_PERMITS
        );
        // Must never panic at startup for any configured value.
        let _ = Semaphore::new(blocking_execution_capacity(4, i64::MAX));
    }

    #[tokio::test]
    async fn timeout_cleanup_is_itself_wall_clock_bounded() {
        let cleanup = std::future::pending::<()>();
        assert!(
            !bounded_cleanup(std::time::Duration::from_millis(20), cleanup).await,
            "a wedged runtime cleanup must return control at its own deadline"
        );
    }

    #[tokio::test]
    async fn claimed_container_setup_obeys_shared_task_deadline() {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(20);
        let result = await_container_setup(deadline, std::future::pending::<()>()).await;
        assert!(
            result.is_err(),
            "post-claim setup must not outlive the task's wall-clock deadline"
        );
    }

    #[tokio::test]
    async fn blocking_slot_saturation_defers_the_queue_message() {
        let slots = Arc::new(Semaphore::new(1));
        let _held = acquire_blocking_execution_slot(
            Arc::clone(&slots),
            std::time::Duration::from_secs(1),
            std::time::Duration::from_millis(9),
        )
        .await
        .unwrap();

        let err = acquire_blocking_execution_slot(
            Arc::clone(&slots),
            std::time::Duration::from_millis(20),
            std::time::Duration::from_millis(9),
        )
        .await
        .expect_err("a saturated blocking-execution cap must defer the queue delivery");

        assert_eq!(err.backoff(), std::time::Duration::from_millis(9));
    }

    #[tokio::test]
    async fn detached_execution_holds_its_blocking_slot_until_the_thread_exits() {
        let slots = Arc::new(Semaphore::new(1));
        let permit = acquire_blocking_execution_slot(
            Arc::clone(&slots),
            std::time::Duration::from_secs(1),
            std::time::Duration::from_millis(1),
        )
        .await
        .unwrap();

        let (entered_tx, entered_rx) = std::sync::mpsc::channel::<()>();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let (exited_tx, exited_rx) = std::sync::mpsc::channel::<()>();
        let handle = tokio::task::spawn_blocking(move || {
            // Mirrors process_code_task: the permit lives inside the blocking
            // closure, so its lifetime is the THREAD's, not the JoinHandle's.
            let blocking_execution_permit = permit;
            entered_tx.send(()).unwrap();
            release_rx.recv().unwrap(); // wedged VM standing in
            drop(blocking_execution_permit);
            exited_tx.send(()).unwrap();
        });
        entered_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();

        // The timeout arm drops the JoinHandle; the detached thread must keep
        // the slot occupied so admission observes the wedged execution.
        drop(handle);
        assert_eq!(slots.available_permits(), 0);
        assert!(
            acquire_blocking_execution_slot(
                Arc::clone(&slots),
                std::time::Duration::from_millis(20),
                std::time::Duration::from_millis(1),
            )
            .await
            .is_err(),
            "a detached thread must still count against the execution cap"
        );

        // Once the thread actually exits, the slot frees.
        release_tx.send(()).unwrap();
        exited_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        assert_eq!(slots.available_permits(), 1);
    }

    #[tokio::test]
    async fn container_slot_helper_enforces_configured_peak_concurrency() {
        let semaphore = Arc::new(Semaphore::new(2));
        let active = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let peak = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut handles = Vec::new();

        for _ in 0..8 {
            let semaphore = Arc::clone(&semaphore);
            let active = Arc::clone(&active);
            let peak = Arc::clone(&peak);
            handles.push(tokio::spawn(async move {
                let _permit = acquire_container_slot(
                    semaphore,
                    std::time::Duration::from_secs(1),
                    std::time::Duration::from_millis(1),
                )
                .await
                .unwrap();
                let now = active.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                peak.fetch_max(now, std::sync::atomic::Ordering::SeqCst);
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                active.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            }));
        }
        for handle in handles {
            handle.await.unwrap();
        }

        assert_eq!(peak.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn container_slot_timeout_returns_infrastructure_retry() {
        let semaphore = Arc::new(Semaphore::new(1));
        let _held = acquire_container_slot(
            Arc::clone(&semaphore),
            std::time::Duration::from_secs(1),
            std::time::Duration::from_millis(7),
        )
        .await
        .unwrap();

        let err = acquire_container_slot(
            semaphore,
            std::time::Duration::from_millis(20),
            std::time::Duration::from_millis(7),
        )
        .await
        .expect_err("a saturated container cap must defer the queue delivery");

        assert_eq!(err.backoff(), std::time::Duration::from_millis(7));
    }

    #[tokio::test]
    async fn usage_calculation_lock_is_single_flight_per_org() {
        let org_a = Uuid::now_v7();
        let org_b = Uuid::now_v7();
        let first = usage_stats_org_lock(org_a).await;
        let same_org = usage_stats_org_lock(org_a).await;
        let other_org = usage_stats_org_lock(org_b).await;

        assert!(Arc::ptr_eq(&first, &same_org));
        assert!(!Arc::ptr_eq(&first, &other_org));

        let guard = first.lock().await;
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), same_org.lock())
                .await
                .is_err(),
            "a follower for the same org must wait for the in-flight calculation"
        );
        drop(guard);
        let follower_guard =
            tokio::time::timeout(std::time::Duration::from_millis(100), same_org.lock())
                .await
                .expect("follower should proceed once the first calculation completes");
        drop(follower_guard);

        drop(first);
        drop(same_org);
        let _trigger_cleanup = usage_stats_org_lock(Uuid::now_v7()).await;
        let locks = USAGE_STATS_LOCKS.get().unwrap().lock().await;
        assert!(
            !locks.contains_key(&org_a),
            "inactive org locks must not accumulate for the worker lifetime"
        );
    }

    #[tokio::test]
    async fn task_timing_uses_creation_to_workload_and_workload_to_completion() {
        let db = hot::db::test_db().await;
        let task_id = Uuid::now_v7();
        let env_id = Uuid::now_v7();
        let stream_id = Uuid::now_v7();
        let build_id = Uuid::now_v7();
        Task::insert(
            &db,
            &task_id,
            &env_id,
            &stream_id,
            &build_id,
            None,
            "::app/timed",
            None,
            None,
            "code",
            60_000,
            None,
        )
        .await
        .unwrap();

        let claimed_at = chrono::Utc::now() - chrono::Duration::milliseconds(200);
        let created_at = claimed_at - chrono::Duration::milliseconds(100);
        let workload_started_at = claimed_at + chrono::Duration::milliseconds(100);
        let hot::db::DatabasePool::Sqlite(pool) = &db else {
            panic!("test_db should use SQLite");
        };
        sqlx::query("UPDATE task SET created_at = ? WHERE task_id = ?")
            .bind(created_at)
            .bind(task_id)
            .execute(pool)
            .await
            .unwrap();
        Task::set_timing(
            &db,
            &task_id,
            &serde_json::json!({"claimed_at": claimed_at.to_rfc3339()}),
        )
        .await
        .unwrap();
        Task::mark_running(&db, &task_id).await.unwrap();
        persist_container_timings(
            &db,
            &task_id,
            claimed_at,
            7,
            &executor::ContainerTimings {
                slot_wait_ms: 3,
                image_pull_ms: 20,
                runtime_start_ms: 10,
                execution_ms: 40,
                logs_collect_ms: 5,
                workload_started_at: Some(workload_started_at),
                workload_ended_at: None,
            },
        )
        .await;
        Task::complete(&db, &task_id, &TaskStatus::Completed, None, None, None)
            .await
            .unwrap();
        finalize_task_timing(&db, &task_id).await;

        let task = Task::get(&db, &task_id).await.unwrap();
        let timing = task.timing.unwrap();
        assert_eq!(timing["waiting_ms"], 200);
        assert_eq!(timing["capacity_wait_ms"], 10);
        assert_eq!(timing["image_pull_ms"], 20);
        assert_eq!(timing["runtime_start_ms"], 10);
        assert_eq!(timing["worker_preparation_ms"], 60);
        assert_eq!(timing["workload_execution_ms"], 40);
        assert_eq!(timing["logs_collect_ms"], 5);
        assert!(timing["execution_ms"].as_i64().unwrap() >= 0);
        assert_eq!(
            timing["total_ms"].as_i64().unwrap(),
            timing["waiting_ms"].as_i64().unwrap() + timing["execution_ms"].as_i64().unwrap()
        );
    }

    #[test]
    fn test_validate_task_request_rejects_env_mismatch() {
        let (request, task, _env_id, stream_id, build_id) = make_task_request_and_row();

        let err =
            validate_task_request_matches_db(&request, &task, Uuid::now_v7(), stream_id, build_id)
                .unwrap_err();

        assert!(err.contains("env_id mismatch"));
    }

    #[test]
    fn test_validate_task_request_rejects_function_mismatch() {
        let (mut request, task, env_id, stream_id, build_id) = make_task_request_and_row();
        request.function_name = "::app/other".to_string();

        let err = validate_task_request_matches_db(&request, &task, env_id, stream_id, build_id)
            .unwrap_err();

        assert!(err.contains("function_name mismatch"));
    }

    #[test]
    fn test_capacity_fairness_config_accepts_supported_none_mode() {
        let conf = val!({
            "task": {
                "capacity-fairness": "none",
            },
        });

        assert!(validate_task_fairness_conf(&conf).is_ok());
    }

    #[test]
    fn test_capacity_fairness_config_rejects_unimplemented_modes() {
        let conf = val!({
            "task": {
                "capacity-fairness": "org",
            },
        });

        let err = validate_task_fairness_conf(&conf).unwrap_err().to_string();
        assert!(err.contains("Unsupported task.capacity-fairness"));
    }

    #[test]
    fn test_task_orphan_idle_validation_accepts_memory_queue() {
        assert!(validate_task_orphan_idle_ms(QueueType::Memory, 1).is_ok());
    }

    #[test]
    fn test_task_orphan_idle_validation_rejects_redis_below_lease_ttl() {
        let err = validate_task_orphan_idle_ms(QueueType::Redis, 60_000)
            .unwrap_err()
            .to_string();

        assert!(err.contains("Redis task lease TTL"));
    }

    #[test]
    fn test_task_orphan_idle_validation_accepts_redis_at_lease_ttl() {
        let lease_ttl_ms = task_lease::DEFAULT_LEASE_TTL.as_millis() as u64;

        assert!(validate_task_orphan_idle_ms(QueueType::Redis, lease_ttl_ms).is_ok());
    }

    #[test]
    fn container_shell_defaults_stop_on_error_without_xtrace() {
        assert!(CONTAINER_SCRIPT_PRELUDE.contains("set -e"));
        assert!(!CONTAINER_SCRIPT_PRELUDE.contains("set -x"));
        assert!(!CONTAINER_SCRIPT_PRELUDE.contains("set -ex"));
        assert_eq!(CONTAINER_SHELL_FLAGS, "-ec");
        assert!(!CONTAINER_SHELL_FLAGS.contains('x'));
    }

    #[test]
    fn test_task_failure_json_simple() {
        let result = task_failure_json("something broke", None);
        assert_eq!(result["$type"], "::hot::task/Failure");
        assert_eq!(result["$val"]["msg"], "something broke");
        assert!(result["$val"]["err"].is_null());
    }

    #[test]
    fn test_task_failure_json_with_details() {
        let details = serde_json::json!({"exit-code": 1, "stderr": "segfault"});
        let result = task_failure_json("container crashed", Some(details.clone()));
        assert_eq!(result["$type"], "::hot::task/Failure");
        assert_eq!(result["$val"]["msg"], "container crashed");
        assert_eq!(result["$val"]["err"]["exit-code"], 1);
        assert_eq!(result["$val"]["err"]["stderr"], "segfault");
    }

    #[test]
    fn test_task_cancellation_json_simple() {
        let result = task_cancellation_json("user cancelled", None);
        assert_eq!(result["$type"], "::hot::task/Cancellation");
        assert_eq!(result["$val"]["msg"], "user cancelled");
        assert!(result["$val"]["data"].is_null());
    }

    #[test]
    fn test_task_cancellation_json_with_data() {
        let data = serde_json::json!({"reason": "timeout", "elapsed_ms": 30000});
        let result = task_cancellation_json("task timed out", Some(data));
        assert_eq!(result["$type"], "::hot::task/Cancellation");
        assert_eq!(result["$val"]["msg"], "task timed out");
        assert_eq!(result["$val"]["data"]["reason"], "timeout");
    }

    #[test]
    fn test_normalize_val_to_task_failure_already_typed() {
        let typed_val: Val = serde_json::from_value(serde_json::json!({
            "$type": "::hot::run/Failure",
            "$val": {"msg": "run error", "err": null}
        }))
        .unwrap();
        let result = normalize_val_to_task_failure(&typed_val);
        assert_eq!(result["$type"], "::hot::run/Failure");
        assert_eq!(result["$val"]["msg"], "run error");
    }

    #[test]
    fn test_normalize_val_to_task_failure_task_typed() {
        let typed_val: Val = serde_json::from_value(serde_json::json!({
            "$type": "::hot::task/Failure",
            "$val": {"msg": "task error", "err": {"detail": "x"}}
        }))
        .unwrap();
        let result = normalize_val_to_task_failure(&typed_val);
        assert_eq!(result["$type"], "::hot::task/Failure");
        assert_eq!(result["$val"]["err"]["detail"], "x");
    }

    #[test]
    fn test_normalize_val_to_task_failure_bare_string() {
        let bare_val = Val::from("connection refused");
        let result = normalize_val_to_task_failure(&bare_val);
        assert_eq!(result["$type"], "::hot::task/Failure");
        assert_eq!(result["$val"]["msg"], "connection refused");
    }

    #[test]
    fn test_normalize_val_to_task_failure_bare_object() {
        let bare_val: Val =
            serde_json::from_value(serde_json::json!({"code": 500, "message": "internal error"}))
                .unwrap();
        let result = normalize_val_to_task_failure(&bare_val);
        assert_eq!(result["$type"], "::hot::task/Failure");
        assert_eq!(result["$val"]["msg"], "Task failed");
        assert_eq!(result["$val"]["err"]["code"], 500);
    }

    #[test]
    fn test_normalize_val_to_task_failure_null() {
        let null_val = Val::Null;
        let result = normalize_val_to_task_failure(&null_val);
        assert_eq!(result["$type"], "::hot::task/Failure");
        assert_eq!(result["$val"]["msg"], "Task failed");
    }

    #[test]
    fn test_classify_task_terminal_result_unwraps_result_err_failure() {
        let failure_val: Val = serde_json::from_value(serde_json::json!({
            "$type": "::hot::task/Failure",
            "$val": {"msg": "task error", "err": {"detail": "x"}}
        }))
        .unwrap();
        let result_err = Val::err(failure_val);

        let (status, json, alert) = classify_task_terminal_result(&result_err)
            .expect("Result.Err(Failure) should be terminal");

        assert_eq!(status, TaskStatus::Failed);
        assert_eq!(alert, "task:failed");
        assert_eq!(json["$type"], "::hot::task/Failure");
        assert_eq!(json["$val"]["err"]["detail"], "x");
    }

    #[test]
    fn test_classify_task_terminal_result_unwraps_result_err_cancellation() {
        let cancellation_val: Val = serde_json::from_value(serde_json::json!({
            "$type": "::hot::task/Cancellation",
            "$val": {"msg": "stopped", "data": {"reason": "user"}}
        }))
        .unwrap();
        let result_err = Val::err(cancellation_val);

        let (status, json, alert) = classify_task_terminal_result(&result_err)
            .expect("Result.Err(Cancellation) should be terminal");

        assert_eq!(status, TaskStatus::Cancelled);
        assert_eq!(alert, "task:cancelled");
        assert_eq!(json["$type"], "::hot::task/Cancellation");
        assert_eq!(json["$val"]["data"]["reason"], "user");
    }

    #[test]
    fn test_classify_task_terminal_result_direct_failure() {
        let failure_val: Val = serde_json::from_value(serde_json::json!({
            "$type": "::hot::task/Failure",
            "$val": {"msg": "panic", "err": {"panic": true}}
        }))
        .unwrap();

        let (status, json, alert) =
            classify_task_terminal_result(&failure_val).expect("typed Failure should be terminal");

        assert_eq!(status, TaskStatus::Failed);
        assert_eq!(alert, "task:failed");
        assert_eq!(json["$type"], "::hot::task/Failure");
        assert_eq!(json["$val"]["err"]["panic"], true);
    }

    #[test]
    fn test_classify_task_terminal_result_success_value() {
        assert!(classify_task_terminal_result(&Val::from(42)).is_none());
    }

    #[test]
    fn test_task_failure_json_roundtrip_serde() {
        let original = task_failure_json("test error", Some(serde_json::json!(42)));
        let json_str = serde_json::to_string(&original).unwrap();
        let deserialized: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        assert_eq!(deserialized, original);
    }
}

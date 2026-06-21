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
use hot::db::{self, DatabasePool, Task, TaskStatus};
use hot::env::retry::RetryConfig;
use hot::lang::cache::bytecode_cache::{BytecodeCache, CachedBytecode};
use hot::lang::emitter::EngineEventEmitter;
use hot::lang::event::{EventPublisher, ExecutionContext};
use hot::lang::hot::task::TaskRequest;
use hot::queue::{ProcessingQueue, Queue, QueueInfrastructureError, QueueType};
use hot::stream::{
    EnvEvent, EnvPublisher, StreamEvent, StreamNext, StreamPubSub, StreamPublisher,
    StreamSubscriberFactory,
};
use hot::val::Val;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{Mutex, Semaphore, mpsc};
use uuid::Uuid;

type UsageStatsCache =
    Arc<Mutex<HashMap<Uuid, (std::time::Instant, hot::db::subscription::OrgUsageStats)>>>;

const USAGE_STATS_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(60);

/// Maximum time to spend on per-task post-execution cleanup (DB writes,
/// stream publishes, retry enqueue). When this fires we log + drop the rest
/// of the cleanup so a stuck DB pool can't pin the worker on a single task.
const POST_TASK_CLEANUP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// Per-DB-call timeout used inside cleanup helpers. A single hung query must
/// not be allowed to consume the entire `POST_TASK_CLEANUP_TIMEOUT` budget,
/// so we apply a tighter per-call ceiling.
const DB_CALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// Maximum time to spend tearing down a container (Docker remove or Kata
/// shim/VM kill). After this we log and move on; the executor itself or the
/// background reaper is responsible for finishing the cleanup.
const CONTAINER_KILL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

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

/// Run the task worker.
pub async fn run(config: TaskWorkerConfig) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
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

    tracing::info!(
        "Starting hot_task_worker (code_max_concurrent={} requested={} cpu_limit={} memory_limit={:?} memory_limit_mb={:?}, container_max_concurrent={} requested={} explicit={} memory_limit={} disk_limit={} resource_budget={}MB mem / {}MB disk recovery_reserved_slots={} backend={}, box_defaults={}MB mem / {}MB disk)",
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
        default_container_memory_mb,
        default_container_disk_mb,
    );

    let queue_name = config
        .queue_name
        .clone()
        .unwrap_or_else(|| "hot:task".to_string());
    // Stable consumer name (host + pid) so XINFO CONSUMERS doesn't grow
    // unbounded across restarts of the same logical task worker. See
    // notes/ideas/QUEUE_OPTIMIZATION.md Phase 4b.
    let host = std::env::var("HOSTNAME").unwrap_or_else(|_| "host".to_string());
    let pid = std::process::id();
    // Tasks legitimately run for hours (and we cap at ~7 days) so 24h is the
    // window beyond which any backlog entry is almost certainly stale —
    // either the run was already cancelled, the user moved on, or the task
    // would now race with whatever new state replaced it. See
    // `RedisStreamQueue::with_startup_window` for full semantics.
    let task_startup_window = std::time::Duration::from_secs(24 * 60 * 60);

    let task_queue = ProcessingQueue::<TaskRequest>::new_with_cluster(
        config.queue_type,
        queue_name,
        config.redis_uri.clone(),
        config.redis_cluster,
        config.serialization,
    )?
    .with_consumer_name(format!("{}-{}-task", host, pid))
    .with_read_batch_size(queue_claim_max)
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

    // Split concurrency: high-limit semaphore for code tasks, resource budget for containers
    let code_semaphore = Arc::new(Semaphore::new(code_max));
    let container_budget = resource_budget::ResourceBudget::new(
        container_budget.memory_budget_mb,
        container_budget.disk_budget_mb,
    );

    let container_executor = Arc::new(
        executor::BoxExecutor::new(
            config.container_backend,
            container_max,
            30,
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
                    // Don't refuse to start the worker over this — the
                    // existing in-process `try_register_task` dedup still
                    // covers same-pod duplication, and the queue layer's
                    // `refill_lock` covers same-instance refill races. We
                    // log loudly and keep going with a noop lease.
                    tracing::error!(
                        error = %e,
                        "Failed to construct Redis task lease; falling back to NoopTaskLease (cross-pod dedup disabled)"
                    );
                    Arc::new(task_lease::NoopTaskLease)
                }
            }
        }
    };

    // Shutdown coordinator — fixed 30s drain followed by cancel + infra-retry
    // re-enqueue + DELCONSUMER. End-to-end fits comfortably within ECS 120s
    // stopTimeout. See `shutdown.rs` module docs for the full timeline.
    let coordinator = Arc::new(shutdown::TaskShutdownCoordinator::new());

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
        &worker_id,
    )
    .await;

    // Reap zombie tasks (code tasks with stale heartbeat, or container tasks with no container)
    reap_zombie_tasks(&db, &stream_publisher, &task_queue_arc).await;

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
                reap_zombie_tasks(&reaper_db, &reaper_pub, &reaper_queue).await;
            }
        });
    }

    // Monitor any adopted containers in background poll tasks
    for (adopted_task_id, adopted_container_id) in adopted {
        let db = Arc::clone(&db);
        let sp = Arc::clone(&stream_publisher);
        let tq = Arc::clone(&task_queue_arc);
        let ex = Arc::clone(&container_executor);
        let coord = Arc::clone(&coordinator);
        tokio::spawn(async move {
            monitor_adopted_container(
                adopted_task_id,
                adopted_container_id,
                &db,
                &sp,
                &tq,
                &ex,
                &coord,
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

    // Phase 5: cap on in-flight spawned task processors. Without this, every
    // permit acquired from `code_semaphore` (inside `process_task`) is
    // released only after the previous task completes — defeating the
    // purpose of concurrency. With this outer cap we spawn up to N concurrent
    // `process_blocking` futures, each of which dequeues, processes, and
    // ACKs one message. The inner `code_semaphore` / `container_budget` still
    // gate by *resource type* within each spawned future.
    //
    // Use the larger derived resource-class budget as the outer claim cap.
    // This prevents Redis PEL from filling with more work than local resources
    // can execute, while the inner code/container gates still enforce the
    // per-resource limits.
    let inflight_semaphore = Arc::new(Semaphore::new(queue_claim_max));

    loop {
        let permit = tokio::select! {
            biased;
            _ = &mut shutdown => {
                tracing::info!("Shutting down task worker");
                coordinator.initiate_shutdown(&db, &stream_publisher, &task_queue_arc).await;
                break;
            }
            permit = Arc::clone(&inflight_semaphore).acquire_owned() => {
                match permit {
                    Ok(p) => p,
                    Err(_) => {
                        // Semaphore closed — should never happen in normal
                        // operation, but bail safely if it does.
                        tracing::error!("In-flight semaphore closed; exiting task loop");
                        break;
                    }
                }
            }
        };

        if coordinator.is_shutting_down() {
            drop(permit);
            break;
        }

        // Spawn a one-shot worker that holds the permit for the lifetime of
        // one message. The permit is released when this future drops (either
        // normally on completion, or on panic via JoinError handling). If
        // the queue is empty `process_blocking` parks for the BLOCK window
        // and returns Ok(None); the permit is released and the main loop
        // simply acquires another one and re-arms the wait.
        let tq = Arc::clone(&task_queue_arc);
        let db_c = Arc::clone(&db);
        let stream_pub_c = Arc::clone(&stream_publisher);
        let cache_c = Arc::clone(&bytecode_cache);
        let code_sem_c = Arc::clone(&code_semaphore);
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

        tokio::spawn(async move {
            let _permit = permit; // released on drop
            let result = tq
                .process_blocking(|request: TaskRequest| {
                    let db = Arc::clone(&db_c);
                    let tq2 = Arc::clone(&tq);
                    let stream_pub = stream_pub_c;
                    let cache = cache_c;
                    let code_sem = code_sem_c;
                    let ctr_budget = ctr_budget_c;
                    let conf = conf_c;
                    let ep = ep_c;
                    let executor = executor_c;
                    let vol_base = vol_base_c;
                    let defaults = defaults_c;
                    let usage_cache = usage_cache_c;
                    let coord = coord_c;
                    let lease = lease_c;
                    let wid = wid_c;
                    async move {
                        process_task(
                            request,
                            db,
                            tq2,
                            stream_pub,
                            cache,
                            code_sem,
                            ctr_budget,
                            conf,
                            ep,
                            executor,
                            vol_base,
                            defaults,
                            usage_cache,
                            coord,
                            lease,
                            wid,
                        )
                        .await
                    }
                })
                .await;

            match result {
                Ok(Some(())) | Ok(None) => {}
                Err(e) => {
                    tracing::error!("Task processing error: {}", e);
                }
            }
        });
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
) {
    let zombies = match Task::find_zombie_tasks(db, ZOMBIE_HEARTBEAT_STALE_SECS).await {
        Ok(tasks) => tasks,
        Err(e) => {
            tracing::warn!("Failed to query zombie tasks: {}", e);
            return;
        }
    };

    // Also check for legacy running tasks without any heartbeat (pre-migration)
    let legacy = match Task::find_running_without_heartbeat(db).await {
        Ok(tasks) => tasks,
        Err(e) => {
            tracing::warn!("Failed to query legacy running tasks: {}", e);
            Vec::new()
        }
    };

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

        if let Err(e) =
            Task::complete(db, &task.task_id, &db::TaskStatus::Failed, Some(&error)).await
        {
            tracing::error!(task_id = %task.task_id, "Failed to reap zombie task: {}", e);
            continue;
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

    if let Err(e) = Task::insert_retry(db, &new_task_id, task, next_attempt, next_retry_at).await {
        tracing::error!(
            task_id = %task.task_id,
            new_task_id = %new_task_id,
            "Failed to insert retry for zombie task: {}", e,
        );
        return;
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
                tracing::info!(new_task_id = %new_task_id, attempt = next_attempt, "Zombie retry enqueued after delay");
            }
        });
    } else if let Err(e) = task_queue.enqueue(retry_request).await {
        tracing::error!(new_task_id = %new_task_id, "Failed to enqueue zombie retry: {}", e);
    } else {
        tracing::info!(new_task_id = %new_task_id, attempt = next_attempt, "Zombie retry enqueued immediately");
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

/// Process a single task request.
#[allow(clippy::too_many_arguments)]
async fn process_task(
    request: TaskRequest,
    db: Arc<DatabasePool>,
    task_queue: Arc<ProcessingQueue<TaskRequest>>,
    stream_publisher: Arc<StreamPubSub>,
    bytecode_cache: Arc<BytecodeCache>,
    code_semaphore: Arc<Semaphore>,
    container_budget: Arc<resource_budget::ResourceBudget>,
    worker_conf: Val,
    event_publisher: Option<Arc<dyn EventPublisher>>,
    container_executor: Arc<executor::BoxExecutor>,
    data_vol_base: Arc<std::path::PathBuf>,
    box_defaults: Arc<box_limits::BoxDefaults>,
    usage_stats_cache: UsageStatsCache,
    coordinator: Arc<shutdown::TaskShutdownCoordinator>,
    task_lease: Arc<dyn task_lease::TaskLease>,
    worker_id: String,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let task_id = Uuid::parse_str(&request.task_id)?;
    let stream_id = Uuid::parse_str(&request.stream_id)?;
    let env_id = Uuid::parse_str(&request.env_id)?;
    let build_id = Uuid::parse_str(&request.build_id)?;
    let timeout_ms = request.timeout_ms.max(1000);

    tracing::info!(
        task_id = %task_id,
        function = %request.function_name,
        task_type = %request.task_type,
        "Processing task"
    );

    let task = match Task::get(&db, &task_id).await {
        Ok(task) => task,
        Err(e) => {
            tracing::error!(
                task_id = %task_id,
                "Rejecting task queue message with no matching DB row: {}", e,
            );
            return Ok(());
        }
    };

    if task.task_status_id == TaskStatus::Cancelled.as_id() {
        tracing::info!(task_id = %task_id, "Task already cancelled, skipping execution");
        return Ok(());
    }

    if let Err(e) = validate_task_request_matches_db(&request, &task, env_id, stream_id, build_id) {
        tracing::error!(
            task_id = %task_id,
            "Rejecting task queue message that does not match DB row: {}", e,
        );
        return Ok(());
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
    let infra_retry_backoff_ms = worker_conf
        .get_int_or_default("queue.infra-retry-backoff-ms", 1_000)
        .max(0) as u64;
    let _lease_guard = match task_lease
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

    let result = if request.task_type == "container" {
        // Container tasks use resource budget (memory + disk) instead of semaphore
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
            worker_id,
        )
        .await
    } else {
        // Code tasks use high-limit semaphore
        let _permit = code_semaphore.acquire().await?;

        if let Err(e) = Task::mark_running(&db, &task_id).await {
            tracing::error!(task_id = %task_id, "Failed to mark task running: {}", e);
        }
        if let Err(e) = Task::set_worker(&db, &task_id, &worker_id).await {
            tracing::error!(task_id = %task_id, "Failed to set worker_id: {}", e);
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
        )
        .await
    };

    coordinator.unregister_task(&task_id);
    result
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
        )
        .await;
        publish_task_alert(&db, org_id, env_id, &task_id, "task:failed", &error).await;
        return Ok(());
    }

    let cmd: Option<Vec<String>> = {
        let script = args
            .get("script")
            .and_then(|v| v.as_str())
            .map(String::from);

        if let Some(script_body) = script {
            // `script` field: write to /tmp/hot-run.sh, execute with sh -ex.
            // `set -ex` is prepended so every command is traced to stderr and
            // execution stops on the first non-zero exit.
            // `mkdir -p /data` ensures the disk-backed working directory exists.
            let full_script = format!("#!/bin/sh\nset -ex\nmkdir -p /data\n{}", script_body.trim());
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
            // `cmd` field: pass through as-is but inject -ex when the user is
            // already using `sh -c "..."` so they also get tracing for free.
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
                            "-exc".to_string(),
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
                    )
                    .await;
                    publish_task_alert(&db, org_id, env_id, &task_id, "task:failed", &error).await;
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

            let cached_usage = {
                let cache = usage_stats_cache.lock().await;
                cache
                    .get(oid)
                    .filter(|(ts, _)| ts.elapsed() < USAGE_STATS_CACHE_TTL)
                    .map(|(_, stats)| stats.clone())
            };
            let usage_result = if let Some(stats) = cached_usage {
                Ok(stats)
            } else {
                let result = hot::db::subscription::OrgUsageStats::calculate(
                    &db,
                    oid,
                    period_start,
                    features.call_retention_days(),
                )
                .await;
                if let Ok(ref stats) = result {
                    let mut cache = usage_stats_cache.lock().await;
                    cache.insert(*oid, (std::time::Instant::now(), stats.clone()));
                }
                result
            };
            if let Ok(usage) = usage_result {
                // CUS (compute units) per month — hard gate for free plans
                let cus_limit = features.compute_units_per_month();
                if cus_limit > 0 && usage.compute_units >= cus_limit && is_free {
                    let msg = format!(
                        "Monthly compute unit limit reached ({}/{}). Upgrade your plan for more compute.",
                        usage.compute_units, cus_limit
                    );
                    let error = task_failure_json(&msg, None);
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
                    )
                    .await;
                    publish_task_alert(&db, org_id, env_id, &task_id, "task:failed", &error).await;
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
                        )
                        .await;
                        publish_task_alert(&db, org_id, env_id, &task_id, "task:failed", &error)
                            .await;
                        return Ok(());
                    }
                }
            }
        }
    }

    // -- Acquire resource budget --
    let resource_mem = limits.memory_mb + limits.tmp_size_mb;
    let resource_disk = limits.disk_size_mb;
    let resource_guard = match budget
        .acquire(
            resource_mem,
            resource_disk,
            std::time::Duration::from_secs(30),
        )
        .await
    {
        Ok(guard) => guard,
        Err(e) => {
            let error = task_failure_json(&e.to_string(), None);
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
            )
            .await;
            publish_task_alert(&db, org_id, env_id, &task_id, "task:failed", &error).await;
            return Ok(());
        }
    };

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
            tracing::warn!(task_id = %task_id, "Data volume creation failed, continuing without /data/: {}", e);
            None
        }
    };

    if let Err(e) = Task::mark_running(&db, &task_id).await {
        tracing::error!(task_id = %task_id, "Failed to mark task running: {}", e);
    }
    if let Err(e) = Task::set_worker(&db, &task_id, &worker_id).await {
        tracing::error!(task_id = %task_id, "Failed to set worker_id for container task: {}", e);
    }

    emit_task_started(
        &stream_publisher,
        task_id,
        env_id,
        stream_id,
        &function_name,
        &request.task_type,
    )
    .await;

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

    // -- Start per-task file server for hotbox CLI access --
    // For Kata, the file server start is deferred to a pre-start hook
    // because it needs the VM's vsock UDS path (only available after create_task).
    let is_kata = std::cfg_select! {
        all(target_os = "linux", feature = "kata") =>
            matches!(executor.backend(), executor::Backend::Kata),
        _ => false,
    };

    let file_server_handle = if !is_kata {
        match start_file_server_for_task(
            &task_id,
            &data_vol_base,
            org_id,
            env_id,
            user_id,
            Some(run_id),
            &db,
            &worker_conf,
            executor.backend(),
        )
        .await
        {
            Ok(handle) => Some(handle),
            Err(e) => {
                tracing::error!("File server start failed: {}", e);
                let error = task_failure_json(
                    "File server failed to start — container cannot access hot:// storage",
                    None,
                );
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
                )
                .await;
                publish_task_alert(&db, org_id, env_id, &task_id, "task:failed", &error).await;
                drop(resource_guard);
                if let Some(vol) = data_volume {
                    vol.cleanup().await;
                }
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
            )
            .await;
            publish_task_alert(&db, org_id, env_id, &task_id, "task:failed", &error).await;
            drop(resource_guard);
            if let Some(vol) = data_volume {
                vol.cleanup().await;
            }
            return Ok(());
        }

        // We need the extracted bundle on disk to source the bind mounts
        // from. For container tasks this is the *first* code path that
        // touches the bundle on disk, so do an explicit extraction here
        // (load_bytecode_bundle's path is shared via ensure_bundle_extracted).
        let extract_dir = match org_id {
            Some(oid) => {
                match ensure_bundle_extracted(&build_id, &oid, &env_id, &worker_conf).await {
                    Ok(p) => p,
                    Err(e) => {
                        let error = task_failure_json(
                            &format!("failed to extract bundle for mounts: {}", e),
                            None,
                        );
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
                        )
                        .await;
                        publish_task_alert(&db, org_id, env_id, &task_id, "task:failed", &error)
                            .await;
                        drop(resource_guard);
                        if let Some(vol) = data_volume {
                            vol.cleanup().await;
                        }
                        return Ok(());
                    }
                }
            }
            None => {
                let error = task_failure_json(
                    "container 'mounts' requires an org_id on the task request",
                    None,
                );
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
                )
                .await;
                publish_task_alert(&db, org_id, env_id, &task_id, "task:failed", &error).await;
                drop(resource_guard);
                if let Some(vol) = data_volume {
                    vol.cleanup().await;
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
            tracing::info!(
                task_id = %task_id,
                container = %container_path,
                resource = %resource_path,
                readonly = readonly,
                "box.mount.bound"
            );
        }

        if let Some(err_msg) = mount_error {
            let error = task_failure_json(&err_msg, None);
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
            )
            .await;
            publish_task_alert(&db, org_id, env_id, &task_id, "task:failed", &error).await;
            drop(resource_guard);
            if let Some(vol) = data_volume {
                vol.cleanup().await;
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
                    )
                    .await;
                    drop(resource_guard);
                    if let Some(vol) = data_volume {
                        vol.cleanup().await;
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
                )
                .await;
                drop(resource_guard);
                if let Some(vol) = data_volume {
                    vol.cleanup().await;
                }
                return Ok(());
            }
        };
        let fs_storage = match hot::file_storage::file_storage_from_config(&worker_conf).await {
            Ok(s) => Arc::from(s),
            Err(e) => {
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
                )
                .await;
                drop(resource_guard);
                if let Some(vol) = data_volume {
                    vol.cleanup().await;
                }
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

    tracing::debug!(
        task_id = %task_id,
        image = %image,
        size = %limits.size,
        timeout_secs = limits.timeout_secs,
        memory_mb = limits.memory_mb,
        disk_size_mb = limits.disk_size_mb,
        has_data_volume = data_volume.is_some(),
        has_file_server = file_server_handle.is_some() || is_kata,
        network = limits.network,
        backend = %executor.backend(),
        "Running container task"
    );

    let total_timeout = std::time::Duration::from_millis(timeout_ms);
    let start = std::time::Instant::now();

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
    let execution_result = if matches!(executor.backend(), executor::Backend::Docker) {
        let mut timings = executor::ContainerTimings::default();
        match executor
            .create_and_start(
                &image,
                cmd,
                env,
                Some(&trace_id),
                Some(&limits),
                extras_ref,
                &mut timings,
            )
            .await
        {
            Ok(container_id) => {
                // Persist container_id so a new worker can adopt it
                if let Err(e) = Task::set_container_id(&db, &task_id, &container_id).await {
                    tracing::warn!(task_id = %task_id, "Failed to store container_id: {}", e);
                }

                // Poll-based monitoring loop
                let deadline = tokio::time::Instant::now() + total_timeout;
                let poll_interval = std::time::Duration::from_secs(2);
                let exit_code = loop {
                    tokio::time::sleep(poll_interval).await;
                    match executor.inspect_status(&container_id).await {
                        Ok(Some(code)) => break Some(code),
                        Ok(None) => {
                            if tokio::time::Instant::now() >= deadline {
                                break None; // timed out
                            }
                        }
                        Err(e) => {
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
                            if tokio::time::Instant::now() >= deadline {
                                break None;
                            }
                        }
                    }
                };
                timings.execution_ms = start.elapsed().as_millis() as i64 - timings.image_pull_ms;

                match exit_code {
                    Some(code) => {
                        let logs_start = std::time::Instant::now();
                        let (stdout, stderr) = executor
                            .collect_logs(&container_id)
                            .await
                            .unwrap_or_default();
                        timings.logs_collect_ms = logs_start.elapsed().as_millis() as i64;
                        executor.remove_container(&container_id).await;

                        Ok(Ok((
                            executor::ContainerOutput {
                                exit_code: code,
                                stdout,
                                stderr,
                                container_id,
                                timed_out: false,
                                oom_killed: code == 137,
                            },
                            timings,
                        )))
                    }
                    None => {
                        // Timed out
                        let logs_start = std::time::Instant::now();
                        let (stdout, stderr) = executor
                            .collect_logs(&container_id)
                            .await
                            .unwrap_or_default();
                        timings.logs_collect_ms = logs_start.elapsed().as_millis() as i64;
                        kill_and_remove_with_timeout(&executor, &container_id, Some(&task_id))
                            .await;

                        Ok(Ok((
                            executor::ContainerOutput {
                                exit_code: -1,
                                stdout,
                                stderr,
                                container_id,
                                timed_out: true,
                                oom_killed: false,
                            },
                            timings,
                        )))
                    }
                }
            }
            Err(e) => Ok(Err(e)),
        }
    } else {
        // Kata: use atomic execute_with_extras (phased not supported)
        tokio::time::timeout(
            total_timeout,
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
        .await
    };

    let duration_ms = start.elapsed().as_millis() as i64;

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
    // unmount can pin the task worker, so apply the same wall-clock cap.
    if let Some(vol) = data_volume
        && tokio::time::timeout(CONTAINER_KILL_TIMEOUT, vol.cleanup())
            .await
            .is_err()
    {
        tracing::error!(task_id = %task_id, "data_volume cleanup timed out — backing file may leak");
    }

    // Release resource budget
    drop(resource_guard);

    match execution_result {
        Ok(Ok((output, timings))) => {
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

            complete_task_with_event(
                &db,
                &stream_publisher,
                &task_id,
                env_id,
                stream_id,
                &function_name,
                &request.task_type,
                status.clone(),
                Some(&result_json),
            )
            .await;

            if status == TaskStatus::Failed || status == TaskStatus::TimedOut {
                publish_task_alert(&db, org_id, env_id, &task_id, "task:failed", &result_json)
                    .await;
            }

            if compute_units > 0 {
                check_cus_thresholds(&db, org_id, env_id, &usage_stats_cache).await;
            }

            tracing::info!(
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
        Ok(Err(e)) => {
            // Infrastructure failure — don't charge CUS
            let is_infra_failure = matches!(
                &e,
                ExecutorError::ImagePull(_)
                    | ExecutorError::Connection(_)
                    | ExecutorError::SlotTimeout(_)
                    | ExecutorError::ImageNotAllowed(_)
                    | ExecutorError::Create(_)
            );
            let compute_units = if is_infra_failure {
                0
            } else {
                limits.size.compute_units(duration_ms)
            };
            let user_message = if is_infra_failure {
                match &e {
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
                    _ => "Container infrastructure error".to_string(),
                }
            } else {
                e.to_string()
            };
            let error = task_failure_json(
                &user_message,
                Some(serde_json::json!({
                    "duration-ms": duration_ms,
                    "size": limits.size.as_str(),
                    "compute-units": compute_units,
                    "infra-failure": is_infra_failure,
                })),
            );
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
            )
            .await;
            publish_task_alert(&db, org_id, env_id, &task_id, "task:failed", &error).await;
            if compute_units > 0 {
                check_cus_thresholds(&db, org_id, env_id, &usage_stats_cache).await;
            }
            maybe_retry_task(&db, &task_queue, &task_id, &request).await;
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
            complete_task_with_event(
                &db,
                &stream_publisher,
                &task_id,
                env_id,
                stream_id,
                &function_name,
                &request.task_type,
                TaskStatus::TimedOut,
                Some(&error),
            )
            .await;
            publish_task_alert(&db, org_id, env_id, &task_id, "task:failed", &error).await;
            if compute_units > 0 {
                check_cus_thresholds(&db, org_id, env_id, &usage_stats_cache).await;
            }
            maybe_retry_task(&db, &task_queue, &task_id, &request).await;
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
                complete_task_with_event(
                    &db,
                    &stream_publisher,
                    &task_id,
                    env_id,
                    stream_id,
                    &function_name,
                    &task_type,
                    TaskStatus::Failed,
                    Some(&error),
                )
                .await;
                publish_task_alert(&db, org_id, env_id, &task_id, "task:failed", &error).await;
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
    let task_handle = tokio::task::spawn_blocking(move || {
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
            complete_task_with_event(&db, &stream_publisher, &task_id, env_id, stream_id, &function_name, &task_type, TaskStatus::Cancelled, Some(&cancellation)).await;
            publish_task_alert(&db, org_id, env_id, &task_id, "task:cancelled", &cancellation).await;
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
                complete_task_with_event(
                    &db,
                    &stream_publisher,
                    &task_id,
                    env_id,
                    stream_id,
                    &function_name,
                    &task_type,
                    status.clone(),
                    Some(&result_json),
                )
                .await;
                publish_task_alert(&db, org_id, env_id, &task_id, alert_name, &result_json).await;
                if status == TaskStatus::Failed {
                    maybe_retry_task(&db, &task_queue, &task_id, &request).await;
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
                )
                .await;
                tracing::info!(task_id = %task_id, status = "completed", "Task finished");
            }
        }
        Ok(Ok(Err(e))) => {
            let error = task_failure_json(&e, None);
            complete_task_with_event(
                &db,
                &stream_publisher,
                &task_id,
                env_id,
                stream_id,
                &function_name,
                &task_type,
                TaskStatus::Failed,
                Some(&error),
            )
            .await;
            publish_task_alert(&db, org_id, env_id, &task_id, "task:failed", &error).await;
            maybe_retry_task(&db, &task_queue, &task_id, &request).await;
            tracing::error!(task_id = %task_id, "Task execution error: {}", e);
        }
        Ok(Err(e)) => {
            let error = task_failure_json(&format!("Task panicked: {}", e), None);
            complete_task_with_event(
                &db,
                &stream_publisher,
                &task_id,
                env_id,
                stream_id,
                &function_name,
                &task_type,
                TaskStatus::Failed,
                Some(&error),
            )
            .await;
            publish_task_alert(&db, org_id, env_id, &task_id, "task:failed", &error).await;
            maybe_retry_task(&db, &task_queue, &task_id, &request).await;
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
            complete_task_with_event(
                &db,
                &stream_publisher,
                &task_id,
                env_id,
                stream_id,
                &function_name,
                &task_type,
                TaskStatus::TimedOut,
                Some(&error),
            )
            .await;
            publish_task_alert(&db, org_id, env_id, &task_id, "task:failed", &error).await;
            maybe_retry_task(&db, &task_queue, &task_id, &request).await;
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

/// Complete a task in the DB and emit a `task:complete` env event.
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
) {
    // Persist to DB. Wrap each call so a stuck DB pool can't pin the worker
    // here forever; the periodic zombie cleanup will reconcile timed-out writes.
    match tokio::time::timeout(
        DB_CALL_TIMEOUT,
        Task::complete(db, task_id, &status, result),
    )
    .await
    {
        Ok(Ok(_)) => {}
        Ok(Err(e)) => {
            tracing::warn!(task_id = %task_id, "Task::complete failed: {}", e);
        }
        Err(_) => {
            tracing::error!(
                task_id = %task_id,
                timeout_secs = DB_CALL_TIMEOUT.as_secs(),
                "Task::complete timed out — moving on (run will be reaped by zombie cleanup)"
            );
        }
    }

    // Re-read to get computed duration_ms (best-effort; null on timeout/error).
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

    let cached = {
        let cache = usage_stats_cache.lock().await;
        cache
            .get(&org_id)
            .filter(|(ts, _)| ts.elapsed() < USAGE_STATS_CACHE_TTL)
            .map(|(_, stats)| stats.clone())
    };
    let usage = if let Some(stats) = cached {
        stats
    } else {
        match hot::db::subscription::OrgUsageStats::calculate(
            db,
            &org_id,
            period_start,
            features.call_retention_days(),
        )
        .await
        {
            Ok(u) => {
                let mut cache = usage_stats_cache.lock().await;
                cache.insert(org_id, (std::time::Instant::now(), u.clone()));
                u
            }
            Err(_) => return,
        }
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
    let task = match tokio::time::timeout(DB_CALL_TIMEOUT, Task::get(db, failed_task_id)).await {
        Ok(Ok(t)) => t,
        Ok(Err(e)) => {
            tracing::warn!(task_id = %failed_task_id, "Retry check: couldn't load task: {}", e);
            return;
        }
        Err(_) => {
            tracing::error!(
                task_id = %failed_task_id,
                "Retry check: Task::get timed out — skipping retry"
            );
            return;
        }
    };

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
        Ok(Ok(_)) => {}
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
                tracing::info!(new_task_id = %new_task_id, attempt = next_attempt, "Retry task enqueued after delay");
            }
        });
    } else if let Err(e) = task_queue.enqueue(retry_request).await {
        tracing::error!(new_task_id = %new_task_id, "Failed to enqueue retry task: {}", e);
    } else {
        tracing::info!(new_task_id = %new_task_id, attempt = next_attempt, "Retry task enqueued immediately");
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
    tracing::info!(build_id = %build_id, project = %project.name, "Bytecode cache miss — compiling live build from source");

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
    if extract_dir.join("manifest.hot").exists() {
        return Ok(extract_dir);
    }

    let storage = hot::storage::build_storage_from_config(conf)
        .await
        .map_err(|e| format!("Failed to create build storage: {}", e))?;
    let build_data = storage
        .retrieve_build(build_id, org_id, env_id)
        .await
        .map_err(|e| format!("Failed to retrieve build data: {}", e))?;
    hot::bundle::extract_bundle_from_bytes(&build_data, &extract_dir)
        .map_err(|e| format!("Failed to extract bundle: {}", e))?;
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
    tracing::info!(build_id = %build_id, "Bytecode cache miss — fetching bundle build from storage");

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
/// Returns a list of `(task_id, container_id)` pairs for containers that
/// were adopted and need continued monitoring.
async fn adopt_orphaned_containers(
    executor: &executor::BoxExecutor,
    db: &DatabasePool,
    stream_publisher: &StreamPubSub,
    task_queue: &ProcessingQueue<TaskRequest>,
    worker_id: &str,
) -> Vec<(Uuid, String)> {
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
                tracing::info!(
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
                tracing::info!(
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
            tracing::info!(
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
                // Container is still running — adopt it
                tracing::info!(
                    task_id = %task_id,
                    container_id = %container_id,
                    "Adopting running container (updating worker_id)"
                );
                if let Err(e) = Task::set_worker(db, &task_id, worker_id).await {
                    tracing::warn!(task_id = %task_id, "Failed to update worker_id during adoption: {}", e);
                }
                adopted.push((task_id, container_id));
            }
            Ok(Some(exit_code)) => {
                // Container stopped — complete the task
                tracing::info!(
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
                complete_task_with_event(
                    db,
                    stream_publisher,
                    &task_id,
                    task.env_id,
                    task.stream_id,
                    &task.function_name,
                    &task.task_type,
                    status.clone(),
                    Some(&result_json),
                )
                .await;
                if status == TaskStatus::Failed {
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
                complete_task_with_event(
                    db,
                    stream_publisher,
                    &task_id,
                    task.env_id,
                    task.stream_id,
                    &task.function_name,
                    &task.task_type,
                    TaskStatus::Failed,
                    Some(&error),
                )
                .await;
                maybe_retry_zombie_task(db, &task, task_queue).await;
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
                tracing::info!(
                    container_id = %container_id,
                    "kata.orphans.cleanup: container had no hot.dev/task-id label"
                );
                continue;
            }
        };

        let task = match Task::get(db, &task_id).await {
            Ok(t) => t,
            Err(e) => {
                tracing::info!(
                    task_id = %task_id,
                    container_id = %container_id,
                    "kata.orphans.cleanup: task not in DB ({})", e
                );
                continue;
            }
        };

        if task.task_status_id != TaskStatus::Running.as_id() {
            tracing::info!(
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
        complete_task_with_event(
            db,
            stream_publisher,
            &task_id,
            task.env_id,
            task.stream_id,
            &task.function_name,
            &task.task_type,
            TaskStatus::Failed,
            Some(&error),
        )
        .await;
        maybe_retry_zombie_task(db, &task, task_queue).await;
    }
}

/// Monitor an adopted container until completion.
/// Runs as a background task, polls container status, and completes the task when done.
async fn monitor_adopted_container(
    task_id: Uuid,
    container_id: String,
    db: &DatabasePool,
    stream_publisher: &StreamPubSub,
    task_queue: &ProcessingQueue<TaskRequest>,
    executor: &executor::BoxExecutor,
    coordinator: &shutdown::TaskShutdownCoordinator,
) {
    let task = match Task::get(db, &task_id).await {
        Ok(t) => t,
        Err(e) => {
            tracing::error!(task_id = %task_id, "Failed to load adopted task: {}", e);
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
            tracing::info!(
                task_id = %task_id,
                container_id = %container_id,
                "Stopping adopted container monitor (shutdown)"
            );
            return;
        }

        tokio::time::sleep(poll_interval).await;

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
                complete_task_with_event(
                    db,
                    stream_publisher,
                    &task_id,
                    task.env_id,
                    task.stream_id,
                    &task.function_name,
                    &task.task_type,
                    status.clone(),
                    Some(&result_json),
                )
                .await;

                if status == TaskStatus::Failed {
                    maybe_retry_zombie_task(db, &task, task_queue).await;
                }

                tracing::info!(
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
                    complete_task_with_event(
                        db,
                        stream_publisher,
                        &task_id,
                        task.env_id,
                        task.stream_id,
                        &task.function_name,
                        &task.task_type,
                        TaskStatus::TimedOut,
                        Some(&error),
                    )
                    .await;
                    maybe_retry_zombie_task(db, &task, task_queue).await;
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
                    complete_task_with_event(
                        db,
                        stream_publisher,
                        &task_id,
                        task.env_id,
                        task.stream_id,
                        &task.function_name,
                        &task.task_type,
                        TaskStatus::Failed,
                        Some(&error),
                    )
                    .await;
                    maybe_retry_zombie_task(db, &task, task_queue).await;
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
                        .arg(path.to_string_lossy().to_string())
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
            timeout_ms: request.timeout_ms as i64,
            retry_attempt: 0,
            next_retry_at: None,
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

    #[test]
    fn test_validate_task_request_accepts_matching_db_row() {
        let (request, task, env_id, stream_id, build_id) = make_task_request_and_row();

        assert!(
            validate_task_request_matches_db(&request, &task, env_id, stream_id, build_id).is_ok()
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

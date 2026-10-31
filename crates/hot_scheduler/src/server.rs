use ahash::AHashMap;
use chrono::Utc;
use hot::data::msg::Message;
use hot::data::serialization::Serialization;
use hot::db::{
    DatabasePool, DatabaseScope, MaintenanceOperation, Schedule, ScheduleLog, SchedulePolicy,
    SchedulerState,
};
use hot::env::is_local_dev;
use hot::lang::event::{Event, EventMessage, EventMessageBody, ExecutionContext};
use hot::queue::{Queue, QueueType, mem::MemQueue, sqlite::SqliteQueue, streams::RedisStreamQueue};
use hot::val;
use hot::val::Val;
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio::time::{Duration, interval};
use tracing::{debug, error, info, warn};
use uuid::Uuid;

// Use our custom scheduler instead of tokio-cron-scheduler
use crate::scheduler::{Job, JobScheduler};

/// Convert English expressions to proper cron format for the scheduler
/// This function handles the conversion that should have been done during build validation
fn convert_to_scheduler_cron(
    cron_expr: &str,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    // First, try parsing as traditional cron - if it works, use as-is
    if hot::db::schedule::parse_cron(cron_expr).is_ok() {
        return Ok(cron_expr.to_string());
    }

    // If traditional cron fails, try English-to-cron conversion
    match english_to_cron::str_cron_syntax(cron_expr) {
        Ok(converted_cron) => {
            // Validate the converted expression works with croner
            hot::db::schedule::parse_cron(&converted_cron).map_err(|e| {
                format!(
                    "Converted cron '{}' from '{}' is invalid: {}",
                    converted_cron, cron_expr, e
                )
            })?;

            debug!(
                "Converted English '{}' to cron '{}'",
                cron_expr, converted_cron
            );
            Ok(converted_cron)
        }
        Err(e) => Err(format!(
            "Failed to convert English expression '{}' to cron: {:?}",
            cron_expr, e
        )
        .into()),
    }
}

/// Represents a scheduled function with its database schedule_id and tokio job uuid
#[derive(Debug, Clone)]
struct ScheduledJobInfo {
    pub schedule_id: Uuid,
    pub job_uuid: Uuid,
    pub cron: String,
    pub ns: String,
    pub var: String,
    pub build_id: Uuid,
}

pub const DEFAULT_QUEUE_TYPE: QueueType = QueueType::Sqlite;
pub const DEFAULT_SERIALIZATION: Serialization = Serialization::ZstdJson; // must match Serialization's #[default]
pub const DEFAULT_REDIS_URL: &str = "redis://localhost:6379";
pub const DEFAULT_SYNC_INTERVAL_SECONDS: u64 = 30; // Sync with database every 30 seconds
pub const DEFAULT_RETRY_INTERVAL_SECONDS: u64 = 1;
pub const DEFAULT_AT_INTERVAL_SECONDS: u64 = 1;
/// The session advisory lock that elects the one active scheduler. Operators can
/// find the active scheduler's database session by this key in `pg_locks`.
pub const SCHEDULER_ADVISORY_LOCK_ID: i64 = 4_849_672_113_597_841;
/// How often the scheduler confirms it still holds its singleton lock, how
/// long one confirmation may take, and how many confirmations in a row may
/// take longer. Together they bound how long a scheduler that lost its lock
/// keeps dispatching alongside a successor: one check when its session ends,
/// about three intervals when its session stops answering.
const SINGLETON_CHECK_INTERVAL: Duration = Duration::from_secs(2);
const SINGLETON_CHECK_TIMEOUT: Duration = Duration::from_secs(1);
const SINGLETON_MAX_SLOW_CHECKS: u32 = 3;

/// Why a singleton check did not confirm the lock.
#[derive(Debug, PartialEq)]
enum LockCheckFailure {
    /// No answer in time. A stalled database (a laptop waking, a paused
    /// container, a long vacuum) answers late; a partitioned session can hang
    /// until TCP gives up while the server has already freed the lock for a
    /// successor. Only several in a row count as lost.
    Slow(Duration),
    /// The session answered that it no longer holds the lock, or failed.
    Lost(String),
}

impl std::fmt::Display for LockCheckFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Slow(limit) => write!(f, "the lock check did not finish within {limit:?}"),
            Self::Lost(reason) => f.write_str(reason),
        }
    }
}

/// One bounded singleton check.
async fn confirm_singleton_lock<E: std::fmt::Display>(
    check: impl std::future::Future<Output = Result<bool, E>>,
    limit: Duration,
) -> Result<(), LockCheckFailure> {
    match tokio::time::timeout(limit, check).await {
        Ok(Ok(true)) => Ok(()),
        Ok(Ok(false)) => Err(LockCheckFailure::Lost(
            "its session no longer holds the lock".into(),
        )),
        Ok(Err(e)) => Err(LockCheckFailure::Lost(format!(
            "its lock session failed: {e}"
        ))),
        Err(_) => Err(LockCheckFailure::Slow(limit)),
    }
}

/// The session that holds the scheduler's singleton lock.
trait LockSession: Send {
    fn still_held(&mut self) -> impl std::future::Future<Output = Result<bool, String>> + Send;
}

impl LockSession for hot::db::AdvisoryLock {
    async fn still_held(&mut self) -> Result<bool, String> {
        self.ensure_held().await.map_err(|e| e.to_string())
    }
}

/// Confirm the singleton lock every `interval` until shutdown. Returns why
/// the lock counts as lost, or None on shutdown.
async fn watch_singleton_lock(
    session: &mut impl LockSession,
    interval: Duration,
    limit: Duration,
    shutdown: &mut tokio::sync::broadcast::Receiver<()>,
) -> Option<String> {
    let mut timer = tokio::time::interval(interval);
    timer.tick().await;
    let mut slow_checks = 0;
    loop {
        tokio::select! {
            _ = timer.tick() => {
                match confirm_singleton_lock(session.still_held(), limit).await {
                    Ok(()) => slow_checks = 0,
                    Err(LockCheckFailure::Lost(reason)) => return Some(reason),
                    Err(slow) => {
                        slow_checks += 1;
                        if slow_checks >= SINGLETON_MAX_SLOW_CHECKS {
                            return Some(format!("{slow}, {slow_checks} times in a row"));
                        }
                        warn!(
                            "hot.dev: SCHEDULER {slow} ({slow_checks} in a row; {SINGLETON_MAX_SLOW_CHECKS} count as a lost singleton lock)"
                        );
                    }
                }
            }
            _ = shutdown.recv() => return None,
        }
    }
}

/// The error a scheduler stops with when it loses its singleton lock.
#[derive(Debug)]
pub struct SingletonLockLost;

impl std::fmt::Display for SingletonLockLost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("scheduler singleton lock was lost; stopping to avoid duplicate scheduling")
    }
}

impl std::error::Error for SingletonLockLost {}

type SchedulerResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

const RESTART_BACKOFF_INITIAL: Duration = Duration::from_secs(1);
const RESTART_BACKOFF_MAX: Duration = Duration::from_secs(30);
/// A scheduler that ran this long before losing its lock again restarts with
/// the initial backoff.
const RESTART_BACKOFF_RESET_AFTER: Duration = Duration::from_secs(60);

/// Run a scheduler until shutdown, starting it again with backoff whenever it
/// loses its singleton lock. For `hot dev` and integration `hot test`, where a
/// brief database stall must not end scheduling for the rest of the session.
/// Restarting is safe: each start takes the lock afresh, and fails while any
/// other scheduler (or this one's old session, until the server ends it) holds
/// it. A production scheduler stops instead and leaves the restart to its
/// supervisor.
pub async fn run_restarting<F, Fut>(start: F) -> SchedulerResult
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = SchedulerResult>,
{
    restart_after_lock_loss(
        start,
        RESTART_BACKOFF_INITIAL,
        RESTART_BACKOFF_MAX,
        hot::signal::shutdown_signal,
    )
    .await
}

async fn restart_after_lock_loss<F, Fut, S, SFut>(
    mut start: F,
    initial: Duration,
    max: Duration,
    mut shutdown: S,
) -> SchedulerResult
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = SchedulerResult>,
    S: FnMut() -> SFut,
    SFut: std::future::Future<Output = ()>,
{
    let mut backoff = initial;
    // After a loss every failed start is retried: the database can still be
    // stalled, and the old session can still hold the lock.
    let mut recovering = false;
    loop {
        let started = tokio::time::Instant::now();
        let error = match start().await {
            Ok(()) => return Ok(()),
            Err(error) => error,
        };
        let lost = error.is::<SingletonLockLost>();
        if !lost && !recovering {
            return Err(error);
        }
        if lost && started.elapsed() >= RESTART_BACKOFF_RESET_AFTER {
            backoff = initial;
        }
        recovering = true;
        warn!("hot.dev: SCHEDULER stopped ({error}); restarting in {backoff:?}");
        tokio::select! {
            _ = tokio::time::sleep(backoff) => {}
            _ = shutdown() => return Ok(()),
        }
        backoff = (backoff * 2).min(max);
    }
}

async fn acquire_scheduler_singleton_guard(
    db: &DatabasePool,
) -> Result<Option<hot::db::AdvisoryLock>, Box<dyn std::error::Error + Send + Sync>> {
    if matches!(db, DatabasePool::Sqlite(_)) {
        return Ok(None);
    }
    let mut guard = hot::db::try_acquire_advisory_lock(db, SCHEDULER_ADVISORY_LOCK_ID)
        .await?
        .ok_or("another hot_scheduler instance already holds the scheduler singleton lock")?;
    // A transaction- or statement-mode pooler can run the next statement on a
    // different backend, where the session lock is invisible. Fail here, before
    // any work, rather than at the first periodic check.
    match confirm_singleton_lock(guard.ensure_held(), SINGLETON_CHECK_TIMEOUT).await {
        Ok(()) => {}
        Err(slow @ LockCheckFailure::Slow(_)) => {
            guard.release().await;
            return Err(
                format!("the scheduler could not confirm its singleton lock ({slow})").into(),
            );
        }
        Err(LockCheckFailure::Lost(reason)) => {
            guard.release().await;
            return Err(format!(
                "the scheduler could not confirm its singleton lock ({reason}); its database \
                 connection must keep one session, so do not route it through a transaction- or \
                 statement-mode pooler"
            )
            .into());
        }
    }
    Ok(Some(guard))
}

/// Resolve durable ownership before any scheduler-produced execution data is written.
async fn admitted_environment_db(
    db: &DatabasePool,
    env_id: Uuid,
) -> Result<(DatabasePool, Uuid), Box<dyn std::error::Error + Send + Sync>> {
    if !hot::db::Org::is_environment_admitted(db, &env_id).await? {
        return Err("environment is suspended; scheduled work remains pending".into());
    }
    let env = hot::db::Env::get_env_uncached(db, &env_id).await?;
    Ok((
        db.with_scope(DatabaseScope::Environment {
            org_id: env.org_id,
            env_id,
        }),
        env.org_id,
    ))
}

pub fn get_resolved_conf(conf: Val) -> Val {
    // Start with defaults
    let default_conf = val!({
        "sync-interval-seconds": DEFAULT_SYNC_INTERVAL_SECONDS as i64,
        "backfill": false,
        "retry-interval-seconds": DEFAULT_RETRY_INTERVAL_SECONDS as i64,
        "at-interval-seconds": DEFAULT_AT_INTERVAL_SECONDS as i64,
        "singleton": true
    });

    // Merge with provided conf (the provided conf will override defaults)
    default_conf.merge(&conf)
}

#[allow(clippy::too_many_arguments)]
pub async fn run(
    queue_type: QueueType,
    redis_uri: Option<String>,
    redis_cluster: bool,
    serialization: Serialization,
    db: Option<DatabasePool>,
    sync_interval_seconds: Option<u64>,
    retry_interval_seconds: Option<u64>,
    at_interval_seconds: Option<u64>,
    backfill_enabled: bool,
    schedule_policy: SchedulePolicy,
    singleton: bool,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    run_for_environment(
        queue_type,
        redis_uri,
        redis_cluster,
        serialization,
        db,
        sync_interval_seconds,
        retry_interval_seconds,
        at_interval_seconds,
        backfill_enabled,
        schedule_policy,
        singleton,
        None,
    )
    .await
}

/// Local development binds scheduler discovery to its selected profile.
#[allow(clippy::too_many_arguments)]
pub async fn run_for_environment(
    queue_type: QueueType,
    redis_uri: Option<String>,
    redis_cluster: bool,
    serialization: Serialization,
    db: Option<DatabasePool>,
    sync_interval_seconds: Option<u64>,
    retry_interval_seconds: Option<u64>,
    at_interval_seconds: Option<u64>,
    backfill_enabled: bool,
    schedule_policy: SchedulePolicy,
    singleton: bool,
    env_id: Option<Uuid>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    debug!("hot.dev: SCHEDULER starting");
    debug!(
        "hot.dev: SCHEDULER schedule policy: min_interval={}s, min_delay={}s, max_active_per_org={}",
        schedule_policy.min_interval_secs,
        schedule_policy.min_delay_secs,
        schedule_policy.max_active_per_org
    );

    // Create queue for sending scheduled events to workers
    let event_queue: Arc<dyn Queue<Message>> = match queue_type {
        QueueType::Memory => {
            let queue = MemQueue::<Message>::new("hot:event".to_string())?
                .with_serialization(serialization);
            Arc::new(queue)
        }
        QueueType::Sqlite => {
            let queue = SqliteQueue::<Message>::new("hot:event".to_string())?
                .with_serialization(serialization);
            Arc::new(queue)
        }
        QueueType::Redis => {
            let redis_uri = redis_uri.ok_or("Redis URL is required for Redis queue type")?;

            // Initialize Rustls crypto provider if using TLS (rediss://)
            if redis_uri.starts_with("rediss://") {
                hot::redis::init_crypto_provider();
            }

            // Check if cluster mode is enabled or auto-detect from URI
            let is_cluster = redis_cluster || hot::redis::is_cluster_uri(&redis_uri);

            if is_cluster {
                debug!("hot.dev: SCHEDULER using Redis Streams cluster mode");
                let client = redis::cluster::ClusterClient::new(vec![redis_uri.as_str()])?;
                let queue =
                    RedisStreamQueue::<Message>::new_cluster(client, "hot:event".to_string())
                        .with_serialization(serialization);
                Arc::new(queue)
            } else {
                debug!("hot.dev: SCHEDULER using Redis Streams standalone mode");
                let client = redis::Client::open(redis_uri.as_str())?;
                let queue = RedisStreamQueue::<Message>::new(client, "hot:event".to_string())
                    .with_serialization(serialization);
                Arc::new(queue)
            }
        }
    };

    // Verify queue connectivity with a quick health check
    debug!("hot.dev: SCHEDULER verifying queue connectivity");
    match queue_type {
        QueueType::Memory => {
            debug!("hot.dev: SCHEDULER using in-memory queue (no connectivity check needed)");
        }
        QueueType::Sqlite | QueueType::Redis => {
            // Force schema/bootstrap or remote connectivity before starting.
            match event_queue.is_empty().await {
                Ok(_) => {
                    debug!(
                        "hot.dev: SCHEDULER successfully connected to {} queue",
                        queue_type
                    );
                }
                Err(e) => {
                    error!(
                        "hot.dev: SCHEDULER failed to connect to {} queue: {}",
                        queue_type, e
                    );
                    return Err(
                        format!("{} queue connectivity check failed: {}", queue_type, e).into(),
                    );
                }
            }
        }
    }

    // Create a scheduler
    let mut scheduler = JobScheduler::new().await?;

    // Track running jobs: schedule_id -> ScheduledJobInfo
    let running_jobs: Arc<RwLock<AHashMap<Uuid, ScheduledJobInfo>>> =
        Arc::new(RwLock::new(AHashMap::new()));

    // Create shutdown signal for background tasks
    let (shutdown_tx, _) = tokio::sync::broadcast::channel::<()>(1);

    // Held until shutdown: dropping the guard closes its session and frees the lock.
    let mut singleton_guard = None;

    // If database is provided, start the sync loop and retry processor
    if let Some(db) = db {
        if !singleton {
            return Err(
                "hot.scheduler.singleton=false is not supported; multi-scheduler mode is not implemented"
                    .into(),
            );
        }
        singleton_guard = acquire_scheduler_singleton_guard(&db).await?;
        let db = Arc::new(db.with_scope(DatabaseScope::Maintenance {
            operation: MaintenanceOperation::Scheduler,
            org_id: None,
        }));
        let sync_interval = sync_interval_seconds
            .unwrap_or(DEFAULT_SYNC_INTERVAL_SECONDS)
            .max(1);
        let retry_interval = retry_interval_seconds
            .unwrap_or(DEFAULT_RETRY_INTERVAL_SECONDS)
            .max(1);
        let at_interval = at_interval_seconds
            .unwrap_or(DEFAULT_AT_INTERVAL_SECONDS)
            .max(1);
        info!(
            "hot.dev: SCHEDULER intervals: sync={}s retry={}s at={}s",
            sync_interval, retry_interval, at_interval
        );

        // Reconciliation checks one counter per short transaction and applies
        // relative deltas; it never holds the account lock used by admission.
        let usage_db = Arc::clone(&db);
        let mut usage_shutdown = shutdown_tx.subscribe();
        tokio::spawn(async move {
            let mut timer = interval(Duration::from_secs(3600));
            loop {
                tokio::select! {
                    _ = timer.tick() => {
                        if let Err(error) = hot::db::usage::reconcile_accounts(&usage_db).await {
                            error!(%error, "hot.dev: SCHEDULER usage reconciliation failed");
                        }
                    }
                    _ = usage_shutdown.recv() => break,
                }
            }
        });

        // Start the database sync task
        let scheduler_clone = scheduler.clone();
        let queue_clone = Arc::clone(&event_queue);
        let running_jobs_clone = Arc::clone(&running_jobs);
        let db_clone = Arc::clone(&db);
        let mut shutdown_rx = shutdown_tx.subscribe();

        tokio::spawn(async move {
            let mut sync_timer = interval(Duration::from_secs(sync_interval));

            loop {
                tokio::select! {
                    _ = sync_timer.tick() => {
                        if let Err(e) = sync_with_database(
                            &scheduler_clone,
                            &queue_clone,
                            &running_jobs_clone,
                            &db_clone,
                            backfill_enabled,
                            schedule_policy,
                            env_id,
                        )
                        .await
                        {
                            error!("hot.dev: SCHEDULER database sync failed: {}", e);
                        }
                    }
                    _ = shutdown_rx.recv() => {
                        tracing::debug!("hot.dev: SCHEDULER sync task received shutdown signal");
                        break;
                    }
                }
            }
        });

        // Start the retry processor task.
        let retry_queue_clone = Arc::clone(&event_queue);
        let retry_db_clone = Arc::clone(&db);
        let mut shutdown_rx = shutdown_tx.subscribe();

        tokio::spawn(async move {
            let mut retry_timer = interval(Duration::from_secs(retry_interval));

            loop {
                tokio::select! {
                    _ = retry_timer.tick() => {
                        if let Err(e) = process_pending_retries(&retry_db_clone, &retry_queue_clone, env_id).await {
                            error!("hot.dev: SCHEDULER retry processor failed: {}", e);
                        }
                    }
                    _ = shutdown_rx.recv() => {
                        tracing::debug!("hot.dev: SCHEDULER retry task received shutdown signal");
                        break;
                    }
                }
            }
        });

        // Start the @at schedule processor task.
        let at_queue_clone = Arc::clone(&event_queue);
        let at_db_clone = Arc::clone(&db);
        let mut shutdown_rx = shutdown_tx.subscribe();

        tokio::spawn(async move {
            let mut at_timer = interval(Duration::from_secs(at_interval));

            loop {
                tokio::select! {
                    _ = at_timer.tick() => {
                        if let Err(e) = process_due_at_schedules(&at_db_clone, &at_queue_clone, env_id).await {
                            error!("hot.dev: SCHEDULER @at schedule processor failed: {}", e);
                        }
                    }
                    _ = shutdown_rx.recv() => {
                        tracing::debug!("hot.dev: SCHEDULER @at task received shutdown signal");
                        break;
                    }
                }
            }
        });
        // Start the daily maintenance task
        // Enqueues a MaintenanceMessage to hot:event every 24 hours for the worker to process.
        // Tasks include: expired session cleanup, inactive schedule cleanup, etc.
        let maint_queue_clone = Arc::clone(&event_queue);
        let mut shutdown_rx = shutdown_tx.subscribe();

        tokio::spawn(async move {
            // Run daily (86400 seconds = 24 hours)
            let mut maint_timer = interval(Duration::from_secs(86400));
            // Skip the first immediate tick — don't run on startup
            maint_timer.tick().await;

            loop {
                tokio::select! {
                    _ = maint_timer.tick() => {
                        debug!("hot.dev: SCHEDULER enqueuing daily maintenance task");
                        let msg = if is_local_dev() {
                            hot::lang::event::MaintenanceMessage::daily_core_tasks()
                        } else {
                            hot::lang::event::MaintenanceMessage::all_tasks()
                        };
                        let message: Message = msg.into();
                        if let Err(e) = maint_queue_clone.enqueue(message).await {
                            error!("hot.dev: SCHEDULER failed to enqueue maintenance task: {}", e);
                        }
                    }
                    _ = shutdown_rx.recv() => {
                        tracing::debug!("hot.dev: SCHEDULER maintenance task received shutdown signal");
                        break;
                    }
                }
            }
        });

        // Custom-domain provider maintenance: skip in local dev (HOT_ENV=development).
        if !is_local_dev() {
            // Start the domain verification task (runs every 5 minutes)
            // Checks unverified custom domains for certificate status.
            let domain_queue_clone = Arc::clone(&event_queue);
            let mut domain_shutdown_rx = shutdown_tx.subscribe();

            tokio::spawn(async move {
                // Run every 5 minutes (300 seconds)
                let mut domain_timer = interval(Duration::from_secs(300));
                // Skip the first immediate tick
                domain_timer.tick().await;

                loop {
                    tokio::select! {
                        _ = domain_timer.tick() => {
                            tracing::debug!("hot.dev: SCHEDULER enqueuing domain verification task");
                            let msg = hot::lang::event::MaintenanceMessage::single_task("domain_verification");
                            let message: Message = msg.into();
                            if let Err(e) = domain_queue_clone.enqueue(message).await {
                                error!("hot.dev: SCHEDULER failed to enqueue domain verification task: {}", e);
                            }
                        }
                        _ = domain_shutdown_rx.recv() => {
                            tracing::debug!("hot.dev: SCHEDULER domain verification task received shutdown signal");
                            break;
                        }
                    }
                }
            });

            // Start the domain provisioning task (runs every 2 minutes)
            // Creates routing targets for verified domains and checks
            // deployment status for provisioning domains.
            let prov_queue_clone = Arc::clone(&event_queue);
            let mut prov_shutdown_rx = shutdown_tx.subscribe();

            tokio::spawn(async move {
                // Run every 2 minutes (120 seconds)
                let mut prov_timer = interval(Duration::from_secs(120));
                // Skip the first immediate tick
                prov_timer.tick().await;

                loop {
                    tokio::select! {
                        _ = prov_timer.tick() => {
                            tracing::debug!("hot.dev: SCHEDULER enqueuing domain provisioning task");
                            let msg = hot::lang::event::MaintenanceMessage::single_task("domain_provisioning");
                            let message: Message = msg.into();
                            if let Err(e) = prov_queue_clone.enqueue(message).await {
                                error!("hot.dev: SCHEDULER failed to enqueue domain provisioning task: {}", e);
                            }
                        }
                        _ = prov_shutdown_rx.recv() => {
                            tracing::debug!("hot.dev: SCHEDULER domain provisioning task received shutdown signal");
                            break;
                        }
                    }
                }
            });

            // Start the domain cleanup task (runs every 2 minutes)
            // Cleans up provider resources for soft-deleted domains.
            let cleanup_queue_clone = Arc::clone(&event_queue);
            let mut cleanup_shutdown_rx = shutdown_tx.subscribe();

            tokio::spawn(async move {
                let mut cleanup_timer = interval(Duration::from_secs(120));
                cleanup_timer.tick().await;

                loop {
                    tokio::select! {
                        _ = cleanup_timer.tick() => {
                            tracing::debug!("hot.dev: SCHEDULER enqueuing domain cleanup task");
                            let msg = hot::lang::event::MaintenanceMessage::single_task("domain_cleanup");
                            let message: Message = msg.into();
                            if let Err(e) = cleanup_queue_clone.enqueue(message).await {
                                error!("hot.dev: SCHEDULER failed to enqueue domain cleanup task: {}", e);
                            }
                        }
                        _ = cleanup_shutdown_rx.recv() => {
                            tracing::debug!("hot.dev: SCHEDULER domain cleanup task received shutdown signal");
                            break;
                        }
                    }
                }
            });
        }
        // Start the upload cleanup task (runs every hour)
        // Removes expired pending multipart uploads.
        let upload_queue_clone = Arc::clone(&event_queue);
        let mut upload_shutdown_rx = shutdown_tx.subscribe();

        tokio::spawn(async move {
            let mut upload_timer = interval(Duration::from_secs(3600));
            upload_timer.tick().await;

            loop {
                tokio::select! {
                    _ = upload_timer.tick() => {
                        debug!("hot.dev: SCHEDULER enqueuing upload cleanup task");
                        let msg = hot::lang::event::MaintenanceMessage::single_task("upload_cleanup");
                        let message: Message = msg.into();
                        if let Err(e) = upload_queue_clone.enqueue(message).await {
                            error!("hot.dev: SCHEDULER failed to enqueue upload cleanup task: {}", e);
                        }
                    }
                    _ = upload_shutdown_rx.recv() => {
                        debug!("hot.dev: SCHEDULER upload cleanup task received shutdown signal");
                        break;
                    }
                }
            }
        });
        // Start the file object cleanup task (runs every 5 minutes)
        // Deletes file objects retired by overwrites and deletes once their
        // grace period has passed.
        let file_object_queue = Arc::clone(&event_queue);
        let mut file_object_shutdown_rx = shutdown_tx.subscribe();

        tokio::spawn(async move {
            let mut file_object_timer = interval(Duration::from_secs(300));
            file_object_timer.tick().await;

            loop {
                tokio::select! {
                    _ = file_object_timer.tick() => {
                        debug!("hot.dev: SCHEDULER enqueuing file object cleanup task");
                        let msg = hot::lang::event::MaintenanceMessage::single_task("file_object_cleanup");
                        let message: Message = msg.into();
                        if let Err(e) = file_object_queue.enqueue(message).await {
                            error!("hot.dev: SCHEDULER failed to enqueue file object cleanup task: {}", e);
                        }
                    }
                    _ = file_object_shutdown_rx.recv() => {
                        debug!("hot.dev: SCHEDULER file object cleanup task received shutdown signal");
                        break;
                    }
                }
            }
        });
    } else {
        warn!("hot.dev: SCHEDULER running without database - no schedule sync will occur");
    }

    // The singleton lock lives on its own session, which can end silently
    // (failover, termination, idle timeout). Stop scheduling if it is lost so a
    // replacement scheduler never runs alongside this one.
    let (lock_lost_tx, lock_lost_rx) = tokio::sync::oneshot::channel::<()>();
    let guard_task = singleton_guard.map(|mut guard| {
        let mut shutdown_rx = shutdown_tx.subscribe();
        tokio::spawn(async move {
            let lost = watch_singleton_lock(
                &mut guard,
                SINGLETON_CHECK_INTERVAL,
                SINGLETON_CHECK_TIMEOUT,
                &mut shutdown_rx,
            )
            .await;
            match lost {
                Some(reason) => {
                    error!("hot.dev: SCHEDULER lost its singleton lock ({reason}); stopping");
                    let _ = lock_lost_tx.send(());
                    None
                }
                None => Some(guard),
            }
        })
    });

    // Start the scheduler
    scheduler.start().await?;

    info!("hot.dev: SCHEDULER started and running");

    // Wait for SIGINT or SIGTERM, or the loss of the singleton lock.
    let watching_lock = guard_task.is_some();
    let lock_lost = async move {
        if watching_lock {
            let _ = lock_lost_rx.await;
        } else {
            std::future::pending::<()>().await;
        }
    };
    let lost = tokio::select! {
        _ = hot::signal::shutdown_signal() => false,
        _ = lock_lost => true,
    };
    info!("hot.dev: SCHEDULER shutting down gracefully");
    let _ = shutdown_tx.send(());
    scheduler.shutdown().await?;
    if let Some(task) = guard_task
        && let Ok(Some(guard)) = task.await
    {
        guard.release().await;
    }
    if lost {
        return Err(Box::new(SingletonLockLost));
    }

    Ok(())
}

/// Sync with database to update running scheduled jobs
async fn sync_with_database(
    scheduler: &JobScheduler,
    event_queue: &Arc<dyn Queue<Message>>,
    running_jobs: &Arc<RwLock<AHashMap<Uuid, ScheduledJobInfo>>>,
    db: &Arc<DatabasePool>,
    backfill_enabled: bool,
    schedule_policy: SchedulePolicy,
    env_id: Option<Uuid>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let now = Utc::now();

    debug!("hot.dev: SCHEDULER syncing with database");

    // Get the last time the scheduler successfully synced
    let last_sync_time = SchedulerState::get_last_sync_time(db).await.map_err(|e| {
        error!(
            "Failed to get scheduler last sync time from database: {}",
            e
        );
        e
    })?;

    if let Some(last_sync) = last_sync_time {
        debug!(
            "hot.dev: SCHEDULER last sync was at {} ({} ago)",
            last_sync,
            format_duration(now - last_sync)
        );
    } else {
        debug!("hot.dev: SCHEDULER first sync - no previous sync time found");
    }

    // Get all schedules for deployed builds
    let schedules = Schedule::get_schedules_for_environment(db, None, None, env_id)
        .await
        .map_err(|e| {
            error!(
                "Failed to get schedules for deployed builds from database: {}",
                e
            );
            e
        })?;
    // A deployment this runtime can't run, made with another major version
    // of Hot, keeps its schedules paused until the project is redeployed.
    let schedules = without_builds_awaiting_redeploy(db, schedules).await;

    // Get current running jobs
    let mut jobs_map = running_jobs.write().await;

    // Track which schedules we've seen
    let mut seen_schedule_ids = ahash::AHashSet::new();

    // Track sync statistics
    let mut added_count = 0;
    let mut updated_count = 0;
    let mut failed_count = 0;
    let mut backfilled_count = 0;

    // Add or update jobs for each schedule
    for schedule in schedules {
        seen_schedule_ids.insert(schedule.schedule_id);

        // Check for missed executions BEFORE updating/adding the job (only if backfill is enabled)
        if backfill_enabled {
            match check_and_backfill_missed_executions(
                db,
                event_queue,
                &schedule,
                last_sync_time,
                now,
            )
            .await
            {
                Ok(count) => {
                    backfilled_count += count;
                }
                Err(e) => {
                    error!(
                        "hot.dev: SCHEDULER failed to check for missed executions for schedule {} ({}:{}): {}",
                        schedule.schedule_id, schedule.ns, schedule.var, e
                    );
                }
            }
        } else {
            debug!(
                "Backfill disabled, skipping missed execution check for schedule {} ({}:{})",
                schedule.schedule_id, schedule.ns, schedule.var
            );
        }

        // Check if we already have this job running
        if let Some(existing_job) = jobs_map.get(&schedule.schedule_id) {
            // Check if the job needs to be updated (cron, function, or build changed)
            if existing_job.cron != schedule.cron
                || existing_job.ns != schedule.ns
                || existing_job.var != schedule.var
                || existing_job.build_id != schedule.build_id
            {
                // Log what changed
                if existing_job.build_id != schedule.build_id {
                    debug!(
                        "hot.dev: SCHEDULER schedule {} moved from build {} to build {}",
                        existing_job.schedule_id, existing_job.build_id, schedule.build_id
                    );
                }
                if existing_job.cron != schedule.cron {
                    debug!(
                        "hot.dev: SCHEDULER schedule {} cron changed from '{}' to '{}'",
                        existing_job.schedule_id, existing_job.cron, schedule.cron
                    );
                }

                // Remove the old job
                if let Err(e) = scheduler.remove(&existing_job.job_uuid).await {
                    warn!(
                        "hot.dev: SCHEDULER failed to remove outdated job {} for schedule {}: {}",
                        existing_job.job_uuid, existing_job.schedule_id, e
                    );
                }

                // Try to add the updated job
                match create_scheduled_job(db, scheduler, event_queue, &schedule, schedule_policy)
                    .await
                {
                    Ok(job_info) => {
                        jobs_map.insert(schedule.schedule_id, job_info);
                        updated_count += 1;
                        debug!(
                            "hot.dev: SCHEDULER updated job for schedule {} ({}:{}) on build {}",
                            schedule.schedule_id, schedule.ns, schedule.var, schedule.build_id
                        );
                    }
                    Err(e) => {
                        failed_count += 1;
                        error!(
                            "hot.dev: SCHEDULER failed to update job for schedule {} ({}:{}) with cron '{}': {}",
                            schedule.schedule_id, schedule.ns, schedule.var, schedule.cron, e
                        );
                    }
                }
            } else {
                // Job is up to date, just log debug info
                debug!(
                    "hot.dev: SCHEDULER schedule {} ({}:{}) on build {} is up to date",
                    existing_job.schedule_id,
                    existing_job.ns,
                    existing_job.var,
                    existing_job.build_id
                );
            }
        } else {
            // Try to add new job
            match create_scheduled_job(db, scheduler, event_queue, &schedule, schedule_policy).await
            {
                Ok(job_info) => {
                    jobs_map.insert(schedule.schedule_id, job_info);
                    added_count += 1;
                    debug!(
                        "hot.dev: SCHEDULER added new job for schedule {} ({}/{}) on build {}",
                        schedule.schedule_id, schedule.ns, schedule.var, schedule.build_id
                    );
                }
                Err(e) => {
                    failed_count += 1;
                    error!(
                        "hot.dev: SCHEDULER failed to add job for schedule {} ({}/{}) with cron '{}': {}",
                        schedule.schedule_id, schedule.ns, schedule.var, schedule.cron, e
                    );
                }
            }
        }
    }

    // Remove jobs that are no longer in the database
    let mut to_remove = Vec::new();
    for (schedule_id, job_info) in jobs_map.iter() {
        if !seen_schedule_ids.contains(schedule_id) {
            to_remove.push(*schedule_id);

            // Remove from tokio scheduler
            if let Err(e) = scheduler.remove(&job_info.job_uuid).await {
                warn!(
                    "hot.dev: SCHEDULER failed to remove job {} for schedule {}: {}",
                    job_info.job_uuid, job_info.schedule_id, e
                );
            }

            debug!(
                "hot.dev: SCHEDULER removed job for schedule {} ({}/{}) from build {}",
                job_info.schedule_id, job_info.ns, job_info.var, job_info.build_id
            );
        }
    }

    // Remove from our tracking map
    let removed_count = to_remove.len();
    for schedule_id in to_remove {
        jobs_map.remove(&schedule_id);
    }

    // Update the scheduler's last successful sync time
    if let Err(e) = SchedulerState::update_sync_time(db, now).await {
        error!(
            "hot.dev: SCHEDULER failed to update sync time in database: {}",
            e
        );
    }

    // Log sync summary
    if failed_count > 0 || backfilled_count > 0 {
        if backfilled_count > 0 {
            warn!(
                "hot.dev: SCHEDULER sync complete. Active: {}, Added: {}, Updated: {}, Removed: {}, Failed: {}, Backfilled: {}",
                jobs_map.len(),
                added_count,
                updated_count,
                removed_count,
                failed_count,
                backfilled_count
            );
        } else {
            error!(
                "hot.dev: SCHEDULER sync complete with errors. Active: {}, Added: {}, Updated: {}, Removed: {}, Failed: {}",
                jobs_map.len(),
                added_count,
                updated_count,
                removed_count,
                failed_count
            );
        }
    } else if added_count > 0 || updated_count > 0 || removed_count > 0 {
        info!(
            "hot.dev: SCHEDULER sync complete. Active: {}, Added: {}, Updated: {}, Removed: {}",
            jobs_map.len(),
            added_count,
            updated_count,
            removed_count
        );
    } else {
        debug!(
            "hot.dev: SCHEDULER sync complete (no changes). Active: {}",
            jobs_map.len()
        );
    }

    Ok(())
}

/// Create a scheduled job for a database schedule
async fn create_scheduled_job(
    db: &Arc<DatabasePool>,
    scheduler: &JobScheduler,
    event_queue: &Arc<dyn Queue<Message>>,
    schedule: &Schedule,
    base_policy: SchedulePolicy,
) -> Result<ScheduledJobInfo, Box<dyn std::error::Error + Send + Sync>> {
    let schedule_id = schedule.schedule_id;
    let cron = schedule.cron.clone();
    let ns = schedule.ns.clone();
    let var = schedule.var.clone();
    let build_id = schedule.build_id;

    // Validate cron expression before creating job (this should rarely fail since
    // validation happens at build time, but provides an extra safety check)
    if let Err(e) = Schedule::validate_cron_expression(&cron) {
        return Err(Box::new(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "Invalid cron expression '{}' for schedule {} ({}:{}): {}",
                cron, schedule_id, ns, var, e
            ),
        )));
    }

    let policy = match hot::db::Build::get_env_id_for_build(db, &build_id).await {
        Ok(env_id) => match hot::db::Env::get_env(db, &env_id).await {
            Ok(env) => {
                let features = hot::db::Features::resolve_for_org(db, &env.org_id).await;
                base_policy.with_features(&features)
            }
            Err(e) => {
                warn!(
                    "hot.dev: SCHEDULER could not resolve env {} for schedule policy: {}",
                    env_id, e
                );
                base_policy
            }
        },
        Err(e) => {
            warn!(
                "hot.dev: SCHEDULER could not resolve build {} env for schedule policy: {}",
                build_id, e
            );
            base_policy
        }
    };
    if let Err(e) = hot::db::validate_recurring_schedule_interval(&cron, policy.min_interval_secs) {
        return Err(Box::new(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "Schedule {} ({}:{}) violates schedule policy: {}",
                schedule_id,
                ns,
                var,
                e.message()
            ),
        )));
    }

    // Convert English expressions to cron format for the scheduler
    // This ensures the Job constructor gets a valid cron expression
    let scheduler_cron = convert_to_scheduler_cron(&cron)?;

    // A failed enqueue skips the occurrence, so retry it within the interval
    // (a Redis restart or failover); only the enqueue, not the event row the
    // run's other steps write.
    let queue_clone: Arc<dyn Queue<Message>> = Arc::new(hot::queue::RetryingEnqueue::new(
        Arc::clone(event_queue),
        vec![
            std::time::Duration::from_secs(1),
            std::time::Duration::from_secs(5),
        ],
    ));
    let db_clone = Arc::clone(db);
    let schedule_clone = schedule.clone();
    let job = Job::new_async(&scheduler_cron, move |_uuid, _lock| {
        let queue = Arc::clone(&queue_clone);
        let db = Arc::clone(&db_clone);
        let schedule = schedule_clone.clone();
        Box::pin(async move {
            if let Err(error) =
                queue_schedule_execution(&db, &queue, &schedule, Utc::now(), false).await
            {
                warn!(schedule_id = %schedule.schedule_id, %error, "Scheduled execution deferred");
            }
        })
    })
    .map_err(|e| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "Failed to create job for schedule {} ({}:{}) with cron '{}': {}",
                schedule_id, ns, var, cron, e
            ),
        )
    })?;

    // Add the job to the scheduler
    let job_uuid = scheduler.add(job).await?;

    Ok(ScheduledJobInfo {
        schedule_id,
        job_uuid,
        cron,
        ns,
        var,
        build_id,
    })
}

/// Check for missed executions and backfill them
/// Returns the number of executions that were backfilled
async fn check_and_backfill_missed_executions(
    db: &Arc<DatabasePool>,
    event_queue: &Arc<dyn Queue<Message>>,
    schedule: &Schedule,
    last_sync_time: Option<chrono::DateTime<Utc>>,
    now: chrono::DateTime<Utc>,
) -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
    // Guard 1: Require last sync time
    let last_sync = match last_sync_time {
        Some(time) => time,
        None => {
            debug!(
                "No previous sync time, skipping backfill for schedule {}",
                schedule.schedule_id
            );
            return Ok(0);
        }
    };

    // Guard 2: Get when this schedule was created
    let schedule_created_at = get_schedule_creation_time(db, schedule)
        .await
        .map_err(|e| {
            error!(
                "Failed to get creation time for schedule {} from database: {}",
                schedule.schedule_id, e
            );
            e
        })?;

    // Guard 3: Get the last time this schedule executed (from log table)
    let last_execution_time = ScheduleLog::get_last_execution_time(db, &schedule.schedule_id)
        .await
        .map_err(|e| {
            error!(
                "Failed to get last execution time for schedule {} from database: {}",
                schedule.schedule_id, e
            );
            e
        })?;

    // Guard 4: Grace period for new schedules (don't backfill within 5 minutes of creation)
    // BUT: only apply grace period if the schedule has NEVER executed
    const GRACE_PERIOD_MINUTES: i64 = 5;
    if last_execution_time.is_none()
        && now - schedule_created_at < chrono::Duration::minutes(GRACE_PERIOD_MINUTES)
    {
        debug!(
            "Schedule {} within grace period ({} minutes) and has never executed, skipping backfill",
            schedule.schedule_id, GRACE_PERIOD_MINUTES
        );
        return Ok(0);
    }

    // Guard 5: Calculate check window start
    // Take the LATEST of: last_execution OR last_sync
    // (schedule_created_at is NOT included - it's only for the grace period check)
    let check_window_start = [last_execution_time, Some(last_sync)]
        .into_iter()
        .flatten()
        .max()
        .unwrap_or(last_sync);

    debug!(
        "Schedule {} ({}:{}): check_window_start={}, last_execution={:?}, last_sync={}, now={}",
        schedule.schedule_id,
        schedule.ns,
        schedule.var,
        check_window_start,
        last_execution_time,
        last_sync,
        now
    );

    // Guard 6: No window to check
    if check_window_start >= now {
        debug!(
            "Schedule {} ({}:{}): check_window_start >= now, no backfill needed",
            schedule.schedule_id, schedule.ns, schedule.var
        );
        return Ok(0);
    }

    // Calculate missed executions in the valid window
    let missed_executions =
        calculate_missed_executions(&schedule.cron, check_window_start, now).await?;

    if missed_executions.is_empty() {
        return Ok(0);
    }

    // Backfill each missed execution
    info!(
        "Found {} missed execution(s) for schedule {} ({}:{}) between {} and {}",
        missed_executions.len(),
        schedule.schedule_id,
        schedule.ns,
        schedule.var,
        check_window_start,
        now
    );

    for missed_time in &missed_executions {
        warn!(
            "Backfilling missed execution for schedule {} ({}:{}) that should have run at {}",
            schedule.schedule_id, schedule.ns, schedule.var, missed_time
        );

        queue_schedule_execution(db, event_queue, schedule, *missed_time, true).await?;
    }

    Ok(missed_executions.len())
}

/// The schedules whose build this runtime can run. A build it can't, or
/// can't look up now, keeps its schedules as they are.
async fn without_builds_awaiting_redeploy(
    db: &Arc<DatabasePool>,
    schedules: Vec<Schedule>,
) -> Vec<Schedule> {
    static WARNED: std::sync::OnceLock<std::sync::Mutex<ahash::AHashSet<Uuid>>> =
        std::sync::OnceLock::new();
    let mut paused: AHashMap<Uuid, bool> = AHashMap::new();
    for schedule in &schedules {
        if paused.contains_key(&schedule.build_id) {
            continue;
        }
        let reason = match hot::db::Build::get_build(db, &schedule.build_id).await {
            Ok(build) => build.runtime_version_error_message(),
            Err(_) => None,
        };
        if let Some(reason) = &reason {
            let first = WARNED
                .get_or_init(Default::default)
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .insert(schedule.build_id);
            if first {
                warn!(
                    "hot.dev: SCHEDULER pauses the schedules of build {} until it's redeployed: {}",
                    schedule.build_id, reason
                );
            }
        }
        paused.insert(schedule.build_id, reason.is_some());
    }
    schedules
        .into_iter()
        .filter(|schedule| !paused.get(&schedule.build_id).copied().unwrap_or(false))
        .collect()
}

/// Get the schedule creation time (from schedule.created_at or fallback to build.created_at)
async fn get_schedule_creation_time(
    db: &Arc<DatabasePool>,
    schedule: &Schedule,
) -> Result<chrono::DateTime<Utc>, Box<dyn std::error::Error + Send + Sync>> {
    // If schedule has created_at, use it
    if let Some(created_at) = schedule.created_at {
        return Ok(created_at);
    }

    // Otherwise, fall back to the build's created_at
    let build = hot::db::Build::get_build(db, &schedule.build_id)
        .await
        .map_err(|e| {
            error!(
                "Failed to get build {} from database for schedule {}: {}",
                schedule.build_id, schedule.schedule_id, e
            );
            e
        })?;
    Ok(build.created_at)
}

/// Calculate which times a schedule should have executed but didn't
async fn calculate_missed_executions(
    cron_expr: &str,
    start_time: chrono::DateTime<Utc>,
    end_time: chrono::DateTime<Utc>,
) -> Result<Vec<chrono::DateTime<Utc>>, Box<dyn std::error::Error + Send + Sync>> {
    let scheduler_cron = convert_to_scheduler_cron(cron_expr)?;
    let cron = hot::db::schedule::parse_cron(&scheduler_cron)?;

    let mut missed = Vec::new();
    let mut check_time = start_time;

    // Safety limit to prevent infinite loops on misconfigured crons
    const MAX_MISSED_EXECUTIONS: usize = 100;

    // Find all times the cron should have fired in the window
    while check_time < end_time && missed.len() < MAX_MISSED_EXECUTIONS {
        // Find the next occurrence after check_time
        let mut current = check_time;
        loop {
            if cron.is_time_matching(&current).unwrap_or(false) {
                // This time matches - add it if it's in range
                if current < end_time {
                    missed.push(current);
                }
                // Move to next second
                check_time = current + chrono::Duration::seconds(1);
                break;
            }

            // Move forward one second and keep looking
            current += chrono::Duration::seconds(1);

            // Give up if we've moved past the end
            if current >= end_time {
                check_time = end_time;
                break;
            }

            // Safety: don't loop forever
            if current - check_time > chrono::Duration::hours(24) {
                warn!(
                    "Stopped searching for cron matches after 24 hours from {}",
                    check_time
                );
                check_time = end_time;
                break;
            }
        }
    }

    if missed.len() >= MAX_MISSED_EXECUTIONS {
        warn!(
            "Truncated missed executions at {} (safety limit). Cron: {}",
            MAX_MISSED_EXECUTIONS, cron_expr
        );
    }

    Ok(missed)
}

/// Queue a schedule execution and log it
async fn queue_schedule_execution(
    db: &Arc<DatabasePool>,
    event_queue: &Arc<dyn Queue<Message>>,
    schedule: &Schedule,
    scheduled_time: chrono::DateTime<Utc>,
    is_backfill: bool,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let event_id = Uuid::now_v7();
    let run_id = Uuid::now_v7();

    // Look up the environment ID for this build
    let env_id = match hot::db::Build::get_env_id_for_build(db, &schedule.build_id).await {
        Ok(env_id) => env_id,
        Err(e) => {
            error!(
                "Failed to get environment for build {}: {}. Skipping scheduled event.",
                schedule.build_id, e
            );
            return Err(format!(
                "schedule build {} cannot be resolved: {}",
                schedule.build_id, e
            )
            .into());
        }
    };

    let (scoped_db, org_id) = admitted_environment_db(db, env_id).await?;
    let db = &Arc::new(scoped_db);

    // Get the build details
    let build = hot::db::Build::get_build(db, &schedule.build_id)
        .await
        .map_err(|e| {
            error!(
                "Failed to get build {} from database: {}",
                schedule.build_id, e
            );
            e
        })?;

    // Create a new stream ID for this scheduled event
    let stream_id = Uuid::now_v7();

    // Create the execution context
    let execution_context = ExecutionContext {
        run_id,
        stream_id,
        run_type_id: hot::db::run::RunType::Schedule.as_id(),
        env_id: Some(env_id),
        env_name: None, // Will be populated later if needed
        user_id: None,
        org_id: Some(org_id),
        org_slug: None, // Will be populated later if needed
        build_id: Some(schedule.build_id),
        build_hash: None,   // Will be populated later if needed
        project_id: None,   // Will be populated later if needed
        project_name: None, // Will be populated later if needed
        event_id: Some(event_id),
        origin_run_id: None,
        retry_attempt: 0,
        secret_keys: ahash::AHashSet::new(), // Will be populated from ctx metadata
        secret_value_hashes: ahash::AHashSet::new(),
        access_id: None, // Scheduler-initiated, no API access log
        agent_type: None,
        queue_timing: None,
        // Enqueue-only producer context; the worker stamps deadline_at at admission
        deadline_at: None,
        trace_calls: true,
    };

    // A dynamic schedule calls its function with the args it was created
    // with, as `::hot::test/run-due` does. A metadata schedule stores none and
    // passes one argument describing the schedule that fired.
    let args = match &schedule.value {
        Some(stored) => match serde_json::from_value::<Val>(stored.clone()) {
            Ok(args @ Val::Vec(_)) => args,
            _ => {
                return Err(format!(
                    "schedule {} has malformed args: {stored}",
                    schedule.schedule_id
                )
                .into());
            }
        },
        None => val!([val!({
            "type": "hot:schedule",
            "schedule_id": schedule.schedule_id.to_string(),
            "scheduled_at": scheduled_time.to_rfc3339(),
            "is_backfill": is_backfill,
            "backfilled_at": if is_backfill {
                Some(Utc::now().to_rfc3339())
            } else {
                None
            }
        })]),
    };

    // Create the event data. The worker attributes the run to its schedule
    // by `schedule_id`.
    let event_data = val!({
        "fn": format!("{}/{}", schedule.ns, schedule.var),
        "args": args,
        "schedule_id": schedule.schedule_id.to_string()
    });

    // Create the event
    let event = Event {
        event_id,
        env_id,
        stream_id,
        event_type: "hot:schedule".to_string(),
        event_data,
        event_time: scheduled_time, // Use the scheduled time, not now
        // Target project from build for routing tie-breaker
        target_project_id: Some(build.project_id),
        target_project_name: None, // Will be resolved by worker if needed
    };

    // Insert the event into the database
    hot::db::event::Event::insert_event(
        db,
        &event_id,
        &env_id,
        &stream_id,
        "hot:schedule",
        &serde_json::Value::from(&event.event_data),
        event.event_time,
        &build.created_by_user_id,
        None,
    )
    .await
    .map_err(|e| {
        error!(
            "Failed to insert schedule event for {}/{} into database: {}",
            schedule.ns, schedule.var, e
        );
        e
    })?;

    // Create event message head
    let mut head = AHashMap::new();
    head.insert("source".to_string(), "scheduler".to_string());
    head.insert("event_type".to_string(), "hot:schedule".to_string());
    head.insert("schedule_id".to_string(), schedule.schedule_id.to_string());
    head.insert(
        "function".to_string(),
        format!("{}/{}", schedule.ns, schedule.var),
    );
    head.insert("timestamp".to_string(), scheduled_time.to_rfc3339());
    if is_backfill {
        head.insert("is_backfill".to_string(), "true".to_string());
    }

    // Create the EventMessage
    let event_message = EventMessage {
        id: event_id,
        head,
        body: EventMessageBody {
            event,
            execution_context,
        },
    };

    // Convert to unified Message format
    let message: Message = event_message.into();

    // Send to event queue
    event_queue.enqueue(message).await.map_err(|e| {
        error!(
            "Failed to enqueue schedule event for {}/{}: {}",
            schedule.ns, schedule.var, e
        );
        e
    })?;

    // Log the execution
    ScheduleLog::insert(
        db,
        &schedule.schedule_id,
        Some(&event_id),
        scheduled_time,
        is_backfill,
    )
    .await
    .map_err(|e| {
        error!(
            "Failed to insert schedule log entry for {}/{} into database: {}",
            schedule.ns, schedule.var, e
        );
        e
    })?;

    if is_backfill {
        debug!(
            "Backfilled and logged schedule execution for {}/{} at {}",
            schedule.ns, schedule.var, scheduled_time
        );
    } else {
        debug!(
            "Queued and logged schedule execution for {}/{} at {}",
            schedule.ns, schedule.var, scheduled_time
        );
    }

    Ok(())
}

/// Format a duration in a human-readable way
fn format_duration(duration: chrono::Duration) -> String {
    let seconds = duration.num_seconds();
    if seconds < 60 {
        format!("{}s", seconds)
    } else if seconds < 3600 {
        format!("{}m {}s", seconds / 60, seconds % 60)
    } else {
        format!("{}h {}m", seconds / 3600, (seconds % 3600) / 60)
    }
}

/// Process runs that are pending retry and ready to be retried
/// Re-creates events for runs whose next_retry_at has passed
async fn process_pending_retries(
    db: &Arc<DatabasePool>,
    event_queue: &Arc<dyn Queue<Message>>,
    env_id: Option<Uuid>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Get runs pending retry (batch of 50)
    let pending_runs = hot::db::run::Run::get_pending_retries_for_environment(db, 50, env_id)
        .await
        .map_err(|e| {
            error!("Failed to get pending retries: {}", e);
            e
        })?;

    if pending_runs.is_empty() {
        return Ok(());
    }

    debug!(
        "hot.dev: SCHEDULER processing {} pending retries",
        pending_runs.len()
    );

    for run in pending_runs {
        let (scoped_db, org_id) = match admitted_environment_db(db, run.env_id).await {
            Ok(scope) => scope,
            Err(error) => {
                // Leave the due window instead of re-occupying every batch: a
                // suspended tenant's backlog is re-checked with growing backoff.
                let until = run.deferred_retry_at(Utc::now());
                if let Err(e) = hot::db::run::Run::defer_pending_retry(db, &run.run_id, until).await
                {
                    warn!(run_id = %run.run_id, error = %e, "Failed to defer pending retry");
                }
                debug!(run_id = %run.run_id, %error, %until, "Retry remains pending until environment is admitted");
                continue;
            }
        };
        let db = &Arc::new(scoped_db);
        // Get the original event to re-send. An attempt without one can't be
        // retried, and fails as the attempt it ran as.
        let event_id = match run.event_id {
            Some(id) => id,
            None => {
                warn!(
                    "Run {} has no event_id, cannot retry - marking as failed",
                    run.run_id
                );
                if let Err(e) = hot::db::run::Run::mark_retry_abandoned(db, &run.run_id).await {
                    error!("Failed to mark run {} as failed: {}", run.run_id, e);
                }
                continue;
            }
        };

        // Get the original event
        let original_event = match hot::db::Event::get_event(db, &event_id).await {
            Ok(evt) => evt,
            Err(hot::db::event::EventError::NotFound) => {
                warn!(
                    "Event {} for retry is gone, marking run {} as failed",
                    event_id, run.run_id
                );
                if let Err(e) = hot::db::run::Run::mark_retry_abandoned(db, &run.run_id).await {
                    error!("Failed to mark run {} as failed: {}", run.run_id, e);
                }
                continue;
            }
            Err(e) => {
                // A database error leaves the retry pending for the next tick.
                warn!(
                    "Failed to get event {} for retry of run {}: {}",
                    event_id, run.run_id, e
                );
                continue;
            }
        };

        // Create a new run_id for the retry
        let new_run_id = Uuid::now_v7();
        let new_event_id = Uuid::now_v7();

        // Create a new event for this retry (same data, new ID)
        let event_data_val: Val =
            serde_json::from_value(original_event.event_data.clone()).unwrap_or(Val::Null);

        // Insert the retry event into the database
        if let Err(e) = hot::db::Event::insert_event(
            db,
            &new_event_id,
            &run.env_id,
            &run.stream_id,
            &original_event.event_type,
            &original_event.event_data,
            Utc::now(),
            &run.by_user_id.unwrap_or_default(),
            None,
        )
        .await
        {
            error!("Failed to insert retry event for run {}: {}", run.run_id, e);
            continue;
        }

        // Create the execution context for the retry
        let execution_context = ExecutionContext {
            run_id: new_run_id,
            stream_id: run.stream_id,
            run_type_id: run.run_type_id,
            env_id: Some(run.env_id),
            env_name: None,
            user_id: run.by_user_id,
            org_id: Some(org_id),
            org_slug: None,
            build_id: run.build_id,
            build_hash: None,
            project_id: run.project_id,
            project_name: run.project_name.clone(),
            event_id: Some(new_event_id),
            origin_run_id: Some(run.run_id), // Link to the original failed run
            retry_attempt: run.retry_attempt, // Already incremented when marked as pending_retry
            secret_keys: ahash::AHashSet::new(), // Will be populated from ctx metadata
            secret_value_hashes: ahash::AHashSet::new(),
            access_id: run.access_id, // Propagate from original run if available
            agent_type: None,
            queue_timing: None,
            // Enqueue-only producer context; the worker stamps deadline_at at admission
            deadline_at: None,
            trace_calls: true,
        };

        // Create the event
        let event = Event {
            event_id: new_event_id,
            env_id: run.env_id,
            stream_id: run.stream_id,
            event_type: original_event.event_type.clone(),
            event_data: event_data_val,
            event_time: Utc::now(),
            // Propagate project context from original run for routing tie-breaker
            target_project_id: run.project_id,
            target_project_name: run.project_name.clone(),
        };

        // Create event message head
        let mut head = ahash::AHashMap::new();
        head.insert("source".to_string(), "retry_scheduler".to_string());
        head.insert("event_type".to_string(), original_event.event_type.clone());
        head.insert("retry_of_run_id".to_string(), run.run_id.to_string());
        head.insert("retry_attempt".to_string(), run.retry_attempt.to_string());
        head.insert("timestamp".to_string(), Utc::now().to_rfc3339());

        // Create the EventMessage
        let event_message = EventMessage {
            id: new_event_id,
            head,
            body: EventMessageBody {
                event,
                execution_context,
            },
        };

        // Convert to unified Message format
        let message: Message = event_message.into();

        // Send to event queue for worker processing
        match event_queue.enqueue(message).await {
            Ok(()) => {
                info!(
                    "hot.dev: SCHEDULER queued retry attempt {} for run {} (event_fn: {:?})",
                    run.retry_attempt, run.run_id, run.event_fn
                );

                // Mark the original run as failed now that we've queued a retry,
                // naming the retry: a separate run linked via origin_run_id
                if let Err(e) =
                    hot::db::run::Run::mark_retry_queued(db, &run.run_id, &new_run_id).await
                {
                    error!(
                        "Failed to mark run {} as failed after queueing retry: {}",
                        run.run_id, e
                    );
                }
            }
            Err(e) => {
                error!(
                    "hot.dev: SCHEDULER failed to queue retry for run {}: {}",
                    run.run_id, e
                );
                // Don't mark as failed - will retry on next tick
            }
        }
    }

    Ok(())
}

/// Process due @at: one-time schedules
/// These are schedules created via hot:schedule:new events with a one-time datetime
/// The cron field contains @at:2024-01-15T10:30:00Z format
async fn process_due_at_schedules(
    db: &Arc<DatabasePool>,
    event_queue: &Arc<dyn Queue<Message>>,
    env_id: Option<Uuid>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let now = Utc::now();

    // Get all active @at: schedules that are due (run_at <= now)
    let due_schedules = Schedule::get_due_at_schedules_for_environment(db, now, env_id)
        .await
        .map_err(|e| {
            error!("Failed to get due @at schedules from database: {}", e);
            e
        })?;

    if due_schedules.is_empty() {
        return Ok(());
    }

    debug!(
        "hot.dev: SCHEDULER found {} due @at schedule(s)",
        due_schedules.len()
    );

    for schedule_with_project in due_schedules {
        // Extract the scheduled time from the @at: prefix
        let scheduled_time = if let Some(datetime_str) = schedule_with_project
            .cron
            .strip_prefix(hot::db::AT_SCHEDULE_PREFIX)
        {
            match chrono::DateTime::parse_from_rfc3339(datetime_str) {
                Ok(dt) => dt.with_timezone(&Utc),
                Err(e) => {
                    error!(
                        "Failed to parse @at datetime '{}' for schedule {}: {}",
                        datetime_str, schedule_with_project.schedule_id, e
                    );
                    continue;
                }
            }
        } else {
            error!(
                "Schedule {} has cron field '{}' but doesn't start with @at: prefix",
                schedule_with_project.schedule_id, schedule_with_project.cron
            );
            continue;
        };

        // Convert ScheduleWithProject to Schedule for queue_schedule_execution
        let schedule = Schedule {
            schedule_id: schedule_with_project.schedule_id,
            build_id: schedule_with_project.build_id,
            cron: schedule_with_project.cron.clone(),
            ns: schedule_with_project.ns.clone(),
            var: schedule_with_project.var.clone(),
            meta: schedule_with_project.meta.clone(),
            value: schedule_with_project.value.clone(),
            file: schedule_with_project.file.clone(),
            line: schedule_with_project.line,
            column: schedule_with_project.column,
            position: schedule_with_project.position,
            active: schedule_with_project.active,
            created_at: schedule_with_project.created_at,
            deactivated_at: schedule_with_project.deactivated_at,
        };

        info!(
            "hot.dev: SCHEDULER executing @at schedule {} ({}:{}) scheduled for {}",
            schedule.schedule_id, schedule.ns, schedule.var, scheduled_time
        );

        // Queue the execution
        if let Err(e) =
            queue_schedule_execution(db, event_queue, &schedule, scheduled_time, false).await
        {
            error!(
                "Failed to queue @at schedule execution for {}:{}: {}",
                schedule.ns, schedule.var, e
            );
            continue;
        }

        // Mark the schedule as inactive (one-time schedules are done after execution)
        if let Err(e) = Schedule::cancel_schedule(db, &schedule.schedule_id).await {
            error!(
                "Failed to deactivate @at schedule {} after execution: {}",
                schedule.schedule_id, e
            );
        } else {
            debug!(
                "Deactivated @at schedule {} after execution",
                schedule.schedule_id
            );
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn singleton_check_fails_closed() {
        let limit = Duration::from_millis(20);
        assert!(
            confirm_singleton_lock(async { Ok::<_, String>(true) }, limit)
                .await
                .is_ok()
        );
        assert!(
            confirm_singleton_lock(async { Ok::<_, String>(false) }, limit)
                .await
                .unwrap_err()
                .to_string()
                .contains("no longer holds")
        );
        assert!(
            confirm_singleton_lock(async { Err::<bool, _>("connection reset") }, limit)
                .await
                .unwrap_err()
                .to_string()
                .contains("connection reset")
        );
        // A partitioned session never answers; the check must not wait for TCP.
        let hung = std::future::pending::<Result<bool, String>>();
        assert_eq!(
            confirm_singleton_lock(hung, limit).await,
            Err(LockCheckFailure::Slow(limit))
        );
    }

    /// Answers each check as scripted: `Some(answer)` at once, `None` never.
    /// After the script it keeps confirming the lock.
    struct ScriptedSession(std::collections::VecDeque<Option<Result<bool, String>>>);

    impl LockSession for ScriptedSession {
        async fn still_held(&mut self) -> Result<bool, String> {
            match self.0.pop_front() {
                Some(Some(answer)) => answer,
                Some(None) => std::future::pending().await,
                None => Ok(true),
            }
        }
    }

    async fn watch_script(
        script: Vec<Option<Result<bool, String>>>,
        within: Duration,
    ) -> Option<Option<String>> {
        let mut session = ScriptedSession(script.into());
        let (_shutdown_tx, mut shutdown_rx) = tokio::sync::broadcast::channel(1);
        tokio::time::timeout(
            within,
            watch_singleton_lock(
                &mut session,
                Duration::from_millis(20),
                Duration::from_millis(10),
                &mut shutdown_rx,
            ),
        )
        .await
        .ok()
    }

    #[tokio::test]
    async fn a_stalled_lock_check_is_lost_only_after_consecutive_slow_checks() {
        // A stall that delays two checks in a row, twice, keeps the lock: an
        // answer in between resets the count.
        let stalls = vec![None, None, Some(Ok(true)), None, None];
        assert_eq!(
            watch_script(stalls, Duration::from_millis(600)).await,
            None,
            "the scheduler keeps running"
        );

        let lost = watch_script(vec![None, None, None], Duration::from_secs(5))
            .await
            .expect("three slow checks in a row count as lost")
            .expect("a lost lock has a reason");
        assert!(lost.contains("3 times in a row"), "{lost}");

        // An answer that the lock is gone, or a failed session, counts at once.
        let gone = watch_script(vec![Some(Ok(false))], Duration::from_secs(5)).await;
        assert!(gone.flatten().unwrap().contains("no longer holds"));
        let failed = watch_script(
            vec![None, Some(Err("connection reset".into()))],
            Duration::from_secs(5),
        )
        .await;
        assert!(failed.flatten().unwrap().contains("connection reset"));
    }

    #[tokio::test]
    async fn watching_the_lock_ends_on_shutdown() {
        let mut session = ScriptedSession(Default::default());
        let (shutdown_tx, mut shutdown_rx) = tokio::sync::broadcast::channel(1);
        shutdown_tx.send(()).unwrap();
        let lost = watch_singleton_lock(
            &mut session,
            Duration::from_millis(20),
            Duration::from_millis(10),
            &mut shutdown_rx,
        )
        .await;
        assert_eq!(lost, None);
    }

    fn scripted_starts(
        script: Vec<SchedulerResult>,
    ) -> (
        Arc<std::sync::Mutex<usize>>,
        impl FnMut() -> std::future::Ready<SchedulerResult>,
    ) {
        let starts = Arc::new(std::sync::Mutex::new(0));
        let counter = Arc::clone(&starts);
        let mut script = std::collections::VecDeque::from(script);
        let start = move || {
            *counter.lock().unwrap() += 1;
            std::future::ready(script.pop_front().unwrap_or(Ok(())))
        };
        (starts, start)
    }

    fn lost() -> SchedulerResult {
        Err(Box::new(SingletonLockLost))
    }

    #[tokio::test]
    async fn development_scheduler_restarts_after_losing_its_lock() {
        let backoff = Duration::from_millis(1);
        // Lost twice; the first restart finds the old session still holding
        // the lock, the next one runs until shutdown.
        let (starts, start) = scripted_starts(vec![
            lost(),
            Err("another hot_scheduler instance already holds the scheduler singleton lock".into()),
            lost(),
            Ok(()),
        ]);
        restart_after_lock_loss(start, backoff, backoff, std::future::pending::<()>)
            .await
            .unwrap();
        assert_eq!(*starts.lock().unwrap(), 4);

        // A scheduler that never started (here, its configuration) is not
        // retried.
        let (starts, start) = scripted_starts(vec![Err("singleton=false".into())]);
        let error = restart_after_lock_loss(start, backoff, backoff, std::future::pending::<()>)
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), "singleton=false");
        assert_eq!(*starts.lock().unwrap(), 1);

        // Shutdown during the backoff ends it without another start.
        let (starts, start) = scripted_starts(vec![lost()]);
        restart_after_lock_loss(start, Duration::from_secs(3600), backoff, || {
            std::future::ready(())
        })
        .await
        .unwrap();
        assert_eq!(*starts.lock().unwrap(), 1);
    }

    #[test]
    fn test_convert_to_scheduler_cron() {
        // Test traditional cron expressions (should pass through unchanged)
        assert_eq!(
            convert_to_scheduler_cron("0 30 9 * * MON").unwrap(),
            "0 30 9 * * MON"
        );
        assert_eq!(
            convert_to_scheduler_cron("*/5 * * * * *").unwrap(),
            "*/5 * * * * *"
        );

        // Test English expressions (should be converted)
        let result = convert_to_scheduler_cron("every second").unwrap();
        assert!(result.contains("*")); // Should be converted to a cron expression
        println!("'every second' -> '{}'", result);

        let result = convert_to_scheduler_cron("every minute").unwrap();
        assert!(result.contains("*")); // Should be converted to a cron expression
        println!("'every minute' -> '{}'", result);

        // Test invalid expressions (should fail)
        assert!(convert_to_scheduler_cron("invalid cron").is_err());
    }

    #[tokio::test]
    async fn test_env_id_lookup() {
        use hot::val;

        println!("Testing environment ID lookup functionality...");

        // Test that our database chain lookup function exists and can be called
        // This test doesn't test with real data since it would require a full database setup,
        // but it verifies the function signature and that it compiles
        let build_id = uuid::Uuid::now_v7();

        // Create an in-memory database for testing (this will fail but we're testing the call path)
        let conf = val!({
            "db": {
                "uri": "sqlite::memory:"
            }
        });

        match hot::db::create_db_pool(&conf).await {
            Ok(db) => {
                // This will likely fail since we don't have the schema setup, but that's fine
                // We're just testing that the function call path is correct
                match hot::db::Build::get_env_id_for_build(&db, &build_id).await {
                    Ok(env_id) => {
                        println!("✅ Environment ID lookup succeeded: {}", env_id);
                    }
                    Err(e) => {
                        println!(
                            "⚠️  Environment ID lookup failed (expected without proper schema): {}",
                            e
                        );
                        // This is expected since we don't have proper test data
                    }
                }
            }
            Err(e) => {
                println!("⚠️  Database creation failed (expected in test): {}", e);
            }
        }

        println!("✅ Environment ID lookup function call path verified");
    }

    #[tokio::test]
    async fn queued_schedule_execution_context_carries_no_deadline() {
        let db = Arc::new(hot::db::test_db().await);
        let data = hot::db::insert_test_data(&db).await.unwrap();

        // Unique queue name so this test never sees other tests' messages.
        let queue: Arc<dyn Queue<Message>> = Arc::new(
            MemQueue::new(format!("test:schedule-deadline:{}", Uuid::now_v7()))
                .expect("memory queue"),
        );

        let schedule = Schedule {
            schedule_id: Uuid::now_v7(),
            build_id: data.build_id,
            cron: "0 0 * * * *".to_string(),
            ns: "test".to_string(),
            var: "tick".to_string(),
            meta: None,
            value: None,
            file: None,
            line: None,
            column: None,
            position: None,
            active: true,
            created_at: None,
            deactivated_at: None,
        };

        let DatabasePool::Sqlite(pool) = db.as_ref() else {
            unreachable!()
        };
        sqlx::query("UPDATE org SET lifecycle_state = 'suspended' WHERE org_id = ?")
            .bind(data.org_id)
            .execute(pool)
            .await
            .unwrap();
        assert!(
            queue_schedule_execution(&db, &queue, &schedule, Utc::now(), false)
                .await
                .is_err()
        );
        assert!(
            queue.is_empty().await.unwrap(),
            "Suspended schedules must not enqueue execution"
        );
        sqlx::query("UPDATE org SET lifecycle_state = 'active' WHERE org_id = ?")
            .bind(data.org_id)
            .execute(pool)
            .await
            .unwrap();
        queue_schedule_execution(&db, &queue, &schedule, Utc::now(), false)
            .await
            .expect("queue_schedule_execution should succeed");

        let message = queue
            .dequeue()
            .await
            .expect("dequeue should not fail")
            .expect("schedule execution should be enqueued");
        let event_message =
            EventMessage::try_from(message).expect("queued message should decode as EventMessage");
        assert!(
            event_message.body.execution_context.deadline_at.is_none(),
            "scheduler producer contexts must not carry a deadline; the worker stamps it at admission"
        );
        assert_eq!(
            event_message.body.execution_context.run_type_id,
            hot::db::run::RunType::Schedule.as_id()
        );
    }

    /// The event data the worker runs for `schedule`: the queued message
    /// names the event, and the worker reads its data from the database.
    async fn scheduled_event_data(db: &Arc<DatabasePool>, schedule: &Schedule) -> Val {
        let queue: Arc<dyn Queue<Message>> = Arc::new(
            MemQueue::new(format!("test:schedule-args:{}", Uuid::now_v7())).expect("memory queue"),
        );
        queue_schedule_execution(db, &queue, schedule, Utc::now(), false)
            .await
            .expect("queue_schedule_execution should succeed");
        let message = queue.dequeue().await.unwrap().expect("an enqueued event");
        let event = EventMessage::try_from(message)
            .expect("queued message should decode as EventMessage")
            .body
            .event;
        assert_eq!(event.event_type, "hot:schedule");
        let stored = hot::db::Event::get_event(db, &event.event_id)
            .await
            .unwrap();
        serde_json::from_value(stored.event_data).unwrap()
    }

    #[tokio::test]
    async fn dynamic_schedules_call_their_function_with_their_args() {
        let db = Arc::new(hot::db::test_db().await);
        let data = hot::db::insert_test_data(&db).await.unwrap();

        // As `::hot::schedule/create` stores a `hot:schedule:new` event's args.
        let create = |var: &'static str, args: Option<Val>| {
            let db = Arc::clone(&db);
            async move {
                let schedule_id = Uuid::now_v7();
                let stored =
                    args.map(|args| serde_json::to_value(args.to_hot_data_repr()).unwrap());
                Schedule::insert_dynamic_schedule(
                    &db,
                    &schedule_id,
                    &data.build_id,
                    &hot::db::ScheduleType::Cron("0 0 * * * *".into()),
                    Some(&data.org_id),
                    SchedulePolicy::self_hosted_default(),
                    "::app::steps",
                    var,
                    None,
                    stored.as_ref(),
                )
                .await
                .unwrap();
                Schedule::get_schedule(&db, &schedule_id).await.unwrap()
            }
        };

        let with_args = create("check-due", Some(val!(["s-1"]))).await;
        let event_data = scheduled_event_data(&db, &with_args).await;
        assert_eq!(event_data.get("fn"), Some(val!("::app::steps/check-due")));
        assert_eq!(event_data.get("args"), Some(val!(["s-1"])));
        // The worker attributes the run to its schedule by this id.
        assert_eq!(
            event_data.get("schedule_id"),
            Some(val!(with_args.schedule_id.to_string()))
        );

        // What the worker's `schedule-event-handler` then runs: a target that
        // declares `fn (id: Str)` receives the id.
        let source = format!(
            "::app::steps ns\n\
             check-due fn (id: Str): Str {{ `late ${{id}}` }}\n\
             call(resolve({}), {})",
            event_data.get("fn").unwrap(),
            event_data.get("args").unwrap()
        );
        let result = tokio::task::spawn_blocking(move || {
            hot::lang::engine::Engine::new().eval_code(
                &source,
                &[],
                &[],
                Some(&Val::map_empty()),
                None,
            )
        })
        .await
        .unwrap();
        assert_eq!(result, Ok(val!("late s-1")));

        // No args call the function with none.
        let without_args = create("sweep", None).await;
        let event_data = scheduled_event_data(&db, &without_args).await;
        assert_eq!(event_data.get("args"), Some(val!([])));
    }

    #[tokio::test]
    async fn metadata_schedules_pass_the_schedule_that_fired() {
        let db = Arc::new(hot::db::test_db().await);
        let data = hot::db::insert_test_data(&db).await.unwrap();
        let schedule = Schedule {
            schedule_id: Uuid::now_v7(),
            build_id: data.build_id,
            cron: "0 0 * * * *".to_string(),
            ns: "::app::jobs".to_string(),
            var: "nightly".to_string(),
            meta: Some(serde_json::json!({"schedule": "0 0 * * * *"})),
            value: None,
            file: None,
            line: None,
            column: None,
            position: None,
            active: true,
            created_at: None,
            deactivated_at: None,
        };

        let event_data = scheduled_event_data(&db, &schedule).await;
        let Some(Val::Vec(args)) = event_data.get("args") else {
            panic!("args are a Vec: {event_data}");
        };
        assert_eq!(args.len(), 1);
        assert_eq!(args[0].get("type"), Some(val!("hot:schedule")));
        assert_eq!(
            args[0].get("schedule_id"),
            Some(val!(schedule.schedule_id.to_string()))
        );
        assert_eq!(
            event_data.get("schedule_id"),
            Some(val!(schedule.schedule_id.to_string()))
        );
    }

    #[tokio::test]
    async fn requeued_retry_context_carries_no_deadline() {
        let db = Arc::new(hot::db::test_db().await);
        let data = hot::db::insert_test_data(&db).await.unwrap();

        // Give the fixture run an original event to retry from.
        let event_id = Uuid::now_v7();
        hot::db::event::Event::insert_event(
            &db,
            &event_id,
            &data.env_id,
            &data.stream_id,
            "order:created",
            &serde_json::json!({"fn": "test/handler"}),
            Utc::now(),
            &data.user_id,
            None,
        )
        .await
        .expect("insert original event");

        // Mark the fixture run as pending retry with a next_retry_at already due.
        let hot::db::DatabasePool::Sqlite(pool) = db.as_ref() else {
            panic!("test_db should be SQLite");
        };
        sqlx::query(
            "UPDATE run SET status_id = ?, retry_attempt = 1, next_retry_at = ?, event_id = ? WHERE run_id = ?",
        )
        .bind(hot::db::run::RunStatus::PendingRetry.as_id())
        .bind(Utc::now() - chrono::Duration::seconds(60))
        .bind(event_id)
        .bind(data.run_id)
        .execute(pool)
        .await
        .expect("mark run pending retry");

        let queue: Arc<dyn Queue<Message>> = Arc::new(
            MemQueue::new(format!("test:retry-deadline:{}", Uuid::now_v7())).expect("memory queue"),
        );

        process_pending_retries(&db, &queue, Some(Uuid::now_v7()))
            .await
            .unwrap();
        assert!(
            queue.is_empty().await.unwrap(),
            "Another profile's retries remain pending"
        );
        process_pending_retries(&db, &queue, Some(data.env_id))
            .await
            .expect("process_pending_retries should succeed");

        let message = queue
            .dequeue()
            .await
            .expect("dequeue should not fail")
            .expect("retry should be enqueued");
        let event_message =
            EventMessage::try_from(message).expect("queued message should decode as EventMessage");
        assert!(
            event_message.body.execution_context.deadline_at.is_none(),
            "retry producer contexts must not carry a deadline; the worker stamps it at admission"
        );
        assert_eq!(
            event_message.body.execution_context.origin_run_id,
            Some(data.run_id)
        );
        assert_eq!(event_message.body.execution_context.retry_attempt, 1);
    }

    /// The fixture run's status, retry attempt and info.
    async fn run_state(db: &DatabasePool, run_id: Uuid) -> (i16, i32, Option<serde_json::Value>) {
        let DatabasePool::Sqlite(pool) = db else {
            panic!("test_db should be SQLite");
        };
        let (status, attempt, info): (i16, i32, Option<String>) =
            sqlx::query_as("SELECT status_id, retry_attempt, info FROM run WHERE run_id = ?")
                .bind(run_id)
                .fetch_one(pool)
                .await
                .unwrap();
        (
            status,
            attempt,
            info.map(|info| serde_json::from_str(&info).unwrap()),
        )
    }

    #[tokio::test]
    async fn a_queued_retry_is_named_on_its_attempt() {
        let db = Arc::new(hot::db::test_db().await);
        let data = hot::db::insert_test_data(&db).await.unwrap();
        let event_id = Uuid::now_v7();
        hot::db::event::Event::insert_event(
            &db,
            &event_id,
            &data.env_id,
            &data.stream_id,
            "order:created",
            &serde_json::json!({"fn": "test/handler"}),
            Utc::now(),
            &data.user_id,
            None,
        )
        .await
        .unwrap();
        let hot::db::DatabasePool::Sqlite(pool) = db.as_ref() else {
            panic!("test_db should be SQLite");
        };
        sqlx::query(
            "UPDATE run SET status_id = ?, retry_attempt = 1, next_retry_at = ?, event_id = ?, info = ? WHERE run_id = ?",
        )
        .bind(hot::db::run::RunStatus::PendingRetry.as_id())
        .bind(Utc::now() - chrono::Duration::seconds(60))
        .bind(event_id)
        .bind(r#"{"warnings":["kept"]}"#)
        .bind(data.run_id)
        .execute(pool)
        .await
        .unwrap();
        let queue: Arc<dyn Queue<Message>> = Arc::new(
            MemQueue::new(format!("test:retry-named:{}", Uuid::now_v7())).expect("memory queue"),
        );

        process_pending_retries(&db, &queue, None).await.unwrap();

        let message = queue
            .dequeue()
            .await
            .unwrap()
            .expect("retry should be enqueued");
        let retry_run_id = EventMessage::try_from(message)
            .unwrap()
            .body
            .execution_context
            .run_id;
        let (status, attempt, info) = run_state(&db, data.run_id).await;
        assert_eq!(status, hot::db::run::RunStatus::Failed.as_id());
        assert_eq!(attempt, 1, "a retried attempt keeps its raised attempt");
        let info = info.expect("the attempt records its retry");
        assert_eq!(info["retry_run_id"], retry_run_id.to_string());
        assert_eq!(info["warnings"], serde_json::json!(["kept"]));
    }

    #[tokio::test]
    async fn an_unretryable_attempt_fails_as_the_attempt_it_ran_as() {
        let db = Arc::new(hot::db::test_db().await);
        let data = hot::db::insert_test_data(&db).await.unwrap();
        let hot::db::DatabasePool::Sqlite(pool) = db.as_ref() else {
            panic!("test_db should be SQLite");
        };
        // The worker raised retry_attempt from 1 to 2 when it scheduled the
        // retry; the attempt has no event to retry from.
        sqlx::query(
            "UPDATE run SET status_id = ?, retry_attempt = 2, next_retry_at = ?, event_id = NULL WHERE run_id = ?",
        )
        .bind(hot::db::run::RunStatus::PendingRetry.as_id())
        .bind(Utc::now() - chrono::Duration::seconds(60))
        .bind(data.run_id)
        .execute(pool)
        .await
        .unwrap();
        let queue: Arc<dyn Queue<Message>> = Arc::new(
            MemQueue::new(format!("test:retry-unretryable:{}", Uuid::now_v7()))
                .expect("memory queue"),
        );

        process_pending_retries(&db, &queue, None).await.unwrap();
        process_pending_retries(&db, &queue, None).await.unwrap();

        assert!(queue.is_empty().await.unwrap(), "nothing to retry");
        let (status, attempt, _) = run_state(&db, data.run_id).await;
        assert_eq!(status, hot::db::run::RunStatus::Failed.as_id());
        assert_eq!(attempt, 1, "back at the attempt it ran as, once");
    }

    #[tokio::test]
    async fn suspended_retries_are_deferred_out_of_the_due_batch() {
        let db = Arc::new(hot::db::test_db().await);
        let data = hot::db::insert_test_data(&db).await.unwrap();
        let hot::db::DatabasePool::Sqlite(pool) = db.as_ref() else {
            panic!("test_db should be SQLite");
        };
        let due = Utc::now() - chrono::Duration::seconds(60);
        sqlx::query(
            "UPDATE run SET status_id = ?, retry_attempt = 1, next_retry_at = ? WHERE run_id = ?",
        )
        .bind(hot::db::run::RunStatus::PendingRetry.as_id())
        .bind(due)
        .bind(data.run_id)
        .execute(pool)
        .await
        .unwrap();
        sqlx::query("UPDATE env SET active = 0 WHERE env_id = ?")
            .bind(data.env_id)
            .execute(pool)
            .await
            .unwrap();
        let queue: Arc<dyn Queue<Message>> = Arc::new(
            MemQueue::new(format!("test:retry-suspended:{}", Uuid::now_v7()))
                .expect("memory queue"),
        );

        process_pending_retries(&db, &queue, None).await.unwrap();

        assert!(
            queue.dequeue().await.unwrap().is_none(),
            "suspended work is not enqueued"
        );
        let (status, next): (i16, Option<chrono::DateTime<Utc>>) =
            sqlx::query_as("SELECT status_id, next_retry_at FROM run WHERE run_id = ?")
                .bind(data.run_id)
                .fetch_one(pool)
                .await
                .unwrap();
        assert_eq!(status, hot::db::run::RunStatus::PendingRetry.as_id());
        assert!(
            next.is_some_and(|next| next > Utc::now()),
            "retry left the due window"
        );
        assert!(
            hot::db::run::Run::get_pending_retries(&db, 50)
                .await
                .unwrap()
                .is_empty()
        );
    }
}

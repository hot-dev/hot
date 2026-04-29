use ahash::AHashMap;
use chrono::Utc;
use croner::Cron;
use hot::data::msg::Message;
use hot::data::serialization::Serialization;
use hot::db::{DatabasePool, Schedule, ScheduleLog, SchedulerState};
use hot::env::is_local_dev;
use hot::lang::event::{Event, EventMessage, EventMessageBody, ExecutionContext};
use hot::queue::{Queue, QueueType, mem::MemQueue, streams::RedisStreamQueue};
use hot::val;
use hot::val::Val;
use std::str::FromStr;
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
    if Cron::from_str(cron_expr).is_ok() {
        return Ok(cron_expr.to_string());
    }

    // If traditional cron fails, try English-to-cron conversion
    match english_to_cron::str_cron_syntax(cron_expr) {
        Ok(converted_cron) => {
            // Validate the converted expression works with croner
            Cron::from_str(&converted_cron).map_err(|e| {
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

pub const DEFAULT_QUEUE_TYPE: QueueType = QueueType::Memory;
pub const DEFAULT_SERIALIZATION: Serialization = Serialization::ZstdJson; // must match Serialization's #[default]
pub const DEFAULT_REDIS_URL: &str = "redis://localhost:6379";
pub const DEFAULT_SYNC_INTERVAL_SECONDS: u64 = 30; // Sync with database every 30 seconds

pub fn get_resolved_conf(conf: Val) -> Val {
    // Start with defaults
    let default_conf = val!({
        "sync-interval-seconds": DEFAULT_SYNC_INTERVAL_SECONDS as i64,
        "backfill": false
    });

    // Merge with provided conf (the provided conf will override defaults)
    default_conf.merge(&conf)
}

pub async fn run(
    queue_type: QueueType,
    redis_uri: Option<String>,
    redis_cluster: bool,
    serialization: Serialization,
    db: Option<DatabasePool>,
    sync_interval_seconds: Option<u64>,
    backfill_enabled: bool,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    debug!("hot.dev: SCHEDULER starting");

    // Create queue for sending scheduled events to workers
    let event_queue: Arc<dyn Queue<Message>> = match queue_type {
        QueueType::Memory => {
            let queue = MemQueue::<Message>::new("hot:event".to_string())?
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
        QueueType::Redis => {
            // Test Redis connection with a simple operation
            match event_queue.is_empty().await {
                Ok(_) => {
                    debug!("hot.dev: SCHEDULER successfully connected to Redis queue");
                }
                Err(e) => {
                    error!("hot.dev: SCHEDULER failed to connect to Redis queue: {}", e);
                    return Err(format!("Redis queue connectivity check failed: {}", e).into());
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

    // If database is provided, start the sync loop and retry processor
    if let Some(db) = db {
        let db = Arc::new(db);
        let sync_interval = sync_interval_seconds.unwrap_or(DEFAULT_SYNC_INTERVAL_SECONDS);

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

        // Start the retry processor task (checks for pending retries every 5 seconds)
        let retry_queue_clone = Arc::clone(&event_queue);
        let retry_db_clone = Arc::clone(&db);
        let mut shutdown_rx = shutdown_tx.subscribe();

        tokio::spawn(async move {
            let mut retry_timer = interval(Duration::from_secs(5));

            loop {
                tokio::select! {
                    _ = retry_timer.tick() => {
                        if let Err(e) = process_pending_retries(&retry_db_clone, &retry_queue_clone).await {
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

        // Start the @at: schedule processor task (checks for due one-time schedules every 5 seconds)
        let at_queue_clone = Arc::clone(&event_queue);
        let at_db_clone = Arc::clone(&db);
        let mut shutdown_rx = shutdown_tx.subscribe();

        tokio::spawn(async move {
            let mut at_timer = interval(Duration::from_secs(5));

            loop {
                tokio::select! {
                    _ = at_timer.tick() => {
                        if let Err(e) = process_due_at_schedules(&at_db_clone, &at_queue_clone).await {
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
                        info!("hot.dev: SCHEDULER enqueuing daily maintenance task");
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
    } else {
        warn!("hot.dev: SCHEDULER running without database - no schedule sync will occur");
    }

    // Start the scheduler
    scheduler.start().await?;

    info!("hot.dev: SCHEDULER started and running");

    // Wait for SIGINT or SIGTERM
    hot::signal::shutdown_signal().await;
    info!("hot.dev: SCHEDULER shutting down gracefully");
    let _ = shutdown_tx.send(());
    scheduler.shutdown().await?;

    Ok(())
}

/// Sync with database to update running scheduled jobs
async fn sync_with_database(
    scheduler: &JobScheduler,
    event_queue: &Arc<dyn Queue<Message>>,
    running_jobs: &Arc<RwLock<AHashMap<Uuid, ScheduledJobInfo>>>,
    db: &Arc<DatabasePool>,
    backfill_enabled: bool,
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
        info!("hot.dev: SCHEDULER first sync - no previous sync time found");
    }

    // Get all schedules for deployed builds
    let schedules = Schedule::get_schedules_for_deployed_builds(db, None, None)
        .await
        .map_err(|e| {
            error!(
                "Failed to get schedules for deployed builds from database: {}",
                e
            );
            e
        })?;

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
                    info!(
                        "hot.dev: SCHEDULER schedule {} moved from build {} to build {}",
                        existing_job.schedule_id, existing_job.build_id, schedule.build_id
                    );
                }
                if existing_job.cron != schedule.cron {
                    info!(
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
                match create_scheduled_job(db, scheduler, event_queue, &schedule).await {
                    Ok(job_info) => {
                        jobs_map.insert(schedule.schedule_id, job_info);
                        updated_count += 1;
                        info!(
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
            match create_scheduled_job(db, scheduler, event_queue, &schedule).await {
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

            info!(
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

    // Convert English expressions to cron format for the scheduler
    // This ensures the Job constructor gets a valid cron expression
    let scheduler_cron = convert_to_scheduler_cron(&cron)?;

    // Clone data for the job closure
    let queue_clone = Arc::clone(event_queue);
    let db_clone = Arc::clone(db);
    let ns_clone = ns.clone();
    let var_clone = var.clone();
    let schedule_id_clone = schedule_id;
    let build_id_clone = build_id;

    // Create the scheduled job using the converted cron expression
    let job = Job::new_async(&scheduler_cron, move |_uuid, _lock| {
        let queue = Arc::clone(&queue_clone);
        let db = Arc::clone(&db_clone);
        let ns = ns_clone.clone();
        let var = var_clone.clone();
        let schedule_id = schedule_id_clone;
        let build_id = build_id_clone;

        Box::pin(async move {
            // Create a scheduled event message
            let event_id = Uuid::now_v7();
            let run_id = Uuid::now_v7();

            // Look up the environment ID for this build
            let env_id = match hot::db::Build::get_env_id_for_build(&db, &build_id).await {
                Ok(env_id) => env_id,
                Err(e) => {
                    error!(
                        "hot.dev: SCHEDULER failed to get environment for build {}: {}. Skipping scheduled event.",
                        build_id, e
                    );
                    return;
                }
            };

            // Get the build details to access the created_by_user_id
            let build = match hot::db::Build::get_build(&db, &build_id).await {
                Ok(build) => build,
                Err(e) => {
                    error!(
                        "hot.dev: SCHEDULER failed to get build {} details: {}",
                        build_id, e
                    );
                    return; // Skip this scheduled event if we can't get build details
                }
            };

            // Create a new stream ID for this scheduled event (scheduled events start new streams)
            let stream_id = Uuid::now_v7();

            // Create the execution context for the schedule
            let execution_context = ExecutionContext {
                run_id,
                stream_id,
                run_type_id: hot::db::run::RunType::Schedule.as_id(),
                env_id: Some(env_id),  // Now we have the actual environment ID
                env_name: None, // Will be populated later if needed
                user_id: None, // Will be set by worker from build.created_by_user_id
                org_id: None,  // Will be set when processed by worker
                org_slug: None, // Will be populated later if needed
                build_id: Some(build_id),
                build_hash: None, // Will be populated later if needed
                project_id: None, // Will be populated later if needed
                project_name: None, // Will be populated later if needed
                event_id: Some(event_id), // Now set since event is inserted into DB
                origin_run_id: None, // No origin_run_id for scheduled events
                retry_attempt: 0,
                secret_keys: ahash::AHashSet::new(), // Will be populated from ctx metadata
                secret_value_hashes: ahash::AHashSet::new(),
                access_id: None, // Scheduler-initiated, no API access log
                agent_type: None,
            };

            // Create the event data for the generic schedule-event-handler
            // The handler will call the scheduled function with args
            let event_data = val!({
                "fn": format!("{}/{}", ns, var),
                "args": [val!({
                    "type": "hot:schedule",
                    "schedule_id": schedule_id.to_string(),
                    "scheduled_at": Utc::now().to_rfc3339()
                })]
            });

            // Create the event
            let event = Event {
                event_id,
                env_id, // Now using the actual environment ID
                stream_id,
                event_type: "hot:schedule".to_string(),
                event_data,
                event_time: Utc::now(),
                // Target project from build for routing tie-breaker
                target_project_id: Some(build.project_id),
                target_project_name: None, // Will be resolved by worker if needed
            };

            // Insert the event into the database before sending to worker
            // This ensures the event exists when the worker creates the run record
            match hot::db::event::Event::insert_event(
                &db,
                &event_id,
                &env_id,
                &stream_id,
                "hot:schedule",
                &serde_json::Value::from(&event.event_data),
                event.event_time,
                &build.created_by_user_id, // Use the build creator as the event creator
                None,
            ).await {
                Ok(_) => {
                    // Event successfully inserted, continue to send message
                }
                Err(e) => {
                    error!(
                        "hot.dev: SCHEDULER failed to insert event for {}/{}: {}",
                        ns, var, e
                    );
                    return; // Skip sending message if event insertion failed
                }
            }

            // Create event message head
            let mut head = AHashMap::new();
            head.insert("source".to_string(), "scheduler".to_string());
            head.insert("event_type".to_string(), "hot:schedule".to_string());
            head.insert("schedule_id".to_string(), schedule_id.to_string());
            head.insert("function".to_string(), format!("{}/{}", ns, var));
            head.insert("timestamp".to_string(), Utc::now().to_rfc3339());

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

            // Send to event queue for worker processing
            match queue.enqueue(message).await {
                Ok(_) => {
                    debug!(
                        "hot.dev: SCHEDULER sent scheduled event for {}/{} to hot:event queue",
                        ns, var
                    );

                    // Log the execution to schedule_log
                    let now = Utc::now();
                    if let Err(e) = ScheduleLog::insert(&db, &schedule_id, Some(&event_id), now, false).await {
                        error!(
                            "hot.dev: SCHEDULER failed to log execution for {}/{}: {}",
                            ns, var, e
                        );
                        // Don't fail the job - the execution still happened
                    }
                }
                Err(e) => {
                    error!(
                        "hot.dev: SCHEDULER failed to send scheduled event for {}/{}: {}",
                        ns, var, e
                    );
                }
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
    let cron = Cron::from_str(&scheduler_cron)?;

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
            return Ok(());
        }
    };

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
        org_id: None,
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
    };

    // Create the event data
    let event_data = val!({
        "fn": format!("{}/{}", schedule.ns, schedule.var),
        "args": [val!({
            "type": "hot:schedule",
            "schedule_id": schedule.schedule_id.to_string(),
            "scheduled_at": scheduled_time.to_rfc3339(),
            "is_backfill": is_backfill,
            "backfilled_at": if is_backfill {
                Some(Utc::now().to_rfc3339())
            } else {
                None
            }
        })]
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
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Get runs pending retry (batch of 50)
    let pending_runs = hot::db::run::Run::get_pending_retries(db, 50)
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
        // Get the original event to re-send
        let event_id = match run.event_id {
            Some(id) => id,
            None => {
                warn!(
                    "Run {} has no event_id, cannot retry - marking as failed",
                    run.run_id
                );
                if let Err(e) = hot::db::run::Run::mark_retry_as_failed(db, &run.run_id).await {
                    error!("Failed to mark run {} as failed: {}", run.run_id, e);
                }
                continue;
            }
        };

        // Get the original event
        let original_event = match hot::db::Event::get_event(db, &event_id).await {
            Ok(evt) => evt,
            Err(e) => {
                warn!(
                    "Failed to get event {} for retry, marking run {} as failed: {}",
                    event_id, run.run_id, e
                );
                if let Err(e) = hot::db::run::Run::mark_retry_as_failed(db, &run.run_id).await {
                    error!("Failed to mark run {} as failed: {}", run.run_id, e);
                }
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
            org_id: None,
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

                // Mark the original run as failed now that we've queued a retry
                // The retry is a separate run linked via origin_run_id
                if let Err(e) = hot::db::run::Run::mark_retry_as_failed(db, &run.run_id).await {
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
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let now = Utc::now();

    // Get all active @at: schedules that are due (run_at <= now)
    let due_schedules = Schedule::get_due_at_schedules(db, now).await.map_err(|e| {
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
}

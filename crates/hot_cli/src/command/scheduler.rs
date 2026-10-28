//! `hot scheduler` — periodic-job scheduler daemon.

use std::str::FromStr;

use hot::data::serialization::Serialization;
use hot::queue::QueueType;
use hot::val::Val;
use tracing::{debug, error, info};

use crate::Env;
use crate::build_info;

pub(crate) async fn run_scheduler(env: Env, conf: Val) {
    info!(
        "hot.dev: SCHEDULER starting, version: {} ({})",
        build_info::VERSION,
        build_info::git_sha_short()
    );

    // Extract configuration values from conf
    let queue_type = QueueType::from_str(&conf.get_str("queue.type")).unwrap_or(QueueType::Memory);

    let redis_uri_str = conf.get_str("redis.uri");
    let redis_uri = if redis_uri_str == "null" || redis_uri_str.is_empty() {
        None
    } else {
        Some(redis_uri_str)
    };

    // Check for cluster mode configuration
    let redis_cluster = conf.get_bool_or_default("redis.cluster", false);

    let serialization =
        Serialization::from_str(&conf.get_str("serialization.type")).unwrap_or_default();

    // Create database connection for schedule sync
    debug!("hot.dev: SCHEDULER verifying database connectivity");
    let db = match hot::db::create_db_pool(&conf).await {
        Ok(pool) => {
            // Test the database connection
            match hot::db::test_connection(&pool).await {
                Ok(_) => {
                    debug!("hot.dev: SCHEDULER successfully connected to database");
                    Some(pool)
                }
                Err(e) => {
                    error!(
                        "hot.dev: SCHEDULER failed to verify database connection: {}",
                        e
                    );
                    return;
                }
            }
        }
        Err(e) => {
            error!("hot.dev: SCHEDULER failed to create database pool: {}", e);
            return;
        }
    };

    let selected_env = if matches!(env, Env::Development) {
        match crate::profile::resolve_cli_identity(db.as_ref().expect("connected database"), &conf)
            .await
        {
            Ok((_, env_id, _)) => Some(env_id),
            Err(error) => {
                error!(%error, "Cannot resolve development scheduler profile");
                return;
            }
        }
    } else {
        None
    };

    let scheduler_conf = conf.get("scheduler");

    // Extract scheduler intervals from configuration.
    let sync_interval_seconds = scheduler_conf.as_ref().map(|scheduler| {
        scheduler
            .get_int_or_default("sync-interval-seconds", 30)
            .max(1) as u64
    });
    let retry_interval_seconds = scheduler_conf.as_ref().map(|scheduler| {
        scheduler
            .get_int_or_default("retry-interval-seconds", 1)
            .max(1) as u64
    });
    let at_interval_seconds = scheduler_conf.as_ref().map(|scheduler| {
        scheduler
            .get_int_or_default("at-interval-seconds", 1)
            .max(1) as u64
    });
    let singleton = scheduler_conf
        .as_ref()
        .map(|scheduler| scheduler.get_bool_or_default("singleton", true))
        .unwrap_or(true);

    // Extract backfill setting from configuration (default to false)
    let backfill_enabled = if let Some(scheduler) = scheduler_conf {
        scheduler.get_bool_or_default("backfill", false)
    } else {
        false
    };

    let schedule_policy = hot::db::SchedulePolicy::from_conf(&conf);

    // A standalone scheduler that stops on an error, such as losing its
    // singleton lock, exits non-zero so its supervisor restarts it.
    let exit_on_error = matches!(env, Env::Production);
    let server = tokio::spawn(async move {
        match hot_scheduler::server::run_for_environment(
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
            selected_env,
        )
        .await
        {
            Ok(_) => info!("hot.dev: SCHEDULER shut down"),
            Err(e) => {
                error!("hot.dev: Scheduler error: {}", e);
                if exit_on_error {
                    std::process::exit(1);
                }
            }
        }
    });

    // Wait for the server task to complete
    // The server has its own Ctrl-C handler that triggers graceful shutdown
    let _ = server.await;
}

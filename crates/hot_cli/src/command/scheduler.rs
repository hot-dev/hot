//! `hot scheduler` — periodic-job scheduler daemon.

use std::str::FromStr;

use hot::data::serialization::Serialization;
use hot::queue::QueueType;
use hot::val::Val;
use tracing::{debug, error, info};

use crate::Env;
use crate::build_info;

pub(crate) async fn run_scheduler(_env: Env, conf: Val) {
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

    // Extract sync interval from configuration (default to 30 seconds)
    let sync_interval_seconds = if conf.get("scheduler").is_some() {
        Some(
            conf.get("scheduler")
                .unwrap()
                .get_int("sync-interval-seconds") as u64,
        )
    } else {
        None
    };

    // Extract backfill setting from configuration (default to false)
    let backfill_enabled = if conf.get("scheduler").is_some() {
        conf.get("scheduler")
            .unwrap()
            .get_bool_or_default("backfill", false)
    } else {
        false
    };

    let schedule_policy = hot::db::SchedulePolicy::from_conf(&conf);

    let server = tokio::spawn(async move {
        match hot_scheduler::server::run(
            queue_type,
            redis_uri,
            redis_cluster,
            serialization,
            db,
            sync_interval_seconds,
            backfill_enabled,
            schedule_policy,
        )
        .await
        {
            Ok(_) => info!("hot.dev: SCHEDULER shut down"),
            Err(e) => error!("hot.dev: Scheduler error: {}", e),
        }
    });

    // Wait for the server task to complete
    // The server has its own Ctrl-C handler that triggers graceful shutdown
    let _ = server.await;
}

//! `hot worker` and `hot task-worker` — background event/task workers.

use std::str::FromStr;

use hot::data::serialization::Serialization;
use hot::queue::QueueType;
use hot::stream::StreamPubSub;
use hot::val::Val;
use tracing::{error, info};

use crate::Env;
use crate::build_info;
use crate::conf::{create_emitter, create_event_publisher};

pub(crate) async fn run_task_worker(conf: Val) {
    info!(
        "hot.dev: TASK_WORKER starting, version: {} ({})",
        build_info::VERSION,
        build_info::git_sha_short()
    );

    let queue_type = QueueType::from_str(&conf.get_str("queue.type")).unwrap_or(QueueType::Memory);
    let redis_uri_str = conf.get_str("redis.uri");
    let redis_uri = if redis_uri_str == "null" || redis_uri_str.is_empty() {
        None
    } else {
        Some(redis_uri_str)
    };
    let redis_cluster = conf.get_bool_or_default("redis.cluster", false);
    let serialization =
        Serialization::from_str(&conf.get_str("serialization.type")).unwrap_or_default();

    let max_concurrent = conf
        .get("task")
        .and_then(|t| t.get("max-concurrent"))
        .and_then(|v| match v {
            Val::Int(i) => Some(i as usize),
            Val::Str(s) => s.parse::<usize>().ok(),
            _ => None,
        })
        .unwrap_or(4)
        .max(1);

    let box_conf = conf.get("box");

    let container_backend = {
        let raw = box_conf
            .as_ref()
            .and_then(|b| b.get("backend"))
            .map(|v| match v {
                Val::Str(s) => s.to_string(),
                other => other.to_string().trim_matches('"').to_string(),
            })
            .unwrap_or_default();
        match raw.parse::<hot_task_worker::Backend>() {
            Ok(b) => b,
            Err(e) => {
                tracing::error!(backend = %raw, "Invalid HOT_BOX_BACKEND, falling back to docker: {}", e);
                hot_task_worker::Backend::default()
            }
        }
    };

    if container_backend == hot_task_worker::Backend::Docker {
        hot_task_worker::ensure_hotbox_binary();
    }

    let containerd_socket = box_conf
        .as_ref()
        .and_then(|b| b.get("containerd"))
        .and_then(|c| c.get("socket"))
        .map(|v| match v {
            Val::Str(s) => s.to_string(),
            other => other.to_string().trim_matches('"').to_string(),
        })
        .filter(|s| !s.is_empty());

    let kata_vmm = box_conf
        .as_ref()
        .and_then(|b| b.get("vmm"))
        .map(|v| match v {
            Val::Str(s) => s.to_string(),
            other => other.to_string().trim_matches('"').to_string(),
        })
        .filter(|s| !s.is_empty());

    let task_conf = conf.get("task");

    let get_task_int = |key: &str, default: i64| -> i64 {
        task_conf
            .as_ref()
            .and_then(|t| t.get(key))
            .and_then(|v| match v {
                Val::Int(i) => Some(i),
                Val::Str(s) => s.parse::<i64>().ok(),
                _ => None,
            })
            .unwrap_or(default)
    };

    let code_max_concurrent = (get_task_int("code-max-concurrent", 500) as usize).max(1);
    let worker_memory_mb = (get_task_int("worker-memory-mb", 8192) as u64).max(256);
    let worker_disk_mb = (get_task_int("worker-disk-mb", 51200) as u64).max(1024);

    let data_volume_base_dir = task_conf
        .as_ref()
        .and_then(|t| t.get("data-volume-dir"))
        .map(|v| match v {
            Val::Str(s) => s.to_string(),
            other => other.to_string().trim_matches('"').to_string(),
        })
        .filter(|s| !s.is_empty() && s != "null");

    let opt_u64 = |key: &str| -> Option<u64> {
        let v = get_task_int(key, 0);
        if v > 0 { Some(v as u64) } else { None }
    };

    let queue_name = {
        let raw = conf.get_str_or_default("queue.name", "");
        if raw.is_empty() || raw == "null" {
            None
        } else {
            Some(raw)
        }
    };

    let config = hot_task_worker::TaskWorkerConfig {
        queue_type,
        redis_uri,
        redis_cluster,
        serialization,
        max_concurrent,
        container_backend,
        containerd_socket,
        kata_vmm,
        worker_conf: conf,
        code_max_concurrent,
        worker_memory_mb,
        worker_disk_mb,
        data_volume_base_dir,
        box_default_memory_mb: opt_u64("box-default-memory-mb"),
        box_default_disk_mb: opt_u64("box-default-disk-mb"),
        box_default_tmp_mb: opt_u64("box-default-tmp-mb"),
        box_default_timeout_secs: opt_u64("box-default-timeout-secs"),
        box_default_cpu_quota: opt_u64("box-default-cpu-quota"),
        queue_name,
    };

    match hot_task_worker::run(config).await {
        Ok(_) => info!("hot.dev: TASK_WORKER shut down"),
        Err(e) => error!("hot.dev: Task worker error: {}", e),
    }
}

pub(crate) async fn run_worker(
    env: Env,
    conf: Val,
    context_storage: Option<ahash::AHashMap<String, hot::val::Val>>,
) {
    run_worker_with_stream_pubsub(env, conf, context_storage, None).await
}

pub(crate) async fn run_worker_with_stream_pubsub(
    env: Env,
    conf: Val,
    context_storage: Option<ahash::AHashMap<String, hot::val::Val>>,
    stream_pubsub: Option<std::sync::Arc<StreamPubSub>>,
) {
    let dev_context_storage = context_storage.map(|ctx| {
        std::sync::Arc::new(std::sync::RwLock::new(Some(ctx)))
            as hot_worker::server::DevContextStorage
    });

    run_worker_with_stream_pubsub_shared_context(env, conf, dev_context_storage, stream_pubsub)
        .await
}

pub(crate) async fn run_worker_with_stream_pubsub_shared_context(
    env: Env,
    conf: Val,
    dev_context_storage: Option<hot_worker::server::DevContextStorage>,
    stream_pubsub: Option<std::sync::Arc<StreamPubSub>>,
) {
    info!(
        "hot.dev: WORKER starting, version: {} ({})",
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

    let worker_conf = conf.get("worker").unwrap();
    let threads = Some(worker_conf.get_int("threads") as usize);

    // Create database pool FIRST - needed by emitter and event publisher
    let db_pool = match hot::db::create_db_pool(&conf).await {
        Ok(pool) => Some(std::sync::Arc::new(pool)),
        Err(e) => {
            error!("Failed to create database pool for worker: {}", e);
            None
        }
    };

    // Create emitter and event publisher for the worker
    // These now require the database pool to be available
    let emitter = if let Some(ref pool) = db_pool {
        match create_emitter(&conf, pool.as_ref()) {
            Ok(emitter) => emitter,
            Err(e) => {
                error!("Failed to create emitter for worker: {}", e);
                None
            }
        }
    } else {
        error!("Cannot create emitter without database connection");
        None
    };

    let event_publisher = if let Some(ref pool) = db_pool {
        match create_event_publisher(&conf, pool.as_ref()) {
            Ok(publisher) => publisher,
            Err(e) => {
                error!("Failed to create event publisher for worker: {}", e);
                None
            }
        }
    } else {
        error!("Cannot create event publisher without database connection");
        None
    };

    // Pass the full configuration to the worker for event handler processing
    // Add profile information for encryption key handling
    let mut full_worker_conf = conf.clone();
    let profile = match env {
        Env::Development => "local-dev".to_string(),
        Env::Production => "production".to_string(),
    };
    full_worker_conf = full_worker_conf.set_str("runtime.profile", Some(profile.clone()), &profile);

    let server = tokio::spawn(async move {
        match hot_worker::server::run_with_components_shared_context(
            queue_type,
            redis_uri,
            redis_cluster,
            serialization,
            threads,
            full_worker_conf,
            emitter,
            event_publisher,
            dev_context_storage,
            stream_pubsub, // Pass shared stream publisher
        )
        .await
        {
            Ok(_) => info!("hot.dev: WORKER shut down"),
            Err(e) => error!("hot.dev: WORKER error: {}", e),
        }
    });

    // Wait for the server task to complete
    // The server has its own Ctrl-C handler that triggers graceful shutdown
    let _ = server.await;
}

// Function to evaluate a Hot code string directly

// Function to run a single Hot source file

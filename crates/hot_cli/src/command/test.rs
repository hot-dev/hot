//! `hot test` — run unit tests, with optional integration test support.

use std::str::FromStr;

use hot::data::serialization::Serialization;
use hot::queue::QueueType;
use hot::stream::{StreamPubSub, StreamPubSubType};
use hot::val::Val;
use tracing::{info, warn};

use crate::Env;
use crate::cli::GlobalOptions;
use crate::command::api::{run_api_with_stream_pubsub, run_app};
use crate::command::deploy::setup_live_build_for_dev;
use crate::command::scheduler::run_scheduler;
use crate::command::worker::{run_task_worker, run_worker_with_stream_pubsub};
use crate::conf::{get_merged_src_paths, get_merged_test_paths, load_conf};

pub(crate) async fn run_test(
    pattern: Option<&str>,
    capture_output: bool,
    conf: &Val,
    global_options: &GlobalOptions,
    context_storage: Option<ahash::AHashMap<String, hot::val::Val>>,
    integration: bool,
    providers: &crate::CliProviders,
) -> Result<i32, String> {
    // Get paths using the merged path functions
    let src_paths = get_merged_src_paths(
        conf,
        global_options.project.as_deref(),
        &global_options.src_paths,
    );
    let test_paths = get_merged_test_paths(
        conf,
        global_options.project.as_deref(),
        &global_options.test_paths,
    );

    // Get project name for dependency resolution
    let project_name = global_options
        .project
        .clone()
        .unwrap_or_else(|| hot::project::get_default_project_name(conf));

    if integration {
        return run_integration_test(
            pattern,
            capture_output,
            conf,
            global_options,
            context_storage,
            &src_paths,
            &test_paths,
            &project_name,
            providers,
        )
        .await;
    }

    // Create or update live build for development (consistent with run/eval/repl/check)
    setup_live_build_for_dev(conf, global_options, &src_paths, &test_paths).await?;

    // Use the enhanced pipeline with dependency loading and context storage
    match hot::lang::engine::Engine::run_tests_with_direct_discovery(
        pattern,
        capture_output,
        &src_paths,
        &test_paths,
        Some(conf),
        Some(&project_name),
        context_storage,
        None, // No task queue
        None, // No stream publisher
        hot::env::is_local_dev(),
    )
    .await
    {
        Ok(success) => {
            if success {
                Ok(0) // Exit code 0 for success
            } else {
                Ok(1) // Exit code 1 for failure
            }
        }
        Err(e) => {
            eprintln!("Test execution error: {}", e);
            Ok(1)
        }
    }
}

/// Discover the integration.hot config file for a project.
/// Looks at the project's src.paths to derive the package directory, then checks
/// for integration.hot in that directory.
fn discover_integration_config(conf: &Val, project_name: &str) -> Result<String, String> {
    let src_paths = get_merged_src_paths(conf, Some(project_name), &[]);
    for src_path in &src_paths {
        let path = std::path::Path::new(src_path);
        // src.paths are typically like "./hot/pkg/ffmpeg/src" — go up one level to the pkg dir
        if let Some(parent) = path.parent() {
            let integration_conf = parent.join("integration.hot");
            if integration_conf.exists() {
                return Ok(integration_conf.to_string_lossy().to_string());
            }
        }
    }
    Err(format!(
        "No integration.hot found for project '{}'. \
         Expected at the package root (e.g., hot/pkg/<name>/integration.hot). \
         Searched src paths: {:?}",
        project_name, src_paths
    ))
}

/// Parse the `hot.test.integration.services` config key into a list of service names.
fn parse_integration_services(integration_conf: &Val) -> Vec<String> {
    integration_conf
        .get("test.integration.services")
        .map(|v| match v {
            Val::Vec(items) => items
                .iter()
                .filter_map(|item| match item {
                    Val::Str(s) => Some(s.to_string()),
                    _ => None,
                })
                .collect(),
            _ => vec![],
        })
        .unwrap_or_default()
}

/// Run integration tests by booting a mini dev stack, running tests, and tearing down.
#[allow(clippy::too_many_arguments)]
async fn run_integration_test(
    pattern: Option<&str>,
    capture_output: bool,
    conf: &Val,
    global_options: &GlobalOptions,
    context_storage: Option<ahash::AHashMap<String, hot::val::Val>>,
    src_paths: &[String],
    test_paths: &[String],
    project_name: &str,
    providers: &crate::CliProviders,
) -> Result<i32, String> {
    use std::sync::Arc;

    info!("hot.test: Integration mode — discovering config and booting services...");

    // 1. Discover and load the integration config
    let integration_conf_path = discover_integration_config(conf, project_name)?;
    info!(
        "hot.test: Loading integration config from {}",
        integration_conf_path
    );

    let integration_conf_val = load_conf(std::slice::from_ref(&integration_conf_path), src_paths)?;
    // Merge: base conf (hot.hot) + integration.hot overrides
    let merged_conf = conf.merge(&integration_conf_val);

    // 2. Check Docker availability if task-worker or box is needed
    let services = parse_integration_services(&merged_conf);
    let needs_docker = services.contains(&"task-worker".to_string())
        || merged_conf.get_bool_or_default("box.enabled", false);

    if needs_docker {
        info!("hot.test: Checking Docker availability...");
        match std::process::Command::new("docker")
            .arg("info")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
        {
            Ok(status) if status.success() => {
                info!("hot.test: Docker is available");
            }
            _ => {
                return Err(
                    "Integration tests require Docker. Please start Docker and try again."
                        .to_string(),
                );
            }
        }
    }

    // 3. Run DB migrations on the configured database
    let db_uri = merged_conf.get_str("db.uri");
    if !db_uri.is_empty() && db_uri != "null" {
        info!("hot.test: Running database migrations...");
        if let Err(e) = crate::run_migrations_with_bootstrap(&merged_conf, providers).await {
            crate::report_migration_failure("Failed to run integration test DB migrations", &e);
            return Err("Failed to run integration test DB migrations".to_string());
        }
    }

    // 4. Setup live build (needed for task worker to compile and execute code)
    info!("hot.test: Setting up live build...");
    setup_live_build_for_dev(&merged_conf, global_options, src_paths, test_paths).await?;

    // 5. Generate a unique queue name per test run to avoid stale shared-memory data
    let id = uuid::Uuid::now_v7().to_string().replace('-', "");
    let unique_queue_name = format!("hot:task:test-{}", &id[id.len() - 12..]);
    info!("hot.test: Using queue name: {}", unique_queue_name);
    let merged_conf = merged_conf.set("queue.name", Val::Str(unique_queue_name.clone().into()));

    // 6. Create shared in-memory queue infrastructure
    let queue_type_str = merged_conf.get_str_or_default("queue.type", "memory");
    let shared_stream_pubsub: Option<Arc<StreamPubSub>> = if queue_type_str == "memory" {
        match StreamPubSub::new(StreamPubSubType::Memory, None, false) {
            Ok(pubsub) => Some(Arc::new(pubsub)),
            Err(e) => {
                warn!("hot.test: Failed to create shared stream pub/sub: {}", e);
                None
            }
        }
    } else {
        None
    };

    // 7. Spawn requested services
    let mut service_handles: Vec<tokio::task::JoinHandle<()>> = Vec::new();

    // Create the task queue that will be shared between the task worker and the test VM
    let task_queue: Option<Arc<hot::queue::ProcessingQueue<hot::lang::hot::task::TaskRequest>>> =
        if services.contains(&"task-worker".to_string()) {
            let queue_type =
                QueueType::from_str(&merged_conf.get_str_or_default("queue.type", "memory"))
                    .unwrap_or(QueueType::Memory);
            let redis_uri_str = merged_conf.get_str("redis.uri");
            let redis_uri = if redis_uri_str.is_empty() || redis_uri_str == "null" {
                None
            } else {
                Some(redis_uri_str)
            };
            let redis_cluster = merged_conf.get_bool_or_default("redis.cluster", false);
            let serialization = Serialization::from_str(
                &merged_conf.get_str_or_default("serialization.type", "json"),
            )
            .unwrap_or_default();

            match hot::queue::ProcessingQueue::new_with_cluster(
                queue_type,
                unique_queue_name.clone(),
                redis_uri,
                redis_cluster,
                serialization,
            ) {
                Ok(q) => Some(Arc::new(q)),
                Err(e) => {
                    return Err(format!("Failed to create task queue: {}", e));
                }
            }
        } else {
            None
        };

    if services.contains(&"task-worker".to_string()) {
        info!("hot.test: Starting task worker...");
        let tw_conf = merged_conf.clone();
        service_handles.push(tokio::spawn(async move {
            run_task_worker(tw_conf).await;
        }));
    }

    if services.contains(&"api".to_string()) {
        info!("hot.test: Starting API server...");
        let api_conf = merged_conf.clone();
        let api_stream = shared_stream_pubsub.clone();
        service_handles.push(tokio::spawn(async move {
            run_api_with_stream_pubsub(Env::Development, api_conf, api_stream).await;
        }));
    }

    if services.contains(&"worker".to_string()) {
        info!("hot.test: Starting event worker...");
        let worker_conf = merged_conf.clone();
        let worker_stream = shared_stream_pubsub.clone();
        service_handles.push(tokio::spawn(async move {
            run_worker_with_stream_pubsub(
                Env::Development,
                worker_conf,
                None, // No context storage for worker
                worker_stream,
            )
            .await;
        }));
    }

    if services.contains(&"scheduler".to_string()) {
        info!("hot.test: Starting scheduler...");
        let sched_conf = merged_conf.clone();
        service_handles.push(tokio::spawn(async move {
            run_scheduler(Env::Development, sched_conf).await;
        }));
    }

    if services.contains(&"app".to_string()) {
        info!("hot.test: Starting app server...");
        let app_conf = merged_conf.clone();
        let app_stream = shared_stream_pubsub.clone();
        service_handles.push(tokio::spawn(async move {
            run_app(Env::Development, app_conf, app_stream).await;
        }));
    }

    // 9. Wait for services to become ready
    if !service_handles.is_empty() {
        info!(
            "hot.test: Waiting for {} service(s) to start...",
            service_handles.len()
        );
        tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
    }

    // 10. Run tests with the wired-up infrastructure
    info!("hot.test: Running integration tests...");
    let test_result = hot::lang::engine::Engine::run_tests_with_direct_discovery(
        pattern,
        capture_output,
        src_paths,
        test_paths,
        Some(&merged_conf),
        Some(project_name),
        context_storage,
        task_queue,
        shared_stream_pubsub,
        hot::env::is_local_dev(),
    )
    .await;

    // 11. Tear down all spawned services
    info!("hot.test: Shutting down integration services...");
    for handle in &service_handles {
        handle.abort();
    }
    for handle in service_handles {
        let _ = handle.await;
    }

    // 12. Clean up ephemeral resources
    let file_storage_path = merged_conf.get_str("file.storage.path");
    if !file_storage_path.is_empty()
        && file_storage_path != "null"
        && file_storage_path.contains("test-integration")
    {
        info!(
            "hot.test: Cleaning up test file storage: {}",
            file_storage_path
        );
        let _ = std::fs::remove_dir_all(&file_storage_path);
    }

    let db_path = merged_conf.get_str("db.uri");
    if db_path.starts_with("sqlite:") && db_path.contains("test-integration") {
        let sqlite_path = db_path.trim_start_matches("sqlite:");
        info!("hot.test: Cleaning up test database: {}", sqlite_path);
        let _ = std::fs::remove_file(sqlite_path);
        let _ = std::fs::remove_file(format!("{}-shm", sqlite_path));
        let _ = std::fs::remove_file(format!("{}-wal", sqlite_path));
    }

    info!("hot.test: Integration test run complete.");

    match test_result {
        Ok(success) => {
            if success {
                Ok(0)
            } else {
                Ok(1)
            }
        }
        Err(e) => {
            eprintln!("Integration test execution error: {}", e);
            Ok(1)
        }
    }
}

//! `hot test` — run unit tests, with optional integration test support.

use std::str::FromStr;

use hot::data::serialization::Serialization;
use hot::queue::QueueType;
use hot::stream::{StreamPubSub, StreamPubSubType};
use hot::val::Val;
use tracing::{info, warn};

use crate::Env;
use crate::cli::GlobalOptions;
use crate::command::api::run_api_with_stream_pubsub;
use crate::command::app::run_app;
use crate::command::deploy::setup_live_build_for_dev;
use crate::command::scheduler::run_scheduler;
use crate::command::worker::{run_task_worker, run_worker_with_stream_pubsub};
use crate::conf::{get_merged_src_paths, get_merged_test_paths, load_conf};

fn isolate_integration_queue(conf: Val) -> Val {
    conf.set_str("queue.type", Some("memory".to_string()), "")
}

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

    // Ordinary unit tests may explicitly disable the configured database and
    // use only the engine's private ephemeral fixture.
    if !hot::db::get_db_uri_from_conf(conf).is_empty() {
        // A fresh checkout's local SQLite database is created on first use.
        crate::command::init::initialize_missing_local_database(conf, providers).await?;
        setup_live_build_for_dev(conf, global_options, &src_paths, &test_paths).await?;
    }

    // Use the enhanced pipeline with dependency loading and context storage
    match hot::lang::engine::Engine::run_tests_with_direct_discovery(
        pattern,
        capture_output,
        &src_paths,
        &test_paths,
        Some(conf),
        Some(&project_name),
        context_storage,
        None, // Private ephemeral database for ordinary unit tests
        None, // No task queue
        None, // No stream publisher
        hot::env::is_local_dev(),
    )
    .await
    {
        Ok(hot::lang::engine::TestRunStatus::Passed) => Ok(0),
        Ok(hot::lang::engine::TestRunStatus::Failed) => Ok(1),
        Ok(hot::lang::engine::TestRunStatus::NoTestsDiscovered) => {
            Ok(empty_run_exit_code(conf, &project_name))
        }
        Err(e) => {
            eprintln!("Test execution error: {}", e);
            Ok(1)
        }
    }
}

/// Exit code for a run that discovered zero tests.
///
/// Nonzero by default: a run that exercised nothing proved nothing, and
/// treating it as success launders every discovery failure into a green
/// build. The code is 3 — the next free code, keeping the sequence dense:
/// 1 is "tests failed", 2 belongs to clap's usage errors. Distinct from 1
/// so scripts can tell "fix the code" from "fix the config or cache".
/// (pytest draws the same distinction with its exit code 5.)
///
/// Escape hatch for genuinely testless projects, mirroring test.capture
/// precedence: `hot.test.allow-empty` then `hot.project.<name>.test.allow-empty`.
pub(crate) const NO_TESTS_EXIT_CODE: i32 = 3;

fn empty_run_exit_code(conf: &Val, project_name: &str) -> i32 {
    let allow_empty = match conf.get("test.allow-empty") {
        Some(Val::Bool(b)) => b,
        _ => hot::project::get_project_conf(conf, project_name)
            .and_then(|project_conf| project_conf.get("test"))
            .and_then(|test_conf| test_conf.get("allow-empty"))
            .map(|v| matches!(v, Val::Bool(true)))
            .unwrap_or(false),
    };
    if allow_empty {
        eprintln!("hot.test: zero tests discovered; passing because test.allow-empty is set");
        0
    } else {
        eprintln!(
            "hot.test: zero tests discovered — exiting {} (set test.allow-empty to permit empty runs)",
            NO_TESTS_EXIT_CODE
        );
        NO_TESTS_EXIT_CODE
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

/// Resolve the selected profile once and carry its exact live build into the VM.
async fn prepare_integration_execution(
    conf: &Val,
    project_name: &str,
) -> Result<
    (
        std::sync::Arc<hot::db::DatabasePool>,
        hot::lang::event::ExecutionContext,
    ),
    String,
> {
    let db = std::sync::Arc::new(crate::profile::create_cli_db_pool(conf).await?);
    let (user_id, env, org) = crate::profile::resolve_cli_environment(&db, conf).await?;
    let project = hot::db::Project::get_project_by_env_and_name(&db, &env.env_id, project_name)
        .await
        .map_err(|error| error.to_string())?
        .ok_or("Integration project is missing from the selected environment")?;
    let build = hot::db::Build::get_live_build_by_project(&db, &project.project_id)
        .await
        .map_err(|error| error.to_string())?
        .ok_or("Integration project has no live build in the selected environment")?;
    let ctx = hot::lang::event::ExecutionContext::new(
        uuid::Uuid::now_v7(),
        uuid::Uuid::now_v7(),
        hot::db::run::RunType::Run.as_id(),
        Some(env.env_id),
        Some(user_id),
        Some(org.org_id),
        Some(build.build_id),
    )
    .with_env_name(Some(env.name))
    .with_org_slug(Some(org.slug));
    Ok((db, ctx))
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
    // Integration services all run in this process and share their queue
    // handles directly. Force memory isolation so `hot test` can never race a
    // sibling `hot dev` through the project-local durable event queue.
    let merged_conf = isolate_integration_queue(conf.merge(&integration_conf_val));

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
    let integration_execution = prepare_integration_execution(&merged_conf, project_name).await?;

    // 5. Generate a unique queue name per test run to avoid stale shared-memory data
    let id = uuid::Uuid::now_v7().to_string().replace('-', "");
    let unique_queue_name = format!("hot:task:test-{}", &id[id.len() - 12..]);
    info!("hot.test: Using queue name: {}", unique_queue_name);
    let merged_conf = merged_conf.set("queue.name", Val::Str(unique_queue_name.clone().into()));

    // 6. Create shared in-process pub/sub infrastructure. SQLite persists
    // queues, while live test-stream notifications remain process-local.
    let queue_type = QueueType::Memory;
    let shared_stream_pubsub: Option<Arc<StreamPubSub>> =
        if matches!(queue_type, QueueType::Memory | QueueType::Sqlite) {
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
        Some(integration_execution),
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

    // The configured database and file backend may be shared with other
    // environments. Only the engine's privately created unit-test artifacts
    // are automatically removed; a path substring is not proof of ownership.

    info!("hot.test: Integration test run complete.");

    match test_result {
        Ok(hot::lang::engine::TestRunStatus::Passed) => Ok(0),
        Ok(hot::lang::engine::TestRunStatus::Failed) => Ok(1),
        Ok(hot::lang::engine::TestRunStatus::NoTestsDiscovered) => {
            Ok(empty_run_exit_code(&merged_conf, project_name))
        }
        Err(e) => {
            eprintln!("Integration test execution error: {}", e);
            Ok(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn integration_runner_uses_selected_child_without_task_worker_or_shared_fixtures() {
        use hot::db::org::CreateChild;
        use hot::db::{Build, Org, Project};
        use hot::store::Store;
        use std::sync::Arc;

        let dir = tempfile::tempdir().unwrap();
        let database_path = dir.path().join("managed.db");
        let conf = hot::val!({"db":{"uri":format!("sqlite:{}", database_path.display())}});
        hot::db::run_migrations(&conf).await.unwrap();
        let db = hot::db::create_db_pool(&conf).await.unwrap();
        let fixture = hot::db::insert_test_data(&db).await.unwrap();
        let user = hot::db::User::get_user(&db, &fixture.user_id)
            .await
            .unwrap();
        let root_project = Project::get_project(&db, &fixture.project_id)
            .await
            .unwrap();
        let account_id = uuid::Uuid::new_v4();
        hot::db::Org::insert_org(
            &db,
            &account_id,
            "Test account",
            "integration-account",
            "organization",
            &fixture.user_id,
        )
        .await
        .unwrap();
        hot::db::OrgUser::insert_org_user(
            &db,
            &uuid::Uuid::new_v4(),
            &account_id,
            &fixture.user_id,
            Some(2),
            &fixture.user_id,
        )
        .await
        .unwrap();
        let child = Org::create_child(
            &db,
            &account_id,
            &fixture.user_id,
            &CreateChild {
                name: "Integration child".into(),
                slug: "integration-child".into(),
                external_id: None,
                idempotency_key: "integration-child".into(),
                hosted: false,
            },
        )
        .await
        .unwrap();
        let child_project = Project::insert_or_get_project(
            &db,
            &uuid::Uuid::new_v4(),
            &child.env_id,
            &root_project.name,
            &fixture.user_id,
        )
        .await
        .unwrap();
        let live = Build::insert_or_update_live_build(
            &db,
            &child_project.project_id,
            "selected-child-live",
            0,
            &fixture.user_id,
        )
        .await
        .unwrap();
        let conf = conf.merge(&hot::val!({
            "set":{"profile":"child"},
            "profile":{"child":{
                "user":{"email":user.email}, "org":{"slug":child.org.slug},
                "env":{"name":"development"}
            }},
            "file":{"storage":{"path":dir.path().join("files").to_string_lossy().to_string()}}
        }));
        let root_store = hot::store::sqlite::SqliteStore::new(
            Arc::new(db.clone()),
            fixture.org_id,
            fixture.env_id,
        );
        root_store
            .put(
                "scope-regression",
                serde_json::json!("key"),
                serde_json::json!("root"),
                None,
                None,
            )
            .await
            .unwrap();
        let (selected_db, ctx) = prepare_integration_execution(&conf, &root_project.name)
            .await
            .unwrap();
        assert_eq!(ctx.env_id, Some(child.env_id));
        assert_eq!(ctx.org_id, Some(child.org.org_id));
        assert_eq!(ctx.build_id, Some(live.build_id));
        let run_id = ctx.run_id;
        let source = dir.path().join("scope.hot");
        std::fs::write(
            &source,
            r#"
            ::integration_scope ns
            selected ::hot::store/Map({name: "scope-regression"})
            selected-scope meta ["test"] fn () {
                ::hot::store/put(selected, "key", "selected")
                assert(eq(::hot::store/get(selected, "key"), "selected"), "selected store value")
            }
        "#,
        )
        .unwrap();
        let paths = vec![dir.path().to_string_lossy().to_string()];
        let result = hot::lang::engine::Engine::run_tests_with_direct_discovery(
            None,
            true,
            &paths,
            &[],
            Some(&conf),
            Some(&root_project.name),
            None,
            Some((selected_db.clone(), ctx.clone())),
            None,
            None,
            false,
        )
        .await
        .unwrap();
        assert!(matches!(result, hot::lang::engine::TestRunStatus::Passed));
        let child_store = hot::store::sqlite::SqliteStore::new(
            selected_db.clone(),
            child.org.org_id,
            child.env_id,
        );
        assert_eq!(
            child_store
                .get("scope-regression", &serde_json::json!("key"))
                .await
                .unwrap()
                .map(|entry| entry.value),
            Some(serde_json::json!("selected"))
        );
        assert_eq!(
            root_store
                .get("scope-regression", &serde_json::json!("key"))
                .await
                .unwrap()
                .map(|entry| entry.value),
            Some(serde_json::json!("root"))
        );
        let persisted = hot::db::Run::get_run(&db, &run_id).await.unwrap();
        assert_eq!(persisted.env_id, child.env_id);
        assert_eq!(
            persisted.status_id,
            hot::db::run::RunStatus::Succeeded.as_id()
        );
        assert_eq!(
            Org::get_all_orgs(&db, Some(100), None).await.unwrap().len(),
            3,
            "integration execution must not seed another organization"
        );
        assert!(
            database_path.exists(),
            "the configured database remains available"
        );

        let mut wrong_build = ctx;
        wrong_build.run_id = uuid::Uuid::now_v7();
        wrong_build.stream_id = uuid::Uuid::now_v7();
        wrong_build.build_id = Some(fixture.build_id);
        let error = hot::lang::engine::Engine::run_tests_with_direct_discovery(
            None,
            true,
            &paths,
            &[],
            Some(&conf),
            Some(&root_project.name),
            None,
            Some((selected_db, wrong_build.clone())),
            None,
            None,
            false,
        )
        .await
        .unwrap_err();
        assert!(error.contains("current live build"), "{error}");
        assert!(
            hot::db::Run::get_run(&db, &wrong_build.run_id)
                .await
                .is_err()
        );
    }

    #[test]
    fn empty_test_run_fails_by_default() {
        assert_eq!(empty_run_exit_code(&Val::Null, "demo"), NO_TESTS_EXIT_CODE);
    }

    #[test]
    fn integration_services_always_use_isolated_memory_queue() {
        let conf = isolate_integration_queue(hot::val!({"queue": {"type": "sqlite"}}));
        assert_eq!(conf.get_str("queue.type"), "memory");
    }

    #[test]
    fn global_allow_empty_permits_empty_test_run() {
        let conf = hot::val!({"test": {"allow-empty": true}});
        assert_eq!(empty_run_exit_code(&conf, "demo"), 0);
    }

    #[test]
    fn project_allow_empty_permits_empty_test_run() {
        let conf = hot::val!({
            "project": {
                "demo": {
                    "test": {"allow-empty": true},
                },
            },
        });
        assert_eq!(empty_run_exit_code(&conf, "demo"), 0);
    }

    #[test]
    fn global_allow_empty_overrides_project_setting() {
        let conf = hot::val!({
            "test": {"allow-empty": false},
            "project": {
                "demo": {
                    "test": {"allow-empty": true},
                },
            },
        });
        assert_eq!(empty_run_exit_code(&conf, "demo"), NO_TESTS_EXIT_CODE);
    }
}

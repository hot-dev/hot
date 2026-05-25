//! `hot run` — execute a single .hot file in the engine.

use hot::val::Val;

use crate::cli::GlobalOptions;
use crate::command::deploy::setup_live_build_for_dev;
use crate::conf::{
    create_emitter, create_event_publisher, get_merged_src_paths, get_merged_test_paths,
};

pub(crate) async fn run_run(
    file_path: &str,
    conf: &Val,
    global_options: &GlobalOptions,
    context_storage: Option<ahash::AHashMap<String, hot::val::Val>>,
    value_format: Option<&str>,
) -> Result<(), String> {
    // Check if target file exists
    let path = std::path::Path::new(file_path);
    if !path.exists() {
        return Err(format!("File not found: {}", file_path));
    }

    // Get paths for dependency resolution - include the file's directory as a source path
    let file_dir = path
        .parent()
        .and_then(|p| p.to_str())
        .unwrap_or(".")
        .to_string();

    let mut src_paths = get_merged_src_paths(
        conf,
        global_options.project.as_deref(),
        &global_options.src_paths,
    );

    // Add the file's directory to source paths if not already included
    if !src_paths.contains(&file_dir) {
        src_paths.push(file_dir);
    }

    let _test_paths = get_merged_test_paths(
        conf,
        global_options.project.as_deref(),
        &global_options.test_paths,
    );

    // Get project name for dependency resolution
    let project_name = global_options
        .project
        .clone()
        .unwrap_or_else(|| hot::project::get_default_project_name(conf));

    // Check context requirements before running
    hot::lang::engine::Engine::check_sources_pipeline_with_context(
        &src_paths,
        &_test_paths,
        Some(conf),
        Some(&project_name),
        context_storage.as_ref(),
        hot::env::is_local_dev(),
    )?;

    // Create or update live build for development
    setup_live_build_for_dev(conf, global_options, &src_paths, &_test_paths).await?;

    // Create database pool first if needed for emitter and file storage
    let db_pool = match hot::db::create_db_pool(conf).await {
        Ok(pool) => Some(std::sync::Arc::new(pool)),
        Err(e) => {
            tracing::warn!("Failed to create database pool for run: {}", e);
            None
        }
    };

    // Get default org/env/user IDs for CLI context (needed for file storage)
    let (org_id, org_slug, env_id, env_name, user_id, build_id) = if let Some(ref pool) = db_pool {
        match hot::db::get_default_org_and_user_ids(pool.as_ref()).await {
            Ok((org, user)) => {
                tracing::debug!("CLI: Got default org_id={} user_id={}", org, user);
                // Get org slug
                let slug = match hot::db::org::Org::get_org(pool.as_ref(), &org).await {
                    Ok(o) => Some(o.slug),
                    Err(_) => None,
                };
                // Get default env for the org
                let (env, ename) =
                    match hot::db::env::Env::get_default_env_by_org(pool.as_ref(), &org).await {
                        Ok(e) => {
                            tracing::debug!("CLI: Got default env_id={}", e.env_id);
                            (Some(e.env_id), Some(e.name))
                        }
                        Err(e) => {
                            tracing::warn!("CLI: Failed to get default env for org {}: {}", org, e);
                            (None, None)
                        }
                    };
                // Get the most recent build_id (required for run table)
                let build =
                    match hot::db::build::Build::get_recent_builds(pool.as_ref(), Some(1)).await {
                        Ok(builds) if !builds.is_empty() => {
                            tracing::debug!("CLI: Got build_id={}", builds[0].build_id);
                            Some(builds[0].build_id)
                        }
                        Ok(_) => {
                            tracing::warn!(
                                "CLI: No builds found, run may fail due to database constraints"
                            );
                            None
                        }
                        Err(e) => {
                            tracing::warn!("CLI: Failed to get recent builds: {}", e);
                            None
                        }
                    };
                (Some(org), slug, env, ename, Some(user), build)
            }
            Err(e) => {
                tracing::warn!("Failed to get default org/user for CLI: {}", e);
                (None, None, None, None, None, None)
            }
        }
    } else {
        tracing::debug!("CLI: No db_pool available, skipping org/env/user lookup");
        (None, None, None, None, None, None)
    };

    // Create file storage from config
    let file_storage: Option<std::sync::Arc<dyn hot::file_storage::FileStorage>> =
        match hot::file_storage::file_storage_from_config(conf).await {
            Ok(fs) => Some(std::sync::Arc::from(fs)),
            Err(e) => {
                tracing::warn!("Failed to create file storage for run: {}", e);
                None
            }
        };

    // Create emitter and execution context for event tracking
    let emitter_type_str = conf.get_str("emitter.type");
    let has_emitter_config =
        !emitter_type_str.is_empty() && emitter_type_str != "null" && emitter_type_str != "none";

    let emitter = if has_emitter_config {
        if let Some(ref pool) = db_pool {
            create_emitter(conf, pool.as_ref()).ok().flatten()
        } else {
            None
        }
    } else {
        None
    };

    // Create event publisher for send() support
    let queue_type_str = conf.get_str("queue.type");
    let has_queue_config =
        !queue_type_str.is_empty() && queue_type_str != "null" && queue_type_str != "none";

    let event_publisher = if has_queue_config {
        if let Some(ref pool) = db_pool {
            create_event_publisher(conf, pool.as_ref()).ok().flatten()
        } else {
            None
        }
    } else {
        None
    };

    let execution_context =
        if emitter.is_some() || event_publisher.is_some() || file_storage.is_some() {
            // Create execution context for this run (needed for file storage even without emitter)
            let run_id = uuid::Uuid::now_v7();
            let stream_id = uuid::Uuid::now_v7(); // CLI runs start new streams
            let run_type_id = hot::db::run::RunType::Run.as_id();
            Some(
                hot::lang::event::ExecutionContext::new(
                    run_id,
                    stream_id,
                    run_type_id,
                    env_id,
                    user_id,
                    org_id,
                    build_id, // build_id from recent builds
                )
                .with_env_name(env_name.clone())
                .with_org_slug(org_slug.clone()),
            )
        } else {
            None
        };

    // CRITICAL: Insert the run record into the database BEFORE starting execution
    // This ensures file operations can reference the run_id (foreign key constraint)
    if let (Some(pool), Some(ctx)) = (&db_pool, &execution_context) {
        match (ctx.env_id, ctx.user_id, ctx.build_id) {
            (Some(env), Some(user), Some(build)) => {
                tracing::debug!(
                    "CLI: Inserting run record run_id={} env_id={} user_id={} build_id={}",
                    ctx.run_id,
                    env,
                    user,
                    build
                );
                if let Err(e) = hot::db::run::Run::insert_run(
                    pool.as_ref(),
                    &ctx.run_id,
                    &env,
                    &ctx.stream_id,
                    Some(&build),
                    ctx.run_type_id,
                    None, // origin_run_id
                    &user,
                    None, // start_time (uses now)
                    None, // access_id
                )
                .await
                {
                    tracing::warn!("Failed to insert run record for CLI: {}", e);
                }
            }
            (None, _, _) => {
                tracing::warn!(
                    "CLI: Cannot insert run record - env_id is None (run_id={})",
                    ctx.run_id
                );
            }
            (_, None, _) => {
                tracing::warn!(
                    "CLI: Cannot insert run record - user_id is None (run_id={})",
                    ctx.run_id
                );
            }
            (_, _, None) => {
                tracing::warn!(
                    "CLI: Cannot insert run record - build_id is None (run_id={})",
                    ctx.run_id
                );
            }
        }
    } else {
        tracing::debug!("CLI: No db_pool or execution_context, skipping run record insert");
    }

    // Use the unified pipeline which supports emitter and execution context
    // IMPORTANT: VM execution runs in spawn_blocking to avoid blocking the tokio runtime.
    // This allows hot-std blocking I/O (HTTP, file ops) to work correctly.
    let conf_for_blocking = conf.clone();
    let src_paths_clone = src_paths.clone();
    let test_paths_clone = _test_paths.clone();
    let project_name_clone = project_name.clone();
    let file_path_clone = file_path.to_string();
    let emitter_clone = emitter.clone();
    let execution_context_clone = execution_context.clone();
    let event_publisher_clone = event_publisher.clone();
    let db_pool_clone = db_pool.clone();

    // Create store and embedding provider from config. Scope to the local default
    // org/env we resolved earlier so the local SQLite store mirrors Postgres semantics.
    let store: Option<std::sync::Arc<dyn hot::store::Store>> =
        match hot::store::store_from_config_with_db(conf, db_pool.clone(), org_id, env_id).await {
            Ok(s) => Some(std::sync::Arc::from(s)),
            Err(e) => {
                tracing::debug!("Store not available: {e}");
                None
            }
        };
    let embedding_provider: Option<std::sync::Arc<dyn hot::store::embedding::EmbeddingProvider>> =
        hot::store::embedding::embedding_provider_from_config(conf).map(std::sync::Arc::from);

    let store_clone = store.clone();
    let embedding_clone = embedding_provider.clone();

    // Capture the current Tokio handle so the VM thread (a plain
    // std::thread, NOT a Tokio worker) can re-enter the runtime. Many
    // hot-std helpers (`::hot::store`, `::hot::http`, `::hot::task`,
    // `::hot::box`, `::hot::file`, `::hot::ws`, …) call
    // `Handle::current().block_on(...)`, which panics when there is no
    // runtime in scope. `Handle::enter()` makes the captured runtime the
    // thread-local current runtime for the duration of the guard.
    let tokio_handle = tokio::runtime::Handle::current();

    let (result_tx, result_rx) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .name("hot-vm-run".into())
        .stack_size(64 * 1024 * 1024)
        .spawn(move || {
            let _runtime_guard = tokio_handle.enter();
            let r = hot::lang::engine::Engine::run_unified_pipeline(
                &src_paths_clone,
                &test_paths_clone,
                Some(&conf_for_blocking),
                Some(&project_name_clone),
                Some(&file_path_clone),
                None, // No eval code
                hot::lang::engine::PipelineMode::Execute,
                emitter_clone,
                execution_context_clone,
                event_publisher_clone, // Event publisher for send() support
                context_storage,       // Context storage from ctx.hot
                db_pool_clone,
                file_storage,
                store_clone,
                embedding_clone,
                None, // No task queue
                None, // No stream publisher
                hot::env::is_local_dev(),
            );
            let _ = result_tx.send(r);
        })
        .map_err(|e| format!("Failed to spawn VM thread: {}", e))?;
    let result = result_rx
        .await
        .map_err(|e| format!("VM thread failed: {}", e))?;

    let result = match result {
        Ok(result) => {
            // Only print the final result if it's not null, no verbose output
            if !matches!(result, hot::val::Val::Null) {
                // Apply CLI value_format option if provided, otherwise use conf
                let display_conf = if let Some(fmt) = value_format {
                    conf.set_str("hot.value.format", Some(fmt.to_string()), "hot")
                } else {
                    conf.clone()
                };
                println!("{}", result.format_with_conf(Some(&display_conf)));
            }
            Ok(result)
        }
        Err(e) => Err(format!("Execution error: {}", e)),
    };

    // Shutdown emitter and event publisher to flush any pending events
    if let Some(emitter) = emitter {
        let _ = emitter.shutdown().await;
    }
    if let Some(event_publisher) = event_publisher {
        let _ = event_publisher.shutdown().await;
    }

    result.map(|_| ())
}

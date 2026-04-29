//! `hot eval` — evaluate a Hot expression and print the result.

use hot::val::Val;

use crate::cli::GlobalOptions;
use crate::command::deploy::setup_live_build_for_dev;
use crate::conf::{
    create_emitter, create_event_publisher, get_merged_src_paths, get_merged_test_paths,
};
use crate::profile::{extract_profile_identifiers, resolve_profile_to_uuids};

pub(crate) async fn run_eval(
    code: &str,
    conf: &Val,
    global_options: &GlobalOptions,
    context_storage: Option<ahash::AHashMap<String, hot::val::Val>>,
    value_format: Option<&str>,
) -> Result<(), String> {
    // Get paths for dependency resolution - eval should load packages like run and test
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

    // Check context requirements before evaluating
    hot::lang::engine::Engine::check_sources_pipeline_with_context(
        &src_paths,
        &test_paths,
        Some(conf),
        Some(&project_name),
        context_storage.as_ref(),
        hot::env::is_local_dev(),
    )?;

    // Create or update live build for development (consistent with run/repl/check)
    setup_live_build_for_dev(conf, global_options, &src_paths, &test_paths).await?;

    // Create database pool first if needed for emitter/event publisher
    let db_pool = match hot::db::create_db_pool(conf).await {
        Ok(pool) => Some(std::sync::Arc::new(pool)),
        Err(e) => {
            tracing::warn!("Failed to create database pool for eval: {}", e);
            None
        }
    };

    // Create emitter and event publisher for eval commands (like run and test)
    // Check for emitter configuration (hot.emitter.type in config becomes emitter.type in resolved conf)
    let emitter_type_str = conf.get_str("emitter.type");
    let has_emitter_config =
        !emitter_type_str.is_empty() && emitter_type_str != "null" && emitter_type_str != "none";

    let emitter = if has_emitter_config {
        if let Some(ref pool) = db_pool {
            match create_emitter(conf, pool.as_ref()) {
                Ok(Some(emitter)) => Some(emitter),
                Ok(None) => None,
                Err(e) => {
                    tracing::warn!("Failed to create emitter for eval: {}", e);
                    None
                }
            }
        } else {
            tracing::warn!("Cannot create emitter without database connection");
            None
        }
    } else {
        None
    };

    // Check for queue configuration (hot.queue.type in config becomes queue.type in resolved conf)
    let queue_type_str = conf.get_str("queue.type");
    let has_queue_config =
        !queue_type_str.is_empty() && queue_type_str != "null" && queue_type_str != "none";

    let event_publisher = if has_queue_config {
        if let Some(ref pool) = db_pool {
            match create_event_publisher(conf, pool.as_ref()) {
                Ok(Some(publisher)) => Some(publisher),
                Ok(None) => None,
                Err(e) => {
                    tracing::warn!("Failed to create event publisher for eval: {}", e);
                    None
                }
            }
        } else {
            tracing::warn!("Cannot create event publisher without database connection");
            None
        }
    } else {
        None
    };

    // Create execution context with proper profile and build resolution
    let execution_context = if emitter.is_some() || event_publisher.is_some() {
        let run_id = uuid::Uuid::now_v7();

        // Set up live build first
        setup_live_build_for_dev(conf, global_options, &src_paths, &test_paths).await?;

        // Create execution context manually with proper profile resolution
        let db_uri = conf.get_str("db.uri");
        if !db_uri.is_empty() {
            if let Some((user_email, org_slug, env_name)) = extract_profile_identifiers(conf) {
                // Connect to database to resolve profile identifiers
                let db = hot::db::create_db_pool(conf)
                    .await
                    .map_err(|e| format!("Failed to connect to database: {}", e))?;

                // Resolve profile identifiers to UUIDs
                let (user_id, env_id, org_id) =
                    resolve_profile_to_uuids(&db, &user_email, &org_slug, &env_name)
                        .await
                        .map_err(|e| format!("Failed to resolve profile: {}", e))?;

                // For eval commands, we need a build_id due to database constraints
                // Get the most recent build to satisfy the constraint, and also get project/hash info
                let (build_id, build_hash, project_id): (
                    Option<uuid::Uuid>,
                    Option<String>,
                    Option<uuid::Uuid>,
                ) = match hot::db::build::Build::get_recent_builds(&db, Some(1)).await {
                    Ok(builds) if !builds.is_empty() => {
                        let build = &builds[0];
                        (
                            Some(build.build_id),
                            Some(build.hash.clone()),
                            Some(build.project_id),
                        )
                    }
                    Ok(_) => {
                        tracing::warn!(
                            "No builds found, eval command may fail due to database constraints"
                        );
                        (None, None, None)
                    }
                    Err(e) => {
                        tracing::warn!("Failed to get recent builds: {}, using None", e);
                        (None, None, None)
                    }
                };

                let stream_id = uuid::Uuid::now_v7(); // CLI runs start new streams
                let execution_context = hot::lang::event::ExecutionContext::new(
                    run_id,
                    stream_id,
                    hot::db::run::RunType::Run.as_id(),
                    Some(env_id),
                    Some(user_id),
                    Some(org_id),
                    build_id,
                )
                .with_build_hash(build_hash)
                .with_project(project_id, Some(project_name.clone()))
                .with_env_name(Some(env_name.clone()))
                .with_org_slug(Some(org_slug.clone()));

                Some(execution_context)
            } else {
                None
            }
        } else {
            None
        }
    } else {
        // For configuration loading and simple eval commands, skip live build setup entirely
        None
    };

    // Use the pipeline with emitter, event publisher, and context storage support
    // IMPORTANT: VM execution runs in spawn_blocking to avoid blocking the tokio runtime.
    // This allows hot-std blocking I/O (HTTP, file ops) to work correctly.
    let code_owned = code.to_string();
    let conf_for_blocking = conf.clone();
    let src_paths_clone = src_paths.clone();
    let test_paths_clone = test_paths.clone();
    let project_name_clone = project_name.clone();
    let emitter_clone = emitter.clone();
    let execution_context_clone = execution_context.clone();
    let event_publisher_clone = event_publisher.clone();

    // Same Tokio-handle dance as `run_run` — see comment there.
    let tokio_handle = tokio::runtime::Handle::current();

    let (result_tx, result_rx) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .name("hot-vm-eval".into())
        .stack_size(64 * 1024 * 1024)
        .spawn(move || {
            let _runtime_guard = tokio_handle.enter();
            let r = hot::lang::engine::Engine::eval_code_pipeline_with_all_features(
                &code_owned,
                &src_paths_clone,
                &test_paths_clone,
                Some(&conf_for_blocking),
                Some(&project_name_clone),
                emitter_clone,
                execution_context_clone,
                event_publisher_clone,
                context_storage,
            );
            let _ = result_tx.send(r);
        })
        .map_err(|e| format!("Failed to spawn VM thread: {}", e))?;
    let result = result_rx
        .await
        .map_err(|e| format!("VM thread failed: {}", e))?;

    // Shutdown emitter and event publisher if present
    if let Some(emitter) = &emitter {
        let _ = emitter.shutdown().await;
    }
    if let Some(event_publisher) = &event_publisher {
        let _ = event_publisher.shutdown().await;
    }

    match result {
        Ok(result) => {
            // Print the final result if it's not null
            if !matches!(result, hot::val::Val::Null) {
                // Apply CLI value_format option if provided, otherwise use conf
                let display_conf = if let Some(fmt) = value_format {
                    conf.set_str("hot.value.format", Some(fmt.to_string()), "hot")
                } else {
                    conf.clone()
                };
                println!("{}", result.format_with_conf(Some(&display_conf)));
            }
            Ok(())
        }
        Err(e) => Err(format!("Evaluation error: {}", e)),
    }
}

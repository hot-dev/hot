//! `hot eval` — evaluate a Hot expression and print the result.

use hot::val::Val;

use crate::cli::GlobalOptions;
use crate::command::deploy::{runs_without_database, setup_live_build_for_dev_with_context};
use crate::conf::{
    create_emitter, create_event_publisher, get_merged_src_paths, get_merged_test_paths,
};

pub(crate) async fn run_eval(
    code: &str,
    conf: &Val,
    global_options: &GlobalOptions,
    context_storage: Option<ahash::AHashMap<String, hot::val::Val>>,
    value_format: Option<&str>,
    providers: &crate::CliProviders,
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

    // No separate pre-flight check pass here: the Execute pipeline runs the
    // same post-compile validations (ctx/box/handler/schedule) itself before
    // executing any user code, so a check_sources_pipeline_with_context call
    // would just compile hot-std and the project sources a second time.

    // Reuse live-build compilation for runtime metadata preflight before any
    // live-build records are mutated. None means an explicit empty key set,
    // not structural-only validation.
    let ad_hoc_default_db = runs_without_database(conf);
    let available_context_keys = context_storage
        .as_ref()
        .map(|storage| storage.keys().cloned().collect())
        .unwrap_or_default();
    // A fresh checkout's local SQLite database is created on first use.
    crate::command::init::prepare_local_database(conf, providers).await?;
    setup_live_build_for_dev_with_context(
        conf,
        global_options,
        &src_paths,
        &test_paths,
        available_context_keys,
    )
    .await?;

    // Create emitter and event publisher for eval commands (like run and test)
    // Check for emitter configuration (hot.emitter.type in config becomes emitter.type in resolved conf)
    let emitter_type_str = conf.get_str("emitter.type");
    let has_emitter_config =
        !emitter_type_str.is_empty() && emitter_type_str != "null" && emitter_type_str != "none";

    // Check for queue configuration (hot.queue.type in config becomes queue.type in resolved conf)
    let queue_type_str = conf.get_str("queue.type");
    let has_queue_config =
        !queue_type_str.is_empty() && queue_type_str != "null" && queue_type_str != "none";

    // A managed database always participates in execution admission, even
    // when emitters and queues are disabled. Ad-hoc local eval stays ephemeral.
    let db_pool = if ad_hoc_default_db {
        None
    } else {
        Some(std::sync::Arc::new(
            crate::profile::create_cli_db_pool(conf)
                .await
                .map_err(|e| format!("Cannot evaluate against configured database: {e}"))?,
        ))
    };

    let emitter = if has_emitter_config {
        if let Some(ref pool) = db_pool {
            create_emitter(conf, pool.as_ref())
                .await
                .map_err(|e| format!("Cannot initialize configured emitter: {e}"))?
        } else {
            tracing::warn!("Cannot create emitter without database connection");
            None
        }
    } else {
        None
    };

    let event_publisher = if has_queue_config {
        if let Some(ref pool) = db_pool {
            create_event_publisher(conf, pool.as_ref())
                .await
                .map_err(|e| format!("Cannot initialize configured event publisher: {e}"))?
        } else {
            tracing::warn!("Cannot create event publisher without database connection");
            None
        }
    } else {
        None
    };

    // Resolve metadata through the same handle that execution will receive.
    let execution_context = if let Some(db) = &db_pool {
        let (user_id, env, org) =
            crate::profile::resolve_cli_environment(db.as_ref(), conf).await?;
        let project =
            hot::db::Project::get_project_by_env_and_name(db.as_ref(), &env.env_id, &project_name)
                .await
                .map_err(|e| format!("Cannot resolve selected project: {e}"))?
                .ok_or_else(|| {
                    format!("Project '{project_name}' is missing after live-build setup")
                })?;
        let build = hot::db::Build::get_live_build_by_project(db.as_ref(), &project.project_id)
            .await
            .map_err(|e| format!("Cannot resolve selected live build: {e}"))?
            .ok_or_else(|| {
                format!("Project '{project_name}' has no live build in the selected environment")
            })?;
        Some(
            hot::lang::event::ExecutionContext::new(
                uuid::Uuid::now_v7(),
                uuid::Uuid::now_v7(),
                hot::db::run::RunType::Eval.as_id(),
                Some(env.env_id),
                Some(user_id),
                Some(org.org_id),
                Some(build.build_id),
            )
            .with_build_hash(Some(build.hash))
            .with_project(Some(project.project_id), Some(project_name.clone()))
            .with_env_name(Some(env.name))
            .with_org_slug(Some(org.slug)),
        )
    } else {
        // For configuration loading and simple eval commands, skip live build setup entirely
        None
    };

    crate::command::run::begin_managed_cli_run(db_pool.as_deref(), execution_context.as_ref())
        .await?;

    // The same file storage, store and embedding provider as `hot run`.
    let file_storage: Option<std::sync::Arc<dyn hot::file_storage::FileStorage>> =
        match hot::file_storage::file_storage_from_config(conf).await {
            Ok(fs) => Some(std::sync::Arc::from(fs)),
            Err(e) => {
                tracing::debug!("File storage not available for eval: {}", e);
                None
            }
        };
    let (org_id, env_id) = execution_context
        .as_ref()
        .map(|ctx| (ctx.org_id, ctx.env_id))
        .unwrap_or((None, None));
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
    let db_pool_clone = db_pool.clone();

    // Same Tokio-handle dance as `run_run` — see comment there.
    let tokio_handle = tokio::runtime::Handle::current();

    let (result_tx, result_rx) = tokio::sync::oneshot::channel();
    let spawned = std::thread::Builder::new()
        .name("hot-vm-eval".into())
        .stack_size(64 * 1024 * 1024)
        .spawn(move || {
            let _runtime_guard = tokio_handle.enter();
            let r = hot::lang::engine::Engine::run_unified_pipeline(
                &src_paths_clone,
                &test_paths_clone,
                Some(&conf_for_blocking),
                Some(&project_name_clone),
                None,
                Some(&code_owned),
                hot::lang::engine::PipelineMode::Execute,
                emitter_clone,
                execution_context_clone,
                event_publisher_clone,
                context_storage,
                db_pool_clone,
                file_storage,
                store,
                embedding_provider,
                None,
                None,
                false,
                None,
            );
            let _ = result_tx.send(r);
        });
    let result = match spawned {
        Ok(_) => result_rx
            .await
            .unwrap_or_else(|error| Err(format!("VM thread failed: {error}"))),
        Err(error) => Err(format!("Failed to spawn VM thread: {error}")),
    };

    // Shutdown emitter and event publisher if present
    let emitter_error = if let Some(emitter) = &emitter {
        emitter.shutdown().await.err()
    } else {
        None
    };
    let publisher_error = if let Some(event_publisher) = &event_publisher {
        event_publisher.shutdown().await.err()
    } else {
        None
    };

    let emitter_error = crate::command::run::finalize_cli_delivery(
        db_pool.as_deref(),
        execution_context.as_ref(),
        &result,
        emitter_error,
    )
    .await;

    let result = match result {
        Ok(result) => {
            // Print the final result if it's not null
            if !matches!(result, hot::val::Val::Null) {
                // Apply CLI value_format option if provided, otherwise use conf
                let display_conf = if let Some(fmt) = value_format {
                    conf.set_str("value.format", Some(fmt.to_string()), "hot")
                } else {
                    conf.clone()
                };
                println!("{}", result.format_with_conf(Some(&display_conf)));
            }
            Ok(())
        }
        Err(e) => Err(format!("Evaluation error: {}", e)),
    };
    crate::command::run::combine_execution_and_delivery_results(
        result,
        emitter_error,
        publisher_error,
    )
}

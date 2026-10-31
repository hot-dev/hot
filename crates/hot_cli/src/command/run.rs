//! `hot run` — execute a single .hot file in the engine.

use hot::val::Val;

use crate::cli::GlobalOptions;
use crate::command::deploy::{runs_without_database, setup_live_build_for_dev_with_context};
use crate::conf::{
    create_emitter, create_event_publisher, get_merged_src_paths, get_merged_test_paths,
};

pub(crate) fn combine_execution_and_delivery_results(
    execution: Result<(), String>,
    emitter_error: Option<String>,
    publisher_error: Option<String>,
) -> Result<(), String> {
    let mut errors = Vec::new();
    if let Err(error) = execution {
        errors.push(error);
    }
    if let Some(error) = emitter_error {
        errors.push(format!("Failed to persist execution events: {error}"));
    }
    if let Some(error) = publisher_error {
        errors.push(format!("Failed to durably enqueue sent event: {error}"));
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

/// Persist the durable run before executing code, independently of trace settings.
pub(crate) async fn begin_managed_cli_run(
    db: Option<&hot::db::DatabasePool>,
    ctx: Option<&hot::lang::event::ExecutionContext>,
) -> Result<(), String> {
    let Some(db) = db else {
        return Ok(());
    };
    let ctx = ctx.ok_or("Managed CLI execution requires an execution context")?;
    let (Some(org_id), Some(env_id), Some(user_id), Some(build_id)) =
        (ctx.org_id, ctx.env_id, ctx.user_id, ctx.build_id)
    else {
        return Err(
            "Managed CLI execution requires an exact user, organization, environment, and build"
                .into(),
        );
    };
    let env = hot::db::Env::get_env_uncached(db, &env_id)
        .await
        .map_err(|error| error.to_string())?;
    if env.org_id != org_id || !env.active {
        return Err("Managed CLI environment ownership mismatch".into());
    }
    crate::profile::ensure_build_environment(db, &build_id, &env_id).await?;
    hot::db::Run::insert_run(
        db,
        &ctx.run_id,
        &env_id,
        &ctx.stream_id,
        Some(&build_id),
        ctx.run_type_id,
        None,
        &user_id,
        None,
        None,
    )
    .await
    .map_err(|error| format!("Cannot persist CLI run before execution: {error}"))
}

/// Run after emitter shutdown so trace terminal states remain authoritative.
pub(crate) async fn finalize_managed_cli_run(
    db: Option<&hot::db::DatabasePool>,
    ctx: Option<&hot::lang::event::ExecutionContext>,
    result: &Result<Val, String>,
) -> Result<(), String> {
    let Some(db) = db else {
        return Ok(());
    };
    let ctx = ctx.ok_or("Managed CLI execution requires an execution context")?;
    let env_id = ctx
        .env_id
        .ok_or("Managed CLI execution requires an environment")?;
    let status = match result {
        Ok(value) if value.is_cancelled() => hot::db::RunStatus::Cancelled,
        Ok(value) if !value.is_err() => hot::db::RunStatus::Succeeded,
        _ => hot::db::RunStatus::Failed,
    };
    hot::db::Run::finish_if_running(db, &env_id, &ctx.run_id, &status)
        .await
        .map_err(|error| format!("Cannot finalize CLI run: {error}"))
}

fn combine_persistence_errors(
    emitter: Option<String>,
    lifecycle: Option<String>,
) -> Option<String> {
    match (emitter, lifecycle) {
        (Some(emitter), Some(lifecycle)) => Some(format!("{emitter}; {lifecycle}")),
        (Some(error), None) | (None, Some(error)) => Some(error),
        (None, None) => None,
    }
}

pub(crate) async fn finalize_cli_delivery(
    db: Option<&hot::db::DatabasePool>,
    ctx: Option<&hot::lang::event::ExecutionContext>,
    result: &Result<Val, String>,
    emitter_error: Option<String>,
) -> Option<String> {
    combine_persistence_errors(
        emitter_error,
        finalize_managed_cli_run(db, ctx, result).await.err(),
    )
}

pub(crate) async fn run_run(
    file_path: &str,
    conf: &Val,
    global_options: &GlobalOptions,
    context_storage: Option<ahash::AHashMap<String, hot::val::Val>>,
    value_format: Option<&str>,
    providers: &crate::CliProviders,
) -> Result<(), String> {
    // Check if target file exists
    let path = std::path::Path::new(file_path);
    if !path.exists() {
        return Err(format!("File not found: {}", file_path));
    }

    // Get paths for dependency resolution - include the file's directory as a source path
    // A bare file name's parent is "", which must mean the current directory.
    let file_dir = path
        .parent()
        .and_then(|p| p.to_str())
        .filter(|p| !p.is_empty())
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
        &_test_paths,
        available_context_keys,
    )
    .await?;

    // The default ad-hoc database path is known to refuse pool creation.
    // Avoid the doomed connection attempt so emitters, event publishers, and
    // run records cannot initialize before Execute validation.
    let db_pool = if ad_hoc_default_db {
        None
    } else {
        Some(std::sync::Arc::new(
            crate::profile::create_cli_db_pool(conf)
                .await
                .map_err(|e| format!("Cannot execute against configured database: {e}"))?,
        ))
    };

    // Preserve the exact profile environment for execution and file storage.
    let (org_id, org_slug, env_id, env_name, user_id, build_id) = if let Some(ref pool) = db_pool {
        let (user, env, org) = crate::profile::resolve_cli_environment(pool.as_ref(), conf).await?;
        let project = hot::db::Project::get_project_by_env_and_name(
            pool.as_ref(),
            &env.env_id,
            &project_name,
        )
        .await
        .map_err(|e| format!("Cannot resolve selected project: {e}"))?
        .ok_or_else(|| {
            format!("Project '{project_name}' is missing from the selected environment")
        })?;
        let build = hot::db::Build::get_live_build_by_project(pool.as_ref(), &project.project_id)
            .await
            .map_err(|e| format!("Cannot resolve selected live build: {e}"))?
            .ok_or_else(|| {
                format!("Project '{project_name}' has no live build in the selected environment")
            })?;
        (
            Some(org.org_id),
            Some(org.slug),
            Some(env.env_id),
            Some(env.name),
            Some(user),
            Some(build.build_id),
        )
    } else {
        tracing::debug!("CLI: No db_pool available, skipping org/env/user lookup");
        (None, None, None, None, None, None)
    };

    // Local file storage construction is lazy and non-mutating, so preserve
    // provider availability for valid ad-hoc programs.
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
            create_emitter(conf, pool.as_ref())
                .await
                .map_err(|e| format!("Cannot initialize configured emitter: {e}"))?
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
            create_event_publisher(conf, pool.as_ref())
                .await
                .map_err(|e| format!("Cannot initialize configured event publisher: {e}"))?
        } else {
            None
        }
    } else {
        None
    };

    let execution_context = if db_pool.is_some()
        || emitter.is_some()
        || event_publisher.is_some()
        || file_storage.is_some()
    {
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
                build_id,
            )
            .with_env_name(env_name.clone())
            .with_org_slug(org_slug.clone()),
        )
    } else {
        None
    };

    begin_managed_cli_run(db_pool.as_deref(), execution_context.as_ref()).await?;

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

    // Scope storage to the exact selected org/env on both database backends.
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
    let spawned = std::thread::Builder::new()
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
                None, // No warnings out
            );
            let _ = result_tx.send(r);
        });
    let result = match spawned {
        Ok(_) => result_rx
            .await
            .unwrap_or_else(|error| Err(format!("VM thread failed: {error}"))),
        Err(error) => Err(format!("Failed to spawn VM thread: {error}")),
    };

    let result = match result {
        Ok(result) => {
            // Only print the final result if it's not null, no verbose output
            if !matches!(result, hot::val::Val::Null) {
                // Apply CLI value_format option if provided, otherwise use conf
                let display_conf = if let Some(fmt) = value_format {
                    conf.set_str("value.format", Some(fmt.to_string()), "hot")
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
    let emitter_error = if let Some(emitter) = emitter {
        emitter.shutdown().await.err()
    } else {
        None
    };
    let publisher_error = if let Some(event_publisher) = event_publisher {
        event_publisher.shutdown().await.err()
    } else {
        None
    };

    let emitter_error = finalize_cli_delivery(
        db_pool.as_deref(),
        execution_context.as_ref(),
        &result,
        emitter_error,
    )
    .await;
    combine_execution_and_delivery_results(result.map(|_| ()), emitter_error, publisher_error)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cli_lifecycle_preserves_terminal_emitter_results_and_rejects_wrong_environment() {
        use hot::db::{Run, RunStatus};
        let db = hot::db::test_db().await;
        let fixture = hot::db::insert_test_data(&db).await.unwrap();
        let context = || {
            hot::lang::event::ExecutionContext::new(
                uuid::Uuid::now_v7(),
                uuid::Uuid::now_v7(),
                hot::db::run::RunType::Run.as_id(),
                Some(fixture.env_id),
                Some(fixture.user_id),
                Some(fixture.org_id),
                Some(fixture.build_id),
            )
        };
        let ctx = context();
        begin_managed_cli_run(Some(&db), Some(&ctx)).await.unwrap();
        Run::update_stop_time_and_status(&db, &ctx.run_id, None, &RunStatus::Cancelled)
            .await
            .unwrap();
        let emitted = Run::get_run(&db, &ctx.run_id).await.unwrap();
        finalize_managed_cli_run(Some(&db), Some(&ctx), &Ok(Val::Null))
            .await
            .unwrap();
        let after = Run::get_run(&db, &ctx.run_id).await.unwrap();
        assert_eq!(after.status_id, RunStatus::Cancelled.as_id());
        assert_eq!(after.stop_time, emitted.stop_time);

        let failed = context();
        begin_managed_cli_run(Some(&db), Some(&failed))
            .await
            .unwrap();
        finalize_managed_cli_run(Some(&db), Some(&failed), &Err("VM thread failed".into()))
            .await
            .unwrap();
        assert_eq!(
            Run::get_run(&db, &failed.run_id).await.unwrap().status_id,
            RunStatus::Failed.as_id()
        );
        let hot_error = context();
        begin_managed_cli_run(Some(&db), Some(&hot_error))
            .await
            .unwrap();
        finalize_managed_cli_run(
            Some(&db),
            Some(&hot_error),
            &Ok(Val::err(Val::from("expected failure"))),
        )
        .await
        .unwrap();
        assert_eq!(
            Run::get_run(&db, &hot_error.run_id)
                .await
                .unwrap()
                .status_id,
            RunStatus::Failed.as_id()
        );
        assert!(
            Run::finish_if_running(
                &db,
                &uuid::Uuid::new_v4(),
                &fixture.run_id,
                &RunStatus::Succeeded
            )
            .await
            .is_err()
        );
        assert_eq!(
            Run::get_run(&db, &fixture.run_id).await.unwrap().status_id,
            RunStatus::Running.as_id()
        );
    }

    #[tokio::test]
    async fn managed_run_and_eval_without_emitter_finish_in_selected_environment() {
        use hot::db::{Build, Env, Org, Project, Run, RunStatus, User};
        let dir = tempfile::tempdir().unwrap();
        let source_dir = dir.path().canonicalize().unwrap().join("src");
        std::fs::create_dir(&source_dir).unwrap();
        let source = source_dir.join("example.hot");
        std::fs::write(&source, "::managed_cli ns\nanswer 42\n").unwrap();
        let conf =
            hot::val!({"db":{"uri":format!("sqlite:{}", dir.path().join("managed.db").display())}});
        hot::db::run_migrations(&conf).await.unwrap();
        let db = hot::db::create_db_pool(&conf).await.unwrap();
        let fixture = hot::db::insert_test_data(&db).await.unwrap();
        let user = User::get_user(&db, &fixture.user_id).await.unwrap();
        let org = Org::get_org_uncached(&db, &fixture.org_id).await.unwrap();
        let selected_env = uuid::Uuid::new_v4();
        Env::insert_env(
            &db,
            &selected_env,
            &fixture.org_id,
            "selected",
            &fixture.user_id,
        )
        .await
        .unwrap();
        let project = Project::insert_or_get_project(
            &db,
            &uuid::Uuid::new_v4(),
            &selected_env,
            "managed",
            &fixture.user_id,
        )
        .await
        .unwrap();
        Build::insert_or_update_live_build(&db, &project.project_id, "live", 0, &fixture.user_id)
            .await
            .unwrap();
        let conf = conf.merge(&hot::val!({
            "set":{"profile":"selected"},
            "profile":{"selected":{"user":{"email":user.email},"org":{"slug":org.slug},"env":{"name":"selected"}}},
            "project":{"managed":{"src":{"paths":[]},"test":{"paths":[]}}},
            "deploy":{"auto":false},
            "emitter":{"type":"none"}, "queue":{"type":"none"},
            "file":{"storage":{"path":dir.path().join("files").to_string_lossy().to_string()}}
        }));
        let options = crate::cli::GlobalOptions {
            project: Some("managed".into()),
            src_paths: vec![source_dir.to_string_lossy().to_string()],
            ..Default::default()
        };
        let providers = crate::CliProviders::default();
        run_run(
            source.to_str().unwrap(),
            &conf,
            &options,
            None,
            None,
            &providers,
        )
        .await
        .unwrap();
        crate::command::eval::run_eval("add(2, 3)", &conf, &options, None, None, &providers)
            .await
            .unwrap();
        let runs = Run::get_runs_by_env(&db, &selected_env, Some(10), None)
            .await
            .unwrap();
        assert_eq!(runs.len(), 2);
        assert!(runs.iter().all(|run| run.status_id == RunStatus::Succeeded.as_id() && run.stop_time.is_some()));
        assert!(
            runs.iter()
                .any(|run| run.run_type_id == hot::db::run::RunType::Eval.as_id())
        );
        assert_eq!(
            Run::get_run(&db, &fixture.run_id).await.unwrap().status_id,
            RunStatus::Running.as_id()
        );
    }

    #[test]
    fn successful_execution_reports_database_flush_failure() {
        let error = combine_execution_and_delivery_results(
            Ok(()),
            Some("RLS denied writer flush".into()),
            None,
        )
        .unwrap_err();
        assert!(error.contains("Failed to persist execution events"));
        assert!(error.contains("RLS denied writer flush"));
    }

    #[test]
    fn durable_send_failure_is_never_silenced() {
        let error = combine_execution_and_delivery_results(
            Ok(()),
            None,
            Some("queue directory is not writable".to_string()),
        )
        .unwrap_err();
        assert!(error.contains("Failed to durably enqueue sent event"));
        assert!(error.contains("not writable"));
    }

    #[test]
    fn execution_and_durable_send_failures_are_both_reported() {
        let error = combine_execution_and_delivery_results(
            Err("handler failed".to_string()),
            Some("RLS denied writer flush".to_string()),
            Some("enqueue failed".to_string()),
        )
        .unwrap_err();
        assert!(error.contains("handler failed"));
        assert!(error.contains("enqueue failed"));
        assert!(error.contains("RLS denied writer flush"));
    }
}

//! `hot dev` — file-watching local development server. Spins up the API,
//! worker, scheduler, and watcher in a single process.

use std::path::Path;

use hot::stream::{StreamPubSub, StreamPubSubType};
use hot::val::Val;
use tracing::{error, info, warn};

use crate::Env;
use crate::build_info;
use crate::cli::{EmitterOptions, GlobalOptions};
use crate::command::api::{run_api_with_stream_pubsub, run_app};
use crate::command::deploy::setup_live_build_for_dev;
use crate::command::init::check_and_run_init_if_needed;
use crate::command::scheduler::run_scheduler;
use crate::command::worker::{run_task_worker, run_worker_with_stream_pubsub};
use crate::conf::{
    apply_env_vars, create_default_conf, get_merged_src_paths, get_merged_test_paths,
    load_dotenv_files, reload_conf_after_init,
};

pub(crate) async fn run_dev(
    conf: Val,
    context_storage: Option<ahash::AHashMap<String, hot::val::Val>>,
    open_browser: bool,
    providers: &crate::CliProviders,
) {
    info!(
        "hot.dev: starting development servers, version: {} ({})",
        build_info::VERSION,
        build_info::git_sha_short()
    );

    // Check if initialization is needed
    let init_ran = match check_and_run_init_if_needed(&conf, providers).await {
        Ok(ran) => ran,
        Err(e) => {
            error!("Auto-initialization failed: {}", e);
            error!("Please run 'hot init' manually to set up your environment");
            std::process::exit(1);
        }
    };

    // If init was run, reload configuration to pick up correct paths
    // After init, .hot/ directory exists, so has_project_config() will return true
    // and db paths will correctly resolve to .hot/db/
    let conf = if init_ran {
        info!("hot.dev: Reloading configuration after initialization...");
        // Give SQLite time to fully checkpoint WAL and release file locks after init
        // This prevents race conditions where services connect before schema is visible
        tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;

        if Path::new("hot.hot").exists() {
            // Full reload including hot.hot config file
            match reload_conf_after_init(&conf) {
                Ok(new_conf) => new_conf,
                Err(e) => {
                    error!("Failed to reload configuration: {}", e);
                    error!("Please restart 'hot dev' to use the new configuration");
                    std::process::exit(1);
                }
            }
        } else {
            // No hot.hot file, but .hot/ directory exists after init
            // Rebuild default config which will now use local .hot/db/ path
            let mut new_conf = create_default_conf();
            new_conf = apply_env_vars(new_conf);
            new_conf
        }
    } else {
        conf
    };

    // Create or update live build for development
    // Extract global options from conf for live build setup
    let global_options = GlobalOptions {
        conf_files: vec![],
        ctx_files: vec![],
        project: None,
        src_paths: vec![],
        test_paths: vec![],
        resource_paths: vec![],
        no_gitignore: false,
        engine_threads: None,
        jit_mode: None,
        jit_threshold: None,
        db_uri: None,
        log_level: None,
        log_target: None,
        log_dir: None,
        log_rotation: None,
        log_retention: None,
        log_format: None,
        deploy_auto: true,
        emitter: EmitterOptions { emitter_type: None },
        with_tests: None,
    };

    let src_paths = get_merged_src_paths(&conf, None, &[]);
    let test_paths = get_merged_test_paths(&conf, None, &[]);

    // Check context requirements before proceeding
    let project_name = hot::project::get_default_project_name(&conf);
    if let Err(e) = hot::lang::engine::Engine::check_sources_pipeline_with_context(
        &src_paths,
        &test_paths,
        Some(&conf),
        Some(&project_name),
        context_storage.as_ref(),
        hot::env::is_local_dev(),
    ) {
        if hot::env::is_local_dev() {
            warn!("{}", e);
            warn!(
                "Continuing in local dev mode — set context variables via the app UI or ctx.hot file"
            );
        } else {
            error!("{}", e);
            std::process::exit(1);
        }
    }

    // Run database migrations to ensure schema is up to date
    // This is safe to run every time - migrations are idempotent
    if !conf.get_str("db.uri").is_empty() {
        info!("hot.dev: Running database migrations...");
        if let Err(e) = crate::run_migrations_with_bootstrap(&conf, providers).await {
            crate::report_migration_failure("Failed to run database migrations", &e);
            error!("Cannot start dev servers without a valid database schema");
            std::process::exit(1);
        }
    }

    // CRITICAL: Wait for live build to complete before starting any services
    // This ensures event handlers and schedules are ready before worker processes events
    info!("hot.dev: Setting up live build and extracting event handlers...");
    if let Err(e) = setup_live_build_for_dev(&conf, &global_options, &src_paths, &test_paths).await
    {
        error!("Failed to setup live build for dev: {}", e);
        error!("Cannot start dev servers without a valid build");
        std::process::exit(1);
    }
    info!("hot.dev: Live build ready, event handlers extracted and deployed");

    // Create a shared stream pub/sub for development mode
    // This ensures API and Worker share the same in-memory pub/sub
    let queue_type_str = conf.get_str_or_default("queue.type", "memory");
    let shared_stream_pubsub: Option<std::sync::Arc<StreamPubSub>> = if queue_type_str == "memory" {
        // In-memory mode requires shared pub/sub instance
        match StreamPubSub::new(StreamPubSubType::Memory, None, false) {
            Ok(pubsub) => {
                info!("hot.dev: Created shared in-memory stream pub/sub for API, APP, and Worker");
                Some(std::sync::Arc::new(pubsub))
            }
            Err(e) => {
                warn!(
                    "hot.dev: Failed to create shared stream pub/sub: {}. SSE streaming may not work.",
                    e
                );
                None
            }
        }
    } else {
        // Redis mode - each service connects to Redis independently
        None
    };

    // Now spawn services in order: API, APP, Scheduler, then Worker
    // API and APP don't depend on build, so they can start immediately
    // Both API and APP need the shared stream pub/sub for real-time SSE updates
    let api_handle = tokio::spawn(run_api_with_stream_pubsub(
        Env::Development,
        conf.clone(),
        shared_stream_pubsub.clone(),
    ));
    let app_handle = tokio::spawn(run_app(
        Env::Development,
        conf.clone(),
        shared_stream_pubsub.clone(),
    ));

    // Open browser to APP URL if requested (via --open flag or dev.open config)
    if open_browser {
        // Brief delay to ensure server is listening
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

        let app_host = conf
            .get("app")
            .and_then(|app| {
                let host = app.get_str("host");
                if host.is_empty() { None } else { Some(host) }
            })
            .unwrap_or_else(|| "127.0.0.1".to_string());
        let app_port = conf
            .get("app")
            .map(|app| app.get_int("port") as u16)
            .unwrap_or(4680);

        let url = format!("http://{}:{}", app_host, app_port);
        info!("hot.dev: Opening browser to {}", url);

        if let Err(e) = open::that(&url) {
            warn!("hot.dev: Failed to open browser: {}", e);
        }
    }

    // Scheduler needs to sync schedules from DB, but can run concurrently with worker
    let scheduler_handle = tokio::spawn(run_scheduler(Env::Development, conf.clone()));

    // Check Docker availability when the backend is Docker
    let box_enabled = conf.get_bool_or_default("box.enabled", false);
    let box_backend = conf
        .get("box")
        .and_then(|b| b.get("backend"))
        .map(|v| v.to_string().trim_matches('"').to_string())
        .unwrap_or_else(|| "docker".to_string());

    if box_enabled && box_backend == "docker" {
        match std::process::Command::new("docker")
            .arg("info")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
        {
            Ok(status) if status.success() => {}
            _ => {
                warn!(
                    "hot.dev: Docker not found or not running — \
                     ::hot::box/start tasks will fail. \
                     Install and start Docker to enable container tasks, \
                     or set hot.box.enabled to false."
                );
            }
        }
    }

    // Start task worker for processing ::hot::task/start and ::hot::box/start tasks
    info!("hot.dev: Starting task worker...");
    let task_worker_handle = tokio::spawn(run_task_worker(conf.clone()));

    // Worker is started last to ensure everything else is ready
    // Add a small delay to let scheduler perform initial sync
    tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
    info!("hot.dev: Starting worker...");
    let worker_handle = tokio::spawn(run_worker_with_stream_pubsub(
        Env::Development,
        conf.clone(),
        context_storage,
        shared_stream_pubsub,
    ));

    // Start file watcher for hot reload of event handlers and schedules
    // Use a shared shutdown flag so we can cleanly stop the watcher
    let watcher_shutdown = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let watcher_shutdown_clone = watcher_shutdown.clone();
    let watcher_handle = tokio::spawn(run_dev_file_watcher(
        conf.clone(),
        global_options,
        src_paths,
        watcher_shutdown_clone,
    ));

    // Wait for main services to complete (they will complete when Ctrl-C is pressed)
    let _ = tokio::join!(
        api_handle,
        app_handle,
        scheduler_handle,
        worker_handle,
        task_worker_handle
    );

    // Signal the file watcher to shut down and wait briefly for it
    watcher_shutdown.store(true, std::sync::atomic::Ordering::SeqCst);
    // Give the watcher up to 200ms to notice the shutdown flag and exit cleanly
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
    watcher_handle.abort();

    info!("hot.dev: all services shut down");

    // Force exit to terminate any orphaned spawn_blocking tasks (e.g., VM compilation)
    // These cannot be cancelled gracefully and would otherwise block the tokio runtime
    std::process::exit(0);
}

/// File watcher for dev mode - watches source files and reloads event handlers/schedules on change
pub(crate) async fn run_dev_file_watcher(
    conf: Val,
    global_options: GlobalOptions,
    src_paths: Vec<String>,
    shutdown: std::sync::Arc<std::sync::atomic::AtomicBool>,
) {
    use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
    use std::path::PathBuf;
    use std::sync::mpsc::channel;
    use std::time::{Duration, Instant};

    let (tx, rx) = channel();
    let mut watcher: RecommendedWatcher = match Watcher::new(tx, notify::Config::default()) {
        Ok(w) => w,
        Err(e) => {
            error!("hot.dev: Failed to create file watcher: {}", e);
            return;
        }
    };

    // Collect paths to watch: src_paths + local package dependency paths
    let mut watch_paths: Vec<PathBuf> = src_paths.iter().map(PathBuf::from).collect();

    // Add local package dependency paths
    let project_name = hot::project::get_default_project_name(&conf);
    if let Ok(resolved_deps) = hot::project::get_resolved_project_dependencies(&conf, &project_name)
    {
        for dep in resolved_deps {
            // Only watch local dependencies (not git-fetched ones in .hot/cache)
            // A dependency is "local" if it was resolved from a local path (not from .hot/cache)
            if dep.spec.local.is_some() && !dep.resolved_path.starts_with(".hot/cache") {
                watch_paths.push(dep.resolved_path);
            }
        }
    }

    // Resource roots — the same set that ends up in the bundle so that an
    // edit to a prompt template or a JSON config in dev triggers the same
    // reload semantics as a code change. We track them separately from the
    // src/dep paths because resource changes refresh the in-process
    // resource registry rather than recompiling event handlers.
    let resource_roots: Vec<PathBuf> =
        hot::project::get_project_resource_paths(&conf, &project_name)
            .into_iter()
            .map(PathBuf::from)
            .filter(|p| p.exists())
            .collect();
    let mut canonical_resource_roots: Vec<PathBuf> = resource_roots
        .iter()
        .filter_map(|p| p.canonicalize().ok())
        .collect();
    canonical_resource_roots.sort();
    canonical_resource_roots.dedup();

    // Set up watchers for src + dep paths.
    let mut watched_count = 0;
    for path in &watch_paths {
        if path.exists() {
            if let Err(e) = watcher.watch(path, RecursiveMode::Recursive) {
                warn!("hot.dev: Failed to watch path {}: {}", path.display(), e);
            } else {
                watched_count += 1;
            }
        }
    }

    // Set up watchers for resource roots (deduped against src paths in case
    // a project lists the same dir as both source and resource — `notify`
    // tolerates duplicate watches but it's wasteful to re-fire on them).
    let already_watched: std::collections::HashSet<PathBuf> = watch_paths
        .iter()
        .filter_map(|p| p.canonicalize().ok())
        .collect();
    let mut watched_resource_count = 0;
    for path in &resource_roots {
        let canon = path.canonicalize().ok();
        if canon
            .as_ref()
            .map(|c| already_watched.contains(c))
            .unwrap_or(false)
        {
            continue;
        }
        if let Err(e) = watcher.watch(path, RecursiveMode::Recursive) {
            warn!(
                "hot.dev: Failed to watch resource path {}: {}",
                path.display(),
                e
            );
        } else {
            watched_resource_count += 1;
        }
    }

    // Watch .env files in the current directory (non-recursive)
    // We watch the current directory and filter for .env/.env.local files
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    if let Err(e) = watcher.watch(&cwd, RecursiveMode::NonRecursive) {
        warn!(
            "hot.dev: Failed to watch current directory for .env files: {}",
            e
        );
    }

    if watched_count == 0 && watched_resource_count == 0 {
        warn!("hot.dev: No paths available to watch for file changes");
        return;
    }

    info!(
        "hot.dev: Watching {} path(s) for changes to reload event handlers and schedules",
        watched_count
    );
    if watched_resource_count > 0 {
        info!(
            "hot.dev: Watching {} resource path(s) for live ::hot::resource refresh",
            watched_resource_count
        );
    }
    info!("hot.dev: Watching .env for environment variable changes");

    let debounce = Duration::from_millis(500);
    let mut last_rebuild = Instant::now();
    let mut last_env_reload = Instant::now();
    let mut last_resource_reload = Instant::now();
    let mut pending_hot = false;
    let mut pending_env = false;
    let mut pending_resource = false;

    // True iff `path` lives under one of the resource roots. Used to decide
    // whether a non-`.hot` change is a resource edit we should react to,
    // vs noise from an unrelated file in a watched src dir.
    let is_resource_path = |path: &std::path::Path| -> bool {
        if canonical_resource_roots.is_empty() {
            return false;
        }
        let canon = path.canonicalize().ok();
        let target = canon.as_deref().unwrap_or(path);
        canonical_resource_roots
            .iter()
            .any(|root| target.starts_with(root))
    };

    loop {
        // Check if shutdown was requested
        if shutdown.load(std::sync::atomic::Ordering::SeqCst) {
            return;
        }

        // Use recv_timeout to periodically check for pending changes and shutdown flag
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Ok(event)) => {
                // Check for .hot files
                let is_hot = event
                    .paths
                    .iter()
                    .any(|p| p.extension().and_then(|e| e.to_str()) == Some("hot"));

                // Check for .env file
                let is_env = event.paths.iter().any(|p| {
                    p.file_name()
                        .and_then(|n| n.to_str())
                        .map(|n| n == ".env")
                        .unwrap_or(false)
                });

                // Resource change = any non-.hot file under a resource root.
                // We exclude .hot intentionally so a `.hot` source file that
                // also happens to live under a resource root is still
                // treated as a code change (which is the more useful
                // behaviour: it triggers handler/schedule reload).
                let is_resource = !is_hot && event.paths.iter().any(|p| is_resource_path(p));

                match event.kind {
                    EventKind::Modify(_)
                    | EventKind::Create(_)
                    | EventKind::Remove(_)
                    | EventKind::Any => {
                        if is_hot {
                            pending_hot = true;
                        }
                        if is_env {
                            pending_env = true;
                        }
                        if is_resource {
                            pending_resource = true;
                        }
                    }
                    _ => {}
                }
            }
            Ok(Err(_)) => {
                // Watch error - ignore and continue
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                // Timeout - check if we have pending changes to process
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                // Channel closed - exit
                info!("hot.dev: File watcher shutting down");
                return;
            }
        }

        // Process pending .env changes after debounce period
        if pending_env && last_env_reload.elapsed() >= debounce {
            pending_env = false;
            info!("hot.dev: .env file changed, reloading environment variables...");

            // Reload .env files
            load_dotenv_files();

            info!("hot.dev: Environment variables reloaded");
            last_env_reload = Instant::now();
        }

        // Process pending resource changes after debounce period.
        // Rebuilds the in-process `ResourceRegistry` so subsequent
        // `::hot::resource/load`, `/list`, etc. observe the new content
        // without restarting `hot dev`. Pure data refresh — does not
        // touch event handlers / schedules.
        if pending_resource && last_resource_reload.elapsed() >= debounce {
            pending_resource = false;
            info!("hot.dev: Resource files changed, refreshing ::hot::resource registry...");
            let _ = hot::project::install_resource_registry(
                &conf,
                &project_name,
                &global_options.resource_paths,
                global_options.no_gitignore,
            );
            // A resource edit may have touched a `*.skill.md` source —
            // regenerate the corresponding `*.skill.hot` stubs so the
            // next compile pass picks up the new metadata or body.
            // Stub writes themselves trigger the .hot watcher branch
            // above, which then reloads handlers/schedules.
            let report = hot::skill_codegen::run_skill_codegen_from_conf(
                &conf,
                &project_name,
                &global_options.resource_paths,
            );
            if report.any_changes() {
                info!("hot.dev: {}", report.summary());
                for (path, err) in &report.errors {
                    warn!(
                        "hot.dev: skill codegen skipped {} ({})",
                        path.display(),
                        err
                    );
                }
            }
            last_resource_reload = Instant::now();
        }

        // Process pending .hot changes after debounce period
        if pending_hot && last_rebuild.elapsed() >= debounce {
            pending_hot = false;

            info!("hot.dev: Source files changed, reloading event handlers and schedules...");

            // Invalidate live docs cache so they regenerate on next request
            if let Some(cache) = hot_app::build_cache::get_build_docs_cache() {
                cache.invalidate_all_live_docs().await;
            }

            // Re-fetch paths in case they changed
            let current_src_paths = get_merged_src_paths(&conf, None, &[]);
            let current_test_paths = get_merged_test_paths(&conf, None, &[]);

            match setup_live_build_for_dev(
                &conf,
                &global_options,
                &current_src_paths,
                &current_test_paths,
            )
            .await
            {
                Ok(()) => {
                    info!("hot.dev: Event handlers and schedules reloaded successfully");
                }
                Err(e) => {
                    error!(
                        "hot.dev: Failed to reload event handlers and schedules: {}",
                        e,
                    );
                    error!("hot.dev: Fix the error and save again to retry");
                }
            }

            // Drain any events that accumulated during the rebuild to prevent immediate re-trigger
            while rx.try_recv().is_ok() {}

            // Reset timer AFTER rebuild completes and events are drained
            last_rebuild = Instant::now();
        }
    }
}

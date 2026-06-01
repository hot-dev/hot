mod build_info;
mod cli;
mod command;
mod conf;
mod profile;
mod remote;
mod update;

use crate::cli::{CacheAction, Cli, Command, ConfAction, DbAction, HIDDEN_COMMANDS_HELP};
use crate::command::ai::run_ai;
use crate::command::api::run_api;
use crate::command::app::run_app;
use crate::command::build::run_build;
use crate::command::builds::run_builds;
use crate::command::check::{run_check_watch, run_check_with_raw};
use crate::command::compile::run_compile;
use crate::command::conf::run_conf_generate;
use crate::command::context::run_context;
use crate::command::db::run_db;
use crate::command::deploy::{run_deploy, run_upload};
use crate::command::deps::run_deps;
use crate::command::dev::run_dev;
use crate::command::eval::run_eval;
use crate::command::extract::run_extract;
use crate::command::fmt::run_fmt;
use crate::command::init::run_init;
use crate::command::project::{run_project_action, run_projects};
use crate::command::queue::run_queue;
use crate::command::repl::run_repl;
use crate::command::run::run_run;
use crate::command::scheduler::run_scheduler;
use crate::command::test::run_test;
use crate::command::worker::{run_task_worker, run_worker};
use crate::conf::{
    ExtractedOptions, apply_command_specific_defaults, apply_configuration_options, apply_env_vars,
    create_default_conf, extract_options_from_command, get_emitter_resolved_conf,
    get_log_format_for_command, get_merged_src_paths, get_merged_test_paths, load_conf, load_ctx,
    load_dotenv_files, show_command_config,
};

// Log a structured OOM line to stderr before the inevitable abort. The
// custom GlobalAlloc only adds a null-check fast path on the alloc-failure
// branch, so the steady-state cost is effectively zero.
#[global_allocator]
static GLOBAL_ALLOC: hot::lang::runtime::oom_logger::LoggingAllocator =
    hot::lang::runtime::oom_logger::LoggingAllocator;

use clap::{CommandFactory, Parser as ClapParser};
use clap_complete::{generate, generate_to};

use hot::lang::cache::paths as cache_paths;
use hot::lang::emitter::EngineEventEmitter;
use hot::queue::QueueType;
use hot::val::Val;

use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use tracing::{error, info};

// Struct to hold all the context needed for running Hot code (excluding compiler)
pub struct RunContext {
    pub execution_context: hot::lang::event::ExecutionContext,
    pub emitter: Option<std::sync::Arc<dyn EngineEventEmitter>>,
    pub event_publisher: Option<std::sync::Arc<dyn hot::lang::event::EventPublisher>>,
}

/// Refuse to start a multi-process service when the queue backend is the
/// in-process `memory` queue. The memory queue is a single-process channel
/// (see `crates/hot/src/queue/mem.rs`) — running e.g. `hot worker` and
/// `hot scheduler` as separate processes against it silently produces a
/// split-brain where the scheduler enqueues into one process's heap and
/// the worker reads from a different, empty heap.
///
/// `hot dev` is the only command that legitimately uses memory mode
/// because every service runs in the same process under one runtime.
///
/// On violation: print an actionable message and exit with code 1 so
/// orchestrators (systemd, k8s, supervisord, foreman) treat it as a
/// configuration failure rather than a silent no-op.
fn enforce_redis_queue_for_standalone_service(service_name: &str, conf: &Val) {
    let queue_type_str = conf.get_str_or_default("queue.type", "memory");
    let queue_type = QueueType::from_str(&queue_type_str).unwrap_or(QueueType::Memory);

    if matches!(queue_type, QueueType::Memory) {
        eprintln!(
            "error: `hot {service}` requires `queue.type = \"redis\"` because\n\
             it is a separate process from other hot services (worker, scheduler,\n\
             api, app, task-worker). The `memory` queue is in-process only and\n\
             cannot be shared across processes — using it here would silently\n\
             drop messages between producers and consumers.\n\
             \n\
             To fix:\n\
               • Set `hot.queue.type \"redis\"` in your hot.hot config, OR\n\
               • Pass `--queue.type=redis` on the CLI, OR\n\
               • Set `HOT_QUEUE_TYPE=redis` in the environment\n\
             \n\
             Make sure `hot.redis.uri` (or `HOT_REDIS_URI`) points at a running\n\
             Redis instance (e.g. `redis://localhost:6379`).\n\
             \n\
             If you want a single-process dev setup with all services bundled,\n\
             use `hot dev` instead — it runs everything in one process and is\n\
             the only command that supports the in-memory queue.",
            service = service_name,
        );
        std::process::exit(1);
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Env {
    Development,
    Production,
}

#[derive(Debug, Clone, Copy)]
pub struct ProductIdentity {
    pub display_name: &'static str,
}

impl Default for ProductIdentity {
    fn default() -> Self {
        Self {
            display_name: "hot",
        }
    }
}

pub trait DatabaseBootstrap: Send + Sync {
    fn bootstrap<'a>(
        &'a self,
        conf: &'a Val,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + 'a>>;
}

#[derive(Debug, Default)]
pub struct NoopDatabaseBootstrap;

impl DatabaseBootstrap for NoopDatabaseBootstrap {
    fn bootstrap<'a>(
        &'a self,
        _conf: &'a Val,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(async { Ok(()) })
    }
}

#[derive(Clone)]
pub struct CliProviders {
    pub identity: ProductIdentity,
    pub default_conf: Val,
    pub database_bootstrap: std::sync::Arc<dyn DatabaseBootstrap>,
}

impl Default for CliProviders {
    fn default() -> Self {
        Self {
            identity: ProductIdentity::default(),
            default_conf: Val::map_empty(),
            database_bootstrap: std::sync::Arc::new(NoopDatabaseBootstrap),
        }
    }
}

pub fn main() {
    main_with_providers(CliProviders::default());
}

pub fn main_with_identity(identity: ProductIdentity) {
    main_with_providers(CliProviders {
        identity,
        ..CliProviders::default()
    });
}

pub fn main_with_providers(providers: CliProviders) {
    // Install the global panic hook ASAP so any panic during startup is
    // captured into structured tracing instead of being lost to stderr.
    // See `hot::lang::user_code` for the full design.
    hot::lang::user_code::install_panic_hook();

    // Build a custom Tokio runtime with 64 MB thread stacks. The Hot VM is
    // deeply recursive and overflows the default ~8 MB stack on complex
    // workloads. This applies to both async worker threads and the blocking
    // thread pool used by spawn_blocking (where VM execution runs).
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_stack_size(64 * 1024 * 1024)
        .build()
        .expect("Failed to build Tokio runtime");

    runtime.block_on(async_main(providers));
}

/// Result of a failed migration or bootstrap step. Splits the primary error from any
/// user-facing hint (such as the Hot 1.x → Hot 2 recovery hint that
/// `translate_migrate_error` attaches), so the CLI can render the two parts cleanly
/// instead of stuffing a multi-line hint into a single tracing error line.
pub(crate) struct MigrationFailure {
    pub primary: String,
    pub hint: Option<String>,
}

impl MigrationFailure {
    fn from_message(msg: String) -> Self {
        // Convention: `translate_migrate_error` separates the primary error from its
        // recovery hint with a blank line. Anything else is a single-paragraph error.
        match msg.split_once("\n\n") {
            Some((primary, hint)) => Self {
                primary: primary.trim_end().to_string(),
                hint: Some(hint.trim().to_string()),
            },
            None => Self {
                primary: msg,
                hint: None,
            },
        }
    }
}

impl std::fmt::Display for MigrationFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.hint {
            Some(hint) => write!(f, "{}\n\n{}", self.primary, hint),
            None => f.write_str(&self.primary),
        }
    }
}

pub(crate) async fn run_migrations_with_bootstrap(
    conf: &Val,
    providers: &CliProviders,
) -> Result<(), MigrationFailure> {
    hot::db::run_migrations(conf)
        .await
        .map_err(|e| MigrationFailure::from_message(e.to_string()))?;
    providers
        .database_bootstrap
        .bootstrap(conf)
        .await
        .map_err(MigrationFailure::from_message)
}

/// Print a database migration failure to stderr in a layered way: the primary error
/// goes through tracing's `error!` (so it picks up timestamps/log formatting), and any
/// attached hint is printed as a separate, indented "hint:" block via `eprintln!` so
/// it does not collide with single-line log formatters.
pub(crate) fn report_migration_failure(prefix: &str, failure: &MigrationFailure) {
    tracing::error!("{}: {}", prefix, failure.primary);
    if let Some(hint) = &failure.hint {
        eprintln!();
        eprintln!("hint: {}", indent_continuation(hint, "      "));
        eprintln!();
    }
}

fn indent_continuation(text: &str, continuation_indent: &str) -> String {
    let mut lines = text.lines();
    let first = lines.next().unwrap_or("");
    let mut out = String::from(first);
    for line in lines {
        out.push('\n');
        out.push_str(continuation_indent);
        out.push_str(line);
    }
    out
}

async fn async_main(providers: CliProviders) {
    let cli = Cli::parse();

    // Handle shell completions generation early and exit
    if let Some(Command::Completions { shell, out_dir }) = &cli.command {
        let mut cmd = Cli::command();
        let bin_name = "hot";
        match out_dir {
            Some(dir) => {
                let _ = generate_to(*shell, &mut cmd, bin_name, dir);
            }
            None => {
                generate(*shell, &mut cmd, bin_name, &mut std::io::stdout());
            }
        }
        return;
    }

    // Handle version command early (before conf processing)
    if matches!(&cli.command, Some(Command::Version)) {
        let short_sha = &build_info::GIT_SHA[..7.min(build_info::GIT_SHA.len())];
        println!(
            "{} {} ({})",
            providers.identity.display_name,
            build_info::VERSION,
            short_sha
        );
        return;
    }

    // Handle help command early (before conf processing)
    if let Some(Command::Help { command }) = &cli.command {
        let mut cmd = Cli::command();
        if let Some(subcommand_name) = command {
            // Find the subcommand and print its help
            for sub in cmd.get_subcommands_mut() {
                if sub.get_name() == subcommand_name {
                    sub.print_help().unwrap();
                    println!();
                    return;
                }
            }
            // If subcommand not found, print error and main help
            eprintln!("error: Unknown command '{}'\n", subcommand_name);
            cmd.print_help().unwrap();
            println!();
            std::process::exit(1);
        } else {
            cmd.print_help().unwrap();
            // Show hidden commands when HOT_FIRE is set
            if std::env::var("HOT_FIRE").is_ok() {
                print!("{}", HIDDEN_COMMANDS_HELP);
            }
            println!();
        }
        return;
    }

    // Handle update command early (before conf processing)
    if let Some(Command::Update { force, version }) = &cli.command {
        let force = *force;
        let target_version = version.as_deref();
        match update::check_for_updates(target_version).await {
            update::UpdateCheckResult::TargetVersion {
                current_version,
                target_version: resolved_version,
                platform,
            } => {
                println!("Installing requested Hot version...");
                println!();
                println!("  Current version: {}", current_version);
                println!("  Target version:  {}", resolved_version);
                println!();
                println!("  Platform: {}", platform.description());
                println!("  Package:  {}", platform.package_name);
                println!();

                match update::run_installer(&platform, Some(&resolved_version)).await {
                    Ok(()) => {
                        println!();
                        println!("Hot updated successfully to version {}!", resolved_version);
                    }
                    Err(e) => {
                        eprintln!("Installation failed: {}", e);
                        eprintln!();
                        eprintln!("You can download manually from:");
                        eprintln!("  {}", platform.download_url);
                        std::process::exit(1);
                    }
                }
            }
            update::UpdateCheckResult::UpdateAvailable {
                current_version,
                latest_version,
                platform,
            } => {
                println!("Update available!");
                println!();
                println!("  Current version: {}", current_version);
                println!("  Latest version:  {}", latest_version);
                println!();
                println!("  Platform: {}", platform.description());
                println!("  Package:  {}", platform.package_name);
                println!();

                match update::run_installer(&platform, None).await {
                    Ok(()) => {
                        println!();
                        println!("Hot updated successfully to version {}!", latest_version);
                    }
                    Err(e) => {
                        eprintln!("Installation failed: {}", e);
                        eprintln!();
                        let install_method = update::detect_install_method();
                        if install_method == update::InstallMethod::Homebrew {
                            eprintln!("You can update manually with:");
                            eprintln!("  brew upgrade hot");
                        } else {
                            eprintln!("You can download manually from:");
                            eprintln!("  {}", platform.download_url);
                        }
                        std::process::exit(1);
                    }
                }
            }
            update::UpdateCheckResult::UpToDate {
                current_version,
                platform,
            } => {
                if force {
                    println!(
                        "Re-installing current version {} (--force)",
                        current_version
                    );
                    println!();
                    println!("  Platform: {}", platform.description());
                    println!("  Package:  {}", platform.package_name);
                    println!();

                    match update::run_installer(&platform, target_version).await {
                        Ok(()) => {
                            println!();
                            println!(
                                "Hot re-installed successfully (version {})!",
                                current_version
                            );
                        }
                        Err(e) => {
                            eprintln!("Installation failed: {}", e);
                            eprintln!();
                            let install_method = update::detect_install_method();
                            if install_method == update::InstallMethod::Homebrew
                                && target_version.is_none()
                            {
                                eprintln!("You can reinstall manually with:");
                                eprintln!("  brew reinstall hot");
                            } else {
                                eprintln!("You can download manually from:");
                                eprintln!("  {}", platform.download_url);
                            }
                            std::process::exit(1);
                        }
                    }
                } else {
                    println!("You're up to date! (version {})", current_version);
                }
            }
            update::UpdateCheckResult::UnsupportedPlatform { os, arch } => {
                eprintln!(
                    "Unable to determine download package for your platform: {} {}",
                    os, arch
                );
                eprintln!();
                eprintln!(
                    "Visit https://hot.dev/docs/getting-started/installation for manual installation options."
                );
                std::process::exit(1);
            }
            update::UpdateCheckResult::Disabled => {
                println!("Update checks are disabled for this installation.");
            }
            update::UpdateCheckResult::CheckFailed { error } => {
                eprintln!("Failed to check for updates: {}", error);
                std::process::exit(1);
            }
        }
        return;
    }

    // 0. Load .env files FIRST (before any env var processing)
    // This populates the process environment so apply_env_vars picks them up
    load_dotenv_files();

    // 1. Start with default configuration
    let mut conf = create_default_conf();
    conf = conf.merge(&providers.default_conf);

    // 2. Apply environment variables (includes vars from .env files)
    conf = apply_env_vars(conf);

    // 3. Apply conf file values (if provided) and command line arguments (highest priority)
    let (
        mut global_options,
        server_options,
        network_options,
        queue_options,
        worker_options,
        test_options,
        show_conf_options,
    ): ExtractedOptions = if let Some(ref command) = cli.command {
        extract_options_from_command(command)
    } else {
        // When no command is provided, use the global options from the CLI
        (cli.global.clone(), None, None, None, None, None, None)
    };

    // Always prioritize the main CLI's global options over subcommand global options
    // This ensures that top-level flags like --db-uri work correctly
    if cli.global.db_uri.is_some() {
        global_options.db_uri = cli.global.db_uri.clone();
    }

    // Check for hot.hot in current directory and merge with CLI options
    let mut all_conf_files = Vec::new();
    let in_project = Path::new("hot.hot").exists();

    // Determine if this is a runtime command (run/eval/repl) that should enable queue/emitter
    // Determine if this is a runtime command that should enable queue/emitter
    // - run/eval/repl: direct code execution
    // - dev: development server that runs code
    // - test: test execution
    // - worker: executes Hot code in response to events
    let is_runtime_command = matches!(
        &cli.command,
        Some(Command::Run { .. })
            | Some(Command::Eval { .. })
            | Some(Command::Repl { .. })
            | Some(Command::Dev { .. })
            | Some(Command::Test { .. })
            | Some(Command::Worker { .. })
            | Some(Command::TaskWorker { .. })
    );
    // Only enable queue/emitter defaults for runtime commands in a project
    let in_project_runtime = in_project && is_runtime_command;

    // First, check if hot.hot exists in current directory
    if in_project {
        all_conf_files.push("hot.hot".to_string());
    }

    // Then add any CLI-provided conf files
    all_conf_files.extend(global_options.conf_files.iter().cloned());

    // Load configurations if any exist
    if !all_conf_files.is_empty() {
        tracing::debug!("Loading configuration from files: {:?}", all_conf_files);

        // Get merged src paths: default from config + CLI overrides
        let merged_src_paths = get_merged_src_paths(
            &conf,
            global_options.project.as_deref(),
            &global_options.src_paths,
        );

        match load_conf(&all_conf_files, &merged_src_paths) {
            Ok(hot_config) => {
                tracing::debug!("Successfully loaded configuration files");
                // The hot.hot file creates the configuration structure directly
                // Merge it with the default configuration
                conf = conf.merge(&hot_config);

                // Check minimum Hot version requirement from hot.min-version
                if let Some(min_version_val) = conf.get("min-version")
                    && let hot::val::Val::Str(min_version) = min_version_val
                {
                    if let Err(e) = hot::build_info::check_min_version(&min_version) {
                        eprintln!("Version requirement not met: {}", e);
                        eprintln!(
                            "This project requires Hot {} or later.",
                            min_version.as_ref()
                        );
                        std::process::exit(1);
                    }
                    tracing::debug!("Project hot.min-version {} satisfied", min_version.as_ref());
                }
            }
            Err(e) => {
                // Display the full error message
                eprintln!("Configuration error: {}", e);
                std::process::exit(1);
            }
        }
    } else {
        tracing::debug!("No configuration files found, using default configuration");
    }

    // 3.5. Apply command-specific default overrides (after config files, before CLI args)
    conf = apply_command_specific_defaults(conf, &cli.command);

    // 4. Apply command line arguments (highest priority)
    conf = apply_configuration_options(
        conf,
        &global_options,
        server_options.as_ref(),
        network_options.as_ref(),
        queue_options.as_ref(),
        worker_options.as_ref(),
        test_options.as_ref(),
    );

    // 5. Resolve project configuration now that user configuration is loaded
    // This allows the project system to decide whether to create default projects
    let project_conf = hot::project::get_resolved_conf(conf.clone());

    // Merge project defaults into the existing default configuration
    let mut existing_default = conf.get("set").unwrap_or_else(Val::map_empty);
    let project_default = project_conf.get("set").unwrap_or_else(Val::map_empty);
    existing_default = existing_default.merge(&project_default);

    // Update the configuration with project data
    conf = conf.set("set", existing_default);
    conf = conf.set(
        "project",
        project_conf.get("project").unwrap_or_else(Val::map_empty),
    );

    // Propagate `--no-gitignore` to every discovery site (engine source
    // walks, bundler, formatter, LSP, docs, …) by setting it process-
    // globally. We do this *before* the resource-registry / codegen pass
    // below (which honors it directly) and before the rest of the
    // discovery-driven config resolution.
    hot::discovery::set_no_gitignore_global(global_options.no_gitignore);

    // Apply analyzer defaults (check/watch) and lsp defaults for runtime usage
    conf = hot::check::get_resolved_conf(conf);
    conf = hot::lsp::get_resolved_conf(conf);

    // Apply emitter defaults AFTER configuration files are loaded
    // This ensures the filter configuration is applied even if hot.hot doesn't specify it
    // For run/eval/repl in a project: type defaults to "db" (enables run tracking)
    //
    // Note: create_default_conf() doesn't set emitter.type, so the type here is only set if
    // the user explicitly provided it (via hot.hot or CLI). get_emitter_resolved_conf will
    // use the context-based default (db for in-project, none otherwise) if type is not set.
    let emitter_conf_from_user = conf.get("emitter").unwrap_or_else(Val::map_empty);
    let resolved_emitter_conf =
        get_emitter_resolved_conf(emitter_conf_from_user, in_project_runtime);
    conf = conf.set("emitter", resolved_emitter_conf);

    // Apply queue defaults AFTER configuration files are loaded
    // For run/eval/repl in a project: type defaults to "memory" (enables send())
    //
    // Note: Similar to emitter, create_default_conf() doesn't set queue.type, so the type
    // is only set if user explicitly provided it. get_resolved_conf will use context-based
    // default (memory for in-project, none otherwise) if type is not set.
    let queue_conf_from_user = conf.get("queue").unwrap_or_else(Val::map_empty);
    let resolved_queue_conf =
        hot::queue::get_resolved_conf(queue_conf_from_user, in_project_runtime);
    conf = conf.set("queue", resolved_queue_conf);

    // 6. Load context variables from hot/ctx.hot file(s)
    // This happens after conf loading so hot/ctx.hot can use conf settings if needed
    let mut all_ctx_files = Vec::new();

    // First, check if hot/ctx.hot exists in hot directory
    if Path::new("hot/ctx.hot").exists() {
        all_ctx_files.push("hot/ctx.hot".to_string());
    }

    // Then add any CLI-provided ctx files
    all_ctx_files.extend(global_options.ctx_files.iter().cloned());

    // Load context variables if any ctx files exist
    let context_storage: Option<ahash::AHashMap<String, hot::val::Val>> =
        if !all_ctx_files.is_empty() {
            tracing::debug!("Loading context from files: {:?}", all_ctx_files);
            match load_ctx(&all_ctx_files) {
                Ok(ctx_storage) => {
                    if !ctx_storage.is_empty() {
                        tracing::debug!("Loaded {} context variables", ctx_storage.len());
                        Some(ctx_storage)
                    } else {
                        tracing::debug!("Context file executed but no context variables were set");
                        None
                    }
                }
                Err(e) => {
                    eprintln!("Context loading error: {}", e);
                    std::process::exit(1);
                }
            }
        } else {
            None
        };

    // Note: Paths are now project-specific and handled through the project configuration system
    // Individual commands that need paths will call get_merged_*_paths() functions directly

    // If we're launching the LSP, default logging to none. The LSP uses stdio by
    // default, so stdout logging would corrupt the transport; explicit stdout/file
    // overrides are redirected to file for debuggability.
    let conf = if matches!(cli.command, Some(Command::Lsp { .. })) {
        let c = conf;
        let mut log_conf = c.get("log").unwrap_or_else(Val::map_empty);
        let requested_target = global_options.log_target.as_deref().map(str::to_lowercase);
        let target = match requested_target.as_deref() {
            Some("none") => "none",
            Some(_) => "file",
            None => "none",
        };
        if log_conf.get_str_or_default("target", "") != target {
            log_conf = log_conf.set_str("target", Some(target.to_string()), "");
        }
        c.set("log", log_conf)
    } else {
        conf
    };

    // For eval, run, and repl commands, default log level to "off" for cleaner output
    // unless the user explicitly set a log level via --log.level
    let conf = if matches!(
        cli.command,
        Some(Command::Eval { .. }) | Some(Command::Run { .. }) | Some(Command::Repl { .. })
    ) && global_options.log_level.is_none()
    {
        let mut log_conf = conf.get("log").unwrap_or_else(Val::map_empty);
        log_conf = log_conf.set_str("level", Some("off".to_string()), "off");
        conf.set("log", log_conf)
    } else {
        conf
    };

    // Setup logging with the configured level/target and command-appropriate format
    let log_format = get_log_format_for_command(&cli.command);
    hot::log::setup_tracing(&conf, log_format).unwrap();

    // Install the resource registry so that ::hot::resource/* bindings can
    // load resources declared via hot.project.<x>.resources.paths and any
    // --resource.path CLI flags, and run the skill-stub codegen against
    // any `*.skill.md` resources before the rest of the CLI proceeds to
    // compile/check/run the project. Done after `setup_tracing` so the
    // info/warn lines are visible; run before the daemon fork so the
    // daemon process inherits the same generated stubs (and re-runs the
    // same idempotent codegen anyway).
    {
        let resolved_project_name = global_options
            .project
            .clone()
            .unwrap_or_else(|| hot::project::get_default_project_name(&conf));
        let registry = hot::project::install_resource_registry(
            &conf,
            &resolved_project_name,
            &global_options.resource_paths,
            global_options.no_gitignore,
        );
        if !registry.entries.is_empty() {
            tracing::debug!(
                "Loaded {} resources for project '{}'",
                registry.entries.len(),
                resolved_project_name
            );
        }

        let report = hot::skill_codegen::run_skill_codegen_from_conf(
            &conf,
            &resolved_project_name,
            &global_options.resource_paths,
        );
        if report.any_changes() {
            tracing::info!("hot.codegen: {}", report.summary());
            for (path, err) in &report.errors {
                tracing::warn!("hot.codegen: skipping {} ({})", path.display(), err);
            }
        } else if !report.unchanged.is_empty() {
            tracing::debug!(
                "hot.codegen: {} skill stub(s) up to date",
                report.unchanged.len()
            );
        }
    }

    // Ensure daemon has a default before reading it
    let conf = if conf.get("daemon").is_none() {
        conf.set_bool("daemon", Some(false), false)
    } else {
        conf
    };

    // Check if we should show configuration and exit
    if let Some(show_conf) = &show_conf_options
        && show_conf.show_conf
    {
        show_command_config(&cli.command, &conf);
        return;
    }

    // When services need their configuration, call their get_resolved_conf with the full merged config
    // This allows each service to apply its defaults and use dotted paths to access values

    let daemon = conf.get_bool("daemon");

    // If daemon mode is enabled, fork to the background
    if daemon {
        // On Unix platforms, fork the process to run in the background
        #[cfg(unix)]
        {
            use std::process::Command;

            // Get the current executable path
            let exe = std::env::current_exe().expect("Failed to get executable path");

            // Build a command with the same arguments, but without the daemon flag
            let mut args: Vec<String> = std::env::args().collect();
            if let Some(pos) = args.iter().position(|arg| arg == "--daemon" || arg == "-d") {
                args.remove(pos);
            }
            args.remove(0); // Remove the program name

            // Create a new process that will run in the background
            let mut cmd = Command::new(exe);
            cmd.args(&args);

            match cmd.spawn() {
                Ok(_) => {
                    println!("Started in daemon mode");
                    return;
                }
                Err(e) => {
                    error!("Failed to start daemon: {}", e);
                    return;
                }
            }
        }

        // On non-Unix platforms, just show a message that daemon mode isn't supported
        #[cfg(not(unix))]
        {
            error!("Daemon mode is only supported on Unix platforms");
        }
    }

    match cli.command {
        Some(Command::Dev { dev, .. }) => {
            // CLI --open flag takes precedence, otherwise use config dev.open
            let open_browser = dev.open || conf.get_bool("dev.open");
            run_dev(
                conf.clone(),
                context_storage.clone(),
                global_options.ctx_files.clone(),
                open_browser,
                &providers,
            )
            .await
        }
        Some(Command::Api { .. }) => {
            enforce_redis_queue_for_standalone_service("api", &conf);
            run_api(Env::Production, conf.clone()).await
        }
        Some(Command::App { .. }) => {
            enforce_redis_queue_for_standalone_service("app", &conf);
            run_app(Env::Production, conf.clone(), None).await
        }
        Some(Command::Worker { .. }) => {
            enforce_redis_queue_for_standalone_service("worker", &conf);
            run_worker(Env::Production, conf.clone(), None).await
        }
        Some(Command::TaskWorker { .. }) => {
            enforce_redis_queue_for_standalone_service("task-worker", &conf);
            run_task_worker(conf.clone()).await
        }
        Some(Command::Scheduler { .. }) => {
            enforce_redis_queue_for_standalone_service("scheduler", &conf);
            run_scheduler(Env::Production, conf.clone()).await
        }
        Some(Command::Completions { .. }) => unreachable!(),
        Some(Command::Run {
            file, value_format, ..
        }) => {
            if let Err(e) = run_run(
                &file,
                &conf,
                &global_options,
                context_storage.clone(),
                value_format.as_deref(),
            )
            .await
            {
                eprintln!("Run failed: {}", e);
                std::process::exit(1);
            }
        }
        Some(Command::Eval {
            code, value_format, ..
        }) => {
            if let Err(e) = run_eval(
                &code,
                &conf,
                &global_options,
                context_storage.clone(),
                value_format.as_deref(),
            )
            .await
            {
                eprintln!("Eval failed: {}", e);
                std::process::exit(1);
            }
        }
        Some(Command::Repl { value_format, .. }) => {
            if let Err(e) = run_repl(
                &conf,
                &global_options,
                context_storage.clone(),
                value_format.as_deref(),
            )
            .await
            {
                error!("REPL failed: {}", e);
                std::process::exit(1);
            }
        }
        Some(Command::Test { test, pattern, .. }) => {
            // Use the capture setting with proper precedence: CLI option > project config > default
            let capture_output = test.capture.unwrap_or_else(|| {
                let project_name = hot::project::get_default_project_name(&conf);
                hot::project::get_project_test_capture(&conf, &project_name)
            });

            match run_test(
                pattern.as_deref(),
                capture_output,
                &conf,
                &global_options,
                context_storage.clone(),
                test.integration,
                &providers,
            )
            .await
            {
                Ok(exit_code) => {
                    std::process::exit(exit_code);
                }
                Err(e) => {
                    eprintln!("Test failed: {}", e);
                    std::process::exit(1);
                }
            }
        }
        // Cache command removed - will be reimplemented with  bytecode caching
        Some(Command::Deps { action, .. }) => {
            let project_name = global_options
                .project
                .as_deref()
                .map(|s| s.to_string())
                .unwrap_or_else(|| hot::project::get_default_project_name(&conf));

            if let Err(e) = run_deps(&action, &conf, &project_name).await {
                error!("Deps command failed: {}", e);
                std::process::exit(1);
            }
        }
        Some(Command::Conf { action, .. }) => {
            match action {
                Some(ConfAction::Generate { template, output }) => {
                    if let Err(e) = run_conf_generate(&template, &output, &conf) {
                        error!("Config generate failed: {}", e);
                        std::process::exit(1);
                    }
                }
                None => {
                    // Default: show resolved configuration
                    // Reconstruct display configuration to preserve top-sorted + alpha-sorted sections
                    let mut display_conf = Val::map_empty();

                    // Top-sorted sections
                    display_conf =
                        display_conf.set("set", conf.get("set").unwrap_or_else(Val::map_empty));
                    display_conf = display_conf.set(
                        "profile",
                        conf.get("profile").unwrap_or_else(Val::map_empty),
                    );
                    display_conf = display_conf.set(
                        "project",
                        conf.get("project").unwrap_or_else(Val::map_empty),
                    );

                    // Keep db, log, redis next (as seen in current output)
                    display_conf =
                        display_conf.set("db", conf.get("db").unwrap_or_else(Val::map_empty));
                    display_conf =
                        display_conf.set("log", conf.get("log").unwrap_or_else(Val::map_empty));
                    display_conf =
                        display_conf.set("redis", conf.get("redis").unwrap_or_else(Val::map_empty));

                    // Alpha-sorted service/config sections
                    let alpha_keys = [
                        "api",
                        "app",
                        "billing",
                        "build",
                        "cache",
                        "check",
                        "emitter",
                        "email",
                        "engine",
                        "file",
                        "lsp",
                        "product",
                        "queue",
                        "scheduler",
                        "serialization",
                        "watch",
                        "worker",
                    ];

                    // Defaults for analyzer if not present
                    let analyzer_defaults = hot::check::get_resolved_conf(Val::map_empty());

                    for key in alpha_keys.iter() {
                        let val = if let Some(v) = conf.get(key) {
                            v
                        } else if *key == "check" || *key == "watch" {
                            analyzer_defaults.get(key).unwrap_or_else(Val::map_empty)
                        } else {
                            Val::map_empty()
                        };
                        display_conf = display_conf.set(key, val);
                    }

                    // Other scalar flags that appear after services
                    // Preserve existing values
                    if let Some(v) = conf.get("daemon") {
                        display_conf = display_conf.set("daemon", v);
                    }
                    if let Some(v) = conf.get("deploy") {
                        display_conf = display_conf.set("deploy", v);
                    }

                    println!(
                        "::hot::conf ns\n\n// Hot Configuration\n{}\n",
                        display_conf.to_dot_separated_with_section_breaks("hot.")
                    );
                }
            }
        }
        Some(Command::Db { action, .. }) => {
            let db_cmd = match action {
                DbAction::Status => "status",
                DbAction::Migrate => "migrate",
                DbAction::PortV1ToV2 => "port-v1-to-v2",
            };

            if let Err(e) = run_db(db_cmd, &conf, &providers).await {
                error!("Database command failed: {}", e);
                std::process::exit(1);
            }
        }
        Some(Command::Cache { action, .. }) => {
            // Get the cache directory (project-local .hot/ or system cache)
            let cache_base = cache_paths::get_cache_base_dir();
            let cache_root = cache_paths::get_cache_root_dir();
            let has_project = cache_paths::has_project_config();

            match action {
                CacheAction::Clear => {
                    // All caches are now under .hot/cache/
                    if cache_root.exists() {
                        match std::fs::remove_dir_all(&cache_root) {
                            Ok(_) => {
                                println!("{} cleared", cache_root.display());
                            }
                            Err(e) => {
                                error!("Failed to clear {}: {}", cache_root.display(), e);
                            }
                        }
                    } else {
                        println!("No caches found to clear.");
                    }
                }
                CacheAction::Status => {
                    // Cache subdirectories under .hot/cache/
                    let cache_subdirs = [
                        ("bytecode", "Compiled bytecode"),
                        ("unit", "Unit AST"),
                        ("cdn", "CDN packages"),
                        ("git", "Git dependencies"),
                        ("docs", "Documentation"),
                    ];
                    let mut total_size: u64 = 0;

                    // Show cache location
                    let location_type = if has_project { "Project" } else { "System" };
                    println!("{} Cache ({}):", location_type, cache_base.display());
                    println!("{:-<60}", "");
                    println!("{:20} {:>15} Description", "Directory", "Size");
                    println!("{:-<60}", "");

                    for (dir_name, description) in cache_subdirs {
                        let cache_path = cache_root.join(dir_name);
                        if cache_path.exists() {
                            let size = dir_size(&cache_path);
                            total_size += size;
                            println!(
                                "{:20} {:>15} {}",
                                format!("cache/{}", dir_name),
                                format_size(size),
                                description
                            );
                        } else {
                            println!(
                                "{:20} {:>15} {}",
                                format!("cache/{}", dir_name),
                                "(empty)",
                                description
                            );
                        }
                    }

                    println!("{:-<60}", "");
                    println!("{:20} {:>15}", "Total", format_size(total_size));
                }
            }
        }
        Some(Command::Queue { action, .. }) => {
            if let Err(e) = run_queue(&action, &conf).await {
                error!("Queue command failed: {}", e);
                std::process::exit(1);
            }
        }
        Some(Command::Context {
            global,
            action,
            local,
            ..
        }) => {
            if let Err(e) = run_context(&action, &conf, &global, local).await {
                eprintln!("Error: {}", e);
                std::process::exit(1);
            }
        }
        Some(Command::Init { path, .. }) => {
            if let Err(e) = run_init(&conf, path.as_deref(), &providers).await {
                error!("Init command failed: {}", e);
                std::process::exit(1);
            }
        }
        Some(Command::Ai { action }) => {
            if let Err(e) = run_ai(&action) {
                error!("AI command failed: {}", e);
                std::process::exit(1);
            }
        }
        Some(Command::Build {
            build_dir,
            global,
            allow_secret_shape,
            ..
        }) => {
            // Use the project name as the bundle name
            let bundle_name = global
                .project
                .as_deref()
                .map(|s| s.to_string())
                .unwrap_or_else(|| hot::project::get_default_project_name(&conf));

            // Get paths using the merged path functions
            let cmd_src_paths =
                get_merged_src_paths(&conf, global.project.as_deref(), &global.src_paths);

            if let Err(e) = run_build(
                &bundle_name,
                &cmd_src_paths,
                build_dir.as_deref(),
                &conf,
                &global.resource_paths,
                global.no_gitignore,
                allow_secret_shape,
            )
            .await
            {
                error!("Build command failed: {}", e);
                std::process::exit(1);
            }
        }
        Some(Command::Builds {
            global,
            limit,
            offset,
            local,
            ..
        }) => {
            if let Err(e) = run_builds(global.project.as_deref(), limit, offset, &conf, local).await
            {
                eprintln!("Error: {}", e);
                std::process::exit(1);
            }
        }
        Some(Command::Projects {
            limit,
            offset,
            local,
            ..
        }) => {
            if let Err(e) = run_projects(limit, offset, &conf, local).await {
                eprintln!("Error: {}", e);
                std::process::exit(1);
            }
        }
        Some(Command::Project { action, local, .. }) => {
            if let Err(e) = run_project_action(&action, &conf, local).await {
                eprintln!("Error: {}", e);
                std::process::exit(1);
            }
        }
        Some(Command::Extract {
            build_path,
            extract_dir,
            build_dir,
            ..
        }) => {
            if let Err(e) = run_extract(&build_path, extract_dir.as_deref(), build_dir.as_deref()) {
                error!("Extract command failed: {}", e);
                std::process::exit(1);
            }
        }
        Some(Command::Compile {
            global,
            project_name,
            ..
        }) => {
            // Use the project name from CLI or default from config
            let project_name = project_name
                .as_deref()
                .map(|s| s.to_string())
                .unwrap_or_else(|| hot::project::get_default_project_name(&conf));

            // Get paths using the merged path functions
            let cmd_src_paths =
                get_merged_src_paths(&conf, global.project.as_deref(), &global.src_paths);
            let include_tests = global.with_tests.unwrap_or_else(|| {
                conf.get_bool("check.with-tests") || conf.get_bool("watch.with-tests")
            });
            let cmd_test_paths = if include_tests {
                get_merged_test_paths(&conf, global.project.as_deref(), &global.test_paths)
            } else {
                Vec::new()
            };

            if let Err(e) = run_compile(
                &project_name,
                &cmd_src_paths,
                &cmd_test_paths,
                &conf,
                &global_options,
            )
            .await
            {
                error!("Compile command failed: {}", e);
                std::process::exit(1);
            }
        }
        Some(Command::Check {
            global,
            format,
            raw,
            deny_warnings,
            path,
            ..
        }) => {
            let effective_format = format.or_else(|| {
                let f = conf.get_str("check.format");
                if f.is_empty() { None } else { Some(f) }
            });
            let effective_raw = if raw {
                true
            } else {
                conf.get_bool("check.raw")
            };
            let effective_deny_warnings =
                deny_warnings || conf.get_bool_or_default("check.deny-warnings", false);
            match run_check_with_raw(
                effective_format.as_deref(),
                effective_raw,
                effective_deny_warnings,
                &conf,
                &global,
                context_storage.clone(),
                path.as_deref(),
            )
            .await
            {
                Ok(exit_code) => std::process::exit(exit_code),
                Err(e) => {
                    error!("Check failed: {}", e);
                    std::process::exit(2);
                }
            }
        }
        Some(Command::Watch {
            global,
            format,
            raw,
            deny_warnings,
            watch_debounce_ms,
            ..
        }) => {
            let effective_format = format.or_else(|| {
                let f = conf.get_str("check.format");
                if f.is_empty() { None } else { Some(f) }
            });
            let effective_raw = if raw {
                true
            } else {
                conf.get_bool("check.raw")
            };
            let effective_deny_warnings =
                deny_warnings || conf.get_bool_or_default("check.deny-warnings", false);
            let effective_debounce =
                watch_debounce_ms.unwrap_or_else(|| conf.get_int("watch.debounce") as u64);
            match run_check_watch(
                effective_format.as_deref(),
                effective_raw,
                effective_deny_warnings,
                effective_debounce,
                &conf,
                &global,
            )
            .await
            {
                Ok(exit_code) => std::process::exit(exit_code),
                Err(e) => {
                    error!("Watch failed: {}", e);
                    std::process::exit(2);
                }
            }
        }

        Some(Command::Deploy {
            build_id,
            global,
            local,
            allow_secret_shape,
            strict,
            ..
        }) => {
            if let Err(e) = run_deploy(
                build_id.as_deref(),
                &conf,
                &global,
                local,
                allow_secret_shape,
                strict,
            )
            .await
            {
                eprintln!("Error: {}", e);
                std::process::exit(1);
            }
        }
        Some(Command::Upload { build_id, .. }) => {
            if let Err(e) = run_upload(&build_id, &conf).await {
                eprintln!("Error: {}", e);
                std::process::exit(1);
            }
        }
        Some(Command::Fmt {
            global,
            file,
            force,
            check,
        }) => {
            let exit_code = match run_fmt(&global, file.as_deref(), force, check).await {
                Ok(code) => code,
                Err(e) => {
                    error!("Fmt failed: {}", e);
                    1
                }
            };
            if exit_code != 0 {
                std::process::exit(exit_code);
            }
        }
        Some(Command::Lsp {
            transport, stdio, ..
        }) => {
            // If --stdio was passed, force stdio transport
            let t = if stdio {
                String::from("stdio")
            } else {
                transport.unwrap_or_else(|| conf.get_str("lsp.transport"))
            };
            match t.as_str() {
                "stdio" | "" => {
                    if let Err(e) = hot_lsp::run_stdio().await {
                        error!("LSP failed: {}", e);
                        std::process::exit(2);
                    }
                }
                other => {
                    error!("Unsupported lsp.transport: {}", other);
                    std::process::exit(2);
                }
            }
        }
        Some(Command::Docs {
            packages,
            all,
            out_dir,
            ..
        }) => {
            let out_dir = out_dir.unwrap_or_else(|| "resources/pkg-docs".to_string());

            // Determine which packages to generate docs for
            let packages_to_generate: Vec<String> = if all {
                // Find all packages in hot/pkg directory
                let pkg_dir = PathBuf::from("./hot/pkg");
                if pkg_dir.exists() {
                    fs::read_dir(&pkg_dir)
                        .map(|entries| {
                            entries
                                .filter_map(|e| e.ok())
                                .filter(|e| e.path().is_dir())
                                .filter(|e| e.path().join("pkg.hot").exists())
                                .filter_map(|e| e.file_name().into_string().ok())
                                .collect()
                        })
                        .unwrap_or_else(|_| vec!["hot-std".to_string()])
                } else {
                    vec!["hot-std".to_string()]
                }
            } else if packages.is_empty() {
                // Default to hot-std if no packages specified
                vec!["hot-std".to_string()]
            } else {
                packages
            };

            let out_path = PathBuf::from(&out_dir);

            // Create output directory if it doesn't exist
            if let Err(e) = fs::create_dir_all(&out_path) {
                error!("Failed to create output directory: {}", e);
                std::process::exit(1);
            }

            // Convert to &str slice for the context function (all packages being generated are served)
            let served_packages: Vec<&str> =
                packages_to_generate.iter().map(|s| s.as_str()).collect();

            // Generate docs in parallel using rayon
            use rayon::prelude::*;
            use std::sync::atomic::{AtomicUsize, Ordering};

            let success_count = AtomicUsize::new(0);
            let error_count = AtomicUsize::new(0);

            packages_to_generate.par_iter().for_each(|pkg_name| {
                info!("Generating versioned docs for {}", pkg_name);

                match hot::pkg::docs::generate_versioned_pkg_docs_with_context(
                    pkg_name,
                    &out_path,
                    &served_packages,
                ) {
                    Ok(version) => {
                        println!("✓ Generated docs for {}@{}", pkg_name, version);
                        success_count.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(e) => {
                        error!("Failed to generate docs for {}: {}", pkg_name, e);
                        error_count.fetch_add(1, Ordering::Relaxed);
                    }
                }
            });

            let success_count = success_count.load(Ordering::Relaxed);
            let error_count = error_count.load(Ordering::Relaxed);

            println!(
                "\nDocs generation complete: {} succeeded, {} failed",
                success_count, error_count
            );

            if error_count > 0 {
                std::process::exit(1);
            }
        }
        Some(Command::Version) | Some(Command::Update { .. }) | Some(Command::Help { .. }) => {
            // Handled early, before config processing
        }
        None => {
            // No command provided - check if stdin is a terminal or has piped input
            use is_terminal::IsTerminal;
            use std::io::{self, Read};

            if io::stdin().is_terminal() {
                // Interactive mode - show help instead of hanging
                Cli::command().print_help().unwrap();
                // Show hidden commands when HOT_FIRE is set
                if std::env::var("HOT_FIRE").is_ok() {
                    print!("{}", HIDDEN_COMMANDS_HELP);
                }
                println!();
                return;
            }

            // Not a terminal (piped input) - read from stdin
            let mut stdin_input = String::new();
            if let Err(e) = io::stdin().read_to_string(&mut stdin_input) {
                error!("Failed to read from stdin: {}", e);
                std::process::exit(1);
            }

            // Check if stdin is empty
            if stdin_input.trim().is_empty() {
                error!("No input provided from stdin. Use 'hot help' for usage information.");
                std::process::exit(1);
            }

            if let Err(e) = run_eval(
                stdin_input.trim(),
                &conf,
                &global_options,
                context_storage.clone(),
                None, // No value_format override for stdin
            )
            .await
            {
                error!("Eval failed: {}", e);
                std::process::exit(1);
            }
        }
    }
}

// fn find_hot_files(dir: &str) -> Result<Vec<std::path::PathBuf>, String> {
//     let mut files = Vec::new();
//     let path = std::path::Path::new(dir);
//
//     if !path.exists() {
//         return Ok(files); // Skip non-existent paths
//     }
//
//     visit_dir_for_hot_files(path, &mut files)?;
//     Ok(files)
// }
//
// fn visit_dir_for_hot_files(
//     dir: &std::path::Path,
//     files: &mut Vec<std::path::PathBuf>,
// ) -> Result<(), String> {
//     if dir.is_dir() {
//         let entries = std::fs::read_dir(dir)
//             .map_err(|e| format!("Failed to read directory {}: {}", dir.display(), e))?;
//
//         for entry in entries {
//             let entry = entry.map_err(|e| format!("Failed to read directory entry: {}", e))?;
//             let path = entry.path();
//
//             if path.is_dir() {
//                 // Skip hidden directories and common build/cache directories
//                 if let Some(name) = path.file_name().and_then(|n| n.to_str())
//                     && (name.starts_with('.') || name == "target" || name == "node_modules")
//                 {
//                     continue;
//                 }
//                 visit_dir_for_hot_files(&path, files)?;
//             } else if path.extension().and_then(|e| e.to_str()) == Some("hot") {
//                 files.push(path);
//             }
//         }
//     } else if dir.extension().and_then(|e| e.to_str()) == Some("hot") {
//         files.push(dir.to_path_buf());
//     }
//     Ok(())
// }

/// Calculate the total size of a directory recursively
fn dir_size(path: &std::path::Path) -> u64 {
    let mut size = 0;
    if path.is_dir()
        && let Ok(entries) = std::fs::read_dir(path)
    {
        for entry in entries.flatten() {
            let entry_path = entry.path();
            if entry_path.is_dir() {
                size += dir_size(&entry_path);
            } else if let Ok(metadata) = entry.metadata() {
                size += metadata.len();
            }
        }
    }
    size
}

/// Format a byte size in human-readable format
fn format_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;

    if bytes >= GB {
        format!("{:.2} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.2} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.2} KB", bytes as f64 / KB as f64)
    } else {
        format!("{} B", bytes)
    }
}

#[cfg(test)]
mod migration_failure_tests {
    use super::*;

    #[test]
    fn splits_primary_and_hint_on_blank_line() {
        let f = MigrationFailure::from_message(
            "Migration error: migration 2 was previously applied but is missing\n\n\
             Hot 2 detected a Hot 1.x SQLite database. Run `hot db port-v1-to-v2` to ..."
                .to_string(),
        );
        assert_eq!(
            f.primary,
            "Migration error: migration 2 was previously applied but is missing"
        );
        assert_eq!(
            f.hint.as_deref(),
            Some("Hot 2 detected a Hot 1.x SQLite database. Run `hot db port-v1-to-v2` to ...")
        );
    }

    #[test]
    fn no_hint_when_no_blank_line() {
        let f = MigrationFailure::from_message("connection refused".to_string());
        assert_eq!(f.primary, "connection refused");
        assert!(f.hint.is_none());
    }

    #[test]
    fn display_round_trips_with_hint() {
        let f = MigrationFailure::from_message("primary\n\nhint line".to_string());
        assert_eq!(f.to_string(), "primary\n\nhint line");
    }

    #[test]
    fn indent_continuation_pads_subsequent_lines() {
        let s = indent_continuation("first\nsecond\nthird", "  ");
        assert_eq!(s, "first\n  second\n  third");
    }

    #[test]
    fn indent_continuation_single_line_unchanged() {
        let s = indent_continuation("only", "      ");
        assert_eq!(s, "only");
    }
}

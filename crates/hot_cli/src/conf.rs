//! Configuration pipeline: defaults, env-var overlay, file loading,
//! command-specific overrides, ctx loading, src/test path merging, and the
//! emitter/event-publisher constructors used during runtime startup.

use std::env;
use std::fs;
use std::path::Path;
use std::str::FromStr;

use hot::data::serialization::Serialization;
use hot::lang::emitter::EngineEventEmitter;
use hot::queue::QueueType;
use hot::val;
use hot::val::Val;

use crate::cli::{
    Command, EmitterOptions, GlobalOptions, NetworkOptions, QueueOptions, ServerOptions,
    ShowConfOptions, TestOptions, WorkerOptions,
};

/// Tuple returned by [`extract_options_from_command`] containing every
/// per-subcommand option group used downstream by the dispatch logic.
pub(crate) type ExtractedOptions = (
    GlobalOptions,
    Option<ServerOptions>,
    Option<NetworkOptions>,
    Option<QueueOptions>,
    Option<WorkerOptions>,
    Option<TestOptions>,
    Option<ShowConfOptions>,
);

fn get_engine_resolved_conf(conf: Val) -> Val {
    // Start with defaults
    let default_conf = hot::val!({
        "threads": 4i64
    });

    // Merge with provided conf (the provided conf will override defaults)
    default_conf.merge(&conf)
}

/// Emitter configuration with defaults - delegates to core function
pub(crate) fn get_emitter_resolved_conf(conf: Val, in_project: bool) -> Val {
    hot::lang::emitter::get_resolved_conf(conf, in_project)
}

// Helper function to convert environment variable name to dot notation conf key
fn env_var_to_conf_key(env_var: &str) -> String {
    env_var
        .strip_prefix("HOT_")
        .unwrap_or(env_var)
        .to_lowercase()
        .replace('_', ".")
}

pub(crate) fn create_default_conf() -> Val {
    // Create app and api default configurations using their server modules
    let profile_conf = hot::profile::get_resolved_conf(Val::map_empty());
    let app_conf = hot_app::server::get_resolved_conf(Val::map_empty());
    let api_conf = hot_api::server::get_resolved_conf(Val::map_empty());
    let worker_conf = hot_worker::server::get_resolved_conf(Val::map_empty());
    let scheduler_conf = hot_scheduler::server::get_resolved_conf(Val::map_empty());
    let log_conf = hot::log::get_resolved_conf(Val::map_empty());
    let engine_conf = get_engine_resolved_conf(Val::map_empty());
    let db_conf = hot::db::get_resolved_conf(Val::map_empty());
    let product_conf = hot::product::get_resolved_conf(Val::map_empty());
    let deploy_conf = hot::deploy::get_resolved_conf(Val::map_empty());
    // Set emitter and queue type to "" (empty string) as sentinel for "not yet set".
    // The actual type will be resolved later based on whether we're in a project.
    // Empty string allows distinguishing "not set" from user explicitly setting "none".
    let emitter_conf = val!({
        "type": "",  // Sentinel: will be resolved to "db" (in-project) or "none" (outside)
        "filter": {
            "var": {
                "ns": {
                    "exclude": [".*"],
                    "include": []
                },
                "meta": {
                    "exclude": [],
                    "include": []
                },
                "value": {
                    "exclude": [],
                    "include": []
                }
            }
        }
    });
    // Queue type "" is sentinel for "not yet set" - will be resolved later
    let queue_conf = val!({
        "type": ""  // Sentinel: will be resolved to "memory" (in-project) or "none" (outside)
    });
    let redis_conf = hot::redis::get_resolved_conf(Val::map_empty());
    let serialization_conf = hot::data::serialization::get_resolved_conf(Val::map_empty());
    let domain_conf = hot::domain::get_resolved_conf(Val::map_empty());
    let box_conf = val!({
        "enabled": true,
        "backend": "docker"
    });
    let task_conf = val!({
        "max-concurrent": 4i64,
        "code-max-concurrent": 500i64,
        "worker-memory-mb": 8192i64,
        "worker-disk-mb": 51200i64
    });
    // Cache configuration removed - will be reimplemented with  bytecode caching

    // Merge profile defaults only (project defaults will be added later)
    let mut default_conf = profile_conf.get("set").unwrap_or_else(Val::map_empty);
    // Add default remote selection
    default_conf = default_conf.set_str("remote", Some("hot-dev".to_string()), "");

    val!({
        "set": default_conf,
        "profile": profile_conf.get("profile").unwrap_or_else(Val::map_empty),
        "product": product_conf,
        // Note: project configuration will be resolved later after user config is loaded
        "api": api_conf,
        "remote": {
            // Remote API configuration for deployment
            "hot-dev": {
                "url": "https://api.hot.dev",
                "key": ""
            }
        },
        "app": app_conf,
        "box": box_conf,
        "task": task_conf,
        // "cache": Bytecode caching config can be reintroduced when needed.
        "db": db_conf,
        "dev": {
            "open": false  // Whether to open browser on `hot dev`
        },
        "deploy": deploy_conf,
        "build": {
            "file": {
                "max-bytes": 104857600i64  // 100MB default for build file uploads
            }
        },
        "file": {
            "max-bytes": -1i64  // -1 = no config ceiling, defer to plan-based limits
        },
        "emitter": emitter_conf,
        "engine": engine_conf,
        "log": log_conf,
        "queue": queue_conf,
        "redis": redis_conf,
        "scheduler": scheduler_conf,
        "serialization": serialization_conf,
        "domain": domain_conf,
        "worker": worker_conf,
        "daemon": false
    })
}

pub(crate) fn load_dotenv_files() {
    if Path::new(".env").exists() {
        match dotenvy::from_path(Path::new(".env")) {
            Ok(_) => tracing::debug!("Loaded environment from .env"),
            Err(e) => tracing::warn!("Failed to load .env: {}", e),
        }
    }
}

// Function to load environment variables and apply them to the configuration
pub(crate) fn apply_env_vars(conf: Val) -> Val {
    let mut conf = conf;

    // Get all environment variables starting with "HOT_"
    for (key, value) in env::vars() {
        if key.starts_with("HOT_") {
            let conf_key = env_var_to_conf_key(&key);
            // Create a new Val with the path for this env var
            let new_val = val!({ conf_key.as_str(): value.as_str() });
            conf = conf.merge(&new_val);
        }
    }

    conf
}

// Function to extract options from the command enum
pub(crate) fn extract_options_from_command(command: &Command) -> ExtractedOptions {
    match command {
        Command::Dev {
            global,
            server,
            network,
            queue,
            worker,
            scheduler: _,
            dev: _,
            show_conf,
        } => (
            global.clone(),
            Some(server.clone()),
            Some(network.clone()),
            Some(queue.clone()),
            Some(worker.clone()),
            None,
            Some(show_conf.clone()),
        ),
        Command::Api {
            global,
            server,
            network,
            queue,
            show_conf,
        } => (
            global.clone(),
            Some(server.clone()),
            Some(network.clone()),
            Some(queue.clone()),
            None,
            None,
            Some(show_conf.clone()),
        ),
        Command::App {
            global,
            server,
            network,
            show_conf,
        } => (
            global.clone(),
            Some(server.clone()),
            Some(network.clone()),
            None,
            None,
            None,
            Some(show_conf.clone()),
        ),
        Command::Worker {
            global,
            server,
            queue,
            worker,
            show_conf,
        } => (
            global.clone(),
            Some(server.clone()),
            None,
            Some(queue.clone()),
            Some(worker.clone()),
            None,
            Some(show_conf.clone()),
        ),
        Command::TaskWorker {
            global,
            queue,
            show_conf,
        } => (
            global.clone(),
            None,
            None,
            Some(queue.clone()),
            None,
            None,
            Some(show_conf.clone()),
        ),
        Command::Scheduler {
            global,
            server,
            queue,
            scheduler: _,
            show_conf,
        } => (
            global.clone(),
            Some(server.clone()),
            None,
            Some(queue.clone()),
            None,
            None,
            Some(show_conf.clone()),
        ),
        Command::Run {
            global, show_conf, ..
        } => (
            global.clone(),
            None,
            None,
            None,
            None,
            None,
            Some(show_conf.clone()),
        ),
        Command::Eval {
            global, show_conf, ..
        } => (
            global.clone(),
            None,
            None,
            None,
            None,
            None,
            Some(show_conf.clone()),
        ),
        Command::Repl {
            global, show_conf, ..
        } => (
            global.clone(),
            None,
            None,
            None,
            None,
            None,
            Some(show_conf.clone()),
        ),
        Command::Test {
            global,
            test,
            show_conf,
            ..
        } => (
            global.clone(),
            None,
            None,
            None,
            None,
            Some(test.clone()),
            Some(show_conf.clone()),
        ),
        // Cache command removed
        Command::Conf {
            global,
            server,
            network,
            queue,
            worker,
            test,
            ..
        } => (
            global.clone(),
            Some(server.clone()),
            Some(network.clone()),
            Some(queue.clone()),
            Some(worker.clone()),
            Some(test.clone()),
            None,
        ),
        Command::Db {
            global, show_conf, ..
        } => (
            global.clone(),
            None,
            None,
            None,
            None,
            None,
            Some(show_conf.clone()),
        ),
        Command::Cache { global, .. } => (global.clone(), None, None, None, None, None, None),
        Command::Init {
            global, show_conf, ..
        } => (
            global.clone(),
            None,
            None,
            None,
            None,
            None,
            Some(show_conf.clone()),
        ),
        Command::Ai { .. } => (GlobalOptions::default(), None, None, None, None, None, None),
        Command::Build {
            global, show_conf, ..
        } => (
            global.clone(),
            None,
            None,
            None,
            None,
            None,
            Some(show_conf.clone()),
        ),
        Command::Builds {
            global, show_conf, ..
        } => (
            global.clone(),
            None,
            None,
            None,
            None,
            None,
            Some(show_conf.clone()),
        ),
        Command::Projects {
            global, show_conf, ..
        } => (
            global.clone(),
            None,
            None,
            None,
            None,
            None,
            Some(show_conf.clone()),
        ),
        Command::Project {
            global, show_conf, ..
        } => (
            global.clone(),
            None,
            None,
            None,
            None,
            None,
            Some(show_conf.clone()),
        ),
        Command::Extract {
            global, show_conf, ..
        } => (
            global.clone(),
            None,
            None,
            None,
            None,
            None,
            Some(show_conf.clone()),
        ),
        Command::Compile {
            global, show_conf, ..
        } => (
            global.clone(),
            None,
            None,
            None,
            None,
            None,
            Some(show_conf.clone()),
        ),
        Command::Check {
            global, show_conf, ..
        } => (
            global.clone(),
            None,
            None,
            None,
            None,
            None,
            Some(show_conf.clone()),
        ),
        Command::Watch {
            global, show_conf, ..
        } => (
            global.clone(),
            None,
            None,
            None,
            None,
            None,
            Some(show_conf.clone()),
        ),
        Command::Deploy {
            global, show_conf, ..
        } => (
            global.clone(),
            None,
            None,
            None,
            None,
            None,
            Some(show_conf.clone()),
        ),
        Command::Upload {
            global, show_conf, ..
        } => (
            global.clone(),
            None,
            None,
            None,
            None,
            None,
            Some(show_conf.clone()),
        ),
        Command::Lsp {
            global, show_conf, ..
        } => (
            global.clone(),
            None,
            None,
            None,
            None,
            None,
            Some(show_conf.clone()),
        ),
        Command::Completions { .. } => (
            GlobalOptions {
                conf_files: vec![],
                ctx_files: vec![],
                project: None,
                src_paths: vec![],

                test_paths: vec![],
                resource_paths: vec![],
                no_gitignore: false,
                // Cache options removed
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
                deploy_auto: false,
                emitter: EmitterOptions { emitter_type: None },
                with_tests: None,
            },
            None,
            None,
            None,
            None,
            None,
            None,
        ),
        Command::Fmt { global, .. } => (global.clone(), None, None, None, None, None, None),
        Command::Deps {
            global, show_conf, ..
        } => (
            global.clone(),
            None,
            None,
            None,
            None,
            None,
            Some(show_conf.clone()),
        ),
        Command::Context {
            global, show_conf, ..
        } => (
            global.clone(),
            None,
            None,
            None,
            None,
            None,
            Some(show_conf.clone()),
        ),
        Command::Docs { global, .. } => (global.clone(), None, None, None, None, None, None),
        Command::Queue { global, queue, .. } => (
            global.clone(),
            None,
            None,
            Some(queue.clone()),
            None,
            None,
            None,
        ),
        Command::Version | Command::Update { .. } | Command::Help { .. } => (
            GlobalOptions {
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
                deploy_auto: false,
                emitter: EmitterOptions { emitter_type: None },
                with_tests: None,
            },
            None,
            None,
            None,
            None,
            None,
            None,
        ),
    }
}

// Apply configuration options from different argument groups
#[allow(clippy::too_many_arguments)]
pub(crate) fn apply_configuration_options(
    mut conf: Val,
    global: &GlobalOptions,
    server: Option<&ServerOptions>,
    network: Option<&NetworkOptions>,
    queue: Option<&QueueOptions>,
    worker: Option<&WorkerOptions>,
    test: Option<&TestOptions>,
) -> Val {
    // Apply global options
    if let Some(engine_threads) = global.engine_threads {
        conf = conf.set_int("engine.threads", Some(engine_threads as i64), 0);
    }
    if let Some(ref mode) = global.jit_mode {
        conf = conf.set_str("jit.mode", Some(mode.clone()), "");
    }
    if let Some(threshold) = global.jit_threshold {
        conf = conf.set_int("jit.threshold", Some(threshold as i64), 0);
    }
    if let Some(ref db_uri) = global.db_uri {
        conf = conf.set_str("db.uri", Some(db_uri.clone()), "");
    }
    // Cache configuration removed - will be reimplemented with  bytecode caching

    // Apply global logging options
    if let Some(ref level) = global.log_level {
        conf = conf.set_str("log.level", Some(level.clone()), "");
    }
    if let Some(ref target) = global.log_target {
        conf = conf.set_str("log.target", Some(target.clone()), "");
    }
    if let Some(ref dir) = global.log_dir {
        conf = conf.set_str("log.dir", Some(dir.clone()), "");
    }
    if let Some(ref rotation) = global.log_rotation {
        conf = conf.set_str("log.rotation", Some(rotation.clone()), "");
    }
    if let Some(retention) = global.log_retention {
        conf = conf.set_int("log.retention", Some(retention), 0);
    }
    if let Some(ref format) = global.log_format {
        conf = conf.set_str("log.format", Some(format.clone()), "");
    }

    // Apply global deploy options
    conf = conf.set_bool("deploy.auto", Some(global.deploy_auto), true);

    // Apply emitter options
    if let Some(ref emitter_type) = global.emitter.emitter_type {
        conf = conf.set_str("emitter.type", Some(emitter_type.clone()), "");
    }
    // Apply server options
    if let Some(server) = server {
        conf = conf.set_bool("daemon", Some(server.daemon), false);
    }

    // Apply network options
    if let Some(network) = network {
        if let Some(ref host) = network.api_host {
            conf = conf.set_str("api.host", Some(host.clone()), "");
        }
        if let Some(port) = network.api_port {
            conf = conf.set_int("api.port", Some(port as i64), 0);
        }
        if let Some(ref host) = network.app_host {
            conf = conf.set_str("app.host", Some(host.clone()), "");
        }
        if let Some(port) = network.app_port {
            conf = conf.set_int("app.port", Some(port as i64), 0);
        }
    }

    // Apply queue options
    if let Some(queue) = queue {
        if let Some(ref queue_type) = queue.queue_type {
            conf = conf.set_str("queue.type", Some(queue_type.clone()), "");
        }
        if let Some(ref redis_uri) = queue.redis_uri {
            conf = conf.set_str("redis.uri", Some(redis_uri.clone()), "");
        }
        if let Some(ref serialization) = queue.serialization {
            conf = conf.set_str("serialization.type", Some(serialization.clone()), "");
        }
    }

    // Apply worker options
    if let Some(worker) = worker
        && let Some(threads) = worker.worker_threads
    {
        conf = conf.set_int("worker.threads", Some(threads as i64), 0);
    }

    // Apply test options
    if let Some(test) = test
        && let Some(capture) = test.capture
    {
        conf = conf.set_bool("test.capture", Some(capture), true);
    }

    conf
}

// Function to load and parse configuration file using engine
pub(crate) fn load_conf(
    conf_paths: &[String],
    _src_paths: &[String],
) -> Result<hot::val::Val, String> {
    // Combine all configuration files into a single code string
    let mut combined_content = String::new();

    for conf_path in conf_paths {
        // Check if file exists
        let path = Path::new(conf_path);
        if !path.exists() {
            return Err(format!("Configuration file not found: {}", conf_path));
        }

        // Read file content
        let content = match fs::read_to_string(path) {
            Ok(content) => content,
            Err(e) => {
                return Err(format!(
                    "Failed to read configuration file {}: {}",
                    conf_path, e
                ));
            }
        };

        // Add content with a newline separator
        if !combined_content.is_empty() {
            combined_content.push('\n');
        }
        combined_content.push_str(&content);
    }

    // Use engine with minimal dependencies for configuration loading
    // This avoids loading all packages but still provides the Hot language features
    if combined_content.trim().is_empty() {
        return Err("Configuration files are empty".to_string());
    }

    // Prepend namespace declaration if not already present
    // This allows users to omit ns declaration from their hot.hot files
    let namespaced_content = if combined_content.contains("::hot::conf ns") {
        combined_content.clone()
    } else {
        format!("::hot::conf ns\n\n{}", combined_content)
    };

    // Use engine to execute the configuration code and extract the 'hot' variable
    tracing::debug!("Executing configuration content");

    // Execute the configuration code and retain the VM state for variable extraction
    // The engine will automatically load hot-std using HOT_HOME or fallback to ./hot/pkg/hot-std
    // when no project configuration is provided (see execute_code_and_retain_state)
    match hot::lang::engine::Engine::execute_eval_and_retain_state(
        &namespaced_content,
        &[],  // src_paths - empty, engine will load hot-std automatically
        &[],  // test_paths - empty for config-only execution
        None, // conf - no existing config needed for this execution
        None, // project_name - not needed for config execution
        hot::env::is_local_dev(),
    ) {
        Ok(executed_engine) => {
            tracing::debug!("Configuration executed successfully");

            // Try to extract the 'hot' variable from the ::hot::conf namespace
            if let Some(hot_var) = executed_engine.extract_var(Some("::hot::conf"), "hot") {
                tracing::debug!(
                    "Successfully extracted 'hot' variable from ::hot::conf namespace: {:?}",
                    hot_var
                );
                Ok(hot_var)
            } else {
                tracing::warn!("'hot' variable not found in executed configuration");

                // Debug: List all available variables
                tracing::debug!(
                    "Available namespaces: {:?}",
                    executed_engine.get_namespace_names()
                );
                if let Some(current_ns_vars) = executed_engine.extract_namespace_vars(None) {
                    tracing::debug!(
                        "Variables in current namespace: {:?}",
                        current_ns_vars.keys().collect::<Vec<_>>()
                    );
                }

                // Return empty configuration if 'hot' variable is not found
                Ok(Val::map_empty())
            }
        }
        Err(e) => {
            tracing::error!("Failed to execute configuration: {}", e);
            Err(format!("Configuration execution failed: {}", e))
        }
    }
}

/// Reload configuration after init has created a new hot.hot file
/// This is a simplified version of the main config loading that only reloads hot.hot
pub(crate) fn reload_conf_after_init(base_conf: &Val) -> Result<Val, String> {
    // Start fresh with defaults and apply env vars
    let mut conf = create_default_conf();
    conf = apply_env_vars(conf);

    // Load the newly created hot.hot file
    let conf_files = vec!["hot.hot".to_string()];
    let merged_src_paths = get_merged_src_paths(&conf, None, &[]);

    match load_conf(&conf_files, &merged_src_paths) {
        Ok(hot_config) => {
            tracing::debug!("Successfully loaded configuration from newly created hot.hot");
            conf = conf.merge(&hot_config);
        }
        Err(e) => {
            return Err(format!(
                "Failed to load newly created hot.hot configuration: {}",
                e
            ));
        }
    }

    // Apply project configuration
    let project_conf = hot::project::get_resolved_conf(conf.clone());
    let mut existing_default = conf.get("set").unwrap_or_else(Val::map_empty);
    let project_default = project_conf.get("set").unwrap_or_else(Val::map_empty);
    existing_default = existing_default.merge(&project_default);
    conf = conf.set("set", existing_default);
    conf = conf.set(
        "project",
        project_conf.get("project").unwrap_or_else(Val::map_empty),
    );

    // Apply other defaults
    conf = hot::check::get_resolved_conf(conf);
    conf = hot::lsp::get_resolved_conf(conf);

    // Apply emitter defaults (in project context since this runs after init)
    // Note: create_default_conf() doesn't set emitter.type, so the type here is only set if
    // the user explicitly provided it (via hot.hot or CLI). get_emitter_resolved_conf will
    // use "db" as the default since we're in a project (in_project=true).
    let emitter_conf_from_user = conf.get("emitter").unwrap_or_else(Val::map_empty);
    let resolved_emitter_conf = get_emitter_resolved_conf(emitter_conf_from_user, true);
    conf = conf.set("emitter", resolved_emitter_conf);

    // Apply queue defaults (in project context since this runs after init)
    // Note: Similar to emitter, create_default_conf() doesn't set queue.type, so the type
    // is only set if user explicitly provided it. get_resolved_conf will use "memory" as
    // the default since we're in a project (in_project=true).
    let queue_conf_from_user = conf.get("queue").unwrap_or_else(Val::map_empty);
    let resolved_queue_conf = hot::queue::get_resolved_conf(queue_conf_from_user, true);
    conf = conf.set("queue", resolved_queue_conf);

    // Preserve any settings from the original conf that may have been set via CLI flags
    // (like db-uri) that we don't want to lose
    if let Some(db_uri) = base_conf
        .get("db")
        .and_then(|db| db.get("uri"))
        .and_then(|uri| match uri {
            Val::Str(s) => Some((*s).to_string()),
            _ => None,
        })
    {
        // Check if it's a custom DB URI (not just the default)
        let default_db_uri = "postgres://localhost:5432/hot";
        if db_uri != default_db_uri {
            // Use "db.uri" as the path, not "db" with "uri" as default!
            conf = conf.set_str("db.uri", Some(db_uri), "");
        }
    }

    Ok(conf)
}

// Function to load and parse context file(s) using engine
// Context files use ::hot::ctx/set calls to set context variables
// Example hot/ctx.hot:
//   ::hot::ctx/set("anthropic.api.key", ::hot::env/get("ANTHROPIC_API_KEY", ""))
//   ::hot::ctx/set("openai.api.key", ::hot::env/get("OPENAI_API_KEY", ""))
pub(crate) fn load_ctx(
    ctx_paths: &[String],
) -> Result<ahash::AHashMap<String, hot::val::Val>, String> {
    // Combine all context files into a single code string
    let mut combined_content = String::new();

    for ctx_path in ctx_paths {
        // Check if file exists
        let path = Path::new(ctx_path);
        if !path.exists() {
            return Err(format!("Context file not found: {}", ctx_path));
        }

        // Read file content
        let content = match fs::read_to_string(path) {
            Ok(content) => content,
            Err(e) => {
                return Err(format!("Failed to read context file {}: {}", ctx_path, e));
            }
        };

        // Add content with a newline separator
        if !combined_content.is_empty() {
            combined_content.push('\n');
        }
        combined_content.push_str(&content);
    }

    // Use engine with minimal dependencies for context loading
    if combined_content.trim().is_empty() {
        return Err("Context files are empty".to_string());
    }

    // Prepend namespace declaration if not already present
    // This allows users to omit ns declaration from their hot/ctx.hot files
    let namespaced_content = if combined_content.contains("::hot::run::ctx ns") {
        combined_content.clone()
    } else {
        format!("::hot::run::ctx ns\n\n{}", combined_content)
    };

    tracing::debug!(
        "Executing context file content ({} bytes)",
        namespaced_content.len()
    );

    // Execute the context code - ::hot::ctx/set calls will populate vm.context_storage
    match hot::lang::engine::Engine::execute_eval_and_retain_state(
        &namespaced_content,
        &[],  // src_paths - empty, engine will load hot-std automatically
        &[],  // test_paths - empty for ctx-only execution
        None, // conf - no existing config needed for this execution
        None, // project_name - not needed for ctx execution
        hot::env::is_local_dev(),
    ) {
        Ok(executed_engine) => {
            // Extract context_storage which was populated by ::hot::ctx/set calls
            let context_storage = executed_engine.extract_context_storage();
            tracing::debug!(
                "Context file executed successfully, loaded {} context variables",
                context_storage.len()
            );
            for key in context_storage.keys() {
                tracing::debug!("  Loaded ctx: {}", key);
            }
            Ok(context_storage)
        }
        Err(e) => {
            tracing::error!("Failed to execute context file: {}", e);
            Err(format!("Context file execution failed: {}", e))
        }
    }
}

// Function to get merged src paths from project configuration and CLI arguments
pub(crate) fn get_merged_src_paths(
    conf: &Val,
    project_name: Option<&str>,
    cli_src_paths: &[String],
) -> Vec<String> {
    let mut merged_paths = Vec::new();

    // Get project name (CLI specified or default)
    let resolved_project_name = project_name
        .map(|s| s.to_string())
        .unwrap_or_else(|| hot::project::get_default_project_name(conf));

    // Get paths from project configuration
    let project_paths = hot::project::get_project_src_paths(conf, &resolved_project_name);
    merged_paths.extend(project_paths);

    // Add CLI paths (they override/extend the project's paths)
    for path in cli_src_paths {
        merged_paths.push(path.clone());
    }

    merged_paths
}

// Function to get merged test paths from project configuration and CLI arguments
pub(crate) fn get_merged_test_paths(
    conf: &Val,
    project_name: Option<&str>,
    cli_test_paths: &[String],
) -> Vec<String> {
    let mut merged_paths = Vec::new();

    // Get project name (CLI specified or default)
    let resolved_project_name = project_name
        .map(|s| s.to_string())
        .unwrap_or_else(|| hot::project::get_default_project_name(conf));

    // Get paths from project configuration
    let project_paths = hot::project::get_project_test_paths(conf, &resolved_project_name);
    merged_paths.extend(project_paths);

    // Add CLI paths (they override/extend the project's paths)
    for path in cli_test_paths {
        merged_paths.push(path.clone());
    }

    merged_paths
}

pub(crate) fn show_command_config(command: &Option<Command>, conf: &Val) {
    let relevant_config = match command {
        Some(Command::Dev { .. }) => {
            // Dev command uses: api, app, worker, scheduler, log, daemon config
            let config_keys = vec![
                "api.host",
                "api.port",
                "app.host",
                "app.port",
                "worker.threads",
                "worker.queue.type",
                "worker.serialization",
                "scheduler.backfill",
                "scheduler.sync-interval-seconds",
                "scheduler.queue.type",
                "scheduler.serialization",
                "log.level",
                "log.target",
                "log.dir",
                "log.rotation",
                "log.retention",
                "log.format",
                "daemon",
                "dev.open",
                "jit",
                "engine",
            ];
            filter_config(conf, &config_keys)
        }
        Some(Command::Test { .. }) => {
            let config_keys = vec![
                "project",
                "set.project",
                "db.uri",
                "db.schema",
                "file",
                "jit",
                "engine",
            ];
            filter_config(conf, &config_keys)
        }
        Some(Command::Api { .. }) => {
            // API command uses: full config - needs db, redis, build storage, queue, etc.
            conf.clone()
        }
        Some(Command::App { .. }) => {
            // App command uses: full config - needs db, redis, build/file storage, billing, etc.
            conf.clone()
        }
        Some(Command::Worker { .. }) => {
            // Worker command uses: full config - needs emitter, engine, box, build/file storage, etc.
            conf.clone()
        }
        Some(Command::TaskWorker { .. }) => {
            // TaskWorker command uses: db, redis, queue, box, task, build, file storage
            conf.clone()
        }
        Some(Command::Scheduler { .. }) => {
            // Scheduler command uses: full config - needs db, redis, queue, etc.
            conf.clone()
        }
        Some(Command::Run { .. }) => {
            let config_keys = vec!["project", "set.project", "jit", "engine"];
            filter_config(conf, &config_keys)
        }
        Some(Command::Eval { .. }) => {
            let config_keys = vec!["project", "set.project", "jit", "engine"];
            filter_config(conf, &config_keys)
        }
        Some(Command::Repl { .. }) => {
            let config_keys = vec!["project", "set.project", "jit", "engine"];
            filter_config(conf, &config_keys)
        }
        Some(Command::Cache { .. }) => {
            // Cache command just needs project name for finding .hot directory
            let config_keys = vec!["project"];
            filter_config(conf, &config_keys)
        }
        Some(Command::Queue { .. }) => {
            // Queue command uses: queue type, redis config
            let config_keys = vec!["queue.type", "redis.uri"];
            filter_config(conf, &config_keys)
        }
        Some(Command::Db { .. }) => {
            // DB command uses: db config
            let config_keys = vec!["db.uri", "db.schema"];
            filter_config(conf, &config_keys)
        }
        Some(Command::Context { .. }) => {
            // Context command uses: db, project config (for --local) and remote config (for API)
            let config_keys = vec![
                "db.uri",
                "db.schema",
                "project",
                "set.project",
                "set.remote",
                "remote",
            ];
            filter_config(conf, &config_keys)
        }
        Some(Command::Init { .. }) => {
            // Init command uses: db, log config
            let config_keys = vec![
                "db.uri",
                "db.schema",
                "log.level",
                "log.target",
                "log.dir",
                "log.rotation",
                "log.retention",
                "log.format",
            ];
            filter_config(conf, &config_keys)
        }
        Some(Command::Ai { .. }) => {
            // Ai command doesn't need any config
            Val::map_empty()
        }
        Some(Command::Build { .. }) => {
            // Build command uses: project, db config
            let config_keys = vec!["project", "set.project", "db.uri"];
            filter_config(conf, &config_keys)
        }
        Some(Command::Builds { .. }) => {
            // Builds command uses: db config (for --local) and remote config (for API)
            let config_keys = vec!["db.uri", "set.remote", "set.project", "remote", "project"];
            filter_config(conf, &config_keys)
        }
        Some(Command::Projects { .. }) => {
            // Projects command uses: db config (for --local) and remote config (for API)
            let config_keys = vec!["db.uri", "set.remote", "remote", "profile"];
            filter_config(conf, &config_keys)
        }
        Some(Command::Project { .. }) => {
            // Project command uses: db config (for --local) and remote config (for API)
            let config_keys = vec!["db.uri", "set.remote", "remote", "profile"];
            filter_config(conf, &config_keys)
        }
        Some(Command::Extract { .. }) => {
            // Extract command uses minimal config
            let config_keys = vec![];
            filter_config(conf, &config_keys)
        }
        Some(Command::Compile { .. }) => {
            // Compile command uses project config only.
            let config_keys = vec!["project", "set.project"];
            filter_config(conf, &config_keys)
        }
        Some(Command::Check { .. }) => {
            // Check command uses project config only.
            let config_keys = vec!["project", "set.project"];
            filter_config(conf, &config_keys)
        }
        Some(Command::Watch { .. }) => {
            // Watch command uses: project and watch config
            let config_keys = vec!["project", "set.project", "watch.debounce"];
            filter_config(conf, &config_keys)
        }
        _ => {
            // For other commands (like conf), show full config
            conf.clone()
        }
    };

    let relevant_config = redact_sensitive_config(&relevant_config);

    println!(
        "{}",
        relevant_config.to_dot_separated_with_section_breaks("hot.")
    );
}

fn filter_config(conf: &Val, keys: &[&str]) -> Val {
    let mut filtered = Val::map_empty();
    for key in keys {
        if let Some(value) = conf.get(key) {
            filtered = filtered.set(key, value.clone());
        }
    }
    filtered
}

fn redact_sensitive_config(conf: &Val) -> Val {
    redact_sensitive_config_at_path(conf, "")
}

fn redact_sensitive_config_at_path(value: &Val, path: &str) -> Val {
    if is_uri_config_path(path) {
        return match value {
            Val::Str(uri) => Val::from(hot::db::redact_password(uri)),
            _ => value.clone(),
        };
    }

    if is_sensitive_config_path(path) {
        return Val::from("***");
    }

    match value {
        Val::Map(map) => {
            let entries = map.iter().map(|(key, child)| {
                let segment = config_key_segment(key);
                let child_path = if path.is_empty() {
                    segment
                } else {
                    format!("{}.{}", path, segment)
                };
                (
                    key.clone(),
                    redact_sensitive_config_at_path(child, &child_path),
                )
            });
            Val::map_from_iter(entries)
        }
        Val::Vec(items) => Val::Vec(
            items
                .iter()
                .map(|item| redact_sensitive_config_at_path(item, path))
                .collect(),
        ),
        _ => value.clone(),
    }
}

fn config_key_segment(key: &Val) -> String {
    match key {
        Val::Str(s) => s.to_string(),
        _ => key.to_string(),
    }
}

fn is_uri_config_path(path: &str) -> bool {
    path.eq_ignore_ascii_case("db.uri")
        || path.eq_ignore_ascii_case("redis.uri")
        || path.ends_with(".db.uri")
        || path.ends_with(".redis.uri")
}

fn is_sensitive_config_path(path: &str) -> bool {
    let last_segment = path.rsplit('.').next().unwrap_or(path).to_ascii_lowercase();
    matches!(
        last_segment.as_str(),
        "key" | "token" | "secret" | "password" | "client-secret" | "session-secret"
    ) || last_segment.ends_with("-key")
        || last_segment.ends_with("_key")
        || last_segment.ends_with("-token")
        || last_segment.ends_with("_token")
        || last_segment.ends_with("-secret")
        || last_segment.ends_with("_secret")
        || last_segment.ends_with("-password")
        || last_segment.ends_with("_password")
}

pub(crate) fn create_emitter(
    conf: &Val,
    db_pool: &hot::db::DatabasePool,
) -> Result<Option<std::sync::Arc<dyn EngineEventEmitter>>, String> {
    // Get resolved emitter configuration
    let emitter_conf = conf.get("emitter").unwrap_or_else(Val::map_empty);

    // Get emitter type
    let emitter_type = emitter_conf.get_str("type");

    tracing::debug!(
        "CLI create_emitter: emitter_conf={:?}, emitter_type='{}'",
        emitter_conf,
        emitter_type
    );

    // Return None if emitter type is "none" or empty
    if emitter_type == "none" || emitter_type.is_empty() {
        tracing::warn!(
            "CLI create_emitter: returning None (type is '{}' or empty)",
            emitter_type
        );
        return Ok(None);
    }

    // Get filter configuration
    let filter_conf = emitter_conf.get("filter");

    // Debug: Log the filter configuration being used
    tracing::debug!("Emitter filter configuration: {:?}", filter_conf);

    // Create the base emitter and wrap with filtering based on type
    match emitter_type.as_str() {
        "console" => {
            let console_emitter = hot::lang::emitter::ConsoleEngineEventEmitter::new();
            let filtered_emitter =
                hot::lang::emitter::FilteredEmitter::new(console_emitter, filter_conf.as_ref())?;
            Ok(Some(std::sync::Arc::new(filtered_emitter)))
        }
        "db" => {
            // Use existing database pool instead of creating a new one
            // Note: stream_data is no longer persisted to DB - delivered via Redis Streams only
            let db_emitter =
                hot::lang::emitter::DatabaseEngineEventEmitter::new_with_pool(db_pool.clone());
            let filtered_emitter =
                hot::lang::emitter::FilteredEmitter::new(db_emitter, filter_conf.as_ref())?;
            Ok(Some(std::sync::Arc::new(filtered_emitter)))
        }
        unknown => Err(format!(
            "Unknown emitter type: {}. Available options: none, console, db",
            unknown
        )),
    }
}

pub(crate) fn create_event_publisher(
    conf: &Val,
    db_pool: &hot::db::DatabasePool,
) -> Result<Option<std::sync::Arc<dyn hot::lang::event::EventPublisher>>, String> {
    // Extract queue configuration
    let queue_type_str = conf.get_str_or_default("queue.type", "memory");
    let queue_type = QueueType::from_str(&queue_type_str).unwrap_or(QueueType::Memory);

    let redis_uri_str = conf.get_str("redis.uri");
    let redis_uri = if redis_uri_str == "null" || redis_uri_str.is_empty() {
        None
    } else {
        Some(redis_uri_str.clone())
    };

    // Get redis cluster setting
    let redis_cluster = conf.get_bool("redis.cluster");

    tracing::debug!(
        "CLI create_event_publisher: queue_type='{}', redis_uri='{}', redis_cluster={}",
        queue_type_str,
        if redis_uri.is_some() {
            "Some(...)"
        } else {
            "None"
        },
        redis_cluster
    );

    let serialization_str = conf.get_str("serialization.type");
    let serialization = Serialization::from_str(&serialization_str).unwrap_or_default();

    // Create database publisher with existing pool (ensures connection is ready)
    let database_publisher =
        hot::lang::event::DatabaseEventPublisher::new_with_pool(db_pool.clone());

    // Create queue publisher with extracted configuration (including cluster support)
    let queue_publisher = hot::lang::event::QueueEventPublisher::new_with_cluster(
        queue_type,
        "hot:event".to_string(),
        redis_uri,
        redis_cluster,
        serialization,
    );

    // Create combined publisher
    let combined_publisher =
        hot::lang::event::QueueAndDatabaseEventPublisher::new(queue_publisher, database_publisher);
    Ok(Some(std::sync::Arc::new(combined_publisher)))
}

// Apply command-specific default overrides (after config files, before CLI args)
/// Determine the appropriate log format based on the command being run.
/// - Server/long-running commands use Full format (timestamp, level, target, message)
/// - CLI/one-shot commands use Simple format (just the message)
pub(crate) fn get_log_format_for_command(command: &Option<Command>) -> hot::log::LogFormat {
    match command {
        // Server/long-running commands - need full logging with timestamps
        Some(Command::Dev { .. })
        | Some(Command::Api { .. })
        | Some(Command::App { .. })
        | Some(Command::Worker { .. })
        | Some(Command::TaskWorker { .. })
        | Some(Command::Scheduler { .. })
        | Some(Command::Lsp { .. }) => hot::log::LogFormat::Full,

        // All other commands use simple format (just the message)
        _ => hot::log::LogFormat::Simple,
    }
}

pub(crate) fn apply_command_specific_defaults(mut conf: Val, command: &Option<Command>) -> Val {
    if let Some(cmd) = command {
        match cmd {
            Command::Check { global, .. } => {
                if global.db_uri.is_none() {
                    conf = conf.set_str("db.uri", Some("".to_string()), "");
                }
                if global.emitter.emitter_type.is_none() {
                    conf = conf.set_str("emitter.type", Some("none".to_string()), "");
                }
            }
            Command::Test { global, .. } => {
                if global.db_uri.is_none() {
                    conf = conf.set_str("db.uri", Some("".to_string()), "");
                }
                if global.emitter.emitter_type.is_none() {
                    conf = conf.set_str("emitter.type", Some("none".to_string()), "");
                }
            }
            _ => {}
        }
    }
    conf
}

#[cfg(test)]
mod tests {
    use super::*;
    use hot::val;

    #[test]
    fn test_redact_sensitive_config_masks_keys_and_uri_passwords() {
        let conf = val!({
            "remote": {
                "hot-dev": {
                    "url": "https://api.hot.dev",
                    "key": "hot_live_secret"
                }
            },
            "db": {
                "uri": "postgres://user:password@localhost/hot"
            },
            "redis": {
                "uri": "redis://:redis-password@localhost:6379"
            },
            "app": {
                "session-secret": "jwt-secret"
            }
        });

        let redacted = redact_sensitive_config(&conf);

        assert_eq!(redacted.get_str("remote.hot-dev.key"), "***");
        assert_eq!(redacted.get_str("app.session-secret"), "***");

        let db_uri = redacted.get_str("db.uri");
        assert!(db_uri.contains("***"));
        assert!(!db_uri.contains("password"));

        let redis_uri = redacted.get_str("redis.uri");
        assert!(redis_uri.contains("***"));
        assert!(!redis_uri.contains("redis-password"));

        assert_eq!(
            redacted.get_str("remote.hot-dev.url"),
            "https://api.hot.dev"
        );
    }
}

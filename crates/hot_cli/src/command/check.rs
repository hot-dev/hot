//! `hot check` — type/syntax check the project, optionally watching.

use std::path::PathBuf;

use hot::val::Val;

use crate::cli::GlobalOptions;
use crate::command::deploy::setup_live_build_for_dev;
use crate::conf::{get_merged_src_paths, get_merged_test_paths};

// Helper function to run check compilation using engine. Prints any
// project-scoped warnings (e.g. deprecated-API usage) and returns whether any
// were emitted so callers can apply the `--deny-warnings` exit policy.
async fn run_check_compilation(conf: &Val, global_options: &GlobalOptions) -> Result<bool, String> {
    let src_paths = get_merged_src_paths(
        conf,
        global_options.project.as_deref(),
        &global_options.src_paths,
    );
    let include_tests = global_options
        .with_tests
        .unwrap_or_else(|| conf.get_bool("check.with-tests"));
    let test_paths = if include_tests {
        get_merged_test_paths(
            conf,
            global_options.project.as_deref(),
            &global_options.test_paths,
        )
    } else {
        Vec::new()
    };

    // Get project name for dependency resolution
    let project_name = global_options
        .project
        .clone()
        .unwrap_or_else(|| hot::project::get_default_project_name(conf));

    // Create or update live build for development (consistent with run/eval/repl/test/check)
    setup_live_build_for_dev(conf, global_options, &src_paths, &test_paths).await?;

    let color = hot::env::is_local_dev();
    let mut warnings = hot::lang::errors::CompilerErrors::new();
    hot::lang::engine::Engine::check_sources_pipeline_with_context_warnings(
        &src_paths,
        &test_paths,
        Some(conf),
        Some(&project_name),
        None,
        color,
        Some(&mut warnings),
    )?;
    if warnings.has_warnings() {
        println!("{}", warnings.format_warnings(color));
    }
    Ok(warnings.has_warnings())
}

pub(crate) async fn run_check_watch(
    format: Option<&str>,
    raw: bool,
    deny_warnings: bool,
    debounce_ms: u64,
    conf: &Val,
    global_options: &GlobalOptions,
) -> Result<i32, String> {
    use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
    use std::sync::mpsc::channel;
    use std::time::{Duration, Instant};

    // Start with an initial run
    let mut last_run = Instant::now();
    let fmt = format.unwrap_or("pretty").to_lowercase();
    let json_mode = fmt == "json" || fmt == "json-min";

    // Synchronous one-shot check that handles both pretty and json output.
    // Async pretty path uses the existing `run_check_compilation` helper;
    // structured json path goes straight to the diagnostics-only pipeline.
    fn emit_json(
        json_min: bool,
        raw: bool,
        deny_warnings: bool,
        conf: &Val,
        global_options: &GlobalOptions,
    ) -> i32 {
        let _ = raw; // raw doesn't apply to json output
        let src_paths = get_merged_src_paths(
            conf,
            global_options.project.as_deref(),
            &global_options.src_paths,
        );
        let include_tests = global_options
            .with_tests
            .unwrap_or_else(|| conf.get_bool("check.with-tests"));
        let test_paths = if include_tests {
            get_merged_test_paths(
                conf,
                global_options.project.as_deref(),
                &global_options.test_paths,
            )
        } else {
            Vec::new()
        };
        let project_name = global_options
            .project
            .clone()
            .unwrap_or_else(|| hot::project::get_default_project_name(conf));
        let errors = hot::lang::engine::Engine::check_sources_pipeline_diagnostics(
            &src_paths,
            &test_paths,
            Some(conf),
            Some(&project_name),
        );
        let diagnostics = errors.to_diagnostics();
        let serialized = if json_min {
            serde_json::to_string(&diagnostics)
        } else {
            serde_json::to_string_pretty(&diagnostics)
        }
        .unwrap_or_else(|_| "[]".to_string());
        println!("{}", serialized);
        let fail = !errors.is_empty() || (deny_warnings && errors.has_warnings());
        if fail { 1 } else { 0 }
    }

    if json_mode {
        emit_json(fmt == "json-min", raw, deny_warnings, conf, global_options);
    } else {
        match run_check_compilation(conf, global_options).await {
            Ok(had_warnings) => {
                if !raw && !had_warnings {
                    println!("No issues found");
                }
            }
            Err(e) => println!("{}", e),
        }
    }

    let (tx, rx) = channel();
    let mut watcher: RecommendedWatcher =
        Watcher::new(tx, notify::Config::default()).map_err(|e| e.to_string())?;
    let include_tests = global_options
        .with_tests
        .unwrap_or_else(|| conf.get_bool("watch.with-tests"));

    for p in get_merged_src_paths(
        conf,
        global_options.project.as_deref(),
        &global_options.src_paths,
    ) {
        let pb = PathBuf::from(p);
        if pb.exists() {
            watcher
                .watch(&pb, RecursiveMode::Recursive)
                .map_err(|e| e.to_string())?;
        }
    }

    if include_tests {
        for p in get_merged_test_paths(
            conf,
            global_options.project.as_deref(),
            &global_options.test_paths,
        ) {
            let pb = PathBuf::from(p);
            if pb.exists() {
                watcher
                    .watch(&pb, RecursiveMode::Recursive)
                    .map_err(|e| e.to_string())?;
            }
        }
    }

    let debounce = Duration::from_millis(debounce_ms.max(50));
    let mut pending = false;

    loop {
        match rx.recv() {
            Ok(Ok(event)) => {
                // filter to .hot files only
                let is_hot = event
                    .paths
                    .iter()
                    .any(|p| p.extension().and_then(|e| e.to_str()) == Some("hot"));
                if is_hot {
                    match event.kind {
                        EventKind::Modify(_)
                        | EventKind::Create(_)
                        | EventKind::Remove(_)
                        | EventKind::Any => {
                            pending = true;
                        }
                        _ => {}
                    }
                }
                if pending && last_run.elapsed() >= debounce {
                    pending = false;
                    last_run = Instant::now();
                    if json_mode {
                        emit_json(fmt == "json-min", raw, deny_warnings, conf, global_options);
                    } else {
                        // Use tokio runtime to call async function from sync context
                        let rt = tokio::runtime::Runtime::new().unwrap();
                        match rt.block_on(run_check_compilation(conf, global_options)) {
                            Ok(had_warnings) => {
                                if !raw && !had_warnings {
                                    println!("No issues found");
                                }
                            }
                            Err(e) => println!("{}", e),
                        }
                    }
                }
            }
            Ok(Err(_e)) => {
                // Watch error - ignore and continue
            }
            Err(_e) => {
                // Channel closed or error - exit gracefully
                return Ok(0);
            }
        }
    }
}

pub(crate) async fn run_check_with_raw(
    format: Option<&str>,
    raw: bool,
    deny_warnings: bool,
    conf: &Val,
    global_options: &GlobalOptions,
    context_storage: Option<ahash::AHashMap<String, hot::val::Val>>,
    extra_path: Option<&str>,
) -> Result<i32, String> {
    tracing::debug!("Checking project sources");

    // Get paths using the merged path functions
    let mut src_paths = get_merged_src_paths(
        conf,
        global_options.project.as_deref(),
        &global_options.src_paths,
    );
    let include_tests = global_options
        .with_tests
        .unwrap_or_else(|| conf.get_bool("check.with-tests"));
    let test_paths = if include_tests {
        get_merged_test_paths(
            conf,
            global_options.project.as_deref(),
            &global_options.test_paths,
        )
    } else {
        Vec::new()
    };

    // If an extra path is provided, add it to src_paths if not already included
    // (This ensures the file/directory is checked even if it's outside normal src paths)
    if let Some(ep) = extra_path {
        let path = std::path::Path::new(ep);
        let path_str = if path.is_absolute() {
            ep.to_string()
        } else {
            std::env::current_dir()
                .map(|cwd| cwd.join(path).display().to_string())
                .unwrap_or_else(|_| ep.to_string())
        };
        // Add to src_paths if not already covered by an existing path
        if !src_paths.iter().any(|p| path_str.starts_with(p)) {
            src_paths.push(path_str);
        }
    }

    // Get project name for dependency resolution
    let project_name = global_options
        .project
        .clone()
        .unwrap_or_else(|| hot::project::get_default_project_name(conf));

    // Create or update live build for development
    setup_live_build_for_dev(conf, global_options, &src_paths, &test_paths).await?;

    let fmt = format.unwrap_or("pretty").to_lowercase();
    let json_mode = fmt == "json" || fmt == "json-min";

    if json_mode {
        // Structured path: collect diagnostics and emit as JSON regardless of
        // success/failure. Includes ctx-requirements so CI can catch missing
        // context variables alongside type errors.
        let available_ctx_keys: ahash::AHashSet<String> = context_storage
            .as_ref()
            .map(|cs| cs.keys().cloned().collect())
            .unwrap_or_default();
        let errors = hot::lang::engine::Engine::check_sources_pipeline_diagnostics_with_ctx(
            &src_paths,
            &test_paths,
            Some(conf),
            Some(&project_name),
            Some(&available_ctx_keys),
        );
        let diagnostics = errors.to_diagnostics();
        let serialized = if fmt == "json-min" {
            serde_json::to_string(&diagnostics)
        } else {
            serde_json::to_string_pretty(&diagnostics)
        }
        .unwrap_or_else(|_| "[]".to_string());
        println!("{}", serialized);
        let fail = !errors.is_empty() || (deny_warnings && errors.has_warnings());
        return Ok(if fail { 1 } else { 0 });
    }

    // Pretty/text path: keep using the legacy formatter with ctx-requirements
    // validation included. Project-scoped warnings (deprecated-API usage) are
    // collected and rendered on success without failing the check unless
    // `--deny-warnings` is set.
    let color = hot::env::is_local_dev();
    let mut warnings = hot::lang::errors::CompilerErrors::new();
    match hot::lang::engine::Engine::check_sources_pipeline_with_context_warnings(
        &src_paths,
        &test_paths,
        Some(conf),
        Some(&project_name),
        context_storage.as_ref(),
        color,
        Some(&mut warnings),
    ) {
        Ok(()) => {
            let had_warnings = warnings.has_warnings();
            if had_warnings {
                println!("{}", warnings.format_warnings(color));
            } else if !raw {
                println!("No issues found");
            }
            Ok(if had_warnings && deny_warnings { 1 } else { 0 })
        }
        Err(e) => {
            println!("{}", e);
            Ok(1)
        }
    }
}

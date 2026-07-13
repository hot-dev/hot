//! `hot repl` — interactive Hot REPL.

use hot::val::Val;
use rustyline::{DefaultEditor, error::ReadlineError};

use crate::cli::GlobalOptions;
use crate::command::deploy::setup_live_build_for_dev;
use crate::conf::{get_merged_src_paths, get_merged_test_paths};

pub(crate) async fn run_repl(
    conf: &Val,
    global_options: &GlobalOptions,
    context_storage: Option<ahash::AHashMap<String, hot::val::Val>>,
    value_format: Option<&str>,
) -> Result<(), String> {
    // Future work: emit run/var events for each REPL input once the eval
    // pipeline accepts an emitter.
    //
    // 1. Create emitter from config:
    //    let emitter = create_emitter(conf)?;
    //
    // 2. Pass emitter to engine for each REPL input (when supported):
    //    Engine::eval_code_pipeline_with_deps_and_emitter(
    //        &eval_code, &src_paths, &test_paths, Some(conf),
    //        Some(&project_name), emitter.clone()
    //    )
    //
    // 3. Events to emit for each input:
    //    - run:start (evaluation begins)
    //    - var:start/var:stop (for each variable execution)
    //    - run:stop (evaluation ends)
    //
    // 4. Shutdown emitter on REPL exit:
    //    if let Some(emitter) = &emitter {
    //        let _ = emitter.shutdown().await;
    //    }
    // Get paths for dependency resolution
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

    // Create or update live build for development
    setup_live_build_for_dev(conf, global_options, &src_paths, &test_paths).await?;

    // Pre-warm the package cache before starting the REPL loop
    // This parses all dependencies once so the first REPL input is fast.
    // Uses spawn_blocking to avoid blocking the async runtime with rayon parallelism.
    let prewarm_src_paths = src_paths.clone();
    let prewarm_conf = conf.clone();
    let prewarm_project_name = project_name.clone();
    let prewarm_result = tokio::task::spawn_blocking(move || {
        hot::lang::engine::Engine::prewarm_package_cache(
            &prewarm_src_paths,
            Some(&prewarm_conf),
            Some(&prewarm_project_name),
        )
    })
    .await
    .map_err(|e| format!("Failed to spawn pre-warm task: {}", e))?;

    match prewarm_result {
        Ok(count) => {
            tracing::debug!("Pre-warmed package cache with {} units", count);
        }
        Err(e) => {
            tracing::warn!("Failed to pre-warm package cache: {}", e);
            // Don't fail REPL startup - it will just be slower on first input
        }
    }

    // Start the REPL loop
    println!("\n\x1b[91mhot repl\x1b[0m ('ctrl-d' to exit)\n");

    // Initialize readline editor
    let mut rl =
        DefaultEditor::new().map_err(|e| format!("Failed to initialize readline: {}", e))?;

    // Initialize REPL session with incremental execution
    let mut repl_session = match initialize_repl_session(
        &src_paths,
        &test_paths,
        conf,
        &project_name,
        context_storage,
    ) {
        Ok(session) => session,
        Err(e) => {
            eprintln!("Failed to initialize REPL session: {}", e);
            return Err(e);
        }
    };

    // Track consecutive Ctrl-C presses for helpful exit message
    let mut consecutive_ctrl_c = 0;
    let mut current_namespace = "::hot::dev".to_string();

    loop {
        // Create colored prompt
        let prompt = format!("\x1b[92m{}>\x1b[0m ", current_namespace);

        // Read user input with readline support
        match rl.readline(&prompt) {
            Ok(input) => {
                let input = input.trim();

                // Reset Ctrl-C counter on successful input
                consecutive_ctrl_c = 0;

                // Skip empty input
                if input.is_empty() {
                    continue;
                }

                // Add non-empty input to history
                let _ = rl.add_history_entry(input.to_string());

                // Note: ns declarations are now handled by the REPL session itself
                // The namespace will be updated automatically and reflected in current_namespace

                // Execute input in REPL session
                // IMPORTANT: VM execution runs in spawn_blocking to avoid blocking the tokio runtime.
                // This allows hot-std blocking I/O (HTTP, file ops) to work correctly.
                // We move the session into spawn_blocking and get it back with the result.
                let input_owned = input.to_string();
                let eval_result = tokio::task::spawn_blocking(move || {
                    let result = repl_session.eval(&input_owned);
                    (result, repl_session)
                })
                .await;

                match eval_result {
                    Ok((result, session)) => {
                        // Restore the session for next iteration
                        repl_session = session;

                        match result {
                            Ok(result) => {
                                // Print the result if it's not null
                                if !matches!(result, hot::val::Val::Null) {
                                    // Apply CLI value_format option if provided, otherwise use conf
                                    let display_conf = if let Some(fmt) = value_format {
                                        conf.set_str("value.format", Some(fmt.to_string()), "hot")
                                    } else {
                                        conf.clone()
                                    };
                                    println!("{}", result.format_with_conf(Some(&display_conf)));
                                }

                                // Update current namespace display from the session
                                current_namespace = repl_session.current_namespace().to_string();
                            }
                            Err(e) => {
                                eprintln!("Error: {}", e);
                            }
                        }
                    }
                    Err(e) => {
                        eprintln!("Task error: {}", e);
                        break;
                    }
                }
            }
            Err(ReadlineError::Interrupted) => {
                // Ctrl-C - increment counter and show helpful message after multiple presses
                consecutive_ctrl_c += 1;

                if consecutive_ctrl_c >= 2 {
                    println!("ctrl-c clears current command; ctrl-d to exit the repl");
                }
                continue;
            }
            Err(ReadlineError::Eof) => {
                // Ctrl-D - exit gracefully
                println!("goodbye, you hot dev, you!");
                break;
            }
            Err(err) => {
                eprintln!("REPL error: {}", err);
                break;
            }
        }
    }

    Ok(())
}

/// Initialize a REPL session with project dependencies loaded
fn initialize_repl_session(
    src_paths: &[String],
    test_paths: &[String],
    conf: &hot::val::Val,
    project_name: &str,
    context_storage: Option<ahash::AHashMap<String, hot::val::Val>>,
) -> Result<hot::lang::repl::ReplSession, String> {
    // Create REPL configuration
    let repl_config = hot::lang::repl::ReplConfig {
        src_paths: src_paths.to_vec(),
        test_paths: test_paths.to_vec(),
        conf: Some(conf.clone()),
        project_name: Some(project_name.to_string()),
        context_storage,
    };

    // Create and return the REPL session (starts in ::hot::dev by default)
    Ok(hot::lang::repl::ReplSession::new(repl_config))
}

//! `hot compile` — validate that a project's source compiles end-to-end.

use hot::val::Val;
use tracing::info;

use crate::cli::GlobalOptions;
use crate::command::deploy::setup_live_build_for_dev;

pub(crate) async fn run_compile(
    project_name: &str,
    src_paths: &[String],
    test_paths: &[String],
    conf: &Val,
    _global_options: &GlobalOptions,
) -> Result<(), String> {
    info!("Compiling project '{}'...", project_name);

    // Create or update live build for development
    setup_live_build_for_dev(conf, _global_options, src_paths, test_paths).await?;

    // Use the same approved pipeline as run command which properly loads hot-std
    // Create a temporary empty file for validation
    let temp_file = std::env::temp_dir().join("compile_validation.hot");
    std::fs::write(&temp_file, "// Compile validation file")
        .map_err(|e| format!("Failed to create temporary validation file: {}", e))?;

    let temp_file_str = temp_file.to_string_lossy().to_string();

    // IMPORTANT: VM execution runs in spawn_blocking to avoid blocking the tokio runtime.
    let src_paths_clone = src_paths.to_vec();
    let test_paths_clone = test_paths.to_vec();
    let conf_clone = conf.clone();
    let project_name_clone = project_name.to_string();

    let validation_result = tokio::task::spawn_blocking(move || {
        hot::lang::engine::Engine::run_file_pipeline_with_deps(
            &temp_file_str,
            &src_paths_clone,
            &test_paths_clone,
            Some(&conf_clone),
            Some(&project_name_clone),
            hot::env::is_local_dev(),
        )
    })
    .await
    .map_err(|e| format!("Task failed: {}", e))?;

    // Clean up temporary file
    let _ = std::fs::remove_file(&temp_file);

    match validation_result {
        Ok(_) => {
            println!("Project '{}' compiled successfully", project_name);
            Ok(())
        }
        Err(e) => {
            eprintln!("Compilation failed: {}", e);
            Err(format!("Compilation error: {}", e))
        }
    }
}

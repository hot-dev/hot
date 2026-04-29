//! `hot build` — package the current project sources into a bundle build,
//! plus the `--allow-secret-shape` plumbing shared with `hot deploy`.

use std::path::PathBuf;

use tracing::warn;

use crate::cli::{EmitterOptions, GlobalOptions};
use crate::command::deploy::setup_live_build_for_dev_with_secret_scan_opts;
use crate::profile::{extract_profile_identifiers, resolve_profile_to_uuids};

/// Build a [`hot::secret_scan::SecretScanOpts`] from project configuration
/// and the optional `--allow-secret-shape` CLI override.
///
/// Reads `hot.build.allow-secret-shape` from `conf`, which can be:
/// - `true` → entire scan disabled (equivalent to passing `--allow-secret-shape`)
/// - a `Vec<Str>` of gitignore-style globs → per-file allowlist
/// - missing/null → empty allowlist (full scan)
///
/// The CLI flag is OR'd onto the config value so a one-shot deploy can
/// bypass without editing hot.hot.
pub(crate) fn build_secret_scan_opts(
    conf: &hot::val::Val,
    cli_allow_all: bool,
) -> hot::secret_scan::SecretScanOpts {
    use hot::val::Val;
    let raw = conf.get("build").and_then(|b| b.get("allow-secret-shape"));
    let (mut allow_all, allow_patterns) = match raw {
        Some(Val::Bool(true)) => (true, Vec::new()),
        Some(Val::Vec(v)) => {
            let pats: Vec<String> = v
                .iter()
                .filter_map(|p| match p {
                    Val::Str(s) => Some(s.to_string()),
                    _ => None,
                })
                .collect();
            (false, pats)
        }
        _ => (false, Vec::new()),
    };
    if cli_allow_all {
        allow_all = true;
    }
    if cli_allow_all {
        eprintln!("⚠️  --allow-secret-shape is set: build-time secret-shape scan is disabled.");
    } else if allow_all {
        eprintln!(
            "⚠️  hot.build.allow-secret-shape is set: build-time secret-shape scan is disabled."
        );
    }
    hot::secret_scan::SecretScanOpts::new(allow_patterns, allow_all)
}

pub(crate) fn resource_bundle_options(
    conf: &hot::val::Val,
    project_name: &str,
    extra_resource_paths: &[String],
    force_no_gitignore: bool,
) -> (Vec<PathBuf>, bool, Vec<String>) {
    let mut resource_paths: Vec<PathBuf> =
        hot::project::get_project_resource_paths(conf, project_name)
            .into_iter()
            .map(PathBuf::from)
            .collect();
    resource_paths.extend(extra_resource_paths.iter().map(PathBuf::from));

    let respect_gitignore = if force_no_gitignore {
        false
    } else {
        hot::project::get_project_respect_gitignore(conf, project_name)
    };

    let mut resource_excludes = hot::project::get_project_ignore_excludes(conf, project_name);
    resource_excludes.extend(hot::project::get_project_resource_excludes(
        conf,
        project_name,
    ));

    (resource_paths, respect_gitignore, resource_excludes)
}

pub(crate) async fn run_build(
    bundle_name: &str,
    src_paths: &[String],
    build_dir: Option<&str>,
    conf: &hot::val::Val,
    extra_resource_paths: &[String],
    force_no_gitignore: bool,
    allow_secret_shape: bool,
) -> Result<(), String> {
    let db_uri = conf.get_str("db.uri");

    // Extract and resolve profile IDs for execution context
    let (user_id, env_id, org_id) =
        if let Some((user_email, org_slug, env_name)) = extract_profile_identifiers(conf) {
            if !db_uri.is_empty() {
                // Connect to database to resolve profile identifiers
                let db = match hot::db::create_db_pool(conf).await {
                    Ok(db) => db,
                    Err(e) => {
                        return Err(format!(
                            "Failed to connect to database for profile resolution: {}",
                            e
                        ));
                    }
                };

                // Resolve profile identifiers to UUIDs
                match resolve_profile_to_uuids(&db, &user_email, &org_slug, &env_name).await {
                    Ok((user_uuid, env_uuid, org_uuid)) => {
                        (Some(user_uuid), Some(env_uuid), Some(org_uuid))
                    }
                    Err(e) => {
                        return Err(e);
                    }
                }
            } else {
                // No database URI, skip profile resolution
                (None, None, None)
            }
        } else {
            // No profile configuration, skip profile resolution
            (None, None, None)
        };

    // Create or update live build before creating bundle build
    // This ensures the project is compiled and validated
    let global_options = GlobalOptions {
        conf_files: vec![],
        ctx_files: vec![],
        project: Some(bundle_name.to_string()),
        src_paths: vec![],
        test_paths: vec![],
        resource_paths: extra_resource_paths.to_vec(),
        no_gitignore: force_no_gitignore,
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

    let secret_scan_opts = build_secret_scan_opts(conf, allow_secret_shape);

    if let Err(e) = setup_live_build_for_dev_with_secret_scan_opts(
        conf,
        &global_options,
        src_paths,
        &[],
        secret_scan_opts.clone(),
    )
    .await
    {
        warn!("Failed to setup live build before bundle build: {}", e);
    }

    // Note: Compilation validation is handled inside hot::build::build_create

    // Create database connection
    let db = hot::db::create_db_pool(conf)
        .await
        .map_err(|e| format!("Failed to connect to database: {}", e))?;

    // Pull resource paths + ignore policy from project conf and CLI flags so
    // the bundle includes everything `::hot::resource/*` would see in `hot dev`.
    let (resource_paths, respect_gitignore, resource_excludes) =
        resource_bundle_options(conf, bundle_name, extra_resource_paths, force_no_gitignore);

    let build_ctx = hot::build::BuildContext::new(
        user_id,
        env_id,
        org_id,
        None,
        bundle_name.to_string(),
        src_paths.to_vec(),
        Vec::new(), // No test paths in builds - tests should not be bundled
    )
    .with_resources(resource_paths, respect_gitignore, resource_excludes)
    .with_secret_scan_opts(secret_scan_opts);

    match hot::build::build_create(
        &db,
        build_dir,
        build_ctx,
        Some(conf), // Pass the configuration for dependency loading
    )
    .await
    {
        Ok(result) => {
            println!("Build created successfully: {}", result.zip_path.display());
            println!("Project: {} ({})", bundle_name, result.build.project_id);
            println!(
                "Build: {} (size: {} bytes)",
                result.build.build_id, result.build.size
            );
            Ok(())
        }
        Err(e) => Err(e),
    }
}

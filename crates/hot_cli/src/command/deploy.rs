//! `hot deploy` and `hot upload` — bundle and ship builds to the API or
//! the local DB. Also hosts `setup_live_build_for_dev`, which all the
//! development-time commands (run/eval/repl/check/compile/test/dev) call to
//! materialize a live build before they execute.

use std::path::{Component, Path, PathBuf};
use std::str::FromStr;

use hot::val::Val;
use tracing::{info, warn};
use uuid::Uuid;

use crate::cli::GlobalOptions;
use crate::command::build::{
    build_secret_scan_opts, resource_bundle_options, secret_scan_opts_from_conf,
};
use crate::conf::get_merged_src_paths;
use crate::profile::{extract_profile_identifiers, resolve_profile_to_uuids};
use crate::remote::ApiClient;

fn resolve_local_build_storage_path(
    build_dir: &str,
    storage_path: &str,
) -> Result<PathBuf, String> {
    let storage_path = Path::new(storage_path);
    if storage_path.is_absolute() {
        return Err("Build storage path must be relative".to_string());
    }

    for component in storage_path.components() {
        if matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        ) {
            return Err(format!(
                "Build storage path cannot escape the build directory: {}",
                storage_path.display()
            ));
        }
    }

    Ok(Path::new(build_dir).join(storage_path))
}

/// Secret-shape matches, and the file each bundle path came from.
pub(crate) type SecretScan = (Vec<hot::secret_scan::SecretMatch>, Vec<(String, PathBuf)>);

/// Run the build-time secret-shape scan over what `hot check` and `hot watch`
/// would bundle (src, dependency and resource files), with the same
/// `hot.build.allow-secret-shape` allowlist and `--allow-secret-shape`
/// override as `hot build`. Returns the matches and, for each bundle path,
/// the file it came from.
pub(crate) fn scan_cli_inputs(
    conf: &Val,
    options: &GlobalOptions,
    src: &[String],
    allow_secret_shape: bool,
) -> Result<SecretScan, String> {
    let project = options
        .project
        .clone()
        .unwrap_or_else(|| hot::project::get_default_project_name(conf));
    let (paths, respect, excludes) = resource_bundle_options(
        conf,
        &project,
        &options.resource_paths,
        options.no_gitignore,
    );
    let opts = secret_scan_opts_from_conf(conf, allow_secret_shape);
    if opts.allow_all {
        return Ok((Vec::new(), Vec::new()));
    }
    // Tests are never bundled, so they are not scanned.
    let context =
        hot::build::BuildContext::new(None, None, None, None, project, src.to_vec(), Vec::new())
            .with_resources(paths, respect, excludes);
    let (files, resources) = hot::build::collect_build_inputs(&context, Some(conf))?;
    let hits = hot::secret_scan::scan_bundle(&files, &resources, &opts)
        .map_err(|e| format!("secret-shape scan failed: {}", e))?;
    let sources = files
        .into_iter()
        .map(|f| (f.relative_path, f.path))
        .chain(resources.into_iter().map(|r| {
            (
                format!("{}/{}", hot::bundle::RESOURCE_BUNDLE_PREFIX, r.rel_path),
                r.abs_path,
            )
        }))
        .collect();
    Ok((hits, sources))
}

pub(crate) fn is_ad_hoc_default_db(conf: &Val) -> bool {
    let db_uri = hot::db::get_db_uri_from_conf(conf);
    !Path::new("hot.hot").exists()
        && conf.get_bool_or_default("__host.ad-hoc-default-db", false)
        && (db_uri == hot::db::DEFAULT_DB_URI
            || db_uri.starts_with("sqlite:.hot/")
            // With `HOT_HOME` set, the default lives at `$HOT_HOME/db`.
            || db_uri == hot::db::default_db_uri())
}

/// Whether a command runs without a database: an ad hoc run on the default
/// database, or a configuration that opts out with an empty `db.uri`.
pub(crate) fn runs_without_database(conf: &Val) -> bool {
    is_ad_hoc_default_db(conf) || hot::db::get_db_uri_from_conf(conf).is_empty()
}

async fn handle_auto_deploy(
    conf: &Val,
    db: &hot::db::DatabasePool,
    build_result: &hot::build::BuildResult,
    user_id: uuid::Uuid,
) -> Result<(), String> {
    if !conf.get_bool("deploy.auto") {
        return Ok(());
    }

    match hot::db::Build::get_deployed_build_by_project(db, &build_result.build.project_id).await {
        Ok(Some(deployed_build)) => {
            // There's a currently deployed build - check if it's not live
            if !deployed_build.is_live() {
                info!(
                    "Current deployed build is not live (type: {}), switching to live build...",
                    deployed_build.build_type
                );

                match hot::db::Build::activate_build_directly(
                    db,
                    &build_result.build.build_id,
                    &user_id,
                )
                .await
                {
                    Ok(_) => {
                        info!("Successfully deployed live build");
                    }
                    Err(e) => {
                        warn!("Failed to deploy live build: {}", e);
                        // Don't fail the entire operation, just warn
                    }
                }
            }
        }
        Ok(_) => {
            // No deployed build - auto-deploy the live build
            info!("No build currently deployed, deploying live build...");

            match hot::db::Build::activate_build_directly(
                db,
                &build_result.build.build_id,
                &user_id,
            )
            .await
            {
                Ok(_) => {
                    info!("Successfully deployed live build");
                }
                Err(e) => {
                    warn!("Failed to deploy live build: {}", e);
                    // Don't fail the entire operation, just warn
                }
            }
        }
        Err(e) => {
            warn!("Failed to check deployed build status: {}", e);
            // Don't fail the entire operation, just warn
        }
    }

    Ok(())
}

// Helper function to create or update live build for development commands
pub(crate) async fn setup_live_build_for_dev(
    conf: &Val,
    global_options: &GlobalOptions,
    src_paths: &[String],
    test_paths: &[String],
) -> Result<(), String> {
    setup_live_build_for_dev_with_color(
        conf,
        global_options,
        src_paths,
        test_paths,
        hot::env::is_local_dev(),
    )
    .await
}

/// Set up a live build with an explicit diagnostic color policy.
///
/// Long-running commands that report failures through `tracing` must disable
/// color here. `tracing-subscriber` sanitizes embedded ANSI control sequences,
/// which otherwise turns an Ariadne report into visible `\x1b` escapes.
pub(crate) async fn setup_live_build_for_dev_with_color(
    conf: &Val,
    global_options: &GlobalOptions,
    src_paths: &[String],
    test_paths: &[String],
    color: bool,
) -> Result<(), String> {
    let secret_scan_opts = build_secret_scan_opts(conf, false);
    setup_live_build_for_dev_with_validation(
        conf,
        global_options,
        src_paths,
        test_paths,
        secret_scan_opts,
        DevLiveBuildValidation::Structural,
        color,
    )
    .await
}

/// Set up a live build with runtime metadata preflight using the context keys
/// that will be passed to the Execute pipeline.
pub(crate) async fn setup_live_build_for_dev_with_context(
    conf: &Val,
    global_options: &GlobalOptions,
    src_paths: &[String],
    test_paths: &[String],
    available_context_keys: ahash::AHashSet<String>,
) -> Result<(), String> {
    let secret_scan_opts = build_secret_scan_opts(conf, false);
    setup_live_build_for_dev_with_validation(
        conf,
        global_options,
        src_paths,
        test_paths,
        secret_scan_opts,
        DevLiveBuildValidation::RuntimePreflight(available_context_keys),
        hot::env::is_local_dev(),
    )
    .await
}

pub(crate) async fn setup_live_build_for_dev_with_secret_scan_opts(
    conf: &Val,
    global_options: &GlobalOptions,
    src_paths: &[String],
    test_paths: &[String],
    secret_scan_opts: hot::secret_scan::SecretScanOpts,
) -> Result<(), String> {
    setup_live_build_for_dev_with_validation(
        conf,
        global_options,
        src_paths,
        test_paths,
        secret_scan_opts,
        DevLiveBuildValidation::Structural,
        hot::env::is_local_dev(),
    )
    .await
}

enum DevLiveBuildValidation {
    Structural,
    RuntimePreflight(ahash::AHashSet<String>),
}

async fn setup_live_build_for_dev_with_validation(
    conf: &Val,
    global_options: &GlobalOptions,
    src_paths: &[String],
    test_paths: &[String],
    secret_scan_opts: hot::secret_scan::SecretScanOpts,
    validation: DevLiveBuildValidation,
    color: bool,
) -> Result<(), String> {
    let project_name = global_options
        .project
        .clone()
        .unwrap_or_else(|| hot::project::get_default_project_name(conf));

    // Ad hoc runs outside a project (no hot.hot / .hot) pointed at the
    // default project-local sqlite database can never produce a live build —
    // create_db_pool refuses to create .hot/db and profile resolution has no
    // rows. Skip the whole setup instead of paying for source scanning and
    // doomed connection attempts.
    if runs_without_database(conf) {
        tracing::debug!("Ad hoc run or no database configured, skipping live build setup");
        return Ok(());
    }

    let (resource_paths, respect_gitignore, resource_excludes) = resource_bundle_options(
        conf,
        &project_name,
        &global_options.resource_paths,
        global_options.no_gitignore,
    );

    let scan_context = hot::build::BuildContext::new(
        None,
        None,
        None,
        None,
        project_name.clone(),
        src_paths.to_vec(),
        test_paths.to_vec(),
    )
    .with_resources(
        resource_paths.clone(),
        respect_gitignore,
        resource_excludes.clone(),
    )
    .with_secret_scan_opts(secret_scan_opts.clone());

    hot::build::scan_build_inputs(&scan_context, Some(conf))?;

    let db_uri = hot::db::get_db_uri_from_conf(conf);

    tracing::debug!(
        "Database configured: {}, setting up live build",
        hot::db::redact_password(&db_uri)
    );

    // Connect to database
    let db = crate::profile::create_cli_db_pool(conf)
        .await
        .map_err(|e| format!("Cannot prepare live build against configured database: {e}"))?;

    // Preserve the selected identity; an unambiguous local database may omit a profile.
    let (user_id, env_id, org_id) = crate::profile::resolve_cli_identity(&db, conf).await?;

    // Create build context
    let build_context = hot::build::BuildContext::new(
        Some(user_id),
        Some(env_id),
        Some(org_id),
        None, // project_id will be generated
        project_name.clone(),
        src_paths.to_vec(),
        test_paths.to_vec(),
    )
    .with_resources(resource_paths, respect_gitignore, resource_excludes)
    .with_secret_scan_opts(secret_scan_opts);
    let build_context = match validation {
        DevLiveBuildValidation::Structural => build_context,
        DevLiveBuildValidation::RuntimePreflight(available_context_keys) => {
            build_context.with_runtime_preflight(available_context_keys, conf.clone())
        }
    };

    // Create live build
    match hot::build::setup_live_build_and_compiler(&db, build_context, color).await {
        Ok(build_result) => {
            tracing::info!(
                "Live build created for project '{}': build_id={}",
                project_name,
                build_result.build.build_id
            );

            // Auto-deploy the live build if enabled
            if let Err(e) = handle_auto_deploy(conf, &db, &build_result, user_id).await {
                return Err(format!("Auto-deploy failed: {}", e));
            }

            tracing::info!("Live build deployed successfully");
            Ok(())
        }
        Err(e) => Err(format!("Failed to create live build: {}", e)),
    }
}

pub(crate) async fn run_deploy(
    build_id_opt: Option<&str>,
    conf: &hot::val::Val,
    global_options: &GlobalOptions,
    local: bool,
    allow_secret_shape: bool,
    strict_flag: bool,
) -> Result<(), String> {
    // CLI flag wins; otherwise honor `hot.deploy.ctx.strict` so CI pipelines
    // can pin strict behavior without remembering the flag every time.
    let strict = strict_flag || conf.get_bool("deploy.ctx.strict");
    // If no build ID provided, create a new bundle build from current source
    let build_uuid = if let Some(build_id) = build_id_opt {
        Uuid::from_str(build_id).map_err(|_| format!("Invalid build ID format: {}", build_id))?
    } else {
        // Create a new bundle build from current source
        info!("No build ID provided, creating new bundle build from current source...");

        let db = crate::profile::create_cli_db_pool(conf)
            .await
            .map_err(|e| format!("Failed to connect to local database: {}", e))?;

        let project_name = global_options
            .project
            .clone()
            .unwrap_or_else(|| hot::project::get_default_project_name(conf));

        let src_paths = get_merged_src_paths(
            conf,
            global_options.project.as_deref(),
            &global_options.src_paths,
        );

        let (user_id, env_id, org_id) = crate::profile::resolve_cli_identity(&db, conf).await?;

        let secret_scan_opts = build_secret_scan_opts(conf, allow_secret_shape);
        let (resource_paths, respect_gitignore, resource_excludes) = resource_bundle_options(
            conf,
            &project_name,
            &global_options.resource_paths,
            global_options.no_gitignore,
        );

        let build_context = hot::build::BuildContext::new(
            Some(user_id),
            Some(env_id),
            Some(org_id),
            None, // Will generate new project_id if needed
            project_name,
            src_paths.clone(),
            Vec::new(), // No test paths in bundles
        )
        .with_resources(resource_paths, respect_gitignore, resource_excludes)
        .with_secret_scan_opts(secret_scan_opts);

        let build_result = hot::build::build_create(
            &db,
            Some(".hot/build"), // Use default build directory
            build_context,
            Some(conf),
        )
        .await
        .map_err(|e| format!("Failed to create bundle build: {}", e))?;

        println!("✓ Created bundle build {}", build_result.build.build_id);
        println!("  Size: {} bytes", build_result.build.size);

        // For non-local deployments, upload the bundle build first since the file
        // was created locally and needs to be available in the remote storage.
        // Without this, the worker won't be able to find the build file even though
        // the database record exists (when sharing the same database in local dev).
        if !local {
            let project_slug = remote_project_slug(conf, global_options, None).await;
            let remote_build_id =
                upload_via_api(build_result.build.build_id, conf, project_slug.clone()).await?;
            return deploy_via_api_only(remote_build_id, conf, project_slug).await;
        }

        build_result.build.build_id
    };

    if local {
        deploy_via_database(build_uuid, conf, strict).await
    } else {
        let project_slug = remote_project_slug(conf, global_options, Some(build_uuid)).await;
        deploy_via_api(build_uuid, conf, project_slug).await
    }
}

async fn deploy_via_api(build_uuid: Uuid, conf: &Val, project_slug: String) -> Result<(), String> {
    let api = ApiClient::from_config(conf)?;

    let path = format!("/v1/projects/{}/builds/{}/deploy", project_slug, build_uuid);

    info!("Deploying build {} via API...", build_uuid);

    #[derive(serde::Deserialize)]
    struct ApiResponse<T> {
        data: T,
    }

    #[derive(serde::Deserialize)]
    struct RuntimeWarningResponse {
        message: String,
    }

    #[derive(serde::Deserialize)]
    struct BuildResponse {
        #[allow(dead_code)]
        build_id: String,
        build_type: String,
        deployed: bool,
        runtime_status: String,
        hash: String,
        size: i64,
        runtime_warning: Option<RuntimeWarningResponse>,
    }

    #[derive(serde::Serialize)]
    struct CreateProjectRequest {
        name: String,
    }

    #[derive(serde::Deserialize)]
    struct ProjectResponse {
        #[allow(dead_code)]
        project_id: String,
        #[allow(dead_code)]
        name: String,
    }

    // Try to deploy the build
    let result: Result<ApiResponse<BuildResponse>, String> = api.post(&path).await;

    match result {
        Ok(response) => {
            if response.data.deployed {
                println!("✓ Successfully deployed build {}", build_uuid);
            } else {
                println!("✓ Accepted deployment for build {}", build_uuid);
            }
            println!("  Build type: {}", response.data.build_type);
            println!("  Runtime status: {}", response.data.runtime_status);
            println!("  Hash: {}", response.data.hash);
            println!("  Size: {} bytes", response.data.size);
            if let Some(warning) = response.data.runtime_warning {
                println!("  Warning: {}", warning.message);
            }
            Ok(())
        }
        Err(err) => {
            // Check if it's a 404 "Project not found" error
            let is_project_not_found = err.contains("404") && err.contains("Project not found");

            // Check if it's a "Build not found" error
            let is_build_not_found = err.contains("404") && err.contains("Build");

            if is_project_not_found {
                // Check if auto-create is enabled
                let auto_create_enabled = conf
                    .get("deploy")
                    .and_then(|d| d.get("auto_create_project"))
                    .and_then(|v| match v {
                        Val::Bool(b) => Some(b),
                        _ => None,
                    })
                    .unwrap_or(true); // Default to true

                if !auto_create_enabled {
                    return Err(format!(
                        "Project '{}' not found in remote environment.\n\
                         To auto-create projects, set deploy.auto_create_project = true in your configuration.",
                        project_slug
                    ));
                }

                // Auto-create the project
                info!("Project '{}' not found, creating it...", project_slug);

                let create_request = CreateProjectRequest {
                    name: project_slug.clone(),
                };

                let _project_response: ApiResponse<ProjectResponse> = api
                    .post_json("/v1/projects", &create_request)
                    .await
                    .map_err(|e| format!("Failed to create project '{}': {}", project_slug, e))?;

                println!(
                    "✓ Created new project '{}' in remote environment",
                    project_slug
                );

                // After creating the project, the build still doesn't exist remotely
                // So we need to upload it first, then deploy
                println!(
                    "Build {} not found in remote environment, uploading...",
                    build_uuid
                );

                let remote_build_id =
                    upload_via_api(build_uuid, conf, project_slug.clone()).await?;

                // Deploy the uploaded build
                info!("Deploying build...");
                let deploy_path = format!(
                    "/v1/projects/{}/builds/{}/deploy",
                    project_slug, remote_build_id
                );
                let response: ApiResponse<BuildResponse> = api.post(&deploy_path).await?;

                if response.data.deployed {
                    println!("✓ Successfully deployed build {}", remote_build_id);
                } else {
                    println!("✓ Accepted deployment for build {}", remote_build_id);
                }
                println!("  Build type: {}", response.data.build_type);
                println!("  Runtime status: {}", response.data.runtime_status);
                println!("  Hash: {}", response.data.hash);
                println!("  Size: {} bytes", response.data.size);
                if let Some(warning) = response.data.runtime_warning {
                    println!("  Warning: {}", warning.message);
                }
                Ok(())
            } else if is_build_not_found {
                // Check if auto-upload is enabled
                let auto_upload_enabled = conf
                    .get("deploy")
                    .and_then(|d| d.get("auto_upload_build"))
                    .and_then(|v| match v {
                        Val::Bool(b) => Some(b),
                        _ => None,
                    })
                    .unwrap_or(true); // Default to true

                if !auto_upload_enabled {
                    return Err(format!(
                        "Build {} not found in remote environment.\n\
                         Upload it first with: hot upload {}\n\
                         Or enable auto-upload with: deploy.auto_upload_build = true",
                        build_uuid, build_uuid
                    ));
                }

                // Auto-upload the build
                info!(
                    "Build {} not found in remote environment, uploading...",
                    build_uuid
                );

                let remote_build_id =
                    upload_via_api(build_uuid, conf, project_slug.clone()).await?;

                // Retry the deployment with the REMOTE build ID
                info!("Retrying deployment...");
                let deploy_path = format!(
                    "/v1/projects/{}/builds/{}/deploy",
                    project_slug, remote_build_id
                );
                let response: ApiResponse<BuildResponse> = api.post(&deploy_path).await?;

                if response.data.deployed {
                    println!("✓ Successfully deployed build {}", remote_build_id);
                } else {
                    println!("✓ Accepted deployment for build {}", remote_build_id);
                }
                println!("  Build type: {}", response.data.build_type);
                println!("  Runtime status: {}", response.data.runtime_status);
                println!("  Hash: {}", response.data.hash);
                println!("  Size: {} bytes", response.data.size);
                Ok(())
            } else {
                // Some other error, return it as-is
                Err(err)
            }
        }
    }
}

/// Deploy a build via API without auto-upload fallback.
/// Used when we've already uploaded the build and just need to mark it deployed.
async fn deploy_via_api_only(
    build_uuid: Uuid,
    conf: &Val,
    project_slug: String,
) -> Result<(), String> {
    let api = ApiClient::from_config(conf)?;

    let path = format!("/v1/projects/{}/builds/{}/deploy", project_slug, build_uuid);

    info!("Deploying build {} via API...", build_uuid);

    #[derive(serde::Deserialize)]
    struct ApiResponse<T> {
        data: T,
    }

    #[derive(serde::Deserialize)]
    struct RuntimeWarningResponse {
        message: String,
    }

    #[derive(serde::Deserialize)]
    struct BuildResponse {
        #[allow(dead_code)]
        build_id: String,
        build_type: String,
        deployed: bool,
        runtime_status: String,
        hash: String,
        size: i64,
        runtime_warning: Option<RuntimeWarningResponse>,
    }

    let response: ApiResponse<BuildResponse> = api.post(&path).await?;

    if response.data.deployed {
        println!("✓ Successfully deployed build {}", build_uuid);
    } else {
        println!("✓ Accepted deployment for build {}", build_uuid);
    }
    println!("  Build type: {}", response.data.build_type);
    println!("  Runtime status: {}", response.data.runtime_status);
    println!("  Hash: {}", response.data.hash);
    println!("  Size: {} bytes", response.data.size);
    if let Some(warning) = response.data.runtime_warning {
        println!("  Warning: {}", warning.message);
    }
    Ok(())
}

pub(crate) async fn run_upload(
    build_id: &str,
    conf: &hot::val::Val,
    global_options: &GlobalOptions,
) -> Result<(), String> {
    let build_uuid =
        Uuid::from_str(build_id).map_err(|_| format!("Invalid build ID format: {}", build_id))?;

    let project_slug = remote_project_slug(conf, global_options, Some(build_uuid)).await;
    upload_via_api(build_uuid, conf, project_slug)
        .await
        .map(|_| ())
}

/// The remote project a deploy or upload targets: `-p`, then the
/// `hot.project.slug` setting, then the local build's own project, then the
/// default project.
async fn remote_project_slug(
    conf: &Val,
    global_options: &GlobalOptions,
    build_uuid: Option<Uuid>,
) -> String {
    if let Some(project) = &global_options.project {
        return project.clone();
    }
    if let Some(slug) = conf.get("project").and_then(|p| p.get("slug")) {
        return slug.to_string();
    }
    if let Some(build_uuid) = build_uuid
        && let Ok(db) = crate::profile::create_cli_db_pool(conf).await
        && let Ok(build) = hot::db::Build::get_build(&db, &build_uuid).await
        && let Ok(project) = hot::db::Project::get_project(&db, &build.project_id).await
    {
        return project.name;
    }
    hot::project::get_default_project_name(conf)
}

async fn upload_via_api(
    build_uuid: Uuid,
    conf: &Val,
    project_slug: String,
) -> Result<Uuid, String> {
    let api = ApiClient::from_config(conf)?;

    info!("Uploading build {} to remote environment...", build_uuid);

    // Get the build from local database
    let db = crate::profile::create_cli_db_pool(conf)
        .await
        .map_err(|e| format!("Failed to connect to local database: {}", e))?;

    let build = hot::db::Build::get_build(&db, &build_uuid)
        .await
        .map_err(|_| format!("Build {} not found in local database", build_uuid))?;

    // Get profile info (user, org and env) for attribution and storage operations
    let (user_id, org_id, env_id) =
        if let Some((user_email, org_slug, env_name)) = extract_profile_identifiers(conf)? {
            match resolve_profile_to_uuids(&db, &user_email, &org_slug, &env_name).await {
                Ok((user_uuid, env_uuid, org_uuid)) => (user_uuid, org_uuid, env_uuid),
                Err(e) => return Err(format!("Failed to resolve profile: {}", e)),
            }
        } else {
            return Err("Profile configuration required for build upload".to_string());
        };
    crate::profile::ensure_build_environment(&db, &build_uuid, &env_id).await?;

    // Check if this is a live build
    let (actual_build_uuid, upload_build, build_data) = if build.is_live() {
        // Check if auto-bundle is enabled
        let auto_bundle_enabled = conf
            .get("deploy")
            .and_then(|d| d.get("auto_bundle_live_build"))
            .and_then(|v| match v {
                Val::Bool(b) => Some(b),
                _ => None,
            })
            .unwrap_or(true); // Default to true

        if !auto_bundle_enabled {
            return Err(format!(
                "Build {} is a live build and cannot be uploaded directly.\n\
                 Create a bundle build first with: hot build\n\
                 Or enable auto-bundling with: deploy.auto_bundle_live_build = true",
                build_uuid
            ));
        }

        // Get project to find source paths
        let project = hot::db::Project::get_project(&db, &build.project_id)
            .await
            .map_err(|e| format!("Failed to get project: {}", e))?;

        info!("Live build detected, creating bundle build from current sources...");

        // Get source paths from config
        let src_paths = conf
            .get("project")
            .and_then(|p| p.get("src"))
            .and_then(|s| match s {
                Val::Vec(v) => Some(
                    v.iter()
                        .filter_map(|x| match x {
                            Val::Str(s) => Some(s.to_string()),
                            _ => None,
                        })
                        .collect::<Vec<_>>(),
                ),
                Val::Str(s) => Some(vec![s.to_string()]),
                _ => None,
            })
            .unwrap_or_else(|| vec!["hot/src".to_string()]);

        // Create bundle build - we now have the profile's user, org and env.
        // Without a user, build_create falls back to the database's first
        // user, who can be someone else and whom a child org's scope may not
        // see at all.
        let build_context = hot::build::BuildContext::new(
            Some(user_id),
            Some(env_id),
            Some(org_id),
            None, // Will generate new project_id if needed
            project.name.clone(),
            src_paths.clone(),
            Vec::new(), // No test paths in bundles
        );

        let build_result = hot::build::build_create(
            &db,
            Some(".hot/build"), // Use default build directory
            build_context,
            Some(conf),
        )
        .await
        .map_err(|e| format!("Failed to create bundle build: {}", e))?;

        println!("✓ Created bundle build {}", build_result.build.build_id);
        println!("  Size: {} bytes", build_result.build.size);

        // Read the zip file directly from the path where it was created
        let build_data = std::fs::read(&build_result.zip_path)
            .map_err(|e| format!("Failed to read build file: {}", e))?;

        // Use the new bundle build for upload
        (build_result.build.build_id, build_result.build, build_data)
    } else {
        // Already a bundle build, use it as-is
        // First, try to read from the storage_path in the database record
        let build_data = if let Some(storage_path) = &build.storage_path {
            // Build was created locally, read from its stored path
            // storage_path is relative to build directory, need to prepend it
            let build_dir = conf
                .get("build")
                .and_then(|b| b.get("dir"))
                .map(|d| d.to_string())
                .unwrap_or_else(|| ".hot/build".to_string());

            let full_path = resolve_local_build_storage_path(&build_dir, storage_path)?;
            std::fs::read(&full_path)
                .map_err(|e| format!("Failed to read build from {}: {}", full_path.display(), e))?
        } else {
            // No storage path, try to retrieve from BuildStorage
            let storage = hot::storage::build_storage_from_config(conf)
                .await
                .map_err(|e| format!("Failed to create storage instance: {}", e))?;

            storage
                .retrieve_build(&build_uuid, &org_id, &env_id)
                .await
                .map_err(|e| format!("Failed to retrieve build from storage: {}", e))?
        };

        (build_uuid, build, build_data)
    };

    info!("Retrieving build file from local storage...");
    info!("Build file size: {} bytes", build_data.len());

    #[derive(serde::Deserialize)]
    struct ApiResponse<T> {
        data: T,
    }

    #[derive(serde::Deserialize)]
    struct BuildUploadResponse {
        build_id: String,
        hash: String,
        size: i32,
    }

    #[derive(serde::Serialize)]
    struct CreateProjectRequest {
        name: String,
    }

    #[derive(serde::Deserialize)]
    struct ProjectResponse {
        #[allow(dead_code)]
        project_id: String,
        #[allow(dead_code)]
        name: String,
    }

    // Upload the build
    let url = format!("{}/v1/projects/{}/builds", api.base_url, project_slug);

    let form = reqwest::multipart::Form::new()
        .part(
            "file",
            reqwest::multipart::Part::bytes(build_data.clone())
                .file_name(format!("{}.hot.zip", actual_build_uuid))
                .mime_str("application/zip")
                .map_err(|e| format!("Failed to set MIME type: {}", e))?,
        )
        .text("hash", upload_build.hash.clone())
        .text("build_id", actual_build_uuid.to_string()); // Send the build ID to the API

    info!("Uploading to remote API...");
    let response = api
        .client
        .post(&url)
        .header("Authorization", format!("Bearer {}", api.api_key))
        .multipart(form)
        .send()
        .await
        .map_err(|e| format!("Failed to upload build: {}", e))?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();

        // Check if it's a 404 "Project not found" error
        let is_project_not_found =
            status == reqwest::StatusCode::NOT_FOUND && body.contains("Project not found");

        if is_project_not_found {
            info!("Project '{}' not found, creating it...", project_slug);

            let create_request = CreateProjectRequest {
                name: project_slug.clone(),
            };

            let _project_response: ApiResponse<ProjectResponse> = api
                .post_json("/v1/projects", &create_request)
                .await
                .map_err(|e| format!("Failed to create project '{}': {}", project_slug, e))?;

            println!(
                "✓ Created new project '{}' in remote environment",
                project_slug
            );

            // Retry the upload
            info!("Retrying upload...");
            let form = reqwest::multipart::Form::new()
                .part(
                    "file",
                    reqwest::multipart::Part::bytes(build_data.clone())
                        .file_name(format!("{}.hot.zip", actual_build_uuid))
                        .mime_str("application/zip")
                        .map_err(|e| format!("Failed to set MIME type: {}", e))?,
                )
                .text("hash", upload_build.hash.clone());

            let response = api
                .client
                .post(&url)
                .header("Authorization", format!("Bearer {}", api.api_key))
                .multipart(form)
                .send()
                .await
                .map_err(|e| format!("Failed to upload build: {}", e))?;

            if !response.status().is_success() {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                return Err(format!("Upload failed ({}): {}", status, body));
            }

            let response_data: ApiResponse<BuildUploadResponse> = response
                .json()
                .await
                .map_err(|e| format!("Failed to parse response: {}", e))?;

            println!("✓ Successfully uploaded build {}", actual_build_uuid);
            println!("  Remote build ID: {}", response_data.data.build_id);
            println!("  Hash: {}", response_data.data.hash);
            println!("  Size: {} bytes", response_data.data.size);

            // Parse and return the remote build ID
            return Uuid::from_str(&response_data.data.build_id)
                .map_err(|e| format!("Failed to parse remote build ID: {}", e));
        } else {
            return Err(format!("Upload failed ({}): {}", status, body));
        }
    }

    // Success - check if build already existed
    let build_exists = response
        .headers()
        .get("x-build-exists")
        .and_then(|v| v.to_str().ok())
        .map(|v| v == "true")
        .unwrap_or(false);

    // Parse response
    let response_data: ApiResponse<BuildUploadResponse> = response
        .json()
        .await
        .map_err(|e| format!("Failed to parse response: {}", e))?;

    if build_exists {
        println!(
            "✓ Build {} already exists in remote environment (no upload needed)",
            actual_build_uuid
        );
        println!("  Hash: {}", response_data.data.hash);
        println!("  Size: {} bytes", response_data.data.size);
    } else {
        println!("✓ Successfully uploaded build {}", actual_build_uuid);
        println!("  Remote build ID: {}", response_data.data.build_id);
        println!("  Hash: {}", response_data.data.hash);
        println!("  Size: {} bytes", response_data.data.size);
    }

    // Parse and return the remote build ID
    Uuid::from_str(&response_data.data.build_id)
        .map_err(|e| format!("Failed to parse remote build ID: {}", e))
        .map(Ok)?
}

async fn deploy_via_database(build_uuid: Uuid, conf: &Val, strict: bool) -> Result<(), String> {
    // Extract and resolve profile IDs for execution context
    let (user_id, env_id, org_id) =
        if let Some((user_email, org_slug, env_name)) = extract_profile_identifiers(conf)? {
            let db_uri = conf.get_str("db.uri");
            if !db_uri.is_empty() {
                // Connect to database to resolve profile identifiers
                let db = match crate::profile::create_cli_db_pool(conf).await {
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
                return Err("Database URI is required for deploy command".to_string());
            }
        } else {
            return Err("Profile configuration is required for deploy command".to_string());
        };

    let deploying_user_id = user_id.ok_or("User ID is required for deploy command")?;
    let org_id = org_id.ok_or("Organization ID is required for deploy command")?;
    let env_id = env_id.ok_or("Environment ID is required for deploy command")?;

    // Create database connection
    let db = crate::profile::create_cli_db_pool(conf)
        .await
        .map_err(|e| format!("Failed to connect to database: {}", e))?;

    // Create build storage
    let storage = hot::storage::build_storage_from_config(conf)
        .await
        .map_err(|e| format!("Failed to create build storage: {}", e))?;

    // First, verify the build exists
    let build = match hot::db::Build::get_build(&db, &build_uuid).await {
        Ok(build) => build,
        Err(_) => return Err(format!("Build not found: {}", build_uuid)),
    };
    crate::profile::ensure_build_environment(&db, &build_uuid, &env_id).await?;

    // Validate requirements for bundle builds before deploying
    if build.is_bundle() {
        hot::build::validate_ctx_requirements_for_deploy(
            &db,
            &build_uuid,
            &build.project_id,
            &org_id,
            &env_id,
            storage.as_ref(),
            strict,
        )
        .await?;

        hot::build::validate_box_requirements_for_deploy(
            &db,
            &build_uuid,
            &org_id,
            &env_id,
            storage.as_ref(),
        )
        .await?;

        hot::build::validate_schedule_requirements_for_deploy(
            &db,
            &build_uuid,
            &org_id,
            &env_id,
            conf,
            storage.as_ref(),
        )
        .await?;
    }

    if build.is_bundle() {
        hot::db::Build::request_bundle_deployment(&db, &build_uuid, &deploying_user_id)
            .await
            .map_err(|e| format!("Failed to request bundle deployment: {}", e))?;

        println!("✓ Accepted bundle deployment: {}", build_uuid);
        println!("  Build type: {}", build.build_type);
        println!("  Build hash: {}", build.hash);
        println!("  Build size: {} bytes", build.size);
        println!("  Bundle will become active after worker preparation completes");
    } else {
        hot::db::Build::activate_build_directly(&db, &build_uuid, &deploying_user_id)
            .await
            .map_err(|e| format!("Failed to deploy live build: {}", e))?;

        println!("✓ Successfully deployed build: {}", build_uuid);
        println!("  Build type: {}", build.build_type);
        println!("  Build hash: {}", build.hash);
        println!("  Build size: {} bytes", build.size);
        println!("  Live build is now active - events will be processed from current source");
    }

    if build.is_bundle() {
        // Enqueue a DeploymentMessage so the worker prepares the bundle, loads
        // manifest runtime data, and performs final activation atomically.
        if let Err(e) = hot::lang::event::enqueue_deployment_message(conf, build_uuid).await {
            let runtime_error = format!("Failed to enqueue deployment message: {e}");
            let _ = hot::db::Build::mark_runtime_failed(&db, &build_uuid, &runtime_error).await;
            eprintln!(
                "  Warning: bundle deployment marked failed because worker message enqueue failed: {}",
                e
            );
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn remote_deploys_target_the_selected_project() {
        let conf = hot::val!({"set": {"project": "alpha"}, "project": {
            "alpha": {"src": {"paths": ["./alpha"]}},
            "beta": {"src": {"paths": ["./beta"]}}
        }});
        let beta = GlobalOptions {
            project: Some("beta".to_string()),
            ..Default::default()
        };
        assert_eq!(remote_project_slug(&conf, &beta, None).await, "beta");
        assert_eq!(
            remote_project_slug(&conf, &GlobalOptions::default(), None).await,
            "alpha"
        );
    }

    #[test]
    fn explicit_profile_never_uses_ad_hoc_database_fallback() {
        assert!(!is_ad_hoc_default_db(
            &hot::val!({"profile":{"set":"missing"}})
        ));
        assert!(!is_ad_hoc_default_db(
            &hot::val!({"db":{"uri":"postgres://localhost/example"}})
        ));
        assert!(!is_ad_hoc_default_db(
            &hot::val!({"uri":"postgresql://localhost/example"})
        ));
    }

    #[test]
    fn test_resolve_local_build_storage_path_allows_relative_child() {
        let path = resolve_local_build_storage_path(".hot/build", "org/env/build.zip").unwrap();
        assert_eq!(path, Path::new(".hot/build").join("org/env/build.zip"));
    }

    #[test]
    fn test_resolve_local_build_storage_path_rejects_parent_escape() {
        let err = resolve_local_build_storage_path(".hot/build", "../secret.txt").unwrap_err();
        assert!(err.contains("cannot escape"));
    }

    #[test]
    fn test_resolve_local_build_storage_path_rejects_absolute_path() {
        // On Windows "/etc/passwd" has no drive prefix, so is_absolute() is
        // false and the RootDir escape guard rejects it instead. Either way
        // the path must be refused.
        let err = resolve_local_build_storage_path(".hot/build", "/etc/passwd").unwrap_err();
        assert!(
            err.contains("relative") || err.contains("cannot escape"),
            "got: {err}"
        );
    }
}

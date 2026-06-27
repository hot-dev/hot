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
use crate::command::build::{build_secret_scan_opts, resource_bundle_options};
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
    let secret_scan_opts = build_secret_scan_opts(conf, false);
    setup_live_build_for_dev_with_secret_scan_opts(
        conf,
        global_options,
        src_paths,
        test_paths,
        secret_scan_opts,
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
    // Get project name before any DB work so local preflight checks still run
    // when a live build would otherwise be skipped.
    let project_name = global_options
        .project
        .clone()
        .unwrap_or_else(|| hot::project::get_default_project_name(conf));

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

    // Check if database is configured
    let db_uri = conf.get_str("db.uri");
    if db_uri.is_empty() {
        // No database configured - skip live build creation
        tracing::debug!("No database configured, skipping live build creation");
        return Ok(());
    }

    tracing::debug!("Database configured: {}, setting up live build", db_uri);

    // Extract profile identifiers
    let (user_email, org_slug, env_name) = match extract_profile_identifiers(conf) {
        Some(profile) => {
            tracing::debug!(
                "Profile configured: user={}, org={}, env={}",
                profile.0,
                profile.1,
                profile.2
            );
            profile
        }
        None => {
            tracing::debug!("No profile configured, skipping live build creation");
            return Ok(());
        }
    };

    // Connect to database
    let db = match hot::db::create_db_pool(conf).await {
        Ok(pool) => pool,
        Err(e) => {
            tracing::debug!("Failed to connect to database: {}, skipping live build", e);
            return Ok(());
        }
    };

    // Resolve profile identifiers to UUIDs
    // If resolution fails, skip live build (non-fatal for CLI usage without proper project setup)
    let (user_id, env_id, org_id) =
        match resolve_profile_to_uuids(&db, &user_email, &org_slug, &env_name).await {
            Ok(ids) => ids,
            Err(e) => {
                tracing::debug!(
                    "Profile resolution failed: {}, skipping live build creation",
                    e
                );
                return Ok(());
            }
        };

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

    // Create live build
    match hot::build::setup_live_build_and_compiler(
        &db,
        build_context,
        false, // enable_cache - disabled for now
        None,  // cache_format
        true,  // load_ctx_hot for CLI commands
        hot::env::is_local_dev(),
    )
    .await
    {
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

        let db = hot::db::create_db_pool(conf)
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

        // Get profile info (org_id and env_id) for build context
        let (org_id_opt, env_id_opt) =
            if let Some((user_email, org_slug, env_name)) = extract_profile_identifiers(conf) {
                match resolve_profile_to_uuids(&db, &user_email, &org_slug, &env_name).await {
                    Ok((_user_uuid, env_uuid, org_uuid)) => (Some(org_uuid), Some(env_uuid)),
                    Err(_) => (None, None),
                }
            } else {
                (None, None)
            };

        let secret_scan_opts = build_secret_scan_opts(conf, allow_secret_shape);
        let (resource_paths, respect_gitignore, resource_excludes) = resource_bundle_options(
            conf,
            &project_name,
            &global_options.resource_paths,
            global_options.no_gitignore,
        );

        let build_context = hot::build::BuildContext::new(
            None, // user_id - will be inferred from context
            env_id_opt,
            org_id_opt,
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
            let remote_build_id = upload_via_api(build_result.build.build_id, conf).await?;
            return deploy_via_api_only(remote_build_id, conf).await;
        }

        build_result.build.build_id
    };

    if local {
        deploy_via_database(build_uuid, conf, strict).await
    } else {
        deploy_via_api(build_uuid, conf).await
    }
}

async fn deploy_via_api(build_uuid: Uuid, conf: &Val) -> Result<(), String> {
    let api = ApiClient::from_config(conf)?;

    let project_slug = conf
        .get("project")
        .and_then(|p| p.get("slug"))
        .map(|s| s.to_string())
        .unwrap_or_else(|| hot::project::get_default_project_name(conf));

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

                let remote_build_id = upload_via_api(build_uuid, conf).await?;

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

                let remote_build_id = upload_via_api(build_uuid, conf).await?;

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
async fn deploy_via_api_only(build_uuid: Uuid, conf: &Val) -> Result<(), String> {
    let api = ApiClient::from_config(conf)?;

    let project_slug = conf
        .get("project")
        .and_then(|p| p.get("slug"))
        .map(|s| s.to_string())
        .unwrap_or_else(|| hot::project::get_default_project_name(conf));

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

pub(crate) async fn run_upload(build_id: &str, conf: &hot::val::Val) -> Result<(), String> {
    let build_uuid =
        Uuid::from_str(build_id).map_err(|_| format!("Invalid build ID format: {}", build_id))?;

    upload_via_api(build_uuid, conf).await.map(|_| ())
}

async fn upload_via_api(build_uuid: Uuid, conf: &Val) -> Result<Uuid, String> {
    let api = ApiClient::from_config(conf)?;

    let project_slug = conf
        .get("project")
        .and_then(|p| p.get("slug"))
        .map(|s| s.to_string())
        .unwrap_or_else(|| hot::project::get_default_project_name(conf));

    info!("Uploading build {} to remote environment...", build_uuid);

    // Get the build from local database
    let db = hot::db::create_db_pool(conf)
        .await
        .map_err(|e| format!("Failed to connect to local database: {}", e))?;

    let build = hot::db::Build::get_build(&db, &build_uuid)
        .await
        .map_err(|_| format!("Build {} not found in local database", build_uuid))?;

    // Get profile info (org_id and env_id) for storage operations
    let (org_id, env_id) =
        if let Some((user_email, org_slug, env_name)) = extract_profile_identifiers(conf) {
            match resolve_profile_to_uuids(&db, &user_email, &org_slug, &env_name).await {
                Ok((_user_uuid, env_uuid, org_uuid)) => (org_uuid, env_uuid),
                Err(e) => return Err(format!("Failed to resolve profile: {}", e)),
            }
        } else {
            return Err("Profile configuration required for build upload".to_string());
        };

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

        // Create bundle build - we now have org_id and env_id from earlier
        let build_context = hot::build::BuildContext::new(
            None, // user_id - will be inferred from context
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
        if let Some((user_email, org_slug, env_name)) = extract_profile_identifiers(conf) {
            let db_uri = conf.get_str("db.uri");
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
                return Err("Database URI is required for deploy command".to_string());
            }
        } else {
            return Err("Profile configuration is required for deploy command".to_string());
        };

    let deploying_user_id = user_id.ok_or("User ID is required for deploy command")?;
    let org_id = org_id.ok_or("Organization ID is required for deploy command")?;
    let env_id = env_id.ok_or("Environment ID is required for deploy command")?;

    // Create database connection
    let db = hot::db::create_db_pool(conf)
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
        let err = resolve_local_build_storage_path(".hot/build", "/etc/passwd").unwrap_err();
        assert!(err.contains("relative"));
    }
}

//! `hot builds` — list builds for a project (locally or via remote API).

use hot::val::Val;

use crate::profile::{extract_profile_identifiers, resolve_profile_to_uuids};
use crate::remote::ApiClient;

fn print_builds_table(builds_with_projects: Vec<(hot::db::Build, Option<hot::db::Project>)>) {
    if builds_with_projects.is_empty() {
        println!("  No builds found");
        return;
    }

    // Print header
    println!();
    println!(
        "{:<16} {:<38} {:<10} {:<10} {:>10} {:>19}",
        "PROJECT", "BUILD_ID", "DEPLOYED", "BUILD_TYPE", "SIZE_BYTES", "CREATED_AT"
    );
    println!("{}", "-".repeat(108));

    for (build, project_opt) in builds_with_projects {
        let deployed_status = if build.deployed { "YES" } else { "NO" };
        let project_name = project_opt
            .map(|p| p.name)
            .unwrap_or_else(|| "unknown".to_string());
        println!(
            "{:<16} {:<38} {:<10} {:<10} {:>10} {:>19}",
            project_name,
            build.build_id.to_string(),
            deployed_status.to_string(),
            build.build_type.to_string(),
            build.size.to_string(),
            build.created_at.format("%Y-%m-%d %H:%M:%S")
        );
    }
}

pub(crate) async fn run_builds(
    project: Option<&str>,
    limit: Option<i64>,
    offset: Option<i64>,
    conf: &hot::val::Val,
    local: bool,
) -> Result<(), String> {
    if local {
        run_builds_local(project, limit, offset, conf).await
    } else {
        run_builds_remote(project, limit, offset, conf).await
    }
}

async fn run_builds_remote(
    project: Option<&str>,
    limit: Option<i64>,
    offset: Option<i64>,
    conf: &Val,
) -> Result<(), String> {
    let api = ApiClient::from_config(conf)?;

    let limit = limit.unwrap_or(20);
    let offset = offset.unwrap_or(0);

    match project {
        Some(project_slug) => {
            // Project-specific builds
            let path = format!(
                "/v1/projects/{}/builds?limit={}&offset={}",
                project_slug, limit, offset
            );

            #[derive(serde::Deserialize)]
            struct ApiResponse<T> {
                data: T,
            }

            #[derive(serde::Deserialize)]
            struct BuildResponse {
                #[allow(dead_code)]
                build_id: String,
                #[allow(dead_code)]
                hash: String,
                size: i64,
                build_type: String,
                deployed: bool,
                created_at: String,
            }

            let response: ApiResponse<Vec<BuildResponse>> = api.get(&path).await?;

            if response.data.is_empty() {
                println!("No builds found for project '{}'", project_slug);
                return Ok(());
            }

            println!(
                "Builds for project '{}' (remote: {}):",
                project_slug, api.base_url
            );
            println!();
            println!(
                "{:<16} {:<38} {:<10} {:<10} {:>10} {:>19}",
                "PROJECT", "BUILD_ID", "DEPLOYED", "BUILD_TYPE", "SIZE_BYTES", "CREATED_AT"
            );
            println!("{}", "-".repeat(108));

            for build in response.data {
                let deployed_status = if build.deployed { "YES" } else { "NO" };
                println!(
                    "{:<16} {:<38} {:<10} {:<10} {:>10} {:>19}",
                    project_slug,
                    build.build_id,
                    deployed_status,
                    build.build_type,
                    build.size,
                    build.created_at
                );
            }
        }
        None => {
            // All builds across projects in the environment
            let path = format!("/v1/builds?limit={}&offset={}", limit, offset);

            #[derive(serde::Deserialize)]
            struct ApiResponse<T> {
                data: T,
            }

            #[derive(serde::Deserialize)]
            struct BuildWithProjectResponse {
                #[allow(dead_code)]
                build_id: String,
                project_name: String,
                #[allow(dead_code)]
                hash: String,
                size: i64,
                build_type: String,
                deployed: bool,
                created_at: String,
            }

            let response: ApiResponse<Vec<BuildWithProjectResponse>> = api.get(&path).await?;

            if response.data.is_empty() {
                println!("No builds found");
                return Ok(());
            }

            println!("All builds (remote: {}):", api.base_url);
            println!();
            println!(
                "{:<16} {:<38} {:<10} {:<10} {:>10} {:>19}",
                "PROJECT", "BUILD_ID", "DEPLOYED", "BUILD_TYPE", "SIZE_BYTES", "CREATED_AT"
            );
            println!("{}", "-".repeat(108));

            for build in response.data {
                let deployed_status = if build.deployed { "YES" } else { "NO" };
                println!(
                    "{:<16} {:<38} {:<10} {:<10} {:>10} {:>19}",
                    build.project_name,
                    build.build_id,
                    deployed_status,
                    build.build_type,
                    build.size,
                    build.created_at
                );
            }
        }
    }

    Ok(())
}

async fn run_builds_local(
    bundle_name: Option<&str>,
    limit: Option<i64>,
    offset: Option<i64>,
    conf: &hot::val::Val,
) -> Result<(), String> {
    let db_uri = conf.get_str("db.uri");

    // Extract and resolve profile IDs for execution context
    let (_user_id, env_id, _org_id) =
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

    // Check if we have required profile context
    let env_id = match env_id {
        Some(id) => id,
        None => {
            return Err(
                "No environment context found. Please ensure your profile is configured."
                    .to_string(),
            );
        }
    };

    // Create database connection
    let db = hot::db::create_db_pool(conf)
        .await
        .map_err(|e| format!("Failed to connect to database: {}", e))?;

    // If bundle_name is specified, get that specific bundle and list its builds
    if let Some(bundle_name) = bundle_name {
        // First find the project by environment and name
        let project =
            match hot::db::Project::get_project_by_env_and_name(&db, &env_id, bundle_name).await {
                Ok(Some(project)) => project,
                Ok(None) => {
                    return Err(format!(
                        "Project '{}' not found in current environment",
                        bundle_name
                    ));
                }
                Err(e) => {
                    return Err(format!("Failed to get project '{}': {}", bundle_name, e));
                }
            };

        // Get builds for this project
        let builds =
            match hot::db::Build::get_builds_by_project(&db, &project.project_id, limit, offset)
                .await
            {
                Ok(builds) => builds,
                Err(e) => {
                    return Err(format!(
                        "Failed to get builds for project '{}': {}",
                        bundle_name, e
                    ));
                }
            };

        println!("Builds for bundle '{}':", bundle_name);
        let builds_with_projects: Vec<(hot::db::Build, Option<hot::db::Project>)> = builds
            .into_iter()
            .map(|b| (b, Some(project.clone())))
            .collect();
        print_builds_table(builds_with_projects);
    } else {
        // Get all builds for the environment (across all bundles)
        let builds = match hot::db::Build::get_recent_builds(&db, limit).await {
            Ok(builds) => builds,
            Err(e) => {
                return Err(format!("Failed to get recent builds: {}", e));
            }
        };

        // Filter builds to only those belonging to bundles in the current environment
        let mut filtered_builds = Vec::new();
        for build in builds {
            // Get the bundle for this build to check if it belongs to current environment
            // Get the project for this build to check environment
            match hot::db::Project::get_project(&db, &build.project_id).await {
                Ok(project) if project.env_id == env_id => {
                    filtered_builds.push((build, project));
                }
                Ok(_) => {
                    // Project belongs to different environment, skip
                }
                Err(e) => {
                    eprintln!(
                        "Warning: Failed to get project for build {}: {}",
                        build.build_id, e
                    );
                }
            }
        }

        println!("Recent builds in current environment:");
        let builds_with_projects: Vec<(hot::db::Build, Option<hot::db::Project>)> = filtered_builds
            .into_iter()
            .map(|(b, p)| (b, Some(p)))
            .collect();
        print_builds_table(builds_with_projects);
    }

    Ok(())
}

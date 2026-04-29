//! `hot projects` and `hot project <action>` — list and toggle projects.

use hot::val::Val;

use crate::cli::ProjectAction;
use crate::profile::{extract_profile_identifiers, resolve_profile_to_uuids};
use crate::remote::ApiClient;

pub(crate) async fn run_projects(
    limit: Option<i64>,
    offset: Option<i64>,
    conf: &hot::val::Val,
    local: bool,
) -> Result<(), String> {
    if local {
        run_projects_local(limit, offset, conf).await
    } else {
        run_projects_remote(limit, offset, conf).await
    }
}

async fn run_projects_remote(
    limit: Option<i64>,
    offset: Option<i64>,
    conf: &Val,
) -> Result<(), String> {
    let api = ApiClient::from_config(conf)?;

    let limit = limit.unwrap_or(20);
    let offset = offset.unwrap_or(0);

    let path = format!("/v1/projects?limit={}&offset={}", limit, offset);

    #[derive(serde::Deserialize)]
    struct ApiResponse<T> {
        data: T,
    }

    #[derive(serde::Deserialize)]
    struct ProjectResponse {
        project_id: String,
        name: String,
        active: bool,
        created_at: String,
    }

    let response: ApiResponse<Vec<ProjectResponse>> = api.get(&path).await?;

    if response.data.is_empty() {
        println!("No projects found");
        return Ok(());
    }

    println!("Projects (remote: {}):", api.base_url);
    println!();
    println!(
        "{:<38} {:<32} {:<8} {:>19}",
        "PROJECT_ID", "NAME", "ACTIVE", "CREATED_AT"
    );
    println!("{}", "-".repeat(100));

    for project in response.data {
        let active_status = if project.active { "YES" } else { "NO" };
        println!(
            "{:<38} {:<32} {:<8} {:>19}",
            project.project_id, project.name, active_status, project.created_at
        );
    }

    Ok(())
}

async fn run_projects_local(
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

    // Get projects for this environment
    let projects = match hot::db::Project::get_projects_by_env(&db, &env_id, limit, offset).await {
        Ok(projects) => projects,
        Err(e) => {
            return Err(format!("Failed to get projects: {}", e));
        }
    };

    if projects.is_empty() {
        println!("No projects found in current environment");
        return Ok(());
    }

    println!("Projects in current environment:");
    println!();
    println!(
        "{:<38} {:<32} {:<8} {:>19}",
        "PROJECT_ID", "NAME", "ACTIVE", "CREATED_AT"
    );
    println!("{}", "-".repeat(100));

    for project in projects {
        let active_status = if project.active { "YES" } else { "NO" };
        let created_at = project.created_at.format("%Y-%m-%d %H:%M:%S").to_string();
        println!(
            "{:<38} {:<32} {:<8} {:>19}",
            project.project_id, project.name, active_status, created_at
        );
    }

    Ok(())
}

pub(crate) async fn run_project_action(
    action: &ProjectAction,
    conf: &hot::val::Val,
    local: bool,
) -> Result<(), String> {
    match action {
        ProjectAction::Activate { project_name } => {
            run_project_toggle_active(project_name, true, conf, local).await
        }
        ProjectAction::Deactivate { project_name } => {
            run_project_toggle_active(project_name, false, conf, local).await
        }
    }
}

async fn run_project_toggle_active(
    project_name: &str,
    active: bool,
    conf: &hot::val::Val,
    local: bool,
) -> Result<(), String> {
    if local {
        run_project_toggle_active_local(project_name, active, conf).await
    } else {
        run_project_toggle_active_remote(project_name, active, conf).await
    }
}

async fn run_project_toggle_active_remote(
    project_name: &str,
    active: bool,
    conf: &Val,
) -> Result<(), String> {
    let api = ApiClient::from_config(conf)?;

    let verb = if active { "activate" } else { "deactivate" };
    let path = format!("/v1/projects/{}/{}", project_name, verb);

    #[derive(serde::Deserialize)]
    struct ApiResponse<T> {
        data: T,
    }

    #[derive(serde::Deserialize)]
    struct ProjectActivateResponse {
        project: ProjectInfo,
        #[serde(default)]
        redeployed_build_id: Option<String>,
    }

    #[derive(serde::Deserialize)]
    struct ProjectInfo {
        #[allow(dead_code)]
        project_id: String,
        name: String,
        active: bool,
    }

    let response: ApiResponse<ProjectActivateResponse> = api.post(&path).await?;
    let action = if response.data.project.active {
        "activated"
    } else {
        "deactivated"
    };
    println!(
        "Project '{}' has been {} (remote: {})",
        response.data.project.name, action, api.base_url
    );

    if active {
        match response.data.redeployed_build_id {
            Some(build_id) => println!("  Queued redeploy of latest build: {}", build_id),
            None => println!("  No builds found for this project; nothing to redeploy."),
        }
    }

    Ok(())
}

async fn run_project_toggle_active_local(
    project_name: &str,
    active: bool,
    conf: &hot::val::Val,
) -> Result<(), String> {
    let db_uri = conf.get_str("db.uri");

    // Extract and resolve profile IDs for execution context
    let (user_id, env_id, _org_id) =
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

    let user_id = match user_id {
        Some(id) => id,
        None => {
            return Err(
                "No user context found. Please ensure your profile is configured.".to_string(),
            );
        }
    };

    // Create database connection
    let db = hot::db::create_db_pool(conf)
        .await
        .map_err(|e| format!("Failed to connect to database: {}", e))?;

    // Get the project by name
    let project =
        match hot::db::Project::get_project_by_env_and_name(&db, &env_id, project_name).await {
            Ok(Some(project)) => project,
            Ok(None) => {
                return Err(format!("Project '{}' not found", project_name));
            }
            Err(e) => {
                return Err(format!("Failed to get project: {}", e));
            }
        };

    // Toggle the active status
    if let Err(e) =
        hot::db::Project::toggle_active(&db, &project.project_id, active, &user_id).await
    {
        return Err(format!("Failed to update project: {}", e));
    }

    let action = if active { "activated" } else { "deactivated" };
    println!("Project '{}' has been {}", project_name, action);

    // On reactivation, redeploy the latest build so schedules / event handlers
    // / MCP tools / webhooks / agents come back online (deactivation tore those
    // down). See enqueue_redeploy_for_project_reactivation for full rationale.
    if active {
        match hot::lang::event::enqueue_redeploy_for_project_reactivation(
            &db,
            conf,
            &project.project_id,
            &user_id,
        )
        .await
        {
            Ok(Some(build_id)) => {
                println!("  Queued redeploy of latest build: {}", build_id);
            }
            Ok(None) => {
                println!("  No builds found for this project; nothing to redeploy.");
            }
            Err(e) => {
                eprintln!(
                    "  Warning: project reactivated but failed to enqueue redeploy: {}",
                    e
                );
            }
        }
    }

    Ok(())
}

use ahash::AHashMap;
use askama::Template;
use axum::{
    extract::{Path, Query, State},
    response::{Html, IntoResponse, Redirect},
};
use hot::db::{Build, Context, DatabasePool, Project};
use hot::val::Val;
use std::sync::Arc;
use uuid::Uuid;

use crate::handlers::list_query;
use crate::{auth::Session, templates};

pub async fn projects_list_handler(
    State(db): State<Arc<DatabasePool>>,
    Query(params): Query<AHashMap<String, String>>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> impl IntoResponse {
    // Parse query parameters
    const PROJECTS_PER_PAGE: i64 = 10;
    let page = list_query::PageParams::parse(&params, PROJECTS_PER_PAGE);
    let current_page_num = page.current_page_num;
    let offset = page.offset;

    let env_id = match session.current_env_id() {
        Some(id) => id,
        None => {
            // Return empty projects list if no environment selected
            let mut breadcrumbs = templates::build_base_breadcrumbs_with_env(&session);
            breadcrumbs.push(templates::BreadcrumbItem::current("Projects".to_string()));

            let template = templates::ProjectsList {
                title: "Projects",
                page_context: templates::PrivatePageContext::with_breadcrumbs(
                    "projects",
                    &session,
                    breadcrumbs,
                ),
                projects: vec![],
                current_page_num: 1,
                total_pages: 1,
                start_page: 1,
                end_page: 1,
                has_next_page: false,
                has_prev_page: false,
                total_projects: 0,
                search_query: String::new(),
                selected_time_range: "all".to_string(),
            };
            return Html(template.render().unwrap());
        }
    };

    let (projects, total_projects) =
        match Project::get_projects_by_env(&db, &env_id, Some(PROJECTS_PER_PAGE), Some(offset))
            .await
        {
            Ok(projects) => {
                // Get total count
                let all_projects = Project::get_projects_by_env(&db, &env_id, None, None)
                    .await
                    .unwrap_or_else(|e| {
                        tracing::error!(
                            "Failed to get total project count for env {}: {}",
                            env_id,
                            e
                        );
                        Vec::new()
                    });
                let total = all_projects.len() as i64;
                (projects, total)
            }
            Err(e) => {
                tracing::error!("Error fetching projects for env {}: {}", env_id, e);
                // Return empty projects list on database error
                let mut breadcrumbs = templates::build_base_breadcrumbs_with_env(&session);
                breadcrumbs.push(templates::BreadcrumbItem::current("Projects".to_string()));

                let template = templates::ProjectsList {
                    title: "Projects",
                    page_context: templates::PrivatePageContext::with_breadcrumbs(
                        "projects",
                        &session,
                        breadcrumbs,
                    ),
                    projects: vec![],
                    current_page_num: 1,
                    total_pages: 1,
                    start_page: 1,
                    end_page: 1,
                    has_next_page: false,
                    has_prev_page: false,
                    total_projects: 0,
                    search_query: String::new(),
                    selected_time_range: "all".to_string(),
                };
                return Html(template.render().unwrap());
            }
        };

    // Build ProjectSummary objects with additional data
    let mut project_summaries = Vec::new();
    for project in projects {
        // Get builds count
        let builds_count = Build::get_count_by_project(&db, &project.project_id)
            .await
            .unwrap_or_else(|e| {
                tracing::error!(
                    "Failed to get builds count for project {}: {}",
                    project.project_id,
                    e
                );
                0
            });

        // Get active/deployed build
        let active_build_id =
            match Build::get_deployed_build_by_project(&db, &project.project_id).await {
                Ok(Some(build)) => Some(build.build_id),
                _ => None,
            };

        // Get context vars count
        let context_vars_count = Context::get_count_by_project(&db, &project.project_id)
            .await
            .unwrap_or_else(|e| {
                tracing::error!(
                    "Failed to get context vars count for project {}: {}",
                    project.project_id,
                    e
                );
                0
            });

        project_summaries.push(templates::ProjectSummary {
            project_id: project.project_id,
            name: project.name,
            created_at: project.created_at,
            active: project.active,
            builds_count,
            active_build_id,
            context_vars_count,
        });
    }

    // Calculate pagination info
    let pagination = list_query::PaginationWindow::new(total_projects, &page);
    let total_pages = pagination.total_pages;
    let has_next_page = pagination.has_next_page;
    let has_prev_page = pagination.has_prev_page;
    let start_page = pagination.start_page;
    let end_page = pagination.end_page;

    let mut breadcrumbs = templates::build_base_breadcrumbs_with_env(&session);
    breadcrumbs.push(templates::BreadcrumbItem::current("Projects".to_string()));

    let template = templates::ProjectsList {
        title: "Projects",
        page_context: templates::PrivatePageContext::with_breadcrumbs(
            "projects",
            &session,
            breadcrumbs,
        ),
        projects: project_summaries,
        current_page_num,
        total_pages,
        start_page,
        end_page,
        has_next_page,
        has_prev_page,
        total_projects,
        search_query: String::new(),
        selected_time_range: "all".to_string(),
    };

    Html(template.render().unwrap())
}

pub async fn projects_detail_handler(
    Path(project_name): Path<String>,
    State(db): State<Arc<DatabasePool>>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> impl IntoResponse {
    let env_id = match session.current_env_id() {
        Some(id) => id,
        None => {
            let template = templates::ProjectsNotFound {
                title: "Project Not Found",
                page_context: templates::PrivatePageContext::new("projects", &session),
                project_name: project_name.clone(),
            };
            return Html(template.render().unwrap());
        }
    };

    let project = match Project::get_project_by_env_and_name(&db, &env_id, &project_name).await {
        Ok(Some(project)) => project,
        Ok(None) => {
            let template = templates::ProjectsNotFound {
                title: "Project Not Found",
                page_context: templates::PrivatePageContext::new("projects", &session),
                project_name: project_name.clone(),
            };
            return Html(template.render().unwrap());
        }
        Err(e) => {
            tracing::error!(
                "Error fetching project {} for env {}: {}",
                project_name,
                env_id,
                e
            );
            let template = templates::ProjectsNotFound {
                title: "Project Not Found",
                page_context: templates::PrivatePageContext::new("projects", &session),
                project_name: project_name.clone(),
            };
            return Html(template.render().unwrap());
        }
    };

    // Fetch deployed build information
    let (
        has_deployed_build,
        deployed_build_id,
        deployed_build_type,
        deployed_build_hash,
        deployed_build_size,
        deployed_build_updated_at,
    ) = match Build::get_deployed_build_by_project(&db, &project.project_id).await {
        Ok(Some(build)) => (
            true,
            build.build_id.to_string(),
            build.build_type,
            build.hash,
            build.size,
            format!(
                "{} {}",
                crate::timezone::format_in_timezone(
                    &build.updated_at,
                    &session.display_timezone,
                    "%Y-%m-%d %H:%M:%S"
                ),
                &session.timezone_abbreviation
            ),
        ),
        Ok(None) => (
            false,
            String::new(),
            String::new(),
            String::new(),
            0,
            String::new(),
        ),
        Err(e) => {
            tracing::error!(
                "Error fetching deployed build for project {} (project_id: {}): {}",
                project_name,
                project.project_id,
                e
            );
            (
                false,
                String::new(),
                String::new(),
                String::new(),
                0,
                String::new(),
            )
        }
    };

    let mut breadcrumbs = templates::build_base_breadcrumbs_with_env(&session);
    breadcrumbs.push(templates::BreadcrumbItem::clickable(
        "Projects".to_string(),
        "/projects".to_string(),
    ));
    breadcrumbs.push(templates::BreadcrumbItem::current(project.name.clone()));

    let template = templates::ProjectsDetail {
        title: &format!("Project: {}", project.name),
        page_context: templates::PrivatePageContext::with_breadcrumbs(
            "projects",
            &session,
            breadcrumbs,
        ),
        project,
        has_deployed_build,
        deployed_build_id,
        deployed_build_type,
        deployed_build_hash,
        deployed_build_size,
        deployed_build_updated_at,
    };

    Html(template.render().unwrap())
}

pub async fn projects_builds_handler(
    Path(project_name): Path<String>,
    Query(params): Query<AHashMap<String, String>>,
    State(db): State<Arc<DatabasePool>>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> impl IntoResponse {
    let env_id = match session.current_env_id() {
        Some(id) => id,
        None => {
            let template = templates::ProjectsNotFound {
                title: "Project Not Found",
                page_context: templates::PrivatePageContext::new("projects", &session),
                project_name: project_name.clone(),
            };
            return Html(template.render().unwrap());
        }
    };

    let project = match Project::get_project_by_env_and_name(&db, &env_id, &project_name).await {
        Ok(Some(project)) => project,
        Ok(None) => {
            // Project not found in current env — if a build_id was provided (e.g. from
            // an alert email link), resolve build -> project -> env to find the right env
            if let Some(build_id_str) = params.get("build")
                && let Ok(build_id) = Uuid::parse_str(build_id_str)
                && let Ok(build) = Build::get_build(&db, &build_id).await
                && let Ok(project) = Project::get_project(&db, &build.project_id).await
                && project.env_id != env_id
                && session.has_env_access(&project.env_id)
            {
                let env_name = session
                    .current_org_envs
                    .iter()
                    .find(|e| e.env_id == project.env_id)
                    .map(|e| e.name.as_str())
                    .unwrap_or("another environment");
                let switch_url = format!(
                    "/envs/{}/switch?redirect=/projects/{}/builds",
                    project.env_id, project_name
                );
                let template = templates::EnvSwitchPrompt {
                    title: "Switch Environment",
                    page_context: templates::PrivatePageContext::new("projects", &session),
                    message: format!(
                        "The project \"{}\" belongs to the \"{}\" environment. Switch to view it.",
                        project_name, env_name
                    ),
                    switch_url,
                    back_url: "/projects".to_string(),
                    back_label: "Back to Projects".to_string(),
                };
                return Html(template.render().unwrap());
            }

            let template = templates::ProjectsNotFound {
                title: "Project Not Found",
                page_context: templates::PrivatePageContext::new("projects", &session),
                project_name: project_name.clone(),
            };
            return Html(template.render().unwrap());
        }
        Err(e) => {
            tracing::error!(
                "Error fetching project {} for env {}: {}",
                project_name,
                env_id,
                e
            );
            let template = templates::ProjectsNotFound {
                title: "Project Not Found",
                page_context: templates::PrivatePageContext::new("projects", &session),
                project_name: project_name.clone(),
            };
            return Html(template.render().unwrap());
        }
    };

    let builds = match Build::get_builds_by_project(&db, &project.project_id, None, None).await {
        Ok(builds) => builds,
        Err(e) => {
            tracing::error!(
                "Error fetching builds for project {}: {}",
                project.project_id,
                e
            );
            let template = templates::ProjectsNotFound {
                title: "Project Not Found",
                page_context: templates::PrivatePageContext::new("projects", &session),
                project_name: project_name.clone(),
            };
            return Html(template.render().unwrap());
        }
    };

    let mut breadcrumbs = templates::build_base_breadcrumbs_with_env(&session);
    breadcrumbs.push(templates::BreadcrumbItem::clickable(
        "Projects".to_string(),
        "/projects".to_string(),
    ));
    breadcrumbs.push(templates::BreadcrumbItem::clickable(
        project.name.clone(),
        format!("/projects/{}", project.name),
    ));
    breadcrumbs.push(templates::BreadcrumbItem::current("Builds".to_string()));

    let template = templates::ProjectsBuilds {
        title: &format!("Project: {} - Builds", project.name),
        page_context: templates::PrivatePageContext::with_breadcrumbs(
            "projects",
            &session,
            breadcrumbs,
        ),
        project,
        builds,
    };

    Html(template.render().unwrap())
}

pub async fn projects_deploy_build_handler(
    Path((project_name, build_id)): Path<(String, Uuid)>,
    State(db): State<Arc<DatabasePool>>,
    State(conf): State<Val>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> impl IntoResponse {
    let redirect_to_builds =
        || Redirect::to(&format!("/projects/{}/builds", project_name)).into_response();

    let env_id = match session.current_env_id() {
        Some(id) => id,
        None => {
            return Redirect::to("/projects").into_response();
        }
    };

    // Verify the project exists and user has access
    let project = match Project::get_project_by_env_and_name(&db, &env_id, &project_name).await {
        Ok(Some(project)) => project,
        Ok(None) | Err(_) => {
            return Redirect::to("/projects").into_response();
        }
    };

    // Verify the build exists and belongs to this project
    let build = match Build::get_build(&db, &build_id).await {
        Ok(build) => build,
        Err(_) => {
            return redirect_to_builds();
        }
    };

    // Verify the build belongs to this project
    if build.project_id != project.project_id {
        return redirect_to_builds();
    }

    // Mark the build deployed in the database.
    if let Err(e) = Build::deploy_build(&db, &build_id, &session.current_user_id()).await {
        tracing::warn!(
            "UI deploy of build {} failed during deploy_build: {}",
            build_id,
            e
        );
        return redirect_to_builds();
    }

    // Enqueue a DeploymentMessage so the worker re-runs the full deploy pipeline
    // (load_build_manifest_data) — this is what reactivates schedules and
    // refreshes event handlers / MCP tools / webhooks / agents from the bundle's
    // manifest.hot. Without this, deploying from the UI after a project
    // deactivate/reactivate leaves schedules stuck at active = false and
    // worker-side handler caches stale; the CLI deploy works because the API
    // handler does this same enqueue.
    if let Err(e) = hot::lang::event::enqueue_deployment_message(&conf, build_id).await {
        tracing::error!(
            "UI deploy of build {} succeeded in DB but failed to enqueue deployment message: {}",
            build_id,
            e
        );
    }

    redirect_to_builds()
}

pub async fn projects_toggle_active_handler(
    Path(project_name): Path<String>,
    State(db): State<Arc<DatabasePool>>,
    State(conf): State<Val>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> impl IntoResponse {
    let env_id = match session.current_env_id() {
        Some(id) => id,
        None => {
            return Redirect::to("/projects");
        }
    };

    // Get the project
    let project = match Project::get_project_by_env_and_name(&db, &env_id, &project_name).await {
        Ok(Some(project)) => project,
        Ok(None) | Err(_) => {
            return Redirect::to("/projects");
        }
    };

    // Toggle the active status (flip current value)
    let new_active = !project.active;
    let user_id = session.current_user_id();
    if let Err(e) = Project::toggle_active(&db, &project.project_id, new_active, &user_id).await {
        tracing::error!(
            "Failed to toggle active for project {}: {}",
            project.project_id,
            e
        );
        return Redirect::to(&format!("/projects/{}", project_name));
    }

    // On reactivation, also redeploy the latest build so schedules / event
    // handlers / MCP tools / webhooks / agents come back online — deactivation
    // tore those down via undeploy + schedule deactivation, and only the
    // worker-side manifest reload (triggered by a DeploymentMessage) restores
    // them. Without this, the project would look "active" in the UI but no
    // crons would fire and event routing would be stale.
    if new_active {
        match hot::lang::event::enqueue_redeploy_for_project_reactivation(
            &db,
            &conf,
            &project.project_id,
            &user_id,
        )
        .await
        {
            Ok(Some(build_id)) => {
                tracing::info!(
                    "Reactivated project {} and queued redeploy of latest build {}",
                    project.project_id,
                    build_id
                );
            }
            Ok(None) => {
                tracing::info!(
                    "Reactivated project {} (no builds yet, nothing to redeploy)",
                    project.project_id
                );
            }
            Err(e) => {
                tracing::error!(
                    "Reactivated project {} but failed to enqueue redeploy of latest build: {}",
                    project.project_id,
                    e
                );
            }
        }
    }

    if new_active {
        Redirect::to(&format!("/projects/{}", project_name))
    } else {
        Redirect::to("/projects")
    }
}

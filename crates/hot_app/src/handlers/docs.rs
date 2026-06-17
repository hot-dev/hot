//! Documentation handlers for project docs
//!
//! Serves documentation from project builds, including:
//! - Project's own namespaces from src_paths
//! - Dependency documentation
//! - hot-std documentation (shared across all projects)

use ahash::AHashMap;
use askama::Template;
use axum::{
    extract::{Path, Query, State},
    response::{Html, IntoResponse},
};
use hot::db::{Build, DatabasePool, Project};
use serde::Deserialize;
use std::sync::Arc;

use crate::{auth::Session, templates};

/// Get the deployed build for documentation.
/// Returns the currently deployed build (live or bundle) - no fallback logic.
/// The build's type determines how docs are loaded:
/// - Live builds: generate docs on-the-fly from source
/// - Bundle builds: extract docs from the build zip
async fn get_build_for_docs(
    db: &DatabasePool,
    project_id: &uuid::Uuid,
) -> Result<Option<Build>, hot::db::BuildError> {
    Build::get_deployed_build_by_project(db, project_id).await
}

// Query params for the docs index page
#[derive(Deserialize, Debug, Default)]
pub struct DocsIndexQuery {
    pub project: Option<String>,
}

#[derive(Template)]
#[template(path = "docs_index.html")]
struct DocsIndexTemplate<'a> {
    title: &'a str,
    page_context: templates::PrivatePageContext,
    projects: Vec<Project>,
    selected_project: Option<Project>,
    has_docs: bool,
    docs_generating: bool,
    build_info: Option<String>,
    project_namespaces: Vec<NamespaceInfo>,
    dependencies: Vec<DependencyInfo>,
}

/// Top-level docs page with project selector
pub async fn docs_index_handler(
    State(db): State<Arc<DatabasePool>>,
    Query(query): Query<DocsIndexQuery>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> impl IntoResponse {
    // Build breadcrumbs
    let mut breadcrumbs = templates::build_base_breadcrumbs_with_env(&session);
    breadcrumbs.push(templates::BreadcrumbItem::current("Docs".to_string()));

    let env_id = match session.current_env_id() {
        Some(id) => id,
        None => {
            return Html(render_docs_error(
                "No Environment Selected",
                "Please select an environment to view documentation.",
            ));
        }
    };

    // Get all active projects for the dropdown
    let projects: Vec<_> =
        match Project::get_projects_by_env(&db, &env_id, Some(100), Some(0)).await {
            Ok(projects) => projects.into_iter().filter(|p| p.active).collect(),
            Err(e) => {
                return Html(render_docs_error(
                    "Error",
                    &format!("Failed to load projects: {}", e),
                ));
            }
        };

    // Get selected project if specified, or auto-select if there's only one project
    let selected_project = if let Some(project_name) = &query.project {
        match Project::get_project_by_env_and_name(&db, &env_id, project_name).await {
            Ok(Some(project)) => Some(project),
            Ok(None) => None,
            Err(_) => None,
        }
    } else if projects.len() == 1 {
        // Auto-select if there's only one project
        Some(projects[0].clone())
    } else {
        None
    };

    // Load docs for selected project
    let (has_docs, docs_generating, build_info, project_namespaces, dependencies) =
        if let Some(ref project) = selected_project {
            // Get the active build for this project (live or deployed bundle)
            let active_build = match get_build_for_docs(&db, &project.project_id).await {
                Ok(Some(build)) => Some(build),
                Ok(None) => None,
                Err(e) => {
                    tracing::error!("Error fetching active build for {}: {}", project.name, e);
                    None
                }
            };

            if let Some(ref build) = active_build {
                let build_info_str = format!(
                    "Build {} (created {} {})",
                    &build.build_id.to_string()[..8],
                    crate::timezone::format_in_timezone(
                        &build.created_at,
                        &session.display_timezone,
                        "%Y-%m-%d %H:%M"
                    ),
                    &session.timezone_abbreviation
                );

                // Check if docs are currently being generated
                let is_generating = if let Some(cache) = crate::build_cache::get_build_docs_cache()
                {
                    cache.is_generating(&project.name).await
                } else {
                    false
                };

                // Try to load docs from cache
                let (namespaces, deps) = match crate::build_cache::get_build_docs_cache() {
                    Some(cache) => match cache.get_build_docs(&db, build).await {
                        Ok(cached) => {
                            let namespaces: Vec<NamespaceInfo> = cached
                                .project_docs
                                .namespaces
                                .iter()
                                .map(|ns| NamespaceInfo {
                                    name: ns.namespace.clone(),
                                    path: ns.name.clone(),
                                    function_count: ns.functions.len(),
                                    type_count: ns.types.len(),
                                    description: ns.doc.clone(),
                                    schedule_count: ns
                                        .functions
                                        .iter()
                                        .filter(|f| f.schedule.is_some())
                                        .count(),
                                    event_count: ns
                                        .functions
                                        .iter()
                                        .filter(|f| f.on_event.is_some())
                                        .count(),
                                    webhook_count: ns
                                        .functions
                                        .iter()
                                        .filter(|f| f.webhook.is_some())
                                        .count(),
                                    mcp_count: ns
                                        .functions
                                        .iter()
                                        .filter(|f| f.mcp.is_some())
                                        .count(),
                                    sends_count: ns
                                        .functions
                                        .iter()
                                        .filter(|f| !f.sends.is_empty())
                                        .count(),
                                })
                                .collect();

                            let mut deps: Vec<DependencyInfo> = cached
                                .dependency_docs
                                .iter()
                                .map(|(name, pkg_docs)| {
                                    // Package name is in <org>/<pkg> format (e.g., "hot.dev/anthropic")
                                    DependencyInfo::new(
                                        name.clone(),
                                        pkg_docs.meta.version.clone(),
                                        if pkg_docs.meta.description.is_empty() {
                                            None
                                        } else {
                                            Some(pkg_docs.meta.description.clone())
                                        },
                                        pkg_docs.namespaces.len(),
                                    )
                                })
                                .collect();

                            // Sort deps: hot-std first, then alphabetically
                            sort_dependencies(&mut deps);

                            (namespaces, deps)
                        }
                        Err(e) => {
                            tracing::warn!("Failed to load docs from cache: {}", e);
                            (vec![], vec![])
                        }
                    },
                    None => {
                        tracing::warn!("Build docs cache not initialized");
                        (vec![], vec![])
                    }
                };

                // Check if docs are generating now (after the get_build_docs call which may trigger generation)
                let is_generating_now =
                    if let Some(cache) = crate::build_cache::get_build_docs_cache() {
                        cache.is_generating(&project.name).await
                    } else {
                        false
                    };

                (
                    true,
                    is_generating || is_generating_now,
                    Some(build_info_str),
                    namespaces,
                    deps,
                )
            } else {
                (false, false, None, vec![], vec![])
            }
        } else {
            (false, false, None, vec![], vec![])
        };

    let template = DocsIndexTemplate {
        title: "Documentation",
        page_context: templates::PrivatePageContext::with_breadcrumbs(
            "docs",
            &session,
            breadcrumbs,
        ),
        projects,
        selected_project,
        has_docs,
        docs_generating,
        build_info,
        project_namespaces,
        dependencies,
    };

    Html(template.render().unwrap_or_else(|e| {
        tracing::error!("Template render error: {}", e);
        render_docs_error("Render Error", &e.to_string())
    }))
}

/// Docs index for a specific project
/// Shows available documentation from the project's active build
pub async fn project_docs_index_handler(
    Path(project_name): Path<String>,
    State(db): State<Arc<DatabasePool>>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> impl IntoResponse {
    let env_id = match session.current_env_id() {
        Some(id) => id,
        None => {
            return Html(render_docs_error(
                "No Environment Selected",
                "Please select an environment to view project documentation.",
            ));
        }
    };

    // Get the project
    let project = match Project::get_project_by_env_and_name(&db, &env_id, &project_name).await {
        Ok(Some(p)) => p,
        Ok(None) => {
            return Html(render_docs_error(
                "Project Not Found",
                &format!("Project '{}' was not found.", project_name),
            ));
        }
        Err(e) => {
            tracing::error!("Error fetching project {}: {}", project_name, e);
            return Html(render_docs_error("Error", "Failed to load project."));
        }
    };

    // Get the active build for this project (live or deployed bundle)
    let active_build = match get_build_for_docs(&db, &project.project_id).await {
        Ok(Some(build)) => Some(build),
        Ok(None) => None,
        Err(e) => {
            tracing::error!("Error fetching active build for {}: {}", project_name, e);
            None
        }
    };

    // Build breadcrumbs
    let mut breadcrumbs = templates::build_base_breadcrumbs_with_env(&session);
    breadcrumbs.push(templates::BreadcrumbItem::link(
        "Projects".to_string(),
        "/projects".to_string(),
    ));
    breadcrumbs.push(templates::BreadcrumbItem::link(
        project_name.clone(),
        format!("/projects/{}", project_name),
    ));
    breadcrumbs.push(templates::BreadcrumbItem::current("Docs".to_string()));

    // Load docs from cache if we have an active build
    let (has_docs, build_info, project_namespaces, dependencies) =
        if let Some(ref build) = active_build {
            let build_info = format!(
                "Build {} (created {} {})",
                &build.build_id.to_string()[..8],
                crate::timezone::format_in_timezone(
                    &build.created_at,
                    &session.display_timezone,
                    "%Y-%m-%d %H:%M"
                ),
                &session.timezone_abbreviation
            );

            // Try to load docs from cache
            let (namespaces, deps) = match crate::build_cache::get_build_docs_cache() {
                Some(cache) => match cache.get_build_docs(&db, build).await {
                    Ok(cached) => {
                        // Convert cached docs to display format
                        let namespaces: Vec<NamespaceInfo> = cached
                            .project_docs
                            .namespaces
                            .iter()
                            .map(|ns| NamespaceInfo {
                                name: ns.namespace.clone(),
                                path: ns.name.clone(), // name is the URL-safe path like "hot/str"
                                function_count: ns.functions.len(),
                                type_count: ns.types.len(),
                                description: ns.doc.clone(),
                                schedule_count: ns
                                    .functions
                                    .iter()
                                    .filter(|f| f.schedule.is_some())
                                    .count(),
                                event_count: ns
                                    .functions
                                    .iter()
                                    .filter(|f| f.on_event.is_some())
                                    .count(),
                                webhook_count: ns
                                    .functions
                                    .iter()
                                    .filter(|f| f.webhook.is_some())
                                    .count(),
                                mcp_count: ns.functions.iter().filter(|f| f.mcp.is_some()).count(),
                                sends_count: ns
                                    .functions
                                    .iter()
                                    .filter(|f| !f.sends.is_empty())
                                    .count(),
                            })
                            .collect();

                        let mut deps: Vec<DependencyInfo> = cached
                            .dependency_docs
                            .iter()
                            .map(|(name, pkg_docs)| {
                                // Package name is in <org>/<pkg> format (e.g., "hot.dev/anthropic")
                                DependencyInfo::new(
                                    name.clone(),
                                    pkg_docs.meta.version.clone(),
                                    if pkg_docs.meta.description.is_empty() {
                                        None
                                    } else {
                                        Some(pkg_docs.meta.description.clone())
                                    },
                                    pkg_docs.namespaces.len(),
                                )
                            })
                            .collect();

                        // Sort deps: hot-std first, then alphabetically
                        sort_dependencies(&mut deps);

                        (namespaces, deps)
                    }
                    Err(e) => {
                        tracing::warn!("Failed to load docs from cache: {}", e);
                        (vec![], vec![])
                    }
                },
                None => {
                    tracing::warn!("Build docs cache not initialized");
                    (vec![], vec![])
                }
            };

            (true, Some(build_info), namespaces, deps)
        } else {
            (false, None, vec![], vec![])
        };

    let has_schedules = project_namespaces.iter().any(|ns| ns.schedule_count > 0);
    let has_events = project_namespaces.iter().any(|ns| ns.event_count > 0);
    let has_webhooks = project_namespaces.iter().any(|ns| ns.webhook_count > 0);
    let has_mcp = project_namespaces.iter().any(|ns| ns.mcp_count > 0);
    let has_sends = project_namespaces.iter().any(|ns| ns.sends_count > 0);

    let template = templates::ProjectDocsIndex {
        title: &format!("{} - Documentation", project_name),
        page_context: templates::PrivatePageContext::with_breadcrumbs(
            "docs",
            &session,
            breadcrumbs,
        ),
        project_name: &project_name,
        has_docs,
        build_info: build_info.as_deref(),
        project_namespaces,
        dependencies,
        has_schedules,
        has_events,
        has_webhooks,
        has_mcp,
        has_sends,
    };

    Html(template.render().unwrap_or_else(|e| {
        tracing::error!("Template render error: {}", e);
        render_docs_error("Render Error", &e.to_string())
    }))
}

/// Render a docs error page
fn render_docs_error(title: &str, message: &str) -> String {
    render_docs_error_with_back(title, message, "/docs", "Back to Docs")
}

fn render_docs_error_with_back(
    title: &str,
    message: &str,
    back_url: &str,
    back_text: &str,
) -> String {
    format!(
        r#"<!DOCTYPE html>
<html>
<head>
    <title>{} - Hot Documentation</title>
    <style>
        body {{ font-family: system-ui, sans-serif; max-width: 600px; margin: 4rem auto; padding: 2rem; }}
        h1 {{ color: #cf2425; }}
        a {{ color: #cf2425; }}
    </style>
</head>
<body>
    <h1>{}</h1>
    <p>{}</p>
    <p><a href="{}">← {}</a></p>
</body>
</html>"#,
        title, title, message, back_url, back_text
    )
}

/// Namespace info for display in docs index
#[derive(Debug, Clone)]
pub struct NamespaceInfo {
    pub name: String,
    pub path: String,
    pub function_count: usize,
    pub type_count: usize,
    pub description: Option<String>,
    pub schedule_count: usize,
    pub event_count: usize,
    pub webhook_count: usize,
    pub mcp_count: usize,
    pub sends_count: usize,
}

/// Dependency info for display in docs index
#[derive(Debug, Clone)]
pub struct DependencyInfo {
    /// Full package identifier for URLs (e.g., "hot.dev/anthropic")
    pub name: String,
    /// Short display name without org prefix (e.g., "anthropic")
    pub display_name: String,
    pub version: String,
    pub description: Option<String>,
    pub namespace_count: usize,
}

impl DependencyInfo {
    /// Create a new DependencyInfo, extracting display_name from the full name
    pub fn new(
        name: String,
        version: String,
        description: Option<String>,
        namespace_count: usize,
    ) -> Self {
        // Extract short name: "hot.dev/anthropic" -> "anthropic"
        let display_name = name.split('/').next_back().unwrap_or(&name).to_string();
        Self {
            name,
            display_name,
            version,
            description,
            namespace_count,
        }
    }
}

/// Sort dependencies: hot-std first, then alphabetically
fn sort_dependencies(deps: &mut [DependencyInfo]) {
    deps.sort_by(|a, b| {
        // hot-std always comes first (name is now "hot.dev/hot-std")
        let a_is_std = a.name.ends_with("/hot-std");
        let b_is_std = b.name.ends_with("/hot-std");

        match (a_is_std, b_is_std) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => a.name.cmp(&b.name), // Alphabetical for the rest
        }
    });
}

// Re-export from shared module for template use
use hot::pkg::docs_html::{NavItem, TocSection};

#[derive(Template)]
#[template(path = "docs_namespace.html")]
struct DocsNamespaceTemplate<'a> {
    title: &'a str,
    page_context: templates::PrivatePageContext,
    project_name: &'a str,
    // Content is pre-rendered HTML from docs_html module
    content: String,
    // Navigation
    nav: Vec<NavItem>,
    toc: Vec<TocSection>,
    current_path: &'a str,
    // Prev/Next
    prev_page: Option<NavItem>,
    next_page: Option<NavItem>,
    // Context for breadcrumbs
    is_dependency: bool,
    dep_name: String,
}

// All HTML rendering functions are now in hot::pkg::docs_html

/// Handler for project namespace detail page
/// Route: /docs/{project_name}/project/{ns_path}
pub async fn project_namespace_handler(
    Path((project_name, ns_path)): Path<(String, String)>,
    State(db): State<Arc<DatabasePool>>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> impl IntoResponse {
    // For project namespaces, pkg_id is None
    namespace_detail_handler_internal(project_name, ns_path, None, db, session).await
}

/// Combined handler for package documentation routes
/// Route: /docs/{project_name}/pkg/{*pkg_path}
///
/// URL scheme matches registry docs: /pkg/{org}/{pkg_name}/{module}
/// Examples:
/// - /docs/{project}/pkg/hot.dev/anthropic → package index
/// - /docs/{project}/pkg/hot.dev/anthropic/readme → README page
/// - /docs/{project}/pkg/hot.dev/anthropic/license → LICENSE page
/// - /docs/{project}/pkg/hot.dev/anthropic/::anthropic → namespace detail
///
/// The path is parsed as: org/pkg_name/module where module can be:
/// - empty (package index)
/// - "readme" or "license" (special pages)
/// - namespace path like "::anthropic::chat" (namespace detail)
pub async fn pkg_route_handler(
    Path((project_name, pkg_path)): Path<(String, String)>,
    State(db): State<Arc<DatabasePool>>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> Html<String> {
    // Parse the path: {org}/{pkg_name}/{module}
    let segments: Vec<&str> = pkg_path.split('/').collect();

    if segments.len() < 2 {
        return Html(render_docs_error(
            "Invalid Package URL",
            "Package URL must be in format: /docs/{project}/pkg/{org}/{pkg_name}",
        ));
    }

    let org = segments[0];
    let pkg_name = segments[1];
    // Full package identifier for HashMap lookup and URLs (e.g., "hot.dev/hot-std")
    let pkg_id = format!("{}/{}", org, pkg_name);

    // Module path is everything after org/pkg_name
    let module_path = if segments.len() > 2 {
        segments[2..].join("/")
    } else {
        String::new()
    };

    if module_path.is_empty() {
        // Package index page
        pkg_index_handler_internal(project_name, pkg_id, db, session).await
    } else if module_path == "readme" {
        // README page
        pkg_readme_handler_internal(project_name, pkg_id, db, session).await
    } else if module_path.eq_ignore_ascii_case("license") {
        // LICENSE page
        pkg_license_handler_internal(project_name, pkg_id, db, session).await
    } else {
        // Namespace detail page - module_path is the namespace like "::anthropic::chat"
        namespace_detail_handler_internal(project_name, module_path, Some(pkg_id), db, session)
            .await
    }
}

/// Internal handler for package index page
/// pkg_id: full package identifier (e.g., "hot.dev/hot-std") used for both lookup and URLs
async fn pkg_index_handler_internal(
    project_name: String,
    pkg_id: String,
    db: Arc<DatabasePool>,
    session: Session,
) -> Html<String> {
    let env_id = match session.current_env_id() {
        Some(id) => id,
        None => {
            return Html(render_docs_error(
                "No Environment Selected",
                "Please select an environment to view documentation.",
            ));
        }
    };

    // Get the project
    let project = match Project::get_project_by_env_and_name(&db, &env_id, &project_name).await {
        Ok(Some(p)) => p,
        Ok(None) => {
            return Html(render_docs_error(
                "Project Not Found",
                &format!("Project '{}' was not found.", project_name),
            ));
        }
        Err(e) => {
            tracing::error!("Error fetching project {}: {}", project_name, e);
            return Html(render_docs_error("Error", "Failed to load project."));
        }
    };

    // Get the active build (live or deployed bundle)
    let build = match get_build_for_docs(&db, &project.project_id).await {
        Ok(Some(b)) => b,
        Ok(None) => {
            return Html(render_docs_error(
                "No Active Build",
                "This project does not have an active build.",
            ));
        }
        Err(e) => {
            tracing::error!("Error fetching build for {}: {}", project_name, e);
            return Html(render_docs_error("Error", "Failed to load build."));
        }
    };

    // Load docs from cache
    let cached_docs = match crate::build_cache::get_build_docs_cache() {
        Some(cache) => match cache.get_build_docs(&db, &build).await {
            Ok(docs) => docs,
            Err(e) => {
                tracing::error!("Error loading docs from cache: {}", e);
                return Html(render_docs_error("Error", "Failed to load documentation."));
            }
        },
        None => {
            return Html(render_docs_error(
                "Cache Not Available",
                "Build docs cache is not initialized.",
            ));
        }
    };

    // Find the dependency by full package identifier
    let pkg_docs = match cached_docs.dependency_docs.get(&pkg_id) {
        Some(docs) => docs,
        None => {
            return Html(render_docs_error(
                "Package Not Found",
                &format!("Package '{}' not found.", pkg_id),
            ));
        }
    };

    // Build breadcrumbs
    let mut breadcrumbs = templates::build_base_breadcrumbs_with_env(&session);
    breadcrumbs.push(templates::BreadcrumbItem::link(
        "Docs".to_string(),
        format!("/docs?project={}", project_name),
    ));
    breadcrumbs.push(templates::BreadcrumbItem::current(pkg_id.clone()));

    // Use the shared docs_html module to generate the package index page
    // hot_app uses /docs/{project}/pkg as base URL
    let base_url = format!("/docs/{}/pkg", project_name);
    let content = hot::pkg::docs_html::generate_pkg_index_html(pkg_docs, &pkg_id, &base_url);

    // Build navigation for this package
    let nav = build_pkg_nav(&project_name, &pkg_id, pkg_docs);

    // Build TOC for the index page (sections: Namespaces)
    let toc = vec![hot::pkg::docs_html::TocSection {
        title: "Contents".to_string(),
        items: vec![hot::pkg::docs_html::TocItem {
            name: "Namespaces".to_string(),
            anchor: "namespaces".to_string(),
            is_core: false,
            indent: 0,
            badges: vec![],
        }],
    }];

    // pkg_id is like "hot.dev/hot-std" - use for URL construction
    let current_path = format!("/docs/{}/pkg/{}", project_name, pkg_id);

    let title_str = format!("{} - {} Docs", pkg_id, project_name);
    let template = DocsNamespaceTemplate {
        title: &title_str,
        page_context: templates::PrivatePageContext::with_breadcrumbs(
            "docs",
            &session,
            breadcrumbs,
        ),
        project_name: &project_name,
        content,
        nav,
        toc,
        current_path: &current_path,
        prev_page: None,
        next_page: None,
        is_dependency: true,
        dep_name: pkg_id,
    };

    Html(template.render().unwrap_or_else(|e| {
        tracing::error!("Template render error: {}", e);
        render_docs_error("Render Error", &e.to_string())
    }))
}

/// Internal handler for dependency README page
async fn pkg_readme_handler_internal(
    project_name: String,
    pkg_id: String,
    db: Arc<DatabasePool>,
    session: Session,
) -> Html<String> {
    dependency_file_handler_internal(project_name, pkg_id, "readme", db, session).await
}

/// Internal handler for dependency LICENSE page
async fn pkg_license_handler_internal(
    project_name: String,
    pkg_id: String,
    db: Arc<DatabasePool>,
    session: Session,
) -> Html<String> {
    dependency_file_handler_internal(project_name, pkg_id, "license", db, session).await
}

/// Internal handler for dependency README/LICENSE pages
/// pkg_id: full package identifier (e.g., "hot.dev/hot-std") for lookup and URLs
async fn dependency_file_handler_internal(
    project_name: String,
    pkg_id: String,
    file_type: &str, // "readme" or "license"
    db: Arc<DatabasePool>,
    session: Session,
) -> Html<String> {
    let env_id = match session.current_env_id() {
        Some(id) => id,
        None => {
            return Html(render_docs_error(
                "No Environment Selected",
                "Please select an environment to view documentation.",
            ));
        }
    };

    let project = match Project::get_project_by_env_and_name(&db, &env_id, &project_name).await {
        Ok(Some(p)) => p,
        Ok(None) => {
            return Html(render_docs_error(
                "Project Not Found",
                &format!("Project '{}' was not found.", project_name),
            ));
        }
        Err(e) => {
            tracing::error!("Error fetching project {}: {}", project_name, e);
            return Html(render_docs_error("Error", "Failed to load project."));
        }
    };

    // Get the active build (live or deployed bundle)
    let build = match get_build_for_docs(&db, &project.project_id).await {
        Ok(Some(b)) => b,
        Ok(None) => {
            return Html(render_docs_error(
                "No Active Build",
                "This project does not have an active build.",
            ));
        }
        Err(e) => {
            tracing::error!("Error fetching build for {}: {}", project_name, e);
            return Html(render_docs_error("Error", "Failed to load build."));
        }
    };

    let cached_docs = match crate::build_cache::get_build_docs_cache() {
        Some(cache) => match cache.get_build_docs(&db, &build).await {
            Ok(docs) => docs,
            Err(e) => {
                tracing::error!("Error loading docs from cache: {}", e);
                return Html(render_docs_error("Error", "Failed to load documentation."));
            }
        },
        None => {
            return Html(render_docs_error(
                "Cache Not Available",
                "Build docs cache is not initialized.",
            ));
        }
    };

    // Look up by full package identifier
    let pkg_docs = match cached_docs.dependency_docs.get(&pkg_id) {
        Some(docs) => docs,
        None => {
            return Html(render_docs_error(
                "Package Not Found",
                &format!("Package '{}' not found.", pkg_id),
            ));
        }
    };

    // Get the file content
    let (title, content) = match file_type {
        "readme" => {
            let readme_content = pkg_docs.readme.as_ref().map(|r| {
                hot::pkg::docs_html::markdown_to_html(r)
            }).unwrap_or_else(|| {
                "<p class=\"text-gray-500 dark:text-gray-400 italic\">No README available for this package.</p>".to_string()
            });
            (format!("README - {}", pkg_id), readme_content)
        }
        "license" => {
            let license_content = pkg_docs.license.as_ref().map(|l| {
                // Simple HTML escape for license text
                let escaped = l
                    .replace('&', "&amp;")
                    .replace('<', "&lt;")
                    .replace('>', "&gt;");
                format!("<pre class=\"license-text whitespace-pre-wrap text-sm\">{}</pre>", escaped)
            }).unwrap_or_else(|| {
                "<p class=\"text-gray-500 dark:text-gray-400 italic\">No LICENSE file available for this package.</p>".to_string()
            });
            (format!("LICENSE - {}", pkg_id), license_content)
        }
        _ => {
            return Html(render_docs_error("Not Found", "File not found."));
        }
    };

    // Build breadcrumbs
    let mut breadcrumbs = templates::build_base_breadcrumbs_with_env(&session);
    breadcrumbs.push(templates::BreadcrumbItem::link(
        "Docs".to_string(),
        format!("/docs?project={}", project_name),
    ));
    breadcrumbs.push(templates::BreadcrumbItem::link(
        pkg_id.clone(),
        format!("/docs/{}/pkg/{}", project_name, pkg_id),
    ));
    breadcrumbs.push(templates::BreadcrumbItem::current(file_type.to_uppercase()));

    // Build navigation for the package
    let nav = build_pkg_nav(&project_name, &pkg_id, pkg_docs);

    // Current path - pkg_id is like "hot.dev/hot-std"
    let current_path = format!("/docs/{}/pkg/{}/{}", project_name, pkg_id, file_type);

    // No TOC for README/LICENSE
    let toc = Vec::new();

    let template = DocsNamespaceTemplate {
        title: &title,
        page_context: templates::PrivatePageContext::with_breadcrumbs(
            "docs",
            &session,
            breadcrumbs,
        ),
        project_name: &project_name,
        content,
        nav,
        toc,
        current_path: &current_path,
        prev_page: None,
        next_page: None,
        is_dependency: true,
        dep_name: pkg_id,
    };

    Html(template.render().unwrap_or_else(|e| {
        tracing::error!("Template render error: {}", e);
        render_docs_error("Render Error", &e.to_string())
    }))
}

/// Common handler for namespace detail pages
/// pkg_id: full package identifier (e.g., "hot.dev/hot-std") for lookup and URLs, None for project namespaces
async fn namespace_detail_handler_internal(
    project_name: String,
    ns_path: String,
    pkg_id: Option<String>,
    db: Arc<DatabasePool>,
    session: Session,
) -> Html<String> {
    let env_id = match session.current_env_id() {
        Some(id) => id,
        None => {
            return Html(render_docs_error(
                "No Environment Selected",
                "Please select an environment to view documentation.",
            ));
        }
    };

    // Get the project
    let project = match Project::get_project_by_env_and_name(&db, &env_id, &project_name).await {
        Ok(Some(p)) => p,
        Ok(None) => {
            return Html(render_docs_error(
                "Project Not Found",
                &format!("Project '{}' was not found.", project_name),
            ));
        }
        Err(e) => {
            tracing::error!("Error fetching project {}: {}", project_name, e);
            return Html(render_docs_error("Error", "Failed to load project."));
        }
    };

    // Get the active build (live or deployed bundle)
    let build = match get_build_for_docs(&db, &project.project_id).await {
        Ok(Some(b)) => b,
        Ok(None) => {
            return Html(render_docs_error(
                "No Active Build",
                "This project does not have an active build with documentation.",
            ));
        }
        Err(e) => {
            tracing::error!("Error fetching build for {}: {}", project_name, e);
            return Html(render_docs_error("Error", "Failed to load build."));
        }
    };

    // Load docs from cache
    let cached_docs = match crate::build_cache::get_build_docs_cache() {
        Some(cache) => match cache.get_build_docs(&db, &build).await {
            Ok(docs) => docs,
            Err(e) => {
                tracing::error!("Error loading docs from cache: {}", e);
                return Html(render_docs_error("Error", "Failed to load documentation."));
            }
        },
        None => {
            return Html(render_docs_error(
                "Cache Not Available",
                "Build docs cache is not initialized.",
            ));
        }
    };

    // Find the namespace
    let is_project_ns = pkg_id.is_none();

    let namespace = if let Some(ref id) = pkg_id {
        // Looking in dependency docs
        match cached_docs.dependency_docs.get(id) {
            Some(pkg_docs) => match pkg_docs.namespaces.iter().find(|ns| ns.name == ns_path) {
                Some(ns) => ns,
                None => {
                    return Html(render_docs_error(
                        "Namespace Not Found",
                        &format!("Namespace '{}' not found in package '{}'.", ns_path, id),
                    ));
                }
            },
            None => {
                return Html(render_docs_error(
                    "Package Not Found",
                    &format!("Package '{}' not found.", id),
                ));
            }
        }
    } else {
        // Looking in project docs
        match cached_docs
            .project_docs
            .namespaces
            .iter()
            .find(|ns| ns.name == ns_path)
        {
            Some(ns) => ns,
            None => {
                return Html(render_docs_error(
                    "Namespace Not Found",
                    &format!("Namespace '{}' not found in project.", ns_path),
                ));
            }
        }
    };

    // Build breadcrumbs
    let mut breadcrumbs = templates::build_base_breadcrumbs_with_env(&session);
    breadcrumbs.push(templates::BreadcrumbItem::link(
        "Docs".to_string(),
        format!("/docs?project={}", project_name),
    ));
    if let Some(ref id) = pkg_id {
        breadcrumbs.push(templates::BreadcrumbItem::link(
            id.clone(),
            format!("/docs/{}/pkg/{}", project_name, id),
        ));
    }
    breadcrumbs.push(templates::BreadcrumbItem::current(
        namespace.namespace.clone(),
    ));

    // Use shared docs_html module to generate HTML content
    use hot::pkg::docs_html;

    // Determine the package name and base_url for rendering
    let (render_pkg_name, base_url) = if is_project_ns {
        (
            project_name.clone(),
            format!("/docs/{}/project", project_name),
        )
    } else {
        let id = pkg_id.as_ref().unwrap_or(&project_name);
        (id.clone(), format!("/docs/{}/pkg", project_name))
    };

    let alias_target_index = if is_project_ns {
        AHashMap::new()
    } else {
        let id = pkg_id.as_ref().unwrap_or(&project_name);
        cached_docs
            .dependency_docs
            .get(id)
            .map(|pkg_docs| {
                docs_html::build_alias_target_index(pkg_docs, &render_pkg_name, &base_url)
            })
            .unwrap_or_default()
    };

    // Generate HTML content using the shared module with correct base_url
    let content = docs_html::generate_namespace_html_with_registry_and_aliases(
        namespace,
        &render_pkg_name,
        &AHashMap::new(), // type_index - not used in hot_app currently
        None,             // cross_pkg_registry - not used in hot_app currently
        &base_url,
        &alias_target_index,
    );

    // Build navigation and TOC using shared module
    let (nav, current_path) = if is_project_ns {
        // For project namespaces, build nav from project docs
        let nav = build_project_nav(&project_name, &cached_docs.project_docs);
        let path = format!("/docs/{}/project/{}", project_name, ns_path);
        (nav, path)
    } else {
        // For dependency namespaces, build nav from the dependency's docs
        let id = pkg_id.as_ref().unwrap();
        let pkg_docs = cached_docs.dependency_docs.get(id).unwrap();
        let nav = build_pkg_nav(&project_name, id, pkg_docs);
        let path = format!("/docs/{}/pkg/{}/{}", project_name, id, ns_path);
        (nav, path)
    };

    // Build TOC for the namespace
    let toc = docs_html::build_namespace_toc(namespace);

    // Get prev/next pages
    let (prev_page, next_page) = docs_html::get_prev_next(&nav, &current_path);

    let title_str = format!("{} - {}", namespace.namespace, project_name);
    let template = DocsNamespaceTemplate {
        title: &title_str,
        page_context: templates::PrivatePageContext::with_breadcrumbs(
            "docs",
            &session,
            breadcrumbs,
        ),
        project_name: &project_name,
        content,
        nav,
        toc,
        current_path: &current_path,
        prev_page,
        next_page,
        is_dependency: !is_project_ns,
        dep_name: pkg_id.unwrap_or_default(),
    };

    Html(template.render().unwrap_or_else(|e| {
        tracing::error!("Template render error: {}", e);
        render_docs_error("Render Error", &e.to_string())
    }))
}

/// Build navigation items for project namespaces
fn build_project_nav(
    project_name: &str,
    project_docs: &hot::pkg::docs::ProjectDocs,
) -> Vec<NavItem> {
    use hot::pkg::docs_html::parse_namespace;

    let mut items = Vec::new();
    for ns in &project_docs.namespaces {
        let (prefix, suffix) = parse_namespace(&ns.namespace);
        let display = if prefix.is_empty() {
            suffix
        } else {
            format!("{}{}", prefix, suffix)
        };
        items.push(NavItem {
            title: display,
            path: Some(format!("/docs/{}/project/{}", project_name, ns.name)),
            children: Vec::new(),
        });
    }
    items
}

/// Build navigation items for a package's namespaces
///
/// URL scheme: /docs/{project}/pkg/{dep_name}/{module}
/// where dep_name is like "hot.dev/anthropic" and module is the namespace path
fn build_pkg_nav(
    project_name: &str,
    dep_name: &str,
    pkg_docs: &hot::pkg::docs::PkgDocs,
) -> Vec<NavItem> {
    let dep_name_prefix = format!("{}/", dep_name);

    // Generate nav - this returns a single root item with the package name
    // and its children are the namespace groups/items
    let mut nav_items = hot::pkg::docs_html::generate_pkg_nav(dep_name, pkg_docs);

    // Flatten: if there's a single root item with children, use the children directly
    // This removes the duplicate package name since the template already shows it as a heading
    let items_to_process = if nav_items.len() == 1 && !nav_items[0].children.is_empty() {
        // Take the children of the root item
        std::mem::take(&mut nav_items[0].children)
    } else {
        nav_items
    };

    // Rewrite all paths to use the correct URL scheme
    items_to_process
        .into_iter()
        .map(|mut item| {
            // Rewrite paths for this level (namespace groups have path: None, so this only affects direct namespace items)
            if let Some(ref path) = item.path {
                if let Some(ns_path) = path.strip_prefix(&dep_name_prefix) {
                    item.path = Some(format!(
                        "/docs/{}/pkg/{}/{}",
                        project_name, dep_name, ns_path
                    ));
                } else if path == dep_name {
                    item.path = Some(format!("/docs/{}/pkg/{}", project_name, dep_name));
                }
            }
            // Rewrite paths for children (the actual namespace links inside groups)
            for child in &mut item.children {
                if let Some(ref path) = child.path
                    && let Some(ns_path) = path.strip_prefix(&dep_name_prefix)
                {
                    child.path = Some(format!(
                        "/docs/{}/pkg/{}/{}",
                        project_name, dep_name, ns_path
                    ));
                }
            }
            item
        })
        .collect()
}

// ============================================================================
// Search API
// ============================================================================

/// Search result item for package documentation
#[derive(Debug, Clone, serde::Serialize)]
pub struct SearchItem {
    pub name: String,
    pub kind: String,              // "function", "type", "namespace"
    pub namespace: String,         // e.g., "::hot::str"
    pub pkg: String,               // e.g., "hot-std" or project name
    pub path: String,              // URL path e.g., "/pkg/hot.dev/hot-std/hot/str#uppercase"
    pub signature: Option<String>, // e.g., "fn (s: Str): Str"
    pub summary: String,           // First line of doc
    pub is_core: bool,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct SearchIndex {
    pub items: Vec<SearchItem>,
}

/// Search API handler - returns search index for a project's docs
/// Route: /api/docs/{project_name}/search
pub async fn docs_search_handler(
    Path(project_name): Path<String>,
    State(db): State<Arc<DatabasePool>>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> impl IntoResponse {
    let env_id = match session.current_env_id() {
        Some(id) => id,
        None => {
            return axum::Json(SearchIndex { items: vec![] }).into_response();
        }
    };

    // Get the project
    let project = match Project::get_project_by_env_and_name(&db, &env_id, &project_name).await {
        Ok(Some(p)) => p,
        _ => {
            return axum::Json(SearchIndex { items: vec![] }).into_response();
        }
    };

    // Get the active build (live or deployed bundle)
    let build = match get_build_for_docs(&db, &project.project_id).await {
        Ok(Some(b)) => b,
        _ => {
            return axum::Json(SearchIndex { items: vec![] }).into_response();
        }
    };

    // Load docs from cache
    let cached_docs = match crate::build_cache::get_build_docs_cache() {
        Some(cache) => match cache.get_build_docs(&db, &build).await {
            Ok(docs) => docs,
            Err(_) => {
                return axum::Json(SearchIndex { items: vec![] }).into_response();
            }
        },
        None => {
            return axum::Json(SearchIndex { items: vec![] }).into_response();
        }
    };

    let mut items = Vec::new();

    // Index project namespaces
    for ns in &cached_docs.project_docs.namespaces {
        let ns_path = format!("/docs/{}/project/{}", project_name, ns.name);

        // Add namespace
        items.push(SearchItem {
            name: ns.namespace.clone(),
            kind: "namespace".to_string(),
            namespace: ns.namespace.clone(),
            pkg: project_name.clone(),
            path: ns_path.clone(),
            signature: None,
            summary: ns.doc.as_ref().map(|d| first_line(d)).unwrap_or_default(),
            is_core: false,
        });

        // Add types
        for typ in &ns.types {
            items.push(SearchItem {
                name: typ.name.clone(),
                kind: "type".to_string(),
                namespace: ns.namespace.clone(),
                pkg: project_name.clone(),
                path: format!("{}#type-{}", ns_path, typ.name),
                signature: None,
                summary: typ.doc.as_ref().map(|d| first_line(d)).unwrap_or_default(),
                is_core: typ.is_core,
            });
        }

        // Add functions
        for func in &ns.functions {
            let signature = func.signatures.first().map(|sig| {
                let params: Vec<String> = sig
                    .params
                    .iter()
                    .map(|p| {
                        let mut s = String::new();
                        if p.is_lazy {
                            s.push_str("lazy ");
                        }
                        if p.is_variadic {
                            s.push_str("...");
                        }
                        s.push_str(&p.name);
                        if let Some(t) = &p.type_annotation {
                            s.push_str(": ");
                            s.push_str(t);
                        }
                        s
                    })
                    .collect();
                let ret = sig
                    .return_type
                    .as_ref()
                    .map(|r| format!(": {}", r))
                    .unwrap_or_default();
                format!("fn ({}){}", params.join(", "), ret)
            });

            items.push(SearchItem {
                name: func.name.clone(),
                kind: "function".to_string(),
                namespace: ns.namespace.clone(),
                pkg: project_name.clone(),
                path: format!("{}#{}", ns_path, func.name),
                signature,
                summary: func.doc.as_ref().map(|d| first_line(d)).unwrap_or_default(),
                is_core: func.is_core,
            });
        }
    }

    // Index dependency namespaces
    for (pkg_name, pkg_docs) in &cached_docs.dependency_docs {
        // Package name should already be in <org>/<pkg> format (e.g., "hot.dev/anthropic")
        for ns in &pkg_docs.namespaces {
            // pkg_name is like "hot.dev/anthropic", ns.name is like "::anthropic::chat"
            let ns_path = format!("/docs/{}/pkg/{}/{}", project_name, pkg_name, ns.name);

            // Add namespace
            items.push(SearchItem {
                name: ns.namespace.clone(),
                kind: "namespace".to_string(),
                namespace: ns.namespace.clone(),
                pkg: pkg_name.clone(),
                path: ns_path.clone(),
                signature: None,
                summary: ns.doc.as_ref().map(|d| first_line(d)).unwrap_or_default(),
                is_core: false,
            });

            // Add types
            for typ in &ns.types {
                items.push(SearchItem {
                    name: typ.name.clone(),
                    kind: "type".to_string(),
                    namespace: ns.namespace.clone(),
                    pkg: pkg_name.clone(),
                    path: format!("{}#type-{}", ns_path, typ.name),
                    signature: None,
                    summary: typ.doc.as_ref().map(|d| first_line(d)).unwrap_or_default(),
                    is_core: typ.is_core,
                });
            }

            // Add functions
            for func in &ns.functions {
                let signature = func.signatures.first().map(|sig| {
                    let params: Vec<String> = sig
                        .params
                        .iter()
                        .map(|p| {
                            let mut s = String::new();
                            if p.is_lazy {
                                s.push_str("lazy ");
                            }
                            if p.is_variadic {
                                s.push_str("...");
                            }
                            s.push_str(&p.name);
                            if let Some(t) = &p.type_annotation {
                                s.push_str(": ");
                                s.push_str(t);
                            }
                            s
                        })
                        .collect();
                    let ret = sig
                        .return_type
                        .as_ref()
                        .map(|r| format!(": {}", r))
                        .unwrap_or_default();
                    format!("fn ({}){}", params.join(", "), ret)
                });

                items.push(SearchItem {
                    name: func.name.clone(),
                    kind: "function".to_string(),
                    namespace: ns.namespace.clone(),
                    pkg: pkg_name.clone(),
                    path: format!("{}#{}", ns_path, func.name),
                    signature,
                    summary: func.doc.as_ref().map(|d| first_line(d)).unwrap_or_default(),
                    is_core: func.is_core,
                });
            }
        }
    }

    axum::Json(SearchIndex { items }).into_response()
}

/// Extract first line of text
fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or("").to_string()
}

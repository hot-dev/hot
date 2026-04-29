use crate::auth::Session;
use crate::templates;
use crate::templates::filters;
use ahash::AHashSet;
use askama::Template;
use axum::extract::{Form, Path, Query, State};
use axum::response::{Html, IntoResponse, Redirect};
use hot::context_encryption::ContextEncryption;
use hot::db::{Build, Context, DatabasePool, Project};
use serde::Deserialize;
use std::sync::Arc;
use uuid::Uuid;

// Form data structure for creating/editing context variables
#[derive(Deserialize, Debug)]
pub struct ContextForm {
    pub key: String,
    pub value: String,
    pub description: Option<String>,
}

// Query params for the contexts index page
#[derive(Deserialize, Debug, Default)]
pub struct ContextsIndexQuery {
    pub tab: Option<String>,     // "env" (default) or "project"
    pub project: Option<String>, // project name (for project tab)
}

// A required context variable (from build manifest)
#[derive(Clone, Debug)]
struct RequiredCtxVar {
    key: String,
    is_set: bool,
}

#[derive(Template)]
#[template(path = "contexts_index.html")]
struct ContextsIndexTemplate {
    title: &'static str,
    page_context: templates::PrivatePageContext,
    active_tab: String, // "env" or "project"
    // Environment tab data
    env_contexts: Vec<Context>,
    // Project tab data
    projects: Vec<Project>,
    selected_project: Option<Project>,
    project_contexts: Vec<Context>,
    required_ctx_vars: Vec<RequiredCtxVar>,
    has_missing_required_ctx_vars: bool, // Pre-computed for template
    is_admin: bool,
}

#[derive(Template)]
#[template(path = "contexts_list.html")]
struct ContextsListTemplate {
    title: &'static str,
    page_context: templates::PrivatePageContext,
    project_name: String,
    contexts: Vec<Context>,
    is_admin: bool,
}

#[derive(Template)]
#[template(path = "contexts_new.html")]
struct ContextsNewTemplate {
    title: &'static str,
    page_context: templates::PrivatePageContext,
    scope: String, // "env" or project name
    scope_display: String,
    error: Option<String>,
    key: String,
    value: String,
    description: String,
}

#[derive(Template)]
#[template(path = "contexts_edit.html")]
struct ContextsEditTemplate {
    title: String,
    page_context: templates::PrivatePageContext,
    scope: String, // "env" or project name
    scope_display: String,
    context: Context,
    value: String,
    description: String,
    error: Option<String>,
}

/// Top-level contexts page with Environment/Project tabs
pub async fn contexts_index_handler(
    State(db): State<Arc<DatabasePool>>,
    Query(query): Query<ContextsIndexQuery>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> impl IntoResponse {
    let is_admin = session.is_current_org_admin;

    // Build breadcrumbs
    let mut breadcrumbs = templates::build_base_breadcrumbs_with_env(&session);
    breadcrumbs.push(templates::BreadcrumbItem::current(
        "Context Variables".to_string(),
    ));

    // Get current env_id
    let env_id = match &session.current_env {
        Some(env) => env.env_id,
        None => {
            return Html("No environment selected".to_string()).into_response();
        }
    };

    // Determine active tab (default to "env")
    let active_tab = query.tab.clone().unwrap_or_else(|| "env".to_string());

    // Get environment-level context variables
    let env_contexts = match Context::get_by_env(&db, &env_id).await {
        Ok(contexts) => contexts,
        Err(e) => {
            tracing::error!("Error loading env contexts: {}", e);
            Vec::new()
        }
    };

    // Get all active projects for the dropdown
    let projects: Vec<_> =
        match Project::get_projects_by_env(&db, &env_id, Some(100), Some(0)).await {
            Ok(projects) => projects.into_iter().filter(|p| p.active).collect(),
            Err(e) => {
                tracing::error!("Error loading projects: {}", e);
                Vec::new()
            }
        };

    // Get selected project if specified
    let selected_project = if let Some(project_name) = &query.project {
        match Project::get_project_by_env_and_name(&db, &env_id, project_name).await {
            Ok(Some(project)) => Some(project),
            Ok(None) => None,
            Err(_) => None,
        }
    } else {
        None
    };

    // Get project-level contexts for selected project
    let project_contexts = if let Some(ref project) = selected_project {
        match Context::get_by_project(&db, &project.project_id).await {
            Ok(contexts) => contexts,
            Err(e) => {
                tracing::error!("Error loading project contexts: {}", e);
                Vec::new()
            }
        }
    } else {
        Vec::new()
    };

    // Get required ctx vars from the deployed build (if any)
    // Merge both env and project contexts to check which are set
    let all_existing_contexts: Vec<_> = env_contexts
        .iter()
        .chain(project_contexts.iter())
        .cloned()
        .collect();
    let required_ctx_vars = if let Some(ref project) = selected_project {
        get_required_ctx_vars_for_project(&db, &project.project_id, &all_existing_contexts).await
    } else {
        Vec::new()
    };

    // Pre-compute whether there are any missing required ctx vars (for template use)
    let has_missing_required_ctx_vars = required_ctx_vars.iter().any(|r| !r.is_set);

    let template = ContextsIndexTemplate {
        title: "Context Variables",
        page_context: templates::PrivatePageContext::with_breadcrumbs(
            "contexts",
            &session,
            breadcrumbs,
        ),
        active_tab,
        env_contexts,
        projects,
        selected_project,
        project_contexts,
        required_ctx_vars,
        has_missing_required_ctx_vars,
        is_admin,
    };

    Html(template.render().unwrap()).into_response()
}

/// Get required context variables from the deployed build for a project
async fn get_required_ctx_vars_for_project(
    db: &DatabasePool,
    project_id: &Uuid,
    existing_contexts: &[Context],
) -> Vec<RequiredCtxVar> {
    // Get the deployed build for this project
    let deployed_build = match Build::get_deployed_build_by_project(db, project_id).await {
        Ok(Some(build)) => build,
        Ok(None) => return Vec::new(),
        Err(e) => {
            tracing::debug!("No deployed build found for project: {}", e);
            return Vec::new();
        }
    };

    // Only bundle builds have ctx_requirements in manifest
    if !deployed_build.is_bundle() {
        return Vec::new();
    }

    // Read the build file and extract ctx_requirements
    let storage_path = match &deployed_build.storage_path {
        Some(path) => path,
        None => return Vec::new(),
    };

    let build_data = match std::fs::read(storage_path) {
        Ok(data) => data,
        Err(e) => {
            tracing::debug!("Failed to read build file: {}", e);
            return Vec::new();
        }
    };

    let ctx_requirements = match hot::build::extract_ctx_requirements_from_build(&build_data) {
        Ok(reqs) => reqs,
        Err(e) => {
            tracing::debug!("Failed to extract ctx_requirements: {}", e);
            return Vec::new();
        }
    };

    // Build set of existing context variable keys
    let existing_keys: AHashSet<_> = existing_contexts
        .iter()
        .filter(|c| c.active)
        .map(|c| c.key.clone())
        .collect();

    // Convert to RequiredCtxVar with is_set flag
    let mut required_vars: Vec<_> = ctx_requirements
        .into_iter()
        .map(|key| RequiredCtxVar {
            is_set: existing_keys.contains(&key),
            key,
        })
        .collect();

    // Sort by is_set (missing first), then by key name
    required_vars.sort_by(|a, b| match (a.is_set, b.is_set) {
        (false, true) => std::cmp::Ordering::Less,
        (true, false) => std::cmp::Ordering::Greater,
        _ => a.key.cmp(&b.key),
    });

    required_vars
}

pub async fn contexts_list_handler(
    State(db): State<Arc<DatabasePool>>,
    Path(project_name): Path<String>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> impl IntoResponse {
    // Check if user has permission to view context variables
    let is_admin = session.is_current_org_admin;

    // Build breadcrumbs: <org> (<env>) / Projects / <project> / Context Variables
    let mut breadcrumbs = templates::build_base_breadcrumbs_with_env(&session);
    breadcrumbs.push(templates::BreadcrumbItem::clickable(
        "Projects".to_string(),
        "/projects".to_string(),
    ));
    breadcrumbs.push(templates::BreadcrumbItem::clickable(
        project_name.clone(),
        format!("/projects/{}", project_name),
    ));
    breadcrumbs.push(templates::BreadcrumbItem::current(
        "Context Variables".to_string(),
    ));

    // Get current env_id
    let env_id = match &session.current_env {
        Some(env) => env.env_id,
        None => {
            return Html("No environment selected".to_string()).into_response();
        }
    };

    // Get project
    let project = match Project::get_project_by_env_and_name(&db, &env_id, &project_name).await {
        Ok(Some(project)) => project,
        Ok(None) => {
            return Html(format!("Project '{}' not found", project_name)).into_response();
        }
        Err(e) => {
            return Html(format!("Error loading project: {}", e)).into_response();
        }
    };

    // Get all context variables for this project
    let contexts = match Context::get_by_project(&db, &project.project_id).await {
        Ok(contexts) => contexts,
        Err(e) => {
            return Html(format!("Error loading context variables: {}", e)).into_response();
        }
    };

    let template = ContextsListTemplate {
        title: "Context Variables",
        page_context: templates::PrivatePageContext::with_breadcrumbs(
            "contexts",
            &session,
            breadcrumbs,
        ),
        project_name,
        contexts,
        is_admin,
    };

    Html(template.render().unwrap()).into_response()
}

/// New context variable form - handles both env and project scopes
pub async fn contexts_new_handler(
    State(_db): State<Arc<DatabasePool>>,
    Path(scope): Path<String>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> impl IntoResponse {
    // Only admins can create context variables
    if !session.is_current_org_admin {
        let redirect_url = if scope == "env" {
            "/contexts?tab=env".to_string()
        } else {
            format!("/contexts?tab=project&project={}", scope)
        };
        return Redirect::to(&redirect_url).into_response();
    }

    let scope_display = if scope == "env" {
        "Environment".to_string()
    } else {
        format!("Project: {}", scope)
    };

    // Build breadcrumbs
    let mut breadcrumbs = templates::build_base_breadcrumbs_with_env(&session);
    let back_url = if scope == "env" {
        "/contexts?tab=env".to_string()
    } else {
        format!("/contexts?tab=project&project={}", scope)
    };
    breadcrumbs.push(templates::BreadcrumbItem::clickable(
        "Context Variables".to_string(),
        back_url,
    ));
    breadcrumbs.push(templates::BreadcrumbItem::current("New".to_string()));

    let template = ContextsNewTemplate {
        title: "New Context Variable",
        page_context: templates::PrivatePageContext::with_breadcrumbs(
            "contexts",
            &session,
            breadcrumbs,
        ),
        scope,
        scope_display,
        error: None,
        key: String::new(),
        value: String::new(),
        description: String::new(),
    };

    Html(template.render().unwrap()).into_response()
}

/// Create context variable - handles both env and project scopes
pub async fn contexts_create_handler(
    State(db): State<Arc<DatabasePool>>,
    Path(scope): Path<String>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
    Form(form): Form<ContextForm>,
) -> impl IntoResponse {
    // Only admins can create context variables
    if !session.is_current_org_admin {
        let redirect_url = if scope == "env" {
            "/contexts?tab=env".to_string()
        } else {
            format!("/contexts?tab=project&project={}", scope)
        };
        return Redirect::to(&redirect_url).into_response();
    }

    // Get current env_id and org_id
    let env_id = match &session.current_env {
        Some(env) => env.env_id,
        None => {
            return Redirect::to("/contexts").into_response();
        }
    };

    let org_id = match &session.current_org {
        Some(org) => org.org_id,
        None => {
            return Redirect::to("/contexts").into_response();
        }
    };

    let scope_display = if scope == "env" {
        "Environment".to_string()
    } else {
        format!("Project: {}", scope)
    };

    let back_url = if scope == "env" {
        "/contexts?tab=env".to_string()
    } else {
        format!("/contexts?tab=project&project={}", scope)
    };

    // Helper for building breadcrumbs
    let build_new_breadcrumbs = || {
        let mut breadcrumbs = templates::build_base_breadcrumbs_with_env(&session);
        breadcrumbs.push(templates::BreadcrumbItem::clickable(
            "Context Variables".to_string(),
            back_url.clone(),
        ));
        breadcrumbs.push(templates::BreadcrumbItem::current("New".to_string()));
        breadcrumbs
    };

    // For project scope, get the project
    let project = if scope != "env" {
        match Project::get_project_by_env_and_name(&db, &env_id, &scope).await {
            Ok(Some(project)) => Some(project),
            Ok(None) => {
                return Redirect::to("/contexts").into_response();
            }
            Err(_) => {
                return Redirect::to("/contexts").into_response();
            }
        }
    } else {
        None
    };

    // Load encryption
    let encryption = match ContextEncryption::from_env_or_generate_for_dev("local-dev") {
        Ok(enc) => enc,
        Err(e) => {
            tracing::error!("Failed to load encryption: {}", e);
            let template = ContextsNewTemplate {
                title: "New Context Variable",
                page_context: templates::PrivatePageContext::with_breadcrumbs(
                    "contexts",
                    &session,
                    build_new_breadcrumbs(),
                ),
                scope,
                scope_display,
                error: Some(format!("Encryption error: {}", e)),
                key: form.key.clone(),
                value: form.value.clone(),
                description: form.description.clone().unwrap_or_default(),
            };
            return Html(template.render().unwrap()).into_response();
        }
    };

    // Validate Hot code
    let validated_val = match Context::validate_hot_value(&form.value) {
        Ok(val) => val,
        Err(e) => {
            let template = ContextsNewTemplate {
                title: "New Context Variable",
                page_context: templates::PrivatePageContext::with_breadcrumbs(
                    "contexts",
                    &session,
                    build_new_breadcrumbs(),
                ),
                scope,
                scope_display,
                error: Some(format!("Invalid Hot code: {}", e)),
                key: form.key.clone(),
                value: form.value.clone(),
                description: form.description.clone().unwrap_or_default(),
            };
            return Html(template.render().unwrap()).into_response();
        }
    };

    // Encrypt value
    let encrypted_value = match Context::set_value_from_val(&validated_val, &encryption, &org_id) {
        Ok(enc) => enc,
        Err(e) => {
            tracing::error!("Failed to encrypt value: {}", e);
            let template = ContextsNewTemplate {
                title: "New Context Variable",
                page_context: templates::PrivatePageContext::with_breadcrumbs(
                    "contexts",
                    &session,
                    build_new_breadcrumbs(),
                ),
                scope,
                scope_display,
                error: Some(format!("Encryption error: {}", e)),
                key: form.key.clone(),
                value: form.value.clone(),
                description: form.description.clone().unwrap_or_default(),
            };
            return Html(template.render().unwrap()).into_response();
        }
    };

    // Create context variable based on scope
    let result = if let Some(project) = project {
        // Project-level context variable
        #[allow(deprecated)]
        Context::insert(
            &db,
            &Uuid::now_v7(),
            &project.project_id,
            &form.key,
            &encrypted_value,
            form.description.as_deref(),
            &session.current_user_id(),
        )
        .await
    } else {
        // Environment-level context variable
        Context::insert_env(
            &db,
            &Uuid::now_v7(),
            &env_id,
            &form.key,
            &encrypted_value,
            form.description.as_deref(),
            &session.current_user_id(),
        )
        .await
    };

    match result {
        Ok(_) => Redirect::to(&back_url).into_response(),
        Err(e) => {
            tracing::error!("Failed to create context variable: {}", e);
            let template = ContextsNewTemplate {
                title: "New Context Variable",
                page_context: templates::PrivatePageContext::with_breadcrumbs(
                    "contexts",
                    &session,
                    build_new_breadcrumbs(),
                ),
                scope,
                scope_display,
                error: Some(format!("Database error: {}", e)),
                key: form.key.clone(),
                value: form.value.clone(),
                description: form.description.clone().unwrap_or_default(),
            };
            Html(template.render().unwrap()).into_response()
        }
    }
}

pub async fn contexts_edit_handler(
    State(db): State<Arc<DatabasePool>>,
    Path((scope, context_id)): Path<(String, Uuid)>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> impl IntoResponse {
    // Only admins can edit context variables
    if !session.is_current_org_admin {
        let redirect_url = if scope == "env" {
            "/contexts?tab=env".to_string()
        } else {
            format!("/contexts?tab=project&project={}", scope)
        };
        return Redirect::to(&redirect_url).into_response();
    }

    // Get current env_id and org_id
    let env_id = match session.current_env_id() {
        Some(id) => id,
        None => {
            return Redirect::to("/contexts").into_response();
        }
    };

    let org_id = match &session.current_org {
        Some(org) => org.org_id,
        None => {
            return Redirect::to("/contexts").into_response();
        }
    };

    let back_url = if scope == "env" {
        "/contexts?tab=env".to_string()
    } else {
        format!("/contexts?tab=project&project={}", scope)
    };

    // Get context variable
    let context = match Context::get_by_id(&db, &context_id).await {
        Ok(ctx) => ctx,
        Err(_) => {
            return Redirect::to(&back_url).into_response();
        }
    };

    // SECURITY: Verify the context belongs to the current environment
    if scope == "env" {
        // For env-level, check env_id matches
        if context.env_id != Some(env_id) {
            return Redirect::to(&back_url).into_response();
        }
    } else {
        // For project-level, verify project belongs to current environment
        if let Some(project_id) = context.project_id {
            let project = match Project::get_project(&db, &project_id).await {
                Ok(p) => p,
                Err(_) => {
                    return Redirect::to(&back_url).into_response();
                }
            };
            if project.env_id != env_id {
                return Redirect::to(&back_url).into_response();
            }
        } else {
            return Redirect::to(&back_url).into_response();
        }
    }

    // Load encryption and decrypt value
    let encryption = match ContextEncryption::from_env_or_generate_for_dev("local-dev") {
        Ok(enc) => enc,
        Err(e) => {
            tracing::error!("Failed to load encryption: {}", e);
            return Redirect::to(&back_url).into_response();
        }
    };

    let decrypted_val = match context.get_decrypted_value(&encryption, &org_id) {
        Ok(val) => val,
        Err(e) => {
            tracing::error!("Failed to decrypt value: {}", e);
            return Redirect::to(&back_url).into_response();
        }
    };

    let decrypted_value = decrypted_val.pretty_print();

    let scope_display = if scope == "env" {
        "Environment".to_string()
    } else {
        format!("Project: {}", scope)
    };

    // Build breadcrumbs
    let mut breadcrumbs = templates::build_base_breadcrumbs_with_env(&session);
    breadcrumbs.push(templates::BreadcrumbItem::clickable(
        "Context Variables".to_string(),
        back_url,
    ));
    breadcrumbs.push(templates::BreadcrumbItem::current(format!(
        "Edit {}",
        context.key
    )));

    let template = ContextsEditTemplate {
        title: format!("Edit Context Variable: {}", context.key),
        page_context: templates::PrivatePageContext::with_breadcrumbs(
            "contexts",
            &session,
            breadcrumbs,
        ),
        scope,
        scope_display,
        value: decrypted_value.clone(),
        description: context.description.clone().unwrap_or_default(),
        context,
        error: None,
    };

    Html(template.render().unwrap()).into_response()
}

pub async fn contexts_update_handler(
    State(db): State<Arc<DatabasePool>>,
    Path((scope, context_id)): Path<(String, Uuid)>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
    Form(form): Form<ContextForm>,
) -> impl IntoResponse {
    // Only admins can update context variables
    if !session.is_current_org_admin {
        let redirect_url = if scope == "env" {
            "/contexts?tab=env".to_string()
        } else {
            format!("/contexts?tab=project&project={}", scope)
        };
        return Redirect::to(&redirect_url).into_response();
    }

    // Get current env_id and org_id
    let env_id = match session.current_env_id() {
        Some(id) => id,
        None => {
            return Redirect::to("/contexts").into_response();
        }
    };

    let org_id = match &session.current_org {
        Some(org) => org.org_id,
        None => {
            return Redirect::to("/contexts").into_response();
        }
    };

    let back_url = if scope == "env" {
        "/contexts?tab=env".to_string()
    } else {
        format!("/contexts?tab=project&project={}", scope)
    };

    // Get existing context variable
    let context = match Context::get_by_id(&db, &context_id).await {
        Ok(ctx) => ctx,
        Err(_) => {
            return Redirect::to(&back_url).into_response();
        }
    };

    // SECURITY: Verify the context belongs to the current environment
    if scope == "env" {
        if context.env_id != Some(env_id) {
            return Redirect::to(&back_url).into_response();
        }
    } else if let Some(project_id) = context.project_id {
        let project = match Project::get_project(&db, &project_id).await {
            Ok(p) => p,
            Err(_) => {
                return Redirect::to(&back_url).into_response();
            }
        };
        if project.env_id != env_id {
            return Redirect::to(&back_url).into_response();
        }
    } else {
        return Redirect::to(&back_url).into_response();
    }

    // Load encryption
    let encryption = match ContextEncryption::from_env_or_generate_for_dev("local-dev") {
        Ok(enc) => enc,
        Err(e) => {
            tracing::error!("Failed to load encryption: {}", e);
            return Redirect::to(&back_url).into_response();
        }
    };

    let scope_display = if scope == "env" {
        "Environment".to_string()
    } else {
        format!("Project: {}", scope)
    };

    // Validate Hot code
    let validated_val = match Context::validate_hot_value(&form.value) {
        Ok(val) => val,
        Err(e) => {
            let mut breadcrumbs = templates::build_base_breadcrumbs_with_env(&session);
            breadcrumbs.push(templates::BreadcrumbItem::clickable(
                "Context Variables".to_string(),
                back_url,
            ));
            breadcrumbs.push(templates::BreadcrumbItem::current(format!(
                "Edit {}",
                context.key
            )));

            let template = ContextsEditTemplate {
                title: format!("Edit Context Variable: {}", context.key),
                page_context: templates::PrivatePageContext::with_breadcrumbs(
                    "contexts",
                    &session,
                    breadcrumbs,
                ),
                scope,
                scope_display,
                value: form.value.clone(),
                description: form.description.clone().unwrap_or_default(),
                context,
                error: Some(format!("Invalid Hot code: {}", e)),
            };
            return Html(template.render().unwrap()).into_response();
        }
    };

    // Encrypt value
    let encrypted_value = match Context::set_value_from_val(&validated_val, &encryption, &org_id) {
        Ok(enc) => enc,
        Err(e) => {
            tracing::error!("Failed to encrypt value: {}", e);
            return Redirect::to(&back_url).into_response();
        }
    };

    // Update context variable
    match Context::update(
        &db,
        &context_id,
        &encrypted_value,
        form.description.as_deref(),
        &session.current_user_id(),
    )
    .await
    {
        Ok(_) => Redirect::to(&back_url).into_response(),
        Err(e) => {
            tracing::error!("Failed to update context variable: {}", e);
            Redirect::to(&back_url).into_response()
        }
    }
}

pub async fn contexts_delete_handler(
    State(db): State<Arc<DatabasePool>>,
    Path((scope, context_id)): Path<(String, Uuid)>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> impl IntoResponse {
    // Only admins can delete context variables
    let back_url = if scope == "env" {
        "/contexts?tab=env".to_string()
    } else {
        format!("/contexts?tab=project&project={}", scope)
    };

    if !session.is_current_org_admin {
        return Redirect::to(&back_url);
    }

    // Get current env_id for access check
    let env_id = match session.current_env_id() {
        Some(id) => id,
        None => {
            return Redirect::to(&back_url);
        }
    };

    // Get context variable to verify access
    let context = match Context::get_by_id(&db, &context_id).await {
        Ok(ctx) => ctx,
        Err(_) => {
            return Redirect::to(&back_url);
        }
    };

    // SECURITY: Verify the context belongs to the current environment
    if scope == "env" {
        if context.env_id != Some(env_id) {
            return Redirect::to(&back_url);
        }
    } else if let Some(project_id) = context.project_id {
        let project = match Project::get_project(&db, &project_id).await {
            Ok(p) => p,
            Err(_) => {
                return Redirect::to(&back_url);
            }
        };
        if project.env_id != env_id {
            return Redirect::to(&back_url);
        }
    } else {
        return Redirect::to(&back_url);
    }

    // Delete context variable (soft delete)
    match Context::delete(&db, &context_id).await {
        Ok(_) => Redirect::to(&back_url),
        Err(e) => {
            tracing::error!("Failed to delete context variable: {}", e);
            Redirect::to(&back_url)
        }
    }
}

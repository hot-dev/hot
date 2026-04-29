use crate::auth::Session;
use crate::templates;
use ahash::AHashMap;
use askama::Template;
use axum::extract::{Form, Path, Query, State};
use axum::response::{Html, IntoResponse, Redirect};
use hot::db::DatabasePool;
use serde::Deserialize;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use uuid::Uuid;

// Simple in-memory session store for temporary API key data
#[derive(Clone)]
struct ApiKeySession {
    key_hash: String,
    created_at: Instant,
}

type ApiKeySessionStore = Arc<RwLock<AHashMap<Uuid, ApiKeySession>>>;

// Global session store with 5-minute TTL
static API_KEY_SESSIONS: std::sync::OnceLock<ApiKeySessionStore> = std::sync::OnceLock::new();

fn get_session_store() -> &'static ApiKeySessionStore {
    API_KEY_SESSIONS.get_or_init(|| Arc::new(RwLock::new(AHashMap::new())))
}

// Clean up expired sessions (called periodically)
async fn cleanup_expired_sessions() {
    let store = get_session_store();
    let mut sessions = store.write().await;
    let now = Instant::now();
    sessions.retain(|_, session| now.duration_since(session.created_at) < Duration::from_secs(300)); // 5 minutes
}

// Store API key data in session
async fn store_api_key_session(api_key_id: Uuid, key_hash: String) {
    cleanup_expired_sessions().await; // Clean up old sessions
    let store = get_session_store();
    let mut sessions = store.write().await;
    sessions.insert(
        api_key_id,
        ApiKeySession {
            key_hash,
            created_at: Instant::now(),
        },
    );
}

// Retrieve and remove API key data from session
async fn take_api_key_session(api_key_id: &Uuid) -> Option<String> {
    let store = get_session_store();
    let mut sessions = store.write().await;

    // Check if session exists and is not expired
    if let Some(session) = sessions.get(api_key_id) {
        if Instant::now().duration_since(session.created_at) < Duration::from_secs(300) {
            // Session is valid, remove and return the key_hash
            let key_hash = session.key_hash.clone();
            sessions.remove(api_key_id);
            return Some(key_hash);
        } else {
            // Session expired, remove it
            sessions.remove(api_key_id);
        }
    }
    None
}

// Form data structure for creating API keys
#[derive(Deserialize, Debug)]
pub struct ApiKeyCreateForm {
    pub api_key_id: Uuid,
    pub description: String,
    pub access_level: String,
    #[serde(default)]
    pub permissions: String,
}

// Form data structure for editing API keys
#[derive(Deserialize, Debug)]
pub struct ApiKeyEditForm {
    pub description: String,
    pub active: bool,
    pub access_level: String,
    #[serde(default)]
    pub permissions: String,
}

pub async fn keys_list_handler(
    State(db): State<Arc<DatabasePool>>,
    Query(params): Query<AHashMap<String, String>>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> impl IntoResponse {
    // Build breadcrumbs: <org> (<env>) / API Keys
    let mut breadcrumbs = templates::build_base_breadcrumbs_with_env(&session);
    breadcrumbs.push(templates::BreadcrumbItem::current("API Keys".to_string()));

    // Parse query parameters
    let current_page_num = params
        .get("p")
        .and_then(|p| p.parse::<i64>().ok())
        .unwrap_or(1);

    const KEYS_PER_PAGE: i64 = 10;

    // Calculate offset
    let offset = (current_page_num - 1) * KEYS_PER_PAGE;

    // Get API keys for current environment
    let (api_keys, total_keys) = if let Some(env) = &session.current_env {
        let keys = hot::db::api_key::ApiKey::get_api_keys_by_env(
            &db,
            &env.env_id,
            Some(KEYS_PER_PAGE),
            Some(offset),
        )
        .await
        .unwrap_or_default();

        // Get total count by fetching all keys first
        let all_keys = hot::db::api_key::ApiKey::get_api_keys_by_env(&db, &env.env_id, None, None)
            .await
            .unwrap_or_default();
        let total = all_keys.len() as i64;

        (keys, total)
    } else {
        (Vec::new(), 0)
    };

    // Calculate pagination info
    let total_pages = if total_keys > 0 {
        (total_keys + KEYS_PER_PAGE - 1) / KEYS_PER_PAGE
    } else {
        1
    };
    let has_next_page = current_page_num < total_pages;
    let has_prev_page = current_page_num > 1;

    // Calculate pagination window
    let start_page = std::cmp::max(1, current_page_num - 2);
    let end_page = std::cmp::min(total_pages, current_page_num + 2);

    let template = templates::ApiKeysList {
        title: "API Keys",
        page_context: templates::PrivatePageContext::with_breadcrumbs(
            "keys",
            &session,
            breadcrumbs,
        ),
        api_keys,
        is_admin: session.is_current_org_admin,
        current_page_num,
        total_pages,
        start_page,
        end_page,
        has_next_page,
        has_prev_page,
        total_keys,
    };

    Html(template.render().unwrap()).into_response()
}

pub async fn keys_new_handler(
    State(db): State<Arc<DatabasePool>>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> impl IntoResponse {
    // Check if environment is selected
    let current_env = match &session.current_env {
        Some(env) => env,
        None => {
            return Html("No environment selected".to_string()).into_response();
        }
    };

    // Check if user is admin
    if !session.is_current_org_admin {
        return Html("You must be an admin to create API keys".to_string()).into_response();
    }

    // Generate a new API key ID and the actual key
    let api_key_id = uuid::Uuid::now_v7();
    let (generated_key, key_hash) = match hot::db::api_key::ApiKey::generate_api_key(&api_key_id) {
        Ok(key_data) => key_data,
        Err(_) => return Html("Failed to generate API key".to_string()).into_response(),
    };

    // Store the key hash in the session
    store_api_key_session(api_key_id, key_hash.clone()).await;

    let (mcp_tools_json, webhooks_json) =
        super::build_permission_path_options(&db, &current_env.env_id).await;

    // Build breadcrumbs: <org> (<env>) / API Keys / New
    let mut breadcrumbs = templates::build_base_breadcrumbs_with_env(&session);
    breadcrumbs.push(templates::BreadcrumbItem::clickable(
        "API Keys".to_string(),
        "/keys".to_string(),
    ));
    breadcrumbs.push(templates::BreadcrumbItem::current("New".to_string()));

    let template = templates::ApiKeysNew {
        title: "New API Key",
        page_context: templates::PrivatePageContext::with_breadcrumbs(
            "keys",
            &session,
            breadcrumbs,
        ),
        generated_key,
        api_key_id,
        error_message: "",
        description: "",
        access_level: "full",
        mcp_tools_json,
        webhooks_json,
    };

    Html(template.render().unwrap()).into_response()
}

pub async fn keys_edit_handler(
    Path(api_key_id): Path<Uuid>,
    State(db): State<Arc<DatabasePool>>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> impl IntoResponse {
    // Get current env_id for access check
    let current_env = match &session.current_env {
        Some(env) => env,
        None => {
            return Redirect::to("/keys").into_response();
        }
    };

    // Get API key details
    match hot::db::api_key::ApiKey::get_api_key(&db, &api_key_id).await {
        Ok(api_key) => {
            // SECURITY: Verify the API key belongs to the current environment
            if api_key.env_id != current_env.env_id {
                return Redirect::to("/keys").into_response();
            }

            // Determine access level from permissions
            let access_level = if api_key.has_full_permissions() {
                "full"
            } else {
                "restricted"
            };

            // Serialize current permissions for the form
            let permissions_json =
                serde_json::to_string(&api_key.permissions).unwrap_or_else(|_| "{}".to_string());

            let (mcp_tools_json, webhooks_json) =
                super::build_permission_path_options(&db, &current_env.env_id).await;

            // Build breadcrumbs: <org> (<env>) / API Keys / <description> / Edit
            let mut breadcrumbs = templates::build_base_breadcrumbs_with_env(&session);
            breadcrumbs.push(templates::BreadcrumbItem::clickable(
                "API Keys".to_string(),
                "/keys".to_string(),
            ));
            breadcrumbs.push(templates::BreadcrumbItem::clickable(
                api_key.description.clone(),
                format!("/keys/{}", api_key_id),
            ));
            breadcrumbs.push(templates::BreadcrumbItem::current("Edit".to_string()));

            let template = templates::ApiKeysEdit {
                title: &format!("Edit API Key: {}", api_key.description),
                page_context: templates::PrivatePageContext::with_breadcrumbs(
                    "keys",
                    &session,
                    breadcrumbs,
                ),
                api_key,
                error_message: "",
                access_level,
                permissions_json,
                mcp_tools_json,
                webhooks_json,
            };

            Html(template.render().unwrap()).into_response()
        }
        Err(_) => {
            // API key not found, redirect to keys list
            Redirect::to("/keys").into_response()
        }
    }
}

pub async fn keys_create_handler(
    State(db): State<Arc<DatabasePool>>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
    Form(form): Form<ApiKeyCreateForm>,
) -> Result<Redirect, Html<String>> {
    let current_env = match &session.current_env {
        Some(env) => env,
        None => {
            return Err(Html("No environment selected".to_string()));
        }
    };

    // Check if user is admin
    if !session.is_current_org_admin {
        return Err(Html("You must be an admin to create API keys".to_string()));
    }

    // Validate form data
    if form.description.trim().is_empty() {
        return Err(render_keys_new_with_error(
            &session,
            "Description is required",
            "",
            &form.api_key_id,
        )
        .await);
    }

    // Retrieve the key hash from the session store
    let key_hash = match take_api_key_session(&form.api_key_id).await {
        Some(hash) => hash,
        None => {
            return Err(render_keys_new_with_error(
                &session,
                "Session expired. Please try again.",
                &form.description,
                &form.api_key_id,
            )
            .await);
        }
    };

    // Parse the key hash JSON
    let key_data: serde_json::Value = match serde_json::from_str(&key_hash) {
        Ok(data) => data,
        Err(_) => {
            return Err(render_keys_new_with_error(
                &session,
                "Failed to process API key",
                &form.description,
                &form.api_key_id,
            )
            .await);
        }
    };

    // Build and validate permissions from form data
    let permissions_json = if form.access_level == "restricted" {
        if form.permissions.is_empty() {
            serde_json::json!({})
        } else {
            let parsed: serde_json::Value = match serde_json::from_str(&form.permissions) {
                Ok(v) => v,
                Err(e) => {
                    return Err(render_keys_new_with_error(
                        &session,
                        &format!("Invalid permissions JSON: {}", e),
                        &form.description,
                        &form.api_key_id,
                    )
                    .await);
                }
            };
            // Validate the permissions structure
            if let Err(e) = hot::permission::Permissions::from_json_validated(&parsed) {
                return Err(render_keys_new_with_error(
                    &session,
                    &format!("Invalid permissions: {}", e),
                    &form.description,
                    &form.api_key_id,
                )
                .await);
            }
            parsed
        }
    } else {
        serde_json::json!({"*:*": ["*"]}) // full access
    };

    // Insert the API key into the database
    match hot::db::api_key::ApiKey::insert_api_key(
        &db,
        &form.api_key_id,
        &current_env.env_id,
        &form.description,
        &key_data,
        &session.current_user_id(),
        &permissions_json,
    )
    .await
    {
        Ok(_) => {
            // Redirect to the keys list
            Ok(Redirect::to("/keys"))
        }
        Err(_) => Err(render_keys_new_with_error(
            &session,
            "Failed to create API key",
            &form.description,
            &form.api_key_id,
        )
        .await),
    }
}

pub async fn keys_update_handler(
    Path(api_key_id): Path<Uuid>,
    State(db): State<Arc<DatabasePool>>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
    Form(form): Form<ApiKeyEditForm>,
) -> Result<Redirect, Html<String>> {
    let current_env = match &session.current_env {
        Some(env) => env,
        None => {
            return Err(Html("No environment selected".to_string()));
        }
    };

    // Check if user is admin
    if !session.is_current_org_admin {
        return Err(Html("You must be an admin to edit API keys".to_string()));
    }

    // Get the API key
    let api_key = match hot::db::api_key::ApiKey::get_api_key(&db, &api_key_id).await {
        Ok(key) => key,
        Err(_) => return Ok(Redirect::to("/keys")),
    };

    // Check if API key belongs to current environment
    if api_key.env_id != current_env.env_id {
        return Ok(Redirect::to("/keys"));
    }

    // Validate form data
    if form.description.trim().is_empty() {
        return Err(
            render_keys_edit_with_error(&session, &api_key, "Description is required").await,
        );
    }

    // Update the API key description
    if hot::db::api_key::ApiKey::update_description(
        &db,
        &api_key_id,
        &form.description,
        &session.current_user_id(),
    )
    .await
    .is_err()
    {
        return Err(
            render_keys_edit_with_error(&session, &api_key, "Failed to update API key").await,
        );
    }

    // Update the active status if it changed
    if form.active != api_key.active
        && let Err(_) = hot::db::api_key::ApiKey::toggle_active(
            &db,
            &api_key_id,
            form.active,
            &session.current_user_id(),
        )
        .await
    {
        return Err(render_keys_edit_with_error(
            &session,
            &api_key,
            "Failed to update API key status",
        )
        .await);
    }

    // Update and validate permissions
    let permissions_json = if form.access_level == "restricted" {
        if form.permissions.is_empty() {
            serde_json::json!({})
        } else {
            let parsed: serde_json::Value = match serde_json::from_str(&form.permissions) {
                Ok(v) => v,
                Err(e) => {
                    return Err(render_keys_edit_with_error(
                        &session,
                        &api_key,
                        &format!("Invalid permissions JSON: {}", e),
                    )
                    .await);
                }
            };
            // Validate the permissions structure
            if let Err(e) = hot::permission::Permissions::from_json_validated(&parsed) {
                return Err(render_keys_edit_with_error(
                    &session,
                    &api_key,
                    &format!("Invalid permissions: {}", e),
                )
                .await);
            }
            parsed
        }
    } else {
        serde_json::json!({"*:*": ["*"]}) // full access
    };

    if hot::db::api_key::ApiKey::update_permissions(
        &db,
        &api_key_id,
        &permissions_json,
        &session.current_user_id(),
    )
    .await
    .is_err()
    {
        return Err(render_keys_edit_with_error(
            &session,
            &api_key,
            "Failed to update API key permissions",
        )
        .await);
    }

    // Redirect to the keys list
    Ok(Redirect::to("/keys"))
}

// Helper function to render keys new page with error
async fn render_keys_new_with_error(
    session: &Session,
    error_message: &str,
    description: &str,
    api_key_id: &Uuid,
) -> Html<String> {
    // Build breadcrumbs: <org> (<env>) / API Keys / New
    let mut breadcrumbs = templates::build_base_breadcrumbs_with_env(session);
    breadcrumbs.push(templates::BreadcrumbItem::clickable(
        "API Keys".to_string(),
        "/keys".to_string(),
    ));
    breadcrumbs.push(templates::BreadcrumbItem::current("New".to_string()));

    let template = templates::ApiKeysNew {
        title: "New API Key",
        page_context: templates::PrivatePageContext::with_breadcrumbs("keys", session, breadcrumbs),
        generated_key: "".to_string(),
        api_key_id: *api_key_id,
        error_message,
        description,
        access_level: "full",
        mcp_tools_json: "[]".into(),
        webhooks_json: "[]".into(),
    };

    Html(template.render().unwrap())
}

// Helper function to render keys edit page with error
async fn render_keys_edit_with_error(
    session: &Session,
    api_key: &hot::db::api_key::ApiKey,
    error_message: &str,
) -> Html<String> {
    // Build breadcrumbs: <org> (<env>) / API Keys / <description> / Edit
    let mut breadcrumbs = templates::build_base_breadcrumbs_with_env(session);
    breadcrumbs.push(templates::BreadcrumbItem::clickable(
        "API Keys".to_string(),
        "/keys".to_string(),
    ));
    breadcrumbs.push(templates::BreadcrumbItem::clickable(
        api_key.description.clone(),
        format!("/keys/{}", api_key.api_key_id),
    ));
    breadcrumbs.push(templates::BreadcrumbItem::current("Edit".to_string()));

    let access_level = if api_key.has_full_permissions() {
        "full"
    } else {
        "restricted"
    };

    let permissions_json =
        serde_json::to_string(&api_key.permissions).unwrap_or_else(|_| "{}".to_string());

    let template = templates::ApiKeysEdit {
        title: &format!("Edit API Key: {}", api_key.description),
        page_context: templates::PrivatePageContext::with_breadcrumbs("keys", session, breadcrumbs),
        api_key: api_key.clone(),
        error_message,
        access_level,
        permissions_json,
        mcp_tools_json: "[]".into(),
        webhooks_json: "[]".into(),
    };

    Html(template.render().unwrap())
}

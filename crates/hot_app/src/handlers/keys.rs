use crate::auth::Session;
use crate::handlers::list_query;
use crate::templates;
use ahash::AHashMap;
use askama::Template;
use axum::extract::{Form, Path, Query, State};
use axum::response::{Html, IntoResponse, Redirect};
use chrono::Utc;
use hot::db::DatabasePool;
use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, Validation, decode, encode};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use uuid::Uuid;

/// How long a generated key stays redeemable after the new-key page renders.
const API_KEY_HANDOFF_TTL_SECS: i64 = 300;

/// The new-key page shows a secret and the create form commits its hash. The
/// hash travels in a signed form token rather than process memory, so any app
/// instance can complete the handoff.
#[derive(Serialize, Deserialize)]
struct ApiKeyHandoff {
    api_key_id: Uuid,
    user_id: Uuid,
    org_id: Uuid,
    env_id: Uuid,
    key_hash: String,
    exp: usize,
}

/// Separate the handoff key from the session-token key so neither token type
/// validates as the other.
fn api_key_handoff_secret() -> Result<Vec<u8>, String> {
    let secret = crate::auth::get_session_secret()?;
    Ok(format!("{secret}:api-key-handoff").into_bytes())
}

fn sign_api_key_handoff(
    api_key_id: Uuid,
    user_id: Uuid,
    org_id: Uuid,
    env_id: Uuid,
    key_hash: String,
) -> Result<String, String> {
    let exp = (Utc::now().timestamp() + API_KEY_HANDOFF_TTL_SECS) as usize;
    let claims = ApiKeyHandoff {
        api_key_id,
        user_id,
        org_id,
        env_id,
        key_hash,
        exp,
    };
    let key = EncodingKey::from_secret(&api_key_handoff_secret()?);
    encode(&Header::new(Algorithm::HS256), &claims, &key)
        .map_err(|e| format!("Failed to sign API key handoff: {e}"))
}

/// Return the key hash when the token is authentic, unexpired, and was issued
/// for this key, user, and execution scope. Reuse fails at insert because the
/// key ID is the primary key.
fn verify_api_key_handoff(
    token: &str,
    api_key_id: &Uuid,
    user_id: Uuid,
    org_id: Uuid,
    env_id: Uuid,
) -> Option<String> {
    let secret = api_key_handoff_secret().ok()?;
    let mut validation = Validation::new(Algorithm::HS256);
    validation.leeway = 0;
    let handoff = decode::<ApiKeyHandoff>(token, &DecodingKey::from_secret(&secret), &validation)
        .ok()?
        .claims;
    // A pending credential belongs to the identity and execution scope that
    // saw its secret. A mismatched request must not redeem it.
    if (
        handoff.api_key_id,
        handoff.user_id,
        handoff.org_id,
        handoff.env_id,
    ) != (*api_key_id, user_id, org_id, env_id)
    {
        return None;
    }
    Some(handoff.key_hash)
}

// Form data structure for creating API keys
#[derive(Deserialize, Debug)]
pub struct ApiKeyCreateForm {
    pub api_key_id: Uuid,
    #[serde(default)]
    pub handoff_token: String,
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

fn keys_list_redirect(session: &Session) -> Redirect {
    // A different tab can change navigation cookies before this redirect is
    // followed. Keep the destination bound to the workspace that handled it.
    let target = match &session.current_env {
        Some(env) => format!("/keys?org_id={}&env_id={}", env.org_id, env.env_id),
        None => "/keys".to_string(),
    };
    Redirect::to(&target)
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
    const KEYS_PER_PAGE: i64 = 10;
    let page = list_query::PageParams::parse(&params, KEYS_PER_PAGE);
    let current_page_num = page.current_page_num;
    let offset = page.offset;

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
    let pagination = list_query::PaginationWindow::new(total_keys, &page);
    let total_pages = pagination.total_pages;
    let has_next_page = pagination.has_next_page;
    let has_prev_page = pagination.has_prev_page;
    let start_page = pagination.start_page;
    let end_page = pagination.end_page;

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

    let handoff_token = match sign_api_key_handoff(
        api_key_id,
        session.current_user_id(),
        current_env.org_id,
        current_env.env_id,
        key_hash,
    ) {
        Ok(token) => token,
        Err(_) => return Html("Failed to generate API key".to_string()).into_response(),
    };

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
        handoff_token,
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
            return keys_list_redirect(&session).into_response();
        }
    };

    // Get API key details
    match hot::db::api_key::ApiKey::get_api_key(&db, &api_key_id).await {
        Ok(api_key) => {
            // SECURITY: Verify the API key belongs to the current environment
            if api_key.env_id != current_env.env_id {
                return keys_list_redirect(&session).into_response();
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
            keys_list_redirect(&session).into_response()
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
            &form.handoff_token,
        )
        .await);
    }

    let key_hash = match verify_api_key_handoff(
        &form.handoff_token,
        &form.api_key_id,
        session.current_user_id(),
        current_env.org_id,
        current_env.env_id,
    ) {
        Some(hash) => hash,
        None => {
            return Err(render_keys_new_with_error(
                &session,
                "Session expired. Please try again.",
                &form.description,
                &form.api_key_id,
                &form.handoff_token,
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
                &form.handoff_token,
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
                        &form.handoff_token,
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
                    &form.handoff_token,
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
            Ok(keys_list_redirect(&session))
        }
        Err(_) => Err(render_keys_new_with_error(
            &session,
            "Failed to create API key",
            &form.description,
            &form.api_key_id,
            &form.handoff_token,
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
        Err(_) => return Ok(keys_list_redirect(&session)),
    };

    // Check if API key belongs to current environment
    if api_key.env_id != current_env.env_id {
        return Ok(keys_list_redirect(&session));
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
    Ok(keys_list_redirect(&session))
}

// Helper function to render keys new page with error
async fn render_keys_new_with_error(
    session: &Session,
    error_message: &str,
    description: &str,
    api_key_id: &Uuid,
    handoff_token: &str,
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
        handoff_token: handoff_token.to_string(),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn ids() -> (Uuid, Uuid, Uuid, Uuid) {
        (
            Uuid::now_v7(),
            Uuid::now_v7(),
            Uuid::now_v7(),
            Uuid::now_v7(),
        )
    }

    #[test]
    fn handoff_redeems_without_process_state() {
        let (key, user, org, env) = ids();
        let token = sign_api_key_handoff(key, user, org, env, "hash".into()).unwrap();
        // Verification reads nothing but the token and the shared secret, so
        // another app instance accepts it.
        assert_eq!(
            verify_api_key_handoff(&token, &key, user, org, env).as_deref(),
            Some("hash")
        );
    }

    #[test]
    fn handoff_rejects_other_key_identity_or_scope() {
        let (key, user, org, env) = ids();
        let token = sign_api_key_handoff(key, user, org, env, "hash".into()).unwrap();
        let other = Uuid::now_v7();
        assert!(verify_api_key_handoff(&token, &other, user, org, env).is_none());
        assert!(verify_api_key_handoff(&token, &key, other, org, env).is_none());
        assert!(verify_api_key_handoff(&token, &key, user, other, env).is_none());
        assert!(verify_api_key_handoff(&token, &key, user, org, other).is_none());
    }

    #[test]
    fn handoff_rejects_expired_tampered_and_session_signed_tokens() {
        let (key, user, org, env) = ids();
        let expired = ApiKeyHandoff {
            api_key_id: key,
            user_id: user,
            org_id: org,
            env_id: env,
            key_hash: "hash".into(),
            exp: (Utc::now().timestamp() - 1) as usize,
        };
        let token = encode(
            &Header::new(Algorithm::HS256),
            &expired,
            &EncodingKey::from_secret(&api_key_handoff_secret().unwrap()),
        )
        .unwrap();
        assert!(verify_api_key_handoff(&token, &key, user, org, env).is_none());

        let fresh = ApiKeyHandoff {
            exp: (Utc::now().timestamp() + 60) as usize,
            ..expired
        };
        let session_key = crate::auth::get_session_secret().unwrap();
        let session_signed = encode(
            &Header::new(Algorithm::HS256),
            &fresh,
            &EncodingKey::from_secret(session_key.as_bytes()),
        )
        .unwrap();
        assert!(verify_api_key_handoff(&session_signed, &key, user, org, env).is_none());

        let mut tampered = sign_api_key_handoff(key, user, org, env, "hash".into()).unwrap();
        tampered.pop();
        assert!(verify_api_key_handoff(&tampered, &key, user, org, env).is_none());
        assert!(verify_api_key_handoff("", &key, user, org, env).is_none());
    }
}

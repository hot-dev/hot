use crate::auth::Session;
use crate::handlers::list_query;
use crate::templates;
use ahash::AHashMap;
use askama::Template;
use axum::extract::{Form, Path, Query, State};
use axum::response::{Html, IntoResponse, Redirect, Response};
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
    // Check if user is admin
    if session.current_env.is_some() && !session.is_current_org_admin {
        return Html("You must be an admin to create API keys".to_string()).into_response();
    }

    render_keys_new(
        &db,
        &session,
        String::new(),
        "",
        "full",
        &serde_json::Value::Object(Default::default()),
    )
    .await
    .into_response()
}

/// Render the new-key page with a freshly generated key and handoff token.
///
/// Error re-renders come here too. They keep the submitted description, access level and
/// permissions, but show a new key: the secret shown before is not recoverable from the form,
/// and committing a key the user never saw would leave them without its secret.
async fn render_keys_new(
    db: &DatabasePool,
    session: &Session,
    error_message: String,
    description: &str,
    access_level: &str,
    permissions: &serde_json::Value,
) -> Response {
    let current_env = match &session.current_env {
        Some(env) => env,
        None => return Html("No environment selected".to_string()).into_response(),
    };

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

    let (mcp_tool_options, webhook_options) =
        super::build_permission_path_options(db, &current_env.env_id).await;

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
        generated_key,
        api_key_id,
        handoff_token,
        error_message: &error_message,
        description,
        access_level,
        permissions: permissions.clone(),
        mcp_tool_options,
        webhook_options,
    };

    // The page shows a key secret: keep it out of caches and Referer headers.
    (
        [
            ("Cache-Control", "no-store"),
            ("Referrer-Policy", "no-referrer"),
        ],
        Html(template.render().unwrap()),
    )
        .into_response()
}

/// The access level a key form selected. Anything other than an explicit choice is rejected,
/// so a missing or mangled field can never fall through to full access.
fn selected_access_level(raw: &str) -> Option<&'static str> {
    match raw {
        "full" => Some("full"),
        "restricted" => Some("restricted"),
        _ => None,
    }
}

/// The submitted permissions, for re-rendering a form after an error. Text that isn't JSON is
/// passed through as a string, which the page's permission builder ignores.
fn submitted_permissions(raw: &str) -> serde_json::Value {
    if raw.trim().is_empty() {
        serde_json::Value::Object(Default::default())
    } else {
        serde_json::from_str(raw).unwrap_or_else(|_| serde_json::Value::String(raw.to_string()))
    }
}

/// Parse and validate the permissions a key form submitted.
fn permissions_from_form(access_level: &str, raw: &str) -> Result<serde_json::Value, String> {
    if access_level != "restricted" {
        return Ok(serde_json::json!({"*:*": ["*"]})); // full access
    }
    if raw.is_empty() {
        return Ok(serde_json::json!({}));
    }
    let parsed: serde_json::Value =
        serde_json::from_str(raw).map_err(|e| format!("Invalid permissions JSON: {}", e))?;
    // Validate the permissions structure
    hot::permission::Permissions::from_json_validated(&parsed)
        .map_err(|e| format!("Invalid permissions: {}", e))?;
    Ok(parsed)
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
            let permissions = api_key.permissions.clone();
            let description = api_key.description.clone();

            render_keys_edit(
                &db,
                &session,
                api_key,
                String::new(),
                &description,
                access_level,
                permissions,
            )
            .await
            .into_response()
        }
        Err(_) => {
            // API key not found, redirect to keys list
            keys_list_redirect(&session).into_response()
        }
    }
}

/// Render the edit-key page. Error re-renders pass the submitted values, so a failed save
/// shows what the user entered rather than reverting to the stored key.
async fn render_keys_edit(
    db: &DatabasePool,
    session: &Session,
    api_key: hot::db::api_key::ApiKey,
    error_message: String,
    description: &str,
    access_level: &str,
    permissions: serde_json::Value,
) -> Response {
    let (mcp_tool_options, webhook_options) =
        super::build_permission_path_options(db, &api_key.env_id).await;

    // Build breadcrumbs: <org> (<env>) / API Keys / <description> / Edit. There is no key
    // detail page, so the description crumb is not a link.
    let mut breadcrumbs = templates::build_base_breadcrumbs_with_env(session);
    breadcrumbs.push(templates::BreadcrumbItem::clickable(
        "API Keys".to_string(),
        "/keys".to_string(),
    ));
    breadcrumbs.push(templates::BreadcrumbItem::current(
        api_key.description.clone(),
    ));
    breadcrumbs.push(templates::BreadcrumbItem::current("Edit".to_string()));

    let template = templates::ApiKeysEdit {
        title: &format!("Edit API Key: {}", api_key.description),
        page_context: templates::PrivatePageContext::with_breadcrumbs("keys", session, breadcrumbs),
        api_key,
        error_message: &error_message,
        description,
        access_level,
        permissions,
        mcp_tool_options,
        webhook_options,
    };

    Html(template.render().unwrap()).into_response()
}

pub async fn keys_create_handler(
    State(db): State<Arc<DatabasePool>>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
    Form(form): Form<ApiKeyCreateForm>,
) -> Result<Redirect, Response> {
    let current_env = match &session.current_env {
        Some(env) => env,
        None => {
            return Err(Html("No environment selected".to_string()).into_response());
        }
    };

    // Check if user is admin
    if !session.is_current_org_admin {
        return Err(Html("You must be an admin to create API keys".to_string()).into_response());
    }

    // Re-render with the submitted choices. A Restricted submission never comes back as Full.
    let submitted_level = selected_access_level(&form.access_level).unwrap_or("restricted");
    let submitted = submitted_permissions(&form.permissions);
    let retry = |message: String| {
        render_keys_new(
            &db,
            &session,
            message,
            &form.description,
            submitted_level,
            &submitted,
        )
    };

    // Validate form data
    if form.description.trim().is_empty() {
        return Err(retry("Description is required".to_string()).await);
    }
    let Some(access_level) = selected_access_level(&form.access_level) else {
        return Err(retry("Choose Full Access or Restricted".to_string()).await);
    };

    let key_hash = match verify_api_key_handoff(
        &form.handoff_token,
        &form.api_key_id,
        session.current_user_id(),
        current_env.org_id,
        current_env.env_id,
    ) {
        Some(hash) => hash,
        None => {
            // An expired form, or one rendered by another app version during a deploy.
            return Err(retry(
                "This form expired before it was submitted. A new key was generated below: copy it, then create the key again.".to_string(),
            )
            .await);
        }
    };

    // Parse the key hash JSON
    let key_data: serde_json::Value = match serde_json::from_str(&key_hash) {
        Ok(data) => data,
        Err(_) => return Err(retry("Failed to process API key".to_string()).await),
    };

    // Build and validate permissions from form data
    let permissions_json = match permissions_from_form(access_level, &form.permissions) {
        Ok(permissions) => permissions,
        Err(message) => return Err(retry(message).await),
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
        Err(_) => Err(retry("Failed to create API key".to_string()).await),
    }
}

pub async fn keys_update_handler(
    Path(api_key_id): Path<Uuid>,
    State(db): State<Arc<DatabasePool>>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
    Form(form): Form<ApiKeyEditForm>,
) -> Result<Redirect, Response> {
    let current_env = match &session.current_env {
        Some(env) => env,
        None => {
            return Err(Html("No environment selected".to_string()).into_response());
        }
    };

    // Check if user is admin
    if !session.is_current_org_admin {
        return Err(Html("You must be an admin to edit API keys".to_string()).into_response());
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

    // Re-render with the submitted values rather than the stored ones.
    let submitted_level = selected_access_level(&form.access_level).unwrap_or("restricted");
    let retry = |message: String| {
        render_keys_edit(
            &db,
            &session,
            api_key.clone(),
            message,
            &form.description,
            submitted_level,
            submitted_permissions(&form.permissions),
        )
    };

    // Validate everything before changing anything, so a rejected form saves nothing.
    if form.description.trim().is_empty() {
        return Err(retry("Description is required".to_string()).await);
    }
    let Some(access_level) = selected_access_level(&form.access_level) else {
        return Err(retry("Choose Full Access or Restricted".to_string()).await);
    };
    let permissions_json = match permissions_from_form(access_level, &form.permissions) {
        Ok(permissions) => permissions,
        Err(message) => return Err(retry(message).await),
    };

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
        return Err(retry("Failed to update API key".to_string()).await);
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
        return Err(retry("Failed to update API key status".to_string()).await);
    }

    if hot::db::api_key::ApiKey::update_permissions(
        &db,
        &api_key_id,
        &permissions_json,
        &session.current_user_id(),
    )
    .await
    .is_err()
    {
        return Err(retry("Failed to update API key permissions".to_string()).await);
    }

    // Redirect to the keys list
    Ok(keys_list_redirect(&session))
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

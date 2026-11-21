// Handlers answer errors with a rendered page: an axum Response, which is large by design.
#![allow(clippy::result_large_err)]

use crate::auth::Session;
use crate::handlers::list_query;
use crate::templates;
use ahash::AHashMap;
use askama::Template;
use axum::extract::{Form, Path, Query, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Redirect, Response};
use chrono::{DateTime, Duration, Utc};
use hot::db::api_key::{ApiKey, ApiKeyError, RotatedSecret};
use hot::db::env::Env;
use hot::db::org::Org;
use hot::db::org_api_key::{
    CreateOrgApiKey, KeyStatus, MAX_ROTATION_OVERLAP_SECONDS, OrgApiKey, OrgApiKeyError,
    PreparedOrgKey,
};
use hot::db::org_note::OrgNote;
use hot::db::{BootstrapOperation, DatabasePool, DatabaseScope};
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
    let key = EncodingKey::from_secret(&api_key_handoff_secret()?)
        .map_err(|e| format!("Failed to sign API key handoff: {e}"))?;
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
    let handoff =
        decode::<ApiKeyHandoff>(token, &DecodingKey::from_secret(&secret).ok()?, &validation)
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
    #[serde(default)]
    pub expires_in_days: String,
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

/// A key status as the key pages label it, with its badge style.
fn status_view(status: KeyStatus) -> (&'static str, &'static str) {
    match status {
        KeyStatus::Active => ("Active", "badge-success"),
        KeyStatus::Disabled => ("Disabled", "badge-neutral"),
        KeyStatus::Revoked => ("Revoked", "badge-failure"),
        KeyStatus::Expired => ("Expired", "badge-failure"),
        KeyStatus::IssuerInactive => ("Issuer inactive", "badge-failure"),
        KeyStatus::Suspended => ("Suspended", "badge-pending"),
    }
}

fn keys_load_failed(session: &Session) -> Response {
    super::error_page(
        session,
        StatusCode::INTERNAL_SERVER_ERROR,
        "Unable to load API keys",
    )
}

/// An environment key with its current status, following its issuer chain.
async fn key_row(
    db: &DatabasePool,
    session: &Session,
    env: &Env,
    key: ApiKey,
) -> Result<templates::ApiKeyRow, Response> {
    let lookup = db.with_scope(DatabaseScope::Bootstrap(BootstrapOperation::Authentication));
    let state = ApiKey::get_credential_state(&lookup, &key.api_key_id)
        .await
        .map_err(|error| {
            tracing::error!(%error, "API key state lookup failed");
            keys_load_failed(session)
        })?;
    let issuer = OrgApiKey::issuer_of_env_key(&lookup, key.api_key_id)
        .await
        .map_err(|_| keys_load_failed(session))?;
    let status = OrgApiKey::env_key_status(&lookup, &key, &state, issuer, env)
        .await
        .map_err(|_| keys_load_failed(session))?;
    let (status_label, status_badge) = status_view(status);
    Ok(templates::ApiKeyRow {
        status_label,
        status_badge,
        expires_at: state
            .expires_at
            .map(|at| super::sub_orgs::display_time(session, &at)),
        previous_secret_expires_at: state
            .previous_secret_expires_at()
            .filter(|_| status.is_active())
            .map(|at| super::sub_orgs::display_time(session, &at)),
        issuer,
        revoked: state.revoked_at.is_some(),
        rotatable: status.is_active(),
        key,
    })
}

/// The workspace's organization, when the user may manage its org API keys:
/// only an administrator of a parent organization may.
async fn org_keys_root(db: &DatabasePool, session: &Session) -> Option<Org> {
    let org = session.current_org.as_ref()?;
    if org.is_individual() || org.is_child() {
        return None;
    }
    session
        .can_manage_account(db, &org.org_id)
        .await
        .then(|| org.clone())
}

/// A page that shows a key secret: keep it out of caches and Referer headers.
fn secret_page(status: StatusCode, html: String) -> Response {
    (
        status,
        [
            ("Cache-Control", "no-store"),
            ("Referrer-Policy", "no-referrer"),
        ],
        Html(html),
    )
        .into_response()
}

/// The page that shows a rotated key's new secret, once.
fn rotated_secret_page(
    page_context: templates::PrivatePageContext,
    heading: &str,
    note: &str,
    token: String,
    back_url: String,
    back_label: &str,
) -> Response {
    let template = templates::KeySecret {
        title: "New Secret",
        page_context,
        heading,
        note,
        token,
        back_url,
        back_label,
    };
    secret_page(StatusCode::OK, template.render().unwrap())
}

/// Render the Environment keys tab of the API Keys page: the workspace
/// environment's keys. `root` decides whether the page offers the Org keys tab.
async fn render_keys_page(
    db: &DatabasePool,
    session: &Session,
    params: &AHashMap<String, String>,
    root: Option<Org>,
    error: Option<String>,
) -> Response {
    // Build breadcrumbs: <org> (<env>) / API Keys
    let mut breadcrumbs = templates::build_base_breadcrumbs_with_env(session);
    breadcrumbs.push(templates::BreadcrumbItem::current("API Keys".to_string()));

    // Parse query parameters
    const KEYS_PER_PAGE: i64 = 10;
    let page = list_query::PageParams::parse(params, KEYS_PER_PAGE);
    let current_page_num = page.current_page_num;
    let offset = page.offset;

    // Get API keys for current environment
    let mut api_keys = Vec::new();
    let mut total_keys = 0;
    if let Some(env) = &session.current_env {
        // Read under the environment's own scope: a handler on an organization
        // route would otherwise see no keys and report an empty environment.
        let scoped = db.with_scope(DatabaseScope::Environment {
            org_id: env.org_id,
            env_id: env.env_id,
        });
        let keys = match ApiKey::get_api_keys_by_env(
            &scoped,
            &env.env_id,
            Some(KEYS_PER_PAGE),
            Some(offset),
        )
        .await
        {
            Ok(keys) => keys,
            Err(error) => {
                tracing::error!(%error, env_id = %env.env_id, "API key list failed");
                return keys_load_failed(session);
            }
        };
        for key in keys {
            match key_row(db, session, env, key).await {
                Ok(row) => api_keys.push(row),
                Err(page) => return page,
            }
        }
        total_keys = match ApiKey::count_api_keys_by_env(&scoped, &env.env_id).await {
            Ok(total) => total,
            Err(error) => {
                tracing::error!(%error, env_id = %env.env_id, "API key count failed");
                return keys_load_failed(session);
            }
        };
    }

    // Calculate pagination info
    let pagination = list_query::PaginationWindow::new(total_keys, &page);
    let total_pages = pagination.total_pages;
    let has_next_page = pagination.has_next_page;
    let has_prev_page = pagination.has_prev_page;
    let start_page = pagination.start_page;
    let end_page = pagination.end_page;

    let status = if error.is_some() {
        StatusCode::BAD_REQUEST
    } else {
        StatusCode::OK
    };
    let template = templates::ApiKeysList {
        title: "API Keys",
        page_context: templates::PrivatePageContext::with_breadcrumbs("keys", session, breadcrumbs),
        api_keys,
        keys_tab: "env",
        org_keys_root: root,
        error_message: error,
        is_admin: session.is_current_org_admin,
        current_page_num,
        total_pages,
        start_page,
        end_page,
        has_next_page,
        has_prev_page,
        total_keys,
    };

    (status, Html(template.render().unwrap())).into_response()
}

pub async fn keys_list_handler(
    State(db): State<Arc<DatabasePool>>,
    Query(params): Query<AHashMap<String, String>>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> impl IntoResponse {
    let root = org_keys_root(&db, &session).await;
    render_keys_page(&db, &session, &params, root, None).await
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
    expires_in_days: &str,
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
        expires_in_days,
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

/// The most permissions JSON a key form parses. A grant is about a hundred
/// bytes, so this is room for thousands of them, and far less than a request
/// body may hold.
const MAX_PERMISSIONS_JSON_BYTES: usize = 256 * 1024;

const PERMISSIONS_TOO_LARGE: &str = "Permissions are too large. Use fewer rules, or wildcards.";

/// The submitted permissions, for re-rendering a form after an error. Text that isn't JSON is
/// passed through as a string, which the page's permission editor shows to be fixed. Oversized
/// text is neither parsed nor sent back.
fn submitted_permissions(raw: &str) -> serde_json::Value {
    if raw.len() > MAX_PERMISSIONS_JSON_BYTES {
        return serde_json::Value::Object(Default::default());
    }
    if raw.trim().is_empty() {
        serde_json::Value::Object(Default::default())
    } else {
        serde_json::from_str(raw).unwrap_or_else(|_| serde_json::Value::String(raw.to_string()))
    }
}

/// The expiry a key form's optional "expires in days" field asks for.
fn expiry_from_days(raw: &str) -> Result<Option<DateTime<Utc>>, &'static str> {
    let days = raw.trim();
    if days.is_empty() {
        return Ok(None);
    }
    days.parse()
        .ok()
        .filter(|days| (1..=3650).contains(days))
        .map(|days| Some(Utc::now() + Duration::days(days)))
        .ok_or("Expiry must be 1–3650 days.")
}

/// Record a change to an environment key on its organization. Secrets never go in `metadata`.
async fn record_key_note(
    db: &DatabasePool,
    session: &Session,
    env: &Env,
    note_type: &str,
    message: &str,
    metadata: serde_json::Value,
) -> bool {
    let scoped = db.with_scope(DatabaseScope::Organization { org_id: env.org_id });
    let user_id = session.current_user_id();
    match OrgNote::create(
        &scoped,
        &env.org_id,
        "security",
        note_type,
        message,
        Some(&metadata),
        Some(&user_id),
    )
    .await
    {
        Ok(_) => true,
        Err(error) => {
            tracing::error!(%error, org_id = %env.org_id, note_type, "Failed to record API key note");
            false
        }
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
    if raw.len() > MAX_PERMISSIONS_JSON_BYTES {
        return Err(PERMISSIONS_TOO_LARGE.to_string());
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
    let Some(env) = session.current_env.as_ref() else {
        return keys_list_redirect(session).into_response();
    };
    let row = match key_row(db, session, env, api_key).await {
        Ok(row) => row,
        Err(page) => return page,
    };
    let (mcp_tool_options, webhook_options) =
        super::build_permission_path_options(db, &row.key.env_id).await;

    // Build breadcrumbs: <org> (<env>) / API Keys / <description> / Edit. There is no key
    // detail page, so the description crumb is not a link.
    let mut breadcrumbs = templates::build_base_breadcrumbs_with_env(session);
    breadcrumbs.push(templates::BreadcrumbItem::clickable(
        "API Keys".to_string(),
        "/keys".to_string(),
    ));
    breadcrumbs.push(templates::BreadcrumbItem::current(
        row.key.description.clone(),
    ));
    breadcrumbs.push(templates::BreadcrumbItem::current("Edit".to_string()));

    let template = templates::ApiKeysEdit {
        title: &format!("Edit API Key: {}", row.key.description),
        page_context: templates::PrivatePageContext::with_breadcrumbs("keys", session, breadcrumbs),
        row,
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
            &form.expires_in_days,
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
    let expires_at = match expiry_from_days(&form.expires_in_days) {
        Ok(expires_at) => expires_at,
        Err(message) => return Err(retry(message.to_string()).await),
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
    match ApiKey::insert_expiring_api_key(
        &db,
        &form.api_key_id,
        &current_env.env_id,
        &form.description,
        &key_data,
        &session.current_user_id(),
        &permissions_json,
        expires_at,
    )
    .await
    {
        Ok(_) => {
            // The key exists and the user holds its secret, so a missing note
            // is logged rather than reported as a failed create.
            record_key_note(
                &db,
                &session,
                current_env,
                "api-key.create",
                "API key created",
                serde_json::json!({
                    "api_key_id": form.api_key_id,
                    "env_id": current_env.env_id,
                    "permissions": permissions_json,
                    "expires_at": expires_at,
                }),
            )
            .await;
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

    // A revoked key is read-only. The page offers no form for one, so this is
    // a stale tab or a hand-made request.
    let revoked = match ApiKey::get_credential_state(&db, &api_key_id).await {
        Ok(state) => state.revoked_at.is_some(),
        Err(_) => return Err(keys_load_failed(&session)),
    };

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

    if revoked {
        return Err(
            retry("This API key is revoked and can no longer be changed.".to_string()).await,
        );
    }

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

    if form.description != api_key.description
        || form.active != api_key.active
        || permissions_json != api_key.permissions
    {
        // The change is saved, so a missing note is logged rather than
        // reported as a failed update.
        record_key_note(
            &db,
            &session,
            current_env,
            "api-key.update",
            "API key updated",
            serde_json::json!({
                "api_key_id": api_key_id,
                "env_id": current_env.env_id,
                "active": form.active,
                "permissions": permissions_json,
            }),
        )
        .await;
    }

    // Redirect to the keys list
    Ok(keys_list_redirect(&session))
}

/// A key of the workspace's environment, for an administrator to change.
async fn key_for_admin(
    db: &DatabasePool,
    session: &Session,
    api_key_id: &Uuid,
) -> Result<(ApiKey, Env), Response> {
    if !session.is_current_org_admin {
        return Err(Html("You must be an admin to change API keys".to_string()).into_response());
    }
    let Some(env) = session.current_env.clone() else {
        return Err(keys_list_redirect(session).into_response());
    };
    match ApiKey::get_api_key(db, api_key_id).await {
        Ok(key) if key.env_id == env.env_id => Ok((key, env)),
        _ => Err(keys_list_redirect(session).into_response()),
    }
}

/// The edit page for a stored key, explaining why an action was refused.
async fn key_action_refused(
    db: &DatabasePool,
    session: &Session,
    key: ApiKey,
    message: String,
) -> Response {
    let access_level = if key.has_full_permissions() {
        "full"
    } else {
        "restricted"
    };
    let permissions = key.permissions.clone();
    let description = key.description.clone();
    render_keys_edit(
        db,
        session,
        key,
        message,
        &description,
        access_level,
        permissions,
    )
    .await
}

/// What the new-secret page says about the secret it replaced.
fn rotated_note(
    session: &Session,
    rotated: &RotatedSecret,
    overlap: Duration,
    tail: &str,
) -> String {
    let previous = if overlap.is_zero() {
        "The previous secret has stopped working.".to_string()
    } else {
        let until = super::sub_orgs::display_time(session, &rotated.previous_secret_expires_at);
        format!("The previous secret works until {until}.")
    };
    format!("It will not be shown again. {previous}{tail}")
}

fn overlap_running_message(session: &Session, until: &DateTime<Utc>) -> String {
    let until = super::sub_orgs::display_time(session, until);
    format!(
        "This key's previous secret works until {until}. Rotate after that, or choose no overlap, which stops it now."
    )
}

#[derive(Deserialize)]
pub struct RotateKeyForm {
    overlap_seconds: i64,
}

/// Issue a new secret for an environment key. The page shows it once, as it
/// does for a new key.
pub async fn keys_rotate_handler(
    Path(api_key_id): Path<Uuid>,
    State(db): State<Arc<DatabasePool>>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
    Form(form): Form<RotateKeyForm>,
) -> Result<Response, Response> {
    let (key, env) = key_for_admin(&db, &session, &api_key_id).await?;
    if !(0..=MAX_ROTATION_OVERLAP_SECONDS).contains(&form.overlap_seconds) {
        let message = "Overlap must be between 0 seconds and 7 days.".to_string();
        return Err(key_action_refused(&db, &session, key, message).await);
    }
    let overlap = Duration::seconds(form.overlap_seconds);
    let row = key_row(&db, &session, &env, key.clone()).await?;
    if !row.rotatable {
        let message = "Only a working API key can be rotated.".to_string();
        return Err(key_action_refused(&db, &session, key, message).await);
    }
    // Checked here too, so a refused rotation leaves no note.
    let state = ApiKey::get_credential_state(&db, &api_key_id)
        .await
        .map_err(|_| keys_load_failed(&session))?;
    if let Some(until) = state
        .previous_secret_expires_at()
        .filter(|_| !overlap.is_zero())
    {
        let message = overlap_running_message(&session, &until);
        return Err(key_action_refused(&db, &session, key, message).await);
    }
    if !record_key_note(
        &db,
        &session,
        &env,
        "api-key.rotate",
        "API key rotated",
        serde_json::json!({
            "api_key_id": api_key_id,
            "env_id": env.env_id,
            "overlap_seconds": form.overlap_seconds,
        }),
    )
    .await
    {
        let message = "Unable to rotate the API key.".to_string();
        return Err(key_action_refused(&db, &session, key, message).await);
    }
    match ApiKey::rotate(&db, &api_key_id, overlap, &session.current_user_id()).await {
        Ok(rotated) => {
            let mut breadcrumbs = templates::build_base_breadcrumbs_with_env(&session);
            breadcrumbs.push(templates::BreadcrumbItem::clickable(
                "API Keys".to_string(),
                "/keys".to_string(),
            ));
            breadcrumbs.push(templates::BreadcrumbItem::clickable(
                key.description.clone(),
                format!("/keys/{api_key_id}/edit"),
            ));
            breadcrumbs.push(templates::BreadcrumbItem::current("New secret".to_string()));
            Ok(rotated_secret_page(
                templates::PrivatePageContext::with_breadcrumbs("keys", &session, breadcrumbs),
                "Save this API key's new secret now",
                &rotated_note(&session, &rotated, overlap, ""),
                rotated.token,
                format!("/keys?org_id={}&env_id={}", env.org_id, env.env_id),
                "Back to API Keys",
            ))
        }
        Err(ApiKeyError::OverlapRunning(until)) => {
            let message = overlap_running_message(&session, &until);
            Err(key_action_refused(&db, &session, key, message).await)
        }
        Err(error) => {
            tracing::error!(%error, %api_key_id, "API key rotation failed");
            let message = "Unable to rotate the API key.".to_string();
            Err(key_action_refused(&db, &session, key, message).await)
        }
    }
}

/// Revoke an environment key for good.
pub async fn keys_revoke_handler(
    Path(api_key_id): Path<Uuid>,
    State(db): State<Arc<DatabasePool>>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> Result<Redirect, Response> {
    let (key, env) = key_for_admin(&db, &session, &api_key_id).await?;
    let state = ApiKey::get_credential_state(&db, &api_key_id)
        .await
        .map_err(|_| keys_load_failed(&session))?;
    if state.revoked_at.is_some() {
        return Ok(keys_list_redirect(&session));
    }
    if !record_key_note(
        &db,
        &session,
        &env,
        "api-key.revoke",
        "API key revoked",
        serde_json::json!({"api_key_id": api_key_id, "env_id": env.env_id}),
    )
    .await
    {
        let message = "Unable to revoke the API key.".to_string();
        return Err(key_action_refused(&db, &session, key, message).await);
    }
    if let Err(error) = ApiKey::revoke(&db, &api_key_id, &session.current_user_id()).await {
        tracing::error!(%error, %api_key_id, "API key revocation failed");
        let message = "Unable to revoke the API key.".to_string();
        return Err(key_action_refused(&db, &session, key, message).await);
    }
    Ok(keys_list_redirect(&session))
}

/// The new-org-key page shows a secret and the create form commits its hash,
/// as the environment key page does. The token binds the hash to the user and
/// the organization that saw the secret.
#[derive(Serialize, Deserialize)]
struct OrgKeyHandoff {
    org_api_key_id: Uuid,
    user_id: Uuid,
    org_id: Uuid,
    key_hash: String,
    exp: usize,
}

/// A key of its own, so an environment key handoff never redeems as an org
/// key handoff or the other way round.
fn org_key_handoff_secret() -> Result<Vec<u8>, String> {
    let secret = crate::auth::get_session_secret()?;
    Ok(format!("{secret}:org-api-key-handoff").into_bytes())
}

fn sign_org_key_handoff(
    org_api_key_id: Uuid,
    user_id: Uuid,
    org_id: Uuid,
    key_hash: String,
) -> Result<String, String> {
    let claims = OrgKeyHandoff {
        org_api_key_id,
        user_id,
        org_id,
        key_hash,
        exp: (Utc::now().timestamp() + API_KEY_HANDOFF_TTL_SECS) as usize,
    };
    let key = EncodingKey::from_secret(&org_key_handoff_secret()?)
        .map_err(|e| format!("Failed to sign org key handoff: {e}"))?;
    encode(&Header::new(Algorithm::HS256), &claims, &key)
        .map_err(|e| format!("Failed to sign org key handoff: {e}"))
}

fn verify_org_key_handoff(
    token: &str,
    org_api_key_id: &Uuid,
    user_id: Uuid,
    org_id: Uuid,
) -> Option<String> {
    let secret = org_key_handoff_secret().ok()?;
    let mut validation = Validation::new(Algorithm::HS256);
    validation.leeway = 0;
    let handoff =
        decode::<OrgKeyHandoff>(token, &DecodingKey::from_secret(&secret).ok()?, &validation)
            .ok()?
            .claims;
    if (handoff.org_api_key_id, handoff.user_id, handoff.org_id)
        != (*org_api_key_id, user_id, org_id)
    {
        return None;
    }
    Some(handoff.key_hash)
}

/// The Org keys tab, bound to the organization that handled the request.
fn org_keys_list_url(root: &Org) -> String {
    format!("/keys/org?org_id={}", root.org_id)
}

/// The parent organization whose org keys the user manages, or the page that
/// says they can't: org keys belong to a parent organization's administrators.
async fn org_keys_root_or_refuse(db: &DatabasePool, session: &Session) -> Result<Org, Response> {
    org_keys_root(db, session).await.ok_or_else(|| {
        super::error_page(
            session,
            StatusCode::FORBIDDEN,
            "Org API keys are managed by administrators of a parent organization",
        )
    })
}

/// What an org key's grants cover, as short labels for the key list.
fn org_key_access_labels(permissions: &serde_json::Value) -> Vec<&'static str> {
    const ORDER: [&str; 6] = [
        "Organizations",
        "Environments",
        "Environment keys",
        "Org keys",
        "Usage",
        "Key limits",
    ];
    let mut found = [false; 6];
    for urn in permissions
        .as_object()
        .into_iter()
        .flat_map(|map| map.keys())
    {
        let Some(rest) = urn.strip_prefix("org:") else {
            continue;
        };
        let label = match rest.split_once('/') {
            None => 0,
            Some((_, rest)) => match rest.strip_prefix("env:") {
                Some(env) => match env.split_once('/') {
                    None => 1,
                    Some((_, inner)) if inner.starts_with("api-key:") => 2,
                    Some(_) => 5,
                },
                None if rest.starts_with("api-key:") => 3,
                None if rest.starts_with("usage:") => 4,
                None => continue,
            },
        };
        found[label] = true;
    }
    ORDER
        .into_iter()
        .zip(found)
        .filter_map(|(label, found)| found.then_some(label))
        .collect()
}

async fn org_key_row(
    db: &DatabasePool,
    session: &Session,
    key: OrgApiKey,
) -> Result<(templates::OrgApiKeyRow, KeyStatus), Response> {
    let status = key
        .status(db)
        .await
        .map_err(|_| keys_load_failed(session))?;
    let (status_label, status_badge) = status_view(status);
    let row = templates::OrgApiKeyRow {
        status_label,
        status_badge,
        expires_at: key
            .expires_at
            .map(|at| super::sub_orgs::display_time(session, &at)),
        previous_secret_expires_at: key
            .previous_secret_expires_at()
            .filter(|_| status.is_active())
            .map(|at| super::sub_orgs::display_time(session, &at)),
        revocable: status.is_active(),
        access: org_key_access_labels(&key.permissions),
        key,
    };
    Ok((row, status))
}

/// Breadcrumbs for an org key page: API Keys / Org keys, then `tail`.
fn org_key_breadcrumbs(tail: Vec<templates::BreadcrumbItem>) -> templates::Breadcrumbs {
    let mut breadcrumbs = vec![
        templates::BreadcrumbItem::clickable("API Keys".to_string(), "/keys".to_string()),
        templates::BreadcrumbItem::clickable("Org keys".to_string(), "/keys/org".to_string()),
    ];
    breadcrumbs.extend(tail);
    breadcrumbs
}

/// The Org keys tab of the API Keys page.
pub async fn org_keys_list_handler(
    State(db): State<Arc<DatabasePool>>,
    Query(params): Query<AHashMap<String, String>>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> Result<Response, Response> {
    // Someone without org keys followed a link here: their keys are the environment's.
    let Some(root) = org_keys_root(&db, &session).await else {
        return Ok(keys_list_redirect(&session).into_response());
    };
    let keys = match OrgApiKey::list_for_user(&db, root.org_id, session.current_user_id()).await {
        Ok(keys) => keys,
        Err(OrgApiKeyError::Unauthorized) => {
            return Ok(keys_list_redirect(&session).into_response());
        }
        Err(_) => return Err(keys_load_failed(&session)),
    };
    let show_ended = params.get("show").is_some_and(|show| show == "all");
    let mut rows = Vec::with_capacity(keys.len());
    let mut ended_count = 0;
    for key in keys {
        let (row, status) = org_key_row(&db, &session, key).await?;
        // Revoked, expired and issuer-revoked keys never work again.
        let ended = matches!(
            status,
            KeyStatus::Revoked | KeyStatus::Expired | KeyStatus::IssuerInactive
        );
        ended_count += usize::from(ended);
        if show_ended || !ended {
            rows.push(row);
        }
    }
    const KEYS_PER_PAGE: i64 = 25;
    let page = list_query::PageParams::parse(&params, KEYS_PER_PAGE);
    let total_keys = rows.len() as i64;
    let pagination = list_query::PaginationWindow::new(total_keys, &page);
    let rows: Vec<_> = rows
        .into_iter()
        .skip(page.offset as usize)
        .take(KEYS_PER_PAGE as usize)
        .collect();
    let breadcrumbs = vec![
        templates::BreadcrumbItem::clickable("API Keys".to_string(), "/keys".to_string()),
        templates::BreadcrumbItem::current("Org keys".to_string()),
    ];
    let template = templates::OrgKeysList {
        title: "Org API Keys",
        page_context: templates::PrivatePageContext::for_org_page("keys", &session, breadcrumbs),
        keys_tab: "org",
        org_keys_root: Some(root),
        rows,
        error_message: None,
        show_ended,
        ended_count,
        current_page_num: page.current_page_num,
        start_page: pagination.start_page,
        end_page: pagination.end_page,
        has_next_page: pagination.has_next_page,
        has_prev_page: pagination.has_prev_page,
        total_keys,
    };
    Ok(Html(template.render().unwrap()).into_response())
}

/// The grants a new org key form starts from: enough to provision
/// sub-organizations and hand each an environment key that sends events and
/// reads runs.
fn org_key_example_permissions() -> serde_json::Value {
    serde_json::json!({
        "org:*": ["create", "read", "update"],
        "org:*/env:*": ["create", "read"],
        "org:*/env:*/api-key:*": ["create", "read", "update", "delete"],
        "org:*/env:*/event:*": ["create"],
        "org:*/env:*/run:*": ["read"],
    })
}

/// Render the new-org-key page with a freshly generated key and handoff
/// token. An error re-render keeps what was submitted but shows a new key, as
/// the environment key page does.
fn render_org_key_new(
    session: &Session,
    root: Org,
    error_message: &str,
    description: &str,
    expires_in_days: &str,
    permissions: serde_json::Value,
) -> Response {
    let failed = || {
        super::error_page(
            session,
            StatusCode::INTERNAL_SERVER_ERROR,
            "Unable to generate an org API key",
        )
    };
    let org_api_key_id = Uuid::now_v7();
    let Ok((generated_key, key_hash)) = ApiKey::generate_api_key(&org_api_key_id) else {
        return failed();
    };
    let Ok(handoff_token) = sign_org_key_handoff(
        org_api_key_id,
        session.current_user_id(),
        root.org_id,
        key_hash,
    ) else {
        return failed();
    };
    let breadcrumbs =
        org_key_breadcrumbs(vec![templates::BreadcrumbItem::current("New".to_string())]);
    let status = if error_message.is_empty() {
        StatusCode::OK
    } else {
        StatusCode::BAD_REQUEST
    };
    let template = templates::OrgKeysNew {
        title: "New Org API Key",
        page_context: templates::PrivatePageContext::for_org_page("keys", session, breadcrumbs),
        root_org_id: root.org_id.to_string(),
        root,
        generated_key,
        org_api_key_id,
        handoff_token,
        error_message,
        description,
        expires_in_days,
        permissions,
    };
    secret_page(status, template.render().unwrap())
}

pub async fn org_keys_new_handler(
    State(db): State<Arc<DatabasePool>>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> Result<Response, Response> {
    let root = org_keys_root_or_refuse(&db, &session).await?;
    Ok(render_org_key_new(
        &session,
        root,
        "",
        "",
        "",
        org_key_example_permissions(),
    ))
}

#[derive(Deserialize)]
pub struct OrgApiKeyForm {
    org_api_key_id: Uuid,
    #[serde(default)]
    handoff_token: String,
    description: String,
    #[serde(default)]
    permissions: String,
    #[serde(default)]
    expires_in_days: String,
}

pub async fn org_keys_create_handler(
    State(db): State<Arc<DatabasePool>>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
    Form(form): Form<OrgApiKeyForm>,
) -> Result<Redirect, Response> {
    let root = org_keys_root_or_refuse(&db, &session).await?;
    let submitted = submitted_permissions(&form.permissions);
    let retry = |message: &str| {
        render_org_key_new(
            &session,
            root.clone(),
            message,
            &form.description,
            &form.expires_in_days,
            submitted.clone(),
        )
    };
    if form.description.trim().is_empty() {
        return Err(retry("Description is required"));
    }
    let expires_at = match expiry_from_days(&form.expires_in_days) {
        Ok(expires_at) => expires_at,
        Err(message) => return Err(retry(message)),
    };
    // Checked here as well as where it is parsed: an oversized set must be
    // refused, not created as a key with no permissions.
    if form.permissions.len() > MAX_PERMISSIONS_JSON_BYTES {
        return Err(retry(PERMISSIONS_TOO_LARGE));
    }
    if !submitted.is_object() {
        return Err(retry("Permissions must be a JSON object."));
    }
    let user_id = session.current_user_id();
    let Some(key_hash) = verify_org_key_handoff(
        &form.handoff_token,
        &form.org_api_key_id,
        user_id,
        root.org_id,
    ) else {
        return Err(retry(
            "This form expired before it was submitted. A new key was generated below: copy it, then create the key again.",
        ));
    };
    let Ok(key_data) = serde_json::from_str(&key_hash) else {
        return Err(retry("Failed to process the org API key"));
    };
    let request = CreateOrgApiKey {
        description: form.description.clone(),
        permissions: submitted.clone(),
        expires_at,
    };
    let prepared = PreparedOrgKey {
        org_api_key_id: form.org_api_key_id,
        key_data,
    };
    match OrgApiKey::create_prepared_for_user(&db, root.org_id, user_id, &request, prepared).await {
        Ok(_) => Ok(Redirect::to(&org_keys_list_url(&root))),
        Err(error) => {
            // The same form submitted twice: the key ID is the primary key, so
            // the first submission made the key and this one changed nothing.
            let scoped = db.with_scope(DatabaseScope::Organization {
                org_id: root.org_id,
            });
            if OrgApiKey::get(&scoped, form.org_api_key_id)
                .await
                .is_ok_and(|key| key.org_id == root.org_id)
            {
                return Ok(Redirect::to(&org_keys_list_url(&root)));
            }
            tracing::info!(root_org_id = %root.org_id, %error, "Org API key rejected");
            let message = match &error {
                OrgApiKeyError::Invalid(reason) => {
                    format!("Unable to create the org API key: {reason}.")
                }
                OrgApiKeyError::Unauthorized => {
                    "Unable to create the org API key. The account must be active, and you must administer it.".to_string()
                }
                _ => "Unable to create the org API key.".to_string(),
            };
            Err(retry(&message))
        }
    }
}

/// An org key of `root`, as its page shows it. The list is the App's
/// authority: it holds exactly the keys the user may manage.
async fn org_key_for_user(
    db: &DatabasePool,
    session: &Session,
    root: &Org,
    key_id: Uuid,
) -> Result<templates::OrgApiKeyRow, Response> {
    let keys = OrgApiKey::list_for_user(db, root.org_id, session.current_user_id())
        .await
        .map_err(|_| keys_load_failed(session))?;
    let Some(key) = keys.into_iter().find(|key| key.org_api_key_id == key_id) else {
        return Err(super::error_page(
            session,
            StatusCode::NOT_FOUND,
            "Org API key not found",
        ));
    };
    Ok(org_key_row(db, session, key).await?.0)
}

/// Render an org key's page; `error` explains a refused action, with status 400.
async fn render_org_key_detail(
    db: &DatabasePool,
    session: &Session,
    root: Org,
    key_id: Uuid,
    error: &str,
) -> Response {
    let row = match org_key_for_user(db, session, &root, key_id).await {
        Ok(row) => row,
        Err(page) => return page,
    };
    let breadcrumbs = org_key_breadcrumbs(vec![templates::BreadcrumbItem::current(
        row.key.description.clone(),
    )]);
    let status = if error.is_empty() {
        StatusCode::OK
    } else {
        StatusCode::BAD_REQUEST
    };
    let template = templates::OrgKeyDetail {
        title: &format!("Org API Key: {}", row.key.description),
        page_context: templates::PrivatePageContext::for_org_page("keys", session, breadcrumbs),
        root_org_id: root.org_id.to_string(),
        root,
        row,
        error_message: error,
    };
    (status, Html(template.render().unwrap())).into_response()
}

pub async fn org_key_detail_handler(
    Path(key_id): Path<Uuid>,
    State(db): State<Arc<DatabasePool>>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> Result<Response, Response> {
    let root = org_keys_root_or_refuse(&db, &session).await?;
    Ok(render_org_key_detail(&db, &session, root, key_id, "").await)
}

/// Issue a new secret for an org key. The page shows it once.
pub async fn org_key_rotate_handler(
    Path(key_id): Path<Uuid>,
    State(db): State<Arc<DatabasePool>>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
    Form(form): Form<RotateKeyForm>,
) -> Result<Response, Response> {
    let root = org_keys_root_or_refuse(&db, &session).await?;
    if !(0..=MAX_ROTATION_OVERLAP_SECONDS).contains(&form.overlap_seconds) {
        let message = "Overlap must be between 0 seconds and 7 days.";
        return Err(render_org_key_detail(&db, &session, root, key_id, message).await);
    }
    let overlap = Duration::seconds(form.overlap_seconds);
    let user_id = session.current_user_id();
    // Also answers 404 for a key that isn't this organization's.
    let description = org_key_for_user(&db, &session, &root, key_id)
        .await?
        .key
        .description;
    match OrgApiKey::rotate_for_user(&db, root.org_id, user_id, key_id, overlap).await {
        Ok(rotated) => {
            let breadcrumbs = org_key_breadcrumbs(vec![
                templates::BreadcrumbItem::clickable(
                    description,
                    format!("/keys/org/{key_id}/edit"),
                ),
                templates::BreadcrumbItem::current("New secret".to_string()),
            ]);
            Ok(rotated_secret_page(
                templates::PrivatePageContext::for_org_page("keys", &session, breadcrumbs),
                "Save this org key's new secret now",
                &rotated_note(
                    &session,
                    &rotated,
                    overlap,
                    " Keys this key created keep working.",
                ),
                rotated.token,
                format!("/keys/org/{key_id}/edit?org_id={}", root.org_id),
                "Back to the Org Key",
            ))
        }
        Err(OrgApiKeyError::OverlapRunning(until)) => {
            let message = overlap_running_message(&session, &until);
            Err(render_org_key_detail(&db, &session, root, key_id, &message).await)
        }
        Err(error) => {
            tracing::info!(root_org_id = %root.org_id, %error, "Org API key rotation rejected");
            let message = "Unable to rotate the org API key.";
            Err(render_org_key_detail(&db, &session, root, key_id, message).await)
        }
    }
}

pub async fn org_key_revoke_handler(
    Path(key_id): Path<Uuid>,
    State(db): State<Arc<DatabasePool>>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> Result<Redirect, Response> {
    let root = org_keys_root_or_refuse(&db, &session).await?;
    match OrgApiKey::revoke_for_user(&db, root.org_id, session.current_user_id(), key_id).await {
        Ok(()) => Ok(Redirect::to(&org_keys_list_url(&root))),
        Err(_) => {
            let message = "Unable to revoke the org API key.";
            Err(render_org_key_detail(&db, &session, root, key_id, message).await)
        }
    }
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
            &EncodingKey::from_secret(&api_key_handoff_secret().unwrap()).unwrap(),
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
            &EncodingKey::from_secret(session_key.as_bytes()).unwrap(),
        )
        .unwrap();
        assert!(verify_api_key_handoff(&session_signed, &key, user, org, env).is_none());

        let mut tampered = sign_api_key_handoff(key, user, org, env, "hash".into()).unwrap();
        tampered.pop();
        assert!(verify_api_key_handoff(&tampered, &key, user, org, env).is_none());
        assert!(verify_api_key_handoff("", &key, user, org, env).is_none());
    }
}

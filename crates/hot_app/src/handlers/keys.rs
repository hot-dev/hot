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
use hot::db::api_key::{ApiKey, ApiKeyError};
use hot::db::env::Env;
use hot::db::org::Org;
use hot::db::org_api_key::{
    CreateOrgApiKey, KeyStatus, MAX_ROTATION_OVERLAP_SECONDS, OrgApiKey, OrgApiKeyError,
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

/// What the org key form submitted, when it comes back rejected.
#[derive(Default)]
struct OrgKeyFormValues {
    description: String,
    permissions: Option<String>,
    expires_in_days: String,
}

async fn org_keys_section(
    db: &DatabasePool,
    session: &Session,
    root: Org,
    form: OrgKeyFormValues,
) -> Result<Option<templates::OrgKeysSection>, Response> {
    let keys = match OrgApiKey::list_for_user(db, root.org_id, session.current_user_id()).await {
        Ok(keys) => keys,
        Err(OrgApiKeyError::Unauthorized) => return Ok(None),
        Err(_) => return Err(keys_load_failed(session)),
    };
    let mut rows = Vec::with_capacity(keys.len());
    for key in keys {
        let status = key
            .status(db)
            .await
            .map_err(|_| keys_load_failed(session))?;
        let (status_label, status_badge) = status_view(status);
        rows.push(templates::OrgApiKeyRow {
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
            key,
        });
    }
    let form_open = form.permissions.is_some();
    let example = || {
        serde_json::to_string_pretty(&serde_json::json!({
            "org:*": ["create", "read", "update"],
            "org:*/env:*": ["create", "read"],
            "org:*/env:*/api-key:*": ["create", "read", "update", "delete"],
            "org:*/env:*/event:*": ["create"],
            "org:*/env:*/run:*": ["read"],
        }))
        .unwrap_or_default()
    };
    Ok(Some(templates::OrgKeysSection {
        root,
        rows,
        permissions: form.permissions.unwrap_or_else(example),
        description: form.description,
        expires_in_days: form.expires_in_days,
        form_open,
    }))
}

/// What a form handler sends back to the API Keys page.
#[derive(Default)]
struct KeysPageState {
    issued_secret: Option<templates::IssuedSecret>,
    error: Option<String>,
    org_form: OrgKeyFormValues,
}

impl KeysPageState {
    fn error(message: impl Into<String>) -> Self {
        Self {
            error: Some(message.into()),
            ..Self::default()
        }
    }

    fn issued(heading: &'static str, note: &'static str, token: String) -> Self {
        Self {
            issued_secret: Some(templates::IssuedSecret {
                heading,
                note,
                token,
            }),
            ..Self::default()
        }
    }
}

/// Render the API Keys page: the workspace environment's keys and, for
/// `root`, its org API keys.
async fn render_keys_page(
    db: &DatabasePool,
    session: &Session,
    params: &AHashMap<String, String>,
    root: Option<Org>,
    state: KeysPageState,
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
        let keys = ApiKey::get_api_keys_by_env(db, &env.env_id, Some(KEYS_PER_PAGE), Some(offset))
            .await
            .unwrap_or_default();
        for key in keys {
            match key_row(db, session, env, key).await {
                Ok(row) => api_keys.push(row),
                Err(page) => return page,
            }
        }
        total_keys = ApiKey::count_api_keys_by_env(db, &env.env_id)
            .await
            .unwrap_or_default();
    }

    let org_keys = match root {
        Some(root) => match org_keys_section(db, session, root, state.org_form).await {
            Ok(section) => section,
            Err(page) => return page,
        },
        None => None,
    };

    // Calculate pagination info
    let pagination = list_query::PaginationWindow::new(total_keys, &page);
    let total_pages = pagination.total_pages;
    let has_next_page = pagination.has_next_page;
    let has_prev_page = pagination.has_prev_page;
    let start_page = pagination.start_page;
    let end_page = pagination.end_page;

    let status = if state.error.is_some() {
        StatusCode::BAD_REQUEST
    } else {
        StatusCode::OK
    };
    let template = templates::ApiKeysList {
        title: "API Keys",
        page_context: templates::PrivatePageContext::with_breadcrumbs("keys", session, breadcrumbs),
        api_keys,
        org_keys,
        issued_secret: state.issued_secret,
        error_message: state.error,
        is_admin: session.is_current_org_admin,
        current_page_num,
        total_pages,
        start_page,
        end_page,
        has_next_page,
        has_prev_page,
        total_keys,
    };

    // The page can show a key secret: keep it out of caches and Referer headers.
    (
        status,
        [
            ("Cache-Control", "no-store"),
            ("Referrer-Policy", "no-referrer"),
        ],
        Html(template.render().unwrap()),
    )
        .into_response()
}

pub async fn keys_list_handler(
    State(db): State<Arc<DatabasePool>>,
    Query(params): Query<AHashMap<String, String>>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> impl IntoResponse {
    let root = org_keys_root(&db, &session).await;
    render_keys_page(&db, &session, &params, root, KeysPageState::default()).await
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

/// The submitted permissions, for re-rendering a form after an error. Text that isn't JSON is
/// passed through as a string, which the page's permission builder ignores.
fn submitted_permissions(raw: &str) -> serde_json::Value {
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
            let root = org_keys_root(&db, &session).await;
            let state = KeysPageState::issued(
                "Save this API key's new secret now",
                "It will not be shown again.",
                rotated.token,
            );
            Ok(render_keys_page(&db, &session, &AHashMap::new(), root, state).await)
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

#[derive(Deserialize)]
pub struct OrgApiKeyForm {
    description: String,
    permissions: String,
    #[serde(default)]
    expires_in_days: String,
}

pub async fn create_org_api_key(
    Path(slug): Path<String>,
    State(db): State<Arc<DatabasePool>>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
    Form(form): Form<OrgApiKeyForm>,
) -> Result<Response, Response> {
    let root = super::sub_orgs::root_for_admin(&db, &session, &slug).await?;
    let rejected = |error: &str| KeysPageState {
        error: Some(error.to_string()),
        org_form: OrgKeyFormValues {
            description: form.description.clone(),
            permissions: Some(form.permissions.clone()),
            expires_in_days: form.expires_in_days.clone(),
        },
        ..KeysPageState::default()
    };
    let no_params = AHashMap::new();
    let expires_at = match expiry_from_days(&form.expires_in_days) {
        Ok(expires_at) => expires_at,
        Err(message) => {
            let state = rejected(message);
            return Ok(render_keys_page(&db, &session, &no_params, Some(root), state).await);
        }
    };
    let permissions = match serde_json::from_str(&form.permissions) {
        Ok(permissions) => permissions,
        Err(_) => {
            let state = rejected("Permissions must be a JSON object.");
            return Ok(render_keys_page(&db, &session, &no_params, Some(root), state).await);
        }
    };
    let request = CreateOrgApiKey {
        description: form.description.clone(),
        permissions,
        expires_at,
    };
    let created =
        OrgApiKey::create_for_user(&db, root.org_id, session.current_user_id(), &request).await;
    let state = match created {
        Ok((_, token)) => KeysPageState::issued(
            "Save this org API key now",
            "It will not be shown again. Revoking it also stops every key it created.",
            token,
        ),
        Err(error) => {
            tracing::info!(root_org_id = %root.org_id, %error, "Org API key rejected");
            rejected(
                "Unable to create the org API key. Permissions must use org: URNs such as org:* or org:{org}/env:*/project:*.",
            )
        }
    };
    Ok(render_keys_page(&db, &session, &no_params, Some(root), state).await)
}

/// Issue a new secret for an org key. The page shows it once, as it does for a
/// new key.
pub async fn rotate_org_api_key(
    Path((slug, key_id)): Path<(String, Uuid)>,
    State(db): State<Arc<DatabasePool>>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
    Form(form): Form<RotateKeyForm>,
) -> Result<Response, Response> {
    let root = super::sub_orgs::root_for_admin(&db, &session, &slug).await?;
    let no_params = AHashMap::new();
    if !(0..=MAX_ROTATION_OVERLAP_SECONDS).contains(&form.overlap_seconds) {
        let state = KeysPageState::error("Overlap must be between 0 seconds and 7 days.");
        return Ok(render_keys_page(&db, &session, &no_params, Some(root), state).await);
    }
    let overlap = Duration::seconds(form.overlap_seconds);
    let user_id = session.current_user_id();
    let state = match OrgApiKey::rotate_for_user(&db, root.org_id, user_id, key_id, overlap).await {
        Ok(rotated) => KeysPageState::issued(
            "Save this org API key's new secret now",
            "It will not be shown again. Keys this key created keep working.",
            rotated.token,
        ),
        Err(OrgApiKeyError::OverlapRunning(until)) => {
            KeysPageState::error(overlap_running_message(&session, &until))
        }
        Err(error) => {
            tracing::info!(root_org_id = %root.org_id, %error, "Org API key rotation rejected");
            KeysPageState::error("Unable to rotate the org API key.")
        }
    };
    Ok(render_keys_page(&db, &session, &no_params, Some(root), state).await)
}

pub async fn revoke_org_api_key(
    Path((slug, key_id)): Path<(String, Uuid)>,
    State(db): State<Arc<DatabasePool>>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> Result<Redirect, Response> {
    let root = super::sub_orgs::root_for_admin(&db, &session, &slug).await?;
    match OrgApiKey::revoke_for_user(&db, root.org_id, session.current_user_id(), key_id).await {
        Ok(()) => Ok(Redirect::to(&format!("/keys?org_id={}", root.org_id))),
        Err(_) => {
            let state = KeysPageState::error("Unable to revoke the org API key.");
            Err(render_keys_page(&db, &session, &AHashMap::new(), Some(root), state).await)
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

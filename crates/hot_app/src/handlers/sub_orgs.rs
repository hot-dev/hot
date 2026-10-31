// Handlers answer errors with a rendered page: an axum Response, which is large by design.
#![allow(clippy::result_large_err)]

use crate::auth::Session;
use crate::templates::{self, PrivatePageContext, filters};
use askama::Template;
use axum::extract::{Form, Path, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Redirect, Response};
use chrono::{DateTime, Datelike, TimeZone, Utc};
use hot::db::org::{CreateChild, Org, OrgLifecycle, OrgUser};
use hot::db::org_api_key::{CreateOrgApiKey, KeyStatus, OrgApiKey};
use hot::db::{BootstrapOperation, DatabasePool, DatabaseScope};
use hot::val::Val;
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Arc;
use uuid::Uuid;

type PageError = (StatusCode, &'static str);

/// Render a page error inside the app layout.
fn page_error(session: &Session, (status, message): PageError) -> Response {
    super::error_page(session, status, message)
}

async fn root_for_admin(db: &DatabasePool, session: &Session, slug: &str) -> Result<Org, Response> {
    let root = Org::get_org_by_slug(db, slug)
        .await
        .map_err(|_| page_error(session, (StatusCode::NOT_FOUND, "Organization not found")))?;
    if root.is_individual() {
        return Err(page_error(
            session,
            (
                StatusCode::FORBIDDEN,
                "Personal accounts can't have child organizations. Create an organization to manage children.",
            ),
        ));
    }
    if root.is_child() || !session.can_manage_account(db, &root.org_id).await {
        return Err(page_error(
            session,
            (
                StatusCode::FORBIDDEN,
                "Root administrator access is required",
            ),
        ));
    }
    Ok(root)
}

/// What a failed form submitted, so the page comes back with an explanation and the values.
#[derive(Default)]
struct FormState {
    error: Option<String>,
    child_name: String,
    child_slug: String,
    child_external_id: String,
    key_description: String,
    key_permissions: Option<String>,
    key_expires_in_days: String,
}

impl FormState {
    fn error(message: impl Into<String>) -> Self {
        Self {
            error: Some(message.into()),
            ..Self::default()
        }
    }
}

/// A user-facing reason for a rejected org operation. Database details stay in the log.
fn org_error_message(error: &hot::db::org::OrgError, fallback: &str) -> String {
    use hot::db::org::OrgError;
    match error {
        OrgError::Invalid(reason) => {
            let mut chars = reason.chars();
            match chars.next() {
                Some(first) => format!("{}{}.", first.to_uppercase(), chars.as_str()),
                None => fallback.to_string(),
            }
        }
        OrgError::LimitReached => {
            "This account has reached its organization limit for its plan.".to_string()
        }
        OrgError::Forbidden => "You don't have permission to make that change.".to_string(),
        OrgError::NotFound => "That organization no longer exists.".to_string(),
        OrgError::SlugTaken => "That handle is already in use.".to_string(),
        OrgError::ExternalIdTaken => {
            "A child organization already uses that customer reference.".to_string()
        }
        OrgError::Database(error)
            if error
                .as_database_error()
                .is_some_and(|e| e.is_unique_violation()) =>
        {
            "That handle or customer reference is already in use.".to_string()
        }
        OrgError::Database(_) => fallback.to_string(),
    }
}

#[derive(Template)]
#[template(path = "sub_orgs.html")]
struct SubOrgsPage {
    title: &'static str,
    page_context: PrivatePageContext,
    root: Org,
    children: Vec<Org>,
    overview: Vec<OrgOverview>,
    members: Vec<hot::db::org::OrgUserWithRole>,
    idempotency_key: Uuid,
    org_api_keys: Vec<OrgApiKeyRow>,
    org_key_example: String,
    issued_token: Option<String>,
    form: FormState,
}

struct OrgApiKeyRow {
    key: OrgApiKey,
    expires_at: Option<String>,
    /// Whether the key still authenticates, following its issuer chain: a
    /// key whose issuing key was revoked is dead though its own row is not.
    revocable: bool,
    status_label: Option<&'static str>,
}

fn status_label(status: KeyStatus) -> Option<&'static str> {
    match status {
        KeyStatus::Active => None,
        KeyStatus::Revoked => Some("Revoked"),
        KeyStatus::Expired => Some("Expired"),
        KeyStatus::IssuerInactive => Some("Issuer inactive"),
        KeyStatus::Suspended => Some("Suspended"),
    }
}

struct OrgOverview {
    org_id: Uuid,
    name: String,
    slug: String,
    lifecycle: String,
    lifecycle_badge: &'static str,
    root: bool,
    envs: Vec<OverviewEnv>,
    running: i64,
    failed_24h: i64,
    month_runs: i64,
    last_run: String,
    file_bytes: String,
}

/// The status badge recipe for a lifecycle state.
fn lifecycle_badge(state: &str, held: bool) -> &'static str {
    match state {
        _ if held => "badge-pending",
        "active" => "badge-success",
        "suspended" => "badge-pending",
        _ => "badge-neutral",
    }
}
struct OverviewEnv {
    env_id: Uuid,
    name: String,
    active: bool,
}
#[derive(sqlx::FromRow)]
struct RunActivity {
    env_id: Uuid,
    running: i64,
    failed_24h: i64,
    month_runs: i64,
    last_run: Option<DateTime<Utc>>,
}

type OverviewRows = (Vec<RunActivity>, Vec<(Uuid, i64)>);

/// The administrator gate runs before this function. Construct authority from
/// discovered ownership, never a client-supplied list of org/environment IDs.
async fn account_overview(
    db: &DatabasePool,
    session: &Session,
    root: &Org,
    children: &[Org],
) -> Result<Vec<OrgOverview>, PageError> {
    let metadata = db.with_scope(DatabaseScope::Bootstrap(BootstrapOperation::Membership));
    let mut orgs = vec![root.clone()];
    orgs.extend_from_slice(children);
    let mut envs = HashMap::new();
    let mut env_ids = Vec::new();
    for org in &orgs {
        let owned = hot::db::Env::get_envs_by_org(&metadata, &org.org_id)
            .await
            .map_err(|_| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Unable to resolve account environments",
                )
            })?;
        env_ids.extend(owned.iter().map(|env| env.env_id));
        envs.insert(org.org_id, owned);
    }
    let org_ids: Vec<Uuid> = orgs.iter().map(|org| org.org_id).collect();
    let read = db.with_scope(DatabaseScope::ReadSet {
        org_ids: org_ids.clone(),
        env_ids: env_ids.clone(),
    });
    let now = Utc::now();
    let since = now - chrono::Duration::hours(24);
    let month = Utc
        .with_ymd_and_hms(now.year(), now.month(), 1, 0, 0, 0)
        .single()
        .expect("valid calendar month");
    // A GROUP BY over the account's runs read every retained run. These
    // per-environment subqueries read only run index ranges instead: running
    // or failed runs, this month's runs, and the latest run.
    macro_rules! overview_queries {
        ($pool:expr,$kind:ty)=>{{
            let mut activity=sqlx::QueryBuilder::<$kind>::new("SELECT e.env_id, (SELECT COUNT(*) FROM run r WHERE r.env_id=e.env_id AND r.status_id=1) AS running, (SELECT COUNT(*) FROM run r WHERE r.env_id=e.env_id AND r.status_id=3 AND COALESCE(r.stop_time,r.start_time)>=");
            activity.push_bind(since).push(") AS failed_24h, (SELECT COUNT(*) FROM run r WHERE r.env_id=e.env_id AND r.start_time>=").push_bind(month).push(") AS month_runs, (SELECT MAX(r.start_time) FROM run r WHERE r.env_id=e.env_id) AS last_run FROM env e WHERE e.env_id IN (");
            if env_ids.is_empty(){activity.push("NULL");}else{let mut ids=activity.separated(",");for id in &env_ids{ids.push_bind(id);}}
            activity.push(")");
            let runs=activity.build_query_as::<RunActivity>().fetch_all($pool).await?;
            let mut storage=sqlx::QueryBuilder::<$kind>::new("SELECT org_id, CAST(COALESCE(SUM(size),0) AS BIGINT) FROM file WHERE active=true AND org_id IN (");
            {let mut ids=storage.separated(",");for id in &org_ids{ids.push_bind(id);}}
            storage.push(") AND (env_id IS NULL OR env_id IN (");
            if env_ids.is_empty(){storage.push("NULL");}else{let mut ids=storage.separated(",");for id in &env_ids{ids.push_bind(id);}}
            storage.push(")) GROUP BY org_id");
            let files=storage.build_query_as::<(Uuid,i64)>().fetch_all($pool).await?;
            Ok::<_,sqlx::Error>((runs,files))
        }};
    }
    let query_result: Result<OverviewRows, sqlx::Error> = async {
        match &read {
            DatabasePool::Postgres(pool) => overview_queries!(pool, sqlx::Postgres),
            DatabasePool::Sqlite(pool) => overview_queries!(pool, sqlx::Sqlite),
        }
    }
    .await;
    let (runs, files) = query_result.map_err(|error| {
        tracing::warn!(%error,"Account overview read failed");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Unable to load account activity",
        )
    })?;
    let activity: HashMap<Uuid, RunActivity> =
        runs.into_iter().map(|row| (row.env_id, row)).collect();
    let files: HashMap<Uuid, i64> = files.into_iter().collect();
    Ok(orgs
        .into_iter()
        .map(|org| {
            let owned = envs.remove(&org.org_id).unwrap_or_default();
            let rows: Vec<&RunActivity> = owned
                .iter()
                .filter_map(|env| activity.get(&env.env_id))
                .collect();
            let held = org.is_child() && org.is_admitting() && !root.is_admitting();
            let lifecycle = if held {
                format!("Held · account {}", root.lifecycle_state)
            } else {
                org.lifecycle_state.clone()
            };
            let lifecycle_badge = lifecycle_badge(&org.lifecycle_state, held);
            OrgOverview {
                org_id: org.org_id,
                root: org.org_id == root.org_id,
                name: org.name,
                slug: org.slug,
                lifecycle,
                lifecycle_badge,
                running: rows.iter().map(|row| row.running).sum(),
                failed_24h: rows.iter().map(|row| row.failed_24h).sum(),
                month_runs: rows.iter().map(|row| row.month_runs).sum(),
                last_run: rows
                    .iter()
                    .filter_map(|row| row.last_run)
                    .max()
                    .map(|time| display_time(session, &time))
                    .unwrap_or_else(|| "No runs".into()),
                file_bytes: templates::format_blob_size(
                    files.get(&org.org_id).copied().unwrap_or(0),
                ),
                envs: owned
                    .into_iter()
                    .map(|env| OverviewEnv {
                        env_id: env.env_id,
                        name: env.name,
                        active: env.active,
                    })
                    .collect(),
            }
        })
        .collect())
}

/// A timestamp in the user's display timezone, like the rest of the App.
fn display_time(session: &Session, time: &DateTime<Utc>) -> String {
    format!(
        "{} {}",
        crate::timezone::format_in_timezone(time, &session.display_timezone, "%Y-%m-%d %H:%M"),
        session.timezone_abbreviation
    )
}

pub async fn list(
    Path(slug): Path<String>,
    State(db): State<Arc<DatabasePool>>,
    axum::Extension(session): axum::Extension<Session>,
) -> Result<Response, Response> {
    let root = root_for_admin(&db, &session, &slug).await?;
    render_page(&db, &session, root, None, FormState::default()).await
}

/// Render the page; a [`FormState`] with an error renders with status 400.
async fn render_page(
    db: &DatabasePool,
    session: &Session,
    root: Org,
    issued_token: Option<String>,
    form: FormState,
) -> Result<Response, Response> {
    render_page_inner(db, session, root, issued_token, form)
        .await
        .map_err(|error| page_error(session, error))
}

async fn render_page_inner(
    db: &DatabasePool,
    session: &Session,
    root: Org,
    issued_token: Option<String>,
    form: FormState,
) -> Result<Response, PageError> {
    let load_failed = |_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Unable to load org API keys",
        )
    };
    let keys = OrgApiKey::list_for_user(db, root.org_id, session.current_user_id())
        .await
        .map_err(load_failed)?;
    let mut org_api_keys = Vec::with_capacity(keys.len());
    for key in keys {
        let status = key.status(db).await.map_err(load_failed)?;
        org_api_keys.push(OrgApiKeyRow {
            expires_at: key.expires_at.map(|at| display_time(session, &at)),
            revocable: status.is_active(),
            status_label: status_label(status),
            key,
        });
    }
    let org_key_example = serde_json::to_string_pretty(&serde_json::json!({
        "org:*": ["create", "read", "update"],
        "org:*/env:*": ["create", "read"],
        "org:*/env:*/api-key:*": ["create", "read", "update", "delete"],
        "org:*/env:*/event:*": ["create"],
        "org:*/env:*/run:*": ["read"],
    }))
    .unwrap_or_default();
    let children = Org::list_children(db, &root.org_id).await.map_err(|_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Unable to load child organizations",
        )
    })?;
    let overview = account_overview(db, session, &root, &children).await?;
    let members = OrgUser::get_users_with_roles_by_org(db, &root.org_id)
        .await
        .map_err(|_| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Unable to load account members",
            )
        })?
        .into_iter()
        .filter(|member| member.active)
        .collect();
    let breadcrumbs = vec![
        templates::BreadcrumbItem::clickable(root.name.clone(), format!("/@{}", root.slug)),
        templates::BreadcrumbItem::current("Child organizations".to_string()),
    ];
    let status = if form.error.is_some() {
        StatusCode::BAD_REQUEST
    } else {
        StatusCode::OK
    };
    let template = SubOrgsPage {
        title: "Child organizations",
        page_context: PrivatePageContext::for_org_page("children", session, breadcrumbs),
        root,
        children,
        overview,
        members,
        idempotency_key: Uuid::now_v7(),
        org_key_example: form.key_permissions.clone().unwrap_or(org_key_example),
        org_api_keys,
        issued_token,
        form,
    };
    let html = template.render().map_err(|_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Unable to render child organizations",
        )
    })?;
    Ok((
        status,
        [
            ("Cache-Control", "no-store"),
            ("Referrer-Policy", "no-referrer"),
        ],
        Html(html),
    )
        .into_response())
}

#[derive(Deserialize)]
pub struct ChildForm {
    name: String,
    slug: String,
    external_id: Option<String>,
    idempotency_key: String,
}

pub async fn create(
    Path(slug): Path<String>,
    State(db): State<Arc<DatabasePool>>,
    axum::Extension(conf): axum::Extension<Val>,
    axum::Extension(session): axum::Extension<Session>,
    Form(form): Form<ChildForm>,
) -> Result<Redirect, Response> {
    let root = root_for_admin(&db, &session, &slug).await?;
    let slug = form.slug.trim().to_string();
    let submitted = |error: String| FormState {
        error: Some(error),
        child_name: form.name.clone(),
        child_slug: form.slug.clone(),
        child_external_id: form.external_id.clone().unwrap_or_default(),
        ..FormState::default()
    };
    let reserved = crate::slug::extra_reserved_from_conf(&conf);
    if let Err(error) = crate::slug::validate_format_with_extra(&slug, &reserved) {
        let page = render_page(&db, &session, root, None, submitted(error.message().into()));
        return Err(page.await.unwrap_or_else(|page| page));
    }
    let child = CreateChild {
        name: form.name.clone(),
        slug,
        external_id: form
            .external_id
            .clone()
            .filter(|value| !value.trim().is_empty()),
        idempotency_key: form.idempotency_key.clone(),
        hosted: hot::product::billing_enabled(&conf),
    };
    if let Err(error) =
        Org::create_child(&db, &root.org_id, &session.current_user_id(), &child).await
    {
        tracing::warn!(root_org_id = %root.org_id, %error, "Child provisioning rejected");
        let message = org_error_message(&error, "Unable to create the child organization.");
        let page = render_page(&db, &session, root, None, submitted(message));
        return Err(page.await.unwrap_or_else(|page| page));
    }
    Ok(Redirect::to(&format!("/@{}/children", root.slug)))
}

/// Redirect back to the page, or re-render it with `error` explained above the forms.
async fn back_to_page(
    db: &DatabasePool,
    session: &Session,
    root: Org,
    error: Option<String>,
) -> Result<Redirect, Response> {
    match error {
        None => Ok(Redirect::to(&format!("/@{}/children", root.slug))),
        Some(error) => {
            let page = render_page(db, session, root, None, FormState::error(error));
            Err(page.await.unwrap_or_else(|page| page))
        }
    }
}

#[derive(Deserialize)]
pub struct LifecycleForm {
    state: OrgLifecycle,
}

pub async fn lifecycle(
    Path((slug, child_id)): Path<(String, Uuid)>,
    State(db): State<Arc<DatabasePool>>,
    axum::Extension(session): axum::Extension<Session>,
    Form(form): Form<LifecycleForm>,
) -> Result<Redirect, Response> {
    let root = root_for_admin(&db, &session, &slug).await?;
    let child = Org::get_org_uncached(&db, &child_id).await.ok();
    if child.is_none_or(|child| child.parent_org_id != Some(root.org_id)) {
        return Err(page_error(
            &session,
            (StatusCode::NOT_FOUND, "Child organization not found"),
        ));
    }
    let error = Org::set_lifecycle(&db, &child_id, &session.current_user_id(), form.state)
        .await
        .err()
        .map(|error| {
            org_error_message(
                &error,
                "That status change isn't allowed. Restore an archived child to suspended before activating it.",
            )
        });
    back_to_page(&db, &session, root, error).await
}

#[derive(Deserialize)]
pub struct GrantForm {
    user_id: Uuid,
    role_id: i16,
}

pub async fn grant(
    Path((slug, child_id)): Path<(String, Uuid)>,
    State(db): State<Arc<DatabasePool>>,
    axum::Extension(session): axum::Extension<Session>,
    Form(form): Form<GrantForm>,
) -> Result<Redirect, Response> {
    let root = root_for_admin(&db, &session, &slug).await?;
    let role = match form.role_id {
        0 => None,
        1 | 2 => Some(form.role_id),
        _ => {
            return back_to_page(&db, &session, root, Some("Choose a child role.".into())).await;
        }
    };
    let error = Org::set_child_user_grant(
        &db,
        &root.org_id,
        &child_id,
        &session.current_user_id(),
        &form.user_id,
        role,
    )
    .await
    .err()
    .map(|error| org_error_message(&error, "Unable to change child access."));
    back_to_page(&db, &session, root, error).await
}

#[derive(Deserialize)]
pub struct OrgApiKeyForm {
    description: String,
    permissions: String,
    expires_in_days: Option<String>,
}

pub async fn create_org_api_key(
    Path(slug): Path<String>,
    State(db): State<Arc<DatabasePool>>,
    axum::Extension(session): axum::Extension<Session>,
    Form(form): Form<OrgApiKeyForm>,
) -> Result<Response, Response> {
    let root = root_for_admin(&db, &session, &slug).await?;
    let submitted = |error: &str| FormState {
        error: Some(error.to_string()),
        key_description: form.description.clone(),
        key_permissions: Some(form.permissions.clone()),
        key_expires_in_days: form.expires_in_days.clone().unwrap_or_default(),
        ..FormState::default()
    };
    let expires_at = match form.expires_in_days.as_deref().map(str::trim) {
        None | Some("") => None,
        Some(days) => match days.parse().ok().filter(|days| (1..=3650).contains(days)) {
            Some(days) => Some(Utc::now() + chrono::Duration::days(days)),
            None => {
                let form = submitted("Expiry must be 1–3650 days.");
                return render_page(&db, &session, root, None, form).await;
            }
        },
    };
    let permissions = match serde_json::from_str(&form.permissions) {
        Ok(permissions) => permissions,
        Err(_) => {
            let form = submitted("Permissions must be a JSON object.");
            return render_page(&db, &session, root, None, form).await;
        }
    };
    let request = CreateOrgApiKey {
        description: form.description.clone(),
        permissions,
        expires_at,
    };
    match OrgApiKey::create_for_user(&db, root.org_id, session.current_user_id(), &request).await {
        Ok((_, token)) => render_page(&db, &session, root, Some(token), FormState::default()).await,
        Err(error) => {
            tracing::info!(root_org_id = %root.org_id, %error, "Org API key rejected");
            let form = submitted(
                "Unable to create the org API key. Permissions must use org: URNs such as org:* or org:{org}/env:*/project:*.",
            );
            render_page(&db, &session, root, None, form).await
        }
    }
}

pub async fn revoke_org_api_key(
    Path((slug, key_id)): Path<(String, Uuid)>,
    State(db): State<Arc<DatabasePool>>,
    axum::Extension(session): axum::Extension<Session>,
) -> Result<Redirect, Response> {
    let root = root_for_admin(&db, &session, &slug).await?;
    let error = OrgApiKey::revoke_for_user(&db, root.org_id, session.current_user_id(), key_id)
        .await
        .err()
        .map(|_| "Unable to revoke the org API key.".to_string());
    back_to_page(&db, &session, root, error).await
}

pub async fn root_lifecycle(
    Path(slug): Path<String>,
    State(db): State<Arc<DatabasePool>>,
    axum::Extension(session): axum::Extension<Session>,
    Form(form): Form<LifecycleForm>,
) -> Result<Redirect, Response> {
    let root = root_for_admin(&db, &session, &slug).await?;
    let error = Org::set_lifecycle(&db, &root.org_id, &session.current_user_id(), form.state)
        .await
        .err()
        .map(|error| org_error_message(&error, "That account status change isn't allowed."));
    back_to_page(&db, &session, root, error).await
}

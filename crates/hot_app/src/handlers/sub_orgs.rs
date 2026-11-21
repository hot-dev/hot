// Handlers answer errors with a rendered page: an axum Response, which is large by design.
#![allow(clippy::result_large_err)]

use crate::auth::Session;
use crate::handlers::list_query;
use crate::templates::{self, PrivatePageContext, filters};
use ahash::AHashMap;
use askama::Template;
use axum::extract::{Form, Path, Query, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Redirect, Response};
use chrono::{DateTime, Datelike, TimeZone, Utc};
use hot::db::org::{CreateChild, Org, OrgLifecycle, OrgUser};
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

pub(super) async fn root_for_admin(
    db: &DatabasePool,
    session: &Session,
    slug: &str,
) -> Result<Org, Response> {
    let root = Org::get_org_by_slug(db, slug)
        .await
        .map_err(|_| page_error(session, (StatusCode::NOT_FOUND, "Organization not found")))?;
    if root.is_individual() {
        return Err(page_error(
            session,
            (
                StatusCode::FORBIDDEN,
                "Personal accounts can't have sub-organizations. Create an organization to manage them.",
            ),
        ));
    }
    if root.is_child() || !session.can_manage_account(db, &root.org_id).await {
        return Err(page_error(
            session,
            (
                StatusCode::FORBIDDEN,
                "Administrator access to the parent organization is required",
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
            "A sub-organization already uses that customer reference.".to_string()
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
    /// The parent organization's own status and activity.
    account: OrgOverview,
    /// One page of sub-organizations.
    rows: Vec<OrgOverview>,
    total_children: i64,
    matching_children: i64,
    search: String,
    state: &'static str,
    /// Whether a search or status filter narrows the list.
    filtered: bool,
    /// The list URL up to its page number, keeping the search and filter.
    page_prefix: String,
    current_page_num: i64,
    start_page: i64,
    end_page: i64,
    has_next_page: bool,
    has_prev_page: bool,
}

#[derive(Template)]
#[template(path = "sub_orgs_new.html")]
struct SubOrgNewPage {
    title: &'static str,
    page_context: PrivatePageContext,
    root: Org,
    idempotency_key: Uuid,
    form: FormState,
}

#[derive(Template)]
#[template(path = "sub_org_detail.html")]
struct SubOrgDetailPage {
    title: String,
    page_context: PrivatePageContext,
    root: Org,
    child: Org,
    overview: OrgOverview,
    /// What the sub-organization's status means for its work right now.
    status_note: String,
    grants: Vec<ExplicitGrant>,
}

/// One choice on a status page.
struct LifecycleOption {
    value: &'static str,
    label: &'static str,
    note: &'static str,
}

/// The status page of the account or of one sub-organization.
#[derive(Template)]
#[template(path = "sub_org_lifecycle.html")]
struct LifecyclePage {
    title: String,
    page_context: PrivatePageContext,
    heading: String,
    intro: String,
    /// The organization's name, for the confirmation prompt.
    subject: String,
    confirm_note: &'static str,
    current_state: String,
    current_badge: &'static str,
    /// The choice to preselect: the current state, or what a rejected form asked for.
    selected_state: String,
    options: [LifecycleOption; 3],
    rule: &'static str,
    action: String,
    back_url: String,
    error: Option<String>,
}

#[derive(Template)]
#[template(path = "sub_org_access.html")]
struct SubOrgAccessPage {
    title: String,
    page_context: PrivatePageContext,
    root: Org,
    child: Org,
    members: Vec<hot::db::org::OrgUserWithRole>,
    grants: Vec<ExplicitGrant>,
    /// The member a rejected form chose.
    selected_user: String,
    error: Option<String>,
}

/// An account member given access to a sub-organization directly, rather than
/// through administering the account.
struct ExplicitGrant {
    email: String,
    role_label: &'static str,
}

struct OrgOverview {
    org_id: Uuid,
    name: String,
    slug: String,
    external_id: Option<String>,
    lifecycle: String,
    lifecycle_badge: &'static str,
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
///
/// Reports on exactly `orgs`, in order: each must be `root` or one of its
/// sub-organizations. A page passes the few organizations it shows, so the
/// cost follows the page and not the size of the account.
async fn account_overview(
    db: &DatabasePool,
    session: &Session,
    root: &Org,
    orgs: &[Org],
) -> Result<Vec<OrgOverview>, PageError> {
    let metadata = db.with_scope(DatabaseScope::Bootstrap(BootstrapOperation::Membership));
    let orgs = orgs.to_vec();
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
                name: org.name,
                slug: org.slug,
                external_id: org.external_id,
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
pub(super) fn display_time(session: &Session, time: &DateTime<Utc>) -> String {
    format!(
        "{} {}",
        crate::timezone::format_in_timezone(time, &session.display_timezone, "%Y-%m-%d %H:%M"),
        session.timezone_abbreviation
    )
}

/// How many sub-organizations a page of the list shows.
const CHILDREN_PER_PAGE: i64 = 25;

fn children_url(root: &Org) -> String {
    format!("/@{}/children", root.slug)
}

fn child_url(root: &Org, child_id: &Uuid) -> String {
    format!("/@{}/children/{child_id}", root.slug)
}

/// Breadcrumbs under the Sub-organizations list of `root`.
fn child_breadcrumbs(root: &Org, tail: Vec<templates::BreadcrumbItem>) -> templates::Breadcrumbs {
    // The organization itself is the breadcrumb bar's switcher.
    let mut breadcrumbs = vec![templates::BreadcrumbItem::clickable(
        "Sub-organizations".to_string(),
        children_url(root),
    )];
    breadcrumbs.extend(tail);
    breadcrumbs
}

/// A page with status 400 when it explains a rejected form, and no caching.
fn form_page(html: askama::Result<String>, rejected: bool) -> Result<Response, PageError> {
    let html = html.map_err(|_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Unable to render the page",
        )
    })?;
    let status = if rejected {
        StatusCode::BAD_REQUEST
    } else {
        StatusCode::OK
    };
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

/// A sub-organization of `root`, or the page that says there is none.
async fn child_of(
    db: &DatabasePool,
    session: &Session,
    root: &Org,
    child_id: &Uuid,
) -> Result<Org, Response> {
    Org::get_org_uncached(db, child_id)
        .await
        .ok()
        .filter(|child| child.parent_org_id == Some(root.org_id))
        .ok_or_else(|| {
            page_error(
                session,
                (StatusCode::NOT_FOUND, "Sub-organization not found"),
            )
        })
}

/// The account members given access to `child` directly.
async fn explicit_grants(db: &DatabasePool, child: &Org) -> Result<Vec<ExplicitGrant>, PageError> {
    let membership = db.with_scope(DatabaseScope::Bootstrap(BootstrapOperation::Membership));
    Ok(
        OrgUser::get_users_with_roles_by_org(&membership, &child.org_id)
            .await
            .map_err(|_| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Unable to load access to the sub-organization",
                )
            })?
            .into_iter()
            .filter(|member| member.active)
            .map(|member| ExplicitGrant {
                email: member.email,
                role_label: if member.org_user_role_id == 2 {
                    "Administrator"
                } else {
                    "Member"
                },
            })
            .collect(),
    )
}

/// The Sub-organizations list: one page of an account that may hold thousands.
pub async fn list(
    Path(slug): Path<String>,
    Query(params): Query<AHashMap<String, String>>,
    State(db): State<Arc<DatabasePool>>,
    axum::Extension(session): axum::Extension<Session>,
) -> Result<Response, Response> {
    let root = root_for_admin(&db, &session, &slug).await?;
    render_list(&db, &session, root, &params)
        .await
        .map_err(|error| page_error(&session, error))
}

async fn render_list(
    db: &DatabasePool,
    session: &Session,
    root: Org,
    params: &AHashMap<String, String>,
) -> Result<Response, PageError> {
    let page = list_query::PageParams::parse(params, CHILDREN_PER_PAGE);
    let search = params
        .get("search")
        .map(|search| search.trim().chars().take(200).collect::<String>())
        .unwrap_or_default();
    let (state, lifecycle) = match params.get("state").map(String::as_str) {
        Some("active") => ("active", Some(OrgLifecycle::Active)),
        Some("suspended") => ("suspended", Some(OrgLifecycle::Suspended)),
        Some("archived") => ("archived", Some(OrgLifecycle::Archived)),
        _ => ("", None),
    };
    let (children, matching_children, total_children) = Org::list_children_page(
        db,
        &root.org_id,
        &search,
        lifecycle,
        CHILDREN_PER_PAGE,
        page.offset,
    )
    .await
    .map_err(|_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Unable to load sub-organizations",
        )
    })?;
    // Activity for the account and this page's sub-organizations only.
    let mut shown = vec![root.clone()];
    shown.extend(children);
    let mut rows = account_overview(db, session, &root, &shown).await?;
    let account = rows.remove(0);
    let pagination = list_query::PaginationWindow::new(matching_children, &page);
    let query: String = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("search", &search)
        .append_pair("state", state)
        .finish();
    let breadcrumbs = vec![templates::BreadcrumbItem::current(
        "Sub-organizations".to_string(),
    )];
    let template = SubOrgsPage {
        title: "Sub-organizations",
        page_context: PrivatePageContext::for_org_page("children", session, breadcrumbs),
        account,
        rows,
        total_children,
        matching_children,
        filtered: !search.is_empty() || lifecycle.is_some(),
        page_prefix: format!("{}?{query}&p=", children_url(&root)),
        search,
        state,
        current_page_num: page.current_page_num,
        start_page: pagination.start_page,
        end_page: pagination.end_page,
        has_next_page: pagination.has_next_page,
        has_prev_page: pagination.has_prev_page,
        root,
    };
    form_page(template.render(), false)
}

fn render_new(session: &Session, root: Org, form: FormState) -> Result<Response, PageError> {
    let breadcrumbs = child_breadcrumbs(
        &root,
        vec![templates::BreadcrumbItem::current("New".to_string())],
    );
    let rejected = form.error.is_some();
    let template = SubOrgNewPage {
        title: "New Sub-organization",
        page_context: PrivatePageContext::for_org_page("children", session, breadcrumbs),
        root,
        idempotency_key: Uuid::now_v7(),
        form,
    };
    form_page(template.render(), rejected)
}

pub async fn new(
    Path(slug): Path<String>,
    State(db): State<Arc<DatabasePool>>,
    axum::Extension(session): axum::Extension<Session>,
) -> Result<Response, Response> {
    let root = root_for_admin(&db, &session, &slug).await?;
    render_new(&session, root, FormState::default()).map_err(|error| page_error(&session, error))
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
    // The form comes back with what was submitted and why it was refused.
    let rejected = |root: Org, error: String| {
        let form = FormState {
            error: Some(error),
            child_name: form.name.clone(),
            child_slug: form.slug.clone(),
            child_external_id: form.external_id.clone().unwrap_or_default(),
        };
        render_new(&session, root, form).unwrap_or_else(|error| page_error(&session, error))
    };
    let reserved = crate::slug::extra_reserved_from_conf(&conf);
    if let Err(error) = crate::slug::validate_format_with_extra(&slug, &reserved) {
        return Err(rejected(root, error.message().into()));
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
    match Org::create_child(&db, &root.org_id, &session.current_user_id(), &child).await {
        Ok(provisioned) => Ok(Redirect::to(&child_url(&root, &provisioned.org.org_id))),
        Err(error) => {
            tracing::warn!(root_org_id = %root.org_id, %error, "Child provisioning rejected");
            let message = org_error_message(&error, "Unable to create the sub-organization.");
            Err(rejected(root, message))
        }
    }
}

/// What a sub-organization's status means for its work right now.
fn status_note(root: &Org, child: &Org) -> String {
    match child.lifecycle_state.as_str() {
        "active" if !root.is_admitting() => format!(
            "Active, but held: the {} account is {}, so no new work starts here.",
            root.name, root.lifecycle_state
        ),
        "active" => "Active. New work starts normally.".to_string(),
        "suspended" => {
            "Suspended. New work is held and its API keys are refused until it is reactivated."
                .to_string()
        }
        _ => "Archived. It is retired and holds no new work.".to_string(),
    }
}

pub async fn detail(
    Path((slug, child_id)): Path<(String, Uuid)>,
    State(db): State<Arc<DatabasePool>>,
    axum::Extension(session): axum::Extension<Session>,
) -> Result<Response, Response> {
    let root = root_for_admin(&db, &session, &slug).await?;
    let child = child_of(&db, &session, &root, &child_id).await?;
    render_detail(&db, &session, root, child)
        .await
        .map_err(|error| page_error(&session, error))
}

async fn render_detail(
    db: &DatabasePool,
    session: &Session,
    root: Org,
    child: Org,
) -> Result<Response, PageError> {
    let overview = account_overview(db, session, &root, std::slice::from_ref(&child))
        .await?
        .pop()
        .ok_or((
            StatusCode::INTERNAL_SERVER_ERROR,
            "Unable to load account activity",
        ))?;
    let grants = explicit_grants(db, &child).await?;
    let breadcrumbs = child_breadcrumbs(
        &root,
        vec![templates::BreadcrumbItem::current(child.name.clone())],
    );
    let template = SubOrgDetailPage {
        title: child.name.clone(),
        page_context: PrivatePageContext::for_org_page("children", session, breadcrumbs),
        status_note: status_note(&root, &child),
        root,
        child,
        overview,
        grants,
    };
    form_page(template.render(), false)
}

#[derive(Deserialize)]
pub struct LifecycleForm {
    state: OrgLifecycle,
}

const CHILD_LIFECYCLE_OPTIONS: [LifecycleOption; 3] = [
    LifecycleOption {
        value: "active",
        label: "Active",
        note: "New work starts normally.",
    },
    LifecycleOption {
        value: "suspended",
        label: "Suspended",
        note: "Holds new work and refuses its API keys. Reversible.",
    },
    LifecycleOption {
        value: "archived",
        label: "Archived",
        note: "Retires the sub-organization. Restore it to suspended before activating it again.",
    },
];

const ACCOUNT_LIFECYCLE_OPTIONS: [LifecycleOption; 3] = [
    LifecycleOption {
        value: "active",
        label: "Active",
        note: "New work starts normally in the account and its sub-organizations.",
    },
    LifecycleOption {
        value: "suspended",
        label: "Suspended",
        note: "Holds new work in every sub-organization. Administrators keep recovery and billing access.",
    },
    LifecycleOption {
        value: "archived",
        label: "Archived",
        note: "Retires the account. Restore it to suspended before activating it again.",
    },
];

/// The status page of one sub-organization.
fn render_child_lifecycle(
    session: &Session,
    root: &Org,
    child: &Org,
    selected: Option<OrgLifecycle>,
    error: Option<String>,
) -> Result<Response, PageError> {
    let breadcrumbs = child_breadcrumbs(
        root,
        vec![
            templates::BreadcrumbItem::clickable(
                child.name.clone(),
                child_url(root, &child.org_id),
            ),
            templates::BreadcrumbItem::current("Status".to_string()),
        ],
    );
    let rejected = error.is_some();
    let template = LifecyclePage {
        title: format!("Status: {}", child.name),
        page_context: PrivatePageContext::for_org_page("children", session, breadcrumbs),
        heading: format!("Status of {}", child.name),
        intro: format!(
            "Changes only this sub-organization. Other sub-organizations of {} are not affected.",
            root.name
        ),
        subject: child.name.clone(),
        confirm_note: "New work stops in this sub-organization and its API keys are refused. Runs already in progress are not interrupted.",
        current_badge: lifecycle_badge(&child.lifecycle_state, false),
        selected_state: selected
            .map(|state| state.as_str().to_string())
            .unwrap_or_else(|| child.lifecycle_state.clone()),
        current_state: child.lifecycle_state.clone(),
        options: CHILD_LIFECYCLE_OPTIONS,
        rule: "An archived sub-organization goes back to suspended before it can be active.",
        action: format!("{}/lifecycle", child_url(root, &child.org_id)),
        back_url: child_url(root, &child.org_id),
        error,
    };
    form_page(template.render(), rejected)
}

pub async fn lifecycle_page(
    Path((slug, child_id)): Path<(String, Uuid)>,
    State(db): State<Arc<DatabasePool>>,
    axum::Extension(session): axum::Extension<Session>,
) -> Result<Response, Response> {
    let root = root_for_admin(&db, &session, &slug).await?;
    let child = child_of(&db, &session, &root, &child_id).await?;
    render_child_lifecycle(&session, &root, &child, None, None)
        .map_err(|error| page_error(&session, error))
}

pub async fn lifecycle(
    Path((slug, child_id)): Path<(String, Uuid)>,
    State(db): State<Arc<DatabasePool>>,
    axum::Extension(session): axum::Extension<Session>,
    Form(form): Form<LifecycleForm>,
) -> Result<Redirect, Response> {
    let root = root_for_admin(&db, &session, &slug).await?;
    let child = child_of(&db, &session, &root, &child_id).await?;
    match Org::set_lifecycle(&db, &child_id, &session.current_user_id(), form.state).await {
        Ok(_) => Ok(Redirect::to(&child_url(&root, &child_id))),
        Err(error) => {
            let message = org_error_message(
                &error,
                "That status change isn't allowed. Restore an archived sub-organization to suspended before activating it.",
            );
            Err(
                render_child_lifecycle(&session, &root, &child, Some(form.state), Some(message))
                    .unwrap_or_else(|error| page_error(&session, error)),
            )
        }
    }
}

/// The account's own status page.
fn render_root_lifecycle(
    session: &Session,
    root: &Org,
    selected: Option<OrgLifecycle>,
    error: Option<String>,
) -> Result<Response, PageError> {
    let breadcrumbs = child_breadcrumbs(
        root,
        vec![templates::BreadcrumbItem::current(
            "Account status".to_string(),
        )],
    );
    let rejected = error.is_some();
    let template = LifecyclePage {
        title: "Account Status".to_string(),
        page_context: PrivatePageContext::for_org_page("children", session, breadcrumbs),
        heading: format!("Account status of {}", root.name),
        intro: format!("Changes {} and every sub-organization under it.", root.name),
        subject: root.name.clone(),
        confirm_note: "New work stops in this organization and in every sub-organization, and their API keys are refused. Runs already in progress are not interrupted.",
        current_badge: lifecycle_badge(&root.lifecycle_state, false),
        selected_state: selected
            .map(|state| state.as_str().to_string())
            .unwrap_or_else(|| root.lifecycle_state.clone()),
        current_state: root.lifecycle_state.clone(),
        options: ACCOUNT_LIFECYCLE_OPTIONS,
        rule: "An archived account goes back to suspended before it can be active.",
        action: format!("/@{}/lifecycle", root.slug),
        back_url: children_url(root),
        error,
    };
    form_page(template.render(), rejected)
}

pub async fn root_lifecycle_page(
    Path(slug): Path<String>,
    State(db): State<Arc<DatabasePool>>,
    axum::Extension(session): axum::Extension<Session>,
) -> Result<Response, Response> {
    let root = root_for_admin(&db, &session, &slug).await?;
    render_root_lifecycle(&session, &root, None, None).map_err(|error| page_error(&session, error))
}

pub async fn root_lifecycle(
    Path(slug): Path<String>,
    State(db): State<Arc<DatabasePool>>,
    axum::Extension(session): axum::Extension<Session>,
    Form(form): Form<LifecycleForm>,
) -> Result<Redirect, Response> {
    let root = root_for_admin(&db, &session, &slug).await?;
    match Org::set_lifecycle(&db, &root.org_id, &session.current_user_id(), form.state).await {
        Ok(_) => Ok(Redirect::to(&children_url(&root))),
        Err(error) => {
            let message = org_error_message(&error, "That account status change isn't allowed.");
            Err(
                render_root_lifecycle(&session, &root, Some(form.state), Some(message))
                    .unwrap_or_else(|error| page_error(&session, error)),
            )
        }
    }
}

async fn render_access(
    db: &DatabasePool,
    session: &Session,
    root: Org,
    child: Org,
    selected_user: String,
    error: Option<String>,
) -> Result<Response, PageError> {
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
    let grants = explicit_grants(db, &child).await?;
    let breadcrumbs = child_breadcrumbs(
        &root,
        vec![
            templates::BreadcrumbItem::clickable(
                child.name.clone(),
                child_url(&root, &child.org_id),
            ),
            templates::BreadcrumbItem::current("Access".to_string()),
        ],
    );
    let rejected = error.is_some();
    let template = SubOrgAccessPage {
        title: format!("Access: {}", child.name),
        page_context: PrivatePageContext::for_org_page("children", session, breadcrumbs),
        root,
        child,
        members,
        grants,
        selected_user,
        error,
    };
    form_page(template.render(), rejected)
}

pub async fn access_page(
    Path((slug, child_id)): Path<(String, Uuid)>,
    State(db): State<Arc<DatabasePool>>,
    axum::Extension(session): axum::Extension<Session>,
) -> Result<Response, Response> {
    let root = root_for_admin(&db, &session, &slug).await?;
    let child = child_of(&db, &session, &root, &child_id).await?;
    render_access(&db, &session, root, child, String::new(), None)
        .await
        .map_err(|error| page_error(&session, error))
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
    let child = child_of(&db, &session, &root, &child_id).await?;
    let role = match form.role_id {
        0 => Ok(None),
        1 | 2 => Ok(Some(form.role_id)),
        _ => Err("Choose a role.".to_string()),
    };
    let saved = match role {
        Ok(role) => Org::set_child_user_grant(
            &db,
            &root.org_id,
            &child_id,
            &session.current_user_id(),
            &form.user_id,
            role,
        )
        .await
        .map_err(|error| org_error_message(&error, "Unable to change access.")),
        Err(message) => Err(message),
    };
    match saved {
        // Back to the access page, where the change shows in the list.
        Ok(()) => Ok(Redirect::to(&format!(
            "{}/access",
            child_url(&root, &child_id)
        ))),
        Err(message) => Err(render_access(
            &db,
            &session,
            root,
            child,
            form.user_id.to_string(),
            Some(message),
        )
        .await
        .unwrap_or_else(|error| page_error(&session, error))),
    }
}

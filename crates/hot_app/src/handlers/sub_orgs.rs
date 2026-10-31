use crate::auth::Session;
use crate::templates::{self, PrivatePageContext, filters};
use askama::Template;
use axum::extract::{Form, Path, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Redirect, Response};
use chrono::{DateTime, Datelike, TimeZone, Utc};
use hot::db::org::{CreateChild, Org, OrgLifecycle, OrgUser};
use hot::db::org_api_key::{CreateOrgApiKey, OrgApiKey};
use hot::db::{BootstrapOperation, DatabasePool, DatabaseScope};
use hot::val::Val;
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Arc;
use uuid::Uuid;

type PageError = (StatusCode, &'static str);

async fn root_for_admin(
    db: &DatabasePool,
    session: &Session,
    slug: &str,
) -> Result<Org, PageError> {
    let root = Org::get_org_by_slug(db, slug)
        .await
        .map_err(|_| (StatusCode::NOT_FOUND, "Organization not found"))?;
    if root.is_child() || !session.can_manage_account(db, &root.org_id).await {
        return Err((
            StatusCode::FORBIDDEN,
            "Root administrator access is required",
        ));
    }
    Ok(root)
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
    org_api_keys: Vec<OrgApiKey>,
    org_key_example: String,
    issued_token: Option<String>,
}

struct OrgOverview {
    org_id: Uuid,
    name: String,
    slug: String,
    lifecycle: String,
    root: bool,
    envs: Vec<OverviewEnv>,
    running: i64,
    failed_24h: i64,
    month_runs: i64,
    last_run: String,
    file_bytes: i64,
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
            let lifecycle = if org.is_child() && org.is_admitting() && !root.is_admitting() {
                format!("Held · account {}", root.lifecycle_state)
            } else {
                org.lifecycle_state.clone()
            };
            OrgOverview {
                org_id: org.org_id,
                root: org.org_id == root.org_id,
                name: org.name,
                slug: org.slug,
                lifecycle,
                running: rows.iter().map(|row| row.running).sum(),
                failed_24h: rows.iter().map(|row| row.failed_24h).sum(),
                month_runs: rows.iter().map(|row| row.month_runs).sum(),
                last_run: rows
                    .iter()
                    .filter_map(|row| row.last_run)
                    .max()
                    .map(|time| time.format("%Y-%m-%d %H:%M UTC").to_string())
                    .unwrap_or_else(|| "No runs".into()),
                file_bytes: files.get(&org.org_id).copied().unwrap_or(0),
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

pub async fn list(
    Path(slug): Path<String>,
    State(db): State<Arc<DatabasePool>>,
    axum::Extension(session): axum::Extension<Session>,
) -> Result<Response, PageError> {
    let root = root_for_admin(&db, &session, &slug).await?;
    render_page(&db, &session, root, None).await
}

async fn render_page(
    db: &DatabasePool,
    session: &Session,
    root: Org,
    issued_token: Option<String>,
) -> Result<Response, PageError> {
    let org_api_keys = OrgApiKey::list_for_user(db, root.org_id, session.current_user_id())
        .await
        .map_err(|_| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Unable to load org API keys",
            )
        })?;
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
    let overview = account_overview(db, &root, &children).await?;
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
    let template = SubOrgsPage {
        title: "Child organizations",
        page_context: PrivatePageContext::for_org_page("children", session, breadcrumbs),
        root,
        children,
        overview,
        members,
        idempotency_key: Uuid::now_v7(),
        org_api_keys,
        org_key_example,
        issued_token,
    };
    let html = template.render().map_err(|_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Unable to render child organizations",
        )
    })?;
    Ok((
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
) -> Result<Redirect, PageError> {
    let root = root_for_admin(&db, &session, &slug).await?;
    let reserved = crate::slug::extra_reserved_from_conf(&conf);
    crate::slug::validate_format_with_extra(&form.slug, &reserved)
        .map_err(|_| (StatusCode::BAD_REQUEST, "Invalid organization handle"))?;
    let child = CreateChild {
        name: form.name,
        slug: form.slug,
        external_id: form.external_id.filter(|value| !value.trim().is_empty()),
        idempotency_key: form.idempotency_key,
        hosted: hot::product::billing_enabled(&conf),
    };
    Org::create_child(&db, &root.org_id, &session.current_user_id(), &child)
        .await
        .map_err(|error| {
            tracing::warn!(root_org_id = %root.org_id, %error, "Child provisioning rejected");
            (
                StatusCode::BAD_REQUEST,
                "Unable to create child organization; check its handle and account limit",
            )
        })?;
    Ok(Redirect::to(&format!("/@{}/children", root.slug)))
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
) -> Result<Redirect, PageError> {
    let root = root_for_admin(&db, &session, &slug).await?;
    let child = Org::get_org_uncached(&db, &child_id)
        .await
        .map_err(|_| (StatusCode::NOT_FOUND, "Child organization not found"))?;
    if child.parent_org_id != Some(root.org_id) {
        return Err((StatusCode::NOT_FOUND, "Child organization not found"));
    }
    Org::set_lifecycle(&db, &child_id, &session.current_user_id(), form.state)
        .await
        .map_err(|_| {
            (
                StatusCode::BAD_REQUEST,
                "Lifecycle transition is not allowed",
            )
        })?;
    Ok(Redirect::to(&format!("/@{}/children", root.slug)))
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
) -> Result<Redirect, PageError> {
    let root = root_for_admin(&db, &session, &slug).await?;
    let role = match form.role_id {
        0 => None,
        1 | 2 => Some(form.role_id),
        _ => return Err((StatusCode::BAD_REQUEST, "Invalid child role")),
    };
    Org::set_child_user_grant(
        &db,
        &root.org_id,
        &child_id,
        &session.current_user_id(),
        &form.user_id,
        role,
    )
    .await
    .map_err(|_| (StatusCode::BAD_REQUEST, "Unable to change child access"))?;
    Ok(Redirect::to(&format!("/@{}/children", root.slug)))
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
) -> Result<Response, PageError> {
    let root = root_for_admin(&db, &session, &slug).await?;
    let expires_at = match form.expires_in_days.as_deref().map(str::trim) {
        None | Some("") => None,
        Some(days) => {
            let days: i64 = days
                .parse()
                .ok()
                .filter(|days| (1..=3650).contains(days))
                .ok_or((StatusCode::BAD_REQUEST, "Expiry must be 1–3650 days"))?;
            Some(Utc::now() + chrono::Duration::days(days))
        }
    };
    let request = CreateOrgApiKey {
        description: form.description,
        permissions: serde_json::from_str(&form.permissions)
            .map_err(|_| (StatusCode::BAD_REQUEST, "Invalid permissions JSON"))?,
        expires_at,
    };
    let (_, token) = OrgApiKey::create_for_user(&db, root.org_id, session.current_user_id(), &request)
        .await
        .map_err(|_| {
            (
                StatusCode::BAD_REQUEST,
                "Unable to create org API key; permissions must use org: URNs such as org:* or org:{org}/env:*/project:*",
            )
        })?;
    render_page(&db, &session, root, Some(token)).await
}

pub async fn revoke_org_api_key(
    Path((slug, key_id)): Path<(String, Uuid)>,
    State(db): State<Arc<DatabasePool>>,
    axum::Extension(session): axum::Extension<Session>,
) -> Result<Redirect, PageError> {
    let root = root_for_admin(&db, &session, &slug).await?;
    OrgApiKey::revoke_for_user(&db, root.org_id, session.current_user_id(), key_id)
        .await
        .map_err(|_| (StatusCode::BAD_REQUEST, "Unable to revoke org API key"))?;
    Ok(Redirect::to(&format!("/@{}/children", root.slug)))
}

pub async fn root_lifecycle(
    Path(slug): Path<String>,
    State(db): State<Arc<DatabasePool>>,
    axum::Extension(session): axum::Extension<Session>,
    Form(form): Form<LifecycleForm>,
) -> Result<Redirect, PageError> {
    let root = root_for_admin(&db, &session, &slug).await?;
    Org::set_lifecycle(&db, &root.org_id, &session.current_user_id(), form.state)
        .await
        .map_err(|_| {
            (
                StatusCode::BAD_REQUEST,
                "Lifecycle transition is not allowed",
            )
        })?;
    Ok(Redirect::to(&format!("/@{}/children", root.slug)))
}

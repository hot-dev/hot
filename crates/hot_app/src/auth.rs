use ahash::AHashMap;
use axum::{
    extract::{Request, State},
    http::StatusCode,
    middleware::Next,
    response::{IntoResponse, Redirect, Response},
};
use axum_extra::extract::CookieJar;
use chrono::{Duration, Utc};
use hot::db::{BootstrapOperation, DatabasePool, DatabaseScope, User, with_database_scope};
use hot::stream::StreamPubSub;
use hot::val::Val;
use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, Validation, decode, encode};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::watch;
use uuid::Uuid;

// App state that holds the database pool and configuration
#[derive(Clone)]
pub struct AppState {
    pub db: Arc<DatabasePool>,
    pub conf: Val,
    pub stream_pubsub: Option<Arc<StreamPubSub>>,
    /// Blob store for rehydrating spilled payloads before display.
    /// None when blob.mode is "disabled".
    pub blob_store: Option<Arc<hot::blob::BlobStore>>,
    /// Shutdown signal receiver - becomes true when server is shutting down
    /// Used by SSE handlers to cleanly terminate long-lived connections
    pub shutdown_rx: watch::Receiver<bool>,
}

impl AppState {
    pub fn new(db: Arc<DatabasePool>, conf: Val, shutdown_rx: watch::Receiver<bool>) -> Self {
        Self {
            db,
            conf,
            stream_pubsub: None,
            blob_store: None,
            shutdown_rx,
        }
    }

    pub fn with_stream_pubsub(mut self, pubsub: Option<Arc<StreamPubSub>>) -> Self {
        self.stream_pubsub = pubsub;
        self
    }

    pub fn with_blob_store(mut self, blob_store: Option<Arc<hot::blob::BlobStore>>) -> Self {
        self.blob_store = blob_store;
        self
    }
}

// Allow extracting Arc<DatabasePool> from AppState for handlers that only need db
impl axum::extract::FromRef<AppState> for Arc<DatabasePool> {
    fn from_ref(state: &AppState) -> Self {
        state.db.clone()
    }
}

// Allow extracting Val from AppState for handlers that need conf
impl axum::extract::FromRef<AppState> for Val {
    fn from_ref(state: &AppState) -> Self {
        state.conf.clone()
    }
}

// Allow extracting the optional blob store for handlers that rehydrate
// spilled payloads before display
impl axum::extract::FromRef<AppState> for Option<Arc<hot::blob::BlobStore>> {
    fn from_ref(state: &AppState) -> Self {
        state.blob_store.clone()
    }
}

// Session struct to hold all user context data
#[derive(Debug, Clone)]
pub struct Session {
    pub user: User,
    pub user_initials: String,
    pub user_name: String,
    pub current_org: Option<hot::db::org::Org>,
    pub user_orgs: Vec<hot::db::org::Org>,
    pub is_current_org_admin: bool,
    pub current_env: Option<hot::db::env::Env>,
    pub current_org_envs: Vec<hot::db::env::Env>,
    /// Resolved display timezone (user > org > UTC)
    pub display_timezone: String,
    /// Abbreviation for the display timezone (e.g., "CST", "EST")
    pub timezone_abbreviation: String,
    /// Subscription status for the current org (None for local dev or no org)
    pub current_org_subscription_status: Option<hot::db::OrgPlanStatus>,
    /// Plan name for the current org (e.g., "Hot Cloud Starter", "Hot Cloud Pro")
    pub current_org_plan_name: Option<String>,
    /// Resolved features for the current org (plan defaults + org overrides)
    pub current_org_features: hot::db::Features,
    /// User's preferred value format: "hot" (default) or "json"
    pub value_format: String,
    /// Product experience for UX policy (local-dev, self-host, hot-cloud).
    pub product_experience: hot::product::ProductExperienceMode,
    /// Whether hosted billing is enabled for this request.
    pub billing_enabled: bool,
    /// Product marketing site URL.
    pub product_web_url: String,
    /// Product pricing page URL.
    pub product_pricing_url: String,
    /// Product support contact email.
    pub product_support_email: String,
    /// Why the selected workspace is held (suspended or archived itself, or its account), if it is.
    /// Pages show it as a notice; only recovery paths stay open while it is set.
    pub current_org_hold: Option<String>,
}

/// Why a signed session could not be loaded.
#[derive(Debug)]
pub enum SessionError {
    /// The user is missing or inactive, so the token can never work again.
    Unusable(String),
    /// The lookup failed (for example a database timeout); the token may be fine.
    Unavailable(String),
}

impl std::fmt::Display for SessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unusable(reason) | Self::Unavailable(reason) => f.write_str(reason),
        }
    }
}

impl std::error::Error for SessionError {}

/// Clear a token only when it can never load: /signin sends any valid token
/// back to /, so keeping an unusable one loops. A failed lookup keeps the
/// cookies and asks the client to retry, so a database blip does not sign
/// everyone out.
fn handle_session_error(
    error: SessionError,
    cookies: CookieJar,
    user_id: &Uuid,
) -> Result<CookieJar, StatusCode> {
    match error {
        SessionError::Unusable(reason) => {
            tracing::info!(%user_id, %reason, "Clearing unusable session");
            Ok(cookies
                .add(build_removal_cookie(jwt_cookie_name()))
                .add(build_removal_cookie(current_org_cookie_name()))
                .add(build_removal_cookie(current_env_cookie_name())))
        }
        SessionError::Unavailable(reason) => {
            tracing::error!(%user_id, %reason, "Session lookup failed");
            Err(StatusCode::SERVICE_UNAVAILABLE)
        }
    }
}

impl Session {
    /// Create a new session from user_id and cookies
    pub async fn from_user_id(
        db: &DatabasePool,
        conf: &Val,
        user_id: &Uuid,
        cookies: &CookieJar,
    ) -> Result<Self, SessionError> {
        let scoped_db = db.with_scope(DatabaseScope::Bootstrap(BootstrapOperation::Membership));
        let db = &scoped_db;
        // Get user details
        let user = match User::get_user(db, user_id).await {
            Ok(user) => user,
            Err(
                hot::db::user::UserError::NotFound
                | hot::db::user::UserError::Database(sqlx::Error::RowNotFound),
            ) => return Err(SessionError::Unusable("User not found".into())),
            Err(error) => {
                return Err(SessionError::Unavailable(format!(
                    "Failed to get user: {error}"
                )));
            }
        };
        if !user.active {
            return Err(SessionError::Unusable("User is inactive".into()));
        }

        // Extract initials from name or email
        let user_initials = if let Some(name) = &user.name {
            name.split_whitespace()
                .map(|word| word.chars().next().unwrap_or('U'))
                .take(2)
                .collect::<String>()
                .to_uppercase()
        } else {
            user.email
                .chars()
                .next()
                .unwrap_or('U')
                .to_uppercase()
                .to_string()
        };

        let user_name = user.name.as_ref().unwrap_or(&user.email).clone();

        // Get all organizations for this user
        let user_orgs = hot::db::org::Org::get_orgs_by_user(db, user_id)
            .await
            .map_err(|error| {
                SessionError::Unavailable(format!(
                    "Unable to resolve organization membership: {error}"
                ))
            })?;

        // Get current organization from cookie or default to first org
        let current_org_id = get_current_org_id_from_cookies(cookies);
        let current_org = if let Some(org_id_str) = current_org_id {
            // Try to parse the string ID as a UUID and find the org
            if let Ok(org_id_uuid) = uuid::Uuid::parse_str(&org_id_str) {
                let found_org = user_orgs
                    .iter()
                    .find(|org| org.org_id == org_id_uuid)
                    .cloned();

                // If the cookie org_id doesn't exist in user's orgs (stale cookie),
                // fall back to first org and log a warning
                if found_org.is_none() && !user_orgs.is_empty() {
                    tracing::warn!(
                        "Cookie org_id {} not found in user's organizations - falling back to first org. This may happen after database reset.",
                        org_id_str
                    );
                    user_orgs.first().cloned()
                } else {
                    found_org
                }
            } else {
                // Invalid UUID format, use first org
                user_orgs.first().cloned()
            }
        } else {
            // No cookie set, default to first org
            user_orgs.first().cloned()
        };

        // Check if user is admin of current organization
        let is_current_org_admin = if let Some(ref org) = current_org {
            hot::db::Org::resolve_user_access(db, &org.org_id, user_id)
                .await
                .ok()
                .flatten()
                .is_some_and(|access| access.is_admin)
        } else {
            false
        };

        // Get environments for current organization
        let current_org_envs = if let Some(ref org) = current_org {
            hot::db::env::Env::get_envs_by_org(db, &org.org_id)
                .await
                .map_err(|error| {
                    SessionError::Unavailable(format!("Unable to resolve environments: {error}"))
                })?
        } else {
            Vec::new()
        };

        // Get current environment from cookie or default to first env
        let current_env_id = get_current_env_id_from_cookies(cookies);
        let current_env = if let Some(env_id_str) = current_env_id {
            // Try to parse the string ID as a UUID and find the env
            if let Ok(env_id_uuid) = uuid::Uuid::parse_str(&env_id_str) {
                let found_env = current_org_envs
                    .iter()
                    .find(|env| env.env_id == env_id_uuid)
                    .cloned();

                // If the cookie env_id doesn't exist in current org's envs (stale cookie),
                // fall back to first env and log a warning
                if found_env.is_none() && !current_org_envs.is_empty() {
                    tracing::warn!(
                        "Cookie env_id {} not found in current org's environments - falling back to first env. This may happen after database reset.",
                        env_id_str
                    );
                    current_org_envs.first().cloned()
                } else {
                    found_env
                }
            } else {
                // Invalid UUID format, use first env
                current_org_envs.first().cloned()
            }
        } else {
            // No cookie set, default to first env
            current_org_envs.first().cloned()
        };

        // Resolve display timezone: user > org > UTC
        let user_tz = user.get_display_timezone();
        let org_tz = current_org
            .as_ref()
            .and_then(|org| org.get_display_timezone());
        let display_timezone =
            crate::timezone::resolve_display_timezone(user_tz.as_deref(), org_tz.as_deref());

        // Get timezone abbreviation
        let timezone_abbreviation = crate::timezone::get_timezone_abbreviation(&display_timezone);

        let billing_enabled = hot::product::billing_enabled(conf);

        // Get subscription status, plan name, and resolved features for current org
        let (current_org_subscription_status, current_org_plan_name, current_org_features) =
            if hot::env::is_local_dev() && !billing_enabled {
                (None, None, hot::db::Features::unlimited())
            } else if let Some(ref org) = current_org {
                // Try to get subscription and plan info
                let (status, plan_name) =
                    match hot::db::OrgPlan::get_by_org_id(db, &org.billing_owner_id()).await {
                        Ok(sub) => {
                            let status = sub.status();
                            let plan_name = sub.get_plan(db).await.ok().map(|p| p.plan_name);
                            (status, plan_name)
                        }
                        Err(_) => (None, None),
                    };
                // Resolve features (plan defaults + org overrides)
                let features = if billing_enabled {
                    hot::db::Features::resolve_for_hosted_org(db, &org.org_id).await
                } else {
                    hot::db::Features::resolve_for_org(db, &org.org_id).await
                };
                (status, plan_name, features)
            } else if billing_enabled {
                (None, None, hot::db::Features::resolve(None, None))
            } else {
                (None, None, hot::db::Features::unlimited())
            };

        // Get value format preference
        let value_format = user.get_value_format();

        Ok(Session {
            current_org_hold: None,
            user,
            user_initials,
            user_name,
            current_org,
            user_orgs,
            is_current_org_admin,
            current_env,
            current_org_envs,
            display_timezone,
            timezone_abbreviation,
            current_org_subscription_status,
            current_org_plan_name,
            current_org_features,
            value_format,
            product_experience: hot::product::experience(conf),
            billing_enabled,
            product_web_url: hot::product::web_url(conf),
            product_pricing_url: hot::product::pricing_url(conf),
            product_support_email: hot::product::support_email(conf),
        })
    }

    pub fn current_user_id(&self) -> Uuid {
        self.user.user_id
    }

    pub fn is_local_dev_experience(&self) -> bool {
        matches!(
            self.product_experience,
            hot::product::ProductExperienceMode::LocalDev
        )
    }

    pub fn is_self_host_experience(&self) -> bool {
        matches!(
            self.product_experience,
            hot::product::ProductExperienceMode::SelfHost
        )
    }

    pub fn is_hot_cloud_experience(&self) -> bool {
        matches!(
            self.product_experience,
            hot::product::ProductExperienceMode::HotCloud
        )
    }

    /// Check if user is admin of a specific organization (by string ID)
    pub async fn is_org_admin(&self, db: &DatabasePool, org_id: &Uuid) -> bool {
        hot::db::Org::resolve_user_access(db, org_id, &self.user.user_id)
            .await
            .ok()
            .flatten()
            .is_some_and(|access| access.is_admin)
    }

    /// Account administration is inherited from the root, not from a child role.
    pub async fn can_manage_account(&self, db: &DatabasePool, org_id: &Uuid) -> bool {
        hot::db::Org::resolve_user_access(db, org_id, &self.user.user_id)
            .await
            .ok()
            .flatten()
            .is_some_and(|access| access.can_manage_account)
    }

    pub fn billing_org(&self) -> Option<&hot::db::Org> {
        let org = self.current_org.as_ref()?;
        let root_id = org.billing_owner_id();
        self.user_orgs
            .iter()
            .find(|candidate| candidate.org_id == root_id)
    }

    /// A URL/header target belongs to this page; ambient cookies are only a
    /// navigation default. Never turn an invalid explicit selector into a fallback.
    fn select_request_scope<'a>(
        &'a mut self,
        db: &'a DatabasePool,
        request: &Request,
    ) -> impl std::future::Future<Output = Result<(), StatusCode>> + Send + 'a {
        let uri = request.uri().clone();
        let method = request.method().clone();
        let headers = request.headers().clone();
        async move {
            let pairs = uri
                .query()
                .map(|query| {
                    url::form_urlencoded::parse(query.as_bytes())
                        .into_owned()
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            for selector in ["org_id", "env_id"] {
                if pairs.iter().filter(|(name, _)| name == selector).count() > 1 {
                    return Err(StatusCode::BAD_REQUEST);
                }
            }
            let params = uri
                .query()
                .map(|query| {
                    url::form_urlencoded::parse(query.as_bytes())
                        .into_owned()
                        .collect::<AHashMap<String, String>>()
                })
                .unwrap_or_default();
            let selector = |name: &str, header: &str| -> Result<Option<Uuid>, StatusCode> {
                let query = params.get(name).map(String::as_str);
                let header = headers
                    .get(header)
                    .map(|v| v.to_str().map_err(|_| StatusCode::BAD_REQUEST))
                    .transpose()?;
                if query.is_some() && header.is_some() && query != header {
                    return Err(StatusCode::BAD_REQUEST);
                }
                query
                    .or(header)
                    .map(|s| Uuid::parse_str(s).map_err(|_| StatusCode::BAD_REQUEST))
                    .transpose()
            };
            let requested_org = selector("org_id", "x-hot-org-id")?;
            let requested_env = selector("env_id", "x-hot-env-id")?;
            let path = uri.path();
            let route_slug = path
                .strip_prefix("/@")
                .and_then(|tail| tail.split('/').next());
            let org = if let Some(slug) = route_slug {
                let org = self
                    .user_orgs
                    .iter()
                    .find(|org| org.slug == slug)
                    .cloned()
                    .ok_or(StatusCode::NOT_FOUND)?;
                if requested_org.is_some_and(|id| id != org.org_id) {
                    return Err(StatusCode::BAD_REQUEST);
                }
                Some(org)
            } else if let Some(id) = requested_org {
                Some(
                    self.user_orgs
                        .iter()
                        .find(|org| org.org_id == id)
                        .cloned()
                        .ok_or(StatusCode::NOT_FOUND)?,
                )
            } else {
                self.current_org.clone()
            };
            let org_control = is_org_control_path(path);
            if !matches!(method, axum::http::Method::GET | axum::http::Method::HEAD)
                // Handle claiming is user onboarding. An already-member's
                // stale form only redirects; it cannot mutate that org.
                && path != "/claim-handle"
                && !is_scope_selection_path(path)
                && route_slug.is_none()
                && self.current_org.is_some()
                && (requested_org.is_none() || (!org_control && requested_env.is_none()))
            {
                return Err(StatusCode::BAD_REQUEST);
            }
            if let Some(org) = org {
                self.select_org(db, org).await?;
            }
            if let Some(env_id) = requested_env {
                self.current_env = Some(
                    self.current_org_envs
                        .iter()
                        .find(|env| env.env_id == env_id)
                        .cloned()
                        .ok_or(StatusCode::NOT_FOUND)?,
                );
            }
            if !org_control
                && !is_scope_selection_path(path)
                && self.current_env.as_ref().is_some_and(|env| !env.active)
            {
                return Err(StatusCode::FORBIDDEN);
            }
            if let Some(org) = &self.current_org {
                let access = hot::db::Org::resolve_user_access(db, &org.org_id, &self.user.user_id)
                    .await
                    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
                    .ok_or(StatusCode::NOT_FOUND)?;
                self.current_org_hold =
                    (!access.runtime_allowed).then(|| hold_reason(org, &self.user_orgs));
                if !access.runtime_allowed
                    && !(access.can_manage_account && is_recovery_path(path, &method))
                {
                    return Err(StatusCode::FORBIDDEN);
                }
            }
            Ok(())
        }
    }

    fn database_scope(&self, path: &str) -> DatabaseScope {
        if path == "/account" || path == "/account/notifications" {
            return DatabaseScope::User {
                user_id: self.current_user_id(),
            };
        }
        if path == "/claim-handle" || path == "/orgs/new" || self.current_org.is_none() {
            return DatabaseScope::Bootstrap(BootstrapOperation::Onboarding);
        }
        let org_id = self.current_org_id().expect("checked current organization");
        if !is_org_control_path(path)
            && !is_scope_selection_path(path)
            && let Some(env_id) = self.current_env_id()
        {
            return DatabaseScope::Environment { org_id, env_id };
        }
        DatabaseScope::Organization { org_id }
    }

    async fn select_org(&mut self, db: &DatabasePool, org: hot::db::Org) -> Result<(), StatusCode> {
        if self.current_org_id() == Some(org.org_id) {
            return Ok(());
        }
        let access = hot::db::Org::resolve_user_access(db, &org.org_id, &self.user.user_id)
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
            .ok_or(StatusCode::NOT_FOUND)?;
        self.current_org_envs = hot::db::Env::get_envs_by_org(db, &org.org_id)
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        self.current_env = self.current_org_envs.first().cloned();
        self.is_current_org_admin = access.is_admin;
        self.current_org_features = if self.billing_enabled {
            hot::db::Features::resolve_for_hosted_org(db, &org.org_id).await
        } else {
            hot::db::Features::resolve_for_org(db, &org.org_id).await
        };
        let plan = hot::db::OrgPlan::get_by_org_id(db, &org.billing_owner_id())
            .await
            .ok();
        self.current_org_subscription_status = plan.as_ref().and_then(|plan| plan.status());
        self.current_org_plan_name = match plan {
            Some(plan) => plan.get_plan(db).await.ok().map(|plan| plan.plan_name),
            None => None,
        };
        // A user without a timezone of their own sees the selected org's.
        self.display_timezone = crate::timezone::resolve_display_timezone(
            self.user.get_display_timezone().as_deref(),
            org.get_display_timezone().as_deref(),
        );
        self.timezone_abbreviation =
            crate::timezone::get_timezone_abbreviation(&self.display_timezone);
        self.current_org = Some(org);
        Ok(())
    }

    /// Get the current organization ID, if any
    pub fn current_org_id(&self) -> Option<Uuid> {
        self.current_org.as_ref().map(|org| org.org_id)
    }

    /// Check if user has access to a specific organization (by string ID)
    pub fn has_org_access(&self, org_id: &Uuid) -> bool {
        self.user_orgs.iter().any(|org| org.org_id == *org_id)
    }

    /// Get the current environment ID, if any
    pub fn current_env_id(&self) -> Option<Uuid> {
        self.current_env.as_ref().map(|env| env.env_id)
    }

    /// Check if user has access to a specific environment (via organization) (by string ID)
    pub fn has_env_access(&self, env_id: &Uuid) -> bool {
        self.current_org_envs
            .iter()
            .any(|env| env.env_id == *env_id)
    }

    /// Get display organizations for UI (all orgs are now visible)
    pub fn display_orgs(&self) -> Vec<hot::db::org::Org> {
        self.user_orgs.clone()
    }

    /// Check if user has no orgs (needs to claim a handle)
    pub fn has_no_orgs(&self) -> bool {
        self.user_orgs.is_empty()
    }

    /// Get the user's individual org if it exists
    pub fn individual_org(&self) -> Option<&hot::db::org::Org> {
        self.user_orgs.iter().find(|org| org.is_individual())
    }
}

// JWT Claims structure
#[derive(Debug, Serialize, Deserialize)]
pub struct Claims {
    pub sub: String, // user_id
    pub exp: usize,  // expiration time
    pub iat: usize,  // issued at
}

/// A cookie's name: `__Host-` and `base` outside local development. The
/// prefix keeps a subdomain from setting or overwriting the cookie, but a
/// browser takes it only on a secure cookie, and local development serves
/// plain HTTP.
fn cookie_name(base: &'static str, host_only: &'static str, local: bool) -> &'static str {
    debug_assert_eq!(host_only.strip_prefix("__Host-"), Some(base));
    if local { base } else { host_only }
}

/// Name of the JWT token cookie.
pub fn jwt_cookie_name() -> &'static str {
    cookie_name(
        "hot_auth_token",
        "__Host-hot_auth_token",
        hot::env::is_local_dev(),
    )
}

/// Name of the current organization cookie.
pub fn current_org_cookie_name() -> &'static str {
    cookie_name(
        "hot_current_org",
        "__Host-hot_current_org",
        hot::env::is_local_dev(),
    )
}

/// Name of the current environment cookie.
pub fn current_env_cookie_name() -> &'static str {
    cookie_name(
        "hot_current_env",
        "__Host-hot_current_env",
        hot::env::is_local_dev(),
    )
}

/// Session length in days for the JWT cookie and token (30-day sessions with
/// sliding refresh in the session middleware; see `session_middleware`).
pub const SESSION_COOKIE_DAYS: i64 = 30;

/// Name of the cookie for the CSRF double-submit token used on guest auth
/// forms (signin / signup). The same random value is set as a cookie and
/// embedded as a hidden form field; a POST is only accepted when the two
/// match.
pub fn csrf_cookie_name() -> &'static str {
    cookie_name("hot_csrf", "__Host-hot_csrf", hot::env::is_local_dev())
}

/// Generate a random CSRF token (double-submit pattern; the value carries no
/// claims, it only has to be unguessable and match the cookie).
pub fn generate_csrf_token() -> String {
    format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple())
}

/// Build the CSRF cookie for `token` (1h lifetime, enough for a form fill).
pub fn build_csrf_cookie(token: String) -> axum_extra::extract::cookie::Cookie<'static> {
    build_cookie(csrf_cookie_name(), token, time::Duration::hours(1))
}

/// Validate a submitted form token against the CSRF cookie. Both must be
/// present, non-empty, and equal.
pub fn validate_csrf(cookies: &CookieJar, form_token: Option<&str>) -> bool {
    let Some(cookie_value) = cookies.get(csrf_cookie_name()).map(|c| c.value()) else {
        return false;
    };
    match form_token {
        Some(token) if !token.is_empty() => token == cookie_value,
        _ => false,
    }
}

/// Is `next` a safe same-site redirect target?
///
/// Accepts only absolute paths within this origin. Rejects:
/// - protocol-relative URLs (`//evil.com`)
/// - backslash variants (`/\evil.com`) which some browsers normalize to `//`
pub fn is_safe_next(next: &str) -> bool {
    next.starts_with('/') && !next.starts_with("//") && !next.contains('\\')
}

/// Build a session-scoped cookie with the hardened defaults used everywhere
/// in hot_app: path `/`, HttpOnly, SameSite=Lax, and no domain; Secure
/// outside local dev, as the `__Host-` names require there.
///
/// Callers that need a JS-readable cookie (e.g. the cross-subdomain presence
/// cookie) should call `.set_http_only(false)` on the result.
pub fn build_cookie(
    name: &'static str,
    value: String,
    max_age: time::Duration,
) -> axum_extra::extract::cookie::Cookie<'static> {
    let mut cookie = axum_extra::extract::cookie::Cookie::new(name, value);
    cookie.set_path("/");
    cookie.set_max_age(max_age);
    cookie.set_http_only(true);
    cookie.set_same_site(axum_extra::extract::cookie::SameSite::Lax);
    cookie.set_secure(!hot::env::is_local_dev());
    cookie
}

/// Build an expired cookie that clears `name` on the client.
pub fn build_removal_cookie(name: &'static str) -> axum_extra::extract::cookie::Cookie<'static> {
    build_cookie(name, String::new(), time::Duration::seconds(0))
}

/// Whether the App listens only on a loopback address. Set once at startup.
static LISTENS_ON_LOOPBACK: std::sync::OnceLock<bool> = std::sync::OnceLock::new();

/// Whether `host` names only this machine: `localhost` or a loopback address.
pub(crate) fn is_loopback_host(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// Record the host the App listens on, before it signs or checks a session.
pub(crate) fn set_listen_host(host: &str) {
    let _ = LISTENS_ON_LOOPBACK.set(is_loopback_host(host));
}

/// Whether local development's conveniences apply: signing visitors in, the
/// built-in session secret and the seeded local password. An unset `HOT_ENV`
/// means local development, so on a network address they also need
/// `HOT_ENV=development` set: a deployment that forgot `HOT_ENV` must not
/// sign in everyone who can reach it.
pub fn local_dev_conveniences() -> bool {
    hot::env::is_local_dev()
        && (std::env::var_os("HOT_ENV").is_some()
            || LISTENS_ON_LOOPBACK.get().copied().unwrap_or(true))
}

/// Get session secret from environment variable
/// In development mode, falls back to a default secret for convenience
pub(crate) fn get_session_secret() -> Result<String, String> {
    match hot::environment::read_optional("HOT_APP_SESSION_SECRET")? {
        Some(secret) if !secret.is_empty() => Ok(secret),
        _ => {
            // In local development, use a fallback secret for convenience
            if local_dev_conveniences() {
                Ok("hotdev-session-secret-key-change-in-production".to_string())
            } else if hot::env::is_local_dev() {
                Err(
                    "HOT_APP_SESSION_SECRET is required: HOT_ENV is unset and the App \
                     listens on a network address. Set HOT_ENV=production and \
                     HOT_APP_SESSION_SECRET for a deployment, or HOT_ENV=development to \
                     keep local development's built-in secret"
                        .to_string(),
                )
            } else {
                Err(
                    "HOT_APP_SESSION_SECRET environment variable is required in production"
                        .to_string(),
                )
            }
        }
    }
}

/// Get session timeout in hours from conf, defaulting to 30 days.
fn get_session_timeout_hours(conf: &Val) -> i64 {
    conf.get("app")
        .and_then(|app| app.get("session"))
        .and_then(|session| session.get("timeout"))
        .and_then(|timeout| match timeout {
            Val::Int(i) => Some(i),
            _ => None,
        })
        .unwrap_or(SESSION_COOKIE_DAYS * 24)
}

/// Generate a JWT token for a user using configuration
pub fn generate_token(user_id: &Uuid, conf: &Val) -> Result<String, String> {
    let secret = get_session_secret()?;
    let timeout_hours = get_session_timeout_hours(conf);

    let now = Utc::now();
    let expires_at = now + Duration::hours(timeout_hours);

    let claims = Claims {
        sub: user_id.to_string(),
        exp: expires_at.timestamp() as usize,
        iat: now.timestamp() as usize,
    };

    let key = EncodingKey::from_secret(secret.as_ref());

    encode(&Header::new(Algorithm::HS256), &claims, &key)
        .map_err(|e| format!("Failed to generate token: {}", e))
}

/// Validate a JWT token and extract user_id
pub fn validate_token(token: &str) -> Result<Uuid, String> {
    let secret = get_session_secret()?;
    let key = DecodingKey::from_secret(secret.as_ref());

    let token_data = decode::<Claims>(token, &key, &Validation::new(Algorithm::HS256))
        .map_err(|e| format!("Failed to validate token: {}", e))?;

    Uuid::parse_str(&token_data.claims.sub).map_err(|e| format!("Invalid user ID in token: {}", e))
}

/// Extract user_id from JWT cookie in request
pub fn get_user_id_from_cookies(cookies: &CookieJar) -> Option<Uuid> {
    cookies
        .get(jwt_cookie_name())
        .and_then(|cookie| validate_token(cookie.value()).ok())
}

/// Sliding session refresh: if the JWT in the cookie jar is past half its
/// lifetime, mint a fresh token. Returns the new cookie to set, or `None`
/// when the current token is still young (or absent/invalid — expiry is the
/// signin flow's problem, not the refresher's).
fn refresh_session_cookie(
    cookies: &CookieJar,
    conf: &Val,
) -> Option<axum_extra::extract::cookie::Cookie<'static>> {
    let token = cookies.get(jwt_cookie_name())?.value();
    let secret = get_session_secret().ok()?;
    let key = DecodingKey::from_secret(secret.as_ref());
    let claims = decode::<Claims>(token, &key, &Validation::new(Algorithm::HS256))
        .ok()?
        .claims;

    let now = Utc::now().timestamp();
    let half_life = (claims.exp as i64 - claims.iat as i64) / 2;
    if now - claims.iat as i64 <= half_life {
        return None;
    }

    let user_id = Uuid::parse_str(&claims.sub).ok()?;
    let new_token = generate_token(&user_id, conf).ok()?;
    Some(build_cookie(
        jwt_cookie_name(),
        new_token,
        time::Duration::days(SESSION_COOKIE_DAYS),
    ))
}

/// Extract current_org_id from cookie in request
pub fn get_current_org_id_from_cookies(cookies: &CookieJar) -> Option<String> {
    cookies
        .get(current_org_cookie_name())
        .map(|cookie| cookie.value().to_string())
}

/// Extract current_env_id from cookie in request
pub fn get_current_env_id_from_cookies(cookies: &CookieJar) -> Option<String> {
    cookies
        .get(current_env_cookie_name())
        .map(|cookie| cookie.value().to_string())
}

/// Extract session from request extensions
pub fn get_session_from_request(request: &Request) -> Option<&Session> {
    request.extensions().get::<Session>()
}

/// Check if request is from HTMX by looking for HX-Request header
fn is_htmx_request(request: &Request) -> bool {
    crate::handlers::is_htmx_request(request.headers())
}

/// Create a redirect response that works with both normal and HTMX requests
fn create_redirect_response(request: &Request, path: &str) -> Response {
    if is_htmx_request(request) {
        // For HTMX requests, use HX-Redirect header to trigger a client-side redirect
        Response::builder()
            .status(StatusCode::OK)
            .header("HX-Redirect", path)
            .body(axum::body::Body::empty())
            .unwrap()
    } else {
        // For normal requests, use standard redirect
        Redirect::to(path).into_response()
    }
}

fn is_org_billing_path(path: &str) -> bool {
    let mut segments = path.split('/');
    let _empty = segments.next();
    matches!(
        (segments.next(), segments.next()),
        (Some(org), Some("billing")) if org.starts_with('@')
    )
}

fn is_plan_gate_exempt_path(path: &str) -> bool {
    path == "/claim-handle"
        || path == "/account/billing"
        || path.starts_with("/billing/")
        || path.starts_with("/switch-org/")
        || is_org_billing_path(path)
}

fn onboarding_redirect(session: &Session, path: &str) -> Option<String> {
    if !session.billing_enabled || is_plan_gate_exempt_path(path) {
        return None;
    }

    match session.current_org.as_ref() {
        Some(org) if session.current_org_subscription_status.is_none() => {
            let root = session.billing_org().unwrap_or(org);
            Some(format!("/@{}/billing/checkout", root.slug))
        }
        None if session.user_orgs.is_empty() => Some("/claim-handle".to_string()),
        _ => None,
    }
}

/// Explain a held workspace: its own lifecycle state, or its account's.
fn hold_reason(org: &hot::db::org::Org, user_orgs: &[hot::db::org::Org]) -> String {
    if org.lifecycle_state != "active" {
        return format!("This workspace is {}.", org.lifecycle_state);
    }
    let root_id = org.billing_owner_id();
    match user_orgs
        .iter()
        .find(|candidate| candidate.org_id == root_id)
    {
        Some(root) if root.org_id != org.org_id => {
            format!(
                "This workspace's account, {}, is {}.",
                root.name, root.lifecycle_state
            )
        }
        _ => "This workspace's account is not active.".to_string(),
    }
}

/// The response for a request whose explicit scope was refused: the shared error page, or a
/// short fragment for HTMX widgets, instead of an empty body.
fn scope_error_response(session: &Session, status: StatusCode, is_htmx: bool) -> Response {
    let message = match (status, &session.current_org_hold) {
        (StatusCode::FORBIDDEN, Some(hold)) => {
            // Name the parent only to members who can see it.
            let admins = match session.billing_org() {
                Some(parent) => format!("an administrator of {}", parent.name),
                None => "an administrator of its parent organization".to_string(),
            };
            format!("{hold} Runs, deploys and data are unavailable until {admins} restores it.")
        }
        (StatusCode::FORBIDDEN, None) if session.current_env.as_ref().is_some_and(|env| !env.active) => {
            "This environment is inactive. Choose another environment or reactivate it.".to_string()
        }
        (StatusCode::NOT_FOUND, _) => {
            "That organization or environment doesn't exist, or you aren't a member of it.".to_string()
        }
        (StatusCode::BAD_REQUEST, _) => {
            "This request names a different workspace than the page it came from. Reload the page and try again.".to_string()
        }
        _ => "You don't have access to this page.".to_string(),
    };
    if is_htmx {
        // The reason names the organization, so it is escaped like any other text.
        let escaped = message
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;");
        let fragment = format!("<div class=\"error-box\" role=\"alert\">{escaped}</div>");
        return (status, axum::response::Html(fragment)).into_response();
    }
    if status == StatusCode::FORBIDDEN && session.current_org_hold.is_some() {
        return crate::handlers::error_page_with(
            session,
            status,
            "Workspace unavailable",
            message,
            "/account",
            "Go to your account",
        );
    }
    crate::handlers::error_page(session, status, message)
}

fn is_scope_selection_path(path: &str) -> bool {
    path.starts_with("/switch-org/")
        || path.starts_with("/switch-env/")
        || (path.starts_with("/envs/") && path.ends_with("/switch"))
}

/// Pages about the signed-in user rather than the selected org. An admin of
/// a suspended account can always reach them and switch away from it.
fn is_personal_path(path: &str) -> bool {
    is_scope_selection_path(path)
        || matches!(
            path,
            "/account" | "/account/notifications" | "/orgs/new" | "/claim-handle"
        )
}

fn is_recovery_path(path: &str, method: &axum::http::Method) -> bool {
    if is_personal_path(path) {
        return true;
    }
    let read = *method == axum::http::Method::GET || *method == axum::http::Method::HEAD;
    // A suspended account can still repair payment or choose a subscription.
    // The middleware and billing handlers both require root administration.
    if path.starts_with("/billing/")
        && (read || (path == "/billing/create-checkout" && *method == axum::http::Method::POST))
    {
        return true;
    }
    if read && (path == "/" || path == "/orgs" || path == "/account/billing") {
        return true;
    }
    // Revoking a credential only removes access.
    if *method == axum::http::Method::POST
        && path.starts_with("/service-keys/")
        && path.ends_with("/revoke")
    {
        return true;
    }
    let Some(tail) = path.strip_prefix("/@") else {
        return false;
    };
    let segments: Vec<_> = tail.split('/').collect();
    if read {
        return segments.len() == 1
            || matches!(segments.get(1), Some(&"children" | &"billing" | &"usage"));
    }
    segments.last() == Some(&"lifecycle")
        || (segments.get(1) == Some(&"org-api-keys") && segments.last() == Some(&"revoke"))
        || (segments.get(1) == Some(&"billing")
            && matches!(
                segments.last(),
                Some(&"cancel" | &"reactivate" | &"checkout")
            ))
}

fn is_org_control_path(path: &str) -> bool {
    path.starts_with("/@")
        || path == "/orgs"
        || path.starts_with("/orgs/")
        || path == "/envs"
        || path.starts_with("/envs/")
        || path == "/account"
        || path.starts_with("/account/")
        || path.starts_with("/settings/alerts")
        || path.starts_with("/billing/")
        || path == "/claim-handle"
}

/// Only attached to the known authentication/onboarding routers, never runtime routes.
/// Reject a state-changing request that a browser sent from another
/// origin. The session cookie is `SameSite=Lax`, which still accompanies a
/// form posted from a same-site page (another host under the same domain),
/// so the browser's fetch metadata decides: only `same-origin` requests may
/// change state. Without that header, a mismatched `Origin` is rejected. A
/// request with neither header didn't come from a browser page.
pub async fn reject_cross_origin_writes(request: Request, next: Next) -> Response {
    use axum::http::{Method, header};
    if matches!(
        *request.method(),
        Method::GET | Method::HEAD | Method::OPTIONS
    ) {
        return next.run(request).await;
    }
    let headers = request.headers();
    let text = |name: &str| headers.get(name).and_then(|value| value.to_str().ok());
    let cross_origin = match text("sec-fetch-site") {
        Some(site) => !matches!(site, "same-origin" | "none"),
        None => match headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) {
            None => false,
            Some(origin) => {
                let origin_host = origin.split_once("://").map_or(origin, |(_, host)| host);
                ![text("host"), text("x-forwarded-host")]
                    .into_iter()
                    .flatten()
                    .any(|host| origin_host.eq_ignore_ascii_case(host))
            }
        },
    };
    if cross_origin {
        tracing::warn!(
            method = %request.method(),
            path = %request.uri().path(),
            "Rejected a cross-origin request"
        );
        return (StatusCode::FORBIDDEN, "Cross-origin request rejected").into_response();
    }
    next.run(request).await
}

pub async fn onboarding_scope_middleware(request: Request, next: Next) -> Response {
    with_database_scope(
        DatabaseScope::Bootstrap(BootstrapOperation::Onboarding),
        next.run(request),
    )
    .await
}

/// Middleware to check if user is authenticated
pub async fn auth_middleware(
    cookies: CookieJar,
    request: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    // Check if we have a valid JWT token
    if get_user_id_from_cookies(&cookies).is_some() {
        // User is authenticated, continue to the handler
        Ok(next.run(request).await)
    } else {
        // User is not authenticated, redirect to signin with next param to return after login
        let next_url = build_signin_redirect(request.uri());
        Ok(create_redirect_response(&request, &next_url))
    }
}

/// Build signin redirect URL, preserving the original path as a `next` query param
/// so the user is returned to their intended destination after login.
fn build_signin_redirect(uri: &axum::http::Uri) -> String {
    let path = uri.path();

    // Skip `next` entirely for auth-related paths
    if path == "/" || path.starts_with("/signin") || path.starts_with("/signup") {
        return "/signin".to_string();
    }

    // HTMX widget and data endpoints are partial fragments, not full pages.
    // Resolve them to the parent page so users don't land on a bare fragment.
    let effective_path = if path.starts_with("/dashboard/widgets/") || path.starts_with("/data/") {
        "/dashboard"
    } else {
        path
    };

    // After resolving, skip `next` if we'd just be sending them to the default landing page
    if effective_path == "/" || effective_path == "/dashboard" {
        return "/signin".to_string();
    }

    let next_value = if let Some(query) = uri.query() {
        format!("{}?{}", effective_path, query)
    } else {
        effective_path.to_string()
    };
    format!("/signin?next={}", urlencoding::encode(&next_value))
}

/// Enhanced auth middleware that extracts session data and adds it to request extensions
/// In local development mode, automatically creates a session with the first available user
pub async fn session_middleware(
    State(db): axum::extract::State<std::sync::Arc<DatabasePool>>,
    axum::extract::Extension(conf): axum::extract::Extension<Val>,
    mut cookies: CookieJar,
    mut request: Request,
    next: Next,
) -> Result<impl IntoResponse, StatusCode> {
    let db = Arc::new(db.with_scope(DatabaseScope::Bootstrap(BootstrapOperation::Membership)));
    // Store whether this is an HTMX request for later use
    let is_htmx = is_htmx_request(&request);

    // Check if we have a valid JWT token
    if let Some(user_id) = get_user_id_from_cookies(&cookies) {
        // Create session from user_id and cookies
        match Session::from_user_id(&db, &conf, &user_id, &cookies).await {
            Ok(mut session) => {
                if let Err(status) = session.select_request_scope(&db, &request).await {
                    return Ok((cookies, scope_error_response(&session, status, is_htmx)));
                }
                // Check if we need to update cookies due to fallback
                let mut updated_cookies = cookies.clone();
                let mut cookies_changed = false;

                // Sliding session refresh: re-issue the JWT once it's past
                // half its life so active users never hit the 30-day expiry.
                if let Some(refreshed) = refresh_session_cookie(&cookies, &conf) {
                    updated_cookies = updated_cookies.add(refreshed);
                    cookies_changed = true;
                    tracing::debug!("Refreshed session JWT for user {}", user_id);
                }

                // Check if org_id in session differs from cookie (fallback occurred)
                if let Some(current_org) = &session.current_org {
                    let cookie_org_id = get_current_org_id_from_cookies(&cookies);
                    if cookie_org_id.as_deref() != Some(&current_org.org_id.to_string()) {
                        // Update org_id cookie to match the fallback value
                        updated_cookies = updated_cookies.add(build_cookie(
                            current_org_cookie_name(),
                            current_org.org_id.to_string(),
                            time::Duration::days(365),
                        ));
                        cookies_changed = true;
                        tracing::debug!(
                            "Updated org_id cookie to {} after fallback",
                            current_org.org_id
                        );
                    }
                }

                // Check if env_id in session differs from cookie (fallback occurred)
                if let Some(current_env) = &session.current_env {
                    let cookie_env_id = get_current_env_id_from_cookies(&cookies);
                    if cookie_env_id.as_deref() != Some(&current_env.env_id.to_string()) {
                        // Update env_id cookie to match the fallback value
                        updated_cookies = updated_cookies.add(build_cookie(
                            current_env_cookie_name(),
                            current_env.env_id.to_string(),
                            time::Duration::days(365),
                        ));
                        cookies_changed = true;
                        tracing::debug!(
                            "Updated env_id cookie to {} after fallback",
                            current_env.env_id
                        );
                    }
                }

                if let Some(path) = onboarding_redirect(&session, request.uri().path()) {
                    let response = create_redirect_response(&request, &path);
                    return Ok((updated_cookies, response));
                }

                let scope = session.database_scope(request.uri().path());
                // Add session to request extensions
                request.extensions_mut().insert(session);

                // Run the handler and get response
                let response = with_database_scope(scope, next.run(request)).await;

                // If cookies were updated, return response with updated cookies
                if cookies_changed {
                    return Ok((updated_cookies, response));
                }

                // User is authenticated, continue to the handler
                return Ok((cookies, response));
            }
            Err(error) => {
                // An unusable token is cleared; local development then falls
                // through to auto-login, otherwise the redirect below drops it.
                cookies = handle_session_error(error, cookies, &user_id)?;
            }
        }
    }

    // If not authenticated and in ordinary local development mode, auto-login
    // with the first user. Hot Cloud test configs should keep production auth behavior.
    if local_dev_conveniences() && !hot::product::billing_enabled(&conf) {
        // Try to get the first user from database
        match User::get_first_user(&db).await {
            Ok(user) => {
                // Generate JWT token for auto-login
                match generate_token(&user.user_id, &conf) {
                    Ok(token) => {
                        let cookies_with_jwt = cookies.clone().add(build_cookie(
                            jwt_cookie_name(),
                            token,
                            time::Duration::days(1),
                        ));

                        // Set default org/env cookies if not already set
                        let cookies_with_defaults = if get_current_org_id_from_cookies(
                            &cookies_with_jwt,
                        )
                        .is_none()
                        {
                            match crate::handlers::set_default_org_env_cookies(
                                &db,
                                &user.user_id,
                                cookies_with_jwt.clone(),
                            )
                            .await
                            {
                                Ok(updated_cookies) => {
                                    tracing::debug!("Set default org/env cookies for auto-login");
                                    updated_cookies
                                }
                                Err(e) => {
                                    tracing::warn!(
                                        "Failed to set default org/env cookies for auto-login: {}",
                                        e
                                    );
                                    cookies_with_jwt.clone()
                                }
                            }
                        } else {
                            cookies_with_jwt.clone()
                        };

                        // Create session with cookies that have org/env set
                        match Session::from_user_id(
                            &db,
                            &conf,
                            &user.user_id,
                            &cookies_with_defaults,
                        )
                        .await
                        {
                            Ok(mut session) => {
                                if let Err(status) =
                                    session.select_request_scope(&db, &request).await
                                {
                                    let response = scope_error_response(&session, status, is_htmx);
                                    return Ok((cookies_with_defaults, response));
                                }
                                if let Some(path) =
                                    onboarding_redirect(&session, request.uri().path())
                                {
                                    let response = create_redirect_response(&request, &path);
                                    return Ok((cookies_with_defaults, response));
                                }

                                let scope = session.database_scope(request.uri().path());
                                // Add session to request extensions
                                request.extensions_mut().insert(session);
                                tracing::debug!(
                                    "Auto-logged in user {} ({}) in local development mode",
                                    user.email,
                                    user.user_id
                                );

                                // Run the request and get the response
                                let response = with_database_scope(scope, next.run(request)).await;

                                // Return response with cookies
                                return Ok((cookies_with_defaults, response));
                            }
                            Err(e) => {
                                tracing::error!("Failed to create session for auto-login: {}", e);
                            }
                        }
                    }
                    Err(e) => {
                        tracing::error!("Failed to generate token for auto-login: {}", e);
                    }
                }
            }
            Err(e) => {
                tracing::warn!(
                    "No users found for auto-login in local development mode: {}. Please run 'hot init'.",
                    e
                );
            }
        }
    }

    // User is not authenticated, redirect to signin with next param
    let signin_url = build_signin_redirect(request.uri());
    let redirect_response = if is_htmx {
        Response::builder()
            .status(StatusCode::OK)
            .header("HX-Redirect", &signin_url)
            .body(axum::body::Body::empty())
            .unwrap()
    } else {
        Redirect::to(&signin_url).into_response()
    };

    Ok((cookies, redirect_response))
}

/// Middleware to redirect authenticated users away from auth pages
pub async fn guest_only_middleware(
    cookies: CookieJar,
    request: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    // Check if user is already authenticated
    if get_user_id_from_cookies(&cookies).is_some() {
        // User is authenticated - check if they're trying to access signup with plan params
        // If so, redirect to checkout flow instead of dashboard
        let uri = request.uri();
        if uri.path() == "/signup"
            && let Some(query) = uri.query()
        {
            // Parse query string for plan and billing params
            let params: AHashMap<String, String> = url::form_urlencoded::parse(query.as_bytes())
                .into_owned()
                .collect();

            if let Some(plan) = params.get("plan") {
                let billing = params
                    .get("billing")
                    .map(|s| s.as_str())
                    .unwrap_or("monthly");
                // Redirect to checkout form with plan params (will use session's current org)
                return Ok(Redirect::to(&format!(
                    "/billing/create-checkout-form?plan={}&billing={}",
                    plan, billing
                ))
                .into_response());
            }
        }

        // Default: redirect to dashboard
        Ok(Redirect::to("/").into_response())
    } else {
        // User is not authenticated, continue to the handler (signin page)
        Ok(next.run(request).await)
    }
}

#[cfg(test)]
mod scope_contract_tests {
    #[test]
    fn only_loopback_hosts_keep_local_dev_conveniences_without_hot_env() {
        use super::is_loopback_host;
        for host in [
            "localhost",
            "LOCALHOST",
            "127.0.0.1",
            "127.1.2.3",
            "::1",
            "[::1]",
        ] {
            assert!(is_loopback_host(host), "{host}");
        }
        for host in ["0.0.0.0", "::", "[::]", "10.0.0.5", "app.example.com", ""] {
            assert!(!is_loopback_host(host), "{host}");
        }
    }

    #[test]
    fn session_cookies_take_the_host_prefix_outside_local_dev() {
        use super::{build_cookie, cookie_name, jwt_cookie_name};
        assert_eq!(
            cookie_name("hot_auth_token", "__Host-hot_auth_token", false),
            "__Host-hot_auth_token"
        );
        assert_eq!(
            cookie_name("hot_auth_token", "__Host-hot_auth_token", true),
            "hot_auth_token"
        );
        // Tests run as local development, over plain HTTP: bare names.
        assert_eq!(jwt_cookie_name(), "hot_auth_token");
        let cookie = build_cookie(jwt_cookie_name(), String::new(), time::Duration::days(1));
        assert_ne!(cookie.secure(), Some(true));
    }

    #[tokio::test]
    async fn cross_origin_writes_are_rejected() {
        use axum::body::Body;
        use axum::http::Request as HttpRequest;
        use tower::ServiceExt;

        let app = axum::Router::new()
            .route(
                "/write",
                axum::routing::post(|| async { "ok" }).get(|| async { "read" }),
            )
            .layer(axum::middleware::from_fn(reject_cross_origin_writes));
        let status = |method: &str, headers: &[(&str, &str)]| {
            let app = app.clone();
            let mut request = HttpRequest::builder()
                .method(method)
                .uri("/write")
                .header("host", "app.example.com");
            for (name, value) in headers {
                request = request.header(*name, *value);
            }
            async move {
                app.oneshot(request.body(Body::empty()).unwrap())
                    .await
                    .unwrap()
                    .status()
            }
        };

        assert_eq!(
            status("POST", &[("sec-fetch-site", "same-origin")]).await,
            StatusCode::OK
        );
        assert_eq!(
            status("POST", &[("sec-fetch-site", "same-site")]).await,
            StatusCode::FORBIDDEN,
            "a page on another host of the same site"
        );
        assert_eq!(
            status("POST", &[("sec-fetch-site", "cross-site")]).await,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            status("POST", &[("origin", "https://api.example.com")]).await,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            status("POST", &[("origin", "https://app.example.com")]).await,
            StatusCode::OK
        );
        assert_eq!(
            status("POST", &[]).await,
            StatusCode::OK,
            "not from a browser page"
        );
        assert_eq!(
            status("GET", &[("sec-fetch-site", "cross-site")]).await,
            StatusCode::OK,
            "reads pass"
        );
    }

    use super::*;
    use hot::db::org::{CreateChild, Org, OrgLifecycle};

    async fn fixture() -> (DatabasePool, Session, Org, Uuid) {
        let db = hot::db::test_db().await;
        let data = hot::db::insert_test_data(&db).await.unwrap();
        if let DatabasePool::Sqlite(pool) = &db {
            sqlx::query("UPDATE org SET org_type = 'organization' WHERE org_id = ?")
                .bind(data.org_id)
                .execute(pool)
                .await
                .unwrap();
        }
        let child = Org::create_child(
            &db,
            &data.org_id,
            &data.user_id,
            &CreateChild {
                name: "Customer".into(),
                slug: "scope-child".into(),
                external_id: None,
                idempotency_key: "scope-child".into(),
                hosted: false,
            },
        )
        .await
        .unwrap();
        let cookies = CookieJar::new().add(build_cookie(
            current_org_cookie_name(),
            data.org_id.to_string(),
            time::Duration::days(1),
        ));
        let session = Session::from_user_id(&db, &hot::val!({}), &data.user_id, &cookies)
            .await
            .unwrap();
        (db, session, child.org, child.env_id)
    }

    fn request(method: &str, uri: &str) -> Request {
        Request::builder()
            .method(method)
            .uri(uri)
            .body(axum::body::Body::empty())
            .unwrap()
    }

    #[tokio::test]
    async fn only_unusable_sessions_clear_cookies() {
        let db = hot::db::test_db().await;
        let data = hot::db::insert_test_data(&db).await.unwrap();
        let conf = hot::val!({});
        let jar = CookieJar::new();
        assert!(matches!(
            Session::from_user_id(&db, &conf, &Uuid::now_v7(), &jar).await,
            Err(SessionError::Unusable(_))
        ));
        if let DatabasePool::Sqlite(pool) = &db {
            sqlx::query("UPDATE user SET active = 0 WHERE user_id = ?")
                .bind(data.user_id)
                .execute(pool)
                .await
                .unwrap();
        }
        let inactive = Session::from_user_id(&db, &conf, &data.user_id, &jar)
            .await
            .unwrap_err();
        assert!(matches!(inactive, SessionError::Unusable(_)));
        let cleared = handle_session_error(inactive, jar.clone(), &data.user_id).unwrap();
        assert!(
            cleared.get(jwt_cookie_name()).is_some(),
            "removal cookie set"
        );

        // A database failure says nothing about the token: keep the cookies.
        if let DatabasePool::Sqlite(pool) = &db {
            pool.close().await;
        }
        let unavailable = Session::from_user_id(&db, &conf, &data.user_id, &jar)
            .await
            .unwrap_err();
        assert!(matches!(unavailable, SessionError::Unavailable(_)));
        assert_eq!(
            handle_session_error(unavailable, jar, &data.user_id).unwrap_err(),
            StatusCode::SERVICE_UNAVAILABLE
        );
    }

    #[tokio::test]
    async fn explicit_page_scope_wins_over_cookie_and_rejects_wrong_env() {
        let (db, mut session, child, env_id) = fixture().await;
        let root_id = session.current_org_id().unwrap();
        session
            .select_request_scope(
                &db,
                &request(
                    "POST",
                    &format!("/keys?org_id={}&env_id={env_id}", child.org_id),
                ),
            )
            .await
            .unwrap();
        assert_eq!(session.current_org_id(), Some(child.org_id));
        assert_eq!(session.current_env_id(), Some(env_id));
        assert_eq!(
            session.database_scope("/keys"),
            DatabaseScope::Environment {
                org_id: child.org_id,
                env_id
            }
        );
        assert_eq!(
            session
                .select_request_scope(
                    &db,
                    &request("POST", &format!("/keys?org_id={root_id}&env_id={env_id}"))
                )
                .await,
            Err(StatusCode::NOT_FOUND)
        );
    }

    #[tokio::test]
    async fn invalid_or_missing_mutation_scope_never_falls_back() {
        let (db, mut session, child, _) = fixture().await;
        assert_eq!(
            session
                .select_request_scope(&db, &request("POST", "/keys"))
                .await,
            Err(StatusCode::BAD_REQUEST)
        );
        assert_eq!(
            session
                .select_request_scope(&db, &request("GET", "/runs?org_id=invalid"))
                .await,
            Err(StatusCode::BAD_REQUEST)
        );
        assert_eq!(
            session
                .select_request_scope(
                    &db,
                    &request("GET", &format!("/runs?org_id={}", Uuid::new_v4()))
                )
                .await,
            Err(StatusCode::NOT_FOUND)
        );
        session
            .select_request_scope(&db, &request("GET", &format!("/@{}", child.slug)))
            .await
            .unwrap();
        assert_eq!(session.current_org_id(), Some(child.org_id));
        let conflicting = Request::builder()
            .uri(format!("/runs?org_id={}", child.org_id))
            .header("x-hot-org-id", Uuid::new_v4().to_string())
            .body(axum::body::Body::empty())
            .unwrap();
        assert_eq!(
            session.select_request_scope(&db, &conflicting).await,
            Err(StatusCode::BAD_REQUEST)
        );
    }

    #[tokio::test]
    async fn suspended_child_allows_only_admin_recovery() {
        let (db, mut session, child, env_id) = fixture().await;
        Org::set_lifecycle(
            &db,
            &child.org_id,
            &session.current_user_id(),
            OrgLifecycle::Suspended,
        )
        .await
        .unwrap();
        let url = format!("/runs?org_id={}&env_id={env_id}", child.org_id);
        assert_eq!(
            session
                .select_request_scope(&db, &request("GET", &url))
                .await,
            Err(StatusCode::FORBIDDEN)
        );
        assert!(is_recovery_path(
            &format!("/@{}/lifecycle", child.slug),
            &axum::http::Method::POST
        ));
        assert!(!is_recovery_path(
            &format!("/@{}/users/invite", child.slug),
            &axum::http::Method::POST
        ));
        assert!(!is_recovery_path("/envs/new", &axum::http::Method::POST));
        assert!(is_recovery_path(
            "/billing/create-checkout-form",
            &axum::http::Method::GET
        ));
        assert!(is_recovery_path(
            "/billing/create-checkout",
            &axum::http::Method::POST
        ));
        assert!(is_recovery_path(
            &format!("/@{}/billing/checkout", child.slug),
            &axum::http::Method::POST
        ));
        assert!(is_recovery_path(
            &format!("/service-keys/{}/revoke", Uuid::now_v7()),
            &axum::http::Method::POST
        ));
        assert!(!is_recovery_path(
            "/service-keys/new",
            &axum::http::Method::POST
        ));
        assert!(is_recovery_path("/", &axum::http::Method::GET));
    }

    /// Only root admins keep access to a suspended account (other members
    /// lose it entirely), so the recovery list is what stands between an
    /// admin with a stale org cookie and a 403 on every page.
    #[tokio::test]
    async fn suspended_account_keeps_personal_pages_and_dashboard_for_admins() {
        let (db, mut admin, _child, _env_id) = fixture().await;
        let root = admin.current_org.clone().unwrap();
        Org::set_lifecycle(
            &db,
            &root.org_id,
            &admin.current_user_id(),
            OrgLifecycle::Suspended,
        )
        .await
        .unwrap();
        for path in ["/", "/account", "/account/notifications", "/orgs/new"] {
            assert_eq!(
                admin.select_request_scope(&db, &request("GET", path)).await,
                Ok(()),
                "{path}"
            );
        }
        assert_eq!(
            admin
                .select_request_scope(&db, &request("GET", "/runs"))
                .await,
            Err(StatusCode::FORBIDDEN)
        );
        assert!(is_personal_path(&format!("/switch-org/{}", root.org_id)));
    }
}

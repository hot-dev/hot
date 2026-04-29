use ahash::AHashMap;
use axum::{
    extract::{Request, State},
    http::StatusCode,
    middleware::Next,
    response::{IntoResponse, Redirect, Response},
};
use axum_extra::extract::CookieJar;
use chrono::{Duration, Utc};
use hot::db::{DatabasePool, User};
use hot::stream::StreamPubSub;
use hot::val::Val;
use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, Validation, decode, encode};
use serde::{Deserialize, Serialize};
use std::env;
use std::sync::Arc;
use tokio::sync::watch;
use uuid::Uuid;

// App state that holds the database pool and configuration
#[derive(Clone)]
pub struct AppState {
    pub db: Arc<DatabasePool>,
    pub conf: Val,
    pub stream_pubsub: Option<Arc<StreamPubSub>>,
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
            shutdown_rx,
        }
    }

    pub fn with_stream_pubsub(mut self, pubsub: Option<Arc<StreamPubSub>>) -> Self {
        self.stream_pubsub = pubsub;
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
}

impl Session {
    /// Create a new session from user_id and cookies
    pub async fn from_user_id(
        db: &DatabasePool,
        conf: &Val,
        user_id: &Uuid,
        cookies: &CookieJar,
    ) -> Result<Self, String> {
        // Get user details
        let user = User::get_user(db, user_id)
            .await
            .map_err(|e| format!("Failed to get user: {}", e))?;

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
            .unwrap_or_default();

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
            match hot::db::org::OrgUser::get_org_user(db, &org.org_id, user_id).await {
                Ok(org_user) => org_user.org_user_role_id == 2, // 2 = admin role
                Err(_) => false,
            }
        } else {
            false
        };

        // Get environments for current organization
        let current_org_envs = if let Some(ref org) = current_org {
            hot::db::env::Env::get_envs_by_org(db, &org.org_id)
                .await
                .unwrap_or_default()
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
                    match hot::db::OrgPlan::get_by_org_id(db, &org.org_id).await {
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
        match hot::db::org::OrgUser::get_org_user(db, org_id, &self.user.user_id).await {
            Ok(org_user) => org_user.org_user_role_id == 2, // 2 = admin role
            Err(_) => false,
        }
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

// Cookie name for the JWT token
pub const JWT_COOKIE_NAME: &str = "hot_auth_token";

// Cookie name for the current organization
pub const CURRENT_ORG_COOKIE_NAME: &str = "hot_current_org";

// Cookie name for the current environment
pub const CURRENT_ENV_COOKIE_NAME: &str = "hot_current_env";

/// Get session secret from environment variable
/// In development mode, falls back to a default secret for convenience
fn get_session_secret() -> Result<String, String> {
    match env::var("HOT_APP_SESSION_SECRET") {
        Ok(secret) if !secret.is_empty() => Ok(secret),
        _ => {
            // In local development, use a fallback secret for convenience
            if hot::env::is_local_dev() {
                Ok("hotdev-session-secret-key-change-in-production".to_string())
            } else {
                Err(
                    "HOT_APP_SESSION_SECRET environment variable is required in production"
                        .to_string(),
                )
            }
        }
    }
}

/// Get session timeout in hours from conf, with default of 24 hours
fn get_session_timeout_hours(conf: &Val) -> i64 {
    conf.get("app")
        .and_then(|app| app.get("session"))
        .and_then(|session| session.get("timeout"))
        .and_then(|timeout| match timeout {
            Val::Int(i) => Some(i),
            _ => None,
        })
        .unwrap_or(24) // Default to 24 hours
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
        .get(JWT_COOKIE_NAME)
        .and_then(|cookie| validate_token(cookie.value()).ok())
}

/// Extract current_org_id from cookie in request
pub fn get_current_org_id_from_cookies(cookies: &CookieJar) -> Option<String> {
    cookies
        .get(CURRENT_ORG_COOKIE_NAME)
        .map(|cookie| cookie.value().to_string())
}

/// Extract current_env_id from cookie in request
pub fn get_current_env_id_from_cookies(cookies: &CookieJar) -> Option<String> {
    cookies
        .get(CURRENT_ENV_COOKIE_NAME)
        .map(|cookie| cookie.value().to_string())
}

/// Extract session from request extensions
pub fn get_session_from_request(request: &Request) -> Option<&Session> {
    request.extensions().get::<Session>()
}

/// Check if request is from HTMX by looking for HX-Request header
fn is_htmx_request(request: &Request) -> bool {
    request
        .headers()
        .get("HX-Request")
        .and_then(|v| v.to_str().ok())
        .map(|v| v == "true")
        .unwrap_or(false)
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
            Some(format!("/@{}/billing/checkout", org.slug))
        }
        None if session.user_orgs.is_empty() => Some("/claim-handle".to_string()),
        _ => None,
    }
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
    cookies: CookieJar,
    mut request: Request,
    next: Next,
) -> Result<impl IntoResponse, StatusCode> {
    // Store whether this is an HTMX request for later use
    let is_htmx = is_htmx_request(&request);

    // Check if we have a valid JWT token
    if let Some(user_id) = get_user_id_from_cookies(&cookies) {
        // Create session from user_id and cookies
        match Session::from_user_id(&db, &conf, &user_id, &cookies).await {
            Ok(session) => {
                // Check if we need to update cookies due to fallback
                let mut updated_cookies = cookies.clone();
                let mut cookies_changed = false;

                // Check if org_id in session differs from cookie (fallback occurred)
                if let Some(current_org) = &session.current_org {
                    let cookie_org_id = get_current_org_id_from_cookies(&cookies);
                    if cookie_org_id.as_deref() != Some(&current_org.org_id.to_string()) {
                        // Update org_id cookie to match the fallback value
                        let mut org_cookie = axum_extra::extract::cookie::Cookie::new(
                            CURRENT_ORG_COOKIE_NAME,
                            current_org.org_id.to_string(),
                        );
                        org_cookie.set_path("/");
                        org_cookie.set_max_age(time::Duration::days(365));
                        org_cookie.set_http_only(true);
                        org_cookie.set_secure(!hot::env::is_local_dev());
                        org_cookie.set_same_site(axum_extra::extract::cookie::SameSite::Lax);
                        updated_cookies = updated_cookies.add(org_cookie);
                        cookies_changed = true;
                        tracing::info!(
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
                        let mut env_cookie = axum_extra::extract::cookie::Cookie::new(
                            CURRENT_ENV_COOKIE_NAME,
                            current_env.env_id.to_string(),
                        );
                        env_cookie.set_path("/");
                        env_cookie.set_max_age(time::Duration::days(365));
                        env_cookie.set_http_only(true);
                        env_cookie.set_secure(!hot::env::is_local_dev());
                        env_cookie.set_same_site(axum_extra::extract::cookie::SameSite::Lax);
                        updated_cookies = updated_cookies.add(env_cookie);
                        cookies_changed = true;
                        tracing::info!(
                            "Updated env_id cookie to {} after fallback",
                            current_env.env_id
                        );
                    }
                }

                if let Some(path) = onboarding_redirect(&session, request.uri().path()) {
                    let response = create_redirect_response(&request, &path);
                    return Ok((updated_cookies, response));
                }

                // Add session to request extensions
                request.extensions_mut().insert(session);

                // Run the handler and get response
                let response = next.run(request).await;

                // If cookies were updated, return response with updated cookies
                if cookies_changed {
                    return Ok((updated_cookies, response));
                }

                // User is authenticated, continue to the handler
                return Ok((cookies, response));
            }
            Err(_) => {
                // Failed to create session - fall through to local dev auto-login check
            }
        }
    }

    // If not authenticated and in ordinary local development mode, auto-login
    // with the first user. Hot Cloud test configs should keep production auth behavior.
    if hot::env::is_local_dev() && !hot::product::billing_enabled(&conf) {
        // Try to get the first user from database
        match User::get_first_user(&db).await {
            Ok(user) => {
                // Generate JWT token for auto-login
                match generate_token(&user.user_id, &conf) {
                    Ok(token) => {
                        // Create JWT cookie
                        let mut jwt_cookie =
                            axum_extra::extract::cookie::Cookie::new(JWT_COOKIE_NAME, token);
                        jwt_cookie.set_path("/");
                        jwt_cookie.set_max_age(time::Duration::days(1));
                        jwt_cookie.set_http_only(true);
                        jwt_cookie.set_secure(!hot::env::is_local_dev());
                        jwt_cookie.set_same_site(axum_extra::extract::cookie::SameSite::Lax);

                        let cookies_with_jwt = cookies.clone().add(jwt_cookie);

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
                                    tracing::info!("Set default org/env cookies for auto-login");
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
                            Ok(session) => {
                                if let Some(path) =
                                    onboarding_redirect(&session, request.uri().path())
                                {
                                    let response = create_redirect_response(&request, &path);
                                    return Ok((cookies_with_defaults, response));
                                }

                                // Add session to request extensions
                                request.extensions_mut().insert(session);
                                tracing::debug!(
                                    "Auto-logged in user {} ({}) in local development mode",
                                    user.email,
                                    user.user_id
                                );

                                // Run the request and get the response
                                let response = next.run(request).await;

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

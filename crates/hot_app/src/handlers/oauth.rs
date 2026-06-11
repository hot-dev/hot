use crate::auth::{JWT_COOKIE_NAME, generate_token};
use crate::handlers::{add_presence_cookie, process_invite_code};
use crate::oauth::{
    OAuthConfig, create_github_client, create_google_client, exchange_code_for_token,
    fetch_github_user_info, fetch_google_user_info, get_github_auth_url, get_google_auth_url,
};
use axum::extract::Extension;
use axum::extract::{Query, State};
use axum::response::{Html, Redirect};
use axum_extra::extract::CookieJar;
use hot::db::{DatabasePool, User, UserAuth};
use hot::val::Val;
use oauth2::AuthorizationCode;
use serde::Deserialize;
use std::sync::Arc;
use uuid::Uuid;

const OAUTH_STATE_COOKIE_NAME: &str = "hot_oauth_state";
const OAUTH_INVITE_COOKIE_NAME: &str = "hot_oauth_invite";
const OAUTH_NEXT_COOKIE_NAME: &str = "hot_oauth_next";
const OAUTH_PLAN_COOKIE_NAME: &str = "hot_oauth_plan";
const OAUTH_BILLING_COOKIE_NAME: &str = "hot_oauth_billing";

/// Query parameters for OAuth initiation
#[derive(Deserialize, Debug)]
pub struct OAuthInitQuery {
    pub invite_code: Option<String>,
    pub next: Option<String>,
    pub plan: Option<String>,
    pub billing: Option<String>,
}

/// Query parameters for OAuth callback
#[derive(Deserialize, Debug)]
pub struct OAuthCallbackQuery {
    pub code: String,
    pub state: String,
}

/// Google OAuth initiation handler
pub async fn google_auth_handler(
    Query(params): Query<OAuthInitQuery>,
    cookies: CookieJar,
) -> Result<(CookieJar, Redirect), Html<String>> {
    let config = OAuthConfig::from_env();

    let google_config = match config.google {
        Some(cfg) => cfg,
        None => {
            return Err(Html(
                "Google OAuth is not configured. Please contact support.".to_string(),
            ));
        }
    };

    let redirect_url = format!("{}/auth/google/callback", config.redirect_base_url);

    let client = create_google_client(&google_config, &redirect_url).map_err(|e| {
        tracing::error!("Failed to create Google OAuth client: {}", e);
        Html("Failed to initialize Google OAuth".to_string())
    })?;

    let (auth_url, csrf_token) = get_google_auth_url(&client, params.invite_code.as_deref())
        .map_err(|e| {
            tracing::error!("Failed to generate Google auth URL: {}", e);
            Html("Failed to generate Google authorization URL".to_string())
        })?;

    // Store CSRF token in secure cookie
    let mut updated_cookies = cookies.add(crate::auth::build_cookie(
        OAUTH_STATE_COOKIE_NAME,
        csrf_token.secret().clone(),
        time::Duration::minutes(10),
    ));

    // Store invite code in cookie if provided
    if let Some(invite_code) = params.invite_code {
        updated_cookies = updated_cookies.add(crate::auth::build_cookie(
            OAUTH_INVITE_COOKIE_NAME,
            invite_code,
            time::Duration::minutes(10),
        ));
    }

    // Store next URL in cookie if provided (for post-login redirect)
    if let Some(next) = params.next.filter(|n| crate::auth::is_safe_next(n)) {
        updated_cookies = updated_cookies.add(crate::auth::build_cookie(
            OAUTH_NEXT_COOKIE_NAME,
            next,
            time::Duration::minutes(10),
        ));
    }

    // Store plan and billing in cookies if provided (for post-signup plan flow)
    if let Some(plan) = params.plan.filter(|p| !p.is_empty()) {
        updated_cookies = updated_cookies.add(crate::auth::build_cookie(
            OAUTH_PLAN_COOKIE_NAME,
            plan,
            time::Duration::minutes(10),
        ));
    }

    if let Some(billing) = params.billing.filter(|b| !b.is_empty()) {
        updated_cookies = updated_cookies.add(crate::auth::build_cookie(
            OAUTH_BILLING_COOKIE_NAME,
            billing,
            time::Duration::minutes(10),
        ));
    }

    Ok((updated_cookies, Redirect::to(auth_url.as_str())))
}

/// Google OAuth callback handler
pub async fn google_callback_handler(
    Query(params): Query<OAuthCallbackQuery>,
    State(db): State<Arc<DatabasePool>>,
    Extension(conf): Extension<Val>,
    cookies: CookieJar,
) -> Result<(CookieJar, Redirect), Html<String>> {
    // Verify CSRF state token
    let stored_state = cookies
        .get(OAUTH_STATE_COOKIE_NAME)
        .map(|c| c.value().to_string())
        .ok_or_else(|| Html("Invalid OAuth state (missing cookie)".to_string()))?;

    if stored_state != params.state {
        tracing::warn!("OAuth state mismatch");
        return Err(Html("Invalid OAuth state (mismatch)".to_string()));
    }

    let config = OAuthConfig::from_env();
    let google_config = config
        .google
        .ok_or_else(|| Html("Google OAuth is not configured".to_string()))?;

    let redirect_url = format!("{}/auth/google/callback", config.redirect_base_url);
    let client = create_google_client(&google_config, &redirect_url)
        .map_err(|e| Html(format!("Failed to create OAuth client: {}", e)))?;

    // Exchange authorization code for access token
    let access_token = exchange_code_for_token(&client, AuthorizationCode::new(params.code))
        .await
        .map_err(|e| {
            tracing::error!("Failed to exchange code for token: {}", e);
            Html("Failed to authenticate with Google".to_string())
        })?;

    // Fetch user info from Google
    let user_info = fetch_google_user_info(&access_token).await.map_err(|e| {
        tracing::error!("Failed to fetch Google user info: {}", e);
        Html("Failed to fetch user information from Google".to_string())
    })?;

    // Verify email is verified
    if !user_info.verified_email {
        return Err(Html(
            "Your Google email is not verified. Please verify your email and try again."
                .to_string(),
        ));
    }

    // Get invite code from cookie if present
    let invite_code = cookies
        .get(OAUTH_INVITE_COOKIE_NAME)
        .map(|c| c.value().to_string());

    // Handle user creation or login
    let (user, is_new_user) = handle_oauth_user(
        &db,
        &user_info.email,
        "google",
        &user_info.id,
        user_info.name.as_deref(),
    )
    .await
    .map_err(|e| Html(format!("Authentication failed: {}", e)))?;

    // Process invite code if provided
    if let Some(invite_code) = invite_code.as_ref()
        && !invite_code.is_empty()
    {
        let _ = process_invite_code(&db, &user.user_id, invite_code).await;
    }

    // Generate JWT token
    let token = generate_token(&user.user_id, &conf)
        .map_err(|e| Html(format!("Failed to generate authentication token: {}", e)))?;

    // Set JWT cookie
    let jwt_cookie = crate::auth::build_cookie(
        JWT_COOKIE_NAME,
        token,
        time::Duration::days(crate::auth::SESSION_COOKIE_DAYS),
    );

    // Read cookies before clearing
    let next_url = cookies
        .get(OAUTH_NEXT_COOKIE_NAME)
        .map(|c| c.value().to_string())
        .filter(|n| crate::auth::is_safe_next(n));

    let oauth_plan = cookies
        .get(OAUTH_PLAN_COOKIE_NAME)
        .map(|c| c.value().to_string())
        .filter(|p| !p.is_empty());

    let oauth_billing = cookies
        .get(OAUTH_BILLING_COOKIE_NAME)
        .map(|c| c.value().to_string())
        .filter(|b| !b.is_empty());

    // Clear all OAuth cookies
    let updated_cookies = clear_oauth_cookies(cookies.add(jwt_cookie));

    // Add cross-subdomain presence cookie
    let final_cookies = add_presence_cookie(updated_cookies);

    // Determine redirect: new users go to claim-handle, existing users go to dashboard/plan
    let redirect_to = determine_oauth_redirect(
        next_url.as_deref(),
        oauth_plan.as_deref(),
        oauth_billing.as_deref(),
        is_new_user,
        &db,
        &user,
    )
    .await;

    Ok((final_cookies, Redirect::to(&redirect_to)))
}

/// GitHub OAuth initiation handler
pub async fn github_auth_handler(
    Query(params): Query<OAuthInitQuery>,
    cookies: CookieJar,
) -> Result<(CookieJar, Redirect), Html<String>> {
    let config = OAuthConfig::from_env();

    let github_config = match config.github {
        Some(cfg) => cfg,
        None => {
            return Err(Html(
                "GitHub OAuth is not configured. Please contact support.".to_string(),
            ));
        }
    };

    let redirect_url = format!("{}/auth/github/callback", config.redirect_base_url);

    let client = create_github_client(&github_config, &redirect_url).map_err(|e| {
        tracing::error!("Failed to create GitHub OAuth client: {}", e);
        Html("Failed to initialize GitHub OAuth".to_string())
    })?;

    let (auth_url, csrf_token) = get_github_auth_url(&client, params.invite_code.as_deref())
        .map_err(|e| {
            tracing::error!("Failed to generate GitHub auth URL: {}", e);
            Html("Failed to generate GitHub authorization URL".to_string())
        })?;

    // Store CSRF token in secure cookie
    let mut updated_cookies = cookies.add(crate::auth::build_cookie(
        OAUTH_STATE_COOKIE_NAME,
        csrf_token.secret().clone(),
        time::Duration::minutes(10),
    ));

    // Store invite code in cookie if provided
    if let Some(invite_code) = params.invite_code {
        updated_cookies = updated_cookies.add(crate::auth::build_cookie(
            OAUTH_INVITE_COOKIE_NAME,
            invite_code,
            time::Duration::minutes(10),
        ));
    }

    // Store next URL in cookie if provided (for post-login redirect)
    if let Some(next) = params.next.filter(|n| crate::auth::is_safe_next(n)) {
        updated_cookies = updated_cookies.add(crate::auth::build_cookie(
            OAUTH_NEXT_COOKIE_NAME,
            next,
            time::Duration::minutes(10),
        ));
    }

    // Store plan and billing in cookies if provided (for post-signup plan flow)
    if let Some(plan) = params.plan.filter(|p| !p.is_empty()) {
        updated_cookies = updated_cookies.add(crate::auth::build_cookie(
            OAUTH_PLAN_COOKIE_NAME,
            plan,
            time::Duration::minutes(10),
        ));
    }

    if let Some(billing) = params.billing.filter(|b| !b.is_empty()) {
        updated_cookies = updated_cookies.add(crate::auth::build_cookie(
            OAUTH_BILLING_COOKIE_NAME,
            billing,
            time::Duration::minutes(10),
        ));
    }

    Ok((updated_cookies, Redirect::to(auth_url.as_str())))
}

/// GitHub OAuth callback handler
pub async fn github_callback_handler(
    Query(params): Query<OAuthCallbackQuery>,
    State(db): State<Arc<DatabasePool>>,
    Extension(conf): Extension<Val>,
    cookies: CookieJar,
) -> Result<(CookieJar, Redirect), Html<String>> {
    // Verify CSRF state token
    let stored_state = cookies
        .get(OAUTH_STATE_COOKIE_NAME)
        .map(|c| c.value().to_string())
        .ok_or_else(|| Html("Invalid OAuth state (missing cookie)".to_string()))?;

    if stored_state != params.state {
        tracing::warn!("OAuth state mismatch");
        return Err(Html("Invalid OAuth state (mismatch)".to_string()));
    }

    let config = OAuthConfig::from_env();
    let github_config = config
        .github
        .ok_or_else(|| Html("GitHub OAuth is not configured".to_string()))?;

    let redirect_url = format!("{}/auth/github/callback", config.redirect_base_url);
    let client = create_github_client(&github_config, &redirect_url)
        .map_err(|e| Html(format!("Failed to create OAuth client: {}", e)))?;

    // Exchange authorization code for access token
    let access_token = exchange_code_for_token(&client, AuthorizationCode::new(params.code))
        .await
        .map_err(|e| {
            tracing::error!("Failed to exchange code for token: {}", e);
            Html("Failed to authenticate with GitHub".to_string())
        })?;

    // Fetch user info from GitHub
    let user_info = fetch_github_user_info(&access_token).await.map_err(|e| {
        tracing::error!("Failed to fetch GitHub user info: {}", e);
        Html("Failed to fetch user information from GitHub".to_string())
    })?;

    // Require a VERIFIED email from GitHub (resolved via /user/emails)
    let email = user_info.email.ok_or_else(|| {
        Html(
            "Your GitHub account has no verified email address. Please verify an email on GitHub and try again."
                .to_string(),
        )
    })?;

    // Get invite code from cookie if present
    let invite_code = cookies
        .get(OAUTH_INVITE_COOKIE_NAME)
        .map(|c| c.value().to_string());

    // Handle user creation or login
    let (user, is_new_user) = handle_oauth_user(
        &db,
        &email,
        "github",
        &user_info.id.to_string(),
        user_info.name.as_deref(),
    )
    .await
    .map_err(|e| Html(format!("Authentication failed: {}", e)))?;

    // Process invite code if provided
    if let Some(invite_code) = invite_code.as_ref()
        && !invite_code.is_empty()
    {
        let _ = process_invite_code(&db, &user.user_id, invite_code).await;
    }

    // Generate JWT token
    let token = generate_token(&user.user_id, &conf)
        .map_err(|e| Html(format!("Failed to generate authentication token: {}", e)))?;

    // Set JWT cookie
    let jwt_cookie = crate::auth::build_cookie(
        JWT_COOKIE_NAME,
        token,
        time::Duration::days(crate::auth::SESSION_COOKIE_DAYS),
    );

    // Read cookies before clearing
    let next_url = cookies
        .get(OAUTH_NEXT_COOKIE_NAME)
        .map(|c| c.value().to_string())
        .filter(|n| crate::auth::is_safe_next(n));

    let oauth_plan = cookies
        .get(OAUTH_PLAN_COOKIE_NAME)
        .map(|c| c.value().to_string())
        .filter(|p| !p.is_empty());

    let oauth_billing = cookies
        .get(OAUTH_BILLING_COOKIE_NAME)
        .map(|c| c.value().to_string())
        .filter(|b| !b.is_empty());

    // Clear all OAuth cookies
    let updated_cookies = clear_oauth_cookies(cookies.add(jwt_cookie));

    // Add cross-subdomain presence cookie
    let final_cookies = add_presence_cookie(updated_cookies);

    // Determine redirect: new users go to claim-handle, existing users go to dashboard/plan
    let redirect_to = determine_oauth_redirect(
        next_url.as_deref(),
        oauth_plan.as_deref(),
        oauth_billing.as_deref(),
        is_new_user,
        &db,
        &user,
    )
    .await;

    Ok((final_cookies, Redirect::to(&redirect_to)))
}

/// Handle OAuth user creation or login
/// Returns (User, is_new_user)
async fn handle_oauth_user(
    db: &DatabasePool,
    email: &str,
    provider: &str, // "google" or "github"
    provider_user_id: &str,
    name: Option<&str>,
) -> Result<(User, bool), String> {
    // Resolution order:
    //   1. provider_user_id — the stable key. Survives the user changing
    //      their email at the provider and changes to our email-resolution
    //      policy (e.g. verified-primary vs public-profile email).
    //   2. email match — links the provider to an existing account
    //      (first OAuth login for an email/password user).
    //   3. create a new user.
    if let Ok(auth) = UserAuth::get_by_provider_user_id(db, provider, provider_user_id).await {
        let user = User::get_user(db, &auth.user_id)
            .await
            .map_err(|e| format!("Failed to load user for OAuth login: {}", e))?;
        tracing::info!(
            "User {} logged in with {} (matched by provider_user_id)",
            user.email,
            provider
        );
        return Ok((user, false));
    }

    // Check if user already exists with this email
    match User::get_user_by_email(db, email).await {
        Ok(user) => {
            // User exists, check if they have this OAuth provider linked
            match UserAuth::get_user_auth(db, provider, email).await {
                Ok(_) => {
                    // OAuth already linked, just log them in
                    tracing::info!("User {} logged in with {}", email, provider);
                    Ok((user, false))
                }
                Err(_) => {
                    // User exists but doesn't have this OAuth provider linked
                    // Link this OAuth provider to the existing account
                    let user_auth_id = Uuid::now_v7();
                    let auth_data = serde_json::json!({
                        "provider_user_id": provider_user_id,
                    });

                    UserAuth::insert_user_auth(
                        db,
                        &user_auth_id,
                        &user.user_id,
                        provider,
                        email,
                        Some(&auth_data),
                        &user.user_id,
                    )
                    .await
                    .map_err(|e| format!("Failed to link OAuth provider: {}", e))?;

                    tracing::info!("Linked {} OAuth to existing user {}", provider, email);
                    Ok((user, false))
                }
            }
        }
        Err(_) => {
            // User doesn't exist, create new user with OAuth
            let user_id = Uuid::now_v7();
            let user_auth_id = Uuid::now_v7();

            // Create user
            let user_name = name.unwrap_or("User");
            User::insert_user(db, &user_id, email, Some(user_name), Some(&user_id))
                .await
                .map_err(|e| format!("Failed to create user: {}", e))?;

            // Create OAuth authentication
            let auth_data = serde_json::json!({
                "provider_user_id": provider_user_id,
            });

            UserAuth::insert_user_auth(
                db,
                &user_auth_id,
                &user_id,
                provider,
                email,
                Some(&auth_data),
                &user_id,
            )
            .await
            .map_err(|e| format!("Failed to create OAuth authentication: {}", e))?;

            // Get the user record
            let user = User::get_user(db, &user_id)
                .await
                .map_err(|e| format!("Failed to get created user: {}", e))?;

            tracing::info!("Created new user {} via {} OAuth", email, provider);
            Ok((user, true))
        }
    }
}

/// Clear all OAuth-related cookies
fn clear_oauth_cookies(cookies: CookieJar) -> CookieJar {
    let cookie_names = [
        OAUTH_STATE_COOKIE_NAME,
        OAUTH_INVITE_COOKIE_NAME,
        OAUTH_NEXT_COOKIE_NAME,
        OAUTH_PLAN_COOKIE_NAME,
        OAUTH_BILLING_COOKIE_NAME,
    ];

    let mut jar = cookies;
    for name in cookie_names {
        jar = jar.add(crate::auth::build_removal_cookie(name));
    }
    jar
}

/// Determine where to redirect after OAuth login/signup.
/// Async wrapper that fetches subscription state, then delegates to the pure function.
async fn determine_oauth_redirect(
    next_url: Option<&str>,
    plan: Option<&str>,
    billing: Option<&str>,
    is_new_user: bool,
    db: &DatabasePool,
    user: &User,
) -> String {
    let has_active_subscription = if plan.is_some() && !is_new_user {
        let user_orgs = hot::db::org::Org::get_orgs_by_user(db, &user.user_id)
            .await
            .unwrap_or_default();

        let mut found = false;
        for org in &user_orgs {
            if let Ok(sub) = hot::db::OrgPlan::get_by_org_id(db, &org.org_id).await
                && sub.is_active()
            {
                found = true;
                break;
            }
        }
        found
    } else {
        false
    };

    compute_oauth_redirect(
        next_url,
        plan,
        billing,
        is_new_user,
        has_active_subscription,
    )
}

/// Pure redirect logic — no I/O, fully testable.
/// Priority: new user -> claim-handle > explicit next URL > plan-based billing redirect > dashboard
fn compute_oauth_redirect(
    next_url: Option<&str>,
    plan: Option<&str>,
    billing: Option<&str>,
    is_new_user: bool,
    has_active_subscription: bool,
) -> String {
    // New OAuth users need to claim their handle first
    if is_new_user {
        let mut url = "/claim-handle".to_string();
        if let Some(plan_id) = plan {
            let billing_period = billing.unwrap_or("monthly");
            url = format!("/claim-handle?plan={}&billing={}", plan_id, billing_period);
        }
        return url;
    }

    if let Some(next) = next_url {
        return next.to_string();
    }

    if let Some(plan_id) = plan {
        let billing_period = billing.unwrap_or("monthly");

        if !has_active_subscription {
            return format!(
                "/billing/create-checkout-form?plan={}&billing={}",
                plan_id, billing_period
            );
        }
    }

    "/".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    // New-user priority: new OAuth users must pick a handle before anything
    // else, so /claim-handle wins over both `next` and plan-based routing.
    // /claim-handle itself forwards to billing/next/dashboard once an org
    // exists (see claim_handle_handler + claim_handle_post_handler).

    #[test]
    fn new_user_with_plan_goes_to_claim_handle() {
        let result = compute_oauth_redirect(None, Some("hot-pro"), Some("annual"), true, false);
        assert_eq!(result, "/claim-handle?plan=hot-pro&billing=annual");
    }

    #[test]
    fn new_user_with_free_plan_goes_to_claim_handle() {
        let result = compute_oauth_redirect(None, Some("hot-free"), Some("monthly"), true, false);
        assert_eq!(result, "/claim-handle?plan=hot-free&billing=monthly");
    }

    #[test]
    fn new_user_no_plan_goes_to_claim_handle() {
        let result = compute_oauth_redirect(None, None, None, true, false);
        assert_eq!(result, "/claim-handle");
    }

    #[test]
    fn new_user_ignores_next_url() {
        // Even an explicit `next` loses to /claim-handle for new users —
        // they don't have an org yet, so `next` would 404 or redirect back.
        let result = compute_oauth_redirect(
            Some("/settings/profile"),
            Some("hot-pro"),
            Some("annual"),
            true,
            false,
        );
        assert_eq!(result, "/claim-handle?plan=hot-pro&billing=annual");
    }

    #[test]
    fn new_user_billing_defaults_to_monthly_when_absent() {
        let result = compute_oauth_redirect(None, Some("hot-free"), None, true, false);
        assert_eq!(result, "/claim-handle?plan=hot-free&billing=monthly");
    }

    // Existing-user paths.

    #[test]
    fn existing_user_next_url_takes_priority() {
        let result = compute_oauth_redirect(
            Some("/settings/profile"),
            Some("hot-pro"),
            Some("annual"),
            false,
            false,
        );
        assert_eq!(result, "/settings/profile");
    }

    #[test]
    fn existing_user_with_plan_and_no_sub_goes_to_billing() {
        let result = compute_oauth_redirect(None, Some("hot-pro"), Some("monthly"), false, false);
        assert_eq!(
            result,
            "/billing/create-checkout-form?plan=hot-pro&billing=monthly"
        );
    }

    #[test]
    fn existing_user_with_plan_and_active_sub_goes_to_dashboard() {
        let result = compute_oauth_redirect(None, Some("hot-pro"), Some("annual"), false, true);
        assert_eq!(result, "/");
    }

    #[test]
    fn existing_user_no_plan_goes_to_dashboard() {
        let result = compute_oauth_redirect(None, None, None, false, true);
        assert_eq!(result, "/");
    }

    #[test]
    fn next_url_wins_over_plan_for_existing_user_without_sub() {
        let result = compute_oauth_redirect(
            Some("/@acme/billing"),
            Some("hot-pro"),
            Some("annual"),
            false,
            false,
        );
        assert_eq!(result, "/@acme/billing");
    }
}

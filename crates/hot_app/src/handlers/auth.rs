use crate::auth::{JWT_COOKIE_NAME, generate_token};
use crate::email::{AppEmailEnqueuer, AppEmailSender};
use crate::templates;
use ahash::AHashMap;
use askama::Template;
use axum::extract::Extension;
use axum::extract::{Form, Query, State};
use axum::response::{Html, IntoResponse, Redirect};
use axum_extra::extract::CookieJar;
use chrono::Utc;
use hot::db::{DatabasePool, EmailVerification, User};
use hot::val::Val;
use jsonwebtoken::{DecodingKey, EncodingKey, Header, Validation, decode, encode};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use time;

// Import common functions from parent module
use super::{
    add_presence_cookie, authenticate_user, create_org, process_invite_code,
    remove_presence_cookie, set_default_org_env_cookies,
};

// Minimum time (in seconds) required between form load and submission
// Submissions faster than this are likely bots
const MIN_FORM_SUBMISSION_TIME_SECS: i64 = 3;

// Claims for the form anti-bot token
#[derive(Debug, Serialize, Deserialize)]
struct FormTokenClaims {
    iat: i64, // Issued at timestamp
    exp: i64, // Expiration (to prevent token reuse)
}

/// Generate a form token for anti-bot protection
/// The token contains the current timestamp and expires after 1 hour
fn generate_form_token() -> String {
    let secret = get_form_token_secret();
    let now = Utc::now().timestamp();

    let claims = FormTokenClaims {
        iat: now,
        exp: now + 3600, // Token valid for 1 hour
    };

    let key = EncodingKey::from_secret(secret.as_ref());
    encode(&Header::default(), &claims, &key).unwrap_or_default()
}

/// Validate form token and check if enough time has passed since form was loaded
/// Returns Ok(()) if valid, Err(message) if invalid or too fast
fn validate_form_token(token: &str) -> Result<(), &'static str> {
    let secret = get_form_token_secret();
    let key = DecodingKey::from_secret(secret.as_ref());

    let token_data = decode::<FormTokenClaims>(token, &key, &Validation::default())
        .map_err(|_| "Invalid form submission. Please try again.")?;

    let now = Utc::now().timestamp();
    let elapsed = now - token_data.claims.iat;

    if elapsed < MIN_FORM_SUBMISSION_TIME_SECS {
        return Err("Please take a moment to fill out the form.");
    }

    Ok(())
}

/// Get the secret for form token signing (reuses session secret)
fn get_form_token_secret() -> String {
    std::env::var("HOT_APP_SESSION_SECRET")
        .unwrap_or_else(|_| "hotdev-form-token-secret-change-in-production".to_string())
}

/// Mint a form token for integration tests, backdated past the minimum
/// submission time so the anti-bot check accepts immediate-on-submit tests.
///
/// Only available when the `test-utils` feature is enabled. Do not call this
/// from non-test code.
#[cfg(feature = "test-utils")]
pub fn mint_form_token_for_tests() -> String {
    let secret = get_form_token_secret();
    let now = Utc::now().timestamp();
    let claims = FormTokenClaims {
        iat: now - (MIN_FORM_SUBMISSION_TIME_SECS + 1),
        exp: now + 3600,
    };
    let key = EncodingKey::from_secret(secret.as_ref());
    encode(&Header::default(), &claims, &key).unwrap_or_default()
}

// Form data structure for signin
#[derive(Deserialize, Debug)]
pub struct SigninForm {
    pub email: String,
    pub password: String,
    pub next: Option<String>,
}

// Form data structure for user signup
#[derive(Deserialize, Debug)]
pub struct SignupForm {
    pub email: String,
    pub password: String,
    pub name: Option<String>,
    pub org_name: Option<String>,
    pub org_slug: Option<String>,
    pub account_type: Option<String>,
    // Anti-bot fields
    pub website: Option<String>, // Honeypot field - should always be empty
    pub form_token: Option<String>, // Time-based token to prevent fast submissions
}

pub async fn signin_handler(
    Extension(conf): Extension<Val>,
    Query(params): Query<AHashMap<String, String>>,
) -> impl IntoResponse {
    // In ordinary local development, redirect to dashboard since auto-login is enabled.
    // Test Hot Cloud configs still need to exercise auth and billing flows.
    if hot::env::is_local_dev() && !hot::product::billing_enabled(&conf) {
        return Redirect::to("/").into_response();
    }

    // User is not authenticated (guaranteed by guest_only_middleware)
    let invite_code = params.get("invite_code").cloned().unwrap_or_default();
    let next = params.get("next").cloned().unwrap_or_default();
    let plan = params.get("plan").cloned().unwrap_or_default();
    let billing = params.get("billing").cloned().unwrap_or_default();

    let template = templates::SignIn {
        title: "Sign In",
        page_context: templates::PublicPageContext::new_with_conf("signin", &conf),
        error_message: "",
        invite_code: invite_code.as_str(),
        next: &next,
        plan: &plan,
        billing: &billing,
    };

    Html(template.render().unwrap()).into_response()
}

pub async fn signin_post_handler(
    State(db): State<Arc<DatabasePool>>,
    Extension(conf): Extension<Val>,
    cookies: CookieJar,
    Query(params): Query<AHashMap<String, String>>,
    Form(form): Form<SigninForm>,
) -> Result<(CookieJar, Redirect), Html<String>> {
    let invite_code = params.get("invite_code").cloned().unwrap_or_default();

    match authenticate_user(&db, &form.email, &form.password).await {
        Ok(user) => {
            // Process invite code if provided
            if !invite_code.is_empty() {
                let _ = process_invite_code(&db, &user.user_id, &invite_code).await;
                // We don't fail if invite processing fails, just continue
            }

            // Generate JWT token for the user
            match generate_token(&user.user_id, &conf) {
                Ok(token) => {
                    let updated_cookies = cookies.add(crate::auth::build_cookie(
                        JWT_COOKIE_NAME,
                        token,
                        time::Duration::days(crate::auth::SESSION_COOKIE_DAYS),
                    ));

                    // Set default org/env cookies if they don't exist yet
                    let cookies_with_defaults =
                        if crate::auth::get_current_org_id_from_cookies(&updated_cookies).is_none()
                        {
                            let fallback_cookies = updated_cookies.clone();
                            set_default_org_env_cookies(&db, &user.user_id, updated_cookies)
                                .await
                                .unwrap_or(fallback_cookies) // Use JWT-only cookies if defaults fail
                        } else {
                            updated_cookies
                        };

                    // Add cross-subdomain presence cookie for hot.dev
                    let final_cookies = add_presence_cookie(cookies_with_defaults);

                    // Authentication successful, redirect to `next` if it's a
                    // safe same-site path, otherwise dashboard
                    let redirect_to = form
                        .next
                        .as_deref()
                        .filter(|n| crate::auth::is_safe_next(n))
                        .unwrap_or("/");
                    Ok((final_cookies, Redirect::to(redirect_to)))
                }
                Err(err) => {
                    // Token generation failed
                    let next_val = form.next.as_deref().unwrap_or("");
                    let template = templates::SignIn {
                        title: "Sign In",
                        page_context: templates::PublicPageContext::new_with_conf("signin", &conf),
                        error_message: &format!("Authentication failed: {}", err),
                        invite_code: invite_code.as_str(),
                        next: next_val,
                        plan: "",
                        billing: "",
                    };

                    Err(Html(template.render().unwrap()))
                }
            }
        }
        Err(error_message) => {
            // Authentication failed, show signin page with error
            let next_val = form.next.as_deref().unwrap_or("");
            let template = templates::SignIn {
                title: "Sign In",
                page_context: templates::PublicPageContext::new_with_conf("signin", &conf),
                error_message: &error_message,
                invite_code: invite_code.as_str(),
                next: next_val,
                plan: "",
                billing: "",
            };

            Err(Html(template.render().unwrap()))
        }
    }
}

pub async fn signout_page_handler() -> impl IntoResponse {
    let template = templates::SignOut {
        title: "Sign Out",
        page_context: templates::PublicPageContext::new("signout"),
    };
    Html(template.render().unwrap()).into_response()
}

pub async fn signout_handler(cookies: CookieJar) -> (CookieJar, Redirect) {
    // In local development, just redirect to dashboard since auto-login is enabled
    if hot::env::is_local_dev() {
        return (cookies, Redirect::to("/"));
    }

    let cookies_cleared = cookies
        .add(crate::auth::build_removal_cookie(JWT_COOKIE_NAME))
        .add(crate::auth::build_removal_cookie(
            crate::auth::CURRENT_ORG_COOKIE_NAME,
        ))
        .add(crate::auth::build_removal_cookie(
            crate::auth::CURRENT_ENV_COOKIE_NAME,
        ));

    // Remove cross-subdomain presence cookie
    let final_cookies = remove_presence_cookie(cookies_cleared);

    // Redirect to signin page
    (final_cookies, Redirect::to("/signin"))
}

pub async fn signup_handler(
    Extension(conf): Extension<Val>,
    Query(params): Query<AHashMap<String, String>>,
) -> impl IntoResponse {
    // In ordinary local development, redirect to dashboard since auto-login is enabled.
    // Test Hot Cloud configs still need to exercise the billing signup flow.
    if hot::env::is_local_dev() && !hot::product::billing_enabled(&conf) {
        return Redirect::to("/").into_response();
    }

    // User is not authenticated (guaranteed by guest_only_middleware)
    let invite_code = params.get("invite_code").cloned().unwrap_or_default();
    let plan = params.get("plan").cloned().unwrap_or_default();
    if hot::product::billing_enabled(&conf) && invite_code.is_empty() && plan.is_empty() {
        return Redirect::to("/signup/plans").into_response();
    }
    let billing = params.get("billing").cloned().unwrap_or_default();

    // Get plan display name
    let plan_display_name = get_plan_display_name(&plan);

    // Generate anti-bot form token
    let form_token = generate_form_token();

    let template = templates::SignUp {
        title: "Sign Up",
        page_context: templates::PublicPageContext::new_with_conf("signup", &conf),
        error_message: "",
        email: "",
        name: "",
        invite_code: &invite_code,
        plan: &plan,
        plan_display_name: &plan_display_name,
        billing: &billing,
        org_name: "",
        org_slug: "",
        account_type: "individual",
        form_token: &form_token,
    };

    Html(template.render().unwrap()).into_response()
}

/// Plan selection page for signup - shown when no plan is pre-selected
pub async fn signup_plans_handler(
    State(db): State<Arc<DatabasePool>>,
    Extension(conf): Extension<Val>,
) -> impl IntoResponse {
    // In ordinary local development, redirect to dashboard.
    // Test Hot Cloud configs still need to exercise the billing signup flow.
    if hot::env::is_local_dev() && !hot::product::billing_enabled(&conf) {
        return Redirect::to("/").into_response();
    }

    let plans: Vec<hot::db::Plan> = hot::db::Plan::get_all_active(&db)
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|p| p.plan_id.is_some())
        .filter(|p| !p.plan_name.contains("Self-Host"))
        .collect();

    let web_url = hot::product::web_url(&conf);

    let template = templates::SignupPlans {
        title: "Choose a Plan",
        page_context: templates::PublicPageContext::new_with_conf("signup", &conf),
        plans,
        web_url: &web_url,
    };

    Html(template.render().unwrap()).into_response()
}

/// Get display name for a plan ID
fn get_plan_display_name(plan_id: &str) -> String {
    match plan_id {
        "hot-free" => "Hot Cloud Free".to_string(),
        "hot-cloud-starter" => "Hot Cloud Starter".to_string(),
        "hot-cloud-pro" => "Hot Cloud Pro".to_string(),
        "hot-cloud-scale" => "Hot Cloud Scale".to_string(),
        _ => plan_id.replace('-', " ").to_string(),
    }
}

pub async fn signup_post_handler(
    State(db): State<Arc<DatabasePool>>,
    Extension(conf): Extension<Val>,
    Query(params): Query<AHashMap<String, String>>,
    Form(form): Form<SignupForm>,
) -> Result<Html<String>, Html<String>> {
    let invite_code = params.get("invite_code").cloned().unwrap_or_default();
    let plan = params.get("plan").cloned().unwrap_or_default();
    let billing = params
        .get("billing")
        .cloned()
        .unwrap_or_else(|| "monthly".to_string());

    // Helper to render error with all fields
    let render_error = |msg: &str| {
        render_signup_with_error_full(
            msg,
            &form.email,
            form.name.as_deref().unwrap_or(""),
            &invite_code,
            &plan,
            &billing,
            form.org_name.as_deref().unwrap_or(""),
            form.org_slug.as_deref().unwrap_or(""),
        )
    };

    if hot::product::billing_enabled(&conf) && invite_code.is_empty() && plan.is_empty() {
        return Err(render_error(
            "Please choose a plan before creating your account.",
        ));
    }

    // Anti-bot check 1: Honeypot field
    // If the hidden "website" field has a value, it's likely a bot
    // Silently return success to not tip off the bot
    if form.website.as_ref().is_some_and(|w| !w.is_empty()) {
        tracing::warn!("Honeypot triggered on signup for email: {}", form.email);
        return Ok(render_check_email(&form.email, false, plan == "hot-free"));
    }

    // Anti-bot check 2: Form submission timing
    // Reject submissions that happen too quickly (likely automated)
    if let Some(token) = &form.form_token {
        if let Err(msg) = validate_form_token(token) {
            tracing::warn!(
                "Form token validation failed on signup for email: {}",
                form.email
            );
            return Err(render_error(msg));
        }
    } else {
        // Missing token - could be a bot that didn't load the form properly
        tracing::warn!("Missing form token on signup for email: {}", form.email);
        return Err(render_error("Invalid form submission. Please try again."));
    }

    // Validate form data
    if form.email.trim().is_empty() {
        return Err(render_error("Email is required"));
    }

    // Validate email format
    if !form.email.contains('@') {
        return Err(render_error("Please enter a valid email address"));
    }

    if form.password.trim().is_empty() {
        return Err(render_error("Password is required"));
    }

    if form.password.len() < 8 {
        return Err(render_error("Password must be at least 8 characters"));
    }

    // Determine account type (default to "individual")
    let account_type = form
        .account_type
        .as_deref()
        .unwrap_or("individual")
        .to_string();

    // Name is always required
    if form.name.as_deref().unwrap_or("").trim().is_empty() {
        return Err(render_error("Full name is required"));
    }

    let is_invite_signup = !invite_code.is_empty();

    // Check for an existing account FIRST. If the user is already registered,
    // the right answer is "go sign in" regardless of what slug they typed.
    if User::get_user_by_email(&db, &form.email).await.is_ok() {
        return Err(render_error("A user with this email already exists"));
    }

    // Check pending verification for this email BEFORE the slug check, so
    // a user who refreshes the signup form (same email, same slug) just
    // gets re-shown the "check your email" page instead of a spurious
    // "handle is already taken" message caused by their own pending record.
    if let Ok(Some(existing)) = EmailVerification::get_pending_by_email(&db, &form.email).await
        && existing.is_valid().is_ok()
    {
        return Ok(render_check_email(&form.email, true, plan == "hot-free"));
    }

    if !is_invite_signup {
        let org_slug = form.org_slug.as_deref().unwrap_or("").trim();

        if account_type == "organization" {
            let org_name = form.org_name.as_deref().unwrap_or("").trim();
            if org_name.is_empty() {
                return Err(render_error("Organization name is required"));
            }
        }

        // Full slug validation: format + reserved (baked-in + the
        // deployment-supplied `hot.org.reserved-slugs` list) + existing orgs +
        // pending verifications. On "taken", suggest an alternative so the
        // user has a one-click retry.
        let extra_reserved = crate::slug::extra_reserved_from_conf(&conf);
        match crate::slug::ensure_available_with_extra(&db, org_slug, &extra_reserved).await {
            Ok(()) => {}
            Err(crate::slug::SlugError::Taken) => {
                let suggestion = crate::slug::suggest_alternative(&db, org_slug).await;
                return Err(render_signup_with_error_full(
                    &format!(
                        "{} Try \u{201c}{}\u{201d}.",
                        crate::slug::SlugError::Taken.message(),
                        suggestion
                    ),
                    &form.email,
                    form.name.as_deref().unwrap_or(""),
                    &invite_code,
                    &plan,
                    &billing,
                    form.org_name.as_deref().unwrap_or(""),
                    &suggestion,
                ));
            }
            Err(e) => return Err(render_error(e.message())),
        }
    }

    // Hash the password
    let password_hash = hot::auth::hash_password_with_random_salt(&form.password)
        .map_err(|_| render_error("Failed to process your request. Please try again."))?;

    // Generate verification token and ID
    let verification_id = uuid::Uuid::now_v7();
    let verification_token = EmailVerification::generate_token();
    let expires_at = Utc::now() + chrono::Duration::hours(24);

    // Normalize empty strings to None so downstream checks (`is_some()`) are
    // accurate — individual signups submit a hidden empty `org_name` field.
    let org_name_trimmed = form
        .org_name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let org_slug_trimmed = form
        .org_slug
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());

    // Create the email verification record
    EmailVerification::insert(
        &db,
        &verification_id,
        &form.email,
        form.name.as_deref(),
        &password_hash,
        &verification_token,
        if invite_code.is_empty() {
            None
        } else {
            Some(&invite_code)
        },
        org_name_trimmed,
        org_slug_trimmed,
        if plan.is_empty() { None } else { Some(&plan) },
        if plan.is_empty() {
            None
        } else {
            Some(&billing)
        },
        expires_at,
        Some(&account_type),
    )
    .await
    .map_err(|e| {
        tracing::error!("Failed to create email verification: {:?}", e);
        render_error("Failed to process your request. Please try again.")
    })?;

    // Enqueue the verification email for sending by the worker
    let email_enqueuer = AppEmailEnqueuer::from_conf(db.clone(), &conf);
    if let Err(e) = email_enqueuer
        .send_verification_email(&form.email, form.name.as_deref(), &verification_token)
        .await
    {
        tracing::error!("Failed to enqueue verification email: {:?}", e);
        // Don't fail the signup, still show the check email page
        // The user can request a resend
    }

    // Show the "check your email" page
    Ok(render_check_email(&form.email, false, plan == "hot-free"))
}

/// Render the "check your email" page
fn render_check_email(email: &str, already_pending: bool, is_free_plan: bool) -> Html<String> {
    let template = templates::CheckEmail {
        title: "Check Your Email",
        page_context: templates::PublicPageContext::new("signup"),
        email,
        already_pending,
        is_free_plan,
    };

    Html(template.render().unwrap())
}

/// Email verification handler - verifies the token and creates the user
pub async fn verify_email_handler(
    State(db): State<Arc<DatabasePool>>,
    Extension(conf): Extension<Val>,
    cookies: CookieJar,
    Query(params): Query<AHashMap<String, String>>,
) -> Result<(CookieJar, Redirect), Html<String>> {
    let token = params.get("token").cloned().unwrap_or_default();

    if token.is_empty() {
        return Err(render_verification_error(
            "Invalid verification link",
            "The verification link is missing required information.",
        ));
    }

    // Get the verification record
    let verification = EmailVerification::get_by_token(&db, &token)
        .await
        .map_err(|_| {
            render_verification_error(
                "Invalid verification link",
                "This verification link is invalid or has already been used.",
            )
        })?;

    // Check if verification is valid
    if let Err(e) = verification.is_valid() {
        match e {
            hot::db::EmailVerificationError::Expired => {
                return Err(render_verification_error(
                    "Verification link expired",
                    "This verification link has expired. Please sign up again to receive a new link.",
                ));
            }
            hot::db::EmailVerificationError::AlreadyVerified => {
                // Idempotent re-run. The link may have been pre-fetched by a
                // mail scanner (Microsoft Safe Links, Proofpoint, …) or the
                // user may have clicked it in a second browser. Either way,
                // we log the real clicker in and forward them to the same
                // "next step" the original verify would have produced.
                return handle_already_verified(&db, &conf, cookies, &verification).await;
            }
            _ => {
                return Err(render_verification_error(
                    "Verification failed",
                    "Unable to verify your email. Please try again.",
                ));
            }
        }
    }

    // If a user record already exists for this email, we're in one of two
    // recoverable states:
    //   (a) a prior verify attempt created the user + auth rows but then
    //       failed on org creation — leaving the verification NOT marked
    //       verified. We want to retry org creation now.
    //   (b) someone signed up with the same email between our two checks
    //       (truly rare). Safest thing is still to continue with org
    //       creation against the existing user — worst case the org
    //       insert fails and we redirect to claim-handle, same as any
    //       other signup mismatch.
    //
    // Either way, reuse the existing user rather than erroring out.
    let user_name = verification.name.as_deref().unwrap_or("User");
    let user_id = match User::get_user_by_email(&db, &verification.email).await {
        Ok(existing) => existing.user_id,
        Err(_) => {
            let new_user_id = uuid::Uuid::now_v7();
            let user_auth_id = uuid::Uuid::now_v7();

            User::insert_user(
                &db,
                &new_user_id,
                &verification.email,
                Some(user_name),
                Some(&new_user_id),
            )
            .await
            .map_err(|e| {
                tracing::error!("Failed to create user during verification: {:?}", e);
                render_verification_error(
                    "Account creation failed",
                    "Failed to create your account. Please try again.",
                )
            })?;

            let auth_data: serde_json::Value = serde_json::from_str(&verification.password_hash)
                .map_err(|e| {
                    tracing::error!("Failed to parse password hash: {:?}", e);
                    render_verification_error(
                        "Account creation failed",
                        "Failed to create your account. Please try again.",
                    )
                })?;

            hot::db::user::UserAuth::insert_user_auth(
                &db,
                &user_auth_id,
                &new_user_id,
                "email_password",
                &verification.email,
                Some(&auth_data),
                &new_user_id,
            )
            .await
            .map_err(|e| {
                tracing::error!("Failed to create user auth during verification: {:?}", e);
                render_verification_error(
                    "Account creation failed",
                    "Failed to create your account. Please try again.",
                )
            })?;

            new_user_id
        }
    };

    // Determine account type from verification record
    let account_type = verification.account_type.as_deref().unwrap_or("individual");

    // Create org if slug was provided during signup. `create_org` is
    // idempotent for slugs owned by this user (adopts orphans from prior
    // partial attempts) and self-rolls-back on post-insert failure, so the
    // only way this returns Err is if the slug is genuinely owned by someone
    // else — i.e. it was grabbed between signup and verify by a different
    // user creating an org via /orgs/new.
    //
    // In that rare case we don't strand the user: log them in and forward
    // to /claim-handle?taken=slug so they can pick an alternative in one step.
    //
    // Track BOTH org_id AND slug so we can redirect directly to
    // `/@{slug}/billing/checkout` and skip the cookie-roundtrip through
    // `/billing/create-checkout-form`. The roundtrip is fragile: if the
    // org cookie doesn't make it back to the browser, the next request's
    // session middleware sees `current_org=None` and bounces the user to
    // `/claim-handle` even though their org was just created. Direct
    // routing avoids that whole class of failure.
    let created_org: Option<(uuid::Uuid, String)> =
        if let Some(org_slug) = verification.org_slug.as_deref() {
            let org_name = if account_type == "organization" {
                verification.org_name.as_deref().unwrap_or(user_name)
            } else {
                user_name
            };

            match create_org(&db, &user_id, org_name, org_slug, account_type).await {
                Ok(org_id) => {
                    // Read-after-write sanity check. `create_org` returning
                    // Ok means insert_org committed (or an existing owned
                    // row was adopted). If we can't fetch it back by slug
                    // immediately afterwards, something is *very* wrong
                    // (replica lag, schema mismatch, parallel deletion,
                    // mis-cased slug, etc.) and constructing a `/@{slug}/`
                    // URL would silently 404 the user. Log loud and treat
                    // it as a creation failure so the caller falls through
                    // to the recovery path instead of generating a dead
                    // redirect.
                    match hot::db::org::Org::get_org_by_slug(&db, org_slug).await {
                        Ok(found) if found.org_id == org_id => {
                            tracing::info!(
                                "verify_email_handler: created/adopted org {} (slug {}) for \
                                 user {} — confirmed visible",
                                org_id,
                                org_slug,
                                verification.email
                            );
                            Some((org_id, org_slug.to_string()))
                        }
                        Ok(found) => {
                            tracing::error!(
                                "verify_email_handler: post-create read-back returned a \
                                 DIFFERENT org for slug {} (got org_id {}, expected {}) — \
                                 user {} — refusing to redirect to dead URL",
                                org_slug,
                                found.org_id,
                                org_id,
                                verification.email
                            );
                            None
                        }
                        Err(e) => {
                            tracing::error!(
                                "verify_email_handler: create_org returned Ok({}) for slug {} \
                                 but get_org_by_slug immediately afterwards failed: {:?} — \
                                 user {} — refusing to redirect to dead URL",
                                org_id,
                                org_slug,
                                e,
                                verification.email
                            );
                            None
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        "verify_email_handler: org creation failed for user {} (slug {}): {} \
                         — forwarding to claim-handle?taken={}",
                        verification.email,
                        org_slug,
                        e,
                        org_slug
                    );
                    None
                }
            }
        } else {
            None
        };
    let created_org_id = created_org.as_ref().map(|(id, _)| *id);

    // Process invite code if provided (for non-paid signups)
    if let Some(invite_code) = &verification.invite_code {
        let _ = process_invite_code(&db, &user_id, invite_code).await;
    }

    // Mark verification as completed only once we've got the user in a
    // good steady state (org exists, or the caller never asked for one).
    //
    // If we wanted an org but couldn't create it, leaving the verification
    // unverified lets the user click the link again (which now hits our
    // idempotent create_org + owned-orphan recovery) — far friendlier than
    // stranding them at /claim-handle with a ghost handle.
    let verification_complete = verification.org_slug.is_none() || created_org_id.is_some();
    if verification_complete {
        let _ = EmailVerification::mark_verified(&db, &verification.verification_id).await;
    }

    // Generate JWT token for the user
    let token = generate_token(&user_id, &conf).map_err(|e| {
        tracing::error!("Failed to generate token during verification: {:?}", e);
        render_verification_error(
            "Account created",
            "Your account was created successfully! Please sign in.",
        )
    })?;

    // Set JWT cookie
    let mut cookie = axum_extra::extract::cookie::Cookie::new(JWT_COOKIE_NAME, token);
    cookie.set_path("/");
    cookie.set_max_age(time::Duration::days(1));
    cookie.set_http_only(true);
    cookie.set_secure(!hot::env::is_local_dev());
    cookie.set_same_site(axum_extra::extract::cookie::SameSite::Lax);

    let updated_cookies = cookies.add(cookie);

    // Set default org/env cookies (prefer newly created org)
    let cookies_with_org = if let Some(org_id) = created_org_id {
        let mut org_cookie = axum_extra::extract::cookie::Cookie::new(
            crate::auth::CURRENT_ORG_COOKIE_NAME,
            org_id.to_string(),
        );
        org_cookie.set_path("/");
        org_cookie.set_max_age(time::Duration::days(365));
        org_cookie.set_http_only(true);
        org_cookie.set_secure(!hot::env::is_local_dev());
        org_cookie.set_same_site(axum_extra::extract::cookie::SameSite::Lax);
        updated_cookies.add(org_cookie)
    } else {
        set_default_org_env_cookies(&db, &user_id, updated_cookies.clone())
            .await
            .unwrap_or(updated_cookies)
    };

    // Add cross-subdomain presence cookie
    let final_cookies = add_presence_cookie(cookies_with_org);

    tracing::info!("User {} verified and logged in", verification.email);

    // If org creation failed but signup wanted one, forward to /claim-handle.
    // The claim-handle page will pre-fill a suggested slug (an *alternative*
    // to the taken one) and preserve plan params.
    let org_creation_needed_but_failed =
        verification.org_slug.is_some() && created_org_id.is_none();
    if org_creation_needed_but_failed {
        let plan = verification.plan.as_deref().unwrap_or("");
        let billing = verification.billing.as_deref().unwrap_or("monthly");
        // Pass the taken slug so /claim-handle can suggest a real alternative
        // (e.g. `alice-2`) instead of echoing the base back at the user.
        let taken = verification.org_slug.as_deref().unwrap_or("");
        let mut qs = Vec::new();
        if !plan.is_empty() {
            qs.push(format!("plan={}", plan));
            qs.push(format!("billing={}", billing));
        }
        if !taken.is_empty() {
            qs.push(format!("taken={}", taken));
        }
        let target = if qs.is_empty() {
            "/claim-handle".to_string()
        } else {
            format!("/claim-handle?{}", qs.join("&"))
        };
        return Ok((final_cookies, Redirect::to(&target)));
    }

    // If a plan was selected, redirect to billing checkout.
    //
    // Prefer the org-slug-scoped URL when we know the slug — this avoids
    // the `/billing/create-checkout-form` indirection that depends on the
    // org cookie surviving the redirect. If for any reason we don't have
    // a slug here (no org_slug on the verification record), fall back to
    // the cookie-based handler.
    if verification.plan.is_some() {
        let plan = verification.plan.as_deref().unwrap_or("hot-cloud-starter");
        let billing = verification.billing.as_deref().unwrap_or("monthly");
        let target = if let Some((_, slug)) = created_org.as_ref() {
            tracing::info!(
                "verify_email_handler: redirecting user {} directly to /@{}/billing/checkout",
                verification.email,
                slug
            );
            format!(
                "/@{}/billing/checkout?plan={}&billing={}",
                slug, plan, billing
            )
        } else {
            tracing::info!(
                "verify_email_handler: no created_org slug; falling back to \
                 /billing/create-checkout-form for user {}",
                verification.email
            );
            format!(
                "/billing/create-checkout-form?plan={}&billing={}",
                plan, billing
            )
        };
        Ok((final_cookies, Redirect::to(&target)))
    } else {
        Ok((final_cookies, Redirect::to("/")))
    }
}

/// Handles a verify-email click on an already-verified token.
///
/// This is triggered by:
/// - Mail scanner pre-fetches (Microsoft Safe Links, Proofpoint, Mimecast, etc.)
///   that "click" every URL in an email before it reaches the user.
/// - The user clicking the link in a different browser than the one where
///   they started signup, then reopening the email later.
/// - The user clicking the verify link twice.
///
/// Behavior: find the real user, log them in (set JWT + org cookies), and
/// redirect them to whichever "next step" a first-time verify would have
/// produced (claim-handle / billing / dashboard). This makes the whole
/// verify flow idempotent and browser-agnostic.
async fn handle_already_verified(
    db: &DatabasePool,
    conf: &Val,
    cookies: CookieJar,
    verification: &EmailVerification,
) -> Result<(CookieJar, Redirect), Html<String>> {
    // Time-bound the idempotent re-login: scanner/second-browser tolerance is
    // only intended within the original verification window. Without this
    // check, every verification email would remain a permanent no-password
    // login link for anyone who obtains it later.
    if verification.expires_at < Utc::now() {
        return Err(render_verification_error(
            "Already verified",
            "This email has already been verified. Please sign in.",
        ));
    }

    let Ok(user) = User::get_user_by_email(db, &verification.email).await else {
        // Verification is marked verified but no user exists — very unusual
        // (would require a partial signup failure). Don't strand the user.
        tracing::warn!(
            "AlreadyVerified but no user for {} — falling back to sign-in page",
            verification.email
        );
        return Err(render_verification_error(
            "Already verified",
            "This email has already been verified. Please sign in.",
        ));
    };

    tracing::info!(
        "User {} re-presented verification token — logging in idempotently",
        verification.email
    );

    // Pick the org that was requested at signup if it exists and belongs to
    // this user; otherwise fall back to any org they have.
    let user_orgs = hot::db::org::Org::get_orgs_by_user(db, &user.user_id)
        .await
        .unwrap_or_default();
    let mut chosen_org = verification
        .org_slug
        .as_deref()
        .and_then(|slug| user_orgs.iter().find(|o| o.slug == slug).cloned())
        .or_else(|| user_orgs.first().cloned());

    // Orphan recovery: the requested slug may exist in `org` but the
    // membership row might be missing (partial failure from a prior
    // attempt, before `create_org` was transactional). If the user owns
    // that `org` row, heal the link + env here so they land on their org.
    if chosen_org.is_none()
        && let Some(slug) = verification.org_slug.as_deref()
        && let Ok(existing) = hot::db::org::Org::get_org_by_slug(db, slug).await
        && existing.created_by_user_id == user.user_id
    {
        tracing::info!(
            "handle_already_verified: adopting orphan org {} for user {}",
            existing.org_id,
            user.user_id
        );
        crate::handlers::ensure_org_membership_and_env(db, &existing.org_id, &user.user_id).await;
        chosen_org = Some(existing);
    }

    let token = generate_token(&user.user_id, conf).map_err(|_| {
        render_verification_error(
            "Session error",
            "Your account is verified, but we couldn't sign you in. Please sign in.",
        )
    })?;

    let mut jwt_cookie = axum_extra::extract::cookie::Cookie::new(JWT_COOKIE_NAME, token);
    jwt_cookie.set_path("/");
    jwt_cookie.set_max_age(time::Duration::days(1));
    jwt_cookie.set_http_only(true);
    jwt_cookie.set_secure(!hot::env::is_local_dev());
    jwt_cookie.set_same_site(axum_extra::extract::cookie::SameSite::Lax);
    let updated_cookies = cookies.add(jwt_cookie);

    let cookies_with_org = if let Some(org) = chosen_org.as_ref() {
        let mut org_cookie = axum_extra::extract::cookie::Cookie::new(
            crate::auth::CURRENT_ORG_COOKIE_NAME,
            org.org_id.to_string(),
        );
        org_cookie.set_path("/");
        org_cookie.set_max_age(time::Duration::days(365));
        org_cookie.set_http_only(true);
        org_cookie.set_secure(!hot::env::is_local_dev());
        org_cookie.set_same_site(axum_extra::extract::cookie::SameSite::Lax);
        updated_cookies.add(org_cookie)
    } else {
        set_default_org_env_cookies(db, &user.user_id, updated_cookies.clone())
            .await
            .unwrap_or(updated_cookies)
    };

    let final_cookies = add_presence_cookie(cookies_with_org);

    // Decide where to send them next. Mirrors the first-time verify logic:
    //  - no org yet → /claim-handle (so they can pick a handle)
    //  - plan selected + no subscription yet → billing checkout (slug-scoped)
    //  - plan selected + subscription exists → dashboard (already paid)
    //  - no plan → dashboard
    //
    // When `chosen_org` is set we route directly to `/@{slug}/billing/checkout`
    // so the next request doesn't need the org cookie to make it back —
    // that round-trip has been the source of "user lands on /claim-handle
    // after verify" reports.
    let plan = verification.plan.as_deref().unwrap_or("");
    let billing = verification.billing.as_deref().unwrap_or("monthly");

    let target = match chosen_org.as_ref() {
        None if !plan.is_empty() => {
            tracing::info!(
                "handle_already_verified: no org for user {}; redirecting to /claim-handle",
                verification.email
            );
            format!("/claim-handle?plan={}&billing={}", plan, billing)
        }
        None => {
            tracing::info!(
                "handle_already_verified: no org for user {}; redirecting to /claim-handle",
                verification.email
            );
            "/claim-handle".to_string()
        }
        Some(org) if !plan.is_empty() => {
            let has_subscription = hot::db::OrgPlan::get_by_org_id(db, &org.org_id)
                .await
                .is_ok();
            if has_subscription {
                tracing::info!(
                    "handle_already_verified: user {} already has subscription on org {}; \
                     redirecting to dashboard",
                    verification.email,
                    org.slug
                );
                "/".to_string()
            } else {
                tracing::info!(
                    "handle_already_verified: redirecting user {} directly to \
                     /@{}/billing/checkout",
                    verification.email,
                    org.slug
                );
                format!(
                    "/@{}/billing/checkout?plan={}&billing={}",
                    org.slug, plan, billing
                )
            }
        }
        Some(_) => "/".to_string(),
    };

    Ok((final_cookies, Redirect::to(&target)))
}

/// Resend verification email handler
pub async fn resend_verification_handler(
    State(db): State<Arc<DatabasePool>>,
    Extension(conf): Extension<Val>,
    Form(form): Form<ResendVerificationForm>,
) -> impl IntoResponse {
    // Find the pending verification
    let verification = match EmailVerification::get_pending_by_email(&db, &form.email).await {
        Ok(Some(v)) => v,
        _ => {
            // Don't reveal whether email exists
            return Html(render_check_email(&form.email, false, false).0);
        }
    };

    // Check attempt limits (max 5 resends)
    if verification.attempts >= 5 {
        let is_free = verification.plan.as_deref() == Some("hot-free");
        let template = templates::CheckEmail {
            title: "Check Your Email",
            page_context: templates::PublicPageContext::new("signup"),
            email: &form.email,
            already_pending: true,
            is_free_plan: is_free,
        };
        return Html(template.render().unwrap());
    }

    // Increment attempts
    let _ = EmailVerification::increment_attempts(&db, &verification.verification_id).await;

    // Generate new token and update expiry
    let new_token = EmailVerification::generate_token();
    let new_expires_at = Utc::now() + chrono::Duration::hours(24);

    if let Err(e) = EmailVerification::update_token(
        &db,
        &verification.verification_id,
        &new_token,
        new_expires_at,
    )
    .await
    {
        tracing::error!("Failed to update verification token: {:?}", e);
    }

    // Enqueue the verification email for sending by the worker
    let email_enqueuer = AppEmailEnqueuer::from_conf(db.clone(), &conf);
    if let Err(e) = email_enqueuer
        .send_verification_email(&form.email, verification.name.as_deref(), &new_token)
        .await
    {
        tracing::error!("Failed to enqueue verification email: {:?}", e);
    }

    let is_free = verification.plan.as_deref() == Some("hot-free");
    Html(render_check_email(&form.email, false, is_free).0)
}

/// Form data for resend verification
#[derive(Deserialize, Debug)]
pub struct ResendVerificationForm {
    pub email: String,
}

/// Render verification error page
fn render_verification_error(title: &str, message: &str) -> Html<String> {
    let template = templates::VerificationError {
        title,
        page_context: templates::PublicPageContext::new("signup"),
        error_title: title,
        error_message: message,
    };

    Html(template.render().unwrap())
}

// Helper function to render signup form with error
#[allow(clippy::too_many_arguments)]
fn render_signup_with_error_full(
    error_message: &str,
    email: &str,
    name: &str,
    invite_code: &str,
    plan: &str,
    billing: &str,
    org_name: &str,
    org_slug: &str,
) -> Html<String> {
    let plan_display_name = get_plan_display_name(plan);
    let form_token = generate_form_token();
    let template = templates::SignUp {
        title: "Sign Up",
        page_context: templates::PublicPageContext::new("signup"),
        error_message,
        email,
        name,
        invite_code,
        plan,
        plan_display_name: &plan_display_name,
        billing,
        org_name,
        org_slug,
        account_type: "individual",
        form_token: &form_token,
    };

    Html(template.render().unwrap())
}

// Form data for claim-handle page
#[derive(Deserialize, Debug)]
pub struct ClaimHandleForm {
    pub org_name: Option<String>,
    pub org_slug: String,
    pub account_type: Option<String>,
}

/// GET /claim-handle — show the claim-handle page for new OAuth users.
///
/// If the user already has an org (e.g., email/password signup where the handle
/// was collected on the signup form), skip the form and forward them — either to
/// their org's checkout page when a plan is being purchased, or to the dashboard.
///
/// Otherwise pre-fill the slug field with an available suggestion derived from
/// the user's name so they have a one-click path forward.
pub async fn claim_handle_handler(
    State(db): State<Arc<DatabasePool>>,
    Extension(conf): Extension<Val>,
    axum::extract::Extension(session): axum::extract::Extension<crate::auth::Session>,
    Query(params): Query<AHashMap<String, String>>,
) -> impl IntoResponse {
    let plan = params.get("plan").cloned().unwrap_or_default();
    let billing = params
        .get("billing")
        .cloned()
        .unwrap_or_else(|| "monthly".to_string());

    // User already has a handle — no need to claim one. Check the session
    // snapshot first, then fall back to a full recovery (fresh DB read of
    // user_orgs + auto-create from pending email_verification slug). This
    // is the safety net for any path that put us at /claim-handle without
    // a current_org despite the user having completed signup.
    let existing_slug: Option<String> = if let Some(org) = &session.current_org {
        Some(org.slug.clone())
    } else {
        crate::handlers::recover_or_create_org_for_user(
            &db,
            &session.current_user_id(),
            &session.user.email,
            &session.user_name,
        )
        .await
    };

    if let Some(slug) = existing_slug {
        tracing::info!(
            "claim_handle_handler: skipping form for user {}; routing to /@{}/...",
            session.user.email,
            slug
        );
        if !plan.is_empty() {
            return Redirect::to(&format!(
                "/@{}/billing/checkout?plan={}&billing={}",
                slug, plan, billing
            ))
            .into_response();
        }
        if session.billing_enabled && session.current_org_subscription_status.is_none() {
            return Redirect::to(&format!("/@{}/billing/checkout", slug)).into_response();
        }
        return Redirect::to("/").into_response();
    }

    // If the user was redirected here because a specific slug was taken at
    // verify-time (see `verify_email_handler`'s graceful-degrade path), pick
    // an *alternative* to that slug so we never echo the taken one back at
    // them. Otherwise derive a fresh base from the user's name.
    let extra_reserved = crate::slug::extra_reserved_from_conf(&conf);
    let taken = params.get("taken").map(String::as_str).unwrap_or("").trim();
    let (error_banner, suggested) = if !taken.is_empty()
        && crate::slug::validate_format_with_extra(taken, &extra_reserved).is_ok()
    {
        let alt = crate::slug::suggest_alternative(&db, taken).await;
        (
            format!(
                "\u{201c}{}\u{201d} was just taken. Try \u{201c}{}\u{201d} instead.",
                taken, alt
            ),
            alt,
        )
    } else {
        // slugify: lowercase, replace non-alphanumeric runs with hyphens, trim hyphens.
        let base = session
            .user_name
            .to_lowercase()
            .chars()
            .map(|c| {
                if c.is_ascii_lowercase() || c.is_ascii_digit() {
                    c
                } else {
                    '-'
                }
            })
            .collect::<String>();
        let base: String = base
            .split('-')
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("-");
        let s = if crate::slug::validate_format_with_extra(&base, &extra_reserved).is_ok() {
            crate::slug::suggest_available(&db, &base).await
        } else {
            String::new()
        };
        (String::new(), s)
    };

    let template = templates::ClaimHandle {
        title: "Claim Your Handle",
        page_context: templates::PublicPageContext::new("claim-handle"),
        error_message: &error_banner,
        org_name: "",
        org_slug: &suggested,
        account_type: "individual",
        plan: &plan,
        billing: &billing,
    };

    Html(template.render().unwrap()).into_response()
}

/// POST /claim-handle — process handle claim and create org
pub async fn claim_handle_post_handler(
    State(db): State<Arc<DatabasePool>>,
    Extension(conf): Extension<Val>,
    axum::extract::Extension(session): axum::extract::Extension<crate::auth::Session>,
    cookies: CookieJar,
    Query(params): Query<AHashMap<String, String>>,
    Form(form): Form<ClaimHandleForm>,
) -> Result<(CookieJar, Redirect), Html<String>> {
    let plan = params.get("plan").cloned().unwrap_or_default();
    let billing = params
        .get("billing")
        .cloned()
        .unwrap_or_else(|| "monthly".to_string());

    let account_type = form.account_type.as_deref().unwrap_or("individual");

    // If the user already has an org (or had one in flight from signup),
    // don't create a duplicate — just forward them. Keeps /claim-handle
    // idempotent for users who already have a handle, and means a refresh
    // / double-submit / racing-tab can never silently create a SECOND org.
    //
    // We only run the FULL recover_or_create path here when the user
    // submitted an EMPTY slug. With a non-empty slug we want their submitted
    // value to take precedence over whatever was on a stale verification
    // record — they may be deliberately picking something different.
    let user_has_org = if let Some(org) = &session.current_org {
        Some(org.slug.clone())
    } else if form.org_slug.trim().is_empty() {
        crate::handlers::recover_or_create_org_for_user(
            &db,
            &session.current_user_id(),
            &session.user.email,
            &session.user_name,
        )
        .await
    } else {
        match hot::db::org::Org::get_orgs_by_user(&db, &session.current_user_id()).await {
            Ok(orgs) if !orgs.is_empty() => Some(orgs[0].slug.clone()),
            _ => None,
        }
    };

    if let Some(slug) = user_has_org {
        tracing::info!(
            "claim_handle_post_handler: user {} already has org {}; idempotent redirect",
            session.user.email,
            slug
        );
        let target = if !plan.is_empty() {
            format!(
                "/@{}/billing/checkout?plan={}&billing={}",
                slug, plan, billing
            )
        } else if session.billing_enabled && session.current_org_subscription_status.is_none() {
            format!("/@{}/billing/checkout", slug)
        } else {
            "/".to_string()
        };
        return Ok((cookies, Redirect::to(&target)));
    }

    let render_with = |msg: &str, slug: &str| {
        let template = templates::ClaimHandle {
            title: "Claim Your Handle",
            page_context: templates::PublicPageContext::new("claim-handle"),
            error_message: msg,
            org_name: form.org_name.as_deref().unwrap_or(""),
            org_slug: slug,
            account_type,
            plan: &plan,
            billing: &billing,
        };
        Html(template.render().unwrap())
    };
    let render_error = |msg: &str| render_with(msg, &form.org_slug);

    // For organization type, org name is required
    if account_type == "organization" {
        let org_name = form.org_name.as_deref().unwrap_or("").trim();
        if org_name.is_empty() {
            return Err(render_error("Organization name is required"));
        }
    }

    // Limit one individual org per user
    if account_type == "individual"
        && let Ok(Some(_)) =
            hot::db::org::Org::get_individual_org_by_user(&db, &session.current_user_id()).await
    {
        return Err(render_error(
            "You already have an individual organization. Choose \"Organization\" to create a team org.",
        ));
    }

    let org_slug = form.org_slug.trim();

    // Format + reserved-word validation (baked-in + the deployment-supplied
    // `hot.org.reserved-slugs` list); no DB. Must pass before we can touch
    // the orgs table at all.
    let extra_reserved = crate::slug::extra_reserved_from_conf(&conf);
    if let Err(e) = crate::slug::validate_format_with_extra(org_slug, &extra_reserved) {
        return Err(render_error(e.message()));
    }

    // Back-button / orphan recovery: if an `org` row already exists at this
    // slug AND it's owned by the current user, adopt it instead of treating
    // it as taken. This covers:
    //   - users who refresh /claim-handle after a successful POST
    //   - orphans left by partial-failure attempts pre-transactional-create_org
    //   - mail-scanner pre-fetches of the verify link that created the org
    //     before the real user clicked through
    if let Ok(existing) = hot::db::org::Org::get_org_by_slug(&db, org_slug).await
        && existing.created_by_user_id == session.current_user_id()
    {
        tracing::info!(
            "claim_handle_post_handler: adopting existing org {} for user {}",
            existing.org_id,
            session.current_user_id()
        );
        crate::handlers::ensure_org_membership_and_env(
            &db,
            &existing.org_id,
            &session.current_user_id(),
        )
        .await;

        let updated_cookies = cookies.add(crate::auth::build_cookie(
            crate::auth::CURRENT_ORG_COOKIE_NAME,
            existing.org_id.to_string(),
            time::Duration::days(365),
        ));

        let target = if !plan.is_empty() {
            format!(
                "/@{}/billing/checkout?plan={}&billing={}",
                existing.slug, plan, billing
            )
        } else if session.billing_enabled {
            format!("/@{}/billing/checkout", existing.slug)
        } else {
            "/".to_string()
        };
        return Ok((updated_cookies, Redirect::to(&target)));
    }

    // Full availability check: existing orgs + pending verifications.
    // On "taken", prefill a suggested alternative for one-click retry.
    if !crate::slug::is_available(&db, org_slug).await {
        let suggestion = crate::slug::suggest_alternative(&db, org_slug).await;
        return Err(render_with(
            &format!(
                "{} Try \u{201c}{}\u{201d}.",
                crate::slug::SlugError::Taken.message(),
                suggestion
            ),
            &suggestion,
        ));
    }

    // Determine org name
    let org_name = if account_type == "organization" {
        form.org_name.as_deref().unwrap_or(org_slug).to_string()
    } else {
        session.user_name.clone()
    };

    // Create the org. `create_org` is idempotent for owned slugs and self-
    // heals partial failures, so this Err branch only fires when the slug
    // is genuinely owned by someone else (race against another signup).
    let create_result = create_org(
        &db,
        &session.current_user_id(),
        &org_name,
        org_slug,
        account_type,
    )
    .await;
    let org_id = match create_result {
        Ok(id) => id,
        Err(e) => {
            tracing::error!("Failed to create org during claim-handle: {}", e);
            let err_str = e.as_str();
            let looks_taken = err_str.contains("already taken")
                || err_str.contains("org_slug_key")
                || err_str.contains("duplicate key");
            return Err(if looks_taken {
                let suggestion = crate::slug::suggest_alternative(&db, org_slug).await;
                render_with(
                    &format!(
                        "{} Try \u{201c}{}\u{201d}.",
                        crate::slug::SlugError::Taken.message(),
                        suggestion
                    ),
                    &suggestion,
                )
            } else {
                render_error("Failed to create your account. Please try again.")
            });
        }
    };

    // Set the newly created org as current
    let updated_cookies = cookies.add(crate::auth::build_cookie(
        crate::auth::CURRENT_ORG_COOKIE_NAME,
        org_id.to_string(),
        time::Duration::days(365),
    ));

    // Redirect to billing (slug-scoped, so the next request doesn't depend
    // on the org cookie surviving the redirect) if a plan was selected,
    // otherwise the dashboard.
    if !plan.is_empty() {
        Ok((
            updated_cookies,
            Redirect::to(&format!(
                "/@{}/billing/checkout?plan={}&billing={}",
                org_slug, plan, billing
            )),
        ))
    } else if session.billing_enabled {
        Ok((
            updated_cookies,
            Redirect::to(&format!("/@{}/billing/checkout", org_slug)),
        ))
    } else {
        Ok((updated_cookies, Redirect::to("/")))
    }
}

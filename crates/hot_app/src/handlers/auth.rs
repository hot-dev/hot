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
use serde::Deserialize;
use std::sync::Arc;
use time;

// Import common functions from parent module
use super::{
    add_presence_cookie, authenticate_user, create_org, process_invite_code,
    remove_presence_cookie, set_default_org_env_cookies,
};

// Form data structure for signin
#[derive(Deserialize, Debug)]
pub struct SigninForm {
    pub email: String,
    pub password: String,
    pub next: Option<String>,
    pub form_token: Option<String>, // CSRF double-submit token
}

// Form data structure for user signup
#[derive(Deserialize, Debug)]
pub struct SignupForm {
    pub email: String,
    pub password: String,
    pub name: Option<String>,
    // Anti-bot fields
    pub website: Option<String>, // Honeypot field - should always be empty
    pub form_token: Option<String>, // CSRF double-submit token
}

pub async fn signin_handler(
    Extension(conf): Extension<Val>,
    cookies: CookieJar,
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

    let csrf_token = crate::auth::generate_csrf_token();
    let updated_cookies = cookies.add(crate::auth::build_csrf_cookie(csrf_token.clone()));

    let template = templates::SignIn {
        title: "Sign In",
        page_context: templates::PublicPageContext::new_with_conf("signin", &conf),
        error_message: "",
        invite_code: invite_code.as_str(),
        next: &next,
        plan: &plan,
        billing: &billing,
        form_token: &csrf_token,
    };

    (updated_cookies, Html(template.render().unwrap())).into_response()
}

pub async fn signin_post_handler(
    State(db): State<Arc<DatabasePool>>,
    Extension(conf): Extension<Val>,
    cookies: CookieJar,
    Query(params): Query<AHashMap<String, String>>,
    Form(form): Form<SigninForm>,
) -> Result<(CookieJar, Redirect), Html<String>> {
    let invite_code = params.get("invite_code").cloned().unwrap_or_default();
    let plan = params.get("plan").cloned().unwrap_or_default();
    let billing = params.get("billing").cloned().unwrap_or_default();

    // The CSRF cookie value is also what error re-renders embed as the form
    // token, so the user can correct and resubmit without a page reload.
    let csrf_cookie_value = cookies
        .get(crate::auth::CSRF_COOKIE_NAME)
        .map(|c| c.value().to_string())
        .unwrap_or_default();

    // Error re-renders preserve plan/billing so a user who arrived from the
    // pricing page doesn't lose their plan selection on a typo'd password.
    let render_signin_error = |error_message: &str| {
        let template = templates::SignIn {
            title: "Sign In",
            page_context: templates::PublicPageContext::new_with_conf("signin", &conf),
            error_message,
            invite_code: invite_code.as_str(),
            next: form.next.as_deref().unwrap_or(""),
            plan: &plan,
            billing: &billing,
            form_token: &csrf_cookie_value,
        };
        Html(template.render().unwrap())
    };

    // CSRF double-submit check
    if !crate::auth::validate_csrf(&cookies, form.form_token.as_deref()) {
        tracing::warn!("CSRF validation failed on signin for email: {}", form.email);
        return Err(render_signin_error(
            "Your session expired. Please try signing in again.",
        ));
    }

    // Per-email rate limit. Keyed on the target account (not the client IP,
    // which isn't trustworthy behind the CDN/LB chain) so a credential-
    // stuffing run against one mailbox locks that mailbox's attempts, not
    // the whole site. Counted before the password check so failed and
    // successful attempts both consume budget.
    if let Err(retry_after) = crate::rate_limit::check_signin(&conf, &form.email) {
        tracing::warn!(
            key_type = "email",
            key_hash = crate::rate_limit::key_fingerprint(&form.email),
            "Signin rate limit hit"
        );
        return Err(render_signin_error(&format!(
            "Too many sign-in attempts for this email. Please try again in {} minutes.",
            retry_after.div_ceil(60).max(1)
        )));
    }

    match authenticate_user(&db, &form.email, &form.password).await {
        Ok(user) => {
            // Process invite code if provided. On failure, sign the user in
            // anyway but land them on the invite page, which explains why
            // the invite could not be applied (mismatched email, expired, …).
            let mut invite_error_redirect: Option<String> = None;
            if !invite_code.is_empty()
                && let Err(e) = process_invite_code(&db, &user.user_id, &invite_code).await
            {
                tracing::warn!("Invite processing failed during signin: {}", e);
                invite_error_redirect = Some(format!("/invite?code={}", invite_code));
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

                    // Authentication successful. Invite problems take
                    // priority, then `next` if it's a safe same-site path,
                    // otherwise dashboard.
                    let redirect_to = invite_error_redirect.as_deref().unwrap_or_else(|| {
                        form.next
                            .as_deref()
                            .filter(|n| crate::auth::is_safe_next(n))
                            .unwrap_or("/")
                    });
                    Ok((final_cookies, Redirect::to(redirect_to)))
                }
                Err(err) => {
                    // Token generation failed
                    Err(render_signin_error(&format!(
                        "Authentication failed: {}",
                        err
                    )))
                }
            }
        }
        Err(error_message) => {
            // Authentication failed, show signin page with error
            Err(render_signin_error(&error_message))
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
    cookies: CookieJar,
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

    // CSRF double-submit token: cookie + hidden form field
    let csrf_token = crate::auth::generate_csrf_token();
    let updated_cookies = cookies.add(crate::auth::build_csrf_cookie(csrf_token.clone()));

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
        form_token: &csrf_token,
        show_signin_link: false,
    };

    (updated_cookies, Html(template.render().unwrap())).into_response()
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
    cookies: CookieJar,
    Query(params): Query<AHashMap<String, String>>,
    Form(form): Form<SignupForm>,
) -> Result<axum::response::Response, Html<String>> {
    let invite_code = params.get("invite_code").cloned().unwrap_or_default();
    let plan = params.get("plan").cloned().unwrap_or_default();
    let billing = params
        .get("billing")
        .cloned()
        .unwrap_or_else(|| "monthly".to_string());

    // Error re-renders embed the existing CSRF cookie value so the user can
    // correct and resubmit without reloading the page.
    let csrf_cookie_value = cookies
        .get(crate::auth::CSRF_COOKIE_NAME)
        .map(|c| c.value().to_string())
        .unwrap_or_default();

    // Helper to render error with all fields
    let render_error_with = |msg: &str, show_signin_link: bool| {
        render_signup_with_error_full(
            msg,
            &form.email,
            form.name.as_deref().unwrap_or(""),
            &invite_code,
            &plan,
            &billing,
            &csrf_cookie_value,
            show_signin_link,
        )
    };
    let render_error = |msg: &str| render_error_with(msg, false);

    if hot::product::billing_enabled(&conf) && invite_code.is_empty() && plan.is_empty() {
        return Err(render_error(
            "Please choose a plan before creating your account.",
        ));
    }

    // Anti-bot check 1: Honeypot field
    // If the hidden "website" field has a value, it's likely a bot
    // Silently return success to not tip off the bot
    if form.website.as_ref().is_some_and(|w| !w.is_empty()) {
        tracing::warn!(
            key_type = "email",
            key_hash = crate::rate_limit::key_fingerprint(&form.email),
            "Honeypot triggered on signup"
        );
        return Ok(
            render_check_email(&form.email, false, plan == "hot-free", &csrf_cookie_value)
                .into_response(),
        );
    }

    // CSRF double-submit check (also catches bots that POST without ever
    // loading the form, since they won't have the cookie)
    if !crate::auth::validate_csrf(&cookies, form.form_token.as_deref()) {
        tracing::warn!("CSRF validation failed on signup for email: {}", form.email);
        return Err(render_error(
            "Your session expired. Please try submitting the form again.",
        ));
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

    // Name is always required
    if form.name.as_deref().unwrap_or("").trim().is_empty() {
        return Err(render_error("Full name is required"));
    }

    // Check for an existing account FIRST. If the user is already registered,
    // the right answer is "go sign in".
    if User::get_user_by_email(&db, &form.email).await.is_ok() {
        return Err(render_error_with(
            "A user with this email already exists.",
            true,
        ));
    }

    // A user who refreshes the signup form (same email) just gets re-shown
    // the "check your email" page instead of starting over.
    if let Ok(Some(existing)) = EmailVerification::get_pending_by_email(&db, &form.email).await
        && existing.is_valid().is_ok()
    {
        return Ok(
            render_check_email(&form.email, true, plan == "hot-free", &csrf_cookie_value)
                .into_response(),
        );
    }

    if crate::rate_limit::check_signup_email(&conf, &form.email).is_err() {
        tracing::warn!(
            key_type = "email",
            key_hash = crate::rate_limit::key_fingerprint(&form.email),
            "Signup per-email rate limit hit"
        );
        return Err(render_error(
            "Too many signup attempts for this email. Please try again in a little while.",
        ));
    }

    // Global signup cap — a backstop against mass account creation that
    // slips past the edge per-IP limits. Count only plausible new-account
    // attempts, after CSRF/honeypot/validation have filtered junk traffic.
    if crate::rate_limit::check_signup_global(&conf).is_err() {
        tracing::warn!(key_type = "global", "Global signup rate limit hit");
        return Err(render_error(
            "We're receiving an unusually high volume of signups right now. Please try again in a little while.",
        ));
    }

    // Hash the password
    let password_hash = hot::auth::hash_password_with_random_salt(&form.password)
        .map_err(|_| render_error("Failed to process your request. Please try again."))?;

    // Invite fast path: when the signup email exactly matches the invite
    // email, the invite delivery already proved the user controls this
    // mailbox — skip the verification email and log them straight in.
    // Mismatched emails fall through to normal verification (and the
    // email-binding check in process_invite_code will reject the invite).
    if !invite_code.is_empty()
        && let Ok(invite) = hot::db::invite::Invite::get_invite_by_code(&db, &invite_code).await
        && invite.is_valid().is_ok()
        && invite.email.eq_ignore_ascii_case(form.email.trim())
    {
        let user_id =
            create_user_with_password(&db, &form.email, form.name.as_deref(), &password_hash)
                .await
                .map_err(|e| {
                    tracing::error!("Invite fast-path user creation failed: {}", e);
                    render_error("Failed to create your account. Please try again.")
                })?;

        if let Err(e) = process_invite_code(&db, &user_id, &invite_code).await {
            // User exists but didn't join the org; show the reason and let
            // them sign in (they'll land on claim-handle without an org).
            tracing::warn!("Invite fast-path processing failed: {}", e);
            return Err(render_error(&format!(
                "Your account was created, but the invite could not be applied: {}",
                e
            )));
        }

        let final_cookies = sign_in_cookies(&db, &conf, cookies, &user_id)
            .await
            .map_err(|_| render_error("Your account was created. Please sign in to continue."))?;

        tracing::info!(
            "User {} signed up via matching invite (verification skipped)",
            form.email
        );
        return Ok((final_cookies, Redirect::to("/")).into_response());
    }

    // Generate verification token and ID
    let verification_id = uuid::Uuid::now_v7();
    let verification_token = EmailVerification::generate_token();
    let expires_at = Utc::now() + chrono::Duration::hours(24);

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
        if plan.is_empty() { None } else { Some(&plan) },
        if plan.is_empty() {
            None
        } else {
            Some(&billing)
        },
        expires_at,
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
    Ok(
        render_check_email(&form.email, false, plan == "hot-free", &csrf_cookie_value)
            .into_response(),
    )
}

/// Create a user + email_password auth row. `password_hash` is the JSON
/// payload produced by `hash_password_with_random_salt`.
async fn create_user_with_password(
    db: &DatabasePool,
    email: &str,
    name: Option<&str>,
    password_hash: &str,
) -> Result<uuid::Uuid, String> {
    let user_id = uuid::Uuid::now_v7();
    let user_auth_id = uuid::Uuid::now_v7();

    User::insert_user(db, &user_id, email, name, Some(&user_id))
        .await
        .map_err(|e| format!("insert_user failed: {:?}", e))?;

    let auth_data: serde_json::Value = serde_json::from_str(password_hash)
        .map_err(|e| format!("invalid password hash payload: {:?}", e))?;

    hot::db::user::UserAuth::insert_user_auth(
        db,
        &user_auth_id,
        &user_id,
        "email_password",
        email,
        Some(&auth_data),
        &user_id,
    )
    .await
    .map_err(|e| format!("insert_user_auth failed: {:?}", e))?;

    Ok(user_id)
}

/// Set the full sign-in cookie set for a user: JWT session cookie, default
/// org/env cookies (when none are set), and the cross-subdomain presence
/// cookie.
async fn sign_in_cookies(
    db: &DatabasePool,
    conf: &Val,
    cookies: CookieJar,
    user_id: &uuid::Uuid,
) -> Result<CookieJar, String> {
    let token = generate_token(user_id, conf)?;
    let updated_cookies = cookies.add(crate::auth::build_cookie(
        JWT_COOKIE_NAME,
        token,
        time::Duration::days(crate::auth::SESSION_COOKIE_DAYS),
    ));
    let cookies_with_org = set_default_org_env_cookies(db, user_id, updated_cookies.clone())
        .await
        .unwrap_or(updated_cookies);
    Ok(add_presence_cookie(cookies_with_org))
}

/// Render the "check your email" page
fn render_check_email(
    email: &str,
    already_pending: bool,
    is_free_plan: bool,
    form_token: &str,
) -> Html<String> {
    render_check_email_full(email, already_pending, is_free_plan, false, form_token)
}

/// Render the "check your email" page; `resend_capped` swaps the resend form
/// for an explanation of the resend limit.
fn render_check_email_full(
    email: &str,
    already_pending: bool,
    is_free_plan: bool,
    resend_capped: bool,
    form_token: &str,
) -> Html<String> {
    let template = templates::CheckEmail {
        title: "Check Your Email",
        page_context: templates::PublicPageContext::new("signup"),
        email,
        form_token,
        already_pending,
        is_free_plan,
        resend_capped,
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

    // If a user record already exists for this email, reuse it: either a
    // prior verify attempt partially completed, or someone signed up with
    // the same email between checks (truly rare).
    let user_id = match User::get_user_by_email(&db, &verification.email).await {
        Ok(existing) => existing.user_id,
        Err(_) => create_user_with_password(
            &db,
            &verification.email,
            Some(verification.name.as_deref().unwrap_or("User")),
            &verification.password_hash,
        )
        .await
        .map_err(|e| {
            tracing::error!("Failed to create user during verification: {}", e);
            render_verification_error(
                "Account creation failed",
                "Failed to create your account. Please try again.",
            )
        })?,
    };

    // Process invite code if provided. Failures (expired invite, email
    // mismatch, revoked) are surfaced instead of silently dropped — the
    // account is created either way, so tell the user what happened.
    let mut invite_joined = false;
    if let Some(invite_code) = &verification.invite_code {
        match process_invite_code(&db, &user_id, invite_code).await {
            Ok(()) => invite_joined = true,
            Err(e) => {
                tracing::warn!(
                    "verify_email_handler: invite processing failed for {}: {}",
                    verification.email,
                    e
                );
                let _ = EmailVerification::mark_verified(&db, &verification.verification_id).await;
                return Err(render_verification_error(
                    "Invite could not be applied",
                    &format!(
                        "Your email is verified and your account was created, but the invite \
                         could not be applied: {} You can sign in to continue.",
                        e
                    ),
                ));
            }
        }
    }

    let _ = EmailVerification::mark_verified(&db, &verification.verification_id).await;

    let final_cookies = sign_in_cookies(&db, &conf, cookies, &user_id)
        .await
        .map_err(|e| {
            tracing::error!("Failed to sign in during verification: {}", e);
            render_verification_error(
                "Account created",
                "Your account was created successfully! Please sign in.",
            )
        })?;

    tracing::info!("User {} verified and logged in", verification.email);

    // Handles are claimed after verification: invite joiners already have an
    // org to land in, everyone else goes to /claim-handle (carrying plan
    // params so checkout follows the handle claim).
    let target = if invite_joined {
        "/".to_string()
    } else {
        match verification.plan.as_deref() {
            Some(plan) => format!(
                "/claim-handle?plan={}&billing={}",
                plan,
                verification.billing.as_deref().unwrap_or("monthly")
            ),
            None => "/claim-handle".to_string(),
        }
    };
    Ok((final_cookies, Redirect::to(&target)))
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

    let user_orgs = hot::db::org::Org::get_orgs_by_user(db, &user.user_id)
        .await
        .unwrap_or_default();
    let chosen_org = user_orgs.first().cloned();

    let final_cookies = sign_in_cookies(db, conf, cookies, &user.user_id)
        .await
        .map_err(|_| {
            render_verification_error(
                "Session error",
                "Your account is verified, but we couldn't sign you in. Please sign in.",
            )
        })?;

    // Decide where to send them next. Mirrors the first-time verify logic:
    //  - no org yet → /claim-handle (carrying plan params)
    //  - org + plan + no subscription yet → billing checkout (slug-scoped)
    //  - otherwise → dashboard
    let plan = verification.plan.as_deref().unwrap_or("");
    let billing = verification.billing.as_deref().unwrap_or("monthly");

    let target = match chosen_org.as_ref() {
        None if !plan.is_empty() => format!("/claim-handle?plan={}&billing={}", plan, billing),
        None => "/claim-handle".to_string(),
        Some(org) if !plan.is_empty() => {
            let has_subscription = hot::db::OrgPlan::get_by_org_id(db, &org.org_id)
                .await
                .is_ok();
            if has_subscription {
                "/".to_string()
            } else {
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
    cookies: CookieJar,
    Form(form): Form<ResendVerificationForm>,
) -> axum::response::Response {
    if !crate::auth::validate_csrf(&cookies, form.form_token.as_deref()) {
        tracing::warn!(
            key_type = "email",
            key_hash = crate::rate_limit::key_fingerprint(&form.email),
            "CSRF validation failed on resend verification"
        );
        let fresh_token = crate::auth::generate_csrf_token();
        let updated_cookies = cookies.add(crate::auth::build_csrf_cookie(fresh_token.clone()));
        return (
            updated_cookies,
            render_check_email_full(&form.email, true, false, false, &fresh_token),
        )
            .into_response();
    }

    let form_token = cookies
        .get(crate::auth::CSRF_COOKIE_NAME)
        .map(|c| c.value().to_string())
        .unwrap_or_default();

    // Per-email rate limit before any DB work; render the same page either
    // way so the endpoint doesn't reveal whether the email exists.
    if let Err(retry_after) = crate::rate_limit::check_resend(&conf, &form.email) {
        tracing::warn!(
            key_type = "email",
            key_hash = crate::rate_limit::key_fingerprint(&form.email),
            "Resend rate limit hit"
        );
        let _ = retry_after;
        return render_check_email_full(&form.email, true, false, true, &form_token)
            .into_response();
    }

    // Find the pending verification
    let verification = match EmailVerification::get_pending_by_email(&db, &form.email).await {
        Ok(Some(v)) => v,
        _ => {
            // Don't reveal whether email exists
            return render_check_email(&form.email, false, false, &form_token).into_response();
        }
    };

    // Check attempt limits (max 5 resends)
    if verification.attempts >= 5 {
        let is_free = verification.plan.as_deref() == Some("hot-free");
        return render_check_email_full(&form.email, true, is_free, true, &form_token)
            .into_response();
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
    render_check_email(&form.email, false, is_free, &form_token).into_response()
}

/// Form data for resend verification
#[derive(Deserialize, Debug)]
pub struct ResendVerificationForm {
    pub email: String,
    pub form_token: Option<String>,
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
    form_token: &str,
    show_signin_link: bool,
) -> Html<String> {
    let plan_display_name = get_plan_display_name(plan);
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
        form_token,
        show_signin_link,
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
    // snapshot first, then a fresh DB read (covers the org cookie not having
    // made it back to the browser yet).
    let existing_slug: Option<String> = if let Some(org) = &session.current_org {
        Some(org.slug.clone())
    } else {
        hot::db::org::Org::get_orgs_by_user(&db, &session.current_user_id())
            .await
            .ok()
            .and_then(|orgs| orgs.first().map(|o| o.slug.clone()))
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

    // Pre-fill the slug field with an available suggestion derived from the
    // user's name so they have a one-click path forward.
    // slugify: lowercase, replace non-alphanumeric runs with hyphens, trim hyphens.
    let extra_reserved = crate::slug::extra_reserved_from_conf(&conf);
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
    let suggested = if crate::slug::validate_format_with_extra(&base, &extra_reserved).is_ok() {
        crate::slug::suggest_available(&db, &base).await
    } else {
        String::new()
    };

    let template = templates::ClaimHandle {
        title: "Claim Your Handle",
        page_context: templates::PublicPageContext::new("claim-handle"),
        error_message: "",
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

    // If the user already has an org, don't create a duplicate — just
    // forward them. Keeps /claim-handle idempotent for users who already
    // have a handle, and means a refresh / double-submit / racing-tab can
    // never silently create a SECOND org.
    let user_has_org = if let Some(org) = &session.current_org {
        Some(org.slug.clone())
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

    if let Err(retry_after) =
        crate::rate_limit::check_claim_handle(&conf, &session.current_user_id())
    {
        tracing::warn!(
            key_type = "user",
            key_hash = crate::rate_limit::key_fingerprint(&session.current_user_id().to_string()),
            "Claim-handle rate limit hit"
        );
        return Err(render_error(&format!(
            "Too many handle claim attempts. Please try again in {} minutes.",
            retry_after.div_ceil(60).max(1)
        )));
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

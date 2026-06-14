//! HTTP-level integration tests for the signup → verify → claim-handle flow.
//!
//! Drives the real Axum router via `hot_app::test_support`, exercising every
//! handler hop a browser would traverse and asserting the redirect chain that
//! hands the user off to claim-handle and billing.
//!
//! These tests *complement* `signup_flow.rs` (DB-layer slug tests) by
//! verifying the full request/response contract end-to-end.

use chrono::{Duration, Utc};
use hot::db::{DatabasePool, EmailVerification};
use hot::val;
use hot_app::test_support::TestClient;
use uuid::Uuid;

/// Helpers
async fn read_pending_token(db: &DatabasePool, email: &str) -> String {
    let verification = EmailVerification::get_pending_by_email(db, email)
        .await
        .expect("query failed")
        .expect("no pending verification for email");
    verification.verification_token
}

/// Build a signup form. `form_token` must come from `client.prime_csrf()`
/// (CSRF double-submit: the cookie and the field have to match).
fn submit_signup(email: &str, form_token: String) -> Vec<(&'static str, String)> {
    vec![
        ("email", email.to_string()),
        ("password", "supersecret123".to_string()),
        ("name", "Alice Example".to_string()),
        ("website", String::new()), // honeypot (empty = human)
        ("form_token", form_token),
    ]
}

/// Convert `Vec<(&'static str, String)>` into the `&[(&str, &str)]` shape
/// expected by `TestClient::post_form`.
fn as_form_slice<'a>(form: &'a [(&'static str, String)]) -> Vec<(&'a str, &'a str)> {
    form.iter().map(|(k, v)| (*k, v.as_str())).collect()
}

fn hot_cloud_billing_conf() -> hot::val::Val {
    val!({
        "app": {
            "host": "localhost",
            "port": 4680
        },
        "product": {
            "experience": "hot-cloud"
        },
        "billing": {
            "enabled": true
        }
    })
}

fn conf_with_signup_email_limit(max: i64) -> hot::val::Val {
    val!({
        "app": {
            "host": "localhost",
            "port": 4680,
            "abuse-limits": {
                "signup-email": {
                    "max": max,
                    "window-secs": 60,
                },
            },
        },
    })
}

// ---------------------------------------------------------------------------
// Signup → pending verification row
// ---------------------------------------------------------------------------

#[tokio::test]
async fn hot_cloud_signup_without_plan_redirects_to_plan_picker() {
    let mut client = TestClient::new_with_conf(hot_cloud_billing_conf()).await;

    let resp = client.get("/signup").await;

    resp.assert_redirect_to("/signup/plans");
}

#[tokio::test]
async fn hot_cloud_signup_post_without_plan_is_rejected() {
    let mut client = TestClient::new_with_conf(hot_cloud_billing_conf()).await;

    let form = submit_signup("alice@example.com", client.prime_csrf());
    let resp = client.post_form("/signup", &as_form_slice(&form)).await;

    resp.assert_status(axum::http::StatusCode::OK);
    assert!(
        resp.body.contains("Please choose a plan"),
        "expected no-plan signup error, body snippet = {}",
        resp.body.chars().take(400).collect::<String>()
    );

    let pending = EmailVerification::get_pending_by_email(client.db(), "alice@example.com")
        .await
        .expect("query");
    assert!(
        pending.is_none(),
        "no-plan Hot Cloud signup must not create a verification row"
    );
}

#[tokio::test]
async fn signup_creates_pending_verification_and_renders_check_email() {
    let mut client = TestClient::new().await;

    let form = submit_signup("alice@example.com", client.prime_csrf());
    let resp = client
        .post_form(
            "/signup?plan=hot-free&billing=monthly",
            &as_form_slice(&form),
        )
        .await;

    resp.assert_status(axum::http::StatusCode::OK);
    assert!(
        resp.body.contains("alice@example.com"),
        "check-email page should echo the submitted email, body was: {}",
        resp.body.chars().take(300).collect::<String>()
    );

    let verification = EmailVerification::get_pending_by_email(client.db(), "alice@example.com")
        .await
        .expect("query")
        .expect("pending verification row");
    assert_eq!(verification.plan.as_deref(), Some("hot-free"));
    assert!(verification.expires_at > Utc::now() + Duration::hours(23));
}

#[tokio::test]
async fn duplicate_signup_reshows_check_email_page() {
    let mut client = TestClient::new().await;

    let form = submit_signup("alice@example.com", client.prime_csrf());
    client
        .post_form(
            "/signup?plan=hot-free&billing=monthly",
            &as_form_slice(&form),
        )
        .await
        .assert_status(axum::http::StatusCode::OK);

    // Submitting again with the same email must not create a second row and
    // should re-show the check-email page.
    let form2 = submit_signup("alice@example.com", client.prime_csrf());
    let resp = client
        .post_form(
            "/signup?plan=hot-free&billing=monthly",
            &as_form_slice(&form2),
        )
        .await;
    resp.assert_status(axum::http::StatusCode::OK);
    assert!(
        resp.body.contains("alice@example.com"),
        "expected check-email page on duplicate signup"
    );
}

#[tokio::test]
async fn signup_post_without_csrf_token_is_rejected() {
    let mut client = TestClient::new().await;

    // No prime_csrf(): neither the cookie nor the field is present.
    let form = vec![
        ("email", "alice@example.com".to_string()),
        ("password", "supersecret123".to_string()),
        ("name", "Alice Example".to_string()),
        ("website", String::new()),
    ];
    let resp = client
        .post_form(
            "/signup?plan=hot-free&billing=monthly",
            &as_form_slice(&form),
        )
        .await;

    resp.assert_status(axum::http::StatusCode::OK);
    assert!(
        resp.body.contains("session expired") || resp.body.contains("try submitting"),
        "missing CSRF token must re-render the form with an error, got: {}",
        resp.body.chars().take(400).collect::<String>()
    );
    assert!(
        EmailVerification::get_pending_by_email(client.db(), "alice@example.com")
            .await
            .expect("query")
            .is_none(),
        "no verification row may be created without a valid CSRF token"
    );
}

#[tokio::test]
async fn signup_per_email_limit_blocks_after_expired_pending_attempt() {
    let mut client = TestClient::new_with_conf(conf_with_signup_email_limit(1)).await;
    let email = format!("limit-{}@example.com", Uuid::now_v7());

    let form = submit_signup(&email, client.prime_csrf());
    client
        .post_form(
            "/signup?plan=hot-free&billing=monthly",
            &as_form_slice(&form),
        )
        .await
        .assert_status(axum::http::StatusCode::OK);

    let verification = EmailVerification::get_pending_by_email(client.db(), &email)
        .await
        .expect("query")
        .expect("pending row");
    EmailVerification::mark_expired(client.db(), &verification.verification_id)
        .await
        .expect("expire pending row");

    let form2 = submit_signup(&email, client.prime_csrf());
    let resp = client
        .post_form(
            "/signup?plan=hot-free&billing=monthly",
            &as_form_slice(&form2),
        )
        .await;

    resp.assert_status(axum::http::StatusCode::OK);
    assert!(
        resp.body.contains("Too many signup attempts"),
        "expected per-email signup limiter, got: {}",
        resp.body.chars().take(500).collect::<String>()
    );
}

#[tokio::test]
async fn duplicate_email_signup_offers_signin_link() {
    let mut client = TestClient::new().await;

    let user_id = Uuid::now_v7();
    hot::db::User::insert_user(
        client.db(),
        &user_id,
        "alice@example.com",
        Some("Alice"),
        Some(&user_id),
    )
    .await
    .expect("insert user");

    let form = submit_signup("alice@example.com", client.prime_csrf());
    let resp = client
        .post_form(
            "/signup?invite_code=test-invite-code&plan=hot-free&billing=monthly",
            &as_form_slice(&form),
        )
        .await;

    resp.assert_status(axum::http::StatusCode::OK);
    assert!(
        resp.body.contains("already exists"),
        "expected duplicate-email error"
    );
    assert!(
        resp.body.contains("Sign in instead")
            && resp.body.contains("/signin?invite_code=test-invite-code")
            && resp.body.contains("plan=hot-free")
            && resp.body.contains("billing=monthly"),
        "duplicate-email error must link to signin preserving invite and plan params, got: {}",
        resp.body.chars().take(600).collect::<String>()
    );
}

// ---------------------------------------------------------------------------
// Invite fast path: matching email skips verification entirely
// ---------------------------------------------------------------------------

#[tokio::test]
async fn invite_signup_with_matching_email_skips_verification() {
    let mut client = TestClient::new().await;

    // Org admin invites bob@example.com.
    let admin_id = Uuid::now_v7();
    hot::db::User::insert_user(
        client.db(),
        &admin_id,
        "admin@example.com",
        Some("Admin"),
        Some(&admin_id),
    )
    .await
    .expect("insert admin");
    let org_id =
        hot_app::handlers::create_org(client.db(), &admin_id, "Acme", "acme", "organization")
            .await
            .expect("create org");
    hot::db::Invite::insert_invite(
        client.db(),
        &Uuid::now_v7(),
        "test-invite-code",
        "bob@example.com",
        &org_id,
        1,
        &admin_id,
        Utc::now() + Duration::days(7),
    )
    .await
    .expect("insert invite");

    // Bob signs up with the exact invite email: no verification email, he is
    // signed in immediately and lands in the org.
    let form = vec![
        ("email", "bob@example.com".to_string()),
        ("password", "supersecret123".to_string()),
        ("name", "Bob Example".to_string()),
        ("website", String::new()),
        ("form_token", client.prime_csrf()),
    ];
    let resp = client
        .post_form(
            "/signup?invite_code=test-invite-code",
            &as_form_slice(&form),
        )
        .await;

    assert!(
        resp.status.is_redirection(),
        "matching-email invite signup should redirect (logged in), got {}: {}",
        resp.status,
        resp.body.chars().take(300).collect::<String>()
    );
    assert!(
        client.cookies().get("hot_auth_token").is_some(),
        "invite fast path must set the session cookie"
    );
    assert!(
        EmailVerification::get_pending_by_email(client.db(), "bob@example.com")
            .await
            .expect("query")
            .is_none(),
        "no pending verification row may exist for the invite fast path"
    );

    // Bob is a member of the org.
    let bob = hot::db::User::get_user_by_email(client.db(), "bob@example.com")
        .await
        .expect("bob exists");
    let orgs = hot::db::org::Org::get_orgs_by_user(client.db(), &bob.user_id)
        .await
        .expect("query orgs");
    assert!(
        orgs.iter().any(|o| o.org_id == org_id),
        "invite fast path must add the user to the inviting org"
    );
}

#[tokio::test]
async fn invite_signup_with_mismatched_email_falls_back_to_verification() {
    let mut client = TestClient::new().await;

    let admin_id = Uuid::now_v7();
    hot::db::User::insert_user(
        client.db(),
        &admin_id,
        "admin2@example.com",
        Some("Admin"),
        Some(&admin_id),
    )
    .await
    .expect("insert admin");
    let org_id =
        hot_app::handlers::create_org(client.db(), &admin_id, "Bcme", "bcme", "organization")
            .await
            .expect("create org");
    hot::db::Invite::insert_invite(
        client.db(),
        &Uuid::now_v7(),
        "mismatch-invite-code",
        "carol@example.com",
        &org_id,
        1,
        &admin_id,
        Utc::now() + Duration::days(7),
    )
    .await
    .expect("insert invite");

    // Signing up with a DIFFERENT email goes through normal verification.
    let form = vec![
        ("email", "mallory@example.com".to_string()),
        ("password", "supersecret123".to_string()),
        ("name", "Mallory".to_string()),
        ("website", String::new()),
        ("form_token", client.prime_csrf()),
    ];
    let resp = client
        .post_form(
            "/signup?invite_code=mismatch-invite-code",
            &as_form_slice(&form),
        )
        .await;

    resp.assert_status(axum::http::StatusCode::OK);
    assert!(
        client.cookies().get("hot_auth_token").is_none(),
        "mismatched invite email must NOT be signed in directly"
    );
    assert!(
        EmailVerification::get_pending_by_email(client.db(), "mallory@example.com")
            .await
            .expect("query")
            .is_some(),
        "mismatched invite email must go through email verification"
    );
}

// ---------------------------------------------------------------------------
// Rate limiting & resend cap
// ---------------------------------------------------------------------------

#[tokio::test]
async fn signin_is_rate_limited_per_email() {
    let mut client = TestClient::new().await;

    // Unique email so the process-wide limiter doesn't collide with other
    // tests in this binary.
    let email = format!("ratelimit-{}@example.com", Uuid::now_v7());

    let mut last_body = String::new();
    // SIGNIN_MAX_PER_EMAIL is 10; the 11th attempt must be limited.
    for _ in 0..11 {
        let token = client.prime_csrf();
        let form = vec![
            ("email", email.clone()),
            ("password", "wrong-password".to_string()),
            ("form_token", token),
        ];
        let resp = client.post_form("/signin", &as_form_slice(&form)).await;
        last_body = resp.body;
    }

    assert!(
        last_body.contains("Too many sign-in attempts"),
        "11th signin for one email must be rate limited, got: {}",
        last_body.chars().take(400).collect::<String>()
    );
}

#[tokio::test]
async fn resend_cap_renders_explicit_messaging() {
    let mut client = TestClient::new().await;

    let form = submit_signup("alice@example.com", client.prime_csrf());
    client
        .post_form(
            "/signup?plan=hot-free&billing=monthly",
            &as_form_slice(&form),
        )
        .await;

    // Exhaust the per-row resend cap (5 attempts).
    let verification = EmailVerification::get_pending_by_email(client.db(), "alice@example.com")
        .await
        .expect("query")
        .expect("pending row");
    for _ in 0..5 {
        EmailVerification::increment_attempts(client.db(), &verification.verification_id)
            .await
            .expect("increment attempts");
    }

    let token = client.prime_csrf();
    let resp = client
        .post_form(
            "/resend-verification",
            &[
                ("email", "alice@example.com"),
                ("form_token", token.as_str()),
            ],
        )
        .await;

    resp.assert_status(axum::http::StatusCode::OK);
    assert!(
        resp.body.contains("resend limit"),
        "capped resend must explain the limit instead of silently re-showing \
         the page, got: {}",
        resp.body.chars().take(400).collect::<String>()
    );
    assert!(
        !resp.body.contains("Resend verification email"),
        "the resend button should be hidden once the cap is reached"
    );
}

#[tokio::test]
async fn resend_without_csrf_rerenders_with_fresh_retry_form() {
    let mut client = TestClient::new().await;

    let form = submit_signup("csrf-resend@example.com", client.prime_csrf());
    client
        .post_form(
            "/signup?plan=hot-free&billing=monthly",
            &as_form_slice(&form),
        )
        .await;

    let verification =
        EmailVerification::get_pending_by_email(client.db(), "csrf-resend@example.com")
            .await
            .expect("query")
            .expect("pending row");
    assert_eq!(verification.attempts, 0);

    client.clear_cookies();
    let resp = client
        .post_form(
            "/resend-verification",
            &[("email", "csrf-resend@example.com")],
        )
        .await;

    resp.assert_status(axum::http::StatusCode::OK);
    assert!(
        resp.body.contains("Resend verification email")
            && resp.body.contains("name=\"form_token\""),
        "missing-CSRF resend should show a fresh retry form, got: {}",
        resp.body.chars().take(500).collect::<String>()
    );

    let verification =
        EmailVerification::get_pending_by_email(client.db(), "csrf-resend@example.com")
            .await
            .expect("query")
            .expect("pending row");
    assert_eq!(
        verification.attempts, 0,
        "CSRF-invalid resend must not consume DB resend attempts"
    );
}

// ---------------------------------------------------------------------------
// Happy path: verify → claim-handle (handles are claimed post-verification)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn verify_happy_path_logs_in_and_redirects_to_claim_handle() {
    let mut client = TestClient::new().await;

    let form = submit_signup("alice@example.com", client.prime_csrf());
    client
        .post_form(
            "/signup?plan=hot-free&billing=monthly",
            &as_form_slice(&form),
        )
        .await
        .assert_status(axum::http::StatusCode::OK);

    let token = read_pending_token(client.db(), "alice@example.com").await;

    // Click the verification link: log in and forward to claim-handle with
    // the plan params preserved.
    let verify_resp = client.get(&format!("/verify-email?token={}", token)).await;
    verify_resp.assert_redirect_to("/claim-handle?plan=hot-free&billing=monthly");

    // JWT cookie should now be set so the user is logged in on the next hop.
    assert!(
        client.cookies().get("hot_auth_token").is_some(),
        "expected hot_auth_token cookie after verify, jar = {:?}",
        client.cookies()
    );

    // User exists but has no org yet — the handle hasn't been claimed.
    let alice = hot::db::User::get_user_by_email(client.db(), "alice@example.com")
        .await
        .expect("alice user");
    let orgs = hot::db::org::Org::get_orgs_by_user(client.db(), &alice.user_id)
        .await
        .expect("query orgs");
    assert!(orgs.is_empty(), "no org should exist before claim-handle");
}

// ---------------------------------------------------------------------------
// Claim-handle POST → billing checkout
// ---------------------------------------------------------------------------

#[tokio::test]
async fn claim_handle_post_creates_org_and_redirects_to_billing() {
    let mut client = TestClient::new().await;

    let form = submit_signup("alice@example.com", client.prime_csrf());
    client
        .post_form(
            "/signup?plan=hot-free&billing=monthly",
            &as_form_slice(&form),
        )
        .await;

    let token = read_pending_token(client.db(), "alice@example.com").await;
    client.get(&format!("/verify-email?token={}", token)).await;

    // Alice claims her handle.
    let claim_form = vec![
        ("account_type", "individual".to_string()),
        ("org_name", String::new()),
        ("org_slug", "alice".to_string()),
    ];
    let claim_resp = client
        .post_form(
            "/claim-handle?plan=hot-free&billing=monthly",
            &as_form_slice(&claim_form),
        )
        .await;

    // Slug-scoped redirect — never bounce through /billing/create-checkout-form.
    claim_resp.assert_redirect_to("/@alice/billing/checkout?plan=hot-free&billing=monthly");

    // And the org really got created.
    let org = hot::db::org::Org::get_org_by_slug(client.db(), "alice")
        .await
        .expect("alice org should exist");
    assert_eq!(org.slug, "alice");
    assert_eq!(org.org_type, "individual");
}

#[tokio::test]
async fn claim_handle_suggests_alternative_when_slug_taken() {
    let mut client = TestClient::new().await;

    // Someone already owns "alice".
    let other_user_id = Uuid::now_v7();
    hot::db::User::insert_user(
        client.db(),
        &other_user_id,
        "sneaky@example.com",
        Some("Sneaky"),
        Some(&other_user_id),
    )
    .await
    .expect("insert other user");
    hot_app::handlers::create_org(client.db(), &other_user_id, "Sneaky", "alice", "individual")
        .await
        .expect("create_org for squatter");

    // Alice signs up, verifies, then tries to claim "alice".
    let form = submit_signup("alice@example.com", client.prime_csrf());
    client
        .post_form(
            "/signup?plan=hot-free&billing=monthly",
            &as_form_slice(&form),
        )
        .await;
    let token = read_pending_token(client.db(), "alice@example.com").await;
    client.get(&format!("/verify-email?token={}", token)).await;

    let claim_form = vec![
        ("account_type", "individual".to_string()),
        ("org_name", String::new()),
        ("org_slug", "alice".to_string()),
    ];
    let claim_resp = client
        .post_form(
            "/claim-handle?plan=hot-free&billing=monthly",
            &as_form_slice(&claim_form),
        )
        .await;

    // Form re-renders with an alternative suggestion — never echoes the
    // taken slug as the suggestion.
    claim_resp.assert_status(axum::http::StatusCode::OK);
    assert!(
        claim_resp.body.contains("alice-2"),
        "expected suggestion `alice-2` in claim-handle error body; got snippet = {}",
        claim_resp.body.chars().take(400).collect::<String>()
    );
}

// ---------------------------------------------------------------------------
// Idempotent verify: re-clicking a verified link (mail scanner pre-fetch)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn verify_link_is_idempotent_across_multiple_clicks() {
    let mut client = TestClient::new().await;

    let form = submit_signup("alice@example.com", client.prime_csrf());
    client
        .post_form(
            "/signup?plan=hot-free&billing=monthly",
            &as_form_slice(&form),
        )
        .await;

    let token = read_pending_token(client.db(), "alice@example.com").await;

    // First click: creates the user, logs in, forwards to claim-handle.
    let first = client.get(&format!("/verify-email?token={}", token)).await;
    first.assert_redirect_to("/claim-handle?plan=hot-free&billing=monthly");

    // Second click on the same token (e.g. mail scanner pre-fetch, or user
    // reopening the email). Must NOT error out — `handle_already_verified`
    // logs the user back in and forwards to the same destination.
    let second = client.get(&format!("/verify-email?token={}", token)).await;
    assert!(
        second.status.is_redirection(),
        "re-clicking verify should redirect, got {}: {}",
        second.status,
        second.body.chars().take(200).collect::<String>()
    );
    let loc = second.location().unwrap_or("");
    assert!(
        loc.starts_with("/claim-handle") || loc == "/" || loc.starts_with("/@"),
        "re-click Location should be claim-handle/dashboard/org page, got `{}`",
        loc
    );
}

/// A verified token whose verification window has passed must NOT act as a
/// login link anymore — the idempotent re-login is only for the original
/// window (mail scanners / second browsers), not a permanent credential.
#[tokio::test]
async fn expired_verified_token_no_longer_logs_in() {
    let mut client = TestClient::new().await;

    let form = submit_signup("alice@example.com", client.prime_csrf());
    client
        .post_form(
            "/signup?plan=hot-free&billing=monthly",
            &as_form_slice(&form),
        )
        .await;

    let token = read_pending_token(client.db(), "alice@example.com").await;
    client
        .get(&format!("/verify-email?token={}", token))
        .await
        .assert_redirect_to("/claim-handle?plan=hot-free&billing=monthly");

    // Force-expire the (now verified) verification row.
    let verification = EmailVerification::get_by_token(client.db(), &token)
        .await
        .expect("verification row");
    EmailVerification::update_token(
        client.db(),
        &verification.verification_id,
        &token,
        Utc::now() - Duration::hours(1),
    )
    .await
    .expect("force-expire");

    // Clear the session and re-click the link.
    client.clear_cookies();
    let resp = client.get(&format!("/verify-email?token={}", token)).await;

    resp.assert_status(axum::http::StatusCode::OK);
    assert!(
        resp.body.contains("Please sign in"),
        "expired verified token must render an error, not log in. Body: {}",
        resp.body.chars().take(300).collect::<String>()
    );
    assert!(
        client.cookies().get("hot_auth_token").is_none(),
        "no session cookie may be issued for an expired verified token"
    );
}

// ---------------------------------------------------------------------------
// Orphan recovery: user owns an `org` row but has no `org_user` membership
// ---------------------------------------------------------------------------

/// Regression for orphaned org recovery. `create_org` is idempotent for
/// owned slugs (adopts them) and `claim_handle_post_handler` adopts the
/// owned org directly, heals the missing `org_user` link, and forwards to
/// billing.
#[tokio::test]
async fn claim_handle_adopts_orphan_org_owned_by_user() {
    let mut client = TestClient::new().await;

    // Insert the user directly — we're simulating a partial signup where
    // the user exists but the org_user link was never made.
    let user_id = Uuid::now_v7();
    hot::db::User::insert_user(
        client.db(),
        &user_id,
        "alice@example.com",
        Some("Alice"),
        Some(&user_id),
    )
    .await
    .expect("insert user");

    // Insert the orphan `org` row. Note: only `insert_org`, no `insert_org_user`.
    let orphan_org_id = Uuid::now_v7();
    hot::db::org::Org::insert_org(
        client.db(),
        &orphan_org_id,
        "Alice",
        "alice",
        "individual",
        &user_id,
    )
    .await
    .expect("insert orphan org");

    // Sanity: the user currently has NO orgs (no org_user row).
    let pre_orgs = hot::db::org::Org::get_orgs_by_user(client.db(), &user_id)
        .await
        .expect("query orgs");
    assert!(
        pre_orgs.is_empty(),
        "orphan org must be invisible via membership"
    );

    // Alice is logged in but session.current_org is None.
    client.login_as(&user_id);

    // POST /claim-handle with the slug she already "owns" as an orphan.
    let form = vec![
        ("account_type", "individual".to_string()),
        ("org_name", String::new()),
        ("org_slug", "alice".to_string()),
    ];
    let resp = client
        .post_form(
            "/claim-handle?plan=hot-free&billing=monthly",
            &as_form_slice(&form),
        )
        .await;

    // Must redirect to billing, NOT show a "taken / try alice-2" error.
    resp.assert_redirect_to("/@alice/billing/checkout?plan=hot-free&billing=monthly");

    // org_user link was healed.
    let post_orgs = hot::db::org::Org::get_orgs_by_user(client.db(), &user_id)
        .await
        .expect("query orgs");
    assert_eq!(
        post_orgs.len(),
        1,
        "orphan org should now be linked to the user"
    );
    assert_eq!(post_orgs[0].slug, "alice");
    assert_eq!(post_orgs[0].org_id, orphan_org_id);

    // The org's id is unchanged — we adopted the existing row, not created
    // a new one.
    let current = hot::db::org::Org::get_org_by_slug(client.db(), "alice")
        .await
        .unwrap();
    assert_eq!(current.org_id, orphan_org_id);
}

/// Companion: if the user submits /claim-handle for a slug they already own
/// AND have a proper membership for (i.e. back-button after a successful
/// claim), just forward to billing — no duplicate insert, no error.
#[tokio::test]
async fn claim_handle_is_idempotent_for_already_owned_slug() {
    let mut client = TestClient::new().await;

    let user_id = Uuid::now_v7();
    hot::db::User::insert_user(
        client.db(),
        &user_id,
        "alice@example.com",
        Some("Alice"),
        Some(&user_id),
    )
    .await
    .expect("insert user");

    // Full, healthy org — including the org_user membership.
    let org_id =
        hot_app::handlers::create_org(client.db(), &user_id, "Alice", "alice", "individual")
            .await
            .expect("create_org");

    // Simulate a stale browser tab: logged in, but no `hot_current_org`
    // cookie. session_middleware will fall back to `user_orgs.first()`, so
    // current_org WILL resolve. The /claim-handle GET should forward
    // straight to billing.
    client.login_as(&user_id);

    let resp = client
        .get("/claim-handle?plan=hot-free&billing=monthly")
        .await;
    resp.assert_redirect_to("/@alice/billing/checkout?plan=hot-free&billing=monthly");

    // And POST does the same — idempotent even without current_org
    // resolving for some reason.
    let form = vec![
        ("account_type", "individual".to_string()),
        ("org_name", String::new()),
        ("org_slug", "alice".to_string()),
    ];
    let resp = client
        .post_form(
            "/claim-handle?plan=hot-free&billing=monthly",
            &as_form_slice(&form),
        )
        .await;
    resp.assert_redirect_to("/@alice/billing/checkout?plan=hot-free&billing=monthly");

    // No duplicate org was created.
    let orgs = hot::db::org::Org::get_orgs_by_user(client.db(), &user_id)
        .await
        .unwrap();
    assert_eq!(orgs.len(), 1);
    assert_eq!(orgs[0].org_id, org_id);
}

// ---------------------------------------------------------------------------
// Cookie-less fallbacks
// ---------------------------------------------------------------------------

/// Even if the org cookie somehow goes missing on the way to
/// `/billing/create-checkout-form`, the handler must NOT redirect to
/// `/claim-handle` if the user actually has an org. It should fall back to
/// a fresh DB read and route to the org's checkout.
#[tokio::test]
async fn checkout_form_falls_back_to_db_when_cookie_is_missing() {
    let mut client = TestClient::new().await;

    let user_id = Uuid::now_v7();
    hot::db::User::insert_user(
        client.db(),
        &user_id,
        "alice@example.com",
        Some("Alice"),
        Some(&user_id),
    )
    .await
    .expect("insert user");
    hot_app::handlers::create_org(client.db(), &user_id, "Alice", "alice", "individual")
        .await
        .expect("create_org");

    client.login_as(&user_id);

    let resp = client
        .get("/billing/create-checkout-form?plan=hot-free&billing=monthly")
        .await;

    let location = resp.location().unwrap_or("");
    assert!(
        location.starts_with("/@alice/billing/checkout"),
        "checkout_form_handler must route to /@alice/billing/checkout when \
         the user has an org, not bounce to /claim-handle. Got: {}",
        location
    );
}

/// The user is logged in, has no org cookie, and lands on /claim-handle
/// directly. If they *do* have an org, claim_handle_handler must not render
/// the form — it must redirect.
#[tokio::test]
async fn claim_handle_get_falls_back_to_db_when_cookie_is_missing() {
    let mut client = TestClient::new().await;

    let user_id = Uuid::now_v7();
    hot::db::User::insert_user(
        client.db(),
        &user_id,
        "alice@example.com",
        Some("Alice"),
        Some(&user_id),
    )
    .await
    .expect("insert user");
    hot_app::handlers::create_org(client.db(), &user_id, "Alice", "alice", "individual")
        .await
        .expect("create_org");

    client.login_as(&user_id);

    let resp = client
        .get("/claim-handle?plan=hot-free&billing=monthly")
        .await;

    let location = resp.location().unwrap_or("");
    assert!(
        location.starts_with("/@alice/billing/checkout"),
        "claim_handle GET must redirect away when the user already has an org, \
         even if the org cookie isn't set. Got: {}",
        location
    );
}

/// A logged-in user with no org should see the claim-handle form (typical
/// for new OAuth users and freshly-verified email users).
#[tokio::test]
async fn claim_handle_get_renders_form_for_user_without_org() {
    let mut client = TestClient::new().await;

    let user_id = Uuid::now_v7();
    hot::db::User::insert_user(
        client.db(),
        &user_id,
        "newoauth@example.com",
        Some("New OAuth User"),
        Some(&user_id),
    )
    .await
    .expect("insert user");

    client.login_as(&user_id);

    let resp = client
        .get("/claim-handle?plan=hot-free&billing=monthly")
        .await;

    resp.assert_status(axum::http::StatusCode::OK);
    assert!(
        resp.body.to_lowercase().contains("handle"),
        "should render the claim-handle form for users with no org. \
         Body snippet: {}",
        resp.body.chars().take(300).collect::<String>()
    );
}

#[tokio::test]
async fn oauth_claim_handle_without_plan_redirects_to_plan_selection() {
    let mut client = TestClient::new_with_conf(hot_cloud_billing_conf()).await;

    let user_id = Uuid::now_v7();
    hot::db::User::insert_user(
        client.db(),
        &user_id,
        "newoauth@example.com",
        Some("New OAuth User"),
        Some(&user_id),
    )
    .await
    .expect("insert user");
    client.login_as(&user_id);

    let form = vec![
        ("account_type", "individual".to_string()),
        ("org_name", String::new()),
        ("org_slug", "new-oauth".to_string()),
    ];
    let resp = client
        .post_form("/claim-handle", &as_form_slice(&form))
        .await;

    resp.assert_redirect_to("/@new-oauth/billing/checkout");
}

#[tokio::test]
async fn hot_cloud_user_without_org_redirects_to_claim_handle() {
    let mut client = TestClient::new_with_conf(hot_cloud_billing_conf()).await;

    let user_id = Uuid::now_v7();
    hot::db::User::insert_user(
        client.db(),
        &user_id,
        "newoauth@example.com",
        Some("New OAuth User"),
        Some(&user_id),
    )
    .await
    .expect("insert user");
    client.login_as(&user_id);

    let resp = client.get("/").await;

    resp.assert_redirect_to("/claim-handle");
}

#[tokio::test]
async fn hot_cloud_org_without_plan_is_redirected_to_plan_selection() {
    let mut client = TestClient::new_with_conf(hot_cloud_billing_conf()).await;

    let user_id = Uuid::now_v7();
    hot::db::User::insert_user(
        client.db(),
        &user_id,
        "alice@example.com",
        Some("Alice"),
        Some(&user_id),
    )
    .await
    .expect("insert user");
    hot_app::handlers::create_org(client.db(), &user_id, "Alice", "alice", "individual")
        .await
        .expect("create org");
    client.login_as(&user_id);

    let resp = client.get("/").await;
    resp.assert_redirect_to("/@alice/billing/checkout");
}

#[tokio::test]
async fn non_admin_cannot_create_org_checkout() {
    let mut client = TestClient::new_with_conf(hot_cloud_billing_conf()).await;

    let admin_id = Uuid::now_v7();
    hot::db::User::insert_user(
        client.db(),
        &admin_id,
        "admin@example.com",
        Some("Admin"),
        Some(&admin_id),
    )
    .await
    .expect("insert admin");
    let org_id =
        hot_app::handlers::create_org(client.db(), &admin_id, "Acme", "acme", "organization")
            .await
            .expect("create org");

    let member_id = Uuid::now_v7();
    hot::db::User::insert_user(
        client.db(),
        &member_id,
        "member@example.com",
        Some("Member"),
        Some(&member_id),
    )
    .await
    .expect("insert member");
    hot::db::org::OrgUser::insert_org_user(
        client.db(),
        &Uuid::now_v7(),
        &org_id,
        &member_id,
        None,
        &admin_id,
    )
    .await
    .expect("insert member org_user");

    client.login_as(&member_id);

    client
        .get("/@acme/billing/checkout")
        .await
        .assert_status(axum::http::StatusCode::FORBIDDEN);

    let form = vec![
        ("plan_id", "hot-free".to_string()),
        ("billing_period", "monthly".to_string()),
    ];
    client
        .post_form("/@acme/billing/checkout", &as_form_slice(&form))
        .await
        .assert_status(axum::http::StatusCode::FORBIDDEN);
}

/// Hitting `/@<slug>/billing/checkout` for a slug that doesn't exist must
/// return 404 — handles are claimed explicitly at /claim-handle, never
/// auto-created from URLs.
#[tokio::test]
async fn org_checkout_form_returns_404_when_slug_does_not_exist() {
    let mut client = TestClient::new().await;

    let user_id = Uuid::now_v7();
    hot::db::User::insert_user(
        client.db(),
        &user_id,
        "stranger@example.com",
        Some("Stranger"),
        Some(&user_id),
    )
    .await
    .expect("insert user");

    client.login_as(&user_id);

    let resp = client
        .get("/@some-other-org/billing/checkout?plan=hot-free&billing=monthly")
        .await;

    resp.assert_status(axum::http::StatusCode::NOT_FOUND);
    assert!(
        hot::db::org::Org::get_org_by_slug(client.db(), "some-other-org")
            .await
            .is_err(),
        "must NOT have auto-created an unrelated slug"
    );
}

//! HTTP-level integration tests for the signup → verify → claim-handle flow.
//!
//! Drives the real Axum router via `hot_app::test_support`, exercising every
//! handler hop a browser would traverse and asserting the redirect chain that
//! hands the user off to billing/card-verification.
//!
//! These tests *complement* `signup_flow.rs` (DB-layer slug tests) by
//! verifying the full request/response contract end-to-end.

use chrono::{Duration, Utc};
use hot::db::{DatabasePool, EmailVerification};
use hot::val;
use hot_app::test_support::{TestClient, mint_form_token};
use uuid::Uuid;

/// Helpers
async fn read_pending_token(db: &DatabasePool, email: &str) -> String {
    let verification = EmailVerification::get_pending_by_email(db, email)
        .await
        .expect("query failed")
        .expect("no pending verification for email");
    verification.verification_token
}

fn submit_signup(email: &str, slug: &str) -> Vec<(&'static str, String)> {
    let form_token = mint_form_token();
    vec![
        ("email", email.to_string()),
        ("password", "supersecret123".to_string()),
        ("name", "Alice Example".to_string()),
        ("account_type", "individual".to_string()),
        ("org_slug", slug.to_string()),
        ("org_name", String::new()),
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

    let form = submit_signup("alice@example.com", "alice");
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

    let form = submit_signup("alice@example.com", "alice");
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
    assert_eq!(verification.org_slug.as_deref(), Some("alice"));
    assert_eq!(verification.plan.as_deref(), Some("hot-free"));
    assert_eq!(verification.account_type.as_deref(), Some("individual"));
}

// ---------------------------------------------------------------------------
// Happy path: verify → billing/create-checkout-form
// ---------------------------------------------------------------------------

#[tokio::test]
async fn verify_happy_path_redirects_to_billing_checkout() {
    let mut client = TestClient::new().await;

    let form = submit_signup("alice@example.com", "alice");
    client
        .post_form(
            "/signup?plan=hot-free&billing=monthly",
            &as_form_slice(&form),
        )
        .await
        .assert_status(axum::http::StatusCode::OK);

    let token = read_pending_token(client.db(), "alice@example.com").await;

    // Click the verification link.
    //
    // CRITICAL: this MUST redirect directly to `/@alice/billing/checkout` and
    // NOT to `/billing/create-checkout-form`. The latter goes through
    // `checkout_form_handler`, which depends on the org cookie surviving
    // the redirect to resolve `session.current_org`. If that cookie is absent,
    // the user can be bounced to `/claim-handle` and asked to pick a handle
    // they already chose.
    // Redirecting to the slug-scoped URL eliminates that whole class of bug.
    let verify_resp = client.get(&format!("/verify-email?token={}", token)).await;
    verify_resp.assert_redirect_to("/@alice/billing/checkout?plan=hot-free&billing=monthly");

    // JWT cookie should now be set so the user is logged in on the next hop.
    assert!(
        client.cookies().get("hot_auth_token").is_some(),
        "expected hot_auth_token cookie after verify, jar = {:?}",
        client.cookies()
    );
    // The org cookie should point at the newly-created org.
    assert!(
        client.cookies().get("hot_current_org").is_some(),
        "expected hot_current_org cookie after verify"
    );

    // Org actually exists in the DB with the requested slug.
    let org = hot::db::org::Org::get_org_by_slug(client.db(), "alice")
        .await
        .expect("org should exist after verify");
    assert_eq!(org.slug, "alice");
    assert_eq!(org.org_type, "individual");
}

// ---------------------------------------------------------------------------
// Race path: slug grabbed between signup and verify → claim-handle with ?taken
// ---------------------------------------------------------------------------

/// Race recovery path. Two hops:
///   1. `verify-email` hits a duplicate-key violation on `create_org` and
///      redirects to `/claim-handle?plan=hot-free&billing=monthly&taken=alice`.
///   2. `/claim-handle` (GET) renders a form pre-filled with a DIFFERENT slug
///      (`alice-2`) and an explanatory banner.
#[tokio::test]
async fn verify_race_redirects_to_claim_handle_with_taken_param() {
    let mut client = TestClient::new().await;

    // Alice submits signup.
    let form = submit_signup("alice@example.com", "alice");
    client
        .post_form(
            "/signup?plan=hot-free&billing=monthly",
            &as_form_slice(&form),
        )
        .await
        .assert_status(axum::http::StatusCode::OK);

    // Meanwhile, someone else creates an org with slug `alice` before Alice
    // verifies. Simulate that by inserting a user + org directly.
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

    // Alice clicks her verify link.
    let token = read_pending_token(client.db(), "alice@example.com").await;
    let verify_resp = client.get(&format!("/verify-email?token={}", token)).await;

    // Hop 1: verify → /claim-handle?taken=alice&plan=... (graceful-degrade).
    // The verify handler still emits the `?taken=` redirect so the caller has
    // explicit context about WHY they're at claim-handle. The fix-forward
    // recovery happens at the next hop.
    verify_resp.assert_redirect_to("/claim-handle?");
    let location = verify_resp.location().unwrap();
    assert!(
        location.contains("taken=alice"),
        "expected `taken=alice` in Location, got {}",
        location
    );
    assert!(
        location.contains("plan=hot-free"),
        "expected `plan=hot-free` in Location, got {}",
        location
    );

    // Alice is logged in already (so she can pick a new handle without a
    // second sign-in).
    assert!(
        client.cookies().get("hot_auth_token").is_some(),
        "expected JWT cookie after verify even in degraded path"
    );

    // Hop 2: follow to /claim-handle — fix-forward recovery kicks in. Alice
    // has a verification record carrying `org_slug=alice` but no actual org,
    // so claim_handle_handler auto-picks `alice-2`, creates that org, and
    // forwards to billing. She never sees the form.
    let claim_resp = client.follow_redirect(&verify_resp).await.expect("follow");
    let claim_loc = claim_resp.location().unwrap_or("");
    assert!(
        claim_loc.starts_with("/@alice-2/billing/checkout"),
        "claim-handle should auto-recover with alternative slug `alice-2` and \
         forward to billing — no form re-prompt for a handle she already chose. \
         Got: {}",
        claim_loc
    );

    // And the alternative org actually got created and is owned by Alice.
    let alice = hot::db::User::get_user_by_email(client.db(), "alice@example.com")
        .await
        .expect("alice user");
    let alice_orgs = hot::db::org::Org::get_orgs_by_user(client.db(), &alice.user_id)
        .await
        .expect("query alice orgs");
    assert_eq!(alice_orgs.len(), 1, "alice should have exactly one org");
    assert_eq!(alice_orgs[0].slug, "alice-2");
}

// ---------------------------------------------------------------------------
// Claim-handle POST → billing/create-checkout-form
// ---------------------------------------------------------------------------

#[tokio::test]
async fn claim_handle_post_redirects_to_billing_with_plan() {
    let mut client = TestClient::new().await;

    // Drive through signup + race so we're logged in and on /claim-handle.
    let form = submit_signup("alice@example.com", "alice");
    client
        .post_form(
            "/signup?plan=hot-free&billing=monthly",
            &as_form_slice(&form),
        )
        .await;

    // Squat the slug.
    let other_user_id = Uuid::now_v7();
    hot::db::User::insert_user(
        client.db(),
        &other_user_id,
        "sneaky@example.com",
        Some("Sneaky"),
        Some(&other_user_id),
    )
    .await
    .unwrap();
    hot_app::handlers::create_org(client.db(), &other_user_id, "Sneaky", "alice", "individual")
        .await
        .unwrap();

    let token = read_pending_token(client.db(), "alice@example.com").await;
    client.get(&format!("/verify-email?token={}", token)).await;

    // Now Alice submits her alternative handle via /claim-handle POST.
    let claim_form = vec![
        ("account_type", "individual".to_string()),
        ("org_name", String::new()),
        ("org_slug", "alice-2".to_string()),
    ];
    let claim_resp = client
        .post_form(
            "/claim-handle?plan=hot-free&billing=monthly",
            &as_form_slice(&claim_form),
        )
        .await;

    // Slug-scoped redirect — never bounce through /billing/create-checkout-form.
    claim_resp.assert_redirect_to("/@alice-2/billing/checkout?plan=hot-free&billing=monthly");

    // And the org really got created under the new slug.
    let org = hot::db::org::Org::get_org_by_slug(client.db(), "alice-2")
        .await
        .expect("alice-2 org should exist");
    assert_eq!(org.slug, "alice-2");
    assert_eq!(org.org_type, "individual");
}

// ---------------------------------------------------------------------------
// Idempotent verify: re-clicking a verified link (mail scanner pre-fetch)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn verify_link_is_idempotent_across_multiple_clicks() {
    let mut client = TestClient::new().await;

    let form = submit_signup("alice@example.com", "alice");
    client
        .post_form(
            "/signup?plan=hot-free&billing=monthly",
            &as_form_slice(&form),
        )
        .await;

    let token = read_pending_token(client.db(), "alice@example.com").await;

    // First click: creates org + logs in + redirects DIRECTLY to the
    // slug-scoped billing checkout (not via /billing/create-checkout-form).
    let first = client.get(&format!("/verify-email?token={}", token)).await;
    first.assert_redirect_to("/@alice/billing/checkout?plan=hot-free&billing=monthly");

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
    // Must redirect somewhere sensible — either billing (plan present) or
    // the dashboard. Must NOT leave the user on an error page.
    let loc = second.location().unwrap_or("");
    assert!(
        loc.starts_with("/billing/") || loc == "/" || loc.starts_with("/@"),
        "re-click Location should be billing/dashboard/org page, got `{}`",
        loc
    );
}

// ---------------------------------------------------------------------------
// Orphan recovery: user owns an `org` row but has no `org_user` membership
// ---------------------------------------------------------------------------

/// Regression for orphaned org recovery. Previously, `create_org` wasn't transactional
/// and left orphan `org` rows behind on partial failures. A logged-in user
/// whose own slug was one such orphan would get hit with
/// `duplicate key value violates unique constraint "org_slug_key"` every time
/// they submitted `/claim-handle` — with no way to escape.
///
/// Post-fix: `create_org` is idempotent for owned slugs (adopts them) and
/// `claim_handle_post_handler` adopts the owned org directly, heals the
/// missing `org_user` link, and forwards to billing.
#[tokio::test]
async fn claim_handle_adopts_orphan_org_owned_by_user() {
    let mut client = TestClient::new().await;

    // Insert the user directly — we're simulating a partial signup where
    // the user + verification exist but the org_user link was never made.
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
// Verify must never send users to /claim-handle if the org was successfully
// created. The problematic redirect chain is:
//
//   GET /verify-email?token=…              -> 303 (org created, JWT set)
//   GET /billing/create-checkout-form?…    -> 303 (current_org=None!)
//   GET /claim-handle?…                    -> 200 (asks for handle again)
//
// Root cause was using `/billing/create-checkout-form` as an intermediate
// hop, which depends on the org cookie surviving the redirect. The fix
// routes verify→billing directly through the slug-scoped URL.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn verify_never_bounces_through_create_checkout_form() {
    let mut client = TestClient::new().await;

    let form = submit_signup("alice@example.com", "alice");
    client
        .post_form(
            "/signup?plan=hot-free&billing=monthly",
            &as_form_slice(&form),
        )
        .await
        .assert_status(axum::http::StatusCode::OK);

    let token = read_pending_token(client.db(), "alice@example.com").await;
    let verify_resp = client.get(&format!("/verify-email?token={}", token)).await;

    let location = verify_resp.location().unwrap_or("");
    assert!(
        !location.starts_with("/billing/create-checkout-form"),
        "verify must not route through /billing/create-checkout-form (cookie-roundtrip \
         is fragile and can send users back to /claim-handle). \
         Got Location: {}",
        location
    );
    assert!(
        !location.starts_with("/claim-handle"),
        "verify must not redirect to /claim-handle when the org was created \
         successfully. Got Location: {}",
        location
    );
    assert!(
        location.starts_with("/@alice/billing/checkout"),
        "verify should redirect directly to the slug-scoped billing URL. \
         Got Location: {}",
        location
    );
}

/// Even if the org cookie somehow goes missing on the way to
/// `/billing/create-checkout-form`, the handler
/// must NOT redirect to `/claim-handle` if the user actually has an org.
/// It should fall back to a fresh DB read and route to the org's checkout.
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

    // Logged in, but no `hot_current_org` cookie set. session_middleware
    // will still find the org via `user_orgs.first()` fallback — but the
    // CRITICAL test is that the checkout_form_handler's *own* fresh-DB
    // fallback is in place too, so even pathological cases (empty
    // user_orgs snapshot) recover.
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

/// Even more pathological: the user is logged in, has no org cookie, and
/// somehow lands on /claim-handle directly. If they *do* have an org,
/// claim_handle_handler must not render the form — it must redirect.
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

// ---------------------------------------------------------------------------
// Soft reservation via pending verification
// ---------------------------------------------------------------------------

#[tokio::test]
async fn signup_rejects_slug_already_pending_for_another_user() {
    let mut client = TestClient::new().await;

    // Alice signs up first.
    let alice_form = submit_signup("alice@example.com", "taken-handle");
    client
        .post_form(
            "/signup?plan=hot-free&billing=monthly",
            &as_form_slice(&alice_form),
        )
        .await
        .assert_status(axum::http::StatusCode::OK);

    // Bob tries to sign up with the same handle — should get an error back
    // with a suggested alternative.
    let bob_form = submit_signup("bob@example.com", "taken-handle");
    let resp = client
        .post_form(
            "/signup?plan=hot-free&billing=monthly",
            &as_form_slice(&bob_form),
        )
        .await;
    resp.assert_status(axum::http::StatusCode::OK);
    assert!(
        resp.body.contains("taken-handle-2"),
        "expected suggestion `taken-handle-2` in error body; got snippet = {}",
        resp.body.chars().take(400).collect::<String>()
    );

    // And no verification row was created for bob.
    let bob_pending = EmailVerification::get_pending_by_email(client.db(), "bob@example.com")
        .await
        .expect("query");
    assert!(
        bob_pending.is_none(),
        "bob's signup should have been rejected before writing a verification row"
    );

    // But a row DID get written for the 24-hour window we expect.
    let alice_pending = EmailVerification::get_pending_by_email(client.db(), "alice@example.com")
        .await
        .expect("query")
        .expect("alice's row should exist");
    assert!(alice_pending.expires_at > Utc::now() + Duration::hours(23));
}

// ---------------------------------------------------------------------------
// Fix-forward recovery: user has a verification record with an org_slug,
// but no actual org exists yet. This can happen if verification was marked
// complete but org creation did not finish, or if another path left the user
// without an org. The expected behavior is that
// any subsequent landing on /claim-handle (or /billing/create-checkout-form)
// auto-creates the org from the original signup slug, so the user never
// sees the claim-handle form a second time.
// ---------------------------------------------------------------------------

/// Helper: create a "post-verify but no org" state — a user record exists,
/// an email_verification row exists with `org_slug`, but no `org` row was
/// ever created. Returns the user_id for `client.login_as(...)`.
async fn seed_user_with_orphaned_verification(
    db: &DatabasePool,
    email: &str,
    name: &str,
    slug: &str,
) -> Uuid {
    let user_id = Uuid::now_v7();
    hot::db::User::insert_user(db, &user_id, email, Some(name), Some(&user_id))
        .await
        .expect("insert user");

    let verification_id = Uuid::now_v7();
    EmailVerification::insert(
        db,
        &verification_id,
        email,
        Some(name),
        "{}", // dummy auth payload — we never re-verify in these tests
        "dummy-token",
        None,
        None,
        Some(slug),
        Some("hot-free"),
        Some("monthly"),
        Utc::now() + Duration::hours(24),
        Some("individual"),
    )
    .await
    .expect("insert verification");

    user_id
}

#[tokio::test]
async fn claim_handle_get_recovers_org_from_pending_verification_slug() {
    let mut client = TestClient::new().await;

    let user_id =
        seed_user_with_orphaned_verification(client.db(), "alice@example.com", "Alice", "alice")
            .await;

    // Confirm the precondition: user is logged in but has zero orgs and no
    // org cookie. Pre-fix, this would render the claim-handle form, asking
    // them to pick a handle they already chose.
    let pre_orgs = hot::db::org::Org::get_orgs_by_user(client.db(), &user_id)
        .await
        .expect("query");
    assert!(pre_orgs.is_empty(), "precondition: user has no orgs");

    client.login_as(&user_id);

    let resp = client
        .get("/claim-handle?plan=hot-free&billing=monthly")
        .await;

    let location = resp.location().unwrap_or("");
    assert!(
        location.starts_with("/@alice/billing/checkout"),
        "claim_handle GET must auto-recover from the pending verification slug \
         and route to billing — never re-render the form when the user already \
         picked a handle. Got: {}",
        location
    );

    // The org was actually created.
    let post_orgs = hot::db::org::Org::get_orgs_by_user(client.db(), &user_id)
        .await
        .expect("query");
    assert_eq!(post_orgs.len(), 1, "recovery should create the org");
    assert_eq!(post_orgs[0].slug, "alice");
}

#[tokio::test]
async fn checkout_form_recovers_org_from_pending_verification_slug() {
    let mut client = TestClient::new().await;

    let user_id =
        seed_user_with_orphaned_verification(client.db(), "alice@example.com", "Alice", "alice")
            .await;

    client.login_as(&user_id);

    // The exact path verify_email_handler historically bounced through.
    // Now even THIS hop must self-heal instead of dumping the user on
    // /claim-handle.
    let resp = client
        .get("/billing/create-checkout-form?plan=hot-free&billing=monthly")
        .await;

    let location = resp.location().unwrap_or("");
    assert!(
        location.starts_with("/@alice/billing/checkout"),
        "checkout_form_handler must auto-recover from the pending verification \
         slug instead of bouncing to /claim-handle. \
         Got: {}",
        location
    );

    let post_orgs = hot::db::org::Org::get_orgs_by_user(client.db(), &user_id)
        .await
        .expect("query");
    assert_eq!(post_orgs.len(), 1);
    assert_eq!(post_orgs[0].slug, "alice");
}

/// If the user's original signup slug is somehow taken by someone else by
/// the time we recover (lost a race), we should auto-pick `slug-2` and
/// land them on THAT — never bounce to the form.
#[tokio::test]
async fn claim_handle_get_recovers_with_alternative_slug_when_original_is_taken() {
    let mut client = TestClient::new().await;

    // Bob already owns "alice" (don't ask).
    let bob_id = Uuid::now_v7();
    hot::db::User::insert_user(
        client.db(),
        &bob_id,
        "bob@example.com",
        Some("Bob"),
        Some(&bob_id),
    )
    .await
    .expect("insert bob");
    hot_app::handlers::create_org(client.db(), &bob_id, "Alice", "alice", "individual")
        .await
        .expect("bob creates `alice`");

    // Alice signed up with "alice", but only the verification record exists
    // — her org never got created (because Bob beat her).
    let alice_id = seed_user_with_orphaned_verification(
        client.db(),
        "alice-real@example.com",
        "Alice",
        "alice",
    )
    .await;

    client.login_as(&alice_id);

    let resp = client
        .get("/claim-handle?plan=hot-free&billing=monthly")
        .await;

    let location = resp.location().unwrap_or("");
    assert!(
        location.starts_with("/@alice-2/billing/checkout"),
        "claim_handle GET must pick an alternative slug when the original \
         was lost to a race, never bounce to the form. Got: {}",
        location
    );

    let alice_orgs = hot::db::org::Org::get_orgs_by_user(client.db(), &alice_id)
        .await
        .expect("query");
    assert_eq!(alice_orgs.len(), 1);
    assert_eq!(alice_orgs[0].slug, "alice-2");
}

/// Negative case: a logged-in user with NO email_verification record (the
/// new-OAuth-user path) should still see the claim-handle form. We don't
/// want the recovery helper to over-reach and skip the form when there's
/// genuinely nothing to recover from.
#[tokio::test]
async fn claim_handle_get_renders_form_for_user_without_verification() {
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
    // NOTE: deliberately no EmailVerification::insert — OAuth users skip it.

    client.login_as(&user_id);

    let resp = client
        .get("/claim-handle?plan=hot-free&billing=monthly")
        .await;

    resp.assert_status(axum::http::StatusCode::OK);
    assert!(
        resp.body.to_lowercase().contains("handle"),
        "should render the claim-handle form for OAuth users with no verification \
         record. Body snippet: {}",
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

/// Regression: a user landing on `/@<slug>/billing/checkout` for a slug
/// that doesn't exist in the org table — but DOES match their pending
/// verification record — must auto-create the org and proceed to billing,
/// not bounce them with a 404. Staging hit this when verify-email's
/// `create_org` step had a transient failure and the user followed the
/// already-rendered redirect URL anyway.
#[tokio::test]
async fn org_checkout_form_self_heals_when_slug_is_missing_but_verification_exists() {
    let mut client = TestClient::new().await;

    let user_id =
        seed_user_with_orphaned_verification(client.db(), "alice@example.com", "Alice", "alice")
            .await;

    // Precondition: no org with slug "alice" exists.
    assert!(
        hot::db::org::Org::get_org_by_slug(client.db(), "alice")
            .await
            .is_err(),
        "precondition: slug must not yet exist"
    );

    client.login_as(&user_id);

    let resp = client
        .get("/@alice/billing/checkout?plan=hot-free&billing=monthly")
        .await;

    resp.assert_status(axum::http::StatusCode::SEE_OTHER);
    let location = resp.location().unwrap_or("");
    assert!(
        location.starts_with("/@alice/billing/checkout"),
        "org_checkout_form_handler must self-heal by creating the org from \
         the user's pending verification slug instead of returning 404. Got: {}",
        location
    );

    let post_orgs = hot::db::org::Org::get_orgs_by_user(client.db(), &user_id)
        .await
        .expect("query");
    assert_eq!(post_orgs.len(), 1);
    assert_eq!(post_orgs[0].slug, "alice");
}

/// Negative case: hitting `/@<slug>/billing/checkout` for a slug that the
/// user has no claim to (no matching verification, no membership) must
/// still return 404 — we should not be auto-creating arbitrary slugs that
/// happen to appear in the URL bar.
#[tokio::test]
async fn org_checkout_form_returns_404_when_slug_unrelated_to_user() {
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

//! HTTP-level tests for invite-only sign-up (`app.signup = "invite"`), for
//! inviting people on a self-hosted installation, and for the alert destination
//! emails it writes.

use chrono::{Duration, Utc};
use hot::db::alert::AlertDestination;
use hot::db::{DatabasePool, EmailVerification, Invite, User, UserAuth};
use hot::val;
use hot::val::Val;
use hot_app::test_support::{TestClient, TestResponse};
use uuid::Uuid;

/// A self-hosted installation that delivers email through SMTP.
fn self_host_with_email(extra: Val) -> Val {
    val!({
        "product": {"experience": "self-host"},
        "email": {"provider": "smtp", "from": "hot@example.com", "smtp": {"host": "smtp.example.com"}},
    })
    .merge(&extra)
}

fn invite_only() -> Val {
    self_host_with_email(val!({"app": {"signup": "invite"}}))
}

/// An organization `slug` with an administrator, and an invitation to it
/// for `email`. Returns the organization.
async fn invite(
    db: &DatabasePool,
    slug: &str,
    email: &str,
    code: &str,
    expires_at: chrono::DateTime<Utc>,
) -> Uuid {
    let admin_id = Uuid::now_v7();
    User::insert_user(
        db,
        &admin_id,
        &format!("admin@{slug}.example.com"),
        Some("Admin"),
        Some(&admin_id),
    )
    .await
    .expect("insert admin");
    let org_id = hot_app::handlers::create_org(db, &admin_id, slug, slug, "organization")
        .await
        .expect("create org");
    Invite::insert_invite(
        db,
        &Uuid::now_v7(),
        code,
        email,
        &org_id,
        1,
        &admin_id,
        expires_at,
    )
    .await
    .expect("insert invite");
    org_id
}

async fn sql(db: &DatabasePool, statement: &'static str, org_id: &Uuid) {
    let DatabasePool::Sqlite(pool) = db else {
        panic!("these tests use SQLite");
    };
    sqlx::query(statement)
        .bind(org_id)
        .execute(pool)
        .await
        .expect(statement);
}

/// A password for one test, so none is written in the source.
fn new_password() -> String {
    Uuid::new_v4().simple().to_string()
}

async fn sign_up(client: &mut TestClient, email: &str, invite_code: &str) -> TestResponse {
    sign_up_with_password(client, email, invite_code, &new_password()).await
}

async fn sign_up_with_password(
    client: &mut TestClient,
    email: &str,
    invite_code: &str,
    password: &str,
) -> TestResponse {
    let form_token = client.prime_csrf();
    let path = if invite_code.is_empty() {
        "/signup".to_string()
    } else {
        format!("/signup?invite_code={invite_code}")
    };
    client
        .post_form(
            &path,
            &[
                ("email", email),
                ("password", password),
                ("name", "Example Person"),
                ("website", ""),
                ("form_token", &form_token),
            ],
        )
        .await
}

async fn resend(client: &mut TestClient, email: &str) {
    let form_token = client.prime_csrf();
    client
        .post_form(
            "/resend-verification",
            &[("email", email), ("form_token", &form_token)],
        )
        .await
        .assert_status(axum::http::StatusCode::OK);
}

async fn confirm(client: &mut TestClient, token: &str) -> TestResponse {
    let form_token = client.prime_csrf();
    client
        .post_form(
            "/verify-email",
            &[("token", token), ("form_token", &form_token)],
        )
        .await
}

async fn pending_verification(db: &DatabasePool, email: &str) -> EmailVerification {
    EmailVerification::get_pending_by_email(db, email)
        .await
        .unwrap()
        .expect("a pending verification")
}

async fn joined(db: &DatabasePool, email: &str, org_id: &Uuid) -> bool {
    let user = User::get_user_by_email(db, email).await.expect(email);
    hot::db::org::Org::get_orgs_by_user(db, &user.user_id)
        .await
        .unwrap()
        .iter()
        .any(|org| org.org_id == *org_id)
}

fn snippet(resp: &TestResponse) -> String {
    resp.body.chars().take(400).collect()
}

#[tokio::test]
async fn invite_only_refuses_before_revealing_whether_an_account_exists() {
    let mut client = TestClient::new_with_conf(invite_only()).await;
    let user_id = Uuid::now_v7();
    User::insert_user(
        client.db(),
        &user_id,
        "taken@example.com",
        Some("Taken"),
        Some(&user_id),
    )
    .await
    .unwrap();

    let resp = sign_up(&mut client, "taken@example.com", "").await;
    assert!(
        resp.body.contains("requires an invitation"),
        "{}",
        snippet(&resp)
    );
    assert!(!resp.body.contains("already exists"), "{}", snippet(&resp));
}

#[tokio::test]
async fn invite_only_refuses_invitations_that_can_no_longer_be_applied() {
    let mut client = TestClient::new_with_conf(invite_only()).await;
    let db = client.db().clone();

    // The organization was suspended after inviting Eve.
    let suspended = invite(
        &db,
        "suspended",
        "eve@example.com",
        "suspended-code",
        Utc::now() + Duration::days(7),
    )
    .await;
    sql(
        &db,
        "UPDATE org SET lifecycle_state = 'suspended' WHERE org_id = ?",
        &suspended,
    )
    .await;
    let resp = sign_up(&mut client, "eve@example.com", "suspended-code").await;
    assert!(
        resp.body.contains("can no longer be used"),
        "{}",
        snippet(&resp)
    );

    // The organization's plan has no seat left for Frank.
    let full = invite(
        &db,
        "full",
        "frank@example.com",
        "full-code",
        Utc::now() + Duration::days(7),
    )
    .await;
    sql(
        &db,
        r#"UPDATE org SET features = '{"team_members": 1}' WHERE org_id = ?"#,
        &full,
    )
    .await;
    let resp = sign_up(&mut client, "frank@example.com", "full-code").await;
    assert!(
        resp.body.contains("can no longer be used"),
        "{}",
        snippet(&resp)
    );

    // An expired invitation.
    invite(
        &db,
        "expired",
        "grace@example.com",
        "expired-code",
        Utc::now() - Duration::days(1),
    )
    .await;
    let resp = sign_up(&mut client, "grace@example.com", "expired-code").await;
    assert!(
        resp.body.contains("can no longer be used"),
        "{}",
        snippet(&resp)
    );

    for email in ["eve@example.com", "frank@example.com", "grace@example.com"] {
        assert!(
            User::get_user_by_email(&db, email).await.is_err(),
            "no account may exist for {email}"
        );
    }

    // A differently cased address is the same invitation.
    invite(
        &db,
        "acme",
        "Bob@Example.com",
        "acme-code",
        Utc::now() + Duration::days(7),
    )
    .await;
    let resp = sign_up(&mut client, "bob@example.com", "acme-code").await;
    assert!(resp.status.is_redirection(), "{}", snippet(&resp));
}

#[tokio::test]
async fn verifications_sent_while_signup_was_open_lapse_under_invite_only() {
    let mut open = TestClient::new_with_conf(self_host_with_email(val!({}))).await;
    let resp = sign_up(&mut open, "alice@example.com", "").await;
    resp.assert_status(axum::http::StatusCode::OK);
    let pending = EmailVerification::get_pending_by_email(open.db(), "alice@example.com")
        .await
        .unwrap()
        .expect("open sign-up sends a verification email");

    // The operator then switches the installation to invite-only.
    let db = open.db().as_ref().clone();
    let mut client = TestClient::new_with_db(db.clone(), invite_only()).await;

    let resp = sign_up(&mut client, "alice@example.com", "").await;
    assert!(
        resp.body.contains("requires an invitation"),
        "a pending verification no longer re-shows check-email: {}",
        snippet(&resp)
    );

    // Resending does not keep the verification alive.
    let form_token = client.prime_csrf();
    client
        .post_form(
            "/resend-verification",
            &[("email", "alice@example.com"), ("form_token", &form_token)],
        )
        .await
        .assert_status(axum::http::StatusCode::OK);
    let after = EmailVerification::get_pending_by_email(&db, "alice@example.com")
        .await
        .unwrap()
        .expect("still pending");
    assert_eq!(after.verification_token, pending.verification_token);

    // Confirming it is refused, and leaves it pending.
    let form_token = client.prime_csrf();
    let resp = client
        .post_form(
            "/verify-email",
            &[
                ("token", &pending.verification_token),
                ("form_token", &form_token),
            ],
        )
        .await;
    assert!(
        resp.body.contains("Invitation required"),
        "{}",
        snippet(&resp)
    );
    assert!(
        User::get_user_by_email(&db, "alice@example.com")
            .await
            .is_err()
    );

    // Being invited doesn't revive it: Alice's way in is her invitation link.
    let org_id = invite(
        &db,
        "acme",
        "alice@example.com",
        "alice-code",
        Utc::now() + Duration::days(7),
    )
    .await;
    let resp = confirm(&mut client, &pending.verification_token).await;
    assert!(
        resp.body.contains("through the link in your invitation"),
        "{}",
        snippet(&resp)
    );
    assert!(
        User::get_user_by_email(&db, "alice@example.com")
            .await
            .is_err()
    );
    let resp = sign_up(&mut client, "alice@example.com", "alice-code").await;
    assert!(resp.status.is_redirection(), "{}", snippet(&resp));
    assert!(joined(&db, "alice@example.com", &org_id).await);
}

#[tokio::test]
async fn a_verification_from_the_open_period_cannot_create_the_invited_persons_account() {
    let email = "invited@example.com";
    let earlier_password = new_password();
    let owners_password = new_password();
    let mut open = TestClient::new_with_conf(self_host_with_email(val!({}))).await;
    let db = open.db().as_ref().clone();

    // Someone else asked to verify this address while sign-up was open, with
    // a password of their own.
    sign_up_with_password(&mut open, email, "", &earlier_password)
        .await
        .assert_status(axum::http::StatusCode::OK);
    let stale = pending_verification(&db, email).await;

    // The installation goes invite-only, and the mailbox's owner is invited.
    let org_id = invite(
        &db,
        "acme",
        email,
        "invited-code",
        Utc::now() + Duration::days(7),
    )
    .await;
    let mut client = TestClient::new_with_db(db.clone(), invite_only()).await;

    // They can't have a fresh link sent to the owner, and the page they get
    // points at the invitation instead of claiming an email went out.
    let form_token = client.prime_csrf();
    let resp = client
        .post_form(
            "/resend-verification",
            &[("email", email), ("form_token", &form_token)],
        )
        .await;
    resp.assert_status(axum::http::StatusCode::OK);
    assert!(
        resp.body.contains("through the link in your invitation"),
        "{}",
        snippet(&resp)
    );
    assert!(!resp.body.contains("We've sent"), "{}", snippet(&resp));
    assert!(
        !resp.body.contains("Click the link in the email"),
        "the link in that email only answers 'Invitation required'"
    );
    assert!(!resp.body.contains("Didn't receive the email"));
    let after = pending_verification(&db, email).await;
    assert_eq!(after.verification_token, stale.verification_token);
    assert_eq!(after.attempts, stale.attempts);
    assert_eq!(
        queued_emails(&db, email).await,
        1,
        "only the original email"
    );

    // An address with no verification at all gets the same page.
    let form_token = client.prime_csrf();
    let nobody = client
        .post_form(
            "/resend-verification",
            &[("email", "nobody@example.com"), ("form_token", &form_token)],
        )
        .await;
    assert!(
        nobody.body.contains("through the link in your invitation"),
        "{}",
        snippet(&nobody)
    );

    // The owner clicks the link already in their mailbox: no account comes of
    // it, and nothing joins the organization.
    let resp = confirm(&mut client, &stale.verification_token).await;
    assert!(
        resp.body.contains("Invitation required"),
        "{}",
        snippet(&resp)
    );
    assert!(
        resp.body.contains("through the link in your invitation"),
        "{}",
        snippet(&resp)
    );
    assert!(User::get_user_by_email(&db, email).await.is_err());
    assert!(
        UserAuth::get_user_auth(&db, "email_password", email)
            .await
            .is_err(),
        "the earlier password is not on any account"
    );

    // The invitation link gives them an account with their own password, and
    // ends the earlier verification.
    let resp = sign_up_with_password(&mut client, email, "invited-code", &owners_password).await;
    assert!(resp.status.is_redirection(), "{}", snippet(&resp));
    assert!(joined(&db, email, &org_id).await);
    let auth = UserAuth::get_user_auth(&db, "email_password", email)
        .await
        .unwrap();
    let stored = serde_json::to_string(&auth.auth_data.unwrap()).unwrap();
    assert!(hot::auth::verify_password(&owners_password, &stored).unwrap());
    assert!(!hot::auth::verify_password(&earlier_password, &stored).unwrap());
    client.clear_cookies();
    let resp = confirm(&mut client, &stale.verification_token).await;
    assert!(
        resp.body.contains("Verification unavailable"),
        "{}",
        snippet(&resp)
    );
}

/// A pending verification for `email` that carries `invite_code`.
async fn verification_carrying(db: &DatabasePool, email: &str, invite_code: &str) -> String {
    verification_on_plan(db, email, invite_code, None).await
}

async fn verification_on_plan(
    db: &DatabasePool,
    email: &str,
    invite_code: &str,
    plan: Option<&str>,
) -> String {
    let token = EmailVerification::generate_token();
    EmailVerification::insert(
        db,
        &Uuid::now_v7(),
        email,
        Some("Example Person"),
        &hot::auth::hash_password_with_random_salt(&new_password()).unwrap(),
        &token,
        Some(invite_code),
        plan,
        None,
        Utc::now() + Duration::hours(24),
    )
    .await
    .expect("insert verification");
    token
}

/// What the page from resending for `email` says, without its address and
/// form token. The layout around it carries a timestamp.
async fn resend_page(client: &mut TestClient, email: &str) -> String {
    let form_token = client.prime_csrf();
    let resp = client
        .post_form(
            "/resend-verification",
            &[("email", email), ("form_token", &form_token)],
        )
        .await;
    resp.assert_status(axum::http::StatusCode::OK);
    let start = resp.body.find("Check your email").expect("the heading");
    let end = resp.body.find("Wrong email?").expect("the last line");
    resp.body[start..end]
        .replace(email, "EMAIL")
        .replace(&form_token, "FORM_TOKEN")
}

#[tokio::test]
async fn the_invite_only_resend_page_says_the_same_for_every_address() {
    let mut client = TestClient::new_with_conf(invite_only()).await;
    let db = client.db().clone();
    let expires_at = Utc::now() + Duration::days(7);

    let page = resend_page(&mut client, "page-nobody@example.com").await;
    assert!(page.contains("through the link in your invitation"));
    assert!(page.contains("started from your invitation link"));
    assert!(page.contains("Resend verification email"));
    for misleading in [
        "We've sent",
        "We've already sent",
        "Click the link in the email",
        "Didn't receive the email",
        "add a card",
        "resend limit",
    ] {
        assert!(!page.contains(misleading), "{misleading}");
    }

    // A verification this installation would refuse.
    let refused = "page-refused@example.com";
    let token = verification_carrying(&db, refused, "no-such-code").await;
    assert_eq!(resend_page(&mut client, refused).await, page);
    let after = pending_verification(&db, refused).await;
    assert_eq!(after.verification_token, token, "it isn't kept alive");

    // One carrying its own invitation still gets a new email.
    let admitted = "page-admitted@example.com";
    invite(&db, "pagea", admitted, "page-admitted-code", expires_at).await;
    let token = verification_carrying(&db, admitted, "page-admitted-code").await;
    assert_eq!(resend_page(&mut client, admitted).await, page);
    let after = pending_verification(&db, admitted).await;
    assert_ne!(after.verification_token, token);
    assert_eq!(queued_emails(&db, admitted).await, 1);

    // So does one on a plan, without saying so.
    let on_plan = "page-on-plan@example.com";
    invite(&db, "pageb", on_plan, "page-on-plan-code", expires_at).await;
    verification_on_plan(&db, on_plan, "page-on-plan-code", Some("hot-free")).await;
    assert_eq!(resend_page(&mut client, on_plan).await, page);
    assert_eq!(queued_emails(&db, on_plan).await, 1);

    // One out of resends gets no email, also without saying so.
    let at_cap = "page-at-cap@example.com";
    invite(&db, "pagec", at_cap, "page-at-cap-code", expires_at).await;
    let token = verification_carrying(&db, at_cap, "page-at-cap-code").await;
    let row = pending_verification(&db, at_cap).await;
    for _ in 0..5 {
        EmailVerification::increment_attempts(&db, &row.verification_id)
            .await
            .expect("increment attempts");
    }
    assert_eq!(resend_page(&mut client, at_cap).await, page);
    let after = pending_verification(&db, at_cap).await;
    assert_eq!(after.verification_token, token);
    assert_eq!(queued_emails(&db, at_cap).await, 0);

    // The limit on requests applies to any address, whatever it holds. Its
    // page doesn't send anyone back to an earlier email either.
    let limited = "page-limited@example.com";
    for _ in 0..3 {
        assert_eq!(resend_page(&mut client, limited).await, page);
    }
    let limit_page = resend_page(&mut client, limited).await;
    assert!(limit_page.contains("resend limit"), "{limit_page}");
    assert!(limit_page.contains("started from your invitation link"));
    assert!(!limit_page.contains("Check your spam folder for one of the earlier emails"));
    assert!(!limit_page.contains("Resend verification email"));
}

#[tokio::test]
async fn a_verification_admits_through_the_invitation_it_carries() {
    let mut client = TestClient::new_with_conf(invite_only()).await;
    let db = client.db().clone();

    let org_id = invite(
        &db,
        "acme",
        "carries@example.com",
        "carries-code",
        Utc::now() + Duration::days(7),
    )
    .await;
    let sent = verification_carrying(&db, "carries@example.com", "carries-code").await;
    resend(&mut client, "carries@example.com").await;
    let resent = pending_verification(&db, "carries@example.com").await;
    assert_ne!(resent.verification_token, sent);
    let resp = confirm(&mut client, &resent.verification_token).await;
    assert!(resp.status.is_redirection(), "{}", snippet(&resp));
    assert!(joined(&db, "carries@example.com", &org_id).await);
    client.clear_cookies();

    // An invitation for another address is no invitation for this one, and
    // the address's own invitation doesn't step in for it.
    invite(
        &db,
        "bcme",
        "someone-else@example.com",
        "someone-else-code",
        Utc::now() + Duration::days(7),
    )
    .await;
    invite(
        &db,
        "ccme",
        "other-code@example.com",
        "other-code-own",
        Utc::now() + Duration::days(7),
    )
    .await;
    let token = verification_carrying(&db, "other-code@example.com", "someone-else-code").await;
    resend(&mut client, "other-code@example.com").await;
    let after = pending_verification(&db, "other-code@example.com").await;
    assert_eq!(after.verification_token, token);
    let resp = confirm(&mut client, &token).await;
    assert!(
        resp.body.contains("Invitation required"),
        "{}",
        snippet(&resp)
    );
    assert!(
        User::get_user_by_email(&db, "other-code@example.com")
            .await
            .is_err()
    );

    // Nor does an invitation that can't be applied any more.
    invite(
        &db,
        "dcme",
        "lapsed@example.com",
        "lapsed-code",
        Utc::now() - Duration::days(1),
    )
    .await;
    let token = verification_carrying(&db, "lapsed@example.com", "lapsed-code").await;
    let resp = confirm(&mut client, &token).await;
    assert!(
        resp.body.contains("Invitation required"),
        "{}",
        snippet(&resp)
    );
    assert!(
        User::get_user_by_email(&db, "lapsed@example.com")
            .await
            .is_err()
    );
}

/// Signs in an administrator of a fresh organization and invites `email`.
async fn invite_as_admin(client: &mut TestClient, email: &str) -> (TestResponse, Uuid) {
    let td = hot::db::insert_test_data(client.db()).await.unwrap();
    let org = hot::db::Org::get_org_uncached(client.db(), &td.org_id)
        .await
        .unwrap();
    client.login_as(&td.user_id);
    let resp = client
        .post_form(
            &format!("/@{}/users/invite", org.slug),
            &[("email", email), ("role_id", "1")],
        )
        .await;
    (resp, td.org_id)
}

#[tokio::test]
async fn shared_invitation_links_use_the_address_the_admin_is_on() {
    let mut client =
        TestClient::new_with_conf(val!({"product": {"experience": "self-host"}})).await;
    let (resp, _) = invite_as_admin(&mut client, "carol@example.com").await;
    resp.assert_status(axum::http::StatusCode::OK);
    let invite = Invite::get_invites_by_email(client.db(), "carol@example.com")
        .await
        .unwrap()
        .pop()
        .expect("the invitation is stored");
    assert!(
        resp.body.contains(&format!(
            r#"data-invite-path="/invite?code={}""#,
            invite.invite_code
        )),
        "{}",
        snippet(&resp)
    );
    assert!(resp.body.contains("window.location.origin"));
    assert!(
        !resp.body.contains("https://app.hot.dev/invite"),
        "the link never names another installation"
    );
}

#[tokio::test]
async fn an_expired_invitation_can_be_sent_again() {
    let mut client =
        TestClient::new_with_conf(val!({"product": {"experience": "self-host"}})).await;
    let (resp, org_id) = invite_as_admin(&mut client, "dave@example.com").await;
    resp.assert_status(axum::http::StatusCode::OK);
    sql(
        client.db(),
        "UPDATE invite SET expires_at = '2000-01-01T00:00:00Z' WHERE org_id = ?",
        &org_id,
    )
    .await;

    let td_slug = hot::db::Org::get_org_uncached(client.db(), &org_id)
        .await
        .unwrap()
        .slug;
    let resp = client
        .post_form(
            &format!("/@{td_slug}/users/invite"),
            &[("email", "dave@example.com"), ("role_id", "1")],
        )
        .await;
    assert!(!resp.body.contains("already pending"), "{}", snippet(&resp));
    let invites = Invite::get_invites_by_email(client.db(), "dave@example.com")
        .await
        .unwrap();
    assert_eq!(invites.len(), 2);
    assert!(invites.iter().any(|invite| invite.is_valid().is_ok()));
}

#[tokio::test]
async fn a_padded_address_is_the_same_address() {
    // It takes the invitation, so no account is stranded outside the
    // organization under an address the invitation does not name.
    let mut client = TestClient::new_with_conf(invite_only()).await;
    let db = client.db().clone();
    let org_id = invite(
        &db,
        "acme",
        "padded-bob@example.com",
        "padded-code",
        Utc::now() + Duration::days(7),
    )
    .await;
    let resp = sign_up(&mut client, " padded-bob@example.com ", "padded-code").await;
    assert!(resp.status.is_redirection(), "{}", snippet(&resp));
    assert!(joined(&db, "padded-bob@example.com", &org_id).await);
    assert!(
        User::get_user_by_email(&db, " padded-bob@example.com ")
            .await
            .is_err(),
        "no account under the padded address"
    );

    // It finds the account that already exists.
    let mut client = TestClient::new_with_conf(self_host_with_email(val!({}))).await;
    let db = client.db().clone();
    let user_id = Uuid::now_v7();
    User::insert_user(
        &db,
        &user_id,
        "padded-taken@example.com",
        Some("Taken"),
        Some(&user_id),
    )
    .await
    .unwrap();
    let resp = sign_up(&mut client, " padded-taken@example.com", "").await;
    assert!(resp.body.contains("already exists"), "{}", snippet(&resp));
    assert!(
        User::get_user_by_email(&db, " padded-taken@example.com")
            .await
            .is_err()
    );

    // Its verification is for the address itself.
    let resp = sign_up(&mut client, "padded-carol@example.com ", "").await;
    resp.assert_status(axum::http::StatusCode::OK);
    pending_verification(&db, "padded-carol@example.com").await;
    assert!(
        EmailVerification::get_pending_by_email(&db, "padded-carol@example.com ")
            .await
            .unwrap()
            .is_none()
    );

    // Elsewhere, an address accepting the invitation would refuse admits nobody.
    invite(
        &db,
        "bcme",
        "padded-dan@example.com",
        "padded-dan-code",
        Utc::now() + Duration::days(7),
    )
    .await;
    let invitation = Invite::get_invite_by_code(&db, "padded-dan-code")
        .await
        .unwrap();
    assert!(hot_app::signup::invite_admits(&db, &invitation, "padded-dan@example.com").await);
    assert!(!hot_app::signup::invite_admits(&db, &invitation, " padded-dan@example.com").await);
}

#[tokio::test]
async fn an_invitation_link_takes_over_from_a_pending_verification() {
    // With sign-up open, invite-only, and on an installation that lost email.
    for (label, conf) in [
        ("open", self_host_with_email(val!({}))),
        ("invite", invite_only()),
        ("no-email", val!({"product": {"experience": "self-host"}})),
    ] {
        let email = format!("takeover-{label}@example.com");
        let earlier_password = new_password();
        let invitees_password = new_password();
        let mut open = TestClient::new_with_conf(self_host_with_email(val!({}))).await;
        let db = open.db().as_ref().clone();

        // Someone asked for this address while sign-up was open.
        let resp = sign_up_with_password(&mut open, &email, "", &earlier_password).await;
        resp.assert_status(axum::http::StatusCode::OK);
        let stale = pending_verification(&db, &email).await;
        let org_id = invite(
            &db,
            "acme",
            &email,
            "takeover-code",
            Utc::now() + Duration::days(7),
        )
        .await;

        // The invitee signs up through the link, and is not sent to an email
        // that may never come.
        let mut client = TestClient::new_with_db(db.clone(), conf).await;
        let resp =
            sign_up_with_password(&mut client, &email, "takeover-code", &invitees_password).await;
        assert!(resp.status.is_redirection(), "{label}: {}", snippet(&resp));
        assert!(joined(&db, &email, &org_id).await, "{label}");

        // The account has the invitee's password, not the earlier one.
        let auth = UserAuth::get_user_auth(&db, "email_password", &email)
            .await
            .unwrap();
        let stored = serde_json::to_string(&auth.auth_data.unwrap()).unwrap();
        assert!(hot::auth::verify_password(&invitees_password, &stored).unwrap());
        assert!(!hot::auth::verify_password(&earlier_password, &stored).unwrap());

        // The earlier verification is over, and can't be resent.
        assert!(
            EmailVerification::get_pending_by_email(&db, &email)
                .await
                .unwrap()
                .is_none(),
            "{label}"
        );
        client.clear_cookies();
        let resp = confirm(&mut client, &stale.verification_token).await;
        assert!(
            resp.body.contains("Verification unavailable"),
            "{label}: {}",
            snippet(&resp)
        );
    }
}

#[tokio::test]
async fn a_valid_link_for_someone_else_keeps_the_pending_verification() {
    let mut client = TestClient::new_with_conf(self_host_with_email(val!({}))).await;
    let db = client.db().clone();
    let email = "keeps-pending@example.com";
    sign_up(&mut client, email, "")
        .await
        .assert_status(axum::http::StatusCode::OK);
    invite(
        &db,
        "acme",
        "someone-else@example.com",
        "someone-else-code",
        Utc::now() + Duration::days(7),
    )
    .await;

    // This is the refreshed form, which re-shows "check your email".
    let resp = sign_up(&mut client, email, "someone-else-code").await;
    resp.assert_status(axum::http::StatusCode::OK);
    assert!(User::get_user_by_email(&db, email).await.is_err());
    pending_verification(&db, email).await;
}

#[tokio::test]
async fn resending_without_delivery_keeps_the_link_already_sent() {
    let mut open = TestClient::new_with_conf(self_host_with_email(val!({}))).await;
    let email = "resend-lost-email@example.com";
    sign_up(&mut open, email, "")
        .await
        .assert_status(axum::http::StatusCode::OK);
    let db = open.db().as_ref().clone();
    let sent = pending_verification(&db, email).await;

    // Email then stops working: the provider is gone.
    let mut client =
        TestClient::new_with_db(db.clone(), val!({"product": {"experience": "self-host"}})).await;
    resend(&mut client, email).await;
    let after = pending_verification(&db, email).await;
    assert_eq!(after.verification_token, sent.verification_token);
    assert_eq!(after.attempts, sent.attempts);
    let DatabasePool::Sqlite(pool) = &db else {
        panic!("these tests use SQLite");
    };
    let queued: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM email_queue WHERE to_address = ?")
        .bind(email)
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(queued, 1, "only the original email");

    // The link already delivered still works.
    let resp = confirm(&mut client, &sent.verification_token).await;
    assert!(resp.status.is_redirection(), "{}", snippet(&resp));
    assert!(User::get_user_by_email(&db, email).await.is_ok());
}

#[tokio::test]
async fn resending_with_delivery_replaces_the_link() {
    let mut client = TestClient::new_with_conf(self_host_with_email(val!({}))).await;
    let db = client.db().clone();
    let email = "resend-delivers@example.com";
    sign_up(&mut client, email, "")
        .await
        .assert_status(axum::http::StatusCode::OK);
    let sent = pending_verification(&db, email).await;
    resend(&mut client, email).await;
    let after = pending_verification(&db, email).await;
    assert_ne!(after.verification_token, sent.verification_token);
    assert_eq!(after.attempts, sent.attempts + 1);
}

#[tokio::test]
async fn invitations_match_addresses_in_any_case() {
    let mut client = TestClient::new_with_conf(invite_only()).await;
    let db = client.db().clone();
    let email = "dana.any-case@corp.example.com";

    // The invitation was typed with capitals; the verification was not.
    let org_id = invite(
        &db,
        "acme",
        "Dana.Any-Case@Corp.Example.com",
        "dana-code",
        Utc::now() + Duration::days(7),
    )
    .await;
    let sent = verification_carrying(&db, email, "dana-code").await;

    // So resending goes ahead, and the new link admits Dana.
    resend(&mut client, email).await;
    let resent = pending_verification(&db, email).await;
    assert_ne!(resent.verification_token, sent);
    let resp = confirm(&mut client, &resent.verification_token).await;
    assert!(resp.status.is_redirection(), "{}", snippet(&resp));
    assert!(joined(&db, email, &org_id).await);

    // Without the link, in other capitals, with the invitation stored in
    // lowercase. Google and GitHub sign-in look invitations up this way.
    invite(
        &db,
        "bcme",
        "erin@corp.example.com",
        "erin-code",
        Utc::now() + Duration::days(7),
    )
    .await;
    let admitted = hot_app::signup::admitting_invite(&db, "ERIN@corp.example.com", None)
        .await
        .unwrap()
        .expect("the invitation admits Erin");
    assert_eq!(admitted.invite_code, "erin-code");
}

#[tokio::test]
async fn a_signed_in_invitee_accepts_in_any_case() {
    let mut client = TestClient::new_with_conf(self_host_with_email(val!({}))).await;
    let db = client.db().clone();
    let org_id = invite(
        &db,
        "acme",
        "Bob@Corp.Example.com",
        "signed-in-code",
        Utc::now() + Duration::days(7),
    )
    .await;
    let bob = Uuid::now_v7();
    User::insert_user(&db, &bob, "bob@corp.example.com", Some("Bob"), Some(&bob))
        .await
        .unwrap();
    client.login_as(&bob);

    let resp = client.get("/invite?code=signed-in-code").await;
    resp.assert_status(axum::http::StatusCode::OK);
    assert!(
        !resp.body.contains("but you are signed in as"),
        "{}",
        snippet(&resp)
    );

    let resp = client
        .post_form("/invite", &[("code", "signed-in-code")])
        .await;
    resp.assert_redirect_to("/?invite_accepted=1");
    assert!(joined(&db, "bob@corp.example.com", &org_id).await);

    // Someone else, signed in, is still told it is not theirs.
    invite(
        &db,
        "bcme",
        "carol@corp.example.com",
        "carols-code",
        Utc::now() + Duration::days(7),
    )
    .await;
    let resp = client.get("/invite?code=carols-code").await;
    assert!(
        resp.body.contains("but you are signed in as"),
        "{}",
        snippet(&resp)
    );
    let resp = client
        .post_form("/invite", &[("code", "carols-code")])
        .await;
    assert!(resp.body.contains("Email Mismatch"), "{}", snippet(&resp));
}

/// How many emails are queued for `to`.
async fn queued_emails(db: &DatabasePool, to: &str) -> i64 {
    let DatabasePool::Sqlite(pool) = db else {
        panic!("these tests use SQLite");
    };
    sqlx::query_scalar("SELECT COUNT(*) FROM email_queue WHERE to_address = ?")
        .bind(to)
        .fetch_one(pool)
        .await
        .unwrap()
}

/// Add an alert destination for `address` as the seeded admin. Returns where
/// the app sent them, the destination, and the query naming the page's scope.
async fn add_email_destination(
    client: &mut TestClient,
    address: &str,
) -> (String, AlertDestination, String) {
    let td = hot::db::insert_test_data(client.db()).await.unwrap();
    client.login_as(&td.user_id);
    let scope = format!("org_id={}&env_id={}", td.org_id, td.env_id);
    let resp = client
        .post_form(
            &format!("/settings/alerts/destinations?{scope}"),
            &[
                ("name", "Ops"),
                ("destination_type", "email"),
                ("email_target", "address"),
                ("email_address", address),
            ],
        )
        .await;
    assert!(resp.is_redirect(), "{}", snippet(&resp));
    let destination = AlertDestination::get_by_org(client.db(), &td.org_id)
        .await
        .unwrap()
        .pop()
        .expect("the destination is stored");
    (resp.location().unwrap().to_string(), destination, scope)
}

#[tokio::test]
async fn destination_email_without_a_sender_of_its_own_is_not_queued() {
    // This app could be sharing the worker's relay; it would still write mail
    // from Hot Cloud's sender.
    let mut client =
        TestClient::new_with_conf(val!({"product": {"experience": "self-host"}})).await;
    let db = client.db().clone();
    let address = "ops@partner.example";
    let (location, destination, scope) = add_email_destination(&mut client, address).await;
    assert!(
        location.ends_with("error=verification_not_sent"),
        "{location}"
    );
    assert_eq!(queued_emails(&db, address).await, 0);
    assert!(!destination.verified);
    let page = client.get(&location).await;
    assert!(
        page.body.contains("could not be queued"),
        "{}",
        snippet(&page)
    );

    // Resending says so, and leaves the destination's link as it was.
    let resp = client
        .post_form(
            &format!(
                "/settings/alerts/destinations/{}/resend-verification?{scope}",
                destination.alert_destination_id
            ),
            &[],
        )
        .await;
    assert!(
        resp.location().unwrap().ends_with("error=resend_failed"),
        "{}",
        snippet(&resp)
    );
    assert_eq!(queued_emails(&db, address).await, 0);
    let after = AlertDestination::get_by_id_for_org(
        &db,
        &destination.alert_destination_id,
        &destination.org_id,
    )
    .await
    .unwrap();
    assert_eq!(after.verification_token, destination.verification_token);
}

#[tokio::test]
async fn destination_email_with_a_sender_of_its_own_is_queued() {
    let mut client = TestClient::new_with_conf(self_host_with_email(val!({}))).await;
    let db = client.db().clone();
    let address = "ops@partner.example";
    let (location, _, _) = add_email_destination(&mut client, address).await;
    assert!(location.ends_with("info=verification_sent"), "{location}");
    assert_eq!(queued_emails(&db, address).await, 1);
}

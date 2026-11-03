//! Invite-only sign-up under the production PostgreSQL role contract: the
//! invitation checks run before anyone is signed in, so they must read
//! through row-level security with the scope the invitation grants.
//!
//! `privileged_fixture` says when this test skips and when it fails: without
//! `HOT_TEST_POSTGRES_URI` it skips, and it must name a login that can create
//! databases and roles.

#[allow(dead_code, unused_macros)]
#[path = "../../hot/tests/support/privileged_fixture.rs"]
mod privileged_fixture;

use hot::db::{self, DatabasePool};
use hot_app::test_support::TestClient;
use privileged_fixture::{PrivilegedDb, TestResult};
use uuid::Uuid;

#[tokio::test]
async fn invite_only_signup_reads_invitations_through_the_runtime_login() -> TestResult {
    let Ok(admin_uri) = std::env::var("HOT_TEST_POSTGRES_URI") else {
        eprintln!("skipping PostgreSQL sign-up policy test: HOT_TEST_POSTGRES_URI not set");
        return Ok(());
    };
    let postgres = PrivilegedDb::create(&admin_uri, "hot_app_signup").await?;
    // Clean up after a failed assertion too, then report it.
    let outcome = futures::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(
        invite_only_signup(&postgres),
    ))
    .await;
    let cleaned = postgres.destroy().await;
    privileged_fixture::report(outcome, cleaned)
}

async fn invite_only_signup(postgres: &PrivilegedDb) -> TestResult {
    let admin = &postgres.admin;
    let suffix = Uuid::new_v4().simple().to_string();
    let runtime =
        db::create_db_pool(&hot::val!({"uri":postgres.runtime_uri.clone(),"schema":"hot"})).await?;

    // Two organizations invite people; the second one is then suspended,
    // and the first allows one more member than it has.
    let admin_id = Uuid::new_v4();
    sqlx::query("INSERT INTO hot.\"user\"(user_id,email,name,created_by_user_id) VALUES($1,$2,'Administrator',$1)")
        .bind(admin_id).bind(format!("{suffix}@signup.example.test")).execute(admin).await?;
    let invite = |org: Uuid, email: &'static str, code: &'static str| {
        let admin = admin.clone();
        async move {
            sqlx::query("INSERT INTO hot.invite(invite_id,invite_code,email,org_id,invite_status_id,intended_org_user_role_id,created_by_user_id,expires_at) VALUES($1,$2,$3,$4,1,1,$5,now() + interval '7 days')")
                .bind(Uuid::new_v4()).bind(code).bind(email).bind(org).bind(admin_id).execute(&admin).await
        }
    };
    let mut orgs = Vec::new();
    for (label, features) in [("acme", r#"{"team_members":2}"#), ("bcme", "{}")] {
        let org = Uuid::new_v4();
        sqlx::query("INSERT INTO hot.org(org_id,name,slug,org_type,features,created_by_user_id) VALUES($1,$2,$3,'organization',$4::jsonb,$5)")
            .bind(org).bind(label).bind(format!("{label}-{}", &suffix[..16])).bind(features).bind(admin_id).execute(admin).await?;
        sqlx::query("INSERT INTO hot.org_user(org_user_id,org_id,user_id,org_user_role_id,created_by_user_id) VALUES($1,$2,$3,2,$3)")
            .bind(Uuid::new_v4()).bind(org).bind(admin_id).execute(admin).await?;
        orgs.push(org);
    }
    invite(orgs[0], "Bob@Example.test", "bob-code").await?;
    invite(orgs[0], "carol@example.test", "carol-code").await?;
    invite(orgs[1], "eve@example.test", "eve-code").await?;
    sqlx::query("UPDATE hot.org SET lifecycle_state='suspended' WHERE org_id=$1")
        .bind(orgs[1])
        .execute(admin)
        .await?;

    // The helpers read invitations by code and by address, and the
    // organization's state and seats, through the restricted login.
    let admitted = |email: &'static str, code: Option<&'static str>| {
        let runtime = runtime.clone();
        async move {
            hot_app::signup::admitting_invite(&runtime, email, code)
                .await
                .map(|invite| invite.map(|invite| invite.invite_code))
        }
    };
    assert_eq!(
        admitted("bob@example.test", Some("bob-code")).await?,
        Some("bob-code".into())
    );
    // Without the link, in any case on either side.
    assert_eq!(
        admitted("bob@example.test", None).await?,
        Some("bob-code".into())
    );
    assert_eq!(admitted(" bob@example.test", Some("bob-code")).await?, None);
    assert_eq!(
        admitted("Carol@Example.test", None).await?,
        Some("carol-code".into())
    );
    assert_eq!(admitted("eve@example.test", Some("eve-code")).await?, None);
    assert_eq!(
        admitted("mallory@example.test", Some("bob-code")).await?,
        None
    );

    // Someone asked to verify Bob's address while sign-up was open.
    sqlx::query("INSERT INTO hot.email_verification(verification_id,email,name,password_hash,verification_token,status_id,expires_at) VALUES($1,$2,'Someone','{}',$3,1,now() + interval '1 day')")
        .bind(Uuid::new_v4())
        .bind("bob@example.test")
        .bind(format!("stale-{suffix}"))
        .execute(admin)
        .await?;

    let mut client = TestClient::new_with_db(
        runtime.clone(),
        hot::val!({"app":{"host":"localhost","port":4680,"signup":"invite"},"product":{"experience":"self-host"},"db":{"uri":postgres.runtime_uri.clone(),"schema":"hot"}}),
    )
    .await;

    // It carries no invitation, so Bob's invitation doesn't let it through.
    let form_token = client.prime_csrf();
    let stale_token = format!("stale-{suffix}");
    let resp = client
        .post_form(
            "/verify-email",
            &[("token", &stale_token), ("form_token", &form_token)],
        )
        .await;
    assert!(
        resp.body.contains("Invitation required"),
        "{}: {}",
        resp.status,
        resp.body.chars().take(400).collect::<String>()
    );

    // Bob signs up through the form with his invitation and joins Acme.
    let form_token = client.prime_csrf();
    let resp = client
        .post_form(
            "/signup?invite_code=bob-code",
            &[
                ("email", "bob@example.test"),
                ("password", "supersecret123"),
                ("name", "Bob"),
                ("website", ""),
                ("form_token", &form_token),
            ],
        )
        .await;
    assert!(
        resp.status.is_redirection(),
        "{}: {}",
        resp.status,
        resp.body.chars().take(400).collect::<String>()
    );
    let joined: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM hot.org_user ou JOIN hot.\"user\" u ON u.user_id=ou.user_id WHERE ou.org_id=$1 AND u.email='bob@example.test' AND ou.active)")
        .bind(orgs[0]).fetch_one(admin).await?;
    assert!(joined, "Bob joined the inviting organization");
    let still_pending: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM hot.email_verification WHERE email='bob@example.test' AND status_id=1)")
        .fetch_one(admin).await?;
    assert!(!still_pending, "the earlier verification is over");

    // Acme's last seat is now taken, so Carol's invitation no longer admits.
    assert_eq!(admitted("carol@example.test", None).await?, None);

    if let DatabasePool::Postgres(pool) = &runtime {
        pool.close().await;
    }
    Ok(())
}

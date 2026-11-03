//! One account per address, whatever its capitals. The same scenario runs on
//! SQLite and, when HOT_TEST_POSTGRES_URI names a dedicated privileged fixture
//! database, on PostgreSQL under the production role contract, so the
//! restricted runtime login has to be able to do every lookup.

use chrono::{Duration, Utc};
use hot::db::{
    self, BootstrapOperation, DatabasePool, DatabaseScope, EmailVerification, Invite, User,
    UserAuth,
};
use hot::val::Val;
use hot_app::handlers::oauth::handle_oauth_user;
use hot_app::test_support::{TestClient, TestResponse};
use sqlx::{Executor, PgPool};
use uuid::Uuid;

const PASSWORD: &str = "supersecret123";

/// One address in the three spellings the scenarios sign in with.
struct Address {
    typed: String,
    lower: String,
    upper: String,
}

impl Address {
    fn new(name: &str, run: &str) -> Self {
        let typed = format!("{name}.{run}@Example.COM");
        Self {
            lower: typed.to_ascii_lowercase(),
            upper: typed.to_ascii_uppercase(),
            typed,
        }
    }
}

async fn sign_up(client: &mut TestClient, email: &str, query: &str) -> TestResponse {
    let form_token = client.prime_csrf();
    client
        .post_form(
            &format!("/signup{query}"),
            &[
                ("email", email),
                ("password", PASSWORD),
                ("name", "Test Person"),
                ("website", ""),
                ("form_token", &form_token),
            ],
        )
        .await
}

async fn signs_in(client: &mut TestClient, email: &str, password: &str) -> bool {
    client.clear_cookies();
    let form_token = client.prime_csrf();
    let response = client
        .post_form(
            "/signin",
            &[
                ("email", email),
                ("password", password),
                ("form_token", &form_token),
            ],
        )
        .await;
    let signed_in =
        response.status.is_redirection() && client.cookies().get("hot_auth_token").is_some();
    client.clear_cookies();
    signed_in
}

async fn queued_addresses(db: &DatabasePool) -> Vec<String> {
    let sql = "SELECT to_address FROM email_queue";
    match db {
        DatabasePool::Postgres(pool) => sqlx::query_scalar(sql).fetch_all(pool).await.unwrap(),
        DatabasePool::Sqlite(pool) => sqlx::query_scalar(sql).fetch_all(pool).await.unwrap(),
    }
}

async fn accounts_named(db: &DatabasePool, address: &Address) -> i64 {
    let lower = address.lower.clone();
    match db {
        DatabasePool::Postgres(pool) => {
            sqlx::query_scalar(r#"SELECT COUNT(*) FROM "user" WHERE lower(email) = $1"#)
                .bind(lower)
                .fetch_one(pool)
                .await
                .unwrap()
        }
        DatabasePool::Sqlite(pool) => {
            sqlx::query_scalar("SELECT COUNT(*) FROM user WHERE lower(email) = ?")
                .bind(lower)
                .fetch_one(pool)
                .await
                .unwrap()
        }
    }
}

/// Form sign-up, verification, resend and sign-in all reach one account from
/// any spelling, and mail goes to the address as it was typed.
async fn form_sign_up_and_sign_in(client: &mut TestClient, db: &DatabasePool, run: &str) {
    let dana = Address::new("Dana", run);

    let response = sign_up(client, &dana.typed, "").await;
    assert!(response.status.is_success(), "{}", response.body);
    let pending = EmailVerification::get_pending_by_email(db, &dana.lower)
        .await
        .unwrap()
        .expect("pending under other capitals");
    assert_eq!(pending.email, dana.typed, "the address stays as typed");

    // Refreshing the form with other capitals starts nothing new.
    let response = sign_up(client, &dana.upper, "").await;
    assert!(response.status.is_success(), "{}", response.body);
    let again = EmailVerification::get_pending_by_email(db, &dana.upper)
        .await
        .unwrap()
        .expect("pending under other capitals");
    assert_eq!(again.verification_id, pending.verification_id);

    let form_token = client.prime_csrf();
    client
        .post_form(
            "/resend-verification",
            &[("email", dana.lower.as_str()), ("form_token", &form_token)],
        )
        .await;
    let resent = EmailVerification::get_pending_by_email(db, &dana.typed)
        .await
        .unwrap()
        .expect("still pending");
    assert_eq!(resent.attempts, 1, "resend found the row");
    let queued: Vec<String> = queued_addresses(db)
        .await
        .into_iter()
        .filter(|to| to.eq_ignore_ascii_case(&dana.typed))
        .collect();
    assert_eq!(queued, [dana.typed.clone(), dana.typed.clone()]);

    let form_token = client.prime_csrf();
    let response = client
        .post_form(
            "/verify-email",
            &[
                ("token", resent.verification_token.as_str()),
                ("form_token", &form_token),
            ],
        )
        .await;
    assert!(response.status.is_redirection(), "{}", response.body);
    client.clear_cookies();

    let user = User::get_user_by_email(db, &dana.upper).await.unwrap();
    assert_eq!(
        user.email, dana.typed,
        "the account keeps the typed address"
    );
    let found = EmailVerification::get_latest_by_email(db, &dana.upper)
        .await
        .unwrap()
        .expect("latest under other capitals");
    assert_eq!(found.verification_id, pending.verification_id);

    // A second spelling can't make a second account, through the form or below it.
    let response = sign_up(client, &dana.lower, "").await;
    assert!(
        response
            .body
            .contains("A user with this email already exists"),
        "{}",
        response.body
    );
    let stranger = Uuid::now_v7();
    assert!(
        User::insert_user(db, &stranger, &dana.upper, None, Some(&stranger))
            .await
            .is_err(),
        "the unique index refuses a second spelling"
    );
    assert_eq!(accounts_named(db, &dana).await, 1);

    for spelling in [&dana.typed, &dana.lower, &dana.upper] {
        assert!(signs_in(client, spelling, PASSWORD).await, "{spelling}");
    }
    assert!(!signs_in(client, &dana.lower, "not-the-password").await);
}

/// OAuth reaches the same account from any spelling, links once, and creates
/// an account under the provider's spelling. Only ASCII capitals fold: dots,
/// plus suffixes and other letters still name other addresses.
async fn oauth_sign_in(db: &DatabasePool, conf: &Val, run: &str) {
    let erin = Address::new("Erin", run);
    let (created, is_new) = handle_oauth_user(
        db,
        conf,
        &erin.typed,
        "github",
        &format!("github-{run}"),
        Some("Erin"),
    )
    .await
    .unwrap();
    assert!(is_new);
    assert_eq!(created.email, erin.typed);

    for (spelling, provider_id) in [
        (&erin.lower, format!("github-{run}")),
        (&erin.upper, format!("other-github-{run}")),
    ] {
        let (user, is_new) =
            handle_oauth_user(db, conf, spelling, "github", &provider_id, Some("Erin"))
                .await
                .unwrap();
        assert!(!is_new, "{spelling}");
        assert_eq!(user.user_id, created.user_id, "{spelling}");
    }
    let (user, is_new) = handle_oauth_user(
        db,
        conf,
        &erin.upper,
        "google",
        &format!("google-{run}"),
        Some("Erin"),
    )
    .await
    .unwrap();
    assert!(!is_new);
    assert_eq!(user.user_id, created.user_id, "a new provider links");
    let links = UserAuth::get_user_auths_by_user(db, &created.user_id)
        .await
        .unwrap();
    let mut providers: Vec<&str> = links.iter().map(|link| link.auth_type.as_str()).collect();
    providers.sort();
    assert_eq!(providers, ["github", "google"], "each provider linked once");
    assert_eq!(accounts_named(db, &erin).await, 1);

    // A form sign-up finds the account OAuth made.
    let lookup = User::get_user_by_email(db, &erin.lower).await.unwrap();
    assert_eq!(lookup.user_id, created.user_id);

    for (index, email) in [
        format!("dots.{run}@example.com"),
        format!("do.ts.{run}@example.com"),
        format!("dots.{run}+tag@example.com"),
        format!("é.{run}@example.com"),
        format!("É.{run}@example.com"),
    ]
    .iter()
    .enumerate()
    {
        let (user, is_new) = handle_oauth_user(
            db,
            conf,
            email,
            "github",
            &format!("distinct-{index}-{run}"),
            None,
        )
        .await
        .unwrap();
        assert!(is_new, "{email} names its own address");
        assert_eq!(&user.email, email);
    }
}

/// Invitations match their address whatever its capitals, for someone signing
/// up with it and for someone already signed in.
async fn invitations(client: &mut TestClient, db: &DatabasePool, run: &str) {
    let admin = Address::new("Admin", run);
    let (admin, _) = handle_oauth_user(
        db,
        &hot::val!({}),
        &admin.typed,
        "github",
        &format!("admin-{run}"),
        None,
    )
    .await
    .unwrap();
    let org = hot_app::handlers::create_org(
        db,
        &admin.user_id,
        "Case test",
        &format!("case-{run}"),
        "organization",
    )
    .await
    .unwrap();
    let invite = |code: String, email: String| {
        let (db, admin) = (db.clone(), admin.user_id);
        async move {
            Invite::insert_invite(
                &db,
                &Uuid::now_v7(),
                &code,
                &email,
                &org,
                1,
                &admin,
                Utc::now() + Duration::days(7),
            )
            .await
            .unwrap();
        }
    };

    let bob = Address::new("Bob", run);
    invite(format!("signup-{run}"), bob.typed.clone()).await;
    // The invite page reads an organization's own invitations in its scope.
    let organization = db.with_scope(DatabaseScope::Organization { org_id: org });
    assert_eq!(
        Invite::get_invites_by_email(&organization, &bob.upper)
            .await
            .unwrap()
            .len(),
        1,
        "found under other capitals"
    );
    let response = sign_up(client, &bob.upper, &format!("?invite_code=signup-{run}")).await;
    assert!(response.status.is_redirection(), "{}", response.body);
    assert!(
        client.cookies().get("hot_auth_token").is_some(),
        "the invitation proved the mailbox"
    );
    client.clear_cookies();

    let carol = Address::new("Carol", run);
    let (signed_in, _) = handle_oauth_user(
        db,
        &hot::val!({}),
        &carol.lower,
        "github",
        &format!("carol-{run}"),
        None,
    )
    .await
    .unwrap();
    invite(format!("accept-{run}"), carol.typed.clone()).await;
    client.login_as(&signed_in.user_id);
    let response = client.get(&format!("/invite?code=accept-{run}")).await;
    assert!(
        !response.body.contains("but you are signed in as"),
        "{}",
        response.body
    );
    let form_token = client.prime_csrf();
    let response = client
        .post_form(
            "/invite",
            &[
                ("code", format!("accept-{run}").as_str()),
                ("form_token", &form_token),
            ],
        )
        .await;
    assert!(
        !response.body.contains("Email Mismatch"),
        "{}",
        response.body
    );
    assert!(response.status.is_redirection(), "{}", response.body);
    client.clear_cookies();
}

/// Accounts that differed only in capitals before migration 028 keep their
/// own passwords under their own spellings, and any other spelling reaches the
/// account that kept the lowered address.
async fn accounts_older_than_the_index(client: &mut TestClient, db: &DatabasePool, run: &str) {
    let fay = Address::new("Fay", run);
    let keeper = Uuid::now_v7();
    let exact = Uuid::now_v7();
    let exact_spelling = fay.upper.clone();
    User::insert_user(db, &keeper, &fay.lower, None, Some(&keeper))
        .await
        .unwrap();
    // The index can't hold two of them now, so start the second from another
    // address and give it the state 028 leaves behind.
    let placeholder = format!("placeholder.{run}@example.com");
    User::insert_user(db, &exact, &placeholder, None, Some(&exact))
        .await
        .unwrap();
    match db {
        DatabasePool::Postgres(pool) => {
            sqlx::query(
                r#"UPDATE "user" SET email_exact_only = true, email = $2 WHERE user_id = $1"#,
            )
            .bind(exact)
            .bind(&exact_spelling)
            .execute(pool)
            .await
            .unwrap();
        }
        DatabasePool::Sqlite(pool) => {
            sqlx::query("UPDATE user SET email_exact_only = 1, email = ? WHERE user_id = ?")
                .bind(&exact_spelling)
                .bind(exact)
                .execute(pool)
                .await
                .unwrap();
        }
    }
    let other_password = "another-password";
    for (user, email, password) in [
        (keeper, &fay.lower, PASSWORD),
        (exact, &exact_spelling, other_password),
    ] {
        let hash = hot::auth::hash_password_with_random_salt(password).unwrap();
        let data: serde_json::Value = serde_json::from_str(&hash).unwrap();
        UserAuth::insert_user_auth(
            db,
            &Uuid::now_v7(),
            &user,
            "email_password",
            email,
            Some(&data),
            &user,
        )
        .await
        .unwrap();
    }

    for (spelling, expected) in [
        (&fay.lower, keeper),
        (&exact_spelling, exact),
        (&fay.typed, keeper),
    ] {
        let user = User::get_user_by_email(db, spelling).await.unwrap();
        assert_eq!(user.user_id, expected, "{spelling}");
    }
    assert!(signs_in(client, &fay.lower, PASSWORD).await);
    assert!(signs_in(client, &exact_spelling, other_password).await);
    assert!(signs_in(client, &fay.typed, PASSWORD).await);
    assert!(!signs_in(client, &exact_spelling, PASSWORD).await);
    assert!(!signs_in(client, &fay.typed, other_password).await);
}

async fn every_scenario(client: &mut TestClient, conf: &Val) {
    let run = Uuid::now_v7().simple().to_string();
    // The App's public routes run in this scope; so do these direct calls.
    let db = client
        .db()
        .with_scope(DatabaseScope::Bootstrap(BootstrapOperation::Onboarding));
    form_sign_up_and_sign_in(client, &db, &run).await;
    oauth_sign_in(&db, conf, &run).await;
    invitations(client, &db, &run).await;
    accounts_older_than_the_index(client, &db, &run).await;
}

#[tokio::test]
async fn one_account_per_address_on_sqlite() {
    let conf = hot::val!({"app": {"host": "localhost", "port": 4680}});
    let mut client = TestClient::new_with_conf(conf.clone()).await;
    every_scenario(&mut client, &conf).await;
}

#[tokio::test]
async fn one_account_per_address_on_postgres_under_a_restricted_login()
-> Result<(), Box<dyn std::error::Error>> {
    let Ok(admin_uri) = std::env::var("HOT_TEST_POSTGRES_URI") else {
        eprintln!("skipping PostgreSQL email case test: HOT_TEST_POSTGRES_URI not set");
        return Ok(());
    };
    let suffix = Uuid::new_v4().simple().to_string();
    let database = format!("hot_email_case_{suffix}");
    let cluster = PgPool::connect(&admin_uri).await?;
    cluster
        .execute(sqlx::AssertSqlSafe(format!("CREATE DATABASE {database}")))
        .await?;
    let mut fixture_url = url::Url::parse(&admin_uri)?;
    fixture_url.set_path(&format!("/{database}"));
    let admin_uri = fixture_url.to_string();
    db::run_migrations(&hot::val!({"uri":admin_uri.clone(),"rls":{"enforce":false}})).await?;
    let admin = PgPool::connect(&admin_uri).await?;
    let role = format!("hot_email_case_{suffix}");
    let password = Uuid::new_v4().simple().to_string();
    admin.execute(sqlx::AssertSqlSafe(format!("CREATE ROLE {role} LOGIN PASSWORD '{password}' NOSUPERUSER NOBYPASSRLS NOINHERIT NOCREATEDB NOCREATEROLE"))).await?;
    let admin_password = fixture_url.password().unwrap_or_default().to_owned();
    fixture_url
        .set_password(None)
        .map_err(|_| "invalid fixture URL")?;
    let provision = tokio::process::Command::new("psql")
        .args([
            "-X",
            "-v",
            "ON_ERROR_STOP=1",
            "-v",
            &format!("runtime_role={role}"),
            "-v",
            "schema=hot",
            "-f",
        ])
        .arg(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../resources/db/postgres/provision_runtime.sql"),
        )
        .arg("--dbname")
        .arg(fixture_url.as_str())
        .env("PGPASSWORD", admin_password)
        .output()
        .await?;
    assert!(
        provision.status.success(),
        "{}",
        String::from_utf8_lossy(&provision.stderr)
    );
    let mut runtime_url = url::Url::parse(&admin_uri)?;
    runtime_url
        .set_username(&role)
        .map_err(|_| "invalid fixture URL")?;
    runtime_url
        .set_password(Some(&password))
        .map_err(|_| "invalid fixture URL")?;
    // Production pool creation performs role qualification with enforcement on.
    let runtime =
        db::create_db_pool(&hot::val!({"uri":runtime_url.to_string(),"schema":"hot"})).await?;
    let conf = hot::val!({"app":{"host":"localhost","port":4680},"db":{"uri":runtime_url.to_string(),"schema":"hot"},"store":{"type":"postgres"}});
    let mut client = TestClient::new_with_db(runtime.clone(), conf.clone()).await;

    every_scenario(&mut client, &conf).await;

    if let DatabasePool::Postgres(pool) = &runtime {
        pool.close().await;
    }
    admin.close().await;
    cluster
        .execute(sqlx::AssertSqlSafe(format!(
            "DROP DATABASE {database} WITH (FORCE)"
        )))
        .await?;
    cluster
        .execute(sqlx::AssertSqlSafe(format!("DROP ROLE {role}")))
        .await?;
    cluster.close().await;
    Ok(())
}

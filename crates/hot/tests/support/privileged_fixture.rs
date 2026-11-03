//! A throwaway PostgreSQL database with a restricted runtime login, and the
//! Redis next to it, for suites that run Hot the way a deployment does. The
//! suites in `hot_api` and `hot_app` include this file by path.
//!
//! The suite skips when `HOT_TEST_POSTGRES_URI` is not set. When it is set, it
//! must name a login that can create databases and roles, not the restricted
//! runtime login `hot test` uses; with any other login the suite fails.
//!
//! Redis is `HOT_TEST_REDIS_URI`, else `HOT_REDIS_URI` or `REDIS_URL`, else
//! `redis://127.0.0.1:6379`. A Redis one of those variables names must answer,
//! or the suite fails; only the unnamed default can skip it.

use sqlx::{Executor, PgPool};
use std::path::Path;
use std::time::Duration;
use uuid::Uuid;

pub type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

const DEFAULT_REDIS_URI: &str = "redis://127.0.0.1:6379";
/// In the order they are tried.
const REDIS_URI_VARIABLES: [&str; 3] = ["HOT_TEST_REDIS_URI", "HOT_REDIS_URI", "REDIS_URL"];

/// The administrator URI and Redis URI, or `None` when the suite should skip.
pub async fn services() -> TestResult<Option<(String, String)>> {
    let Ok(admin_uri) = std::env::var("HOT_TEST_POSTGRES_URI") else {
        eprintln!("skipping test: HOT_TEST_POSTGRES_URI is not set");
        return Ok(None);
    };
    let named = named_redis(|name| std::env::var(name).ok());
    let Some(redis_uri) = reachable_redis(named).await? else {
        eprintln!("skipping test: nothing answers at the default Redis address");
        return Ok(None);
    };
    Ok(Some((admin_uri, redis_uri)))
}

/// The first Redis variable that is set, and its value.
fn named_redis(lookup: impl Fn(&str) -> Option<String>) -> Option<(&'static str, String)> {
    REDIS_URI_VARIABLES.iter().find_map(|name| {
        lookup(name)
            .filter(|uri| !uri.is_empty())
            .map(|uri| (*name, uri))
    })
}

/// The Redis to use, or `None` when only the default was tried and it is not
/// there. Reports the variable rather than the URI, which may hold a password.
async fn reachable_redis(named: Option<(&'static str, String)>) -> TestResult<Option<String>> {
    let Some((name, uri)) = named else {
        return Ok(redis_answers(DEFAULT_REDIS_URI)
            .await
            .then(|| DEFAULT_REDIS_URI.to_string()));
    };
    if redis_answers(&uri).await {
        return Ok(Some(uri));
    }
    Err(format!("the Redis {name} names did not answer PING").into())
}

async fn redis_answers(uri: &str) -> bool {
    let Ok(client) = redis::Client::open(uri) else {
        return false;
    };
    let ping = async {
        let mut conn = client.get_multiplexed_async_connection().await.ok()?;
        redis::cmd("PING")
            .query_async::<String>(&mut conn)
            .await
            .ok()
    };
    matches!(
        tokio::time::timeout(Duration::from_secs(5), ping).await,
        Ok(Some(_))
    )
}

pub struct PrivilegedDb {
    cluster: PgPool,
    database: String,
    role: String,
    /// The fixture database, as the administrator.
    pub admin: PgPool,
    /// The administrator's URI for the fixture database.
    pub fixture_uri: String,
    /// The restricted runtime login's URI for the fixture database.
    pub runtime_uri: String,
}

impl PrivilegedDb {
    /// A migrated database named from `prefix`, and a runtime login granted
    /// what deployments grant it. A step that fails leaves nothing behind.
    pub async fn create(admin_uri: &str, prefix: &str) -> TestResult<Self> {
        Self::create_with_psql(admin_uri, prefix, "psql").await
    }

    async fn create_with_psql(admin_uri: &str, prefix: &str, psql: &str) -> TestResult<Self> {
        let suffix = Uuid::new_v4().simple().to_string();
        let database = format!("{prefix}_{suffix}");
        let role = format!("{prefix}_runtime_{suffix}");
        let cluster = PgPool::connect(admin_uri).await?;
        match provision(&cluster, admin_uri, &database, &role, psql).await {
            Ok((admin, fixture_uri, runtime_uri)) => Ok(Self {
                cluster,
                database,
                role,
                admin,
                fixture_uri,
                runtime_uri,
            }),
            Err(error) => {
                note_failed_cleanup(drop_database_and_role(&cluster, &database, &role).await);
                cluster.close().await;
                Err(error)
            }
        }
    }

    /// Drop the database and the login.
    pub async fn destroy(self) -> TestResult {
        self.admin.close().await;
        let dropped = drop_database_and_role(&self.cluster, &self.database, &self.role).await;
        self.cluster.close().await;
        dropped
    }

    /// Give up after a later setup step failed with `error`: drop everything
    /// and report `error`, whatever happens to the drop.
    pub async fn abandon<T>(self, error: Box<dyn std::error::Error>) -> TestResult<T> {
        note_failed_cleanup(self.destroy().await);
        Err(error)
    }
}

/// Returns the administrator pool, the fixture URI, and the runtime URI.
async fn provision(
    cluster: &PgPool,
    admin_uri: &str,
    database: &str,
    role: &str,
    psql: &str,
) -> TestResult<(PgPool, String, String)> {
    let needs_privileges = |e: sqlx::Error| {
        format!("HOT_TEST_POSTGRES_URI must be able to create databases and roles: {e}")
    };
    let password = Uuid::new_v4().simple().to_string();
    cluster
        .execute(sqlx::AssertSqlSafe(format!("CREATE DATABASE {database}")))
        .await
        .map_err(needs_privileges)?;
    cluster
        .execute(sqlx::AssertSqlSafe(format!(
            "CREATE ROLE {role} LOGIN PASSWORD '{password}' NOSUPERUSER NOBYPASSRLS \
             NOINHERIT NOCREATEDB NOCREATEROLE NOREPLICATION"
        )))
        .await
        .map_err(needs_privileges)?;

    let mut url = url::Url::parse(admin_uri)?;
    url.set_path(&format!("/{database}"));
    let fixture_uri = url.to_string();
    hot::db::run_migrations(
        &hot::val!({"uri": fixture_uri.clone(), "migration-uri": fixture_uri.clone()}),
    )
    .await?;
    let admin = PgPool::connect(&fixture_uri).await?;

    // Grant the runtime role exactly what deployments grant it.
    let provisioning_password = decoded_password(&url);
    url.set_password(None)
        .map_err(|_| "invalid fixture password")?;
    let mut command = tokio::process::Command::new(psql);
    command
        .args([
            "-X",
            "--dbname",
            url.as_str(),
            "-v",
            "ON_ERROR_STOP=1",
            "-v",
            &format!("runtime_role={role}"),
            "-v",
            "schema=hot",
            "-f",
        ])
        .arg(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../resources/db/postgres/provision_runtime.sql"),
        );
    if let Some(password) = provisioning_password {
        command.env("PGPASSWORD", password);
    }
    let output = command
        .output()
        .await
        .map_err(|e| format!("could not run {psql}: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "provision_runtime.sql failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }
    url.set_username(role).map_err(|_| "invalid fixture role")?;
    url.set_password(Some(&password))
        .map_err(|_| "invalid fixture password")?;
    Ok((admin, fixture_uri, url.to_string()))
}

/// `Url::password` keeps the URI's percent-encoding; psql wants the password.
fn decoded_password(url: &url::Url) -> Option<String> {
    // Form decoding would otherwise read `+` as a space and `&` as a separator.
    let encoded = url.password()?.replace('+', "%2B").replace('&', "%26");
    let query = format!("password={encoded}");
    let (_, password) = url::form_urlencoded::parse(query.as_bytes()).next()?;
    Some(password.into_owned())
}

/// Tries both, so a failed database drop still reports the role it strands.
async fn drop_database_and_role(cluster: &PgPool, database: &str, role: &str) -> TestResult {
    let dropped_database = cluster
        .execute(sqlx::AssertSqlSafe(format!(
            "DROP DATABASE IF EXISTS {database} WITH (FORCE)"
        )))
        .await;
    let dropped_role = cluster
        .execute(sqlx::AssertSqlSafe(format!("DROP ROLE IF EXISTS {role}")))
        .await;
    dropped_database?;
    dropped_role?;
    Ok(())
}

fn note_failed_cleanup(cleaned: TestResult) {
    if let Err(error) = cleaned {
        eprintln!("fixture cleanup also failed: {error}");
    }
}

/// A test's result from what its body did and how cleanup went. The body's
/// failure comes first; a cleanup failure only fails a test that passed.
pub fn report(outcome: std::thread::Result<TestResult>, cleaned: TestResult) -> TestResult {
    match outcome {
        Ok(Ok(())) => cleaned,
        Ok(Err(error)) => {
            note_failed_cleanup(cleaned);
            Err(error)
        }
        Err(panic) => {
            note_failed_cleanup(cleaned);
            std::panic::resume_unwind(panic)
        }
    }
}

/// Run `body` against a fresh `Fixture` and always clean it up.
macro_rules! with_fixture {
    ($body:ident) => {{
        let Some(fixture) = Fixture::start().await? else {
            return Ok(());
        };
        // Clean up after a failed assertion too, then report it.
        let outcome =
            futures::FutureExt::catch_unwind(std::panic::AssertUnwindSafe($body(&fixture))).await;
        let cleaned = fixture.cleanup().await;
        privileged_fixture::report(outcome, cleaned)
    }};
}

#[test]
fn the_redis_variables_are_tried_in_order() {
    let only = |set: &'static [&'static str]| {
        move |name: &str| set.contains(&name).then(|| format!("redis://{name}"))
    };
    assert_eq!(named_redis(only(&[])), None);
    assert_eq!(
        named_redis(only(&["REDIS_URL"])),
        Some(("REDIS_URL", "redis://REDIS_URL".to_string()))
    );
    assert_eq!(
        named_redis(only(&["REDIS_URL", "HOT_REDIS_URI"])),
        Some(("HOT_REDIS_URI", "redis://HOT_REDIS_URI".to_string()))
    );
    assert_eq!(
        named_redis(only(&["REDIS_URL", "HOT_REDIS_URI", "HOT_TEST_REDIS_URI"])),
        Some((
            "HOT_TEST_REDIS_URI",
            "redis://HOT_TEST_REDIS_URI".to_string()
        ))
    );
    assert_eq!(named_redis(|_| Some(String::new())), None);
}

#[tokio::test]
async fn a_named_redis_that_does_not_answer_fails_instead_of_skipping() {
    let named = Some(("REDIS_URL", "redis://127.0.0.1:1".to_string()));
    let error = reachable_redis(named).await.unwrap_err().to_string();
    assert!(error.contains("REDIS_URL"), "{error}");
    assert!(!error.contains("127.0.0.1"), "{error}");
}

#[test]
fn the_provisioning_password_is_percent_decoded() {
    for (uri, password) in [
        ("postgres://hot:plain@localhost/hot", Some("plain")),
        (
            "postgres://hot:p%40ss%3Aw%2Frd@localhost/hot",
            Some("p@ss:w/rd"),
        ),
        ("postgres://hot:a+b&c@localhost/hot", Some("a+b&c")),
        ("postgres://hot:100%25@localhost/hot", Some("100%")),
        ("postgres://hot@localhost/hot", None),
    ] {
        let url = url::Url::parse(uri).unwrap();
        assert_eq!(decoded_password(&url).as_deref(), password, "{uri}");
    }
}

#[test]
fn the_body_failure_comes_before_a_cleanup_failure() {
    let cleanup_failed = || -> TestResult { Err("cleanup".into()) };
    let error = report(Ok(Err("body".into())), cleanup_failed()).unwrap_err();
    assert_eq!(error.to_string(), "body");

    let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        report(Err(Box::new("assertion")), cleanup_failed())
    }))
    .expect_err("the body's panic is raised again");
    assert_eq!(panicked.downcast_ref::<&str>(), Some(&"assertion"));

    let error = report(Ok(Ok(())), cleanup_failed()).unwrap_err();
    assert_eq!(error.to_string(), "cleanup");
    assert!(report(Ok(Ok(())), Ok(())).is_ok());
}

/// A restricted login gets told what the suite needs, not a bare permission
/// error, and fails rather than skips.
#[tokio::test]
async fn a_login_without_privileges_fails_and_says_why() -> TestResult {
    let Ok(admin_uri) = std::env::var("HOT_TEST_POSTGRES_URI") else {
        return Ok(());
    };
    let role = format!("hot_fx_{}", Uuid::new_v4().simple());
    let password = Uuid::new_v4().simple().to_string();
    let cluster = PgPool::connect(&admin_uri).await?;
    cluster
        .execute(sqlx::AssertSqlSafe(format!(
            "CREATE ROLE {role} LOGIN PASSWORD '{password}' NOCREATEDB NOCREATEROLE"
        )))
        .await?;
    let mut restricted = url::Url::parse(&admin_uri)?;
    restricted
        .set_username(&role)
        .map_err(|_| "invalid fixture role")?;
    restricted
        .set_password(Some(&password))
        .map_err(|_| "invalid fixture password")?;
    let failed = PrivilegedDb::create(restricted.as_str(), "hot_fx").await;
    cluster
        .execute(sqlx::AssertSqlSafe(format!("DROP ROLE {role}")))
        .await?;
    cluster.close().await;
    let error = failed.err().expect("setup should fail").to_string();
    assert!(
        error.contains("HOT_TEST_POSTGRES_URI must be able to create databases and roles"),
        "{error}"
    );
    Ok(())
}

/// Runs against the real server, with a `psql` that cannot start: the
/// database and login already exist by then.
#[tokio::test]
async fn a_failed_setup_leaves_nothing_behind() -> TestResult {
    let Ok(admin_uri) = std::env::var("HOT_TEST_POSTGRES_URI") else {
        return Ok(());
    };
    let prefix = format!("hot_fx_{}", &Uuid::new_v4().simple().to_string()[..8]);
    let failed = PrivilegedDb::create_with_psql(&admin_uri, &prefix, "psql-is-not-installed").await;
    let error = failed.err().expect("setup should fail").to_string();
    assert!(error.contains("psql-is-not-installed"), "{error}");

    let cluster = PgPool::connect(&admin_uri).await?;
    let (databases, roles): (i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM pg_database WHERE datname LIKE $1), \
                (SELECT count(*) FROM pg_roles WHERE rolname LIKE $1)",
    )
    .bind(format!("{prefix}%"))
    .fetch_one(&cluster)
    .await?;
    cluster.close().await;
    assert_eq!((databases, roles), (0, 0));
    Ok(())
}

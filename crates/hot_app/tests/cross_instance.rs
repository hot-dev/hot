//! Two app replicas sharing one PostgreSQL database, one Redis, and one
//! storage directory, configured as the self-host preset runs them:
//! `HOT_ENV=production` and one `HOT_APP_SESSION_SECRET` for every replica.
//!
//! Each instance is the production router with its own restricted runtime
//! pool and its own Redis pub/sub. The shared directory stands in for S3 or
//! GCS. State changes that could expose a per-process cache are made out of
//! band, through the fixture administrator.
//!
//! Skips unless `HOT_TEST_POSTGRES_URI` names a privileged fixture server and
//! Redis answers at `HOT_TEST_REDIS_URI` (default `redis://127.0.0.1:6379`).
//! Redis keys are scoped to each test's fresh environment ID and deleted.

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use futures::StreamExt;
use hot::db::{self, DatabasePool, TestData};
use hot::stream::{EnvEvent, EnvPublisher, StreamPubSub, StreamPubSubType};
use hot_app::test_support::{CookieJar, TestApp, TestResponse};
use sqlx::{Executor, PgPool};
use std::sync::{Arc, Once};
use std::time::Duration;
use tower::ServiceExt;
use uuid::Uuid;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

const DELIVERY: Duration = Duration::from_secs(10);
/// The seeded user's credentials (`hot::db::insert_default_data`).
const EMAIL: &str = "local@hot.dev";
const PASSWORD: &str = "hotdev";

static PRODUCTION_MODE: Once = Once::new();

/// Run as a deployed replica: no local-development auto-login, and a session
/// secret every replica shares.
fn production_mode() {
    PRODUCTION_MODE.call_once(|| {
        // SAFETY: each test calls this before doing anything else, and the
        // values never change afterwards.
        unsafe {
            std::env::set_var("HOT_ENV", "production");
            std::env::set_var(
                "HOT_APP_SESSION_SECRET",
                "cross-instance-secret-shared-by-every-replica",
            );
        }
    });
}

struct Fixture {
    cluster: PgPool,
    admin: PgPool,
    database: String,
    role: String,
    runtime_uri: String,
    redis_uri: String,
    storage: tempfile::TempDir,
    data: TestData,
}

async fn redis_uri_if_available() -> Option<String> {
    let uri = std::env::var("HOT_TEST_REDIS_URI")
        .unwrap_or_else(|_| "redis://127.0.0.1:6379".to_string());
    let client = redis::Client::open(uri.as_str()).ok()?;
    let ping = async {
        let mut conn = client.get_multiplexed_async_connection().await.ok()?;
        redis::cmd("PING")
            .query_async::<String>(&mut conn)
            .await
            .ok()
    };
    match tokio::time::timeout(Duration::from_secs(2), ping).await {
        Ok(Some(_)) => Some(uri),
        _ => None,
    }
}

impl Fixture {
    async fn start() -> TestResult<Option<Self>> {
        production_mode();
        let Ok(admin_uri) = std::env::var("HOT_TEST_POSTGRES_URI") else {
            eprintln!("skipping cross-instance test: HOT_TEST_POSTGRES_URI is not set");
            return Ok(None);
        };
        let Some(redis_uri) = redis_uri_if_available().await else {
            eprintln!("skipping cross-instance test: Redis is not available");
            return Ok(None);
        };

        let suffix = Uuid::new_v4().simple().to_string();
        let database = format!("hot_app_xinst_{suffix}");
        let role = format!("hot_app_xinst_runtime_{suffix}");
        let password = Uuid::new_v4().simple().to_string();
        let cluster = PgPool::connect(&admin_uri).await?;
        cluster
            .execute(sqlx::AssertSqlSafe(format!("CREATE DATABASE {database}")))
            .await?;
        cluster
            .execute(sqlx::AssertSqlSafe(format!(
                "CREATE ROLE {role} LOGIN PASSWORD '{password}' NOSUPERUSER NOBYPASSRLS \
                 NOINHERIT NOCREATEDB NOCREATEROLE NOREPLICATION"
            )))
            .await?;
        let mut url = url::Url::parse(&admin_uri)?;
        url.set_path(&format!("/{database}"));
        let fixture_uri = url.to_string();
        db::run_migrations(
            &hot::val!({"uri": fixture_uri.clone(), "migration-uri": fixture_uri.clone()}),
        )
        .await?;
        let admin = PgPool::connect(&fixture_uri).await?;

        let provisioning_password = url.password().map(str::to_owned);
        url.set_password(None)
            .map_err(|_| "invalid fixture password")?;
        let mut provision = tokio::process::Command::new("psql");
        provision
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
                std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../../resources/db/postgres/provision_runtime.sql"),
            );
        if let Some(password) = provisioning_password {
            provision.env("PGPASSWORD", password);
        }
        let output = provision.output().await?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        url.set_username(&role)
            .map_err(|_| "invalid fixture role")?;
        url.set_password(Some(&password))
            .map_err(|_| "invalid fixture password")?;

        let admin_db = db::create_db_pool(
            &hot::val!({"uri": fixture_uri.clone(), "schema": "hot", "rls": {"enforce": false}}),
        )
        .await?;
        let data = db::insert_test_data(&admin_db).await?;
        if let DatabasePool::Postgres(pool) = admin_db {
            pool.close().await;
        }

        Ok(Some(Self {
            cluster,
            admin,
            database,
            role,
            runtime_uri: url.to_string(),
            redis_uri,
            storage: tempfile::tempdir()?,
            data,
        }))
    }

    fn conf(&self) -> hot::val::Val {
        hot::val!({
            "app": {"host": "localhost", "port": 4680},
            "product": {"experience": "self-host"},
            "db": {"uri": self.runtime_uri.clone(), "schema": "hot"},
            "store": {"type": "postgres"},
            "queue": {"type": "redis"},
            "redis": {"uri": self.redis_uri.clone()},
            "file": {"storage": {"type": "local", "path": self.storage.path().join("files").to_string_lossy().to_string()}},
        })
    }

    async fn runtime_db(&self) -> TestResult<DatabasePool> {
        Ok(
            db::create_db_pool(&hot::val!({"uri": self.runtime_uri.clone(), "schema": "hot"}))
                .await?,
        )
    }

    fn pubsub(&self) -> TestResult<StreamPubSub> {
        Ok(
            StreamPubSub::new(StreamPubSubType::Redis, Some(self.redis_uri.clone()), false)
                .map_err(|e| e.to_string())?,
        )
    }

    /// A new app replica: its own pool and its own Redis pub/sub.
    async fn instance(&self) -> TestResult<TestApp> {
        Ok(TestApp::spawn_with_db_and_pubsub(
            self.runtime_db().await?,
            self.conf(),
            Some(Arc::new(self.pubsub()?)),
        )
        .await)
    }

    async fn cleanup(self) -> TestResult {
        if let Ok(client) = redis::Client::open(self.redis_uri.as_str())
            && let Ok(mut conn) = client.get_multiplexed_async_connection().await
        {
            let _: redis::RedisResult<()> = redis::cmd("DEL")
                .arg(format!("hot:env:{{{}}}", self.data.env_id))
                .query_async(&mut conn)
                .await;
        }
        self.admin.close().await;
        self.cluster
            .execute(sqlx::AssertSqlSafe(format!(
                "DROP DATABASE {} WITH (FORCE)",
                self.database
            )))
            .await?;
        self.cluster
            .execute(sqlx::AssertSqlSafe(format!("DROP ROLE {}", self.role)))
            .await?;
        self.cluster.close().await;
        Ok(())
    }
}

macro_rules! with_fixture {
    ($body:ident) => {{
        let Some(fixture) = Fixture::start().await? else {
            return Ok(());
        };
        // Clean up after a failed assertion too, then report it.
        let outcome =
            futures::FutureExt::catch_unwind(std::panic::AssertUnwindSafe($body(&fixture))).await;
        fixture.cleanup().await?;
        match outcome {
            Ok(result) => result,
            Err(panic) => std::panic::resume_unwind(panic),
        }
    }};
}

/// One browser: a cookie jar sent to whichever instance serves the request.
#[derive(Default)]
struct Browser {
    cookies: CookieJar,
}

impl Browser {
    fn request(&self, method: &str, path: &str) -> axum::http::request::Builder {
        let mut builder = Request::builder()
            .method(method)
            .uri(path)
            .header(header::HOST, "localhost");
        if let Some(cookie) = self.cookies.header_value() {
            builder = builder.header(header::COOKIE, cookie);
        }
        builder
    }

    async fn get(&mut self, app: &TestApp, path: &str) -> TestResponse {
        let response = app
            .send(self.request("GET", path).body(Body::empty()).unwrap())
            .await;
        self.cookies.merge(&response.set_cookies);
        response
    }

    async fn post_form(
        &mut self,
        app: &TestApp,
        path: &str,
        form: &[(&str, &str)],
    ) -> TestResponse {
        let body = form
            .iter()
            .map(|(k, v)| format!("{}={}", urlencoding::encode(k), urlencoding::encode(v)))
            .collect::<Vec<_>>()
            .join("&");
        let response = app
            .send(
                self.request("POST", path)
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await;
        self.cookies.merge(&response.set_cookies);
        response
    }

    /// Render the sign-in form on `form_app`, submit it to `submit_app`.
    async fn sign_in(&mut self, form_app: &TestApp, submit_app: &TestApp) {
        self.get(form_app, "/signin")
            .await
            .assert_status(StatusCode::OK);
        let form_token = self
            .cookies
            .get(hot_app::auth::csrf_cookie_name())
            .expect("the sign-in page sets the CSRF cookie")
            .to_string();
        self.post_form(
            submit_app,
            "/signin",
            &[
                ("email", EMAIL),
                ("password", PASSWORD),
                ("form_token", &form_token),
            ],
        )
        .await
        .assert_redirect_to("/");
        assert!(
            self.cookies.get(hot_app::auth::jwt_cookie_name()).is_some(),
            "sign-in must set the session cookie"
        );
    }
}

/// A sign-in form rendered by A and submitted to B yields a session that A,
/// B, and a replacement instance all accept. Deactivating the user out of
/// band ends it on every instance at the next request.
#[tokio::test]
async fn app_sessions_span_instances() -> TestResult {
    with_fixture!(sessions_body)
}

async fn sessions_body(fx: &Fixture) -> TestResult {
    let a = fx.instance().await?;
    let b = fx.instance().await?;
    let mut browser = Browser::default();

    // Signed out, every instance sends the browser to sign in.
    browser
        .get(&a, "/projects")
        .await
        .assert_redirect_to("/signin");

    browser.sign_in(&a, &b).await;
    for app in [&a, &b] {
        let projects = browser.get(app, "/projects").await;
        projects.assert_status(StatusCode::OK);
        assert!(projects.body.contains("test-project"));
    }

    drop(a);
    let replacement = fx.instance().await?;
    browser
        .get(&replacement, "/projects")
        .await
        .assert_status(StatusCode::OK);

    sqlx::query("UPDATE hot.\"user\" SET active = false WHERE user_id = $1")
        .bind(fx.data.user_id)
        .execute(&fx.admin)
        .await?;
    let session = browser.cookies.clone();
    for app in [&b, &replacement] {
        let mut same_browser = Browser {
            cookies: session.clone(),
        };
        same_browser
            .get(app, "/projects")
            .await
            .assert_redirect_to("/signin");
    }
    Ok(())
}

/// The dashboard's live-update stream on one instance receives environment
/// events another process (a worker) publishes, including after the
/// handler's periodic revalidation has interrupted its Redis read.
#[tokio::test]
async fn dashboard_updates_reach_another_instance() -> TestResult {
    with_fixture!(dashboard_body)
}

async fn dashboard_body(fx: &Fixture) -> TestResult {
    let a = fx.instance().await?;
    let b = fx.instance().await?;
    let mut browser = Browser::default();
    browser.sign_in(&a, &a).await;

    let response = b
        .router
        .clone()
        .oneshot(
            browser
                .request("GET", "/env/subscribe")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    let (tx, mut events) = tokio::sync::mpsc::unbounded_channel::<String>();
    let reader = tokio::spawn(async move {
        let mut body = response.into_body().into_data_stream();
        let mut buffer = String::new();
        while let Some(Ok(chunk)) = body.next().await {
            buffer.push_str(&String::from_utf8_lossy(&chunk));
            while let Some(end) = buffer.find("\n\n") {
                let frame: String = buffer.drain(..end + 2).collect();
                if let Some(data) = frame.lines().find_map(|line| line.strip_prefix("data:"))
                    && tx.send(data.trim().to_string()).is_err()
                {
                    return;
                }
            }
        }
    });

    let worker = fx.pubsub()?;
    for _ in 0..3 {
        let run_id = Uuid::now_v7();
        worker
            .publish_env(EnvEvent::RunStart {
                run_id,
                env_id: fx.data.env_id,
                stream_id: fx.data.stream_id,
                event_id: None,
                project_id: Some(fx.data.project_id),
                fn_name: Some("::app/handler".into()),
                run_type: "event".into(),
            })
            .await?;
        let deadline = tokio::time::Instant::now() + DELIVERY;
        loop {
            let data = tokio::time::timeout_at(deadline, events.recv())
                .await
                .map_err(|_| format!("run {run_id} did not reach the dashboard stream"))?
                .ok_or("dashboard stream ended")?;
            if data.contains(&run_id.to_string()) {
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(1500)).await;
    }
    reader.abort();
    Ok(())
}

/// A file another process writes to the shared storage downloads through
/// every app instance.
#[tokio::test]
async fn files_written_elsewhere_download_from_every_instance() -> TestResult {
    with_fixture!(files_body)
}

async fn files_body(fx: &Fixture) -> TestResult {
    let writer_db = Arc::new(fx.runtime_db().await?);
    let storage = hot::file_storage::file_storage_from_config(&fx.conf()).await?;
    let content = b"exported by a worker on another host".to_vec();
    let written = storage
        .write_file(
            "exports/report.txt",
            &content,
            Some("text/plain"),
            &hot::file_storage::FileStorageContext::minimal(
                Arc::new(writer_db.with_scope(db::DatabaseScope::Environment {
                    org_id: fx.data.org_id,
                    env_id: fx.data.env_id,
                })),
                fx.data.org_id,
                Some(fx.data.env_id),
                fx.data.user_id,
            ),
        )
        .await?;

    let a = fx.instance().await?;
    let b = fx.instance().await?;
    let mut browser = Browser::default();
    browser.sign_in(&b, &a).await;
    for app in [&a, &b] {
        let download = browser
            .get(app, &format!("/files/{}/download", written.file_id))
            .await;
        download.assert_status(StatusCode::OK);
        assert_eq!(download.body.as_bytes(), content.as_slice());
    }
    Ok(())
}

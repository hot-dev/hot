//! Two app replicas sharing one PostgreSQL database, one Redis, and one
//! storage directory, configured as the self-host preset runs them:
//! `HOT_ENV=production` and one `HOT_APP_SESSION_SECRET` for every replica.
//!
//! Each instance is the production router with its own restricted runtime
//! pool and its own Redis pub/sub. The shared directory stands in for S3 or
//! GCS. State changes that could expose a per-process cache are made out of
//! band, through the fixture administrator.
//!
//! `privileged_fixture` says when these tests skip and when they fail: without
//! `HOT_TEST_POSTGRES_URI` they skip, and it must name a login that can create
//! databases and roles. Redis keys are scoped to each test's fresh environment
//! ID and deleted.

#[macro_use]
#[path = "../../hot/tests/support/privileged_fixture.rs"]
mod privileged_fixture;
#[path = "../../hot/tests/support/sse_reader.rs"]
mod sse_reader;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use hot::db::{self, DatabasePool, TestData};
use hot::stream::{EnvEvent, EnvPublisher, StreamPubSub, StreamPubSubType};
use hot_app::test_support::{CookieJar, TestApp, TestResponse};
use privileged_fixture::{PrivilegedDb, TestResult};
use sse_reader::SseReader;
use std::sync::{Arc, Once, OnceLock};
use tower::ServiceExt;
use uuid::Uuid;

/// The seeded user (`hot::db::insert_test_data`).
const EMAIL: &str = "local@hot.dev";

/// The password the fixture gives the seeded user, new for each run.
fn user_password() -> &'static str {
    static PASSWORD: OnceLock<String> = OnceLock::new();
    PASSWORD.get_or_init(|| Uuid::new_v4().simple().to_string())
}

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
    postgres: PrivilegedDb,
    redis_uri: String,
    storage: tempfile::TempDir,
    data: TestData,
}

/// Seed through the administrator, as onboarding would.
async fn seed(postgres: &PrivilegedDb) -> TestResult<TestData> {
    let admin_db = db::create_db_pool(
        &hot::val!({"uri": postgres.fixture_uri.clone(), "schema": "hot", "rls": {"enforce": false}}),
    )
    .await?;
    let data = db::insert_test_data(&admin_db).await?;
    // The local identity has no password; give it one, as
    // `hot admin bootstrap` does for an installation's administrator.
    let hash = hot::auth::hash_password_with_random_salt(user_password())?;
    hot::db::UserAuth::insert_user_auth(
        &admin_db.with_scope(db::DatabaseScope::Bootstrap(
            db::BootstrapOperation::Onboarding,
        )),
        &Uuid::now_v7(),
        &data.user_id,
        "email_password",
        EMAIL,
        Some(&serde_json::from_str(&hash)?),
        &data.user_id,
    )
    .await?;
    if let DatabasePool::Postgres(pool) = admin_db {
        pool.close().await;
    }
    Ok(data)
}

impl Fixture {
    async fn start() -> TestResult<Option<Self>> {
        production_mode();
        let Some((admin_uri, redis_uri)) = privileged_fixture::services().await? else {
            return Ok(None);
        };
        let storage = tempfile::tempdir()?;
        let postgres = PrivilegedDb::create(&admin_uri, "hot_app_xinst").await?;
        match seed(&postgres).await {
            Ok(data) => Ok(Some(Self {
                postgres,
                redis_uri,
                storage,
                data,
            })),
            Err(error) => postgres.abandon(error).await,
        }
    }

    fn conf(&self) -> hot::val::Val {
        hot::val!({
            "app": {"host": "localhost", "port": 4680},
            "product": {"experience": "self-host"},
            "db": {"uri": self.postgres.runtime_uri.clone(), "schema": "hot"},
            "store": {"type": "postgres"},
            "queue": {"type": "redis"},
            "redis": {"uri": self.redis_uri.clone()},
            "file": {"storage": {"type": "local", "path": self.storage.path().join("files").to_string_lossy().to_string()}},
        })
    }

    async fn runtime_db(&self) -> TestResult<DatabasePool> {
        Ok(db::create_db_pool(
            &hot::val!({"uri": self.postgres.runtime_uri.clone(), "schema": "hot"}),
        )
        .await?)
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
        self.postgres.destroy().await
    }
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
                ("password", user_password()),
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
        .execute(&fx.postgres.admin)
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
/// events another process (a worker) publishes, one after another. Reads cut
/// short by the handler's periodic revalidation are covered by
/// `env_subscriber_keeps_entries_through_cancelled_waits` in `hot`.
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
    let mut events = SseReader::new(response);

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
        let run = run_id.to_string();
        events
            .next_where(&format!("dashboard event for run {run}"), |_, data| {
                data.contains(&run)
            })
            .await;
    }
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

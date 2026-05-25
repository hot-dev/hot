//! HTTP-level integration test support for `hot_app`.
//!
//! This module provides a thin harness for exercising the real Axum `Router`
//! against an in-memory SQLite database with all production migrations
//! applied. It lets tests drive the app the way a browser does — issuing HTTP
//! requests, following redirects, and preserving cookies across hops — without
//! needing to stand up a real TCP listener.
//!
//! # Gated by the `test-utils` feature
//!
//! To keep production builds lean, this module is compiled only when the
//! `test-utils` feature is enabled. Tests inside this crate opt in via the
//! `dev-dependencies.hot_app = { features = ["test-utils"] }` line in
//! `Cargo.toml`.
//!
//! # What's NOT covered
//!
//! - provider checkout calls. Integration tests assert redirects up to
//!   `/billing/create-checkout-form?...`; the provider checkout itself is
//!   out of scope.
//! - Email delivery. The verification email is enqueued to the database
//!   `hot:email` queue rather than sent. Tests read the verification token
//!   directly from `email_verification`.
//! - Streaming/SSE endpoints. Real-time subscribers are stubbed out
//!   (`stream_pubsub = None`).
//!
//! # Minimal example
//!
//! ```no_run
//! # async fn example() {
//! use hot_app::test_support::TestClient;
//!
//! let mut client = TestClient::new().await;
//! let response = client.get("/status").await;
//! assert_eq!(response.status, axum::http::StatusCode::OK);
//! # }
//! ```

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use hot::db::DatabasePool;
use hot::val;
use hot::val::Val;
use http_body_util::BodyExt;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::watch;
use tower::ServiceExt;

/// A self-contained Axum application wired up to an in-memory SQLite DB.
///
/// `TestApp` is stateless with respect to cookies — prefer [`TestClient`] for
/// most tests. Use `TestApp` directly only when you need fine-grained control
/// (e.g. testing a single handler without a persistent session).
pub struct TestApp {
    pub db: Arc<DatabasePool>,
    pub router: Router,
    pub conf: Val,
    /// Kept alive so watch receivers inside handlers don't see `Err(RecvError)`.
    _shutdown_tx: watch::Sender<bool>,
}

impl TestApp {
    /// Build a fresh `TestApp` with:
    /// - An in-memory SQLite DB with all migrations applied.
    /// - Minimal `Val` config (app defaults: host/port placeholder, 24h session).
    /// - No `StreamPubSub` (SSE endpoints won't produce real-time updates).
    ///
    /// The DB starts empty — no seed user/org. This means guest-only routes
    /// like `/signup` behave as if no one is signed in (the local-dev
    /// auto-login fallback in `session_middleware` finds no users and
    /// falls through).
    pub async fn spawn() -> Self {
        Self::spawn_with_conf(minimal_conf()).await
    }

    pub async fn spawn_with_conf(conf: Val) -> Self {
        let db = setup_db().await;
        let db = Arc::new(db);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);

        let router = crate::routes::routes(db.clone(), conf.clone(), None, shutdown_rx);

        Self {
            db,
            router,
            conf,
            _shutdown_tx: shutdown_tx,
        }
    }

    /// Send a single request through the router. Does NOT follow redirects
    /// or track cookies. Use [`TestClient`] for flows that need either.
    pub async fn send(&self, req: Request<Body>) -> TestResponse {
        let response = self
            .router
            .clone()
            .oneshot(req)
            .await
            .expect("router oneshot failed");

        let status = response.status();
        let headers = response.headers().clone();

        let set_cookies: Vec<String> = response
            .headers()
            .get_all(header::SET_COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok().map(|s| s.to_string()))
            .collect();

        let body_bytes = response
            .into_body()
            .collect()
            .await
            .expect("collect body")
            .to_bytes();
        let body = String::from_utf8_lossy(&body_bytes).to_string();

        TestResponse {
            status,
            headers,
            body,
            set_cookies,
        }
    }
}

/// An HTTP client with a persistent cookie jar, bound to one [`TestApp`].
///
/// Mirrors the browser's behavior just enough for signup/verify/checkout
/// flows: merges `Set-Cookie` headers from each response into its jar and
/// sends the accumulated jar back on subsequent requests.
pub struct TestClient {
    pub app: TestApp,
    cookies: CookieJar,
}

impl TestClient {
    /// Build a fresh client over a fresh `TestApp`.
    pub async fn new() -> Self {
        Self::new_with_conf(minimal_conf()).await
    }

    pub async fn new_with_conf(conf: Val) -> Self {
        Self {
            app: TestApp::spawn_with_conf(conf).await,
            cookies: CookieJar::default(),
        }
    }

    /// Direct access to the underlying DB (for seeding and assertions).
    pub fn db(&self) -> &Arc<DatabasePool> {
        &self.app.db
    }

    /// Mint a JWT for `user_id` and stash it in the cookie jar so subsequent
    /// requests behave as if the user is signed in. Bypasses the full signup
    /// flow — useful for tests that only exercise post-login behavior (e.g.
    /// /claim-handle orphan recovery).
    pub fn login_as(&mut self, user_id: &uuid::Uuid) {
        let token = crate::auth::generate_token(user_id, &self.app.conf).expect("generate_token");
        self.cookies
            .inner
            .insert(crate::auth::JWT_COOKIE_NAME.to_string(), token);
    }

    /// Get the current cookie jar as a read-only snapshot.
    pub fn cookies(&self) -> &CookieJar {
        &self.cookies
    }

    /// GET `path`. Cookies are attached from the jar and merged from
    /// `Set-Cookie` response headers into the jar.
    pub async fn get(&mut self, path: &str) -> TestResponse {
        self.get_with_headers(path, &[]).await
    }

    /// GET `path` with extra request headers (e.g. `HX-Request: true` for
    /// HTMX-style partial responses).
    pub async fn get_with_headers(
        &mut self,
        path: &str,
        extra_headers: &[(&str, &str)],
    ) -> TestResponse {
        let mut builder = Request::builder().method("GET").uri(path);
        if let Some(cookie) = self.cookies.header_value() {
            builder = builder.header(header::COOKIE, cookie);
        }
        for (name, value) in extra_headers {
            builder = builder.header(*name, *value);
        }
        let req = builder.body(Body::empty()).expect("build request");
        let resp = self.app.send(req).await;
        self.cookies.merge(&resp.set_cookies);
        resp
    }

    /// POST form-encoded body to `path`.
    pub async fn post_form(&mut self, path: &str, form: &[(&str, &str)]) -> TestResponse {
        let body = urlencoded(form);
        let mut builder = Request::builder()
            .method("POST")
            .uri(path)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(header::CONTENT_LENGTH, body.len());
        if let Some(cookie) = self.cookies.header_value() {
            builder = builder.header(header::COOKIE, cookie);
        }
        let req = builder.body(Body::from(body)).expect("build request");
        let resp = self.app.send(req).await;
        self.cookies.merge(&resp.set_cookies);
        resp
    }

    /// If `response` is a redirect (3xx with a `Location` header), follow
    /// it with a GET and return the next response. Returns `None` otherwise.
    pub async fn follow_redirect(&mut self, response: &TestResponse) -> Option<TestResponse> {
        let location = response.location()?.to_string();
        Some(self.get(&location).await)
    }

    /// Follow up to `max_hops` redirects. Stops when a non-3xx response is
    /// returned or the cap is reached. Panics if the chain exceeds `max_hops`.
    pub async fn follow_redirects(
        &mut self,
        mut response: TestResponse,
        max_hops: usize,
    ) -> TestResponse {
        for _ in 0..max_hops {
            if !response.is_redirect() {
                return response;
            }
            response = self
                .follow_redirect(&response)
                .await
                .expect("redirect had no Location");
        }
        if response.is_redirect() {
            panic!(
                "redirect chain exceeded {} hops (last Location = {:?})",
                max_hops,
                response.location()
            );
        }
        response
    }
}

/// A captured HTTP response from [`TestApp::send`] or [`TestClient`].
#[derive(Debug)]
pub struct TestResponse {
    pub status: StatusCode,
    pub headers: axum::http::HeaderMap,
    pub body: String,
    /// Raw `Set-Cookie` header values (one entry per header, pre-parse).
    pub set_cookies: Vec<String>,
}

impl TestResponse {
    /// True for 3xx responses that carry a `Location` header.
    pub fn is_redirect(&self) -> bool {
        self.status.is_redirection() && self.location().is_some()
    }

    /// Returns the `Location` header value, if present.
    pub fn location(&self) -> Option<&str> {
        self.headers
            .get(header::LOCATION)
            .and_then(|v| v.to_str().ok())
    }

    /// Assert status code, panicking on mismatch with a helpful snippet.
    pub fn assert_status(&self, expected: StatusCode) -> &Self {
        assert_eq!(
            self.status,
            expected,
            "unexpected status: body = {}",
            self.body.chars().take(400).collect::<String>()
        );
        self
    }

    /// Assert that this is a redirect to `expected` (prefix match — trailing
    /// query strings count). Panics on mismatch.
    pub fn assert_redirect_to(&self, expected: &str) -> &Self {
        let location = self
            .location()
            .unwrap_or_else(|| panic!("expected redirect, got {}: {}", self.status, self.body));
        assert!(
            location.starts_with(expected),
            "expected redirect to start with `{}`, got `{}`",
            expected,
            location
        );
        self
    }
}

/// A minimal cookie jar: stores name → value pairs and serializes to a
/// `Cookie:` header. Ignores attributes beyond `name=value` — sufficient for
/// the stateful flows these tests drive.
#[derive(Debug, Default, Clone)]
pub struct CookieJar {
    inner: HashMap<String, String>,
}

impl CookieJar {
    /// Get a stored cookie value by name.
    pub fn get(&self, name: &str) -> Option<&str> {
        self.inner.get(name).map(|s| s.as_str())
    }

    /// Total stored cookies.
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    /// True if no cookies are stored.
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// Serialize to a `Cookie:` header value (or `None` if empty).
    pub fn header_value(&self) -> Option<String> {
        if self.inner.is_empty() {
            return None;
        }
        let mut parts: Vec<String> = self
            .inner
            .iter()
            .map(|(k, v)| format!("{}={}", k, v))
            .collect();
        parts.sort();
        Some(parts.join("; "))
    }

    /// Absorb an iterator of raw `Set-Cookie` header values.
    pub fn merge(&mut self, set_cookies: &[String]) {
        for raw in set_cookies {
            // Take "name=value" before the first `;`.
            let head = raw.split(';').next().unwrap_or(raw).trim();
            let Some((name, value)) = head.split_once('=') else {
                continue;
            };
            // Empty value on a Set-Cookie typically means deletion. Honor
            // that so logout/clear flows work as expected.
            if value.is_empty() {
                self.inner.remove(name.trim());
            } else {
                self.inner
                    .insert(name.trim().to_string(), value.trim().to_string());
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers re-exported for test convenience
// ---------------------------------------------------------------------------

/// Mint a form token backdated past the anti-bot minimum submission time.
///
/// Re-exported from `handlers::auth::mint_form_token_for_tests` so tests can
/// import it from a single stable path.
pub fn mint_form_token() -> String {
    crate::handlers::auth::mint_form_token_for_tests()
}

// ---------------------------------------------------------------------------
// Internals
// ---------------------------------------------------------------------------

async fn setup_db() -> DatabasePool {
    let db_conf = val!({
        "uri": "sqlite::memory:",
        "schema": "hot"
    });
    let db = hot::db::create_db_pool(&db_conf)
        .await
        .expect("create db pool");

    match &db {
        DatabasePool::Sqlite(pool) => {
            let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
            let migration_path = manifest_dir
                .parent()
                .unwrap()
                .parent()
                .unwrap()
                .join("resources/db/sqlite/migrations");

            let migrator = sqlx::migrate::Migrator::new(migration_path)
                .await
                .expect("build migrator");
            migrator.run(pool).await.expect("run migrations");
        }
        _ => panic!("tests require SQLite DatabasePool"),
    }

    db
}

fn minimal_conf() -> Val {
    // The router only reads a few keys off `conf`; supply just those so
    // handlers get sensible defaults.
    val!({
        "app": {
            "host": "localhost",
            "port": 4680
        }
    })
}

fn urlencoded(form: &[(&str, &str)]) -> String {
    form.iter()
        .map(|(k, v)| format!("{}={}", urlencoding::encode(k), urlencoding::encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

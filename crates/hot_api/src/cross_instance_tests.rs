//! Two API replicas sharing one PostgreSQL database, one Redis, and one
//! storage directory, as the self-host scalable preset runs them.
//!
//! Each instance comes from `build_app`, which builds the router `hot api`
//! serves, with its own database pool, storage clients, blob store, Redis
//! pub/sub, and domain cache. The shared directory stands in for S3 or GCS.
//! The database is a fresh fixture database reached through a restricted
//! runtime login, as in production.
//!
//! Two instances in one process also share process-global statics that two
//! replicas would not: the SQLite-only entity caches, the API rate-limit
//! state, and the lazily created queue clients. The cache tests therefore
//! change state out of band, through the fixture's administrator connection,
//! so a stale per-process value could not be refreshed by the other instance's
//! own write.
//!
//! Skips unless `HOT_TEST_POSTGRES_URI` names a privileged fixture server and
//! Redis answers at `HOT_TEST_REDIS_URI` (default `redis://127.0.0.1:6379`).
//! Redis keys a test writes are named by its fresh environment ID or by an
//! MCP transport session it opened, and the test deletes them when it
//! finishes.

use crate::server::build_app;
use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode};
use axum::response::Response;
use futures::StreamExt;
use hot::db::{self, DatabasePool, TestData};
use hot::stream::{
    EnvEvent, EnvPublisher, StreamEvent, StreamPubSub, StreamPubSubType, StreamPublisher,
};
use hot::val::Val;
use serde_json::{Value, json};
use sqlx::{Executor, PgPool};
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tower::ServiceExt;
use uuid::Uuid;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

/// Longest wait for a cross-instance delivery.
const DELIVERY: Duration = Duration::from_secs(10);

struct Fixture {
    cluster: PgPool,
    admin: PgPool,
    database: String,
    role: String,
    runtime_uri: String,
    redis_uri: String,
    storage: tempfile::TempDir,
    data: TestData,
    org_slug: String,
    env_name: String,
    api_key_id: Uuid,
    api_key: String,
    /// Redis keys named by something other than the environment ID.
    redis_keys: std::sync::Mutex<Vec<String>>,
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
    /// A fresh database, runtime role, environment, and API key, or `None`
    /// when PostgreSQL or Redis is not available.
    async fn start() -> TestResult<Option<Self>> {
        let Ok(admin_uri) = std::env::var("HOT_TEST_POSTGRES_URI") else {
            eprintln!("skipping cross-instance test: HOT_TEST_POSTGRES_URI is not set");
            return Ok(None);
        };
        let Some(redis_uri) = redis_uri_if_available().await else {
            eprintln!("skipping cross-instance test: Redis is not available");
            return Ok(None);
        };

        let suffix = Uuid::new_v4().simple().to_string();
        let database = format!("hot_xinst_{suffix}");
        let role = format!("hot_xinst_runtime_{suffix}");
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

        // Grant the runtime role exactly what deployments grant it.
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
                Path::new(env!("CARGO_MANIFEST_DIR"))
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
        let runtime_uri = url.to_string();

        // Seed through the administrator, as onboarding would.
        let admin_db = db::create_db_pool(
            &hot::val!({"uri": fixture_uri.clone(), "schema": "hot", "rls": {"enforce": false}}),
        )
        .await?;
        let data = db::insert_test_data(&admin_db).await?;
        let (org_slug, env_name): (String, String) = sqlx::query_as(
            "SELECT o.slug, e.name FROM hot.env e JOIN hot.org o ON o.org_id = e.org_id \
             WHERE e.env_id = $1",
        )
        .bind(data.env_id)
        .fetch_one(&admin)
        .await?;
        let api_key_id = Uuid::now_v7();
        let (api_key, key_data) = db::api_key::ApiKey::generate_api_key(&api_key_id)?;
        db::api_key::ApiKey::insert_api_key(
            &admin_db.with_scope(db::DatabaseScope::Environment {
                org_id: data.org_id,
                env_id: data.env_id,
            }),
            &api_key_id,
            &data.env_id,
            "cross-instance",
            &serde_json::from_str(&key_data)?,
            &data.user_id,
            &json!({"*:*": ["*"]}),
        )
        .await?;
        if let DatabasePool::Postgres(pool) = admin_db {
            pool.close().await;
        }

        Ok(Some(Self {
            cluster,
            admin,
            database,
            role,
            runtime_uri,
            redis_uri,
            storage: tempfile::tempdir()?,
            data,
            org_slug,
            env_name,
            api_key_id,
            api_key,
            redis_keys: Default::default(),
        }))
    }

    /// Configuration every replica receives: the same database, Redis, and
    /// storage.
    fn conf(&self) -> Val {
        let root = self.storage.path();
        hot::val!({
            "db": {"uri": self.runtime_uri.clone(), "schema": "hot"},
            "queue": {"type": "redis"},
            "redis": {"uri": self.redis_uri.clone()},
            "build": {
                "storage": {"type": "local", "path": root.join("builds").to_string_lossy().to_string()},
                "file": {"max-bytes": 10_485_760i64},
            },
            "file": {"storage": {"type": "local", "path": root.join("files").to_string_lossy().to_string()}},
            "blob": {"mode": "service", "spill": {"threshold-bytes": 1024i64}},
            "domain": {"mode": "none"},
        })
    }

    /// A new API replica.
    async fn instance(&self) -> Router {
        build_app(&self.conf(), None)
            .await
            .layer(axum::Extension(ConnectInfo(SocketAddr::from((
                [127, 0, 0, 1],
                40000,
            )))))
    }

    /// Another process publishing to Redis, as a worker does.
    fn worker_pubsub(&self) -> TestResult<StreamPubSub> {
        Ok(
            StreamPubSub::new(StreamPubSubType::Redis, Some(self.redis_uri.clone()), false)
                .map_err(|e| e.to_string())?,
        )
    }

    fn mcp_path(&self) -> String {
        format!("/mcp/{}/{}/tools", self.org_slug, self.env_name)
    }

    async fn cleanup(self) -> TestResult {
        if let Ok(client) = redis::Client::open(self.redis_uri.as_str())
            && let Ok(mut conn) = client.get_multiplexed_async_connection().await
        {
            // Stream channels (MCP replies included) and the env channel.
            let env = self.data.env_id;
            let mut keys = std::mem::take(&mut *self.redis_keys.lock().unwrap());
            for pattern in [
                format!("hot:stream:{{{env}:*"),
                format!("hot:env:{{{env}}}"),
            ] {
                let mut cursor: u64 = 0;
                loop {
                    let (next, batch): (u64, Vec<String>) = redis::cmd("SCAN")
                        .arg(cursor)
                        .arg("MATCH")
                        .arg(&pattern)
                        .arg("COUNT")
                        .arg(1000)
                        .query_async(&mut conn)
                        .await?;
                    keys.extend(batch);
                    cursor = next;
                    if cursor == 0 {
                        break;
                    }
                }
            }
            if !keys.is_empty() {
                let _: () = redis::cmd("DEL").arg(&keys).query_async(&mut conn).await?;
            }
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

/// Run `body` against a fresh fixture and always clean it up.
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

fn request(method: &str, path: &str, token: Option<&str>, body: Body) -> Request<Body> {
    let mut builder = Request::builder()
        .method(method)
        .uri(path)
        .header("host", "localhost");
    if let Some(token) = token {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    builder.body(body).expect("request should build")
}

async fn send(app: &Router, request: Request<Body>) -> Response {
    app.clone()
        .oneshot(request)
        .await
        .expect("router is infallible")
}

async fn json_call(
    app: &Router,
    method: &str,
    path: &str,
    token: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut request = request(
        method,
        path,
        Some(token),
        body.map_or_else(Body::empty, |body| Body::from(body.to_string())),
    );
    request
        .headers_mut()
        .insert("content-type", "application/json".parse().unwrap());
    let response = send(app, request).await;
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let value = serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()));
    (status, value)
}

async fn bytes_call(app: &Router, path: &str, token: &str) -> (StatusCode, Vec<u8>) {
    let response = send(app, request("GET", path, Some(token), Body::empty())).await;
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (status, bytes.to_vec())
}

/// Server-sent events read from a response body. A task drains the body
/// continuously, as the HTTP server does for a connected client.
struct SseReader {
    events: mpsc::UnboundedReceiver<(String, String)>,
    task: tokio::task::JoinHandle<()>,
}

impl SseReader {
    fn new(response: Response) -> Self {
        assert_eq!(response.status(), StatusCode::OK, "SSE request failed");
        let (tx, events) = mpsc::unbounded_channel();
        let task = tokio::spawn(async move {
            let mut body = response.into_body().into_data_stream();
            let mut buffer = String::new();
            while let Some(Ok(chunk)) = body.next().await {
                buffer.push_str(&String::from_utf8_lossy(&chunk));
                while let Some(end) = buffer.find("\n\n") {
                    let frame: String = buffer.drain(..end + 2).collect();
                    let mut event = "message".to_string();
                    let mut data = Vec::new();
                    for line in frame.lines() {
                        if let Some(name) = line.strip_prefix("event:") {
                            event = name.trim().to_string();
                        } else if let Some(value) = line.strip_prefix("data:") {
                            data.push(value.strip_prefix(' ').unwrap_or(value).to_string());
                        }
                    }
                    if !data.is_empty() && tx.send((event, data.join("\n"))).is_err() {
                        return;
                    }
                }
            }
        });
        Self { events, task }
    }

    /// The data of the next event named `name`, skipping other events.
    async fn next(&mut self, name: &str) -> String {
        let deadline = tokio::time::Instant::now() + DELIVERY;
        loop {
            match tokio::time::timeout_at(deadline, self.events.recv()).await {
                Ok(Some((event, data))) if event == name => return data,
                Ok(Some(_)) => continue,
                Ok(None) => panic!("SSE stream ended while waiting for `{name}`"),
                Err(_) => panic!("no `{name}` event within {DELIVERY:?}"),
            }
        }
    }
}

impl Drop for SseReader {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn json_rpc(id: i64, method: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "method": method, "params": {}})
}

/// Legacy MCP HTTP+SSE: the stream opens on A, the paired message POSTs land
/// on B and on a replacement for B, and every reply reaches A's stream through
/// Redis. The transport-session binding lives in Redis, so B also enforces it.
#[tokio::test]
async fn mcp_http_sse_replies_reach_the_stream_on_another_instance() -> TestResult {
    with_fixture!(mcp_http_sse_body)
}

async fn mcp_http_sse_body(fx: &Fixture) -> TestResult {
    let a = fx.instance().await;
    let b = fx.instance().await;

    let mut open = request("GET", &fx.mcp_path(), Some(&fx.api_key), Body::empty());
    open.headers_mut()
        .insert("accept", "text/event-stream".parse()?);
    let mut stream = SseReader::new(send(&a, open).await);
    let endpoint = stream.next("endpoint").await;
    let session_id = endpoint
        .strip_prefix(&format!("{}/messages?sessionId=", fx.mcp_path()))
        .unwrap_or_else(|| panic!("unexpected endpoint {endpoint}"));
    // The binding expires on its own; delete it with the fixture.
    fx.redis_keys
        .lock()
        .unwrap()
        .push(format!("hot:mcp:http-sse-session:{{{session_id}}}"));

    let post = |app: Router, id: i64, method: &'static str, token: String, path: String| async move {
        json_call(&app, "POST", &path, &token, Some(json_rpc(id, method))).await
    };
    let reply = |data: String| -> Value { serde_json::from_str(&data).expect("JSON-RPC reply") };

    let (status, _) = post(
        b.clone(),
        1,
        "initialize",
        fx.api_key.clone(),
        endpoint.clone(),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let initialized = reply(stream.next("message").await);
    assert_eq!(initialized["id"], 1);
    assert!(
        initialized["result"]["protocolVersion"].is_string(),
        "{initialized}"
    );

    let (status, _) = post(
        b.clone(),
        2,
        "tools/list",
        fx.api_key.clone(),
        endpoint.clone(),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let tools = reply(stream.next("message").await);
    assert_eq!(tools["id"], 2);
    assert!(tools["result"]["tools"].is_array(), "{tools}");

    // B's binding check reads Redis: an unknown session and another
    // credential are refused there too.
    let unknown = format!("{}/messages?sessionId={}", fx.mcp_path(), Uuid::now_v7());
    let (status, body) = post(b.clone(), 3, "ping", fx.api_key.clone(), unknown).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    let other_key_id = Uuid::now_v7();
    let (other_key, other_key_data) = db::api_key::ApiKey::generate_api_key(&other_key_id)?;
    sqlx::query(
        "INSERT INTO hot.api_key(api_key_id, env_id, description, key_data, created_by_user_id, permissions) \
         VALUES ($1, $2, 'other', $3, $4, $5)",
    )
    .bind(other_key_id)
    .bind(fx.data.env_id)
    .bind(serde_json::from_str::<Value>(&other_key_data)?)
    .bind(fx.data.user_id)
    .bind(json!({"*:*": ["*"]}))
    .execute(&fx.admin)
    .await?;
    let (status, body) = post(b.clone(), 4, "ping", other_key, endpoint.clone()).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");

    // Rolling replacement of B: the replacement routes to A's stream.
    drop(b);
    let replacement = fx.instance().await;
    let (status, _) = post(replacement, 5, "ping", fx.api_key.clone(), endpoint.clone()).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let pong = reply(stream.next("message").await);
    assert_eq!(pong["id"], 5);

    // The instance holding the stream still accepts messages for it.
    let (status, _) = post(a.clone(), 6, "ping", fx.api_key.clone(), endpoint.clone()).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(reply(stream.next("message").await)["id"], 6);
    Ok(())
}

/// `/v1/env/subscribe` on A receives events another process (a worker)
/// publishes for the environment.
#[tokio::test]
async fn env_subscription_receives_events_published_elsewhere() -> TestResult {
    with_fixture!(env_subscription_body)
}

async fn env_subscription_body(fx: &Fixture) -> TestResult {
    let a = fx.instance().await;
    let worker = fx.worker_pubsub()?;
    let mut events = SseReader::new(
        send(
            &a,
            request("GET", "/v1/env/subscribe", Some(&fx.api_key), Body::empty()),
        )
        .await,
    );

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
        worker
            .publish_env(EnvEvent::RunStop {
                run_id,
                env_id: fx.data.env_id,
                stream_id: fx.data.stream_id,
                event_id: None,
                project_id: Some(fx.data.project_id),
                fn_name: Some("::app/handler".into()),
                run_type: "event".into(),
                duration_ms: Some(5),
            })
            .await?;
        let started: Value = serde_json::from_str(&events.next("run:start").await)?;
        assert_eq!(started["run_id"], run_id.to_string());
        let stopped: Value = serde_json::from_str(&events.next("run:stop").await)?;
        assert_eq!(stopped["run_id"], run_id.to_string());
        // Let the subscription idle between runs, as real traffic does.
        tokio::time::sleep(Duration::from_millis(1500)).await;
    }
    Ok(())
}

/// `/v1/streams/{id}/subscribe` on A receives stream data another process
/// publishes, after the handler's one-second database poll has raced the
/// Redis read several times.
#[tokio::test]
async fn stream_subscription_receives_data_published_elsewhere() -> TestResult {
    with_fixture!(stream_subscription_body)
}

async fn stream_subscription_body(fx: &Fixture) -> TestResult {
    let a = fx.instance().await;
    let worker = fx.worker_pubsub()?;
    let path = format!("/v1/streams/{}/subscribe", fx.data.stream_id);
    let mut events =
        SseReader::new(send(&a, request("GET", &path, Some(&fx.api_key), Body::empty())).await);

    for n in 0..3 {
        tokio::time::sleep(Duration::from_millis(2500)).await;
        worker
            .publish(StreamEvent::StreamData {
                stream_data_id: Uuid::now_v7(),
                run_id: fx.data.run_id,
                env_id: Some(fx.data.env_id),
                stream_id: fx.data.stream_id,
                data_type: "progress".into(),
                payload: json!({"step": n}),
            })
            .await?;
        let data: Value = serde_json::from_str(&events.next("stream:data").await)?;
        assert_eq!(data["payload"]["step"], n, "{data}");
        assert_eq!(data["run_id"], fx.data.run_id.to_string());
    }
    Ok(())
}

fn multipart_build(boundary: &str, build_id: Uuid, bytes: &[u8]) -> Vec<u8> {
    let mut body = Vec::new();
    for (name, value) in [
        ("hash", "cross-instance-hash".to_string()),
        ("build_id", build_id.to_string()),
    ] {
        body.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
            )
            .as_bytes(),
        );
    }
    body.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"build.hot.zip\"\r\n\
             Content-Type: application/zip\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(bytes);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    body
}

async fn upload_build(app: &Router, fx: &Fixture, build_id: Uuid, bytes: &[u8]) -> Response {
    let boundary = "cross-instance-boundary";
    let path = format!("/v1/projects/{}/builds", fx.data.project_id);
    let mut upload = request(
        "POST",
        &path,
        Some(&fx.api_key),
        Body::from(multipart_build(boundary, build_id, bytes)),
    );
    upload.headers_mut().insert(
        "content-type",
        format!("multipart/form-data; boundary={boundary}")
            .parse()
            .unwrap(),
    );
    send(app, upload).await
}

/// A build uploaded through A is downloadable through B and through a
/// replacement instance, and B recognizes A's upload as already stored.
#[tokio::test]
async fn uploaded_builds_are_served_by_every_instance() -> TestResult {
    with_fixture!(builds_body)
}

async fn builds_body(fx: &Fixture) -> TestResult {
    let a = fx.instance().await;
    let b = fx.instance().await;
    let build_id = Uuid::now_v7();
    let bundle: Vec<u8> = (0..64 * 1024).map(|i| (i % 251) as u8).collect();

    let uploaded = upload_build(&a, fx, build_id, &bundle).await;
    assert_eq!(uploaded.status(), StatusCode::CREATED);

    let download = format!(
        "/v1/projects/{}/builds/{build_id}/download",
        fx.data.project_id
    );
    let (status, bytes) = bytes_call(&b, &download, &fx.api_key).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(bytes, bundle, "B must serve the bytes A stored");

    // An idempotent retry that lands on B finds A's stored bundle.
    let retried = upload_build(&b, fx, build_id, &bundle).await;
    assert_eq!(retried.status(), StatusCode::OK);
    assert_eq!(
        retried
            .headers()
            .get("x-build-exists")
            .map(|v| v.as_bytes()),
        Some(&b"true"[..])
    );

    let (status, build) = json_call(
        &b,
        "GET",
        &format!("/v1/projects/{}/builds/{build_id}", fx.data.project_id),
        &fx.api_key,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{build}");
    assert_eq!(build["data"]["size"], bundle.len());

    drop(a);
    let replacement = fx.instance().await;
    let (status, bytes) = bytes_call(&replacement, &download, &fx.api_key).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(bytes, bundle);
    Ok(())
}

/// Files written through one instance are read, replaced, assembled from
/// multipart parts, and deleted across instances.
#[tokio::test]
async fn files_span_instances() -> TestResult {
    with_fixture!(files_body)
}

async fn files_body(fx: &Fixture) -> TestResult {
    let a = fx.instance().await;
    let b = fx.instance().await;
    let upload = |app: Router, path: &'static str, bytes: Vec<u8>| {
        let token = fx.api_key.clone();
        async move {
            let response = send(
                &app,
                request(
                    "PUT",
                    &format!("/v1/files/upload/{path}"),
                    Some(&token),
                    Body::from(bytes),
                ),
            )
            .await;
            let status = response.status();
            let body: Value =
                serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                    .unwrap_or(Value::Null);
            assert_eq!(status, StatusCode::OK, "{body}");
            body["data"]["file_id"].as_str().unwrap().to_string()
        }
    };

    let file_id = upload(
        a.clone(),
        "reports/summary.txt",
        b"written through A".to_vec(),
    )
    .await;
    let download = format!("/v1/files/{file_id}/download");
    assert_eq!(
        bytes_call(&b, &download, &fx.api_key).await,
        (StatusCode::OK, b"written through A".to_vec())
    );

    // Replace through B; A serves the new bytes at once.
    let replaced_id = upload(
        b.clone(),
        "reports/summary.txt",
        b"replaced through B".to_vec(),
    )
    .await;
    assert_eq!(
        bytes_call(
            &a,
            &format!("/v1/files/{replaced_id}/download"),
            &fx.api_key
        )
        .await,
        (StatusCode::OK, b"replaced through B".to_vec())
    );

    // Multipart: initiate on A, parts on B and A, complete on B.
    let (status, initiated) = json_call(
        &a,
        "POST",
        "/v1/files/uploads",
        &fx.api_key,
        Some(json!({"path": "reports/parts.bin"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{initiated}");
    let upload_id = initiated["data"]["upload_id"].as_str().unwrap().to_string();
    let first = vec![b'a'; 4096];
    let second = vec![b'b'; 1024];
    for (app, number, part) in [(&b, 1, first.clone()), (&a, 2, second.clone())] {
        let response = send(
            app,
            request(
                "PUT",
                &format!("/v1/files/uploads/{upload_id}/{number}"),
                Some(&fx.api_key),
                Body::from(part),
            ),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK, "part {number}");
    }
    let (status, completed) = json_call(
        &b,
        "POST",
        &format!("/v1/files/uploads/{upload_id}/complete"),
        &fx.api_key,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{completed}");
    let assembled_id = completed["data"]["file_id"].as_str().unwrap();
    let (status, bytes) = bytes_call(
        &a,
        &format!("/v1/files/{assembled_id}/download"),
        &fx.api_key,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(bytes, [first, second].concat());

    // Delete through A; B no longer serves it.
    let (status, body) = json_call(
        &a,
        "DELETE",
        &format!("/v1/files/{assembled_id}"),
        &fx.api_key,
        None,
    )
    .await;
    assert!(status.is_success(), "{status} {body}");
    let (status, _) = bytes_call(
        &b,
        &format!("/v1/files/{assembled_id}/download"),
        &fx.api_key,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    Ok(())
}

/// A payload another process spills to blob storage (as a worker does for
/// large results) is rehydrated and downloadable through every API instance.
#[tokio::test]
async fn spilled_blobs_are_read_through_every_instance() -> TestResult {
    with_fixture!(blobs_body)
}

async fn blobs_body(fx: &Fixture) -> TestResult {
    let worker_db = Arc::new(db::create_db_pool(&db::get_resolved_conf(fx.conf())).await?);
    let worker_blobs = hot::blob::blob_store_from_conf(worker_db.clone(), &fx.conf())
        .await
        .ok_or("blob storage should be enabled")?;
    let payload = "spilled ".repeat(1024);
    let spilled = worker_blobs
        .spill_large_val(
            hot::val!({"report": payload.clone()}),
            hot::blob::BlobScope {
                org_id: fx.data.org_id,
                env_id: Some(fx.data.env_id),
                run_id: Some(fx.data.run_id),
            },
            hot::blob::SpillSource::RunResult,
            Some(&fx.data.run_id.to_string()),
        )
        .await?;
    let spilled = serde_json::to_value(&spilled)?;
    let blob_ref_id = spilled["report"]["$val"]["id"]
        .as_str()
        .ok_or("the payload should have spilled")?
        .to_string();
    sqlx::query("UPDATE hot.run SET result = $1 WHERE run_id = $2")
        .bind(&spilled)
        .bind(fx.data.run_id)
        .execute(&fx.admin)
        .await?;
    drop(worker_blobs);
    if let Ok(DatabasePool::Postgres(pool)) = Arc::try_unwrap(worker_db) {
        pool.close().await;
    }

    let a = fx.instance().await;
    let b = fx.instance().await;
    let (status, run) = json_call(
        &a,
        "GET",
        &format!("/v1/runs/{}", fx.data.run_id),
        &fx.api_key,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{run}");
    assert_eq!(run["data"]["result"]["report"], payload);
    for app in [&a, &b] {
        let (status, bytes) = bytes_call(
            app,
            &format!("/v1/blobs/{blob_ref_id}/download"),
            &fx.api_key,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(bytes, payload.as_bytes());
    }
    Ok(())
}

/// API sessions and keys are checked against the database on every request:
/// a session minted on A works on B, and revocation through B (or out of
/// band) takes effect on A immediately.
#[tokio::test]
async fn credentials_are_honored_and_revoked_across_instances() -> TestResult {
    with_fixture!(credentials_body)
}

async fn credentials_body(fx: &Fixture) -> TestResult {
    let a = fx.instance().await;
    let b = fx.instance().await;

    let (status, session) = json_call(
        &a,
        "POST",
        "/v1/sessions",
        &fx.api_key,
        Some(json!({"permissions": {"event:*": ["read"]}})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{session}");
    let token = session["data"]["token"].as_str().unwrap().to_string();
    let session_id = session["data"]["session_id"].as_str().unwrap().to_string();

    // Sessions reach the event, task, and stream routes.
    let (status, events) = json_call(&b, "GET", "/v1/events", &token, None).await;
    assert_eq!(status, StatusCode::OK, "{events}");

    let (status, body) = json_call(
        &b,
        "DELETE",
        &format!("/v1/sessions/{session_id}"),
        &fx.api_key,
        None,
    )
    .await;
    assert!(status.is_success(), "{status} {body}");
    let (status, _) = json_call(&a, "GET", "/v1/events", &token, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // Both instances have just authenticated the API key; revoke it out of
    // band and neither keeps accepting it.
    for app in [&a, &b] {
        assert_eq!(
            json_call(app, "GET", "/v1/env", &fx.api_key, None).await.0,
            StatusCode::OK
        );
    }
    sqlx::query("UPDATE hot.api_key SET active = false WHERE api_key_id = $1")
        .bind(fx.api_key_id)
        .execute(&fx.admin)
        .await?;
    for app in [&a, &b] {
        assert_eq!(
            json_call(app, "GET", "/v1/env", &fx.api_key, None).await.0,
            StatusCode::UNAUTHORIZED
        );
    }
    Ok(())
}

/// Per-instance state that could go stale: project metadata (the entity
/// cache is SQLite-only) and custom-domain routes (the domain cache re-checks
/// every positive entry). Changes are made out of band so neither instance's
/// own write path could refresh it.
#[tokio::test]
async fn instances_do_not_serve_stale_project_or_domain_state() -> TestResult {
    with_fixture!(caches_body)
}

async fn caches_body(fx: &Fixture) -> TestResult {
    let a = fx.instance().await;
    let b = fx.instance().await;
    let project = format!("/v1/projects/{}", fx.data.project_id);

    for app in [&a, &b] {
        let (status, body) = json_call(app, "GET", &project, &fx.api_key, None).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["data"]["name"], "test-project");
    }
    sqlx::query("UPDATE hot.project SET name = 'renamed-elsewhere' WHERE project_id = $1")
        .bind(fx.data.project_id)
        .execute(&fx.admin)
        .await?;
    for app in [&a, &b] {
        let (_, body) = json_call(app, "GET", &project, &fx.api_key, None).await;
        assert_eq!(body["data"]["name"], "renamed-elsewhere");
    }

    // A verified custom domain routes on both instances; once another
    // instance deletes it, neither keeps routing it from its cache.
    let host = format!("mcp-{}.example.test", Uuid::now_v7().simple());
    sqlx::query("INSERT INTO hot.domain(env_id, domain, verified_at) VALUES ($1, $2, now())")
        .bind(fx.data.env_id)
        .bind(&host)
        .execute(&fx.admin)
        .await?;
    let ping = |host: String| {
        let mut request = request(
            "POST",
            "/mcp/tools",
            None,
            Body::from(json_rpc(1, "ping").to_string()),
        );
        request.headers_mut().insert("host", host.parse().unwrap());
        request
            .headers_mut()
            .insert("content-type", "application/json".parse().unwrap());
        request
    };
    for app in [&a, &b] {
        assert_eq!(send(app, ping(host.clone())).await.status(), StatusCode::OK);
    }
    sqlx::query("UPDATE hot.domain SET deleted_at = now() WHERE domain = $1")
        .bind(&host)
        .execute(&fx.admin)
        .await?;
    for app in [&a, &b] {
        assert_eq!(
            send(app, ping(host.clone())).await.status(),
            StatusCode::NOT_FOUND
        );
    }
    Ok(())
}

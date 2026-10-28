//! Full production-router lifecycle rehearsal with an enforced runtime login.
//! The privileged fixture creates only the database, roles and initial account;
//! child provisioning, credentials and every HTTP operation use the runtime role.
use super::*;
use axum::body::{Body, to_bytes};
use hot::db::org_api_key::CreateOrgApiKey;
use hot::db::{self, DatabasePool};
use hot::queue::Queue;
use hot::stream::{StreamPubSub, StreamPubSubType};
use serde_json::{Value, json};
use sqlx::{Executor, PgPool};
use std::{path::Path, sync::Arc};
use tower::ServiceExt;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

struct Fixture {
    cluster: PgPool,
    admin: PgPool,
    runtime: Arc<DatabasePool>,
    database: String,
    role: String,
    user: Uuid,
    deployer: Uuid,
    root: Uuid,
}

impl Fixture {
    async fn new(admin_uri: &str) -> TestResult<Self> {
        let suffix = Uuid::new_v4().simple().to_string();
        let database = format!("hot_lifecycle_{suffix}");
        let role = format!("hot_lifecycle_runtime_{suffix}");
        let password = Uuid::new_v4().simple().to_string();
        let cluster = PgPool::connect(admin_uri).await?;
        cluster
            .execute(sqlx::AssertSqlSafe(format!("CREATE DATABASE {database}")))
            .await?;
        cluster.execute(sqlx::AssertSqlSafe(format!("CREATE ROLE {role} LOGIN PASSWORD '{password}' NOSUPERUSER NOBYPASSRLS NOINHERIT NOCREATEDB NOCREATEROLE NOREPLICATION"))).await?;
        let mut url = url::Url::parse(admin_uri)?;
        url.set_path(&format!("/{database}"));
        db::run_migrations(&hot::val!({"uri":url.to_string(),"migration-uri":url.to_string()}))
            .await?;
        let admin = PgPool::connect(url.as_str()).await?;
        let provisioning_password = url.password().map(str::to_owned);
        url.set_password(None)
            .map_err(|_| "invalid fixture password")?;
        let mut command = tokio::process::Command::new("psql");
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
        let output = command.output().await?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        url.set_username(&role)
            .map_err(|_| "invalid fixture role")?;
        url.set_password(Some(&password))
            .map_err(|_| "invalid fixture password")?;
        let runtime = Arc::new(db::create_db_pool(&hot::val!({"uri":url.to_string()})).await?);
        let user = Uuid::new_v4();
        let deployer = Uuid::new_v4();
        let root = Uuid::new_v4();
        sqlx::query("INSERT INTO hot.\"user\"(user_id,email,created_by_user_id) VALUES($1,$2,$1)")
            .bind(user)
            .bind(format!("{suffix}@lifecycle.example.test"))
            .execute(&admin)
            .await?;
        sqlx::query("INSERT INTO hot.org(org_id,name,slug,org_type,created_by_user_id) VALUES($1,'Lifecycle account',$2,'organization',$3)")
            .bind(root).bind(format!("account-{suffix}")).bind(user).execute(&admin).await?;
        sqlx::query("INSERT INTO hot.org_user(org_user_id,org_id,user_id,org_user_role_id,created_by_user_id) VALUES($1,$2,$3,2,$3)")
            .bind(Uuid::new_v4()).bind(root).bind(user).execute(&admin).await?;
        sqlx::query("INSERT INTO hot.\"user\"(user_id,email,created_by_user_id) VALUES($1,$2,$1)")
            .bind(deployer)
            .bind(format!("deployer-{suffix}@lifecycle.example.test"))
            .execute(&admin)
            .await?;
        sqlx::query("INSERT INTO hot.org_user(org_user_id,org_id,user_id,org_user_role_id,created_by_user_id) VALUES($1,$2,$3,1,$4)")
            .bind(Uuid::new_v4()).bind(root).bind(deployer).bind(user).execute(&admin).await?;
        Ok(Self {
            cluster,
            admin,
            runtime,
            database,
            role,
            user,
            deployer,
            root,
        })
    }

    async fn cleanup(self) -> TestResult {
        if let DatabasePool::Postgres(pool) = self.runtime.as_ref() {
            pool.close().await;
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

async fn request(
    app: &Router,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut builder = axum::http::Request::builder()
        .method(method)
        .uri(path)
        .header("host", "localhost")
        .header("content-type", "application/json");
    if let Some(token) = token {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    let response = app
        .clone()
        .oneshot(
            builder
                .body(body.map_or_else(Body::empty, |body| Body::from(body.to_string())))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()))
    };
    (status, body)
}

async fn expect(
    app: &Router,
    method: &str,
    path: &str,
    token: &str,
    body: Option<Value>,
    expected: StatusCode,
) -> Value {
    let (status, body) = request(app, method, path, Some(token), body).await;
    assert_eq!(status, expected, "{method} {path}: {body}");
    body
}

async fn event_work_snapshot(
    admin: &PgPool,
    queue: &hot::queue::ProcessingQueue<hot::data::msg::Message>,
) -> TestResult<(i64, usize)> {
    // Suspended/archived scopes cannot see their retained events. Observe the
    // isolated fixture through its administrator so hidden writes are detected.
    let persisted = sqlx::query_scalar("SELECT count(*) FROM hot.event")
        .fetch_one(admin)
        .await?;
    let queued = queue.len().await.map_err(|error| error.to_string())?;
    Ok((persisted, queued))
}

struct Child {
    org: Uuid,
    env: Uuid,
    slug: String,
    api_id: Uuid,
    api_token: String,
    service_id: Uuid,
    service_token: String,
    project: Uuid,
}

impl Child {
    fn webhook_tenant(
        &self,
        deployer: Uuid,
    ) -> crate::handlers::webhook::postgres_lifecycle::Tenant<'_> {
        crate::handlers::webhook::postgres_lifecycle::Tenant {
            org_id: self.org,
            env_id: self.env,
            slug: &self.slug,
            env_name: "production",
            api_token: &self.api_token,
            service_token: &self.service_token,
            deployment_creator: deployer,
        }
    }
}

async fn create_child(app: &Router, manager_token: &str, label: &str) -> Child {
    let payload = json!({"name":label,"slug":label,"idempotency_key":label,"external_id":label});
    let child = expect(
        app,
        "POST",
        "/v1/orgs",
        manager_token,
        Some(payload.clone()),
        StatusCode::CREATED,
    )
    .await;
    let repeated = expect(
        app,
        "POST",
        "/v1/orgs",
        manager_token,
        Some(payload),
        StatusCode::CREATED,
    )
    .await;
    assert_eq!(child["data"]["created"], true);
    assert_eq!(repeated["data"]["created"], false);
    assert_eq!(
        child["data"]["org"]["org_id"],
        repeated["data"]["org"]["org_id"]
    );
    assert_eq!(child["data"]["env_id"], repeated["data"]["env_id"]);
    let org = Uuid::parse_str(child["data"]["org"]["org_id"].as_str().unwrap()).unwrap();
    let env_path = format!("/v1/orgs/{org}/envs");
    let env = expect(
        app,
        "POST",
        &env_path,
        manager_token,
        Some(json!({"name":"production"})),
        StatusCode::CREATED,
    )
    .await;
    let repeated = expect(
        app,
        "POST",
        &env_path,
        manager_token,
        Some(json!({"name":"production"})),
        StatusCode::CREATED,
    )
    .await;
    assert_eq!(env["data"]["env_id"], repeated["data"]["env_id"]);
    let env = Uuid::parse_str(env["data"]["env_id"].as_str().unwrap()).unwrap();
    let key = expect(
        app,
        "POST",
        &format!("{env_path}/{env}/api-keys"),
        manager_token,
        Some(json!({"description":"child operator","permissions":{"*:*": ["*"]}})),
        StatusCode::CREATED,
    )
    .await;
    let api_token = key["data"]["token"].as_str().unwrap().to_owned();
    let api_id = Uuid::parse_str(key["data"]["api_key_id"].as_str().unwrap()).unwrap();
    let service = expect(app,"POST","/v1/service-keys",&api_token,Some(json!({"name":"customer service","permissions":{"event:lifecycle.*":["create"],"event:*":["read"],"webhook:*": ["execute"]}})),StatusCode::CREATED).await;
    let service_token = service["data"]["token"].as_str().unwrap().to_owned();
    let service_id = Uuid::parse_str(service["data"]["service_key_id"].as_str().unwrap()).unwrap();
    let project = expect(
        app,
        "POST",
        "/v1/projects",
        &api_token,
        Some(json!({"name":"same-name"})),
        StatusCode::CREATED,
    )
    .await;
    Child {
        org,
        env,
        slug: label.into(),
        api_id,
        api_token,
        service_id,
        service_token,
        project: Uuid::parse_str(project["data"]["project_id"].as_str().unwrap()).unwrap(),
    }
}

#[tokio::test]
async fn restricted_runtime_sub_org_lifecycle_through_production_router() -> TestResult {
    let Ok(admin_uri) = std::env::var("HOT_TEST_POSTGRES_URI") else {
        return Ok(());
    };
    // The memory queue registry is process-global, and other API unit tests
    // consume the same hot:event queue. A separate test process keeps this
    // complete HTTP/queue roundtrip deterministic under workspace parallelism.
    if std::env::var_os("HOT_SUB_ORG_LIFECYCLE_TEST_CHILD").is_none() {
        let output = tokio::process::Command::new(std::env::current_exe()?)
            .args(["--exact","handlers::orgs::postgres_lifecycle::restricted_runtime_sub_org_lifecycle_through_production_router","--nocapture"])
            .env("HOT_SUB_ORG_LIFECYCLE_TEST_CHILD","1")
            .output().await?;
        assert!(
            output.status.success(),
            "lifecycle subprocess failed:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("1 passed; 0 failed"),
            "exact subprocess filter must execute the lifecycle test"
        );
        return Ok(());
    }
    let fixture = Fixture::new(&admin_uri).await?;
    let result: TestResult = async {
        let (manager, manager_token) = OrgApiKey::create_for_user(
            &fixture.runtime,
            fixture.root,
            fixture.user,
            &CreateOrgApiKey {
                description: "Account automation".into(),
                permissions: json!({
                    "org:*": ["create", "read", "update"],
                    "org:*/env:*": ["create", "read"],
                    "org:*/env:*/api-key:*": ["create", "read", "update", "delete"],
                    "org:*/env:*/*:*": ["*"],
                    format!("org:{}/usage:*", fixture.root): ["read"],
                }),
                expires_at: Some(chrono::Utc::now() + chrono::Duration::hours(1)),
            },
        )
        .await?;
        let storage = tempfile::tempdir()?;
        let conf = Arc::new(
            hot::val!({"queue":{"type":"memory"},"webhook":{"timeout":5},"domain":{"mode":"none"}}),
        );
        let pubsub = Arc::new(
            StreamPubSub::new(StreamPubSubType::Memory, None, false).map_err(|e| e.to_string())?,
        );
        let state: ApiStateData = (
            fixture.runtime.clone(),
            Arc::new(Box::new(hot::storage::LocalBuildStorage::new(
                storage.path().to_path_buf(),
            ))),
            conf.clone(),
            Some(pubsub.clone()),
        );
        let app = crate::server::build_router(
            state,
            Arc::new(Box::new(hot::file_storage::LocalFileStorage::new(
                storage.path().join("files"),
            ))),
            None,
            Arc::new(crate::client_ip::ClientIpPolicy::from_conf(&conf)?),
            300 * 1024 * 1024,
            128,
        )
        .layer(Extension(axum::extract::ConnectInfo(
            std::net::SocketAddr::from(([127, 0, 0, 1], 12345)),
        )));
        let a = create_child(&app, &manager_token, "lifecycle-alpha").await;
        let b = create_child(&app, &manager_token, "lifecycle-beta").await;
        assert_ne!(a.env, b.env);
        assert_ne!(a.project, b.project);
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM hot.org_user WHERE org_id=ANY($1)")
                .bind(vec![a.org, b.org])
                .fetch_one(&fixture.admin)
                .await?,
            0,
            "child operations must work without copied root memberships"
        );
        for own in [&a, &b] {
            let env = expect(&app, "GET", "/v1/env", &own.api_token, None, StatusCode::OK).await;
            assert_eq!(env["data"]["org_id"], own.org.to_string());
            assert_eq!(env["data"]["env_id"], own.env.to_string());
            let list = expect(
                &app,
                "GET",
                "/v1/projects",
                &own.api_token,
                None,
                StatusCode::OK,
            )
            .await;
            assert_eq!(list["data"].as_array().unwrap().len(), 1);
            assert_eq!(list["data"][0]["project_id"], own.project.to_string());
            let mismatch = expect(
                &app,
                "GET",
                "/v1/orgs",
                &own.api_token,
                None,
                StatusCode::FORBIDDEN,
            )
            .await;
            assert_eq!(mismatch["error"]["code"], "key_scope_mismatch");
            expect(
                &app,
                "POST",
                "/v1/service-keys",
                &own.service_token,
                Some(json!({"permissions":{"event:*": ["read"]}})),
                StatusCode::FORBIDDEN,
            )
            .await;
        }
        let mismatch = expect(
            &app,
            "GET",
            "/v1/env",
            &manager_token,
            None,
            StatusCode::FORBIDDEN,
        )
        .await;
        assert_eq!(mismatch["error"]["code"], "key_scope_mismatch");
        for (own, other) in [(&a, &b), (&b, &a)] {
            expect(
                &app,
                "GET",
                &format!("/v1/projects/{}", other.project),
                &own.api_token,
                None,
                StatusCode::NOT_FOUND,
            )
            .await;
            expect(
                &app,
                "PATCH",
                &format!("/v1/projects/{}", other.project),
                &own.api_token,
                Some(json!({"name":"stolen"})),
                StatusCode::NOT_FOUND,
            )
            .await;
            expect(
                &app,
                "GET",
                &format!("/v1/service-keys/{}", other.service_id),
                &own.api_token,
                None,
                StatusCode::NOT_FOUND,
            )
            .await;
            expect(
                &app,
                "DELETE",
                &format!("/v1/service-keys/{}", other.service_id),
                &own.api_token,
                None,
                StatusCode::NOT_FOUND,
            )
            .await;
            expect(
                &app,
                "GET",
                &format!("/v1/env?org_id={}&env_id={}", other.org, other.env),
                &own.api_token,
                None,
                StatusCode::FORBIDDEN,
            )
            .await;
            expect(
                &app,
                "POST",
                &format!("/v1/orgs/{}/envs/{}/api-keys", own.org, other.env),
                &manager_token,
                Some(json!({"description":"invalid owner","permissions":{"env:*": ["read"]}})),
                StatusCode::NOT_FOUND,
            )
            .await;
        }
        let event = expect(
            &app,
            "POST",
            "/v1/events",
            &a.service_token,
            Some(json!({"event_type":"lifecycle.created","event_data":{"marker":"alpha"}})),
            StatusCode::CREATED,
        )
        .await;
        let event_id = event["data"]["event_id"].as_str().unwrap();
        // Inspect and consume this API event before the webhook roundtrips use
        // the same process-local queue. No other test shares this subprocess.
        let event_queue = hot::queue::ProcessingQueue::<hot::data::msg::Message>::new(
            hot::queue::QueueType::Memory,
            "hot:event".into(),
            None,
            hot::data::serialization::Serialization::Json,
        )
        .map_err(|e| e.to_string())?;
        let queued = event_queue
            .dequeue()
            .await
            .map_err(|e| e.to_string())?
            .expect("published API event must be queued");
        let queued = hot::lang::event::EventMessage::try_from(queued).map_err(|e| e.to_string())?;
        assert_eq!(queued.id, Uuid::parse_str(event_id)?);
        assert_eq!(queued.body.execution_context.org_id, Some(a.org));
        assert_eq!(queued.body.execution_context.env_id, Some(a.env));
        expect(
            &app,
            "GET",
            &format!("/v1/events/{event_id}"),
            &a.service_token,
            None,
            StatusCode::OK,
        )
        .await;
        expect(
            &app,
            "GET",
            &format!("/v1/events/{event_id}"),
            &b.service_token,
            None,
            StatusCode::NOT_FOUND,
        )
        .await;
        let access_count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM hot.access WHERE env_id=$1 AND service_key_id=$2",
        )
        .bind(a.env)
        .bind(a.service_id)
        .fetch_one(&fixture.admin)
        .await?;
        assert!(
            access_count >= 2,
            "real access logging must preserve the derived credential and child environment"
        );
        let a_db = fixture.runtime.with_scope(DatabaseScope::Environment {
            org_id: a.org,
            env_id: a.env,
        });
        hot::db::api_key::ApiKey::toggle_active(&a_db, &a.api_id, false, &fixture.user).await?;
        for token in [&a.api_token, &a.service_token] {
            expect(
                &app,
                "GET",
                &format!("/v1/events/{event_id}"),
                token,
                None,
                StatusCode::UNAUTHORIZED,
            )
            .await;
        }
        expect(&app, "GET", "/v1/env", &b.api_token, None, StatusCode::OK).await;
        hot::db::api_key::ApiKey::toggle_active(&a_db, &a.api_id, true, &fixture.user).await?;
        expect(
            &app,
            "GET",
            &format!("/v1/events/{event_id}"),
            &a.service_token,
            None,
            StatusCode::OK,
        )
        .await;
        let public_webhook = crate::handlers::webhook::postgres_lifecycle::exercise(
            &app,
            &fixture.runtime,
            &conf,
            &pubsub,
            fixture.user,
            a.webhook_tenant(fixture.deployer),
            b.webhook_tenant(fixture.deployer),
        )
        .await;
        expect(
            &app,
            "DELETE",
            &format!("/v1/service-keys/{}", b.service_id),
            &b.api_token,
            None,
            StatusCode::NO_CONTENT,
        )
        .await;
        expect(
            &app,
            "GET",
            &format!("/v1/events/{event_id}"),
            &b.service_token,
            None,
            StatusCode::UNAUTHORIZED,
        )
        .await;
        expect(
            &app,
            "GET",
            &format!("/v1/events/{event_id}"),
            &a.service_token,
            None,
            StatusCode::OK,
        )
        .await;
        let state_path = format!("/v1/orgs/{}", a.org);
        expect(
            &app,
            "PATCH",
            &state_path,
            &manager_token,
            Some(json!({"state":"suspended"})),
            StatusCode::OK,
        )
        .await;
        let work_before = event_work_snapshot(&fixture.admin, &event_queue).await?;
        let (status, body) = request(&app, "POST", &public_webhook, None, Some(json!({}))).await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "suspended public webhook: {body}"
        );
        assert_eq!(
            event_work_snapshot(&fixture.admin, &event_queue).await?,
            work_before,
            "suspended webhook must not persist or queue an event"
        );
        // Suspension refuses the child's credentials without revoking them, so
        // they are forbidden rather than invalid until it is reactivated.
        for token in [&a.api_token, &a.service_token] {
            expect(
                &app,
                "GET",
                &format!("/v1/events/{event_id}"),
                token,
                None,
                StatusCode::FORBIDDEN,
            )
            .await;
        }
        expect(&app, "GET", "/v1/env", &b.api_token, None, StatusCode::OK).await;
        expect(
            &app,
            "POST",
            &format!("/v1/orgs/{}/envs", a.org),
            &manager_token,
            Some(json!({"name":"blocked"})),
            StatusCode::FORBIDDEN,
        )
        .await;
        expect(
            &app,
            "PATCH",
            &state_path,
            &manager_token,
            Some(json!({"state":"active"})),
            StatusCode::OK,
        )
        .await;
        expect(
            &app,
            "GET",
            &format!("/v1/events/{event_id}"),
            &a.service_token,
            None,
            StatusCode::OK,
        )
        .await;
        expect(
            &app,
            "PATCH",
            &state_path,
            &manager_token,
            Some(json!({"state":"archived"})),
            StatusCode::OK,
        )
        .await;
        let work_before = event_work_snapshot(&fixture.admin, &event_queue).await?;
        let (status, body) = request(&app, "POST", &public_webhook, None, Some(json!({}))).await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "archived public webhook: {body}"
        );
        assert_eq!(
            event_work_snapshot(&fixture.admin, &event_queue).await?,
            work_before,
            "archived webhook must not persist or queue an event"
        );
        expect(
            &app,
            "PATCH",
            &state_path,
            &manager_token,
            Some(json!({"state":"active"})),
            StatusCode::BAD_REQUEST,
        )
        .await;
        expect(
            &app,
            "PATCH",
            &state_path,
            &manager_token,
            Some(json!({"state":"suspended"})),
            StatusCode::OK,
        )
        .await;
        expect(
            &app,
            "PATCH",
            &state_path,
            &manager_token,
            Some(json!({"state":"active"})),
            StatusCode::OK,
        )
        .await;
        expect(
            &app,
            "GET",
            &format!("/v1/events/{event_id}"),
            &a.service_token,
            None,
            StatusCode::OK,
        )
        .await;
        let root_db = fixture.runtime.with_scope(DatabaseScope::Organization {
            org_id: fixture.root,
        });
        let DatabasePool::Postgres(root_pool) = &root_db else {
            unreachable!()
        };
        sqlx::query("UPDATE org_api_key SET permissions=$1 WHERE org_api_key_id=$2")
            .bind(json!({"org:*/env:*/event:*": ["read"],"org:*/env:*/env:*": ["read"]}))
            .bind(manager.org_api_key_id)
            .execute(root_pool)
            .await?;
        expect(
            &app,
            "POST",
            "/v1/events",
            &a.service_token,
            Some(json!({"event_type":"lifecycle.denied","event_data":{}})),
            StatusCode::FORBIDDEN,
        )
        .await;
        expect(
            &app,
            "GET",
            &format!("/v1/events/{event_id}"),
            &a.service_token,
            None,
            StatusCode::OK,
        )
        .await;
        OrgApiKey::revoke_for_user(
            &fixture.runtime,
            fixture.root,
            fixture.user,
            manager.org_api_key_id,
        )
        .await?;
        for token in [
            &a.api_token,
            &a.service_token,
            &b.api_token,
            &b.service_token,
        ] {
            expect(
                &app,
                "GET",
                &format!("/v1/events/{event_id}"),
                token,
                None,
                StatusCode::UNAUTHORIZED,
            )
            .await;
        }
        // The runtime wrapper rejects missing authorization before issuing SQL,
        // including after every lifecycle change has completed.
        if let DatabasePool::Postgres(pool) = fixture.runtime.as_ref() {
            let error = sqlx::query_scalar::<_, i64>("SELECT count(*) FROM event")
                .fetch_one(pool)
                .await
                .unwrap_err();
            assert!(
                matches!(error, sqlx::Error::Protocol(ref message)
                    if message == "Database operation requires an authorized scope"),
                "unexpected unscoped database error: {error}"
            );
        }
        let notes: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM hot.org_note WHERE org_id=$1 AND note_type='org-api.request'",
        )
        .bind(fixture.root)
        .fetch_one(&fixture.admin)
        .await?;
        assert!(notes > 10);
        Ok(())
    }
    .await;
    fixture.cleanup().await?;
    result
}

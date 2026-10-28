use super::*;
use axum::body::to_bytes;
use hot::db::org_api_key::CreateOrgApiKey;
use serde_json::{Value, json};
use std::sync::Arc;

struct App {
    router: Router,
    db: Arc<DatabasePool>,
    data: hot::db::TestData,
    _storage: tempfile::TempDir,
}

impl App {
    async fn new() -> Self {
        let db = Arc::new(hot::db::test_db().await);
        let data = hot::db::insert_test_data(&db).await.unwrap();
        let DatabasePool::Sqlite(pool) = db.as_ref() else {
            unreachable!("SQLite fixture");
        };
        sqlx::query("UPDATE org SET org_type = 'organization' WHERE org_id = ?")
            .bind(data.org_id)
            .execute(pool)
            .await
            .unwrap();
        sqlx::query("PRAGMA foreign_keys = ON")
            .execute(pool)
            .await
            .unwrap();
        let storage = tempfile::tempdir().unwrap();
        let conf = Arc::new(
            hot::val!({"queue":{"type":"memory"},"webhook":{"timeout":5},"domain":{"mode":"none"}}),
        );
        let state: ApiStateData = (
            db.clone(),
            Arc::new(Box::new(hot::storage::LocalBuildStorage::new(
                storage.path().to_path_buf(),
            ))),
            conf.clone(),
            None,
        );
        let router = crate::server::build_router(
            state,
            Arc::new(Box::new(hot::file_storage::LocalFileStorage::new(
                storage.path().join("files"),
            ))),
            None,
            Arc::new(crate::client_ip::ClientIpPolicy::from_conf(&conf).unwrap()),
            300 * 1024 * 1024,
            128,
        )
        .layer(Extension(axum::extract::ConnectInfo(
            std::net::SocketAddr::from(([127, 0, 0, 1], 12345)),
        )));
        Self {
            router,
            db,
            data,
            _storage: storage,
        }
    }

    async fn org_key(&self, permissions: Value) -> (OrgApiKey, String) {
        OrgApiKey::create_for_user(
            &self.db,
            self.data.org_id,
            self.data.user_id,
            &CreateOrgApiKey {
                description: "automation".into(),
                permissions,
                expires_at: None,
            },
        )
        .await
        .unwrap()
    }

    async fn call(
        &self,
        method: &str,
        path: &str,
        token: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let response = self
            .router
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method(method)
                    .uri(path)
                    .header("host", "localhost")
                    .header("authorization", format!("Bearer {token}"))
                    .header("content-type", "application/json")
                    .body(body.map_or_else(Body::empty, |value| Body::from(value.to_string())))
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
        &self,
        method: &str,
        path: &str,
        token: &str,
        body: Option<Value>,
        expected: StatusCode,
    ) -> Value {
        let (status, body) = self.call(method, path, token, body).await;
        assert_eq!(status, expected, "{method} {path}: {body}");
        body
    }

    async fn create_child(&self, token: &str, slug: &str) -> (Uuid, Uuid) {
        let child = self
            .expect(
                "POST",
                "/v1/orgs",
                token,
                Some(json!({"name": slug, "slug": slug, "idempotency_key": slug})),
                StatusCode::CREATED,
            )
            .await;
        (
            Uuid::parse_str(child["data"]["org"]["org_id"].as_str().unwrap()).unwrap(),
            Uuid::parse_str(child["data"]["env_id"].as_str().unwrap()).unwrap(),
        )
    }
}

fn provisioner() -> Value {
    json!({
        "org:*": ["create", "read", "update"],
        "org:*/env:*": ["create", "read"],
        "org:*/env:*/api-key:*": ["create", "read", "update", "delete"],
        "org:*/env:*/project:*": ["create", "read", "update"],
        "org:*/env:*/context:*": ["create", "read", "update", "delete"],
        "org:*/env:*/build:*": ["create", "read", "execute"],
        "org:*/env:*/event:*": ["create"],
        "org:*/env:*/run:*": ["read"],
        "org:*/env:*/env:*": ["read"],
    })
}

#[tokio::test]
async fn org_keys_provision_children_and_mint_environment_keys() {
    let app = App::new().await;
    let (_, token) = app.org_key(provisioner()).await;
    let request = json!({"name": "Acme", "slug": "acme", "external_id": "leela-org-1", "idempotency_key": "leela-org-1"});
    let child = app
        .expect(
            "POST",
            "/v1/orgs",
            &token,
            Some(request.clone()),
            StatusCode::CREATED,
        )
        .await;
    assert_eq!(child["data"]["created"], true);
    assert_eq!(child["data"]["org"]["external_id"], "leela-org-1");
    let repeated = app
        .expect(
            "POST",
            "/v1/orgs",
            &token,
            Some(request),
            StatusCode::CREATED,
        )
        .await;
    assert_eq!(repeated["data"]["created"], false);
    assert_eq!(
        repeated["data"]["org"]["org_id"],
        child["data"]["org"]["org_id"]
    );
    let org = child["data"]["org"]["org_id"].as_str().unwrap().to_owned();
    let listed = app
        .expect("GET", "/v1/orgs", &token, None, StatusCode::OK)
        .await;
    assert_eq!(listed["data"].as_array().unwrap().len(), 1);
    assert_eq!(listed["data"][0]["org_id"], org);
    app.expect(
        "GET",
        &format!("/v1/orgs/{org}"),
        &token,
        None,
        StatusCode::OK,
    )
    .await;

    let env = app
        .expect(
            "POST",
            &format!("/v1/orgs/{org}/envs"),
            &token,
            Some(json!({"name": "program"})),
            StatusCode::CREATED,
        )
        .await;
    let env_id = env["data"]["env_id"].as_str().unwrap().to_owned();
    let again = app
        .expect(
            "POST",
            &format!("/v1/orgs/{org}/envs"),
            &token,
            Some(json!({"name": "program"})),
            StatusCode::CREATED,
        )
        .await;
    assert_eq!(again["data"]["env_id"], env_id);
    let envs = app
        .expect(
            "GET",
            &format!("/v1/orgs/{org}/envs"),
            &token,
            None,
            StatusCode::OK,
        )
        .await;
    assert_eq!(
        envs["data"].as_array().unwrap().len(),
        2,
        "default and program"
    );

    let keys_path = format!("/v1/orgs/{org}/envs/{env_id}/api-keys");
    // Wider than the org key's grants for that environment.
    let (status, body) = app
        .call(
            "POST",
            &keys_path,
            &token,
            Some(json!({"description": "too wide", "permissions": {"*:*": ["*"]}})),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let minted = app
        .expect(
            "POST",
            &keys_path,
            &token,
            Some(json!({"description": "events", "permissions": {"event:*": ["create"], "run:*": ["read"], "env:*": ["read"]}})),
            StatusCode::CREATED,
        )
        .await;
    assert_eq!(minted["data"]["scope"], "env");
    let env_token = minted["data"]["token"].as_str().unwrap().to_owned();
    let env_key_id = minted["data"]["api_key_id"].as_str().unwrap().to_owned();
    let listed = app
        .expect("GET", &keys_path, &token, None, StatusCode::OK)
        .await;
    assert_eq!(listed["data"][0]["api_key_id"], env_key_id);
    assert!(listed["data"][0].get("token").is_none());

    // The minted key works at runtime, in its own environment only.
    let info = app
        .expect("GET", "/v1/env", &env_token, None, StatusCode::OK)
        .await;
    assert_eq!(info["data"]["env_id"], env_id);
    assert_eq!(info["data"]["org_id"], org);
    // Each kind of key is told which kind the route needs.
    let mismatch = app
        .expect("GET", "/v1/orgs", &env_token, None, StatusCode::FORBIDDEN)
        .await;
    assert_eq!(mismatch["error"]["code"], "key_scope_mismatch");
    let mismatch = app
        .expect("GET", "/v1/env", &token, None, StatusCode::FORBIDDEN)
        .await;
    assert_eq!(mismatch["error"]["code"], "key_scope_mismatch");
    app.expect(
        "POST",
        "/v1/service-keys",
        &token,
        Some(json!({"permissions": {"event:*": ["create"]}})),
        StatusCode::FORBIDDEN,
    )
    .await;

    // A full-access environment key never gains org authority.
    let legacy_id = Uuid::now_v7();
    let (legacy_token, legacy_hash) = ApiKey::generate_api_key(&legacy_id).unwrap();
    ApiKey::insert_api_key(
        &app.db,
        &legacy_id,
        &app.data.env_id,
        "full access",
        &serde_json::from_str(&legacy_hash).unwrap(),
        &app.data.user_id,
        &json!({"*:*": ["*"]}),
    )
    .await
    .unwrap();
    let mismatch = app
        .expect(
            "POST",
            "/v1/orgs",
            &legacy_token,
            Some(json!({"name":"x","slug":"x","idempotency_key":"x"})),
            StatusCode::FORBIDDEN,
        )
        .await;
    assert_eq!(mismatch["error"]["code"], "key_scope_mismatch");
}

#[tokio::test]
async fn org_wildcards_never_reach_siblings_the_root_or_mint_authority() {
    let app = App::new().await;
    let (_, token) = app.org_key(provisioner()).await;
    let (a, a_env) = app.create_child(&token, "alpha").await;
    let (b, b_env) = app.create_child(&token, "beta").await;
    // A path naming one child and another child's environment is not found.
    app.expect(
        "POST",
        &format!("/v1/orgs/{a}/envs/{b_env}/api-keys"),
        &token,
        Some(json!({"description": "mismatch", "permissions": {"event:*": ["create"]}})),
        StatusCode::NOT_FOUND,
    )
    .await;
    app.expect(
        "GET",
        &format!("/v1/orgs/{a}/envs/{b_env}/projects"),
        &token,
        None,
        StatusCode::NOT_FOUND,
    )
    .await;
    // org:* never covers the key's own org: no grant names the root.
    let root = app.data.org_id;
    let root_env = app.data.env_id;
    app.expect(
        "GET",
        &format!("/v1/orgs/{root}"),
        &token,
        None,
        StatusCode::FORBIDDEN,
    )
    .await;
    app.expect(
        "GET",
        &format!("/v1/orgs/{root}/envs/{root_env}/projects"),
        &token,
        None,
        StatusCode::FORBIDDEN,
    )
    .await;
    app.expect(
        "GET",
        &format!("/v1/orgs/{root}/api-keys"),
        &token,
        None,
        StatusCode::FORBIDDEN,
    )
    .await;
    // Another root's IDs are not found, whatever the grants.
    app.expect(
        "GET",
        &format!("/v1/orgs/{}", Uuid::now_v7()),
        &token,
        None,
        StatusCode::NOT_FOUND,
    )
    .await;

    // A key that may create children but holds no api-key grant cannot mint keys.
    let (_, creator) = app.org_key(json!({"org:*": ["create", "read"]})).await;
    app.expect(
        "POST",
        &format!("/v1/orgs/{a}/envs/{a_env}/api-keys"),
        &creator,
        Some(json!({"description": "no grant", "permissions": {"event:*": ["create"]}})),
        StatusCode::FORBIDDEN,
    )
    .await;
    app.expect(
        "POST",
        &format!("/v1/orgs/{a}/envs"),
        &creator,
        Some(json!({"name": "nope"})),
        StatusCode::FORBIDDEN,
    )
    .await;

    // A key limited to one child sees only that child.
    let (_, one) = app
        .org_key(json!({format!("org:{a}"): ["read"], format!("org:{a}/env:*"): ["read"]}))
        .await;
    let listed = app
        .expect("GET", "/v1/orgs", &one, None, StatusCode::OK)
        .await;
    assert_eq!(listed["data"].as_array().unwrap().len(), 1);
    assert_eq!(listed["data"][0]["org_id"], a.to_string());
    app.expect(
        "GET",
        &format!("/v1/orgs/{b}"),
        &one,
        None,
        StatusCode::FORBIDDEN,
    )
    .await;
    app.expect(
        "GET",
        &format!("/v1/orgs/{b}/envs"),
        &one,
        None,
        StatusCode::FORBIDDEN,
    )
    .await;
    app.expect(
        "GET",
        &format!("/v1/orgs/{a}/envs"),
        &one,
        None,
        StatusCode::OK,
    )
    .await;
}

#[tokio::test]
async fn org_keys_administer_child_projects_through_environment_handlers() {
    // Same fixed test key the context integration tests use.
    unsafe {
        std::env::set_var(
            "HOT_ENCRYPTION_KEY",
            "0000000000000000000000000000000000000000000000000000000000000000",
        );
    }
    let app = App::new().await;
    let (_, token) = app.org_key(provisioner()).await;
    let (org, env) = app.create_child(&token, "acme").await;
    let base = format!("/v1/orgs/{org}/envs/{env}");
    let project = app
        .expect(
            "POST",
            &format!("{base}/projects"),
            &token,
            Some(json!({"name": "program"})),
            StatusCode::CREATED,
        )
        .await;
    assert_eq!(project["data"]["env_id"], env.to_string());
    let listed = app
        .expect(
            "GET",
            &format!("{base}/projects"),
            &token,
            None,
            StatusCode::OK,
        )
        .await;
    assert_eq!(listed["data"].as_array().unwrap().len(), 1);
    app.expect(
        "POST",
        &format!("{base}/projects/program/context"),
        &token,
        Some(json!({"key": "leela.callback.url", "value": "https://leela.example.test/hot"})),
        StatusCode::CREATED,
    )
    .await;
    let context = app
        .expect(
            "GET",
            &format!("{base}/projects/program/context"),
            &token,
            None,
            StatusCode::OK,
        )
        .await;
    assert_eq!(context["data"][0]["key"], "leela.callback.url");
    assert!(
        context["data"][0].get("value").is_none(),
        "context values are never returned"
    );
    app.expect(
        "GET",
        &format!("{base}/builds"),
        &token,
        None,
        StatusCode::OK,
    )
    .await;
    // Project deletion was not granted; the environment handler's policy applies.
    app.expect(
        "DELETE",
        &format!("{base}/projects/program"),
        &token,
        None,
        StatusCode::FORBIDDEN,
    )
    .await;
    // Only project, context and build routes are reachable this way.
    app.expect(
        "GET",
        &format!("{base}/projects/../service-keys"),
        &token,
        None,
        StatusCode::NOT_FOUND,
    )
    .await;
    let (status, _) = app
        .call("GET", &format!("{base}/service-keys"), &token, None)
        .await;
    assert!(
        matches!(
            status,
            StatusCode::NOT_FOUND | StatusCode::METHOD_NOT_ALLOWED
        ),
        "{status}"
    );

    // The project is in the child's environment, not the root's.
    let root_db = app.db.with_scope(DatabaseScope::Environment {
        org_id: app.data.org_id,
        env_id: app.data.env_id,
    });
    assert!(
        hot::db::Project::get_project_by_env_and_name(&root_db, &app.data.env_id, "program")
            .await
            .unwrap()
            .is_none()
    );
    // A key whose grants name another child cannot reach this one.
    let (_, other) = app
        .org_key(json!({format!("org:{}/env:*/project:*", Uuid::now_v7()): ["read"]}))
        .await;
    app.expect(
        "GET",
        &format!("{base}/projects"),
        &other,
        None,
        StatusCode::FORBIDDEN,
    )
    .await;
}

#[tokio::test]
async fn rotation_narrowing_and_revocation_follow_lineage() {
    let app = App::new().await;
    let (top, token) = app.org_key(provisioner()).await;
    let (org, env) = app.create_child(&token, "acme").await;
    let minted = app
        .expect(
            "POST",
            &format!("/v1/orgs/{org}/envs/{env}/api-keys"),
            &token,
            Some(json!({"description": "events", "permissions": {"event:*": ["create"], "env:*": ["read"]}})),
            StatusCode::CREATED,
        )
        .await;
    let key_id = minted["data"]["api_key_id"].as_str().unwrap().to_owned();
    let first = minted["data"]["token"].as_str().unwrap().to_owned();
    app.expect(
        "GET",
        &format!("/v1/api-keys/{key_id}"),
        &token,
        None,
        StatusCode::OK,
    )
    .await;

    // A peer org key with the same grants did not issue it, so it can read the
    // key but cannot rotate (and so receive) its secret, narrow or revoke it.
    let (_, peer) = app.org_key(provisioner()).await;
    app.expect(
        "GET",
        &format!("/v1/api-keys/{key_id}"),
        &peer,
        None,
        StatusCode::OK,
    )
    .await;
    app.expect(
        "POST",
        &format!("/v1/api-keys/{key_id}/rotate"),
        &peer,
        None,
        StatusCode::FORBIDDEN,
    )
    .await;
    app.expect(
        "DELETE",
        &format!("/v1/api-keys/{key_id}"),
        &peer,
        None,
        StatusCode::FORBIDDEN,
    )
    .await;
    // No key rotates itself.
    app.expect(
        "POST",
        &format!("/v1/api-keys/{}/rotate", top.org_api_key_id),
        &token,
        None,
        StatusCode::FORBIDDEN,
    )
    .await;

    let rotated = app
        .expect(
            "POST",
            &format!("/v1/api-keys/{key_id}/rotate"),
            &token,
            Some(json!({"overlap_seconds": 3600})),
            StatusCode::OK,
        )
        .await;
    assert_eq!(
        rotated["data"]["api_key_id"], key_id,
        "rotation keeps the key ID"
    );
    let second = rotated["data"]["token"].as_str().unwrap().to_owned();
    assert_ne!(first, second);
    app.expect("GET", "/v1/env", &first, None, StatusCode::OK)
        .await;
    app.expect("GET", "/v1/env", &second, None, StatusCode::OK)
        .await;
    let (status, _) = app
        .call(
            "POST",
            &format!("/v1/api-keys/{key_id}/rotate"),
            &token,
            Some(json!({"overlap_seconds": 0})),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    app.expect("GET", "/v1/env", &second, None, StatusCode::UNAUTHORIZED)
        .await;
    app.expect("GET", "/v1/env", &first, None, StatusCode::UNAUTHORIZED)
        .await;
    let (status, _) = app
        .call(
            "POST",
            &format!("/v1/api-keys/{key_id}/rotate"),
            &token,
            Some(json!({"overlap_seconds": 999999})),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let current = app
        .expect(
            "POST",
            &format!("/v1/api-keys/{key_id}/rotate"),
            &token,
            None,
            StatusCode::OK,
        )
        .await["data"]["token"]
        .as_str()
        .unwrap()
        .to_owned();
    app.expect("GET", "/v1/env", &current, None, StatusCode::OK)
        .await;

    // Narrowing within the issuer's grants; widening past them is rejected.
    app.expect(
        "PATCH",
        &format!("/v1/api-keys/{key_id}"),
        &token,
        Some(json!({"permissions": {"project:*": ["delete"]}})),
        StatusCode::BAD_REQUEST,
    )
    .await;
    app.expect(
        "PATCH",
        &format!("/v1/api-keys/{key_id}"),
        &token,
        Some(json!({"permissions": {"event:*": ["create"]}})),
        StatusCode::OK,
    )
    .await;
    app.expect("GET", "/v1/env", &current, None, StatusCode::FORBIDDEN)
        .await;

    // Revoking the issuing org key stops the key it minted.
    OrgApiKey::revoke_for_user(
        &app.db,
        app.data.org_id,
        app.data.user_id,
        top.org_api_key_id,
    )
    .await
    .unwrap();
    app.expect("GET", "/v1/orgs", &token, None, StatusCode::UNAUTHORIZED)
        .await;
    let (status, _) = app
        .call(
            "POST",
            "/v1/events",
            &current,
            Some(json!({"event_type": "x", "event_data": {}})),
        )
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn org_keys_mint_narrower_org_keys_and_suspension_blocks_children() {
    let app = App::new().await;
    let root = app.data.org_id;
    let mut grants = provisioner();
    grants[format!("org:{root}/api-key:*")] = json!(["create", "read", "update", "delete"]);
    let (top, token) = app.org_key(grants).await;
    let (org, _) = app.create_child(&token, "acme").await;
    let path = format!("/v1/orgs/{root}/api-keys");
    app.expect(
        "POST",
        &path,
        &token,
        Some(json!({"description": "wider", "permissions": {format!("org:{root}/env:*/project:*"): ["read"]}})),
        StatusCode::BAD_REQUEST,
    )
    .await;
    let reader = app
        .expect(
            "POST",
            &path,
            &token,
            Some(json!({"description": "reader", "permissions": {"org:*": ["read", "update"]}})),
            StatusCode::CREATED,
        )
        .await;
    assert_eq!(reader["data"]["scope"], "org");
    assert_eq!(
        reader["data"]["issuer_api_key_id"],
        top.org_api_key_id.to_string()
    );
    let reader_token = reader["data"]["token"].as_str().unwrap().to_owned();
    app.expect(
        "GET",
        &format!("/v1/orgs/{org}"),
        &reader_token,
        None,
        StatusCode::OK,
    )
    .await;
    app.expect(
        "POST",
        "/v1/orgs",
        &reader_token,
        Some(json!({"name":"x","slug":"x-org","idempotency_key":"x"})),
        StatusCode::FORBIDDEN,
    )
    .await;
    let keys = app.expect("GET", &path, &token, None, StatusCode::OK).await;
    assert_eq!(keys["data"].as_array().unwrap().len(), 2);

    // Suspension blocks work in the child, but its state can still change.
    app.expect(
        "PATCH",
        &format!("/v1/orgs/{org}"),
        &reader_token,
        Some(json!({"state": "suspended"})),
        StatusCode::OK,
    )
    .await;
    app.expect(
        "POST",
        &format!("/v1/orgs/{org}/envs"),
        &token,
        Some(json!({"name": "blocked"})),
        StatusCode::FORBIDDEN,
    )
    .await;
    let resumed = app
        .expect(
            "PATCH",
            &format!("/v1/orgs/{org}"),
            &token,
            Some(json!({"state": "active"})),
            StatusCode::OK,
        )
        .await;
    assert_eq!(resumed["data"]["lifecycle_state"], "active");
    // A key cannot change its own org's state.
    app.expect(
        "PATCH",
        &format!("/v1/orgs/{root}"),
        &token,
        Some(json!({"state": "suspended"})),
        StatusCode::FORBIDDEN,
    )
    .await;

    // Revoking the issuer revokes the minted org key.
    let reader_id = reader["data"]["api_key_id"].as_str().unwrap();
    app.expect(
        "DELETE",
        &format!("/v1/api-keys/{reader_id}"),
        &reader_token,
        None,
        StatusCode::FORBIDDEN,
    )
    .await;
    app.expect(
        "DELETE",
        &format!("/v1/api-keys/{}", top.org_api_key_id),
        &token,
        None,
        StatusCode::FORBIDDEN,
    )
    .await;
    OrgApiKey::revoke_for_user(&app.db, root, app.data.user_id, top.org_api_key_id)
        .await
        .unwrap();
    app.expect(
        "GET",
        &format!("/v1/orgs/{org}"),
        &reader_token,
        None,
        StatusCode::UNAUTHORIZED,
    )
    .await;
}

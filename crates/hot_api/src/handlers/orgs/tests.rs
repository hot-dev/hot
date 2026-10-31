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
            hot::val!({"queue":{"type":"memory"},"webhook":{"timeout":5},"domain":{"mode":"none"},"org":{"reserved-slugs":["acme-internal"]}}),
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
        // Paths carry key IDs, so failures name only the method and response.
        assert_eq!(status, expected, "{method}: {body}");
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
    let request = json!({"name": "Globex", "slug": "globex", "external_id": "leela-org-1", "idempotency_key": "leela-org-1"});
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
        .expect("POST", "/v1/orgs", &token, Some(request), StatusCode::OK)
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
            StatusCode::OK,
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
    assert_eq!(minted["data"]["status"], "active");
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
async fn taken_slugs_and_external_ids_are_conflicts() {
    let app = App::new().await;
    let (_, token) = app.org_key(json!({"org:*": ["create", "read"]})).await;
    app.expect(
        "POST",
        "/v1/orgs",
        &token,
        Some(json!({"name": "Globex", "slug": "globex", "external_id": "crm-1", "idempotency_key": "one"})),
        StatusCode::CREATED,
    )
    .await;
    // Slugs are global: another account's handle is taken too.
    Org::insert_org(
        &app.db,
        &Uuid::now_v7(),
        "Elsewhere",
        "elsewhere",
        "organization",
        &app.data.user_id,
    )
    .await
    .unwrap();
    for slug in ["globex", "elsewhere"] {
        let body = app
            .expect(
                "POST",
                "/v1/orgs",
                &token,
                Some(json!({"name": "Again", "slug": slug, "idempotency_key": format!("again-{slug}")})),
                StatusCode::CONFLICT,
            )
            .await;
        assert_eq!(body["error"]["code"], "slug_taken");
    }
    let body = app
        .expect(
            "POST",
            "/v1/orgs",
            &token,
            Some(json!({"name": "Other", "slug": "globex-two", "external_id": "crm-1", "idempotency_key": "two"})),
            StatusCode::CONFLICT,
        )
        .await;
    assert_eq!(body["error"]["code"], "external_id_taken");
    let listed = app
        .expect("GET", "/v1/orgs", &token, None, StatusCode::OK)
        .await;
    assert_eq!(listed["pagination"]["total"], 1);
}

#[tokio::test]
async fn removed_management_api_and_credentials_point_at_org_keys() {
    let app = App::new().await;
    for (method, path) in [
        ("GET", "/v1/management/children"),
        ("POST", "/v1/management/children"),
        ("PATCH", "/v1/management/children/x/state"),
        ("GET", "/v1/management/usage"),
        ("GET", "/v1/management"),
    ] {
        let body = app.expect(method, path, "", None, StatusCode::GONE).await;
        assert_eq!(body["error"]["code"], "gone", "{method} {path}");
        let message = body["error"]["message"].as_str().unwrap();
        assert!(message.contains("/v1/orgs") && message.contains("/docs/api/management"));
    }
    let hm = "hm_01a18e9c227475d48b0dbc3f0a4e9bf2_d563cc5447f473760beb7354b4d9bb3c";
    for path in ["/v1/orgs", "/v1/projects", "/v1/env"] {
        let body = app
            .expect("GET", path, hm, None, StatusCode::UNAUTHORIZED)
            .await;
        assert_eq!(body["error"]["code"], "management_credential_retired");
    }
}

#[tokio::test]
async fn common_errors_use_the_json_envelope() {
    let app = App::new().await;
    let (_, token) = app.org_key(json!({"org:*": ["create", "read"]})).await;
    for (method, path, bearer, status, code) in [
        (
            "GET",
            "/v1/projects",
            "",
            StatusCode::UNAUTHORIZED,
            "unauthorized",
        ),
        (
            "GET",
            "/v1/orgs",
            "",
            StatusCode::UNAUTHORIZED,
            "unauthorized",
        ),
        (
            "GET",
            "/v1/projects",
            "hot_00000000000000000000000000000000_00",
            StatusCode::UNAUTHORIZED,
            "unauthorized",
        ),
        (
            "GET",
            "/v1/no-such-route",
            "",
            StatusCode::NOT_FOUND,
            "not_found",
        ),
        (
            "GET",
            "/v1/orgs/not-a-uuid",
            token.as_str(),
            StatusCode::BAD_REQUEST,
            "bad_request",
        ),
        (
            "PUT",
            "/v1/orgs",
            token.as_str(),
            StatusCode::METHOD_NOT_ALLOWED,
            "method_not_allowed",
        ),
    ] {
        let (actual, body) = app.call(method, path, bearer, None).await;
        assert_eq!(actual, status, "{method} {path}: {body}");
        assert_eq!(body["error"]["code"], code, "{method} {path}: {body}");
        assert!(body["error"]["request_id"].is_string(), "{body}");
    }
    // Extractor rejections: malformed JSON and a missing content type.
    let response = app
        .router
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/v1/orgs")
                .header("host", "localhost")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from("{not json"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        response.headers()["content-type"],
        "application/json",
        "malformed JSON"
    );
    let body: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 1 << 20).await.unwrap()).unwrap();
    assert_eq!(body["error"]["code"], "bad_request");
    assert!(body["error"]["message"].as_str().unwrap().contains("JSON"));
    let response = app
        .router
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/v1/orgs")
                .header("host", "localhost")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    let body: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 1 << 20).await.unwrap()).unwrap();
    assert_eq!(body["error"]["code"], "unsupported_media_type");
}

#[tokio::test]
async fn org_lists_page_with_limit_and_offset() {
    let app = App::new().await;
    let root = app.data.org_id;
    let env = app.data.env_id;
    let mut grants = provisioner();
    grants[format!("org:{root}/env:*")] = json!(["create", "read"]);
    grants[format!("org:{root}/env:*/api-key:*")] = json!(["create", "read"]);
    grants[format!("org:{root}/env:*/event:*")] = json!(["create"]);
    grants[format!("org:{root}/api-key:*")] = json!(["create", "read"]);
    let (_, token) = app.org_key(grants).await;

    // More than ten environment keys: every one is listed and counted.
    let keys_path = format!("/v1/orgs/{root}/envs/{env}/api-keys");
    for n in 0..12 {
        app.expect(
            "POST",
            &keys_path,
            &token,
            Some(
                json!({"description": format!("key {n}"), "permissions": {"event:*": ["create"]}}),
            ),
            StatusCode::CREATED,
        )
        .await;
    }
    let all = app
        .expect(
            "GET",
            &format!("{keys_path}?limit=50"),
            &token,
            None,
            StatusCode::OK,
        )
        .await;
    let total = all["pagination"]["total"].as_i64().unwrap();
    assert!(total >= 12, "{total}");
    assert_eq!(all["data"].as_array().unwrap().len() as i64, total);
    let last = app
        .expect(
            "GET",
            &format!("{keys_path}?limit=5&offset={}", total - 2),
            &token,
            None,
            StatusCode::OK,
        )
        .await;
    assert_eq!(last["data"].as_array().unwrap().len(), 2);
    assert_eq!(last["pagination"]["has_more"], false);
    assert_eq!(
        last["data"][1]["api_key_id"],
        all["data"][total as usize - 1]["api_key_id"]
    );

    for name in ["one", "two"] {
        app.expect(
            "POST",
            &format!("/v1/orgs/{root}/envs"),
            &token,
            Some(json!({"name": name})),
            StatusCode::CREATED,
        )
        .await;
    }
    let envs = app
        .expect(
            "GET",
            &format!("/v1/orgs/{root}/envs?limit=1&offset=1"),
            &token,
            None,
            StatusCode::OK,
        )
        .await;
    assert_eq!(envs["data"].as_array().unwrap().len(), 1);
    assert_eq!(envs["pagination"]["limit"], 1);
    assert_eq!(envs["pagination"]["offset"], 1);
    assert_eq!(envs["pagination"]["total"], 3);
    assert_eq!(envs["pagination"]["has_more"], true);

    app.expect(
        "POST",
        &format!("/v1/orgs/{root}/api-keys"),
        &token,
        Some(json!({"description": "second", "permissions": {format!("org:{root}/env:*"): ["read"]}})),
        StatusCode::CREATED,
    )
    .await;
    let keys = app
        .expect(
            "GET",
            &format!("/v1/orgs/{root}/api-keys?limit=1&offset=1"),
            &token,
            None,
            StatusCode::OK,
        )
        .await;
    assert_eq!(keys["data"].as_array().unwrap().len(), 1);
    assert_eq!(keys["pagination"]["total"], 2);
    assert_eq!(keys["pagination"]["offset"], 1);
    assert_eq!(keys["pagination"]["has_more"], false);
}

#[tokio::test]
async fn key_status_follows_the_issuer_chain_and_suspension_is_403() {
    let app = App::new().await;
    let root = app.data.org_id;
    let env = app.data.env_id;
    let grants = json!({
        format!("org:{root}/env:*/api-key:*"): ["create", "read"],
        format!("org:{root}/env:*/env:*"): ["read"],
        "org:*": ["read"],
    });
    let (_, reader) = app.org_key(grants.clone()).await;
    let (issuer, issuer_token) = app.org_key(grants).await;
    let minted = app
        .expect(
            "POST",
            &format!("/v1/orgs/{root}/envs/{env}/api-keys"),
            &issuer_token,
            Some(json!({"description": "minted", "permissions": {"env:*": ["read"]}})),
            StatusCode::CREATED,
        )
        .await;
    let minted_id = minted["data"]["api_key_id"].as_str().unwrap().to_owned();
    let minted_token = minted["data"]["token"].as_str().unwrap().to_owned();

    // While the account is suspended, org keys and minted keys get 403, like
    // app-created keys: they work again on reactivation.
    let DatabasePool::Sqlite(pool) = app.db.as_ref() else {
        unreachable!("SQLite fixture");
    };
    sqlx::query("UPDATE org SET lifecycle_state = 'suspended', active = 0 WHERE org_id = ?")
        .bind(root)
        .execute(pool)
        .await
        .unwrap();
    let body = app
        .expect("GET", "/v1/orgs", &reader, None, StatusCode::FORBIDDEN)
        .await;
    assert_eq!(body["error"]["code"], "account_suspended");
    let body = app
        .expect("GET", "/v1/env", &minted_token, None, StatusCode::FORBIDDEN)
        .await;
    assert_eq!(body["error"]["code"], "forbidden");
    sqlx::query("UPDATE org SET lifecycle_state = 'active', active = 1 WHERE org_id = ?")
        .bind(root)
        .execute(pool)
        .await
        .unwrap();
    app.expect("GET", "/v1/env", &minted_token, None, StatusCode::OK)
        .await;

    // Revoking the issuer ends the minted key, and responses say so.
    OrgApiKey::revoke_for_user(&app.db, root, app.data.user_id, issuer.org_api_key_id)
        .await
        .unwrap();
    app.expect(
        "GET",
        "/v1/env",
        &minted_token,
        None,
        StatusCode::UNAUTHORIZED,
    )
    .await;
    let key = app
        .expect(
            "GET",
            &format!("/v1/api-keys/{minted_id}"),
            &reader,
            None,
            StatusCode::OK,
        )
        .await;
    assert_eq!(key["data"]["active"], false);
    assert_eq!(key["data"]["status"], "issuer_inactive");
    let listed = app
        .expect(
            "GET",
            &format!("/v1/orgs/{root}/envs/{env}/api-keys?limit=100"),
            &reader,
            None,
            StatusCode::OK,
        )
        .await;
    let entry = listed["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|key| key["api_key_id"] == minted_id.as_str())
        .unwrap();
    assert_eq!(entry["status"], "issuer_inactive");
}

#[tokio::test]
async fn creating_orgs_rejects_reserved_slugs() {
    let app = App::new().await;
    let (_, token) = app.org_key(json!({"org:*": ["create"]})).await;
    for slug in ["api", "acme-internal"] {
        app.expect(
            "POST",
            "/v1/orgs",
            &token,
            Some(json!({"name": "Reserved", "slug": slug, "idempotency_key": slug})),
            StatusCode::BAD_REQUEST,
        )
        .await;
    }
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
    let (org, env) = app.create_child(&token, "globex").await;
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
    let (org, env) = app.create_child(&token, "globex").await;
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
    let (org, _) = app.create_child(&token, "globex").await;
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
    let blocked = app
        .expect(
            "POST",
            &format!("/v1/orgs/{org}/envs"),
            &token,
            Some(json!({"name": "blocked"})),
            StatusCode::FORBIDDEN,
        )
        .await;
    assert_eq!(blocked["error"]["code"], "org_suspended");
    // Reads stay available, so a suspended child's keys can be found and revoked.
    let envs = app
        .expect(
            "GET",
            &format!("/v1/orgs/{org}/envs"),
            &token,
            None,
            StatusCode::OK,
        )
        .await;
    let env = envs["data"][0]["env_id"].as_str().unwrap().to_owned();
    app.expect(
        "GET",
        &format!("/v1/orgs/{org}/envs/{env}/api-keys"),
        &token,
        None,
        StatusCode::OK,
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

impl App {
    async fn env_key(&self, env_id: Uuid, permissions: Value) -> String {
        let key_id = Uuid::now_v7();
        let (token, hash) = ApiKey::generate_api_key(&key_id).unwrap();
        ApiKey::insert_api_key(
            &self.db,
            &key_id,
            &env_id,
            "calls",
            &serde_json::from_str(&hash).unwrap(),
            &self.data.user_id,
            &permissions,
        )
        .await
        .unwrap();
        token
    }

    async fn run_in(&self, env_id: Uuid, stream_id: Uuid) -> Uuid {
        self.insert_run(env_id, stream_id, hot::db::run::RunType::Run, None)
            .await
    }

    /// A task runs as its own run, in the stream of the run that started it.
    async fn task_of(&self, env_id: Uuid, stream_id: Uuid, origin_run_id: Uuid) -> Uuid {
        self.insert_run(
            env_id,
            stream_id,
            hot::db::run::RunType::Task,
            Some(origin_run_id),
        )
        .await
    }

    async fn insert_run(
        &self,
        env_id: Uuid,
        stream_id: Uuid,
        run_type: hot::db::run::RunType,
        origin_run_id: Option<Uuid>,
    ) -> Uuid {
        let run_id = Uuid::now_v7();
        hot::db::Stream::create_or_get_stream(&self.db, stream_id, env_id)
            .await
            .unwrap();
        hot::db::run::Run::insert_run(
            &self.db,
            &run_id,
            &env_id,
            &stream_id,
            Some(&self.data.build_id),
            run_type.as_id(),
            origin_run_id.as_ref(),
            &self.data.user_id,
            None,
            None,
        )
        .await
        .unwrap();
        run_id
    }

    async fn trace_calls(&self, env_id: Uuid, run_id: Uuid, count: usize) {
        let DatabasePool::Sqlite(pool) = self.db.as_ref() else {
            unreachable!("SQLite fixture");
        };
        for _ in 0..count {
            sqlx::query("INSERT INTO call(call_id,run_id,env_id,function_name,static_scope,runtime_path,call_depth,start_time,size) VALUES(?,?,?,'::purge/test','root','root',0,CURRENT_TIMESTAMP,1)")
                .bind(Uuid::now_v7())
                .bind(run_id)
                .bind(env_id)
                .execute(pool)
                .await
                .unwrap();
        }
    }

    async fn calls_in(&self, run_id: Uuid) -> i64 {
        let DatabasePool::Sqlite(pool) = self.db.as_ref() else {
            unreachable!("SQLite fixture");
        };
        sqlx::query_scalar("SELECT COUNT(*) FROM call WHERE run_id = ?")
            .bind(run_id)
            .fetch_one(pool)
            .await
            .unwrap()
    }
}

fn purged(body: &Value, calls: i64) {
    assert_eq!(body["data"]["calls_deleted"], calls, "{body}");
    assert_eq!(body["data"]["complete"], true, "{body}");
}

#[tokio::test]
async fn environment_keys_purge_calls_only_with_call_delete_and_in_scope() {
    let app = App::new().await;
    let (_, org_token) = app.org_key(json!({"org:*": ["create"]})).await;
    let (_, child_env) = app.create_child(&org_token, "elsewhere").await;
    let env = app.data.env_id;
    let run = app.run_in(env, app.data.stream_id).await;
    let other_stream = Uuid::now_v7();
    let stream_run = app.run_in(env, other_stream).await;
    let foreign_run = app.run_in(child_env, Uuid::now_v7()).await;
    app.trace_calls(env, run, 2).await;
    app.trace_calls(env, stream_run, 1).await;
    app.trace_calls(child_env, foreign_run, 1).await;
    let run_calls = format!("/v1/runs/{run}/calls");

    // Every run action still grants no call purge.
    let reader = app.env_key(env, json!({"run:*": ["*"]})).await;
    let one_run = app
        .env_key(env, json!({(format!("call:{run}/*")): ["delete"]}))
        .await;
    let all_runs = app
        .env_key(env, json!({"run:*": ["read"], "call:*": ["delete"]}))
        .await;
    for (token, path) in [
        (&reader, run_calls.clone()),
        (&one_run, format!("/v1/runs/{stream_run}/calls")),
        (&one_run, format!("/v1/streams/{other_stream}/calls")),
        (&one_run, "/v1/env/calls".into()),
    ] {
        app.expect("DELETE", &path, token, None, StatusCode::FORBIDDEN)
            .await;
    }
    assert_eq!(app.calls_in(run).await, 2);

    let body = app
        .expect("DELETE", &run_calls, &one_run, None, StatusCode::OK)
        .await;
    purged(&body, 2);
    // The run itself remains.
    app.expect(
        "GET",
        &format!("/v1/runs/{run}"),
        &all_runs,
        None,
        StatusCode::OK,
    )
    .await;

    // Another environment's run or stream is not found, and keeps its calls.
    for path in [
        format!("/v1/runs/{foreign_run}/calls"),
        format!("/v1/runs/{}/calls", Uuid::now_v7()),
        format!("/v1/streams/{}/calls", Uuid::now_v7()),
    ] {
        app.expect("DELETE", &path, &all_runs, None, StatusCode::NOT_FOUND)
            .await;
    }
    // Purging a run leaves the calls of tasks it started; purging its stream
    // reaches them.
    let task = app.task_of(env, other_stream, stream_run).await;
    app.trace_calls(env, task, 1).await;
    let body = app
        .expect(
            "DELETE",
            &format!("/v1/runs/{stream_run}/calls"),
            &all_runs,
            None,
            StatusCode::OK,
        )
        .await;
    purged(&body, 1);
    assert_eq!(app.calls_in(task).await, 1);
    let body = app
        .expect(
            "DELETE",
            &format!("/v1/streams/{other_stream}/calls"),
            &all_runs,
            None,
            StatusCode::OK,
        )
        .await;
    purged(&body, 1);

    app.trace_calls(env, run, 3).await;
    let body = app
        .expect("DELETE", "/v1/env/calls", &all_runs, None, StatusCode::OK)
        .await;
    purged(&body, 3);
    assert_eq!(app.calls_in(foreign_run).await, 1);
    // Org keys purge through /v1/orgs.
    let mismatch = app
        .expect(
            "DELETE",
            "/v1/env/calls",
            &org_token,
            None,
            StatusCode::FORBIDDEN,
        )
        .await;
    assert_eq!(mismatch["error"]["code"], "key_scope_mismatch");
}

#[tokio::test]
async fn org_keys_purge_calls_of_every_org_they_may_delete_calls_in() {
    let app = App::new().await;
    let root = app.data.org_id;
    let (_, provisioner) = app.org_key(json!({"org:*": ["create"]})).await;
    let (child, child_env) = app.create_child(&provisioner, "purged").await;
    let root_run = app.run_in(app.data.env_id, Uuid::now_v7()).await;
    let child_run = app.run_in(child_env, Uuid::now_v7()).await;
    app.trace_calls(app.data.env_id, root_run, 2).await;
    app.trace_calls(child_env, child_run, 1).await;
    let root_calls = format!("/v1/orgs/{root}/calls");
    let with_children = format!("{root_calls}?include_sub_orgs=true");

    let (_, reader) = app
        .org_key(json!({(format!("org:{root}/env:*/run:*")): ["*"], "org:*/env:*/run:*": ["*"]}))
        .await;
    let (_, own_only) = app
        .org_key(json!({(org_urn::calls(&root)): ["delete"]}))
        .await;
    let (_, children_only) = app.org_key(json!({"org:*/env:*/call:*": ["delete"]})).await;
    for (token, path) in [
        (&reader, root_calls.clone()),
        // `org:*` names the children, never the key's own org.
        (&children_only, root_calls.clone()),
        // Every org is authorized before anything is deleted.
        (&own_only, with_children.clone()),
    ] {
        app.expect("DELETE", &path, token, None, StatusCode::FORBIDDEN)
            .await;
    }
    assert_eq!(app.calls_in(root_run).await, 2);
    assert_eq!(app.calls_in(child_run).await, 1);

    let body = app
        .expect("DELETE", &root_calls, &own_only, None, StatusCode::OK)
        .await;
    purged(&body, 2);
    assert_eq!(app.calls_in(child_run).await, 1);
    let body = app
        .expect(
            "DELETE",
            &format!("/v1/orgs/{child}/calls"),
            &children_only,
            None,
            StatusCode::OK,
        )
        .await;
    purged(&body, 1);

    app.trace_calls(app.data.env_id, root_run, 1).await;
    app.trace_calls(child_env, child_run, 1).await;
    let (_, both) = app
        .org_key(json!({(org_urn::calls(&root)): ["delete"], "org:*/env:*/call:*": ["delete"]}))
        .await;
    let body = app
        .expect("DELETE", &with_children, &both, None, StatusCode::OK)
        .await;
    purged(&body, 2);
    // Environment keys purge through their own routes.
    let env_key = app.env_key(app.data.env_id, json!({"*:*": ["*"]})).await;
    let mismatch = app
        .expect("DELETE", &root_calls, &env_key, None, StatusCode::FORBIDDEN)
        .await;
    assert_eq!(mismatch["error"]["code"], "key_scope_mismatch");
}

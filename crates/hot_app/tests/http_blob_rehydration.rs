//! HTTP-level integration tests for BlobRef rehydration in the web app.
//!
//! Blob storage is meant to be invisible to users: large payloads are spilled
//! to content-addressed blob storage when persisted, and every read boundary
//! that shows the data to a user must transparently rehydrate the original
//! value. These tests exercise the app the way a browser does and assert that
//! a spilled run result comes back as the full original data, never as a
//! `::hot::blob/BlobRef` typed map.

use hot::db::{DatabasePool, insert_test_data};
use hot::val;
use hot::val::Val;
use hot_app::test_support::TestClient;
use uuid::Uuid;

fn blob_conf(storage_path: &str) -> Val {
    val!({
        "app": {"host": "localhost", "port": 4680},
        "blob": {
            "mode": "service",
            "spill": {"threshold-bytes": 1024},
        },
        "file": {"storage": {"path": storage_path}},
    })
}

async fn set_run_result(db: &DatabasePool, run_id: &Uuid, result: &serde_json::Value) {
    match db {
        DatabasePool::Sqlite(pool) => {
            sqlx::query("UPDATE run SET result = ? WHERE run_id = ?")
                .bind(result.to_string())
                .bind(run_id)
                .execute(pool)
                .await
                .expect("update run result");
        }
        _ => panic!("tests run against SQLite"),
    }
}

/// A run whose result was spilled to blob storage must be served back to the
/// dashboard fully rehydrated — users always get the actual data.
#[tokio::test]
async fn run_json_rehydrates_spilled_result() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let conf = blob_conf(temp_dir.path().to_str().unwrap());
    let mut client = TestClient::new_with_conf(conf.clone()).await;
    let td = insert_test_data(client.db()).await.expect("test data");
    client.login_as(&td.user_id);

    // Spill a large run result through the same store configuration the app
    // router was built with.
    let blob_store = hot::blob::blob_store_from_conf(client.db().clone(), &conf)
        .await
        .expect("blob store enabled by conf");
    let payload = "x".repeat(4096);
    let scope = hot::blob::BlobScope {
        org_id: td.org_id,
        env_id: Some(td.env_id),
        run_id: Some(td.run_id),
    };
    let spilled = blob_store
        .spill_large_val(
            val!({"result": payload.clone()}),
            scope,
            hot::blob::SpillSource::RunResult,
            Some(&td.run_id.to_string()),
        )
        .await
        .expect("spill run result");
    let spilled_json = serde_json::to_value(&spilled).expect("spilled to json");
    assert!(
        spilled_json.to_string().contains("::hot::blob/BlobRef"),
        "payload should have spilled: {}",
        spilled_json
    );
    set_run_result(client.db(), &td.run_id, &spilled_json).await;

    let resp = client.get(&format!("/data/runs/{}/json", td.run_id)).await;
    resp.assert_status(axum::http::StatusCode::OK);

    let body: serde_json::Value = serde_json::from_str(&resp.body).expect("json response");
    assert_eq!(
        body["result"]["result"].as_str(),
        Some(payload.as_str()),
        "result should be the full rehydrated value: {}",
        resp.body
    );
    assert!(
        !resp.body.contains("::hot::blob/BlobRef"),
        "no BlobRef should leak to the user: {}",
        resp.body
    );
}

/// The run detail page (HTML) must also render the rehydrated value.
#[tokio::test]
async fn run_detail_page_renders_rehydrated_result() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let conf = blob_conf(temp_dir.path().to_str().unwrap());
    let mut client = TestClient::new_with_conf(conf.clone()).await;
    let td = insert_test_data(client.db()).await.expect("test data");
    client.login_as(&td.user_id);

    let blob_store = hot::blob::blob_store_from_conf(client.db().clone(), &conf)
        .await
        .expect("blob store enabled by conf");
    // A recognizable marker inside a large payload.
    let payload = format!("rehydrated-marker-{}", "y".repeat(4096));
    let scope = hot::blob::BlobScope {
        org_id: td.org_id,
        env_id: Some(td.env_id),
        run_id: Some(td.run_id),
    };
    let spilled = blob_store
        .spill_large_val(
            val!({"result": payload.clone()}),
            scope,
            hot::blob::SpillSource::RunResult,
            Some(&td.run_id.to_string()),
        )
        .await
        .expect("spill run result");
    let spilled_json = serde_json::to_value(&spilled).expect("spilled to json");
    set_run_result(client.db(), &td.run_id, &spilled_json).await;

    let resp = client.get(&format!("/runs/{}", td.run_id)).await;
    resp.assert_status(axum::http::StatusCode::OK);
    assert!(
        resp.body.contains("rehydrated-marker-"),
        "detail page should show the rehydrated value"
    );
    assert!(
        !resp.body.contains("#blob["),
        "detail page should not fall back to the blob summary"
    );
}

async fn insert_call(
    db: &DatabasePool,
    run_id: &Uuid,
    call_id: &Uuid,
    args: &serde_json::Value,
    return_value: &serde_json::Value,
) {
    match db {
        DatabasePool::Sqlite(pool) => {
            sqlx::query(
                "INSERT INTO call (call_id, run_id, function_name, static_scope, runtime_path, \
                 call_depth, args, return_value, start_time, stop_time, duration_us) \
                 VALUES (?, ?, '::app/main', '::app/main', 'main-0', 0, ?, ?, \
                 datetime('now'), datetime('now'), 1000)",
            )
            .bind(call_id)
            .bind(run_id)
            .bind(args.to_string())
            .bind(return_value.to_string())
            .execute(pool)
            .await
            .expect("insert call");
        }
        _ => panic!("tests run against SQLite"),
    }
}

/// The hierarchy response must stay slim: call payloads (which may be spilled
/// BlobRefs) never travel with the tree. The inspector fetches them lazily via
/// GET /data/calls/{call_id}, which returns the fully rehydrated payloads.
#[tokio::test]
async fn hierarchy_is_lazy_and_call_detail_rehydrates() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let conf = blob_conf(temp_dir.path().to_str().unwrap());
    let mut client = TestClient::new_with_conf(conf.clone()).await;
    let td = insert_test_data(client.db()).await.expect("test data");
    client.login_as(&td.user_id);

    // Spill large call args the way the emitter would.
    let blob_store = hot::blob::blob_store_from_conf(client.db().clone(), &conf)
        .await
        .expect("blob store enabled by conf");
    let payload = format!("call-args-marker-{}", "a".repeat(4096));
    let scope = hot::blob::BlobScope {
        org_id: td.org_id,
        env_id: Some(td.env_id),
        run_id: Some(td.run_id),
    };
    let call_id = Uuid::now_v7();
    let spilled_args = blob_store
        .spill_large_val(
            val!({"input": payload.clone()}),
            scope,
            hot::blob::SpillSource::CallArgs,
            Some(&call_id.to_string()),
        )
        .await
        .expect("spill call args");
    let args_json = serde_json::to_value(&spilled_args).expect("args to json");
    assert!(
        args_json.to_string().contains("::hot::blob/BlobRef"),
        "args should have spilled: {}",
        args_json
    );
    let return_json = serde_json::json!({"ok": true});
    insert_call(client.db(), &td.run_id, &call_id, &args_json, &return_json).await;

    // 1. Hierarchy: metadata only — no payloads, no BlobRefs.
    let resp = client
        .get(&format!("/data/runs/{}/hierarchy", td.run_id))
        .await;
    resp.assert_status(axum::http::StatusCode::OK);
    let body: serde_json::Value = serde_json::from_str(&resp.body).expect("json response");
    assert_eq!(body["success"], serde_json::json!(true), "{}", resp.body);
    assert!(
        !resp.body.contains("::hot::blob/BlobRef"),
        "hierarchy must not carry BlobRefs: {}",
        resp.body
    );
    assert!(
        !resp.body.contains("call-args-marker-"),
        "hierarchy must not carry call payloads: {}",
        resp.body
    );
    let node = &body["data"]["tree"][0];
    assert_eq!(node["has_args"], serde_json::json!(true), "{}", resp.body);
    assert_eq!(
        node["has_return_value"],
        serde_json::json!(true),
        "{}",
        resp.body
    );
    assert!(
        node.get("args").is_none(),
        "tree nodes must not have an args field: {}",
        resp.body
    );

    // 2. Call detail: full payloads, transparently rehydrated.
    let resp = client.get(&format!("/data/calls/{}", call_id)).await;
    resp.assert_status(axum::http::StatusCode::OK);
    let body: serde_json::Value = serde_json::from_str(&resp.body).expect("json response");
    assert_eq!(body["success"], serde_json::json!(true), "{}", resp.body);
    assert_eq!(
        body["call"]["args"]["input"].as_str(),
        Some(payload.as_str()),
        "args should be the full rehydrated value: {}",
        resp.body
    );
    assert_eq!(body["call"]["return_value"], return_json, "{}", resp.body);
    assert!(
        !resp.body.contains("::hot::blob/BlobRef"),
        "no BlobRef should leak from the call detail endpoint: {}",
        resp.body
    );
}

/// Server-side payload search: since call payloads no longer travel with the
/// hierarchy, the UI asks the server which calls match a search term. Matches
/// must cover args and return values, be case-insensitive, and — for spilled
/// payloads — match against the inline BlobRef preview text.
#[tokio::test]
async fn call_search_matches_args_results_and_blob_previews() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let conf = blob_conf(temp_dir.path().to_str().unwrap());
    let mut client = TestClient::new_with_conf(conf.clone()).await;
    let td = insert_test_data(client.db()).await.expect("test data");
    client.login_as(&td.user_id);

    // Call 1: plain args/result.
    let call_plain = Uuid::now_v7();
    insert_call(
        client.db(),
        &td.run_id,
        &call_plain,
        &serde_json::json!({"city": "Amsterdam"}),
        &serde_json::json!({"status": "SHIPPED"}),
    )
    .await;

    // Call 2: result spilled to a blob; the term appears in the preview
    // (first 256 bytes) of the spilled string.
    let blob_store = hot::blob::blob_store_from_conf(client.db().clone(), &conf)
        .await
        .expect("blob store enabled by conf");
    let call_spilled = Uuid::now_v7();
    let scope = hot::blob::BlobScope {
        org_id: td.org_id,
        env_id: Some(td.env_id),
        run_id: Some(td.run_id),
    };
    let payload = format!("needle-in-preview {}", "b".repeat(4096));
    let spilled = blob_store
        .spill_large_val(
            val!({"data": payload}),
            scope,
            hot::blob::SpillSource::CallReturn,
            Some(&call_spilled.to_string()),
        )
        .await
        .expect("spill call return");
    let spilled_json = serde_json::to_value(&spilled).expect("spilled to json");
    insert_call(
        client.db(),
        &td.run_id,
        &call_spilled,
        &serde_json::json!(null),
        &spilled_json,
    )
    .await;

    let search = |q: &str| format!("/data/runs/{}/calls/search?q={}", td.run_id, q);
    let ids = |body: &str| -> Vec<String> {
        let v: serde_json::Value = serde_json::from_str(body).expect("json response");
        assert_eq!(v["success"], serde_json::json!(true), "{}", body);
        v["call_ids"]
            .as_array()
            .expect("call_ids array")
            .iter()
            .map(|id| id.as_str().unwrap().to_string())
            .collect()
    };

    // Args match, case-insensitive.
    let resp = client.get(&search("amsterdam")).await;
    assert_eq!(ids(&resp.body), vec![call_plain.to_string()]);

    // Return value match, case-insensitive.
    let resp = client.get(&search("shipped")).await;
    assert_eq!(ids(&resp.body), vec![call_plain.to_string()]);

    // Spilled payload matches via the BlobRef preview text.
    let resp = client.get(&search("needle-in-preview")).await;
    assert_eq!(ids(&resp.body), vec![call_spilled.to_string()]);

    // No match.
    let resp = client.get(&search("nonexistent-term")).await;
    assert!(ids(&resp.body).is_empty());

    // LIKE wildcards in the term are treated literally, not as wildcards.
    let resp = client.get(&search("%25")).await; // url-encoded "%"
    assert!(ids(&resp.body).is_empty());

    // Empty term returns no matches rather than everything.
    let resp = client.get(&search("")).await;
    assert!(ids(&resp.body).is_empty());
}

/// Payload search must not be usable by a user without access to the run's
/// environment.
#[tokio::test]
async fn call_search_requires_env_access() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let conf = blob_conf(temp_dir.path().to_str().unwrap());
    let mut client = TestClient::new_with_conf(conf).await;
    let td = insert_test_data(client.db()).await.expect("test data");

    let call_id = Uuid::now_v7();
    insert_call(
        client.db(),
        &td.run_id,
        &call_id,
        &serde_json::json!({"input": "secret"}),
        &serde_json::json!({"ok": true}),
    )
    .await;

    let outsider_id = Uuid::now_v7();
    hot::db::User::insert_user(
        client.db(),
        &outsider_id,
        "outsider2@example.com",
        Some("Outsider Two"),
        Some(&td.user_id),
    )
    .await
    .expect("insert outsider");
    client.login_as(&outsider_id);

    let resp = client
        .get(&format!("/data/runs/{}/calls/search?q=secret", td.run_id))
        .await;
    assert!(
        !resp.body.contains(&call_id.to_string()),
        "outsider must not learn which calls match: {} {}",
        resp.status,
        resp.body
    );
}

/// Call detail must not be readable by a user without access to the run's
/// environment (cross-tenant isolation).
#[tokio::test]
async fn call_detail_requires_env_access() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let conf = blob_conf(temp_dir.path().to_str().unwrap());
    let mut client = TestClient::new_with_conf(conf).await;
    let td = insert_test_data(client.db()).await.expect("test data");

    let call_id = Uuid::now_v7();
    insert_call(
        client.db(),
        &td.run_id,
        &call_id,
        &serde_json::json!({"input": "secret"}),
        &serde_json::json!({"ok": true}),
    )
    .await;

    // An outsider user with no membership in the run's org.
    let outsider_id = Uuid::now_v7();
    hot::db::User::insert_user(
        client.db(),
        &outsider_id,
        "outsider@example.com",
        Some("Outsider"),
        Some(&td.user_id),
    )
    .await
    .expect("insert outsider");
    client.login_as(&outsider_id);

    let resp = client.get(&format!("/data/calls/{}", call_id)).await;
    assert!(
        !resp.body.contains("secret"),
        "outsider must not see call payloads: {} {}",
        resp.status,
        resp.body
    );
}

/// When blob mode is disabled (no store), pages must still render: the
/// BlobRef map falls back to the compact summary instead of erroring.
#[tokio::test]
async fn run_detail_page_falls_back_to_summary_without_store() {
    let spill_dir = tempfile::TempDir::new().unwrap();
    // App conf has blobs disabled — no rehydration possible.
    let conf = val!({"app": {"host": "localhost", "port": 4680}});
    let mut client = TestClient::new_with_conf(conf).await;
    let td = insert_test_data(client.db()).await.expect("test data");
    client.login_as(&td.user_id);

    // Spill with a separately-configured store to simulate data written by a
    // worker with blobs enabled.
    let spill_conf = blob_conf(spill_dir.path().to_str().unwrap());
    let blob_store = hot::blob::blob_store_from_conf(client.db().clone(), &spill_conf)
        .await
        .expect("blob store enabled by conf");
    let payload = "z".repeat(4096);
    let scope = hot::blob::BlobScope {
        org_id: td.org_id,
        env_id: Some(td.env_id),
        run_id: Some(td.run_id),
    };
    let spilled = blob_store
        .spill_large_val(
            val!({"result": payload}),
            scope,
            hot::blob::SpillSource::RunResult,
            Some(&td.run_id.to_string()),
        )
        .await
        .expect("spill run result");
    let spilled_json = serde_json::to_value(&spilled).expect("spilled to json");
    set_run_result(client.db(), &td.run_id, &spilled_json).await;

    let resp = client.get(&format!("/runs/{}", td.run_id)).await;
    resp.assert_status(axum::http::StatusCode::OK);
    assert!(
        resp.body.contains("#blob["),
        "without a store the page should render the compact summary"
    );
}

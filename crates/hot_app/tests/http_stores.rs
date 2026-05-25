//! HTTP-level integration tests for the `::hot::store` browser pages.
//!
//! Drives the real Axum router via `hot_app::test_support`, seeds entries
//! into the SQLite store backend (which now lives in the main hot DB), and
//! asserts list/detail/delete behavior including the admin gate on entry
//! deletion.

use hot::db::{insert_test_data, org::OrgUser, user::User};
use hot::store::sqlite::SqliteStore;
use hot::store::{Store, StoreMapConfig};
use hot::val;
use hot_app::test_support::TestClient;
use serde_json::json;
use std::sync::Arc;
use uuid::Uuid;

fn store_conf() -> hot::val::Val {
    val!({
        "app": {
            "host": "localhost",
            "port": 4680
        }
    })
}

fn make_store(db: &Arc<hot::db::DatabasePool>, org_id: Uuid, env_id: Uuid) -> SqliteStore {
    SqliteStore::new(db.clone(), org_id, env_id)
}

async fn seed_two_stores(db: &Arc<hot::db::DatabasePool>, org_id: Uuid, env_id: Uuid) {
    let store = make_store(db, org_id, env_id);
    store
        .ensure_store(&StoreMapConfig {
            name: "settings".to_string(),
            embedding_model: None,
            embedding_field: None,
            embedding_dimensions: None,
            text_search: false,
        })
        .await
        .unwrap();
    store
        .put(
            "settings",
            json!("welcome"),
            json!({"text": "hi"}),
            None,
            None,
        )
        .await
        .unwrap();
    store
        .put("settings", json!("retries"), json!(3), None, None)
        .await
        .unwrap();
    store
        .put(
            "settings",
            json!("quote ' \" emoji 🔥"),
            json!({"text": "unicode"}),
            None,
            None,
        )
        .await
        .unwrap();

    store
        .ensure_store(&StoreMapConfig {
            name: "docs".to_string(),
            embedding_model: Some("text-embedding-3-small".to_string()),
            embedding_field: Some("body".to_string()),
            embedding_dimensions: Some(1536),
            text_search: true,
        })
        .await
        .unwrap();
    store
        .put(
            "docs",
            json!({"id": 1}),
            json!({"title": "Hello"}),
            None,
            Some("hello world".into()),
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn stores_list_renders_seeded_stores() {
    let mut client = TestClient::new_with_conf(store_conf()).await;

    let test_data = insert_test_data(client.db()).await.expect("test data");
    seed_two_stores(client.db(), test_data.org_id, test_data.env_id).await;

    client.login_as(&test_data.user_id);

    let resp = client.get("/stores").await;
    assert_eq!(resp.status, axum::http::StatusCode::OK);

    let body = &resp.body;
    assert!(
        body.contains("settings"),
        "list should mention settings store"
    );
    assert!(body.contains("docs"), "list should mention docs store");
    assert!(body.contains("text-embedding-3-small"));
    assert!(body.contains("Text Search"));
}

#[tokio::test]
async fn stores_list_filter_by_name() {
    let mut client = TestClient::new_with_conf(store_conf()).await;

    let test_data = insert_test_data(client.db()).await.unwrap();
    seed_two_stores(client.db(), test_data.org_id, test_data.env_id).await;
    client.login_as(&test_data.user_id);

    let resp = client.get("/stores?search=docs").await;
    assert_eq!(resp.status, axum::http::StatusCode::OK);
    let body = &resp.body;
    assert!(body.contains("docs"));
    assert!(
        !body.contains("/stores/settings"),
        "settings should be filtered out of /stores?search=docs"
    );
}

#[tokio::test]
async fn store_detail_renders_entries() {
    let mut client = TestClient::new_with_conf(store_conf()).await;

    let test_data = insert_test_data(client.db()).await.unwrap();
    seed_two_stores(client.db(), test_data.org_id, test_data.env_id).await;
    client.login_as(&test_data.user_id);

    let resp = client.get("/stores/settings").await;
    assert_eq!(resp.status, axum::http::StatusCode::OK);
    let body = &resp.body;
    assert!(body.contains("welcome"));
    assert!(body.contains("retries"));
    assert!(body.contains("emoji"));
    assert!(body.contains("Show values"));
    assert!(body.contains("store-values-column-toggle"));
    assert!(
        !body.contains("Reveal value"),
        "store detail should reveal values only through the column header toggle"
    );
    assert!(
        !body.contains("hot.store.showValues"),
        "store detail uses a column-level toggle, not a persisted session toggle"
    );
    assert!(
        !body.contains("unicode"),
        "store detail should not render values before reveal"
    );
    assert!(
        !body.contains("store-val-hot-"),
        "store detail should not embed hidden value blobs before reveal"
    );
    assert!(body.contains("data-key-preview="));
    assert!(
        !body.contains("confirmDeleteStoreEntry('"),
        "key preview should not be interpolated into inline JavaScript"
    );
    assert!(body.contains("Delete"));
}

#[tokio::test]
async fn store_detail_htmx_request_returns_partial() {
    let mut client = TestClient::new_with_conf(store_conf()).await;

    let test_data = insert_test_data(client.db()).await.unwrap();
    seed_two_stores(client.db(), test_data.org_id, test_data.env_id).await;
    client.login_as(&test_data.user_id);

    let resp = client
        .get_with_headers("/stores/settings?search=welcome", &[("HX-Request", "true")])
        .await;
    assert_eq!(resp.status, axum::http::StatusCode::OK);

    assert!(
        !resp.body.contains("<html"),
        "HTMX response should not include the full page layout"
    );
    assert!(
        !resp.body.contains("Search keys and values"),
        "HTMX partial should not re-render the search filter card"
    );
    assert!(
        resp.body.contains("welcome"),
        "HTMX partial should include matching entries"
    );
    assert!(
        resp.body.contains("matching entr"),
        "HTMX partial should render the matching entries summary"
    );
}

#[tokio::test]
async fn store_detail_searches_keys_and_values_without_revealing_values() {
    let mut client = TestClient::new_with_conf(store_conf()).await;

    let test_data = insert_test_data(client.db()).await.unwrap();
    seed_two_stores(client.db(), test_data.org_id, test_data.env_id).await;
    client.login_as(&test_data.user_id);

    let by_key = client.get("/stores/settings?search=welcome").await;
    assert_eq!(by_key.status, axum::http::StatusCode::OK);
    assert!(by_key.body.contains("welcome"));
    assert!(
        !by_key.body.contains("retries"),
        "search by key should filter out unrelated entries"
    );
    assert!(
        !by_key.body.contains("store-val-hot-"),
        "search results should keep values hidden until reveal"
    );

    let by_value = client.get("/stores/settings?search=unicode").await;
    assert_eq!(by_value.status, axum::http::StatusCode::OK);
    assert!(by_value.body.contains("Search keys and values"));
    assert!(
        by_value.body.contains("emoji"),
        "search should match entry values as well as keys"
    );
    assert!(
        !by_value.body.contains("welcome"),
        "value search should filter out unrelated keys"
    );
    assert!(
        !by_value.body.contains("store-val-hot-"),
        "value search should not render hidden value blobs"
    );
}

#[tokio::test]
async fn entry_detail_hides_value_until_admin_reveal() {
    let mut client = TestClient::new_with_conf(store_conf()).await;

    let test_data = insert_test_data(client.db()).await.unwrap();
    seed_two_stores(client.db(), test_data.org_id, test_data.env_id).await;
    client.login_as(&test_data.user_id);

    let key_encoded = hot_app::templates::encode_entry_key(&json!("quote ' \" emoji 🔥"));
    let resp = client
        .get(&format!("/stores/settings/entries/{}", key_encoded))
        .await;
    assert_eq!(resp.status, axum::http::StatusCode::OK);
    let body = &resp.body;
    assert!(body.contains("emoji"));
    assert!(body.contains("Reveal value"));
    assert!(
        !body.contains("hot.store.showValues"),
        "entry detail should not persist value visibility in localStorage"
    );
    assert!(
        !body.contains("unicode"),
        "entry detail should not render the value before reveal"
    );
    assert!(
        !body.contains("entry-val-hot"),
        "entry detail should not embed hidden value blobs before reveal"
    );

    let reveal = client
        .get(&format!(
            "/stores/settings/entries/{}/value?view=panel",
            key_encoded
        ))
        .await;
    assert_eq!(reveal.status, axum::http::StatusCode::OK);
    assert_eq!(
        reveal
            .headers
            .get(axum::http::header::CACHE_CONTROL)
            .and_then(|v| v.to_str().ok()),
        Some("no-store, private")
    );
    assert_eq!(
        reveal
            .headers
            .get(axum::http::header::PRAGMA)
            .and_then(|v| v.to_str().ok()),
        Some("no-cache")
    );
    assert!(reveal.body.contains("unicode"));
    assert!(reveal.body.contains("View full value"));
    assert!(reveal.body.contains("language-hot"));
}

#[tokio::test]
async fn admin_can_reveal_value_from_store_detail() {
    let mut client = TestClient::new_with_conf(store_conf()).await;

    let test_data = insert_test_data(client.db()).await.unwrap();
    seed_two_stores(client.db(), test_data.org_id, test_data.env_id).await;
    client.login_as(&test_data.user_id);

    let key_encoded = hot_app::templates::encode_entry_key(&json!("quote ' \" emoji 🔥"));
    let reveal = client
        .get(&format!(
            "/stores/settings/entries/{}/value?view=cell",
            key_encoded
        ))
        .await;
    assert_eq!(reveal.status, axum::http::StatusCode::OK);
    assert_eq!(
        reveal
            .headers
            .get(axum::http::header::CACHE_CONTROL)
            .and_then(|v| v.to_str().ok()),
        Some("no-store, private")
    );
    assert!(reveal.body.contains("unicode"));
    assert!(reveal.body.contains("store-val-hot-"));
    assert!(reveal.body.contains("language-hot"));
}

#[tokio::test]
async fn hidden_cell_fragment_matches_initial_render() {
    let mut client = TestClient::new_with_conf(store_conf()).await;

    let test_data = insert_test_data(client.db()).await.unwrap();
    seed_two_stores(client.db(), test_data.org_id, test_data.env_id).await;
    client.login_as(&test_data.user_id);

    let key_encoded = hot_app::templates::encode_entry_key(&json!("retries"));

    // Cell variant.
    let cell = client
        .get(&format!(
            "/stores/settings/entries/{}/value?view=hidden-cell",
            key_encoded
        ))
        .await;
    assert_eq!(cell.status, axum::http::StatusCode::OK);
    assert_eq!(
        cell.headers
            .get(axum::http::header::CACHE_CONTROL)
            .and_then(|v| v.to_str().ok()),
        Some("no-store, private")
    );
    assert!(
        !cell.body.contains("Reveal value"),
        "hidden table cells should not include per-row reveal controls"
    );
    assert!(cell.body.contains("Hidden by default"));
    assert!(cell.body.contains("data-revealed=\"false\""));
    assert!(cell.body.contains(&format!("entry-value-{}", key_encoded)));
    assert!(
        !cell.body.contains("\"3\""),
        "hidden cell fragment must not leak the entry value: {}",
        cell.body
    );

    // Mobile variant.
    let m_cell = client
        .get(&format!(
            "/stores/settings/entries/{}/value?view=hidden-mobile-cell",
            key_encoded
        ))
        .await;
    assert_eq!(m_cell.status, axum::http::StatusCode::OK);
    assert!(
        !m_cell.body.contains("Reveal value"),
        "hidden mobile cells should not include per-row reveal controls"
    );
    assert!(m_cell.body.contains("Hidden by default"));
    assert!(
        m_cell
            .body
            .contains(&format!("m-entry-value-{}", key_encoded))
    );
}

#[tokio::test]
async fn admin_can_delete_store_entry() {
    let mut client = TestClient::new_with_conf(store_conf()).await;

    let test_data = insert_test_data(client.db()).await.unwrap();
    seed_two_stores(client.db(), test_data.org_id, test_data.env_id).await;
    client.login_as(&test_data.user_id);

    let key_encoded = hot_app::templates::encode_entry_key(&json!("welcome"));
    let resp = client
        .post_form(
            "/stores/settings/entries/delete",
            &[("key_encoded", key_encoded.as_str()), ("origin", "store")],
        )
        .await;
    assert!(resp.is_redirect(), "delete should redirect");
    assert!(
        resp.location().unwrap_or("").contains("deleted=1"),
        "delete should redirect with deleted flash"
    );

    let store = make_store(client.db(), test_data.org_id, test_data.env_id);
    assert!(
        store
            .get("settings", &json!("welcome"))
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn non_admin_cannot_delete_store_entry() {
    let mut client = TestClient::new_with_conf(store_conf()).await;

    let test_data = insert_test_data(client.db()).await.unwrap();
    seed_two_stores(client.db(), test_data.org_id, test_data.env_id).await;

    let viewer_id = Uuid::now_v7();
    User::insert_user(
        client.db(),
        &viewer_id,
        "viewer@example.com",
        Some("Viewer"),
        Some(&test_data.user_id),
    )
    .await
    .unwrap();
    OrgUser::insert_org_user(
        client.db(),
        &Uuid::now_v7(),
        &test_data.org_id,
        &viewer_id,
        Some(1), // 1 = member, 2 = admin
        &test_data.user_id,
    )
    .await
    .unwrap();

    client.login_as(&viewer_id);

    let key_encoded = hot_app::templates::encode_entry_key(&json!("retries"));
    let reveal = client
        .get(&format!(
            "/stores/settings/entries/{}/value?view=cell",
            key_encoded
        ))
        .await;
    assert_eq!(reveal.status, axum::http::StatusCode::FORBIDDEN);

    let resp = client
        .post_form(
            "/stores/settings/entries/delete",
            &[("key_encoded", key_encoded.as_str()), ("origin", "store")],
        )
        .await;
    assert_eq!(resp.status, axum::http::StatusCode::FORBIDDEN);

    let store = make_store(client.db(), test_data.org_id, test_data.env_id);
    assert!(
        store
            .get("settings", &json!("retries"))
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn non_admin_detail_page_omits_delete_controls() {
    let mut client = TestClient::new_with_conf(store_conf()).await;

    let test_data = insert_test_data(client.db()).await.unwrap();
    seed_two_stores(client.db(), test_data.org_id, test_data.env_id).await;

    let viewer_id = Uuid::now_v7();
    User::insert_user(
        client.db(),
        &viewer_id,
        "viewer2@example.com",
        Some("Viewer Two"),
        Some(&test_data.user_id),
    )
    .await
    .unwrap();
    OrgUser::insert_org_user(
        client.db(),
        &Uuid::now_v7(),
        &test_data.org_id,
        &viewer_id,
        Some(1),
        &test_data.user_id,
    )
    .await
    .unwrap();
    client.login_as(&viewer_id);

    let resp = client.get("/stores/settings").await;
    assert_eq!(resp.status, axum::http::StatusCode::OK);
    let body = &resp.body;
    assert!(body.contains("welcome"));
    assert!(!body.contains("Reveal value"));
    assert!(!body.contains("unicode"));
    assert!(
        !body.contains("/stores/settings/entries/delete"),
        "non-admin should not see delete form"
    );
}

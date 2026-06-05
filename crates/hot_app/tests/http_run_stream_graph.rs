//! HTTP-level integration tests for the run stream-graph JSON partial.
//!
//! `GET /data/runs/{run_id}/stream-graph` backs the live refresh of the run
//! detail Stream Graph tab. It must return current graph data (not the stale
//! page-embedded snapshot) and enforce environment ownership.

use hot::db::insert_test_data;
use hot_app::test_support::TestClient;
use uuid::Uuid;

#[tokio::test]
async fn run_stream_graph_returns_graph_json_for_owned_run() {
    let mut client = TestClient::new().await;

    let test_data = insert_test_data(client.db()).await.expect("test data");
    client.login_as(&test_data.user_id);

    let resp = client
        .get(&format!("/data/runs/{}/stream-graph", test_data.run_id))
        .await;

    assert_eq!(resp.status, axum::http::StatusCode::OK);
    assert!(
        resp.body.contains("\"nodes\""),
        "stream-graph response should include nodes: {}",
        resp.body
    );
    assert!(
        resp.body.contains("\"edges\""),
        "stream-graph response should include edges: {}",
        resp.body
    );
    assert!(
        !resp.body.contains("\"error\""),
        "owned run should not return an error payload: {}",
        resp.body
    );
}

#[tokio::test]
async fn run_stream_graph_unknown_run_returns_error_payload() {
    let mut client = TestClient::new().await;

    let test_data = insert_test_data(client.db()).await.expect("test data");
    client.login_as(&test_data.user_id);

    let resp = client
        .get(&format!("/data/runs/{}/stream-graph", Uuid::now_v7()))
        .await;

    // The handler returns a JSON error body (not an HTTP error) for unknown runs.
    assert_eq!(resp.status, axum::http::StatusCode::OK);
    assert!(
        resp.body.contains("\"error\""),
        "unknown run should return an error payload: {}",
        resp.body
    );
}

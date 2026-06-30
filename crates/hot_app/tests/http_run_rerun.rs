//! HTTP-level integration tests for the run re-run / retry endpoints.
//!
//! `POST /runs/{run_id}/rerun` and `POST /runs/{run_id}/retry` re-dispatch a
//! previous run. The handler must re-dispatch *based on the original event
//! type*:
//!
//! - `hot:call` / `hot:schedule` runs carry a target function in their event
//!   data and re-dispatch as a `hot:call`.
//! - Event-handler runs are triggered by custom event types (e.g.
//!   `data:analyze` via `meta {on-event: ...}`) and have no `fn` field — they
//!   must be re-emitted with their original event type and data so the matching
//!   on-event handlers fire again. Regression test for the
//!   "Original event has no function name" failure.

use hot::db::insert_test_data;
use hot::db::run::RunStatus;
use hot::db::{DatabasePool, Event};
use hot_app::test_support::TestClient;
use serde_json::json;
use uuid::Uuid;

/// Link an event to the seeded run and force the run into `status`.
///
/// `insert_test_data` creates a run without an `event_id`, so we wire one up
/// directly. There is no public helper that sets `run.event_id`, hence the raw
/// UPDATE.
async fn link_event_and_set_status(
    db: &DatabasePool,
    run_id: &Uuid,
    event_id: &Uuid,
    status: RunStatus,
) {
    match db {
        DatabasePool::Sqlite(pool) => {
            sqlx::query("UPDATE run SET event_id = ?, status_id = ? WHERE run_id = ?")
                .bind(event_id)
                .bind(status.as_id())
                .bind(run_id)
                .execute(pool)
                .await
                .expect("link event to run");
        }
        _ => panic!("tests run against SQLite"),
    }
}

/// Re-running an event-handler run (custom event type, no `fn`) must re-emit
/// the original event verbatim instead of failing with
/// "Original event has no function name".
#[tokio::test]
async fn rerun_event_handler_run_reemits_original_event() {
    let mut client = TestClient::new().await;
    let td = insert_test_data(client.db()).await.expect("test data");
    client.login_as(&td.user_id);

    let event_id = Uuid::now_v7();
    Event::insert_event(
        client.db(),
        &event_id,
        &td.env_id,
        &td.stream_id,
        "data:analyze",
        &json!({ "file": "hot://demo/orders.csv" }),
        chrono::Utc::now(),
        &td.user_id,
        None,
    )
    .await
    .expect("insert data:analyze event");

    link_event_and_set_status(client.db(), &td.run_id, &event_id, RunStatus::Succeeded).await;

    let resp = client
        .post_form(&format!("/runs/{}/rerun", td.run_id), &[])
        .await;

    resp.assert_status(axum::http::StatusCode::OK);
    let body: serde_json::Value = serde_json::from_str(&resp.body).expect("json response");
    assert_eq!(body["success"], json!(true), "body = {}", resp.body);
    assert_eq!(body["action"], json!("re-run"), "body = {}", resp.body);

    // The newly emitted event must preserve the original event type and data.
    let new_event_id =
        Uuid::parse_str(body["event_id"].as_str().expect("event_id string")).expect("uuid");
    let new_event = Event::get_event(client.db(), &new_event_id)
        .await
        .expect("new event exists");

    assert_eq!(new_event.event_type, "data:analyze");
    assert_eq!(
        new_event.event_data.get("file").and_then(|v| v.as_str()),
        Some("hot://demo/orders.csv"),
        "original payload should be preserved: {:?}",
        new_event.event_data
    );
    assert!(
        new_event.event_data.get("fn").is_none(),
        "custom event re-run must not synthesize a fn field: {:?}",
        new_event.event_data
    );
    // Re-run (not retry) starts a fresh stream with no retry linkage.
    assert!(
        new_event.event_data.get("retry").is_none(),
        "re-run must not attach retry context: {:?}",
        new_event.event_data
    );
}

/// Retrying a failed event-handler run re-emits the original event and attaches
/// retry context (origin run + attempt) so the worker links the new run.
#[tokio::test]
async fn retry_event_handler_run_attaches_retry_context() {
    let mut client = TestClient::new().await;
    let td = insert_test_data(client.db()).await.expect("test data");
    client.login_as(&td.user_id);

    let event_id = Uuid::now_v7();
    Event::insert_event(
        client.db(),
        &event_id,
        &td.env_id,
        &td.stream_id,
        "data:analyze",
        &json!({ "file": "hot://demo/orders.csv" }),
        chrono::Utc::now(),
        &td.user_id,
        None,
    )
    .await
    .expect("insert data:analyze event");

    // Retry only applies to failed runs.
    link_event_and_set_status(client.db(), &td.run_id, &event_id, RunStatus::Failed).await;

    let resp = client
        .post_form(&format!("/runs/{}/retry", td.run_id), &[])
        .await;

    resp.assert_status(axum::http::StatusCode::OK);
    let body: serde_json::Value = serde_json::from_str(&resp.body).expect("json response");
    assert_eq!(body["action"], json!("retried"), "body = {}", resp.body);

    let new_event_id =
        Uuid::parse_str(body["event_id"].as_str().expect("event_id string")).expect("uuid");
    let new_event = Event::get_event(client.db(), &new_event_id)
        .await
        .expect("new event exists");

    assert_eq!(new_event.event_type, "data:analyze");
    assert_eq!(
        new_event.event_data.get("file").and_then(|v| v.as_str()),
        Some("hot://demo/orders.csv"),
    );
    let retry = new_event
        .event_data
        .get("retry")
        .expect("retry context attached");
    assert_eq!(
        retry.get("origin-run-id").and_then(|v| v.as_str()),
        Some(td.run_id.to_string().as_str()),
        "retry should reference the original run: {:?}",
        retry
    );
    assert_eq!(
        retry.get("attempt").and_then(|v| v.as_i64()),
        Some(1),
        "first retry should be attempt 1: {:?}",
        retry
    );
}

/// Regression guard: re-running a direct function call (`hot:call`) still
/// re-dispatches as a `hot:call` with the original function name and args.
#[tokio::test]
async fn rerun_function_call_run_redispatches_hot_call() {
    let mut client = TestClient::new().await;
    let td = insert_test_data(client.db()).await.expect("test data");
    client.login_as(&td.user_id);

    let event_id = Uuid::now_v7();
    Event::insert_event(
        client.db(),
        &event_id,
        &td.env_id,
        &td.stream_id,
        "hot:call",
        &json!({ "fn": "::demo/run-analysis", "args": [{ "x": 1 }] }),
        chrono::Utc::now(),
        &td.user_id,
        None,
    )
    .await
    .expect("insert hot:call event");

    link_event_and_set_status(client.db(), &td.run_id, &event_id, RunStatus::Succeeded).await;

    let resp = client
        .post_form(&format!("/runs/{}/rerun", td.run_id), &[])
        .await;

    resp.assert_status(axum::http::StatusCode::OK);
    let body: serde_json::Value = serde_json::from_str(&resp.body).expect("json response");
    assert_eq!(body["success"], json!(true), "body = {}", resp.body);

    let new_event_id =
        Uuid::parse_str(body["event_id"].as_str().expect("event_id string")).expect("uuid");
    let new_event = Event::get_event(client.db(), &new_event_id)
        .await
        .expect("new event exists");

    assert_eq!(new_event.event_type, "hot:call");
    assert_eq!(
        new_event.event_data.get("fn").and_then(|v| v.as_str()),
        Some("::demo/run-analysis"),
        "function name should be preserved: {:?}",
        new_event.event_data
    );
    assert_eq!(
        new_event.event_data.get("args"),
        Some(&json!([{ "x": 1 }])),
        "args should be preserved: {:?}",
        new_event.event_data
    );
}

/// A run with no associated event still produces the explicit
/// "Run has no associated event" error (not a panic / 500).
#[tokio::test]
async fn rerun_run_without_event_returns_bad_request() {
    let mut client = TestClient::new().await;
    let td = insert_test_data(client.db()).await.expect("test data");
    client.login_as(&td.user_id);

    // Seeded run has no event_id linked.
    let resp = client
        .post_form(&format!("/runs/{}/rerun", td.run_id), &[])
        .await;

    resp.assert_status(axum::http::StatusCode::BAD_REQUEST);
    assert!(
        resp.body.contains("Run has no associated event"),
        "expected missing-event error: {}",
        resp.body
    );
}

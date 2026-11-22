//! What a run's hierarchy says about its calls on an installation that deletes
//! them after `db.retention.call.days`: an old run's calls are gone because
//! they expired, not because it recorded none, and the retention that deleted
//! them is the one reported.

use hot::db::{DatabasePool, insert_test_data};
use hot::val;
use hot::val::Val;
use hot_app::test_support::TestClient;

/// The hierarchy's call data status and retention, for a run of an organization
/// without a plan that started `days_ago`.
async fn told_about_run_started(days_ago: i64, conf: Val) -> (String, i64) {
    let mut client = TestClient::new_with_conf(conf).await;
    let td = insert_test_data(client.db()).await.expect("test data");
    client.login_as(&td.user_id);
    let DatabasePool::Sqlite(pool) = client.db().as_ref() else {
        panic!("tests run against SQLite");
    };
    sqlx::query("UPDATE run SET start_time = ? WHERE run_id = ?")
        .bind(chrono::Utc::now() - chrono::Duration::days(days_ago))
        .bind(td.run_id)
        .execute(pool)
        .await
        .expect("move the run back");

    let resp = client
        .get(&format!("/data/runs/{}/hierarchy", td.run_id))
        .await;
    resp.assert_status(axum::http::StatusCode::OK);
    let body: serde_json::Value = serde_json::from_str(&resp.body).expect("json response");
    assert_eq!(body["success"], serde_json::json!(true), "{}", resp.body);
    (
        body["call_data_status"]
            .as_str()
            .expect("status")
            .to_string(),
        body["call_retention_days"].as_i64().expect("retention"),
    )
}

fn deleting_calls_after_30_days() -> Val {
    val!({
        "app": {"host": "localhost", "port": 4680},
        "db": {"retention": {"call": {"days": 30}}},
    })
}

#[tokio::test]
async fn a_run_older_than_the_installations_retention_has_expired() {
    let (status, days) = told_about_run_started(45, deleting_calls_after_30_days()).await;
    assert_eq!((status.as_str(), days), ("expired", 30));
}

#[tokio::test]
async fn a_run_inside_the_installations_retention_has_not_expired() {
    let (status, days) = told_about_run_started(5, deleting_calls_after_30_days()).await;
    assert_ne!(status, "expired");
    assert_eq!(days, 30);
}

#[tokio::test]
async fn an_installation_that_keeps_calls_expires_nothing() {
    let conf = val!({"app": {"host": "localhost", "port": 4680}});
    let (status, days) = told_about_run_started(45, conf).await;
    assert_ne!(status, "expired");
    assert_eq!(days, -1);
}

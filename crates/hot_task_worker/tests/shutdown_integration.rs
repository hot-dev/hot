//! Integration tests for `TaskShutdownCoordinator`.
//!
//! These exercise the end-to-end shutdown flow against a SQLite in-memory
//! database and an in-process `MemoryQueue`, so we can verify the
//! observable side effects (DB rows updated, retry copies enqueued, etc.)
//! without needing real Postgres or Redis.

use hot::data::serialization::Serialization;
use hot::db::{self, DatabasePool, Task, TaskStatus};
use hot::lang::hot::task::TaskRequest;
use hot::queue::{ProcessingQueue, Queue, QueueType};
use hot::stream::{StreamPubSub, StreamPubSubType};
use hot::val;
use hot_task_worker::shutdown::{ActiveTask, TaskShutdownCoordinator};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Test helpers
// ---------------------------------------------------------------------------

async fn create_test_db() -> DatabasePool {
    let db_conf = val!({"uri": "sqlite::memory:", "schema": "hot"});
    let db = db::create_db_pool(&db_conf).await.unwrap();
    match &db {
        DatabasePool::Sqlite(pool) => {
            let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
            // crates/hot_task_worker/ -> crates/ -> workspace root
            let migration_path = manifest_dir
                .parent()
                .unwrap()
                .parent()
                .unwrap()
                .join("resources/db/sqlite/migrations");
            let migrator = sqlx::migrate::Migrator::new(migration_path)
                .await
                .expect("Failed to create migrator");
            migrator.run(pool).await.expect("Failed to run migrations");
        }
        _ => panic!("Expected SQLite database for tests"),
    }
    db
}

async fn make_test_env() -> (
    DatabasePool,
    db::TestData,
    Arc<ProcessingQueue<TaskRequest>>,
    Arc<StreamPubSub>,
) {
    let db = create_test_db().await;
    let test_data = db::insert_test_data(&db).await.unwrap();

    // Memory queues are looked up via a global registry keyed by name+type,
    // so each test needs a unique queue name to avoid cross-test pollution.
    let queue_name = format!("{{hot:task}}-{}", Uuid::now_v7());
    let queue = Arc::new(
        ProcessingQueue::<TaskRequest>::new(
            QueueType::Memory,
            queue_name,
            None,
            Serialization::Json,
        )
        .expect("memory queue construction"),
    );
    let pubsub = Arc::new(StreamPubSub::new(StreamPubSubType::Memory, None, false).unwrap());
    (db, test_data, queue, pubsub)
}

async fn insert_active_task(
    db: &DatabasePool,
    test_data: &db::TestData,
    options: Option<&serde_json::Value>,
) -> (Uuid, TaskRequest) {
    let task_id = Uuid::now_v7();
    Task::insert(
        db,
        &task_id,
        &test_data.env_id,
        &test_data.stream_id,
        &test_data.build_id,
        None,
        "::myapp/long-job",
        Some(&serde_json::json!({"input": "x"})),
        options,
        "code",
        300_000,
        Some(&test_data.user_id),
    )
    .await
    .expect("Task::insert failed");
    Task::mark_running(db, &task_id)
        .await
        .expect("Task::mark_running failed");

    let request = TaskRequest {
        task_id: task_id.to_string(),
        function_name: "::myapp/long-job".to_string(),
        args: serde_json::json!({"input": "x"}),
        stream_id: test_data.stream_id.to_string(),
        env_id: test_data.env_id.to_string(),
        build_id: test_data.build_id.to_string(),
        org_id: Some(test_data.org_id.to_string()),
        user_id: Some(test_data.user_id.to_string()),
        project_id: None,
        project_name: None,
        timeout_ms: 300_000,
        task_type: "code".to_string(),
        created_at_unix_ms: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64,
        origin_run_id: None,
    };
    (task_id, request)
}

fn register(
    coord: &TaskShutdownCoordinator,
    task_id: Uuid,
    request: TaskRequest,
) -> Arc<AtomicBool> {
    let cancel = Arc::new(AtomicBool::new(false));
    coord.register_task(ActiveTask {
        task_id,
        env_id: Uuid::parse_str(&request.env_id).unwrap(),
        stream_id: Uuid::parse_str(&request.stream_id).unwrap(),
        function_name: request.function_name.clone(),
        task_type: request.task_type.clone(),
        cancel_token: Some(Arc::clone(&cancel)),
        original_request: request,
    });
    cancel
}

// ---------------------------------------------------------------------------
// Basic lifecycle
// ---------------------------------------------------------------------------

#[tokio::test]
async fn empty_drain_unregisters_and_returns_promptly() {
    let (db, _td, queue, pubsub) = make_test_env().await;
    let coord = TaskShutdownCoordinator::with_drain_secs(60);

    assert!(!coord.is_shutting_down());

    let started = std::time::Instant::now();
    coord.initiate_shutdown(&db, &pubsub, &queue).await;
    let elapsed = started.elapsed();

    assert!(coord.is_shutting_down(), "is_shutting_down should flip");
    assert!(
        elapsed < Duration::from_secs(2),
        "empty drain should return immediately (took {:?})",
        elapsed,
    );
}

#[tokio::test]
async fn duplicate_initiate_shutdown_is_idempotent() {
    let (db, _td, queue, pubsub) = make_test_env().await;
    let coord = Arc::new(TaskShutdownCoordinator::with_drain_secs(2));

    let c1 = Arc::clone(&coord);
    let db1 = db.clone();
    let q1 = Arc::clone(&queue);
    let p1 = Arc::clone(&pubsub);
    let h1 = tokio::spawn(async move { c1.initiate_shutdown(&db1, &p1, &q1).await });

    tokio::time::sleep(Duration::from_millis(50)).await;

    let c2 = Arc::clone(&coord);
    let db2 = db.clone();
    let q2 = Arc::clone(&queue);
    let p2 = Arc::clone(&pubsub);
    let h2 = tokio::spawn(async move { c2.initiate_shutdown(&db2, &p2, &q2).await });

    h1.await.unwrap();
    h2.await.unwrap();
    assert!(coord.is_shutting_down());
}

#[tokio::test]
async fn drain_phase_exits_early_when_tasks_finish_naturally() {
    let (db, td, queue, pubsub) = make_test_env().await;
    let coord = Arc::new(TaskShutdownCoordinator::with_drain_secs(60));

    let (task_id, req) = insert_active_task(&db, &td, None).await;
    register(&coord, task_id, req);

    let c = Arc::clone(&coord);
    let unreg = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(200)).await;
        c.unregister_task(&task_id);
    });

    let started = std::time::Instant::now();
    coord.initiate_shutdown(&db, &pubsub, &queue).await;
    let elapsed = started.elapsed();
    unreg.await.unwrap();

    // Should exit shortly after the unregister, not wait the full 60s drain.
    assert!(
        elapsed < Duration::from_secs(3),
        "drain should exit early (took {:?})",
        elapsed,
    );

    // Original task should NOT have been finalized as failed — it was
    // unregistered cleanly during drain.
    let row = Task::get(&db, &task_id).await.unwrap();
    assert_ne!(
        row.task_status_id,
        TaskStatus::Failed.as_id(),
        "naturally-finished task should not be marked failed",
    );
}

// ---------------------------------------------------------------------------
// Cancel + finalize path
// ---------------------------------------------------------------------------

#[tokio::test]
async fn drain_timeout_signals_cancel_then_finalizes() {
    let (db, td, queue, pubsub) = make_test_env().await;
    // 1s drain so the test runs in ~5s instead of ~33s.
    let coord = TaskShutdownCoordinator::with_drain_secs(1);

    let (task_id, req) = insert_active_task(&db, &td, None).await;
    let cancel = register(&coord, task_id, req.clone());

    coord.initiate_shutdown(&db, &pubsub, &queue).await;

    assert!(
        cancel.load(Ordering::Relaxed),
        "cancel_token should be signalled after drain timeout",
    );

    let row = Task::get(&db, &task_id).await.unwrap();
    assert_eq!(
        row.task_status_id,
        TaskStatus::Failed.as_id(),
        "interrupted task should be marked failed",
    );

    // Result payload should carry the infra_interrupted marker.
    let result = row.result.expect("failure result should be set");
    let val = result.get("$val").expect("result should be tagged Failure");
    assert_eq!(
        val.get("infra_interrupted").and_then(|v| v.as_bool()),
        Some(true),
        "failure payload should carry infra_interrupted",
    );

    // A retry copy should have been enqueued.
    let retry = queue
        .dequeue()
        .await
        .expect("dequeue must succeed")
        .expect("retry should be enqueued");
    assert_ne!(
        retry.task_id, req.task_id,
        "retry should have a fresh task_id",
    );
    assert_eq!(retry.function_name, req.function_name);
    assert_eq!(retry.args, req.args);
    assert_eq!(retry.env_id, req.env_id);
    assert_eq!(retry.stream_id, req.stream_id);

    // The retry row should exist in the DB with a fresh task_id, queued
    // status, and the same function_name as the original. We don't bump
    // retry_attempt for infra-interrupts (it's not the user's retry).
    let new_id = Uuid::parse_str(&retry.task_id).unwrap();
    let retry_row = Task::get(&db, &new_id).await.expect("retry row exists");
    assert_eq!(retry_row.task_status_id, TaskStatus::Queued.as_id());
    assert_eq!(retry_row.function_name, "::myapp/long-job");
    assert_eq!(
        retry_row.retry_attempt, row.retry_attempt,
        "infra-retry should NOT bump the user's retry_attempt counter",
    );
}

#[tokio::test]
async fn infra_retry_bypasses_user_max_retries_zero() {
    // User explicitly set retry: 0 (no user retries). Infra-retry should
    // STILL fire — it's an infra interrupt, not a user-error retry.
    let (db, td, queue, pubsub) = make_test_env().await;
    let coord = TaskShutdownCoordinator::with_drain_secs(1);

    let opts = serde_json::json!({"retry": 0});
    let (task_id, req) = insert_active_task(&db, &td, Some(&opts)).await;
    register(&coord, task_id, req);

    coord.initiate_shutdown(&db, &pubsub, &queue).await;

    let retry = queue.dequeue().await.unwrap();
    assert!(
        retry.is_some(),
        "infra-retry should fire even when user set retry: 0",
    );

    let row = Task::get(&db, &task_id).await.unwrap();
    assert_eq!(row.task_status_id, TaskStatus::Failed.as_id());
}

#[tokio::test]
async fn container_tasks_get_finalized_too() {
    // Earlier versions of the coordinator left container tasks running.
    // Now we treat them the same as code tasks because the host VM is
    // about to disappear.
    let (db, td, queue, pubsub) = make_test_env().await;
    let coord = TaskShutdownCoordinator::with_drain_secs(1);

    let task_id = Uuid::now_v7();
    Task::insert(
        &db,
        &task_id,
        &td.env_id,
        &td.stream_id,
        &td.build_id,
        None,
        "::hot::box/start",
        Some(&serde_json::json!({"image": "alpine"})),
        None,
        "container",
        600_000,
        Some(&td.user_id),
    )
    .await
    .unwrap();
    Task::mark_running(&db, &task_id).await.unwrap();

    let request = TaskRequest {
        task_id: task_id.to_string(),
        function_name: "::hot::box/start".to_string(),
        args: serde_json::json!({"image": "alpine"}),
        stream_id: td.stream_id.to_string(),
        env_id: td.env_id.to_string(),
        build_id: td.build_id.to_string(),
        org_id: None,
        user_id: None,
        project_id: None,
        project_name: None,
        timeout_ms: 600_000,
        task_type: "container".to_string(),
        created_at_unix_ms: 0,
        origin_run_id: None,
    };
    let cancel = Arc::new(AtomicBool::new(false));
    coord.register_task(ActiveTask {
        task_id,
        env_id: td.env_id,
        stream_id: td.stream_id,
        function_name: "::hot::box/start".to_string(),
        task_type: "container".to_string(),
        cancel_token: Some(Arc::clone(&cancel)),
        original_request: request,
    });

    coord.initiate_shutdown(&db, &pubsub, &queue).await;

    assert!(
        cancel.load(Ordering::Relaxed),
        "container task got cancel signal"
    );
    let row = Task::get(&db, &task_id).await.unwrap();
    assert_eq!(
        row.task_status_id,
        TaskStatus::Failed.as_id(),
        "container task should be finalized as failed",
    );
    assert!(
        queue.dequeue().await.unwrap().is_some(),
        "container task should be re-enqueued for the next worker",
    );
}

// ---------------------------------------------------------------------------
// Defaults
// ---------------------------------------------------------------------------

#[test]
fn default_constructor_uses_production_drain() {
    // Just exercise the Default + new() paths so they don't bitrot.
    let _ = TaskShutdownCoordinator::new();
    let _ = TaskShutdownCoordinator::default();
}

//! Integration tests for the Hot Task system.
//!
//! Tests the full lifecycle: DB insert -> queue -> worker processes -> DB complete,
//! as well as send/receive via StreamPubSub and TaskMessage events.

use hot::data::serialization::Serialization;
use hot::db::{self, DatabasePool, Task, TaskStatus};
use hot::lang::hot::task::TaskRequest;
use hot::queue::{ProcessingQueue, Queue, QueueProcessor, QueueType};
use hot::stream::{
    StreamEvent, StreamNext, StreamPubSub, StreamPubSubType, StreamPublisher,
    StreamSubscriberFactory,
};
use hot::val;
use std::sync::Arc;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Test helpers
// ---------------------------------------------------------------------------

async fn create_test_db() -> DatabasePool {
    let db_conf = val!({
        "uri": "sqlite::memory:",
        "schema": "hot"
    });

    let db = db::create_db_pool(&db_conf).await.unwrap();

    match &db {
        DatabasePool::Sqlite(pool) => {
            let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
            // crates/hot/ -> crates/ -> workspace root
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

async fn create_test_db_with_data() -> (DatabasePool, db::TestData) {
    let db = create_test_db().await;
    let test_data = db::insert_test_data(&db).await.unwrap();
    (db, test_data)
}

fn make_task_request(test_data: &db::TestData) -> (Uuid, TaskRequest) {
    let task_id = Uuid::now_v7();
    let request = TaskRequest {
        task_id: task_id.to_string(),
        function_name: "::myapp/background-job".to_string(),
        args: serde_json::json!({"input": "test-data"}),
        stream_id: test_data.stream_id.to_string(),
        env_id: test_data.env_id.to_string(),
        build_id: test_data.build_id.to_string(),
        org_id: Some(test_data.org_id.to_string()),
        user_id: Some(test_data.user_id.to_string()),
        project_id: None,
        project_name: Some("test-project".to_string()),
        timeout_ms: 60_000,
        task_type: "code".to_string(),
        created_at_unix_ms: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64,
        origin_run_id: None,
    };
    (task_id, request)
}

// ---------------------------------------------------------------------------
// Task DB lifecycle
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_task_full_db_lifecycle() {
    let (db, td) = create_test_db_with_data().await;
    let (task_id, _) = make_task_request(&td);

    // Insert
    Task::insert(
        &db,
        &task_id,
        &td.env_id,
        &td.stream_id,
        &td.build_id,
        Some(&td.run_id),
        "::app/worker",
        Some(&serde_json::json!({"x": 1})),
        None,
        "code",
        300_000,
        Some(&td.user_id),
    )
    .await
    .unwrap();

    let task = Task::get(&db, &task_id).await.unwrap();
    assert_eq!(task.task_status_id, TaskStatus::Queued.as_id());

    // Mark running
    Task::mark_running(&db, &task_id).await.unwrap();
    let task = Task::get(&db, &task_id).await.unwrap();
    assert_eq!(task.task_status_id, TaskStatus::Running.as_id());

    // Complete
    let result = serde_json::json!({"output": "done"});
    Task::complete(&db, &task_id, &TaskStatus::Completed, Some(&result))
        .await
        .unwrap();
    let task = Task::get(&db, &task_id).await.unwrap();
    assert_eq!(task.task_status_id, TaskStatus::Completed.as_id());
    assert!(task.duration_ms.is_some());
    assert!(task.stop_time.is_some());
}

#[tokio::test]
async fn test_task_failure_lifecycle() {
    let (db, td) = create_test_db_with_data().await;
    let (task_id, _) = make_task_request(&td);

    Task::insert(
        &db,
        &task_id,
        &td.env_id,
        &td.stream_id,
        &td.build_id,
        Some(&td.run_id),
        "::app/failing",
        None,
        None,
        "code",
        60_000,
        None,
    )
    .await
    .unwrap();

    Task::mark_running(&db, &task_id).await.unwrap();

    let err = serde_json::json!({"error": "something broke"});
    Task::complete(&db, &task_id, &TaskStatus::Failed, Some(&err))
        .await
        .unwrap();

    let task = Task::get(&db, &task_id).await.unwrap();
    assert_eq!(task.task_status_id, TaskStatus::Failed.as_id());
}

#[tokio::test]
async fn test_task_timeout_lifecycle() {
    let (db, td) = create_test_db_with_data().await;
    let (task_id, _) = make_task_request(&td);

    Task::insert(
        &db,
        &task_id,
        &td.env_id,
        &td.stream_id,
        &td.build_id,
        Some(&td.run_id),
        "::app/slow",
        None,
        None,
        "code",
        1000,
        None,
    )
    .await
    .unwrap();

    Task::mark_running(&db, &task_id).await.unwrap();

    let err = serde_json::json!({"error": "Task timed out"});
    Task::complete(&db, &task_id, &TaskStatus::TimedOut, Some(&err))
        .await
        .unwrap();

    let task = Task::get(&db, &task_id).await.unwrap();
    assert_eq!(task.task_status_id, TaskStatus::TimedOut.as_id());
}

// ---------------------------------------------------------------------------
// TaskRequest queue round-trip
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_task_request_queue_enqueue_dequeue() {
    let queue = ProcessingQueue::<TaskRequest>::new_with_cluster(
        QueueType::Memory,
        "test:task".to_string(),
        None,
        false,
        Serialization::Json,
    )
    .unwrap();

    let task_id = Uuid::now_v7();
    let request = TaskRequest {
        task_id: task_id.to_string(),
        function_name: "::ns/handler".to_string(),
        args: serde_json::json!({"key": "value"}),
        stream_id: Uuid::now_v7().to_string(),
        env_id: Uuid::now_v7().to_string(),
        build_id: Uuid::now_v7().to_string(),
        org_id: None,
        user_id: None,
        project_id: None,
        project_name: None,
        timeout_ms: 300_000,
        task_type: "code".to_string(),
        created_at_unix_ms: 1700000000000,
        origin_run_id: None,
    };

    queue.enqueue(request.clone()).await.unwrap();

    let dequeued = queue
        .dequeue_and_work(|msg: TaskRequest| async move {
            assert_eq!(msg.task_id, task_id.to_string());
            assert_eq!(msg.function_name, "::ns/handler");
            assert_eq!(msg.args["key"], "value");
            assert_eq!(msg.timeout_ms, 300_000);
            Ok::<_, Box<dyn std::error::Error + Send + Sync>>(msg)
        })
        .await
        .unwrap();

    assert!(dequeued.is_some());
    let msg = dequeued.unwrap();
    assert_eq!(msg.task_type, "code");
}

#[tokio::test]
async fn test_task_queue_empty_returns_none() {
    let queue = ProcessingQueue::<TaskRequest>::new_with_cluster(
        QueueType::Memory,
        "test:task:empty".to_string(),
        None,
        false,
        Serialization::Json,
    )
    .unwrap();

    let result = queue
        .dequeue_and_work(|_msg: TaskRequest| async {
            panic!("Should not be called");
            #[allow(unreachable_code)]
            Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
        })
        .await
        .unwrap();

    assert!(result.is_none());
}

// ---------------------------------------------------------------------------
// StreamEvent::TaskMessage pub/sub round-trip
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_task_message_pubsub_round_trip() {
    let pubsub = Arc::new(StreamPubSub::new(StreamPubSubType::Memory, None, false).unwrap());

    let task_id = Uuid::now_v7();

    // Subscribe before publishing
    let mut sub = pubsub.subscribe(task_id).await.unwrap();

    // Publish a TaskMessage
    let event = StreamEvent::TaskMessage {
        task_id: task_id.to_string(),
        payload: serde_json::json!({"command": "process", "data": [1, 2, 3]}),
    };
    pubsub.publish(event).await.unwrap();

    // Receive from subscription
    let received = tokio::time::timeout(std::time::Duration::from_secs(2), sub.next())
        .await
        .expect("Timed out waiting for message");

    match received {
        StreamNext::Event(StreamEvent::TaskMessage {
            task_id: tid,
            payload,
        }) => {
            assert_eq!(tid, task_id.to_string());
            assert_eq!(payload["command"], "process");
            assert_eq!(payload["data"], serde_json::json!([1, 2, 3]));
        }
        other => panic!("Expected TaskMessage, got: {:?}", other),
    }
}

#[tokio::test]
async fn test_task_message_multiple_messages() {
    let pubsub = Arc::new(StreamPubSub::new(StreamPubSubType::Memory, None, false).unwrap());

    let task_id = Uuid::now_v7();
    let mut sub = pubsub.subscribe(task_id).await.unwrap();

    for i in 0..3 {
        let event = StreamEvent::TaskMessage {
            task_id: task_id.to_string(),
            payload: serde_json::json!({"seq": i}),
        };
        pubsub.publish(event).await.unwrap();
    }

    for i in 0..3 {
        let msg = tokio::time::timeout(std::time::Duration::from_secs(2), sub.next())
            .await
            .expect("Timed out");

        match msg {
            StreamNext::Event(StreamEvent::TaskMessage { payload, .. }) => {
                assert_eq!(payload["seq"], i);
            }
            other => panic!("Expected TaskMessage seq={}, got: {:?}", i, other),
        }
    }
}

// ---------------------------------------------------------------------------
// DB queries: get_by_stream, get_by_env
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_get_tasks_by_stream() {
    let (db, td) = create_test_db_with_data().await;

    for i in 0..3 {
        let task_id = Uuid::now_v7();
        Task::insert(
            &db,
            &task_id,
            &td.env_id,
            &td.stream_id,
            &td.build_id,
            Some(&td.run_id),
            &format!("::app/task-{}", i),
            None,
            None,
            "code",
            60_000,
            None,
        )
        .await
        .unwrap();
    }

    let tasks = Task::get_by_stream(&db, &td.stream_id, &td.env_id, Some(10))
        .await
        .unwrap();
    assert_eq!(tasks.len(), 3);
}

#[tokio::test]
async fn test_get_tasks_by_env() {
    let (db, td) = create_test_db_with_data().await;

    for i in 0..2 {
        let task_id = Uuid::now_v7();
        Task::insert(
            &db,
            &task_id,
            &td.env_id,
            &td.stream_id,
            &td.build_id,
            Some(&td.run_id),
            &format!("::app/env-task-{}", i),
            None,
            None,
            "code",
            60_000,
            None,
        )
        .await
        .unwrap();
    }

    let tasks = Task::get_by_env(&db, &td.env_id, Some(10), None)
        .await
        .unwrap();
    assert_eq!(tasks.len(), 2);
}

// ---------------------------------------------------------------------------
// TaskRequest serialization integrity across queue boundaries
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_task_request_preserves_all_fields_through_queue() {
    let queue = ProcessingQueue::<TaskRequest>::new_with_cluster(
        QueueType::Memory,
        "test:task:fields".to_string(),
        None,
        false,
        Serialization::Json,
    )
    .unwrap();

    let original = TaskRequest {
        task_id: "019506ab-1234-7000-8000-aaaaaaaaaaaa".to_string(),
        function_name: "::myapp::workers/heavy-compute".to_string(),
        args: serde_json::json!({
            "nested": {"deep": true},
            "array": [1, 2, 3],
            "null_field": null
        }),
        stream_id: "019506ab-1234-7000-8000-bbbbbbbbbbbb".to_string(),
        env_id: "019506ab-1234-7000-8000-cccccccccccc".to_string(),
        build_id: "019506ab-1234-7000-8000-dddddddddddd".to_string(),
        org_id: Some("019506ab-1234-7000-8000-eeeeeeeeeeee".to_string()),
        user_id: Some("019506ab-1234-7000-8000-ffffffffffff".to_string()),
        project_id: Some("019506ab-5678-7000-8000-111111111111".to_string()),
        project_name: Some("my-project".to_string()),
        timeout_ms: 3_600_000,
        task_type: "container".to_string(),
        created_at_unix_ms: 1700000000000,
        origin_run_id: None,
    };

    queue.enqueue(original.clone()).await.unwrap();

    let result = queue
        .dequeue_and_work(|msg: TaskRequest| async move {
            assert_eq!(msg.task_id, "019506ab-1234-7000-8000-aaaaaaaaaaaa");
            assert_eq!(msg.function_name, "::myapp::workers/heavy-compute");
            assert_eq!(msg.args["nested"]["deep"], true);
            assert_eq!(msg.args["array"], serde_json::json!([1, 2, 3]));
            assert!(msg.args["null_field"].is_null());
            assert_eq!(msg.stream_id, "019506ab-1234-7000-8000-bbbbbbbbbbbb");
            assert_eq!(
                msg.org_id,
                Some("019506ab-1234-7000-8000-eeeeeeeeeeee".to_string())
            );
            assert_eq!(
                msg.user_id,
                Some("019506ab-1234-7000-8000-ffffffffffff".to_string())
            );
            assert_eq!(
                msg.project_id,
                Some("019506ab-5678-7000-8000-111111111111".to_string())
            );
            assert_eq!(msg.project_name, Some("my-project".to_string()));
            assert_eq!(msg.timeout_ms, 3_600_000);
            assert_eq!(msg.task_type, "container");
            assert_eq!(msg.created_at_unix_ms, 1700000000000);
            Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
        })
        .await
        .unwrap();

    assert!(result.is_some());
}

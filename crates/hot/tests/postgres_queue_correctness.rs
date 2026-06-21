//! Optional Postgres-backed smoke tests for queue-related durable state.
//!
//! These tests are skipped unless `HOT_TEST_POSTGRES_URI` is set. They are
//! intended for local verification with the same URI used by `pg.redis.test.hot`,
//! e.g. `postgres://hot:hot@127.0.0.1:55432/hot`.

use hot::data::serialization::Serialization;
use hot::db::{self, DatabasePool, Task, TaskStatus};
use hot::lang::hot::task::TaskRequest;
use hot::queue::{ProcessingQueue, Queue, QueueProcessor, QueueType};
use hot::val;
use sqlx::Executor;
use std::error::Error;
use uuid::Uuid;

async fn reset_schema_if_requested(uri: &str, schema: &str) {
    if std::env::var("HOT_TEST_POSTGRES_RESET_SCHEMA").as_deref() != Ok("1") {
        return;
    }

    assert!(
        schema
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_'),
        "test schema name must be identifier-safe"
    );

    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(uri)
        .await
        .expect("reset pool should connect");
    pool.execute(sqlx::AssertSqlSafe(format!(
        "drop schema if exists {} cascade",
        schema
    )))
    .await
    .expect("test schema should reset");
    pool.close().await;
}

async fn postgres_db() -> Option<(DatabasePool, String)> {
    let uri = match std::env::var("HOT_TEST_POSTGRES_URI") {
        Ok(uri) => uri,
        Err(_) => {
            eprintln!("skipping: HOT_TEST_POSTGRES_URI is not set");
            return None;
        }
    };

    let schema = std::env::var("HOT_TEST_POSTGRES_SCHEMA").unwrap_or_else(|_| "hot".to_string());
    reset_schema_if_requested(&uri, &schema).await;

    let conf = val!({
        "uri": uri.clone(),
        "schema": schema.clone(),
    });

    db::run_migrations(&conf)
        .await
        .expect("Postgres migrations should run");
    let db = db::create_db_pool(&conf)
        .await
        .expect("Postgres pool should connect");

    Some((db, schema))
}

async fn drop_schema(db: &DatabasePool, schema: &str) {
    if std::env::var("HOT_TEST_POSTGRES_RESET_SCHEMA").as_deref() != Ok("1") {
        return;
    }

    if let DatabasePool::Postgres(pool) = db {
        let _ = pool
            .execute(sqlx::AssertSqlSafe(format!(
                "drop schema if exists {} cascade",
                schema
            )))
            .await;
    }
}

async fn redis_client_if_available() -> Option<redis::Client> {
    let uri = std::env::var("HOT_REDIS_URI")
        .or_else(|_| std::env::var("REDIS_URL"))
        .unwrap_or_else(|_| "redis://127.0.0.1:6379".to_string());
    let client = match redis::Client::open(uri.as_str()) {
        Ok(client) => client,
        Err(e) => {
            eprintln!("skipping Redis round-trip: Redis client failed to open: {e}");
            return None;
        }
    };

    match client.get_multiplexed_async_connection().await {
        Ok(mut conn) => {
            let pong: redis::RedisResult<String> = redis::cmd("PING").query_async(&mut conn).await;
            if pong.is_ok() {
                Some(client)
            } else {
                eprintln!("skipping Redis round-trip: Redis PING failed");
                None
            }
        }
        Err(e) => {
            eprintln!("skipping Redis round-trip: Redis unavailable: {e}");
            None
        }
    }
}

async fn cleanup_redis_queue(client: &redis::Client, queue_name: &str) {
    let stream_key = format!("{{{}}}", queue_name);
    let dlq_key = format!("{}:deadletter", stream_key);
    if let Ok(mut conn) = client.get_multiplexed_async_connection().await {
        let _: redis::RedisResult<()> = redis::cmd("DEL")
            .arg(&stream_key)
            .arg(&dlq_key)
            .query_async(&mut conn)
            .await;
    }
}

#[tokio::test]
async fn postgres_task_lifecycle_smoke() {
    let Some((db, schema)) = postgres_db().await else {
        return;
    };

    let test_data = db::insert_test_data(&db)
        .await
        .expect("test data should insert");
    let task_id = Uuid::now_v7();

    Task::insert(
        &db,
        &task_id,
        &test_data.env_id,
        &test_data.stream_id,
        &test_data.build_id,
        Some(&test_data.run_id),
        "::app/postgres-task",
        Some(&serde_json::json!({"input": "postgres"})),
        None,
        "code",
        300_000,
        Some(&test_data.user_id),
    )
    .await
    .expect("task row should insert");

    let task = Task::get(&db, &task_id)
        .await
        .expect("task row should be readable");
    assert_eq!(task.task_status_id, TaskStatus::Queued.as_id());

    Task::mark_running(&db, &task_id)
        .await
        .expect("task should mark running");
    let task = Task::get(&db, &task_id)
        .await
        .expect("task row should be readable after running");
    assert_eq!(task.task_status_id, TaskStatus::Running.as_id());

    let result = serde_json::json!({"ok": true});
    Task::complete(&db, &task_id, &TaskStatus::Completed, Some(&result))
        .await
        .expect("task should complete");
    let task = Task::get(&db, &task_id)
        .await
        .expect("task row should be readable after complete");
    assert_eq!(task.task_status_id, TaskStatus::Completed.as_id());
    assert!(task.stop_time.is_some());

    if let Some(redis_client) = redis_client_if_available().await {
        let redis_task_id = Uuid::now_v7();
        let queue_name = format!("hot:task:test-{}", Uuid::now_v7().simple());
        let queue = ProcessingQueue::<TaskRequest>::new(
            QueueType::Redis,
            queue_name.clone(),
            std::env::var("HOT_REDIS_URI")
                .or_else(|_| std::env::var("REDIS_URL"))
                .ok(),
            Serialization::Json,
        )
        .expect("Redis task queue should construct");

        Task::insert(
            &db,
            &redis_task_id,
            &test_data.env_id,
            &test_data.stream_id,
            &test_data.build_id,
            Some(&test_data.run_id),
            "::app/postgres-redis-task",
            Some(&serde_json::json!({"input": "postgres-redis"})),
            None,
            "code",
            300_000,
            Some(&test_data.user_id),
        )
        .await
        .expect("Redis round-trip task row should insert");

        queue
            .enqueue(TaskRequest {
                task_id: redis_task_id.to_string(),
                function_name: "::app/postgres-redis-task".to_string(),
                args: serde_json::json!({"input": "postgres-redis"}),
                stream_id: test_data.stream_id.to_string(),
                env_id: test_data.env_id.to_string(),
                build_id: test_data.build_id.to_string(),
                org_id: Some(test_data.org_id.to_string()),
                user_id: Some(test_data.user_id.to_string()),
                project_id: Some(test_data.project_id.to_string()),
                project_name: Some("postgres-redis-test".to_string()),
                timeout_ms: 300_000,
                task_type: "code".to_string(),
                created_at_unix_ms: chrono::Utc::now().timestamp_millis() as u64,
                origin_run_id: Some(test_data.run_id.to_string()),
            })
            .await
            .expect("task request should enqueue to Redis");

        let db_for_worker = db.clone();
        let processed = queue
            .dequeue_and_work(|request: TaskRequest| async move {
                assert_eq!(request.task_id, redis_task_id.to_string());
                Task::mark_running(&db_for_worker, &redis_task_id).await?;
                Task::complete(
                    &db_for_worker,
                    &redis_task_id,
                    &TaskStatus::Completed,
                    Some(&serde_json::json!({"ok": true, "backend": "redis"})),
                )
                .await?;
                Ok::<_, Box<dyn Error + Send + Sync>>(request.task_id)
            })
            .await
            .expect("Redis queue worker should process the task");

        assert_eq!(processed, Some(redis_task_id.to_string()));
        let task = Task::get(&db, &redis_task_id)
            .await
            .expect("Redis round-trip task row should be readable");
        assert_eq!(task.task_status_id, TaskStatus::Completed.as_id());
        assert!(task.stop_time.is_some());

        cleanup_redis_queue(&redis_client, &queue_name).await;
    }

    drop_schema(&db, &schema).await;
}

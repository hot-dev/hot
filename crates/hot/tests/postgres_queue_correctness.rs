//! Optional Postgres-backed smoke tests for queue-related durable state.
//!
//! These tests are skipped unless `HOT_TEST_POSTGRES_URI` is set. They are
//! intended for local verification with the same URI used by `pg.redis.test.hot`,
//! e.g. `postgres://hot:hot@127.0.0.1:55432/hot`.

use hot::data::serialization::Serialization;
use hot::db::{self, DatabasePool, InfraRetryFinalizeOutcome, Task, TaskStatus};
use hot::lang::emitter::{DatabaseEngineEventEmitter, EngineEvent, EngineEventEmitter};
use hot::lang::event::{ExecutionContext, QueueExecutionTiming};
use hot::lang::hot::task::TaskRequest;
use hot::queue::{ProcessingQueue, Queue, QueueProcessor, QueueType};
use hot::val;
use sqlx::Executor;
use std::error::Error;
use uuid::Uuid;

// The Postgres tests intentionally exercise destructive schema reset and
// teardown against one dedicated database. Rust runs tests in this binary in
// parallel by default, so serialize the tests that share that schema.
static POSTGRES_SCHEMA_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn external_schema_dependencies(pool: &sqlx::PgPool, schema: &str) -> Vec<String> {
    sqlx::query_scalar(
        "WITH dependency_objects AS ( \
             SELECT d.classid, d.objid, dependent.type, \
                    dependent.schema AS dependent_schema, dependent.identity, \
                    referenced.schema AS referenced_schema \
             FROM pg_depend AS d \
             CROSS JOIN LATERAL pg_identify_object(\
                 d.refclassid, d.refobjid, d.refobjsubid\
             ) AS referenced \
             CROSS JOIN LATERAL pg_identify_object(\
                 d.classid, d.objid, d.objsubid\
             ) AS dependent \
         ) \
         SELECT identity FROM dependency_objects \
          WHERE referenced_schema = $1 \
            AND dependent_schema IS NOT NULL \
            AND dependent_schema <> $1 \
            AND dependent_schema <> 'information_schema' \
            AND dependent_schema NOT LIKE 'pg_%' \
         UNION \
         SELECT dependency_objects.identity \
           FROM dependency_objects \
           JOIN pg_rewrite ON dependency_objects.classid = 'pg_rewrite'::regclass \
                          AND dependency_objects.objid = pg_rewrite.oid \
           JOIN pg_class owner_class ON owner_class.oid = pg_rewrite.ev_class \
           JOIN pg_namespace owner_namespace ON owner_namespace.oid = owner_class.relnamespace \
          WHERE dependency_objects.referenced_schema = $1 \
            AND owner_namespace.nspname <> $1 \
            AND owner_namespace.nspname NOT LIKE 'pg_%' \
         UNION \
         SELECT dependency_objects.identity \
           FROM dependency_objects \
           JOIN pg_attrdef ON dependency_objects.classid = 'pg_attrdef'::regclass \
                          AND dependency_objects.objid = pg_attrdef.oid \
           JOIN pg_class owner_class ON owner_class.oid = pg_attrdef.adrelid \
           JOIN pg_namespace owner_namespace ON owner_namespace.oid = owner_class.relnamespace \
          WHERE dependency_objects.referenced_schema = $1 \
            AND owner_namespace.nspname <> $1 \
            AND owner_namespace.nspname NOT LIKE 'pg_%' \
         UNION \
         SELECT dependency_objects.identity \
           FROM dependency_objects \
           JOIN pg_trigger ON dependency_objects.classid = 'pg_trigger'::regclass \
                          AND dependency_objects.objid = pg_trigger.oid \
           JOIN pg_class owner_class ON owner_class.oid = pg_trigger.tgrelid \
           JOIN pg_namespace owner_namespace ON owner_namespace.oid = owner_class.relnamespace \
          WHERE dependency_objects.referenced_schema = $1 \
            AND owner_namespace.nspname <> $1 \
            AND owner_namespace.nspname NOT LIKE 'pg_%' \
         UNION \
         SELECT dependency_objects.identity \
           FROM dependency_objects \
           JOIN pg_policy ON dependency_objects.classid = 'pg_policy'::regclass \
                         AND dependency_objects.objid = pg_policy.oid \
           JOIN pg_class owner_class ON owner_class.oid = pg_policy.polrelid \
           JOIN pg_namespace owner_namespace ON owner_namespace.oid = owner_class.relnamespace \
          WHERE dependency_objects.referenced_schema = $1 \
            AND owner_namespace.nspname <> $1 \
            AND owner_namespace.nspname NOT LIKE 'pg_%' \
         ORDER BY 1",
    )
    .bind(schema)
    .fetch_all(pool)
    .await
    .expect("cross-schema dependencies should be queryable")
}

async fn assert_schema_drop_isolated(pool: &sqlx::PgPool, schema: &str) {
    let dependencies = external_schema_dependencies(pool, schema).await;
    assert!(
        dependencies.is_empty(),
        "refusing to DROP SCHEMA `{schema}` CASCADE because objects outside the schema depend on it: {}",
        dependencies.join(", ")
    );
}

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
    // Refuse to run against a database where these extensions live outside the
    // test schema. Relocating them via `ALTER EXTENSION ... SET SCHEMA` would
    // make the `DROP SCHEMA ... CASCADE` below cascade-drop every dependent
    // object database-wide (e.g. vector columns on unrelated public tables).
    // Fresh hot-cloud Compose volumes install the extensions into `hot`. This
    // check MUST run before the drop so a shared database is refused instead
    // of destroyed.
    for extension in ["vector", "pg_trgm"] {
        let installed_schema: Option<String> = sqlx::query_scalar(
            "SELECT n.nspname FROM pg_extension e JOIN pg_namespace n ON n.oid = e.extnamespace WHERE e.extname = $1",
        )
        .bind(extension)
        .fetch_optional(&pool)
        .await
        .expect("extension location should be queryable");
        if let Some(installed) = installed_schema.as_deref().filter(|name| *name != schema) {
            panic!(
                "extension `{}` is installed in schema `{}`, not the test schema `{}`; \
                 this test needs a dedicated database (fresh hot-cloud Compose volumes install \
                 the extensions into the `hot` schema). Refusing to relocate a shared extension \
                 because teardown's DROP SCHEMA ... CASCADE would destroy dependent objects \
                 outside the test schema.",
                extension, installed, schema
            );
        }
    }

    // Check the complete catalog dependency graph, not just extension-owned
    // column types. Views, foreign keys, functions, and indexes using an
    // extension operator class can all live outside this schema yet be
    // cascade-dropped with it.
    assert_schema_drop_isolated(&pool, schema).await;

    pool.execute(sqlx::AssertSqlSafe(format!(
        "drop schema if exists {} cascade",
        schema
    )))
    .await
    .expect("test schema should reset");
    pool.execute(sqlx::AssertSqlSafe(format!(
        "create schema if not exists {}",
        schema
    )))
    .await
    .expect("test schema should be recreated");
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
    assert_eq!(
        schema, "hot",
        "Postgres migrations contain schema-qualified hot.* objects; isolate this test with a dedicated database, not a different schema"
    );
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
        assert_schema_drop_isolated(pool, schema).await;
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
    let _schema_guard = POSTGRES_SCHEMA_TEST_LOCK.lock().await;
    let Some((db, schema)) = postgres_db().await else {
        return;
    };

    let test_data = db::insert_test_data(&db)
        .await
        .expect("test data should insert");

    if let DatabasePool::Postgres(pool) = &db {
        let statement_timeout: String = sqlx::query_scalar("SHOW statement_timeout")
            .fetch_one(pool)
            .await
            .expect("statement timeout should be readable");
        let lock_timeout: String = sqlx::query_scalar("SHOW lock_timeout")
            .fetch_one(pool)
            .await
            .expect("lock timeout should be readable");
        assert_eq!(statement_timeout, "30s");
        assert_eq!(lock_timeout, "5s");
    }

    let cancelled_task_id = Uuid::now_v7();
    Task::insert(
        &db,
        &cancelled_task_id,
        &test_data.env_id,
        &test_data.stream_id,
        &test_data.build_id,
        Some(&test_data.run_id),
        "::app/cancel-race",
        None,
        None,
        "code",
        300_000,
        Some(&test_data.user_id),
    )
    .await
    .expect("cancel-race task should insert");
    assert!(Task::cancel(&db, &cancelled_task_id).await.unwrap());
    assert!(
        !Task::claim_for_worker(&db, &cancelled_task_id, "late-worker")
            .await
            .unwrap()
    );
    assert!(!Task::mark_running(&db, &cancelled_task_id).await.unwrap());
    assert!(
        !Task::complete(
            &db,
            &cancelled_task_id,
            &TaskStatus::Completed,
            None,
            None,
            None
        )
        .await
        .unwrap()
    );
    assert_eq!(
        Task::get(&db, &cancelled_task_id)
            .await
            .unwrap()
            .task_status_id,
        TaskStatus::Cancelled.as_id()
    );

    let mut execution_context = ExecutionContext::new(
        Uuid::now_v7(),
        test_data.stream_id,
        hot::db::run::RunType::Event.as_id(),
        Some(test_data.env_id),
        Some(test_data.user_id),
        Some(test_data.org_id),
        Some(test_data.build_id),
    );
    let claimed_at = chrono::Utc::now();
    execution_context.queue_timing = Some(QueueExecutionTiming {
        backend: "redis".to_string(),
        enqueued_at: Some(claimed_at - chrono::Duration::milliseconds(4)),
        claimed_at,
        queue_wait_us: 4_000,
        redelivered: false,
        handler_dispatched_at: Some(claimed_at),
    });
    let emitter = DatabaseEngineEventEmitter::new_with_pool(db.clone());
    emitter.emit(EngineEvent::run_start(&execution_context));
    emitter.emit(EngineEvent::run_stop(
        &execution_context,
        hot::val::Val::Null,
    ));
    emitter
        .shutdown()
        .await
        .expect("Postgres emitter should flush timing info");
    let timing_run = hot::db::run::Run::get_run(&db, &execution_context.run_id)
        .await
        .expect("timed run should be readable");
    assert_eq!(
        timing_run.info.as_ref().unwrap()["queue_timing"]["queue_wait_us"],
        4_000
    );

    hot::db::run::Run::update_info(
        &db,
        &execution_context.run_id,
        Some(&serde_json::json!({"warning": "route:dup"})),
    )
    .await
    .expect("diagnostics should merge with timing info");
    let timing_run = hot::db::run::Run::get_run(&db, &execution_context.run_id)
        .await
        .expect("merged run should be readable");
    let info = timing_run.info.unwrap();
    assert_eq!(info["queue_timing"]["queue_wait_us"], 4_000);
    assert_eq!(info["warning"], "route:dup");

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

    Task::set_worker(&db, &task_id, "postgres-test-worker")
        .await
        .expect("task worker ownership should persist");
    assert!(
        Task::release_worker(&db, &task_id, "postgres-test-worker")
            .await
            .expect("unfinished task ownership should release"),
        "release_worker with the owning worker id should report a released row"
    );
    let released = Task::get(&db, &task_id)
        .await
        .expect("released task should remain readable");
    assert!(released.worker_id.is_none());
    assert!(
        Task::find_zombie_tasks(&db, 30)
            .await
            .expect("released task should be eligible for reconciliation")
            .iter()
            .any(|candidate| candidate.task_id == task_id)
    );

    let result = serde_json::json!({"ok": true});
    Task::complete(
        &db,
        &task_id,
        &TaskStatus::Completed,
        Some(&result),
        None,
        None,
    )
    .await
    .expect("task should complete");
    let task = Task::get(&db, &task_id)
        .await
        .expect("task row should be readable after complete");
    assert_eq!(task.task_status_id, TaskStatus::Completed.as_id());
    assert!(task.stop_time.is_some());
    assert!(
        task.duration_ms.is_some(),
        "without an override the Postgres arm must still compute stop-start duration"
    );

    // Postgres arm of the `duration_ms_override` COALESCE: an explicitly
    // measured billable duration must be persisted verbatim instead of the
    // claim-to-persist stop-start computation (container tasks rely on this
    // so the task-minutes quota and re-read event duration exclude worker
    // setup/teardown).
    let override_task_id = Uuid::now_v7();
    Task::insert(
        &db,
        &override_task_id,
        &test_data.env_id,
        &test_data.stream_id,
        &test_data.build_id,
        Some(&test_data.run_id),
        "::app/duration-override",
        None,
        None,
        "container",
        300_000,
        Some(&test_data.user_id),
    )
    .await
    .expect("override task should insert");
    assert!(Task::mark_running(&db, &override_task_id).await.unwrap());
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    assert!(
        Task::complete(
            &db,
            &override_task_id,
            &TaskStatus::Completed,
            Some(&serde_json::json!({"exit-code": 0})),
            Some(543_210),
            None,
        )
        .await
        .expect("override task should complete")
    );
    let override_task = Task::get(&db, &override_task_id)
        .await
        .expect("override task row should be readable");
    assert_eq!(
        override_task.duration_ms,
        Some(543_210),
        "duration_ms_override must be persisted verbatim on the Postgres arm"
    );

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
                    None,
                    None,
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

    if let DatabasePool::Postgres(pool) = &db {
        pool.execute("CREATE VIEW public.hot_reset_guard_view AS SELECT task_id FROM hot.task")
            .await
            .expect("cross-schema probe view should create");
        pool.execute("CREATE TABLE public.hot_reset_guard_text (value text)")
            .await
            .expect("cross-schema probe table should create");
        pool.execute(
            "CREATE INDEX hot_reset_guard_trgm_idx ON public.hot_reset_guard_text \
             USING gin (value hot.gin_trgm_ops)",
        )
        .await
        .expect("cross-schema pg_trgm probe index should create");

        let dependencies = external_schema_dependencies(pool, &schema).await;
        assert!(
            dependencies
                .iter()
                .any(|identity| identity.contains("hot_reset_guard_view")),
            "the reset guard must detect a public view depending on hot: {dependencies:?}"
        );
        assert!(
            dependencies
                .iter()
                .any(|identity| identity.contains("hot_reset_guard_trgm_idx")),
            "the reset guard must detect a public index using hot.gin_trgm_ops: {dependencies:?}"
        );

        pool.execute("DROP VIEW public.hot_reset_guard_view")
            .await
            .expect("probe view should drop");
        pool.execute("DROP TABLE public.hot_reset_guard_text")
            .await
            .expect("probe table should drop");
    }

    drop_schema(&db, &schema).await;
}

#[tokio::test]
async fn postgres_shutdown_finalize_is_atomic_with_retry_child() {
    let _schema_guard = POSTGRES_SCHEMA_TEST_LOCK.lock().await;
    let Some((db, schema)) = postgres_db().await else {
        return;
    };
    let test_data = db::insert_test_data(&db)
        .await
        .expect("test data should insert");
    let retry_error = serde_json::json!({"msg": "retry durable"});
    let exhausted_error = serde_json::json!({"msg": "retry exhausted"});

    let task_id = Uuid::now_v7();
    Task::insert(
        &db,
        &task_id,
        &test_data.env_id,
        &test_data.stream_id,
        &test_data.build_id,
        None,
        "::test/atomic-finalize",
        None,
        None,
        "code",
        60_000,
        Some(&test_data.user_id),
    )
    .await
    .unwrap();
    assert!(Task::mark_running(&db, &task_id).await.unwrap());
    let child_id = Uuid::now_v7();
    let outcome = Task::finalize_with_infra_retry(
        &db,
        &task_id,
        &child_id,
        3,
        &retry_error,
        &exhausted_error,
        chrono::Utc::now(),
    )
    .await
    .unwrap();
    assert!(matches!(
        outcome,
        InfraRetryFinalizeOutcome::RetryReady {
            retry_task_id,
            should_enqueue: true,
            ..
        } if retry_task_id == child_id
    ));
    assert_eq!(
        Task::get(&db, &task_id).await.unwrap().task_status_id,
        TaskStatus::Failed.as_id()
    );
    assert_eq!(
        Task::get(&db, &child_id).await.unwrap().parent_task_id,
        Some(task_id)
    );

    let rollback_id = Uuid::now_v7();
    Task::insert(
        &db,
        &rollback_id,
        &test_data.env_id,
        &test_data.stream_id,
        &test_data.build_id,
        None,
        "::test/atomic-rollback",
        None,
        None,
        "code",
        60_000,
        Some(&test_data.user_id),
    )
    .await
    .unwrap();
    assert!(Task::mark_running(&db, &rollback_id).await.unwrap());
    assert!(
        Task::finalize_with_infra_retry(
            &db,
            &rollback_id,
            &rollback_id,
            3,
            &retry_error,
            &exhausted_error,
            chrono::Utc::now(),
        )
        .await
        .is_err(),
        "retry primary-key failure must abort the transaction"
    );
    assert_eq!(
        Task::get(&db, &rollback_id).await.unwrap().task_status_id,
        TaskStatus::Running.as_id(),
        "terminal write must roll back with the failed child insert"
    );

    drop_schema(&db, &schema).await;
}

#[tokio::test]
async fn redis_success_path_rejects_an_ack_lost_before_commit() {
    let Some(client) = redis_client_if_available().await else {
        return;
    };
    let queue_name = format!("hot:ack-loss:test-{}", Uuid::now_v7().simple());
    let stream_key = format!("{{{}}}", queue_name);
    let queue = ProcessingQueue::<String>::new(
        QueueType::Redis,
        queue_name.clone(),
        std::env::var("HOT_REDIS_URI")
            .or_else(|_| std::env::var("REDIS_URL"))
            .ok(),
        Serialization::Json,
    )
    .expect("Redis queue should construct");
    queue
        .enqueue("ack-loss".to_string())
        .await
        .expect("test message should enqueue");

    let client_for_worker = client.clone();
    let stream_for_worker = stream_key.clone();
    let result = queue
        .dequeue_and_work(move |message| async move {
            assert_eq!(message, "ack-loss");
            let mut conn = client_for_worker.get_multiplexed_async_connection().await?;
            let pending: redis::Value = redis::cmd("XPENDING")
                .arg(&stream_for_worker)
                .arg("hot-workers")
                .arg("-")
                .arg("+")
                .arg(1)
                .query_async(&mut conn)
                .await?;
            let entries: Vec<Vec<redis::Value>> =
                redis::from_redis_value_ref(&pending).unwrap_or_default();
            let message_id: String = entries
                .first()
                .and_then(|entry| entry.first())
                .and_then(|value| redis::from_redis_value_ref(value).ok())
                .expect("worker delivery should be present in the Redis PEL");
            let pre_acked: i64 = redis::cmd("XACK")
                .arg(&stream_for_worker)
                .arg("hot-workers")
                .arg(&message_id)
                .query_async(&mut conn)
                .await?;
            assert_eq!(pre_acked, 1, "test must consume the pending ACK first");

            Ok::<_, Box<dyn Error + Send + Sync>>(())
        })
        .await;

    let err = result.expect_err("the queue must reject a success ACK count of zero");
    assert!(err.to_string().contains("affected 0 entries"));
    cleanup_redis_queue(&client, &queue_name).await;
}

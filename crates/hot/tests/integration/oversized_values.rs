//! A value that other code reads back must survive PostgreSQL's 256 MB limit on
//! a `jsonb` container, or its writer must get an Err. It can't be left out, as
//! a record's value can. Needs `HOT_TEST_POSTGRES_URI`.

use hot::blob::{BlobConfig, BlobMode, BlobScope, BlobStore, SpillSource};
use hot::data::serialization::Serialization;
use hot::db::{DatabasePool, DatabaseScope, Task, TaskStatus, TestData};
use hot::lang::bytecode::BytecodeProgram;
use hot::lang::compiler::core_registry::CoreVariableRegistry;
use hot::lang::event::queue::QueueEventPublisher;
use hot::lang::event::{DatabaseEventPublisher, ExecutionContext, QueueAndDatabaseEventPublisher};
use hot::lang::hot::r#type::HotResult;
use hot::lang::runtime::vm::VirtualMachine;
use hot::queue::{ProcessingQueue, QueueType};
use hot::val::Val;
use std::sync::Arc;
use tempfile::TempDir;
use uuid::Uuid;

/// These hold gigabytes in memory, so they take turns.
static ONE_AT_A_TIME: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct Database {
    db: DatabasePool,
    raw: sqlx::PgPool,
    fixture: TestData,
}

/// Its own database, so that nothing else's rows or schema reset can interfere.
async fn database() -> Option<Database> {
    let Ok(admin_uri) = std::env::var("HOT_TEST_POSTGRES_URI") else {
        eprintln!("skipping: HOT_TEST_POSTGRES_URI not set");
        return None;
    };
    let admin = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(&admin_uri)
        .await
        .expect("connect to the fixture database");
    let name = format!("hot_oversized_values_{}", Uuid::now_v7().simple());
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE DATABASE {name}")))
        .execute(&admin)
        .await
        .expect("create the test database");
    let (server, _) = admin_uri.rsplit_once('/').expect("a database in the URI");
    let uri = format!("{server}/{name}");
    let conf = hot::val!({"uri": uri.clone(), "rls": {"enforce": false}});
    hot::db::run_migrations(&conf).await.expect("migrate");
    let db = hot::db::create_db_pool(&conf).await.expect("connect");
    let fixture = hot::db::insert_test_data(&db).await.expect("fixture");
    let raw = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(&uri)
        .await
        .expect("connect to the test database");
    Some(Database { db, raw, fixture })
}

fn blob_store(database: &Database) -> (Arc<BlobStore>, TempDir) {
    let storage_dir = tempfile::tempdir().unwrap();
    let store = Arc::new(BlobStore::new(
        Arc::new(database.db.clone()),
        Arc::new(hot::file_storage::LocalFileStorage::new(
            storage_dir.path().to_path_buf(),
        )),
        BlobConfig {
            mode: BlobMode::Service,
            ..BlobConfig::default()
        },
    ));
    (store, storage_dir)
}

/// Config that gives natives, which build their own store, the same storage.
fn conf_with_blobs(storage_dir: &TempDir) -> Val {
    let path = storage_dir.path().to_str().unwrap();
    hot::val!({
        "blob": {"mode": "service"},
        "file": {"storage": {"type": "local", "path": path}},
    })
}

fn context(fixture: &TestData) -> ExecutionContext {
    ExecutionContext::new(
        fixture.run_id,
        fixture.stream_id,
        hot::db::run::RunType::Run.as_id(),
        Some(fixture.env_id),
        Some(fixture.user_id),
        Some(fixture.org_id),
        Some(fixture.build_id),
    )
}

fn scope(fixture: &TestData) -> BlobScope {
    BlobScope {
        org_id: fixture.org_id,
        env_id: Some(fixture.env_id),
        run_id: None,
    }
}

/// `jsonb` takes about six times the space of `0,`, the worst case for the
/// limit: this is past it, at under 50 MB of JSON.
fn over_the_limit() -> Val {
    Val::vec(vec![Val::Int(0); 24_000_000])
}

/// What PostgreSQL says about a value too large for `jsonb`, depending on its
/// shape, rather than a test's setup going wrong.
fn refused_for_its_size(error: &str) -> bool {
    error.contains("exceeds the maximum") || error.contains("invalid memory alloc")
}

/// A worker's VM, to build on the thread that uses it. Natives block on async
/// work, so that's a blocking thread.
fn vm(db: DatabasePool, context: ExecutionContext, conf: Option<Val>) -> VirtualMachine {
    let mut vm = VirtualMachine::new(
        Arc::new(BytecodeProgram::new()),
        None,
        Arc::new(indexmap::IndexMap::new()),
        Arc::new(indexmap::IndexMap::new()),
        Arc::new(indexmap::IndexMap::new()),
        Arc::new(CoreVariableRegistry::new()),
        conf,
    );
    vm.set_database_pool(Arc::new(db));
    vm.set_execution_context(context);
    vm
}

/// `::hot::event/send`, as a worker's VM calls it, publishing with `publisher`.
async fn send(
    database: &Database,
    publisher: QueueAndDatabaseEventPublisher,
    data: Val,
) -> HotResult<Val> {
    let (db, context) = (database.db.clone(), context(&database.fixture));
    tokio::task::spawn_blocking(move || {
        let mut vm = vm(db, context, None);
        vm.set_event_publisher(Arc::new(publisher));
        hot::lang::hot::event::send_event(&mut vm, &[Val::from("big:event"), data])
    })
    .await
    .unwrap()
}

fn publisher(
    database: &Database,
    blob_store: Option<Arc<BlobStore>>,
) -> QueueAndDatabaseEventPublisher {
    let queue = QueueEventPublisher::new(
        QueueType::Memory,
        "hot:event".to_string(),
        None,
        Serialization::Json,
    )
    .unwrap();
    QueueAndDatabaseEventPublisher::new(
        queue,
        DatabaseEventPublisher::new_with_pool_and_blob_store(database.db.clone(), blob_store),
    )
}

async fn stored_events(database: &Database) -> Vec<String> {
    sqlx::query_scalar("SELECT event_data::text FROM hot.event WHERE event_type = 'big:event'")
        .fetch_all(&database.raw)
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn sent_data_past_the_limit_is_stored_and_reads_back() {
    let _turn = ONE_AT_A_TIME.lock().await;
    let Some(database) = database().await else {
        return;
    };
    let (store, _dir) = blob_store(&database);
    let rows = over_the_limit();

    let sent = send(
        &database,
        publisher(&database, Some(store.clone())),
        rows.clone(),
    )
    .await;
    assert!(matches!(sent, HotResult::Ok(_)), "{sent:.200?}");

    let events = stored_events(&database).await;
    assert_eq!(events.len(), 1);
    let stored: Val = serde_json::from_str(&events[0]).unwrap();
    assert!(hot::blob::is_blob_ref_val(&stored), "{stored:.200?}");
    let budget = hot::blob::RehydrateBudget::from_config(store.config());
    let restored = store
        .rehydrate_blob_refs(stored, scope(&database.fixture), budget)
        .await
        .unwrap();
    assert_eq!(restored, rows);
}

#[tokio::test(flavor = "multi_thread")]
async fn sent_data_past_the_limit_without_blob_storage_is_an_err() {
    let _turn = ONE_AT_A_TIME.lock().await;
    let Some(database) = database().await else {
        return;
    };

    let sent = send(&database, publisher(&database, None), over_the_limit()).await;

    let HotResult::Err(error) = sent else {
        panic!("the caller must hear that the event wasn't stored");
    };
    let error = format!("{error}");
    assert!(error.contains("failed to store event"), "{error}");
    assert!(refused_for_its_size(&error), "{error}");
    assert!(stored_events(&database).await.is_empty());
}

/// `::hot::task/start`, as a worker's VM calls it.
async fn start_task(database: &Database, conf: Option<Val>, args: Val) -> HotResult<Val> {
    let (db, context) = (database.db.clone(), context(&database.fixture));
    tokio::task::spawn_blocking(move || {
        let mut vm = vm(db, context, conf);
        vm.set_task_queue(Arc::new(
            ProcessingQueue::new(
                QueueType::Memory,
                "hot:task".to_string(),
                None,
                Serialization::Json,
            )
            .unwrap(),
        ));
        hot::lang::hot::task::start(&mut vm, &[Val::from("::rig/takes-rows"), args])
    })
    .await
    .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn task_arguments_past_the_limit_are_stored_and_read_back() {
    let _turn = ONE_AT_A_TIME.lock().await;
    let Some(database) = database().await else {
        return;
    };
    let (store, dir) = blob_store(&database);
    let rows = over_the_limit();

    let started = start_task(&database, Some(conf_with_blobs(&dir)), rows.clone()).await;
    assert!(matches!(started, HotResult::Ok(_)), "{started:.200?}");

    let args: String = sqlx::query_scalar("SELECT args::text FROM hot.task")
        .fetch_one(&database.raw)
        .await
        .unwrap();
    let stored: Val = serde_json::from_str(&args).unwrap();
    assert!(hot::blob::is_blob_ref_val(&stored), "{stored:.200?}");
    let restored =
        hot::blob::rehydrate_whole(Some(store.as_ref()), stored, scope(&database.fixture))
            .await
            .unwrap();
    assert_eq!(restored, rows);
}

#[tokio::test(flavor = "multi_thread")]
async fn task_arguments_past_the_limit_without_blob_storage_are_an_err() {
    let _turn = ONE_AT_A_TIME.lock().await;
    let Some(database) = database().await else {
        return;
    };

    let started = start_task(&database, None, over_the_limit()).await;

    let HotResult::Err(error) = started else {
        panic!("the caller must hear that the task wasn't started");
    };
    let error = format!("{error}");
    assert!(error.contains("failed to insert task"), "{error}");
    assert!(refused_for_its_size(&error), "{error}");
    let tasks: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM hot.task")
        .fetch_one(&database.raw)
        .await
        .unwrap();
    assert_eq!(tasks, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_task_result_past_the_limit_only_fits_in_blob_storage() {
    let _turn = ONE_AT_A_TIME.lock().await;
    let Some(database) = database().await else {
        return;
    };
    let (store, dir) = blob_store(&database);
    let fixture = &database.fixture;
    let db = database.db.with_scope(DatabaseScope::Environment {
        org_id: fixture.org_id,
        env_id: fixture.env_id,
    });
    let task_id = Uuid::now_v7();
    Task::insert(
        &db,
        &task_id,
        &fixture.env_id,
        &fixture.stream_id,
        &fixture.build_id,
        None,
        "::rig/returns-rows",
        None,
        None,
        "code",
        60_000,
        None,
    )
    .await
    .unwrap();
    let rows = over_the_limit();
    let result = serde_json::to_value(&rows).unwrap();

    let refused = Task::complete(
        &db,
        &task_id,
        &TaskStatus::Completed,
        Some(&result),
        None,
        None,
    )
    .await;
    assert!(refused.is_err(), "the database should refuse this value");

    let stored = store
        .spill_whole_if_large(
            result,
            scope(fixture),
            SpillSource::TaskResult,
            Some(&task_id.to_string()),
        )
        .await
        .unwrap();
    assert!(
        Task::complete(
            &db,
            &task_id,
            &TaskStatus::Completed,
            Some(&stored),
            None,
            None,
        )
        .await
        .unwrap()
    );

    // And `await` hands the caller the value, not the reference.
    let context = context(fixture);
    let conf = conf_with_blobs(&dir);
    let id = Val::from(task_id.to_string());
    let awaited = tokio::task::spawn_blocking(move || {
        hot::lang::hot::task::await_task(&mut vm(db, context, Some(conf)), &[id])
    })
    .await
    .unwrap();
    let HotResult::Ok(awaited) = awaited else {
        panic!("await should succeed");
    };
    assert_eq!(awaited.get("result"), Some(rows));
}

/// A large value inline takes the database longer to take in, however fast the
/// host is. That must not read as a failure, or as anything but its real cause.
#[tokio::test(flavor = "multi_thread")]
async fn large_task_arguments_get_longer_to_insert() {
    let _turn = ONE_AT_A_TIME.lock().await;
    let Some(database) = database().await else {
        return;
    };
    for statement in [
        "CREATE FUNCTION hot.slow_tasks() RETURNS trigger LANGUAGE plpgsql AS \
         $$ BEGIN PERFORM pg_sleep(7); RETURN NEW; END $$",
        "CREATE TRIGGER slow_tasks BEFORE INSERT ON hot.task \
         FOR EACH ROW EXECUTE FUNCTION hot.slow_tasks()",
    ] {
        sqlx::query(statement)
            .execute(&database.raw)
            .await
            .expect("slow task inserts down");
    }

    let args = Val::vec(vec![Val::Int(0); 2_000_000]);
    let started = start_task(&database, None, args).await;

    assert!(matches!(started, HotResult::Ok(_)), "{started:.200?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn task_arguments_of_a_task_that_cant_be_inserted_are_released() {
    let _turn = ONE_AT_A_TIME.lock().await;
    let Some(database) = database().await else {
        return;
    };
    let (_store, dir) = blob_store(&database);
    let conf = conf_with_blobs(&dir);
    for statement in [
        "CREATE FUNCTION hot.refuse_tasks() RETURNS trigger LANGUAGE plpgsql AS \
         $$ BEGIN RAISE EXCEPTION 'refused'; END $$",
        "CREATE TRIGGER refuse_tasks BEFORE INSERT ON hot.task \
         FOR EACH ROW EXECUTE FUNCTION hot.refuse_tasks()",
    ] {
        sqlx::query(statement)
            .execute(&database.raw)
            .await
            .expect("refuse task inserts");
    }
    let (db, context) = (database.db.clone(), context(&database.fixture));
    let args = Val::vec(vec![Val::Int(0); 2_000_000]);
    let started = tokio::task::spawn_blocking(move || {
        let mut vm = vm(db, context, Some(conf));
        vm.set_task_queue(Arc::new(
            ProcessingQueue::new(
                QueueType::Memory,
                "hot:task".to_string(),
                None,
                Serialization::Json,
            )
            .unwrap(),
        ));
        hot::lang::hot::task::start(&mut vm, &[Val::from("::rig/takes-rows"), args])
    })
    .await
    .unwrap();
    let HotResult::Err(error) = started else {
        panic!("the insert should fail");
    };
    assert!(
        format!("{error}").contains("failed to insert task"),
        "{error}"
    );

    let (total, active): (i64, i64) = sqlx::query_as(
        "SELECT COUNT(*), COUNT(*) FILTER (WHERE active) FROM hot.blob_ref WHERE source_kind = 'task_args'",
    )
    .fetch_one(&database.raw)
    .await
    .unwrap();
    assert_eq!((total, active), (1, 0));
}

/// A `::hot::store` store on PostgreSQL, with one map to write to.
async fn pg_store(database: &Database, conf: Val) -> Box<dyn hot::store::Store> {
    let store = hot::store::store_from_config_with_db(
        &conf,
        Some(Arc::new(database.db.clone())),
        Some(database.fixture.org_id),
        Some(database.fixture.env_id),
    )
    .await
    .unwrap();
    store
        .ensure_store(
            &hot::store::store_map_config_from_val(&hot::val!({
                "$type": "::hot::store/Map",
                "$val": {"name": "big-store"},
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    store
}

#[tokio::test(flavor = "multi_thread")]
async fn a_store_value_past_the_limit_is_stored_and_reads_back() {
    let _turn = ONE_AT_A_TIME.lock().await;
    let Some(database) = database().await else {
        return;
    };
    let (_store, dir) = blob_store(&database);
    let mut conf = conf_with_blobs(&dir);
    conf = conf.set("store", hot::val!({"type": "postgres"}));
    let store = pg_store(&database, conf).await;
    let key = serde_json::json!("k");
    let rows = over_the_limit().to_exact_json();

    let written = store
        .put("big-store", key.clone(), rows.clone(), None, None)
        .await
        .unwrap();
    assert_eq!(written.value, rows);

    let (kept, size): (String, i64) =
        sqlx::query_as("SELECT value::text, size FROM hot.store_map_entry")
            .fetch_one(&database.raw)
            .await
            .unwrap();
    assert!(kept.len() < 4096, "{kept:.200}");
    assert!(kept.contains(hot::blob::BLOB_REF_TYPE), "{kept:.200}");
    assert!(
        size > hot::blob::MAX_INLINE_HARD_LIMIT_BYTES as i64,
        "{size}"
    );

    let entry = store.get("big-store", &key).await.unwrap().unwrap();
    assert_eq!(entry.value, rows);

    store
        .put("big-store", key.clone(), serde_json::json!(1), None, None)
        .await
        .unwrap();
    let active: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM hot.blob_ref WHERE source_kind = 'store_value' AND active",
    )
    .fetch_one(&database.raw)
    .await
    .unwrap();
    assert_eq!(active, 0, "overwriting should release the blob");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_store_value_past_the_limit_without_blob_storage_is_an_err_and_leaves_the_old_value() {
    let _turn = ONE_AT_A_TIME.lock().await;
    let Some(database) = database().await else {
        return;
    };
    let store = pg_store(&database, hot::val!({"store": {"type": "postgres"}})).await;
    let key = serde_json::json!("k");
    let old = serde_json::json!({"kept": true});
    store
        .put("big-store", key.clone(), old.clone(), None, None)
        .await
        .unwrap();

    let refused = store
        .put(
            "big-store",
            key.clone(),
            over_the_limit().to_exact_json(),
            None,
            None,
        )
        .await
        .expect_err("put must fail at the call");

    assert!(refused.contains("store write failed"), "{refused}");
    assert!(refused_for_its_size(&refused), "{refused}");
    let entry = store.get("big-store", &key).await.unwrap().unwrap();
    assert_eq!(entry.value, old);
}

//! A value too large to record must not fail a run on PostgreSQL, which refuses
//! a `jsonb` container over 256 MB. Needs `HOT_TEST_POSTGRES_URI`.

use hot::blob::{BlobConfig, BlobMode, BlobRefVal, BlobScope, BlobStore, RehydrateBudget};
use hot::db::{DatabasePool, TestData};
use hot::lang::emitter::database::DatabaseEngineEventEmitter;
use hot::lang::emitter::{EngineEvent, EngineEventEmitter};
use hot::lang::event::ExecutionContext;
use hot::val::Val;
use std::sync::Arc;
use uuid::Uuid;

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
    let name = format!("hot_oversized_{}", Uuid::now_v7().simple());
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

/// Record a call returning `returned`, which has to succeed, and return its id.
async fn record(
    emitter: &DatabaseEngineEventEmitter,
    context: &ExecutionContext,
    returned: Val,
) -> Uuid {
    let call_id = Uuid::now_v7();
    emitter.emit(EngineEvent::call_start(
        context,
        call_id,
        None,
        "::hot::test/rows".to_string(),
        "::hot::test/rows".to_string(),
        "run/test".to_string(),
        0,
        vec![],
        None,
        chrono::Utc::now(),
        None,
    ));
    emitter.emit(EngineEvent::call_stop(
        context,
        call_id,
        Some(returned),
        None,
        chrono::Utc::now(),
        10,
    ));
    emitter
        .flush_async()
        .await
        .expect("recording a value must not fail the run");
    call_id
}

async fn stored_text(raw: &sqlx::PgPool, call_id: Uuid) -> String {
    sqlx::query_scalar("SELECT return_value::text FROM hot.call WHERE call_id = $1")
        .bind(call_id)
        .fetch_one(raw)
        .await
        .expect("the call was recorded")
}

/// `jsonb` takes about six times the space of `0,`, the worst case for the limit.
fn zeros(count: usize) -> Val {
    Val::vec(vec![Val::Int(0); count])
}

#[tokio::test(flavor = "multi_thread")]
async fn the_worst_shape_at_the_limit_fits_jsonb_and_past_it_is_left_out() {
    let Some(database) = database().await else {
        return;
    };
    let emitter = DatabaseEngineEventEmitter::new_with_pool(database.db.clone());
    let context = context(&database.fixture);

    // Just under the limit, as JSON text.
    let at_limit = (hot::blob::MAX_INLINE_HARD_LIMIT_BYTES - 2) / 2;
    let call_id = record(&emitter, &context, zeros(at_limit)).await;
    let stored: i32 = sqlx::query_scalar(
        "SELECT jsonb_array_length(return_value) FROM hot.call WHERE call_id = $1",
    )
    .bind(call_id)
    .fetch_one(&database.raw)
    .await
    .unwrap();
    assert_eq!(stored as usize, at_limit);

    // Past what `jsonb` can hold, which fails the writes without the limit.
    let call_id = record(&emitter, &context, zeros(24_000_000)).await;
    let stored = stored_text(&database.raw, call_id).await;
    assert!(stored.contains(hot::blob::OMITTED_TYPE), "{stored:.200}");

    emitter
        .shutdown()
        .await
        .expect("the run's writes succeeded");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_value_of_many_small_parts_is_stored_whole_and_reads_back() {
    let Some(database) = database().await else {
        return;
    };
    let storage_dir = tempfile::tempdir().unwrap();
    let config = BlobConfig {
        mode: BlobMode::Service,
        ..BlobConfig::default()
    };
    let store = Arc::new(BlobStore::new(
        Arc::new(database.db.clone()),
        Arc::new(hot::file_storage::LocalFileStorage::new(
            storage_dir.path().to_path_buf(),
        )),
        config,
    ));
    let emitter = DatabaseEngineEventEmitter::new_with_pool_and_blob_store(
        database.db.clone(),
        Some(store.clone()),
    );
    let context = context(&database.fixture);
    let rows = Val::vec(
        (0..40_000i64)
            .map(|i| hot::val!({"id": i, "note": "a small cell", "zip": "02134"}))
            .collect(),
    );

    let call_id = record(&emitter, &context, rows.clone()).await;
    emitter
        .shutdown()
        .await
        .expect("the run's writes succeeded");

    let stored: Val = serde_json::from_str(&stored_text(&database.raw, call_id).await).unwrap();
    assert!(BlobRefVal::from_val(&stored).unwrap().is_some());
    let scope = BlobScope {
        org_id: database.fixture.org_id,
        env_id: Some(database.fixture.env_id),
        run_id: Some(database.fixture.run_id),
    };
    let restored = store
        .rehydrate_blob_refs(stored, scope, RehydrateBudget::from_config(store.config()))
        .await
        .unwrap();
    assert_eq!(restored, rows);
}

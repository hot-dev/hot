//! Session advisory locks against a real PostgreSQL server.
//! Skipped unless HOT_TEST_POSTGRES_URI names a privileged fixture database.

use hot::db::{DatabasePool, ScopedPgPool};
use sqlx::postgres::PgPoolOptions;

#[tokio::test]
async fn a_lock_lost_with_its_session_is_detected() -> Result<(), Box<dyn std::error::Error>> {
    let Ok(admin_uri) = std::env::var("HOT_TEST_POSTGRES_URI") else {
        eprintln!("skipping advisory lock test: HOT_TEST_POSTGRES_URI not set");
        return Ok(());
    };
    let admin = PgPoolOptions::new()
        .max_connections(2)
        .connect(&admin_uri)
        .await?;
    let db = DatabasePool::Postgres(ScopedPgPool::new(admin.clone()));
    let lock_id = rand_lock_id();
    let mut guard = hot::db::try_acquire_advisory_lock(&db, lock_id)
        .await?
        .expect("a fresh lock is free");
    assert!(guard.ensure_held().await?);
    assert!(
        hot::db::try_acquire_advisory_lock(&db, lock_id)
            .await?
            .is_none(),
        "the lock is exclusive while held"
    );

    // Ending the holder's session silently frees the lock for anyone else.
    let terminated: bool = sqlx::query_scalar(
        "SELECT pg_terminate_backend(pid) FROM pg_locks WHERE locktype = 'advisory' AND granted \
         AND classid = (($1 >> 32) & 4294967295)::oid AND objid = ($1 & 4294967295)::oid",
    )
    .bind(lock_id)
    .fetch_one(&admin)
    .await?;
    assert!(terminated);
    let mut successor = None;
    for _ in 0..50 {
        successor = hot::db::try_acquire_advisory_lock(&db, lock_id).await?;
        if successor.is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let successor = successor.expect("the lock is free once its session ends");
    assert!(
        !matches!(guard.ensure_held().await, Ok(true)),
        "the original holder must learn it no longer holds the lock"
    );
    successor.release().await;
    guard.release().await;
    Ok(())
}

fn rand_lock_id() -> i64 {
    // Positive and unlikely to collide with production lock ids.
    let bytes = uuid::Uuid::new_v4().into_bytes();
    i64::from_le_bytes(bytes[..8].try_into().unwrap()) & 0x0000_7fff_ffff_ffff
}

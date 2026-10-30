//! The scheduler's singleton lock against a real PostgreSQL server.
//! Skipped unless HOT_TEST_POSTGRES_URI names a privileged fixture database.

use hot::db::{DatabasePool, SchedulePolicy, ScopedPgPool};
use hot::queue::QueueType;
use hot_scheduler::server::SCHEDULER_ADVISORY_LOCK_ID;
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use std::time::Duration;

async fn start_scheduler(uri: &str) -> tokio::task::JoinHandle<Result<(), String>> {
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(uri)
        .await
        .unwrap();
    let db = DatabasePool::Postgres(ScopedPgPool::new(pool));
    tokio::spawn(async move {
        // Long intervals: only the singleton lock is under test.
        hot_scheduler::run(
            QueueType::Memory,
            None,
            false,
            hot_scheduler::DEFAULT_SERIALIZATION,
            Some(db),
            Some(3600),
            Some(3600),
            Some(3600),
            false,
            SchedulePolicy::self_hosted_default(),
            true,
        )
        .await
        .map_err(|e| e.to_string())
    })
}

async fn lock_holder(admin: &PgPool) -> Option<i32> {
    sqlx::query_scalar(
        "SELECT pid FROM pg_locks WHERE locktype = 'advisory' AND granted AND objsubid = 1 \
         AND classid = (($1 >> 32) & 4294967295)::oid AND objid = ($1 & 4294967295)::oid",
    )
    .bind(SCHEDULER_ADVISORY_LOCK_ID)
    .fetch_optional(admin)
    .await
    .unwrap()
}

async fn wait_for_new_holder(admin: &PgPool, previous: Option<i32>) -> i32 {
    for _ in 0..200 {
        if let Some(pid) = lock_holder(admin).await
            && Some(pid) != previous
        {
            return pid;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("no scheduler took the singleton lock");
}

#[tokio::test]
async fn a_scheduler_that_loses_its_lock_stops() -> Result<(), Box<dyn std::error::Error>> {
    let Ok(uri) = std::env::var("HOT_TEST_POSTGRES_URI") else {
        eprintln!("skipping scheduler singleton test: HOT_TEST_POSTGRES_URI not set");
        return Ok(());
    };
    let admin = PgPoolOptions::new()
        .max_connections(2)
        .connect(&uri)
        .await?;

    let first = start_scheduler(&uri).await;
    let holder = wait_for_new_holder(&admin, None).await;

    // Deployment overlap: a second scheduler refuses to start while one runs.
    let second = start_scheduler(&uri).await.await?;
    assert!(
        second.as_ref().unwrap_err().contains("already holds"),
        "{second:?}"
    );

    // Failover, `pg_terminate_backend`, or an idle timeout ends the lock's
    // session. The first scheduler must stop within one bounded check.
    let terminated: bool = sqlx::query_scalar("SELECT pg_terminate_backend($1)")
        .bind(holder)
        .fetch_one(&admin)
        .await?;
    assert!(terminated);
    let outcome = tokio::time::timeout(Duration::from_secs(10), first)
        .await
        .expect("a scheduler that lost its lock must stop")?;
    assert!(
        outcome.as_ref().unwrap_err().contains("lock was lost"),
        "{outcome:?}"
    );

    // Its replacement acquires the lock afresh.
    let replacement = start_scheduler(&uri).await;
    wait_for_new_holder(&admin, Some(holder)).await;
    replacement.abort();
    Ok(())
}

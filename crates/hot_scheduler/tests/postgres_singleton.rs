//! The scheduler's singleton lock against a real PostgreSQL server.
//! Skipped unless HOT_TEST_POSTGRES_URI names a privileged fixture database.

use hot::db::{DatabasePool, SchedulePolicy, ScopedPgPool};
use hot::queue::QueueType;
use hot_scheduler::server::SCHEDULER_ADVISORY_LOCK_ID;
use sqlx::PgPool;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

// The tests share one lock id, so they run one at a time.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn start_scheduler(uri: &str) -> tokio::task::JoinHandle<Result<(), String>> {
    start_scheduler_with(PgConnectOptions::from_str(uri).unwrap()).await
}

async fn start_scheduler_with(
    options: PgConnectOptions,
) -> tokio::task::JoinHandle<Result<(), String>> {
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect_with(options)
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
    let _serial = SERIAL.lock().await;
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

/// Forwards connections to PostgreSQL, and holds their traffic while paused,
/// as a paused container or a sleeping laptop does.
async fn stalling_proxy(upstream: (String, u16)) -> (u16, Arc<AtomicBool>) {
    async fn pump(
        mut from: tokio::net::tcp::OwnedReadHalf,
        mut to: tokio::net::tcp::OwnedWriteHalf,
        paused: Arc<AtomicBool>,
    ) {
        let mut buffer = vec![0u8; 16 * 1024];
        while let Ok(n @ 1..) = from.read(&mut buffer).await {
            while paused.load(Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            if to.write_all(&buffer[..n]).await.is_err() {
                break;
            }
        }
    }

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let paused = Arc::new(AtomicBool::new(false));
    let stall = Arc::clone(&paused);
    tokio::spawn(async move {
        while let Ok((client, _)) = listener.accept().await {
            let Ok(server) = tokio::net::TcpStream::connect(upstream.clone()).await else {
                continue;
            };
            let (client_read, client_write) = client.into_split();
            let (server_read, server_write) = server.into_split();
            tokio::spawn(pump(client_read, server_write, Arc::clone(&stall)));
            tokio::spawn(pump(server_read, client_write, Arc::clone(&stall)));
        }
    });
    (port, paused)
}

#[tokio::test]
async fn a_scheduler_keeps_its_lock_through_a_brief_database_stall()
-> Result<(), Box<dyn std::error::Error>> {
    let Ok(uri) = std::env::var("HOT_TEST_POSTGRES_URI") else {
        eprintln!("skipping scheduler stall test: HOT_TEST_POSTGRES_URI not set");
        return Ok(());
    };
    let _serial = SERIAL.lock().await;
    let admin = PgPoolOptions::new()
        .max_connections(2)
        .connect(&uri)
        .await?;
    let options = PgConnectOptions::from_str(&uri)?;
    let (port, paused) = stalling_proxy((options.get_host().to_string(), options.get_port())).await;

    let scheduler = start_scheduler_with(options.host("127.0.0.1").port(port)).await;
    let holder = wait_for_new_holder(&admin, None).await;
    // Let startup confirm the lock before the stall.
    tokio::time::sleep(Duration::from_millis(500)).await;

    // A 3 s stall delays one or two lock checks past their 1 s limit. The
    // scheduler keeps its lock instead of stopping for good.
    paused.store(true, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_secs(3)).await;
    paused.store(false, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_secs(3)).await;

    assert!(
        !scheduler.is_finished(),
        "the scheduler stopped: {:?}",
        scheduler.await
    );
    assert_eq!(lock_holder(&admin).await, Some(holder));
    scheduler.abort();
    Ok(())
}

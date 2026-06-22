use futures::future::join_all;
use hot::data::msg::Message;
use hot::data::serialization::Serialization;
use hot::queue::{ProcessingQueue, Queue, QueueType};
use hot::val;
use std::sync::Arc;
use uuid::Uuid;

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(default)
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn percentile(sorted: &[u64], percentile: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let index = ((sorted.len() - 1) as f64 * percentile).ceil() as usize;
    sorted[index.min(sorted.len() - 1)]
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "load harness; run explicitly with `cargo test -p hot --test queue_wait_load -- --ignored --nocapture`"]
async fn memory_queue_enqueue_to_dequeue_wait_p99_stays_below_target() {
    let item_count = env_usize("HOT_QUEUE_LOAD_ITEMS", 512);
    let p99_target_ms = env_usize("HOT_QUEUE_LOAD_P99_MS", 1_000) as u64;
    let queue = Arc::new(
        ProcessingQueue::<Message>::new(
            QueueType::Memory,
            format!("queue-load-{}", Uuid::now_v7()),
            None,
            Serialization::Json,
        )
        .expect("memory queue should construct"),
    );

    for _ in 0..item_count {
        let created_at_unix_ms = now_ms();
        queue
            .enqueue(Message {
                id: Uuid::now_v7(),
                head: val!({
                    "__type": "LoadHarnessMessage",
                    "created_at_unix_ms": created_at_unix_ms as i64,
                }),
                body: val!({
                    "ok": true,
                }),
            })
            .await
            .expect("enqueue should succeed");
    }

    let tasks = (0..item_count).map(|_| {
        let queue = Arc::clone(&queue);
        tokio::spawn(async move {
            queue
                .process_blocking(|message: Message| async move {
                    let created_at_unix_ms = message
                        .head
                        .get_int_or_default("created_at_unix_ms", 0)
                        .max(0) as u64;
                    let wait_ms = now_ms().saturating_sub(created_at_unix_ms);
                    Ok::<u64, Box<dyn std::error::Error + Send + Sync>>(wait_ms)
                })
                .await
        })
    });

    let mut waits_ms = Vec::with_capacity(item_count);
    for result in join_all(tasks).await {
        let wait_ms = result
            .expect("worker task should join")
            .expect("queue processing should succeed")
            .expect("queue should have work");
        waits_ms.push(wait_ms);
    }

    waits_ms.sort_unstable();
    let p50 = percentile(&waits_ms, 0.50);
    let p95 = percentile(&waits_ms, 0.95);
    let p99 = percentile(&waits_ms, 0.99);
    println!(
        "memory queue load: items={} p50={}ms p95={}ms p99={}ms target={}ms",
        item_count, p50, p95, p99, p99_target_ms
    );
    assert!(
        p99 <= p99_target_ms,
        "queue wait p99 {}ms exceeded target {}ms",
        p99,
        p99_target_ms
    );
}

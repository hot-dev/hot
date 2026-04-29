//! `hot queue` — manage event queues (clear, status).

use hot::val::Val;
use tracing::info;

use crate::cli::QueueAction;

pub(crate) async fn run_queue(action: &QueueAction, conf: &Val) -> Result<(), String> {
    use std::str::FromStr;

    let queue_type_str = conf.get_str_or_default("queue.type", "memory");
    let queue_type =
        hot::queue::QueueType::from_str(&queue_type_str).unwrap_or(hot::queue::QueueType::Memory);

    let redis_cluster = conf.get_bool_or_default("redis.cluster", false);

    match action {
        QueueAction::Clear => match queue_type {
            hot::queue::QueueType::Memory => {
                println!(
                    "Memory queues are in-process only — nothing to clear from the CLI.\n\
                         Stop the running `hot dev` / `hot worker` to drop all queued items,\n\
                         or set `queue.type = \"redis\"` (or HOT_QUEUE_TYPE=redis) for a\n\
                         persistent multi-process queue."
                );
            }
            hot::queue::QueueType::Redis => {
                let redis_uri = conf.get_str_or_default("redis.uri", "redis://127.0.0.1/");
                if redis_uri.is_empty() || redis_uri == "null" {
                    return Err("Redis URI not configured. Set redis.uri in config.".to_string());
                }

                info!(
                    "Clearing Redis queues at {}{}...",
                    redis_uri,
                    if redis_cluster { " (cluster mode)" } else { "" }
                );

                let admin = hot::queue::RedisQueueAdmin::new(redis_uri, redis_cluster);
                let cleared = admin.clear_all()?;

                for key in &cleared {
                    info!("  Cleared: {}", key);
                }
                if cleared.is_empty() {
                    info!("  No queues found to clear.");
                }

                println!("Redis queues cleared.");
            }
        },
        QueueAction::Status => {
            println!("Queue Status:");
            println!("  Type: {}", queue_type);

            match queue_type {
                hot::queue::QueueType::Memory => {
                    println!("  Backend: In-process channel (no cross-process visibility)");
                    println!(
                        "\n  Memory queues live inside the running process and cannot be\n\
                         inspected from a separate CLI invocation.\n\
                         For introspectable queues, set `queue.type = \"redis\"` (or\n\
                         HOT_QUEUE_TYPE=redis)."
                    );
                }
                hot::queue::QueueType::Redis => {
                    let redis_uri = conf.get_str_or_default("redis.uri", "redis://127.0.0.1/");
                    println!(
                        "  Backend: Redis ({}){}",
                        redis_uri,
                        if redis_cluster { " [cluster]" } else { "" }
                    );

                    if redis_uri.is_empty() || redis_uri == "null" {
                        println!("\n  Redis URI not configured.");
                        return Ok(());
                    }

                    let admin = hot::queue::RedisQueueAdmin::new(redis_uri, redis_cluster);

                    match admin.status() {
                        Ok(summary) => {
                            println!("\nQueues:");
                            println!(
                                "  {:<20} {:>8} {:>10} {:>10}",
                                "Name", "Pending", "Processing", "DeadLetter"
                            );
                            println!("  {:-<20} {:->8} {:->10} {:->10}", "", "", "", "");

                            for queue in &summary.queues {
                                println!(
                                    "  {:<20} {:>8} {:>10} {:>10}",
                                    queue.name, queue.pending, queue.processing, queue.deadletter
                                );
                            }

                            println!("\nSummary:");
                            println!("  Total pending:    {}", summary.total_pending);
                            println!("  Total processing: {}", summary.total_processing);
                            println!("  Total deadletter: {}", summary.total_deadletter);
                        }
                        Err(e) => {
                            println!("\n  Query failed: {}", e);
                        }
                    }
                }
            }
        }
    }

    Ok(())
}

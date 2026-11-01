//! `hot queue` — manage event queues (clear, status).

use hot::queue::QueueAdmin;
use hot::val::Val;
use tracing::info;

use crate::cli::QueueAction;

pub(crate) async fn run_queue(action: &QueueAction, conf: &Val) -> Result<(), String> {
    use std::str::FromStr;

    let queue_type_str = conf.get_str_or_default("queue.type", "sqlite");
    let queue_type =
        hot::queue::QueueType::from_str(&queue_type_str).unwrap_or(hot::queue::QueueType::Memory);

    match action {
        QueueAction::Clear => {
            let Some(admin) = queue_admin(queue_type, conf).await? else {
                println!(
                    "Memory queues are in-process only — nothing to clear from the CLI.\n\
                         Stop the running `hot dev` / `hot worker` to drop all queued items,\n\
                         or use `queue.type = \"sqlite\"` for project-local persistence or\n\
                         `queue.type = \"redis\"` for a distributed queue."
                );
                return Ok(());
            };

            info!(
                "Clearing {} queues at {}...",
                admin.backend_name(),
                admin.location()
            );
            let cleared = admin.clear_all().await?;
            for queue in &cleared {
                info!("  Cleared: {}", queue);
            }
            if cleared.is_empty() {
                info!("  No queues found to clear.");
            }
            println!("{} queues cleared.", admin.backend_name());
        }
        QueueAction::Status { json: true } => {
            let Some(admin) = queue_admin(queue_type, conf).await? else {
                return Err(
                    "Memory queues live inside the running process and cannot be inspected"
                        .to_string(),
                );
            };
            let summary = admin.status().await?;
            let mut status = serde_json::to_value(&summary).map_err(|e| e.to_string())?;
            status["type"] = queue_type.to_string().into();
            println!("{}", status);
        }
        QueueAction::Status { json: false } => {
            println!("Queue Status:");
            println!("  Type: {}", queue_type);

            let admin = match queue_admin(queue_type, conf).await {
                Ok(Some(admin)) => admin,
                Ok(None) => {
                    println!("  Backend: In-process channel (no cross-process visibility)");
                    println!(
                        "\n  Memory queues live inside the running process and cannot be\n\
                         inspected from a separate CLI invocation.\n\
                         For introspectable queues, use `queue.type = \"sqlite\"` or\n\
                         `queue.type = \"redis\"`."
                    );
                    return Ok(());
                }
                Err(error) => {
                    println!("\n  {}", error);
                    return Ok(());
                }
            };

            println!("  Backend: {} ({})", admin.backend_name(), admin.location());
            match admin.status().await {
                Ok(summary) => print_summary(&summary),
                Err(error) => println!("\n  Query failed: {}", error),
            }
        }
    }

    Ok(())
}

/// The admin for the configured queue backend, or None for memory queues,
/// which live inside their process.
async fn queue_admin(
    queue_type: hot::queue::QueueType,
    conf: &Val,
) -> Result<Option<Box<dyn QueueAdmin>>, String> {
    match queue_type {
        hot::queue::QueueType::Memory => Ok(None),
        hot::queue::QueueType::Sqlite => Ok(Some(Box::new(
            hot::queue::sqlite::SqliteQueueAdmin::default(),
        ))),
        hot::queue::QueueType::Redis => {
            let redis_uri = conf.get_str_or_default("redis.uri", "redis://127.0.0.1/");
            if redis_uri.is_empty() || redis_uri == "null" {
                return Err("Redis URI not configured. Set redis.uri in config.".to_string());
            }
            let redis_cluster = conf.get_bool_or_default("redis.cluster", false);
            Ok(Some(Box::new(
                hot::queue::RedisQueueAdmin::new(redis_uri, redis_cluster)
                    .with_password(hot::redis::one_off_password().await?),
            )))
        }
    }
}

fn print_summary(summary: &hot::queue::QueueStatusSummary) {
    let wait = |seconds: Option<i64>| seconds.map_or("-".to_string(), |s| format!("{}s", s));
    println!("\nQueues:");
    println!(
        "  {:<20} {:>8} {:>10} {:>10} {:>12}",
        "Name", "Pending", "Processing", "DeadLetter", "OldestWait"
    );
    println!(
        "  {:-<20} {:->8} {:->10} {:->10} {:->12}",
        "", "", "", "", ""
    );
    for queue in &summary.queues {
        println!(
            "  {:<20} {:>8} {:>10} {:>10} {:>12}",
            queue.name,
            queue.pending,
            queue.processing,
            queue.deadletter,
            wait(queue.oldest_pending_secs)
        );
    }
    println!("\nSummary:");
    println!("  Total pending:    {}", summary.total_pending);
    println!("  Total processing: {}", summary.total_processing);
    println!("  Total deadletter: {}", summary.total_deadletter);
    println!("  Oldest wait:      {}", wait(summary.oldest_pending_secs));
}

//! Notification worker - dedicated thread for alert deliveries and app emails
//!
//! Consumes from hot:alert and hot:email queues, providing guaranteed processing
//! that can't be starved by the main worker threads processing events/deployments.

use hot::data::msg::Message;
use hot::db::DatabasePool;
use hot::email::EmailSender;
use hot::lang::event::queue::{AlertDeliveryMessage, EmailMessage};
use hot::queue::ProcessingQueue;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;

/// HTTP request timeout for webhook/Slack/PagerDuty deliveries
const HTTP_TIMEOUT_SECS: u64 = 10;

/// Spawn the notification worker as a background task
///
/// Consumes from both hot:alert and hot:email queues.
/// The worker will process messages until the shutdown signal is received.
pub fn spawn_notification_worker(
    db: Arc<DatabasePool>,
    conf: hot::val::Val,
    alert_queue: ProcessingQueue<Message>,
    email_queue: ProcessingQueue<Message>,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    // Callers pass an unscoped pool; each queue runs under its own operation.
    let alert_db = db.with_scope(hot::db::DatabaseScope::Maintenance {
        operation: hot::db::MaintenanceOperation::Alerts,
        org_id: None,
    });
    let email_db = db.with_scope(hot::db::DatabaseScope::Maintenance {
        operation: hot::db::MaintenanceOperation::Email,
        org_id: None,
    });
    tokio::spawn(async move {
        tracing::info!("Notification worker started (alert + email queue consumer)");

        // Create HTTP client for webhook/Slack/PagerDuty deliveries
        let http_client = hot::outbound::client_builder()
            .timeout(Duration::from_secs(HTTP_TIMEOUT_SECS))
            .connect_timeout(Duration::from_secs(5))
            .build()
            .expect("Failed to create HTTP client for notification worker");

        // Create email senders
        // Kept for the worker's life: an SMTP sender owns a connection pool.
        let alert_email_sender = Arc::new(EmailSender::alerts_from_conf(&conf));
        let app_email_sender = Arc::new(EmailSender::from_conf(&conf));

        let alert_email_available = alert_email_sender.is_available();
        let app_email_available = app_email_sender.is_available();

        if !alert_email_available {
            tracing::warn!("Alert email sender not configured");
        }
        if !app_email_available {
            tracing::warn!("App email sender not configured");
        }

        loop {
            // Race shutdown vs both queues. process_blocking parks until a
            // message is enqueued (Memory) or BLOCK times out (Redis), so we
            // wake instantly on real work without polling.
            tokio::select! {
                biased;
                changed = shutdown_rx.changed() => {
                    if changed.is_err() || *shutdown_rx.borrow() {
                        tracing::info!("Notification worker received shutdown signal");
                        break;
                    }
                }
                result = run_one_alert(
                    &alert_queue,
                    &alert_db,
                    &conf,
                    &http_client,
                    &alert_email_sender,
                    alert_email_available,
                ) => {
                    if let ProcessResult::Error(e) = result {
                        tracing::error!("Notification worker alert processing error: {}", e);
                    }
                }
                result = run_one_email(
                    &email_queue,
                    &email_db,
                    &app_email_sender,
                    app_email_available,
                ) => {
                    if let ProcessResult::Error(e) = result {
                        tracing::error!("Notification worker email processing error: {}", e);
                    }
                }
            }
        }

        tracing::info!("Notification worker stopped");
    });
}

enum ProcessResult {
    Processed,
    Empty,
    Error(String),
}

/// Process one alert delivery message from the queue using the blocking
/// `process_blocking` API so the future parks until a message is enqueued
/// (Memory) or the Redis BLOCK times out.
async fn run_one_alert(
    queue: &ProcessingQueue<Message>,
    db: &DatabasePool,
    conf: &hot::val::Val,
    http_client: &reqwest::Client,
    alert_email_sender: &Arc<EmailSender>,
    email_available: bool,
) -> ProcessResult {
    let result = queue
        .process_blocking(|message| {
            let db = db.clone();
            let conf = conf.clone();
            let http_client = http_client.clone();
            let alert_email_sender = Arc::clone(alert_email_sender);
            async move {
                let alert_msg: AlertDeliveryMessage = message.try_into().map_err(|e: String| {
                    Box::new(std::io::Error::other(e)) as Box<dyn std::error::Error + Send + Sync>
                })?;

                tracing::debug!(
                    "Processing alert delivery {} for alert {} (type: {})",
                    alert_msg.body.alert_delivery_id,
                    alert_msg.body.alert_id,
                    alert_msg.body.destination_type
                );

                let alert_email_config = hot::email::EmailConfig::alerts_from_conf(&conf);
                let email_sender_ref: Option<&dyn hot::db::alert::AlertEmailSender> =
                    if email_available {
                        Some(alert_email_sender.as_ref())
                    } else {
                        None
                    };

                match hot::db::alert::process_single_alert_delivery(
                    &db,
                    &http_client,
                    hot::outbound::DestinationPolicy::for_alert_delivery(&conf),
                    email_sender_ref,
                    &alert_email_config,
                    &alert_msg.body.alert_delivery_id,
                )
                .await
                {
                    Ok(success) => {
                        if success {
                            tracing::info!(
                                "Alert delivery {} sent successfully",
                                alert_msg.body.alert_delivery_id
                            );
                        } else {
                            tracing::warn!(
                                "Alert delivery {} failed (will retry if attempts remain)",
                                alert_msg.body.alert_delivery_id
                            );
                        }
                        Ok(())
                    }
                    Err(e) => {
                        tracing::error!(
                            "Alert delivery {} processing error: {}",
                            alert_msg.body.alert_delivery_id,
                            e
                        );
                        Err(Box::new(std::io::Error::other(e.to_string()))
                            as Box<dyn std::error::Error + Send + Sync>)
                    }
                }
            }
        })
        .await;

    match result {
        Ok(Some(_)) => ProcessResult::Processed,
        Ok(None) => ProcessResult::Empty,
        Err(e) => ProcessResult::Error(e.to_string()),
    }
}

/// Process one app email message from the queue.
async fn run_one_email(
    queue: &ProcessingQueue<Message>,
    db: &DatabasePool,
    app_email_sender: &Arc<EmailSender>,
    email_available: bool,
) -> ProcessResult {
    let result = queue
        .process_blocking(|message| {
            let db = db.clone();
            let sender = Arc::clone(app_email_sender);
            async move {
                let email_msg: EmailMessage = message.try_into().map_err(|e: String| {
                    Box::new(std::io::Error::other(e)) as Box<dyn std::error::Error + Send + Sync>
                })?;

                tracing::debug!(
                    "Processing app email to {} (subject: {})",
                    email_msg.body.to_address,
                    email_msg.body.subject
                );

                if !email_available {
                    tracing::warn!(
                        "App email sender not configured, marking email {} as failed",
                        email_msg.body.email_queue_id
                    );
                    let _ = hot::db::email_queue::EmailQueueEntry::mark_failed(
                        &db,
                        &email_msg.body.email_queue_id,
                        sender.unavailable_reason(),
                    )
                    .await;
                    return Ok(());
                }

                let email = hot::email::Email {
                    to: email_msg.body.to_address.clone(),
                    subject: email_msg.body.subject.clone(),
                    html: email_msg.body.html_body.clone(),
                    text: email_msg.body.text_body.clone(),
                };

                match sender
                    .send_email_with_from(&email, &email_msg.body.from_address)
                    .await
                {
                    Ok(()) => {
                        tracing::info!(
                            "App email sent to {}: {}",
                            email_msg.body.to_address,
                            email_msg.body.subject
                        );
                        let _ = hot::db::email_queue::EmailQueueEntry::mark_sent(
                            &db,
                            &email_msg.body.email_queue_id,
                        )
                        .await;
                    }
                    Err(e) => {
                        // Return worker error so queue-level retry/DLQ logic handles transient
                        // send failures (including provider rate limits) without dropping email.
                        tracing::warn!(
                            "Failed to send app email to {} (id: {}), will retry via queue: {}",
                            email_msg.body.to_address,
                            email_msg.body.email_queue_id,
                            e
                        );
                        // Add a small interval before requeue retry to avoid hot-looping.
                        tokio::time::sleep(Duration::from_secs(1)).await;
                        return Err(Box::new(std::io::Error::other(format!(
                            "app email send failed for {}: {}",
                            email_msg.body.email_queue_id, e
                        )))
                            as Box<dyn std::error::Error + Send + Sync>);
                    }
                }

                Ok(())
            }
        })
        .await;

    match result {
        Ok(Some(_)) => ProcessResult::Processed,
        Ok(None) => ProcessResult::Empty,
        Err(e) => ProcessResult::Error(e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hot::db::email_queue::{EmailQueueEntry, EmailQueueStatus};
    use hot::lang::event::queue::EmailMessageBody;
    use hot::queue::{Queue, mem::MemQueue};
    use uuid::Uuid;

    #[tokio::test]
    async fn an_app_email_this_worker_cannot_send_records_why() {
        let sender = Arc::new(EmailSender::from_conf(&hot::val!({
            "email": {"provider": "mystery", "from": "hot@example.test"}
        })));
        let db = hot::db::test_db().await;
        let (to_address, from_address) = ("ops@example.test", "Hot <hot@example.test>");
        let email_queue_id = EmailQueueEntry::enqueue(
            &db,
            to_address,
            "Hello",
            None,
            Some("undelivered"),
            from_address,
        )
        .await
        .unwrap();
        let queue = ProcessingQueue::Memory(
            MemQueue::new(format!("test:app-email:{}", Uuid::now_v7())).unwrap(),
        );
        queue
            .enqueue(
                EmailMessage {
                    id: Uuid::now_v7(),
                    head: ahash::AHashMap::new(),
                    body: EmailMessageBody {
                        email_queue_id,
                        to_address: to_address.to_string(),
                        subject: "Hello".to_string(),
                        html_body: None,
                        text_body: Some("undelivered".to_string()),
                        from_address: from_address.to_string(),
                    },
                }
                .into(),
            )
            .await
            .unwrap();

        let result = run_one_email(&queue, &db, &sender, sender.is_available()).await;
        assert!(matches!(result, ProcessResult::Processed));

        let entry = EmailQueueEntry::get_by_id(&db, &email_queue_id)
            .await
            .unwrap();
        assert_eq!(entry.status_id, EmailQueueStatus::Failed as i16);
        assert_eq!(
            entry.error_message.as_deref(),
            Some("email provider 'mystery' is not registered")
        );
    }
}

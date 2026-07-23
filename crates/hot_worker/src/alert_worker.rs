//! Alert delivery background worker
//!
//! Slow safety-net poller for pending alert deliveries (email, Slack,
//! PagerDuty, webhook). Most deliveries flow through the `hot:alert`
//! queue with sub-second latency via the notification worker; this poller
//! exists only to catch deliveries that slipped past the queue path
//! (e.g. enqueued before the worker started, or worker crash mid-process).
//! As a result the interval is intentionally coarse — tightening it just
//! adds DB load without improving end-user latency.

use hot::db::DatabasePool;
use hot::db::alert::process_pending_deliveries;
use hot::email::EmailSender;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;

/// Default interval between alert delivery processing runs (in seconds).
/// Raised from 10s → 60s in Phase 4d: real-time deliveries now go through
/// the `hot:alert` Redis Stream consumed by the notification worker, so
/// this poll is a slow safety net rather than the primary path.
const DEFAULT_POLL_INTERVAL_SECS: u64 = 60;

/// Default batch size for processing deliveries
const DEFAULT_BATCH_SIZE: i64 = 50;

/// HTTP request timeout - kept short so shutdown isn't blocked too long
const HTTP_TIMEOUT_SECS: u64 = 10;

/// Alert delivery worker configuration
pub struct AlertWorkerConfig {
    /// How often to poll for pending deliveries
    pub poll_interval: Duration,
    /// How many deliveries to process per batch
    pub batch_size: i64,
}

impl Default for AlertWorkerConfig {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_secs(DEFAULT_POLL_INTERVAL_SECS),
            batch_size: DEFAULT_BATCH_SIZE,
        }
    }
}

/// Spawn the alert delivery worker as a background task
///
/// The worker will poll for pending deliveries and process them until
/// the shutdown signal is received.
pub fn spawn_alert_worker(
    db: Arc<DatabasePool>,
    conf: hot::val::Val,
    config: AlertWorkerConfig,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    tokio::spawn(async move {
        tracing::info!(
            "Alert delivery worker started (poll interval: {:?}, batch size: {})",
            config.poll_interval,
            config.batch_size
        );

        // Create HTTP client for webhook/Slack/PagerDuty deliveries
        // Use a shorter timeout so we don't block shutdown for too long
        let http_client = reqwest::Client::builder()
            .timeout(Duration::from_secs(HTTP_TIMEOUT_SECS))
            .connect_timeout(Duration::from_secs(5))
            .build()
            .expect("Failed to create HTTP client for alert worker");

        // Create email sender for alert emails using the shared infrastructure.
        let email_sender = EmailSender::alerts_from_conf(&conf);
        let email_config = hot::email::EmailConfig::alerts_from_conf(&conf);
        let email_available = email_sender.is_available();
        if !email_available {
            tracing::warn!("Alert email sender not configured");
        }

        loop {
            // Check for shutdown signal before starting work
            if *shutdown_rx.borrow() {
                tracing::info!("Alert delivery worker received shutdown signal");
                break;
            }

            // Process pending deliveries with a timeout so we can check for shutdown
            // This ensures we don't block shutdown for too long
            // EmailSender implements AlertEmailSender trait, so we can pass it directly
            let email_sender_ref: Option<&dyn hot::db::alert::AlertEmailSender> = if email_available
            {
                Some(&email_sender)
            } else {
                None
            };
            let process_future = process_pending_deliveries(
                &db,
                &http_client,
                hot::outbound::DestinationPolicy::for_alert_delivery(&conf),
                email_sender_ref,
                &email_config,
                config.batch_size,
            );

            tokio::select! {
                biased; // Check shutdown first

                _ = shutdown_rx.changed() => {
                    if *shutdown_rx.borrow() {
                        tracing::info!("Alert delivery worker shutting down (interrupted during processing)");
                        break;
                    }
                }

                result = process_future => {
                    match result {
                        Ok((total, success, failure)) => {
                            if total > 0 {
                                tracing::info!(
                                    "Alert worker processed {} deliveries ({} success, {} failure)",
                                    total,
                                    success,
                                    failure
                                );
                            }
                        }
                        Err(e) => {
                            tracing::error!("Alert worker error: {}", e);
                        }
                    }
                }
            }

            // Check again after processing (in case shutdown came during processing)
            if *shutdown_rx.borrow() {
                tracing::info!("Alert delivery worker shutting down");
                break;
            }

            // Wait for next poll interval or shutdown
            tokio::select! {
                biased; // Check shutdown first

                _ = shutdown_rx.changed() => {
                    if *shutdown_rx.borrow() {
                        tracing::info!("Alert delivery worker shutting down");
                        break;
                    }
                }

                _ = tokio::time::sleep(config.poll_interval) => {}
            }
        }

        tracing::info!("Alert delivery worker stopped");
    });
}

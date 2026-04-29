//! Global notification queue registry
//!
//! Provides process-wide access to the alert and email queues.
//! Set during process startup (worker or app), used by publish_alert()
//! and email enqueue operations to push messages to the queue.

use crate::data::msg::Message;
use crate::env::is_local_dev;
use crate::queue::ProcessingQueue;
use std::sync::{Arc, OnceLock};

static ALERT_QUEUE: OnceLock<Arc<ProcessingQueue<Message>>> = OnceLock::new();
static EMAIL_QUEUE: OnceLock<Arc<ProcessingQueue<Message>>> = OnceLock::new();

/// Initialize the global alert queue (call once during process startup)
pub fn init_alert_queue(queue: Arc<ProcessingQueue<Message>>) {
    if ALERT_QUEUE.set(queue).is_err() {
        if is_local_dev() {
            tracing::debug!("Alert queue already initialized, ignoring duplicate init");
        } else {
            tracing::warn!("Alert queue already initialized, ignoring duplicate init");
        }
    }
}

/// Initialize the global email queue (call once during process startup)
pub fn init_email_queue(queue: Arc<ProcessingQueue<Message>>) {
    if EMAIL_QUEUE.set(queue).is_err() {
        if is_local_dev() {
            tracing::debug!("Email queue already initialized, ignoring duplicate init");
        } else {
            tracing::warn!("Email queue already initialized, ignoring duplicate init");
        }
    }
}

/// Get the global alert queue (None if not initialized)
pub fn alert_queue() -> Option<&'static Arc<ProcessingQueue<Message>>> {
    ALERT_QUEUE.get()
}

/// Get the global email queue (None if not initialized)
pub fn email_queue() -> Option<&'static Arc<ProcessingQueue<Message>>> {
    EMAIL_QUEUE.get()
}

//! Cancellation of a running container task.
//!
//! `::hot::task/cancel` marks the task row cancelled, then publishes
//! `{$cancel: true}` on the task's channel. A code task's VM polls a cancel
//! token; a container has nothing to poll, so the worker watches for the
//! cancel itself and stops the container. The message is only a fast path:
//! the row is read right after subscribing, so a cancel published before the
//! subscription is not missed, and again every two seconds, so a message lost
//! with a pub/sub connection still stops the container.

use hot::db::{DatabasePool, Task, TaskStatus};
use hot::stream::{StreamEvent, StreamNext, StreamPubSub, StreamSubscriberFactory};
use std::sync::Arc;
use tokio::sync::watch;
use uuid::Uuid;

/// How often the task row is read for a cancel the message didn't deliver,
/// as often as a running Docker container's status is polled. (`hot dev`
/// gives its task worker its own in-memory pub/sub, so there the row is the
/// only path.)
const ROW_CHECK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);

pub(crate) struct CancelWatch {
    cancelled: watch::Receiver<bool>,
    watchers: Vec<tokio::task::JoinHandle<()>>,
}

impl CancelWatch {
    /// Start watching `task_id`, which runs in `env_id`, for cancellation.
    pub(crate) fn start(
        db: Arc<DatabasePool>,
        publisher: Arc<StreamPubSub>,
        task_id: Uuid,
        env_id: Uuid,
    ) -> Self {
        let (tx, cancelled) = watch::channel(false);
        let tx = Arc::new(tx);
        let (subscribed_tx, subscribed_rx) = tokio::sync::oneshot::channel::<()>();

        // The subscriber is never raced against a timer: dropping a pending
        // `next()` can lose a message on some backends.
        let message_tx = Arc::clone(&tx);
        let messages = tokio::spawn(async move {
            let mut subscription = match publisher.subscribe(task_id).await {
                Ok(subscription) => subscription,
                Err(error) => {
                    tracing::warn!(%task_id, %error, "Cannot subscribe to container task messages; watching its row for a cancel");
                    let _ = subscribed_tx.send(());
                    return;
                }
            };
            let _ = subscribed_tx.send(());
            loop {
                match subscription.next().await {
                    StreamNext::Event(StreamEvent::TaskMessage {
                        env_id: sender_env,
                        payload,
                        ..
                    }) if sender_env == Some(env_id) && is_cancel_message(&payload) => {
                        let _ = message_tx.send(true);
                        return;
                    }
                    StreamNext::Event(_) | StreamNext::Idle => {}
                    StreamNext::Closed => return,
                }
            }
        });

        let rows = tokio::spawn(async move {
            // Read the row only once the subscription exists, so a cancel is
            // either already in the row or still to arrive as a message.
            let _ = subscribed_rx.await;
            loop {
                if row_is_cancelled(&db, &task_id).await {
                    let _ = tx.send(true);
                    return;
                }
                tokio::time::sleep(ROW_CHECK_INTERVAL).await;
            }
        });

        Self {
            cancelled,
            watchers: vec![messages, rows],
        }
    }

    /// Resolves once the task is cancelled; never resolves otherwise.
    pub(crate) async fn cancelled(&self) {
        let mut cancelled = self.cancelled.clone();
        if cancelled.wait_for(|cancelled| *cancelled).await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

impl Drop for CancelWatch {
    fn drop(&mut self) {
        for watcher in &self.watchers {
            watcher.abort();
        }
    }
}

fn is_cancel_message(payload: &serde_json::Value) -> bool {
    payload
        .get("$cancel")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
}

/// Whether the task's row says it was cancelled. A read that fails says no.
pub(crate) async fn row_is_cancelled(db: &DatabasePool, task_id: &Uuid) -> bool {
    matches!(
        tokio::time::timeout(super::DB_CALL_TIMEOUT, Task::get(db, task_id)).await,
        Ok(Ok(task)) if task.task_status_id == TaskStatus::Cancelled.as_id()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use hot::stream::{StreamPubSubType, StreamPublisher};

    async fn running_task(db: &DatabasePool) -> (Uuid, Uuid) {
        let task_id = Uuid::now_v7();
        let env_id = Uuid::now_v7();
        Task::insert(
            db,
            &task_id,
            &env_id,
            &Uuid::now_v7(),
            &Uuid::now_v7(),
            None,
            "::hot::box/start",
            None,
            None,
            "container",
            60_000,
            None,
        )
        .await
        .unwrap();
        assert!(Task::claim_for_worker(db, &task_id, "owner").await.unwrap());
        (task_id, env_id)
    }

    fn cancel_message(task_id: Uuid, env_id: Uuid) -> StreamEvent {
        StreamEvent::TaskMessage {
            task_id: task_id.to_string(),
            env_id: Some(env_id),
            payload: serde_json::json!({"$cancel": true}),
        }
    }

    async fn wait_cancelled(watch: &CancelWatch) -> bool {
        tokio::time::timeout(std::time::Duration::from_secs(2), watch.cancelled())
            .await
            .is_ok()
    }

    #[tokio::test]
    async fn cancel_message_from_the_task_environment_stops_the_container() {
        let db = Arc::new(hot::db::test_db().await);
        let publisher = Arc::new(StreamPubSub::new(StreamPubSubType::Memory, None, false).unwrap());
        let (task_id, env_id) = running_task(&db).await;
        let watch = CancelWatch::start(Arc::clone(&db), Arc::clone(&publisher), task_id, env_id);
        // Let the subscription and the first row read land before the cancel.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        publisher
            .publish(cancel_message(task_id, Uuid::now_v7()))
            .await
            .unwrap();
        assert!(
            !wait_cancelled(&watch).await,
            "a cancel message from another environment is ignored"
        );

        publisher
            .publish(cancel_message(task_id, env_id))
            .await
            .unwrap();
        assert!(wait_cancelled(&watch).await);
    }

    #[tokio::test]
    async fn a_cancel_written_before_the_watch_started_is_seen_in_the_row() {
        let db = Arc::new(hot::db::test_db().await);
        let publisher = Arc::new(StreamPubSub::new(StreamPubSubType::Memory, None, false).unwrap());
        let (task_id, env_id) = running_task(&db).await;
        assert!(Task::cancel(&db, &task_id, &env_id).await.unwrap());

        let watch = CancelWatch::start(db, publisher, task_id, env_id);
        assert!(wait_cancelled(&watch).await);
    }

    #[tokio::test]
    async fn an_uncancelled_task_is_not_reported_cancelled() {
        let db = Arc::new(hot::db::test_db().await);
        let publisher = Arc::new(StreamPubSub::new(StreamPubSubType::Memory, None, false).unwrap());
        let (task_id, env_id) = running_task(&db).await;
        let watch = CancelWatch::start(db, publisher, task_id, env_id);
        assert!(!wait_cancelled(&watch).await);
    }
}

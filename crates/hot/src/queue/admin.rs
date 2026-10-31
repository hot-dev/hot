//! Operator access to the standard queues, the same on every durable backend.
//!
//! `hot queue status` and `hot queue clear` work through [`QueueAdmin`], so a
//! new backend adds one implementation here instead of a branch per command.
//! Memory queues live inside their process and have no admin.

use super::sqlite::SqliteQueueAdmin;
use super::{QueueStatusSummary, RedisQueueAdmin};
use async_trait::async_trait;

#[async_trait]
pub trait QueueAdmin: Send + Sync {
    /// The backend's name for messages, such as "SQLite".
    fn backend_name(&self) -> &'static str;

    /// Where the queues live, with any credentials redacted.
    fn location(&self) -> String;

    /// Waiting, processing, and dead-lettered counts for each queue.
    async fn status(&self) -> Result<QueueStatusSummary, String>;

    /// Remove every waiting, processing, and dead-lettered message. Returns
    /// the queues and dead-letter stores that held any.
    async fn clear_all(&self) -> Result<Vec<String>, String>;
}

#[async_trait]
impl QueueAdmin for SqliteQueueAdmin {
    fn backend_name(&self) -> &'static str {
        "SQLite"
    }

    fn location(&self) -> String {
        self.directory().display().to_string()
    }

    async fn status(&self) -> Result<QueueStatusSummary, String> {
        SqliteQueueAdmin::status(self)
            .await
            .map_err(|e| e.to_string())
    }

    async fn clear_all(&self) -> Result<Vec<String>, String> {
        SqliteQueueAdmin::clear_all(self)
            .await
            .map_err(|e| e.to_string())
    }
}

#[async_trait]
impl QueueAdmin for RedisQueueAdmin {
    fn backend_name(&self) -> &'static str {
        "Redis"
    }

    fn location(&self) -> String {
        let uri = crate::db::redact_password(self.uri());
        if self.is_cluster() {
            format!("{} [cluster]", uri)
        } else {
            uri
        }
    }

    async fn status(&self) -> Result<QueueStatusSummary, String> {
        RedisQueueAdmin::status(self)
    }

    async fn clear_all(&self) -> Result<Vec<String>, String> {
        RedisQueueAdmin::clear_all(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::serialization::Serialization;
    use crate::queue::Queue;
    use crate::queue::sqlite::SqliteQueue;

    #[tokio::test]
    async fn sqlite_admin_reports_and_clears_through_the_trait() {
        let temp = tempfile::tempdir().unwrap();
        let queue = SqliteQueue::<String>::new_at("hot:event".to_string(), temp.path().into())
            .unwrap()
            .with_serialization(Serialization::Json);
        queue.enqueue("waiting".to_string()).await.unwrap();

        let admin: Box<dyn QueueAdmin> = Box::new(SqliteQueueAdmin::new(temp.path().into()));
        assert_eq!(admin.backend_name(), "SQLite");
        assert_eq!(admin.location(), temp.path().display().to_string());
        assert_eq!(admin.status().await.unwrap().total_pending, 1);

        assert_eq!(admin.clear_all().await.unwrap(), ["hot:event"]);
        assert_eq!(admin.status().await.unwrap().total_pending, 0);
    }

    #[test]
    fn redis_admin_location_hides_the_password() {
        let admin = RedisQueueAdmin::new("redis://:secret@cache:6379/".to_string(), true);
        assert_eq!(admin.backend_name(), "Redis");
        let location = admin.location();
        assert!(!location.contains("secret"), "{location}");
        assert!(location.ends_with(" [cluster]"), "{location}");
    }
}

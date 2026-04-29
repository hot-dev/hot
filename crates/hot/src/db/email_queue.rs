//! Email queue database module
//!
//! Provides audit trail for app emails processed through the hot:email queue.
//! The queue drives processing; this table records what was sent/failed.

use chrono::{DateTime, Utc};
use sqlx::FromRow;
use uuid::Uuid;

// =============================================================================
// Email Queue Status
// =============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i16)]
pub enum EmailQueueStatus {
    Pending = 1,
    Sent = 2,
    Failed = 3,
}

// =============================================================================
// Email Queue Entry
// =============================================================================

#[derive(Debug, Clone, FromRow)]
pub struct EmailQueueEntry {
    pub email_queue_id: Uuid,
    pub to_address: String,
    pub subject: String,
    pub html_body: Option<String>,
    pub text_body: Option<String>,
    pub from_address: String,
    pub status_id: i16,
    pub error_message: Option<String>,
    pub created_at: DateTime<Utc>,
    pub sent_at: Option<DateTime<Utc>>,
    pub updated_at: DateTime<Utc>,
}

// =============================================================================
// Error Type
// =============================================================================

#[derive(Debug)]
pub enum EmailQueueError {
    Database(sqlx::Error),
    NotFound,
}

impl std::fmt::Display for EmailQueueError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EmailQueueError::Database(e) => write!(f, "Database error: {}", e),
            EmailQueueError::NotFound => write!(f, "Email queue entry not found"),
        }
    }
}

impl std::error::Error for EmailQueueError {}

impl From<sqlx::Error> for EmailQueueError {
    fn from(e: sqlx::Error) -> Self {
        EmailQueueError::Database(e)
    }
}

// =============================================================================
// CRUD Operations
// =============================================================================

impl EmailQueueEntry {
    /// Enqueue a new email for sending. Returns the email_queue_id.
    pub async fn enqueue(
        db: &crate::db::DatabasePool,
        to_address: &str,
        subject: &str,
        html_body: Option<&str>,
        text_body: Option<&str>,
        from_address: &str,
    ) -> Result<Uuid, EmailQueueError> {
        let id = Uuid::now_v7();

        match db {
            crate::db::DatabasePool::Postgres(pool) => {
                sqlx::query(
                    "INSERT INTO email_queue (email_queue_id, to_address, subject, html_body, text_body, from_address)
                     VALUES ($1, $2, $3, $4, $5, $6)"
                )
                .bind(id)
                .bind(to_address)
                .bind(subject)
                .bind(html_body)
                .bind(text_body)
                .bind(from_address)
                .execute(pool)
                .await?;
            }
            crate::db::DatabasePool::Sqlite(pool) => {
                sqlx::query(
                    "INSERT INTO email_queue (email_queue_id, to_address, subject, html_body, text_body, from_address)
                     VALUES (?, ?, ?, ?, ?, ?)"
                )
                .bind(id)
                .bind(to_address)
                .bind(subject)
                .bind(html_body)
                .bind(text_body)
                .bind(from_address)
                .execute(pool)
                .await?;
            }
        }

        Ok(id)
    }

    /// Mark an email as successfully sent
    pub async fn mark_sent(
        db: &crate::db::DatabasePool,
        email_queue_id: &Uuid,
    ) -> Result<(), EmailQueueError> {
        match db {
            crate::db::DatabasePool::Postgres(pool) => {
                sqlx::query(
                    "UPDATE email_queue SET status_id = $1, sent_at = NOW(), updated_at = NOW() WHERE email_queue_id = $2"
                )
                .bind(EmailQueueStatus::Sent as i16)
                .bind(email_queue_id)
                .execute(pool)
                .await?;
            }
            crate::db::DatabasePool::Sqlite(pool) => {
                sqlx::query(
                    "UPDATE email_queue SET status_id = ?, sent_at = datetime('now'), updated_at = datetime('now') WHERE email_queue_id = ?"
                )
                .bind(EmailQueueStatus::Sent as i16)
                .bind(email_queue_id)
                .execute(pool)
                .await?;
            }
        }

        Ok(())
    }

    /// Get an email queue entry by ID (used for testing and status checks)
    pub async fn get_by_id(
        db: &crate::db::DatabasePool,
        email_queue_id: &Uuid,
    ) -> Result<EmailQueueEntry, EmailQueueError> {
        match db {
            crate::db::DatabasePool::Postgres(pool) => sqlx::query_as::<_, EmailQueueEntry>(
                "SELECT * FROM email_queue WHERE email_queue_id = $1",
            )
            .bind(email_queue_id)
            .fetch_optional(pool)
            .await?
            .ok_or(EmailQueueError::NotFound),
            crate::db::DatabasePool::Sqlite(pool) => sqlx::query_as::<_, EmailQueueEntry>(
                "SELECT * FROM email_queue WHERE email_queue_id = ?",
            )
            .bind(email_queue_id)
            .fetch_optional(pool)
            .await?
            .ok_or(EmailQueueError::NotFound),
        }
    }

    /// Mark an email as failed
    pub async fn mark_failed(
        db: &crate::db::DatabasePool,
        email_queue_id: &Uuid,
        error: &str,
    ) -> Result<(), EmailQueueError> {
        match db {
            crate::db::DatabasePool::Postgres(pool) => {
                sqlx::query(
                    "UPDATE email_queue SET status_id = $1, error_message = $2, updated_at = NOW() WHERE email_queue_id = $3"
                )
                .bind(EmailQueueStatus::Failed as i16)
                .bind(error)
                .bind(email_queue_id)
                .execute(pool)
                .await?;
            }
            crate::db::DatabasePool::Sqlite(pool) => {
                sqlx::query(
                    "UPDATE email_queue SET status_id = ?, error_message = ?, updated_at = datetime('now') WHERE email_queue_id = ?"
                )
                .bind(EmailQueueStatus::Failed as i16)
                .bind(error)
                .bind(email_queue_id)
                .execute(pool)
                .await?;
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn setup_test_db() -> crate::db::DatabasePool {
        crate::db::test_db().await
    }

    #[tokio::test]
    async fn test_enqueue_email() {
        let db = setup_test_db().await;

        let id = EmailQueueEntry::enqueue(
            &db,
            "user@example.com",
            "Test Subject",
            Some("<p>Hello</p>"),
            Some("Hello"),
            "Hot Dev <hi@notifications.hot.dev>",
        )
        .await
        .unwrap();

        // Verify the entry was created
        let entry = EmailQueueEntry::get_by_id(&db, &id).await.unwrap();
        assert_eq!(entry.to_address, "user@example.com");
        assert_eq!(entry.subject, "Test Subject");
        assert_eq!(entry.html_body, Some("<p>Hello</p>".to_string()));
        assert_eq!(entry.text_body, Some("Hello".to_string()));
        assert_eq!(entry.from_address, "Hot Dev <hi@notifications.hot.dev>");
        assert_eq!(entry.status_id, EmailQueueStatus::Pending as i16);
        assert!(entry.error_message.is_none());
        assert!(entry.sent_at.is_none());
    }

    #[tokio::test]
    async fn test_enqueue_email_no_body() {
        let db = setup_test_db().await;

        let id = EmailQueueEntry::enqueue(
            &db,
            "user@example.com",
            "No Body",
            None,
            None,
            "test@example.com",
        )
        .await
        .unwrap();

        let entry = EmailQueueEntry::get_by_id(&db, &id).await.unwrap();
        assert_eq!(entry.html_body, None);
        assert_eq!(entry.text_body, None);
    }

    #[tokio::test]
    async fn test_mark_sent() {
        let db = setup_test_db().await;

        let id = EmailQueueEntry::enqueue(
            &db,
            "user@example.com",
            "Test",
            Some("<p>Hi</p>"),
            None,
            "from@example.com",
        )
        .await
        .unwrap();

        // Mark as sent
        EmailQueueEntry::mark_sent(&db, &id).await.unwrap();

        // Verify status changed
        let entry = EmailQueueEntry::get_by_id(&db, &id).await.unwrap();
        assert_eq!(entry.status_id, EmailQueueStatus::Sent as i16);
        assert!(entry.sent_at.is_some());
        assert!(entry.error_message.is_none());
    }

    #[tokio::test]
    async fn test_mark_failed() {
        let db = setup_test_db().await;

        let id = EmailQueueEntry::enqueue(
            &db,
            "user@example.com",
            "Test",
            Some("<p>Hi</p>"),
            None,
            "from@example.com",
        )
        .await
        .unwrap();

        // Mark as failed
        EmailQueueEntry::mark_failed(&db, &id, "Connection timeout")
            .await
            .unwrap();

        // Verify status changed
        let entry = EmailQueueEntry::get_by_id(&db, &id).await.unwrap();
        assert_eq!(entry.status_id, EmailQueueStatus::Failed as i16);
        assert_eq!(entry.error_message, Some("Connection timeout".to_string()));
        assert!(entry.sent_at.is_none());
    }

    #[tokio::test]
    async fn test_get_by_id_not_found() {
        let db = setup_test_db().await;

        let result = EmailQueueEntry::get_by_id(&db, &Uuid::now_v7()).await;
        assert!(matches!(result, Err(EmailQueueError::NotFound)));
    }
}

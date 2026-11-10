//! Email queue database module
//!
//! Provides audit trail for app emails processed through the hot:email queue.
//! The queue drives processing; this table records what was sent/failed.
//!
//! Two parties end an email as `failed`, and no other:
//!
//! - the delivery that holds its send claim, when the send is rejected or the
//!   sender is unavailable ([`EmailQueueEntry::mark_failed`]), or when its
//!   message is dead-lettered before it could record the outcome
//!   ([`EmailQueueEntry::fail_sending`]);
//! - the recovery sweep, for an email still unsent past [`RECOVERY_WINDOW`]
//!   ([`EmailQueueEntry::settle_stale`]).
//!
//! A send that fails for a reason that may pass (the relay is down, refuses
//! the login, or times out) fails nothing: the email is `pending` again
//! ([`EmailQueueEntry::release_send`]). Its message retries a few times
//! within minutes; after that the sweep enqueues the email again, each time
//! later, until the window ends. A queue message that is dead-lettered without
//! the claim fails nothing either.

use chrono::{DateTime, Utc};
use sqlx::FromRow;
use uuid::Uuid;

// =============================================================================
// Email Queue Status
// =============================================================================

/// Where an app email stands. The values are stored in `email_queue.status_id`,
/// a plain smallint with no lookup table or CHECK constraint.
///
/// ```text
/// pending --claim_send--> sending --mark_sent-----> sent
///    ^                       |-----mark_failed---> failed   (rejected, or no sender)
///    '-----release_send------'                              (transient failure)
/// ```
///
/// Only the delivery that holds the send claim leaves `sending`. A `pending`
/// email belongs to the recovery sweep whenever no delivery holds it: the
/// sweep enqueues an overdue one again ([`EmailQueueEntry::claim_overdue`]),
/// whether its message was never enqueued, was lost, or was dead-lettered. The
/// sweep also returns an email stuck in `sending` to `pending`, and fails
/// either once it is older than [`RECOVERY_WINDOW`]
/// ([`EmailQueueEntry::settle_stale`]), so every email ends `sent` or `failed`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i16)]
pub enum EmailQueueStatus {
    Pending = 1,
    Sent = 2,
    Failed = 3,
    /// One delivery holds the email and is sending it
    /// (see [`EmailQueueEntry::claim_send`]).
    Sending = 4,
}

/// How long a pending email is left to its own queue message before the
/// recovery sweep enqueues it again (see [`EmailQueueEntry::claim_overdue`]).
/// Several messages can then carry one email, during a backlog on `hot:email`
/// or a retry backoff; the worker's send claim lets only one of them send.
pub const RECOVERY_GRACE: chrono::Duration = chrono::Duration::minutes(10);
/// An email older than this is no longer recovered but failed: mail this late
/// is stale, and nothing here counts the attempts of one that can never be
/// sent.
pub const RECOVERY_WINDOW: chrono::Duration = chrono::Duration::hours(6);
/// The longest a worker lets one send run before it counts as failed. The SMTP
/// timeout (`email.smtp.timeout-seconds`, 30 s by default) bounds each step of
/// a send, not the send, so the worker bounds the whole of it.
pub const SEND_DEADLINE: std::time::Duration = std::time::Duration::from_secs(5 * 60);
/// How long an email stays `sending` before the sweep takes its worker for
/// dead and returns it to `pending`. A live worker settles the row within
/// `SEND_DEADLINE` plus its few retries of the write, well inside this.
pub const SENDING_STALE_AFTER: chrono::Duration = chrono::Duration::minutes(20);
const _: () = assert!(SENDING_STALE_AFTER.num_seconds() as u64 >= 3 * SEND_DEADLINE.as_secs());

/// What [`EmailQueueEntry::settle_stale`] changed.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct StaleEmails {
    /// Emails older than `RECOVERY_WINDOW`, now failed.
    pub failed: u64,
    /// Emails stuck in `sending`, now pending and overdue.
    pub released: u64,
}

const EXPIRED_PENDING_ERROR: &str = "Not sent within the recovery window: its queue messages were lost, dead-lettered, or kept failing";
const EXPIRED_SENDING_ERROR: &str = "Left in sending past the recovery window: its send was interrupted or its outcome was not recorded";
/// Joins an expiry reason to the error the email's last failed send left.
const LAST_SEND_ERROR_PREFIX: &str = "; last send error: ";

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
        // The outbox is platform infrastructure: tenant request scopes cannot
        // insert into it, so every enqueue runs as the email operation.
        let db = &db.with_scope(crate::db::DatabaseScope::Maintenance {
            operation: crate::db::MaintenanceOperation::Email,
            org_id: None,
        });

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

    /// Claim a pending email for sending: the one statement moves it to
    /// `sending` and says whether this caller won. Only the winner sends, so
    /// the several queue messages one email can have (see `RECOVERY_GRACE`)
    /// send it once. The winner then settles the row with `mark_sent`,
    /// `mark_failed`, or `release_send`.
    ///
    /// A worker that dies between the claim and the send leaves the row
    /// `sending`; the sweep returns it to `pending` after
    /// `SENDING_STALE_AFTER` (see `settle_stale`). An email is therefore sent
    /// twice only when a worker died, or lost its database, after the relay
    /// took the mail and before `mark_sent`: delivery is at least once.
    pub async fn claim_send(
        db: &crate::db::DatabasePool,
        email_queue_id: &Uuid,
    ) -> Result<bool, EmailQueueError> {
        let claimed = match db {
            crate::db::DatabasePool::Postgres(pool) => sqlx::query(
                "UPDATE email_queue SET status_id = $1, updated_at = NOW() WHERE email_queue_id = $2 AND status_id = $3",
            )
            .bind(EmailQueueStatus::Sending as i16)
            .bind(email_queue_id)
            .bind(EmailQueueStatus::Pending as i16)
            .execute(pool)
            .await?
            .rows_affected(),
            crate::db::DatabasePool::Sqlite(pool) => sqlx::query(
                "UPDATE email_queue SET status_id = ?, updated_at = datetime('now') WHERE email_queue_id = ? AND status_id = ?",
            )
            .bind(EmailQueueStatus::Sending as i16)
            .bind(email_queue_id)
            .bind(EmailQueueStatus::Pending as i16)
            .execute(pool)
            .await?
            .rows_affected(),
        };
        Ok(claimed == 1)
    }

    /// Give up a send claim after a failure that may pass: the email is
    /// `pending` again, with `error` on it, for the message's retry delivery
    /// to claim or, once the message is out of attempts, for the sweep to
    /// enqueue again. Its recovery grace starts over, so the sweep leaves a
    /// retry its time, and waits as long as the email has already been
    /// pending (see `claim_overdue`).
    pub async fn release_send(
        db: &crate::db::DatabasePool,
        email_queue_id: &Uuid,
        error: &str,
    ) -> Result<(), EmailQueueError> {
        match db {
            crate::db::DatabasePool::Postgres(pool) => {
                sqlx::query(
                    "UPDATE email_queue SET status_id = $1, error_message = $2, updated_at = NOW() WHERE email_queue_id = $3 AND status_id = $4",
                )
                .bind(EmailQueueStatus::Pending as i16)
                .bind(error)
                .bind(email_queue_id)
                .bind(EmailQueueStatus::Sending as i16)
                .execute(pool)
                .await?;
            }
            crate::db::DatabasePool::Sqlite(pool) => {
                sqlx::query(
                    "UPDATE email_queue SET status_id = ?, error_message = ?, updated_at = datetime('now') WHERE email_queue_id = ? AND status_id = ?",
                )
                .bind(EmailQueueStatus::Pending as i16)
                .bind(error)
                .bind(email_queue_id)
                .bind(EmailQueueStatus::Sending as i16)
                .execute(pool)
                .await?;
            }
        }
        Ok(())
    }

    /// Fail an email its own delivery left in `sending`: the caller holds the
    /// send claim, could not record the send's outcome, and its queue message
    /// was dead-lettered. Says whether the email was failed.
    ///
    /// Only `sending` is failed, and only for the claim's holder. A dead-letter
    /// that holds no claim fails nothing: a `pending` email can be between the
    /// retries of another message for it, which would then skip an email it
    /// could have sent. The sweep owns a `pending` email either way: it
    /// enqueues it again while it is overdue and fails it past
    /// `RECOVERY_WINDOW` (see `claim_overdue` and `settle_stale`).
    pub async fn fail_sending(
        db: &crate::db::DatabasePool,
        email_queue_id: &Uuid,
        error: &str,
    ) -> Result<bool, EmailQueueError> {
        let failed = match db {
            crate::db::DatabasePool::Postgres(pool) => sqlx::query(
                "UPDATE email_queue SET status_id = $1, error_message = $2, updated_at = NOW() WHERE email_queue_id = $3 AND status_id = $4",
            )
            .bind(EmailQueueStatus::Failed as i16)
            .bind(error)
            .bind(email_queue_id)
            .bind(EmailQueueStatus::Sending as i16)
            .execute(pool)
            .await?
            .rows_affected(),
            crate::db::DatabasePool::Sqlite(pool) => sqlx::query(
                "UPDATE email_queue SET status_id = ?, error_message = ?, updated_at = datetime('now') WHERE email_queue_id = ? AND status_id = ?",
            )
            .bind(EmailQueueStatus::Failed as i16)
            .bind(error)
            .bind(email_queue_id)
            .bind(EmailQueueStatus::Sending as i16)
            .execute(pool)
            .await?
            .rows_affected(),
        };
        Ok(failed == 1)
    }

    /// The sweep's housekeeping, as of `now`:
    ///
    /// - an email older than `RECOVERY_WINDOW` that is still `pending` (and
    ///   was last touched more than `RECOVERY_GRACE` ago, so a message just
    ///   enqueued for it has had its time) or stuck in `sending` is failed
    ///   with the reason, and the error its last failed send left, instead of
    ///   staying unsent for good. This is the only
    ///   place a `pending` email fails: a dead-lettered queue message leaves
    ///   it to `claim_overdue` and, at the end, to this;
    /// - a younger email that has been `sending` for `SENDING_STALE_AFTER`,
    ///   which no live worker takes (see the constant), is `pending` again and
    ///   overdue at once, for `claim_overdue` to pick up. If its worker did
    ///   hand the mail to the relay before it died, this sends it twice.
    ///
    /// Both statements are idempotent, so sweeps may overlap. On PostgreSQL
    /// `email_queue_sending_idx` covers the `sending` rows; SQLite scans the
    /// table, so call this every minute or so, not every tick.
    pub async fn settle_stale(
        db: &crate::db::DatabasePool,
        now: DateTime<Utc>,
    ) -> Result<StaleEmails, EmailQueueError> {
        let db = &db.with_scope(crate::db::DatabaseScope::Maintenance {
            operation: crate::db::MaintenanceOperation::Email,
            org_id: None,
        });
        let (failed, released) = match db {
            crate::db::DatabasePool::Postgres(pool) => {
                let failed = sqlx::query(
                    "UPDATE email_queue SET status_id = $2,
                         error_message = (CASE WHEN status_id = $3 THEN $5::text ELSE $6::text END)
                             || COALESCE($10::text || error_message, ''),
                         updated_at = $1
                     WHERE created_at < $7
                       AND ((status_id = $3 AND updated_at <= $8)
                         OR (status_id = $4 AND updated_at <= $9))",
                )
                .bind(now)
                .bind(EmailQueueStatus::Failed as i16)
                .bind(EmailQueueStatus::Pending as i16)
                .bind(EmailQueueStatus::Sending as i16)
                .bind(EXPIRED_PENDING_ERROR)
                .bind(EXPIRED_SENDING_ERROR)
                .bind(now - RECOVERY_WINDOW)
                .bind(now - RECOVERY_GRACE)
                .bind(now - SENDING_STALE_AFTER)
                .bind(LAST_SEND_ERROR_PREFIX)
                .execute(pool)
                .await?
                .rows_affected();
                let released = sqlx::query(
                    "UPDATE email_queue SET status_id = $1, updated_at = created_at
                     WHERE status_id = $2 AND updated_at <= $3",
                )
                .bind(EmailQueueStatus::Pending as i16)
                .bind(EmailQueueStatus::Sending as i16)
                .bind(now - SENDING_STALE_AFTER)
                .execute(pool)
                .await?
                .rows_affected();
                (failed, released)
            }
            crate::db::DatabasePool::Sqlite(pool) => {
                // Times are compared as instants: see `claim_overdue`.
                let days = |span: chrono::Duration| span.num_seconds() as f64 / 86_400.0;
                let now = now.format("%Y-%m-%d %H:%M:%S").to_string();
                let failed = sqlx::query(
                    "UPDATE email_queue SET status_id = ?2,
                         error_message = (CASE WHEN status_id = ?3 THEN ?5 ELSE ?6 END)
                             || COALESCE(?10 || error_message, ''),
                         updated_at = ?1
                     WHERE julianday(?1) - julianday(created_at) > ?7
                       AND ((status_id = ?3 AND julianday(?1) - julianday(updated_at) >= ?8)
                         OR (status_id = ?4 AND julianday(?1) - julianday(updated_at) >= ?9))",
                )
                .bind(&now)
                .bind(EmailQueueStatus::Failed as i16)
                .bind(EmailQueueStatus::Pending as i16)
                .bind(EmailQueueStatus::Sending as i16)
                .bind(EXPIRED_PENDING_ERROR)
                .bind(EXPIRED_SENDING_ERROR)
                .bind(days(RECOVERY_WINDOW))
                .bind(days(RECOVERY_GRACE))
                .bind(days(SENDING_STALE_AFTER))
                .bind(LAST_SEND_ERROR_PREFIX)
                .execute(pool)
                .await?
                .rows_affected();
                let released = sqlx::query(
                    "UPDATE email_queue SET status_id = ?2, updated_at = created_at
                     WHERE status_id = ?3 AND julianday(?1) - julianday(updated_at) >= ?4",
                )
                .bind(&now)
                .bind(EmailQueueStatus::Pending as i16)
                .bind(EmailQueueStatus::Sending as i16)
                .bind(days(SENDING_STALE_AFTER))
                .execute(pool)
                .await?
                .rows_affected();
                (failed, released)
            }
        };
        Ok(StaleEmails { failed, released })
    }

    /// Undo `claim_overdue` for an email whose enqueue then failed: it is
    /// overdue again at once, and the failed pass cost it no backoff (its next
    /// wait is still measured from when it was written).
    pub async fn unclaim_overdue(
        db: &crate::db::DatabasePool,
        email_queue_id: &Uuid,
    ) -> Result<(), EmailQueueError> {
        let db = &db.with_scope(crate::db::DatabaseScope::Maintenance {
            operation: crate::db::MaintenanceOperation::Email,
            org_id: None,
        });
        match db {
            crate::db::DatabasePool::Postgres(pool) => {
                sqlx::query(
                    "UPDATE email_queue SET updated_at = created_at WHERE email_queue_id = $1 AND status_id = $2",
                )
                .bind(email_queue_id)
                .bind(EmailQueueStatus::Pending as i16)
                .execute(pool)
                .await?;
            }
            crate::db::DatabasePool::Sqlite(pool) => {
                sqlx::query(
                    "UPDATE email_queue SET updated_at = created_at WHERE email_queue_id = ? AND status_id = ?",
                )
                .bind(email_queue_id)
                .bind(EmailQueueStatus::Pending as i16)
                .execute(pool)
                .await?;
            }
        }
        Ok(())
    }

    /// Claim up to `limit` pending emails whose queue message is overdue, for
    /// the caller to enqueue again. An email is written here before it is
    /// enqueued, so one whose enqueue failed, or whose message was dead
    /// lettered (which fails no `pending` email, see `fail_sending`), is still
    /// `pending` with nothing left to send it.
    ///
    /// The claim moves `updated_at` to `now` in the one statement that picks
    /// the rows, so concurrent sweeps get disjoint emails. A claimed email is
    /// due again only after it has waited as long as it had already been
    /// pending (and never less than `RECOVERY_GRACE`), which spaces the retries
    /// of an email that cannot be sent out to a handful before
    /// `RECOVERY_WINDOW` ends them. A caller whose enqueue fails gives the
    /// claim back with `unclaim_overdue`.
    pub async fn claim_overdue(
        db: &crate::db::DatabasePool,
        now: DateTime<Utc>,
        limit: i64,
    ) -> Result<Vec<EmailQueueEntry>, EmailQueueError> {
        let db = &db.with_scope(crate::db::DatabaseScope::Maintenance {
            operation: crate::db::MaintenanceOperation::Email,
            org_id: None,
        });
        let entries = match db {
            crate::db::DatabasePool::Postgres(pool) => {
                sqlx::query_as::<_, EmailQueueEntry>(
                    "UPDATE email_queue SET updated_at = $1
                     WHERE email_queue_id IN (
                         SELECT email_queue_id FROM email_queue
                         WHERE status_id = $2 AND created_at >= $3
                           AND updated_at <= $1 - GREATEST($4 * interval '1 second', updated_at - created_at)
                         ORDER BY created_at LIMIT $5
                         FOR UPDATE SKIP LOCKED)
                     RETURNING *",
                )
                .bind(now)
                .bind(EmailQueueStatus::Pending as i16)
                .bind(now - RECOVERY_WINDOW)
                .bind(RECOVERY_GRACE.num_seconds() as f64)
                .bind(limit)
                .fetch_all(pool)
                .await?
            }
            crate::db::DatabasePool::Sqlite(pool) => {
                // One statement, so one writer: no other sweep reads between
                // the pick and the claim. Times are compared as instants
                // because the column defaults and bound values differ as text.
                const DAY_SECONDS: f64 = 86_400.0;
                sqlx::query_as::<_, EmailQueueEntry>(
                    "UPDATE email_queue SET updated_at = ?1
                     WHERE email_queue_id IN (
                         SELECT email_queue_id FROM email_queue
                         WHERE status_id = ?2 AND julianday(created_at) >= julianday(?1) - ?3
                           AND julianday(?1) - julianday(updated_at) >= MAX(?4, julianday(updated_at) - julianday(created_at))
                         ORDER BY created_at LIMIT ?5)
                     RETURNING *",
                )
                .bind(now.format("%Y-%m-%d %H:%M:%S").to_string())
                .bind(EmailQueueStatus::Pending as i16)
                .bind(RECOVERY_WINDOW.num_seconds() as f64 / DAY_SECONDS)
                .bind(RECOVERY_GRACE.num_seconds() as f64 / DAY_SECONDS)
                .bind(limit)
                .fetch_all(pool)
                .await?
            }
        };
        Ok(entries)
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
    async fn overdue_pending_emails_are_claimed_once_and_backed_off() {
        let db = setup_test_db().await;
        let enqueue = async |to: &'static str| {
            EmailQueueEntry::enqueue(&db, to, "Subject", None, Some("Hello"), "from@example.com")
                .await
                .unwrap()
        };
        let lost = enqueue("lost@example.com").await;
        let sent = enqueue("sent@example.com").await;
        EmailQueueEntry::mark_sent(&db, &sent).await.unwrap();
        let failed = enqueue("failed@example.com").await;
        EmailQueueEntry::mark_failed(&db, &failed, "no sender")
            .await
            .unwrap();

        let created = Utc::now();
        let claim = async |after: chrono::Duration| {
            EmailQueueEntry::claim_overdue(&db, created + after, 10)
                .await
                .unwrap()
                .into_iter()
                .map(|entry| entry.email_queue_id)
                .collect::<Vec<_>>()
        };
        let minutes = chrono::Duration::minutes;

        // Its own queue message may still be on its way.
        assert!(claim(minutes(5)).await.is_empty());
        // Overdue: one sweep gets it, and a second one running beside it does not.
        assert_eq!(claim(minutes(11)).await, vec![lost]);
        assert!(claim(minutes(11)).await.is_empty());
        // It then waits as long again as it had been pending.
        assert!(claim(minutes(20)).await.is_empty());
        assert_eq!(claim(minutes(23)).await, vec![lost]);
        // Mail this late is not sent at all.
        assert!(claim(RECOVERY_WINDOW + minutes(1)).await.is_empty());
    }

    async fn status_of(db: &crate::db::DatabasePool, id: &Uuid) -> i16 {
        EmailQueueEntry::get_by_id(db, id).await.unwrap().status_id
    }

    #[tokio::test]
    async fn one_send_claim_wins_and_the_row_follows_the_send() {
        let db = setup_test_db().await;
        let id = EmailQueueEntry::enqueue(&db, "a@example.com", "S", None, None, "f@example.com")
            .await
            .unwrap();

        // Two deliveries of one email: the second claim loses.
        let (one, two) = tokio::join!(
            EmailQueueEntry::claim_send(&db, &id),
            EmailQueueEntry::claim_send(&db, &id)
        );
        assert_eq!(usize::from(one.unwrap()) + usize::from(two.unwrap()), 1);
        assert_eq!(status_of(&db, &id).await, EmailQueueStatus::Sending as i16);
        assert!(!EmailQueueEntry::claim_send(&db, &id).await.unwrap());

        // A transient failure gives the claim back for the retry to take.
        EmailQueueEntry::release_send(&db, &id, "relay down")
            .await
            .unwrap();
        let entry = EmailQueueEntry::get_by_id(&db, &id).await.unwrap();
        assert_eq!(entry.status_id, EmailQueueStatus::Pending as i16);
        assert_eq!(entry.error_message.as_deref(), Some("relay down"));
        assert!(EmailQueueEntry::claim_send(&db, &id).await.unwrap());

        // Sent, failed or missing: nothing to claim, and a late release
        // does not make a sent email pending again.
        EmailQueueEntry::mark_sent(&db, &id).await.unwrap();
        EmailQueueEntry::release_send(&db, &id, "late")
            .await
            .unwrap();
        assert_eq!(status_of(&db, &id).await, EmailQueueStatus::Sent as i16);
        assert!(!EmailQueueEntry::claim_send(&db, &id).await.unwrap());
        assert!(
            !EmailQueueEntry::claim_send(&db, &Uuid::now_v7())
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn a_dead_letter_fails_a_sending_email_and_leaves_a_pending_one_to_the_sweep() {
        let db = setup_test_db().await;
        let enqueue = async |to: &'static str| {
            EmailQueueEntry::enqueue(&db, to, "S", None, None, "f@example.com")
                .await
                .unwrap()
        };
        let fail = async |id: Uuid| {
            EmailQueueEntry::fail_sending(&db, &id, "dead-lettered")
                .await
                .unwrap()
        };

        // Left in sending by the delivery that holds its claim: failed.
        let sending = enqueue("sending@example.com").await;
        assert!(EmailQueueEntry::claim_send(&db, &sending).await.unwrap());
        assert!(fail(sending).await);
        let entry = EmailQueueEntry::get_by_id(&db, &sending).await.unwrap();
        assert_eq!(entry.status_id, EmailQueueStatus::Failed as i16);
        assert_eq!(entry.error_message.as_deref(), Some("dead-lettered"));

        // Pending, as between the retries of another message for it: not
        // failed, so that retry still claims and sends it.
        let pending = enqueue("pending@example.com").await;
        assert!(!fail(pending).await);
        assert_eq!(
            status_of(&db, &pending).await,
            EmailQueueStatus::Pending as i16
        );
        assert!(EmailQueueEntry::claim_send(&db, &pending).await.unwrap());
        EmailQueueEntry::mark_sent(&db, &pending).await.unwrap();

        // Sent or failed already: unchanged.
        assert!(!fail(pending).await);
        assert_eq!(
            status_of(&db, &pending).await,
            EmailQueueStatus::Sent as i16
        );
        assert!(!fail(sending).await);
        assert!(!fail(Uuid::now_v7()).await);
    }

    #[tokio::test]
    async fn a_pending_email_whose_message_was_dead_lettered_is_recovered_then_expired() {
        let db = setup_test_db().await;
        let id = EmailQueueEntry::enqueue(&db, "a@example.com", "S", None, None, "f@example.com")
            .await
            .unwrap();
        let created = Utc::now();
        let minutes = chrono::Duration::minutes;
        // Its only message is dead-lettered by a delivery without the claim.
        assert!(
            !EmailQueueEntry::fail_sending(&db, &id, "dead-lettered")
                .await
                .unwrap()
        );

        // The sweep enqueues it again once it is overdue, each time later.
        // Swept every minute, off the minute so no pass lands on a due time.
        let mut enqueued = 0;
        let mut at = chrono::Duration::seconds(30);
        while at < RECOVERY_WINDOW {
            at += minutes(1);
            let now = created + at;
            assert_eq!(
                EmailQueueEntry::settle_stale(&db, now).await.unwrap(),
                StaleEmails::default()
            );
            enqueued += EmailQueueEntry::claim_overdue(&db, now, 10)
                .await
                .unwrap()
                .len();
        }
        assert_eq!(
            enqueued, 6,
            "after waits of 10, 10, 20, 40, 80 and 160 minutes"
        );

        // No message sends it: past the window it fails, at the latest one
        // grace after the last enqueue.
        let end = created + RECOVERY_WINDOW + RECOVERY_GRACE + minutes(1);
        assert!(
            EmailQueueEntry::claim_overdue(&db, end, 10)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            EmailQueueEntry::settle_stale(&db, end)
                .await
                .unwrap()
                .failed,
            1
        );
        let entry = EmailQueueEntry::get_by_id(&db, &id).await.unwrap();
        assert_eq!(entry.status_id, EmailQueueStatus::Failed as i16);
        assert_eq!(entry.error_message.as_deref(), Some(EXPIRED_PENDING_ERROR));
    }

    #[tokio::test]
    async fn an_email_stuck_in_sending_is_recovered_after_the_bound() {
        let db = setup_test_db().await;
        let id = EmailQueueEntry::enqueue(&db, "a@example.com", "S", None, None, "f@example.com")
            .await
            .unwrap();
        // Claimed, and then its worker died.
        assert!(EmailQueueEntry::claim_send(&db, &id).await.unwrap());
        let claimed = Utc::now();
        let minutes = chrono::Duration::minutes;

        // A live worker may still be sending it: no sweep touches it.
        let early = claimed + SENDING_STALE_AFTER - minutes(1);
        assert_eq!(
            EmailQueueEntry::settle_stale(&db, early).await.unwrap(),
            StaleEmails::default()
        );
        assert!(
            EmailQueueEntry::claim_overdue(&db, early, 10)
                .await
                .unwrap()
                .is_empty()
        );

        // Past the bound it is pending again, and overdue at once.
        let late = claimed + SENDING_STALE_AFTER + minutes(1);
        let settled = EmailQueueEntry::settle_stale(&db, late).await.unwrap();
        assert_eq!((settled.failed, settled.released), (0, 1));
        assert_eq!(status_of(&db, &id).await, EmailQueueStatus::Pending as i16);
        let due = EmailQueueEntry::claim_overdue(&db, late, 10).await.unwrap();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].email_queue_id, id);
        assert_eq!(
            EmailQueueEntry::settle_stale(&db, late).await.unwrap(),
            StaleEmails::default()
        );
    }

    #[tokio::test]
    async fn emails_past_the_recovery_window_are_failed() {
        let db = setup_test_db().await;
        let enqueue = async |to: &'static str| {
            EmailQueueEntry::enqueue(&db, to, "S", None, None, "f@example.com")
                .await
                .unwrap()
        };
        let pending = enqueue("pending@example.com").await;
        let sending = enqueue("sending@example.com").await;
        assert!(EmailQueueEntry::claim_send(&db, &sending).await.unwrap());
        let sent = enqueue("sent@example.com").await;
        EmailQueueEntry::mark_sent(&db, &sent).await.unwrap();
        let created = Utc::now();
        let minutes = chrono::Duration::minutes;

        // Inside the window nothing fails; the sweep enqueues `pending`
        // again just before the window ends.
        let before = created + RECOVERY_WINDOW - minutes(5);
        let claimed = EmailQueueEntry::claim_overdue(&db, before, 1)
            .await
            .unwrap();
        assert_eq!(claimed[0].email_queue_id, pending);

        // Past it, the email stuck in sending fails; the one just enqueued
        // again keeps the time its new message needs.
        let settle = async |after: chrono::Duration| {
            EmailQueueEntry::settle_stale(&db, created + RECOVERY_WINDOW + after)
                .await
                .unwrap()
        };
        assert_eq!(settle(minutes(1)).await.failed, 1);
        let entry = EmailQueueEntry::get_by_id(&db, &sending).await.unwrap();
        assert_eq!(entry.status_id, EmailQueueStatus::Failed as i16);
        assert_eq!(entry.error_message.as_deref(), Some(EXPIRED_SENDING_ERROR));
        assert_eq!(
            status_of(&db, &pending).await,
            EmailQueueStatus::Pending as i16
        );

        // Then it fails too, rather than staying pending for good.
        assert_eq!(settle(minutes(10)).await.failed, 1);
        let entry = EmailQueueEntry::get_by_id(&db, &pending).await.unwrap();
        assert_eq!(entry.status_id, EmailQueueStatus::Failed as i16);
        assert_eq!(entry.error_message.as_deref(), Some(EXPIRED_PENDING_ERROR));
        assert_eq!(status_of(&db, &sent).await, EmailQueueStatus::Sent as i16);
        assert_eq!(settle(minutes(30)).await, StaleEmails::default());
    }

    #[tokio::test]
    async fn an_email_whose_sends_kept_failing_expires_with_its_last_send_error() {
        let db = setup_test_db().await;
        let id = EmailQueueEntry::enqueue(&db, "a@example.com", "S", None, None, "f@example.com")
            .await
            .unwrap();
        let minutes = chrono::Duration::minutes;
        // Its message spent its attempts: pending again, with the reason.
        assert!(EmailQueueEntry::claim_send(&db, &id).await.unwrap());
        EmailQueueEntry::release_send(&db, &id, "relay down")
            .await
            .unwrap();
        let released = Utc::now();

        // The release restarts the grace, then the sweep enqueues it again.
        let claim = async |after: chrono::Duration| {
            EmailQueueEntry::claim_overdue(&db, released + after, 10)
                .await
                .unwrap()
                .len()
        };
        assert_eq!(claim(RECOVERY_GRACE - minutes(1)).await, 0);
        assert_eq!(claim(RECOVERY_GRACE + minutes(1)).await, 1);

        // Past the window it fails, and still says why no send worked.
        let end = released + RECOVERY_WINDOW + RECOVERY_GRACE + minutes(2);
        assert_eq!(
            claim(RECOVERY_WINDOW + RECOVERY_GRACE + minutes(2)).await,
            0
        );
        assert_eq!(
            EmailQueueEntry::settle_stale(&db, end)
                .await
                .unwrap()
                .failed,
            1
        );
        let entry = EmailQueueEntry::get_by_id(&db, &id).await.unwrap();
        assert_eq!(entry.status_id, EmailQueueStatus::Failed as i16);
        assert_eq!(
            entry.error_message,
            Some(format!(
                "{EXPIRED_PENDING_ERROR}{LAST_SEND_ERROR_PREFIX}relay down"
            ))
        );
    }

    #[tokio::test]
    async fn an_overdue_claim_given_back_is_due_at_once_and_costs_no_backoff() {
        let db = setup_test_db().await;
        let id = EmailQueueEntry::enqueue(&db, "a@example.com", "S", None, None, "f@example.com")
            .await
            .unwrap();
        let created = Utc::now();
        let claim = async |after: chrono::Duration| {
            EmailQueueEntry::claim_overdue(&db, created + after, 10)
                .await
                .unwrap()
                .len()
        };
        let minutes = chrono::Duration::minutes;

        // Claimed, but the enqueue failed: the next pass gets it again.
        assert_eq!(claim(minutes(11)).await, 1);
        EmailQueueEntry::unclaim_overdue(&db, &id).await.unwrap();
        assert_eq!(claim(minutes(11)).await, 1);
        // That enqueue worked: it waits as long again as it had been pending.
        assert_eq!(claim(minutes(20)).await, 0);
        assert_eq!(claim(minutes(23)).await, 1);
    }

    #[tokio::test]
    async fn a_claim_takes_no_more_than_its_limit() {
        let db = setup_test_db().await;
        for to in ["a@example.com", "b@example.com", "c@example.com"] {
            EmailQueueEntry::enqueue(&db, to, "Subject", None, None, "from@example.com")
                .await
                .unwrap();
        }
        let due = Utc::now() + RECOVERY_GRACE + chrono::Duration::minutes(1);
        let claim = async || {
            EmailQueueEntry::claim_overdue(&db, due, 2)
                .await
                .unwrap()
                .len()
        };
        assert_eq!(claim().await, 2);
        assert_eq!(claim().await, 1);
        assert_eq!(claim().await, 0);
    }

    #[tokio::test]
    async fn test_get_by_id_not_found() {
        let db = setup_test_db().await;

        let result = EmailQueueEntry::get_by_id(&db, &Uuid::now_v7()).await;
        assert!(matches!(result, Err(EmailQueueError::NotFound)));
    }
}

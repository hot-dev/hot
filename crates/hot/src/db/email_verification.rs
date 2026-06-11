use chrono::{DateTime, Utc};
use sqlx::FromRow;
use std::str::FromStr;
use thiserror::Error;
use uuid::Uuid;

#[derive(Error, Debug)]
pub enum EmailVerificationError {
    #[error("Database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("Verification not found")]
    NotFound,
    #[error("Verification expired")]
    Expired,
    #[error("Verification already used")]
    AlreadyVerified,
    #[error("Invalid verification status")]
    InvalidStatus,
    #[error("Too many attempts")]
    TooManyAttempts,
}

#[derive(Debug, Clone, PartialEq)]
pub enum VerificationStatus {
    Pending,
    Verified,
    Expired,
}

impl VerificationStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            VerificationStatus::Pending => "pending",
            VerificationStatus::Verified => "verified",
            VerificationStatus::Expired => "expired",
        }
    }

    pub fn as_id(&self) -> i16 {
        match self {
            VerificationStatus::Pending => 1,
            VerificationStatus::Verified => 2,
            VerificationStatus::Expired => 3,
        }
    }

    pub fn from_id(id: i16) -> Option<Self> {
        match id {
            1 => Some(VerificationStatus::Pending),
            2 => Some(VerificationStatus::Verified),
            3 => Some(VerificationStatus::Expired),
            _ => None,
        }
    }
}

impl std::fmt::Display for VerificationStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

impl FromStr for VerificationStatus {
    type Err = EmailVerificationError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "pending" => Ok(VerificationStatus::Pending),
            "verified" => Ok(VerificationStatus::Verified),
            "expired" => Ok(VerificationStatus::Expired),
            _ => Err(EmailVerificationError::InvalidStatus),
        }
    }
}

#[derive(Debug, Clone, FromRow)]
pub struct EmailVerification {
    pub verification_id: Uuid,
    pub email: String,
    pub name: Option<String>,
    pub password_hash: String,
    pub verification_token: String,
    pub status_id: i16,
    pub invite_code: Option<String>,
    pub plan: Option<String>,
    pub billing: Option<String>,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub verified_at: Option<DateTime<Utc>>,
    pub attempts: i32,
}

impl EmailVerification {
    /// Get verification by ID
    pub async fn get_by_id(
        db: &crate::db::DatabasePool,
        verification_id: &Uuid,
    ) -> Result<EmailVerification, EmailVerificationError> {
        match db {
            crate::db::DatabasePool::Sqlite(pool) => {
                let row = sqlx::query_as::<_, EmailVerification>(
                    "SELECT verification_id, email, name, password_hash, verification_token, status_id, invite_code, plan, billing, created_at, expires_at, verified_at, attempts FROM email_verification WHERE verification_id = ?",
                )
                .bind(verification_id)
                .fetch_optional(pool)
                .await?;

                row.ok_or(EmailVerificationError::NotFound)
            }
            crate::db::DatabasePool::Postgres(pool) => {
                let row = sqlx::query_as::<_, EmailVerification>(
                    "SELECT verification_id, email, name, password_hash, verification_token, status_id, invite_code, plan, billing, created_at, expires_at, verified_at, attempts FROM email_verification WHERE verification_id = $1",
                )
                .bind(verification_id)
                .fetch_optional(pool)
                .await?;

                row.ok_or(EmailVerificationError::NotFound)
            }
        }
    }

    /// Get verification by token
    pub async fn get_by_token(
        db: &crate::db::DatabasePool,
        token: &str,
    ) -> Result<EmailVerification, EmailVerificationError> {
        match db {
            crate::db::DatabasePool::Sqlite(pool) => {
                let row = sqlx::query_as::<_, EmailVerification>(
                    "SELECT verification_id, email, name, password_hash, verification_token, status_id, invite_code, plan, billing, created_at, expires_at, verified_at, attempts FROM email_verification WHERE verification_token = ?",
                )
                .bind(token)
                .fetch_optional(pool)
                .await?;

                row.ok_or(EmailVerificationError::NotFound)
            }
            crate::db::DatabasePool::Postgres(pool) => {
                let row = sqlx::query_as::<_, EmailVerification>(
                    "SELECT verification_id, email, name, password_hash, verification_token, status_id, invite_code, plan, billing, created_at, expires_at, verified_at, attempts FROM email_verification WHERE verification_token = $1",
                )
                .bind(token)
                .fetch_optional(pool)
                .await?;

                row.ok_or(EmailVerificationError::NotFound)
            }
        }
    }

    /// Get pending verification by email (most recent)
    pub async fn get_pending_by_email(
        db: &crate::db::DatabasePool,
        email: &str,
    ) -> Result<Option<EmailVerification>, EmailVerificationError> {
        match db {
            crate::db::DatabasePool::Sqlite(pool) => {
                let row = sqlx::query_as::<_, EmailVerification>(
                    "SELECT verification_id, email, name, password_hash, verification_token, status_id, invite_code, plan, billing, created_at, expires_at, verified_at, attempts FROM email_verification WHERE email = ? AND status_id = 1 ORDER BY created_at DESC LIMIT 1",
                )
                .bind(email)
                .fetch_optional(pool)
                .await?;

                Ok(row)
            }
            crate::db::DatabasePool::Postgres(pool) => {
                let row = sqlx::query_as::<_, EmailVerification>(
                    "SELECT verification_id, email, name, password_hash, verification_token, status_id, invite_code, plan, billing, created_at, expires_at, verified_at, attempts FROM email_verification WHERE email = $1 AND status_id = 1 ORDER BY created_at DESC LIMIT 1",
                )
                .bind(email)
                .fetch_optional(pool)
                .await?;

                Ok(row)
            }
        }
    }

    /// Get the most recent verification record for an email, regardless of status.
    ///
    /// Used by the org-recovery path: if a user lands on `/claim-handle` (or
    /// `/billing/create-checkout-form`) authenticated but with no org, we can
    /// look up the slug they originally chose at signup and auto-adopt /
    /// auto-create the org from it instead of stranding them on a form asking
    /// them to pick a handle they already chose.
    pub async fn get_latest_by_email(
        db: &crate::db::DatabasePool,
        email: &str,
    ) -> Result<Option<EmailVerification>, EmailVerificationError> {
        match db {
            crate::db::DatabasePool::Sqlite(pool) => {
                let row = sqlx::query_as::<_, EmailVerification>(
                    "SELECT verification_id, email, name, password_hash, verification_token, status_id, invite_code, plan, billing, created_at, expires_at, verified_at, attempts FROM email_verification WHERE email = ? ORDER BY created_at DESC LIMIT 1",
                )
                .bind(email)
                .fetch_optional(pool)
                .await?;

                Ok(row)
            }
            crate::db::DatabasePool::Postgres(pool) => {
                let row = sqlx::query_as::<_, EmailVerification>(
                    "SELECT verification_id, email, name, password_hash, verification_token, status_id, invite_code, plan, billing, created_at, expires_at, verified_at, attempts FROM email_verification WHERE email = $1 ORDER BY created_at DESC LIMIT 1",
                )
                .bind(email)
                .fetch_optional(pool)
                .await?;

                Ok(row)
            }
        }
    }

    /// Insert a new email verification
    #[allow(clippy::too_many_arguments)]
    pub async fn insert(
        db: &crate::db::DatabasePool,
        verification_id: &Uuid,
        email: &str,
        name: Option<&str>,
        password_hash: &str,
        verification_token: &str,
        invite_code: Option<&str>,
        plan: Option<&str>,
        billing: Option<&str>,
        expires_at: DateTime<Utc>,
    ) -> Result<(), EmailVerificationError> {
        match db {
            crate::db::DatabasePool::Sqlite(pool) => {
                sqlx::query(
                    "INSERT INTO email_verification (verification_id, email, name, password_hash, verification_token, status_id, invite_code, plan, billing, expires_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                )
                .bind(verification_id)
                .bind(email)
                .bind(name)
                .bind(password_hash)
                .bind(verification_token)
                .bind(VerificationStatus::Pending.as_id())
                .bind(invite_code)
                .bind(plan)
                .bind(billing)
                .bind(expires_at)
                .execute(pool)
                .await?;
            }
            crate::db::DatabasePool::Postgres(pool) => {
                sqlx::query(
                    "INSERT INTO email_verification (verification_id, email, name, password_hash, verification_token, status_id, invite_code, plan, billing, expires_at) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
                )
                .bind(verification_id)
                .bind(email)
                .bind(name)
                .bind(password_hash)
                .bind(verification_token)
                .bind(VerificationStatus::Pending.as_id())
                .bind(invite_code)
                .bind(plan)
                .bind(billing)
                .bind(expires_at)
                .execute(pool)
                .await?;
            }
        }
        Ok(())
    }

    /// Mark verification as verified
    pub async fn mark_verified(
        db: &crate::db::DatabasePool,
        verification_id: &Uuid,
    ) -> Result<(), EmailVerificationError> {
        let now = Utc::now();

        match db {
            crate::db::DatabasePool::Sqlite(pool) => {
                sqlx::query(
                    "UPDATE email_verification SET status_id = ?, verified_at = ? WHERE verification_id = ?",
                )
                .bind(VerificationStatus::Verified.as_id())
                .bind(now)
                .bind(verification_id)
                .execute(pool)
                .await?;
            }
            crate::db::DatabasePool::Postgres(pool) => {
                sqlx::query(
                    "UPDATE email_verification SET status_id = $1, verified_at = $2 WHERE verification_id = $3",
                )
                .bind(VerificationStatus::Verified.as_id())
                .bind(now)
                .bind(verification_id)
                .execute(pool)
                .await?;
            }
        }
        Ok(())
    }

    /// Mark verification as expired
    pub async fn mark_expired(
        db: &crate::db::DatabasePool,
        verification_id: &Uuid,
    ) -> Result<(), EmailVerificationError> {
        match db {
            crate::db::DatabasePool::Sqlite(pool) => {
                sqlx::query(
                    "UPDATE email_verification SET status_id = ? WHERE verification_id = ?",
                )
                .bind(VerificationStatus::Expired.as_id())
                .bind(verification_id)
                .execute(pool)
                .await?;
            }
            crate::db::DatabasePool::Postgres(pool) => {
                sqlx::query(
                    "UPDATE email_verification SET status_id = $1 WHERE verification_id = $2",
                )
                .bind(VerificationStatus::Expired.as_id())
                .bind(verification_id)
                .execute(pool)
                .await?;
            }
        }
        Ok(())
    }

    /// Increment attempt counter
    pub async fn increment_attempts(
        db: &crate::db::DatabasePool,
        verification_id: &Uuid,
    ) -> Result<i32, EmailVerificationError> {
        match db {
            crate::db::DatabasePool::Sqlite(pool) => {
                let result: (i32,) = sqlx::query_as(
                    "UPDATE email_verification SET attempts = attempts + 1 WHERE verification_id = ? RETURNING attempts",
                )
                .bind(verification_id)
                .fetch_one(pool)
                .await?;
                Ok(result.0)
            }
            crate::db::DatabasePool::Postgres(pool) => {
                let result: (i32,) = sqlx::query_as(
                    "UPDATE email_verification SET attempts = attempts + 1 WHERE verification_id = $1 RETURNING attempts",
                )
                .bind(verification_id)
                .fetch_one(pool)
                .await?;
                Ok(result.0)
            }
        }
    }

    /// Update verification token (for resend)
    pub async fn update_token(
        db: &crate::db::DatabasePool,
        verification_id: &Uuid,
        new_token: &str,
        new_expires_at: DateTime<Utc>,
    ) -> Result<(), EmailVerificationError> {
        match db {
            crate::db::DatabasePool::Sqlite(pool) => {
                sqlx::query(
                    "UPDATE email_verification SET verification_token = ?, expires_at = ? WHERE verification_id = ?",
                )
                .bind(new_token)
                .bind(new_expires_at)
                .bind(verification_id)
                .execute(pool)
                .await?;
            }
            crate::db::DatabasePool::Postgres(pool) => {
                sqlx::query(
                    "UPDATE email_verification SET verification_token = $1, expires_at = $2 WHERE verification_id = $3",
                )
                .bind(new_token)
                .bind(new_expires_at)
                .bind(verification_id)
                .execute(pool)
                .await?;
            }
        }
        Ok(())
    }

    /// Expire all pending verifications older than a given duration
    pub async fn expire_old_pending(
        db: &crate::db::DatabasePool,
    ) -> Result<u64, EmailVerificationError> {
        let now = Utc::now();

        match db {
            crate::db::DatabasePool::Sqlite(pool) => {
                let result = sqlx::query(
                    "UPDATE email_verification SET status_id = ? WHERE status_id = ? AND expires_at < ?",
                )
                .bind(VerificationStatus::Expired.as_id())
                .bind(VerificationStatus::Pending.as_id())
                .bind(now)
                .execute(pool)
                .await?;
                Ok(result.rows_affected())
            }
            crate::db::DatabasePool::Postgres(pool) => {
                let result = sqlx::query(
                    "UPDATE email_verification SET status_id = $1 WHERE status_id = $2 AND expires_at < $3",
                )
                .bind(VerificationStatus::Expired.as_id())
                .bind(VerificationStatus::Pending.as_id())
                .bind(now)
                .execute(pool)
                .await?;
                Ok(result.rows_affected())
            }
        }
    }

    /// Check if verification is valid (not expired, not already verified)
    pub fn is_valid(&self) -> Result<(), EmailVerificationError> {
        if self.status_id == VerificationStatus::Verified.as_id() {
            return Err(EmailVerificationError::AlreadyVerified);
        }

        if self.status_id == VerificationStatus::Expired.as_id() {
            return Err(EmailVerificationError::Expired);
        }

        if self.expires_at < Utc::now() {
            return Err(EmailVerificationError::Expired);
        }

        Ok(())
    }

    /// Get verification status as enum
    pub fn get_status(&self) -> VerificationStatus {
        VerificationStatus::from_id(self.status_id).unwrap_or(VerificationStatus::Pending)
    }

    /// Generate a secure random verification token
    pub fn generate_token() -> String {
        use rand::Rng;
        const CHARSET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
        const TOKEN_LENGTH: usize = 64;

        let mut rng = rand::thread_rng();
        (0..TOKEN_LENGTH)
            .map(|_| {
                let idx = rng.gen_range(0..CHARSET.len());
                CHARSET[idx] as char
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_verification_status_conversion() {
        assert_eq!(VerificationStatus::Pending.as_str(), "pending");
        assert_eq!(VerificationStatus::Verified.as_str(), "verified");
        assert_eq!(VerificationStatus::Expired.as_str(), "expired");

        assert_eq!(
            VerificationStatus::from_id(1),
            Some(VerificationStatus::Pending)
        );
        assert_eq!(
            VerificationStatus::from_id(2),
            Some(VerificationStatus::Verified)
        );
        assert_eq!(
            VerificationStatus::from_id(3),
            Some(VerificationStatus::Expired)
        );
        assert_eq!(VerificationStatus::from_id(4), None);
    }

    #[test]
    fn test_generate_token() {
        let token = EmailVerification::generate_token();
        assert_eq!(token.len(), 64);

        // Ensure it's different each time
        let token2 = EmailVerification::generate_token();
        assert_ne!(token, token2);
    }
}

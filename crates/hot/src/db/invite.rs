use chrono::{DateTime, Utc};
use sqlx::FromRow;
use std::str::FromStr;
use thiserror::Error;
use uuid::Uuid;

#[derive(Error, Debug)]
pub enum InviteError {
    #[error("Database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("Invite not found")]
    NotFound,
    #[error("Invite expired")]
    Expired,
    #[error("Invite already used")]
    AlreadyUsed,
    #[error("Invalid invite status")]
    InvalidStatus,
}

#[derive(Debug, Clone, PartialEq)]
pub enum InviteStatus {
    Invited,
    Joined,
    Declined,
}

impl InviteStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            InviteStatus::Invited => "invited",
            InviteStatus::Joined => "joined",
            InviteStatus::Declined => "declined",
        }
    }

    pub fn as_id(&self) -> i16 {
        match self {
            InviteStatus::Invited => 1,
            InviteStatus::Joined => 2,
            InviteStatus::Declined => 3,
        }
    }

    pub fn from_id(id: i16) -> Option<Self> {
        match id {
            1 => Some(InviteStatus::Invited),
            2 => Some(InviteStatus::Joined),
            3 => Some(InviteStatus::Declined),
            _ => None,
        }
    }
}

impl std::fmt::Display for InviteStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

impl FromStr for InviteStatus {
    type Err = InviteError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "invited" => Ok(InviteStatus::Invited),
            "joined" => Ok(InviteStatus::Joined),
            "declined" => Ok(InviteStatus::Declined),
            _ => Err(InviteError::InvalidStatus),
        }
    }
}

#[derive(Debug, Clone, FromRow)]
pub struct Invite {
    pub invite_id: Uuid,
    pub invite_code: String,
    pub email: String,
    pub org_id: Uuid,
    pub invite_status_id: i16,
    pub intended_org_user_role_id: i16,
    pub active: bool,
    pub created_at: DateTime<Utc>,
    pub created_by_user_id: Uuid,
    pub updated_at: DateTime<Utc>,
    pub updated_by_user_id: Option<Uuid>,
    pub expires_at: DateTime<Utc>,
    pub used_at: Option<DateTime<Utc>>,
}

impl Invite {
    /// Get invite by ID
    pub async fn get_invite(
        db: &crate::db::DatabasePool,
        invite_id: &Uuid,
    ) -> Result<Invite, InviteError> {
        match db {
            crate::db::DatabasePool::Sqlite(db) => {
                let row = sqlx::query_as::<_, Invite>(
                    "SELECT invite_id, invite_code, email, org_id, invite_status_id, intended_org_user_role_id, active, created_at, created_by_user_id, updated_at, updated_by_user_id, expires_at, used_at FROM invite WHERE invite_id = ?",
                )
                .bind(invite_id)
                .fetch_optional(db)
                .await?;

                row.ok_or(InviteError::NotFound)
            }
            crate::db::DatabasePool::Postgres(db) => {
                let row = sqlx::query_as::<_, Invite>(
                    "SELECT invite_id, invite_code, email, org_id, invite_status_id, intended_org_user_role_id, active, created_at, created_by_user_id, updated_at, updated_by_user_id, expires_at, used_at FROM invite WHERE invite_id = $1",
                )
                .bind(invite_id)
                .fetch_optional(db)
                .await?;

                row.ok_or(InviteError::NotFound)
            }
        }
    }

    /// Get invite by code
    pub async fn get_invite_by_code(
        db: &crate::db::DatabasePool,
        invite_code: &str,
    ) -> Result<Invite, InviteError> {
        match db {
            crate::db::DatabasePool::Sqlite(db) => {
                let row = sqlx::query_as::<_, Invite>(
                    "SELECT invite_id, invite_code, email, org_id, invite_status_id, intended_org_user_role_id, active, created_at, created_by_user_id, updated_at, updated_by_user_id, expires_at, used_at FROM invite WHERE invite_code = ? AND active = 1",
                )
                .bind(invite_code)
                .fetch_optional(db)
                .await?;

                row.ok_or(InviteError::NotFound)
            }
            crate::db::DatabasePool::Postgres(db) => {
                let row = sqlx::query_as::<_, Invite>(
                    "SELECT invite_id, invite_code, email, org_id, invite_status_id, intended_org_user_role_id, active, created_at, created_by_user_id, updated_at, updated_by_user_id, expires_at, used_at FROM invite WHERE invite_code = $1 AND active = true",
                )
                .bind(invite_code)
                .fetch_optional(db)
                .await?;

                row.ok_or(InviteError::NotFound)
            }
        }
    }

    /// Get invites by organization
    pub async fn get_invites_by_org(
        db: &crate::db::DatabasePool,
        org_id: &Uuid,
        limit: Option<i64>,
        offset: Option<i64>,
    ) -> Result<Vec<Invite>, InviteError> {
        let limit = limit.unwrap_or(10);
        let offset = offset.unwrap_or(0);

        match db {
            crate::db::DatabasePool::Sqlite(db) => {
                let rows = sqlx::query_as::<_, Invite>(
                    "SELECT invite_id, invite_code, email, org_id, invite_status_id, intended_org_user_role_id, active, created_at, created_by_user_id, updated_at, updated_by_user_id, expires_at, used_at FROM invite WHERE org_id = ? AND active = 1 ORDER BY created_at DESC LIMIT ? OFFSET ?",
                )
                .bind(org_id)
                .bind(limit)
                .bind(offset)
                .fetch_all(db)
                .await?;

                Ok(rows)
            }
            crate::db::DatabasePool::Postgres(db) => {
                let rows = sqlx::query_as::<_, Invite>(
                    "SELECT invite_id, invite_code, email, org_id, invite_status_id, intended_org_user_role_id, active, created_at, created_by_user_id, updated_at, updated_by_user_id, expires_at, used_at FROM invite WHERE org_id = $1 AND active = true ORDER BY created_at DESC LIMIT $2 OFFSET $3",
                )
                .bind(org_id)
                .bind(limit)
                .bind(offset)
                .fetch_all(db)
                .await?;

                Ok(rows)
            }
        }
    }

    /// Get invites by email
    pub async fn get_invites_by_email(
        db: &crate::db::DatabasePool,
        email: &str,
    ) -> Result<Vec<Invite>, InviteError> {
        match db {
            crate::db::DatabasePool::Sqlite(db) => {
                let rows = sqlx::query_as::<_, Invite>(
                    "SELECT invite_id, invite_code, email, org_id, invite_status_id, intended_org_user_role_id, active, created_at, created_by_user_id, updated_at, updated_by_user_id, expires_at, used_at FROM invite WHERE email = ? AND active = 1 ORDER BY created_at DESC",
                )
                .bind(email)
                .fetch_all(db)
                .await?;

                Ok(rows)
            }
            crate::db::DatabasePool::Postgres(db) => {
                let rows = sqlx::query_as::<_, Invite>(
                    "SELECT invite_id, invite_code, email, org_id, invite_status_id, intended_org_user_role_id, active, created_at, created_by_user_id, updated_at, updated_by_user_id, expires_at, used_at FROM invite WHERE email = $1 AND active = true ORDER BY created_at DESC",
                )
                .bind(email)
                .fetch_all(db)
                .await?;

                Ok(rows)
            }
        }
    }

    /// Insert invite
    #[allow(clippy::too_many_arguments)]
    pub async fn insert_invite(
        db: &crate::db::DatabasePool,
        invite_id: &Uuid,
        invite_code: &str,
        email: &str,
        org_id: &Uuid,
        intended_org_user_role_id: i16,
        created_by_user_id: &Uuid,
        expires_at: DateTime<Utc>,
    ) -> Result<(), InviteError> {
        match db {
            crate::db::DatabasePool::Sqlite(db) => {
                sqlx::query(
                    "INSERT INTO invite (invite_id, invite_code, email, org_id, invite_status_id, intended_org_user_role_id, created_by_user_id, expires_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
                )
                .bind(invite_id)
                .bind(invite_code)
                .bind(email)
                .bind(org_id)
                .bind(InviteStatus::Invited.as_id())
                .bind(intended_org_user_role_id)
                .bind(created_by_user_id)
                .bind(expires_at)
                .execute(db)
                .await?;
            }
            crate::db::DatabasePool::Postgres(db) => {
                sqlx::query(
                    "INSERT INTO invite (invite_id, invite_code, email, org_id, invite_status_id, intended_org_user_role_id, created_by_user_id, expires_at) VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
                )
                .bind(invite_id)
                .bind(invite_code)
                .bind(email)
                .bind(org_id)
                .bind(InviteStatus::Invited.as_id())
                .bind(intended_org_user_role_id)
                .bind(created_by_user_id)
                .bind(expires_at)
                .execute(db)
                .await?;
            }
        }
        Ok(())
    }

    /// Count pending invites for an org.
    ///
    /// "Pending" means: still in the `Invited` status, the row is `active`,
    /// and it has not yet passed `expires_at`. This is the count that
    /// matters for plan-seat enforcement: every pending invite is a seat
    /// we've already promised to someone, so it must be reserved against
    /// the team_members limit alongside currently-active members.
    pub async fn count_pending_by_org(
        db: &crate::db::DatabasePool,
        org_id: &Uuid,
    ) -> Result<i64, InviteError> {
        let now = Utc::now();
        let pending_id = InviteStatus::Invited.as_id();
        match db {
            crate::db::DatabasePool::Sqlite(pool) => {
                let row: (i64,) = sqlx::query_as(
                    "SELECT COUNT(*) FROM invite WHERE org_id = ? AND active = 1 AND invite_status_id = ? AND expires_at > ?",
                )
                .bind(org_id)
                .bind(pending_id)
                .bind(now)
                .fetch_one(pool)
                .await?;
                Ok(row.0)
            }
            crate::db::DatabasePool::Postgres(pool) => {
                let row: (i64,) = sqlx::query_as(
                    "SELECT COUNT(*) FROM invite WHERE org_id = $1 AND active = true AND invite_status_id = $2 AND expires_at > $3",
                )
                .bind(org_id)
                .bind(pending_id)
                .bind(now)
                .fetch_one(pool)
                .await?;
                Ok(row.0)
            }
        }
    }

    /// Update invite status
    pub async fn update_invite_status(
        db: &crate::db::DatabasePool,
        invite_id: &Uuid,
        status: &InviteStatus,
        updated_by_user_id: Option<&Uuid>,
    ) -> Result<(), InviteError> {
        let now = Utc::now();
        let used_at = if *status == InviteStatus::Joined {
            Some(now)
        } else {
            None
        };

        match db {
            crate::db::DatabasePool::Sqlite(db) => {
                sqlx::query(
                    "UPDATE invite SET invite_status_id = ?, updated_at = ?, updated_by_user_id = ?, used_at = ? WHERE invite_id = ?",
                )
                .bind(status.as_id())
                .bind(now)
                .bind(updated_by_user_id)
                .bind(used_at)
                .bind(invite_id)
                .execute(db)
                .await?;
            }
            crate::db::DatabasePool::Postgres(db) => {
                sqlx::query(
                    "UPDATE invite SET invite_status_id = $1, updated_at = $2, updated_by_user_id = $3, used_at = $4 WHERE invite_id = $5",
                )
                .bind(status.as_id())
                .bind(now)
                .bind(updated_by_user_id)
                .bind(used_at)
                .bind(invite_id)
                .execute(db)
                .await?;
            }
        }
        Ok(())
    }

    /// Check if invite is valid (not expired, not used)
    pub fn is_valid(&self) -> Result<(), InviteError> {
        if !self.active {
            return Err(InviteError::NotFound);
        }

        if self.expires_at < Utc::now() {
            return Err(InviteError::Expired);
        }

        if self.invite_status_id == InviteStatus::Joined.as_id() {
            return Err(InviteError::AlreadyUsed);
        }

        Ok(())
    }

    /// Generate a secure random invite code
    pub fn generate_invite_code() -> String {
        use rand::Rng;
        const CHARSET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
        const CODE_LENGTH: usize = 64;

        let mut rng = rand::thread_rng();
        (0..CODE_LENGTH)
            .map(|_| {
                let idx = rng.gen_range(0..CHARSET.len());
                CHARSET[idx] as char
            })
            .collect()
    }

    /// Get invite status as enum
    pub fn get_status(&self) -> InviteStatus {
        InviteStatus::from_id(self.invite_status_id).unwrap_or(InviteStatus::Invited)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_invite_status_conversion() {
        assert_eq!(InviteStatus::Invited.as_str(), "invited");
        assert_eq!(InviteStatus::Joined.as_str(), "joined");
        assert_eq!(InviteStatus::Declined.as_str(), "declined");

        assert_eq!(InviteStatus::from_id(1), Some(InviteStatus::Invited));
        assert_eq!(InviteStatus::from_id(2), Some(InviteStatus::Joined));
        assert_eq!(InviteStatus::from_id(3), Some(InviteStatus::Declined));
        assert_eq!(InviteStatus::from_id(4), None);
    }

    #[test]
    fn test_generate_invite_code() {
        let code = Invite::generate_invite_code();
        assert_eq!(code.len(), 64);

        // Ensure it's different each time
        let code2 = Invite::generate_invite_code();
        assert_ne!(code, code2);
    }
}

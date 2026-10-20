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
    #[error("Organization seat limit reached")]
    SeatLimit,
    #[error("Invite is not authorized for this account or user")]
    Unauthorized,
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

fn seat_limit(plan: Option<String>, overrides: Option<String>) -> Result<i32, InviteError> {
    if plan.is_none() && overrides.is_none() {
        return Ok(-1);
    }
    let plan = plan
        .map(|value| serde_json::from_str(&value))
        .transpose()
        .map_err(|_| InviteError::Unauthorized)?;
    let overrides = overrides
        .map(|value| serde_json::from_str(&value))
        .transpose()
        .map_err(|_| InviteError::Unauthorized)?;
    Ok(crate::db::Features::resolve(plan.as_ref(), overrides.as_ref()).team_members())
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
        let scoped = db.with_scope(crate::db::DatabaseScope::Bootstrap(
            crate::db::BootstrapOperation::Membership,
        ));
        let db = &scoped;
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
        use crate::db::{DatabasePool, DatabaseScope};
        if !matches!(intended_org_user_role_id, 1 | 2) {
            return Err(InviteError::Unauthorized);
        }
        let access = crate::db::Org::resolve_user_access(db, org_id, created_by_user_id)
            .await
            .map_err(|_| InviteError::Unauthorized)?;
        if !access.is_some_and(|access| {
            access.root_org_id == *org_id && access.can_manage_account && access.runtime_allowed
        }) {
            return Err(InviteError::Unauthorized);
        }
        let db = db.with_scope(DatabaseScope::Organization { org_id: *org_id });
        macro_rules! insert_reserved {
            ($pool:expr, $lock:literal) => {{
                let mut tx = $pool.begin().await?;
                sqlx::query($lock).bind(org_id).execute(&mut *tx).await?;
                let (parent, active, lifecycle): (Option<Uuid>, bool, String) = sqlx::query_as("SELECT parent_org_id, active, lifecycle_state FROM org WHERE org_id=$1")
                    .bind(org_id).fetch_one(&mut *tx).await?;
                if parent.is_some() || !active || lifecycle != "active" { return Err(InviteError::Unauthorized); }
                let is_admin: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM org_user WHERE org_id=$1 AND user_id=$2 AND active=true AND org_user_role_id=2)")
                    .bind(org_id).bind(created_by_user_id).fetch_one(&mut *tx).await?;
                if !is_admin { return Err(InviteError::Unauthorized); }
                let existing_member: bool = sqlx::query_scalar(r#"SELECT EXISTS(SELECT 1 FROM org_user ou JOIN "user" u ON u.user_id=ou.user_id WHERE ou.org_id=$1 AND ou.active=true AND lower(u.email)=lower($2))"#)
                    .bind(org_id).bind(email).fetch_one(&mut *tx).await?;
                if existing_member { return Err(InviteError::AlreadyUsed); }
                let (plan, overrides): (Option<String>, Option<String>) = sqlx::query_as(
                    "SELECT CAST(p.features AS TEXT), CAST(o.features AS TEXT) FROM org o LEFT JOIN org_plan op ON op.org_id=o.org_id LEFT JOIN plan p ON p.plan_uuid=op.plan_uuid WHERE o.org_id=$1")
                    .bind(org_id).fetch_one(&mut *tx).await?;
                let limit = seat_limit(plan, overrides)?;
                let seats: i64 = sqlx::query_scalar("SELECT (SELECT COUNT(*) FROM org_user WHERE org_id=$1 AND active=true) + (SELECT COUNT(DISTINCT lower(email)) FROM invite WHERE org_id=$1 AND active=true AND invite_status_id=1 AND expires_at>$2)")
                    .bind(org_id).bind(Utc::now()).fetch_one(&mut *tx).await?;
                if limit >= 0 && seats >= i64::from(limit) { return Err(InviteError::SeatLimit); }
                let duplicate: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM invite WHERE org_id=$1 AND lower(email)=lower($2) AND active=true AND invite_status_id=1 AND expires_at>$3)")
                    .bind(org_id).bind(email).bind(Utc::now()).fetch_one(&mut *tx).await?;
                if duplicate { return Err(InviteError::AlreadyUsed); }
                sqlx::query("INSERT INTO invite (invite_id, invite_code, email, org_id, invite_status_id, intended_org_user_role_id, created_by_user_id, expires_at) VALUES ($1,$2,$3,$4,1,$5,$6,$7)")
                    .bind(invite_id).bind(invite_code).bind(email).bind(org_id)
                    .bind(intended_org_user_role_id).bind(created_by_user_id).bind(expires_at)
                    .execute(&mut *tx).await?;
                tx.commit().await?;
            }};
        }
        match &db {
            DatabasePool::Postgres(pool) => {
                insert_reserved!(pool, "SELECT org_id FROM org WHERE org_id=$1 FOR UPDATE")
            }
            DatabasePool::Sqlite(pool) => {
                insert_reserved!(pool, "UPDATE org SET updated_at=updated_at WHERE org_id=$1")
            }
        }
        Ok(())
    }

    /// Consume the reserved seat and the email-bound invite in one transaction.
    /// Rejoining restores only root membership; child grants remain inactive.
    pub async fn accept_for_user(
        db: &crate::db::DatabasePool,
        code: &str,
        user_id: Uuid,
    ) -> Result<(), InviteError> {
        use crate::db::{BootstrapOperation, DatabasePool, DatabaseScope};
        let lookup = db.with_scope(DatabaseScope::Bootstrap(BootstrapOperation::Membership));
        let invite = Self::get_invite_by_code(&lookup, code).await?;
        let db = db.with_scope(DatabaseScope::Organization {
            org_id: invite.org_id,
        });
        macro_rules! pg_identity {
            ($tx:ident, $root:expr) => {{
                DatabaseScope::User { user_id }
                    .apply_to_connection(&mut $tx)
                    .await?;
                let identity: (String, bool) =
                    sqlx::query_as(r#"SELECT email,active FROM "user" WHERE user_id=$1 FOR SHARE"#)
                        .bind(user_id)
                        .fetch_one(&mut *$tx)
                        .await?;
                DatabaseScope::Organization { org_id: $root }
                    .apply_to_connection(&mut $tx)
                    .await?;
                identity
            }};
        }
        macro_rules! sqlite_identity {
            ($tx:ident, $root:expr) => {{
                sqlx::query_as::<_, (String, bool)>(
                    "SELECT email,active FROM user WHERE user_id=$1",
                )
                .bind(user_id)
                .fetch_one(&mut *$tx)
                .await?
            }};
        }
        macro_rules! accept_reserved {
            ($pool:expr, $lock:literal, $identity:ident) => {{
                let mut tx = $pool.begin().await?;
                sqlx::query($lock).bind(invite.org_id).execute(&mut *tx).await?;
                let invite: Invite = sqlx::query_as("SELECT * FROM invite WHERE invite_code=$1")
                    .bind(code).fetch_one(&mut *tx).await?;
                invite.is_valid()?;
                if !invite.active || invite.invite_status_id != 1 { return Err(InviteError::AlreadyUsed); }
                let (parent, active, lifecycle): (Option<Uuid>, bool, String) = sqlx::query_as("SELECT parent_org_id, active, lifecycle_state FROM org WHERE org_id=$1")
                    .bind(invite.org_id).fetch_one(&mut *tx).await?;
                if parent.is_some() || !active || lifecycle != "active" { return Err(InviteError::Unauthorized); }
                let (email, active) = $identity!(tx, invite.org_id);
                if !active || !email.eq_ignore_ascii_case(&invite.email) { return Err(InviteError::Unauthorized); }
                let (plan, overrides): (Option<String>, Option<String>) = sqlx::query_as(
                    "SELECT CAST(p.features AS TEXT), CAST(o.features AS TEXT) FROM org o LEFT JOIN org_plan op ON op.org_id=o.org_id LEFT JOIN plan p ON p.plan_uuid=op.plan_uuid WHERE o.org_id=$1")
                    .bind(invite.org_id).fetch_one(&mut *tx).await?;
                let limit = seat_limit(plan, overrides)?;
                let seats: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM org_user WHERE org_id=$1 AND active=true")
                    .bind(invite.org_id).fetch_one(&mut *tx).await?;
                if limit >= 0 && seats >= i64::from(limit) { return Err(InviteError::SeatLimit); }
                let prior: Option<(Uuid,bool)> = sqlx::query_as("SELECT org_user_id,active FROM org_user WHERE org_id=$1 AND user_id=$2")
                    .bind(invite.org_id).bind(user_id).fetch_optional(&mut *tx).await?;
                if prior.is_some_and(|(_,active)| active) { return Err(InviteError::AlreadyUsed); }
                if let Some((id,_)) = prior {
                    sqlx::query("UPDATE org_user SET active=true,org_user_role_id=$1,updated_at=CURRENT_TIMESTAMP,updated_by_user_id=$2 WHERE org_user_id=$3")
                        .bind(invite.intended_org_user_role_id).bind(invite.created_by_user_id).bind(id).execute(&mut *tx).await?;
                } else {
                    sqlx::query("INSERT INTO org_user(org_user_id,org_id,user_id,org_user_role_id,created_by_user_id) VALUES($1,$2,$3,$4,$5)")
                        .bind(Uuid::now_v7()).bind(invite.org_id).bind(user_id).bind(invite.intended_org_user_role_id).bind(invite.created_by_user_id).execute(&mut *tx).await?;
                }
                sqlx::query("UPDATE invite SET invite_status_id=2,used_at=CURRENT_TIMESTAMP,updated_at=CURRENT_TIMESTAMP,updated_by_user_id=$1 WHERE invite_id=$2")
                    .bind(user_id).bind(invite.invite_id).execute(&mut *tx).await?;
                tx.commit().await?;
            }};
        }
        match &db {
            DatabasePool::Postgres(pool) => accept_reserved!(
                pool,
                "SELECT org_id FROM org WHERE org_id=$1 FOR UPDATE",
                pg_identity
            ),
            DatabasePool::Sqlite(pool) => accept_reserved!(
                pool,
                "UPDATE org SET updated_at=updated_at WHERE org_id=$1",
                sqlite_identity
            ),
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
        use rand::RngExt;
        const CHARSET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
        const CODE_LENGTH: usize = 64;

        let mut rng = rand::rng();
        (0..CODE_LENGTH)
            .map(|_| {
                let idx = rng.random_range(0..CHARSET.len());
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

#[cfg(test)]
mod seat_reservation_tests {
    use super::*;
    use crate::db::{self, DatabasePool};

    #[tokio::test]
    async fn concurrent_invites_reserve_one_root_seat_and_acceptance_is_atomic() {
        let db = db::test_db().await;
        let (root, admin, _) = db::insert_default_data(&db).await.unwrap();
        let DatabasePool::Sqlite(pool) = &db else {
            unreachable!()
        };
        sqlx::query("UPDATE org SET features=? WHERE org_id=?")
            .bind(r#"{"team_members":2}"#)
            .bind(root)
            .execute(pool)
            .await
            .unwrap();
        let a = Uuid::now_v7();
        let b = Uuid::now_v7();
        let expiry = Utc::now() + chrono::Duration::days(1);
        let (a_result, b_result) = tokio::join!(
            Invite::insert_invite(
                &db,
                &a,
                "invite-a",
                "a@example.test",
                &root,
                1,
                &admin,
                expiry
            ),
            Invite::insert_invite(
                &db,
                &b,
                "invite-b",
                "b@example.test",
                &root,
                1,
                &admin,
                expiry
            )
        );
        assert_eq!(
            usize::from(a_result.is_ok()) + usize::from(b_result.is_ok()),
            1
        );
        let (code, email) = if a_result.is_ok() {
            ("invite-a", "a@example.test")
        } else {
            ("invite-b", "b@example.test")
        };
        assert_eq!(Invite::count_pending_by_org(&db, &root).await.unwrap(), 1);
        let user = Uuid::now_v7();
        sqlx::query("INSERT INTO user(user_id,email,created_by_user_id) VALUES(?,?,?)")
            .bind(user)
            .bind(email)
            .bind(user)
            .execute(pool)
            .await
            .unwrap();
        assert!(matches!(
            Invite::accept_for_user(&db, code, admin).await,
            Err(InviteError::Unauthorized)
        ));
        Invite::accept_for_user(&db, code, user).await.unwrap();
        assert_eq!(Invite::count_pending_by_org(&db, &root).await.unwrap(), 0);
        assert_eq!(
            db::org::OrgUser::count_active_members(&db, &root)
                .await
                .unwrap(),
            2
        );
        assert!(Invite::accept_for_user(&db, code, user).await.is_err());
    }
}

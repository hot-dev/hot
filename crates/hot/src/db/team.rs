use chrono::{DateTime, Utc};
use sqlx::FromRow;
use thiserror::Error;
use uuid::Uuid;

#[derive(Error, Debug)]
pub enum TeamError {
    #[error("Database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("Team not found")]
    NotFound,
}

#[derive(Debug, Clone, FromRow)]
pub struct Team {
    pub team_id: Uuid,
    pub org_id: Uuid,
    pub name: String,
    pub active: bool,
    pub created_at: DateTime<Utc>,
    pub created_by_user_id: Uuid,
    pub updated_at: DateTime<Utc>,
    pub updated_by_user_id: Option<Uuid>,
    pub active_toggle_at: Option<DateTime<Utc>>,
    pub active_toggle_by_user_id: Option<Uuid>,
}

#[derive(Debug, FromRow, Clone)]
pub struct TeamUser {
    pub team_user_id: Uuid,
    pub team_id: Uuid,
    pub user_id: Uuid,
    pub team_user_role_id: i16,
    pub active: bool,
    pub created_at: DateTime<Utc>,
    pub created_by_user_id: Uuid,
    pub updated_at: DateTime<Utc>,
    pub updated_by_user_id: Option<Uuid>,
    pub active_toggle_at: Option<DateTime<Utc>>,
    pub active_toggle_by_user_id: Option<Uuid>,
}

#[derive(Debug, FromRow, Clone)]
pub struct TeamUserWithRole {
    pub user_id: Uuid,
    pub email: String,
    pub name: String,
    pub team_user_role_id: i16,
    pub role: String,
    pub active: bool,
    pub created_at: DateTime<Utc>,
}

impl Team {
    /// Get team by ID
    pub async fn get_team(db: &crate::db::DatabasePool, team_id: &Uuid) -> Result<Team, TeamError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let team = sqlx::query_as::<_, Team>(
                    "SELECT team_id, org_id, name, active, created_at, created_by_user_id, updated_at, updated_by_user_id, active_toggle_at, active_toggle_by_user_id FROM team WHERE team_id = $1"
                )
                .bind(team_id)
                .fetch_one(pg_pool)
                .await?;
                Ok(team)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let team = sqlx::query_as::<_, Team>(
                    "SELECT team_id, org_id, name, active, created_at, created_by_user_id, updated_at, updated_by_user_id, active_toggle_at, active_toggle_by_user_id FROM team WHERE team_id = ?",
                )
                .bind(team_id)
                .fetch_one(sqlite_pool)
                .await?;
                Ok(team)
            }
        }
    }

    /// Get a team by ID within an organization.
    pub async fn get_team_by_org(
        db: &crate::db::DatabasePool,
        team_id: &Uuid,
        org_id: &Uuid,
    ) -> Result<Team, TeamError> {
        let team = match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                sqlx::query_as::<_, Team>(
                    "SELECT team_id, org_id, name, active, created_at, created_by_user_id, updated_at, updated_by_user_id, active_toggle_at, active_toggle_by_user_id FROM team WHERE team_id = $1 AND org_id = $2",
                )
                .bind(team_id)
                .bind(org_id)
                .fetch_optional(pg_pool)
                .await?
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                sqlx::query_as::<_, Team>(
                    "SELECT team_id, org_id, name, active, created_at, created_by_user_id, updated_at, updated_by_user_id, active_toggle_at, active_toggle_by_user_id FROM team WHERE team_id = ? AND org_id = ?",
                )
                .bind(team_id)
                .bind(org_id)
                .fetch_optional(sqlite_pool)
                .await?
            }
        };

        team.ok_or(TeamError::NotFound)
    }

    /// Get count of teams
    pub async fn get_count(db: &crate::db::DatabasePool) -> Result<i64, TeamError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM team")
                    .fetch_one(pg_pool)
                    .await?;
                Ok(count)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM team")
                    .fetch_one(sqlite_pool)
                    .await?;
                Ok(count)
            }
        }
    }

    /// Get teams by organization ID
    pub async fn get_teams_by_org(
        db: &crate::db::DatabasePool,
        org_id: &Uuid,
    ) -> Result<Vec<Team>, TeamError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let teams = sqlx::query_as::<_, Team>(
                    "SELECT team_id, org_id, name, active, created_at, created_by_user_id, updated_at, updated_by_user_id, active_toggle_at, active_toggle_by_user_id FROM team WHERE org_id = $1 ORDER BY created_at"
                )
                .bind(org_id)
                .fetch_all(pg_pool)
                .await?;
                Ok(teams)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let teams = sqlx::query_as::<_, Team>(
                    "SELECT team_id, org_id, name, active, created_at, created_by_user_id, updated_at, updated_by_user_id, active_toggle_at, active_toggle_by_user_id FROM team WHERE org_id = ? ORDER BY created_at"
                )
                .bind(org_id)
                .fetch_all(sqlite_pool)
                .await?;
                Ok(teams)
            }
        }
    }

    /// Get teams by user ID
    pub async fn get_teams_by_user(
        db: &crate::db::DatabasePool,
        user_id: &Uuid,
    ) -> Result<Vec<Team>, TeamError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let teams = sqlx::query_as::<_, Team>(
                    r#"
                    SELECT t.team_id, t.org_id, t.name, t.active, t.created_at, t.created_by_user_id, t.updated_at, t.updated_by_user_id, t.active_toggle_at, t.active_toggle_by_user_id
                    FROM team t
                    INNER JOIN team_user tu ON t.team_id = tu.team_id
                    WHERE tu.user_id = $1
                    ORDER BY t.created_at
                    "#,
                )
                .bind(user_id)
                .fetch_all(pg_pool)
                .await?;
                Ok(teams)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let teams = sqlx::query_as::<_, Team>(
                    r#"
                    SELECT t.team_id, t.org_id, t.name, t.active, t.created_at, t.created_by_user_id, t.updated_at, t.updated_by_user_id, t.active_toggle_at, t.active_toggle_by_user_id
                    FROM team t
                    INNER JOIN team_user tu ON t.team_id = tu.team_id
                    WHERE tu.user_id = ?
                    ORDER BY t.created_at
                    "#,
                )
                .bind(user_id)
                .fetch_all(sqlite_pool)
                .await?;
                Ok(teams)
            }
        }
    }

    /// Insert a new team
    pub async fn insert_team(
        db: &crate::db::DatabasePool,
        team_id: &Uuid,
        org_id: &Uuid,
        name: &str,
        created_by_user_id: &Uuid,
    ) -> Result<(), TeamError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                sqlx::query("INSERT INTO team (team_id, org_id, name, created_by_user_id) VALUES ($1, $2, $3, $4)")
                    .bind(team_id)
                    .bind(org_id)
                    .bind(name)
                    .bind(created_by_user_id)
                    .execute(pg_pool)
                    .await?;
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                sqlx::query("INSERT INTO team (team_id, org_id, name, created_by_user_id) VALUES (?, ?, ?, ?)")
                    .bind(team_id)
                    .bind(org_id)
                    .bind(name)
                    .bind(created_by_user_id)
                    .execute(sqlite_pool)
                    .await?;
            }
        }
        Ok(())
    }

    /// Update team name
    pub async fn update_name(
        db: &crate::db::DatabasePool,
        team_id: &Uuid,
        name: &str,
    ) -> Result<(), TeamError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                sqlx::query("UPDATE team SET name = $2, updated_at = NOW() WHERE team_id = $1")
                    .bind(team_id)
                    .bind(name)
                    .execute(pg_pool)
                    .await?;
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                sqlx::query(
                    "UPDATE team SET name = ?, updated_at = CURRENT_TIMESTAMP WHERE team_id = ?",
                )
                .bind(name)
                .bind(team_id)
                .execute(sqlite_pool)
                .await?;
            }
        }
        Ok(())
    }

    /// Delete team
    pub async fn delete_team(
        db: &crate::db::DatabasePool,
        team_id: &Uuid,
    ) -> Result<(), TeamError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                sqlx::query("DELETE FROM team WHERE team_id = $1")
                    .bind(team_id)
                    .execute(pg_pool)
                    .await?;
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                sqlx::query("DELETE FROM team WHERE team_id = ?")
                    .bind(team_id)
                    .execute(sqlite_pool)
                    .await?;
            }
        }
        Ok(())
    }
}

impl TeamUser {
    /// Get team user relationship
    pub async fn get_team_user(
        db: &crate::db::DatabasePool,
        team_id: &Uuid,
        user_id: &Uuid,
    ) -> Result<TeamUser, TeamError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let team_user = sqlx::query_as::<_, TeamUser>(
                    "SELECT team_user_id, team_id, user_id, team_user_role_id, active, created_at, created_by_user_id, updated_at, updated_by_user_id, active_toggle_at, active_toggle_by_user_id FROM team_user WHERE team_id = $1 AND user_id = $2",
                )
                .bind(team_id)
                .bind(user_id)
                .fetch_one(pg_pool)
                .await?;
                Ok(team_user)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let team_user = sqlx::query_as::<_, TeamUser>(
                    "SELECT team_user_id, team_id, user_id, team_user_role_id, active, created_at, created_by_user_id, updated_at, updated_by_user_id, active_toggle_at, active_toggle_by_user_id FROM team_user WHERE team_id = ? AND user_id = ?",
                )
                .bind(team_id)
                .bind(user_id)
                .fetch_one(sqlite_pool)
                .await?;
                Ok(team_user)
            }
        }
    }

    /// Insert team user relationship
    pub async fn insert_team_user(
        db: &crate::db::DatabasePool,
        team_user_id: &Uuid,
        team_id: &Uuid,
        user_id: &Uuid,
        role_id: Option<i16>,
        created_by_user_id: &Uuid,
    ) -> Result<(), TeamError> {
        let role_id = role_id.unwrap_or(1); // Default to member role

        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                sqlx::query("INSERT INTO team_user (team_user_id, team_id, user_id, team_user_role_id, created_by_user_id) VALUES ($1, $2, $3, $4, $5)")
                    .bind(team_user_id)
                    .bind(team_id)
                    .bind(user_id)
                    .bind(role_id)
                    .bind(created_by_user_id)
                    .execute(pg_pool)
                    .await?;
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                sqlx::query("INSERT INTO team_user (team_user_id, team_id, user_id, team_user_role_id, created_by_user_id) VALUES (?, ?, ?, ?, ?)")
                    .bind(team_user_id)
                    .bind(team_id)
                    .bind(user_id)
                    .bind(role_id)
                    .bind(created_by_user_id)
                    .execute(sqlite_pool)
                    .await?;
            }
        }
        Ok(())
    }

    /// Get users by team
    pub async fn get_users_by_team(
        db: &crate::db::DatabasePool,
        team_id: &Uuid,
    ) -> Result<Vec<TeamUser>, TeamError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let users = sqlx::query_as::<_, TeamUser>(
                    "SELECT team_user_id, team_id, user_id, team_user_role_id, active, created_at, created_by_user_id, updated_at, updated_by_user_id, active_toggle_at, active_toggle_by_user_id FROM team_user WHERE team_id = $1",
                )
                .bind(team_id)
                .fetch_all(pg_pool)
                .await?;
                Ok(users)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let users = sqlx::query_as::<_, TeamUser>(
                    "SELECT team_user_id, team_id, user_id, team_user_role_id, active, created_at, created_by_user_id, updated_at, updated_by_user_id, active_toggle_at, active_toggle_by_user_id FROM team_user WHERE team_id = ?",
                )
                .bind(team_id)
                .fetch_all(sqlite_pool)
                .await?;
                Ok(users)
            }
        }
    }

    /// Get users with roles by team
    pub async fn get_users_with_roles_by_team(
        db: &crate::db::DatabasePool,
        team_id: &Uuid,
    ) -> Result<Vec<TeamUserWithRole>, TeamError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let users = sqlx::query_as::<_, TeamUserWithRole>(
                    r#"
                    SELECT u.user_id, u.email, u.name, tu.team_user_role_id, tur.role, tu.active, tu.created_at
                    FROM "user" u
                    INNER JOIN team_user tu ON u.user_id = tu.user_id
                    INNER JOIN team_user_role tur ON tu.team_user_role_id = tur.team_user_role_id
                    WHERE tu.team_id = $1
                    ORDER BY u.email
                    "#,
                )
                .bind(team_id)
                .fetch_all(pg_pool)
                .await?;
                Ok(users)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let users = sqlx::query_as::<_, TeamUserWithRole>(
                    r#"
                    SELECT u.user_id, u.email, u.name, tu.team_user_role_id, tur.role, tu.active, tu.created_at
                    FROM user u
                    INNER JOIN team_user tu ON u.user_id = tu.user_id
                    INNER JOIN team_user_role tur ON tu.team_user_role_id = tur.team_user_role_id
                    WHERE tu.team_id = ?
                    ORDER BY u.email
                    "#,
                )
                .bind(team_id)
                .fetch_all(sqlite_pool)
                .await?;
                Ok(users)
            }
        }
    }

    /// Upsert team user
    pub async fn upsert_team_user(
        db: &crate::db::DatabasePool,
        team_id: &Uuid,
        user_id: &Uuid,
        role_id: i16,
        updated_by_user_id: &Uuid,
    ) -> Result<(), TeamError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                sqlx::query(
                    r#"
                    INSERT INTO team_user (team_user_id, team_id, user_id, team_user_role_id, created_by_user_id)
                    VALUES (uuidv7(), $1, $2, $3, $4)
                    ON CONFLICT (team_id, user_id)
                    DO UPDATE SET team_user_role_id = $3, updated_at = NOW(), updated_by_user_id = $4
                    "#,
                )
                .bind(team_id)
                .bind(user_id)
                .bind(role_id)
                .bind(updated_by_user_id)
                .execute(pg_pool)
                .await?;
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                sqlx::query(
                    r#"
                    INSERT INTO team_user (team_user_id, team_id, user_id, team_user_role_id, created_by_user_id)
                    VALUES (?, ?, ?, ?, ?)
                    ON CONFLICT (team_id, user_id)
                    DO UPDATE SET team_user_role_id = ?, updated_at = CURRENT_TIMESTAMP, updated_by_user_id = ?
                    "#,
                )
                .bind(Uuid::now_v7())
                .bind(team_id)
                .bind(user_id)
                .bind(role_id)
                .bind(updated_by_user_id)
                .bind(role_id)
                .bind(updated_by_user_id)
                .execute(sqlite_pool)
                .await?;
            }
        }
        Ok(())
    }

    /// Update team user relationship
    pub async fn update_team_user(
        db: &crate::db::DatabasePool,
        team_id: &Uuid,
        user_id: &Uuid,
        role_id: i16,
        active: bool,
        updated_by_user_id: &Uuid,
    ) -> Result<(), TeamError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                sqlx::query("UPDATE team_user SET team_user_role_id = $3, active = $4, updated_at = NOW(), updated_by_user_id = $5 WHERE team_id = $1 AND user_id = $2")
                    .bind(team_id)
                    .bind(user_id)
                    .bind(role_id)
                    .bind(active)
                    .bind(updated_by_user_id)
                    .execute(pg_pool)
                    .await?;
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let active_int = if active { 1 } else { 0 };
                sqlx::query("UPDATE team_user SET team_user_role_id = ?, active = ?, updated_at = CURRENT_TIMESTAMP, updated_by_user_id = ? WHERE team_id = ? AND user_id = ?")
                    .bind(role_id)
                    .bind(active_int)
                    .bind(updated_by_user_id)
                    .bind(team_id)
                    .bind(user_id)
                    .execute(sqlite_pool)
                    .await?;
            }
        }
        Ok(())
    }

    /// Remove user from team
    pub async fn remove_team_user(
        db: &crate::db::DatabasePool,
        team_id: &Uuid,
        user_id: &Uuid,
    ) -> Result<(), TeamError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                sqlx::query("DELETE FROM team_user WHERE team_id = $1 AND user_id = $2")
                    .bind(team_id)
                    .bind(user_id)
                    .execute(pg_pool)
                    .await?;
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                sqlx::query("DELETE FROM team_user WHERE team_id = ? AND user_id = ?")
                    .bind(team_id)
                    .bind(user_id)
                    .execute(sqlite_pool)
                    .await?;
            }
        }
        Ok(())
    }

    /// Check if user is team admin
    pub async fn is_team_admin(
        db: &crate::db::DatabasePool,
        team_id: &Uuid,
        user_id: &Uuid,
    ) -> Result<bool, TeamError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let count: i64 = sqlx::query_scalar(
                    "SELECT COUNT(*) FROM team_user WHERE team_id = $1 AND user_id = $2 AND team_user_role_id = 2 AND active = true",
                )
                .bind(team_id)
                .bind(user_id)
                .fetch_one(pg_pool)
                .await?;
                Ok(count > 0)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let count: i64 = sqlx::query_scalar(
                    "SELECT COUNT(*) FROM team_user WHERE team_id = ? AND user_id = ? AND team_user_role_id = 2 AND active = 1",
                )
                .bind(team_id)
                .bind(user_id)
                .fetch_one(sqlite_pool)
                .await?;
                Ok(count > 0)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn get_team_by_org_hides_cross_org_team() {
        let db = crate::db::test_db().await;
        let owner_org_id = Uuid::now_v7();
        let foreign_org_id = Uuid::now_v7();
        let team_id = Uuid::now_v7();
        let user_id = Uuid::now_v7();

        Team::insert_team(&db, &team_id, &owner_org_id, "Private Team", &user_id)
            .await
            .unwrap();

        let team = Team::get_team_by_org(&db, &team_id, &owner_org_id)
            .await
            .unwrap();
        assert_eq!(team.team_id, team_id);

        assert!(matches!(
            Team::get_team_by_org(&db, &team_id, &foreign_org_id).await,
            Err(TeamError::NotFound)
        ));
    }
}

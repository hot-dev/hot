use chrono::{DateTime, Utc};
use serde_json::Value as JsonValue;
use sqlx::FromRow;
use thiserror::Error;
use uuid::Uuid;

#[derive(Error, Debug)]
pub enum OrgError {
    #[error("Database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("Organization not found")]
    NotFound,
}

/// Organization settings structure stored as JSON
/// {
///   "display": {
///     "timezone": "America/New_York"  // IANA timezone; null = UTC default
///   }
/// }
///
/// NOTE on `org_type`: in Postgres this column is a custom enum type
/// (`CREATE TYPE org_type AS ENUM (...)`, see migration 022). sqlx cannot
/// decode a Postgres custom enum into a Rust `String` directly — it errors
/// with `ColumnDecode { ... is not compatible with SQL type "org_type" }`.
///
/// Every Postgres SELECT in this module therefore casts the column with
/// `org_type::text AS org_type`. Do NOT remove the cast or add a new
/// SELECT that pulls `org_type` without it, or every read of this struct
/// will fail at runtime (and silently break flows like `is_available` and
/// `get_org_by_slug`-based slug recovery in `create_org`).
#[derive(Debug, Clone, FromRow)]
pub struct Org {
    pub org_id: Uuid,
    pub name: String,
    pub slug: String,
    pub org_type: String,
    pub settings: Option<JsonValue>,
    pub active: bool,
    pub created_at: DateTime<Utc>,
    pub created_by_user_id: Uuid,
    pub updated_at: DateTime<Utc>,
    pub updated_by_user_id: Option<Uuid>,
    pub active_toggle_at: Option<DateTime<Utc>>,
    pub active_toggle_by_user_id: Option<Uuid>,
}

impl Org {
    pub fn is_individual(&self) -> bool {
        self.org_type == "individual"
    }

    pub fn is_organization(&self) -> bool {
        self.org_type == "organization"
    }
}

#[derive(Debug, FromRow)]
pub struct OrgUser {
    pub org_user_id: Uuid,
    pub org_id: Uuid,
    pub user_id: Uuid,
    pub org_user_role_id: i16,
    pub active: bool,
    pub created_at: DateTime<Utc>,
    pub created_by_user_id: Uuid,
    pub updated_at: DateTime<Utc>,
    pub updated_by_user_id: Option<Uuid>,
    pub active_toggle_at: Option<DateTime<Utc>>,
    pub active_toggle_by_user_id: Option<Uuid>,
}

#[derive(Debug, FromRow)]
pub struct OrgUserWithRole {
    pub user_id: Uuid,
    pub email: String,
    pub name: String,
    pub org_user_role_id: i16,
    pub role: String,
    pub active: bool,
    pub created_at: DateTime<Utc>,
}

impl Org {
    /// Get organization by ID
    pub async fn get_org(db: &crate::db::DatabasePool, org_id: &Uuid) -> Result<Org, OrgError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let org = sqlx::query_as::<_, Org>(
                    "SELECT org_id, name, slug, org_type::text AS org_type, settings, active, created_at, created_by_user_id, updated_at, updated_by_user_id, active_toggle_at, active_toggle_by_user_id FROM org WHERE org_id = $1",
                )
                .bind(org_id)
                .fetch_one(pg_pool)
                .await?;
                Ok(org)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let org = sqlx::query_as::<_, Org>(
                    "SELECT org_id, name, slug, org_type, settings, active, created_at, created_by_user_id, updated_at, updated_by_user_id, active_toggle_at, active_toggle_by_user_id FROM org WHERE org_id = ?"
                )
                .bind(org_id)
                .fetch_one(sqlite_pool)
                .await?;
                Ok(org)
            }
        }
    }

    /// Get count of organizations
    pub async fn get_count(db: &crate::db::DatabasePool) -> Result<i64, OrgError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM org")
                    .fetch_one(pg_pool)
                    .await?;
                Ok(count)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM org")
                    .fetch_one(sqlite_pool)
                    .await?;
                Ok(count)
            }
        }
    }

    /// Get the default (first) organization
    pub async fn get_default_org(db: &crate::db::DatabasePool) -> Result<Org, OrgError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let org = sqlx::query_as::<_, Org>(
                    "SELECT org_id, name, slug, org_type::text AS org_type, settings, active, created_at, created_by_user_id, updated_at, updated_by_user_id, active_toggle_at, active_toggle_by_user_id FROM org ORDER BY created_at LIMIT 1"
                )
                .fetch_one(pg_pool)
                .await?;
                Ok(org)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let org = sqlx::query_as::<_, Org>(
                    "SELECT org_id, name, slug, org_type, settings, active, created_at, created_by_user_id, updated_at, updated_by_user_id, active_toggle_at, active_toggle_by_user_id FROM org ORDER BY created_at LIMIT 1"
                )
                .fetch_one(sqlite_pool)
                .await?;
                Ok(org)
            }
        }
    }

    /// Delete an organization by its org_id.
    ///
    /// This is primarily used as a compensating action when a multi-step
    /// org-creation flow fails partway through (e.g. `insert_org` succeeded
    /// but `insert_org_user` failed) and we need to avoid leaving an orphan
    /// row behind. Prefer not to call this from normal business logic.
    pub async fn delete_by_id(db: &crate::db::DatabasePool, org_id: &Uuid) -> Result<(), OrgError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                sqlx::query("DELETE FROM org WHERE org_id = $1")
                    .bind(org_id)
                    .execute(pg_pool)
                    .await?;
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                sqlx::query("DELETE FROM org WHERE org_id = ?")
                    .bind(org_id)
                    .execute(sqlite_pool)
                    .await?;
            }
        }
        Ok(())
    }

    /// Insert a new organization with the given org_type ("individual" or "organization")
    pub async fn insert_org(
        db: &crate::db::DatabasePool,
        org_id: &Uuid,
        name: &str,
        slug: &str,
        org_type: &str,
        created_by_user_id: &Uuid,
    ) -> Result<(), OrgError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                sqlx::query("INSERT INTO org (org_id, name, slug, org_type, created_by_user_id) VALUES ($1, $2, $3, $4::org_type, $5)")
                    .bind(org_id)
                    .bind(name)
                    .bind(slug)
                    .bind(org_type)
                    .bind(created_by_user_id)
                    .execute(pg_pool)
                    .await?;
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                sqlx::query(
                    "INSERT INTO org (org_id, name, slug, org_type, created_by_user_id) VALUES (?, ?, ?, ?, ?)",
                )
                .bind(org_id)
                .bind(name)
                .bind(slug)
                .bind(org_type)
                .bind(created_by_user_id)
                .execute(sqlite_pool)
                .await?;
            }
        }
        Ok(())
    }

    /// Get all organizations by user ID
    pub async fn get_orgs_by_user(
        db: &crate::db::DatabasePool,
        user_id: &Uuid,
    ) -> Result<Vec<Org>, OrgError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let orgs = sqlx::query_as::<_, Org>(
                    r#"
                    SELECT o.org_id, o.name, o.slug, o.org_type::text AS org_type, o.settings, o.active, o.created_at, o.created_by_user_id, o.updated_at, o.updated_by_user_id, o.active_toggle_at, o.active_toggle_by_user_id
                    FROM org o
                    INNER JOIN org_user ou ON o.org_id = ou.org_id
                    WHERE ou.user_id = $1
                    ORDER BY o.created_at
                    "#,
                )
                .bind(user_id)
                .fetch_all(pg_pool)
                .await?;
                Ok(orgs)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let orgs = sqlx::query_as::<_, Org>(
                    r#"
                    SELECT o.org_id, o.name, o.slug, o.org_type, o.settings, o.active, o.created_at, o.created_by_user_id, o.updated_at, o.updated_by_user_id, o.active_toggle_at, o.active_toggle_by_user_id
                    FROM org o
                    INNER JOIN org_user ou ON o.org_id = ou.org_id
                    WHERE ou.user_id = ?
                    ORDER BY o.created_at
                    "#,
                )
                .bind(user_id)
                .fetch_all(sqlite_pool)
                .await?;
                Ok(orgs)
            }
        }
    }

    /// Get individual organization for a user (if they have one)
    pub async fn get_individual_org_by_user(
        db: &crate::db::DatabasePool,
        user_id: &Uuid,
    ) -> Result<Option<Org>, OrgError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let org = sqlx::query_as::<_, Org>(
                    r#"
                    SELECT o.org_id, o.name, o.slug, o.org_type::text AS org_type, o.settings, o.active, o.created_at, o.created_by_user_id, o.updated_at, o.updated_by_user_id, o.active_toggle_at, o.active_toggle_by_user_id
                    FROM org o
                    INNER JOIN org_user ou ON o.org_id = ou.org_id
                    WHERE ou.user_id = $1 AND o.org_type = 'individual'
                    LIMIT 1
                    "#,
                )
                .bind(user_id)
                .fetch_optional(pg_pool)
                .await?;
                Ok(org)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let org = sqlx::query_as::<_, Org>(
                    r#"
                    SELECT o.org_id, o.name, o.slug, o.org_type, o.settings, o.active, o.created_at, o.created_by_user_id, o.updated_at, o.updated_by_user_id, o.active_toggle_at, o.active_toggle_by_user_id
                    FROM org o
                    INNER JOIN org_user ou ON o.org_id = ou.org_id
                    WHERE ou.user_id = ? AND o.org_type = 'individual'
                    LIMIT 1
                    "#,
                )
                .bind(user_id)
                .fetch_optional(sqlite_pool)
                .await?;
                Ok(org)
            }
        }
    }

    /// Get organization by slug
    pub async fn get_org_by_slug(
        db: &crate::db::DatabasePool,
        slug: &str,
    ) -> Result<Org, OrgError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let org = sqlx::query_as::<_, Org>(
                    "SELECT org_id, name, slug, org_type::text AS org_type, settings, active, created_at, created_by_user_id, updated_at, updated_by_user_id, active_toggle_at, active_toggle_by_user_id FROM org WHERE slug = $1",
                )
                .bind(slug)
                .fetch_one(pg_pool)
                .await?;
                Ok(org)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let org = sqlx::query_as::<_, Org>(
                    "SELECT org_id, name, slug, org_type, settings, active, created_at, created_by_user_id, updated_at, updated_by_user_id, active_toggle_at, active_toggle_by_user_id FROM org WHERE slug = ?"
                )
                .bind(slug)
                .fetch_one(sqlite_pool)
                .await?;
                Ok(org)
            }
        }
    }

    /// Get all organizations with pagination
    pub async fn get_all_orgs(
        db: &crate::db::DatabasePool,
        limit: Option<i64>,
        offset: Option<i64>,
    ) -> Result<Vec<Org>, OrgError> {
        let limit = limit.unwrap_or(10);
        let offset = offset.unwrap_or(0);

        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let orgs = sqlx::query_as::<_, Org>(
                    "SELECT org_id, name, slug, org_type::text AS org_type, settings, active, created_at, created_by_user_id, updated_at, updated_by_user_id, active_toggle_at, active_toggle_by_user_id FROM org ORDER BY created_at DESC LIMIT $1 OFFSET $2",
                )
                .bind(limit)
                .bind(offset)
                .fetch_all(pg_pool)
                .await?;
                Ok(orgs)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let orgs = sqlx::query_as::<_, Org>(
                    "SELECT org_id, name, slug, org_type, settings, active, created_at, created_by_user_id, updated_at, updated_by_user_id, active_toggle_at, active_toggle_by_user_id FROM org ORDER BY created_at DESC LIMIT ? OFFSET ?",
                )
                .bind(limit)
                .bind(offset)
                .fetch_all(sqlite_pool)
                .await?;
                Ok(orgs)
            }
        }
    }

    /// Update organization
    pub async fn update_org(
        db: &crate::db::DatabasePool,
        org_id: &Uuid,
        name: &str,
        slug: &str,
        updated_by_user_id: &Uuid,
    ) -> Result<(), OrgError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                sqlx::query("UPDATE org SET name = $2, slug = $3, updated_at = NOW(), updated_by_user_id = $4 WHERE org_id = $1")
                    .bind(org_id)
                    .bind(name)
                    .bind(slug)
                    .bind(updated_by_user_id)
                    .execute(pg_pool)
                    .await?;
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                sqlx::query("UPDATE org SET name = ?, slug = ?, updated_at = CURRENT_TIMESTAMP, updated_by_user_id = ? WHERE org_id = ?")
                    .bind(name)
                    .bind(slug)
                    .bind(updated_by_user_id)
                    .bind(org_id)
                    .execute(sqlite_pool)
                    .await?;
            }
        }
        Ok(())
    }

    /// Update a specific setting path within the settings JSON
    pub async fn update_setting(
        db: &crate::db::DatabasePool,
        org_id: &Uuid,
        key: &str,
        value: &JsonValue,
        updated_by_user_id: &Uuid,
    ) -> Result<(), OrgError> {
        let update_obj = serde_json::json!({ key: value });

        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                sqlx::query(
                    "UPDATE org SET settings = COALESCE(settings, '{}'::jsonb) || $2::jsonb, updated_at = NOW(), updated_by_user_id = $3 WHERE org_id = $1",
                )
                .bind(org_id)
                .bind(&update_obj)
                .bind(updated_by_user_id)
                .execute(pg_pool)
                .await?;
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                // For SQLite, fetch, merge, then update
                let current = Self::get_org(db, org_id).await?;
                let mut settings = current.settings.unwrap_or(serde_json::json!({}));
                if let Some(obj) = settings.as_object_mut() {
                    obj.insert(key.to_string(), value.clone());
                }
                let settings_str = serde_json::to_string(&settings)
                    .map_err(|e| OrgError::Database(sqlx::Error::Encode(Box::new(e))))?;
                sqlx::query(
                    "UPDATE org SET settings = ?, updated_at = CURRENT_TIMESTAMP, updated_by_user_id = ? WHERE org_id = ?",
                )
                .bind(settings_str)
                .bind(updated_by_user_id)
                .bind(org_id)
                .execute(sqlite_pool)
                .await?;
            }
        }
        Ok(())
    }

    /// Update the display timezone setting for an organization
    /// Pass None to clear (will use UTC default)
    pub async fn update_display_timezone(
        db: &crate::db::DatabasePool,
        org_id: &Uuid,
        timezone: Option<&str>,
        updated_by_user_id: &Uuid,
    ) -> Result<(), OrgError> {
        let display_settings = match timezone {
            Some(tz) => serde_json::json!({ "timezone": tz }),
            None => serde_json::json!({ "timezone": null }),
        };
        Self::update_setting(db, org_id, "display", &display_settings, updated_by_user_id).await
    }

    /// Get the organization's display timezone from settings
    /// Returns None if not set (should use UTC default)
    pub fn get_display_timezone(&self) -> Option<String> {
        self.settings
            .as_ref()
            .and_then(|s| s.get("display"))
            .and_then(|d| d.get("timezone"))
            .and_then(|tz| tz.as_str())
            .map(|s| s.to_string())
    }
}

impl OrgUser {
    /// Insert organization user relationship
    pub async fn insert_org_user(
        db: &crate::db::DatabasePool,
        org_user_id: &Uuid,
        org_id: &Uuid,
        user_id: &Uuid,
        role_id: Option<i16>,
        created_by_user_id: &Uuid,
    ) -> Result<(), OrgError> {
        let role_id = role_id.unwrap_or(1); // Default to member role

        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                sqlx::query("INSERT INTO org_user (org_user_id, org_id, user_id, org_user_role_id, created_by_user_id) VALUES ($1, $2, $3, $4, $5)")
                    .bind(org_user_id)
                    .bind(org_id)
                    .bind(user_id)
                    .bind(role_id)
                    .bind(created_by_user_id)
                    .execute(pg_pool)
                    .await?;
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                sqlx::query("INSERT INTO org_user (org_user_id, org_id, user_id, org_user_role_id, created_by_user_id) VALUES (?, ?, ?, ?, ?)")
                    .bind(org_user_id)
                    .bind(org_id)
                    .bind(user_id)
                    .bind(role_id)
                    .bind(created_by_user_id)
                    .execute(sqlite_pool)
                    .await?;
            }
        }
        Ok(())
    }

    /// Get organization user relationship
    pub async fn get_org_user(
        db: &crate::db::DatabasePool,
        org_id: &Uuid,
        user_id: &Uuid,
    ) -> Result<OrgUser, OrgError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let org_user = sqlx::query_as::<_, OrgUser>(
                    "SELECT org_user_id, org_id, user_id, org_user_role_id, active, created_at, created_by_user_id, updated_at, updated_by_user_id, active_toggle_at, active_toggle_by_user_id FROM org_user WHERE org_id = $1 AND user_id = $2",
                )
                .bind(org_id)
                .bind(user_id)
                .fetch_one(pg_pool)
                .await?;
                Ok(org_user)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let org_user = sqlx::query_as::<_, OrgUser>(
                    "SELECT org_user_id, org_id, user_id, org_user_role_id, active, created_at, created_by_user_id, updated_at, updated_by_user_id, active_toggle_at, active_toggle_by_user_id FROM org_user WHERE org_id = ? AND user_id = ?"
                )
                .bind(org_id)
                .bind(user_id)
                .fetch_one(sqlite_pool)
                .await?;
                Ok(org_user)
            }
        }
    }

    /// Get users with roles by organization
    pub async fn get_users_with_roles_by_org(
        db: &crate::db::DatabasePool,
        org_id: &Uuid,
    ) -> Result<Vec<OrgUserWithRole>, OrgError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let users = sqlx::query_as::<_, OrgUserWithRole>(
                    r#"
                    SELECT u.user_id, u.email, u.name, ou.org_user_role_id, our.role, ou.active, ou.created_at
                    FROM "user" u
                    INNER JOIN org_user ou ON u.user_id = ou.user_id
                    INNER JOIN org_user_role our ON ou.org_user_role_id = our.org_user_role_id
                    WHERE ou.org_id = $1
                    ORDER BY u.email
                    "#,
                )
                .bind(org_id)
                .fetch_all(pg_pool)
                .await?;
                Ok(users)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let users = sqlx::query_as::<_, OrgUserWithRole>(
                    r#"
                    SELECT u.user_id, u.email, u.name, ou.org_user_role_id, our.role, ou.active, ou.created_at
                    FROM user u
                    INNER JOIN org_user ou ON u.user_id = ou.user_id
                    INNER JOIN org_user_role our ON ou.org_user_role_id = our.org_user_role_id
                    WHERE ou.org_id = ?
                    ORDER BY u.email
                    "#,
                )
                .bind(org_id)
                .fetch_all(sqlite_pool)
                .await?;
                Ok(users)
            }
        }
    }

    /// Get admin org_users for an organization (role_id = 2)
    pub async fn get_admins_for_org(
        db: &crate::db::DatabasePool,
        org_id: &Uuid,
    ) -> Result<Vec<OrgUser>, OrgError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let org_users = sqlx::query_as::<_, OrgUser>(
                    "SELECT org_user_id, org_id, user_id, org_user_role_id, active, created_at, created_by_user_id, updated_at, updated_by_user_id, active_toggle_at, active_toggle_by_user_id FROM org_user WHERE org_id = $1 AND org_user_role_id = 2 AND active = true",
                )
                .bind(org_id)
                .fetch_all(pg_pool)
                .await?;
                Ok(org_users)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let org_users = sqlx::query_as::<_, OrgUser>(
                    "SELECT org_user_id, org_id, user_id, org_user_role_id, active, created_at, created_by_user_id, updated_at, updated_by_user_id, active_toggle_at, active_toggle_by_user_id FROM org_user WHERE org_id = ? AND org_user_role_id = 2 AND active = 1"
                )
                .bind(org_id)
                .fetch_all(sqlite_pool)
                .await?;
                Ok(org_users)
            }
        }
    }

    /// Update organization user relationship
    pub async fn update_org_user(
        db: &crate::db::DatabasePool,
        org_id: &Uuid,
        user_id: &Uuid,
        role_id: i16,
        active: bool,
        updated_by_user_id: &Uuid,
    ) -> Result<(), OrgError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                sqlx::query("UPDATE org_user SET org_user_role_id = $3, active = $4, updated_at = NOW(), updated_by_user_id = $5 WHERE org_id = $1 AND user_id = $2")
                    .bind(org_id)
                    .bind(user_id)
                    .bind(role_id)
                    .bind(active)
                    .bind(updated_by_user_id)
                    .execute(pg_pool)
                    .await?;
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let active_int = if active { 1 } else { 0 };
                sqlx::query("UPDATE org_user SET org_user_role_id = ?, active = ?, updated_at = CURRENT_TIMESTAMP, updated_by_user_id = ? WHERE org_id = ? AND user_id = ?")
                    .bind(role_id)
                    .bind(active_int)
                    .bind(updated_by_user_id)
                    .bind(org_id)
                    .bind(user_id)
                    .execute(sqlite_pool)
                    .await?;
            }
        }
        Ok(())
    }

    /// Count active members in an organization
    pub async fn count_active_members(
        db: &crate::db::DatabasePool,
        org_id: &Uuid,
    ) -> Result<i64, OrgError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let row: (i64,) = sqlx::query_as(
                    "SELECT COUNT(*) FROM org_user WHERE org_id = $1 AND active = true",
                )
                .bind(org_id)
                .fetch_one(pg_pool)
                .await?;
                Ok(row.0)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let row: (i64,) =
                    sqlx::query_as("SELECT COUNT(*) FROM org_user WHERE org_id = ? AND active = 1")
                        .bind(org_id)
                        .fetch_one(sqlite_pool)
                        .await?;
                Ok(row.0)
            }
        }
    }

    /// Remove organization user relationship
    pub async fn remove_org_user(
        db: &crate::db::DatabasePool,
        org_id: &Uuid,
        user_id: &Uuid,
    ) -> Result<(), OrgError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                sqlx::query("DELETE FROM org_user WHERE org_id = $1 AND user_id = $2")
                    .bind(org_id)
                    .bind(user_id)
                    .execute(pg_pool)
                    .await?;
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                sqlx::query("DELETE FROM org_user WHERE org_id = ? AND user_id = ?")
                    .bind(org_id)
                    .bind(user_id)
                    .execute(sqlite_pool)
                    .await?;
            }
        }
        Ok(())
    }
}

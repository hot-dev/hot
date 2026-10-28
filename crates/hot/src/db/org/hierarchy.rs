//! Organization hierarchy control plane. Data ownership always remains the
//! selected organization; billing and effective human grants are resolved here.
use super::{Org, OrgError};
use crate::db::{BootstrapOperation, DatabasePool, DatabaseScope, Features, MaintenanceOperation};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OrgLifecycle {
    Active,
    Suspended,
    Archived,
}
impl OrgLifecycle {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Suspended => "suspended",
            Self::Archived => "archived",
        }
    }
}

/// Current effective authority, including provenance. `is_admin` alone does not
/// authorize runtime access to a suspended organization or root account actions.
#[derive(Debug, Clone, Serialize)]
pub struct OrgAccess {
    pub root_org_id: Uuid,
    pub org_id: Uuid,
    pub is_admin: bool,
    pub inherited: bool,
    pub can_manage_account: bool,
    pub runtime_allowed: bool,
}

#[derive(Debug, Clone)]
pub struct CreateChild {
    pub name: String,
    pub slug: String,
    pub external_id: Option<String>,
    pub idempotency_key: String,
    /// Trusted deployment configuration, never a value accepted from an API body.
    pub hosted: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ChildProvisioned {
    pub org: Org,
    pub env_id: Uuid,
    pub created: bool,
}

impl CreateChild {
    fn validate(&self) -> Result<(), OrgError> {
        if self.name.trim().is_empty() || self.name.len() > 200 {
            return Err(OrgError::Invalid("name must contain 1–200 bytes".into()));
        }
        if self.slug.is_empty()
            || self.slug.len() > 63
            || !self
                .slug
                .bytes()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
            || self.slug.starts_with('-')
            || self.slug.ends_with('-')
        {
            return Err(OrgError::Invalid("invalid organization slug".into()));
        }
        if self.idempotency_key.trim().is_empty() || self.idempotency_key.len() > 128 {
            return Err(OrgError::Invalid(
                "idempotency key must contain 1–128 bytes".into(),
            ));
        }
        if self
            .external_id
            .as_ref()
            .is_some_and(|s| s.trim().is_empty() || s.len() > 255)
        {
            return Err(OrgError::Invalid(
                "external ID must contain 1–255 bytes".into(),
            ));
        }
        Ok(())
    }
    fn fingerprint(&self) -> String {
        serde_json::to_string(&(&self.name, &self.slug, &self.external_id))
            .expect("string tuple serializes")
    }
}

impl Org {
    pub fn billing_owner_id(&self) -> Uuid {
        self.parent_org_id.unwrap_or(self.org_id)
    }
    pub fn is_child(&self) -> bool {
        self.parent_org_id.is_some()
    }
    pub fn is_admitting(&self) -> bool {
        self.active && self.lifecycle_state == "active"
    }

    /// Fresh identity/status read. Authorization must never depend on entity-cache TTL.
    pub async fn get_org_uncached(db: &DatabasePool, org_id: &Uuid) -> Result<Self, OrgError> {
        match db {
            DatabasePool::Postgres(pool) => Ok(sqlx::query_as::<_, Self>(
                "SELECT org_id, parent_org_id, external_id, lifecycle_state, name, slug,
                 org_type::text AS org_type, settings, active, created_at, created_by_user_id,
                 updated_at, updated_by_user_id, active_toggle_at, active_toggle_by_user_id
                 FROM org WHERE org_id = $1",
            )
            .bind(org_id)
            .fetch_one(pool)
            .await?),
            DatabasePool::Sqlite(pool) => Ok(sqlx::query_as::<_, Self>(
                "SELECT org_id, parent_org_id, external_id, lifecycle_state, name, slug,
                 org_type, settings, active, created_at, created_by_user_id, updated_at,
                 updated_by_user_id, active_toggle_at, active_toggle_by_user_id
                 FROM org WHERE org_id = ?",
            )
            .bind(org_id)
            .fetch_one(pool)
            .await?),
        }
    }

    pub async fn billing_owner_id_for(db: &DatabasePool, org_id: &Uuid) -> Result<Uuid, OrgError> {
        let db = db.with_scope(DatabaseScope::Bootstrap(BootstrapOperation::Membership));
        Ok(Self::get_org_uncached(&db, org_id)
            .await?
            .billing_owner_id())
    }

    /// A handle with billing authority over the account that owns `org_id`,
    /// for trusted billing code (such as a provider webhook) once it has
    /// resolved the account from verified provider data.
    pub async fn billing_scope(db: &DatabasePool, org_id: &Uuid) -> Result<DatabasePool, OrgError> {
        let root_id = Self::billing_owner_id_for(db, org_id).await?;
        Ok(db.with_scope(DatabaseScope::Maintenance {
            operation: MaintenanceOperation::Billing,
            org_id: Some(root_id),
        }))
    }

    /// None means the identity has no effective grant. A root admin can have an
    /// administrative grant with runtime_allowed=false for billing/lifecycle recovery.
    pub async fn resolve_user_access(
        db: &DatabasePool,
        org_id: &Uuid,
        user_id: &Uuid,
    ) -> Result<Option<OrgAccess>, OrgError> {
        let db = db.with_scope(DatabaseScope::Bootstrap(BootstrapOperation::Membership));
        let user_active: bool = match &db {
            DatabasePool::Postgres(pool) => sqlx::query_scalar(
                "SELECT COALESCE(active, false) FROM \"user\" WHERE user_id = $1",
            )
            .bind(user_id)
            .fetch_optional(pool)
            .await?
            .unwrap_or(false),
            DatabasePool::Sqlite(pool) => {
                sqlx::query_scalar("SELECT COALESCE(active, 0) FROM user WHERE user_id = ?")
                    .bind(user_id)
                    .fetch_optional(pool)
                    .await?
                    .unwrap_or(false)
            }
        };
        if !user_active {
            return Ok(None);
        }
        let org = match Self::get_org_uncached(&db, org_id).await {
            Ok(org) => org,
            Err(OrgError::Database(sqlx::Error::RowNotFound)) => return Ok(None),
            Err(error) => return Err(error),
        };
        let root_id = org.billing_owner_id();
        let root = if org.is_child() {
            Self::get_org_uncached(&db, &root_id).await?
        } else {
            org.clone()
        };
        let root_role = active_role(&db, &root_id, user_id).await?;
        let Some(root_role) = root_role else {
            return Ok(None);
        };
        let root_admin = root_role == 2;
        let role = if !org.is_child() || root_admin {
            Some(root_role)
        } else {
            active_role(&db, org_id, user_id).await?
        };
        let Some(role) = role else {
            return Ok(None);
        };
        let runtime_allowed = root.is_admitting() && org.is_admitting();
        if !runtime_allowed && !root_admin {
            return Ok(None);
        }
        Ok(Some(OrgAccess {
            root_org_id: root_id,
            org_id: *org_id,
            is_admin: role == 2,
            inherited: org.is_child() && root_admin,
            can_manage_account: root_admin,
            runtime_allowed,
        }))
    }

    /// Admission is separate from permission and is checked at ingress/start.
    /// Existing durable work may still finalize under its persisted exact scope.
    pub async fn is_environment_admitted(
        db: &DatabasePool,
        env_id: &Uuid,
    ) -> Result<bool, OrgError> {
        let db = db.with_scope(DatabaseScope::Bootstrap(BootstrapOperation::Membership));
        match &db {
            DatabasePool::Postgres(pool) => Ok(sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM env e JOIN org o ON o.org_id = e.org_id
                 JOIN org r ON r.org_id = COALESCE(o.parent_org_id, o.org_id)
                 WHERE e.env_id = $1 AND e.active = true AND o.active = true AND r.active = true
                 AND o.lifecycle_state = 'active' AND r.lifecycle_state = 'active')",
            )
            .bind(env_id)
            .fetch_one(pool)
            .await?),
            DatabasePool::Sqlite(pool) => Ok(sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM env e JOIN org o ON o.org_id = e.org_id
                 JOIN org r ON r.org_id = COALESCE(o.parent_org_id, o.org_id)
                 WHERE e.env_id = ? AND e.active = 1 AND o.active = 1 AND r.active = 1
                 AND o.lifecycle_state = 'active' AND r.lifecycle_state = 'active')",
            )
            .bind(env_id)
            .fetch_one(pool)
            .await?),
        }
    }

    /// Caller must authorize root discovery before returning these records.
    pub async fn list_children(db: &DatabasePool, root_id: &Uuid) -> Result<Vec<Self>, OrgError> {
        let db = db.with_scope(DatabaseScope::Bootstrap(BootstrapOperation::Membership));
        match &db {
            DatabasePool::Postgres(pool) => Ok(sqlx::query_as::<_, Self>(
                "SELECT org_id, parent_org_id, external_id, lifecycle_state, name, slug,
                 org_type::text AS org_type, settings, active, created_at, created_by_user_id,
                 updated_at, updated_by_user_id, active_toggle_at, active_toggle_by_user_id
                 FROM org WHERE parent_org_id = $1 ORDER BY created_at, org_id",
            )
            .bind(root_id)
            .fetch_all(pool)
            .await?),
            DatabasePool::Sqlite(pool) => Ok(sqlx::query_as::<_, Self>(
                "SELECT org_id, parent_org_id, external_id, lifecycle_state, name, slug,
                 org_type, settings, active, created_at, created_by_user_id, updated_at,
                 updated_by_user_id, active_toggle_at, active_toggle_by_user_id
                 FROM org WHERE parent_org_id = ? ORDER BY created_at, org_id",
            )
            .bind(root_id)
            .fetch_all(pool)
            .await?),
        }
    }

    /// Atomic fixed one-second account request budget, shared by all API
    /// replicas and child organizations. Unlimited budgets allocate no counter.
    pub async fn try_acquire_account_request(
        db: &DatabasePool,
        org_id: &Uuid,
        limit: i64,
    ) -> Result<bool, OrgError> {
        if limit < 0 {
            return Ok(true);
        }
        if limit == 0 {
            return Ok(false);
        }
        let root_id = Self::billing_owner_id_for(db, org_id).await?;
        let db = db.with_scope(DatabaseScope::Maintenance {
            operation: MaintenanceOperation::Billing,
            org_id: Some(root_id),
        });
        let acquired: Option<i64> =
            match &db {
                DatabasePool::Postgres(pool) => sqlx::query_scalar(
                    "INSERT INTO account_request_window (root_org_id,window_start,request_count)
                 VALUES ($1,FLOOR(EXTRACT(EPOCH FROM clock_timestamp()))::bigint,1)
                 ON CONFLICT (root_org_id) DO UPDATE SET
                 window_start=EXCLUDED.window_start,
                 request_count=CASE WHEN account_request_window.window_start=EXCLUDED.window_start
                    THEN account_request_window.request_count+1 ELSE 1 END
                 WHERE account_request_window.window_start<>EXCLUDED.window_start
                    OR account_request_window.request_count<$2
                 RETURNING request_count",
                )
                .bind(root_id)
                .bind(limit)
                .fetch_optional(pool)
                .await?,
                DatabasePool::Sqlite(pool) => sqlx::query_scalar(
                    "INSERT INTO account_request_window (root_org_id,window_start,request_count)
                 VALUES (?,CAST(strftime('%s','now') AS integer),1)
                 ON CONFLICT (root_org_id) DO UPDATE SET
                 window_start=excluded.window_start,
                 request_count=CASE WHEN account_request_window.window_start=excluded.window_start
                    THEN account_request_window.request_count+1 ELSE 1 END
                 WHERE account_request_window.window_start<>excluded.window_start
                    OR account_request_window.request_count<?
                 RETURNING request_count",
                )
                .bind(root_id)
                .bind(limit)
                .fetch_optional(pool)
                .await?,
            };
        Ok(acquired.is_some())
    }

    pub async fn create_child(
        db: &DatabasePool,
        root_id: &Uuid,
        actor_id: &Uuid,
        request: &CreateChild,
    ) -> Result<ChildProvisioned, OrgError> {
        require_root_admin(db, root_id, actor_id).await?;
        Self::create_child_authorized(db, root_id, actor_id, request).await
    }

    /// Host-only management operation. The caller MUST have validated a current
    /// root management capability. actor_id is attribution, not machine authority.
    pub async fn create_child_authorized(
        db: &DatabasePool,
        root_id: &Uuid,
        actor_id: &Uuid,
        request: &CreateChild,
    ) -> Result<ChildProvisioned, OrgError> {
        request.validate()?;
        let db = db.with_scope(DatabaseScope::Maintenance {
            operation: MaintenanceOperation::OrganizationProvisioning,
            org_id: Some(*root_id),
        });
        let fingerprint = request.fingerprint();
        let org_id = Uuid::now_v7();
        let env_id = Uuid::now_v7();
        let note_id = Uuid::now_v7();
        let (result_org, result_env, created) = match &db {
            DatabasePool::Postgres(pool) => {
                let mut tx = pool.begin().await?;
                let root: (Option<Uuid>, String, String, Option<Value>) = sqlx::query_as(
                    "SELECT parent_org_id, org_type::text, lifecycle_state, features FROM org WHERE org_id = $1 FOR UPDATE"
                ).bind(root_id).fetch_one(&mut *tx).await?;
                validate_parent(&root.0, &root.1, &root.2)?;
                let prior: Option<(Uuid, Uuid, String)> = sqlx::query_as(
                    "SELECT child_org_id, env_id, request_fingerprint FROM org_child_provisioning WHERE root_org_id = $1 AND idempotency_key = $2"
                ).bind(root_id).bind(&request.idempotency_key).fetch_optional(&mut *tx).await?;
                if let Some((child, env, saved)) = prior {
                    if saved != fingerprint {
                        return Err(OrgError::Invalid(
                            "idempotency key was used with different parameters".into(),
                        ));
                    }
                    tx.commit().await?;
                    (child, env, false)
                } else {
                    let plan: Option<Value> = sqlx::query_scalar(
                        "SELECT p.features FROM org_plan op JOIN plan p ON p.plan_uuid = op.plan_uuid WHERE op.org_id = $1"
                    ).bind(root_id).fetch_optional(&mut *tx).await?.flatten();
                    let limit = child_limit(plan.as_ref(), root.3.as_ref(), request.hosted);
                    let count: i64 =
                        sqlx::query_scalar("SELECT COUNT(*) FROM org WHERE parent_org_id = $1")
                            .bind(root_id)
                            .fetch_one(&mut *tx)
                            .await?;
                    if limit >= 0 && count >= limit {
                        return Err(OrgError::LimitReached);
                    }
                    let env_limit = environment_limit(plan.as_ref(), root.3.as_ref());
                    let env_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM env e JOIN org o ON o.org_id=e.org_id WHERE o.org_id=$1 OR o.parent_org_id=$1")
                        .bind(root_id).fetch_one(&mut *tx).await?;
                    if env_limit >= 0 && env_count >= env_limit {
                        return Err(OrgError::LimitReached);
                    }
                    sqlx::query("INSERT INTO org (org_id, parent_org_id, name, slug, external_id, org_type, created_by_user_id) VALUES ($1,$2,$3,$4,$5,'organization',$6)")
                        .bind(org_id).bind(root_id).bind(&request.name).bind(&request.slug).bind(&request.external_id).bind(actor_id).execute(&mut *tx).await?;
                    sqlx::query("INSERT INTO env (env_id, org_id, name, created_by_user_id) VALUES ($1,$2,'development',$3)")
                        .bind(env_id).bind(org_id).bind(actor_id).execute(&mut *tx).await?;
                    sqlx::query("INSERT INTO org_child_provisioning (root_org_id,idempotency_key,child_org_id,env_id,request_fingerprint) VALUES ($1,$2,$3,$4,$5)")
                        .bind(root_id).bind(&request.idempotency_key).bind(org_id).bind(env_id).bind(&fingerprint).execute(&mut *tx).await?;
                    sqlx::query("INSERT INTO org_note (note_id,org_id,category,note_type,message,metadata,created_by) VALUES ($1,$2,'security','child_created','Child organization created',$3,$4)")
                        .bind(note_id).bind(root_id).bind(serde_json::json!({"child_org_id":org_id,"env_id":env_id})).bind(actor_id).execute(&mut *tx).await?;
                    tx.commit().await?;
                    (org_id, env_id, true)
                }
            }
            DatabasePool::Sqlite(pool) => {
                let mut tx = pool.begin().await?;
                // First statement is a write: serializes admission before taking a read snapshot.
                sqlx::query("UPDATE org SET updated_at = updated_at WHERE org_id = ?")
                    .bind(root_id)
                    .execute(&mut *tx)
                    .await?;
                let root: (Option<Uuid>, String, String, Option<String>) = sqlx::query_as(
                    "SELECT parent_org_id, org_type, lifecycle_state, features FROM org WHERE org_id = ?"
                ).bind(root_id).fetch_one(&mut *tx).await?;
                validate_parent(&root.0, &root.1, &root.2)?;
                let prior: Option<(Uuid, Uuid, String)> = sqlx::query_as(
                    "SELECT child_org_id, env_id, request_fingerprint FROM org_child_provisioning WHERE root_org_id = ? AND idempotency_key = ?"
                ).bind(root_id).bind(&request.idempotency_key).fetch_optional(&mut *tx).await?;
                if let Some((child, env, saved)) = prior {
                    if saved != fingerprint {
                        return Err(OrgError::Invalid(
                            "idempotency key was used with different parameters".into(),
                        ));
                    }
                    tx.commit().await?;
                    (child, env, false)
                } else {
                    let plan: Option<String> = sqlx::query_scalar(
                        "SELECT p.features FROM org_plan op JOIN plan p ON p.plan_uuid = op.plan_uuid WHERE op.org_id = ?"
                    ).bind(root_id).fetch_optional(&mut *tx).await?.flatten();
                    let plan = parse_features(plan)?;
                    let overrides = parse_features(root.3)?;
                    let limit = child_limit(plan.as_ref(), overrides.as_ref(), request.hosted);
                    let count: i64 =
                        sqlx::query_scalar("SELECT COUNT(*) FROM org WHERE parent_org_id = ?")
                            .bind(root_id)
                            .fetch_one(&mut *tx)
                            .await?;
                    if limit >= 0 && count >= limit {
                        return Err(OrgError::LimitReached);
                    }
                    let env_limit = environment_limit(plan.as_ref(), overrides.as_ref());
                    let env_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM env e JOIN org o ON o.org_id=e.org_id WHERE o.org_id=?1 OR o.parent_org_id=?1")
                        .bind(root_id).fetch_one(&mut *tx).await?;
                    if env_limit >= 0 && env_count >= env_limit {
                        return Err(OrgError::LimitReached);
                    }
                    sqlx::query("INSERT INTO org (org_id, parent_org_id, name, slug, external_id, org_type, created_by_user_id) VALUES (?,?,?,?,?,'organization',?)")
                        .bind(org_id).bind(root_id).bind(&request.name).bind(&request.slug).bind(&request.external_id).bind(actor_id).execute(&mut *tx).await?;
                    sqlx::query("INSERT INTO env (env_id, org_id, name, created_by_user_id) VALUES (?,?,'development',?)")
                        .bind(env_id).bind(org_id).bind(actor_id).execute(&mut *tx).await?;
                    sqlx::query("INSERT INTO org_child_provisioning (root_org_id,idempotency_key,child_org_id,env_id,request_fingerprint) VALUES (?,?,?,?,?)")
                        .bind(root_id).bind(&request.idempotency_key).bind(org_id).bind(env_id).bind(&fingerprint).execute(&mut *tx).await?;
                    sqlx::query("INSERT INTO org_note (note_id,org_id,category,note_type,message,metadata,created_by) VALUES (?,?,'security','child_created','Child organization created',?,?)")
                        .bind(note_id).bind(root_id).bind(serde_json::json!({"child_org_id":org_id,"env_id":env_id}).to_string()).bind(actor_id).execute(&mut *tx).await?;
                    tx.commit().await?;
                    (org_id, env_id, true)
                }
            }
        };
        Ok(ChildProvisioned {
            org: Self::get_org_uncached(&db, &result_org).await?,
            env_id: result_env,
            created,
        })
    }

    /// Idempotent management operation for one concrete organization's environment.
    /// The caller must authorize environment creation; account limits are checked
    /// under the same root lock used by child provisioning.
    pub async fn provision_environment_authorized(
        db: &DatabasePool,
        org_id: &Uuid,
        actor_id: &Uuid,
        name: &str,
        _hosted: bool,
    ) -> Result<crate::db::Env, OrgError> {
        if name.is_empty()
            || name.len() > 63
            || name.starts_with('-')
            || name.ends_with('-')
            || !name
                .bytes()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
        {
            return Err(OrgError::Invalid("invalid environment name".into()));
        }
        let root_id = Self::billing_owner_id_for(db, org_id).await?;
        let db = db.with_scope(DatabaseScope::Maintenance {
            operation: MaintenanceOperation::OrganizationProvisioning,
            org_id: Some(root_id),
        });
        let env_id = Uuid::now_v7();
        let result_id = match &db {
            DatabasePool::Postgres(pool) => {
                let mut tx = pool.begin().await?;
                let root: (String, Option<Value>) = sqlx::query_as(
                    "SELECT lifecycle_state,features FROM org WHERE org_id=$1 FOR UPDATE",
                )
                .bind(root_id)
                .fetch_one(&mut *tx)
                .await?;
                let target: (String, Uuid) = sqlx::query_as("SELECT lifecycle_state,COALESCE(parent_org_id,org_id) FROM org WHERE org_id=$1")
                    .bind(org_id).fetch_one(&mut *tx).await?;
                if root.0 != "active" || target.0 != "active" || target.1 != root_id {
                    return Err(OrgError::Forbidden);
                }
                let existing: Option<(Uuid, bool)> =
                    sqlx::query_as("SELECT env_id,active FROM env WHERE org_id=$1 AND name=$2")
                        .bind(org_id)
                        .bind(name)
                        .fetch_optional(&mut *tx)
                        .await?;
                let id = if let Some((id, active)) = existing {
                    if !active {
                        return Err(OrgError::Invalid(
                            "environment exists but is inactive".into(),
                        ));
                    }
                    id
                } else {
                    let plan: Option<Value> = sqlx::query_scalar("SELECT p.features FROM org_plan op JOIN plan p ON p.plan_uuid=op.plan_uuid WHERE op.org_id=$1")
                        .bind(root_id).fetch_optional(&mut *tx).await?.flatten();
                    let limit = environment_limit(plan.as_ref(), root.1.as_ref());
                    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM env e JOIN org o ON o.org_id=e.org_id WHERE o.org_id=$1 OR o.parent_org_id=$1")
                        .bind(root_id).fetch_one(&mut *tx).await?;
                    if limit >= 0 && count >= limit {
                        return Err(OrgError::LimitReached);
                    }
                    sqlx::query("INSERT INTO env (env_id,org_id,name,created_by_user_id) VALUES ($1,$2,$3,$4)")
                        .bind(env_id).bind(org_id).bind(name).bind(actor_id).execute(&mut *tx).await?;
                    sqlx::query("INSERT INTO org_note (note_id,org_id,category,note_type,message,metadata,created_by) VALUES ($1,$2,'security','env_created','Environment created',$3,$4)")
                        .bind(Uuid::now_v7()).bind(root_id).bind(serde_json::json!({"org_id":org_id,"env_id":env_id,"name":name})).bind(actor_id).execute(&mut *tx).await?;
                    env_id
                };
                tx.commit().await?;
                id
            }
            DatabasePool::Sqlite(pool) => {
                let mut tx = pool.begin().await?;
                sqlx::query("UPDATE org SET updated_at=updated_at WHERE org_id=?")
                    .bind(root_id)
                    .execute(&mut *tx)
                    .await?;
                let root: (String, Option<String>) =
                    sqlx::query_as("SELECT lifecycle_state,features FROM org WHERE org_id=?")
                        .bind(root_id)
                        .fetch_one(&mut *tx)
                        .await?;
                let target: (String, Uuid) = sqlx::query_as(
                    "SELECT lifecycle_state,COALESCE(parent_org_id,org_id) FROM org WHERE org_id=?",
                )
                .bind(org_id)
                .fetch_one(&mut *tx)
                .await?;
                if root.0 != "active" || target.0 != "active" || target.1 != root_id {
                    return Err(OrgError::Forbidden);
                }
                let existing: Option<(Uuid, bool)> =
                    sqlx::query_as("SELECT env_id,active FROM env WHERE org_id=? AND name=?")
                        .bind(org_id)
                        .bind(name)
                        .fetch_optional(&mut *tx)
                        .await?;
                let id = if let Some((id, active)) = existing {
                    if !active {
                        return Err(OrgError::Invalid(
                            "environment exists but is inactive".into(),
                        ));
                    }
                    id
                } else {
                    let plan: Option<String> = sqlx::query_scalar("SELECT p.features FROM org_plan op JOIN plan p ON p.plan_uuid=op.plan_uuid WHERE op.org_id=?")
                        .bind(root_id).fetch_optional(&mut *tx).await?.flatten();
                    let plan = parse_features(plan)?;
                    let overrides = parse_features(root.1)?;
                    let limit = environment_limit(plan.as_ref(), overrides.as_ref());
                    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM env e JOIN org o ON o.org_id=e.org_id WHERE o.org_id=?1 OR o.parent_org_id=?1")
                        .bind(root_id).fetch_one(&mut *tx).await?;
                    if limit >= 0 && count >= limit {
                        return Err(OrgError::LimitReached);
                    }
                    sqlx::query(
                        "INSERT INTO env (env_id,org_id,name,created_by_user_id) VALUES (?,?,?,?)",
                    )
                    .bind(env_id)
                    .bind(org_id)
                    .bind(name)
                    .bind(actor_id)
                    .execute(&mut *tx)
                    .await?;
                    sqlx::query("INSERT INTO org_note (note_id,org_id,category,note_type,message,metadata,created_by) VALUES (?,?,'security','env_created','Environment created',?,?)")
                        .bind(Uuid::now_v7()).bind(root_id).bind(serde_json::json!({"org_id":org_id,"env_id":env_id,"name":name}).to_string()).bind(actor_id).execute(&mut *tx).await?;
                    env_id
                };
                tx.commit().await?;
                id
            }
        };
        crate::db::Env::get_env(&db, &result_id)
            .await
            .map_err(|error| OrgError::Invalid(error.to_string()))
    }

    pub async fn set_lifecycle(
        db: &DatabasePool,
        org_id: &Uuid,
        actor_id: &Uuid,
        state: OrgLifecycle,
    ) -> Result<(), OrgError> {
        require_root_admin(db, org_id, actor_id).await?;
        Self::set_lifecycle_authorized(db, org_id, actor_id, state).await
    }

    /// Caller must hold a validated management capability for this exact target.
    pub async fn set_lifecycle_authorized(
        db: &DatabasePool,
        org_id: &Uuid,
        actor_id: &Uuid,
        state: OrgLifecycle,
    ) -> Result<(), OrgError> {
        let root_id = Self::billing_owner_id_for(db, org_id).await?;
        let db = db.with_scope(DatabaseScope::Maintenance {
            operation: MaintenanceOperation::OrganizationProvisioning,
            org_id: Some(root_id),
        });
        let note_id = Uuid::now_v7();
        match &db {
            DatabasePool::Postgres(pool) => {
                let mut tx = pool.begin().await?;
                sqlx::query("SELECT org_id FROM org WHERE org_id=$1 FOR UPDATE")
                    .bind(root_id)
                    .execute(&mut *tx)
                    .await?;
                let before: String = sqlx::query_scalar(
                    "SELECT lifecycle_state FROM org WHERE org_id = $1 FOR UPDATE",
                )
                .bind(org_id)
                .fetch_one(&mut *tx)
                .await?;
                validate_transition(&before, state)?;
                sqlx::query("UPDATE org SET lifecycle_state=$2, active=$3, updated_at=now(), updated_by_user_id=$4, active_toggle_at=now(), active_toggle_by_user_id=$4 WHERE org_id=$1")
                    .bind(org_id).bind(state.as_str()).bind(state == OrgLifecycle::Active).bind(actor_id).execute(&mut *tx).await?;
                sqlx::query("INSERT INTO org_note (note_id,org_id,category,note_type,message,metadata,created_by) VALUES ($1,$2,'security','org_lifecycle','Organization lifecycle changed',$3,$4)")
                    .bind(note_id).bind(root_id).bind(serde_json::json!({"org_id":org_id,"from":before,"to":state.as_str()})).bind(actor_id).execute(&mut *tx).await?;
                tx.commit().await?;
            }
            DatabasePool::Sqlite(pool) => {
                let mut tx = pool.begin().await?;
                sqlx::query("UPDATE org SET updated_at=updated_at WHERE org_id=?")
                    .bind(root_id)
                    .execute(&mut *tx)
                    .await?;
                let before: String =
                    sqlx::query_scalar("SELECT lifecycle_state FROM org WHERE org_id=?")
                        .bind(org_id)
                        .fetch_one(&mut *tx)
                        .await?;
                validate_transition(&before, state)?;
                sqlx::query("UPDATE org SET lifecycle_state=?, active=?, updated_at=current_timestamp, updated_by_user_id=?, active_toggle_at=current_timestamp, active_toggle_by_user_id=? WHERE org_id=?")
                    .bind(state.as_str()).bind(state == OrgLifecycle::Active).bind(actor_id).bind(actor_id).bind(org_id).execute(&mut *tx).await?;
                sqlx::query("INSERT INTO org_note (note_id,org_id,category,note_type,message,metadata,created_by) VALUES (?,?,'security','org_lifecycle','Organization lifecycle changed',?,?)")
                    .bind(note_id).bind(root_id).bind(serde_json::json!({"org_id":org_id,"from":before,"to":state.as_str()}).to_string()).bind(actor_id).execute(&mut *tx).await?;
                tx.commit().await?;
            }
        }
        Self::invalidate_org_cache(org_id);
        Ok(())
    }

    pub async fn set_child_user_grant(
        db: &DatabasePool,
        root_id: &Uuid,
        child_id: &Uuid,
        actor_id: &Uuid,
        user_id: &Uuid,
        role: Option<i16>,
    ) -> Result<(), OrgError> {
        if Self::billing_owner_id_for(db, child_id).await? != *root_id || root_id == child_id {
            return Err(OrgError::Forbidden);
        }
        Self::set_child_grant(db, child_id, actor_id, user_id, role).await
    }

    /// Assign or revoke an existing root member's explicit child role. None
    /// deactivates the grant; only a current root admin can perform this operation.
    pub async fn set_child_grant(
        db: &DatabasePool,
        child_id: &Uuid,
        actor_id: &Uuid,
        user_id: &Uuid,
        role: Option<i16>,
    ) -> Result<(), OrgError> {
        if role.is_some_and(|role| role != 1 && role != 2) {
            return Err(OrgError::Invalid("invalid child role".into()));
        }
        let access = require_root_admin(db, child_id, actor_id).await?;
        if access.root_org_id == *child_id {
            return Err(OrgError::Invalid("target must be a child".into()));
        }
        let db = db.with_scope(DatabaseScope::Maintenance {
            operation: MaintenanceOperation::OrganizationProvisioning,
            org_id: Some(access.root_org_id),
        });
        if role.is_some()
            && active_role(&db, &access.root_org_id, user_id)
                .await?
                .is_none()
        {
            return Err(OrgError::Invalid(
                "user must be an active root member".into(),
            ));
        }
        let grant_id = Uuid::now_v7();
        match &db {
            DatabasePool::Postgres(pool) => {
                let mut tx = pool.begin().await?;
                let admin: Option<i16>=sqlx::query_scalar("SELECT org_user_role_id FROM org_user WHERE org_id=$1 AND user_id=$2 AND active=true FOR UPDATE")
                    .bind(access.root_org_id).bind(actor_id).fetch_optional(&mut *tx).await?;
                if admin != Some(2) {
                    return Err(OrgError::Forbidden);
                }
                sqlx::query("INSERT INTO org_user (org_user_id,org_id,user_id,org_user_role_id,active,created_by_user_id) VALUES ($1,$2,$3,$4,$5,$6)
                    ON CONFLICT (org_id,user_id) DO UPDATE SET org_user_role_id=EXCLUDED.org_user_role_id,active=EXCLUDED.active,updated_at=now(),updated_by_user_id=EXCLUDED.created_by_user_id")
                    .bind(grant_id).bind(child_id).bind(user_id).bind(role.unwrap_or(1)).bind(role.is_some()).bind(actor_id).execute(&mut *tx).await?;
                sqlx::query("INSERT INTO org_note (note_id,org_id,category,note_type,message,metadata,created_by) VALUES ($1,$2,'security','child_grant','Child membership grant changed',$3,$4)")
                    .bind(Uuid::now_v7()).bind(access.root_org_id).bind(serde_json::json!({"child_org_id":child_id,"user_id":user_id,"role":role})).bind(actor_id).execute(&mut *tx).await?;
                tx.commit().await?;
            }
            DatabasePool::Sqlite(pool) => {
                let mut tx = pool.begin().await?;
                sqlx::query("UPDATE org SET updated_at=updated_at WHERE org_id=?")
                    .bind(access.root_org_id)
                    .execute(&mut *tx)
                    .await?;
                let admin: Option<i16>=sqlx::query_scalar("SELECT org_user_role_id FROM org_user WHERE org_id=? AND user_id=? AND active=1")
                    .bind(access.root_org_id).bind(actor_id).fetch_optional(&mut *tx).await?;
                if admin != Some(2) {
                    return Err(OrgError::Forbidden);
                }
                sqlx::query("INSERT INTO org_user (org_user_id,org_id,user_id,org_user_role_id,active,created_by_user_id) VALUES (?,?,?,?,?,?)
                    ON CONFLICT (org_id,user_id) DO UPDATE SET org_user_role_id=excluded.org_user_role_id,active=excluded.active,updated_at=current_timestamp,updated_by_user_id=excluded.created_by_user_id")
                    .bind(grant_id).bind(child_id).bind(user_id).bind(role.unwrap_or(1)).bind(role.is_some()).bind(actor_id).execute(&mut *tx).await?;
                sqlx::query("INSERT INTO org_note (note_id,org_id,category,note_type,message,metadata,created_by) VALUES (?,?,'security','child_grant','Child membership grant changed',?,?)")
                    .bind(Uuid::now_v7()).bind(access.root_org_id).bind(serde_json::json!({"child_org_id":child_id,"user_id":user_id,"role":role}).to_string()).bind(actor_id).execute(&mut *tx).await?;
                tx.commit().await?;
            }
        }
        Ok(())
    }
}

async fn active_role(
    db: &DatabasePool,
    org_id: &Uuid,
    user_id: &Uuid,
) -> Result<Option<i16>, OrgError> {
    match db {
        DatabasePool::Postgres(pool) => Ok(sqlx::query_scalar(
            "SELECT org_user_role_id FROM org_user WHERE org_id=$1 AND user_id=$2 AND active=true",
        )
        .bind(org_id)
        .bind(user_id)
        .fetch_optional(pool)
        .await?),
        DatabasePool::Sqlite(pool) => Ok(sqlx::query_scalar(
            "SELECT org_user_role_id FROM org_user WHERE org_id=? AND user_id=? AND active=1",
        )
        .bind(org_id)
        .bind(user_id)
        .fetch_optional(pool)
        .await?),
    }
}
async fn require_root_admin(
    db: &DatabasePool,
    org_id: &Uuid,
    actor_id: &Uuid,
) -> Result<OrgAccess, OrgError> {
    Org::resolve_user_access(db, org_id, actor_id)
        .await?
        .filter(|access| access.can_manage_account)
        .ok_or(OrgError::Forbidden)
}
fn validate_parent(parent: &Option<Uuid>, kind: &str, state: &str) -> Result<(), OrgError> {
    if parent.is_some() || kind != "organization" || state != "active" {
        Err(OrgError::Invalid(
            "child requires an active organization root".into(),
        ))
    } else {
        Ok(())
    }
}
fn validate_transition(before: &str, state: OrgLifecycle) -> Result<(), OrgError> {
    if before == "archived" && state == OrgLifecycle::Active {
        Err(OrgError::Invalid(
            "restore archived organization to suspended before activation".into(),
        ))
    } else {
        Ok(())
    }
}
fn parse_features(value: Option<String>) -> Result<Option<Value>, OrgError> {
    value
        .map(|value| {
            serde_json::from_str(&value)
                .map_err(|_| OrgError::Invalid("malformed root feature configuration".into()))
        })
        .transpose()
}
fn environment_limit(plan: Option<&Value>, overrides: Option<&Value>) -> i64 {
    // Environment counts were historically unlimited. An explicitly configured
    // account ceiling is enforced without imposing a new default plan restriction.
    Features::resolve(plan, overrides).get_i64_or("environments", -1)
}
fn child_limit(plan: Option<&Value>, overrides: Option<&Value>, hosted: bool) -> i64 {
    // A hosted account without a plan stays on the conservative defaults.
    if hosted {
        return Features::resolve(plan, overrides).sub_orgs();
    }
    Features::for_account(plan, overrides).sub_orgs()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::org::OrgUser;

    async fn user(db: &DatabasePool, email: &str) -> Uuid {
        let id = Uuid::now_v7();
        let DatabasePool::Sqlite(pool) = db else {
            unreachable!()
        };
        sqlx::query("INSERT INTO user (user_id,email,created_by_user_id) VALUES (?,?,?)")
            .bind(id)
            .bind(email)
            .bind(id)
            .execute(pool)
            .await
            .unwrap();
        id
    }
    async fn fixture() -> (DatabasePool, Uuid, Uuid, Uuid) {
        let db = crate::db::test_db().await;
        let admin = user(&db, "root-admin@example.test").await;
        let member = user(&db, "member@example.test").await;
        let root = Uuid::now_v7();
        Org::insert_org(&db, &root, "Root", "root", "organization", &admin)
            .await
            .unwrap();
        for (user, role) in [(admin, 2), (member, 1)] {
            OrgUser::insert_org_user(&db, &Uuid::now_v7(), &root, &user, Some(role), &admin)
                .await
                .unwrap();
        }
        (db, root, admin, member)
    }
    fn request(slug: &str) -> CreateChild {
        CreateChild {
            name: slug.into(),
            slug: slug.into(),
            external_id: Some(slug.into()),
            idempotency_key: format!("create-{slug}"),
            hosted: false,
        }
    }
    async fn features(db: &DatabasePool, id: Uuid, value: Value) {
        let DatabasePool::Sqlite(pool) = db else {
            unreachable!()
        };
        sqlx::query("UPDATE org SET features=? WHERE org_id=?")
            .bind(value.to_string())
            .bind(id)
            .execute(pool)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn member_reactivation_respects_reserved_root_seats() {
        let (db, root, admin, member) = fixture().await;
        features(&db, root, serde_json::json!({"team_members":2})).await;
        OrgUser::update_org_user(&db, &root, &member, 1, false, &admin)
            .await
            .unwrap();
        crate::db::invite::Invite::insert_invite(
            &db,
            &Uuid::now_v7(),
            "reserved-seat",
            "pending@example.test",
            &root,
            1,
            &admin,
            chrono::Utc::now() + chrono::Duration::days(1),
        )
        .await
        .unwrap();
        assert!(matches!(
            OrgUser::update_org_user(&db, &root, &member, 1, true, &admin).await,
            Err(OrgError::LimitReached)
        ));
        let DatabasePool::Sqlite(pool) = &db else {
            unreachable!()
        };
        sqlx::query("UPDATE invite SET active=0 WHERE org_id=?")
            .bind(root)
            .execute(pool)
            .await
            .unwrap();
        OrgUser::update_org_user(&db, &root, &member, 1, true, &admin)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn environment_capacity_is_atomic_across_children_and_same_name_is_idempotent() {
        let (db, root, admin, _) = fixture().await;
        let a = Org::create_child(&db, &root, &admin, &request("env-a"))
            .await
            .unwrap();
        let b = Org::create_child(&db, &root, &admin, &request("env-b"))
            .await
            .unwrap();
        features(
            &db,
            root,
            serde_json::json!({"sub_orgs":3,"environments":3}),
        )
        .await;
        let (one, two) = tokio::join!(
            Org::provision_environment_authorized(&db, &a.org.org_id, &admin, "production", false),
            Org::provision_environment_authorized(&db, &b.org.org_id, &admin, "production", false)
        );
        assert_eq!(usize::from(one.is_ok()) + usize::from(two.is_ok()), 1);
        let env = one.ok().or_else(|| two.ok()).unwrap();
        let same =
            Org::provision_environment_authorized(&db, &env.org_id, &admin, "production", false)
                .await
                .unwrap();
        assert_eq!(same.env_id, env.env_id);
    }

    #[tokio::test]
    async fn inherited_admin_and_explicit_grants_preserve_sibling_isolation() {
        let (db, root, admin, member) = fixture().await;
        let a = Org::create_child(&db, &root, &admin, &request("child-a"))
            .await
            .unwrap();
        let b = Org::create_child(&db, &root, &admin, &request("child-b"))
            .await
            .unwrap();
        let inherited = Org::resolve_user_access(&db, &a.org.org_id, &admin)
            .await
            .unwrap()
            .unwrap();
        assert!(inherited.inherited && inherited.is_admin && inherited.runtime_allowed);
        assert!(
            OrgUser::get_org_user(&db, &a.org.org_id, &admin)
                .await
                .is_err()
        );
        assert!(
            Org::resolve_user_access(&db, &a.org.org_id, &member)
                .await
                .unwrap()
                .is_none()
        );
        Org::set_child_user_grant(&db, &root, &a.org.org_id, &admin, &member, Some(2))
            .await
            .unwrap();
        let direct = Org::resolve_user_access(&db, &a.org.org_id, &member)
            .await
            .unwrap()
            .unwrap();
        assert!(direct.is_admin && !direct.inherited && !direct.can_manage_account);
        assert!(
            Org::resolve_user_access(&db, &b.org.org_id, &member)
                .await
                .unwrap()
                .is_none()
        );
        assert!(matches!(
            Org::set_child_grant(&db, &b.org.org_id, &member, &member, Some(1)).await,
            Err(OrgError::Forbidden)
        ));
        let visible = Org::get_orgs_by_user(&db, &member).await.unwrap();
        assert_eq!(visible.len(), 2);
        assert!(visible.iter().all(|org| org.org_id != b.org.org_id));
    }

    #[tokio::test]
    async fn removal_and_reinvitation_never_resurrect_child_grants() {
        let (db, root, admin, member) = fixture().await;
        let child = Org::create_child(&db, &root, &admin, &request("child"))
            .await
            .unwrap();
        Org::set_child_grant(&db, &child.org.org_id, &admin, &member, Some(1))
            .await
            .unwrap();
        OrgUser::remove_org_user(&db, &root, &member).await.unwrap();
        assert!(
            !OrgUser::get_org_user(&db, &child.org.org_id, &member)
                .await
                .unwrap()
                .active
        );
        OrgUser::insert_org_user(&db, &Uuid::now_v7(), &root, &member, Some(1), &admin)
            .await
            .unwrap();
        assert!(
            Org::resolve_user_access(&db, &child.org.org_id, &member)
                .await
                .unwrap()
                .is_none()
        );
        Org::set_child_grant(&db, &child.org.org_id, &admin, &member, Some(1))
            .await
            .unwrap();
        OrgUser::update_org_user(&db, &root, &member, 1, false, &admin)
            .await
            .unwrap();
        OrgUser::update_org_user(&db, &root, &member, 1, true, &admin)
            .await
            .unwrap();
        assert!(
            Org::resolve_user_access(&db, &child.org.org_id, &member)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn provisioning_is_atomic_idempotent_and_rejects_key_reuse() {
        let (db, root, admin, _) = fixture().await;
        let req = request("child");
        let first = Org::create_child(&db, &root, &admin, &req).await.unwrap();
        let retry = Org::create_child(&db, &root, &admin, &req).await.unwrap();
        assert!(first.created && !retry.created);
        assert_eq!(first.org.org_id, retry.org.org_id);
        assert_eq!(first.env_id, retry.env_id);
        assert_eq!(Org::list_children(&db, &root).await.unwrap().len(), 1);
        let mut changed = req.clone();
        changed.name = "different".into();
        assert!(matches!(
            Org::create_child(&db, &root, &admin, &changed).await,
            Err(OrgError::Invalid(_))
        ));
        let mut duplicate = request("child");
        duplicate.idempotency_key = "another-key".into();
        assert!(
            Org::create_child(&db, &root, &admin, &duplicate)
                .await
                .is_err()
        );
        assert_eq!(Org::list_children(&db, &root).await.unwrap().len(), 1);
        let DatabasePool::Sqlite(pool) = &db else {
            unreachable!()
        };
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM env WHERE org_id=?")
                .bind(first.org.org_id)
                .fetch_one(pool)
                .await
                .unwrap(),
            1
        );
    }

    #[tokio::test]
    async fn sibling_provisioning_reserves_account_limit_atomically() {
        let (db, root, admin, _) = fixture().await;
        features(&db, root, serde_json::json!({"sub_orgs":1})).await;
        let a = request("child-a");
        let b = request("child-b");
        let (a, b) = tokio::join!(
            Org::create_child(&db, &root, &admin, &a),
            Org::create_child(&db, &root, &admin, &b)
        );
        assert_eq!(usize::from(a.is_ok()) + usize::from(b.is_ok()), 1);
        assert!(
            matches!(a, Err(OrgError::LimitReached)) || matches!(b, Err(OrgError::LimitReached))
        );
        let child = Org::list_children(&db, &root).await.unwrap().remove(0);
        Org::set_lifecycle(&db, &child.org_id, &admin, OrgLifecycle::Archived)
            .await
            .unwrap();
        assert!(matches!(
            Org::create_child(&db, &root, &admin, &request("next")).await,
            Err(OrgError::LimitReached)
        ));
    }

    #[tokio::test]
    async fn suspended_root_denies_children_but_allows_admin_recovery() {
        let (db, root, admin, member) = fixture().await;
        let child = Org::create_child(&db, &root, &admin, &request("child"))
            .await
            .unwrap();
        Org::set_child_grant(&db, &child.org.org_id, &admin, &member, Some(1))
            .await
            .unwrap();
        Org::set_lifecycle(&db, &root, &admin, OrgLifecycle::Archived)
            .await
            .unwrap();
        assert!(
            !Org::is_environment_admitted(&db, &child.env_id)
                .await
                .unwrap()
        );
        let recovery = Org::resolve_user_access(&db, &child.org.org_id, &admin)
            .await
            .unwrap()
            .unwrap();
        assert!(recovery.can_manage_account && !recovery.runtime_allowed);
        assert_eq!(Org::get_orgs_by_user(&db, &admin).await.unwrap().len(), 2);
        assert!(
            Org::get_orgs_by_user(&db, &member)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            Org::set_lifecycle(&db, &root, &admin, OrgLifecycle::Active)
                .await
                .is_err()
        );
        Org::set_lifecycle(&db, &root, &admin, OrgLifecycle::Suspended)
            .await
            .unwrap();
        Org::set_lifecycle(&db, &root, &admin, OrgLifecycle::Active)
            .await
            .unwrap();
        assert!(
            Org::is_environment_admitted(&db, &child.env_id)
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn database_guards_hierarchy_grants_and_legacy_lifecycle_writes() {
        let (db, root, admin, _) = fixture().await;
        let child = Org::create_child(&db, &root, &admin, &request("child"))
            .await
            .unwrap();
        let outsider = user(&db, "outsider@example.test").await;
        assert!(
            OrgUser::insert_org_user(
                &db,
                &Uuid::now_v7(),
                &child.org.org_id,
                &outsider,
                Some(1),
                &admin
            )
            .await
            .is_err()
        );
        let DatabasePool::Sqlite(pool) = &db else {
            unreachable!()
        };
        assert!(sqlx::query("INSERT INTO org (org_id,parent_org_id,name,slug,created_by_user_id) VALUES (?,?, 'Nested','nested',?)")
            .bind(Uuid::now_v7()).bind(child.org.org_id).bind(admin).execute(pool).await.is_err());
        assert!(
            sqlx::query("UPDATE org SET parent_org_id=? WHERE org_id=?")
                .bind(child.org.org_id)
                .bind(root)
                .execute(pool)
                .await
                .is_err()
        );
        sqlx::query("UPDATE org SET active=0 WHERE org_id=?")
            .bind(root)
            .execute(pool)
            .await
            .unwrap();
        assert_eq!(
            Org::get_org_uncached(&db, &root)
                .await
                .unwrap()
                .lifecycle_state,
            "suspended"
        );
        sqlx::query("UPDATE org SET active=1 WHERE org_id=?")
            .bind(root)
            .execute(pool)
            .await
            .unwrap();
        assert_eq!(
            Org::get_org_uncached(&db, &root)
                .await
                .unwrap()
                .lifecycle_state,
            "active"
        );
    }

    #[tokio::test]
    async fn child_entitlements_use_root_and_seats_are_not_duplicated() {
        let (db, root, admin, member) = fixture().await;
        features(
            &db,
            root,
            serde_json::json!({"sub_orgs":2,"runs_per_month":23}),
        )
        .await;
        let child = Org::create_child(&db, &root, &admin, &request("child"))
            .await
            .unwrap();
        features(
            &db,
            child.org.org_id,
            serde_json::json!({"runs_per_month":9999}),
        )
        .await;
        assert_eq!(
            Features::resolve_for_hosted_org(&db, &child.org.org_id)
                .await
                .runs_per_month(),
            23
        );
        Org::set_child_grant(&db, &child.org.org_id, &admin, &member, Some(1))
            .await
            .unwrap();
        let usage = crate::db::OrgUsageStats::calculate_account(&db, &root, chrono::Utc::now(), 0)
            .await
            .unwrap();
        assert_eq!(usage.team_members_count, 2);
        let invalid =
            Org::create_child(&db, &child.org.org_id, &admin, &request("grandchild")).await;
        assert!(matches!(invalid, Err(OrgError::Invalid(_))));
    }

    #[test]
    fn hosted_child_creation_is_disabled_without_entitlement() {
        assert_eq!(child_limit(None, None, true), 0);
        assert_eq!(child_limit(None, None, false), -1);
    }
}

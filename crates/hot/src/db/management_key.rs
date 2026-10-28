//! Root-owned control-plane credentials. They are never runtime principals.
//! Human issuance and immutable runtime-key lineage prevent delegation laundering.

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use uuid::Uuid;

use super::api_key::ApiKey;
use super::{BootstrapOperation, DatabasePool, DatabaseScope, Env, Org};
use crate::permission::Permissions;

pub const CAPABILITIES: &[&str] = &[
    "children:read",
    "children:create",
    "children:update",
    "environments:create",
    "runtime-keys:create",
    "usage:read",
];

#[derive(Debug, thiserror::Error)]
pub enum ManagementKeyError {
    #[error("Management credential is invalid or no longer authorized")]
    Unauthorized,
    #[error("Invalid management credential configuration: {0}")]
    Invalid(String),
    #[error(transparent)]
    Database(#[from] sqlx::Error),
}

#[derive(Debug, Clone, Serialize)]
pub struct ManagementKey {
    pub management_key_id: Uuid,
    pub root_org_id: Uuid,
    pub created_by_user_id: Uuid,
    pub name: String,
    #[serde(skip)]
    secret_hash: Vec<u8>,
    pub capabilities: Vec<String>,
    pub target_org_ids: Vec<Uuid>,
    pub all_children: bool,
    pub delegation_permissions: Option<serde_json::Value>,
    pub expires_at: DateTime<Utc>,
    pub revoked_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Deserialize)]
pub struct CreateManagementKey {
    pub name: String,
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub target_org_ids: Vec<Uuid>,
    #[serde(default)]
    pub all_children: bool,
    pub delegation_permissions: Option<serde_json::Value>,
    pub expires_in_seconds: u32,
}

#[derive(FromRow)]
struct KeyRow {
    management_key_id: Uuid,
    root_org_id: Uuid,
    created_by_user_id: Uuid,
    name: String,
    secret_hash: Vec<u8>,
    capabilities: String,
    target_org_ids: String,
    all_children: bool,
    delegation_permissions: Option<String>,
    expires_at: DateTime<Utc>,
    revoked_at: Option<DateTime<Utc>>,
    created_at: DateTime<Utc>,
}

impl TryFrom<KeyRow> for ManagementKey {
    type Error = ManagementKeyError;
    fn try_from(row: KeyRow) -> Result<Self, Self::Error> {
        Ok(Self {
            management_key_id: row.management_key_id,
            root_org_id: row.root_org_id,
            created_by_user_id: row.created_by_user_id,
            name: row.name,
            secret_hash: row.secret_hash,
            capabilities: serde_json::from_str(&row.capabilities)
                .map_err(|_| ManagementKeyError::Unauthorized)?,
            target_org_ids: serde_json::from_str(&row.target_org_ids)
                .map_err(|_| ManagementKeyError::Unauthorized)?,
            all_children: row.all_children,
            delegation_permissions: row
                .delegation_permissions
                .map(|value| serde_json::from_str(&value))
                .transpose()
                .map_err(|_| ManagementKeyError::Unauthorized)?,
            expires_at: row.expires_at,
            revoked_at: row.revoked_at,
            created_at: row.created_at,
        })
    }
}

const SELECT_KEY: &str = "SELECT management_key_id, root_org_id, created_by_user_id, name, secret_hash, CAST(capabilities AS TEXT) AS capabilities, CAST(target_org_ids AS TEXT) AS target_org_ids, all_children, CAST(delegation_permissions AS TEXT) AS delegation_permissions, expires_at, revoked_at, created_at FROM management_key";

/// A missing or RLS-hidden row denies the key; any other database failure is
/// reported as one, so callers can retry instead of treating it as a denial.
fn lookup_error(error: sqlx::Error) -> ManagementKeyError {
    match error {
        sqlx::Error::RowNotFound => ManagementKeyError::Unauthorized,
        error => ManagementKeyError::Database(error),
    }
}

fn org_lookup_error(error: crate::db::org::OrgError) -> ManagementKeyError {
    match error {
        crate::db::org::OrgError::Database(error) => lookup_error(error),
        _ => ManagementKeyError::Unauthorized,
    }
}

fn env_lookup_error(error: crate::db::env::EnvError) -> ManagementKeyError {
    match error {
        crate::db::env::EnvError::Database(error) => lookup_error(error),
        crate::db::env::EnvError::NotFound => ManagementKeyError::Unauthorized,
    }
}

impl ManagementKey {
    pub fn is_active(&self) -> bool {
        self.revoked_at.is_none() && self.expires_at > Utc::now()
    }

    pub fn has_capability(&self, capability: &str) -> bool {
        self.is_active() && self.capabilities.iter().any(|value| value == capability)
    }

    pub fn validate_request(request: &CreateManagementKey) -> Result<(), ManagementKeyError> {
        if request.name.trim().is_empty() || request.name.len() > 200 {
            return Err(ManagementKeyError::Invalid(
                "name must contain 1–200 characters".into(),
            ));
        }
        if request.expires_in_seconds == 0 || request.expires_in_seconds > 90 * 86400 {
            return Err(ManagementKeyError::Invalid(
                "expiry must be within 90 days".into(),
            ));
        }
        if request.capabilities.is_empty()
            || request
                .capabilities
                .iter()
                .any(|value| !CAPABILITIES.contains(&value.as_str()))
        {
            return Err(ManagementKeyError::Invalid(
                "unknown or empty capabilities".into(),
            ));
        }
        if request
            .capabilities
            .iter()
            .any(|value| value == "children:create")
            && !request.all_children
        {
            return Err(ManagementKeyError::Invalid(
                "child creation requires explicit all_children authority".into(),
            ));
        }
        if request
            .capabilities
            .iter()
            .any(|value| value == "runtime-keys:create")
        {
            let ceiling = request.delegation_permissions.as_ref().ok_or_else(|| {
                ManagementKeyError::Invalid(
                    "runtime key issuance requires a delegation ceiling".into(),
                )
            })?;
            Permissions::from_json_validated(ceiling)
                .map_err(|e| ManagementKeyError::Invalid(e.to_string()))?;
        } else if request.delegation_permissions.is_some() {
            return Err(ManagementKeyError::Invalid(
                "delegation ceiling requires runtime-keys:create".into(),
            ));
        }
        Ok(())
    }

    async fn require_root_admin(
        db: &DatabasePool,
        root_id: Uuid,
        user_id: Uuid,
    ) -> Result<(), ManagementKeyError> {
        let lookup = db.with_scope(DatabaseScope::Bootstrap(BootstrapOperation::Membership));
        let root = Org::get_org_uncached(&lookup, &root_id)
            .await
            .map_err(org_lookup_error)?;
        let access = Org::resolve_user_access(&lookup, &root_id, &user_id)
            .await
            .map_err(org_lookup_error)?;
        if root.parent_org_id.is_some()
            || !access.is_some_and(|grant| grant.is_admin && grant.can_manage_account)
        {
            return Err(ManagementKeyError::Unauthorized);
        }
        Ok(())
    }

    /// Only root-admin human callers may issue management credentials.
    pub async fn create_for_user(
        db: &DatabasePool,
        root_id: Uuid,
        user_id: Uuid,
        request: &CreateManagementKey,
    ) -> Result<(Self, String), ManagementKeyError> {
        Self::require_root_admin(db, root_id, user_id).await?;
        Self::validate_request(request)?;
        let lookup = db.with_scope(DatabaseScope::Bootstrap(BootstrapOperation::Membership));
        let root = Org::get_org_uncached(&lookup, &root_id)
            .await
            .map_err(org_lookup_error)?;
        if !root.is_admitting() {
            return Err(ManagementKeyError::Unauthorized);
        }
        for target in &request.target_org_ids {
            let org = Org::get_org_uncached(&lookup, target)
                .await
                .map_err(|_| ManagementKeyError::Unauthorized)?;
            if org.parent_org_id != Some(root_id) {
                return Err(ManagementKeyError::Unauthorized);
            }
        }
        let id = Uuid::now_v7();
        let (raw, secret_hash) = super::service_key::ServiceKey::generate_token(&id)
            .map_err(|_| ManagementKeyError::Unauthorized)?;
        let token = format!("hm_{raw}");
        let expires_at = Utc::now() + Duration::seconds(i64::from(request.expires_in_seconds));
        let capabilities = serde_json::to_value(&request.capabilities)
            .map_err(|_| ManagementKeyError::Unauthorized)?;
        let targets = serde_json::to_value(&request.target_org_ids)
            .map_err(|_| ManagementKeyError::Unauthorized)?;
        let scoped = db.with_scope(DatabaseScope::Organization { org_id: root_id });
        super::org_note::OrgNote::create(&scoped, &root_id, "security", "management-key.create", "Root administrator requested management credential creation", Some(&serde_json::json!({"management_key_id": id, "capabilities": request.capabilities, "target_org_ids": request.target_org_ids, "all_children": request.all_children})), Some(&user_id))
            .await.map_err(|e| ManagementKeyError::Invalid(e.to_string()))?;
        match &scoped {
            DatabasePool::Postgres(pool) => {
                sqlx::query("INSERT INTO management_key (management_key_id,root_org_id,created_by_user_id,name,secret_hash,capabilities,target_org_ids,all_children,delegation_permissions,expires_at) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10)")
                    .bind(id).bind(root_id).bind(user_id).bind(request.name.trim()).bind(&secret_hash)
                    .bind(&capabilities).bind(&targets).bind(request.all_children).bind(&request.delegation_permissions).bind(expires_at)
                    .execute(pool).await?;
            }
            DatabasePool::Sqlite(pool) => {
                sqlx::query("INSERT INTO management_key (management_key_id,root_org_id,created_by_user_id,name,secret_hash,capabilities,target_org_ids,all_children,delegation_permissions,expires_at) VALUES (?,?,?,?,?,?,?,?,?,?)")
                    .bind(id).bind(root_id).bind(user_id).bind(request.name.trim()).bind(&secret_hash)
                    .bind(capabilities.to_string()).bind(targets.to_string()).bind(request.all_children)
                    .bind(request.delegation_permissions.as_ref().map(serde_json::Value::to_string)).bind(expires_at)
                    .execute(pool).await?;
            }
        }
        Ok((Self::get(&scoped, id).await?, token))
    }

    pub async fn get(db: &DatabasePool, id: Uuid) -> Result<Self, ManagementKeyError> {
        let sql = format!("{SELECT_KEY} WHERE management_key_id = $1");
        let row = match db {
            DatabasePool::Postgres(pool) => {
                sqlx::query_as::<_, KeyRow>(sqlx::AssertSqlSafe(sql.as_str()))
                    .bind(id)
                    .fetch_one(pool)
                    .await?
            }
            DatabasePool::Sqlite(pool) => {
                sqlx::query_as::<_, KeyRow>(sqlx::AssertSqlSafe(sql.as_str()))
                    .bind(id)
                    .fetch_one(pool)
                    .await?
            }
        };
        row.try_into()
    }

    pub async fn list_for_user(
        db: &DatabasePool,
        root_id: Uuid,
        user_id: Uuid,
    ) -> Result<Vec<Self>, ManagementKeyError> {
        Self::require_root_admin(db, root_id, user_id).await?;
        let scoped = db.with_scope(DatabaseScope::Organization { org_id: root_id });
        let sql = format!("{SELECT_KEY} WHERE root_org_id = $1 ORDER BY created_at DESC");
        let rows = match &scoped {
            DatabasePool::Postgres(pool) => {
                sqlx::query_as::<_, KeyRow>(sqlx::AssertSqlSafe(sql.as_str()))
                    .bind(root_id)
                    .fetch_all(pool)
                    .await?
            }
            DatabasePool::Sqlite(pool) => {
                sqlx::query_as::<_, KeyRow>(sqlx::AssertSqlSafe(sql.as_str()))
                    .bind(root_id)
                    .fetch_all(pool)
                    .await?
            }
        };
        rows.into_iter().map(TryInto::try_into).collect()
    }

    pub async fn revoke_for_user(
        db: &DatabasePool,
        root_id: Uuid,
        user_id: Uuid,
        id: Uuid,
    ) -> Result<(), ManagementKeyError> {
        Self::require_root_admin(db, root_id, user_id).await?;
        let scoped = db.with_scope(DatabaseScope::Organization { org_id: root_id });
        let sql = "UPDATE management_key SET revoked_at = COALESCE(revoked_at,CURRENT_TIMESTAMP) WHERE management_key_id = $1 AND root_org_id = $2";
        super::org_note::OrgNote::create(
            &scoped,
            &root_id,
            "security",
            "management-key.revoke",
            "Root administrator requested management credential revocation",
            Some(&serde_json::json!({"management_key_id": id})),
            Some(&user_id),
        )
        .await
        .map_err(|e| ManagementKeyError::Invalid(e.to_string()))?;
        let changed = match &scoped {
            DatabasePool::Postgres(pool) => sqlx::query(sql)
                .bind(id)
                .bind(root_id)
                .execute(pool)
                .await?
                .rows_affected(),
            DatabasePool::Sqlite(pool) => sqlx::query(sql)
                .bind(id)
                .bind(root_id)
                .execute(pool)
                .await?
                .rows_affected(),
        };
        if changed == 0 {
            return Err(ManagementKeyError::Unauthorized);
        }
        Ok(())
    }

    pub async fn verify_token(db: &DatabasePool, token: &str) -> Result<Self, ManagementKeyError> {
        use sha2::{Digest, Sha256};
        let raw = token
            .strip_prefix("hm_")
            .ok_or(ManagementKeyError::Unauthorized)?;
        let (id, secret) = super::service_key::ServiceKey::parse_token(raw)
            .map_err(|_| ManagementKeyError::Unauthorized)?;
        let lookup = db.with_scope(DatabaseScope::Bootstrap(BootstrapOperation::Authentication));
        let key = Self::get(&lookup, id).await?;
        let computed = Sha256::digest(secret.as_bytes());
        if computed.len() != key.secret_hash.len()
            || computed
                .iter()
                .zip(&key.secret_hash)
                .fold(0u8, |diff, (a, b)| diff | (a ^ b))
                != 0
            || !key.is_active()
        {
            return Err(ManagementKeyError::Unauthorized);
        }
        key.validate_root(&lookup).await?;
        Ok(key)
    }

    async fn validate_root(&self, db: &DatabasePool) -> Result<(), ManagementKeyError> {
        let root = Org::get_org_uncached(db, &self.root_org_id)
            .await
            .map_err(org_lookup_error)?;
        if root.parent_org_id.is_some() || !root.is_admitting() {
            return Err(ManagementKeyError::Unauthorized);
        }
        // A key acts with its creator's authority, so it stops working while
        // the creator isn't a root administrator (removed or demoted).
        Self::require_root_admin(db, self.root_org_id, self.created_by_user_id).await
    }

    pub async fn authorize_child(
        &self,
        db: &DatabasePool,
        child_id: Uuid,
        capability: &str,
        require_active: bool,
    ) -> Result<Org, ManagementKeyError> {
        if !self.has_capability(capability) {
            return Err(ManagementKeyError::Unauthorized);
        }
        let lookup = db.with_scope(DatabaseScope::Bootstrap(BootstrapOperation::Authentication));
        self.validate_root(&lookup).await?;
        let child = Org::get_org_uncached(&lookup, &child_id)
            .await
            .map_err(org_lookup_error)?;
        if child.parent_org_id != Some(self.root_org_id)
            || (!self.all_children && !self.target_org_ids.contains(&child_id))
            || (require_active && !child.is_admitting())
        {
            return Err(ManagementKeyError::Unauthorized);
        }
        Ok(child)
    }

    /// Re-evaluate the complete issuance chain for an API key. User-issued keys
    /// have no lineage row and retain their independent organization lifecycle.
    pub async fn effective_runtime_permissions(
        db: &DatabasePool,
        api_key: &ApiKey,
    ) -> Result<Permissions, ManagementKeyError> {
        let lookup = db.with_scope(DatabaseScope::Bootstrap(BootstrapOperation::Authentication));
        let sql = "SELECT management_key_id FROM api_key_management_issuer WHERE api_key_id = $1";
        let issuer: Option<Uuid> = match &lookup {
            DatabasePool::Postgres(pool) => {
                sqlx::query_scalar(sql)
                    .bind(api_key.api_key_id)
                    .fetch_optional(pool)
                    .await?
            }
            DatabasePool::Sqlite(pool) => {
                sqlx::query_scalar(sql)
                    .bind(api_key.api_key_id)
                    .fetch_optional(pool)
                    .await?
            }
        };
        let own = api_key
            .get_permissions()
            .map_err(|_| ManagementKeyError::Unauthorized)?;
        let Some(id) = issuer else {
            return Ok(own);
        };
        let issuer = Self::get(&lookup, id).await?;
        let env = Env::get_env_uncached(&lookup, &api_key.env_id)
            .await
            .map_err(env_lookup_error)?;
        issuer
            .authorize_child(&lookup, env.org_id, "runtime-keys:create", true)
            .await?;
        let ceiling = Permissions::from_json(
            issuer
                .delegation_permissions
                .as_ref()
                .ok_or(ManagementKeyError::Unauthorized)?,
        )
        .map_err(|_| ManagementKeyError::Unauthorized)?;
        Ok(own.intersection(&ceiling))
    }

    /// Atomically issue a child environment API key and immutable issuer link.
    pub async fn issue_runtime_key(
        &self,
        db: &DatabasePool,
        child_id: Uuid,
        env_id: Uuid,
        description: &str,
        permissions: &serde_json::Value,
    ) -> Result<(Uuid, String), ManagementKeyError> {
        self.authorize_child(db, child_id, "runtime-keys:create", true)
            .await?;
        let lookup = db.with_scope(DatabaseScope::Bootstrap(BootstrapOperation::Authentication));
        let env = Env::get_env_uncached(&lookup, &env_id)
            .await
            .map_err(env_lookup_error)?;
        if env.org_id != child_id || !env.active || description.trim().is_empty() {
            return Err(ManagementKeyError::Unauthorized);
        }
        let grants = Permissions::from_json_validated(permissions)
            .map_err(|e| ManagementKeyError::Invalid(e.to_string()))?;
        let ceiling = Permissions::from_json(
            self.delegation_permissions
                .as_ref()
                .ok_or(ManagementKeyError::Unauthorized)?,
        )
        .map_err(|_| ManagementKeyError::Unauthorized)?;
        grants
            .validate_subset_of(&ceiling)
            .map_err(|e| ManagementKeyError::Invalid(e.to_string()))?;
        let id = Uuid::now_v7();
        let (token, hash) =
            ApiKey::generate_api_key(&id).map_err(|_| ManagementKeyError::Unauthorized)?;
        let key_data: serde_json::Value =
            serde_json::from_str(&hash).map_err(|_| ManagementKeyError::Unauthorized)?;
        let scoped = db.with_scope(DatabaseScope::Environment {
            org_id: child_id,
            env_id,
        });
        match &scoped {
            DatabasePool::Postgres(pool) => {
                let mut tx = pool.begin().await?;
                sqlx::query("INSERT INTO api_key (api_key_id,env_id,description,key_data,created_by_user_id,permissions) VALUES ($1,$2,$3,$4,$5,$6)")
                    .bind(id).bind(env_id).bind(description).bind(&key_data).bind(self.created_by_user_id).bind(permissions).execute(&mut *tx).await?;
                sqlx::query("INSERT INTO api_key_management_issuer (api_key_id,management_key_id) VALUES ($1,$2)")
                    .bind(id).bind(self.management_key_id).execute(&mut *tx).await?;
                tx.commit().await?;
            }
            DatabasePool::Sqlite(pool) => {
                let mut tx = pool.begin().await?;
                sqlx::query("INSERT INTO api_key (api_key_id,env_id,description,key_data,created_by_user_id,permissions) VALUES (?,?,?,?,?,?)")
                    .bind(id).bind(env_id).bind(description).bind(key_data.to_string()).bind(self.created_by_user_id).bind(permissions.to_string()).execute(&mut *tx).await?;
                sqlx::query("INSERT INTO api_key_management_issuer (api_key_id,management_key_id) VALUES (?,?)")
                    .bind(id).bind(self.management_key_id).execute(&mut *tx).await?;
                tx.commit().await?;
            }
        }
        Ok((id, token))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn delegated_lineage_cannot_be_removed_by_deleting_the_key() {
        let db = crate::db::test_db().await;
        let DatabasePool::Sqlite(pool) = &db else {
            unreachable!()
        };
        let key_id = Uuid::now_v7();
        let issuer_id = Uuid::now_v7();
        sqlx::query("INSERT INTO api_key(api_key_id,env_id,description,key_data,created_by_user_id) VALUES(?,?,'delegated','{}',?)")
            .bind(key_id).bind(Uuid::now_v7()).bind(Uuid::now_v7()).execute(pool).await.unwrap();
        sqlx::query(
            "INSERT INTO api_key_management_issuer(api_key_id,management_key_id) VALUES(?,?)",
        )
        .bind(key_id)
        .bind(issuer_id)
        .execute(pool)
        .await
        .unwrap();
        assert!(
            sqlx::query("DELETE FROM api_key WHERE api_key_id=?")
                .bind(key_id)
                .execute(pool)
                .await
                .is_err()
        );
        assert!(
            sqlx::query("DELETE FROM api_key_management_issuer WHERE api_key_id=?")
                .bind(key_id)
                .execute(pool)
                .await
                .is_err()
        );
        sqlx::query("UPDATE api_key SET active=0 WHERE api_key_id=?")
            .bind(key_id)
            .execute(pool)
            .await
            .unwrap();
        assert_eq!(
            sqlx::query_scalar::<_, Uuid>(
                "SELECT management_key_id FROM api_key_management_issuer WHERE api_key_id=?"
            )
            .bind(key_id)
            .fetch_one(pool)
            .await
            .unwrap(),
            issuer_id
        );
    }

    #[test]
    fn management_delegation_requires_an_explicit_ceiling() {
        let mut request = CreateManagementKey {
            name: "provisioner".into(),
            capabilities: vec!["runtime-keys:create".into()],
            target_org_ids: vec![Uuid::now_v7()],
            all_children: false,
            delegation_permissions: None,
            expires_in_seconds: 3600,
        };
        assert!(ManagementKey::validate_request(&request).is_err());
        request.delegation_permissions = Some(serde_json::json!({"event:*": ["create"]}));
        assert!(ManagementKey::validate_request(&request).is_ok());
        request.capabilities.push("human-grants:write".into());
        assert!(ManagementKey::validate_request(&request).is_err());
    }

    #[tokio::test]
    async fn revoking_keeps_the_first_time_and_rejects_unknown_keys() {
        let db = crate::db::test_db().await;
        let data = crate::db::insert_test_data(&db).await.unwrap();
        let DatabasePool::Sqlite(pool) = &db else {
            unreachable!()
        };
        sqlx::query("UPDATE org SET org_type='organization' WHERE org_id=?")
            .bind(data.org_id)
            .execute(pool)
            .await
            .unwrap();
        let (key, _token) = ManagementKey::create_for_user(
            &db,
            data.org_id,
            data.user_id,
            &CreateManagementKey {
                name: "reader".into(),
                capabilities: vec!["children:read".into()],
                target_org_ids: Vec::new(),
                all_children: true,
                delegation_permissions: None,
                expires_in_seconds: 3600,
            },
        )
        .await
        .unwrap();
        let revoked_at = || async {
            sqlx::query_scalar::<_, String>(
                "SELECT CAST(revoked_at AS TEXT) FROM management_key WHERE management_key_id=?",
            )
            .bind(key.management_key_id)
            .fetch_one(pool)
            .await
            .unwrap()
        };
        ManagementKey::revoke_for_user(&db, data.org_id, data.user_id, key.management_key_id)
            .await
            .unwrap();
        let first = revoked_at().await;
        // CURRENT_TIMESTAMP has one-second resolution.
        tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
        ManagementKey::revoke_for_user(&db, data.org_id, data.user_id, key.management_key_id)
            .await
            .unwrap();
        assert_eq!(revoked_at().await, first);
        assert!(matches!(
            ManagementKey::revoke_for_user(&db, data.org_id, data.user_id, Uuid::now_v7()).await,
            Err(ManagementKeyError::Unauthorized)
        ));
    }

    #[test]
    fn lookup_failures_are_database_errors_but_missing_rows_deny() {
        assert!(matches!(
            lookup_error(sqlx::Error::RowNotFound),
            ManagementKeyError::Unauthorized
        ));
        assert!(matches!(
            lookup_error(sqlx::Error::PoolTimedOut),
            ManagementKeyError::Database(_)
        ));
        assert!(matches!(
            env_lookup_error(crate::db::env::EnvError::NotFound),
            ManagementKeyError::Unauthorized
        ));
    }
    #[test]
    fn provisioning_cannot_implicitly_expand_target_scope() {
        let request = CreateManagementKey {
            name: "provisioner".into(),
            capabilities: vec!["children:create".into()],
            target_org_ids: Vec::new(),
            all_children: false,
            delegation_permissions: None,
            expires_in_seconds: 3600,
        };
        assert!(ManagementKey::validate_request(&request).is_err());
    }
}

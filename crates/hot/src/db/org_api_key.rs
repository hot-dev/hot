//! Org-scoped API keys. An org key belongs to one root organization and calls
//! only control-plane routes; it is never a runtime principal. Keys it mints
//! (org keys in its org, environment keys in its org or a child) keep an
//! immutable issuer link, and every request intersects the whole chain, so
//! revoking, narrowing or expiring a key restricts everything it issued.
//! Rotation keeps the key ID, so it cascades nothing.

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use uuid::Uuid;

use super::api_key::ApiKey;
use super::env::EnvError;
use super::org::OrgError;
use super::{BootstrapOperation, DatabasePool, DatabaseScope, Env, Org};
use crate::permission::{PermissionError, Permissions, actions, org_urn};

/// Org keys in a chain: a person issues the first, and it may mint one more
/// level of org keys. Environment keys sit below either.
pub const MAX_ORG_KEY_DEPTH: usize = 2;
pub const DEFAULT_ROTATION_OVERLAP_SECONDS: i64 = 86_400;
pub const MAX_ROTATION_OVERLAP_SECONDS: i64 = 7 * 86_400;

#[derive(Debug, thiserror::Error)]
pub enum OrgApiKeyError {
    #[error("Org API key is invalid or not authorized for this action")]
    Unauthorized,
    #[error("Not found")]
    NotFound,
    #[error("Invalid request: {0}")]
    Invalid(String),
    #[error(transparent)]
    Database(#[from] sqlx::Error),
}

impl From<PermissionError> for OrgApiKeyError {
    fn from(error: PermissionError) -> Self {
        Self::Invalid(error.to_string())
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct OrgApiKey {
    pub org_api_key_id: Uuid,
    pub org_id: Uuid,
    pub description: String,
    #[serde(skip)]
    key_data: serde_json::Value,
    #[serde(skip)]
    previous_key_data: Option<serde_json::Value>,
    pub previous_key_expires_at: Option<DateTime<Utc>>,
    pub permissions: serde_json::Value,
    pub active: bool,
    pub expires_at: Option<DateTime<Utc>>,
    pub revoked_at: Option<DateTime<Utc>>,
    pub issuer_org_api_key_id: Option<Uuid>,
    pub created_by_user_id: Uuid,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(FromRow)]
struct KeyRow {
    org_api_key_id: Uuid,
    org_id: Uuid,
    description: String,
    key_data: String,
    previous_key_data: Option<String>,
    previous_key_expires_at: Option<DateTime<Utc>>,
    permissions: String,
    active: bool,
    expires_at: Option<DateTime<Utc>>,
    revoked_at: Option<DateTime<Utc>>,
    issuer_org_api_key_id: Option<Uuid>,
    created_by_user_id: Uuid,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl TryFrom<KeyRow> for OrgApiKey {
    type Error = OrgApiKeyError;
    fn try_from(row: KeyRow) -> Result<Self, Self::Error> {
        let json = |value: &str| {
            serde_json::from_str(value).map_err(|_| OrgApiKeyError::Invalid("stored key".into()))
        };
        Ok(Self {
            org_api_key_id: row.org_api_key_id,
            org_id: row.org_id,
            description: row.description,
            key_data: json(&row.key_data)?,
            previous_key_data: row.previous_key_data.as_deref().map(json).transpose()?,
            previous_key_expires_at: row.previous_key_expires_at,
            permissions: json(&row.permissions)?,
            active: row.active,
            expires_at: row.expires_at,
            revoked_at: row.revoked_at,
            issuer_org_api_key_id: row.issuer_org_api_key_id,
            created_by_user_id: row.created_by_user_id,
            created_at: row.created_at,
            updated_at: row.updated_at,
        })
    }
}

const SELECT_KEY: &str = "SELECT org_api_key_id, org_id, description, CAST(key_data AS TEXT) AS key_data, CAST(previous_key_data AS TEXT) AS previous_key_data, previous_key_expires_at, CAST(permissions AS TEXT) AS permissions, active, expires_at, revoked_at, issuer_org_api_key_id, created_by_user_id, created_at, updated_at FROM org_api_key";

/// A new org key: by a root admin in the App, or minted by another org key.
#[derive(Debug, Clone, Deserialize)]
pub struct CreateOrgApiKey {
    pub description: String,
    pub permissions: serde_json::Value,
    #[serde(default)]
    pub expires_at: Option<DateTime<Utc>>,
}

/// A new environment key minted by an org key.
#[derive(Debug, Clone, Deserialize)]
pub struct MintEnvApiKey {
    pub description: String,
    pub permissions: serde_json::Value,
    #[serde(default)]
    pub expires_at: Option<DateTime<Utc>>,
}

/// An org key's current authority: its own grants intersected with every
/// ancestor's, and the earliest expiry in the chain.
#[derive(Debug, Clone)]
pub struct EffectiveOrgGrants {
    pub permissions: Permissions,
    pub expires_at: Option<DateTime<Utc>>,
    /// Org keys in the chain, including this one.
    pub depth: usize,
}

fn auth_db(db: &DatabasePool) -> DatabasePool {
    db.with_scope(DatabaseScope::Bootstrap(BootstrapOperation::Authentication))
}

fn validate_description(description: &str) -> Result<(), OrgApiKeyError> {
    if description.trim().is_empty() || description.len() > 200 {
        return Err(OrgApiKeyError::Invalid(
            "description must contain 1–200 characters".into(),
        ));
    }
    Ok(())
}

/// A requested expiry must be in the future and no later than the issuer's.
fn validate_expiry(
    requested: Option<DateTime<Utc>>,
    ceiling: Option<DateTime<Utc>>,
) -> Result<Option<DateTime<Utc>>, OrgApiKeyError> {
    if requested.is_some_and(|at| at <= Utc::now()) {
        return Err(OrgApiKeyError::Invalid(
            "expiry must be in the future".into(),
        ));
    }
    match (requested, ceiling) {
        (Some(at), Some(limit)) if at > limit => Err(OrgApiKeyError::Invalid(
            "expiry cannot be later than the issuing key's".into(),
        )),
        (None, Some(limit)) => Ok(Some(limit)),
        (requested, _) => Ok(requested),
    }
}

/// A missing or RLS-hidden row is `missing`; any other database failure is
/// reported as one, so callers can retry instead of treating it as a denial.
fn lookup_error(error: sqlx::Error, missing: OrgApiKeyError) -> OrgApiKeyError {
    match error {
        sqlx::Error::RowNotFound => missing,
        error => OrgApiKeyError::Database(error),
    }
}

fn org_lookup_error(missing: OrgApiKeyError) -> impl FnOnce(OrgError) -> OrgApiKeyError {
    move |error| match error {
        OrgError::Database(error) => lookup_error(error, missing),
        _ => missing,
    }
}

fn env_lookup_error(missing: OrgApiKeyError) -> impl FnOnce(EnvError) -> OrgApiKeyError {
    move |error| match error {
        EnvError::Database(error) => lookup_error(error, missing),
        EnvError::NotFound => missing,
    }
}

/// Any failure in an issuer chain denies, except a database failure.
fn chain_error(error: OrgApiKeyError) -> OrgApiKeyError {
    match error {
        OrgApiKeyError::Database(error) => lookup_error(error, OrgApiKeyError::Unauthorized),
        _ => OrgApiKeyError::Unauthorized,
    }
}

async fn require_root_admin(
    db: &DatabasePool,
    root_id: Uuid,
    user_id: Uuid,
) -> Result<Org, OrgApiKeyError> {
    let lookup = db.with_scope(DatabaseScope::Bootstrap(BootstrapOperation::Membership));
    let root = Org::get_org_uncached(&lookup, &root_id)
        .await
        .map_err(org_lookup_error(OrgApiKeyError::Unauthorized))?;
    let access = Org::resolve_user_access(&lookup, &root_id, &user_id)
        .await
        .map_err(org_lookup_error(OrgApiKeyError::Unauthorized))?;
    if root.parent_org_id.is_some()
        || !access.is_some_and(|grant| grant.is_admin && grant.can_manage_account)
    {
        return Err(OrgApiKeyError::Unauthorized);
    }
    Ok(root)
}

impl OrgApiKey {
    pub fn is_usable(&self) -> bool {
        self.active && self.revoked_at.is_none() && self.expires_at.is_none_or(|at| at > Utc::now())
    }

    pub fn get_permissions(&self) -> Result<Permissions, OrgApiKeyError> {
        Permissions::from_json(&self.permissions).map_err(|_| OrgApiKeyError::Unauthorized)
    }

    pub async fn get(db: &DatabasePool, id: Uuid) -> Result<Self, OrgApiKeyError> {
        let sql = format!("{SELECT_KEY} WHERE org_api_key_id = $1");
        let row = match db {
            DatabasePool::Postgres(pool) => {
                sqlx::query_as::<_, KeyRow>(sqlx::AssertSqlSafe(sql.as_str()))
                    .bind(id)
                    .fetch_optional(pool)
                    .await?
            }
            DatabasePool::Sqlite(pool) => {
                sqlx::query_as::<_, KeyRow>(sqlx::AssertSqlSafe(sql.as_str()))
                    .bind(id)
                    .fetch_optional(pool)
                    .await?
            }
        };
        row.ok_or(OrgApiKeyError::NotFound)?.try_into()
    }

    /// Org keys of one organization, newest first.
    pub async fn list_for_org(
        db: &DatabasePool,
        org_id: Uuid,
    ) -> Result<Vec<Self>, OrgApiKeyError> {
        let scoped = db.with_scope(DatabaseScope::Organization { org_id });
        let sql =
            format!("{SELECT_KEY} WHERE org_id = $1 ORDER BY created_at DESC, org_api_key_id DESC");
        let rows = match &scoped {
            DatabasePool::Postgres(pool) => {
                sqlx::query_as::<_, KeyRow>(sqlx::AssertSqlSafe(sql.as_str()))
                    .bind(org_id)
                    .fetch_all(pool)
                    .await?
            }
            DatabasePool::Sqlite(pool) => {
                sqlx::query_as::<_, KeyRow>(sqlx::AssertSqlSafe(sql.as_str()))
                    .bind(org_id)
                    .fetch_all(pool)
                    .await?
            }
        };
        rows.into_iter().map(TryInto::try_into).collect()
    }

    async fn insert(
        db: &DatabasePool,
        org_id: Uuid,
        issuer: Option<Uuid>,
        created_by_user_id: Uuid,
        request: &CreateOrgApiKey,
        expires_at: Option<DateTime<Utc>>,
    ) -> Result<(Self, String), OrgApiKeyError> {
        let id = Uuid::now_v7();
        let (token, hash) =
            ApiKey::generate_api_key(&id).map_err(|_| OrgApiKeyError::Unauthorized)?;
        let key_data: serde_json::Value =
            serde_json::from_str(&hash).map_err(|_| OrgApiKeyError::Unauthorized)?;
        let scoped = db.with_scope(DatabaseScope::Organization { org_id });
        let description = request.description.trim();
        match &scoped {
            DatabasePool::Postgres(pool) => {
                sqlx::query("INSERT INTO org_api_key (org_api_key_id,org_id,description,key_data,permissions,expires_at,issuer_org_api_key_id,created_by_user_id) VALUES ($1,$2,$3,$4,$5,$6,$7,$8)")
                    .bind(id).bind(org_id).bind(description).bind(&key_data).bind(&request.permissions)
                    .bind(expires_at).bind(issuer).bind(created_by_user_id)
                    .execute(pool).await?;
            }
            DatabasePool::Sqlite(pool) => {
                sqlx::query("INSERT INTO org_api_key (org_api_key_id,org_id,description,key_data,permissions,expires_at,issuer_org_api_key_id,created_by_user_id) VALUES (?,?,?,?,?,?,?,?)")
                    .bind(id).bind(org_id).bind(description).bind(key_data.to_string()).bind(request.permissions.to_string())
                    .bind(expires_at).bind(issuer).bind(created_by_user_id)
                    .execute(pool).await?;
            }
        }
        super::org_note::OrgNote::create(
            &scoped,
            &org_id,
            "security",
            "org-api-key.create",
            "Org API key created",
            Some(&serde_json::json!({"org_api_key_id": id, "issuer_org_api_key_id": issuer, "permissions": request.permissions, "expires_at": expires_at})),
            Some(&created_by_user_id),
        )
        .await
        .map_err(|e| OrgApiKeyError::Invalid(e.to_string()))?;
        Ok((Self::get(&scoped, id).await?, token))
    }

    /// Only a root administrator creates an org key without an issuer.
    pub async fn create_for_user(
        db: &DatabasePool,
        root_id: Uuid,
        user_id: Uuid,
        request: &CreateOrgApiKey,
    ) -> Result<(Self, String), OrgApiKeyError> {
        // A suspended account can still list and revoke keys, but not issue them.
        if !require_root_admin(db, root_id, user_id)
            .await?
            .is_admitting()
        {
            return Err(OrgApiKeyError::Unauthorized);
        }
        validate_description(&request.description)?;
        Permissions::from_json(&request.permissions)?.validate_org_key()?;
        let expires_at = validate_expiry(request.expires_at, None)?;
        Self::insert(db, root_id, None, user_id, request, expires_at).await
    }

    /// Mint an org key in this key's org. Its grants must fit inside this
    /// key's current grants, and the chain stays within MAX_ORG_KEY_DEPTH.
    pub async fn mint_org_key(
        &self,
        db: &DatabasePool,
        effective: &EffectiveOrgGrants,
        request: &CreateOrgApiKey,
    ) -> Result<(Self, String), OrgApiKeyError> {
        if !effective.permissions.org_key_allows(
            &self.org_id,
            &org_urn::org_api_keys(&self.org_id),
            actions::CREATE,
        ) {
            return Err(OrgApiKeyError::Unauthorized);
        }
        if effective.depth >= MAX_ORG_KEY_DEPTH {
            return Err(OrgApiKeyError::Invalid(format!(
                "org keys can be at most {MAX_ORG_KEY_DEPTH} deep"
            )));
        }
        validate_description(&request.description)?;
        let grants = Permissions::from_json(&request.permissions)?;
        grants.validate_org_key()?;
        grants.validate_org_key_subset_of(&effective.permissions, &self.org_id)?;
        let expires_at = validate_expiry(request.expires_at, effective.expires_at)?;
        Self::insert(
            db,
            self.org_id,
            Some(self.org_api_key_id),
            self.created_by_user_id,
            request,
            expires_at,
        )
        .await
    }

    /// Resolve an environment this key may administer: in its own org or a
    /// direct child, owned by `org_id`, and admitting work.
    pub async fn resolve_env(
        &self,
        db: &DatabasePool,
        org_id: Uuid,
        env_id: Uuid,
    ) -> Result<(Org, Env), OrgApiKeyError> {
        let lookup = auth_db(db);
        let org = self.resolve_org(&lookup, org_id, true).await?;
        let env = Env::get_env_uncached(&lookup, &env_id)
            .await
            .map_err(env_lookup_error(OrgApiKeyError::NotFound))?;
        if env.org_id != org.org_id {
            return Err(OrgApiKeyError::NotFound);
        }
        if !env.active {
            return Err(OrgApiKeyError::Unauthorized);
        }
        Ok((org, env))
    }

    /// The key's own org or one of its direct children. Never another root,
    /// a grandchild, or a fallback.
    pub async fn resolve_org(
        &self,
        db: &DatabasePool,
        org_id: Uuid,
        require_admitting: bool,
    ) -> Result<Org, OrgApiKeyError> {
        let org = Org::get_org_uncached(&auth_db(db), &org_id)
            .await
            .map_err(org_lookup_error(OrgApiKeyError::NotFound))?;
        if org.org_id != self.org_id && org.parent_org_id != Some(self.org_id) {
            return Err(OrgApiKeyError::NotFound);
        }
        if require_admitting && !org.is_admitting() {
            return Err(OrgApiKeyError::Unauthorized);
        }
        Ok(org)
    }

    /// Mint an environment key. Its grants must fit inside this key's current
    /// grants projected onto that environment.
    pub async fn mint_env_key(
        &self,
        db: &DatabasePool,
        effective: &EffectiveOrgGrants,
        org_id: Uuid,
        env_id: Uuid,
        request: &MintEnvApiKey,
    ) -> Result<(Uuid, String, Option<DateTime<Utc>>), OrgApiKeyError> {
        if !effective.permissions.org_key_allows(
            &self.org_id,
            &org_urn::env_api_keys(&org_id, &env_id),
            actions::CREATE,
        ) {
            return Err(OrgApiKeyError::Unauthorized);
        }
        let (org, env) = self.resolve_env(db, org_id, env_id).await?;
        validate_description(&request.description)?;
        let grants = Permissions::from_json_validated(&request.permissions)?;
        let ceiling = effective
            .permissions
            .project_to_env(&self.org_id, &org.org_id, &env.env_id);
        grants.validate_subset_of(&ceiling)?;
        let expires_at = validate_expiry(request.expires_at, effective.expires_at)?;
        let id = Uuid::now_v7();
        let (token, hash) =
            ApiKey::generate_api_key(&id).map_err(|_| OrgApiKeyError::Unauthorized)?;
        let key_data: serde_json::Value =
            serde_json::from_str(&hash).map_err(|_| OrgApiKeyError::Unauthorized)?;
        let description = request.description.trim();
        let scoped = db.with_scope(DatabaseScope::Environment {
            org_id: org.org_id,
            env_id: env.env_id,
        });
        match &scoped {
            DatabasePool::Postgres(pool) => {
                let mut tx = pool.begin().await?;
                sqlx::query("INSERT INTO api_key (api_key_id,env_id,description,key_data,created_by_user_id,permissions,expires_at) VALUES ($1,$2,$3,$4,$5,$6,$7)")
                    .bind(id).bind(env.env_id).bind(description).bind(&key_data).bind(self.created_by_user_id).bind(&request.permissions).bind(expires_at)
                    .execute(&mut *tx).await?;
                sqlx::query(
                    "INSERT INTO api_key_issuer (api_key_id,issuer_org_api_key_id) VALUES ($1,$2)",
                )
                .bind(id)
                .bind(self.org_api_key_id)
                .execute(&mut *tx)
                .await?;
                tx.commit().await?;
            }
            DatabasePool::Sqlite(pool) => {
                let mut tx = pool.begin().await?;
                sqlx::query("INSERT INTO api_key (api_key_id,env_id,description,key_data,created_by_user_id,permissions,expires_at) VALUES (?,?,?,?,?,?,?)")
                    .bind(id).bind(env.env_id).bind(description).bind(key_data.to_string()).bind(self.created_by_user_id).bind(request.permissions.to_string()).bind(expires_at)
                    .execute(&mut *tx).await?;
                sqlx::query(
                    "INSERT INTO api_key_issuer (api_key_id,issuer_org_api_key_id) VALUES (?,?)",
                )
                .bind(id)
                .bind(self.org_api_key_id)
                .execute(&mut *tx)
                .await?;
                tx.commit().await?;
            }
        }
        Ok((id, token, expires_at))
    }

    /// Authenticate an org key token (`hot_<id>_<secret>`, the same format as
    /// environment keys). The previous secret works until a rotation's overlap
    /// ends. Returns NotFound when the ID is not an org key.
    pub async fn verify_token(db: &DatabasePool, token: &str) -> Result<Self, OrgApiKeyError> {
        let (id, secret) =
            ApiKey::parse_api_key(token).map_err(|_| OrgApiKeyError::Unauthorized)?;
        let lookup = auth_db(db);
        let key = Self::get(&lookup, id).await?;
        let current = ApiKey::secret_matches(&secret, &key.key_data).unwrap_or(false);
        let previous = !current
            && key
                .previous_key_expires_at
                .is_some_and(|until| until > Utc::now())
            && key
                .previous_key_data
                .as_ref()
                .is_some_and(|data| ApiKey::secret_matches(&secret, data).unwrap_or(false));
        if !(current || previous) || !key.is_usable() {
            return Err(OrgApiKeyError::Unauthorized);
        }
        Ok(key)
    }

    /// Current authority: own grants intersected with every ancestor's. Fails
    /// when any key in the chain is unusable or the org is not an admitting root.
    pub async fn effective(&self, db: &DatabasePool) -> Result<EffectiveOrgGrants, OrgApiKeyError> {
        let lookup = auth_db(db);
        let org = Org::get_org_uncached(&lookup, &self.org_id)
            .await
            .map_err(org_lookup_error(OrgApiKeyError::Unauthorized))?;
        if org.parent_org_id.is_some() || !org.is_admitting() || !self.is_usable() {
            return Err(OrgApiKeyError::Unauthorized);
        }
        let mut permissions = self.get_permissions()?;
        let mut expires_at = self.expires_at;
        let mut depth = 1;
        let mut issuer = self.issuer_org_api_key_id;
        while let Some(id) = issuer {
            depth += 1;
            if depth > MAX_ORG_KEY_DEPTH {
                return Err(OrgApiKeyError::Unauthorized);
            }
            let ancestor = Self::get(&lookup, id).await.map_err(chain_error)?;
            if ancestor.org_id != self.org_id || !ancestor.is_usable() {
                return Err(OrgApiKeyError::Unauthorized);
            }
            permissions =
                permissions.org_key_intersection(&ancestor.get_permissions()?, &self.org_id);
            expires_at = match (expires_at, ancestor.expires_at) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            };
            issuer = ancestor.issuer_org_api_key_id;
        }
        Ok(EffectiveOrgGrants {
            permissions,
            expires_at,
            depth,
        })
    }

    /// The org key that minted an environment key, if one did.
    pub async fn issuer_of_env_key(
        db: &DatabasePool,
        api_key_id: Uuid,
    ) -> Result<Option<Uuid>, OrgApiKeyError> {
        let sql = "SELECT issuer_org_api_key_id FROM api_key_issuer WHERE api_key_id = $1";
        let lookup = auth_db(db);
        Ok(match &lookup {
            DatabasePool::Postgres(pool) => {
                sqlx::query_scalar(sql)
                    .bind(api_key_id)
                    .fetch_optional(pool)
                    .await?
            }
            DatabasePool::Sqlite(pool) => {
                sqlx::query_scalar(sql)
                    .bind(api_key_id)
                    .fetch_optional(pool)
                    .await?
            }
        })
    }

    /// Effective grants of an environment API key. A user-issued key keeps its
    /// own grants. A minted key is limited by its issuer chain, projected onto
    /// the key's environment; any unusable link revokes it.
    pub async fn effective_env_permissions(
        db: &DatabasePool,
        api_key: &ApiKey,
    ) -> Result<Permissions, OrgApiKeyError> {
        let lookup = auth_db(db);
        let state = ApiKey::get_credential_state(&lookup, &api_key.api_key_id)
            .await
            .map_err(|error| match error {
                super::api_key::ApiKeyError::Database(error) => {
                    lookup_error(error, OrgApiKeyError::Unauthorized)
                }
                _ => OrgApiKeyError::Unauthorized,
            })?;
        if state.is_expired() {
            return Err(OrgApiKeyError::Unauthorized);
        }
        let own = api_key
            .get_permissions()
            .map_err(|_| OrgApiKeyError::Unauthorized)?;
        let Some(issuer_id) = Self::issuer_of_env_key(&lookup, api_key.api_key_id).await? else {
            return Ok(own);
        };
        let issuer = Self::get(&lookup, issuer_id).await.map_err(chain_error)?;
        let effective = issuer.effective(&lookup).await?;
        let env = Env::get_env_uncached(&lookup, &api_key.env_id)
            .await
            .map_err(env_lookup_error(OrgApiKeyError::Unauthorized))?;
        issuer
            .resolve_org(&lookup, env.org_id, false)
            .await
            .map_err(chain_error)?;
        Ok(own.intersection(&effective.permissions.project_to_env(
            &issuer.org_id,
            &env.org_id,
            &env.env_id,
        )))
    }

    /// Whether `ancestor` issued this org key, directly or through others.
    pub async fn has_ancestor(
        &self,
        db: &DatabasePool,
        ancestor: Uuid,
    ) -> Result<bool, OrgApiKeyError> {
        let lookup = auth_db(db);
        let mut issuer = self.issuer_org_api_key_id;
        let mut hops = 0;
        while let Some(id) = issuer {
            if id == ancestor {
                return Ok(true);
            }
            hops += 1;
            if hops > MAX_ORG_KEY_DEPTH {
                return Ok(false);
            }
            issuer = Self::get(&lookup, id).await?.issuer_org_api_key_id;
        }
        Ok(false)
    }

    /// Issue a new secret for the same key ID; the previous one works until
    /// `overlap` ends. Lineage, grants and expiry are unchanged.
    pub async fn rotate(
        &self,
        db: &DatabasePool,
        overlap: Duration,
    ) -> Result<String, OrgApiKeyError> {
        let (token, hash) = ApiKey::generate_api_key(&self.org_api_key_id)
            .map_err(|_| OrgApiKeyError::Unauthorized)?;
        let key_data: serde_json::Value =
            serde_json::from_str(&hash).map_err(|_| OrgApiKeyError::Unauthorized)?;
        let until = Utc::now() + overlap;
        let scoped = db.with_scope(DatabaseScope::Organization {
            org_id: self.org_id,
        });
        let updated = match &scoped {
            DatabasePool::Postgres(pool) => sqlx::query("UPDATE org_api_key SET previous_key_data = key_data, previous_key_expires_at = $1, key_data = $2, updated_at = now() WHERE org_api_key_id = $3 AND revoked_at IS NULL")
                .bind(until).bind(&key_data).bind(self.org_api_key_id)
                .execute(pool).await?.rows_affected(),
            DatabasePool::Sqlite(pool) => sqlx::query("UPDATE org_api_key SET previous_key_data = key_data, previous_key_expires_at = ?, key_data = ?, updated_at = CURRENT_TIMESTAMP WHERE org_api_key_id = ? AND revoked_at IS NULL")
                .bind(until).bind(key_data.to_string()).bind(self.org_api_key_id)
                .execute(pool).await?.rows_affected(),
        };
        if updated == 0 {
            return Err(OrgApiKeyError::NotFound);
        }
        Ok(token)
    }

    /// Irreversible. Every key this one issued stops on its next request.
    pub async fn revoke(&self, db: &DatabasePool) -> Result<(), OrgApiKeyError> {
        let scoped = db.with_scope(DatabaseScope::Organization {
            org_id: self.org_id,
        });
        match &scoped {
            DatabasePool::Postgres(pool) => {
                sqlx::query("UPDATE org_api_key SET revoked_at = COALESCE(revoked_at, now()), active = false, updated_at = now() WHERE org_api_key_id = $1")
                    .bind(self.org_api_key_id)
                    .execute(pool)
                    .await?;
            }
            DatabasePool::Sqlite(pool) => {
                sqlx::query("UPDATE org_api_key SET revoked_at = COALESCE(revoked_at, CURRENT_TIMESTAMP), active = 0, updated_at = CURRENT_TIMESTAMP WHERE org_api_key_id = ?")
                    .bind(self.org_api_key_id)
                    .execute(pool)
                    .await?;
            }
        }
        Ok(())
    }

    /// Replace stored grants. Callers check the new grants against their own.
    pub async fn set_permissions(
        &self,
        db: &DatabasePool,
        permissions: &Permissions,
    ) -> Result<(), OrgApiKeyError> {
        permissions.validate_org_key()?;
        let value = permissions.to_json();
        let scoped = db.with_scope(DatabaseScope::Organization {
            org_id: self.org_id,
        });
        match &scoped {
            DatabasePool::Postgres(pool) => {
                sqlx::query("UPDATE org_api_key SET permissions = $1, updated_at = now() WHERE org_api_key_id = $2")
                    .bind(&value)
                    .bind(self.org_api_key_id)
                    .execute(pool)
                    .await?;
            }
            DatabasePool::Sqlite(pool) => {
                sqlx::query("UPDATE org_api_key SET permissions = ?, updated_at = CURRENT_TIMESTAMP WHERE org_api_key_id = ?")
                    .bind(value.to_string())
                    .bind(self.org_api_key_id)
                    .execute(pool)
                    .await?;
            }
        }
        Ok(())
    }

    /// Root administrators revoke any org key of their root in the App.
    pub async fn revoke_for_user(
        db: &DatabasePool,
        root_id: Uuid,
        user_id: Uuid,
        id: Uuid,
    ) -> Result<(), OrgApiKeyError> {
        require_root_admin(db, root_id, user_id).await?;
        let scoped = db.with_scope(DatabaseScope::Organization { org_id: root_id });
        let key = Self::get(&scoped, id).await?;
        if key.org_id != root_id {
            return Err(OrgApiKeyError::NotFound);
        }
        super::org_note::OrgNote::create(
            &scoped,
            &root_id,
            "security",
            "org-api-key.revoke",
            "Root administrator revoked an org API key",
            Some(&serde_json::json!({"org_api_key_id": id})),
            Some(&user_id),
        )
        .await
        .map_err(|e| OrgApiKeyError::Invalid(e.to_string()))?;
        key.revoke(db).await
    }

    /// Root administrators list their root's org keys in the App.
    pub async fn list_for_user(
        db: &DatabasePool,
        root_id: Uuid,
        user_id: Uuid,
    ) -> Result<Vec<Self>, OrgApiKeyError> {
        require_root_admin(db, root_id, user_id).await?;
        Self::list_for_org(db, root_id).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::test_db;
    use serde_json::json;

    async fn fixture() -> (DatabasePool, crate::db::TestData) {
        let db = test_db().await;
        let data = crate::db::insert_test_data(&db).await.unwrap();
        let DatabasePool::Sqlite(pool) = &db else {
            unreachable!()
        };
        sqlx::query("UPDATE org SET org_type = 'organization' WHERE org_id = ?")
            .bind(data.org_id)
            .execute(pool)
            .await
            .unwrap();
        (db, data)
    }

    fn request(permissions: serde_json::Value) -> CreateOrgApiKey {
        CreateOrgApiKey {
            description: "provisioner".into(),
            permissions,
            expires_at: None,
        }
    }

    #[tokio::test]
    async fn root_admin_creates_org_keys_and_tokens_verify() {
        let (db, data) = fixture().await;
        let (key, token) = OrgApiKey::create_for_user(
            &db,
            data.org_id,
            data.user_id,
            &request(json!({"org:*": ["create", "read"]})),
        )
        .await
        .unwrap();
        assert_eq!(
            OrgApiKey::verify_token(&db, &token)
                .await
                .unwrap()
                .org_api_key_id,
            key.org_api_key_id
        );
        assert!(
            OrgApiKey::verify_token(&db, &token.replace('a', "b"))
                .await
                .is_err()
        );
        // Environment-key authentication never accepts an org key.
        assert!(ApiKey::verify_api_key(&db, &token).await.unwrap().is_none());
        // Unqualified grants are rejected for org keys.
        assert!(
            OrgApiKey::create_for_user(
                &db,
                data.org_id,
                data.user_id,
                &request(json!({"project:*": ["read"]}))
            )
            .await
            .is_err()
        );
        // Only a root admin may create one.
        assert!(
            OrgApiKey::create_for_user(
                &db,
                data.org_id,
                Uuid::now_v7(),
                &request(json!({"org:*": ["read"]}))
            )
            .await
            .is_err()
        );
    }

    #[tokio::test]
    async fn rotation_keeps_the_id_and_overlaps_the_previous_secret() {
        let (db, data) = fixture().await;
        let (key, old) = OrgApiKey::create_for_user(
            &db,
            data.org_id,
            data.user_id,
            &request(json!({"org:*": ["read"]})),
        )
        .await
        .unwrap();
        let new = key.rotate(&db, Duration::seconds(60)).await.unwrap();
        assert_ne!(old, new);
        assert_eq!(
            OrgApiKey::verify_token(&db, &new)
                .await
                .unwrap()
                .org_api_key_id,
            key.org_api_key_id
        );
        assert!(OrgApiKey::verify_token(&db, &old).await.is_ok());
        let key = OrgApiKey::get(&db, key.org_api_key_id).await.unwrap();
        let newer = key.rotate(&db, Duration::seconds(0)).await.unwrap();
        assert!(
            OrgApiKey::verify_token(&db, &new).await.is_err(),
            "overlap ended"
        );
        assert!(OrgApiKey::verify_token(&db, &old).await.is_err());
        assert!(OrgApiKey::verify_token(&db, &newer).await.is_ok());
    }

    #[tokio::test]
    async fn minted_keys_fit_inside_and_follow_their_issuer() {
        let (db, data) = fixture().await;
        let root = data.org_id;
        let (top, _) = OrgApiKey::create_for_user(
            &db,
            root,
            data.user_id,
            &request(json!({
                format!("org:{root}/api-key:*"): ["create"],
                format!("org:{root}/env:*/api-key:*"): ["create"],
                format!("org:{root}/env:*/event:*"): ["create"],
                "org:*": ["read"],
            })),
        )
        .await
        .unwrap();
        let effective = top.effective(&db).await.unwrap();
        assert_eq!(effective.depth, 1);
        // A minted org key cannot exceed its issuer.
        assert!(
            top.mint_org_key(&db, &effective, &request(json!({"org:*": ["create"]})))
                .await
                .is_err()
        );
        let (minted, _) = top
            .mint_org_key(
                &db,
                &effective,
                &request(json!({
                    format!("org:{root}/env:*/api-key:*"): ["create"],
                    format!("org:{root}/env:*/event:*"): ["create"],
                    format!("org:{root}/api-key:*"): ["create"],
                })),
            )
            .await
            .unwrap();
        let minted_effective = minted.effective(&db).await.unwrap();
        assert_eq!(minted_effective.depth, 2);
        assert!(minted.has_ancestor(&db, top.org_api_key_id).await.unwrap());
        assert!(!top.has_ancestor(&db, minted.org_api_key_id).await.unwrap());
        // The depth limit stops a third org key.
        assert!(
            minted
                .mint_org_key(
                    &db,
                    &minted_effective,
                    &request(json!({format!("org:{root}/api-key:*"): ["create"]}))
                )
                .await
                .is_err()
        );
        // Environment keys fit inside the projection onto their environment.
        let mint = |permissions| MintEnvApiKey {
            description: "child runtime".into(),
            permissions,
            expires_at: None,
        };
        assert!(
            minted
                .mint_env_key(
                    &db,
                    &minted_effective,
                    root,
                    data.env_id,
                    &mint(json!({"run:*": ["read"]}))
                )
                .await
                .is_err()
        );
        let (env_key_id, env_token, _) = minted
            .mint_env_key(
                &db,
                &minted_effective,
                root,
                data.env_id,
                &mint(json!({"event:*": ["create"]})),
            )
            .await
            .unwrap();
        let env_key = ApiKey::verify_api_key(&db, &env_token)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(env_key.api_key_id, env_key_id);
        assert_eq!(
            OrgApiKey::issuer_of_env_key(&db, env_key_id).await.unwrap(),
            Some(minted.org_api_key_id)
        );
        assert!(
            OrgApiKey::effective_env_permissions(&db, &env_key)
                .await
                .unwrap()
                .has_permission("event:orders", "create")
        );
        // Narrowing the top key narrows the whole chain.
        top.set_permissions(
            &db,
            &Permissions::from_json(&json!({format!("org:{root}/env:*/api-key:*"): ["create"]}))
                .unwrap(),
        )
        .await
        .unwrap();
        assert!(
            OrgApiKey::effective_env_permissions(&db, &env_key)
                .await
                .unwrap()
                .is_empty()
        );
        // Revoking it revokes the chain.
        top.revoke(&db).await.unwrap();
        assert!(minted.effective(&db).await.is_err());
        assert!(
            OrgApiKey::effective_env_permissions(&db, &env_key)
                .await
                .is_err()
        );
        // Lineage survives: neither the env key nor its issuer link can be deleted.
        let DatabasePool::Sqlite(pool) = &db else {
            unreachable!()
        };
        assert!(
            sqlx::query("DELETE FROM api_key_issuer WHERE api_key_id=?")
                .bind(env_key_id)
                .execute(pool)
                .await
                .is_err()
        );
        assert!(
            sqlx::query("DELETE FROM api_key WHERE api_key_id=?")
                .bind(env_key_id)
                .execute(pool)
                .await
                .is_err()
        );
        assert!(
            sqlx::query("UPDATE org_api_key SET issuer_org_api_key_id=NULL WHERE org_api_key_id=?")
                .bind(minted.org_api_key_id)
                .execute(pool)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn minted_expiry_cannot_outlive_the_issuer() {
        let (db, data) = fixture().await;
        let root = data.org_id;
        let soon = Utc::now() + Duration::hours(1);
        let (top, _) = OrgApiKey::create_for_user(
            &db,
            root,
            data.user_id,
            &CreateOrgApiKey {
                description: "expiring".into(),
                permissions: json!({format!("org:{root}/env:*/api-key:*"): ["create"], format!("org:{root}/env:*/event:*"): ["create"]}),
                expires_at: Some(soon),
            },
        )
        .await
        .unwrap();
        let effective = top.effective(&db).await.unwrap();
        let mint = |expires_at| MintEnvApiKey {
            description: "runtime".into(),
            permissions: json!({"event:*": ["create"]}),
            expires_at,
        };
        assert!(
            top.mint_env_key(
                &db,
                &effective,
                root,
                data.env_id,
                &mint(Some(soon + Duration::hours(1)))
            )
            .await
            .is_err()
        );
        let (_, _, inherited) = top
            .mint_env_key(&db, &effective, root, data.env_id, &mint(None))
            .await
            .unwrap();
        assert_eq!(inherited, Some(soon));
    }
}

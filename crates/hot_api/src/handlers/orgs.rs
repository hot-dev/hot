//! Control-plane API for org-scoped API keys. Organizations, environments and
//! API keys are ordinary resources, authorized by qualified URNs such as
//! `org:*` or `org:{org}/env:{env}/api-key:*`. Only org keys authenticate
//! here; environment keys, sessions and service keys get `key_scope_mismatch`,
//! and org keys get the same error from runtime routes.
//!
//! Project, context and build routes inside an environment reuse the
//! environment handlers: the org key's grants are projected onto that
//! environment and the request is re-dispatched to those handlers with that
//! principal, so they authorize exactly as they do for an environment key.
use super::ListQueryParams;
use crate::auth::{AuthContext, authenticate_token, authorize_route, key_scope_mismatch};
use crate::{
    ApiStateData,
    domain_resolver::ResolvedDomain,
    models::{ApiErrorResponse, ApiListResponse, ApiResponse},
};
use axum::{
    Extension, Json, Router,
    body::Body,
    extract::{
        DefaultBodyLimit, FromRequestParts, MatchedPath, Path, Query, RawPathParams, Request, State,
    },
    http::StatusCode,
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{any, get, post},
};
use chrono::{DateTime, Duration, Utc};
use hot::db::api_key::ApiKey;
use hot::db::org::{CreateChild, OrgLifecycle};
use hot::db::org_api_key::{
    CreateOrgApiKey, DEFAULT_ROTATION_OVERLAP_SECONDS, EffectiveOrgGrants,
    MAX_ROTATION_OVERLAP_SECONDS, MintEnvApiKey, OrgApiKey, OrgApiKeyError,
};
use hot::db::{BootstrapOperation, DatabasePool, DatabaseScope, Env, Org, with_database_scope};
use hot::permission::{Permissions, actions, org_urn};
use serde::{Deserialize, Serialize};
use tower::ServiceExt;
use uuid::Uuid;

/// The authenticated org key and its current chain-limited grants.
#[derive(Debug, Clone)]
pub struct OrgPrincipal {
    pub key: OrgApiKey,
    pub effective: EffectiveOrgGrants,
}

impl OrgPrincipal {
    fn allows(&self, resource: &str, action: &str) -> bool {
        self.effective
            .permissions
            .org_key_allows(&self.key.org_id, resource, action)
    }

    fn require(&self, resource: &str, action: &str) -> Result<(), ApiError> {
        if self.allows(resource, action) {
            Ok(())
        } else {
            Err(denied())
        }
    }
}

/// `forward` serves project, context and build routes for an environment
/// key; see the module comment.
pub(crate) fn router(
    state: ApiStateData,
    forward: Router<ApiStateData>,
    max_body_size: usize,
) -> Router<ApiStateData> {
    let forward: Router = forward
        .layer(DefaultBodyLimit::max(max_body_size))
        .route_layer(middleware::from_fn(authorize_forwarded))
        .with_state(state.clone());
    let control = Router::new()
        .route("/v1/orgs", get(list_orgs).post(create_org))
        .route("/v1/orgs/{org_id}", get(get_org).patch(update_org))
        .route("/v1/orgs/{org_id}/usage", get(get_usage))
        .route("/v1/orgs/{org_id}/envs", get(list_envs).post(create_env))
        .route("/v1/orgs/{org_id}/envs/{env_id}", get(get_env))
        .route(
            "/v1/orgs/{org_id}/envs/{env_id}/api-keys",
            get(list_env_keys).post(mint_env_key),
        )
        .route(
            "/v1/orgs/{org_id}/api-keys",
            get(list_org_keys).post(mint_org_key),
        )
        .route(
            "/v1/api-keys/{key_id}",
            get(get_api_key)
                .patch(update_api_key)
                .delete(revoke_api_key),
        )
        .route("/v1/api-keys/{key_id}/rotate", post(rotate_api_key))
        .layer(DefaultBodyLimit::max(64 * 1024));
    let env_routes = Router::new()
        .route(
            "/v1/orgs/{org_id}/envs/{env_id}/projects",
            any(forward_to_env),
        )
        .route(
            "/v1/orgs/{org_id}/envs/{env_id}/projects/{*rest}",
            any(forward_to_env),
        )
        .route(
            "/v1/orgs/{org_id}/envs/{env_id}/builds",
            any(forward_to_env),
        )
        .layer(Extension(forward.into_service::<Body>()))
        .layer(DefaultBodyLimit::max(max_body_size));
    control
        .merge(env_routes)
        .route_layer(middleware::from_fn_with_state(
            state,
            org_key_auth_middleware,
        ))
}

type ApiError = (StatusCode, Json<ApiErrorResponse>);
type ForwardService = axum::routing::RouterIntoService<Body>;

fn denied() -> ApiError {
    (
        StatusCode::FORBIDDEN,
        Json(ApiErrorResponse::new(
            "forbidden",
            "Org API key does not authorize this action",
        )),
    )
}

fn not_found(what: &str) -> ApiError {
    (
        StatusCode::NOT_FOUND,
        Json(ApiErrorResponse::not_found(what)),
    )
}

fn bad_request(message: &str) -> ApiError {
    (
        StatusCode::BAD_REQUEST,
        Json(ApiErrorResponse::bad_request(message)),
    )
}

fn internal(error: impl std::fmt::Display) -> ApiError {
    tracing::error!(%error, "org API operation failed");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(ApiErrorResponse::internal_error("Org API operation failed")),
    )
}

fn key_error(error: OrgApiKeyError, what: &str) -> ApiError {
    match error {
        OrgApiKeyError::Unauthorized => denied(),
        OrgApiKeyError::NotFound => not_found(what),
        OrgApiKeyError::Invalid(message) => bad_request(&message),
        OrgApiKeyError::Database(error) => internal(error),
    }
}

fn org_error(error: hot::db::org::OrgError) -> ApiError {
    use hot::db::org::OrgError;
    match error {
        OrgError::NotFound | OrgError::Database(sqlx::Error::RowNotFound) => {
            not_found("Organization")
        }
        OrgError::Database(error) => internal(error),
        OrgError::Forbidden => denied(),
        other => bad_request(&other.to_string()),
    }
}

/// Authenticate an org key, apply the account rate limit, and audit the
/// request and its outcome on the key's organization.
pub async fn org_key_auth_middleware(
    State((db, _, _, _)): State<ApiStateData>,
    mut request: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    if request.extensions().get::<ResolvedDomain>().is_some() {
        return Err(StatusCode::FORBIDDEN);
    }
    let token = request
        .headers()
        .get("authorization")
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "))
        .ok_or(StatusCode::UNAUTHORIZED)?
        .to_owned();
    // A credential the database could not check is unavailable, not invalid:
    // clients retry a 503 but discard a credential that got a 401.
    let key = match OrgApiKey::verify_token(&db, &token).await {
        Ok(key) => key,
        Err(OrgApiKeyError::Database(error)) => {
            tracing::error!(%error, "Org API key lookup failed");
            return Err(StatusCode::SERVICE_UNAVAILABLE);
        }
        Err(_) if authenticate_token(&db, &token).await.is_ok() => {
            return Ok(key_scope_mismatch(
                "This route needs an org API key; environment keys, sessions and service keys call runtime routes",
            ));
        }
        Err(_) => return Err(StatusCode::UNAUTHORIZED),
    };
    let effective = key.effective(&db).await.map_err(|error| match error {
        OrgApiKeyError::Database(_) => StatusCode::SERVICE_UNAVAILABLE,
        _ => StatusCode::UNAUTHORIZED,
    })?;
    if let Err(exceeded) = crate::rate_limit::check_org_rate_limit(
        &db,
        &key.org_id,
        crate::rate_limit::PublicRateLimitMode::Enforce,
        "org-api",
    )
    .await
    {
        return Ok(crate::rate_limit::rate_limit_response(exceeded));
    }
    let org_id = key.org_id;
    let audit_db = db.with_scope(DatabaseScope::Organization { org_id });
    let metadata = serde_json::json!({
        "org_api_key_id": key.org_api_key_id,
        "issuer_org_api_key_id": key.issuer_org_api_key_id,
        "method": request.method().as_str(),
        "path": request.uri().path(),
    });
    hot::db::org_note::OrgNote::create_system(
        &audit_db,
        &org_id,
        "security",
        "org-api.request",
        "Org API key operation requested",
        Some(&metadata),
    )
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    request
        .extensions_mut()
        .insert(OrgPrincipal { key, effective });
    let response = next.run(request).await;
    let mut result = metadata;
    result["status"] = serde_json::json!(response.status().as_u16());
    if let Err(error) = hot::db::org_note::OrgNote::create_system(
        &audit_db,
        &org_id,
        "security",
        "org-api.response",
        "Org API key operation completed",
        Some(&result),
    )
    .await
    {
        tracing::error!(%error, %org_id, "Failed to persist org API response audit; request audit retained");
    }
    Ok(response)
}

// ============================================================================
// Organizations
// ============================================================================

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct OrgResponse {
    pub org_id: Uuid,
    pub parent_org_id: Option<Uuid>,
    pub name: String,
    pub slug: String,
    pub external_id: Option<String>,
    pub lifecycle_state: String,
    pub active: bool,
    pub created_at: DateTime<Utc>,
}

impl From<Org> for OrgResponse {
    fn from(org: Org) -> Self {
        Self {
            org_id: org.org_id,
            parent_org_id: org.parent_org_id,
            name: org.name,
            slug: org.slug,
            external_id: org.external_id,
            lifecycle_state: org.lifecycle_state,
            active: org.active,
            created_at: org.created_at,
        }
    }
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct CreateOrgResponse {
    pub org: OrgResponse,
    /// The child's default environment.
    pub env_id: Uuid,
    /// False when the idempotency key matched an earlier request.
    pub created: bool,
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct CreateOrgRequest {
    pub name: String,
    pub slug: String,
    pub external_id: Option<String>,
    pub idempotency_key: String,
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct UpdateOrgRequest {
    /// `active`, `suspended`, or `archived`.
    pub state: String,
}

/// Children of the key's organization that the key may read.
pub async fn list_orgs(
    State((db, _, _, _)): State<ApiStateData>,
    Extension(principal): Extension<OrgPrincipal>,
    Query(params): Query<ListQueryParams>,
) -> Result<Json<ApiListResponse<OrgResponse>>, ApiError> {
    let children = Org::list_children(&db, &principal.key.org_id)
        .await
        .map_err(org_error)?;
    let readable: Vec<Org> = children
        .into_iter()
        .filter(|child| principal.allows(&org_urn::org(&child.org_id), actions::READ))
        .collect();
    let total = readable.len() as i64;
    let page = readable
        .into_iter()
        .skip(params.offset.max(0) as usize)
        .take(params.limit as usize)
        .map(OrgResponse::from)
        .collect();
    Ok(Json(ApiListResponse::new(
        page,
        total,
        params.limit,
        params.offset,
    )))
}

pub async fn create_org(
    State((db, _, conf, _)): State<ApiStateData>,
    Extension(principal): Extension<OrgPrincipal>,
    Json(body): Json<CreateOrgRequest>,
) -> Result<(StatusCode, Json<ApiResponse<CreateOrgResponse>>), ApiError> {
    principal.require(org_urn::CHILDREN, actions::CREATE)?;
    hot::slug::validate_format_with_extra(&body.slug, &hot::slug::extra_reserved_from_conf(&conf))
        .map_err(|error| bad_request(error.message()))?;
    let request = CreateChild {
        name: body.name,
        slug: body.slug,
        external_id: body.external_id,
        idempotency_key: body.idempotency_key,
        hosted: hot::product::billing_enabled(&conf),
    };
    let child = Org::create_child_authorized(
        &db,
        &principal.key.org_id,
        &principal.key.created_by_user_id,
        &request,
    )
    .await
    .map_err(org_error)?;
    tracing::info!(org_api_key_id = %principal.key.org_api_key_id, org_id = %principal.key.org_id, child_org_id = %child.org.org_id, action = "org.create", "org API mutation");
    Ok((
        StatusCode::CREATED,
        Json(ApiResponse::new(CreateOrgResponse {
            org: child.org.into(),
            env_id: child.env_id,
            created: child.created,
        })),
    ))
}

pub async fn get_org(
    State((db, _, _, _)): State<ApiStateData>,
    Extension(principal): Extension<OrgPrincipal>,
    Path(org_id): Path<Uuid>,
) -> Result<Json<ApiResponse<OrgResponse>>, ApiError> {
    principal.require(&org_urn::org(&org_id), actions::READ)?;
    let org = principal
        .key
        .resolve_org(&db, org_id, false)
        .await
        .map_err(|e| key_error(e, "Organization"))?;
    Ok(Json(ApiResponse::new(org.into())))
}

/// Change a child's lifecycle state. A key cannot suspend its own org.
pub async fn update_org(
    State((db, _, _, _)): State<ApiStateData>,
    Extension(principal): Extension<OrgPrincipal>,
    Path(org_id): Path<Uuid>,
    Json(body): Json<UpdateOrgRequest>,
) -> Result<Json<ApiResponse<OrgResponse>>, ApiError> {
    principal.require(&org_urn::org(&org_id), actions::UPDATE)?;
    let org = principal
        .key
        .resolve_org(&db, org_id, false)
        .await
        .map_err(|e| key_error(e, "Organization"))?;
    if org.parent_org_id != Some(principal.key.org_id) {
        return Err(bad_request(
            "only a child organization's state can be changed",
        ));
    }
    let state = match body.state.as_str() {
        "active" => OrgLifecycle::Active,
        "suspended" => OrgLifecycle::Suspended,
        "archived" => OrgLifecycle::Archived,
        _ => return Err(bad_request("state must be active, suspended, or archived")),
    };
    Org::set_lifecycle_authorized(&db, &org_id, &principal.key.created_by_user_id, state)
        .await
        .map_err(org_error)?;
    tracing::info!(org_api_key_id = %principal.key.org_api_key_id, target_org_id = %org_id, action = "org.lifecycle", "org API mutation");
    let org = principal
        .key
        .resolve_org(&db, org_id, false)
        .await
        .map_err(|e| key_error(e, "Organization"))?;
    Ok(Json(ApiResponse::new(org.into())))
}

/// Account usage for the key's root: durable admissions across the root and
/// every child in the current billing period.
pub async fn get_usage(
    State((db, _, conf, _)): State<ApiStateData>,
    Extension(principal): Extension<OrgPrincipal>,
    Path(org_id): Path<Uuid>,
) -> Result<Json<ApiResponse<serde_json::Value>>, ApiError> {
    principal.require(&org_urn::usage(&org_id), actions::READ)?;
    if org_id != principal.key.org_id {
        return Err(bad_request(
            "usage is reported for the account; use the root organization's ID",
        ));
    }
    let usage_db = db.with_scope(DatabaseScope::Maintenance {
        operation: hot::db::MaintenanceOperation::Billing,
        org_id: Some(org_id),
    });
    let (_, features, period_start, period_end) =
        super::org::account_plan_details(&usage_db, &org_id, hot::product::billing_enabled(&conf))
            .await
            .map_err(|_| internal("account plan is unavailable"))?;
    let usage = hot::db::subscription::OrgUsageStats::calculate_account(
        &usage_db,
        &org_id,
        period_start,
        features.call_retention_days(),
    )
    .await
    .map_err(|_| internal("account usage is unavailable"))?;
    Ok(Json(ApiResponse::new(serde_json::json!({
        "org_id": org_id,
        "usage_scope": "account",
        "period_start": period_start,
        "period_end": period_end,
        "usage": usage,
    }))))
}

// ============================================================================
// Environments
// ============================================================================

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct OrgEnvResponse {
    pub env_id: Uuid,
    pub org_id: Uuid,
    pub name: String,
    pub active: bool,
    pub created_at: DateTime<Utc>,
}

impl From<Env> for OrgEnvResponse {
    fn from(env: Env) -> Self {
        Self {
            env_id: env.env_id,
            org_id: env.org_id,
            name: env.name,
            active: env.active,
            created_at: env.created_at,
        }
    }
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct CreateEnvRequest {
    pub name: String,
}

pub async fn list_envs(
    State((db, _, _, _)): State<ApiStateData>,
    Extension(principal): Extension<OrgPrincipal>,
    Path(org_id): Path<Uuid>,
) -> Result<Json<ApiListResponse<OrgEnvResponse>>, ApiError> {
    principal.require(&org_urn::envs(&org_id), actions::READ)?;
    let org = principal
        .key
        .resolve_org(&db, org_id, false)
        .await
        .map_err(|e| key_error(e, "Organization"))?;
    let scoped = db.with_scope(DatabaseScope::Organization { org_id: org.org_id });
    let envs: Vec<OrgEnvResponse> = Env::get_envs_by_org(&scoped, &org.org_id)
        .await
        .map_err(internal)?
        .into_iter()
        .map(OrgEnvResponse::from)
        .collect();
    let total = envs.len() as i64;
    Ok(Json(ApiListResponse::new(envs, total, total.max(1), 0)))
}

/// Idempotent by exact name within the organization.
pub async fn create_env(
    State((db, _, conf, _)): State<ApiStateData>,
    Extension(principal): Extension<OrgPrincipal>,
    Path(org_id): Path<Uuid>,
    Json(body): Json<CreateEnvRequest>,
) -> Result<(StatusCode, Json<ApiResponse<OrgEnvResponse>>), ApiError> {
    principal.require(&org_urn::envs(&org_id), actions::CREATE)?;
    let org = principal
        .key
        .resolve_org(&db, org_id, true)
        .await
        .map_err(|e| key_error(e, "Organization"))?;
    let env = Org::provision_environment_authorized(
        &db,
        &org.org_id,
        &principal.key.created_by_user_id,
        &body.name,
        hot::product::billing_enabled(&conf),
    )
    .await
    .map_err(org_error)?;
    Ok((StatusCode::CREATED, Json(ApiResponse::new(env.into()))))
}

pub async fn get_env(
    State((db, _, _, _)): State<ApiStateData>,
    Extension(principal): Extension<OrgPrincipal>,
    Path((org_id, env_id)): Path<(Uuid, Uuid)>,
) -> Result<Json<ApiResponse<OrgEnvResponse>>, ApiError> {
    principal.require(&org_urn::env(&org_id, &env_id), actions::READ)?;
    principal
        .key
        .resolve_org(&db, org_id, false)
        .await
        .map_err(|e| key_error(e, "Organization"))?;
    let lookup = db.with_scope(DatabaseScope::Bootstrap(BootstrapOperation::Authentication));
    let env = Env::get_env_uncached(&lookup, &env_id)
        .await
        .map_err(|_| not_found("Environment"))?;
    if env.org_id != org_id {
        return Err(not_found("Environment"));
    }
    Ok(Json(ApiResponse::new(env.into())))
}

// ============================================================================
// API keys
// ============================================================================

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct ApiKeyResponse {
    pub api_key_id: Uuid,
    /// `org` or `env`.
    pub scope: &'static str,
    pub org_id: Uuid,
    pub env_id: Option<Uuid>,
    pub description: String,
    pub permissions: serde_json::Value,
    pub active: bool,
    pub expires_at: Option<DateTime<Utc>>,
    /// The org key that minted this key; null when a person created it.
    pub issuer_api_key_id: Option<Uuid>,
    pub created_at: DateTime<Utc>,
    /// Returned once, when the key is created or rotated.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    /// When the previous secret stops working, after a rotation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_secret_expires_at: Option<DateTime<Utc>>,
}

fn org_key_response(key: &OrgApiKey) -> ApiKeyResponse {
    ApiKeyResponse {
        api_key_id: key.org_api_key_id,
        scope: "org",
        org_id: key.org_id,
        env_id: None,
        description: key.description.clone(),
        permissions: key.permissions.clone(),
        active: key.is_usable(),
        expires_at: key.expires_at,
        issuer_api_key_id: key.issuer_org_api_key_id,
        created_at: key.created_at,
        token: None,
        previous_secret_expires_at: None,
    }
}

async fn env_key_response(
    db: &DatabasePool,
    env: &Env,
    key: &ApiKey,
) -> Result<ApiKeyResponse, ApiError> {
    let lookup = db.with_scope(DatabaseScope::Bootstrap(BootstrapOperation::Authentication));
    let state = ApiKey::get_credential_state(&lookup, &key.api_key_id)
        .await
        .map_err(|e| internal(format!("{e:?}")))?;
    let issuer = OrgApiKey::issuer_of_env_key(&lookup, key.api_key_id)
        .await
        .map_err(internal)?;
    Ok(ApiKeyResponse {
        api_key_id: key.api_key_id,
        scope: "env",
        org_id: env.org_id,
        env_id: Some(env.env_id),
        description: key.description.clone(),
        permissions: key.permissions.clone(),
        active: key.active && !state.is_expired(),
        expires_at: state.expires_at,
        issuer_api_key_id: issuer,
        created_at: key.created_at,
        token: None,
        previous_secret_expires_at: None,
    })
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct CreateApiKeyRequest {
    pub description: String,
    pub permissions: serde_json::Value,
    /// Optional; never later than the issuing key's expiry.
    #[serde(default)]
    pub expires_at: Option<DateTime<Utc>>,
}

pub async fn list_env_keys(
    State((db, _, _, _)): State<ApiStateData>,
    Extension(principal): Extension<OrgPrincipal>,
    Path((org_id, env_id)): Path<(Uuid, Uuid)>,
    Query(params): Query<ListQueryParams>,
) -> Result<Json<ApiListResponse<ApiKeyResponse>>, ApiError> {
    principal.require(&org_urn::env_api_keys(&org_id, &env_id), actions::READ)?;
    let (org, env) = principal
        .key
        .resolve_env(&db, org_id, env_id)
        .await
        .map_err(|e| key_error(e, "Environment"))?;
    let scoped = db.with_scope(DatabaseScope::Environment {
        org_id: org.org_id,
        env_id: env.env_id,
    });
    let keys = ApiKey::get_api_keys_by_env(&scoped, &env.env_id, None, None)
        .await
        .map_err(|e| internal(format!("{e:?}")))?;
    let total = keys.len() as i64;
    let mut page = Vec::new();
    for key in keys
        .iter()
        .skip(params.offset.max(0) as usize)
        .take(params.limit as usize)
    {
        page.push(env_key_response(&db, &env, key).await?);
    }
    Ok(Json(ApiListResponse::new(
        page,
        total,
        params.limit,
        params.offset,
    )))
}

pub async fn mint_env_key(
    State((db, _, _, _)): State<ApiStateData>,
    Extension(principal): Extension<OrgPrincipal>,
    Path((org_id, env_id)): Path<(Uuid, Uuid)>,
    Json(body): Json<CreateApiKeyRequest>,
) -> Result<(StatusCode, Json<ApiResponse<ApiKeyResponse>>), ApiError> {
    principal.require(&org_urn::env_api_keys(&org_id, &env_id), actions::CREATE)?;
    let (id, token, expires_at) = principal
        .key
        .mint_env_key(
            &db,
            &principal.effective,
            org_id,
            env_id,
            &MintEnvApiKey {
                description: body.description.clone(),
                permissions: body.permissions.clone(),
                expires_at: body.expires_at,
            },
        )
        .await
        .map_err(|e| key_error(e, "Environment"))?;
    tracing::info!(org_api_key_id = %principal.key.org_api_key_id, target_org_id = %org_id, %env_id, api_key_id = %id, action = "api-key.create", "org API mutation");
    Ok((
        StatusCode::CREATED,
        Json(ApiResponse::new(ApiKeyResponse {
            api_key_id: id,
            scope: "env",
            org_id,
            env_id: Some(env_id),
            description: body.description.trim().to_string(),
            permissions: body.permissions,
            active: true,
            expires_at,
            issuer_api_key_id: Some(principal.key.org_api_key_id),
            created_at: Utc::now(),
            token: Some(token),
            previous_secret_expires_at: None,
        })),
    ))
}

pub async fn list_org_keys(
    State((db, _, _, _)): State<ApiStateData>,
    Extension(principal): Extension<OrgPrincipal>,
    Path(org_id): Path<Uuid>,
) -> Result<Json<ApiListResponse<ApiKeyResponse>>, ApiError> {
    principal.require(&org_urn::org_api_keys(&org_id), actions::READ)?;
    if org_id != principal.key.org_id {
        return Err(not_found("Organization"));
    }
    let keys: Vec<ApiKeyResponse> = OrgApiKey::list_for_org(&db, org_id)
        .await
        .map_err(|e| key_error(e, "Organization"))?
        .iter()
        .map(org_key_response)
        .collect();
    let total = keys.len() as i64;
    Ok(Json(ApiListResponse::new(keys, total, total.max(1), 0)))
}

pub async fn mint_org_key(
    State((db, _, _, _)): State<ApiStateData>,
    Extension(principal): Extension<OrgPrincipal>,
    Path(org_id): Path<Uuid>,
    Json(body): Json<CreateApiKeyRequest>,
) -> Result<(StatusCode, Json<ApiResponse<ApiKeyResponse>>), ApiError> {
    principal.require(&org_urn::org_api_keys(&org_id), actions::CREATE)?;
    if org_id != principal.key.org_id {
        return Err(not_found("Organization"));
    }
    let (key, token) = principal
        .key
        .mint_org_key(
            &db,
            &principal.effective,
            &CreateOrgApiKey {
                description: body.description,
                permissions: body.permissions,
                expires_at: body.expires_at,
            },
        )
        .await
        .map_err(|e| key_error(e, "Organization"))?;
    tracing::info!(org_api_key_id = %principal.key.org_api_key_id, minted = %key.org_api_key_id, action = "org-api-key.create", "org API mutation");
    let mut response = org_key_response(&key);
    response.token = Some(token);
    Ok((StatusCode::CREATED, Json(ApiResponse::new(response))))
}

/// A key addressed by ID, with the qualified URN that authorizes it.
enum Target {
    Org(OrgApiKey),
    Env { key: ApiKey, env: Env },
}

impl Target {
    fn urn(&self) -> String {
        match self {
            Target::Org(key) => org_urn::org_api_key(&key.org_id, &key.org_api_key_id),
            Target::Env { key, env } => {
                org_urn::env_api_key(&env.org_id, &env.env_id, &key.api_key_id)
            }
        }
    }
}

/// Resolve a key the caller's org or one of its children owns. Keys anywhere
/// else are not found, whatever the caller's grants.
async fn resolve_target(
    db: &DatabasePool,
    principal: &OrgPrincipal,
    key_id: Uuid,
) -> Result<Target, ApiError> {
    let lookup = db.with_scope(DatabaseScope::Bootstrap(BootstrapOperation::Authentication));
    match OrgApiKey::get(&lookup, key_id).await {
        Ok(key) if key.org_id == principal.key.org_id => return Ok(Target::Org(key)),
        Ok(_) => return Err(not_found("API key")),
        Err(OrgApiKeyError::NotFound) => {}
        Err(error) => return Err(key_error(error, "API key")),
    }
    let key = ApiKey::get_api_key(&lookup, &key_id)
        .await
        .map_err(|_| not_found("API key"))?;
    let env = Env::get_env_uncached(&lookup, &key.env_id)
        .await
        .map_err(|_| not_found("API key"))?;
    principal
        .key
        .resolve_org(&lookup, env.org_id, false)
        .await
        .map_err(|_| not_found("API key"))?;
    Ok(Target::Env { key, env })
}

/// Rotating, narrowing or revoking a key needs a grant on it and descent: the
/// caller issued it, directly or through keys it issued. Rotation returns the
/// new secret, so a grant alone would hand over another key's secret.
async fn require_descendant(
    db: &DatabasePool,
    principal: &OrgPrincipal,
    target: &Target,
    action: &str,
) -> Result<(), ApiError> {
    principal.require(&target.urn(), action)?;
    let caller = principal.key.org_api_key_id;
    let issued = match target {
        Target::Org(key) => key
            .has_ancestor(db, caller)
            .await
            .map_err(|e| key_error(e, "API key"))?,
        Target::Env { key, .. } => {
            match OrgApiKey::issuer_of_env_key(db, key.api_key_id)
                .await
                .map_err(|e| key_error(e, "API key"))?
            {
                Some(issuer) if issuer == caller => true,
                Some(issuer) => {
                    let lookup =
                        db.with_scope(DatabaseScope::Bootstrap(BootstrapOperation::Authentication));
                    OrgApiKey::get(&lookup, issuer)
                        .await
                        .map_err(|e| key_error(e, "API key"))?
                        .has_ancestor(db, caller)
                        .await
                        .map_err(|e| key_error(e, "API key"))?
                }
                None => false,
            }
        }
    };
    if issued { Ok(()) } else { Err(denied()) }
}

pub async fn get_api_key(
    State((db, _, _, _)): State<ApiStateData>,
    Extension(principal): Extension<OrgPrincipal>,
    Path(key_id): Path<Uuid>,
) -> Result<Json<ApiResponse<ApiKeyResponse>>, ApiError> {
    let target = resolve_target(&db, &principal, key_id).await?;
    principal.require(&target.urn(), actions::READ)?;
    let response = match &target {
        Target::Org(key) => org_key_response(key),
        Target::Env { key, env } => env_key_response(&db, env, key).await?,
    };
    Ok(Json(ApiResponse::new(response)))
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct UpdateApiKeyRequest {
    /// New grants; they must fit inside the caller's own.
    pub permissions: serde_json::Value,
}

pub async fn update_api_key(
    State((db, _, _, _)): State<ApiStateData>,
    Extension(principal): Extension<OrgPrincipal>,
    Path(key_id): Path<Uuid>,
    Json(body): Json<UpdateApiKeyRequest>,
) -> Result<Json<ApiResponse<ApiKeyResponse>>, ApiError> {
    let target = resolve_target(&db, &principal, key_id).await?;
    require_descendant(&db, &principal, &target, actions::UPDATE).await?;
    let caller_org = principal.key.org_id;
    let response = match target {
        Target::Org(key) => {
            let grants = Permissions::from_json(&body.permissions)
                .map_err(|e| bad_request(&e.to_string()))?;
            grants
                .validate_org_key()
                .and_then(|_| {
                    grants.validate_org_key_subset_of(&principal.effective.permissions, &caller_org)
                })
                .map_err(|e| bad_request(&e.to_string()))?;
            key.set_permissions(&db, &grants)
                .await
                .map_err(|e| key_error(e, "API key"))?;
            org_key_response(
                &OrgApiKey::get(
                    &db.with_scope(DatabaseScope::Organization { org_id: caller_org }),
                    key.org_api_key_id,
                )
                .await
                .map_err(|e| key_error(e, "API key"))?,
            )
        }
        Target::Env { key, env } => {
            let grants = Permissions::from_json_validated(&body.permissions)
                .map_err(|e| bad_request(&e.to_string()))?;
            grants
                .validate_subset_of(&principal.effective.permissions.project_to_env(
                    &caller_org,
                    &env.org_id,
                    &env.env_id,
                ))
                .map_err(|e| bad_request(&e.to_string()))?;
            let scoped = db.with_scope(DatabaseScope::Environment {
                org_id: env.org_id,
                env_id: env.env_id,
            });
            ApiKey::update_permissions(
                &scoped,
                &key.api_key_id,
                &body.permissions,
                &principal.key.created_by_user_id,
            )
            .await
            .map_err(|e| internal(format!("{e:?}")))?;
            let key = ApiKey::get_api_key(&scoped, &key.api_key_id)
                .await
                .map_err(|_| not_found("API key"))?;
            env_key_response(&db, &env, &key).await?
        }
    };
    tracing::info!(org_api_key_id = %principal.key.org_api_key_id, api_key_id = %key_id, action = "api-key.update", "org API mutation");
    Ok(Json(ApiResponse::new(response)))
}

/// Revoke a key. Everything it issued stops on its next request.
pub async fn revoke_api_key(
    State((db, _, _, _)): State<ApiStateData>,
    Extension(principal): Extension<OrgPrincipal>,
    Path(key_id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    let target = resolve_target(&db, &principal, key_id).await?;
    require_descendant(&db, &principal, &target, actions::DELETE).await?;
    match target {
        Target::Org(key) => key.revoke(&db).await.map_err(|e| key_error(e, "API key"))?,
        Target::Env { key, env } => {
            let scoped = db.with_scope(DatabaseScope::Environment {
                org_id: env.org_id,
                env_id: env.env_id,
            });
            ApiKey::toggle_active(
                &scoped,
                &key.api_key_id,
                false,
                &principal.key.created_by_user_id,
            )
            .await
            .map_err(|e| internal(format!("{e:?}")))?;
        }
    }
    tracing::info!(org_api_key_id = %principal.key.org_api_key_id, api_key_id = %key_id, action = "api-key.revoke", "org API mutation");
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Default, Deserialize, utoipa::ToSchema)]
pub struct RotateApiKeyRequest {
    /// Seconds the previous secret keeps working (default 86400, maximum 604800).
    #[serde(default)]
    pub overlap_seconds: Option<i64>,
}

/// Issue a new secret for the same key. Lineage and descendants are untouched.
pub async fn rotate_api_key(
    State((db, _, _, _)): State<ApiStateData>,
    Extension(principal): Extension<OrgPrincipal>,
    Path(key_id): Path<Uuid>,
    body: axum::body::Bytes,
) -> Result<Json<ApiResponse<ApiKeyResponse>>, ApiError> {
    // The body is optional, with or without a content type.
    let body: RotateApiKeyRequest = if body.iter().all(u8::is_ascii_whitespace) {
        RotateApiKeyRequest::default()
    } else {
        serde_json::from_slice(&body).map_err(|e| bad_request(&e.to_string()))?
    };
    let overlap = body
        .overlap_seconds
        .unwrap_or(DEFAULT_ROTATION_OVERLAP_SECONDS);
    if !(0..=MAX_ROTATION_OVERLAP_SECONDS).contains(&overlap) {
        return Err(bad_request("overlap_seconds must be between 0 and 604800"));
    }
    let overlap = Duration::seconds(overlap);
    let target = resolve_target(&db, &principal, key_id).await?;
    require_descendant(&db, &principal, &target, actions::UPDATE).await?;
    let mut response = match &target {
        Target::Org(key) => {
            let token = key
                .rotate(&db, overlap)
                .await
                .map_err(|e| key_error(e, "API key"))?;
            let mut response = org_key_response(key);
            response.token = Some(token);
            response
        }
        Target::Env { key, env } => {
            let scoped = db.with_scope(DatabaseScope::Environment {
                org_id: env.org_id,
                env_id: env.env_id,
            });
            let token = ApiKey::rotate(
                &scoped,
                &key.api_key_id,
                overlap,
                &principal.key.created_by_user_id,
            )
            .await
            .map_err(|_| not_found("API key"))?;
            let mut response = env_key_response(&db, env, key).await?;
            response.token = Some(token);
            response
        }
    };
    response.previous_secret_expires_at = Some(Utc::now() + overlap);
    tracing::info!(org_api_key_id = %principal.key.org_api_key_id, api_key_id = %key_id, action = "api-key.rotate", "org API mutation");
    Ok(Json(ApiResponse::new(response)))
}

// ============================================================================
// Project, context and build routes inside an environment
// ============================================================================

/// Re-dispatch `/v1/orgs/{org}/envs/{env}/<rest>` to the environment handler
/// for `/v1/<rest>`, as an environment principal holding the org key's grants
/// projected onto that environment.
async fn forward_to_env(
    State((db, _, _, _)): State<ApiStateData>,
    Extension(principal): Extension<OrgPrincipal>,
    Extension(forward): Extension<ForwardService>,
    Path(params): Path<std::collections::HashMap<String, String>>,
    request: Request,
) -> Response {
    let ids = (
        params.get("org_id").and_then(|v| Uuid::parse_str(v).ok()),
        params.get("env_id").and_then(|v| Uuid::parse_str(v).ok()),
    );
    let (Some(org_id), Some(env_id)) = ids else {
        return bad_request("invalid organization or environment ID").into_response();
    };
    let (org, env) = match principal.key.resolve_env(&db, org_id, env_id).await {
        Ok(found) => found,
        Err(error) => return key_error(error, "Environment").into_response(),
    };
    let projected = principal.effective.permissions.project_to_env(
        &principal.key.org_id,
        &org.org_id,
        &env.env_id,
    );
    if projected.is_empty() {
        return denied().into_response();
    }
    // Keep the raw (still percent-encoded) remainder of the path.
    let (parts, body) = request.into_parts();
    let Some(rest) = parts.uri.path().splitn(7, '/').nth(6) else {
        return not_found("Route").into_response();
    };
    let uri = match parts.uri.query() {
        Some(query) => format!("/v1/{rest}?{query}"),
        None => format!("/v1/{rest}"),
    };
    let mut builder = Request::builder()
        .method(parts.method)
        .uri(uri)
        .version(parts.version);
    if let Some(headers) = builder.headers_mut() {
        *headers = parts.headers;
    }
    let Ok(mut inner) = builder.body(body) else {
        return bad_request("invalid request").into_response();
    };
    let synthetic = ApiKey {
        api_key_id: principal.key.org_api_key_id,
        env_id: env.env_id,
        description: principal.key.description.clone(),
        key_data: serde_json::json!({}),
        active: true,
        created_by_user_id: principal.key.created_by_user_id,
        created_at: principal.key.created_at,
        updated_at: principal.key.updated_at,
        updated_by_user_id: None,
        active_toggle_at: None,
        active_toggle_by_user_id: None,
        permissions: projected.to_json(),
    };
    inner
        .extensions_mut()
        .insert(AuthContext::ApiKey(synthetic.clone()));
    inner.extensions_mut().insert(synthetic);
    let scope = DatabaseScope::Environment {
        org_id: org.org_id,
        env_id: env.env_id,
    };
    match with_database_scope(scope, forward.oneshot(inner)).await {
        Ok(response) => response.into_response(),
        Err(infallible) => match infallible {},
    }
}

/// Route-level authorization for forwarded requests, the same policy the
/// environment API applies to environment keys.
async fn authorize_forwarded(request: Request, next: Next) -> Response {
    let (mut parts, body) = request.into_parts();
    let Some(auth) = parts.extensions.get::<AuthContext>().cloned() else {
        return denied().into_response();
    };
    let Some(route) = parts
        .extensions
        .get::<MatchedPath>()
        .map(|p| p.as_str().to_owned())
    else {
        return denied().into_response();
    };
    let params = match RawPathParams::from_request_parts(&mut parts, &()).await {
        Ok(params) => params
            .iter()
            .map(|(key, value)| (key.to_owned(), value.to_owned()))
            .collect(),
        Err(_) => return bad_request("Invalid route parameters").into_response(),
    };
    if let Err(status) = authorize_route(&auth, &parts.method, &route, &params) {
        let error = if status == StatusCode::BAD_REQUEST {
            ApiErrorResponse::bad_request("Invalid route parameters")
        } else {
            ApiErrorResponse::new("forbidden", "Org API key does not authorize this action")
        };
        return (status, Json(error)).into_response();
    }
    next.run(Request::from_parts(parts, body)).await
}

#[cfg(test)]
mod postgres_lifecycle;
#[cfg(test)]
mod tests;

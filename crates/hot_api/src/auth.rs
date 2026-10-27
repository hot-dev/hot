mod route_permissions;

use crate::ApiStateData;
use crate::domain_resolver::ResolvedDomain;
use axum::{
    Json,
    extract::{FromRequestParts, MatchedPath, RawPathParams, Request, State},
    http::StatusCode,
    middleware::Next,
    response::{IntoResponse, Response},
};
use hot::db::api_key::ApiKey;
use hot::db::service_key::ServiceKey;
use hot::db::session::Session;
use hot::db::{BootstrapOperation, DatabasePool, DatabaseScope, with_database_scope};
use hot::permission::Permissions;
use std::sync::Arc;
use tracing::{debug, error};
use uuid::Uuid;

// ============================================================================
// AuthContext — unified authentication context for all handlers
// ============================================================================

/// Represents the authenticated principal for an API request.
///
/// Three credential types are supported:
/// - `ApiKey`: long-lived, environment-scoped keys for Hot Dev customers
/// - `Session`: short-lived, permission-scoped tokens for ephemeral access
/// - `ServiceKey`: long-lived, permission-scoped keys for customers and integrations (MCP, webhooks, etc.)
#[derive(Debug, Clone)]
pub enum AuthContext {
    ApiKey(ApiKey),
    Session {
        session: Session,
        /// The parent API key ID (for audit trail)
        api_key_id: Uuid,
        /// Current issuer grants, loaded afresh during authentication.
        issuer_permissions: Permissions,
    },
    ServiceKey {
        service_key: ServiceKey,
        /// The parent API key ID (for audit trail)
        api_key_id: Uuid,
        /// Current issuer grants, loaded afresh during authentication.
        issuer_permissions: Permissions,
    },
}

// A credential the database could not check is unavailable, not invalid:
// clients retry a 503 but discard a credential that got a 401.
fn api_key_status(error: hot::db::api_key::ApiKeyError) -> StatusCode {
    match error {
        hot::db::api_key::ApiKeyError::Database(_) => StatusCode::SERVICE_UNAVAILABLE,
        _ => StatusCode::UNAUTHORIZED,
    }
}

fn session_status(error: hot::db::session::SessionError) -> StatusCode {
    match error {
        hot::db::session::SessionError::Database(_) => StatusCode::SERVICE_UNAVAILABLE,
        _ => StatusCode::UNAUTHORIZED,
    }
}

fn service_key_status(error: hot::db::service_key::ServiceKeyError) -> StatusCode {
    match error {
        hot::db::service_key::ServiceKeyError::Database(_) => StatusCode::SERVICE_UNAVAILABLE,
        _ => StatusCode::UNAUTHORIZED,
    }
}

fn management_key_status(error: hot::db::management_key::ManagementKeyError) -> StatusCode {
    match error {
        hot::db::management_key::ManagementKeyError::Database(_) => StatusCode::SERVICE_UNAVAILABLE,
        _ => StatusCode::UNAUTHORIZED,
    }
}

/// A missing or RLS-hidden environment or org is forbidden; any other
/// database failure is unavailable.
fn scope_lookup_status(error: &sqlx::Error) -> StatusCode {
    if matches!(error, sqlx::Error::RowNotFound) {
        StatusCode::FORBIDDEN
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    }
}

fn env_scope_status(error: hot::db::env::EnvError) -> StatusCode {
    match error {
        hot::db::env::EnvError::Database(error) => scope_lookup_status(&error),
        hot::db::env::EnvError::NotFound => StatusCode::FORBIDDEN,
    }
}

fn org_scope_status(error: hot::db::org::OrgError) -> StatusCode {
    match error {
        hot::db::org::OrgError::Database(error) => scope_lookup_status(&error),
        _ => StatusCode::FORBIDDEN,
    }
}

impl AuthContext {
    /// Re-read a previously authenticated principal without retaining its secret.
    /// Used by long-lived connections; scope and issuer lineage cannot change.
    pub async fn revalidate(&self, db: &DatabasePool) -> Result<Self, StatusCode> {
        let db = db.with_scope(DatabaseScope::Bootstrap(BootstrapOperation::Authentication));
        let db = &db;
        with_database_scope(
            DatabaseScope::Bootstrap(BootstrapOperation::Authentication),
            async {
                let mut parent = ApiKey::get_api_key(db, &self.api_key_id())
                    .await
                    .map_err(api_key_status)?;
                if !parent.active || parent.env_id != self.env_id() {
                    return Err(StatusCode::UNAUTHORIZED);
                }
                let issuer_permissions =
                    hot::db::management_key::ManagementKey::effective_runtime_permissions(
                        db, &parent,
                    )
                    .await
                    .map_err(management_key_status)?;
                parent.permissions = issuer_permissions.to_json();
                let current = match self {
                    Self::ApiKey(_) => Self::ApiKey(parent),
                    Self::Session { session, .. } => {
                        let current = Session::get_session(db, &session.session_id)
                            .await
                            .map_err(session_status)?;
                        if current.is_expired()
                            || current.is_revoked()
                            || current.env_id != self.env_id()
                            || current.api_key_id != self.api_key_id()
                        {
                            return Err(StatusCode::UNAUTHORIZED);
                        }
                        Self::Session {
                            session: current,
                            api_key_id: parent.api_key_id,
                            issuer_permissions,
                        }
                    }
                    Self::ServiceKey { service_key, .. } => {
                        let current = ServiceKey::get_service_key(db, &service_key.service_key_id)
                            .await
                            .map_err(service_key_status)?;
                        if current.is_expired()
                            || current.is_revoked()
                            || current.env_id != self.env_id()
                            || current.api_key_id != self.api_key_id()
                        {
                            return Err(StatusCode::UNAUTHORIZED);
                        }
                        Self::ServiceKey {
                            service_key: current,
                            api_key_id: parent.api_key_id,
                            issuer_permissions,
                        }
                    }
                };
                runtime_database_scope(db, current.env_id()).await?;
                Ok(current)
            },
        )
        .await
    }
    /// Get the environment ID for this principal.
    pub fn env_id(&self) -> Uuid {
        match self {
            AuthContext::ApiKey(key) => key.env_id,
            AuthContext::Session { session, .. } => session.env_id,
            AuthContext::ServiceKey { service_key, .. } => service_key.env_id,
        }
    }

    /// Get the API key ID (for audit/tracing).
    /// For sessions and service keys, this is the parent API key that created them.
    pub fn api_key_id(&self) -> Uuid {
        match self {
            AuthContext::ApiKey(key) => key.api_key_id,
            AuthContext::Session { api_key_id, .. } => *api_key_id,
            AuthContext::ServiceKey { api_key_id, .. } => *api_key_id,
        }
    }

    /// Get the ID of the credential presented on this request.
    ///
    /// Unlike [`Self::api_key_id`], this distinguishes child sessions and
    /// service keys from their parent API key for per-credential accounting.
    pub fn credential_id(&self) -> Uuid {
        match self {
            AuthContext::ApiKey(key) => key.api_key_id,
            AuthContext::Session { session, .. } => session.session_id,
            AuthContext::ServiceKey { service_key, .. } => service_key.service_key_id,
        }
    }

    /// Check if this is an API key (not a session or service key).
    pub fn is_api_key(&self) -> bool {
        matches!(self, AuthContext::ApiKey(_))
    }

    /// Check if this is a session token.
    pub fn is_session(&self) -> bool {
        matches!(self, AuthContext::Session { .. })
    }

    /// Check if this is a service key.
    pub fn is_service_key(&self) -> bool {
        matches!(self, AuthContext::ServiceKey { .. })
    }

    /// Get the underlying ApiKey if this is an API key context.
    pub fn as_api_key(&self) -> Option<&ApiKey> {
        match self {
            AuthContext::ApiKey(key) => Some(key),
            _ => None,
        }
    }

    /// Get the underlying Session if this is a session context.
    pub fn as_session(&self) -> Option<&Session> {
        match self {
            AuthContext::Session { session, .. } => Some(session),
            _ => None,
        }
    }

    /// Get the underlying ServiceKey if this is a service key context.
    pub fn as_service_key(&self) -> Option<&ServiceKey> {
        match self {
            AuthContext::ServiceKey { service_key, .. } => Some(service_key),
            _ => None,
        }
    }

    // ========================================================================
    // Permission checking
    // ========================================================================

    /// Get the permissions for this principal.
    /// All credential types now use the unified permissions model.
    pub fn get_permissions(&self) -> Option<Permissions> {
        match self {
            AuthContext::ApiKey(key) => key.get_permissions().ok(),
            AuthContext::Session {
                session,
                issuer_permissions,
                ..
            } => Some(
                session
                    .get_permissions()
                    .ok()?
                    .intersection(issuer_permissions),
            ),
            AuthContext::ServiceKey {
                service_key,
                issuer_permissions,
                ..
            } => Some(
                service_key
                    .get_permissions()
                    .ok()?
                    .intersection(issuer_permissions),
            ),
        }
    }

    /// Check if this principal has a specific permission (resource + action).
    pub fn has_permission(&self, resource: &str, action: &str) -> bool {
        match self.get_permissions() {
            Some(perms) => perms.has_permission(resource, action),
            None => false,
        }
    }

    /// Extract service restrictions from permissions for a given prefix (e.g., "mcp:", "webhook:").
    /// Returns None if unrestricted, Some(vec) of specific service names if restricted.
    fn service_restrictions(&self, prefix: &str, action: &str) -> Option<Vec<String>> {
        let Some(perms) = self.get_permissions() else {
            return Some(Vec::new());
        };
        // Check if we have broad access (prefix + __any__)
        let any_resource = format!("{}__any__", prefix);
        if perms.has_permission(&any_resource, action) {
            return None;
        }
        // Collect specific service names from permissions
        let mut services = Vec::new();
        for resource in perms
            .inner()
            .keys()
            .filter(|resource| perms.has_permission(resource, action))
        {
            if let Some(rest) = resource.strip_prefix(prefix) {
                let service = rest.split('/').next().unwrap_or(rest);
                if service != "*" && !services.contains(&service.to_string()) {
                    services.push(service.to_string());
                }
            }
        }
        Some(services)
    }

    /// Get MCP service restrictions.
    /// Returns None if unrestricted, Some(vec) of specific service names if restricted.
    pub fn mcp_service_restrictions(&self) -> Option<Vec<String>> {
        self.service_restrictions("mcp:", "execute")
    }

    /// Get webhook service restrictions.
    pub fn webhook_service_restrictions(&self) -> Option<Vec<String>> {
        self.service_restrictions("webhook:", "execute")
    }

    /// Get stream restrictions for this principal.
    /// Returns None if unrestricted, Some(vec) of specific stream IDs if restricted.
    pub fn stream_restrictions(&self) -> Option<Vec<String>> {
        let Some(perms) = self.get_permissions() else {
            return Some(Vec::new());
        };
        perms.stream_restrictions()
    }
}

// ============================================================================
// Shared Token Authentication
// ============================================================================

/// Result of successfully authenticating a token.
/// Contains the `AuthContext` and the resolved parent `ApiKey`
/// (for backward compatibility with handlers that extract `Extension<ApiKey>`).
pub struct AuthenticatedToken {
    pub auth_ctx: AuthContext,
    /// The API key associated with this token.
    /// For API key tokens, this is the key itself.
    /// For sessions and service keys, this is the parent API key.
    pub api_key: ApiKey,
    pub scope: DatabaseScope,
}

/// Resolve current ownership and admission without using entity caches.
pub async fn runtime_database_scope(
    db: &DatabasePool,
    env_id: Uuid,
) -> Result<DatabaseScope, StatusCode> {
    let db = db.with_scope(DatabaseScope::Bootstrap(BootstrapOperation::Authentication));
    let db = &db;
    with_database_scope(
        DatabaseScope::Bootstrap(BootstrapOperation::Authentication),
        async {
            let env = hot::db::Env::get_env_uncached(db, &env_id)
                .await
                .map_err(env_scope_status)?;
            let org = hot::db::Org::get_org_uncached(db, &env.org_id)
                .await
                .map_err(org_scope_status)?;
            if !env.active || !org.is_admitting() {
                return Err(StatusCode::FORBIDDEN);
            }
            if let Some(root_id) = org.parent_org_id {
                let root = hot::db::Org::get_org_uncached(db, &root_id)
                    .await
                    .map_err(org_scope_status)?;
                if !root.is_admitting() || root.parent_org_id.is_some() {
                    return Err(StatusCode::FORBIDDEN);
                }
            }
            Ok(DatabaseScope::Environment {
                org_id: env.org_id,
                env_id,
            })
        },
    )
    .await
}

/// Keep both database scope and revocation checks alive after HTTP middleware
/// has returned the streaming body. The stream closes when its environment
/// stops admitting or the principal loses one of `requirements` (the
/// resource/action pairs the endpoint authorized); other grant changes don't
/// affect it. A check the database can't answer leaves the stream open until
/// the next one, so an outage doesn't drop every subscriber.
pub fn guard_stream<S, T>(
    source: S,
    db: Arc<DatabasePool>,
    principal: Option<AuthContext>,
    scope: DatabaseScope,
    requirements: Vec<(String, String)>,
) -> impl futures::Stream<Item = T>
where
    S: futures::Stream<Item = T> + Send,
    T: Send,
{
    guard_stream_every(
        source,
        db,
        principal,
        scope,
        requirements,
        std::time::Duration::from_secs(30),
    )
}

fn guard_stream_every<S, T>(
    source: S,
    db: Arc<DatabasePool>,
    principal: Option<AuthContext>,
    scope: DatabaseScope,
    requirements: Vec<(String, String)>,
    period: std::time::Duration,
) -> impl futures::Stream<Item = T>
where
    S: futures::Stream<Item = T> + Send,
    T: Send,
{
    use futures::StreamExt;
    async_stream::stream! {
        futures::pin_mut!(source);
        let mut interval = tokio::time::interval(period);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                biased;
                _ = interval.tick() => {
                    let admitted = match &scope {
                        DatabaseScope::Environment { env_id, .. } => match runtime_database_scope(&db, *env_id).await {
                            Ok(_) => true,
                            Err(status) if status.is_server_error() => continue,
                            Err(_) => false,
                        },
                        _ => false,
                    };
                    if !admitted { break; }
                    if let Some(principal) = &principal {
                        match principal.revalidate(&db).await {
                            Ok(current) if requirements.iter().all(|(resource,action)|current.has_permission(resource,action)) => {}
                            Err(status) if status.is_server_error() => continue,
                            _ => break,
                        }
                    }
                }
                next = with_database_scope(scope.clone(), source.next()) => {
                    match next {
                        Some(item) => yield item,
                        None => break,
                    }
                }
            }
        }
    }
}

/// Authenticate a bearer token against the database.
///
/// This is the single, shared authentication path used by both the auth middleware
/// and any handler that needs to authenticate a token directly (e.g., webhook handlers
/// with per-endpoint auth).
///
/// Dispatches by token prefix:
/// - `s_` → session token
/// - `hot_` → API key
/// - fallback → service key (no prefix)
///
/// On success, also fires a background `touch` to update `last_used_at` for
/// sessions and service keys.
pub async fn authenticate_token(
    db: &Arc<DatabasePool>,
    token: &str,
) -> Result<AuthenticatedToken, StatusCode> {
    let db = Arc::new(db.with_scope(DatabaseScope::Bootstrap(BootstrapOperation::Authentication)));
    let db = &db;
    with_database_scope(
        DatabaseScope::Bootstrap(BootstrapOperation::Authentication),
        async {
            let (auth_ctx, mut api_key) = authenticate_token_inner(db, token).await?;
            let scope = runtime_database_scope(db, auth_ctx.env_id()).await?;
            // Compatibility extraction must never expose broader issuer permissions.
            api_key.env_id = auth_ctx.env_id();
            api_key.permissions = auth_ctx
                .get_permissions()
                .ok_or(StatusCode::UNAUTHORIZED)?
                .to_json();
            let touch_db = db.with_scope(scope.clone());
            match &auth_ctx {
                AuthContext::Session { session, .. } => {
                    let id = session.session_id;
                    tokio::spawn(async move {
                        Session::touch(&touch_db, &id).await;
                    });
                }
                AuthContext::ServiceKey { service_key, .. } => {
                    let id = service_key.service_key_id;
                    tokio::spawn(async move {
                        ServiceKey::touch(&touch_db, &id).await;
                    });
                }
                AuthContext::ApiKey(_) => {}
            }
            Ok(AuthenticatedToken {
                auth_ctx,
                api_key,
                scope,
            })
        },
    )
    .await
}

async fn authenticate_token_inner(
    db: &Arc<DatabasePool>,
    token: &str,
) -> Result<(AuthContext, ApiKey), StatusCode> {
    // Management credentials are a separate control-plane principal.
    if token.starts_with("hm_") {
        return Err(StatusCode::UNAUTHORIZED);
    }
    if Session::is_session_token(token) {
        // ================================================================
        // Session token flow
        // ================================================================
        let session = Session::verify_token(db, token).await.map_err(|e| {
            debug!("Session token validation failed: {}", e);
            session_status(e)
        })?;

        let api_key_id = session.api_key_id;

        // Verify parent API key is still active
        let mut parent_key = ApiKey::get_api_key(db, &api_key_id).await.map_err(|e| {
            debug!("Parent API key not found for session: {:?}", e);
            api_key_status(e)
        })?;

        if !parent_key.active || parent_key.env_id != session.env_id {
            debug!(
                "Parent API key is inactive for session: {}",
                session.session_id
            );
            return Err(StatusCode::UNAUTHORIZED);
        }

        debug!(
            "Session validated successfully for env: {} (session: {}, parent key: {})",
            session.env_id, session.session_id, api_key_id
        );

        parent_key.permissions =
            hot::db::management_key::ManagementKey::effective_runtime_permissions(db, &parent_key)
                .await
                .map_err(management_key_status)?
                .to_json();
        let auth_ctx = AuthContext::Session {
            session: session.clone(),
            api_key_id,
            issuer_permissions: parent_key
                .get_permissions()
                .map_err(|_| StatusCode::UNAUTHORIZED)?,
        };

        Ok((auth_ctx, parent_key))
    } else if token.starts_with("hot_") {
        // ================================================================
        // API key flow
        // ================================================================
        match ApiKey::verify_api_key(db, token).await {
            Ok(Some(mut key)) => {
                key.permissions =
                    hot::db::management_key::ManagementKey::effective_runtime_permissions(db, &key)
                        .await
                        .map_err(management_key_status)?
                        .to_json();
                debug!("API key validated successfully for env: {}", key.env_id);

                let auth_ctx = AuthContext::ApiKey(key.clone());

                Ok((auth_ctx, key))
            }
            Ok(None) => {
                debug!("API key not found or invalid");
                Err(StatusCode::UNAUTHORIZED)
            }
            Err(e) => {
                error!("Error validating API key: {:?}", e);
                Err(StatusCode::INTERNAL_SERVER_ERROR)
            }
        }
    } else {
        // ================================================================
        // Service key flow (no prefix — fallback)
        // ================================================================
        let service_key = ServiceKey::verify_token(db, token).await.map_err(|e| {
            debug!("Service key validation failed: {}", e);
            service_key_status(e)
        })?;

        let api_key_id = service_key.api_key_id;

        // Verify parent API key is still active
        let mut parent_key = ApiKey::get_api_key(db, &api_key_id).await.map_err(|e| {
            debug!("Parent API key not found for service key: {:?}", e);
            api_key_status(e)
        })?;

        if !parent_key.active || parent_key.env_id != service_key.env_id {
            debug!(
                "Parent API key is inactive for service key: {}",
                service_key.service_key_id
            );
            return Err(StatusCode::UNAUTHORIZED);
        }

        debug!(
            "Service key validated successfully for env: {} (service_key: {}, parent key: {})",
            service_key.env_id, service_key.service_key_id, api_key_id
        );

        parent_key.permissions =
            hot::db::management_key::ManagementKey::effective_runtime_permissions(db, &parent_key)
                .await
                .map_err(management_key_status)?
                .to_json();
        let auth_ctx = AuthContext::ServiceKey {
            service_key: service_key.clone(),
            api_key_id,
            issuer_permissions: parent_key
                .get_permissions()
                .map_err(|_| StatusCode::UNAUTHORIZED)?,
        };

        Ok((auth_ctx, parent_key))
    }
}

// ============================================================================
// Auth Middleware
// ============================================================================

/// Middleware to validate API key, session token, or service key from Authorization header.
///
/// Supports three token types (dispatched by prefix):
/// - Session tokens: `Bearer s_<session_id>_<secret>` — short-lived, permission-scoped
/// - API keys: `Bearer hot_<uuid>_<secret>` — long-lived, environment-scoped
/// - Service keys: `Bearer <uuid>_<secret>` — long-lived, permission-scoped (no prefix, fallback)
///
/// Dispatch order: `s_` → session, `hot_` → API key, else → service key.
///
/// On success, inserts both an `AuthContext` and (for backward compatibility) an `ApiKey`
/// into the request extensions.
pub async fn api_key_auth_middleware(
    state: State<ApiStateData>,
    request: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    auth_middleware_inner(state, request, next, true).await
}

/// Production routers place action authorization inside rate limiting and logging.
pub async fn authentication_middleware(
    state: State<ApiStateData>,
    request: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    auth_middleware_inner(state, request, next, false).await
}

async fn auth_middleware_inner(
    State((db, _storage, _conf, _stream_pubsub)): State<ApiStateData>,
    mut request: Request,
    next: Next,
    authorize: bool,
) -> Result<Response, StatusCode> {
    // Get the Authorization header
    let auth_header = request
        .headers()
        .get("Authorization")
        .and_then(|header| header.to_str().ok());

    let auth_header = match auth_header {
        Some(header) => header,
        None => {
            debug!("No Authorization header found");
            return Err(StatusCode::UNAUTHORIZED);
        }
    };

    // Check if it starts with "Bearer "
    if !auth_header.starts_with("Bearer ") {
        debug!("Authorization header doesn't start with 'Bearer '");
        return Err(StatusCode::UNAUTHORIZED);
    }

    // Extract the token
    let token = &auth_header[7..]; // Remove "Bearer " prefix

    // Authenticate via the shared path
    let authenticated = authenticate_token(&db, token).await?;

    let scope = authenticated.scope;
    request.extensions_mut().insert(scope.clone());
    // Insert AuthContext for new handlers
    request.extensions_mut().insert(authenticated.auth_ctx);

    // Also insert ApiKey for backward compatibility with existing handlers
    request.extensions_mut().insert(authenticated.api_key);

    if authorize {
        request = match authorize_request(request).await {
            Ok(request) => request,
            Err(response) => return Ok(*response),
        };
    }
    Ok(with_database_scope(scope, next.run(request)).await)
}

/// Action and selector denials pass through the account rate limiter and audit.
pub async fn route_authorization_middleware(request: Request, next: Next) -> Response {
    match authorize_request(request).await {
        Ok(request) => next.run(request).await,
        Err(response) => *response,
    }
}

async fn authorize_request(mut request: Request) -> Result<Request, Box<Response>> {
    let auth_ctx = request
        .extensions()
        .get::<AuthContext>()
        .cloned()
        .ok_or_else(|| StatusCode::UNAUTHORIZED.into_response())?;
    let scope = request
        .extensions()
        .get::<DatabaseScope>()
        .cloned()
        .ok_or_else(|| StatusCode::FORBIDDEN.into_response())?;
    // If a custom domain was resolved, verify the credential belongs to the same environment.
    // This prevents using credentials from env A to access a custom domain pointing to env B.
    if let Some(resolved) = request.extensions().get::<ResolvedDomain>()
        && auth_ctx.env_id() != resolved.env_id
    {
        debug!(
            "Credential env_id {} does not match custom domain '{}' env_id {}",
            auth_ctx.env_id(),
            resolved.domain,
            resolved.env_id
        );
        return Err(Box::new(StatusCode::FORBIDDEN.into_response()));
    }

    if let DatabaseScope::Environment { org_id, env_id } = &scope {
        for (name, value) in
            url::form_urlencoded::parse(request.uri().query().unwrap_or("").as_bytes())
        {
            let expected = match name.as_ref() {
                "org_id" => Some(*org_id),
                "env_id" => Some(*env_id),
                "tenant_id" => return Err(Box::new(StatusCode::BAD_REQUEST.into_response())),
                _ => None,
            };
            if let Some(expected) = expected {
                let supplied =
                    Uuid::parse_str(&value).map_err(|_| StatusCode::BAD_REQUEST.into_response())?;
                if supplied != expected {
                    return Err(Box::new(StatusCode::FORBIDDEN.into_response()));
                }
            }
        }
    }
    let (mut parts, body) = request.into_parts();
    let route = parts
        .extensions
        .get::<MatchedPath>()
        .ok_or_else(|| StatusCode::FORBIDDEN.into_response())?
        .as_str()
        .to_owned();
    let params = RawPathParams::from_request_parts(&mut parts, &())
        .await
        .map_err(|_| StatusCode::BAD_REQUEST.into_response())?;
    let params = params
        .iter()
        .map(|(key, value)| (key.to_owned(), value.to_owned()))
        .collect();
    if let Err(status) = route_permissions::authorize(&auth_ctx, &parts.method, &route, &params) {
        let error = if status == StatusCode::BAD_REQUEST {
            crate::models::ApiErrorResponse::bad_request("Invalid route parameters")
        } else {
            crate::models::ApiErrorResponse::new(
                "forbidden",
                "Credential does not authorize this API action",
            )
        };
        return Err(Box::new((status, Json(error)).into_response()));
    }
    request = Request::from_parts(parts, body);
    Ok(request)
}

#[cfg(test)]
mod scope_tests {
    use super::*;
    use chrono::{Duration, Utc};
    use serde_json::json;

    fn session_context(child: serde_json::Value, issuer: serde_json::Value) -> AuthContext {
        let api_key_id = Uuid::now_v7();
        AuthContext::Session {
            session: Session {
                session_id: Uuid::now_v7(),
                api_key_id,
                env_id: Uuid::now_v7(),
                secret_hash: Vec::new(),
                permissions: child,
                metadata: None,
                expires_at: Utc::now() + Duration::hours(1),
                revoked_at: None,
                created_at: Utc::now(),
                last_used_at: None,
            },
            api_key_id,
            issuer_permissions: Permissions::from_json(&issuer).unwrap(),
        }
    }

    #[test]
    fn derived_principal_uses_current_issuer_for_actions_and_discovery() {
        let auth = session_context(
            json!({"mcp:*": ["execute"], "stream:*": ["read"]}),
            json!({"mcp:weather/*": ["execute"], "stream:one": ["read"]}),
        );
        assert!(auth.has_permission("mcp:weather/forecast", "execute"));
        assert!(!auth.has_permission("mcp:payments/send", "execute"));
        assert_eq!(
            auth.mcp_service_restrictions(),
            Some(vec!["weather".to_string()])
        );
        assert_eq!(auth.stream_restrictions(), Some(vec!["one".to_string()]));
    }

    #[test]
    fn missing_or_disjoint_issuer_permissions_fail_closed() {
        let empty = session_context(json!({"mcp:*": ["execute"]}), json!({}));
        assert!(!empty.has_permission("mcp:weather/forecast", "execute"));
        assert_eq!(empty.mcp_service_restrictions(), Some(Vec::new()));
        let invalid = session_context(json!(null), json!({"*:*": ["*"]}));
        assert_eq!(invalid.mcp_service_restrictions(), Some(Vec::new()));
        assert_eq!(invalid.stream_restrictions(), Some(Vec::new()));
    }

    #[test]
    fn issuer_expansion_does_not_expand_derived_grants() {
        let auth = session_context(json!({"mcp:weather/*": ["execute"]}), json!({"*:*": ["*"]}));
        assert!(auth.has_permission("mcp:weather/forecast", "execute"));
        assert!(!auth.has_permission("mcp:payments/send", "execute"));
    }

    #[test]
    fn database_failures_are_unavailable_but_missing_rows_deny() {
        use hot::db::{env::EnvError, org::OrgError, session::SessionError};
        assert_eq!(
            session_status(SessionError::Database(sqlx::Error::PoolTimedOut)),
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(
            session_status(SessionError::NotFound),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            env_scope_status(EnvError::Database(sqlx::Error::RowNotFound)),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            org_scope_status(OrgError::Database(sqlx::Error::PoolTimedOut)),
            StatusCode::SERVICE_UNAVAILABLE
        );
    }

    #[tokio::test]
    async fn stream_guard_tracks_the_required_permission_not_the_whole_grant() {
        use futures::StreamExt;
        let db = Arc::new(hot::db::test_db().await);
        let data = hot::db::insert_test_data(&db).await.unwrap();
        let api_key_id = Uuid::now_v7();
        let (token, key_data) = ApiKey::generate_api_key(&api_key_id).unwrap();
        ApiKey::insert_api_key(
            &db,
            &api_key_id,
            &data.env_id,
            "stream reader",
            &serde_json::from_str(&key_data).unwrap(),
            &data.user_id,
            &json!({"stream:*": ["read"], "project:*": ["read"]}),
        )
        .await
        .unwrap();
        let authenticated = authenticate_token(&db, &token).await.unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<u32>();
        let source = async_stream::stream! {
            while let Some(item) = rx.recv().await {
                yield item;
            }
        };
        let period = std::time::Duration::from_millis(20);
        let guarded = guard_stream_every(
            source,
            db.clone(),
            Some(authenticated.auth_ctx),
            authenticated.scope,
            vec![("stream:orders".into(), "read".into())],
            period,
        );
        futures::pin_mut!(guarded);
        tx.send(1).unwrap();
        assert_eq!(guarded.next().await, Some(1));

        // After a period, the guard re-checks before it yields the next item.
        ApiKey::update_permissions(
            &db,
            &api_key_id,
            &json!({"stream:*": ["read"]}),
            &data.user_id,
        )
        .await
        .unwrap();
        tokio::time::sleep(period * 3).await;
        tx.send(2).unwrap();
        assert_eq!(guarded.next().await, Some(2), "unrelated change");

        ApiKey::update_permissions(
            &db,
            &api_key_id,
            &json!({"project:*": ["read"]}),
            &data.user_id,
        )
        .await
        .unwrap();
        tokio::time::sleep(period * 3).await;
        tx.send(3).unwrap();
        assert_eq!(guarded.next().await, None, "required permission removed");
    }
    #[tokio::test]
    async fn authentication_reloads_issuer_and_rejects_cross_environment_lineage() {
        let db = Arc::new(hot::db::test_db().await);
        let data = hot::db::insert_test_data(&db).await.unwrap();
        let api_key_id = Uuid::now_v7();
        let (token, key_data) = ApiKey::generate_api_key(&api_key_id).unwrap();
        ApiKey::insert_api_key(
            &db,
            &api_key_id,
            &data.env_id,
            "issuer",
            &serde_json::from_str(&key_data).unwrap(),
            &data.user_id,
            &json!({"*:*": ["*"]}),
        )
        .await
        .unwrap();
        let (_, session_token) = Session::create(
            &db,
            &api_key_id,
            &data.env_id,
            &json!({"event:*": ["create", "read"]}),
            None,
            Some(3600),
        )
        .await
        .unwrap();
        assert!(
            authenticate_token(&db, &session_token)
                .await
                .unwrap()
                .auth_ctx
                .has_permission("event:order", "read")
        );
        ApiKey::update_permissions(
            &db,
            &api_key_id,
            &json!({"event:*": ["create"]}),
            &data.user_id,
        )
        .await
        .unwrap();
        let authenticated = authenticate_token(&db, &session_token).await.unwrap();
        assert!(
            authenticated
                .auth_ctx
                .has_permission("event:order", "create")
        );
        assert!(!authenticated.auth_ctx.has_permission("event:order", "read"));
        assert!(
            !authenticated
                .api_key
                .get_permissions()
                .unwrap()
                .has_permission("event:order", "read")
        );
        let (_, mismatched_token) = Session::create(
            &db,
            &api_key_id,
            &Uuid::now_v7(),
            &json!({"event:*": ["create"]}),
            None,
            Some(3600),
        )
        .await
        .unwrap();
        assert!(authenticate_token(&db, &mismatched_token).await.is_err());
        ApiKey::toggle_active(&db, &api_key_id, false, &data.user_id)
            .await
            .unwrap();
        assert!(authenticated.auth_ctx.revalidate(&db).await.is_err());
        assert!(authenticate_token(&db, &token).await.is_err());
    }

    #[tokio::test]
    async fn management_ancestor_limits_runtime_keys_and_grandchildren() {
        use hot::db::management_key::{CreateManagementKey, ManagementKey};
        use hot::db::org::{CreateChild, Org};
        let db = Arc::new(hot::db::test_db().await);
        let data = hot::db::insert_test_data(&db).await.unwrap();
        let DatabasePool::Sqlite(pool) = db.as_ref() else {
            panic!("sqlite fixture");
        };
        sqlx::query("UPDATE org SET org_type = 'organization' WHERE org_id = ?")
            .bind(data.org_id)
            .execute(pool)
            .await
            .unwrap();
        let child = Org::create_child(
            &db,
            &data.org_id,
            &data.user_id,
            &CreateChild {
                name: "Child".into(),
                slug: "scope-child".into(),
                external_id: None,
                idempotency_key: "scope-child".into(),
                hosted: false,
            },
        )
        .await
        .unwrap();
        let (manager, manager_token) = ManagementKey::create_for_user(
            &db,
            data.org_id,
            data.user_id,
            &CreateManagementKey {
                name: "issuer".into(),
                capabilities: vec!["runtime-keys:create".into()],
                target_org_ids: vec![child.org.org_id],
                all_children: false,
                delegation_permissions: Some(json!({"event:*": ["create", "read"]})),
                expires_in_seconds: 3600,
            },
        )
        .await
        .unwrap();
        assert!(authenticate_token(&db, &manager_token).await.is_err());
        let (key_id, token) = manager
            .issue_runtime_key(
                &db,
                child.org.org_id,
                child.env_id,
                "child key",
                &json!({"event:*": ["create", "read"]}),
            )
            .await
            .unwrap();
        assert!(ManagementKey::verify_token(&db, &token).await.is_err());
        let (_, session_token) = Session::create(
            &db,
            &key_id,
            &child.env_id,
            &json!({"event:*": ["create", "read"]}),
            None,
            Some(3600),
        )
        .await
        .unwrap();
        assert!(
            authenticate_token(&db, &session_token)
                .await
                .unwrap()
                .auth_ctx
                .has_permission("event:order", "read")
        );
        sqlx::query(
            "UPDATE management_key SET delegation_permissions = ? WHERE management_key_id = ?",
        )
        .bind(json!({"event:*": ["create"]}).to_string())
        .bind(manager.management_key_id)
        .execute(pool)
        .await
        .unwrap();
        let narrowed = authenticate_token(&db, &session_token)
            .await
            .unwrap()
            .auth_ctx;
        assert!(narrowed.has_permission("event:order", "create"));
        assert!(!narrowed.has_permission("event:order", "read"));
        assert!(
            manager
                .issue_runtime_key(
                    &db,
                    data.org_id,
                    data.env_id,
                    "wrong target",
                    &json!({"event:*": ["create"]})
                )
                .await
                .is_err()
        );
        sqlx::query("UPDATE management_key SET expires_at = '2000-01-01T00:00:00Z' WHERE management_key_id = ?")
            .bind(manager.management_key_id).execute(pool).await.unwrap();
        assert!(authenticate_token(&db, &token).await.is_err());
        assert!(authenticate_token(&db, &session_token).await.is_err());
        assert!(narrowed.revalidate(&db).await.is_err());
    }
}

use crate::ApiStateData;
use crate::domain_resolver::ResolvedDomain;
use axum::{
    extract::{Request, State},
    http::StatusCode,
    middleware::Next,
    response::Response,
};
use hot::db::DatabasePool;
use hot::db::api_key::ApiKey;
use hot::db::service_key::ServiceKey;
use hot::db::session::Session;
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
    },
    ServiceKey {
        service_key: ServiceKey,
        /// The parent API key ID (for audit trail)
        api_key_id: Uuid,
    },
}

impl AuthContext {
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
    fn get_permissions(&self) -> Option<Permissions> {
        match self {
            AuthContext::ApiKey(key) => key.get_permissions().ok(),
            AuthContext::Session { session, .. } => session.get_permissions().ok(),
            AuthContext::ServiceKey { service_key, .. } => service_key.get_permissions().ok(),
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
        let perms = self.get_permissions()?;
        // Check if we have broad access (prefix + __any__)
        let any_resource = format!("{}__any__", prefix);
        if perms.has_permission(&any_resource, action) {
            return None;
        }
        // Collect specific service names from permissions
        let mut services = Vec::new();
        for resource in perms.inner().keys() {
            if let Some(rest) = resource.strip_prefix(prefix) {
                let service = rest.split('/').next().unwrap_or(rest);
                if service != "*" && !services.contains(&service.to_string()) {
                    services.push(service.to_string());
                }
            }
        }
        if services.is_empty() {
            None
        } else {
            Some(services)
        }
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
        let perms = self.get_permissions()?;
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
    if Session::is_session_token(token) {
        // ================================================================
        // Session token flow
        // ================================================================
        let session = Session::verify_token(db, token).await.map_err(|e| {
            debug!("Session token validation failed: {}", e);
            StatusCode::UNAUTHORIZED
        })?;

        let api_key_id = session.api_key_id;

        // Verify parent API key is still active
        let parent_key = ApiKey::get_api_key(db, &api_key_id).await.map_err(|e| {
            debug!("Parent API key not found for session: {:?}", e);
            StatusCode::UNAUTHORIZED
        })?;

        if !parent_key.active {
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

        let auth_ctx = AuthContext::Session {
            session: session.clone(),
            api_key_id,
        };

        // Fire-and-forget: update last_used_at
        let db_clone = db.clone();
        let sid = session.session_id;
        tokio::spawn(async move {
            Session::touch(&db_clone, &sid).await;
        });

        Ok(AuthenticatedToken {
            auth_ctx,
            api_key: parent_key,
        })
    } else if token.starts_with("hot_") {
        // ================================================================
        // API key flow
        // ================================================================
        match ApiKey::verify_api_key(db, token).await {
            Ok(Some(key)) => {
                debug!("API key validated successfully for env: {}", key.env_id);

                let auth_ctx = AuthContext::ApiKey(key.clone());

                Ok(AuthenticatedToken {
                    auth_ctx,
                    api_key: key,
                })
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
            StatusCode::UNAUTHORIZED
        })?;

        let api_key_id = service_key.api_key_id;

        // Verify parent API key is still active
        let parent_key = ApiKey::get_api_key(db, &api_key_id).await.map_err(|e| {
            debug!("Parent API key not found for service key: {:?}", e);
            StatusCode::UNAUTHORIZED
        })?;

        if !parent_key.active {
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

        let auth_ctx = AuthContext::ServiceKey {
            service_key: service_key.clone(),
            api_key_id,
        };

        // Fire-and-forget: update last_used_at
        let db_clone = db.clone();
        let ckid = service_key.service_key_id;
        tokio::spawn(async move {
            ServiceKey::touch(&db_clone, &ckid).await;
        });

        Ok(AuthenticatedToken {
            auth_ctx,
            api_key: parent_key,
        })
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
    State((db, _storage, _conf, _stream_pubsub)): State<ApiStateData>,
    mut request: Request,
    next: Next,
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

    // If a custom domain was resolved, verify the credential belongs to the same environment.
    // This prevents using credentials from env A to access a custom domain pointing to env B.
    if let Some(resolved) = request.extensions().get::<ResolvedDomain>()
        && authenticated.auth_ctx.env_id() != resolved.env_id
    {
        debug!(
            "Credential env_id {} does not match custom domain '{}' env_id {}",
            authenticated.auth_ctx.env_id(),
            resolved.domain,
            resolved.env_id
        );
        return Err(StatusCode::FORBIDDEN);
    }

    // Insert AuthContext for new handlers
    request.extensions_mut().insert(authenticated.auth_ctx);

    // Also insert ApiKey for backward compatibility with existing handlers
    request.extensions_mut().insert(authenticated.api_key);

    Ok(next.run(request).await)
}

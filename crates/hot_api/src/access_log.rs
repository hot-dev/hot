//! Access logging middleware
//!
//! Creates an `access` record for each authenticated API request.
//! Records which credential was used, the client IP (via X-Forwarded-For),
//! user agent, and HTTP request details (host, method, path, query).
//!
//! The `access_id` is inserted into request extensions so downstream
//! handlers can attach it to runs, events, etc. for attribution.

use axum::{
    extract::{FromRequestParts, Request, State},
    http::{HeaderMap, request::Parts},
    middleware::Next,
    response::Response,
};
use hot::db::DatabasePool;
use hot::db::access::{Access, source};
use std::sync::Arc;
use tracing::debug;
use uuid::Uuid;

use crate::auth::AuthContext;
use crate::domain_resolver::ResolvedDomain;

// ============================================================================
// Extension type for downstream handlers
// ============================================================================

/// Extension injected by the access logging middleware.
/// Contains the access_id for attribution on runs, events, etc.
#[derive(Debug, Clone)]
pub struct AccessId(pub Uuid);

/// Optional access ID extractor for handlers.
///
/// Extracts the `AccessId` from request extensions if present.
/// Returns `None` if the access logging middleware didn't insert one
/// (e.g., DB insert failed or unauthenticated request).
///
/// Usage in handlers: `OptionalAccessId(access_id): OptionalAccessId`
pub struct OptionalAccessId(pub Option<Uuid>);

impl<S> FromRequestParts<S> for OptionalAccessId
where
    S: Send + Sync,
{
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        Ok(OptionalAccessId(
            parts.extensions.get::<AccessId>().map(|a| a.0),
        ))
    }
}

// ============================================================================
// IP extraction
// ============================================================================

/// Extract the client IP address from request headers.
/// Checks X-Forwarded-For first (for ALB/proxy setups), then falls back
/// to the direct connection address.
pub fn extract_client_ip(headers: &HeaderMap) -> Option<String> {
    // X-Forwarded-For: client, proxy1, proxy2
    // Take the first (leftmost) address — the original client.
    if let Some(xff) = headers.get("x-forwarded-for")
        && let Ok(xff_str) = xff.to_str()
        && let Some(first_ip) = xff_str.split(',').next()
    {
        let ip = first_ip.trim();
        if !ip.is_empty() {
            return Some(ip.to_string());
        }
    }

    None
}

// ============================================================================
// Middleware
// ============================================================================

/// Access logging middleware. Runs AFTER auth middleware (needs AuthContext).
///
/// For each authenticated request:
/// 1. Extract credential IDs from AuthContext
/// 2. Extract client IP, user agent, host, method, path, query
/// 3. Create an access record (fire-and-forget)
/// 4. Inject AccessId into request extensions
pub async fn access_log_middleware(
    State(db): State<Arc<DatabasePool>>,
    mut request: Request,
    next: Next,
) -> Response {
    // Get AuthContext (inserted by auth middleware)
    let auth_ctx = request.extensions().get::<AuthContext>().cloned();

    let auth_ctx = match auth_ctx {
        Some(ctx) => ctx,
        None => {
            // No auth context = unauthenticated request, skip logging
            return next.run(request).await;
        }
    };

    // Extract request metadata
    let ip_address = extract_client_ip(request.headers());
    let user_agent = request
        .headers()
        .get("user-agent")
        .and_then(|h| h.to_str().ok())
        .map(|s| s.to_string());
    // Use the resolved custom domain if present, otherwise fall back to the raw Host header
    let host = request
        .extensions()
        .get::<ResolvedDomain>()
        .map(|rd| rd.domain.clone())
        .or_else(|| {
            request
                .headers()
                .get("host")
                .and_then(|h| h.to_str().ok())
                .map(|s| s.to_string())
        });
    let method = request.method().to_string();
    let path = request.uri().path().to_string();
    let query_params = request.uri().query().map(|q| q.to_string());

    // Build the access record
    let env_id = auth_ctx.env_id();
    let mut builder = Access::builder(env_id, source::API);

    match &auth_ctx {
        AuthContext::ApiKey(key) => {
            builder = builder.api_key_id(key.api_key_id);
        }
        AuthContext::Session { session, .. } => {
            builder = builder.session_id(session.session_id);
        }
        AuthContext::ServiceKey { service_key, .. } => {
            builder = builder.service_key_id(service_key.service_key_id);
        }
    }

    if let Some(ip) = ip_address {
        builder = builder.ip_address(ip);
    }
    if let Some(ua) = user_agent {
        builder = builder.user_agent(ua);
    }
    if let Some(h) = host {
        builder = builder.host(h);
    }
    builder = builder.method(method);
    builder = builder.path(path);
    if let Some(q) = query_params {
        builder = builder.query_params(q);
    }

    // Insert the access record (async, but we await to get the access_id)
    match builder.insert(&db).await {
        Ok(access) => {
            debug!(
                "Access log created: {} for env {}",
                access.access_id, env_id
            );
            request.extensions_mut().insert(AccessId(access.access_id));
        }
        Err(e) => {
            // Non-fatal — don't fail the request if access logging fails
            tracing::warn!("Failed to create access log: {}", e);
        }
    }

    next.run(request).await
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_client_ip_xff() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-forwarded-for",
            "1.2.3.4, 10.0.0.1, 10.0.0.2".parse().unwrap(),
        );

        let ip = extract_client_ip(&headers);
        assert_eq!(ip.as_deref(), Some("1.2.3.4"));
    }

    #[test]
    fn test_extract_client_ip_xff_single() {
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", "203.0.113.42".parse().unwrap());

        let ip = extract_client_ip(&headers);
        assert_eq!(ip.as_deref(), Some("203.0.113.42"));
    }

    #[test]
    fn test_extract_client_ip_no_xff() {
        let headers = HeaderMap::new();

        let ip = extract_client_ip(&headers);
        assert_eq!(ip, None);
    }
}

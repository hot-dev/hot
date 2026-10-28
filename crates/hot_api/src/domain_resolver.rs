//! Domain resolution middleware
//!
//! Resolves custom domain `Host` headers to Hot Dev environments.
//! When a request arrives at (e.g.) `mcp.example.com`, this middleware
//! looks up the verified domain record and injects the resolved `env_id`
//! so downstream handlers know which environment to operate against.
//!
//! Caches negative lookups and environment ownership, but verifies live domain
//! ownership on every positive request so another instance can revoke a route.

use ahash::AHashMap;
use axum::{
    extract::{Request, State},
    http::StatusCode,
    middleware::Next,
    response::Response,
};
use hot::db::DatabasePool;
use hot::db::domain::Domain;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};
use tracing::debug;
use uuid::Uuid;

// ============================================================================
// Cache
// ============================================================================

/// TTL for cached domain lookups (positive and negative).
const CACHE_TTL: Duration = Duration::from_secs(60);

/// Maximum entries in the cache before eviction.
const MAX_CACHE_ENTRIES: usize = 10_000;

#[derive(Debug, Clone)]
struct CacheEntry {
    /// None = negative cache (domain not found or not verified)
    resolved: Option<ResolvedIds>,
    inserted_at: Instant,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct ResolvedIds {
    env_id: Uuid,
    org_id: Uuid,
}

impl CacheEntry {
    fn is_expired(&self) -> bool {
        self.inserted_at.elapsed() > CACHE_TTL
    }
}

/// Simple in-memory domain cache. Thread-safe via RwLock.
#[derive(Debug, Clone)]
pub struct DomainCache {
    entries: Arc<RwLock<AHashMap<String, CacheEntry>>>,
}

impl DomainCache {
    pub fn new() -> Self {
        Self {
            entries: Arc::new(RwLock::new(AHashMap::new())),
        }
    }

    /// Look up a domain in the cache. Returns:
    /// - Some(Some(ids)) = cache hit, domain is valid
    /// - Some(None) = cache hit, domain is not found/not verified
    /// - None = cache miss
    fn get(&self, domain: &str) -> Option<Option<ResolvedIds>> {
        let entries = self.entries.read().ok()?;
        let entry = entries.get(domain)?;
        if entry.is_expired() {
            return None;
        }
        Some(entry.resolved)
    }

    /// Remove a domain from the cache (e.g., after deletion or verification change).
    pub fn remove(&self, domain: &str) {
        if let Ok(mut entries) = self.entries.write() {
            entries.remove(domain);
        }
    }

    /// Insert a domain lookup result into the cache.
    fn insert(&self, domain: String, resolved: Option<ResolvedIds>) {
        if let Ok(mut entries) = self.entries.write() {
            if entries.len() >= MAX_CACHE_ENTRIES {
                entries.retain(|_, v| !v.is_expired());
                if entries.len() >= MAX_CACHE_ENTRIES {
                    entries.clear();
                }
            }
            entries.insert(
                domain,
                CacheEntry {
                    resolved,
                    inserted_at: Instant::now(),
                },
            );
        }
    }
}

impl Default for DomainCache {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// Resolved domain extension
// ============================================================================

/// Extension inserted when a request arrives via a custom domain.
/// Downstream handlers can check this to get the resolved org and env.
#[derive(Debug, Clone)]
pub struct ResolvedDomain {
    pub domain: String,
    pub env_id: Uuid,
    pub org_id: Uuid,
}

// ============================================================================
// Known hosts (skip resolution for these)
// ============================================================================

/// Hosts that are known API hosts (not custom domains).
const KNOWN_HOSTS: &[&str] = &["api.hot.dev", "api.hot-stg.dev", "localhost", "127.0.0.1"];

fn configured_known_hosts() -> Vec<String> {
    std::env::var("HOT_API_KNOWN_HOSTS")
        .unwrap_or_default()
        .split(',')
        .map(|host| host.trim().to_lowercase())
        .filter(|host| !host.is_empty())
        .collect()
}

/// Strip an optional port from a Host header value, handling bracketed IPv6
/// literals ("[::1]:4681" → "::1") that a naive split-on-colon mangles.
fn strip_port(host: &str) -> &str {
    if let Some(rest) = host.strip_prefix('[')
        && let Some(end) = rest.find(']')
    {
        return &rest[..end];
    }
    // A bare IPv6 literal is all colons and has no port to strip.
    if host.parse::<std::net::IpAddr>().is_ok() {
        return host;
    }
    host.split(':').next().unwrap_or(host)
}

/// Check if a host is a known API host (should skip domain resolution).
fn is_known_host(host: &str) -> bool {
    // Strip port if present
    let hostname = strip_port(host).to_lowercase();
    if KNOWN_HOSTS.contains(&hostname.as_str()) {
        return true;
    }
    if configured_known_hosts()
        .iter()
        .any(|known| known == &hostname)
    {
        return true;
    }
    // IP addresses are never custom domains. This also handles ALB health
    // checks, which send the target's private IP as the Host header.
    if hostname.parse::<std::net::IpAddr>().is_ok() {
        return true;
    }
    false
}

// ============================================================================
// Middleware
// ============================================================================

/// Domain resolution middleware.
///
/// For each incoming request:
/// 1. Extract the `Host` header
/// 2. Skip if it's a known API host (api.hot.dev, localhost, configured hosts, etc.)
/// 3. Check the negative cache
/// 4. Query the DB for the current verified domain record
/// 5. If found, inject `ResolvedDomain` into request extensions
/// 6. If not found, return 404
pub async fn domain_resolution_middleware(
    State((db, cache)): State<(Arc<DatabasePool>, DomainCache)>,
    mut request: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    let db = Arc::new(db.with_scope(hot::db::DatabaseScope::Bootstrap(
        hot::db::BootstrapOperation::Routing,
    )));
    // Extract host header
    let host = request
        .headers()
        .get("host")
        .and_then(|h| h.to_str().ok())
        .map(|h| strip_port(h).to_lowercase());

    let host = match host {
        Some(h) => h,
        None => {
            // No host header — still inject the cache for downstream handlers
            request.extensions_mut().insert(cache.clone());
            return Ok(next.run(request).await);
        }
    };

    // Always inject the cache so API handlers (e.g., domain delete) can invalidate entries
    request.extensions_mut().insert(cache.clone());

    // Skip known hosts
    if is_known_host(&host) {
        return Ok(next.run(request).await);
    }

    // A positive entry is not routing authority: deletion/verification changes
    // on another API instance do not invalidate this process's cache.
    let cached_ids = match cache.get(&host) {
        Some(Some(ids)) => Some(ids),
        Some(None) => {
            debug!("Domain cache hit (negative): {}", host);
            return Err(StatusCode::NOT_FOUND);
        }
        None => None,
    };

    // Query DB for the domain, then look up the env to get org_id
    match Domain::get_by_domain_name(&db, &host).await {
        Ok(domain) => {
            let env_id = domain.env_id;
            // Environment ownership is immutable. Reuse it only when the live
            // domain still points to the cached environment.
            let org_id = if let Some(ids) = cached_ids.filter(|ids| ids.env_id == env_id) {
                ids.org_id
            } else {
                match hot::db::Env::get_env(&db, &env_id).await {
                    Ok(env) => env.org_id,
                    // A missing environment is cached as not found below; a
                    // failed lookup is not, so it can't hide the domain.
                    Err(hot::db::env::EnvError::Database(error))
                        if !matches!(error, sqlx::Error::RowNotFound) =>
                    {
                        tracing::warn!(%error, "Cannot resolve domain environment");
                        return Err(StatusCode::SERVICE_UNAVAILABLE);
                    }
                    Err(_) => {
                        debug!(
                            "Domain {} resolved to env {} but env lookup failed",
                            host, env_id
                        );
                        cache.insert(host, None);
                        return Err(StatusCode::NOT_FOUND);
                    }
                }
            };
            debug!(
                "Domain resolved: {} -> env {}, org {}",
                host, env_id, org_id
            );

            cache.insert(host.clone(), Some(ResolvedIds { env_id, org_id }));

            request.extensions_mut().insert(ResolvedDomain {
                domain: host,
                env_id,
                org_id,
            });

            Ok(next.run(request).await)
        }
        Err(hot::db::domain::DomainError::Database(error)) => {
            tracing::warn!(%error, "Cannot resolve custom domain");
            Err(StatusCode::SERVICE_UNAVAILABLE)
        }
        Err(_) => {
            debug!("Domain not found or not verified: {}", host);
            cache.insert(host, None);
            Err(StatusCode::NOT_FOUND)
        }
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cached_domain_rechecks_remote_reassignment_and_revocation() {
        use axum::{
            Extension, Router,
            body::{Body, to_bytes},
            middleware,
            routing::get,
        };
        use tower::ServiceExt;

        let db = Arc::new(hot::db::test_db().await);
        let data = hot::db::insert_test_data(&db).await.unwrap();
        let other_org = Uuid::now_v7();
        let other_env = Uuid::now_v7();
        hot::db::Org::insert_org(
            &db,
            &other_org,
            "Other",
            &other_org.to_string(),
            "organization",
            &data.user_id,
        )
        .await
        .unwrap();
        hot::db::Env::insert_env(&db, &other_env, &other_org, "production", &data.user_id)
            .await
            .unwrap();
        let DatabasePool::Sqlite(pool) = db.as_ref() else {
            unreachable!()
        };
        sqlx::query("UPDATE org SET features = ? WHERE org_id IN (?, ?)")
            .bind(r#"{"custom_domains": 2}"#)
            .bind(data.org_id)
            .bind(other_org)
            .execute(pool)
            .await
            .unwrap();
        let host = "routed.example.com";
        let original = Domain::create(&db, &data.env_id, host).await.unwrap();
        Domain::mark_verified(&db, &original.domain_id)
            .await
            .unwrap();
        let app = Router::new()
            .route(
                "/scope",
                get(|Extension(domain): Extension<ResolvedDomain>| async move {
                    format!("{}:{}", domain.org_id, domain.env_id)
                }),
            )
            .layer(middleware::from_fn_with_state(
                (db.clone(), DomainCache::new()),
                domain_resolution_middleware,
            ));
        let request = || {
            axum::http::Request::builder()
                .uri("/scope")
                .header("host", host)
                .body(Body::empty())
                .unwrap()
        };
        let response = app.clone().oneshot(request()).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            to_bytes(response.into_body(), 1024).await.unwrap(),
            format!("{}:{}", data.org_id, data.env_id)
        );

        // Simulate deletion, cleanup and reassignment on another server. The
        // warmed local cache receives no invalidation message.
        Domain::soft_delete(&db, &original.domain_id).await.unwrap();
        Domain::hard_delete(&db, &original.domain_id).await.unwrap();
        let replacement = Domain::create(&db, &other_env, host).await.unwrap();
        Domain::mark_verified(&db, &replacement.domain_id)
            .await
            .unwrap();
        let response = app.clone().oneshot(request()).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            to_bytes(response.into_body(), 1024).await.unwrap(),
            format!("{other_org}:{other_env}")
        );

        Domain::soft_delete(&db, &replacement.domain_id)
            .await
            .unwrap();
        assert_eq!(
            app.oneshot(request()).await.unwrap().status(),
            StatusCode::NOT_FOUND
        );
    }

    #[test]
    fn test_known_hosts() {
        assert!(is_known_host("api.hot.dev"));
        assert!(is_known_host("api.hot-stg.dev"));
        assert!(is_known_host("localhost"));
        assert!(is_known_host("localhost:4681"));
        assert!(is_known_host("127.0.0.1"));
        assert!(is_known_host("127.0.0.1:4681"));

        // IP addresses (ALB health checks use target IP)
        assert!(is_known_host("172.31.12.241"));
        assert!(is_known_host("10.0.1.50"));
        assert!(is_known_host("192.168.1.1"));
        assert!(is_known_host("54.200.1.1"));

        // Bracketed IPv6 literals ("localhost" can resolve to ::1)
        assert!(is_known_host("[::1]"));
        assert!(is_known_host("[::1]:4681"));
        assert!(is_known_host("::1"));
        assert!(is_known_host("[fe80::1]:4681"));

        assert!(!is_known_host("mcp.example.com"));
        assert!(!is_known_host("webhook.example.io"));
    }

    #[test]
    fn test_strip_port() {
        assert_eq!(strip_port("localhost:4681"), "localhost");
        assert_eq!(strip_port("localhost"), "localhost");
        assert_eq!(strip_port("[::1]:4681"), "::1");
        assert_eq!(strip_port("[::1]"), "::1");
        assert_eq!(strip_port("::1"), "::1");
        assert_eq!(strip_port("127.0.0.1:4681"), "127.0.0.1");
    }

    #[test]
    fn test_cache_basic() {
        let cache = DomainCache::new();
        let env_id = Uuid::now_v7();
        let org_id = Uuid::now_v7();

        // Cache miss
        assert!(cache.get("mcp.example.com").is_none());

        // Insert positive
        cache.insert(
            "mcp.example.com".to_string(),
            Some(ResolvedIds { env_id, org_id }),
        );
        let hit = cache.get("mcp.example.com").unwrap().unwrap();
        assert_eq!(hit.env_id, env_id);
        assert_eq!(hit.org_id, org_id);

        // Insert negative
        cache.insert("unknown.com".to_string(), None);
        assert_eq!(cache.get("unknown.com"), Some(None));
    }
}

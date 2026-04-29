//! Domain resolution middleware
//!
//! Resolves custom domain `Host` headers to Hot Dev environments.
//! When a request arrives at (e.g.) `mcp.example.com`, this middleware
//! looks up the verified domain record and injects the resolved `env_id`
//! so downstream handlers know which environment to operate against.
//!
//! Uses a simple in-memory cache with TTL to avoid DB lookups on every request.

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

/// Check if a host is a known API host (should skip domain resolution).
fn is_known_host(host: &str) -> bool {
    // Strip port if present
    let hostname = host.split(':').next().unwrap_or(host).to_lowercase();
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
/// 3. Check the domain cache
/// 4. On cache miss, query the DB for a verified domain record
/// 5. If found, inject `ResolvedDomain` into request extensions
/// 6. If not found, return 404
pub async fn domain_resolution_middleware(
    State((db, cache)): State<(Arc<DatabasePool>, DomainCache)>,
    mut request: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    // Extract host header
    let host = request
        .headers()
        .get("host")
        .and_then(|h| h.to_str().ok())
        .map(|h| h.split(':').next().unwrap_or(h).to_lowercase());

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

    // Check cache
    match cache.get(&host) {
        Some(Some(ids)) => {
            debug!("Domain cache hit: {} -> env {}", host, ids.env_id);
            request.extensions_mut().insert(ResolvedDomain {
                domain: host,
                env_id: ids.env_id,
                org_id: ids.org_id,
            });
            return Ok(next.run(request).await);
        }
        Some(None) => {
            debug!("Domain cache hit (negative): {}", host);
            return Err(StatusCode::NOT_FOUND);
        }
        None => {
            // Cache miss: query DB
        }
    }

    // Query DB for the domain, then look up the env to get org_id
    match Domain::get_by_domain_name(&db, &host).await {
        Ok(domain) => {
            let env_id = domain.env_id;
            let org_id = match hot::db::Env::get_env(&db, &env_id).await {
                Ok(env) => env.org_id,
                Err(_) => {
                    debug!(
                        "Domain {} resolved to env {} but env lookup failed",
                        host, env_id
                    );
                    cache.insert(host, None);
                    return Err(StatusCode::NOT_FOUND);
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

        assert!(!is_known_host("mcp.example.com"));
        assert!(!is_known_host("webhook.example.io"));
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

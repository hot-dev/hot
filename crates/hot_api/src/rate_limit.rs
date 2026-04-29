//! Per-org rate limiting middleware (sliding window, in-memory).
//!
//! Enforces requests-per-second limits based on the org's subscription plan
//! features. Uses a sliding window algorithm with in-memory state.
//!
//! ## Architecture
//!
//! Three caches avoid repeated DB lookups on the hot path:
//! - **env → org**: Maps `env_id` to `org_id` (essentially permanent — envs don't move between orgs)
//! - **org → rate_limit_rps**: Caches the resolved feature value with a 60-second TTL
//! - **sliding window**: Per-org `VecDeque<Instant>` tracking recent request timestamps
//!
//! ## Behavior
//!
//! - Runs AFTER auth middleware (requires `AuthContext` in request extensions)
//! - Returns 429 Too Many Requests with `Retry-After` header when the limit is exceeded
//! - Unlimited (`-1`) orgs bypass the check entirely
//! - Falls through gracefully on lookup failures (allow rather than block)

use ahash::AHashMap;
use axum::{
    extract::{Request, State},
    http::StatusCode,
    middleware::Next,
    response::{IntoResponse, Response},
};
use hot::db::env::Env;
use hot::db::features::Features;
use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tracing::debug;
use uuid::Uuid;

use crate::ApiStateData;
use crate::auth::AuthContext;

// ============================================================================
// Env → Org cache (rarely changes, no TTL)
// ============================================================================

struct EnvToOrgCache {
    cache: Mutex<AHashMap<Uuid, Uuid>>,
}

impl EnvToOrgCache {
    fn new() -> Self {
        Self {
            cache: Mutex::new(AHashMap::new()),
        }
    }

    fn get(&self, env_id: &Uuid) -> Option<Uuid> {
        self.cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(env_id)
            .copied()
    }

    fn insert(&self, env_id: Uuid, org_id: Uuid) {
        self.cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(env_id, org_id);
    }
}

// ============================================================================
// Features cache (with TTL)
// ============================================================================

struct FeaturesEntry {
    rate_limit_rps: i64,
    fetched_at: Instant,
}

struct FeaturesCache {
    cache: Mutex<AHashMap<Uuid, FeaturesEntry>>,
    ttl: Duration,
}

impl FeaturesCache {
    fn new(ttl: Duration) -> Self {
        Self {
            cache: Mutex::new(AHashMap::new()),
            ttl,
        }
    }

    fn get(&self, org_id: &Uuid) -> Option<i64> {
        let cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        cache.get(org_id).and_then(|entry| {
            if entry.fetched_at.elapsed() < self.ttl {
                Some(entry.rate_limit_rps)
            } else {
                None
            }
        })
    }

    fn insert(&self, org_id: Uuid, rate_limit_rps: i64) {
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        cache.insert(
            org_id,
            FeaturesEntry {
                rate_limit_rps,
                fetched_at: Instant::now(),
            },
        );
    }
}

// ============================================================================
// Sliding window rate limiter (per org, 1-second window)
// ============================================================================

struct SlidingWindowLimiter {
    windows: Mutex<AHashMap<Uuid, VecDeque<Instant>>>,
    window_duration: Duration,
    last_sweep: Mutex<Instant>,
}

/// How often to sweep stale entries from the windows map.
const SWEEP_INTERVAL: Duration = Duration::from_secs(60);

/// Remove org entries whose newest request is older than this.
const STALE_THRESHOLD: Duration = Duration::from_secs(300); // 5 minutes

impl SlidingWindowLimiter {
    fn new() -> Self {
        Self {
            windows: Mutex::new(AHashMap::new()),
            window_duration: Duration::from_secs(1),
            last_sweep: Mutex::new(Instant::now()),
        }
    }

    /// Check if a request from the given org is allowed.
    /// Returns `Ok(())` if allowed, `Err(retry_after_secs)` if rate limited.
    ///
    /// Also performs periodic eviction of stale org entries to prevent
    /// unbounded memory growth from inactive orgs.
    fn check(&self, org_id: &Uuid, max_rps: i64) -> Result<(), f64> {
        let mut windows = self.windows.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        let cutoff = now - self.window_duration;

        // Periodic sweep: remove stale org entries to prevent unbounded memory growth
        {
            let mut last_sweep = self.last_sweep.lock().unwrap_or_else(|e| e.into_inner());
            if now.duration_since(*last_sweep) >= SWEEP_INTERVAL {
                let stale_cutoff = now - STALE_THRESHOLD;
                windows.retain(|_, deque| {
                    // Keep entries that have recent requests or are non-empty with fresh data
                    deque.back().is_some_and(|t| *t >= stale_cutoff)
                });
                *last_sweep = now;
            }
        }

        let window = windows.entry(*org_id).or_default();

        // Evict expired entries from this org's window
        while window.front().is_some_and(|t| *t < cutoff) {
            window.pop_front();
        }

        if window.len() >= max_rps as usize {
            let oldest = window.front().unwrap();
            let retry_after =
                self.window_duration.as_secs_f64() - now.duration_since(*oldest).as_secs_f64();
            return Err(retry_after.max(0.1));
        }

        window.push_back(now);
        Ok(())
    }
}

// ============================================================================
// Global state (lazy-initialized singleton)
// ============================================================================

struct RateLimitState {
    env_to_org: EnvToOrgCache,
    features: FeaturesCache,
    limiter: SlidingWindowLimiter,
}

static STATE: once_cell::sync::Lazy<RateLimitState> =
    once_cell::sync::Lazy::new(|| RateLimitState {
        env_to_org: EnvToOrgCache::new(),
        features: FeaturesCache::new(Duration::from_secs(60)),
        limiter: SlidingWindowLimiter::new(),
    });

// ============================================================================
// Middleware
// ============================================================================

/// Per-org rate limiting middleware. Runs AFTER auth middleware (needs `AuthContext`).
///
/// Enforces per-org RPS limits from the subscription plan's `rate_limit_rps` feature.
/// Returns 429 Too Many Requests with `Retry-After` header when the limit is exceeded.
pub async fn rate_limit_middleware(
    State((db, _storage, _conf, _stream_pubsub)): State<ApiStateData>,
    request: Request,
    next: Next,
) -> Response {
    // Get AuthContext (inserted by auth middleware)
    let auth_ctx = match request.extensions().get::<AuthContext>() {
        Some(ctx) => ctx,
        None => return next.run(request).await,
    };

    let env_id = auth_ctx.env_id();

    // Resolve org_id from env_id (cached, essentially permanent)
    let org_id = if let Some(org_id) = STATE.env_to_org.get(&env_id) {
        org_id
    } else {
        match Env::get_env(&db, &env_id).await {
            Ok(env) => {
                STATE.env_to_org.insert(env_id, env.org_id);
                env.org_id
            }
            Err(_) => {
                // Can't determine org — allow request through
                debug!("Rate limit: could not resolve org_id for env {}", env_id);
                return next.run(request).await;
            }
        }
    };

    // Get rate_limit_rps for org (cached with 60s TTL)
    let rate_limit_rps = if let Some(rps) = STATE.features.get(&org_id) {
        rps
    } else {
        let features = Features::resolve_for_org(&db, &org_id).await;
        let rps = features.rate_limit_rps();
        STATE.features.insert(org_id, rps);
        rps
    };

    // -1 means unlimited — skip the check
    if rate_limit_rps < 0 {
        return next.run(request).await;
    }

    // Enforce the sliding window
    if let Err(retry_after) = STATE.limiter.check(&org_id, rate_limit_rps) {
        let retry_after_ceil = retry_after.ceil() as u64;
        return (
            StatusCode::TOO_MANY_REQUESTS,
            [("Retry-After", retry_after_ceil.to_string())],
            axum::Json(serde_json::json!({
                "error": "Rate limit exceeded",
                "retry_after": retry_after_ceil
            })),
        )
            .into_response();
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
    fn test_sliding_window_allows_within_limit() {
        let limiter = SlidingWindowLimiter::new();
        let org_id = Uuid::new_v4();

        // Should allow up to max_rps requests
        for _ in 0..10 {
            assert!(limiter.check(&org_id, 10).is_ok());
        }
    }

    #[test]
    fn test_sliding_window_rejects_over_limit() {
        let limiter = SlidingWindowLimiter::new();
        let org_id = Uuid::new_v4();

        // Fill up the window
        for _ in 0..5 {
            assert!(limiter.check(&org_id, 5).is_ok());
        }

        // Next request should be rejected
        let result = limiter.check(&org_id, 5);
        assert!(result.is_err());
        let retry_after = result.unwrap_err();
        assert!(retry_after > 0.0);
        assert!(retry_after <= 1.0);
    }

    #[test]
    fn test_sliding_window_separate_orgs() {
        let limiter = SlidingWindowLimiter::new();
        let org_a = Uuid::new_v4();
        let org_b = Uuid::new_v4();

        // Fill up org_a
        for _ in 0..3 {
            assert!(limiter.check(&org_a, 3).is_ok());
        }
        assert!(limiter.check(&org_a, 3).is_err());

        // org_b should still be allowed
        assert!(limiter.check(&org_b, 3).is_ok());
    }

    #[test]
    fn test_env_to_org_cache() {
        let cache = EnvToOrgCache::new();
        let env_id = Uuid::new_v4();
        let org_id = Uuid::new_v4();

        assert!(cache.get(&env_id).is_none());
        cache.insert(env_id, org_id);
        assert_eq!(cache.get(&env_id), Some(org_id));
    }

    #[test]
    fn test_features_cache_with_ttl() {
        let cache = FeaturesCache::new(Duration::from_millis(50));
        let org_id = Uuid::new_v4();

        cache.insert(org_id, 100);
        assert_eq!(cache.get(&org_id), Some(100));

        // After TTL expires, entry should be stale
        std::thread::sleep(Duration::from_millis(60));
        assert!(cache.get(&org_id).is_none());
    }
}

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
use hot::val::Val;
use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tracing::debug;
use uuid::Uuid;

use crate::ApiStateData;
use crate::auth::AuthContext;
use crate::models::ApiErrorResponse;

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

const DEFAULT_PUBLIC_ORG_INFLIGHT_LIMIT: i64 = 50;
const DEFAULT_SSE_PER_CREDENTIAL_LIMIT: i64 = 128;
const DEFAULT_SSE_PER_ORG_LIMIT: i64 = 1_024;
const DEFAULT_SSE_GLOBAL_LIMIT: i64 = 10_000;

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
    inflight: InFlightLimiter,
    sse_connections: SseConnectionLimiter,
}

static STATE: once_cell::sync::Lazy<RateLimitState> =
    once_cell::sync::Lazy::new(|| RateLimitState {
        env_to_org: EnvToOrgCache::new(),
        features: FeaturesCache::new(Duration::from_secs(60)),
        limiter: SlidingWindowLimiter::new(),
        inflight: InFlightLimiter::new(),
        sse_connections: SseConnectionLimiter::new(),
    });

// ============================================================================
// In-flight limiter (per org)
// ============================================================================

struct InFlightLimiter {
    counts: Mutex<AHashMap<Uuid, usize>>,
}

impl InFlightLimiter {
    fn new() -> Self {
        Self {
            counts: Mutex::new(AHashMap::new()),
        }
    }

    fn try_acquire(&'static self, org_id: Uuid, max: usize) -> Result<InFlightGuard, usize> {
        let mut counts = self.counts.lock().unwrap_or_else(|e| e.into_inner());
        let count = counts.entry(org_id).or_default();
        if *count >= max {
            return Err(*count);
        }
        *count += 1;
        Ok(InFlightGuard {
            org_id,
            limiter: self,
        })
    }

    fn release(&self, org_id: &Uuid) {
        let mut counts = self.counts.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(count) = counts.get_mut(org_id) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                counts.remove(org_id);
            }
        }
    }
}

/// RAII guard that releases an in-flight slot when the request completes.
pub struct InFlightGuard {
    org_id: Uuid,
    limiter: &'static InFlightLimiter,
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.limiter.release(&self.org_id);
    }
}

// ============================================================================
// Active SSE connection limiter
// ============================================================================

#[derive(Debug, Clone, Copy)]
struct SseConnectionLimits {
    per_credential: usize,
    per_org: usize,
    global: usize,
}

impl SseConnectionLimits {
    fn from_conf(conf: &Val) -> Self {
        fn configured_limit(conf: &Val, key: &str, default: i64) -> usize {
            conf.get_int_or_default(key, default).max(0) as usize
        }

        Self {
            per_credential: configured_limit(
                conf,
                "api.sse-per-credential-connection-limit",
                DEFAULT_SSE_PER_CREDENTIAL_LIMIT,
            ),
            per_org: configured_limit(
                conf,
                "api.sse-per-org-connection-limit",
                DEFAULT_SSE_PER_ORG_LIMIT,
            ),
            global: configured_limit(
                conf,
                "api.sse-global-connection-limit",
                DEFAULT_SSE_GLOBAL_LIMIT,
            ),
        }
    }
}

#[derive(Default)]
struct SseConnectionCounts {
    per_credential: AHashMap<Uuid, usize>,
    per_org: AHashMap<Uuid, usize>,
    global: usize,
}

struct SseConnectionLimiter {
    counts: Mutex<SseConnectionCounts>,
}

impl SseConnectionLimiter {
    fn new() -> Self {
        Self {
            counts: Mutex::new(SseConnectionCounts::default()),
        }
    }

    fn acquire(
        &'static self,
        credential_id: Uuid,
        org_id: Uuid,
        limits: SseConnectionLimits,
        mode: PublicRateLimitMode,
        context: &'static str,
    ) -> Result<SseConnectionGuard, RateLimitExceeded> {
        let mut counts = self.counts.lock().unwrap_or_else(|e| e.into_inner());
        let credential_count = counts
            .per_credential
            .get(&credential_id)
            .copied()
            .unwrap_or_default();
        let org_count = counts.per_org.get(&org_id).copied().unwrap_or_default();
        let exceeded = [
            (limits.per_credential, credential_count, "credential"),
            (limits.per_org, org_count, "organization"),
            (limits.global, counts.global, "global"),
        ]
        .into_iter()
        .find(|(limit, current, _)| *limit > 0 && *current >= *limit);

        if let Some((limit, current, scope)) = exceeded {
            tracing::warn!(
                context,
                credential_id = %credential_id,
                org_id = %org_id,
                scope,
                current,
                limit,
                enforced = mode == PublicRateLimitMode::Enforce,
                "Active SSE connection limit hit"
            );
            if mode == PublicRateLimitMode::Enforce {
                return Err(RateLimitExceeded {
                    retry_after_secs: 1,
                    message: format!("Too many active SSE connections for this {}", scope),
                });
            }
        }

        *counts.per_credential.entry(credential_id).or_default() += 1;
        *counts.per_org.entry(org_id).or_default() += 1;
        counts.global += 1;
        Ok(SseConnectionGuard {
            credential_id,
            org_id,
            limiter: self,
        })
    }

    fn release(&self, credential_id: &Uuid, org_id: &Uuid) {
        let mut counts = self.counts.lock().unwrap_or_else(|e| e.into_inner());
        decrement_and_remove(&mut counts.per_credential, credential_id);
        decrement_and_remove(&mut counts.per_org, org_id);
        counts.global = counts.global.saturating_sub(1);
    }
}

fn decrement_and_remove(counts: &mut AHashMap<Uuid, usize>, id: &Uuid) {
    if let Some(count) = counts.get_mut(id) {
        *count = count.saturating_sub(1);
        if *count == 0 {
            counts.remove(id);
        }
    }
}

/// RAII guard held by the response body, so disconnects and cancellation
/// release all active-connection counters.
pub struct SseConnectionGuard {
    credential_id: Uuid,
    org_id: Uuid,
    limiter: &'static SseConnectionLimiter,
}

impl Drop for SseConnectionGuard {
    fn drop(&mut self) {
        self.limiter.release(&self.credential_id, &self.org_id);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublicRateLimitMode {
    Observe,
    Enforce,
}

impl PublicRateLimitMode {
    pub fn from_conf(conf: &Val) -> Self {
        match conf
            .get_str_or_default("api.public-org-rate-limit-mode", "observe")
            .trim()
            .to_ascii_lowercase()
            .as_str()
        {
            "enforce" => Self::Enforce,
            _ => Self::Observe,
        }
    }

    fn sse_from_conf(conf: &Val) -> Self {
        match conf
            .get_str_or_default("api.sse-connection-limit-mode", "observe")
            .trim()
            .to_ascii_lowercase()
            .as_str()
        {
            "enforce" => Self::Enforce,
            _ => Self::Observe,
        }
    }
}

#[derive(Debug, Clone)]
pub struct RateLimitExceeded {
    pub retry_after_secs: u64,
    pub message: String,
}

pub fn rate_limit_error_body(exceeded: RateLimitExceeded) -> ApiErrorResponse {
    ApiErrorResponse::new(
        "rate_limit_exceeded",
        format!(
            "{} Retry after {} seconds.",
            exceeded.message, exceeded.retry_after_secs
        ),
    )
    .with_retry_after(exceeded.retry_after_secs)
}

pub fn rate_limit_response(exceeded: RateLimitExceeded) -> Response {
    let retry_after_secs = exceeded.retry_after_secs;
    (
        StatusCode::TOO_MANY_REQUESTS,
        [("Retry-After", retry_after_secs.to_string())],
        axum::Json(rate_limit_error_body(exceeded)),
    )
        .into_response()
}

async fn org_rate_limit_rps(db: &hot::db::DatabasePool, org_id: &Uuid) -> i64 {
    if let Some(rps) = STATE.features.get(org_id) {
        return rps;
    }

    let features = Features::resolve_for_org(db, org_id).await;
    let rps = features.rate_limit_rps();
    STATE.features.insert(*org_id, rps);
    rps
}

pub async fn check_org_rate_limit(
    db: &hot::db::DatabasePool,
    org_id: &Uuid,
    mode: PublicRateLimitMode,
    context: &'static str,
) -> Result<(), RateLimitExceeded> {
    let rate_limit_rps = org_rate_limit_rps(db, org_id).await;
    if rate_limit_rps < 0 {
        return Ok(());
    }

    if let Err(retry_after) = STATE.limiter.check(org_id, rate_limit_rps) {
        let retry_after_secs = retry_after.ceil() as u64;
        match mode {
            PublicRateLimitMode::Observe => {
                tracing::warn!(
                    context,
                    org_id = %org_id,
                    retry_after_secs,
                    "Public org RPS limit would have been hit"
                );
                Ok(())
            }
            PublicRateLimitMode::Enforce => Err(RateLimitExceeded {
                retry_after_secs,
                message: "Rate limit exceeded".to_string(),
            }),
        }
    } else {
        Ok(())
    }
}

pub async fn check_public_org_inflight(
    db: &hot::db::DatabasePool,
    conf: &Val,
    org_id: &Uuid,
    context: &'static str,
) -> Result<Option<InFlightGuard>, RateLimitExceeded> {
    let rate_limit_rps = org_rate_limit_rps(db, org_id).await;
    if rate_limit_rps < 0 {
        return Ok(None);
    }

    let configured = conf.get_int_or_default(
        "api.public-org-inflight-limit",
        DEFAULT_PUBLIC_ORG_INFLIGHT_LIMIT,
    );
    if configured <= 0 {
        return Ok(None);
    }

    match STATE.inflight.try_acquire(*org_id, configured as usize) {
        Ok(guard) => Ok(Some(guard)),
        Err(current) => {
            tracing::warn!(
                context,
                org_id = %org_id,
                current,
                limit = configured,
                "Public org in-flight limit hit"
            );
            Err(RateLimitExceeded {
                retry_after_secs: 1,
                message: "Too many requests are currently running for this organization"
                    .to_string(),
            })
        }
    }
}

pub async fn acquire_sse_connection(
    db: &hot::db::DatabasePool,
    conf: &Val,
    auth: &AuthContext,
    context: &'static str,
) -> Result<SseConnectionGuard, RateLimitExceeded> {
    let env_id = auth.env_id();
    let mode = PublicRateLimitMode::sse_from_conf(conf);
    let org_id = if let Some(org_id) = STATE.env_to_org.get(&env_id) {
        org_id
    } else {
        match Env::get_env(db, &env_id).await {
            Ok(env) => {
                STATE.env_to_org.insert(env_id, env.org_id);
                env.org_id
            }
            Err(e) => {
                tracing::warn!(
                    context,
                    %env_id,
                    error = %e,
                    enforced = mode == PublicRateLimitMode::Enforce,
                    "Could not resolve organization for SSE connection accounting"
                );
                if mode == PublicRateLimitMode::Enforce {
                    return Err(RateLimitExceeded {
                        retry_after_secs: 1,
                        message: "Unable to account for this SSE connection".to_string(),
                    });
                }
                // Observe mode must preserve successful SSE behavior. The nil
                // scope still accounts global and per-credential connections.
                Uuid::nil()
            }
        }
    };

    STATE.sse_connections.acquire(
        auth.credential_id(),
        org_id,
        SseConnectionLimits::from_conf(conf),
        mode,
        context,
    )
}

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

    if let Err(exceeded) =
        check_org_rate_limit(&db, &org_id, PublicRateLimitMode::Enforce, "api-v1").await
    {
        return rate_limit_response(exceeded);
    }

    next.run(request).await
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    async fn memory_db() -> hot::db::DatabasePool {
        hot::db::create_db_pool(&hot::val!({
            "uri": "sqlite::memory:",
            "schema": "hot",
        }))
        .await
        .expect("create memory db")
    }

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

    #[test]
    fn rate_limit_error_body_includes_retry_after() {
        let body = rate_limit_error_body(RateLimitExceeded {
            retry_after_secs: 3,
            message: "Rate limit exceeded".to_string(),
        });

        assert_eq!(body.error.code, "rate_limit_exceeded");
        assert_eq!(body.error.retry_after, Some(3));
        assert!(body.error.message.contains("Retry after 3 seconds."));
    }

    #[test]
    fn public_rate_limit_mode_defaults_to_observe() {
        let empty = Val::map_empty();
        assert_eq!(
            PublicRateLimitMode::from_conf(&empty),
            PublicRateLimitMode::Observe
        );

        let enforce = hot::val!({
            "api": {
                "public-org-rate-limit-mode": "enforce",
            },
        });
        assert_eq!(
            PublicRateLimitMode::from_conf(&enforce),
            PublicRateLimitMode::Enforce
        );
    }

    #[test]
    fn inflight_limiter_releases_on_drop() {
        let limiter = Box::leak(Box::new(InFlightLimiter::new()));
        let org_id = Uuid::new_v4();

        let guard = limiter.try_acquire(org_id, 1).unwrap();
        assert!(limiter.try_acquire(org_id, 1).is_err());
        drop(guard);
        assert!(limiter.try_acquire(org_id, 1).is_ok());
    }

    #[test]
    fn sse_limits_default_to_observe_with_high_caps() {
        let conf = Val::map_empty();
        let limits = SseConnectionLimits::from_conf(&conf);

        assert_eq!(
            PublicRateLimitMode::sse_from_conf(&conf),
            PublicRateLimitMode::Observe
        );
        assert_eq!(limits.per_credential, 128);
        assert_eq!(limits.per_org, 1_024);
        assert_eq!(limits.global, 10_000);
    }

    #[test]
    fn sse_enforcement_accounts_scopes_and_releases_on_drop() {
        let limiter = Box::leak(Box::new(SseConnectionLimiter::new()));
        let credential_id = Uuid::new_v4();
        let other_credential_id = Uuid::new_v4();
        let org_id = Uuid::new_v4();
        let limits = SseConnectionLimits {
            per_credential: 1,
            per_org: 2,
            global: 3,
        };

        let first = limiter
            .acquire(
                credential_id,
                org_id,
                limits,
                PublicRateLimitMode::Enforce,
                "test",
            )
            .unwrap();
        assert!(
            limiter
                .acquire(
                    credential_id,
                    org_id,
                    limits,
                    PublicRateLimitMode::Enforce,
                    "test",
                )
                .is_err()
        );
        let second = limiter
            .acquire(
                other_credential_id,
                org_id,
                limits,
                PublicRateLimitMode::Enforce,
                "test",
            )
            .unwrap();
        assert!(
            limiter
                .acquire(
                    Uuid::new_v4(),
                    org_id,
                    limits,
                    PublicRateLimitMode::Enforce,
                    "test",
                )
                .is_err()
        );

        drop(first);
        assert!(
            limiter
                .acquire(
                    credential_id,
                    Uuid::new_v4(),
                    limits,
                    PublicRateLimitMode::Enforce,
                    "test",
                )
                .is_ok()
        );
        drop(second);
    }

    #[test]
    fn sse_observe_mode_accounts_connections_above_limit() {
        let limiter = Box::leak(Box::new(SseConnectionLimiter::new()));
        let credential_id = Uuid::new_v4();
        let org_id = Uuid::new_v4();
        let limits = SseConnectionLimits {
            per_credential: 1,
            per_org: 1,
            global: 1,
        };

        let first = limiter
            .acquire(
                credential_id,
                org_id,
                limits,
                PublicRateLimitMode::Observe,
                "test",
            )
            .unwrap();
        let second = limiter
            .acquire(
                credential_id,
                org_id,
                limits,
                PublicRateLimitMode::Observe,
                "test",
            )
            .expect("observe mode must preserve successful SSE responses");

        let counts = limiter.counts.lock().unwrap();
        assert_eq!(counts.global, 2);
        assert_eq!(counts.per_credential.get(&credential_id), Some(&2));
        drop(counts);
        drop((first, second));
        assert_eq!(limiter.counts.lock().unwrap().global, 0);
    }

    #[tokio::test]
    async fn public_org_rps_observe_allows_would_block() {
        let db = memory_db().await;
        let org_id = Uuid::new_v4();
        STATE.features.insert(org_id, 1);

        assert!(
            check_org_rate_limit(&db, &org_id, PublicRateLimitMode::Observe, "test")
                .await
                .is_ok()
        );
        assert!(
            check_org_rate_limit(&db, &org_id, PublicRateLimitMode::Observe, "test")
                .await
                .is_ok(),
            "observe mode should log but allow over-limit requests"
        );
    }

    #[tokio::test]
    async fn public_org_rps_enforce_blocks() {
        let db = memory_db().await;
        let org_id = Uuid::new_v4();
        STATE.features.insert(org_id, 1);

        assert!(
            check_org_rate_limit(&db, &org_id, PublicRateLimitMode::Enforce, "test")
                .await
                .is_ok()
        );
        let err = check_org_rate_limit(&db, &org_id, PublicRateLimitMode::Enforce, "test")
            .await
            .expect_err("enforce mode should block over-limit requests");
        assert_eq!(err.message, "Rate limit exceeded");
        assert!(err.retry_after_secs >= 1);
    }

    #[tokio::test]
    async fn public_org_inflight_helper_blocks_and_releases() {
        let db = memory_db().await;
        let org_id = Uuid::new_v4();
        STATE.features.insert(org_id, 1);
        let conf = hot::val!({
            "api": {
                "public-org-inflight-limit": 1,
            },
        });

        let guard = check_public_org_inflight(&db, &conf, &org_id, "test")
            .await
            .expect("first acquire should not error")
            .expect("limited org should return a guard");
        assert!(
            check_public_org_inflight(&db, &conf, &org_id, "test")
                .await
                .is_err(),
            "second acquire should hit the configured in-flight cap"
        );
        drop(guard);
        assert!(
            check_public_org_inflight(&db, &conf, &org_id, "test")
                .await
                .expect("acquire after drop should not error")
                .is_some()
        );
    }
}

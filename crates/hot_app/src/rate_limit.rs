//! Identity-keyed rate limiting for the auth surface (sliding window,
//! in-memory).
//!
//! ## Design
//!
//! hot_app sits behind CloudFront → ALB → Nginx, so the client IP visible to
//! the app is not trustworthy without fragile proxy-hop configuration.
//! Per-IP limiting is therefore delegated to the edge (WAF / Nginx
//! `limit_req`); the app enforces limits on what it *can* trust:
//!
//! - **per-email** limits on signin and resend-verification (the email being
//!   attacked is a stable key regardless of source IP)
//! - a **global cap** on signups, as a backstop against mass account
//!   creation that the edge limits miss
//!
//! State is in-memory per process. Multiple app instances multiply the
//! effective limits by the instance count, which is acceptable for these
//! backstop limits.

use ahash::AHashMap;
use hot::val::Val;
use std::collections::{VecDeque, hash_map::DefaultHasher};
use std::hash::{Hash, Hasher};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use uuid::Uuid;

/// Signin: max failed-or-not attempts per email per window.
const SIGNIN_MAX_PER_EMAIL: usize = 10;
const SIGNIN_WINDOW: Duration = Duration::from_secs(15 * 60);

/// Resend-verification: max sends per email per window (the per-row
/// `attempts` cap of 5 still applies on top of this).
const RESEND_MAX_PER_EMAIL: usize = 3;
const RESEND_WINDOW: Duration = Duration::from_secs(60 * 60);

/// Signup: global cap across all emails — a backstop, not a per-user limit.
const SIGNUP_MAX_GLOBAL: usize = 300;
const SIGNUP_WINDOW: Duration = Duration::from_secs(60 * 60);

/// Signup: repeated new-account attempts for the same email.
const SIGNUP_MAX_PER_EMAIL: usize = 30;

/// OAuth callback: coarse backstop before outbound token exchange.
const OAUTH_CALLBACK_MAX_GLOBAL: usize = 600;
const OAUTH_CALLBACK_WINDOW: Duration = Duration::from_secs(10 * 60);

/// OAuth: repeated new-account attempts for the same provider identity/email.
const OAUTH_NEW_IDENTITY_MAX: usize = 20;
const OAUTH_NEW_IDENTITY_WINDOW: Duration = Duration::from_secs(60 * 60);

/// Authenticated onboarding: repeated org creation attempts by one user.
const CLAIM_HANDLE_MAX_PER_USER: usize = 30;
const CLAIM_HANDLE_WINDOW: Duration = Duration::from_secs(15 * 60);

/// Billing checkout: repeated checkout session creation for the same plan.
const CHECKOUT_MAX_PER_ORG_PLAN: usize = 20;
const CHECKOUT_WINDOW: Duration = Duration::from_secs(15 * 60);

/// How often to sweep stale keys from the map.
const SWEEP_INTERVAL: Duration = Duration::from_secs(300);

/// Bound the in-memory map so identity rotation cannot grow it forever.
const MAX_KEYS: usize = 10_000;

struct SlidingWindow {
    windows: Mutex<AHashMap<String, VecDeque<Instant>>>,
    last_sweep: Mutex<Instant>,
}

impl SlidingWindow {
    fn new() -> Self {
        Self {
            windows: Mutex::new(AHashMap::new()),
            last_sweep: Mutex::new(Instant::now()),
        }
    }

    /// Record an attempt under `key` and check it against `max` per `window`.
    /// Returns `Ok(())` if allowed, `Err(retry_after_secs)` if limited.
    fn check(&self, key: &str, max: usize, window: Duration, max_keys: usize) -> Result<(), u64> {
        let mut windows = self.windows.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();

        // Periodic sweep so abandoned keys don't accumulate forever. A key
        // is stale once its newest entry is older than the largest window
        // we use anywhere.
        {
            let mut last_sweep = self.last_sweep.lock().unwrap_or_else(|e| e.into_inner());
            if now.duration_since(*last_sweep) >= SWEEP_INTERVAL {
                let stale_cutoff = now
                    - SIGNUP_WINDOW
                        .max(SIGNIN_WINDOW)
                        .max(RESEND_WINDOW)
                        .max(OAUTH_CALLBACK_WINDOW)
                        .max(OAUTH_NEW_IDENTITY_WINDOW)
                        .max(CLAIM_HANDLE_WINDOW)
                        .max(CHECKOUT_WINDOW);
                windows.retain(|_, deque| deque.back().is_some_and(|t| *t >= stale_cutoff));
                *last_sweep = now;
            }
        }

        if !windows.contains_key(key)
            && windows.len() >= max_keys.max(1)
            && let Some(oldest_key) = windows
                .iter()
                .min_by_key(|(_, deque)| deque.back().copied().unwrap_or(now))
                .map(|(key, _)| key.clone())
        {
            windows.remove(&oldest_key);
        }

        let deque = windows.entry(key.to_string()).or_default();
        let cutoff = now - window;
        while deque.front().is_some_and(|t| *t < cutoff) {
            deque.pop_front();
        }

        if deque.len() >= max {
            let oldest = deque.front().unwrap();
            let retry_after = window
                .saturating_sub(now.duration_since(*oldest))
                .as_secs()
                .max(1);
            return Err(retry_after);
        }

        deque.push_back(now);
        Ok(())
    }
}

static LIMITER: once_cell::sync::Lazy<SlidingWindow> =
    once_cell::sync::Lazy::new(SlidingWindow::new);

#[derive(Debug, Clone, Copy)]
struct Limit {
    max: usize,
    window: Duration,
}

impl Limit {
    fn from_conf(
        conf: &Val,
        max_path: &str,
        default_max: usize,
        window_path: &str,
        default_window: Duration,
    ) -> Option<Self> {
        let configured_max = conf.get_int_or_default(max_path, default_max as i64);
        if configured_max <= 0 {
            return None;
        }

        let configured_window_secs =
            conf.get_int_or_default(window_path, default_window.as_secs() as i64);

        Some(Self {
            max: configured_max as usize,
            window: Duration::from_secs(configured_window_secs.max(1) as u64),
        })
    }
}

fn max_keys(conf: &Val) -> usize {
    conf.get_int_or_default("app.abuse-limits.max-keys", MAX_KEYS as i64)
        .max(1) as usize
}

fn email_key(prefix: &str, email: &str) -> String {
    format!("{}:{}", prefix, email.trim().to_lowercase())
}

fn check_key(conf: &Val, key: String, limit: Option<Limit>) -> Result<(), u64> {
    if let Some(limit) = limit {
        LIMITER.check(&key, limit.max, limit.window, max_keys(conf))
    } else {
        Ok(())
    }
}

/// Short stable fingerprint for limiter logging. Do not log raw emails,
/// OAuth IDs, or other user-controlled/semi-secret keys.
pub fn key_fingerprint(value: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    value.hash(&mut hasher);
    hasher.finish()
}

/// Record a signin attempt for `email`. `Err(retry_after_secs)` when the
/// email has seen too many recent attempts.
pub fn check_signin(conf: &Val, email: &str) -> Result<(), u64> {
    check_key(
        conf,
        email_key("signin", email),
        Limit::from_conf(
            conf,
            "app.abuse-limits.signin.max",
            SIGNIN_MAX_PER_EMAIL,
            "app.abuse-limits.signin.window-secs",
            SIGNIN_WINDOW,
        ),
    )
}

/// Record a resend-verification request for `email`.
pub fn check_resend(conf: &Val, email: &str) -> Result<(), u64> {
    check_key(
        conf,
        email_key("resend", email),
        Limit::from_conf(
            conf,
            "app.abuse-limits.resend.max",
            RESEND_MAX_PER_EMAIL,
            "app.abuse-limits.resend.window-secs",
            RESEND_WINDOW,
        ),
    )
}

/// Record a signup attempt against the global cap.
pub fn check_signup_global(conf: &Val) -> Result<(), u64> {
    check_key(
        conf,
        "signup:global".to_string(),
        Limit::from_conf(
            conf,
            "app.abuse-limits.signup-global.max",
            SIGNUP_MAX_GLOBAL,
            "app.abuse-limits.signup-global.window-secs",
            SIGNUP_WINDOW,
        ),
    )
}

/// Record a plausible new signup attempt for one email.
pub fn check_signup_email(conf: &Val, email: &str) -> Result<(), u64> {
    check_key(
        conf,
        email_key("signup-email", email),
        Limit::from_conf(
            conf,
            "app.abuse-limits.signup-email.max",
            SIGNUP_MAX_PER_EMAIL,
            "app.abuse-limits.signup-email.window-secs",
            SIGNUP_WINDOW,
        ),
    )
}

/// Coarse OAuth callback backstop before provider token exchange.
pub fn check_oauth_callback_global(conf: &Val, provider: &str) -> Result<(), u64> {
    check_key(
        conf,
        format!("oauth-callback:{}", provider),
        Limit::from_conf(
            conf,
            "app.abuse-limits.oauth-callback.max",
            OAUTH_CALLBACK_MAX_GLOBAL,
            "app.abuse-limits.oauth-callback.window-secs",
            OAUTH_CALLBACK_WINDOW,
        ),
    )
}

/// Record a new-account OAuth attempt for the provider identity and email.
pub fn check_oauth_new_identity(
    conf: &Val,
    provider: &str,
    provider_user_id: &str,
    email: &str,
) -> Result<(), u64> {
    let limit = Limit::from_conf(
        conf,
        "app.abuse-limits.oauth-new-identity.max",
        OAUTH_NEW_IDENTITY_MAX,
        "app.abuse-limits.oauth-new-identity.window-secs",
        OAUTH_NEW_IDENTITY_WINDOW,
    );
    check_key(
        conf,
        format!("oauth-new-id:{}:{}", provider, provider_user_id),
        limit,
    )?;
    check_key(conf, email_key("oauth-new-email", email), limit)
}

/// Record repeated handle/org creation attempts by one signed-in user.
pub fn check_claim_handle(conf: &Val, user_id: &Uuid) -> Result<(), u64> {
    check_key(
        conf,
        format!("claim-handle:{}", user_id),
        Limit::from_conf(
            conf,
            "app.abuse-limits.claim-handle.max",
            CLAIM_HANDLE_MAX_PER_USER,
            "app.abuse-limits.claim-handle.window-secs",
            CLAIM_HANDLE_WINDOW,
        ),
    )
}

/// Record repeated checkout session creation for one user/org/plan.
pub fn check_checkout(conf: &Val, user_id: &Uuid, org_id: &Uuid, plan_id: &str) -> Result<(), u64> {
    check_key(
        conf,
        format!("checkout:{}:{}:{}", user_id, org_id, plan_id),
        Limit::from_conf(
            conf,
            "app.abuse-limits.checkout.max",
            CHECKOUT_MAX_PER_ORG_PLAN,
            "app.abuse-limits.checkout.window-secs",
            CHECKOUT_WINDOW,
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_within_limit_and_rejects_over() {
        let limiter = SlidingWindow::new();
        for _ in 0..3 {
            assert!(
                limiter
                    .check("k", 3, Duration::from_secs(60), MAX_KEYS)
                    .is_ok()
            );
        }
        let err = limiter.check("k", 3, Duration::from_secs(60), MAX_KEYS);
        assert!(err.is_err());
        assert!(err.unwrap_err() >= 1);
    }

    #[test]
    fn keys_are_independent() {
        let limiter = SlidingWindow::new();
        for _ in 0..2 {
            assert!(
                limiter
                    .check("a", 2, Duration::from_secs(60), MAX_KEYS)
                    .is_ok()
            );
        }
        assert!(
            limiter
                .check("a", 2, Duration::from_secs(60), MAX_KEYS)
                .is_err()
        );
        assert!(
            limiter
                .check("b", 2, Duration::from_secs(60), MAX_KEYS)
                .is_ok()
        );
    }

    #[test]
    fn window_expiry_frees_capacity() {
        let limiter = SlidingWindow::new();
        assert!(
            limiter
                .check("k", 1, Duration::from_millis(20), MAX_KEYS)
                .is_ok()
        );
        assert!(
            limiter
                .check("k", 1, Duration::from_millis(20), MAX_KEYS)
                .is_err()
        );
        std::thread::sleep(Duration::from_millis(30));
        assert!(
            limiter
                .check("k", 1, Duration::from_millis(20), MAX_KEYS)
                .is_ok()
        );
    }

    #[test]
    fn email_keys_are_case_insensitive() {
        assert_eq!(
            email_key("signin", "Alice@Example.COM"),
            email_key("signin", "alice@example.com")
        );
    }

    #[test]
    fn max_keys_evicts_oldest_key() {
        let limiter = SlidingWindow::new();
        assert!(limiter.check("old", 10, Duration::from_secs(60), 1).is_ok());
        std::thread::sleep(Duration::from_millis(2));
        assert!(limiter.check("new", 10, Duration::from_secs(60), 1).is_ok());
        let windows = limiter.windows.lock().unwrap();
        assert!(windows.contains_key("new"));
        assert!(!windows.contains_key("old"));
    }

    #[test]
    fn disabled_limit_allows_requests() {
        let conf = hot::val!({"app": {"abuse-limits": {"signin": {"max": 0}}}});
        for _ in 0..20 {
            assert!(check_signin(&conf, "disabled@example.com").is_ok());
        }
    }

    #[test]
    fn configured_signup_email_limit_blocks() {
        let conf = hot::val!({
            "app": {
                "abuse-limits": {
                    "signup-email": {
                        "max": 1,
                        "window-secs": 60,
                    },
                },
            },
        });
        let email = format!("{}@example.com", Uuid::now_v7());
        assert!(check_signup_email(&conf, &email).is_ok());
        assert!(check_signup_email(&conf, &email).is_err());
    }

    #[test]
    fn oauth_new_identity_limit_blocks_new_account_bucket() {
        let conf = hot::val!({
            "app": {
                "abuse-limits": {
                    "oauth-new-identity": {
                        "max": 1,
                        "window-secs": 60,
                    },
                },
            },
        });
        let provider_user_id = Uuid::now_v7().to_string();
        let email = format!("{}@example.com", Uuid::now_v7());
        assert!(check_oauth_new_identity(&conf, "github", &provider_user_id, &email).is_ok());
        assert!(check_oauth_new_identity(&conf, "github", &provider_user_id, &email).is_err());
    }

    #[test]
    fn claim_handle_and_checkout_limits_block_when_configured_low() {
        let conf = hot::val!({
            "app": {
                "abuse-limits": {
                    "claim-handle": {
                        "max": 1,
                        "window-secs": 60,
                    },
                    "checkout": {
                        "max": 1,
                        "window-secs": 60,
                    },
                },
            },
        });
        let user_id = Uuid::now_v7();
        let org_id = Uuid::now_v7();

        assert!(check_claim_handle(&conf, &user_id).is_ok());
        assert!(check_claim_handle(&conf, &user_id).is_err());

        assert!(check_checkout(&conf, &user_id, &org_id, "hot-free").is_ok());
        assert!(check_checkout(&conf, &user_id, &org_id, "hot-free").is_err());
    }
}

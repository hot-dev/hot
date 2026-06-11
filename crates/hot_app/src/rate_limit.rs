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
use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{Duration, Instant};

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

/// How often to sweep stale keys from the map.
const SWEEP_INTERVAL: Duration = Duration::from_secs(300);

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
    fn check(&self, key: &str, max: usize, window: Duration) -> Result<(), u64> {
        let mut windows = self.windows.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();

        // Periodic sweep so abandoned keys don't accumulate forever. A key
        // is stale once its newest entry is older than the largest window
        // we use anywhere.
        {
            let mut last_sweep = self.last_sweep.lock().unwrap_or_else(|e| e.into_inner());
            if now.duration_since(*last_sweep) >= SWEEP_INTERVAL {
                let stale_cutoff = now - SIGNUP_WINDOW.max(SIGNIN_WINDOW).max(RESEND_WINDOW);
                windows.retain(|_, deque| deque.back().is_some_and(|t| *t >= stale_cutoff));
                *last_sweep = now;
            }
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

fn email_key(prefix: &str, email: &str) -> String {
    format!("{}:{}", prefix, email.trim().to_lowercase())
}

/// Record a signin attempt for `email`. `Err(retry_after_secs)` when the
/// email has seen too many recent attempts.
pub fn check_signin(email: &str) -> Result<(), u64> {
    LIMITER.check(
        &email_key("signin", email),
        SIGNIN_MAX_PER_EMAIL,
        SIGNIN_WINDOW,
    )
}

/// Record a resend-verification request for `email`.
pub fn check_resend(email: &str) -> Result<(), u64> {
    LIMITER.check(
        &email_key("resend", email),
        RESEND_MAX_PER_EMAIL,
        RESEND_WINDOW,
    )
}

/// Record a signup attempt against the global cap.
pub fn check_signup_global() -> Result<(), u64> {
    LIMITER.check("signup:global", SIGNUP_MAX_GLOBAL, SIGNUP_WINDOW)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_within_limit_and_rejects_over() {
        let limiter = SlidingWindow::new();
        for _ in 0..3 {
            assert!(limiter.check("k", 3, Duration::from_secs(60)).is_ok());
        }
        let err = limiter.check("k", 3, Duration::from_secs(60));
        assert!(err.is_err());
        assert!(err.unwrap_err() >= 1);
    }

    #[test]
    fn keys_are_independent() {
        let limiter = SlidingWindow::new();
        for _ in 0..2 {
            assert!(limiter.check("a", 2, Duration::from_secs(60)).is_ok());
        }
        assert!(limiter.check("a", 2, Duration::from_secs(60)).is_err());
        assert!(limiter.check("b", 2, Duration::from_secs(60)).is_ok());
    }

    #[test]
    fn window_expiry_frees_capacity() {
        let limiter = SlidingWindow::new();
        assert!(limiter.check("k", 1, Duration::from_millis(20)).is_ok());
        assert!(limiter.check("k", 1, Duration::from_millis(20)).is_err());
        std::thread::sleep(Duration::from_millis(30));
        assert!(limiter.check("k", 1, Duration::from_millis(20)).is_ok());
    }

    #[test]
    fn email_keys_are_case_insensitive() {
        assert_eq!(
            email_key("signin", "Alice@Example.COM"),
            email_key("signin", "alice@example.com")
        );
    }
}

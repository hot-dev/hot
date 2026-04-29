/// Environment configuration helpers
use std::env;

/// Get the current environment
pub fn get_env() -> String {
    env::var("HOT_ENV").unwrap_or_else(|_| "development".to_string())
}

/// Check if we're running in local development mode
pub fn is_local_dev() -> bool {
    get_env().to_lowercase() == "development"
}

/// Get the Hot App URL based on HOT_ENV or explicit HOT_APP_URL
pub fn get_app_url() -> String {
    // First check for explicit HOT_APP_URL
    if let Ok(url) = env::var("HOT_APP_URL") {
        return url;
    }

    // Fall back to HOT_ENV-based defaults
    match get_env().to_lowercase().as_str() {
        "production" => "https://app.hot.dev".to_string(),
        "staging" => "https://app.hot-stg.dev".to_string(),
        _ => "http://localhost:4680".to_string(), // development
    }
}

/// Get the Hot API URL based on HOT_ENV or explicit HOT_API_URL
pub fn get_api_url() -> String {
    // First check for explicit HOT_API_URL
    if let Ok(url) = env::var("HOT_API_URL") {
        return url;
    }

    // Fall back to HOT_ENV-based defaults
    match get_env().to_lowercase().as_str() {
        "production" => "https://api.hot.dev".to_string(),
        "staging" => "https://api.hot-stg.dev".to_string(),
        _ => "http://localhost:4681".to_string(), // development
    }
}

/// Get the Hot Web URL based on HOT_ENV or explicit HOT_WEB_URL
pub fn get_web_url() -> String {
    // First check for explicit HOT_WEB_URL
    if let Ok(url) = env::var("HOT_WEB_URL") {
        return url;
    }

    // Fall back to HOT_ENV-based defaults
    match get_env().to_lowercase().as_str() {
        "production" => "https://hot.dev".to_string(),
        "staging" => "https://hot-stg.dev".to_string(),
        _ => "http://localhost:8080".to_string(), // development
    }
}

/// Get the cookie domain for cross-subdomain cookie sharing
/// Returns None for localhost (which doesn't support subdomain cookies)
pub fn get_cookie_domain() -> Option<String> {
    // First check for explicit HOT_COOKIE_DOMAIN
    if let Ok(domain) = env::var("HOT_COOKIE_DOMAIN") {
        return Some(domain);
    }

    // Fall back to HOT_ENV-based defaults
    match get_env().to_lowercase().as_str() {
        "production" => Some(".hot.dev".to_string()),
        "staging" => Some(".hot-stg.dev".to_string()),
        _ => None, // localhost doesn't support subdomain cookies
    }
}

// =============================================================================
// Retry Configuration
// =============================================================================

/// Default values for retry configuration
pub mod retry {
    use crate::val::Val;
    use std::env;

    /// Maximum number of retry attempts allowed (hard limit)
    /// Default: 10, can be overridden via hot.retry.max-attempts or HOT_RETRY_MAX_ATTEMPTS
    pub const DEFAULT_MAX_RETRIES: i16 = 10;

    /// Maximum retry delay in milliseconds (hard limit)
    /// Default: 3600000 (1 hour), can be overridden via hot.retry.max-delay-ms or HOT_RETRY_MAX_DELAY_MS
    pub const DEFAULT_MAX_DELAY_MS: i32 = 3_600_000;

    /// Minimum retry delay in milliseconds
    /// Default: 100ms
    pub const MIN_DELAY_MS: i32 = 100;

    /// Default retry delay when not specified in meta
    /// Default: 1000ms (1 second), can be overridden via hot.retry.default-delay-ms or HOT_RETRY_DEFAULT_DELAY_MS
    pub const DEFAULT_DELAY_MS: i32 = 1_000;

    /// Default max delay for backoff strategies (5 minutes)
    pub const DEFAULT_BACKOFF_MAX_DELAY_MS: i32 = 300_000;

    /// Backoff strategy for retry delays
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
    #[repr(i16)]
    pub enum BackoffStrategy {
        /// Fixed delay between retries (default, current behavior)
        #[default]
        Fixed = 0,
        /// Exponential backoff: delay * 2^attempt
        Exponential = 1,
        /// Linear backoff: delay * (attempt + 1)
        Linear = 2,
    }

    impl BackoffStrategy {
        /// Convert from database smallint value
        pub fn from_i16(value: i16) -> Self {
            match value {
                1 => BackoffStrategy::Exponential,
                2 => BackoffStrategy::Linear,
                _ => BackoffStrategy::Fixed,
            }
        }

        /// Convert to database smallint value
        pub fn as_i16(&self) -> i16 {
            *self as i16
        }

        /// Parse from string (for meta parsing)
        pub fn parse(s: &str) -> Self {
            match s.to_lowercase().as_str() {
                "exponential" => BackoffStrategy::Exponential,
                "linear" => BackoffStrategy::Linear,
                _ => BackoffStrategy::Fixed,
            }
        }
    }

    /// Calculate the actual delay for a retry attempt based on backoff strategy
    ///
    /// # Arguments
    /// * `base_delay_ms` - Base delay in milliseconds
    /// * `attempt` - Current retry attempt (0-indexed: 0 = first retry)
    /// * `strategy` - Backoff strategy to use
    /// * `max_delay_ms` - Maximum delay cap
    /// * `jitter` - Whether to add random jitter (±10%)
    ///
    /// # Returns
    /// The calculated delay in milliseconds
    pub fn calculate_retry_delay(
        base_delay_ms: i32,
        attempt: i16,
        strategy: BackoffStrategy,
        max_delay_ms: i32,
        jitter: bool,
    ) -> i64 {
        let attempt = attempt.max(0) as u32;

        let calculated = match strategy {
            BackoffStrategy::Fixed => base_delay_ms as i64,
            BackoffStrategy::Exponential => {
                // delay * 2^attempt, with overflow protection
                let multiplier = 2_i64.saturating_pow(attempt);
                (base_delay_ms as i64).saturating_mul(multiplier)
            }
            BackoffStrategy::Linear => {
                // delay * (attempt + 1)
                (base_delay_ms as i64).saturating_mul(attempt as i64 + 1)
            }
        };

        // Apply max delay cap
        let capped = calculated.min(max_delay_ms as i64);

        // Apply jitter if enabled (±10%)
        if jitter && capped > 0 {
            let jitter_range = (capped as f64 * 0.1) as i64;
            if jitter_range > 0 {
                // Simple pseudo-random jitter using current time
                let now_nanos = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos() as i64)
                    .unwrap_or(0);
                let jitter_offset = (now_nanos % (jitter_range * 2 + 1)) - jitter_range;
                return (capped + jitter_offset).max(MIN_DELAY_MS as i64);
            }
        }

        capped.max(MIN_DELAY_MS as i64)
    }

    /// Get the maximum allowed retry attempts from environment or default
    pub fn get_max_retries_limit() -> i16 {
        env::var("HOT_RETRY_MAX_ATTEMPTS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_MAX_RETRIES)
    }

    /// Get the maximum allowed retry attempts from conf or environment
    pub fn get_max_retries_limit_from_conf(conf: &Val) -> i16 {
        // Try conf value first: hot.retry.max-attempts -> retry.max-attempts after load
        if let Some(retry_conf) = conf.get("retry") {
            let val = retry_conf.get_int("max-attempts");
            if val > 0 {
                return val.clamp(1, i16::MAX as i64) as i16;
            }
        }
        // Fall back to environment variable
        get_max_retries_limit()
    }

    /// Get the maximum allowed delay in ms from environment or default
    pub fn get_max_delay_limit() -> i32 {
        env::var("HOT_RETRY_MAX_DELAY_MS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_MAX_DELAY_MS)
    }

    /// Get the maximum allowed delay in ms from conf or environment
    pub fn get_max_delay_limit_from_conf(conf: &Val) -> i32 {
        // Try conf value first: hot.retry.max-delay-ms -> retry.max-delay-ms after load
        if let Some(retry_conf) = conf.get("retry") {
            let val = retry_conf.get_int("max-delay-ms");
            if val > 0 {
                return val.clamp(MIN_DELAY_MS as i64, i32::MAX as i64) as i32;
            }
        }
        // Fall back to environment variable
        get_max_delay_limit()
    }

    /// Get the default delay in ms from conf or environment
    pub fn get_default_delay_from_conf(conf: &Val) -> i32 {
        // Try conf value first: hot.retry.default-delay-ms -> retry.default-delay-ms after load
        if let Some(retry_conf) = conf.get("retry") {
            let val = retry_conf.get_int("default-delay-ms");
            if val > 0 {
                return val.clamp(MIN_DELAY_MS as i64, i32::MAX as i64) as i32;
            }
        }
        // Fall back to environment variable
        env::var("HOT_RETRY_DEFAULT_DELAY_MS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_DELAY_MS)
    }

    /// Clamp retry count to allowed range [0, max_limit]
    pub fn clamp_retries(retries: i64) -> i16 {
        let max = get_max_retries_limit();
        retries.clamp(0, max as i64) as i16
    }

    /// Clamp retry count to allowed range using conf limits
    pub fn clamp_retries_with_conf(retries: i64, conf: &Val) -> i16 {
        let max = get_max_retries_limit_from_conf(conf);
        retries.clamp(0, max as i64) as i16
    }

    /// Clamp retry delay to allowed range [MIN_DELAY_MS, max_limit]
    pub fn clamp_delay(delay_ms: i64) -> i32 {
        let max = get_max_delay_limit();
        delay_ms.clamp(MIN_DELAY_MS as i64, max as i64) as i32
    }

    /// Clamp retry delay to allowed range using conf limits
    pub fn clamp_delay_with_conf(delay_ms: i64, conf: &Val) -> i32 {
        let max = get_max_delay_limit_from_conf(conf);
        delay_ms.clamp(MIN_DELAY_MS as i64, max as i64) as i32
    }

    /// Configuration extracted from function metadata
    #[derive(Debug, Clone, Default)]
    pub struct RetryConfig {
        /// Maximum retry attempts (0 = no retry)
        pub max_retries: i16,
        /// Base delay between retries in milliseconds
        pub delay_ms: i32,
        /// Backoff strategy for calculating retry delays
        pub backoff: BackoffStrategy,
        /// Maximum delay cap in milliseconds (for exponential/linear backoff)
        pub max_delay_ms: i32,
        /// Whether to add random jitter (±10%) to prevent thundering herd
        pub jitter: bool,
    }

    impl RetryConfig {
        /// Create a new RetryConfig with defaults
        pub fn new() -> Self {
            Self {
                max_retries: 0,
                delay_ms: DEFAULT_DELAY_MS,
                backoff: BackoffStrategy::Fixed,
                max_delay_ms: DEFAULT_BACKOFF_MAX_DELAY_MS,
                jitter: false,
            }
        }

        /// Create a new RetryConfig with defaults from conf
        pub fn new_with_conf(conf: &Val) -> Self {
            Self {
                max_retries: 0,
                delay_ms: get_default_delay_from_conf(conf),
                backoff: BackoffStrategy::Fixed,
                max_delay_ms: DEFAULT_BACKOFF_MAX_DELAY_MS,
                jitter: false,
            }
        }

        /// Create a RetryConfig from function metadata JSON
        /// Supports two formats:
        /// - Simple: `"retry": 3` → 3 attempts with default delay
        /// - Full: `"retry": { "attempts": 3, "delay": 1000, "backoff": "exponential", "max_delay": 300000, "jitter": true }`
        pub fn from_meta(meta: Option<&serde_json::Value>) -> Self {
            let mut config = Self::new();

            if let Some(meta_obj) = meta.and_then(|m| m.as_object())
                && let Some(retry_val) = meta_obj.get("retry")
            {
                if let Some(n) = retry_val.as_i64() {
                    // Simple format: "retry": 3
                    config.max_retries = clamp_retries(n);
                } else if let Some(retry_obj) = retry_val.as_object() {
                    // Full format: "retry": { "attempts": 3, "delay": 1000, ... }
                    if let Some(attempts) = retry_obj.get("attempts").and_then(|v| v.as_i64()) {
                        config.max_retries = clamp_retries(attempts);
                    }
                    if let Some(delay) = retry_obj.get("delay").and_then(|v| v.as_i64()) {
                        config.delay_ms = clamp_delay(delay);
                    }
                    // Parse backoff strategy
                    if let Some(backoff_str) = retry_obj.get("backoff").and_then(|v| v.as_str()) {
                        config.backoff = BackoffStrategy::parse(backoff_str);
                    }
                    // Parse max_delay (for backoff cap)
                    if let Some(max_delay) = retry_obj.get("max_delay").and_then(|v| v.as_i64()) {
                        config.max_delay_ms = clamp_delay(max_delay);
                    }
                    // Parse jitter flag
                    if let Some(jitter) = retry_obj.get("jitter").and_then(|v| v.as_bool()) {
                        config.jitter = jitter;
                    }
                }
            }

            config
        }

        /// Create a RetryConfig from function metadata JSON using conf for limits
        /// Supports two formats:
        /// - Simple: `"retry": 3` → 3 attempts with default delay
        /// - Full: `"retry": { "attempts": 3, "delay": 1000, "backoff": "exponential", "max_delay": 300000, "jitter": true }`
        pub fn from_meta_with_conf(meta: Option<&serde_json::Value>, conf: &Val) -> Self {
            let mut config = Self::new_with_conf(conf);

            if let Some(meta_obj) = meta.and_then(|m| m.as_object())
                && let Some(retry_val) = meta_obj.get("retry")
            {
                if let Some(n) = retry_val.as_i64() {
                    // Simple format: "retry": 3
                    config.max_retries = clamp_retries_with_conf(n, conf);
                } else if let Some(retry_obj) = retry_val.as_object() {
                    // Full format: "retry": { "attempts": 3, "delay": 1000, ... }
                    if let Some(attempts) = retry_obj.get("attempts").and_then(|v| v.as_i64()) {
                        config.max_retries = clamp_retries_with_conf(attempts, conf);
                    }
                    if let Some(delay) = retry_obj.get("delay").and_then(|v| v.as_i64()) {
                        config.delay_ms = clamp_delay_with_conf(delay, conf);
                    }
                    // Parse backoff strategy
                    if let Some(backoff_str) = retry_obj.get("backoff").and_then(|v| v.as_str()) {
                        config.backoff = BackoffStrategy::parse(backoff_str);
                    }
                    // Parse max_delay (for backoff cap)
                    if let Some(max_delay) = retry_obj.get("max_delay").and_then(|v| v.as_i64()) {
                        config.max_delay_ms = clamp_delay_with_conf(max_delay, conf);
                    }
                    // Parse jitter flag
                    if let Some(jitter) = retry_obj.get("jitter").and_then(|v| v.as_bool()) {
                        config.jitter = jitter;
                    }
                }
            }

            config
        }

        /// Returns true if retries are enabled
        pub fn is_enabled(&self) -> bool {
            self.max_retries > 0
        }

        /// Calculate the delay for a specific retry attempt
        pub fn delay_for_attempt(&self, attempt: i16) -> i64 {
            calculate_retry_delay(
                self.delay_ms,
                attempt,
                self.backoff,
                self.max_delay_ms,
                self.jitter,
            )
        }
    }

    /// Retry context from event data (for manual retries via hot:call)
    /// This is passed in the event data under the "retry" key
    #[derive(Debug, Clone, Default)]
    pub struct RetryContext {
        /// The original run ID that this is a retry of
        pub origin_run_id: Option<uuid::Uuid>,
        /// Current retry attempt (1 = first retry, 2 = second retry, etc.)
        pub attempt: i16,
        /// Maximum retry attempts
        pub max_retries: i16,
        /// Base delay between retries in milliseconds
        pub delay_ms: i32,
        /// Backoff strategy
        pub backoff: BackoffStrategy,
        /// Maximum delay cap in milliseconds
        pub max_delay_ms: i32,
        /// Whether to add jitter
        pub jitter: bool,
    }

    impl RetryContext {
        /// Extract retry context from event data's "retry" key
        /// Expected format: {"origin-run-id": "uuid", "attempt": N, "max-retries": N, "delay": N, "backoff": "exponential", ...}
        pub fn from_event_data(
            event_data: &crate::val::Val,
            conf: &crate::val::Val,
        ) -> Option<Self> {
            use crate::val::Val;

            // Get the "retry" key from event data
            let retry_val = event_data.get("retry")?;
            if matches!(retry_val, Val::Null) {
                return None;
            }

            let origin_run_id = retry_val.get("origin-run-id").and_then(|v| match v {
                Val::Str(s) => uuid::Uuid::parse_str(&s).ok(),
                _ => None,
            });

            let attempt = retry_val
                .get("attempt")
                .and_then(|v| match v {
                    Val::Int(n) => Some(n.clamp(0, i16::MAX as i64) as i16),
                    _ => None,
                })
                .unwrap_or(1);

            let max_retries = retry_val
                .get("max-retries")
                .and_then(|v| match v {
                    Val::Int(n) => Some(clamp_retries_with_conf(n, conf)),
                    _ => None,
                })
                .unwrap_or(0);

            let delay_ms = retry_val
                .get("delay")
                .and_then(|v| match v {
                    Val::Int(n) => Some(clamp_delay_with_conf(n, conf)),
                    _ => None,
                })
                .unwrap_or_else(|| get_default_delay_from_conf(conf));

            let backoff = retry_val
                .get("backoff")
                .and_then(|v| match v {
                    Val::Str(s) => Some(BackoffStrategy::parse(&s)),
                    _ => None,
                })
                .unwrap_or(BackoffStrategy::Fixed);

            let max_delay_ms = retry_val
                .get("max-delay")
                .and_then(|v| match v {
                    Val::Int(n) => Some(clamp_delay_with_conf(n, conf)),
                    _ => None,
                })
                .unwrap_or(DEFAULT_BACKOFF_MAX_DELAY_MS);

            let jitter = retry_val
                .get("jitter")
                .and_then(|v| match v {
                    Val::Bool(b) => Some(b),
                    _ => None,
                })
                .unwrap_or(false);

            Some(Self {
                origin_run_id,
                attempt,
                max_retries,
                delay_ms,
                backoff,
                max_delay_ms,
                jitter,
            })
        }

        /// Returns true if this represents a valid retry (has origin_run_id)
        pub fn is_valid(&self) -> bool {
            self.origin_run_id.is_some()
        }

        /// Calculate the delay for the current attempt
        pub fn delay_for_current_attempt(&self) -> i64 {
            calculate_retry_delay(
                self.delay_ms,
                self.attempt.saturating_sub(1), // attempt is 1-indexed, calculation expects 0-indexed
                self.backoff,
                self.max_delay_ms,
                self.jitter,
            )
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn test_fixed_backoff() {
            assert_eq!(
                calculate_retry_delay(1000, 0, BackoffStrategy::Fixed, 300000, false),
                1000
            );
            assert_eq!(
                calculate_retry_delay(1000, 1, BackoffStrategy::Fixed, 300000, false),
                1000
            );
            assert_eq!(
                calculate_retry_delay(1000, 5, BackoffStrategy::Fixed, 300000, false),
                1000
            );
        }

        #[test]
        fn test_exponential_backoff() {
            // delay * 2^attempt
            assert_eq!(
                calculate_retry_delay(1000, 0, BackoffStrategy::Exponential, 300000, false),
                1000
            ); // 1000 * 2^0 = 1000
            assert_eq!(
                calculate_retry_delay(1000, 1, BackoffStrategy::Exponential, 300000, false),
                2000
            ); // 1000 * 2^1 = 2000
            assert_eq!(
                calculate_retry_delay(1000, 2, BackoffStrategy::Exponential, 300000, false),
                4000
            ); // 1000 * 2^2 = 4000
            assert_eq!(
                calculate_retry_delay(1000, 3, BackoffStrategy::Exponential, 300000, false),
                8000
            ); // 1000 * 2^3 = 8000
        }

        #[test]
        fn test_linear_backoff() {
            // delay * (attempt + 1)
            assert_eq!(
                calculate_retry_delay(1000, 0, BackoffStrategy::Linear, 300000, false),
                1000
            ); // 1000 * 1 = 1000
            assert_eq!(
                calculate_retry_delay(1000, 1, BackoffStrategy::Linear, 300000, false),
                2000
            ); // 1000 * 2 = 2000
            assert_eq!(
                calculate_retry_delay(1000, 2, BackoffStrategy::Linear, 300000, false),
                3000
            ); // 1000 * 3 = 3000
            assert_eq!(
                calculate_retry_delay(1000, 4, BackoffStrategy::Linear, 300000, false),
                5000
            ); // 1000 * 5 = 5000
        }

        #[test]
        fn test_max_delay_cap() {
            // Should cap at max_delay
            assert_eq!(
                calculate_retry_delay(1000, 10, BackoffStrategy::Exponential, 10000, false),
                10000
            ); // Would be 1024000, capped at 10000
        }

        #[test]
        fn test_min_delay_floor() {
            // Should not go below MIN_DELAY_MS
            assert_eq!(
                calculate_retry_delay(50, 0, BackoffStrategy::Fixed, 300000, false),
                MIN_DELAY_MS as i64
            );
        }

        #[test]
        fn test_jitter_range() {
            // With jitter, result should be within ±10% of calculated value
            let base = 10000;
            let result = calculate_retry_delay(base, 0, BackoffStrategy::Fixed, 300000, true);
            assert!(result >= (base as f64 * 0.9) as i64);
            assert!(result <= (base as f64 * 1.1) as i64);
        }

        #[test]
        fn test_backoff_strategy_from_str() {
            assert_eq!(BackoffStrategy::parse("fixed"), BackoffStrategy::Fixed);
            assert_eq!(
                BackoffStrategy::parse("exponential"),
                BackoffStrategy::Exponential
            );
            assert_eq!(BackoffStrategy::parse("linear"), BackoffStrategy::Linear);
            assert_eq!(
                BackoffStrategy::parse("EXPONENTIAL"),
                BackoffStrategy::Exponential
            );
            assert_eq!(BackoffStrategy::parse("unknown"), BackoffStrategy::Fixed);
        }

        #[test]
        fn test_retry_config_delay_for_attempt() {
            let config = RetryConfig {
                max_retries: 5,
                delay_ms: 1000,
                backoff: BackoffStrategy::Exponential,
                max_delay_ms: 300000,
                jitter: false,
            };

            assert_eq!(config.delay_for_attempt(0), 1000);
            assert_eq!(config.delay_for_attempt(1), 2000);
            assert_eq!(config.delay_for_attempt(2), 4000);
        }
    }
}

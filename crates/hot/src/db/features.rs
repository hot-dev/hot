use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use thiserror::Error;
use uuid::Uuid;

use super::DatabasePool;

#[derive(Error, Debug)]
pub enum FeaturesError {
    #[error("Database error: {0}")]
    Database(#[from] sqlx::Error),
}

/// Well-known feature keys.
/// Using constants avoids typos and enables IDE autocomplete.
///
/// ## Key categories
///
/// **Monthly quotas** (`*_per_month`): accumulated usage compared against these
/// values each billing period. -1 = unlimited, 0 = disabled.
///
/// **Per-container caps** (`box_*`): each container is clamped to these maximums.
/// Resolved via 5-tier hierarchy (default → plan → org → BoxConf → system max).
///
/// **Code task caps** (`task_timeout_secs`): per-task ceiling for non-container tasks.
pub mod keys {
    // Monthly quotas (usage accumulates, resets each billing period)
    pub const RUNS_PER_MONTH: &str = "runs_per_month";
    pub const TASK_MINUTES_PER_MONTH: &str = "task_minutes_per_month";
    pub const COMPUTE_UNITS_PER_MONTH: &str = "compute_units_per_month";

    // Resource limits (i64 values, -1 = unlimited)
    pub const STORAGE_BYTES: &str = "storage_bytes";
    pub const FILE_UPLOAD_MAX_BYTES: &str = "file_upload_max_bytes";
    /// Absolute ceiling for file uploads (50 GB). Plans that set -1 (unlimited)
    /// are clamped to this value — S3 and infrastructure constraints make truly
    /// unlimited uploads impractical.
    pub const MAX_FILE_UPLOAD_BYTES: i64 = 50_i64 * 1024 * 1024 * 1024; // 50 GB
    pub const TEAM_MEMBERS: &str = "team_members";
    pub const CALL_RETENTION_DAYS: &str = "call_retention_days";
    pub const CALL_STORAGE_BYTES: &str = "call_storage_bytes";
    pub const STORE_STORAGE_BYTES: &str = "store_storage_bytes";

    // Rate limit features (i64 values, -1 = unlimited)
    pub const RATE_LIMIT_RPS: &str = "rate_limit_rps";

    // Per-container caps (5-tier resolution via BoxLimits)
    pub const BOX_TMP_SIZE_MB: &str = "box_tmp_size_mb";
    pub const BOX_DISK_SIZE_MB: &str = "box_disk_size_mb";
    pub const BOX_MEMORY_MB: &str = "box_memory_mb";
    pub const BOX_TIMEOUT_SECS: &str = "box_timeout_secs";
    pub const BOX_CPU_QUOTA: &str = "box_cpu_quota";
    pub const BOX_NETWORK: &str = "box_network";

    // Org-wide container quotas (3-tier: cloud defaults → plan → org override)
    pub const BOX_CONCURRENT_TASKS: &str = "box_concurrent_tasks";

    // Org-level spending cap (set in org.features only, not plan features)
    pub const COMPUTE_UNITS_BUDGET: &str = "compute_units_budget";

    // Code task caps
    pub const TASK_TIMEOUT_SECS: &str = "task_timeout_secs";

    // Boolean features (true/false, -1 = unlimited, 0 = disabled)
    pub const CUSTOM_DOMAINS: &str = "custom_domains";
    pub const SERVICE_KEYS: &str = "service_keys";
    pub const ALERTS: &str = "alerts";
    pub const SELF_HOSTED: &str = "self_hosted";
}

/// Resolved feature set for an organization.
///
/// Features are a flat JSON object with string keys and number/bool values.
/// Resolution merges plan defaults with per-org overrides (org wins).
///
/// ## Numeric conventions
/// - `-1` means **unlimited**
/// - `0` means **disabled / none**
/// - Positive values are the actual limit
///
/// ## Example JSON
/// ```json
/// {
///   "runs_per_month": 100000,
///   "storage_bytes": 2147483648,
///   "team_members": 5,
///   "call_retention_days": 30,
///   "call_storage_bytes": 1073741824,
///   "custom_domains": 5
/// }
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Features {
    #[serde(flatten)]
    inner: serde_json::Map<String, JsonValue>,
}

impl Default for Features {
    fn default() -> Self {
        Self::unlimited()
    }
}

impl Features {
    /// Create an empty Features set (all values will return defaults).
    pub fn empty() -> Self {
        Self {
            inner: serde_json::Map::new(),
        }
    }

    /// Create unlimited features (for self-hosted / local dev).
    pub fn unlimited() -> Self {
        let mut inner = serde_json::Map::new();
        inner.insert(
            keys::RUNS_PER_MONTH.to_string(),
            JsonValue::Number((-1).into()),
        );
        inner.insert(
            keys::STORAGE_BYTES.to_string(),
            JsonValue::Number((-1).into()),
        );
        inner.insert(
            keys::FILE_UPLOAD_MAX_BYTES.to_string(),
            JsonValue::Number((-1).into()),
        );
        inner.insert(
            keys::TEAM_MEMBERS.to_string(),
            JsonValue::Number((-1).into()),
        );
        inner.insert(
            keys::CALL_RETENTION_DAYS.to_string(),
            JsonValue::Number((-1).into()),
        );
        inner.insert(
            keys::CALL_STORAGE_BYTES.to_string(),
            JsonValue::Number((-1).into()),
        );
        inner.insert(
            keys::STORE_STORAGE_BYTES.to_string(),
            JsonValue::Number((-1).into()),
        );
        inner.insert(
            keys::RATE_LIMIT_RPS.to_string(),
            JsonValue::Number((-1).into()),
        );
        inner.insert(
            keys::CUSTOM_DOMAINS.to_string(),
            JsonValue::Number((-1).into()),
        );
        inner.insert(
            keys::TASK_MINUTES_PER_MONTH.to_string(),
            JsonValue::Number((-1).into()),
        );
        inner.insert(
            keys::COMPUTE_UNITS_PER_MONTH.to_string(),
            JsonValue::Number((-1).into()),
        );
        inner.insert(
            keys::BOX_TMP_SIZE_MB.to_string(),
            JsonValue::Number((-1).into()),
        );
        inner.insert(
            keys::BOX_DISK_SIZE_MB.to_string(),
            JsonValue::Number((-1).into()),
        );
        inner.insert(
            keys::BOX_MEMORY_MB.to_string(),
            JsonValue::Number((-1).into()),
        );
        inner.insert(
            keys::BOX_TIMEOUT_SECS.to_string(),
            JsonValue::Number((-1).into()),
        );
        inner.insert(
            keys::BOX_CPU_QUOTA.to_string(),
            JsonValue::Number((-1).into()),
        );
        inner.insert(keys::BOX_NETWORK.to_string(), JsonValue::Bool(true));
        inner.insert(
            keys::BOX_CONCURRENT_TASKS.to_string(),
            JsonValue::Number((-1).into()),
        );
        inner.insert(
            keys::TASK_TIMEOUT_SECS.to_string(),
            JsonValue::Number((-1).into()),
        );
        inner.insert(keys::SERVICE_KEYS.to_string(), JsonValue::Bool(true));
        inner.insert(keys::ALERTS.to_string(), JsonValue::Bool(true));
        inner.insert(keys::SELF_HOSTED.to_string(), JsonValue::Bool(true));
        Self { inner }
    }

    /// Construct from a JSON value (expects an object).
    pub fn from_json(value: Option<&JsonValue>) -> Self {
        match value {
            Some(JsonValue::Object(map)) => Self { inner: map.clone() },
            _ => Self::empty(),
        }
    }

    /// Convert to a JSON value for storage.
    pub fn to_json(&self) -> JsonValue {
        JsonValue::Object(self.inner.clone())
    }

    /// Conservative defaults for cloud plans.
    ///
    /// Used as the base layer when a subscription exists so that any feature
    /// key missing from the plan/org JSON gets a safe value instead of
    /// accidentally granting unlimited access.
    ///
    /// - Numeric limits default to 0 (disabled) except `team_members` (1)
    ///   and `rate_limit_rps` (10).
    /// - Boolean features default to false.
    fn cloud_defaults() -> serde_json::Map<String, JsonValue> {
        let mut m = serde_json::Map::new();
        m.insert(
            keys::RUNS_PER_MONTH.to_string(),
            JsonValue::Number(0.into()),
        );
        m.insert(keys::STORAGE_BYTES.to_string(), JsonValue::Number(0.into()));
        m.insert(
            keys::FILE_UPLOAD_MAX_BYTES.to_string(),
            JsonValue::Number(104_857_600.into()), // 100 MB
        );
        m.insert(keys::TEAM_MEMBERS.to_string(), JsonValue::Number(1.into()));
        m.insert(
            keys::CALL_RETENTION_DAYS.to_string(),
            JsonValue::Number(0.into()),
        );
        m.insert(
            keys::CALL_STORAGE_BYTES.to_string(),
            JsonValue::Number(0.into()),
        );
        m.insert(
            keys::STORE_STORAGE_BYTES.to_string(),
            JsonValue::Number(0.into()),
        );
        m.insert(
            keys::RATE_LIMIT_RPS.to_string(),
            JsonValue::Number(10.into()),
        );
        m.insert(
            keys::CUSTOM_DOMAINS.to_string(),
            JsonValue::Number(0.into()),
        );
        m.insert(
            keys::TASK_MINUTES_PER_MONTH.to_string(),
            JsonValue::Number(0.into()),
        );
        m.insert(
            keys::COMPUTE_UNITS_PER_MONTH.to_string(),
            JsonValue::Number(0.into()),
        );
        m.insert(
            keys::BOX_TMP_SIZE_MB.to_string(),
            JsonValue::Number(500.into()),
        );
        m.insert(
            keys::BOX_DISK_SIZE_MB.to_string(),
            JsonValue::Number(5120.into()),
        );
        m.insert(
            keys::BOX_MEMORY_MB.to_string(),
            JsonValue::Number(512.into()),
        );
        m.insert(
            keys::BOX_TIMEOUT_SECS.to_string(),
            JsonValue::Number(60.into()),
        );
        m.insert(
            keys::BOX_CPU_QUOTA.to_string(),
            JsonValue::Number(50000.into()),
        );
        m.insert(keys::BOX_NETWORK.to_string(), JsonValue::Bool(false));
        m.insert(
            keys::BOX_CONCURRENT_TASKS.to_string(),
            JsonValue::Number(1.into()),
        );
        m.insert(
            keys::TASK_TIMEOUT_SECS.to_string(),
            JsonValue::Number(300.into()),
        );
        m.insert(keys::SERVICE_KEYS.to_string(), JsonValue::Bool(false));
        m.insert(keys::ALERTS.to_string(), JsonValue::Bool(false));
        m.insert(keys::SELF_HOSTED.to_string(), JsonValue::Bool(false));
        m
    }

    /// Resolve effective features by merging cloud defaults, plan, and org overrides.
    ///
    /// Layering order (later wins):
    /// 1. `cloud_defaults()` — safe floor so missing keys never grant unlimited
    /// 2. Plan features from `plan.features`
    /// 3. Org overrides from `org.features`
    pub fn resolve(plan_features: Option<&JsonValue>, org_features: Option<&JsonValue>) -> Self {
        // Start with conservative cloud defaults so missing keys
        // never accidentally grant unlimited access.
        let mut merged = Self::cloud_defaults();

        // Layer plan values on top
        if let Some(JsonValue::Object(plan)) = plan_features {
            for (k, v) in plan {
                merged.insert(k.clone(), v.clone());
            }
        }

        // Layer org overrides on top (wins over plan)
        if let Some(JsonValue::Object(org)) = org_features {
            for (k, v) in org {
                merged.insert(k.clone(), v.clone());
            }
        }

        Self { inner: merged }
    }

    /// Resolve features for an organization by looking up plan + org overrides.
    /// Falls back to unlimited defaults if no plan exists (self-hosted / local dev).
    pub async fn resolve_for_org(db: &DatabasePool, org_id: &Uuid) -> Self {
        use super::subscription::OrgPlan;

        // Get plan features from the org's selected plan.
        let plan_features = match OrgPlan::get_by_org_id(db, org_id).await {
            Ok(subscription) => {
                if let Ok(plan) = subscription.get_plan(db).await {
                    plan.features.clone()
                } else {
                    None
                }
            }
            Err(_) => None,
        };

        // Get org-level overrides
        let org_features = Self::get_org_features(db, org_id).await;

        // If no subscription at all, return unlimited (self-hosted / local dev)
        if plan_features.is_none() && org_features.is_none() {
            return Self::unlimited();
        }

        Self::resolve(plan_features.as_ref(), org_features.as_ref())
    }

    /// Resolve features for an organization in hosted billing mode.
    ///
    /// Unlike `resolve_for_org`, a missing subscription is not treated as
    /// self-hosted/local-dev unlimited access. Hosted callers use this while
    /// onboarding redirects users to plan selection so any accidental feature
    /// read stays conservative.
    pub async fn resolve_for_hosted_org(db: &DatabasePool, org_id: &Uuid) -> Self {
        use super::subscription::OrgPlan;

        let plan_features = match OrgPlan::get_by_org_id(db, org_id).await {
            Ok(subscription) => {
                if let Ok(plan) = subscription.get_plan(db).await {
                    plan.features.clone()
                } else {
                    None
                }
            }
            Err(_) => None,
        };

        let org_features = Self::get_org_features(db, org_id).await;
        Self::resolve(plan_features.as_ref(), org_features.as_ref())
    }

    /// Fetch the org.features column value.
    async fn get_org_features(db: &DatabasePool, org_id: &Uuid) -> Option<JsonValue> {
        match db {
            DatabasePool::Postgres(pool) => {
                let row: Option<(Option<JsonValue>,)> =
                    sqlx::query_as("SELECT features FROM org WHERE org_id = $1")
                        .bind(org_id)
                        .fetch_optional(pool)
                        .await
                        .ok()?;
                row.and_then(|(f,)| f)
            }
            DatabasePool::Sqlite(pool) => {
                let row: Option<(Option<String>,)> =
                    sqlx::query_as("SELECT features FROM org WHERE org_id = ?")
                        .bind(org_id)
                        .fetch_optional(pool)
                        .await
                        .ok()?;
                row.and_then(|(f,)| f)
                    .and_then(|s| serde_json::from_str(&s).ok())
            }
        }
    }

    // ── Typed Accessors ──────────────────────────────────────────────────

    /// Check if a boolean feature is enabled.
    /// Returns false if the key is absent or not a boolean.
    pub fn has(&self, key: &str) -> bool {
        self.inner
            .get(key)
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
    }

    /// Get an i64 feature value.
    /// Returns `None` if the key is absent or not a number.
    pub fn get_i64(&self, key: &str) -> Option<i64> {
        self.inner.get(key).and_then(|v| v.as_i64())
    }

    /// Get an i64 feature value with a default.
    pub fn get_i64_or(&self, key: &str, default: i64) -> i64 {
        self.get_i64(key).unwrap_or(default)
    }

    /// Check if a numeric feature is unlimited (-1).
    pub fn is_unlimited(&self, key: &str) -> bool {
        self.get_i64(key).is_some_and(|v| v < 0)
    }

    // ── Convenience Methods (matching old UsageLimit interface) ───────────

    pub fn runs_per_month(&self) -> i32 {
        self.get_i64_or(keys::RUNS_PER_MONTH, -1) as i32
    }

    pub fn storage_bytes(&self) -> i64 {
        self.get_i64_or(keys::STORAGE_BYTES, -1)
    }

    /// Maximum bytes per file upload. Plans with -1 (unlimited) are clamped
    /// to `keys::MAX_FILE_UPLOAD_BYTES` (50 GB).
    pub fn file_upload_max_bytes(&self) -> i64 {
        let v = self.get_i64_or(keys::FILE_UPLOAD_MAX_BYTES, 104_857_600); // 100 MB default
        if v < 0 {
            keys::MAX_FILE_UPLOAD_BYTES
        } else {
            v
        }
    }

    pub fn is_unlimited_file_upload(&self) -> bool {
        self.file_upload_max_bytes() == keys::MAX_FILE_UPLOAD_BYTES
    }

    pub fn team_members(&self) -> i32 {
        self.get_i64_or(keys::TEAM_MEMBERS, -1) as i32
    }

    pub fn call_retention_days(&self) -> i32 {
        self.get_i64_or(keys::CALL_RETENTION_DAYS, -1) as i32
    }

    pub fn call_storage_bytes(&self) -> i64 {
        self.get_i64_or(keys::CALL_STORAGE_BYTES, -1)
    }

    pub fn store_storage_bytes(&self) -> i64 {
        self.get_i64_or(keys::STORE_STORAGE_BYTES, -1)
    }

    pub fn task_minutes_per_month(&self) -> i32 {
        self.get_i64_or(keys::TASK_MINUTES_PER_MONTH, -1) as i32
    }

    pub fn compute_units_per_month(&self) -> i64 {
        self.get_i64_or(keys::COMPUTE_UNITS_PER_MONTH, -1)
    }

    pub fn is_unlimited_compute_units(&self) -> bool {
        self.is_unlimited(keys::COMPUTE_UNITS_PER_MONTH)
    }

    /// Org-level CUS spending cap. -1 or absent means no cap.
    pub fn compute_units_budget(&self) -> i64 {
        self.get_i64_or(keys::COMPUTE_UNITS_BUDGET, -1)
    }

    // ── Box (container) feature accessors ─────────────────────────────────

    pub fn box_tmp_size_mb(&self) -> i64 {
        self.get_i64_or(keys::BOX_TMP_SIZE_MB, -1)
    }

    pub fn box_disk_size_mb(&self) -> i64 {
        self.get_i64_or(keys::BOX_DISK_SIZE_MB, -1)
    }

    pub fn box_memory_mb(&self) -> i64 {
        self.get_i64_or(keys::BOX_MEMORY_MB, -1)
    }

    pub fn box_timeout_secs(&self) -> i64 {
        self.get_i64_or(keys::BOX_TIMEOUT_SECS, -1)
    }

    pub fn box_cpu_quota(&self) -> i64 {
        self.get_i64_or(keys::BOX_CPU_QUOTA, -1)
    }

    /// Whether this org is allowed to use network-enabled containers.
    pub fn box_network_allowed(&self) -> bool {
        self.has(keys::BOX_NETWORK)
    }

    pub fn box_concurrent_tasks(&self) -> i64 {
        self.get_i64_or(keys::BOX_CONCURRENT_TASKS, -1)
    }

    // ── Code task caps ────────────────────────────────────────────────────

    pub fn task_timeout_secs(&self) -> i64 {
        self.get_i64_or(keys::TASK_TIMEOUT_SECS, -1)
    }

    pub fn is_unlimited_runs(&self) -> bool {
        self.is_unlimited(keys::RUNS_PER_MONTH)
    }

    pub fn is_unlimited_storage(&self) -> bool {
        self.is_unlimited(keys::STORAGE_BYTES)
    }

    pub fn is_unlimited_team_members(&self) -> bool {
        self.is_unlimited(keys::TEAM_MEMBERS)
    }

    pub fn is_unlimited_call_retention(&self) -> bool {
        self.is_unlimited(keys::CALL_RETENTION_DAYS)
    }

    pub fn is_unlimited_call_storage(&self) -> bool {
        self.is_unlimited(keys::CALL_STORAGE_BYTES)
    }

    pub fn is_unlimited_store_storage(&self) -> bool {
        self.is_unlimited(keys::STORE_STORAGE_BYTES)
    }

    /// Maximum number of custom domains allowed.
    /// Returns -1 for unlimited, 0 for disabled, or a positive limit.
    pub fn max_custom_domains(&self) -> i32 {
        match self.inner.get(keys::CUSTOM_DOMAINS) {
            Some(JsonValue::Number(n)) => n.as_i64().unwrap_or(0) as i32,
            _ => 0,
        }
    }

    /// Check if custom domains are enabled (limit > 0 or unlimited).
    pub fn has_custom_domains(&self) -> bool {
        self.max_custom_domains() != 0
    }

    pub fn is_unlimited_custom_domains(&self) -> bool {
        self.max_custom_domains() < 0
    }

    pub fn has_service_keys(&self) -> bool {
        self.has(keys::SERVICE_KEYS)
    }

    pub fn has_alerts(&self) -> bool {
        self.has(keys::ALERTS)
    }

    /// Get the rate limit in requests per second.
    /// Returns -1 for unlimited, or a positive value for the limit.
    pub fn rate_limit_rps(&self) -> i64 {
        self.get_i64_or(keys::RATE_LIMIT_RPS, -1)
    }

    pub fn is_unlimited_rate_limit(&self) -> bool {
        self.is_unlimited(keys::RATE_LIMIT_RPS)
    }

    // ── Formatting Helpers ───────────────────────────────────────────────

    /// Format storage bytes as human-readable string.
    pub fn format_bytes<T: std::borrow::Borrow<i64>>(bytes: T) -> String {
        Self::format_bytes_internal(*bytes.borrow(), false)
    }

    /// Format bytes with ~ prefix to indicate approximation (for estimated values).
    pub fn format_bytes_approx<T: std::borrow::Borrow<i64>>(bytes: T) -> String {
        Self::format_bytes_internal(*bytes.borrow(), true)
    }

    fn format_bytes_internal(bytes: i64, approx: bool) -> String {
        if bytes < 0 {
            return "Unlimited".to_string();
        }
        if bytes == 0 {
            return "0 B".to_string();
        }

        let prefix = if approx { "~" } else { "" };

        const KB: i64 = 1024;
        const MB: i64 = KB * 1024;
        const GB: i64 = MB * 1024;
        const TB: i64 = GB * 1024;

        if bytes >= TB {
            format!("{}{:.1} TB", prefix, bytes as f64 / TB as f64)
        } else if bytes >= GB {
            format!("{}{:.1} GB", prefix, bytes as f64 / GB as f64)
        } else if bytes >= MB {
            format!("{}{:.1} MB", prefix, bytes as f64 / MB as f64)
        } else if bytes >= KB {
            format!("{}{:.1} KB", prefix, bytes as f64 / KB as f64)
        } else {
            format!("{}{} B", prefix, bytes)
        }
    }

    /// Format call retention days as human-readable string.
    pub fn format_retention_days(&self) -> String {
        let days = self.call_retention_days();
        if days < 0 {
            "Unlimited".to_string()
        } else if days == 0 {
            "None".to_string()
        } else if days == 1 {
            "1 day".to_string()
        } else {
            format!("{} days", days)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_unlimited_defaults() {
        let f = Features::unlimited();
        assert!(f.is_unlimited_runs());
        assert!(f.is_unlimited_storage());
        assert!(f.is_unlimited_team_members());
        assert!(f.is_unlimited_call_retention());
        assert!(f.is_unlimited_call_storage());
        assert!(f.is_unlimited_store_storage());
        assert_eq!(f.store_storage_bytes(), -1);
        assert!(f.has_custom_domains());
        assert!(f.is_unlimited_custom_domains());
        assert_eq!(f.max_custom_domains(), -1);
        assert!(f.has_service_keys());
        assert!(f.has_alerts());
        // Box/task features are unlimited
        assert_eq!(f.task_minutes_per_month(), -1);
        assert_eq!(f.box_tmp_size_mb(), -1);
        assert_eq!(f.box_disk_size_mb(), -1);
        assert_eq!(f.box_memory_mb(), -1);
        assert_eq!(f.box_timeout_secs(), -1);
        assert_eq!(f.box_cpu_quota(), -1);
        assert!(f.box_network_allowed());
        assert_eq!(f.box_concurrent_tasks(), -1);
        assert_eq!(f.task_timeout_secs(), -1);
    }

    #[test]
    fn test_resolve_plan_only() {
        let plan = json!({
            "runs_per_month": 100000,
            "storage_bytes": 2147483648_i64,
            "team_members": 5,
            "call_retention_days": 30,
            "call_storage_bytes": 1073741824_i64,
            "store_storage_bytes": 1073741824_i64,
            "custom_domains": 0
        });

        let f = Features::resolve(Some(&plan), None);
        assert_eq!(f.runs_per_month(), 100000);
        assert_eq!(f.storage_bytes(), 2147483648);
        assert_eq!(f.team_members(), 5);
        assert_eq!(f.call_retention_days(), 30);
        assert_eq!(f.call_storage_bytes(), 1073741824);
        assert_eq!(f.store_storage_bytes(), 1073741824);
        assert!(!f.is_unlimited_store_storage());
        assert!(!f.has_custom_domains());
        assert_eq!(f.max_custom_domains(), 0);
    }

    #[test]
    fn test_resolve_org_overrides_plan() {
        let plan = json!({
            "runs_per_month": 100000,
            "custom_domains": 5
        });
        let org = json!({
            "runs_per_month": 500000,
            "custom_domains": 25
        });

        let f = Features::resolve(Some(&plan), Some(&org));
        assert_eq!(f.runs_per_month(), 500000);
        assert!(f.has_custom_domains());
        assert_eq!(f.max_custom_domains(), 25);
    }

    #[test]
    fn test_custom_domains_numeric_limits() {
        let plan = json!({ "custom_domains": 5 });
        let f = Features::resolve(Some(&plan), None);
        assert!(f.has_custom_domains());
        assert!(!f.is_unlimited_custom_domains());
        assert_eq!(f.max_custom_domains(), 5);

        let plan = json!({ "custom_domains": -1 });
        let f = Features::resolve(Some(&plan), None);
        assert!(f.has_custom_domains());
        assert!(f.is_unlimited_custom_domains());
        assert_eq!(f.max_custom_domains(), -1);

        let plan = json!({ "custom_domains": 0 });
        let f = Features::resolve(Some(&plan), None);
        assert!(!f.has_custom_domains());
        assert_eq!(f.max_custom_domains(), 0);
    }

    #[test]
    fn test_format_bytes() {
        assert_eq!(Features::format_bytes(-1i64), "Unlimited");
        assert_eq!(Features::format_bytes(0i64), "0 B");
        assert_eq!(Features::format_bytes(1024i64), "1.0 KB");
        assert_eq!(Features::format_bytes(2147483648i64), "2.0 GB");
    }

    #[test]
    fn test_format_retention_days() {
        let mut f = Features::empty();
        f.inner.insert(
            keys::CALL_RETENTION_DAYS.to_string(),
            JsonValue::Number((-1).into()),
        );
        assert_eq!(f.format_retention_days(), "Unlimited");

        f.inner.insert(
            keys::CALL_RETENTION_DAYS.to_string(),
            JsonValue::Number(0.into()),
        );
        assert_eq!(f.format_retention_days(), "None");

        f.inner.insert(
            keys::CALL_RETENTION_DAYS.to_string(),
            JsonValue::Number(30.into()),
        );
        assert_eq!(f.format_retention_days(), "30 days");
    }

    #[test]
    fn test_empty_features_defaults() {
        let f = Features::empty();
        // Empty features return -1 (unlimited) for all numeric fields via defaults
        assert_eq!(f.runs_per_month(), -1);
        assert_eq!(f.rate_limit_rps(), -1);
        assert!(!f.has_custom_domains()); // defaults to false for booleans
        assert!(!f.has_service_keys()); // defaults to false for booleans
        assert!(!f.has_alerts()); // defaults to false for booleans
    }

    #[test]
    fn test_cloud_defaults_for_missing_keys() {
        // When a cloud plan exists but is missing feature keys,
        // resolve() should return conservative defaults, not unlimited.
        let plan = json!({
            "runs_per_month": 50000
        });
        // Only runs_per_month is set; everything else should get cloud defaults
        let f = Features::resolve(Some(&plan), None);

        // Provided key keeps its value
        assert_eq!(f.runs_per_month(), 50000);

        // Missing numeric keys get safe conservative defaults (not -1 unlimited)
        assert_eq!(f.storage_bytes(), 0);
        assert_eq!(f.team_members(), 1);
        assert_eq!(f.call_retention_days(), 0);
        assert_eq!(f.call_storage_bytes(), 0);
        assert_eq!(f.store_storage_bytes(), 0);
        assert_eq!(f.rate_limit_rps(), 10);

        // Missing boolean keys default to false (disabled)
        assert!(!f.has_custom_domains());
        assert!(!f.has_service_keys());
        assert!(!f.has_alerts());

        // Box/task features get conservative cloud defaults
        assert_eq!(f.task_minutes_per_month(), 0);
        assert_eq!(f.box_tmp_size_mb(), 500);
        assert_eq!(f.box_disk_size_mb(), 5120);
        assert_eq!(f.box_memory_mb(), 512);
        assert_eq!(f.box_timeout_secs(), 60);
        assert_eq!(f.box_cpu_quota(), 50000);
        assert!(!f.box_network_allowed());
        assert_eq!(f.box_concurrent_tasks(), 1);
        assert_eq!(f.task_timeout_secs(), 300);

        // None of these should report as unlimited
        assert!(!f.is_unlimited_runs());
        assert!(!f.is_unlimited_storage());
        assert!(!f.is_unlimited_team_members());
        assert!(!f.is_unlimited_call_retention());
        assert!(!f.is_unlimited_call_storage());
        assert!(!f.is_unlimited_store_storage());
        assert!(!f.is_unlimited_rate_limit());
    }

    #[test]
    fn test_resolve_with_empty_plan_json() {
        // An empty plan JSON object should get all cloud defaults
        let plan = json!({});
        let f = Features::resolve(Some(&plan), None);

        assert_eq!(f.runs_per_month(), 0);
        assert_eq!(f.storage_bytes(), 0);
        assert_eq!(f.team_members(), 1);
        assert_eq!(f.call_retention_days(), 0);
        assert_eq!(f.call_storage_bytes(), 0);
        assert_eq!(f.store_storage_bytes(), 0);
        assert_eq!(f.rate_limit_rps(), 10);
        assert!(!f.has_custom_domains());
        assert!(!f.has_service_keys());
        assert!(!f.has_alerts());
    }

    #[test]
    fn test_org_override_fills_missing_plan_keys() {
        // Plan is missing custom_domains, but org override enables it
        let plan = json!({
            "runs_per_month": 100000
        });
        let org = json!({
            "custom_domains": 10,
            "service_keys": true
        });
        let f = Features::resolve(Some(&plan), Some(&org));

        assert_eq!(f.runs_per_month(), 100000);
        assert!(f.has_custom_domains());
        assert_eq!(f.max_custom_domains(), 10);
        assert!(f.has_service_keys());
        // Storage still gets cloud default since neither plan nor org set it
        assert_eq!(f.storage_bytes(), 0);
    }

    #[test]
    fn test_rate_limit_rps() {
        let plan = json!({
            "rate_limit_rps": 20
        });
        let f = Features::resolve(Some(&plan), None);
        assert_eq!(f.rate_limit_rps(), 20);
        assert!(!f.is_unlimited_rate_limit());

        // Org override to unlimited
        let org = json!({
            "rate_limit_rps": -1
        });
        let f = Features::resolve(Some(&plan), Some(&org));
        assert_eq!(f.rate_limit_rps(), -1);
        assert!(f.is_unlimited_rate_limit());

        // Unlimited defaults
        let f = Features::unlimited();
        assert!(f.is_unlimited_rate_limit());
    }

    #[test]
    fn test_box_features_plan_override() {
        let plan = json!({
            "box_tmp_size_mb": 2048,
            "box_disk_size_mb": 10240,
            "box_memory_mb": 1024,
            "box_timeout_secs": 3600,
            "box_cpu_quota": 100000,
            "box_network": true,
            "box_concurrent_tasks": 5,
            "task_minutes_per_month": 500,
            "task_timeout_secs": 600
        });
        let f = Features::resolve(Some(&plan), None);
        assert_eq!(f.box_tmp_size_mb(), 2048);
        assert_eq!(f.box_disk_size_mb(), 10240);
        assert_eq!(f.box_memory_mb(), 1024);
        assert_eq!(f.box_timeout_secs(), 3600);
        assert_eq!(f.box_cpu_quota(), 100000);
        assert!(f.box_network_allowed());
        assert_eq!(f.box_concurrent_tasks(), 5);
        assert_eq!(f.task_minutes_per_month(), 500);
        assert_eq!(f.task_timeout_secs(), 600);
    }

    #[test]
    fn test_box_features_org_overrides_plan() {
        let plan = json!({
            "box_memory_mb": 512,
            "box_concurrent_tasks": 3
        });
        let org = json!({
            "box_memory_mb": 2048,
            "box_concurrent_tasks": 10
        });
        let f = Features::resolve(Some(&plan), Some(&org));
        assert_eq!(f.box_memory_mb(), 2048);
        assert_eq!(f.box_concurrent_tasks(), 10);
        // Unset keys still get cloud defaults
        assert_eq!(f.box_timeout_secs(), 60);
    }

    #[test]
    fn test_box_features_unlimited_marker() {
        let plan = json!({
            "box_memory_mb": -1,
            "box_concurrent_tasks": -1
        });
        let f = Features::resolve(Some(&plan), None);
        assert!(f.is_unlimited(keys::BOX_MEMORY_MB));
        assert!(f.is_unlimited(keys::BOX_CONCURRENT_TASKS));
        assert_eq!(f.box_memory_mb(), -1);
        assert_eq!(f.box_concurrent_tasks(), -1);
    }

    #[test]
    fn test_compute_units_per_month_defaults() {
        let f = Features::unlimited();
        assert_eq!(f.compute_units_per_month(), -1);
        assert!(f.is_unlimited_compute_units());

        let plan = json!({ "compute_units_per_month": 5000 });
        let f = Features::resolve(Some(&plan), None);
        assert_eq!(f.compute_units_per_month(), 5000);
        assert!(!f.is_unlimited_compute_units());

        // Cloud defaults give 0 when missing
        let plan = json!({});
        let f = Features::resolve(Some(&plan), None);
        assert_eq!(f.compute_units_per_month(), 0);
    }

    #[test]
    fn test_compute_units_budget_resolution() {
        // Budget is an org-level setting, not set in plan
        let plan = json!({ "compute_units_per_month": 50000 });
        let f = Features::resolve(Some(&plan), None);
        assert_eq!(f.compute_units_budget(), -1); // no budget by default

        // Org sets a budget
        let org = json!({ "compute_units_budget": 100000 });
        let f = Features::resolve(Some(&plan), Some(&org));
        assert_eq!(f.compute_units_budget(), 100000);
        assert_eq!(f.compute_units_per_month(), 50000); // plan value preserved
    }

    #[test]
    fn test_file_upload_max_bytes_defaults() {
        // Unlimited (self-hosted) clamps to 50 GB ceiling
        let f = Features::unlimited();
        assert_eq!(f.file_upload_max_bytes(), keys::MAX_FILE_UPLOAD_BYTES);
        assert!(f.is_unlimited_file_upload());

        // Empty features should return the default (100MB)
        let f = Features::empty();
        assert_eq!(f.file_upload_max_bytes(), 104_857_600);
        assert!(!f.is_unlimited_file_upload());

        // Cloud defaults should be 100MB
        let f = Features::resolve(None, None);
        assert_eq!(f.file_upload_max_bytes(), 104_857_600);
    }

    #[test]
    fn test_file_upload_max_bytes_resolution() {
        use serde_json::json;

        // Plan sets 1GB
        let plan = json!({"file_upload_max_bytes": 1_073_741_824i64});
        let f = Features::resolve(Some(&plan), None);
        assert_eq!(f.file_upload_max_bytes(), 1_073_741_824);

        // Org overrides to 5GB
        let org = json!({"file_upload_max_bytes": 5_368_709_120i64});
        let f = Features::resolve(Some(&plan), Some(&org));
        assert_eq!(f.file_upload_max_bytes(), 5_368_709_120);

        // Org overrides to unlimited — clamped to 50 GB ceiling
        let org = json!({"file_upload_max_bytes": -1});
        let f = Features::resolve(Some(&plan), Some(&org));
        assert_eq!(f.file_upload_max_bytes(), keys::MAX_FILE_UPLOAD_BYTES);
        assert!(f.is_unlimited_file_upload());
    }
}

//! 5-tier limit resolution for container (box) tasks.
//!
//! Resolution hierarchy (later wins, capped by system max):
//! 1. **Default** — sensible baseline when BoxConf omits a field
//! 2. **Plan limit** — from `plan.features` via `Features`
//! 3. **Org override** — from `org.features` (already merged into `Features`)
//! 4. **BoxConf request** — per-container value set by user code (size preset or raw fields)
//! 5. **System max** — absolute ceiling, nothing can exceed these

use hot::db::features::{self, Features};

#[derive(Debug, Clone)]
pub struct BoxLimits {
    pub tmp_size_mb: u64,
    pub disk_size_mb: u64,
    pub memory_mb: u64,
    pub timeout_secs: u64,
    pub cpu_quota: u64,
    pub network: bool,
    /// The resolved size label (from explicit `size` field or inferred).
    pub size: BoxSize,
}

/// Named container size presets.
///
/// Each size maps to a fixed resource profile and a CUS (Compute Unit Seconds)
/// multiplier used for metered billing. Users specify a size in BoxConf;
/// raw fields (`memory`, `disk-size`, etc.) override individual values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoxSize {
    Nano,
    Micro,
    Small,
    Medium,
    Large,
    Xlarge,
    Xxlarge,
    X4large,
}

/// Resource profile for a single BoxSize preset.
struct SizeProfile {
    memory_mb: u64,
    cpu_quota: u64,
    tmp_size_mb: u64,
    disk_size_mb: u64,
    timeout_secs: u64,
}

impl BoxSize {
    pub const ALL: &[BoxSize] = &[
        BoxSize::Nano,
        BoxSize::Micro,
        BoxSize::Small,
        BoxSize::Medium,
        BoxSize::Large,
        BoxSize::Xlarge,
        BoxSize::Xxlarge,
        BoxSize::X4large,
    ];

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Nano => "nano",
            Self::Micro => "micro",
            Self::Small => "small",
            Self::Medium => "medium",
            Self::Large => "large",
            Self::Xlarge => "xlarge",
            Self::Xxlarge => "2xlarge",
            Self::X4large => "4xlarge",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        s.parse().ok()
    }

    /// CUS multiplier for metered billing.
    /// Base unit: `small` = 1x. Usage = multiplier * wall-clock seconds.
    pub fn cus_multiplier(&self) -> f64 {
        match self {
            Self::Nano => 0.25,
            Self::Micro => 0.5,
            Self::Small => 1.0,
            Self::Medium => 2.0,
            Self::Large => 4.0,
            Self::Xlarge => 8.0,
            Self::Xxlarge => 16.0,
            Self::X4large => 32.0,
        }
    }

    /// Calculate CUS for a completed task.
    pub fn compute_units(&self, duration_ms: i64) -> i64 {
        let secs = (duration_ms as f64) / 1000.0;
        (secs * self.cus_multiplier()).ceil() as i64
    }

    fn profile(&self) -> SizeProfile {
        match self {
            Self::Nano => SizeProfile {
                memory_mb: 64,
                cpu_quota: 10_000,
                tmp_size_mb: 32,
                disk_size_mb: 256,
                timeout_secs: 60,
            },
            Self::Micro => SizeProfile {
                memory_mb: 128,
                cpu_quota: 25_000,
                tmp_size_mb: 64,
                disk_size_mb: 512,
                timeout_secs: 60,
            },
            Self::Small => SizeProfile {
                memory_mb: 256,
                cpu_quota: 25_000,
                tmp_size_mb: 128,
                disk_size_mb: 1_024,
                timeout_secs: 60,
            },
            Self::Medium => SizeProfile {
                memory_mb: 512,
                cpu_quota: 50_000,
                tmp_size_mb: 256,
                disk_size_mb: 5_120,
                timeout_secs: 300,
            },
            Self::Large => SizeProfile {
                memory_mb: 1_024,
                cpu_quota: 75_000,
                tmp_size_mb: 500,
                disk_size_mb: 10_240,
                timeout_secs: 600,
            },
            Self::Xlarge => SizeProfile {
                memory_mb: 2_048,
                cpu_quota: 100_000,
                tmp_size_mb: 1_024,
                disk_size_mb: 20_480,
                timeout_secs: 1_800,
            },
            Self::Xxlarge => SizeProfile {
                memory_mb: 4_096,
                cpu_quota: 100_000,
                tmp_size_mb: 2_048,
                disk_size_mb: 51_200,
                timeout_secs: 3_600,
            },
            Self::X4large => SizeProfile {
                memory_mb: 8_192,
                cpu_quota: 100_000,
                tmp_size_mb: 4_096,
                disk_size_mb: 51_200,
                timeout_secs: 7_200,
            },
        }
    }

    /// Infer the closest size from resolved memory_mb (used when no explicit size was given).
    pub fn infer_from_memory(memory_mb: u64) -> Self {
        for size in Self::ALL.iter().rev() {
            if memory_mb >= size.profile().memory_mb {
                return *size;
            }
        }
        Self::Nano
    }
}

impl std::fmt::Display for BoxSize {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for BoxSize {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "nano" => Ok(Self::Nano),
            "micro" => Ok(Self::Micro),
            "small" | "sm" => Ok(Self::Small),
            "medium" | "md" => Ok(Self::Medium),
            "large" | "lg" => Ok(Self::Large),
            "xlarge" | "xl" => Ok(Self::Xlarge),
            "2xlarge" | "xxl" => Ok(Self::Xxlarge),
            "4xlarge" => Ok(Self::X4large),
            _ => Err(()),
        }
    }
}

/// Overrides for per-VM defaults, set via worker config.
/// Any `None` field falls back to the compile-time default.
#[derive(Debug, Clone, Default)]
pub struct BoxDefaults {
    pub memory_mb: Option<u64>,
    pub disk_size_mb: Option<u64>,
    pub tmp_size_mb: Option<u64>,
    pub timeout_secs: Option<u64>,
    pub cpu_quota: Option<u64>,
}

impl BoxLimits {
    // Tier 5: System max — absolute ceiling
    const SYSTEM_MAX_TMP_SIZE_MB: u64 = 10_240;
    const SYSTEM_MAX_DISK_SIZE_MB: u64 = 51_200;
    const SYSTEM_MAX_MEMORY_MB: u64 = 8_192;
    const SYSTEM_MAX_TIMEOUT_SECS: u64 = 86_400; // 24 hours
    const SYSTEM_MAX_CPU_QUOTA: u64 = 100_000;

    // Tier 1: Compile-time defaults — used when BoxConf omits a field
    pub const DEFAULT_TMP_SIZE_MB: u64 = 500;
    pub const DEFAULT_DISK_SIZE_MB: u64 = 5_120;
    pub const DEFAULT_MEMORY_MB: u64 = 512;
    pub const DEFAULT_TIMEOUT_SECS: u64 = 60;
    pub const DEFAULT_CPU_QUOTA: u64 = 50_000;

    /// Resolve effective limits from the merged Features (tiers 1-3) and
    /// per-container BoxConf args (tier 4), capped by system max (tier 5).
    pub fn resolve(features: &Features, boxconf_args: &serde_json::Value) -> Self {
        Self::resolve_with_defaults(features, boxconf_args, &BoxDefaults::default())
    }

    /// Resolve with custom per-VM defaults (from worker config).
    ///
    /// When `boxconf_args` contains a `"size"` field, the named preset provides
    /// base values. Raw fields (`memory_mb`, `disk_size_mb`, etc.) override
    /// individual values from the preset. When no size is given, worker defaults
    /// (or compile-time defaults) apply as before.
    pub fn resolve_with_defaults(
        features: &Features,
        boxconf_args: &serde_json::Value,
        defaults: &BoxDefaults,
    ) -> Self {
        let explicit_size = boxconf_args
            .get("size")
            .and_then(|v| v.as_str())
            .and_then(BoxSize::parse);

        // Base values: size preset > worker defaults > compile-time defaults
        let (default_mem, default_disk, default_tmp, default_timeout, default_cpu) =
            if let Some(size) = explicit_size {
                let p = size.profile();
                (
                    p.memory_mb,
                    p.disk_size_mb,
                    p.tmp_size_mb,
                    p.timeout_secs,
                    p.cpu_quota,
                )
            } else {
                (
                    defaults.memory_mb.unwrap_or(Self::DEFAULT_MEMORY_MB),
                    defaults.disk_size_mb.unwrap_or(Self::DEFAULT_DISK_SIZE_MB),
                    defaults.tmp_size_mb.unwrap_or(Self::DEFAULT_TMP_SIZE_MB),
                    defaults.timeout_secs.unwrap_or(Self::DEFAULT_TIMEOUT_SECS),
                    defaults.cpu_quota.unwrap_or(Self::DEFAULT_CPU_QUOTA),
                )
            };

        let tmp_size_mb = Self::resolve_numeric(
            boxconf_args.get("tmp_size_mb").and_then(|v| v.as_u64()),
            default_tmp,
            features.get_i64(features::keys::BOX_TMP_SIZE_MB),
            Self::SYSTEM_MAX_TMP_SIZE_MB,
        );

        let disk_size_mb = Self::resolve_numeric(
            boxconf_args.get("disk_size_mb").and_then(|v| v.as_u64()),
            default_disk,
            features.get_i64(features::keys::BOX_DISK_SIZE_MB),
            Self::SYSTEM_MAX_DISK_SIZE_MB,
        );

        let memory_mb = Self::resolve_numeric(
            boxconf_args.get("memory_mb").and_then(|v| v.as_u64()),
            default_mem,
            features.get_i64(features::keys::BOX_MEMORY_MB),
            Self::SYSTEM_MAX_MEMORY_MB,
        );

        let timeout_secs = Self::resolve_numeric(
            boxconf_args.get("timeout_secs").and_then(|v| v.as_u64()),
            default_timeout,
            features.get_i64(features::keys::BOX_TIMEOUT_SECS),
            Self::SYSTEM_MAX_TIMEOUT_SECS,
        );

        let cpu_quota = Self::resolve_numeric(
            boxconf_args.get("cpu_quota").and_then(|v| v.as_u64()),
            default_cpu,
            features.get_i64(features::keys::BOX_CPU_QUOTA),
            Self::SYSTEM_MAX_CPU_QUOTA,
        );

        let boxconf_network = boxconf_args
            .get("network")
            .and_then(|v| v.as_str())
            .map(|s| s == "internet");

        let network = match boxconf_network {
            Some(false) => false,
            _ => features.box_network_allowed(),
        };

        let size = explicit_size.unwrap_or_else(|| BoxSize::infer_from_memory(memory_mb));

        Self {
            tmp_size_mb,
            disk_size_mb,
            memory_mb,
            timeout_secs,
            cpu_quota,
            network,
            size,
        }
    }

    /// Resolve a single numeric limit through the 5-tier hierarchy.
    ///
    /// - `boxconf_value`: user-requested value from BoxConf (tier 4), None = use default
    /// - `default`: tier 1 baseline
    /// - `feature_limit`: from Features (merged tiers 2+3), -1 = unlimited, None/0 = system max
    /// - `system_max`: absolute ceiling (tier 5)
    fn resolve_numeric(
        boxconf_value: Option<u64>,
        default: u64,
        feature_limit: Option<i64>,
        system_max: u64,
    ) -> u64 {
        let requested = boxconf_value.unwrap_or(default);

        let feature_max = match feature_limit {
            Some(v) if v < 0 => system_max, // -1 = unlimited → defer to system max
            Some(v) if v > 0 => v as u64,
            _ => system_max, // missing or 0 → system max
        };

        requested.min(feature_max).min(system_max)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn features_from(plan_json: serde_json::Value) -> Features {
        Features::resolve(Some(&plan_json), None)
    }

    #[test]
    fn test_defaults_when_boxconf_empty() {
        let features = Features::unlimited();
        let args = json!({});
        let limits = BoxLimits::resolve(&features, &args);

        assert_eq!(limits.tmp_size_mb, BoxLimits::DEFAULT_TMP_SIZE_MB);
        assert_eq!(limits.disk_size_mb, BoxLimits::DEFAULT_DISK_SIZE_MB);
        assert_eq!(limits.memory_mb, BoxLimits::DEFAULT_MEMORY_MB);
        assert_eq!(limits.timeout_secs, BoxLimits::DEFAULT_TIMEOUT_SECS);
        assert_eq!(limits.cpu_quota, BoxLimits::DEFAULT_CPU_QUOTA);
        assert!(limits.network);
    }

    #[test]
    fn test_boxconf_values_used() {
        let features = Features::unlimited();
        let args = json!({
            "tmp_size_mb": 1024,
            "disk_size_mb": 10240,
            "memory_mb": 2048,
            "timeout_secs": 3600,
            "cpu_quota": 80000,
            "network": "internet"
        });
        let limits = BoxLimits::resolve(&features, &args);

        assert_eq!(limits.tmp_size_mb, 1024);
        assert_eq!(limits.disk_size_mb, 10240);
        assert_eq!(limits.memory_mb, 2048);
        assert_eq!(limits.timeout_secs, 3600);
        assert_eq!(limits.cpu_quota, 80000);
        assert!(limits.network);
    }

    #[test]
    fn test_plan_limits_cap_boxconf() {
        let features = features_from(json!({
            "box_memory_mb": 1024,
            "box_timeout_secs": 300,
            "box_network": true
        }));
        let args = json!({
            "memory_mb": 4096,
            "timeout_secs": 7200,
            "network": "internet"
        });
        let limits = BoxLimits::resolve(&features, &args);

        assert_eq!(limits.memory_mb, 1024);
        assert_eq!(limits.timeout_secs, 300);
        assert!(limits.network);
    }

    #[test]
    fn test_system_max_caps_everything() {
        let features = Features::unlimited();
        let args = json!({
            "tmp_size_mb": 999999,
            "disk_size_mb": 999999,
            "memory_mb": 999999,
            "timeout_secs": 999999,
            "cpu_quota": 999999
        });
        let limits = BoxLimits::resolve(&features, &args);

        assert_eq!(limits.tmp_size_mb, BoxLimits::SYSTEM_MAX_TMP_SIZE_MB);
        assert_eq!(limits.disk_size_mb, BoxLimits::SYSTEM_MAX_DISK_SIZE_MB);
        assert_eq!(limits.memory_mb, BoxLimits::SYSTEM_MAX_MEMORY_MB);
        assert_eq!(limits.timeout_secs, BoxLimits::SYSTEM_MAX_TIMEOUT_SECS);
        assert_eq!(limits.cpu_quota, BoxLimits::SYSTEM_MAX_CPU_QUOTA);
    }

    #[test]
    fn test_network_denied_when_feature_off() {
        let features = features_from(json!({"box_network": false}));
        let args = json!({"network": "internet"});
        let limits = BoxLimits::resolve(&features, &args);

        assert!(!limits.network);
    }

    #[test]
    fn test_network_default_when_not_specified() {
        let features = features_from(json!({"box_network": true}));
        let args = json!({});
        let limits = BoxLimits::resolve(&features, &args);

        assert!(limits.network);
    }

    #[test]
    fn test_network_none_explicit() {
        let features = features_from(json!({"box_network": true}));
        let args = json!({"network": "none"});
        let limits = BoxLimits::resolve(&features, &args);

        assert!(!limits.network);
    }

    #[test]
    fn test_cloud_defaults_restrict_all() {
        let features = features_from(json!({}));
        let args = json!({
            "memory_mb": 4096,
            "timeout_secs": 7200,
            "disk_size_mb": 40000,
            "network": "internet"
        });
        let limits = BoxLimits::resolve(&features, &args);

        assert_eq!(limits.memory_mb, 512);
        assert_eq!(limits.timeout_secs, 60);
        assert_eq!(limits.disk_size_mb, 5120);
        assert!(!limits.network);
    }

    #[test]
    fn test_resolve_numeric_unlimited() {
        let val = BoxLimits::resolve_numeric(Some(4096), 512, Some(-1), 8192);
        assert_eq!(val, 4096);
    }

    #[test]
    fn test_resolve_numeric_no_feature() {
        let val = BoxLimits::resolve_numeric(Some(4096), 512, None, 8192);
        assert_eq!(val, 4096);
    }

    #[test]
    fn test_resolve_numeric_feature_caps() {
        let val = BoxLimits::resolve_numeric(Some(4096), 512, Some(1024), 8192);
        assert_eq!(val, 1024);
    }

    #[test]
    fn test_resolve_numeric_default_used() {
        let val = BoxLimits::resolve_numeric(None, 512, Some(2048), 8192);
        assert_eq!(val, 512);
    }

    #[test]
    fn test_custom_defaults_override_compile_time() {
        let features = Features::unlimited();
        let args = json!({});
        let defaults = BoxDefaults {
            memory_mb: Some(128),
            disk_size_mb: Some(1024),
            tmp_size_mb: Some(64),
            timeout_secs: Some(30),
            cpu_quota: Some(25000),
        };
        let limits = BoxLimits::resolve_with_defaults(&features, &args, &defaults);

        assert_eq!(limits.memory_mb, 128);
        assert_eq!(limits.disk_size_mb, 1024);
        assert_eq!(limits.tmp_size_mb, 64);
        assert_eq!(limits.timeout_secs, 30);
        assert_eq!(limits.cpu_quota, 25000);
    }

    #[test]
    fn test_custom_defaults_partial_override() {
        let features = Features::unlimited();
        let args = json!({});
        let defaults = BoxDefaults {
            memory_mb: Some(128),
            ..Default::default()
        };
        let limits = BoxLimits::resolve_with_defaults(&features, &args, &defaults);

        assert_eq!(limits.memory_mb, 128);
        assert_eq!(limits.disk_size_mb, BoxLimits::DEFAULT_DISK_SIZE_MB);
    }

    #[test]
    fn test_boxconf_overrides_custom_defaults() {
        let features = Features::unlimited();
        let args = json!({"memory_mb": 256});
        let defaults = BoxDefaults {
            memory_mb: Some(128),
            ..Default::default()
        };
        let limits = BoxLimits::resolve_with_defaults(&features, &args, &defaults);

        assert_eq!(limits.memory_mb, 256);
    }

    #[test]
    fn test_feature_caps_custom_defaults() {
        let features = features_from(json!({"box_memory_mb": 64}));
        let args = json!({});
        let defaults = BoxDefaults {
            memory_mb: Some(128),
            ..Default::default()
        };
        let limits = BoxLimits::resolve_with_defaults(&features, &args, &defaults);

        assert_eq!(limits.memory_mb, 64);
    }

    // ── BoxSize tests ──────────────────────────────────────────────────────

    #[test]
    fn test_size_from_str() {
        assert_eq!("nano".parse(), Ok(BoxSize::Nano));
        assert_eq!("micro".parse(), Ok(BoxSize::Micro));
        assert_eq!("small".parse(), Ok(BoxSize::Small));
        assert_eq!("sm".parse(), Ok(BoxSize::Small));
        assert_eq!("medium".parse(), Ok(BoxSize::Medium));
        assert_eq!("large".parse(), Ok(BoxSize::Large));
        assert_eq!("xlarge".parse(), Ok(BoxSize::Xlarge));
        assert_eq!("2xlarge".parse(), Ok(BoxSize::Xxlarge));
        assert_eq!("4xlarge".parse(), Ok(BoxSize::X4large));
        assert!("invalid".parse::<BoxSize>().is_err());
    }

    #[test]
    fn test_size_roundtrip() {
        for size in BoxSize::ALL {
            assert_eq!(size.as_str().parse(), Ok(*size));
        }
    }

    #[test]
    fn test_size_cus_multipliers_ordered() {
        let mut prev = 0.0;
        for size in BoxSize::ALL {
            let mult = size.cus_multiplier();
            assert!(
                mult > prev,
                "{} multiplier {} should be > {}",
                size,
                mult,
                prev
            );
            prev = mult;
        }
    }

    #[test]
    fn test_size_compute_units() {
        assert_eq!(BoxSize::Small.compute_units(60_000), 60);
        assert_eq!(BoxSize::Nano.compute_units(60_000), 15);
        assert_eq!(BoxSize::Medium.compute_units(60_000), 120);
        assert_eq!(BoxSize::Large.compute_units(30_000), 120);
        assert_eq!(BoxSize::Small.compute_units(500), 1); // rounds up
    }

    #[test]
    fn test_size_preset_sets_base_values() {
        let features = Features::unlimited();
        let args = json!({"size": "nano"});
        let limits = BoxLimits::resolve(&features, &args);

        assert_eq!(limits.memory_mb, 64);
        assert_eq!(limits.cpu_quota, 10_000);
        assert_eq!(limits.tmp_size_mb, 32);
        assert_eq!(limits.disk_size_mb, 256);
        assert_eq!(limits.size, BoxSize::Nano);
    }

    #[test]
    fn test_size_large_preset() {
        let features = Features::unlimited();
        let args = json!({"size": "large"});
        let limits = BoxLimits::resolve(&features, &args);

        assert_eq!(limits.memory_mb, 1024);
        assert_eq!(limits.cpu_quota, 75_000);
        assert_eq!(limits.tmp_size_mb, 500);
        assert_eq!(limits.disk_size_mb, 10_240);
        assert_eq!(limits.size, BoxSize::Large);
    }

    #[test]
    fn test_size_with_raw_override() {
        let features = Features::unlimited();
        let args = json!({"size": "nano", "memory_mb": 256});
        let limits = BoxLimits::resolve(&features, &args);

        assert_eq!(limits.memory_mb, 256);
        assert_eq!(limits.cpu_quota, 10_000); // from nano preset
        assert_eq!(limits.tmp_size_mb, 32); // from nano preset
    }

    #[test]
    fn test_size_capped_by_plan() {
        let features = features_from(json!({
            "box_memory_mb": 128,
            "box_cpu_quota": -1
        }));
        let args = json!({"size": "large"});
        let limits = BoxLimits::resolve(&features, &args);

        assert_eq!(limits.memory_mb, 128); // capped by plan
        assert_eq!(limits.cpu_quota, 75_000); // from large preset (cpu unlimited)
    }

    #[test]
    fn test_no_size_infers_from_memory() {
        let features = Features::unlimited();
        let args = json!({"memory_mb": 1024});
        let limits = BoxLimits::resolve(&features, &args);

        assert_eq!(limits.size, BoxSize::Large);
    }

    #[test]
    fn test_no_size_default_memory_infers_small() {
        let features = Features::unlimited();
        let args = json!({});
        let defaults = BoxDefaults {
            memory_mb: Some(256),
            ..Default::default()
        };
        let limits = BoxLimits::resolve_with_defaults(&features, &args, &defaults);

        assert_eq!(limits.size, BoxSize::Small);
    }

    #[test]
    fn test_infer_from_memory_boundaries() {
        assert_eq!(BoxSize::infer_from_memory(32), BoxSize::Nano);
        assert_eq!(BoxSize::infer_from_memory(64), BoxSize::Nano);
        assert_eq!(BoxSize::infer_from_memory(128), BoxSize::Micro);
        assert_eq!(BoxSize::infer_from_memory(256), BoxSize::Small);
        assert_eq!(BoxSize::infer_from_memory(512), BoxSize::Medium);
        assert_eq!(BoxSize::infer_from_memory(1024), BoxSize::Large);
        assert_eq!(BoxSize::infer_from_memory(2048), BoxSize::Xlarge);
        assert_eq!(BoxSize::infer_from_memory(4096), BoxSize::Xxlarge);
        assert_eq!(BoxSize::infer_from_memory(8192), BoxSize::X4large);
    }
}

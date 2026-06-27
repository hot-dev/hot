#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct SemverCore {
    major: u64,
    minor: u64,
    patch: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum VersionRelation {
    Missing,
    NonSemantic,
    Clean,
    OlderMinor,
    Newer,
    MajorMismatch,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeVersionWarning {
    pub build_engine_version: String,
    pub build_hot_std_version: Option<String>,
    pub runtime_version: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeVersionCompatibility {
    pub compatible: bool,
    pub warning: Option<RuntimeVersionWarning>,
    pub error: Option<String>,
}

pub fn current_runtime_version() -> &'static str {
    crate::build_info::VERSION
}

fn parse_semver_core(version: &str) -> Option<SemverCore> {
    let core = version
        .split_once(['-', '+'])
        .map(|(core, _)| core)
        .unwrap_or(version);
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;

    if parts.next().is_some() {
        return None;
    }

    Some(SemverCore {
        major,
        minor,
        patch,
    })
}

fn relation_to_runtime(build_version: Option<&str>, runtime_version: &str) -> VersionRelation {
    let Some(build_version) = build_version else {
        return VersionRelation::Missing;
    };
    if build_version.is_empty() {
        return VersionRelation::Missing;
    }

    let Some(build) = parse_semver_core(build_version) else {
        return VersionRelation::NonSemantic;
    };
    let Some(runtime) = parse_semver_core(runtime_version) else {
        return VersionRelation::NonSemantic;
    };

    if build.major != runtime.major {
        return VersionRelation::MajorMismatch;
    }
    if build > runtime {
        return VersionRelation::Newer;
    }
    if build.minor < runtime.minor {
        return VersionRelation::OlderMinor;
    }

    VersionRelation::Clean
}

fn incompatibility_error(
    label: &str,
    build_version: &str,
    runtime_version: &str,
    relation: VersionRelation,
) -> Option<String> {
    match relation {
        VersionRelation::MajorMismatch => Some(format!(
            "{label} version {build_version} is incompatible with runtime {runtime_version}; major versions must match"
        )),
        VersionRelation::Newer => Some(format!(
            "{label} version {build_version} is incompatible with runtime {runtime_version}; runtime must be greater than or equal to the build version"
        )),
        _ => None,
    }
}

pub fn check_runtime_version_compatibility(
    build_engine_version: Option<&str>,
    build_hot_std_version: Option<&str>,
    runtime_version: &str,
) -> RuntimeVersionCompatibility {
    let engine_relation = relation_to_runtime(build_engine_version, runtime_version);
    if let Some(build_engine_version) = build_engine_version
        && let Some(error) = incompatibility_error(
            "Bundle engine",
            build_engine_version,
            runtime_version,
            engine_relation.clone(),
        )
    {
        return RuntimeVersionCompatibility {
            compatible: false,
            warning: None,
            error: Some(error),
        };
    }

    let hot_std_relation = relation_to_runtime(build_hot_std_version, runtime_version);
    if let Some(build_hot_std_version) = build_hot_std_version
        && let Some(error) = incompatibility_error(
            "Bundle hot-std",
            build_hot_std_version,
            runtime_version,
            hot_std_relation.clone(),
        )
    {
        return RuntimeVersionCompatibility {
            compatible: false,
            warning: None,
            error: Some(error),
        };
    }

    let warn_for_engine = matches!(engine_relation, VersionRelation::OlderMinor);
    let warn_for_hot_std = matches!(hot_std_relation, VersionRelation::OlderMinor);
    let warning = if warn_for_engine || warn_for_hot_std {
        let engine_version = build_engine_version.unwrap_or("unknown").to_string();
        let hot_std_version = build_hot_std_version
            .filter(|version| *version != engine_version)
            .map(ToString::to_string);
        let message = match &hot_std_version {
            Some(hot_std_version) => format!(
                "This build was created with Hot {engine_version} and hot-std {hot_std_version}; it is running on runtime {runtime_version}. Redeploy to refresh it."
            ),
            None => format!(
                "This build was created with Hot {engine_version} and is running on runtime {runtime_version}. Redeploy to refresh it."
            ),
        };

        Some(RuntimeVersionWarning {
            build_engine_version: engine_version,
            build_hot_std_version: hot_std_version,
            runtime_version: runtime_version.to_string(),
            message,
        })
    } else {
        None
    };

    RuntimeVersionCompatibility {
        compatible: true,
        warning,
        error: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_and_older_patch_are_clean() {
        assert_eq!(
            check_runtime_version_compatibility(Some("2.4.2"), Some("2.4.2"), "2.4.2").warning,
            None
        );
        assert_eq!(
            check_runtime_version_compatibility(Some("2.4.1"), Some("2.4.1"), "2.4.2").warning,
            None
        );
    }

    #[test]
    fn older_minor_is_compatible_with_warning() {
        let result = check_runtime_version_compatibility(Some("2.3.7"), Some("2.3.7"), "2.4.2");
        assert!(result.compatible);
        assert!(result.error.is_none());
        assert_eq!(
            result
                .warning
                .as_ref()
                .map(|warning| warning.message.as_str()),
            Some(
                "This build was created with Hot 2.3.7 and is running on runtime 2.4.2. Redeploy to refresh it."
            )
        );
    }

    #[test]
    fn newer_patch_minor_and_major_mismatch_are_incompatible() {
        assert!(
            !check_runtime_version_compatibility(Some("2.4.3"), Some("2.4.3"), "2.4.2").compatible
        );
        assert!(
            !check_runtime_version_compatibility(Some("2.5.0"), Some("2.5.0"), "2.4.2").compatible
        );
        assert!(
            !check_runtime_version_compatibility(Some("3.0.0"), Some("3.0.0"), "2.4.2").compatible
        );
    }
}

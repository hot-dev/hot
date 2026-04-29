/// Version embedded at compile time from resources/version.txt
pub const VERSION: &str = env!("HOT_VERSION");

/// Git SHA embedded at compile time (full 40-character SHA)
pub const GIT_SHA: &str = env!("GIT_SHA");

/// Parse a semver version string into (major, minor, patch) components.
/// Returns None if the version string is invalid.
/// Handles versions with pre-release suffixes like "0.11.0-beta".
pub fn parse_semver(version: &str) -> Option<(u32, u32, u32)> {
    // Strip any pre-release suffix (e.g., "-beta", "-rc1")
    let base_version = version.split('-').next()?;
    let parts: Vec<&str> = base_version.split('.').collect();
    if parts.len() < 2 {
        return None;
    }

    let major = parts[0].parse().ok()?;
    let minor = parts[1].parse().ok()?;
    let patch = parts.get(2).and_then(|p| p.parse().ok()).unwrap_or(0);

    Some((major, minor, patch))
}

/// Check if the current Hot version meets a minimum version requirement.
/// Returns Ok(()) if the requirement is met, or Err with an error message.
pub fn check_min_version(required: &str) -> Result<(), String> {
    let current = parse_semver(VERSION).ok_or_else(|| {
        format!(
            "Failed to parse current Hot version '{}' as semver",
            VERSION
        )
    })?;

    let required_ver = parse_semver(required)
        .ok_or_else(|| format!("Failed to parse required version '{}' as semver", required))?;

    if current >= required_ver {
        Ok(())
    } else {
        Err(format!(
            "Hot version {} is required, but you are running {}",
            required, VERSION
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_semver() {
        assert_eq!(parse_semver("0.11.0"), Some((0, 11, 0)));
        assert_eq!(parse_semver("1.2.3"), Some((1, 2, 3)));
        assert_eq!(parse_semver("0.11.0-beta"), Some((0, 11, 0)));
        assert_eq!(parse_semver("1.0"), Some((1, 0, 0)));
        assert_eq!(parse_semver("invalid"), None);
    }

    #[test]
    fn test_check_min_version() {
        // These tests depend on the actual VERSION constant
        // In practice, the current version should always pass against itself
        let result = check_min_version(VERSION);
        assert!(result.is_ok());
    }
}

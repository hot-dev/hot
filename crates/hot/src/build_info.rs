/// Version embedded at compile time from resources/version.txt
pub const VERSION: &str = env!("HOT_VERSION");

/// Git SHA embedded at compile time (full 40-character SHA)
pub const GIT_SHA: &str = env!("GIT_SHA");

/// Fingerprint of the compiler/runtime source used for this build.
pub const BUILD_FINGERPRINT: &str = env!("HOT_BUILD_FINGERPRINT");

pub use hot_lang::version::{check_min_version, parse_semver};

#[cfg(test)]
mod tests {
    #[test]
    fn hot_and_hot_lang_report_the_same_version() {
        assert_eq!(super::VERSION, hot_lang::version::VERSION);
    }
}

/// Version embedded at compile time from resources/version.txt
pub const VERSION: &str = env!("HOT_VERSION");

/// Git SHA embedded at compile time (full 40-character SHA)
pub const GIT_SHA: &str = env!("GIT_SHA");

/// Get short git SHA (7 characters, matching GitHub's standard)
pub fn git_sha_short() -> &'static str {
    &GIT_SHA[..7.min(GIT_SHA.len())]
}

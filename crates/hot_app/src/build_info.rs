use once_cell::sync::Lazy;
use std::time::SystemTime;

/// Version embedded at compile time from resources/version.txt
pub const VERSION: &str = env!("HOT_VERSION");

/// Git SHA embedded at compile time (full 40-character SHA)
pub const GIT_SHA: &str = env!("GIT_SHA");

/// Get short git SHA (7 characters, matching GitHub's standard)
pub fn git_sha_short() -> &'static str {
    &GIT_SHA[..7.min(GIT_SHA.len())]
}

/// Server start time
pub static START_TIME: Lazy<SystemTime> = Lazy::new(SystemTime::now);

/// Get the start time as an ISO 8601 formatted string
pub fn start_time_iso() -> String {
    START_TIME
        .duration_since(SystemTime::UNIX_EPOCH)
        .ok()
        .and_then(|d| chrono::DateTime::from_timestamp(d.as_secs() as i64, d.subsec_nanos()))
        .map(|dt| dt.to_rfc3339())
        .unwrap_or_else(|| "unknown".to_string())
}

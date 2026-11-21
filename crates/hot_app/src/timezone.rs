//! Timezone utilities for display formatting
//!
//! All data is stored and transferred in UTC. This module handles converting
//! UTC datetimes to the user's preferred display timezone.

use chrono::{DateTime, Utc};
use chrono_tz::Tz;

/// Default timezone when none is configured
pub const DEFAULT_TIMEZONE: &str = "UTC";

/// Resolve display timezone with priority: user > org > UTC default
pub fn resolve_display_timezone(user_timezone: Option<&str>, org_timezone: Option<&str>) -> String {
    user_timezone
        .filter(|s| !s.is_empty())
        .or(org_timezone.filter(|s| !s.is_empty()))
        .unwrap_or(DEFAULT_TIMEZONE)
        .to_string()
}

/// Parse a timezone string into a chrono-tz Tz, falling back to UTC if invalid
pub fn parse_timezone(timezone_str: &str) -> Tz {
    timezone_str.parse::<Tz>().unwrap_or(chrono_tz::UTC)
}

/// Convert a UTC datetime to the specified timezone
pub fn to_display_timezone(utc_datetime: &DateTime<Utc>, timezone_str: &str) -> DateTime<Tz> {
    let tz = parse_timezone(timezone_str);
    utc_datetime.with_timezone(&tz)
}

/// Format a UTC datetime in the specified timezone
pub fn format_in_timezone(
    utc_datetime: &DateTime<Utc>,
    timezone_str: &str,
    format: &str,
) -> String {
    let display_dt = to_display_timezone(utc_datetime, timezone_str);
    display_dt.format(format).to_string()
}

/// Get the timezone abbreviation for display (e.g., "EST", "PST", "UTC")
pub fn get_timezone_abbreviation(timezone_str: &str) -> String {
    let tz = parse_timezone(timezone_str);
    // Get current time to determine the correct abbreviation (handles DST)
    let now = Utc::now().with_timezone(&tz);
    now.format("%Z").to_string()
}

/// Get a display name for a timezone (e.g., "Eastern Time (US & Canada)")
/// Returns Some for known timezones, None for unknown ones
pub fn get_timezone_display_name(timezone_id: &str) -> Option<&'static str> {
    match timezone_id {
        "UTC" => Some("UTC (Coordinated Universal Time)"),
        // Americas
        "America/New_York" => Some("Eastern Time (US & Canada)"),
        "America/Chicago" => Some("Central Time (US & Canada)"),
        "America/Denver" => Some("Mountain Time (US & Canada)"),
        "America/Los_Angeles" => Some("Pacific Time (US & Canada)"),
        "America/Anchorage" => Some("Alaska Time"),
        "America/Phoenix" => Some("Arizona (No DST)"),
        "America/Toronto" => Some("Eastern Time (Canada)"),
        "America/Vancouver" => Some("Pacific Time (Canada)"),
        "America/Mexico_City" => Some("Mexico City"),
        "America/Sao_Paulo" => Some("São Paulo, Brazil"),
        "America/Buenos_Aires" => Some("Buenos Aires, Argentina"),
        // Europe
        "Europe/London" => Some("London (GMT/BST)"),
        "Europe/Paris" => Some("Paris, Berlin, Rome (CET)"),
        "Europe/Berlin" => Some("Berlin, Frankfurt (CET)"),
        "Europe/Amsterdam" => Some("Amsterdam, Netherlands"),
        "Europe/Madrid" => Some("Madrid, Spain"),
        "Europe/Rome" => Some("Rome, Italy"),
        "Europe/Zurich" => Some("Zurich, Switzerland"),
        "Europe/Stockholm" => Some("Stockholm, Sweden"),
        "Europe/Moscow" => Some("Moscow, Russia"),
        "Europe/Istanbul" => Some("Istanbul, Turkey"),
        // Asia
        "Asia/Dubai" => Some("Dubai, UAE"),
        "Asia/Kolkata" => Some("India Standard Time"),
        "Asia/Singapore" => Some("Singapore"),
        "Asia/Hong_Kong" => Some("Hong Kong"),
        "Asia/Shanghai" => Some("Beijing, Shanghai (CST)"),
        "Asia/Tokyo" => Some("Tokyo, Japan (JST)"),
        "Asia/Seoul" => Some("Seoul, South Korea"),
        "Asia/Bangkok" => Some("Bangkok, Thailand"),
        "Asia/Jakarta" => Some("Jakarta, Indonesia"),
        // Oceania
        "Australia/Sydney" => Some("Sydney, Australia (AEST)"),
        "Australia/Melbourne" => Some("Melbourne, Australia"),
        "Australia/Brisbane" => Some("Brisbane (No DST)"),
        "Australia/Perth" => Some("Perth, Australia (AWST)"),
        "Pacific/Auckland" => Some("Auckland, New Zealand"),
        "Pacific/Honolulu" => Some("Hawaii (No DST)"),
        // Africa
        "Africa/Johannesburg" => Some("Johannesburg, South Africa"),
        "Africa/Cairo" => Some("Cairo, Egypt"),
        "Africa/Lagos" => Some("Lagos, Nigeria"),
        // Unknown
        _ => None,
    }
}

/// Check if a timezone string is valid
pub fn is_valid_timezone(timezone_str: &str) -> bool {
    timezone_str.parse::<Tz>().is_ok()
}

/// Get the current UTC offset for a timezone as SQLite-compatible modifiers
/// Returns a string for use in SQLite datetime()
///
/// Uses minute precision so half-hour and 45-minute zones (e.g. Asia/Kolkata,
/// Australia/Adelaide, Asia/Kathmandu) align accurately with Rust bucket labels.
///
/// Examples:
/// - "America/New_York" in winter → "-300 minutes"
/// - "America/New_York" in summer → "-240 minutes"
/// - "Asia/Kolkata" → "+330 minutes"
/// - "UTC" → "+0 minutes"
pub fn get_sqlite_offset_modifier(timezone_str: &str) -> String {
    let offset_minutes = get_utc_offset_seconds(timezone_str) / 60;

    // SQLite datetime() modifier format
    if offset_minutes >= 0 {
        format!("+{} minutes", offset_minutes)
    } else {
        format!("{} minutes", offset_minutes)
    }
}

/// Get the UTC offset in seconds for a timezone (for calculations)
pub fn get_utc_offset_seconds(timezone_str: &str) -> i32 {
    use chrono::Offset;
    let tz = parse_timezone(timezone_str);
    let now_in_tz = Utc::now().with_timezone(&tz);
    now_in_tz.offset().fix().local_minus_utc()
}

/// Get the UTC offset as a string like "+05:00" or "-04:00" for display
pub fn get_utc_offset_string(timezone_str: &str) -> String {
    let offset_seconds = get_utc_offset_seconds(timezone_str);
    let hours = offset_seconds / 3600;
    let minutes = (offset_seconds.abs() % 3600) / 60;

    if hours >= 0 {
        format!("+{:02}:{:02}", hours, minutes)
    } else {
        format!("{:03}:{:02}", hours, minutes)
    }
}

// ============================================================================
// SQL Helpers for timezone-aware date bucketing
// ============================================================================

/// Generate Postgres SQL for timezone-aware date truncation
///
/// The result is cast back to TIMESTAMPTZ so sqlx can decode it as DateTime<Utc>.
/// Example output for "day" and "America/New_York":
/// `(DATE_TRUNC('day', r.start_time AT TIME ZONE 'America/New_York') AT TIME ZONE 'America/New_York')`
pub fn postgres_date_trunc(time_unit: &str, column: &str, timezone: &str) -> String {
    let trunc_unit = match time_unit {
        "hour" => "hour",
        "month" => "month",
        _ => "day",
    };
    let timezone = parse_timezone(timezone).name();

    // DATE_TRUNC with AT TIME ZONE returns a TIMESTAMP (without timezone)
    // We need to convert it back to TIMESTAMPTZ by applying the timezone again
    format!(
        "(DATE_TRUNC('{}', {} AT TIME ZONE '{}') AT TIME ZONE '{}')",
        trunc_unit, column, timezone, timezone
    )
}

/// Generate Postgres date format pattern for a time unit
pub fn postgres_date_format(time_unit: &str) -> &'static str {
    match time_unit {
        "hour" => "%Y-%m-%d %H:00",
        "month" => "%Y-%m",
        _ => "%Y-%m-%d",
    }
}

/// Generate SQLite SQL for timezone-aware date bucketing
/// Uses the current offset for the timezone (accurate for recent data)
///
/// Example output for "day" and "America/New_York" (currently UTC-5):
/// `strftime('%Y-%m-%d', datetime(r.start_time, '-5 hours'))`
pub fn sqlite_date_bucket(time_unit: &str, column: &str, timezone: &str) -> String {
    let format_str = match time_unit {
        "hour" => "%Y-%m-%d %H:00",
        "month" => "%Y-%m",
        _ => "%Y-%m-%d",
    };

    let offset_modifier = get_sqlite_offset_modifier(timezone);

    format!(
        "strftime('{}', datetime({}, '{}'))",
        format_str, column, offset_modifier
    )
}

/// Generate SQLite date format pattern for a time unit
pub fn sqlite_date_format(time_unit: &str) -> &'static str {
    match time_unit {
        "hour" => "%Y-%m-%d %H:00",
        "month" => "%Y-%m",
        _ => "%Y-%m-%d",
    }
}

// ============================================================================
// Time bucket generation for complete timeline graphs
// ============================================================================

/// Generate all time bucket labels from (now - days) to now, inclusive of the current partial bucket.
/// This ensures graphs show zeros for periods with no activity rather than gaps.
///
/// # Arguments
/// * `time_unit` - "hour", "day", or "month"
/// * `days` - Number of days to look back (ignored for "all" queries)
/// * `timezone` - Display timezone for formatting bucket labels
///
/// # Returns
/// Vec of formatted date strings representing each bucket, in chronological order
pub fn generate_time_buckets(time_unit: &str, days: i64, timezone: &str) -> Vec<String> {
    generate_time_buckets_at(time_unit, days, timezone, Utc::now())
}

fn truncate_to_hour_instant(dt: DateTime<Tz>) -> DateTime<Tz> {
    use chrono::{Duration, Timelike};

    dt - Duration::minutes(i64::from(dt.minute()))
        - Duration::seconds(i64::from(dt.second()))
        - Duration::nanoseconds(i64::from(dt.nanosecond()))
}

pub fn generate_time_buckets_at(
    time_unit: &str,
    days: i64,
    timezone: &str,
    now_utc: DateTime<Utc>,
) -> Vec<String> {
    use chrono::{Datelike, Duration};

    let tz = parse_timezone(timezone);
    let now_local = now_utc.with_timezone(&tz);

    // Calculate start time (days ago) and truncate to bucket boundary
    let start_utc = now_utc - Duration::days(days);
    let start_local = start_utc.with_timezone(&tz);

    let format_str = match time_unit {
        "hour" => "%Y-%m-%d %H:00",
        "month" => "%Y-%m",
        _ => "%Y-%m-%d", // day
    };

    let mut buckets = Vec::new();

    match time_unit {
        "hour" => {
            // Subtract along the instant timeline rather than using local wall-clock
            // `.with_minute(0)` so ambiguous fall-back hours and half-hour DST jumps
            // never panic. Deduplicate consecutive repeated local hours so bucket
            // labels match SQL's merged local-hour grouping.
            let mut current = truncate_to_hour_instant(start_local);
            let end = truncate_to_hour_instant(now_local);

            while current <= end {
                buckets.push(current.format(format_str).to_string());
                current += Duration::hours(1);
            }
            // Ensure the current partial hour is always present (e.g. during a
            // 30-minute spring-forward like Australia/Lord_Howe where subtracting
            // local minutes can step back across the transition).
            buckets.push(now_local.format(format_str).to_string());
            buckets.dedup();
        }
        "month" => {
            // Start from the first of the month of (now - days)
            let mut year = start_local.year();
            let mut month = start_local.month();
            let end_year = now_local.year();
            let end_month = now_local.month();

            loop {
                let bucket_str = format!("{:04}-{:02}", year, month);
                buckets.push(bucket_str);

                if year == end_year && month == end_month {
                    break;
                }

                month += 1;
                if month > 12 {
                    month = 1;
                    year += 1;
                }
            }
        }
        _ => {
            // Step calendar dates directly via NaiveDate so midnight-DST zones
            // (e.g. America/Havana, Asia/Beirut) never panic and 23/25-hour DST
            // days never duplicate or skip a date.
            let mut current_date = start_local.date_naive();
            let end_date = now_local.date_naive();

            while current_date <= end_date {
                buckets.push(current_date.format(format_str).to_string());
                match current_date.succ_opt() {
                    Some(next) => current_date = next,
                    None => break,
                }
            }
        }
    }

    buckets
}

/// Common timezones for display in settings dropdowns
/// Returns (timezone_id, display_name) tuples grouped by region
pub fn common_timezones() -> Vec<(&'static str, &'static str, &'static str)> {
    vec![
        // (region, timezone_id, display_name)
        ("UTC", "UTC", "UTC (Coordinated Universal Time)"),
        // Americas
        ("Americas", "America/New_York", "Eastern Time (US & Canada)"),
        ("Americas", "America/Chicago", "Central Time (US & Canada)"),
        ("Americas", "America/Denver", "Mountain Time (US & Canada)"),
        (
            "Americas",
            "America/Los_Angeles",
            "Pacific Time (US & Canada)",
        ),
        ("Americas", "America/Anchorage", "Alaska Time"),
        ("Americas", "America/Phoenix", "Arizona (No DST)"),
        ("Americas", "America/Toronto", "Eastern Time (Canada)"),
        ("Americas", "America/Vancouver", "Pacific Time (Canada)"),
        ("Americas", "America/Mexico_City", "Mexico City"),
        ("Americas", "America/Sao_Paulo", "São Paulo, Brazil"),
        (
            "Americas",
            "America/Buenos_Aires",
            "Buenos Aires, Argentina",
        ),
        // Europe
        ("Europe", "Europe/London", "London (GMT/BST)"),
        ("Europe", "Europe/Paris", "Paris, Berlin, Rome (CET)"),
        ("Europe", "Europe/Berlin", "Berlin, Frankfurt (CET)"),
        ("Europe", "Europe/Amsterdam", "Amsterdam, Netherlands"),
        ("Europe", "Europe/Madrid", "Madrid, Spain"),
        ("Europe", "Europe/Rome", "Rome, Italy"),
        ("Europe", "Europe/Zurich", "Zurich, Switzerland"),
        ("Europe", "Europe/Stockholm", "Stockholm, Sweden"),
        ("Europe", "Europe/Moscow", "Moscow, Russia"),
        ("Europe", "Europe/Istanbul", "Istanbul, Turkey"),
        // Asia
        ("Asia", "Asia/Dubai", "Dubai, UAE"),
        ("Asia", "Asia/Kolkata", "India Standard Time"),
        ("Asia", "Asia/Singapore", "Singapore"),
        ("Asia", "Asia/Hong_Kong", "Hong Kong"),
        ("Asia", "Asia/Shanghai", "Beijing, Shanghai (CST)"),
        ("Asia", "Asia/Tokyo", "Tokyo, Japan (JST)"),
        ("Asia", "Asia/Seoul", "Seoul, South Korea"),
        ("Asia", "Asia/Bangkok", "Bangkok, Thailand"),
        ("Asia", "Asia/Jakarta", "Jakarta, Indonesia"),
        // Oceania
        ("Oceania", "Australia/Sydney", "Sydney, Australia (AEST)"),
        ("Oceania", "Australia/Melbourne", "Melbourne, Australia"),
        ("Oceania", "Australia/Brisbane", "Brisbane (No DST)"),
        ("Oceania", "Australia/Perth", "Perth, Australia (AWST)"),
        ("Oceania", "Pacific/Auckland", "Auckland, New Zealand"),
        ("Oceania", "Pacific/Honolulu", "Hawaii (No DST)"),
        // Africa
        (
            "Africa",
            "Africa/Johannesburg",
            "Johannesburg, South Africa",
        ),
        ("Africa", "Africa/Cairo", "Cairo, Egypt"),
        ("Africa", "Africa/Lagos", "Lagos, Nigeria"),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_display_timezone() {
        // User timezone takes priority
        assert_eq!(
            resolve_display_timezone(Some("America/New_York"), Some("Europe/London")),
            "America/New_York"
        );

        // Falls back to org timezone
        assert_eq!(
            resolve_display_timezone(None, Some("Europe/London")),
            "Europe/London"
        );

        // Falls back to UTC
        assert_eq!(resolve_display_timezone(None, None), "UTC");

        // Empty strings are treated as not set
        assert_eq!(
            resolve_display_timezone(Some(""), Some("Europe/London")),
            "Europe/London"
        );
    }

    #[test]
    fn test_is_valid_timezone() {
        assert!(is_valid_timezone("America/New_York"));
        assert!(is_valid_timezone("UTC"));
        assert!(is_valid_timezone("Europe/London"));
        assert!(!is_valid_timezone("Invalid/Timezone"));
        assert!(!is_valid_timezone(""));
    }

    #[test]
    fn test_parse_timezone_fallback() {
        // Valid timezone
        assert_eq!(
            parse_timezone("America/New_York").name(),
            "America/New_York"
        );

        // Invalid timezone falls back to UTC
        assert_eq!(parse_timezone("Invalid").name(), "UTC");
    }

    #[test]
    fn test_hour_buckets_chicago_fall_back_no_panic_and_deduped() {
        // 2026-11-01 in America/Chicago: 01:00..01:59 occurs twice (CDT -05:00 then CST -06:00)
        for iso in ["2026-11-01T06:30:00Z", "2026-11-01T07:30:00Z"] {
            let now: DateTime<Utc> = iso.parse().unwrap();
            let buckets = generate_time_buckets_at("hour", 1, "America/Chicago", now);
            assert!(!buckets.is_empty());
            assert_eq!(buckets.last().map(String::as_str), Some("2026-11-01 01:00"));
            // Ambiguous 01:00 hour must not appear twice consecutively
            for pair in buckets.windows(2) {
                assert_ne!(pair[0], pair[1], "duplicate consecutive hour for {iso}");
            }
        }
    }

    #[test]
    fn test_hour_and_day_buckets_chicago_spring_forward() {
        // 2026-03-08 in America/Chicago: 02:00 jumps to 03:00 (23-hour day)
        let now: DateTime<Utc> = "2026-03-08T12:30:00Z".parse().unwrap();
        let hour_buckets = generate_time_buckets_at("hour", 1, "America/Chicago", now);
        assert!(hour_buckets.contains(&"2026-03-08 01:00".to_string()));
        assert!(!hour_buckets.contains(&"2026-03-08 02:00".to_string()));
        assert!(hour_buckets.contains(&"2026-03-08 03:00".to_string()));

        let day_buckets = generate_time_buckets_at("day", 3, "America/Chicago", now);
        assert_eq!(
            day_buckets,
            vec!["2026-03-05", "2026-03-06", "2026-03-07", "2026-03-08"]
        );
    }

    #[test]
    fn test_day_buckets_midnight_dst_zones_do_not_panic() {
        // America/Havana springs forward at 00:00 -> 01:00 on 2026-03-08
        let havana_now: DateTime<Utc> = "2026-03-08T12:00:00Z".parse().unwrap();
        let havana_days = generate_time_buckets_at("day", 2, "America/Havana", havana_now);
        assert_eq!(havana_days, vec!["2026-03-06", "2026-03-07", "2026-03-08"]);

        // Asia/Beirut springs forward at 00:00 -> 01:00 on 2026-03-29
        let beirut_now: DateTime<Utc> = "2026-03-29T12:00:00Z".parse().unwrap();
        let beirut_days = generate_time_buckets_at("day", 2, "Asia/Beirut", beirut_now);
        assert_eq!(beirut_days, vec!["2026-03-27", "2026-03-28", "2026-03-29"]);
    }

    #[test]
    fn test_hour_buckets_australia_lord_howe_half_hour_dst() {
        // Australia/Lord_Howe springs forward 30 min at 2026-10-04 02:00 (+10:30) -> 02:30 (+11:00)
        // 2026-10-03T15:45:00Z is 2026-10-04 02:45+11:00
        let now: DateTime<Utc> = "2026-10-03T15:45:00Z".parse().unwrap();
        let buckets = generate_time_buckets_at("hour", 1, "Australia/Lord_Howe", now);
        assert!(!buckets.is_empty());
        assert_eq!(buckets.last().map(String::as_str), Some("2026-10-04 02:00"));
        for pair in buckets.windows(2) {
            assert_ne!(pair[0], pair[1]);
        }

        // Fall-back: 2026-04-05 02:00 (+11:00) -> 01:30 (+10:30)
        let fall_now: DateTime<Utc> = "2026-04-04T15:15:00Z".parse().unwrap();
        let fall_buckets = generate_time_buckets_at("hour", 1, "Australia/Lord_Howe", fall_now);
        assert!(!fall_buckets.is_empty());
        for pair in fall_buckets.windows(2) {
            assert_ne!(pair[0], pair[1]);
        }
    }

    #[test]
    fn test_sqlite_offset_modifier_minute_precision() {
        // Asia/Kolkata has no DST and is always UTC+05:30 (+330 minutes)
        assert_eq!(get_sqlite_offset_modifier("Asia/Kolkata"), "+330 minutes");
        assert_eq!(get_sqlite_offset_modifier("UTC"), "+0 minutes");
        // Asia/Kathmandu has no DST and is always UTC+05:45 (+345 minutes)
        assert_eq!(get_sqlite_offset_modifier("Asia/Kathmandu"), "+345 minutes");
        assert_eq!(
            sqlite_date_bucket("hour", "e.created_at", "Asia/Kolkata"),
            "strftime('%Y-%m-%d %H:00', datetime(e.created_at, '+330 minutes'))"
        );
    }
}

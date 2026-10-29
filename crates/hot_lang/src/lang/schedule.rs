//! The recurring schedule forms `meta {schedule: ...}` accepts: cron
//! expressions (with croner's shortcut steps) and English schedules.

use chrono::Utc;

/// The recurring schedule forms `meta {schedule: ...}` accepts, for
/// diagnostics on a schedule that does not parse.
pub const RECURRING_SCHEDULE_FORMS: &str = "Use a cron expression such as \"0 2 * * *\" or \"0 30 9 * * MON\" (with seconds), a shortcut such as \"@daily\" or \"@hourly\", or an English schedule such as \"every 5 minutes\", \"every day at 9am\" or \"every Monday at 2 PM\"";

/// The shortest interval a recurring schedule may have, whatever the conf says.
pub const TECHNICAL_MIN_RECURRING_INTERVAL_SECS: i64 = 1;

/// The minimum interval between a recurring schedule's runs that a conf
/// allows (`schedule.min-interval-seconds`, never below one second).
pub fn min_recurring_interval_secs(conf: &crate::val::Val) -> i64 {
    conf.get_int_or_default(
        "schedule.min-interval-seconds",
        TECHNICAL_MIN_RECURRING_INTERVAL_SECS,
    )
    .max(TECHNICAL_MIN_RECURRING_INTERVAL_SECS)
}

fn contains_unsupported_subsecond_unit(expr: &str) -> bool {
    let lower = expr.to_ascii_lowercase();
    [
        "millisecond",
        "milliseconds",
        "msec",
        "msecs",
        "ms",
        "microsecond",
        "microseconds",
        "usec",
        "usecs",
        "nanosecond",
        "nanoseconds",
        "nsec",
        "nsecs",
    ]
    .iter()
    .any(|unit| {
        lower
            .split(|c: char| !c.is_ascii_alphanumeric())
            .any(|t| t == *unit)
    })
}

/// Parses a cron expression the way croner 3 did. croner 4 rejects shortcut
/// steps such as `0/5` unless `sloppy_ranges` is on, and saved schedules and
/// english-to-cron output use them: "every 5 minutes" becomes `0 0/5 * * * ? *`.
pub fn parse_cron(expr: &str) -> Result<croner::Cron, croner::errors::CronError> {
    croner::parser::CronParser::builder()
        .sloppy_ranges(true)
        .build()
        .parse(expr)
}

pub fn normalize_recurring_schedule_expression(expr: &str) -> Result<String, String> {
    let expr = expr.trim();

    if contains_unsupported_subsecond_unit(expr) {
        return Err(
            "Sub-second schedule intervals are not supported; use seconds or a slower interval"
                .to_string(),
        );
    }

    if parse_cron(expr).is_ok() {
        return Ok(expr.to_string());
    }

    match english_to_cron::str_cron_syntax(expr) {
        Ok(converted_cron) => {
            parse_cron(&converted_cron).map_err(|e| {
                format!(
                    "English expression '{}' converted to invalid cron '{}': {}",
                    expr, converted_cron, e
                )
            })?;
            Ok(converted_cron)
        }
        Err(_) => Err(format!(
            "Could not parse '{}' as a cron expression or a supported English schedule",
            expr
        )),
    }
}

#[derive(Debug, Clone)]
pub enum ScheduleIntervalValidation {
    /// The schedule is not a cron expression or a supported English schedule.
    Unparseable { original: String, reason: String },
    /// The schedule parses but has no upcoming run to measure.
    NoOccurrence {
        original: String,
        normalized_cron: String,
        reason: String,
    },
    TooFrequent {
        original: String,
        normalized_cron: String,
        observed_interval_secs: i64,
        required_interval_secs: i64,
    },
}

impl ScheduleIntervalValidation {
    pub fn message(&self) -> String {
        match self {
            Self::Unparseable { reason, .. } => {
                format!("{}. {}", reason, RECURRING_SCHEDULE_FORMS)
            }
            Self::NoOccurrence {
                original,
                normalized_cron,
                reason,
            } => format!(
                "Schedule '{}' has no upcoming run (normalized cron: '{}'): {}",
                original, normalized_cron, reason
            ),
            Self::TooFrequent {
                original,
                normalized_cron,
                observed_interval_secs,
                required_interval_secs,
            } => format!(
                "Schedule '{}' runs every {} second(s), below the minimum of {} second(s) (normalized cron: '{}')",
                original, observed_interval_secs, required_interval_secs, normalized_cron
            ),
        }
    }
}

pub fn validate_recurring_schedule_interval(
    expr: &str,
    min_interval_secs: i64,
) -> Result<(), ScheduleIntervalValidation> {
    let required = min_interval_secs.max(TECHNICAL_MIN_RECURRING_INTERVAL_SECS);
    let normalized = normalize_recurring_schedule_expression(expr).map_err(|reason| {
        ScheduleIntervalValidation::Unparseable {
            original: expr.to_string(),
            reason,
        }
    })?;

    let cron = parse_cron(&normalized).map_err(|e| ScheduleIntervalValidation::Unparseable {
        original: expr.to_string(),
        reason: format!("Could not parse '{}' as cron: {}", normalized, e),
    })?;

    let no_occurrence = |e: croner::errors::CronError| ScheduleIntervalValidation::NoOccurrence {
        original: expr.to_string(),
        normalized_cron: normalized.clone(),
        reason: e.to_string(),
    };
    let mut prev = cron
        .find_next_occurrence(&Utc::now(), false)
        .map_err(no_occurrence)?;

    let mut min_gap = i64::MAX;
    for _ in 0..64 {
        let next = cron
            .find_next_occurrence(&prev, false)
            .map_err(no_occurrence)?;
        let gap = (next - prev).num_seconds();
        if gap > 0 {
            min_gap = min_gap.min(gap);
        }
        prev = next;
    }

    if min_gap < required {
        Err(ScheduleIntervalValidation::TooFrequent {
            original: expr.to_string(),
            normalized_cron: normalized,
            observed_interval_secs: min_gap,
            required_interval_secs: required,
        })
    } else {
        Ok(())
    }
}

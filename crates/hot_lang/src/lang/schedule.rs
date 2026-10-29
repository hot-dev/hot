//! The recurring schedule forms `meta {schedule: ...}` accepts: cron
//! expressions (with croner's shortcut steps) and English schedules.

/// The recurring schedule forms `meta {schedule: ...}` accepts, for
/// diagnostics on a schedule that does not parse.
pub const RECURRING_SCHEDULE_FORMS: &str = "Use a cron expression such as \"0 2 * * *\" or \"0 30 9 * * MON\" (with seconds), a shortcut such as \"@daily\" or \"@hourly\", or an English schedule such as \"every 5 minutes\", \"every day at 9am\" or \"every Monday at 2 PM\"";

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

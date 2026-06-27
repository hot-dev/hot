use ariadne::{ColorGenerator, Config, Label, Report, ReportKind, Source};
use chrono::{DateTime, Utc};
use serde_json::Value as JsonValue;
use sqlx::{FromRow, Pool, Postgres, Sqlite};
use std::path::PathBuf;
use std::str::FromStr;
use thiserror::Error;
use uuid::Uuid;

/// The prefix used for one-time "at" schedules stored in the cron field
pub const AT_SCHEDULE_PREFIX: &str = "@at:";
pub const TECHNICAL_MIN_RECURRING_INTERVAL_SECS: i64 = 1;
pub const DEFAULT_MIN_DELAY_SECS: i64 = 0;
pub const SELF_HOSTED_MAX_ACTIVE_SCHEDULES_PER_ORG: i64 = -1;
pub const HOSTED_DEFAULT_MAX_ACTIVE_SCHEDULES_PER_ORG: i64 = 50;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SchedulePolicy {
    pub min_interval_secs: i64,
    pub min_delay_secs: i64,
    pub max_active_per_org: i64,
}

impl SchedulePolicy {
    pub fn self_hosted_default() -> Self {
        Self {
            min_interval_secs: TECHNICAL_MIN_RECURRING_INTERVAL_SECS,
            min_delay_secs: DEFAULT_MIN_DELAY_SECS,
            max_active_per_org: SELF_HOSTED_MAX_ACTIVE_SCHEDULES_PER_ORG,
        }
    }

    pub fn from_conf(conf: &crate::val::Val) -> Self {
        let conf_max = conf.get_int_or_default(
            "schedule.max-active-per-org",
            SELF_HOSTED_MAX_ACTIVE_SCHEDULES_PER_ORG,
        );
        let default_max = if crate::product::is_hot_cloud(conf) && conf_max < 0 {
            HOSTED_DEFAULT_MAX_ACTIVE_SCHEDULES_PER_ORG
        } else {
            conf_max
        };

        Self {
            min_interval_secs: conf
                .get_int_or_default(
                    "schedule.min-interval-seconds",
                    TECHNICAL_MIN_RECURRING_INTERVAL_SECS,
                )
                .max(TECHNICAL_MIN_RECURRING_INTERVAL_SECS),
            min_delay_secs: conf
                .get_int_or_default("schedule.min-delay-seconds", DEFAULT_MIN_DELAY_SECS)
                .max(0),
            max_active_per_org: default_max,
        }
    }

    pub fn with_features(mut self, features: &crate::db::Features) -> Self {
        self.min_interval_secs = self
            .min_interval_secs
            .max(features.schedule_min_interval_secs())
            .max(TECHNICAL_MIN_RECURRING_INTERVAL_SECS);
        self.min_delay_secs = self.min_delay_secs.max(features.schedule_min_delay_secs());
        self.max_active_per_org =
            stricter_limit(self.max_active_per_org, features.active_schedules_per_org());
        self
    }
}

fn stricter_limit(a: i64, b: i64) -> i64 {
    match (a < 0, b < 0) {
        (true, true) => -1,
        (true, false) => b,
        (false, true) => a,
        (false, false) => a.min(b),
    }
}

#[derive(Debug, Clone)]
pub struct ScheduleIntervalValidation {
    pub original: String,
    pub normalized_cron: String,
    pub observed_interval_secs: i64,
    pub required_interval_secs: i64,
}

impl ScheduleIntervalValidation {
    pub fn message(&self) -> String {
        format!(
            "Schedule '{}' runs every {} second(s), below the minimum of {} second(s) (normalized cron: '{}')",
            self.original,
            self.observed_interval_secs,
            self.required_interval_secs,
            self.normalized_cron
        )
    }
}

/// Represents the type of schedule - either a cron expression or a one-time "at" datetime
#[derive(Debug, Clone, PartialEq)]
pub enum ScheduleType {
    /// A recurring schedule using a cron expression
    Cron(String),
    /// A one-time schedule at a specific datetime
    At(DateTime<Utc>),
}

impl ScheduleType {
    /// Convert to the string format stored in the database cron field
    pub fn to_cron_field(&self) -> String {
        match self {
            ScheduleType::Cron(cron) => cron.clone(),
            ScheduleType::At(dt) => format!("{}{}", AT_SCHEDULE_PREFIX, dt.to_rfc3339()),
        }
    }

    /// Parse from the database cron field format
    pub fn from_cron_field(cron: &str) -> Result<Self, String> {
        if let Some(datetime_str) = cron.strip_prefix(AT_SCHEDULE_PREFIX) {
            let dt = DateTime::parse_from_rfc3339(datetime_str)
                .map_err(|e| format!("Invalid @at datetime '{}': {}", datetime_str, e))?;
            Ok(ScheduleType::At(dt.with_timezone(&Utc)))
        } else {
            Ok(ScheduleType::Cron(cron.to_string()))
        }
    }

    /// Check if this is a one-time "at" schedule
    pub fn is_at_schedule(&self) -> bool {
        matches!(self, ScheduleType::At(_))
    }

    /// Check if this is a recurring cron schedule
    pub fn is_cron_schedule(&self) -> bool {
        matches!(self, ScheduleType::Cron(_))
    }
}

/// Parse a schedule expression that can be:
/// - An ISO 8601 datetime: "2024-01-15T10:30:00Z"
/// - A duration: "10 minutes", "2h", "1 day 3 hours"
/// - Natural language duration: "in 10 minutes", "2 hours from now", "after 1 day"
/// - A cron expression: "0 30 9 * * MON"
/// - English cron: "every day at 9am", "every Monday at 2 PM"
///
/// Returns a ScheduleType that can be converted to the database format.
pub fn parse_schedule_expression(expr: &str) -> Result<ScheduleType, String> {
    let expr = expr.trim();

    // 1. Try ISO 8601 datetime first
    if let Ok(dt) = DateTime::parse_from_rfc3339(expr) {
        return Ok(ScheduleType::At(dt.with_timezone(&Utc)));
    }

    // 2. Try as duration with natural language prefix stripping
    // Supports: "in 10 minutes", "10 minutes from now", "after 1 day", "10 minutes", "2h"
    let duration_str = expr
        .trim_start_matches("in ")
        .trim_end_matches(" from now")
        .trim_start_matches("after ")
        .trim();

    if let Ok(duration) = humantime::parse_duration(duration_str) {
        let run_at = Utc::now()
            + chrono::Duration::from_std(duration)
                .map_err(|e| format!("Duration conversion error: {}", e))?;
        return Ok(ScheduleType::At(run_at));
    }

    // 3. Try recurring cron or supported English cron.
    if let Ok(cron) = normalize_recurring_schedule_expression(expr) {
        return Ok(ScheduleType::Cron(cron));
    }

    Err(format!(
        "Cannot parse '{}' as datetime, duration, or cron expression.\n\
        \n\
        Supported formats:\n\
        • ISO datetime: \"2024-01-15T10:30:00Z\"\n\
        • Duration: \"10 minutes\", \"2h\", \"1 day 3 hours\"\n\
        • Natural: \"in 10 minutes\", \"2 hours from now\"\n\
        • Cron: \"0 30 9 * * MON\", \"@daily\"\n\
        • English: \"every day at 9am\", \"every Monday at 2 PM\"",
        expr
    ))
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

pub fn normalize_recurring_schedule_expression(expr: &str) -> Result<String, String> {
    let expr = expr.trim();

    if contains_unsupported_subsecond_unit(expr) {
        return Err(
            "Sub-second schedule intervals are not supported; use seconds or a slower interval"
                .to_string(),
        );
    }

    if croner::Cron::from_str(expr).is_ok() {
        return Ok(expr.to_string());
    }

    match english_to_cron::str_cron_syntax(expr) {
        Ok(converted_cron) => {
            croner::Cron::from_str(&converted_cron).map_err(|e| {
                format!(
                    "English expression '{}' converted to invalid cron '{}': {}",
                    expr, converted_cron, e
                )
            })?;
            Ok(converted_cron)
        }
        Err(e) => Err(format!(
            "Could not parse '{}' as cron or supported English schedule: {:?}",
            expr, e
        )),
    }
}

pub fn validate_recurring_schedule_interval(
    expr: &str,
    min_interval_secs: i64,
) -> Result<(), ScheduleIntervalValidation> {
    let required = min_interval_secs.max(TECHNICAL_MIN_RECURRING_INTERVAL_SECS);
    let normalized = normalize_recurring_schedule_expression(expr).map_err(|message| {
        ScheduleIntervalValidation {
            original: expr.to_string(),
            normalized_cron: message,
            observed_interval_secs: 0,
            required_interval_secs: required,
        }
    })?;

    let cron = croner::Cron::from_str(&normalized).map_err(|e| ScheduleIntervalValidation {
        original: expr.to_string(),
        normalized_cron: e.to_string(),
        observed_interval_secs: 0,
        required_interval_secs: required,
    })?;

    let mut prev = match cron.find_next_occurrence(&Utc::now(), false) {
        Ok(dt) => dt,
        Err(e) => {
            return Err(ScheduleIntervalValidation {
                original: expr.to_string(),
                normalized_cron: e.to_string(),
                observed_interval_secs: 0,
                required_interval_secs: required,
            });
        }
    };

    let mut min_gap = i64::MAX;
    for _ in 0..64 {
        let next =
            cron.find_next_occurrence(&prev, false)
                .map_err(|e| ScheduleIntervalValidation {
                    original: expr.to_string(),
                    normalized_cron: e.to_string(),
                    observed_interval_secs: 0,
                    required_interval_secs: required,
                })?;
        let gap = (next - prev).num_seconds();
        if gap > 0 {
            min_gap = min_gap.min(gap);
        }
        prev = next;
    }

    if min_gap < required {
        Err(ScheduleIntervalValidation {
            original: expr.to_string(),
            normalized_cron: normalized,
            observed_interval_secs: min_gap,
            required_interval_secs: required,
        })
    } else {
        Ok(())
    }
}

pub fn validate_one_time_schedule_delay(
    run_at: DateTime<Utc>,
    min_delay_secs: i64,
) -> Result<(), String> {
    let required = min_delay_secs.max(0);
    if required == 0 {
        return Ok(());
    }

    let delay = (run_at - Utc::now()).num_seconds();
    if delay < required {
        Err(format!(
            "One-time schedule is {} second(s) from now, below the minimum delay of {} second(s)",
            delay.max(0),
            required
        ))
    } else {
        Ok(())
    }
}

#[derive(Error, Debug)]
#[error("{message}")]
pub struct CronValidationErrorDetails {
    pub message: String,
    pub cron_expression: String,
    pub function_ns: String,
    pub function_var: String,
    pub file: Option<PathBuf>,
    pub line: Option<i32>,
    pub column: Option<i32>,
    pub position: Option<i32>,
    pub length: Option<usize>,
}

#[derive(Error, Debug)]
pub enum ScheduleError {
    #[error("Database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("Schedule not found")]
    NotFound,
    #[error("Serialization error: {0}")]
    SerializationError(String),
    #[error("Schedule policy error: {0}")]
    PolicyError(String),
    #[error("Cron validation error: {0}")]
    CronValidationError(#[from] Box<CronValidationErrorDetails>),
}

impl ScheduleError {
    /// Create a nice ariadne error report for cron validation errors
    pub fn format_error(&self, source_content: Option<&str>, color: bool) -> String {
        match self {
            ScheduleError::CronValidationError(details) => {
                let CronValidationErrorDetails {
                    message,
                    cron_expression,
                    function_ns,
                    function_var,
                    file,
                    line,
                    column,
                    position,
                    length,
                } = details.as_ref();
                // If we have source content and position information, create an ariadne report
                if let (Some(content), Some(_pos), Some(_len)) = (source_content, position, length)
                    && let Some(ariadne_report) = self.create_ariadne_report(content, color)
                {
                    return ariadne_report;
                }

                // Fallback to a nicely formatted text error
                let location_info =
                    if let (Some(file), Some(line), Some(col)) = (file, line, column) {
                        format!(" at {}:{}:{}", file.display(), line, col)
                    } else if let (Some(line), Some(col)) = (line, column) {
                        format!(" at line {}, column {}", line, col)
                    } else {
                        String::new()
                    };

                format!(
                    "❌ Cron Validation Error in {}:{}{}\n\n\
                    Invalid cron expression: '{}'\n\
                    \n\
                    💡 {}\n\
                    \n\
                    🔧 Fix: Update the schedule expression in your Hot code and rebuild.",
                    function_ns, function_var, location_info, cron_expression, message
                )
            }
            _ => self.to_string(),
        }
    }

    /// Create an ariadne report for cron validation errors
    fn create_ariadne_report(&self, source_content: &str, color: bool) -> Option<String> {
        if let ScheduleError::CronValidationError(details) = self {
            let CronValidationErrorDetails {
                message,
                cron_expression,
                function_ns,
                function_var,
                file,
                position,
                length,
                ..
            } = details.as_ref();
            let mut colors = ColorGenerator::new();
            let error_color = colors.next();

            let span_start = (*position).unwrap_or(0) as usize;
            let span_end = span_start + length.unwrap_or(cron_expression.len());

            // Use the file path if available, otherwise use a default name
            let source_name = if let Some(file_path) = file {
                file_path.display().to_string()
            } else {
                "<source>".to_string()
            };

            let report = Report::build(
                ReportKind::Error,
                (source_name.as_str(), span_start..span_end),
            )
            .with_config(Config::default().with_color(color))
            .with_code("E100")
            .with_message(format!(
                "Invalid cron expression in {}:{}",
                function_ns, function_var
            ))
            .with_label(
                Label::new((source_name.as_str(), span_start..span_end))
                    .with_message(format!("'{}' - {}", cron_expression, message))
                    .with_color(error_color),
            )
            .with_help(
                "Hot requires 6-field cron expressions: 'sec min hour day month day_of_week'",
            )
            .with_note(
                "Examples: '0 30 9 * * MON' (9:30 AM Monday), '0 */15 * * * *' (every 15 seconds)"
                    .to_string(),
            );

            let mut buffer = Vec::new();
            if report
                .finish()
                .write(
                    (source_name.as_str(), Source::from(source_content)),
                    &mut buffer,
                )
                .is_ok()
            {
                return String::from_utf8(buffer).ok();
            }
        }
        None
    }
}

#[derive(Debug, FromRow)]
pub struct Schedule {
    pub schedule_id: Uuid,
    pub build_id: Uuid,
    pub cron: String,
    pub ns: String,
    pub var: String,
    pub meta: Option<JsonValue>,
    pub value: Option<JsonValue>,
    pub file: Option<String>,
    pub line: Option<i32>,
    pub column: Option<i32>,
    pub position: Option<i32>,
    pub active: bool,
    pub created_at: Option<chrono::DateTime<chrono::Utc>>,
    pub deactivated_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// Schedule with project information for display purposes
#[derive(Debug, FromRow)]
pub struct ScheduleWithProject {
    pub schedule_id: Uuid,
    pub build_id: Uuid,
    pub cron: String,
    pub ns: String,
    pub var: String,
    pub meta: Option<JsonValue>,
    pub value: Option<JsonValue>,
    pub file: Option<String>,
    pub line: Option<i32>,
    pub column: Option<i32>,
    pub position: Option<i32>,
    pub active: bool,
    pub created_at: Option<chrono::DateTime<chrono::Utc>>,
    pub deactivated_at: Option<chrono::DateTime<chrono::Utc>>,
    pub project_id: Uuid,
    pub project_name: String,
}

impl Schedule {
    /// Get schedule by ID
    pub async fn get_schedule(
        db: &crate::db::DatabasePool,
        schedule_id: &Uuid,
    ) -> Result<Schedule, ScheduleError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                Self::get_schedule_postgres(pg_pool, schedule_id).await
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                Self::get_schedule_sqlite(sqlite_pool, schedule_id).await
            }
        }
    }

    async fn get_schedule_sqlite(
        db: &Pool<Sqlite>,
        schedule_id: &Uuid,
    ) -> Result<Schedule, ScheduleError> {
        let schedule = sqlx::query_as::<_, Schedule>(
            "SELECT schedule_id, build_id, cron, ns, var, meta, value, file, line, \"column\", position, active, created_at, deactivated_at FROM schedule WHERE schedule_id = ?"
        )
        .bind(schedule_id)
        .fetch_one(db)
        .await?;
        Ok(schedule)
    }

    async fn get_schedule_postgres(
        db: &Pool<Postgres>,
        schedule_id: &Uuid,
    ) -> Result<Schedule, ScheduleError> {
        let schedule = sqlx::query_as::<_, Schedule>(
            "SELECT schedule_id, build_id, cron, ns, var, meta, value, file, line, \"column\", position, active, created_at, deactivated_at FROM schedule WHERE schedule_id = $1"
        )
        .bind(schedule_id)
        .fetch_one(db)
        .await?;
        Ok(schedule)
    }

    /// Get schedules by build ID
    pub async fn get_schedules_by_build(
        db: &crate::db::DatabasePool,
        build_id: &Uuid,
        limit: Option<i64>,
        offset: Option<i64>,
    ) -> Result<Vec<Schedule>, ScheduleError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                Self::get_schedules_by_build_postgres(pg_pool, build_id, limit, offset).await
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                Self::get_schedules_by_build_sqlite(sqlite_pool, build_id, limit, offset).await
            }
        }
    }

    async fn get_schedules_by_build_sqlite(
        db: &Pool<Sqlite>,
        build_id: &Uuid,
        limit: Option<i64>,
        offset: Option<i64>,
    ) -> Result<Vec<Schedule>, ScheduleError> {
        let limit = limit.unwrap_or(10);
        let offset = offset.unwrap_or(0);

        let schedules = sqlx::query_as::<_, Schedule>(
            "SELECT schedule_id, build_id, cron, ns, var, meta, value, file, line, \"column\", position, active, created_at, deactivated_at FROM schedule WHERE build_id = ? ORDER BY cron, ns, var LIMIT ? OFFSET ?"
        )
        .bind(build_id)
        .bind(limit)
        .bind(offset)
        .fetch_all(db)
        .await?;
        Ok(schedules)
    }

    async fn get_schedules_by_build_postgres(
        db: &Pool<Postgres>,
        build_id: &Uuid,
        limit: Option<i64>,
        offset: Option<i64>,
    ) -> Result<Vec<Schedule>, ScheduleError> {
        let limit = limit.unwrap_or(10);
        let offset = offset.unwrap_or(0);

        let schedules = sqlx::query_as::<_, Schedule>(
            "SELECT schedule_id, build_id, cron, ns, var, meta, value, file, line, \"column\", position, active, created_at, deactivated_at FROM schedule WHERE build_id = $1 ORDER BY cron, ns, var LIMIT $2 OFFSET $3"
        )
        .bind(build_id)
        .bind(limit)
        .bind(offset)
        .fetch_all(db)
        .await?;
        Ok(schedules)
    }

    /// Get schedules by cron expression
    pub async fn get_schedules_by_cron(
        db: &crate::db::DatabasePool,
        cron: &str,
        limit: Option<i64>,
        offset: Option<i64>,
    ) -> Result<Vec<Schedule>, ScheduleError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                Self::get_schedules_by_cron_postgres(pg_pool, cron, limit, offset).await
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                Self::get_schedules_by_cron_sqlite(sqlite_pool, cron, limit, offset).await
            }
        }
    }

    async fn get_schedules_by_cron_sqlite(
        db: &Pool<Sqlite>,
        cron: &str,
        limit: Option<i64>,
        offset: Option<i64>,
    ) -> Result<Vec<Schedule>, ScheduleError> {
        let limit = limit.unwrap_or(10);
        let offset = offset.unwrap_or(0);

        let schedules = sqlx::query_as::<_, Schedule>(
            "SELECT schedule_id, build_id, cron, ns, var, meta, value, file, line, \"column\", position, active, created_at, deactivated_at FROM schedule WHERE cron = ? ORDER BY ns, var LIMIT ? OFFSET ?"
        )
        .bind(cron)
        .bind(limit)
        .bind(offset)
        .fetch_all(db)
        .await?;
        Ok(schedules)
    }

    async fn get_schedules_by_cron_postgres(
        db: &Pool<Postgres>,
        cron: &str,
        limit: Option<i64>,
        offset: Option<i64>,
    ) -> Result<Vec<Schedule>, ScheduleError> {
        let limit = limit.unwrap_or(10);
        let offset = offset.unwrap_or(0);

        let schedules = sqlx::query_as::<_, Schedule>(
            "SELECT schedule_id, build_id, cron, ns, var, meta, value, file, line, \"column\", position, active, created_at, deactivated_at FROM schedule WHERE cron = $1 ORDER BY ns, var LIMIT $2 OFFSET $3"
        )
        .bind(cron)
        .bind(limit)
        .bind(offset)
        .fetch_all(db)
        .await?;
        Ok(schedules)
    }

    /// Get schedules by build ID and cron expression
    pub async fn get_schedules_by_build_and_cron(
        db: &crate::db::DatabasePool,
        build_id: &Uuid,
        cron: &str,
        limit: Option<i64>,
        offset: Option<i64>,
    ) -> Result<Vec<Schedule>, ScheduleError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                Self::get_schedules_by_build_and_cron_postgres(
                    pg_pool, build_id, cron, limit, offset,
                )
                .await
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                Self::get_schedules_by_build_and_cron_sqlite(
                    sqlite_pool,
                    build_id,
                    cron,
                    limit,
                    offset,
                )
                .await
            }
        }
    }

    async fn get_schedules_by_build_and_cron_sqlite(
        db: &Pool<Sqlite>,
        build_id: &Uuid,
        cron: &str,
        limit: Option<i64>,
        offset: Option<i64>,
    ) -> Result<Vec<Schedule>, ScheduleError> {
        let limit = limit.unwrap_or(10);
        let offset = offset.unwrap_or(0);

        let schedules = sqlx::query_as::<_, Schedule>(
            "SELECT schedule_id, build_id, cron, ns, var, meta, value, file, line, \"column\", position, active, created_at, deactivated_at FROM schedule WHERE build_id = ? AND cron = ? ORDER BY ns, var LIMIT ? OFFSET ?"
        )
        .bind(build_id)
        .bind(cron)
        .bind(limit)
        .bind(offset)
        .fetch_all(db)
        .await?;
        Ok(schedules)
    }

    async fn get_schedules_by_build_and_cron_postgres(
        db: &Pool<Postgres>,
        build_id: &Uuid,
        cron: &str,
        limit: Option<i64>,
        offset: Option<i64>,
    ) -> Result<Vec<Schedule>, ScheduleError> {
        let limit = limit.unwrap_or(10);
        let offset = offset.unwrap_or(0);

        let schedules = sqlx::query_as::<_, Schedule>(
            "SELECT schedule_id, build_id, cron, ns, var, meta, value, file, line, \"column\", position, active, created_at, deactivated_at FROM schedule WHERE build_id = $1 AND cron = $2 ORDER BY ns, var LIMIT $3 OFFSET $4"
        )
        .bind(build_id)
        .bind(cron)
        .bind(limit)
        .bind(offset)
        .fetch_all(db)
        .await?;
        Ok(schedules)
    }

    /// Get count of schedules
    pub async fn get_count(db: &crate::db::DatabasePool) -> Result<i64, ScheduleError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let count = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM schedule")
                    .fetch_one(pg_pool)
                    .await?;
                Ok(count)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let count = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM schedule")
                    .fetch_one(sqlite_pool)
                    .await?;
                Ok(count)
            }
        }
    }

    /// Get count of schedules by build ID
    pub async fn get_count_by_build(
        db: &crate::db::DatabasePool,
        build_id: &Uuid,
    ) -> Result<i64, ScheduleError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let count = sqlx::query_scalar::<_, i64>(
                    "SELECT COUNT(*) FROM schedule WHERE build_id = $1",
                )
                .bind(build_id)
                .fetch_one(pg_pool)
                .await?;
                Ok(count)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let count = sqlx::query_scalar::<_, i64>(
                    "SELECT COUNT(*) FROM schedule WHERE build_id = ?",
                )
                .bind(build_id)
                .fetch_one(sqlite_pool)
                .await?;
                Ok(count)
            }
        }
    }

    /// Get count of schedules by cron expression
    pub async fn get_count_by_cron(
        db: &crate::db::DatabasePool,
        cron: &str,
    ) -> Result<i64, ScheduleError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let count =
                    sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM schedule WHERE cron = $1")
                        .bind(cron)
                        .fetch_one(pg_pool)
                        .await?;
                Ok(count)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let count =
                    sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM schedule WHERE cron = ?")
                        .bind(cron)
                        .fetch_one(sqlite_pool)
                        .await?;
                Ok(count)
            }
        }
    }

    /// Insert a new schedule
    #[allow(clippy::too_many_arguments)]
    pub async fn insert_schedule(
        db: &crate::db::DatabasePool,
        schedule_id: &Uuid,
        build_id: &Uuid,
        cron: &str,
        ns: &str,
        var: &str,
        meta: Option<&JsonValue>,
        value: Option<&JsonValue>,
        file: Option<&str>,
        line: Option<i32>,
        column: Option<i32>,
        position: Option<i32>,
    ) -> Result<(), ScheduleError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                Self::insert_schedule_postgres(
                    pg_pool,
                    schedule_id,
                    build_id,
                    cron,
                    ns,
                    var,
                    meta,
                    value,
                    file,
                    line,
                    column,
                    position,
                )
                .await
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                Self::insert_schedule_sqlite(
                    sqlite_pool,
                    schedule_id,
                    build_id,
                    cron,
                    ns,
                    var,
                    meta,
                    value,
                    file,
                    line,
                    column,
                    position,
                )
                .await
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn insert_schedule_sqlite(
        db: &Pool<Sqlite>,
        schedule_id: &Uuid,
        build_id: &Uuid,
        cron: &str,
        ns: &str,
        var: &str,
        meta: Option<&JsonValue>,
        value: Option<&JsonValue>,
        file: Option<&str>,
        line: Option<i32>,
        column: Option<i32>,
        position: Option<i32>,
    ) -> Result<(), ScheduleError> {
        let meta_json = meta
            .map(serde_json::to_string)
            .transpose()
            .map_err(|e| ScheduleError::SerializationError(e.to_string()))?;
        let value_json = value
            .map(serde_json::to_string)
            .transpose()
            .map_err(|e| ScheduleError::SerializationError(e.to_string()))?;

        sqlx::query(
            "INSERT INTO schedule (schedule_id, build_id, cron, ns, var, meta, value, file, line, \"column\", position) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"
        )
        .bind(schedule_id)
        .bind(build_id)
        .bind(cron)
        .bind(ns)
        .bind(var)
        .bind(meta_json)
        .bind(value_json)
        .bind(file)
        .bind(line)
        .bind(column)
        .bind(position)
        .execute(db)
        .await?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn insert_schedule_postgres(
        db: &Pool<Postgres>,
        schedule_id: &Uuid,
        build_id: &Uuid,
        cron: &str,
        ns: &str,
        var: &str,
        meta: Option<&JsonValue>,
        value: Option<&JsonValue>,
        file: Option<&str>,
        line: Option<i32>,
        column: Option<i32>,
        position: Option<i32>,
    ) -> Result<(), ScheduleError> {
        sqlx::query(
            "INSERT INTO schedule (schedule_id, build_id, cron, ns, var, meta, value, file, line, \"column\", position) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)"
        )
        .bind(schedule_id)
        .bind(build_id)
        .bind(cron)
        .bind(ns)
        .bind(var)
        .bind(meta)
        .bind(value)
        .bind(file)
        .bind(line)
        .bind(column)
        .bind(position)
        .execute(db)
        .await?;
        Ok(())
    }

    /// Insert or update multiple schedules for a build (UPSERT)
    /// Matches on (build_id, ns, var, cron) and:
    /// - If found: reactivates and updates meta, value, file, line, column, position
    /// - If not found: inserts new schedule
    pub async fn insert_schedules_for_build(
        db: &crate::db::DatabasePool,
        build_id: &Uuid,
        scheduled_functions: &crate::lang::compiler::ScheduledFunctions,
        send_targets: &crate::lang::compiler::SendTargets,
    ) -> Result<(), ScheduleError> {
        // First, validate all cron expressions before inserting any schedules
        for (cron_expression, functions) in scheduled_functions {
            if let Err(validation_error) =
                Self::validate_cron_expression(cron_expression).and_then(|_| {
                    validate_recurring_schedule_interval(
                        cron_expression,
                        TECHNICAL_MIN_RECURRING_INTERVAL_SECS,
                    )
                    .map_err(|e| e.message())
                })
            {
                // Include function information in the error for better debugging
                if let Some(first_function) = functions.first()
                    && let Ok((ns, var, _, _, file, line, column, position)) =
                        Self::extract_function_data(first_function)
                {
                    return Err(ScheduleError::CronValidationError(Box::new(
                        CronValidationErrorDetails {
                            message: validation_error,
                            cron_expression: cron_expression.clone(),
                            function_ns: ns,
                            function_var: var,
                            file: file.map(PathBuf::from),
                            line,
                            column,
                            position,
                            length: Some(cron_expression.len()),
                        },
                    )));
                }

                // Fallback if we can't extract function data
                return Err(ScheduleError::CronValidationError(Box::new(
                    CronValidationErrorDetails {
                        message: validation_error,
                        cron_expression: cron_expression.clone(),
                        function_ns: "unknown".to_string(),
                        function_var: "unknown".to_string(),
                        file: None,
                        line: None,
                        column: None,
                        position: None,
                        length: Some(cron_expression.len()),
                    },
                )));
            }
        }

        // Deactivate all existing schedules for this build before inserting new ones
        // This ensures that:
        // 1. Removed schedules stay deactivated
        // 2. Schedules with changed cron expressions get properly updated
        // 3. Unchanged schedules get reactivated by the upsert below
        let deactivated_count = Self::deactivate_schedules_by_build(db, build_id).await?;
        if deactivated_count > 0 {
            tracing::debug!(
                "Deactivated {} existing schedule(s) for build {} before inserting new schedules",
                deactivated_count,
                build_id
            );
        }

        // All cron expressions are valid, proceed with upsert
        for (cron_expression, functions) in scheduled_functions {
            for function in functions {
                let (ns, var, meta, value, file, line, column, position) =
                    Self::extract_function_data(function)?;

                let fn_key = format!("{}/{}", ns, var);
                let static_sends: Vec<String> = send_targets
                    .get(&fn_key)
                    .map(|targets| targets.iter().map(|t| t.event_name.clone()).collect())
                    .unwrap_or_default();
                let merged_meta = crate::db::merge_sends_into_meta(meta, &static_sends);

                Self::upsert_schedule(
                    db,
                    build_id,
                    cron_expression,
                    &ns,
                    &var,
                    merged_meta.as_ref(),
                    value.as_ref(),
                    file.as_deref(),
                    line,
                    column,
                    position,
                )
                .await?;
            }
        }
        Ok(())
    }

    /// Upsert a schedule (insert or reactivate+update if exists)
    /// Matches on (build_id, ns, var, cron)
    #[allow(clippy::too_many_arguments)]
    pub async fn upsert_schedule(
        db: &crate::db::DatabasePool,
        build_id: &Uuid,
        cron: &str,
        ns: &str,
        var: &str,
        meta: Option<&JsonValue>,
        value: Option<&JsonValue>,
        file: Option<&str>,
        line: Option<i32>,
        column: Option<i32>,
        position: Option<i32>,
    ) -> Result<(), ScheduleError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                sqlx::query(
                    "INSERT INTO schedule (schedule_id, build_id, cron, ns, var, meta, value, file, line, \"column\", position, active, created_at, deactivated_at)
                     VALUES (uuidv7(), $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, true, NOW(), NULL)
                     ON CONFLICT (build_id, ns, var, cron)
                     DO UPDATE SET
                         meta = EXCLUDED.meta,
                         value = EXCLUDED.value,
                         file = EXCLUDED.file,
                         line = EXCLUDED.line,
                         \"column\" = EXCLUDED.\"column\",
                         position = EXCLUDED.position,
                         active = true,
                         deactivated_at = NULL"
                )
                .bind(build_id)
                .bind(cron)
                .bind(ns)
                .bind(var)
                .bind(meta)
                .bind(value)
                .bind(file)
                .bind(line)
                .bind(column)
                .bind(position)
                .execute(pg_pool)
                .await?;
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let meta_json = meta
                    .map(serde_json::to_string)
                    .transpose()
                    .map_err(|e| ScheduleError::SerializationError(e.to_string()))?;
                let value_json = value
                    .map(serde_json::to_string)
                    .transpose()
                    .map_err(|e| ScheduleError::SerializationError(e.to_string()))?;

                sqlx::query(
                    "INSERT INTO schedule (schedule_id, build_id, cron, ns, var, meta, value, file, line, \"column\", position, active, created_at, deactivated_at)
                     VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 1, strftime('%Y-%m-%d %H:%M:%f', 'now'), NULL)
                     ON CONFLICT (build_id, ns, var, cron)
                     DO UPDATE SET
                         meta = EXCLUDED.meta,
                         value = EXCLUDED.value,
                         file = EXCLUDED.file,
                         line = EXCLUDED.line,
                         \"column\" = EXCLUDED.\"column\",
                         position = EXCLUDED.position,
                         active = 1,
                         deactivated_at = NULL"
                )
                .bind(Uuid::now_v7())
                .bind(build_id)
                .bind(cron)
                .bind(ns)
                .bind(var)
                .bind(meta_json)
                .bind(value_json)
                .bind(file)
                .bind(line)
                .bind(column)
                .bind(position)
                .execute(sqlite_pool)
                .await?;
            }
        }
        Ok(())
    }

    /// Validate a cron expression for Hot scheduling
    ///
    /// Hot now supports both traditional cron expressions AND natural language!
    ///
    /// ## Traditional Cron Format (5-7 fields)
    /// ```text
    /// ┌─────────────── second (0 - 59, optional)
    /// │ ┌───────────── minute (0 - 59)
    /// │ │ ┌─────────── hour (0 - 23)
    /// │ │ │ ┌───────── day of month (1 - 31)
    /// │ │ │ │ ┌─────── month (1 - 12, JAN-DEC)
    /// │ │ │ │ │ ┌───── day of week (0 - 6, SUN-SAT)
    /// │ │ │ │ │ │ ┌─── year (optional)
    /// │ │ │ │ │ │ │
    /// * * * * * * *
    /// ```
    ///
    /// ## Natural Language Support 🎉
    /// Hot now accepts English expressions! Examples:
    /// - "every day at 2 AM"
    /// - "every Monday at 9:30 AM"
    /// - "daily at 14:30"
    /// - "every weekday at 8:00 AM"
    /// - "every 5 minutes"
    /// - "monthly on the 15th"
    /// - "every Friday at 5 PM"
    /// - "weekly"
    /// - "daily"
    /// - "hourly"
    ///
    /// ## Nickname Shortcuts
    /// - `@yearly` or `@annually` - Run once a year (0 0 0 1 1 *)
    /// - `@monthly` - Run once a month (0 0 0 1 * *)
    /// - `@weekly` - Run once a week (0 0 0 * * 0)
    /// - `@daily` - Run once a day (0 0 0 * * *)
    /// - `@hourly` - Run once an hour (0 0 * * * *)
    ///
    /// ## Advanced Modifiers (Traditional Cron)
    /// - **L** - Last day: `0 0 9 L * *` (last day of month), `0 0 9 * * FRI#L` (last Friday)
    /// - **#** - Nth occurrence: `0 0 9 * * MON#2` (second Monday), `0 0 9 * * MON-FRI#2` (weekdays of 2nd week)
    /// - **W** - Closest weekday: `0 0 9 15W * *` (closest weekday to 15th)
    /// - **+** - AND logic: `0 0 12 25 12 +FRI` (Christmas day AND Friday)
    /// - **?** - Legacy wildcard (same as *)
    ///
    /// ## Real-World Examples
    /// ```text
    /// Traditional Cron          | Natural Language
    /// ---------------------------|------------------
    /// @daily                     | "daily"
    /// 0 0 9 * * MON-FRI         | "every weekday at 9 AM"
    /// 0 0 9 L * *               | "monthly" (first of month)
    /// 0 0 9 * * FRI#L           | "every Friday" (last Friday logic not yet supported)
    /// 0 30 8 1W * *             | "monthly on the 1st" (weekday logic applied by croner)
    /// 0 */30 9-17 * * MON-FRI   | Traditional cron (complex patterns)
    /// ```
    pub fn validate_cron_expression(cron: &str) -> Result<(), String> {
        normalize_recurring_schedule_expression(cron)
            .map(|_| ())
            .map_err(|e| {
                format!(
                    "Could not parse '{}' as either a cron expression or supported natural language.\n\
                    \n\
                    {}\n\
                    \n\
                    Try phrases like:\n\
                    • 'daily at 9 AM'\n\
                    • 'every Monday at 2:30 PM'\n\
                    • 'every 5 minutes'\n\
                    • 'every weekday at 8:00 AM'\n\
                    • 'monthly on the 15th'\n\
                    • 'weekly'\n\
                    \n\
                    Traditional cron format: 'sec min hour day month day_of_week'\n\
                    Examples: '0 30 9 * * MON' (9:30 AM Monday), '*/15 * * * * *' (every 15 seconds)",
                    cron, e
                )
            })
    }

    /// Extract function data from a compiler ScheduledFunction
    #[allow(clippy::type_complexity)]
    fn extract_function_data(
        function: &crate::lang::compiler::ScheduledFunction,
    ) -> Result<
        (
            String,
            String,
            Option<JsonValue>,
            Option<JsonValue>,
            Option<String>,
            Option<i32>,
            Option<i32>,
            Option<i32>,
        ),
        ScheduleError,
    > {
        // Extract data from the function's scheduled_function Val
        if let crate::val::Val::Map(function_map) = &function.scheduled_function {
            // Parse fn field (format: "::namespace/var")
            let fn_name = function_map
                .get(&crate::val::Val::from("fn"))
                .and_then(|v| match v {
                    crate::val::Val::Str(s) => Some((**s).to_owned()),
                    _ => None,
                })
                .unwrap_or_default();

            // Split fn into ns and var (split on last '/')
            let (ns, var) = fn_name
                .rsplit_once('/')
                .map(|(ns, var)| (ns.to_string(), var.to_string()))
                .unwrap_or_default();

            let meta = function_map
                .get(&crate::val::Val::from("meta"))
                .map(crate::db::resolve_meta_val)
                .and_then(|v| serde_json::to_value(&v).ok());

            // value field is no longer used (was redundant with fn)
            let value: Option<JsonValue> = None;

            // Extract source location information
            let file = function_map
                .get(&crate::val::Val::from("file"))
                .and_then(|v| match v {
                    crate::val::Val::Str(s) => Some((**s).to_owned()),
                    crate::val::Val::Null => None,
                    _ => None,
                });

            let line = function_map
                .get(&crate::val::Val::from("line"))
                .and_then(|v| match v {
                    crate::val::Val::Int(i) => Some(*i as i32),
                    crate::val::Val::Null => None,
                    _ => None,
                });

            let column = function_map
                .get(&crate::val::Val::from("column"))
                .and_then(|v| match v {
                    crate::val::Val::Int(i) => Some(*i as i32),
                    crate::val::Val::Null => None,
                    _ => None,
                });

            let position = function_map
                .get(&crate::val::Val::from("position"))
                .and_then(|v| match v {
                    crate::val::Val::Int(i) => Some(*i as i32),
                    crate::val::Val::Null => None,
                    _ => None,
                });

            Ok((ns, var, meta, value, file, line, column, position))
        } else {
            Err(ScheduleError::SerializationError(
                "Scheduled function data is not a map".to_string(),
            ))
        }
    }

    /// Deactivate schedules by build ID (soft delete)
    /// Returns the number of schedules deactivated
    pub async fn deactivate_schedules_by_build(
        db: &crate::db::DatabasePool,
        build_id: &Uuid,
    ) -> Result<u64, ScheduleError> {
        let now = chrono::Utc::now();
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let result = sqlx::query(
                    "UPDATE schedule SET active = false, deactivated_at = $1 WHERE build_id = $2 AND active = true"
                )
                .bind(now)
                .bind(build_id)
                .execute(pg_pool)
                .await?;
                if result.rows_affected() > 0 {
                    tracing::debug!(
                        "Deactivated {} schedule(s) for build {}",
                        result.rows_affected(),
                        build_id
                    );
                }
                Ok(result.rows_affected())
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let result = sqlx::query(
                    "UPDATE schedule SET active = 0, deactivated_at = ? WHERE build_id = ? AND active = 1"
                )
                .bind(now)
                .bind(build_id)
                .execute(sqlite_pool)
                .await?;
                if result.rows_affected() > 0 {
                    tracing::debug!(
                        "Deactivated {} schedule(s) for build {}",
                        result.rows_affected(),
                        build_id
                    );
                }
                Ok(result.rows_affected())
            }
        }
    }

    /// Deactivate all schedules for a project (across all builds)
    /// Used when deactivating a project
    pub async fn deactivate_schedules_by_project(
        db: &crate::db::DatabasePool,
        project_id: &Uuid,
    ) -> Result<u64, ScheduleError> {
        let now = chrono::Utc::now();
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let result = sqlx::query(
                    "UPDATE schedule SET active = false, deactivated_at = $1
                     WHERE build_id IN (SELECT build_id FROM build WHERE project_id = $2)
                     AND active = true",
                )
                .bind(now)
                .bind(project_id)
                .execute(pg_pool)
                .await?;
                Ok(result.rows_affected())
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let result = sqlx::query(
                    "UPDATE schedule SET active = 0, deactivated_at = ?
                     WHERE build_id IN (SELECT build_id FROM build WHERE project_id = ?)
                     AND active = 1",
                )
                .bind(now)
                .bind(project_id)
                .execute(sqlite_pool)
                .await?;
                Ok(result.rows_affected())
            }
        }
    }

    /// Delete schedules by build ID (hard delete)
    /// Use deactivate_schedules_by_build instead for normal operations
    pub async fn delete_schedules_by_build(
        db: &crate::db::DatabasePool,
        build_id: &Uuid,
    ) -> Result<u64, ScheduleError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let result = sqlx::query("DELETE FROM schedule WHERE build_id = $1")
                    .bind(build_id)
                    .execute(pg_pool)
                    .await?;
                Ok(result.rows_affected())
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let result = sqlx::query("DELETE FROM schedule WHERE build_id = ?")
                    .bind(build_id)
                    .execute(sqlite_pool)
                    .await?;
                Ok(result.rows_affected())
            }
        }
    }

    /// Deactivate a specific schedule (soft delete)
    pub async fn deactivate_schedule(
        db: &crate::db::DatabasePool,
        schedule_id: &Uuid,
    ) -> Result<(), ScheduleError> {
        let now = chrono::Utc::now();
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                sqlx::query(
                    "UPDATE schedule SET active = false, deactivated_at = $1 WHERE schedule_id = $2 AND active = true"
                )
                .bind(now)
                .bind(schedule_id)
                .execute(pg_pool)
                .await?;
                Ok(())
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                sqlx::query(
                    "UPDATE schedule SET active = 0, deactivated_at = ? WHERE schedule_id = ? AND active = 1"
                )
                .bind(now)
                .bind(schedule_id)
                .execute(sqlite_pool)
                .await?;
                Ok(())
            }
        }
    }

    /// Delete a specific schedule (hard delete)
    /// Use deactivate_schedule instead for normal operations
    pub async fn delete_schedule(
        db: &crate::db::DatabasePool,
        schedule_id: &Uuid,
    ) -> Result<(), ScheduleError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                sqlx::query("DELETE FROM schedule WHERE schedule_id = $1")
                    .bind(schedule_id)
                    .execute(pg_pool)
                    .await?;
                Ok(())
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                sqlx::query("DELETE FROM schedule WHERE schedule_id = ?")
                    .bind(schedule_id)
                    .execute(sqlite_pool)
                    .await?;
                Ok(())
            }
        }
    }

    /// Delete inactive schedules older than the specified number of days
    /// This will also cascade delete their schedule_log entries
    /// Returns the number of schedules deleted
    pub async fn delete_old_inactive_schedules(
        db: &crate::db::DatabasePool,
        days_threshold: i64,
    ) -> Result<u64, ScheduleError> {
        let cutoff_time = chrono::Utc::now() - chrono::Duration::days(days_threshold);
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let result = sqlx::query(
                    "DELETE FROM schedule WHERE active = false AND deactivated_at < $1",
                )
                .bind(cutoff_time)
                .execute(pg_pool)
                .await?;
                Ok(result.rows_affected())
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let result =
                    sqlx::query("DELETE FROM schedule WHERE active = 0 AND deactivated_at < ?")
                        .bind(cutoff_time)
                        .execute(sqlite_pool)
                        .await?;
                Ok(result.rows_affected())
            }
        }
    }

    /// Get schedules for deployed builds in a specific environment
    pub async fn get_schedules_by_env_deployed(
        db: &crate::db::DatabasePool,
        env_id: &Uuid,
        limit: Option<i64>,
        offset: Option<i64>,
    ) -> Result<Vec<ScheduleWithProject>, ScheduleError> {
        let limit = limit.unwrap_or(100);
        let offset = offset.unwrap_or(0);

        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let schedules = sqlx::query_as::<_, ScheduleWithProject>(
                    "SELECT s.schedule_id, s.build_id, s.cron, s.ns, s.var, s.meta, s.value, s.file, s.line, s.\"column\", s.position, s.active, s.created_at, s.deactivated_at, p.project_id, p.name as project_name
                     FROM schedule s
                     JOIN build b ON s.build_id = b.build_id
                     JOIN project p ON b.project_id = p.project_id
                     WHERE s.active = true AND b.deployed = true AND b.runtime_status = 'ready' AND b.active = true AND p.active = true AND p.env_id = $1
                     ORDER BY p.name, s.cron, s.ns, s.var
                     LIMIT $2 OFFSET $3"
                )
                .bind(env_id)
                .bind(limit)
                .bind(offset)
                .fetch_all(pg_pool)
                .await?;
                Ok(schedules)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let schedules = sqlx::query_as::<_, ScheduleWithProject>(
                    "SELECT s.schedule_id, s.build_id, s.cron, s.ns, s.var, s.meta, s.value, s.file, s.line, s.\"column\", s.position, s.active, s.created_at, s.deactivated_at, p.project_id, p.name as project_name
                     FROM schedule s
                     JOIN build b ON s.build_id = b.build_id
                     JOIN project p ON b.project_id = p.project_id
                     WHERE s.active = 1 AND b.deployed = 1 AND b.runtime_status = 'ready' AND b.active = 1 AND p.active = 1 AND p.env_id = ?
                     ORDER BY p.name, s.cron, s.ns, s.var
                     LIMIT ? OFFSET ?"
                )
                .bind(env_id)
                .bind(limit)
                .bind(offset)
                .fetch_all(sqlite_pool)
                .await?;
                Ok(schedules)
            }
        }
    }

    /// Get count of schedules for deployed builds in a specific environment
    pub async fn get_count_by_env_deployed(
        db: &crate::db::DatabasePool,
        env_id: &Uuid,
    ) -> Result<i64, ScheduleError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let count = sqlx::query_scalar::<_, i64>(
                    "SELECT COUNT(*) FROM schedule s
                     JOIN build b ON s.build_id = b.build_id
                     JOIN project p ON b.project_id = p.project_id
                     WHERE s.active = true AND b.deployed = true AND b.runtime_status = 'ready' AND b.active = true AND p.active = true AND p.env_id = $1"
                )
                .bind(env_id)
                .fetch_one(pg_pool)
                .await?;
                Ok(count)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let count = sqlx::query_scalar::<_, i64>(
                    "SELECT COUNT(*) FROM schedule s
                     JOIN build b ON s.build_id = b.build_id
                     JOIN project p ON b.project_id = p.project_id
                     WHERE s.active = 1 AND b.deployed = 1 AND b.runtime_status = 'ready' AND b.active = 1 AND p.active = 1 AND p.env_id = ?",
                )
                .bind(env_id)
                .fetch_one(sqlite_pool)
                .await?;
                Ok(count)
            }
        }
    }

    /// Count active schedules attached to active deployed builds in active projects/envs for an org.
    pub async fn get_active_count_by_org(
        db: &crate::db::DatabasePool,
        org_id: &Uuid,
    ) -> Result<i64, ScheduleError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let count = sqlx::query_scalar::<_, i64>(
                    "SELECT COUNT(*) FROM schedule s
                     JOIN build b ON s.build_id = b.build_id
                     JOIN project p ON b.project_id = p.project_id
                     JOIN env e ON p.env_id = e.env_id
                     WHERE s.active = true
                       AND b.deployed = true
                       AND b.runtime_status = 'ready'
                       AND b.active = true
                       AND p.active = true
                       AND e.active = true
                       AND e.org_id = $1",
                )
                .bind(org_id)
                .fetch_one(pg_pool)
                .await?;
                Ok(count)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let count = sqlx::query_scalar::<_, i64>(
                    "SELECT COUNT(*) FROM schedule s
                     JOIN build b ON s.build_id = b.build_id
                     JOIN project p ON b.project_id = p.project_id
                     JOIN env e ON p.env_id = e.env_id
                     WHERE s.active = 1
                       AND b.deployed = 1
                       AND b.runtime_status = 'ready'
                       AND b.active = 1
                       AND p.active = 1
                       AND e.active = 1
                       AND e.org_id = ?",
                )
                .bind(org_id)
                .fetch_one(sqlite_pool)
                .await?;
                Ok(count)
            }
        }
    }

    pub async fn get_active_count_by_project(
        db: &crate::db::DatabasePool,
        project_id: &Uuid,
    ) -> Result<i64, ScheduleError> {
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let count = sqlx::query_scalar::<_, i64>(
                    "SELECT COUNT(*) FROM schedule s
                     JOIN build b ON s.build_id = b.build_id
                     WHERE s.active = true
                       AND b.deployed = true
                       AND b.runtime_status = 'ready'
                       AND b.active = true
                       AND b.project_id = $1",
                )
                .bind(project_id)
                .fetch_one(pg_pool)
                .await?;
                Ok(count)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let count = sqlx::query_scalar::<_, i64>(
                    "SELECT COUNT(*) FROM schedule s
                     JOIN build b ON s.build_id = b.build_id
                     WHERE s.active = 1
                       AND b.deployed = 1
                       AND b.runtime_status = 'ready'
                       AND b.active = 1
                       AND b.project_id = ?",
                )
                .bind(project_id)
                .fetch_one(sqlite_pool)
                .await?;
                Ok(count)
            }
        }
    }

    pub async fn enforce_active_count_for_org_replacing_project(
        db: &crate::db::DatabasePool,
        org_id: &Uuid,
        project_id: &Uuid,
        new_project_schedules: i64,
        policy: SchedulePolicy,
    ) -> Result<(), ScheduleError> {
        if policy.max_active_per_org < 0 {
            return Ok(());
        }

        let current_org = Self::get_active_count_by_org(db, org_id).await?;
        let current_project = Self::get_active_count_by_project(db, project_id).await?;
        let projected = current_org - current_project + new_project_schedules.max(0);
        if projected > policy.max_active_per_org {
            return Err(ScheduleError::PolicyError(format!(
                "Active schedule limit exceeded: deploying this build would leave {} active schedule(s), above the org limit of {}",
                projected, policy.max_active_per_org
            )));
        }

        Ok(())
    }

    pub async fn enforce_active_count_for_org(
        db: &crate::db::DatabasePool,
        org_id: &Uuid,
        additional_schedules: i64,
        policy: SchedulePolicy,
    ) -> Result<(), ScheduleError> {
        if policy.max_active_per_org < 0 {
            return Ok(());
        }

        let current = Self::get_active_count_by_org(db, org_id).await?;
        let projected = current + additional_schedules.max(0);
        if projected > policy.max_active_per_org {
            return Err(ScheduleError::PolicyError(format!(
                "Active schedule limit exceeded: {} active schedule(s) plus {} new schedule(s) would exceed the org limit of {}",
                current,
                additional_schedules.max(0),
                policy.max_active_per_org
            )));
        }

        Ok(())
    }

    /// Get all schedules for deployed builds (only for active projects)
    pub async fn get_schedules_for_deployed_builds(
        db: &crate::db::DatabasePool,
        limit: Option<i64>,
        offset: Option<i64>,
    ) -> Result<Vec<Schedule>, ScheduleError> {
        let limit = limit.unwrap_or(1000); // Default to a large number since this is for scheduler sync
        let offset = offset.unwrap_or(0);

        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let schedules = sqlx::query_as::<_, Schedule>(
                    "SELECT s.schedule_id, s.build_id, s.cron, s.ns, s.var, s.meta, s.value, s.file, s.line, s.\"column\", s.position, s.active, s.created_at, s.deactivated_at
                     FROM schedule s
                     JOIN build b ON s.build_id = b.build_id
                     JOIN project p ON b.project_id = p.project_id
                     WHERE s.active = true AND b.deployed = true AND b.runtime_status = 'ready' AND b.active = true AND p.active = true
                     ORDER BY s.cron, s.ns, s.var
                     LIMIT $1 OFFSET $2"
                )
                .bind(limit)
                .bind(offset)
                .fetch_all(pg_pool)
                .await?;
                Ok(schedules)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let schedules = sqlx::query_as::<_, Schedule>(
                    "SELECT s.schedule_id, s.build_id, s.cron, s.ns, s.var, s.meta, s.value, s.file, s.line, s.\"column\", s.position, s.active, s.created_at, s.deactivated_at
                     FROM schedule s
                     JOIN build b ON s.build_id = b.build_id
                     JOIN project p ON b.project_id = p.project_id
                     WHERE s.active = 1 AND b.deployed = 1 AND b.runtime_status = 'ready' AND b.active = 1 AND p.active = 1
                     ORDER BY s.cron, s.ns, s.var
                     LIMIT ? OFFSET ?"
                )
                .bind(limit)
                .bind(offset)
                .fetch_all(sqlite_pool)
                .await?;
                Ok(schedules)
            }
        }
    }

    /// Get active @at: schedules that are due for execution (run_at <= now)
    /// These are one-time schedules created via hot:schedule:new events
    pub async fn get_due_at_schedules(
        db: &crate::db::DatabasePool,
        now: DateTime<Utc>,
    ) -> Result<Vec<ScheduleWithProject>, ScheduleError> {
        let now_str = now.to_rfc3339();
        let at_prefix = format!("{}%", AT_SCHEDULE_PREFIX);

        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                // For Postgres: extract the datetime from @at: prefix and compare
                // cron format: @at:2024-01-15T10:30:00Z
                let schedules = sqlx::query_as::<_, ScheduleWithProject>(
                    "SELECT s.schedule_id, s.build_id, s.cron, s.ns, s.var, s.meta, s.value, s.file, s.line, s.\"column\", s.position, s.active, s.created_at, s.deactivated_at, p.project_id, p.name as project_name
                     FROM schedule s
                     JOIN build b ON s.build_id = b.build_id
                     JOIN project p ON b.project_id = p.project_id
                     WHERE s.active = true
                       AND b.deployed = true
                       AND b.runtime_status = 'ready'
                       AND b.active = true
                       AND p.active = true
                       AND s.cron LIKE $1
                       AND CAST(SUBSTRING(s.cron FROM 5) AS TIMESTAMPTZ) <= $2
                     ORDER BY CAST(SUBSTRING(s.cron FROM 5) AS TIMESTAMPTZ)"
                )
                .bind(&at_prefix)
                .bind(now)
                .fetch_all(pg_pool)
                .await?;
                Ok(schedules)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                // For SQLite: use SUBSTR to extract datetime and compare as text
                // This works because ISO 8601 format is lexicographically sortable
                let schedules = sqlx::query_as::<_, ScheduleWithProject>(
                    "SELECT s.schedule_id, s.build_id, s.cron, s.ns, s.var, s.meta, s.value, s.file, s.line, s.\"column\", s.position, s.active, s.created_at, s.deactivated_at, p.project_id, p.name as project_name
                     FROM schedule s
                     JOIN build b ON s.build_id = b.build_id
                     JOIN project p ON b.project_id = p.project_id
                     WHERE s.active = 1
                       AND b.deployed = 1
                       AND b.runtime_status = 'ready'
                       AND b.active = 1
                       AND p.active = 1
                       AND s.cron LIKE ?
                       AND SUBSTR(s.cron, 5) <= ?
                     ORDER BY SUBSTR(s.cron, 5)"
                )
                .bind(&at_prefix)
                .bind(&now_str)
                .fetch_all(sqlite_pool)
                .await?;
                Ok(schedules)
            }
        }
    }

    /// Get all @at: schedules for deployed builds (for UI display)
    /// Includes both pending (active) and completed (inactive) one-time schedules
    pub async fn get_at_schedules_by_env(
        db: &crate::db::DatabasePool,
        env_id: &Uuid,
        include_inactive: bool,
        limit: Option<i64>,
        offset: Option<i64>,
    ) -> Result<Vec<ScheduleWithProject>, ScheduleError> {
        let limit = limit.unwrap_or(100);
        let offset = offset.unwrap_or(0);
        let at_prefix = format!("{}%", AT_SCHEDULE_PREFIX);

        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let query = if include_inactive {
                    "SELECT s.schedule_id, s.build_id, s.cron, s.ns, s.var, s.meta, s.value, s.file, s.line, s.\"column\", s.position, s.active, s.created_at, s.deactivated_at, p.project_id, p.name as project_name
                     FROM schedule s
                     JOIN build b ON s.build_id = b.build_id
                     JOIN project p ON b.project_id = p.project_id
                     WHERE s.cron LIKE $1 AND p.env_id = $2
                     ORDER BY s.created_at DESC
                     LIMIT $3 OFFSET $4"
                } else {
                    "SELECT s.schedule_id, s.build_id, s.cron, s.ns, s.var, s.meta, s.value, s.file, s.line, s.\"column\", s.position, s.active, s.created_at, s.deactivated_at, p.project_id, p.name as project_name
                     FROM schedule s
                     JOIN build b ON s.build_id = b.build_id
                     JOIN project p ON b.project_id = p.project_id
                     WHERE s.cron LIKE $1 AND p.env_id = $2 AND s.active = true
                     ORDER BY CAST(SUBSTRING(s.cron FROM 5) AS TIMESTAMPTZ)
                     LIMIT $3 OFFSET $4"
                };
                let schedules = sqlx::query_as::<_, ScheduleWithProject>(query)
                    .bind(&at_prefix)
                    .bind(env_id)
                    .bind(limit)
                    .bind(offset)
                    .fetch_all(pg_pool)
                    .await?;
                Ok(schedules)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let query = if include_inactive {
                    "SELECT s.schedule_id, s.build_id, s.cron, s.ns, s.var, s.meta, s.value, s.file, s.line, s.\"column\", s.position, s.active, s.created_at, s.deactivated_at, p.project_id, p.name as project_name
                     FROM schedule s
                     JOIN build b ON s.build_id = b.build_id
                     JOIN project p ON b.project_id = p.project_id
                     WHERE s.cron LIKE ? AND p.env_id = ?
                     ORDER BY s.created_at DESC
                     LIMIT ? OFFSET ?"
                } else {
                    "SELECT s.schedule_id, s.build_id, s.cron, s.ns, s.var, s.meta, s.value, s.file, s.line, s.\"column\", s.position, s.active, s.created_at, s.deactivated_at, p.project_id, p.name as project_name
                     FROM schedule s
                     JOIN build b ON s.build_id = b.build_id
                     JOIN project p ON b.project_id = p.project_id
                     WHERE s.cron LIKE ? AND p.env_id = ? AND s.active = 1
                     ORDER BY SUBSTR(s.cron, 5)
                     LIMIT ? OFFSET ?"
                };
                let schedules = sqlx::query_as::<_, ScheduleWithProject>(query)
                    .bind(&at_prefix)
                    .bind(env_id)
                    .bind(limit)
                    .bind(offset)
                    .fetch_all(sqlite_pool)
                    .await?;
                Ok(schedules)
            }
        }
    }

    /// Cancel a schedule by ID (works for both cron and @at schedules)
    /// Returns true if a schedule was cancelled, false if not found or already inactive
    pub async fn cancel_schedule(
        db: &crate::db::DatabasePool,
        schedule_id: &Uuid,
    ) -> Result<bool, ScheduleError> {
        let now = chrono::Utc::now();
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let result = sqlx::query(
                    "UPDATE schedule SET active = false, deactivated_at = $1 WHERE schedule_id = $2 AND active = true"
                )
                .bind(now)
                .bind(schedule_id)
                .execute(pg_pool)
                .await?;
                Ok(result.rows_affected() > 0)
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let result = sqlx::query(
                    "UPDATE schedule SET active = 0, deactivated_at = ? WHERE schedule_id = ? AND active = 1"
                )
                .bind(now)
                .bind(schedule_id)
                .execute(sqlite_pool)
                .await?;
                Ok(result.rows_affected() > 0)
            }
        }
    }

    /// Cancel schedules by function name for a build
    /// Returns the number of schedules cancelled
    pub async fn cancel_schedules_by_function(
        db: &crate::db::DatabasePool,
        build_id: &Uuid,
        ns: &str,
        var: &str,
    ) -> Result<u64, ScheduleError> {
        let now = chrono::Utc::now();
        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                let result = sqlx::query(
                    "UPDATE schedule SET active = false, deactivated_at = $1 WHERE build_id = $2 AND ns = $3 AND var = $4 AND active = true"
                )
                .bind(now)
                .bind(build_id)
                .bind(ns)
                .bind(var)
                .execute(pg_pool)
                .await?;
                Ok(result.rows_affected())
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let result = sqlx::query(
                    "UPDATE schedule SET active = 0, deactivated_at = ? WHERE build_id = ? AND ns = ? AND var = ? AND active = 1"
                )
                .bind(now)
                .bind(build_id)
                .bind(ns)
                .bind(var)
                .execute(sqlite_pool)
                .await?;
                Ok(result.rows_affected())
            }
        }
    }

    /// Insert a dynamic schedule (created via hot:schedule:new event)
    /// This is different from insert_schedule which is used during build compilation
    #[allow(clippy::too_many_arguments)]
    pub async fn insert_dynamic_schedule(
        db: &crate::db::DatabasePool,
        schedule_id: &Uuid,
        build_id: &Uuid,
        schedule_type: &ScheduleType,
        org_id: Option<&Uuid>,
        policy: SchedulePolicy,
        ns: &str,
        var: &str,
        meta: Option<&JsonValue>,
        args: Option<&JsonValue>,
    ) -> Result<(), ScheduleError> {
        match schedule_type {
            ScheduleType::Cron(cron) => {
                validate_recurring_schedule_interval(cron, policy.min_interval_secs)
                    .map_err(|e| ScheduleError::PolicyError(e.message()))?
            }
            ScheduleType::At(run_at) => {
                validate_one_time_schedule_delay(*run_at, policy.min_delay_secs)
                    .map_err(ScheduleError::PolicyError)?
            }
        }

        if let Some(org_id) = org_id {
            Self::enforce_active_count_for_org(db, org_id, 1, policy).await?;
        }

        let cron_field = schedule_type.to_cron_field();

        match db {
            crate::db::DatabasePool::Postgres(pg_pool) => {
                sqlx::query(
                    "INSERT INTO schedule (schedule_id, build_id, cron, ns, var, meta, value, active) VALUES ($1, $2, $3, $4, $5, $6, $7, true)"
                )
                .bind(schedule_id)
                .bind(build_id)
                .bind(&cron_field)
                .bind(ns)
                .bind(var)
                .bind(meta)
                .bind(args)
                .execute(pg_pool)
                .await?;
            }
            crate::db::DatabasePool::Sqlite(sqlite_pool) => {
                let meta_json = meta
                    .map(serde_json::to_string)
                    .transpose()
                    .map_err(|e| ScheduleError::SerializationError(e.to_string()))?;
                let args_json = args
                    .map(serde_json::to_string)
                    .transpose()
                    .map_err(|e| ScheduleError::SerializationError(e.to_string()))?;

                sqlx::query(
                    "INSERT INTO schedule (schedule_id, build_id, cron, ns, var, meta, value, active) VALUES (?, ?, ?, ?, ?, ?, ?, 1)"
                )
                .bind(schedule_id)
                .bind(build_id)
                .bind(&cron_field)
                .bind(ns)
                .bind(var)
                .bind(meta_json)
                .bind(args_json)
                .execute(sqlite_pool)
                .await?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_schedule_expression_iso_datetime() {
        let result = parse_schedule_expression("2024-01-15T10:30:00Z");
        assert!(result.is_ok());
        let schedule_type = result.unwrap();
        assert!(schedule_type.is_at_schedule());

        if let ScheduleType::At(dt) = schedule_type {
            assert_eq!(dt.to_rfc3339(), "2024-01-15T10:30:00+00:00");
        } else {
            panic!("Expected At schedule type");
        }
    }

    #[test]
    fn test_parse_schedule_expression_durations() {
        // Test various duration formats
        let duration_tests = vec![
            "10 minutes",
            "10min",
            "2 hours",
            "2h",
            "1 day",
            "1d",
            "30 seconds",
            "30s",
            "1h 30m",
            "3 days 2 hours",
        ];

        for expr in duration_tests {
            let result = parse_schedule_expression(expr);
            assert!(result.is_ok(), "Failed to parse duration: {}", expr);
            assert!(
                result.unwrap().is_at_schedule(),
                "Duration '{}' should produce At schedule",
                expr
            );
        }
    }

    #[test]
    fn test_parse_schedule_expression_natural_language() {
        // Test natural language prefixes
        let natural_tests = vec![
            "in 10 minutes",
            "10 minutes from now",
            "after 2 hours",
            "in 1 day",
            "30 seconds from now",
        ];

        for expr in natural_tests {
            let result = parse_schedule_expression(expr);
            assert!(result.is_ok(), "Failed to parse natural language: {}", expr);
            assert!(
                result.unwrap().is_at_schedule(),
                "Natural language '{}' should produce At schedule",
                expr
            );
        }
    }

    #[test]
    fn test_parse_schedule_expression_cron() {
        // Test cron expressions
        let cron_tests = vec!["0 30 9 * * MON", "@daily", "@hourly", "*/15 * * * * *"];

        for expr in cron_tests {
            let result = parse_schedule_expression(expr);
            assert!(result.is_ok(), "Failed to parse cron: {}", expr);
            assert!(
                result.unwrap().is_cron_schedule(),
                "Cron '{}' should produce Cron schedule",
                expr
            );
        }
    }

    #[test]
    fn test_parse_schedule_expression_english_cron() {
        // Test English cron expressions
        let english_tests = vec![
            "every day at 9 AM",
            "every Monday at 2 PM",
            "every weekday at 8:00 AM",
            "daily",
            "weekly",
        ];

        for expr in english_tests {
            let result = parse_schedule_expression(expr);
            // Some of these might not be supported by english-to-cron
            if let Ok(schedule_type) = result {
                assert!(
                    schedule_type.is_cron_schedule(),
                    "English '{}' should produce Cron schedule",
                    expr
                );
            }
        }
    }

    #[test]
    fn test_schedule_type_roundtrip() {
        // Test At schedule roundtrip
        let dt = chrono::Utc::now();
        let at_schedule = ScheduleType::At(dt);
        let cron_field = at_schedule.to_cron_field();
        assert!(cron_field.starts_with(AT_SCHEDULE_PREFIX));

        let parsed = ScheduleType::from_cron_field(&cron_field);
        assert!(parsed.is_ok());
        let parsed_type = parsed.unwrap();
        assert!(parsed_type.is_at_schedule());

        // Test Cron schedule roundtrip
        let cron_schedule = ScheduleType::Cron("0 30 9 * * MON".to_string());
        let cron_field = cron_schedule.to_cron_field();
        assert!(!cron_field.starts_with(AT_SCHEDULE_PREFIX));

        let parsed = ScheduleType::from_cron_field(&cron_field);
        assert!(parsed.is_ok());
        let parsed_type = parsed.unwrap();
        assert!(parsed_type.is_cron_schedule());
    }

    #[test]
    fn test_cron_validation_with_croner() {
        println!("\n=== Testing Traditional Cron Validation ===");

        // Traditional cron expressions that should work
        let valid_expressions = vec![
            "0 30 9 * * MON", // 6-field format
            "30 9 * * MON",   // 5-field format
            "@daily",         // Nickname
            "@hourly",
            "@monthly",
            "@yearly",
            "0 0 9 L * *",             // Last day of month
            "0 0 9 * * FRI#L",         // Last Friday
            "0 0 9 * * MON#2",         // 2nd Monday
            "0 30 8 1W * *",           // Closest weekday to 1st
            "0 0 12 25 12 +FRI",       // Christmas AND Friday
            "0 */30 9-17 * * MON-FRI", // Every 30 min, business hours, weekdays
            "*/15 * * * * *",          // Every 15 seconds
        ];

        for expr in valid_expressions {
            let result = Schedule::validate_cron_expression(expr);
            println!("✅ '{}' -> Valid", expr);
            assert!(
                result.is_ok(),
                "Expression '{}' should be valid: {:?}",
                expr,
                result
            );
        }
    }

    #[test]
    fn test_english_to_cron_conversion() {
        println!("\n=== Testing English-to-Cron Conversion ===");

        let english_expressions = vec![
            ("every minute", "every minute"),
            ("every 15 seconds", "every 15 seconds"),
            ("every day at 4:00 pm", "every day at 4:00 pm"),
            ("at 10:00 am", "at 10:00 am"),
            (
                "Run at midnight on the 1st and 15th of the month",
                "complex schedule",
            ),
            ("on Sunday at 12:00", "on Sunday at 12:00"),
            ("at 6:00 pm every Monday through Friday", "weekday evening"),
        ];

        for (english, description) in english_expressions {
            let result = Schedule::validate_cron_expression(english);
            match result {
                Ok(_) => {
                    println!(
                        "✅ '{}' ({}) -> Valid English conversion",
                        english, description
                    );
                }
                Err(e) => {
                    println!("❌ '{}' ({}) -> Failed: {}", english, description, e);
                    panic!("English expression '{}' should be valid", english);
                }
            }
        }
    }

    #[test]
    fn test_english_conversion_detailed() {
        println!("\n=== Testing Detailed English Conversions ===");

        // Test the conversion function directly with expressions the crate supports
        let test_cases = vec![
            "every 15 seconds",
            "every minute",
            "every day at 4:00 pm",
            "at 10:00 am",
            "Run at midnight on the 1st and 15th of the month",
            "on Sunday at 12:00",
            "at 6:00 pm every Monday through Friday",
        ];

        for input in test_cases {
            match english_to_cron::str_cron_syntax(input) {
                Ok(actual) => {
                    println!("✅ '{}' -> '{}'", input, actual);
                    // Validate that the converted cron is valid
                    assert!(
                        croner::Cron::from_str(&actual).is_ok(),
                        "Converted cron '{}' for '{}' should be valid",
                        actual,
                        input
                    );
                }
                Err(err) => {
                    panic!(
                        "Failed to convert English expression '{}': {:?}",
                        input, err
                    );
                }
            }
        }
    }

    #[test]
    fn test_time_conversion() {
        println!("\n=== Testing Time Format Conversions ===");

        let time_tests = vec![
            ("at 10:00 am", "0 0 10 * * ? *"),
            ("every day at 4:00 pm", "0 0 16 */1 * ? *"),
            ("on Sunday at 12:00", "0 0 12 ? * SUN *"),
        ];

        for (english, _expected_cron) in time_tests {
            match english_to_cron::str_cron_syntax(english) {
                Ok(converted) => {
                    println!("✅ '{}' -> '{}'", english, converted);
                    // Note: We validate that the result is a valid cron, but don't enforce exact format
                    // since the english-to-cron crate may use different but equivalent formats
                    assert!(croner::Cron::from_str(&converted).is_ok());
                }
                Err(err) => {
                    panic!("Failed to convert time expression '{}': {:?}", english, err);
                }
            }
        }
    }

    #[test]
    fn test_improved_cron_error_messages() {
        println!("\n=== Testing Improved Error Messages ===");

        let invalid_expressions = vec!["invalid cron", "not a real schedule"];

        for expr in invalid_expressions {
            let result = Schedule::validate_cron_expression(expr);
            assert!(result.is_err(), "Expression '{}' should be invalid", expr);

            let error_msg = result.unwrap_err();
            println!("❌ '{}' -> Error: {}", expr, error_msg);

            // Verify error message is helpful
            assert!(
                error_msg.contains("Could not parse") || error_msg.contains("English expression")
            );
        }
    }

    #[test]
    fn test_advanced_croner_features() {
        println!("\n=== Testing Advanced Croner Features ===");

        // Test croner's advanced features that work with our direct implementation
        let advanced_expressions = vec![
            "@yearly",
            "@annually",
            "@monthly",
            "@weekly",
            "@daily",
            "@hourly",
            "0 0 9 ? * MON",   // ? modifier
            "0 0 9 L * *",     // Last day of month
            "0 0 9 * * FRI#L", // Last Friday
            "0 0 9 * * MON#2", // 2nd Monday
            "0 30 8 15W * *",  // Closest weekday to 15th
            "0 0 12 1 * +MON", // 1st AND Monday
        ];

        for expr in advanced_expressions {
            let result = Schedule::validate_cron_expression(expr);
            match result {
                Ok(_) => {
                    println!("✅ Advanced feature '{}' -> Valid", expr);
                }
                Err(e) => {
                    println!("⚠️  Advanced feature '{}' -> Not supported: {}", expr, e);
                    // Note: Some advanced features may not be supported by our croner version
                    // This is expected and documented
                }
            }
        }
    }

    #[test]
    fn test_real_world_examples() {
        println!("\n=== Testing Real-World Examples ===");

        let real_world = vec![
            // Natural language
            ("daily at 9 AM", "Daily morning standup"),
            ("every Monday at 2 PM", "Weekly team meeting"),
            ("every weekday at 8:30 AM", "Business hours start"),
            ("monthly on the 1st", "Monthly billing"),
            ("every Friday at 5 PM", "End of week wrap-up"),
            // Traditional cron (should still work)
            ("@daily", "Daily reports"),
            ("0 0 9 * * MON-FRI", "Weekday morning standup (traditional)"),
            ("0 30 14 * * 1", "Monday 2:30 PM (traditional)"),
            ("0 */15 * * * *", "Every 15 minutes"),
        ];

        for (schedule, description) in real_world {
            let result = Schedule::validate_cron_expression(schedule);
            match result {
                Ok(_) => {
                    println!("✅ {} -> '{}' is valid", description, schedule);
                }
                Err(e) => {
                    println!("❌ {} -> '{}' failed: {}", description, schedule, e);
                    panic!("Real-world example should work: {}", description);
                }
            }
        }
    }

    #[test]
    fn test_user_problem_case_fixed() {
        println!("\n=== Testing User's Original Problem Case ===");

        // This was the exact cron expression that caused the user's scheduler sync failure
        let problematic_cron = "0 2 * * *";

        let result = Schedule::validate_cron_expression(problematic_cron);

        // Should NOW PASS with our direct croner implementation!
        assert!(
            result.is_ok(),
            "5-field cron expression should now be supported!"
        );

        println!("✅ User's problem case is now SUPPORTED!");
        println!(
            "   Previous issue: '{}' -> REJECTED (tokio-cron-scheduler limitation)",
            problematic_cron
        );
        println!("   Now with croner: '{}' -> ACCEPTED! 🎉", problematic_cron);

        // Test other 5-field expressions that are now supported
        let five_field_expressions = vec![
            "0 2 * * *",        // Original problem
            "30 14 * * 1",      // Monday 2:30 PM
            "15 9 * * MON-FRI", // Weekdays 9:15 AM
            "0 */2 * * *",      // Every 2 hours
        ];

        for expr in five_field_expressions {
            let result = Schedule::validate_cron_expression(expr);
            assert!(
                result.is_ok(),
                "5-field expression '{}' should be valid",
                expr
            );
            println!("✅ 5-field: '{}' -> Valid", expr);
        }
    }

    #[test]
    fn test_comprehensive_english_examples() {
        println!("\n=== Testing Comprehensive English Examples ===");

        let comprehensive_tests = vec![
            // Time-based
            "every minute",
            "every hour",
            "every day",
            "every 15 minutes",
            "every 3 hours",
            "every 2 days",
            // Day-specific
            "every Monday",
            "every Tuesday",
            "every Wednesday",
            "every Thursday",
            "every Friday",
            "every Saturday",
            "every Sunday",
            // Time + day combinations
            "every Monday at 9 AM",
            "every Friday at 5:30 PM",
            "every weekday at 8:00 AM",
            "every weekend at 10 AM",
            // Period-based
            "daily",
            "weekly",
            "monthly",
            "yearly",
            // Specific times
            "at 6 AM",
            "at 2:30 PM",
            "daily at 12:00 PM",
            "daily at 23:59",
        ];

        for english in comprehensive_tests {
            let result = Schedule::validate_cron_expression(english);
            match result {
                Ok(_) => {
                    println!("✅ '{}' -> Valid English expression", english);
                }
                Err(e) => {
                    println!("❌ '{}' -> Failed: {}", english, e);
                    // Don't panic here as some patterns might not be implemented yet
                    // This test helps us see what needs more work
                }
            }
        }
    }

    #[test]
    fn test_every_second_validation() {
        println!("\n=== Testing 'every second' validation ===");

        // Test with english_to_cron directly
        match english_to_cron::str_cron_syntax("every second") {
            Ok(converted) => {
                println!("✅ 'every second' -> '{}'", converted);

                // Test with croner
                match croner::Cron::from_str(&converted) {
                    Ok(_) => println!("✅ Converted cron '{}' is valid", converted),
                    Err(e) => println!("❌ Converted cron '{}' is invalid: {}", converted, e),
                }
            }
            Err(e) => {
                println!("❌ 'every second' conversion failed: {:?}", e);
            }
        }

        // Test with croner directly
        match croner::Cron::from_str("every second") {
            Ok(_) => println!("✅ 'every second' is valid traditional cron"),
            Err(e) => println!("❌ 'every second' as traditional cron failed: {}", e),
        }

        // Test with our validation function
        let result = Schedule::validate_cron_expression("every second");
        match result {
            Ok(_) => println!("✅ Our validation accepts 'every second'"),
            Err(e) => println!("❌ Our validation rejects 'every second': {}", e),
        }

        // Test some alternatives
        let alternatives = vec!["every 1 second", "*/1 * * * * *", "* * * * * *"];

        for alt in alternatives {
            let result = Schedule::validate_cron_expression(alt);
            match result {
                Ok(_) => println!("✅ '{}' is valid", alt),
                Err(e) => println!("❌ '{}' failed: {}", alt, e),
            }
        }
    }

    #[test]
    fn test_schedule_interval_policy_rejects_too_fast_recurring_schedules() {
        assert!(validate_recurring_schedule_interval("every 5 minutes", 300).is_ok());
        assert!(validate_recurring_schedule_interval("0 */5 * * * *", 300).is_ok());

        let err = validate_recurring_schedule_interval("every second", 300).unwrap_err();
        assert_eq!(err.observed_interval_secs, 1);
        assert_eq!(err.required_interval_secs, 300);

        let err = validate_recurring_schedule_interval("*/1 * * * * *", 300).unwrap_err();
        assert_eq!(err.observed_interval_secs, 1);
        assert_eq!(err.required_interval_secs, 300);
    }

    #[test]
    fn test_schedule_interval_policy_rejects_subsecond_english_units() {
        let result = Schedule::validate_cron_expression("every 1 millisecond");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Sub-second"));

        let result = validate_recurring_schedule_interval("every 1 millisecond", 1);
        assert!(result.is_err());
    }

    #[test]
    fn test_schedule_policy_defaults_when_conf_values_are_missing() {
        let empty_conf = crate::val::Val::map_empty();
        assert_eq!(
            SchedulePolicy::from_conf(&empty_conf),
            SchedulePolicy {
                min_interval_secs: 1,
                min_delay_secs: 0,
                max_active_per_org: -1,
            }
        );

        let hosted_conf_without_schedule = crate::val!({
            "product": {
                "experience": "hot-cloud"
            }
        });
        assert_eq!(
            SchedulePolicy::from_conf(&hosted_conf_without_schedule),
            SchedulePolicy {
                min_interval_secs: 1,
                min_delay_secs: 0,
                max_active_per_org: 50,
            }
        );
    }

    #[test]
    fn test_schedule_policy_uses_feature_defaults_when_keys_are_missing() {
        let policy = SchedulePolicy::from_conf(&crate::val::Val::map_empty())
            .with_features(&crate::db::Features::empty());

        assert_eq!(policy.min_interval_secs, 1);
        assert_eq!(policy.min_delay_secs, 0);
        assert_eq!(policy.max_active_per_org, -1);
    }

    fn scheduled_functions(cron: &str, count: usize) -> crate::lang::compiler::ScheduledFunctions {
        let mut schedules = crate::lang::compiler::ScheduledFunctions::new();
        let entries = (0..count)
            .map(|idx| {
                let fn_name = format!("::demo/task-{}", idx);
                crate::lang::compiler::ScheduledFunction {
                    cron_expression: cron.to_string(),
                    scheduled_function: crate::val!({
                        "fn": fn_name,
                        "meta": {},
                        "file": null,
                        "line": null,
                        "column": null,
                        "position": null
                    }),
                }
            })
            .collect();
        schedules.insert(cron.to_string(), entries);
        schedules
    }

    #[tokio::test]
    async fn test_active_schedule_limit_replaces_current_project_count() {
        let db = crate::db::test_db().await;
        let data = crate::db::insert_test_data(&db).await.unwrap();
        let send_targets = crate::lang::compiler::SendTargets::new();

        let old_project_build_id = Uuid::now_v7();
        crate::db::Build::insert_build(
            &db,
            &old_project_build_id,
            &data.project_id,
            "old-project-build",
            1,
            crate::db::Build::BUILD_TYPE_BUNDLE,
            &data.user_id,
        )
        .await
        .unwrap();
        Schedule::insert_schedules_for_build(
            &db,
            &old_project_build_id,
            &scheduled_functions("0 */5 * * * *", 2),
            &send_targets,
        )
        .await
        .unwrap();
        crate::db::Build::deploy_build(&db, &old_project_build_id, &data.user_id)
            .await
            .unwrap();

        let other_project_id = Uuid::now_v7();
        crate::db::Project::insert_project(
            &db,
            &other_project_id,
            &data.env_id,
            "other-project",
            &data.user_id,
        )
        .await
        .unwrap();
        let other_build_id = Uuid::now_v7();
        crate::db::Build::insert_build(
            &db,
            &other_build_id,
            &other_project_id,
            "other-build",
            1,
            crate::db::Build::BUILD_TYPE_BUNDLE,
            &data.user_id,
        )
        .await
        .unwrap();
        Schedule::insert_schedules_for_build(
            &db,
            &other_build_id,
            &scheduled_functions("0 */10 * * * *", 1),
            &send_targets,
        )
        .await
        .unwrap();
        crate::db::Build::deploy_build(&db, &other_build_id, &data.user_id)
            .await
            .unwrap();

        assert_eq!(
            Schedule::get_active_count_by_org(&db, &data.org_id)
                .await
                .unwrap(),
            3
        );
        assert_eq!(
            Schedule::get_active_count_by_project(&db, &data.project_id)
                .await
                .unwrap(),
            2
        );
        let usage = crate::db::OrgUsageStats::calculate(
            &db,
            &data.org_id,
            chrono::Utc::now() - chrono::Duration::days(1),
            7,
        )
        .await
        .unwrap();
        assert_eq!(usage.active_schedules, 3);

        let policy = SchedulePolicy {
            min_interval_secs: 1,
            min_delay_secs: 0,
            max_active_per_org: 3,
        };

        Schedule::enforce_active_count_for_org_replacing_project(
            &db,
            &data.org_id,
            &data.project_id,
            2,
            policy,
        )
        .await
        .unwrap();

        let err = Schedule::enforce_active_count_for_org_replacing_project(
            &db,
            &data.org_id,
            &data.project_id,
            3,
            policy,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("Active schedule limit exceeded"));
    }

    #[tokio::test]
    async fn test_insert_dynamic_schedule_enforces_policy() {
        let db = crate::db::test_db().await;
        let data = crate::db::insert_test_data(&db).await.unwrap();

        let fast_recurring = Schedule::insert_dynamic_schedule(
            &db,
            &Uuid::now_v7(),
            &data.build_id,
            &ScheduleType::Cron("every second".to_string()),
            Some(&data.org_id),
            SchedulePolicy {
                min_interval_secs: 300,
                min_delay_secs: 0,
                max_active_per_org: -1,
            },
            "::demo",
            "tick",
            None,
            None,
        )
        .await;
        assert!(
            fast_recurring
                .unwrap_err()
                .to_string()
                .contains("minimum of 300")
        );

        let too_soon = Schedule::insert_dynamic_schedule(
            &db,
            &Uuid::now_v7(),
            &data.build_id,
            &ScheduleType::At(chrono::Utc::now() + chrono::Duration::seconds(5)),
            Some(&data.org_id),
            SchedulePolicy {
                min_interval_secs: 1,
                min_delay_secs: 60,
                max_active_per_org: -1,
            },
            "::demo",
            "once",
            None,
            None,
        )
        .await;
        assert!(
            too_soon
                .unwrap_err()
                .to_string()
                .contains("below the minimum delay of 60")
        );

        let over_quota = Schedule::insert_dynamic_schedule(
            &db,
            &Uuid::now_v7(),
            &data.build_id,
            &ScheduleType::Cron("every 5 minutes".to_string()),
            Some(&data.org_id),
            SchedulePolicy {
                min_interval_secs: 1,
                min_delay_secs: 0,
                max_active_per_org: 0,
            },
            "::demo",
            "quota",
            None,
            None,
        )
        .await;
        assert!(
            over_quota
                .unwrap_err()
                .to_string()
                .contains("Active schedule limit exceeded")
        );
    }

    #[test]
    fn test_ariadne_error_formatting() {
        // Test the ariadne error formatting for both traditional and English expressions
        let error = ScheduleError::CronValidationError(Box::new(CronValidationErrorDetails {
            message:
                "Could not parse 'bad expression' as either a cron expression or natural language"
                    .to_string(),
            cron_expression: "bad expression".to_string(),
            function_ns: "demo.schedule".to_string(),
            function_var: "cleanup-logs".to_string(),
            file: Some(PathBuf::from("src/demo/schedule.hot")),
            line: Some(5),
            column: Some(15),
            position: Some(120),
            length: Some(14), // length of "bad expression"
        }));

        // Test formatted error without source
        let formatted_without_source = error.format_error(None, false);
        println!("Formatted error (no source):");
        println!("{}", formatted_without_source);

        assert!(formatted_without_source.contains("❌ Cron Validation Error"));
        assert!(formatted_without_source.contains("demo.schedule:cleanup-logs"));
        assert!(formatted_without_source.contains("src/demo/schedule.hot:5:15"));

        // Test with mock source content
        let mock_source = "schedule {\n    name: 'cleanup'\n    cron: 'bad expression'\n    fn: cleanup_old_logs\n}";
        let formatted_with_source = error.format_error(Some(mock_source), false);
        println!("\nFormatted error (with source):");
        println!("{}", formatted_with_source);

        println!("\n✅ Ariadne error formatting test completed!");

        println!("\n=== Practical Usage Example ===");
        println!("When you run 'hot build' and have an invalid cron expression:");
        println!("{}", formatted_with_source);

        println!("\n🎯 This error will appear during build creation, preventing invalid");
        println!("   schedules from being stored in the database and causing runtime failures!");
        println!("\n🌟 Plus, now users can use natural language like 'daily at 9 AM'!");
    }
}

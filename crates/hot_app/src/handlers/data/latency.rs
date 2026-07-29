use crate::auth::Session;
use ahash::AHashMap;
use axum::extract::{Query, State};
use axum::response::{IntoResponse, Json};
use chrono::{DateTime, Utc};
use hot::db::DatabasePool;
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

const MAX_LATENCY_SAMPLES: usize = 100_000;
const LATENCY_QUERY_LIMIT: usize = MAX_LATENCY_SAMPLES + 1;

#[derive(Clone, Copy)]
enum LatencyKind {
    Run,
    Task,
}

impl LatencyKind {
    fn name(self) -> &'static str {
        match self {
            Self::Run => "run",
            Self::Task => "task",
        }
    }
}

#[derive(Clone, Default, Serialize)]
pub struct LatencyPair {
    waiting_ms: Option<f64>,
    execution_ms: Option<f64>,
}

#[derive(Default, Serialize)]
pub struct LatencySummary {
    sample_count: i64,
    precise_sample_count: i64,
    precise_coverage_percent: f64,
    truncated: bool,
    p50: LatencyPair,
    p95: LatencyPair,
    p99: LatencyPair,
}

#[derive(Default, Serialize)]
pub struct LatencyPercentileSeries {
    waiting_ms: Vec<Option<f64>>,
    execution_ms: Vec<Option<f64>>,
}

#[derive(Default, Serialize)]
pub struct LatencyTimelineData {
    dates: Vec<String>,
    sample_counts: Vec<i64>,
    p50: LatencyPercentileSeries,
    p95: LatencyPercentileSeries,
    p99: LatencyPercentileSeries,
    summary: LatencySummary,
}

#[derive(Clone, Default)]
struct LatencyBucket {
    date: Option<String>,
    sample_count: i64,
    precise_sample_count: i64,
    p50: LatencyPair,
    p95: LatencyPair,
    p99: LatencyPair,
    truncated: bool,
}

#[derive(Clone)]
struct LatencySample {
    date: String,
    waiting_ms: f64,
    execution_ms: f64,
    precise: bool,
}

struct LatencyFilters {
    days: Option<i64>,
    time_unit: String,
    project_id: Option<uuid::Uuid>,
}

impl LatencyFilters {
    fn from_params(params: &AHashMap<String, String>) -> Self {
        let time_range = params
            .get("time_range")
            .map(String::as_str)
            .unwrap_or("P1D");
        let days = match time_range {
            "all" => None,
            "P7D" => Some(7),
            "P30D" => Some(30),
            "P90D" => Some(90),
            _ => Some(1),
        };
        let default_time_unit = match days {
            Some(1) => "hour",
            Some(7 | 30) => "day",
            _ => "month",
        };
        let time_unit = match params.get("time_unit").map(String::as_str) {
            Some("hour") => "hour",
            Some("month") => "month",
            Some("day") => "day",
            _ => default_time_unit,
        }
        .to_string();
        let project_id = params
            .get("project_id")
            .and_then(|value| uuid::Uuid::parse_str(value).ok());

        Self {
            days,
            time_unit,
            project_id,
        }
    }
}

/// GET /data/run-latency-timeline
///
/// Waiting is event creation through handler execution start. Execution is
/// handler start through run completion. Task-backed runs are excluded because
/// tasks have their own latency widget.
pub async fn run_latency_timeline_handler(
    State(db): State<Arc<DatabasePool>>,
    Query(params): Query<AHashMap<String, String>>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> impl IntoResponse {
    latency_timeline_handler(db, params, session, LatencyKind::Run).await
}

/// GET /data/task-latency-timeline
///
/// New tasks use their persisted workload boundary. Older tasks fall back to
/// start/stop timestamps and are reflected in the precision coverage.
pub async fn task_latency_timeline_handler(
    State(db): State<Arc<DatabasePool>>,
    Query(params): Query<AHashMap<String, String>>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> impl IntoResponse {
    latency_timeline_handler(db, params, session, LatencyKind::Task).await
}

async fn latency_timeline_handler(
    db: Arc<DatabasePool>,
    params: AHashMap<String, String>,
    session: Session,
    kind: LatencyKind,
) -> axum::response::Response {
    let env_id = match session.current_env_id() {
        Some(id) => id,
        None => return Json(LatencyTimelineData::default()).into_response(),
    };
    let filters = LatencyFilters::from_params(&params);

    let buckets = match db.as_ref() {
        DatabasePool::Postgres(pool) => {
            query_postgres_latency(pool, env_id, &filters, &session.display_timezone, kind).await
        }
        DatabasePool::Sqlite(pool) => {
            query_sqlite_latency(pool, env_id, &filters, &session.display_timezone, kind).await
        }
    };

    match buckets {
        Ok(buckets) => Json(assemble_latency_timeline(
            buckets,
            filters.days,
            &filters.time_unit,
            &session.display_timezone,
        ))
        .into_response(),
        Err(error) => {
            tracing::error!(
                env_id = %env_id,
                latency_kind = kind.name(),
                "Failed to fetch dashboard latency data: {}",
                error
            );
            Json(LatencyTimelineData::default()).into_response()
        }
    }
}

type PostgresLatencyRow = (
    Option<DateTime<Utc>>,
    i64,
    i64,
    Option<f64>,
    Option<f64>,
    Option<f64>,
    Option<f64>,
    Option<f64>,
    Option<f64>,
    bool,
);

async fn query_postgres_latency(
    pool: &sqlx::PgPool,
    env_id: uuid::Uuid,
    filters: &LatencyFilters,
    display_timezone: &str,
    kind: LatencyKind,
) -> Result<Vec<LatencyBucket>, sqlx::Error> {
    let timestamp_column = match kind {
        LatencyKind::Run => "e.created_at",
        LatencyKind::Task => "t.created_at",
    };
    let period = crate::timezone::postgres_date_trunc(
        &filters.time_unit,
        timestamp_column,
        display_timezone,
    );
    let project_placeholder = if filters.days.is_some() { "$3" } else { "$2" };
    let project_join = match (kind, filters.project_id.is_some()) {
        (LatencyKind::Run, true) => format!(
            "JOIN build latency_build ON r.build_id = latency_build.build_id AND latency_build.project_id = {project_placeholder}"
        ),
        (LatencyKind::Task, true) => format!(
            "JOIN build latency_build ON t.build_id = latency_build.build_id AND latency_build.project_id = {project_placeholder}"
        ),
        _ => String::new(),
    };
    let interval_filter = match (kind, filters.days.is_some()) {
        (LatencyKind::Run, true) => "AND e.created_at >= NOW() - ($2 || ' days')::INTERVAL",
        (LatencyKind::Task, true) => "AND t.created_at >= NOW() - ($2 || ' days')::INTERVAL",
        _ => "",
    };
    let candidate_sql =
        postgres_latency_candidate_sql(kind, &period, &project_join, interval_filter);
    let query = format!(
        r#"
        WITH candidate_samples AS (
            {candidate_sql}
        ),
        limited_samples AS (
            SELECT *
            FROM candidate_samples
            WHERE sample_created_at IS NOT NULL
                AND waiting_ms >= 0
                AND execution_ms >= 0
            ORDER BY sample_created_at DESC
            LIMIT {LATENCY_QUERY_LIMIT}
        ),
        samples AS (
            SELECT *
            FROM limited_samples
            ORDER BY sample_created_at DESC
            LIMIT {MAX_LATENCY_SAMPLES}
        )
        SELECT
            period,
            COUNT(*)::bigint AS sample_count,
            COUNT(*) FILTER (WHERE precise)::bigint AS precise_sample_count,
            percentile_cont(0.50) WITHIN GROUP (ORDER BY waiting_ms)::double precision,
            percentile_cont(0.50) WITHIN GROUP (ORDER BY execution_ms)::double precision,
            percentile_cont(0.95) WITHIN GROUP (ORDER BY waiting_ms)::double precision,
            percentile_cont(0.95) WITHIN GROUP (ORDER BY execution_ms)::double precision,
            percentile_cont(0.99) WITHIN GROUP (ORDER BY waiting_ms)::double precision,
            percentile_cont(0.99) WITHIN GROUP (ORDER BY execution_ms)::double precision,
            (SELECT COUNT(*) > {MAX_LATENCY_SAMPLES} FROM limited_samples) AS truncated
        FROM samples
        GROUP BY GROUPING SETS ((period), ())
        ORDER BY period ASC NULLS LAST
        "#
    );

    let mut query =
        sqlx::query_as::<_, PostgresLatencyRow>(sqlx::AssertSqlSafe(query.as_str())).bind(env_id);
    if let Some(days) = filters.days {
        query = query.bind(days);
    }
    if let Some(project_id) = filters.project_id {
        query = query.bind(project_id);
    }
    let rows = query.fetch_all(pool).await?;
    let date_format = crate::timezone::postgres_date_format(&filters.time_unit);

    Ok(rows
        .into_iter()
        .map(
            |(
                period,
                sample_count,
                precise_sample_count,
                p50_waiting,
                p50_execution,
                p95_waiting,
                p95_execution,
                p99_waiting,
                p99_execution,
                truncated,
            )| LatencyBucket {
                date: period.map(|value| {
                    crate::timezone::format_in_timezone(&value, display_timezone, date_format)
                }),
                sample_count,
                precise_sample_count,
                p50: LatencyPair {
                    waiting_ms: p50_waiting,
                    execution_ms: p50_execution,
                },
                p95: LatencyPair {
                    waiting_ms: p95_waiting,
                    execution_ms: p95_execution,
                },
                p99: LatencyPair {
                    waiting_ms: p99_waiting,
                    execution_ms: p99_execution,
                },
                truncated,
            },
        )
        .collect())
}

fn postgres_latency_candidate_sql(
    kind: LatencyKind,
    period: &str,
    project_join: &str,
    interval_filter: &str,
) -> String {
    match kind {
        LatencyKind::Run => format!(
            r#"
            SELECT
                {period} AS period,
                e.created_at AS sample_created_at,
                EXTRACT(EPOCH FROM (r.start_time - e.created_at)) * 1000.0 AS waiting_ms,
                EXTRACT(EPOCH FROM (r.stop_time - r.start_time)) * 1000.0 AS execution_ms,
                TRUE AS precise
            FROM run r
            JOIN event e ON r.event_id = e.event_id AND e.env_id = $1
            {project_join}
            WHERE r.env_id = $1
                AND r.run_type_id != 7
                AND r.stop_time IS NOT NULL
                {interval_filter}
            "#
        ),
        LatencyKind::Task => format!(
            r#"
            SELECT
                {period} AS period,
                t.created_at AS sample_created_at,
                COALESCE(
                    (t.timing->>'waiting_ms')::double precision,
                    EXTRACT(EPOCH FROM (t.start_time - t.created_at)) * 1000.0
                ) AS waiting_ms,
                COALESCE(
                    (t.timing->>'execution_ms')::double precision,
                    t.duration_ms::double precision,
                    EXTRACT(EPOCH FROM (t.stop_time - t.start_time)) * 1000.0
                ) AS execution_ms,
                (
                    t.timing->>'waiting_ms' IS NOT NULL
                    AND t.timing->>'execution_ms' IS NOT NULL
                ) AS precise
            FROM task t
            {project_join}
            WHERE t.env_id = $1
                AND t.stop_time IS NOT NULL
                {interval_filter}
            "#
        ),
    }
}

async fn query_sqlite_latency(
    pool: &sqlx::SqlitePool,
    env_id: uuid::Uuid,
    filters: &LatencyFilters,
    display_timezone: &str,
    kind: LatencyKind,
) -> Result<Vec<LatencyBucket>, sqlx::Error> {
    let timestamp_column = match kind {
        LatencyKind::Run => "e.created_at",
        LatencyKind::Task => "t.created_at",
    };
    let period =
        crate::timezone::sqlite_date_bucket(&filters.time_unit, timestamp_column, display_timezone);
    let project_placeholder = if filters.days.is_some() { "?3" } else { "?2" };
    let project_join = match (kind, filters.project_id.is_some()) {
        (LatencyKind::Run, true) => format!(
            "JOIN build latency_build ON r.build_id = latency_build.build_id AND latency_build.project_id = {project_placeholder}"
        ),
        (LatencyKind::Task, true) => format!(
            "JOIN build latency_build ON t.build_id = latency_build.build_id AND latency_build.project_id = {project_placeholder}"
        ),
        _ => String::new(),
    };
    let interval_filter = match (kind, filters.days.is_some()) {
        (LatencyKind::Run, true) => "AND e.created_at >= datetime('now', '-' || ?2 || ' days')",
        (LatencyKind::Task, true) => "AND t.created_at >= datetime('now', '-' || ?2 || ' days')",
        _ => "",
    };
    let candidate_sql = sqlite_latency_candidate_sql(kind, &period, &project_join, interval_filter);
    let query = format!(
        r#"
        SELECT period, waiting_ms, execution_ms, precise
        FROM ({candidate_sql})
        WHERE sample_created_at IS NOT NULL
            AND waiting_ms >= 0
            AND execution_ms >= 0
        ORDER BY sample_created_at DESC
        LIMIT {LATENCY_QUERY_LIMIT}
        "#
    );

    let mut query =
        sqlx::query_as::<_, (String, f64, f64, bool)>(sqlx::AssertSqlSafe(query.as_str()))
            .bind(env_id);
    if let Some(days) = filters.days {
        query = query.bind(days);
    }
    if let Some(project_id) = filters.project_id {
        query = query.bind(project_id);
    }
    let rows = query.fetch_all(pool).await?;
    let truncated = rows.len() > MAX_LATENCY_SAMPLES;
    let samples = rows
        .into_iter()
        .take(MAX_LATENCY_SAMPLES)
        .map(|(date, waiting_ms, execution_ms, precise)| LatencySample {
            date,
            waiting_ms,
            execution_ms,
            precise,
        })
        .collect::<Vec<_>>();

    Ok(aggregate_latency_samples(samples, truncated))
}

fn sqlite_latency_candidate_sql(
    kind: LatencyKind,
    period: &str,
    project_join: &str,
    interval_filter: &str,
) -> String {
    match kind {
        LatencyKind::Run => format!(
            r#"
            SELECT
                {period} AS period,
                e.created_at AS sample_created_at,
                (julianday(r.start_time) - julianday(e.created_at)) * 86400000.0 AS waiting_ms,
                (julianday(r.stop_time) - julianday(r.start_time)) * 86400000.0 AS execution_ms,
                1 AS precise
            FROM run r
            JOIN event e ON r.event_id = e.event_id AND e.env_id = ?1
            {project_join}
            WHERE r.env_id = ?1
                AND r.run_type_id != 7
                AND r.stop_time IS NOT NULL
                {interval_filter}
            "#
        ),
        LatencyKind::Task => format!(
            r#"
            SELECT
                {period} AS period,
                t.created_at AS sample_created_at,
                COALESCE(
                    CAST(json_extract(t.timing, '$.waiting_ms') AS REAL),
                    (julianday(t.start_time) - julianday(t.created_at)) * 86400000.0
                ) AS waiting_ms,
                COALESCE(
                    CAST(json_extract(t.timing, '$.execution_ms') AS REAL),
                    CAST(t.duration_ms AS REAL),
                    (julianday(t.stop_time) - julianday(t.start_time)) * 86400000.0
                ) AS execution_ms,
                (
                    json_extract(t.timing, '$.waiting_ms') IS NOT NULL
                    AND json_extract(t.timing, '$.execution_ms') IS NOT NULL
                ) AS precise
            FROM task t
            {project_join}
            WHERE t.env_id = ?1
                AND t.stop_time IS NOT NULL
                {interval_filter}
            "#
        ),
    }
}

fn aggregate_latency_samples(samples: Vec<LatencySample>, truncated: bool) -> Vec<LatencyBucket> {
    let mut by_date: BTreeMap<String, Vec<&LatencySample>> = BTreeMap::new();
    for sample in &samples {
        by_date.entry(sample.date.clone()).or_default().push(sample);
    }

    let mut buckets = by_date
        .into_iter()
        .map(|(date, samples)| latency_bucket(Some(date), &samples, truncated))
        .collect::<Vec<_>>();
    let all_samples = samples.iter().collect::<Vec<_>>();
    buckets.push(latency_bucket(None, &all_samples, truncated));
    buckets
}

fn latency_bucket(
    date: Option<String>,
    samples: &[&LatencySample],
    truncated: bool,
) -> LatencyBucket {
    let waiting = samples
        .iter()
        .map(|sample| sample.waiting_ms)
        .collect::<Vec<_>>();
    let execution = samples
        .iter()
        .map(|sample| sample.execution_ms)
        .collect::<Vec<_>>();

    LatencyBucket {
        date,
        sample_count: samples.len() as i64,
        precise_sample_count: samples.iter().filter(|sample| sample.precise).count() as i64,
        p50: LatencyPair {
            waiting_ms: percentile_cont(&waiting, 0.50),
            execution_ms: percentile_cont(&execution, 0.50),
        },
        p95: LatencyPair {
            waiting_ms: percentile_cont(&waiting, 0.95),
            execution_ms: percentile_cont(&execution, 0.95),
        },
        p99: LatencyPair {
            waiting_ms: percentile_cont(&waiting, 0.99),
            execution_ms: percentile_cont(&execution, 0.99),
        },
        truncated,
    }
}

fn percentile_cont(values: &[f64], percentile: f64) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let position = (sorted.len() - 1) as f64 * percentile;
    let lower = position.floor() as usize;
    let upper = position.ceil() as usize;
    if lower == upper {
        return Some(sorted[lower]);
    }
    let fraction = position - lower as f64;
    Some(sorted[lower] + (sorted[upper] - sorted[lower]) * fraction)
}

fn assemble_latency_timeline(
    buckets: Vec<LatencyBucket>,
    days: Option<i64>,
    time_unit: &str,
    display_timezone: &str,
) -> LatencyTimelineData {
    let summary_bucket = buckets
        .iter()
        .find(|bucket| bucket.date.is_none())
        .cloned()
        .unwrap_or_default();
    let by_date = buckets
        .into_iter()
        .filter_map(|bucket| bucket.date.clone().map(|date| (date, bucket)))
        .collect::<BTreeMap<_, _>>();
    let dates = match days {
        Some(days) => crate::timezone::generate_time_buckets(time_unit, days, display_timezone),
        None => by_date
            .keys()
            .cloned()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect(),
    };

    let pair_series = |select: fn(&LatencyBucket) -> &LatencyPair| LatencyPercentileSeries {
        waiting_ms: dates
            .iter()
            .map(|date| {
                by_date
                    .get(date)
                    .and_then(|bucket| select(bucket).waiting_ms)
            })
            .collect(),
        execution_ms: dates
            .iter()
            .map(|date| {
                by_date
                    .get(date)
                    .and_then(|bucket| select(bucket).execution_ms)
            })
            .collect(),
    };
    let precise_coverage_percent = if summary_bucket.sample_count == 0 {
        0.0
    } else {
        summary_bucket.precise_sample_count as f64 / summary_bucket.sample_count as f64 * 100.0
    };

    LatencyTimelineData {
        sample_counts: dates
            .iter()
            .map(|date| {
                by_date
                    .get(date)
                    .map(|bucket| bucket.sample_count)
                    .unwrap_or(0)
            })
            .collect(),
        p50: pair_series(|bucket| &bucket.p50),
        p95: pair_series(|bucket| &bucket.p95),
        p99: pair_series(|bucket| &bucket.p99),
        dates,
        summary: LatencySummary {
            sample_count: summary_bucket.sample_count,
            precise_sample_count: summary_bucket.precise_sample_count,
            precise_coverage_percent,
            truncated: summary_bucket.truncated,
            p50: summary_bucket.p50,
            p95: summary_bucket.p95,
            p99: summary_bucket.p99,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn continuous_percentiles_match_postgres_interpolation() {
        let values = [0.0, 10.0, 20.0, 30.0];
        assert_eq!(percentile_cont(&values, 0.50), Some(15.0));
        assert!((percentile_cont(&values, 0.95).expect("p95") - 28.5).abs() < 1e-12);
    }

    #[test]
    fn latency_sql_is_tenant_scoped_and_separates_tasks() {
        let run_sql = postgres_latency_candidate_sql(
            LatencyKind::Run,
            "period",
            "",
            "AND e.created_at >= cutoff",
        );
        assert!(run_sql.contains("JOIN event e ON r.event_id = e.event_id AND e.env_id = $1"));
        assert!(run_sql.contains("WHERE r.env_id = $1"));
        assert!(run_sql.contains("r.run_type_id != 7"));

        let task_sql = sqlite_latency_candidate_sql(LatencyKind::Task, "period", "", "");
        assert!(task_sql.contains("WHERE t.env_id = ?1"));
        assert!(task_sql.contains("$.waiting_ms"));
        assert!(task_sql.contains("$.execution_ms"));
    }

    #[test]
    fn task_precision_coverage_distinguishes_legacy_fallbacks() {
        let samples = vec![
            LatencySample {
                date: "2026-07-29".to_string(),
                waiting_ms: 10.0,
                execution_ms: 20.0,
                precise: true,
            },
            LatencySample {
                date: "2026-07-29".to_string(),
                waiting_ms: 30.0,
                execution_ms: 40.0,
                precise: false,
            },
        ];
        let data = assemble_latency_timeline(
            aggregate_latency_samples(samples, false),
            None,
            "day",
            "UTC",
        );
        assert_eq!(data.summary.sample_count, 2);
        assert_eq!(data.summary.precise_sample_count, 1);
        assert_eq!(data.summary.precise_coverage_percent, 50.0);
        assert_eq!(data.summary.p50.waiting_ms, Some(20.0));
    }
}

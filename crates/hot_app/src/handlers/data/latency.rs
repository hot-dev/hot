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
    last_bucket_partial: bool,
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

struct LatencyQueryResult {
    buckets: Vec<LatencyBucket>,
    negative_sample_count: i64,
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
        Ok(result) => {
            if result.negative_sample_count > 0 {
                tracing::warn!(
                    env_id = %env_id,
                    project_id = ?filters.project_id,
                    latency_kind = kind.name(),
                    negative_sample_count = result.negative_sample_count,
                    "Discarded negative dashboard latency samples; check clock synchronization and stored timing boundaries"
                );
            }
            let requested_dates = filters.days.map(|days| {
                crate::timezone::generate_time_buckets(
                    &filters.time_unit,
                    days,
                    &session.display_timezone,
                )
            });
            Json(assemble_latency_timeline(result.buckets, requested_dates)).into_response()
        }
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
    i64,
);

async fn query_postgres_latency(
    pool: &sqlx::PgPool,
    env_id: uuid::Uuid,
    filters: &LatencyFilters,
    display_timezone: &str,
    kind: LatencyKind,
) -> Result<LatencyQueryResult, sqlx::Error> {
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
        observed_samples AS (
            SELECT *
            FROM candidate_samples
            WHERE sample_created_at IS NOT NULL
                AND waiting_ms IS NOT NULL
                AND execution_ms IS NOT NULL
            ORDER BY sample_created_at DESC
            LIMIT {LATENCY_QUERY_LIMIT}
        ),
        sample_stats AS (
            SELECT COUNT(*)::bigint AS negative_sample_count
            FROM observed_samples
            WHERE waiting_ms < 0 OR execution_ms < 0
        ),
        valid_samples AS (
            SELECT *
            FROM observed_samples
            WHERE waiting_ms >= 0
                AND execution_ms >= 0
        ),
        samples AS (
            SELECT *
            FROM valid_samples
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
            (SELECT COUNT(*) > {MAX_LATENCY_SAMPLES} FROM observed_samples) AS truncated,
            (SELECT negative_sample_count FROM sample_stats) AS negative_sample_count
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

    let negative_sample_count = rows.first().map(|row| row.10).unwrap_or_default();
    let buckets = rows
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
                _negative_sample_count,
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
        .collect();

    Ok(LatencyQueryResult {
        buckets,
        negative_sample_count,
    })
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
) -> Result<LatencyQueryResult, sqlx::Error> {
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
        WITH candidate_samples AS (
            {candidate_sql}
        ),
        observed_samples AS (
            SELECT *
            FROM candidate_samples
            WHERE sample_created_at IS NOT NULL
                AND waiting_ms IS NOT NULL
                AND execution_ms IS NOT NULL
            ORDER BY sample_created_at DESC
            LIMIT {LATENCY_QUERY_LIMIT}
        ),
        sample_stats AS (
            SELECT
                COUNT(*) AS observed_sample_count,
                COUNT(CASE WHEN waiting_ms < 0 OR execution_ms < 0 THEN 1 END) AS negative_sample_count
            FROM observed_samples
        ),
        limited_samples AS (
            SELECT period, waiting_ms, execution_ms, precise
            FROM observed_samples
            WHERE waiting_ms >= 0
                AND execution_ms >= 0
            ORDER BY sample_created_at DESC
            LIMIT {MAX_LATENCY_SAMPLES}
        )
        SELECT
            period,
            waiting_ms,
            execution_ms,
            precise,
            NULL AS observed_sample_count,
            NULL AS negative_sample_count
        FROM limited_samples
        UNION ALL
        SELECT NULL, NULL, NULL, NULL, observed_sample_count, negative_sample_count
        FROM sample_stats
        "#
    );

    let mut query = sqlx::query_as::<
        _,
        (
            Option<String>,
            Option<f64>,
            Option<f64>,
            Option<bool>,
            Option<i64>,
            Option<i64>,
        ),
    >(sqlx::AssertSqlSafe(query.as_str()))
    .bind(env_id);
    if let Some(days) = filters.days {
        query = query.bind(days);
    }
    if let Some(project_id) = filters.project_id {
        query = query.bind(project_id);
    }
    let rows = query.fetch_all(pool).await?;
    let observed_sample_count = rows
        .iter()
        .find_map(|(_, _, _, _, count, _)| *count)
        .unwrap_or_default();
    let negative_sample_count = rows
        .iter()
        .find_map(|(_, _, _, _, _, count)| *count)
        .unwrap_or_default();
    let truncated = observed_sample_count > MAX_LATENCY_SAMPLES as i64;
    let samples = rows
        .into_iter()
        .filter_map(|(date, waiting_ms, execution_ms, precise, _, _)| {
            Some(LatencySample {
                date: date?,
                waiting_ms: waiting_ms?,
                execution_ms: execution_ms?,
                precise: precise?,
            })
        })
        .take(MAX_LATENCY_SAMPLES)
        .collect::<Vec<_>>();

    Ok(LatencyQueryResult {
        buckets: aggregate_latency_samples(samples, truncated),
        negative_sample_count,
    })
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
    requested_dates: Option<Vec<String>>,
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
    // A capped query contains only the newest samples. Generating the full
    // requested range in that case would turn omitted, pre-cap periods into
    // misleading zero-activity buckets.
    let dates = match (&requested_dates, summary_bucket.truncated) {
        (Some(dates), false) => dates.clone(),
        _ => by_date
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
    let last_bucket_partial = requested_dates
        .as_ref()
        .and_then(|dates| dates.last())
        .is_some_and(|current_bucket| dates.last() == Some(current_bucket));

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
        last_bucket_partial,
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

    #[tokio::test]
    async fn sqlite_latency_counts_and_discards_negative_samples() {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("in-memory SQLite should open");
        sqlx::query(
            r#"
            CREATE TABLE task (
                env_id BLOB NOT NULL,
                created_at DATETIME NOT NULL,
                start_time DATETIME,
                stop_time DATETIME,
                duration_ms INTEGER,
                timing TEXT
            )
            "#,
        )
        .execute(&pool)
        .await
        .expect("task fixture table should be created");

        let env_id = uuid::Uuid::now_v7();
        let now = chrono::Utc::now();
        for waiting_ms in [10.0, -5.0] {
            sqlx::query(
                "INSERT INTO task (env_id, created_at, start_time, stop_time, duration_ms, timing) VALUES (?, ?, ?, ?, ?, ?)",
            )
            .bind(env_id)
            .bind(now - chrono::Duration::milliseconds(30))
            .bind(now - chrono::Duration::milliseconds(20))
            .bind(now)
            .bind(20_i64)
            .bind(serde_json::json!({
                "waiting_ms": waiting_ms,
                "execution_ms": 20.0,
            }).to_string())
            .execute(&pool)
            .await
            .expect("latency fixture should insert");
        }

        let result = query_sqlite_latency(
            &pool,
            env_id,
            &LatencyFilters {
                days: None,
                time_unit: "day".to_string(),
                project_id: None,
            },
            "UTC",
            LatencyKind::Task,
        )
        .await
        .expect("latency query should succeed");

        assert_eq!(result.negative_sample_count, 1);
        let summary = result
            .buckets
            .iter()
            .find(|bucket| bucket.date.is_none())
            .expect("summary bucket should exist");
        assert_eq!(summary.sample_count, 1);
        assert_eq!(summary.p50.waiting_ms, Some(10.0));
    }

    #[tokio::test]
    async fn postgres_latency_counts_and_discards_negative_samples_when_configured() {
        let Ok(uri) = std::env::var("HOT_TEST_POSTGRES_URI") else {
            eprintln!("skipping: HOT_TEST_POSTGRES_URI is not set");
            return;
        };

        let schema = format!("hot_latency_test_{}", uuid::Uuid::now_v7().simple());
        let admin_pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect(&uri)
            .await
            .expect("PostgreSQL test database should connect");
        sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
            .execute(&admin_pool)
            .await
            .expect("isolated latency test schema should be created");

        let connection_schema = schema.clone();
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .after_connect(move |connection, _| {
                let statement = format!("SET search_path TO {connection_schema}");
                Box::pin(async move {
                    sqlx::query(sqlx::AssertSqlSafe(statement))
                        .execute(connection)
                        .await?;
                    Ok(())
                })
            })
            .connect(&uri)
            .await
            .expect("schema-scoped PostgreSQL pool should connect");
        sqlx::query(
            r#"
            CREATE TABLE event (
                event_id UUID PRIMARY KEY,
                env_id UUID NOT NULL,
                created_at TIMESTAMPTZ NOT NULL
            )
            "#,
        )
        .execute(&pool)
        .await
        .expect("event fixture table should be created");
        sqlx::query(
            r#"
            CREATE TABLE run (
                event_id UUID NOT NULL,
                env_id UUID NOT NULL,
                build_id UUID,
                run_type_id SMALLINT NOT NULL,
                start_time TIMESTAMPTZ,
                stop_time TIMESTAMPTZ
            )
            "#,
        )
        .execute(&pool)
        .await
        .expect("run fixture table should be created");

        let env_id = uuid::Uuid::now_v7();
        let now = chrono::Utc::now();
        for (created_at, start_time) in [
            (
                now - chrono::Duration::milliseconds(30),
                now - chrono::Duration::milliseconds(20),
            ),
            (now, now - chrono::Duration::milliseconds(5)),
        ] {
            let event_id = uuid::Uuid::now_v7();
            sqlx::query("INSERT INTO event (event_id, env_id, created_at) VALUES ($1, $2, $3)")
                .bind(event_id)
                .bind(env_id)
                .bind(created_at)
                .execute(&pool)
                .await
                .expect("event latency fixture should insert");
            sqlx::query(
                "INSERT INTO run (event_id, env_id, run_type_id, start_time, stop_time) VALUES ($1, $2, $3, $4, $5)",
            )
            .bind(event_id)
            .bind(env_id)
            .bind(1_i16)
            .bind(start_time)
            .bind(now)
            .execute(&pool)
            .await
            .expect("run latency fixture should insert");
        }

        let result = query_postgres_latency(
            &pool,
            env_id,
            &LatencyFilters {
                days: None,
                time_unit: "day".to_string(),
                project_id: None,
            },
            "UTC",
            LatencyKind::Run,
        )
        .await
        .expect("PostgreSQL latency query should succeed");

        assert_eq!(result.negative_sample_count, 1);
        let summary = result
            .buckets
            .iter()
            .find(|bucket| bucket.date.is_none())
            .expect("summary bucket should exist");
        assert_eq!(summary.sample_count, 1);
        assert!((summary.p50.waiting_ms.expect("p50 waiting") - 10.0).abs() < 0.01);

        pool.close().await;
        sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
            .execute(&admin_pool)
            .await
            .expect("isolated latency test schema should be removed");
        admin_pool.close().await;
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
        let data = assemble_latency_timeline(aggregate_latency_samples(samples, false), None);
        assert_eq!(data.summary.sample_count, 2);
        assert_eq!(data.summary.precise_sample_count, 1);
        assert_eq!(data.summary.precise_coverage_percent, 50.0);
        assert_eq!(data.summary.p50.waiting_ms, Some(20.0));
    }

    #[test]
    fn capped_latency_does_not_synthesize_pre_cap_zero_buckets() {
        let samples = vec![LatencySample {
            date: "2026-07-29".to_string(),
            waiting_ms: 10.0,
            execution_ms: 20.0,
            precise: true,
        }];
        let data = assemble_latency_timeline(
            aggregate_latency_samples(samples, true),
            Some(vec!["2026-07-28".to_string(), "2026-07-29".to_string()]),
        );
        assert_eq!(data.dates, vec!["2026-07-29"]);
        assert_eq!(data.sample_counts, vec![1]);
        assert!(data.last_bucket_partial);
    }

    #[test]
    fn all_time_latency_does_not_mark_a_historical_last_bucket_partial() {
        let samples = vec![LatencySample {
            date: "2026-06".to_string(),
            waiting_ms: 10.0,
            execution_ms: 20.0,
            precise: true,
        }];
        let data = assemble_latency_timeline(aggregate_latency_samples(samples, false), None);
        assert!(!data.last_bucket_partial);
    }

    #[test]
    fn capped_historical_latency_does_not_mark_its_last_bucket_partial() {
        let samples = vec![LatencySample {
            date: "2026-06-01".to_string(),
            waiting_ms: 10.0,
            execution_ms: 20.0,
            precise: true,
        }];
        let data = assemble_latency_timeline(
            aggregate_latency_samples(samples, true),
            Some(vec!["2026-07-28".to_string(), "2026-07-29".to_string()]),
        );
        assert!(!data.last_bucket_partial);
    }
}

use crate::auth::Session;
use ahash::AHashMap;
use axum::extract::{Query, State};
use axum::response::{IntoResponse, Json};
use hot::db::{DatabasePool, Run};
use hot::time_range::parse_time_range_cutoff;
use std::sync::Arc;

/// GET /data/run-type-data - Get run type chart data with cross-filters
pub async fn run_type_data_handler(
    State(db): State<Arc<DatabasePool>>,
    Query(params): Query<AHashMap<String, String>>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> impl IntoResponse {
    let env_id = match session.current_env_id() {
        Some(id) => id,
        None => {
            return Json(serde_json::json!({"error": "No environment selected"})).into_response();
        }
    };

    let time_range = params
        .get("time_range")
        .map(String::as_str)
        .unwrap_or("P7D");
    let time_unit = params.get("time_unit").map(String::as_str).unwrap_or("day");
    let selected_run_types: Vec<&str> = params
        .get("run_types")
        .map(|types| types.split(',').collect())
        .unwrap_or_else(|| vec!["call", "event", "schedule", "run", "eval", "repl"]);
    let selected_statuses: Vec<&str> = params
        .get("statuses")
        .map(|statuses| statuses.split(',').collect())
        .unwrap_or_else(|| vec!["running", "succeeded", "failed", "cancelled"]);

    let time_range_cutoff = parse_time_range_cutoff(Some(time_range), chrono::Utc::now());

    let chart_data_json = match Run::get_run_type_chart_data_with_cross_filters(
        &db,
        &env_id,
        time_range_cutoff,
        time_unit,
        &selected_run_types,
        &selected_statuses,
    )
    .await
    {
        Ok(daily_counts) => {
            // Extract all unique dates from the data
            let all_dates: std::collections::BTreeSet<String> =
                daily_counts.keys().cloned().collect();
            let dates: Vec<String> = all_dates.into_iter().collect();

            let mut chart_data = serde_json::json!({ "dates": dates, "series": [] });
            let dates_array = chart_data["dates"].as_array().unwrap().clone();
            let series = chart_data["series"].as_array_mut().unwrap();

            for run_type in &selected_run_types {
                let mut data = Vec::new();
                for date in &dates_array {
                    let date = date.as_str().unwrap();
                    let count = daily_counts
                        .get(date)
                        .and_then(|day_data| day_data.get(*run_type))
                        .unwrap_or(&0);
                    data.push(count);
                }
                series.push(serde_json::json!({
                    "name": run_type,
                    "type": "bar",
                    "stack": "total",
                    "data": data
                }));
            }

            chart_data
        }
        Err(_) => serde_json::json!({"error": "Failed to fetch chart data"}),
    };

    Json(chart_data_json).into_response()
}

/// GET /data/status-chart-data - Get run status chart data with cross-filters
pub async fn status_chart_data_handler(
    State(db): State<Arc<DatabasePool>>,
    Query(params): Query<AHashMap<String, String>>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> impl IntoResponse {
    let env_id = match session.current_env_id() {
        Some(id) => id,
        None => {
            return Json(serde_json::json!({"error": "No environment selected"})).into_response();
        }
    };

    let time_range = params
        .get("time_range")
        .map(String::as_str)
        .unwrap_or("P7D");
    let time_unit = params.get("time_unit").map(String::as_str).unwrap_or("day");
    let selected_statuses: Vec<&str> = params
        .get("statuses")
        .map(|statuses| statuses.split(',').collect())
        .unwrap_or_else(|| vec!["running", "succeeded", "failed", "cancelled"]);
    let selected_run_types: Vec<&str> = params
        .get("run_types")
        .map(|types| types.split(',').collect())
        .unwrap_or_else(|| vec!["call", "event", "schedule", "run", "eval", "repl"]);

    let time_range_cutoff = parse_time_range_cutoff(Some(time_range), chrono::Utc::now());

    let chart_data_json = match Run::get_run_status_chart_data_with_cross_filters(
        &db,
        &env_id,
        time_range_cutoff,
        time_unit,
        &selected_statuses,
        &selected_run_types,
    )
    .await
    {
        Ok(daily_counts) => {
            // Extract all unique dates from the data
            let all_dates: std::collections::BTreeSet<String> =
                daily_counts.keys().cloned().collect();
            let dates: Vec<String> = all_dates.into_iter().collect();

            let mut chart_data = serde_json::json!({ "dates": dates, "series": [] });
            let dates_array = chart_data["dates"].as_array().unwrap().clone();
            let series = chart_data["series"].as_array_mut().unwrap();

            for status in &selected_statuses {
                let mut data = Vec::new();
                for date in &dates_array {
                    let date = date.as_str().unwrap();
                    let count = daily_counts
                        .get(date)
                        .and_then(|day_data| day_data.get(*status))
                        .unwrap_or(&0);
                    data.push(count);
                }
                series.push(serde_json::json!({
                    "name": status,
                    "type": "line",
                    "data": data
                }));
            }

            chart_data
        }
        Err(_) => serde_json::json!({"error": "Failed to fetch status chart data"}),
    };

    Json(chart_data_json).into_response()
}

/// GET /data/filtered-type-summary - Get run type summary counts with filters
pub async fn filtered_type_summary_handler(
    State(db): State<Arc<DatabasePool>>,
    Query(params): Query<AHashMap<String, String>>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> impl IntoResponse {
    let env_id = match session.current_env_id() {
        Some(id) => id,
        None => {
            return Json(serde_json::json!({"error": "No environment selected"})).into_response();
        }
    };

    let time_range = params
        .get("time_range")
        .map(String::as_str)
        .unwrap_or("P7D");
    let run_types = params
        .get("run_types")
        .map(String::as_str)
        .unwrap_or("call,event,schedule,run,eval,repl");
    let statuses = params
        .get("statuses")
        .map(String::as_str)
        .unwrap_or("running,succeeded,failed,cancelled");

    let type_list: Vec<&str> = run_types.split(',').collect();
    let status_list: Vec<&str> = statuses.split(',').collect();

    let start_time = if time_range == "all" {
        chrono::Utc::now() - chrono::Duration::days(36500)
    } else {
        let days = match time_range {
            "PT24H" | "P1D" => 1,
            "P7D" => 7,
            "P15D" => 15,
            "P30D" => 30,
            "P60D" => 60,
            "P90D" => 90,
            "P1M" => 30,
            "P3M" => 90,
            "P6M" => 180,
            "P1Y" => 365,
            _ => 7,
        };
        chrono::Utc::now() - chrono::Duration::days(days as i64)
    };

    let mut summary_data = serde_json::Map::new();
    let mut total_count = 0i64;

    for type_str in &type_list {
        let run_type = match type_str.trim() {
            "call" => hot::db::run::RunType::Call,
            "event" => hot::db::run::RunType::Event,
            "schedule" => hot::db::run::RunType::Schedule,
            "run" => hot::db::run::RunType::Run,
            "eval" => hot::db::run::RunType::Eval,
            "repl" => hot::db::run::RunType::Repl,
            _ => continue,
        };

        let count = Run::get_count_by_type_time_env_and_statuses(
            &db,
            &run_type,
            &env_id,
            start_time,
            &status_list,
        )
        .await
        .unwrap_or(0);

        summary_data.insert(
            type_str.trim().to_string(),
            serde_json::Value::Number(serde_json::Number::from(count)),
        );
        total_count += count;
    }

    summary_data.insert(
        "total".to_string(),
        serde_json::Value::Number(serde_json::Number::from(total_count)),
    );

    Json(serde_json::Value::Object(summary_data)).into_response()
}

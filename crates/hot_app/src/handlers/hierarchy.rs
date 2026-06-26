use crate::auth::Session;
use axum::extract::{Path, State};
use axum::response::{IntoResponse, Json};
use chrono::Utc;
use hot::db::{DatabasePool, Features, Run, build_hierarchy};
use std::sync::Arc;
use uuid::Uuid;

/// Call data availability status
///
/// Tells the UI why call data may or may not be present for a run:
/// - `available`    — call data exists and is returned normally
/// - `collecting`   — run is still in progress, calls may still arrive
/// - `expired`      — run is older than the org's call retention window
/// - `not_included` — org's plan does not include call data (retention = 0)
/// - `empty`        — run completed within retention but recorded no calls
const CALL_STATUS_AVAILABLE: &str = "available";
const CALL_STATUS_COLLECTING: &str = "collecting";
const CALL_STATUS_EXPIRED: &str = "expired";
const CALL_STATUS_NOT_INCLUDED: &str = "not_included";
const CALL_STATUS_EMPTY: &str = "empty";

/// GET /data/runs/{run_id}/hierarchy
/// Returns the call hierarchy tree for a run
pub async fn get_hierarchy_handler(
    Path(run_id): Path<Uuid>,
    State(db): State<Arc<DatabasePool>>,
    axum::extract::Extension(session): axum::extract::Extension<Session>,
) -> impl IntoResponse {
    // Get list of environment IDs the user has access to
    let user_env_ids: Vec<Uuid> = session
        .current_org_envs
        .iter()
        .map(|env| env.env_id)
        .collect();

    // Check if user has access to this run
    if !user_env_ids.is_empty() {
        let has_access = Run::is_run_in_envs(&db, &run_id, &user_env_ids)
            .await
            .unwrap_or(false);

        if !has_access {
            tracing::warn!(
                "User {} denied access to run hierarchy: {}",
                session.current_user_id(),
                run_id
            );
            return Json(serde_json::json!({
                "success": false,
                "error": "Access denied"
            }))
            .into_response();
        }
    } else {
        tracing::warn!(
            "User {} has no environment access",
            session.current_user_id()
        );
        return Json(serde_json::json!({
            "success": false,
            "error": "No environment access"
        }))
        .into_response();
    }

    let run = match Run::get_run(&db, &run_id).await {
        Ok(run) => run,
        Err(e) => {
            tracing::error!("Failed to get run {} for hierarchy: {}", run_id, e);
            return Json(serde_json::json!({
                "success": false,
                "error": "Run not found"
            }))
            .into_response();
        }
    };

    // Resolve the org's call retention policy
    let features = if let Some(ref org) = session.current_org {
        Features::resolve_for_org(&db, &org.org_id).await
    } else {
        Features::unlimited()
    };
    let call_retention_days = features.call_retention_days();

    // Early exit: plan does not include call data at all
    if call_retention_days == 0 {
        return Json(serde_json::json!({
            "success": true,
            "data": {
                "run_id": run_id,
                "total_duration_us": 0,
                "total_calls": 0,
                "total_vars": 0,
                "tree": []
            },
            "build_id": run.build_id,
            "call_data_status": CALL_STATUS_NOT_INCLUDED,
            "call_retention_days": call_retention_days
        }))
        .into_response();
    }

    // Check if the run is outside the retention window (skip DB query for calls)
    if call_retention_days > 0 {
        let retention_cutoff = Utc::now() - chrono::Duration::days(call_retention_days as i64);
        if run.start_time < retention_cutoff {
            return Json(serde_json::json!({
                "success": true,
                "data": {
                    "run_id": run_id,
                    "total_duration_us": 0,
                    "total_calls": 0,
                    "total_vars": 0,
                    "tree": []
                },
                "build_id": run.build_id,
                "call_data_status": CALL_STATUS_EXPIRED,
                "call_retention_days": call_retention_days
            }))
            .into_response();
        }
    }

    // Build hierarchy (call_retention_days is -1 unlimited, or run is within window)
    tracing::debug!("Building hierarchy for run: {}", run_id);
    match build_hierarchy(&db, &run_id).await {
        Ok(hierarchy) => {
            // Determine fine-grained status based on call count and run state
            let call_data_status = if hierarchy.total_calls > 0 {
                CALL_STATUS_AVAILABLE
            } else {
                // No calls — check if the run is still in progress
                if run.status == "running" {
                    CALL_STATUS_COLLECTING
                } else {
                    CALL_STATUS_EMPTY
                }
            };

            tracing::debug!(
                "Built hierarchy for run {}: {} calls (status: {})",
                run_id,
                hierarchy.total_calls,
                call_data_status
            );
            Json(serde_json::json!({
                "success": true,
                "data": hierarchy,
                "build_id": run.build_id,
                "call_data_status": call_data_status,
                "call_retention_days": call_retention_days
            }))
            .into_response()
        }
        Err(e) => {
            tracing::error!("Failed to build hierarchy for run {}: {}", run_id, e);
            Json(serde_json::json!({
                "success": false,
                "error": format!("Failed to build hierarchy: {}", e)
            }))
            .into_response()
        }
    }
}

//! Organization handlers

use axum::{Extension, Json, extract::State, http::StatusCode};
use chrono::{Datelike, TimeZone};
use hot::db::Features;
use hot::db::api_key::ApiKey;
use hot::db::env::Env;
use hot::db::subscription::{OrgPlan, OrgUsageStats};
use serde::Serialize;
use utoipa::ToSchema;

use crate::ApiStateData;
use crate::models::*;

/// Response for organization usage and limits
#[derive(Debug, Serialize, ToSchema)]
pub struct OrgUsageResponse {
    /// Organization ID
    pub org_id: uuid::Uuid,
    /// Current usage statistics
    pub usage: UsageStats,
    /// Current plan limits
    pub limits: Limits,
    /// Usage percentages (0-100+, can exceed 100 if over limit)
    pub usage_percent: UsagePercent,
    /// Plan information
    pub plan: PlanInfo,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct UsageStats {
    /// Runs executed this billing period
    pub runs_this_period: i64,
    /// Total file storage in bytes
    pub file_storage_bytes: i64,
    /// Number of active team members
    pub team_members: i32,
    /// Call data storage in bytes (within retention period)
    pub call_storage_bytes: i64,
    /// Total number of stored calls
    pub call_count: i64,
    /// Store data storage in bytes (::hot::store)
    pub store_storage_bytes: i64,
    /// Active schedules attached to deployed builds
    pub active_schedules: i64,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct Limits {
    /// Maximum runs per month (-1 for unlimited)
    pub runs_per_month: i32,
    /// Maximum file storage in bytes (-1 for unlimited)
    pub storage_bytes: i64,
    /// Maximum team members (-1 for unlimited)
    pub team_members: i32,
    /// Call data retention in days (-1 for unlimited)
    pub call_retention_days: i32,
    /// Maximum call storage in bytes (-1 for unlimited)
    pub call_storage_bytes: i64,
    /// Maximum store storage in bytes (-1 for unlimited)
    pub store_storage_bytes: i64,
    /// Maximum compute units per month (-1 for unlimited)
    pub compute_units_per_month: i64,
    /// Compute units used this billing period
    pub compute_units_used: i64,
    /// Org-level CUS spending cap (-1 for no cap)
    pub compute_units_budget: i64,
    /// Maximum task minutes per month (-1 for unlimited)
    pub task_minutes_per_month: i32,
    /// Maximum active schedules per org (-1 for unlimited)
    pub active_schedules_per_org: i64,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct UsagePercent {
    /// Runs usage percentage (0-100+)
    pub runs: f64,
    /// File storage usage percentage (0-100+)
    pub file_storage: f64,
    /// Team members usage percentage (0-100+)
    pub team_members: f64,
    /// Call storage usage percentage (0-100+)
    pub call_storage: f64,
    /// Store storage usage percentage (0-100+)
    pub store_storage: f64,
    /// Active schedules usage percentage (0-100+)
    pub active_schedules: f64,
    /// True if any usage is above 90%
    pub has_warning: bool,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct PlanInfo {
    /// Plan name (e.g., "Free", "Pro", "Team")
    pub name: String,
    /// Billing period start
    pub period_start: chrono::DateTime<chrono::Utc>,
    /// Billing period end
    pub period_end: chrono::DateTime<chrono::Utc>,
}

/// Get organization usage and limits
///
/// Returns current usage statistics, plan limits, and usage percentages
/// for the organization associated with this API key.
pub async fn get_org_usage(
    State((db, _storage, _conf, _stream_pubsub)): State<ApiStateData>,
    Extension(api_key): Extension<ApiKey>,
) -> Result<Json<ApiResponse<OrgUsageResponse>>, (StatusCode, Json<ApiErrorResponse>)> {
    // Get the environment to find org_id
    let env = Env::get_env(&db, &api_key.env_id).await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ApiErrorResponse::internal_error(&e.to_string())),
        )
    })?;

    let org_id = env.org_id;

    // Get the organization's subscription (may not exist for free/local deployments)
    let subscription_result = OrgPlan::get_by_org_id(&db, &org_id).await;

    // Get plan info and features based on subscription status
    let (plan_name, features, period_start, period_end) = match subscription_result {
        Ok(subscription) => {
            // Has subscription - get plan and features
            let plan = subscription.get_plan(&db).await.map_err(|e| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(ApiErrorResponse::internal_error(&format!(
                        "Failed to get plan: {}",
                        e
                    ))),
                )
            })?;

            let now = chrono::Utc::now();
            let period_start = subscription.current_period_start.unwrap_or_else(|| {
                chrono::Utc
                    .with_ymd_and_hms(now.year(), now.month(), 1, 0, 0, 0)
                    .single()
                    .unwrap_or(now)
            });
            let period_end = subscription.current_period_end.unwrap_or_else(|| {
                let (year, month) = if now.month() == 12 {
                    (now.year() + 1, 1)
                } else {
                    (now.year(), now.month() + 1)
                };
                chrono::Utc
                    .with_ymd_and_hms(year, month, 1, 0, 0, 0)
                    .single()
                    .unwrap_or(now)
            });

            // Resolve features (plan defaults + org overrides)
            let features = Features::resolve_for_org(&db, &org_id).await;

            (plan.plan_name, features, period_start, period_end)
        }
        Err(_) => {
            // No subscription - unlimited features (self-hosted/local)
            let now = chrono::Utc::now();
            let period_start = chrono::Utc
                .with_ymd_and_hms(now.year(), now.month(), 1, 0, 0, 0)
                .single()
                .unwrap_or(now);
            let (year, month) = if now.month() == 12 {
                (now.year() + 1, 1)
            } else {
                (now.year(), now.month() + 1)
            };
            let period_end = chrono::Utc
                .with_ymd_and_hms(year, month, 1, 0, 0, 0)
                .single()
                .unwrap_or(now);

            (
                "Self-Hosted".to_string(),
                Features::unlimited(),
                period_start,
                period_end,
            )
        }
    };

    // Calculate current usage stats
    let usage_stats =
        OrgUsageStats::calculate(&db, &org_id, period_start, features.call_retention_days())
            .await
            .map_err(|e| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(ApiErrorResponse::internal_error(&format!(
                        "Failed to calculate usage: {}",
                        e
                    ))),
                )
            })?;

    // Build the response
    let response = OrgUsageResponse {
        org_id,
        usage: UsageStats {
            runs_this_period: usage_stats.runs_this_period,
            file_storage_bytes: usage_stats.file_storage_bytes,
            team_members: usage_stats.team_members_count,
            call_storage_bytes: usage_stats.call_storage_bytes,
            call_count: usage_stats.call_count,
            store_storage_bytes: usage_stats.store_storage_bytes,
            active_schedules: usage_stats.active_schedules,
        },
        limits: Limits {
            runs_per_month: features.runs_per_month(),
            storage_bytes: features.storage_bytes(),
            team_members: features.team_members(),
            call_retention_days: features.call_retention_days(),
            call_storage_bytes: features.call_storage_bytes(),
            store_storage_bytes: features.store_storage_bytes(),
            compute_units_per_month: features.compute_units_per_month(),
            compute_units_used: usage_stats.compute_units,
            compute_units_budget: features.compute_units_budget(),
            task_minutes_per_month: features.task_minutes_per_month(),
            active_schedules_per_org: features.active_schedules_per_org(),
        },
        usage_percent: UsagePercent {
            runs: usage_stats.runs_pct(&features),
            file_storage: usage_stats.file_storage_pct(&features),
            team_members: usage_stats.team_members_pct(&features),
            call_storage: usage_stats.call_storage_pct(&features),
            store_storage: usage_stats.store_storage_pct(&features),
            active_schedules: usage_stats.active_schedules_pct(&features),
            has_warning: usage_stats.has_warning(&features),
        },
        plan: PlanInfo {
            name: plan_name,
            period_start,
            period_end,
        },
    };

    Ok(Json(ApiResponse::new(response)))
}

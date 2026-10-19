//! Organization handlers

use axum::{Extension, Json, extract::State, http::StatusCode};
use chrono::{Datelike, TimeZone};
use hot::db::api_key::ApiKey;
use hot::db::env::Env;
use hot::db::subscription::{OrgPlan, OrgUsageStats, PlanError};
use hot::db::{DatabasePool, DatabaseScope, Features, MaintenanceOperation, Org};
use serde::Serialize;
use utoipa::ToSchema;

use crate::ApiStateData;
use crate::auth::AuthContext;
use crate::models::*;

/// Response for organization usage and limits
#[derive(Debug, Serialize, ToSchema)]
pub struct OrgUsageResponse {
    /// Organization ID
    pub org_id: uuid::Uuid,
    /// The credential's environment. Consumption never includes sibling environments.
    pub env_id: uuid::Uuid,
    /// Always "environment"; usage_percent compares this environment to shared limits.
    pub usage_scope: String,
    /// Always "billing_account"; limits are inherited from the owning root.
    pub limits_scope: String,
    /// Current usage statistics for this environment
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
    /// Runs admitted for this environment during this billing period
    pub runs_this_period: i64,
    /// Total file storage in bytes
    pub file_storage_bytes: i64,
    /// Always zero: directory seats do not belong to an environment
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
    /// Compute units used by this environment during this billing period
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
/// Returns this credential's environment consumption and its root account's
/// limits. Percentages do not represent total account usage or remaining quota;
/// management credentials provide the separate account aggregate.
pub async fn get_org_usage(
    State((db, _storage, conf, _stream_pubsub)): State<ApiStateData>,
    Extension(auth): Extension<AuthContext>,
    Extension(api_key): Extension<ApiKey>,
) -> Result<Json<ApiResponse<OrgUsageResponse>>, (StatusCode, Json<ApiErrorResponse>)> {
    super::require_api_key(&auth, "Only API keys can read organization usage.")?;

    // Get the environment to find org_id
    let env = Env::get_env(&db, &api_key.env_id).await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ApiErrorResponse::internal_error(&e.to_string())),
        )
    })?;

    let org_id = env.org_id;

    let db = db.with_scope(DatabaseScope::Environment {
        org_id,
        env_id: env.env_id,
    });
    let (plan_name, features, period_start, period_end) =
        account_plan_details(&db, &org_id, hot::product::billing_enabled(&conf))
            .await
            .map_err(|error| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(ApiErrorResponse::internal_error(&format!(
                        "Cannot resolve account limits: {error}"
                    ))),
                )
            })?;

    // Calculate current usage stats
    let usage_stats = OrgUsageStats::calculate_environment(
        &db,
        &org_id,
        &env.env_id,
        period_start,
        features.call_retention_days(),
    )
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
        env_id: env.env_id,
        usage_scope: "environment".into(),
        limits_scope: "billing_account".into(),
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

/// Only entitlement metadata crosses the environment boundary. No account
/// consumption or directory rows are returned by this trusted helper.
pub(super) async fn account_plan_details(
    db: &DatabasePool,
    org_id: &uuid::Uuid,
    hosted: bool,
) -> Result<
    (
        String,
        Features,
        chrono::DateTime<chrono::Utc>,
        chrono::DateTime<chrono::Utc>,
    ),
    PlanError,
> {
    let root = Org::billing_owner_id_for(db, org_id)
        .await
        .map_err(|error| PlanError::InvalidOrganization(error.to_string()))?;
    let billing = db.with_scope(DatabaseScope::Maintenance {
        operation: MaintenanceOperation::Billing,
        org_id: Some(root),
    });
    let overrides = Features::account_overrides(&billing, &root).await?;
    let subscription = match OrgPlan::get_by_org_id(&billing, &root).await {
        Ok(plan) => Some(plan),
        Err(PlanError::NotFound) => None,
        Err(error) => return Err(error),
    };
    let plan = match &subscription {
        Some(subscription) => Some(subscription.get_plan(&billing).await?),
        None => None,
    };
    let plan_features = plan.as_ref().map(|plan| {
        plan.features
            .clone()
            .unwrap_or_else(|| serde_json::json!({}))
    });
    for features in [plan_features.as_ref(), overrides.as_ref()]
        .into_iter()
        .flatten()
    {
        if !features.is_object() {
            return Err(PlanError::InvalidPlan);
        }
        for key in [
            "runs_per_month",
            "storage_bytes",
            "team_members",
            "call_retention_days",
            "call_storage_bytes",
            "store_storage_bytes",
            "compute_units_per_month",
            "compute_units_budget",
            "task_minutes_per_month",
            "active_schedules_per_org",
        ] {
            if features
                .get(key)
                .is_some_and(|value| value.as_i64().is_none())
            {
                return Err(PlanError::InvalidPlan);
            }
        }
    }
    let features = if !hosted && plan_features.is_none() && overrides.is_none() {
        Features::unlimited()
    } else {
        Features::resolve(plan_features.as_ref(), overrides.as_ref())
    };
    let now = chrono::Utc::now();
    let default_start = chrono::Utc
        .with_ymd_and_hms(now.year(), now.month(), 1, 0, 0, 0)
        .single()
        .unwrap_or(now);
    let (year, month) = if now.month() == 12 {
        (now.year() + 1, 1)
    } else {
        (now.year(), now.month() + 1)
    };
    let default_end = chrono::Utc
        .with_ymd_and_hms(year, month, 1, 0, 0, 0)
        .single()
        .unwrap_or(now);
    let period_start = subscription
        .as_ref()
        .and_then(|s| s.current_period_start)
        .unwrap_or(default_start);
    let period_end = subscription
        .as_ref()
        .and_then(|s| s.current_period_end)
        .unwrap_or(default_end);
    let name = plan
        .map(|p| p.plan_name)
        .unwrap_or_else(|| if hosted { "No Plan" } else { "Self-Hosted" }.into());
    Ok((name, features, period_start, period_end))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn account_limits_inherit_root_and_lookup_errors_never_report_unlimited() {
        let db = hot::db::test_db().await;
        let DatabasePool::Sqlite(pool) = &db else {
            unreachable!()
        };
        let root = uuid::Uuid::now_v7();
        let child = uuid::Uuid::now_v7();
        sqlx::query(r#"INSERT INTO org(org_id,name,slug,org_type,features,created_by_user_id) VALUES (?,'Root',?,'organization','{"runs_per_month":12}',?)"#)
            .bind(root).bind(root.to_string()).bind(root).execute(pool).await.unwrap();
        sqlx::query(r#"INSERT INTO org(org_id,parent_org_id,name,slug,org_type,features,created_by_user_id) VALUES (?,?,'Child',?,'organization','{"runs_per_month":-1}',?)"#)
            .bind(child).bind(root).bind(child.to_string()).bind(root).execute(pool).await.unwrap();
        assert_eq!(
            account_plan_details(&db, &child, false)
                .await
                .unwrap()
                .1
                .runs_per_month(),
            12
        );
        assert!(
            account_plan_details(&db, &uuid::Uuid::now_v7(), false)
                .await
                .is_err()
        );
        sqlx::query("UPDATE org SET features='broken-json' WHERE org_id=?")
            .bind(root)
            .execute(pool)
            .await
            .unwrap();
        assert!(account_plan_details(&db, &child, false).await.is_err());
        sqlx::query("UPDATE org SET features=NULL WHERE org_id=?")
            .bind(root)
            .execute(pool)
            .await
            .unwrap();
        assert_eq!(
            account_plan_details(&db, &child, false)
                .await
                .unwrap()
                .1
                .runs_per_month(),
            -1
        );
        assert_eq!(
            account_plan_details(&db, &child, true)
                .await
                .unwrap()
                .1
                .runs_per_month(),
            0
        );
    }

    #[tokio::test]
    async fn account_plan_details_use_subscription_period_for_management_and_runtime() {
        let db = hot::db::test_db().await;
        let DatabasePool::Sqlite(pool) = &db else {
            unreachable!()
        };
        let root = uuid::Uuid::now_v7();
        let plan = uuid::Uuid::now_v7();
        sqlx::query("INSERT INTO org(org_id,name,slug,org_type,created_by_user_id) VALUES (?,'Root',?,'organization',?)")
            .bind(root).bind(root.to_string()).bind(root).execute(pool).await.unwrap();
        sqlx::query("INSERT INTO plan(plan_uuid,plan_name,base_price_monthly_cents,base_price_annual_cents,sort_order,features) VALUES (?,'Subscription Period Test',0,0,1,'{}')")
            .bind(plan).execute(pool).await.unwrap();
        let start = chrono::Utc::now() - chrono::Duration::days(15);
        let end = start + chrono::Duration::days(30);
        sqlx::query("INSERT INTO org_plan(org_plan_id,org_id,plan_uuid,status_id,billing_period,current_period_start,current_period_end,created_by_user_id) VALUES (?,?,?,1,'monthly',?,?,?)")
            .bind(uuid::Uuid::now_v7()).bind(root).bind(plan).bind(start).bind(end).bind(root).execute(pool).await.unwrap();
        let (_, _, period_start, period_end) =
            account_plan_details(&db, &root, true).await.unwrap();
        assert_eq!(period_start, start);
        assert_eq!(period_end, end);
    }
}

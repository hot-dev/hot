use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use sqlx::FromRow;
use sqlx::types::Uuid;
use thiserror::Error;

use super::DatabasePool;

#[derive(Error, Debug)]
pub enum PlanError {
    #[error("Database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("Subscription not found")]
    NotFound,
    #[error("Invalid plan")]
    InvalidPlan,
}

/// Subscription Plan
#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct Plan {
    pub plan_uuid: Uuid,
    pub plan_id: Option<String>,
    pub plan_name: String,
    pub base_price_monthly_cents: i32,
    pub base_price_annual_cents: i32,
    pub sort_order: i32,
    pub active: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// JSON features for this plan (limits + flags).
    /// Contains runs_per_month, storage_bytes, team_members, call_retention_days,
    /// call_storage_bytes, custom_domains, self_hosted, etc.
    #[sqlx(json)]
    pub features: Option<JsonValue>,
}

impl Plan {
    /// Get all active subscription plans ordered by sort_order
    pub async fn get_all_active(db: &DatabasePool) -> Result<Vec<Self>, PlanError> {
        match db {
            DatabasePool::Postgres(pool) => {
                let plans = sqlx::query_as::<_, Self>(
                    "SELECT * FROM plan WHERE active = true ORDER BY sort_order",
                )
                .fetch_all(pool)
                .await?;
                Ok(plans)
            }
            DatabasePool::Sqlite(pool) => {
                let plans = sqlx::query_as::<_, Self>(
                    "SELECT * FROM plan WHERE active = true ORDER BY sort_order",
                )
                .fetch_all(pool)
                .await?;
                Ok(plans)
            }
        }
    }

    /// Get a subscription plan by UUID
    pub async fn get_by_id(db: &DatabasePool, plan_uuid: &Uuid) -> Result<Self, PlanError> {
        match db {
            DatabasePool::Postgres(pool) => {
                let plan = sqlx::query_as::<_, Self>("SELECT * FROM plan WHERE plan_uuid = $1")
                    .bind(plan_uuid)
                    .fetch_optional(pool)
                    .await?
                    .ok_or(PlanError::NotFound)?;
                Ok(plan)
            }
            DatabasePool::Sqlite(pool) => {
                let plan = sqlx::query_as::<_, Self>("SELECT * FROM plan WHERE plan_uuid = ?")
                    .bind(plan_uuid)
                    .fetch_optional(pool)
                    .await?
                    .ok_or(PlanError::NotFound)?;
                Ok(plan)
            }
        }
    }

    /// Get a subscription plan by plan name (display name like "Hot Cloud Starter")
    pub async fn get_by_name(db: &DatabasePool, plan_name: &str) -> Result<Self, PlanError> {
        match db {
            DatabasePool::Postgres(pool) => {
                let plan = sqlx::query_as::<_, Self>("SELECT * FROM plan WHERE plan_name = $1")
                    .bind(plan_name)
                    .fetch_optional(pool)
                    .await?
                    .ok_or(PlanError::NotFound)?;
                Ok(plan)
            }
            DatabasePool::Sqlite(pool) => {
                let plan = sqlx::query_as::<_, Self>("SELECT * FROM plan WHERE plan_name = ?")
                    .bind(plan_name)
                    .fetch_optional(pool)
                    .await?
                    .ok_or(PlanError::NotFound)?;
                Ok(plan)
            }
        }
    }

    /// Get a subscription plan by plan_id (URL-friendly identifier like "hot-cloud-starter")
    pub async fn get_by_plan_id(db: &DatabasePool, plan_id: &str) -> Result<Self, PlanError> {
        match db {
            DatabasePool::Postgres(pool) => {
                let plan = sqlx::query_as::<_, Self>("SELECT * FROM plan WHERE plan_id = $1")
                    .bind(plan_id)
                    .fetch_optional(pool)
                    .await?
                    .ok_or(PlanError::NotFound)?;
                Ok(plan)
            }
            DatabasePool::Sqlite(pool) => {
                let plan = sqlx::query_as::<_, Self>("SELECT * FROM plan WHERE plan_id = ?")
                    .bind(plan_id)
                    .fetch_optional(pool)
                    .await?
                    .ok_or(PlanError::NotFound)?;
                Ok(plan)
            }
        }
    }

    /// Get resolved features for this plan.
    pub fn get_features(&self) -> super::features::Features {
        super::features::Features::from_json(self.features.as_ref())
    }

    /// Get monthly price in dollars (for display)
    pub fn monthly_price_dollars(&self) -> f64 {
        self.base_price_monthly_cents as f64 / 100.0
    }

    /// Get annual price in dollars (for display)
    pub fn annual_price_dollars(&self) -> f64 {
        self.base_price_annual_cents as f64 / 100.0
    }

    pub fn is_free_plan(&self) -> bool {
        self.plan_id.as_deref() == Some("hot-free")
    }

    /// Calculate annual savings percentage
    pub fn annual_savings_percent(&self) -> f64 {
        if self.base_price_monthly_cents == 0 {
            return 0.0;
        }
        let monthly_annual_cost = (self.base_price_monthly_cents * 12) as f64;
        let annual_cost = self.base_price_annual_cents as f64;
        ((monthly_annual_cost - annual_cost) / monthly_annual_cost) * 100.0
    }

    /// Get the next active plan by sort_order (the plan immediately above this one).
    pub async fn get_next_active_by_sort_order(
        db: &DatabasePool,
        current_sort_order: i32,
    ) -> Result<Self, PlanError> {
        match db {
            DatabasePool::Postgres(pool) => {
                sqlx::query_as::<_, Self>(
                    "SELECT * FROM plan WHERE sort_order > $1 AND active = true ORDER BY sort_order LIMIT 1",
                )
                .bind(current_sort_order)
                .fetch_optional(pool)
                .await?
                .ok_or(PlanError::NotFound)
            }
            DatabasePool::Sqlite(pool) => {
                sqlx::query_as::<_, Self>(
                    "SELECT * FROM plan WHERE sort_order > ? AND active = true ORDER BY sort_order LIMIT 1",
                )
                .bind(current_sort_order)
                .fetch_optional(pool)
                .await?
                .ok_or(PlanError::NotFound)
            }
        }
    }
}

/// Subscription Status
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrgPlanStatus {
    Active = 1,
    Inactive = 2,
    Pending = 3,
}

impl OrgPlanStatus {
    pub fn from_i16(status_id: i16) -> Option<Self> {
        match status_id {
            1 => Some(OrgPlanStatus::Active),
            2 => Some(OrgPlanStatus::Inactive),
            3 => Some(OrgPlanStatus::Pending),
            _ => None,
        }
    }

    pub fn to_i16(self) -> i16 {
        self as i16
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            OrgPlanStatus::Active => "active",
            OrgPlanStatus::Inactive => "inactive",
            OrgPlanStatus::Pending => "pending",
        }
    }
}

/// Billing Period
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BillingPeriod {
    Monthly,
    Annual,
}

impl BillingPeriod {
    pub fn as_str(&self) -> &'static str {
        match self {
            BillingPeriod::Monthly => "monthly",
            BillingPeriod::Annual => "annual",
        }
    }
}

impl std::str::FromStr for BillingPeriod {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "monthly" => Ok(BillingPeriod::Monthly),
            "annual" => Ok(BillingPeriod::Annual),
            _ => Err(format!("Invalid billing period: {}", s)),
        }
    }
}

/// Organization Subscription
#[derive(Debug, Clone, FromRow, Serialize)]
pub struct OrgPlan {
    pub org_plan_id: Uuid,
    pub org_id: Uuid,
    pub plan_uuid: Uuid,
    pub status_id: i16,
    pub billing_period: String,
    pub current_period_start: Option<DateTime<Utc>>,
    pub current_period_end: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub created_by_user_id: Uuid,
    pub updated_at: DateTime<Utc>,
    pub updated_by_user_id: Option<Uuid>,
}

impl OrgPlan {
    pub async fn get_by_id(db: &DatabasePool, org_plan_id: &Uuid) -> Result<Self, PlanError> {
        match db {
            DatabasePool::Postgres(pool) => {
                sqlx::query_as::<_, Self>("SELECT * FROM org_plan WHERE org_plan_id = $1")
                    .bind(org_plan_id)
                    .fetch_optional(pool)
                    .await?
                    .ok_or(PlanError::NotFound)
            }
            DatabasePool::Sqlite(pool) => {
                sqlx::query_as::<_, Self>("SELECT * FROM org_plan WHERE org_plan_id = ?")
                    .bind(org_plan_id)
                    .fetch_optional(pool)
                    .await?
                    .ok_or(PlanError::NotFound)
            }
        }
    }

    /// Get subscription by organization ID
    pub async fn get_by_org_id(db: &DatabasePool, org_id: &Uuid) -> Result<Self, PlanError> {
        match db {
            DatabasePool::Postgres(pool) => {
                let subscription =
                    sqlx::query_as::<_, Self>("SELECT * FROM org_plan WHERE org_id = $1")
                        .bind(org_id)
                        .fetch_optional(pool)
                        .await?
                        .ok_or(PlanError::NotFound)?;
                Ok(subscription)
            }
            DatabasePool::Sqlite(pool) => {
                let subscription =
                    sqlx::query_as::<_, Self>("SELECT * FROM org_plan WHERE org_id = ?")
                        .bind(org_id)
                        .fetch_optional(pool)
                        .await?
                        .ok_or(PlanError::NotFound)?;
                Ok(subscription)
            }
        }
    }

    /// Create a new subscription for an organization
    pub async fn create(
        db: &DatabasePool,
        org_id: &Uuid,
        plan_uuid: &Uuid,
        billing_period: BillingPeriod,
        created_by_user_id: &Uuid,
    ) -> Result<Self, PlanError> {
        let org_plan_id = Uuid::now_v7();
        let billing_period_str = billing_period.as_str();

        match db {
            DatabasePool::Postgres(pool) => {
                sqlx::query(
                    "INSERT INTO org_plan
                     (org_plan_id, org_id, plan_uuid, status_id, billing_period, created_by_user_id)
                     VALUES ($1, $2, $3, $4, $5, $6)",
                )
                .bind(org_plan_id)
                .bind(org_id)
                .bind(plan_uuid)
                .bind(OrgPlanStatus::Pending.to_i16())
                .bind(billing_period_str)
                .bind(created_by_user_id)
                .execute(pool)
                .await?;
            }
            DatabasePool::Sqlite(pool) => {
                sqlx::query(
                    "INSERT INTO org_plan
                     (org_plan_id, org_id, plan_uuid, status_id, billing_period, created_by_user_id)
                     VALUES (?, ?, ?, ?, ?, ?)",
                )
                .bind(org_plan_id)
                .bind(org_id)
                .bind(plan_uuid)
                .bind(OrgPlanStatus::Pending.to_i16())
                .bind(billing_period_str)
                .bind(created_by_user_id)
                .execute(pool)
                .await?;
            }
        }

        Self::get_by_org_id(db, org_id).await
    }

    /// Get the subscription plan for this subscription
    pub async fn get_plan(&self, db: &DatabasePool) -> Result<Plan, PlanError> {
        Plan::get_by_id(db, &self.plan_uuid).await
    }

    /// Activate or update an organization's selected plan after checkout.
    pub async fn update_after_checkout(
        db: &DatabasePool,
        org_plan_id: &Uuid,
        plan_uuid: &Uuid,
        billing_period: &str,
        current_period_start: DateTime<Utc>,
        current_period_end: DateTime<Utc>,
        updated_by_user_id: &Uuid,
    ) -> Result<(), PlanError> {
        match db {
            DatabasePool::Postgres(pool) => {
                sqlx::query(
                    "UPDATE org_plan
                     SET plan_uuid = $1,
                         billing_period = $2,
                         current_period_start = $3,
                         current_period_end = $4,
                         status_id = $5,
                         updated_at = NOW(),
                         updated_by_user_id = $6
                     WHERE org_plan_id = $7",
                )
                .bind(plan_uuid)
                .bind(billing_period)
                .bind(current_period_start)
                .bind(current_period_end)
                .bind(OrgPlanStatus::Active.to_i16())
                .bind(updated_by_user_id)
                .bind(org_plan_id)
                .execute(pool)
                .await?;
                Ok(())
            }
            DatabasePool::Sqlite(pool) => {
                sqlx::query(
                    "UPDATE org_plan
                     SET plan_uuid = ?,
                         billing_period = ?,
                         current_period_start = ?,
                         current_period_end = ?,
                         status_id = ?,
                         updated_at = datetime('now'),
                         updated_by_user_id = ?
                     WHERE org_plan_id = ?",
                )
                .bind(plan_uuid)
                .bind(billing_period)
                .bind(current_period_start)
                .bind(current_period_end)
                .bind(OrgPlanStatus::Active.to_i16())
                .bind(updated_by_user_id)
                .bind(org_plan_id)
                .execute(pool)
                .await?;
                Ok(())
            }
        }
    }

    /// Update subscription status
    pub async fn update_status(
        db: &DatabasePool,
        org_plan_id: &Uuid,
        status: OrgPlanStatus,
        updated_by_user_id: Option<&Uuid>,
    ) -> Result<(), PlanError> {
        match db {
            DatabasePool::Postgres(pool) => {
                sqlx::query(
                    "UPDATE org_plan
                     SET status_id = $1,
                         updated_at = NOW(),
                         updated_by_user_id = $2
                     WHERE org_plan_id = $3",
                )
                .bind(status.to_i16())
                .bind(updated_by_user_id)
                .bind(org_plan_id)
                .execute(pool)
                .await?;
                Ok(())
            }
            DatabasePool::Sqlite(pool) => {
                sqlx::query(
                    "UPDATE org_plan
                     SET status_id = ?,
                         updated_at = datetime('now'),
                         updated_by_user_id = ?
                     WHERE org_plan_id = ?",
                )
                .bind(status.to_i16())
                .bind(updated_by_user_id)
                .bind(org_plan_id)
                .execute(pool)
                .await?;
                Ok(())
            }
        }
    }

    /// Activate a free-tier subscription immediately (no provider checkout required).
    /// Sets status to Active and initializes the billing period to now.
    pub async fn activate_free(db: &DatabasePool, org_plan_id: &Uuid) -> Result<(), PlanError> {
        let now = chrono::Utc::now();
        let period_end = now + chrono::Duration::days(30);
        match db {
            DatabasePool::Postgres(pool) => {
                sqlx::query(
                    "UPDATE org_plan
                     SET status_id = $1,
                         current_period_start = $2,
                         current_period_end = $3,
                         updated_at = NOW()
                     WHERE org_plan_id = $4",
                )
                .bind(OrgPlanStatus::Active.to_i16())
                .bind(now)
                .bind(period_end)
                .bind(org_plan_id)
                .execute(pool)
                .await?;
                Ok(())
            }
            DatabasePool::Sqlite(pool) => {
                sqlx::query(
                    "UPDATE org_plan
                     SET status_id = ?,
                         current_period_start = ?,
                         current_period_end = ?,
                         updated_at = datetime('now')
                     WHERE org_plan_id = ?",
                )
                .bind(OrgPlanStatus::Active.to_i16())
                .bind(now)
                .bind(period_end)
                .bind(org_plan_id)
                .execute(pool)
                .await?;
                Ok(())
            }
        }
    }

    /// Record that the billing provider accepted a cancellation request.
    pub async fn mark_cancel_at_period_end(
        db: &DatabasePool,
        org_plan_id: &Uuid,
        updated_by_user_id: &Uuid,
    ) -> Result<(), PlanError> {
        match db {
            DatabasePool::Postgres(pool) => {
                sqlx::query(
                    "UPDATE org_plan
                     SET updated_at = NOW(), updated_by_user_id = $1
                     WHERE org_plan_id = $2",
                )
                .bind(updated_by_user_id)
                .bind(org_plan_id)
                .execute(pool)
                .await?;
                Ok(())
            }
            DatabasePool::Sqlite(pool) => {
                sqlx::query(
                    "UPDATE org_plan
                     SET updated_at = datetime('now'), updated_by_user_id = ?
                     WHERE org_plan_id = ?",
                )
                .bind(updated_by_user_id)
                .bind(org_plan_id)
                .execute(pool)
                .await?;
                Ok(())
            }
        }
    }

    /// Reactivate the public plan assignment after the provider reactivation succeeds.
    pub async fn reactivate(
        db: &DatabasePool,
        org_plan_id: &Uuid,
        updated_by_user_id: &Uuid,
    ) -> Result<(), PlanError> {
        Self::update_status(
            db,
            org_plan_id,
            OrgPlanStatus::Active,
            Some(updated_by_user_id),
        )
        .await
    }

    /// Deactivate the public plan assignment.
    pub async fn cancel_immediately(
        db: &DatabasePool,
        org_plan_id: &Uuid,
        updated_by_user_id: &Uuid,
    ) -> Result<(), PlanError> {
        Self::update_status(
            db,
            org_plan_id,
            OrgPlanStatus::Inactive,
            Some(updated_by_user_id),
        )
        .await
    }

    /// Update subscription period dates
    pub async fn update_period(
        db: &DatabasePool,
        org_plan_id: &Uuid,
        period_start: DateTime<Utc>,
        period_end: DateTime<Utc>,
    ) -> Result<(), PlanError> {
        match db {
            DatabasePool::Postgres(pool) => {
                sqlx::query(
                    "UPDATE org_plan
                     SET current_period_start = $1,
                         current_period_end = $2,
                         updated_at = NOW()
                     WHERE org_plan_id = $3",
                )
                .bind(period_start)
                .bind(period_end)
                .bind(org_plan_id)
                .execute(pool)
                .await?;
                Ok(())
            }
            DatabasePool::Sqlite(pool) => {
                sqlx::query(
                    "UPDATE org_plan
                     SET current_period_start = ?,
                         current_period_end = ?,
                         updated_at = datetime('now')
                     WHERE org_plan_id = ?",
                )
                .bind(period_start)
                .bind(period_end)
                .bind(org_plan_id)
                .execute(pool)
                .await?;
                Ok(())
            }
        }
    }

    /// Get subscription status enum
    pub fn status(&self) -> Option<OrgPlanStatus> {
        OrgPlanStatus::from_i16(self.status_id)
    }

    /// Get billing period enum
    pub fn billing_period_enum(&self) -> Option<BillingPeriod> {
        self.billing_period.parse().ok()
    }

    /// Check if subscription is active
    pub fn is_active(&self) -> bool {
        self.status() == Some(OrgPlanStatus::Active)
    }

    /// Get the plan name (requires fetching the plan)
    pub async fn get_plan_name(&self, db: &DatabasePool) -> Result<String, PlanError> {
        let plan = self.get_plan(db).await?;
        Ok(plan.plan_name)
    }

    /// Get all org IDs that have an active subscription.
    pub async fn get_all_active_org_ids(db: &DatabasePool) -> Result<Vec<Uuid>, PlanError> {
        match db {
            DatabasePool::Postgres(pool) => Ok(sqlx::query_scalar(
                "SELECT org_id FROM org_plan WHERE status_id = 1",
            )
            .fetch_all(pool)
            .await?),
            DatabasePool::Sqlite(pool) => Ok(sqlx::query_scalar(
                "SELECT org_id FROM org_plan WHERE status_id = 1",
            )
            .fetch_all(pool)
            .await?),
        }
    }
}

/// Compute the call data deletion cutoff (in days) for an organization.
///
/// Resolution order:
/// 1. If `org.features` has an explicit `call_retention_days` override, use that.
/// 2. Otherwise, look up the next plan tier (by `sort_order`) and use its
///    `call_retention_days` — this keeps enough data for a seamless upgrade.
/// 3. If the current plan is the top cloud tier (no next plan, or next plan is
///    unlimited), fall back to 2x the current plan's retention.
///
/// Returns -1 for unlimited (never delete).
pub async fn call_deletion_days_for_org(db: &DatabasePool, org_id: &Uuid) -> i32 {
    use super::features::keys;

    // Check for an explicit per-org override (not merged with plan)
    let org_features_json = match db {
        DatabasePool::Postgres(pool) => {
            sqlx::query_scalar::<_, Option<JsonValue>>("SELECT features FROM org WHERE org_id = $1")
                .bind(org_id)
                .fetch_optional(pool)
                .await
                .ok()
                .flatten()
                .flatten()
        }
        DatabasePool::Sqlite(pool) => {
            sqlx::query_scalar::<_, Option<String>>("SELECT features FROM org WHERE org_id = ?")
                .bind(org_id)
                .fetch_optional(pool)
                .await
                .ok()
                .flatten()
                .flatten()
                .and_then(|s| serde_json::from_str::<JsonValue>(&s).ok())
        }
    };

    if let Some(JsonValue::Object(ref map)) = org_features_json
        && let Some(val) = map.get(keys::CALL_RETENTION_DAYS)
        && let Some(days) = val.as_i64()
    {
        return days as i32;
    }

    // No org override — resolve from plan + dynamic next-tier lookup
    let subscription = match OrgPlan::get_by_org_id(db, org_id).await {
        Ok(s) => s,
        Err(_) => return -1, // no subscription = self-hosted / local dev
    };

    let plan = match subscription.get_plan(db).await {
        Ok(p) => p,
        Err(_) => return -1,
    };

    let retention = plan.get_features().call_retention_days();
    if retention < 0 {
        return -1; // unlimited
    }

    // Find the next tier up and use its visible retention as our deletion cutoff
    match Plan::get_next_active_by_sort_order(db, plan.sort_order).await {
        Ok(next_plan) => {
            let next_retention = next_plan.get_features().call_retention_days();
            if next_retention < 0 {
                retention * 2 // next tier is unlimited (Self-Host), use 2x
            } else {
                next_retention
            }
        }
        Err(_) => retention * 2, // top tier, no next plan
    }
}

/// Organization Usage Tracking
#[derive(Debug, Clone, FromRow, Serialize)]
pub struct OrgUsage {
    pub usage_id: Uuid,
    pub org_id: Uuid,
    pub usage_period_start: DateTime<Utc>,
    pub usage_period_end: DateTime<Utc>,
    pub runs_count: i32,
    pub team_members_count: i32,
    #[sqlx(json)]
    pub metrics: Option<JsonValue>,
    pub created_at: DateTime<Utc>,
}

impl OrgUsage {
    /// Record usage for an organization
    pub async fn record_usage(
        db: &DatabasePool,
        org_id: &Uuid,
        period_start: DateTime<Utc>,
        period_end: DateTime<Utc>,
        runs_count: i32,
        team_members_count: i32,
    ) -> Result<(), PlanError> {
        let usage_id = Uuid::now_v7();

        match db {
            DatabasePool::Postgres(pool) => {
                sqlx::query(
                    "INSERT INTO org_usage
                     (usage_id, org_id, usage_period_start, usage_period_end, runs_count, team_members_count)
                     VALUES ($1, $2, $3, $4, $5, $6)",
                )
                .bind(usage_id)
                .bind(org_id)
                .bind(period_start)
                .bind(period_end)
                .bind(runs_count)
                .bind(team_members_count)
                .execute(pool)
                .await?;
                Ok(())
            }
            DatabasePool::Sqlite(pool) => {
                sqlx::query(
                    "INSERT INTO org_usage
                     (usage_id, org_id, usage_period_start, usage_period_end, runs_count, team_members_count)
                     VALUES (?, ?, ?, ?, ?, ?)",
                )
                .bind(usage_id)
                .bind(org_id)
                .bind(period_start)
                .bind(period_end)
                .bind(runs_count)
                .bind(team_members_count)
                .execute(pool)
                .await?;
                Ok(())
            }
        }
    }

    /// Get current period usage for an organization
    pub async fn get_current_period_usage(
        db: &DatabasePool,
        org_id: &Uuid,
    ) -> Result<Option<Self>, PlanError> {
        match db {
            DatabasePool::Postgres(pool) => {
                let usage = sqlx::query_as::<_, Self>(
                    "SELECT * FROM org_usage
                     WHERE org_id = $1
                     ORDER BY usage_period_start DESC
                     LIMIT 1",
                )
                .bind(org_id)
                .fetch_optional(pool)
                .await?;
                Ok(usage)
            }
            DatabasePool::Sqlite(pool) => {
                let usage = sqlx::query_as::<_, Self>(
                    "SELECT * FROM org_usage
                     WHERE org_id = ?
                     ORDER BY usage_period_start DESC
                     LIMIT 1",
                )
                .bind(org_id)
                .fetch_optional(pool)
                .await?;
                Ok(usage)
            }
        }
    }
}

/// Real-time organization usage statistics
/// Calculated from actual data, not cached snapshots
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrgUsageStats {
    /// Runs in the current billing period
    pub runs_this_period: i64,
    /// Total file storage in bytes
    pub file_storage_bytes: i64,
    /// Number of active team members
    pub team_members_count: i32,
    /// Call data storage in bytes (within retention period)
    pub call_storage_bytes: i64,
    /// Total number of calls (within retention period)
    pub call_count: i64,
    /// Oldest call timestamp (within retention period)
    pub oldest_call_time: Option<DateTime<Utc>>,
    /// Store data storage in bytes (::hot::store)
    pub store_storage_bytes: i64,
    /// Tasks in the current billing period
    pub tasks_this_period: i64,
    /// Total task duration in milliseconds (current billing period)
    pub task_duration_ms: i64,
    /// Total compute unit seconds (CUS) consumed this billing period.
    /// Extracted from `result."compute-units"` on completed container tasks.
    pub compute_units: i64,
    /// Active schedules attached to active deployed builds in this org.
    pub active_schedules: i64,
}

impl OrgUsageStats {
    /// Calculate real-time usage statistics for an organization
    pub async fn calculate(
        db: &DatabasePool,
        org_id: &Uuid,
        period_start: DateTime<Utc>,
        retention_days: i32,
    ) -> Result<Self, PlanError> {
        match db {
            DatabasePool::Postgres(pool) => {
                Self::calculate_postgres(pool, org_id, period_start, retention_days).await
            }
            DatabasePool::Sqlite(pool) => {
                Self::calculate_sqlite(pool, org_id, period_start, retention_days).await
            }
        }
    }

    async fn calculate_postgres(
        pool: &sqlx::PgPool,
        org_id: &Uuid,
        period_start: DateTime<Utc>,
        retention_days: i32,
    ) -> Result<Self, PlanError> {
        // Pre-fetch env_ids for this org once (tiny table, instant) to eliminate
        // JOIN env from every subsequent query — lets Postgres use env_id indexes directly.
        let env_ids: Vec<Uuid> = sqlx::query_scalar("SELECT env_id FROM env WHERE org_id = $1")
            .bind(org_id)
            .fetch_all(pool)
            .await?;

        let runs_fut = sqlx::query_as::<_, (i64,)>(
            "SELECT COUNT(*)::bigint FROM run
             WHERE env_id = ANY($1) AND start_time >= $2 AND run_type_id != 7",
        )
        .bind(&env_ids)
        .bind(period_start)
        .fetch_one(pool);

        let file_fut = sqlx::query_as::<_, (Option<i64>,)>(
            "SELECT SUM(size)::bigint FROM file
             WHERE org_id = $1 AND active = true",
        )
        .bind(org_id)
        .fetch_one(pool);

        let team_fut = sqlx::query_as::<_, (i64,)>(
            "SELECT COUNT(*)::bigint FROM org_user
             WHERE org_id = $1 AND active = true",
        )
        .bind(org_id)
        .fetch_one(pool);

        let retention_str = retention_days.to_string();
        let call_fut = async {
            if retention_days < 0 {
                sqlx::query_as::<_, (i64, i64, Option<DateTime<Utc>>)>(
                    "SELECT COALESCE(SUM(c.size), 0)::bigint, COUNT(*)::bigint, MIN(c.start_time)
                     FROM call c
                     JOIN run r ON r.run_id = c.run_id
                     WHERE r.env_id = ANY($1)",
                )
                .bind(&env_ids)
                .fetch_one(pool)
                .await
            } else if retention_days == 0 {
                Ok((0i64, 0i64, None))
            } else {
                sqlx::query_as::<_, (i64, i64, Option<DateTime<Utc>>)>(
                    "SELECT COALESCE(SUM(c.size), 0)::bigint, COUNT(*)::bigint, MIN(c.start_time)
                     FROM call c
                     JOIN run r ON r.run_id = c.run_id
                     WHERE r.env_id = ANY($1) AND c.start_time > NOW() - ($2 || ' days')::interval",
                )
                .bind(&env_ids)
                .bind(&retention_str)
                .fetch_one(pool)
                .await
            }
        };

        let store_fut = sqlx::query_as::<_, (i64,)>(
            "SELECT COALESCE(SUM(size), 0)::bigint FROM store_map_entry WHERE org_id = $1",
        )
        .bind(org_id)
        .fetch_one(pool);

        let task_fut = sqlx::query_as::<_, (i64, Option<i64>, Option<i64>)>(
            "SELECT COUNT(*)::bigint, COALESCE(SUM(duration_ms), 0)::bigint,
                    COALESCE(SUM(CASE
                        WHEN result->'$val'->'err'->>'compute-units' IS NOT NULL
                            THEN (result->'$val'->'err'->>'compute-units')::bigint
                        WHEN result->>'compute-units' IS NOT NULL
                            THEN (result->>'compute-units')::bigint
                        ELSE 0
                    END), 0)::bigint
             FROM task
             WHERE env_id = ANY($1) AND created_at >= $2",
        )
        .bind(&env_ids)
        .bind(period_start)
        .fetch_one(pool);

        let schedule_fut = sqlx::query_as::<_, (i64,)>(
            "SELECT COUNT(*)::bigint FROM schedule s
             JOIN build b ON s.build_id = b.build_id
             JOIN project p ON b.project_id = p.project_id
             JOIN env e ON p.env_id = e.env_id
             WHERE s.active = true
               AND b.deployed = true
               AND b.active = true
               AND p.active = true
               AND e.active = true
               AND e.org_id = $1",
        )
        .bind(org_id)
        .fetch_one(pool);

        let (runs_row, file_row, team_row, call_row, store_row, task_row, schedule_row) = tokio::try_join!(
            runs_fut,
            file_fut,
            team_fut,
            call_fut,
            store_fut,
            task_fut,
            schedule_fut
        )?;

        Ok(Self {
            runs_this_period: runs_row.0,
            file_storage_bytes: file_row.0.unwrap_or(0),
            team_members_count: team_row.0 as i32,
            call_storage_bytes: call_row.0,
            call_count: call_row.1,
            oldest_call_time: call_row.2,
            store_storage_bytes: store_row.0,
            tasks_this_period: task_row.0,
            task_duration_ms: task_row.1.unwrap_or(0),
            compute_units: task_row.2.unwrap_or(0),
            active_schedules: schedule_row.0,
        })
    }

    async fn calculate_sqlite(
        pool: &sqlx::SqlitePool,
        org_id: &Uuid,
        period_start: DateTime<Utc>,
        retention_days: i32,
    ) -> Result<Self, PlanError> {
        // SQLite doesn't support ANY(...), so use an IN-subquery to avoid the JOIN.
        // The subquery is evaluated once and the result set is tiny (few env_ids per org).
        const ENV_SUB: &str = "env_id IN (SELECT env_id FROM env WHERE org_id = ?)";

        let runs_q = format!(
            "SELECT COUNT(*) FROM run WHERE {ENV_SUB} AND start_time >= ? AND run_type_id != 7",
        );
        let runs_fut = sqlx::query_as::<_, (i64,)>(sqlx::AssertSqlSafe(runs_q.as_str()))
            .bind(org_id)
            .bind(period_start)
            .fetch_one(pool);

        let file_fut = sqlx::query_as::<_, (Option<i64>,)>(
            "SELECT SUM(size) FROM file
             WHERE org_id = ? AND active = 1",
        )
        .bind(org_id)
        .fetch_one(pool);

        let team_fut = sqlx::query_as::<_, (i64,)>(
            "SELECT COUNT(*) FROM org_user
             WHERE org_id = ? AND active = 1",
        )
        .bind(org_id)
        .fetch_one(pool);

        let call_q_all = format!(
            "SELECT COALESCE(SUM(c.size), 0), COUNT(*), MIN(c.start_time)
             FROM call c
             JOIN run r ON r.run_id = c.run_id
             WHERE r.{ENV_SUB}",
        );
        let call_q_retention = format!(
            "SELECT COALESCE(SUM(c.size), 0), COUNT(*), MIN(c.start_time)
             FROM call c
             JOIN run r ON r.run_id = c.run_id
             WHERE r.{ENV_SUB} AND c.start_time > datetime('now', '-' || ? || ' days')",
        );
        let call_fut = async {
            if retention_days < 0 {
                sqlx::query_as::<_, (i64, i64, Option<DateTime<Utc>>)>(sqlx::AssertSqlSafe(
                    call_q_all.as_str(),
                ))
                .bind(org_id)
                .fetch_one(pool)
                .await
            } else if retention_days == 0 {
                Ok((0i64, 0i64, None))
            } else {
                sqlx::query_as::<_, (i64, i64, Option<DateTime<Utc>>)>(sqlx::AssertSqlSafe(
                    call_q_retention.as_str(),
                ))
                .bind(org_id)
                .bind(retention_days)
                .fetch_one(pool)
                .await
            }
        };

        let task_q = format!(
            "SELECT COUNT(*), COALESCE(SUM(duration_ms), 0),
                    COALESCE(SUM(CASE
                        WHEN json_extract(result, '$.\"$val\".\"err\".\"compute-units\"') IS NOT NULL
                            THEN CAST(json_extract(result, '$.\"$val\".\"err\".\"compute-units\"') AS INTEGER)
                        WHEN json_extract(result, '$.\"compute-units\"') IS NOT NULL
                            THEN CAST(json_extract(result, '$.\"compute-units\"') AS INTEGER)
                        ELSE 0
                    END), 0)
             FROM task
             WHERE {ENV_SUB} AND created_at >= ?",
        );
        let task_fut = sqlx::query_as::<_, (i64, Option<i64>, Option<i64>)>(sqlx::AssertSqlSafe(
            task_q.as_str(),
        ))
        .bind(org_id)
        .bind(period_start)
        .fetch_one(pool);

        let schedule_fut = sqlx::query_as::<_, (i64,)>(
            "SELECT COUNT(*) FROM schedule s
             JOIN build b ON s.build_id = b.build_id
             JOIN project p ON b.project_id = p.project_id
             JOIN env e ON p.env_id = e.env_id
             WHERE s.active = 1
               AND b.deployed = 1
               AND b.active = 1
               AND p.active = 1
               AND e.active = 1
               AND e.org_id = ?",
        )
        .bind(org_id)
        .fetch_one(pool);

        let (runs_row, file_row, team_row, call_row, task_row, schedule_row) = tokio::try_join!(
            runs_fut,
            file_fut,
            team_fut,
            call_fut,
            task_fut,
            schedule_fut
        )?;

        Ok(Self {
            runs_this_period: runs_row.0,
            file_storage_bytes: file_row.0.unwrap_or(0),
            team_members_count: team_row.0 as i32,
            call_storage_bytes: call_row.0,
            call_count: call_row.1,
            oldest_call_time: call_row.2,
            store_storage_bytes: 0,
            tasks_this_period: task_row.0,
            task_duration_ms: task_row.1.unwrap_or(0),
            compute_units: task_row.2.unwrap_or(0),
            active_schedules: schedule_row.0,
        })
    }

    /// Calculate usage percentage for runs (0-100+, can exceed 100 if over limit)
    pub fn runs_pct(&self, features: &super::features::Features) -> f64 {
        let limit = features.runs_per_month();
        if limit <= 0 {
            return 0.0; // Unlimited or no limit
        }
        (self.runs_this_period as f64 / limit as f64) * 100.0
    }

    /// Calculate usage percentage for file storage (0-100+)
    pub fn file_storage_pct(&self, features: &super::features::Features) -> f64 {
        let limit = features.storage_bytes();
        if limit <= 0 {
            return 0.0;
        }
        (self.file_storage_bytes as f64 / limit as f64) * 100.0
    }

    /// Calculate usage percentage for team members (0-100+)
    pub fn team_members_pct(&self, features: &super::features::Features) -> f64 {
        let limit = features.team_members();
        if limit <= 0 {
            return 0.0;
        }
        (self.team_members_count as f64 / limit as f64) * 100.0
    }

    /// Calculate usage percentage for call storage (0-100+)
    pub fn call_storage_pct(&self, features: &super::features::Features) -> f64 {
        let limit = features.call_storage_bytes();
        if limit <= 0 {
            return 0.0;
        }
        (self.call_storage_bytes as f64 / limit as f64) * 100.0
    }

    /// Calculate usage percentage for store storage (0-100+)
    pub fn store_storage_pct(&self, features: &super::features::Features) -> f64 {
        let limit = features.store_storage_bytes();
        if limit <= 0 {
            return 0.0;
        }
        (self.store_storage_bytes as f64 / limit as f64) * 100.0
    }

    /// Calculate usage percentage for task minutes this period (0-100+)
    pub fn task_minutes_pct(&self, features: &super::features::Features) -> f64 {
        let limit = features.task_minutes_per_month();
        if limit <= 0 {
            return 0.0;
        }
        let minutes_used = self.task_duration_ms as f64 / 60_000.0;
        (minutes_used / limit as f64) * 100.0
    }

    /// Calculate usage percentage for compute units this period (0-100+)
    pub fn compute_units_pct(&self, features: &super::features::Features) -> f64 {
        let limit = features.compute_units_per_month();
        if limit <= 0 {
            return 0.0;
        }
        (self.compute_units as f64 / limit as f64) * 100.0
    }

    pub fn active_schedules_pct(&self, features: &super::features::Features) -> f64 {
        let limit = features.active_schedules_per_org();
        if limit <= 0 {
            return 0.0;
        }
        (self.active_schedules as f64 / limit as f64) * 100.0
    }

    /// Check if any usage is approaching limit (>90%)
    pub fn has_warning(&self, features: &super::features::Features) -> bool {
        self.runs_pct(features) > 90.0
            || self.file_storage_pct(features) > 90.0
            || self.call_storage_pct(features) > 90.0
            || self.store_storage_pct(features) > 90.0
            || self.compute_units_pct(features) > 90.0
            || self.active_schedules_pct(features) > 90.0
    }

    /// Format a percentage as a string with one decimal place
    pub fn fmt_pct(pct: f64) -> String {
        format!("{:.1}", pct)
    }

    /// Format a number with commas as thousands separators
    pub fn fmt_num<T: std::borrow::Borrow<i64>>(num: T) -> String {
        let num: i64 = *num.borrow();
        if num < 0 {
            return format!("-{}", Self::fmt_num(Box::new(-num)));
        }
        let s = num.to_string();
        let mut result = String::new();
        for (i, c) in s.chars().rev().enumerate() {
            if i > 0 && i % 3 == 0 {
                result.push(',');
            }
            result.push(c);
        }
        result.chars().rev().collect()
    }

    /// Format a number (i32) with commas as thousands separators
    pub fn fmt_num_i32<T: std::borrow::Borrow<i32>>(num: T) -> String {
        Self::fmt_num(Box::new(*num.borrow() as i64))
    }

    /// Get the highest usage percentage category for upgrade prompts
    pub fn highest_usage_category(
        &self,
        features: &super::features::Features,
    ) -> Option<(&'static str, f64)> {
        let mut highest: Option<(&'static str, f64)> = None;

        let runs_pct = self.runs_pct(features);
        if runs_pct > 90.0 && highest.is_none_or(|(_, pct)| runs_pct > pct) {
            highest = Some(("runs", runs_pct));
        }

        let file_pct = self.file_storage_pct(features);
        if file_pct > 90.0 && highest.is_none_or(|(_, pct)| file_pct > pct) {
            highest = Some(("file_storage", file_pct));
        }

        let call_pct = self.call_storage_pct(features);
        if call_pct > 90.0 && highest.is_none_or(|(_, pct)| call_pct > pct) {
            highest = Some(("call_storage", call_pct));
        }

        let store_pct = self.store_storage_pct(features);
        if store_pct > 90.0 && highest.is_none_or(|(_, pct)| store_pct > pct) {
            highest = Some(("store_storage", store_pct));
        }

        let cus_pct = self.compute_units_pct(features);
        if cus_pct > 90.0 && highest.is_none_or(|(_, pct)| cus_pct > pct) {
            highest = Some(("compute_units", cus_pct));
        }

        let schedule_pct = self.active_schedules_pct(features);
        if schedule_pct > 90.0 && highest.is_none_or(|(_, pct)| schedule_pct > pct) {
            highest = Some(("active_schedules", schedule_pct));
        }

        highest
    }

    /// Format compute unit seconds with commas (e.g., "12,450 CUS")
    pub fn fmt_cus<T: std::borrow::Borrow<i64>>(cus: T) -> String {
        format!("{} Compute Unit Seconds (CUS)", Self::fmt_num(cus))
    }
}

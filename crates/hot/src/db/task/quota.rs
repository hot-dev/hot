use chrono::{Datelike, TimeZone, Utc};
use serde_json::Value;
use uuid::Uuid;

use super::{Task, TaskError};
use crate::db::{DatabasePool, DatabaseScope, Features, MaintenanceOperation};

type TaskAccountPlan<T> = (Option<T>, Option<String>, Option<chrono::DateTime<Utc>>);

/// Backend-resolved maximum billable execution. The worker computes this from
/// the same effective resource profile and timeout used to execute the task.
#[derive(Debug, Clone, Copy)]
pub struct TaskQuotaReservation {
    pub compute_units: i64,
    pub duration_ms: i64,
}

impl Task {
    pub async fn quota_reservation(
        db: &DatabasePool,
        task_id: &Uuid,
    ) -> Result<TaskQuotaReservation, TaskError> {
        let (compute_units, duration_ms): (i64, i64) =
            match db {
                DatabasePool::Postgres(pool) => sqlx::query_as(
                    "SELECT reserved_compute_units,reserved_duration_ms FROM task WHERE task_id=$1",
                )
                .bind(task_id)
                .fetch_one(pool)
                .await?,
                DatabasePool::Sqlite(pool) => sqlx::query_as(
                    "SELECT reserved_compute_units,reserved_duration_ms FROM task WHERE task_id=?",
                )
                .bind(task_id)
                .fetch_one(pool)
                .await?,
            };
        Ok(TaskQuotaReservation {
            compute_units,
            duration_ms,
        })
    }

    /// Discovery only; callers must verify local runtime ownership and termination
    /// before settling. An old heartbeat alone never releases cancelled capacity.
    pub async fn unsettled_cancelled_containers(db: &DatabasePool) -> Result<Vec<Task>, TaskError> {
        match db {
            DatabasePool::Postgres(pool) => Ok(sqlx::query_as("SELECT t.*,ts.name AS status FROM task t JOIN task_status ts ON ts.task_status_id=t.task_status_id WHERE t.task_status_id=5 AND t.task_type='container' AND t.reserved_duration_ms>0").fetch_all(pool).await?),
            DatabasePool::Sqlite(pool) => Ok(sqlx::query_as("SELECT t.*,ts.name AS status FROM task t JOIN task_status ts ON ts.task_status_id=t.task_status_id WHERE t.task_status_id=5 AND t.task_type='container' AND t.reserved_duration_ms>0").fetch_all(pool).await?),
        }
    }

    /// Claim and reserve account capacity in one root-locked transaction.
    /// A quota error leaves the task queued for a later delivery. A false result
    /// means another claim or terminal transition already won.
    pub async fn claim_for_worker_with_quota(
        db: &DatabasePool,
        task_id: &Uuid,
        worker_id: &str,
        reserve: TaskQuotaReservation,
    ) -> Result<bool, TaskError> {
        if reserve.compute_units < 0 || reserve.duration_ms <= 0 {
            return Err(TaskError::QuotaExceeded(
                "Invalid task quota reservation".into(),
            ));
        }
        // Read through the caller's already-authorized exact task scope first.
        let task = Self::get(db, task_id).await?;
        if task.task_status_id != 1 {
            return Ok(false);
        }
        if task.task_type == "container" && reserve.compute_units == 0 {
            return Err(TaskError::QuotaExceeded(
                "Container claim requires a compute reservation".into(),
            ));
        }
        let env = crate::db::env::Env::get_env(db, &task.env_id)
            .await
            .map_err(|e| TaskError::QuotaExceeded(format!("Cannot resolve task owner: {e}")))?;
        if let DatabasePool::Postgres(pool) = db
            && !matches!(pool.scope()?, DatabaseScope::Environment { org_id, env_id } if org_id == env.org_id && env_id == task.env_id)
        {
            return Err(TaskError::QuotaExceeded(
                "Task claim requires its exact environment scope".into(),
            ));
        }
        let root_id = crate::db::org::Org::billing_owner_id_for(db, &env.org_id)
            .await
            .map_err(|e| TaskError::QuotaExceeded(format!("Cannot resolve task account: {e}")))?;
        let billing = db.with_scope(DatabaseScope::Maintenance {
            operation: MaintenanceOperation::Billing,
            org_id: Some(root_id),
        });
        let now = Utc::now();
        let month_start = Utc
            .with_ymd_and_hms(now.year(), now.month(), 1, 0, 0, 0)
            .single()
            .expect("valid month");
        match &billing {
            DatabasePool::Postgres(pool) => {
                let mut tx = pool.begin().await?;
                let overrides: Option<Value> =
                    sqlx::query_scalar("SELECT features FROM org WHERE org_id=$1 FOR UPDATE")
                        .bind(root_id)
                        .fetch_one(&mut *tx)
                        .await?;
                let queued: bool =
                    sqlx::query_scalar("SELECT task_status_id=1 FROM task WHERE task_id=$1")
                        .bind(task_id)
                        .fetch_one(&mut *tx)
                        .await?;
                if !queued {
                    tx.rollback().await?;
                    return Ok(false);
                }
                let admitted: bool = sqlx::query_scalar("SELECT e.active AND o.active AND o.lifecycle_state='active' AND r.active AND r.lifecycle_state='active' FROM env e JOIN org o ON o.org_id=e.org_id JOIN org r ON r.org_id=COALESCE(o.parent_org_id,o.org_id) WHERE e.env_id=$1")
                    .bind(task.env_id).fetch_one(&mut *tx).await?;
                if !admitted {
                    return Err(TaskError::QuotaExceeded(
                        "Task environment is suspended".into(),
                    ));
                }
                let plan: Option<TaskAccountPlan<Value>> = sqlx::query_as(
                    "SELECT COALESCE(p.features,'{}'),p.plan_id,op.current_period_start FROM org_plan op JOIN plan p ON p.plan_uuid=op.plan_uuid WHERE op.org_id=$1"
                ).bind(root_id).fetch_optional(&mut *tx).await?;
                let period = plan.as_ref().and_then(|p| p.2).unwrap_or(month_start);
                let usage: (i64,i64,i64,i64,i64) = sqlx::query_as(
                    "SELECT
                     COUNT(*) FILTER(WHERE (t.task_status_id=2 OR (t.task_status_id=5 AND t.task_type='container' AND t.reserved_duration_ms>0)) AND t.task_type='container')::bigint,
                     COALESCE(SUM(CASE WHEN (t.task_status_id=2 OR (t.task_status_id=5 AND t.task_type='container' AND t.reserved_duration_ms>0)) THEN CASE WHEN t.reserved_compute_units>0 THEN t.reserved_compute_units WHEN t.task_type='container' THEN ((GREATEST(t.timeout_ms,0)*32+999)/1000) ELSE 0 END ELSE 0 END),0)::bigint,
                     COALESCE(SUM(CASE WHEN NOT (t.task_status_id=2 OR (t.task_status_id=5 AND t.task_type='container' AND t.reserved_duration_ms>0)) AND t.task_type='container' AND COALESCE(t.stop_time,t.start_time,t.created_at) >= $2 THEN GREATEST(COALESCE((t.result->'$val'->'err'->>'compute-units')::bigint,(t.result->>'compute-units')::bigint,t.reserved_compute_units,0),0) ELSE 0 END),0)::bigint,
                     COALESCE(SUM(CASE WHEN (t.task_status_id=2 OR (t.task_status_id=5 AND t.task_type='container' AND t.reserved_duration_ms>0)) THEN CASE WHEN t.reserved_duration_ms>0 THEN t.reserved_duration_ms ELSE GREATEST(t.timeout_ms,0) END ELSE 0 END),0)::bigint,
                     COALESCE(SUM(CASE WHEN NOT (t.task_status_id=2 OR (t.task_status_id=5 AND t.task_type='container' AND t.reserved_duration_ms>0)) AND COALESCE(t.stop_time,t.start_time,t.created_at) >= $2 THEN GREATEST(COALESCE(t.duration_ms,0),CASE WHEN t.task_status_id=5 THEN t.reserved_duration_ms ELSE 0 END,0) ELSE 0 END),0)::bigint
                     FROM task t JOIN env e ON e.env_id=t.env_id JOIN org o ON o.org_id=e.org_id WHERE o.org_id=$1 OR o.parent_org_id=$1"
                ).bind(root_id).bind(period).fetch_one(&mut *tx).await?;
                check_capacity(
                    plan.as_ref().and_then(|p| p.0.as_ref()),
                    overrides.as_ref(),
                    plan.as_ref()
                        .is_some_and(|p| p.1.as_deref() == Some("hot-free")),
                    task.task_type == "container",
                    &reserve,
                    usage,
                )?;
                {
                    crate::db::run::quota::admit_postgres(
                        &mut tx,
                        env.org_id,
                        task.env_id,
                        *task_id,
                    )
                    .await
                    .map_err(|e| TaskError::QuotaExceeded(e.to_string()))?;
                }
                DatabaseScope::Environment {
                    org_id: env.org_id,
                    env_id: task.env_id,
                }
                .apply_to_connection(&mut tx)
                .await?;
                let changed = sqlx::query("UPDATE task SET task_status_id=2,start_time=$2,worker_id=$3,last_heartbeat_at=$2,reserved_compute_units=$4,reserved_duration_ms=$5 WHERE task_id=$1 AND task_status_id=1")
                    .bind(task_id).bind(now).bind(worker_id).bind(reserve.compute_units).bind(reserve.duration_ms).execute(&mut *tx).await?.rows_affected();
                if changed == 0 {
                    tx.rollback().await?;
                    return Ok(false);
                }
                tx.commit().await?;
                Ok(true)
            }
            DatabasePool::Sqlite(pool) => {
                let mut tx = pool.begin().await?;
                sqlx::query("UPDATE org SET org_id=org_id WHERE org_id=?")
                    .bind(root_id)
                    .execute(&mut *tx)
                    .await?;
                let overrides: Option<String> =
                    sqlx::query_scalar("SELECT features FROM org WHERE org_id=?")
                        .bind(root_id)
                        .fetch_one(&mut *tx)
                        .await?;
                let queued: bool =
                    sqlx::query_scalar("SELECT task_status_id=1 FROM task WHERE task_id=?")
                        .bind(task_id)
                        .fetch_one(&mut *tx)
                        .await?;
                if !queued {
                    tx.rollback().await?;
                    return Ok(false);
                }
                let admitted: bool = sqlx::query_scalar("SELECT e.active AND o.active AND o.lifecycle_state='active' AND r.active AND r.lifecycle_state='active' FROM env e JOIN org o ON o.org_id=e.org_id JOIN org r ON r.org_id=COALESCE(o.parent_org_id,o.org_id) WHERE e.env_id=?")
                    .bind(task.env_id).fetch_one(&mut *tx).await?;
                if !admitted {
                    return Err(TaskError::QuotaExceeded(
                        "Task environment is suspended".into(),
                    ));
                }
                let plan: Option<TaskAccountPlan<String>> = sqlx::query_as(
                    "SELECT COALESCE(p.features,'{}'),p.plan_id,op.current_period_start FROM org_plan op JOIN plan p ON p.plan_uuid=op.plan_uuid WHERE op.org_id=?"
                ).bind(root_id).fetch_optional(&mut *tx).await?;
                let period = plan.as_ref().and_then(|p| p.2).unwrap_or(month_start);
                let overrides = parse_features(overrides)?;
                let plan_features = parse_features(plan.as_ref().and_then(|p| p.0.clone()))?;
                let usage: (i64,i64,i64,i64,i64) = sqlx::query_as(
                    "SELECT
                     COALESCE(SUM(CASE WHEN (t.task_status_id=2 OR (t.task_status_id=5 AND t.task_type='container' AND t.reserved_duration_ms>0)) AND t.task_type='container' THEN 1 ELSE 0 END),0),
                     COALESCE(SUM(CASE WHEN (t.task_status_id=2 OR (t.task_status_id=5 AND t.task_type='container' AND t.reserved_duration_ms>0)) THEN CASE WHEN t.reserved_compute_units>0 THEN t.reserved_compute_units WHEN t.task_type='container' THEN ((MAX(t.timeout_ms,0)*32+999)/1000) ELSE 0 END ELSE 0 END),0),
                     COALESCE(SUM(CASE WHEN NOT (t.task_status_id=2 OR (t.task_status_id=5 AND t.task_type='container' AND t.reserved_duration_ms>0)) AND t.task_type='container' AND COALESCE(t.stop_time,t.start_time,t.created_at) >= ? THEN MAX(COALESCE(CAST(json_extract(t.result,'$.\"$val\".err.\"compute-units\"') AS integer),CAST(json_extract(t.result,'$.\"compute-units\"') AS integer),t.reserved_compute_units,0),0) ELSE 0 END),0),
                     COALESCE(SUM(CASE WHEN (t.task_status_id=2 OR (t.task_status_id=5 AND t.task_type='container' AND t.reserved_duration_ms>0)) THEN CASE WHEN t.reserved_duration_ms>0 THEN t.reserved_duration_ms ELSE MAX(t.timeout_ms,0) END ELSE 0 END),0),
                     COALESCE(SUM(CASE WHEN NOT (t.task_status_id=2 OR (t.task_status_id=5 AND t.task_type='container' AND t.reserved_duration_ms>0)) AND COALESCE(t.stop_time,t.start_time,t.created_at) >= ? THEN MAX(COALESCE(t.duration_ms,0),CASE WHEN t.task_status_id=5 THEN t.reserved_duration_ms ELSE 0 END,0) ELSE 0 END),0)
                     FROM task t JOIN env e ON e.env_id=t.env_id JOIN org o ON o.org_id=e.org_id WHERE o.org_id=? OR o.parent_org_id=?"
                ).bind(period).bind(period).bind(root_id).bind(root_id).fetch_one(&mut *tx).await?;
                check_capacity(
                    plan_features.as_ref(),
                    overrides.as_ref(),
                    plan.as_ref()
                        .is_some_and(|p| p.1.as_deref() == Some("hot-free")),
                    task.task_type == "container",
                    &reserve,
                    usage,
                )?;
                {
                    crate::db::run::quota::admit_sqlite(&mut tx, env.org_id, task.env_id, *task_id)
                        .await
                        .map_err(|e| TaskError::QuotaExceeded(e.to_string()))?;
                }
                let changed=sqlx::query("UPDATE task SET task_status_id=2,start_time=?,worker_id=?,last_heartbeat_at=?,reserved_compute_units=?,reserved_duration_ms=? WHERE task_id=? AND task_status_id=1")
                    .bind(now).bind(worker_id).bind(now).bind(reserve.compute_units).bind(reserve.duration_ms).bind(task_id).execute(&mut *tx).await?.rows_affected();
                if changed == 0 {
                    tx.rollback().await?;
                    return Ok(false);
                }
                tx.commit().await?;
                Ok(true)
            }
        }
    }
}

fn parse_features(json: Option<String>) -> Result<Option<Value>, TaskError> {
    json.map(|s| {
        serde_json::from_str(&s)
            .map_err(|_| TaskError::QuotaExceeded("Malformed account features".into()))
    })
    .transpose()
}

fn check_capacity(
    plan: Option<&Value>,
    overrides: Option<&Value>,
    is_free: bool,
    container: bool,
    reserve: &TaskQuotaReservation,
    usage: (i64, i64, i64, i64, i64),
) -> Result<(), TaskError> {
    let features = if plan.is_none() && overrides.is_none() {
        Features::unlimited()
    } else {
        Features::resolve(plan, overrides)
    };
    let concurrent = features.box_concurrent_tasks();
    if container && concurrent >= 0 && usage.0 >= concurrent {
        return Err(TaskError::QuotaExceeded(
            "Account concurrent container limit reached".into(),
        ));
    }
    let monthly = features.compute_units_per_month();
    if container && monthly == 0 {
        return Err(TaskError::QuotaExceeded(
            "Container tasks are disabled on this account".into(),
        ));
    }
    let total = usage
        .1
        .checked_add(usage.2)
        .and_then(|v| v.checked_add(reserve.compute_units));
    let budget = features.compute_units_budget();
    if (budget > 0 && total.is_none_or(|v| v > budget))
        || (is_free && monthly >= 0 && total.is_none_or(|v| v > monthly))
    {
        return Err(TaskError::QuotaExceeded("Account compute quota cannot cover this task's maximum execution; reduce its timeout or increase capacity".into()));
    }
    let minutes = i64::from(features.task_minutes_per_month());
    if is_free
        && minutes >= 0
        && usage
            .3
            .checked_add(usage.4)
            .and_then(|v| v.checked_add(reserve.duration_ms))
            .is_none_or(|v| v > minutes.saturating_mul(60_000))
    {
        return Err(TaskError::QuotaExceeded(
            "Account task-minute quota cannot cover this task's maximum execution".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::task::TaskStatus;

    async fn fixture(features: &str) -> (DatabasePool, Uuid, Uuid, Uuid) {
        let db = crate::db::test_db().await;
        let DatabasePool::Sqlite(pool) = &db else {
            unreachable!()
        };
        let root = Uuid::now_v7();
        sqlx::query("INSERT INTO org (org_id,name,slug,org_type,features,created_by_user_id) VALUES (?,'Root',?,'organization',?,?)")
            .bind(root).bind(root.to_string()).bind(features).bind(root).execute(pool).await.unwrap();
        let mut envs = Vec::new();
        for _ in 0..2 {
            let child = Uuid::now_v7();
            let env = Uuid::now_v7();
            sqlx::query("INSERT INTO org (org_id,parent_org_id,name,slug,org_type,created_by_user_id) VALUES (?,?,'Child',?,'organization',?)")
                .bind(child).bind(root).bind(child.to_string()).bind(root).execute(pool).await.unwrap();
            sqlx::query("INSERT INTO env (env_id,org_id,name,created_by_user_id) VALUES (?,?,'development',?)")
                .bind(env).bind(child).bind(root).execute(pool).await.unwrap();
            envs.push(env);
        }
        (db, root, envs[0], envs[1])
    }
    async fn queued(db: &DatabasePool, env: Uuid) -> Uuid {
        let id = Uuid::now_v7();
        Task::insert(
            db,
            &id,
            &env,
            &Uuid::now_v7(),
            &Uuid::now_v7(),
            None,
            "::box/run",
            None,
            None,
            "container",
            60_000,
            None,
        )
        .await
        .unwrap();
        id
    }
    #[tokio::test]
    async fn concurrent_sibling_claims_share_one_atomic_container_slot() {
        let (db, root, e1, e2) = fixture(
            r#"{"runs_per_month":-1,"box_concurrent_tasks":1,"compute_units_per_month":-1}"#,
        )
        .await;
        let a = queued(&db, e1).await;
        let b = queued(&db, e2).await;
        let reserve = TaskQuotaReservation {
            compute_units: 60,
            duration_ms: 60_000,
        };
        let (ar, br) = tokio::join!(
            Task::claim_for_worker_with_quota(&db, &a, "a", reserve),
            Task::claim_for_worker_with_quota(&db, &b, "b", reserve)
        );
        assert_eq!(
            usize::from(matches!(ar, Ok(true))) + usize::from(matches!(br, Ok(true))),
            1
        );
        let (running, pending) = if ar.is_ok() { (a, b) } else { (b, a) };
        assert_eq!(Task::get(&db, &pending).await.unwrap().task_status_id, 1);
        let DatabasePool::Sqlite(pool) = &db else {
            unreachable!()
        };
        let consumed: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM run_admission WHERE run_id=?")
            .bind(pending)
            .fetch_one(pool)
            .await
            .unwrap();
        assert_eq!(
            consumed, 0,
            "a rejected task claim cannot consume a run slot"
        );
        Task::complete(
            &db,
            &running,
            &TaskStatus::Completed,
            Some(&serde_json::json!({"compute-units":1})),
            Some(1000),
            None,
        )
        .await
        .unwrap();
        assert!(
            Task::claim_for_worker_with_quota(&db, &pending, "retry", reserve)
                .await
                .unwrap()
        );
        let DatabasePool::Sqlite(pool) = &db else {
            unreachable!()
        };
        sqlx::query("UPDATE org SET lifecycle_state='suspended',active=0 WHERE org_id=?")
            .bind(root)
            .execute(pool)
            .await
            .unwrap();
        let next = queued(&db, e1).await;
        assert!(
            Task::claim_for_worker_with_quota(&db, &next, "blocked", reserve)
                .await
                .is_err()
        );
    }
    #[tokio::test]
    async fn compute_reservations_cover_siblings_and_release_unused_capacity() {
        let (db,_,e1,e2)=fixture(r#"{"runs_per_month":-1,"box_concurrent_tasks":10,"compute_units_per_month":-1,"compute_units_budget":100}"#).await;
        let a = queued(&db, e1).await;
        let b = queued(&db, e2).await;
        let reserve = TaskQuotaReservation {
            compute_units: 60,
            duration_ms: 60_000,
        };
        assert!(
            Task::claim_for_worker_with_quota(&db, &a, "a", reserve)
                .await
                .unwrap()
        );
        assert!(matches!(
            Task::claim_for_worker_with_quota(&db, &b, "b", reserve).await,
            Err(TaskError::QuotaExceeded(_))
        ));
        Task::complete(
            &db,
            &a,
            &TaskStatus::Completed,
            Some(&serde_json::json!({"compute-units":20})),
            Some(20_000),
            None,
        )
        .await
        .unwrap();
        assert!(
            Task::claim_for_worker_with_quota(&db, &b, "b", reserve)
                .await
                .unwrap()
        );
        let c = queued(&db, e1).await;
        let small = TaskQuotaReservation {
            compute_units: 30,
            duration_ms: 30_000,
        };
        assert!(
            Task::claim_for_worker_with_quota(&db, &c, "c", small)
                .await
                .is_err()
        );
        Task::complete(
            &db,
            &b,
            &TaskStatus::Failed,
            Some(&serde_json::json!({"compute-units":0})),
            Some(0),
            None,
        )
        .await
        .unwrap();
        assert!(
            Task::claim_for_worker_with_quota(&db, &c, "c", small)
                .await
                .unwrap()
        );
        assert!(
            !Task::claim_for_worker_with_quota(&db, &c, "duplicate", small)
                .await
                .unwrap()
        );
    }
    #[tokio::test]
    async fn cancellation_holds_sibling_capacity_until_fenced_usage_settlement() {
        let (db,_,e1,e2)=fixture(r#"{"runs_per_month":-1,"box_concurrent_tasks":1,"compute_units_per_month":-1,"compute_units_budget":100}"#).await;
        let a = queued(&db, e1).await;
        let b = queued(&db, e2).await;
        let unstarted = queued(&db, e1).await;
        assert!(Task::cancel(&db, &unstarted).await.unwrap());
        let reservation = TaskQuotaReservation {
            compute_units: 60,
            duration_ms: 60000,
        };
        assert!(
            Task::claim_for_worker_with_quota(&db, &a, "owner", reservation)
                .await
                .unwrap()
        );
        assert!(Task::cancel(&db, &a).await.unwrap());
        assert!(!Task::cancel(&db, &a).await.unwrap());
        let DatabasePool::Sqlite(pool) = &db else {
            unreachable!()
        };
        sqlx::query("UPDATE task SET last_heartbeat_at='2000-01-01 00:00:00' WHERE task_id=?")
            .bind(a)
            .execute(pool)
            .await
            .unwrap();
        assert!(
            Task::claim_for_worker_with_quota(&db, &b, "next", reservation)
                .await
                .is_err(),
            "cancellation or stale heartbeat cannot release running capacity"
        );
        let measured = serde_json::json!({"compute-units":50});
        assert!(
            !Task::complete(
                &db,
                &a,
                &TaskStatus::Cancelled,
                Some(&measured),
                Some(50000),
                None
            )
            .await
            .unwrap()
        );
        assert!(
            !Task::complete(
                &db,
                &a,
                &TaskStatus::Cancelled,
                Some(&measured),
                Some(50000),
                Some("stale")
            )
            .await
            .unwrap()
        );
        assert!(
            !Task::complete(
                &db,
                &a,
                &TaskStatus::Completed,
                Some(&measured),
                Some(50000),
                Some("owner")
            )
            .await
            .unwrap()
        );
        assert!(
            !Task::complete(
                &db,
                &a,
                &TaskStatus::Cancelled,
                None,
                Some(50000),
                Some("owner")
            )
            .await
            .unwrap()
        );
        assert_eq!(
            Task::quota_reservation(&db, &a)
                .await
                .unwrap()
                .compute_units,
            60
        );
        assert!(
            Task::complete(
                &db,
                &a,
                &TaskStatus::Cancelled,
                Some(&measured),
                Some(50000),
                Some("owner")
            )
            .await
            .unwrap()
        );
        assert_eq!(
            Task::get(&db, &a).await.unwrap().task_status_id,
            TaskStatus::Cancelled.as_id()
        );
        assert!(
            !Task::complete(
                &db,
                &a,
                &TaskStatus::Cancelled,
                Some(&serde_json::json!({"compute-units":0})),
                Some(0),
                Some("owner")
            )
            .await
            .unwrap(),
            "duplicate settlement cannot refund billed compute"
        );
        assert_eq!(
            Task::quota_reservation(&db, &a).await.unwrap().duration_ms,
            0
        );
        assert!(
            Task::claim_for_worker_with_quota(&db, &b, "next", reservation)
                .await
                .is_err(),
            "settled cancellation still consumes measured compute"
        );
        let small = TaskQuotaReservation {
            compute_units: 40,
            duration_ms: 40000,
        };
        assert!(
            Task::claim_for_worker_with_quota(&db, &b, "next", small)
                .await
                .unwrap()
        );
    }
}

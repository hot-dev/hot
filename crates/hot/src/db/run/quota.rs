//! Durable account-wide run admission, separate from retention of run history.
use chrono::{DateTime, Utc};
use serde_json::Value;
use uuid::Uuid;

use super::{Run, RunError};
use crate::db::{DatabasePool, DatabaseScope, Features, MaintenanceOperation};

/// Days the admission ledger keeps a row. Nothing reads older rows: billing
/// periods, annual ones included, start later, and a redelivery of the same
/// run comes long before.
pub const ADMISSION_RETENTION_DAYS: i64 = 400;

impl Run {
    /// Trusted retention maintenance: delete admission ledger rows older than
    /// `days`, `batch_size` rows at a time. Run counters are kept separately
    /// and don't decrease when ledger rows go.
    pub async fn delete_admissions_older_than(
        db: &DatabasePool,
        days: i64,
        batch_size: i64,
    ) -> Result<u64, RunError> {
        let cutoff = Utc::now() - chrono::Duration::days(days);
        let mut total = 0;
        loop {
            let deleted = match db {
                DatabasePool::Postgres(pool) => {
                    sqlx::query("DELETE FROM run_admission WHERE run_id IN (SELECT run_id FROM run_admission WHERE admitted_at < $1 LIMIT $2)")
                        .bind(cutoff)
                        .bind(batch_size)
                        .execute(pool)
                        .await?
                        .rows_affected()
                }
                // Backfilled rows store SQLite's timestamp format, so compare
                // as julian days, as the usage counters do.
                DatabasePool::Sqlite(pool) => {
                    sqlx::query("DELETE FROM run_admission WHERE run_id IN (SELECT run_id FROM run_admission WHERE julianday(admitted_at) < julianday($1) LIMIT $2)")
                        .bind(cutoff)
                        .bind(batch_size)
                        .execute(pool)
                        .await?
                        .rows_affected()
                }
            };
            total += deleted;
            if deleted < batch_size as u64 {
                return Ok(total);
            }
        }
    }

    /// Reserve one execution before running any user bytecode. Reusing the same
    /// immutable run identity does not consume another slot. A new VM still
    /// checks lifecycle, even when a previous delivery reserved that identity.
    pub async fn admit_execution(
        db: &DatabasePool,
        org_id: &Uuid,
        env_id: &Uuid,
        run_id: &Uuid,
    ) -> Result<(), RunError> {
        Self::admit_executions(db, org_id, env_id, &[*run_id]).await
    }

    /// Reserve all handlers of one event atomically so exhaustion cannot execute
    /// an initial subset and force the event queue to replay those side effects.
    pub async fn admit_executions(
        db: &DatabasePool,
        org_id: &Uuid,
        env_id: &Uuid,
        run_ids: &[Uuid],
    ) -> Result<(), RunError> {
        if let DatabasePool::Postgres(pool) = db
            && !matches!(pool.scope()?, DatabaseScope::Environment { org_id: o, env_id: e } if o == *org_id && e == *env_id)
        {
            return Err(RunError::ScopeDenied);
        }
        let env = crate::db::env::Env::get_env_uncached(db, env_id)
            .await
            .map_err(env_lookup_error)?;
        if env.org_id != *org_id {
            return Err(RunError::ScopeDenied);
        }
        let root_id = crate::db::org::Org::billing_owner_id_for(db, org_id)
            .await
            .map_err(org_lookup_error)?;
        let billing = db.with_scope(DatabaseScope::Maintenance {
            operation: MaintenanceOperation::Billing,
            org_id: Some(root_id),
        });
        match &billing {
            DatabasePool::Postgres(pool) => {
                // `admit_postgres` locks the account root only when the account
                // has a run limit; the lock then holds across the whole group.
                let mut tx = pool.begin().await?;
                for run_id in run_ids {
                    admit_postgres(&mut tx, *org_id, *env_id, *run_id).await?;
                }
                tx.commit().await?;
            }
            DatabasePool::Sqlite(pool) => {
                let mut tx = pool.begin().await?;
                // Acquire the SQLite writer lock before reading inside this tx.
                sqlx::query("UPDATE org SET updated_at=updated_at WHERE org_id=$1")
                    .bind(root_id)
                    .execute(&mut *tx)
                    .await?;
                for run_id in run_ids {
                    admit_sqlite(&mut tx, *org_id, *env_id, *run_id).await?;
                }
                tx.commit().await?;
            }
        }
        Ok(())
    }
}

// These helpers deliberately take the caller's transaction connection. Task
// claims reserve execution time and the run slot together, or neither commits.
//
// `$lock` serializes admissions against the account's run count. SQLite must
// take its writer lock before reading, so it always locks first. PostgreSQL
// locks with NO KEY UPDATE, which doesn't block the foreign-key checks of
// inserts that reference the org, and only when the account has a run limit:
// an unlimited account has no count to protect, and a racing delivery of the
// same run is settled by the ledger's primary key.
macro_rules! admission {
    ($name:ident, $connection:ty, $lock:literal, $lock_first:literal, $usage:ident) => {
        pub(crate) async fn $name(
            conn: &mut $connection,
            org_id: Uuid,
            env_id: Uuid,
            run_id: Uuid,
        ) -> Result<(), RunError> {
            let root_id: Option<Uuid> = sqlx::query_scalar(
                "SELECT COALESCE(o.parent_org_id,o.org_id) FROM env e JOIN org o ON o.org_id=e.org_id WHERE e.env_id=$1 AND e.org_id=$2"
            ).bind(env_id).bind(org_id).fetch_optional(&mut *conn).await?;
            let root_id = root_id.ok_or(RunError::ScopeDenied)?;
            if $lock_first {
                sqlx::query($lock).bind(root_id).execute(&mut *conn).await?;
            }
            let overrides: Option<String> = sqlx::query_scalar(
                "SELECT CAST(features AS TEXT) FROM org WHERE org_id=$1"
            ).bind(root_id).fetch_one(&mut *conn).await?;
            let plan: Option<(Option<String>, Option<DateTime<Utc>>)> = sqlx::query_as(
                "SELECT CAST(COALESCE(p.features,'{}') AS TEXT),op.current_period_start FROM org_plan op JOIN plan p ON p.plan_uuid=op.plan_uuid WHERE op.org_id=$1"
            ).bind(root_id).fetch_optional(&mut *conn).await?;
            let now = Utc::now();
            let period = crate::db::subscription::monthly_usage_start(now, plan.as_ref().and_then(|p| p.1));
            let overrides = parse_features(overrides)?;
            let plan_features = parse_features(plan.and_then(|p| p.0))?;
            let limit = run_limit(plan_features.as_ref(), overrides.as_ref())?;
            if limit >= 0 && !$lock_first {
                sqlx::query($lock).bind(root_id).execute(&mut *conn).await?;
            }
            let admitted: bool = sqlx::query_scalar(
                "SELECT e.active AND o.active AND o.lifecycle_state='active' AND r.active AND r.lifecycle_state='active' AND r.parent_org_id IS NULL FROM env e JOIN org o ON o.org_id=e.org_id JOIN org r ON r.org_id=COALESCE(o.parent_org_id,o.org_id) WHERE e.env_id=$1 AND e.org_id=$2"
            ).bind(env_id).bind(org_id).fetch_one(&mut *conn).await?;
            if !admitted {
                return Err(RunError::Suspended("Run environment or account is suspended".into()));
            }
            let prior: Option<(Uuid, Uuid)> = sqlx::query_as(
                "SELECT org_id,env_id FROM run_admission WHERE run_id=$1"
            ).bind(run_id).fetch_optional(&mut *conn).await?;
            if let Some((prior_org, prior_env)) = prior {
                return if prior_org == org_id && prior_env == env_id { Ok(()) } else { Err(RunError::ScopeDenied) };
            }
            if limit >= 0 {
                let used = crate::db::usage::$usage(conn, root_id, period).await?;
                if used >= limit {
                    return Err(RunError::QuotaExceeded("Account monthly run limit reached".into()));
                }
            }
            let inserted = sqlx::query("INSERT INTO run_admission (run_id,root_org_id,org_id,env_id,admitted_at) VALUES ($1,$2,$3,$4,$5) ON CONFLICT (run_id) DO NOTHING")
                .bind(run_id).bind(root_id).bind(org_id).bind(env_id).bind(now).execute(&mut *conn).await?.rows_affected();
            if inserted == 0 {
                // Another delivery of this run admitted it first.
                let prior: Option<(Uuid, Uuid)> = sqlx::query_as(
                    "SELECT org_id,env_id FROM run_admission WHERE run_id=$1"
                ).bind(run_id).fetch_optional(&mut *conn).await?;
                return match prior {
                    Some((prior_org, prior_env)) if prior_org == org_id && prior_env == env_id => Ok(()),
                    _ => Err(RunError::ScopeDenied),
                };
            }
            Ok(())
        }
    };
}

admission!(
    admit_postgres,
    sqlx::PgConnection,
    "SELECT org_id FROM org WHERE org_id=$1 FOR NO KEY UPDATE",
    false,
    run_usage_postgres
);
admission!(
    admit_sqlite,
    sqlx::SqliteConnection,
    "UPDATE org SET updated_at=updated_at WHERE org_id=$1",
    true,
    run_usage_sqlite
);

/// A missing or out-of-scope owner (RLS hides it as RowNotFound) is a scope
/// denial. Any other failure (pool timeout, I/O, statement timeout) is
/// infrastructure and must leave accepted work pending.
fn env_lookup_error(error: crate::db::env::EnvError) -> RunError {
    use crate::db::env::EnvError;
    match error {
        EnvError::NotFound | EnvError::Database(sqlx::Error::RowNotFound) => RunError::ScopeDenied,
        EnvError::Database(error) => RunError::Database(error),
    }
}

fn org_lookup_error(error: crate::db::org::OrgError) -> RunError {
    use crate::db::org::OrgError;
    match error {
        OrgError::Database(sqlx::Error::RowNotFound) => RunError::ScopeDenied,
        OrgError::Database(error) => RunError::Database(error),
        _ => RunError::ScopeDenied,
    }
}

fn parse_features(json: Option<String>) -> Result<Option<Value>, RunError> {
    json.map(|s| {
        serde_json::from_str::<Value>(&s)
            .map_err(|_| RunError::QuotaExceeded("Malformed account features".into()))
            .and_then(|v| {
                if v.is_object() {
                    Ok(v)
                } else {
                    Err(RunError::QuotaExceeded("Malformed account features".into()))
                }
            })
    })
    .transpose()
}

pub(crate) fn run_limit(plan: Option<&Value>, overrides: Option<&Value>) -> Result<i64, RunError> {
    if plan.is_none() && overrides.is_none() {
        return Ok(-1); // Preserve self-hosted / local unlimited defaults.
    }
    // Do not let a malformed or overflowing value turn into unlimited capacity.
    for value in [plan, overrides].into_iter().flatten() {
        if value
            .get("runs_per_month")
            .is_some_and(|limit| limit.as_i64().is_none())
        {
            return Err(RunError::QuotaExceeded("Invalid monthly run limit".into()));
        }
    }
    // With no plan, a missing limit is unlimited, not the cloud default of 0.
    Ok(Features::for_account(plan, overrides)
        .get_i64("runs_per_month")
        .unwrap_or(-1))
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn fixture(limit: i64) -> (DatabasePool, Uuid, [(Uuid, Uuid); 2]) {
        let db = crate::db::test_db().await;
        let DatabasePool::Sqlite(pool) = &db else {
            unreachable!()
        };
        let root = Uuid::now_v7();
        sqlx::query("INSERT INTO org(org_id,name,slug,org_type,features,created_by_user_id) VALUES (?,'Root',?,'organization',?,?)")
            .bind(root).bind(root.to_string()).bind(format!("{{\"runs_per_month\":{limit}}}")).bind(root).execute(pool).await.unwrap();
        let mut children = [(Uuid::nil(), Uuid::nil()); 2];
        for slot in &mut children {
            let org = Uuid::now_v7();
            let env = Uuid::now_v7();
            sqlx::query("INSERT INTO org(org_id,parent_org_id,name,slug,org_type,created_by_user_id) VALUES (?,?,'Child',?,'organization',?)")
                .bind(org).bind(root).bind(org.to_string()).bind(root).execute(pool).await.unwrap();
            sqlx::query("INSERT INTO env(env_id,org_id,name,created_by_user_id) VALUES (?,?,'development',?)")
                .bind(env).bind(org).bind(root).execute(pool).await.unwrap();
            *slot = (org, env);
        }
        (db, root, children)
    }

    #[tokio::test]
    async fn retention_prunes_only_admissions_past_the_window() {
        let (db, root, children) = fixture(-1).await;
        let DatabasePool::Sqlite(pool) = &db else {
            unreachable!()
        };
        let (org, env) = children[0];
        let old = Uuid::now_v7();
        sqlx::query("INSERT INTO run_admission(run_id,root_org_id,org_id,env_id,admitted_at) VALUES (?,?,?,?,?)")
            .bind(old).bind(root).bind(org).bind(env)
            .bind(Utc::now() - chrono::Duration::days(ADMISSION_RETENTION_DAYS + 1))
            .execute(pool).await.unwrap();
        let recent = Uuid::now_v7();
        Run::admit_execution(&db, &org, &env, &recent)
            .await
            .unwrap();
        assert_eq!(
            Run::delete_admissions_older_than(&db, ADMISSION_RETENTION_DAYS, 1)
                .await
                .unwrap(),
            1
        );
        let left: Vec<Uuid> = sqlx::query_scalar("SELECT run_id FROM run_admission")
            .fetch_all(pool)
            .await
            .unwrap();
        assert_eq!(left, vec![recent]);
    }

    /// An annual plan, or a free plan that never renews, keeps its first
    /// period start. Its monthly allowance must still reset every month.
    #[tokio::test]
    async fn monthly_run_allowance_resets_on_the_subscription_anniversary() {
        let (db, root, children) = fixture(1).await;
        let DatabasePool::Sqlite(pool) = &db else {
            unreachable!()
        };
        let plan = Uuid::now_v7();
        sqlx::query("INSERT INTO plan (plan_uuid, plan_id, plan_name, base_price_monthly_cents, base_price_annual_cents, sort_order, active, features) VALUES (?, 'annual-test', 'Annual Test', 0, 0, 1000, 1, '{\"runs_per_month\":1}')")
            .bind(plan)
            .execute(pool)
            .await
            .unwrap();
        crate::db::OrgPlan::create(&db, &root, &plan, crate::db::BillingPeriod::Annual, &root)
            .await
            .unwrap();
        sqlx::query("UPDATE org_plan SET current_period_start=? WHERE org_id=?")
            .bind(Utc::now() - chrono::Duration::days(40))
            .bind(root)
            .execute(pool)
            .await
            .unwrap();
        let (org, env) = children[0];
        sqlx::query("INSERT INTO run_admission(run_id,root_org_id,org_id,env_id,admitted_at) VALUES (?,?,?,?,?)")
            .bind(Uuid::now_v7()).bind(root).bind(org).bind(env)
            .bind(Utc::now() - chrono::Duration::days(35))
            .execute(pool).await.unwrap();
        Run::admit_execution(&db, &org, &env, &Uuid::now_v7())
            .await
            .expect("last month's run does not count against this month");
        assert!(
            Run::admit_execution(&db, &org, &env, &Uuid::now_v7())
                .await
                .is_err(),
            "this month's one run is used"
        );
    }

    #[tokio::test]
    async fn sibling_run_admissions_share_one_atomic_slot_and_replay_is_free() {
        let (db, _, children) = fixture(1).await;
        let a = Uuid::now_v7();
        let b = Uuid::now_v7();
        let (first, second) = tokio::join!(
            Run::admit_execution(&db, &children[0].0, &children[0].1, &a),
            Run::admit_execution(&db, &children[1].0, &children[1].1, &b)
        );
        assert_eq!(usize::from(first.is_ok()) + usize::from(second.is_ok()), 1);
        let (child, id) = if first.is_ok() {
            (children[0], a)
        } else {
            (children[1], b)
        };
        Run::admit_execution(&db, &child.0, &child.1, &id)
            .await
            .unwrap();
        let DatabasePool::Sqlite(pool) = &db else {
            unreachable!()
        };
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM run_admission")
                .fetch_one(pool)
                .await
                .unwrap(),
            1
        );
        assert!(matches!(
            Run::admit_execution(&db, &children[0].0, &children[1].1, &Uuid::now_v7()).await,
            Err(RunError::ScopeDenied)
        ));
    }

    #[tokio::test]
    async fn failed_handler_batch_does_not_consume_slots_and_suspension_blocks_reentry() {
        let (db, root, children) = fixture(1).await;
        let child = children[0];
        let ids = [Uuid::now_v7(), Uuid::now_v7()];
        assert!(matches!(
            Run::admit_executions(&db, &child.0, &child.1, &ids).await,
            Err(RunError::QuotaExceeded(_))
        ));
        let DatabasePool::Sqlite(pool) = &db else {
            unreachable!()
        };
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM run_admission")
                .fetch_one(pool)
                .await
                .unwrap(),
            0
        );
        Run::admit_execution(&db, &child.0, &child.1, &ids[0])
            .await
            .unwrap();
        sqlx::query("UPDATE org SET active=false WHERE org_id=?")
            .bind(root)
            .execute(pool)
            .await
            .unwrap();
        assert!(matches!(
            Run::admit_execution(&db, &child.0, &child.1, &ids[0]).await,
            Err(RunError::Suspended(_))
        ));
        sqlx::query("UPDATE org SET active=true WHERE org_id=?")
            .bind(root)
            .execute(pool)
            .await
            .unwrap();
        Run::admit_execution(&db, &child.0, &child.1, &ids[0])
            .await
            .unwrap();
        assert!(matches!(
            Run::admit_execution(&db, &child.0, &child.1, &ids[1]).await,
            Err(RunError::QuotaExceeded(_))
        ));
    }

    #[tokio::test]
    async fn transient_lookup_failures_defer_instead_of_denying_scope() {
        let (db, _, children) = fixture(1).await;
        assert!(matches!(
            Run::admit_execution(&db, &children[0].0, &Uuid::now_v7(), &Uuid::now_v7()).await,
            Err(RunError::ScopeDenied)
        ));
        let DatabasePool::Sqlite(pool) = &db else {
            unreachable!()
        };
        pool.close().await;
        assert!(matches!(
            Run::admit_execution(&db, &children[0].0, &children[0].1, &Uuid::now_v7()).await,
            Err(RunError::Database(_))
        ));
        assert!(matches!(
            env_lookup_error(crate::db::env::EnvError::Database(
                sqlx::Error::PoolTimedOut
            )),
            RunError::Database(_)
        ));
        assert!(matches!(
            org_lookup_error(crate::db::org::OrgError::Database(sqlx::Error::RowNotFound)),
            RunError::ScopeDenied
        ));
    }

    #[test]
    fn run_limits_preserve_local_unlimited_and_reject_malformed_values() {
        assert_eq!(run_limit(None, None).unwrap(), -1);
        assert_eq!(
            run_limit(
                None,
                Some(&serde_json::json!({"runs_per_month": 5_000_000_000_i64}))
            )
            .unwrap(),
            5_000_000_000
        );
        assert!(
            run_limit(
                None,
                Some(&serde_json::json!({"runs_per_month":"unlimited"}))
            )
            .is_err()
        );
    }
}

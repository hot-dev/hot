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

    /// Stable wake-up key for quota-parked work. Merely polling this key
    /// never reserves a run or cycles its queue message through admission.
    pub async fn admission_epoch(db: &DatabasePool, org_id: Uuid) -> Result<String, RunError> {
        let root = crate::db::Org::billing_owner_id_for(db, &org_id)
            .await
            .map_err(org_lookup_error)?;
        let billing = db.with_scope(DatabaseScope::Maintenance {
            operation: MaintenanceOperation::Billing,
            org_id: Some(root),
        });
        type Epoch = (
            Option<String>,
            Option<String>,
            Option<DateTime<Utc>>,
            Option<DateTime<Utc>>,
        );
        let query = "SELECT CAST(o.features AS TEXT), CASE WHEN op.org_plan_id IS NULL THEN NULL WHEN op.status_id = 1 THEN CAST(COALESCE(p.features,'{}') AS TEXT) ELSE '{}' END, op.current_period_start, op.current_period_end FROM org o LEFT JOIN org_plan op ON op.org_id=o.org_id LEFT JOIN plan p ON p.plan_uuid=op.plan_uuid WHERE o.org_id=$1";
        let (overrides, plan, start, end): Epoch = match &billing {
            DatabasePool::Postgres(pool) => {
                sqlx::query_as(query).bind(root).fetch_one(pool).await?
            }
            DatabasePool::Sqlite(pool) => sqlx::query_as(query).bind(root).fetch_one(pool).await?,
        };
        let period = crate::db::subscription::monthly_usage_start(Utc::now(), start, end);
        Ok(serde_json::json!([overrides, plan, period]).to_string())
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
        match db {
            DatabasePool::Postgres(pool) => {
                if !matches!(
                    pool.scope()?,
                    DatabaseScope::Environment {
                        org_id: o,
                        env_id: e
                    } if o == *org_id && e == *env_id
                ) {
                    return Err(RunError::ScopeDenied);
                }
                // Verify environment/org ownership through the caller's exact
                // Environment RLS scope in one query (instead of separate env +
                // org lookups). For single-run admissions (such as VM entry
                // after worker pre-admission), also probe `run_admission` under
                // the same Environment RLS policy so already-admitted runs only
                // pay for a fresh lifecycle check.
                let root_id = if let [run_id] = run_ids {
                    type EnvReplayProbe = (Option<Uuid>, bool, Option<Uuid>, Option<Uuid>);
                    let row: Option<EnvReplayProbe> = sqlx::query_as(
                        "SELECT o.parent_org_id, \
                                    (e.active AND o.active AND o.lifecycle_state='active'), \
                                    ra.org_id, ra.env_id \
                             FROM env e \
                             JOIN org o ON o.org_id=e.org_id \
                             LEFT JOIN run_admission ra ON ra.run_id=$3 \
                             WHERE e.env_id=$1 AND e.org_id=$2",
                    )
                    .bind(env_id)
                    .bind(org_id)
                    .bind(run_id)
                    .fetch_optional(pool)
                    .await?;
                    let Some((parent_org_id, local_admitted, prior_org, prior_env)) = row else {
                        return Err(RunError::ScopeDenied);
                    };
                    if !local_admitted {
                        return Err(RunError::Suspended(
                            "Run environment or account is suspended".into(),
                        ));
                    }
                    let root_id = parent_org_id.unwrap_or(*org_id);
                    if let (Some(prior_org), Some(prior_env)) = (prior_org, prior_env) {
                        if prior_org != *org_id || prior_env != *env_id {
                            return Err(RunError::ScopeDenied);
                        }
                        if parent_org_id.is_none() {
                            return Ok(());
                        }
                        // Child org: verify the billing root is still admitting
                        // without paying for full plan/lineage/ledger re-admission.
                        let billing_pool = pool.with_scope(DatabaseScope::Maintenance {
                            operation: MaintenanceOperation::Billing,
                            org_id: Some(root_id),
                        });
                        let root_admitted: Option<bool> = sqlx::query_scalar(
                            "SELECT r.active AND r.lifecycle_state='active' AND r.parent_org_id IS NULL \
                             FROM org r WHERE r.org_id=$1",
                        )
                        .bind(root_id)
                        .fetch_optional(&billing_pool)
                        .await?;
                        return match root_admitted {
                            Some(true) => Ok(()),
                            Some(false) => Err(RunError::Suspended(
                                "Run environment or account is suspended".into(),
                            )),
                            None => Err(RunError::ScopeDenied),
                        };
                    }
                    root_id
                } else {
                    let row: Option<(Option<Uuid>, bool)> = sqlx::query_as(
                        "SELECT o.parent_org_id, \
                                (e.active AND o.active AND o.lifecycle_state='active') \
                         FROM env e \
                         JOIN org o ON o.org_id=e.org_id \
                         WHERE e.env_id=$1 AND e.org_id=$2",
                    )
                    .bind(env_id)
                    .bind(org_id)
                    .fetch_optional(pool)
                    .await?;
                    let Some((parent_org_id, local_admitted)) = row else {
                        return Err(RunError::ScopeDenied);
                    };
                    if run_ids.is_empty() {
                        return Ok(());
                    }
                    if !local_admitted {
                        return Err(RunError::Suspended(
                            "Run environment or account is suspended".into(),
                        ));
                    }
                    parent_org_id.unwrap_or(*org_id)
                };
                let billing_pool = pool.with_scope(DatabaseScope::Maintenance {
                    operation: MaintenanceOperation::Billing,
                    org_id: Some(root_id),
                });
                let mut tx = billing_pool.begin().await?;
                admit_executions_postgres(&mut tx, root_id, *org_id, *env_id, run_ids).await?;
                tx.commit().await?;
            }
            DatabasePool::Sqlite(_) => {
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
                let DatabasePool::Sqlite(pool) = &billing else {
                    unreachable!()
                };
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
// PostgreSQL locks with NO KEY UPDATE, which doesn't block the foreign-key
// checks of inserts that reference the org, and only when the account has a
// run limit: an unlimited account has no count to protect, and a racing
// delivery of the same run is settled by the ledger's primary key. SQLite must
// take its writer lock before reading, so it always locks first.

/// The task an infrastructure retry ultimately retries: walk parent_task_id
/// while the current row is an infrastructure retry, within the run's
/// environment. A run that isn't a task, or isn't a retry, maps to itself.
const LOGICAL_RUN: &str = "WITH RECURSIVE lineage(task_id,parent_task_id,infra_retry_count,depth) AS (SELECT task_id,parent_task_id,infra_retry_count,0 FROM task WHERE task_id=$1 AND env_id=$2 UNION ALL SELECT p.task_id,p.parent_task_id,p.infra_retry_count,l.depth+1 FROM task p JOIN lineage l ON p.task_id=l.parent_task_id WHERE l.infra_retry_count>0 AND l.depth<100 AND p.env_id=$2) SELECT task_id FROM lineage ORDER BY depth DESC LIMIT 1";

const PG_ACCOUNT_ADMISSION_STATE: &str = "SELECT \
    CAST(r.features AS TEXT), \
    CASE WHEN op.org_plan_id IS NULL THEN NULL \
         WHEN op.status_id = 1 THEN CAST(COALESCE(p.features,'{}') AS TEXT) \
         ELSE '{}' END, \
    op.current_period_start, \
    op.current_period_end, \
    (e.active AND o.active AND o.lifecycle_state='active' \
     AND r.active AND r.lifecycle_state='active' AND r.parent_org_id IS NULL) \
    FROM env e \
    JOIN org o ON o.org_id=e.org_id \
    JOIN org r ON r.org_id=COALESCE(o.parent_org_id,o.org_id) \
    LEFT JOIN org_plan op ON op.org_id=r.org_id \
    LEFT JOIN plan p ON p.plan_uuid=op.plan_uuid \
    WHERE e.env_id=$1 AND e.org_id=$2 AND r.org_id=$3";

type PgAccountAdmissionRow = (
    Option<String>,
    Option<String>,
    Option<DateTime<Utc>>,
    Option<DateTime<Utc>>,
    bool,
);

/// Fetch account plan/overrides/lifecycle once for a batch of run admissions,
/// locking the account root (and re-reading state under the lock) only when the
/// account enforces a monthly run limit.
async fn admit_executions_postgres(
    conn: &mut sqlx::PgConnection,
    root_id: Uuid,
    org_id: Uuid,
    env_id: Uuid,
    run_ids: &[Uuid],
) -> Result<(), RunError> {
    let now = Utc::now();
    let row: Option<PgAccountAdmissionRow> = sqlx::query_as(PG_ACCOUNT_ADMISSION_STATE)
        .bind(env_id)
        .bind(org_id)
        .bind(root_id)
        .fetch_optional(&mut *conn)
        .await?;
    let Some((overrides, plan_features, period_start, period_end, mut admitted)) = row else {
        return Err(RunError::ScopeDenied);
    };
    let mut period = crate::db::subscription::monthly_usage_start(now, period_start, period_end);
    let mut limit = run_limit(
        parse_features(plan_features)?.as_ref(),
        parse_features(overrides)?.as_ref(),
    )?;
    if limit >= 0 {
        sqlx::query("SELECT org_id FROM org WHERE org_id=$1 FOR NO KEY UPDATE")
            .bind(root_id)
            .execute(&mut *conn)
            .await?;
        let locked: Option<PgAccountAdmissionRow> = sqlx::query_as(PG_ACCOUNT_ADMISSION_STATE)
            .bind(env_id)
            .bind(org_id)
            .bind(root_id)
            .fetch_optional(&mut *conn)
            .await?;
        let Some((overrides, plan_features, period_start, period_end, locked_admitted)) = locked
        else {
            return Err(RunError::ScopeDenied);
        };
        admitted = locked_admitted;
        period = crate::db::subscription::monthly_usage_start(now, period_start, period_end);
        limit = run_limit(
            parse_features(plan_features)?.as_ref(),
            parse_features(overrides)?.as_ref(),
        )?;
    }
    if !admitted {
        return Err(RunError::Suspended(
            "Run environment or account is suspended".into(),
        ));
    }
    for run_id in run_ids {
        admit_run_with_account_postgres(conn, root_id, org_id, env_id, *run_id, limit, period, now)
            .await?;
    }
    Ok(())
}

/// Reserve a single run on PostgreSQL inside an active Billing-scoped
/// transaction whose caller has already verified fresh account lifecycle,
/// resolved `limit`/`period`, and taken `org FOR NO KEY UPDATE` when
/// `limit >= 0`.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn admit_run_with_account_postgres(
    conn: &mut sqlx::PgConnection,
    root_id: Uuid,
    org_id: Uuid,
    env_id: Uuid,
    run_id: Uuid,
    limit: i64,
    period: DateTime<Utc>,
    now: DateTime<Utc>,
) -> Result<(), RunError> {
    if limit < 0 {
        // Unlimited account: resolve infra-retry lineage and upsert the
        // admission ledger row in a single statement.
        let (logical_id, owner_org, owner_env): (Uuid, Option<Uuid>, Option<Uuid>) =
            sqlx::query_as(
                "WITH RECURSIVE lineage(task_id,parent_task_id,infra_retry_count,depth) AS (\
                     SELECT task_id,parent_task_id,infra_retry_count,0 FROM task \
                     WHERE task_id=$1 AND env_id=$2 \
                     UNION ALL \
                     SELECT p.task_id,p.parent_task_id,p.infra_retry_count,l.depth+1 \
                     FROM task p JOIN lineage l ON p.task_id=l.parent_task_id \
                     WHERE l.infra_retry_count>0 AND l.depth<100 AND p.env_id=$2\
                 ), \
                 resolved AS (\
                     SELECT COALESCE((SELECT task_id FROM lineage ORDER BY depth DESC LIMIT 1),$1) AS logical_id\
                 ), \
                 inserted AS (\
                     INSERT INTO run_admission (run_id,root_org_id,org_id,env_id,admitted_at) \
                     SELECT logical_id,$3,$4,$2,$5 FROM resolved \
                     ON CONFLICT (run_id) DO NOTHING \
                     RETURNING org_id,env_id\
                 ) \
                 SELECT r.logical_id, COALESCE(i.org_id,ra.org_id), COALESCE(i.env_id,ra.env_id) \
                 FROM resolved r \
                 LEFT JOIN inserted i ON true \
                 LEFT JOIN run_admission ra ON ra.run_id=r.logical_id",
            )
            .bind(run_id)
            .bind(env_id)
            .bind(root_id)
            .bind(org_id)
            .bind(now)
            .fetch_one(&mut *conn)
            .await?;
        if let (Some(owner_org), Some(owner_env)) = (owner_org, owner_env) {
            return if owner_org == org_id && owner_env == env_id {
                Ok(())
            } else {
                Err(RunError::ScopeDenied)
            };
        }
        // Rare concurrent race: another transaction committed `logical_id`
        // after our CTE snapshot started (or another root account owns it and
        // RLS hid the row). Re-read with a fresh READ COMMITTED snapshot.
        let prior: Option<(Uuid, Uuid)> =
            sqlx::query_as("SELECT org_id,env_id FROM run_admission WHERE run_id=$1")
                .bind(logical_id)
                .fetch_optional(&mut *conn)
                .await?;
        return match prior {
            Some((prior_org, prior_env)) if prior_org == org_id && prior_env == env_id => Ok(()),
            _ => Err(RunError::ScopeDenied),
        };
    }

    // Limited account: resolve lineage and check existing ledger row together.
    let (logical_id, prior_org, prior_env): (Uuid, Option<Uuid>, Option<Uuid>) = sqlx::query_as(
        "WITH RECURSIVE lineage(task_id,parent_task_id,infra_retry_count,depth) AS (\
             SELECT task_id,parent_task_id,infra_retry_count,0 FROM task \
             WHERE task_id=$1 AND env_id=$2 \
             UNION ALL \
             SELECT p.task_id,p.parent_task_id,p.infra_retry_count,l.depth+1 \
             FROM task p JOIN lineage l ON p.task_id=l.parent_task_id \
             WHERE l.infra_retry_count>0 AND l.depth<100 AND p.env_id=$2\
         ), \
         resolved AS (\
             SELECT COALESCE((SELECT task_id FROM lineage ORDER BY depth DESC LIMIT 1),$1) AS logical_id\
         ) \
         SELECT r.logical_id, ra.org_id, ra.env_id \
         FROM resolved r \
         LEFT JOIN run_admission ra ON ra.run_id=r.logical_id",
    )
    .bind(run_id)
    .bind(env_id)
    .fetch_one(&mut *conn)
    .await?;
    if let (Some(prior_org), Some(prior_env)) = (prior_org, prior_env) {
        return if prior_org == org_id && prior_env == env_id {
            Ok(())
        } else {
            Err(RunError::ScopeDenied)
        };
    }
    let used = crate::db::usage::run_usage_postgres(conn, root_id, period).await?;
    if used >= limit {
        return Err(RunError::QuotaExceeded(
            "Account monthly run limit reached".into(),
        ));
    }
    let inserted = sqlx::query(
        "INSERT INTO run_admission (run_id,root_org_id,org_id,env_id,admitted_at) VALUES ($1,$2,$3,$4,$5) ON CONFLICT (run_id) DO NOTHING",
    )
    .bind(logical_id)
    .bind(root_id)
    .bind(org_id)
    .bind(env_id)
    .bind(now)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    if inserted == 0 {
        let prior: Option<(Uuid, Uuid)> =
            sqlx::query_as("SELECT org_id,env_id FROM run_admission WHERE run_id=$1")
                .bind(logical_id)
                .fetch_optional(&mut *conn)
                .await?;
        return match prior {
            Some((prior_org, prior_env)) if prior_org == org_id && prior_env == env_id => Ok(()),
            _ => Err(RunError::ScopeDenied),
        };
    }
    Ok(())
}

pub(crate) async fn admit_sqlite(
    conn: &mut sqlx::SqliteConnection,
    org_id: Uuid,
    env_id: Uuid,
    run_id: Uuid,
) -> Result<(), RunError> {
    let root_id: Option<Uuid> = sqlx::query_scalar(
        "SELECT COALESCE(o.parent_org_id,o.org_id) FROM env e JOIN org o ON o.org_id=e.org_id WHERE e.env_id=$1 AND e.org_id=$2",
    )
    .bind(env_id)
    .bind(org_id)
    .fetch_optional(&mut *conn)
    .await?;
    let root_id = root_id.ok_or(RunError::ScopeDenied)?;
    sqlx::query("UPDATE org SET updated_at=updated_at WHERE org_id=$1")
        .bind(root_id)
        .execute(&mut *conn)
        .await?;
    let overrides: Option<String> =
        sqlx::query_scalar("SELECT CAST(features AS TEXT) FROM org WHERE org_id=$1")
            .bind(root_id)
            .fetch_one(&mut *conn)
            .await?;
    type SqlitePlanRow = (Option<String>, Option<DateTime<Utc>>, Option<DateTime<Utc>>);
    let plan: Option<SqlitePlanRow> = sqlx::query_as(
        "SELECT CASE WHEN op.status_id = 1 THEN CAST(COALESCE(p.features,'{}') AS TEXT) ELSE '{}' END,op.current_period_start,op.current_period_end FROM org_plan op JOIN plan p ON p.plan_uuid=op.plan_uuid WHERE op.org_id=$1",
    )
    .bind(root_id)
    .fetch_optional(&mut *conn)
    .await?;
    let now = Utc::now();
    let period = crate::db::subscription::monthly_usage_start(
        now,
        plan.as_ref().and_then(|p| p.1),
        plan.as_ref().and_then(|p| p.2),
    );
    let overrides = parse_features(overrides)?;
    let plan_features = parse_features(plan.and_then(|p| p.0))?;
    let limit = run_limit(plan_features.as_ref(), overrides.as_ref())?;
    let admitted: bool = sqlx::query_scalar(
        "SELECT e.active AND o.active AND o.lifecycle_state='active' AND r.active AND r.lifecycle_state='active' AND r.parent_org_id IS NULL FROM env e JOIN org o ON o.org_id=e.org_id JOIN org r ON r.org_id=CASE WHEN o.parent_org_id IS NULL THEN o.org_id ELSE o.parent_org_id END WHERE e.env_id=$1 AND e.org_id=$2",
    )
    .bind(env_id)
    .bind(org_id)
    .fetch_one(&mut *conn)
    .await?;
    if !admitted {
        return Err(RunError::Suspended(
            "Run environment or account is suspended".into(),
        ));
    }
    // An infrastructure retry is the run it retries, so it reuses that
    // admission; a user-requested retry (infra_retry_count 0) is new.
    let logical: Option<Uuid> = sqlx::query_scalar(LOGICAL_RUN)
        .bind(run_id)
        .bind(env_id)
        .fetch_optional(&mut *conn)
        .await?;
    let run_id = logical.unwrap_or(run_id);
    let prior: Option<(Uuid, Uuid)> =
        sqlx::query_as("SELECT org_id,env_id FROM run_admission WHERE run_id=$1")
            .bind(run_id)
            .fetch_optional(&mut *conn)
            .await?;
    if let Some((prior_org, prior_env)) = prior {
        return if prior_org == org_id && prior_env == env_id {
            Ok(())
        } else {
            Err(RunError::ScopeDenied)
        };
    }
    if limit >= 0 {
        let used = crate::db::usage::run_usage_sqlite(conn, root_id, period).await?;
        if used >= limit {
            return Err(RunError::QuotaExceeded(
                "Account monthly run limit reached".into(),
            ));
        }
    }
    let inserted = sqlx::query(
        "INSERT INTO run_admission (run_id,root_org_id,org_id,env_id,admitted_at) VALUES ($1,$2,$3,$4,$5) ON CONFLICT (run_id) DO NOTHING",
    )
    .bind(run_id)
    .bind(root_id)
    .bind(org_id)
    .bind(env_id)
    .bind(now)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    if inserted == 0 {
        // Another delivery of this run admitted it first.
        let prior: Option<(Uuid, Uuid)> =
            sqlx::query_as("SELECT org_id,env_id FROM run_admission WHERE run_id=$1")
                .bind(run_id)
                .fetch_optional(&mut *conn)
                .await?;
        return match prior {
            Some((prior_org, prior_env)) if prior_org == org_id && prior_env == env_id => Ok(()),
            _ => Err(RunError::ScopeDenied),
        };
    }
    Ok(())
}

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

    #[tokio::test]
    async fn infrastructure_retries_reuse_the_original_tasks_admission() {
        let (db, _, children) = fixture(1).await;
        let DatabasePool::Sqlite(pool) = &db else {
            unreachable!()
        };
        let (org, env) = children[0];
        let original = Uuid::now_v7();
        let retry = Uuid::now_v7();
        for (task, parent, retries) in [(original, None, 0), (retry, Some(original), 1)] {
            sqlx::query("INSERT INTO task(task_id,env_id,stream_id,build_id,function_name,task_type,parent_task_id,infra_retry_count) VALUES(?,?,?,?,'::app/work','code',?,?)")
                .bind(task).bind(env).bind(Uuid::now_v7()).bind(Uuid::now_v7())
                .bind(parent).bind(retries).execute(pool).await.unwrap();
        }
        Run::admit_execution(&db, &org, &env, &original)
            .await
            .unwrap();
        // The month's one run is the original task, and its retry is that run.
        Run::admit_execution(&db, &org, &env, &retry)
            .await
            .expect("an infrastructure retry reuses its task's admission");
        assert!(
            Run::admit_execution(&db, &org, &env, &Uuid::now_v7())
                .await
                .is_err()
        );
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
        sqlx::query("UPDATE org_plan SET status_id=1, current_period_start=? WHERE org_id=?")
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

    #[tokio::test]
    async fn pending_and_inactive_plans_deny_run_admission_until_activated() {
        let (db, root, children) = fixture(1).await;
        let DatabasePool::Sqlite(pool) = &db else {
            unreachable!()
        };
        // Clear root org feature overrides so run quota comes strictly from org_plan.
        sqlx::query("UPDATE org SET features = NULL WHERE org_id = ?")
            .bind(root)
            .execute(pool)
            .await
            .unwrap();

        let plan = Uuid::now_v7();
        sqlx::query("INSERT INTO plan (plan_uuid, plan_id, plan_name, base_price_monthly_cents, base_price_annual_cents, sort_order, active, features) VALUES (?, 'pending-test', 'Pending Test', 2500, 25000, 1000, 1, '{\"runs_per_month\":5}')")
            .bind(plan)
            .execute(pool)
            .await
            .unwrap();

        let org_plan =
            crate::db::OrgPlan::create(&db, &root, &plan, crate::db::BillingPeriod::Monthly, &root)
                .await
                .unwrap();

        let (org, env) = children[0];
        // Pending plan must deny run admission (cloud default 0 runs).
        assert!(matches!(
            Run::admit_execution(&db, &org, &env, &Uuid::now_v7()).await,
            Err(RunError::QuotaExceeded(_))
        ));

        let epoch_pending = Run::admission_epoch(&db, root).await.unwrap();

        // Activating the plan changes the admission epoch and admits runs.
        crate::db::OrgPlan::update_status(
            &db,
            &org_plan.org_plan_id,
            crate::db::OrgPlanStatus::Active,
            Some(&root),
        )
        .await
        .unwrap();
        let epoch_active = Run::admission_epoch(&db, root).await.unwrap();
        assert_ne!(epoch_pending, epoch_active);

        Run::admit_execution(&db, &org, &env, &Uuid::now_v7())
            .await
            .expect("active plan admits runs");

        // Deactivating the plan changes the admission epoch again and blocks further runs.
        crate::db::OrgPlan::cancel_immediately(&db, &org_plan.org_plan_id, &root)
            .await
            .unwrap();
        let epoch_inactive = Run::admission_epoch(&db, root).await.unwrap();
        assert_ne!(epoch_active, epoch_inactive);

        assert!(matches!(
            Run::admit_execution(&db, &org, &env, &Uuid::now_v7()).await,
            Err(RunError::QuotaExceeded(_))
        ));
    }
}

//! Maintained account counters. Admission holds the account lock; row triggers
//! publish usage in the same transaction as the work they count.
use chrono::{DateTime, Utc};
use sqlx::{PgConnection, SqliteConnection};
use uuid::Uuid;

use super::{DatabasePool, DatabaseScope, MaintenanceOperation};

type TaskTotals = (i64, i64, i64, i64, i64);
type TaskDrift = (Uuid, Uuid, Option<DateTime<Utc>>, [i64; 5]);
const TASK_TOTALS: &str = "SELECT CAST(COALESCE(SUM(u.active_containers),0) AS BIGINT),CAST(COALESCE(SUM(u.reserved_compute_units),0) AS BIGINT),CAST(COALESCE(SUM(u.compute_units),0) AS BIGINT),CAST(COALESCE(SUM(u.reserved_duration_ms),0) AS BIGINT),CAST(COALESCE(SUM(u.duration_ms),0) AS BIGINT) FROM usage_task u JOIN org o ON o.org_id=u.org_id WHERE o.org_id=$1 OR o.parent_org_id=$1";

/// Recount one environment's usage for `period`. Callers lock the counter row
/// first; this runs as a new statement, so its snapshot includes every writer
/// that held the row. The timestamp predicate matches `usage_at` and uses
/// `task_env_usage_at_idx`, so the cost follows the period, not history.
const PERIOD_RECOUNT: &str = "WITH totals AS (SELECT t.env_id,SUM(f.compute_units) AS cu,SUM(f.duration_ms) AS ms FROM task t CROSS JOIN LATERAL hot.task_usage(t) f WHERE t.env_id=ANY($1) AND COALESCE(t.stop_time,t.start_time,t.created_at) >= $2 GROUP BY t.env_id) UPDATE usage_task u SET compute_units=COALESCE((SELECT cu FROM totals WHERE env_id=u.env_id),0),duration_ms=COALESCE((SELECT ms FROM totals WHERE env_id=u.env_id),0),period_start=$2 WHERE u.env_id=ANY($1)";

/// Initialize counters for a new billing period before admission takes the
/// account lock. Each environment commits separately under its own scope, so
/// a rejected claim cannot discard the work and make every redelivery rescan
/// history while holding the lock. Admission's in-transaction initialization
/// only remains for a period that changes in between.
pub(crate) async fn prepare_task_period(
    db: &DatabasePool,
    root: Uuid,
    period: DateTime<Utc>,
) -> Result<(), sqlx::Error> {
    let DatabasePool::Postgres(pool) = db else {
        return Ok(());
    };
    let billing = pool.with_scope(DatabaseScope::Maintenance {
        operation: MaintenanceOperation::Billing,
        org_id: Some(root),
    });
    let stale: Vec<(Uuid, Uuid)> = sqlx::query_as("SELECT e.env_id,e.org_id FROM env e JOIN org o ON o.org_id=e.org_id LEFT JOIN usage_task u ON u.env_id=e.env_id WHERE (o.org_id=$1 OR o.parent_org_id=$1) AND u.period_start IS DISTINCT FROM $2 ORDER BY e.env_id")
        .bind(root).bind(period).fetch_all(&billing).await?;
    for (env_id, org_id) in stale {
        let mut tx = pool
            .with_scope(DatabaseScope::Environment { org_id, env_id })
            .begin()
            .await?;
        sqlx::query(
            "INSERT INTO usage_task(env_id,org_id) VALUES($1,$2) ON CONFLICT(env_id) DO NOTHING",
        )
        .bind(env_id)
        .bind(org_id)
        .execute(&mut *tx)
        .await?;
        let locked: Option<Uuid> = sqlx::query_scalar("SELECT env_id FROM usage_task WHERE env_id=$1 AND period_start IS DISTINCT FROM $2 FOR UPDATE")
            .bind(env_id).bind(period).fetch_optional(&mut *tx).await?;
        if locked.is_some() {
            sqlx::query(PERIOD_RECOUNT)
                .bind(vec![env_id])
                .bind(period)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
    }
    Ok(())
}

/// Called under the root lock. Lock stale counter rows before taking the fresh
/// statement snapshot that initializes a new billing period. Task writers after
/// that snapshot wait in their trigger and then apply their own deltas.
pub(crate) async fn task_usage_postgres(
    conn: &mut PgConnection,
    root: Uuid,
    period: DateTime<Utc>,
) -> Result<TaskTotals, sqlx::Error> {
    sqlx::query("INSERT INTO usage_task(env_id,org_id) SELECT e.env_id,e.org_id FROM env e JOIN org o ON o.org_id=e.org_id WHERE o.org_id=$1 OR o.parent_org_id=$1 ORDER BY e.env_id ON CONFLICT(env_id) DO NOTHING")
        .bind(root).execute(&mut *conn).await?;
    let stale: Vec<Uuid> = sqlx::query_scalar("SELECT u.env_id FROM usage_task u JOIN org o ON o.org_id=u.org_id WHERE (o.org_id=$1 OR o.parent_org_id=$1) AND u.period_start IS DISTINCT FROM $2 ORDER BY u.env_id FOR UPDATE OF u")
        .bind(root).bind(period).fetch_all(&mut *conn).await?;
    if !stale.is_empty() {
        sqlx::query(PERIOD_RECOUNT)
            .bind(stale)
            .bind(period)
            .execute(&mut *conn)
            .await?;
    }
    sqlx::query_as(TASK_TOTALS).bind(root).fetch_one(conn).await
}

pub(crate) async fn task_usage_sqlite(
    conn: &mut SqliteConnection,
    root: Uuid,
    period: DateTime<Utc>,
) -> Result<TaskTotals, sqlx::Error> {
    // The caller already owns SQLite's writer lock.
    sqlx::query("INSERT INTO usage_task(env_id,org_id) SELECT e.env_id,e.org_id FROM env e JOIN org o ON o.org_id=e.org_id WHERE o.org_id=$1 OR o.parent_org_id=$1 ON CONFLICT(env_id) DO NOTHING")
        .bind(root).execute(&mut *conn).await?;
    sqlx::query("UPDATE usage_task SET compute_units=COALESCE((SELECT SUM(f.compute_units) FROM task_usage f WHERE f.env_id=usage_task.env_id AND julianday(f.usage_at)>=julianday($2)),0),duration_ms=COALESCE((SELECT SUM(f.duration_ms) FROM task_usage f WHERE f.env_id=usage_task.env_id AND julianday(f.usage_at)>=julianday($2)),0),period_start=$2 WHERE org_id IN (SELECT org_id FROM org WHERE org_id=$1 OR parent_org_id=$1) AND period_start IS NOT $2")
        .bind(root).bind(period).execute(&mut *conn).await?;
    sqlx::query_as(TASK_TOTALS).bind(root).fetch_one(conn).await
}

// A run counter represents COUNT(admitted_at >= period), including backdated
// period changes. Initialization and insertion share the existing root lock.
macro_rules! run_usage {
    ($name:ident,$connection:ty,$since:literal) => {
        pub(crate) async fn $name(conn: &mut $connection, root: Uuid, period: DateTime<Utc>) -> Result<i64,sqlx::Error> {
            let prior: Option<i64> = sqlx::query_scalar("SELECT admitted FROM usage_run WHERE root_org_id=$1 AND period_start=$2")
                .bind(root).bind(period).fetch_optional(&mut *conn).await?;
            if let Some(used)=prior { return Ok(used); }
            sqlx::query("DELETE FROM usage_run WHERE root_org_id=$1").bind(root).execute(&mut *conn).await?;
            sqlx::query(concat!("INSERT INTO usage_run(root_org_id,period_start,admitted) SELECT $1,$2,COUNT(*) FROM run_admission WHERE root_org_id=$1 AND ",$since))
                .bind(root).bind(period).execute(&mut *conn).await?;
            sqlx::query_scalar("SELECT admitted FROM usage_run WHERE root_org_id=$1 AND period_start=$2")
                .bind(root).bind(period).fetch_one(conn).await
        }
    };
}
run_usage!(run_usage_postgres, PgConnection, "admitted_at >= $2");
run_usage!(
    run_usage_sqlite,
    SqliteConnection,
    "julianday(admitted_at) >= julianday($2)"
);

#[derive(Debug, Default)]
struct Snapshot {
    storage: Vec<(Uuid, Uuid, i64, i64)>,
    tasks: Vec<TaskDrift>,
    runs: Vec<(DateTime<Utc>, i64)>,
}

macro_rules! snapshot {
    ($name:ident,$connection:ty,$task_from:literal,$task_env:literal,$since:literal,$run_since:literal) => {
        async fn $name(conn: &mut $connection,root:Uuid) -> Result<Snapshot,sqlx::Error> {
            let root_exists:Option<bool>=sqlx::query_scalar("SELECT parent_org_id IS NULL FROM org WHERE org_id=$1").bind(root).fetch_optional(&mut *conn).await?;
            if root_exists!=Some(true) { return Err(sqlx::Error::Protocol("Usage reconciliation requires a root account".into())); }
            let mut report=Snapshot::default();
            let envs:Vec<(Uuid,Uuid)>=sqlx::query_as("SELECT env_id,org_id FROM env WHERE org_id IN (SELECT org_id FROM org WHERE org_id=$1 OR parent_org_id=$1) UNION SELECT env_id,org_id FROM usage_storage WHERE org_id IN (SELECT org_id FROM org WHERE org_id=$1 OR parent_org_id=$1) UNION SELECT env_id,org_id FROM store_map_entry WHERE org_id IN (SELECT org_id FROM org WHERE org_id=$1 OR parent_org_id=$1) UNION SELECT env_id,org_id FROM usage_task WHERE org_id IN (SELECT org_id FROM org WHERE org_id=$1 OR parent_org_id=$1)")
                .bind(root).fetch_all(&mut *conn).await?;
            for (env,org) in envs {
                let actual:(i64,i64)=sqlx::query_as("SELECT CAST(COALESCE(SUM(store_bytes),0) AS BIGINT),CAST(COALESCE(SUM(call_bytes),0) AS BIGINT) FROM usage_storage WHERE env_id=$1")
                    .bind(env).fetch_one(&mut *conn).await?;
                let store:i64=sqlx::query_scalar("SELECT CAST(COALESCE(SUM(CASE WHEN size<0 THEN 0 ELSE size END),0) AS BIGINT) FROM store_map_entry WHERE env_id=$1")
                    .bind(env).fetch_one(&mut *conn).await?;
                let call:i64=sqlx::query_scalar("SELECT CAST(COALESCE(SUM(CASE WHEN size<0 THEN 0 ELSE size END),0) AS BIGINT) FROM call WHERE env_id=$1")
                    .bind(env).fetch_one(&mut *conn).await?;
                let ds=store.checked_sub(actual.0).ok_or_else(overflow)?;
                let dc=call.checked_sub(actual.1).ok_or_else(overflow)?;
                if ds!=0 || dc!=0 {report.storage.push((env,org,ds,dc));}
                let row:Option<(Option<DateTime<Utc>>,i64,i64,i64,i64,i64)>=sqlx::query_as("SELECT period_start,active_containers,reserved_compute_units,reserved_duration_ms,compute_units,duration_ms FROM usage_task WHERE env_id=$1")
                    .bind(env).fetch_optional(&mut *conn).await?;
                let (period,a,b,c,d,e)=row.unwrap_or((None,0,0,0,0,0));
                let truth:TaskTotals=sqlx::query_as(concat!("SELECT CAST(COALESCE(SUM(f.active_containers),0) AS BIGINT),CAST(COALESCE(SUM(f.reserved_compute_units),0) AS BIGINT),CAST(COALESCE(SUM(f.reserved_duration_ms),0) AS BIGINT),CAST(COALESCE(SUM(CASE WHEN ",$since," THEN f.compute_units ELSE 0 END),0) AS BIGINT),CAST(COALESCE(SUM(CASE WHEN ",$since," THEN f.duration_ms ELSE 0 END),0) AS BIGINT) FROM ",$task_from," WHERE ",$task_env,"=$1"))
                    .bind(env).bind(period).fetch_one(&mut *conn).await?;
                let mut delta=[0;5];
                for (i,(expected,actual)) in [truth.0,truth.1,truth.2,truth.3,truth.4].into_iter().zip([a,b,c,d,e]).enumerate() {
                    delta[i]=expected.checked_sub(actual).ok_or_else(overflow)?;
                }
                if delta.iter().any(|d|*d!=0) { report.tasks.push((env,org,period,delta)); }
            }
            let rows:Vec<(DateTime<Utc>,i64)>=sqlx::query_as("SELECT period_start,admitted FROM usage_run WHERE root_org_id=$1")
                .bind(root).fetch_all(&mut *conn).await?;
            for (period,count) in rows {
                let truth:i64=sqlx::query_scalar(concat!("SELECT COUNT(*) FROM run_admission WHERE root_org_id=$1 AND ", $run_since))
                    .bind(root).bind(period).fetch_one(&mut *conn).await?;
                let delta=truth.checked_sub(count).ok_or_else(overflow)?;
                if delta!=0 { report.runs.push((period,delta)); }
            }
            Ok(report)
        }
    };
}
// Run and task timestamp names differ, so use two explicit snapshot expansions.
snapshot!(
    snapshot_postgres,
    PgConnection,
    "task t CROSS JOIN LATERAL hot.task_usage(t) f",
    "t.env_id",
    "f.usage_at >= $2",
    "admitted_at >= $2"
);
snapshot!(
    snapshot_sqlite,
    SqliteConnection,
    "task_usage f",
    "f.env_id",
    "julianday(f.usage_at) >= julianday($2)",
    "julianday(admitted_at) >= julianday($2)"
);

fn overflow() -> sqlx::Error {
    sqlx::Error::Protocol("Usage reconciliation delta overflow".into())
}

macro_rules! repair {
    ($name:ident,$connection:ty,$same_period:literal) => {
        async fn $name(conn:&mut $connection,root:Uuid,snapshot:&Snapshot) -> Result<(),sqlx::Error> {
            for &(env,org,store,call) in &snapshot.storage {
                sqlx::query("INSERT INTO usage_storage(env_id,org_id,slot,store_bytes,call_bytes) VALUES($1,$2,0,$3,$4) ON CONFLICT(env_id,slot) DO UPDATE SET store_bytes=usage_storage.store_bytes+excluded.store_bytes,call_bytes=usage_storage.call_bytes+excluded.call_bytes")
                    .bind(env).bind(org).bind(store).bind(call).execute(&mut *conn).await?;
                tracing::warn!(%root,%env,store_delta=store,call_delta=call,"Repaired storage usage drift");
            }
            for &(env,org,period,d) in &snapshot.tasks {
                let changed=sqlx::query(concat!("INSERT INTO usage_task(env_id,org_id,period_start,active_containers,reserved_compute_units,reserved_duration_ms,compute_units,duration_ms) VALUES($1,$2,$3,$4,$5,$6,$7,$8) ON CONFLICT(env_id) DO UPDATE SET active_containers=usage_task.active_containers+excluded.active_containers,reserved_compute_units=usage_task.reserved_compute_units+excluded.reserved_compute_units,reserved_duration_ms=usage_task.reserved_duration_ms+excluded.reserved_duration_ms,compute_units=usage_task.compute_units+excluded.compute_units,duration_ms=usage_task.duration_ms+excluded.duration_ms WHERE ",$same_period))
                    .bind(env).bind(org).bind(period).bind(d[0]).bind(d[1]).bind(d[2]).bind(d[3]).bind(d[4]).execute(&mut *conn).await?.rows_affected();
                tracing::warn!(%root,%env,?period,delta=?d,changed,"Reconciled task usage drift");
            }
            for &(period,delta) in &snapshot.runs {
                // A claim may have pruned an old period since the snapshot.
                // Never resurrect that row with a partial repair value.
                sqlx::query("UPDATE usage_run SET admitted=admitted+$3 WHERE root_org_id=$1 AND period_start=$2")
                    .bind(root).bind(period).bind(delta).execute(&mut *conn).await?;
                tracing::warn!(%root,?period,delta,"Reconciled run usage drift");
            }
            Ok(())
        }
    };
}
repair!(
    repair_postgres,
    PgConnection,
    "usage_task.period_start IS NOT DISTINCT FROM excluded.period_start"
);
repair!(
    repair_sqlite,
    SqliteConnection,
    "usage_task.period_start IS excluded.period_start"
);

/// Number of counter rows whose consistent snapshot differed from source data.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct UsageDrift {
    pub storage_rows: usize,
    pub task_rows: usize,
    pub run_rows: usize,
}
impl UsageDrift {
    pub fn is_empty(&self) -> bool {
        self.storage_rows == 0 && self.task_rows == 0 && self.run_rows == 0
    }
}

static SQLITE_RECONCILIATION: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Trusted maintenance only. Compare counters and source rows in one stable
/// snapshot, then apply relative corrections without locking the account row.
/// A separate advisory session serializes repairs, not ordinary writers. This
/// prevents two reconcilers from applying the same snapshot difference twice.
/// None means another reconciler owns that account; a later tick retries it.
pub async fn reconcile_account(
    db: &DatabasePool,
    root: Uuid,
) -> Result<Option<UsageDrift>, sqlx::Error> {
    if let DatabasePool::Postgres(pool) = db {
        match pool.scope()? {
            DatabaseScope::Maintenance {
                operation: MaintenanceOperation::Scheduler,
                ..
            } => {}
            DatabaseScope::Maintenance {
                operation: MaintenanceOperation::Billing,
                org_id: Some(id),
            } if id == root => {}
            _ => {
                return Err(sqlx::Error::Protocol(
                    "Usage reconciliation requires maintenance authority".into(),
                ));
            }
        }
    }
    let _local_guard = if matches!(db, DatabasePool::Sqlite(_)) {
        Some(SQLITE_RECONCILIATION.lock().await)
    } else {
        None
    };
    let bytes = root.as_bytes();
    let key = i64::from_be_bytes(bytes[..8].try_into().unwrap())
        ^ i64::from_be_bytes(bytes[8..].try_into().unwrap()).rotate_left(13)
        ^ 0x55474147455f5243;
    let billing = db.with_scope(DatabaseScope::Maintenance {
        operation: MaintenanceOperation::Billing,
        org_id: Some(root),
    });
    let Some(guard) = super::try_acquire_advisory_lock(&billing, key).await? else {
        return Ok(None);
    };
    let outcome = async {
        let snapshot = match &billing {
            DatabasePool::Postgres(pool) => {
                let mut tx = pool.begin_repeatable_read().await?;
                let snapshot = snapshot_postgres(&mut tx, root).await?;
                tx.commit().await?;
                let mut tx = pool.begin().await?;
                repair_postgres(&mut tx, root, &snapshot).await?;
                tx.commit().await?;
                snapshot
            }
            DatabasePool::Sqlite(pool) => {
                let mut tx = pool.begin().await?;
                let snapshot = snapshot_sqlite(&mut tx, root).await?;
                tx.commit().await?;
                let mut tx = pool.begin().await?;
                repair_sqlite(&mut tx, root, &snapshot).await?;
                tx.commit().await?;
                snapshot
            }
        };
        Ok(Some(UsageDrift {
            storage_rows: snapshot.storage.len(),
            task_rows: snapshot.tasks.len(),
            run_rows: snapshot.runs.len(),
        }))
    }
    .await;
    guard.release().await;
    outcome
}

/// The singleton scheduler enumerates roots; each repair narrows to one account.
pub async fn reconcile_accounts(db: &DatabasePool) -> Result<(), sqlx::Error> {
    let scheduler = db.with_scope(DatabaseScope::Maintenance {
        operation: MaintenanceOperation::Scheduler,
        org_id: None,
    });
    let roots: Vec<Uuid> = match &scheduler {
        DatabasePool::Postgres(pool) => {
            sqlx::query_scalar("SELECT org_id FROM org WHERE parent_org_id IS NULL ORDER BY org_id")
                .fetch_all(pool)
                .await?
        }
        DatabasePool::Sqlite(pool) => {
            sqlx::query_scalar("SELECT org_id FROM org WHERE parent_org_id IS NULL ORDER BY org_id")
                .fetch_all(pool)
                .await?
        }
    };
    for root in roots {
        if let Err(error) = reconcile_account(&scheduler, root).await {
            tracing::warn!(%root,%error,"Usage reconciliation failed; the next tick will retry");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn storage_truth(pool: &sqlx::SqlitePool, env: Uuid) -> (i64, i64) {
        let actual:(i64,i64)=sqlx::query_as("SELECT COALESCE(SUM(store_bytes),0),COALESCE(SUM(call_bytes),0) FROM usage_storage WHERE env_id=?")
            .bind(env).fetch_one(pool).await.unwrap();
        let store: i64 = sqlx::query_scalar(
            "SELECT COALESCE(SUM(MAX(size,0)),0) FROM store_map_entry WHERE env_id=?",
        )
        .bind(env)
        .fetch_one(pool)
        .await
        .unwrap();
        let call: i64 =
            sqlx::query_scalar("SELECT COALESCE(SUM(MAX(size,0)),0) FROM call WHERE env_id=?")
                .bind(env)
                .fetch_one(pool)
                .await
                .unwrap();
        assert_eq!(actual, (store, call));
        actual
    }

    #[tokio::test]
    async fn counter_writes_rollbacks_cascades_and_snapshot_repairs_preserve_truth() {
        let db = super::super::test_db().await;
        let data = super::super::insert_test_data(&db).await.unwrap();
        let DatabasePool::Sqlite(pool) = &db else {
            unreachable!()
        };
        let calls: Vec<_> = (0..17).map(|_| Uuid::now_v7()).collect();
        for step in 0..120_i64 {
            let id = calls[(step as usize * 7) % calls.len()];
            if step % 5 == 0 {
                sqlx::query("DELETE FROM call WHERE call_id=?")
                    .bind(id)
                    .execute(pool)
                    .await
                    .unwrap();
            } else {
                sqlx::query("INSERT INTO call(call_id,run_id,env_id,function_name,static_scope,runtime_path,call_depth,start_time,size) VALUES(?,?,?,'::counter/test','root','root',0,CURRENT_TIMESTAMP,?) ON CONFLICT(call_id) DO UPDATE SET size=excluded.size")
                    .bind(id).bind(data.run_id).bind(data.env_id).bind(step*19-80).execute(pool).await.unwrap();
            }
            storage_truth(pool, data.env_id).await;
        }
        let before = storage_truth(pool, data.env_id).await;
        let mut tx = pool.begin().await.unwrap();
        sqlx::query("UPDATE call SET size=123456 WHERE run_id=?")
            .bind(data.run_id)
            .execute(&mut *tx)
            .await
            .unwrap();
        tx.rollback().await.unwrap();
        assert_eq!(storage_truth(pool, data.env_id).await, before);

        sqlx::query("UPDATE usage_storage SET call_bytes=call_bytes+73 WHERE env_id=?")
            .bind(data.env_id)
            .execute(pool)
            .await
            .unwrap();
        let mut tx = pool.begin().await.unwrap();
        let snapshot = snapshot_sqlite(&mut tx, data.org_id).await.unwrap();
        tx.commit().await.unwrap();
        // A writer after the snapshot must survive the relative correction.
        sqlx::query("UPDATE call SET size=size+11 WHERE run_id=?")
            .bind(data.run_id)
            .execute(pool)
            .await
            .unwrap();
        let mut tx = pool.begin().await.unwrap();
        repair_sqlite(&mut tx, data.org_id, &snapshot)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        storage_truth(pool, data.env_id).await;
        assert!(
            reconcile_account(&db, data.org_id)
                .await
                .unwrap()
                .unwrap()
                .is_empty()
        );
        sqlx::query("UPDATE usage_storage SET call_bytes=call_bytes+31 WHERE env_id=?")
            .bind(data.env_id)
            .execute(pool)
            .await
            .unwrap();
        assert_eq!(
            reconcile_account(&db, data.org_id)
                .await
                .unwrap()
                .unwrap()
                .storage_rows,
            1
        );
        assert!(
            reconcile_account(&db, data.org_id)
                .await
                .unwrap()
                .unwrap()
                .is_empty()
        );
        // Delete the parent: counters must not depend on the now-missing run.
        sqlx::query("PRAGMA foreign_keys=ON")
            .execute(pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM run WHERE run_id=?")
            .bind(data.run_id)
            .execute(pool)
            .await
            .unwrap();
        assert_eq!(storage_truth(pool, data.env_id).await.1, 0);
    }

    #[tokio::test]
    async fn task_periods_cancellation_and_malformed_metering_keep_exact_counters() {
        let db = super::super::test_db().await;
        let data = super::super::insert_test_data(&db).await.unwrap();
        let DatabasePool::Sqlite(pool) = &db else {
            unreachable!()
        };
        let task = Uuid::now_v7();
        sqlx::query("INSERT INTO task(task_id,env_id,stream_id,build_id,function_name,task_type) VALUES(?,?,?,?,'::counter/task','container')")
            .bind(task).bind(data.env_id).bind(data.stream_id).bind(data.build_id).execute(pool).await.unwrap();
        let period = DateTime::parse_from_rfc3339("2000-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let mut tx = pool.begin().await.unwrap();
        let initial = task_usage_sqlite(&mut tx, data.org_id, period)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        let read = || async {
            sqlx::query_as::<_, TaskTotals>(TASK_TOTALS)
                .bind(data.org_id)
                .fetch_one(pool)
                .await
                .unwrap()
        };
        sqlx::query("UPDATE task SET task_status_id=2,reserved_compute_units=100,reserved_duration_ms=1000 WHERE task_id=?").bind(task).execute(pool).await.unwrap();
        let active = read().await;
        assert_eq!(
            (
                active.0 - initial.0,
                active.1 - initial.1,
                active.2 - initial.2,
                active.3 - initial.3,
                active.4 - initial.4
            ),
            (1, 100, 0, 1000, 0)
        );
        sqlx::query("UPDATE task SET task_status_id=5 WHERE task_id=?")
            .bind(task)
            .execute(pool)
            .await
            .unwrap();
        assert_eq!(
            read().await,
            active,
            "cancel-requested container still holds its reservation"
        );
        sqlx::query("UPDATE task SET task_status_id=3,duration_ms=400,result='{}' WHERE task_id=?")
            .bind(task)
            .execute(pool)
            .await
            .unwrap();
        let settled = read().await;
        assert_eq!(
            (
                settled.0 - initial.0,
                settled.1 - initial.1,
                settled.2 - initial.2,
                settled.3 - initial.3,
                settled.4 - initial.4
            ),
            (0, 0, 40, 0, 400)
        );
        for (metering, expected) in [
            (serde_json::json!({"compute-units":i64::MAX}), i64::MAX),
            (
                serde_json::json!({"compute-units":i64::MAX.to_string()}),
                i64::MAX,
            ),
            (
                serde_json::json!({"$val":{"err":{"compute-units":i64::MAX}}}),
                i64::MAX,
            ),
            (
                serde_json::json!({"compute-units":"9223372036854775808"}),
                40,
            ),
            (
                serde_json::json!({"compute-units":9223372036854775808_u64}),
                40,
            ),
            (serde_json::json!({"compute-units":i64::MIN}), 0),
        ] {
            sqlx::query("UPDATE task SET result=? WHERE task_id=?")
                .bind(metering.to_string())
                .bind(task)
                .execute(pool)
                .await
                .unwrap();
            assert_eq!(read().await.2, expected);
        }
        for malformed in [
            serde_json::json!({"compute-units":"broken"}),
            serde_json::json!({"compute-units":true}),
            serde_json::json!({"compute-units":{}}),
        ] {
            sqlx::query("UPDATE task SET result=? WHERE task_id=?")
                .bind(malformed.to_string())
                .bind(task)
                .execute(pool)
                .await
                .unwrap();
            assert_eq!(read().await, settled);
        }
        sqlx::query("UPDATE task SET result='not valid JSON' WHERE task_id=?")
            .bind(task)
            .execute(pool)
            .await
            .unwrap();
        assert_eq!(read().await, settled);
        sqlx::query("UPDATE task SET result='{\"compute-units\":7}' WHERE task_id=?")
            .bind(task)
            .execute(pool)
            .await
            .unwrap();
        assert_eq!(read().await.2, initial.2 + 7);
        let mut tx = pool.begin().await.unwrap();
        task_usage_sqlite(&mut tx, data.org_id, Utc::now() + chrono::Duration::days(1))
            .await
            .unwrap();
        tx.commit().await.unwrap();
        assert_eq!(read().await.2, 0);
        let mut tx = pool.begin().await.unwrap();
        task_usage_sqlite(&mut tx, data.org_id, period)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        assert_eq!(read().await.2, initial.2 + 7);
        sqlx::query(
            "UPDATE usage_task SET reserved_compute_units=reserved_compute_units+5 WHERE env_id=?",
        )
        .bind(data.env_id)
        .execute(pool)
        .await
        .unwrap();
        assert_eq!(
            reconcile_account(&db, data.org_id)
                .await
                .unwrap()
                .unwrap()
                .task_rows,
            1
        );
        assert!(
            reconcile_account(&db, data.org_id)
                .await
                .unwrap()
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn run_counter_tracks_backdated_periods_and_does_not_restore_pruned_rows() {
        let db = super::super::test_db().await;
        let data = super::super::insert_test_data(&db).await.unwrap();
        let DatabasePool::Sqlite(pool) = &db else {
            unreachable!()
        };
        let period = DateTime::parse_from_rfc3339("2000-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let mut tx = pool.begin().await.unwrap();
        let before = run_usage_sqlite(&mut tx, data.org_id, period)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        sqlx::query("INSERT INTO run_admission(run_id,root_org_id,org_id,env_id,admitted_at) VALUES(?,?,?,?,?)")
            .bind(Uuid::now_v7()).bind(data.org_id).bind(data.org_id).bind(data.env_id).bind(Utc::now()).execute(pool).await.unwrap();
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT admitted FROM usage_run WHERE root_org_id=?")
                .bind(data.org_id)
                .fetch_one(pool)
                .await
                .unwrap(),
            before + 1
        );
        sqlx::query("UPDATE usage_run SET admitted=admitted+10 WHERE root_org_id=?")
            .bind(data.org_id)
            .execute(pool)
            .await
            .unwrap();
        let mut tx = pool.begin().await.unwrap();
        let snapshot = snapshot_sqlite(&mut tx, data.org_id).await.unwrap();
        tx.commit().await.unwrap();
        let mut tx = pool.begin().await.unwrap();
        assert_eq!(
            run_usage_sqlite(&mut tx, data.org_id, Utc::now() + chrono::Duration::days(1))
                .await
                .unwrap(),
            0
        );
        tx.commit().await.unwrap();
        let mut tx = pool.begin().await.unwrap();
        repair_sqlite(&mut tx, data.org_id, &snapshot)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM usage_run WHERE root_org_id=?")
                .bind(data.org_id)
                .fetch_one(pool)
                .await
                .unwrap(),
            1
        );
        assert!(
            reconcile_account(&db, data.org_id)
                .await
                .unwrap()
                .unwrap()
                .is_empty()
        );
    }
}

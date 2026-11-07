//! Maintained account counters. Admission holds the account lock; row triggers
//! publish usage in the same transaction as the work they count.
use chrono::{DateTime, Utc};
use sqlx::{PgConnection, SqliteConnection};
use std::time::Duration;
use uuid::Uuid;

use super::{DatabasePool, DatabaseScope, MaintenanceOperation, ScopedPgPool};

type TaskTotals = (i64, i64, i64, i64, i64);
const TASK_TOTALS: &str = "SELECT CAST(COALESCE(SUM(u.active_containers),0) AS BIGINT),CAST(COALESCE(SUM(u.reserved_compute_units),0) AS BIGINT),CAST(COALESCE(SUM(u.compute_units),0) AS BIGINT),CAST(COALESCE(SUM(u.reserved_duration_ms),0) AS BIGINT),CAST(COALESCE(SUM(u.duration_ms),0) AS BIGINT) FROM usage_task u JOIN org o ON o.org_id=u.org_id WHERE o.org_id=$1 OR o.parent_org_id=$1";

/// One environment's usage in `period`. The timestamp predicate matches
/// `usage_at`, which `task_env_usage_at_idx` can serve. Metered results are
/// read from TOAST, so on a large account this takes seconds. It must never run
/// while holding a counter row: every task writer in the environment updates
/// that row, and fails once its lock timeout passes.
const PERIOD_TOTALS: &str = "SELECT CAST(COALESCE(SUM(f.compute_units),0) AS BIGINT),CAST(COALESCE(SUM(f.duration_ms),0) AS BIGINT) FROM task t CROSS JOIN LATERAL hot.task_usage(t) f WHERE t.env_id=$1 AND COALESCE(t.stop_time,t.start_time,t.created_at) >= $2";

/// The account's environments whose counter is not on `period`, including
/// counter rows left by a deleted environment, which admission also reads.
const STALE_ENVS: &str = "SELECT x.env_id,x.org_id FROM (SELECT e.env_id,e.org_id FROM env e JOIN org o ON o.org_id=e.org_id WHERE o.org_id=$1 OR o.parent_org_id=$1 UNION SELECT u.env_id,u.org_id FROM usage_task u JOIN org o ON o.org_id=u.org_id WHERE o.org_id=$1 OR o.parent_org_id=$1) x LEFT JOIN usage_task u ON u.env_id=x.env_id WHERE u.period_start IS DISTINCT FROM $2 ORDER BY x.env_id";

/// How long a caller waits for counters to reach a new period. Shorter than the
/// worker's claim timeout, so a slow recount defers the claim with a clear
/// reason. The recount itself carries on in the background.
const ROLLOVER_WAIT: Duration = Duration::from_secs(10);
const ROLLOVER_POLL: Duration = Duration::from_millis(200);
/// The recount may outlast the pool's statement timeout on a large account.
/// Reads only, so all it holds back meanwhile is vacuum.
const ROLLOVER_STATEMENT_TIMEOUT_MS: &str = "600000";
/// Attempts to start a recount in the moment between its snapshot and its mark.
const ROLLOVER_ATTEMPTS: usize = 5;
const ROLLOVER_LOCK: i64 = 0x5553_4147_4552_4f4c;

/// An environment's counter while a new period is recounted without holding its
/// row. Dropping this before `finish` abandons the recount: the next one starts
/// over, and its mark discards what this one counted.
struct PeriodRollover {
    /// Repeatable read, so the totals are as of the snapshot the mark was
    /// checked against. Holds the recount's advisory lock until it ends.
    snapshot: crate::db::ScopedPgTransaction,
    env_id: Uuid,
    period: DateTime<Utc>,
}

/// What became of an attempt to start a recount.
enum RolloverStart {
    Started(PeriodRollover),
    /// The counter is already on the period.
    Current,
    /// Another worker is recounting, or task writers kept beating the mark.
    Deferred,
}

impl PeriodRollover {
    /// Take a snapshot and mark the counter row, if no writer has changed it
    /// since. Every change to a task's usage updates that row in the same
    /// transaction, so an unchanged row means the snapshot holds every writer
    /// that has committed. A writer still to reach the row finds the mark and
    /// counts itself in the rollover columns, and is not in the snapshot.
    async fn start(
        pool: &ScopedPgPool,
        env_id: Uuid,
        org_id: Uuid,
        period: DateTime<Utc>,
    ) -> Result<RolloverStart, sqlx::Error> {
        sqlx::query(
            "INSERT INTO usage_task(env_id,org_id) VALUES($1,$2) ON CONFLICT(env_id) DO NOTHING",
        )
        .bind(env_id)
        .bind(org_id)
        .execute(pool)
        .await?;
        for _ in 0..ROLLOVER_ATTEMPTS {
            let mut snapshot = pool.begin_repeatable_read().await?;
            // A second recount's mark would discard the changes this one counted.
            if !lock_unit(&mut snapshot, reconcile_lock(env_id, ROLLOVER_LOCK)).await? {
                return Ok(RolloverStart::Deferred);
            }
            sqlx::query("SELECT set_config('statement_timeout',$1,true)")
                .bind(ROLLOVER_STATEMENT_TIMEOUT_MS)
                .execute(&mut *snapshot)
                .await?;
            let version: Option<String> = sqlx::query_scalar(
                "SELECT xmin::text FROM usage_task WHERE env_id=$1 AND period_start IS DISTINCT FROM $2",
            )
            .bind(env_id)
            .bind(period)
            .fetch_optional(&mut *snapshot)
            .await?;
            let Some(version) = version else {
                return Ok(RolloverStart::Current);
            };
            let marked = sqlx::query(
                "UPDATE usage_task SET rollover_period=$2,rollover_compute_units=0,rollover_duration_ms=0 WHERE env_id=$1 AND xmin::text=$3",
            )
            .bind(env_id)
            .bind(period)
            .bind(version)
            .execute(pool)
            .await?
            .rows_affected();
            if marked == 1 {
                return Ok(RolloverStart::Started(Self {
                    snapshot,
                    env_id,
                    period,
                }));
            }
        }
        Ok(RolloverStart::Deferred)
    }

    /// Read the period's totals as of the snapshot, holding no row lock.
    async fn totals(&mut self) -> Result<(i64, i64), sqlx::Error> {
        sqlx::query_as(PERIOD_TOTALS)
            .bind(self.env_id)
            .bind(self.period)
            .fetch_one(&mut *self.snapshot)
            .await
    }

    /// Move the counter to the period in one short update: the snapshot's totals
    /// plus every change counted since the mark.
    async fn finish(self, pool: &ScopedPgPool, totals: (i64, i64)) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE usage_task SET compute_units=$3+rollover_compute_units,duration_ms=$4+rollover_duration_ms,period_start=$2,rollover_period=NULL,rollover_compute_units=0,rollover_duration_ms=0 WHERE env_id=$1 AND rollover_period=$2",
        )
        .bind(self.env_id)
        .bind(self.period)
        .bind(totals.0)
        .bind(totals.1)
        .execute(pool)
        .await?;
        self.snapshot.commit().await
    }
}

async fn recount_period(
    pool: &ScopedPgPool,
    stale: Vec<(Uuid, Uuid)>,
    period: DateTime<Utc>,
) -> Result<(), sqlx::Error> {
    for (env_id, org_id) in stale {
        if let RolloverStart::Started(mut rollover) =
            PeriodRollover::start(pool, env_id, org_id, period).await?
        {
            let totals = rollover.totals().await?;
            rollover.finish(pool, totals).await?;
        }
    }
    Ok(())
}

/// Initialize counters for a new billing period before admission takes the
/// account lock, so neither the account row nor a counter row is held during
/// the recount. Each environment commits separately under Billing authority,
/// which also covers a deleted environment's counter, so a rejected claim
/// cannot discard the work. Returns false while the counters are still moving;
/// the caller retries later. A caller that gives up leaves the recount running,
/// otherwise a recount longer than the caller's timeout would never finish.
pub(crate) async fn prepare_task_period(
    db: &DatabasePool,
    root: Uuid,
    period: DateTime<Utc>,
) -> Result<bool, sqlx::Error> {
    let DatabasePool::Postgres(pool) = db else {
        return Ok(true);
    };
    let billing = pool.with_scope(DatabaseScope::Maintenance {
        operation: MaintenanceOperation::Billing,
        org_id: Some(root),
    });
    let stale: Vec<(Uuid, Uuid)> = sqlx::query_as(STALE_ENVS)
        .bind(root)
        .bind(period)
        .fetch_all(&billing)
        .await?;
    if stale.is_empty() {
        return Ok(true);
    }
    let deadline = tokio::time::Instant::now() + ROLLOVER_WAIT;
    let recount_pool = billing.clone();
    let recount = tokio::spawn(async move {
        let result = recount_period(&recount_pool, stale, period).await;
        if let Err(error) = &result {
            tracing::warn!(%root, %error, "Could not move usage counters to a new billing period");
        }
        result
    });
    match tokio::time::timeout_at(deadline, recount).await {
        Ok(Ok(result)) => result?,
        Ok(Err(error)) => return Err(sqlx::Error::Protocol(error.to_string())),
        Err(_) => return Ok(false),
    }
    // Environments another worker was recounting may still be moving.
    loop {
        let moving: Vec<(Uuid, Uuid)> = sqlx::query_as(STALE_ENVS)
            .bind(root)
            .bind(period)
            .fetch_all(&billing)
            .await?;
        if moving.is_empty() {
            return Ok(true);
        }
        if tokio::time::Instant::now() + ROLLOVER_POLL >= deadline {
            return Ok(false);
        }
        tokio::time::sleep(ROLLOVER_POLL).await;
    }
}

/// Called under the root lock, after `prepare_task_period`. A counter that is
/// not on the period now, because the plan's period moved or a new environment
/// appeared in between, defers this claim to the next delivery's preparation:
/// admission never recounts tasks, so it cannot hold the account lock or a
/// counter row across a scan.
pub(crate) async fn task_usage_postgres(
    conn: &mut PgConnection,
    root: Uuid,
    period: DateTime<Utc>,
) -> Result<TaskTotals, sqlx::Error> {
    let moved: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM usage_task u JOIN org o ON o.org_id=u.org_id WHERE (o.org_id=$1 OR o.parent_org_id=$1) AND u.period_start IS DISTINCT FROM $2)")
        .bind(root).bind(period).fetch_one(&mut *conn).await?;
    if moved {
        return Err(sqlx::Error::Protocol(
            "Usage counters moved to another billing period; retry the claim".into(),
        ));
    }
    sqlx::query_as(TASK_TOTALS).bind(root).fetch_one(conn).await
}

/// An account's task usage in one billing period, from the counters admission
/// enforces. Checks ahead of a claim read this rather than scanning task
/// history: claim admission remains the authority.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AccountTaskUsage {
    pub compute_units: i64,
    pub duration_ms: i64,
}

/// `None` while the account's counters are still moving to `period`.
pub async fn account_task_usage(
    db: &DatabasePool,
    org_id: &Uuid,
    period: DateTime<Utc>,
) -> Result<Option<AccountTaskUsage>, sqlx::Error> {
    let root = super::org::Org::billing_owner_id_for(db, org_id)
        .await
        .map_err(|error| sqlx::Error::Protocol(error.to_string()))?;
    let billing = db.with_scope(DatabaseScope::Maintenance {
        operation: MaintenanceOperation::Billing,
        org_id: Some(root),
    });
    let totals = match &billing {
        DatabasePool::Postgres(pool) => {
            if !prepare_task_period(&billing, root, period).await? {
                return Ok(None);
            }
            sqlx::query_as::<_, TaskTotals>(TASK_TOTALS)
                .bind(root)
                .fetch_one(pool)
                .await?
        }
        DatabasePool::Sqlite(pool) => {
            let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
            let totals = task_usage_sqlite(&mut tx, root, period).await?;
            tx.commit().await?;
            totals
        }
    };
    Ok(Some(AccountTaskUsage {
        compute_units: totals.2,
        duration_ms: totals.4,
    }))
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

// Reconciliation checks one counter at a time: a single statement compares the
// counter with its source rows (one snapshot), and the same short transaction
// adds the difference. No snapshot spans an account, and no repair holds more
// than one counter row, so repairs cannot deadlock with ordered writers.
//
// The sum stays in that one statement because only one snapshot makes the
// difference exact: summing `call` in batches would compare a counter with rows
// from several snapshots, and a writer between them would read as drift. Nor do
// batches read any less. `call_env_start_time_size_idx` is what keeps this from
// reading the trace heap, whose rows hold each call's arguments and return value.

/// The account's environments, including counter rows a deleted environment
/// left, in environment order. The flag marks environments that still exist.
/// Store rows need no branch here, and it would read every entry in the account:
/// since migration 019, foreign keys stop them outliving their environment.
const ACCOUNT_ENVS: &str = "SELECT x.env_id,x.org_id,EXISTS(SELECT 1 FROM env e WHERE e.env_id=x.env_id AND e.org_id=x.org_id) AS live FROM (SELECT env_id,org_id FROM env WHERE org_id IN (SELECT org_id FROM org WHERE org_id=$1 OR parent_org_id=$1) UNION SELECT env_id,org_id FROM usage_storage WHERE org_id IN (SELECT org_id FROM org WHERE org_id=$1 OR parent_org_id=$1) UNION SELECT env_id,org_id FROM usage_task WHERE org_id IN (SELECT org_id FROM org WHERE org_id=$1 OR parent_org_id=$1)) x ORDER BY x.env_id,live DESC";
/// Store rows are found through their key, which starts with the organization.
/// Reading their sizes from the table is deliberate: `size` in an index would
/// stop every store write that changes it from being a HOT update.
const STORAGE_CHECK: &str = "SELECT (SELECT CAST(COALESCE(SUM(CASE WHEN size<0 THEN 0 ELSE size END),0) AS BIGINT) FROM store_map_entry WHERE org_id=$2 AND env_id=$1),(SELECT CAST(COALESCE(SUM(CASE WHEN size<0 THEN 0 ELSE size END),0) AS BIGINT) FROM call WHERE env_id=$1),(SELECT CAST(COALESCE(SUM(store_bytes),0) AS BIGINT) FROM usage_storage WHERE env_id=$1),(SELECT CAST(COALESCE(SUM(call_bytes),0) AS BIGINT) FROM usage_storage WHERE env_id=$1)";
const STORAGE_REPAIR: &str = "INSERT INTO usage_storage(env_id,org_id,slot,store_bytes,call_bytes) VALUES($1,$2,0,$3,$4) ON CONFLICT(env_id,slot) DO UPDATE SET store_bytes=usage_storage.store_bytes+excluded.store_bytes,call_bytes=usage_storage.call_bytes+excluded.call_bytes";
const RUN_REPAIR: &str =
    "UPDATE usage_run SET admitted=admitted+$3 WHERE root_org_id=$1 AND period_start=$2";

type TaskCheck = (
    Option<DateTime<Utc>>,
    i64,
    i64,
    i64,
    i64,
    i64,
    i64,
    i64,
    i64,
    i64,
    i64,
);

fn overflow() -> sqlx::Error {
    sqlx::Error::Protocol("Usage reconciliation delta overflow".into())
}

macro_rules! reconcile_units {
    ($storage:ident,$task:ident,$run:ident,$connection:ty,$task_from:literal,$task_env:literal,$since:literal,$same_period:literal,$run_since:literal) => {
        /// Add the difference between one environment's store and call bytes
        /// and its storage counters to slot zero.
        async fn $storage(conn: &mut $connection, env: Uuid, org: Uuid) -> Result<bool, sqlx::Error> {
            let (store, call, counted_store, counted_call): (i64, i64, i64, i64) =
                sqlx::query_as(STORAGE_CHECK).bind(env).bind(org).fetch_one(&mut *conn).await?;
            let store = store.checked_sub(counted_store).ok_or_else(overflow)?;
            let call = call.checked_sub(counted_call).ok_or_else(overflow)?;
            if store == 0 && call == 0 {
                return Ok(false);
            }
            sqlx::query(STORAGE_REPAIR).bind(env).bind(org).bind(store).bind(call).execute(conn).await?;
            tracing::warn!(%env,store_delta=store,call_delta=call,"Repaired storage usage drift");
            Ok(true)
        }

        /// Add the difference between one environment's tasks and its task
        /// counter. A period that changed before the repair skips it.
        async fn $task(conn: &mut $connection, env: Uuid, org: Uuid) -> Result<bool, sqlx::Error> {
            let row: TaskCheck = sqlx::query_as(concat!(
                "SELECT u.p,u.a,u.b,u.c,u.d,u.e,s.a,s.b,s.c,s.d,s.e FROM (SELECT MAX(period_start) AS p,CAST(COALESCE(MAX(active_containers),0) AS BIGINT) AS a,CAST(COALESCE(MAX(reserved_compute_units),0) AS BIGINT) AS b,CAST(COALESCE(MAX(reserved_duration_ms),0) AS BIGINT) AS c,CAST(COALESCE(MAX(compute_units),0) AS BIGINT) AS d,CAST(COALESCE(MAX(duration_ms),0) AS BIGINT) AS e FROM usage_task WHERE env_id=$1) u,",
                "(SELECT CAST(COALESCE(SUM(f.active_containers),0) AS BIGINT) AS a,CAST(COALESCE(SUM(f.reserved_compute_units),0) AS BIGINT) AS b,CAST(COALESCE(SUM(f.reserved_duration_ms),0) AS BIGINT) AS c,CAST(COALESCE(SUM(CASE WHEN ",$since," THEN f.compute_units ELSE 0 END),0) AS BIGINT) AS d,CAST(COALESCE(SUM(CASE WHEN ",$since," THEN f.duration_ms ELSE 0 END),0) AS BIGINT) AS e FROM ",$task_from," WHERE ",$task_env,"=$1) s"))
                .bind(env).fetch_one(&mut *conn).await?;
            let period = row.0;
            let mut delta = [0; 5];
            for (i, (expected, counted)) in [row.6, row.7, row.8, row.9, row.10]
                .into_iter()
                .zip([row.1, row.2, row.3, row.4, row.5])
                .enumerate()
            {
                delta[i] = expected.checked_sub(counted).ok_or_else(overflow)?;
            }
            if delta.iter().all(|d| *d == 0) {
                return Ok(false);
            }
            let changed=sqlx::query(concat!("INSERT INTO usage_task(env_id,org_id,period_start,active_containers,reserved_compute_units,reserved_duration_ms,compute_units,duration_ms) VALUES($1,$2,$3,$4,$5,$6,$7,$8) ON CONFLICT(env_id) DO UPDATE SET active_containers=usage_task.active_containers+excluded.active_containers,reserved_compute_units=usage_task.reserved_compute_units+excluded.reserved_compute_units,reserved_duration_ms=usage_task.reserved_duration_ms+excluded.reserved_duration_ms,compute_units=usage_task.compute_units+excluded.compute_units,duration_ms=usage_task.duration_ms+excluded.duration_ms WHERE ",$same_period))
                .bind(env).bind(org).bind(period).bind(delta[0]).bind(delta[1]).bind(delta[2]).bind(delta[3]).bind(delta[4]).execute(conn).await?.rows_affected();
            tracing::warn!(%env,?period,?delta,changed,"Reconciled task usage drift");
            Ok(true)
        }

        /// Add the difference between the admission ledger and one run counter
        /// period. A period pruned in between is never recreated.
        async fn $run(conn: &mut $connection, root: Uuid, period: DateTime<Utc>) -> Result<bool, sqlx::Error> {
            let row: Option<(i64, i64)> = sqlx::query_as(concat!("SELECT u.admitted,(SELECT COUNT(*) FROM run_admission WHERE root_org_id=$1 AND ",$run_since,") FROM usage_run u WHERE u.root_org_id=$1 AND u.period_start=$2"))
                .bind(root).bind(period).fetch_optional(&mut *conn).await?;
            let Some((counted, admitted)) = row else {
                return Ok(false);
            };
            let delta = admitted.checked_sub(counted).ok_or_else(overflow)?;
            if delta == 0 {
                return Ok(false);
            }
            sqlx::query(RUN_REPAIR).bind(root).bind(period).bind(delta).execute(conn).await?;
            tracing::warn!(%root,?period,delta,"Reconciled run usage drift");
            Ok(true)
        }
    };
}
// Task and run timestamp predicates differ, so use two explicit expansions.
reconcile_units!(
    reconcile_storage_postgres,
    reconcile_task_postgres,
    reconcile_run_postgres,
    PgConnection,
    "task t CROSS JOIN LATERAL hot.task_usage(t) f",
    "t.env_id",
    "f.usage_at >= (SELECT period_start FROM usage_task WHERE env_id=$1)",
    "usage_task.period_start IS NOT DISTINCT FROM excluded.period_start",
    "admitted_at >= $2"
);
reconcile_units!(
    reconcile_storage_sqlite,
    reconcile_task_sqlite,
    reconcile_run_sqlite,
    SqliteConnection,
    "task_usage f",
    "f.env_id",
    "julianday(f.usage_at) >= julianday((SELECT period_start FROM usage_task WHERE env_id=$1))",
    "usage_task.period_start IS excluded.period_start",
    "julianday(admitted_at) >= julianday($2)"
);

/// Number of counter rows whose check differed from source data.
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

/// The first check of a pass to fail. The pass carries on: a counter that keeps
/// failing must not stop the rest of the account from being checked every hour.
#[derive(Default)]
struct FailedChecks(Option<sqlx::Error>);

impl FailedChecks {
    /// Whether the check found drift. Only an error from the check's own
    /// statements is survivable: after a lost connection or an exhausted pool,
    /// each remaining check would fail too, waiting out a timeout in turn.
    fn note(
        &mut self,
        check: &str,
        id: Uuid,
        result: Result<bool, sqlx::Error>,
    ) -> Result<bool, sqlx::Error> {
        match result {
            Err(error @ sqlx::Error::Database(_)) => {
                tracing::warn!(check, %id, %error, "Usage check failed; the next tick will retry");
                self.0.get_or_insert(error);
                Ok(false)
            }
            result => result,
        }
    }

    fn finish(self, drift: UsageDrift) -> Result<UsageDrift, sqlx::Error> {
        match self.0 {
            Some(error) => Err(error),
            None => Ok(drift),
        }
    }
}

static SQLITE_RECONCILIATION: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

const STORAGE_LOCK: i64 = 0x5553_4147_4553_544f;
const TASK_LOCK: i64 = 0x5553_4147_4554_4153;
const RUN_LOCK: i64 = 0x5553_4147_4552_554e;

fn reconcile_lock(id: Uuid, kind: i64) -> i64 {
    let bytes = id.as_bytes();
    i64::from_be_bytes(bytes[..8].try_into().unwrap())
        ^ i64::from_be_bytes(bytes[8..].try_into().unwrap()).rotate_left(13)
        ^ kind
}

/// PostgreSQL check units take a transaction advisory lock, so two reconcilers
/// never add the same difference twice. A unit that another reconciler holds
/// is skipped until the next tick.
async fn lock_unit(conn: &mut PgConnection, key: i64) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar("SELECT pg_try_advisory_xact_lock($1)")
        .bind(key)
        .fetch_one(conn)
        .await
}

/// Trusted maintenance only. Check each counter of the account against its
/// source rows in its own short transaction and add any difference, without
/// locking the account row. Live environments are checked under their exact
/// scope; orphaned rows and run counters under Billing authority. SQLite
/// checks hold the writer lock, and a mutex serializes local reconcilers.
/// A check that fails doesn't stop the others; the pass then returns its error.
pub async fn reconcile_account(db: &DatabasePool, root: Uuid) -> Result<UsageDrift, sqlx::Error> {
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
    let billing = db.with_scope(DatabaseScope::Maintenance {
        operation: MaintenanceOperation::Billing,
        org_id: Some(root),
    });
    let mut drift = UsageDrift::default();
    let mut failed = FailedChecks::default();
    let mut previous = None;
    match &billing {
        DatabasePool::Postgres(pool) => {
            let is_root: Option<bool> =
                sqlx::query_scalar("SELECT parent_org_id IS NULL FROM org WHERE org_id=$1")
                    .bind(root)
                    .fetch_optional(pool)
                    .await?;
            if is_root != Some(true) {
                return Err(root_required());
            }
            let overrides: Option<serde_json::Value> =
                sqlx::query_scalar("SELECT features FROM org WHERE org_id=$1")
                    .bind(root)
                    .fetch_one(pool)
                    .await?;
            let plan: Option<serde_json::Value> = sqlx::query_scalar(
                "SELECT COALESCE(p.features,'{}'::jsonb) FROM org_plan op JOIN plan p ON p.plan_uuid=op.plan_uuid WHERE op.org_id=$1",
            )
            .bind(root)
            .fetch_optional(pool)
            .await?
            .flatten();
            if !counters_enforced(plan.as_ref(), overrides.as_ref()) {
                return Ok(drift);
            }
            let envs: Vec<(Uuid, Uuid, bool)> = sqlx::query_as(ACCOUNT_ENVS)
                .bind(root)
                .fetch_all(pool)
                .await?;
            for (env, org, live) in envs {
                if previous.replace(env) == Some(env) {
                    continue;
                }
                let scoped = if live {
                    pool.with_scope(DatabaseScope::Environment {
                        org_id: org,
                        env_id: env,
                    })
                } else {
                    pool.clone()
                };
                let checked = async {
                    let mut tx = scoped.begin().await?;
                    let found = lock_unit(&mut tx, reconcile_lock(env, STORAGE_LOCK)).await?
                        && reconcile_storage_postgres(&mut tx, env, org).await?;
                    tx.commit().await?;
                    Ok(found)
                }
                .await;
                if failed.note("storage", env, checked)? {
                    drift.storage_rows += 1;
                }
                let checked = async {
                    let mut tx = scoped.begin().await?;
                    let found = lock_unit(&mut tx, reconcile_lock(env, TASK_LOCK)).await?
                        && reconcile_task_postgres(&mut tx, env, org).await?;
                    tx.commit().await?;
                    Ok(found)
                }
                .await;
                if failed.note("task", env, checked)? {
                    drift.task_rows += 1;
                }
            }
            let periods: Vec<DateTime<Utc>> = sqlx::query_scalar(
                "SELECT period_start FROM usage_run WHERE root_org_id=$1 ORDER BY period_start",
            )
            .bind(root)
            .fetch_all(pool)
            .await?;
            for period in periods {
                let checked = async {
                    let mut tx = pool.begin().await?;
                    let found = lock_unit(&mut tx, reconcile_lock(root, RUN_LOCK)).await?
                        && reconcile_run_postgres(&mut tx, root, period).await?;
                    tx.commit().await?;
                    Ok(found)
                }
                .await;
                if failed.note("run", root, checked)? {
                    drift.run_rows += 1;
                }
            }
        }
        DatabasePool::Sqlite(pool) => {
            let is_root: Option<bool> =
                sqlx::query_scalar("SELECT parent_org_id IS NULL FROM org WHERE org_id=$1")
                    .bind(root)
                    .fetch_optional(pool)
                    .await?;
            if is_root != Some(true) {
                return Err(root_required());
            }
            let overrides: Option<String> =
                sqlx::query_scalar("SELECT features FROM org WHERE org_id=$1")
                    .bind(root)
                    .fetch_one(pool)
                    .await?;
            let plan: Option<String> = sqlx::query_scalar(
                "SELECT COALESCE(p.features,'{}') FROM org_plan op JOIN plan p ON p.plan_uuid=op.plan_uuid WHERE op.org_id=$1",
            )
            .bind(root)
            .fetch_optional(pool)
            .await?
            .flatten();
            let parse = |raw: Option<String>| {
                raw.map(|s| serde_json::from_str::<serde_json::Value>(&s))
                    .transpose()
            };
            let enforced = match (parse(plan), parse(overrides)) {
                (Ok(plan), Ok(overrides)) => counters_enforced(plan.as_ref(), overrides.as_ref()),
                _ => true,
            };
            if !enforced {
                return Ok(drift);
            }
            let envs: Vec<(Uuid, Uuid, bool)> = sqlx::query_as(ACCOUNT_ENVS)
                .bind(root)
                .fetch_all(pool)
                .await?;
            for (env, org, _) in envs {
                if previous.replace(env) == Some(env) {
                    continue;
                }
                let checked = async {
                    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
                    let found = reconcile_storage_sqlite(&mut tx, env, org).await?;
                    tx.commit().await?;
                    Ok(found)
                }
                .await;
                if failed.note("storage", env, checked)? {
                    drift.storage_rows += 1;
                }
                let checked = async {
                    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
                    let found = reconcile_task_sqlite(&mut tx, env, org).await?;
                    tx.commit().await?;
                    Ok(found)
                }
                .await;
                if failed.note("task", env, checked)? {
                    drift.task_rows += 1;
                }
            }
            let periods: Vec<DateTime<Utc>> = sqlx::query_scalar(
                "SELECT period_start FROM usage_run WHERE root_org_id=$1 ORDER BY period_start",
            )
            .bind(root)
            .fetch_all(pool)
            .await?;
            for period in periods {
                let checked = async {
                    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
                    let found = reconcile_run_sqlite(&mut tx, root, period).await?;
                    tx.commit().await?;
                    Ok(found)
                }
                .await;
                if failed.note("run", root, checked)? {
                    drift.run_rows += 1;
                }
            }
        }
    }
    failed.finish(drift)
}

/// Whether any limit of the account reads a usage counter. An account
/// without one (no plan, or every limit unlimited) has nothing to enforce, so
/// reconciliation skips its full-history checks. Unreadable features count as
/// limited.
fn counters_enforced(
    plan: Option<&serde_json::Value>,
    overrides: Option<&serde_json::Value>,
) -> bool {
    use super::features::keys;
    let features = super::Features::for_account(plan, overrides);
    let limited = |key: &str| features.get_i64(key).is_none_or(|limit| limit >= 0);
    limited(keys::STORE_STORAGE_BYTES)
        || limited(keys::CALL_STORAGE_BYTES)
        || limited(keys::RUNS_PER_MONTH)
        || features.box_concurrent_tasks() >= 0
        || features.compute_units_budget() > 0
        || features.compute_units_per_month() >= 0
        || features.task_minutes_per_month() >= 0
}

fn root_required() -> sqlx::Error {
    sqlx::Error::Protocol("Usage reconciliation requires a root account".into())
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
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    /// Reconciliation only checks accounts with an enforced limit.
    async fn limit_runs(pool: &sqlx::SqlitePool, org: Uuid) {
        sqlx::query("UPDATE org SET features='{\"runs_per_month\":1000}' WHERE org_id=?")
            .bind(org)
            .execute(pool)
            .await
            .unwrap();
    }

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
        limit_runs(pool, data.org_id).await;
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
        // A later writer keeps its own trigger delta beside the correction.
        sqlx::query("UPDATE call SET size=size+11 WHERE run_id=?")
            .bind(data.run_id)
            .execute(pool)
            .await
            .unwrap();
        assert_eq!(
            reconcile_account(&db, data.org_id)
                .await
                .unwrap()
                .storage_rows,
            1
        );
        storage_truth(pool, data.env_id).await;
        assert!(
            reconcile_account(&db, data.org_id)
                .await
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
                .storage_rows,
            1
        );
        assert!(
            reconcile_account(&db, data.org_id)
                .await
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
        limit_runs(pool, data.org_id).await;
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
            reconcile_account(&db, data.org_id).await.unwrap().task_rows,
            1
        );
        assert!(
            reconcile_account(&db, data.org_id)
                .await
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
        limit_runs(pool, data.org_id).await;
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
        let drifted: DateTime<Utc> =
            sqlx::query_scalar("SELECT period_start FROM usage_run WHERE root_org_id=?")
                .bind(data.org_id)
                .fetch_one(pool)
                .await
                .unwrap();
        // A claim prunes that period after the reconciler listed it.
        let mut tx = pool.begin().await.unwrap();
        assert_eq!(
            run_usage_sqlite(&mut tx, data.org_id, Utc::now() + chrono::Duration::days(1))
                .await
                .unwrap(),
            0
        );
        tx.commit().await.unwrap();
        let mut tx = pool.begin().await.unwrap();
        assert!(
            !reconcile_run_sqlite(&mut tx, data.org_id, drifted)
                .await
                .unwrap()
        );
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
                .is_empty()
        );
    }

    #[tokio::test]
    async fn reconciliation_skips_accounts_without_limits() {
        let db = super::super::test_db().await;
        let data = super::super::insert_test_data(&db).await.unwrap();
        let DatabasePool::Sqlite(pool) = &db else {
            unreachable!()
        };
        sqlx::query("INSERT INTO usage_storage(env_id,org_id,slot,store_bytes) VALUES(?,?,0,99) ON CONFLICT(env_id,slot) DO UPDATE SET store_bytes=store_bytes+99")
            .bind(data.env_id)
            .bind(data.org_id)
            .execute(pool)
            .await
            .unwrap();
        // Nothing is enforced without a plan or a limited override.
        assert!(
            reconcile_account(&db, data.org_id)
                .await
                .unwrap()
                .is_empty()
        );
        limit_runs(pool, data.org_id).await;
        assert_eq!(
            reconcile_account(&db, data.org_id)
                .await
                .unwrap()
                .storage_rows,
            1
        );
    }

    #[tokio::test]
    async fn storage_check_reads_through_indexes() {
        let db = super::super::test_db().await;
        let data = super::super::insert_test_data(&db).await.unwrap();
        let DatabasePool::Sqlite(pool) = &db else {
            unreachable!()
        };
        let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
            "EXPLAIN QUERY PLAN {STORAGE_CHECK}"
        )))
        .bind(data.env_id)
        .bind(data.org_id)
        .fetch_all(pool)
        .await
        .unwrap();
        let plan: Vec<String> = rows
            .iter()
            .map(|row| sqlx::Row::get::<String, _>(row, "detail"))
            .collect();
        assert!(
            plan.iter().any(|step| step
                .contains("SEARCH call USING COVERING INDEX call_env_start_time_size_idx")),
            "{plan:?}"
        );
        assert!(
            plan.iter()
                .any(|step| step.contains("SEARCH store_map_entry")
                    && step.contains("org_id=? AND env_id=?")),
            "{plan:?}"
        );
    }

    #[tokio::test]
    async fn a_failed_check_leaves_the_rest_of_the_account_checked() {
        let db = super::super::test_db().await;
        let data = super::super::insert_test_data(&db).await.unwrap();
        let DatabasePool::Sqlite(pool) = &db else {
            unreachable!()
        };
        limit_runs(pool, data.org_id).await;
        let other = Uuid::new_v4();
        sqlx::query("INSERT INTO env(env_id,org_id,name,created_by_user_id) VALUES(?,?,'other',?)")
            .bind(other)
            .bind(data.org_id)
            .bind(data.user_id)
            .execute(pool)
            .await
            .unwrap();
        let (first, last) = if data.env_id < other {
            (data.env_id, other)
        } else {
            (other, data.env_id)
        };
        for env in [first, last] {
            sqlx::query("INSERT INTO usage_storage(env_id,org_id,slot,store_bytes) VALUES(?,?,0,99) ON CONFLICT(env_id,slot) DO UPDATE SET store_bytes=store_bytes+99")
                .bind(env).bind(data.org_id).execute(pool).await.unwrap();
        }
        let period = DateTime::parse_from_rfc3339("2000-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let mut tx = pool.begin().await.unwrap();
        run_usage_sqlite(&mut tx, data.org_id, period)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        sqlx::query("UPDATE usage_run SET admitted=admitted+10 WHERE root_org_id=?")
            .bind(data.org_id)
            .execute(pool)
            .await
            .unwrap();
        // The first environment's repair fails on every attempt.
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
            "CREATE TRIGGER refuse_repair BEFORE INSERT ON usage_storage WHEN NEW.env_id=x'{}' BEGIN SELECT RAISE(ABORT,'repair refused'); END",
            first.simple()
        )))
        .execute(pool)
        .await
        .unwrap();
        let error = reconcile_account(&db, data.org_id).await.unwrap_err();
        assert!(error.to_string().contains("repair refused"), "{error}");
        // Everything after it was still checked and repaired.
        storage_truth(pool, last).await;
        let (counted, admitted): (i64, i64) = sqlx::query_as("SELECT admitted,(SELECT COUNT(*) FROM run_admission WHERE root_org_id=?1) FROM usage_run WHERE root_org_id=?1")
            .bind(data.org_id)
            .fetch_one(pool)
            .await
            .unwrap();
        assert_eq!(counted, admitted);
        let unrepaired: i64 =
            sqlx::query_scalar("SELECT store_bytes FROM usage_storage WHERE env_id=? AND slot=0")
                .bind(first)
                .fetch_one(pool)
                .await
                .unwrap();
        assert_eq!(unrepaired, 99);
        // The next tick repairs it, and only it.
        sqlx::raw_sql("DROP TRIGGER refuse_repair")
            .execute(pool)
            .await
            .unwrap();
        assert_eq!(
            reconcile_account(&db, data.org_id).await.unwrap(),
            UsageDrift {
                storage_rows: 1,
                task_rows: 0,
                run_rows: 0
            }
        );
        storage_truth(pool, first).await;
    }

    #[test]
    fn a_failure_outside_the_check_ends_the_pass() {
        // A lost connection or exhausted pool would fail every remaining check.
        let mut failed = FailedChecks::default();
        assert!(matches!(
            failed.note("storage", Uuid::nil(), Err(sqlx::Error::PoolTimedOut)),
            Err(sqlx::Error::PoolTimedOut)
        ));
        assert!(failed.0.is_none());
        assert!(matches!(
            failed.note("storage", Uuid::nil(), Ok(true)),
            Ok(true)
        ));
    }

    #[tokio::test]
    async fn reconciliation_finds_lost_counters_and_ones_a_deleted_environment_left() {
        let db = super::super::test_db().await;
        let data = super::super::insert_test_data(&db).await.unwrap();
        let DatabasePool::Sqlite(pool) = &db else {
            unreachable!()
        };
        limit_runs(pool, data.org_id).await;
        // A store entry whose counter row is gone is found through its environment.
        sqlx::query("INSERT INTO store_map_entry(org_id,env_id,store_name,key,value,size) VALUES(?,?,'kept','\"key\"','\"value\"',41)")
            .bind(data.org_id)
            .bind(data.env_id)
            .execute(pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM usage_storage WHERE env_id=?")
            .bind(data.env_id)
            .execute(pool)
            .await
            .unwrap();
        // A counter row can outlive its environment, unlike a store entry.
        let gone = Uuid::new_v4();
        sqlx::query("INSERT INTO usage_storage(env_id,org_id,slot,store_bytes,call_bytes) VALUES(?,?,0,9,4)")
            .bind(gone)
            .bind(data.org_id)
            .execute(pool)
            .await
            .unwrap();
        assert_eq!(
            reconcile_account(&db, data.org_id)
                .await
                .unwrap()
                .storage_rows,
            2
        );
        assert_eq!(storage_truth(pool, gone).await, (0, 0));
        assert_eq!(storage_truth(pool, data.env_id).await.0, 41);
        assert!(
            reconcile_account(&db, data.org_id)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn account_task_usage_reads_the_counters_admission_enforces() {
        let db = super::super::test_db().await;
        let data = super::super::insert_test_data(&db).await.unwrap();
        let DatabasePool::Sqlite(pool) = &db else {
            unreachable!()
        };
        let period = Utc::now() - chrono::Duration::days(1);
        for (stop, units) in [
            (Utc::now(), 7),
            (period - chrono::Duration::days(3), 100),
            (period + chrono::Duration::hours(1), 5),
        ] {
            sqlx::query("INSERT INTO task(task_id,env_id,stream_id,build_id,function_name,task_type,task_status_id,result,duration_ms,stop_time) VALUES(?,?,?,?,'::counter/task','container',3,?,1000,?)")
                .bind(Uuid::now_v7()).bind(data.env_id).bind(data.stream_id).bind(data.build_id)
                .bind(serde_json::json!({"compute-units":units}).to_string()).bind(stop)
                .execute(pool).await.unwrap();
        }
        // The first read moves the counters onto the period; a task stopped
        // before it counts for neither figure.
        assert_eq!(
            account_task_usage(&db, &data.org_id, period).await.unwrap(),
            Some(AccountTaskUsage {
                compute_units: 12,
                duration_ms: 2000
            })
        );
        let mut tx = pool.begin().await.unwrap();
        let admitted = task_usage_sqlite(&mut tx, data.org_id, period)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        assert_eq!((admitted.2, admitted.4), (12, 2000));
        // A later period recounts rather than carrying the old figures over.
        assert_eq!(
            account_task_usage(&db, &data.org_id, Utc::now() + chrono::Duration::days(1))
                .await
                .unwrap(),
            Some(AccountTaskUsage {
                compute_units: 0,
                duration_ms: 0
            })
        );
    }

    type CounterRow = (
        i64,
        i64,
        Option<DateTime<Utc>>,
        Option<DateTime<Utc>>,
        i64,
        i64,
    );

    /// A disposable account on the privileged PostgreSQL fixture, whose role
    /// bypasses RLS: `postgres_rls` covers these statements as a restricted login.
    struct PgAccount {
        raw: sqlx::PgPool,
        db: DatabasePool,
        billing: ScopedPgPool,
        root: Uuid,
        env: Uuid,
        stream: Uuid,
        build: Uuid,
        period: DateTime<Utc>,
    }

    impl PgAccount {
        async fn new() -> Option<Self> {
            use sqlx::Executor;
            let Ok(uri) = std::env::var("HOT_TEST_POSTGRES_URI") else {
                eprintln!("skipping: HOT_TEST_POSTGRES_URI not set");
                return None;
            };
            let conf =
                crate::val!({"uri": uri.clone(), "schema": "hot", "rls": {"enforce": false}});
            crate::db::run_migrations(&conf).await.unwrap();
            let raw = sqlx::postgres::PgPoolOptions::new()
                .max_connections(4)
                .after_connect(|conn, _| {
                    Box::pin(async move {
                        // Task writers here fail rather than wait out a recount.
                        conn.execute("SET search_path = hot, public; SET lock_timeout = '1s'")
                            .await?;
                        Ok(())
                    })
                })
                .connect(&uri)
                .await
                .unwrap();
            let (root, env, user, project, build, stream) = (
                Uuid::now_v7(),
                Uuid::now_v7(),
                Uuid::now_v7(),
                Uuid::now_v7(),
                Uuid::now_v7(),
                Uuid::now_v7(),
            );
            sqlx::query(r#"INSERT INTO "user"(user_id,email,created_by_user_id) VALUES($1,$2,$1)"#)
                .bind(user)
                .bind(format!("{user}@rollover.test"))
                .execute(&raw)
                .await
                .unwrap();
            sqlx::query("INSERT INTO org(org_id,name,slug,org_type,created_by_user_id) VALUES($1,'Rollover',$2,'organization',$3)")
                .bind(root).bind(root.to_string()).bind(user).execute(&raw).await.unwrap();
            sqlx::query("INSERT INTO env(env_id,org_id,name,created_by_user_id) VALUES($1,$2,'development',$3)")
                .bind(env).bind(root).bind(user).execute(&raw).await.unwrap();
            sqlx::query("INSERT INTO project(project_id,env_id,name,created_by_user_id) VALUES($1,$2,'rollover',$3)")
                .bind(project).bind(env).bind(user).execute(&raw).await.unwrap();
            sqlx::query("INSERT INTO build(build_id,project_id,hash,size,created_by_user_id) VALUES($1,$2,'rollover',0,$3)")
                .bind(build).bind(project).bind(user).execute(&raw).await.unwrap();
            sqlx::query("INSERT INTO stream(stream_id,env_id) VALUES($1,$2)")
                .bind(stream)
                .bind(env)
                .execute(&raw)
                .await
                .unwrap();
            let db = crate::db::create_db_pool(&conf).await.unwrap();
            let DatabasePool::Postgres(pool) = &db else {
                unreachable!()
            };
            let billing = pool.with_scope(DatabaseScope::Maintenance {
                operation: MaintenanceOperation::Billing,
                org_id: Some(root),
            });
            Some(Self {
                raw,
                db,
                billing,
                root,
                env,
                stream,
                build,
                period: Utc::now() - chrono::Duration::days(1),
            })
        }

        /// Reconciliation only checks accounts with an enforced limit.
        async fn limit_runs(&self) {
            sqlx::query(
                "UPDATE org SET features='{\"runs_per_month\":1000}'::jsonb WHERE org_id=$1",
            )
            .bind(self.root)
            .execute(&self.raw)
            .await
            .unwrap();
        }

        async fn run(&self) -> Uuid {
            let id = Uuid::now_v7();
            sqlx::query("INSERT INTO run(run_id,env_id,stream_id,build_id,run_type_id) VALUES($1,$2,$3,$4,1)")
                .bind(id).bind(self.env).bind(self.stream).bind(self.build)
                .execute(&self.raw).await.unwrap();
            id
        }

        async fn reconcile(&self) -> UsageDrift {
            reconcile_account(&DatabasePool::Postgres(self.billing.clone()), self.root)
                .await
                .unwrap()
        }

        /// The account's storage counter and the store entries it stands for.
        async fn store_bytes(&self) -> (i64, i64) {
            sqlx::query_as("SELECT (SELECT COALESCE(SUM(store_bytes),0)::bigint FROM usage_storage WHERE env_id=$1),(SELECT COALESCE(SUM(GREATEST(size,0)),0)::bigint FROM store_map_entry WHERE env_id=$1)")
                .bind(self.env)
                .fetch_one(&self.raw)
                .await
                .unwrap()
        }

        /// The account's storage counter and the call rows it stands for.
        async fn call_bytes(&self) -> (i64, i64) {
            sqlx::query_as("SELECT (SELECT COALESCE(SUM(call_bytes),0)::bigint FROM usage_storage WHERE env_id=$1),(SELECT COALESCE(SUM(GREATEST(size,0)),0)::bigint FROM call WHERE env_id=$1)")
                .bind(self.env)
                .fetch_one(&self.raw)
                .await
                .unwrap()
        }

        async fn finished_task(&self, units: i64, stop: DateTime<Utc>) -> Uuid {
            let id = Uuid::now_v7();
            sqlx::query("INSERT INTO task(task_id,env_id,stream_id,build_id,function_name,task_type,task_status_id,result,duration_ms,stop_time) VALUES($1,$2,$3,$4,'::rollover/task','container',3,$5,1000,$6)")
                .bind(id).bind(self.env).bind(self.stream).bind(self.build)
                .bind(serde_json::json!({"compute-units":units})).bind(stop)
                .execute(&self.raw).await.unwrap();
            id
        }

        /// Put the counter on a past period with a figure a recount must replace.
        async fn age_counter(&self) {
            sqlx::query("INSERT INTO usage_task(env_id,org_id,period_start,compute_units) VALUES($1,$2,$3,999) ON CONFLICT(env_id) DO UPDATE SET period_start=excluded.period_start,compute_units=excluded.compute_units")
                .bind(self.env).bind(self.root).bind(self.period - chrono::Duration::days(30))
                .execute(&self.raw).await.unwrap();
        }

        async fn start(&self) -> RolloverStart {
            PeriodRollover::start(&self.billing, self.env, self.root, self.period)
                .await
                .unwrap()
        }

        /// The task table's answer for the period.
        async fn truth(&self) -> (i64, i64) {
            sqlx::query_as(
                "SELECT COALESCE(SUM(f.compute_units),0)::bigint,COALESCE(SUM(f.duration_ms),0)::bigint FROM task t CROSS JOIN LATERAL hot.task_usage(t) f WHERE t.env_id=$1 AND f.usage_at >= $2",
            )
            .bind(self.env)
            .bind(self.period)
            .fetch_one(&self.raw)
            .await
            .unwrap()
        }

        async fn assert_counter_is_true(&self) {
            let row: CounterRow =
                sqlx::query_as("SELECT compute_units,duration_ms,period_start,rollover_period,rollover_compute_units,rollover_duration_ms FROM usage_task WHERE env_id=$1")
                    .bind(self.env)
                    .fetch_one(&self.raw)
                    .await
                    .unwrap();
            assert_eq!((row.0, row.1), self.truth().await);
            // PostgreSQL stores microseconds; the period keeps nanoseconds here.
            assert_eq!(
                row.2.map(|at| at.timestamp_micros()),
                Some(self.period.timestamp_micros())
            );
            assert_eq!((row.3, row.4, row.5), (None, 0, 0));
        }
    }

    #[tokio::test]
    async fn postgres_rollover_counts_each_concurrent_writer_once_without_blocking_it() {
        let Some(account) = PgAccount::new().await else {
            return;
        };
        let (now, before) = (Utc::now(), account.period - chrono::Duration::days(5));
        let kept = account.finished_task(7, now).await;
        let removed = account.finished_task(11, now).await;
        let moved_in = account.finished_task(17, before).await;
        let running = Uuid::now_v7();
        sqlx::query("INSERT INTO task(task_id,env_id,stream_id,build_id,function_name,task_type,task_status_id,reserved_compute_units,reserved_duration_ms,start_time) VALUES($1,$2,$3,$4,'::rollover/task','container',2,100,1000,now())")
            .bind(running).bind(account.env).bind(account.stream).bind(account.build)
            .execute(&account.raw).await.unwrap();
        account.age_counter().await;
        let RolloverStart::Started(mut rollover) = account.start().await else {
            panic!("the recount should start");
        };
        // Readers still see the old period, so they never read a partial count.
        let period: Option<DateTime<Utc>> =
            sqlx::query_scalar("SELECT period_start FROM usage_task WHERE env_id=$1")
                .bind(account.env)
                .fetch_one(&account.raw)
                .await
                .unwrap();
        assert!(period.unwrap() < account.period);
        // Every kind of writer runs to completion mid-recount, under a short
        // lock timeout: none of them waits for the recount.
        sqlx::query("UPDATE task SET result=$2 WHERE task_id=$1")
            .bind(kept)
            .bind(serde_json::json!({"compute-units":3}))
            .execute(&account.raw)
            .await
            .unwrap();
        sqlx::query("DELETE FROM task WHERE task_id=$1")
            .bind(removed)
            .execute(&account.raw)
            .await
            .unwrap();
        sqlx::query("UPDATE task SET stop_time=now() WHERE task_id=$1")
            .bind(moved_in)
            .execute(&account.raw)
            .await
            .unwrap();
        sqlx::query("UPDATE task SET task_status_id=3,stop_time=now(),duration_ms=400,result=$2 WHERE task_id=$1")
            .bind(running)
            .bind(serde_json::json!({"compute-units":5}))
            .execute(&account.raw)
            .await
            .unwrap();
        account.finished_task(19, now).await;
        // This one finished outside the period, so it must add nothing.
        account.finished_task(23, before).await;
        let totals = rollover.totals().await.unwrap();
        assert_eq!(totals, (18, 2000), "as of the snapshot, before any writer");
        rollover.finish(&account.billing, totals).await.unwrap();
        account.assert_counter_is_true().await;
        assert_eq!(account.truth().await.0, 44);
        // Later writers count against the new period as usual.
        account.finished_task(29, now).await;
        account.assert_counter_is_true().await;
        assert_eq!(
            account_task_usage(&account.db, &account.root, account.period)
                .await
                .unwrap(),
            Some(AccountTaskUsage {
                compute_units: 73,
                duration_ms: account.truth().await.1
            })
        );
    }

    /// One writer's calls and store entries. Every kind of change a trace or
    /// store writer or retention makes, each in its own transaction, so
    /// reconciliation runs between them.
    async fn write_usage(
        raw: sqlx::PgPool,
        org: Uuid,
        env: Uuid,
        run: Uuid,
        stop: Arc<AtomicBool>,
    ) -> usize {
        let store = Uuid::now_v7().to_string();
        let mut writes = 0;
        let mut sizes = 0_i64;
        while !stop.load(Ordering::Relaxed) {
            let call = Uuid::now_v7();
            sizes += 1;
            sqlx::query("INSERT INTO store_map_entry(org_id,env_id,store_name,key,value,size) VALUES($1,$2,$3,$4,'0',$5) ON CONFLICT(org_id,env_id,store_name,key) DO UPDATE SET size=excluded.size")
                .bind(org).bind(env).bind(&store).bind(serde_json::json!(sizes % 5)).bind(sizes % 90 + 10)
                .execute(&raw).await.unwrap();
            sqlx::query("INSERT INTO call(call_id,run_id,env_id,function_name,static_scope,runtime_path,call_depth,start_time,size) VALUES($1,$2,$3,'::counter/test','root','root',0,now(),$4)")
                .bind(call).bind(run).bind(env).bind(sizes % 90 + 10).execute(&raw).await.unwrap();
            sqlx::query("UPDATE call SET size=size+7 WHERE call_id=$1")
                .bind(call)
                .execute(&raw)
                .await
                .unwrap();
            if sizes % 3 == 0 {
                sqlx::query("DELETE FROM call WHERE call_id=$1")
                    .bind(call)
                    .execute(&raw)
                    .await
                    .unwrap();
            }
            writes += 1;
        }
        writes
    }

    #[tokio::test]
    async fn postgres_storage_reconcile_is_exact_under_concurrent_trace_writers() {
        let Some(account) = PgAccount::new().await else {
            return;
        };
        account.limit_runs().await;
        let run = account.run().await;
        // Vacuumed history below whatever the writers add, as on a real table,
        // so the check reads some of it from the index alone.
        for _ in 0..50 {
            sqlx::query("INSERT INTO call(call_id,run_id,env_id,function_name,static_scope,runtime_path,call_depth,start_time,size) VALUES($1,$2,$3,'::counter/test','root','root',0,now(),1000)")
                .bind(Uuid::now_v7()).bind(run).bind(account.env).execute(&account.raw).await.unwrap();
        }
        sqlx::query("VACUUM hot.call")
            .execute(&account.raw)
            .await
            .unwrap();
        assert!(account.reconcile().await.is_empty());
        let stop = Arc::new(AtomicBool::new(false));
        let writers: Vec<_> = (0..3)
            .map(|_| {
                tokio::spawn(write_usage(
                    account.raw.clone(),
                    account.root,
                    account.env,
                    run,
                    stop.clone(),
                ))
            })
            .collect();
        tokio::time::sleep(Duration::from_millis(50)).await;
        // There is no drift yet. A check that mixed snapshots would report a
        // writer as drift, and its repair would then create some.
        for _ in 0..20 {
            assert_eq!(account.reconcile().await, UsageDrift::default());
        }
        sqlx::query("UPDATE usage_storage SET call_bytes=call_bytes+73,store_bytes=store_bytes+31 WHERE env_id=$1 AND slot=(SELECT MIN(slot) FROM usage_storage WHERE env_id=$1)")
            .bind(account.env)
            .execute(&account.raw)
            .await
            .unwrap();
        assert_eq!(account.reconcile().await.storage_rows, 1);
        for _ in 0..20 {
            assert_eq!(account.reconcile().await, UsageDrift::default());
        }
        stop.store(true, Ordering::Relaxed);
        let mut writes = 0;
        for writer in writers {
            writes += writer.await.unwrap();
        }
        assert!(writes > 0, "reconciliation should have run beside writers");
        let (counter, calls) = account.call_bytes().await;
        assert_eq!(counter, calls);
        assert!(calls >= 50_000);
        let (counter, stored) = account.store_bytes().await;
        assert_eq!(counter, stored);
        assert!(stored > 0);
    }

    /// Reconciliation lists an account's environments without reading its store
    /// entries because of this.
    #[tokio::test]
    async fn postgres_store_entries_cannot_outlive_their_environment() {
        let Some(account) = PgAccount::new().await else {
            return;
        };
        let env = Uuid::now_v7();
        sqlx::query("INSERT INTO env(env_id,org_id,name,created_by_user_id) SELECT $1,$2,'store',created_by_user_id FROM env WHERE env_id=$3")
            .bind(env).bind(account.root).bind(account.env).execute(&account.raw).await.unwrap();
        sqlx::query("INSERT INTO store_map_entry(org_id,env_id,store_name,key,value) VALUES($1,$2,'kept','\"key\"','\"value\"')")
            .bind(account.root).bind(env).execute(&account.raw).await.unwrap();
        let error = sqlx::query("DELETE FROM env WHERE env_id=$1")
            .bind(env)
            .execute(&account.raw)
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("store_map_entry_env_id_fkey"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn postgres_storage_check_reads_through_indexes() {
        let Some(account) = PgAccount::new().await else {
            return;
        };
        let run = account.run().await;
        for size in [100, -5, 300] {
            sqlx::query("INSERT INTO call(call_id,run_id,env_id,function_name,static_scope,runtime_path,call_depth,start_time,size) VALUES($1,$2,$3,'::counter/test','root','root',0,now(),$4)")
                .bind(Uuid::now_v7()).bind(run).bind(account.env).bind(size).execute(&account.raw).await.unwrap();
            sqlx::query("INSERT INTO store_map_entry(org_id,env_id,store_name,key,value,size) VALUES($1,$2,'plan',$3,'0',$4)")
                .bind(account.root).bind(account.env).bind(serde_json::json!(size)).bind(size).execute(&account.raw).await.unwrap();
        }
        let mut tx = account.raw.begin().await.unwrap();
        // The test tables are too small for the planner to prefer any index.
        sqlx::raw_sql("SET LOCAL enable_seqscan=off; SET LOCAL enable_bitmapscan=off")
            .execute(&mut *tx)
            .await
            .unwrap();
        let plan: Vec<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF) {STORAGE_CHECK}"
        )))
        .bind(account.env)
        .bind(account.root)
        .fetch_all(&mut *tx)
        .await
        .unwrap();
        assert!(
            plan.iter()
                .any(|line| line
                    .contains("Index Only Scan using call_env_start_time_size_idx on call")),
            "{plan:#?}"
        );
        // One descent through the key, not one for each organization.
        let store = plan
            .iter()
            .position(|line| line.contains("Scan") && line.contains("on store_map_entry"))
            .unwrap_or_else(|| panic!("{plan:#?}"));
        let details = plan[store..].iter().take(5).cloned().collect::<Vec<_>>();
        assert!(
            details
                .iter()
                .any(|line| line.contains("org_id =") && line.contains("env_id =")),
            "{plan:#?}"
        );
        assert!(
            details
                .iter()
                .any(|line| line.contains("Index Searches: 1")),
            "{plan:#?}"
        );
        let (stored, calls, _, counted): (i64, i64, i64, i64) = sqlx::query_as(STORAGE_CHECK)
            .bind(account.env)
            .bind(account.root)
            .fetch_one(&mut *tx)
            .await
            .unwrap();
        assert_eq!(
            (stored, calls, counted),
            (400, 400, 400),
            "negative sizes count as zero"
        );
        tx.rollback().await.unwrap();
    }

    #[tokio::test]
    async fn postgres_abandoned_rollover_starts_over() {
        let Some(account) = PgAccount::new().await else {
            return;
        };
        let task = account.finished_task(7, Utc::now()).await;
        account.age_counter().await;
        let RolloverStart::Started(abandoned) = account.start().await else {
            panic!("the recount should start");
        };
        // A second recount cannot start: its mark would discard these changes.
        assert!(matches!(account.start().await, RolloverStart::Deferred));
        sqlx::query("UPDATE task SET result=$2 WHERE task_id=$1")
            .bind(task)
            .bind(serde_json::json!({"compute-units":3}))
            .execute(&account.raw)
            .await
            .unwrap();
        abandoned.snapshot.rollback().await.unwrap();
        // The next recount discards the changes the abandoned one counted,
        // because its own snapshot already holds them.
        assert!(
            prepare_task_period(&account.db, account.root, account.period)
                .await
                .unwrap()
        );
        account.assert_counter_is_true().await;
        assert_eq!(account.truth().await.0, 3);
    }

    #[tokio::test]
    async fn postgres_rollover_retakes_its_snapshot_after_a_writer_in_flight() {
        let Some(account) = PgAccount::new().await else {
            return;
        };
        let task = account.finished_task(7, Utc::now()).await;
        account.age_counter().await;
        // This writer holds the counter row, uncommitted, so the snapshot taken
        // now cannot see it and the mark must not trust that snapshot.
        let mut writer = account.raw.begin().await.unwrap();
        sqlx::query("UPDATE task SET result=$2 WHERE task_id=$1")
            .bind(task)
            .bind(serde_json::json!({"compute-units":3}))
            .execute(&mut *writer)
            .await
            .unwrap();
        let (billing, env, root, period) = (
            account.billing.clone(),
            account.env,
            account.root,
            account.period,
        );
        let starting =
            tokio::spawn(async move { PeriodRollover::start(&billing, env, root, period).await });
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!starting.is_finished(), "the mark waits for the writer");
        writer.commit().await.unwrap();
        let RolloverStart::Started(mut rollover) = starting.await.unwrap().unwrap() else {
            panic!("the recount should start");
        };
        let totals = rollover.totals().await.unwrap();
        assert_eq!(totals.0, 3, "the new snapshot holds the writer");
        rollover.finish(&account.billing, totals).await.unwrap();
        account.assert_counter_is_true().await;
    }

    #[tokio::test]
    async fn postgres_admission_defers_rather_than_recounting_a_stale_counter() {
        let Some(account) = PgAccount::new().await else {
            return;
        };
        account.finished_task(7, Utc::now()).await;
        account.age_counter().await;
        let mut tx = account.raw.begin().await.unwrap();
        assert!(matches!(
            task_usage_postgres(&mut tx, account.root, account.period).await,
            Err(sqlx::Error::Protocol(_))
        ));
        tx.rollback().await.unwrap();
        assert!(
            prepare_task_period(&account.db, account.root, account.period)
                .await
                .unwrap()
        );
        let mut tx = account.raw.begin().await.unwrap();
        let totals = task_usage_postgres(&mut tx, account.root, account.period)
            .await
            .unwrap();
        tx.rollback().await.unwrap();
        assert_eq!(totals.2, 7);
    }

    #[tokio::test]
    async fn postgres_deleted_environment_counter_cannot_block_admission() {
        let Some(account) = PgAccount::new().await else {
            return;
        };
        let deleted = Uuid::now_v7();
        sqlx::query("INSERT INTO usage_task(env_id,org_id,compute_units) VALUES($1,$2,5)")
            .bind(deleted)
            .bind(account.root)
            .execute(&account.raw)
            .await
            .unwrap();
        assert!(
            prepare_task_period(&account.db, account.root, account.period)
                .await
                .unwrap()
        );
        let mut tx = account.raw.begin().await.unwrap();
        assert!(
            task_usage_postgres(&mut tx, account.root, account.period)
                .await
                .is_ok()
        );
        tx.rollback().await.unwrap();
        sqlx::query("DELETE FROM usage_task WHERE env_id=$1")
            .bind(deleted)
            .execute(&account.raw)
            .await
            .unwrap();
    }
}

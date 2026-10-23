//! Atomic account storage admission. The privileged transaction never escapes a
//! Store write and is narrowed back to its verified environment before mutation.
use crate::db::{DatabasePool, DatabaseScope, Env, Features, MaintenanceOperation, Org};
use serde_json::Value;
use uuid::Uuid;

#[derive(Clone, Copy)]
pub(crate) enum QuotaKind {
    Store,
    Call,
}
impl QuotaKind {
    fn key(self) -> &'static str {
        match self {
            Self::Store => "store_storage_bytes",
            Self::Call => "call_storage_bytes",
        }
    }
    fn usage_sql(self) -> &'static str {
        match self {
            Self::Store => {
                "SELECT CAST(COALESCE(SUM(s.store_bytes),0) AS BIGINT) FROM usage_storage s JOIN org o ON o.org_id=s.org_id WHERE o.org_id=$1 OR o.parent_org_id=$1"
            }
            Self::Call => {
                "SELECT CAST(COALESCE(SUM(c.call_bytes),0) AS BIGINT) FROM usage_storage c JOIN org o ON o.org_id=c.org_id WHERE o.org_id=$1 OR o.parent_org_id=$1"
            }
        }
    }
}

pub(crate) struct Budget {
    used: i64,
    limit: i64,
    root_id: Uuid,
    org_id: Uuid,
    env_id: Uuid,
    kind: QuotaKind,
}
impl Budget {
    fn check_total(&self, total: i64) -> Result<(), String> {
        if total < 0 {
            return Err("Invalid storage accounting".into());
        }
        if total > self.used && self.limit >= 0 && total > self.limit {
            return Err(format!("Account {} limit reached", self.kind.key()));
        }
        Ok(())
    }
    /// Read counters after merged call UPSERT payloads under the held root lock, restoring
    /// exact environment scope before returning. The caller commits only on Ok.
    pub(crate) async fn check_postgres(&self, conn: &mut sqlx::PgConnection) -> Result<(), String> {
        if self.limit < 0 {
            return Ok(());
        }
        DatabaseScope::Maintenance {
            operation: MaintenanceOperation::Billing,
            org_id: Some(self.root_id),
        }
        .apply_to_connection(conn)
        .await
        .map_err(|e| e.to_string())?;
        let total = sqlx::query_scalar(sqlx::AssertSqlSafe(self.kind.usage_sql()))
            .bind(self.root_id)
            .fetch_one(&mut *conn)
            .await
            .map_err(|e| e.to_string());
        narrow_postgres(conn, self.org_id, self.env_id).await?;
        self.check_total(total?)
    }
    pub(crate) async fn check_sqlite(
        &self,
        conn: &mut sqlx::SqliteConnection,
    ) -> Result<(), String> {
        if self.limit < 0 {
            return Ok(());
        }
        let total = sqlx::query_scalar(sqlx::AssertSqlSafe(self.kind.usage_sql()))
            .bind(self.root_id)
            .fetch_one(&mut *conn)
            .await
            .map_err(|e| e.to_string())?;
        self.check_total(total)
    }

    pub fn replace(&mut self, old: i64, new: i64) -> Result<(), String> {
        if old < 0 || new < 0 {
            return Err("Invalid store storage accounting".into());
        }
        // Unlimited accounts skip admission reads; triggers still maintain usage.
        if self.limit < 0 {
            return Ok(());
        }
        let next = self
            .used
            .checked_sub(old)
            .and_then(|v| v.checked_add(new))
            .filter(|v| *v >= 0)
            .ok_or("Store storage accounting overflow")?;
        if new > old && self.limit >= 0 && next > self.limit {
            return Err(format!("Account {} limit reached", self.kind.key()));
        }
        self.used = next;
        Ok(())
    }
}

pub(super) fn entry_size(
    key: usize,
    value: usize,
    text: usize,
    embedding_bytes: usize,
) -> Result<i64, String> {
    key.checked_add(value)
        .and_then(|n| n.checked_add(text))
        .and_then(|n| n.checked_add(embedding_bytes))
        .and_then(|n| i64::try_from(n).ok())
        .ok_or_else(|| "Store entry is too large".into())
}

async fn root_for(db: &DatabasePool, org: Uuid, env: Uuid) -> Result<Uuid, String> {
    if let DatabasePool::Postgres(pool) = db
        && !matches!(pool.scope().map_err(|e| e.to_string())?, DatabaseScope::Environment { org_id, env_id } if org_id==org && env_id==env)
    {
        return Err("Store write requires its exact environment scope".into());
    }
    let owner = Env::get_env_uncached(db, &env)
        .await
        .map_err(|e| e.to_string())?;
    if owner.org_id != org {
        return Err("Store environment ownership mismatch".into());
    }
    Org::billing_owner_id_for(db, &org)
        .await
        .map_err(|e| e.to_string())
}

fn limit(plan: Option<String>, overrides: Option<String>, kind: QuotaKind) -> Result<i64, String> {
    let parse = |raw: String| -> Result<Value, String> {
        let value: Value =
            serde_json::from_str(&raw).map_err(|_| "Malformed account storage limits")?;
        if !value.is_object()
            || value
                .get(kind.key())
                .is_some_and(|n| n.as_i64().is_none_or(|limit| limit < -1))
        {
            return Err("Malformed account storage limits".into());
        }
        Ok(value)
    };
    let plan = plan.map(parse).transpose()?;
    let overrides = overrides.map(parse).transpose()?;
    if plan.is_none() && overrides.is_none() {
        return Ok(-1);
    }
    Features::resolve(plan.as_ref(), overrides.as_ref())
        .get_i64(kind.key())
        .ok_or_else(|| "Missing account store limit".into())
}

macro_rules! begin {
    ($name:ident,$database:ty,$variant:ident,$begin:expr,$lock:literal,$narrow:expr) => {
        pub(crate) async fn $name(db:&DatabasePool,org:Uuid,env:Uuid,kind:QuotaKind)
            -> Result<(sqlx::Transaction<'static,$database>,Budget),String> {
            let root=root_for(db,org,env).await?;
            let billing=db.with_scope(DatabaseScope::Maintenance { operation:MaintenanceOperation::Billing,org_id:Some(root) });
            let DatabasePool::$variant(pool)=&billing else { return Err("Incorrect store database backend".into()); };
            let mut tx=$begin(pool).await.map_err(|e| e.to_string())?;
            let overrides:Option<String>=sqlx::query_scalar("SELECT CAST(features AS TEXT) FROM org WHERE org_id=$1")
                .bind(root).fetch_one(&mut *tx).await.map_err(|e|e.to_string())?;
            let plan:Option<String>=sqlx::query_scalar("SELECT CAST(COALESCE(p.features,'{}') AS TEXT) FROM org_plan op JOIN plan p ON p.plan_uuid=op.plan_uuid WHERE op.org_id=$1")
                .bind(root).fetch_optional(&mut *tx).await.map_err(|e|e.to_string())?;
            let limit=limit(plan,overrides,kind)?;
            // Unlimited accounts need neither the sibling-serializing account
            // lock nor a recount of every stored byte in the account.
            let used:i64=if limit<0 { 0 } else {
                sqlx::query($lock).bind(root).execute(&mut *tx).await.map_err(|e|e.to_string())?;
                sqlx::query_scalar(sqlx::AssertSqlSafe(kind.usage_sql()))
                    .bind(root).fetch_one(&mut *tx).await.map_err(|e|e.to_string())?
            };
            $narrow(&mut tx,org,env).await?;
            Ok((tx,Budget {used,limit,root_id:root,org_id:org,env_id:env,kind}))
        }
    };
}

async fn narrow_postgres(
    tx: &mut sqlx::PgConnection,
    org_id: Uuid,
    env_id: Uuid,
) -> Result<(), String> {
    DatabaseScope::Environment { org_id, env_id }
        .apply_to_connection(tx)
        .await
        .map_err(|e| e.to_string())
}
async fn narrow_sqlite(_: &mut sqlx::SqliteConnection, _: Uuid, _: Uuid) -> Result<(), String> {
    Ok(())
}
async fn begin_postgres_tx(
    pool: &crate::db::ScopedPgPool,
) -> Result<sqlx::Transaction<'static, sqlx::Postgres>, sqlx::Error> {
    pool.begin().await
}
/// Admission reads before it writes. A deferred SQLite transaction that has
/// read cannot wait for the writer lock: its first write fails at once with
/// SQLITE_BUSY instead of honoring busy_timeout. Take the writer lock up front.
async fn begin_sqlite_tx(
    pool: &sqlx::SqlitePool,
) -> Result<sqlx::Transaction<'static, sqlx::Sqlite>, sqlx::Error> {
    pool.begin_with("BEGIN IMMEDIATE").await
}
begin!(
    begin_postgres,
    sqlx::Postgres,
    Postgres,
    begin_postgres_tx,
    "SELECT org_id FROM org WHERE org_id=$1 FOR UPDATE",
    narrow_postgres
);
begin!(
    begin_sqlite,
    sqlx::Sqlite,
    Sqlite,
    begin_sqlite_tx,
    "UPDATE org SET org_id=org_id WHERE org_id=$1",
    narrow_sqlite
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn storage_limits_allow_only_minus_one_for_unlimited() {
        for kind in [QuotaKind::Store, QuotaKind::Call] {
            let encoded = |number: i64| Some(serde_json::json!({(kind.key()):number}).to_string());
            assert_eq!(limit(None, None, kind).unwrap(), -1);
            assert_eq!(limit(None, encoded(-1), kind).unwrap(), -1);
            assert_eq!(limit(None, encoded(0), kind).unwrap(), 0);
            assert!(limit(None, encoded(-2), kind).is_err());
            assert!(
                limit(
                    None,
                    Some(serde_json::json!({(kind.key()):"unlimited"}).to_string()),
                    kind
                )
                .is_err()
            );
            assert_eq!(limit(None, Some("{}".into()), kind).unwrap(), 0);
        }
    }

    #[tokio::test]
    async fn sqlite_admission_waits_for_another_writer_instead_of_failing() {
        let dir = tempfile::tempdir().unwrap();
        let conf =
            crate::val!({"uri": format!("sqlite:{}", dir.path().join("quota.db").display())});
        crate::db::run_migrations(&conf).await.unwrap();
        let db = crate::db::create_db_pool(&conf).await.unwrap();
        let DatabasePool::Sqlite(pool) = &db else {
            unreachable!()
        };
        let data = crate::db::insert_test_data(&db).await.unwrap();
        let (org, env) = (data.org_id, data.env_id);
        // Another connection holds the writer lock while admission starts.
        let mut holder = pool.begin_with("BEGIN IMMEDIATE").await.unwrap();
        sqlx::query("UPDATE env SET name=name WHERE env_id=?")
            .bind(env)
            .execute(&mut *holder)
            .await
            .unwrap();
        let release = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            holder.commit().await.unwrap();
        });
        let (mut tx, budget) = begin_sqlite(&db, org, env, QuotaKind::Store).await.unwrap();
        // The first write after admission's reads must not hit SQLITE_BUSY.
        sqlx::query("UPDATE org SET org_id=org_id WHERE org_id=?")
            .bind(org)
            .execute(&mut *tx)
            .await
            .unwrap();
        budget.check_sqlite(&mut tx).await.unwrap();
        tx.commit().await.unwrap();
        release.await.unwrap();
    }
}

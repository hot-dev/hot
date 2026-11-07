//! A pool executor that sets authority on the connection executing each query.
//! Never expose Deref<Target = PgPool>: that would silently bypass this boundary.

use futures::{TryStreamExt, future::BoxFuture, stream::BoxStream};
use sqlx::{Database, Either, Execute, Executor, PgPool, Postgres, SqlStr};

use super::scope::{BootstrapOperation, DatabaseScope, current_database_scope};

/// An owned scoped transaction. A cancelled or failed BEGIN+scope must close
/// its connection: SQLx's custom BEGIN tracks depth only after the whole SQL
/// string succeeds, so its transaction drop cannot roll back a partial BEGIN.
pub struct ScopedPgTransaction {
    connection: Option<sqlx::pool::PoolConnection<Postgres>>,
    open: bool,
}
impl std::ops::Deref for ScopedPgTransaction {
    type Target = sqlx::PgConnection;
    fn deref(&self) -> &Self::Target {
        self.connection.as_ref().expect("transaction connection")
    }
}
impl std::ops::DerefMut for ScopedPgTransaction {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.connection.as_mut().expect("transaction connection")
    }
}
impl ScopedPgTransaction {
    pub async fn commit(mut self) -> Result<(), sqlx::Error> {
        self.connection.as_mut().unwrap().execute("COMMIT").await?;
        self.open = false;
        Ok(())
    }
    pub async fn rollback(mut self) -> Result<(), sqlx::Error> {
        self.connection
            .as_mut()
            .unwrap()
            .execute("ROLLBACK")
            .await?;
        self.open = false;
        Ok(())
    }
}
impl Drop for ScopedPgTransaction {
    fn drop(&mut self) {
        if self.open {
            // Closing also rolls back on the server, including when a BEGIN
            // future was cancelled before receiving ReadyForQuery. Never let
            // a partially scoped session re-enter the connection pool.
            if let Some(mut connection) = self.connection.take() {
                connection.close_on_drop();
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct ScopedPgPool {
    pool: PgPool,
    scope: Option<DatabaseScope>,
}

impl ScopedPgPool {
    pub fn new(pool: PgPool) -> Self {
        Self { pool, scope: None }
    }

    pub fn with_scope(&self, scope: DatabaseScope) -> Self {
        Self {
            pool: self.pool.clone(),
            scope: Some(scope),
        }
    }

    pub fn scope(&self) -> Result<DatabaseScope, sqlx::Error> {
        let scope = self
            .scope
            .clone()
            .or_else(current_database_scope)
            .ok_or_else(|| {
                sqlx::Error::Protocol("Database operation requires an authorized scope".into())
            })?;
        scope.validate()?;
        Ok(scope)
    }

    pub async fn begin(&self) -> Result<ScopedPgTransaction, sqlx::Error> {
        self.begin_at_isolation("BEGIN").await
    }

    /// A stable snapshot must be selected before applying the billing scope,
    /// because applying scope itself reads environment ownership.
    pub async fn begin_repeatable_read(&self) -> Result<ScopedPgTransaction, sqlx::Error> {
        self.begin_at_isolation("BEGIN ISOLATION LEVEL REPEATABLE READ")
            .await
    }

    async fn begin_at_isolation(&self, begin: &str) -> Result<ScopedPgTransaction, sqlx::Error> {
        let scope = self.scope()?;
        // Values are trusted enums/UUIDs, escaped as SQL literals. Both
        // transaction opening and transaction-local authority use one exchange.
        let statement = if scope == DatabaseScope::Bootstrap(BootstrapOperation::Health) {
            begin.to_string()
        } else {
            let (mode, org, env, orgs, envs, operation, user) = scope.settings();
            let literals = [mode, org, env, orgs, envs, operation, user]
                .map(|value| format!("E'{}'", value.replace('\\', "\\\\").replace('\'', "''")));
            format!("{begin}; SELECT hot.apply_scope({})", literals.join(","))
        };
        let mut tx = ScopedPgTransaction {
            connection: Some(self.pool.acquire().await?),
            open: true,
        };
        (&mut *tx).execute(sqlx::AssertSqlSafe(statement)).await?;
        Ok(tx)
    }

    /// Run a unit of work with one scope application and one commit. An error
    /// or cancelled future rolls the transaction back; no unscoped pool escapes.
    pub async fn transaction<T, E, F>(&self, work: F) -> Result<T, E>
    where
        T: Send,
        E: From<sqlx::Error> + Send,
        F: for<'c> FnOnce(&'c mut sqlx::PgConnection) -> BoxFuture<'c, Result<T, E>> + Send,
    {
        let mut tx = self.begin().await?;
        let result = work(&mut tx).await?;
        tx.commit().await?;
        Ok(result)
    }

    pub async fn close(&self) {
        self.pool.close().await;
    }
    pub fn size(&self) -> u32 {
        self.pool.size()
    }
    pub fn num_idle(&self) -> usize {
        self.pool.num_idle()
    }

    // Session advisory locks are infrastructure coordination, not data access.
    // Kept crate-private so request handlers cannot obtain an unscoped executor.
    pub(crate) async fn acquire_advisory_connection(
        &self,
    ) -> Result<sqlx::PgConnection, sqlx::Error> {
        // Coordination may outlive an entire scheduler process. Detaching keeps
        // a one-connection data pool usable while that session holds its lock.
        Ok(self.pool.acquire().await?.detach())
    }
}

impl From<PgPool> for ScopedPgPool {
    fn from(pool: PgPool) -> Self {
        Self::new(pool)
    }
}

impl<'p> Executor<'p> for &ScopedPgPool {
    type Database = Postgres;

    fn fetch_many<'e, 'q: 'e, E>(
        self,
        query: E,
    ) -> BoxStream<
        'e,
        Result<
            Either<<Postgres as Database>::QueryResult, <Postgres as Database>::Row>,
            sqlx::Error,
        >,
    >
    where
        E: 'q + Execute<'q, Postgres>,
        'p: 'e,
    {
        // Capture task authority now, rather than when a stream is later polled
        // from a different async task.
        let bound = self.scope().map(|scope| self.with_scope(scope));
        Box::pin(async_stream::try_stream! {
            let pool = bound?;
            let mut tx = pool.begin().await?;
            {
                let mut rows = (&mut *tx).fetch_many(query);
                while let Some(row) = rows.try_next().await? { yield row; }
            }
            tx.commit().await?;
        })
    }

    fn fetch_optional<'e, 'q: 'e, E>(
        self,
        query: E,
    ) -> BoxFuture<'e, Result<Option<<Postgres as Database>::Row>, sqlx::Error>>
    where
        E: 'q + Execute<'q, Postgres>,
        'p: 'e,
    {
        let bound = self.scope().map(|scope| self.with_scope(scope));
        Box::pin(async move {
            let pool = bound?;
            let mut tx = pool.begin().await?;
            let row = (&mut *tx).fetch_optional(query).await?;
            tx.commit().await?;
            Ok(row)
        })
    }

    fn prepare_with<'e>(
        self,
        sql: SqlStr,
        parameters: &'e [<Postgres as Database>::TypeInfo],
    ) -> BoxFuture<'e, Result<<Postgres as Database>::Statement, sqlx::Error>>
    where
        'p: 'e,
    {
        let bound = self.scope().map(|scope| self.with_scope(scope));
        Box::pin(async move {
            let pool = bound?;
            let mut tx = pool.begin().await?;
            let statement = (&mut *tx).prepare_with(sql, parameters).await?;
            tx.commit().await?;
            Ok(statement)
        })
    }

    fn describe<'e>(
        self,
        sql: SqlStr,
    ) -> BoxFuture<'e, Result<sqlx::Describe<Postgres>, sqlx::Error>>
    where
        'p: 'e,
    {
        let bound = self.scope().map(|scope| self.with_scope(scope));
        Box::pin(async move {
            let pool = bound?;
            let mut tx = pool.begin().await?;
            let description = (&mut *tx).describe(sql).await?;
            tx.commit().await?;
            Ok(description)
        })
    }
}

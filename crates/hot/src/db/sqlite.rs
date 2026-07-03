//! SQLite connection tuning and periodic maintenance (WAL checkpoint, incremental vacuum).

use crate::val::Val;
use sqlx::Row;
use sqlx::sqlite::SqliteConnection;
use sqlx::{Executor, SqlitePool};

/// Tunable SQLite PRAGMA settings applied on every pool connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqlitePragmaConfig {
    /// Page cache size in KiB (`PRAGMA cache_size = -N`). 0 leaves SQLite default.
    pub cache_size_kb: i64,
    /// Memory-map up to this many bytes of the database file. 0 disables mmap.
    pub mmap_size_bytes: i64,
    /// Auto-checkpoint after this many WAL pages (`PRAGMA wal_autocheckpoint`).
    pub wal_autocheckpoint_pages: i64,
    /// Soft WAL size cap in KiB during checkpoint (`PRAGMA journal_size_limit`).
    pub journal_size_limit_kb: i64,
    /// `PRAGMA synchronous` level: `normal`, `full`, or `off`.
    pub synchronous: String,
    /// `PRAGMA temp_store`: `memory`, `file`, or `default`.
    pub temp_store: String,
    /// `PRAGMA auto_vacuum`: `incremental` or `none`.
    pub auto_vacuum: String,
    /// Pages to reclaim per maintenance run (`PRAGMA incremental_vacuum(N)`). 0 skips.
    pub maintenance_incremental_vacuum_pages: i64,
    /// Whether daily maintenance runs `PRAGMA wal_checkpoint(TRUNCATE)`.
    pub maintenance_wal_checkpoint: bool,
}

impl Default for SqlitePragmaConfig {
    fn default() -> Self {
        Self {
            cache_size_kb: 65_536,
            mmap_size_bytes: 268_435_456,
            wal_autocheckpoint_pages: 4_000,
            journal_size_limit_kb: 65_536,
            synchronous: "normal".to_string(),
            temp_store: "memory".to_string(),
            auto_vacuum: "incremental".to_string(),
            maintenance_incremental_vacuum_pages: 1_000,
            maintenance_wal_checkpoint: true,
        }
    }
}

impl SqlitePragmaConfig {
    pub fn from_conf(conf: &Val) -> Self {
        let defaults = Self::default();
        Self {
            cache_size_kb: conf
                .get_int_or_default("db.sqlite.cache-size-kb", defaults.cache_size_kb),
            mmap_size_bytes: conf
                .get_int_or_default("db.sqlite.mmap-size-bytes", defaults.mmap_size_bytes),
            wal_autocheckpoint_pages: conf.get_int_or_default(
                "db.sqlite.wal-autocheckpoint-pages",
                defaults.wal_autocheckpoint_pages,
            ),
            journal_size_limit_kb: conf.get_int_or_default(
                "db.sqlite.journal-size-limit-kb",
                defaults.journal_size_limit_kb,
            ),
            synchronous: conf
                .get_str_or_default("db.sqlite.synchronous", &defaults.synchronous)
                .to_ascii_lowercase(),
            temp_store: conf
                .get_str_or_default("db.sqlite.temp-store", &defaults.temp_store)
                .to_ascii_lowercase(),
            auto_vacuum: conf
                .get_str_or_default("db.sqlite.auto-vacuum", &defaults.auto_vacuum)
                .to_ascii_lowercase(),
            maintenance_incremental_vacuum_pages: conf.get_int_or_default(
                "db.sqlite.maintenance.incremental-vacuum-pages",
                defaults.maintenance_incremental_vacuum_pages,
            ),
            maintenance_wal_checkpoint: conf.get_bool_or_default(
                "db.sqlite.maintenance.wal-checkpoint",
                defaults.maintenance_wal_checkpoint,
            ),
        }
    }

    /// Apply baseline connection pragmas plus optional performance tuning.
    pub async fn apply(&self, conn: &mut SqliteConnection) -> Result<(), sqlx::Error> {
        conn.execute("PRAGMA journal_mode = WAL;").await?;
        conn.execute("PRAGMA busy_timeout = 30000;").await?;
        conn.execute("PRAGMA foreign_keys = ON;").await?;

        if self.cache_size_kb > 0 {
            conn.execute(sqlx::AssertSqlSafe(format!(
                "PRAGMA cache_size = -{};",
                self.cache_size_kb
            )))
            .await?;
        }
        if self.mmap_size_bytes > 0 {
            conn.execute(sqlx::AssertSqlSafe(format!(
                "PRAGMA mmap_size = {};",
                self.mmap_size_bytes
            )))
            .await?;
        }
        if self.wal_autocheckpoint_pages > 0 {
            conn.execute(sqlx::AssertSqlSafe(format!(
                "PRAGMA wal_autocheckpoint = {};",
                self.wal_autocheckpoint_pages
            )))
            .await?;
        }
        if self.journal_size_limit_kb > 0 {
            // PRAGMA journal_size_limit takes bytes; config is KiB.
            conn.execute(sqlx::AssertSqlSafe(format!(
                "PRAGMA journal_size_limit = {};",
                self.journal_size_limit_kb.saturating_mul(1024)
            )))
            .await?;
        }

        let synchronous = match self.synchronous.as_str() {
            "off" => "OFF",
            "full" => "FULL",
            "extra" => "EXTRA",
            _ => "NORMAL",
        };
        conn.execute(sqlx::AssertSqlSafe(format!(
            "PRAGMA synchronous = {};",
            synchronous
        )))
        .await?;

        let temp_store = match self.temp_store.as_str() {
            "file" => "FILE",
            "default" => "DEFAULT",
            _ => "MEMORY",
        };
        conn.execute(sqlx::AssertSqlSafe(format!(
            "PRAGMA temp_store = {};",
            temp_store
        )))
        .await?;

        let auto_vacuum = match self.auto_vacuum.as_str() {
            "incremental" => "INCREMENTAL",
            _ => "NONE",
        };
        conn.execute(sqlx::AssertSqlSafe(format!(
            "PRAGMA auto_vacuum = {};",
            auto_vacuum
        )))
        .await?;

        tracing::debug!(
            cache_size_kb = self.cache_size_kb,
            mmap_size_bytes = self.mmap_size_bytes,
            wal_autocheckpoint_pages = self.wal_autocheckpoint_pages,
            journal_size_limit_kb = self.journal_size_limit_kb,
            synchronous,
            temp_store,
            auto_vacuum,
            "SQLite connection pragmas applied"
        );
        Ok(())
    }
}

/// Result of a SQLite maintenance pass.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SqliteMaintenanceStats {
    pub wal_checkpoint_busy: i64,
    pub wal_checkpoint_log_pages: i64,
    pub wal_checkpointed_pages: i64,
    pub incremental_vacuum_pages_freed: i64,
}

/// Run WAL checkpoint and optional incremental vacuum. Safe to call from the
/// daily maintenance worker; no-ops individual steps when disabled in config.
pub async fn run_maintenance(
    pool: &SqlitePool,
    conf: &Val,
) -> Result<SqliteMaintenanceStats, String> {
    let config = SqlitePragmaConfig::from_conf(conf);
    let mut stats = SqliteMaintenanceStats::default();

    if config.maintenance_wal_checkpoint {
        let row = sqlx::query("PRAGMA wal_checkpoint(TRUNCATE);")
            .fetch_one(pool)
            .await
            .map_err(|e| format!("wal_checkpoint failed: {e}"))?;
        stats.wal_checkpoint_busy = row.try_get(0).unwrap_or(0);
        stats.wal_checkpoint_log_pages = row.try_get(1).unwrap_or(0);
        stats.wal_checkpointed_pages = row.try_get(2).unwrap_or(0);
    }

    if config.maintenance_incremental_vacuum_pages > 0 {
        let pages = config.maintenance_incremental_vacuum_pages;
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "PRAGMA incremental_vacuum({pages});"
        )))
        .execute(pool)
        .await
        .map_err(|e| format!("incremental_vacuum failed: {e}"))?;
        stats.incremental_vacuum_pages_freed = pages;
    }

    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::SqlitePoolOptions;

    #[test]
    fn test_from_conf_defaults() {
        let conf = Val::map_empty();
        let cfg = SqlitePragmaConfig::from_conf(&conf);
        assert_eq!(cfg.cache_size_kb, 65_536);
        assert_eq!(cfg.mmap_size_bytes, 268_435_456);
        assert_eq!(cfg.wal_autocheckpoint_pages, 4_000);
        assert!(cfg.maintenance_wal_checkpoint);
    }

    #[tokio::test]
    async fn test_apply_pragmas_on_memory_db() {
        let cfg = SqlitePragmaConfig::default();
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .after_connect({
                let cfg = cfg.clone();
                move |conn, _meta| {
                    let cfg = cfg.clone();
                    Box::pin(async move {
                        cfg.apply(conn).await?;
                        Ok(())
                    })
                }
            })
            .connect("sqlite::memory:")
            .await
            .unwrap();

        let cache_size: i64 = sqlx::query_scalar("PRAGMA cache_size;")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(cache_size, -65_536);

        let temp_store: i64 = sqlx::query_scalar("PRAGMA temp_store;")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(temp_store, 2); // MEMORY
    }

    #[tokio::test]
    async fn test_run_maintenance_on_memory_db() {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::query("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT);")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO t (v) VALUES ('x');")
            .execute(&pool)
            .await
            .unwrap();

        run_maintenance(&pool, &Val::map_empty())
            .await
            .expect("maintenance should succeed");
    }
}

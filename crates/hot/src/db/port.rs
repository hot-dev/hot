//! Hot 1.x → Hot 2 SQLite database port.
//!
//! Implements the `hot db port-v1-to-v2` command for SQLite. Backs up the existing v1
//! file, applies the Hot 2 baseline migrations to a fresh file at the same path, then
//! copies user data from the backup into the new file using `ATTACH DATABASE`.
//!
//! The result is a Hot 2 database whose schema is byte-identical to one produced by a
//! fresh `hot init`, with the user's preserved rows. This sidesteps the schema drift
//! that an in-place "ledger rewrite" adoption would carry forward (extra v1-only
//! tables, column ordering, default expressions, indexes, constraint names).
//!
//! Postgres has no equivalent path in Hot 2; the Hot Cloud v1→v2 backfill is owned
//! by the private cloud repository.

use crate::db::{
    DatabaseError, DatabaseType, get_db_uri_from_conf, redact_password, run_migrations,
};
use crate::val::Val;
use sqlx::sqlite::{SqliteConnectOptions, SqliteConnection};
use sqlx::{ConnectOptions, Connection};
use std::path::{Path, PathBuf};

/// Tables the v2 baseline pre-populates (lookup tables + bootstrap rows). Copying v1
/// rows into them would either conflict with a fixed seed-row primary key or duplicate
/// the v2-authoritative seed values.
const SEED_TABLES: &[&str] = &[
    "alert_channel",
    "alert_delivery_status",
    "alert_destination_type",
    "build_type",
    "email_verification_status",
    "invite_status",
    "org_plan_status",
    "org_user_role",
    "run_status",
    "run_type",
    "scheduler_state",
    "task_status",
    "team_user_role",
];

/// Tables that exist in v1 but not in the v2 baseline. Their data is intentionally
/// dropped during the port. Reported in the [`PortReport`] so callers can surface what
/// the v2 database does not carry forward.
const V1_ONLY_TABLES: &[&str] = &["store", "subscription", "subscription_plan"];

/// Per-table outcome for a successful copy.
#[derive(Debug, Clone)]
pub struct TableCopyReport {
    pub table: String,
    pub rows_copied: i64,
    /// Columns present in the v1 table but not in the v2 table; their data was dropped.
    pub v1_only_columns: Vec<String>,
    /// Columns present in the v2 table but not in the v1 table; rows landed with the
    /// v2 column default (or NULL).
    pub v2_only_columns: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    /// Table is not present in the v1 backup; nothing to copy.
    NotInV1,
    /// Table is pre-seeded by the v2 baseline; copying would conflict with the seed.
    SeedTable,
}

#[derive(Debug, Clone)]
pub struct SkippedTable {
    pub table: String,
    pub reason: SkipReason,
}

#[derive(Debug, Clone)]
pub struct DroppedV1Table {
    pub table: String,
    pub rows_dropped: i64,
}

/// Summary returned by [`port_v1_sqlite_to_v2`].
#[derive(Debug, Clone)]
pub struct PortReport {
    pub backup_path: PathBuf,
    pub db_path: PathBuf,
    pub copied_tables: Vec<TableCopyReport>,
    pub skipped_tables: Vec<SkippedTable>,
    pub dropped_v1_tables: Vec<DroppedV1Table>,
}

impl PortReport {
    /// Total rows copied across all tables.
    pub fn total_rows_copied(&self) -> i64 {
        self.copied_tables.iter().map(|t| t.rows_copied).sum()
    }

    /// Total rows in v1 that were dropped because the destination table no longer exists.
    pub fn total_rows_dropped(&self) -> i64 {
        self.dropped_v1_tables.iter().map(|t| t.rows_dropped).sum()
    }
}

/// Port a Hot 1.x SQLite database at the configured URI into a fresh Hot 2 database.
///
/// On success, the file at the configured URI is replaced with a fresh Hot 2 database
/// containing the user's preserved rows, and a backup of the original file is left at
/// `<original>.v1.bak.<utc-timestamp>`.
///
/// Errors if the configured database is not SQLite, the file is missing, the file is
/// already a Hot 2 database, or the copy fails. On copy failure the v2 database is left
/// in place at the configured URI for inspection; the v1 backup is always preserved.
pub async fn port_v1_sqlite_to_v2(conf: &Val) -> Result<PortReport, DatabaseError> {
    let uri = get_db_uri_from_conf(conf);
    let db_type = DatabaseType::from_uri(&uri)?;
    if !matches!(db_type, DatabaseType::Sqlite) {
        return Err(DatabaseError::UnsupportedType(format!(
            "hot db port-v1-to-v2 only supports SQLite databases. Configured database is: {}",
            redact_password(&uri),
        )));
    }

    let path_str = uri
        .strip_prefix("sqlite:")
        .ok_or_else(|| DatabaseError::UnsupportedType(format!("invalid sqlite uri: {uri}")))?;
    if path_str.contains(":memory:") {
        return Err(DatabaseError::Migration(
            "cannot port an in-memory SQLite database".to_string(),
        ));
    }

    let db_path = PathBuf::from(path_str);
    if !db_path.exists() {
        return Err(DatabaseError::NotInitialized(format!(
            "no SQLite database file at {} to port",
            db_path.display(),
        )));
    }

    inspect_v1_database(&db_path).await?;

    let backup_path = backup_path_for(&db_path);
    std::fs::copy(&db_path, &backup_path).map_err(|e| {
        DatabaseError::Migration(format!(
            "failed to copy {} to backup {}: {}",
            db_path.display(),
            backup_path.display(),
            e
        ))
    })?;
    tracing::info!(
        "Backed up Hot 1.x SQLite database to {}",
        backup_path.display()
    );

    std::fs::remove_file(&db_path).map_err(|e| {
        DatabaseError::Migration(format!(
            "failed to remove original SQLite file at {}: {}. A backup is at {}.",
            db_path.display(),
            e,
            backup_path.display(),
        ))
    })?;
    // SQLite's WAL/SHM sidecars survive remove_file; clean them up so the new v2 db
    // doesn't inherit a stale WAL from the v1 file. Sidecars sit next to the db file
    // with a "-wal" / "-shm" suffix on the full filename (not as a real extension).
    let mut wal = db_path.clone().into_os_string();
    wal.push("-wal");
    let _ = std::fs::remove_file(PathBuf::from(&wal));
    let mut shm = db_path.clone().into_os_string();
    shm.push("-shm");
    let _ = std::fs::remove_file(PathBuf::from(&shm));

    run_migrations(conf).await.map_err(|e| {
        DatabaseError::Migration(format!(
            "{e}. Backup of original Hot 1.x database is at {}.",
            backup_path.display()
        ))
    })?;

    let report = copy_user_data(&db_path, &backup_path).await?;

    tracing::info!(
        "Hot 1.x → Hot 2 SQLite port complete: {} rows copied across {} tables, {} rows in {} \
         v1-only tables dropped. Backup retained at {}.",
        report.total_rows_copied(),
        report.copied_tables.len(),
        report.total_rows_dropped(),
        report.dropped_v1_tables.len(),
        report.backup_path.display(),
    );
    Ok(report)
}

async fn copy_user_data(db_path: &Path, backup_path: &Path) -> Result<PortReport, DatabaseError> {
    let mut conn = SqliteConnectOptions::new()
        .filename(db_path)
        .create_if_missing(false)
        .connect()
        .await?;

    // PRAGMA must be set before ATTACH and outside any implicit transaction. Disable FK
    // enforcement for the duration of the copy; verify with foreign_key_check at the end.
    sqlx::raw_sql("PRAGMA foreign_keys = OFF")
        .execute(&mut conn)
        .await?;

    let backup_path_for_attach = backup_path.to_string_lossy().replace('\'', "''");
    let attach_sql = format!("ATTACH DATABASE '{backup_path_for_attach}' AS v1");
    sqlx::raw_sql(&attach_sql).execute(&mut conn).await?;

    let v2_tables = list_user_tables(&mut conn, "main").await?;

    let mut copied_tables = Vec::new();
    let mut skipped_tables = Vec::new();

    for table in &v2_tables {
        if SEED_TABLES.contains(&table.as_str()) {
            skipped_tables.push(SkippedTable {
                table: table.clone(),
                reason: SkipReason::SeedTable,
            });
            continue;
        }
        if !table_exists(&mut conn, "v1", table).await? {
            skipped_tables.push(SkippedTable {
                table: table.clone(),
                reason: SkipReason::NotInV1,
            });
            continue;
        }

        let v2_cols = column_names(&mut conn, "main", table).await?;
        let v1_cols = column_names(&mut conn, "v1", table).await?;
        let v2_only_columns: Vec<String> = v2_cols
            .iter()
            .filter(|c| !v1_cols.contains(c))
            .cloned()
            .collect();
        let v1_only_columns: Vec<String> = v1_cols
            .iter()
            .filter(|c| !v2_cols.contains(c))
            .cloned()
            .collect();
        let intersection: Vec<&String> = v2_cols.iter().filter(|c| v1_cols.contains(c)).collect();
        if intersection.is_empty() {
            skipped_tables.push(SkippedTable {
                table: table.clone(),
                reason: SkipReason::NotInV1,
            });
            continue;
        }
        let cols_sql = intersection
            .iter()
            .map(|c| format!("\"{c}\""))
            .collect::<Vec<_>>()
            .join(", ");
        let copy_sql = format!(
            "INSERT INTO main.\"{table}\" ({cols_sql}) SELECT {cols_sql} FROM v1.\"{table}\""
        );
        let result = sqlx::query(&copy_sql)
            .execute(&mut conn)
            .await
            .map_err(|e| {
                DatabaseError::Migration(format!(
                    "failed to copy table {table} from v1 to v2 (sql: {copy_sql}): {e}. The v2 \
                 database is at {} and the v1 backup is at {}.",
                    db_path.display(),
                    backup_path.display(),
                ))
            })?;
        copied_tables.push(TableCopyReport {
            table: table.clone(),
            rows_copied: result.rows_affected() as i64,
            v1_only_columns,
            v2_only_columns,
        });
    }

    let dropped_v1_tables = inspect_v1_only_tables(&mut conn).await?;

    let fk_violations: Vec<(String, i64, String, i64)> = sqlx::query_as("PRAGMA foreign_key_check")
        .fetch_all(&mut conn)
        .await?;
    if !fk_violations.is_empty() {
        return Err(DatabaseError::Migration(format!(
            "Hot 2 database has {} foreign-key violation(s) after the port. The v2 database is \
             at {} and the v1 backup is at {}. First violation: table={}, rowid={}, parent={}, \
             fkid={}.",
            fk_violations.len(),
            db_path.display(),
            backup_path.display(),
            fk_violations[0].0,
            fk_violations[0].1,
            fk_violations[0].2,
            fk_violations[0].3,
        )));
    }

    sqlx::raw_sql("PRAGMA foreign_keys = ON")
        .execute(&mut conn)
        .await?;
    sqlx::raw_sql("DETACH DATABASE v1")
        .execute(&mut conn)
        .await?;
    conn.close().await?;

    Ok(PortReport {
        backup_path: backup_path.to_path_buf(),
        db_path: db_path.to_path_buf(),
        copied_tables,
        skipped_tables,
        dropped_v1_tables,
    })
}

async fn inspect_v1_database(db_path: &Path) -> Result<(), DatabaseError> {
    let mut conn = SqliteConnectOptions::new()
        .filename(db_path)
        .create_if_missing(false)
        .connect()
        .await?;

    let migrations_table_exists: Option<String> = sqlx::query_scalar(
        "SELECT name FROM sqlite_master WHERE type = 'table' AND name = '_sqlx_migrations'",
    )
    .fetch_optional(&mut conn)
    .await?;
    if migrations_table_exists.is_none() {
        return Err(DatabaseError::Migration(format!(
            "{} does not contain a Hot migration ledger; nothing to port",
            db_path.display(),
        )));
    }

    let row1: Option<(i64, String)> =
        sqlx::query_as("SELECT version, description FROM _sqlx_migrations WHERE version = 1")
            .fetch_optional(&mut conn)
            .await?;
    let (_, description) = row1.ok_or_else(|| {
        DatabaseError::Migration(format!(
            "{} has an empty migration ledger; nothing to port",
            db_path.display(),
        ))
    })?;

    if description.eq_ignore_ascii_case("hot 2 initial schema") {
        return Err(DatabaseError::Migration(format!(
            "{} is already a Hot 2 database; nothing to port",
            db_path.display(),
        )));
    }

    conn.close().await?;
    Ok(())
}

async fn list_user_tables(
    conn: &mut SqliteConnection,
    schema: &str,
) -> Result<Vec<String>, DatabaseError> {
    validate_identifier(schema, "schema")?;
    let sql = format!(
        "SELECT name FROM {schema}.sqlite_master \
         WHERE type = 'table' AND name NOT LIKE 'sqlite_%' AND name != '_sqlx_migrations' \
         ORDER BY name"
    );
    let rows: Vec<(String,)> = sqlx::query_as(&sql).fetch_all(&mut *conn).await?;
    Ok(rows.into_iter().map(|(name,)| name).collect())
}

async fn table_exists(
    conn: &mut SqliteConnection,
    schema: &str,
    table: &str,
) -> Result<bool, DatabaseError> {
    validate_identifier(schema, "schema")?;
    let sql = format!(
        "SELECT EXISTS(SELECT 1 FROM {schema}.sqlite_master WHERE type = 'table' AND name = ?)"
    );
    let exists: bool = sqlx::query_scalar(&sql)
        .bind(table)
        .fetch_one(&mut *conn)
        .await?;
    Ok(exists)
}

async fn column_names(
    conn: &mut SqliteConnection,
    schema: &str,
    table: &str,
) -> Result<Vec<String>, DatabaseError> {
    // SQLite's `pragma_table_info` is a table-valued function. The reliable way to scope
    // it to a non-`main` attached database is the two-argument form
    // `pragma_table_info(<table>, <schema>)`. The function-name database prefix
    // (e.g. `v1.pragma_table_info(<table>)`) silently falls back to `main` instead of
    // erroring, which would let v1 column inspection return v2 columns.
    //
    // Both arguments are interpolated because table-valued PRAGMAs do not accept bind
    // parameters. Identifiers come from sqlite_master and the hardcoded schema names
    // ("main", "v1"), but defense-in-depth is cheap.
    validate_identifier(schema, "schema")?;
    validate_identifier(table, "table")?;
    let sql = format!("SELECT name FROM pragma_table_info('{table}', '{schema}')");
    let rows: Vec<(String,)> = sqlx::query_as(&sql).fetch_all(&mut *conn).await?;
    Ok(rows.into_iter().map(|(name,)| name).collect())
}

fn validate_identifier(name: &str, kind: &str) -> Result<(), DatabaseError> {
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err(DatabaseError::Migration(format!(
            "refusing to interpolate unexpected {kind} name {name:?}"
        )));
    }
    Ok(())
}

async fn inspect_v1_only_tables(
    conn: &mut SqliteConnection,
) -> Result<Vec<DroppedV1Table>, DatabaseError> {
    let mut out = Vec::new();
    for table in V1_ONLY_TABLES {
        if !table_exists(conn, "v1", table).await? {
            continue;
        }
        let count_sql = format!("SELECT count(*) FROM v1.\"{table}\"");
        let rows_dropped: i64 = sqlx::query_scalar(&count_sql).fetch_one(&mut *conn).await?;
        out.push(DroppedV1Table {
            table: table.to_string(),
            rows_dropped,
        });
    }
    Ok(out)
}

fn backup_path_for(db_path: &Path) -> PathBuf {
    let timestamp = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    let mut name = db_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "hot.sqlite.db".to_string());
    name.push_str(".v1.bak.");
    name.push_str(&timestamp);
    db_path.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::Executor;
    use uuid::Uuid;

    /// Build a synthetic v1-shaped SQLite database at `path`. The schema mimics v1's
    /// real column shape (so the column intersection satisfies v2's NOT NULL/FK
    /// constraints) plus deliberate divergences that exercise the port's logic:
    ///
    ///   - `org.usage_limit_id` (v1-only column) → port should drop it.
    ///   - `org` is missing `features` (v2-only column) → port should leave it NULL.
    ///   - `org` is missing `org_type` (v2-only column with NOT NULL DEFAULT) → port
    ///     should leave it at the v2 default ('organization').
    ///   - v1-only tables `store` and `subscription_plan` → port should report rows
    ///     dropped.
    ///   - v1's `org_user_role` lookup table → port should skip it as a SeedTable.
    ///
    /// The `_sqlx_migrations` ledger uses v1's actual version-1 description so the
    /// "is this already a Hot 2 db" check does not short-circuit.
    async fn build_v1_fixture(path: &Path) -> (Uuid, Uuid, Uuid) {
        if path.exists() {
            std::fs::remove_file(path).unwrap();
        }
        let mut conn = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .connect()
            .await
            .expect("open v1 fixture");

        conn.execute(
            "CREATE TABLE _sqlx_migrations (
                version BIGINT PRIMARY KEY,
                description TEXT NOT NULL,
                installed_on TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
                success BOOLEAN NOT NULL,
                checksum BLOB NOT NULL,
                execution_time BIGINT NOT NULL
            )",
        )
        .await
        .unwrap();
        for v in 1..=23i64 {
            let desc = if v == 1 {
                "create initial schema"
            } else {
                "v1 migration"
            };
            sqlx::query(
                "INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time)
                 VALUES (?, ?, 1, ?, 0)",
            )
            .bind(v)
            .bind(desc)
            .bind(&vec![0u8; 32][..])
            .execute(&mut conn)
            .await
            .unwrap();
        }

        // Self-referential FK on user.created_by_user_id; FK enforcement off during
        // fixture build so we can seed the singleton user without populating its parent.
        sqlx::raw_sql("PRAGMA foreign_keys = OFF")
            .execute(&mut conn)
            .await
            .unwrap();

        conn.execute(
            r#"
            CREATE TABLE user (
                user_id blob primary key,
                email text unique not null,
                name text,
                settings text,
                active integer default 1,
                created_at datetime default current_timestamp,
                created_by_user_id blob not null references user(user_id),
                updated_at datetime default current_timestamp,
                updated_by_user_id blob references user(user_id),
                active_toggle_at datetime,
                active_toggle_by_user_id blob references user(user_id)
            );
            CREATE TABLE org (
                org_id blob primary key,
                name text not null,
                slug text unique not null,
                is_personal integer default 0,
                usage_limit_id blob,
                settings text default '{}',
                active integer default 1,
                created_at datetime default current_timestamp,
                created_by_user_id blob not null references user(user_id),
                updated_at datetime default current_timestamp,
                updated_by_user_id blob references user(user_id),
                active_toggle_at datetime,
                active_toggle_by_user_id blob references user(user_id)
            );
            CREATE TABLE env (
                env_id blob primary key,
                org_id blob not null references org(org_id),
                name text not null,
                active integer default 1,
                created_by_user_id blob not null references user(user_id),
                created_at datetime default current_timestamp,
                updated_at datetime default current_timestamp,
                updated_by_user_id blob references user(user_id),
                active_toggle_at datetime,
                active_toggle_by_user_id blob references user(user_id)
            );
            CREATE TABLE project (
                project_id blob primary key,
                env_id blob not null references env(env_id),
                name text not null,
                active integer default 1,
                created_by_user_id blob not null references user(user_id),
                created_at datetime default current_timestamp,
                updated_at datetime default current_timestamp,
                updated_by_user_id blob references user(user_id),
                active_toggle_at datetime,
                active_toggle_by_user_id blob references user(user_id)
            );
            CREATE TABLE store (
                key TEXT PRIMARY KEY,
                value TEXT
            );
            CREATE TABLE subscription_plan (
                subscription_plan_id BLOB PRIMARY KEY,
                plan_name TEXT NOT NULL
            );
            CREATE TABLE org_user_role (
                org_user_role_id INTEGER PRIMARY KEY,
                role TEXT NOT NULL,
                sort_order INTEGER NOT NULL
            );
            "#,
        )
        .await
        .unwrap();

        let user_id = Uuid::now_v7();
        let org_id = Uuid::now_v7();
        let env_id = Uuid::now_v7();
        let project_id = Uuid::now_v7();

        sqlx::query(
            "INSERT INTO user (user_id, email, name, created_by_user_id) VALUES (?, ?, ?, ?)",
        )
        .bind(user_id.as_bytes().as_slice())
        .bind("dev@example.com")
        .bind("Dev")
        .bind(user_id.as_bytes().as_slice())
        .execute(&mut conn)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO org (org_id, name, slug, usage_limit_id, created_by_user_id) \
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(org_id.as_bytes().as_slice())
        .bind("Acme")
        .bind("acme")
        .bind(Uuid::now_v7().as_bytes().as_slice())
        .bind(user_id.as_bytes().as_slice())
        .execute(&mut conn)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO env (env_id, org_id, name, created_by_user_id) VALUES (?, ?, ?, ?)",
        )
        .bind(env_id.as_bytes().as_slice())
        .bind(org_id.as_bytes().as_slice())
        .bind("development")
        .bind(user_id.as_bytes().as_slice())
        .execute(&mut conn)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO project (project_id, env_id, name, created_by_user_id) \
             VALUES (?, ?, ?, ?)",
        )
        .bind(project_id.as_bytes().as_slice())
        .bind(env_id.as_bytes().as_slice())
        .bind("hello-world")
        .bind(user_id.as_bytes().as_slice())
        .execute(&mut conn)
        .await
        .unwrap();
        sqlx::query("INSERT INTO store (key, value) VALUES (?, ?), (?, ?)")
            .bind("k1")
            .bind("val1")
            .bind("k2")
            .bind("val2")
            .execute(&mut conn)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO subscription_plan (subscription_plan_id, plan_name) VALUES (?, ?)",
        )
        .bind(Uuid::now_v7().as_bytes().as_slice())
        .bind("starter")
        .execute(&mut conn)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO org_user_role (org_user_role_id, role, sort_order) VALUES (?, ?, ?)",
        )
        .bind(1i64)
        .bind("member")
        .bind(1i64)
        .execute(&mut conn)
        .await
        .unwrap();

        conn.close().await.unwrap();
        (org_id, user_id, env_id)
    }

    fn conf_for_path(path: &Path) -> Val {
        crate::val!({
            "db": {
                "uri": format!("sqlite:{}", path.display()),
            }
        })
    }

    #[tokio::test]
    async fn test_port_v1_sqlite_to_v2_happy_path() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("hot.sqlite.db");
        let (org_id, user_id, _) = build_v1_fixture(&db_path).await;
        let conf = conf_for_path(&db_path);

        let report = port_v1_sqlite_to_v2(&conf)
            .await
            .expect("port should succeed");

        assert!(report.backup_path.exists(), "backup file must exist");
        assert!(
            report
                .backup_path
                .file_name()
                .unwrap()
                .to_string_lossy()
                .contains(".v1.bak."),
            "backup file name must contain .v1.bak. marker"
        );
        assert!(report.db_path.exists(), "v2 db must exist");

        // Open the new v2 db directly and verify rows landed.
        let mut v2 = SqliteConnectOptions::new()
            .filename(&report.db_path)
            .connect()
            .await
            .unwrap();
        let org_count: i64 = sqlx::query_scalar("SELECT count(*) FROM org")
            .fetch_one(&mut v2)
            .await
            .unwrap();
        assert_eq!(org_count, 1);
        let user_count: i64 = sqlx::query_scalar("SELECT count(*) FROM user")
            .fetch_one(&mut v2)
            .await
            .unwrap();
        assert_eq!(user_count, 1);
        let env_count: i64 = sqlx::query_scalar("SELECT count(*) FROM env")
            .fetch_one(&mut v2)
            .await
            .unwrap();
        assert_eq!(env_count, 1);
        let project_count: i64 = sqlx::query_scalar("SELECT count(*) FROM project")
            .fetch_one(&mut v2)
            .await
            .unwrap();
        assert_eq!(project_count, 1);

        // Sentinel value preserved. v1 column ordering or types are irrelevant; the row
        // matches by primary key.
        let org_name: String = sqlx::query_scalar("SELECT name FROM org WHERE org_id = ?")
            .bind(org_id.as_bytes().as_slice())
            .fetch_one(&mut v2)
            .await
            .unwrap();
        assert_eq!(org_name, "Acme");
        let user_email: String = sqlx::query_scalar("SELECT email FROM user WHERE user_id = ?")
            .bind(user_id.as_bytes().as_slice())
            .fetch_one(&mut v2)
            .await
            .unwrap();
        assert_eq!(user_email, "dev@example.com");

        // v1-only column (`usage_limit_id`) was dropped; v2-only columns (`features`
        // and `org_type`) are at their v2 defaults.
        let features: Option<String> =
            sqlx::query_scalar("SELECT features FROM org WHERE org_id = ?")
                .bind(org_id.as_bytes().as_slice())
                .fetch_one(&mut v2)
                .await
                .unwrap();
        assert!(features.is_none(), "features should be NULL after port");
        let org_type: String = sqlx::query_scalar("SELECT org_type FROM org WHERE org_id = ?")
            .bind(org_id.as_bytes().as_slice())
            .fetch_one(&mut v2)
            .await
            .unwrap();
        assert_eq!(
            org_type, "organization",
            "org_type should fall back to the v2 NOT NULL DEFAULT"
        );

        // v1-only tables (`store`, `subscription_plan`, `subscription`) must not exist
        // in v2. We assert on `store` because the fixture populated it.
        let store_present: Option<String> = sqlx::query_scalar(
            "SELECT name FROM sqlite_master WHERE type = 'table' AND name = 'store'",
        )
        .fetch_optional(&mut v2)
        .await
        .unwrap();
        assert!(
            store_present.is_none(),
            "v2 must not contain v1-only `store`"
        );

        v2.close().await.unwrap();

        // The org table copy report should mention the v1-only column we added and the
        // v2-only column we omitted.
        let org_report = report
            .copied_tables
            .iter()
            .find(|t| t.table == "org")
            .expect("org should be in copied_tables");
        assert!(
            org_report
                .v1_only_columns
                .iter()
                .any(|c| c == "usage_limit_id"),
            "v1_only_columns should include usage_limit_id; got {:?}",
            org_report.v1_only_columns
        );
        assert!(
            org_report.v2_only_columns.iter().any(|c| c == "features"),
            "v2_only_columns should include features; got {:?}",
            org_report.v2_only_columns
        );
        assert!(
            org_report.v2_only_columns.iter().any(|c| c == "org_type"),
            "v2_only_columns should include org_type; got {:?}",
            org_report.v2_only_columns
        );
        assert_eq!(org_report.rows_copied, 1);

        // Seed tables (org_user_role) must be skipped, not copied; v2 baseline already
        // populates them.
        assert!(
            report
                .skipped_tables
                .iter()
                .any(|t| t.table == "org_user_role" && t.reason == SkipReason::SeedTable),
            "org_user_role should be skipped as a seed table"
        );

        // v1-only `store` should be reported as dropped with its row count.
        let store_dropped = report
            .dropped_v1_tables
            .iter()
            .find(|t| t.table == "store")
            .expect("store should appear in dropped_v1_tables");
        assert_eq!(store_dropped.rows_dropped, 2);
    }

    #[tokio::test]
    async fn test_port_resulting_schema_matches_fresh_v2() {
        // After porting, the user-table set in the v2 db should equal the table set
        // produced by a fresh `hot db migrate` against an empty file.
        let tmp = tempfile::tempdir().unwrap();
        let ported_path = tmp.path().join("ported.sqlite.db");
        build_v1_fixture(&ported_path).await;
        let conf = conf_for_path(&ported_path);
        port_v1_sqlite_to_v2(&conf).await.expect("port");

        let fresh_path = tmp.path().join("fresh.sqlite.db");
        let fresh_conf = conf_for_path(&fresh_path);
        run_migrations(&fresh_conf).await.expect("fresh migrate");

        async fn user_table_names(path: &Path) -> Vec<String> {
            let mut conn = SqliteConnectOptions::new()
                .filename(path)
                .connect()
                .await
                .unwrap();
            let rows: Vec<(String,)> = sqlx::query_as(
                "SELECT name FROM sqlite_master \
                 WHERE type = 'table' AND name NOT LIKE 'sqlite_%' AND name != '_sqlx_migrations' \
                 ORDER BY name",
            )
            .fetch_all(&mut conn)
            .await
            .unwrap();
            conn.close().await.unwrap();
            rows.into_iter().map(|(n,)| n).collect()
        }

        let ported_tables = user_table_names(&ported_path).await;
        let fresh_tables = user_table_names(&fresh_path).await;
        assert_eq!(
            ported_tables, fresh_tables,
            "ported and fresh v2 databases must have identical user-table sets"
        );
    }

    #[tokio::test]
    async fn test_port_refuses_already_v2_database() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("hot.sqlite.db");
        let conf = conf_for_path(&db_path);
        // Apply v2 baseline directly: the file is now a Hot 2 db.
        run_migrations(&conf).await.expect("fresh v2 migrate");

        let err = port_v1_sqlite_to_v2(&conf)
            .await
            .expect_err("port should refuse a Hot 2 database");
        assert!(
            err.to_string().contains("already a Hot 2 database"),
            "got: {err}"
        );
    }

    #[tokio::test]
    async fn test_port_refuses_missing_database_file() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("does-not-exist.sqlite.db");
        let conf = conf_for_path(&db_path);

        let err = port_v1_sqlite_to_v2(&conf)
            .await
            .expect_err("port should refuse when the file is missing");
        assert!(
            err.to_string().contains("no SQLite database file"),
            "got: {err}"
        );
    }

    #[tokio::test]
    async fn test_port_refuses_postgres_uri() {
        let conf = crate::val!({
            "db": {
                "uri": "postgres://localhost/hot",
            }
        });
        let err = port_v1_sqlite_to_v2(&conf)
            .await
            .expect_err("port should refuse non-sqlite uris");
        assert!(
            err.to_string().contains("only supports SQLite"),
            "got: {err}"
        );
    }

    #[tokio::test]
    async fn test_backup_path_format() {
        let p = PathBuf::from("/tmp/some/hot.sqlite.db");
        let backup = backup_path_for(&p);
        let name = backup.file_name().unwrap().to_string_lossy();
        assert!(name.starts_with("hot.sqlite.db.v1.bak."));
        assert_eq!(backup.parent(), p.parent());
    }

    #[test]
    fn test_validate_identifier_rejects_injection() {
        assert!(validate_identifier("main", "schema").is_ok());
        assert!(validate_identifier("v1", "schema").is_ok());
        assert!(validate_identifier("org_user_role", "table").is_ok());
        assert!(validate_identifier("", "table").is_err());
        assert!(validate_identifier("foo;DROP TABLE bar", "table").is_err());
        assert!(validate_identifier("foo bar", "table").is_err());
        assert!(validate_identifier("foo'", "table").is_err());
    }
}

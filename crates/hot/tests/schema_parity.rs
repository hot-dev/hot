//! Keep the SQLite and PostgreSQL migration chains on one schema. Both
//! directories carry the same migration file names, so a version number means
//! the same schema on either backend, and a fresh database built by each chain
//! has the same tables, columns, keys, indexes, reference rows and ownership
//! guards. Differences the engines force are listed, with their reason, below.
//! The live comparison needs HOT_TEST_POSTGRES_URI to name a disposable
//! privileged fixture; the test creates and drops its own database there.

use regex::Regex;
use sqlx::{
    Executor, PgPool, Row, SqlitePool, migrate::Migrator, postgres::PgPoolOptions,
    sqlite::SqliteConnectOptions,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};
use uuid::Uuid;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

/// Columns whose nullability differs only in how it is enforced: SQLite cannot
/// add NOT NULL to an existing column, so a trigger rejects NULL instead.
const NULLABILITY_ENFORCED_BY_TRIGGER: &[&str] = &["org.active"];

/// Indexes PostgreSQL has and SQLite cannot: trigram search over store text.
const POSTGRES_ONLY_INDEXES: &[&str] = &["idx_store_map_entry_text"];

/// Indexes both have whose definitions only differ in engine syntax:
/// PostgreSQL's UNIQUE NULLS NOT DISTINCT is SQLite's zero-sentinel COALESCE.
const DIALECT_INDEXES: &[&str] = &["usage_file_org_id_env_id_slot_key"];

/// Tables whose PostgreSQL primary key is a SQLite UNIQUE constraint: SQLite
/// only autoincrements an INTEGER PRIMARY KEY, so `seq` takes that role there.
/// Both engines enforce the same unique key; it is compared as an index.
const AUTOINCREMENT_KEY_TABLES: &[&str] = &["store_map_entry"];

/// Reference tables whose seeded rows both baselines must agree on.
const REFERENCE_TABLES: &[&str] = &[
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

fn migrations(backend: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../resources/db")
        .join(backend)
        .join("migrations")
}

fn migration_files(backend: &str) -> Vec<String> {
    let mut files: Vec<String> = std::fs::read_dir(migrations(backend))
        .expect("migration directory should be readable")
        .filter_map(|entry| {
            let name = entry.ok()?.file_name().to_string_lossy().to_string();
            name.ends_with(".sql").then_some(name)
        })
        .collect();
    files.sort();
    files
}

/// Released migrations named before the chains were held in lockstep. Their
/// numbers match; renaming them would change what existing ledgers recorded.
const GRANDFATHERED_NAMES: &[&str] = &["002"];

#[test]
fn migration_chains_share_numbers_and_names() {
    let names = |backend| {
        migration_files(backend)
            .into_iter()
            .map(|file| {
                match GRANDFATHERED_NAMES
                    .iter()
                    .find(|version| file.starts_with(&format!("{version}_")))
                {
                    Some(version) => format!("{version}_*"),
                    None => file,
                }
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(
        names("sqlite"),
        names("postgres"),
        "each migration needs a file of the same name for both backends; a change \
         one engine does not need still gets a no-op file in the other"
    );
}

#[derive(Debug, Default, PartialEq)]
struct Schema {
    tables: BTreeSet<String>,
    columns: BTreeSet<String>,
    primary_keys: BTreeSet<String>,
    foreign_keys: BTreeSet<String>,
    defaults: BTreeSet<String>,
    /// Normalized (table, unique, columns, predicate) of each non-primary index.
    index_shapes: BTreeMap<String, String>,
    /// Names of explicitly created (not constraint-backed) indexes.
    index_names: BTreeSet<String>,
    reference_rows: BTreeMap<String, Vec<String>>,
    immutable_guards: BTreeSet<String>,
    integrity_guards: BTreeSet<String>,
}

/// Rewrite one engine's spelling of an index expression or predicate into a
/// shared one: no casts, quotes, spaces, sort order, or PostgreSQL-only forms.
fn normalize(expression: &str) -> String {
    let mut text = expression.to_lowercase().replace("hot.", "");
    for (pattern, replacement) in [
        (r"\s+(desc|asc)(\s+nulls\s+(last|first))?\b", ""),
        (r"=\s*any\s*\(array\[(.*?)\]\)", " in ($1)"),
        (
            r"'00000000-0000-0000-0000-000000000000'::uuid",
            "x'00000000000000000000000000000000'",
        ),
        (r"json_extract\((\w+),\s*'\$\.(\w+)'\)", "$1->>'$2'"),
        (r"::[a-z_ ]+(\[\])?", ""),
        (r#"[()"\s]"#, ""),
    ] {
        text = Regex::new(pattern)
            .unwrap()
            .replace_all(&text, replacement)
            .to_string();
    }
    text.replace("<>", "!=")
        .replace("true", "1")
        .replace("false", "0")
}

fn shape(table: &str, unique: bool, columns: &[String], predicate: &str) -> String {
    format!(
        "{table} unique={unique} ({}) where {}",
        columns.join(","),
        normalize(predicate)
    )
}

/// The parenthesized list that starts at `open`, without its parentheses.
fn parenthesized(text: &str, open: usize) -> &str {
    let mut depth = 0;
    for (offset, character) in text[open..].char_indices() {
        match character {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return &text[open + 1..open + offset];
                }
            }
            _ => {}
        }
    }
    panic!("unbalanced index definition: {text}")
}

/// Split a column list on its top-level commas.
fn split_columns(list: &str) -> Vec<String> {
    let (mut parts, mut depth, mut start) = (Vec::new(), 0, 0);
    for (offset, character) in list.char_indices() {
        match character {
            '(' => depth += 1,
            ')' => depth -= 1,
            ',' if depth == 0 => {
                parts.push(normalize(&list[start..offset]));
                start = offset + 1;
            }
            _ => {}
        }
    }
    parts.push(normalize(&list[start..]));
    parts
}

async fn postgres_schema(pool: &PgPool) -> TestResult<Schema> {
    let mut schema = Schema::default();
    let tables: Vec<String> = sqlx::query_scalar(
        "SELECT table_name::text FROM information_schema.tables WHERE table_schema='hot' AND table_type='BASE TABLE' AND table_name <> '_sqlx_migrations'",
    )
    .fetch_all(pool)
    .await?;
    schema.tables = tables.into_iter().collect();
    for row in sqlx::query(
        "SELECT table_name::text, column_name::text, is_nullable='NO', column_default FROM information_schema.columns WHERE table_schema='hot'",
    )
    .fetch_all(pool)
    .await?
    {
        let (table, column): (String, String) = (row.get(0), row.get(1));
        if !schema.tables.contains(&table) {
            continue;
        }
        let required: bool = row.get(2);
        let qualified = format!("{table}.{column}");
        let required = required && !NULLABILITY_ENFORCED_BY_TRIGGER.contains(&qualified.as_str());
        schema.columns.insert(format!("{qualified} not_null={required}"));
        // Generated identifiers come from the application on SQLite.
        let default: Option<String> = row.get(3);
        if default.is_some_and(|d| !(d.contains("uuid") && d.ends_with("()")) && !d.starts_with("nextval(")) {
            schema.defaults.insert(qualified);
        }
    }
    let constraint_columns = |side: &str| {
        format!(
            "(SELECT string_agg(a.attname, ',' ORDER BY k.ord) FROM unnest(c.{side}key) WITH ORDINALITY k(n, ord) \
             JOIN pg_attribute a ON a.attrelid = c.{}relid AND a.attnum = k.n)",
            if side == "con" { "con" } else { "conf" }
        )
    };
    for row in sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT c.contype::text, c.conrelid::regclass::text, {}, c.confrelid::regclass::text, {}, c.confdeltype::text \
         FROM pg_constraint c WHERE c.connamespace='hot'::regnamespace AND c.contype IN ('p','f')",
        constraint_columns("con"),
        constraint_columns("conf")
    )))
    .fetch_all(pool)
    .await?
    {
        let kind: String = row.get(0);
        let table = row.get::<String, _>(1).replace("hot.", "").replace('"', "");
        let columns: String = row.get(2);
        if !schema.tables.contains(&table) {
            continue;
        }
        if kind == "p" {
            if AUTOINCREMENT_KEY_TABLES.contains(&table.as_str()) {
                continue;
            }
            schema.primary_keys.insert(format!("{table}({columns})"));
            continue;
        }
        let parent = row.get::<String, _>(3).replace("hot.", "").replace('"', "");
        let parent_columns: String = row.get(4);
        // RESTRICT and NO ACTION only differ for deferred checks, which no
        // constraint here uses.
        let delete = match row.get::<String, _>(5).as_str() {
            "c" => "cascade",
            "n" => "set null",
            "d" => "set default",
            _ => "restrict",
        };
        schema.foreign_keys.insert(format!(
            "{table}({columns}) -> {parent}({parent_columns}) on delete {delete}"
        ));
    }
    let include = Regex::new(r"INCLUDE \(([^)]*)\)").unwrap();
    for row in sqlx::query(
        "SELECT t.relname::text, i.relname::text, ix.indisunique, ix.indisprimary, \
         pg_get_indexdef(ix.indexrelid), COALESCE(pg_get_expr(ix.indpred, ix.indrelid), ''), \
         EXISTS (SELECT 1 FROM pg_constraint c WHERE c.conindid = ix.indexrelid) \
         FROM pg_index ix JOIN pg_class i ON i.oid = ix.indexrelid JOIN pg_class t ON t.oid = ix.indrelid \
         WHERE t.relnamespace = 'hot'::regnamespace",
    )
    .fetch_all(pool)
    .await?
    {
        let (table, name): (String, String) = (row.get(0), row.get(1));
        let (unique, primary, constraint): (bool, bool, bool) = (row.get(2), row.get(3), row.get(6));
        let compared_key = primary && AUTOINCREMENT_KEY_TABLES.contains(&table.as_str());
        if (primary && !compared_key)
            || !schema.tables.contains(&table)
            || POSTGRES_ONLY_INDEXES.contains(&name.as_str())
        {
            continue;
        }
        let definition: String = row.get(4);
        let open = definition.find(" USING ").and_then(|at| definition[at..].find('(').map(|o| at + o));
        let mut columns = split_columns(parenthesized(&definition, open.expect("index columns")));
        // A covering INCLUDE column is SQLite's trailing key column.
        if let Some(included) = include.captures(&definition) {
            columns.extend(split_columns(&included[1]));
        }
        let predicate: String = row.get(5);
        let value = if DIALECT_INDEXES.contains(&name.as_str()) {
            table.clone()
        } else {
            shape(&table, unique, &columns, &predicate)
        };
        schema.index_shapes.insert(name.clone(), value);
        if !constraint {
            schema.index_names.insert(name);
        }
    }
    for table in REFERENCE_TABLES {
        let columns: Vec<String> = sqlx::query_scalar(
            // UUIDs compare as SQLite renders its blobs: bare lowercase hex.
            "SELECT CASE WHEN data_type = 'uuid' THEN format('replace(%I::text, ''-'', '''')', column_name) \
             ELSE format('%I::text', column_name) END FROM information_schema.columns WHERE table_schema='hot' AND table_name=$1 \
             AND column_default IS DISTINCT FROM 'uuidv7()' AND column_name NOT IN ('created_at','updated_at','last_successful_sync_time') ORDER BY column_name",
        )
        .bind(table)
        .fetch_all(pool)
        .await?;
        let select = columns
            .iter()
            .map(|c| format!("COALESCE({c}, 'NULL')"))
            .collect::<Vec<_>>()
            .join(" || '|' || ");
        let mut rows: Vec<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT {select} FROM hot.{table}"
        )))
        .fetch_all(pool)
        .await?;
        rows = rows
            .into_iter()
            .map(|r| r.replace("|true", "|1").replace("|false", "|0"))
            .collect();
        rows.sort();
        schema.reference_rows.insert(table.to_string(), rows);
    }
    for row in sqlx::query(
        "SELECT c.relname::text, t.tgname::text FROM pg_trigger t JOIN pg_class c ON c.oid = t.tgrelid \
         WHERE NOT t.tgisinternal AND c.relnamespace = 'hot'::regnamespace",
    )
    .fetch_all(pool)
    .await?
    {
        let (table, trigger): (String, String) = (row.get(0), row.get(1));
        match trigger.as_str() {
            "immutable_resource_scope" => schema.immutable_guards.insert(table),
            "resource_scope_integrity" => schema.integrity_guards.insert(table),
            _ => false,
        };
    }
    Ok(schema)
}

async fn sqlite_schema(pool: &SqlitePool) -> TestResult<Schema> {
    let mut schema = Schema::default();
    let where_clause = Regex::new(r"(?is)\bwhere\b(.*)$").unwrap();
    let tables: Vec<String> = sqlx::query_scalar(
        "SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' AND name NOT LIKE '_sqlx%'",
    )
    .fetch_all(pool)
    .await?;
    for table in &tables {
        let mut primary = BTreeMap::new();
        for row in sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT name, \"notnull\", pk, dflt_value FROM pragma_table_info('{table}')"
        )))
        .fetch_all(pool)
        .await?
        {
            let column: String = row.get(0);
            let (not_null, pk): (bool, i64) = (row.get(1), row.get(2));
            schema
                .columns
                .insert(format!("{table}.{column} not_null={}", not_null || pk > 0));
            if pk > 0 {
                primary.insert(pk, column.clone());
            }
            let default: Option<String> = row.get(3);
            if default.is_some_and(|d| !d.eq_ignore_ascii_case("null")) {
                schema.defaults.insert(format!("{table}.{column}"));
            }
        }
        if !primary.is_empty() && !AUTOINCREMENT_KEY_TABLES.contains(&table.as_str()) {
            schema.primary_keys.insert(format!(
                "{table}({})",
                primary.into_values().collect::<Vec<_>>().join(",")
            ));
        }
        let mut references: BTreeMap<i64, (String, Vec<String>, Vec<String>, String)> =
            BTreeMap::new();
        for row in sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT id, \"table\", \"from\", \"to\", on_delete FROM pragma_foreign_key_list('{table}') ORDER BY id, seq"
        )))
        .fetch_all(pool)
        .await?
        {
            let entry = references.entry(row.get(0)).or_insert_with(|| {
                (row.get(1), Vec::new(), Vec::new(), row.get(4))
            });
            entry.1.push(row.get(2));
            entry.2.push(row.get(3));
        }
        for (parent, columns, parent_columns, delete) in references.into_values() {
            let delete = match delete.as_str() {
                "CASCADE" => "cascade",
                "SET NULL" => "set null",
                "SET DEFAULT" => "set default",
                _ => "restrict",
            };
            schema.foreign_keys.insert(format!(
                "{table}({}) -> {parent}({}) on delete {delete}",
                columns.join(","),
                parent_columns.join(",")
            ));
        }
        for row in sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT l.name, l.\"unique\", l.origin, m.sql FROM pragma_index_list('{table}') l \
             LEFT JOIN sqlite_master m ON m.name = l.name"
        )))
        .fetch_all(pool)
        .await?
        {
            let (name, unique, origin): (String, bool, String) =
                (row.get(0), row.get(1), row.get(2));
            if origin == "pk" {
                continue;
            }
            let sql: Option<String> = row.get(3);
            let columns = match &sql {
                Some(sql) => {
                    let open = sql
                        .find(" ON ")
                        .or_else(|| sql.find(" on "))
                        .expect("ON clause");
                    let open = open + sql[open..].find('(').expect("index columns");
                    split_columns(parenthesized(sql, open))
                }
                None => sqlx::query_scalar::<_, String>(sqlx::AssertSqlSafe(format!(
                    "SELECT name FROM pragma_index_info('{name}') ORDER BY seqno"
                )))
                .fetch_all(pool)
                .await?
                .iter()
                .map(|c| normalize(c))
                .collect(),
            };
            let predicate = sql
                .as_deref()
                .and_then(|sql| where_clause.captures(sql))
                .map(|c| c[1].to_string())
                .unwrap_or_default();
            let value = if DIALECT_INDEXES.contains(&name.as_str()) {
                table.clone()
            } else {
                shape(table, unique, &columns, &predicate)
            };
            schema.index_shapes.insert(name.clone(), value);
            if sql.is_some() {
                schema.index_names.insert(name);
            }
        }
    }
    schema.tables = tables.into_iter().collect();
    for table in REFERENCE_TABLES {
        let columns: Vec<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT CASE WHEN lower(type) = 'blob' THEN 'nullif(lower(hex(' || name || ')), '''')' \
             ELSE 'CAST(' || name || ' AS TEXT)' END FROM pragma_table_info('{table}') WHERE NOT (pk = 1 AND lower(type) = 'blob') \
             AND name NOT IN ('created_at','updated_at','last_successful_sync_time') ORDER BY name"
        )))
        .fetch_all(pool)
        .await?;
        let select = columns
            .iter()
            .map(|c| format!("COALESCE({c}, 'NULL')"))
            .collect::<Vec<_>>()
            .join(" || '|' || ");
        let mut rows: Vec<String> =
            sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT {select} FROM {table}")))
                .fetch_all(pool)
                .await?;
        rows.sort();
        schema.reference_rows.insert(table.to_string(), rows);
    }
    // SQLite trigger names are schema-wide, so each guard carries its table.
    for (table, trigger) in sqlx::query_as::<_, (String, String)>(
        "SELECT tbl_name, name FROM sqlite_master WHERE type='trigger'",
    )
    .fetch_all(pool)
    .await?
    {
        if trigger.ends_with("immutable_resource_scope")
            || trigger.starts_with("immutable_")
            || trigger.ends_with("scope_immutable")
        {
            schema.immutable_guards.insert(table.clone());
        }
        if trigger.contains("resource_scope_integrity") || trigger.contains("scope_insert") {
            schema.integrity_guards.insert(table);
        }
    }
    Ok(schema)
}

/// Report each side's extra entries rather than one opaque inequality.
fn assert_same<T: Ord + std::fmt::Debug + Clone>(
    what: &str,
    postgres: impl IntoIterator<Item = T>,
    sqlite: impl IntoIterator<Item = T>,
) {
    let (postgres, sqlite): (BTreeSet<T>, BTreeSet<T>) =
        (postgres.into_iter().collect(), sqlite.into_iter().collect());
    let only_postgres: Vec<_> = postgres.difference(&sqlite).cloned().collect();
    let only_sqlite: Vec<_> = sqlite.difference(&postgres).cloned().collect();
    assert!(
        only_postgres.is_empty() && only_sqlite.is_empty(),
        "{what} differ\n  only in PostgreSQL: {only_postgres:#?}\n  only in SQLite: {only_sqlite:#?}"
    );
}

struct ParityDatabase {
    cluster: PgPool,
    database: String,
}

impl Drop for ParityDatabase {
    fn drop(&mut self) {
        let options = self.cluster.connect_options().as_ref().clone();
        let database = self.database.clone();
        let _ = std::thread::spawn(move || {
            if let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                runtime.block_on(async {
                    if let Ok(pool) = PgPoolOptions::new()
                        .max_connections(1)
                        .connect_with(options)
                        .await
                    {
                        let _ = pool
                            .execute(sqlx::AssertSqlSafe(format!(
                                "DROP DATABASE IF EXISTS {database} WITH (FORCE)"
                            )))
                            .await;
                        pool.close().await;
                    }
                });
            }
        })
        .join();
    }
}

#[tokio::test]
async fn fresh_databases_share_one_schema() -> TestResult {
    let Ok(admin_uri) = std::env::var("HOT_TEST_POSTGRES_URI") else {
        eprintln!("skipping schema parity test: HOT_TEST_POSTGRES_URI not set");
        return Ok(());
    };
    let cluster = PgPoolOptions::new()
        .max_connections(1)
        .connect(&admin_uri)
        .await?;
    let database = format!("hot_parity_{}", Uuid::new_v4().simple());
    cluster
        .execute(sqlx::AssertSqlSafe(format!("CREATE DATABASE {database}")))
        .await?;
    let _cleanup = ParityDatabase {
        cluster,
        database: database.clone(),
    };
    let mut uri = url::Url::parse(&admin_uri)?;
    uri.set_path(&format!("/{database}"));
    let postgres = PgPoolOptions::new()
        .max_connections(1)
        .after_connect(|connection, _| {
            Box::pin(async move {
                connection.execute("SET search_path = hot, public").await?;
                Ok(())
            })
        })
        .connect(uri.as_str())
        .await?;
    postgres.execute("CREATE SCHEMA hot").await?;
    Migrator::new(migrations("postgres"))
        .await?
        .run(&postgres)
        .await?;

    let directory = tempfile::tempdir()?;
    let sqlite = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(
            SqliteConnectOptions::new()
                .filename(directory.path().join("parity.db"))
                .create_if_missing(true)
                .foreign_keys(true),
        )
        .await?;
    Migrator::new(migrations("sqlite"))
        .await?
        .run(&sqlite)
        .await?;

    let (pg, sl) = (
        postgres_schema(&postgres).await?,
        sqlite_schema(&sqlite).await?,
    );
    postgres.close().await;
    assert_same("tables", pg.tables, sl.tables);
    assert_same("columns and nullability", pg.columns, sl.columns);
    assert_same("primary keys", pg.primary_keys, sl.primary_keys);
    assert_same("foreign keys", pg.foreign_keys, sl.foreign_keys);
    assert_same("column defaults", pg.defaults, sl.defaults);
    assert_same(
        "index definitions",
        pg.index_shapes.values().cloned(),
        sl.index_shapes.values().cloned(),
    );
    // Constraint-backed indexes get engine-generated names; the rest match.
    let unnamed: Vec<_> = pg.index_names.difference(&sl.index_names).collect();
    assert!(
        unnamed.is_empty(),
        "PostgreSQL index names missing from SQLite: {unnamed:?}"
    );
    assert_eq!(
        pg.reference_rows, sl.reference_rows,
        "reference rows differ"
    );
    assert_same(
        "immutable ownership guards",
        pg.immutable_guards,
        sl.immutable_guards,
    );
    assert_same(
        "ownership integrity guards",
        pg.integrity_guards,
        sl.integrity_guards,
    );
    Ok(())
}

/// `db::test_db` drops the integrity guards along with foreign keys; a real
/// SQLite database refuses cross-environment rows the way PostgreSQL does.
#[tokio::test]
async fn sqlite_integrity_guards_refuse_foreign_ownership() -> TestResult {
    let directory = tempfile::tempdir()?;
    let sqlite = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(
            SqliteConnectOptions::new()
                .filename(directory.path().join("guards.db"))
                .create_if_missing(true)
                .foreign_keys(true),
        )
        .await?;
    Migrator::new(migrations("sqlite"))
        .await?
        .run(&sqlite)
        .await?;
    let task = sqlx::query(
        "INSERT INTO task (task_id, env_id, stream_id, build_id, function_name) VALUES (?, ?, ?, ?, 'f')",
    )
    .bind(Uuid::new_v4())
    .bind(Uuid::new_v4())
    .bind(Uuid::new_v4())
    .bind(Uuid::new_v4())
    .execute(&sqlite)
    .await
    .unwrap_err();
    assert!(
        task.to_string().contains("Build ownership mismatch"),
        "{task}"
    );
    let store =
        sqlx::query("INSERT INTO store_map (store_id, org_id, env_id, name) VALUES (?, ?, ?, 's')")
            .bind(Uuid::new_v4())
            .bind(Uuid::new_v4())
            .bind(Uuid::new_v4())
            .execute(&sqlite)
            .await
            .unwrap_err();
    assert!(
        store.to_string().contains("Environment ownership mismatch"),
        "{store}"
    );
    Ok(())
}

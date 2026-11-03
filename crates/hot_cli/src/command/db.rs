//! `hot db` — database administration (status, migrate, port-v1-to-v2,
//! redeploy).

use hot::storage_move::MovedObject;
use hot::val::Val;
use std::collections::HashSet;
use std::ffi::OsStr;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use tracing::info;
use uuid::Uuid;

pub(crate) async fn run_db(
    db_cmd: &str,
    conf: &Val,
    providers: &crate::CliProviders,
) -> Result<(), String> {
    use hot::db::{
        DatabaseType, create_db_pool, get_db_uri_from_conf, redact_password, test_connection,
    };

    match db_cmd {
        "status" => {
            println!("Database Status:");
            println!("  URI: {}", redact_password(&get_db_uri_from_conf(conf)));

            match create_db_pool(conf).await {
                Ok(db) => match test_connection(&db).await {
                    Ok(_) => println!("  Connection: OK"),
                    Err(e) => {
                        println!("  Connection: FAILED - {}", e);
                        return Ok(());
                    }
                },
                Err(e) => {
                    println!("  Connection: FAILED - {}", e);
                    return Ok(());
                }
            }
            match hot::db::migration_state(conf).await {
                Ok(Some(state)) => match state.conflict() {
                    Some(conflict) => println!("  Migrations: {conflict}"),
                    None if state.pending.is_empty() => {
                        println!("  Migrations: up to date ({} applied)", state.applied.len())
                    }
                    None => {
                        let versions: Vec<String> =
                            state.pending.iter().map(|v| format!("{v:03}")).collect();
                        println!(
                            "  Migrations: {} pending ({}). Run `hot db migrate`.",
                            state.pending.len(),
                            versions.join(", ")
                        );
                    }
                },
                // PostgreSQL keeps the ledger from the runtime login.
                Ok(None) => println!(
                    "  Migrations: not shown; pass --db.migration-uri (the schema owner) to read the ledger"
                ),
                Err(e) => println!("  Migrations: unknown - {}", e),
            }
        }
        "migrate" => {
            info!("Running database migrations...");
            match crate::run_migrations_with_bootstrap(conf, providers).await {
                Ok(_) => {
                    println!("Migrations completed successfully");
                    if matches!(
                        DatabaseType::from_uri(&get_db_uri_from_conf(conf)),
                        Ok(DatabaseType::Postgres)
                    ) {
                        // New tables have no runtime grants until provisioning
                        // runs again, and services fail every request without them.
                        println!(
                            "Rerun resources/db/postgres/provision_runtime.sql for the runtime \
                             login before starting services: \
                             https://hot.dev/docs/configuration/postgres"
                        );
                    }
                }
                Err(e) => {
                    crate::report_migration_failure("Migration failed", &e);
                    return Err("Migration failed".to_string());
                }
            }
        }
        // `hot db migrate --status`
        "migrate-status" => println!("{}", read_migration_status(conf).await?),
        "provision-store-indexes" => {
            let created = hot::db::rls::provision_store_indexes(conf).await?;
            println!("Store index provisioning completed: {created} index(es) created");
        }
        "port-v1-to-v2" => {
            let uri = get_db_uri_from_conf(conf);
            let db_type = DatabaseType::from_uri(&uri).map_err(|e| e.to_string())?;
            match db_type {
                DatabaseType::Sqlite => {
                    info!("Porting Hot 1.x SQLite database to Hot 2...");
                    let report = hot::db::port::port_v1_sqlite_to_v2(conf)
                        .await
                        .map_err(|e| format!("Port failed: {}", e))?;
                    print_port_report(&report);
                }
                DatabaseType::Postgres => {
                    return Err(
                        "hot db port-v1-to-v2 is implemented for SQLite only. Hot 2 does not \
                         auto-port Postgres databases; point Hot 2 at a fresh Postgres database \
                         (or schema). \
                         See https://hot.dev/docs/migrations#upgrading-from-hot-1x-to-hot-2"
                            .to_string(),
                    );
                }
            }
        }
        _ => {
            return Err(format!(
                "Unknown database command: {}. Available commands: status, migrate, provision-store-indexes, port-v1-to-v2",
                db_cmd
            ));
        }
    }

    Ok(())
}

/// What `hot db migrate --status` prints: one JSON line saying how far the
/// database has migrated. `level` is the highest migration in its ledger,
/// whichever Hot applied it, so a release older than the database can still say
/// so; 0 means none has run. `latest` is the highest migration this Hot has.
fn migration_status_line(state: &hot::db::MigrationState) -> String {
    let highest = |lists: [&[i64]; 3]| lists.into_iter().flatten().max().copied().unwrap_or(0);
    serde_json::json!({
        "level": highest([&state.applied, &state.modified, &state.unknown]),
        "latest": highest([&state.applied, &state.modified, &state.pending]),
        "pending": state.pending,
        "modified": state.modified,
        "unknown": state.unknown,
    })
    .to_string()
}

/// Read the database's migration level, changing nothing. The answer is for
/// tools to trust over what they last recorded, so anything short of reading
/// the right ledger is an error. A database another Hot migrated isn't one:
/// saying how far it went is the point.
async fn read_migration_status(conf: &Val) -> Result<String, String> {
    use hot::db::{DatabaseType, get_db_uri_from_conf};

    // Hot ignores a migration URI beside any other runtime database, so the
    // ledger below would be that database's, not the migration login's.
    if !conf.get_str_or_default("db.migration-uri", "").is_empty()
        && !matches!(
            DatabaseType::from_uri(&get_db_uri_from_conf(conf)),
            Ok(DatabaseType::Postgres)
        )
    {
        return Err(
            "Cannot tell the migration level: db.migration-uri needs a PostgreSQL db.uri naming \
             the same database"
                .to_string(),
        );
    }
    let state = hot::db::migration_state(conf)
        .await
        .map_err(|e| format!("Cannot tell the migration level: {e}"))?
        .ok_or(
            "Cannot tell the migration level: PostgreSQL keeps the ledger from the runtime \
             login. Pass --db.migration-uri (the schema owner)",
        )?;
    Ok(migration_status_line(&state))
}

/// `hot db redeploy`: build and redeploy the deployments that another major
/// version of Hot built (see `hot::redeploy`).
pub(crate) async fn run_db_redeploy(
    conf: &Val,
    dry_run: bool,
    notify: bool,
    org: Option<String>,
) -> Result<(), String> {
    use hot::redeploy::RedeployResult;

    let db = hot::db::create_db_pool(conf)
        .await
        .map_err(|e| format!("Cannot connect to the database: {e}"))?;
    let options = hot::redeploy::RedeployOptions {
        dry_run,
        notify,
        org,
    };
    let reports = hot::redeploy::redeploy(&db, conf, &options).await?;
    let runtime = hot::version::current_runtime_version();
    if reports.is_empty() {
        println!("Every deployment runs on Hot {runtime}.");
        return Ok(());
    }

    println!(
        "Hot {runtime} doesn't run these {} deployment(s), built with another major version:",
        reports.len()
    );
    let (mut redeployed, mut failed, mut skipped) = (0, 0, 0);
    for report in &reports {
        let name = format!(
            "{}/{}/{} (Hot {})",
            report.org_slug, report.env_name, report.project_name, report.engine_version
        );
        match &report.result {
            RedeployResult::Redeployed { build_id: Some(id) } => {
                redeployed += 1;
                println!("  {name}: redeployed as build {id}");
            }
            RedeployResult::Redeployed { build_id: None } => {
                redeployed += 1;
                println!("  {name}: builds and checks");
            }
            RedeployResult::Failed { reason, notified } => {
                failed += 1;
                let first = reason.lines().next().unwrap_or_default();
                if notify && !dry_run {
                    println!("  {name}: FAILED, {notified} admin(s) emailed: {first}");
                } else {
                    println!("  {name}: FAILED: {first}");
                }
            }
            RedeployResult::Skipped { reason } => {
                skipped += 1;
                println!("  {name}: skipped, {reason}");
            }
        }
    }
    if dry_run {
        println!(
            "Dry run: {redeployed} would redeploy, {failed} fail, {skipped} skipped. Nothing changed."
        );
    } else {
        println!(
            "Redeployed {redeployed}, failed {failed}, skipped {skipped}. A worker activates each new build, and the old one serves until then. A failed deployment's work waits until its project is redeployed."
        );
    }
    Ok(())
}

fn print_port_report(report: &hot::db::port::PortReport) {
    use hot::db::port::SkipReason;

    println!("Hot 1.x \u{2192} Hot 2 SQLite port complete.");
    println!("  Backup: {}", report.backup_path.display());
    println!("  v2 db:  {}", report.db_path.display());
    println!(
        "  Copied: {} rows across {} table(s).",
        report.total_rows_copied(),
        report.copied_tables.len()
    );
    for t in &report.copied_tables {
        if t.rows_copied == 0 && t.v1_only_columns.is_empty() && t.v2_only_columns.is_empty() {
            continue;
        }
        let mut notes = Vec::new();
        if !t.v1_only_columns.is_empty() {
            notes.push(format!(
                "dropped v1 columns: {}",
                t.v1_only_columns.join(", ")
            ));
        }
        if !t.v2_only_columns.is_empty() {
            notes.push(format!(
                "v2-only columns left at default: {}",
                t.v2_only_columns.join(", ")
            ));
        }
        let suffix = if notes.is_empty() {
            String::new()
        } else {
            format!(" ({})", notes.join("; "))
        };
        println!("    {}: {} rows{}", t.table, t.rows_copied, suffix);
    }
    let seed_skipped: Vec<&str> = report
        .skipped_tables
        .iter()
        .filter(|t| t.reason == SkipReason::SeedTable)
        .map(|t| t.table.as_str())
        .collect();
    if !seed_skipped.is_empty() {
        println!(
            "  Skipped (Hot 2 seeds these tables): {}",
            seed_skipped.join(", ")
        );
    }
    if !report.dropped_v1_tables.is_empty() {
        println!(
            "  Dropped (no Hot 2 destination): {} rows in {} v1-only table(s):",
            report.total_rows_dropped(),
            report.dropped_v1_tables.len(),
        );
        for t in &report.dropped_v1_tables {
            println!(
                "    {}: {} rows (in v1 backup only)",
                t.table, t.rows_dropped
            );
        }
    }
}

/// Where `hot db move-storage` moves a role's objects.
pub(crate) struct MoveStorageTarget {
    pub to: String,
    pub path: String,
    pub bucket: String,
    pub prefix: String,
    pub region: String,
}

/// Records to leave behind in `hot db move-storage`: comma-separated file, blob
/// object, or build ids, for objects the operator has accepted can never move.
const MOVE_SKIP_ENV: &str = "HOT_MOVE_STORAGE_SKIP";

/// Names a file of more such ids, when `--skip-file` doesn't. A variable holds
/// only so many ids, and a file any number. This reads differently from other
/// `HOT_*_FILE` names: the file has one id per line, not the setting's value,
/// and it adds to the setting instead of standing in for it.
const MOVE_SKIP_FILE_ENV: &str = "HOT_MOVE_STORAGE_SKIP_FILE";

fn parse_move_skip(raw: &str) -> Result<HashSet<Uuid>, String> {
    raw.split(',')
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(|id| {
            Uuid::parse_str(id).map_err(|e| format!("{MOVE_SKIP_ENV} has a bad id '{id}': {e}"))
        })
        .collect()
}

/// The ids in a skip file, one per line. A file that can't be read, or a line
/// that isn't an id, refuses the move rather than running without the list.
/// Errors name the path and line, never what the line says, since this may be
/// the wrong file.
fn read_move_skip_file(path: &Path) -> Result<HashSet<Uuid>, String> {
    let unreadable =
        |e: std::io::Error| format!("Cannot read the skip file {}: {e}", path.display());
    let file = std::fs::File::open(path).map_err(unreadable)?;
    let mut ids = HashSet::new();
    for (index, line) in std::io::BufReader::new(file).lines().enumerate() {
        let line = line.map_err(unreadable)?;
        // Some editors save a UTF-8 byte order mark, which no id means.
        let line = line.strip_prefix('\u{feff}').unwrap_or(&line).trim();
        if line.is_empty() {
            continue;
        }
        let id = Uuid::parse_str(line).map_err(|_| {
            format!(
                "Line {} of the skip file {} is not an object id. The file lists one id per line",
                index + 1,
                path.display()
            )
        })?;
        ids.insert(id);
    }
    Ok(ids)
}

/// Everything a move leaves behind: the ids in `list`, the value of
/// `HOT_MOVE_STORAGE_SKIP`, and those in the skip file. `flag`, the value of
/// `--skip-file`, names that file ahead of `variable`, the value of
/// `HOT_MOVE_STORAGE_SKIP_FILE`. A variable set to nothing names no file, as it
/// does for `HOT_MOVE_STORAGE_SKIP`.
fn move_skip(
    list: &str,
    flag: Option<&str>,
    variable: Option<&OsStr>,
) -> Result<HashSet<Uuid>, String> {
    let mut skip = parse_move_skip(list)?;
    let file = flag
        .map(PathBuf::from)
        .or_else(|| variable.filter(|path| !path.is_empty()).map(PathBuf::from));
    if let Some(file) = file {
        skip.extend(read_move_skip_file(&file)?);
    }
    Ok(skip)
}

/// The manifest's destination. A file is appended to: running again skips what
/// already moved, so what earlier runs wrote is the only record of those objects.
fn open_manifest(manifest: Option<&str>) -> Result<Option<Box<dyn Write>>, String> {
    Ok(match manifest {
        None => None,
        Some("-") => Some(Box::new(std::io::stdout())),
        Some(path) => Some(Box::new(
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .map_err(|e| format!("Cannot write {path}: {e}"))?,
        )),
    })
}

/// One manifest line, whole and flushed, so it outlives a crash of this process.
fn write_manifest_line(out: &mut dyn Write, object: &MovedObject) -> Result<(), String> {
    let mut line = serde_json::to_vec(object).map_err(|e| e.to_string())?;
    line.push(b'\n');
    out.write_all(&line)
        .and_then(|()| out.flush())
        .map_err(|e| format!("Cannot write the manifest: {e}"))
}

/// A running scheduler holds this lock for as long as it is the active one, so
/// a lock nobody can take means Hot is up, and records it is using would change
/// under it. Holding the lock also keeps a scheduler started during the move
/// from dispatching work. SQLite has no such lock, and needs no check for its
/// single process.
async fn hold_while_hot_is_stopped(
    db: &hot::db::DatabasePool,
    lock_id: i64,
) -> Result<hot::db::AdvisoryLock, String> {
    hot::db::try_acquire_advisory_lock(db, lock_id)
        .await
        .map_err(|e| format!("Cannot check that Hot is stopped: {e}"))?
        .ok_or_else(|| {
            "A Hot scheduler is running. Stop Hot before moving its storage: services would \
             refuse the records that moved. --dry-run can run at any time."
                .to_string()
        })
}

/// Move one storage role's objects to another backend, record by record,
/// checking each copy. Run it while Hot is stopped, then switch the role's
/// configuration to the target. Exits with an error if any object failed;
/// running it again retries those and skips the ones already moved.
/// `skip_file` is the value of `--skip-file`.
pub(crate) async fn run_db_move_storage_with_skip_file(
    conf: &Val,
    role: &str,
    target: MoveStorageTarget,
    dry_run: bool,
    manifest: Option<String>,
    skip_file: Option<String>,
) -> Result<(), String> {
    use hot::storage_move::{MoveOptions, MoveSummary, MoveTarget, StorageRole, move_storage};

    let role = if role == "build" {
        StorageRole::Build
    } else {
        StorageRole::File
    };
    let kind = hot::file_storage::parse_storage_backend_name(&target.to, "--to")?;
    let target = MoveTarget {
        kind,
        path: target.path,
        bucket: target.bucket,
        prefix: target.prefix,
        region: target.region,
    };
    let options = MoveOptions {
        dry_run,
        skip: move_skip(
            &std::env::var(MOVE_SKIP_ENV).unwrap_or_default(),
            skip_file.as_deref(),
            std::env::var_os(MOVE_SKIP_FILE_ENV).as_deref(),
        )?,
    };
    let db = hot::db::create_db_pool(conf)
        .await
        .map_err(|e| format!("Cannot connect to the database: {e}"))?;
    let mut out = open_manifest(manifest.as_deref())?;
    let lock = if dry_run {
        None
    } else {
        let lock_id = hot_scheduler::server::SCHEDULER_ADVISORY_LOCK_ID;
        Some(hold_while_hot_is_stopped(&db, lock_id).await?)
    };
    let mut summary = MoveSummary::default();
    let moved = move_storage(
        &db,
        conf,
        role,
        &target,
        &options,
        &mut summary,
        &mut |object| {
            if let Some(error) = &object.error {
                eprintln!("{} {} did not move: {error}", object.kind, object.id);
            }
            if let Some(warning) = &object.warning {
                eprintln!("{} {} moved, but {warning}", object.kind, object.id);
            }
            match out.as_mut() {
                Some(out) => write_manifest_line(out, object),
                None => Ok(()),
            }
        },
    )
    .await;
    if let Some(lock) = lock {
        lock.release().await;
    }
    let summary_line = serde_json::json!({ "summary": summary });
    eprintln!("{summary_line}");
    moved?;
    if summary.failed > 0 {
        return Err(format!(
            "{} object(s) did not move, and their records did not change. Fix the cause and run \
             the command again: objects that already moved are skipped. To leave behind an object \
             that can never move, because its bytes are gone, say, list its id in {MOVE_SKIP_ENV}, \
             or on its own line in a file named by --skip-file or {MOVE_SKIP_FILE_ENV}. Hot can't \
             read what stays behind: a file left there can only be deleted, and a build must be \
             deployed again",
            summary.failed
        ));
    }
    if !dry_run {
        let settings: Vec<String> = target
            .settings(role)?
            .into_iter()
            .filter(|(key, value)| !(key.ends_with(".region") && value.is_empty()))
            .map(|(key, value)| format!("{key}={value}"))
            .collect();
        eprintln!(
            "Before starting Hot, set exactly: {}. Hot finds internal blobs and builds through \
             these settings, not through their records. Running this command again afterwards \
             names any setting that differs.",
            settings.join(", ")
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn object(status: &'static str) -> MovedObject {
        MovedObject {
            kind: "file",
            id: Uuid::nil(),
            org_id: Uuid::nil(),
            env_id: None,
            from: "from".into(),
            to: "to".into(),
            bytes: 1,
            digest: "digest".into(),
            status,
            error: None,
            warning: None,
        }
    }

    fn parse_status(line: &str) -> serde_json::Value {
        assert_eq!(line.lines().count(), 1, "{line}");
        serde_json::from_str(line).unwrap()
    }

    #[test]
    fn the_migration_level_is_the_highest_migration_in_the_ledger() {
        use hot::db::MigrationState;

        // Partly migrated: this release has one more to apply.
        let status = parse_status(&migration_status_line(&MigrationState {
            applied: vec![1, 2],
            pending: vec![3],
            ..Default::default()
        }));
        assert_eq!(status["level"], 2);
        assert_eq!(status["latest"], 3);
        assert_eq!(status["pending"], serde_json::json!([3]));

        // A newer Hot migrated it, so this release says so instead of failing.
        let status = parse_status(&migration_status_line(&MigrationState {
            applied: vec![1, 2],
            unknown: vec![3, 4],
            ..Default::default()
        }));
        assert_eq!(status["level"], 4);
        assert_eq!(status["latest"], 2);
        assert_eq!(status["unknown"], serde_json::json!([3, 4]));

        // A migration applied from another file still counts as applied.
        let status = parse_status(&migration_status_line(&MigrationState {
            applied: vec![1],
            modified: vec![2],
            ..Default::default()
        }));
        assert_eq!(status["level"], 2);
        assert_eq!(status["latest"], 2);
        assert_eq!(status["modified"], serde_json::json!([2]));

        // No migration has run.
        let status = parse_status(&migration_status_line(&MigrationState {
            pending: vec![1, 2],
            ..Default::default()
        }));
        assert_eq!(status["level"], 0);
        assert_eq!(status["latest"], 2);
    }

    #[tokio::test]
    async fn the_migration_status_reads_a_sqlite_database_without_changing_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hot.db");
        let conf = hot::val!({"db": {"uri": format!("sqlite:{}", path.display())}});

        // Nothing to read: it can't tell, and it doesn't make the file.
        assert!(read_migration_status(&conf).await.is_err());
        assert!(!path.exists());

        // A database no migration ran against has no ledger.
        std::fs::write(&path, b"").unwrap();
        let status = parse_status(&read_migration_status(&conf).await.unwrap());
        assert_eq!(status["level"], 0);
        assert_eq!(status["pending"][0], 1);
        assert_eq!(std::fs::read(&path).unwrap(), b"");

        hot::db::run_migrations(&conf).await.unwrap();
        let before = std::fs::read(&path).unwrap();
        let status = parse_status(&read_migration_status(&conf).await.unwrap());
        assert!(status["level"].as_i64().unwrap() > 0, "{status}");
        assert_eq!(status["level"], status["latest"]);
        assert_eq!(status["pending"], serde_json::json!([]));
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    #[tokio::test]
    async fn the_migration_status_refuses_a_login_it_cannot_read_the_ledger_with() {
        // Nothing here reaches a server: these fail before connecting.
        let runtime = hot::val!({"db": {"uri": "postgres://hot_app@127.0.0.1:1/hot"}});
        let error = read_migration_status(&runtime).await.unwrap_err();
        assert!(error.contains("--db.migration-uri"), "{error}");

        let beside_sqlite = hot::val!({"db": {
            "uri": "sqlite::memory:",
            "migration-uri": "postgres://hot_owner@127.0.0.1:1/hot"
        }});
        let error = read_migration_status(&beside_sqlite).await.unwrap_err();
        assert!(error.contains("PostgreSQL db.uri"), "{error}");
    }

    #[tokio::test]
    async fn the_migration_login_alone_reads_a_postgres_ledger() {
        let Ok(uri) = std::env::var("HOT_TEST_POSTGRES_URI") else {
            eprintln!("skipping: HOT_TEST_POSTGRES_URI not set");
            return;
        };
        // As the migrate job runs: both URIs are the schema owner's.
        let conf = |schema: &str| hot::val!({"db": {"uri": uri.clone(), "migration-uri": uri.clone(), "schema": schema}});
        let migrated = conf("hot");
        hot::db::run_migrations(&migrated).await.unwrap();
        let status = parse_status(&read_migration_status(&migrated).await.unwrap());
        assert!(status["level"].as_i64().unwrap() > 0, "{status}");
        assert_eq!(status["level"], status["latest"]);
        assert_eq!(status["pending"], serde_json::json!([]));

        // A schema with no ledger, which reading doesn't create.
        let schema = format!("no_ledger_{}", Uuid::new_v4().simple());
        let status = parse_status(&read_migration_status(&conf(&schema)).await.unwrap());
        assert_eq!(status["level"], 0);
    }

    #[test]
    fn the_skip_list_is_a_comma_separated_list_of_ids() {
        assert!(parse_move_skip("").unwrap().is_empty());
        let (a, b) = (Uuid::now_v7(), Uuid::now_v7());
        let parsed = parse_move_skip(&format!("{a}, {b},")).unwrap();
        assert_eq!(parsed, HashSet::from([a, b]));
        let error = parse_move_skip("not-an-id").unwrap_err();
        assert!(error.contains(MOVE_SKIP_ENV) && error.contains("not-an-id"));
    }

    /// A skip file holding `contents`, and the directory that keeps it.
    fn skip_file(contents: impl AsRef<[u8]>) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("skip");
        std::fs::write(&path, contents).unwrap();
        (dir, path)
    }

    #[test]
    fn the_skip_file_lists_one_id_per_line() {
        let (a, b, c) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
        let (_dir, path) = skip_file(format!("{a}\n\n  {b}  \r\n{a}\n{c}"));
        let expected = HashSet::from([a, b, c]);
        assert_eq!(read_move_skip_file(&path).unwrap(), expected);

        let (_dir, path) = skip_file("");
        assert!(read_move_skip_file(&path).unwrap().is_empty());

        // As an editor might save it.
        let (_dir, path) = skip_file(format!("\u{feff}{a}\n"));
        assert_eq!(read_move_skip_file(&path).unwrap(), HashSet::from([a]));
    }

    #[test]
    fn the_skip_file_holds_more_ids_than_one_argument_can() {
        // Linux takes at most 128 KiB in one environment variable.
        let ids: Vec<Uuid> = (0..4_000).map(|_| Uuid::new_v4()).collect();
        let lines: Vec<String> = ids.iter().map(Uuid::to_string).collect();
        let contents = lines.join("\n");
        assert!(contents.len() > 128 * 1024);
        let (_dir, path) = skip_file(contents);
        let read = read_move_skip_file(&path).unwrap();
        assert_eq!(read, ids.into_iter().collect::<HashSet<_>>());
    }

    #[test]
    fn a_line_that_is_not_an_id_refuses_the_skip_file() {
        let good = Uuid::now_v7();
        let (a, b) = (Uuid::now_v7(), Uuid::now_v7());
        for (contents, line) in [
            (format!("{good}\n\nprivate-value\n"), "Line 3"),
            // The setting's commas don't carry over.
            (format!("{a},{b}\n"), "Line 1"),
        ] {
            let (_dir, path) = skip_file(&contents);
            let error = read_move_skip_file(&path).unwrap_err();
            assert!(error.contains(line), "{error}");
            assert!(error.contains(path.to_str().unwrap()), "{error}");
            // It may be the wrong file, so nothing of it reaches the logs.
            assert!(!error.contains("private-value"), "{error}");
            assert!(!error.contains(&a.to_string()), "{error}");
        }
    }

    #[test]
    fn a_skip_file_that_cannot_be_read_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing");
        let (_binary, binary) = skip_file([0xff, 0xfe, 0x00, b'\n']);
        for path in [missing.as_path(), dir.path(), binary.as_path()] {
            let error = read_move_skip_file(path).unwrap_err();
            assert!(error.starts_with("Cannot read the skip file"), "{error}");
            assert!(error.contains(path.to_str().unwrap()), "{error}");
        }
    }

    #[test]
    fn the_skip_file_adds_to_the_list_and_the_flag_names_it_first() {
        let (listed, in_flag, in_variable) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
        let list = listed.to_string();
        let (_flag, flag) = skip_file(format!("{in_flag}\n"));
        let flag = flag.to_str().unwrap();
        let (_variable, variable) = skip_file(format!("{in_variable}\n"));
        let variable = variable.as_os_str();

        let skip = |flag, variable| move_skip(&list, flag, variable);
        assert_eq!(skip(None, None).unwrap(), HashSet::from([listed]));
        assert_eq!(
            skip(Some(flag), None).unwrap(),
            HashSet::from([listed, in_flag])
        );
        assert_eq!(
            skip(None, Some(variable)).unwrap(),
            HashSet::from([listed, in_variable])
        );
        assert_eq!(
            skip(Some(flag), Some(variable)).unwrap(),
            HashSet::from([listed, in_flag])
        );

        // A variable the flag overrides isn't read.
        let missing = OsStr::new("/no/such/skip/file");
        assert_eq!(
            skip(Some(flag), Some(missing)).unwrap(),
            HashSet::from([listed, in_flag])
        );
        // Set to nothing, as a command clears it, the variable names no file.
        assert_eq!(
            skip(None, Some(OsStr::new(""))).unwrap(),
            HashSet::from([listed])
        );

        // Anything else that can't be read refuses, whichever way it's named.
        assert!(skip(None, Some(missing)).is_err());
        assert!(skip(Some("/no/such/skip/file"), None).is_err());
        assert!(skip(Some(""), None).is_err());
        assert!(skip(None, Some(OsStr::new(" "))).is_err());
        // A bad list still refuses when a file is fine.
        assert!(move_skip("not-an-id", Some(flag), None).is_err());
    }

    /// A SQLite database with one active file record, and that record's id.
    async fn database_with_a_file(path: &Path) -> (Val, Uuid) {
        let conf = hot::val!({
            "db": {"uri": format!("sqlite:{}", path.join("hot.db").display())},
            "file": {"storage": {"type": "local", "path": path.join("files").to_str().unwrap()}}
        });
        hot::db::run_migrations(&conf).await.unwrap();
        let db = hot::db::create_db_pool(&conf).await.unwrap();
        let data = hot::db::insert_test_data(&db).await.unwrap();
        let record = hot::db::file::insert_file_record(
            &db,
            "a.txt",
            1,
            None,
            None,
            "local",
            path.join("files/a.txt").to_str().unwrap(),
            data.org_id,
            Some(data.env_id),
            data.user_id,
            None,
        )
        .await
        .unwrap();
        (conf, record.file_id)
    }

    fn to_s3() -> MoveStorageTarget {
        MoveStorageTarget {
            to: "s3".into(),
            path: String::new(),
            bucket: "acme".into(),
            prefix: "files".into(),
            region: "us-east-1".into(),
        }
    }

    #[tokio::test]
    async fn the_objects_in_a_skip_file_stay_behind() {
        let dir = tempfile::tempdir().unwrap();
        let (conf, id) = database_with_a_file(dir.path()).await;
        let statuses = async |skip_file: Option<String>| {
            let manifest = dir.path().join(format!("{}.jsonl", Uuid::new_v4()));
            let manifest = manifest.to_str().unwrap().to_string();
            run_db_move_storage_with_skip_file(
                &conf,
                "file",
                to_s3(),
                true,
                Some(manifest.clone()),
                skip_file,
            )
            .await
            .unwrap();
            std::fs::read_to_string(manifest)
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
                .filter(|object| object["id"] == id.to_string())
                .map(|object| object["status"].as_str().unwrap().to_string())
                .collect::<Vec<_>>()
        };

        assert_eq!(statuses(None).await, ["planned"]);
        let (_skip, path) = skip_file(format!("{}\n{id}\n", Uuid::now_v7()));
        let path = path.to_str().unwrap().to_string();
        assert_eq!(statuses(Some(path)).await, ["skipped"]);
    }

    #[tokio::test]
    async fn a_bad_skip_file_stops_the_move_before_the_database() {
        let dir = tempfile::tempdir().unwrap();
        let database = dir.path().join("hot.db");
        let conf = hot::val!({"db": {"uri": format!("sqlite:{}", database.display())}});
        for dry_run in [true, false] {
            let error = run_db_move_storage_with_skip_file(
                &conf,
                "file",
                to_s3(),
                dry_run,
                None,
                Some(dir.path().join("missing").to_str().unwrap().to_string()),
            )
            .await
            .unwrap_err();
            assert!(error.starts_with("Cannot read the skip file"), "{error}");
        }
        assert!(!database.exists());
    }

    #[test]
    fn running_again_adds_to_the_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("move.jsonl");
        let path = path.to_str().unwrap();
        for status in ["moved", "planned"] {
            let mut out = open_manifest(Some(path)).unwrap().unwrap();
            write_manifest_line(&mut out, &object(status)).unwrap();
        }
        let manifest = std::fs::read_to_string(path).unwrap();
        let lines: Vec<serde_json::Value> = manifest
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(lines.len(), 2, "{manifest}");
        assert_eq!(lines[0]["status"], "moved");
        assert_eq!(lines[1]["status"], "planned");
        assert!(open_manifest(None).unwrap().is_none());
        assert!(open_manifest(Some("/no/such/directory/move.jsonl")).is_err());
    }

    #[tokio::test]
    async fn a_held_scheduler_lock_refuses_the_move() {
        let Ok(uri) = std::env::var("HOT_TEST_POSTGRES_URI") else {
            eprintln!("skipping: HOT_TEST_POSTGRES_URI not set");
            return;
        };
        let conf = hot::val!({"uri": uri, "schema": "hot", "rls": {"enforce": false}});
        let db = hot::db::create_db_pool(&conf).await.unwrap();
        // Its own key: tests of the real scheduler share this server.
        let lock_id = i64::from_be_bytes(Uuid::new_v4().as_bytes()[..8].try_into().unwrap());
        let scheduler = hold_while_hot_is_stopped(&db, lock_id).await.unwrap();
        let refused = hold_while_hot_is_stopped(&db, lock_id).await;
        assert!(refused.err().unwrap().contains("scheduler is running"));
        scheduler.release().await;
        hold_while_hot_is_stopped(&db, lock_id)
            .await
            .unwrap()
            .release()
            .await;
    }
}

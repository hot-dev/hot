//! `hot db` — database administration (status, migrate, port-v1-to-v2).

use hot::val::Val;
use tracing::info;

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
        }
        "migrate" => {
            info!("Running database migrations...");
            match crate::run_migrations_with_bootstrap(conf, providers).await {
                Ok(_) => println!("Migrations completed successfully"),
                Err(e) => {
                    crate::report_migration_failure("Migration failed", &e);
                    return Err("Migration failed".to_string());
                }
            }
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
                         (or schema). For Hot Cloud production environments, the v1\u{2192}v2 \
                         backfill is owned by the private cloud repository. \
                         See https://hot.dev/docs/migrations#upgrading-from-hot-1x-to-hot-2"
                            .to_string(),
                    );
                }
            }
        }
        _ => {
            return Err(format!(
                "Unknown database command: {}. Available commands: status, migrate, port-v1-to-v2",
                db_cmd
            ));
        }
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

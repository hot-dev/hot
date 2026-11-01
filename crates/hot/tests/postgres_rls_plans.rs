//! Plans of Hot's hot-path statements under RLS, as the restricted runtime
//! login sees them. Run against a dedicated PostgreSQL database with
//! HOT_TEST_POSTGRES_URI set to a privileged fixture connection. This is its
//! own test binary because granting on the schema races the RLS isolation
//! test's grants ("tuple concurrently updated") when both run at once.

use hot::db::{self, DatabaseScope, ScopedPgPool};
use sqlx::{Executor, postgres::PgPoolOptions};
use std::time::Duration;
use uuid::Uuid;

struct TestRoleCleanup {
    options: sqlx::postgres::PgConnectOptions,
    role: String,
}
impl Drop for TestRoleCleanup {
    fn drop(&mut self) {
        let options = self.options.clone();
        let role = self.role.clone();
        let _ = std::thread::spawn(move || {
            if let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                runtime.block_on(async {
                    let cleanup = async {
                        if let Ok(pool) = PgPoolOptions::new()
                            .max_connections(1)
                            .connect_with(options)
                            .await
                        {
                            let _ = pool
                                .execute(sqlx::AssertSqlSafe(format!("DROP OWNED BY {role}")))
                                .await;
                            let _ = pool
                                .execute(sqlx::AssertSqlSafe(format!("DROP ROLE IF EXISTS {role}")))
                                .await;
                            pool.close().await;
                        }
                    };
                    let _ = tokio::time::timeout(Duration::from_secs(10), cleanup).await;
                });
            }
        })
        .join();
    }
}

/// Root-organization joins must stay probes of the org key under RLS.
/// PostgreSQL won't push a COALESCE root key through the policies' security
/// barrier, so each admission check and scheduler poll that joins on one
/// reads the whole org table.
#[tokio::test]
async fn root_org_joins_probe_the_org_key_under_rls() -> Result<(), Box<dyn std::error::Error>> {
    let Ok(admin_uri) = std::env::var("HOT_TEST_POSTGRES_URI") else {
        eprintln!("skipping PostgreSQL RLS test: HOT_TEST_POSTGRES_URI not set");
        return Ok(());
    };
    db::run_migrations(&hot::val!({"uri": admin_uri.clone(), "rls": {"enforce": false}})).await?;
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect(&admin_uri)
        .await?;
    let role = format!("hot_rls_plan_{}", Uuid::new_v4().simple());
    let password = Uuid::new_v4().simple().to_string();
    admin.execute(sqlx::AssertSqlSafe(format!("CREATE ROLE {role} LOGIN PASSWORD '{password}' NOSUPERUSER NOBYPASSRLS NOINHERIT NOCREATEDB NOCREATEROLE"))).await?;
    let _role_cleanup = TestRoleCleanup {
        options: admin.connect_options().as_ref().clone(),
        role: role.clone(),
    };
    for grant in [
        format!("GRANT USAGE ON SCHEMA hot TO {role}"),
        format!("GRANT SELECT ON ALL TABLES IN SCHEMA hot TO {role}"),
    ] {
        admin.execute(sqlx::AssertSqlSafe(grant)).await?;
    }
    let mut runtime_url = url::Url::parse(&admin_uri)?;
    runtime_url
        .set_username(&role)
        .map_err(|_| "invalid fixture URL")?;
    runtime_url
        .set_password(Some(&password))
        .map_err(|_| "invalid fixture URL")?;
    // With sequential scans priced out, the planner reads the whole table
    // through an index instead when the join can't bound the scan.
    let raw = PgPoolOptions::new()
        .max_connections(1)
        .after_connect(|conn, _| {
            Box::pin(async move {
                conn.execute("SET search_path = hot, public; SET enable_seqscan = off")
                    .await?;
                Ok(())
            })
        })
        .connect(runtime_url.as_str())
        .await?;
    let scheduler = ScopedPgPool::new(raw.clone()).with_scope(DatabaseScope::Maintenance {
        operation: db::MaintenanceOperation::Scheduler,
        org_id: None,
    });
    let plan = |root: &'static str| {
        let scheduler = scheduler.clone();
        async move {
            let lines: Vec<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                "EXPLAIN SELECT e.active AND o.active AND r.active FROM env e \
                 JOIN org o ON o.org_id = e.org_id JOIN org r ON r.org_id = {root} \
                 WHERE e.env_id = $1"
            )))
            .bind(Uuid::new_v4())
            .fetch_all(&scheduler)
            .await?;
            Ok::<_, sqlx::Error>(lines.join("\n"))
        }
    };
    // Scans of org that no index condition bounds read every organization.
    let unbounded_org_scans = |plan: &str| {
        let lines: Vec<&str> = plan.lines().collect();
        (0..lines.len())
            .filter(|&i| {
                lines[i].contains(" on org ")
                    && !lines[i + 1..]
                        .iter()
                        .take_while(|line| !line.contains("->"))
                        .any(|line| line.contains("Index Cond"))
            })
            .count()
    };
    let case =
        plan("CASE WHEN o.parent_org_id IS NULL THEN o.org_id ELSE o.parent_org_id END").await?;
    assert_eq!(unbounded_org_scans(&case), 0, "{case}");
    let coalesce = plan("COALESCE(o.parent_org_id, o.org_id)").await?;
    assert!(
        unbounded_org_scans(&coalesce) > 0,
        "COALESCE is bounded under RLS now; this test no longer guards anything:\n{coalesce}"
    );
    raw.close().await;
    admin.close().await;
    Ok(())
}

/// Keep the leaky root-organization key out of Hot's SQL. See
/// `root_org_joins_probe_the_org_key_under_rls`.
#[test]
fn sql_never_joins_on_a_coalesce_root_org_key() {
    let pattern = regex::Regex::new(r"=\s*COALESCE\(\s*\w+\.parent_org_id\s*,").unwrap();
    let crates = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let mut found = Vec::new();
    let mut pending = vec![crates];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                if path.file_name().is_some_and(|name| name != "target") {
                    pending.push(path);
                }
            } else if path.extension().is_some_and(|ext| ext == "rs")
                && !path.ends_with("tests/postgres_rls_plans.rs")
            {
                let source = std::fs::read_to_string(&path).unwrap();
                for (number, line) in source.lines().enumerate() {
                    if pattern.is_match(line) {
                        found.push(format!("{}:{}", path.display(), number + 1));
                    }
                }
            }
        }
    }
    assert!(
        found.is_empty(),
        "join on CASE WHEN x.parent_org_id IS NULL THEN x.org_id ELSE x.parent_org_id END instead: {found:?}"
    );
}

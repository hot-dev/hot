//! Upgrade populated, on-disk databases using the real SQLx migration ledger.
//! Unlike lightweight repository fixtures, these keep foreign keys enabled.

use hot::db;
use sqlx::{SqlitePool, migrate::Migrator, sqlite::SqliteConnectOptions};
use std::path::Path;
use uuid::Uuid;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

async fn migrations_through(version: i64) -> TestResult<Migrator> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../resources/db/sqlite/migrations");
    let mut migrator = Migrator::new(path).await?;
    migrator
        .migrations
        .to_mut()
        .retain(|migration| migration.version <= version);
    Ok(migrator)
}

async fn existing_database(path: &Path, version: i64) -> TestResult<SqlitePool> {
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(
            SqliteConnectOptions::new()
                .filename(path)
                .create_if_missing(true)
                .foreign_keys(true),
        )
        .await?;
    migrations_through(version).await?.run(&pool).await?;
    Ok(pool)
}

async fn ledger(pool: &SqlitePool) -> TestResult<Vec<(i64, Vec<u8>)>> {
    Ok(
        sqlx::query_as("SELECT version, checksum FROM _sqlx_migrations ORDER BY version")
            .fetch_all(pool)
            .await?,
    )
}

struct ExistingRows {
    user: Uuid,
    org: Uuid,
    other_org: Uuid,
    env: Uuid,
    sibling_env: Uuid,
    other_env: Uuid,
    run: Uuid,
    call: Uuid,
    alert: Uuid,
    subscription: Uuid,
    destination: Uuid,
    other_destination: Uuid,
    delivery: Uuid,
    key: Uuid,
    issuer: Uuid,
}

impl ExistingRows {
    async fn seed(pool: &SqlitePool) -> TestResult<Self> {
        let rows = Self {
            user: Uuid::new_v4(),
            org: Uuid::new_v4(),
            other_org: Uuid::new_v4(),
            env: Uuid::new_v4(),
            sibling_env: Uuid::new_v4(),
            other_env: Uuid::new_v4(),
            run: Uuid::new_v4(),
            call: Uuid::new_v4(),
            alert: Uuid::new_v4(),
            subscription: Uuid::new_v4(),
            destination: Uuid::new_v4(),
            other_destination: Uuid::new_v4(),
            delivery: Uuid::new_v4(),
            key: Uuid::new_v4(),
            issuer: Uuid::new_v4(),
        };
        sqlx::query("INSERT INTO user(user_id,email,created_by_user_id) VALUES(?,?,?)")
            .bind(rows.user)
            .bind("existing@example.test")
            .bind(rows.user)
            .execute(pool)
            .await?;
        for (org, slug) in [(rows.org, "existing"), (rows.other_org, "other")] {
            sqlx::query("INSERT INTO org(org_id,name,slug,created_by_user_id) VALUES(?,?,?,?)")
                .bind(org)
                .bind(slug)
                .bind(slug)
                .bind(rows.user)
                .execute(pool)
                .await?;
        }
        for (env, org, name) in [
            (rows.env, rows.org, "production"),
            (rows.sibling_env, rows.org, "staging"),
            (rows.other_env, rows.other_org, "production"),
        ] {
            sqlx::query("INSERT INTO env(env_id,org_id,name,created_by_user_id) VALUES(?,?,?,?)")
                .bind(env)
                .bind(org)
                .bind(name)
                .bind(rows.user)
                .execute(pool)
                .await?;
        }
        let project = Uuid::new_v4();
        let build = Uuid::new_v4();
        let stream = Uuid::new_v4();
        sqlx::query("INSERT INTO project(project_id,env_id,name,created_by_user_id) VALUES(?,?,'retained',?)")
            .bind(project).bind(rows.env).bind(rows.user).execute(pool).await?;
        sqlx::query("INSERT INTO build(build_id,project_id,hash,size,created_by_user_id) VALUES(?,?,'retained-bundle',123,?)")
            .bind(build).bind(project).bind(rows.user).execute(pool).await?;
        sqlx::query("INSERT INTO stream(stream_id,env_id) VALUES(?,?)")
            .bind(stream)
            .bind(rows.env)
            .execute(pool)
            .await?;
        sqlx::query("INSERT INTO run(run_id,env_id,stream_id,build_id,run_type_id,start_time,result) VALUES(?,?,?,?,1,'2026-09-01 12:00:00','\"retained result\"')")
            .bind(rows.run).bind(rows.env).bind(stream).bind(build).execute(pool).await?;
        let has_env: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('call') WHERE name='env_id')",
        )
        .fetch_one(pool)
        .await?;
        let mut insert = sqlx::query(if has_env { "INSERT INTO call(call_id,run_id,function_name,static_scope,runtime_path,call_depth,args,error,start_time,size,env_id) VALUES(?,?,'::retained/work','root','root',0,'[\"é\"]','☃','2026-09-01 12:00:00',999,?)" } else { "INSERT INTO call(call_id,run_id,function_name,static_scope,runtime_path,call_depth,args,error,start_time,size) VALUES(?,?,'::retained/work','root','root',0,'[\"é\"]','☃','2026-09-01 12:00:00',999)" })
            .bind(rows.call).bind(rows.run);
        if has_env {
            insert = insert.bind(rows.env);
        }
        insert.execute(pool).await?;
        sqlx::query("INSERT INTO task(task_id,env_id,stream_id,build_id,task_type,function_name,task_status_id,timeout_ms) VALUES(?,?,?,?,'container','::retained/task',2,1000)")
            .bind(rows.run).bind(rows.env).bind(stream).bind(build).execute(pool).await?;
        sqlx::query("INSERT INTO alert(alert_id,org_id,env_id,channel,data) VALUES(?,?,?,'run.failed','{\"retained\":true}')")
            .bind(rows.alert).bind(rows.org).bind(rows.env).execute(pool).await?;
        sqlx::query("INSERT INTO alert_subscription(alert_subscription_id,org_id,env_id,created_by_user_id) VALUES(?,?,?,?)")
            .bind(rows.subscription).bind(rows.org).bind(rows.env).bind(rows.user).execute(pool).await?;
        for (destination, org) in [
            (rows.destination, rows.org),
            (rows.other_destination, rows.other_org),
        ] {
            sqlx::query("INSERT INTO alert_destination(alert_destination_id,org_id,name,destination_type_id,config,created_by_user_id) VALUES(?,?,'existing-mail',1,'{}',?)")
                .bind(destination).bind(org).bind(rows.user).execute(pool).await?;
        }
        sqlx::query("INSERT INTO alert_delivery(alert_delivery_id,alert_id,subscription_id,destination_id,attempts,last_error) VALUES(?,?,?,?,2,'retained retry')")
            .bind(rows.delivery).bind(rows.alert).bind(rows.subscription).bind(rows.destination).execute(pool).await?;
        sqlx::query("INSERT INTO api_key(api_key_id,env_id,description,key_data,created_by_user_id,permissions) VALUES(?,?,'retained credential','{}',?,'{\"file:allowed/*\":[\"read\"]}')")
            .bind(rows.key).bind(rows.env).bind(rows.user).execute(pool).await?;
        Ok(rows)
    }

    /// A 3.0.0 management credential and the runtime key it issued, before 016.
    async fn add_management_issuer(&self, pool: &SqlitePool) -> TestResult {
        sqlx::query("INSERT INTO management_key(management_key_id,root_org_id,created_by_user_id,name,secret_hash,capabilities,expires_at) VALUES(?,?,?,'existing issuer',x'010203','[\"keys:issue\"]','2099-01-01T00:00:00Z')")
            .bind(self.issuer).bind(self.org).bind(self.user).execute(pool).await?;
        sqlx::query(
            "INSERT INTO api_key_management_issuer(api_key_id,management_key_id) VALUES(?,?)",
        )
        .bind(self.key)
        .bind(self.issuer)
        .execute(pool)
        .await?;
        Ok(())
    }

    /// An org key and the environment key it minted, after 016.
    async fn add_org_key_issuer(&self, pool: &SqlitePool) -> TestResult {
        sqlx::query("INSERT INTO org_api_key(org_api_key_id,org_id,description,key_data,permissions,created_by_user_id) VALUES(?,?,'existing issuer','{}','{\"org:*\":[\"read\"]}',?)")
            .bind(self.issuer).bind(self.org).bind(self.user).execute(pool).await?;
        sqlx::query("INSERT INTO api_key_issuer(api_key_id,issuer_org_api_key_id) VALUES(?,?)")
            .bind(self.key)
            .bind(self.issuer)
            .execute(pool)
            .await?;
        Ok(())
    }

    /// `retired` says whether a management credential issued the key before 016.
    async fn assert_preserved_and_protected(&self, pool: &SqlitePool, retired: bool) -> TestResult {
        assert_eq!(
            sqlx::query_scalar::<_, Uuid>("SELECT env_id FROM call WHERE call_id=?")
                .bind(self.call)
                .fetch_one(pool)
                .await?,
            self.env
        );
        let bytes: i64 = sqlx::query_scalar(
            "SELECT COALESCE(SUM(call_bytes),0) FROM usage_storage WHERE env_id=?",
        )
        .bind(self.env)
        .fetch_one(pool)
        .await?;
        assert_eq!(
            bytes,
            sqlx::query_scalar::<_, i64>(
                "SELECT COALESCE(SUM(MAX(size,0)),0) FROM call WHERE env_id=?"
            )
            .bind(self.env)
            .fetch_one(pool)
            .await?
        );
        let task_usage: (i64,i64,i64,Option<String>,i64,i64) = sqlx::query_as("SELECT active_containers,reserved_compute_units,reserved_duration_ms,period_start,compute_units,duration_ms FROM usage_task WHERE env_id=?").bind(self.env).fetch_one(pool).await?;
        assert_eq!(task_usage, (1, 32, 1000, None, 0, 0));
        let delivery: (Uuid, Uuid, Uuid, i64, String) = sqlx::query_as("SELECT alert_id,subscription_id,destination_id,attempts,last_error FROM alert_delivery WHERE alert_delivery_id=?")
            .bind(self.delivery).fetch_one(pool).await?;
        assert_eq!(
            delivery,
            (
                self.alert,
                self.subscription,
                self.destination,
                2,
                "retained retry".to_string()
            )
        );
        // 016 retires management credentials. A key one issued is kept but
        // deactivated, never turned into an independent key.
        let key: (Uuid, String, String, bool) = sqlx::query_as(
            "SELECT env_id,key_data,permissions,active FROM api_key WHERE api_key_id=?",
        )
        .bind(self.key)
        .fetch_one(pool)
        .await?;
        assert_eq!(
            key,
            (
                self.env,
                "{}".to_string(),
                "{\"file:allowed/*\":[\"read\"]}".to_string(),
                !retired
            )
        );
        let retired_tables: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM sqlite_master WHERE name IN ('management_key','api_key_management_issuer','management_key_scope_immutable','api_key_management_issuer_no_delete')",
        )
        .fetch_one(pool)
        .await?;
        assert_eq!(retired_tables, 0);
        let result: String = sqlx::query_scalar("SELECT result FROM run WHERE run_id=?")
            .bind(self.run)
            .fetch_one(pool)
            .await?;
        assert_eq!(result, "\"retained result\"");
        sqlx::query("UPDATE alert_delivery SET attempts=3 WHERE alert_delivery_id=?")
            .bind(self.delivery)
            .execute(pool)
            .await?;
        assert!(
            sqlx::query("UPDATE alert_delivery SET destination_id=? WHERE alert_delivery_id=?")
                .bind(self.other_destination)
                .bind(self.delivery)
                .execute(pool)
                .await
                .is_err()
        );
        // Org-key lineage cannot be detached, reassigned or deleted around.
        assert!(
            sqlx::query("DELETE FROM api_key WHERE api_key_id=?")
                .bind(self.key)
                .execute(pool)
                .await
                .is_err()
        );
        assert!(
            sqlx::query("DELETE FROM api_key_issuer WHERE api_key_id=?")
                .bind(self.key)
                .execute(pool)
                .await
                .is_err()
        );
        assert!(
            sqlx::query("UPDATE org_api_key SET org_id=? WHERE org_api_key_id=?")
                .bind(self.other_org)
                .bind(self.issuer)
                .execute(pool)
                .await
                .is_err()
        );
        sqlx::query("UPDATE api_key SET active=0 WHERE api_key_id=?")
            .bind(self.key)
            .execute(pool)
            .await?;
        let foreign_key_errors = sqlx::query("PRAGMA foreign_key_check")
            .fetch_all(pool)
            .await?;
        assert!(foreign_key_errors.is_empty());
        Ok(())
    }
}

#[tokio::test]
async fn sqlite_upgrades_preserve_populated_data_and_retire_management_credentials() -> TestResult {
    for baseline in [9, 13] {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("existing.db");
        let pool = existing_database(&path, baseline).await?;
        let rows = ExistingRows::seed(&pool).await?;
        if baseline < 13 {
            migrations_through(13).await?.run(&pool).await?;
            let trace_size: i64 = sqlx::query_scalar("SELECT size FROM call WHERE call_id=?")
                .bind(rows.call)
                .fetch_one(&pool)
                .await?;
            assert_eq!(trace_size, ("[\"é\"]".len() + "☃".len() + 50) as i64);
        }
        rows.add_management_issuer(&pool).await?;
        let previous_ledger = ledger(&pool).await?;
        pool.close().await;
        let conf = hot::val!({"uri": format!("sqlite:{}", path.display())});
        db::run_migrations(&conf).await?;
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(
                SqliteConnectOptions::new()
                    .filename(&path)
                    .foreign_keys(true),
            )
            .await?;
        let upgraded = ledger(&pool).await?;
        assert_eq!(
            &upgraded[..previous_ledger.len()],
            previous_ledger.as_slice()
        );
        assert_eq!(
            upgraded.len(),
            migrations_through(i64::MAX).await?.migrations.len()
        );
        // Both baselines precede 014, which backfills admissions from runs.
        let admission: (Uuid, Uuid, Uuid, String) = sqlx::query_as(
            "SELECT root_org_id,org_id,env_id,admitted_at FROM run_admission WHERE run_id=?",
        )
        .bind(rows.run)
        .fetch_one(&pool)
        .await?;
        assert_eq!(
            admission,
            (
                rows.org,
                rows.org,
                rows.env,
                "2026-09-01 12:00:00".to_string()
            )
        );
        rows.add_org_key_issuer(&pool).await?;
        rows.assert_preserved_and_protected(&pool, true).await?;
        pool.close().await;
        // A real deployment retry must neither replay DDL nor alter checksums.
        db::run_migrations(&conf).await?;
        let pool = sqlx::SqlitePool::connect(&format!("sqlite:{}", path.display())).await?;
        assert_eq!(ledger(&pool).await?, upgraded);
        pool.close().await;
    }
    Ok(())
}

#[tokio::test]
async fn sqlite_013_rejects_legacy_mismatches_atomically_and_allows_reviewed_retry() -> TestResult {
    for mismatch in ["destination_org", "subscription_org", "subscription_env"] {
        let directory = tempfile::tempdir()?;
        let pool = existing_database(&directory.path().join("existing.db"), 12).await?;
        let rows = ExistingRows::seed(&pool).await?;
        match mismatch {
            "destination_org" => {
                sqlx::query("UPDATE alert_delivery SET destination_id=? WHERE alert_delivery_id=?")
                    .bind(rows.other_destination)
                    .bind(rows.delivery)
                    .execute(&pool)
                    .await?;
            }
            "subscription_org" => {
                sqlx::query(
                    "UPDATE alert_subscription SET org_id=?,env_id=? WHERE alert_subscription_id=?",
                )
                .bind(rows.other_org)
                .bind(rows.other_env)
                .bind(rows.subscription)
                .execute(&pool)
                .await?;
            }
            _ => {
                sqlx::query("UPDATE alert_subscription SET env_id=? WHERE alert_subscription_id=?")
                    .bind(rows.sibling_env)
                    .bind(rows.subscription)
                    .execute(&pool)
                    .await?;
            }
        }
        let before = ledger(&pool).await?;
        let error = migrations_through(i64::MAX)
            .await?
            .run(&pool)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("13"), "{mismatch}: {error}");
        assert_eq!(ledger(&pool).await?, before);
        let installed: i64 = sqlx::query_scalar("SELECT count(*) FROM sqlite_master WHERE name IN ('management_key','call_environment_insert','alert_delivery_scope_insert','alert_delivery_scope_update','management_key_scope_immutable','delegated_api_key_no_delete','api_key_management_issuer_no_delete')")
            .fetch_one(&pool).await?;
        assert_eq!(
            installed, 0,
            "failed migration must not partially install guards"
        );
        let temporary: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM sqlite_temp_master WHERE name='scope_integrity_preflight'",
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(
            temporary, 0,
            "rollback must remove the temporary preflight table"
        );
        let current_destination: Uuid = sqlx::query_scalar(
            "SELECT destination_id FROM alert_delivery WHERE alert_delivery_id=?",
        )
        .bind(rows.delivery)
        .fetch_one(&pool)
        .await?;
        assert_eq!(
            current_destination,
            if mismatch == "destination_org" {
                rows.other_destination
            } else {
                rows.destination
            }
        );
        // This fixture knows the intended target from its retained ownership
        // evidence. Production repairs require that same explicit investigation.
        sqlx::query("UPDATE alert_delivery SET destination_id=? WHERE alert_delivery_id=?")
            .bind(rows.destination)
            .bind(rows.delivery)
            .execute(&pool)
            .await?;
        sqlx::query(
            "UPDATE alert_subscription SET org_id=?,env_id=? WHERE alert_subscription_id=?",
        )
        .bind(rows.org)
        .bind(rows.env)
        .bind(rows.subscription)
        .execute(&pool)
        .await?;
        migrations_through(i64::MAX).await?.run(&pool).await?;
        rows.add_org_key_issuer(&pool).await?;
        rows.assert_preserved_and_protected(&pool, false).await?;
        pool.close().await;
    }
    Ok(())
}

async fn runtime_reference(
    pool: &SqlitePool,
    env: Uuid,
    user: Uuid,
) -> TestResult<(Uuid, Uuid, Uuid, Uuid)> {
    let project = Uuid::new_v4();
    let build = Uuid::new_v4();
    let stream = Uuid::new_v4();
    let run = Uuid::new_v4();
    let event = Uuid::new_v4();
    sqlx::query("INSERT INTO project(project_id,env_id,name,created_by_user_id) VALUES(?,?,'runtime-reference',?)")
        .bind(project).bind(env).bind(user).execute(pool).await?;
    sqlx::query("INSERT INTO build(build_id,project_id,hash,size,created_by_user_id) VALUES(?,?,'runtime-reference',1,?)")
        .bind(build).bind(project).bind(user).execute(pool).await?;
    sqlx::query("INSERT INTO stream(stream_id,env_id) VALUES(?,?)")
        .bind(stream)
        .bind(env)
        .execute(pool)
        .await?;
    sqlx::query("INSERT INTO run(run_id,env_id,stream_id,build_id,run_type_id) VALUES(?,?,?,?,1)")
        .bind(run)
        .bind(env)
        .bind(stream)
        .bind(build)
        .execute(pool)
        .await?;
    sqlx::query("INSERT INTO event(event_id,env_id,stream_id,event_type,event_data,event_time,created_by_user_id) VALUES(?,?,?,'retained.event','{}','2026-09-01 12:00:00',?)")
        .bind(event).bind(env).bind(stream).bind(user).execute(pool).await?;
    Ok((run, stream, build, event))
}

#[tokio::test]
async fn sqlite_013_rejects_retained_cross_environment_origins_and_events() -> TestResult {
    for mismatch in [
        "run_origin_environment",
        "task_origin_environment",
        "run_event_environment",
    ] {
        let directory = tempfile::tempdir()?;
        let pool = existing_database(&directory.path().join("existing.db"), 9).await?;
        let rows = ExistingRows::seed(&pool).await?;
        let (local_run, local_stream, local_build, local_event) =
            runtime_reference(&pool, rows.env, rows.user).await?;
        let (foreign_run, _, _, foreign_event) =
            runtime_reference(&pool, rows.other_env, rows.user).await?;
        let task = Uuid::new_v4();
        let offending_id = match mismatch {
            "run_origin_environment" => {
                sqlx::query("UPDATE run SET origin_run_id=? WHERE run_id=?")
                    .bind(foreign_run)
                    .bind(rows.run)
                    .execute(&pool)
                    .await?;
                foreign_run
            }
            "task_origin_environment" => {
                sqlx::query("INSERT INTO task(task_id,env_id,stream_id,build_id,origin_run_id,function_name) VALUES(?,?,?,?,?,'::retained/task')")
                    .bind(task).bind(rows.env).bind(local_stream).bind(local_build).bind(foreign_run).execute(&pool).await?;
                foreign_run
            }
            _ => {
                sqlx::query("UPDATE run SET event_id=? WHERE run_id=?")
                    .bind(foreign_event)
                    .bind(rows.run)
                    .execute(&pool)
                    .await?;
                foreign_event
            }
        };
        let error = migrations_through(i64::MAX)
            .await?
            .run(&pool)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("13"), "{error}");
        assert!(error.to_string().contains(mismatch), "{error}");
        let before_retry = ledger(&pool).await?;
        assert_eq!(before_retry.last().map(|row| row.0), Some(12));
        assert_eq!(
            before_retry.len(),
            12,
            "earlier migrations remain committed"
        );
        let temporary: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM sqlite_temp_master WHERE name='retained_runtime_scope_preflight'",
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(temporary, 0);
        let installed: i64 = sqlx::query_scalar("SELECT count(*) FROM sqlite_master WHERE name IN ('alert_delivery_scope_insert','delegated_api_key_no_delete')")
            .fetch_one(&pool).await?;
        assert_eq!(installed, 0, "013 must roll back as a whole");
        let (retained_id, repaired_id) = match mismatch {
            "run_origin_environment" => {
                let retained =
                    sqlx::query_scalar::<_, Uuid>("SELECT origin_run_id FROM run WHERE run_id=?")
                        .bind(rows.run)
                        .fetch_one(&pool)
                        .await?;
                sqlx::query("UPDATE run SET origin_run_id=? WHERE run_id=?")
                    .bind(local_run)
                    .bind(rows.run)
                    .execute(&pool)
                    .await?;
                (retained, local_run)
            }
            "task_origin_environment" => {
                let retained =
                    sqlx::query_scalar::<_, Uuid>("SELECT origin_run_id FROM task WHERE task_id=?")
                        .bind(task)
                        .fetch_one(&pool)
                        .await?;
                sqlx::query("UPDATE task SET origin_run_id=? WHERE task_id=?")
                    .bind(local_run)
                    .bind(task)
                    .execute(&pool)
                    .await?;
                (retained, local_run)
            }
            _ => {
                let retained =
                    sqlx::query_scalar::<_, Uuid>("SELECT event_id FROM run WHERE run_id=?")
                        .bind(rows.run)
                        .fetch_one(&pool)
                        .await?;
                sqlx::query("UPDATE run SET event_id=? WHERE run_id=?")
                    .bind(local_event)
                    .bind(rows.run)
                    .execute(&pool)
                    .await?;
                (retained, local_event)
            }
        };
        assert_eq!(
            retained_id, offending_id,
            "failed preflight must not rewrite provenance"
        );
        assert_ne!(retained_id, repaired_id);
        migrations_through(i64::MAX).await?.run(&pool).await?;
        let after_retry = ledger(&pool).await?;
        assert_eq!(&after_retry[..before_retry.len()], before_retry.as_slice());
        assert_eq!(
            after_retry.len(),
            migrations_through(i64::MAX).await?.migrations.len()
        );
        assert!(
            sqlx::query("PRAGMA foreign_key_check")
                .fetch_all(&pool)
                .await?
                .is_empty()
        );
        let env: Uuid = sqlx::query_scalar("SELECT env_id FROM run WHERE run_id=?")
            .bind(rows.run)
            .fetch_one(&pool)
            .await?;
        assert_eq!(env, rows.env);
        pool.close().await;
    }
    Ok(())
}

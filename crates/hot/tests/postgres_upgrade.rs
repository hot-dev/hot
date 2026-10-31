//! Rehearse upgrades against populated databases, never the caller's database.
//! HOT_TEST_POSTGRES_URI must name a disposable privileged fixture. Each case
//! creates its own database plus NOSUPERUSER/NOBYPASSRLS owner/runtime logins.
//! Requires psql to exercise the actual operator provisioning script.

use hot::db::{self, DatabasePool, DatabaseScope, MaintenanceOperation};
use sqlx::{Executor, PgPool, migrate::Migrator, postgres::PgPoolOptions};
use std::{
    path::{Path, PathBuf},
    process::Output,
};
use uuid::Uuid;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

fn repository() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

async fn migrations_through(version: i64) -> TestResult<Migrator> {
    let mut migrator = Migrator::new(repository().join("resources/db/postgres/migrations")).await?;
    migrator
        .migrations
        .to_mut()
        .retain(|migration| migration.version <= version);
    Ok(migrator)
}

async fn connect(uri: &str) -> TestResult<PgPool> {
    Ok(PgPoolOptions::new()
        .max_connections(2)
        .after_connect(|connection, _| {
            Box::pin(async move {
                connection.execute("SET search_path = hot, public").await?;
                Ok(())
            })
        })
        .connect(uri)
        .await?)
}

async fn ledger(pool: &PgPool) -> TestResult<Vec<(i64, Vec<u8>)>> {
    Ok(
        sqlx::query_as("SELECT version,checksum FROM _sqlx_migrations ORDER BY version")
            .fetch_all(pool)
            .await?,
    )
}

async fn assert_latest_ledger(pool: &PgPool) -> TestResult {
    let expected = migrations_through(i64::MAX).await?;
    let applied = ledger(pool).await?;
    assert_eq!(
        applied.iter().map(|entry| entry.0).collect::<Vec<_>>(),
        expected
            .iter()
            .map(|entry| entry.version)
            .collect::<Vec<_>>(),
        "successful upgrade must apply every current migration"
    );
    Ok(())
}

struct UpgradeDatabase {
    cluster: PgPool,
    admin: PgPool,
    owner: PgPool,
    owner_uri: String,
    runtime_uri: String,
    database: String,
    owner_role: String,
    runtime_role: String,
    group_role: String,
    cleaned: bool,
}

// Cleanup must also run when a test assertion unwinds before explicit cleanup.
impl Drop for UpgradeDatabase {
    fn drop(&mut self) {
        if self.cleaned {
            return;
        }
        let options = self.cluster.connect_options().as_ref().clone();
        let database = self.database.clone();
        let roles = [
            self.runtime_role.clone(),
            self.group_role.clone(),
            self.owner_role.clone(),
        ];
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
                                .execute(sqlx::AssertSqlSafe(format!(
                                    "DROP DATABASE IF EXISTS {database} WITH (FORCE)"
                                )))
                                .await;
                            for role in roles {
                                let _ = pool
                                    .execute(sqlx::AssertSqlSafe(format!(
                                        "DROP ROLE IF EXISTS {role}"
                                    )))
                                    .await;
                            }
                            pool.close().await;
                        }
                    };
                    let _ = tokio::time::timeout(std::time::Duration::from_secs(10), cleanup).await;
                });
            }
        })
        .join();
    }
}

impl UpgradeDatabase {
    async fn new(admin_uri: &str, version: i64) -> TestResult<Self> {
        let suffix = Uuid::new_v4().simple().to_string();
        let database = format!("hot_upgrade_{suffix}");
        let owner_role = format!("hot_owner_{suffix}");
        let runtime_role = format!("hot_runtime_{suffix}");
        let group_role = format!("hot_group_{suffix}");
        let password = Uuid::new_v4().simple().to_string();
        let cluster = connect(admin_uri).await?;
        for role in [&owner_role, &runtime_role] {
            cluster.execute(sqlx::AssertSqlSafe(format!("CREATE ROLE {role} LOGIN PASSWORD '{password}' NOSUPERUSER NOBYPASSRLS NOINHERIT NOCREATEDB NOCREATEROLE NOREPLICATION"))).await?;
        }
        cluster
            .execute(sqlx::AssertSqlSafe(format!(
                "CREATE ROLE {group_role} NOLOGIN NOSUPERUSER NOBYPASSRLS"
            )))
            .await?;
        cluster
            .execute(sqlx::AssertSqlSafe(format!(
                "CREATE DATABASE {database} OWNER {owner_role}"
            )))
            .await?;
        let mut uri = url::Url::parse(admin_uri)?;
        uri.set_path(&format!("/{database}"));
        let admin = connect(uri.as_str()).await?;
        admin
            .execute(sqlx::AssertSqlSafe(format!(
                "CREATE SCHEMA hot AUTHORIZATION {owner_role}"
            )))
            .await?;
        // Untrusted extensions are installed administratively once; the owner
        // running every application migration has neither SUPERUSER nor BYPASSRLS.
        admin
            .execute("CREATE EXTENSION vector WITH SCHEMA hot")
            .await?;
        admin
            .execute("CREATE EXTENSION pg_trgm WITH SCHEMA hot")
            .await?;
        uri.set_username(&owner_role)
            .map_err(|_| "invalid fixture username")?;
        uri.set_password(Some(&password))
            .map_err(|_| "invalid fixture password")?;
        let owner_uri = uri.to_string();
        // Apply the baseline on its own pool, as run_migrations does: migration
        // 001 turns row_security off for its session, which must not leak into
        // the scoped queries tests later run through `owner`.
        let migrator = connect(&owner_uri).await?;
        migrations_through(version).await?.run(&migrator).await?;
        migrator.close().await;
        let owner = connect(&owner_uri).await?;
        uri.set_username(&runtime_role)
            .map_err(|_| "invalid fixture username")?;
        let runtime_uri = uri.to_string();
        Ok(Self {
            cluster,
            admin,
            owner,
            owner_uri,
            runtime_uri,
            database,
            owner_role,
            runtime_role,
            group_role,
            cleaned: false,
        })
    }

    async fn upgrade(&self) -> Result<(), db::DatabaseError> {
        db::run_migrations(
            &hot::val!({"uri": self.runtime_uri.clone(), "migration-uri": self.owner_uri.clone()}),
        )
        .await
    }

    async fn provision(&self) -> TestResult<Output> {
        let mut uri = url::Url::parse(&self.owner_uri)?;
        let password = uri.password().unwrap().to_owned();
        uri.set_password(None)
            .map_err(|_| "invalid fixture password")?;
        Ok(tokio::process::Command::new("psql")
            .args([
                "-X",
                "-v",
                "ON_ERROR_STOP=1",
                "-v",
                &format!("runtime_role={}", self.runtime_role),
                "-v",
                "schema=hot",
                "-f",
            ])
            .arg(repository().join("resources/db/postgres/provision_runtime.sql"))
            .arg("--dbname")
            .arg(uri.as_str())
            .env("PGPASSWORD", password)
            .output()
            .await
            .map_err(|e| format!("upgrade role tests require psql: {e}"))?)
    }

    async fn provision_ok(&self) -> TestResult {
        let output = self.provision().await?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(())
    }

    async fn cleanup(mut self) -> TestResult {
        self.owner.close().await;
        self.admin.close().await;
        self.cluster
            .execute(sqlx::AssertSqlSafe(format!(
                "DROP DATABASE {} WITH (FORCE)",
                self.database
            )))
            .await?;
        for role in [&self.runtime_role, &self.group_role, &self.owner_role] {
            self.cluster
                .execute(sqlx::AssertSqlSafe(format!("DROP ROLE {role}")))
                .await?;
        }
        self.cluster.close().await;
        self.cleaned = true;
        Ok(())
    }
}

struct ExistingRows {
    user: Uuid,
    org: Uuid,
    other_org: Uuid,
    env: Uuid,
    sibling_env: Uuid,
    other_env: Uuid,
    project: Uuid,
    build: Uuid,
    webhook: Uuid,
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
    async fn seed(pool: &PgPool) -> TestResult<Self> {
        let rows = Self {
            user: Uuid::new_v4(),
            org: Uuid::new_v4(),
            other_org: Uuid::new_v4(),
            env: Uuid::new_v4(),
            sibling_env: Uuid::new_v4(),
            other_env: Uuid::new_v4(),
            project: Uuid::new_v4(),
            build: Uuid::new_v4(),
            webhook: Uuid::new_v4(),
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
        sqlx::query("INSERT INTO \"user\"(user_id,email,created_by_user_id) VALUES($1,$2,$3)")
            .bind(rows.user)
            .bind("existing@example.test")
            .bind(rows.user)
            .execute(pool)
            .await?;
        for (org, slug) in [(rows.org, "existing"), (rows.other_org, "other")] {
            sqlx::query("INSERT INTO org(org_id,name,slug,created_by_user_id) VALUES($1,$2,$3,$4)")
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
            sqlx::query(
                "INSERT INTO env(env_id,org_id,name,created_by_user_id) VALUES($1,$2,$3,$4)",
            )
            .bind(env)
            .bind(org)
            .bind(name)
            .bind(rows.user)
            .execute(pool)
            .await?;
        }
        let project = rows.project;
        let build = rows.build;
        let stream = Uuid::new_v4();
        sqlx::query("INSERT INTO project(project_id,env_id,name,created_by_user_id) VALUES($1,$2,'retained',$3)")
            .bind(project).bind(rows.env).bind(rows.user).execute(pool).await?;
        sqlx::query("INSERT INTO build(build_id,project_id,hash,size,created_by_user_id) VALUES($1,$2,'retained-bundle',123,$3)")
            .bind(build).bind(project).bind(rows.user).execute(pool).await?;
        sqlx::query("INSERT INTO webhook(webhook_id,build_id,service,path,method,ns,var,name,meta) VALUES($1,$2,'retained-service','/retained','POST','retained','handler','retained handler',$3)")
            .bind(rows.webhook).bind(build).bind(serde_json::json!({"auth":"required","retained":true})).execute(pool).await?;
        sqlx::query("INSERT INTO stream(stream_id,env_id) VALUES($1,$2)")
            .bind(stream)
            .bind(rows.env)
            .execute(pool)
            .await?;
        sqlx::query("INSERT INTO run(run_id,env_id,stream_id,build_id,run_type_id,start_time,result) VALUES($1,$2,$3,$4,1,'2026-09-01 12:00:00','\"retained result\"')")
            .bind(rows.run).bind(rows.env).bind(stream).bind(build).execute(pool).await?;
        let has_env: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM information_schema.columns WHERE table_schema='hot' AND table_name='call' AND column_name='env_id')").fetch_one(pool).await?;
        let mut insert = sqlx::query(if has_env { "INSERT INTO call(call_id,run_id,function_name,static_scope,runtime_path,call_depth,args,error,start_time,size,env_id) VALUES($1,$2,'::retained/work','root','root',0,'[\"é\"]','☃','2026-09-01 12:00:00',999,$3)" } else { "INSERT INTO call(call_id,run_id,function_name,static_scope,runtime_path,call_depth,args,error,start_time,size) VALUES($1,$2,'::retained/work','root','root',0,'[\"é\"]','☃','2026-09-01 12:00:00',999)" })
            .bind(rows.call).bind(rows.run);
        if has_env {
            insert = insert.bind(rows.env);
        }
        insert.execute(pool).await?;
        sqlx::query("INSERT INTO task(task_id,env_id,stream_id,build_id,task_type,function_name,task_status_id,timeout_ms) VALUES($1,$2,$3,$4,'container','::retained/task',2,1000)")
            .bind(rows.run).bind(rows.env).bind(stream).bind(build).execute(pool).await?;
        sqlx::query("INSERT INTO alert(alert_id,org_id,env_id,channel,data) VALUES($1,$2,$3,'run.failed','{\"retained\":true}')")
            .bind(rows.alert).bind(rows.org).bind(rows.env).execute(pool).await?;
        sqlx::query("INSERT INTO alert_subscription(alert_subscription_id,org_id,env_id,created_by_user_id) VALUES($1,$2,$3,$4)")
            .bind(rows.subscription).bind(rows.org).bind(rows.env).bind(rows.user).execute(pool).await?;
        for (destination, org) in [
            (rows.destination, rows.org),
            (rows.other_destination, rows.other_org),
        ] {
            sqlx::query("INSERT INTO alert_destination(alert_destination_id,org_id,name,destination_type_id,config,created_by_user_id) VALUES($1,$2,'existing-mail',1,'{}',$3)")
                .bind(destination).bind(org).bind(rows.user).execute(pool).await?;
        }
        sqlx::query("INSERT INTO alert_delivery(alert_delivery_id,alert_id,subscription_id,destination_id,attempts,last_error) VALUES($1,$2,$3,$4,2,'retained retry')")
            .bind(rows.delivery).bind(rows.alert).bind(rows.subscription).bind(rows.destination).execute(pool).await?;
        sqlx::query("INSERT INTO api_key(api_key_id,env_id,description,key_data,created_by_user_id,permissions) VALUES($1,$2,'retained credential','{}',$3,'{\"file:allowed/*\":[\"read\"]}')")
            .bind(rows.key).bind(rows.env).bind(rows.user).execute(pool).await?;
        // A deleted file, a live one, and an older deleted version that still
        // shares the live version's object.
        for (name, storage_path, active) in [
            ("gone.txt", "/files/gone", false),
            ("kept.txt", "/files/kept", true),
            ("kept.txt", "/files/kept", false),
        ] {
            sqlx::query("INSERT INTO file(file_id,path,size,storage_backend,storage_path,org_id,env_id,created_by_user_id,active) VALUES($1,$2,1,'local',$3,$4,$5,$6,$7)")
                .bind(Uuid::new_v4()).bind(name).bind(storage_path).bind(rows.org).bind(rows.env).bind(rows.user).bind(active)
                .execute(pool).await?;
        }
        Ok(rows)
    }

    /// A 3.0.0 management credential and the runtime key it issued, before 019.
    async fn add_management_issuer(&self, pool: &PgPool) -> TestResult {
        sqlx::query("INSERT INTO management_key(management_key_id,root_org_id,created_by_user_id,name,secret_hash,capabilities,expires_at) VALUES($1,$2,$3,'existing issuer',decode('010203','hex'),'[\"keys:issue\"]','2099-01-01T00:00:00Z')")
            .bind(self.issuer).bind(self.org).bind(self.user).execute(pool).await?;
        sqlx::query(
            "INSERT INTO api_key_management_issuer(api_key_id,management_key_id) VALUES($1,$2)",
        )
        .bind(self.key)
        .bind(self.issuer)
        .execute(pool)
        .await?;
        Ok(())
    }

    /// An org key and the environment key it minted, after 019.
    async fn add_org_key_issuer(&self, pool: &PgPool) -> TestResult {
        sqlx::query("INSERT INTO org_api_key(org_api_key_id,org_id,description,key_data,permissions,created_by_user_id) VALUES($1,$2,'existing issuer','{}','{\"org:*\":[\"read\"]}',$3)")
            .bind(self.issuer).bind(self.org).bind(self.user).execute(pool).await?;
        sqlx::query("INSERT INTO api_key_issuer(api_key_id,issuer_org_api_key_id) VALUES($1,$2)")
            .bind(self.key)
            .bind(self.issuer)
            .execute(pool)
            .await?;
        Ok(())
    }
}

async fn assert_runtime_contract(fixture: &UpgradeDatabase, rows: &ExistingRows) -> TestResult {
    let runtime = db::create_db_pool(&hot::val!({"uri": fixture.runtime_uri.clone()})).await?;
    let DatabasePool::Postgres(pool) = runtime else {
        unreachable!()
    };
    let env = pool.with_scope(DatabaseScope::Environment {
        org_id: rows.org,
        env_id: rows.env,
    });
    let other = pool.with_scope(DatabaseScope::Environment {
        org_id: rows.other_org,
        env_id: rows.other_env,
    });
    let organization = pool.with_scope(DatabaseScope::Organization { org_id: rows.org });
    assert_eq!(
        sqlx::query_scalar::<_, Uuid>("SELECT env_id FROM call WHERE call_id=$1")
            .bind(rows.call)
            .fetch_one(&env)
            .await?,
        rows.env
    );
    let bytes: i64 = sqlx::query_scalar(
        "SELECT COALESCE(SUM(call_bytes),0)::bigint FROM usage_storage WHERE env_id=$1",
    )
    .bind(rows.env)
    .fetch_one(&env)
    .await?;
    assert_eq!(
        bytes,
        sqlx::query_scalar::<_, i64>(
            "SELECT COALESCE(SUM(GREATEST(size,0)),0)::bigint FROM call WHERE env_id=$1"
        )
        .bind(rows.env)
        .fetch_one(&env)
        .await?
    );
    let task_usage: (i64,i64,i64,Option<chrono::DateTime<chrono::Utc>>,i64,i64) = sqlx::query_as("SELECT active_containers,reserved_compute_units,reserved_duration_ms,period_start,compute_units,duration_ms FROM usage_task WHERE env_id=$1").bind(rows.env).fetch_one(&env).await?;
    assert_eq!(task_usage, (1, 32, 1000, None, 0, 0));
    let raw = connect(&fixture.runtime_uri).await?;
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM run")
            .fetch_one(&raw)
            .await?,
        0
    );
    assert_eq!(
        sqlx::query_scalar::<_, Uuid>("SELECT run_id FROM run")
            .fetch_one(&env)
            .await?,
        rows.run
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM run")
            .fetch_one(&other)
            .await?,
        0
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM org_api_key")
            .fetch_one(&env)
            .await?,
        0
    );
    assert_eq!(
        sqlx::query_scalar::<_, Uuid>("SELECT org_api_key_id FROM org_api_key")
            .fetch_one(&organization)
            .await?,
        rows.issuer
    );
    assert_eq!(
        sqlx::query_scalar::<_, Uuid>("SELECT issuer_org_api_key_id FROM api_key_issuer")
            .fetch_one(&env)
            .await?,
        rows.issuer
    );
    let delivery: (Uuid, Uuid, Uuid, i32, String) = sqlx::query_as(
        "SELECT alert_id,subscription_id,destination_id,attempts,last_error FROM alert_delivery",
    )
    .fetch_one(&env)
    .await?;
    assert_eq!(
        delivery,
        (
            rows.alert,
            rows.subscription,
            rows.destination,
            2,
            "retained retry".into()
        )
    );
    let permissions: serde_json::Value = sqlx::query_scalar("SELECT permissions FROM api_key")
        .fetch_one(&env)
        .await?;
    assert_eq!(permissions, serde_json::json!({"file:allowed/*":["read"]}));
    assert_eq!(
        sqlx::query("UPDATE run SET result='\"changed\"' WHERE run_id=$1")
            .bind(rows.run)
            .execute(&other)
            .await?
            .rows_affected(),
        0
    );
    assert!(
        sqlx::query("DELETE FROM api_key WHERE api_key_id=$1")
            .bind(rows.key)
            .execute(&env)
            .await
            .is_err()
    );
    assert!(
        sqlx::query(
            "INSERT INTO alert_delivery(alert_id,subscription_id,destination_id) VALUES($1,$2,$3)"
        )
        .bind(rows.alert)
        .bind(rows.subscription)
        .bind(rows.other_destination)
        .execute(&env)
        .await
        .is_err()
    );
    assert!(
        sqlx::query("SELECT * FROM _sqlx_migrations")
            .fetch_all(&raw)
            .await
            .is_err()
    );
    assert!(
        sqlx::query("CREATE TABLE hot.forbidden(value int)")
            .execute(&raw)
            .await
            .is_err()
    );
    assert!(sqlx::query("TRUNCATE hot.run").execute(&raw).await.is_err());
    sqlx::query("UPDATE alert_delivery SET attempts=3 WHERE alert_delivery_id=$1")
        .bind(rows.delivery)
        .execute(&env)
        .await?;
    sqlx::query("UPDATE api_key SET active=false WHERE api_key_id=$1")
        .bind(rows.key)
        .execute(&env)
        .await?;

    // A retained endpoint keeps its identity and metadata across deployment of
    // the next build of the same project. The old build_id immutability guard
    // must be replaced by the upgrade without allowing project/scope changes.
    assert_eq!(
        sqlx::query_scalar::<_, Uuid>("SELECT build_id FROM webhook WHERE webhook_id=$1")
            .bind(rows.webhook)
            .fetch_one(&env)
            .await?,
        rows.build
    );
    let next_build = Uuid::new_v4();
    sqlx::query("INSERT INTO build(build_id,project_id,hash,size,created_by_user_id) VALUES($1,$2,'next-bundle',123,$3)")
        .bind(next_build).bind(rows.project).bind(rows.user).execute(&env).await?;
    assert_eq!(
        sqlx::query("UPDATE webhook SET build_id=$1 WHERE webhook_id=$2")
            .bind(next_build)
            .bind(rows.webhook)
            .execute(&env)
            .await?
            .rows_affected(),
        1
    );
    let retained: (Uuid, Uuid, String, String, serde_json::Value) = sqlx::query_as(
        "SELECT webhook_id,build_id,service,path,meta FROM webhook WHERE webhook_id=$1",
    )
    .bind(rows.webhook)
    .fetch_one(&env)
    .await?;
    assert_eq!(
        retained,
        (
            rows.webhook,
            next_build,
            "retained-service".into(),
            "/retained".into(),
            serde_json::json!({"auth":"required","retained":true}),
        )
    );
    for (target_scope, target_env) in [(&env, rows.env), (&other, rows.other_env)] {
        let target_project = Uuid::new_v4();
        let target_build = Uuid::new_v4();
        sqlx::query("INSERT INTO project(project_id,env_id,name,created_by_user_id) VALUES($1,$2,'forbidden-webhook-target',$3)")
            .bind(target_project).bind(target_env).bind(rows.user).execute(target_scope).await?;
        sqlx::query("INSERT INTO build(build_id,project_id,hash,size,created_by_user_id) VALUES($1,$2,'forbidden-bundle',123,$3)")
            .bind(target_build).bind(target_project).bind(rows.user).execute(target_scope).await?;
        assert!(
            sqlx::query("UPDATE webhook SET build_id=$1 WHERE webhook_id=$2")
                .bind(target_build)
                .bind(rows.webhook)
                .execute(&env)
                .await
                .is_err(),
            "a deployment cannot move a retained webhook to another project or environment"
        );
        assert_eq!(
            sqlx::query("UPDATE webhook SET build_id=$1 WHERE webhook_id=$2")
                .bind(target_build)
                .bind(rows.webhook)
                .execute(&other)
                .await?
                .rows_affected(),
            0,
            "another environment cannot mutate the retained endpoint"
        );
    }
    assert_eq!(
        sqlx::query_scalar::<_, Uuid>("SELECT build_id FROM webhook WHERE webhook_id=$1")
            .bind(rows.webhook)
            .fetch_one(&env)
            .await?,
        next_build,
        "rejected reassignments preserve the deployed build"
    );

    // Provisioning a child after upgrade cannot change the parent deployment's
    // owner. A child environment gets positive writes but no parent run access.
    let child = Uuid::new_v4();
    let child_env = Uuid::new_v4();
    let provision = pool.with_scope(DatabaseScope::Maintenance {
        operation: MaintenanceOperation::OrganizationProvisioning,
        org_id: Some(rows.org),
    });
    sqlx::query("INSERT INTO org(org_id,parent_org_id,name,slug,created_by_user_id) VALUES($1,$2,'child',$3,$4)")
        .bind(child).bind(rows.org).bind(child.to_string()).bind(rows.user).execute(&provision).await?;
    sqlx::query(
        "INSERT INTO env(env_id,org_id,name,created_by_user_id) VALUES($1,$2,'production',$3)",
    )
    .bind(child_env)
    .bind(child)
    .bind(rows.user)
    .execute(&provision)
    .await?;
    let child_scope = pool.with_scope(DatabaseScope::Environment {
        org_id: child,
        env_id: child_env,
    });
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM run")
            .fetch_one(&child_scope)
            .await?,
        0
    );
    sqlx::query("INSERT INTO store_map_entry(org_id,env_id,store_name,key,value) VALUES($1,$2,'retained','\"key\"','\"child\"')")
        .bind(child).bind(child_env).execute(&child_scope).await?;
    assert_eq!(
        sqlx::query_scalar::<_, Option<String>>(
            "SELECT key_text FROM store_map_entry WHERE env_id=$1"
        )
        .bind(child_env)
        .fetch_one(&fixture.admin)
        .await?,
        Some("key".to_string()),
        "the runtime's writes keep the key text for prefix reads"
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM store_map_entry")
            .fetch_one(&env)
            .await?,
        0
    );
    raw.close().await;
    pool.close().await;
    Ok(())
}

#[tokio::test]
async fn populated_upgrades_use_non_bypass_owner_and_reprovision_runtime_grants() -> TestResult {
    let Ok(admin_uri) = std::env::var("HOT_TEST_POSTGRES_URI") else {
        return Ok(());
    };
    for baseline in [9, 12, 13] {
        let fixture = UpgradeDatabase::new(&admin_uri, baseline).await?;
        let result: TestResult = async {
            let rows = ExistingRows::seed(&fixture.admin).await?;
            if baseline >= 13 { rows.add_management_issuer(&fixture.admin).await?; }
            for (env, org, key) in [
                (rows.sibling_env, rows.org, "\"case:1\""),
                (rows.sibling_env, rows.org, "7"),
                (rows.other_env, rows.other_org, "\"other\""),
            ] {
                sqlx::query("INSERT INTO store_map_entry(org_id,env_id,store_name,key,value) VALUES($1,$2,'prefix-backfill',$3::jsonb,'1')")
                    .bind(org).bind(env).bind(key).execute(&fixture.admin).await?;
            }
            let old_ledger = ledger(&fixture.owner).await?;
            let owner_privileged: bool = sqlx::query_scalar("SELECT rolsuper OR rolbypassrls OR rolcreaterole OR rolcreatedb FROM pg_roles WHERE rolname=current_user")
                .fetch_one(&fixture.owner).await?;
            assert!(!owner_privileged);
            fixture.upgrade().await?;
            let final_ledger = ledger(&fixture.owner).await?;
            assert!(final_ledger.starts_with(&old_ledger), "applied checksums must remain unchanged");
            assert_latest_ledger(&fixture.owner).await?;
            fixture.upgrade().await?;
            assert_eq!(ledger(&fixture.owner).await?, final_ledger);
            // 022 adds the key text prefix reads compare, and 023 fills it in
            // for existing string keys, though the non-bypass owner updates
            // one environment at a time.
            let key_texts: Vec<(Uuid, String, Option<String>)> = sqlx::query_as("SELECT env_id, key::text, key_text FROM store_map_entry WHERE store_name='prefix-backfill' ORDER BY seq")
                .fetch_all(&fixture.admin).await?;
            assert_eq!(key_texts, [
                (rows.sibling_env, "\"case:1\"".to_string(), Some("case:1".to_string())),
                (rows.sibling_env, "7".to_string(), None),
                (rows.other_env, "\"other\"".to_string(), Some("other".to_string())),
            ]);
            // 019 retires management credentials: the key one issued is kept
            // but deactivated, and the management tables are gone.
            let retired: (bool, bool, bool) = sqlx::query_as("SELECT active, to_regclass('hot.management_key') IS NULL, to_regclass('hot.api_key_management_issuer') IS NULL FROM api_key WHERE api_key_id=$1")
                .bind(rows.key).fetch_one(&fixture.admin).await?;
            assert_eq!(retired, (baseline < 13, true, true));
            rows.add_org_key_issuer(&fixture.admin).await?;
            // Every baseline precedes 014, which backfills admissions; 013
            // reconciles the sizes of traces written before it.
            let admission: (Uuid,Uuid,Uuid,bool) = sqlx::query_as("SELECT root_org_id,org_id,env_id,admitted_at='2026-09-01 12:00:00+00'::timestamptz FROM run_admission WHERE run_id=$1")
                .bind(rows.run).fetch_one(&fixture.admin).await?;
            assert_eq!(admission,(rows.org,rows.org,rows.env,true));
            if baseline < 13 {
                let trace_size: i64 = sqlx::query_scalar("SELECT size FROM call WHERE call_id=$1").bind(rows.call).fetch_one(&fixture.admin).await?;
                assert_eq!(trace_size, 59);
            }
            let retained: serde_json::Value = sqlx::query_scalar("SELECT result FROM run WHERE run_id=$1").bind(rows.run).fetch_one(&fixture.admin).await?;
            assert_eq!(retained,serde_json::json!("retained result"));
            // 019 retires a deleted file's object, though not one a live
            // version shares, even under the non-bypass owner.
            let retired: Vec<String> = sqlx::query_scalar("SELECT storage_path FROM retired_file_object")
                .fetch_all(&fixture.admin).await?;
            assert_eq!(retired, ["/files/gone"]);
            // Existing direct grants can come from several historical scripts.
            // Table-level REVOKE alone cannot remove the column ACLs below.
            for grant in [
                "GRANT ALL ON ALL TABLES IN SCHEMA hot TO",
                "GRANT CREATE ON SCHEMA hot TO",
                "GRANT UPDATE(plan_id) ON hot.plan TO",
                "GRANT SELECT(version) ON hot._sqlx_migrations TO",
                "GRANT REFERENCES(env_id) ON hot.env TO",
            ] {
                fixture.owner.execute(sqlx::AssertSqlSafe(format!("{grant} {}",fixture.runtime_role))).await?;
            }
            assert!(db::create_db_pool(&hot::val!({"uri": fixture.runtime_uri.clone()})).await.is_err());
            fixture.provision_ok().await?;
            fixture.provision_ok().await?;
            assert_runtime_contract(&fixture,&rows).await?;
            Ok(())
        }.await;
        fixture.cleanup().await?;
        result?;
    }
    Ok(())
}

#[tokio::test]
async fn alert_delivery_mismatch_aborts_013_and_rolls_back_all_ddl_then_retries_after_repair()
-> TestResult {
    let Ok(admin_uri) = std::env::var("HOT_TEST_POSTGRES_URI") else {
        return Ok(());
    };
    for mismatch in ["destination", "subscription_org", "subscription_env"] {
        let fixture = UpgradeDatabase::new(&admin_uri, 12).await?;
        let result: TestResult = async {
            let rows = ExistingRows::seed(&fixture.admin).await?;
            // Existing data can hold these references because FK checks only
            // check existence, not owner equality.
            if mismatch == "destination" {
                sqlx::query("UPDATE alert_delivery SET destination_id=$1 WHERE alert_delivery_id=$2")
                    .bind(rows.other_destination).bind(rows.delivery).execute(&fixture.admin).await?;
            } else {
                let subscription = Uuid::new_v4();
                let (org,env) = if mismatch == "subscription_org" { (rows.other_org,rows.other_env) } else { (rows.org,rows.sibling_env) };
                sqlx::query("INSERT INTO alert_subscription(alert_subscription_id,org_id,env_id,created_by_user_id) VALUES($1,$2,$3,$4)")
                    .bind(subscription).bind(org).bind(env).bind(rows.user).execute(&fixture.admin).await?;
                sqlx::query("UPDATE alert_delivery SET subscription_id=$1 WHERE alert_delivery_id=$2")
                    .bind(subscription).bind(rows.delivery).execute(&fixture.admin).await?;
            }
            let old_ledger = ledger(&fixture.owner).await?;
            let old_delivery: (Uuid,Uuid) = sqlx::query_as("SELECT subscription_id,destination_id FROM alert_delivery WHERE alert_delivery_id=$1")
                .bind(rows.delivery).fetch_one(&fixture.admin).await?;
            let error = fixture.upgrade().await.unwrap_err().to_string();
            assert!(error.contains("Existing alert delivery ownership mismatch"),"{error}");
            assert_eq!(ledger(&fixture.owner).await?,old_ledger);
            let delivery: (Uuid,Uuid) = sqlx::query_as("SELECT subscription_id,destination_id FROM alert_delivery WHERE alert_delivery_id=$1")
                .bind(rows.delivery).fetch_one(&fixture.admin).await?;
            assert_eq!(delivery,old_delivery,"failed migration must preserve evidence");
            let rolled_back: (bool,bool,bool) = sqlx::query_as("SELECT to_regclass('hot.management_key') IS NULL, NOT relrowsecurity, NOT EXISTS(SELECT 1 FROM information_schema.columns WHERE table_schema='hot' AND table_name='call' AND column_name='env_id') FROM pg_class WHERE oid='hot.alert_delivery'::regclass")
                .fetch_one(&fixture.owner).await?;
            assert_eq!(rolled_back,(true,true,true),"013 must roll back as a whole");
            // Nothing enforces RLS before 013, so the fixture repairs directly.
            // Production operators must investigate their own data.
            sqlx::query("UPDATE alert_delivery SET subscription_id=$1,destination_id=$2 WHERE alert_delivery_id=$3")
                .bind(rows.subscription).bind(rows.destination).bind(rows.delivery).execute(&fixture.owner).await?;
            fixture.upgrade().await?;
            assert_latest_ledger(&fixture.owner).await?;
            rows.add_org_key_issuer(&fixture.admin).await?;
            fixture.provision_ok().await?;
            assert_runtime_contract(&fixture,&rows).await?;
            Ok(())
        }.await;
        fixture.cleanup().await?;
        result?;
    }
    Ok(())
}

#[tokio::test]
async fn provisioning_rejects_shared_and_set_role_privileges_atomically() -> TestResult {
    let Ok(admin_uri) = std::env::var("HOT_TEST_POSTGRES_URI") else {
        return Ok(());
    };
    let fixture = UpgradeDatabase::new(&admin_uri, 13).await?;
    let result: TestResult = async {
        fixture.upgrade().await?;
        assert_latest_ledger(&fixture.owner).await?;
        fixture.provision_ok().await?;
        let raw = connect(&fixture.runtime_uri).await?;
        fixture.cluster.execute(sqlx::AssertSqlSafe(format!("GRANT {} TO {}",fixture.group_role,fixture.runtime_role))).await?;
        let inherited: bool = sqlx::query_scalar("SELECT pg_has_role(current_user,$1,'USAGE')").bind(&fixture.group_role).fetch_one(&raw).await?;
        assert!(!inherited,"test must cover NOINHERIT membership, not ordinary inherited privileges");
        for privilege in [
            "TRUNCATE ON hot.env",
            "REFERENCES(env_id) ON hot.env",
            "UPDATE(plan_id) ON hot.plan",
            "SELECT(version) ON hot._sqlx_migrations",
            "CREATE ON SCHEMA hot",
        ] {
            fixture.owner.execute(sqlx::AssertSqlSafe(format!("GRANT {privilege} TO {}",fixture.group_role))).await?;
            assert!(db::rls::validate_runtime_role(&raw,"hot").await.is_err(),"SET ROLE must not restore {privilege}");
            // This direct grant is removed early in the script, but must remain
            // if a later shared-authority check aborts the transaction.
            fixture.owner.execute(sqlx::AssertSqlSafe(format!("GRANT UPDATE(plan_id) ON hot.plan TO {}",fixture.runtime_role))).await?;
            let output = fixture.provision().await?;
            assert!(!output.status.success(),"provisioning unexpectedly accepted {privilege}");
            assert!(String::from_utf8_lossy(&output.stderr).contains("Runtime role"));
            let direct: bool = sqlx::query_scalar("SELECT has_column_privilege(current_user,'hot.plan','plan_id','UPDATE')").fetch_one(&raw).await?;
            assert!(direct,"failed provisioning must roll back direct ACL changes");
            fixture.owner.execute(sqlx::AssertSqlSafe(format!("REVOKE {privilege} FROM {}",fixture.group_role))).await?;
            fixture.provision_ok().await?;
            db::rls::validate_runtime_role(&raw,"hot").await?;
        }
        fixture.owner.execute("GRANT REFERENCES(env_id) ON hot.env TO PUBLIC").await?;
        assert!(db::rls::validate_runtime_role(&raw,"hot").await.is_err());
        assert!(!fixture.provision().await?.status.success());
        let public_unchanged: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_attribute a,LATERAL aclexplode(a.attacl) acl WHERE a.attrelid='hot.env'::regclass AND a.attname='env_id' AND acl.grantee=0 AND acl.privilege_type='REFERENCES')")
            .fetch_one(&fixture.owner).await?;
        assert!(public_unchanged,"the script must not mutate shared PUBLIC grants");
        fixture.owner.execute("REVOKE REFERENCES(env_id) ON hot.env FROM PUBLIC").await?;
        fixture.provision_ok().await?;
        db::rls::validate_runtime_role(&raw,"hot").await?;
        raw.close().await;
        Ok(())
    }.await;
    fixture.cleanup().await?;
    result
}

#[tokio::test]
async fn retained_cross_environment_origins_abort_013_without_rewriting_history() -> TestResult {
    let Ok(admin_uri) = std::env::var("HOT_TEST_POSTGRES_URI") else {
        return Ok(());
    };
    for mismatch in ["run_origin", "task_origin", "run_event"] {
        let fixture = UpgradeDatabase::new(&admin_uri, 9).await?;
        let result: TestResult = async {
            let rows = ExistingRows::seed(&fixture.admin).await?;
            let other_project = Uuid::new_v4();
            let other_build = Uuid::new_v4();
            let other_stream = Uuid::new_v4();
            let other_run = Uuid::new_v4();
            let event = Uuid::new_v4();
            let task = Uuid::new_v4();
            let local_run = Uuid::new_v4();
            let local_event = Uuid::new_v4();
            sqlx::query("INSERT INTO run(run_id,env_id,stream_id,build_id,run_type_id) SELECT $1,env_id,stream_id,build_id,1 FROM run WHERE run_id=$2")
                .bind(local_run).bind(rows.run).execute(&fixture.admin).await?;
            sqlx::query("INSERT INTO event(event_id,env_id,stream_id,event_type,event_data,event_time,created_by_user_id) SELECT $1,env_id,stream_id,'retained','{}',now(),$2 FROM run WHERE run_id=$3")
                .bind(local_event).bind(rows.user).bind(rows.run).execute(&fixture.admin).await?;
            sqlx::query("INSERT INTO project(project_id,env_id,name,created_by_user_id) VALUES($1,$2,'other',$3)")
                .bind(other_project).bind(rows.other_env).bind(rows.user).execute(&fixture.admin).await?;
            sqlx::query("INSERT INTO build(build_id,project_id,hash,size,created_by_user_id) VALUES($1,$2,'other',123,$3)")
                .bind(other_build).bind(other_project).bind(rows.user).execute(&fixture.admin).await?;
            sqlx::query("INSERT INTO stream(stream_id,env_id) VALUES($1,$2)").bind(other_stream).bind(rows.other_env).execute(&fixture.admin).await?;
            sqlx::query("INSERT INTO run(run_id,env_id,stream_id,build_id,run_type_id) VALUES($1,$2,$3,$4,1)")
                .bind(other_run).bind(rows.other_env).bind(other_stream).bind(other_build).execute(&fixture.admin).await?;
            sqlx::query("INSERT INTO event(event_id,env_id,stream_id,event_type,event_data,event_time,created_by_user_id) VALUES($1,$2,$3,'retained','{}',now(),$4)")
                .bind(event).bind(rows.other_env).bind(other_stream).bind(rows.user).execute(&fixture.admin).await?;
            let (read_sql, repair_sql, diagnostic) = match mismatch {
                "run_origin" => {
                    sqlx::query("UPDATE run SET origin_run_id=$1 WHERE run_id=$2").bind(other_run).bind(rows.run).execute(&fixture.admin).await?;
                    ("SELECT origin_run_id FROM run WHERE run_id=$1", "UPDATE run SET origin_run_id=$2 WHERE run_id=$1", "run has 1 origin-run/env ownership mismatches")
                }
                "task_origin" => {
                    sqlx::query("INSERT INTO task(task_id,env_id,stream_id,build_id,origin_run_id,function_name) SELECT $1,env_id,stream_id,build_id,$2,'::retained/work' FROM run WHERE run_id=$3")
                        .bind(task).bind(other_run).bind(rows.run).execute(&fixture.admin).await?;
                    ("SELECT origin_run_id FROM task WHERE task_id=$1", "UPDATE task SET origin_run_id=$2 WHERE task_id=$1", "task has 1 origin-run/env ownership mismatches")
                }
                _ => {
                    sqlx::query("UPDATE run SET event_id=$1 WHERE run_id=$2").bind(event).bind(rows.run).execute(&fixture.admin).await?;
                    ("SELECT event_id FROM run WHERE run_id=$1", "UPDATE run SET event_id=$2 WHERE run_id=$1", "run has 1 event/env ownership mismatches")
                }
            };
            let record = if mismatch == "task_origin" { task } else { rows.run };
            let original: Uuid = sqlx::query_scalar(read_sql).bind(record).fetch_one(&fixture.admin).await?;
            let error = fixture.upgrade().await.unwrap_err().to_string();
            assert!(error.contains(diagnostic),"{error}");
            let committed = ledger(&fixture.owner).await?;
            assert_eq!(committed.last().unwrap().0,12,"earlier transactional migrations remain committed");
            let retained: Uuid = sqlx::query_scalar(read_sql).bind(record).fetch_one(&fixture.admin).await?;
            assert_eq!(retained,original);
            let isolated: bool = sqlx::query_scalar("SELECT to_regclass('hot.management_key') IS NOT NULL")
                .fetch_one(&fixture.owner).await?;
            assert!(!isolated,"013 must roll back as a whole");
            let valid_target = if mismatch == "run_event" { local_event } else { local_run };
            sqlx::query(repair_sql).bind(record).bind(valid_target).execute(&fixture.owner).await?;
            fixture.upgrade().await?;
            assert!(ledger(&fixture.owner).await?.starts_with(&committed));
            assert_latest_ledger(&fixture.owner).await?;
            let retained: Uuid = sqlx::query_scalar(read_sql).bind(record).fetch_one(&fixture.admin).await?;
            assert_eq!(retained,valid_target,"valid non-NULL relationships must survive the preflight");
            fixture.provision_ok().await?;
            let raw = connect(&fixture.runtime_uri).await?;
            db::rls::validate_runtime_role(&raw,"hot").await?;
            raw.close().await;
            Ok(())
        }.await;
        fixture.cleanup().await?;
        result?;
    }
    Ok(())
}

#[tokio::test]
async fn bypass_rls_migrations_initialize_each_account_counter_once() -> TestResult {
    let Ok(admin_uri) = std::env::var("HOT_TEST_POSTGRES_URI") else {
        return Ok(());
    };
    let fixture = UpgradeDatabase::new(&admin_uri, 12).await?;
    let result: TestResult = async {
        // Two accounts with stored bytes: the first holds a trace, the second a
        // store entry. A superuser sees both accounts in every 014 pass.
        let rows = ExistingRows::seed(&fixture.admin).await?;
        sqlx::query("INSERT INTO store_map_entry(org_id,env_id,store_name,key,value,size) VALUES($1,$2,'retained','\"k\"','\"v\"',40)")
            .bind(rows.other_org).bind(rows.other_env).execute(&fixture.admin).await?;
        let privileged: bool = sqlx::query_scalar("SELECT rolsuper OR rolbypassrls FROM pg_roles WHERE rolname=current_user")
            .fetch_one(&fixture.admin).await?;
        assert!(privileged, "the fixture admin must bypass RLS");
        migrations_through(i64::MAX).await?.run(&fixture.admin).await?;
        assert_latest_ledger(&fixture.admin).await?;
        let counters: Vec<(Uuid, i64, i64, i64)> = sqlx::query_as(
            "SELECT env_id, COUNT(*), SUM(store_bytes)::bigint, SUM(call_bytes)::bigint FROM usage_storage GROUP BY env_id ORDER BY env_id",
        )
        .fetch_all(&fixture.admin)
        .await?;
        let mut expected = vec![(rows.env, 1, 0, 59), (rows.other_env, 1, 40, 0)];
        expected.sort();
        assert_eq!(counters, expected);
        Ok(())
    }
    .await;
    fixture.cleanup().await?;
    result
}

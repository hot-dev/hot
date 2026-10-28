//! Real-role RLS integration. Run against a dedicated PostgreSQL database with
//! HOT_TEST_POSTGRES_URI set to a privileged fixture connection. The test creates
//! a unique, restricted login; all isolation assertions use that login.

use hot::db::{self, DatabaseScope, ScopedPgPool, with_database_scope};
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

async fn upsert_test_webhook(
    db: &db::DatabasePool,
    webhook_id: &Uuid,
    build_id: &Uuid,
) -> Result<(), db::WebhookError> {
    db::Webhook::insert_webhook(
        db,
        webhook_id,
        build_id,
        "redeploy",
        "/stable",
        "POST",
        "::rls",
        "echo",
        "Stable webhook",
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .await
}

#[tokio::test]
async fn scoped_pool_enforces_isolation_under_a_restricted_login()
-> Result<(), Box<dyn std::error::Error>> {
    let Ok(admin_uri) = std::env::var("HOT_TEST_POSTGRES_URI") else {
        eprintln!("skipping PostgreSQL RLS test: HOT_TEST_POSTGRES_URI not set");
        return Ok(());
    };
    db::run_migrations(&hot::val!({"uri": admin_uri.clone(), "rls": {"enforce": false}})).await?;
    let admin = PgPoolOptions::new()
        .max_connections(2)
        .connect(&admin_uri)
        .await?;
    let role = format!("hot_rls_test_{}", Uuid::new_v4().simple());
    let password = Uuid::new_v4().simple().to_string();
    // Both interpolated values are generated identifier-safe UUID strings.
    admin.execute(sqlx::AssertSqlSafe(format!("CREATE ROLE {role} LOGIN PASSWORD '{password}' NOSUPERUSER NOBYPASSRLS NOINHERIT NOCREATEDB NOCREATEROLE"))).await?;
    let _role_cleanup = TestRoleCleanup {
        options: admin.connect_options().as_ref().clone(),
        role: role.clone(),
    };
    admin
        .execute(sqlx::AssertSqlSafe(format!(
            "GRANT USAGE ON SCHEMA hot TO {role}"
        )))
        .await?;
    // Mirror provision_runtime.sql: protected customer data gets DML, global
    // reference data gets SELECT, and migration control state gets no grants.
    let protected_tables: Vec<String> = sqlx::query_scalar(
        "SELECT format('%I.%I', n.nspname, c.relname) FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname='hot' AND c.relkind IN ('r','p') AND c.relrowsecurity AND c.relforcerowsecurity"
    ).fetch_all(&admin).await?;
    for table in protected_tables {
        admin
            .execute(sqlx::AssertSqlSafe(format!(
                "GRANT SELECT, INSERT, UPDATE, DELETE ON TABLE {table} TO {role}"
            )))
            .await?;
    }
    let reference_tables: Vec<String> = sqlx::query_scalar(
        "SELECT format('%I.%I', n.nspname, c.relname) FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname='hot' AND c.relkind IN ('r','p') AND c.relname IN ('build_type','email_verification_status','invite_status','org_user_role','run_status','run_type','task_status','team_user_role','org_plan_status','alert_delivery_status','alert_destination_type','plan')"
    ).fetch_all(&admin).await?;
    for table in reference_tables {
        admin
            .execute(sqlx::AssertSqlSafe(format!(
                "GRANT SELECT ON TABLE {table} TO {role}"
            )))
            .await?;
    }
    admin
        .execute(sqlx::AssertSqlSafe(format!(
            "GRANT USAGE, SELECT ON ALL SEQUENCES IN SCHEMA hot TO {role}"
        )))
        .await?;

    let mut runtime_url = url::Url::parse(&admin_uri)?;
    runtime_url
        .set_username(&role)
        .map_err(|_| "invalid fixture URL")?;
    runtime_url
        .set_password(Some(&password))
        .map_err(|_| "invalid fixture URL")?;
    let raw_runtime = PgPoolOptions::new()
        .max_connections(1)
        .after_connect(|conn, _| {
            Box::pin(async move {
                conn.execute("SET search_path = hot, public").await?;
                Ok(())
            })
        })
        .connect(runtime_url.as_str())
        .await?;
    db::rls::validate_runtime_role(&raw_runtime, "hot").await?;
    assert!(db::rls::validate_runtime_role(&admin, "hot").await.is_err());
    assert!(
        sqlx::query("DELETE FROM hot.plan")
            .execute(&raw_runtime)
            .await
            .is_err()
    );
    assert!(
        sqlx::query("DELETE FROM hot.run_status")
            .execute(&raw_runtime)
            .await
            .is_err()
    );
    assert!(
        sqlx::query("SELECT * FROM hot._sqlx_migrations")
            .fetch_all(&raw_runtime)
            .await
            .is_err()
    );
    admin
        .execute(sqlx::AssertSqlSafe(format!(
            "GRANT UPDATE ON hot.plan TO {role}"
        )))
        .await?;
    assert!(
        db::rls::validate_runtime_role(&raw_runtime, "hot")
            .await
            .is_err()
    );
    admin
        .execute(sqlx::AssertSqlSafe(format!(
            "REVOKE UPDATE ON hot.plan FROM {role}"
        )))
        .await?;
    admin
        .execute(sqlx::AssertSqlSafe(format!(
            "GRANT UPDATE(plan_id) ON hot.plan TO {role}"
        )))
        .await?;
    assert!(
        db::rls::validate_runtime_role(&raw_runtime, "hot")
            .await
            .is_err()
    );
    admin
        .execute(sqlx::AssertSqlSafe(format!(
            "REVOKE UPDATE(plan_id) ON hot.plan FROM {role}"
        )))
        .await?;
    admin
        .execute(sqlx::AssertSqlSafe(format!(
            "GRANT SELECT ON hot._sqlx_migrations TO {role}"
        )))
        .await?;
    assert!(
        db::rls::validate_runtime_role(&raw_runtime, "hot")
            .await
            .is_err()
    );
    admin
        .execute(sqlx::AssertSqlSafe(format!(
            "REVOKE SELECT ON hot._sqlx_migrations FROM {role}"
        )))
        .await?;
    db::rls::validate_runtime_role(&raw_runtime, "hot").await?;
    // These grants do not set rolsuper/BYPASSRLS, but expose data outside the
    // row policy boundary (including column-level REFERENCES grants).
    for privilege in ["REFERENCES", "REFERENCES(env_id)", "TRIGGER"] {
        admin
            .execute(sqlx::AssertSqlSafe(format!(
                "GRANT {privilege} ON hot.env TO {role}"
            )))
            .await?;
        assert!(
            db::rls::validate_runtime_role(&raw_runtime, "hot")
                .await
                .is_err()
        );
        admin
            .execute(sqlx::AssertSqlSafe(format!(
                "REVOKE {privilege} ON hot.env FROM {role}"
            )))
            .await?;
    }
    for privileged_role in [
        "pg_read_server_files",
        "pg_write_server_files",
        "pg_execute_server_program",
    ] {
        admin
            .execute(sqlx::AssertSqlSafe(format!(
                "GRANT {privileged_role} TO {role}"
            )))
            .await?;
        assert!(
            db::rls::validate_runtime_role(&raw_runtime, "hot")
                .await
                .is_err()
        );
        admin
            .execute(sqlx::AssertSqlSafe(format!(
                "REVOKE {privileged_role} FROM {role}"
            )))
            .await?;
    }
    db::rls::validate_runtime_role(&raw_runtime, "hot").await?;
    let pool = ScopedPgPool::new(raw_runtime.clone());

    let user = Uuid::new_v4();
    let invitee = Uuid::new_v4();
    let root = Uuid::new_v4();
    let child_a = Uuid::new_v4();
    let child_b = Uuid::new_v4();
    let env_a = Uuid::new_v4();
    let env_b = Uuid::new_v4();
    sqlx::query("INSERT INTO hot.\"user\" (user_id,email,created_by_user_id) VALUES ($1,$2,$1)")
        .bind(user)
        .bind(format!("{user}@rls.example.test"))
        .execute(&admin)
        .await?;
    sqlx::query(
        "INSERT INTO hot.org (org_id,name,slug,created_by_user_id) VALUES ($1,'RLS root',$2,$3)",
    )
    .bind(root)
    .bind(root.to_string())
    .bind(user)
    .execute(&admin)
    .await?;
    for child in [child_a, child_b] {
        sqlx::query("INSERT INTO hot.org (org_id,parent_org_id,name,slug,created_by_user_id) VALUES ($1,$2,'RLS child',$3,$4)")
            .bind(child).bind(root).bind(child.to_string()).bind(user).execute(&admin).await?;
    }
    for (org, env) in [(child_a, env_a), (child_b, env_b)] {
        sqlx::query("INSERT INTO hot.env (env_id,org_id,name,created_by_user_id) VALUES ($1,$2,'production',$3)")
            .bind(env).bind(org).bind(user).execute(&admin).await?;
    }
    let scope_a = DatabaseScope::Environment {
        org_id: child_a,
        env_id: env_a,
    };
    let scope_b = DatabaseScope::Environment {
        org_id: child_b,
        env_id: env_b,
    };
    let a = pool.with_scope(scope_a.clone());
    let b = pool.with_scope(scope_b.clone());
    for (p, org, env) in [(&a, child_a, env_a), (&b, child_b, env_b)] {
        sqlx::query("INSERT INTO store_map_entry (org_id,env_id,store_name,key,value) VALUES ($1,$2,'same-name','\"same-key\"','\"private\"')")
            .bind(org).bind(env).execute(p).await?;
    }
    let ids = |p: ScopedPgPool| async move {
        sqlx::query_scalar::<_, Uuid>("SELECT env_id FROM store_map_entry")
            .fetch_all(&p)
            .await
    };
    assert_eq!(ids(a.clone()).await?, vec![env_a]);
    assert_eq!(ids(b.clone()).await?, vec![env_b]);
    assert!(
        ids(pool.clone()).await.is_err(),
        "missing backend scope must fail before SQL"
    );
    // A direct query with no custom scope demonstrates the actual database
    // policy also denies rows; this is not only a Rust-wrapper test.
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM store_map_entry")
        .fetch_one(&raw_runtime)
        .await?;
    assert_eq!(count, 0);
    let changed = sqlx::query("UPDATE store_map_entry SET value = '\"changed\"' WHERE env_id = $1")
        .bind(env_b)
        .execute(&a)
        .await?;
    assert_eq!(changed.rows_affected(), 0);
    assert!(sqlx::query("INSERT INTO store_map_entry(org_id,env_id,store_name,key,value) VALUES($1,$2,'forged','0','0')")
        .bind(child_b).bind(env_b).execute(&a).await.is_err());
    assert!(
        sqlx::query("UPDATE store_map_entry SET org_id=$1, env_id=$2")
            .bind(child_b)
            .bind(env_b)
            .execute(&a)
            .await
            .is_err()
    );
    assert!(
        sqlx::query("TRUNCATE store_map_entry")
            .execute(&a)
            .await
            .is_err()
    );
    assert!(
        sqlx::query("CREATE TABLE hot.forbidden_runtime_ddl(id integer)")
            .execute(&a)
            .await
            .is_err()
    );
    assert!(
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "SET ROLE {}",
            url::Url::parse(&admin_uri)?.username()
        )))
        .execute(&a)
        .await
        .is_err()
    );

    let both = pool.with_scope(DatabaseScope::ReadSet {
        org_ids: vec![child_a, child_b],
        env_ids: vec![env_a, env_b],
    });
    let read_set_db = db::DatabasePool::Postgres(both.clone());
    assert_eq!(ids(both.clone()).await?.len(), 2);
    // Scope verification runs once per transaction, so it must be an index
    // probe rather than a scan of every tenant's environments. With sequential
    // scans disabled, applying either scope must leave hot.env unscanned.
    let read_set_orgs = serde_json::json!([child_a, child_b]).to_string();
    let read_set_envs = serde_json::json!([env_a, env_b]).to_string();
    for (mode, org, env, orgs, envs) in [
        (
            "environment",
            child_a.to_string(),
            env_a.to_string(),
            "[]",
            "[]",
        ),
        (
            "read_set",
            String::new(),
            String::new(),
            read_set_orgs.as_str(),
            read_set_envs.as_str(),
        ),
    ] {
        let scans = "SELECT COALESCE(seq_scan, 0) FROM pg_stat_xact_user_tables WHERE schemaname = 'hot' AND relname = 'env'";
        let mut tx = raw_runtime.begin().await?;
        sqlx::query("SET LOCAL enable_seqscan = off")
            .execute(&mut *tx)
            .await?;
        let before: i64 = sqlx::query_scalar(scans).fetch_one(&mut *tx).await?;
        sqlx::query("SELECT hot.apply_scope($1, $2, $3, $4, $5, '', '')")
            .bind(mode)
            .bind(&org)
            .bind(&env)
            .bind(orgs)
            .bind(envs)
            .execute(&mut *tx)
            .await?;
        let after: i64 = sqlx::query_scalar(scans).fetch_one(&mut *tx).await?;
        assert_eq!(
            before, after,
            "{mode} scope verification must not scan every environment"
        );
        let verified: String = sqlx::query_scalar(
            "SELECT current_setting('hot.scope_env_verified') || current_setting('hot.scope_envs_verified')",
        )
        .fetch_one(&mut *tx)
        .await?;
        assert!(verified.contains(&env_a.to_string()), "{mode}: {verified}");
        tx.rollback().await?;
    }
    assert!(
        sqlx::query("DELETE FROM store_map_entry")
            .execute(&both)
            .await
            .is_err()
    );
    let domain_error = db::domain::Domain::create(
        &read_set_db,
        &env_a,
        &format!("{}.example.test", Uuid::new_v4().simple()),
    )
    .await
    .unwrap_err();
    assert!(domain_error.to_string().contains("exact environment scope"));
    let org_only = pool.with_scope(DatabaseScope::Organization { org_id: child_a });
    assert!(
        ids(org_only).await?.is_empty(),
        "org metadata authority must not imply runtime-data authority"
    );
    let mismatched = pool.with_scope(DatabaseScope::Environment {
        org_id: child_a,
        env_id: env_b,
    });
    assert!(ids(mismatched).await?.is_empty());

    // A one-connection pool exercises reuse after errors, rollback, timeout,
    // concurrent scopes and nested task-local scopes.
    let (a_rows, b_rows) = tokio::join!(ids(a.clone()), ids(b.clone()));
    assert_eq!(a_rows?, vec![env_a]);
    assert_eq!(b_rows?, vec![env_b]);
    assert!(
        tokio::time::timeout(
            Duration::from_millis(30),
            sqlx::query("SELECT pg_sleep(1)").execute(&a)
        )
        .await
        .is_err()
    );
    assert_eq!(ids(b.clone()).await?, vec![env_b]);
    {
        let mut tx = a.begin().await?;
        sqlx::query("UPDATE store_map_entry SET value='\"rolled-back\"'")
            .execute(&mut *tx)
            .await?;
        tx.rollback().await?;
    }
    let value: serde_json::Value = sqlx::query_scalar("SELECT value FROM store_map_entry")
        .fetch_one(&a)
        .await?;
    assert_eq!(value, serde_json::json!("private"));
    with_database_scope(scope_a, async {
        assert_eq!(ids(pool.clone()).await.unwrap(), vec![env_a]);
        with_database_scope(scope_b, async {
            assert_eq!(ids(pool.clone()).await.unwrap(), vec![env_b]);
        })
        .await;
        assert_eq!(ids(pool.clone()).await.unwrap(), vec![env_a]);
    })
    .await;
    let setting: Option<String> =
        sqlx::query_scalar("SELECT current_setting('hot.scope_org',true)")
            .fetch_one(&raw_runtime)
            .await?;
    assert!(setting.is_none_or(|v| v.is_empty()));

    // Exercise the control-plane repository methods with the same restricted
    // login: passing SQLite tests must not hide missing RLS policy authority.
    sqlx::query("INSERT INTO hot.org_user(org_user_id,org_id,user_id,org_user_role_id,created_by_user_id) VALUES($1,$2,$3,2,$3)")
        .bind(Uuid::new_v4()).bind(root).bind(user).execute(&admin).await?;
    let runtime_db = db::DatabasePool::Postgres(pool.clone());

    // Profile scope must work before the invited user has any membership.
    sqlx::query("INSERT INTO hot.\"user\"(user_id,email,created_by_user_id) VALUES($1,$2,$1)")
        .bind(invitee)
        .bind(format!("{invitee}@rls.example.test"))
        .execute(&admin)
        .await?;
    let profile = pool.with_scope(DatabaseScope::User { user_id: invitee });
    let visible_users: Vec<Uuid> = sqlx::query_scalar("SELECT user_id FROM \"user\"")
        .fetch_all(&profile)
        .await?;
    assert_eq!(visible_users, vec![invitee]);
    assert_eq!(
        sqlx::query("UPDATE \"user\" SET name='My profile' WHERE user_id=$1")
            .bind(invitee)
            .execute(&profile)
            .await?
            .rows_affected(),
        1
    );
    assert_eq!(
        sqlx::query("UPDATE \"user\" SET name='Other profile' WHERE user_id=$1")
            .bind(user)
            .execute(&profile)
            .await?
            .rows_affected(),
        0
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM org")
            .fetch_one(&profile)
            .await?,
        0
    );

    let invitation = Uuid::new_v4();
    let code = Uuid::new_v4().simple().to_string();
    db::invite::Invite::insert_invite(
        &runtime_db,
        &invitation,
        &code,
        &format!("{invitee}@rls.example.test"),
        &root,
        1,
        &user,
        chrono::Utc::now() + chrono::Duration::hours(1),
    )
    .await?;
    db::invite::Invite::accept_for_user(&runtime_db, &code, invitee).await?;
    let root_scope = pool.with_scope(DatabaseScope::Organization { org_id: root });
    let directory: Vec<Uuid> = sqlx::query_scalar("SELECT user_id FROM \"user\" ORDER BY user_id")
        .fetch_all(&root_scope)
        .await?;
    let mut expected_directory = vec![user, invitee];
    expected_directory.sort();
    assert_eq!(directory, expected_directory);
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM org_user WHERE org_id=$1 AND user_id=$2 AND active"
        )
        .bind(root)
        .bind(invitee)
        .fetch_one(&root_scope)
        .await?,
        1
    );
    assert!(
        db::invite::Invite::accept_for_user(&runtime_db, &code, invitee)
            .await
            .is_err()
    );
    let provisioned = db::Org::create_child(
        &runtime_db,
        &root,
        &user,
        &db::org::CreateChild {
            name: "Provisioned child".into(),
            slug: format!("child-{}", Uuid::new_v4().simple()),
            external_id: None,
            idempotency_key: Uuid::new_v4().to_string(),
            hosted: false,
        },
    )
    .await?;
    let (management, token) = db::management_key::ManagementKey::create_for_user(
        &runtime_db,
        root,
        user,
        &db::management_key::CreateManagementKey {
            name: "test".into(),
            capabilities: vec!["children:read".into(), "runtime-keys:create".into()],
            target_org_ids: vec![provisioned.org.org_id],
            all_children: false,
            delegation_permissions: Some(serde_json::json!({"event:*": ["create"]})),
            expires_in_seconds: 60,
        },
    )
    .await?;
    assert!(
        db::management_key::ManagementKey::verify_token(&runtime_db, &token)
            .await
            .is_ok()
    );
    let root_env = Uuid::new_v4();
    sqlx::query("INSERT INTO hot.env(env_id,org_id,name,created_by_user_id) VALUES($1,$2,'management-isolation',$3)")
        .bind(root_env).bind(root).bind(user).execute(&admin).await?;
    let root_runtime = pool.with_scope(DatabaseScope::Environment {
        org_id: root,
        env_id: root_env,
    });
    let root_read_set = pool.with_scope(DatabaseScope::ReadSet {
        org_ids: vec![root],
        env_ids: vec![root_env],
    });
    for forbidden in [&root_runtime, &root_read_set, &a] {
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM management_key")
                .fetch_one(forbidden)
                .await?,
            0
        );
        assert!(sqlx::query("INSERT INTO management_key(management_key_id,root_org_id,created_by_user_id,name,secret_hash,capabilities,expires_at) VALUES($1,$2,$3,'forged',$4,'[\"children:create\"]',now()+interval '1 hour')")
            .bind(Uuid::new_v4()).bind(root).bind(user).bind(vec![0u8; 32]).execute(forbidden).await.is_err());
    }
    assert_eq!(
        sqlx::query("UPDATE management_key SET revoked_at=now() WHERE management_key_id=$1")
            .bind(management.management_key_id)
            .execute(&root_runtime)
            .await?
            .rows_affected(),
        0
    );
    assert!(
        db::management_key::ManagementKey::verify_token(&runtime_db, &token)
            .await
            .is_ok()
    );
    sqlx::query("DELETE FROM hot.env WHERE env_id=$1")
        .bind(root_env)
        .execute(&admin)
        .await?;
    let (delegated_id, _) = management
        .issue_runtime_key(
            &runtime_db,
            provisioned.org.org_id,
            provisioned.env_id,
            "delegated",
            &serde_json::json!({"event:*": ["create"]}),
        )
        .await?;
    let delegated_pool = pool.with_scope(DatabaseScope::Environment {
        org_id: provisioned.org.org_id,
        env_id: provisioned.env_id,
    });
    let delegated_db = db::DatabasePool::Postgres(delegated_pool.clone());
    let delegated_key = db::api_key::ApiKey::get_api_key(&delegated_db, &delegated_id).await?;
    assert!(
        db::management_key::ManagementKey::effective_runtime_permissions(
            &runtime_db,
            &delegated_key
        )
        .await
        .is_ok()
    );
    assert_eq!(
        sqlx::query("DELETE FROM api_key_management_issuer WHERE api_key_id=$1")
            .bind(delegated_id)
            .execute(&delegated_pool)
            .await?
            .rows_affected(),
        0
    );
    assert!(
        sqlx::query("DELETE FROM api_key WHERE api_key_id=$1")
            .bind(delegated_id)
            .execute(&delegated_pool)
            .await
            .is_err()
    );
    db::management_key::ManagementKey::revoke_for_user(
        &runtime_db,
        root,
        user,
        management.management_key_id,
    )
    .await?;
    assert!(
        db::management_key::ManagementKey::verify_token(&runtime_db, &token)
            .await
            .is_err()
    );
    assert!(
        db::management_key::ManagementKey::effective_runtime_permissions(
            &runtime_db,
            &delegated_key
        )
        .await
        .is_err()
    );
    sqlx::query("UPDATE api_key SET active=false WHERE api_key_id=$1")
        .bind(delegated_id)
        .execute(&delegated_pool)
        .await?;
    sqlx::query("DELETE FROM hot.api_key_management_issuer WHERE api_key_id=$1")
        .bind(delegated_id)
        .execute(&admin)
        .await?;
    sqlx::query("DELETE FROM hot.api_key WHERE api_key_id=$1")
        .bind(delegated_id)
        .execute(&admin)
        .await?;
    // Ledger admission shares the root limit across siblings and survives run
    // retention; repeating the same run UUID consumes no second slot.
    sqlx::query("UPDATE hot.org SET features=$1 WHERE org_id=$2")
        .bind(serde_json::json!({"runs_per_month": 2}))
        .bind(root)
        .execute(&admin)
        .await?;
    let admitted_a = Uuid::new_v4();
    let admitted_b = Uuid::new_v4();
    let db_a = db::DatabasePool::Postgres(a.clone());
    let db_b = db::DatabasePool::Postgres(b.clone());
    db::Run::admit_execution(&db_a, &child_a, &env_a, &admitted_a).await?;
    db::Run::admit_execution(&db_a, &child_a, &env_a, &admitted_a).await?;
    db::Run::admit_execution(&db_b, &child_b, &env_b, &admitted_b).await?;
    assert!(matches!(
        db::Run::admit_execution(&db_a, &child_a, &env_a, &Uuid::new_v4()).await,
        Err(db::run::RunError::QuotaExceeded(_))
    ));
    assert!(
        db::Run::admit_execution(&db_a, &child_b, &env_b, &Uuid::new_v4())
            .await
            .is_err()
    );
    assert!(matches!(
        db::Run::admit_execution(&read_set_db, &child_a, &env_a, &Uuid::new_v4()).await,
        Err(db::run::RunError::ScopeDenied)
    ));
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM run_admission")
            .fetch_one(&a)
            .await?,
        1
    );
    assert_eq!(
        sqlx::query("DELETE FROM run_admission")
            .execute(&a)
            .await?
            .rows_affected(),
        0
    );
    sqlx::query("DELETE FROM hot.run_admission WHERE root_org_id=$1")
        .bind(root)
        .execute(&admin)
        .await?;
    sqlx::query("UPDATE hot.org SET features=NULL WHERE org_id=$1")
        .bind(root)
        .execute(&admin)
        .await?;
    sqlx::query("DELETE FROM hot.invite WHERE invite_id=$1")
        .bind(invitation)
        .execute(&admin)
        .await?;

    sqlx::query("DELETE FROM hot.management_key WHERE root_org_id=$1")
        .bind(root)
        .execute(&admin)
        .await?;
    sqlx::query("DELETE FROM hot.org_child_provisioning WHERE root_org_id=$1")
        .bind(root)
        .execute(&admin)
        .await?;
    sqlx::query(
        "DELETE FROM hot.stream WHERE env_id IN (SELECT env_id FROM hot.env WHERE org_id=$1)",
    )
    .bind(provisioned.org.org_id)
    .execute(&admin)
    .await?;
    sqlx::query("DELETE FROM hot.env WHERE org_id=$1")
        .bind(provisioned.org.org_id)
        .execute(&admin)
        .await?;
    sqlx::query("DELETE FROM hot.org WHERE org_id=$1")
        .bind(provisioned.org.org_id)
        .execute(&admin)
        .await?;
    sqlx::query("DELETE FROM hot.org_note WHERE org_id=$1")
        .bind(root)
        .execute(&admin)
        .await?;
    sqlx::query("DELETE FROM hot.org_user WHERE org_id=$1")
        .bind(root)
        .execute(&admin)
        .await?;

    // Service scopes read the directories they deliver to (migration 017):
    // alert recipients, billing emails, deactivated members an org can
    // reactivate, and the root administrators a child inherits.
    let member = Uuid::new_v4();
    let former = Uuid::new_v4();
    for (id, name) in [(member, "member"), (former, "former")] {
        sqlx::query(
            "INSERT INTO hot.\"user\" (user_id,email,created_by_user_id) VALUES ($1,$2,$1)",
        )
        .bind(id)
        .bind(format!("{name}-{id}@rls.example.test"))
        .execute(&admin)
        .await?;
    }
    // A child grant requires an active root membership.
    for (org, member_id, role, active) in [
        (root, user, 2_i16, true),
        (root, member, 1, true),
        (root, former, 1, true),
        (child_a, member, 1, true),
        (child_a, former, 1, false),
    ] {
        sqlx::query("INSERT INTO hot.org_user (org_id,user_id,org_user_role_id,active,created_by_user_id) VALUES ($1,$2,$3,$4,$5)")
            .bind(org).bind(member_id).bind(role).bind(active).bind(user).execute(&admin).await?;
    }
    let directory_users = [user, member, former];
    let users_seen = |scope: DatabaseScope| {
        let pool = pool.with_scope(scope);
        async move {
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM hot.\"user\" WHERE user_id=ANY($1)")
                .bind(directory_users.to_vec())
                .fetch_one(&pool)
                .await
        }
    };
    let alerts_scope = DatabaseScope::Maintenance {
        operation: db::MaintenanceOperation::Alerts,
        org_id: None,
    };
    let billing_scope = DatabaseScope::Maintenance {
        operation: db::MaintenanceOperation::Billing,
        org_id: Some(root),
    };
    let child_scope = DatabaseScope::Organization { org_id: child_a };
    assert_eq!(users_seen(alerts_scope.clone()).await?, 3);
    assert_eq!(users_seen(billing_scope.clone()).await?, 3);
    assert_eq!(users_seen(child_scope.clone()).await?, 3);
    assert_eq!(
        users_seen(DatabaseScope::Organization { org_id: child_b }).await?,
        1,
        "a sibling sees only the inherited root administrator"
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM hot.org_user WHERE (org_id=$1 AND user_id=$2) OR user_id=ANY($3)"
        )
        .bind(root)
        .bind(user)
        .bind(vec![member, former])
        .fetch_one(&pool.with_scope(alerts_scope.clone()))
        .await?,
        5
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM hot.org_user WHERE org_id=$1")
            .bind(root)
            .fetch_one(&pool.with_scope(child_scope))
            .await?,
        1,
        "a child reads the root administrator it inherits"
    );
    let billing = pool.with_scope(billing_scope);
    sqlx::query("INSERT INTO hot.org_note (org_id,category,note_type,message) VALUES ($1,'billing','compute-units.threshold','80%')")
        .bind(root)
        .execute(&billing)
        .await?;
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM hot.org_note WHERE org_id=$1 AND note_type='compute-units.threshold'"
        )
        .bind(root)
        .fetch_one(&billing)
        .await?,
        1
    );
    assert!(
        sqlx::query("INSERT INTO hot.org_note (org_id,category,note_type,message) VALUES ($1,'billing','compute-units.threshold','80%')")
            .bind(child_a)
            .execute(&pool.with_scope(DatabaseScope::Maintenance {
                operation: db::MaintenanceOperation::Billing,
                org_id: Some(child_b),
            }))
            .await
            .is_err(),
        "billing writes notes only for its own account"
    );
    sqlx::query("DELETE FROM hot.org_note WHERE org_id=$1")
        .bind(root)
        .execute(&admin)
        .await?;
    sqlx::query("DELETE FROM hot.org_user WHERE (org_id=$1 AND user_id=$2) OR user_id=ANY($3)")
        .bind(root)
        .bind(user)
        .bind(vec![member, former])
        .execute(&admin)
        .await?;

    // Delivery ownership is checked even under the cross-account alert worker
    // scope. A same-org subscription in another env is also a foreign target.
    let alerts = pool.with_scope(DatabaseScope::Maintenance {
        operation: db::MaintenanceOperation::Alerts,
        org_id: None,
    });
    let env_a_other = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO hot.env(env_id,org_id,name,created_by_user_id) VALUES($1,$2,'other',$3)",
    )
    .bind(env_a_other)
    .bind(child_a)
    .bind(user)
    .execute(&admin)
    .await?;
    let alert_a = Uuid::new_v4();
    let destination_a = Uuid::new_v4();
    let destination_b = Uuid::new_v4();
    let subscription_a = Uuid::new_v4();
    let subscription_b = Uuid::new_v4();
    let subscription_other_env = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO alert(alert_id,org_id,env_id,channel,data) VALUES($1,$2,$3,'test','{}')",
    )
    .bind(alert_a)
    .bind(child_a)
    .bind(env_a)
    .execute(&a)
    .await?;
    for (id, org) in [(destination_a, child_a), (destination_b, child_b)] {
        sqlx::query("INSERT INTO hot.alert_destination(alert_destination_id,org_id,name,destination_type_id,config,created_by_user_id) VALUES($1,$2,$3,1,'{}',$4)")
            .bind(id).bind(org).bind(id.to_string()).bind(user).execute(&admin).await?;
    }
    for (id, org, env) in [
        (subscription_a, child_a, env_a),
        (subscription_b, child_b, env_b),
        (subscription_other_env, child_a, env_a_other),
    ] {
        sqlx::query("INSERT INTO hot.alert_subscription(alert_subscription_id,org_id,env_id,created_by_user_id) VALUES($1,$2,$3,$4)")
            .bind(id).bind(org).bind(env).bind(user).execute(&admin).await?;
    }
    for executor in [&a, &alerts] {
        for (subscription, destination) in [
            (subscription_b, destination_a),
            (subscription_other_env, destination_a),
            (subscription_a, destination_b),
        ] {
            assert!(sqlx::query("INSERT INTO alert_delivery(alert_delivery_id,alert_id,subscription_id,destination_id) VALUES($1,$2,$3,$4)")
                .bind(Uuid::new_v4()).bind(alert_a).bind(subscription).bind(destination).execute(executor).await.is_err());
        }
    }
    let delivery = Uuid::new_v4();
    sqlx::query("INSERT INTO alert_delivery(alert_delivery_id,alert_id,subscription_id,destination_id) VALUES($1,$2,$3,$4)")
        .bind(delivery).bind(alert_a).bind(subscription_a).bind(destination_a).execute(&a).await?;
    assert!(
        sqlx::query("UPDATE alert_delivery SET subscription_id=$1 WHERE alert_delivery_id=$2")
            .bind(subscription_other_env)
            .bind(delivery)
            .execute(&alerts)
            .await
            .is_err()
    );
    sqlx::query("UPDATE alert_delivery SET attempts=attempts+1 WHERE alert_delivery_id=$1")
        .bind(delivery)
        .execute(&alerts)
        .await?;
    sqlx::query("DELETE FROM hot.alert WHERE alert_id=$1")
        .bind(alert_a)
        .execute(&admin)
        .await?;
    sqlx::query("DELETE FROM hot.alert_subscription WHERE alert_subscription_id=ANY($1)")
        .bind(vec![subscription_a, subscription_b, subscription_other_env])
        .execute(&admin)
        .await?;
    sqlx::query("DELETE FROM hot.alert_destination WHERE alert_destination_id=ANY($1)")
        .bind(vec![destination_a, destination_b])
        .execute(&admin)
        .await?;
    sqlx::query("DELETE FROM hot.env WHERE env_id=$1")
        .bind(env_a_other)
        .execute(&admin)
        .await?;

    // Writer shards may receive multiple customers in one flush. A task-local
    // scope does not survive into the writer task, so each write carries its
    // trusted execution scope and the writer partitions transactions by it.
    use hot::lang::emitter::database_writer::{DatabaseWrite, DatabaseWriter};
    use hot::lang::event::ExecutionContext;
    use std::sync::Arc;
    let writer_db = Arc::new(tokio::sync::RwLock::new(Some(db::DatabasePool::Postgres(
        pool.clone(),
    ))));
    let writer = DatabaseWriter::new(writer_db);
    let run_a = Uuid::new_v4();
    let run_b = Uuid::new_v4();
    let project_a = Uuid::new_v4();
    let project_b = Uuid::new_v4();
    let build_a = Uuid::new_v4();
    let build_b = Uuid::new_v4();
    for (connection, env, project, build) in [
        (&a, env_a, project_a, build_a),
        (&b, env_b, project_b, build_b),
    ] {
        sqlx::query("INSERT INTO project(project_id,env_id,name,created_by_user_id) VALUES($1,$2,'owned-project',$3)")
            .bind(project).bind(env).bind(user).execute(connection).await?;
        sqlx::query("INSERT INTO build(build_id,project_id,hash,size,created_by_user_id) VALUES($1,$2,'test-build',0,$3)")
            .bind(build).bind(project).bind(user).execute(connection).await?;
    }
    let org_only = pool.with_scope(DatabaseScope::Organization { org_id: child_a });
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM project")
            .fetch_one(&org_only)
            .await?,
        1
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM build")
            .fetch_one(&org_only)
            .await?,
        0
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM build")
            .fetch_one(&a)
            .await?,
        1
    );
    let deployment_error = db::Build::activate_build_directly(&read_set_db, &build_a, &user)
        .await
        .unwrap_err();
    assert!(
        deployment_error
            .to_string()
            .contains("exact environment scope")
    );
    assert!(
        !sqlx::query_scalar::<_, bool>("SELECT deployed FROM build WHERE build_id=$1")
            .bind(build_a)
            .fetch_one(&a)
            .await?
    );

    // Stable webhook capabilities must survive a new build of the same
    // project, while their project/environment/organization ownership cannot
    // change. Exercise the real upsert used by local and manifest deployments.
    let next_build_a = Uuid::new_v4();
    let other_project_a = Uuid::new_v4();
    let other_build_a = Uuid::new_v4();
    sqlx::query("INSERT INTO project(project_id,env_id,name,created_by_user_id) VALUES($1,$2,'other-project',$3)")
        .bind(other_project_a).bind(env_a).bind(user).execute(&a).await?;
    for (project, build) in [(project_a, next_build_a), (other_project_a, other_build_a)] {
        sqlx::query("INSERT INTO build(build_id,project_id,hash,size,created_by_user_id) VALUES($1,$2,'redeploy-build',0,$3)")
            .bind(build).bind(project).bind(user).execute(&a).await?;
    }
    let webhook_id = Uuid::new_v4();
    upsert_test_webhook(&db_a, &webhook_id, &build_a).await?;
    let token = db::Webhook::get_webhook(&db_a, &webhook_id).await?.token();
    assert_eq!(
        db::Webhook::find_existing_webhook_id_for_build(
            &db_a,
            &next_build_a,
            "redeploy",
            "/stable",
            "POST",
        )
        .await?,
        Some(webhook_id)
    );
    upsert_test_webhook(&db_a, &webhook_id, &next_build_a).await?;
    let redeployed = db::Webhook::get_webhook(&db_a, &webhook_id).await?;
    assert_eq!(redeployed.build_id, next_build_a);
    assert_eq!(redeployed.token(), token);
    // An unchanged build remains a valid upsert as well.
    upsert_test_webhook(&db_a, &webhook_id, &next_build_a).await?;
    let queue_db = db::DatabasePool::Postgres(pool.with_scope(DatabaseScope::Maintenance {
        operation: db::MaintenanceOperation::Queue,
        org_id: None,
    }));
    for connection in [&db_a, &queue_db] {
        for forbidden_build in [other_build_a, build_b, Uuid::new_v4()] {
            assert!(
                upsert_test_webhook(connection, &webhook_id, &forbidden_build)
                    .await
                    .is_err()
            );
            // INSERT ... ON CONFLICT can fail its insertion RLS check before
            // reaching the update trigger. A direct update proves the guard
            // itself rejects hidden/missing builds, including under broad
            // maintenance visibility where both projects are otherwise legal.
            let db::DatabasePool::Postgres(pg_connection) = connection else {
                unreachable!()
            };
            let error = sqlx::query("UPDATE webhook SET build_id=$1 WHERE webhook_id=$2")
                .bind(forbidden_build)
                .bind(webhook_id)
                .execute(pg_connection)
                .await
                .unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("Webhook project ownership is immutable"),
                "unexpected webhook reassignment error: {error}"
            );
            assert_eq!(
                db::Webhook::get_webhook(&db_a, &webhook_id).await?.build_id,
                next_build_a
            );
        }
    }
    // Rolling back to an earlier build of that same project also keeps the
    // public URL stable; broad queue visibility does not grant reparenting.
    upsert_test_webhook(&queue_db, &webhook_id, &build_a).await?;
    assert_eq!(
        db::Webhook::get_webhook(&db_a, &webhook_id).await?.token(),
        token
    );
    assert!(
        db::Webhook::get_webhook(&db::DatabasePool::Postgres(b.clone()), &webhook_id)
            .await
            .is_err()
    );
    sqlx::query("DELETE FROM webhook WHERE webhook_id=$1")
        .bind(webhook_id)
        .execute(&a)
        .await?;
    sqlx::query("DELETE FROM build WHERE build_id=ANY($1)")
        .bind(vec![next_build_a, other_build_a])
        .execute(&a)
        .await?;
    sqlx::query("DELETE FROM project WHERE project_id=$1")
        .bind(other_project_a)
        .execute(&a)
        .await?;

    for (org, env, run, build) in [
        (child_a, env_a, run_a, build_a),
        (child_b, env_b, run_b, build_b),
    ] {
        writer.write(DatabaseWrite::RunStart {
            execution_context: ExecutionContext::new(
                run,
                Uuid::new_v4(),
                4,
                Some(env),
                Some(user),
                Some(org),
                Some(build),
            ),
            event_time: chrono::Utc::now(),
        });
        writer.write(DatabaseWrite::RunStop {
            run_id: run,
            event_time: chrono::Utc::now(),
            result: hot::val!("private result"),
        });
    }
    writer.flush_async().await?;
    writer.shutdown().await?;
    let runs_a: Vec<Uuid> = sqlx::query_scalar("SELECT run_id FROM run")
        .fetch_all(&a)
        .await?;
    let runs_b: Vec<Uuid> = sqlx::query_scalar("SELECT run_id FROM run")
        .fetch_all(&b)
        .await?;
    assert_eq!(runs_a, vec![run_a]);
    assert_eq!(runs_b, vec![run_b]);

    // Two actual runtime connections race sibling writes against one account
    // budget. A single-connection pool would conceal a missing account lock.
    let quota_runtime = PgPoolOptions::new()
        .max_connections(2)
        .after_connect(|conn, _| {
            Box::pin(async move {
                conn.execute("SET search_path = hot, public").await?;
                Ok(())
            })
        })
        .connect(runtime_url.as_str())
        .await?;
    let quota_pool = ScopedPgPool::new(quota_runtime.clone());
    let quota_a = db::DatabasePool::Postgres(quota_pool.with_scope(DatabaseScope::Environment {
        org_id: child_a,
        env_id: env_a,
    }));
    let quota_b = db::DatabasePool::Postgres(quota_pool.with_scope(DatabaseScope::Environment {
        org_id: child_b,
        env_id: env_b,
    }));
    sqlx::query("UPDATE hot.org SET features=$1 WHERE org_id=$2")
        .bind(serde_json::json!({"store_storage_bytes":12,"call_storage_bytes":60}))
        .bind(root)
        .execute(&admin)
        .await?;
    // Child overrides cannot enlarge the shared account budget.
    sqlx::query("UPDATE hot.org SET features=$1 WHERE org_id=ANY($2)")
        .bind(serde_json::json!({"store_storage_bytes":-1,"call_storage_bytes":-1}))
        .bind(vec![child_a, child_b])
        .execute(&admin)
        .await?;
    use hot::store::Store;
    let store_a = hot::store::postgres::PgStore::new(Arc::new(quota_a.clone()), child_a, env_a);
    let store_b = hot::store::postgres::PgStore::new(Arc::new(quota_b.clone()), child_b, env_b);
    let (stored_a, stored_b) = tokio::join!(
        store_a.put(
            "quota",
            serde_json::json!("k"),
            serde_json::json!("v"),
            Some(vec![1.0]),
            Some("ab".into())
        ),
        store_b.put(
            "quota",
            serde_json::json!("k"),
            serde_json::json!("v"),
            Some(vec![1.0]),
            Some("ab".into())
        )
    );
    assert_ne!(
        stored_a.is_ok(),
        stored_b.is_ok(),
        "siblings must share the final storage slot"
    );
    let winning_store = if stored_a.is_ok() { &store_a } else { &store_b };
    winning_store
        .put(
            "quota",
            serde_json::json!("k"),
            serde_json::json!("v"),
            Some(vec![1.0]),
            Some("ab".into()),
        )
        .await?;
    assert!(
        winning_store
            .put(
                "quota",
                serde_json::json!("k"),
                serde_json::json!("larger"),
                Some(vec![1.0]),
                Some("ab".into())
            )
            .await
            .is_err()
    );
    assert!(
        !winning_store
            .put_if_missing(
                "quota",
                serde_json::json!("k"),
                serde_json::json!("larger"),
                None,
                None
            )
            .await?
    );
    assert_eq!(
        winning_store
            .get("quota", &serde_json::json!("k"))
            .await?
            .unwrap()
            .value,
        serde_json::json!("v")
    );

    // Files share the account's storage budget the same way, counted by the
    // usage_file triggers (migration 016) rather than a sum over every file.
    sqlx::query("UPDATE hot.org SET features=$1 WHERE org_id=$2")
        .bind(serde_json::json!({
            "store_storage_bytes": 12,
            "call_storage_bytes": 60,
            "storage_bytes": 10
        }))
        .bind(root)
        .execute(&admin)
        .await?;
    let file_bytes = async || -> Result<i64, sqlx::Error> {
        sqlx::query_scalar(
            "SELECT COALESCE(SUM(active_bytes),0)::bigint FROM hot.usage_file WHERE org_id=ANY($1)",
        )
        .bind(vec![child_a, child_b])
        .fetch_one(&admin)
        .await
    };
    use hot::db::file::{insert_file_record, mark_file_inactive, update_file_record};
    let (file_a, file_b) = tokio::join!(
        insert_file_record(
            &quota_a,
            "quota/a",
            6,
            None,
            None,
            "local",
            "quota/a",
            child_a,
            Some(env_a),
            user,
            None
        ),
        insert_file_record(
            &quota_b,
            "quota/b",
            6,
            None,
            None,
            "local",
            "quota/b",
            child_b,
            Some(env_b),
            user,
            None
        )
    );
    assert_ne!(
        file_a.is_ok(),
        file_b.is_ok(),
        "siblings must share the file budget"
    );
    assert_eq!(file_bytes().await?, 6);
    let (file_db, other_db, other_org, other_env) = if file_a.is_ok() {
        (&quota_a, &quota_b, child_b, env_b)
    } else {
        (&quota_b, &quota_a, child_a, env_a)
    };
    let file = file_a.or(file_b)?;
    // A replacement counts its growth; the budget then fills exactly.
    update_file_record(
        file_db,
        file.file_id,
        7,
        None,
        None,
        "quota/new",
        user,
        None,
    )
    .await?;
    insert_file_record(
        other_db,
        "quota/other",
        3,
        None,
        None,
        "local",
        "quota/other",
        other_org,
        Some(other_env),
        user,
        None,
    )
    .await?;
    assert_eq!(file_bytes().await?, 10);
    assert!(
        update_file_record(
            file_db,
            file.file_id,
            8,
            None,
            None,
            "quota/bigger",
            user,
            None
        )
        .await
        .is_err(),
        "a replacement past the budget is refused"
    );
    // Deactivating a file frees its bytes for the account.
    assert!(mark_file_inactive(file_db, file.file_id, "quota/new", user, None).await?);
    assert_eq!(file_bytes().await?, 3);
    insert_file_record(
        file_db,
        "quota/after",
        7,
        None,
        None,
        "local",
        "quota/after",
        file.org_id,
        file.env_id,
        user,
        None,
    )
    .await?;
    assert_eq!(file_bytes().await?, 10);
    let summed: i64 = sqlx::query_scalar(
        "SELECT COALESCE(SUM(size),0)::bigint FROM hot.file WHERE active AND org_id=ANY($1)",
    )
    .bind(vec![child_a, child_b])
    .fetch_one(&admin)
    .await?;
    assert_eq!(summed, 10, "the counters agree with the files");

    // A full trace budget rejects trace payloads while both terminal run writes
    // still persist. The SQL size includes JSONB formatting and UTF-8 errors.
    sqlx::query("UPDATE hot.run SET status_id=1,stop_time=NULL WHERE run_id=ANY($1)")
        .bind(vec![run_a, run_b])
        .execute(&admin)
        .await?;
    let call_a = Uuid::new_v4();
    let call_b = Uuid::new_v4();
    let trace_a = DatabaseWriter::new(Arc::new(tokio::sync::RwLock::new(Some(quota_a.clone()))));
    let trace_b = DatabaseWriter::new(Arc::new(tokio::sync::RwLock::new(Some(quota_b.clone()))));
    let make_trace = |org, env, run, build, call, error: &str| DatabaseWrite::Call {
        execution_context: Box::new(ExecutionContext::new(
            run,
            Uuid::new_v4(),
            4,
            Some(env),
            Some(user),
            Some(org),
            Some(build),
        )),
        call_id: call,
        parent_call_id: None,
        function_name: "test/trace".into(),
        static_scope: "test/trace".into(),
        runtime_path: "test/trace".into(),
        call_depth: 0,
        args: Some("{\"a\":1}".into()),
        return_value: None,
        error: Some(error.into()),
        flow: None,
        file: None,
        line: None,
        column: None,
        position: None,
        start_time: Some(chrono::Utc::now()),
        stop_time: None,
        duration_us: None,
    };
    trace_a.write(make_trace(child_a, env_a, run_a, build_a, call_a, "é"));
    trace_b.write(make_trace(child_b, env_b, run_b, build_b, call_b, "é"));
    for (trace, run) in [(&trace_a, run_a), (&trace_b, run_b)] {
        trace.write(DatabaseWrite::RunStop {
            run_id: run,
            event_time: chrono::Utc::now(),
            result: hot::val::Val::Null,
        });
    }
    let (traced_a, traced_b) = tokio::join!(trace_a.flush_async(), trace_b.flush_async());
    assert_ne!(
        traced_a.is_ok(),
        traced_b.is_ok(),
        "siblings must share the final trace slot"
    );
    let shutdown_a = trace_a.shutdown().await;
    let shutdown_b = trace_b.shutdown().await;
    assert_eq!(traced_a.is_ok(), shutdown_a.is_ok());
    assert_eq!(traced_b.is_ok(), shutdown_b.is_ok());
    assert_eq!(
        db::Run::get_run(&db_a, &run_a).await?.status_id,
        db::RunStatus::Succeeded.as_id()
    );
    assert_eq!(
        db::Run::get_run(&db_b, &run_b).await?.status_id,
        db::RunStatus::Succeeded.as_id()
    );
    let (winner_db, winner_org, winner_env, winner_run, winner_build, winner_call) =
        if traced_a.is_ok() {
            (&quota_a, child_a, env_a, run_a, build_a, call_a)
        } else {
            (&quota_b, child_b, env_b, run_b, build_b, call_b)
        };
    let db::DatabasePool::Postgres(winner_pg) = winner_db else {
        unreachable!()
    };
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT size FROM call WHERE call_id=$1")
            .bind(winner_call)
            .fetch_one(winner_pg)
            .await?,
        60
    );
    let growing_trace =
        DatabaseWriter::new(Arc::new(tokio::sync::RwLock::new(Some(winner_db.clone()))));
    growing_trace.write(make_trace(
        winner_org,
        winner_env,
        winner_run,
        winner_build,
        winner_call,
        "éé",
    ));
    assert!(growing_trace.flush_async().await.is_err());
    assert!(growing_trace.shutdown().await.is_err());
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT error FROM call WHERE call_id=$1")
            .bind(winner_call)
            .fetch_one(winner_pg)
            .await?,
        "é"
    );

    // Re-run only 013's call backfill inside a rollback transaction under a
    // real non-BYPASS login. It keeps ownership and accounts all bytes.
    let mut backfill = winner_pg.begin().await?;
    sqlx::query("UPDATE call SET size=0 WHERE call_id=$1")
        .bind(winner_call)
        .execute(&mut *backfill)
        .await?;
    let migration =
        include_str!("../../../resources/db/postgres/migrations/013_scope_isolation.sql");
    let start = migration.find("UPDATE hot.call c SET").unwrap();
    let end = start + migration[start..].find(';').unwrap() + 1;
    sqlx::raw_sql(&migration[start..end])
        .execute(&mut *backfill)
        .await?;
    assert_eq!(
        sqlx::query_as::<_, (Uuid, i64)>("SELECT run_id,size FROM call WHERE call_id=$1")
            .bind(winner_call)
            .fetch_one(&mut *backfill)
            .await?,
        (winner_run, 60)
    );
    backfill.rollback().await?;
    sqlx::query("DELETE FROM hot.call WHERE run_id=ANY($1)")
        .bind(vec![run_a, run_b])
        .execute(&admin)
        .await?;
    sqlx::query("UPDATE hot.org SET features=NULL WHERE org_id=ANY($1)")
        .bind(vec![root, child_a, child_b])
        .execute(&admin)
        .await?;
    drop(store_a);
    drop(store_b);
    quota_runtime.close().await;

    let stream_a: Uuid = sqlx::query_scalar("SELECT stream_id FROM run WHERE run_id=$1")
        .bind(run_a)
        .fetch_one(&a)
        .await?;
    let queued_task = Uuid::new_v4();
    db::Task::insert(
        &db_a,
        &queued_task,
        &env_a,
        &stream_a,
        &build_a,
        None,
        "test/task",
        None,
        None,
        "code",
        1000,
        Some(&user),
    )
    .await?;
    let reserve = db::task::TaskQuotaReservation {
        compute_units: 0,
        duration_ms: 1000,
    };
    let claim_error =
        db::Task::claim_for_worker_with_quota(&read_set_db, &queued_task, "rls-test", reserve)
            .await
            .unwrap_err();
    assert!(claim_error.to_string().contains("exact environment scope"));
    assert_eq!(db::Task::get(&db_a, &queued_task).await?.task_status_id, 1);
    assert!(
        db::Task::claim_for_worker_with_quota(&db_a, &queued_task, "rls-test", reserve)
            .await
            .map_err(|e| format!("initial code task claim: {e:?}"))?
    );
    assert_eq!(db::Task::get(&db_a, &queued_task).await?.task_status_id, 2);
    // Use independent data connections for the concurrency checks below; the
    // original fixture intentionally has a one-connection pool.
    let counter_runtime = PgPoolOptions::new()
        .max_connections(4)
        .after_connect(|conn, _| {
            Box::pin(async move {
                conn.execute("SET search_path=hot,public").await?;
                Ok(())
            })
        })
        .connect(runtime_url.as_str())
        .await?;
    let counter_pool = ScopedPgPool::new(counter_runtime);
    let a = counter_pool.with_scope(DatabaseScope::Environment {
        org_id: child_a,
        env_id: env_a,
    });
    let b = counter_pool.with_scope(DatabaseScope::Environment {
        org_id: child_b,
        env_id: env_b,
    });
    let db_a = db::DatabasePool::Postgres(a.clone());
    let db_b = db::DatabasePool::Postgres(b.clone());
    // A completion holding the counter row must become visible to a period
    // initialization that waited for it. Source reads use a fresh statement
    // snapshot after acquiring counter locks.
    let stream_b: Uuid = sqlx::query_scalar("SELECT stream_id FROM run WHERE run_id=$1")
        .bind(run_b)
        .fetch_one(&b)
        .await?;
    let reserve = db::task::TaskQuotaReservation {
        compute_units: 1,
        ..reserve
    };
    let period_task = Uuid::new_v4();
    let period_claim = Uuid::new_v4();
    for (id, env, stream, build, connection) in [
        (period_task, env_a, stream_a, build_a, &a),
        (period_claim, env_b, stream_b, build_b, &b),
    ] {
        sqlx::query("INSERT INTO task(task_id,env_id,stream_id,build_id,function_name,task_type) VALUES($1,$2,$3,$4,'period-test','container')")
            .bind(id).bind(env).bind(stream).bind(build).execute(connection).await?;
    }
    sqlx::query("UPDATE hot.org SET features=$1 WHERE org_id=$2")
        .bind(serde_json::json!({"compute_units_budget":1000000,"runs_per_month":-1,"box_concurrent_tasks":-1,"compute_units_per_month":-1}))
        .bind(root)
        .execute(&admin)
        .await?;
    assert!(
        db::Task::claim_for_worker_with_quota(&db_a, &period_task, "period-first", reserve)
            .await
            .map_err(|e| format!("first period task claim: {e:?}"))?
    );
    sqlx::query("UPDATE hot.usage_task SET period_start=now()+interval '10 days',compute_units=0,duration_ms=0 WHERE org_id=ANY($1)").bind(vec![child_a,child_b]).execute(&admin).await?;
    let mut completion = a.begin().await?;
    sqlx::query(
        "UPDATE task SET task_status_id=3,duration_ms=200,stop_time=now() WHERE task_id=$1",
    )
    .bind(period_task)
    .execute(&mut *completion)
    .await?;
    let claim_db = db_b.clone();
    let waiting = tokio::spawn(async move {
        db::Task::claim_for_worker_with_quota(&claim_db, &period_claim, "period-second", reserve)
            .await
    });
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert!(
        !waiting.is_finished(),
        "claim must wait for the uncommitted counter writer"
    );
    completion.commit().await?;
    assert!(
        waiting
            .await?
            .map_err(|e| format!("rollover task claim: {e:?}"))?
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT duration_ms FROM usage_task WHERE env_id=$1")
            .bind(env_a)
            .fetch_one(&a)
            .await?,
        200
    );
    for (metering, expected) in [
        (serde_json::json!({"compute-units":i64::MAX}), i64::MAX),
        (
            serde_json::json!({"compute-units":i64::MAX.to_string()}),
            i64::MAX,
        ),
        (
            serde_json::json!({"$val":{"err":{"compute-units":i64::MAX}}}),
            i64::MAX,
        ),
        (
            serde_json::json!({"compute-units":"9223372036854775808"}),
            20,
        ),
        (
            serde_json::json!({"compute-units":9223372036854775808_u64}),
            20,
        ),
        (serde_json::json!({"compute-units":i64::MIN}), 0),
        (serde_json::json!({"compute-units":true}), 20),
    ] {
        sqlx::query("UPDATE task SET task_type='container',reserved_compute_units=100,result=$2 WHERE task_id=$1")
            .bind(period_task).bind(metering).execute(&a).await?;
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT compute_units FROM usage_task WHERE env_id=$1")
                .bind(env_a)
                .fetch_one(&a)
                .await?,
            expected
        );
    }
    // A rejected claim at period rollover must not discard counter
    // initialization; otherwise every redelivery rescans history under the
    // account lock.
    let rejected_claim = Uuid::new_v4();
    sqlx::query("INSERT INTO task(task_id,env_id,stream_id,build_id,function_name,task_type) VALUES($1,$2,$3,$4,'period-test','container')")
        .bind(rejected_claim).bind(env_b).bind(stream_b).bind(build_b).execute(&b).await?;
    sqlx::query("UPDATE hot.org SET features=$1 WHERE org_id=$2")
        .bind(serde_json::json!({"box_concurrent_tasks":0,"runs_per_month":-1}))
        .bind(root)
        .execute(&admin)
        .await?;
    sqlx::query(
        "UPDATE hot.usage_task SET period_start=now()+interval '10 days' WHERE org_id=ANY($1)",
    )
    .bind(vec![child_a, child_b])
    .execute(&admin)
    .await?;
    let container_reserve = db::task::TaskQuotaReservation {
        compute_units: 10,
        duration_ms: 1000,
    };
    let rejected = db::Task::claim_for_worker_with_quota(
        &db_b,
        &rejected_claim,
        "period-rejected",
        container_reserve,
    )
    .await;
    assert!(
        matches!(rejected, Err(db::task::TaskError::QuotaExhausted(_))),
        "an account with containers disabled refuses the claim for good"
    );
    let initialized: bool = sqlx::query_scalar(
        "SELECT bool_and(period_start <= now()) FROM hot.usage_task WHERE org_id=ANY($1)",
    )
    .bind(vec![child_a, child_b])
    .fetch_one(&admin)
    .await?;
    assert!(
        initialized,
        "period initialization must survive a rejected claim"
    );
    sqlx::query("DELETE FROM hot.task WHERE task_id=$1")
        .bind(rejected_claim)
        .execute(&admin)
        .await?;
    sqlx::query("DELETE FROM hot.task WHERE task_id=ANY($1)")
        .bind(vec![period_task, period_claim])
        .execute(&admin)
        .await?;
    sqlx::query("UPDATE hot.org SET features=NULL WHERE org_id=$1")
        .bind(root)
        .execute(&admin)
        .await?;
    // Enqueues change no usage, so they must not serialize on the
    // environment's counter row.
    let enqueue = |id: Uuid| {
        sqlx::query("INSERT INTO task(task_id,env_id,stream_id,build_id,function_name,task_type) VALUES($1,$2,$3,$4,'enqueue-test','container')")
            .bind(id).bind(env_a).bind(stream_a).bind(build_a)
    };
    let enqueued = [Uuid::new_v4(), Uuid::new_v4()];
    let mut first_enqueue = a.begin().await?;
    enqueue(enqueued[0]).execute(&mut *first_enqueue).await?;
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        enqueue(enqueued[1]).execute(&a),
    )
    .await
    .map_err(|_| "a concurrent enqueue waited on the counter row")??;
    first_enqueue.commit().await?;
    // A limited claim holds the account lock, so it must not wait on another
    // environment's in-flight counter change.
    sqlx::query("UPDATE hot.org SET features=$1 WHERE org_id=$2")
        .bind(serde_json::json!({"box_concurrent_tasks":100,"compute_units_per_month":-1,"runs_per_month":-1}))
        .bind(root)
        .execute(&admin)
        .await?;
    let mut in_flight = b.begin().await?;
    sqlx::query("INSERT INTO task(task_id,env_id,stream_id,build_id,function_name,task_type,task_status_id,start_time,reserved_compute_units,reserved_duration_ms) VALUES($1,$2,$3,$4,'in-flight-test','container',2,now(),10,1000)")
        .bind(Uuid::new_v4()).bind(env_b).bind(stream_b).bind(build_b).execute(&mut *in_flight).await?;
    let claimed = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        db::Task::claim_for_worker_with_quota(&db_a, &enqueued[0], "in-flight", container_reserve),
    )
    .await
    .map_err(|_| "a claim waited on another environment's counter row")?
    .map_err(|e| format!("claim beside an in-flight counter change: {e:?}"))?;
    assert!(claimed);
    in_flight.rollback().await?;
    sqlx::query("DELETE FROM hot.task WHERE task_id=ANY($1)")
        .bind(enqueued.to_vec())
        .execute(&admin)
        .await?;
    sqlx::query("UPDATE hot.org SET features=NULL WHERE org_id=$1")
        .bind(root)
        .execute(&admin)
        .await?;
    // Code task results are user values: account usage neither fails on nor
    // counts their compute-units, matching the counters.
    let usage_since = chrono::Utc::now() - chrono::Duration::days(1);
    let compute_units = || async {
        db::subscription::OrgUsageStats::calculate_account(&db_a, &root, usage_since, -1)
            .await
            .map(|usage| usage.compute_units)
            .map_err(|e| format!("account usage: {e:?}"))
    };
    let before = compute_units().await?;
    let metered = vec![Uuid::new_v4(), Uuid::new_v4()];
    for (id, task_type, result) in [
        (
            metered[0],
            "code",
            serde_json::json!({"compute-units":"n/a"}),
        ),
        (
            metered[1],
            "container",
            serde_json::json!({"compute-units":7}),
        ),
    ] {
        sqlx::query("INSERT INTO task(task_id,env_id,stream_id,build_id,function_name,task_type,task_status_id,result,stop_time) VALUES($1,$2,$3,$4,'usage-test',$5,3,$6,now())")
            .bind(id).bind(env_a).bind(stream_a).bind(build_a).bind(task_type).bind(result).execute(&a).await?;
    }
    assert_eq!(compute_units().await?, before + 7);
    sqlx::query("DELETE FROM hot.task WHERE task_id=ANY($1)")
        .bind(metered)
        .execute(&admin)
        .await?;

    // Multi-statement units of work preserve the exact scope and rollback.
    let tx_ids: (i64, i64) = a
        .transaction(|conn| {
            Box::pin(async move {
                let first = sqlx::query_scalar("SELECT txid_current()")
                    .fetch_one(&mut *conn)
                    .await?;
                let second = sqlx::query_scalar("SELECT txid_current()")
                    .fetch_one(&mut *conn)
                    .await?;
                Ok::<_, sqlx::Error>((first, second))
            })
        })
        .await?;
    assert_eq!(tx_ids.0, tx_ids.1);
    let original: String = sqlx::query_scalar("SELECT name FROM env WHERE env_id=$1")
        .bind(env_a)
        .fetch_one(&a)
        .await?;
    let rejected: Result<(), sqlx::Error> = a
        .transaction(move |conn| {
            Box::pin(async move {
                sqlx::query("UPDATE env SET name='must-rollback' WHERE env_id=$1")
                    .bind(env_a)
                    .execute(&mut *conn)
                    .await?;
                Err(sqlx::Error::Protocol("intentional rollback".into()))
            })
        })
        .await;
    assert!(rejected.is_err());
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT name FROM env WHERE env_id=$1")
            .bind(env_a)
            .fetch_one(&a)
            .await?,
        original
    );
    // Even a one-connection data pool remains usable while coordination owns
    // an advisory session. The guard must not consume the pool's only slot.
    let single = PgPoolOptions::new()
        .max_connections(1)
        .after_connect(|conn, _| {
            Box::pin(async move {
                conn.execute("SET search_path=hot,public").await?;
                Ok(())
            })
        })
        .connect(runtime_url.as_str())
        .await?;
    let single = ScopedPgPool::new(single).with_scope(DatabaseScope::Environment {
        org_id: child_a,
        env_id: env_a,
    });
    let guard = db::try_acquire_advisory_lock(
        &db::DatabasePool::Postgres(single.clone()),
        i64::from_be_bytes(Uuid::new_v4().as_bytes()[..8].try_into()?),
    )
    .await?
    .unwrap();
    assert_eq!(
        tokio::time::timeout(
            Duration::from_secs(3),
            sqlx::query_scalar::<_, i32>("SELECT 1").fetch_one(&single)
        )
        .await??,
        1
    );
    guard.release().await;
    single.close().await;

    // All counter tables retain the same exact ownership boundary as source rows.
    let counter_call = Uuid::new_v4();
    sqlx::query("INSERT INTO call(call_id,run_id,env_id,function_name,static_scope,runtime_path,call_depth,start_time,size) VALUES($1,$2,$3,'counter','counter','counter',0,now(),11)")
        .bind(counter_call).bind(run_a).bind(env_a).execute(&a).await?;
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM usage_storage WHERE env_id=$1")
            .bind(env_a)
            .fetch_one(&b)
            .await?,
        0
    );
    let denied =
        sqlx::query("INSERT INTO usage_storage(env_id,org_id,slot,call_bytes) VALUES($1,$2,0,1)")
            .bind(env_a)
            .bind(child_a)
            .execute(&b)
            .await
            .unwrap_err();
    assert_eq!(
        denied.as_database_error().and_then(|e| e.code()).as_deref(),
        Some("42501")
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM usage_task WHERE env_id=$1")
            .bind(env_a)
            .fetch_one(&b)
            .await?,
        0
    );
    let denied = sqlx::query("INSERT INTO usage_task(env_id,org_id) VALUES($1,$2) ON CONFLICT(env_id) DO UPDATE SET compute_units=usage_task.compute_units+1")
            .bind(env_a)
            .bind(child_a)
            .execute(&b)
            .await
            .unwrap_err();
    assert_eq!(
        denied.as_database_error().and_then(|e| e.code()).as_deref(),
        Some("42501")
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM usage_run")
            .fetch_one(&b)
            .await?,
        0
    );
    // A NOT NULL violation would also be an error; only a policy denial (42501)
    // shows that the counter itself is protected.
    let denied = sqlx::query("INSERT INTO usage_run(root_org_id,period_start,admitted) VALUES($1,now(),1) ON CONFLICT(root_org_id,period_start) DO UPDATE SET admitted=usage_run.admitted+1")
        .bind(root)
        .execute(&b)
        .await
        .unwrap_err();
    assert_eq!(
        denied.as_database_error().and_then(|e| e.code()).as_deref(),
        Some("42501")
    );
    // Ownership guards skip updates that keep ownership, so trace and counter
    // writes do not pay for them, but still reject reassignment.
    let unguarded: Vec<String> = sqlx::query_scalar(
        "SELECT tgrelid::regclass::text FROM pg_trigger WHERE tgname='immutable_resource_scope' AND tgqual IS NULL ORDER BY 1",
    )
    .fetch_all(&admin)
    .await?;
    assert!(unguarded.is_empty(), "unguarded: {unguarded:?}");
    sqlx::query("UPDATE hot.call SET env_id=env_id,run_id=run_id WHERE call_id=$1")
        .bind(counter_call)
        .execute(&admin)
        .await?;
    let moved = sqlx::query("UPDATE hot.usage_storage SET org_id=$1 WHERE env_id=$2")
        .bind(child_b)
        .bind(env_a)
        .execute(&admin)
        .await
        .unwrap_err();
    assert!(
        moved
            .to_string()
            .contains("Resource ownership is immutable"),
        "{moved}"
    );
    let queue = pool.with_scope(DatabaseScope::Maintenance {
        operation: db::MaintenanceOperation::Queue,
        org_id: None,
    });
    assert!(sqlx::query("INSERT INTO call(call_id,run_id,env_id,function_name,static_scope,runtime_path,call_depth,start_time,size) VALUES($1,$2,$3,'counter','counter','counter',0,now(),11)")
        .bind(Uuid::new_v4()).bind(run_a).bind(env_b).execute(&queue).await.is_err(),"broad queue scope still cannot forge call ownership");
    let billing_db = db::DatabasePool::Postgres(pool.with_scope(DatabaseScope::Maintenance {
        operation: db::MaintenanceOperation::Billing,
        org_id: Some(root),
    }));
    // Reconciliation checks only accounts with an enforced limit.
    sqlx::query(
        "UPDATE hot.org SET features='{\"runs_per_month\":1000000}'::jsonb WHERE org_id=$1",
    )
    .bind(root)
    .execute(&admin)
    .await?;
    db::usage::reconcile_account(&billing_db, root).await?;
    sqlx::query("UPDATE hot.usage_storage SET call_bytes=call_bytes+29 WHERE env_id=$1 AND slot=(SELECT min(slot) FROM hot.usage_storage WHERE env_id=$1)").bind(env_a).execute(&admin).await?;
    let writer = a.clone();
    let writing = tokio::spawn(async move {
        for size in 12..32 {
            sqlx::query("UPDATE call SET size=$1 WHERE call_id=$2")
                .bind(size)
                .bind(counter_call)
                .execute(&writer)
                .await
                .unwrap();
        }
    });
    let report = db::usage::reconcile_account(&billing_db, root).await?;
    assert_eq!(report.storage_rows, 1);
    writing.await?;
    assert!(
        db::usage::reconcile_account(&billing_db, root)
            .await?
            .is_empty()
    );
    let actual: i64 = sqlx::query_scalar(
        "SELECT COALESCE(SUM(call_bytes),0)::bigint FROM usage_storage WHERE env_id=$1",
    )
    .bind(env_a)
    .fetch_one(&a)
    .await?;
    let expected: i64 = sqlx::query_scalar(
        "SELECT COALESCE(SUM(GREATEST(size,0)),0)::bigint FROM call WHERE env_id=$1",
    )
    .bind(env_a)
    .fetch_one(&a)
    .await?;
    assert_eq!(actual, expected);
    // Each counter is checked and repaired in its own short transaction: a
    // writer holding one environment's counter delays only that repair, not
    // the account's other environments, and no snapshot spans them.
    let (first, last) = if env_a < env_b {
        (env_a, env_b)
    } else {
        (env_b, env_a)
    };
    for (env, org) in [(env_a, child_a), (env_b, child_b)] {
        sqlx::query("INSERT INTO hot.usage_storage(env_id,org_id,slot,store_bytes) VALUES($1,$2,0,7) ON CONFLICT(env_id,slot) DO UPDATE SET store_bytes=hot.usage_storage.store_bytes+7")
            .bind(env).bind(org).execute(&admin).await?;
    }
    let mut holder = admin.begin().await?;
    sqlx::query("SELECT 1 FROM hot.usage_storage WHERE env_id=$1 AND slot=0 FOR UPDATE")
        .bind(last)
        .execute(&mut *holder)
        .await?;
    let reconciling = tokio::spawn({
        let billing_db = billing_db.clone();
        async move { db::usage::reconcile_account(&billing_db, root).await }
    });
    let store_matches = |env: Uuid| {
        sqlx::query_scalar::<_, bool>("SELECT (SELECT COALESCE(SUM(store_bytes),0) FROM hot.usage_storage WHERE env_id=$1)=(SELECT COALESCE(SUM(GREATEST(size,0)),0) FROM hot.store_map_entry WHERE env_id=$1)")
            .bind(env)
            .fetch_one(&admin)
    };
    let mut first_repaired = false;
    for _ in 0..100 {
        if store_matches(first).await? {
            first_repaired = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(
        first_repaired && !reconciling.is_finished(),
        "an earlier environment's repair must commit while a later one waits"
    );
    holder.rollback().await?;
    assert_eq!(reconciling.await??.storage_rows, 2);
    assert!(store_matches(last).await?);
    // Reconciliation needs no dedicated coordination connection, so a
    // one-connection pool keeps its session.
    let one_connection = ScopedPgPool::new(
        PgPoolOptions::new()
            .max_connections(1)
            .after_connect(|conn, _| {
                Box::pin(async move {
                    conn.execute("SET search_path=hot,public").await?;
                    Ok(())
                })
            })
            .connect(runtime_url.as_str())
            .await?,
    )
    .with_scope(DatabaseScope::Maintenance {
        operation: db::MaintenanceOperation::Billing,
        org_id: Some(root),
    });
    let backend = || async {
        let mut tx = one_connection.begin().await?;
        let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok::<_, sqlx::Error>(pid)
    };
    let session = backend().await?;
    db::usage::reconcile_account(&db::DatabasePool::Postgres(one_connection.clone()), root).await?;
    assert_eq!(backend().await?, session);
    one_connection.close().await;

    // Distinct runtime sessions publish deltas to their own backend slots.
    // Retention and a cascaded run delete both subtract exactly once.
    let cascade_run = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO run(run_id,env_id,stream_id,build_id,run_type_id) VALUES($1,$2,$3,$4,1)",
    )
    .bind(cascade_run)
    .bind(env_a)
    .bind(stream_a)
    .bind(build_a)
    .execute(&a)
    .await?;
    let prior_slots: Vec<(i16, i64)> =
        sqlx::query_as("SELECT slot,call_bytes FROM usage_storage WHERE env_id=$1")
            .bind(env_a)
            .fetch_all(&a)
            .await?;
    let mut first = a.begin().await?;
    let mut second = a.begin().await?;
    let slot1: i32 = sqlx::query_scalar("SELECT pg_backend_pid()%16")
        .fetch_one(&mut *first)
        .await?;
    let slot2: i32 = sqlx::query_scalar("SELECT pg_backend_pid()%16")
        .fetch_one(&mut *second)
        .await?;
    sqlx::query("INSERT INTO call(call_id,run_id,env_id,function_name,static_scope,runtime_path,call_depth,start_time,size) VALUES($1,$2,$3,'slots','slots','slots',0,'2000-01-01',17)")
        .bind(Uuid::new_v4()).bind(cascade_run).bind(env_a).execute(&mut *first).await?;
    first.commit().await?;
    sqlx::query("INSERT INTO call(call_id,run_id,env_id,function_name,static_scope,runtime_path,call_depth,start_time,size) VALUES($1,$2,$3,'slots','slots','slots',0,'2000-01-01',23)")
        .bind(Uuid::new_v4()).bind(cascade_run).bind(env_a).execute(&mut *second).await?;
    second.commit().await?;
    let mut expected_slots: std::collections::BTreeMap<i16, i64> =
        prior_slots.into_iter().collect();
    *expected_slots.entry(slot1 as i16).or_default() += 17;
    *expected_slots.entry(slot2 as i16).or_default() += 23;
    let current_slots: Vec<(i16, i64)> =
        sqlx::query_as("SELECT slot,call_bytes FROM usage_storage WHERE env_id=$1")
            .bind(env_a)
            .fetch_all(&a)
            .await?;
    assert_eq!(
        current_slots
            .into_iter()
            .collect::<std::collections::BTreeMap<_, _>>(),
        expected_slots
    );
    let retention = db::DatabasePool::Postgres(pool.with_scope(DatabaseScope::Maintenance {
        operation: db::MaintenanceOperation::Retention,
        org_id: None,
    }));
    assert_eq!(
        db::Call::delete_older_than_for_org(&retention, &[env_a], 3650, 100).await?,
        2
    );
    sqlx::query("INSERT INTO call(call_id,run_id,env_id,function_name,static_scope,runtime_path,call_depth,start_time,size) VALUES($1,$2,$3,'cascade','cascade','cascade',0,now(),29)")
        .bind(Uuid::new_v4()).bind(cascade_run).bind(env_a).execute(&a).await?;
    let db::DatabasePool::Postgres(retention_pg) = &retention else {
        unreachable!()
    };
    sqlx::query("DELETE FROM run WHERE run_id=$1")
        .bind(cascade_run)
        .execute(retention_pg)
        .await?;
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT COALESCE(SUM(call_bytes),0)::bigint FROM usage_storage WHERE env_id=$1"
        )
        .bind(env_a)
        .fetch_one(&a)
        .await?,
        expected
    );
    sqlx::query("DELETE FROM call WHERE call_id=$1")
        .bind(counter_call)
        .execute(&a)
        .await?;
    sqlx::query("DELETE FROM hot.task WHERE task_id=$1")
        .bind(queued_task)
        .execute(&admin)
        .await?;
    sqlx::query("DELETE FROM hot.run_admission WHERE root_org_id=$1")
        .bind(root)
        .execute(&admin)
        .await?;
    assert!(sqlx::query("INSERT INTO run(run_id,env_id,stream_id,build_id,run_type_id,origin_run_id,by_user_id) VALUES($1,$2,$3,$4,4,$5,$6)")
        .bind(Uuid::new_v4()).bind(env_a).bind(stream_a).bind(build_a).bind(run_b).bind(user).execute(&a).await.is_err());
    sqlx::query("DELETE FROM hot.run WHERE run_id=ANY($1)")
        .bind(vec![run_a, run_b])
        .execute(&admin)
        .await?;

    sqlx::query("DELETE FROM hot.build WHERE build_id=ANY($1)")
        .bind(vec![build_a, build_b])
        .execute(&admin)
        .await?;
    sqlx::query("DELETE FROM hot.project WHERE project_id=ANY($1)")
        .bind(vec![project_a, project_b])
        .execute(&admin)
        .await?;

    // Remove only this test's unique fixture records and role, never schemas.
    counter_pool.close().await;
    pool.close().await;
    sqlx::query("DELETE FROM hot.store_map_entry WHERE org_id = ANY($1)")
        .bind(vec![child_a, child_b])
        .execute(&admin)
        .await?;
    sqlx::query("DELETE FROM hot.stream WHERE env_id = ANY($1)")
        .bind(vec![env_a, env_b])
        .execute(&admin)
        .await?;
    sqlx::query("DELETE FROM hot.file WHERE org_id=ANY($1)")
        .bind(vec![child_a, child_b])
        .execute(&admin)
        .await?;
    for table in ["usage_storage", "usage_task", "usage_file"] {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "DELETE FROM hot.{table} WHERE org_id=ANY($1)"
        )))
        .bind(vec![child_a, child_b])
        .execute(&admin)
        .await?;
    }
    sqlx::query("DELETE FROM hot.usage_run WHERE root_org_id=$1")
        .bind(root)
        .execute(&admin)
        .await?;
    sqlx::query("DELETE FROM hot.env WHERE env_id = ANY($1)")
        .bind(vec![env_a, env_b])
        .execute(&admin)
        .await?;
    sqlx::query("DELETE FROM hot.org WHERE org_id = ANY($1)")
        .bind(vec![child_a, child_b])
        .execute(&admin)
        .await?;
    sqlx::query("DELETE FROM hot.org WHERE org_id=$1")
        .bind(root)
        .execute(&admin)
        .await?;
    sqlx::query("DELETE FROM hot.\"user\" WHERE user_id=ANY($1)")
        .bind(vec![user, invitee])
        .execute(&admin)
        .await?;
    admin
        .execute(sqlx::AssertSqlSafe(format!("DROP OWNED BY {role}")))
        .await?;
    admin
        .execute(sqlx::AssertSqlSafe(format!("DROP ROLE {role}")))
        .await?;
    admin.close().await;
    Ok(())
}

//! Real-router smoke coverage under the production PostgreSQL role contract.
//! HOT_TEST_POSTGRES_URI must identify a dedicated privileged fixture database.
use axum::http::StatusCode;
use hot::db::alert::{AlertChannel, AlertDestination, AlertSubscription, DestinationType};
use hot::db::{self, DatabasePool, DatabaseScope};
use hot_app::test_support::TestClient;
use sqlx::{Executor, PgPool};
use uuid::Uuid;

struct Workspace {
    org: Uuid,
    env: Uuid,
    project: Uuid,
    build: Uuid,
    stream: Uuid,
    run: Uuid,
    api_key: Uuid,
    marker: String,
}

impl Workspace {
    fn new(label: &str, suffix: &str) -> Self {
        Self {
            org: Uuid::new_v4(),
            env: Uuid::new_v4(),
            project: Uuid::new_v4(),
            build: Uuid::new_v4(),
            stream: Uuid::new_v4(),
            run: Uuid::new_v4(),
            api_key: Uuid::new_v4(),
            marker: format!("{label}-{suffix}"),
        }
    }

    async fn seed(&self, admin: &PgPool, root: Uuid, user: Uuid) -> Result<(), sqlx::Error> {
        sqlx::query("INSERT INTO hot.org(org_id,parent_org_id,name,slug,org_type,created_by_user_id) VALUES($1,$2,$3,$3,'organization',$4)")
            .bind(self.org).bind(root).bind(&self.marker).bind(user).execute(admin).await?;
        sqlx::query(
            "INSERT INTO hot.env(env_id,org_id,name,created_by_user_id) VALUES($1,$2,$3,$4)",
        )
        .bind(self.env)
        .bind(self.org)
        .bind(format!("{}-env", self.marker))
        .bind(user)
        .execute(admin)
        .await?;
        sqlx::query("INSERT INTO hot.project(project_id,env_id,name,created_by_user_id) VALUES($1,$2,$3,$4)")
            .bind(self.project).bind(self.env).bind(format!("{}-project", self.marker)).bind(user).execute(admin).await?;
        sqlx::query("INSERT INTO hot.build(build_id,project_id,hash,size,created_by_user_id) VALUES($1,$2,'fixture',0,$3)")
            .bind(self.build).bind(self.project).bind(user).execute(admin).await?;
        sqlx::query("INSERT INTO hot.stream(stream_id,env_id,name) VALUES($1,$2,$3)")
            .bind(self.stream)
            .bind(self.env)
            .bind(format!("{}-stream", self.marker))
            .execute(admin)
            .await?;
        sqlx::query("INSERT INTO hot.run(run_id,env_id,stream_id,build_id,run_type_id,by_user_id) VALUES($1,$2,$3,$4,4,$5)")
            .bind(self.run).bind(self.env).bind(self.stream).bind(self.build).bind(user).execute(admin).await?;
        sqlx::query("INSERT INTO hot.store_map(name,org_id,env_id) VALUES($1,$2,$3)")
            .bind(format!("{}-store", self.marker))
            .bind(self.org)
            .bind(self.env)
            .execute(admin)
            .await?;
        sqlx::query("INSERT INTO hot.store_map_entry(org_id,env_id,store_name,key,value) VALUES($1,$2,$3,'\"fixture\"','\"private\"')")
            .bind(self.org).bind(self.env).bind(format!("{}-store", self.marker)).execute(admin).await?;
        sqlx::query("INSERT INTO hot.file(path,size,storage_backend,storage_path,org_id,env_id,created_by_user_id) VALUES($1,10,'local','fixture-only',$2,$3,$4)")
            .bind(format!("{}-file.txt", self.marker)).bind(self.org).bind(self.env).bind(user).execute(admin).await?;
        sqlx::query("INSERT INTO hot.context(env_id,key,value,created_by_user_id) VALUES($1,$2,'fixture-only',$3)")
            .bind(self.env).bind(format!("{}-context", self.marker)).bind(user).execute(admin).await?;
        sqlx::query("INSERT INTO hot.api_key(api_key_id,env_id,description,key_data,created_by_user_id) VALUES($1,$2,$3,'{}',$4)")
            .bind(self.api_key).bind(self.env).bind(format!("{}-api-key", self.marker)).bind(user).execute(admin).await?;
        sqlx::query("INSERT INTO hot.service_key(api_key_id,env_id,name,secret_hash,permissions) VALUES($1,$2,$3,$4,'{}')")
            .bind(self.api_key).bind(self.env).bind(format!("{}-service-key", self.marker)).bind(vec![1_u8;32]).execute(admin).await?;
        Ok(())
    }

    fn scope(&self) -> DatabaseScope {
        DatabaseScope::Environment {
            org_id: self.org,
            env_id: self.env,
        }
    }
}

#[tokio::test]
async fn root_admin_pages_render_child_data_under_a_restricted_login()
-> Result<(), Box<dyn std::error::Error>> {
    let Ok(admin_uri) = std::env::var("HOT_TEST_POSTGRES_URI") else {
        eprintln!("skipping PostgreSQL app scope smoke: HOT_TEST_POSTGRES_URI not set");
        return Ok(());
    };
    db::run_migrations(&hot::val!({"uri":admin_uri.clone(),"rls":{"enforce":false}})).await?;
    let admin = PgPool::connect(&admin_uri).await?;
    let suffix = Uuid::new_v4().simple().to_string();
    let role = format!("hot_app_rls_{suffix}");
    let password = Uuid::new_v4().simple().to_string();
    admin.execute(sqlx::AssertSqlSafe(format!("CREATE ROLE {role} LOGIN PASSWORD '{password}' NOSUPERUSER NOBYPASSRLS NOINHERIT NOCREATEDB NOCREATEROLE"))).await?;
    admin
        .execute(sqlx::AssertSqlSafe(format!(
            "GRANT USAGE ON SCHEMA hot TO {role}"
        )))
        .await?;
    // Exactly the table categories in provision_runtime.sql; no migration grants.
    let grants: Vec<(String, bool)> = sqlx::query_as(
        "SELECT format('%I.%I',n.nspname,c.relname),c.relrowsecurity AND c.relforcerowsecurity FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname='hot' AND c.relkind IN ('r','p') AND ((c.relrowsecurity AND c.relforcerowsecurity) OR c.relname IN ('build_type','email_verification_status','invite_status','org_user_role','run_status','run_type','task_status','team_user_role','org_plan_status','alert_delivery_status','alert_destination_type','plan'))"
    ).fetch_all(&admin).await?;
    for (table, scoped) in grants {
        let privileges = if scoped {
            "SELECT, INSERT, UPDATE, DELETE"
        } else {
            "SELECT"
        };
        admin
            .execute(sqlx::AssertSqlSafe(format!(
                "GRANT {privileges} ON {table} TO {role}"
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
    // Production pool creation performs role qualification with enforcement on.
    let runtime =
        db::create_db_pool(&hot::val!({"uri":runtime_url.to_string(),"schema":"hot"})).await?;

    let user = Uuid::new_v4();
    let root = Uuid::new_v4();
    let root_slug = format!("root-{suffix}");
    sqlx::query("INSERT INTO hot.\"user\"(user_id,email,name,created_by_user_id) VALUES($1,$2,'Root administrator',$1)")
        .bind(user).bind(format!("{suffix}@app-rls.example.test")).execute(&admin).await?;
    sqlx::query("INSERT INTO hot.org(org_id,name,slug,org_type,features,created_by_user_id) VALUES($1,'HTTP test account',$2,'organization','{\"alerts\":true,\"service_keys\":true}',$3)")
        .bind(root).bind(&root_slug).bind(user).execute(&admin).await?;
    sqlx::query("INSERT INTO hot.org_user(org_user_id,org_id,user_id,org_user_role_id,created_by_user_id) VALUES($1,$2,$3,2,$3)")
        .bind(Uuid::new_v4()).bind(root).bind(user).execute(&admin).await?;
    let a = Workspace::new("alpha", &suffix);
    let b = Workspace::new("beta", &suffix);
    a.seed(&admin, root, user).await?;
    b.seed(&admin, root, user).await?;
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM hot.org_user WHERE org_id=ANY($1)")
            .bind(vec![a.org, b.org])
            .fetch_one(&admin)
            .await?,
        0,
        "access must be inherited from root admin"
    );

    // These control-plane writes also exercise the env-specific alert policies
    // through the restricted login instead of relying only on admin fixtures.
    let mut alerts = Vec::new();
    for workspace in [&a, &b] {
        let org_db = runtime.with_scope(DatabaseScope::Organization {
            org_id: workspace.org,
        });
        let env_db = runtime.with_scope(workspace.scope());
        let destination = AlertDestination::create(
            &org_db,
            &workspace.org,
            &format!("{}-destination", workspace.marker),
            DestinationType::Email,
            &serde_json::json!({"email":"fixture@example.test"}),
            &user,
        )
        .await?;
        let channel = AlertChannel::create(
            &env_db,
            &workspace.org,
            Some(&workspace.env),
            &format!("{}-channel", workspace.marker),
            ".*",
            &user,
        )
        .await?;
        let subscription = AlertSubscription::create(
            &env_db,
            &workspace.org,
            Some(&workspace.env),
            &[channel.alert_channel_id],
            &[destination.alert_destination_id],
            &user,
        )
        .await?;
        alerts.push((destination, channel, subscription));
    }
    let a_db = runtime.with_scope(a.scope());
    assert!(
        AlertSubscription::create(
            &a_db,
            &a.org,
            Some(&a.env),
            &[alerts[1].1.alert_channel_id],
            &[alerts[0].0.alert_destination_id],
            &user
        )
        .await
        .is_err()
    );
    assert!(
        AlertSubscription::create(
            &a_db,
            &a.org,
            Some(&a.env),
            &[alerts[0].1.alert_channel_id],
            &[alerts[1].0.alert_destination_id],
            &user
        )
        .await
        .is_err()
    );

    let mut client = TestClient::new_with_db(
        runtime.clone(),
        hot::val!({"app":{"host":"localhost","port":4680},"db":{"uri":runtime_url.to_string(),"schema":"hot"},"store":{"type":"postgres"}}),
    )
    .await;
    client.login_as(&user);
    for (selected, other) in [(&a, &b), (&b, &a)] {
        let cases = [
            (
                "/dashboard",
                format!("{}-project", selected.marker),
                format!("{}-project", other.marker),
            ),
            (
                "/stores",
                format!("{}-store", selected.marker),
                format!("{}-store", other.marker),
            ),
            ("/runs", selected.run.to_string(), other.run.to_string()),
            (
                "/files",
                format!("{}-file.txt", selected.marker),
                format!("{}-file.txt", other.marker),
            ),
            (
                "/contexts",
                format!("{}-context", selected.marker),
                format!("{}-context", other.marker),
            ),
            (
                "/projects",
                format!("{}-project", selected.marker),
                format!("{}-project", other.marker),
            ),
            (
                "/keys",
                format!("{}-api-key", selected.marker),
                format!("{}-api-key", other.marker),
            ),
            (
                "/service-keys",
                format!("{}-service-key", selected.marker),
                format!("{}-service-key", other.marker),
            ),
            (
                "/envs",
                format!("{}-env", selected.marker),
                format!("{}-env", other.marker),
            ),
            (
                "/settings/alerts/channels",
                format!("{}-channel", selected.marker),
                format!("{}-channel", other.marker),
            ),
            (
                "/settings/alerts/destinations",
                format!("{}-destination", selected.marker),
                format!("{}-destination", other.marker),
            ),
            (
                "/settings/alerts/subscriptions",
                format!("{}-channel", selected.marker),
                format!("{}-channel", other.marker),
            ),
        ];
        for (path, expected, forbidden) in cases {
            let url = format!("{path}?org_id={}&env_id={}", selected.org, selected.env);
            let response = client.get(&url).await;
            assert_eq!(
                response.status,
                StatusCode::OK,
                "{url}: {}",
                response.body.chars().take(500).collect::<String>()
            );
            assert!(
                response.body.contains(&expected),
                "{url} silently omitted {expected}: {}",
                response.body
            );
            assert!(
                !response.body.contains(&forbidden),
                "{url} leaked sibling fixture {forbidden}"
            );
        }
    }
    client
        .get(&format!("/stores?org_id={}&env_id={}", a.org, b.env))
        .await
        .assert_status(StatusCode::NOT_FOUND);
    let overview = client.get(&format!("/@{root_slug}/children")).await;
    overview.assert_status(StatusCode::OK);
    assert!(
        overview
            .body
            .contains(&format!("data-overview-org=\"{}\"", a.org))
    );
    assert!(
        overview
            .body
            .contains(&format!("data-overview-org=\"{}\"", b.org))
    );

    // Remove only this test's fixtures; never reset the shared test database.
    let org_ids = vec![a.org, b.org];
    let env_ids = vec![a.env, b.env];
    sqlx::query("DELETE FROM hot.alert_subscription_channel WHERE subscription_id IN (SELECT alert_subscription_id FROM hot.alert_subscription WHERE org_id=ANY($1))").bind(&org_ids).execute(&admin).await?;
    sqlx::query("DELETE FROM hot.alert_subscription_destination WHERE subscription_id IN (SELECT alert_subscription_id FROM hot.alert_subscription WHERE org_id=ANY($1))").bind(&org_ids).execute(&admin).await?;
    sqlx::query("DELETE FROM hot.alert_subscription WHERE org_id=ANY($1)")
        .bind(&org_ids)
        .execute(&admin)
        .await?;
    sqlx::query("DELETE FROM hot.alert_channel WHERE org_id=ANY($1)")
        .bind(&org_ids)
        .execute(&admin)
        .await?;
    sqlx::query("DELETE FROM hot.alert_destination WHERE org_id=ANY($1)")
        .bind(&org_ids)
        .execute(&admin)
        .await?;
    sqlx::query("DELETE FROM hot.file WHERE org_id=ANY($1)")
        .bind(&org_ids)
        .execute(&admin)
        .await?;
    sqlx::query("DELETE FROM hot.context WHERE env_id=ANY($1)")
        .bind(&env_ids)
        .execute(&admin)
        .await?;
    sqlx::query("DELETE FROM hot.service_key WHERE env_id=ANY($1)")
        .bind(&env_ids)
        .execute(&admin)
        .await?;
    sqlx::query("DELETE FROM hot.api_key WHERE env_id=ANY($1)")
        .bind(&env_ids)
        .execute(&admin)
        .await?;
    sqlx::query("DELETE FROM hot.run WHERE env_id=ANY($1)")
        .bind(&env_ids)
        .execute(&admin)
        .await?;
    sqlx::query("DELETE FROM hot.stream WHERE env_id=ANY($1)")
        .bind(&env_ids)
        .execute(&admin)
        .await?;
    sqlx::query("DELETE FROM hot.build WHERE project_id=ANY($1)")
        .bind(vec![a.project, b.project])
        .execute(&admin)
        .await?;
    sqlx::query("DELETE FROM hot.project WHERE env_id=ANY($1)")
        .bind(&env_ids)
        .execute(&admin)
        .await?;
    sqlx::query("DELETE FROM hot.store_map_entry WHERE org_id=ANY($1)")
        .bind(&org_ids)
        .execute(&admin)
        .await?;
    sqlx::query("DELETE FROM hot.store_map WHERE org_id=ANY($1)")
        .bind(&org_ids)
        .execute(&admin)
        .await?;
    sqlx::query("DELETE FROM hot.env WHERE env_id=ANY($1)")
        .bind(&env_ids)
        .execute(&admin)
        .await?;
    sqlx::query("DELETE FROM hot.org WHERE org_id=ANY($1)")
        .bind(&org_ids)
        .execute(&admin)
        .await?;
    sqlx::query("DELETE FROM hot.org_user WHERE org_id=$1")
        .bind(root)
        .execute(&admin)
        .await?;
    sqlx::query("DELETE FROM hot.org WHERE org_id=$1")
        .bind(root)
        .execute(&admin)
        .await?;
    sqlx::query("DELETE FROM hot.\"user\" WHERE user_id=$1")
        .bind(user)
        .execute(&admin)
        .await?;
    if let DatabasePool::Postgres(pool) = &runtime {
        pool.close().await;
    }
    admin
        .execute(sqlx::AssertSqlSafe(format!("DROP OWNED BY {role}")))
        .await?;
    admin
        .execute(sqlx::AssertSqlSafe(format!("DROP ROLE {role}")))
        .await?;
    admin.close().await;
    Ok(())
}

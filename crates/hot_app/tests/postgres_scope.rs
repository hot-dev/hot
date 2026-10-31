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
    fn new(label: &str, suffix: &str, org: Uuid, env: Uuid) -> Self {
        Self {
            org,
            env,
            project: Uuid::new_v4(),
            build: Uuid::new_v4(),
            stream: Uuid::new_v4(),
            run: Uuid::new_v4(),
            api_key: Uuid::new_v4(),
            marker: format!("{label}-{suffix}"),
        }
    }

    async fn seed(&self, admin: &PgPool, user: Uuid) -> Result<(), sqlx::Error> {
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
async fn sub_org_lifecycle_and_browser_scope_use_a_restricted_runtime_login()
-> Result<(), Box<dyn std::error::Error>> {
    let Ok(admin_uri) = std::env::var("HOT_TEST_POSTGRES_URI") else {
        eprintln!("skipping PostgreSQL app scope smoke: HOT_TEST_POSTGRES_URI not set");
        return Ok(());
    };
    let suffix = Uuid::new_v4().simple().to_string();
    let database = format!("hot_app_lifecycle_{suffix}");
    let cluster = PgPool::connect(&admin_uri).await?;
    cluster
        .execute(sqlx::AssertSqlSafe(format!("CREATE DATABASE {database}")))
        .await?;
    let mut fixture_url = url::Url::parse(&admin_uri)?;
    fixture_url.set_path(&format!("/{database}"));
    let admin_uri = fixture_url.to_string();
    db::run_migrations(&hot::val!({"uri":admin_uri.clone(),"rls":{"enforce":false}})).await?;
    let admin = PgPool::connect(&admin_uri).await?;
    let role = format!("hot_app_rls_{suffix}");
    let password = Uuid::new_v4().simple().to_string();
    admin.execute(sqlx::AssertSqlSafe(format!("CREATE ROLE {role} LOGIN PASSWORD '{password}' NOSUPERUSER NOBYPASSRLS NOINHERIT NOCREATEDB NOCREATEROLE"))).await?;
    let admin_password = fixture_url.password().unwrap_or_default().to_owned();
    fixture_url
        .set_password(None)
        .map_err(|_| "invalid fixture URL")?;
    let provision = tokio::process::Command::new("psql")
        .args([
            "-X",
            "-v",
            "ON_ERROR_STOP=1",
            "-v",
            &format!("runtime_role={role}"),
            "-v",
            "schema=hot",
            "-f",
        ])
        .arg(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../resources/db/postgres/provision_runtime.sql"),
        )
        .arg("--dbname")
        .arg(fixture_url.as_str())
        .env("PGPASSWORD", admin_password)
        .output()
        .await?;
    assert!(
        provision.status.success(),
        "{}",
        String::from_utf8_lossy(&provision.stderr)
    );
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
    let root_slug = format!("root-{}", &suffix[..16]);
    sqlx::query("INSERT INTO hot.\"user\"(user_id,email,name,created_by_user_id) VALUES($1,$2,'Root administrator',$1)")
        .bind(user).bind(format!("{suffix}@app-rls.example.test")).execute(&admin).await?;
    sqlx::query("INSERT INTO hot.org(org_id,name,slug,org_type,features,created_by_user_id) VALUES($1,'HTTP test account',$2,'organization','{\"alerts\":true,\"service_keys\":true,\"sub_orgs\":4}',$3)")
        .bind(root).bind(&root_slug).bind(user).execute(&admin).await?;
    sqlx::query("INSERT INTO hot.org_user(org_user_id,org_id,user_id,org_user_role_id,created_by_user_id) VALUES($1,$2,$3,2,$3)")
        .bind(Uuid::new_v4()).bind(root).bind(user).execute(&admin).await?;
    let mut client = TestClient::new_with_db(
        runtime.clone(),
        hot::val!({"app":{"host":"localhost","port":4680},"db":{"uri":runtime_url.to_string(),"schema":"hot"},"store":{"type":"postgres"}}),
    ).await;
    client.login_as(&user);
    let children_path = format!("/@{root_slug}/children");
    let mut workspaces = Vec::new();
    for label in ["alpha", "beta"] {
        let slug = format!("{label}-{}", &suffix[..16]);
        let form = [
            ("name", slug.as_str()),
            ("slug", slug.as_str()),
            ("idempotency_key", slug.as_str()),
        ];
        for _ in 0..2 {
            client
                .post_form(&children_path, &form)
                .await
                .assert_status(StatusCode::SEE_OTHER);
        }
        let org: Uuid = sqlx::query_scalar("SELECT org_id FROM hot.org WHERE slug=$1")
            .bind(&slug)
            .fetch_one(&admin)
            .await?;
        let env: Uuid = sqlx::query_scalar("SELECT env_id FROM hot.env WHERE org_id=$1")
            .bind(org)
            .fetch_one(&admin)
            .await?;
        let workspace = Workspace::new(label, &suffix[..16], org, env);
        client
            .post_form(
                &format!("/envs/{env}/edit?org_id={org}&env_id={env}"),
                &[("name", &format!("{}-env", workspace.marker))],
            )
            .await
            .assert_status(StatusCode::SEE_OTHER);
        workspace.seed(&admin, user).await?;
        workspaces.push(workspace);
    }
    let [a, b]: [Workspace; 2] = workspaces.try_into().ok().unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM hot.org WHERE parent_org_id=$1")
            .bind(root)
            .fetch_one(&admin)
            .await?,
        2,
        "idempotent child provisioning must not duplicate children"
    );
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
    // Each child's one seeded run is still running and started this month.
    for workspace in [&a, &b] {
        let marker = format!("data-overview-org=\"{}\"", workspace.org);
        let row = overview
            .body
            .split(&marker)
            .nth(1)
            .and_then(|row| row.split("</tr>").next())
            .unwrap_or_default();
        assert!(
            row.contains(r#"<td class="p-2">active</td><td class="p-2">1</td><td class="p-2">0</td><td class="p-2">1</td>"#),
            "{row}"
        );
    }

    let scope_a = format!("org_id={}&env_id={}", a.org, a.env);
    let scope_b = format!("org_id={}&env_id={}", b.org, b.env);
    let page_a = format!("/stores?{scope_a}");
    let page_b = format!("/stores?{scope_b}");

    // Generate in A, switch the browser to B, and submit the old form. The
    // pending secret must not migrate to B or be consumed by the wrong scope.
    let pending = client.get(&format!("/keys/new?{scope_a}")).await;
    pending.assert_status(StatusCode::OK);
    let key_id = pending
        .body
        .split("name=\"api_key_id\" value=\"")
        .nth(1)
        .unwrap()
        .split('"')
        .next()
        .unwrap()
        .to_string();
    let handoff_token = pending
        .body
        .split("name=\"handoff_token\" value=\"")
        .nth(1)
        .unwrap()
        .split('"')
        .next()
        .unwrap()
        .to_owned();
    let key_form = [
        ("api_key_id", key_id.as_str()),
        ("handoff_token", handoff_token.as_str()),
        ("description", "lifecycle-ui-key"),
        ("access_level", "full"),
    ];
    client
        .post_form(&format!("/switch-org/{}", b.org), &[])
        .await
        .assert_status(StatusCode::SEE_OTHER);
    let wrong_scope = client
        .post_form(&format!("/keys/new?{scope_b}"), &key_form)
        .await;
    assert!(wrong_scope.body.contains("Session expired"));
    let created = client
        .post_form(&format!("/keys/new?{scope_a}"), &key_form)
        .await;
    created.assert_status(StatusCode::SEE_OTHER);
    let key_redirect = created.location().unwrap().to_owned();
    assert_eq!(key_redirect, format!("/keys?{scope_a}"));
    let key_env: Uuid = sqlx::query_scalar("SELECT env_id FROM hot.api_key WHERE api_key_id=$1")
        .bind(Uuid::parse_str(&key_id)?)
        .fetch_one(&admin)
        .await?;
    assert_eq!(key_env, a.env);
    // Simulate another tab switching after the mutation response but before
    // following its redirect. The redirect itself must carry A's authority.
    client
        .post_form(&format!("/switch-org/{}", b.org), &[])
        .await
        .assert_status(StatusCode::SEE_OTHER);
    let returned = client.get(&key_redirect).await;
    returned.assert_status(StatusCode::OK);
    assert!(returned.body.contains("lifecycle-ui-key"));
    assert!(!returned.body.contains(&format!("{}-api-key", b.marker)));
    client.get(&page_a).await.assert_status(StatusCode::OK);
    client
        .get(&format!("/stores?org_id={}&env_id={}", a.org, b.env))
        .await
        .assert_status(StatusCode::NOT_FOUND);
    client
        .get_with_headers(&page_a, &[("X-Hot-Org-Id", &b.org.to_string())])
        .await
        .assert_status(StatusCode::BAD_REQUEST);
    client
        .post_form("/keys/new", &key_form)
        .await
        .assert_status(StatusCode::BAD_REQUEST);

    let issued = client
        .post_form(
            &format!("/service-keys/new?{scope_a}"),
            &[
                ("name", "lifecycle-service-key"),
                ("permissions", r#"{"webhook:*":["*"]}"#),
            ],
        )
        .await;
    issued.assert_status(StatusCode::OK);
    assert!(
        issued.body.contains("Service Key Created"),
        "{}",
        issued.body
    );
    let (service_key, parent_key, service_env): (Uuid,Uuid,Uuid) = sqlx::query_as("SELECT service_key_id,api_key_id,env_id FROM hot.service_key WHERE name='lifecycle-service-key'").fetch_one(&admin).await?;
    assert_eq!(
        (parent_key, service_env),
        (Uuid::parse_str(&key_id)?, a.env)
    );
    client
        .get(&format!("/service-keys/{service_key}?{scope_b}"))
        .await
        .assert_status(StatusCode::NOT_FOUND);
    client
        .post_form(
            &format!("/service-keys/{service_key}/revoke?{scope_b}"),
            &[],
        )
        .await
        .assert_status(StatusCode::NOT_FOUND);
    assert!(
        sqlx::query_scalar::<_, bool>(
            "SELECT revoked_at IS NULL FROM hot.service_key WHERE service_key_id=$1"
        )
        .bind(service_key)
        .fetch_one(&admin)
        .await?
    );
    let revoked = client
        .post_form(
            &format!("/service-keys/{service_key}/revoke?{scope_a}"),
            &[],
        )
        .await;
    revoked.assert_status(StatusCode::SEE_OTHER);
    let service_redirect = revoked.location().unwrap().to_owned();
    assert_eq!(service_redirect, format!("/service-keys?{scope_a}"));
    client
        .post_form(&format!("/switch-org/{}", b.org), &[])
        .await
        .assert_status(StatusCode::SEE_OTHER);
    let returned = client.get(&service_redirect).await;
    returned.assert_status(StatusCode::OK);
    assert!(
        returned
            .body
            .contains(&format!("const orgId = \"{}\"", a.org))
    );
    assert!(
        !sqlx::query_scalar::<_, bool>(
            "SELECT revoked_at IS NULL FROM hot.service_key WHERE service_key_id=$1"
        )
        .bind(service_key)
        .fetch_one(&admin)
        .await?
    );
    let edit_form = [
        ("description", "lifecycle-revoked-api-key"),
        ("active", "false"),
        ("access_level", "full"),
    ];
    client
        .post_form(&format!("/keys/{key_id}/edit?{scope_b}"), &edit_form)
        .await
        .assert_status(StatusCode::SEE_OTHER);
    assert!(
        sqlx::query_scalar::<_, bool>("SELECT active FROM hot.api_key WHERE api_key_id=$1")
            .bind(Uuid::parse_str(&key_id)?)
            .fetch_one(&admin)
            .await?
    );
    let edited = client
        .post_form(&format!("/keys/{key_id}/edit?{scope_a}"), &edit_form)
        .await;
    edited.assert_status(StatusCode::SEE_OTHER);
    assert_eq!(edited.location(), Some(format!("/keys?{scope_a}").as_str()));
    assert!(
        !sqlx::query_scalar::<_, bool>("SELECT active FROM hot.api_key WHERE api_key_id=$1")
            .bind(Uuid::parse_str(&key_id)?)
            .fetch_one(&admin)
            .await?
    );

    let lifecycle = format!("/@{root_slug}/children/{}/lifecycle", a.org);
    for state in ["suspended", "archived"] {
        client
            .post_form(&lifecycle, &[("state", state)])
            .await
            .assert_status(StatusCode::SEE_OTHER);
        client
            .get(&page_a)
            .await
            .assert_status(StatusCode::FORBIDDEN);
        client.get(&page_b).await.assert_status(StatusCode::OK);
        client
            .get(&children_path)
            .await
            .assert_status(StatusCode::OK);
    }
    client
        .post_form(&lifecycle, &[("state", "active")])
        .await
        .assert_status(StatusCode::BAD_REQUEST);
    for state in ["suspended", "active"] {
        client
            .post_form(&lifecycle, &[("state", state)])
            .await
            .assert_status(StatusCode::SEE_OTHER);
    }
    client.get(&page_a).await.assert_status(StatusCode::OK);
    let root_lifecycle = format!("/@{root_slug}/lifecycle");
    client
        .post_form(&root_lifecycle, &[("state", "suspended")])
        .await
        .assert_status(StatusCode::SEE_OTHER);
    client
        .get(&page_a)
        .await
        .assert_status(StatusCode::FORBIDDEN);
    client
        .get(&page_b)
        .await
        .assert_status(StatusCode::FORBIDDEN);
    client
        .get(&children_path)
        .await
        .assert_status(StatusCode::OK);
    client
        .post_form(&root_lifecycle, &[("state", "active")])
        .await
        .assert_status(StatusCode::SEE_OTHER);
    client.get(&page_a).await.assert_status(StatusCode::OK);

    let member = Uuid::new_v4();
    sqlx::query("INSERT INTO hot.\"user\"(user_id,email,name,created_by_user_id) VALUES($1,$2,'Child member',$3)").bind(member).bind(format!("member-{suffix}@example.test")).bind(user).execute(&admin).await?;
    sqlx::query("INSERT INTO hot.org_user(org_user_id,org_id,user_id,org_user_role_id,created_by_user_id) VALUES($1,$2,$3,1,$4)").bind(Uuid::new_v4()).bind(root).bind(member).bind(user).execute(&admin).await?;
    client.login_as(&member);
    client
        .get(&page_a)
        .await
        .assert_status(StatusCode::NOT_FOUND);
    client
        .get(&children_path)
        .await
        .assert_status(StatusCode::FORBIDDEN);
    client.login_as(&user);
    let grant_path = format!("/@{root_slug}/children/{}/access", a.org);
    client
        .post_form(
            &grant_path,
            &[("user_id", &member.to_string()), ("role_id", "1")],
        )
        .await
        .assert_status(StatusCode::SEE_OTHER);
    client.login_as(&member);
    client.get(&page_a).await.assert_status(StatusCode::OK);
    client
        .get(&page_b)
        .await
        .assert_status(StatusCode::NOT_FOUND);
    client
        .get(&format!("/service-keys?{scope_a}"))
        .await
        .assert_status(StatusCode::FORBIDDEN);
    client
        .post_form(&lifecycle, &[("state", "suspended")])
        .await
        .assert_status(StatusCode::FORBIDDEN);
    client.login_as(&user);
    client
        .post_form(
            &grant_path,
            &[("user_id", &member.to_string()), ("role_id", "0")],
        )
        .await
        .assert_status(StatusCode::SEE_OTHER);
    client.login_as(&member);
    client
        .get(&page_a)
        .await
        .assert_status(StatusCode::NOT_FOUND);

    if let DatabasePool::Postgres(pool) = &runtime {
        pool.close().await;
    }
    admin.close().await;
    cluster
        .execute(sqlx::AssertSqlSafe(format!(
            "DROP DATABASE {database} WITH (FORCE)"
        )))
        .await?;
    cluster
        .execute(sqlx::AssertSqlSafe(format!("DROP ROLE {role}")))
        .await?;
    cluster.close().await;
    Ok(())
}

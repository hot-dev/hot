//! Browser-level scope, hierarchy, and recovery checks through the real router.
use axum::http::StatusCode;
use hot::db::org::CreateChild;
use hot::db::{DatabasePool, Env, Org, RunStatus, insert_test_data, org::OrgUser, user::User};
use hot_app::test_support::TestClient;
use uuid::Uuid;

fn sqlite(client: &TestClient) -> &sqlx::SqlitePool {
    let DatabasePool::Sqlite(pool) = client.db().as_ref() else {
        unreachable!()
    };
    pool
}

#[tokio::test]
async fn child_workspaces_have_explicit_scope_and_root_admin_recovery() {
    let mut client =
        TestClient::new_with_conf(hot::val!({"app": {"host": "localhost", "port": 4680}})).await;
    let td = insert_test_data(client.db()).await.unwrap();
    let DatabasePool::Sqlite(pool) = client.db().as_ref() else {
        unreachable!()
    };
    sqlx::query("UPDATE org SET org_type='organization' WHERE org_id=?")
        .bind(td.org_id)
        .execute(pool)
        .await
        .unwrap();
    let root = Org::get_org_uncached(client.db(), &td.org_id)
        .await
        .unwrap();
    client.login_as(&td.user_id);
    let path = format!("/@{}/children", root.slug);
    client.get(&path).await.assert_status(StatusCode::OK);
    for slug in ["customer-alpha", "customer-beta"] {
        client
            .post_form(
                &path,
                &[("name", slug), ("slug", slug), ("idempotency_key", slug)],
            )
            .await
            .assert_status(StatusCode::SEE_OTHER);
    }
    let children = Org::list_children(client.db(), &td.org_id).await.unwrap();
    assert_eq!(children.len(), 2);
    let a = children
        .iter()
        .find(|child| child.slug == "customer-alpha")
        .unwrap();
    let b = children
        .iter()
        .find(|child| child.slug == "customer-beta")
        .unwrap();
    let env_a = hot::db::Env::get_envs_by_org(client.db(), &a.org_id)
        .await
        .unwrap()[0]
        .env_id;
    let env_b = hot::db::Env::get_envs_by_org(client.db(), &b.org_id)
        .await
        .unwrap()[0]
        .env_id;
    assert_ne!(env_a, env_b);
    let overview = client.get(&path).await;
    overview.assert_status(StatusCode::OK);
    assert!(overview.body.contains("Account overview"));
    for child in [a, b] {
        assert!(
            overview
                .body
                .contains(&format!("data-overview-org=\"{}\"", child.org_id))
        );
    }
    assert!(overview.body.contains(&format!("env_id={env_a}")));
    assert!(overview.body.contains(&format!("env_id={env_b}")));

    // Page A remains authoritative after the navigation cookie switches to B.
    let page_a = format!("/stores?org_id={}&env_id={env_a}", a.org_id);
    client.get(&page_a).await.assert_status(StatusCode::OK);
    client
        .post_form(&format!("/switch-org/{}", b.org_id), &[])
        .await
        .assert_status(StatusCode::SEE_OTHER);
    let stale_tab = client.get(&page_a).await;
    stale_tab.assert_status(StatusCode::OK);
    assert!(
        stale_tab
            .body
            .contains(&format!("const orgId = \"{}\"", a.org_id))
    );
    assert!(
        stale_tab
            .body
            .contains(&format!("const envId = \"{env_a}\""))
    );
    client
        .get(&format!("/stores?org_id={}&env_id={env_b}", a.org_id))
        .await
        .assert_status(StatusCode::NOT_FOUND);
    client
        .post_form("/stores/settings/entries/delete", &[])
        .await
        .assert_status(StatusCode::BAD_REQUEST);
    client
        .get(&format!("/stores?org_id={}&org_id={}", a.org_id, b.org_id))
        .await
        .assert_status(StatusCode::BAD_REQUEST);

    let lifecycle = format!("/@{}/children/{}/lifecycle", root.slug, a.org_id);
    client
        .post_form(&lifecycle, &[("state", "suspended")])
        .await
        .assert_status(StatusCode::SEE_OTHER);
    // A held workspace explains itself instead of answering with an empty 403.
    let held = client.get(&page_a).await;
    held.assert_status(StatusCode::FORBIDDEN);
    assert!(held.body.contains("Workspace unavailable"), "{}", held.body);
    assert!(held.body.contains("This workspace is suspended."));
    let widget = client
        .get_with_headers(&page_a, &[("HX-Request", "true")])
        .await;
    widget.assert_status(StatusCode::FORBIDDEN);
    assert!(widget.body.starts_with("<div class=\"error-box\""));
    client.get(&path).await.assert_status(StatusCode::OK);
    client
        .post_form(&lifecycle, &[("state", "active")])
        .await
        .assert_status(StatusCode::SEE_OTHER);
    client.get(&page_a).await.assert_status(StatusCode::OK);

    // Root membership alone cannot discover children; an explicit grant opens
    // just one child and never grants account administration.
    let member = Uuid::now_v7();
    User::insert_user(
        client.db(),
        &member,
        "child-member@example.test",
        Some("Child member"),
        Some(&td.user_id),
    )
    .await
    .unwrap();
    OrgUser::insert_org_user(
        client.db(),
        &Uuid::now_v7(),
        &td.org_id,
        &member,
        Some(1),
        &td.user_id,
    )
    .await
    .unwrap();
    client
        .post_form(
            &format!("/@{}/children/{}/access", root.slug, a.org_id),
            &[("user_id", &member.to_string()), ("role_id", "1")],
        )
        .await
        .assert_status(StatusCode::SEE_OTHER);
    client.login_as(&member);
    client.get(&page_a).await.assert_status(StatusCode::OK);
    client
        .get(&format!("/stores?org_id={}&env_id={env_b}", b.org_id))
        .await
        .assert_status(StatusCode::NOT_FOUND);
    client.get(&path).await.assert_status(StatusCode::FORBIDDEN);

    client.login_as(&td.user_id);
    let root_lifecycle = format!("/@{}/lifecycle", root.slug);
    client
        .post_form(&root_lifecycle, &[("state", "suspended")])
        .await
        .assert_status(StatusCode::SEE_OTHER);
    let held = client.get(&page_a).await;
    held.assert_status(StatusCode::FORBIDDEN);
    assert!(held.body.contains("is suspended."), "{}", held.body);
    // The recovery page stays open and says why work is held.
    let recovery = client.get(&path).await;
    recovery.assert_status(StatusCode::OK);
    assert!(recovery.body.contains("Manage account status"));
    assert!(recovery.body.contains("data-workspace-held=\"true\""));
    client
        .post_form(&root_lifecycle, &[("state", "active")])
        .await
        .assert_status(StatusCode::SEE_OTHER);
    client.get(&page_a).await.assert_status(StatusCode::OK);
}

#[tokio::test]
async fn an_explicit_org_selector_shows_that_orgs_timezone() {
    let mut client = TestClient::new().await;
    let td = insert_test_data(client.db()).await.unwrap();
    let tokyo = Uuid::now_v7();
    Org::insert_org(
        client.db(),
        &tokyo,
        "Tokyo",
        "tokyo",
        "organization",
        &td.user_id,
    )
    .await
    .unwrap();
    OrgUser::insert_org_user(
        client.db(),
        &Uuid::now_v7(),
        &tokyo,
        &td.user_id,
        Some(2),
        &td.user_id,
    )
    .await
    .unwrap();
    let tokyo_env = Uuid::now_v7();
    Env::insert_env(client.db(), &tokyo_env, &tokyo, "production", &td.user_id)
        .await
        .unwrap();
    sqlx::query(
        r#"UPDATE org SET settings='{"display":{"timezone":"Asia/Tokyo"}}' WHERE org_id=?"#,
    )
    .bind(tokyo)
    .execute(sqlite(&client))
    .await
    .unwrap();
    let created = Env::get_env(client.db(), &tokyo_env)
        .await
        .unwrap()
        .created_at;
    let in_tokyo = format!(
        "{} JST",
        hot_app::timezone::format_in_timezone(&created, "Asia/Tokyo", "%Y-%m-%d %H:%M:%S")
    );
    client.login_as(&td.user_id);

    // The session defaults to the first org, which has no timezone.
    let home = client.get(&format!("/envs?org_id={}", td.org_id)).await;
    home.assert_status(StatusCode::OK);
    assert!(!home.body.contains("JST"));
    let page = client.get(&format!("/envs?org_id={tokyo}")).await;
    page.assert_status(StatusCode::OK);
    assert!(page.body.contains(&in_tokyo), "{}", page.body);
}

#[tokio::test]
async fn only_a_root_admin_sees_usage_labelled_as_the_account() {
    let mut client = TestClient::new().await;
    let td = insert_test_data(client.db()).await.unwrap();
    sqlx::query("UPDATE org SET org_type='organization' WHERE org_id=?")
        .bind(td.org_id)
        .execute(sqlite(&client))
        .await
        .unwrap();
    let root = Org::get_org_uncached(client.db(), &td.org_id)
        .await
        .unwrap();
    let child = Org::create_child(
        client.db(),
        &td.org_id,
        &td.user_id,
        &CreateChild {
            name: "Customer".into(),
            slug: "usage-customer".into(),
            external_id: None,
            idempotency_key: "usage-customer".into(),
            hosted: false,
        },
    )
    .await
    .unwrap()
    .org;
    let member = Uuid::now_v7();
    User::insert_user(
        client.db(),
        &member,
        "usage-member@example.test",
        Some("Usage member"),
        Some(&td.user_id),
    )
    .await
    .unwrap();
    OrgUser::insert_org_user(
        client.db(),
        &Uuid::now_v7(),
        &td.org_id,
        &member,
        Some(1),
        &td.user_id,
    )
    .await
    .unwrap();
    let account = "Account usage and limits";
    let workspace = "This workspace's usage. Limits are shared with the account.";
    let root_usage = format!("/@{}/usage", root.slug);

    client.login_as(&td.user_id);
    let page = client.get(&root_usage).await;
    page.assert_status(StatusCode::OK);
    assert!(page.body.contains(account));
    let page = client.get(&format!("/@{}/usage", child.slug)).await;
    page.assert_status(StatusCode::OK);
    assert!(page.body.contains(workspace));
    assert!(!page.body.contains(account));

    // A plain member's stats cover only the root org itself.
    client.login_as(&member);
    let page = client.get(&root_usage).await;
    page.assert_status(StatusCode::OK);
    assert!(page.body.contains(workspace));
    assert!(!page.body.contains(account));
}

#[tokio::test]
async fn environment_names_and_account_limits_report_why_nothing_changed() {
    let mut client =
        TestClient::new_with_conf(hot::val!({"product":{"experience":"self-host"}})).await;
    let td = insert_test_data(client.db()).await.unwrap();
    sqlx::query("UPDATE org SET org_type='organization' WHERE org_id=?")
        .bind(td.org_id)
        .execute(sqlite(&client))
        .await
        .unwrap();
    let root = Org::get_org_uncached(client.db(), &td.org_id)
        .await
        .unwrap();
    client.login_as(&td.user_id);
    let create = format!("/envs/new?org_id={}", td.org_id);

    // The trimmed name is what gets provisioned, and provisioning an existing
    // name returns that environment, so a padded duplicate must be refused
    // instead of reported as created.
    let response = client.post_form(&create, &[("name", "development ")]).await;
    response.assert_status(StatusCode::OK);
    assert!(
        response
            .body
            .contains("Environment with this name already exists"),
        "{}",
        response.body
    );
    client
        .post_form(
            &format!("/envs/{}/edit?org_id={}", td.env_id, td.org_id),
            &[("name", "staging ")],
        )
        .await
        .assert_status(StatusCode::SEE_OTHER);
    assert_eq!(
        Env::get_env(client.db(), &td.env_id).await.unwrap().name,
        "staging"
    );

    sqlx::query(r#"UPDATE org SET features='{"environments":1,"team_members":1}' WHERE org_id=?"#)
        .bind(td.org_id)
        .execute(sqlite(&client))
        .await
        .unwrap();
    let response = client.post_form(&create, &[("name", "production")]).await;
    assert!(
        response.body.contains("Account environment limit reached"),
        "{}",
        response.body
    );
    assert_eq!(
        Env::get_envs_by_org(client.db(), &td.org_id)
            .await
            .unwrap()
            .len(),
        1
    );

    let member = Uuid::now_v7();
    User::insert_user(
        client.db(),
        &member,
        "seat-member@example.test",
        Some("Seat member"),
        Some(&td.user_id),
    )
    .await
    .unwrap();
    OrgUser::insert_org_user(
        client.db(),
        &Uuid::now_v7(),
        &td.org_id,
        &member,
        Some(1),
        &td.user_id,
    )
    .await
    .unwrap();
    sqlx::query("UPDATE org_user SET active=0 WHERE org_id=? AND user_id=?")
        .bind(td.org_id)
        .bind(member)
        .execute(sqlite(&client))
        .await
        .unwrap();
    let response = client
        .post_form(
            &format!("/@{}/users/{member}/edit", root.slug),
            &[("role_id", "1"), ("active", "true")],
        )
        .await;
    assert!(
        response.body.contains("Account team member limit reached"),
        "{}",
        response.body
    );
    assert!(
        !OrgUser::get_org_user(client.db(), &td.org_id, &member)
            .await
            .unwrap()
            .active
    );
}

/// Deploy a new project in `env_id` with `count` active schedules.
async fn deploy_schedules(client: &TestClient, env_id: &Uuid, user_id: &Uuid, count: usize) {
    let project_id = Uuid::now_v7();
    hot::db::Project::insert_project(
        client.db(),
        &project_id,
        env_id,
        &format!("scheduled-{project_id}"),
        user_id,
    )
    .await
    .unwrap();
    let build_id = Uuid::now_v7();
    hot::db::Build::insert_build(
        client.db(),
        &build_id,
        &project_id,
        "scheduled-build",
        1,
        hot::db::Build::BUILD_TYPE_BUNDLE,
        user_id,
    )
    .await
    .unwrap();
    let cron = "0 */5 * * * *";
    let mut functions = hot::lang::compiler::ScheduledFunctions::new();
    functions.insert(
        cron.to_string(),
        (0..count)
            .map(|idx| {
                let fn_name = format!("::demo/task-{idx}");
                hot::lang::compiler::ScheduledFunction {
                    cron_expression: cron.to_string(),
                    scheduled_function: hot::val!({
                        "fn": fn_name,
                        "meta": {},
                        "file": null,
                        "line": null,
                        "column": null,
                        "position": null
                    }),
                }
            })
            .collect(),
    );
    hot::db::Schedule::insert_schedules_for_build(
        client.db(),
        &build_id,
        &functions,
        &hot::lang::compiler::SendTargets::new(),
    )
    .await
    .unwrap();
    hot::db::Build::deploy_build(client.db(), &build_id, user_id)
        .await
        .unwrap();
}

#[tokio::test]
async fn the_schedule_count_covers_the_whole_account() {
    let mut client = TestClient::new().await;
    let td = insert_test_data(client.db()).await.unwrap();
    sqlx::query("UPDATE org SET org_type='organization' WHERE org_id=?")
        .bind(td.org_id)
        .execute(sqlite(&client))
        .await
        .unwrap();
    let child = Org::create_child(
        client.db(),
        &td.org_id,
        &td.user_id,
        &CreateChild {
            name: "Customer".into(),
            slug: "scheduled-customer".into(),
            external_id: None,
            idempotency_key: "scheduled-customer".into(),
            hosted: false,
        },
    )
    .await
    .unwrap();
    deploy_schedules(&client, &td.env_id, &td.user_id, 1).await;
    deploy_schedules(&client, &child.env_id, &td.user_id, 2).await;
    client.login_as(&td.user_id);

    for (org_id, env_id) in [(td.org_id, td.env_id), (child.org.org_id, child.env_id)] {
        let page = client
            .get(&format!("/schedules?org_id={org_id}&env_id={env_id}"))
            .await;
        page.assert_status(StatusCode::OK);
        assert!(
            page.body.contains("3 active schedules across this org"),
            "{}",
            page.body
        );
    }
}

/// Record a run in `env_id` that started at `start` and ended with `status`
/// at `stop`.
async fn record_finished_run(
    client: &TestClient,
    env_id: &Uuid,
    build_id: &Uuid,
    user_id: &Uuid,
    start: chrono::DateTime<chrono::Utc>,
    status: RunStatus,
    stop: chrono::DateTime<chrono::Utc>,
) {
    let run_id = Uuid::now_v7();
    hot::db::Run::insert_run(
        client.db(),
        &run_id,
        env_id,
        &Uuid::now_v7(),
        Some(build_id),
        hot::db::run::RunType::Run.as_id(),
        None,
        user_id,
        Some(start),
        None,
    )
    .await
    .unwrap();
    sqlx::query("UPDATE run SET status_id=?, stop_time=? WHERE run_id=?")
        .bind(status.as_id())
        .bind(stop)
        .bind(run_id)
        .execute(sqlite(client))
        .await
        .unwrap();
}

#[tokio::test]
async fn the_account_overview_counts_each_environments_runs() {
    let mut client = TestClient::new().await;
    let td = insert_test_data(client.db()).await.unwrap();
    sqlx::query("UPDATE org SET org_type='organization' WHERE org_id=?")
        .bind(td.org_id)
        .execute(sqlite(&client))
        .await
        .unwrap();
    let root = Org::get_org_uncached(client.db(), &td.org_id)
        .await
        .unwrap();
    let child = Org::create_child(
        client.db(),
        &td.org_id,
        &td.user_id,
        &CreateChild {
            name: "Customer".into(),
            slug: "overview-customer".into(),
            external_id: None,
            idempotency_key: "overview-customer".into(),
            hosted: false,
        },
    )
    .await
    .unwrap();
    let child_project = Uuid::now_v7();
    hot::db::Project::insert_project(
        client.db(),
        &child_project,
        &child.env_id,
        "child-project",
        &td.user_id,
    )
    .await
    .unwrap();
    let child_build = Uuid::now_v7();
    hot::db::Build::insert_build(
        client.db(),
        &child_build,
        &child_project,
        "child-build",
        0,
        hot::db::Build::BUILD_TYPE_LIVE,
        &td.user_id,
    )
    .await
    .unwrap();
    let now = chrono::Utc::now();
    let long_ago = now - chrono::Duration::days(40);
    // The fixture's own run is still running. Beside it in the root: a failure
    // outside every window, a long run that failed just now, and a success
    // this month. The child has only an old failure.
    let (root_env, root_build) = (td.env_id, td.build_id);
    for (env, build, start, status, stop) in [
        (root_env, root_build, long_ago, RunStatus::Failed, long_ago),
        (root_env, root_build, long_ago, RunStatus::Failed, now),
        (root_env, root_build, now, RunStatus::Succeeded, now),
        (
            child.env_id,
            child_build,
            long_ago,
            RunStatus::Failed,
            long_ago,
        ),
    ] {
        record_finished_run(&client, &env, &build, &td.user_id, start, status, stop).await;
    }
    client.login_as(&td.user_id);

    let page = client.get(&format!("/@{}/children", root.slug)).await;
    page.assert_status(StatusCode::OK);
    let row = |org_id: Uuid| {
        let marker = format!("data-overview-org=\"{org_id}\"");
        let rest = page.body.split(&marker).nth(1).expect("overview row");
        rest.split("</tr>").next().unwrap().to_owned()
    };
    // Status badge, then running, failed in 24h and runs this month.
    let cells = |values: [&str; 3]| {
        let counts: String = values
            .iter()
            .map(|value| format!("<td class=\"p-2\">{value}</td>"))
            .collect();
        format!("<td class=\"p-2\"><span class=\"badge badge-success\">active</span></td>{counts}")
    };
    let root_row = row(td.org_id);
    assert!(root_row.contains(&cells(["1", "1", "2"])), "{root_row}");
    let child_row = row(child.org.org_id);
    assert!(child_row.contains(&cells(["0", "0", "0"])), "{child_row}");
    assert!(child_row.contains(&long_ago.format("%Y-%m-%d %H:%M UTC").to_string()));
}

#[tokio::test]
async fn child_org_tabs_leave_out_billing() {
    let mut client = TestClient::new().await;
    let td = insert_test_data(client.db()).await.unwrap();
    sqlx::query("UPDATE org SET org_type='organization' WHERE org_id=?")
        .bind(td.org_id)
        .execute(sqlite(&client))
        .await
        .unwrap();
    let root = Org::get_org_uncached(client.db(), &td.org_id)
        .await
        .unwrap();
    let child = Org::create_child(
        client.db(),
        &td.org_id,
        &td.user_id,
        &CreateChild {
            name: "Customer".into(),
            slug: "billing-customer".into(),
            external_id: None,
            idempotency_key: "billing-customer".into(),
            hosted: false,
        },
    )
    .await
    .unwrap()
    .org;
    client.login_as(&td.user_id);

    let page = client.get(&format!("/@{}/usage", root.slug)).await;
    page.assert_status(StatusCode::OK);
    assert!(
        page.body
            .contains(&format!("href=\"/@{}/billing\"", root.slug))
    );
    // Billing belongs to the root, so the child's billing page refuses everyone.
    let child_billing = format!("/@{}/billing", child.slug);
    client
        .get(&child_billing)
        .await
        .assert_status(StatusCode::FORBIDDEN);
    let page = client.get(&format!("/@{}/usage", child.slug)).await;
    page.assert_status(StatusCode::OK);
    assert!(!page.body.contains(&child_billing), "{}", page.body);
}

#[tokio::test]
async fn a_stale_tab_switches_environments_within_its_own_org() {
    let mut client = TestClient::new().await;
    let td = insert_test_data(client.db()).await.unwrap();
    let staging = Uuid::now_v7();
    Env::insert_env(client.db(), &staging, &td.org_id, "staging", &td.user_id)
        .await
        .unwrap();
    let other = Uuid::now_v7();
    Org::insert_org(
        client.db(),
        &other,
        "Other",
        "other-org",
        "organization",
        &td.user_id,
    )
    .await
    .unwrap();
    OrgUser::insert_org_user(
        client.db(),
        &Uuid::now_v7(),
        &other,
        &td.user_id,
        Some(2),
        &td.user_id,
    )
    .await
    .unwrap();
    Env::insert_env(
        client.db(),
        &Uuid::now_v7(),
        &other,
        "production",
        &td.user_id,
    )
    .await
    .unwrap();
    client.login_as(&td.user_id);
    // This tab renders the first org; another tab then moves the navigation
    // cookies to the other org.
    client
        .get(&format!("/envs?org_id={}", td.org_id))
        .await
        .assert_status(StatusCode::OK);
    client
        .post_form(&format!("/switch-org/{other}"), &[])
        .await
        .assert_status(StatusCode::SEE_OTHER);

    // The scope script sends the tab's org with an environment switch;
    // without it the target is looked up in the cookie's org.
    let switch = format!("/envs/{staging}/switch");
    let response = client.post_form(&switch, &[]).await;
    assert!(
        response
            .body
            .contains("You don't have access to this environment"),
        "{}",
        response.body
    );
    client
        .post_form(&format!("{switch}?org_id={}", td.org_id), &[])
        .await
        .assert_redirect_to(&format!("/dashboard?org_id={}&env_id={staging}", td.org_id));
}

#[tokio::test]
async fn org_keys_show_their_issuer_chain_status() {
    use hot::db::org_api_key::{CreateOrgApiKey, OrgApiKey};
    let mut client = TestClient::new().await;
    let td = insert_test_data(client.db()).await.unwrap();
    sqlx::query("UPDATE org SET org_type='organization' WHERE org_id=?")
        .bind(td.org_id)
        .execute(sqlite(&client))
        .await
        .unwrap();
    let root = Org::get_org_uncached(client.db(), &td.org_id)
        .await
        .unwrap();
    let (issuer, _) = OrgApiKey::create_for_user(
        client.db(),
        td.org_id,
        td.user_id,
        &CreateOrgApiKey {
            description: "issuer".into(),
            permissions: serde_json::json!({"org:*": ["read"]}),
            expires_at: None,
        },
    )
    .await
    .unwrap();
    // A key the issuer minted, as the org API records one.
    let minted = Uuid::now_v7();
    sqlx::query("INSERT INTO org_api_key(org_api_key_id,org_id,description,key_data,permissions,issuer_org_api_key_id,created_by_user_id) SELECT ?,org_id,'minted',key_data,permissions,org_api_key_id,created_by_user_id FROM org_api_key WHERE org_api_key_id=?")
        .bind(minted)
        .bind(issuer.org_api_key_id)
        .execute(sqlite(&client))
        .await
        .unwrap();
    OrgApiKey::revoke_for_user(client.db(), td.org_id, td.user_id, issuer.org_api_key_id)
        .await
        .unwrap();
    client.login_as(&td.user_id);

    let page = client.get(&format!("/@{}/children", root.slug)).await;
    page.assert_status(StatusCode::OK);
    assert!(page.body.contains(">Revoked<"), "{}", page.body);
    // The minted key's own row is active, but it no longer authenticates.
    assert!(page.body.contains(">Issuer inactive<"), "{}", page.body);
    assert!(
        !page.body.contains(&format!("org-api-keys/{minted}/revoke")),
        "a dead key offers no revoke: {}",
        page.body
    );
}

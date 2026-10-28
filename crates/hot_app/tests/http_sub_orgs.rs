//! Browser-level scope, hierarchy, and recovery checks through the real router.
use axum::http::StatusCode;
use hot::db::org::CreateChild;
use hot::db::{DatabasePool, Env, Org, insert_test_data, org::OrgUser, user::User};
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
            .contains(&format!("const orgId = '{}'", a.org_id))
    );
    assert!(stale_tab.body.contains(&format!("const envId = '{env_a}'")));
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
    client
        .get(&page_a)
        .await
        .assert_status(StatusCode::FORBIDDEN);
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
    client
        .get(&page_a)
        .await
        .assert_status(StatusCode::FORBIDDEN);
    client.get(&path).await.assert_status(StatusCode::OK);
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

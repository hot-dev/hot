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
    let new = format!("{path}/new");
    client.get(&new).await.assert_status(StatusCode::OK);
    for slug in ["customer-alpha", "customer-beta"] {
        let created = client
            .post_form(
                &new,
                &[("name", slug), ("slug", slug), ("idempotency_key", slug)],
            )
            .await;
        // A new sub-organization opens on its own page.
        created.assert_status(StatusCode::SEE_OTHER);
        assert!(created.location().unwrap().starts_with(&format!("{path}/")));
    }
    // A taken handle comes back to the form with what was entered.
    let taken = client
        .post_form(
            &new,
            &[
                ("name", "Again"),
                ("slug", "customer-alpha"),
                ("idempotency_key", "again"),
            ],
        )
        .await;
    taken.assert_status(StatusCode::BAD_REQUEST);
    assert!(taken.body.contains("already in use"), "{}", taken.body);
    assert!(taken.body.contains("value=\"Again\""), "{}", taken.body);
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
    assert!(overview.body.contains("Change account status"));
    for child in [a, b] {
        assert!(
            overview
                .body
                .contains(&format!("data-overview-org=\"{}\"", child.org_id))
        );
    }
    // Each sub-organization's page links its own environments, and no other's.
    for (child, own, other) in [(a, env_a, env_b), (b, env_b, env_a)] {
        let page = client.get(&format!("{path}/{}", child.org_id)).await;
        page.assert_status(StatusCode::OK);
        assert!(page.body.contains(&format!("env_id={own}")));
        assert!(!page.body.contains(&format!("env_id={other}")));
        for form in ["lifecycle", "access"] {
            client
                .get(&format!("{path}/{}/{form}", child.org_id))
                .await
                .assert_status(StatusCode::OK);
        }
    }
    // An organization that isn't this account's sub-organization has no page here.
    for tail in ["", "/lifecycle", "/access"] {
        client
            .get(&format!("{path}/{}{tail}", td.org_id))
            .await
            .assert_status(StatusCode::NOT_FOUND);
    }

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
    // An archived sub-organization can't go straight back to active: the
    // status page says so and keeps the choice.
    client
        .post_form(&lifecycle, &[("state", "archived")])
        .await
        .assert_status(StatusCode::SEE_OTHER);
    let refused = client.post_form(&lifecycle, &[("state", "active")]).await;
    refused.assert_status(StatusCode::BAD_REQUEST);
    assert!(refused.body.contains("error-box"), "{}", refused.body);
    assert!(
        refused
            .body
            .contains("id=\"state-active\" name=\"state\" type=\"radio\" value=\"active\" checked"),
        "{}",
        refused.body
    );
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
    let access = format!("/@{}/children/{}/access", root.slug, a.org_id);
    client
        .post_form(
            &access,
            &[("user_id", &member.to_string()), ("role_id", "9")],
        )
        .await
        .assert_status(StatusCode::BAD_REQUEST);
    client
        .post_form(
            &access,
            &[("user_id", &member.to_string()), ("role_id", "1")],
        )
        .await
        .assert_redirect_to(&access);
    // The access page lists who was given access, and the other one doesn't.
    let granted = client.get(&access).await;
    let granted_badge = "<span class=\"badge badge-neutral\">Member</span>";
    assert!(granted.body.contains(granted_badge), "{}", granted.body);
    let other = client
        .get(&format!("/@{}/children/{}/access", root.slug, b.org_id))
        .await;
    assert!(!other.body.contains(granted_badge), "{}", other.body);
    client.login_as(&member);
    // A member who can't open the Sub-orgs tab isn't shown it.
    let org_page = client.get(&format!("/@{}/users", root.slug)).await;
    org_page.assert_status(StatusCode::OK);
    assert!(
        !org_page.body.contains(&format!("href=\"{path}\"")),
        "{}",
        org_page.body
    );
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
    // The notice leads to the form that reactivates the account.
    assert!(
        recovery
            .body
            .contains(&format!("href=\"{root_lifecycle}\""))
    );
    let status_page = client.get(&root_lifecycle).await;
    status_page.assert_status(StatusCode::OK);
    assert!(status_page.body.contains("value=\"suspended\" checked"));
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
    // The text of each `tag` element in an organization's part of the page.
    let texts = |org_id: Uuid, end: &str, tag: &str| -> Vec<String> {
        let marker = format!("data-overview-org=\"{org_id}\"");
        let rest = page.body.split(&marker).nth(1).expect("overview");
        rest.split(end)
            .next()
            .unwrap()
            .split(&format!("<{tag}"))
            .skip(1)
            .map(|cell| {
                let inner = cell.split(&format!("</{tag}>")).next().unwrap();
                let mut text = String::new();
                let mut in_tag = true; // the rest of the opening tag
                for c in inner.chars() {
                    match c {
                        '<' => in_tag = true,
                        '>' => in_tag = false,
                        c if !in_tag => text.push(c),
                        _ => {}
                    }
                }
                text.trim().to_string()
            })
            .collect()
    };
    // The parent organization: running, failed in 24h and runs this month.
    let root_counts = texts(td.org_id, "</section>", "dd");
    assert_eq!(root_counts[..3], ["1", "1", "2"], "{root_counts:?}");
    // A sub-organization's row: name, status, then the same three counts.
    let child_row = texts(child.org.org_id, "</tr>", "td");
    assert_eq!(child_row[1..5], ["active", "0", "0", "0"], "{child_row:?}");
    let last_run = long_ago.format("%Y-%m-%d %H:%M UTC").to_string();
    assert!(child_row.contains(&last_run), "{child_row:?}");
    // Its own page reports the same activity.
    let detail = client
        .get(&format!("/@{}/children/{}", root.slug, child.org.org_id))
        .await;
    detail.assert_status(StatusCode::OK);
    assert!(detail.body.contains(&last_run), "{}", detail.body);
}

/// An account can hold thousands of sub-organizations: the list is a page at a
/// time, and found by name, handle or customer reference.
#[tokio::test]
async fn the_sub_org_list_pages_searches_and_filters() {
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
    for n in 1..=30 {
        Org::create_child(
            client.db(),
            &td.org_id,
            &td.user_id,
            &CreateChild {
                name: format!("Customer {n:02}"),
                slug: format!("paged-{n:02}"),
                external_id: Some(format!("ref-{n:02}")),
                idempotency_key: format!("paged-{n:02}"),
                hosted: false,
            },
        )
        .await
        .unwrap();
    }
    let suspended = Org::get_org_by_slug(client.db(), "paged-07").await.unwrap();
    Org::set_lifecycle(
        client.db(),
        &suspended.org_id,
        &td.user_id,
        hot::db::org::OrgLifecycle::Suspended,
    )
    .await
    .unwrap();
    client.login_as(&td.user_id);
    let path = format!("/@{}/children", root.slug);
    let rows = |body: &str| body.matches("<tr class=\"hover:").count();

    let page = client.get(&path).await;
    page.assert_status(StatusCode::OK);
    assert_eq!(rows(&page.body), 25);
    assert!(page.body.contains("30 sub-organizations"), "{}", page.body);
    assert!(page.body.contains(">Customer 01<"));
    assert!(!page.body.contains(">Customer 26<"));
    let page = client.get(&format!("{path}?p=2")).await;
    assert_eq!(rows(&page.body), 5);
    assert!(page.body.contains(">Customer 26<"));

    // By name, by handle and by customer reference.
    for (search, found) in [("customer+1", 10), ("PAGED-2", 10), ("ref-30", 1)] {
        let page = client.get(&format!("{path}?search={search}")).await;
        page.assert_status(StatusCode::OK);
        assert_eq!(rows(&page.body), found, "{search}");
    }
    // The page links keep the search.
    for n in 31..=60 {
        Org::create_child(
            client.db(),
            &td.org_id,
            &td.user_id,
            &CreateChild {
                name: format!("Customer {n:02}"),
                slug: format!("paged-{n:02}"),
                external_id: None,
                idempotency_key: format!("paged-{n:02}"),
                hosted: false,
            },
        )
        .await
        .unwrap();
    }
    let page = client.get(&format!("{path}?search=customer")).await;
    let next = page
        .body
        .split("href=\"")
        .map(|rest| rest.split('"').next().unwrap())
        .find(|href| href.ends_with("p=2"))
        .expect("a link to the second page");
    assert!(next.starts_with(&path), "{next}");
    assert!(next.contains("search=customer"), "{next}");

    let page = client.get(&format!("{path}?state=suspended")).await;
    assert_eq!(rows(&page.body), 1);
    assert!(page.body.contains(">Customer 07<"));
    let page = client
        .get(&format!("{path}?state=suspended&search=customer+08"))
        .await;
    assert_eq!(rows(&page.body), 0);
    assert!(page.body.contains("No matches"), "{}", page.body);

    // A page past the end doesn't claim the account is empty.
    let page = client.get(&format!("{path}?p=99")).await;
    page.assert_status(StatusCode::OK);
    assert!(page.body.contains("Nothing on this page"), "{}", page.body);
    assert!(!page.body.contains("No sub-organizations yet"));
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

    // Keys that will never work again stay out of the way until asked for.
    let list = format!("/keys/org?org_id={}", root.org_id);
    let page = client.get(&list).await;
    page.assert_status(StatusCode::OK);
    assert!(!page.body.contains(">Revoked<"), "{}", page.body);
    assert!(
        page.body.contains("Show 2 revoked or expired"),
        "{}",
        page.body
    );
    let page = client.get(&format!("{list}&show=all")).await;
    page.assert_status(StatusCode::OK);
    assert!(page.body.contains(">Revoked<"), "{}", page.body);
    // The minted key's own row is active, but it no longer authenticates.
    assert!(page.body.contains(">Issuer inactive<"), "{}", page.body);
    let page = client
        .get(&format!("/keys/org/{minted}/edit?org_id={}", root.org_id))
        .await;
    page.assert_status(StatusCode::OK);
    assert!(page.body.contains(">Issuer inactive<"), "{}", page.body);
    assert!(
        !page.body.contains(&format!("/keys/org/{minted}/revoke")),
        "a dead key offers no revoke: {}",
        page.body
    );
}

/// A key a person made has no issuer to rotate it, and no key rotates itself,
/// so the page is the only way to replace its secret.
#[tokio::test]
async fn an_administrator_rotates_an_org_key_and_sees_the_overlap() {
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
    let (key, first) = OrgApiKey::create_for_user(
        client.db(),
        td.org_id,
        td.user_id,
        &CreateOrgApiKey {
            description: "person-made".into(),
            permissions: serde_json::json!({"org:*": ["read"]}),
            expires_at: None,
        },
    )
    .await
    .unwrap();
    let id = key.org_api_key_id;
    client.login_as(&td.user_id);
    let page_path = format!("/keys/org/{id}/edit?org_id={}", root.org_id);
    // A form that changes an environment-scoped page names its workspace.
    let rotate_path = format!("/keys/org/{id}/rotate");
    let rotate = format!("{rotate_path}?org_id={}&env_id={}", root.org_id, td.env_id);

    let page = client.get(&page_path).await;
    assert!(page.body.contains(&rotate_path), "{}", page.body);
    assert!(!page.body.contains("Previous secret works until"));

    let page = client
        .post_form(&rotate, &[("overlap_seconds", "3600")])
        .await;
    page.assert_status(StatusCode::OK);
    let token = page
        .body
        .split("<code data-issued-secret")
        .nth(1)
        .and_then(|rest| rest.split('>').nth(1))
        .and_then(|rest| rest.split('<').next())
        .expect("the new secret is shown once")
        .to_string();
    assert_ne!(token, first);
    assert_eq!(
        OrgApiKey::verify_token(client.db(), &token)
            .await
            .unwrap()
            .org_api_key_id,
        id,
        "the same key"
    );
    assert!(OrgApiKey::verify_token(client.db(), &first).await.is_ok());
    assert!(
        page.body.contains("The previous secret works until"),
        "{}",
        page.body
    );
    let page = client.get(&page_path).await;
    assert!(!page.body.contains(&token), "shown only once");
    assert!(page.body.contains("Previous secret works until"));

    // Another rotation would stop the secret the page just promised.
    let page = client
        .post_form(&rotate, &[("overlap_seconds", "3600")])
        .await;
    page.assert_status(StatusCode::BAD_REQUEST);
    assert!(page.body.contains("Rotate after that"), "{}", page.body);
    assert!(OrgApiKey::verify_token(client.db(), &first).await.is_ok());
    assert!(OrgApiKey::verify_token(client.db(), &token).await.is_ok());

    let page = client.post_form(&rotate, &[("overlap_seconds", "0")]).await;
    page.assert_status(StatusCode::OK);
    assert!(OrgApiKey::verify_token(client.db(), &first).await.is_err());
    assert!(OrgApiKey::verify_token(client.db(), &token).await.is_err());
    assert!(
        page.body
            .contains("The previous secret has stopped working"),
        "{}",
        page.body
    );
    let page = client.get(&page_path).await;
    assert!(!page.body.contains("Previous secret works until"));

    client
        .post_form(&rotate, &[("overlap_seconds", "999999")])
        .await
        .assert_status(StatusCode::BAD_REQUEST);
    OrgApiKey::revoke_for_user(client.db(), td.org_id, td.user_id, id)
        .await
        .unwrap();
    let page = client.get(&page_path).await;
    assert!(
        !page.body.contains(&rotate_path),
        "a dead key offers no rotate"
    );
    client
        .post_form(&rotate, &[("overlap_seconds", "0")])
        .await
        .assert_status(StatusCode::BAD_REQUEST);
}

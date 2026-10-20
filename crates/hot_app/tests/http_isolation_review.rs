//! Regressions for app boundaries that must hold even without PostgreSQL RLS.
use axum::http::StatusCode;
use hot::db::{DatabasePool, Env, Org, OrgUser, Run, Team, TeamUser, User, insert_test_data};
use hot_app::test_support::TestClient;
use serde_json::{Value, json};
use uuid::Uuid;

fn sqlite(client: &TestClient) -> &sqlx::SqlitePool {
    let DatabasePool::Sqlite(pool) = client.db().as_ref() else {
        unreachable!()
    };
    pool
}

#[tokio::test]
async fn call_inspection_requires_the_selected_environment() {
    let mut client = TestClient::new().await;
    let td = insert_test_data(client.db()).await.unwrap();
    let other_env = Uuid::now_v7();
    Env::insert_env(client.db(), &other_env, &td.org_id, "other", &td.user_id)
        .await
        .unwrap();
    let other_project = Uuid::now_v7();
    let other_build = Uuid::now_v7();
    hot::db::Project::insert_project(
        client.db(),
        &other_project,
        &other_env,
        "other-project",
        &td.user_id,
    )
    .await
    .unwrap();
    hot::db::Build::insert_build(
        client.db(),
        &other_build,
        &other_project,
        "other-build",
        0,
        hot::db::Build::BUILD_TYPE_LIVE,
        &td.user_id,
    )
    .await
    .unwrap();
    let other_run = Uuid::now_v7();
    Run::insert_run(
        client.db(),
        &other_run,
        &other_env,
        &Uuid::now_v7(),
        Some(&other_build),
        1,
        None,
        &td.user_id,
        None,
        None,
    )
    .await
    .unwrap();
    let call = Uuid::now_v7();
    sqlx::query("INSERT INTO call (call_id,run_id,function_name,static_scope,runtime_path,call_depth,args,start_time,stop_time,duration_us,env_id) VALUES (?,?,'::private/customer','::private/customer','customer-0',0,?,datetime('now'),datetime('now'),1000,?)")
        .bind(call).bind(other_run).bind(json!(["private-customer-payload"]).to_string()).bind(other_env)
        .execute(sqlite(&client)).await.unwrap();
    client.login_as(&td.user_id);
    let paths = [
        format!("/data/runs/{other_run}/hierarchy"),
        format!("/data/runs/{other_run}/calls/search?q=private-customer-payload"),
        format!("/data/calls/{call}"),
    ];
    for path in &paths {
        let join = if path.contains('?') { '&' } else { '?' };
        let response = client
            .get(&format!(
                "{path}{join}org_id={}&env_id={other_env}",
                td.org_id
            ))
            .await;
        response.assert_status(StatusCode::OK);
        let body: Value = serde_json::from_str(&response.body).unwrap();
        assert_eq!(
            body["success"], true,
            "positive control {path}: {}",
            response.body
        );
        let response = client
            .get(&format!(
                "{path}{join}org_id={}&env_id={}",
                td.org_id, td.env_id
            ))
            .await;
        response.assert_status(StatusCode::OK);
        let body: Value = serde_json::from_str(&response.body).unwrap();
        assert_eq!(
            body["success"], false,
            "cross-environment read {path}: {}",
            response.body
        );
        assert!(!response.body.contains("private-customer"));
    }
    // Deactivating another environment must not leave a read path through a
    // still-active environment in the same organization.
    sqlx::query("UPDATE env SET active=false WHERE env_id=?")
        .bind(other_env)
        .execute(sqlite(&client))
        .await
        .unwrap();
    for path in paths {
        let join = if path.contains('?') { '&' } else { '?' };
        let response = client
            .get(&format!(
                "{path}{join}org_id={}&env_id={}",
                td.org_id, td.env_id
            ))
            .await;
        let body: Value = serde_json::from_str(&response.body).unwrap();
        assert_eq!(body["success"], false);
    }
}

#[tokio::test]
async fn pending_api_keys_cannot_move_between_organizations_environments_or_users() {
    let mut client = TestClient::new().await;
    let td = insert_test_data(client.db()).await.unwrap();
    let other_env = Uuid::now_v7();
    Env::insert_env(client.db(), &other_env, &td.org_id, "other", &td.user_id)
        .await
        .unwrap();
    let other_user = Uuid::now_v7();
    User::insert_user(
        client.db(),
        &other_user,
        "other-admin@example.test",
        Some("Other admin"),
        Some(&td.user_id),
    )
    .await
    .unwrap();
    OrgUser::insert_org_user(
        client.db(),
        &Uuid::now_v7(),
        &td.org_id,
        &other_user,
        Some(2),
        &td.user_id,
    )
    .await
    .unwrap();
    client.login_as(&td.user_id);
    let page = client
        .get(&format!(
            "/keys/new?org_id={}&env_id={}",
            td.org_id, td.env_id
        ))
        .await;
    page.assert_status(StatusCode::OK);
    let key_id = page
        .body
        .split("name=\"api_key_id\" value=\"")
        .nth(1)
        .unwrap()
        .split('"')
        .next()
        .unwrap()
        .to_owned();
    let form = [
        ("api_key_id", key_id.as_str()),
        ("description", "Scope-bound credential"),
        ("access_level", "full"),
    ];
    let create_path = |env| format!("/keys/new?org_id={}&env_id={env}", td.org_id);
    // An explicit new target must not rebind a previously displayed secret.
    let response = client.post_form(&create_path(other_env), &form).await;
    assert!(
        response.body.contains("Session expired"),
        "{}",
        response.body
    );
    let foreign_org = Uuid::now_v7();
    let foreign_env = Uuid::now_v7();
    Org::insert_org(
        client.db(),
        &foreign_org,
        "Other account",
        "other-account",
        "organization",
        &td.user_id,
    )
    .await
    .unwrap();
    OrgUser::insert_org_user(
        client.db(),
        &Uuid::now_v7(),
        &foreign_org,
        &td.user_id,
        Some(2),
        &td.user_id,
    )
    .await
    .unwrap();
    Env::insert_env(
        client.db(),
        &foreign_env,
        &foreign_org,
        "production",
        &td.user_id,
    )
    .await
    .unwrap();
    let response = client
        .post_form(
            &format!("/keys/new?org_id={foreign_org}&env_id={foreign_env}"),
            &form,
        )
        .await;
    assert!(
        response.body.contains("Session expired"),
        "{}",
        response.body
    );
    client.login_as(&other_user);
    let response = client.post_form(&create_path(td.env_id), &form).await;
    assert!(
        response.body.contains("Session expired"),
        "{}",
        response.body
    );
    assert!(
        hot::db::api_key::ApiKey::get_api_key(client.db(), &Uuid::parse_str(&key_id).unwrap())
            .await
            .is_err()
    );
    // Rejected use must not consume the original owner's pending credential.
    client.login_as(&td.user_id);
    client
        .post_form(&create_path(td.env_id), &form)
        .await
        .assert_status(StatusCode::SEE_OTHER);
    let key =
        hot::db::api_key::ApiKey::get_api_key(client.db(), &Uuid::parse_str(&key_id).unwrap())
            .await
            .unwrap();
    assert_eq!(key.env_id, td.env_id);
}

#[tokio::test]
async fn child_team_members_require_active_child_access() {
    let mut client =
        TestClient::new_with_conf(hot::val!({"product":{"experience":"self-host"}})).await;
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
        &hot::db::org::CreateChild {
            name: "Customer".into(),
            slug: "team-customer".into(),
            external_id: None,
            idempotency_key: "team-customer".into(),
            hosted: false,
        },
    )
    .await
    .unwrap();
    let team = Uuid::now_v7();
    Team::insert_team(
        client.db(),
        &team,
        &child.org.org_id,
        "Customer team",
        &td.user_id,
    )
    .await
    .unwrap();
    let member = Uuid::now_v7();
    User::insert_user(
        client.db(),
        &member,
        "account-member@example.test",
        Some("Account member"),
        Some(&td.user_id),
    )
    .await
    .unwrap();
    client.login_as(&td.user_id);
    let add = format!("/@{}/teams/{team}/users/add", child.org.slug);
    let member_id = member.to_string();
    let form = [("user_id", member_id.as_str()), ("role_id", "1")];
    let response = client.post_form(&add, &form).await;
    assert!(
        response.body.contains("active member of this organization"),
        "{}",
        response.body
    );
    assert!(
        TeamUser::get_team_user(client.db(), &team, &member)
            .await
            .is_err()
    );
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
    let response = client.post_form(&add, &form).await;
    assert!(
        response.body.contains("active member of this organization"),
        "{}",
        response.body
    );
    assert!(
        TeamUser::get_team_user(client.db(), &team, &member)
            .await
            .is_err()
    );
    Org::set_child_user_grant(
        client.db(),
        &td.org_id,
        &child.org.org_id,
        &td.user_id,
        &member,
        Some(1),
    )
    .await
    .unwrap();
    client
        .post_form(&add, &form)
        .await
        .assert_status(StatusCode::SEE_OTHER);
    TeamUser::get_team_user(client.db(), &team, &member)
        .await
        .unwrap();
    // A removed child grant cannot be revived through an existing team row.
    Org::set_child_user_grant(
        client.db(),
        &td.org_id,
        &child.org.org_id,
        &td.user_id,
        &member,
        None,
    )
    .await
    .unwrap();
    TeamUser::update_team_user(client.db(), &team, &member, 1, false, &td.user_id)
        .await
        .unwrap();
    let response = client
        .post_form(
            &format!("/@{}/teams/{team}/users/{member}/edit", child.org.slug),
            &[("role_id", "1"), ("active", "true")],
        )
        .await;
    assert!(
        response.body.contains("active member of this organization"),
        "{}",
        response.body
    );
    assert!(
        !TeamUser::get_team_user(client.db(), &team, &member)
            .await
            .unwrap()
            .active
    );
}

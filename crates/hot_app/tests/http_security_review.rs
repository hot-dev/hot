//! Integration tests for org-admin authorization on custom domains, project
//! deploys / toggle-active, API key edit, service-key expiry bounds, and
//! secret-page cache/referrer headers.

use axum::http::StatusCode;
use hot::context_encryption::ContextEncryption;
use hot::db::api_key::ApiKey;
use hot::db::domain::Domain;
use hot::db::org::OrgUser;
use hot::db::{Build, Context, Project, User, insert_test_data};
use hot::val;
use hot_app::test_support::TestClient;
use serde_json::json;
use uuid::Uuid;

fn in_env(path: &str, org_id: Uuid, env_id: Uuid) -> String {
    format!("{path}?org_id={org_id}&env_id={env_id}")
}

#[tokio::test]
async fn non_admins_are_rejected_from_domains_projects_and_key_edit_while_admins_succeed() {
    let conf = val!({
        "domain": { "mode": "test" },
        "app": {
            "host": "127.0.0.1",
            "port": 0,
            "session": { "duration-hours": 24 }
        }
    });
    let mut client = TestClient::new_with_conf(conf).await;
    let td = insert_test_data(client.db()).await.unwrap();

    // Seed a non-admin org member (role 1 = member, 2 = admin)
    let member_id = Uuid::now_v7();
    User::insert_user(
        client.db(),
        &member_id,
        "member@example.com",
        Some("Member"),
        Some(&td.user_id),
    )
    .await
    .unwrap();
    OrgUser::insert_org_user(
        client.db(),
        &Uuid::now_v7(),
        &td.org_id,
        &member_id,
        Some(1),
        &td.user_id,
    )
    .await
    .unwrap();

    // Seed an existing domain, project, build (live build_type_id = 1), and API key
    let domain = Domain::create(client.db(), &td.env_id, "api.example.com")
        .await
        .unwrap();
    let project_id = Uuid::now_v7();
    Project::insert_project(
        client.db(),
        &project_id,
        &td.env_id,
        "demo-proj",
        &td.user_id,
    )
    .await
    .unwrap();
    let build_id = Uuid::now_v7();
    Build::insert_build(
        client.db(),
        &build_id,
        &project_id,
        "deadbeef",
        42,
        1, // live build (activates directly without bundle file)
        &td.user_id,
    )
    .await
    .unwrap();
    if let hot::db::DatabasePool::Sqlite(pool) = client.db().as_ref() {
        sqlx::query("UPDATE build SET runtime_status = 'ready' WHERE build_id = ?")
            .bind(build_id)
            .execute(pool)
            .await
            .unwrap();
    }
    let api_key_id = Uuid::now_v7();
    let (_token, hash) = ApiKey::generate_api_key(&api_key_id).unwrap();
    ApiKey::insert_api_key(
        client.db(),
        &api_key_id,
        &td.env_id,
        "env-key",
        &serde_json::from_str(&hash).unwrap(),
        &td.user_id,
        &json!({"*:*": ["*"]}),
    )
    .await
    .unwrap();

    // --- Non-admin member ---
    client.login_as(&member_id);

    // 1. Custom domain routes: GET /domains/new and POST create/verify/delete all 403
    client
        .get(&in_env("/domains/new", td.org_id, td.env_id))
        .await
        .assert_status(StatusCode::FORBIDDEN);
    client
        .post_form(
            &in_env("/domains/new", td.org_id, td.env_id),
            &[("domain", "member.example.com")],
        )
        .await
        .assert_status(StatusCode::FORBIDDEN);
    client
        .post_form(
            &in_env(
                &format!("/domains/{}/verify", domain.domain_id),
                td.org_id,
                td.env_id,
            ),
            &[],
        )
        .await
        .assert_status(StatusCode::FORBIDDEN);
    client
        .post_form(
            &in_env(
                &format!("/domains/{}/delete", domain.domain_id),
                td.org_id,
                td.env_id,
            ),
            &[],
        )
        .await
        .assert_status(StatusCode::FORBIDDEN);

    // 2. Project deploy & toggle-active: both 403
    client
        .post_form(
            &in_env(
                &format!("/projects/demo-proj/builds/{build_id}/deploy"),
                td.org_id,
                td.env_id,
            ),
            &[],
        )
        .await
        .assert_status(StatusCode::FORBIDDEN);
    client
        .post_form(
            &in_env("/projects/demo-proj/toggle-active", td.org_id, td.env_id),
            &[],
        )
        .await
        .assert_status(StatusCode::FORBIDDEN);

    // 3. API key edit page: 403 for non-admin
    client
        .get(&in_env(
            &format!("/keys/{api_key_id}/edit"),
            td.org_id,
            td.env_id,
        ))
        .await
        .assert_status(StatusCode::FORBIDDEN);

    // 4. Templates hide admin-only actions for non-admins
    let domains_list = client.get(&in_env("/domains", td.org_id, td.env_id)).await;
    domains_list.assert_status(StatusCode::OK);
    assert!(!domains_list.body.contains("href=\"/domains/new\""));

    let domain_detail = client
        .get(&in_env(
            &format!("/domains/{}", domain.domain_id),
            td.org_id,
            td.env_id,
        ))
        .await;
    domain_detail.assert_status(StatusCode::OK);
    assert!(!domain_detail.body.contains("/delete"));
    assert!(!domain_detail.body.contains("/verify"));

    let project_detail = client
        .get(&in_env("/projects/demo-proj", td.org_id, td.env_id))
        .await;
    project_detail.assert_status(StatusCode::OK);
    assert!(!project_detail.body.contains("/toggle-active"));

    let project_builds = client
        .get(&in_env("/projects/demo-proj/builds", td.org_id, td.env_id))
        .await;
    project_builds.assert_status(StatusCode::OK);
    assert!(!project_builds.body.contains("/deploy"));

    // --- Org admin ---
    client.login_as(&td.user_id);

    // Templates show admin actions for admins
    let admin_domains = client.get(&in_env("/domains", td.org_id, td.env_id)).await;
    admin_domains.assert_status(StatusCode::OK);
    assert!(admin_domains.body.contains("href=\"/domains/new\""));

    let admin_project = client
        .get(&in_env("/projects/demo-proj", td.org_id, td.env_id))
        .await;
    admin_project.assert_status(StatusCode::OK);
    assert!(admin_project.body.contains("/toggle-active"));

    let admin_builds = client
        .get(&in_env("/projects/demo-proj/builds", td.org_id, td.env_id))
        .await;
    admin_builds.assert_status(StatusCode::OK);
    assert!(admin_builds.body.contains("/deploy"));

    // 1. Admin creates, verifies, and deletes a domain
    let create_res = client
        .post_form(
            &in_env("/domains/new", td.org_id, td.env_id),
            &[("domain", "admin.example.com")],
        )
        .await;
    assert_eq!(create_res.status, StatusCode::SEE_OTHER);

    let verify_res = client
        .post_form(
            &in_env(
                &format!("/domains/{}/verify", domain.domain_id),
                td.org_id,
                td.env_id,
            ),
            &[],
        )
        .await;
    assert_eq!(verify_res.status, StatusCode::SEE_OTHER);

    let delete_res = client
        .post_form(
            &in_env(
                &format!("/domains/{}/delete", domain.domain_id),
                td.org_id,
                td.env_id,
            ),
            &[],
        )
        .await;
    assert_eq!(delete_res.status, StatusCode::SEE_OTHER);

    // 2. Admin deploys a build and toggles project active state
    let deploy_res = client
        .post_form(
            &in_env(
                &format!("/projects/demo-proj/builds/{build_id}/deploy"),
                td.org_id,
                td.env_id,
            ),
            &[],
        )
        .await;
    assert_eq!(deploy_res.status, StatusCode::SEE_OTHER);

    let toggle_res = client
        .post_form(
            &in_env("/projects/demo-proj/toggle-active", td.org_id, td.env_id),
            &[],
        )
        .await;
    assert_eq!(toggle_res.status, StatusCode::SEE_OTHER);
    let updated_project =
        Project::get_project_by_env_and_name(client.db(), &td.env_id, "demo-proj")
            .await
            .unwrap()
            .unwrap();
    assert!(!updated_project.active);
}

#[tokio::test]
async fn service_key_create_rejects_overflowing_expires_in_days() {
    let mut client = TestClient::new().await;
    let td = insert_test_data(client.db()).await.unwrap();
    let api_key_id = Uuid::now_v7();
    let (_token, hash) = ApiKey::generate_api_key(&api_key_id).unwrap();
    ApiKey::insert_api_key(
        client.db(),
        &api_key_id,
        &td.env_id,
        "parent-key",
        &serde_json::from_str(&hash).unwrap(),
        &td.user_id,
        &json!({"*:*": ["*"]}),
    )
    .await
    .unwrap();
    client.login_as(&td.user_id);

    let huge = i64::MAX.to_string();
    let res = client
        .post_form(
            &in_env("/service-keys/new", td.org_id, td.env_id),
            &[
                ("name", "overflow-test"),
                ("permissions", r#"{"webhook:*":["execute"]}"#),
                ("expires_in_days", &huge),
            ],
        )
        .await;
    res.assert_status(StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn contexts_edit_sets_no_store_and_no_referrer_headers() {
    let mut client = TestClient::new().await;
    let td = insert_test_data(client.db()).await.unwrap();
    let encryption = ContextEncryption::from_env_or_generate_for_dev("local-dev").unwrap();
    let encrypted =
        Context::set_value_from_val(&val!("top-secret"), &encryption, &td.org_id).unwrap();
    let ctx_id = Uuid::now_v7();
    Context::insert_env(
        client.db(),
        &ctx_id,
        &td.env_id,
        "api.secret",
        &encrypted,
        Some("Secret value"),
        &td.user_id,
    )
    .await
    .unwrap();

    client.login_as(&td.user_id);
    let res = client
        .get(&in_env(
            &format!("/contexts/env/{ctx_id}/edit"),
            td.org_id,
            td.env_id,
        ))
        .await;
    res.assert_status(StatusCode::OK);
    assert_eq!(
        res.headers
            .get("cache-control")
            .and_then(|v| v.to_str().ok()),
        Some("no-store")
    );
    assert_eq!(
        res.headers
            .get("referrer-policy")
            .and_then(|v| v.to_str().ok()),
        Some("no-referrer")
    );
}

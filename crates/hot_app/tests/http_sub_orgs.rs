//! Browser-level scope, hierarchy, and recovery checks through the real router.
use axum::http::StatusCode;
use hot::db::{DatabasePool, Org, insert_test_data, org::OrgUser, user::User};
use hot_app::test_support::TestClient;
use uuid::Uuid;

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

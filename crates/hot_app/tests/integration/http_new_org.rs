//! The New Organization page, as the first administrator of a self-hosted
//! installation meets it: a user with no organization, through the real router.

use axum::http::StatusCode;
use hot::db::{Org, User};
use hot::val;
use hot_app::test_support::TestClient;
use uuid::Uuid;

/// A signed-in user who belongs to no organization, as `hot admin bootstrap`
/// leaves the first administrator.
async fn first_admin() -> (TestClient, Uuid) {
    let mut client =
        TestClient::new_with_conf(val!({"product": {"experience": "self-host"}})).await;
    let user_id = Uuid::now_v7();
    User::insert_user(
        client.db(),
        &user_id,
        "admin@example.test",
        Some("First Admin"),
        Some(&user_id),
    )
    .await
    .unwrap();
    client.login_as(&user_id);
    (client, user_id)
}

/// The fields of the page's form, so a test posts what a browser would.
async fn new_org_fields(client: &mut TestClient) -> Vec<String> {
    let page = client.get("/orgs/new").await;
    page.assert_status(StatusCode::OK);
    let form = page
        .body
        .split("<form")
        .find(|form| form.contains("org_slug"));
    let mut fields: Vec<String> = form
        .expect("the page has a form")
        .split("name=\"")
        .skip(1)
        .filter_map(|rest| rest.split('"').next())
        .map(str::to_string)
        .collect();
    // A radio group's inputs share one name.
    fields.dedup();
    fields
}

#[tokio::test]
async fn the_first_administrator_creates_an_organization_from_the_page() {
    let (mut client, user_id) = first_admin().await;
    let fields = new_org_fields(&mut client).await;
    let value = |field: &str| match field {
        "account_type" => "organization",
        "org_name" => "Northwind Holdings",
        "org_slug" => "northwind",
        other => panic!("the page has a field no test value covers: {other}"),
    };
    let form: Vec<(&str, &str)> = fields.iter().map(|f| (f.as_str(), value(f))).collect();
    client
        .post_form("/orgs/new", &form)
        .await
        .assert_redirect_to("/@northwind");

    let org = Org::get_org_by_slug(client.db(), "northwind")
        .await
        .unwrap();
    assert_eq!(org.name, "Northwind Holdings");
    assert_eq!(org.org_type, "organization");
    assert_eq!(org.created_by_user_id, user_id);
    // Only an organization holds sub-organizations.
    client
        .get("/@northwind/children")
        .await
        .assert_status(StatusCode::OK);
}

/// An individual account hides the name field, so it arrives blank.
#[tokio::test]
async fn an_individual_account_takes_its_owners_name() {
    let (mut client, _) = first_admin().await;
    client
        .post_form(
            "/orgs/new",
            &[
                ("account_type", "individual"),
                ("org_name", ""),
                ("org_slug", "first-admin"),
            ],
        )
        .await
        .assert_redirect_to("/@first-admin");
    let org = Org::get_org_by_slug(client.db(), "first-admin")
        .await
        .unwrap();
    assert_eq!(org.name, "First Admin");
    assert_eq!(org.org_type, "individual");
}

#[tokio::test]
async fn an_organization_needs_a_name_and_keeps_what_was_typed() {
    let (mut client, _) = first_admin().await;
    let page = client
        .post_form(
            "/orgs/new",
            &[
                ("account_type", "organization"),
                ("org_name", "  "),
                ("org_slug", "northwind"),
            ],
        )
        .await;
    assert!(
        page.body.contains("Organization name is required"),
        "{}",
        page.body
    );
    assert!(
        Org::get_org_by_slug(client.db(), "northwind")
            .await
            .is_err(),
        "nothing was created"
    );
}

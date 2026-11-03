//! An App on a network address with `HOT_ENV` unset, which is how a deployment
//! that forgot `HOT_ENV` runs. Nobody is signed in automatically, so the sign-in
//! pages have to work. This binary has a single test because it changes the
//! process's environment and fixes the host the App listens on.

use axum::http::StatusCode;
use hot::db::User;
use hot_app::auth::{jwt_cookie_name, local_dev_conveniences};
use hot_app::test_support::{TestClient, set_listen_host};
use uuid::Uuid;

#[tokio::test]
async fn sign_in_pages_work_without_automatic_sign_in() {
    // SAFETY: nothing else runs in this binary, for the whole of this test.
    unsafe {
        std::env::remove_var("HOT_ENV");
        std::env::set_var("HOT_APP_SESSION_SECRET", "a-secret-of-this-deployments-own");
    }
    set_listen_host("0.0.0.0");
    assert!(hot::env::is_local_dev());
    assert!(!local_dev_conveniences());

    // An account exists, so automatic sign-in would have someone to choose.
    let mut client = TestClient::new().await;
    let user_id = Uuid::now_v7();
    User::insert_user(
        client.db(),
        &user_id,
        "person@example.com",
        Some("Person"),
        Some(&user_id),
    )
    .await
    .expect("insert user");

    // The dashboard sends a visitor to the sign-in page, which stays put.
    let dashboard = client.get("/").await;
    dashboard.assert_redirect_to("/signin");
    let signin = client.follow_redirects(dashboard, 3).await;
    assert_eq!(signin.status, StatusCode::OK);
    assert!(signin.body.contains("form_token"), "{}", signin.body);

    // That page offers the sign-in providers this deployment configured.
    // SAFETY: as above.
    unsafe {
        std::env::set_var("HOT_GOOGLE_CLIENT_ID", "google-id");
        std::env::set_var("HOT_GOOGLE_CLIENT_SECRET", "google-hush");
    }
    let page = client.get("/signin").await;
    assert!(page.body.contains("/auth/google"), "{}", page.body);

    // Neither sign-up page sends a visitor round in a circle.
    for path in ["/signup", "/signup/plans"] {
        let response = client.get(path).await;
        assert_ne!(response.location(), Some("/"), "{path}");
        let page = client.follow_redirects(response, 3).await;
        assert_eq!(page.status, StatusCode::OK, "{path}");
    }

    // Signing out ends the session, rather than going back to the dashboard
    // with the session still in place.
    client.login_as(&user_id);
    assert!(client.cookies().get(jwt_cookie_name()).is_some());
    let signout = client.post_form("/signout", &[]).await;
    signout.assert_redirect_to("/signin");
    assert!(client.cookies().get(jwt_cookie_name()).is_none());
    client.get("/").await.assert_redirect_to("/signin");
}

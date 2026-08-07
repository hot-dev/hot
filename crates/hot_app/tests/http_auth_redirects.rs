use axum::http::{StatusCode, header};
use hot_app::test_support::TestClient;

#[tokio::test]
async fn expired_dashboard_fetch_gets_an_htmx_redirect_without_signin_html() {
    let mut client = TestClient::new().await;

    let response = client
        .get_with_headers(
            "/dashboard/widgets/getting-started",
            &[("HX-Request", "true")],
        )
        .await;

    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(
        response
            .headers
            .get("HX-Redirect")
            .and_then(|value| value.to_str().ok()),
        Some("/signin")
    );
    assert!(
        response.body.is_empty(),
        "HTMX auth redirects must not return a sign-in document that can be swapped into the page"
    );
}

#[tokio::test]
async fn expired_run_detail_fetch_redirects_instead_of_returning_partial_html() {
    let mut client = TestClient::new().await;

    let response = client.get("/runs/run-123/tasks-tab").await;

    assert_eq!(response.status, StatusCode::SEE_OTHER);
    assert_eq!(
        response
            .headers
            .get(header::LOCATION)
            .and_then(|value| value.to_str().ok()),
        Some("/signin?next=%2Fruns%2Frun-123%2Ftasks-tab")
    );
    assert!(response.body.is_empty());
}

//! Untrusted values must reach the App's JavaScript only as `js`-filtered literals.
//!
//! Each test plants a payload from the v3.0.1 review in a page's data and checks the rendered
//! HTML: the value appears JSON-encoded (quotes and `<` as \uXXXX escapes) and never in a shape
//! that closes a JS string or a `<script>` element. `scripts/browser-security.test.mjs` runs
//! the same encodings in a real browser and lints the templates for unfiltered interpolation.
use axum::http::StatusCode;
use hot::db::{DatabasePool, TestData, insert_test_data};
use hot_app::test_support::{TestClient, TestResponse};
use serde_json::json;
use uuid::Uuid;

const SCRIPT_BREAKOUT: &str = "</script><script>alert(1)</script>";

fn sqlite(client: &TestClient) -> &sqlx::SqlitePool {
    let DatabasePool::Sqlite(pool) = client.db().as_ref() else {
        unreachable!()
    };
    pool
}

async fn deploy_test_build(client: &TestClient, td: &TestData) {
    sqlx::query(
        "UPDATE build SET deployed = 1, active = 1, runtime_status = 'ready' WHERE build_id = ?",
    )
    .bind(td.build_id)
    .execute(sqlite(client))
    .await
    .unwrap();
}

async fn signed_in() -> (TestClient, TestData) {
    let mut client = TestClient::new().await;
    let td = insert_test_data(client.db()).await.unwrap();
    client.login_as(&td.user_id);
    (client, td)
}

fn assert_no_script_breakout(response: &TestResponse) {
    assert!(
        !response.body.contains(SCRIPT_BREAKOUT),
        "a stored value closed a <script> element"
    );
}

/// Mutating requests name their scope explicitly, as the page's scope script does for forms.
fn scoped(path: &str, td: &TestData) -> String {
    format!("{path}?org_id={}&env_id={}", td.org_id, td.env_id)
}

/// The value of `<input ... name="{name}" value="...">` in a rendered form.
fn input_value<'a>(body: &'a str, name: &str) -> &'a str {
    let marker = format!("name=\"{name}\" value=\"");
    let start = body.find(&marker).expect("input present") + marker.len();
    &body[start..start + body[start..].find('"').unwrap()]
}

#[tokio::test]
async fn streams_list_encodes_query_parameters_for_alpine() {
    let (mut client, _td) = signed_in().await;
    let response = client
        .get("/streams?search=x%27%2Ca%3Aalert(1)%2Cb%3A%27&project=x%27%2Ca%3Aalert(2)&time_range=x%27%2Ca%3Aalert(3)")
        .await;
    response.assert_status(StatusCode::OK);
    let body = &response.body;
    // Alpine evaluates x-data after the browser decodes entities, so an HTML-escaped quote
    // would still end the string. The quote must arrive as a JS escape inside JSON.
    assert!(body.contains(r"search: &#34;x\u0027,a:alert(1),b:\u0027&#34;"));
    for decoded_quote in ["',a:alert", "&#39;,a:alert"] {
        assert!(!body.contains(decoded_quote), "{decoded_quote}");
    }
    // An invalid project or time range is not echoed at all.
    assert!(!body.contains("alert(2)") && !body.contains("alert(3)"));
    assert!(body.contains("project: &#34;&#34;") && body.contains("timeRange: &#34;all&#34;"));
}

#[tokio::test]
async fn key_pages_embed_tool_names_and_permissions_as_script_safe_json() {
    let (mut client, td) = signed_in().await;
    deploy_test_build(&client, &td).await;
    hot::db::mcp_tool::McpTool::insert_mcp_tool(
        client.db(),
        &Uuid::now_v7(),
        &td.build_id,
        "tools",
        "::tools",
        "lookup",
        SCRIPT_BREAKOUT,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .await
    .unwrap();

    // Opening "New API key" is enough to run a planted tool name on main.
    let response = client.get("/keys/new").await;
    response.assert_status(StatusCode::OK);
    assert_no_script_breakout(&response);
    assert!(
        response
            .body
            .contains(r"\u003c/script\u003e\u003cscript\u003ealert(1)")
    );

    let api_key_id = Uuid::now_v7();
    hot::db::api_key::ApiKey::insert_api_key(
        client.db(),
        &api_key_id,
        &td.env_id,
        "planted",
        &json!({}),
        &td.user_id,
        &json!({ format!("event:{SCRIPT_BREAKOUT}"): ["read"] }),
    )
    .await
    .unwrap();
    let response = client.get(&format!("/keys/{api_key_id}/edit")).await;
    response.assert_status(StatusCode::OK);
    assert_no_script_breakout(&response);
    assert!(response.body.contains(r#"{"event:\u003c/script\u003e"#));
}

#[tokio::test]
async fn a_failed_restricted_key_form_comes_back_restricted() {
    let (mut client, td) = signed_in().await;
    let first = client.get("/keys/new").await;
    first.assert_status(StatusCode::OK);
    let permissions = r#"{"event:*":["read"]}"#;
    let submitted_id = input_value(&first.body, "api_key_id").to_string();
    let response = client
        .post_form(
            &scoped("/keys/new", &td),
            &[
                ("api_key_id", submitted_id.as_str()),
                ("handoff_token", input_value(&first.body, "handoff_token")),
                ("description", "   "),
                ("access_level", "restricted"),
                ("permissions", permissions),
            ],
        )
        .await;
    let body = &response.body;
    assert!(body.contains("Description is required"));
    assert!(body.contains(r#"accessLevel: "restricted""#));
    assert!(body.contains(&format!("const initial = {permissions};")));
    // The re-render shows a new key: the old secret can't be shown again.
    let retry_id = input_value(body, "api_key_id").to_string();
    assert_ne!(retry_id, submitted_id);
    assert!(body.contains(&format!("hot_{}_", retry_id.replace('-', ""))));

    // Resubmitting the re-rendered form creates a restricted key.
    let created = client
        .post_form(
            &scoped("/keys/new", &td),
            &[
                ("api_key_id", retry_id.as_str()),
                ("handoff_token", input_value(body, "handoff_token")),
                ("description", "ci"),
                ("access_level", "restricted"),
                ("permissions", permissions),
            ],
        )
        .await;
    assert!(created.is_redirect(), "{}", created.body);
    let key = hot::db::api_key::ApiKey::get_api_key(client.db(), &retry_id.parse().unwrap())
        .await
        .unwrap();
    assert_eq!(key.permissions, json!({"event:*": ["read"]}));
    assert_eq!(key.env_id, td.env_id);

    // A missing or unknown access level is rejected rather than treated as full access.
    let page = client.get("/keys/new").await;
    let response = client
        .post_form(
            &scoped("/keys/new", &td),
            &[
                ("api_key_id", input_value(&page.body, "api_key_id")),
                ("handoff_token", input_value(&page.body, "handoff_token")),
                ("description", "ci"),
                ("access_level", "everything"),
            ],
        )
        .await;
    assert!(response.body.contains("Choose Full Access or Restricted"));
    assert!(response.body.contains(r#"accessLevel: "restricted""#));
}

#[tokio::test]
async fn an_expired_key_form_offers_a_fresh_key() {
    let (mut client, td) = signed_in().await;
    let response = client
        .post_form(
            &scoped("/keys/new", &td),
            &[
                ("api_key_id", &Uuid::now_v7().to_string()),
                ("handoff_token", "from-another-app-version"),
                ("description", "ci"),
                ("access_level", "full"),
            ],
        )
        .await;
    assert!(response.body.contains("This form expired"));
    assert!(response.body.contains(r#"accessLevel: "full""#));
    let retry_id = input_value(&response.body, "api_key_id");
    assert!(
        response
            .body
            .contains(&format!("hot_{}_", retry_id.replace('-', "")))
    );
}

#[tokio::test]
async fn webhook_paths_reach_alpine_expressions_as_js_literals() {
    let (mut client, td) = signed_in().await;
    deploy_test_build(&client, &td).await;
    hot::db::webhook::Webhook::insert_webhook(
        client.db(),
        &Uuid::now_v7(),
        &td.build_id,
        "seedhook",
        "/y'+alert('wh-render')+'",
        "POST",
        "::hooks",
        "receive",
        "receive",
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .await
    .unwrap();
    let response = client.get("/webhooks/seedhook").await;
    response.assert_status(StatusCode::OK);
    let body = &response.body;
    assert!(body.contains(r"currentBase + &#34;/y\u0027+alert(\u0027wh-render\u0027)+\u0027&#34;"));
    assert!(!body.contains("currentBase + &#39;"));
}

#[tokio::test]
async fn event_data_and_run_results_leave_inline_handlers() {
    let (mut client, td) = signed_in().await;
    let payload = "q');alert('x`;alert(1);`";
    hot::db::Event::insert_event(
        client.db(),
        &Uuid::now_v7(),
        &td.env_id,
        &td.stream_id,
        "test:event",
        &json!(payload),
        chrono::Utc::now(),
        &td.user_id,
        None,
    )
    .await
    .unwrap();
    let response = client.get("/events").await;
    response.assert_status(StatusCode::OK);
    let body = &response.body;
    assert!(body.contains("data-modal-hot=") && body.contains("openContentModalFromData(this)"));
    // The old server-rendered handler passed the data as JS string arguments.
    assert!(!body.contains("openContentModalWithFormats('Event Data', '"));
    assert!(!body.contains("openContentModalWithFormats('Event Data', `"));
}

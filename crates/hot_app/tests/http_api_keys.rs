//! The API Keys page: environment key lifecycle, and org keys on their own tab.
use axum::http::StatusCode;
use hot::db::api_key::ApiKey;
use hot::db::org::{CreateChild, OrgLifecycle};
use hot::db::org_api_key::{CreateOrgApiKey, MintEnvApiKey, OrgApiKey};
use hot::db::org_note::OrgNote;
use hot::db::{DatabasePool, Org, TestData, insert_test_data};
use hot_app::test_support::TestClient;
use serde_json::json;
use uuid::Uuid;

/// A signed-in administrator of an organization that can have sub-organizations.
async fn admin_client() -> (TestClient, TestData) {
    let mut client = TestClient::new().await;
    let td = insert_test_data(client.db()).await.unwrap();
    let DatabasePool::Sqlite(pool) = client.db().as_ref() else {
        unreachable!()
    };
    sqlx::query("UPDATE org SET org_type='organization' WHERE org_id=?")
        .bind(td.org_id)
        .execute(pool)
        .await
        .unwrap();
    client.login_as(&td.user_id);
    (client, td)
}

async fn env_key(client: &TestClient, td: &TestData) -> (Uuid, String) {
    let id = Uuid::now_v7();
    let (token, hash) = ApiKey::generate_api_key(&id).unwrap();
    ApiKey::insert_api_key(
        client.db(),
        &id,
        &td.env_id,
        "ci key",
        &serde_json::from_str(&hash).unwrap(),
        &td.user_id,
        &json!({"*:*": ["*"]}),
    )
    .await
    .unwrap();
    (id, token)
}

fn in_env(path: &str, td: &TestData) -> String {
    format!("{path}?org_id={}&env_id={}", td.org_id, td.env_id)
}

/// The value of a hidden form field on a page.
fn field(body: &str, name: &str) -> String {
    body.split(&format!("name=\"{name}\" value=\""))
        .nth(1)
        .unwrap_or_else(|| panic!("no {name} field"))
        .split('"')
        .next()
        .unwrap()
        .to_owned()
}

fn issued_secret(body: &str) -> String {
    body.split("<code data-issued-secret")
        .nth(1)
        .and_then(|rest| rest.split('>').nth(1))
        .and_then(|rest| rest.split('<').next())
        .expect("the new secret is shown once")
        .to_string()
}

async fn note_types(client: &TestClient, td: &TestData) -> Vec<String> {
    OrgNote::list_by_org(client.db(), &td.org_id, 50)
        .await
        .unwrap()
        .into_iter()
        .map(|note| note.note_type)
        .collect()
}

#[tokio::test]
async fn an_administrator_rotates_an_environment_key_a_person_made() {
    let (mut client, td) = admin_client().await;
    let (id, first) = env_key(&client, &td).await;
    let rotate = in_env(&format!("/keys/{id}/rotate"), &td);

    let page = client.get(&in_env(&format!("/keys/{id}/edit"), &td)).await;
    page.assert_status(StatusCode::OK);
    assert!(page.body.contains(&format!("/keys/{id}/rotate")));

    let page = client
        .post_form(&rotate, &[("overlap_seconds", "3600")])
        .await;
    page.assert_status(StatusCode::OK);
    let token = issued_secret(&page.body);
    assert_ne!(token, first);
    let verified = ApiKey::verify_api_key(client.db(), &token)
        .await
        .unwrap()
        .expect("the new secret works");
    assert_eq!(verified.api_key_id, id, "the same key");
    assert!(
        ApiKey::verify_api_key(client.db(), &first)
            .await
            .unwrap()
            .is_some()
    );

    // Another rotation would stop the secret the last one promised.
    let page = client
        .post_form(&rotate, &[("overlap_seconds", "3600")])
        .await;
    page.assert_status(StatusCode::OK);
    assert!(page.body.contains("Rotate after that"), "{}", page.body);
    let rotations = note_types(&client, &td)
        .await
        .iter()
        .filter(|kind| *kind == "api-key.rotate")
        .count();
    assert_eq!(rotations, 1, "a refused rotation leaves no note");

    client.post_form(&rotate, &[("overlap_seconds", "0")]).await;
    assert!(
        ApiKey::verify_api_key(client.db(), &first)
            .await
            .unwrap()
            .is_none(),
        "no overlap stops the older secret"
    );
}

#[tokio::test]
async fn a_revoked_key_cannot_be_turned_back_on_but_a_disabled_one_can() {
    let (mut client, td) = admin_client().await;
    let (id, token) = env_key(&client, &td).await;
    let edit = in_env(&format!("/keys/{id}/edit"), &td);
    let form = |active| {
        [
            ("description", "ci key"),
            ("active", active),
            ("access_level", "full"),
        ]
    };

    client.post_form(&edit, &form("false")).await;
    let page = client.get(&in_env("/keys", &td)).await;
    assert!(page.body.contains(">Disabled<"), "{}", page.body);
    client.post_form(&edit, &form("true")).await;
    assert!(
        ApiKey::verify_api_key(client.db(), &token)
            .await
            .unwrap()
            .is_some()
    );

    client
        .post_form(&in_env(&format!("/keys/{id}/revoke"), &td), &[])
        .await;
    assert!(
        ApiKey::verify_api_key(client.db(), &token)
            .await
            .unwrap()
            .is_none()
    );
    let page = client.get(&in_env("/keys", &td)).await;
    assert!(page.body.contains(">Revoked<"), "{}", page.body);
    let page = client.get(&edit).await;
    assert!(!page.body.contains("name=\"active\""), "{}", page.body);
    assert!(!page.body.contains(&format!("/keys/{id}/rotate")));

    // A stale tab can still post the old form.
    client.post_form(&edit, &form("true")).await;
    assert!(
        ApiKey::verify_api_key(client.db(), &token)
            .await
            .unwrap()
            .is_none(),
        "revoking is permanent"
    );

    let notes = note_types(&client, &td).await;
    assert!(
        notes.iter().any(|kind| kind == "api-key.revoke"),
        "{notes:?}"
    );
    assert_eq!(
        notes
            .iter()
            .filter(|kind| *kind == "api-key.update")
            .count(),
        2,
        "each change that took effect: {notes:?}"
    );
}

#[tokio::test]
async fn a_new_key_can_expire_and_shows_when() {
    let (mut client, td) = admin_client().await;
    let new = in_env("/keys/new", &td);
    let page = client.get(&new).await;
    let (key_id, handoff) = (
        field(&page.body, "api_key_id"),
        field(&page.body, "handoff_token"),
    );
    let rejected = [
        ("api_key_id", key_id.as_str()),
        ("handoff_token", handoff.as_str()),
        ("description", "expiring"),
        ("expires_in_days", "0"),
        ("access_level", "full"),
    ];
    let page = client.post_form(&new, &rejected).await;
    assert!(page.body.contains("Expiry must be"), "{}", page.body);

    let page = client.get(&new).await;
    let accepted = [
        ("api_key_id", field(&page.body, "api_key_id")),
        ("handoff_token", field(&page.body, "handoff_token")),
        ("description", "expiring".to_string()),
        ("expires_in_days", "30".to_string()),
        ("access_level", "full".to_string()),
    ];
    let id: Uuid = accepted[0].1.parse().unwrap();
    let accepted: Vec<_> = accepted.iter().map(|(k, v)| (*k, v.as_str())).collect();
    client.post_form(&new, &accepted).await;
    let state = ApiKey::get_credential_state(client.db(), &id)
        .await
        .unwrap();
    assert!(state.expires_at.is_some());
    let page = client.get(&in_env("/keys", &td)).await;
    assert!(page.body.contains("Expires "), "{}", page.body);
    assert!(
        note_types(&client, &td)
            .await
            .contains(&"api-key.create".to_string())
    );
}

#[tokio::test]
async fn org_keys_have_their_own_tab_and_minted_keys_say_so() {
    let (mut client, td) = admin_client().await;
    let root = Org::get_org_uncached(client.db(), &td.org_id)
        .await
        .unwrap();
    let (key, _) = OrgApiKey::create_for_user(
        client.db(),
        td.org_id,
        td.user_id,
        &CreateOrgApiKey {
            description: "provisioner".into(),
            permissions: json!({
                format!("org:{}/env:*/api-key:*", td.org_id): ["create"],
                format!("org:{}/env:*/event:*", td.org_id): ["create"],
            }),
            expires_at: None,
        },
    )
    .await
    .unwrap();
    let effective = key.effective(client.db()).await.unwrap();
    key.mint_env_key(
        client.db(),
        &effective,
        td.org_id,
        td.env_id,
        &MintEnvApiKey {
            description: "minted".into(),
            permissions: json!({"event:*": ["create"]}),
            expires_at: None,
        },
    )
    .await
    .unwrap();

    // The environment tab lists environment keys only, and links a minted
    // key to the org key that made it.
    let page = client.get(&in_env("/keys", &td)).await;
    page.assert_status(StatusCode::OK);
    assert!(page.body.contains("minted"), "{}", page.body);
    assert!(!page.body.contains("provisioner"), "{}", page.body);
    assert!(page.body.contains("Created by org key"), "{}", page.body);
    assert!(
        page.body
            .contains(&format!("/keys/org/{}/edit", key.org_api_key_id)),
        "{}",
        page.body
    );
    assert!(page.body.contains("href=\"/keys/org\""), "{}", page.body);

    let page = client.get(&in_env("/keys/org", &td)).await;
    page.assert_status(StatusCode::OK);
    assert!(page.body.contains("provisioner"), "{}", page.body);
    assert!(!page.body.contains("minted"), "{}", page.body);
    assert!(page.body.contains(">Environment keys<"), "{}", page.body);

    let page = client.get(&format!("/@{}/children", root.slug)).await;
    page.assert_status(StatusCode::OK);
    assert!(
        page.body
            .contains(&format!("/keys/org?org_id={}", root.org_id))
    );

    let page = client
        .get(&in_env(
            &format!("/keys/org/{}/edit", key.org_api_key_id),
            &td,
        ))
        .await;
    page.assert_status(StatusCode::OK);
    assert!(page.body.contains("provisioner"), "{}", page.body);
    // An unknown key is a 404, not a redirect to somewhere else.
    client
        .get(&in_env(&format!("/keys/org/{}/edit", Uuid::now_v7()), &td))
        .await
        .assert_status(StatusCode::NOT_FOUND);
}

/// The new-org-key page shows the secret, and the form commits its hash: the
/// secret a person copied is the one that works, and a form posted twice makes
/// one key.
#[tokio::test]
async fn a_new_org_key_works_with_the_secret_its_page_showed() {
    let (mut client, td) = admin_client().await;
    let new = in_env("/keys/org/new", &td);
    let shown = |body: &str| {
        body.split("<code id=\"generated-key\"")
            .nth(1)
            .and_then(|rest| rest.split('>').nth(1))
            .and_then(|rest| rest.split('<').next())
            .expect("the page shows the new key")
            .to_string()
    };

    // A rejected form comes back with what was entered, and a new key.
    let page = client.get(&new).await;
    page.assert_status(StatusCode::OK);
    let rejected = [
        ("org_api_key_id", field(&page.body, "org_api_key_id")),
        ("handoff_token", field(&page.body, "handoff_token")),
        ("description", "bad".to_string()),
        ("permissions", "not json".to_string()),
    ];
    let rejected: Vec<_> = rejected.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let page = client.post_form(&new, &rejected).await;
    page.assert_status(StatusCode::BAD_REQUEST);
    assert!(page.body.contains("must be a JSON object"), "{}", page.body);
    assert!(page.body.contains("not json"), "{}", page.body);
    assert!(page.body.contains("value=\"bad\""), "{}", page.body);

    // Environment-key grants aren't org-key grants.
    let unqualified = [
        ("org_api_key_id", field(&page.body, "org_api_key_id")),
        ("handoff_token", field(&page.body, "handoff_token")),
        ("description", "bad".to_string()),
        ("permissions", r#"{"run:*": ["read"]}"#.to_string()),
    ];
    let unqualified: Vec<_> = unqualified.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let page = client.post_form(&new, &unqualified).await;
    page.assert_status(StatusCode::BAD_REQUEST);
    assert!(
        page.body.contains("Unable to create the org API key"),
        "{}",
        page.body
    );

    let page = client.get(&new).await;
    let token = shown(&page.body);
    let id: Uuid = field(&page.body, "org_api_key_id").parse().unwrap();
    assert!(
        OrgApiKey::verify_token(client.db(), &token).await.is_err(),
        "nothing exists until the form is submitted"
    );
    let accepted = [
        ("org_api_key_id", id.to_string()),
        ("handoff_token", field(&page.body, "handoff_token")),
        ("description", "second".to_string()),
        ("expires_in_days", "30".to_string()),
        ("permissions", r#"{"org:*": ["read"]}"#.to_string()),
    ];
    let accepted: Vec<_> = accepted.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let list = format!("/keys/org?org_id={}", td.org_id);
    client
        .post_form(&new, &accepted)
        .await
        .assert_redirect_to(&list);
    let verified = OrgApiKey::verify_token(client.db(), &token)
        .await
        .expect("the secret the page showed works");
    assert_eq!(verified.org_api_key_id, id);
    assert!(verified.expires_at.is_some());

    // The browser's back button and a second submit change nothing.
    client
        .post_form(&new, &accepted)
        .await
        .assert_redirect_to(&list);
    let keys = OrgApiKey::list_for_org(client.db(), td.org_id)
        .await
        .unwrap();
    assert_eq!(keys.len(), 1, "one key however often the form is posted");

    // A handoff for one key doesn't create another.
    let mut forged = accepted.clone();
    let other = Uuid::now_v7().to_string();
    forged[0] = ("org_api_key_id", other.as_str());
    client
        .post_form(&new, &forged)
        .await
        .assert_status(StatusCode::BAD_REQUEST);
    assert!(
        note_types(&client, &td)
            .await
            .contains(&"org-api-key.create".to_string())
    );
}

#[tokio::test]
async fn org_keys_appear_only_for_a_parent_organization() {
    let (mut client, td) = admin_client().await;
    let child = Org::create_child(
        client.db(),
        &td.org_id,
        &td.user_id,
        &CreateChild {
            name: "Child".into(),
            slug: "child-keys".into(),
            external_id: None,
            idempotency_key: "child-keys".into(),
            hosted: false,
        },
    )
    .await
    .unwrap();
    let page = client.get(&format!("/keys?org_id={}", td.org_id)).await;
    assert!(page.body.contains("href=\"/keys/org\""), "{}", page.body);
    let in_child = format!("?org_id={}", child.org.org_id);
    let page = client.get(&format!("/keys{in_child}")).await;
    page.assert_status(StatusCode::OK);
    assert!(!page.body.contains("href=\"/keys/org\""), "{}", page.body);
    // A sub-organization has no org keys: its tab leads back to its own keys,
    // and nothing there can be created.
    let page = client.get(&format!("/keys/org{in_child}")).await;
    assert!(page.is_redirect(), "{}", page.body);
    assert!(page.location().unwrap().starts_with("/keys?"));
    client
        .get(&format!("/keys/org/new{in_child}"))
        .await
        .assert_status(StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn a_suspended_account_can_still_find_and_revoke_its_keys() {
    let (mut client, td) = admin_client().await;
    let root = Org::get_org_uncached(client.db(), &td.org_id)
        .await
        .unwrap();
    let (env_key_id, token) = env_key(&client, &td).await;
    let (org_key, _) = OrgApiKey::create_for_user(
        client.db(),
        td.org_id,
        td.user_id,
        &CreateOrgApiKey {
            description: "leaked".into(),
            permissions: json!({"org:*": ["read"]}),
            expires_at: None,
        },
    )
    .await
    .unwrap();
    Org::set_lifecycle(
        client.db(),
        &td.org_id,
        &td.user_id,
        OrgLifecycle::Suspended,
    )
    .await
    .unwrap();

    client
        .get(&in_env("/keys", &td))
        .await
        .assert_status(StatusCode::OK);
    let page = client.get(&in_env("/keys/org", &td)).await;
    page.assert_status(StatusCode::OK);
    assert!(page.body.contains("leaked"), "{}", page.body);
    client
        .get(&in_env(&format!("/keys/{env_key_id}/edit"), &td))
        .await
        .assert_status(StatusCode::OK);
    let org_key_page = in_env(&format!("/keys/org/{}/edit", org_key.org_api_key_id), &td);
    client
        .get(&org_key_page)
        .await
        .assert_status(StatusCode::OK);
    // Issuing a credential waits for reactivation.
    client
        .get(&in_env("/keys/org/new", &td))
        .await
        .assert_status(StatusCode::FORBIDDEN);
    client
        .post_form(
            &in_env(&format!("/keys/org/{}/rotate", org_key.org_api_key_id), &td),
            &[("overlap_seconds", "0")],
        )
        .await
        .assert_status(StatusCode::FORBIDDEN);

    // Rotating adds a credential, so it waits for reactivation.
    client
        .post_form(
            &in_env(&format!("/keys/{env_key_id}/rotate"), &td),
            &[("overlap_seconds", "0")],
        )
        .await
        .assert_status(StatusCode::FORBIDDEN);

    client
        .post_form(&in_env(&format!("/keys/{env_key_id}/revoke"), &td), &[])
        .await;
    assert!(
        ApiKey::verify_api_key(client.db(), &token)
            .await
            .unwrap()
            .is_none()
    );
    client
        .post_form(
            &in_env(&format!("/keys/org/{}/revoke", org_key.org_api_key_id), &td),
            &[],
        )
        .await
        .assert_redirect_to(&format!("/keys/org?org_id={}", root.org_id));
    let stored = OrgApiKey::get(client.db(), org_key.org_api_key_id)
        .await
        .unwrap();
    assert!(stored.revoked_at.is_some());
}

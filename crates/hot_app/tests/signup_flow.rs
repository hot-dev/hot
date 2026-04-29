//! Integration tests for the signup → verify → claim-handle flow.
//!
//! These tests focus on the slug-reservation logic that sits between
//! `signup_post_handler`, `verify_email_handler`, and `claim_handle_post_handler`.
//! They run against an in-memory SQLite database with the real migrations
//! applied, so we exercise the same schema production uses.
//!
//! The regression these tests protect against is:
//!   1. A user signs up with handle `alice` (row written to `email_verification`
//!      with a pending status).
//!   2. Another user creates an org with handle `alice` before the first
//!      user clicks their verification link.
//!   3. When the first user verifies, `create_org` fails with a duplicate-key
//!      violation — we must recover gracefully and suggest a unique alternative
//!      (`alice-2`, `alice-3`, …) rather than echoing the rejected slug back.

use chrono::{Duration, Utc};
use hot::db::{self, DatabasePool, EmailVerification};
use hot::val;
use hot_app::handlers::create_org;
use hot_app::slug::{self, SlugError};
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Test helpers
// ---------------------------------------------------------------------------

async fn setup_db() -> DatabasePool {
    let db_conf = val!({
        "uri": "sqlite::memory:",
        "schema": "hot"
    });
    let db = db::create_db_pool(&db_conf).await.unwrap();

    match &db {
        DatabasePool::Sqlite(pool) => {
            let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
            // crates/hot_app/ -> crates/ -> workspace root
            let migration_path = manifest_dir
                .parent()
                .unwrap()
                .parent()
                .unwrap()
                .join("resources/db/sqlite/migrations");

            let migrator = sqlx::migrate::Migrator::new(migration_path)
                .await
                .expect("Failed to create migrator");
            migrator.run(pool).await.expect("Failed to run migrations");
        }
        _ => panic!("Expected SQLite database for tests"),
    }

    db
}

async fn insert_test_user(db: &DatabasePool, email: &str) -> Uuid {
    let user_id = Uuid::now_v7();
    // `user.created_by_user_id` is NOT NULL; self-reference it for seed users.
    hot::db::User::insert_user(db, &user_id, email, Some("Test User"), Some(&user_id))
        .await
        .expect("insert_user failed");
    user_id
}

async fn insert_pending_verification(
    db: &DatabasePool,
    email: &str,
    slug: &str,
    expires_in: Duration,
) {
    let verification_id = Uuid::now_v7();
    EmailVerification::insert(
        db,
        &verification_id,
        email,
        Some("Test User"),
        "fake-password-hash",
        &format!("token-{}", verification_id),
        None,
        None,
        Some(slug),
        Some("hot-free"),
        Some("monthly"),
        Utc::now() + expires_in,
        Some("individual"),
    )
    .await
    .expect("EmailVerification::insert failed");
}

// ---------------------------------------------------------------------------
// Slug availability: existing org
// ---------------------------------------------------------------------------

#[tokio::test]
async fn slug_is_available_on_empty_db() {
    let db = setup_db().await;
    assert!(slug::is_available(&db, "alice").await);
    assert_eq!(slug::ensure_available(&db, "alice").await, Ok(()));
}

#[tokio::test]
async fn slug_unavailable_after_create_org() {
    let db = setup_db().await;
    let user_id = insert_test_user(&db, "alice@example.com").await;

    // Precondition: slug is free.
    assert!(slug::is_available(&db, "alice").await);

    // User claims it.
    create_org(&db, &user_id, "Alice", "alice", "individual")
        .await
        .expect("create_org failed");

    // Now the slug should be unavailable and ensure_available should error.
    assert!(!slug::is_available(&db, "alice").await);
    assert_eq!(
        slug::ensure_available(&db, "alice").await,
        Err(SlugError::Taken)
    );
}

// ---------------------------------------------------------------------------
// Slug availability: pending email verification (soft reservation)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn pending_verification_reserves_slug() {
    let db = setup_db().await;

    // Alice signs up but hasn't clicked the verification link yet.
    insert_pending_verification(&db, "alice@example.com", "alice", Duration::hours(24)).await;

    // Bob can't grab `alice` out from under her.
    assert!(!slug::is_available(&db, "alice").await);
    assert_eq!(
        slug::ensure_available(&db, "alice").await,
        Err(SlugError::Taken)
    );
}

#[tokio::test]
async fn expired_verification_releases_slug() {
    let db = setup_db().await;

    // Alice's signup expired without being verified.
    insert_pending_verification(&db, "alice@example.com", "alice", Duration::hours(-1)).await;

    // Slug should be available again.
    assert!(slug::is_available(&db, "alice").await);
}

// ---------------------------------------------------------------------------
// Alternative-slug suggestion on conflict
// ---------------------------------------------------------------------------

#[tokio::test]
async fn suggest_available_returns_base_when_free() {
    let db = setup_db().await;
    assert_eq!(slug::suggest_available(&db, "alice").await, "alice");
}

#[tokio::test]
async fn suggest_available_returns_dash_2_when_taken() {
    let db = setup_db().await;
    let user_id = insert_test_user(&db, "alice@example.com").await;
    create_org(&db, &user_id, "Alice", "alice", "individual")
        .await
        .unwrap();

    assert_eq!(slug::suggest_available(&db, "alice").await, "alice-2");
}

#[tokio::test]
async fn suggest_alternative_never_echoes_base() {
    let db = setup_db().await;

    // Even though `alice` is free on an empty DB, suggest_alternative is for
    // the "we just saw a duplicate-key error on `alice`" path. It must return
    // something DIFFERENT so we never show "X is taken. Try X instead."
    let alt = slug::suggest_alternative(&db, "alice").await;
    assert_ne!(alt, "alice");
    assert_eq!(alt, "alice-2");
}

#[tokio::test]
async fn suggest_available_skips_taken_variants() {
    let db = setup_db().await;
    let u1 = insert_test_user(&db, "a@example.com").await;
    let u2 = insert_test_user(&db, "b@example.com").await;
    let u3 = insert_test_user(&db, "c@example.com").await;

    // alice, alice-2, alice-3 all taken → should get alice-4.
    create_org(&db, &u1, "Alice 1", "alice", "individual")
        .await
        .unwrap();
    create_org(&db, &u2, "Alice 2", "alice-2", "individual")
        .await
        .unwrap();
    create_org(&db, &u3, "Alice 3", "alice-3", "individual")
        .await
        .unwrap();

    assert_eq!(slug::suggest_available(&db, "alice").await, "alice-4");
}

#[tokio::test]
async fn suggest_available_considers_pending_verifications() {
    let db = setup_db().await;
    let u1 = insert_test_user(&db, "a@example.com").await;

    // alice is a real org; alice-2 is soft-reserved by a pending signup.
    create_org(&db, &u1, "Alice", "alice", "individual")
        .await
        .unwrap();
    insert_pending_verification(&db, "bob@example.com", "alice-2", Duration::hours(24)).await;

    // Suggestion should skip both and land on alice-3.
    assert_eq!(slug::suggest_available(&db, "alice").await, "alice-3");
}

// ---------------------------------------------------------------------------
// End-to-end flow: race between two signups using the same handle
// ---------------------------------------------------------------------------

/// Simulates a signup race:
///
///   t0: Alice submits `/signup` with handle `alice` → pending verification row.
///   t1: Another user happens to create an org with slug `alice` (e.g. they got
///       their verification email first, or an admin provisioned it).
///   t2: Alice clicks her verification link. `create_org` fails because the
///       slug is no longer unique. `verify_email_handler` must redirect her
///       to `/claim-handle?taken=alice`, and the suggested alternative must
///       NOT be `alice`.
#[tokio::test]
async fn signup_race_produces_unique_suggestion() {
    let db = setup_db().await;

    // t0: Alice signs up.
    insert_pending_verification(&db, "alice@example.com", "alice", Duration::hours(24)).await;

    // t1: Someone else claims `alice` first.
    let other = insert_test_user(&db, "sneaky@example.com").await;
    create_org(&db, &other, "Sneaky", "alice", "individual")
        .await
        .expect("create_org failed for second user");

    // t2: Alice's verification triggers create_org, which would fail. We
    // simulate the recovery path: ask for a suggestion DIFFERENT from the
    // rejected slug.
    let suggestion = slug::suggest_alternative(&db, "alice").await;
    assert_ne!(
        suggestion, "alice",
        "suggestion must not echo the taken slug"
    );
    assert_eq!(suggestion, "alice-2");
    assert!(
        slug::is_available(&db, &suggestion).await,
        "suggestion {} should actually be available",
        suggestion
    );
}

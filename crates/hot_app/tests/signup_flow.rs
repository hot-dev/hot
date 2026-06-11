//! Integration tests for slug availability and suggestion logic used by
//! `/claim-handle` (handles are claimed post-verification, so availability
//! is purely a question of existing orgs).
//!
//! They run against an in-memory SQLite database with the real migrations
//! applied, so we exercise the same schema production uses.

use hot::db::{self, DatabasePool};
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

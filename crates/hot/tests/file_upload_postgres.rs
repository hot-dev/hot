//! Run upload retry state against Postgres when HOT_TEST_POSTGRES_URI is set.
//! Uses the same dedicated test database as the other Postgres integration tests.
use chrono::{Duration, Utc};
use hot::db::{self, DatabasePool, env::Env, file_upload};
use hot::file_storage::{FileStorage, FileStorageContext, LocalFileStorage};
use std::sync::Arc;

#[tokio::test]
async fn postgres_upload_retry_state() {
    let Ok(uri) = std::env::var("HOT_TEST_POSTGRES_URI") else {
        eprintln!("skipping: HOT_TEST_POSTGRES_URI not set");
        return;
    };
    let conf = hot::val!({"uri": uri, "schema": "hot"});
    db::run_migrations(&conf).await.unwrap();
    let db = Arc::new(db::create_db_pool(&conf).await.unwrap());
    // The test database is shared with the other Postgres integration tests,
    // so default data may already exist; seed it only when it is missing.
    let env = match Env::get_default_env(&db).await {
        Ok(env) => env,
        Err(_) => {
            // Another test may seed concurrently; its row is just as good.
            let _ = db::insert_default_data(&db).await;
            Env::get_default_env(&db).await.unwrap()
        }
    };
    let dir = tempfile::TempDir::new().unwrap();
    let storage = LocalFileStorage::new(dir.path().to_path_buf());
    let ctx = FileStorageContext::minimal(
        db.clone(),
        env.org_id,
        Some(env.env_id),
        env.created_by_user_id,
    );
    let path = format!("postgres-retry/{}.bin", uuid::Uuid::new_v4());
    let original = storage
        .write_file(&path, b"original", None, &ctx)
        .await
        .unwrap();
    let upload = file_upload::insert_upload(
        &db,
        &path,
        env.org_id,
        Some(env.env_id),
        env.created_by_user_id,
        Some(3),
        None,
        64 * 1024 * 1024,
        Some(1),
        Some("test-upload"),
        "local",
        Utc::now() + Duration::days(1),
        100,
        1024 * 1024,
    )
    .await
    .unwrap();
    let claim = async |hash: &str| {
        file_upload::reserve_part(&db, upload.upload_id, env.org_id, env.env_id, 1, hash)
            .await
            .unwrap()
    };
    use file_upload::PartReservation;
    let PartReservation::Claimed(first_owner) = claim("body-hash").await else {
        panic!("first claim");
    };
    assert!(matches!(claim("body-hash").await, PartReservation::Busy));
    assert!(matches!(
        claim("other-body").await,
        PartReservation::DifferentContent
    ));
    file_upload::release_part(&db, upload.upload_id, 1, first_owner)
        .await
        .unwrap();
    let PartReservation::Claimed(owner) = claim("body-hash").await else {
        panic!("reclaim");
    };
    let part = file_upload::PartInfo {
        part_number: 1,
        size: 3,
        etag: "opaque-token".to_string(),
    };
    assert!(
        file_upload::record_part(
            &db,
            upload.upload_id,
            env.org_id,
            env.env_id,
            &part,
            first_owner
        )
        .await
        .is_err()
    );
    file_upload::record_part(&db, upload.upload_id, env.org_id, env.env_id, &part, owner)
        .await
        .unwrap();
    let begin = async || {
        file_upload::begin_completion(
            &db,
            upload.upload_id,
            env.org_id,
            env.env_id,
            Utc::now() + Duration::hours(1),
        )
        .await
        .unwrap()
    };
    let base = begin().await;
    assert_eq!(base, original.storage_path);
    file_upload::release_completion(&db, upload.upload_id, env.org_id, env.env_id)
        .await
        .unwrap();
    storage
        .write_file(&path, b"newer", None, &ctx)
        .await
        .unwrap();
    assert_eq!(
        begin().await,
        base,
        "a released completion must retain its first base"
    );
    // The response is kept even when another completion tries to replace it.
    let result = serde_json::json!({"original-result": true});
    file_upload::complete_upload(&db, upload.upload_id, env.org_id, env.env_id, &result)
        .await
        .unwrap();
    file_upload::complete_upload(
        &db,
        upload.upload_id,
        env.org_id,
        env.env_id,
        &serde_json::json!({"wrong":true}),
    )
    .await
    .unwrap();
    let completed = file_upload::get_upload(&db, upload.upload_id, env.org_id, env.env_id)
        .await
        .unwrap();
    assert_eq!(completed.completion_result, Some(result));
    assert_eq!(completed.status, "completed");

    let upload = file_upload::insert_upload(
        &db,
        &path,
        env.org_id,
        Some(env.env_id),
        env.created_by_user_id,
        Some(3),
        None,
        64 * 1024 * 1024,
        Some(1),
        None,
        "local",
        Utc::now() + Duration::days(1),
        100,
        1024 * 1024,
    )
    .await
    .unwrap();
    file_upload::begin_abort(&db, upload.upload_id, env.org_id, env.env_id)
        .await
        .unwrap();
    assert!(
        file_upload::cleanup_expired_uploads(&db)
            .await
            .unwrap()
            .is_empty()
    );
    let DatabasePool::Postgres(pool) = db.as_ref() else {
        unreachable!();
    };
    sqlx::query("UPDATE file_upload SET expires_at = $2 WHERE upload_id = $1")
        .bind(upload.upload_id)
        .bind(Utc::now() - Duration::minutes(1))
        .execute(pool)
        .await
        .unwrap();
    let swept = file_upload::cleanup_expired_uploads(&db).await.unwrap();
    assert_eq!(swept.len(), 1);
    assert!(
        file_upload::cleanup_expired_uploads(&db)
            .await
            .unwrap()
            .is_empty()
    );
    file_upload::release_cleanup_claim(&db, upload.upload_id, env.org_id, env.env_id)
        .await
        .unwrap();
    file_upload::begin_abort(&db, upload.upload_id, env.org_id, env.env_id)
        .await
        .unwrap();
    file_upload::abort_upload(&db, upload.upload_id, env.org_id, env.env_id)
        .await
        .unwrap();
    file_upload::abort_upload(&db, upload.upload_id, env.org_id, env.env_id)
        .await
        .unwrap();
    // Keep the fixture's user-facing file tidy too.
    storage.delete_file(&path, &ctx).await.unwrap();
}

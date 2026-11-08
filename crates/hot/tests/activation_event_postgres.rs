//! Activate builds on Postgres and read back what each activation wrote.
//! Skipped unless HOT_TEST_POSTGRES_URI is set.
use hot::db::{self, Build, DatabasePool, DatabaseScope, Schedule, SchedulePolicy, ScheduleType};
use uuid::Uuid;

#[tokio::test]
async fn an_activation_writes_its_event_and_cancels_dynamic_schedules() {
    let Ok(uri) = std::env::var("HOT_TEST_POSTGRES_URI") else {
        eprintln!("skipping: HOT_TEST_POSTGRES_URI not set");
        return;
    };
    let conf = hot::val!({"uri": uri, "schema": "hot", "rls": {"enforce": false}});
    db::run_migrations(&conf).await.unwrap();
    let pool = db::create_db_pool(&conf).await.unwrap();

    let (user, org, env, project) = (
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
    );
    let onboarding = pool.with_scope(DatabaseScope::Bootstrap(db::BootstrapOperation::Onboarding));
    db::User::insert_user(
        &onboarding,
        &user,
        &format!("activation-{user}@example.test"),
        None,
        Some(&user),
    )
    .await
    .unwrap();
    db::Org::insert_org(
        &onboarding,
        &org,
        "Activation fixture",
        &format!("activation-{org}"),
        "individual",
        &user,
    )
    .await
    .unwrap();
    db::Env::insert_env(&onboarding, &env, &org, "development", &user)
        .await
        .unwrap();

    let db = pool.with_scope(DatabaseScope::Environment {
        org_id: org,
        env_id: env,
    });
    db::Project::insert_project(&db, &project, &env, "reminders", &user)
        .await
        .unwrap();
    let mut builds = Vec::new();
    for _ in 0..2 {
        let build = Uuid::now_v7();
        Build::insert_build(
            &db,
            &build,
            &project,
            "test-hash",
            1024,
            Build::BUILD_TYPE_BUNDLE,
            &user,
        )
        .await
        .unwrap();
        builds.push(build);
    }

    Build::activate_build_directly(&db, &builds[0], &user)
        .await
        .unwrap();
    let waiting = Schedule::insert_dynamic_schedule(
        &db,
        &Uuid::now_v7(),
        &builds[0],
        &ScheduleType::At(chrono::Utc::now() + chrono::Duration::hours(1)),
        SchedulePolicy::self_hosted_default(),
        "::app::jobs",
        "remind",
        None,
        None,
    )
    .await
    .unwrap();

    Build::request_bundle_deployment(&db, &builds[1], &user)
        .await
        .unwrap();
    assert!(
        Build::activate_prepared_build(&db, &builds[1], &user)
            .await
            .unwrap()
    );

    let schedule = Schedule::get_schedule(&db, &waiting).await.unwrap();
    assert!(!schedule.active);

    let DatabasePool::Postgres(scoped) = &db else {
        unreachable!();
    };
    let events: Vec<(serde_json::Value, bool, i32)> = sqlx::query_as(
        "SELECT e.event_data, e.enqueue_payload IS NOT NULL, s.total_events
         FROM event e JOIN stream s ON s.stream_id = e.stream_id
         WHERE e.env_id = $1 AND e.event_type = 'hot:deploy:activated'
         ORDER BY e.event_id",
    )
    .bind(env)
    .fetch_all(scoped)
    .await
    .unwrap();
    let expected = |build: &Uuid, cancelled: i64| {
        (
            serde_json::json!({
                "project": "reminders",
                "build-id": build.to_string(),
                "cancelled-schedules": cancelled,
            }),
            true,
            1,
        )
    };
    assert_eq!(
        events,
        vec![expected(&builds[0], 0), expected(&builds[1], 1)]
    );
}

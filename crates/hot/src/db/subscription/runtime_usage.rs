//! Usage visible to a credential bound to one environment.
use super::{DatabasePool, DateTime, OrgUsageStats, PlanError, Utc, Uuid};

impl OrgUsageStats {
    /// Environment consumption compared against shared account limits. This
    /// never returns directory seat counts or another environment's resources.
    pub async fn calculate_environment(
        db: &DatabasePool,
        org_id: &Uuid,
        env_id: &Uuid,
        period_start: DateTime<Utc>,
        retention_days: i32,
    ) -> Result<Self, PlanError> {
        let env = crate::db::Env::get_env_uncached(db, env_id)
            .await
            .map_err(|e| PlanError::InvalidOrganization(e.to_string()))?;
        if env.org_id != *org_id {
            return Err(PlanError::InvalidOrganization(
                "Environment ownership mismatch".into(),
            ));
        }
        let retention_start = Utc::now()
            .checked_sub_signed(chrono::Duration::days(i64::from(retention_days.max(0))))
            .unwrap_or(DateTime::<Utc>::MIN_UTC);
        macro_rules! calculate {
            ($pool:expr, $task_sql:literal) => {{
                let runs = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM run_admission WHERE org_id=$1 AND env_id=$2 AND admitted_at >= $3")
                    .bind(org_id).bind(env_id).bind(period_start).fetch_one($pool);
                let files = sqlx::query_scalar::<_, i64>("SELECT CAST(COALESCE(SUM(size),0) AS BIGINT) FROM file WHERE org_id=$1 AND env_id=$2 AND active=true")
                    .bind(org_id).bind(env_id).fetch_one($pool);
                let calls = sqlx::query_as::<_, (i64,i64,Option<DateTime<Utc>>)>("SELECT CAST(COALESCE(SUM(c.size),0) AS BIGINT),COUNT(*),MIN(c.start_time) FROM call c JOIN run r ON r.run_id=c.run_id WHERE r.env_id=$1 AND ($2 OR c.start_time >= $3)")
                    .bind(env_id).bind(retention_days < 0).bind(retention_start).fetch_one($pool);
                let store = sqlx::query_scalar::<_, i64>("SELECT CAST(COALESCE(SUM(size),0) AS BIGINT) FROM store_map_entry WHERE org_id=$1 AND env_id=$2")
                    .bind(org_id).bind(env_id).fetch_one($pool);
                let blobs = sqlx::query_scalar::<_, i64>("SELECT CAST(COALESCE(SUM(size),0) AS BIGINT) FROM blob_object WHERE org_id=$1 AND env_id=$2 AND status='available'")
                    .bind(org_id).bind(env_id).fetch_one($pool);
                let tasks = sqlx::query_as::<_, (i64,i64,i64)>($task_sql)
                    .bind(env_id).bind(period_start).fetch_one($pool);
                let schedules = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM schedule s JOIN build b ON b.build_id=s.build_id JOIN project p ON p.project_id=b.project_id JOIN env e ON e.env_id=p.env_id WHERE e.env_id=$1 AND e.org_id=$2 AND s.active=true AND b.active=true AND b.deployed=true AND b.runtime_status='ready' AND p.active=true AND e.active=true")
                    .bind(env_id).bind(org_id).fetch_one($pool);
                let (runs, files, calls, store, blobs, tasks, schedules) = tokio::try_join!(runs, files, calls, store, blobs, tasks, schedules)?;
                Ok(Self {
                    runs_this_period: runs,
                    file_storage_bytes: files,
                    team_members_count: 0,
                    call_storage_bytes: if retention_days == 0 { 0 } else { calls.0 },
                    call_count: if retention_days == 0 { 0 } else { calls.1 },
                    oldest_call_time: if retention_days == 0 { None } else { calls.2 },
                    store_storage_bytes: store,
                    blob_storage_bytes: blobs,
                    tasks_this_period: tasks.0,
                    task_duration_ms: tasks.1,
                    compute_units: tasks.2,
                    active_schedules: schedules,
                })
            }};
        }
        match db {
            DatabasePool::Postgres(pool) => calculate!(
                pool,
                "SELECT COUNT(*),CAST(COALESCE(SUM(t.duration_ms),0) AS BIGINT),CAST(COALESCE(SUM(u.compute_units),0) AS BIGINT) FROM task t CROSS JOIN LATERAL hot.task_usage(t) u WHERE t.env_id=$1 AND t.created_at >= $2"
            ),
            DatabasePool::Sqlite(pool) => calculate!(
                pool,
                "SELECT COUNT(*),CAST(COALESCE(SUM(t.duration_ms),0) AS BIGINT),CAST(COALESCE(SUM(u.compute_units),0) AS BIGINT) FROM task t JOIN task_usage u ON u.task_id=t.task_id WHERE t.env_id=$1 AND t.created_at >= $2"
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monthly_windows_follow_anniversaries_and_clamp_month_ends_without_drift() {
        use crate::db::subscription::monthly_usage_start;
        let parse = |s: &str| {
            chrono::DateTime::parse_from_rfc3339(s)
                .unwrap()
                .with_timezone(&chrono::Utc)
        };
        // An annual or never-renewing plan keeps its first start, and its
        // allowance still resets every month from that anniversary.
        let anchor = parse("2024-01-31T12:00:00Z");
        assert_eq!(
            monthly_usage_start(parse("2024-03-15T00:00:00Z"), Some(anchor)),
            parse("2024-02-29T12:00:00Z")
        );
        assert_eq!(
            monthly_usage_start(parse("2024-04-01T00:00:00Z"), Some(anchor)),
            parse("2024-03-31T12:00:00Z")
        );
        assert_eq!(
            monthly_usage_start(parse("2025-02-01T00:00:00Z"), Some(anchor)),
            parse("2025-01-31T12:00:00Z")
        );
        // A monthly plan's own period start is already the window.
        let monthly = parse("2024-09-15T00:00:00Z");
        assert_eq!(
            monthly_usage_start(parse("2024-10-05T00:00:00Z"), Some(monthly)),
            monthly
        );
        // No plan, or a start in the future, uses the calendar month rather
        // than moving the window (and resetting its counter) every request.
        for start in [None, Some(parse("2024-01-05T00:00:00Z"))] {
            assert_eq!(
                monthly_usage_start(parse("2024-01-04T12:00:00Z"), start),
                parse("2024-01-01T00:00:00Z")
            );
        }
    }

    #[tokio::test]
    async fn retention_keeps_data_when_the_account_cannot_be_resolved() {
        let db = crate::db::test_db().await;
        assert_eq!(
            crate::db::subscription::call_deletion_days_for_org(&db, &Uuid::new_v4()).await,
            -1
        );
    }

    #[tokio::test]
    async fn retention_keeps_data_when_override_or_next_plan_cannot_be_read() {
        let db = crate::db::test_db().await;
        let data = crate::db::insert_test_data(&db).await.unwrap();
        let DatabasePool::Sqlite(pool) = &db else {
            unreachable!()
        };
        let plan = Uuid::now_v7();
        sqlx::query("INSERT INTO plan (plan_uuid, plan_id, plan_name, base_price_monthly_cents, base_price_annual_cents, sort_order, active, features) VALUES (?, 'retention-test', 'Retention Test', 0, 0, 1000, 1, ?)")
            .bind(plan)
            .bind(serde_json::json!({"call_retention_days": 7}).to_string())
            .execute(pool)
            .await
            .unwrap();
        crate::db::OrgPlan::create(
            &db,
            &data.org_id,
            &plan,
            crate::db::BillingPeriod::Monthly,
            &data.user_id,
        )
        .await
        .unwrap();
        let days = |db| async move {
            crate::db::subscription::call_deletion_days_for_org(db, &data.org_id).await
        };
        assert!(days(&db).await > 0, "the plan sets a deletion cutoff");

        // An unreadable override must not fall through to the plan cutoff,
        // nor may one that isn't an object or a whole number of days.
        for features in [
            "{not json",
            "[]",
            r#"{"call_retention_days": "7"}"#,
            r#"{"call_retention_days": 7.5}"#,
            r#"{"call_retention_days": 4294967303}"#,
        ] {
            sqlx::query("UPDATE org SET features = ? WHERE org_id = ?")
                .bind(features)
                .bind(data.org_id)
                .execute(pool)
                .await
                .unwrap();
            assert_eq!(days(&db).await, -1, "{features}");
        }

        // A failed next-tier lookup is not the same as "no next tier".
        sqlx::query("UPDATE org SET features = NULL WHERE org_id = ?")
            .bind(data.org_id)
            .execute(pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO plan (plan_uuid, plan_id, plan_name, base_price_monthly_cents, base_price_annual_cents, sort_order, active, features) VALUES (?, 'retention-next', 'Retention Next', 0, 0, 1001, 1, '{bad')")
            .bind(Uuid::now_v7())
            .execute(pool)
            .await
            .unwrap();
        assert_eq!(days(&db).await, -1);
    }

    #[tokio::test]
    async fn environment_usage_excludes_sibling_environments_and_directory_seats() {
        let db = crate::db::test_db().await;
        let DatabasePool::Sqlite(pool) = &db else {
            unreachable!()
        };
        let org = Uuid::now_v7();
        sqlx::query("INSERT INTO org(org_id,name,slug,created_by_user_id) VALUES (?,'Org',?,?)")
            .bind(org)
            .bind(org.to_string())
            .bind(org)
            .execute(pool)
            .await
            .unwrap();
        let envs = [Uuid::now_v7(), Uuid::now_v7()];
        for (index, env) in envs.iter().enumerate() {
            sqlx::query("INSERT INTO env(env_id,org_id,name,created_by_user_id) VALUES (?,?,?,?)")
                .bind(env)
                .bind(org)
                .bind(format!("env{index}"))
                .bind(org)
                .execute(pool)
                .await
                .unwrap();
            sqlx::query("INSERT INTO run_admission(run_id,root_org_id,org_id,env_id,admitted_at) VALUES (?,?,?,?,?)")
                .bind(Uuid::now_v7()).bind(org).bind(org).bind(env).bind(Utc::now()).execute(pool).await.unwrap();
            sqlx::query("INSERT INTO store_map_entry(org_id,env_id,store_name,key,value,size) VALUES (?,?,'test','key','value',?)")
                .bind(org).bind(env).bind(if index == 0 { 10i64 } else { 999i64 }).execute(pool).await.unwrap();
        }
        sqlx::query("INSERT INTO org_user(org_user_id,org_id,user_id,org_user_role_id,created_by_user_id) VALUES (?,?,?,2,?)")
            .bind(Uuid::now_v7()).bind(org).bind(org).bind(org).execute(pool).await.unwrap();
        let usage = OrgUsageStats::calculate_environment(
            &db,
            &org,
            &envs[0],
            Utc::now() - chrono::Duration::days(1),
            -1,
        )
        .await
        .unwrap();
        assert_eq!(usage.runs_this_period, 1);
        assert_eq!(usage.store_storage_bytes, 10);
        assert_eq!(usage.team_members_count, 0);
        assert!(
            OrgUsageStats::calculate_environment(&db, &Uuid::now_v7(), &envs[0], Utc::now(), -1)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn task_usage_counts_only_metered_container_units() {
        let db = crate::db::test_db().await;
        let DatabasePool::Sqlite(pool) = &db else {
            unreachable!()
        };
        let org = Uuid::now_v7();
        let env = Uuid::now_v7();
        sqlx::query("INSERT INTO org(org_id,name,slug,created_by_user_id) VALUES (?,'Org',?,?)")
            .bind(org)
            .bind(org.to_string())
            .bind(org)
            .execute(pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO env(env_id,org_id,name,created_by_user_id) VALUES (?,?,'development',?)",
        )
        .bind(env)
        .bind(org)
        .bind(org)
        .execute(pool)
        .await
        .unwrap();
        // Code task results are user values; only container metering counts.
        for (task_type, status, result) in [
            ("code", 3, r#"{"compute-units":1000000}"#),
            ("code", 3, r#"{"compute-units":"n/a"}"#),
            ("container", 3, r#"{"compute-units":7}"#),
            ("container", 4, r#"{"$val":{"err":{"compute-units":5}}}"#),
        ] {
            sqlx::query("INSERT INTO task(task_id,env_id,stream_id,build_id,function_name,task_type,task_status_id,result,duration_ms,stop_time,created_at) VALUES (?,?,?,?,'::app/task',?,?,?,1000,?,?)")
                .bind(Uuid::now_v7()).bind(env).bind(Uuid::now_v7()).bind(Uuid::now_v7())
                .bind(task_type).bind(status).bind(result).bind(Utc::now()).bind(Utc::now())
                .execute(pool).await.unwrap();
        }
        let period = Utc::now() - chrono::Duration::days(1);
        let account = OrgUsageStats::calculate_account(&db, &org, period, -1)
            .await
            .unwrap();
        assert_eq!(account.compute_units, 12);
        assert_eq!(account.tasks_this_period, 4);
        let environment = OrgUsageStats::calculate_environment(&db, &org, &env, period, -1)
            .await
            .unwrap();
        assert_eq!(environment.compute_units, 12);
    }

    #[tokio::test]
    async fn account_run_usage_keeps_task_admissions_after_retention_and_honors_period() {
        let db = crate::db::test_db().await;
        let DatabasePool::Sqlite(pool) = &db else {
            unreachable!()
        };
        let root = Uuid::now_v7();
        let child = Uuid::now_v7();
        sqlx::query("INSERT INTO org(org_id,name,slug,org_type,created_by_user_id) VALUES (?,'Root',?,'organization',?)")
            .bind(root).bind(root.to_string()).bind(root).execute(pool).await.unwrap();
        sqlx::query("INSERT INTO org(org_id,parent_org_id,name,slug,org_type,created_by_user_id) VALUES (?,?,'Child',?,'organization',?)")
            .bind(child).bind(root).bind(child.to_string()).bind(root).execute(pool).await.unwrap();
        let period = Utc::now() - chrono::Duration::days(15);
        let root_env = Uuid::now_v7();
        let child_env = Uuid::now_v7();
        for (org, env) in [(root, root_env), (child, child_env)] {
            sqlx::query("INSERT INTO env(env_id,org_id,name,created_by_user_id) VALUES (?,?,'development',?)")
                .bind(env).bind(org).bind(root).execute(pool).await.unwrap();
            let run = Uuid::now_v7();
            sqlx::query("INSERT INTO run(run_id,env_id,stream_id,build_id,run_type_id,start_time) VALUES (?,?,?,?,7,?)")
                .bind(run).bind(env).bind(Uuid::now_v7()).bind(Uuid::now_v7()).bind(period+chrono::Duration::days(1)).execute(pool).await.unwrap();
            sqlx::query("INSERT INTO run_admission(run_id,root_org_id,org_id,env_id,admitted_at) VALUES (?,?,?,?,?)")
                .bind(run).bind(root).bind(org).bind(env).bind(period+chrono::Duration::days(1)).execute(pool).await.unwrap();
        }
        sqlx::query("INSERT INTO run_admission(run_id,root_org_id,org_id,env_id,admitted_at) VALUES (?,?,?,?,?)")
            .bind(Uuid::now_v7()).bind(root).bind(child).bind(child_env).bind(period-chrono::Duration::seconds(1)).execute(pool).await.unwrap();
        let before = OrgUsageStats::calculate_account(&db, &root, period, -1)
            .await
            .unwrap();
        assert_eq!(
            before.runs_this_period, 2,
            "task runs consume the account limit"
        );
        sqlx::query("DELETE FROM run").execute(pool).await.unwrap();
        let after = OrgUsageStats::calculate_account(&db, &child, period, -1)
            .await
            .unwrap();
        assert_eq!(
            after.runs_this_period, 2,
            "retention cannot erase account consumption"
        );
        let wider =
            OrgUsageStats::calculate_account(&db, &root, period - chrono::Duration::days(1), -1)
                .await
                .unwrap();
        assert_eq!(
            wider.runs_this_period, 3,
            "the caller's period controls the ledger window"
        );
    }
}

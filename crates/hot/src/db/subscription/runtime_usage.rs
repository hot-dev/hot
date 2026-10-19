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
                "SELECT COUNT(*),CAST(COALESCE(SUM(duration_ms),0) AS BIGINT),CAST(COALESCE(SUM(COALESCE((result->'$val'->'err'->>'compute-units')::bigint,(result->>'compute-units')::bigint,0)),0) AS BIGINT) FROM task WHERE env_id=$1 AND created_at >= $2"
            ),
            DatabasePool::Sqlite(pool) => calculate!(
                pool,
                "SELECT COUNT(*),CAST(COALESCE(SUM(duration_ms),0) AS BIGINT),CAST(COALESCE(SUM(COALESCE(CAST(json_extract(result,'$.\"$val\".err.\"compute-units\"') AS integer),CAST(json_extract(result,'$.\"compute-units\"') AS integer),0)),0) AS BIGINT) FROM task WHERE env_id=$1 AND created_at >= $2"
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

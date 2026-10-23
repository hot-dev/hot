use super::{ScheduleError, stricter_limit};
use crate::db::{DatabasePool, DatabaseScope, Features, MaintenanceOperation};
use uuid::Uuid;

/// A root lock remains held through schedule insertion or build activation.
/// Capacity is validated on the resulting deployment before commit. Suspended
/// environments/projects retain reservations until their schedules are disabled
/// or their build is undeployed, so reactivation cannot bypass the account cap.
pub(crate) enum Admission {
    Postgres {
        tx: sqlx::Transaction<'static, sqlx::Postgres>,
        root: Uuid,
        limit: i64,
    },
    Sqlite {
        tx: sqlx::Transaction<'static, sqlx::Sqlite>,
        root: Uuid,
        limit: i64,
    },
}
impl Admission {
    pub(crate) async fn begin(
        db: &DatabasePool,
        build_id: &Uuid,
        configured_limit: i64,
    ) -> Result<Self, ScheduleError> {
        let env_id = crate::db::Build::get_env_id_for_build(db, build_id)
            .await
            .map_err(|e| ScheduleError::PolicyError(e.to_string()))?;
        let env = crate::db::Env::get_env_uncached(db, &env_id)
            .await
            .map_err(|e| ScheduleError::PolicyError(e.to_string()))?;
        if let DatabasePool::Postgres(pool) = db
            && !matches!(pool.scope()?, DatabaseScope::Environment { org_id, env_id: selected } if org_id == env.org_id && selected == env_id)
        {
            return Err(ScheduleError::PolicyError(
                "Schedule mutation requires its exact environment scope".into(),
            ));
        }
        let root = crate::db::Org::billing_owner_id_for(db, &env.org_id)
            .await
            .map_err(|e| ScheduleError::PolicyError(e.to_string()))?;
        let billing = db.with_scope(DatabaseScope::Maintenance {
            operation: MaintenanceOperation::Billing,
            org_id: Some(root),
        });
        match &billing {
            DatabasePool::Postgres(pool) => {
                let mut tx = pool.begin().await?;
                sqlx::query("SELECT org_id FROM org WHERE org_id=$1 FOR UPDATE")
                    .bind(root)
                    .execute(&mut *tx)
                    .await?;
                let (plan,overrides):(Option<String>,Option<String>)=sqlx::query_as("SELECT CAST(p.features AS TEXT),CAST(o.features AS TEXT) FROM org o LEFT JOIN org_plan op ON op.org_id=o.org_id LEFT JOIN plan p ON p.plan_uuid=op.plan_uuid WHERE o.org_id=$1")
                    .bind(root).fetch_one(&mut *tx).await?;
                let limit = resolve_limit(plan, overrides, configured_limit)?;
                DatabaseScope::Environment {
                    org_id: env.org_id,
                    env_id,
                }
                .apply_to_connection(&mut tx)
                .await?;
                Ok(Self::Postgres { tx, root, limit })
            }
            DatabasePool::Sqlite(pool) => {
                let mut tx = pool.begin().await?;
                sqlx::query("UPDATE org SET org_id=org_id WHERE org_id=?")
                    .bind(root)
                    .execute(&mut *tx)
                    .await?;
                let (plan,overrides):(Option<String>,Option<String>)=sqlx::query_as("SELECT CAST(p.features AS TEXT),CAST(o.features AS TEXT) FROM org o LEFT JOIN org_plan op ON op.org_id=o.org_id LEFT JOIN plan p ON p.plan_uuid=op.plan_uuid WHERE o.org_id=?")
                    .bind(root).fetch_one(&mut *tx).await?;
                let limit = resolve_limit(plan, overrides, configured_limit)?;
                Ok(Self::Sqlite { tx, root, limit })
            }
        }
    }
}
fn resolve_limit(
    plan: Option<String>,
    overrides: Option<String>,
    configured: i64,
) -> Result<i64, ScheduleError> {
    if plan.is_none() && overrides.is_none() {
        return Ok(configured);
    }
    let plan = plan
        .map(|s| serde_json::from_str(&s))
        .transpose()
        .map_err(|_| ScheduleError::PolicyError("Malformed schedule entitlements".into()))?;
    let overrides = overrides
        .map(|s| serde_json::from_str(&s))
        .transpose()
        .map_err(|_| ScheduleError::PolicyError("Malformed schedule entitlements".into()))?;
    Ok(stricter_limit(
        configured,
        Features::resolve(plan.as_ref(), overrides.as_ref()).active_schedules_per_org(),
    ))
}
pub(crate) async fn validate_postgres(
    tx: &mut sqlx::PgConnection,
    root: Uuid,
    limit: i64,
) -> Result<(), ScheduleError> {
    DatabaseScope::Maintenance {
        operation: MaintenanceOperation::Billing,
        org_id: Some(root),
    }
    .apply_to_connection(tx)
    .await?;
    if limit < 0 {
        return Ok(());
    }
    let count:i64=sqlx::query_scalar("SELECT COUNT(*) FROM schedule s JOIN build b ON b.build_id=s.build_id JOIN project p ON p.project_id=b.project_id JOIN env e ON e.env_id=p.env_id JOIN org o ON o.org_id=e.org_id WHERE s.active AND b.deployed  AND (o.org_id=$1 OR o.parent_org_id=$1)")
        .bind(root).fetch_one(tx).await?;
    validate_count(count, limit)
}
pub(crate) async fn validate_sqlite(
    tx: &mut sqlx::SqliteConnection,
    root: Uuid,
    limit: i64,
) -> Result<(), ScheduleError> {
    if limit < 0 {
        return Ok(());
    }
    let count:i64=sqlx::query_scalar("SELECT COUNT(*) FROM schedule s JOIN build b ON b.build_id=s.build_id JOIN project p ON p.project_id=b.project_id JOIN env e ON e.env_id=p.env_id JOIN org o ON o.org_id=e.org_id WHERE s.active=1 AND b.deployed=1  AND (o.org_id=? OR o.parent_org_id=?)")
        .bind(root).bind(root).fetch_one(tx).await?;
    validate_count(count, limit)
}
fn validate_count(count: i64, limit: i64) -> Result<(), ScheduleError> {
    if count > limit {
        return Err(ScheduleError::PolicyError(format!(
            "Account active schedule limit exceeded ({count}/{limit})"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{Build, Env, Org, Project, Schedule};
    #[tokio::test]
    async fn sibling_deployments_atomically_share_schedule_capacity_and_rollback() {
        let db = crate::db::test_db().await;
        let root = Uuid::now_v7();
        let actor = Uuid::now_v7();
        Org::insert_org(
            &db,
            &root,
            "Root",
            &root.to_string(),
            "organization",
            &actor,
        )
        .await
        .unwrap();
        let DatabasePool::Sqlite(pool) = &db else {
            unreachable!()
        };
        sqlx::query("UPDATE org SET features=? WHERE org_id=?")
            .bind(r#"{"active_schedules_per_org":1}"#)
            .bind(root)
            .execute(pool)
            .await
            .unwrap();
        let mut builds = Vec::new();
        for _ in 0..2 {
            let child = Uuid::now_v7();
            let env = Uuid::now_v7();
            let project = Uuid::now_v7();
            let build = Uuid::now_v7();
            sqlx::query("INSERT INTO org (org_id,parent_org_id,name,slug,org_type,created_by_user_id) VALUES (?,?,'Child',?,'organization',?)")
                .bind(child).bind(root).bind(child.to_string()).bind(actor).execute(pool).await.unwrap();
            Env::insert_env(&db, &env, &child, "development", &actor)
                .await
                .unwrap();
            Project::insert_project(&db, &project, &env, "Project", &actor)
                .await
                .unwrap();
            Build::insert_build(
                &db,
                &build,
                &project,
                "hash",
                1,
                Build::BUILD_TYPE_BUNDLE,
                &actor,
            )
            .await
            .unwrap();
            Schedule::insert_schedule(
                &db,
                &Uuid::now_v7(),
                &build,
                "0 * * * * *",
                "app",
                "run",
                None,
                None,
                None,
                None,
                None,
                None,
            )
            .await
            .unwrap();
            builds.push(build);
        }
        let (a, b) = tokio::join!(
            Build::activate_build_directly(&db, &builds[0], &actor),
            Build::activate_build_directly(&db, &builds[1], &actor)
        );
        assert_eq!(usize::from(a.is_ok()) + usize::from(b.is_ok()), 1);
        let winner = if a.is_ok() { builds[0] } else { builds[1] };
        let loser = if a.is_ok() { builds[1] } else { builds[0] };
        assert!(!Build::get_build(&db, &loser).await.unwrap().deployed);
        assert_eq!(
            Schedule::get_active_count_by_account(&db, &root)
                .await
                .unwrap(),
            1
        );
        let id = Uuid::now_v7();
        assert!(
            Schedule::insert_schedule(
                &db,
                &id,
                &winner,
                "0 */2 * * * *",
                "app",
                "another",
                None,
                None,
                None,
                None,
                None,
                None
            )
            .await
            .is_err()
        );
        assert!(Schedule::get_schedule(&db, &id).await.is_err());
        Schedule::deactivate_schedules_by_build(&db, &winner)
            .await
            .unwrap();
        Build::activate_build_directly(&db, &loser, &actor)
            .await
            .unwrap();
        assert_eq!(
            Schedule::get_active_count_by_account(&db, &root)
                .await
                .unwrap(),
            1
        );
    }
}

//! Cancellation is a user-visible terminal state, not proof of runtime exit.
//! Reservations survive it until the owning runtime confirms termination, or
//! until long after the task's deadline when nothing shows it still running.

use super::{CONTAINER_KILL_TIMEOUT, DB_CALL_TIMEOUT, executor, shutdown};
use chrono::{DateTime, Utc};
use hot::db::{DatabasePool, DatabaseScope, Env, Task, TaskStatus};
use std::collections::HashMap;
use uuid::Uuid;

/// How long after a cancelled container's deadline (its start plus its
/// timeout) its reservation is released without proof that it stopped. A
/// working runtime stops a container at its timeout, so one that nothing
/// shows running this long after has lost its host or its worker.
const UNCONFIRMED_RELEASE_GRACE: chrono::Duration = chrono::Duration::minutes(15);

/// What a runtime says about a container.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RuntimeState {
    Stopped,
    Running,
    /// The runtime didn't answer.
    Unknown,
}

fn state(result: Result<Option<i64>, executor::ExecutorError>) -> RuntimeState {
    match result {
        Ok(Some(_)) | Err(executor::ExecutorError::ContainerNotFound(_)) => RuntimeState::Stopped,
        Ok(None) => RuntimeState::Running,
        Err(_) => RuntimeState::Unknown,
    }
}

pub(super) async fn runtime_state(
    executor: &executor::BoxExecutor,
    container_id: &str,
) -> RuntimeState {
    match tokio::time::timeout(DB_CALL_TIMEOUT, executor.inspect_status(container_id)).await {
        Ok(result) => state(result),
        Err(_) => RuntimeState::Unknown,
    }
}

pub(super) async fn runtime_stopped(executor: &executor::BoxExecutor, container_id: &str) -> bool {
    runtime_state(executor, container_id).await == RuntimeState::Stopped
}

/// How a cancelled execution's end is known.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Termination {
    /// Nothing shows it stopped: the reservation stays.
    Unconfirmed,
    /// Its runtime confirmed it stopped after the cancel.
    Confirmed,
    /// It ran to its end on its own before the cancel reached it.
    AlreadyEnded,
    /// Nothing shows it running, and its deadline is long past.
    PastDeadline,
}

/// How a cancelled container that started at `start` with `timeout_ms` is
/// known to have ended, from its runtime's `state` (`None` when no runtime
/// here knows the container).
fn termination(
    state: Option<RuntimeState>,
    start: DateTime<Utc>,
    timeout_ms: i64,
    now: DateTime<Utc>,
) -> Termination {
    let past_deadline =
        start + chrono::Duration::milliseconds(timeout_ms.max(0)) + UNCONFIRMED_RELEASE_GRACE < now;
    match state {
        Some(RuntimeState::Stopped) => Termination::Confirmed,
        Some(RuntimeState::Running) => Termination::Unconfirmed,
        Some(RuntimeState::Unknown) | None if past_deadline => Termination::PastDeadline,
        Some(RuntimeState::Unknown) | None => Termination::Unconfirmed,
    }
}

/// Settle a cancelled execution's quota. Missing measurements charge the
/// full reserved maximum, never zero.
pub(super) async fn settle(
    db: &DatabasePool,
    task_id: &Uuid,
    worker_id: &str,
    termination: Termination,
    measured: Option<(i64, i64)>,
) -> bool {
    if termination == Termination::Unconfirmed {
        tracing::warn!(%task_id, "Cancelled execution termination is unconfirmed; retaining account quota reservation");
        return false;
    }
    let reservation = match Task::quota_reservation(db, task_id).await {
        Ok(reservation) if reservation.duration_ms > 0 => reservation,
        _ => return false,
    };
    let (compute_units, duration_ms) =
        measured.unwrap_or((reservation.compute_units, reservation.duration_ms));
    if compute_units < 0 || duration_ms < 0 {
        return false;
    }
    let msg = match termination {
        Termination::Confirmed => "Task cancelled; its execution was stopped",
        Termination::AlreadyEnded => "Task cancelled after its execution had already ended",
        Termination::Unconfirmed | Termination::PastDeadline => {
            "Task cancelled; quota released after the deadline without termination evidence"
        }
    };
    let result = serde_json::json!({
        "$type": "::hot::task/Cancellation",
        "$val": {"msg": msg, "data": null},
        "compute-units": compute_units,
        "duration-ms": duration_ms,
        "quota-estimated": measured.is_none(),
        "termination-confirmed": matches!(
            termination,
            Termination::Confirmed | Termination::AlreadyEnded
        ),
    });
    match Task::complete(
        db,
        task_id,
        &TaskStatus::Cancelled,
        Some(&result),
        Some(duration_ms),
        Some(worker_id),
    )
    .await
    {
        Ok(settled) => settled,
        Err(error) => {
            tracing::warn!(%task_id, %error, "Failed to settle cancelled execution; quota reservation retained");
            false
        }
    }
}

/// Discover local containers before consulting a cancelled row. Absence on an
/// unrelated executor is not evidence that another host's process has stopped.
pub(super) async fn reap(
    db: &DatabasePool,
    executor: &executor::BoxExecutor,
    coordinator: &shutdown::TaskShutdownCoordinator,
    current_worker: &str,
) {
    let candidates = match Task::unsettled_cancelled_containers(db).await {
        Ok(tasks) => tasks,
        Err(error) => {
            tracing::warn!(%error, "Cannot discover unsettled cancelled executions");
            return;
        }
    };
    if candidates.is_empty() {
        return;
    }
    let local = match local_containers(executor).await {
        Ok(containers) => containers,
        Err(error) => {
            tracing::warn!(%error, "Cannot establish local container ownership for cancellation recovery");
            return;
        }
    };
    for task in candidates {
        if coordinator.is_task_active(&task.task_id) {
            continue;
        }
        let Some(owner) = task.worker_id.as_deref() else {
            continue;
        };
        let container_id = local.get(&task.task_id).cloned().or_else(|| {
            // Only this exact worker generation may trust a saved runtime ID
            // that has already disappeared from its local container listing.
            (owner == current_worker)
                .then(|| task.container_id.clone())
                .flatten()
        });
        let state = match container_id {
            Some(container_id) => {
                if runtime_state(executor, &container_id).await != RuntimeState::Stopped {
                    let _ = tokio::time::timeout(
                        CONTAINER_KILL_TIMEOUT,
                        executor.cleanup_orphan(&container_id),
                    )
                    .await;
                }
                Some(runtime_state(executor, &container_id).await)
            }
            // This generation ran the task, never recorded a container for
            // it, and has none labelled for it: setup failed before one
            // started, so nothing is running.
            None if owner == current_worker && task.container_id.is_none() => {
                Some(RuntimeState::Stopped)
            }
            // Another worker generation ran it: a lost host, or a restart.
            None => None,
        };
        let termination = termination(
            state,
            task.start_time.unwrap_or(task.created_at),
            task.timeout_ms,
            Utc::now(),
        );
        match termination {
            Termination::Unconfirmed if state.is_none() => {
                // The owning runtime may have settled it since this pass
                // listed it; then nothing needs recovery.
                if Task::quota_reservation(db, &task.task_id)
                    .await
                    .is_ok_and(|reservation| reservation.duration_ms == 0)
                {
                    continue;
                }
                tracing::warn!(task_id=%task.task_id, worker_id=%owner, "Cancelled quota needs recovery on its owning runtime; no local ownership evidence");
                continue;
            }
            Termination::PastDeadline => {
                tracing::warn!(task_id=%task.task_id, worker_id=%owner, "Releasing a cancelled execution's quota after its deadline without termination evidence");
            }
            Termination::Unconfirmed | Termination::Confirmed | Termination::AlreadyEnded => {}
        }
        let Ok(env) = Env::get_env_uncached(db, &task.env_id).await else {
            continue;
        };
        let exact = db.with_scope(DatabaseScope::Environment {
            org_id: env.org_id,
            env_id: task.env_id,
        });
        if settle(&exact, &task.task_id, owner, termination, None).await {
            tracing::info!(task_id=%task.task_id, "Recovered cancelled execution; charged reserved maximum and released concurrency");
        }
    }
}

/// Whether this worker's runtime has no container labelled for `task_id`.
/// Every container a worker starts carries that label, so for the worker
/// generation that ran the task, absence means none was started or it is gone.
pub(super) async fn no_local_container(executor: &executor::BoxExecutor, task_id: &Uuid) -> bool {
    local_containers(executor)
        .await
        .is_ok_and(|local| !local.contains_key(task_id))
}

async fn local_containers(
    executor: &executor::BoxExecutor,
) -> Result<HashMap<Uuid, String>, String> {
    match executor {
        executor::BoxExecutor::Docker(docker) => {
            use bollard::query_parameters::ListContainersOptionsBuilder;
            let options = ListContainersOptionsBuilder::default()
                .all(true)
                .filters(&HashMap::from([(
                    "label".to_owned(),
                    vec!["hot.dev/managed-by=hot-task-worker".to_owned()],
                )]))
                .build();
            let containers = tokio::time::timeout(
                DB_CALL_TIMEOUT,
                docker.docker.list_containers(Some(options)),
            )
            .await
            .map_err(|e| e.to_string())?
            .map_err(|e| e.to_string())?;
            Ok(containers
                .into_iter()
                .filter_map(|container| {
                    let task =
                        Uuid::parse_str(container.labels.as_ref()?.get("hot.dev/task-id")?).ok()?;
                    Some((task, container.id?))
                })
                .collect())
        }
        #[cfg(all(target_os = "linux", feature = "kata"))]
        executor::BoxExecutor::Kata(_) => {
            let containers =
                tokio::time::timeout(DB_CALL_TIMEOUT, executor.list_orphan_containers())
                    .await
                    .map_err(|e| e.to_string())?
                    .map_err(|e| e.to_string())?;
            Ok(containers
                .into_iter()
                .filter_map(|(id, task)| Some((Uuid::parse_str(task.as_deref()?).ok()?, id)))
                .collect())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    async fn claimed_cancelled() -> (DatabasePool, Uuid) {
        let db = hot::db::test_db().await;
        let id = Uuid::now_v7();
        let env = Uuid::now_v7();
        Task::insert(
            &db,
            &id,
            &env,
            &Uuid::now_v7(),
            &Uuid::now_v7(),
            None,
            "box",
            None,
            None,
            "container",
            60000,
            None,
        )
        .await
        .unwrap();
        assert!(Task::claim_for_worker(&db, &id, "owner").await.unwrap());
        let DatabasePool::Sqlite(pool) = &db else {
            unreachable!()
        };
        sqlx::query(
            "UPDATE task SET reserved_compute_units=60,reserved_duration_ms=60000 WHERE task_id=?",
        )
        .bind(id)
        .execute(pool)
        .await
        .unwrap();
        assert!(Task::cancel(&db, &id, &env).await.unwrap());
        (db, id)
    }
    #[test]
    fn only_runtime_exit_or_missing_container_confirms_termination() {
        assert_eq!(state(Ok(None)), RuntimeState::Running);
        assert_eq!(
            state(Err(executor::ExecutorError::Other(
                "daemon unavailable".into()
            ))),
            RuntimeState::Unknown
        );
        assert_eq!(state(Ok(Some(137))), RuntimeState::Stopped);
        assert_eq!(
            state(Err(executor::ExecutorError::ContainerNotFound(
                "known-local".into()
            ))),
            RuntimeState::Stopped
        );
    }

    #[test]
    fn an_unknown_container_is_released_only_long_after_its_deadline() {
        let start = Utc::now() - chrono::Duration::hours(1);
        let timeout_ms = 60_000;
        let within = start + chrono::Duration::minutes(10);
        let after = start + chrono::Duration::minutes(20);
        for state in [None, Some(RuntimeState::Unknown)] {
            assert_eq!(
                termination(state, start, timeout_ms, within),
                Termination::Unconfirmed
            );
            assert_eq!(
                termination(state, start, timeout_ms, after),
                Termination::PastDeadline
            );
        }
        // A runtime that says it runs keeps it, however late.
        assert_eq!(
            termination(Some(RuntimeState::Running), start, timeout_ms, after),
            Termination::Unconfirmed
        );
        assert_eq!(
            termination(Some(RuntimeState::Stopped), start, timeout_ms, within),
            Termination::Confirmed
        );
    }
    #[tokio::test]
    async fn termination_proof_and_owner_fence_control_cancellation_settlement() {
        let (db, id) = claimed_cancelled().await;
        assert!(
            !settle(
                &db,
                &id,
                "owner",
                Termination::Unconfirmed,
                Some((10, 10000))
            )
            .await
        );
        assert!(
            !settle(
                &db,
                &id,
                "stale-worker",
                Termination::Confirmed,
                Some((10, 10000))
            )
            .await
        );
        assert_eq!(
            Task::quota_reservation(&db, &id)
                .await
                .unwrap()
                .compute_units,
            60
        );
        assert!(settle(&db, &id, "owner", Termination::Confirmed, Some((10, 10000))).await);
        assert_eq!(
            Task::quota_reservation(&db, &id).await.unwrap().duration_ms,
            0
        );
        let task = Task::get(&db, &id).await.unwrap();
        assert_eq!(task.task_status_id, TaskStatus::Cancelled.as_id());
        assert_eq!(task.result.unwrap()["compute-units"], 10);
        assert!(!settle(&db, &id, "owner", Termination::Confirmed, Some((0, 0))).await);
    }
    #[tokio::test]
    async fn confirmed_orphan_termination_charges_reserved_maximum() {
        let (db, id) = claimed_cancelled().await;
        assert!(settle(&db, &id, "owner", Termination::Confirmed, None).await);
        let task = Task::get(&db, &id).await.unwrap();
        let result = task.result.unwrap();
        assert_eq!(result["compute-units"], 60);
        assert_eq!(result["termination-confirmed"], true);
        assert_eq!(task.duration_ms, Some(60000));
    }

    #[tokio::test]
    async fn a_release_past_the_deadline_charges_reserved_maximum() {
        let (db, id) = claimed_cancelled().await;
        assert!(settle(&db, &id, "owner", Termination::PastDeadline, None).await);
        let task = Task::get(&db, &id).await.unwrap();
        assert_eq!(task.task_status_id, TaskStatus::Cancelled.as_id());
        let result = task.result.unwrap();
        assert_eq!(result["compute-units"], 60);
        assert_eq!(result["quota-estimated"], true);
        assert_eq!(result["termination-confirmed"], false);
        assert_eq!(
            Task::quota_reservation(&db, &id).await.unwrap().duration_ms,
            0
        );
    }
    /// Run on a Docker host: cargo test -p hot_task_worker docker_cancellation_requires_exit_before_releasing_quota -- --ignored
    #[tokio::test]
    #[ignore = "requires local Docker and may pull alpine:3.21"]
    async fn docker_cancellation_requires_exit_before_releasing_quota() {
        let executor = executor::BoxExecutor::new(executor::Backend::Docker, 1, 5, None, None)
            .await
            .unwrap();
        let (db, id) = claimed_cancelled().await;
        let mut timings = executor::ContainerTimings::default();
        let container = tokio::time::timeout(
            std::time::Duration::from_secs(60),
            executor.create_and_start(
                "alpine:3.21",
                Some(vec!["sleep".into(), "60".into()]),
                None,
                Some(&id.to_string()),
                None,
                None,
                &mut timings,
            ),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(!runtime_stopped(&executor, &container).await);
        assert!(
            !settle(
                &db,
                &id,
                "owner",
                if runtime_stopped(&executor, &container).await {
                    Termination::Confirmed
                } else {
                    Termination::Unconfirmed
                },
                None
            )
            .await
        );
        assert_eq!(
            Task::quota_reservation(&db, &id)
                .await
                .unwrap()
                .compute_units,
            60
        );
        tokio::time::timeout(CONTAINER_KILL_TIMEOUT, executor.kill_and_remove(&container))
            .await
            .unwrap();
        assert!(runtime_stopped(&executor, &container).await);
        assert!(
            settle(
                &db,
                &id,
                "owner",
                if runtime_stopped(&executor, &container).await {
                    Termination::Confirmed
                } else {
                    Termination::Unconfirmed
                },
                None
            )
            .await
        );
        assert_eq!(
            Task::quota_reservation(&db, &id).await.unwrap().duration_ms,
            0
        );
        assert_eq!(
            Task::get(&db, &id).await.unwrap().result.unwrap()["compute-units"],
            60
        );
    }
}

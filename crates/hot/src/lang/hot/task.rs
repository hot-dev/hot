//! Task module - long-running execution primitives
//!
//! Provides `::hot::task/start`, `::hot::task/send`, and `::hot::task/receive`
//! for spawning and communicating with long-running tasks from Hot Runs.

use crate::lang::hot::r#type::HotResult;
use crate::lang::runtime::function_ref::extract_function_ref;
use crate::lang::runtime::vm::VirtualMachine;
use crate::queue::Queue;
use crate::stream::StreamPublisher;
use crate::val::Val;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Native task primitives run on a blocking VM thread. Every async DB/queue
/// operation they bridge into must have a short wall-clock ceiling so the VM's
/// cooperative cancellation can regain control before the worker hard timeout.
const TASK_NATIVE_IO_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
const PARENT_COMPLETION_RESERVE: std::time::Duration = std::time::Duration::from_secs(5);

fn initial_await_lookup_budget(
    execution_context: Option<&crate::lang::event::ExecutionContext>,
) -> Option<std::time::Duration> {
    match execution_context {
        Some(context) => {
            context.cap_wait_to_deadline(TASK_NATIVE_IO_TIMEOUT, PARENT_COMPLETION_RESERVE)
        }
        None => Some(TASK_NATIVE_IO_TIMEOUT),
    }
}

/// The ceiling for writing `size` bytes inline. A large value takes the database
/// longer, but cancellation must still regain control before the worker's hard
/// timeout, so it's never past the run's deadline, or below the usual ceiling.
fn inline_write_timeout(
    context: &crate::lang::event::ExecutionContext,
    size: usize,
) -> std::time::Duration {
    let wanted = crate::blob::timeout_for_size(TASK_NATIVE_IO_TIMEOUT, size);
    context
        .cap_wait_to_deadline(wanted, PARENT_COMPLETION_RESERVE)
        .unwrap_or(TASK_NATIVE_IO_TIMEOUT)
        .max(TASK_NATIVE_IO_TIMEOUT)
}

fn err_val(msg: String) -> Val {
    Val::err(Val::from(msg))
}

/// The environment the calling code runs in. A task ID only names a task
/// within it: RLS doesn't cover SQLite or pub/sub, so every native that
/// takes a task ID checks ownership against this env itself.
fn caller_env_id(vm: &VirtualMachine, native: &str) -> Result<Uuid, Val> {
    vm.get_execution_context()
        .as_ref()
        .and_then(|ctx| ctx.env_id)
        .ok_or_else(|| err_val(format!("{native}: no env_id in execution context")))
}

// ---------------------------------------------------------------------------
// TaskRequest — serialized into the task queue for the hot_task_worker
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskRequest {
    pub task_id: String,
    pub function_name: String,
    pub args: serde_json::Value,
    pub stream_id: String,
    pub env_id: String,
    pub build_id: String,
    pub org_id: Option<String>,
    pub user_id: Option<String>,
    pub project_id: Option<String>,
    pub project_name: Option<String>,
    pub timeout_ms: u64,
    pub task_type: String,
    pub created_at_unix_ms: u64,
    #[serde(default)]
    pub origin_run_id: Option<String>,
}

// ---------------------------------------------------------------------------
// ::hot::task/start — spawn a long-running task from a Run
// ---------------------------------------------------------------------------

/// Start a new task.
///
/// # Arguments
/// * `task-fn` - Function reference (Fn) to execute as the task
/// * `args`    - Arguments to pass to the task function (must be serializable data)
///
/// # Returns
/// `TaskInfo { id: Str, stream-id: Str }`
pub fn start(vm: &mut VirtualMachine, args: &[Val]) -> HotResult<Val> {
    if args.is_empty() || args.len() > 3 {
        return HotResult::Err(err_val(
            "::hot::task/start: expected 1-3 args (task-fn [, args [, options]])".to_string(),
        ));
    }
    if let Err(error) = vm.require_work_admission() {
        return HotResult::Err(err_val(error));
    }

    // Extract function qualified name
    let function_name = match extract_function_name("::hot::task/start", &args[0]) {
        Ok(name) => name,
        Err(e) => return HotResult::Err(e),
    };

    // Extract args (default to null)
    let task_args = if args.len() > 1 {
        args[1].clone()
    } else {
        Val::Null
    };

    // Extract options (default empty)
    let options = if args.len() > 2 {
        match &args[2] {
            Val::Map(_) => args[2].clone(),
            Val::Null => Val::map_empty(),
            _ => {
                return HotResult::Err(err_val(
                    "::hot::task/start: options must be a Map".to_string(),
                ));
            }
        }
    } else {
        Val::map_empty()
    };

    let timeout_ms: u64 = match options.get("timeout") {
        Some(Val::Int(n)) => (n as u64).clamp(1000, 604_800_000), // 1s to 7 days
        _ => 1_800_000,                                           // 30 minutes default
    };

    let task_type = match options.get("type") {
        Some(Val::Str(s)) => s.to_string(),
        _ => "code".to_string(),
    };
    // A container task runs an image, not `task-fn`. Only `::hot::box/start`
    // creates one, after checking that Hot Box is enabled.
    if task_type.eq_ignore_ascii_case("container") {
        return HotResult::Err(err_val(format!(
            "::hot::task/start: type \"container\" is reserved; start containers with {}",
            crate::lang::hot::r#box::START_FUNCTION
        )));
    }

    // Get execution context
    let ctx = match vm.get_execution_context() {
        Some(ctx) => ctx.clone(),
        None => {
            return HotResult::Err(err_val(
                "::hot::task/start: no execution context (not running in a run)".to_string(),
            ));
        }
    };

    let env_id = match ctx.env_id {
        Some(id) => id,
        None => {
            return HotResult::Err(err_val(
                "::hot::task/start: no env_id in execution context".to_string(),
            ));
        }
    };

    let build_id = match ctx.build_id {
        Some(id) => id,
        None => {
            return HotResult::Err(err_val(
                "::hot::task/start: no build_id in execution context".to_string(),
            ));
        }
    };

    let stream_id = ctx.stream_id;
    if stream_id == Uuid::nil() {
        return HotResult::Err(err_val(
            "::hot::task/start: no stream_id in execution context".to_string(),
        ));
    }

    crate::validate_record_expansion!("::hot::task/start", &[task_args.clone(), options.clone()]);
    // Serialize task args and options to JSON
    let args_json: serde_json::Value = (&task_args).into();
    let options_json: serde_json::Value = (&options).into();
    let options_json_opt =
        if options_json.is_null() || options_json.as_object().is_some_and(|m| m.is_empty()) {
            None
        } else {
            Some(options_json)
        };

    let task_id = Uuid::now_v7();
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;

    let mut request = TaskRequest {
        task_id: task_id.to_string(),
        function_name: function_name.clone(),
        args: serde_json::Value::Null,
        stream_id: stream_id.to_string(),
        env_id: env_id.to_string(),
        build_id: build_id.to_string(),
        org_id: ctx.org_id.map(|id| id.to_string()),
        user_id: ctx.user_id.map(|id| id.to_string()),
        project_id: ctx.project_id.map(|id| id.to_string()),
        project_name: ctx.project_name.clone(),
        timeout_ms,
        task_type: task_type.clone(),
        created_at_unix_ms: now_ms,
        origin_run_id: Some(ctx.run_id.to_string()),
    };

    // Insert task row into database and enqueue to task queue
    let db = vm.get_database_pool();
    let task_queue = vm.get_task_queue();
    let vm_conf = vm.get_conf();

    let start_result = tokio::runtime::Handle::current().block_on(async {
        let db =
            db.ok_or_else(|| err_val("::hot::task/start: no database configured".to_string()))?;
        let queue = task_queue
            .ok_or_else(|| err_val("::hot::task/start: no task queue configured".to_string()))?;

        // The emitter/writer pipeline is async, so run:start may not be
        // committed yet. Bound this bridge just like the task insert itself.
        match tokio::time::timeout(
            TASK_NATIVE_IO_TIMEOUT,
            crate::db::run::Run::ensure_run_exists(
                &db,
                &ctx.run_id,
                &env_id,
                &ctx.stream_id,
                ctx.build_id.as_ref(),
                ctx.run_type_id,
                ctx.origin_run_id.as_ref(),
                ctx.user_id.as_ref().unwrap_or(&Uuid::nil()),
                ctx.access_id.as_ref(),
            ),
        )
        .await
        {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                return Err(err_val(format!(
                    "::hot::task/start: failed to create origin run: {}",
                    e
                )));
            }
            Err(_) => {
                return Err(err_val(format!(
                    "::hot::task/start: origin run write timed out after {}s",
                    TASK_NATIVE_IO_TIMEOUT.as_secs()
                )));
            }
        }

        // Too large to store inline: both the row and the queue message hold a
        // reference, which the task worker reads back. Not bound by the
        // timeout, as it's as long as the value takes to write.
        let might_spill = crate::blob::estimate_val_size(&task_args)
            > crate::blob::BlobConfig::from_conf(vm_conf).max_inline_bytes
            || crate::blob::is_blob_ref_val(&task_args);
        let blob_store = match ctx.org_id.filter(|_| might_spill) {
            Some(org_id) => crate::blob::blob_store_from_conf(std::sync::Arc::clone(&db), vm_conf)
                .await
                .map(|store| (store, org_id)),
            None => None,
        };
        let args_scope = |org_id| crate::blob::BlobScope {
            org_id,
            env_id: Some(env_id),
            run_id: Some(ctx.run_id),
        };
        let args_json = match &blob_store {
            Some((store, org_id)) => store
                .spill_whole_if_large(
                    args_json,
                    args_scope(*org_id),
                    crate::blob::SpillSource::TaskArgs,
                    Some(&task_id.to_string()),
                )
                .await
                .map_err(|e| {
                    err_val(format!(
                        "::hot::task/start: failed to store arguments in blob storage: {}",
                        e
                    ))
                })?,
            None => args_json,
        };
        request.args = args_json.clone();
        let inline_size = if crate::store::spill::is_spilled(&args_json) {
            0
        } else {
            crate::blob::estimate_val_size(&task_args)
        };
        let write_timeout = inline_write_timeout(&ctx, inline_size);
        let args_json_opt = if args_json.is_null() {
            None
        } else {
            Some(&args_json)
        };
        match tokio::time::timeout(
            write_timeout,
            crate::db::Task::insert(
                &db,
                &task_id,
                &env_id,
                &stream_id,
                &build_id,
                Some(&ctx.run_id),
                &function_name,
                args_json_opt,
                options_json_opt.as_ref(),
                &task_type,
                timeout_ms as i64,
                ctx.user_id.as_ref(),
            ),
        )
        .await
        {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                // Unless the row made it anyway, as when a connection fails
                // after a commit, nothing will read the arguments.
                if let Some((store, org_id)) = &blob_store
                    && matches!(
                        crate::db::Task::get(&db, &task_id).await,
                        Err(crate::db::TaskError::NotFound)
                    )
                    && let Err(e) = store
                        .release_source(
                            args_scope(*org_id),
                            crate::blob::SpillSource::TaskArgs,
                            &task_id.to_string(),
                        )
                        .await
                {
                    tracing::warn!(task_id = %task_id, error = %e, "failed to release an unstored task's arguments");
                }
                return Err(err_val(format!(
                    "::hot::task/start: failed to insert task: {}",
                    e
                )));
            }
            Err(_) => {
                return Err(err_val(format!(
                    "::hot::task/start: task insert timed out after {}s",
                    write_timeout.as_secs()
                )));
            }
        }

        match tokio::time::timeout(write_timeout, queue.enqueue(request)).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => {
                let failure = serde_json::json!({
                    "$type": "::hot::run/Failure",
                    "$val": {"msg": format!("Failed to enqueue task: {}", e)}
                });
                fail_task_after_enqueue_error(
                    &db,
                    &task_id,
                    "::hot::task/start",
                    &failure,
                    TASK_NATIVE_IO_TIMEOUT,
                )
                .await;
                Err(err_val(format!(
                    "::hot::task/start: failed to enqueue task: {}",
                    e
                )))
            }
            Err(_) => {
                let failure = serde_json::json!({
                    "$type": "::hot::run/Failure",
                    "$val": {"msg": "Timed out enqueueing task"}
                });
                fail_task_after_enqueue_error(
                    &db,
                    &task_id,
                    "::hot::task/start",
                    &failure,
                    TASK_NATIVE_IO_TIMEOUT,
                )
                .await;
                Err(err_val(format!(
                    "::hot::task/start: queue write timed out after {}s",
                    write_timeout.as_secs()
                )))
            }
        }
    });
    if let Err(e) = start_result {
        return HotResult::Err(e);
    }

    // Build typed Stream value
    let stream_val = crate::val!({
        "$type": "::hot::stream/Stream",
        "$val": {
            "id": stream_id.to_string()
        }
    });

    // Build typed Run value for the origin run
    let start_time = vm.get_run_start_time();
    let start_time_val = crate::val!({
        "$type": "::hot::time/Instant",
        "$val": {
            "epochNanoseconds": start_time.timestamp_nanos_opt().unwrap_or(0)
        }
    });
    let run_type_str = match ctx.run_type_id {
        1 => "call",
        2 => "event",
        3 => "schedule",
        4 => "run",
        5 => "eval",
        6 => "repl",
        _ => "unknown",
    };
    let origin_run_id_val = match ctx.origin_run_id {
        Some(id) => Val::from(id.to_string()),
        None => Val::Null,
    };
    let origin_run_val = crate::val!({
        "$type": "::hot::run/Run",
        "$val": {
            "id": ctx.run_id.to_string(),
            "type": run_type_str,
            "status": "running",
            "start-time": start_time_val,
            "retry-attempt": ctx.retry_attempt as i64,
            "max-retries": ctx.max_retries as i64,
            "retry-delay": ctx.retry_delay_ms as i64,
            "origin-run-id": origin_run_id_val
        }
    });

    // Return TaskInfo
    let result = crate::val!({
        "$type": "::hot::task/TaskInfo",
        "$val": {
            "id": task_id.to_string(),
            "stream": stream_val,
            "origin-run": origin_run_val,
            "data": Val::Null
        }
    });

    HotResult::Ok(result)
}

/// A task's result stored in blob storage, for the caller's org and the task's
/// environment.
fn read_blob_result(vm: &VirtualMachine, result: Val, env_id: Uuid) -> Result<Val, String> {
    let db = vm
        .get_database_pool()
        .ok_or_else(|| "no database configured".to_string())?;
    let org_id = vm
        .get_execution_context()
        .as_ref()
        .and_then(|ctx| ctx.org_id)
        .ok_or_else(|| "no org_id in execution context".to_string())?;
    let conf = vm.get_conf();
    tokio::runtime::Handle::current().block_on(async {
        let store = crate::blob::blob_store_from_conf(db, conf).await;
        let scope = crate::blob::BlobScope {
            org_id,
            env_id: Some(env_id),
            run_id: None,
        };
        crate::blob::rehydrate_whole(store.as_deref(), result, scope)
            .await
            .map_err(|e| e.to_string())
    })
}

// ---------------------------------------------------------------------------
// ::hot::task/send — send data to a running task
// ---------------------------------------------------------------------------

/// Send data to a running task.
///
/// Publishes a message to the task's Redis stream channel (`hot:task:{task_id}`).
/// The task worker forwards it to the task's receive channel.
/// If the task failed and was retried, the message goes to the newest retry.
///
/// # Arguments
/// * `task-id` - Task ID string
/// * `data`    - Any serializable data
///
/// # Returns
/// `true` on success
pub fn send(vm: &mut VirtualMachine, args: &[Val]) -> HotResult<Val> {
    if args.len() != 2 {
        return HotResult::Err(err_val(
            "::hot::task/send: expected 2 args (task-id, data)".to_string(),
        ));
    }

    let task_id = match &args[0] {
        Val::Str(s) => match Uuid::parse_str(s) {
            Ok(id) => id,
            Err(_) => {
                return HotResult::Err(err_val(
                    "::hot::task/send: invalid task-id UUID".to_string(),
                ));
            }
        },
        _ => {
            return HotResult::Err(err_val(
                "::hot::task/send: task-id must be a string".to_string(),
            ));
        }
    };

    let env_id = match caller_env_id(vm, "::hot::task/send") {
        Ok(id) => id,
        Err(e) => return HotResult::Err(e),
    };

    let data = &args[1];
    crate::validate_record_expansion!("::hot::task/send", std::slice::from_ref(data));
    let payload_json: serde_json::Value = data.into();

    let Some(db) = vm.get_database_pool() else {
        return HotResult::Err(err_val(
            "::hot::task/send: no database available".to_string(),
        ));
    };
    let Some(pub_sub) = vm.get_stream_publisher() else {
        return HotResult::Err(err_val(
            "::hot::task/send: no stream publisher available".to_string(),
        ));
    };

    let sent = tokio::runtime::Handle::current().block_on(async {
        // Only a task in the caller's own environment may receive the message.
        let task_id = match tokio::time::timeout(
            TASK_NATIVE_IO_TIMEOUT,
            crate::db::Task::get_in_env(&db, &task_id, &env_id),
        )
        .await
        {
            // A retried task runs under a new id: message the newest attempt.
            Ok(Ok(task)) => newest_retry_attempt(&db, task, env_id).await.task_id,
            Ok(Err(crate::db::TaskError::NotFound)) => {
                return Err(err_val(format!(
                    "::hot::task/send: task not found: {}",
                    task_id
                )));
            }
            Ok(Err(e)) => {
                return Err(err_val(format!(
                    "::hot::task/send: task lookup failed: {}",
                    e
                )));
            }
            Err(_) => {
                return Err(err_val(format!(
                    "::hot::task/send: database read timed out after {}s",
                    TASK_NATIVE_IO_TIMEOUT.as_secs()
                )));
            }
        };

        let event = crate::stream::StreamEvent::TaskMessage {
            task_id: task_id.to_string(),
            env_id: Some(env_id),
            payload: payload_json,
        };
        match tokio::time::timeout(TASK_NATIVE_IO_TIMEOUT, pub_sub.publish(event)).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(err_val(format!(
                "::hot::task/send: failed to publish: {}",
                e
            ))),
            Err(_) => Err(err_val(format!(
                "::hot::task/send: publish timed out after {}s",
                TASK_NATIVE_IO_TIMEOUT.as_secs()
            ))),
        }
    });

    match sent {
        Ok(()) => HotResult::Ok(Val::from(true)),
        Err(e) => HotResult::Err(e),
    }
}

// ---------------------------------------------------------------------------
// ::hot::task/receive — receive data sent to the current task
// ---------------------------------------------------------------------------

/// Receive the next message sent to this task.
///
/// Blocks until a message arrives or the task is shutting down (returns null).
/// Only callable from within a task (errors otherwise).
///
/// # Returns
/// The next message value, or `null` on shutdown/close.
pub fn receive(vm: &mut VirtualMachine, args: &[Val]) -> HotResult<Val> {
    let _parked = match super::native_io::park_task_wait() {
        Ok(parked) => parked,
        Err(error) => return HotResult::Err(error),
    };
    if !args.is_empty() {
        return HotResult::Err(err_val("::hot::task/receive: expected 0 args".to_string()));
    }

    // Capture the cancellation token so a blocked receive releases the VM thread
    // on worker shutdown / run timeout instead of blocking forever.
    let cancel_token = vm.external_cancel_token();
    let receiver = vm.get_task_receiver();
    match receiver {
        Some(rx) => {
            // parking_lot::Mutex::lock() never returns an error (no poisoning)
            let mut guard = rx.lock();
            let is_cancelled = || {
                cancel_token
                    .as_ref()
                    .map(|flag| flag.load(std::sync::atomic::Ordering::Relaxed))
                    .unwrap_or(false)
            };
            // Race the channel receive against periodic cancellation polling. A
            // delivered message (including a cooperative cancel message published
            // by `::hot::task/cancel`) returns immediately; a closed channel or an
            // external cancel resolves to Null (treated as shutdown by callers).
            let received = tokio::runtime::Handle::current().block_on(async {
                loop {
                    tokio::select! {
                        msg = guard.recv() => return msg,
                        _ = tokio::time::sleep(std::time::Duration::from_millis(250)) => {
                            if is_cancelled() {
                                return None;
                            }
                        }
                    }
                }
            });
            match received {
                Some(val) => HotResult::Ok(val),
                None => HotResult::Ok(Val::Null), // channel closed / cancelled = shutdown
            }
        }
        None => HotResult::Err(err_val(
            "::hot::task/receive: not running inside a task".to_string(),
        )),
    }
}

// ---------------------------------------------------------------------------
// ::hot::task/cancel — cancel a queued or running task
// ---------------------------------------------------------------------------

/// Cancel a task by ID.
///
/// Sets the task status to 'cancelled' if it is still queued or running.
/// For in-flight tasks, also publishes a cancellation message via pub/sub
/// so the task can cooperatively exit via `::hot::task/receive`.
/// If the task failed and was retried, the newest retry is the one cancelled.
///
/// # Arguments
/// * `task-id` - Task ID string
///
/// # Returns
/// `true` if the task was cancelled, `false` if already in a terminal state.
pub fn cancel(vm: &mut VirtualMachine, args: &[Val]) -> HotResult<Val> {
    if args.len() != 1 {
        return HotResult::Err(err_val(
            "::hot::task/cancel: expected 1 arg (task-id)".to_string(),
        ));
    }

    let task_id_str = match &args[0] {
        Val::Str(s) => s.to_string(),
        _ => {
            return HotResult::Err(err_val(
                "::hot::task/cancel: task-id must be a string".to_string(),
            ));
        }
    };

    let task_id = match Uuid::parse_str(&task_id_str) {
        Ok(id) => id,
        Err(_) => {
            return HotResult::Err(err_val(
                "::hot::task/cancel: invalid task-id UUID".to_string(),
            ));
        }
    };

    let env_id = match caller_env_id(vm, "::hot::task/cancel") {
        Ok(id) => id,
        Err(e) => return HotResult::Err(e),
    };

    let db = vm.get_database_pool();
    let publisher = vm.get_stream_publisher();

    let cancelled = tokio::runtime::Handle::current().block_on(async {
        // A retried task runs under a new id: cancel the newest attempt.
        let (task_id, was_cancelled) = if let Some(db) = &db {
            cancel_newest_attempt(db, task_id, env_id).await?
        } else {
            return Err(err_val(
                "::hot::task/cancel: no database available".to_string(),
            ));
        };

        if was_cancelled
            && let Some(pub_sub) = &publisher
        {
            let event = crate::stream::StreamEvent::TaskMessage {
                task_id: task_id.to_string(),
                env_id: Some(env_id),
                payload: serde_json::json!({"$cancel": true}),
            };
            match tokio::time::timeout(TASK_NATIVE_IO_TIMEOUT, pub_sub.publish(event)).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    tracing::warn!(task_id = %task_id, "::hot::task/cancel: failed to publish cancel message: {}", e);
                }
                Err(_) => {
                    tracing::warn!(task_id = %task_id, "::hot::task/cancel: cancel publish timed out");
                }
            }
        }

        Ok::<bool, Val>(was_cancelled)
    });

    match cancelled {
        Ok(cancelled) => HotResult::Ok(Val::from(cancelled)),
        Err(e) => HotResult::Err(e),
    }
}

// ---------------------------------------------------------------------------
// ::hot::task/await — wait for a task to complete and return full result
// ---------------------------------------------------------------------------

/// Wait for a task to complete and return its full result including CUS and timing.
///
/// Polls Task::get every 5% of the time waited so far, no less than 100ms
/// and no more than 5s apart (see `await_poll_delay_ms`).
/// Stops when the task reaches a terminal status (completed, failed, timed_out, cancelled).
/// A failed or timed-out attempt that was retried is followed to its retry,
/// so the result is the outcome of the last attempt in the lineage.
/// Returns a TaskResult with all execution details; `id` stays the awaited id.
///
/// # Arguments
/// * `task-id` - Task ID string (UUID)
///
/// # Returns
/// `TaskResult` with id, status, timing breakdown, CUS, and result.
pub fn await_task(vm: &mut VirtualMachine, args: &[Val]) -> HotResult<Val> {
    let _parked = match super::native_io::park_task_wait() {
        Ok(parked) => parked,
        Err(error) => return HotResult::Err(error),
    };
    if args.len() != 1 {
        return HotResult::Err(err_val(
            "::hot::task/await: expected 1 arg (task-id)".to_string(),
        ));
    }

    let task_id_str = match &args[0] {
        Val::Str(s) => s.to_string(),
        _ => {
            return HotResult::Err(err_val(
                "::hot::task/await: task-id must be a string".to_string(),
            ));
        }
    };

    let task_id = match Uuid::parse_str(&task_id_str) {
        Ok(id) => id,
        Err(_) => {
            return HotResult::Err(err_val(
                "::hot::task/await: invalid task-id UUID".to_string(),
            ));
        }
    };

    let db = match vm.get_database_pool() {
        Some(pool) => pool,
        None => {
            return HotResult::Err(err_val(
                "::hot::task/await: no database available".to_string(),
            ));
        }
    };

    let env_id = match caller_env_id(vm, "::hot::task/await") {
        Ok(id) => id,
        Err(e) => return HotResult::Err(e),
    };

    let execution_context = vm.get_execution_context().clone();

    // Capture the cancellation token so the poll loop can release the blocking
    // VM thread promptly on worker shutdown or run timeout instead of pinning it
    // for up to the task's (potentially 24h) timeout.
    let cancel_token = vm.external_cancel_token();
    let is_cancelled = move || {
        cancel_token
            .as_ref()
            .map(|flag| flag.load(std::sync::atomic::Ordering::Relaxed))
            .unwrap_or(false)
    };

    let task = tokio::runtime::Handle::current().block_on(async {
        const MAX_TIMEOUT_MS: i64 = 24 * 60 * 60 * 1000; // 24 hours

        // Preserve the finalization reserve during the initial lookup too. A
        // fixed five-second read here could otherwise consume the entire
        // reserve before the polling loop even began.
        let lookup_budget = match initial_await_lookup_budget(execution_context.as_ref()) {
            Some(budget) => budget,
            None => {
                return Err(err_val(
                    "::hot::task/await: parent execution deadline is too close to look up task"
                        .to_string(),
                ));
            }
        };

        let task = match tokio::time::timeout(
            lookup_budget,
            crate::db::Task::get_in_env(&db, &task_id, &env_id),
        )
        .await
        {
            Ok(Ok(t)) => t,
            Ok(Err(crate::db::TaskError::NotFound)) => {
                return Err(err_val(format!(
                    "::hot::task/await: task not found: {}",
                    task_id
                )));
            }
            Ok(Err(e)) => {
                return Err(err_val(format!(
                    "::hot::task/await: task lookup failed: {}",
                    e
                )));
            }
            Err(_) => {
                return Err(err_val(format!(
                    "::hot::task/await: database read timed out after {}ms",
                    lookup_budget.as_millis()
                )));
            }
        };

        let timeout_ms = task.timeout_ms.clamp(1000, MAX_TIMEOUT_MS) as u64;
        let requested_wait = std::time::Duration::from_millis(timeout_ms);
        let wait_budget = match execution_context.as_ref() {
            Some(ctx) => {
                match ctx.cap_wait_to_deadline(requested_wait, PARENT_COMPLETION_RESERVE) {
                    Some(budget) => budget,
                    None => {
                        return Err(err_val(
                        "::hot::task/await: parent execution deadline is too close to wait for task"
                            .to_string(),
                    ));
                    }
                }
            }
            None => requested_wait,
        };
        let deadline = tokio::time::Instant::now() + wait_budget;

        let db = &db;
        // A retry is a fresh attempt with its own retry delay and timeout, so
        // following one extends the wait, still inside the parent's deadline.
        let retry_deadline = |retry: &crate::db::Task| {
            let delay = retry
                .next_retry_at
                .and_then(|at| (at - chrono::Utc::now()).to_std().ok())
                .unwrap_or_default();
            let requested = delay
                + std::time::Duration::from_millis(
                    retry.timeout_ms.clamp(1000, MAX_TIMEOUT_MS) as u64
                );
            let budget = match execution_context.as_ref() {
                Some(ctx) => ctx.cap_wait_to_deadline(requested, PARENT_COMPLETION_RESERVE)?,
                None => requested,
            };
            Some(tokio::time::Instant::now() + budget)
        };
        await_task_lineage(
            task,
            deadline,
            &is_cancelled,
            move |id: Uuid, read_timeout| async move {
                match tokio::time::timeout(
                    read_timeout,
                    crate::db::Task::get_in_env(db, &id, &env_id),
                )
                .await
                {
                    Ok(Ok(t)) => Ok(t),
                    Ok(Err(crate::db::TaskError::NotFound)) => Err(PollFailure::NotFound),
                    Ok(Err(e)) => Err(PollFailure::Transport(e.to_string())),
                    Err(_) => Err(PollFailure::TimedOut(read_timeout)),
                }
            },
            move |parent: Uuid, read_timeout| lookup_retry_child(db, parent, env_id, read_timeout),
            &retry_deadline,
        )
        .await
    });

    let task = match task {
        Ok(t) => t,
        Err(e) => return HotResult::Err(e),
    };

    // Build TaskResult from task + parsed result JSON.
    // For container tasks: result has exit-code, stdout, etc. directly, or in $val.err for Failure.
    // For code tasks: result is the return value.
    let result_json: Option<&serde_json::Value> = task.result.as_ref();
    let container_payload: Option<serde_json::Value> =
        result_json.and_then(|r: &serde_json::Value| {
            if r.get("$type")
                .and_then(|t: &serde_json::Value| t.as_str())
                .is_some()
            {
                r.get("$val")
                    .and_then(|v: &serde_json::Value| v.get("err"))
                    .cloned()
            } else {
                Some(r.clone())
            }
        });

    let get_int = |key: &str| -> Option<i64> {
        container_payload
            .as_ref()
            .and_then(|p: &serde_json::Value| p.get(key))
            .and_then(|v: &serde_json::Value| v.as_i64())
    };
    let get_str = |key: &str| -> Option<String> {
        container_payload
            .as_ref()
            .and_then(|p: &serde_json::Value| p.get(key))
            .and_then(|v: &serde_json::Value| v.as_str())
            .map(String::from)
    };
    let get_dec_val = |key: &str| -> Val {
        container_payload
            .as_ref()
            .and_then(|p: &serde_json::Value| p.get(key))
            .and_then(|v: &serde_json::Value| serde_json::from_value(v.clone()).ok())
            .unwrap_or(Val::Null)
    };

    let is_container = task.task_type == "container";
    let result_val = if is_container {
        Val::Null
    } else {
        result_json
            .map(|j: &serde_json::Value| serde_json::from_value(j.clone()).unwrap_or(Val::Null))
            .unwrap_or(Val::Null)
    };
    let result_val = if crate::blob::is_blob_ref_val(&result_val) {
        match read_blob_result(vm, result_val, task.env_id) {
            Ok(val) => val,
            Err(e) => {
                return HotResult::Err(err_val(format!(
                    "::hot::task/await: failed to read the result of task {}: {}",
                    task_id, e
                )));
            }
        }
    } else {
        result_val
    };

    let exit_code_val = if is_container {
        get_int("exit-code").map(Val::from).unwrap_or(Val::Null)
    } else {
        Val::Null
    };
    let stdout_val = if is_container {
        get_str("stdout").map(Val::from).unwrap_or(Val::Null)
    } else {
        Val::Null
    };
    let stderr_val = if is_container {
        get_str("stderr").map(Val::from).unwrap_or(Val::Null)
    } else {
        Val::Null
    };
    let duration_val = task
        .duration_ms
        .or_else(|| get_int("duration-ms"))
        .map(Val::from)
        .unwrap_or(Val::Null);
    let slot_wait_val = get_int("slot-wait-ms").map(Val::from).unwrap_or(Val::Null);
    let image_pull_val = get_int("image-pull-ms").map(Val::from).unwrap_or(Val::Null);
    let execution_val = get_int("execution-ms").map(Val::from).unwrap_or(Val::Null);
    let logs_collect_val = get_int("logs-collect-ms")
        .map(Val::from)
        .unwrap_or(Val::Null);
    let size_val = get_str("size").map(Val::from).unwrap_or(Val::Null);
    let compute_units_val = get_int("compute-units").map(Val::from).unwrap_or(Val::Null);
    let cus_multiplier_val = get_dec_val("cus-multiplier");
    let container_id_val = get_str("container-id").map(Val::from).unwrap_or(Val::Null);
    let backend_val = get_str("backend").map(Val::from).unwrap_or(Val::Null);

    let mut val_map = IndexMap::new();
    val_map.insert(Val::from("id"), Val::from(task_id.to_string()));
    val_map.insert(Val::from("status"), Val::from(task.status.clone()));
    val_map.insert(Val::from("exit-code"), exit_code_val);
    val_map.insert(Val::from("stdout"), stdout_val);
    val_map.insert(Val::from("stderr"), stderr_val);
    val_map.insert(Val::from("duration-ms"), duration_val);
    val_map.insert(Val::from("slot-wait-ms"), slot_wait_val);
    val_map.insert(Val::from("image-pull-ms"), image_pull_val);
    val_map.insert(Val::from("execution-ms"), execution_val);
    val_map.insert(Val::from("logs-collect-ms"), logs_collect_val);
    val_map.insert(Val::from("size"), size_val);
    val_map.insert(Val::from("compute-units"), compute_units_val);
    val_map.insert(Val::from("cus-multiplier"), cus_multiplier_val);
    val_map.insert(Val::from("container-id"), container_id_val);
    val_map.insert(Val::from("backend"), backend_val);
    val_map.insert(Val::from("result"), result_val);

    let mut result_map = IndexMap::new();
    result_map.insert(Val::from("$type"), Val::from("::hot::task/TaskResult"));
    result_map.insert(Val::from("$val"), Val::map(val_map));
    let result = Val::map(result_map);

    HotResult::Ok(result)
}

/// Compute the next sleep slice (ms) for `::hot::task/await`'s backoff loop.
///
/// Bounded by the remaining backoff delay, the cancellation poll interval,
/// and the time left until the await deadline. A return of 0 means the
/// deadline has been reached (sub-millisecond remainders truncate to 0) and
/// the loop must wake instead of sleeping past it.
fn backoff_sleep_slice_ms(
    remaining_delay_ms: u64,
    poll_slice_ms: u64,
    remaining_to_deadline: std::time::Duration,
) -> u64 {
    let deadline_ms = u64::try_from(remaining_to_deadline.as_millis()).unwrap_or(u64::MAX);
    remaining_delay_ms.min(poll_slice_ms).min(deadline_ms)
}

/// Sleep out one backoff delay in short slices, capping every slice by the
/// remaining time to `deadline` so the loop wakes AT the deadline instead of
/// overshooting it — a full backoff sleep (up to AWAIT_POLL_MAX_DELAY_MS) landing past
/// the deadline would consume the PARENT_COMPLETION_RESERVE that
/// `cap_wait_to_deadline` set aside for the parent run's finalization.
///
/// Returns true if `is_cancelled` observed a cancellation while sleeping.
async fn sleep_backoff_capped(
    delay_ms: u64,
    poll_slice_ms: u64,
    deadline: tokio::time::Instant,
    is_cancelled: &dyn Fn() -> bool,
) -> bool {
    let mut remaining = delay_ms;
    while remaining > 0 {
        if is_cancelled() {
            return true;
        }
        let to_deadline = deadline.saturating_duration_since(tokio::time::Instant::now());
        let slice = backoff_sleep_slice_ms(remaining, poll_slice_ms, to_deadline);
        if slice == 0 {
            // Deadline reached mid-backoff: wake now so the caller's deadline
            // check fires instead of sleeping into the parent's reserve.
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(slice)).await;
        remaining -= slice;
    }
    false
}

/// A single failed status poll inside `::hot::task/await`'s wait loop,
/// classified by how the loop must respond to it.
#[derive(Debug)]
enum PollFailure {
    /// The task row does not exist. Rows are inserted before the queue
    /// enqueue, so a missing row means the task is gone, not "not yet
    /// visible" — the await fails immediately.
    NotFound,
    /// The database read failed in transit. Retryable: the task may still be
    /// running, so the poll counts as missed while await budget remains.
    Transport(String),
    /// The database read exceeded its timeout slice. Retryable, same as
    /// `Transport`.
    TimedOut(std::time::Duration),
}

impl std::fmt::Display for PollFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PollFailure::NotFound => write!(f, "task not found"),
            PollFailure::Transport(e) => write!(f, "task lookup failed: {}", e),
            PollFailure::TimedOut(t) => write!(f, "database read timed out after {:?}", t),
        }
    }
}

const AWAIT_POLL_MIN_DELAY_MS: u64 = 100;
const AWAIT_POLL_MAX_DELAY_MS: u64 = 5_000;
/// The pause before the next poll is this fraction of the wait so far.
const AWAIT_POLL_ELAPSED_DIVISOR: u64 = 20;

/// How long `::hot::task/await` pauses before its next status poll, having
/// waited `elapsed` for the attempt it is polling.
///
/// Polling is the only completion signal the await has, and each poll reads
/// the task row. A fixed short interval would read it several times a second
/// for the hours an await can last, so the pause grows with the wait: 5% of
/// it, within 100ms to 5s. A task that finishes is then seen no later than 5%
/// of its own run time after it does (100ms for a 2s task, 500ms for a 10s
/// one) and never more than 5s late.
fn await_poll_delay_ms(elapsed: std::time::Duration) -> u64 {
    let elapsed_ms = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX);
    (elapsed_ms / AWAIT_POLL_ELAPSED_DIVISOR)
        .clamp(AWAIT_POLL_MIN_DELAY_MS, AWAIT_POLL_MAX_DELAY_MS)
}

/// Drive `poll` until `task` reaches a terminal status, the await `deadline`
/// passes, or `is_cancelled` observes a cancellation.
///
/// Polls are spaced by [`await_poll_delay_ms`], counted from this call.
/// `await_task_lineage` calls it again for each retry it follows, so the
/// spacing starts over at 100ms there: a retry is a fresh attempt and a
/// fresh wait, however long the attempt before it ran.
///
/// A timed-out or transport-errored poll is a MISSED poll, not a fatal error:
/// while the await deadline still has budget the task may yet complete, so
/// the failure is logged at warn and the next backoff tick retries. Each
/// poll's timeout slice is capped at the remaining budget, so failed polls
/// can never extend the wait past `deadline` — the deadline check at the top
/// of the loop fires instead, mentioning the last poll failure.
/// `PollFailure::NotFound` stays fatal: task rows exist before enqueue, so a
/// missing row means the task is gone.
async fn await_task_poll_loop<F, Fut>(
    mut task: crate::db::Task,
    deadline: tokio::time::Instant,
    is_cancelled: &dyn Fn() -> bool,
    mut poll: F,
) -> Result<crate::db::Task, Val>
where
    F: FnMut(std::time::Duration) -> Fut,
    Fut: std::future::Future<Output = Result<crate::db::Task, PollFailure>>,
{
    const TERMINAL_STATUSES: &[&str] = &["completed", "failed", "timed_out", "cancelled"];
    // Cap each sleep slice so cancellation is observed within ~250ms
    // whatever the poll delay is.
    const CANCEL_POLL_SLICE_MS: u64 = 250;

    let started = tokio::time::Instant::now();
    let mut last_poll_failure: Option<String> = None;

    while !TERMINAL_STATUSES.contains(&task.status.as_str()) {
        if is_cancelled() {
            return Err(err_val(
                "::hot::task/await: cancelled while waiting for task".to_string(),
            ));
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(match last_poll_failure {
                Some(failure) => err_val(format!(
                    "::hot::task/await: timeout waiting for task to complete (last poll error: {})",
                    failure
                )),
                None => {
                    err_val("::hot::task/await: timeout waiting for task to complete".to_string())
                }
            });
        }

        let delay_ms = await_poll_delay_ms(started.elapsed());
        if sleep_backoff_capped(delay_ms, CANCEL_POLL_SLICE_MS, deadline, is_cancelled).await {
            return Err(err_val(
                "::hot::task/await: cancelled while waiting for task".to_string(),
            ));
        }

        let remaining_to_deadline = deadline.saturating_duration_since(tokio::time::Instant::now());
        let read_timeout = TASK_NATIVE_IO_TIMEOUT.min(remaining_to_deadline);
        match poll(read_timeout).await {
            Ok(t) => {
                last_poll_failure = None;
                task = t;
            }
            Err(PollFailure::NotFound) => {
                return Err(err_val(format!(
                    "::hot::task/await: task not found: {}",
                    task.task_id
                )));
            }
            Err(failure) => {
                tracing::warn!(
                    task_id = %task.task_id,
                    "::hot::task/await: missed a status poll ({}); retrying until the await deadline",
                    failure
                );
                last_poll_failure = Some(failure.to_string());
            }
        }
    }

    Ok(task)
}

// ---------------------------------------------------------------------------
// Retry lineage — a retry is a new task row under a new id
// ---------------------------------------------------------------------------

/// How long after a failure with unspent `retry` attempts a retry row that
/// is not there yet is still looked for.
///
/// A worker of this version commits the retry in the transaction that fails
/// the attempt (`Task::complete_with_retry`,
/// `Task::fail_stale_running_with_retry`), as a deploy interruption always
/// has, so for its rows one lookup is the whole answer and nothing waits.
/// The wait exists only for a rolling deploy: a worker from before that
/// change inserts the retry after its failure is already visible, and
/// nothing on the row says which kind of worker wrote it. Remove it once no
/// such worker can still be running.
const LEGACY_RETRY_CHILD_GRACE: std::time::Duration = std::time::Duration::from_secs(2);
/// How long a retry lookup that fails is repeated for a row that points to a
/// retry.
const RETRY_LOOKUP_ERROR_WINDOW: std::time::Duration = std::time::Duration::from_secs(2);
const RETRY_CHILD_RECHECK_MS: u64 = 100;

/// Only a failed or timed-out attempt is ever retried. `completed` and
/// `cancelled` are final: lineage is never followed from them.
fn is_retryable_terminal(status: &str) -> bool {
    matches!(status, "failed" | "timed_out")
}

/// Whether `task`'s own row points to a retry: a deploy/shutdown interruption
/// marks the result `infra_interrupted`, and a `retry` policy leaves attempts
/// unspent. Either way the retry was committed with the failure, except by a
/// worker older than [`LEGACY_RETRY_CHILD_GRACE`] describes.
fn retry_expected(task: &crate::db::Task) -> bool {
    let infra_interrupted = task
        .result
        .as_ref()
        .and_then(|r| r.get("$val"))
        .and_then(|v| v.get("infra_interrupted"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    infra_interrupted || retry_attempts_unspent(task)
}

fn retry_attempts_unspent(task: &crate::db::Task) -> bool {
    let retry = crate::env::retry::RetryConfig::from_meta(task.options.as_ref());
    retry.is_enabled() && task.retry_attempt < retry.max_retries
}

/// What is left of [`LEGACY_RETRY_CHILD_GRACE`] for `parent`, counted from
/// its failure rather than from this lookup. Zero for a failure no `retry`
/// policy could follow.
fn legacy_retry_wait(parent: &crate::db::Task) -> std::time::Duration {
    if !retry_attempts_unspent(parent) {
        return std::time::Duration::ZERO;
    }
    let since_failure = parent
        .stop_time
        .and_then(|at| (chrono::Utc::now() - at).to_std().ok())
        .unwrap_or_default();
    LEGACY_RETRY_CHILD_GRACE.saturating_sub(since_failure)
}

async fn lookup_retry_child(
    db: &crate::db::DatabasePool,
    parent: Uuid,
    env_id: Uuid,
    read_timeout: std::time::Duration,
) -> Result<Option<crate::db::Task>, PollFailure> {
    match tokio::time::timeout(
        read_timeout,
        crate::db::Task::get_retry_child(db, &parent, &env_id),
    )
    .await
    {
        Ok(Ok(child)) => Ok(child),
        Ok(Err(e)) => Err(PollFailure::Transport(e.to_string())),
        Err(_) => Err(PollFailure::TimedOut(read_timeout)),
    }
}

/// Find the retry of a failed or timed-out `parent`.
///
/// One lookup settles it: a retry is committed with the failure it follows.
/// The lookup is repeated, never past `deadline`, only when it fails for a
/// row that points to a retry ([`retry_expected`], for at most
/// [`RETRY_LOOKUP_ERROR_WINDOW`]), or finds nothing for a failure an older
/// worker may still be about to retry ([`legacy_retry_wait`]). `None` means
/// the parent's failure stands.
async fn find_retry_child<C, Fut>(
    parent: &crate::db::Task,
    deadline: tokio::time::Instant,
    is_cancelled: &dyn Fn() -> bool,
    lookup: &mut C,
) -> Option<crate::db::Task>
where
    C: FnMut(Uuid, std::time::Duration) -> Fut,
    Fut: std::future::Future<Output = Result<Option<crate::db::Task>, PollFailure>>,
{
    let started = tokio::time::Instant::now();
    let error_window = if retry_expected(parent) {
        RETRY_LOOKUP_ERROR_WINDOW
    } else {
        std::time::Duration::ZERO
    };
    let give_up_on_errors = (started + error_window).min(deadline);
    let give_up_on_legacy = (started + legacy_retry_wait(parent)).min(deadline);
    loop {
        let to_deadline = deadline.saturating_duration_since(tokio::time::Instant::now());
        if to_deadline.is_zero() || is_cancelled() {
            return None;
        }
        let give_up = match lookup(parent.task_id, TASK_NATIVE_IO_TIMEOUT.min(to_deadline)).await {
            Ok(Some(child)) => return Some(child),
            Ok(None) => give_up_on_legacy,
            Err(failure) => {
                tracing::warn!(
                    task_id = %parent.task_id,
                    "retry lookup failed ({}); the task's own outcome stands unless a retry is found",
                    failure
                );
                give_up_on_errors
            }
        };
        let to_give_up = give_up.saturating_duration_since(tokio::time::Instant::now());
        if to_give_up.is_zero() {
            return None;
        }
        let pause = std::time::Duration::from_millis(RETRY_CHILD_RECHECK_MS).min(to_give_up);
        tokio::time::sleep(pause).await;
    }
}

/// `await_task_poll_loop`, continued down the retry lineage: when the awaited
/// attempt fails or times out and was retried, the wait moves to the retry,
/// and so on, returning the last attempt's row. `retry_deadline` gives the
/// wait a retry is allowed; the deadline only ever moves later.
async fn await_task_lineage<P, PFut, C, CFut>(
    mut task: crate::db::Task,
    mut deadline: tokio::time::Instant,
    is_cancelled: &dyn Fn() -> bool,
    mut poll: P,
    mut retry_child: C,
    retry_deadline: &dyn Fn(&crate::db::Task) -> Option<tokio::time::Instant>,
) -> Result<crate::db::Task, Val>
where
    P: FnMut(Uuid, std::time::Duration) -> PFut,
    PFut: std::future::Future<Output = Result<crate::db::Task, PollFailure>>,
    C: FnMut(Uuid, std::time::Duration) -> CFut,
    CFut: std::future::Future<Output = Result<Option<crate::db::Task>, PollFailure>>,
{
    loop {
        let current = task.task_id;
        task = await_task_poll_loop(task, deadline, is_cancelled, |read_timeout| {
            poll(current, read_timeout)
        })
        .await?;
        if !is_retryable_terminal(&task.status) {
            return Ok(task);
        }
        let Some(retry) = find_retry_child(&task, deadline, is_cancelled, &mut retry_child).await
        else {
            return Ok(task);
        };
        tracing::debug!(
            task_id = %task.task_id,
            retry_task_id = %retry.task_id,
            "::hot::task/await: attempt {} was retried; following the retry",
            task.status
        );
        if let Some(extended) = retry_deadline(&retry) {
            deadline = deadline.max(extended);
        }
        task = retry;
    }
}

/// The newest attempt in `task`'s retry lineage: what a caller still holding
/// an earlier attempt's id means by it. Stops at the first attempt that is
/// active or final.
async fn newest_retry_attempt(
    db: &crate::db::DatabasePool,
    mut task: crate::db::Task,
    env_id: Uuid,
) -> crate::db::Task {
    let deadline = tokio::time::Instant::now() + TASK_NATIVE_IO_TIMEOUT;
    let mut lookup =
        |parent: Uuid, read_timeout| lookup_retry_child(db, parent, env_id, read_timeout);
    while is_retryable_terminal(&task.status) {
        match find_retry_child(&task, deadline, &|| false, &mut lookup).await {
            Some(retry) => task = retry,
            None => break,
        }
    }
    task
}

/// Cancel the newest attempt of `task_id`'s lineage. Returns the id acted on
/// and whether it was cancelled.
async fn cancel_newest_attempt(
    db: &crate::db::DatabasePool,
    task_id: Uuid,
    env_id: Uuid,
) -> Result<(Uuid, bool), Val> {
    let mut target = task_id;
    for _ in 0..2 {
        let mut was_active = false;
        if let Ok(Ok(task)) = tokio::time::timeout(
            TASK_NATIVE_IO_TIMEOUT,
            crate::db::Task::get_in_env(db, &target, &env_id),
        )
        .await
        {
            let newest = newest_retry_attempt(db, task, env_id).await;
            was_active = matches!(newest.status.as_str(), "queued" | "running");
            target = newest.task_id;
        }
        let cancelled = match tokio::time::timeout(
            TASK_NATIVE_IO_TIMEOUT,
            crate::db::Task::cancel(db, &target, &env_id),
        )
        .await
        {
            Ok(Ok(c)) => c,
            Ok(Err(e)) => {
                tracing::error!(task_id = %target, "::hot::task/cancel: DB error: {}", e);
                return Err(err_val(format!("::hot::task/cancel: DB error: {}", e)));
            }
            Err(_) => {
                return Err(err_val(format!(
                    "::hot::task/cancel: database write timed out after {}s",
                    TASK_NATIVE_IO_TIMEOUT.as_secs()
                )));
            }
        };
        if cancelled || !was_active {
            return Ok((target, cancelled));
        }
        // The attempt went terminal between the lookup and the write. A
        // deploy fails it and queues its retry in one transaction, so look
        // once more for a retry to cancel.
    }
    Ok((target, false))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Compensate for an enqueue failure after the task row was inserted by
/// terminally failing the task — but only while it is still `queued`.
///
/// A client-side enqueue error or timeout does not prove the queue entry
/// never landed server-side (dropping the enqueue future abandons only the
/// reply, not the write). If a worker has already claimed the task
/// (queued -> running), overwriting it to `failed` would clobber a
/// legitimate execution and suppress its real result, so the guarded write
/// skips the row and the claim is logged instead. Compensation failures are
/// logged, never swallowed: the row would otherwise stay `queued` forever.
pub(crate) async fn fail_task_after_enqueue_error(
    db: &crate::db::DatabasePool,
    task_id: &Uuid,
    fn_label: &str,
    failure: &serde_json::Value,
    io_timeout: std::time::Duration,
) {
    match tokio::time::timeout(
        io_timeout,
        crate::db::Task::fail_if_queued(db, task_id, Some(failure)),
    )
    .await
    {
        Ok(Ok(true)) => {}
        Ok(Ok(false)) => {
            tracing::warn!(
                task_id = %task_id,
                "{}: task already claimed by a worker after an enqueue failure; the queue entry likely landed — leaving the task untouched",
                fn_label
            );
        }
        Ok(Err(e)) => {
            tracing::error!(
                task_id = %task_id,
                "{}: failed to persist enqueue-failure compensation: {}",
                fn_label,
                e
            );
        }
        Err(_) => {
            tracing::error!(
                task_id = %task_id,
                "{}: enqueue-failure compensation timed out after {}s",
                fn_label,
                io_timeout.as_secs()
            );
        }
    }
}

/// Extract a qualified function name from a Val that represents a function.
fn extract_function_name(fn_name: &str, val: &Val) -> Result<String, Val> {
    // Try FunctionRef (Val::Box)
    if let Some(func_ref) = extract_function_ref(val) {
        return Ok(func_ref.name.clone());
    }

    // Try typed Fn map: {$type: "::hot::type/Fn", $val: "::ns/fn"}
    if let Val::Map(map) = val
        && let Some(Val::Str(type_name)) = map.get(&Val::from("$type"))
        && &**type_name == "::hot::type/Fn"
        && let Some(Val::Str(qualified_name)) = map.get(&Val::from("$val"))
    {
        return Ok((**qualified_name).to_owned());
    }

    // Try plain string (qualified name)
    if let Val::Str(s) = val
        && s.starts_with("::")
    {
        return Ok((**s).to_owned());
    }

    Err(err_val(format!(
        "{}: first arg must be a function reference, got: {:?}",
        fn_name, val
    )))
}

// ---------------------------------------------------------------------------
// ::hot::task/checkpoint — save application-level state for resumable tasks
// ---------------------------------------------------------------------------

/// Save a checkpoint for the current task. The data is persisted to the task's
/// `info` column and survives worker restarts. On retry, `::hot::task/restore`
/// can retrieve it.
///
/// # Arguments
/// * `data` - Any serializable Hot value (Map, Vec, Str, Int, etc.)
///
/// # Returns
/// `true` on success
pub fn checkpoint(vm: &mut VirtualMachine, args: &[Val]) -> HotResult<Val> {
    if args.len() != 1 {
        return HotResult::Err(err_val(
            "::hot::task/checkpoint: expected 1 arg (data)".to_string(),
        ));
    }

    let task_id = match vm.get_task_id() {
        Some(id) => id,
        None => {
            return HotResult::Err(err_val(
                "::hot::task/checkpoint: can only be called inside a task".to_string(),
            ));
        }
    };

    let db = match vm.get_database_pool() {
        Some(pool) => pool,
        None => {
            return HotResult::Err(err_val(
                "::hot::task/checkpoint: no database available".to_string(),
            ));
        }
    };

    let data_json = match args[0].to_checked_record_json("::hot::task/checkpoint") {
        Ok(json) => json,
        Err(error) => return HotResult::Err(err_val(error)),
    };

    let result = tokio::runtime::Handle::current().block_on(async {
        tokio::time::timeout(
            TASK_NATIVE_IO_TIMEOUT,
            crate::db::Task::set_checkpoint(&db, &task_id, &data_json),
        )
        .await
    });

    match result {
        Ok(Ok(())) => HotResult::Ok(Val::Bool(true)),
        Ok(Err(e)) => HotResult::Err(err_val(format!(
            "::hot::task/checkpoint: failed to save: {}",
            e
        ))),
        Err(_) => HotResult::Err(err_val(format!(
            "::hot::task/checkpoint: database write timed out after {}s",
            TASK_NATIVE_IO_TIMEOUT.as_secs()
        ))),
    }
}

// ---------------------------------------------------------------------------
// ::hot::task/restore — retrieve the last checkpoint saved for this task
// ---------------------------------------------------------------------------

/// Retrieve the last checkpoint for the current task (or a specific task by ID).
/// Returns `null` if no checkpoint exists.
///
/// # Arguments (0 or 1)
/// * No args: restores checkpoint for the current task
/// * `task-id: Str`: restores checkpoint for a specific task
///
/// # Returns
/// The checkpointed data, or `null`
pub fn restore(vm: &mut VirtualMachine, args: &[Val]) -> HotResult<Val> {
    if args.len() > 1 {
        return HotResult::Err(err_val(
            "::hot::task/restore: expected 0-1 args ([task-id])".to_string(),
        ));
    }

    let task_id = if args.is_empty() {
        match vm.get_task_id() {
            Some(id) => id,
            None => {
                return HotResult::Err(err_val(
                    "::hot::task/restore: can only be called inside a task (or pass a task-id)"
                        .to_string(),
                ));
            }
        }
    } else {
        match &args[0] {
            Val::Str(s) => match Uuid::parse_str(s) {
                Ok(id) => id,
                Err(_) => {
                    return HotResult::Err(err_val(
                        "::hot::task/restore: invalid task-id UUID".to_string(),
                    ));
                }
            },
            _ => {
                return HotResult::Err(err_val(
                    "::hot::task/restore: task-id must be a string".to_string(),
                ));
            }
        }
    };

    let db = match vm.get_database_pool() {
        Some(pool) => pool,
        None => {
            return HotResult::Err(err_val(
                "::hot::task/restore: no database available".to_string(),
            ));
        }
    };

    let env_id = match caller_env_id(vm, "::hot::task/restore") {
        Ok(id) => id,
        Err(e) => return HotResult::Err(e),
    };

    let result = tokio::runtime::Handle::current().block_on(async {
        tokio::time::timeout(
            TASK_NATIVE_IO_TIMEOUT,
            crate::db::Task::get_checkpoint(&db, &task_id, &env_id),
        )
        .await
    });

    match result {
        Ok(Ok(Some(data))) => {
            let val: Val = serde_json::from_value(data).unwrap_or(Val::Null);
            HotResult::Ok(val)
        }
        Ok(Ok(None)) => HotResult::Ok(Val::Null),
        Ok(Err(e)) => HotResult::Err(err_val(format!(
            "::hot::task/restore: failed to load: {}",
            e
        ))),
        Err(_) => HotResult::Err(err_val(format!(
            "::hot::task/restore: database read timed out after {}s",
            TASK_NATIVE_IO_TIMEOUT.as_secs()
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lang::runtime::function_ref::FunctionRef;
    use std::sync::Arc;

    // -----------------------------------------------------------------------
    // extract_function_name tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_extract_from_function_ref() {
        let val = Val::Box(Box::new(FunctionRef::new("::app/my-task".to_string())));
        let result = extract_function_name("test", &val);
        assert_eq!(result.unwrap(), "::app/my-task");
    }

    #[test]
    fn test_extract_from_function_ref_with_arity() {
        let val = Val::Box(Box::new(FunctionRef::with_arity(
            "::ns/handler".to_string(),
            2,
        )));
        let result = extract_function_name("test", &val);
        assert_eq!(result.unwrap(), "::ns/handler");
    }

    #[test]
    fn test_extract_from_typed_fn_map() {
        let val = crate::val!({
            "$type": "::hot::type/Fn",
            "$val": "::myapp/process"
        });
        let result = extract_function_name("test", &val);
        assert_eq!(result.unwrap(), "::myapp/process");
    }

    #[test]
    fn test_extract_from_qualified_string() {
        let val = Val::from("::app/handler");
        let result = extract_function_name("test", &val);
        assert_eq!(result.unwrap(), "::app/handler");
    }

    #[test]
    fn test_extract_rejects_unqualified_string() {
        let val = Val::from("not-qualified");
        let result = extract_function_name("test", &val);
        assert!(result.is_err());
    }

    #[test]
    fn test_extract_rejects_int() {
        let val = Val::Int(42);
        let result = extract_function_name("test", &val);
        assert!(result.is_err());
    }

    #[test]
    fn test_extract_rejects_null() {
        let val = Val::Null;
        let result = extract_function_name("test", &val);
        assert!(result.is_err());
    }

    #[test]
    fn test_extract_rejects_bool() {
        let val = Val::Bool(true);
        let result = extract_function_name("test", &val);
        assert!(result.is_err());
    }

    // -----------------------------------------------------------------------
    // TaskRequest serialization tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_task_request_serde_round_trip() {
        let request = TaskRequest {
            task_id: "019506ab-1234-7000-8000-000000000001".to_string(),
            function_name: "::myapp/long-running".to_string(),
            args: serde_json::json!({"url": "https://example.com", "retries": 3}),
            stream_id: "019506ab-1234-7000-8000-000000000002".to_string(),
            env_id: "019506ab-1234-7000-8000-000000000003".to_string(),
            build_id: "019506ab-1234-7000-8000-000000000004".to_string(),
            org_id: Some("019506ab-1234-7000-8000-000000000005".to_string()),
            user_id: None,
            project_id: Some("019506ab-1234-7000-8000-000000000006".to_string()),
            project_name: Some("test-project".to_string()),
            timeout_ms: 300_000,
            task_type: "code".to_string(),
            created_at_unix_ms: 1700000000000,
            origin_run_id: Some("019506ab-1234-7000-8000-000000000007".to_string()),
        };

        let json = serde_json::to_string(&request).unwrap();
        let deserialized: TaskRequest = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.task_id, request.task_id);
        assert_eq!(deserialized.function_name, request.function_name);
        assert_eq!(deserialized.args, request.args);
        assert_eq!(deserialized.stream_id, request.stream_id);
        assert_eq!(deserialized.env_id, request.env_id);
        assert_eq!(deserialized.build_id, request.build_id);
        assert_eq!(deserialized.org_id, request.org_id);
        assert_eq!(deserialized.user_id, request.user_id);
        assert_eq!(deserialized.project_id, request.project_id);
        assert_eq!(deserialized.project_name, request.project_name);
        assert_eq!(deserialized.timeout_ms, request.timeout_ms);
        assert_eq!(deserialized.task_type, request.task_type);
        assert_eq!(deserialized.created_at_unix_ms, request.created_at_unix_ms);
    }

    #[test]
    fn test_task_request_null_optionals() {
        let request = TaskRequest {
            task_id: "id".to_string(),
            function_name: "::app/fn".to_string(),
            args: serde_json::Value::Null,
            stream_id: "sid".to_string(),
            env_id: "eid".to_string(),
            build_id: "bid".to_string(),
            org_id: None,
            user_id: None,
            project_id: None,
            project_name: None,
            timeout_ms: 1_800_000,
            task_type: "code".to_string(),
            created_at_unix_ms: 0,
            origin_run_id: None,
        };

        let json = serde_json::to_string(&request).unwrap();
        let deserialized: TaskRequest = serde_json::from_str(&json).unwrap();
        assert!(deserialized.org_id.is_none());
        assert!(deserialized.user_id.is_none());
        assert!(deserialized.args.is_null());
    }

    // -----------------------------------------------------------------------
    // task/await backoff deadline-capping tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_backoff_slice_bounded_by_poll_interval() {
        let far_deadline = std::time::Duration::from_secs(60);
        assert_eq!(backoff_sleep_slice_ms(5000, 250, far_deadline), 250);
    }

    #[test]
    fn test_backoff_slice_bounded_by_remaining_delay() {
        let far_deadline = std::time::Duration::from_secs(60);
        assert_eq!(backoff_sleep_slice_ms(100, 250, far_deadline), 100);
    }

    #[test]
    fn test_backoff_slice_bounded_by_deadline() {
        assert_eq!(
            backoff_sleep_slice_ms(5000, 250, std::time::Duration::from_millis(80)),
            80
        );
    }

    #[test]
    fn test_backoff_slice_zero_at_or_past_deadline() {
        assert_eq!(
            backoff_sleep_slice_ms(5000, 250, std::time::Duration::ZERO),
            0
        );
        // A sub-millisecond remainder truncates to zero: wake at the deadline
        // rather than sleeping a whole extra slice past it.
        assert_eq!(
            backoff_sleep_slice_ms(5000, 250, std::time::Duration::from_micros(500)),
            0
        );
    }

    #[test]
    fn initial_task_lookup_preserves_parent_completion_reserve() {
        let context = crate::lang::event::ExecutionContext::new(
            Uuid::now_v7(),
            Uuid::now_v7(),
            crate::db::run::RunType::Task.as_id(),
            None,
            None,
            None,
            None,
        )
        .with_deadline(Some(
            chrono::Utc::now()
                + chrono::Duration::from_std(
                    PARENT_COMPLETION_RESERVE + std::time::Duration::from_secs(1),
                )
                .unwrap(),
        ));

        let budget = initial_await_lookup_budget(Some(&context)).unwrap();
        assert!(budget <= std::time::Duration::from_secs(1));
        assert!(budget > std::time::Duration::ZERO);
        assert_eq!(
            initial_await_lookup_budget(None),
            Some(TASK_NATIVE_IO_TIMEOUT)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn test_backoff_sleep_wakes_at_deadline_mid_backoff() {
        // The deadline lands between two backoff slices of a max-length (5s)
        // backoff sleep; the loop must wake exactly at the deadline, never
        // past it (overshooting would consume the parent completion reserve).
        let start = tokio::time::Instant::now();
        let deadline = start + std::time::Duration::from_millis(600);
        let cancelled = sleep_backoff_capped(5000, 250, deadline, &|| false).await;
        assert!(!cancelled);
        assert_eq!(
            tokio::time::Instant::now(),
            deadline,
            "backoff sleep must not overshoot the await deadline"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn test_backoff_sleep_full_delay_when_deadline_far() {
        let start = tokio::time::Instant::now();
        let deadline = start + std::time::Duration::from_secs(60);
        let cancelled = sleep_backoff_capped(1000, 250, deadline, &|| false).await;
        assert!(!cancelled);
        assert_eq!(
            tokio::time::Instant::now() - start,
            std::time::Duration::from_millis(1000),
            "without deadline pressure the full backoff delay is slept"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn test_backoff_sleep_observes_cancellation() {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(60);
        assert!(sleep_backoff_capped(1000, 250, deadline, &|| true).await);
    }

    // -----------------------------------------------------------------------
    // task/await poll-loop resilience tests
    // -----------------------------------------------------------------------

    fn poll_loop_task(status: &str) -> crate::db::Task {
        crate::db::Task {
            task_id: Uuid::nil(),
            env_id: Uuid::nil(),
            stream_id: Uuid::nil(),
            build_id: Uuid::nil(),
            origin_run_id: None,
            task_status_id: 2,
            status: status.to_string(),
            function_name: "::app/fn".to_string(),
            args: None,
            options: None,
            task_type: "code".to_string(),
            start_time: None,
            stop_time: None,
            duration_ms: None,
            result: None,
            info: None,
            timing: None,
            timeout_ms: 300_000,
            retry_attempt: 0,
            infra_retry_count: 0,
            next_retry_at: None,
            parent_task_id: None,
            created_at: chrono::Utc::now(),
            by_user_id: None,
            run_id: None,
            worker_id: None,
            last_heartbeat_at: None,
            container_id: None,
            origin_run_fn: None,
        }
    }

    #[tokio::test(start_paused = true)]
    async fn test_poll_loop_terminal_task_returns_without_polling() {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(60);
        let polls = std::cell::Cell::new(0u32);
        let task = await_task_poll_loop(
            poll_loop_task("completed"),
            deadline,
            &|| false,
            |_read_timeout: std::time::Duration| {
                polls.set(polls.get() + 1);
                async { Ok(poll_loop_task("completed")) }
            },
        )
        .await
        .expect("an already-terminal task must be returned as-is");
        assert_eq!(task.status, "completed");
        assert_eq!(polls.get(), 0, "no polls needed for a terminal task");
    }

    #[tokio::test(start_paused = true)]
    async fn test_poll_loop_transient_poll_failures_then_success() {
        // One timed-out poll and one transport-errored poll are MISSED polls,
        // not fatal errors: with deadline budget left the await must keep
        // polling and return the task's eventual terminal result.
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(60);
        let polls = std::cell::Cell::new(0u32);
        let task = await_task_poll_loop(
            poll_loop_task("running"),
            deadline,
            &|| false,
            |_read_timeout: std::time::Duration| {
                let attempt = polls.get();
                polls.set(attempt + 1);
                async move {
                    match attempt {
                        0 => Err(PollFailure::TimedOut(TASK_NATIVE_IO_TIMEOUT)),
                        1 => Err(PollFailure::Transport(
                            "connection reset by peer".to_string(),
                        )),
                        _ => Ok(poll_loop_task("completed")),
                    }
                }
            },
        )
        .await
        .expect("transient poll failures with budget left must not fail the await");
        assert_eq!(task.status, "completed");
        assert_eq!(polls.get(), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn test_poll_loop_persistent_poll_failures_hit_deadline_exactly() {
        let start = tokio::time::Instant::now();
        let deadline = start + std::time::Duration::from_secs(30);
        let err = await_task_poll_loop(
            poll_loop_task("running"),
            deadline,
            &|| false,
            |read_timeout: std::time::Duration| async move {
                // Model a hung read: tokio::time::timeout consumes the whole
                // slice before reporting Elapsed.
                tokio::time::sleep(read_timeout).await;
                Err(PollFailure::TimedOut(read_timeout))
            },
        )
        .await
        .expect_err("persistent poll failures must end in a deadline error");
        assert_eq!(
            tokio::time::Instant::now(),
            deadline,
            "failed-poll retries must not extend the wait past the deadline"
        );
        let msg = format!("{:?}", err);
        assert!(
            msg.contains("timeout waiting for task to complete"),
            "unexpected error: {msg}"
        );
        assert!(
            msg.contains("last poll error"),
            "deadline error must mention the last poll failure: {msg}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn test_poll_loop_not_found_fails_immediately() {
        let start = tokio::time::Instant::now();
        let deadline = start + std::time::Duration::from_secs(60);
        let err = await_task_poll_loop(
            poll_loop_task("running"),
            deadline,
            &|| false,
            |_read_timeout: std::time::Duration| async { Err(PollFailure::NotFound) },
        )
        .await
        .expect_err("a missing task row must fail the await immediately");
        let msg = format!("{:?}", err);
        assert!(msg.contains("task not found"), "unexpected error: {msg}");
        assert_eq!(
            tokio::time::Instant::now() - start,
            std::time::Duration::from_millis(100),
            "NotFound must fail on the first poll, not retry to the deadline"
        );
    }

    #[test]
    fn await_poll_delay_is_five_percent_of_the_wait_within_100ms_and_5s() {
        let delay = |ms| await_poll_delay_ms(std::time::Duration::from_millis(ms));
        assert_eq!(delay(0), 100);
        assert_eq!(delay(2_000), 100);
        assert_eq!(delay(2_100), 105);
        assert_eq!(delay(10_000), 500);
        assert_eq!(delay(100_000), 5_000);
        assert_eq!(delay(3_600_000), 5_000, "an hour in, still every 5s");
        assert_eq!(await_poll_delay_ms(std::time::Duration::MAX), 5_000);
    }

    /// When each poll of an attempt that finishes on poll number `polls`
    /// lands, in milliseconds since the await started.
    async fn poll_times_ms(polls: usize) -> Vec<u64> {
        let start = tokio::time::Instant::now();
        let deadline = start + std::time::Duration::from_secs(24 * 3600);
        let times = std::cell::RefCell::new(Vec::new());
        let task = await_task_poll_loop(
            poll_loop_task("running"),
            deadline,
            &|| false,
            |_read_timeout: std::time::Duration| {
                times.borrow_mut().push(start.elapsed().as_millis() as u64);
                let done = times.borrow().len() == polls;
                async move { Ok(poll_loop_task(if done { "completed" } else { "running" })) }
            },
        )
        .await
        .unwrap();
        assert_eq!(task.status, "completed");
        times.into_inner()
    }

    #[tokio::test(start_paused = true)]
    async fn test_poll_loop_spaces_polls_by_the_time_waited() {
        let times = poll_times_ms(400).await;
        // Every 100ms for the first two seconds.
        let first: Vec<u64> = (1..=21).map(|n| n * 100).collect();
        assert_eq!(times[..21], first[..]);
        // Then 5% of the wait so far.
        assert_eq!(times[21..24], [2_205, 2_315, 2_430]);
        for pair in times.windows(2) {
            let (at, gap) = (pair[0], pair[1] - pair[0]);
            assert_eq!(gap, (at / 20).clamp(100, 5_000), "after the poll at {at}ms");
            // A task that finished just after a poll is seen by the next
            // one: late by at most 5% of its run time, or 100ms.
            assert!(gap <= 100 || gap * 20 <= at);
        }
        // Past 100s the polls are 5s apart, and the wait got that far.
        let last = times.len() - 1;
        assert!(times[last - 1] > 100_000);
        assert_eq!(times[last] - times[last - 1], 5_000);
        // Far fewer reads than a fixed 250ms interval over the same wait.
        assert!((times[last] / 250) as usize > 4 * times.len());
    }

    #[tokio::test(start_paused = true)]
    async fn test_poll_spacing_starts_over_for_a_retry() {
        let (parent, child) = (Uuid::now_v7(), Uuid::now_v7());
        let start = tokio::time::Instant::now();
        let deadline = start + std::time::Duration::from_secs(3600);
        // The first attempt fails after a minute; its retry runs for 300ms.
        let failed_at = start + std::time::Duration::from_secs(60);
        let child_polls = std::cell::RefCell::new(Vec::new());
        let task = await_task_lineage(
            lineage_task(parent, "running"),
            deadline,
            &|| false,
            |id: Uuid, _read_timeout: std::time::Duration| {
                let now = tokio::time::Instant::now();
                let row = if id == parent {
                    let status = if now >= failed_at {
                        "failed"
                    } else {
                        "running"
                    };
                    infra_interrupted(lineage_task(parent, status))
                } else {
                    child_polls.borrow_mut().push(now);
                    let done = child_polls.borrow().len() == 3;
                    lineage_task(child, if done { "completed" } else { "running" })
                };
                async move { Ok(row) }
            },
            |_parent: Uuid, _read_timeout: std::time::Duration| async move {
                Ok(Some(lineage_task(child, "queued")))
            },
            &|_retry| None,
        )
        .await
        .unwrap();
        assert_eq!((task.task_id, task.status.as_str()), (child, "completed"));
        let child_polls = child_polls.into_inner();
        let followed_at = child_polls[0] - std::time::Duration::from_millis(100);
        assert!(
            followed_at >= failed_at,
            "the retry is polled 100ms after it is followed, not seconds"
        );
        assert!(followed_at - failed_at <= std::time::Duration::from_secs(3));
        let gaps: Vec<_> = child_polls.windows(2).map(|p| p[1] - p[0]).collect();
        assert_eq!(gaps, [std::time::Duration::from_millis(100); 2]);
    }

    // -----------------------------------------------------------------------
    // task/await, cancel and send follow the retry lineage
    // -----------------------------------------------------------------------

    fn lineage_task(task_id: Uuid, status: &str) -> crate::db::Task {
        crate::db::Task {
            task_id,
            ..poll_loop_task(status)
        }
    }

    fn infra_interrupted(mut task: crate::db::Task) -> crate::db::Task {
        task.result = Some(serde_json::json!({
            "$type": "::hot::task/Failure",
            "$val": {"msg": "interrupted", "err": null, "infra_interrupted": true}
        }));
        task
    }

    fn with_retry_budget(mut task: crate::db::Task, attempts: i64) -> crate::db::Task {
        task.options = Some(serde_json::json!({"retry": attempts}));
        task
    }

    /// Await over a scripted lineage: `rows` is what each id polls as, and
    /// `children` answers each successive retry lookup.
    async fn await_lineage(
        first: crate::db::Task,
        rows: &[crate::db::Task],
        children: &dyn Fn(Uuid, u32) -> Option<crate::db::Task>,
    ) -> (crate::db::Task, u32) {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(60);
        let lookups = std::cell::Cell::new(0u32);
        let task = await_task_lineage(
            first,
            deadline,
            &|| false,
            |id: Uuid, _read_timeout: std::time::Duration| {
                let row = rows.iter().find(|row| row.task_id == id).map(|row| {
                    let mut copy = lineage_task(row.task_id, &row.status);
                    copy.result = row.result.clone();
                    copy.options = row.options.clone();
                    copy
                });
                async move { row.ok_or(PollFailure::NotFound) }
            },
            |parent: Uuid, _read_timeout: std::time::Duration| {
                lookups.set(lookups.get() + 1);
                let child = children(parent, lookups.get());
                async move { Ok(child) }
            },
            &|_retry| None,
        )
        .await
        .expect("the lineage ends in a terminal task");
        (task, lookups.get())
    }

    #[tokio::test(start_paused = true)]
    async fn await_follows_an_interrupted_task_to_its_retrys_outcome() {
        let (parent, child, grandchild) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
        let start = tokio::time::Instant::now();
        let (task, _) = await_lineage(
            lineage_task(parent, "running"),
            &[
                infra_interrupted(lineage_task(parent, "failed")),
                lineage_task(child, "timed_out"),
                lineage_task(grandchild, "completed"),
            ],
            &|id, _| match id {
                id if id == parent => Some(lineage_task(child, "queued")),
                id if id == child => Some(lineage_task(grandchild, "running")),
                _ => None,
            },
        )
        .await;
        assert_eq!(
            (task.task_id, task.status.as_str()),
            (grandchild, "completed")
        );
        assert!(
            tokio::time::Instant::now() - start < std::time::Duration::from_secs(1),
            "a retry that is already there is followed without a grace wait"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn await_never_follows_lineage_from_completed_or_cancelled() {
        for status in ["completed", "cancelled"] {
            let id = Uuid::now_v7();
            let (task, lookups) = await_lineage(
                with_retry_budget(lineage_task(id, status), 3),
                &[],
                &|_, _| Some(lineage_task(Uuid::now_v7(), "running")),
            )
            .await;
            assert_eq!((task.task_id, task.status.as_str()), (id, status));
            assert_eq!(lookups, 0, "{status} is final");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn await_returns_a_plain_failure_at_once() {
        // No retry policy, no interruption marker: one lookup, no waiting.
        let id = Uuid::now_v7();
        let start = tokio::time::Instant::now();
        let (task, lookups) = await_lineage(lineage_task(id, "failed"), &[], &|_, _| None).await;
        assert_eq!((task.task_id, task.status.as_str()), (id, "failed"));
        assert_eq!(lookups, 1);
        assert_eq!(tokio::time::Instant::now(), start);

        // Retry budget spent: the last attempt's failure is final too.
        let mut last = with_retry_budget(lineage_task(id, "failed"), 2);
        last.retry_attempt = 2;
        let (task, lookups) = await_lineage(last, &[], &|_, _| None).await;
        assert_eq!(task.status, "failed");
        assert_eq!(lookups, 1);
        assert_eq!(tokio::time::Instant::now(), start);
    }

    #[tokio::test(start_paused = true)]
    async fn await_rechecks_for_a_retry_an_older_worker_inserts_late() {
        // A worker from before the retry was committed with the failure
        // inserts it after the failure is visible: the row's unspent attempts
        // make the await look again.
        let (parent, child) = (Uuid::now_v7(), Uuid::now_v7());
        let (task, lookups) = await_lineage(
            with_retry_budget(lineage_task(parent, "failed"), 3),
            &[lineage_task(child, "completed")],
            &|_, lookup| (lookup >= 3).then(|| lineage_task(child, "queued")),
        )
        .await;
        assert_eq!((task.task_id, task.status.as_str()), (child, "completed"));
        assert_eq!(lookups, 3);
    }

    #[tokio::test(start_paused = true)]
    async fn await_stops_rechecking_after_the_grace_period() {
        let id = Uuid::now_v7();
        let start = tokio::time::Instant::now();
        let (task, _) = await_lineage(
            with_retry_budget(lineage_task(id, "timed_out"), 3),
            &[],
            &|_, _| None,
        )
        .await;
        assert_eq!((task.task_id, task.status.as_str()), (id, "timed_out"));
        assert_eq!(
            tokio::time::Instant::now() - start,
            LEGACY_RETRY_CHILD_GRACE
        );
    }

    #[tokio::test(start_paused = true)]
    async fn await_takes_one_lookup_once_a_failure_is_older_than_the_legacy_grace() {
        // Even a pre-atomic worker has inserted its retry by now: one lookup
        // is the answer, with attempts unspent and no retry row.
        let id = Uuid::now_v7();
        let mut failed = with_retry_budget(lineage_task(id, "failed"), 3);
        failed.stop_time = Some(chrono::Utc::now() - chrono::Duration::seconds(3));
        let start = tokio::time::Instant::now();
        let (task, lookups) = await_lineage(failed, &[], &|_, _| None).await;
        assert_eq!((task.task_id, task.status.as_str()), (id, "failed"));
        assert_eq!(lookups, 1);
        assert_eq!(tokio::time::Instant::now(), start);

        // A deploy interruption never waited on a later insert.
        let interrupted = infra_interrupted(lineage_task(id, "failed"));
        let (_, lookups) = await_lineage(interrupted, &[], &|_, _| None).await;
        assert_eq!(lookups, 1);
        assert_eq!(tokio::time::Instant::now(), start);
    }

    #[tokio::test(start_paused = true)]
    async fn await_gives_a_retry_its_own_wait_inside_the_callers_deadline() {
        // The first attempt used up nearly all of the original wait; the
        // retry gets the deadline `retry_deadline` allows it.
        let (parent, child) = (Uuid::now_v7(), Uuid::now_v7());
        let start = tokio::time::Instant::now();
        let task = await_task_lineage(
            infra_interrupted(lineage_task(parent, "failed")),
            start + std::time::Duration::from_secs(1),
            &|| false,
            |id: Uuid, _read_timeout: std::time::Duration| {
                let done = tokio::time::Instant::now() - start >= std::time::Duration::from_secs(5);
                async move { Ok(lineage_task(id, if done { "completed" } else { "running" })) }
            },
            |_parent: Uuid, _read_timeout: std::time::Duration| async move {
                Ok(Some(lineage_task(child, "queued")))
            },
            &|_retry| Some(start + std::time::Duration::from_secs(30)),
        )
        .await
        .expect("the retry finishes inside its own wait");
        assert_eq!((task.task_id, task.status.as_str()), (child, "completed"));

        // Without an extension the original deadline still ends the await.
        let err = await_task_lineage(
            infra_interrupted(lineage_task(parent, "failed")),
            tokio::time::Instant::now() + std::time::Duration::from_secs(1),
            &|| false,
            |id: Uuid, _read_timeout: std::time::Duration| async move {
                Ok(lineage_task(id, "running"))
            },
            |_parent: Uuid, _read_timeout: std::time::Duration| async move {
                Ok(Some(lineage_task(child, "queued")))
            },
            &|_retry| None,
        )
        .await
        .expect_err("the deadline check survives a hop");
        assert!(format!("{err:?}").contains("timeout waiting for task to complete"));
    }

    async fn insert_lineage_root(db: &crate::db::DatabasePool) -> crate::db::Task {
        insert_lineage_root_with(db, None).await
    }

    async fn insert_lineage_root_with(
        db: &crate::db::DatabasePool,
        options: Option<&serde_json::Value>,
    ) -> crate::db::Task {
        let data = crate::db::insert_test_data(db).await.unwrap();
        let task_id = Uuid::now_v7();
        crate::db::Task::insert(
            db,
            &task_id,
            &data.env_id,
            &data.stream_id,
            &data.build_id,
            None,
            "::app/work",
            None,
            options,
            "code",
            60_000,
            None,
        )
        .await
        .unwrap();
        crate::db::Task::get(db, &task_id).await.unwrap()
    }

    /// A task that fails under a `retry` policy has its retry committed with
    /// the failure, so the await that sees `failed` finds the retry on its
    /// first lookup and never spends the legacy grace.
    #[tokio::test]
    async fn await_follows_a_policy_retry_without_a_grace_wait() {
        let db = crate::db::test_db().await;
        let options = serde_json::json!({"retry": {"attempts": 2, "delay": 0}});
        let root = insert_lineage_root_with(&db, Some(&options)).await;
        let env_id = root.env_id;
        assert!(
            crate::db::Task::mark_running(&db, &root.task_id)
                .await
                .unwrap()
        );
        let running = crate::db::Task::get(&db, &root.task_id).await.unwrap();

        let retry_id = Uuid::now_v7();
        let failure = serde_json::json!({"$type": "::hot::task/Failure", "$val": {"msg": "boom"}});
        let write = crate::db::Task::complete_with_retry(
            &db,
            &root.task_id,
            &crate::db::TaskStatus::Failed,
            Some(&failure),
            None,
            None,
            &retry_id,
        )
        .await
        .unwrap();
        assert_eq!(write.retry.map(|retry| retry.retry_task_id), Some(retry_id));
        // The retry runs to completion before the await looks.
        assert!(crate::db::Task::mark_running(&db, &retry_id).await.unwrap());
        assert!(
            crate::db::Task::complete(
                &db,
                &retry_id,
                &crate::db::TaskStatus::Completed,
                None,
                None,
                None
            )
            .await
            .unwrap()
        );

        let lookups = std::cell::Cell::new(0u32);
        let started = std::time::Instant::now();
        let task = await_task_lineage(
            running,
            tokio::time::Instant::now() + std::time::Duration::from_secs(30),
            &|| false,
            |id: Uuid, _read_timeout: std::time::Duration| {
                let db = &db;
                async move {
                    crate::db::Task::get_in_env(db, &id, &env_id)
                        .await
                        .map_err(|e| PollFailure::Transport(e.to_string()))
                }
            },
            |parent: Uuid, read_timeout: std::time::Duration| {
                lookups.set(lookups.get() + 1);
                lookup_retry_child(&db, parent, env_id, read_timeout)
            },
            &|_retry| None,
        )
        .await
        .expect("the lineage ends in the retry's outcome");
        assert_eq!(
            (task.task_id, task.status.as_str()),
            (retry_id, "completed")
        );
        assert_eq!(lookups.get(), 1, "the first lookup finds the retry");
        assert!(
            started.elapsed() < LEGACY_RETRY_CHILD_GRACE,
            "following a committed retry must not wait out the legacy grace"
        );

        // `cancel` and `send` resolve through the same single lookup.
        let failed = crate::db::Task::get(&db, &root.task_id).await.unwrap();
        let newest = newest_retry_attempt(&db, failed, env_id).await;
        assert_eq!(newest.task_id, retry_id);
    }

    /// Fail `task` the way a deploy does: `failed` plus a queued retry, in
    /// one transaction. Returns the retry's id.
    async fn interrupt_with_retry(db: &crate::db::DatabasePool, task: &crate::db::Task) -> Uuid {
        let retry_id = Uuid::now_v7();
        let error = serde_json::json!({
            "$type": "::hot::task/Failure",
            "$val": {"msg": "interrupted", "err": null, "infra_interrupted": true}
        });
        crate::db::Task::finalize_with_infra_retry(
            db,
            &task.task_id,
            &retry_id,
            3,
            &error,
            &error,
            chrono::Utc::now(),
        )
        .await
        .unwrap();
        retry_id
    }

    #[tokio::test]
    async fn cancel_and_send_resolve_to_the_newest_retry() {
        let db = crate::db::test_db().await;
        let root = insert_lineage_root(&db).await;
        let first_retry = interrupt_with_retry(&db, &root).await;
        let first_retry_row = crate::db::Task::get(&db, &first_retry).await.unwrap();
        let second_retry = interrupt_with_retry(&db, &first_retry_row).await;

        // What `send` targets when handed the original id.
        let root = crate::db::Task::get(&db, &root.task_id).await.unwrap();
        assert_eq!(root.status, "failed");
        let newest = newest_retry_attempt(&db, root, first_retry_row.env_id).await;
        assert_eq!(newest.task_id, second_retry);

        let (target, cancelled) =
            cancel_newest_attempt(&db, first_retry_row.parent_task_id.unwrap(), newest.env_id)
                .await
                .unwrap();
        assert_eq!((target, cancelled), (second_retry, true));
        let row = crate::db::Task::get(&db, &second_retry).await.unwrap();
        assert_eq!(row.status, "cancelled");

        // Nothing left to cancel: the lineage now ends in a final state.
        let (target, cancelled) = cancel_newest_attempt(&db, first_retry, row.env_id)
            .await
            .unwrap();
        assert_eq!((target, cancelled), (second_retry, false));
    }

    #[tokio::test]
    async fn cancelled_task_is_final_even_with_a_retry_row() {
        let db = crate::db::test_db().await;
        let root = insert_lineage_root(&db).await;
        assert!(
            crate::db::Task::cancel(&db, &root.task_id, &root.env_id)
                .await
                .unwrap()
        );
        assert!(
            crate::db::Task::insert_retry(&db, &Uuid::now_v7(), &root, 1, chrono::Utc::now())
                .await
                .unwrap()
        );
        let cancelled = crate::db::Task::get(&db, &root.task_id).await.unwrap();
        let newest = newest_retry_attempt(&db, cancelled, root.env_id).await;
        assert_eq!(newest.task_id, root.task_id);
    }

    // -----------------------------------------------------------------------
    // checkpoint / restore error path tests
    // -----------------------------------------------------------------------

    fn create_test_vm() -> VirtualMachine {
        use crate::lang::bytecode::BytecodeProgram;
        use crate::lang::compiler::core_registry::CoreVariableRegistry;

        let program = Arc::new(BytecodeProgram::new());
        let function_mapping = Arc::new(indexmap::IndexMap::new());
        let core_functions = Arc::new(indexmap::IndexMap::new());
        let type_implementations = Arc::new(indexmap::IndexMap::new());
        let core_variables = Arc::new(CoreVariableRegistry::new());

        VirtualMachine::new(
            program,
            None,
            function_mapping,
            core_functions,
            type_implementations,
            core_variables,
            None,
        )
    }

    /// A run that started before its environment was suspended must not
    /// create more work: held tasks would only retry until reactivation.
    #[test]
    fn task_start_refuses_work_in_a_suspended_environment() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let (db, data) = runtime.block_on(async {
            let db = crate::db::test_db().await;
            let data = crate::db::insert_test_data(&db).await.unwrap();
            (db, data)
        });
        let crate::db::DatabasePool::Sqlite(pool) = &db else {
            return;
        };
        runtime
            .block_on(
                sqlx::query("UPDATE env SET active=0 WHERE env_id=?")
                    .bind(data.env_id)
                    .execute(pool),
            )
            .unwrap();
        let _entered = runtime.enter();
        let mut vm = create_test_vm();
        vm.set_database_pool(Arc::new(db.clone()));
        vm.set_execution_context(crate::lang::event::ExecutionContext::new(
            Uuid::now_v7(),
            data.stream_id,
            crate::db::run::RunType::Run.as_id(),
            Some(data.env_id),
            Some(data.user_id),
            Some(data.org_id),
            Some(data.build_id),
        ));
        let refused = vm.require_work_admission().unwrap_err();
        assert!(refused.contains("not admitting"), "{refused}");
        assert!(matches!(
            start(&mut vm, &[Val::from("::app/work")]),
            HotResult::Err(_)
        ));
    }

    /// `type: "container"` would run an image as a container task without
    /// `::hot::box/start`'s Hot Box and image checks.
    #[test]
    fn task_start_refuses_the_container_type() {
        let mut vm = create_test_vm();
        for task_type in ["container", "Container"] {
            let options = crate::val!({"type": task_type});
            let HotResult::Err(error) =
                start(&mut vm, &[Val::from("::app/work"), Val::Null, options])
            else {
                panic!("::hot::task/start accepted type {task_type:?}");
            };
            assert!(error.to_string().contains("::hot::box/start"), "{error}");
        }
    }

    #[test]
    fn test_checkpoint_wrong_arity() {
        let mut vm = create_test_vm();
        let result = checkpoint(&mut vm, &[]);
        assert!(matches!(result, HotResult::Err(_)));

        let result = checkpoint(&mut vm, &[Val::from("a"), Val::from("b")]);
        assert!(matches!(result, HotResult::Err(_)));
    }

    #[test]
    fn test_checkpoint_no_task_id() {
        let mut vm = create_test_vm();
        let result = checkpoint(&mut vm, &[Val::from("data")]);
        assert!(matches!(result, HotResult::Err(_)));
    }

    #[test]
    fn test_checkpoint_no_database() {
        let mut vm = create_test_vm();
        vm.set_task_id(uuid::Uuid::now_v7());
        let result = checkpoint(&mut vm, &[Val::from("data")]);
        assert!(matches!(result, HotResult::Err(_)));
    }

    #[test]
    fn test_restore_too_many_args() {
        let mut vm = create_test_vm();
        let result = restore(&mut vm, &[Val::from("a"), Val::from("b")]);
        assert!(matches!(result, HotResult::Err(_)));
    }

    #[test]
    fn test_restore_no_task_id_no_args() {
        let mut vm = create_test_vm();
        let result = restore(&mut vm, &[]);
        assert!(matches!(result, HotResult::Err(_)));
    }

    #[test]
    fn test_restore_invalid_uuid() {
        let mut vm = create_test_vm();
        let result = restore(&mut vm, &[Val::from("not-a-uuid")]);
        assert!(matches!(result, HotResult::Err(_)));
    }

    #[test]
    fn test_restore_non_string_arg() {
        let mut vm = create_test_vm();
        let result = restore(&mut vm, &[Val::Int(42)]);
        assert!(matches!(result, HotResult::Err(_)));
    }

    #[test]
    fn test_restore_no_database() {
        let mut vm = create_test_vm();
        vm.set_task_id(uuid::Uuid::now_v7());
        let result = restore(&mut vm, &[]);
        assert!(matches!(result, HotResult::Err(_)));
    }
}

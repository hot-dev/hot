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

fn err_val(msg: String) -> Val {
    Val::err(Val::from(msg))
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

    let request = TaskRequest {
        task_id: task_id.to_string(),
        function_name: function_name.clone(),
        args: args_json.clone(),
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

    tokio::runtime::Handle::current().block_on(async {
        if let Some(db) = db {
            // Ensure the origin run row exists in the DB before inserting the task.
            // The emitter/writer pipeline is async, so the run:start may not be committed yet.
            // Use ON CONFLICT to avoid duplicates if the writer already flushed it.
            if let Err(e) = crate::db::run::Run::ensure_run_exists(
                &db,
                &ctx.run_id,
                &env_id,
                &ctx.stream_id,
                ctx.build_id.as_ref(),
                ctx.run_type_id,
                ctx.origin_run_id.as_ref(),
                ctx.user_id.as_ref().unwrap_or(&Uuid::nil()),
                ctx.access_id.as_ref(),
            )
            .await
            {
                tracing::warn!(
                    "::hot::task/start: failed to ensure origin run exists: {}",
                    e
                );
            }

            // Insert task row with status 'queued'
            let args_json_opt = if args_json.is_null() {
                None
            } else {
                Some(&args_json)
            };
            if let Err(e) = crate::db::Task::insert(
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
            )
            .await
            {
                tracing::error!("::hot::task/start: failed to insert task row: {}", e);
            }
        }

        // Enqueue to task queue
        if let Some(queue) = task_queue {
            if let Err(e) = queue.enqueue(request).await {
                tracing::error!("::hot::task/start: failed to enqueue task: {}", e);
            }
        } else {
            tracing::warn!("::hot::task/start: no task queue configured");
        }
    });

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

// ---------------------------------------------------------------------------
// ::hot::task/send — send data to a running task
// ---------------------------------------------------------------------------

/// Send data to a running task.
///
/// Publishes a message to the task's Redis stream channel (`hot:task:{task_id}`).
/// The task worker forwards it to the task's receive channel.
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
        Val::Str(s) => s.to_string(),
        _ => {
            return HotResult::Err(err_val(
                "::hot::task/send: task-id must be a string".to_string(),
            ));
        }
    };

    let data = &args[1];
    let payload_json: serde_json::Value = data.into();

    // Use the stream publisher to publish to the task's channel
    let publisher = vm.get_stream_publisher();
    match publisher {
        Some(pub_sub) => {
            let event = crate::stream::StreamEvent::TaskMessage {
                task_id: task_id.clone(),
                payload: payload_json,
            };

            tokio::runtime::Handle::current().block_on(async {
                if let Err(e) = pub_sub.publish(event).await {
                    tracing::warn!("::hot::task/send: failed to publish: {}", e);
                }
            });

            HotResult::Ok(Val::from(true))
        }
        None => HotResult::Err(err_val(
            "::hot::task/send: no stream publisher available".to_string(),
        )),
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
    if !args.is_empty() {
        return HotResult::Err(err_val("::hot::task/receive: expected 0 args".to_string()));
    }

    let receiver = vm.get_task_receiver();
    match receiver {
        Some(rx) => {
            // parking_lot::Mutex::lock() never returns an error (no poisoning)
            let mut guard = rx.lock();
            match guard.blocking_recv() {
                Some(val) => HotResult::Ok(val),
                None => HotResult::Ok(Val::Null), // channel closed = shutdown
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

    let db = vm.get_database_pool();
    let publisher = vm.get_stream_publisher();

    let cancelled = tokio::runtime::Handle::current().block_on(async {
        let was_cancelled = if let Some(db) = &db {
            match crate::db::Task::cancel(db, &task_id).await {
                Ok(c) => c,
                Err(e) => {
                    tracing::error!(task_id = %task_id, "::hot::task/cancel: DB error: {}", e);
                    false
                }
            }
        } else {
            false
        };

        if was_cancelled
            && let Some(pub_sub) = &publisher
        {
            let event = crate::stream::StreamEvent::TaskMessage {
                task_id: task_id_str.clone(),
                payload: serde_json::json!({"$cancel": true}),
            };
            if let Err(e) = pub_sub.publish(event).await {
                tracing::warn!(task_id = %task_id, "::hot::task/cancel: failed to publish cancel message: {}", e);
            }
        }

        was_cancelled
    });

    HotResult::Ok(Val::from(cancelled))
}

// ---------------------------------------------------------------------------
// ::hot::task/await — wait for a task to complete and return full result
// ---------------------------------------------------------------------------

/// Wait for a task to complete and return its full result including CUS and timing.
///
/// Polls Task::get with exponential backoff (100ms -> 200ms -> ... up to 5s).
/// Stops when the task reaches a terminal status (completed, failed, timed_out, cancelled).
/// Returns a TaskResult with all execution details.
///
/// # Arguments
/// * `task-id` - Task ID string (UUID)
///
/// # Returns
/// `TaskResult` with id, status, timing breakdown, CUS, and result.
pub fn await_task(vm: &mut VirtualMachine, args: &[Val]) -> HotResult<Val> {
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

    let task = tokio::runtime::Handle::current().block_on(async {
        const TERMINAL_STATUSES: &[&str] = &["completed", "failed", "timed_out", "cancelled"];
        const MAX_TIMEOUT_MS: i64 = 24 * 60 * 60 * 1000; // 24 hours
        const INITIAL_DELAY_MS: u64 = 100;
        const MAX_DELAY_MS: u64 = 5000;

        let mut task = match crate::db::Task::get(&db, &task_id).await {
            Ok(t) => t,
            Err(e) => return Err(err_val(format!("::hot::task/await: task not found: {}", e))),
        };

        let timeout_ms = task.timeout_ms.clamp(1000, MAX_TIMEOUT_MS) as u64;
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);

        let mut delay_ms = INITIAL_DELAY_MS;

        while !TERMINAL_STATUSES.contains(&task.status.as_str()) {
            if std::time::Instant::now() >= deadline {
                return Err(err_val(
                    "::hot::task/await: timeout waiting for task to complete".to_string(),
                ));
            }
            tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
            delay_ms = (delay_ms * 2).min(MAX_DELAY_MS);

            task = match crate::db::Task::get(&db, &task_id).await {
                Ok(t) => t,
                Err(e) => return Err(err_val(format!("::hot::task/await: task not found: {}", e))),
            };
        }

        Ok::<crate::db::Task, Val>(task)
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
    result_map.insert(Val::from("$val"), Val::Map(Box::new(val_map)));
    let result = Val::Map(Box::new(result_map));

    HotResult::Ok(result)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

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

    let data_json: serde_json::Value =
        serde_json::to_value(args[0].to_hot_data_repr()).unwrap_or(serde_json::Value::Null);

    let result = tokio::runtime::Handle::current()
        .block_on(async { crate::db::Task::set_checkpoint(&db, &task_id, &data_json).await });

    match result {
        Ok(()) => HotResult::Ok(Val::Bool(true)),
        Err(e) => HotResult::Err(err_val(format!(
            "::hot::task/checkpoint: failed to save: {}",
            e
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

    let result = tokio::runtime::Handle::current()
        .block_on(async { crate::db::Task::get_checkpoint(&db, &task_id).await });

    match result {
        Ok(Some(data)) => {
            let val: Val = serde_json::from_value(data).unwrap_or(Val::Null);
            HotResult::Ok(val)
        }
        Ok(None) => HotResult::Ok(Val::Null),
        Err(e) => HotResult::Err(err_val(format!(
            "::hot::task/restore: failed to load: {}",
            e
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
